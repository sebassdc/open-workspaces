//! Explicit move/fork composition over the existing durable transfer runner.
use super::*;

fn capable_move(c: &Catalog, node: &str, disk: u64) -> Result<()> {
    capable(c, node, disk)?;
    let caps: String =
        c.db.query_row("SELECT capabilities FROM nodes WHERE id=?1", [node], |r| {
            r.get(0)
        })?;
    ensure!(
        serde_json::from_str::<Value>(&caps)?["move_fork_v1"] == true,
        refuse(
            node,
            "runs a worker without move/fork support or artifact store; upgrade its worker"
        )
    );
    Ok(())
}

/// Admits a move or cross-runner fork. A refused admission is journaled as a failed
/// operation carrying its public reason, so the requester can see why in ow status.
pub(crate) fn relocate(shared: Shared, identity: &Identity, request: &Value) -> Result<Value> {
    admit(shared.clone(), identity, request).or_else(|error| {
        let (reason, code) = if let Some(r) = error.downcast_ref::<crate::limits::Rejection>() {
            (r.0.clone(), "refused")
        } else if let Some(r) = error.downcast_ref::<crate::catalog::Refusal>() {
            (r.message.clone(), r.code)
        } else {
            return Err(error);
        };
        let mut c = shared.lock().unwrap();
        let user = c.user(identity)?;
        let resource = request["child"].as_str().or(request["id"].as_str());
        c.db.execute("INSERT INTO operations(owner,op,resource,state,node,retry_key,request,response,finished_at) VALUES(?1,?2,?3,'failed',?4,?5,?6,?7,CURRENT_TIMESTAMP)",params![user,request["op"].as_str(),resource,request["node"].as_str(),request["operation_key"].as_str(),json!({"request":request}).to_string(),json!({"error":reason,"refused":true,"code":code}).to_string()])?;
        let id = c.db.last_insert_rowid();
        Err(crate::catalog::OperationFailure(c.operation_info(user, id)?).into())
    })
}

