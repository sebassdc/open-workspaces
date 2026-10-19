//! Durable controller transfers over the existing catalog, artifact store and node jobs.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Move,
    Wake,
    Park,
    Fork,
    Restore,
}
impl Kind {
    pub fn operation(self) -> &'static str {
        match self {
            Self::Move => "move",
            Self::Wake => "wake",
            Self::Park => "park",
            Self::Fork => "fork",
            Self::Restore => "restore",
        }
    }
    pub fn permission(self) -> &'static str {
        if self == Self::Wake {
            "start"
        } else {
            self.operation()
        }
    }
    pub fn needs_fence(self) -> bool {
        matches!(self, Self::Move | Self::Park | Self::Restore)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Step {
    Preparing,
    Exported,
    Importing,
    ImportUncertain,
    Prepared,
    Committed,
    StartUncertain,
    Started,
    Succeeded,
    Aborted,
    Failed,
}
impl Step {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Aborted | Self::Failed)
    }
    pub fn before_commit(self) -> bool {
        matches!(
            self,
            Self::Preparing
                | Self::Exported
                | Self::Importing
                | Self::ImportUncertain
                | Self::Prepared
        )
    }
    pub fn permits(self, kind: Kind, next: Self) -> bool {
        match (self, next) {
            (Self::Preparing, Self::Exported) => kind.needs_fence() || kind == Kind::Wake,
            (Self::Preparing, Self::Importing) => !kind.needs_fence(),
            (Self::Exported, Self::Prepared) => kind == Kind::Park,
            (Self::Exported, Self::Importing) => kind != Kind::Park,
            (Self::Importing, Self::ImportUncertain | Self::Prepared) => true,
            (Self::ImportUncertain, Self::Prepared) => true,
            (Self::Committed, Self::StartUncertain | Self::Started) => kind != Kind::Park,
            (Self::StartUncertain, Self::Started) => true,
            (Self::Committed, Self::Succeeded) => kind == Kind::Park,
            (Self::Started, Self::Succeeded) => true,
            (_, Self::Aborted | Self::Failed) => self.before_commit(),
            _ => false, // Commit is a separate conditional catalog transaction.
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Child {
    pub id: String,
    pub name: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Intent {
    pub kind: Kind,
    pub machine: String, // authorized logical address, never artifact-provided ownership
    pub target: Option<String>, // None means parked, not an inferred local runner
    pub capture: Option<String>, // existing F-store artifact ID
    pub child: Option<Child>,
    pub memory_mib: u32,
    pub cpus: u32,
    pub disk_bytes: u64,
    pub accept_cold: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub explicit_start: bool,
}
fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Default)]
pub(crate) struct Evidence {
    pub capture: Option<String>,
    pub source_fenced: bool,
    pub target_prepared: bool,
    pub target_excluded: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Transfer {
    pub id: i64,
    pub operation: i64,
    pub kind: Kind,
    pub source: String,
    pub machine: String,
    pub source_node: String,
    pub source_epoch: i64,
    pub source_capture: Option<String>,
    pub target: Option<String>,
    pub epoch: i64,
    pub capture: Option<String>,
    pub step: Step,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PublicSshPolicy {
    Disabled,
    Enrolled(ow_core::ssh_guest::Policy),
}

use crate::{
    catalog::{Catalog, Identity},
    common, nodes, wire,
};
use anyhow::{Context, Result, ensure};
use ow_core::store::{self, Objects, Reference};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

type Shared = Arc<Mutex<Catalog>>;
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn name(t: &Transfer) -> String {
    format!("transfer-{}", t.id)
}
fn authority(t: &Transfer, node: &str) -> common::Placement {
    common::Placement {
        epoch: t.epoch as u64,
        node: node.into(),
        transfer: name(t),
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    id: String,
    machine: String,
    store_url: String,
    reference: Reference,
    snapshot: String,
    memory_mib: u32,
    cpus: u32,
    disk_bytes: u64,
}
fn artifact(c: &Catalog, t: &Transfer) -> Result<Artifact> {
    let intent = intent(c, t)?;
    c.db.query_row("SELECT a.id,a.machine,a.store_url,a.attempt,a.digest,a.local_name FROM artifacts a JOIN resources r ON r.kind='machine' AND r.physical=?2 WHERE a.id=?1 AND a.owner=r.owner AND a.machine=r.physical AND a.state='stored' AND a.kind IN ('capture','snapshot')",params![t.capture,t.source],|r| Ok(Artifact {
        id:r.get(0)?,machine:r.get(1)?,store_url:r.get(2)?,reference:Reference{attempt:r.get(3)?,digest:r.get(4)?},snapshot:r.get(5)?,memory_mib:intent.memory_mib,cpus:intent.cpus,disk_bytes:intent.disk_bytes
    })).map_err(Into::into)
}
fn intent(c: &Catalog, t: &Transfer) -> Result<Intent> {
    let text: String = c.db.query_row(
        "SELECT json_extract(request,'$.transfer') FROM operations WHERE id=?1",
        [t.operation],
        |r| r.get(0),
    )?;
    Ok(serde_json::from_str(&text)?)
}
fn initiator_authorized(c: &Catalog, t: &Transfer) -> Result<bool> {
    let (user, address): (i64, String) = c.db.query_row(
        "SELECT owner,json_extract(request,'$.transfer.machine') FROM operations WHERE id=?1",
        [t.operation],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(c.machine(user, &address, t.kind.permission())
        .is_ok_and(|id| id == t.source))
}
fn authorized(c: &Catalog, t: &Transfer) -> Result<()> {
    let (user, address): (i64, String) = c.db.query_row(
        "SELECT owner,json_extract(request,'$.transfer.machine') FROM operations WHERE id=?1",
        [t.operation],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(
        c.machine(user, &address, t.kind.permission())? == t.source,
        "transfer authorization changed"
    );
    ensure!(c.db.query_row("SELECT r.ssh_policy_generation=t.source_policy_generation FROM transfers t JOIN resources r ON r.kind='machine' AND r.physical=t.source WHERE t.id=?1",[t.id],|r|r.get::<_,bool>(0))?, "source authority or SSH policy changed");
    Ok(())
}
fn capable(c: &Catalog, node: &str, disk: u64) -> Result<()> {
    let text: String = c.db.query_row("SELECT capabilities FROM nodes WHERE id=?1 AND revoked=0 AND heartbeat>CAST(strftime('%s','now') AS INTEGER)-15 AND heartbeat<=CAST(strftime('%s','now') AS INTEGER) AND COALESCE(json_extract(capabilities,'$.drain'),0)=0",[node],|r|r.get(0)).context("target unavailable or draining")?;
    let caps: Value = serde_json::from_str(&text)?;
    common::placement_capable(&caps)?;
    ensure!(
        caps["managed_restore_v1"] == true,
        "runner agent lacks managed restore jobs"
    );
    ensure!(
        caps["disk_observed_unix_s"]
            .as_i64()
            .is_some_and(|at| at <= now() && at > now() - 15),
        "target disk headroom is stale or unknown"
    );
    let pending: i64 = c.db.query_row(
        "SELECT COALESCE(SUM(disk_bytes),0) FROM transfer_reservations WHERE node=?1",
        [node],
        |r| r.get(0),
    )?;
    ensure!(
        caps["available_disk_bytes"]
            .as_u64()
            .is_some_and(|n| n >= disk.saturating_add((pending as u64).saturating_mul(2))),
        "target disk headroom exhausted or unknown"
    );
    Ok(())
}
fn rpc(root: &Path, node: &str, request: Value) -> Result<Value> {
    let route = nodes::route(root, node)?;
    if node == "local" {
        return if matches!(
            request["op"].as_str(),
            Some(
                "capture-cache"
                    | "capture-pull"
                    | "restore-policy"
                    | "capture-push"
                    | "capture-published"
                    | "capture-progress"
            )
        ) {
            job(root, &request)
        } else {
            wire::request(root, request)
        };
    }
    Ok(wire::connect_waiting(&route, &request, Duration::from_secs(7200))?.1)
}

/// Admit a catalog-owned restore; never resolve an owner from object storage.
/// All worker/agent waits occur without the catalog mutex.
pub(crate) fn restore(shared: Shared, identity: &Identity, request: &Value) -> Result<Value> {
    let key = request["operation_key"]
        .as_str()
        .context("operation key required")?;
    let address = request["id"].as_str().context("machine required")?;
    let target = request["node"].as_str().context("target required")?;
    let capture = request["artifact"].as_str().context("artifact required")?;
    for value in [key, target, capture] {
        common::identifier(value)?;
    }
    common::address(address)?;
    let (user, root, physical, source, epoch, mut proposed, joined) = {
        let mut c = shared.lock().unwrap();
        let user = c.user(identity)?;
        let physical = c.machine(user, address, "restore")?;
        let (owner,source,epoch,metadata):(i64,String,i64,String) = c.db.query_row("SELECT owner,node,placement_epoch,metadata FROM resources WHERE kind='machine' AND physical=?1",[&physical],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        ensure!(owner == user, "managed restore requires owner");
        ensure!(c.db.query_row("SELECT EXISTS(SELECT 1 FROM artifacts WHERE id=?1 AND owner=?2 AND machine=?3 AND state='stored' AND kind IN ('capture','snapshot') AND digest IS NOT NULL)",params![capture,user,physical],|r|r.get::<_,bool>(0))?,"capture is not a committed artifact of this machine and owner");

        let meta: Value = serde_json::from_str(&metadata)?;
        let proposed = Intent {
            kind: Kind::Restore,
            machine: address.into(),
            target: Some(target.into()),
            capture: Some(capture.into()),
            child: None,
            memory_mib: meta["memory_mib"]
                .as_u64()
                .context("unknown source memory")?
                .try_into()?,
            cpus: meta["vcpu_count"]
                .as_u64()
                .context("unknown source CPUs")?
                .try_into()?,
            disk_bytes: meta["disk_bytes"]
                .as_u64()
                .context("unknown source disk size")?,
            accept_cold: request["accept_cold"].as_bool().unwrap_or(false),
            explicit_start: false,
        };
        let joined:Option<i64> = c.db.query_row("SELECT t.id FROM transfers t JOIN operations o ON o.id=t.operation WHERE o.owner=?1 AND o.retry_key=?2",params![user,key],|r|r.get(0)).optional()?;
        (
            user,
            c.root.clone(),
            physical,
            source,
            epoch,
            proposed,
            joined,
        )
    };
    let transfer = if let Some(id) = joined {
        let c = shared.lock().unwrap();
        // Shape comes from the original intent after placement changes, not a refreshed inventory.
        let previous = intent(&c, &c.transfer(id)?)?;
        proposed.memory_mib = previous.memory_mib;
        proposed.cpus = previous.cpus;
        proposed.disk_bytes = previous.disk_bytes;
        c.begin_transfer(user, key, &proposed)?
    } else {
        ensure!(
            target != source && !source.is_empty(),
            "restore requires a different target and a positively fenced source"
        );
        // The agent reads only this positively stopped copy's public policy. No guest wake.
        let policy = rpc(&root, &source, json!({"op":"restore-policy","id":physical}))?;
        let c = shared.lock().unwrap();
        ensure!(
            c.machine(user, address, "restore")? == physical,
            "source changed"
        );
        ensure!(c.db.query_row("SELECT node=?2 AND placement_epoch=?3 FROM resources WHERE kind='machine' AND physical=?1",params![physical,source,epoch],|r|r.get::<_,bool>(0))?,"source authority changed");
        capable(&c, target, proposed.disk_bytes.saturating_mul(2))?;
        let stored: Option<String> = c.db.query_row(
            "SELECT ssh_policy FROM resources WHERE kind='machine' AND physical=?1",
            [&physical],
            |r| r.get(0),
        )?;
        let policy: PublicSshPolicy = serde_json::from_value(policy)?;
        if stored.is_none() {
            c.set_transfer_ssh_policy(user, address, 0, &policy)?;
        } else {
            ensure!(
                same_policy(
                    &serde_json::from_str::<Value>(&stored.unwrap())?,
                    &serde_json::to_value(&policy)?
                ),
                "source public policy differs from catalog"
            );
        }
        c.begin_transfer(user, key, &proposed)?
    };
    let result = shared
        .lock()
        .unwrap()
        .operation_info(user, transfer.operation)?;
    launch(shared, transfer.id);
    Ok(result)
}
fn same_policy(a: &Value, b: &Value) -> bool {
    let mut a = a.clone();
    let mut b = b.clone();
    for v in [&mut a, &mut b] {
        if let Some(p) = v.get_mut("enrolled").and_then(Value::as_object_mut) {
            p.remove("ready");
            p.remove("generation");
        }
    }
    a == b
}

/// Also run at controller startup: pending durable intents survive process loss.
pub(crate) fn recover(shared: &Shared) {
    let weak = Arc::downgrade(shared);
    std::thread::spawn(move || {
        loop {
            let Some(shared) = weak.upgrade() else { break };
            let ids = (|| -> Result<Vec<i64>> {
                let c = shared.lock().unwrap();
                Ok(c.db.prepare("SELECT id FROM transfers WHERE kind IN ('restore','park','wake') AND step NOT IN ('succeeded','aborted','failed')")?.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?)
            })();
            if let Ok(ids) = ids {
                for id in ids {
                    launch(shared.clone(), id);
                    let _ = capture_upload::refresh_progress(&shared, id);
                }
            }
            drop(shared);
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}
fn launch(shared: Shared, id: i64) {
    static RUNNING: Mutex<std::collections::BTreeSet<(std::path::PathBuf, i64)>> =
        Mutex::new(std::collections::BTreeSet::new());
    let root = shared.lock().unwrap().root.clone();
    let key = (root.clone(), id);
    {
        let mut running = RUNNING.lock().unwrap();
        if running.contains(&key) || running.len() >= 4 {
            return;
        }
        running.insert(key.clone());
    }
    std::thread::spawn(move || {
        let result = (|| -> Result<()> {
            let lock = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(root.join(format!("transfer-{id}.lock")))?;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Ok(());
            }
            #[cfg(test)]
            process_tests::controller_checkpoint(&root, id)?;
            loop {
                let more = tick(&shared, id, now(), &|node, request| {
                    rpc(&root, node, request)
                })?;
                #[cfg(test)]
                process_tests::controller_checkpoint(&root, id)?;
                if !more {
                    break;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            let c = shared.lock().unwrap();
            if let Ok(t) = c.transfer(id) {
                if t.step == Step::Importing {
                    let _ = c.advance_transfer(
                        t.id,
                        t.epoch,
                        t.step,
                        Step::ImportUncertain,
                        &Evidence::default(),
                    );
                }
                let _=c.db.execute("UPDATE operations SET state='uncertain',response=json_set(COALESCE(response,'{}'),'$.error',?1) WHERE id=?2 AND state<>'succeeded'",params![format!("{error:#}"),t.operation]);
            }
        }
        RUNNING.lock().unwrap().remove(&key);
    });
}

// A previous failed restore may have fenced this same committed source already.
// Only a matching durable catalog intent plus a fresh worker acknowledgment is reusable.
fn source_fence(
    c: &Catalog,
    t: &Transfer,
    seen: &Value,
    original: &common::Placement,
) -> Result<()> {
    ensure!(
        seen["id"] == t.source
            && seen["vmm_running"] == false
            && matches!(
                seen["lifecycle"]["phase"].as_str(),
                Some("exported" | "cleaned")
            ),
        "contradictory returning source; reconciliation required"
    );
    let export = seen["lifecycle"]["intent"]["transfer"]
        .as_str()
        .context("missing source export identity")?;
    let id = export
        .strip_prefix("transfer-")
        .context("unknown source export")?
        .parse::<i64>()?;
    let previous = c.transfer(id)?;
    ensure!(
        matches!(previous.kind, Kind::Restore | Kind::Park)
            && previous.source == t.source
            && previous.source_node == t.source_node
            && previous.source_epoch == t.source_epoch
            && previous.epoch <= t.epoch,
        "source export belongs to another intent"
    );
    let expected = if t.source_epoch == 0 {
        authority(&previous, &t.source_node)
    } else {
        original.clone()
    };
    ensure!(
        seen["placement"] == json!(expected),
        "contradictory source placement"
    );
    Ok(())
}

fn export_source(
    shared: &Shared,
    t: &Transfer,
    input: &Intent,
    policy: &Value,
    source_authority: &common::Placement,
    call: &impl Fn(&str, Value) -> Result<Value>,
) -> Result<bool> {
    let inspect = |node: &str| call(node, json!({"op":"inspect","id":t.machine}));
    let step_request = |op: &str, auth: &common::Placement| json!({"op":op,"id":t.machine,"placement":auth,"operation_key":format!("{}-{op}",name(t))});
    let advance = |next, proof| {
        shared
            .lock()
            .unwrap()
            .advance_transfer(t.id, t.epoch, t.step, next, &proof)
            .map(|_| true)
    };

    let seen = inspect(&t.source_node)?;
    ensure!(
        seen["id"] == t.source
            && seen["memory_mib"] == input.memory_mib
            && seen["vcpu_count"] == input.cpus
            && seen["disk_bytes"] == input.disk_bytes,
        "source identity or disk shape differs from catalog intent"
    );
    ensure!(
        matches!(
            seen["state"].as_str(),
            Some("stopped" | "hibernated" | "exported")
        ) && seen["vmm_running"] != true,
        "running or unknown source is ineligible for transfer"
    );
    if matches!(
        seen["lifecycle"]["phase"].as_str(),
        Some("exported" | "cleaned")
    ) {
        source_fence(&shared.lock().unwrap(), &t, &seen, &source_authority)?;
        return advance(
            Step::Exported,
            Evidence {
                source_fenced: true,
                ..Default::default()
            },
        );
    }
    if seen.get("placement").is_none() {
        ensure!(
            t.source_epoch == 0 && seen["state"] == "stopped",
            "unmanaged source must be stopped"
        );
        let mut register = step_request("placement-register", &source_authority);
        register["committed"] = json!(true);
        call(&t.source_node, register)?;
    } else {
        ensure!(
            seen["placement"] == json!(source_authority),
            "contradictory source placement"
        );
    }
    let mut export = step_request("export", &source_authority);
    export["transfer"] = json!(name(&t));
    export["capture"] = json!(format!("{}-source", name(&t)));
    let result = call(&t.source_node, export)?;
    source_fence(&shared.lock().unwrap(), &t, &result, &source_authority)?;
    let current = call(&t.source_node, json!({"op":"restore-policy","id":t.source}))?;
    ensure!(
        same_policy(&policy, &current),
        "source SSH policy changed before export"
    );
    advance(
        Step::Exported,
        Evidence {
            source_fenced: true,
            ..Default::default()
        },
    )
}

/// One durable transition. A restart can occur on either side of every RPC or write.
fn tick(
    shared: &Shared,
    id: i64,
    at: i64,
    call: &impl Fn(&str, Value) -> Result<Value>,
) -> Result<bool> {
    let initial = shared.lock().unwrap().transfer(id)?;
    if initial.kind == Kind::Park {
        return parking::tick(shared, &initial, at, call);
    }
    if initial.kind == Kind::Wake && initial.step == Step::Preparing {
        return parking::reserve_wake(shared, &initial);
    }
    let (t, a, input, policy, source_authority) = {
        let c = shared.lock().unwrap();
        let t = c.transfer(id)?;
        if t.step.terminal() {
            return Ok(false);
        }
        authorized(&c, &t)?;
        let a = artifact(&c, &t)?;
        let input = intent(&c, &t)?;
        let policy: Value = serde_json::from_str(&c.db.query_row(
            "SELECT ssh_policy FROM resources WHERE kind='machine' AND physical=?1",
            [&t.source],
            |r| r.get::<_, String>(0),
        )?)?;
        let source_authority = if t.source_epoch == 0 {
            authority(&t, &t.source_node)
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
        (t, a, input, policy, source_authority)
    };
    let target = t.target.as_deref().context("target missing")?;
    let placement = authority(&t, target);
    let advance = |next, proof| {
        shared
            .lock()
            .unwrap()
            .advance_transfer(t.id, t.epoch, t.step, next, &proof)
            .map(|_| true)
    };
    let inspect = |node: &str| call(node, json!({"op":"inspect","id":t.machine}));
    let step_request = |op: &str, auth: &common::Placement| json!({"op":op,"id":t.machine,"placement":auth,"operation_key":format!("{}-{op}",name(&t))});
    match t.step {
        Step::Preparing => export_source(shared, &t, &input, &policy, &source_authority, call),
        Step::Exported => {
            // Cache inspection is read-only, digest-bearing proof; never blindly reissue a pull.
            let mut request = json!({"op":"capture-cache","artifact":a});
            let cached = call(target, request.clone())?;
            if cached["present"] != true {
                let token = {
                    let c = shared.lock().unwrap();
                    capable(&c, target, 0)?;
                    let holder = format!("{}-pull", name(&t));
                    let tx = rusqlite::Transaction::new_unchecked(
                        &c.db,
                        rusqlite::TransactionBehavior::Immediate,
                    )?;
                    let old:Option<i64>=tx.query_row("SELECT MAX(expires) FROM leases WHERE artifact=?1 AND holder=?2 AND purpose='pull'",params![a.id,holder],|r|r.get(0))?;
                    ensure!(
                        old.is_none_or(|expires| expires <= at),
                        "pull outcome uncertain; retain old lease until expiry and inspect cache"
                    );
                    let token = store::token()?;
                    tx.execute("INSERT INTO leases(artifact,holder,purpose,token,expires) VALUES(?1,?2,'pull',?3,?4)",params![a.id,holder,token,at+7200])?;
                    tx.commit()?;
                    token
                };
                request["op"] = json!("capture-pull");
                request["attempt"] = json!(token);
                request["expires"] = json!(at + 7200);
                let result = call(target, request)?;
                ensure!(
                    result["digest"] == a.reference.digest && result["present"] == true,
                    "pull did not confirm pinned digest"
                );
            }
            let cached = call(target, json!({"op":"capture-cache","artifact":a}))?;
            ensure!(
                cached["present"] == true && cached["digest"] == a.reference.digest,
                "verified target cache unavailable"
            );
            if cached["ram"] == true && !input.accept_cold {
                let status = call(target, json!({"op":"status"}))?;
                if status["compat"] != cached["compat"] {
                    let c = shared.lock().unwrap();
                    c.advance_transfer(t.id, t.epoch, t.step, Step::Failed, &Evidence::default())?;
                    c.db.execute("UPDATE operations SET response=json_set(COALESCE(response,'{}'),'$.error',?1) WHERE id=?2",params!["incompatible stored RAM; retry with a new operation key and explicit --accept-cold consent",t.operation])?;
                    return Ok(false);
                }
            }
            let c = shared.lock().unwrap();
            authorized(&c, &t)?;
            if let Some(bytes) = cached["bytes_total"].as_u64() {
                c.db.execute("UPDATE operations SET response=json_set(COALESCE(response,'{}'),'$.progress',json(?1)) WHERE id=?2",params![json!({"bytes_completed":bytes,"bytes_total":bytes,"unit":"logical bytes in completed file objects"}).to_string(),t.operation])?;
            }
            c.db.execute("INSERT INTO artifact_copies(artifact,node,state,last_used) VALUES(?1,?2,'present',?3) ON CONFLICT(artifact,node) DO UPDATE SET state='present',last_used=excluded.last_used",params![a.id,target,at])?;
            drop(c);
            advance(Step::Importing, Evidence::default())
        }
        Step::Importing | Step::ImportUncertain => {
            let cache = call(target, json!({"op":"capture-cache","artifact":a}))?;
            ensure!(
                cache["present"] == true && cache["digest"] == a.reference.digest,
                "verified target cache unavailable"
            );
            let mut request = step_request("import-prepare", &placement);
            request["snapshot"] = json!(a.snapshot);
            request["manifest_sha256"] = cache["manifest_sha256"].clone();
            request["cold"] = json!(input.accept_cold);
            // T2 journal replays only the same immutable key/fingerprint and reconciles partial imports.
            let mut inventory = call(target, json!({"op":"list"}))?;
            if t.kind == Kind::Wake {
                if let Some(seen) = inventory
                    .as_array()
                    .context("invalid target inventory")?
                    .iter()
                    .find(|v| v["id"] == t.machine)
                {
                    if matches!(
                        seen["lifecycle"]["phase"].as_str(),
                        Some("exported" | "cleaned")
                    ) {
                        parking::clean_cached_source(shared, &t, target, seen, call)?;
                        inventory
                            .as_array_mut()
                            .unwrap()
                            .retain(|v| v["id"] != t.machine);
                    }
                }
            }
            let existing = inventory
                .as_array()
                .context("invalid target inventory")?
                .iter()
                .find(|v| v["id"] == t.machine);
            if let Some(existing) = existing {
                ensure!(
                    existing["placement"] == json!(placement)
                        && existing["lifecycle"]["import"]["snapshot"] == a.snapshot
                        && existing["lifecycle"]["import"]["manifest_sha256"]
                            == cache["manifest_sha256"]
                        && existing["lifecycle"]["import"]["cold"] == input.accept_cold
                        && existing["vmm_running"] == false
                        && matches!(
                            existing["lifecycle"]["phase"].as_str(),
                            Some("importing" | "prepared")
                        ),
                    "contradictory target incarnation"
                );
            }
            let result = if existing.is_some_and(|v| v["lifecycle"]["phase"] == "prepared") {
                existing.unwrap().clone()
            } else {
                call(target, request)?
            };
            ensure!(
                result["id"] == t.machine
                    && result["placement"] == json!(placement)
                    && result["lifecycle"]["phase"] == "prepared"
                    && result["vmm_running"] == false,
                "contradictory import outcome"
            );
            call(
                target,
                json!({"op":"restore-policy","id":t.machine,"placement":placement,"policy":policy}),
            )?;
            advance(
                Step::Prepared,
                Evidence {
                    target_prepared: true,
                    ..Default::default()
                },
            )
        }
        Step::Prepared => {
            if t.kind.needs_fence()
                && let Ok(source) = inspect(&t.source_node)
            {
                source_fence(&shared.lock().unwrap(), &t, &source, &source_authority)?;
            }
            let result = inspect(target)?;
            ensure!(
                result["id"] == t.machine
                    && result["placement"] == json!(placement)
                    && result["lifecycle"]["phase"] == "prepared"
                    && result["vmm_running"] == false,
                "prepared target changed"
            );
            let c = shared.lock().unwrap();
            authorized(&c, &t)?;
            c.commit_transfer(t.id, t.epoch)?;
            Ok(true)
        }
        Step::Committed => advance(Step::StartUncertain, Evidence::default()),
        Step::StartUncertain => {
            if t.kind.needs_fence()
                && let Ok(source) = inspect(&t.source_node)
            {
                source_fence(&shared.lock().unwrap(), &t, &source, &source_authority)?;
            }
            let seen = inspect(target)?;
            ensure!(
                seen["id"] == t.machine && seen["placement"] == json!(placement),
                "contradictory activation placement"
            );
            if seen["lifecycle"]["phase"] == "active" {
                ensure!(
                    seen["state"] == "running" && seen["vmm_running"] == true,
                    "activation outcome needs explicit reconciliation; refusing another start"
                );
            } else {
                ensure!(
                    seen["lifecycle"]["phase"] == "prepared" && seen["vmm_running"] == false,
                    "contradictory activation state"
                );
                let mut request = step_request("activate", &placement);
                request["committed"] = json!(true);
                request["start"] = json!(true);
                call(target, request)?;
                let seen = inspect(target)?;
                ensure!(
                    seen["placement"] == json!(placement)
                        && seen["state"] == "running"
                        && seen["vmm_running"] == true,
                    "activation not confirmed"
                );
            }
            advance(Step::Started, Evidence::default())
        }
        Step::Started => {
            let seen = inspect(target)?;
            ensure!(
                seen["placement"] == json!(placement) && seen["lifecycle"]["phase"] == "active",
                "target authority changed"
            );
            let c = shared.lock().unwrap();
            c.db.execute("UPDATE resources SET metadata=?1 WHERE kind='machine' AND physical=?2 AND placement_epoch=?3 AND node=?4",params![seen.to_string(),t.machine,t.epoch,target])?;
            c.advance_transfer(t.id, t.epoch, t.step, Step::Succeeded, &Evidence::default())?;
            Ok(false)
        }
        _ => anyhow::bail!("unsupported restore phase"),
    }
}

fn native(manifest: &store::Manifest, a: &Artifact) -> Result<(String, Value)> {
    manifest.validate()?;
    ensure!(
        matches!(manifest.kind.as_str(), "capture" | "snapshot"),
        "not a workspace capture"
    );
    let raw = manifest.metadata["native_manifest"]
        .as_str()
        .context("native manifest required")?
        .to_owned();
    ensure!(raw.len() <= 65536, "native manifest too large");
    let value: Value = serde_json::from_str(&raw)?;
    ensure!(
        value["workspace"] == a.machine && value["name"] == a.snapshot,
        "artifact lineage differs from catalog intent"
    );
    ensure!(
        value["memory_mib"] == a.memory_mib && value["vcpu_count"] == a.cpus,
        "artifact shape differs from catalog intent"
    );
    ensure!(
        manifest.files["disk.ext4"].size == a.disk_bytes,
        "artifact disk differs from catalog intent"
    );
    ensure!(
        manifest.files.len() == 1
            || (manifest.files.len() == 3
                && manifest.files.contains_key("state")
                && manifest.files.contains_key("memory")),
        "incomplete consistency set"
    );
    let hashes = value["files"]
        .as_object()
        .context("native hashes required")?;
    ensure!(
        hashes.len() == manifest.files.len(),
        "native file set mismatch"
    );
    for (file, stored) in &manifest.files {
        ensure!(
            hashes.get(file) == Some(&json!(stored.sha256)),
            "native file hash mismatch"
        );
    }
    Ok((raw, value))
}
fn cache(root: &Path, a: &Artifact) -> Result<Value> {
    common::identifier(&a.snapshot)?;
    common::identifier(&a.machine)?;
    store::valid_digest(&a.reference.digest)?;
    store::attempt_prefix(&a.reference.attempt)?;
    let path = root.join("snapshots").join(&a.snapshot);
    if !path.exists() {
        return Ok(json!({"present":false}));
    }
    ensure!(
        fs::symlink_metadata(&path)?.is_dir(),
        "capture cache is not a directory"
    );
    let bytes = fs::read(path.join(".artifact.json"))?;
    ensure!(
        bytes.len() <= 1024 * 1024 && store::sha256(&bytes) == a.reference.digest,
        "cached manifest digest differs"
    );
    let manifest: store::Manifest = serde_json::from_slice(&bytes)?;
    manifest.validate()?;
    ensure!(
        manifest.attempt == a.reference.attempt,
        "cached artifact attempt differs"
    );
    let (raw, native) = native(&manifest, a)?;
    ensure!(
        fs::read(path.join("manifest.json"))? == raw.as_bytes(),
        "cached native manifest differs"
    );
    for (file, stored) in &manifest.files {
        let path = path.join(file);
        let meta = fs::symlink_metadata(&path)?;
        ensure!(
            meta.is_file()
                && meta.len() == stored.size
                && store::hash_file(&path)? == stored.sha256,
            "cached file hash mismatch"
        );
    }
    Ok(
        json!({"present":true,"digest":a.reference.digest,"manifest_sha256":store::sha256(raw.as_bytes()),"bytes_total":manifest.files.values().map(|file|file.size).sum::<u64>(),"ram":manifest.files.len()==3,"compat":{"runtime":native["runtime"],"cpu":native["cpu"],"kernel_sha256":native["kernel_sha256"]}}),
    )
}
fn pull(objects: &dyn Objects, root: &Path, a: &Artifact, attempt: &str) -> Result<Value> {
    store::attempt_prefix(attempt)?;
    let snapshots = root.join("snapshots");
    fs::create_dir_all(&snapshots)?;
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(snapshots.join(format!(".{}.lock", a.snapshot)))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "capture pull already in flight"
    );
    let cached = cache(root, a)?;
    if cached["present"] == true {
        return Ok(cached);
    }
    let stage = snapshots.join(format!(".pull-{attempt}"));
    // Each agent attempt owns one staging path. Expired attempts are never replayed.
    let manifest = capture_upload::pull(objects, root, a, attempt, &stage)?;
    let result = (|| -> Result<Value> {
        let (raw, _) = native(&manifest, a)?;
        fs::rename(stage.join("manifest.json"), stage.join(".artifact.json"))?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o400)
            .open(stage.join("manifest.json"))?;
        use std::io::Write;
        file.write_all(raw.as_bytes())?;
        file.sync_all()?;
        fs::File::open(&stage)?.sync_all()?;
        ensure!(
            !snapshots.join(&a.snapshot).exists(),
            "existing capture cannot be overwritten"
        );
        fs::rename(&stage, snapshots.join(&a.snapshot))?;
        fs::File::open(&snapshots)?.sync_all()?;
        cache(root, a)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(stage);
    }
    result
}
/// Private agent jobs; transfer/hash work stays outside the serial worker.
pub(crate) fn job(root: &Path, request: &Value) -> Result<Value> {
    if matches!(
        request["op"].as_str(),
        Some("capture-push" | "capture-published" | "capture-progress")
    ) {
        return capture_upload::job(root, request);
    }
    if request["op"] == "restore-policy" {
        let id = request["id"].as_str().context("machine required")?;
        common::identifier(id)?;
        let seen = wire::request(root, json!({"op":"inspect","id":id}))?;
        ensure!(
            seen["vmm_running"] != true
                && matches!(
                    seen["state"].as_str(),
                    Some("stopped" | "hibernated" | "prepared" | "exported")
                ),
            "public policy requires a fenced or stopped copy"
        );
        if let Some(value) = request.get("policy") {
            let placement = common::Placement::from_request(request)?;
            ensure!(
                seen["placement"] == json!(placement) && seen["lifecycle"]["phase"] == "prepared",
                "policy install requires exact unstarted import"
            );
            let policy: PublicSshPolicy = serde_json::from_value(value.clone())?;
            let path = ow_core::ssh_guest::path(root, id);
            match policy {
                PublicSshPolicy::Disabled => {
                    ensure!(!path.exists(), "unexpected target public SSH policy");
                }
                PublicSshPolicy::Enrolled(mut policy) => {
                    common::identifier(&policy.identity)?;
                    common::identifier(&policy.generation)?;
                    ensure!(
                        ow_core::ssh_guest::key(&policy.host_key)? == policy.host_key,
                        "noncanonical host key"
                    );
                    ensure!(
                        ow_core::ssh_guest::validate_keys(&json!(policy.keys))? == policy.keys,
                        "noncanonical keys"
                    );
                    policy.ready = false;
                    ow_core::runtime::atomic_json(&path, &json!(policy))?;
                }
            }
            return Ok(json!({"installed":true}));
        }
        return match ow_core::ssh_guest::load(root, id) {
            Ok(policy) => Ok(serde_json::to_value(PublicSshPolicy::Enrolled(policy))?),
            Err(_) if !ow_core::ssh_guest::path(root, id).exists() => Ok(json!("disabled")),
            Err(e) => Err(e),
        };
    }
    let a: Artifact = serde_json::from_value(request["artifact"].clone())?;
    common::identifier(&a.snapshot)?;
    common::identifier(&a.machine)?;
    ensure!(
        crate::image_store::configured_url(root)? == a.store_url,
        "controller store differs from runner configuration"
    );
    if request["op"] == "capture-cache" {
        return cache(root, &a);
    }
    ensure!(
        request["op"] == "capture-pull"
            && request["expires"]
                .as_i64()
                .is_some_and(|expires| expires > now() && expires <= now() + 7200),
        "expired capture pull"
    );
    let attempt = request["attempt"]
        .as_str()
        .context("pull attempt required")?;
    let objects = store::S3Objects::new(&a.store_url, None)?;
    pull(&objects, root, &a, attempt)
}

#[cfg(test)]
#[path = "transfer_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "transfer_process_tests.rs"]
pub(crate) mod process_tests;

#[path = "parking.rs"]
mod parking;
pub(crate) use parking::{ensure_awake, park, wake};

#[path = "capture_upload.rs"]
mod capture_upload;
