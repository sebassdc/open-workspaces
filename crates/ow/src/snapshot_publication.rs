//! Named snapshots of managed boxes live in the artifact store, so they outlast their runner.
//! The recovery loop publishes each one once, rehomes it to the store when its box leaves that
//! runner, and then removes runner copies that no catalog row places there any more.
use super::*;

pub(crate) fn reconcile_snapshots(shared: &Shared) {
    static RUNNING: Mutex<std::collections::BTreeSet<std::path::PathBuf>> =
        Mutex::new(std::collections::BTreeSet::new());
    let root = shared.lock().unwrap().root.clone();
    if !RUNNING.lock().unwrap().insert(root.clone()) {
        return;
    }
    let shared = shared.clone();
    std::thread::spawn(move || {
        let call = |node: &str, request: Value| rpc(&root, node, request);
        for (item, result) in pass(&shared, &call) {
            report(&item, result);
        }
        RUNNING.lock().unwrap().remove(&root);
    });
}

/// Logs a failure once until it changes: the loop retries every few seconds.
fn report(item: &str, result: Result<()>) {
    static LAST: Mutex<std::collections::BTreeMap<String, String>> =
        Mutex::new(std::collections::BTreeMap::new());
    let mut last = LAST.lock().unwrap();
    match result {
        Ok(()) => {
            last.remove(item);
        }
        Err(error) => {
            let text = format!("{error:#}");
            if last.get(item) != Some(&text) {
                eprintln!("snapshot publication {item}: {text}");
                last.insert(item.to_owned(), text);
            }
        }
    }
}

pub(super) fn pass(
    shared: &Shared,
    call: &impl Fn(&str, Value) -> Result<Value>,
) -> Vec<(String, Result<()>)> {
    let mut results = vec![("sizes".to_owned(), sizes(shared, call))];
    let unpublished = (|| -> Result<Vec<(String, String, String, (u32, u32, u64))>> {
        let c = shared.lock().unwrap();
        let mut stmt = c.db.prepare("SELECT s.node,s.physical,m.physical,json_extract(s.metadata,'$.memory_mib'),json_extract(s.metadata,'$.vcpu_count'),json_extract(s.metadata,'$.disk_bytes') FROM resources s JOIN resources m ON m.kind='machine' AND m.physical=json_extract(s.metadata,'$.workspace') JOIN nodes n ON n.id=s.node WHERE s.kind='snapshot' AND s.node<>'' AND json_extract(s.metadata,'$.state')='ready' AND m.placement_managed=1 AND json_extract(s.metadata,'$.disk_bytes') IS NOT NULL AND json_extract(n.capabilities,'$.managed_restore_v1')=1 AND NOT EXISTS(SELECT 1 FROM drain_publications p WHERE p.node=s.node AND p.name=s.physical) ORDER BY s.created_at")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    (r.get(3)?, r.get(4)?, r.get::<_, i64>(5)? as u64),
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    })();
    match unpublished {
        Ok(rows) => {
            for (node, snapshot, machine, shape) in rows {
                if nodes::online(&shared.lock().unwrap().root, &node).is_err() {
                    continue;
                }
                let result =
                    draining::publish_named(shared, &node, &snapshot, &machine, shape, None, call);
                results.push((format!("{node}/{snapshot}"), result));
            }
        }
        Err(error) => results.push(("scan".into(), Err(error))),
    }
    results.push(("rehome".into(), rehome(shared)));
    match superseded(shared) {
        Ok(copies) => {
            for (node, snapshot, machine) in copies {
                let result = remove_copy(shared, &node, &snapshot, &machine, call);
                results.push((format!("{node}/{snapshot} copy"), result));
            }
        }
        Err(error) => results.push(("copies".into(), Err(error))),
    }
    results
}

