//! Outbound node transport. The listener has no browser/admin authentication routes.
use crate::{common, wire};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{
        Path as RoutePath, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    net::SocketAddr,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Semaphore, mpsc, oneshot},
};

const FRAME: usize = 65536;
const OFFLINE: i64 = 15;
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn random_secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn digest(secret: &str) -> String {
    format!("{:x}", Sha256::digest(secret.as_bytes()))
}

pub(crate) fn db(root: &Path) -> Result<Connection> {
    let path = root.join("catalog.sqlite3");
    if !path.exists() {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
    }
    private(&path)?;
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(5))?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    ensure!(version <= 3, "catalog schema is newer than this binary");
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
      CREATE TABLE IF NOT EXISTS nodes(id TEXT PRIMARY KEY, credential_hash TEXT, revoked INTEGER NOT NULL DEFAULT 0, heartbeat INTEGER NOT NULL DEFAULT 0, memory_mib INTEGER NOT NULL, slots INTEGER NOT NULL, capabilities TEXT NOT NULL DEFAULT '{}', session TEXT);
      CREATE TABLE IF NOT EXISTS node_joins(secret_hash TEXT PRIMARY KEY, node TEXT NOT NULL REFERENCES nodes(id), expires INTEGER NOT NULL, consumed INTEGER NOT NULL DEFAULT 0);")?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS node_limits(node TEXT PRIMARY KEY REFERENCES nodes(id), vcpu_cap INTEGER NOT NULL CHECK(vcpu_cap BETWEEN 1 AND 16));")?;
    // Additive larger-cap authority; legacy readers retain conservative CPU limits.
    db.execute_batch("CREATE TABLE IF NOT EXISTS node_capacity_limits(node TEXT PRIMARY KEY REFERENCES nodes(id), vcpu_cap INTEGER NOT NULL CHECK(vcpu_cap BETWEEN 1 AND 64));")?;
    Ok(db)
}
pub(crate) fn private(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_file() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "credential/catalog must be an owned private regular file"
    );
    Ok(())
}
pub(crate) fn write_private(path: &Path, value: &Value) -> Result<()> {
    let parent = path.parent().context("private parent directory required")?;
    let m = fs::symlink_metadata(parent)?;
    ensure!(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "credential directory must be owned and private"
    );
    let temporary = path.with_extension(format!("{}.tmp", common::nonce()?));
    let result = (|| -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        f.write_all(value.to_string().as_bytes())?;
        f.sync_all()?;
        fs::hard_link(&temporary, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result?;
    Ok(())
}
pub fn mint(
    root: &Path,
    node: &str,
    output: &Path,
    ttl: u64,
    memory: u32,
    slots: u32,
) -> Result<()> {
    let value = invitation(root, node, ttl, memory, slots, 16, None, |v| {
        write_private(output, v)
    })?;
    drop(value);
    Ok(())
}
pub const POOL_POLICY: &str = "Trusted shared private pool: all admitted workspace users may select this host. The host operator can inspect guest data and stop participation. This invitation grants node participation only, not human workspace management.";
#[allow(clippy::too_many_arguments)] // Explicit owner-issued limits and transactional publication callback.
pub fn invitation(
    root: &Path,
    node: &str,
    ttl: u64,
    memory: u32,
    slots: u32,
    cpus: u32,
    controller: Option<&str>,
    publish: impl FnOnce(&Value) -> Result<()>,
) -> Result<Value> {
    common::identifier(node)?;
    ensure!(node != "local", "local is reserved");
    ensure!((1..=3600).contains(&ttl), "invalid enrollment expiry");
    common::host_limits(memory, slots, cpus)?;
    let mut db = db(root)?;
    let secret = random_secret()?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    ensure!(
        !tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=?1 AND (credential_hash IS NOT NULL OR revoked=1))",
            [node],
            |r| r.get::<_, bool>(0)
        )?,
        "node already enrolled or revoked; use a new node ID"
    );
    tx.execute("INSERT INTO nodes(id,memory_mib,slots) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET memory_mib=?2,slots=?3", params![node,memory,slots])?;
    tx.execute(
        "INSERT INTO node_joins(secret_hash,node,expires) VALUES(?1,?2,?3)",
        params![digest(&secret), node, now() + ttl as i64],
    )?;
    tx.execute("INSERT INTO node_limits(node,vcpu_cap) VALUES(?1,?2) ON CONFLICT(node) DO UPDATE SET vcpu_cap=?2",params![node,cpus.min(16)])?;
    tx.execute("INSERT INTO node_capacity_limits(node,vcpu_cap) VALUES(?1,?2) ON CONFLICT(node) DO UPDATE SET vcpu_cap=?2",params![node,cpus])?;
    // Reissuing an unused invitation invalidates the older invitations for this node.
    tx.execute(
        "UPDATE node_joins SET consumed=1 WHERE node=?1 AND secret_hash<>?2",
        params![node, digest(&secret)],
    )?;
    let value = json!({"node":node,"secret":secret,"expires":now()+ttl as i64,"policy":POOL_POLICY,"controller":controller,"memory":memory,"slots":slots,"cpus":cpus});
    publish(&value)?;
    tx.commit()?;
    Ok(value)
}
fn enroll(root: &Path, value: Value) -> Result<Value> {
    let node = value["node"].as_str().context("node required")?;
    common::identifier(node)?;
    let secret = value["secret"].as_str().context("secret required")?;
    ensure!(secret.len() == 64, "invalid secret");
    let mut db = db(root)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    ensure!(tx.execute("UPDATE node_joins SET consumed=1 WHERE secret_hash=?1 AND node=?2 AND consumed=0 AND expires>?3 AND EXISTS(SELECT 1 FROM nodes WHERE id=?2 AND credential_hash IS NULL AND revoked=0)", params![digest(secret),node,now()])?==1, "invalid or expired enrollment");
    let credential = random_secret()?;
    tx.execute(
        "UPDATE nodes SET credential_hash=?1 WHERE id=?2",
        params![digest(&credential), node],
    )?;
    tx.commit()?;
    Ok(json!({"node":node,"credential":credential}))
}
/// Inventory transport has an absolute deadline; no SQLite writer is held here.
fn budget_inventory(root: &Path, node: &str) -> Result<Value> {
    let mut stream =
        std::os::unix::net::UnixStream::connect(route(root, node)?.join("control.sock"))?;
    stream.set_write_timeout(Some(Duration::from_millis(250)))?;
    wire::send(&mut stream, &json!({"op":"list"}))?;
    stream.set_nonblocking(true)?;
    let deadline = std::time::Instant::now();
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 65536];
    loop {
        ensure!(
            deadline.elapsed() < Duration::from_secs(3),
            "idle inventory deadline exceeded; budget unchanged"
        );
        match stream.read(&mut buffer) {
            Ok(0) => anyhow::bail!("idle inventory disconnected"),
            Ok(n) => {
                bytes.extend_from_slice(&buffer[..n]);
                ensure!(bytes.len() <= 1024 * 1024, "idle inventory oversized");
                if let Some(end) = bytes.iter().position(|b| *b == b'\n') {
                    let envelope: Value = serde_json::from_slice(&bytes[..end])?;
                    ensure!(envelope["ok"] == true, "idle inventory rejected");
                    return Ok(envelope["result"].clone());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => return Err(e.into()),
        }
    }
}
/// Owner authority only. Caller serializes with catalog admission.
pub fn budget(root: &Path, node: &str, memory: u32, slots: u32, cpus: u32) -> Result<Value> {
    common::identifier(node)?;
    common::host_limits(memory, slots, cpus)?;
    let mut db = db(root)?;
    let sql = "SELECT memory_mib,slots,COALESCE((SELECT vcpu_cap FROM node_capacity_limits WHERE node=nodes.id),(SELECT vcpu_cap FROM node_limits WHERE node=nodes.id),16),session FROM nodes WHERE id=?1 AND credential_hash IS NOT NULL AND revoked=0";
    let prior: (u32, u32, u32, Option<String>) = db
        .query_row(sql, [node], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .context("active enrolled node required")?;
    let lowering = memory < prior.0 || slots < prior.1 || cpus < prior.2;
    if lowering {
        let inventory = budget_inventory(root, node)?;
        ensure!(
            inventory
                .as_array()
                .is_some_and(|a| a.iter().all(|m| matches!(
                    m["state"].as_str(),
                    Some("stopped" | "hibernated" | "failed")
                ))),
            "lowering requires positively idle inventory"
        );
    }
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let current: (u32, u32, u32, Option<String>) = tx
        .query_row(sql, [node], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .context("active enrolled node required")?;
    ensure!(
        prior == current,
        "node authority/session changed during inventory; retry deliberately"
    );
    if lowering {
        let fresh: bool = tx.query_row(
            "SELECT heartbeat>?1 AND heartbeat<=?2 FROM nodes WHERE id=?3",
            params![now() - OFFLINE, now(), node],
            |r| r.get(0),
        )?;
        ensure!(fresh, "idle inventory no longer online");
        let unresolved: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM resources WHERE node=?1 AND (reserved_mib>0 OR reserved_cpus>0)) OR EXISTS(SELECT 1 FROM operations WHERE node=?1 AND state IN ('pending','uncertain'))",[node],|r|r.get(0))?;
        let extra: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM node_usage WHERE node=?1 AND (extra_mib>0 OR extra_cpus>0 OR extra_slots>0))",[node],|r|r.get(0))?;
        ensure!(!extra, "lowering rejected: unregistered host reservations");
        ensure!(
            !unresolved,
            "lowering rejected: running or unresolved catalog demand"
        );
    }
    tx.execute(
        "UPDATE nodes SET memory_mib=?1,slots=?2 WHERE id=?3",
        params![memory, slots, node],
    )?;
    tx.execute("INSERT INTO node_capacity_limits(node,vcpu_cap) VALUES(?1,?2) ON CONFLICT(node) DO UPDATE SET vcpu_cap=?2",params![node,cpus])?;
    tx.execute("INSERT INTO node_limits(node,vcpu_cap) VALUES(?1,?2) ON CONFLICT(node) DO UPDATE SET vcpu_cap=?2",params![node,cpus.min(16)])?;
    // Clamp already-reported capabilities immediately; increases still need the next heartbeat.
    let capabilities: String =
        tx.query_row("SELECT capabilities FROM nodes WHERE id=?1", [node], |r| {
            r.get(0)
        })?;
    let mut cap: Value = serde_json::from_str(&capabilities)?;
    for (key, bound) in [("memory_mib", memory), ("slots", slots), ("vcpus", cpus)] {
        cap[key] = json!(cap[key].as_u64().unwrap_or(0).min(bound as u64));
    }
    tx.execute(
        "UPDATE nodes SET capabilities=?1 WHERE id=?2",
        params![cap.to_string(), node],
    )?;
    tx.commit()?;
    Ok(
        json!({"node":node,"memory":memory,"slots":slots,"cpus":cpus,"operator_configuration_required":true}),
    )
}
pub fn revoke(root: &Path, node: &str) -> Result<()> {
    ensure!(
        db(root)?.execute("UPDATE nodes SET revoked=1,heartbeat=0 WHERE id=?1", [node])? == 1,
        "unknown node"
    );
    Ok(())
}
pub fn inventory(root: &Path) -> Result<Value> {
    let db = db(root)?;
    let mut stmt = db.prepare("SELECT id,revoked,heartbeat,memory_mib,slots,capabilities,COALESCE((SELECT vcpu_cap FROM node_capacity_limits WHERE node=nodes.id),(SELECT vcpu_cap FROM node_limits WHERE node=nodes.id),16) FROM nodes ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        let ram:u32 = r.get(3)?;
        let slots:u32 = r.get(4)?;
        let cpus:u32 = r.get(6)?;
        let mut cap:Value = serde_json::from_str(&r.get::<_,String>(5)?).unwrap_or(json!({}));
        if !cap.is_object() { cap=json!({}); }
        for (key,bound) in [("memory_mib",ram),("slots",slots),("vcpus",cpus)] {
            cap[key]=json!(cap[key].as_u64().unwrap_or(0).min(bound as u64));
        }
        Ok(json!({"id":r.get::<_,String>(0)?,"revoked":r.get::<_,bool>(1)?,"online":!r.get::<_,bool>(1)? && r.get::<_,i64>(2)?>now()-OFFLINE && r.get::<_,i64>(2)?<=now(),"memory_mib":cap["memory_mib"],"slots":cap["slots"],"capabilities":cap,"owner_limits":{"memory":ram,"slots":slots,"cpus":cpus}}))
    })?;
    Ok(Value::Array(
        rows.collect::<std::result::Result<Vec<_>, _>>()?,
    ))
}

