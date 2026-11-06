//! GitHub Actions runner pools on the controller (docs/ACTIONS.md). Every 2 s one thread
//! reads `actions.pools`, lists each repository's runners on GitHub (an unchanged list costs
//! no rate limit) and runs `actions::plan`. Each box it makes or removes gets its own thread,
//! so GitHub calls and waits of different boxes overlap; their box operations go through the
//! catalog like any other (one at a time, as a runner's worker runs them). A pool whose
//! launch failed (its runners are full, say) holds until one of its boxes is removed, or for
//! 10 s.
//!
//! A box: create (ephemeral, through the pool's workload token) -> a single-use runner
//! registration (JIT config) from the GitHub App -> put it in the box -> start the runner.
//! GitHub hands a queued job to an idle runner at once; the box is removed when GitHub drops
//! the runner after its job. The App key stays in a 0600 file and in memory; no key, token or
//! registration is logged or kept in the catalog.
use crate::actions::{self, Action, Entry, Phase, Pool, Runner, api_base};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const TICK: Duration = Duration::from_secs(2);
const MAX_THREADS: usize = 32;
const HOLD_S: i64 = 10;
/// A warm box found the pool's own runners full: none is tried again until one of the pool's
/// boxes is removed, or for this long.
const HOME_HOLD_S: i64 = 60;
/// Queued jobs are asked for at most this often per pool, and only while it is saturated.
const QUEUED_EVERY_S: i64 = 10;
/// The newest workflow runs read per status: a backlog beyond them is undercounted (safe:
/// fewer boxes), never overcounted. At most 2 + 2 x this calls per ask.
const QUEUED_RUNS: u32 = 10;
/// Below this rate limit left, queued jobs are not asked for.
const QUEUED_FLOOR: i64 = 1000;
const REMOVE_RETRY_S: i64 = 10;
const RECENT: usize = 50;
/// The image contract (docs/ACTIONS.md#image): the start command reads the registration here.
const JIT_PATH: &str = "/home/dev/.ow-ci/jitconfig";
const START: &str = "ow-ci-runner-start";
const GITHUB: &str = "https://api.github.com";
/// The GitHub App's private key, in the controller's data directory.
pub(crate) const KEY_FILE: &str = "runners-github-app.pem";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

struct Shared {
    root: PathBuf,
    catalog: Arc<Mutex<crate::catalog::Catalog>>,
    entries: Mutex<BTreeMap<String, Entry>>,
    /// Pool -> unix s until which it makes no box.
    holds: Mutex<HashMap<String, i64>>,
    /// Pool -> (unix s asked, jobs queued for its runners).
    queued: Mutex<HashMap<String, (i64, u32)>>,
    /// Pool -> its last launch or removal error.
    errors: Mutex<HashMap<String, (i64, String)>>,
    /// The latest boxes' start times (ms since epoch), newest last.
    recent: Mutex<VecDeque<Value>>,
    threads: AtomicUsize,
    github: GitHub,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

/// Starts the pools' thread (once per process); with no pool configured it only reads the
/// setting every pass.
pub(crate) fn start(root: &Path, catalog: Arc<Mutex<crate::catalog::Catalog>>) {
    if SHARED.get().is_some() {
        return;
    }
    let entries = match load(root) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("actions pools: not started, state unreadable: {error:#}");
            return;
        }
    };
    let shared = Arc::new(Shared {
        root: root.to_path_buf(),
        catalog,
        entries: Mutex::new(entries),
        holds: Mutex::default(),
        queued: Mutex::default(),
        errors: Mutex::default(),
        recent: Mutex::default(),
        threads: AtomicUsize::new(0),
        github: GitHub::new(root),
    });
    if SHARED.set(shared.clone()).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("actions-pools".into())
        .spawn(move || {
            let mut last_error = Instant::now() - Duration::from_secs(3600);
            loop {
                let started = Instant::now();
                if let Err(error) = pass(&shared) {
                    // One line a minute at most: a missing key or an outage repeats every pass.
                    if last_error.elapsed() >= Duration::from_secs(60) {
                        eprintln!("actions pools: {error:#}");
                        last_error = Instant::now();
                    }
                }
                std::thread::sleep(TICK.saturating_sub(started.elapsed()));
            }
        })
        .map(|_| ())
        .unwrap_or_else(|e| eprintln!("actions pools: thread not started: {e}"));
}

// ------------------------------------------------------------------------- persistence

fn db(root: &Path) -> Result<rusqlite::Connection> {
    let db = crate::nodes::db(root)?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS runner_boxes(name TEXT PRIMARY KEY,entry TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS runner_pool_status(id INTEGER PRIMARY KEY CHECK(id=1),body TEXT NOT NULL);")?;
    Ok(db)
}

/// What a previous process left: anything it was still making or removing is removed now.
fn load(root: &Path) -> Result<BTreeMap<String, Entry>> {
    let mut out = load_raw(root)?;
    for entry in out.values_mut() {
        if entry.phase != Phase::Started {
            entry.phase = Phase::Removing;
            entry.retry_at = 0;
        }
    }
    Ok(out)
}

/// The boxes as last saved.
fn load_raw(root: &Path) -> Result<BTreeMap<String, Entry>> {
    let db = db(root)?;
    let rows: Vec<(String, String)> = db
        .prepare("SELECT name,entry FROM runner_boxes")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|(name, text)| Ok((name, serde_json::from_str(&text)?)))
        .collect()
}

