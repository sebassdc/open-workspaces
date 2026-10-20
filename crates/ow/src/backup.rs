//! Backups: a consistent SQLite copy of the controller catalog, and an interim
//! runner's sealed snapshots to an S3-compatible [`SnapshotStore`], restorable
//! onto another runner. This operator-local path never grants catalog ownership;
//! managed remote restore consumes authorized catalog artifacts in `transfers`.
use crate::{BackupAction, common, selfhost, wire};
use anyhow::{Context, Result, ensure};
use ow_core::snapshot_store::{S3Store, SnapshotStore};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

pub fn run(base: Option<PathBuf>, action: BackupAction) -> Result<i32> {
    match action {
        BackupAction::CatalogCheck { catalog, against } => {
            let checked = catalog_check(&catalog)?;
            if let Some(source) = against {
                ensure!(
                    checked == catalog_check(&source)?,
                    "catalogs differ; keep writers and GC stopped and resolve the backup gap"
                );
            }
            println!("{}", serde_json::to_string_pretty(&checked)?);
            Ok(0)
        }
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
        BackupAction::Recover {
            to,
            from,
            endpoint_url,
        } => {
            let from = match from {
                Some(dir) => dir,
                None => selfhost::base(base)?.join("runner"),
            };
            let store = S3Store::new(&to, endpoint_url.as_deref())?
                .incremental()
                .index(&from.join("recovery-index"));
            recover(&store, &from, &store.identity())?;
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
            let s3 = S3Store::new(&store, endpoint_url.as_deref())?;
            let url = store.trim_end_matches('/');
            let uploaded = ls(&s3, url)?
                .into_iter()
                .filter_map(|(at, key)| {
                    let name = key.strip_suffix("/manifest.json")?.rsplit('/').next()?;
                    Some((name.to_owned(), at))
                })
                .collect();
            let owners = match newest_catalog(&s3, url)? {
                Some(key) => owners_from_s3(&s3, &key)?,
                None => HashMap::new(),
            };
            let rows = listing(&s3, &uploaded, &owners)?;
            println!("{}", serde_json::to_string_pretty(&rows)?);
            Ok(0)
        }
    }
}

