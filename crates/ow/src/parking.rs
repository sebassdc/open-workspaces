//! Explicit parking and connection admission; no parking policy or timer.
use super::*;

fn shape(c: &Catalog, physical: &str, kind: Kind, address: &str) -> Result<Intent> {
    let metadata: String = c.db.query_row(
        "SELECT metadata FROM resources WHERE kind='machine' AND physical=?1",
        [physical],
        |r| r.get(0),
    )?;
    let meta: Value = serde_json::from_str(&metadata)?;
    Ok(Intent {
        kind,
        machine: address.into(),
        target: None,
        capture: None,
        child: None,
        memory_mib: meta["memory_mib"]
            .as_u64()
            .context("unknown memory")?
            .try_into()?,
        cpus: meta["vcpu_count"]
            .as_u64()
            .context("unknown CPUs")?
            .try_into()?,
        disk_bytes: meta["disk_bytes"].as_u64().context("unknown disk size")?,
        accept_cold: false,
        explicit_start: false,
    })
}
pub(super) fn admission(c: &Catalog, user: i64, request: &Value, kind: Kind) -> Result<Transfer> {
    let key = request["operation_key"]
        .as_str()
        .context("operation key required")?;
    common::identifier(key)?;
    let address = request["id"].as_str().context("machine required")?;
    let physical = c.machine(user, address, kind.permission())?;
    if let Some(id) = c.db.query_row("SELECT t.id FROM transfers t JOIN operations o ON o.id=t.operation WHERE o.owner=?1 AND o.retry_key=?2",params![user,key],|r|r.get::<_,i64>(0)).optional()? {
        let t = c.transfer(id)?;
        let previous = intent(c,&t)?;
        ensure!(t.kind == kind && previous.machine == address && previous.accept_cold == request["accept_cold"].as_bool().unwrap_or(false) && previous.explicit_start == (request["op"] == "start" || request["op"] == "wake"), "operation key conflicts with original parameters");
        return c.begin_transfer(user,key,&previous);
    }
    let mut input = shape(c, &physical, kind, address)?;
    input.accept_cold = request["accept_cold"].as_bool().unwrap_or(false);
    input.explicit_start = request["op"] == "start" || request["op"] == "wake";
    if kind == Kind::Wake {
        // All callers and users join the single locked wake. Their own access is checked first.
        if let Some(id) = c.db.query_row("SELECT t.id FROM transfer_locks l JOIN transfers t ON t.id=l.transfer WHERE l.machine=?1 AND t.kind='wake'",[&physical],|r|r.get::<_,i64>(0)).optional()? {
            let t = c.transfer(id)?;
            return join_wake(c,user,request,t);
        }
        input.capture = c.db.query_row("SELECT current_capture FROM resources WHERE kind='machine' AND physical=?1 AND node='' AND placement_managed=1",[&physical],|r|r.get(0))?;
    } else {
        ensure!(c.db.query_row("SELECT node<>'' AND json_extract(metadata,'$.state') IN ('stopped','hibernated') FROM resources WHERE kind='machine' AND physical=?1",[&physical],|r|r.get::<_,bool>(0))?,"park requires a stopped or hibernated placement");
    }
    c.begin_transfer(user, key, &input)
}
fn join_wake(c: &Catalog, user: i64, request: &Value, t: Transfer) -> Result<Transfer> {
    let cold = request["accept_cold"].as_bool().unwrap_or(false);
    ensure!(
        !cold || intent(c, &t)?.accept_cold,
        "wake already in progress; inspect its operation before a cold retry"
    );
    if !initiator_authorized(c, &t)? {
        c.resume_wake(
            user,
            request["operation_key"]
                .as_str()
                .context("operation key required")?,
            request["id"].as_str().context("machine required")?,
            t.id,
            matches!(request["op"].as_str(), Some("start" | "wake")),
            cold,
        )
    } else {
        Ok(t)
    }
}
pub(crate) fn park(shared: Shared, identity: &Identity, request: &Value) -> Result<Value> {
    let (user, physical, node, epoch, root) = {
        let mut c = shared.lock().unwrap();
        let user = c.user(identity)?;
        let address = request["id"].as_str().context("machine required")?;
        let physical = c.machine(user, address, "park")?;
        ensure!(
            c.db.query_row(
                "SELECT owner=?2 FROM resources WHERE kind='machine' AND physical=?1",
                params![physical, user],
                |r| r.get::<_, bool>(0)
            )?,
            "park requires the owner"
        );
        let (node, epoch): (String, i64) = c.db.query_row(
            "SELECT node,placement_epoch FROM resources WHERE kind='machine' AND physical=?1",
            [&physical],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        // Retry terminal history even though its placement is now empty.
        if c.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE owner=?1 AND retry_key=?2)",
            params![user, request["operation_key"].as_str()],
            |r| r.get::<_, bool>(0),
        )? {
            let t = admission(&c, user, request, Kind::Park)?;
            let info = c.operation_info(user, t.operation)?;
            drop(c);
            launch(shared, t.id);
            return Ok(info);
        }
        (user, physical, node, epoch, c.root.clone())
    };
    {
        let c = shared.lock().unwrap();
        let caps: String=c.db.query_row("SELECT capabilities FROM nodes WHERE id=?1 AND revoked=0 AND heartbeat>CAST(strftime('%s','now') AS INTEGER)-15",[&node],|r|r.get(0))?;
        common::placement_capable(&serde_json::from_str(&caps)?)?;
        ensure!(
            c.db.query_row(
                "SELECT json_extract(capabilities,'$.park_wake_v1')=1 FROM nodes WHERE id=?1",
                [&node],
                |r| r.get::<_, Option<bool>>(0)
            )?
            .unwrap_or(false),
            "runner agent lacks park publication jobs"
        );
    }
    let policy: PublicSshPolicy = serde_json::from_value(rpc(
        &root,
        &node,
        json!({"op":"restore-policy","id":physical}),
    )?)?;
    let c = shared.lock().unwrap();
    ensure!(c.db.query_row("SELECT node=?2 AND placement_epoch=?3 FROM resources WHERE kind='machine' AND physical=?1",params![physical,node,epoch],|r|r.get::<_,bool>(0))?,"source authority changed");
    let stored: Option<String> = c.db.query_row(
        "SELECT ssh_policy FROM resources WHERE kind='machine' AND physical=?1",
        [&physical],
        |r| r.get(0),
    )?;
    if let Some(stored) = stored {
        ensure!(
            same_policy(&serde_json::from_str::<Value>(&stored)?, &json!(policy)),
            "source public policy differs from catalog"
        );
    } else {
        c.set_transfer_ssh_policy(user, request["id"].as_str().unwrap(), 0, &policy)?;
    }
    let t = admission(&c, user, request, Kind::Park)?;
    let info = c.operation_info(user, t.operation)?;
    drop(c);
    launch(shared, t.id);
    Ok(info)
}
pub(crate) fn wake(shared: Shared, identity: &Identity, request: &Value) -> Result<Value> {
    let mut c = shared.lock().unwrap();
    let user = c.user(identity)?;
    let t = admission(&c, user, request, Kind::Wake)?;
    let actor: i64 = c.db.query_row(
        "SELECT owner FROM operations WHERE id=?1",
        [t.operation],
        |r| r.get(0),
    )?;
    let info = if actor == user {
        c.operation_info(user, t.operation)?
    } else {
        public_status(&c, &t)?
    };
    drop(c);
    launch(shared, t.id);
    Ok(info)
}
/// Wait outside the catalog lock. A timed-out connection never cancels or readmits a wake.
/// The caller must route/check authorization again after this returns.
pub(crate) fn ensure_awake(
    shared: Shared,
    identity: &Identity,
    address: &str,
    permission: &str,
    explicit: bool,
) -> Result<Option<Value>> {
    let t = {
        let mut c = shared.lock().unwrap();
        let user = c.user(identity)?;
        let physical = c.machine(user, address, permission)?;
        let locked:Option<i64>=c.db.query_row("SELECT t.id FROM transfer_locks l JOIN transfers t ON t.id=l.transfer WHERE l.machine=?1 AND t.kind='wake'",[&physical],|r|r.get(0)).optional()?;
        let parked:bool=c.db.query_row("SELECT node='' AND placement_managed=1 FROM resources WHERE kind='machine' AND physical=?1",[&physical],|r|r.get(0))?;
        if !parked && locked.is_none() {
            return Ok(None);
        }
        let epoch: i64 = c.db.query_row(
            "SELECT placement_epoch FROM resources WHERE kind='machine' AND physical=?1",
            [&physical],
            |r| r.get(0),
        )?;
        let key = format!(
            "w-{}",
            &store::sha256(format!("{physical}:{epoch}").as_bytes())[..30]
        );
        let request =
            json!({"op":if explicit {"start"}else{"connect"},"id":address,"operation_key":key});
        match locked {
            Some(id) => join_wake(&c, user, &request, c.transfer(id)?)?,
            None => admission(&c, user, &request, Kind::Wake)?,
        }
    };
    launch(shared.clone(), t.id);
    wait_awake(
        shared,
        identity,
        address,
        permission,
        &t,
        Duration::from_secs(25),
    )
}
pub(super) fn wait_awake(
    shared: Shared,
    identity: &Identity,
    address: &str,
    permission: &str,
    t: &Transfer,
    wait: Duration,
) -> Result<Option<Value>> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        let c = shared.lock().unwrap();
        let user: i64 = c.db.query_row(
            "SELECT id FROM users WHERE issuer=?1 AND subject=?2",
            params![identity.issuer, identity.subject],
            |r| r.get(0),
        )?;
        c.machine(user, address, permission)?;
        let current = c.transfer(t.id)?;
        // A joining user's operation is private to its initiating user; expose only safe progress.
        let info = public_status(&c, &current)?;
        if current.step == Step::Succeeded {
            return Ok(Some(info));
        }
        if current.step.terminal() || std::time::Instant::now() >= deadline {
            return Err(crate::catalog::OperationFailure(info).into());
        }
        drop(c);
        std::thread::sleep(Duration::from_millis(100));
    }
}
fn public_status(c: &Catalog, t: &Transfer) -> Result<Value> {
    let mut value = json!({"id":t.operation,"op":t.kind.operation(),"state":"pending","transfer":{"id":t.id,"epoch":t.epoch,"step":t.step},"error":"Wake is pending; reconnect to join the same operation."});
    let actor: i64 = c.db.query_row(
        "SELECT owner FROM operations WHERE id=?1",
        [t.operation],
        |r| r.get(0),
    )?;
    let info = c.operation_info(actor, t.operation)?;
    for field in ["state", "progress", "queued", "error", "error_code"] {
        if let Some(v) = info.get(field) {
            value[field] = v.clone();
        }
    }
    if t.step == Step::Succeeded {
        value.as_object_mut().unwrap().remove("error");
    }
    Ok(value)
}
pub(super) fn reserve_wake(shared: &Shared, t: &Transfer) -> Result<bool> {
    let c = shared.lock().unwrap();
    authorized(&c, t)?;
    let input = intent(&c, t)?;
    let nodes: Vec<String> =
        c.db.prepare("SELECT id FROM nodes WHERE revoked=0 ORDER BY id")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
    for node in nodes {
        if capable(&c, &node, input.disk_bytes.saturating_mul(2)).is_ok()
            && c.reserve_wake(t.id, &node, &input).is_ok()
        {
            return Ok(true);
        }
    }
    c.db.execute(
        "UPDATE operations SET response=?1 WHERE id=?2",
        params![
            json!({"queued":"No eligible runner with capacity; wake remains queued."}).to_string(),
            t.operation
        ],
    )?;
    Ok(false)
}