fn save(shared: &Shared, name: &str, entry: Option<&Entry>) {
    let result = db(&shared.root).and_then(|db| {
        match entry {
            Some(e) => db.execute(
                "INSERT INTO runner_boxes(name,entry) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET entry=excluded.entry",
                rusqlite::params![name, serde_json::to_string(e)?],
            )?,
            None => db.execute("DELETE FROM runner_boxes WHERE name=?1", [name])?,
        };
        Ok(())
    });
    if let Err(error) = result {
        eprintln!("actions pools: saving {name}: {error:#}");
    }
}

fn update(shared: &Shared, name: &str, f: impl FnOnce(&mut Entry)) -> Option<Entry> {
    let mut entries = shared.entries.lock().unwrap();
    let entry = entries.get_mut(name)?;
    f(entry);
    let copy = entry.clone();
    drop(entries);
    save(shared, name, Some(&copy));
    Some(copy)
}

// ------------------------------------------------------------------------- the pass

fn pools(root: &Path) -> Result<(Vec<Pool>, u64)> {
    let settings = crate::telemetry::settings(&crate::nodes::db(root)?)?;
    let pools: Vec<Pool> = serde_json::from_value(settings[actions::POOLS].clone())?;
    Ok((pools, settings[actions::APP_ID].as_u64().unwrap_or(0)))
}

