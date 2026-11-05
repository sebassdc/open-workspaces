//! GitHub Actions runner pools on the controller (docs/ACTIONS.md). Every 2 s one thread
//! reads `actions.pools`, lists each repository's runners on GitHub (an unchanged list costs
//! no rate limit) and runs `actions::plan`. Each box it makes or removes gets its own thread,
//! with its own catalog connection, so creates, hand-offs and deletes run side by side and
//! none waits on the dashboard's catalog lock. A pool whose launch failed (its runners are
//! full, say) holds until one of its boxes is removed, or for 10 s.
//!
//! A box: create (ephemeral, through the pool's workload token) -> a single-use runner
//! registration (JIT config) from the GitHub App -> put it in the box -> start the runner.
//! GitHub hands a queued job to an idle runner at once; the box is removed when GitHub drops
//! the runner after its job. The App key stays in a 0600 file and in memory; no key, token or
//! registration is logged or kept in the catalog.
use crate::actions::{self, Action, Entry, Phase, Pool, Runner};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
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
    pool_admins: Vec<String>,
    entries: Mutex<BTreeMap<String, Entry>>,
    /// Pool -> unix s until which it makes no box.
    holds: Mutex<HashMap<String, i64>>,
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
pub(crate) fn start(root: &Path, pool_admins: Vec<String>) {
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
        pool_admins,
        entries: Mutex::new(entries),
        holds: Mutex::default(),
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
    db.execute_batch("CREATE TABLE IF NOT EXISTS runner_boxes(name TEXT PRIMARY KEY,entry TEXT NOT NULL);")?;
    Ok(db)
}

