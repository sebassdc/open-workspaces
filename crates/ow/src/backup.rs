//! Backups: a consistent SQLite copy of the controller catalog, and an interim
//! push of a runner's sealed snapshots to S3-compatible storage.
use crate::{BackupAction, selfhost};
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use serde_json::json;
use std::{
    fs,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Command,
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
            let status = snapshots(&from.join("snapshots"), &to, endpoint_url.as_deref())?
                .status()
                .context("the AWS CLI (aws) is required for snapshot sync")?;
            ensure!(status.success(), "snapshot sync failed ({status})");
            println!("Snapshots in {} synced to {to}", from.display());
            Ok(0)
        }
    }
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

/// `aws s3 sync` of sealed snapshot directories; staging (`.pending-*`) is skipped.
/// shortcut: shells out to the AWS CLI and never deletes remote copies; the phase 2
/// `SnapshotStore` replaces it with retention and a tested restore.
fn snapshots(dir: &Path, to: &str, endpoint: Option<&str>) -> Result<Command> {
    ensure!(
        to.strip_prefix("s3://")
            .is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/')),
        "--to takes s3://bucket[/prefix]"
    );
    ensure!(
        dir.is_dir(),
        "no snapshots at {}; pass --from with the runner's data directory",
        dir.display()
    );
    let mut command = Command::new("aws");
    command
        .args(["s3", "sync", "--only-show-errors", "--no-follow-symlinks"])
        .args(["--exclude", ".pending-*"])
        .arg(dir)
        .arg(to);
    if let Some(endpoint) = endpoint {
        let url = reqwest::Url::parse(endpoint).context("--endpoint-url must be a URL")?;
        // Snapshots hold guest memory: plain HTTP only to a store on this machine.
        let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
        let loopback = host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && loopback),
            "--endpoint-url must use https (http only for loopback)"
        );
        command.args(["--endpoint-url", endpoint]);
    }
    Ok(command)
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

    #[test]
    fn snapshot_sync_skips_staging_and_requires_safe_endpoints() {
        let dir = temp("snapshots");
        let args = |to: &str, endpoint: Option<&str>| {
            snapshots(&dir, to, endpoint).map(|c| {
                c.get_args()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
        };
        let plain = args("s3://bucket/ow", None).unwrap();
        assert_eq!(&plain[..2], ["s3", "sync"]);
        assert!(plain.windows(2).any(|w| w == ["--exclude", ".pending-*"]));
        assert_eq!(plain.last().unwrap(), "s3://bucket/ow");
        let minio = args("s3://b", Some("http://127.0.0.1:9000")).unwrap();
        assert_eq!(
            &minio[minio.len() - 2..],
            ["--endpoint-url", "http://127.0.0.1:9000"]
        );
        assert!(args("s3://b", Some("https://r2.example.com")).is_ok());
        for (to, endpoint) in [
            ("/tmp/x", None),
            ("s3://", None),
            ("s3:///x", None),
            ("s3://b", Some("http://store.example.com")),
            ("s3://b", Some("ftp://127.0.0.1")),
        ] {
            assert!(args(to, endpoint).is_err(), "{to} {endpoint:?} accepted");
        }
        assert!(snapshots(&dir.join("missing"), "s3://b", None).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