/// Names of the boxes on runners being drained (their idle runners must go).
fn draining(root: &Path) -> Result<std::collections::HashSet<String>> {
    let db = crate::nodes::db(root)?;
    let mut stmt = db.prepare("SELECT r.name FROM resources r JOIN runner_drains d ON d.node=r.node WHERE r.kind='machine' AND d.state IN ('soft-draining','draining')")?;
    Ok(stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn pass(shared: &Arc<Shared>) -> Result<()> {
    // Spilled boxes stopped to make room for a runner's own box (docs/MULTINODE.md#spill).
    crate::catalog::Catalog::delete_queued(&shared.catalog);
    let (pools, app_id) = pools(&shared.root)?;
    let repos: std::collections::BTreeSet<String> = {
        let entries = shared.entries.lock().unwrap();
        if pools.is_empty() && entries.is_empty() {
            publish(shared);
            return Ok(());
        }
        pools
            .iter()
            .map(|p| p.scope().to_owned())
            .chain(entries.values().map(|e| repo_of(&e.spec)))
            .collect()
    };
    let draining = draining(&shared.root).unwrap_or_default();
    let at = now();
    let mut failure = None;
    let mut actions = vec![];
    // Each repository on its own: one GitHub cannot read stops only its own pools.
    for repo in &repos {
        let listed = match shared.github.runners(app_id, repo) {
            Ok(found) => Some(found),
            Err(error) => {
                let error = error.context(format!("listing {repo}'s runners"));
                // In each of its pools' status, not only the log.
                for p in pools.iter().filter(|p| p.scope() == repo) {
                    self::error(shared, &p.name, format!("{error:#}"));
                }
                failure = Some(error);
                None
            }
        };
        let repo_pools: Vec<Pool> = pools
            .iter()
            .filter(|p| p.scope() == repo)
            .cloned()
            .collect();
        let mut entries = shared.entries.lock().unwrap();
        if let Some(listed) = &listed {
            let before = entries.clone();
            actions::observe(at, &mut entries, listed);
            for (name, e) in entries.iter() {
                if before
                    .get(name)
                    .is_some_and(|b| (b.online, b.busy) != (e.online, e.busy))
                {
                    save(shared, name, Some(e));
                    if e.online.is_some() && before[name].online.is_none() {
                        timing(shared, name, "online_ms");
                    }
                    if e.busy.is_some() && before[name].busy.is_none() {
                        timing(shared, name, "busy_ms");
                    }
                }
            }
        }
        let mine: BTreeMap<String, Entry> = entries
            .iter()
            .filter(|(_, e)| &repo_of(&e.spec) == repo)
            .map(|(n, e)| (n.clone(), e.clone()))
            .collect();
        drop(entries);
        let holds = shared.holds.lock().unwrap().clone();
        let mut queued = HashMap::new();
        if listed.is_some() {
            for p in &repo_pools {
                queued.insert(p.name.clone(), queued_jobs(shared, p, &mine, app_id, at));
            }
        }
        actions.extend(actions::plan(
            at,
            &repo_pools,
            &mine,
            listed.as_ref(),
            &|pool| holds.get(pool).is_some_and(|until| *until > at),
            &draining,
            &|pool| queued.get(pool).copied().unwrap_or(0),
        ));
    }
    for action in actions {
        match action {
            Action::Launch { pool, count, spill } => {
                let Some(pool) = pools.iter().find(|p| p.name == pool) else {
                    continue;
                };
                for _ in 0..count {
                    // Recorded before its thread runs, so the next pass counts it.
                    let name = match enter(shared, pool) {
                        Ok(name) => name,
                        Err(e) => {
                            error(shared, &pool.name, format!("{e:#}"));
                            break;
                        }
                    };
                    let (p, n) = (pool.clone(), name.clone());
                    if !spawn(shared, move |s| launch(&s, &p, &n, app_id, spill)) {
                        shared.entries.lock().unwrap().remove(&name);
                        save(shared, &name, None);
                        break;
                    }
                }
            }
            Action::Remove { name, why, force } => {
                // In flight from here on: the planner leaves it to this thread.
                if update(shared, &name, |e| {
                    e.phase = Phase::Removing;
                    e.retry_at = at + 3600;
                })
                .is_none()
                {
                    continue;
                }
                let again = name.clone();
                if !spawn(shared, move |s| remove(&s, &name, why, force, true, app_id)) {
                    // Out of threads: the next pass tries again.
                    update(shared, &again, |e| e.retry_at = 0);
                }
            }
        }
    }
    publish(shared);
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Holds, errors and start times, kept in the catalog when they change, so either process
/// serving the admin API answers `actions.status` (the pools run in the dashboard's).
fn publish(shared: &Shared) {
    let body = json!({
        "holds": *shared.holds.lock().unwrap(),
        "errors": shared.errors.lock().unwrap().iter().map(|(p, (t, m))| (p.clone(), json!({"unix_s":t,"message":m}))).collect::<serde_json::Map<_, _>>(),
        "recent": *shared.recent.lock().unwrap(),
    })
    .to_string();
    static LAST: Mutex<String> = Mutex::new(String::new());
    let mut last = LAST.lock().unwrap();
    if *last == body {
        return;
    }
    let result = db(&shared.root).and_then(|db| {
        db.execute("INSERT INTO runner_pool_status(id,body) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET body=excluded.body", [&body])?;
        Ok(())
    });
    match result {
        Ok(()) => *last = body,
        Err(e) => eprintln!("actions pools: saving status: {e:#}"),
    }
}

fn repo_of(spec: &str) -> String {
    spec.split('|').next().unwrap_or_default().to_owned()
}

/// Runs `f` on its own thread, unless MAX_THREADS are busy (false).
fn spawn(shared: &Arc<Shared>, f: impl FnOnce(Arc<Shared>) + Send + 'static) -> bool {
    if shared.threads.fetch_add(1, Ordering::SeqCst) >= MAX_THREADS {
        shared.threads.fetch_sub(1, Ordering::SeqCst);
        return false;
    }
    let s = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("runner-box".into())
        .spawn(move || {
            struct Slot(Arc<Shared>);
            impl Drop for Slot {
                fn drop(&mut self) {
                    self.0.threads.fetch_sub(1, Ordering::SeqCst);
                }
            }
            let slot = Slot(s.clone());
            f(s);
            drop(slot);
        });
    if spawned.is_err() {
        shared.threads.fetch_sub(1, Ordering::SeqCst);
    }
    spawned.is_ok()
}

fn timing(shared: &Shared, name: &str, key: &str) {
    let mut recent = shared.recent.lock().unwrap();
    if let Some(t) = recent.iter_mut().rev().find(|t| t["box"] == name) {
        if t[key].is_null() {
            t[key] = json!(now_ms());
        }
    }
}

fn error(shared: &Shared, pool: &str, message: String) {
    // A fault that repeats every pass is logged once a minute; status has the latest.
    static LOGGED: Mutex<BTreeMap<String, i64>> = Mutex::new(BTreeMap::new());
    let mut logged = LOGGED.lock().unwrap();
    if logged.get(pool).is_none_or(|at| now() - at >= 60) {
        eprintln!("actions pools: {pool}: {message}");
        logged.insert(pool.to_owned(), now());
    }
    shared
        .errors
        .lock()
        .unwrap()
        .insert(pool.to_owned(), (now(), message));
}

// ------------------------------------------------------------------------- boxes

/// Jobs queued for the pool's runners, asked of GitHub only when they could get a box: the
/// pool is saturated (every box busy, under `max`) and its placement pool may spill. Cached
/// for QUEUED_EVERY_S; 0 when GitHub cannot say.
fn queued_jobs(
    shared: &Shared,
    pool: &Pool,
    entries: &BTreeMap<String, Entry>,
    app_id: u64,
    at: i64,
) -> u32 {
    if !actions::saturated(pool, entries) || !may_spill(&shared.root, &pool.token) {
        shared.queued.lock().unwrap().remove(&pool.name);
        return 0;
    }
    if let Some((asked, count)) = shared.queued.lock().unwrap().get(&pool.name)
        && at - asked < QUEUED_EVERY_S
    {
        return *count;
    }
    let count = match shared
        .github
        .queued(app_id, pool.scope(), &pool.queue_repos(), &pool.labels)
    {
        Ok(count) => count,
        Err(e) => {
            error(shared, &pool.name, format!("listing queued jobs: {e:#}"));
            0
        }
    };
    shared
        .queued
        .lock()
        .unwrap()
        .insert(pool.name.clone(), (at, count));
    count
}

/// Whether the placement pool of the pool's workload token has a spill rule that lends a box.
fn may_spill(root: &Path, token: &str) -> bool {
    (|| -> Result<bool> {
        let db = crate::nodes::db(root)?;
        let Some((_, pool)) = crate::tokens::by_name(&db, token)? else {
            return Ok(false);
        };
        let settings = crate::telemetry::settings(&db)?;
        let rules: Vec<crate::spill::Rule> =
            serde_json::from_value(settings[crate::spill::RULES].clone()).unwrap_or_default();
        Ok(rules.iter().any(|r| r.pool == pool && r.max_boxes > 0))
    })()
    .unwrap_or(false)
}

/// Whether placement put the box on a runner of another pool. When the catalog cannot say,
/// yes: a spilled box then goes after its idle minute instead of staying warm.
fn spilled(root: &Path, name: &str) -> bool {
    crate::nodes::db(root)
        .and_then(|db| {
            Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM resources r JOIN box_pools b ON b.physical=r.physical AND b.spilled=1 WHERE r.kind='machine' AND r.name=?1)",
                [name],
                |r| r.get(0),
            )?)
        })
        .unwrap_or(true)
}

