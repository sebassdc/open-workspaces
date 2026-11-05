//! GitHub Actions runner pools (docs/ACTIONS.md): the `actions.pools` setting and the
//! planner that decides, from what the pool holds and what GitHub lists, which boxes to
//! make and which to remove. The controller side that acts on it is `actions_pool`.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

/// The setting: a JSON list of pools.
pub(crate) const POOLS: &str = "actions.pools";
/// The GitHub App the pools mint runner registrations with (0: none).
pub(crate) const APP_ID: &str = "actions.github_app_id";

/// A runner that was started but never came online by then failed to start.
pub(crate) const OFFLINE_S: i64 = 5 * 60;
/// A job running longer than this is cut off and its box removed.
pub(crate) const LIFETIME_S: i64 = 3 * 3600;
/// An idle runner this old is replaced by a fresh one.
pub(crate) const IDLE_MAX_S: i64 = 6 * 3600;
/// A fresh registration may not be listed yet.
pub(crate) const GONE_GRACE_S: i64 = 60;
/// Creating, minting and starting together take at most this long.
pub(crate) const LAUNCH_TIMEOUT_S: i64 = 10 * 60;
/// A box spilled onto another pool's runner for a queued job that has no job after being
/// online this long is removed: spilled boxes are never kept warm (docs/MULTINODE.md#spill).
pub(crate) const SPILL_IDLE_S: i64 = 60;

const DEFAULT_LABELS: [&str; 3] = ["self-hosted", "linux", "x64"];

fn default_memory() -> u32 {
    4096
}
fn default_cpus() -> u32 {
    2
}

/// One pool: `warm` idle runners for `repo` with `labels`, at most `max` boxes, made with the
/// workload token `token` (its owner's limits and its placement pool apply).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pool {
    pub name: String,
    pub repo: String,
    pub labels: Vec<String>,
    pub token: String,
    pub image: String,
    #[serde(default = "default_memory")]
    pub memory_mib: u32,
    #[serde(default = "default_cpus")]
    pub vcpu_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gib: Option<u32>,
    pub warm: u32,
    pub max: u32,
}

impl Pool {
    /// What a box's shape depends on: an idle box with another one is replaced.
    pub(crate) fn spec(&self) -> String {
        format!(
            "{}|{}|{}|{}|{:?}|{}",
            self.repo,
            self.labels.join(","),
            self.image,
            self.memory_mib,
            self.disk_gib,
            self.vcpu_count
        )
    }
}

fn identifier(what: &str, value: &str, max: usize) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= max
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !value.starts_with('-'),
        "{what} is 1 to {max} lowercase letters, digits or '-'"
    );
    Ok(())
}

/// Parses and checks the setting's JSON text ("" or "[]": no pools).
pub(crate) fn parse(text: &str) -> Result<Vec<Pool>> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let pools: Vec<Pool> = serde_json::from_str(text).context(
        "expected a list of {\"name\", \"repo\", \"labels\", \"token\", \"image\", \"warm\", \"max\"} (optional memory_mib, vcpu_count, disk_gib)",
    )?;
    ensure!(pools.len() <= 16, "at most 16 pools");
    let mut names = std::collections::HashSet::new();
    for pool in &pools {
        // Its boxes are named NAME-<12 hex>, within a box name's 32 characters.
        identifier("name", &pool.name, 16)?;
        ensure!(
            names.insert(&pool.name),
            "pool {} is listed twice",
            pool.name
        );
        let (owner, repo) = pool.repo.split_once('/').unwrap_or_default();
        let part = |s: &str, extra: &[u8]| {
            !s.is_empty()
                && s.len() <= 100
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || extra.contains(&b))
        };
        ensure!(
            part(owner, b"") && part(repo, b"_."),
            "{}: repo is OWNER/NAME",
            pool.name
        );
        ensure!(
            !pool.labels.is_empty() && pool.labels.len() <= 16,
            "{}: 1 to 16 labels",
            pool.name
        );
        for label in &pool.labels {
            ensure!(
                !label.is_empty()
                    && label.len() <= 64
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
                "{}: a label is 1 to 64 letters, digits, '-', '_' or '.'",
                pool.name
            );
        }
        // Only jobs that ask for one of the pool's own labels may land on its runners.
        ensure!(
            pool.labels
                .iter()
                .any(|l| !DEFAULT_LABELS.contains(&l.to_ascii_lowercase().as_str())),
            "{}: labels need one beyond self-hosted, Linux and X64",
            pool.name
        );
        identifier("token", &pool.token, 64)?;
        identifier("image", &pool.image, 64)?;
        crate::common::resources(pool.memory_mib, pool.vcpu_count)
            .with_context(|| format!("{}: memory_mib and vcpu_count", pool.name))?;
        if let Some(disk) = pool.disk_gib {
            ensure!(
                (1..=1024).contains(&disk),
                "{}: disk_gib is 1 to 1024",
                pool.name
            );
        }
        ensure!(
            pool.max <= 64 && pool.warm <= pool.max,
            "{}: warm <= max <= 64",
            pool.name
        );
    }
    Ok(pools)
}

