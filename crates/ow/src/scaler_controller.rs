//! Private controller observations feed the shadow planner; never refresh through a cloud API.
use super::*;
use crate::{catalog::Catalog, nodes};
use serde::de::DeserializeOwned;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Shadow,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    mode: Mode,
    pools: Vec<Pool>,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum InstanceState {
    Ready,
    Pending,
    Retained,
    Orphaned,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Instance {
    id: String,
    pool: String,
    node: Option<String>,
    state: InstanceState,
    action: Option<String>,
    hourly_cents: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Disk {
    id: String,
    pool: String,
    logical_bytes: u64,
    hourly_cents: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Observations {
    observed_at: u64,
    /// Must match limits.pool.hours; forecasts must never silently change horizons.
    hours: u64,
    complete: bool,
    retention_ok: bool,
    outcome_resolved: bool,
    instances: Vec<Instance>,
    /// Every attached, pending, retained and orphaned billable volume, even with no node.
    disks: Vec<Disk>,
    receipts: BTreeMap<String, Receipt>,
}
fn read_private<T: DeserializeOwned>(path: &Path) -> Result<T> {
    nodes::private(path)?;
    ensure!(
        std::fs::metadata(path)?.len() <= 262144,
        "scaler file exceeds 256 KiB"
    );
    serde_json::from_slice(&std::fs::read(path)?).context("invalid private scaler file")
}
fn uint(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(row.get::<_, i64>(index)?).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(e),
        )
    })
}
fn inputs_on(
    db: &Connection,
    pools: &[Pool],
    observed: &Observations,
    now: u64,
    hours: u64,
    retired_nodes: &[String],
) -> Result<Vec<Input>> {
    ensure!(
        observed.hours == hours,
        "scaler liability horizon differs from limits.pool.hours"
    );
    ensure!(
        observed.instances.len() <= 10000 && observed.disks.len() <= 10000,
        "scaler inventory too large"
    );
    let mut ids = BTreeSet::new();
    let mut node_ids = BTreeSet::new();
    let mut total = limits::Liability::default();
    for i in &observed.instances {
        ensure!(
            ids.insert(&i.id) && !i.id.is_empty() && !i.pool.is_empty(),
            "duplicate or missing instance ID"
        );
        if let Some(node) = &i.node {
            ensure!(
                node_ids.insert(node),
                "node maps to multiple provider instances"
            );
        }
        total = total.add(limits::Liability {
            cents: i
                .hourly_cents
                .checked_mul(hours)
                .context("instance forecast overflow")?,
            instances: 1,
            disk_bytes: 0,
        })?;
    }
    ids.clear();
    for d in &observed.disks {
        ensure!(
            ids.insert(&d.id) && !d.id.is_empty() && !d.pool.is_empty(),
            "duplicate or missing disk identity"
        );
        total = total.add(limits::Liability {
            cents: d
                .hourly_cents
                .checked_mul(hours)
                .context("disk forecast overflow")?,
            instances: 0,
            disk_bytes: d.logical_bytes,
        })?;
    }
    // Missing/expired/revoked enrollment can still incur costs. Coverage is explicit.
    let nodes: Vec<String> = db
        .prepare("SELECT id FROM nodes ORDER BY rowid")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for retired in retired_nodes {
        ensure!(
            db.query_row("SELECT revoked FROM nodes WHERE id=?1", [retired], |r| r
                .get::<_, bool>(
                0
            ))?,
            "retired pool node must first be revoked"
        );
    }
    let coverage = nodes
        .iter()
        .filter(|n| !retired_nodes.contains(n))
        .all(|n| node_ids.contains(n));
    let base = nodes.first();
    let mut out = Vec::new();
    for pool in pools {
        let mut runners = Vec::new();
        let mut durability_healthy = true;
        for instance in observed.instances.iter().filter(|i| i.pool == pool.name) {
            let Some(id) = &instance.node else { continue };
            if !matches!(instance.state, InstanceState::Ready) {
                continue;
            }
            let row = db.query_row("SELECT heartbeat,capabilities,memory_mib,slots,rowid,revoked FROM nodes WHERE id=?1",[id],|r|Ok((uint(r,0)?,r.get::<_,String>(1)?,uint(r,2)?,uint(r,3)?,uint(r,4)?,r.get::<_,bool>(5)?))).optional()?;
            let Some((heartbeat, raw, memory, slots, joined, revoked)) = row else {
                durability_healthy = false;
                continue;
            };
            let mut cap: Value = serde_json::from_str(&raw)?;
            ensure!(cap.is_object(), "invalid runner capabilities");
            let (actual_pool, active, controller_drain): (String, bool, bool) = db
                .query_row(
                    "SELECT pool,active,controller_drain FROM node_placement_policy WHERE node=?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?
                .unwrap_or(("default".into(), true, false));
            // Provider membership cannot expand catalog pool authority.
            if actual_pool != pool.name {
                durability_healthy = false;
                continue;
            }
            cap["controller_drain"] = json!(controller_drain);
            let online = !revoked && fresh(heartbeat, now);
            let cpu_cap:u64 = db.query_row("SELECT COALESCE((SELECT vcpu_cap FROM node_capacity_limits WHERE node=?1),(SELECT vcpu_cap FROM node_limits WHERE node=?1),16)",[id],|r|uint(r,0))?;
            let total_shape = Shape {
                cpus: cap["vcpus"].as_u64().unwrap_or(0).min(cpu_cap),
                memory_mib: cap["memory_mib"].as_u64().unwrap_or(0).min(memory),
                slots: cap["slots"].as_u64().unwrap_or(0).min(slots),
                disk_bytes: cap["max_disk_gib"]
                    .as_u64()
                    .unwrap_or(0)
                    .checked_mul(common::GIB)
                    .context("runner disk overflow")?,
            };
            let (ram,cpu,count):(u64,u64,u64) = db.query_row("SELECT COALESCE(SUM(reserved_mib),0),COALESCE(SUM(reserved_cpus),0),COALESCE(SUM(CASE WHEN reserved_mib>0 THEN 1 ELSE 0 END),0) FROM resources WHERE kind='machine' AND node=?1",[id],|r|Ok((uint(r,0)?,uint(r,1)?,uint(r,2)?)))?;
            let (extra_ram, extra_cpu, extra_slots): (u64, u64, u64) = db
                .query_row(
                    "SELECT extra_mib,extra_cpus,extra_slots FROM node_usage WHERE node=?1",
                    [id],
                    |r| Ok((uint(r, 0)?, uint(r, 1)?, uint(r, 2)?)),
                )
                .optional()?
                .unwrap_or_default();
            let (transfer_ram,transfer_cpu,transfer_slots,transfer_disk):(u64,u64,u64,u64) = db.query_row("SELECT COALESCE(SUM(memory_mib),0),COALESCE(SUM(cpus),0),COUNT(*),COALESCE(SUM(disk_bytes),0) FROM transfer_reservations WHERE node=?1",[id],|r|Ok((uint(r,0)?,uint(r,1)?,uint(r,2)?,uint(r,3)?)))?;
            let logical:u64 = db.query_row("SELECT COALESCE(SUM(a.disk_gib),0) FROM owner_allocations a JOIN resources r ON r.kind='machine' AND r.physical=a.physical WHERE r.node=?1",[id],|r|uint(r,0))?;
            let logical = logical
                .checked_mul(common::GIB)
                .context("logical disk overflow")?;
            let disk_extra = transfer_disk.saturating_mul(2).saturating_add(
                logical.saturating_sub(cap["reserved_disk_bytes"].as_u64().unwrap_or(0)),
            );
            let (drain_state, eligible): (String, bool) = db
                .query_row(
                    "SELECT state,eligible FROM runner_drains WHERE node=?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .unwrap_or(("active".into(), false));
            let unresolved:bool = db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE node=?1 AND state IN ('pending','uncertain')) OR EXISTS(SELECT 1 FROM transfers WHERE (source_node=?1 OR target=?1) AND step NOT IN ('succeeded','failed','aborted'))",[id],|r|r.get(0))?;
            let boxes:Vec<(String,u64)> = db.prepare("SELECT metadata,CAST(strftime('%s',updated_at) AS INTEGER) FROM resources WHERE kind='machine' AND node=?1")?.query_map([id],|r|Ok((r.get(0)?,uint(r,1)?)))?.collect::<rusqlite::Result<_>>()?;
            let mut drain_safe = eligible
                && base != Some(id)
                && !pool.protected_nodes.contains(id)
                && extra_ram == 0
                && extra_cpu == 0
                && extra_slots == 0
                && !unresolved;
            drain_safe &= cap["workspace_count"].as_u64() == Some(boxes.len() as u64)
                && cap["drain_inventory_v1"] == true;
            for (raw, at) in boxes {
                let m: Value = serde_json::from_str(&raw)?;
                drain_safe &= fresh(at, now)
                    && matches!(m["state"].as_str(), Some("stopped" | "hibernated"))
                    && m["pinned"] != true;
                if matches!(m["state"].as_str(), Some("running" | "hibernated")) {
                    let d = &m["durability"];
                    let healthy = fresh(at, now)
                        && d["overdue"] == false
                        && d["error"].is_null()
                        && (d["current_state_durable"] == true
                            || d["last_durable_at"].as_u64().is_some_and(|at| {
                                at <= now && now - at <= ow_core::recovery::LOSS_TARGET
                            }));
                    durability_healthy &= healthy;
                } else if m["state"] != "stopped" {
                    durability_healthy = false;
                }
            }
            runners.push(Runner {
                candidate: placement::Candidate {
                    id: id.clone(),
                    online,
                    active: active
                        && drain_state == "active"
                        && !controller_drain
                        && cap["drain"] != true,
                    pool: actual_pool,
                    capabilities: cap.clone(),
                    free: placement::Capacity {
                        memory_mib: total_shape.memory_mib.saturating_sub(
                            ram.saturating_add(extra_ram).saturating_add(transfer_ram),
                        ),
                        cpus: total_shape.cpus.saturating_sub(
                            cpu.saturating_add(extra_cpu).saturating_add(transfer_cpu),
                        ),
                        slots: total_shape.slots.saturating_sub(
                            count
                                .saturating_add(extra_slots)
                                .saturating_add(transfer_slots),
                        ),
                        disk_bytes: if fresh(cap["disk_observed_unix_s"].as_u64().unwrap_or(0), now)
                        {
                            cap["available_disk_bytes"]
                                .as_u64()
                                .unwrap_or(0)
                                .saturating_sub(disk_extra)
                        } else {
                            0
                        },
                    },
                    joined,
                },
                total: total_shape,
                drain_safe,
                soft_draining: drain_state == "soft-draining",
            });
        }
        let mut queued = Vec::new();
        // Existing unreserved wake intents are the durable queue. Failed creates are not queued work.
        if pool.name == "default" {
            let rows:Vec<(i64,String,String)> = db.prepare("SELECT t.id,o.request,r.metadata FROM transfers t JOIN operations o ON o.id=t.operation JOIN resources r ON r.kind='machine' AND r.physical=t.source WHERE t.kind='wake' AND o.state IN ('pending','uncertain') AND t.step='preparing' AND t.target IS NULL ORDER BY t.id LIMIT 1025")?.query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
            ensure!(rows.len() <= 1024, "scaler queue exceeds 1024 shapes");
            for (id, raw, metadata) in rows {
                let v: Value = serde_json::from_str(&raw)?;
                let m: Value = serde_json::from_str(&metadata)?;
                let intent = &v["transfer"];
                // Missing compatibility remains unknown and cannot authorize a launch prediction.
                let saved:Option<(String,String)> = db.query_row("SELECT json_extract(o.response,'$.capture_manifest'),a.digest FROM artifacts a JOIN transfers t ON t.capture=a.id JOIN operations o ON o.id=t.operation WHERE a.id=(SELECT current_capture FROM resources WHERE kind='machine' AND physical=(SELECT source FROM transfers WHERE id=?1)) AND json_type(o.response,'$.capture_manifest')='object' ORDER BY t.id DESC LIMIT 1",[id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                let mut compatibility_known = intent["accept_cold"] == true;
                let mut compatibility = None;
                if !compatibility_known && let Some((raw, digest)) = saved {
                    let manifest: ow_core::store::Manifest = serde_json::from_str(&raw)?;
                    manifest.validate()?;
                    ensure!(
                        ow_core::store::sha256(&manifest.bytes()?) == digest,
                        "queued capture digest differs"
                    );
                    if manifest.files.len() == 1 {
                        compatibility_known = true;
                    } else if manifest.files.len() == 3 {
                        let native: Value = serde_json::from_str(
                            manifest.metadata["native_manifest"]
                                .as_str()
                                .context("missing capture compatibility")?,
                        )?;
                        let expected = json!({"runtime":native["runtime"],"cpu":native["cpu"],"kernel_sha256":native["kernel_sha256"]});
                        compatibility_known = expected
                            .as_object()
                            .unwrap()
                            .values()
                            .all(|v| v.as_str().is_some_and(|s| !s.is_empty()));
                        compatibility = Some(expected);
                    }
                }
                queued.push(Queued {
                    id: format!("wake-{id}"),
                    image: m["image"].as_str().unwrap_or("").into(),
                    shape: Shape {
                        cpus: intent["cpus"].as_u64().unwrap_or(0),
                        memory_mib: intent["memory_mib"].as_u64().unwrap_or(0),
                        slots: 1,
                        disk_bytes: intent["disk_bytes"]
                            .as_u64()
                            .unwrap_or(0)
                            .checked_mul(2)
                            .context("wake staging disk overflow")?,
                    },
                    compatibility,
                    compatibility_known,
                });
            }
        }
        let ready_instances = runners
            .iter()
            .filter(|r| r.candidate.online && r.candidate.active && !r.soft_draining)
            .count() as u64;
        let receipt = observed.receipts.get(&pool.name).cloned();
        if let Some(r) = &receipt {
            let correlated: Vec<_> = observed
                .instances
                .iter()
                .filter(|i| i.action.as_deref() == Some(&r.action))
                .collect();
            match r.outcome {
                Outcome::Joined => ensure!(
                    correlated.len() == 1
                        && correlated[0].pool == pool.name
                        && correlated[0].node == r.node
                        && matches!(correlated[0].state, InstanceState::Ready),
                    "join receipt has no exact provider instance"
                ),
                Outcome::Absent => ensure!(
                    correlated.is_empty(),
                    "absent receipt still has a provider instance"
                ),
                _ => {}
            }
        }
        out.push(Input {
            now,
            inventory_fresh: observed.complete && coverage && fresh(observed.observed_at, now),
            retention_ok: observed.retention_ok,
            outcome_resolved: observed.outcome_resolved,
            durability_healthy,
            liabilities: total,
            pool_instances: observed
                .instances
                .iter()
                .filter(|i| i.pool == pool.name)
                .count() as u64,
            ready_instances,
            runners,
            queued,
            receipt,
        });
    }
    Ok(out)
}
pub(crate) fn command(root: &Path, tick_now: bool) -> Result<Value> {
    let mut db = nodes::db(root)?;
    ensure!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? >= 7,
        "upgrade controller catalog before scaler commands"
    );
    if !tick_now {
        let mut result = status(&db)?;
        result["config_present"] = json!(root.join("scaler.json").exists());
        return Ok(result);
    }
    // Local tick serializes with the gateway through BEGIN IMMEDIATE.
    attempt(root, &mut db, ow_core::recovery::now())
}
fn tick_on(root: &Path, db: &mut Connection, now: u64) -> Result<Value> {
    let config: Config = read_private(&root.join("scaler.json"))?;
    let Mode::Shadow = config.mode;
    let observed: Observations = read_private(&root.join("scaler-observations.json"))?;
    let (hours, caps, retired_nodes) = limits::elastic_caps(root)?;
    ensure!(
        !config.pools.is_empty() && config.pools.len() <= 32,
        "1–32 scaler pools required"
    );
    let mut names = BTreeSet::new();
    for p in &config.pools {
        validate(p)?;
        ensure!(names.insert(&p.name), "duplicate scaler pool");
    }
    // No network or file access inside this writer transaction.
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let collected = inputs_on(&tx, &config.pools, &observed, now, hours, &retired_nodes)?;
    for (pool, input) in config.pools.iter().zip(collected.iter()) {
        record(&tx, pool, input, caps)?;
    }
    tx.execute("DELETE FROM scaler_health", [])?;
    let result = status(&tx)?;
    tx.commit()?;
    Ok(result)
}
fn attempt(root: &Path, db: &mut Connection, now: u64) -> Result<Value> {
    let result = tick_on(root, db, now);
    if let Err(e) = &result {
        db.execute(
            "INSERT INTO scaler_health VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET error=?1",
            [format!("{e:#}")],
        )?;
    }
    result
}
pub(crate) fn start(shared: &Arc<Mutex<Catalog>>) {
    if !shared.lock().unwrap().root.join("scaler.json").exists() {
        return;
    }
    let weak = Arc::downgrade(shared);
    std::thread::spawn(move || {
        loop {
            let Some(shared) = weak.upgrade() else { break };
            {
                let mut c = shared.lock().unwrap();
                let root = c.root.clone();
                let _ = attempt(&root, &mut c.db, ow_core::recovery::now());
            }
            drop(shared);
            if nodes::STOPPING.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

#[cfg(test)]
#[path = "scaler_controller_tests.rs"]
mod tests;