/// Snapshots recorded before captures carried a disk size (or by an older worker) get it
/// from their runner's inventory, which upgraded workers report, so they can be published.
fn sizes(shared: &Shared, call: &impl Fn(&str, Value) -> Result<Value>) -> Result<()> {
    let nodes: Vec<String> = {
        let c = shared.lock().unwrap();
        let mut stmt = c.db.prepare("SELECT DISTINCT s.node FROM resources s JOIN resources m ON m.kind='machine' AND m.physical=json_extract(s.metadata,'$.workspace') JOIN nodes n ON n.id=s.node WHERE s.kind='snapshot' AND s.node<>'' AND json_extract(s.metadata,'$.state')='ready' AND json_extract(s.metadata,'$.disk_bytes') IS NULL AND m.placement_managed=1 AND json_extract(n.capabilities,'$.managed_lifecycle_v1')=1 ORDER BY s.node")?;
        stmt.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    for node in nodes {
        if nodes::online(&shared.lock().unwrap().root, &node).is_err() {
            continue;
        }
        let listed = call(&node, json!({"op":"snapshots"}))?;
        let c = shared.lock().unwrap();
        for s in listed.as_array().context("invalid snapshot inventory")? {
            if let (Some(name), Some(workspace), Some(bytes)) = (
                s["name"].as_str(),
                s["workspace"].as_str(),
                s["disk_bytes"].as_u64(),
            ) {
                c.db.execute("UPDATE resources SET metadata=json_set(metadata,'$.disk_bytes',?1),updated_at=CURRENT_TIMESTAMP WHERE kind='snapshot' AND node=?2 AND physical=?3 AND json_extract(metadata,'$.workspace')=?4 AND json_extract(metadata,'$.disk_bytes') IS NULL",params![bytes as i64,node,name,workspace])?;
            }
        }
    }
    Ok(())
}

/// A published snapshot whose box left its runner belongs to the store, not to that runner.
fn rehome(shared: &Shared) -> Result<()> {
    let c = shared.lock().unwrap();
    c.db.execute("UPDATE resources SET node='',updated_at=CURRENT_TIMESTAMP WHERE kind='snapshot' AND node<>'' AND EXISTS(SELECT 1 FROM drain_publications p JOIN artifacts a ON a.id=p.artifact WHERE p.node=resources.node AND p.name=resources.physical AND a.state='stored' AND a.owner=resources.owner) AND node<>(SELECT m.node FROM resources m WHERE m.kind='machine' AND m.physical=json_extract(resources.metadata,'$.workspace') AND m.placement_managed=1 AND NOT EXISTS(SELECT 1 FROM transfer_locks l WHERE l.machine=m.physical))",[])?;
    Ok(())
}

/// Runner copies with a publication record but no catalog row placing them on that runner:
/// the snapshot was deleted or rehomed. A box being moved or forked keeps its copies.
fn superseded(shared: &Shared) -> Result<Vec<(String, String, String)>> {
    let c = shared.lock().unwrap();
    let mut stmt = c.db.prepare("SELECT p.node,p.name,a.machine FROM drain_publications p JOIN artifacts a ON a.id=p.artifact WHERE NOT EXISTS(SELECT 1 FROM resources s WHERE s.kind='snapshot' AND s.physical=p.name AND s.node=p.node) AND NOT EXISTS(SELECT 1 FROM transfer_locks l WHERE l.machine=a.machine) ORDER BY p.node,p.name")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn remove_copy(
    shared: &Shared,
    node: &str,
    snapshot: &str,
    machine: &str,
    call: &impl Fn(&str, Value) -> Result<Value>,
) -> Result<()> {
    let forget = || -> Result<()> {
        shared.lock().unwrap().db.execute(
            "DELETE FROM drain_publications WHERE node=?1 AND name=?2",
            params![node, snapshot],
        )?;
        Ok(())
    };
    let (known, retired, capable): (bool, bool, bool) = {
        let c = shared.lock().unwrap();
        c.db.query_row("SELECT EXISTS(SELECT 1 FROM nodes WHERE id=?1 AND revoked=0),EXISTS(SELECT 1 FROM runner_drains WHERE node=?1 AND state IN ('retained','removed')),COALESCE((SELECT json_extract(capabilities,'$.managed_lifecycle_v1')=1 FROM nodes WHERE id=?1),0)",[node],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?
    };
    // A retired or removed runner runs nothing; only its publication root is released.
    if !known || retired {
        return forget();
    }
    if nodes::online(&shared.lock().unwrap().root, node).is_err() {
        return Ok(());
    }
    ensure!(
        capable,
        "runner {node} cannot remove a moved box's snapshot until its worker is upgraded"
    );
    let mut request = json!({"op":"snapshot-delete","id":machine,"name":snapshot});
    match call(node, json!({"op":"inspect","id":machine})) {
        Ok(seen) => {
            if !seen["placement"].is_null() {
                request["placement"] = seen["placement"].clone();
            }
        }
        // That runner no longer knows the box. A copy there (one a fork pulled, say) has no
        // owner to delete it through, so its record stays as long as the runner lists it.
        Err(error) if format!("{error:#}").contains("workspace not found") => {
            let listed = call(node, json!({"op":"snapshots"}))?;
            let held = listed
                .as_array()
                .context("invalid snapshot inventory")?
                .iter()
                .any(|s| s["name"] == snapshot);
            return if held { Ok(()) } else { forget() };
        }
        Err(error) => return Err(error),
    }
    let result = call(node, request)?;
    ensure!(
        result["deleted"] == snapshot && result["workspace"] == machine,
        "snapshot copy removal unconfirmed"
    );
    forget()
}