/// One box op as the pool's workload token: its owner, its placement pool, ephemeral.
fn box_op(shared: &Shared, token: &str, op: Value) -> Result<Value> {
    let (user, op) = {
        let mut catalog = shared
            .catalog
            .lock()
            .map_err(|_| anyhow!("catalog lock poisoned"))?;
        let (identity, pool) = crate::tokens::by_name(&catalog.db, token)?
            .with_context(|| format!("workload token {token} not found (ow token list)"))?;
        (catalog.user(&identity)?, crate::tokens::scoped(op, &pool)?)
    };
    // Unlocked for the runner call: the pool's boxes are made side by side.
    crate::catalog::Catalog::operation_shared(&shared.catalog, user, op)
}

/// Attaches the pool's AWS role to a new box as the token's owner, through the same handler
/// as `ow aws-role set`: the operator's allow-list for that owner applies, and a running box's
/// credentials are on its runner before this returns.
fn attach_role(shared: &Shared, token: &str, name: &str, role: &str) -> Result<()> {
    let user = {
        let mut catalog = shared
            .catalog
            .lock()
            .map_err(|_| anyhow!("catalog lock poisoned"))?;
        let (identity, _) = crate::tokens::by_name(&catalog.db, token)?
            .with_context(|| format!("workload token {token} not found (ow token list)"))?;
        catalog.user(&identity)?
    };
    crate::aws_metadata::role(
        &shared.catalog,
        user,
        &json!({"op":"aws-role","id":name,"role_arn":role}),
        false,
        &mut crate::aws_metadata::Instance::default(),
    )?;
    Ok(())
}

/// Whether a launch still owns its box: the planner may have timed it out meanwhile.
fn launching(shared: &Shared, name: &str) -> bool {
    shared
        .entries
        .lock()
        .unwrap()
        .get(name)
        .is_some_and(|e| e.phase == Phase::Launching)
}

/// A new box's entry, in the launching phase: its name.
fn enter(shared: &Shared, pool: &Pool) -> Result<String> {
    let name = format!("{}-{}", pool.name, &crate::common::nonce()?[..12]);
    let entry = Entry {
        pool: pool.name.clone(),
        token: pool.token.clone(),
        spec: pool.spec(),
        phase: Phase::Launching,
        runner_id: None,
        created: now(),
        started: None,
        online: None,
        busy: None,
        retry_at: 0,
        made: false,
        spilled: false,
    };
    shared
        .entries
        .lock()
        .unwrap()
        .insert(name.clone(), entry.clone());
    save(shared, &name, Some(&entry));
    Ok(name)
}