pub fn online(root: &Path, node: &str) -> Result<()> {
    if node == "local" {
        return Ok(());
    }
    ensure!(
        db(root)?.query_row(
            "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=?1 AND revoked=0 AND heartbeat>?2 AND heartbeat<=?3)",
            params![node, now() - OFFLINE,now()],
            |r| r.get::<_, bool>(0)
        )?,
        "node offline or revoked; placement remains fixed"
    );
    Ok(())
}
pub fn ssh_capable(root: &Path, node: &str) -> Result<()> {
    online(root, node)?;
    if node != "local" {
        let cap: String =
            db(root)?.query_row("SELECT capabilities FROM nodes WHERE id=?1", [node], |r| {
                r.get(0)
            })?;
        ensure!(
            serde_json::from_str::<Value>(&cap)?["guest_ssh_v1"] == true,
            "node does not support guest SSH v1"
        );
    }
    Ok(())
}
pub fn route(root: &Path, node: &str) -> Result<PathBuf> {
    online(root, node)?;
    common::identifier(node)?;
    Ok(if node == "local" {
        root.to_owned()
    } else {
        root.join("nodes").join(node)
    })
}
fn auth(root: &Path, node: &str, headers: &HeaderMap) -> Result<()> {
    common::identifier(node)?;
    let secret = headers
        .get("authorization")
        .and_then(|s| s.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .context("node credential required")?;
    ensure!(secret.len() == 64, "invalid credential");
    ensure!(
        db(root)?.query_row(
            "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=?1 AND credential_hash=?2 AND revoked=0)",
            params![node, digest(secret)],
            |r| r.get::<_, bool>(0)
        )?,
        "invalid credential"
    );
    Ok(())
}
struct Session {
    generation: String,
    tx: mpsc::Sender<String>,
}
struct Job {
    node: String,
    generation: String,
    tx: oneshot::Sender<WebSocket>,
}
#[derive(Clone)]
struct Controller {
    root: PathBuf,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    jobs: Arc<Mutex<HashMap<String, Job>>>,
    slots: Arc<Semaphore>,
}
async fn enrollment(State(c): State<Controller>, Json(v): Json<Value>) -> Response {
    match enroll(&c.root, v) {
        Ok(v) => Json(v).into_response(),
        Err(_) => (StatusCode::UNAUTHORIZED, "enrollment rejected").into_response(),
    }
}
async fn session(
    State(c): State<Controller>,
    RoutePath(node): RoutePath<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if auth(&c.root, &node, &headers).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.max_message_size(4096)
        .max_frame_size(4096)
        .on_upgrade(move |socket| session_loop(c, node, headers, socket))
}
async fn job(
    State(c): State<Controller>,
    RoutePath((node, id)): RoutePath<(String, String)>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if auth(&c.root, &node, &headers).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let job = {
        let sessions = c.sessions.lock().unwrap();
        let mut jobs = c.jobs.lock().unwrap();
        let valid = jobs.get(&id).is_some_and(|j| {
            j.node == node
                && sessions
                    .get(&node)
                    .is_some_and(|s| s.generation == j.generation)
        });
        if !valid {
            return StatusCode::NOT_FOUND.into_response();
        }
        jobs.remove(&id).unwrap()
    };
    ws.max_message_size(FRAME)
        .max_frame_size(FRAME)
        .write_buffer_size(0)
        .max_write_buffer_size(2 * FRAME)
        .on_upgrade(move |socket| async move {
            let _ = job.tx.send(socket);
        })
}
async fn session_loop(c: Controller, node: String, headers: HeaderMap, mut socket: WebSocket) {
    // Latest authenticated connection wins; prior control and job generations are fenced.
    let generation = common::nonce().unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    {
        let mut sessions = c.sessions.lock().unwrap();
        if sessions.len() >= 16 && !sessions.contains_key(&node) {
            return;
        }
        if let Ok(db) = db(&c.root) {
            let _ = db.execute(
                "UPDATE nodes SET session=?1,heartbeat=?2 WHERE id=?3 AND revoked=0",
                params![generation, 0, node],
            );
        }
        sessions.insert(
            node.clone(),
            Session {
                generation: generation.clone(),
                tx,
            },
        );
    }
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    let mut last = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _=tick.tick()=>{
                let current=c.sessions.lock().unwrap().get(&node).is_some_and(|s|s.generation==generation);
                if !current || last.elapsed()>Duration::from_secs(15) || auth(&c.root,&node,&headers).is_err() {break;}
                if !matches!(tokio::time::timeout(Duration::from_secs(3),socket.send(Message::Ping(vec![].into()))).await,Ok(Ok(()))) {break;}
            }
            msg=socket.recv()=>{
                match msg {Some(Ok(Message::Text(text)))=>{
                    let Ok(v)=serde_json::from_str::<Value>(&text) else {break;};
                    if common::guest_limits(&v).is_err() || v["type"]!="heartbeat" || v["protocol"]!=1 || v["backend"]!="firecracker" || v["arch"]!="x86_64" || v["runtime"]!="firecracker-v1.17.0" || !(256..=common::HOST_MEMORY_MIB as u64).contains(&v["memory_mib"].as_u64().unwrap_or(0)) || !(1..=common::HOST_SLOTS as u64).contains(&v["slots"].as_u64().unwrap_or(0)) || !(1..=common::HOST_CPUS as u64).contains(&v["vcpus"].as_u64().unwrap_or(0)) || !v["images"].as_array().is_some_and(|a|a.len()<=3 && a.iter().all(|i|matches!(i.as_str(),Some("alpine"|"arch"|"ubuntu")))) {break;}
                    last=tokio::time::Instant::now();
                    if !c.root.join("nodes").join(&node).join("control.sock").exists(){continue;}
                    if let Ok(db)=db(&c.root)
                        && let Ok((ram,slots,cpus)) = db.query_row("SELECT memory_mib,slots,COALESCE((SELECT vcpu_cap FROM node_capacity_limits WHERE node=nodes.id),(SELECT vcpu_cap FROM node_limits WHERE node=nodes.id),16) FROM nodes WHERE id=?1",[&node],|r|Ok((r.get::<_,u32>(0)? as u64,r.get::<_,u32>(1)? as u64,r.get::<_,u32>(2)? as u64))) {
                            let cap=json!({"protocol":1,"backend":"firecracker","arch":"x86_64","runtime":"firecracker-v1.17.0","memory_mib":v["memory_mib"].as_u64().unwrap().min(ram),"slots":v["slots"].as_u64().unwrap().min(slots),"vcpus":v["vcpus"].as_u64().unwrap().min(cpus),"images":v["images"],"max_guest_memory_mib":common::guest_limits(&v).unwrap().0,"max_guest_vcpus":common::guest_limits(&v).unwrap().1,"guest_ssh_v1":v["guest_ssh_v1"]==true});
                            if db.execute("UPDATE nodes SET heartbeat=?1,capabilities=?2 WHERE id=?3 AND session=?4 AND revoked=0",params![now(),cap.to_string(),node,generation]).is_ok_and(|n|n==1) {
                                // Old native agents expect job IDs only; acknowledge only the guided agent's optional extension.
                                if v["ready_ack"]==true {let ack=json!({"type":"ready","node":node,"generation":generation}).to_string();if !matches!(tokio::time::timeout(Duration::from_secs(3),socket.send(Message::Text(ack.into()))).await,Ok(Ok(()))) {break;}}
                            }
                    }
                }, Some(Ok(Message::Pong(_)))=>{}, _=>break}
            }
            id=rx.recv()=>{
                let Some(id)=id else {break;};
                if !matches!(tokio::time::timeout(Duration::from_secs(3),socket.send(Message::Text(id.into()))).await,Ok(Ok(()))) {break;}
            }
        }
    }
    {
        let mut sessions = c.sessions.lock().unwrap();
        if sessions
            .get(&node)
            .is_some_and(|s| s.generation == generation)
        {
            sessions.remove(&node);
        }
        c.jobs
            .lock()
            .unwrap()
            .retain(|_, j| j.generation != generation);
    }
    if let Ok(db) = db(&c.root) {
        let _ = db.execute(
            "UPDATE nodes SET heartbeat=0 WHERE id=?1 AND session=?2",
            params![node, generation],
        );
    }
}
async fn proxy(c: Controller, node: String, mut stream: tokio::net::UnixStream) -> Result<()> {
    let _permit = c
        .slots
        .clone()
        .try_acquire_owned()
        .context("controller capacity")?;
    // Only the controller Unix peer may submit commands (700 parent, 600 socket plus peer UID).
    ensure!(
        stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
        "unauthorized proxy peer"
    );
    online(&c.root, &node)?;
    let id = common::nonce()?;
    let (tx, rx) = oneshot::channel();
    let generation;
    {
        let sessions = c.sessions.lock().unwrap();
        let session = sessions.get(&node).context("node disconnected")?;
        generation = session.generation.clone();
        c.jobs.lock().unwrap().insert(
            id.clone(),
            Job {
                node: node.clone(),
                generation: session.generation.clone(),
                tx,
            },
        );
        if session.tx.try_send(id.clone()).is_err() {
            c.jobs.lock().unwrap().remove(&id);
            anyhow::bail!("node dispatch full");
        }
    }
    let result = tokio::time::timeout(Duration::from_secs(10), rx).await;
    c.jobs.lock().unwrap().remove(&id);
    let mut socket = result
        .context("node dispatch timeout")?
        .context("node disconnected")?;
    let (mut reader, mut writer) = stream.split();
    let mut buffer = vec![0; FRAME];
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    loop {
        tokio::select! {
            _=tick.tick()=>{online(&c.root,&node)?;ensure!(c.sessions.lock().unwrap().get(&node).is_some_and(|s|s.generation==generation),"node session replaced");}
            n=reader.read(&mut buffer)=>{
                let n=n?; if n==0 {break;}
                online(&c.root,&node)?;ensure!(c.sessions.lock().unwrap().get(&node).is_some_and(|s|s.generation==generation),"stale node input");
                tokio::time::timeout(Duration::from_secs(5),socket.send(Message::Binary(buffer[..n].to_vec().into()))).await??;
            }
            msg=socket.recv()=>match msg {
                Some(Ok(Message::Binary(bytes)))=>{online(&c.root,&node)?;ensure!(c.sessions.lock().unwrap().get(&node).is_some_and(|s|s.generation==generation),"stale node result");tokio::time::timeout(Duration::from_secs(5),writer.write_all(&bytes)).await??;}
                Some(Ok(Message::Ping(_)|Message::Pong(_)))=>{}, _=>break,
            }
        }
    }
    Ok(())
}
async fn proxies(c: Controller) -> Result<()> {
    let mut bound = HashMap::new();
    loop {
        let entries = inventory(&c.root)?;
        for node in entries.as_array().unwrap() {
            let node = node["id"].as_str().unwrap();
            if bound.contains_key(node) {
                continue;
            }
            let dir = c.root.join("nodes").join(node);
            fs::create_dir_all(&dir)?;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
            let path = dir.join("control.sock");
            if path.exists() {
                fs::remove_file(&path)?;
            }
            let listener = tokio::net::UnixListener::bind(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            let child = c.clone();
            let node = node.to_owned();
            bound.insert(node.clone(), true);
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let child = child.clone();
                    let node = node.clone();
                    tokio::spawn(async move {
                        let _ = proxy(child, node, stream).await;
                    });
                }
            });
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
/// Hold an admission permit for the entire TCP/TLS/HTTP/upgraded connection.
struct BoundedIo<T> {
    inner: T,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for BoundedIo<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}
impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for BoundedIo<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
#[derive(Clone)]
struct BoundedAccept<A> {
    inner: A,
    slots: Arc<Semaphore>,
}
impl<A, S> axum_server::accept::Accept<tokio::net::TcpStream, S> for BoundedAccept<A>
where
    A: axum_server::accept::Accept<tokio::net::TcpStream, S> + Clone + Send + 'static,
    A::Future: Send,
    A::Stream: Send,
    S: Send + 'static,
    A::Service: Send,
{
    type Stream = BoundedIo<A::Stream>;
    type Service = A::Service;
    type Future = std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::io::Result<(Self::Stream, Self::Service)>> + Send,
        >,
    >;
    fn accept(&self, stream: tokio::net::TcpStream, service: S) -> Self::Future {
        let permit = self.slots.clone().try_acquire_owned();
        let inner = self.inner.clone();
        Box::pin(async move {
            let permit = permit.map_err(|_| std::io::Error::other("node connection capacity"))?;
            let (inner, service) = inner.accept(stream, service).await?;
            Ok((
                BoundedIo {
                    inner,
                    _permit: permit,
                },
                service,
            ))
        })
    }
}
fn routes(c: Controller) -> Router {
    Router::new()
        .route("/_nodes/enroll", post(enrollment))
        .route("/_nodes/node/{node}", get(session))
        .route("/_nodes/job/{node}/{id}", get(job))
        .route("/enroll", post(enrollment))
        .route("/node/{node}", get(session))
        .route("/job/{node}/{id}", get(job))
        .layer(axum::extract::DefaultBodyLimit::max(4096))
        .layer(axum::middleware::from_fn(
            |request: axum::extract::Request, next: axum::middleware::Next| async move {
                let path = request.uri().path();
                if request.uri().query().is_some()
                    || path.contains('%')
                    || path.contains("..")
                    || request.uri().authority().is_some()
                {
                    return StatusCode::BAD_REQUEST.into_response();
                }
                tokio::time::timeout(Duration::from_secs(10), next.run(request))
                    .await
                    .unwrap_or_else(|_| StatusCode::REQUEST_TIMEOUT.into_response())
            },
        ))
        .with_state(c)
}
pub fn controller(
    root: &Path,
    listen: SocketAddr,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
    insecure: bool,
) -> Result<()> {
    ensure!(
        (cert.is_some() && key.is_some() && !insecure)
            || (cert.is_none() && key.is_none() && insecure && listen.ip().is_loopback()),
        "provide TLS cert/key, or explicit insecure loopback test mode"
    );
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join("node-controller.lock"))?;
    use std::os::fd::AsRawFd;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "controller already running"
    );
    db(root)?.execute("UPDATE nodes SET heartbeat=0,session=NULL", [])?;
    let c = Controller {
        root: root.into(),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        jobs: Arc::new(Mutex::new(HashMap::new())),
        slots: Arc::new(Semaphore::new(32)),
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let routes = routes(c.clone());
        tokio::spawn(proxies(c));
        let slots = Arc::new(Semaphore::new(128));
        if let (Some(cert), Some(key)) = (cert, key) {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await?;
            let mut server = axum_server::bind_rustls(listen, config)
                .map(|inner| BoundedAccept { inner, slots });
            server
                .http_builder()
                .http1()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .max_buf_size(16384);
            server.serve(routes.into_make_service()).await?;
        } else {
            let mut server = axum_server::bind(listen).map(|inner| BoundedAccept { inner, slots });
            server
                .http_builder()
                .http1()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .max_buf_size(16384);
            server.serve(routes.into_make_service()).await?;
        }
        Ok::<_, anyhow::Error>(())
    })
}

