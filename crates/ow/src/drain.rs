//! Explicit runner retirement. Provider destruction is a separate consumer of the certificate.
use crate::{catalog::Catalog, common, nodes, transfers, wire};
use anyhow::{Context, Result, ensure};
use ow_core::store;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    fs,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS runner_drains(
      node TEXT PRIMARY KEY,eligible INTEGER NOT NULL DEFAULT 0 CHECK(eligible IN (0,1)),
      state TEXT NOT NULL DEFAULT 'active' CHECK(state IN ('active','soft-draining','draining','empty','retained','removed')),
      generation INTEGER NOT NULL DEFAULT 0,seal TEXT,certificate TEXT,retention TEXT,error TEXT);
      CREATE TABLE IF NOT EXISTS drain_publications(node TEXT NOT NULL,name TEXT NOT NULL,artifact TEXT NOT NULL REFERENCES artifacts(id),manifest TEXT NOT NULL,PRIMARY KEY(node,name));
      CREATE TABLE IF NOT EXISTS drain_revision(id INTEGER PRIMARY KEY CHECK(id=1),version INTEGER NOT NULL);
      INSERT OR IGNORE INTO drain_revision VALUES(1,0);")?;
    for table in [
        "resources",
        "transfers",
        "transfer_reservations",
        "operations",
        "artifacts",
        "artifact_copies",
        "leases",
        "drain_publications",
        "node_usage",
    ] {
        for event in ["INSERT", "UPDATE", "DELETE"] {
            db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS drain_revision_{table}_{event} AFTER {event} ON {table} BEGIN UPDATE drain_revision SET version=version+1 WHERE id=1; END;"))?;
        }
    }
    for (table, predicate) in [
        (
            "resources",
            "NEW.node<>'' AND (NEW.kind='machine' OR NEW.kind='snapshot')",
        ),
        ("transfer_reservations", "1"),
    ] {
        for event in ["INSERT", "UPDATE"] {
            let same = if event == "UPDATE" && table == "resources" {
                " AND NEW.node<>OLD.node AND NOT EXISTS(SELECT 1 FROM transfer_reservations q JOIN transfers t ON t.id=q.transfer WHERE NEW.kind='machine' AND t.machine=NEW.physical AND q.node=NEW.node AND t.epoch=NEW.placement_epoch AND q.epoch=t.epoch)"
            } else {
                ""
            };
            db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS drain_admission_{table}_{event} BEFORE {event} ON {table} WHEN {predicate}{same} AND EXISTS(SELECT 1 FROM runner_drains d WHERE d.node=NEW.node AND d.state<>'active') BEGIN SELECT RAISE(ABORT,'runner drain blocks placement'); END;"))?;
        }
    }
    // Even a legacy operation admitted between inventory and certificate invalidates the proof.
    db.execute_batch("CREATE TRIGGER IF NOT EXISTS drain_operations_insert BEFORE INSERT ON operations WHEN NEW.state IN ('pending','uncertain') AND EXISTS(SELECT 1 FROM runner_drains d WHERE d.node=NEW.node AND d.state IN ('empty','retained','removed')) BEGIN SELECT RAISE(ABORT,'runner sealed for removal'); END;
      CREATE TRIGGER IF NOT EXISTS drain_no_placement_policy_clear BEFORE UPDATE ON runner_drains WHEN OLD.state IN ('draining','empty','retained','removed') AND NEW.state IN ('active','soft-draining') BEGIN SELECT RAISE(ABORT,'only soft drain can be cancelled'); END;")?;
    Ok(())
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn revision(db: &Connection) -> Result<i64> {
    Ok(
        db.query_row("SELECT version FROM drain_revision WHERE id=1", [], |r| {
            r.get(0)
        })?,
    )
}
pub(crate) fn blocked(db: &Connection, node: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM runner_drains WHERE node=?1 AND state<>'active')",
        [node],
        |r| r.get(0),
    )?)
}
fn status(db: &Connection, node: &str) -> Result<Value> {
    let result = db.query_row("SELECT state,eligible,generation,certificate,retention,error FROM runner_drains WHERE node=?1",[node],|r| Ok((r.get::<_,String>(0)?,r.get::<_,bool>(1)?,r.get::<_,i64>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<String>>(5)?))).optional()?;
    Ok(
        if let Some((state, eligible, generation, certificate, retention, error)) = result {
            json!({"node":node,"state":state,"scale_in_eligible":eligible,"generation":generation,"certificate":certificate.map(|s|serde_json::from_str::<Value>(&s)).transpose()?,"retention":retention.map(|s|serde_json::from_str::<Value>(&s)).transpose()?,"error":error})
        } else {
            json!({"node":node,"state":"active","scale_in_eligible":false})
        },
    )
}
/// Local owner command; no browser user or node credential can grant scale-in authority.
pub(crate) fn command(
    root: &Path,
    node: &str,
    action: &str,
    evidence: Option<&Path>,
    certificate: Option<&str>,
) -> Result<Value> {
    common::identifier(node)?;
    let mut db = nodes::db(root)?;
    ensure!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? >= 6,
        "upgrade the controller catalog first"
    );
    if action == "status" {
        return status(&db, node);
    }
    if matches!(action, "retain" | "remove") {
        let expected: Value = serde_json::from_str(&db.query_row(
            "SELECT certificate FROM runner_drains WHERE node=?1",
            [node],
            |r| r.get::<_, String>(0),
        )?)?;
        ensure!(
            certificate == expected["token"].as_str(),
            "exact certificate token required"
        );
        let observed = nodes::drain_inventory(root, node, expected["seal"].as_str(), false)?;
        let retention = if action == "retain" {
            let path = evidence.context("private provider retention evidence file required")?;
            ensure!(
                fs::metadata(path)?.len() <= 16384,
                "retention evidence too large"
            );
            let v: Value = serde_json::from_slice(&fs::read(path)?)?;
            ensure!(
                v["node"] == node
                    && v["dev_disk_retained"] == true
                    && v["proof"].as_str().is_some_and(|s| !s.trim().is_empty()),
                "positive retained dev-disk proof required"
            );
            Some(v)
        } else {
            None
        };
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        consume(&tx, node, &expected, &observed, retention.as_ref())?;
        tx.commit()?;
    } else {
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(
            tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM live_nodes WHERE id=?1 AND revoked=0)",
                [node],
                |r| r.get::<_, bool>(0)
            )?,
            "unknown or revoked runner"
        );
        tx.execute(
            "INSERT OR IGNORE INTO runner_drains(node) VALUES(?1)",
            [node],
        )?;
        match action {
            "allow" => {
                ensure!(
                    node != "local"
                        && !tx.query_row(
                            "SELECT id=?1 FROM live_nodes ORDER BY rowid LIMIT 1",
                            [node],
                            |r| r.get::<_, bool>(0)
                        )?,
                    "base runner is permanently ineligible for scale-in"
                );
                ensure!(!tx.query_row("SELECT EXISTS(SELECT 1 FROM resources WHERE node=?1 AND kind='machine' AND json_extract(metadata,'$.state')='running')",[node],|r|r.get::<_,bool>(0))?,"a running workstation cannot be enrolled for scale-in");
                tx.execute(
                    "UPDATE runner_drains SET eligible=1 WHERE node=?1 AND state='active'",
                    [node],
                )?;
            }
            "begin" => {
                ensure!(tx.execute("UPDATE runner_drains SET state='soft-draining',generation=generation+1,seal=lower(hex(randomblob(16))),certificate=NULL,error=NULL WHERE node=?1 AND eligible=1 AND state='active'",[node])?==1,"runner protected or drain already in progress");
            }
            "cancel" => {
                ensure!(tx.execute("UPDATE runner_drains SET state='active',generation=generation+1,seal=NULL,certificate=NULL,error=NULL WHERE node=?1 AND state='soft-draining'",[node])?==1,"only soft drain can be cancelled");
            }
            "reconcile" => {
                ensure!(tx.execute("UPDATE runner_drains SET certificate=NULL,error=NULL,state='draining' WHERE node=?1 AND state IN ('empty','retained','draining')",[node])?==1,"runner is not draining");
            }
            _ => anyhow::bail!("unknown drain action"),
        }
        tx.commit()?;
    }
    status(&db, node)
}
fn session(db: &Connection, node: &str) -> Result<String> {
    Ok(db.query_row("SELECT session FROM live_nodes WHERE id=?1 AND revoked=0 AND heartbeat>unixepoch()-15 AND heartbeat<=unixepoch()",[node],|r|r.get(0)).context("fresh runner generation required")?)
}
fn catalog_empty(db: &Connection, node: &str) -> Result<()> {
    for (sql, why) in [
        (
            "SELECT EXISTS(SELECT 1 FROM resources WHERE node=?1 AND kind='machine')",
            "workspace still placed",
        ),
        (
            "SELECT EXISTS(SELECT 1 FROM resources WHERE node=?1 AND (reserved_mib<>0 OR reserved_cpus<>0))",
            "unresolved resource reservation",
        ),
        (
            "SELECT EXISTS(SELECT 1 FROM transfers WHERE (source_node=?1 OR target=?1) AND step NOT IN ('succeeded','failed','aborted'))",
            "active transfer",
        ),
        (
            "SELECT EXISTS(SELECT 1 FROM node_usage WHERE node=?1 AND (extra_mib<>0 OR extra_cpus<>0 OR extra_slots<>0))",
            "unresolved unmanaged reservations",
        ),
        (
            "SELECT EXISTS(SELECT 1 FROM transfer_reservations WHERE node=?1)",
            "unresolved transfer reservation",
        ),
        (
            "SELECT EXISTS(SELECT 1 FROM operations WHERE node=?1 AND state IN ('pending','uncertain'))",
            "unresolved operation",
        ),
        (
            "SELECT EXISTS(SELECT 1 FROM artifact_copies c JOIN artifacts a ON a.id=c.artifact WHERE c.node=?1 AND (a.state<>'stored' OR c.state<>'present' OR EXISTS(SELECT 1 FROM leases l WHERE l.artifact=a.id AND l.expires>unixepoch())))",
            "unstored artifact or active lease",
        ),
        (
            "SELECT EXISTS(SELECT 1 FROM resources r WHERE r.kind='snapshot' AND r.node=?1 AND NOT EXISTS(SELECT 1 FROM artifacts a WHERE a.owner=r.owner AND a.local_name=r.physical AND a.state='stored' AND a.digest IS NOT NULL))",
            "unstored named snapshot",
        ),
    ] {
        ensure!(
            !db.query_row(sql, [node], |r| r.get::<_, bool>(0))?,
            "{why}; runner and disk retained"
        );
    }
    Ok(())
}
fn inventory_empty(db: &Connection, node: &str, inventory: &Value, seal: &str) -> Result<()> {
    ensure!(
        inventory["seal"] == seal
            && inventory["worker"]["pid"].is_u64()
            && inventory["worker"]["start"].is_string()
            && inventory["worker"]["boot"].is_string(),
        "unbound worker inventory"
    );
    ensure!(
        inventory["running"] == 0
            && inventory["unmanaged"]
                .as_array()
                .is_some_and(|a| a.is_empty()),
        "running/unknown VM or unmanaged local data; runner retained"
    );
    for m in inventory["machines"]
        .as_array()
        .context("missing VM inventory")?
    {
        ensure!(
            m["vmm_running"] == false
                && matches!(
                    m["lifecycle"]["phase"].as_str(),
                    Some("cleaned" | "discarded")
                ),
            "uncleaned or unknown VM; runner retained"
        );
    }
    for s in inventory["captures"]
        .as_array()
        .context("missing capture inventory")?
    {
        let native = &s["native"];
        let raw: Option<String> = db
            .query_row(
                "SELECT manifest FROM drain_publications WHERE node=?1 AND name=?2",
                params![node, s["name"].as_str()],
                |r| r.get(0),
            )
            .optional()?;
        // A publication is pinned to the exact native manifest, not just its reusable local name.
        let stored: Option<String> = if let Some(raw) = raw {
            Some(raw)
        } else {
            db.query_row("SELECT json_extract(o.response,'$.capture_manifest') FROM operations o JOIN transfers t ON t.operation=o.id JOIN artifacts a ON a.id=t.capture WHERE a.local_name=?1 AND a.machine=?2 AND a.state='stored' AND t.step='succeeded' AND json_type(o.response,'$.capture_manifest')='object' ORDER BY t.id DESC LIMIT 1",params![s["name"].as_str(),native["workspace"].as_str()],|r|r.get(0)).optional()?.flatten()
        };
        let manifest: store::Manifest =
            serde_json::from_str(stored.as_deref().context("unstored local capture")?)?;
        let stored_native: Value = serde_json::from_str(
            manifest.metadata["native_manifest"]
                .as_str()
                .context("stored native manifest missing")?,
        )?;
        ensure!(
            stored_native == *native,
            "local capture differs from stored capture"
        );
        ensure!(db.query_row("SELECT EXISTS(SELECT 1 FROM artifacts WHERE local_name=?1 AND machine=?2 AND state='stored' AND digest=?3)",params![s["name"].as_str(),native["workspace"].as_str(),store::sha256(&manifest.bytes()?)],|r|r.get::<_,bool>(0))?,"capture is no longer durably stored");
    }
    Ok(())
}
fn certify(
    db: &Connection,
    node: &str,
    generation: i64,
    version: i64,
    connection: &str,
    inventory: &Value,
) -> Result<Value> {
    let tx = rusqlite::Transaction::new_unchecked(db, rusqlite::TransactionBehavior::Immediate)?;
    let seal: String = tx.query_row(
        "SELECT seal FROM runner_drains WHERE node=?1 AND generation=?2 AND state='draining'",
        params![node, generation],
        |r| r.get(0),
    )?;
    ensure!(
        session(&tx, node)? == connection && revision(&tx)? == version,
        "runner/catalog changed during final inventory"
    );
    catalog_empty(&tx, node)?;
    inventory_empty(&tx, node, inventory, &seal)?;
    let certificate = json!({"token":common::nonce()?,"node":node,"generation":generation,"version":version,"session":connection,"inventory":store::sha256(&serde_json::to_vec(inventory)?),"worker":inventory["worker"],"seal":seal,"issued":now()});
    tx.execute("UPDATE runner_drains SET state='empty',certificate=?1,error=NULL WHERE node=?2 AND generation=?3",params![certificate.to_string(),node,generation])?;
    tx.commit()?;
    Ok(certificate)
}
pub(crate) fn consume(
    db: &Connection,
    node: &str,
    certificate: &Value,
    inventory: &Value,
    retention: Option<&Value>,
) -> Result<()> {
    let (state, raw, proof): (String, String, Option<String>) = db.query_row(
        "SELECT state,certificate,retention FROM runner_drains WHERE node=?1 AND generation=?2",
        params![node, certificate["generation"].as_i64()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    ensure!(
        serde_json::from_str::<Value>(&raw)? == *certificate && certificate["node"] == node,
        "stale certificate"
    );
    ensure!(
        certificate["session"] == session(db, node)?
            && certificate["version"] == revision(db)?
            && certificate["inventory"] == store::sha256(&serde_json::to_vec(inventory)?),
        "stale runner generation, inventory or catalog certificate"
    );
    catalog_empty(db, node)?;
    inventory_empty(
        db,
        node,
        inventory,
        certificate["seal"]
            .as_str()
            .context("certificate seal missing")?,
    )?;
    if let Some(proof) = retention {
        ensure!(
            proof["node"] == node
                && proof["dev_disk_retained"] == true
                && proof["proof"]
                    .as_str()
                    .is_some_and(|s| !s.trim().is_empty()),
            "positive retained dev-disk proof required"
        );
        ensure!(state == "empty", "retention needs an empty certificate");
        db.execute(
            "UPDATE runner_drains SET state='retained',retention=?1 WHERE node=?2",
            params![proof.to_string(), node],
        )?;
    } else {
        ensure!(
            state == "retained" && proof.is_some(),
            "retain the dev disk before removal"
        );
        db.execute(
            "UPDATE runner_drains SET state='removed' WHERE node=?1",
            [node],
        )?;
        db.execute("UPDATE nodes SET revoked=1,heartbeat=0 WHERE id=?1", [node])?;
    }
    Ok(())
}

/// One bounded pass from the existing transfer reconciler; never holds a database writer over RPC.
pub(crate) fn reconcile(shared: &Arc<Mutex<Catalog>>) {
    let rows = (|| -> Result<Vec<String>> {
        let c = shared.lock().unwrap();
        Ok(c.db
            .prepare("SELECT node FROM runner_drains WHERE state IN ('soft-draining','draining')")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    })();
    if let Ok(rows) = rows {
        for node in rows {
            let root = shared.lock().unwrap().root.clone();
            let lock = (|| -> Result<fs::File> {
                let f = fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .mode(0o600)
                    .open(root.join(format!("drain-{node}.lock")))?;
                ensure!(
                    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
                    "drain busy"
                );
                Ok(f)
            })();
            if let Ok(_lock) = lock {
                if let Err(e) = pass(shared, &node, &|n, v| rpc(&root, n, v)) {
                    let c = shared.lock().unwrap();
                    let _ = c.db.execute(
                        "UPDATE runner_drains SET error=?1 WHERE node=?2",
                        params![format!("{e:#}"), node],
                    );
                }
            }
        }
    }
}
fn rpc(root: &Path, node: &str, value: Value) -> Result<Value> {
    Ok(wire::connect_waiting(
        &nodes::route(root, node)?,
        &value,
        Duration::from_secs(if value["op"] == "drain-inventory" {
            10
        } else {
            7200
        }),
    )?
    .1)
}
pub(crate) fn pass(
    shared: &Arc<Mutex<Catalog>>,
    node: &str,
    call: &impl Fn(&str, Value) -> Result<Value>,
) -> Result<()> {
    let (state, generation, seal, connection) = {
        let c = shared.lock().unwrap();
        let row = c.db.query_row(
            "SELECT state,generation,seal FROM runner_drains WHERE node=?1",
            [node],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )?;
        (row.0, row.1, row.2, session(&c.db, node)?)
    };
    {
        let c = shared.lock().unwrap();
        let caps: String = c.db.query_row(
            "SELECT capabilities FROM live_nodes WHERE id=?1",
            [node],
            |r| r.get(0),
        )?;
        ensure!(
            serde_json::from_str::<Value>(&caps)?["drain_inventory_v1"] == true,
            "runner lacks bounded drain inventory"
        );
    }
    let inventory = call(node, json!({"op":"drain-inventory"}))?;
    ensure!(
        inventory["unmanaged"]
            .as_array()
            .is_some_and(|a| a.is_empty()),
        "unmanaged local data; retained"
    );
    ensure!(
        inventory["running"] == 0,
        "waiting for running workloads; no VM stopped or migrated"
    );
    let machines = inventory["machines"]
        .as_array()
        .context("missing VM inventory")?;
    ensure!(
        machines.iter().all(|m| matches!(
            m["state"].as_str(),
            Some(
                "stopped"
                    | "hibernated"
                    | "exported"
                    | "exported-cleaned"
                    | "deleted"
                    | "discarded"
            )
        )),
        "unknown VM state; retained"
    );
    {
        let c = shared.lock().unwrap();
        ensure!(
            session(&c.db, node)? == connection,
            "runner generation changed"
        );
        if state == "soft-draining" {
            ensure!(!c.db.query_row("SELECT EXISTS(SELECT 1 FROM transfers WHERE (source_node=?1 OR target=?1) AND step NOT IN ('succeeded','failed','aborted')) OR EXISTS(SELECT 1 FROM transfer_reservations WHERE node=?1) OR EXISTS(SELECT 1 FROM operations WHERE node=?1 AND state IN ('pending','uncertain'))",[node],|r|r.get::<_,bool>(0))?,"waiting for already admitted effects; runner retained");
            ensure!(c.db.execute("UPDATE runner_drains SET state='draining',error=NULL WHERE node=?1 AND generation=?2 AND state='soft-draining'",params![node,generation])?==1,"drain cancelled");
        }
    }
    for m in machines {
        if matches!(
            m["lifecycle"]["phase"].as_str(),
            Some("cleaned" | "discarded")
        ) {
            continue;
        }
        if m["lifecycle"]["phase"] == "exported" {
            transfers::drain_cleanup(shared, node, m, call)?;
            continue;
        }
        transfers::drain_park(shared, node, generation, m, call)?;
        return Ok(()); // Existing transfer runner owns capture/publication and retries.
    }
    for capture in inventory["captures"]
        .as_array()
        .context("missing capture inventory")?
    {
        transfers::drain_publish(shared, node, capture, call)?;
    }
    let version = {
        let c = shared.lock().unwrap();
        c.db.execute("DELETE FROM leases WHERE purpose IN ('push','pull') AND EXISTS(SELECT 1 FROM transfers t JOIN artifacts a ON a.id=t.capture WHERE EXISTS(SELECT 1 FROM artifact_copies c WHERE c.node=?1 AND c.artifact=a.id) AND t.step='succeeded' AND a.state='stored' AND a.id=leases.artifact AND (leases.holder='transfer-'||t.id OR leases.holder='transfer-'||t.id||'-pull') AND (leases.purpose='pull' OR a.attempt=leases.token))",[node])?;
        catalog_empty(&c.db, node)?;
        revision(&c.db)?
    };
    let final_inventory = call(
        node,
        json!({"op":"drain-inventory","seal":seal,"close":true}),
    )?;
    let c = shared.lock().unwrap();
    certify(
        &c.db,
        node,
        generation,
        version,
        &connection,
        &final_inventory,
    )?;
    Ok(())
}

/// Every node job holds this shared lock across effects; final sealing takes it exclusively.
pub(crate) fn job_guard(root: &Path, request: &Value) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join("drain-jobs.lock"))?;
    let inventory = request["op"] == "drain-inventory";
    ensure!(
        unsafe {
            libc::flock(
                file.as_raw_fd(),
                (if inventory {
                    libc::LOCK_EX
                } else {
                    libc::LOCK_SH
                }) | libc::LOCK_NB,
            )
        } == 0,
        "runner inventory/effects busy; retry"
    );
    if root.join("drain-seal.json").exists() && !inventory {
        ensure!(
            matches!(
                request["op"].as_str(),
                Some(
                    "status"
                        | "list"
                        | "inspect"
                        | "snapshots"
                        | "stats"
                        | "ssh-info"
                        | "ssh-policy"
                        | "capture-progress"
                        | "capture-published"
                )
            ),
            "runner sealed for removal"
        );
    }
    Ok(file)
}
pub(crate) fn agent_inventory(root: &Path, request: &Value) -> Result<Value> {
    if let Some(seal) = request["seal"].as_str() {
        common::identifier(seal)?;
        let path = root.join("drain-seal.json");
        if path.exists() {
            ensure!(
                serde_json::from_slice::<Value>(&fs::read(&path)?)?["seal"] == seal,
                "another drain owns the seal"
            );
        }
        if request["close"] == true {
            ow_core::runtime::atomic_json(&path, &json!({"seal":seal}))?;
        }
    }
    if root.join("drain-seal.json").exists() {
        wire::connect_waiting(
            root,
            &json!({"op":"quiesce","closing":true}),
            Duration::from_secs(3),
        )?;
    }
    let status = wire::connect_waiting(root, &json!({"op":"status"}), Duration::from_secs(3))?.1;
    let machines = wire::connect_waiting(root, &json!({"op":"list"}), Duration::from_secs(3))?.1;
    let pid = status["worker_pid"]
        .as_u64()
        .context("worker identity missing")?;
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let start = stat
        .rsplit_once(") ")
        .context("invalid worker stat")?
        .1
        .split_whitespace()
        .nth(19)
        .context("worker start missing")?;
    let mut unmanaged = Vec::new();
    let machines_root = fs::canonicalize(root)?.join("machines");
    for (count, entry) in fs::read_dir("/proc")?.enumerate() {
        ensure!(count < 16384, "process inventory too large");
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_none_or(|s| s.parse::<u32>().is_err())
        {
            continue;
        }
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = entry.metadata() else { continue };
        if meta.uid() != unsafe { libc::geteuid() } {
            continue;
        }
        match fs::read_link(entry.path().join("cwd")) {
            Ok(cwd) if cwd.starts_with(&machines_root) => unmanaged.push(format!(
                "live process {} in workspace data",
                entry.file_name().to_string_lossy()
            )),
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                anyhow::bail!("owned process inventory unavailable")
            }
            _ => {}
        }
    }
    for (count, entry) in fs::read_dir(root)?.take(4097).enumerate() {
        ensure!(count < 4096, "runner root inventory too large");
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().context("invalid root data name")?;
        if !matches!(
            name,
            "machines"
                | "snapshots"
                | "recovery"
                | "images"
                | "assets"
                | "transfer-progress"
                | "control.sock"
                | "worker.lock"
                | "worker.log"
                | "state.json"
                | "node-agent.lock"
                | "node-channel.json"
                | "agent.json"
                | "node.json"
                | "controller-ca.pem"
                | "store-url"
                | "host.json"
                | "host.pending.json"
                | "host-lifecycle.lock"
                | "host-agent.log"
                | "recovery-upload.lock"
                | "recovery-index"
                | "cache.lock"
                | "cache-space.lock"
                | "drain"
                | "drain-seal.json"
                | "drain-jobs.lock"
                | "node-operations.sqlite3"
                | "node-operations.sqlite3-wal"
                | "node-operations.sqlite3-shm"
                | ".ow-data"
                | ".cache-staging"
                | "cache-evictions"
        ) {
            unmanaged.push(name.to_owned());
        }
    }
    if ow_core::cache::staging_bytes(root)? > 0 {
        unmanaged.push("active cache staging".into());
    }
    let mut captures = Vec::new();
    let mut recovery_journals = Vec::new();
    let mut capture_bytes = 0;
    for dir in ["machines", "snapshots", "recovery", "images"] {
        let path = root.join(dir);
        if !path.exists() {
            continue;
        }
        if !fs::symlink_metadata(&path)?.is_dir() {
            unmanaged.push(dir.to_owned());
            continue;
        }
        for (count, entry) in fs::read_dir(path)?.take(4097).enumerate() {
            ensure!(count < 4096, "runner inventory too large");
            let entry = entry?;
            let name = entry
                .file_name()
                .to_str()
                .context("invalid data name")?
                .to_owned();
            if dir == "machines" {
                if !entry.file_type()?.is_dir()
                    || !machines
                        .as_array()
                        .context("invalid machines")?
                        .iter()
                        .any(|m| {
                            m["id"] == name
                                && m["lifecycle"]["phase"] != "cleaned"
                                && m["lifecycle"]["phase"] != "discarded"
                        })
                {
                    unmanaged.push(format!("{dir}/{name}"));
                }
            } else if dir == "snapshots" && entry.file_type()?.is_dir() && !name.starts_with('.') {
                let manifest = entry.path().join("manifest.json");
                let meta = fs::symlink_metadata(&manifest)?;
                ensure!(
                    meta.is_file() && meta.len() <= 65536,
                    "native manifest oversized or not regular"
                );
                let raw = fs::read(manifest)?;
                capture_bytes += raw.len();
                ensure!(capture_bytes <= 512 * 1024, "capture inventory oversized");
                for (index, part) in fs::read_dir(entry.path())?.take(17).enumerate() {
                    ensure!(index < 16, "capture file inventory too large");
                    let part = part?;
                    let part_name = part.file_name();
                    let part_name = part_name.to_str().context("invalid capture file name")?;
                    if !part.file_type()?.is_file()
                        || !(matches!(
                            part_name,
                            "disk.ext4"
                                | "state"
                                | "memory"
                                | "manifest.json"
                                | "chunks.json"
                                | ".artifact.json"
                        ) || part_name.starts_with(".published-"))
                    {
                        unmanaged.push(format!("snapshots/{name}/{part_name}"));
                    }
                }
                captures.push(json!({"name":name,"native":serde_json::from_slice::<Value>(&raw)?,"disk_bytes":fs::metadata(entry.path().join("disk.ext4"))?.len()}));
            } else if dir == "snapshots"
                && entry.file_type()?.is_file()
                && name.starts_with('.')
                && name.ends_with(".lock")
                && entry.metadata()?.len() == 0
            {
                // Persistent advisory lock files carry no guest data.
            } else if dir == "recovery" && entry.file_type()?.is_file() {
                let meta = entry.metadata()?;
                ensure!(meta.len() <= 65536, "recovery journal oversized");
                let raw = fs::read(entry.path())?;
                capture_bytes += raw.len();
                ensure!(capture_bytes <= 512 * 1024, "recovery inventory oversized");
                let value: Value = serde_json::from_slice(&raw)?;
                let journal: ow_core::recovery::Recovery = serde_json::from_value(value.clone())?;
                let id = name.strip_suffix(".json").unwrap_or("");
                let point_valid = |point: &ow_core::recovery::Point| {
                    common::identifier(&point.snapshot).is_ok()
                        && point
                            .source
                            .as_deref()
                            .is_none_or(|s| common::identifier(s).is_ok())
                };
                if common::identifier(id).is_ok()
                    && machines
                        .as_array()
                        .context("invalid machines")?
                        .iter()
                        .any(|m| m["id"] == id)
                    && journal.store.len() == 64
                    && journal.store.bytes().all(|b| b.is_ascii_hexdigit())
                    && serde_json::to_value(&journal)? == value
                    && journal.pending.is_none()
                    && journal
                        .sleep
                        .as_deref()
                        .is_none_or(|s| common::identifier(s).is_ok())
                    && journal.durable.as_ref().is_none_or(|p| {
                        point_valid(p)
                            && captures
                                .iter()
                                .any(|c| c["name"] == p.snapshot && c["native"]["workspace"] == id)
                    })
                {
                    // Settled scheduling metadata carries no disk; captures are checked separately.
                    recovery_journals.push(json!({"machine":id,"journal":value}));
                } else {
                    unmanaged.push(format!("{dir}/{name}"));
                }
            } else if dir == "images" {
                // Only digest-verified immutable image caches may be left on a retained disk.
                let entries = ow_core::cache::inventory(root)?;
                if !entries.iter().any(|e| e.name == name) {
                    unmanaged.push(format!("{dir}/{name}"));
                }
            } else {
                unmanaged.push(format!("{dir}/{name}"));
            }
        }
    }
    captures.sort_by_key(|v| v["name"].as_str().unwrap_or_default().to_owned());
    unmanaged.sort();
    recovery_journals.sort_by_key(|v| v["machine"].as_str().unwrap_or_default().to_owned());
    let seal = if root.join("drain-seal.json").exists() {
        serde_json::from_slice::<Value>(&fs::read(root.join("drain-seal.json"))?)?["seal"].clone()
    } else {
        Value::Null
    };
    let result = json!({"worker":{"pid":pid,"start":start,"boot":fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim()},"running":status["running"],"machines":machines,"captures":captures,"recovery_journals":recovery_journals,"unmanaged":unmanaged,"seal":seal});
    ensure!(
        serde_json::to_vec(&result)?.len() < 900_000,
        "runner inventory too large"
    );
    Ok(result)
}

#[cfg(test)]
#[path = "drain_tests.rs"]
mod tests;
