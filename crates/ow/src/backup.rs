//! Backups: a consistent SQLite copy of the controller catalog, and an interim
//! runner's sealed snapshots to an S3-compatible [`SnapshotStore`], restorable
//! onto another runner.
use crate::{BackupAction, common, selfhost, wire};
use anyhow::{Context, Result, ensure};
use ow_core::snapshot_store::{S3Store, SnapshotStore};
use rusqlite::Connection;
use serde_json::json;
use std::{
    fs,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

pub fn run(base: Option<PathBuf>, action: BackupAction) -> Result<i32> {
    match action {
        BackupAction::Catalog { output, from } => {
            let from = match from {
                Some(dir) => dir,
                None => selfhost::base(base)?.join("controller"),
            };
            let bytes = catalog(&from.join("catalog.sqlite3"), &output)?;
            println!(
                "{}",
                json!({"catalog":output,"bytes":bytes,"integrity":"ok"})
            );
            Ok(0)
        }
        BackupAction::Snapshots {
            to,
            from,
            endpoint_url,
        } => {
            let from = match from {
                Some(dir) => dir,
                None => selfhost::base(base)?.join("runner"),
            };
            let store = S3Store::new(&to, endpoint_url.as_deref())?;
            let pushed = push_new(&store, &from.join("snapshots"))?;
            println!("{}", json!({"store":to,"uploaded":pushed}));
            Ok(0)
        }
        BackupAction::Restore {
            snapshot,
            from,
            to,
            endpoint_url,
            cold,
        } => {
            common::identifier(&snapshot)?;
            let runner = match to {
                Some(dir) => dir,
                None => selfhost::base(base)?.join("runner"),
            };
            let store = S3Store::new(&from, endpoint_url.as_deref())?;
            fetch(&store, &snapshot, &runner.join("snapshots"))?;
            let result = wire::request(
                &runner,
                json!({"op":"import","snapshot":snapshot,"cold":cold}),
            )
            .with_context(|| {
                format!("starting the machine of {snapshot} on {}", runner.display())
            })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(0)
        }
        BackupAction::List {
            store,
            endpoint_url,
        } => {
            let names = S3Store::new(&store, endpoint_url.as_deref())?.list()?;
            println!("{}", json!(names));
            Ok(0)
        }
    }
}

/// Uploads every sealed snapshot in `dir` that the store lacks; staging
/// (`.pending-*`) never counts. Returns the uploaded names.
fn push_new(store: &dyn SnapshotStore, dir: &Path) -> Result<Vec<String>> {
    ensure!(
        dir.is_dir(),
        "no snapshots at {}; pass --from with the runner's data directory",
        dir.display()
    );
    let stored = store.list()?;
    let mut pushed = Vec::new();
    let mut entries = fs::read_dir(dir)?
        .map(|e| Ok(e?.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>>>()?;
    entries.sort();
    for name in entries {
        let path = dir.join(&name);
        if name.starts_with('.') || !path.join("manifest.json").is_file() {
            continue;
        }
        if stored.contains(&name) {
            // Already stored only if it is this snapshot; never skip a different one.
            let remote: serde_json::Value = serde_json::from_slice(&store.manifest(&name)?)
                .with_context(|| format!("invalid stored manifest for {name}"))?;
            let local = ow_core::snapshot_store::read_manifest(&path.join("manifest.json"), &name)?;
            ensure!(
                remote == local,
                "the store already has a different snapshot named {name}; use a prefix per runner"
            );
            continue;
        }
        store
            .push(&name, &path)
            .with_context(|| format!("uploading snapshot {name}"))?;
        pushed.push(name);
    }
    Ok(pushed)
}

/// Downloads `name` into `dir` (a runner's snapshots) through a staging directory,
/// so a partial download is never a snapshot. A snapshot of that name already on
/// the runner is used only when it is the one in the store (same manifest, which
/// holds the workspace and every file's hash); any other is refused.
fn fetch(store: &dyn SnapshotStore, name: &str, dir: &Path) -> Result<()> {
    let target = dir.join(name);
    if target.join("manifest.json").exists() {
        let stored: serde_json::Value = serde_json::from_slice(&store.manifest(name)?)
            .with_context(|| format!("invalid stored manifest for {name}"))?;
        let local = ow_core::snapshot_store::read_manifest(&target.join("manifest.json"), name)?;
        ensure!(
            stored == local,
            "this runner already has a different snapshot named {name} (workspace {}, \
             the store's is for {}); restore onto another runner or delete the local one",
            local["workspace"],
            stored["workspace"]
        );
        return Ok(());
    }
    ensure!(
        dir.is_dir(),
        "no runner at {}; start its worker first or pass --to",
        dir.display()
    );
    ensure!(
        !target.exists(),
        "incomplete snapshot {} exists; preserve and inspect it",
        target.display()
    );
    let staging = dir.join(format!(".pending-{}", common::nonce()?));
    store.pull(name, &staging)?;
    fs::rename(&staging, &target)?;
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Copies a live catalog with `VACUUM INTO` (one read transaction, so writers keep
/// going) into a new 0600 file, then checks the copy. Returns its size in bytes.
fn catalog(source: &Path, output: &Path) -> Result<u64> {
    ensure!(
        source.is_file(),
        "no catalog at {}; pass --from with the controller's data directory",
        source.display()
    );
    // Created first so the copy is never readable by others; SQLite fills the empty file.
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(output)
        .with_context(|| format!("{} must not exist yet", output.display()))?;
    let copied = (|| -> Result<u64> {
        let db = Connection::open(source)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute(
            "VACUUM INTO ?1",
            [output.to_str().context("backup path must be UTF-8")?],
        )?;
        let check: String =
            Connection::open(output)?.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        ensure!(check == "ok", "backup failed its integrity check: {check}");
        Ok(fs::metadata(output)?.len())
    })();
    if copied.is_err() {
        let _ = fs::remove_file(output);
    }
    copied
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ow-backup-{name}-{}",
            crate::common::nonce().unwrap()
        ));
        fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn catalog_copy_is_private_consistent_and_never_overwrites() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("catalog");
        let source = dir.join("catalog.sqlite3");
        let live = Connection::open(&source).unwrap();
        live.execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE nodes(id TEXT); INSERT INTO nodes VALUES('a'),('b');",
        )
        .unwrap();
        // An open write transaction elsewhere must not leak into the copy.
        live.execute_batch("BEGIN; INSERT INTO nodes VALUES('uncommitted');")
            .unwrap();
        let output = dir.join("backup.sqlite3");
        assert!(catalog(&source, &output).unwrap() > 0);
        assert_eq!(
            fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let copy = Connection::open(&output).unwrap();
        let rows: i64 = copy
            .query_row("SELECT count(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 2);
        assert!(catalog(&source, &output).is_err(), "overwrote a backup");
        assert!(catalog(&dir.join("missing.sqlite3"), &dir.join("x")).is_err());
        assert!(!dir.join("x").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    /// A store in a local directory, standing in for S3.
    struct Dir(PathBuf);
    impl SnapshotStore for Dir {
        fn list(&self) -> Result<Vec<String>> {
            let mut names = Vec::new();
            for e in fs::read_dir(&self.0)? {
                let e = e?;
                if e.path().join("manifest.json").exists() {
                    names.push(e.file_name().to_string_lossy().into_owned());
                }
            }
            Ok(names)
        }
        fn push(&self, name: &str, directory: &Path) -> Result<()> {
            fs::create_dir(self.0.join(name))?;
            for f in ["state", "memory", "disk.ext4", "manifest.json"] {
                fs::copy(directory.join(f), self.0.join(name).join(f))?;
            }
            Ok(())
        }
        fn manifest(&self, name: &str) -> Result<Vec<u8>> {
            Ok(fs::read(self.0.join(name).join("manifest.json"))?)
        }
        fn pull(&self, name: &str, directory: &Path) -> Result<()> {
            ensure!(self.0.join(name).join("manifest.json").exists(), "missing");
            fs::create_dir(directory)?;
            for f in ["state", "memory", "disk.ext4", "manifest.json"] {
                fs::copy(self.0.join(name).join(f), directory.join(f))?;
            }
            Ok(())
        }
    }

    #[test]
    fn push_skips_staging_and_stored_and_fetch_never_leaves_partials() {
        let (runner, remote, other) = (temp("runner"), temp("remote"), temp("other"));
        let manifest = |name: &str, workspace: &str| {
            let h = "a".repeat(64);
            json!({"name":name,"workspace":workspace,"files":{"state":h,"memory":h,"disk.ext4":h}})
                .to_string()
        };
        for name in ["a", "b", ".pending-x"] {
            fs::create_dir(runner.join(name)).unwrap();
            for f in ["state", "memory", "disk.ext4"] {
                fs::write(runner.join(name).join(f), name).unwrap();
            }
            fs::write(
                runner.join(name).join("manifest.json"),
                manifest(name, "box-b"),
            )
            .unwrap();
        }
        fs::create_dir(runner.join("unsealed")).unwrap();
        let store = Dir(remote.clone());
        assert_eq!(push_new(&store, &runner).unwrap(), ["a", "b"]);
        assert!(
            push_new(&store, &runner).unwrap().is_empty(),
            "uploaded twice"
        );
        // Another runner's same-named snapshot in the store is not taken for ours.
        fs::write(remote.join("a/manifest.json"), manifest("a", "box-x")).unwrap();
        assert!(
            push_new(&store, &runner).is_err(),
            "skipped a different stored snapshot"
        );
        fs::write(remote.join("a/manifest.json"), manifest("a", "box-b")).unwrap();
        assert!(push_new(&store, &runner.join("missing")).is_err());

        fetch(&store, "a", &other).unwrap();
        assert_eq!(fs::read(other.join("a/memory")).unwrap(), b"a");
        fetch(&store, "a", &other).unwrap(); // already there
        assert!(fetch(&store, "nope", &other).is_err());
        let left: Vec<_> = fs::read_dir(&other)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["a"], "a failed download left files behind");
        fs::create_dir(other.join("b")).unwrap();
        assert!(
            fetch(&store, "b", &other).is_err(),
            "adopted an incomplete snapshot"
        );
        fs::remove_dir(other.join("b")).unwrap();
        // A same-named local snapshot of another workspace is never used instead.
        fs::create_dir(other.join("b")).unwrap();
        fs::write(other.join("b/manifest.json"), manifest("b", "box-a")).unwrap();
        let error = fetch(&store, "b", &other).unwrap_err().to_string();
        assert!(error.contains("different snapshot named b"), "{error}");
        fs::write(other.join("b/manifest.json"), manifest("b", "box-b")).unwrap();
        fetch(&store, "b", &other).unwrap();
        for d in [runner, remote, other] {
            fs::remove_dir_all(d).unwrap();
        }
    }
}
