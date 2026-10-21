//! Drain composes the existing parked placement and immutable publication primitives.
use super::*;

pub(crate) fn drain_park(
    shared: &Shared,
    node: &str,
    generation: i64,
    seen: &Value,
    call: &impl Fn(&str, Value) -> Result<Value>,
) -> Result<()> {
    let machine = seen["id"].as_str().context("unknown VM identity")?;
    let policy: PublicSshPolicy =
        serde_json::from_value(call(node, json!({"op":"restore-policy","id":machine}))?)?;
    let c = shared.lock().unwrap();
    let (user, address): (i64, String) =
        c.db.query_row(
            "SELECT owner,name FROM resources WHERE kind='machine' AND physical=?1 AND node=?2",
            params![machine, node],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .context("unmanaged local workspace; retained")?;
    if let Some(id) =
        c.db.query_row(
            "SELECT transfer FROM transfer_locks WHERE machine=?1",
            [machine],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
    {
        drop(c);
        launch(shared.clone(), id);
        return Ok(());
    }
    let old: Option<String> = c.db.query_row(
        "SELECT ssh_policy FROM resources WHERE kind='machine' AND physical=?1",
        [machine],
        |r| r.get(0),
    )?;
    if let Some(old) = old {
        ensure!(
            same_policy(&serde_json::from_str::<Value>(&old)?, &json!(policy)),
            "public policy differs from catalog"
        );
    } else {
        let g = c.db.query_row(
            "SELECT ssh_policy_generation FROM resources WHERE kind='machine' AND physical=?1",
            [machine],
            |r| r.get(0),
        )?;
        c.set_transfer_ssh_policy(user, &address, g, &policy)?;
    }
    let request = json!({"op":"park","id":address,"operation_key":format!("d-{}",&store::sha256(format!("{node}:{machine}:{generation}").as_bytes())[..30])});
    let t = parking::admission(&c, user, &request, Kind::Park)?;
    drop(c);
    launch(shared.clone(), t.id);
    Ok(())
}
pub(crate) fn drain_cleanup(
    shared: &Shared,
    node: &str,
    seen: &Value,
    call: &impl Fn(&str, Value) -> Result<Value>,
) -> Result<()> {
    let id = seen["lifecycle"]["intent"]["transfer"]
        .as_str()
        .context("export intent missing")?
        .strip_prefix("transfer-")
        .context("unknown export intent")?
        .parse::<i64>()?;
    let c = shared.lock().unwrap();
    let t = c.transfer(id)?;
    ensure!(
        t.step == Step::Succeeded
            && t.source_node == node
            && seen["id"] == t.source
            && matches!(t.kind, Kind::Park | Kind::Move | Kind::Restore),
        "unfinished source export; retained"
    );
    let placement = movement::source_authority(&c, &t)?;
    source_fence(&c, &t, seen, &placement)?;
    ensure!(
        t.capture.as_ref().is_some_and(|a| c
            .db
            .query_row(
                "SELECT state='stored' AND digest IS NOT NULL FROM artifacts WHERE id=?1",
                [a],
                |r| r.get::<_, bool>(0)
            )
            .unwrap_or(false)),
        "source capture not durable"
    );
    // A deleted box no longer owns any placement.
    ensure!(c.db.query_row("SELECT node<>?2 AND placement_epoch>=?3 FROM resources WHERE kind='machine' AND physical=?1",params![t.machine,node,t.epoch],|r|r.get::<_,bool>(0)).optional()?.unwrap_or(true),"source still owns placement");
    drop(c);
    let result = call(
        node,
        json!({"op":"export-cleanup","id":t.source,"placement":placement,"transfer":name(&t),"capture":seen["lifecycle"]["intent"]["capture"],"operation_key":step_key(&t,"cleanup")}),
    )?;
    ensure!(
        result["vmm_running"] == false
            && result["lifecycle"]["phase"] == "cleaned"
            && result["placement"] == json!(placement),
        "source cleanup unconfirmed"
    );
    Ok(())
}
pub(crate) fn drain_publish(
    shared: &Shared,
    node: &str,
    capture: &Value,
    call: &impl Fn(&str, Value) -> Result<Value>,
) -> Result<()> {
    let native = &capture["native"];
    let shape = (
        native["memory_mib"]
            .as_u64()
            .context("capture memory unknown")?
            .try_into()?,
        native["vcpu_count"]
            .as_u64()
            .context("capture CPU unknown")?
            .try_into()?,
        capture["disk_bytes"]
            .as_u64()
            .context("capture disk unknown")?,
    );
    publish_named(
        shared,
        node,
        native["name"].as_str().context("capture name missing")?,
        native["workspace"]
            .as_str()
            .context("capture lineage missing")?,
        shape,
        Some(native),
        call,
    )
}
/// Publishes a runner's named capture once, recorded per runner and name. `expected` pins the
/// exact native manifest the runner reported; without it the catalog shape and hashes decide.
pub(super) fn publish_named(
    shared: &Shared,
    node: &str,
    snapshot: &str,
    machine: &str,
    (memory_mib, cpus, disk_bytes): (u32, u32, u64),
    expected: Option<&Value>,
    call: &impl Fn(&str, Value) -> Result<Value>,
) -> Result<()> {
    let (a, expires) = {
        let c = shared.lock().unwrap();
        // Already published transfer captures keep their original artifact/root; do not duplicate
        // them for drain. A named snapshot is published as one, so it forks and rehomes.
        if expected.is_some() && c.db.query_row("SELECT EXISTS(SELECT 1 FROM artifacts a JOIN transfers t ON t.capture=a.id JOIN operations o ON o.id=t.operation WHERE a.local_name=?1 AND a.machine=?2 AND a.state='stored' AND t.step='succeeded' AND json_extract(o.response,'$.capture_manifest.metadata.native_manifest') IS NOT NULL)",params![snapshot,machine],|r|r.get::<_,bool>(0))? {return Ok(());}
        let owner:i64=c.db.query_row("SELECT owner FROM resources WHERE (kind='snapshot' AND physical=?1 AND json_extract(metadata,'$.workspace')=?2) OR (kind='machine' AND physical=?2 AND ?1 LIKE 'sleep-%')",params![snapshot,machine],|r|r.get(0)).context("unmanaged or incomplete local capture; retained")?;
        let mut id = format!(
            "drain-{}",
            &store::sha256(format!("{node}:{snapshot}").as_bytes())[..26]
        );
        // A copy published before, then removed and since pulled back onto this runner, has a
        // stored artifact but no record here; its new record gets the next ID in this chain.
        while c.db.query_row("SELECT EXISTS(SELECT 1 FROM artifacts WHERE id=?1 AND state<>'uploading') AND NOT EXISTS(SELECT 1 FROM drain_publications WHERE node=?2 AND name=?3)",params![id,node,snapshot],|r|r.get::<_,bool>(0))? {
            id = format!("drain-{}", &store::sha256(id.as_bytes())[..26]);
        }
        let url = crate::image_store::configured_url(&c.root)?;
        let tx =
            rusqlite::Transaction::new_unchecked(&c.db, rusqlite::TransactionBehavior::Immediate)?;
        if let Some((raw, artifact)) = tx
            .query_row(
                "SELECT manifest,artifact FROM drain_publications WHERE node=?1 AND name=?2",
                params![node, snapshot],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
        {
            let manifest: store::Manifest = serde_json::from_str(&raw)?;
            let stored: Value = serde_json::from_str(
                manifest.metadata["native_manifest"]
                    .as_str()
                    .context("native manifest missing")?,
            )?;
            ensure!(
                expected.is_none_or(|native| *native == stored),
                "named capture changed"
            );
            ensure!(
                tx.query_row(
                    "SELECT state='stored' AND digest=?2 FROM artifacts WHERE id=?1",
                    params![artifact, store::sha256(&manifest.bytes()?)],
                    |r| r.get::<_, bool>(0)
                )?,
                "named capture is no longer stored"
            );
            tx.commit()?;
            return Ok(());
        }
        tx.execute("INSERT OR IGNORE INTO artifacts(id,kind,owner,machine,local_name,state,attempt,store_url) VALUES(?1,'snapshot',?2,?3,?4,'uploading',?5,?6)",params![id,owner,machine,snapshot,store::token()?,url])?;
        let (mut token,old_url):(String,String)=tx.query_row("SELECT attempt,store_url FROM artifacts WHERE id=?1 AND owner=?2 AND machine=?3 AND local_name=?4 AND state='uploading'",params![id,owner,machine,snapshot],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(old_url == url, "publication store changed");
        let mut expires: Option<i64> = tx
            .query_row(
                "SELECT expires FROM leases WHERE artifact=?1 AND token=?2",
                params![id, token],
                |r| r.get(0),
            )
            .optional()?;
        if expires.is_none_or(|e| e <= now()) {
            if expires.is_some() {
                token = store::token()?;
                tx.execute(
                    "UPDATE artifacts SET attempt=?1 WHERE id=?2",
                    params![token, id],
                )?;
            }
            tx.execute("INSERT INTO leases(artifact,holder,purpose,token,expires) VALUES(?1,?2,'push',?3,?4)",params![id,format!("drain-{node}"),token,now()+7200])?;
            expires = Some(now() + 7200);
        }
        let a = Artifact {
            id,
            machine: machine.into(),
            snapshot: snapshot.into(),
            store_url: url,
            reference: Reference {
                attempt: token,
                digest: String::new(),
            },
            memory_mib,
            cpus,
            disk_bytes,
        };
        tx.commit()?;
        (a, expires.unwrap())
    };
    let mut request = json!({"op":"capture-published","artifact":a,"fork":true});
    let mut result = call(node, request.clone())?;
    if result["present"] != true {
        request["op"] = json!("capture-push");
        request["expires"] = json!(expires);
        result = call(node, request)?;
    }
    let reference: Reference = serde_json::from_value(result["reference"].clone())?;
    let manifest: store::Manifest = serde_json::from_value(result["manifest"].clone())?;
    let mut pinned = a.clone();
    pinned.reference = reference.clone();
    let (_, observed) = native(&manifest, &pinned)?;
    ensure!(
        expected.is_none_or(|native| *native == observed)
            && reference.attempt == a.reference.attempt
            && store::sha256(&manifest.bytes()?) == reference.digest,
        "named snapshot publication differs"
    );
    let c = shared.lock().unwrap();
    let tx = rusqlite::Transaction::new_unchecked(&c.db, rusqlite::TransactionBehavior::Immediate)?;
    ensure!(tx.execute("UPDATE artifacts SET state='stored',digest=?1,stored_at=CURRENT_TIMESTAMP WHERE id=?2 AND attempt=?3 AND state='uploading'",params![reference.digest,a.id,reference.attempt])?==1,"publication changed");
    tx.execute(
        "INSERT INTO drain_publications(node,name,artifact,manifest) VALUES(?1,?2,?3,?4)",
        params![node, snapshot, a.id, serde_json::to_string(&manifest)?],
    )?;
    tx.execute(
        "DELETE FROM leases WHERE artifact=?1 AND token=?2",
        params![a.id, reference.attempt],
    )?;
    tx.commit()?;
    Ok(())
}