fn launch(shared: &Shared, pool: &Pool, name: &str, app_id: u64, spill: bool) {
    let name = name.to_owned();
    {
        let mut recent = shared.recent.lock().unwrap();
        if recent.len() >= RECENT {
            recent.pop_front();
        }
        recent.push_back(json!({"box":name,"pool":pool.name,"create_ms":now_ms()}));
    }
    let result = (|| -> Result<()> {
        // The box first: a full runner then costs no GitHub call. A warm box stays on the
        // pool's own runners; one for a queued job may spill (docs/MULTINODE.md#spill).
        let create = json!({"op":"create","id":name,"image":pool.image,"memory_mib":pool.memory_mib,
            "vcpu_count":pool.vcpu_count,"disk_gib":pool.disk_gib.unwrap_or(crate::common::DEFAULT_DISK_GIB),
            "ephemeral":true,"spill":spill});
        box_op(shared, &pool.token, create).context("create")?;
        let spilled = spilled(&shared.root, &name);
        update(shared, &name, |e| {
            e.made = true;
            e.spilled = spilled;
        });
        // Before the runner registers: no job runs in the box without its role.
        if let Some(role) = &pool.aws_role {
            attach_role(shared, &pool.token, &name, role).context("attaching its AWS role")?;
        }
        timing(shared, &name, "ready_ms");
        ensure!(launching(shared, &name), "timed out while it was made");
        let (runner_id, jit) = shared
            .github
            .jit_config(app_id, pool.scope(), pool.group(), &name, &pool.labels)
            .context("runner registration")?;
        update(shared, &name, |e| e.runner_id = Some(runner_id));
        timing(shared, &name, "registered_ms");
        if !launching(shared, &name) {
            // Timed out meanwhile: the removal may have missed the registration.
            let _ = shared.github.remove_runner(app_id, pool.scope(), runner_id);
            bail!("timed out while it was registered");
        }
        let data = STANDARD.encode(jit.as_bytes());
        drop(jit);
        // A just-made box answers at once; allow a slow first boot.
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match box_op(
                shared,
                &pool.token,
                json!({"op":"put","id":name,"path":JIT_PATH,"data":data}),
            ) {
                Ok(_) => break,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(500))
                }
                Err(e) => return Err(e.context("putting the registration in the box")),
            }
        }
        let started = box_op(
            shared,
            &pool.token,
            json!({"op":"exec","id":name,"command":START}),
        )
        .context("starting the runner")?;
        let code = started["exit_code"]
            .as_i64()
            .or(started["result"]["exit_code"].as_i64());
        ensure!(code == Some(0), "{START} exited {code:?}");
        ensure!(launching(shared, &name), "timed out while it was started");
        Ok(())
    })();
    match result {
        Ok(()) => {
            update(shared, &name, |e| {
                e.phase = Phase::Started;
                e.started = Some(now());
            });
            timing(shared, &name, "started_ms");
        }
        Err(_) if !launching(shared, &name) => {
            // The planner's timeout removal owns it now; the registration it minted is in the
            // entry (or the entry is gone with the box): unregister it if it was left behind.
            if let Some(e) = shared.entries.lock().unwrap().get(&name).cloned()
                && e.runner_id.is_some()
            {
                return;
            }
            // Its entry is gone with whatever existed then: clean up what was made after.
            let _ = shared
                .github
                .remove_runner_named(app_id, pool.scope(), &name);
            let _ = box_op(shared, &pool.token, json!({"op":"stop","id":name}));
            let _ = box_op(shared, &pool.token, json!({"op":"delete","id":name}));
        }
        Err(e) => {
            let message = format!("{e:#}");
            if !spill && message.contains("capacity exhausted") {
                // Its own runners are full of busy boxes: warm boxes wait for one to go.
                shared
                    .holds
                    .lock()
                    .unwrap()
                    .insert(format!("{}/home", pool.name), now() + HOME_HOLD_S);
            } else if message.contains("capacity exhausted") {
                // Its runners and the ones it may borrow are full: jobs wait on GitHub.
                shared
                    .holds
                    .lock()
                    .unwrap()
                    .insert(pool.name.clone(), now() + HOLD_S);
            } else {
                error(
                    shared,
                    &pool.name,
                    format!("{name}: launch failed: {message}"),
                );
                shared
                    .holds
                    .lock()
                    .unwrap()
                    .insert(pool.name.clone(), now() + HOLD_S);
            }
            update(shared, &name, |e| {
                e.phase = Phase::Removing;
                e.retry_at = now() + 3600;
            });
            // Its own cleanup frees no room for the pool: the hold stays.
            remove(shared, &name, "its launch failed", true, false, app_id);
        }
    }
}

fn remove(shared: &Shared, name: &str, why: &str, force: bool, release: bool, app_id: u64) {
    let Some(entry) = shared.entries.lock().unwrap().get(name).cloned() else {
        return;
    };
    let token = entry.token.clone();
    let result = (|| -> Result<bool> {
        if let Some(id) = entry.runner_id {
            if let Err(e) = shared
                .github
                .remove_runner(app_id, &repo_of(&entry.spec), id)
            {
                if !force {
                    // It took a job a moment ago: keep the box.
                    return Ok(false);
                }
                eprintln!("actions pools: {name}: registration left to expire: {e:#}");
            }
        }
        ensure!(
            !token.is_empty(),
            "no workload token recorded; delete the box by hand"
        );
        // Already stopped or never started: the delete decides.
        let _ = box_op(shared, &token, json!({"op":"stop","id":name}));
        match box_op(shared, &token, json!({"op":"delete","id":name})) {
            Ok(_) => Ok(true),
            Err(e)
                if e.downcast_ref::<crate::catalog::ResourceNotFound>()
                    .is_some() =>
            {
                Ok(true)
            }
            Err(e) => Err(e),
        }
    })();
    match result {
        Ok(true) => {
            shared.entries.lock().unwrap().remove(name);
            save(shared, name, None);
            if release {
                // Room was freed: the pool may make boxes again at once.
                let mut holds = shared.holds.lock().unwrap();
                holds.remove(&entry.pool);
                holds.remove(&format!("{}/home", entry.pool));
            }
            timing(shared, name, "removed_ms");
            eprintln!("actions pools: removed {name}: {why}");
        }
        Ok(false) => {
            update(shared, name, |e| {
                e.phase = Phase::Started;
                e.retry_at = 0;
            });
        }
        Err(e) => {
            error(
                shared,
                &entry.pool,
                format!("{name}: removal failed, retrying: {e:#}"),
            );
            update(shared, name, |e| e.retry_at = now() + REMOVE_RETRY_S);
        }
    }
}

// ------------------------------------------------------------------------- admin ops

