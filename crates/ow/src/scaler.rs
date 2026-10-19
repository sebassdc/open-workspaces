//! Bounded shadow decisions. This module has no provider or worker effect path.
use crate::{common, limits, placement};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Preset {
    pub name: String,
    pub capacity: Shape,
    pub capabilities: Value,
    /// Upper bound over limits.pool.hours, including disks and other new charges.
    pub cost: limits::Liability,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Shape {
    pub cpus: u64,
    pub memory_mib: u64,
    pub slots: u64,
    pub disk_bytes: u64,
}
impl Shape {
    fn capacity(self) -> placement::Capacity {
        placement::Capacity {
            cpus: self.cpus,
            memory_mib: self.memory_mib,
            slots: self.slots,
            disk_bytes: self.disk_bytes,
        }
    }
    fn valid(self) -> bool {
        [self.cpus, self.memory_mib, self.slots, self.disk_bytes]
            .iter()
            .all(|n| *n > 0 && *n <= i64::MAX as u64 / 4)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pool {
    pub name: String,
    pub floor: u64,
    pub ceiling: u64,
    pub cooldown_seconds: u64,
    pub idle_seconds: u64,
    pub join_timeout_seconds: u64,
    /// Supplied by operator scheduling; there is no built-in working-hours policy.
    pub launch_allowed: bool,
    pub protected_nodes: Vec<String>,
    pub presets: Vec<Preset>,
    /// None means autonomous replacement liability has no proven finite bound.
    pub replacement_headroom: Option<u64>,
}
#[derive(Clone, Debug)]
pub(crate) struct Queued {
    pub id: String,
    pub image: String,
    pub shape: Shape,
    pub compatibility: Option<Value>,
    pub compatibility_known: bool,
}
#[derive(Clone, Debug)]
pub(crate) struct Runner {
    pub candidate: placement::Candidate,
    pub total: Shape,
    pub drain_safe: bool,
    pub soft_draining: bool,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct State {
    pub tick: Option<u64>,
    pub sequence: u64,
    pub cooldown_until: u64,
    pub idle_since: Option<u64>,
    pub action: Option<Action>,
}
impl State {
    fn discard_expired_shadow(&mut self, now: u64) {
        if self.tick.is_none_or(|at| now > at)
            && now >= self.cooldown_until
            && self
                .action
                .as_ref()
                .is_some_and(|a| matches!(a.phase, Phase::ShadowLaunch | Phase::ShadowDrain))
        {
            self.action = None;
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    ShadowLaunch,
    ShadowDrain,
    Launching,
    Joining,
    Draining,
    Uncertain,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActionKind {
    Launch,
    Drain,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Action {
    pub kind: ActionKind,
    pub id: String,
    pub target: String,
    pub phase: Phase,
    pub deadline: u64,
    pub reservation: limits::Liability,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Pending,
    Unknown,
    /// Positive provider reconciliation proves the attempt has no remaining effects.
    Absent,
    Joined,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    pub action: String,
    pub outcome: Outcome,
    pub node: Option<String>,
}
pub(crate) struct Input {
    pub now: u64,
    pub inventory_fresh: bool,
    pub retention_ok: bool,
    pub outcome_resolved: bool,
    pub durability_healthy: bool,
    pub liabilities: limits::Liability,
    pub pool_instances: u64,
    pub ready_instances: u64,
    pub runners: Vec<Runner>,
    pub queued: Vec<Queued>,
    pub receipt: Option<Receipt>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Plan {
    pub state: State,
    pub decision: String,
    pub reason: String,
    pub queued: Vec<String>,
    pub live_blocked: String,
}
fn fresh(at: u64, now: u64) -> bool {
    at <= now && now - at <= 15
}
fn fits(runner: &placement::Candidate, q: &Queued, now: u64) -> bool {
    q.compatibility_known
        && runner.capabilities["managed_restore_v1"] == true
        && placement::choose(
            std::slice::from_ref(runner),
            &placement::Demand {
                requested: None,
                pool: &runner.pool,
                memory_mib: q.shape.memory_mib,
                cpus: q.shape.cpus,
                disk_bytes: q.shape.disk_bytes,
                image: &q.image,
                image_digest: None,
                parent_digest: None,
                capture_digest: None,
                compatibility: Some(
                    q.compatibility
                        .as_ref()
                        .unwrap_or(&runner.capabilities["compat"]),
                ),
                managed: true,
                disk_sizing: true,
                now,
            },
        )
        .is_some()
}
fn preset_runner(pool: &Pool, p: &Preset, now: u64) -> placement::Candidate {
    let mut capabilities = p.capabilities.clone();
    capabilities["disk_observed_unix_s"] = json!(now);
    placement::Candidate {
        id: p.name.clone(),
        online: true,
        active: true,
        pool: pool.name.clone(),
        capabilities,
        free: p.capacity.capacity(),
        joined: 0,
    }
}
fn validate(p: &Pool) -> Result<()> {
    common::identifier(&p.name)?;
    ensure!(
        p.floor <= p.ceiling && p.ceiling <= 10000,
        "invalid scaler count bounds"
    );
    ensure!(
        p.join_timeout_seconds > 0
            && p.join_timeout_seconds <= 86400
            && p.cooldown_seconds <= 604800
            && p.idle_seconds <= 604800,
        "invalid scaler timers"
    );
    ensure!(
        p.replacement_headroom.is_none_or(|n| n <= 10000),
        "replacement bound out of range"
    );
    ensure!(
        !p.presets.is_empty() && p.presets.len() <= 32,
        "1–32 presets required"
    );
    let mut names = BTreeSet::new();
    for preset in &p.presets {
        common::identifier(&preset.name)?;
        ensure!(
            names.insert(&preset.name) && preset.capacity.valid(),
            "duplicate preset or invalid capacity"
        );
        ensure!(
            preset.cost.instances == 1 && preset.cost.disk_bytes >= preset.capacity.disk_bytes,
            "preset must reserve one instance and its full logical disk"
        );
        common::placement_capable(&preset.capabilities)?;
        common::guest_limits(&preset.capabilities)?;
        ensure!(
            preset.capabilities["backend"] == "firecracker"
                && preset.capabilities["compat"].is_object()
                && ["runtime", "cpu", "kernel_sha256"].iter().all(|key| {
                    preset.capabilities["compat"][key]
                        .as_str()
                        .is_some_and(|s| !s.is_empty())
                }),
            "preset runtime compatibility must be explicit"
        );
        ensure!(
            preset.capabilities["drain"] != true && preset.capabilities["controller_drain"] != true,
            "preset cannot advertise drain"
        );
    }
    Ok(())
}
/// Deterministic single-pool iteration. The result is intent, never permission to call a provider.
pub(crate) fn plan(p: &Pool, old: &State, input: &Input, caps: limits::Liability) -> Result<Plan> {
    validate(p)?;
    let mut result = Plan { state: old.clone(), decision: "wait".into(), reason: String::new(),
        queued: input.queued.iter().map(|q|q.id.clone()).collect(),
        live_blocked: "Shadow only: no provider executor; replacement-controller recovery and provider cost/retention enforcement have not been accepted.".into() };
    macro_rules! done {
        ($why:expr) => {{
            result.reason = $why.into();
            return Ok(result);
        }};
    }
    if old.tick.is_some_and(|at| input.now <= at) {
        done!("duplicate or backward tick");
    }
    result.state.discard_expired_shadow(input.now);
    result.state.tick = Some(input.now);
    if !input.queued.is_empty() {
        result.state.idle_since = None;
    }
    // Demand cancels only our soft intent, never a sealed or effectful drain.
    if !input.queued.is_empty()
        && result
            .state
            .action
            .as_ref()
            .is_some_and(|a| a.phase == Phase::ShadowDrain)
    {
        result.state.action = None;
        result.state.cooldown_until = input.now.saturating_add(p.cooldown_seconds);
        result.decision = "cancel_drain".into();
        done!("demand returned; cancel shadow soft drain");
    }
    let held = result
        .state
        .action
        .as_ref()
        .map(|a| a.reservation)
        .unwrap_or_default();
    if !input.inventory_fresh {
        done!("provider inventory missing, incomplete or stale; actions disabled");
    }
    if !input.retention_ok {
        done!("retention violation; actions disabled");
    }
    if !input.outcome_resolved {
        done!("unresolved provider outcome; actions disabled");
    }
    if let Some(action) = result.state.action.as_mut() {
        if matches!(
            action.phase,
            Phase::Launching | Phase::Joining | Phase::Uncertain | Phase::Draining
        ) {
            if let Some(receipt) = &input.receipt {
                if receipt.action == action.id {
                    match receipt.outcome {
                        Outcome::Absent => {
                            result.state.action = None;
                            result.state.cooldown_until =
                                input.now.saturating_add(p.cooldown_seconds);
                            done!("provider positively reconciled absent; reservation released");
                        }
                        Outcome::Joined if action.kind == ActionKind::Launch => {
                            let preset = p.presets.iter().find(|v| v.name == action.target);
                            let joined = input
                                .runners
                                .iter()
                                .find(|r| Some(&r.candidate.id) == receipt.node.as_ref());
                            if let (Some(preset), Some(r)) = (preset, joined) {
                                if r.candidate.online
                                    && r.candidate.active
                                    && r.candidate.pool == p.name
                                    && r.total.cpus >= preset.capacity.cpus
                                    && r.total.memory_mib >= preset.capacity.memory_mib
                                    && r.total.slots >= preset.capacity.slots
                                    && r.total.disk_bytes >= preset.capacity.disk_bytes
                                    && common::placement_capable(&r.candidate.capabilities).is_ok()
                                    && r.candidate.capabilities["compat"]
                                        == preset.capabilities["compat"]
                                {
                                    result.state.action = None;
                                    result.state.cooldown_until =
                                        input.now.saturating_add(p.cooldown_seconds);
                                    done!(
                                        "fresh enrolled runner reconciled; liability now belongs to inventory"
                                    );
                                }
                            }
                        }
                        Outcome::Pending
                            if action.kind == ActionKind::Launch
                                && matches!(action.phase, Phase::Launching | Phase::Joining) =>
                        {
                            action.phase = Phase::Joining;
                        }
                        Outcome::Unknown => {
                            action.phase = Phase::Uncertain;
                        }
                        _ => {}
                    }
                }
            }
            if input.now >= action.deadline {
                action.phase = Phase::Uncertain;
            }
            done!(if action.phase == Phase::Uncertain {
                "provider outcome or join timeout unresolved; reservation held; actions disabled"
            } else {
                "one action already in flight; reservation held"
            });
        }
        if action.phase == Phase::ShadowLaunch
            && input.queued.is_empty()
            && input.ready_instances >= p.floor
        {
            result.state.action = None;
            result.state.cooldown_until = input.now.saturating_add(p.cooldown_seconds);
            done!("shadow launch no longer needed; forecast released");
        }
        if !input.liabilities.add(held)?.within(caps) || input.pool_instances > p.ceiling {
            done!("liability cap violation; actions disabled");
        }
        if action.phase == Phase::ShadowDrain
            && !input.runners.iter().any(|r| {
                r.candidate.id == action.target
                    && r.candidate.online
                    && r.candidate.active
                    && r.drain_safe
                    && !r.soft_draining
                    && r.candidate.id != "local"
                    && !p.protected_nodes.contains(&r.candidate.id)
            })
        {
            result.state.action = None;
            result.state.cooldown_until = input.now.saturating_add(p.cooldown_seconds);
            done!("shadow drain invalidated; target is no longer safe");
        }
        if !input.durability_healthy {
            done!("durability health missing, stale or overdue; actions disabled");
        }
        done!("shadow action recorded; no provider or drain effect dispatched");
    }
    if !input.liabilities.add(held)?.within(caps) {
        done!("liability cap violation; actions disabled");
    }
    if input.pool_instances > p.ceiling {
        done!("pool count cap violation; actions disabled");
    }
    if !input.durability_healthy {
        done!("durability health missing, stale or overdue; actions disabled");
    }
    if input.now < result.state.cooldown_until {
        done!("cooldown");
    }
    let mut runners: Vec<_> = input
        .runners
        .iter()
        .filter(|r| !r.soft_draining)
        .map(|r| r.candidate.clone())
        .collect();
    let mut unplaced = Vec::new();
    for q in &input.queued {
        if !q.shape.valid() || q.shape.slots != 1 || !q.compatibility_known {
            done!("unknown queued shape; actions disabled");
        }
        if let Some(r) = runners
            .iter_mut()
            .filter(|r| fits(r, q, input.now))
            .min_by_key(|r| (r.free.memory_mib - q.shape.memory_mib, r.id.clone()))
        {
            r.free.cpus -= q.shape.cpus;
            r.free.memory_mib -= q.shape.memory_mib;
            r.free.slots -= 1;
            r.free.disk_bytes -= q.shape.disk_bytes;
        } else {
            unplaced.push(q);
        }
    }
    result.queued = unplaced.iter().map(|q| q.id.clone()).collect();
    let below_floor = input.ready_instances < p.floor;
    if below_floor || !unplaced.is_empty() {
        if !p.launch_allowed {
            done!("launch schedule disabled; demand remains queued");
        }
        let Some(headroom) = p.replacement_headroom else {
            done!(
                "provider autonomous replacements have no enforceable finite bound; activation blocked"
            );
        };
        let mut choices = Vec::new();
        for preset in &p.presets {
            if unplaced
                .first()
                .is_some_and(|q| !fits(&preset_runner(p, preset, input.now), q, input.now))
            {
                continue;
            }
            let reserve = preset
                .cost
                .multiply(headroom.checked_add(1).context("replacement overflow")?)?;
            let next = input.liabilities.add(reserve)?;
            if input
                .pool_instances
                .checked_add(reserve.instances)
                .is_some_and(|n| n <= p.ceiling)
                && next.within(caps)
            {
                choices.push((preset, reserve));
            }
        }
        let Some((preset, reservation)) = choices
            .into_iter()
            .min_by_key(|(p, _)| (p.cost.cents, p.name.clone()))
        else {
            done!("no exact-shape preset within count/cost/disk caps; demand remains queued");
        };
        result.state.sequence = old
            .sequence
            .checked_add(1)
            .context("action sequence exhausted")?;
        result.state.action = Some(Action {
            kind: ActionKind::Launch,
            id: format!("{}-{}", p.name, result.state.sequence),
            target: preset.name.clone(),
            phase: Phase::ShadowLaunch,
            deadline: input.now.saturating_add(p.join_timeout_seconds),
            reservation,
        });
        result.state.cooldown_until = input.now.saturating_add(p.cooldown_seconds);
        result.decision = "launch".into();
        done!(if below_floor {
            "count floor; maximum new liability reserved in shadow"
        } else {
            "exact queued shape needs a runner; maximum new liability reserved in shadow"
        });
    }
    // Even already-placeable demand cancels/prevents shrink: it is not idle capacity.
    if !input.queued.is_empty() {
        done!("queued shapes fit current individual runners; no launch or drain");
    }
    let idle = *result.state.idle_since.get_or_insert(input.now);
    if input.now.saturating_sub(idle) < p.idle_seconds {
        done!("idle hysteresis");
    }
    if input.ready_instances <= p.floor {
        done!("count floor protects capacity");
    }
    let target = input
        .runners
        .iter()
        .filter(|r| {
            r.candidate.online
                && r.candidate.active
                && r.drain_safe
                && !r.soft_draining
                && r.candidate.pool == p.name
                && r.candidate.id != "local"
                && !p.protected_nodes.contains(&r.candidate.id)
        })
        .min_by_key(|r| (&r.candidate.joined, &r.candidate.id));
    let Some(target) = target else {
        done!("no safe drain candidate; running, pinned, base and unknown boxes are protected");
    };
    result.state.sequence = old
        .sequence
        .checked_add(1)
        .context("action sequence exhausted")?;
    result.state.action = Some(Action {
        kind: ActionKind::Drain,
        id: format!("{}-{}", p.name, result.state.sequence),
        target: target.candidate.id.clone(),
        phase: Phase::ShadowDrain,
        deadline: input.now.saturating_add(p.join_timeout_seconds),
        reservation: limits::Liability::default(),
    });
    result.state.cooldown_until = input.now.saturating_add(p.cooldown_seconds);
    result.decision = "drain".into();
    done!(
        "idle surplus; shadow soft drain only; sealed empty and retained-disk proof still required"
    );
}

pub(crate) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS scaler_health(id INTEGER PRIMARY KEY CHECK(id=1),error TEXT);
      CREATE TABLE IF NOT EXISTS scaler_state(pool TEXT PRIMARY KEY,state TEXT NOT NULL CHECK(json_valid(state)),plan TEXT NOT NULL CHECK(json_valid(plan)));
      CREATE TABLE IF NOT EXISTS scaler_actions(pool TEXT NOT NULL,sequence INTEGER NOT NULL,action TEXT NOT NULL CHECK(json_valid(action)),plan TEXT NOT NULL CHECK(json_valid(plan)),PRIMARY KEY(pool,sequence));")?;
    Ok(())
}
pub(crate) fn status(db: &Connection) -> Result<Value> {
    let rows = db
        .prepare("SELECT plan FROM scaler_state ORDER BY pool")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let error: Option<String> = db
        .query_row("SELECT error FROM scaler_health WHERE id=1", [], |r| {
            r.get(0)
        })
        .optional()?
        .flatten();
    let now = ow_core::recovery::now();
    let mut pools = rows
        .into_iter()
        .map(|s| serde_json::from_str::<Value>(&s))
        .collect::<serde_json::Result<Vec<_>>>()?;
    for p in &mut pools {
        p["stale"] = json!(!p["state"]["tick"].as_u64().is_some_and(|at| fresh(at, now)));
    }
    Ok(json!({"mode":"shadow","error":error,"pools":pools}))
}
fn state(db: &Connection, pool: &str) -> Result<State> {
    db.query_row(
        "SELECT state FROM scaler_state WHERE pool=?1",
        [pool],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|s| serde_json::from_str(&s))
    .transpose()
    .map(|s| s.unwrap_or_default())
    .map_err(Into::into)
}
/// Caller holds BEGIN IMMEDIATE. Forecasts across pools serialize with the journal.
fn record(db: &Connection, p: &Pool, input: &Input, caps: limits::Liability) -> Result<Plan> {
    let old = state(db, &p.name)?;
    let mut adjusted = Input {
        now: input.now,
        inventory_fresh: input.inventory_fresh,
        retention_ok: input.retention_ok,
        outcome_resolved: input.outcome_resolved,
        durability_healthy: input.durability_healthy,
        liabilities: input.liabilities,
        pool_instances: input.pool_instances,
        ready_instances: input.ready_instances,
        runners: input.runners.clone(),
        queued: input.queued.clone(),
        receipt: input.receipt.clone(),
    };
    let others: Vec<String> = db
        .prepare("SELECT state FROM scaler_state WHERE pool<>?1")?
        .query_map([&p.name], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for s in others {
        let mut other: State = serde_json::from_str(&s)?;
        other.discard_expired_shadow(input.now);
        if let Some(a) = other.action {
            adjusted.liabilities = adjusted.liabilities.add(a.reservation)?;
            if a.phase == Phase::Uncertain {
                adjusted.outcome_resolved = false;
            }
        }
    }
    let result = plan(p, &old, &adjusted, caps)?;
    if old.tick.is_some_and(|at| input.now <= at) {
        let stored: String = db.query_row(
            "SELECT plan FROM scaler_state WHERE pool=?1",
            [&p.name],
            |r| r.get(0),
        )?;
        return Ok(serde_json::from_str(&stored)?);
    }
    let raw = serde_json::to_string(&result)?;
    db.execute("INSERT INTO scaler_state VALUES(?1,?2,?3) ON CONFLICT(pool) DO UPDATE SET state=?2,plan=?3",params![p.name,serde_json::to_string(&result.state)?,raw])?;
    if let Some(a) = result.state.action.as_ref().or(old.action.as_ref()) {
        db.execute("INSERT INTO scaler_actions VALUES(?1,?2,?3,?4) ON CONFLICT(pool,sequence) DO UPDATE SET action=?3,plan=?4",params![p.name,i64::try_from(result.state.sequence)?,serde_json::to_string(a)?,raw])?;
    }
    Ok(result)
}

#[cfg(target_os = "linux")]
#[path = "scaler_controller.rs"]
mod controller;
#[cfg(target_os = "linux")]
pub(crate) use controller::{command, start};
#[cfg(test)]
mod tests;
