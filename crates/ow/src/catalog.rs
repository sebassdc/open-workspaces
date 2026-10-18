//! Durable control-plane ownership. The private worker journal remains runtime recovery state.
use crate::{common, nodes, wire};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[derive(Clone)]
pub struct Identity {
    pub issuer: String,
    pub subject: String,
    pub email: String,
}
pub struct Catalog {
    db: Connection,
    root: PathBuf,
    _lock: std::fs::File,
}

#[derive(Debug)]
pub struct OperationFailure(pub Value);
impl OperationFailure {
    pub fn message(&self) -> &str {
        self.0["error"].as_str().unwrap_or(
            "Operation did not complete. Check its status before retrying; use the same retry key.",
        )
    }
}
impl std::fmt::Display for OperationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Workspace operation {} is {}: {}",
            self.0["id"],
            self.0["state"],
            self.message()
        )
    }
}
impl std::error::Error for OperationFailure {}

/// Only known diagnostics cross the human API boundary. Raw errors stay in the private
/// operation response and host journal, where the operation ID makes them discoverable.
fn public_operation_error<'a>(op: &str, diagnostic: &'a str) -> &'a str {
    match (op, diagnostic) {
        ("delete", "workspace is running; stop or hibernate it first") => {
            "Box is running. Stop the box first with ow stop, then retry ow delete with a new operation key."
        }
        ("ssh-info", d) if d.starts_with("SSH not enrolled; use ow ssh-authorize:") => {
            "SSH has no registered key. Run ow ssh-authorize BOX --key PATH_TO_PUBLIC_KEY, then retry ssh-info."
        }
        (_, "storage headroom exhausted (full disk reservations plus host reserve)") => diagnostic,
        (_, d)
            if d.strip_prefix("storage headroom exhausted: ")
                .and_then(|s| s.split_once(" bytes available, "))
                .is_some_and(|(a, b)| {
                    a.parse::<u64>().is_ok()
                        && b.strip_suffix(" bytes required")
                            .is_some_and(|s| s.parse::<u64>().is_ok())
                }) =>
        {
            diagnostic
        }
        (_, "snapshot limit (24) reached") => {
            "Named snapshot limit (24) reached. Remove unneeded snapshots on the host before retrying with a new operation key."
        }
        (_, "incomplete snapshot exists; preserve and inspect it") => diagnostic,
        (_, "node route unavailable before dispatch") => {
            "Node is unavailable; no operation was dispatched. Retry with the same operation key when it reconnects."
        }
        _ => {
            "Operation did not complete. Check its status before retrying; use the same retry key. The host journal records the cause under this operation ID."
        }
    }
}

/// Open gateway streams (terminal, SSH) by user and machine, so unsharing closes them at once.
type Sessions = Vec<(i64, String, std::sync::Weak<tokio::sync::Notify>)>;
static SESSIONS: std::sync::Mutex<Sessions> = std::sync::Mutex::new(Vec::new());
fn session(user: i64, physical: &str) -> Arc<tokio::sync::Notify> {
    let notify = Arc::new(tokio::sync::Notify::new());
    let mut sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    sessions.retain(|(_, _, s)| s.strong_count() > 0);
    sessions.push((user, physical.into(), Arc::downgrade(&notify)));
    notify
}
fn cut(user: i64, physical: &str) {
    let sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    for (u, p, s) in sessions.iter() {
        if *u == user && p == physical {
            // notify_one keeps a permit, so a stream that is not polling yet still closes.
            s.upgrade().inspect(|s| s.notify_one());
        }
    }
}