pub(crate) fn origin(value: &str, insecure: bool) -> Result<String> {
    let u = reqwest::Url::parse(value)?;
    let loopback = u.host_str().is_some_and(|h| {
        h == "localhost"
            || h.parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    ensure!(
        (u.scheme() == "https" || (insecure && u.scheme() == "http" && loopback))
            && matches!(u.path(), "/" | "/_nodes" | "/_nodes/")
            && u.host_str().is_some()
            && u.query().is_none()
            && u.fragment().is_none()
            && u.username().is_empty()
            && u.password().is_none(),
        "controller requires HTTPS origin; insecure test permits loopback only"
    );
    Ok(value.trim_end_matches('/').to_owned())
}
fn socket(
    origin: &str,
    path: &str,
    credential: &str,
    ca: Option<&Path>,
) -> Result<tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>> {
    use tungstenite::client::IntoClientRequest;
    let url = format!(
        "{}{path}",
        origin
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1)
    );
    let mut request = url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {credential}").parse()?);
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(FRAME))
        .max_frame_size(Some(FRAME))
        .write_buffer_size(0)
        .max_write_buffer_size(2 * FRAME);
    let connector = if let Some(ca) = ca {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut roots = rustls::RootCertStore::empty();
        // reqwest parses PEM certificates; rustls needs DER. Use the existing PEM base64 engine.
        use base64::Engine;
        let pem = fs::read_to_string(ca)?;
        let der = pem
            .split("-----BEGIN CERTIFICATE-----")
            .nth(1)
            .context("CA PEM required")?
            .split("-----END CERTIFICATE-----")
            .next()
            .unwrap()
            .split_whitespace()
            .collect::<String>();
        roots.add(rustls::pki_types::CertificateDer::from(
            base64::engine::general_purpose::STANDARD.decode(der)?,
        ))?;
        Some(tungstenite::Connector::Rustls(Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )))
    } else {
        None
    };
    // Resolve and connect under bounded deadlines before sending the bearer header.
    use std::net::ToSocketAddrs;
    static RESOLVERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    use std::sync::atomic::Ordering;
    RESOLVERS
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < 4).then_some(n + 1)
        })
        .map_err(|_| anyhow::anyhow!("DNS resolver capacity"))?;
    let u = reqwest::Url::parse(&url)?;
    let host = u.host_str().context("host")?.to_owned();
    let port = u.port_or_known_default().context("port")?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = (host.as_str(), port)
            .to_socket_addrs()
            .map(|a| a.collect::<Vec<_>>());
        let _ = tx.send(result);
        RESOLVERS.fetch_sub(1, Ordering::SeqCst);
    });
    let addresses = rx
        .recv_timeout(Duration::from_secs(5))
        .context("DNS timeout")??;
    let start = std::time::Instant::now();
    let mut connected = None;
    for address in addresses {
        let remaining = Duration::from_secs(5).saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        if let Ok(tcp) = std::net::TcpStream::connect_timeout(&address, remaining) {
            connected = Some(tcp);
            break;
        }
    }
    let tcp = connected.context("node connect failed or timed out")?;
    tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(5)))?;
    // Shutdown watchdog bounds a slow-drip TLS/HTTP handshake as well as an idle peer.
    let watchdog = tcp.try_clone()?;
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if matches!(
            done_rx.recv_timeout(Duration::from_secs(10)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ) {
            let _ = watchdog.shutdown(std::net::Shutdown::Both);
        }
    });
    let result = tungstenite::client_tls_with_config(request, tcp, Some(config), connector);
    let _ = done_tx.send(());
    Ok(result?.0)
}
fn timeout_socket(
    socket: &tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
) -> Result<()> {
    let tcp = match socket.get_ref() {
        tungstenite::stream::MaybeTlsStream::Plain(t) => t,
        tungstenite::stream::MaybeTlsStream::Rustls(t) => &t.sock,
        _ => anyhow::bail!("unsupported transport"),
    };
    tcp.set_read_timeout(Some(Duration::from_millis(100)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(())
}
fn timed_out(error: &tungstenite::Error) -> bool {
    matches!(error,tungstenite::Error::Io(e) if matches!(e.kind(),std::io::ErrorKind::WouldBlock|std::io::ErrorKind::TimedOut))
}

fn channel_status(
    root: &Path,
    state: &str,
    instance: &str,
    generation: Option<&str>,
) -> Result<()> {
    let temporary = root.join(format!("channel-{}.tmp", common::nonce()?));
    let node = fs::read(root.join("node.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .map(|v| v["node"].clone())
        .unwrap_or(Value::Null);
    write_private(
        &temporary,
        &json!({"state":state,"updated":now(),"instance":instance,"generation":generation,"node":node}),
    )?;
    fs::rename(&temporary, root.join("node-channel.json"))?;
    Ok(())
}
pub(crate) fn redeem(origin: &str, value: &Value, ca: Option<&Path>) -> Result<Value> {
    let mut builder = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none());
    if let Some(ca) = ca {
        builder = builder.add_root_certificate(reqwest::Certificate::from_pem(&fs::read(ca)?)?);
    }
    let response = builder
        .build()?
        .post(format!("{origin}/enroll"))
        .json(value)
        .send()?;
    ensure!(
        response.status().is_success(),
        "enrollment rejected; if response was lost, ask owner to revoke and invite a new node ID"
    );
    let mut bytes = Vec::new();
    response.take(4097).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4096, "enrollment response too large");
    let result: Value = serde_json::from_slice(&bytes)?;
    common::identifier(result["node"].as_str().context("node required")?)?;
    ensure!(
        result["node"] == value["node"]
            && result["credential"]
                .as_str()
                .is_some_and(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit())),
        "invalid enrollment response"
    );
    Ok(result)
}

pub fn agent(
    root: &Path,
    controller: &str,
    credential_file: &Path,
    join: Option<&Path>,
    ca: Option<&Path>,
    insecure: bool,
) -> Result<()> {
    let origin = origin(controller, insecure)?;
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join("node-agent.lock"))?;
    use std::os::fd::AsRawFd;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "node agent already running"
    );
    let instance = common::nonce()?;
    let mut lock = &lock;
    let start = fs::read_to_string("/proc/self/stat")?
        .rsplit_once(')')
        .context("process stat")?
        .1
        .split_whitespace()
        .nth(19)
        .context("process start time")?
        .to_owned();
    lock.set_len(0)?;
    lock.write_all(
        json!({"pid":std::process::id(),"start":start,"instance":instance})
            .to_string()
            .as_bytes(),
    )?;
    lock.sync_all()?;
    struct ChannelGuard<'a> {
        root: &'a Path,
        instance: &'a str,
    }
    impl Drop for ChannelGuard<'_> {
        fn drop(&mut self) {
            let _ = channel_status(self.root, "stopped", self.instance, None);
        }
    }
    let _channel_guard = ChannelGuard {
        root,
        instance: &instance,
    };
    if !credential_file.exists() {
        let join = join.context("join file required for first enrollment")?;
        private(join)?;
        let value: Value = serde_json::from_slice(&fs::read(join)?)?;
        let credentials = redeem(&origin, &value, ca)?;
        write_private(credential_file, &credentials)?;
        fs::remove_file(join)?;
    }
    private(credential_file)?;
    let value: Value = serde_json::from_slice(&fs::read(credential_file)?)?;
    let node = value["node"].as_str().context("node")?.to_owned();
    common::identifier(&node)?;
    let credential = value["credential"]
        .as_str()
        .context("credential")?
        .to_owned();
    ensure!(credential.len() == 64, "invalid credential file");
    let status = wire::request(root, json!({"op":"status"}))?;
    let guided = root.join("host.json").exists();
    let images = if guided {
        crate::onboarding::verified_images(root)?
    } else {
        // Legacy unmanaged nodes retain their separately configured asset contract.
        ["alpine", "arch", "ubuntu"]
            .into_iter()
            .filter(|image| {
                crate::assets()
                    .join(match *image {
                        "alpine" => "guest/base.ext4",
                        "arch" => "guest/arch.ext4",
                        _ => "guest/ubuntu.ext4",
                    })
                    .exists()
            })
            .collect::<Vec<_>>()
    };
    let (guest_ram, guest_cpus) = common::guest_limits(&status)?;
    let hello=json!({"type":"heartbeat","ready_ack":guided,"protocol":1,"backend":"firecracker","arch":std::env::consts::ARCH,"runtime":"firecracker-v1.17.0","memory_mib":status["max_memory_mib"],"slots":status["max_running"],"vcpus":status["max_vcpus"],"images":images,"max_guest_memory_mib":guest_ram,"max_guest_vcpus":guest_cpus,"guest_ssh_v1":status["guest_ssh_v1"]==true}).to_string();
    // Jobs are bounded independently of reconnects; no unbounded thread/job queues.
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    loop {
        wire::request(root, json!({"op":"status"}))
            .context("local worker stopped; host participation ended")?;
        let result = (|| -> Result<()> {
            if guided {
                channel_status(root, "connecting", &instance, None)?;
            }
            let mut ws = socket(&origin, &format!("/node/{node}"), &credential, ca)?;
            timeout_socket(&ws)?;
            let mut heartbeat = std::time::Instant::now() - Duration::from_secs(5);
            loop {
                if heartbeat.elapsed() > Duration::from_secs(3) {
                    wire::request(root, json!({"op":"status"})).context("local worker stopped")?;
                    ws.send(tungstenite::Message::Text(hello.clone().into()))?;
                    heartbeat = std::time::Instant::now();
                }
                match ws.read() {
                    Ok(tungstenite::Message::Text(id)) => {
                        if guided && let Ok(v) = serde_json::from_str::<Value>(&id) {
                            ensure!(
                                v["type"] == "ready" && v["node"] == node,
                                "invalid controller acknowledgement"
                            );
                            let generation = v["generation"]
                                .as_str()
                                .context("acknowledgement generation required")?;
                            common::identifier(generation)?;
                            channel_status(root, "dispatchable", &instance, Some(generation))?;
                            continue;
                        }
                        common::identifier(&id)?;
                        use std::sync::atomic::Ordering;
                        if active
                            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                                (n < 16).then_some(n + 1)
                            })
                            .is_err()
                        {
                            continue;
                        }
                        let (root, origin, node, credential, ca, active) = (
                            root.to_owned(),
                            origin.clone(),
                            node.clone(),
                            credential.clone(),
                            ca.map(Path::to_owned),
                            active.clone(),
                        );
                        std::thread::spawn(move || {
                            let _ =
                                agent_job(&root, &origin, &node, &id, &credential, ca.as_deref());
                            active.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                    Ok(tungstenite::Message::Ping(v)) => ws.send(tungstenite::Message::Pong(v))?,
                    Ok(tungstenite::Message::Pong(_)) => {}
                    Err(e) if timed_out(&e) => {}
                    _ => anyhow::bail!("node channel closed"),
                }
            }
        })();
        if result.is_err() {
            if guided {
                let _ = channel_status(root, "disconnected", &instance, None);
            }
            eprintln!("node channel disconnected; reconnecting");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}
fn agent_job(
    root: &Path,
    origin: &str,
    node: &str,
    id: &str,
    credential: &str,
    ca: Option<&Path>,
) -> Result<()> {
    let mut socket = socket(origin, &format!("/job/{node}/{id}"), credential, ca)?;
    timeout_socket(&socket)?;
    // Read only a bounded, complete control request before touching the worker.
    let mut bytes = Vec::new();
    let start = std::time::Instant::now();
    while !bytes.contains(&b'\n') {
        ensure!(start.elapsed() < Duration::from_secs(10), "request timeout");
        match socket.read() {
            Ok(tungstenite::Message::Binary(b)) => {
                ensure!(bytes.len() + b.len() <= 1024 * 1024, "request too large");
                bytes.extend_from_slice(&b);
            }
            Err(e) if timed_out(&e) => {}
            _ => anyhow::bail!("invalid job request"),
        }
    }
    let end = bytes.iter().position(|b| *b == b'\n').unwrap();
    ensure!(end + 1 == bytes.len(), "unexpected request suffix");
    let request: Value = serde_json::from_slice(&bytes[..end])?;
    let op = request["op"].as_str().context("op")?;
    ensure!(
        [
            "status",
            "list",
            "stats",
            "snapshots",
            "inspect",
            "create",
            "resize",
            "start",
            "stop",
            "exec",
            "put",
            "get",
            "snapshot",
            "fork",
            "restore",
            "hibernate",
            "terminal",
            "ssh",
            "ssh-info",
            "ssh-keys"
        ]
        .contains(&op),
        "unsupported node operation"
    );
    if op != "terminal" && op != "ssh" {
        let response = journal_request(root, &request)?;
        let data = serde_json::to_vec(&response)?;
        ensure!(data.len() < 1024 * 1024, "response too large");
        for chunk in data.chunks(FRAME) {
            socket.send(tungstenite::Message::Binary(chunk.to_vec().into()))?;
        }
        socket.send(tungstenite::Message::Binary(vec![b'\n'].into()))?;
        let _ = socket.close(None);
        return Ok(());
    }
    if op == "ssh" {
        ensure!(
            request
                .as_object()
                .is_some_and(|o| o.len() == 2 && o.contains_key("id")),
            "invalid SSH stream request"
        );
        common::identifier(request["id"].as_str().context("SSH ID")?)?;
    }
    let (mut unix, response) = wire::connect(root, &request)?;
    socket.send(tungstenite::Message::Binary(
        format!("{}\n", json!({"ok":true,"result":response}))
            .into_bytes()
            .into(),
    ))?;
    unix.set_read_timeout(Some(Duration::from_millis(1)))?;
    unix.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut buffer = [0; 4096];
    let mut last = std::time::Instant::now();
    loop {
        ensure!(
            last.elapsed() < Duration::from_secs(90),
            "terminal transport idle"
        );
        match socket.read() {
            Ok(tungstenite::Message::Binary(b)) => {
                unix.write_all(&b)?;
                last = std::time::Instant::now();
            }
            Ok(tungstenite::Message::Ping(v)) => socket.send(tungstenite::Message::Pong(v))?,
            Err(e) if timed_out(&e) => {}
            _ => break,
        }
        match unix.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                socket.send(tungstenite::Message::Binary(buffer[..n].to_vec().into()))?;
                last = std::time::Instant::now();
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
// Persist-before-execute: crash-pending effects are not silently repeated. Stable create/fork IDs
// may be reconciled by the runtime's existing parameter/lineage checks; arbitrary exec is uncertain.
fn journal_request(root: &Path, request: &Value) -> Result<Value> {
    let Some(key) = request["operation_key"].as_str() else {
        return Ok(wire::response(wire::request(root, request.clone())));
    };
    common::identifier(key)?;
    let path = root.join("node-operations.sqlite3");
    if !path.exists() {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    fs::OpenOptions::new().write(true).open(&path)
                } else {
                    Err(e)
                }
            })?;
    }
    private(&path)?;
    let mut db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS effects(key TEXT PRIMARY KEY,request TEXT NOT NULL,response TEXT);")?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let previous: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT request,response FROM effects WHERE key=?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((previous, response)) = previous {
        ensure!(
            previous == request.to_string(),
            "operation key reused with different request"
        );
        if let Some(response) = response {
            let v: Value = serde_json::from_str(&response)?;
            if v["uncertain"] != true {
                return Ok(v);
            }
        }
        if !matches!(request["op"].as_str(), Some("create" | "fork" | "snapshot")) {
            return Ok(
                json!({"ok":false,"error":"uncertain node outcome; operator reconciliation required","uncertain":true}),
            );
        }
        tx.commit()?;
        // These operations use fixed IDs and the immutable request fingerprint; the runtime
        // checks shape/source and returns an existing artifact instead of duplicating it.
        let response = match wire::request_envelope(root, request) {
            Ok(v) => v,
            Err(_) => {
                return Ok(
                    json!({"ok":false,"uncertain":true,"error":"uncertain runtime outcome"}),
                );
            }
        };
        db.execute(
            "UPDATE effects SET response=?1 WHERE key=?2",
            params![response.to_string(), key],
        )?;
        return Ok(response);
    }
    tx.execute(
        "INSERT INTO effects(key,request) VALUES(?1,?2)",
        params![key, request.to_string()],
    )?;
    tx.commit()?;
    let response = match wire::request_envelope(root, request) {
        Ok(v) => v,
        Err(e) => json!({"ok":false,"error":format!("{e:#}"),"uncertain":true}),
    };
    db.execute(
        "UPDATE effects SET response=?1 WHERE key=?2",
        params![response.to_string(), key],
    )?;
    Ok(response)
}

#[cfg(test)]
#[path = "node_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "node_e2e.rs"]
mod real_tests;