fn admit(shared: Shared, identity: &Identity, request: &Value) -> Result<Value> {
    let kind = if request["op"] == "move" {
        Kind::Move
    } else {
        Kind::Fork
    };
    let key = request["operation_key"]
        .as_str()
        .context("operation key required")?;
    let address = request["id"].as_str().context("machine required")?;
    let target = request["node"].as_str().context("target node required")?;
    common::identifier(key)?;
    common::identifier(target)?;
    let (user, physical, node, epoch, generation, root, input) = {
        let mut c = shared.lock().unwrap();
        let user = c.user(identity)?;
        let physical = c.machine(user, address, kind.permission())?;
        // A fork for someone else: the child, its name and its quota are theirs.
        let owner = match request["owner"].as_str() {
            Some(email) if kind == Kind::Fork => {
                c.fork_for_others(user, address)?;
                Some(c.user_by_email(user, email)?).filter(|owner| *owner != user)
            }
            Some(_) => anyhow::bail!("only fork takes an owner"),
            None => None,
        };
        // A refused admission keeps its outcome under its key.
        if let Some((id, saved)) = c.db.query_row("SELECT id,request FROM operations WHERE owner=?1 AND retry_key=?2 AND op=?3 AND state='failed' AND json_extract(response,'$.refused')=1",params![user,key,kind.operation()],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?))).optional()? {
            let saved: Value = serde_json::from_str::<Value>(&saved)?["request"].take();
            ensure!(["id","node","child","snapshot","accept_cold","owner"].iter().all(|f| saved[f] == request[f]), "operation key conflicts with original parameters");
            return Err(crate::catalog::OperationFailure(c.operation_info(user,id)?).into());
        }
        if kind == Kind::Move {
            ensure!(
                c.db.query_row(
                    "SELECT owner=?2 FROM resources WHERE kind='machine' AND physical=?1",
                    params![physical, user],
                    |r| r.get::<_, bool>(0)
                )?,
                crate::limits::Rejection("move requires the owner".into())
            );
        }
        if let Some(id) = c.db.query_row("SELECT t.id FROM operations o JOIN transfers t ON t.operation=o.id WHERE o.owner=?1 AND o.retry_key=?2",params![user,key],|r|r.get::<_,i64>(0)).optional()? {
            let t = c.transfer(id)?;
            let previous = intent(&c,&t)?;
            let saved_snapshot:Option<String> = c.db.query_row("SELECT json_extract(response,'$.requested_snapshot') FROM operations WHERE id=?1",[t.operation],|r|r.get(0))?;
            ensure!(previous.kind == kind && previous.machine == address && previous.target.as_deref() == Some(target) && previous.accept_cold == request["accept_cold"].as_bool().unwrap_or(false) && previous.child.as_ref().map(|ch|ch.name.as_str()) == request["child"].as_str() && previous.child.as_ref().and_then(|ch|ch.owner) == owner && saved_snapshot.as_deref() == request["snapshot"].as_str(), "operation key conflicts with original parameters");
            let info = c.operation_info(user,t.operation)?;
            drop(c);
            launch(shared,t.id);
            return Ok(info);
        }
        let (node,epoch,generation):(String,i64,i64) = c.db.query_row("SELECT node,placement_epoch,ssh_policy_generation FROM resources WHERE kind='machine' AND physical=?1",[&physical],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        ensure!(
            !node.is_empty(),
            crate::limits::Rejection("Box is parked; wake it before moving or forking it.".into())
        );
        let mut input = parking::shape(&c, &physical, kind, address)?;
        input.target = Some(target.into());
        input.accept_cold = request["accept_cold"].as_bool().unwrap_or(false);
        if kind == Kind::Fork {
            let child = request["child"].as_str().context("child name required")?;
            common::identifier(child)?;
            ensure!(
                !c.db.query_row("SELECT EXISTS(SELECT 1 FROM resources WHERE owner=?1 AND kind='machine' AND name=?2)",params![owner.unwrap_or(user),child],|r|r.get::<_,bool>(0))?,
                crate::limits::Rejection(format!("A box named {child} already exists; choose another child name."))
            );
            input.child = Some(Child {
                id: format!("m-{}", &common::nonce()?[..24]),
                name: child.into(),
                owner,
            });
            if let Some(snapshot) = request["snapshot"].as_str() {
                named_snapshot(&c, user, address, &physical, &node, snapshot, &mut input)?;
            }
        }
        capable_move(&c, &node, 0)?;
        capable_move(&c, target, input.disk_bytes.saturating_mul(2))?;
        (
            user,
            physical,
            node,
            epoch,
            generation,
            c.root.clone(),
            input,
        )
    };
    // Read the public policy without waking or mutating a guest; generation is checked under admission.
    let policy = if kind == Kind::Move {
        Some(rpc(
            &root,
            &node,
            json!({"op":"restore-policy","id":physical}),
        )?)
    } else {
        None
    };
    let c = shared.lock().unwrap();
    ensure!(
        c.machine(user, address, kind.permission())? == physical,
        "source changed"
    );
    ensure!(c.db.query_row("SELECT node=?2 AND placement_epoch=?3 AND ssh_policy_generation=?4 FROM resources WHERE kind='machine' AND physical=?1",params![physical,node,epoch,generation],|r|r.get::<_,bool>(0))?,"source or SSH policy changed during admission");
    if let Some(policy) = policy {
        let policy: PublicSshPolicy = serde_json::from_value(policy)?;
        let stored: Option<String> = c.db.query_row(
            "SELECT ssh_policy FROM resources WHERE kind='machine' AND physical=?1",
            [&physical],
            |r| r.get(0),
        )?;
        if let Some(stored) = stored {
            ensure!(
                same_policy(
                    &serde_json::from_str(&stored)?,
                    &serde_json::to_value(&policy)?
                ),
                "source public policy differs from catalog"
            );
        } else {
            c.set_transfer_ssh_policy(user, address, generation, &policy)?;
        }
    }
    // Snapshot binding, source/child locks and reservations commit together.
    let t = c.begin_transfer(user, key, &input)?;
    let info = c.operation_info(user, t.operation)?;
    drop(c);
    launch(shared, t.id);
    Ok(info)
}