/// `actions.status`: each pool's boxes and the latest start times.
/// `actions.status`: each pool's boxes and the latest start times. Read from the catalog, so
/// it answers in whichever process serves the admin API.
pub(crate) fn status(root: &Path, only: Option<&str>) -> Result<Value> {
    let (pools, app_id) = pools(root)?;
    let key = key_fingerprint(root)?;
    let entries = load_raw(root)?;
    let published: Option<String> = db(root)?
        .query_row("SELECT body FROM runner_pool_status WHERE id=1", [], |r| {
            r.get(0)
        })
        .optional()?;
    let published: Value = published
        .map(|b| serde_json::from_str(&b))
        .transpose()?
        .unwrap_or_else(|| json!({}));
    let at = now();
    let names: std::collections::BTreeSet<String> = pools
        .iter()
        .map(|p| p.name.clone())
        .chain(entries.values().map(|e| e.pool.clone()))
        .filter(|n| only.is_none_or(|o| o == n))
        .collect();
    let mut out = vec![];
    for name in names {
        let pool = pools.iter().find(|p| p.name == name);
        let mine: Vec<_> = entries.iter().filter(|(_, e)| e.pool == name).collect();
        let count = |f: &dyn Fn(&Entry) -> bool| mine.iter().filter(|(_, e)| f(e)).count();
        let empty = vec![];
        out.push(json!({
            "pool": name,
            "configured": pool.is_some(),
            "warm": pool.map(|p| p.warm),
            "max": pool.map(|p| p.max),
            "launching": count(&|e| e.phase == Phase::Launching),
            "idle": count(&|e| e.phase == Phase::Started && e.busy.is_none()),
            "busy": count(&|e| e.phase == Phase::Started && e.busy.is_some()),
            "removing": count(&|e| e.phase == Phase::Removing),
            "spilled": count(&|e| e.spilled && e.phase != Phase::Removing),
            "home_held_s": published["holds"][format!("{name}/home")].as_i64().map(|u| (u - at).max(0)).unwrap_or(0),
            "held_s": published["holds"][name.as_str()].as_i64().map(|u| (u - at).max(0)).unwrap_or(0),
            "last_error": published["errors"][name.as_str()].clone(),
            "boxes": mine.iter().map(|(n, e)| json!({"box":n,"phase":e.phase,"age_s":at-e.created,
                "online":e.online.is_some(),"busy":e.busy.is_some(),"spilled":e.spilled})).collect::<Vec<_>>(),
            "recent": published["recent"].as_array().unwrap_or(&empty).iter().filter(|t| t["pool"] == name.as_str()).cloned().collect::<Vec<_>>(),
        }));
    }
    Ok(
        json!({"running":!published.as_object().is_some_and(|o| o.is_empty()),"app_id":app_id,"app_key":key,"pools":out}),
    )
}

fn key_fingerprint(root: &Path) -> Result<Value> {
    match fs::read(root.join(KEY_FILE)) {
        Ok(pem) => Ok(json!({"present":true,"fingerprint":fingerprint(&pem)})),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({"present":false})),
        Err(e) => Err(e.into()),
    }
}