/// The worker owns capture/journal changes; network transfer never holds its runtime lock.
fn recover(store: &dyn SnapshotStore, runner: &Path, store_id: &str) -> Result<()> {
    use std::os::fd::AsRawFd;
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(runner.join("recovery-upload.lock"))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "a recovery upload is already running for this runner"
    );
    let status = wire::request(runner, json!({"op":"status"}))?;
    ensure!(
        status["recovery_points_v1"] == true,
        "worker does not support recovery points; complete its normal class-2 upgrade first"
    );
    // A worker that can take its uploader's seals leaves captures unsealed, so
    // each byte is read once: hashed and sent in the same pass.
    let client_seal = status["recovery_client_seal_v1"] == true && store.seals_while_uploading();
    let machines = wire::request(runner, json!({"op":"list"}))?;
    let mut machines = machines.as_array().context("invalid machine list")?.clone();
    // An interrupted long transfer must not always leave later boxes at the back.
    machines.sort_by_key(|m| {
        (
            m["durability"]["last_durable_at"].as_u64().unwrap_or(0),
            m["durability"]["retry_at"].as_u64().unwrap_or(0),
        )
    });
    let mut failed = Vec::new();
    for machine in &machines {
        if machine.get("lifecycle").is_some()
            && (machine["lifecycle"]["phase"] != "active"
                || !machine["lifecycle"]["intent"].is_null())
        {
            continue;
        }
        let id = machine["id"].as_str().context("missing machine ID")?;
        let result = (|| -> Result<()> {
            let mut request = json!({"op":"recovery-prepare","id":id,"store":store_id});
            if let Some(placement) = machine.get("placement") {
                request["placement"] = placement.clone();
            }
            if client_seal {
                request["client_seal"] = json!(true);
            }
            let point = wire::request(runner, request.clone())?;
            let Some(name) = point["snapshot"].as_str() else {
                return Ok(());
            };
            let mut capture = point["chunks"].clone();
            let uploaded = (|| -> Result<Value> {
                let snapshots = runner.join("snapshots");
                if let Some(staging) = point["unsealed"].as_str() {
                    ensure!(
                        staging.starts_with(".pending-") && !staging.contains('/'),
                        "invalid unsealed capture"
                    );
                    let previous = match point["previous"].as_str() {
                        Some(previous) => {
                            common::identifier(previous)?;
                            Some(snapshots.join(previous))
                        }
                        None => None,
                    };
                    let (files, sent) = store
                        .upload_unsealed(&snapshots.join(staging), previous.as_deref())
                        .context("store cannot seal captures")??;
                    let mut seal = request.clone();
                    seal["op"] = json!("recovery-seal");
                    seal["snapshot"] = json!(name);
                    // Large disks' seals exceed the control socket's 1 MiB frame.
                    // Publish in private staging atomically so a killed writer can retry.
                    let path = snapshots.join(staging);
                    let temporary = path.join(format!(".seals-{}", ow_core::common::nonce()?));
                    let mut output = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o400)
                        .open(&temporary)?;
                    use std::io::Write;
                    output.write_all(&serde_json::to_vec(&files)?)?;
                    output.sync_all()?;
                    fs::rename(temporary, path.join(ow_core::snapshot_digest::UPLOAD_SEALS))?;
                    fs::File::open(&path)?.sync_all()?;
                    seal["capture"] = sent["capture"].clone();
                    wire::request(runner, seal)?;
                    capture = sent["capture"].clone();
                    // Every chunk was sent while sealing; this commits the manifest.
                    let mut transfer = store.push(name, &snapshots.join(name))?;
                    transfer["sealed_while_sending"] = sent;
                    Ok(transfer)
                } else {
                    store.push(name, &snapshots.join(name))
                }
            })();
            request["op"] = json!("recovery-result");
            request["snapshot"] = json!(name);
            request["uploaded"] = json!(uploaded.is_ok());
            wire::request(runner, request)?;
            let transfer = uploaded?;
            println!(
                "{}",
                json!({"workspace":id,"snapshot":name,"last_durable_at":point["captured_at"],
                       "capture":capture,"transfer":transfer})
            );
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("recovery {id}: {error:#}");
            failed.push(id.to_owned());
        }
    }
    ensure!(
        failed.is_empty(),
        "recovery failed for {}; retry on next scheduled run",
        failed.join(", ")
    );
    Ok(())
}

/// One row per stored snapshot, oldest upload first: the box it captured
/// (`workspace`, from its manifest) and that box's owner and name from the
/// catalog (null when the catalog does not know it).
fn listing(
    store: &dyn SnapshotStore,
    uploaded: &HashMap<String, String>,
    owners: &HashMap<String, (String, String)>,
) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for name in store.list()? {
        let manifest: Value = serde_json::from_slice(&store.manifest(&name)?)
            .with_context(|| format!("invalid stored manifest for {name}"))?;
        let workspace = manifest["workspace"].as_str().unwrap_or_default();
        let owner = owners.get(workspace);
        rows.push(json!({
            "snapshot": name,
            "workspace": workspace,
            "owner": owner.map(|o| &o.0),
            "machine": owner.map(|o| &o.1),
            "uploaded": uploaded.get(&name),
        }));
    }
    let key = |r: &Value| {
        (
            r["uploaded"].as_str().map(str::to_owned),
            r["snapshot"].to_string(),
        )
    };
    rows.sort_by_key(key);
    Ok(rows)
}

/// `(UTC date time, key)` of every object under `url`.
fn ls(store: &S3Store, url: &str) -> Result<Vec<(String, String)>> {
    Ok(store
        .objects(url)?
        .into_iter()
        .map(|(at, key)| (utc(at), key))
        .collect())
}

