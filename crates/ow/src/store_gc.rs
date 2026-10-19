//! Catalog-rooted collection. Listings enumerate candidates; they never create roots.
use anyhow::{Context, Result, ensure};
use ow_core::store::{
    self, Reference,
    gc::{ControllerObjects, ListedObject},
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

fn same_store(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

const GRACE: i64 = 3600;

/// Additive exclusions also constrain older schema-5 writers, as cache claims do.
pub fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS gc_events(id INTEGER PRIMARY KEY,store_url TEXT NOT NULL,operation TEXT NOT NULL,evidence TEXT,error TEXT,created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
      CREATE TABLE IF NOT EXISTS gc_grace(artifact TEXT PRIMARY KEY REFERENCES artifacts(id),since INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS gc_runs(id TEXT PRIMARY KEY,store_url TEXT NOT NULL,started INTEGER NOT NULL,cutoff INTEGER NOT NULL,live INTEGER NOT NULL,plan TEXT NOT NULL,state TEXT NOT NULL,gate TEXT,error TEXT);
      CREATE TABLE IF NOT EXISTS gc_candidates(run TEXT NOT NULL REFERENCES gc_runs(id),key TEXT NOT NULL,bytes INTEGER NOT NULL,modified INTEGER NOT NULL,prior_version TEXT,delete_marker TEXT,done INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(run,key));
      INSERT INTO settings(key,value) VALUES('store_gc_catalog_id',lower(hex(randomblob(16)))) ON CONFLICT(key) DO NOTHING;
      CREATE TRIGGER IF NOT EXISTS gc_no_row_delete BEFORE DELETE ON artifacts BEGIN SELECT RAISE(ABORT,'artifact history must remain as tombstones'); END;
      CREATE TRIGGER IF NOT EXISTS gc_terminal BEFORE UPDATE ON artifacts WHEN OLD.state IN ('deleting','deleted') AND (NEW.state<>OLD.state AND NOT (OLD.state='deleting' AND NEW.state='deleted') OR NEW.attempt IS NOT OLD.attempt OR NEW.digest IS NOT OLD.digest OR NEW.parent IS NOT OLD.parent OR NEW.store_url<>OLD.store_url) BEGIN SELECT RAISE(ABORT,'artifact deletion is terminal'); END;
      CREATE TRIGGER IF NOT EXISTS gc_immutable BEFORE UPDATE ON artifacts WHEN OLD.state='stored' AND (NEW.state NOT IN ('stored','deleting') OR NEW.attempt IS NOT OLD.attempt OR NEW.digest IS NOT OLD.digest OR NEW.parent IS NOT OLD.parent OR NEW.store_url<>OLD.store_url) BEGIN SELECT RAISE(ABORT,'stored artifact identity is immutable'); END;
      CREATE TRIGGER IF NOT EXISTS gc_push_commit BEFORE UPDATE OF state ON artifacts WHEN OLD.state='uploading' AND NEW.state='stored' AND NOT EXISTS(SELECT 1 FROM leases l WHERE l.artifact=NEW.id AND l.token=NEW.attempt AND l.purpose='push' AND l.expires>unixepoch() AND l.parent IS NEW.parent) BEGIN SELECT RAISE(ABORT,'commit needs an unexpired push lease and approved parent'); END;
      CREATE TRIGGER IF NOT EXISTS gc_push_renew BEFORE UPDATE ON leases WHEN OLD.purpose='push' AND (NEW.expires>OLD.expires AND OLD.expires<=unixepoch() OR NEW.token IS NOT OLD.token OR NEW.artifact<>OLD.artifact OR NEW.parent IS NOT OLD.parent) BEGIN SELECT RAISE(ABORT,'expired or changed push needs a fresh attempt'); END;")?;
    for (table, predicate) in [
        ("resources", "a.id=NEW.current_capture"),
        ("drain_publications", "a.id=NEW.artifact"),
        (
            "transfers",
            "NEW.step NOT IN ('succeeded','aborted','failed') AND (a.id=NEW.capture OR a.id=NEW.source_capture)",
        ),
        (
            "images",
            "NEW.status NOT IN ('retired','failed') AND a.id=NEW.artifact",
        ),
        (
            "operations",
            "NEW.op='create' AND NEW.state IN ('pending','uncertain') AND a.local_name=json_extract(NEW.request,'$.request.image')",
        ),
        ("leases", "a.id=NEW.artifact OR a.id=NEW.parent"),
        (
            "artifacts",
            "NEW.state IN ('stored','uploading') AND a.id=NEW.parent",
        ),
    ] {
        for event in ["INSERT", "UPDATE"] {
            // Finished transfer/copy/history rows may keep their foreign keys forever.
            db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS gc_guard_{table}_{event} BEFORE {event} ON {table} WHEN EXISTS(SELECT 1 FROM artifacts a WHERE a.state IN ('deleting','deleted') AND ({predicate})) BEGIN SELECT RAISE(ABORT,'cannot reference a deleting artifact'); END;"))?;
        }
    }
    // A temporary reference between GC runs restarts grace when it is later released.
    for (table, ids) in [
        ("resources", "NEW.current_capture"),
        ("transfers", "NEW.capture,NEW.source_capture"),
        ("images", "NEW.artifact"),
        (
            "operations",
            "(SELECT id FROM artifacts WHERE local_name=json_extract(NEW.request,'$.request.image'))",
        ),
        ("leases", "NEW.artifact,NEW.parent"),
        ("artifacts", "NEW.parent"),
    ] {
        for event in ["INSERT", "UPDATE"] {
            let seeds = if table == "resources" {
                "SELECT NEW.current_capture UNION SELECT id FROM artifacts WHERE owner=NEW.owner AND kind='snapshot' AND local_name=NEW.physical".to_owned()
            } else {
                format!("SELECT id FROM artifacts WHERE id IN ({ids})")
            };
            db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS gc_grace_{table}_{event} AFTER {event} ON {table} BEGIN DELETE FROM gc_grace WHERE artifact IN (WITH RECURSIVE lineage(id) AS ({seeds} UNION SELECT a.parent FROM artifacts a JOIN lineage l ON a.id=l.id WHERE a.parent IS NOT NULL) SELECT id FROM lineage); END;"))?;
        }
    }
    // Named format-1 snapshots can be discovered by the existing physical snapshot ID.
    for event in ["INSERT", "UPDATE"] {
        db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS gc_snapshot_{event} BEFORE {event} ON resources WHEN NEW.kind='snapshot' AND EXISTS(SELECT 1 FROM artifacts a WHERE a.kind='snapshot' AND a.owner=NEW.owner AND a.local_name=NEW.physical AND a.state IN ('deleting','deleted')) BEGIN SELECT RAISE(ABORT,'cannot name a deleting snapshot'); END;"))?;
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Artifact {
    id: String,
    kind: String,
    attempt: Option<String>,
    digest: Option<String>,
    parent: Option<String>,
    state: String,
    url: String,
    format: i64,
}
#[derive(Debug, PartialEq, Eq)]
struct Authority {
    artifacts: Vec<Artifact>,
    roots: BTreeSet<String>,
}

fn authority(db: &Connection, now: i64) -> Result<Authority> {
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    ensure!(
        [6, 7].contains(&version),
        "GC root contract is not known for this catalog schema"
    );
    ensure!(
        db.query_row::<String, _, _>("PRAGMA quick_check", [], |r| r.get(0))? == "ok",
        "catalog integrity failed"
    );
    ensure!(
        db.query_row::<i64, _, _>("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
            .get(0))?
            == 0,
        "catalog foreign keys are incomplete"
    );
    ensure!(
        db.query_row::<bool, _, _>(
            "SELECT EXISTS(SELECT 1 FROM settings WHERE key='legacy_import' AND value='complete')",
            [],
            |r| r.get(0)
        )?,
        "ownership catalog is not initialized"
    );
    let artifacts: Vec<Artifact> = db
        .prepare("SELECT id,kind,attempt,digest,parent,state,store_url,format FROM artifacts ORDER BY id")?
        .query_map([], |r| {
            Ok(Artifact {
                id: r.get(0)?,
                kind: r.get(1)?,
                attempt: r.get(2)?,
                digest: r.get(3)?,
                parent: r.get(4)?,
                state: r.get(5)?,
                url: r.get(6)?,
                format:r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let roots: BTreeSet<String> = db.prepare("SELECT current_capture FROM resources WHERE current_capture IS NOT NULL
      UNION SELECT a.id FROM resources r JOIN artifacts a ON a.kind='snapshot' AND a.owner=r.owner AND a.local_name=r.physical WHERE r.kind='snapshot'
      UNION SELECT artifact FROM images WHERE status IN ('current','kept')
      UNION SELECT artifact FROM drain_publications
      UNION SELECT capture FROM transfers WHERE step NOT IN ('succeeded','aborted','failed') AND capture IS NOT NULL
      UNION SELECT source_capture FROM transfers WHERE step NOT IN ('succeeded','aborted','failed') AND source_capture IS NOT NULL
      UNION SELECT artifact FROM leases WHERE expires>?1
      UNION SELECT parent FROM leases WHERE expires>?1 AND parent IS NOT NULL
      UNION SELECT a.id FROM operations o JOIN artifacts a ON a.local_name=json_extract(o.request,'$.request.image') WHERE o.op='create' AND o.state IN ('pending','uncertain')")?.query_map([now],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(Authority { artifacts, roots })
}

fn closure(a: &Authority, mut roots: BTreeSet<String>) -> Result<BTreeSet<String>> {
    let by_id: BTreeMap<_, _> = a.artifacts.iter().map(|a| (&a.id, a)).collect();
    for root in roots.clone() {
        let mut path = BTreeSet::new();
        let mut id = &root;
        loop {
            ensure!(path.insert(id), "artifact lineage cycle");
            let row = by_id.get(id).context("missing catalog root/parent")?;
            ensure!(
                matches!(row.state.as_str(), "stored" | "uploading"),
                "root or parent is deleting/deleted"
            );
            roots.insert(id.clone());
            let Some(parent) = &row.parent else { break };
            let p = by_id.get(parent).context("missing catalog parent")?;
            ensure!(
                p.state == "stored" && same_store(&p.url, &row.url),
                "parent is not an approved stored artifact in this store"
            );
            id = parent;
        }
    }
    Ok(roots)
}
fn reference(a: &Artifact) -> Result<Reference> {
    Ok(Reference {
        attempt: a.attempt.clone().context("root has no upload token")?,
        digest: a.digest.clone().context("root has no manifest digest")?,
    })
}
fn keys(objects: &dyn ControllerObjects, a: &Artifact) -> Result<BTreeSet<String>> {
    ensure!(a.format == 1, "unsupported catalog artifact format");
    let r = reference(a)?;
    let m = store::read_manifest(objects, &r)
        .with_context(|| format!("root manifest invalid: {}", a.id))?;
    ensure!(m.kind == a.kind, "root manifest kind differs from catalog");
    let mut keys = BTreeSet::from([format!(
        "{}manifest.json",
        store::attempt_prefix(&r.attempt)?
    )]);
    for file in m.files.values() {
        for object in &file.objects {
            keys.insert(object.key.clone());
        }
    }
    for key in &keys {
        ensure!(objects.head(key)?.is_some(), "live object missing: {key}");
    }
    Ok(keys)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Plan {
    keep: BTreeSet<String>,
    protect: BTreeSet<String>,
    doomed: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct Run {
    pub id: String,
    pub cutoff: i64,
    pub live: bool,
    pub candidates: Vec<ListedObject>,
    pub bytes: u64,
}
/// Dry-run writes history and grace observations, but never transitions an artifact or deletes.
pub fn prepare(
    db: &mut Connection,
    objects: &dyn ControllerObjects,
    url: &str,
    now: i64,
    live: bool,
    gate: Option<&str>,
) -> Result<Run> {
    let result = prepare_inner(db, objects, url, now, live, gate);
    if let Err(error) = &result {
        record_error(db, url, "mark", error)?;
    }
    result
}
fn record_error(db: &Connection, url: &str, op: &str, error: &anyhow::Error) -> Result<()> {
    db.execute(
        "INSERT INTO gc_events(store_url,operation,error) VALUES(?1,?2,?3)",
        params![url, op, format!("{error:#}")],
    )?;
    Ok(())
}
fn prepare_inner(
    db: &mut Connection,
    objects: &dyn ControllerObjects,
    url: &str,
    now: i64,
    live: bool,
    gate: Option<&str>,
) -> Result<Run> {
    objects.configuration()?;
    ensure!(
        !live || gate.is_some(),
        "live GC requires activation evidence"
    );
    let id = store::token()?;
    let started = objects.marker(&id)?;
    let cutoff = started
        .checked_sub(GRACE)
        .context("invalid server cutoff")?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let a = authority(&tx, now)?;
    ensure!(
        a.artifacts
            .iter()
            .filter(|a| same_store(&a.url, url))
            .all(|a| a.format == 1),
        "unknown artifact format; root authority is incomplete"
    );
    let rooted = closure(&a, a.roots.clone())?;
    let mut live_ids = rooted.clone();
    let mut observed_grace = BTreeMap::new();
    for row in &a.artifacts {
        if !same_store(&row.url, url) || !matches!(row.state.as_str(), "stored" | "uploading") {
            continue;
        }
        if rooted.contains(&row.id) {
            tx.execute("DELETE FROM gc_grace WHERE artifact=?1", [&row.id])?;
        } else {
            // First observation uses S3 time, never created_at/unreferenced_at from a local clock.
            tx.execute("INSERT INTO gc_grace(artifact,since) VALUES(?1,?2) ON CONFLICT(artifact) DO NOTHING",params![row.id,started])?;
            let since: i64 = tx.query_row(
                "SELECT since FROM gc_grace WHERE artifact=?1",
                [&row.id],
                |r| r.get(0),
            )?;
            observed_grace.insert(row.id.clone(), since);
            if since >= cutoff {
                live_ids.insert(row.id.clone());
            }
        }
    }
    live_ids = closure(&a, live_ids)?;
    tx.commit()?;
    let mut plan = Plan {
        keep: BTreeSet::new(),
        protect: BTreeSet::new(),
        doomed: vec![],
    };
    for row in &a.artifacts {
        if !same_store(&row.url, url) {
            continue;
        }
        if live_ids.contains(&row.id) {
            if row.state == "stored" {
                plan.keep.extend(keys(objects, row)?);
            }
            if row.state == "uploading" {
                let token = row.attempt.as_ref().context("upload has no token")?;
                store::attempt_prefix(token)?;
                plan.protect.insert(token.clone());
            }
        } else if row.state != "deleted" {
            plan.doomed.push(row.id.clone());
        }
    }
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();
    for object in objects.list()? {
        let token = object
            .key
            .split('/')
            .nth(2)
            .context("invalid candidate key")?;
        let prefix = store::attempt_prefix(token)?;
        ensure!(
            object.key.starts_with(&prefix)
                && ["manifest.json", "disk.ext4.zst", "state.zst", "memory.zst"]
                    .contains(&object.key[prefix.len()..].as_ref()),
            "invalid candidate key"
        );
        ensure!(seen.insert(object.key.clone()), "duplicate listing key");
        if object.modified < cutoff
            && !plan.keep.contains(&object.key)
            && !plan.protect.contains(token)
        {
            candidates.push(object);
        }
    }
    candidates.sort_by_key(|o| (!o.key.ends_with("/manifest.json"), o.key.clone()));
    let bytes = candidates.iter().try_fold(0u64, |n, o| {
        n.checked_add(o.bytes).context("candidate size overflow")
    })?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    ensure!(
        authority(&tx, now)? == a,
        "catalog changed during mark; retry with a new run"
    );
    // Transient references can reset grace without changing the final root set.
    for (artifact, since) in &observed_grace {
        ensure!(
            tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM gc_grace WHERE artifact=?1 AND since=?2)",
                params![artifact, since],
                |r| r.get::<_, bool>(0)
            )?,
            "artifact grace changed during mark; retry with a new run"
        );
    }
    tx.execute("INSERT INTO gc_runs(id,store_url,started,cutoff,live,plan,state,gate) VALUES(?1,?2,?3,?4,?5,?6,'ready',?7)",params![id,url,started,cutoff,live,serde_json::to_string(&plan)?,gate])?;
    if live {
        for id in &plan.doomed {
            tx.execute(
                "UPDATE images SET status='failed' WHERE artifact=?1 AND status='building'",
                [id],
            )?;
            tx.execute("UPDATE artifacts SET state='deleting' WHERE id=?1 AND state IN ('stored','uploading')",[id])?;
        }
    }
    for o in &candidates {
        tx.execute(
            "INSERT INTO gc_candidates(run,key,bytes,modified) VALUES(?1,?2,?3,?4)",
            params![id, o.key, i64::try_from(o.bytes)?, o.modified],
        )?;
    }
    tx.commit()?;
    Ok(Run {
        id,
        cutoff,
        live,
        candidates,
        bytes,
    })
}

pub fn verify(
    db: &Connection,
    objects: &dyn ControllerObjects,
    url: &str,
    now: i64,
) -> Result<usize> {
    let result = verify_inner(db, objects, url, now);
    if let Err(error) = &result {
        record_error(db, url, "verify", error)?;
    }
    result
}
fn verify_inner(
    db: &Connection,
    objects: &dyn ControllerObjects,
    url: &str,
    now: i64,
) -> Result<usize> {
    let a = authority(db, now)?;
    closure(&a, a.roots.clone())?;
    let mut count = 0;
    for row in a
        .artifacts
        .iter()
        .filter(|a| same_store(&a.url, url) && a.state == "stored")
    {
        count += keys(objects, row)?.len();
    }
    Ok(count)
}

/// Terminal claims survive forever. Restart reissues only the same run's remaining keys.
pub fn sweep(
    db: &mut Connection,
    objects: &dyn ControllerObjects,
    url: &str,
    run: &str,
    now: i64,
) -> Result<usize> {
    let (live, run_url, state): (bool, String, String) = db.query_row(
        "SELECT live,store_url,state FROM gc_runs WHERE id=?1",
        [run],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    ensure!(
        live && same_store(&run_url, url)
            && matches!(state.as_str(), "ready" | "failed" | "finished"),
        "not a live resumable run for this store"
    );
    let result = (|| {
        objects.configuration()?;
        verify(db, objects, url, now)?;
        let candidates:Vec<(String,i64)>=db.prepare("SELECT key,bytes FROM gc_candidates WHERE run=?1 AND done=0 ORDER BY CASE WHEN key LIKE '%/manifest.json' THEN 0 ELSE 1 END,key")?.query_map([run],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        for (key, bytes) in &candidates {
            if let Some(head) = objects.head(key)? {
                ensure!(
                    head.bytes == u64::try_from(*bytes)?,
                    "candidate changed after mark"
                );
                // Persist the recoverable version before a possibly lost delete response.
                db.execute("UPDATE gc_candidates SET prior_version=?1 WHERE run=?2 AND key=?3 AND prior_version IS NULL",params![head.version,run,key])?;
                let marker = objects.delete(key)?;
                db.execute(
                    "UPDATE gc_candidates SET delete_marker=?1,done=1 WHERE run=?2 AND key=?3",
                    params![marker, run, key],
                )?;
            } else {
                db.execute(
                    "UPDATE gc_candidates SET done=1 WHERE run=?1 AND key=?2",
                    params![run, key],
                )?;
            }
        }
        let remaining = objects.list()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let attempts: Vec<(String, String)> = tx
            .prepare("SELECT id,attempt FROM artifacts WHERE rtrim(store_url,'/')=rtrim(?1,'/') AND state='deleting'")?
            .query_map([url], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, attempt) in attempts {
            let prefix = store::attempt_prefix(&attempt)?;
            if !remaining.iter().any(|o| o.key.starts_with(&prefix)) {
                tx.execute("UPDATE artifacts SET state='deleted',deleted_at=CURRENT_TIMESTAMP WHERE id=?1 AND state='deleting'",[id])?;
            }
        }
        tx.commit()?;
        let verified = verify(db, objects, url, now)?;
        db.execute("UPDATE gc_runs SET state='finished' WHERE id=?1", [run])?;
        Ok(verified)
    })();
    if let Err(e) = &result {
        record_error(db, url, "sweep", e)?;
        db.execute(
            "UPDATE gc_runs SET state='failed',error=?1 WHERE id=?2",
            params![format!("{e:#}"), run],
        )?;
    }
    result
}

/// Operator attestation after effective-role tests and an actual version restore drill.
/// Bound to this catalog file, a successful dry run, this namespace and a short window.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    pub catalog_id: String,
    pub device: u64,
    pub inode: u64,
    pub store_url: String,
    pub dry_run: String,
    pub expires: i64,
    pub catalog_authoritative: bool,
    pub policy_evidence_sha256: String,
    pub undo_evidence_sha256: String,
}
pub fn activation(
    db: &Connection,
    root: &Path,
    url: &str,
    path: &Path,
    now: i64,
) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    crate::nodes::private(path)?;
    let bytes = std::fs::read(path)?;
    ensure!(bytes.len() <= 16384, "oversized activation receipt");
    let a: Activation = serde_json::from_slice(&bytes)?;
    let id: String = db.query_row(
        "SELECT value FROM settings WHERE key='store_gc_catalog_id'",
        [],
        |r| r.get(0),
    )?;
    let meta = std::fs::metadata(root.join("catalog.sqlite3"))?;
    ensure!(
        a.catalog_id == id
            && a.device == meta.dev()
            && a.inode == meta.ino()
            && a.store_url == url
            && a.catalog_authoritative
            && a.expires > now
            && a.expires <= now + 3600,
        "stale or incomplete catalog authority receipt"
    );
    store::valid_digest(&a.policy_evidence_sha256)?;
    store::valid_digest(&a.undo_evidence_sha256)?;
    ensure!(db.query_row::<bool,_,_>("SELECT EXISTS(SELECT 1 FROM gc_runs WHERE id=?1 AND store_url=?2 AND live=0 AND state='ready')",params![a.dry_run,url],|r|r.get(0))?,"activation needs an exact dry run");
    Ok(serde_json::to_string(&a)?)
}

#[cfg(test)]
mod tests;

fn audit_configuration(db: &Connection, objects: &store::S3Objects, url: &str) -> Result<()> {
    match objects.configuration_snapshot() {
        Ok(value) => {
            db.execute(
                "INSERT INTO gc_events(store_url,operation,evidence) VALUES(?1,'configuration',?2)",
                params![url, value.to_string()],
            )?;
            Ok(())
        }
        Err(error) => {
            record_error(db, url, "configuration", &error)?;
            Err(error)
        }
    }
}

pub fn run(base: Option<std::path::PathBuf>, action: crate::StoreAction) -> Result<i32> {
    use crate::StoreAction;
    use serde_json::json;
    let root = crate::selfhost::base(base)?.join("controller");
    crate::nodes::private(&root.join("catalog.sqlite3"))?;
    let mut db = crate::image_catalog::open(&root)?;
    // The gateway installs guards during normal initialization; never infer missing roots here.
    authority(&db, crate::image_store::now())?;
    initialize(&db)?;
    let now = crate::image_store::now();
    match action {
        StoreAction::Gc {
            to,
            live,
            activation: receipt,
            resume,
            ..
        } => {
            let to = to.trim_end_matches('/').to_owned();
            let objects = store::S3Objects::new(&to, None)?;
            audit_configuration(&db, &objects, &to)?;
            let gate = receipt
                .as_deref()
                .map(|p| activation(&db, &root, &to, p, now))
                .transpose()?;
            if let Some(id) = resume {
                // A new receipt is required on every restart, including after a controller restore.
                sweep(&mut db, &objects, &to, &id, now)?;
                println!("{}", json!({"run":id,"state":"finished"}));
            } else {
                let run = prepare(&mut db, &objects, &to, now, live, gate.as_deref())?;
                println!("{}", serde_json::to_string(&run)?);
                if live {
                    let count = sweep(&mut db, &objects, &to, &run.id, now)?;
                    println!(
                        "{}",
                        json!({"run":run.id,"state":"finished","verified_objects":count})
                    );
                }
            }
        }
        StoreAction::Verify { to } => {
            let to = to.trim_end_matches('/').to_owned();
            let objects = store::S3Objects::new(&to, None)?;
            audit_configuration(&db, &objects, &to)?;
            objects.configuration()?;
            println!(
                "{}",
                json!({"verified_objects":verify(&db,&objects,&to,now)?})
            );
        }
        StoreAction::UndoPlan { run } => {
            let (url, state, error): (String, String, Option<String>) = db.query_row(
                "SELECT store_url,state,error FROM gc_runs WHERE id=?1",
                [&run],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let rows:Vec<serde_json::Value>=db.prepare("SELECT key,bytes,prior_version,delete_marker,done FROM gc_candidates WHERE run=?1 ORDER BY key")?.query_map([&run],|r|Ok(json!({"key":r.get::<_,String>(0)?,"bytes":r.get::<_,i64>(1)?,"prior_version":r.get::<_,Option<String>>(2)?,"delete_marker":r.get::<_,Option<String>>(3)?,"done":r.get::<_,bool>(4)?})))?.collect::<rusqlite::Result<_>>()?;
            println!(
                "{}",
                json!({"run":run,"store_url":url,"state":state,"error":error,"objects":rows,"action":"Use an independently authorized break-glass identity; inspect exact version IDs before removing delete markers. A lost response requires ListObjectVersions reconciliation. Never revive a tombstoned artifact; republish verified recovered bytes with a fresh token."})
            );
        }
    }
    Ok(0)
}
