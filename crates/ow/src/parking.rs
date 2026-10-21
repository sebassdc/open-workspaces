//! Explicit parking and connection admission; no parking policy or timer.
use super::*;

pub(super) fn shape(c: &Catalog, physical: &str, kind: Kind, address: &str) -> Result<Intent> {
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
        snapshot: None,
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
        explicit_start: kind == Kind::Move && meta["state"] == "running",
    })
}
pub(super) fn admission(c: &Catalog, user: i64, request: &Value, kind: Kind) -> Result<Transfer> {
    let key = request["operation_key"]
        .as_str()
        .context("operation key required")?;
    common::identifier(key)?;
    let address = request["id"].as_str().context("machine required")?;
    let physical = c.machine(user, address, kind.permission())?;
    if let Some((operation,raw,state,id)) = c.db.query_row("SELECT o.id,o.request,o.state,t.id FROM operations o JOIN transfers t ON t.operation=o.id OR (o.state='joined' AND t.id=json_extract(o.response,'$.wake_transfer')) WHERE o.owner=?1 AND o.retry_key=?2",params![user,key],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?))).optional()? {
        let t = c.transfer(id)?;
        let previous:Intent = serde_json::from_value(serde_json::from_str::<Value>(&raw)?["transfer"].take())?;
        ensure!(previous.kind == kind && previous.machine == address && previous.accept_cold == request["accept_cold"].as_bool().unwrap_or(false) && previous.explicit_start == (request["op"] == "start" || request["op"] == "wake"), "operation key conflicts with original parameters");
        if state == "joined" {
            ensure!(t.source == physical, "wake source changed");
            return if !t.step.terminal() && !initiator_authorized(c,&t)? { join_wake(c,user,request,t) } else { Ok(t) };
        }
        ensure!(operation == t.operation,"operation no longer owns this transfer");
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
        if matches!(request["op"].as_str(), Some("start" | "wake")) {
            let mut input = intent(c, &t)?;
            input.machine = request["id"].as_str().context("machine required")?.into();
            input.explicit_start = true;
            input.accept_cold = cold;
            // A joined caller key remains bound to this execution after future park cycles.
            c.db.execute("INSERT INTO operations(owner,op,resource,state,node,retry_key,request,response) VALUES(?1,'wake',?2,'joined',?3,?4,?5,?6)",params![user,input.machine,t.target.as_deref().unwrap_or(""),request["operation_key"].as_str(),json!({"transfer":input}).to_string(),json!({"wake_transfer":t.id}).to_string()])?;
        }
        Ok(t)
    }
}
pub(crate) fn park(shared: Shared, identity: &Identity, request: &Value) -> Result<Value> {
    let (user, physical, node, epoch, generation, root) = {
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
        let (node, epoch, generation): (String, i64, i64) = c.db.query_row(
            "SELECT node,placement_epoch,ssh_policy_generation FROM resources WHERE kind='machine' AND physical=?1",
            [&physical],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
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
        (user, physical, node, epoch, generation, c.root.clone())
    };
    {
        let c = shared.lock().unwrap();
        let caps: String=c.db.query_row("SELECT capabilities FROM live_nodes WHERE id=?1 AND revoked=0 AND heartbeat>CAST(strftime('%s','now') AS INTEGER)-15",[&node],|r|r.get(0))?;
        common::placement_capable(&serde_json::from_str(&caps)?)?;
        ensure!(
            c.db.query_row(
                "SELECT json_extract(capabilities,'$.park_wake_v1')=1 FROM live_nodes WHERE id=?1",
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
    ensure!(c.db.query_row("SELECT node=?2 AND placement_epoch=?3 AND ssh_policy_generation=?4 FROM resources WHERE kind='machine' AND physical=?1",params![physical,node,epoch,generation],|r|r.get::<_,bool>(0))?,"source authority or SSH policy changed");
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
        c.set_transfer_ssh_policy(user, request["id"].as_str().unwrap(), generation, &policy)?;
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
    ensure_awake_with_key(shared, identity, address, permission, explicit, None)
}
pub(crate) fn ensure_started(
    shared: Shared,
    identity: &Identity,
    request: &Value,
) -> Result<Option<Value>> {
    let key = match request["operation_key"].as_str() {
        Some(key) => key.to_owned(),
        None => common::nonce()?,
    };
    {
        let mut c = shared.lock().unwrap();
        let user = c.user(identity)?;
        if let Some((id,raw,state,response))=c.db.query_row("SELECT id,request,state,response FROM operations WHERE owner=?1 AND retry_key=?2 AND op='start'",params![user,key],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?))).optional()? {
            let physical=c.machine(user,request["id"].as_str().context("machine required")?,"start")?;
            let original:Value=serde_json::from_str(&raw)?;
            ensure!(original["request"]["id"]==physical && request["node"].as_str().is_none_or(|node|original["node"]==node),"operation key conflicts with original parameters");
            if state=="succeeded" {return Ok(Some(serde_json::from_str(response.as_deref().context("missing start result")?)?));}
            if c.db.query_row("SELECT node='' OR EXISTS(SELECT 1 FROM transfer_locks WHERE machine=?1) FROM resources WHERE kind='machine' AND physical=?1",[physical],|r|r.get::<_,bool>(0))? {
                return Err(crate::catalog::OperationFailure(c.operation_info(user,id)?).into());
            }
            return Ok(None);
        }
    }
    ensure_awake_with_key(
        shared,
        identity,
        request["id"].as_str().context("machine required")?,
        "start",
        true,
        Some(&key),
    )
}
fn ensure_awake_with_key(
    shared: Shared,
    identity: &Identity,
    address: &str,
    permission: &str,
    explicit: bool,
    caller_key: Option<&str>,
) -> Result<Option<Value>> {
    let wait = std::time::Instant::now();
    loop {
        let pending = {
            let mut c = shared.lock().unwrap();
            let user = c.user(identity)?;
            let physical = c.machine(user, address, permission)?;
            let (node,state):(String,String)=c.db.query_row("SELECT node,COALESCE(json_extract(metadata,'$.state'),'unknown') FROM resources WHERE kind='machine' AND physical=?1",[&physical],|r|Ok((r.get(0)?,r.get(1)?)))?;
            if node.is_empty() || !crate::drain::blocked(&c.db, &node)? || state == "running" {
                false
            } else {
                ensure!(
                    explicit || state != "stopped",
                    "stopped box requires explicit start; drain preserves stopped state"
                );
                true
            }
        };
        if !pending {
            break;
        }
        ensure!(
            wait.elapsed() < Duration::from_secs(25),
            "runner draining; joined parking wait, reconnect to wake elsewhere"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let t = {
        let mut c = shared.lock().unwrap();
        let user = c.user(identity)?;
        let physical = c.machine(user, address, permission)?;
        let locked:Option<i64>=c.db.query_row("SELECT t.id FROM transfer_locks l JOIN transfers t ON t.id=l.transfer WHERE l.machine=?1 AND t.kind='wake'",[&physical],|r|r.get(0)).optional()?;
        let parked:bool=c.db.query_row("SELECT node='' AND placement_managed=1 FROM resources WHERE kind='machine' AND physical=?1",[&physical],|r|r.get(0))?;
        let replay = explicit
            && caller_key.is_some()
            && c.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM operations WHERE owner=?1 AND retry_key=?2)",
                params![user, caller_key],
                |r| r.get::<_, bool>(0),
            )?;
        if !parked && locked.is_none() && !replay {
            return Ok(None);
        }
        let epoch: i64 = c.db.query_row(
            "SELECT placement_epoch FROM resources WHERE kind='machine' AND physical=?1",
            [&physical],
            |r| r.get(0),
        )?;
        let key = caller_key.map(str::to_owned).unwrap_or_else(|| {
            format!(
                "w-{}",
                &store::sha256(format!("{physical}:{epoch}").as_bytes())[..30]
            )
        });
        let request =
            json!({"op":if explicit {"start"}else{"connect"},"id":address,"operation_key":key});
        admission(&c, user, &request, Kind::Wake)?
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
pub(crate) fn wake_status(c: &Catalog, id: i64) -> Result<Value> {
    public_status(c, &c.transfer(id)?)
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
    let (input, a, saved) = {
        let c = shared.lock().unwrap();
        authorized(&c, t)?;
        let a = artifact(&c, t)?;
        let saved: Option<String> = c.db.query_row("SELECT json_extract(o.response,'$.capture_manifest') FROM transfers p JOIN operations o ON o.id=p.operation WHERE p.capture=?1 AND p.source=?2 AND p.kind IN ('park','wake') AND json_type(o.response,'$.capture_manifest')='object' ORDER BY p.id DESC LIMIT 1",params![a.id,t.source],|r|r.get(0)).optional()?;
        (intent(&c, t)?, a, saved)
    };
    let mut fetched = None;
    let compatibility = if input.accept_cold {
        None
    } else {
        let manifest: store::Manifest = if let Some(saved) = saved {
            serde_json::from_str(&saved)?
        } else {
            // Older park records need only their pinned manifest; never hold the catalog over I/O.
            let objects = store::S3Objects::new(&a.store_url, None)?;
            let manifest = store::read_manifest(&objects, &a.reference)?;
            fetched = Some(manifest.clone());
            manifest
        };
        ensure!(
            store::sha256(&manifest.bytes()?) == a.reference.digest,
            "saved capture manifest digest differs"
        );
        let (_, native) = native(&manifest, &a)?;
        if manifest.files.len() == 3 {
            let compatibility = json!({"runtime":native["runtime"],"cpu":native["cpu"],"kernel_sha256":native["kernel_sha256"]});
            ensure!(
                compatibility
                    .as_object()
                    .unwrap()
                    .values()
                    .all(|v| v.as_str().is_some_and(|s| !s.is_empty())),
                "saved RAM compatibility is incomplete"
            );
            Some(compatibility)
        } else {
            None
        }
    };
    let c = shared.lock().unwrap();
    authorized(&c, t)?;
    let current_transfer = c.transfer(t.id)?;
    if current_transfer.step != Step::Preparing {
        return Ok(!current_transfer.step.terminal());
    }
    let current = artifact(&c, t)?;
    ensure!(
        current.reference.digest == a.reference.digest
            && current.reference.attempt == a.reference.attempt
            && current.store_url == a.store_url,
        "stored capture changed during wake selection"
    );
    if let Some(manifest) = fetched {
        c.db.execute("UPDATE operations SET response=json_set(COALESCE(response,'{}'),'$.capture_manifest',json(?1)) WHERE id=?2",params![serde_json::to_string(&manifest)?,t.operation])?;
    }
    let nodes: Vec<(String, String)> =
        c.db.prepare("SELECT id,capabilities FROM live_nodes WHERE revoked=0 ORDER BY id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
    let mut compatible_runner = false;
    for (node, caps) in nodes {
        let compatible = compatibility.as_ref().is_none_or(|expected| {
            serde_json::from_str::<Value>(&caps).is_ok_and(|caps| &caps["compat"] == expected)
        });
        compatible_runner |= compatible;
        if compatible
            && capable(&c, &node, input.disk_bytes.saturating_mul(2)).is_ok()
            && c.reserve_wake(t.id, &node, &input).is_ok()
        {
            return Ok(true);
        }
    }
    if !compatible_runner && compatibility.is_some() {
        c.advance_transfer(
            t.id,
            t.epoch,
            Step::Preparing,
            Step::Failed,
            &Evidence::default(),
        )?;
        c.db.execute("UPDATE operations SET response=json_set(COALESCE(response,'{}'),'$.error',?1) WHERE id=?2",params!["incompatible stored RAM; retry with a new operation key and explicit --accept-cold consent",t.operation])?;
        return Ok(false);
    }
    c.db.execute(
        "UPDATE operations SET response=json_set(COALESCE(response,'{}'),'$.queued',?1) WHERE id=?2",
        params![
            "No eligible runner with capacity for this capture; wake remains queued.",
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
            if !publish_capture(shared, t, &input, &source_authority, at, call)? {
                return Ok(true);
            }
            let c = shared.lock().unwrap();
            let current = c.transfer(t.id)?;
            c.advance_transfer(
                t.id,
                t.epoch,
                t.step,
                Step::Prepared,
                &Evidence {
                    capture: current.capture,
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
                && matches!(old.kind, Kind::Park | Kind::Restore | Kind::Move),
            "retained copy is not a completed export"
        );
        let placement: common::Placement = serde_json::from_value(seen["placement"].clone())?;
        source_fence(&c, &old, seen, &placement)?;
        ensure!(c.db.query_row("SELECT node='' OR placement_epoch=?2 FROM resources WHERE kind='machine' AND physical=?1",params![t.machine,t.epoch],|r|r.get::<_,bool>(0))? || (t.kind == Kind::Move && c.db.query_row("SELECT node=?2 AND placement_epoch=?3 FROM resources WHERE kind='machine' AND physical=?1",params![t.source,t.source_node,t.source_epoch],|r|r.get::<_,bool>(0))?),"transfer no longer owns the source");
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

// Shared immutable publication path for park, move and fork.
pub(super) fn publish_capture(
    shared: &Shared,
    t: &Transfer,
    input: &Intent,
    source_authority: &common::Placement,
    at: i64,
    call: &impl Fn(&str, Value) -> Result<Value>,
) -> Result<bool> {
    let snapshot = if t.kind == Kind::Fork {
        shared.lock().unwrap().db.query_row(
            "SELECT json_extract(response,'$.fork_snapshot') FROM operations WHERE id=?1",
            [t.operation],
            |r| r.get::<_, String>(0),
        )?
    } else {
        let seen = call(&t.source_node, json!({"op":"inspect","id":t.source}))?;
        source_fence(&shared.lock().unwrap(), t, &seen, source_authority)?;
        seen["lifecycle"]["intent"]["capture"]
            .as_str()
            .context("fenced capture missing")?
            .to_owned()
    };
    common::identifier(&snapshot)?;
    let (a, expires) = {
        let c = shared.lock().unwrap();
        let artifact = format!(
            "{}-{}",
            if t.kind == Kind::Park {
                "park"
            } else {
                "capture"
            },
            t.id
        );
        let old:Option<(String,String,i64,String,String)>=c.db.query_row("SELECT a.attempt,a.store_url,l.expires,a.local_name,a.state FROM artifacts a JOIN leases l ON l.artifact=a.id AND l.token=a.attempt WHERE a.id=?1",[&artifact],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        let (attempt, url, expires) = if let Some((attempt, url, expires, previous, state)) = old {
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
    let mut request = json!({"op":"capture-published","artifact":a,"fork":t.kind == Kind::Fork});
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
            return Ok(false);
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
    c.db.execute("UPDATE operations SET response=json_set(COALESCE(response,'{}'),'$.progress',json(?1),'$.capture_manifest',json(?2)) WHERE id=?3",params![json!({"bytes_completed":bytes,"bytes_total":bytes,"unit":"logical bytes in completed file objects"}).to_string(),serde_json::to_string(&manifest)?,t.operation])?;
    c.db.execute("UPDATE artifacts SET state='stored',digest=?1,logical_bytes=?2,stored_at=CURRENT_TIMESTAMP WHERE id=?3 AND attempt=?4 AND state IN ('uploading','stored')",params![reference.digest,bytes as i64,a.id,reference.attempt])?;
    c.db.execute("INSERT INTO artifact_copies(artifact,node,state,last_used) VALUES(?1,?2,'present',?3) ON CONFLICT(artifact,node) DO UPDATE SET state='present',last_used=excluded.last_used",params![a.id,t.source_node,at])?;
    Ok(true)
}