/// `YYYY-MM-DD HH:MM:SS` of a Unix time, in UTC.
fn utc(at: i64) -> String {
    let (days, secs) = (at.div_euclid(86400), at.rem_euclid(86400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// Where the catalog copies for a `…/snapshots` store live: beside it, or for a
/// runner's own store (`PREFIX/runners/NAME/snapshots`) beside its controller's.
fn catalog_root(url: &str) -> Option<(&str, bool)> {
    let root = url
        .strip_suffix("/snapshots")
        .filter(|root| root.strip_prefix("s3://").is_some_and(|r| !r.is_empty()))?;
    Some(match root.rsplit_once("/runners/") {
        Some((parent, name)) if !name.is_empty() && !name.contains('/') => (parent, true),
        _ => (root, false),
    })
}

/// The newest `catalog-*.sqlite3` that `ow-backup` stored beside a `…/snapshots`
/// store (in `…/catalog/`), as `s3://bucket/key`; none for any other store.
fn newest_catalog(store: &S3Store, url: &str) -> Result<Option<String>> {
    let Some((root, runner)) = catalog_root(url) else {
        return Ok(None);
    };
    let bucket = root["s3://".len()..].split('/').next().unwrap_or_default();
    let found = match ls(store, &format!("{root}/catalog")) {
        // A runner host may not read its controller's catalog: list without owners.
        Err(_) if runner => return Ok(None),
        other => other?,
    };
    Ok(found
        .into_iter()
        .map(|(_, key)| key)
        .filter(|key| key.ends_with(".sqlite3"))
        // ow-backup stamps names as YYYYMMDDTHHMMSSZ, so the greatest is the newest.
        .max()
        .map(|key| format!("s3://{bucket}/{key}")))
}

/// Downloads a catalog copy into a private directory, reads it, and deletes it:
/// the catalog holds credential hashes.
fn owners_from_s3(store: &S3Store, key: &str) -> Result<HashMap<String, (String, String)>> {
    let dir = std::env::temp_dir().join(format!("ow-catalog-{}", common::nonce()?));
    fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let result = (|| {
        let path = dir.join("catalog.sqlite3");
        store
            .download(key, &path)
            .with_context(|| format!("downloading {key}"))?;
        owners(&path)
    })();
    fs::remove_dir_all(&dir)?;
    result
}

/// Machine ID to (owner email, machine name), from a catalog copy.
fn owners(catalog: &Path) -> Result<HashMap<String, (String, String)>> {
    let db = Connection::open_with_flags(catalog, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut query = db.prepare(
        "SELECT r.physical, u.email, r.name FROM resources r JOIN users u ON u.id = r.owner \
         WHERE r.kind = 'machine'",
    )?;
    let rows = query.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
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
        let local = ow_core::snapshot_store::read_manifest(&path.join("manifest.json"), &name)?;
        if local["managed_recovery"] == true {
            continue;
        }
        if stored.contains(&name) {
            // Already stored only if it is this snapshot; never skip a different one.
            let remote: serde_json::Value = serde_json::from_slice(&store.manifest(&name)?)
                .with_context(|| format!("invalid stored manifest for {name}"))?;
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

/// Hash every persisted table, including future roots; never print private rows.
fn catalog_check(path: &Path) -> Result<Value> {
    use rusqlite::types::ValueRef;
    crate::nodes::private(path)?;
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.execute_batch("BEGIN")?;
    let checks = db
        .prepare("PRAGMA integrity_check")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(checks == ["ok"], "catalog integrity check failed");
    ensure!(
        !db.prepare("PRAGMA foreign_key_check")?.exists([])?,
        "catalog foreign key violations"
    );
    let schema: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    crate::nodes::readable_by(&db, crate::nodes::SCHEMA)?;
    let floor: String = db.query_row(
        "SELECT value FROM settings WHERE key='min_reader_schema'",
        [],
        |r| r.get(0),
    )?;
    let floor: i64 = floor.parse().context("invalid catalog reader floor")?;
    ensure!(
        floor > 0 && floor <= crate::nodes::SCHEMA && floor <= schema,
        "unsupported catalog reader floor"
    );
    let high: String = db.query_row(
        "SELECT COALESCE((SELECT value FROM settings WHERE key='transfer_epoch'),'0')",
        [],
        |r| r.get(0),
    )?;
    let high: i64 = high
        .parse()
        .context("invalid transfer epoch high-watermark")?;
    let used: i64 = db.query_row("SELECT MAX(epoch) FROM (SELECT COALESCE(MAX(placement_epoch),0) AS epoch FROM resources UNION ALL SELECT COALESCE(MAX(epoch),0) FROM transfers UNION ALL SELECT COALESCE(MAX(epoch),0) FROM transfer_reservations)", [], |r| r.get(0))?;
    ensure!(
        high >= used && high >= 0,
        "catalog epoch high-watermark is below persisted authority; remain non-destructive"
    );
    let objects = db
        .prepare("SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let schema_digest = ow_core::store::sha256(&serde_json::to_vec(&objects)?);
    let mut tables = serde_json::Map::new();
    let definitions = db
        .prepare("SELECT name,sql FROM sqlite_schema WHERE type='table' ORDER BY name")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (name, definition) in definitions {
        let mut stmt = db.prepare(&format!("SELECT * FROM \"{}\"", name.replace('"', "\"\"")))?;
        let columns = stmt.column_count();
        let mut rows = stmt
            .query_map([], |r| {
                let mut row = Vec::new();
                for column in 0..columns {
                    // Tag SQLite types so null/text/blob/numeric values cannot collide.
                    row.push(match r.get_ref(column)? {
                        ValueRef::Null => json!(["null"]),
                        ValueRef::Integer(v) => json!(["integer", v]),
                        ValueRef::Real(v) => json!(["real", v.to_bits()]),
                        ValueRef::Text(v) => json!(["text", v]),
                        ValueRef::Blob(v) => json!(["blob", v]),
                    });
                }
                Ok(ow_core::store::sha256(&serde_json::to_vec(&row).unwrap()))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.sort();
        tables.insert(name, json!({"rows":rows.len(),"digest":ow_core::store::sha256(&serde_json::to_vec(&(definition,rows))?)}));
    }
    Ok(
        json!({"schema":schema,"schema_digest":schema_digest,"min_reader_schema":floor,"epoch_high_watermark":high,"tables":tables}),
    )
}

/// Copies a live catalog with `VACUUM INTO` into a new private, checked file.
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
        let db = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
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
    #[test]
    fn a_runner_store_finds_its_controllers_catalog() {
        use super::catalog_root;
        assert_eq!(
            catalog_root("s3://b/ow/snapshots"),
            Some(("s3://b/ow", false))
        );
        assert_eq!(
            catalog_root("s3://b/ow/runners/big/snapshots"),
            Some(("s3://b/ow", true))
        );
        assert_eq!(
            catalog_root("s3://b/ow/runners/a/b/snapshots"),
            Some(("s3://b/ow/runners/a/b", false))
        );
        assert_eq!(catalog_root("s3://b/ow"), None);
        assert_eq!(catalog_root("s3:///snapshots"), None);
    }

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
    fn catalog_check_covers_all_roots_and_rejects_stale_or_invalid_copies() {
        let dir = temp("check");
        let live = crate::catalog::Catalog::open(&dir, "fixture", None).unwrap();
        let source = dir.join("catalog.sqlite3");
        live.db.execute_batch(r#"INSERT INTO settings VALUES('transfer_epoch','77');
            INSERT INTO users(id,issuer,subject,email) VALUES(1,'fixture','owner','owner@example.test');
            INSERT INTO resources(owner,kind,name,physical,metadata,node,placement_epoch,ssh_policy,ssh_policy_generation)
            VALUES(1,'machine','parked','m-parked','{}','',77,'{"keys":[]}',9);
            INSERT INTO resources(owner,kind,name,physical,metadata,node) VALUES(1,'snapshot','named','s-named','{}','runner-a');
            INSERT INTO grants(physical,grantee,role) VALUES('m-parked',1,'admin');
            INSERT INTO publications(https_port,physical,guest_port) VALUES(11001,'m-parked',8080);
            INSERT INTO nodes(id,credential_hash,memory_mib,slots) VALUES('runner-a','private-credential-digest',1024,4);
            INSERT INTO artifacts(id,kind,owner,machine,local_name,state,store_url) VALUES('capture','capture',1,'m-parked','sealed','stored','s3://fixture/private');
            UPDATE resources SET current_capture='capture' WHERE physical='m-parked';
            INSERT INTO artifact_copies(artifact,node,state,last_used) VALUES('capture','runner-a','present',1);
            INSERT INTO leases(artifact,holder,purpose,expires) VALUES('capture','fixture','pull',9999999999);
            INSERT INTO operations(id,owner,op,resource,state) VALUES(1,1,'wake','parked','pending');
            INSERT INTO transfers(id,operation,kind,source,machine,source_node,source_epoch,source_policy_generation,target,epoch,capture,step)
              VALUES(1,1,'wake','m-parked','m-parked','',76,9,'runner-a',77,'capture','prepared');
            INSERT INTO transfer_locks(machine,transfer) VALUES('m-parked',1);
            INSERT INTO transfer_reservations(transfer,node,epoch,memory_mib,cpus,disk_bytes) VALUES(1,'runner-a',77,256,1,1048576);
            CREATE TABLE future_roots(id TEXT, payload BLOB);
            INSERT INTO future_roots VALUES('keep',x'000102');"#).unwrap();
        let snapshot = dir.join("final.sqlite3");
        catalog(&source, &snapshot).unwrap();
        let checked = catalog_check(&snapshot).unwrap();
        assert_eq!(checked, catalog_check(&source).unwrap());
        for table in [
            "users",
            "resources",
            "grants",
            "nodes",
            "transfers",
            "transfer_reservations",
            "artifacts",
            "artifact_copies",
            "leases",
            "settings",
            "future_roots",
        ] {
            assert!(
                checked["tables"][table]["rows"].as_u64().unwrap() > 0,
                "missing roots in {table}"
            );
        }
        assert!(!checked.to_string().contains("owner@example.test"));
        // Parked roots cannot be reconstructed from an online runner's inventory.
        live.db
            .execute_batch(
                "UPDATE resources SET name='renamed',placement_epoch=78;
                               UPDATE settings SET value='78' WHERE key='transfer_epoch';",
            )
            .unwrap();
        assert_ne!(checked, catalog_check(&source).unwrap());
        let copy = Connection::open(&snapshot).unwrap();
        copy.execute_batch("UPDATE settings SET value='0' WHERE key='transfer_epoch';")
            .unwrap();
        assert!(
            catalog_check(&snapshot)
                .unwrap_err()
                .to_string()
                .contains("high-watermark")
        );
        copy.execute_batch("UPDATE settings SET value='77' WHERE key='transfer_epoch'; CREATE TRIGGER future_fence BEFORE DELETE ON future_roots BEGIN SELECT RAISE(ABORT,'fenced'); END;").unwrap();
        assert_ne!(
            checked,
            catalog_check(&snapshot).unwrap(),
            "lost schema guard"
        );
        copy.execute_batch("DROP TRIGGER future_fence;").unwrap();
        assert_eq!(checked, catalog_check(&snapshot).unwrap());
        copy.execute_batch("PRAGMA foreign_keys=OFF; UPDATE resources SET owner=999;")
            .unwrap();
        assert!(
            catalog_check(&snapshot)
                .unwrap_err()
                .to_string()
                .contains("foreign key")
        );
        copy.execute_batch("UPDATE resources SET owner=1; UPDATE settings SET value='999' WHERE key='min_reader_schema';").unwrap();
        assert!(catalog_check(&snapshot).is_err());
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
        fn push(&self, name: &str, directory: &Path) -> Result<Value> {
            fs::create_dir(self.0.join(name))?;
            for f in ["state", "memory", "disk.ext4", "manifest.json"] {
                fs::copy(directory.join(f), self.0.join(name).join(f))?;
            }
            Ok(Value::Null)
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
    fn recovery_reports_failure_and_continues_other_boxes() {
        use std::os::unix::net::UnixListener;
        let (runner, remote) = (temp("recover"), temp("recover-store"));
        fs::create_dir(runner.join("snapshots")).unwrap();
        for name in ["a", "b"] {
            let dir = runner.join("snapshots").join(name);
            fs::create_dir(&dir).unwrap();
            for file in ["state", "memory", "disk.ext4", "manifest.json"] {
                fs::write(dir.join(file), "fixture").unwrap();
            }
        }
        let listener = UnixListener::bind(runner.join("control.sock")).unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = seen.clone();
        let worker = std::thread::spawn(move || {
            for _ in 0..6 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = wire::line(&mut stream).unwrap();
                let result = match request["op"].as_str().unwrap() {
                    "status" => json!({"recovery_points_v1":true}),
                    "list" => json!([
                        {"id":"a","placement":{"epoch":7,"node":"alpha","transfer":"incarnation"},
                         "lifecycle":{"phase":"active","intent":null}},
                        {"id":"b"},
                        {"id":"fenced","placement":{"epoch":7,"node":"alpha","transfer":"incarnation"},
                         "lifecycle":{"phase":"exported","intent":null}}
                    ]),
                    "recovery-prepare" => json!({"snapshot":request["id"],"captured_at":1000}),
                    "recovery-result" => json!({}),
                    _ => panic!("unexpected recovery request"),
                };
                observed.lock().unwrap().push(request);
                wire::send(&mut stream, &json!({"ok":true,"result":result})).unwrap();
            }
        });
        struct Failing(Dir);
        impl SnapshotStore for Failing {
            fn list(&self) -> Result<Vec<String>> {
                self.0.list()
            }
            fn manifest(&self, name: &str) -> Result<Vec<u8>> {
                self.0.manifest(name)
            }
            fn pull(&self, name: &str, directory: &Path) -> Result<()> {
                self.0.pull(name, directory)
            }
            fn push(&self, name: &str, directory: &Path) -> Result<Value> {
                ensure!(name != "a", "injected upload failure");
                self.0.push(name, directory)
            }
        }
        let error = recover(&Failing(Dir(remote.clone())), &runner, &"a".repeat(64)).unwrap_err();
        worker.join().unwrap();
        assert!(error.to_string().contains("failed for a"));
        let seen = seen.lock().unwrap();
        let results: Vec<_> = seen
            .iter()
            .filter(|v| v["op"] == "recovery-result")
            .collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["uploaded"], false);
        assert_eq!(results[1]["uploaded"], true);
        for request in seen.iter().filter(|r| {
            r["op"]
                .as_str()
                .is_some_and(|op| op.starts_with("recovery-"))
        }) {
            if request["id"] == "a" {
                assert_eq!(
                    request["placement"],
                    json!({"epoch":7,"node":"alpha","transfer":"incarnation"})
                );
            } else {
                assert_eq!(request["id"], "b", "a fenced workspace was scheduled");
                assert!(request.get("placement").is_none());
            }
        }
        assert!(remote.join("b/manifest.json").exists());
        for dir in [runner, remote] {
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn listing_names_each_capture_by_owner_from_the_catalog() {
        let (controller, remote) = (temp("controller"), temp("listing"));
        let mut catalog = crate::catalog::Catalog::open(&controller, "test", None).unwrap();
        let owner = catalog
            .user(&crate::catalog::Identity {
                issuer: "test".into(),
                subject: "a".into(),
                email: "a@example.test".into(),
            })
            .unwrap();
        drop(catalog);
        // The real schema, with the row a create would leave.
        Connection::open(controller.join("catalog.sqlite3"))
            .unwrap()
            .execute(
                "INSERT INTO resources(owner,kind,name,physical,metadata) VALUES(?1,'machine','dev','m-1','{}')",
                [owner],
            )
            .unwrap();
        let owners = owners(&controller.join("catalog.sqlite3")).unwrap();
        for (name, workspace) in [
            ("sleep-new", "m-1"),
            ("sleep-old", "m-gone"),
            ("sleep-x", "m-1"),
        ] {
            fs::create_dir(remote.join(name)).unwrap();
            fs::write(
                remote.join(name).join("manifest.json"),
                json!({"name":name,"workspace":workspace}).to_string(),
            )
            .unwrap();
        }
        let uploaded = HashMap::from([
            ("sleep-new".to_owned(), "2026-10-16 01:00:00".to_owned()),
            ("sleep-old".to_owned(), "2026-10-15 01:00:00".to_owned()),
        ]);
        let rows = listing(&Dir(remote.clone()), &uploaded, &owners).unwrap();
        assert_eq!(
            json!(rows),
            json!([
                {"snapshot":"sleep-x","workspace":"m-1","owner":"a@example.test","machine":"dev","uploaded":null},
                {"snapshot":"sleep-old","workspace":"m-gone","owner":null,"machine":null,"uploaded":"2026-10-15 01:00:00"},
                {"snapshot":"sleep-new","workspace":"m-1","owner":"a@example.test","machine":"dev","uploaded":"2026-10-16 01:00:00"},
            ])
        );
        assert!(listing(&Dir(remote.join("missing")), &uploaded, &owners).is_err());
        for d in [controller, remote] {
            fs::remove_dir_all(d).unwrap();
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