/// The setting's typed form, for `config.set` and `config.get`.
pub(crate) fn typed(text: &str) -> Result<Value> {
    Ok(serde_json::to_value(parse(text)?)?)
}

/// `config.set`'s JSON value as the setting's text.
pub(crate) fn from_json(value: &Value) -> Result<String> {
    match value {
        Value::Array(_) => {
            let text = serde_json::to_string(value)?;
            parse(&text)?;
            Ok(text)
        }
        _ => bail!("expected a list of pools"),
    }
}

/// Where a box is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    /// Being created, registered and started (one thread).
    Launching,
    /// Its runner was started; idle or running a job.
    Started,
    /// Being unregistered, stopped and deleted (one thread).
    Removing,
}

/// One box the pools hold.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub pool: String,
    /// The workload token it was made with (its pool's may change or go).
    #[serde(default)]
    pub token: String,
    pub spec: String,
    pub phase: Phase,
    pub runner_id: Option<u64>,
    pub created: i64,
    /// When its runner was started (box ready, registration in place).
    pub started: Option<i64>,
    /// First time GitHub listed it online.
    pub online: Option<i64>,
    /// First time GitHub listed it busy (a job was assigned).
    pub busy: Option<i64>,
    /// Earliest time a failed removal is tried again.
    #[serde(default)]
    pub retry_at: i64,
    /// Its create succeeded, so a machine may be on a runner (entries saved before this
    /// field existed count as made).
    #[serde(default = "made")]
    pub made: bool,
    /// Its create spilled onto a runner of another pool.
    #[serde(default)]
    pub spilled: bool,
}

fn made() -> bool {
    true
}

/// What GitHub lists for one runner.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct Runner {
    pub id: u64,
    pub status: String,
    pub busy: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Make this many boxes for the pool: warm ones only on its own runners (`spill` false),
    /// ones for queued jobs wherever placement finds room, spill included.
    Launch {
        pool: String,
        count: u32,
        spill: bool,
    },
    /// Remove a box. Without `force`, a runner GitHub refuses to unregister (it just took a
    /// job) keeps its box.
    Remove {
        name: String,
        why: &'static str,
        force: bool,
    },
}

/// Updates entries from GitHub's list: online and busy times.
pub(crate) fn observe(
    now: i64,
    entries: &mut BTreeMap<String, Entry>,
    runners: &HashMap<String, Runner>,
) {
    for (name, e) in entries.iter_mut() {
        if let Some(r) = runners.get(name) {
            if e.online.is_none() && (r.status == "online" || r.busy) {
                e.online = Some(now);
            }
            if e.busy.is_none() && r.busy {
                e.busy = Some(now);
            }
        }
    }
}

/// Whether every box of the pool is busy (none launching or idle) and it is under `max`:
/// only then can a job be waiting for a runner, so only then are queued jobs worth asking for.
pub(crate) fn saturated(pool: &Pool, entries: &BTreeMap<String, Entry>) -> bool {
    let mine = || entries.values().filter(|e| e.pool == pool.name);
    let total = mine()
        .filter(|e| e.phase != Phase::Removing || e.made)
        .count() as u32;
    total < pool.max
        && !mine()
            .any(|e| e.phase == Phase::Launching || (e.phase == Phase::Started && e.busy.is_none()))
}