/// Bind a verified identity to its user row (by subject, or a reserved email) and return its ID.
fn user_on(db: &mut Connection, identity: &Identity) -> Result<i64> {
    ensure!(
        !identity.subject.is_empty()
            && identity.subject.len() <= 512
            && identity.email.len() <= 254,
        "invalid identity"
    );
    let email = identity.email.to_ascii_lowercase();
    let tx = db.transaction()?;
    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM users WHERE issuer=?1 AND subject=?2",
            params![identity.issuer, identity.subject],
            |r| r.get(0),
        )
        .optional()?;
    let id = if let Some(id) = existing {
        id
    } else {
        let reserved: Option<(i64, Option<String>)> = tx
            .query_row(
                "SELECT id,subject FROM users WHERE issuer=?1 AND email=?2",
                params![identity.issuer, email],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((id, subject)) = reserved {
            ensure!(subject.is_none(), "email already bound to another subject");
            tx.execute(
                "UPDATE users SET subject=?1 WHERE id=?2 AND subject IS NULL",
                params![identity.subject, id],
            )?;
            id
        } else {
            tx.execute(
                "INSERT INTO users(issuer,subject,email) VALUES(?1,?2,?3)",
                params![identity.issuer, identity.subject, email],
            )?;
            tx.last_insert_rowid()
        }
    };
    tx.commit()?;
    Ok(id)
}
fn shared_with(db: &Connection, physical: &str) -> Result<Value> {
    let mut stmt = db.prepare("SELECT u.email,g.role FROM grants g JOIN users u ON u.id=g.grantee WHERE g.physical=?1 ORDER BY u.email")?;
    let rows = stmt.query_map([physical], |r| {
        Ok(json!({"email":r.get::<_, String>(0)?,"role":r.get::<_, String>(1)?}))
    })?;
    Ok(json!(rows.collect::<rusqlite::Result<Vec<_>>>()?))
}
/// `share` grants a teammate (by login email) a role on an owned machine; `unshare` removes
/// it and closes their open terminal and SSH streams. Machines they forked stay theirs.
fn share_on(db: &mut Connection, user: i64, op: &str, request: &Value) -> Result<Value> {
    let name = request["id"].as_str().context("id")?;
    common::identifier(name)?;
    let physical: String = db
        .query_row(
            "SELECT physical FROM resources WHERE owner=?1 AND kind='machine' AND name=?2",
            params![user, name],
            |r| r.get(0),
        )
        .optional()?
        .context("resource not found")?;
    let email = request["with"]
        .as_str()
        .context("teammate email required")?
        .to_ascii_lowercase();
    common::email(&email)?;
    let tx = db.transaction()?;
    let issuer: String =
        tx.query_row("SELECT issuer FROM users WHERE id=?1", [user], |r| r.get(0))?;
    if op == "share" {
        // An email nobody has logged in with yet is reserved; its first verified login binds it.
        tx.execute(
            "INSERT INTO users(issuer,email) VALUES(?1,?2) ON CONFLICT(issuer,email) DO NOTHING",
            params![issuer, email],
        )?;
    }
    let grantee: Option<i64> = tx
        .query_row(
            "SELECT id FROM users WHERE issuer=?1 AND email=?2",
            params![issuer, email],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(grantee != Some(user), "you already own this machine");
    if let Some(grantee) = grantee {
        if op == "share" {
            let role = request["role"].as_str().unwrap_or("use");
            ensure!(matches!(role, "use" | "admin"), "role must be use or admin");
            tx.execute("INSERT INTO grants(physical,grantee,role) VALUES(?1,?2,?3) ON CONFLICT(physical,grantee) DO UPDATE SET role=excluded.role",params![physical,grantee,role])?;
        } else {
            tx.execute(
                "DELETE FROM grants WHERE physical=?1 AND grantee=?2",
                params![physical, grantee],
            )?;
        }
    }
    tx.commit()?;
    if op == "unshare"
        && let Some(grantee) = grantee
    {
        cut(grantee, &physical);
    }
    Ok(json!({"id":name,"shared_with":shared_with(db, &physical)?}))
}
/// Share or unshare on a connection of its own. The gateway's catalog lock is held across
/// worker calls, and revoking access must not wait behind a slow request it revokes.
pub fn share_unlocked(root: &Path, identity: &Identity, request: &Value) -> Result<Value> {
    let op = request["op"].as_str().context("op")?;
    ensure!(matches!(op, "share" | "unshare"), "not a sharing operation");
    let path = root.join("catalog.sqlite3");
    nodes::private(&path)?;
    let mut db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")?;
    let user = user_on(&mut db, identity)?;
    share_on(&mut db, user, op, request)
}

/// What a grantee may do on a machine shared with them. Resize, delete and sharing stay with
/// the owner.
fn permitted(role: &str, op: &str) -> bool {
    match role {
        "owner" => true,
        "admin" => !matches!(op, "resize" | "delete"),
        "use" => matches!(
            op,
            "exec"
                | "put"
                | "get"
                | "preview"
                | "terminal"
                | "ssh"
                | "ssh-info"
                | "start"
                | "hibernate"
                | "fork"
        ),
        _ => false,
    }
}

impl Catalog {
    pub fn open(root: &Path, issuer: &str, bootstrap: Option<&str>) -> Result<Self> {
        use std::os::fd::AsRawFd;
        let lock_path = root.join("catalog-gateway.lock");
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)?;
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "another gateway owns this catalog"
        );
        let path = root.join("catalog.sqlite3");
        if !path.exists() {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
        }
        let meta = std::fs::symlink_metadata(&path)?;
        ensure!(
            meta.is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
            "catalog must be an owned private regular file"
        );
        let db = Connection::open(&path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;",
        )?;
        crate::nodes::readable(&db)?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        db.execute_batch("BEGIN IMMEDIATE;
          CREATE TABLE IF NOT EXISTS users(id INTEGER PRIMARY KEY, issuer TEXT NOT NULL, subject TEXT, email TEXT NOT NULL, UNIQUE(issuer,subject), UNIQUE(issuer,email));
          CREATE TABLE IF NOT EXISTS resources(owner INTEGER NOT NULL REFERENCES users(id), kind TEXT NOT NULL CHECK(kind IN ('machine','snapshot')), name TEXT NOT NULL, physical TEXT NOT NULL, metadata TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY(owner,kind,name), UNIQUE(kind,physical));
          CREATE TABLE IF NOT EXISTS operations(id INTEGER PRIMARY KEY, owner INTEGER NOT NULL REFERENCES users(id), op TEXT NOT NULL, resource TEXT NOT NULL, state TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, finished_at TEXT);
          CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
          -- Additive, like node_usage: an older binary ignores grants, so grantees lose access.
          -- Never reassign an origin to another box/port: browser storage and service workers persist.
          CREATE TABLE IF NOT EXISTS publications(https_port INTEGER PRIMARY KEY CHECK(https_port BETWEEN 11000 AND 65535),physical TEXT NOT NULL,guest_port INTEGER NOT NULL CHECK(guest_port BETWEEN 1 AND 65535),active INTEGER NOT NULL DEFAULT 0,UNIQUE(physical,guest_port));
          CREATE TABLE IF NOT EXISTS grants(physical TEXT NOT NULL, grantee INTEGER NOT NULL REFERENCES users(id), role TEXT NOT NULL CHECK(role IN ('use','admin')), created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY(physical,grantee));
          ")?;
        if version == 1 {
            // Machine and snapshot IDs occupy separate worker directories and may match.
            db.execute_batch("CREATE TABLE resources_v2(owner INTEGER NOT NULL REFERENCES users(id), kind TEXT NOT NULL CHECK(kind IN ('machine','snapshot')), name TEXT NOT NULL, physical TEXT NOT NULL, metadata TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY(owner,kind,name), UNIQUE(kind,physical));
              INSERT INTO resources_v2 SELECT * FROM resources;
              DROP TABLE resources;
              ALTER TABLE resources_v2 RENAME TO resources;
")?;
        }
        if version < 3 {
            db.execute_batch(
                "ALTER TABLE resources ADD COLUMN node TEXT NOT NULL DEFAULT 'local';
              ALTER TABLE resources ADD COLUMN reserved_mib INTEGER NOT NULL DEFAULT 0;
              ALTER TABLE resources ADD COLUMN reserved_cpus INTEGER NOT NULL DEFAULT 0;
              ALTER TABLE operations ADD COLUMN node TEXT NOT NULL DEFAULT 'local';
              ALTER TABLE operations ADD COLUMN retry_key TEXT;
              ALTER TABLE operations ADD COLUMN request TEXT;
              ALTER TABLE operations ADD COLUMN response TEXT;
              CREATE UNIQUE INDEX operation_retry ON operations(owner,retry_key);",
            )?;
        }
        // A newer catalog this binary may read (its floor allows it) keeps its version.
        db.execute_batch(&format!("UPDATE resources SET reserved_mib=COALESCE(json_extract(metadata,'$.memory_mib'),0) WHERE kind='machine' AND reserved_mib=0 AND json_extract(metadata,'$.state')='running'; PRAGMA user_version={}; COMMIT;", version.max(nodes::SCHEMA)))?;
        nodes::db(root)?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS node_usage(node TEXT PRIMARY KEY,extra_mib INTEGER NOT NULL DEFAULT 0,extra_cpus INTEGER NOT NULL DEFAULT 0,extra_slots INTEGER NOT NULL DEFAULT 0);")?;
        // The exclusive gateway lock proves the previous catalog owner is gone.
        db.execute(
            "UPDATE operations SET state='uncertain' WHERE state='pending'",
            [],
        )?;
        // A persisted pending effect may have reached the worker before this process died.

        let mut catalog = Self {
            db,
            root: root.into(),
            _lock: lock,
        };
        let migrated: bool = catalog.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM settings WHERE key='legacy_import')",
            [],
            |r| r.get(0),
        )?;
        if !migrated {
            let runtime_data = root.join("state.json").exists()
                || root
                    .join("machines")
                    .read_dir()
                    .is_ok_and(|mut entries| entries.next().is_some())
                || root
                    .join("snapshots")
                    .read_dir()
                    .is_ok_and(|mut entries| entries.next().is_some());
            ensure!(
                root.join("control.sock").exists() || !runtime_data,
                "legacy runtime data requires a running local worker and explicit bootstrap owner; import has not completed"
            );
            let machines = if root.join("control.sock").exists() {
                wire::request(root, json!({"op":"list"}))?
            } else {
                json!([])
            };
            let snapshots = if root.join("control.sock").exists() {
                wire::request(root, json!({"op":"snapshots"}))?
            } else {
                json!([])
            };
            let machines = machines.as_array().context("invalid worker inventory")?;
            ensure!(
                machines.is_empty() || bootstrap.is_some(),
                "existing machines require an explicit bootstrap_owner_email"
            );
            let tx = catalog.db.transaction()?;
            if let Some(email) = bootstrap {
                tx.execute(
                    "INSERT INTO users(issuer,email) VALUES(?1,?2)",
                    params![issuer, email.to_ascii_lowercase()],
                )?;
                let owner = tx.last_insert_rowid();
                for (kind, values, key) in [
                    ("machine", machines, "id"),
                    (
                        "snapshot",
                        snapshots.as_array().context("invalid snapshots")?,
                        "name",
                    ),
                ] {
                    for item in values {
                        let name = item[key].as_str().context("invalid legacy name")?;
                        tx.execute("INSERT INTO resources(owner,kind,name,physical,metadata) VALUES(?1,?2,?3,?3,?4)",params![owner,kind,name,item.to_string()])?;
                    }
                }
            }
            tx.execute(
                "INSERT INTO settings(key,value) VALUES('legacy_import','complete')",
                [],
            )?;
            tx.commit()?;
        }
        catalog.db.execute("UPDATE resources SET reserved_mib=MAX(reserved_mib,COALESCE(json_extract(metadata,'$.memory_mib'),0)),reserved_cpus=MAX(reserved_cpus,COALESCE(json_extract(metadata,'$.vcpu_count'),1)) WHERE kind='machine' AND json_extract(metadata,'$.state')='running'",[])?;
        Ok(catalog)
    }

    pub(crate) fn bound_identity(&self, id: i64, identity: &Identity) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM users WHERE id=?1 AND issuer=?2 AND subject=?3)",
            params![id, identity.issuer, identity.subject],
            |r| r.get(0),
        )?)
    }
    pub fn user(&mut self, identity: &Identity) -> Result<i64> {
        user_on(&mut self.db, identity)
    }

    fn physical(&self, user: i64, kind: &str, name: &str) -> Result<String> {
        common::identifier(name)?;
        self.db
            .query_row(
                "SELECT physical FROM resources WHERE owner=?1 AND kind=?2 AND name=?3",
                params![user, kind, name],
                |r| r.get(0),
            )
            .optional()?
            .context("resource not found")
    }
    fn logical(&self, user: i64, kind: &str, physical: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row(
                "SELECT CASE WHEN r.owner=?1 THEN r.name ELSE u.email||'/'||r.name END FROM resources r JOIN users u ON u.id=r.owner WHERE r.kind=?2 AND r.physical=?3 AND (r.owner=?1 OR (r.kind='machine' AND EXISTS(SELECT 1 FROM grants g WHERE g.physical=r.physical AND g.grantee=?1)))",
                params![user, kind, physical],
                |r| r.get(0),
            )
            .optional()?)
    }
    /// Physical ID of a machine the user owns (`name`) or that was shared with them
    /// (`owner@example.com/name`), when their role allows `op`.
    fn machine(&self, user: i64, address: &str, op: &str) -> Result<String> {
        let Some((owner, name)) = address.split_once('/') else {
            return self.physical(user, "machine", address);
        };
        common::address(address)?;
        let (physical, role): (String, String) = self
            .db
            .query_row(
                "SELECT r.physical, CASE WHEN r.owner=?1 THEN 'owner' ELSE g.role END FROM resources r JOIN users u ON u.id=r.owner LEFT JOIN grants g ON g.physical=r.physical AND g.grantee=?1 WHERE r.kind='machine' AND u.issuer=(SELECT issuer FROM users WHERE id=?1) AND u.email=?2 AND r.name=?3 AND (r.owner=?1 OR g.role IS NOT NULL)",
                params![user, owner.to_ascii_lowercase(), name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .context("resource not found")?;
        ensure!(
            permitted(&role, op),
            "{op} is not allowed on a machine shared with you as {role}"
        );
        Ok(physical)
    }
    fn share(&mut self, user: i64, op: &str, request: &Value) -> Result<Value> {
        share_on(&mut self.db, user, op, request)
    }
    fn reserve(&mut self, user: i64, kind: &str, name: &str) -> Result<String> {
        common::identifier(name)?;
        if let Ok(id) = self.physical(user, kind, name) {
            return Ok(id);
        }
        let prefix = if kind == "machine" { "m" } else { "s" };
        let id = format!("{prefix}-{}", &common::nonce()?[..24]);
        self.db.execute(
            "INSERT INTO resources(owner,kind,name,physical,metadata) VALUES(?1,?2,?3,?4,?5)",
            params![user, kind, name, id, json!({"state":"pending"}).to_string()],
        )?;
        Ok(id)
    }
    fn save(&mut self, kind: &str, physical: &str, value: &Value) -> Result<()> {
        self.db.execute(
            "UPDATE resources SET metadata=?1,updated_at=CURRENT_TIMESTAMP WHERE physical=?2 AND kind=?3",
            params![value.to_string(), physical, kind],
        )?;
        Ok(())
    }
    fn public(&self, user: i64, kind: &str, value: &Value) -> Result<Value> {
        let mut value = value.clone();
        let key = if kind == "machine" { "id" } else { "name" };
        let physical = value[key].as_str().context("missing worker ID")?;
        value[key] = json!(
            self.logical(user, kind, physical)?
                .context("resource not found")?
        );
        if let Some(object) = value.as_object_mut() {
            object.remove("index");
        }
        for (key, kind) in [
            ("workspace", "machine"),
            ("source", "machine"),
            ("hibernation", "snapshot"),
        ] {
            if let Some(physical) = value[key].as_str() {
                value[key] = json!(self.logical(user, kind, physical)?);
            }
        }
        Ok(value)
    }

    fn placement(&self, kind: &str, physical: &str) -> Result<String> {
        Ok(self.db.query_row(
            "SELECT node FROM resources WHERE kind=?1 AND physical=?2",
            params![kind, physical],
            |r| r.get(0),
        )?)
    }
    fn save_node(&mut self, node: &str, kind: &str, physical: &str, value: &Value) -> Result<()> {
        self.db.execute("UPDATE resources SET metadata=?1,updated_at=CURRENT_TIMESTAMP WHERE kind=?2 AND physical=?3 AND node=?4",params![value.to_string(),kind,physical,node])?;
        Ok(())
    }
    fn operation_info(&self, user: i64, operation: i64) -> Result<Value> {
        let (mut info, response): (Value, Option<String>) = self.db.query_row("SELECT id,op,resource,state,node,retry_key,response FROM operations WHERE owner=?1 AND id=?2",params![user,operation],|r|Ok((json!({"id":r.get::<_,i64>(0)?,"op":r.get::<_,String>(1)?,"resource":r.get::<_,String>(2)?,"state":r.get::<_,String>(3)?,"node":r.get::<_,String>(4)?,"retry_key":r.get::<_,Option<String>>(5)?}),r.get(6)?)))?;
        if matches!(
            info["state"].as_str(),
            Some("failed" | "uncertain" | "not_dispatched")
        ) {
            let response: Value = response
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(Value::Null);
            info["error"] = json!(public_operation_error(
                info["op"].as_str().unwrap_or(""),
                response["error"].as_str().unwrap_or("")
            ));
        }
        Ok(info)
    }
    fn outcome_error(&self, user: i64, operation: i64) -> anyhow::Error {
        OperationFailure(
            self.operation_info(user, operation)
                .unwrap_or(json!({"id":operation,"state":"uncertain"})),
        )
        .into()
    }
    fn reconcile(&mut self) -> Result<(Value, Value)> {
        let mut all_machines = Vec::new();
        let mut all_snapshots = Vec::new();
        let mut pool = vec!["local".to_owned()];
        for node in nodes::inventory(&self.root)?.as_array().unwrap() {
            if node["online"] == true {
                pool.push(node["id"].as_str().unwrap().to_owned());
            }
        }
        for node in pool {
            let Ok(root) = nodes::route(&self.root, &node) else {
                continue;
            };
            if !root.join("control.sock").exists() {
                continue;
            }
            let Ok(machines) = wire::request(&root, json!({"op":"list"})) else {
                continue;
            };
            let snapshots = if node != "local"
                && nodes::operation_capable(&self.root, &node, "snapshots").is_err()
            {
                json!([])
            } else {
                let Ok(value) = wire::request(&root, json!({"op":"snapshots"})) else {
                    continue;
                };
                value
            };
            let (mut extra_mib, mut extra_cpus, mut extra_slots) = (0i64, 0i64, 0i64);
            for m in machines.as_array().context("invalid machines")? {
                let physical = m["id"].as_str().context("invalid ID")?;
                if matches!(
                    m["state"].as_str(),
                    Some("running" | "starting" | "stopping" | "creating" | "unknown")
                ) && !self.placement("machine", physical).is_ok_and(|n| n == node)
                {
                    extra_mib += m["memory_mib"].as_i64().unwrap_or(4096);
                    extra_cpus += m["vcpu_count"].as_i64().unwrap_or(16);
                    extra_slots += 1;
                }
                self.save_node(&node, "machine", physical, m)?;
                if matches!(
                    m["state"].as_str(),
                    Some("running" | "starting" | "stopping" | "creating" | "unknown")
                ) {
                    self.db.execute("UPDATE resources SET reserved_mib=MAX(reserved_mib,?1),reserved_cpus=MAX(reserved_cpus,?2) WHERE kind='machine' AND physical=?3 AND node=?4",params![m["memory_mib"].as_i64().unwrap_or(0),m["vcpu_count"].as_i64().unwrap_or(1),physical,node])?;
                }
                // Release only on positive stopped/hibernated evidence, never because a node is offline.
                if matches!(
                    m["state"].as_str(),
                    Some("stopped" | "hibernated" | "failed")
                ) {
                    self.db.execute("UPDATE resources SET reserved_mib=0,reserved_cpus=0 WHERE kind='machine' AND physical=?1 AND node=?2 AND NOT EXISTS(SELECT 1 FROM operations WHERE resource=resources.name AND owner=resources.owner AND state IN ('pending','uncertain') AND op IN ('create','start','fork','restore'))",params![physical,node])?;
                }
                all_machines.push(m.clone());
            }
            let observed = machines
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|m| m["id"].as_str())
                .collect::<std::collections::BTreeSet<_>>();
            let mut stmt=self.db.prepare("SELECT physical FROM resources WHERE kind='machine' AND node=?1 AND json_extract(metadata,'$.state')='pending' AND EXISTS(SELECT 1 FROM operations WHERE resource=resources.name AND owner=resources.owner AND state='failed') AND NOT EXISTS(SELECT 1 FROM operations WHERE resource=resources.name AND owner=resources.owner AND state IN ('pending','uncertain'))")?;
            let absent = stmt
                .query_map([&node], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            drop(stmt);
            for physical in absent {
                if !observed.contains(physical.as_str()) {
                    self.db.execute("UPDATE resources SET reserved_mib=0,reserved_cpus=0 WHERE kind='machine' AND physical=?1 AND node=?2",params![physical,node])?;
                }
            }
            // A machine this runner no longer lists is gone from it (deleted there, or the runner
            // lost its data): flag it so its owner can delete it here. Never-observed creates
            // stay pending; a machine listed again gets its metadata back on the next pass.
            self.db.execute("UPDATE resources SET metadata=json_set(metadata,'$.state','missing'),reserved_mib=0,reserved_cpus=0,updated_at=CURRENT_TIMESTAMP WHERE kind='machine' AND node=?1 AND json_extract(metadata,'$.state') NOT IN ('pending','missing') AND physical NOT IN (SELECT value FROM json_each(?2))",params![node,json!(observed).to_string()])?;
            self.db.execute("INSERT INTO node_usage(node,extra_mib,extra_cpus,extra_slots) VALUES(?1,?2,?3,?4) ON CONFLICT(node) DO UPDATE SET extra_mib=?2,extra_cpus=?3,extra_slots=?4",params![node,extra_mib,extra_cpus,extra_slots])?;
            for snapshot in snapshots.as_array().context("invalid snapshots")? {
                let physical = snapshot["name"].as_str().context("snapshot ID")?;
                let parent = snapshot["workspace"].as_str().context("snapshot parent")?;
                let owner:Option<i64>=self.db.query_row("SELECT owner FROM resources WHERE kind='machine' AND physical=?1 AND node=?2",params![parent,node],|r|r.get(0)).optional()?;
                if let Some(owner) = owner {
                    let name=if self.db.query_row("SELECT EXISTS(SELECT 1 FROM resources WHERE owner=?1 AND kind='snapshot' AND name=?2 AND physical<>?2)",params![owner,physical],|r|r.get::<_,bool>(0))?{format!("auto-{}",&common::nonce()?[..24])}else{physical.to_owned()};
                    self.db.execute("INSERT OR IGNORE INTO resources(owner,kind,name,physical,metadata,node) VALUES(?1,'snapshot',?2,?3,?4,?5)",params![owner,name,physical,snapshot.to_string(),node])?;
                    self.save_node(&node, "snapshot", physical, snapshot)?;
                    all_snapshots.push(snapshot.clone());
                }
            }
            self.db.execute("UPDATE resources SET metadata=json_set(metadata,'$.state','failed'),updated_at=CURRENT_TIMESTAMP WHERE kind='snapshot' AND node=?1 AND json_extract(metadata,'$.state')='pending' AND (SELECT state FROM operations WHERE owner=resources.owner AND node=resources.node AND op='snapshot' AND json_extract(request,'$.request.name')=resources.physical ORDER BY id DESC LIMIT 1)='failed'",[&node])?;
        }
        Ok((json!(all_machines), json!(all_snapshots)))
    }
    pub fn state(&mut self, user: i64) -> Result<Value> {
        self.reconcile()?;
        let mut stmt=self.db.prepare("SELECT kind,physical,metadata,node,reserved_mib FROM resources WHERE owner=?1 ORDER BY created_at,name")?;
        let rows = stmt.query_map([user], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, u32>(4)?,
            ))
        })?;
        let mut machines = Vec::new();
        let mut snapshots = Vec::new();
        let mut reserved = 0;
        for row in rows {
            let (kind, physical, metadata, node, memory) = row?;
            let mut value = self.view(user, &kind, &physical, &metadata, &node)?;
            if kind == "machine" {
                reserved += memory;
                value["shared_with"] = shared_with(&self.db, &physical)?;
                value["published_ports"] = self.publications(&physical)?;
                machines.push(value);
            } else {
                snapshots.push(value);
            }
        }
        let mut shared = Vec::new();
        let mut stmt = self.db.prepare("SELECT r.physical,r.metadata,r.node,u.email,g.role FROM grants g JOIN resources r ON r.kind='machine' AND r.physical=g.physical JOIN users u ON u.id=r.owner WHERE g.grantee=?1 ORDER BY u.email,r.name")?;
        let rows = stmt.query_map([user], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        for row in rows {
            let (physical, metadata, node, owner, role) = row?;
            let mut value = self.view(user, "machine", &physical, &metadata, &node)?;
            value["published_ports"] = self.publications(&physical)?;
            value["owner"] = json!(owner);
            value["role"] = json!(role);
            shared.push(value);
        }
        let mut running = BTreeMap::new();
        let mut pss = 0;
        let mut limit = 0;
        let mut pool = vec![("local".to_owned(), 4096)];
        let nodes = nodes::inventory(&self.root)?;
        for n in nodes.as_array().unwrap() {
            pool.push((
                n["id"].as_str().unwrap().into(),
                n["memory_mib"].as_u64().unwrap_or(0),
            ));
        }
        for (node, capacity) in pool {
            limit += capacity;
            let Ok(root) = nodes::route(&self.root, &node) else {
                continue;
            };
            if !root.join("control.sock").exists() {
                continue;
            }
            let Ok(stats) = wire::request(&root, json!({"op":"stats"})) else {
                continue;
            };
            if let Some(entries) = stats["running"].as_object() {
                for (physical, stats) in entries {
                    if self.placement("machine", physical).is_ok_and(|n| n == node) {
                        // Usage covers owned machines only, not ones shared with this user.
                        if let Some(name) = self
                            .logical(user, "machine", physical)?
                            .filter(|name| !name.contains('/'))
                        {
                            pss += stats["pss_kib"].as_u64().unwrap_or(0);
                            running.insert(name, stats.clone());
                        }
                    }
                }
            }
        }
        let mut operations = Vec::new();
        let mut stmt = self
            .db
            .prepare("SELECT id FROM operations WHERE owner=?1 ORDER BY id DESC LIMIT 100")?;
        for id in stmt.query_map([user], |r| r.get::<_, i64>(0))? {
            operations.push(self.operation_info(user, id?)?);
        }
        Ok(
            json!({"operations":operations,"workspaces":machines,"shared":shared,"snapshots":snapshots,"nodes":nodes,"stats":{"running":running,"total_pss_kib":pss,"reserved_memory_mib":reserved,"limit_memory_mib":limit,"note":"Offline usage is stale; reservations remain fixed"}}),
        )
    }
    fn view(
        &self,
        user: i64,
        kind: &str,
        physical: &str,
        metadata: &str,
        node: &str,
    ) -> Result<Value> {
        let mut value: Value = serde_json::from_str(metadata)?;
        value[if kind == "machine" { "id" } else { "name" }] = json!(physical);
        value["node"] = json!(node);
        let online = if node == "local" {
            self.root.join("control.sock").exists()
        } else {
            nodes::online(&self.root, node).is_ok()
        };
        value["node_online"] = json!(online);
        if !online {
            value["observed_state"] = value["state"].clone();
            value["state"] = json!("offline");
        }
        self.public(user, kind, &value)
    }
    /// Drop a deleted machine: its row, its grants (closing grantees' streams) and the snapshots
    /// taken from it, whoever took them. Past operations keep their IDs, which name node journal
    /// entries, but stop claiming the names, so a new machine may reuse them. Returns the
    /// caller's own snapshot names that went with it.
    fn forget(&mut self, user: i64, physical: &str) -> Result<Vec<String>> {
        // Its snapshots: captured ones name it as their workspace; a reservation whose capture
        // never completed is found through the snapshot operation that made it.
        const OF: &str = "kind='snapshot' AND (json_extract(metadata,'$.workspace')=?1 OR (json_extract(metadata,'$.state') IN ('pending','failed') AND physical IN (SELECT json_extract(request,'$.request.name') FROM operations WHERE op='snapshot' AND json_extract(request,'$.request.id')=?1)))";
        let tx = self.db.transaction()?;
        let grantees = tx
            .prepare("SELECT grantee FROM grants WHERE physical=?1")?
            .query_map([physical], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let snapshots = tx
            .prepare(&format!(
                "SELECT name FROM resources WHERE {OF} AND owner=?2 ORDER BY name"
            ))?
            .query_map(params![physical, user], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        tx.execute("UPDATE operations SET resource='deleted:'||resource WHERE op<>'snapshot' AND (owner,resource) IN (SELECT owner,name FROM resources WHERE kind='machine' AND physical=?1)",[physical])?;
        tx.execute(&format!("UPDATE operations SET resource='deleted:'||resource WHERE op='snapshot' AND (owner,resource) IN (SELECT owner,name FROM resources WHERE {OF})"),[physical])?;
        tx.execute(
            "UPDATE publications SET active=0 WHERE physical=?1",
            [physical],
        )?;
        tx.execute("DELETE FROM grants WHERE physical=?1", [physical])?;
        tx.execute(&format!("DELETE FROM resources WHERE {OF}"), [physical])?;
        tx.execute(
            "DELETE FROM resources WHERE kind='machine' AND physical=?1",
            [physical],
        )?;
        tx.commit()?;
        cut(user, physical);
        for grantee in grantees {
            cut(grantee, physical);
        }
        Ok(snapshots)
    }
    /// Whether the runner lists the machine right now.
    fn listed(&self, node: &str, physical: &str) -> Result<bool> {
        let machines = wire::request(&nodes::route(&self.root, node)?, json!({"op":"list"}))
            .context("runner unreachable; the machine may be back, try again")?;
        Ok(machines
            .as_array()
            .context("invalid machines")?
            .iter()
            .any(|m| m["id"] == physical))
    }
    /// Whether reconcile found the machine gone from its runner.
    fn missing(&self, physical: &str) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT json_extract(metadata,'$.state')='missing' FROM resources WHERE kind='machine' AND physical=?1",
            [physical],
            |r| r.get::<_, Option<bool>>(0),
        )? == Some(true))
    }
    /// The saved result of a delete retried with its key. A key held by another operation or
    /// name conflicts; one whose machine went without a recorded result is uncertain.
    fn delete_replay(&self, user: i64, name: &str, key: Option<&str>) -> Result<Option<Value>> {
        let Some(key) = key else {
            return Ok(None);
        };
        common::identifier(key)?;
        let previous:Option<(i64,String,String,String,Option<String>)>=self.db.query_row("SELECT id,op,resource,state,response FROM operations WHERE owner=?1 AND retry_key=?2",params![user,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        let Some((id, op, resource, state, response)) = previous else {
            return Ok(None);
        };
        let forgotten = resource == format!("deleted:{name}");
        ensure!(
            op == "delete" && (resource == name || forgotten),
            "operation key conflicts with original parameters"
        );
        if state == "succeeded" {
            return Ok(Some(serde_json::from_str(
                &response.context("missing result")?,
            )?));
        }
        if forgotten {
            return Err(self.outcome_error(user, id));
        }
        Ok(None)
    }
    /// Delete a machine its runner no longer lists: record the operation under its key, with
    /// the fingerprint a dispatched delete would have, then drop the entry.
    fn delete_missing(
        &mut self,
        user: i64,
        name: &str,
        physical: &str,
        input: &Value,
    ) -> Result<Value> {
        let node = self.placement("machine", physical)?;
        let mut request = input.clone();
        request["id"] = json!(physical);
        let object = request.as_object_mut().context("request")?;
        object.remove("node");
        object.remove("operation_key");
        let fingerprint = json!({"request":request,"node":node}).to_string();
        let key = match input["operation_key"].as_str() {
            Some(key) => key.to_owned(),
            None => common::nonce()?,
        };
        let previous: Option<(i64, String, String)> = self
            .db
            .query_row(
                "SELECT id,request,state FROM operations WHERE owner=?1 AND retry_key=?2",
                params![user, key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let operation = match previous {
            Some((id, previous, state)) => {
                ensure!(
                    previous == fingerprint,
                    "operation key conflicts with original parameters"
                );
                ensure!(
                    state != "failed",
                    "operation previously rejected; use a new operation_key for deliberate retry"
                );
                id
            }
            None => {
                self.db.execute("INSERT INTO operations(owner,op,resource,state,node,retry_key,request) VALUES(?1,'delete',?2,'pending',?3,?4,?5)",params![user,name,node,key,fingerprint])?;
                self.db.last_insert_rowid()
            }
        };
        let mut value = json!({"deleted":name,"snapshots":self.forget(user, physical)?});
        value["operation"] = self.operation_info(user, operation)?;
        value["operation"]["state"] = json!("succeeded");
        value["operation_id"] = json!(operation);
        value["node"] = json!(node);
        self.db.execute("UPDATE operations SET state='succeeded',response=?1,finished_at=CURRENT_TIMESTAMP WHERE id=?2",params![value.to_string(),operation])?;
        Ok(value)
    }
    fn snapshot_source(&self, user: i64, snapshot: &str, physical: &str) -> Result<()> {
        let metadata: String = self.db.query_row(
            "SELECT metadata FROM resources WHERE owner=?1 AND kind='snapshot' AND physical=?2",
            params![user, snapshot],
            |r| r.get(0),
        )?;
        ensure!(
            serde_json::from_str::<Value>(&metadata)?["workspace"] == physical,
            "snapshot source mismatch"
        );
        Ok(())
    }
    fn extra_usage(&self, node: &str) -> Result<(u64, u64, u64)> {
        Ok(self
            .db
            .query_row(
                "SELECT extra_mib,extra_cpus,extra_slots FROM node_usage WHERE node=?1",
                [node],
                |r| {
                    Ok((
                        r.get::<_, u32>(0)? as u64,
                        r.get::<_, u32>(1)? as u64,
                        r.get::<_, u32>(2)? as u64,
                    ))
                },
            )
            .optional()?
            .unwrap_or((0, 0, 0)))
    }
    fn choose(
        &self,
        requested: Option<&str>,
        memory: u64,
        image: &str,
        cpus: u64,
        local_status: &Value,
        disk: bool,
    ) -> Result<String> {
        let inventory = nodes::inventory(&self.root)?;
        if inventory.as_array().unwrap().is_empty() && requested.is_none() {
            common::guest_shape(local_status, memory, cpus)?;
            return Ok("local".into());
        }
        let mut candidates = Vec::new();
        for n in inventory.as_array().unwrap() {
            let id = n["id"].as_str().unwrap();
            if requested.is_some_and(|r| r != id)
                || (disk && n["capabilities"]["disk_resize_v1"] != true)
                || common::guest_shape(&n["capabilities"], memory, cpus).is_err()
                || n["online"] != true
                || n["capabilities"]["drain"] == true
                || !n["capabilities"]["images"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|i| i == image))
                || cpus > n["capabilities"]["vcpus"].as_u64().unwrap_or(0)
            {
                continue;
            }
            let (used,count):(u32,u32)=self.db.query_row("SELECT COALESCE(SUM(reserved_mib),0),SUM(CASE WHEN reserved_mib>0 THEN 1 ELSE 0 END) FROM resources WHERE kind='machine' AND node=?1",[id],|r|Ok((r.get(0)?,r.get::<_,Option<u32>>(1)?.unwrap_or(0))))?;
            let (extra_mib, extra_cpus, extra_slots) = self.extra_usage(id)?;
            let used_cpus:u32=self.db.query_row("SELECT COALESCE(SUM(reserved_cpus),0) FROM resources WHERE kind='machine' AND node=?1",[id],|r|r.get(0))?;
            if used as u64 + extra_mib + memory <= n["memory_mib"].as_u64().unwrap()
                && count as u64 + extra_slots < n["slots"].as_u64().unwrap()
                && used_cpus as u64 + extra_cpus + cpus
                    <= n["capabilities"]["vcpus"].as_u64().unwrap_or(0)
            {
                candidates.push((used, id.to_owned()));
            }
        }
        candidates.sort();
        candidates
            .first()
            .map(|(_, id)| id.clone())
            .context("no online node has compatible capacity")
    }
    fn capacity(
        &self,
        node: &str,
        physical: &str,
        memory: u64,
        cpus: u64,
        image: &str,
        local_status: &Value,
    ) -> Result<()> {
        if node == "local" {
            return common::guest_shape(local_status, memory, cpus);
        }
        nodes::online(&self.root, node)?;
        let inventory = nodes::inventory(&self.root)?;
        let n = inventory
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == node)
            .context("node")?;
        common::guest_shape(&n["capabilities"], memory, cpus)?;
        let (limit, slots) = (
            n["memory_mib"].as_u64().unwrap_or(0),
            n["slots"].as_u64().unwrap_or(0),
        );
        let (used,count):(u32,u32)=self.db.query_row("SELECT COALESCE(SUM(reserved_mib),0),SUM(CASE WHEN reserved_mib>0 THEN 1 ELSE 0 END) FROM resources WHERE kind='machine' AND node=?1 AND physical<>?2",params![node,physical],|r|Ok((r.get(0)?,r.get::<_,Option<u32>>(1)?.unwrap_or(0))))?;
        let used_cpus:u32=self.db.query_row("SELECT COALESCE(SUM(reserved_cpus),0) FROM resources WHERE kind='machine' AND node=?1 AND physical<>?2",params![node,physical],|r|r.get(0))?;
        let (extra_mib, extra_cpus, extra_slots) = self.extra_usage(node)?;
        ensure!(
            used as u64 + extra_mib + memory <= limit
                && count as u64 + extra_slots < slots
                && used_cpus as u64 + extra_cpus + cpus
                    <= n["capabilities"]["vcpus"].as_u64().unwrap_or(0),
            "node capacity exhausted"
        );
        ensure!(
            n["capabilities"]["images"]
                .as_array()
                .is_some_and(|a| a.iter().any(|i| i == image)),
            "node lacks prepared image"
        );
        Ok(())
    }
    fn admit(
        &self,
        node: &str,
        target: &str,
        op: &str,
        request: &Value,
        metadata: &Value,
        local_status: &Value,
    ) -> Result<()> {
        if node != "local" {
            let pool = nodes::inventory(&self.root)?;
            let n = pool
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["id"] == node)
                .context("node")?;
            common::node_operation(&n["capabilities"], op)?;
        }
        if request.get("disk_gib").is_some() {
            let capable = if node == "local" {
                local_status["disk_resize_v1"] == true
            } else {
                nodes::inventory(&self.root)?
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|n| n["id"] == node && n["capabilities"]["disk_resize_v1"] == true)
            };
            ensure!(
                capable,
                "runner does not support disk sizing; upgrade its worker and agent first (Linux Firecracker only)"
            );
        }
        if op == "resize" {
            let mut shape = request.clone();
            for key in ["memory_mib", "vcpu_count"] {
                if shape.get(key).is_none() {
                    shape[key] = metadata[key].clone();
                }
            }
            let (ram, cpus) = common::requested_resources(&shape)?;
            ensure!(
                metadata["state"] == "stopped",
                "resize requires a positively stopped workspace"
            );
            let reserved: bool = self.db.query_row("SELECT reserved_mib>0 OR reserved_cpus>0 OR EXISTS(SELECT 1 FROM operations WHERE node=?2 AND state IN ('pending','uncertain')) FROM resources WHERE physical=?1 AND kind='machine'",params![target,node],|r|r.get(0))?;
            ensure!(!reserved, "resize has unresolved reservations/effects");
            if node == "local" {
                common::guest_shape(local_status, ram as u64, cpus as u64)?;
            } else {
                let pool = nodes::inventory(&self.root)?;
                let n = pool
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|n| n["id"] == node)
                    .context("node")?;
                common::guest_shape(&n["capabilities"], ram as u64, cpus as u64)?;
                ensure!(
                    ram as u64 <= n["memory_mib"].as_u64().unwrap_or(0)
                        && cpus as u64 <= n["capabilities"]["vcpus"].as_u64().unwrap_or(0),
                    "resize exceeds effective host budget"
                );
            }
        }
        let shape = if op == "fork" && request["snapshot"].is_string() {
            let metadata: String = self.db.query_row(
                "SELECT metadata FROM resources WHERE kind='snapshot' AND physical=?1",
                [request["snapshot"].as_str().unwrap()],
                |r| r.get(0),
            )?;
            serde_json::from_str::<Value>(&metadata)?
        } else {
            metadata.clone()
        };
        let memory = if op == "create" {
            request["memory_mib"].as_u64().unwrap_or(256)
        } else {
            shape["memory_mib"].as_u64().unwrap_or(256)
        };
        if matches!(op, "create" | "start" | "fork" | "restore") {
            let cpus = if op == "create" {
                request["vcpu_count"].as_u64().unwrap_or(1)
            } else {
                shape["vcpu_count"].as_u64().unwrap_or(1)
            };
            let image = if op == "create" {
                request["image"].as_str().unwrap_or("alpine")
            } else {
                shape["image"].as_str().unwrap_or("alpine")
            };
            self.capacity(node, target, memory, cpus, image, local_status)?;
            self.db.execute("UPDATE resources SET reserved_mib=?1,reserved_cpus=?2 WHERE kind='machine' AND physical=?3",params![memory as i64,cpus as i64,target])?;
        }
        Ok(())
    }
    pub fn operation(&mut self, user: i64, mut request: Value) -> Result<Value> {
        let op = request["op"].as_str().context("op")?.to_owned();
        ensure!(
            [
                "create",
                "resize",
                "start",
                "stop",
                "exec",
                "ssh-keys",
                "ssh-info",
                "put",
                "get",
                "hibernate",
                "snapshot",
                "fork",
                "restore",
                "share",
                "unshare",
                "delete"
            ]
            .contains(&op.as_str()),
            "unsupported user operation"
        );
        if matches!(op.as_str(), "share" | "unshare") {
            return self.share(user, &op, &request);
        }
        if matches!(op.as_str(), "create" | "resize") {
            common::requested_resources(&request)?;
            common::requested_disk(&request)?;
        }
        let name = request["id"].as_str().context("id")?.to_owned();
        common::address(&name)?;
        let input = request.clone();
        // A finished delete has no row left to resolve: answer its key before looking.
        if op == "delete"
            && let Some(cached) =
                self.delete_replay(user, &name, input["operation_key"].as_str())?
        {
            return Ok(cached);
        }
        // Resolve authority before any worker contact, including inventory refresh.
        if op != "create" {
            let physical = self.machine(user, &name, &op)?;
            // Confirmed gone from its runner: only the row is left. A runner that is online now
            // may have it back, so it must still not list it; an unreachable one refuses.
            if op == "delete" && self.missing(&physical)? {
                let node = self.placement("machine", &physical)?;
                if nodes::online(&self.root, &node).is_err() || !self.listed(&node, &physical)? {
                    return self.delete_missing(user, &name, &physical, &input);
                }
            }
            let node = self.placement("machine", &physical)?;
            nodes::online(&self.root, &node)?;
            nodes::operation_capable(&self.root, &node, &op)?;
            if matches!(op.as_str(), "ssh-keys" | "ssh-info") {
                nodes::ssh_capable(&self.root, &node)?;
            }
            for key in ["name", "snapshot"] {
                if matches!(op.as_str(), "restore" | "fork") {
                    if let Some(snapshot) = request[key].as_str() {
                        let snapshot = self.physical(user, "snapshot", snapshot)?;
                        ensure!(
                            self.placement("snapshot", &snapshot)? == node,
                            "cross-node snapshot denied"
                        );
                        self.snapshot_source(user, &snapshot, &physical)?;
                    }
                }
            }
        } else if let Some(node) = request["node"].as_str() {
            nodes::online(&self.root, node)?;
        }
        self.reconcile()?;
        if op == "delete" {
            // The runner no longer has it: nothing to dispatch, only the row to drop.
            let physical = self.machine(user, &name, &op)?;
            let node = self.placement("machine", &physical)?;
            if self.missing(&physical)? && !self.listed(&node, &physical)? {
                return self.delete_missing(user, &name, &physical, &input);
            }
        }
        // Optional protocol-v1 status fields; query outside the SQLite writer. A missing
        // legacy response conservatively retains the original 2 GiB/4 CPU ceiling.
        let local_status = if matches!(
            op.as_str(),
            "create" | "resize" | "start" | "fork" | "restore"
        ) {
            wire::request(&self.root, json!({"op":"status"})).unwrap_or_else(|_| json!({}))
        } else {
            json!({})
        };
        // BEGIN IMMEDIATE serializes placement, operation fingerprint and memory/slot reservation
        // across gateway processes. No network I/O occurs inside this transaction.
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        let mut prior_reservation = (0i64, 0i64);
        let prepared = (|| -> Result<(String, String, String, i64, Option<Value>)> {
            let physical = if op == "create" {
                self.reserve(user, "machine", &name)?
            } else {
                self.machine(user, &name, &op)?
            };
            let metadata: String = self.db.query_row(
                "SELECT metadata FROM resources WHERE kind='machine' AND physical=?1",
                [&physical],
                |r| r.get(0),
            )?;
            let metadata: Value = serde_json::from_str(&metadata)?;
            if op == "create" && metadata["state"] != "pending" {
                ensure!(
                    metadata["memory_mib"].as_u64()
                        == Some(request["memory_mib"].as_u64().unwrap_or(256))
                        && metadata["vcpu_count"].as_u64().unwrap_or(1)
                            == request["vcpu_count"].as_u64().unwrap_or(1)
                        && metadata["image"].as_str().unwrap_or("alpine")
                            == request["image"].as_str().unwrap_or("alpine")
                        && request
                            .get("disk_gib")
                            .is_none_or(|n| metadata["disk_bytes"].as_u64()
                                == n.as_u64().map(|n| n * common::GIB))
                        && metadata["source"].is_null(),
                    "existing machine parameters conflict"
                );
            }
            let mut node = self.placement("machine", &physical)?;
            if op == "create" && metadata["state"] == "pending" {
                // Choose exactly once: nonlocal placement and an existing create operation pin it.
                let chosen:bool=self.db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE owner=?1 AND resource=?2 AND op IN ('create','fork'))",params![user,name],|r|r.get(0))?;
                if !chosen {
                    node = self.choose(
                        request["node"].as_str(),
                        request["memory_mib"].as_u64().unwrap_or(256),
                        request["image"].as_str().unwrap_or("alpine"),
                        request["vcpu_count"].as_u64().unwrap_or(1),
                        &local_status,
                        request.get("disk_gib").is_some(),
                    )?;
                    self.db.execute(
                        "UPDATE resources SET node=?1 WHERE kind='machine' AND physical=?2",
                        params![node, physical],
                    )?;
                    self.save("machine",&physical,&json!({"state":"pending","image":request["image"].as_str().unwrap_or("alpine"),"memory_mib":request["memory_mib"].as_u64().unwrap_or(256),"vcpu_count":request["vcpu_count"].as_u64().unwrap_or(1)}))?;
                }
            }
            if let Some(chosen) = request["node"].as_str() {
                ensure!(chosen == node, "placement is fixed");
            }
            request["id"] = json!(physical);
            request.as_object_mut().unwrap().remove("node");
            let mut target = physical.clone();
            match op.as_str() {
                "snapshot" => {
                    let name = request["name"].as_str().context("snapshot name")?;
                    let new_snapshot = self.physical(user, "snapshot", name).is_err();
                    let id = self.reserve(user, "snapshot", name)?;
                    if new_snapshot {
                        self.db.execute("UPDATE resources SET node=?1 WHERE kind='snapshot' AND physical=?2 AND json_extract(metadata,'$.state')='pending'",params![node,id])?;
                    }
                    ensure!(
                        self.placement("snapshot", &id)? == node,
                        "snapshot placement mismatch"
                    );
                    request["name"] = json!(id);
                }
                "restore" => {
                    let id = self.physical(
                        user,
                        "snapshot",
                        request["name"].as_str().context("snapshot")?,
                    )?;
                    ensure!(
                        self.placement("snapshot", &id)? == node,
                        "cross-node restore denied"
                    );
                    self.snapshot_source(user, &id, &physical)?;
                    request["name"] = json!(id);
                }
                "fork" => {
                    if let Some(snapshot) = request["snapshot"].as_str() {
                        let snapshot = self.physical(user, "snapshot", snapshot)?;
                        ensure!(
                            self.placement("snapshot", &snapshot)? == node,
                            "cross-node fork denied"
                        );
                        self.snapshot_source(user, &snapshot, &physical)?;
                        request["snapshot"] = json!(snapshot);
                    }
                    let child = request["child"].as_str().context("child")?;
                    let new_child = self.physical(user, "machine", child).is_err();
                    target = self.reserve(user, "machine", child)?;
                    let existing:bool=self.db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE owner=?1 AND resource=?2 AND op='fork')",params![user,child],|r|r.get(0))?;
                    if !existing && new_child {
                        self.db.execute(
                            "UPDATE resources SET node=?1 WHERE kind='machine' AND physical=?2",
                            params![node, target],
                        )?;
                        let mut pending = metadata.clone();
                        pending["state"] = json!("pending");
                        pending["source"] = json!(physical);
                        self.save("machine", &target, &pending)?;
                    }
                    ensure!(
                        new_child || existing,
                        "child already exists from another creation intent"
                    );
                    ensure!(
                        self.placement("machine", &target)? == node,
                        "child placement mismatch"
                    );
                    request["child"] = json!(target);
                }
                _ => {}
            }
            if matches!(op.as_str(), "create" | "fork" | "snapshot") {
                let identifier = if op == "fork" {
                    input["child"].as_str().unwrap_or(&name)
                } else if op == "snapshot" {
                    input["name"].as_str().unwrap_or(&name)
                } else {
                    &name
                };
                let prior:Option<String>=self.db.query_row("SELECT request FROM operations WHERE owner=?1 AND op=?2 AND resource=?3 AND state<>'failed' ORDER BY id LIMIT 1",params![user,op,identifier],|r|r.get(0)).optional()?;
                let mut normalized = request.clone();
                normalized.as_object_mut().unwrap().remove("operation_key");
                if let Some(prior) = prior {
                    ensure!(
                        prior == json!({"request":normalized,"node":node}).to_string(),
                        "resource operation conflicts with original parameters"
                    );
                }
            }
            prior_reservation=self.db.query_row("SELECT reserved_mib,reserved_cpus FROM resources WHERE kind='machine' AND physical=?1",[&target],|r|Ok((r.get(0)?,r.get(1)?)))?;
            let key = if let Some(key) = input["operation_key"].as_str() {
                common::identifier(key)?;
                key.to_owned()
            } else {
                match op.as_str() {
                    "create" => format!("c-{physical}"),
                    "fork" => format!("f-{target}"),
                    "snapshot" => format!("s-{}", request["name"].as_str().unwrap()),
                    _ => common::nonce()?,
                }
            };
            let previous:Option<(i64,String,String,Option<String>)>=self.db.query_row("SELECT id,request,state,response FROM operations WHERE owner=?1 AND retry_key=?2",params![user,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
            request.as_object_mut().unwrap().remove("operation_key");
            let fingerprint = json!({"request":request,"node":node}).to_string();
            if let Some((id, previous, state, response)) = previous {
                ensure!(
                    previous == fingerprint,
                    "operation key conflicts with original parameters"
                );
                if state == "succeeded" {
                    return Ok((
                        physical,
                        node,
                        target,
                        id,
                        Some(serde_json::from_str(&response.context("missing result")?)?),
                    ));
                }
                // Pending after a controller restart is indistinguishable from a lost reply;
                // retry only the same worker journal ID. Local unknown exec must never replay.
                if node == "local"
                    && matches!(state.as_str(), "pending" | "uncertain")
                    && !matches!(op.as_str(), "create" | "fork" | "snapshot")
                {
                    return Err(self.outcome_error(user, id));
                }
                if state == "failed" {
                    return Err(self.outcome_error(user, id));
                }
                if state == "not_dispatched" {
                    self.admit(&node, &target, &op, &request, &metadata, &local_status)?;
                    self.db
                        .execute("UPDATE operations SET state='pending' WHERE id=?1", [id])?;
                }
                // Uncertain effects retry only the same durable agent journal entry.
                return Ok((physical, node, target, id, None));
            }
            nodes::online(&self.root, &node)?;
            self.admit(&node, &target, &op, &request, &metadata, &local_status)?;
            let resource = if op == "fork" {
                input["child"].as_str().unwrap_or(&name)
            } else if op == "snapshot" {
                input["name"].as_str().unwrap_or(&name)
            } else {
                &name
            };
            self.db.execute("INSERT INTO operations(owner,op,resource,state,node,retry_key,request) VALUES(?1,?2,?3,'pending',?4,?5,?6)",params![user,op,resource,node,key,fingerprint])?;
            if op == "snapshot" {
                // A new capture supersedes a failed reservation, not a known sealed artifact.
                self.db.execute("UPDATE resources SET metadata=json_set(metadata,'$.state','pending'),updated_at=CURRENT_TIMESTAMP WHERE owner=?1 AND kind='snapshot' AND physical=?2 AND json_extract(metadata,'$.state')='failed'",params![user,request["name"].as_str().context("snapshot ID")?])?;
            }
            Ok((physical, node, target, self.db.last_insert_rowid(), None))
        })();
        let (physical, node, target, operation, cached) = match prepared {
            Ok(p) => {
                self.db.execute_batch("COMMIT")?;
                p
            }
            Err(e) => {
                self.db.execute_batch("ROLLBACK")?;
                return Err(e);
            }
        };
        if let Some(cached) = cached {
            return Ok(cached);
        }
        let root = match nodes::route(&self.root, &node) {
            Ok(root) => root,
            Err(error) => {
                eprintln!("operation {operation} routing: {error:#}");
                if self.db.execute(
                    "UPDATE operations SET state='not_dispatched',response=?2 WHERE id=?1 AND state='pending'",
                    params![operation, json!({"error":"node route unavailable before dispatch"}).to_string()],
                )? == 1
                {
                    self.db.execute("UPDATE resources SET reserved_mib=?1,reserved_cpus=?2 WHERE kind='machine' AND physical=?3",params![prior_reservation.0,prior_reservation.1,target])?;
                }
                return Err(self.outcome_error(user, operation));
            }
        };
        if node != "local" {
            request["operation_key"] = json!(format!("op-{operation}"));
        }
        let response = wire::request_envelope(&root, &request);
        let envelope = match response {
            Ok(v) => v,
            Err(e) => {
                self.db.execute(
                    "UPDATE operations SET state='uncertain',response=?2 WHERE id=?1",
                    params![
                        operation,
                        json!({"error":format!("transport lost after dispatch: {e:#}")})
                            .to_string()
                    ],
                )?;
                eprintln!("operation {operation} transport: {e:#}");
                return Err(self.outcome_error(user, operation));
            }
        };
        let uncertain = envelope["uncertain"] == true;
        if envelope["ok"] != true {
            self.db.execute(
                "UPDATE operations SET state=?1,response=?3,finished_at=CASE WHEN ?1='failed' THEN CURRENT_TIMESTAMP ELSE NULL END WHERE id=?2",
                params![if uncertain { "uncertain" } else { "failed" }, operation, envelope.to_string()],
            )?;
            // Keep raw worker diagnostics private, but never discard the cause.
            eprintln!(
                "operation {operation} ({op}) on node {node}: {}",
                envelope["error"]
                    .as_str()
                    .unwrap_or("worker returned no error")
            );
            // A runtime error can follow partial effects; inventory decides resource state.
            if let Err(error) = self.reconcile() {
                eprintln!("operation {operation} reconciliation: {error:#}");
            }
            return Err(self.outcome_error(user, operation));
        }
        let mut value = envelope["result"].clone();
        let correlated = if matches!(
            op.as_str(),
            "exec" | "put" | "get" | "ssh-keys" | "ssh-info"
        ) {
            true
        } else if op == "snapshot" {
            value["name"] == request["name"] && value["workspace"] == request["id"]
        } else if op == "stop" {
            value["workspace"]["id"] == request["id"]
        } else if op == "delete" {
            value["deleted"] == request["id"]
        } else {
            value["id"] == target
        };
        if !correlated {
            self.db.execute(
                "UPDATE operations SET state='uncertain',response=?2 WHERE id=?1",
                params![
                    operation,
                    json!({"error":"worker response did not match the requested resource"})
                        .to_string()
                ],
            )?;
            eprintln!(
                "operation {operation}: worker response did not match the requested resource"
            );
            return Err(self.outcome_error(user, operation));
        }
        if matches!(
            op.as_str(),
            "exec" | "put" | "get" | "ssh-keys" | "ssh-info"
        ) {
        } else if op == "snapshot" {
            let id = request["name"].as_str().unwrap();
            self.save_node(&node, "snapshot", id, &value)?;
            value = self.public(user, "snapshot", &value)?;
        } else if op == "stop" {
            self.save_node(&node, "machine", &physical, &value["workspace"])?;
            value["workspace"] = self.public(user, "machine", &value["workspace"])?;
        } else if op == "delete" {
            value = json!({"deleted":name,"snapshots":self.forget(user, &physical)?});
        } else {
            self.save_node(&node, "machine", &target, &value)?;
            value = self.public(user, "machine", &value)?;
        }
        value["operation"] = self.operation_info(user, operation)?;
        value["operation"]["state"] = json!("succeeded");
        value["operation_id"] = json!(operation);
        value["node"] = json!(node);
        self.db.execute("UPDATE operations SET state='succeeded',response=?1,finished_at=CURRENT_TIMESTAMP WHERE id=?2",params![value.to_string(),operation])?;
        if matches!(op.as_str(), "stop" | "hibernate") {
            self.db.execute("UPDATE resources SET reserved_mib=0,reserved_cpus=0 WHERE kind='machine' AND physical=?1",[&physical])?;
        }
        self.reconcile()?;
        Ok(value)
    }
    pub fn publications(&self, physical: &str) -> Result<Value> {
        let mut stmt = self.db.prepare("SELECT guest_port,https_port FROM publications WHERE physical=?1 AND active=1 ORDER BY guest_port")?;
        let rows = stmt.query_map([physical], |r| {
            Ok(json!({"port":r.get::<_,u16>(0)?,"https_port":r.get::<_,u16>(1)?}))
        })?;
        Ok(json!(rows.collect::<rusqlite::Result<Vec<_>>>()?))
    }

    pub fn publication(
        &mut self,
        user: i64,
        request: &Value,
        serve: impl Fn(u16, bool) -> Result<()>,
    ) -> Result<Value> {
        let name = request["id"].as_str().context("id")?;
        // Publication settings belong exclusively to the owner, including admin shares.
        let physical = self.physical(user, "machine", name)?;
        let port = request
            .get("port")
            .map(|p| {
                p.as_u64()
                    .filter(|p| (1..=65535).contains(p))
                    .map(|p| p as u16)
                    .context("invalid port")
            })
            .transpose()?;
        if request["op"] == "publish" {
            let port = port.context("port required")?;
            let node = self.placement("machine", &physical)?;
            nodes::operation_capable(&self.root, &node, "tunnel")?;
            self.db.execute("INSERT OR IGNORE INTO publications(https_port,physical,guest_port) SELECT COALESCE(MAX(https_port),10999)+1,?1,?2 FROM publications", params![physical,port])?;
            let https: u16 = self.db.query_row(
                "SELECT https_port FROM publications WHERE physical=?1 AND guest_port=?2",
                params![physical, port],
                |r| r.get(0),
            )?;
            serve(https, true)?;
            self.db.execute(
                "UPDATE publications SET active=1 WHERE https_port=?1",
                [https],
            )?;
            Ok(json!({"id":name,"port":port,"https_port":https}))
        } else {
            ensure!(
                request["op"] == "unpublish",
                "invalid publication operation"
            );
            let ports = self.db.prepare("SELECT https_port FROM publications WHERE physical=?1 AND (?2 IS NULL OR guest_port=?2)")?.query_map(params![physical,port],|r|r.get::<_,u16>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            self.db.execute("UPDATE publications SET active=0 WHERE physical=?1 AND (?2 IS NULL OR guest_port=?2)",params![physical,port])?;
            for https in &ports {
                let sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
                for (_, key, notify) in sessions.iter() {
                    if key == &format!("preview:{https}") {
                        notify.upgrade().inspect(|n| n.notify_one());
                    }
                }
                drop(sessions);
            }
            for https in ports {
                serve(https, false)?;
            }
            Ok(json!({"id":name,"published_ports":self.publications(&physical)?}))
        }
    }

    pub fn preview_target(&self, user: i64, https: u16) -> Result<crate::preview::Target> {
        let (physical, port): (String,u16) = self.db.query_row("SELECT p.physical,p.guest_port FROM publications p JOIN resources r ON r.kind='machine' AND r.physical=p.physical WHERE p.https_port=?1 AND p.active=1",[https],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let name = self
            .logical(user, "machine", &physical)?
            .context("not shared")?;
        let node = self.placement("machine", &physical)?;
        nodes::operation_capable(&self.root, &node, "tunnel")?;
        let revoked = self.watched(user, &name, "preview", &physical)?;
        Ok(crate::preview::Target {
            root: nodes::route(&self.root, &node)?,
            id: physical,
            port,
            revoked,
            unpublished: session(user, &format!("preview:{https}")),
        })
    }

    /// Register a stream, then check access again: unshare runs outside the catalog lock, so a
    /// grant deleted before this check denies the stream and one deleted after it cuts it.
    fn watched(
        &self,
        user: i64,
        name: &str,
        op: &str,
        physical: &str,
    ) -> Result<Arc<tokio::sync::Notify>> {
        let notify = session(user, physical);
        ensure!(
            self.machine(user, name, op)? == physical,
            "resource not found"
        );
        Ok(notify)
    }
    /// Route for a gateway SSH stream; the returned notify fires when the user loses access.
    pub fn ssh_target(
        &self,
        user: i64,
        name: &str,
    ) -> Result<(PathBuf, String, Arc<tokio::sync::Notify>)> {
        let physical = self.machine(user, name, "ssh")?;
        let node = self.placement("machine", &physical)?;
        nodes::ssh_capable(&self.root, &node)?;
        let route = nodes::route(&self.root, &node)?;
        Ok((
            route,
            physical.clone(),
            self.watched(user, name, "ssh", &physical)?,
        ))
    }
    pub fn terminal_target(
        &self,
        user: i64,
        name: &str,
    ) -> Result<(PathBuf, String, Arc<tokio::sync::Notify>)> {
        let physical = self.machine(user, name, "terminal")?;
        let node = self.placement("machine", &physical)?;
        nodes::operation_capable(&self.root, &node, "terminal")?;
        let route = nodes::route(&self.root, &node)?;
        Ok((
            route,
            physical.clone(),
            self.watched(user, name, "terminal", &physical)?,
        ))
    }

    #[cfg(test)]
    pub fn terminal_id(&self, user: i64, name: &str) -> Result<String> {
        self.physical(user, "machine", name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(legacy: bool) -> PathBuf {
        use std::os::unix::net::UnixListener;
        let root = std::env::temp_dir().join(format!("ow-catalog-{}", common::nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        let listener = UnixListener::bind(root.join("control.sock")).unwrap();
        std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let req = wire::line(&mut stream).unwrap();
                let value = if legacy && req["op"] == "list" {
                    json!([{"id":"legacy","state":"running","image":"ubuntu","memory_mib":1024,"vcpu_count":1}])
                } else if legacy && req["op"] == "snapshots" {
                    json!([{"name":"legacy","workspace":"legacy","state":"ready","memory_mib":1024}])
                } else {
                    json!([])
                };
                wire::send(&mut stream, &json!({"ok":true,"result":value})).unwrap();
            }
        });
        root
    }
    fn identity(subject: &str, email: &str) -> Identity {
        Identity {
            issuer: "https://example.cloudflareaccess.com".into(),
            subject: subject.into(),
            email: email.into(),
        }
    }
    #[test]
    fn ownership_and_subject_binding_survive_restart() {
        let root = fixture(true);
        let alice = identity("alice-sub", "alice@example.test");
        let bob = identity("bob-sub", "bob@example.test");
        let mut c = Catalog::open(&root, &alice.issuer, Some(&alice.email)).unwrap();
        let b = c.user(&bob).unwrap();
        assert!(c.terminal_id(b, "legacy").is_err());
        let a = c.user(&alice).unwrap();
        assert_eq!(c.terminal_id(a, "legacy").unwrap(), "legacy");
        assert_eq!(c.physical(a, "snapshot", "legacy").unwrap(), "legacy");
        c.save(
            "snapshot",
            "legacy",
            &json!({"name":"legacy","state":"ready"}),
        )
        .unwrap();
        let machine_state: String =
            c.db.query_row(
                "SELECT metadata FROM resources WHERE kind='machine' AND physical='legacy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(machine_state.contains("running"));
        let a_id = c.reserve(a, "machine", "demo").unwrap();
        let b_id = c.reserve(b, "machine", "demo").unwrap();
        assert_ne!(a_id, b_id);
        assert!(c.terminal_id(b, &a_id).is_err());
        let checkpoint = c.reserve(a, "snapshot", "private-checkpoint").unwrap();
        assert!(c.physical(b, "snapshot", "private-checkpoint").is_err());
        assert!(c.user(&identity("replacement-sub", &alice.email)).is_err());
        drop(c);
        let mut c = Catalog::open(&root, &alice.issuer, Some(&bob.email)).unwrap();
        assert_eq!(c.user(&alice).unwrap(), a);
        assert_eq!(c.user(&bob).unwrap(), b);
        assert_eq!(c.terminal_id(a, "demo").unwrap(), a_id);
        assert_eq!(
            c.physical(a, "snapshot", "private-checkpoint").unwrap(),
            checkpoint
        );
        assert!(c.terminal_id(b, "legacy").is_err());
        drop(c);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn ssh_foreign_owner_denied_before_worker_contact() {
        use std::os::unix::net::UnixListener;
        let root = std::env::temp_dir().join(format!("ow-ssh-auth-{}", common::nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        let mut c = Catalog::open(&root, "test", None).unwrap();
        let a = c.user(&identity("a", "a@example.test")).unwrap();
        let b = c.user(&identity("b", "b@example.test")).unwrap();
        let physical = c.reserve(a, "machine", "private").unwrap();
        let listener = UnixListener::bind(root.join("control.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        for name in ["private", physical.as_str(), "unknown"] {
            assert!(c.ssh_target(b, name).is_err());
            for op in ["ssh-keys", "ssh-info"] {
                assert!(c.operation(b,json!({"op":op,"id":name,"keys":[],"operation_key":common::nonce().unwrap()})).is_err());
            }
        }
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(c);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn uncertain_mac_create_replay_retains_catalog_and_node_journals() {
        use std::{
            os::unix::net::UnixListener,
            sync::{
                Arc,
                atomic::{AtomicBool, AtomicUsize, Ordering},
            },
            time::Duration,
        };
        let root =
            PathBuf::from("/tmp").join(format!("ow-create-review-{}", common::nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        let remote = root.join("nodes/mac");
        let runtime = remote.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        fn server(
            root: &Path,
            stop: Arc<AtomicBool>,
            mut reply: impl FnMut(Value) -> Value + Send + 'static,
        ) -> std::thread::JoinHandle<()> {
            let listener = UnixListener::bind(root.join("control.sock")).unwrap();
            listener.set_nonblocking(true).unwrap();
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut s, _)) => {
                            let r = wire::line(&mut s).unwrap();
                            wire::send(&mut s, &reply(r)).unwrap();
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2))
                        }
                        Err(e) => panic!("{e}"),
                    }
                }
            })
        }
        let local = server(&root, stop.clone(), |r| {
            if r["op"] == "status" {
                wire::response(Ok(
                    json!({"max_memory_mib":2048,"max_running":1,"max_vcpus":2}),
                ))
            } else {
                wire::response(Ok(json!([])))
            }
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let mut incomplete = None;
        let worker = server(&runtime, stop.clone(), move |r| {
            if r["op"] == "create" {
                count.fetch_add(1, Ordering::SeqCst);
                incomplete = Some(
                    json!({"id":r["id"],"state":"creating","image":"ubuntu-arm64","backend":"apple-virtualization","arch":"aarch64","memory_mib":512,"vcpu_count":1}),
                );
                json!({"ok":false,"uncertain":true,"error":"incomplete create: retained reservation"})
            } else if r["op"] == "list" {
                wire::response(Ok(json!(incomplete.iter().collect::<Vec<_>>())))
            } else if r["op"] == "status" {
                wire::response(Ok(
                    json!({"max_memory_mib":1024,"max_running":1,"max_vcpus":2,"max_guest_memory_mib":2048,"max_guest_vcpus":2}),
                ))
            } else {
                wire::response(Ok(json!([])))
            }
        });
        let journal_root = runtime.clone();
        let proxy = server(&remote, stop.clone(), move |r| {
            nodes::review_journal_request(&journal_root, &r).unwrap()
        });
        let mut c = Catalog::open(&root, "test", None).unwrap();
        let user = c.user(&identity("a", "a@example.test")).unwrap();
        let cap = json!({"backend":"apple-virtualization","arch":"aarch64","runtime":"apple-vz-v1","operations":common::MAC_OPERATIONS,"images":["ubuntu-arm64"],"memory_mib":1024,"slots":1,"vcpus":2,"max_guest_memory_mib":2048,"max_guest_vcpus":2,"guest_ssh_v1":false});
        nodes::db(&root).unwrap().execute("INSERT INTO nodes(id,heartbeat,memory_mib,slots,capabilities) VALUES('mac',?1,1024,1,?2)",params![std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64,cap.to_string()]).unwrap();
        for _ in 0..2 {
            assert!(c.operation(user,json!({"op":"create","id":"partial","node":"mac","image":"ubuntu-arm64","memory_mib":512,"vcpu_count":1,"operation_key":"same-key"})).is_err());
            let state: String =
                c.db.query_row(
                    "SELECT state FROM operations WHERE retry_key='same-key'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(state, "uncertain");
            let reserved: i64 =
                c.db.query_row(
                    "SELECT reserved_mib FROM resources WHERE owner=?1 AND name='partial'",
                    [user],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(reserved >= 512);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let journal = Connection::open(runtime.join("node-operations.sqlite3")).unwrap();
        let response: String = journal
            .query_row("SELECT response FROM effects", [], |r| r.get(0))
            .unwrap();
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["ok"], false);
        assert_eq!(response["uncertain"], true);
        drop(journal);
        drop(c);
        stop.store(true, Ordering::SeqCst);
        for task in [local, worker, proxy] {
            task.join().unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn mac_capability_and_owner_denials_precede_worker_contact() {
        use std::os::unix::net::UnixListener;
        let root =
            std::env::temp_dir().join(format!("ow-mac-catalog-{}", common::nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        let mut c = Catalog::open(&root, "test", None).unwrap();
        let a = c.user(&identity("a", "a@example.test")).unwrap();
        let b = c.user(&identity("b", "b@example.test")).unwrap();
        let physical = c.reserve(a, "machine", "private").unwrap();
        let cap = json!({"backend":"apple-virtualization","arch":"aarch64","runtime":"apple-vz-v1","operations":common::MAC_OPERATIONS,"images":["ubuntu-arm64"],"memory_mib":1024,"slots":1,"vcpus":1,"max_guest_memory_mib":2048,"max_guest_vcpus":2,"guest_ssh_v1":false});
        nodes::db(&root).unwrap().execute("INSERT INTO nodes(id,heartbeat,memory_mib,slots,capabilities) VALUES('mac',?1,1024,1,?2)",params![std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64,cap.to_string()]).unwrap();
        c.db.execute(
            "UPDATE resources SET node='mac' WHERE physical=?1",
            [&physical],
        )
        .unwrap();
        let listener = UnixListener::bind(root.join("control.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let remote = root.join("nodes/mac");
        std::fs::create_dir_all(&remote).unwrap();
        let remote_listener = UnixListener::bind(remote.join("control.sock")).unwrap();
        remote_listener.set_nonblocking(true).unwrap();
        for op in [
            "snapshot",
            "fork",
            "restore",
            "hibernate",
            "ssh-info",
            "ssh-keys",
            "resize",
        ] {
            assert!(
                c.operation(
                    a,
                    json!({"op":op,"id":"private","memory_mib":1024,"vcpu_count":1})
                )
                .is_err(),
                "{op}"
            );
        }
        for op in ["start", "stop", "exec", "put", "get"] {
            assert!(c.operation(b, json!({"op":op,"id":"private"})).is_err());
        }
        assert!(c.terminal_target(b, "private").is_err());
        assert!(c.ssh_target(a, "private").is_err());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(
            remote_listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn newer_schema_opens_only_under_its_reader_floor() {
        let root = fixture(false);
        let owner = identity("owner-sub", "owner@example.test");
        let next = nodes::SCHEMA + 1;
        let set = |sql: &str| {
            Catalog::open(&root, &owner.issuer, None)
                .map(|c| c.db.execute_batch(sql).unwrap())
                .unwrap()
        };
        set(&format!("PRAGMA user_version={next}"));
        let refused = Catalog::open(&root, &owner.issuer, None).err().unwrap();
        assert!(
            refused.to_string().contains("needs a newer binary"),
            "{refused}"
        );
        assert!(nodes::db(&root).is_err());
        let floor = |value: i64| {
            let db = Connection::open(root.join("catalog.sqlite3")).unwrap();
            db.execute(
                "INSERT OR REPLACE INTO settings(key,value) VALUES('min_reader_schema',?1)",
                [value.to_string()],
            )
            .unwrap();
        };
        floor(next);
        assert!(Catalog::open(&root, &owner.issuer, None).is_err());
        floor(nodes::SCHEMA);
        let c = Catalog::open(&root, &owner.issuer, None).unwrap();
        let version: i64 =
            c.db.query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
        assert_eq!(version, next, "an older binary never lowers the version");
        assert!(nodes::db(&root).is_ok());
        drop(c);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn version_one_upgrade_preserves_rows_and_separates_kinds() {
        let root = fixture(false);
        let owner = identity("owner-sub", "owner@example.test");
        let mut c = Catalog::open(&root, &owner.issuer, None).unwrap();
        let user = c.user(&owner).unwrap();
        let physical = c.reserve(user, "machine", "demo").unwrap();
        c.db.execute_batch(
            "ALTER TABLE resources DROP COLUMN node; ALTER TABLE resources DROP COLUMN reserved_mib; ALTER TABLE resources DROP COLUMN reserved_cpus; DROP INDEX operation_retry; ALTER TABLE operations DROP COLUMN node; ALTER TABLE operations DROP COLUMN retry_key; ALTER TABLE operations DROP COLUMN request; ALTER TABLE operations DROP COLUMN response; CREATE UNIQUE INDEX legacy_physical ON resources(physical); PRAGMA user_version=1;",
        )
        .unwrap();
        drop(c);
        let c = Catalog::open(&root, &owner.issuer, None).unwrap();
        let version: i64 =
            c.db.query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
        assert_eq!(version, 3);
        assert_eq!(c.terminal_id(user, "demo").unwrap(), physical);
        c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata) VALUES(?1,'snapshot','demo',?2,'{}')",params![user,physical]).unwrap();
        assert_eq!(c.physical(user, "snapshot", "demo").unwrap(), physical);
        drop(c);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_import_requires_explicit_owner() {
        let root = fixture(true);
        assert!(Catalog::open(&root, "https://example.cloudflareaccess.com", None).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod multinode_tests;