/// What a previous process left: anything it was still making or removing is removed now.
fn load(root: &Path) -> Result<BTreeMap<String, Entry>> {
    let db = db(root)?;
    let mut out = BTreeMap::new();
    let rows: Vec<(String, String)> = db
        .prepare("SELECT name,entry FROM runner_boxes")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (name, text) in rows {
        let mut entry: Entry = serde_json::from_str(&text)?;
        if entry.phase != Phase::Started {
            entry.phase = Phase::Removing;
            entry.retry_at = 0;
        }
        out.insert(name, entry);
    }
    Ok(out)
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

fn pass(shared: &Arc<Shared>) -> Result<()> {
    let (pools, app_id) = pools(&shared.root)?;
    let repos: std::collections::BTreeSet<String> = {
        let entries = shared.entries.lock().unwrap();
        if pools.is_empty() && entries.is_empty() {
            return Ok(());
        }
        pools
            .iter()
            .map(|p| p.repo.clone())
            .chain(entries.values().map(|e| repo_of(&e.spec)))
            .collect()
    };
    // Every repository's runners, or None when one could not be read.
    let mut listed = Some(HashMap::new());
    let mut failure = None;
    for repo in &repos {
        match shared.github.runners(app_id, repo) {
            Ok(found) => {
                if let Some(all) = listed.as_mut() {
                    all.extend(found);
                }
            }
            Err(error) => {
                listed = None;
                failure = Some(error.context(format!("listing {repo}'s runners")));
                break;
            }
        }
    }
    let at = now();
    let actions = {
        let mut entries = shared.entries.lock().unwrap();
        if let Some(listed) = &listed {
            let before = entries.clone();
            actions::observe(at, &mut entries, listed);
            for (name, e) in entries.iter() {
                if before.get(name).is_some_and(|b| (b.online, b.busy) != (e.online, e.busy)) {
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
        let holds = shared.holds.lock().unwrap().clone();
        actions::plan(at, &pools, &entries, listed.as_ref(), &|pool| {
            holds.get(pool).is_some_and(|until| *until > at)
        })
    };
    for action in actions {
        match action {
            Action::Launch { pool, count } => {
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
                    if !spawn(shared, move |s| launch(&s, &p, &n, app_id)) {
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
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
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
    eprintln!("actions pools: {pool}: {message}");
    shared
        .errors
        .lock()
        .unwrap()
        .insert(pool.to_owned(), (now(), message));
}

// ------------------------------------------------------------------------- boxes

/// One box op as the pool's workload token: its owner, its placement pool, ephemeral.
fn box_op(shared: &Shared, token: &str, op: Value) -> Result<Value> {
    let mut catalog = crate::catalog::Catalog::secondary(&shared.root, shared.pool_admins.clone())?;
    let (identity, pool) = crate::tokens::by_name(&catalog.db, token)?
        .with_context(|| format!("workload token {token} not found (ow token list)"))?;
    let user = catalog.user(&identity)?;
    catalog.operation(user, crate::tokens::scoped(op, &pool)?)
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
    };
    shared
        .entries
        .lock()
        .unwrap()
        .insert(name.clone(), entry.clone());
    save(shared, &name, Some(&entry));
    Ok(name)
}

fn launch(shared: &Shared, pool: &Pool, name: &str, app_id: u64) {
    let name = name.to_owned();
    {
        let mut recent = shared.recent.lock().unwrap();
        if recent.len() >= RECENT {
            recent.pop_front();
        }
        recent.push_back(json!({"box":name,"pool":pool.name,"create_ms":now_ms()}));
    }
    let result = (|| -> Result<()> {
        let create = json!({"op":"create","id":name,"image":pool.image,"memory_mib":pool.memory_mib,
            "vcpu_count":pool.vcpu_count,"disk_gib":pool.disk_gib.unwrap_or(crate::common::DEFAULT_DISK_GIB),"ephemeral":true});
        box_op(shared, &pool.token, create).context("create")?;
        timing(shared, &name, "ready_ms");
        let (runner_id, jit) = shared
            .github
            .jit_config(app_id, &pool.repo, &name, &pool.labels)
            .context("runner registration")?;
        update(shared, &name, |e| e.runner_id = Some(runner_id));
        timing(shared, &name, "registered_ms");
        let data = STANDARD.encode(jit.as_bytes());
        drop(jit);
        // A just-made box answers at once; allow a slow first boot.
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match box_op(shared, &pool.token, json!({"op":"put","id":name,"path":JIT_PATH,"data":data})) {
                Ok(_) => break,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(500)),
                Err(e) => return Err(e.context("putting the registration in the box")),
            }
        }
        let started = box_op(shared, &pool.token, json!({"op":"exec","id":name,"command":START}))
            .context("starting the runner")?;
        let code = started["exit_code"].as_i64().or(started["result"]["exit_code"].as_i64());
        ensure!(code == Some(0), "{START} exited {code:?}");
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
        Err(e) => {
            error(shared, &pool.name, format!("{name}: launch failed: {e:#}"));
            shared
                .holds
                .lock()
                .unwrap()
                .insert(pool.name.clone(), now() + HOLD_S);
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
            if let Err(e) = shared.github.remove_runner(app_id, &repo_of(&entry.spec), id) {
                if !force {
                    // It took a job a moment ago: keep the box.
                    return Ok(false);
                }
                eprintln!("actions pools: {name}: registration left to expire: {e:#}");
            }
        }
        ensure!(!token.is_empty(), "no workload token recorded; delete the box by hand");
        // Already stopped or never started: the delete decides.
        let _ = box_op(shared, &token, json!({"op":"stop","id":name}));
        match box_op(shared, &token, json!({"op":"delete","id":name})) {
            Ok(_) => Ok(true),
            Err(e) if e.downcast_ref::<crate::catalog::ResourceNotFound>().is_some() => Ok(true),
            Err(e) => Err(e),
        }
    })();
    match result {
        Ok(true) => {
            shared.entries.lock().unwrap().remove(name);
            save(shared, name, None);
            if release {
                // Room was freed: the pool may make boxes again at once.
                shared.holds.lock().unwrap().remove(&entry.pool);
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
            error(shared, &entry.pool, format!("{name}: removal failed, retrying: {e:#}"));
            update(shared, name, |e| e.retry_at = now() + REMOVE_RETRY_S);
        }
    }
}

// ------------------------------------------------------------------------- admin ops

/// `actions.status`: each pool's boxes and the latest start times.
pub(crate) fn status(root: &Path, only: Option<&str>) -> Result<Value> {
    let (pools, app_id) = pools(root)?;
    let key = key_fingerprint(root)?;
    let Some(shared) = SHARED.get() else {
        return Ok(json!({"running":false,"app_id":app_id,"app_key":key,"pools":[]}));
    };
    let entries = shared.entries.lock().unwrap().clone();
    let holds = shared.holds.lock().unwrap().clone();
    let errors = shared.errors.lock().unwrap().clone();
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
        out.push(json!({
            "pool": name,
            "configured": pool.is_some(),
            "warm": pool.map(|p| p.warm),
            "max": pool.map(|p| p.max),
            "launching": count(&|e| e.phase == Phase::Launching),
            "idle": count(&|e| e.phase == Phase::Started && e.busy.is_none()),
            "busy": count(&|e| e.phase == Phase::Started && e.busy.is_some()),
            "removing": count(&|e| e.phase == Phase::Removing),
            "held_s": holds.get(&name).map(|u| (u - at).max(0)).unwrap_or(0),
            "last_error": errors.get(&name).map(|(t, m)| json!({"unix_s":t,"message":m})),
            "boxes": mine.iter().map(|(n, e)| json!({"box":n,"phase":e.phase,"age_s":at-e.created,
                "online":e.online.is_some(),"busy":e.busy.is_some()})).collect::<Vec<_>>(),
            "recent": shared.recent.lock().unwrap().iter().filter(|t| t["pool"] == name.as_str()).cloned().collect::<Vec<_>>(),
        }));
    }
    Ok(json!({"running":true,"app_id":app_id,"app_key":key,"pools":out}))
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
        digest[..8].iter().map(|b| format!("{b:02x}")).collect::<String>()
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
                let (_, got) = self.call("GET", &format!("/repos/{repo}/installation"), None, &jwt)?;
                let id = got["id"].as_u64().context("installation id")?;
                self.installations.lock().unwrap().insert(repo.to_owned(), id);
                id
            }
        };
        let (_, got) = self.call("POST", &format!("/app/installations/{id}/access_tokens"), None, &jwt)?;
        let token = got["token"].as_str().context("installation token")?.to_owned();
        // GitHub's are valid for an hour.
        self.tokens
            .lock()
            .unwrap()
            .insert(id, (token.clone(), now() + 3300));
        Ok(token)
    }

    /// One call: (status, JSON body). 304 on a GET answers the cached body.
    fn call(&self, method: &str, path: &str, body: Option<&Value>, bearer: &str) -> Result<(u16, Value)> {
        let http = self.http.as_ref().context("no HTTP client")?;
        let wait = *self.wait_until.lock().unwrap();
        ensure!(now() >= wait, "GitHub rate limit: waiting {} s", wait - now());
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
        if matches!(status, 403 | 429) {
            if let Some(retry) = number("retry-after") {
                *self.wait_until.lock().unwrap() = now() + retry;
            } else if number("x-ratelimit-remaining") == Some(0) {
                *self.wait_until.lock().unwrap() = number("x-ratelimit-reset").unwrap_or(now() + 60) + 1;
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

    /// The repository's runners by name.
    fn runners(&self, app_id: u64, repo: &str) -> Result<HashMap<String, Runner>> {
        let token = self.token(app_id, repo)?;
        let mut out = HashMap::new();
        for page in 1..=20 {
            let (status, got) = self.call(
                "GET",
                &format!("/repos/{repo}/actions/runners?per_page=100&page={page}"),
                None,
                &token,
            )?;
            ensure!(status == 200, "runner list: HTTP {status}");
            let list = got["runners"].as_array().context("runner list")?;
            for r in list {
                if let (Some(name), Ok(runner)) = (r["name"].as_str(), serde_json::from_value::<Runner>(r.clone())) {
                    out.insert(name.to_owned(), runner);
                }
            }
            if list.len() < 100 {
                break;
            }
        }
        Ok(out)
    }

    /// A single-use registration: (runner id, encoded JIT config).
    fn jit_config(&self, app_id: u64, repo: &str, name: &str, labels: &[String]) -> Result<(u64, String)> {
        let token = self.token(app_id, repo)?;
        let (_, got) = self.call(
            "POST",
            &format!("/repos/{repo}/actions/runners/generate-jitconfig"),
            Some(&json!({"name":name,"runner_group_id":1,"labels":labels,"work_folder":"_work"})),
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

    /// Unregisters a runner; one already gone is fine. Busy runners are refused by GitHub.
    fn remove_runner(&self, app_id: u64, repo: &str, id: u64) -> Result<()> {
        let token = self.token(app_id, repo)?;
        self.call("DELETE", &format!("/repos/{repo}/actions/runners/{id}"), None, &token)?;
        Ok(())
    }
}