/// The actions for this pass, for one repository's pools and boxes. `runners` is None when
/// GitHub could not be read: then only limits that need no GitHub answer apply. `held(pool)`
/// pools make no new boxes this pass, `held("POOL/home")` no warm ones; boxes named in
/// `draining` are on a runner being drained; `queued(pool)` jobs wait for one of its runners.
pub(crate) fn plan(
    now: i64,
    pools: &[Pool],
    entries: &BTreeMap<String, Entry>,
    runners: Option<&HashMap<String, Runner>>,
    held: &dyn Fn(&str) -> bool,
    draining: &HashSet<String>,
    queued: &dyn Fn(&str) -> u32,
) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut removing = std::collections::HashSet::new();
    let pool = |name: &str| pools.iter().find(|p| p.name == name);
    for (name, e) in entries {
        let remove = |why: &'static str, force: bool| Action::Remove {
            name: name.clone(),
            why,
            force,
        };
        let age = now - e.created;
        let action = match e.phase {
            Phase::Removing => (now >= e.retry_at).then(|| remove("removal retried", true)),
            Phase::Launching => {
                (age > LAUNCH_TIMEOUT_S).then(|| remove("its start timed out", true))
            }
            Phase::Started if e.busy.is_some_and(|b| now - b > LIFETIME_S) => {
                Some(remove("its job ran over the time limit", true))
            }
            // Never longer than an idle life and a job, whatever GitHub says or cannot say.
            Phase::Started if age > IDLE_MAX_S + LIFETIME_S => {
                Some(remove("over the lifetime limit", true))
            }
            Phase::Started => match runners {
                None => None,
                Some(runners) => {
                    let started = e.started.unwrap_or(e.created);
                    match runners.get(name) {
                        // GitHub drops a single-use runner once its job ends.
                        None if e.busy.is_some() || e.online.is_some() => {
                            Some(remove("its job ended", true))
                        }
                        None if now - started > GONE_GRACE_S => {
                            Some(remove("its runner never registered", true))
                        }
                        None => None,
                        Some(r) if r.busy => None,
                        Some(r) if e.busy.is_some() && r.status == "offline" => {
                            Some(remove("its job ended", true))
                        }
                        Some(r) if r.status != "online" && now - started > OFFLINE_S => {
                            Some(remove("its runner did not come online", false))
                        }
                        Some(_) if e.busy.is_some() => None,
                        // Its runner can only finish draining once the idle box is gone.
                        Some(_) if draining.contains(name) => {
                            Some(remove("its runner is draining", false))
                        }
                        // Made for a queued job that another runner took: never kept warm.
                        Some(_)
                            if e.spilled && e.online.is_some_and(|o| now - o > SPILL_IDLE_S) =>
                        {
                            Some(remove("spilled and idle", false))
                        }
                        Some(_) => match pool(&e.pool) {
                            None => Some(remove("its pool was removed", false)),
                            Some(p) if p.spec() != e.spec => {
                                Some(remove("its pool's settings changed", false))
                            }
                            Some(_) if now - e.started.unwrap_or(e.created) > IDLE_MAX_S => {
                                Some(remove("idle too long", false))
                            }
                            Some(_) => None,
                        },
                    }
                }
            },
        };
        if let Some(action) = action {
            removing.insert(name.clone());
            actions.push(action);
        }
    }
    // New boxes need a registration from GitHub: none while it cannot be read.
    for p in pools.iter().filter(|_| runners.is_some()) {
        if held(&p.name) {
            continue;
        }
        let mine = entries.iter().filter(|(_, e)| e.pool == p.name);
        // A box whose create failed holds no machine (an uncertain one may, and the runner's
        // capacity check then refuses what would not fit); its removal can be refused until
        // the creation timeout, so while it goes it does not count against max.
        let total = mine
            .clone()
            .filter(|(_, e)| e.phase != Phase::Removing || e.made)
            .count() as u32;
        // Idle: launching, or started with no job yet, and not about to go.
        let idle: Vec<&Entry> = mine
            .filter(|(n, e)| {
                !removing.contains(*n)
                    && match e.phase {
                        Phase::Launching => true,
                        Phase::Started => e.busy.is_none(),
                        Phase::Removing => false,
                    }
            })
            .map(|(_, e)| e)
            .collect();
        // Warm runners are kept on the pool's own runners; a spilled box is not one.
        let home_idle = idle.iter().filter(|e| !e.spilled).count() as u32;
        let room = p.max.saturating_sub(total);
        let warm = if held(&format!("{}/home", p.name)) {
            0
        } else {
            p.warm.saturating_sub(home_idle).min(room)
        };
        // Jobs no idle runner will take: boxes for them may spill.
        let demand = queued(&p.name)
            .saturating_sub(idle.len() as u32 + warm)
            .min(room - warm);
        for (count, spill) in [(warm, false), (demand, true)] {
            if count > 0 {
                actions.push(Action::Launch {
                    pool: p.name.clone(),
                    count,
                    spill,
                });
            }
        }
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(warm: u32, max: u32) -> Pool {
        parse(&format!(
            r#"[{{"name":"ci","repo":"acme/app","labels":["self-hosted","Linux","X64","app-ci"],"token":"ci-runners","image":"ci-runner","warm":{warm},"max":{max}}}]"#
        ))
        .unwrap()
        .remove(0)
    }

    fn entry(p: &Pool, phase: Phase, created: i64) -> Entry {
        Entry {
            pool: p.name.clone(),
            token: p.token.clone(),
            spec: p.spec(),
            phase,
            runner_id: Some(1),
            created,
            started: Some(created),
            online: None,
            busy: None,
            retry_at: 0,
            made: true,
            spilled: false,
        }
    }

    fn online(busy: bool) -> Runner {
        Runner {
            id: 1,
            status: "online".into(),
            busy,
        }
    }

    #[test]
    fn the_setting_is_checked() {
        let p = pool(2, 3);
        assert_eq!((p.memory_mib, p.vcpu_count, p.disk_gib), (4096, 2, None));
        assert!(parse("").unwrap().is_empty());
        assert!(parse("[]").unwrap().is_empty());
        for (bad, why) in [
            (
                r#"[{"name":"ci","repo":"acme/app","labels":["self-hosted","Linux","X64"],"token":"t","image":"i","warm":1,"max":1}]"#,
                "beyond",
            ),
            (
                r#"[{"name":"ci","repo":"acme","labels":["x"],"token":"t","image":"i","warm":1,"max":1}]"#,
                "OWNER/NAME",
            ),
            (
                r#"[{"name":"ci","repo":"acme/app","labels":["x"],"token":"t","image":"i","warm":3,"max":2}]"#,
                "warm <= max",
            ),
            (
                r#"[{"name":"CI","repo":"acme/app","labels":["x"],"token":"t","image":"i","warm":1,"max":1}]"#,
                "lowercase",
            ),
            (
                r#"[{"name":"ci","repo":"acme/app","labels":["x"],"token":"t","image":"i","warm":1,"max":1,"extra":1}]"#,
                "expected a list",
            ),
        ] {
            let error = format!("{:#}", parse(bad).unwrap_err());
            assert!(error.contains(why), "{bad}: {error}");
        }
        let twice = format!("[{0},{0}]", serde_json::to_string(&pool(1, 1)).unwrap());
        assert!(format!("{:#}", parse(&twice).unwrap_err()).contains("twice"));
        // The typed value round-trips through config.set's JSON form.
        let typed = typed(&twice.replacen("\"ci\"", "\"cj\"", 1)).unwrap();
        assert_eq!(parse(&from_json(&typed).unwrap()).unwrap().len(), 2);
    }

    #[test]
    fn an_empty_pool_fills_to_warm_within_max() {
        let p = pool(11, 11);
        let none = BTreeMap::new();
        let runners = HashMap::new();
        assert_eq!(
            plan(
                0,
                &[p.clone()],
                &none,
                Some(&runners),
                &|_| false,
                &HashSet::new(),
                &|_| 0
            ),
            vec![Action::Launch {
                pool: "ci".into(),
                count: 11,
                spill: false
            }]
        );
        // Busy boxes count against max, not against warm.
        let mut held = BTreeMap::new();
        let mut runners = HashMap::new();
        for i in 0..8 {
            let mut e = entry(&p, Phase::Started, 0);
            e.busy = Some(1);
            held.insert(format!("b{i}"), e);
            runners.insert(format!("b{i}"), online(true));
        }
        assert_eq!(
            plan(
                10,
                &[p.clone()],
                &held,
                Some(&runners),
                &|_| false,
                &HashSet::new(),
                &|_| 0
            ),
            vec![Action::Launch {
                pool: "ci".into(),
                count: 3,
                spill: false
            }]
        );
        // A held pool (its node is full) makes nothing until the hold ends.
        assert!(
            plan(
                10,
                &[p],
                &held,
                Some(&runners),
                &|_| true,
                &HashSet::new(),
                &|_| 0
            )
            .is_empty()
        );
    }

    #[test]
    fn a_box_goes_the_moment_its_job_ends_and_is_replaced() {
        let p = pool(1, 2);
        let mut entries = BTreeMap::new();
        let mut e = entry(&p, Phase::Started, 0);
        e.online = Some(5);
        e.busy = Some(9);
        entries.insert("a".to_string(), e);
        let gone = HashMap::new();
        assert_eq!(
            plan(
                20,
                &[p],
                &entries,
                Some(&gone),
                &|_| false,
                &HashSet::new(),
                &|_| 0
            ),
            vec![
                Action::Remove {
                    name: "a".into(),
                    why: "its job ended",
                    force: true
                },
                Action::Launch {
                    pool: "ci".into(),
                    count: 1,
                    spill: false
                },
            ]
        );
    }

    #[test]
    fn idle_boxes_of_an_old_shape_are_replaced_but_busy_ones_finish() {
        let p = pool(2, 4);
        let mut entries = BTreeMap::new();
        let mut idle = entry(&p, Phase::Started, 0);
        idle.spec = "old".into();
        let mut busy = idle.clone();
        busy.busy = Some(3);
        entries.insert("idle".to_string(), idle);
        entries.insert("busy".to_string(), busy);
        let runners = HashMap::from([
            ("idle".to_string(), online(false)),
            ("busy".to_string(), online(true)),
        ]);
        assert_eq!(
            plan(
                10,
                &[p],
                &entries,
                Some(&runners),
                &|_| false,
                &HashSet::new(),
                &|_| 0
            ),
            vec![
                Action::Remove {
                    name: "idle".into(),
                    why: "its pool's settings changed",
                    force: false
                },
                Action::Launch {
                    pool: "ci".into(),
                    count: 2,
                    spill: false
                },
            ]
        );
    }

    #[test]
    fn without_github_only_limits_apply_nothing_is_made_and_failed_removals_retry() {
        let p = pool(3, 4);
        let cap = IDLE_MAX_S + LIFETIME_S;
        let mut entries = BTreeMap::new();
        entries.insert("old".to_string(), entry(&p, Phase::Started, 0));
        let mut long_job = entry(&p, Phase::Started, cap);
        long_job.busy = Some(cap);
        entries.insert("long".to_string(), long_job);
        let mut stuck = entry(&p, Phase::Removing, 0);
        stuck.retry_at = cap + 5;
        entries.insert("stuck".to_string(), stuck);
        // No launch: a new box needs a registration from GitHub.
        assert_eq!(
            plan(
                cap + 1,
                &[p.clone()],
                &entries,
                None,
                &|_| false,
                &HashSet::new(),
                &|_| 0
            ),
            vec![Action::Remove {
                name: "old".into(),
                why: "over the lifetime limit",
                force: true
            }]
        );
        let later = plan(
            cap + LIFETIME_S + 1,
            &[p],
            &entries,
            None,
            &|_| false,
            &HashSet::new(),
            &|_| 0,
        );
        assert!(later.contains(&Action::Remove {
            name: "stuck".into(),
            why: "removal retried",
            force: true
        }));
        assert!(later.contains(&Action::Remove {
            name: "long".into(),
            why: "its job ran over the time limit",
            force: true
        }));
    }

    #[test]
    fn a_failed_create_being_removed_does_not_take_a_slot() {
        let p = pool(2, 2);
        let github = HashMap::new();
        let mut entries = BTreeMap::new();
        let mut failed = entry(&p, Phase::Removing, 0);
        failed.started = None;
        failed.made = false;
        failed.retry_at = 100;
        entries.insert("failed".to_string(), failed);
        let launch = |entries: &BTreeMap<String, Entry>| {
            plan(
                10,
                &[p.clone()],
                entries,
                Some(&github),
                &|_| false,
                &HashSet::new(),
                &|_| 0,
            )
        };
        assert_eq!(
            launch(&entries),
            vec![Action::Launch {
                pool: "ci".into(),
                count: 2,
                spill: false
            }]
        );
        // A made box (here its put or start failed) is on its runner until deleted: it keeps
        // its slot. So does an entry saved before `made` existed.
        entries.get_mut("failed").unwrap().made = true;
        assert_eq!(
            launch(&entries),
            vec![Action::Launch {
                pool: "ci".into(),
                count: 1,
                spill: false
            }]
        );
    }

    #[test]
    fn an_idle_runner_is_replaced_after_its_idle_life_and_a_long_job_is_not() {
        let p = pool(1, 2);
        let mut entries = BTreeMap::new();
        entries.insert("idle".to_string(), entry(&p, Phase::Started, 0));
        let mut busy = entry(&p, Phase::Started, 0);
        busy.busy = Some(IDLE_MAX_S - 60);
        entries.insert("busy".to_string(), busy);
        let runners = HashMap::from([
            ("idle".to_string(), online(false)),
            ("busy".to_string(), online(true)),
        ]);
        assert_eq!(
            plan(
                IDLE_MAX_S + 1,
                &[p],
                &entries,
                Some(&runners),
                &|_| false,
                &HashSet::new(),
                &|_| 0
            ),
            vec![Action::Remove {
                name: "idle".into(),
                why: "idle too long",
                force: false
            }]
        );
    }

    #[test]
    fn idle_boxes_on_a_draining_runner_go_and_busy_ones_finish() {
        let p = pool(0, 2);
        let mut entries = BTreeMap::new();
        entries.insert("idle".to_string(), entry(&p, Phase::Started, 0));
        let mut busy = entry(&p, Phase::Started, 0);
        busy.busy = Some(1);
        entries.insert("busy".to_string(), busy);
        let runners = HashMap::from([
            ("idle".to_string(), online(false)),
            ("busy".to_string(), online(true)),
        ]);
        let draining = HashSet::from(["idle".to_string(), "busy".to_string()]);
        assert_eq!(
            plan(
                10,
                &[p],
                &entries,
                Some(&runners),
                &|_| false,
                &draining,
                &|_| 0
            ),
            vec![Action::Remove {
                name: "idle".into(),
                why: "its runner is draining",
                force: false
            }]
        );
    }

    #[test]
    fn a_runner_that_never_registers_or_comes_online_goes() {
        let p = pool(0, 2);
        let mut entries = BTreeMap::new();
        entries.insert("never".to_string(), entry(&p, Phase::Started, 0));
        entries.insert("offline".to_string(), entry(&p, Phase::Started, 0));
        let runners = HashMap::from([(
            "offline".to_string(),
            Runner {
                id: 2,
                status: "offline".into(),
                busy: false,
            },
        )]);
        assert!(
            plan(
                30,
                &[p.clone()],
                &entries,
                Some(&runners),
                &|_| false,
                &HashSet::new(),
                &|_| 0
            )
            .is_empty()
        );
        assert_eq!(
            plan(
                OFFLINE_S + 1,
                &[p],
                &entries,
                Some(&runners),
                &|_| false,
                &HashSet::new(),
                &|_| 0
            ),
            vec![
                Action::Remove {
                    name: "never".into(),
                    why: "its runner never registered",
                    force: true
                },
                Action::Remove {
                    name: "offline".into(),
                    why: "its runner did not come online",
                    force: false
                },
            ]
        );
    }

    #[test]
    fn an_entry_saved_before_made_existed_counts_as_made() {
        let p = pool(1, 1);
        let mut saved = serde_json::to_value(entry(&p, Phase::Removing, 0)).unwrap();
        saved.as_object_mut().unwrap().remove("made");
        assert!(serde_json::from_value::<Entry>(saved).unwrap().made);
    }

    #[test]
    fn warm_boxes_stay_home_and_only_queued_jobs_get_boxes_that_may_spill() {
        let p = pool(2, 5);
        let mut entries = BTreeMap::new();
        let mut runners = HashMap::new();
        for i in 0..2 {
            let mut e = entry(&p, Phase::Started, 0);
            e.busy = Some(1);
            entries.insert(format!("b{i}"), e);
            runners.insert(format!("b{i}"), online(true));
        }
        assert!(saturated(&p, &entries));
        let none = HashSet::new();
        let at = |queued: u32,
                  home_held: bool,
                  entries: &BTreeMap<String, Entry>,
                  runners: &HashMap<String, Runner>| {
            plan(
                10,
                &[p.clone()],
                entries,
                Some(runners),
                &|k| home_held && k == "ci/home",
                &none,
                &|_| queued,
            )
        };
        // Nothing queued: warm boxes only, and they ask not to spill.
        assert_eq!(
            at(0, false, &entries, &runners),
            vec![Action::Launch {
                pool: "ci".into(),
                count: 2,
                spill: false
            }]
        );
        // Three jobs wait: the two warm boxes will take two, the third gets a box that may spill.
        assert_eq!(
            at(3, false, &entries, &runners),
            vec![
                Action::Launch {
                    pool: "ci".into(),
                    count: 2,
                    spill: false
                },
                Action::Launch {
                    pool: "ci".into(),
                    count: 1,
                    spill: true
                },
            ]
        );
        // The pool's own runners are full (home hold): queued jobs still get boxes, within max.
        assert_eq!(
            at(9, true, &entries, &runners),
            vec![Action::Launch {
                pool: "ci".into(),
                count: 3,
                spill: true
            }]
        );
        // A spilled idle box is not warm: warm boxes are still made at home. Once online with no
        // job for SPILL_IDLE_S it goes.
        let mut spilled = entry(&p, Phase::Started, 0);
        spilled.spilled = true;
        spilled.online = Some(5);
        entries.insert("s".into(), spilled);
        runners.insert("s".into(), online(false));
        assert!(!saturated(&p, &entries));
        assert_eq!(
            at(0, false, &entries, &runners),
            vec![Action::Launch {
                pool: "ci".into(),
                count: 2,
                spill: false
            }]
        );
        assert_eq!(
            plan(
                6 + SPILL_IDLE_S,
                &[p.clone()],
                &entries,
                Some(&runners),
                &|_| true,
                &none,
                &|_| 0
            ),
            vec![Action::Remove {
                name: "s".into(),
                why: "spilled and idle",
                force: false
            }]
        );
        // A full pool is not saturated: no box can be added.
        let full = pool(0, 2);
        assert!(!saturated(&full, &entries));
    }

    #[test]
    fn observe_records_online_and_busy_once() {
        let p = pool(1, 1);
        let mut entries = BTreeMap::from([("a".to_string(), entry(&p, Phase::Started, 0))]);
        observe(
            3,
            &mut entries,
            &HashMap::from([("a".to_string(), online(false))]),
        );
        observe(
            7,
            &mut entries,
            &HashMap::from([("a".to_string(), online(true))]),
        );
        observe(
            9,
            &mut entries,
            &HashMap::from([("a".to_string(), online(true))]),
        );
        assert_eq!((entries["a"].online, entries["a"].busy), (Some(3), Some(7)));
    }
}