pub(super) fn tick(
    shared: &Shared,
    t: &Transfer,
    at: i64,
    call: &impl Fn(&str, Value) -> Result<Value>,
) -> Result<bool> {
    let (input, policy, source_authority) = {
        let c = shared.lock().unwrap();
        if t.step.terminal() {
            return Ok(false);
        }
        authorized(&c, t)?;
        let input = intent(&c, t)?;
        let policy: Value = serde_json::from_str(&c.db.query_row(
            "SELECT ssh_policy FROM resources WHERE kind='machine' AND physical=?1",
            [&t.source],
            |r| r.get::<_, String>(0),
        )?)?;
        let auth = if t.source_epoch == 0 {
            authority(t, &t.source_node)
        } else {
            common::Placement {
                epoch: t.source_epoch as u64,
                node: t.source_node.clone(),
                transfer: format!(
                    "transfer-{}",
                    c.db.query_row(
                        "SELECT id FROM transfers WHERE epoch=?1",
                        [t.source_epoch],
                        |r| r.get::<_, i64>(0)
                    )?
                ),
            }
        };
        (input, policy, auth)
    };
    match t.step {
        Step::Preparing => export_source(shared, t, &input, &policy, &source_authority, call),
        Step::Exported => {
            let seen = call(&t.source_node, json!({"op":"inspect","id":t.source}))?;
            source_fence(&shared.lock().unwrap(), t, &seen, &source_authority)?;
            let snapshot = seen["lifecycle"]["intent"]["capture"]
                .as_str()
                .context("fenced capture missing")?
                .to_owned();
            common::identifier(&snapshot)?;
            let (a, expires) = {
                let c = shared.lock().unwrap();
                let artifact = format!("park-{}", t.id);
                let old:Option<(String,String,i64,String,String)>=c.db.query_row("SELECT a.attempt,a.store_url,l.expires,a.local_name,a.state FROM artifacts a JOIN leases l ON l.artifact=a.id AND l.token=a.attempt WHERE a.id=?1",[&artifact],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
                let (attempt, url, expires) = if let Some((
                    attempt,
                    url,
                    expires,
                    previous,
                    state,
                )) = old
                {
                    if previous == snapshot {
                        (attempt, url, expires)
                    } else {
                        ensure!(
                            state == "uploading",
                            "published capture identity differs from source fence"
                        );
                        let token = store::token()?;
                        let tx = rusqlite::Transaction::new_unchecked(
                            &c.db,
                            rusqlite::TransactionBehavior::Immediate,
                        )?;
                        tx.execute("UPDATE artifacts SET attempt=?1,local_name=?2 WHERE id=?3 AND state='uploading'",params![token,snapshot,artifact])?;
                        tx.execute("INSERT INTO leases(artifact,holder,purpose,token,expires) VALUES(?1,?2,'push',?3,?4)",params![artifact,name(t),token,at+7200])?;
                        tx.commit()?;
                        (token, url, at + 7200)
                    }
                } else {
                    let url = crate::image_store::configured_url(&c.root)?;
                    let attempt = store::token()?;
                    let tx = rusqlite::Transaction::new_unchecked(
                        &c.db,
                        rusqlite::TransactionBehavior::Immediate,
                    )?;
                    tx.execute("INSERT INTO artifacts(id,kind,owner,machine,local_name,state,attempt,store_url) SELECT ?1,'capture',owner,physical,?2,'uploading',?3,?4 FROM resources WHERE kind='machine' AND physical=?5",params![artifact,snapshot,attempt,url,t.source])?;
                    tx.execute("INSERT INTO leases(artifact,holder,purpose,token,expires) VALUES(?1,?2,'push',?3,?4)",params![artifact,name(t),attempt,at+7200])?;
                    tx.execute(
                        "UPDATE transfers SET capture=?1 WHERE id=?2",
                        params![artifact, t.id],
                    )?;
                    tx.commit()?;
                    (attempt, url, at + 7200)
                };
                (
                    Artifact {
                        id: artifact,
                        machine: t.source.clone(),
                        store_url: url,
                        reference: Reference {
                            attempt,
                            digest: String::new(),
                        },
                        snapshot,
                        memory_mib: input.memory_mib,
                        cpus: input.cpus,
                        disk_bytes: input.disk_bytes,
                    },
                    expires,
                )
            };
            // Probe a completed publication before retrying; an uncertain live attempt is never duplicated.
            let mut request = json!({"op":"capture-published","artifact":a});
            let mut result = call(&t.source_node, request.clone())?;
            if result["present"] != true {
                if expires <= at {
                    let c = shared.lock().unwrap();
                    let token = store::token()?;
                    let tx = rusqlite::Transaction::new_unchecked(
                        &c.db,
                        rusqlite::TransactionBehavior::Immediate,
                    )?;
                    ensure!(tx.execute("UPDATE artifacts SET attempt=?1 WHERE id=?2 AND attempt=?3 AND state='uploading'",params![token,a.id,a.reference.attempt])?==1,"publication attempt changed");
                    tx.execute("INSERT INTO leases(artifact,holder,purpose,token,expires) VALUES(?1,?2,'push',?3,?4)",params![a.id,name(t),token,at+7200])?;
                    tx.commit()?;
                    return Ok(true);
                }
                request["op"] = json!("capture-push");
                request["expires"] = json!(expires);
                result = call(&t.source_node, request)?;
            }
            let reference: Reference = serde_json::from_value(result["reference"].clone())?;
            let manifest: store::Manifest = serde_json::from_value(result["manifest"].clone())?;
            let mut pinned = a.clone();
            pinned.reference = reference.clone();
            native(&manifest, &pinned)?;
            ensure!(
                reference.attempt == a.reference.attempt
                    && store::sha256(&manifest.bytes()?) == reference.digest,
                "publication reference mismatch"
            );
            let c = shared.lock().unwrap();
            authorized(&c, t)?;
            let bytes: u64 = manifest.files.values().map(|f| f.size).sum();
            c.db.execute("UPDATE operations SET response=json_set(COALESCE(response,'{}'),'$.progress',json(?1)) WHERE id=?2",params![json!({"bytes_completed":bytes,"bytes_total":bytes,"unit":"logical bytes in completed file objects"}).to_string(),t.operation])?;
            c.db.execute("UPDATE artifacts SET state='stored',digest=?1,logical_bytes=?2,stored_at=CURRENT_TIMESTAMP WHERE id=?3 AND attempt=?4 AND state IN ('uploading','stored')",params![reference.digest,bytes as i64,a.id,reference.attempt])?;
            c.db.execute("INSERT INTO artifact_copies(artifact,node,state,last_used) VALUES(?1,?2,'present',?3) ON CONFLICT(artifact,node) DO UPDATE SET state='present',last_used=excluded.last_used",params![a.id,t.source_node,at])?;
            c.advance_transfer(
                t.id,
                t.epoch,
                t.step,
                Step::Prepared,
                &Evidence {
                    capture: Some(a.id),
                    ..Default::default()
                },
            )?;
            Ok(true)
        }
        Step::Prepared => {
            let seen = call(&t.source_node, json!({"op":"inspect","id":t.source}))?;
            let c = shared.lock().unwrap();
            source_fence(&c, t, &seen, &source_authority)?;
            authorized(&c, t)?;
            c.commit_transfer(t.id, t.epoch)?;
            Ok(true)
        }
        Step::Committed => {
            let c = shared.lock().unwrap();
            c.advance_transfer(t.id, t.epoch, t.step, Step::Succeeded, &Evidence::default())?;
            Ok(false)
        }
        _ => anyhow::bail!("unsupported park phase"),
    }
}

/// Reuse the retained capture on an old source without keeping its old machine directory.
/// This narrow T2 cleanup is allowed only after a durable park and a matching completed export.
pub(super) fn clean_cached_source(
    shared: &Shared,
    t: &Transfer,
    node: &str,
    seen: &Value,
    call: &impl Fn(&str, Value) -> Result<Value>,
) -> Result<()> {
    let request = {
        let c = shared.lock().unwrap();
        authorized(&c, t)?;
        let export = seen["lifecycle"]["intent"]["transfer"]
            .as_str()
            .context("missing retained export")?;
        let id = export
            .strip_prefix("transfer-")
            .context("unknown retained export")?
            .parse::<i64>()?;
        let old = c.transfer(id)?;
        ensure!(
            old.step == Step::Succeeded
                && old.source == t.machine
                && old.source_node == node
                && matches!(old.kind, Kind::Park | Kind::Restore),
            "retained copy is not a completed export"
        );
        let placement: common::Placement = serde_json::from_value(seen["placement"].clone())?;
        source_fence(&c, &old, seen, &placement)?;
        ensure!(c.db.query_row("SELECT node='' OR placement_epoch=?2 FROM resources WHERE kind='machine' AND physical=?1",params![t.machine,t.epoch],|r|r.get::<_,bool>(0))?,"wake no longer owns the parked source");
        json!({"op":"export-cleanup","id":t.machine,"placement":placement,"transfer":export,"capture":seen["lifecycle"]["intent"]["capture"],"operation_key":format!("{}-cleanup-{}",name(t),id)})
    };
    let cleaned = call(node, request)?;
    ensure!(
        cleaned["id"] == t.machine
            && cleaned["placement"] == seen["placement"]
            && cleaned["vmm_running"] == false
            && cleaned["lifecycle"]["phase"] == "cleaned",
        "retained source cleanup unconfirmed"
    );
    Ok(())
}