/// Binds a fork to a named snapshot of its source and takes its shape. A snapshot in the
/// artifact store forks from any runner, with no new capture; one that is not must still be
/// on the box's current runner.
pub(super) fn named_snapshot(
    c: &Catalog,
    user: i64,
    address: &str,
    physical: &str,
    node: &str,
    snapshot: &str,
    input: &mut Intent,
) -> Result<()> {
    common::identifier(snapshot)?;
    let (metadata, here, stored): (String, bool, Option<String>) = c.db.query_row("SELECT s.metadata,s.node=?3,(SELECT a.id FROM artifacts a JOIN resources m ON m.kind='machine' AND m.physical=?4 WHERE a.kind='snapshot' AND a.owner=s.owner AND a.owner=m.owner AND a.machine=m.physical AND a.local_name=s.physical AND a.state='stored' AND a.digest IS NOT NULL) FROM resources s WHERE s.owner=?1 AND s.kind='snapshot' AND s.name=?2 AND json_extract(s.metadata,'$.workspace')=?4",params![c.snapshot_owner(user, address)?,snapshot,node,physical],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?.ok_or_else(|| crate::limits::Rejection(format!("Snapshot {snapshot} is not a snapshot of this box.")))?;
    ensure!(
        here || stored.is_some(),
        crate::limits::Rejection(format!(
            "Snapshot {snapshot} is on another runner and not in the artifact store yet; retry shortly."
        ))
    );
    input.capture = stored;
    let meta: Value = serde_json::from_str(&metadata)?;
    input.memory_mib = meta["memory_mib"]
        .as_u64()
        .context("unknown snapshot memory")?
        .try_into()?;
    input.cpus = meta["vcpu_count"]
        .as_u64()
        .context("unknown snapshot CPUs")?
        .try_into()?;
    input.disk_bytes = meta["disk_bytes"]
        .as_u64()
        .context("unknown snapshot disk size")?;
    input.snapshot = Some(snapshot.into());
    Ok(())
}

pub(super) fn source_authority(c: &Catalog, t: &Transfer) -> Result<common::Placement> {
    if let Some(raw) = c.db.query_row(
        "SELECT json_extract(response,'$.source_authority') FROM operations WHERE id=?1",
        [t.operation],
        |r| r.get::<_, Option<String>>(0),
    )? {
        return Ok(serde_json::from_str(&raw)?);
    }
    let auth = if t.source_epoch == 0 {
        authority(t, &t.source_node)
    } else {
        let previous: i64 = c.db.query_row(
            "SELECT id FROM transfers WHERE epoch=?1",
            [t.source_epoch],
            |r| r.get(0),
        )?;
        common::Placement {
            epoch: t.source_epoch as u64,
            node: t.source_node.clone(),
            transfer: format!("transfer-{previous}"),
        }
    };
    c.db.execute("UPDATE operations SET response=json_set(COALESCE(response,'{}'),'$.source_authority',json(?1)) WHERE id=?2",params![json!(auth).to_string(),t.operation])?;
    Ok(auth)
}

pub(super) fn prepare(
    shared: &Shared,
    t: &Transfer,
    at: i64,
    call: &impl Fn(&str, Value) -> Result<Value>,
) -> Result<Option<bool>> {
    if t.step.terminal() {
        if t.kind == Kind::Move && t.step == Step::Succeeded && cleanup(shared, t, call)? {
            shared.lock().unwrap().db.execute("UPDATE operations SET response=json_remove(response,'$.cleanup_pending') WHERE id=?1",[t.operation])?;
        }
        return Ok(Some(false));
    }
    let (input, auth, policy, stored) = {
        let c = shared.lock().unwrap();
        let owner_abort:bool=c.db.query_row("SELECT COALESCE(json_extract(response,'$.intervention.owner_abort')=1,0) FROM operations WHERE id=?1",[t.operation],|r|r.get(0))?;
        if !(t.kind == Kind::Fork && owner_abort && t.step.before_commit()) {
            authorized(&c, t)?;
        }
        let input = intent(&c, t)?;
        let auth = source_authority(&c, t)?;
        let policy: Value = if t.kind == Kind::Fork {
            json!("disabled")
        } else {
            serde_json::from_str(&c.db.query_row(
                "SELECT ssh_policy FROM resources WHERE kind='machine' AND physical=?1",
                [&t.source],
                |r| r.get::<_, String>(0),
            )?)?
        };
        let stored = t.capture.as_ref().is_some_and(|_| artifact(&c, t).is_ok());
        (input, auth, policy, stored)
    };
    if let Some(result) = intervention(shared, t, &input, &auth, call)? {
        return Ok(Some(result));
    }
    if t.step == Step::Preparing {
        if t.kind == Kind::Move {
            return export_source(shared, t, &input, &policy, &auth, call).map(Some);
        }
        let c = shared.lock().unwrap();
        if stored {
            // A named snapshot already in the store is the fork's capture: nothing to take.
            authorized(&c, t)?;
            c.advance_transfer(t.id, t.epoch, t.step, Step::Exported, &Evidence::default())?;
            return Ok(Some(true));
        }
        let snapshot: Option<String> = c.db.query_row(
            "SELECT json_extract(response,'$.fork_snapshot') FROM operations WHERE id=?1",
            [t.operation],
            |r| r.get(0),
        )?;
        ensure!(
            input.snapshot.is_none() || snapshot.is_some(),
            "named fork snapshot binding missing"
        );
        // Recover generated captures admitted by older controllers before their response write.
        let snapshot = snapshot.unwrap_or_else(|| format!("{}-fork", name(t)));
        c.db.execute("UPDATE operations SET response=json_set(COALESCE(response,'{}'),'$.fork_snapshot',?1) WHERE id=?2",params![snapshot,t.operation])?;
        let managed = c.managed_placement(&t.source)?;
        // Source is locked by this fork, so managed_placement intentionally cannot route it.
        let source_managed: bool = c.db.query_row(
            "SELECT placement_managed FROM resources WHERE kind='machine' AND physical=?1",
            [&t.source],
            |r| r.get(0),
        )?;
        drop(c);
        let mut request = json!({"op":"snapshot","id":t.source,"name":snapshot,"operation_key":format!("{}-snapshot",name(t))});
        if source_managed {
            request["placement"] = json!(managed.unwrap_or(auth));
        }
        call(&t.source_node, request)?;
        let c = shared.lock().unwrap();
        authorized(&c, t)?;
        c.advance_transfer(
            t.id,
            t.epoch,
            t.step,
            Step::Exported,
            &Evidence {
                source_fenced: true,
                ..Default::default()
            },
        )?;
        return Ok(Some(true));
    }
    if t.step == Step::Exported && !stored {
        parking::publish_capture(shared, t, &input, &auth, at, call)?;
        return Ok(Some(true));
    }
    Ok(None)
}

pub(super) fn cleanup(
    shared: &Shared,
    t: &Transfer,
    call: &impl Fn(&str, Value) -> Result<Value>,
) -> Result<bool> {
    if t.target.as_deref() == Some(&t.source_node) {
        return Ok(true);
    }
    let Ok(seen) = call(&t.source_node, json!({"op":"inspect","id":t.source})) else {
        return Ok(false);
    };
    let c = shared.lock().unwrap();
    // A later placement owns the cleanup; a deleted box still has its old source copy removed.
    if t.step == Step::Succeeded && !c.db.query_row("SELECT placement_epoch=?2 AND node=?3 FROM resources WHERE kind='machine' AND physical=?1",params![t.machine,t.epoch,t.target],|r|r.get::<_,bool>(0)).optional()?.unwrap_or(true) { return Ok(true); }
    let auth = source_authority(&c, t)?;
    if let Err(error) = source_fence(&c, t, &seen, &auth) {
        // A contradictory returning source blocks access again; it never becomes an alternate winner.
        if t.step == Step::Succeeded {
            c.db.execute(
                "UPDATE transfers SET step='started' WHERE id=?1 AND step='succeeded'",
                [t.id],
            )?;
            c.db.execute(
                "INSERT OR IGNORE INTO transfer_locks(machine,transfer) VALUES(?1,?2)",
                params![t.machine, t.id],
            )?;
            c.db.execute(
                "UPDATE operations SET state='uncertain' WHERE id=?1",
                [t.operation],
            )?;
        }
        return Err(error);
    }
    drop(c);
    let result = call(
        &t.source_node,
        json!({"op":"export-cleanup","id":t.source,"placement":auth,"transfer":seen["lifecycle"]["intent"]["transfer"],"capture":seen["lifecycle"]["intent"]["capture"],"operation_key":step_key(t,"cleanup")}),
    )?;
    ensure!(
        result["lifecycle"]["phase"] == "cleaned"
            && result["placement"] == json!(auth)
            && result["vmm_running"] == false,
        "source cleanup unconfirmed"
    );
    Ok(true)
}

/// An explicit recovery decision is queued in the owning intent, never executed beside its runner.
pub(crate) fn control(shared: Shared, identity: &Identity, request: &Value) -> Result<Value> {
    let id = request["transfer"].as_i64().context("transfer required")?;
    let expected = request["epoch"]
        .as_i64()
        .context("current transfer epoch required")?;
    let node = request["node"].as_str();
    ensure!(
        request["abort"] == true || node.is_some(),
        "choose abort or a target"
    );
    if let Some(node) = node {
        common::identifier(node)?;
    }
    let mut c = shared.lock().unwrap();
    let user = c.user(identity)?;
    let t = c.transfer(id)?;
    ensure!(
        matches!(t.kind, Kind::Move | Kind::Fork),
        "only move/fork recovery is supported here"
    );
    ensure!(
        c.machine(
            user,
            request["id"].as_str().context("machine required")?,
            t.kind.permission()
        )? == t.source,
        "source changed"
    );
    let owns = if t.kind == Kind::Fork {
        c.db.query_row(
            "SELECT owner=?2 FROM operations WHERE id=?1",
            params![t.operation, user],
            |r| r.get::<_, bool>(0),
        )?
    } else {
        c.db.query_row(
            "SELECT owner=?2 FROM resources WHERE kind='machine' AND physical=?1",
            params![t.source, user],
            |r| r.get::<_, bool>(0),
        )?
    };
    let source_owner: bool = c.db.query_row(
        "SELECT owner=?2 FROM resources WHERE kind='machine' AND physical=?1",
        params![t.source, user],
        |r| r.get(0),
    )?;
    ensure!(
        owns || source_owner && request["abort"] == true,
        "recovery requires the move owner or fork requester; the source owner may abort a revoked fork"
    );
    let action = json!({"epoch":expected,"node":node,"abort":request["abort"]==true,"owner_abort":source_owner && request["abort"]==true});
    let previous: Option<String> = c.db.query_row(
        "SELECT json_extract(response,'$.last_intervention') FROM operations WHERE id=?1",
        [t.operation],
        |r| r.get(0),
    )?;
    if !previous.is_some_and(|raw| serde_json::from_str::<Value>(&raw).is_ok_and(|v| v == action)) {
        if request["abort"] == true
            && t.kind == Kind::Move
            && t.target.as_deref() == Some(&t.source_node)
        {
            let info = c.operation_info(user, t.operation)?;
            drop(c);
            launch(shared, id);
            return Ok(info);
        }
        ensure!(
            request["abort"] == true || node != t.target.as_deref(),
            "retarget requires another runner"
        );
        ensure!(
            t.epoch == expected && t.step.before_commit(),
            "stale epoch or committed target; only forward recovery is allowed"
        );
        let pending: Option<String> = c.db.query_row(
            "SELECT json_extract(response,'$.intervention') FROM operations WHERE id=?1",
            [t.operation],
            |r| r.get(0),
        )?;
        ensure!(
            pending.is_none_or(|p| serde_json::from_str::<Value>(&p).is_ok_and(|v| v == action)),
            "another recovery decision is pending"
        );
        c.db.execute("UPDATE operations SET response=json_set(COALESCE(response,'{}'),'$.intervention',json(?1)) WHERE id=?2",params![action.to_string(),t.operation])?;
    }
    let actor: i64 = c.db.query_row(
        "SELECT owner FROM operations WHERE id=?1",
        [t.operation],
        |r| r.get(0),
    )?;
    let info = if actor == user {
        c.operation_info(user, t.operation)?
    } else {
        json!({"transfer":{"id":t.id,"epoch":t.epoch,"step":t.step},"state":"pending"})
    };
    drop(c);
    launch(shared, id);
    Ok(info)
}

fn intervention(
    shared: &Shared,
    t: &Transfer,
    input: &Intent,
    auth: &common::Placement,
    call: &impl Fn(&str, Value) -> Result<Value>,
) -> Result<Option<bool>> {
    let action: Option<String> = shared.lock().unwrap().db.query_row(
        "SELECT json_extract(response,'$.intervention') FROM operations WHERE id=?1",
        [t.operation],
        |r| r.get(0),
    )?;
    let Some(action) = action else {
        return Ok(None);
    };
    let action: Value = serde_json::from_str(&action)?;
    ensure!(
        action["epoch"] == t.epoch && t.step.before_commit(),
        "recovery cannot roll back committed execution"
    );
    // A move first finishes its positive source seal/publication. Offline is never exclusion proof.
    if t.kind == Kind::Move
        && (t.step == Step::Preparing || artifact(&shared.lock().unwrap(), t).is_err())
    {
        return Ok(None);
    }
    if action["abort"] != true {
        let next = action["node"].as_str().context("new target required")?;
        ensure!(
            t.target.as_deref() != Some(next),
            "retarget requires another runner"
        );
        capable_move(
            &shared.lock().unwrap(),
            next,
            input.disk_bytes.saturating_mul(2),
        )?;
    }
    let old = t.target.as_deref().context("missing old target")?;
    let excluded = call(
        old,
        json!({"op":"import-discard","id":t.machine,"placement":authority(t,old),"operation_key":step_key(t,"exclude")}),
    )?;
    ensure!(
        excluded["id"] == t.machine
            && excluded["placement"] == json!(authority(t, old))
            && excluded["lifecycle"]["phase"] == "discarded"
            && excluded["vmm_running"] == false,
        "target exclusion unconfirmed"
    );
    let abort = action["abort"] == true;
    if abort && t.kind == Kind::Fork {
        let c = shared.lock().unwrap();
        if action["owner_abort"] != true {
            authorized(&c, t)?;
        }
        ensure!(c.db.query_row("SELECT node=?2 AND placement_epoch=?3 FROM resources WHERE kind='machine' AND physical=?1",params![t.source,t.source_node,t.source_epoch],|r|r.get::<_,bool>(0))?,"source authority changed");
        c.advance_transfer(
            t.id,
            t.epoch,
            t.step,
            Step::Aborted,
            &Evidence {
                target_excluded: true,
                ..Default::default()
            },
        )?;
        c.db.execute("UPDATE operations SET response=json_remove(json_set(response,'$.last_intervention',json(?1)),'$.intervention') WHERE id=?2",params![action.to_string(),t.operation])?;
        return Ok(Some(false));
    }
    let target = if abort {
        t.source_node.as_str()
    } else {
        action["node"].as_str().context("new target required")?
    };
    ensure!(target != old, "retarget requires another runner");
    let c = shared.lock().unwrap();
    authorized(&c, t)?;
    capable_move(&c, target, input.disk_bytes.saturating_mul(2))?;
    c.retarget_transfer(t.id, t.epoch, target, input, &action)?;
    drop(c);
    // A returning source is cleaned only after target exclusion and fresh-epoch reservation.
    // The imported capture remains durable and the saved source fence is retained for retries.
    let _ = auth;
    Ok(Some(true))
}