fn fingerprint(pem: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(pem);
    format!(
        "sha256:{}",
        digest[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

/// `actions.app_key.set`: keeps the App's private key (PEM) for the pools; never returned.
pub(crate) fn set_app_key(root: &Path, pem: &str) -> Result<Value> {
    let pem = pem.trim().to_owned() + "\n";
    ensure!(pem.len() <= 8192, "the key is larger than an RSA App key");
    jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes())
        .map_err(|_| anyhow!("not an RSA private key in PEM form (the App's .pem file)"))?;
    let path = root.join(KEY_FILE);
    let changed = fs::read(&path).map_or(true, |old| old != pem.as_bytes());
    if changed {
        let tmp = root.join(format!("{KEY_FILE}.tmp"));
        // A leftover could keep other permissions: mode applies only to a new file.
        let _ = fs::remove_file(&tmp);
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(pem.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, &path)?;
    }
    if let Some(shared) = SHARED.get() {
        shared.github.forget_tokens();
    }
    Ok(json!({"fingerprint":fingerprint(pem.as_bytes()),"changed":changed}))
}

// ------------------------------------------------------------------------- GitHub

struct GitHub {
    root: PathBuf,
    http: Option<reqwest::blocking::Client>,
    /// repo -> installation; installation -> (token, unix s expiry).
    installations: Mutex<HashMap<String, u64>>,
    tokens: Mutex<HashMap<u64, (String, i64)>>,
    /// GET URL -> (ETag, body): a 304 answer costs no rate limit.
    etags: Mutex<HashMap<String, (String, Value)>>,
    /// Unix s until which GitHub asked us to wait.
    wait_until: Mutex<i64>,
    /// The rate limit left after the last answer that said.
    remaining: Mutex<Option<i64>>,
    /// The key file's modification time the cached tokens were made with.
    key_stamp: Mutex<Option<std::time::SystemTime>>,
}

impl GitHub {
    fn new(root: &Path) -> Self {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("open-workspaces-actions-pools")
            .build()
            .ok();
        Self {
            root: root.to_path_buf(),
            http,
            installations: Mutex::default(),
            tokens: Mutex::default(),
            etags: Mutex::default(),
            wait_until: Mutex::new(0),
            remaining: Mutex::new(None),
            key_stamp: Mutex::new(None),
        }
    }

    fn forget_tokens(&self) {
        self.tokens.lock().unwrap().clear();
        self.installations.lock().unwrap().clear();
    }

    fn app_jwt(&self, app_id: u64) -> Result<String> {
        ensure!(app_id > 0, "actions.github_app_id is not set");
        let pem = fs::read(self.root.join(KEY_FILE))
            .context("no GitHub App key; set it with ow actions app-key set")?;
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(&pem)
            .map_err(|_| anyhow!("the stored GitHub App key is not an RSA PEM key"))?;
        let at = now();
        let claims = json!({"iat": at - 60, "exp": at + 540, "iss": app_id.to_string()});
        Ok(jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
            &claims,
            &key,
        )?)
    }

    fn token(&self, app_id: u64, repo: &str) -> Result<String> {
        // A key set since (maybe in the other process serving the admin API): start again.
        let stamp = fs::metadata(self.root.join(KEY_FILE))
            .and_then(|m| m.modified())
            .ok();
        {
            let mut seen = self.key_stamp.lock().unwrap();
            if *seen != stamp {
                *seen = stamp;
                self.forget_tokens();
            }
        }
        let installation = self.installations.lock().unwrap().get(repo).copied();
        if let Some(id) = installation
            && let Some((token, expires)) = self.tokens.lock().unwrap().get(&id)
            && *expires - 300 > now()
        {
            return Ok(token.clone());
        }
        let jwt = self.app_jwt(app_id)?;
        let id = match installation {
            Some(id) => id,
            None => {
                let (_, got) = self.call(
                    "GET",
                    &format!("{}/installation", api_base(repo)),
                    None,
                    &jwt,
                )?;
                let id = got["id"]
                    .as_u64()
                    .with_context(|| format!("the GitHub App is not installed on {repo}"))?;
                self.installations
                    .lock()
                    .unwrap()
                    .insert(repo.to_owned(), id);
                id
            }
        };
        let (status, got) = self.call(
            "POST",
            &format!("/app/installations/{id}/access_tokens"),
            None,
            &jwt,
        )?;
        if status == 404 {
            // Uninstalled (or reinstalled under a new id): look it up again next time.
            self.installations.lock().unwrap().remove(repo);
            bail!("the GitHub App's installation on {repo} is gone");
        }
        let token = got["token"]
            .as_str()
            .context("installation token")?
            .to_owned();
        // GitHub's are valid for an hour.
        self.tokens
            .lock()
            .unwrap()
            .insert(id, (token.clone(), now() + 3300));
        Ok(token)
    }

    /// One call: (status, JSON body). 304 on a GET answers the cached body.
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        bearer: &str,
    ) -> Result<(u16, Value)> {
        let http = self.http.as_ref().context("no HTTP client")?;
        let wait = *self.wait_until.lock().unwrap();
        ensure!(
            now() >= wait,
            "GitHub rate limit: waiting {} s",
            wait - now()
        );
        let url = format!("{GITHUB}{path}");
        let mut request = http
            .request(method.parse()?, &url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(bearer);
        let cached = (method == "GET")
            .then(|| self.etags.lock().unwrap().get(&url).cloned())
            .flatten();
        if let Some((etag, _)) = &cached {
            request = request.header("If-None-Match", etag);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send()?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let number = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<i64>().ok())
        };
        if let Some(left) = number("x-ratelimit-remaining") {
            *self.remaining.lock().unwrap() = Some(left);
        }
        if matches!(status, 403 | 429) {
            if let Some(retry) = number("retry-after") {
                *self.wait_until.lock().unwrap() = now() + retry;
            } else if number("x-ratelimit-remaining") == Some(0) {
                *self.wait_until.lock().unwrap() =
                    number("x-ratelimit-reset").unwrap_or(now() + 60) + 1;
            }
        }
        if status == 304 {
            if let Some((_, body)) = cached {
                return Ok((200, body));
            }
        }
        let etag = headers
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let text = response.text()?;
        if !(200..300).contains(&status) {
            if status == 404 {
                return Ok((404, Value::Null));
            }
            // GitHub's error text names the resource and the reason, never a credential.
            bail!(
                "GitHub {method} {path}: HTTP {status} {}",
                text.chars().take(200).collect::<String>()
            );
        }
        let value: Value = if text.is_empty() {
            json!({})
        } else {
            serde_json::from_str(&text)?
        };
        if method == "GET"
            && let Some(etag) = etag
        {
            let mut etags = self.etags.lock().unwrap();
            if etags.len() > 200 {
                etags.clear();
            }
            etags.insert(url, (etag, value.clone()));
        }
        Ok((status, value))
    }

    /// The scope's (repository's or organisation's) runners by name.
    fn runners(&self, app_id: u64, repo: &str) -> Result<HashMap<String, Runner>> {
        let token = self.token(app_id, repo)?;
        let mut out = HashMap::new();
        for page in 1..=20 {
            let (status, got) = self.call(
                "GET",
                &format!(
                    "{}/actions/runners?per_page=100&page={page}",
                    api_base(repo)
                ),
                None,
                &token,
            )?;
            if status == 404 {
                self.installations.lock().unwrap().remove(repo);
            }
            ensure!(status == 200, "runner list: HTTP {status}");
            let list = got["runners"].as_array().context("runner list")?;
            for r in list {
                if let (Some(name), Ok(runner)) = (
                    r["name"].as_str(),
                    serde_json::from_value::<Runner>(r.clone()),
                ) {
                    out.insert(name.to_owned(), runner);
                }
            }
            if list.len() < 100 {
                return Ok(out);
            }
        }
        // A runner missing from a cut-off list would look gone and have its box removed.
        bail!("runner list: more than 2000 runners, the list would be incomplete")
    }

    /// Jobs queued for runners with `labels`: every label a queued job asks for is one of
    /// them (as GitHub matches), in the queued and running workflow runs of `repos`, read
    /// with the installation token of `scope` (the repository, or their organisation).
    fn queued(&self, app_id: u64, scope: &str, repos: &[String], labels: &[String]) -> Result<u32> {
        // Never at the cost of what the pool itself needs (runner lists, registrations):
        // checked before every call, as several repositories' scans add up.
        let low = || {
            self.remaining
                .lock()
                .unwrap()
                .is_some_and(|n| n < QUEUED_FLOOR)
        };
        if low() {
            let left = *self.remaining.lock().unwrap();
            // Expected under load, not a fault: no spill until the limit refills.
            eprintln!(
                "actions pools: GitHub rate limit low ({left:?} left): not asking for queued jobs"
            );
            return Ok(0);
        }
        let token = self.token(app_id, scope)?;
        let ours: Vec<String> = labels.iter().map(|l| l.to_ascii_lowercase()).collect();
        let mut count = 0;
        'scan: for (repo, status) in repos
            .iter()
            .flat_map(|r| ["queued", "in_progress"].map(|s| (r, s)))
        {
            if low() {
                break 'scan;
            }
            let (code, runs) = self.call(
                "GET",
                &format!("/repos/{repo}/actions/runs?status={status}&per_page={QUEUED_RUNS}"),
                None,
                &token,
            )?;
            ensure!(code == 200, "workflow runs: HTTP {code}");
            for run in runs["workflow_runs"].as_array().into_iter().flatten() {
                let Some(id) = run["id"].as_u64() else {
                    continue;
                };
                if low() {
                    break 'scan;
                }
                let (code, jobs) = self.call(
                    "GET",
                    &format!("/repos/{repo}/actions/runs/{id}/jobs?filter=latest&per_page=100"),
                    None,
                    &token,
                )?;
                ensure!(code == 200, "workflow jobs: HTTP {code}");
                count += jobs["jobs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|j| j["status"] == "queued" && matches(&j["labels"], &ours))
                    .count() as u32;
            }
        }
        Ok(count)
    }

    /// A single-use registration: (runner id, encoded JIT config).
    fn jit_config(
        &self,
        app_id: u64,
        scope: &str,
        group: u64,
        name: &str,
        labels: &[String],
    ) -> Result<(u64, String)> {
        let token = self.token(app_id, scope)?;
        let (_, got) = self.call(
            "POST",
            &format!("{}/actions/runners/generate-jitconfig", api_base(scope)),
            Some(
                &json!({"name":name,"runner_group_id":group,"labels":labels,"work_folder":"_work"}),
            ),
            &token,
        )?;
        Ok((
            got["runner"]["id"].as_u64().context("runner id")?,
            got["encoded_jit_config"]
                .as_str()
                .context("JIT config")?
                .to_owned(),
        ))
    }

    /// Unregisters a runner by name, if GitHub lists it.
    fn remove_runner_named(&self, app_id: u64, repo: &str, name: &str) -> Result<()> {
        if let Some(r) = self.runners(app_id, repo)?.get(name) {
            self.remove_runner(app_id, repo, r.id)?;
        }
        Ok(())
    }

    /// Unregisters a runner; one already gone is fine. Busy runners are refused by GitHub.
    fn remove_runner(&self, app_id: u64, repo: &str, id: u64) -> Result<()> {
        let token = self.token(app_id, repo)?;
        self.call(
            "DELETE",
            &format!("{}/actions/runners/{id}", api_base(repo)),
            None,
            &token,
        )?;
        Ok(())
    }
}

/// Whether a job asking for `asked` labels can run on a runner with `ours` (lowercase): it
/// asks for at least one and every one is ours.
fn matches(asked: &Value, ours: &[String]) -> bool {
    let asked: Vec<String> = asked
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l.as_str().map(str::to_ascii_lowercase))
        .collect();
    !asked.is_empty() && asked.iter().all(|l| ours.contains(l))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_matches_when_every_label_it_asks_for_is_the_pools() {
        let ours = ["self-hosted", "linux", "x64", "app-ci"].map(String::from);
        assert!(matches(
            &json!(["self-hosted", "Linux", "X64", "app-ci"]),
            &ours
        ));
        assert!(matches(&json!(["app-ci"]), &ours));
        assert!(!matches(&json!(["ubuntu-latest"]), &ours));
        assert!(!matches(&json!(["self-hosted", "app-ci", "gpu"]), &ours));
        assert!(!matches(&json!([]), &ours));
    }
}
