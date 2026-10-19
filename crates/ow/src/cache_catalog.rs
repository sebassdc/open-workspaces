//! Persistent local-copy claims. No transaction waits for a worker or store request.
use anyhow::{Context, Result, ensure};
use ow_core::store::{self, Reference};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::path::Path;

pub fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS cache_evictions(artifact TEXT NOT NULL,node TEXT NOT NULL,token TEXT NOT NULL UNIQUE,PRIMARY KEY(artifact,node),FOREIGN KEY(artifact,node) REFERENCES artifact_copies(artifact,node));")?;
    // Claims do not expire: a lost delete reply stays excluded until positive reconciliation.
    // Guard both INSERT and UPDATE so leases and roots cannot race an in-flight deletion.
    for (table, predicate) in [
        (
            "operations",
            "c.node=NEW.node AND NEW.op='create' AND NEW.state IN ('pending','uncertain') AND c.artifact IN (SELECT id FROM artifacts WHERE local_name=json_extract(NEW.request,'$.request.image'))",
        ),
        ("leases", "c.artifact=NEW.artifact OR c.artifact=NEW.parent"),
        ("resources", "c.artifact=NEW.current_capture"),
        (
            "transfers",
            "c.artifact=NEW.capture OR c.artifact=NEW.source_capture",
        ),
        (
            "images",
            "c.artifact=NEW.artifact AND NEW.status='current' AND NEW.prefetch=1",
        ),
    ] {
        for event in ["INSERT", "UPDATE"] {
            db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS cache_guard_{table}_{event} BEFORE {event} ON {table} WHEN EXISTS(SELECT 1 FROM artifact_copies c WHERE c.state='evicting' AND ({predicate})) BEGIN SELECT RAISE(ABORT,'artifact copy eviction in progress'); END;"))?;
        }
    }
    db.execute_batch("CREATE TRIGGER IF NOT EXISTS cache_guard_artifact BEFORE UPDATE ON artifacts WHEN EXISTS(SELECT 1 FROM artifact_copies c WHERE c.artifact=OLD.id AND c.state='evicting') AND (NEW.state<>'stored' OR NEW.digest IS NOT OLD.digest OR NEW.attempt IS NOT OLD.attempt OR NEW.store_url<>OLD.store_url) BEGIN SELECT RAISE(ABORT,'artifact copy eviction in progress'); END;
      CREATE TRIGGER IF NOT EXISTS cache_guard_copy BEFORE UPDATE ON artifact_copies WHEN OLD.state='evicting' AND NEW.state<>'evicting' BEGIN SELECT RAISE(ABORT,'eviction requires positive completion'); END;
      CREATE TRIGGER IF NOT EXISTS cache_guard_copy_delete BEFORE DELETE ON artifact_copies WHEN EXISTS(SELECT 1 FROM cache_evictions e WHERE e.artifact=OLD.artifact AND e.node=OLD.node) BEGIN SELECT RAISE(ABORT,'eviction requires positive completion'); END;")?;
    Ok(())
}
#[derive(Debug)]
pub struct Claim {
    pub artifact: String,
    pub node: String,
    pub token: String,
    pub name: String,
    pub reference: Reference,
}
/// Claim exactly the reference from fresh verified inventory. Missing hints never authorize deletion.
pub fn claim(db: &mut Connection, node: &str, digest: &str, now: i64) -> Result<Claim> {
    store::valid_digest(digest)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (artifact,name,attempt): (String,String,String) = tx.query_row("SELECT a.id,a.local_name,a.attempt FROM artifacts a JOIN artifact_copies c ON c.artifact=a.id WHERE c.node=?1 AND c.state='present' AND a.digest=?2 AND a.state='stored' AND a.kind IN ('base','image') AND NOT EXISTS(SELECT 1 FROM operations o WHERE o.node=c.node AND o.op='create' AND o.state IN ('pending','uncertain') AND json_extract(o.request,'$.request.image')=a.local_name) AND NOT EXISTS(SELECT 1 FROM leases l WHERE (l.artifact=a.id OR l.parent=a.id) AND l.expires>?3) AND NOT EXISTS(SELECT 1 FROM images i WHERE i.artifact=a.id AND i.status='current' AND i.prefetch=1) AND NOT EXISTS(SELECT 1 FROM resources r WHERE r.current_capture=a.id) AND NOT EXISTS(SELECT 1 FROM transfers t WHERE (t.capture=a.id OR t.source_capture=a.id) AND t.step NOT IN ('succeeded','aborted','failed'))",params![node,digest,now],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).context("cache is unstored, pinned, leased, placed or already claimed")?;
    let token = store::token()?;
    tx.execute(
        "UPDATE artifact_copies SET state='evicting' WHERE artifact=?1 AND node=?2",
        params![artifact, node],
    )?;
    tx.execute(
        "INSERT INTO cache_evictions(artifact,node,token) VALUES(?1,?2,?3)",
        params![artifact, node, token],
    )?;
    tx.commit()?;
    Ok(Claim {
        artifact,
        node: node.into(),
        token,
        name,
        reference: Reference {
            attempt,
            digest: digest.into(),
        },
    })
}
pub fn complete(db: &mut Connection, claim: &Claim, result: &Value) -> Result<()> {
    ensure!(
        result["digest"] == claim.reference.digest
            && result["freed_bytes"].is_u64()
            && result["available_bytes"].is_u64(),
        "invalid eviction confirmation"
    );
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    ensure!(
        tx.execute(
            "DELETE FROM cache_evictions WHERE artifact=?1 AND node=?2 AND token=?3",
            params![claim.artifact, claim.node, claim.token]
        )? == 1,
        "stale eviction completion"
    );
    tx.execute(
        "DELETE FROM artifact_copies WHERE artifact=?1 AND node=?2 AND state='evicting'",
        params![claim.artifact, claim.node],
    )?;
    tx.commit()?;
    Ok(())
}
#[cfg(target_os = "linux")]
fn request(root: &Path, local_runner: &Path, node: &str, value: Value) -> Result<Value> {
    crate::nodes::operation_capable(root, node, value["op"].as_str().unwrap())?;
    let route = if node == "local" {
        local_runner.to_owned()
    } else {
        crate::nodes::route(root, node)?
    };
    Ok(crate::wire::connect_waiting(&route, &value, std::time::Duration::from_secs(120))?.1)
}
/// Bounded LRU, measured after each unlink. Shared extents may free zero bytes.
#[cfg(target_os = "linux")]
pub fn make_room(
    root: &Path,
    local_runner: &Path,
    node: &str,
    staging: u64,
    incoming: &str,
) -> Result<Value> {
    crate::nodes::online(root, node)?;
    let inventory = request(root, local_runner, node, json!({"op":"cache-inventory"}))?;
    let protected = inventory["protected_bytes"]
        .as_u64()
        .context("cache reserves unknown")?;
    let inflight = inventory["staging_bytes"]
        .as_u64()
        .context("cache staging unknown")?;
    let required = protected
        .checked_add(inflight)
        .and_then(|n| n.checked_add(staging))
        .context("cache admission overflow")?;
    let mut available = inventory["available_bytes"]
        .as_u64()
        .context("cache free bytes unknown")?;
    let mut entries: Vec<ow_core::cache::Entry> =
        serde_json::from_value(inventory["entries"].clone())?;
    ensure!(
        entries.len() <= ow_core::cache::MAX_ENTRIES,
        "cache inventory oversized"
    );
    if entries.iter().any(|e| e.digest == incoming) {
        return Ok(json!({"cache_hit":true}));
    }
    entries
        .sort_by(|a, b| (a.last_used, &a.digest, &a.name).cmp(&(b.last_used, &b.digest, &b.name)));
    let mut db = crate::image_catalog::open(root)?;
    let mut freed = 0u64;
    for e in entries {
        if available >= required {
            break;
        }
        e.validate()?;
        if !e.pins.is_empty() {
            continue;
        }
        // Verified fresh worker hints refresh only known stored catalog copies, never claims.
        db.execute("INSERT INTO artifact_copies(artifact,node,state,exclusive_bytes,last_used) SELECT id,?1,'present',?3,?4 FROM artifacts WHERE digest=?2 AND state='stored' AND local_name=?5 ON CONFLICT(artifact,node) DO UPDATE SET exclusive_bytes=excluded.exclusive_bytes,last_used=excluded.last_used WHERE artifact_copies.state='present'",params![node,e.digest,e.exclusive_bytes.map(|n|n as i64),e.last_used as i64,e.name])?;
        let Ok(c) = claim(&mut db, node, &e.digest, crate::image_store::now()) else {
            continue;
        };
        // Remote uncertain failures retain the claim. A reconnect must deliberately retry it.
        let result = request(
            root,
            local_runner,
            node,
            json!({"op":"cache-evict","name":c.name,"reference":c.reference,"claim":c.token}),
        )?;
        complete(&mut db, &c, &result)?;
        available = result["available_bytes"].as_u64().unwrap();
        freed = freed.saturating_add(result["freed_bytes"].as_u64().unwrap());
    }
    ensure!(
        available >= required,
        "cache cannot free enough space; pull was not started"
    );
    Ok(json!({"freed_bytes":freed,"available_bytes":available,"required_bytes":required}))
}

#[cfg(test)]
mod tests;

/// Reissue the exact claimed deletion after a lost reply. Never drop a claim by age.
#[cfg(target_os = "linux")]
pub fn reconcile(root: &Path, local_runner: &Path, node: &str) -> Result<()> {
    let mut db = crate::image_catalog::open(root)?;
    let claims: Vec<Claim> = db.prepare("SELECT e.artifact,e.node,e.token,a.local_name,a.attempt,a.digest FROM cache_evictions e JOIN artifacts a ON a.id=e.artifact WHERE e.node=?1 ORDER BY e.token LIMIT 33")?.query_map([node],|r|Ok(Claim{artifact:r.get(0)?,node:r.get(1)?,token:r.get(2)?,name:r.get(3)?,reference:Reference{attempt:r.get(4)?,digest:r.get(5)?}}))?.collect::<rusqlite::Result<_>>()?;
    ensure!(claims.len() <= 32, "too many pending cache evictions");
    for c in claims {
        let result = request(
            root,
            local_runner,
            node,
            json!({"op":"cache-evict","name":c.name,"reference":c.reference,"claim":c.token}),
        )?;
        complete(&mut db, &c, &result)?;
    }
    Ok(())
}
