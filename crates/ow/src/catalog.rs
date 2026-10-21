//! Durable control-plane ownership. The private worker journal remains runtime recovery state.
use crate::{common, limits, nodes, wire};
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
    pub(crate) db: Connection,
    pub(crate) root: PathBuf,
    /// Login emails of pool admins (gateway config `pool_admins`); empty means none.
    pub(crate) pool_admins: Vec<String>,
    _lock: std::fs::File,
}

#[derive(Debug)]
pub struct ResourceNotFound(pub(crate) String);
impl std::fmt::Display for ResourceNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} was not found.", self.0)
    }
}
impl std::error::Error for ResourceNotFound {}

/// A new create intent for a name the user already has; a retry of the original create is not one.
#[derive(Debug)]
pub struct NameTaken(pub String);
impl std::fmt::Display for NameTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "you already have a machine named '{}'; choose another name",
            self.0
        )
    }
}
impl std::error::Error for NameTaken {}

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

/// A refusal before any effect, in words for the person who asked: `code` lets the
/// dashboard recognise it, `message` is shown as is by the dashboard and the CLI.
#[derive(Debug)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Refusal {}

/// A fork lands on its parent's runner, so another runner's free room does not help.
fn fork_room(node: &str, cpus: u64, memory_mib: u64) -> String {
    let runner = if node == "local" {
        "This host".to_owned()
    } else {
        format!("Runner {node}")
    };
    let memory = if memory_mib % 1024 == 0 {
        format!("{} GiB", memory_mib / 1024)
    } else {
        format!("{memory_mib} MiB")
    };
    format!(
        "{runner} has no room for a {cpus} vCPU / {memory} fork. Forks stay on the parent machine's runner; free capacity there (hibernate or stop a machine) and try again."
    )
}
fn fork_refusal(node: &str, cpus: u64, memory_mib: u64) -> anyhow::Error {
    Refusal {
        code: "fork_capacity",
        message: fork_room(node, cpus, memory_mib),
    }
    .into()
}
/// The worker's own reservation limits (ow-core runtime capacity); only matched, never shown.
fn worker_capacity_refusal(diagnostic: &str) -> bool {
    diagnostic.ends_with(" reached")
        && [
            "running workspace limit (",
            "CPU reservation limit (",
            "memory reservation limit (",
        ]
        .iter()
        .any(|p| diagnostic.starts_with(p))
}

/// Only known diagnostics cross the human API boundary. Raw errors stay in the private
/// operation response and host journal, where the operation ID makes them discoverable.
/// Metadata of a fork child that was never made: failed, flagged so quotas skip it. A
/// worker inventory that lists the machine after all replaces this metadata.
const UNMADE: &str = "metadata=json_set(metadata,'$.state','failed','$.unmade',json('true')),reserved_mib=0,reserved_cpus=0";
/// A create or fork its runner still lacks after this long is failed, not left pending.
const CREATION_TIMEOUT_MINUTES: u32 = 30;
const CREATION_TIMEOUT: &str = "Creation did not complete within 30 minutes and the runner has no such machine; it failed. Delete it, or retry with a new operation key.";

/// The guest ends the command (TERM, then KILL) once it outlives the serial timeout.
pub(crate) const COMMAND_TIMEOUT: &str = "Stopped after 30 s: commands in the console are limited to 30 s. Use Open Terminal (ow shell) for long-running work.";

fn public_operation_error<'a>(op: &str, diagnostic: &'a str) -> &'a str {
    match (op, diagnostic) {
        (_, CREATION_TIMEOUT) => CREATION_TIMEOUT,
        ("fork", d) if d.starts_with("fork source is ") && d.ends_with("; start the box first (ow start), or fork from one of its snapshots") => {
            "Box is not running. Start it first with ow start, or fork from one of its snapshots, then retry with a new operation key."
        }
        (_, d) if d.starts_with("incompatible stored RAM;") => "Saved RAM is incompatible. Retry wake with a new operation key and explicit --accept-cold; the old RAM artifact is retained.",
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
        (_, "incompatible stored RAM; retry with a new operation key and explicit --accept-cold consent") => diagnostic,
        (_, "snapshot limit (24) reached") => {
            "Named snapshot limit (24) reached. Remove unneeded snapshots on the host before retrying with a new operation key."
        }
        (_, "incomplete snapshot exists; preserve and inspect it") => diagnostic,
        (_, "guest command timed out after 30s") => COMMAND_TIMEOUT,
        (_, "node route unavailable before dispatch") => {
            "Node is unavailable; no operation was dispatched. Retry with the same operation key when it reconnects."
        }
        (_, "image pull unavailable before create dispatch") => {
            "Image download failed; no create was dispatched. Retry with the same operation key."
        }
        _ => first_use_error(diagnostic).map(|(_,message)| message).unwrap_or(
            "Operation did not complete. Check its status before retrying; use the same retry key. The host journal records the cause under this operation ID."),
    }
}

/// Exact allowlist only: arbitrary worker diagnostics can contain private host data.
pub fn first_use_error(diagnostic: &str) -> Option<(&'static str, &'static str)> {
    Some(match diagnostic {
        "connection wake requires hibernation; stopped needs explicit start" => (
            "stopped",
            "Box is deliberately stopped. Use ow start or ow wake explicitly before connecting.",
        ),
        "no online node available" | "node offline or revoked; placement remains fixed" => (
            "node_unavailable",
            "No usable node is online. Check ow nodes; wait for the intended runner to reconnect. Retry an uncertain mutation with its original operation key.",
        ),
        "node lacks prepared image" => (
            "unavailable_image",
            "Image is not prepared on an online node. Run ow image list and choose an available image; do not build or enroll a host without operator approval.",
        ),
        "node capacity exhausted"
        | "host disk cap exhausted (OW_MAX_DISK_GIB)"
        | "storage headroom exhausted (full disk reservations plus host reserve)" => (
            "insufficient_capacity",
            "Placement or disk capacity is exhausted. Check ow nodes and image list; wait or choose fitting resources. Inspect the original operation before retrying; reuse its key after an uncertain response.",
        ),
        "node capability does not support create"
        | "runner does not support disk sizing; upgrade its worker and agent first (Linux Firecracker only)"
        | "shape exceeds worker per-guest ceiling"
        | "unsupported Mac guest shape" => (
            "unsupported_capability",
            "Runner does not support the requested capability or shape. Check ow status and ow image list; choose an advertised choice or ask the operator to upgrade.",
        ),
        _ => return None,
    })
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
    let email = request["with"]
        .as_str()
        .context("teammate email required")?
        .to_ascii_lowercase();
    common::email(&email)?;
    // Resolve ownership under the grant writer lock so deletion cannot leave a late grant.
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let physical: String = tx
        .query_row(
            "SELECT physical FROM resources WHERE owner=?1 AND kind='machine' AND name=?2",
            params![user, name],
            |r| r.get(0),
        )
        .optional()?
        .context("resource not found")?;
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
    if grantee == Some(user) {
        return Err(Refusal {
            code: "already_owner",
            message: "You already own this machine; share it with a teammate login.".into(),
        }
        .into());
    }
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
    nodes::readable(&db)?;
    let user = user_on(&mut db, identity)?;
    share_on(&mut db, user, op, request)
}

/// What a grantee may do on a machine shared with them. Resize, delete, rename, transfer and
/// sharing stay with the owner. A pool admin reaches any box to transfer, fork or manage its AWS role.
fn permitted(role: &str, op: &str) -> bool {
    match role {
        "owner" => true,
        "pool-admin" => matches!(op, "transfer" | "fork" | "aws-role"),
        "admin" => !matches!(op, "resize" | "delete" | "rename" | "transfer"),
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
        if version > 0 && version < nodes::SCHEMA {
            let backup = root.join(format!(
                "catalog.pre-placement-{}.sqlite3",
                common::nonce()?
            ));
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&backup)?;
            db.execute(
                "VACUUM main INTO ?1",
                [backup.to_str().context("backup path")?],
            )
            .context("private pre-placement catalog backup failed; migration refused")?;
        }
        db.execute_batch("BEGIN IMMEDIATE;
          CREATE TABLE IF NOT EXISTS users(id INTEGER PRIMARY KEY, issuer TEXT NOT NULL, subject TEXT, email TEXT NOT NULL, UNIQUE(issuer,subject), UNIQUE(issuer,email));
          CREATE TABLE IF NOT EXISTS resources(owner INTEGER NOT NULL REFERENCES users(id), kind TEXT NOT NULL CHECK(kind IN ('machine','snapshot')), name TEXT NOT NULL, physical TEXT NOT NULL, metadata TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY(owner,kind,name), UNIQUE(kind,physical));
          CREATE TABLE IF NOT EXISTS operations(id INTEGER PRIMARY KEY, owner INTEGER NOT NULL REFERENCES users(id), op TEXT NOT NULL, resource TEXT NOT NULL, state TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, finished_at TEXT);
          CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
          -- Additive, like node_usage: an older binary ignores grants, so grantees lose access.
          -- Never reassign an origin to another box/port: browser storage and service workers persist.
          CREATE TABLE IF NOT EXISTS publications(https_port INTEGER PRIMARY KEY CHECK(https_port BETWEEN 11000 AND 65535),physical TEXT NOT NULL,guest_port INTEGER NOT NULL CHECK(guest_port BETWEEN 1 AND 65535),active INTEGER NOT NULL DEFAULT 0,UNIQUE(physical,guest_port));
          CREATE TABLE IF NOT EXISTS grants(physical TEXT NOT NULL, grantee INTEGER NOT NULL REFERENCES users(id), role TEXT NOT NULL CHECK(role IN ('use','admin')), created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY(physical,grantee));
          -- Additive too: ownership changes (transfer, create or fork for another owner), shown to
          -- the actor and both owners. An older binary neither writes nor shows it.
          CREATE TABLE IF NOT EXISTS audit(id INTEGER PRIMARY KEY, operation INTEGER NOT NULL UNIQUE, at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, actor INTEGER NOT NULL REFERENCES users(id), action TEXT NOT NULL CHECK(action IN ('transfer','create','fork')), machine TEXT NOT NULL, source TEXT, from_owner INTEGER REFERENCES users(id), to_owner INTEGER NOT NULL REFERENCES users(id), keep_access INTEGER NOT NULL DEFAULT 0);
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
        crate::image_catalog::initialize(&db)?;
        if version < 5 {
            db.execute_batch("ALTER TABLE resources ADD COLUMN placement_managed INTEGER NOT NULL DEFAULT 0 CHECK(placement_managed IN (0,1));
              ALTER TABLE resources ADD COLUMN placement_epoch INTEGER NOT NULL DEFAULT 0 CHECK(placement_epoch>=0);
              ALTER TABLE resources ADD COLUMN current_capture TEXT REFERENCES artifacts(id);
              ALTER TABLE resources ADD COLUMN ssh_policy TEXT CHECK(ssh_policy IS NULL OR json_valid(ssh_policy));
              ALTER TABLE resources ADD COLUMN ssh_policy_generation INTEGER NOT NULL DEFAULT 0 CHECK(ssh_policy_generation>=0);
              CREATE TABLE transfers(id INTEGER PRIMARY KEY,operation INTEGER NOT NULL UNIQUE REFERENCES operations(id),
                kind TEXT NOT NULL CHECK(kind IN ('move','wake','park','fork','restore')),
                source TEXT NOT NULL,machine TEXT NOT NULL,source_node TEXT NOT NULL,source_epoch INTEGER NOT NULL,
                source_capture TEXT REFERENCES artifacts(id),source_policy_generation INTEGER NOT NULL,
                target TEXT,epoch INTEGER NOT NULL UNIQUE CHECK(epoch>source_epoch),capture TEXT REFERENCES artifacts(id),
                step TEXT NOT NULL CHECK(step IN ('preparing','exported','importing','import_uncertain','prepared','committed','start_uncertain','started','succeeded','aborted','failed')));
              CREATE TABLE transfer_locks(machine TEXT PRIMARY KEY,transfer INTEGER NOT NULL REFERENCES transfers(id));
              CREATE TABLE transfer_reservations(transfer INTEGER PRIMARY KEY REFERENCES transfers(id),node TEXT NOT NULL,epoch INTEGER NOT NULL UNIQUE,
                memory_mib INTEGER NOT NULL CHECK(memory_mib>0),cpus INTEGER NOT NULL CHECK(cpus>0),disk_bytes INTEGER NOT NULL CHECK(disk_bytes>=0));
              INSERT INTO settings(key,value) VALUES('min_reader_schema','5') ON CONFLICT(key) DO UPDATE SET value=MAX(CAST(value AS INTEGER),5);")?;
        }

        // Additive guards are enforced by SQLite even for prior schema-5 writers.
        crate::cache_catalog::initialize(&db)?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS node_usage(node TEXT PRIMARY KEY,extra_mib INTEGER NOT NULL DEFAULT 0,extra_cpus INTEGER NOT NULL DEFAULT 0,extra_slots INTEGER NOT NULL DEFAULT 0);")?;
        crate::drain::initialize(&db)?;
        crate::store_gc::initialize(&db)?;
        crate::scaler::initialize(&db)?;
        crate::aws_identity::initialize(&db)?;
        if version < 7 {
            db.execute_batch("INSERT INTO settings(key,value) VALUES('min_reader_schema','7') ON CONFLICT(key) DO UPDATE SET value=MAX(CAST(value AS INTEGER),7);")?;
        }
        if version < 6 {
            db.execute_batch("INSERT INTO settings(key,value) VALUES('min_reader_schema','6') ON CONFLICT(key) DO UPDATE SET value=MAX(CAST(value AS INTEGER),6);")?;
        }

        // A newer catalog this binary may read (its floor allows it) keeps its version.
        db.execute_batch(&format!("UPDATE resources SET reserved_mib=COALESCE(json_extract(metadata,'$.memory_mib'),0) WHERE kind='machine' AND placement_managed=0 AND reserved_mib=0 AND json_extract(metadata,'$.state')='running'; PRAGMA user_version={}; COMMIT;", version.max(nodes::SCHEMA)))?;
        nodes::db(root)?;
        // The exclusive gateway lock proves the previous catalog owner is gone.
        db.execute(
            "UPDATE operations SET state='uncertain' WHERE state='pending'",
            [],
        )?;
        // A persisted pending effect may have reached the worker before this process died.

        let mut catalog = Self {
            db,
            root: root.into(),
            pool_admins: Vec::new(),
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
        catalog.db.execute("UPDATE resources SET reserved_mib=MAX(reserved_mib,COALESCE(json_extract(metadata,'$.memory_mib'),0)),reserved_cpus=MAX(reserved_cpus,COALESCE(json_extract(metadata,'$.vcpu_count'),1)) WHERE kind='machine' AND placement_managed=0 AND json_extract(metadata,'$.state')='running'",[])?;
        limits::initialize(&catalog.db)?;
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
            .ok_or_else(|| ResourceNotFound(format!("{kind} '{name}'")).into())
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
    pub(crate) fn machine(&self, user: i64, address: &str, op: &str) -> Result<String> {
        Ok(self.access(user, address, op)?.0)
    }
    /// `machine`, with the role it was reached by: owner, a grant's role, or pool-admin.
    pub(crate) fn access(&self, user: i64, address: &str, op: &str) -> Result<(String, String)> {
        let Some((owner, name)) = address.split_once('/') else {
            return Ok((self.physical(user, "machine", address)?, "owner".into()));
        };
        common::address(address)?;
        let admin = permitted("pool-admin", op) && self.pool_admin(user)?;
        let (physical, role): (String, String) = self
            .db
            .query_row(
                "SELECT r.physical, CASE WHEN r.owner=?1 THEN 'owner' ELSE COALESCE(g.role,'pool-admin') END FROM resources r JOIN users u ON u.id=r.owner LEFT JOIN grants g ON g.physical=r.physical AND g.grantee=?1 WHERE r.kind='machine' AND u.issuer=(SELECT issuer FROM users WHERE id=?1) AND u.email=?2 AND r.name=?3 AND (r.owner=?1 OR g.role IS NOT NULL OR ?4)",
                params![user, owner.to_ascii_lowercase(), name, admin],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| ResourceNotFound(format!("machine '{address}'")))?;
        // A pool admin a box is also shared with still acts as pool admin.
        let role = if admin && !permitted(&role, op) {
            "pool-admin".into()
        } else {
            role
        };
        ensure!(
            permitted(&role, op),
            "{op} is not allowed on a machine shared with you as {role}"
        );
        Ok((physical, role))
    }
    /// Whether this signed-in user's bound login email is a configured pool admin.
    pub(crate) fn pool_admin(&self, user: i64) -> Result<bool> {
        let email: Option<String> = self
            .db
            .query_row(
                "SELECT email FROM users WHERE id=?1 AND subject IS NOT NULL",
                [user],
                |r| r.get(0),
            )
            .optional()?;
        Ok(email.is_some_and(|e| self.pool_admins.iter().any(|a| a.eq_ignore_ascii_case(&e))))
    }
    /// The user a login email names, in the caller's identity namespace. An email nobody has
    /// used yet is reserved, as for sharing: its first verified login binds it.
    pub(crate) fn user_by_email(&self, user: i64, email: &str) -> Result<i64> {
        let email = email.to_ascii_lowercase();
        common::email(&email)?;
        let issuer: String =
            self.db
                .query_row("SELECT issuer FROM users WHERE id=?1", [user], |r| r.get(0))?;
        self.db.execute(
            "INSERT INTO users(issuer,email) VALUES(?1,?2) ON CONFLICT(issuer,email) DO NOTHING",
            params![issuer, email],
        )?;
        Ok(self.db.query_row(
            "SELECT id FROM users WHERE issuer=?1 AND email=?2",
            params![issuer, email],
            |r| r.get(0),
        )?)
    }
    fn email(&self, user: i64) -> Result<String> {
        Ok(self
            .db
            .query_row("SELECT email FROM users WHERE id=?1", [user], |r| r.get(0))?)
    }
    /// Record an ownership change for the actor and both owners to see. A replay keeps one row.
    #[allow(clippy::too_many_arguments)]
    fn audit(
        &self,
        operation: i64,
        actor: i64,
        action: &str,
        machine: &str,
        source: Option<&str>,
        from: Option<i64>,
        to: i64,
        keep_access: bool,
    ) -> Result<()> {
        self.db.execute("INSERT INTO audit(operation,actor,action,machine,source,from_owner,to_owner,keep_access) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(operation) DO NOTHING",params![operation,actor,action,machine,source,from,to,keep_access])?;
        Ok(())
    }
    /// Only a machine's owner or a pool admin forks it into someone else's name space.
    pub(crate) fn fork_for_others(&self, user: i64, address: &str) -> Result<()> {
        let (_, role) = self.access(user, address, "fork")?;
        if !matches!(role.as_str(), "owner" | "pool-admin") && !self.pool_admin(user)? {
            return Err(limits::Rejection(
                "only the machine's owner or a pool admin can fork it for someone else".into(),
            )
            .into());
        }
        Ok(())
    }
    /// Whose named snapshots of a machine a fork may start from: a pool admin uses the box
    /// owner's, everyone else their own.
    pub(crate) fn snapshot_owner(&self, user: i64, address: &str) -> Result<i64> {
        let (physical, role) = self.access(user, address, "fork")?;
        if role == "owner" || !self.pool_admin(user)? {
            return Ok(user);
        }
        Ok(self.db.query_row(
            "SELECT owner FROM resources WHERE kind='machine' AND physical=?1",
            [physical],
            |r| r.get(0),
        )?)
    }
    /// Audit a fork when it made a box for someone else, or a pool admin forked another
    /// owner's box (even one shared with them). `operation` is the requester's.
    fn audit_fork(&self, operation: i64, source: &str, child: &str) -> Result<bool> {
        let actor: i64 = self.db.query_row(
            "SELECT owner FROM operations WHERE id=?1",
            [operation],
            |r| r.get(0),
        )?;
        let (from, source): (i64, String) = self.db.query_row(
            "SELECT owner,name FROM resources WHERE kind='machine' AND physical=?1",
            [source],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let (to, child): (i64, String) = self.db.query_row(
            "SELECT owner,name FROM resources WHERE kind='machine' AND physical=?1",
            [child],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let audited = to != actor || from != actor && self.pool_admin(actor)?;
        if audited {
            self.audit(
                operation,
                actor,
                "fork",
                &child,
                Some(&source),
                Some(from),
                to,
                false,
            )?;
        }
        Ok(audited)
    }
    fn audit_log(&self, user: i64) -> Result<Value> {
        let mut stmt = self.db.prepare("SELECT CAST(strftime('%s',a.at) AS INTEGER),a.action,a.machine,a.source,ua.email,uf.email,ut.email,a.keep_access FROM audit a JOIN users ua ON ua.id=a.actor LEFT JOIN users uf ON uf.id=a.from_owner JOIN users ut ON ut.id=a.to_owner WHERE ?1 IN (a.actor,a.from_owner,a.to_owner) ORDER BY a.id DESC LIMIT 100")?;
        let rows = stmt.query_map([user], |r| {
            Ok(json!({"at":r.get::<_,i64>(0)?,"action":r.get::<_,String>(1)?,"machine":r.get::<_,String>(2)?,"source":r.get::<_,Option<String>>(3)?,"by":r.get::<_,String>(4)?,"from":r.get::<_,Option<String>>(5)?,"to":r.get::<_,String>(6)?,"keep_access":r.get::<_,bool>(7)?}))
        })?;
        Ok(json!(rows.collect::<rusqlite::Result<Vec<_>>>()?))
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
            ("fork", "machine"),
            ("hibernation", "snapshot"),
        ] {
            if let Some(physical) = value[key].as_str() {
                value[key] = json!(self.logical(user, kind, physical)?);
            }
        }
        Ok(value)
    }

    /// Catalog policy only; the caller must reconcile the worker before enabling managed access.
    #[allow(dead_code)]
    pub(crate) fn set_transfer_ssh_policy(
        &self,
        user: i64,
        name: &str,
        generation: i64,
        policy: &crate::transfers::PublicSshPolicy,
    ) -> Result<()> {
        if let crate::transfers::PublicSshPolicy::Enrolled(p) = policy {
            common::identifier(&p.identity)?;
            common::identifier(&p.generation)?;
            ensure!(
                ow_core::ssh_guest::key(&p.host_key)? == p.host_key,
                "noncanonical SSH host key"
            );
            ensure!(
                ow_core::ssh_guest::validate_keys(&json!(p.keys))? == p.keys,
                "noncanonical SSH keys"
            );
        }
        let text = serde_json::to_string(policy)?;
        ensure!(text.len() <= 16384, "SSH policy too large");
        self.transfer_write(|c| {
            let physical = c.machine(user,name,"ssh-keys")?;
            ensure!(c.db.execute("UPDATE resources SET ssh_policy=?1,ssh_policy_generation=ssh_policy_generation+1 WHERE kind='machine' AND physical=?2 AND ssh_policy_generation=?3",params![text,physical,generation])? == 1,"stale SSH policy");
            Ok(())
        })
    }

    // SQLite-only transitions: transport and artifact verification happen before entry.
    fn transfer_write<T>(&self, write: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        let tx = rusqlite::Transaction::new_unchecked(
            &self.db,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let result = write(self)?;
        tx.commit()?;
        Ok(result)
    }
    #[allow(dead_code)]
    pub(crate) fn transfer(&self, id: i64) -> Result<crate::transfers::Transfer> {
        use crate::transfers::*;
        let row = self.db.query_row("SELECT id,operation,kind,source,machine,source_node,source_epoch,source_capture,target,epoch,capture,step FROM transfers WHERE id=?1", [id], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,i64>(6)?,r.get::<_,Option<String>>(7)?,r.get::<_,Option<String>>(8)?,r.get::<_,i64>(9)?,r.get::<_,Option<String>>(10)?,r.get::<_,String>(11)?)))?;
        Ok(Transfer {
            id: row.0,
            operation: row.1,
            kind: serde_json::from_value(json!(row.2))?,
            source: row.3,
            machine: row.4,
            source_node: row.5,
            source_epoch: row.6,
            source_capture: row.7,
            target: row.8,
            epoch: row.9,
            capture: row.10,
            step: serde_json::from_value(json!(row.11))?,
        })
    }
    #[allow(dead_code)]
    fn transfer_capture(&self, capture: &str, owner: i64, source: &str) -> Result<()> {
        ensure!(self.db.query_row("SELECT EXISTS(SELECT 1 FROM artifacts WHERE id=?1 AND owner=?2 AND machine=?3 AND kind IN ('capture','snapshot') AND state='stored' AND digest IS NOT NULL)",params![capture,owner,source],|r|r.get::<_,bool>(0))?, "capture is not a committed artifact of this machine and owner");
        Ok(())
    }
    #[allow(dead_code)]
    pub(crate) fn begin_transfer(
        &self,
        user: i64,
        key: &str,
        intent: &crate::transfers::Intent,
    ) -> Result<crate::transfers::Transfer> {
        use crate::transfers::*;
        common::identifier(key)?;
        common::address(&intent.machine)?;
        ensure!(
            if intent.kind == Kind::Park {
                intent.target.is_none()
            } else {
                intent.target.is_some() || intent.kind == Kind::Wake
            },
            "park cannot have a target; other transfers need one"
        );
        ensure!(
            (intent.kind == Kind::Fork) == intent.child.is_some(),
            "only fork reserves a child"
        );
        ensure!(
            intent.memory_mib > 0 && intent.cpus > 0 && intent.disk_bytes <= i64::MAX as u64,
            "invalid target shape"
        );
        ensure!(
            intent.snapshot.is_none() || intent.kind == Kind::Fork,
            "only fork selects a snapshot"
        );
        let fingerprint = json!({"transfer":intent}).to_string();
        self.transfer_write(|c| {
            // Recheck current ownership/sharing even for a joined operation.
            let source = c.machine(user, &intent.machine, intent.kind.permission())?;
            let (owner, source_node, source_epoch, source_capture, policy_generation):(i64,String,i64,Option<String>,i64) = c.db.query_row("SELECT owner,node,placement_epoch,current_capture,ssh_policy_generation FROM resources WHERE kind='machine' AND physical=?1",[&source],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
            ensure!(matches!(intent.kind, Kind::Fork | Kind::Wake) || owner == user, "placement changes require the owner");
            if let Some((operation, previous)) = c.db.query_row("SELECT id,request FROM operations WHERE owner=?1 AND retry_key=?2",params![user,key],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?))).optional()? {
                ensure!(previous == fingerprint, "operation key conflicts with original parameters");
                let id = c.db.query_row("SELECT id FROM transfers WHERE operation=?1",[operation],|r|r.get(0))?;
                return c.transfer(id);
            }
            ensure!(!c.db.query_row("SELECT EXISTS(SELECT 1 FROM transfer_locks WHERE machine=?1)",[&source],|r|r.get::<_,bool>(0))?, limits::Rejection("Box has an unfinished move, fork or wake; wait for it or inspect ow status.".into()));
            ensure!(!c.db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE (owner=?1 AND resource=(SELECT name FROM resources WHERE kind='machine' AND physical=?2) OR json_extract(request,'$.request.id')=?2 OR json_extract(request,'$.request.child')=?2) AND state IN ('pending','uncertain'))",params![owner,source],|r|r.get::<_,bool>(0))?, limits::Rejection("Box has a pending or uncertain operation; resolve it (see ow status) before moving or forking.".into()));
            ensure!(intent.kind == Kind::Fork || c.db.query_row("SELECT ssh_policy IS NOT NULL FROM resources WHERE kind='machine' AND physical=?1",[&source],|r|r.get::<_,bool>(0))?,"reconcile public SSH policy before managing placement");
            ensure!(intent.kind != Kind::Move || intent.target.as_deref() != Some(&source_node),"move requires another node");
            if intent.kind == Kind::Wake {
                ensure!(source_node.is_empty(), "wake requires a parked placement");
                let state:String = c.db.query_row("SELECT json_extract(metadata,'$.state') FROM resources WHERE kind='machine' AND physical=?1",[&source],|r|r.get(0))?;
                ensure!((state == "hibernated" || state == "stopped" && intent.explicit_start), "connection wake requires hibernation; stopped needs explicit start");
            }
            if let Some(capture) = &intent.capture {
                c.transfer_capture(capture, owner, &source)?;
            }
            ensure!(intent.kind.needs_fence() || intent.kind == Kind::Fork || intent.capture.is_some(), "stored capture required");
            let fork_snapshot = if let Some(snapshot) = &intent.snapshot {
                common::identifier(snapshot)?;
                // A pool admin forks the box owner's named snapshots; others their own.
                let snapshots = c.snapshot_owner(user, &intent.machine)?;
                // A stored snapshot forks from its artifact, wherever its runner copy is.
                let physical = c.db.query_row("SELECT physical FROM resources WHERE owner=?1 AND kind='snapshot' AND name=?2 AND (node=?3 OR ?8 IS NOT NULL) AND json_extract(metadata,'$.workspace')=?4 AND json_extract(metadata,'$.memory_mib')=?5 AND json_extract(metadata,'$.vcpu_count')=?6 AND json_extract(metadata,'$.disk_bytes')=?7",params![snapshots,snapshot,source_node,source,intent.memory_mib,intent.cpus,intent.disk_bytes as i64,intent.capture],|r|r.get::<_,String>(0)).context("snapshot identity or shape changed during admission")?;
                if let Some(capture) = &intent.capture {
                    ensure!(c.db.query_row("SELECT local_name=?2 AND kind='snapshot' FROM artifacts WHERE id=?1",params![capture,physical],|r|r.get::<_,bool>(0))?,"stored capture is not this snapshot");
                }
                Some(physical)
            } else { None };
            // Global high-watermark survives aborts, deleted children and controller restarts.
            c.db.execute("INSERT INTO settings(key,value) VALUES('transfer_epoch','0') ON CONFLICT(key) DO NOTHING",[])?;
            let high = c.db.query_row("SELECT value FROM settings WHERE key='transfer_epoch'",[],|r|r.get::<_,String>(0))?.parse::<i64>().context("invalid epoch high-watermark")?;
            ensure!(high >= 0,"invalid epoch high-watermark");
            let epoch = high.max(source_epoch).checked_add(1).context("placement epoch exhausted")?;
            c.db.execute("UPDATE settings SET value=?1 WHERE key='transfer_epoch'",[epoch.to_string()])?;
            let machine = if let Some(child) = &intent.child {
                common::identifier(&child.id)?;
                common::identifier(&child.name)?;
                // INSERT, never reserve/reuse: an existing ID or logical name is a conflict.
                c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node) VALUES(?1,'machine',?2,?3,'{\"state\":\"pending\"}','')",params![child.owner.unwrap_or(user),child.name,child.id])?;
                c.db.execute("UPDATE resources SET ssh_policy='\"disabled\"' WHERE kind='machine' AND physical=?1",[&child.id])?;
                limits::admit(&c.root,&c.db,&child.id,"fork",&json!({}),&json!({"memory_mib":intent.memory_mib,"vcpu_count":intent.cpus,"disk_bytes":intent.disk_bytes}))?;
                child.id.clone()
            } else { source.clone() };
            c.db.execute("INSERT INTO operations(owner,op,resource,state,node,retry_key,request) VALUES(?1,?2,?3,'pending',?4,?5,?6)",params![user,intent.kind.operation(),intent.child.as_ref().map(|ch|ch.name.as_str()).unwrap_or(&intent.machine),intent.target.as_deref().unwrap_or(""),key,fingerprint])?;
            let operation = c.db.last_insert_rowid();
            c.db.execute("INSERT INTO transfers(operation,kind,source,machine,source_node,source_epoch,source_capture,source_policy_generation,target,epoch,capture,step) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'preparing')",params![operation,intent.kind.operation(),source,machine,source_node,source_epoch,source_capture,policy_generation,intent.target,epoch,intent.capture])?;
            let id = c.db.last_insert_rowid();
            if intent.kind == Kind::Fork {
                let snapshot = fork_snapshot.unwrap_or_else(|| format!("transfer-{id}-fork"));
                c.db.execute("UPDATE operations SET response=json_object('fork_snapshot',?1,'requested_snapshot',?2) WHERE id=?3",params![snapshot,intent.snapshot,operation])?;
            }
            for affected in if machine == source { vec![&source] } else { vec![&source,&machine] } {
                c.db.execute("INSERT INTO transfer_locks(machine,transfer) VALUES(?1,?2)",params![affected,id])?;
            }
            if let Some(target) = &intent.target {
                c.reserve_transfer(&c.transfer(id)?, target, intent)?;
            }
            c.transfer(id)
        })
    }
    fn reserve_transfer(
        &self,
        t: &crate::transfers::Transfer,
        target: &str,
        intent: &crate::transfers::Intent,
    ) -> Result<()> {
        let c = self;
        ensure!(
            !crate::drain::blocked(&c.db, target)?,
            limits::Rejection(format!("Runner {target} is draining."))
        );
        // Catalog capacity only; T3 additionally validates worker capability and disk headroom.
        let (limit, slots, cpus):(i64,i64,i64) = c.db.query_row("SELECT MIN(memory_mib,COALESCE(json_extract(capabilities,'$.memory_mib'),0)),MIN(slots,COALESCE(json_extract(capabilities,'$.slots'),0)),MIN(COALESCE(json_extract(capabilities,'$.vcpus'),0),COALESCE((SELECT vcpu_cap FROM node_capacity_limits WHERE node=nodes.id),(SELECT vcpu_cap FROM node_limits WHERE node=nodes.id),16)) FROM nodes WHERE id=?1 AND revoked=0 AND heartbeat<=CAST(strftime('%s','now') AS INTEGER) AND heartbeat>CAST(strftime('%s','now') AS INTEGER)-15 AND COALESCE(json_extract(capabilities,'$.drain'),0)=0 AND NOT EXISTS(SELECT 1 FROM node_placement_policy p WHERE p.node=nodes.id AND (p.active=0 OR p.controller_drain=1))",[target],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?.ok_or_else(|| limits::Rejection(format!("Runner {target} is offline, draining or has no registered budget.")))?;
        let (used_mib,used_cpus,used_slots):(i64,i64,i64) = c.db.query_row("SELECT COALESCE(SUM(reserved_mib),0),COALESCE(SUM(reserved_cpus),0),COALESCE(SUM(reserved_mib>0),0) FROM resources WHERE kind='machine' AND node=?1 AND (?2=0 OR physical<>?3)",params![target,t.kind==crate::transfers::Kind::Move && target==t.source_node,t.source],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let (extra_mib, extra_cpus, extra_slots) = c.extra_usage(target)?;
        let fits = used_mib as u64 + extra_mib + intent.memory_mib as u64 <= limit as u64
            && used_cpus as u64 + extra_cpus + intent.cpus as u64 <= cpus as u64
            && used_slots as u64 + extra_slots < slots as u64;
        if !fits && t.kind == crate::transfers::Kind::Fork && target == t.source_node {
            return Err(fork_refusal(
                target,
                intent.cpus.into(),
                intent.memory_mib.into(),
            ));
        }
        ensure!(
            fits,
            limits::Rejection(format!(
                "Runner {target} lacks free memory, vCPUs or box slots for this box."
            ))
        );
        c.db.execute("INSERT INTO transfer_reservations(transfer,node,epoch,memory_mib,cpus,disk_bytes) VALUES(?1,?2,?3,?4,?5,?6)",params![t.id,target,t.epoch,intent.memory_mib,intent.cpus,intent.disk_bytes as i64])?;
        Ok(())
    }
    pub(crate) fn reserve_wake(
        &self,
        id: i64,
        target: &str,
        intent: &crate::transfers::Intent,
    ) -> Result<()> {
        self.transfer_write(|c| {
            let t = c.transfer(id)?;
            ensure!(
                t.kind == crate::transfers::Kind::Wake
                    && t.step == crate::transfers::Step::Preparing
                    && t.target.is_none(),
                "wake already reserved"
            );
            c.reserve_transfer(&t, target, intent)?;
            c.db.execute(
                "UPDATE transfers SET target=?1 WHERE id=?2",
                params![target, id],
            )?;
            c.db.execute(
                "UPDATE operations SET node=?1,state='pending',response=NULL WHERE id=?2",
                params![target, t.operation],
            )?;
            // The committed park already fenced the source; exported here waits for a verified pull.
            c.db.execute("UPDATE transfers SET step='exported' WHERE id=?1", [id])?;
            Ok(())
        })
    }
    /// Resume the same fenced effects under a fresh owner authorization after grant revocation.
    /// Never release an uncertain target, change its epoch, or restore the revoked grant.
    pub(crate) fn resume_wake(
        &self,
        user: i64,
        key: &str,
        address: &str,
        id: i64,
        explicit_start: bool,
        accept_cold: bool,
    ) -> Result<crate::transfers::Transfer> {
        use crate::transfers::*;
        self.transfer_write(|c|{
            let t=c.transfer(id)?;
            ensure!(t.kind==Kind::Wake && !t.step.terminal(),"wake is not resumable");
            ensure!(c.machine(user,address,"start")?==t.source,"source changed");
            ensure!(c.db.query_row("SELECT owner=?2 AND ssh_policy_generation=(SELECT source_policy_generation FROM transfers WHERE id=?3) FROM resources WHERE kind='machine' AND physical=?1",params![t.source,user,id],|r|r.get::<_,bool>(0))?,"only the current owner may resume; policy changes need reconciliation");
            let (actor,old_address,request,response):(i64,String,String,Option<String>)=c.db.query_row("SELECT owner,json_extract(request,'$.transfer.machine'),request,response FROM operations WHERE id=?1",[t.operation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            ensure!(!c.machine(actor,&old_address,"start").is_ok_and(|id|id==t.source),"wake initiator still authorized");
            let mut original:serde_json::Value=serde_json::from_str(&request)?;
            let mut intent:Intent=serde_json::from_value(original["transfer"].take())?;
            ensure!(intent.accept_cold==accept_cold,"owner must explicitly confirm existing cold consent");
            ensure!(explicit_start || c.db.query_row("SELECT json_extract(metadata,'$.state')<>'stopped' FROM resources WHERE kind='machine' AND physical=?1",[&t.source],|r|r.get::<_,bool>(0))?,"connection wake requires hibernation; stopped needs explicit start");
            intent.machine=address.into();intent.explicit_start=explicit_start;
            let mut progress:serde_json::Value=response.and_then(|s|serde_json::from_str(&s).ok()).unwrap_or(json!({}));
            progress.as_object_mut().context("invalid operation response")?.remove("error");
            let fingerprint=json!({"transfer":intent}).to_string();
            let joined:Option<i64>=c.db.query_row("SELECT id FROM operations WHERE owner=?1 AND retry_key=?2 AND state='joined' AND request=?3 AND json_extract(response,'$.wake_transfer')=?4",params![user,key,fingerprint,id],|r|r.get(0)).optional()?;
            let operation=if let Some(operation)=joined {
                c.db.execute("UPDATE operations SET state='pending',node=?1,response=?2 WHERE id=?3",params![t.target.as_deref().unwrap_or(""),progress.to_string(),operation])?;
                operation
            } else {
                c.db.execute("INSERT INTO operations(owner,op,resource,state,node,retry_key,request,response) VALUES(?1,'wake',?2,'pending',?3,?4,?5,?6)",params![user,address,t.target.as_deref().unwrap_or(""),key,fingerprint,progress.to_string()])?;
                c.db.last_insert_rowid()
            };
            c.db.execute("UPDATE operations SET state='failed',finished_at=CURRENT_TIMESTAMP,response=json_set(COALESCE(response,'{}'),'$.error','Wake authorization revoked; owner resumed the transfer.','$.resumed_by',?1) WHERE id=?2",params![operation,t.operation])?;
            ensure!(c.db.execute("UPDATE transfers SET operation=?1 WHERE id=?2 AND operation=?3 AND EXISTS(SELECT 1 FROM transfer_locks WHERE transfer=?2 AND machine=?4)",params![operation,id,t.operation,t.machine])?==1,"wake authority changed");
            c.transfer(id)
        })
    }
    #[allow(dead_code)]
    pub(crate) fn advance_transfer(
        &self,
        id: i64,
        epoch: i64,
        expected: crate::transfers::Step,
        next: crate::transfers::Step,
        proof: &crate::transfers::Evidence,
    ) -> Result<crate::transfers::Transfer> {
        use crate::transfers::*;
        self.transfer_write(|c| {
            let t = c.transfer(id)?;
            ensure!(t.epoch == epoch && t.step == expected, "stale transfer result");
            ensure!(expected.permits(t.kind,next), "invalid transfer transition");
            if next == Step::Exported && t.kind != Kind::Fork {
                ensure!(proof.source_fenced,"positive source fence required; timeout is not a fence");
                c.db.execute("UPDATE resources SET placement_managed=1 WHERE kind='machine' AND physical=?1",[&t.source])?;
            }
            if let Some(capture) = &proof.capture {
                ensure!(matches!(expected,Step::Preparing | Step::Exported),"capture cannot change after import begins");
                let owner = c.db.query_row("SELECT owner FROM resources WHERE kind='machine' AND physical=?1",[&t.source],|r|r.get(0))?;
                c.transfer_capture(capture,owner,&t.source)?;
                c.db.execute("UPDATE transfers SET capture=?1 WHERE id=?2",params![capture,id])?;
            }
            if matches!(next,Step::Importing | Step::Prepared) {
                let prepared = c.transfer(id)?;
                let capture = prepared.capture.as_deref().context("stored capture required")?;
                let owner = c.db.query_row("SELECT owner FROM resources WHERE kind='machine' AND physical=?1",[&t.source],|r|r.get(0))?;
                c.transfer_capture(capture,owner,&t.source)?;
            }
            if next == Step::Prepared {
                ensure!(t.kind == Kind::Park || proof.target_prepared,"verified unstarted import required");
            }
            if matches!(next,Step::Aborted | Step::Failed) {
                ensure!(matches!(expected,Step::Preparing | Step::Exported) || proof.target_excluded,"uncertain target retains capacity until positively excluded");
                c.db.execute("DELETE FROM transfer_reservations WHERE transfer=?1 AND epoch=?2",params![id,epoch])?;
                if t.kind == Kind::Fork {
                    // Retire the child's identity and name together, as ordinary deletion does.
                    c.db.execute("UPDATE operations SET resource='deleted:'||resource WHERE op<>'snapshot' AND (owner,resource) IN (SELECT owner,name FROM resources WHERE kind='machine' AND physical=?1)",[&t.machine])?;
                    c.db.execute("UPDATE publications SET active=0 WHERE physical=?1",[&t.machine])?;
                    c.db.execute("DELETE FROM grants WHERE physical=?1",[&t.machine])?;
                    c.db.execute("DELETE FROM owner_allocations WHERE physical=?1",[&t.machine])?;
                    // A child made for someone else leaves its name in the requester's history.
                    c.db.execute("UPDATE operations SET resource='deleted:'||resource WHERE id=?1 AND resource NOT LIKE 'deleted:%'",[t.operation])?;
                    ensure!(c.db.execute("DELETE FROM resources WHERE kind='machine' AND physical=?1 AND placement_epoch=0 AND node='' AND owner=(SELECT COALESCE(json_extract(request,'$.transfer.child.owner'),owner) FROM operations WHERE id=?2) AND EXISTS(SELECT 1 FROM transfer_locks WHERE machine=?1 AND transfer=?3)",params![t.machine,t.operation,id])? == 1,"fork child authority changed");
                }
            }
            let step = serde_json::to_value(next)?.as_str().unwrap().to_owned();
            c.db.execute("UPDATE transfers SET step=?1 WHERE id=?2 AND epoch=?3",params![step,id,epoch])?;
            if next == Step::Succeeded && t.kind == Kind::Fork {
                c.audit_fork(t.operation, &t.source, &t.machine)?;
            }
            if next.terminal() {
                c.db.execute("DELETE FROM transfer_locks WHERE transfer=?1",[id])?;
                // A moved or woken box with a role needs credentials on its new runner now.
                crate::aws_identity::notify();
                c.db.execute("UPDATE operations SET state=?1,finished_at=CURRENT_TIMESTAMP WHERE id=?2",params![if next == Step::Succeeded {"succeeded"}else{"failed"},t.operation])?;
            }
            c.transfer(id)
        })
    }
    #[allow(dead_code)]
    pub(crate) fn commit_transfer(
        &self,
        id: i64,
        epoch: i64,
    ) -> Result<crate::transfers::Transfer> {
        use crate::transfers::*;
        self.transfer_write(|c| {
            let t = c.transfer(id)?;
            ensure!(t.epoch == epoch && t.step == Step::Prepared,"stale transfer commit");
            ensure!(c.db.query_row("SELECT json_extract(response,'$.intervention') IS NULL FROM operations WHERE id=?1",[t.operation],|r|r.get::<_,bool>(0))?,"explicit recovery is pending");
            let unchanged:bool = c.db.query_row("SELECT EXISTS(SELECT 1 FROM resources r JOIN transfers t ON t.source=r.physical WHERE r.kind='machine' AND t.id=?1 AND r.node=t.source_node AND r.placement_epoch=t.source_epoch AND r.current_capture IS t.source_capture AND r.ssh_policy_generation=t.source_policy_generation)",[id],|r|r.get(0))?;
            ensure!(unchanged,"source authority or SSH policy changed");
            let (actor,address):(i64,String) = c.db.query_row("SELECT owner,json_extract(request,'$.transfer.machine') FROM operations WHERE id=?1",[t.operation],|r|Ok((r.get(0)?,r.get(1)?)))?;
            ensure!(c.machine(actor,&address,t.kind.permission())? == t.source,"source authorization changed");
            let capture = t.capture.as_deref().context("stored capture required")?;
            let owner = c.db.query_row("SELECT owner FROM resources WHERE kind='machine' AND physical=?1",[&t.source],|r|r.get(0))?;
            c.transfer_capture(capture,owner,&t.source)?;
            let (memory,cpus):(i64,i64) = if let Some(target) = &t.target {
                c.db.query_row("SELECT memory_mib,cpus FROM transfer_reservations WHERE transfer=?1 AND epoch=?2 AND node=?3",params![id,epoch,target],|r|Ok((r.get(0)?,r.get(1)?)))?
            } else { (0,0) };
            let changed = c.db.execute("UPDATE resources SET node=?1,placement_managed=1,placement_epoch=?2,current_capture=?3,reserved_mib=?4,reserved_cpus=?5,updated_at=CURRENT_TIMESTAMP WHERE kind='machine' AND physical=?6 AND placement_epoch=?7 AND node=?8",params![t.target.as_deref().unwrap_or(""),epoch,capture,memory,cpus,t.machine,if t.kind == Kind::Fork {0}else{t.source_epoch},if t.kind == Kind::Fork {""}else{&t.source_node}])?;
            ensure!(changed == 1,"target authority changed");
            c.db.execute("DELETE FROM transfer_reservations WHERE transfer=?1 AND epoch=?2",params![id,epoch])?;
            c.db.execute("UPDATE transfers SET step='committed' WHERE id=?1 AND epoch=?2 AND step='prepared'",params![id,epoch])?;
            c.transfer(id)
        })
    }

    pub(crate) fn retarget_transfer(
        &self,
        id: i64,
        epoch: i64,
        target: &str,
        input: &crate::transfers::Intent,
        action: &Value,
    ) -> Result<()> {
        self.transfer_write(|c| {
            let t=c.transfer(id)?;
            ensure!(t.epoch==epoch && t.step.before_commit(),"stale recovery result");
            let high:i64=c.db.query_row("SELECT CAST(value AS INTEGER) FROM settings WHERE key='transfer_epoch'",[],|r|r.get(0))?;
            let next=high.checked_add(1).context("epoch exhausted")?;
            c.db.execute("UPDATE settings SET value=?1 WHERE key='transfer_epoch'",[next.to_string()])?;
            c.db.execute("DELETE FROM transfer_reservations WHERE transfer=?1 AND epoch=?2",params![id,epoch])?;
            c.db.execute("UPDATE transfers SET target=?1,epoch=?2,step=?4 WHERE id=?3",params![target,next,id,if t.kind==crate::transfers::Kind::Fork && t.step==crate::transfers::Step::Preparing {"preparing"}else{"exported"}])?;
            c.reserve_transfer(&c.transfer(id)?,target,input)?;
            c.db.execute("UPDATE operations SET node=?1,response=json_remove(json_set(COALESCE(response,'{}'),'$.last_intervention',json(?2)),'$.intervention') WHERE id=?3",params![target,action.to_string(),t.operation])?;
            Ok(())
        })
    }

    // Legacy observations have no placement epoch and cannot mutate managed authority.
    fn legacy_machine(&self, physical: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT EXISTS(SELECT 1 FROM resources WHERE kind='machine' AND physical=?1 AND placement_epoch=0 AND placement_managed=0 AND NOT EXISTS(SELECT 1 FROM transfer_locks WHERE machine=?1))",[physical],|r|r.get(0))?)
    }
    fn legacy_access(&self, physical: &str) -> Result<()> {
        let (node,state):(String,Option<String>)=self.db.query_row("SELECT node,json_extract(metadata,'$.state') FROM resources WHERE kind='machine' AND physical=?1",[physical],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(
            !crate::drain::blocked(&self.db, &node)? || state.as_deref() == Some("running"),
            "runner draining; waiting for park and wake elsewhere"
        );
        ensure!(
            self.legacy_machine(physical)? || self.managed_placement(physical)?.is_some(),
            "managed placement requires transfer-aware routing"
        );
        Ok(())
    }
    pub(crate) fn managed_placement(&self, physical: &str) -> Result<Option<common::Placement>> {
        let placement: Option<(i64,String,i64)> = self.db.query_row("SELECT r.placement_epoch,r.node,t.id FROM resources r JOIN transfers t ON t.machine=r.physical AND t.epoch=r.placement_epoch AND t.target=r.node AND t.step='succeeded' AND t.kind IN ('restore','wake','move','fork') WHERE r.kind='machine' AND r.physical=?1 AND r.placement_managed=1 AND NOT EXISTS(SELECT 1 FROM transfer_locks WHERE machine=r.physical)",[physical],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        Ok(placement.map(|(epoch, node, id)| common::Placement {
            epoch: epoch as u64,
            node,
            transfer: format!("transfer-{id}"),
        }))
    }
    fn placement(&self, kind: &str, physical: &str) -> Result<String> {
        Ok(self.db.query_row(
            "SELECT node FROM resources WHERE kind=?1 AND physical=?2",
            params![kind, physical],
            |r| r.get(0),
        )?)
    }
    fn save_node(&mut self, node: &str, kind: &str, physical: &str, value: &Value) -> Result<()> {
        let mut updated = self.db.execute("UPDATE resources SET metadata=?1,updated_at=CURRENT_TIMESTAMP WHERE kind=?2 AND physical=?3 AND node=?4 AND (kind<>'machine' OR (placement_epoch=0 AND placement_managed=0 AND NOT EXISTS(SELECT 1 FROM transfer_locks WHERE machine=resources.physical)))",params![value.to_string(),kind,physical,node])?;
        if kind == "machine" && updated == 0 {
            if let Some(placement) = self.managed_placement(physical)? {
                ensure!(
                    node == placement.node
                        && value["id"] == physical
                        && value["placement"] == json!(placement),
                    "worker result has contradictory placement"
                );
                updated = self.db.execute("UPDATE resources SET metadata=?1,updated_at=CURRENT_TIMESTAMP WHERE kind='machine' AND physical=?2 AND node=?3 AND placement_epoch=?4",params![value.to_string(),physical,node,placement.epoch as i64])?;
            }
        }
        if kind == "machine" && updated == 1 {
            limits::observe(&self.db, physical, value)?;
        }
        Ok(())
    }
    pub(crate) fn operation_info(&self, user: i64, operation: i64) -> Result<Value> {
        let (mut info, response): (Value, Option<String>) = self.db.query_row("SELECT id,op,resource,state,node,retry_key,response FROM operations WHERE owner=?1 AND id=?2",params![user,operation],|r|Ok((json!({"id":r.get::<_,i64>(0)?,"op":r.get::<_,String>(1)?,"resource":r.get::<_,String>(2)?,"state":r.get::<_,String>(3)?,"node":r.get::<_,String>(4)?,"retry_key":r.get::<_,Option<String>>(5)?}),r.get(6)?)))?;
        if info["state"] == "joined" {
            let response: Value =
                serde_json::from_str(response.as_deref().context("missing wake join")?)?;
            let mut status = crate::transfers::wake_status(
                self,
                response["wake_transfer"]
                    .as_i64()
                    .context("missing joined transfer")?,
            )?;
            for field in ["id", "op", "resource", "retry_key"] {
                status[field] = info[field].clone();
            }
            return Ok(status);
        }
        if let Some(value) = response
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
        {
            for field in ["progress", "queued", "cleanup_pending"] {
                if let Some(v) = value.get(field) {
                    info[field] = v.clone();
                }
            }
        }
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
            if let Some((code, _)) = first_use_error(response["error"].as_str().unwrap_or("")) {
                info["error_code"] = json!(code);
            }
            // Written by the controller itself when the worker refused a fork for room.
            if let Some(room) = response["fork_room"].as_str() {
                info["error"] = json!(room);
                info["error_code"] = json!("fork_capacity");
            }
            // Refusals carry only controller-written public reasons.
            if response["refused"] == true {
                info["error"] = response["error"].clone();
                info["error_code"] = json!(response["code"].as_str().unwrap_or("refused"));
            }
        }
        if let Some((id, epoch, step)) = self
            .db
            .query_row(
                "SELECT id,epoch,step FROM transfers WHERE operation=?1",
                [operation],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
        {
            info["transfer"] = json!({"id":id,"epoch":epoch,"step":step});
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
                let managed = self.managed_placement(physical)?;
                if !self.legacy_machine(physical)?
                    && !managed
                        .as_ref()
                        .is_some_and(|p| p.node == node && m["placement"] == json!(p))
                {
                    all_machines.push(m.clone());
                    continue;
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
                ) && (managed.is_none() || m["vmm_running"] == false)
                {
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
            // An uncertain create or fork (an interrupted controller or lost reply; a pending one
            // is still being dispatched by a live process) whose machine the runner still lacks
            // after the timeout never completed: it fails visibly instead of staying pending.
            self.db.execute(&format!("UPDATE operations SET state='failed',response=?3,finished_at=CURRENT_TIMESTAMP WHERE node=?1 AND op IN ('create','fork') AND state='uncertain' AND created_at<datetime('now','-{CREATION_TIMEOUT_MINUTES} minutes') AND NOT EXISTS(SELECT 1 FROM transfers WHERE operation=operations.id) AND COALESCE(json_extract(request,'$.request.child'),json_extract(request,'$.request.id')) NOT IN (SELECT value FROM json_each(?2)) AND COALESCE(json_extract(request,'$.request.child'),json_extract(request,'$.request.id')) IN (SELECT physical FROM resources WHERE kind='machine' AND node=?1 AND placement_managed=0 AND placement_epoch=0 AND json_extract(metadata,'$.state')='pending' AND NOT EXISTS(SELECT 1 FROM transfer_locks WHERE machine=resources.physical))"),params![node,json!(observed).to_string(),json!({"error":CREATION_TIMEOUT}).to_string()])?;
            let mut stmt=self.db.prepare("SELECT physical FROM resources WHERE kind='machine' AND placement_epoch=0 AND placement_managed=0 AND NOT EXISTS(SELECT 1 FROM transfer_locks WHERE machine=resources.physical) AND node=?1 AND json_extract(metadata,'$.state')='pending' AND EXISTS(SELECT 1 FROM operations WHERE resource=resources.name AND owner=resources.owner AND state='failed') AND NOT EXISTS(SELECT 1 FROM operations WHERE resource=resources.name AND owner=resources.owner AND state IN ('pending','uncertain'))")?;
            let absent = stmt
                .query_map([&node], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            drop(stmt);
            for physical in absent {
                if !observed.contains(physical.as_str()) {
                    self.db.execute("UPDATE resources SET reserved_mib=0,reserved_cpus=0 WHERE kind='machine' AND physical=?1 AND node=?2",params![physical,node])?;
                    // A fork child (it names its source) ends failed; a create stays pending
                    // so a retry with a new key reuses its placement.
                    self.db.execute(&format!("UPDATE resources SET {UNMADE} WHERE kind='machine' AND physical=?1 AND json_extract(metadata,'$.state')='pending' AND json_extract(metadata,'$.source') IS NOT NULL"),[&physical])?;
                }
            }
            // Missing inventory releases owner demand only after all effects are resolved.
            let mut stmt = self
                .db
                .prepare("SELECT physical FROM resources WHERE kind='machine' AND placement_epoch=0 AND placement_managed=0 AND NOT EXISTS(SELECT 1 FROM transfer_locks WHERE machine=resources.physical) AND node=?1")?;
            let known = stmt
                .query_map([&node], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            for physical in known {
                if !observed.contains(physical.as_str()) {
                    limits::absent(&self.db, &physical)?;
                }
            }
            // A machine this runner no longer lists is gone from it (deleted there, or the runner
            // lost its data): flag it so its owner can delete it here. Never-observed creates
            // stay pending; a machine listed again gets its metadata back on the next pass.
            self.db.execute("UPDATE resources SET metadata=json_set(metadata,'$.state','missing'),reserved_mib=0,reserved_cpus=0,updated_at=CURRENT_TIMESTAMP WHERE kind='machine' AND placement_epoch=0 AND placement_managed=0 AND NOT EXISTS(SELECT 1 FROM transfer_locks WHERE machine=resources.physical) AND node=?1 AND json_extract(metadata,'$.state') NOT IN ('pending','missing') AND json_extract(metadata,'$.unmade') IS NULL AND physical NOT IN (SELECT value FROM json_each(?2))",params![node,json!(observed).to_string()])?;
            self.db.execute("INSERT INTO node_usage(node,extra_mib,extra_cpus,extra_slots) VALUES(?1,?2,?3,?4) ON CONFLICT(node) DO UPDATE SET extra_mib=?2,extra_cpus=?3,extra_slots=?4",params![node,extra_mib,extra_cpus,extra_slots])?;
            for snapshot in snapshots.as_array().context("invalid snapshots")? {
                let physical = snapshot["name"].as_str().context("snapshot ID")?;
                let parent = snapshot["workspace"].as_str().context("snapshot parent")?;
                let owner:Option<i64>=self.db.query_row("SELECT owner FROM resources WHERE kind='machine' AND physical=?1 AND node=?2 AND placement_managed=0 AND NOT EXISTS(SELECT 1 FROM transfer_locks WHERE machine=resources.physical)",params![parent,node],|r|r.get(0)).optional()?;
                if let Some(owner) = owner {
                    let name=if self.db.query_row("SELECT EXISTS(SELECT 1 FROM resources WHERE owner=?1 AND kind='snapshot' AND name=?2 AND physical<>?2)",params![owner,physical],|r|r.get::<_,bool>(0))?{format!("auto-{}",&common::nonce()?[..24])}else{physical.to_owned()};
                    self.db.execute("INSERT OR IGNORE INTO resources(owner,kind,name,physical,metadata,node) VALUES(?1,'snapshot',?2,?3,?4,?5)",params![owner,name,physical,snapshot.to_string(),node])?;
                    // The worker's summary has no disk size; keep the one recorded at capture.
                    let mut snapshot = snapshot.clone();
                    if snapshot["disk_bytes"].is_null() {
                        snapshot["disk_bytes"] = self.db.query_row("SELECT json_extract(metadata,'$.disk_bytes') FROM resources WHERE kind='snapshot' AND physical=?1",[physical],|r|r.get::<_,Option<i64>>(0))?.map_or(Value::Null, |n| json!(n));
                    }
                    self.save_node(&node, "snapshot", physical, &snapshot)?;
                    all_snapshots.push(snapshot.clone());
                }
            }
            self.db.execute("UPDATE resources SET metadata=json_set(metadata,'$.state','failed'),updated_at=CURRENT_TIMESTAMP WHERE kind='snapshot' AND node=?1 AND json_extract(metadata,'$.state')='pending' AND (SELECT state FROM operations WHERE owner=resources.owner AND node=resources.node AND op='snapshot' AND json_extract(request,'$.request.name')=resources.physical ORDER BY id DESC LIMIT 1)='failed'",[&node])?;
        }
        Ok((json!(all_machines), json!(all_snapshots)))
    }
    pub fn state(&mut self, user: i64) -> Result<Value> {
        self.reconcile()?;
        let mut stmt=self.db.prepare("SELECT kind,physical,metadata,node,reserved_mib,CAST(strftime('%s',created_at) AS INTEGER) FROM resources WHERE owner=?1 ORDER BY created_at,name")?;
        let rows = stmt.query_map([user], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, u32>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut machines = Vec::new();
        let mut snapshots = Vec::new();
        let mut reserved = 0;
        for row in rows {
            let (kind, physical, mut metadata, node, memory, created_at) = row?;
            if kind == "snapshot" {
                // An unfinished capture names no machine yet: its snapshot operation does.
                let attempt: Option<(String, Option<i64>)> = self.db.query_row("SELECT json_extract(request,'$.request.id'),CAST(strftime('%s',finished_at) AS INTEGER) FROM operations WHERE owner=?1 AND op='snapshot' AND json_extract(request,'$.request.name')=?2 ORDER BY id DESC LIMIT 1",params![user,physical],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                let mut value: Value = serde_json::from_str(&metadata)?;
                if let (Some((machine, finished)), true) = (attempt, value["workspace"].is_null()) {
                    value["workspace"] = json!(machine);
                    if value["state"] == "failed" {
                        value["failed_at"] = json!(finished);
                    }
                    metadata = value.to_string();
                }
            }
            let mut value = self.view(user, &kind, &physical, &metadata, &node)?;
            if kind == "machine" {
                value["created_at"] = json!(created_at);
                reserved += memory;
                value["shared_with"] = shared_with(&self.db, &physical)?;
                value["published_ports"] = self.publications(&physical)?;
                value["aws_role"] = crate::aws_identity::view(&self.db, &physical)?;
                machines.push(value);
            } else {
                snapshots.push(value);
            }
        }
        let mut shared = Vec::new();
        let mut stmt = self.db.prepare("SELECT r.physical,r.metadata,r.node,u.email,g.role,CAST(strftime('%s',r.created_at) AS INTEGER) FROM grants g JOIN resources r ON r.kind='machine' AND r.physical=g.physical JOIN users u ON u.id=r.owner WHERE g.grantee=?1 ORDER BY u.email,r.name")?;
        let rows = stmt.query_map([user], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        for row in rows {
            let (physical, metadata, node, owner, role, created_at) = row?;
            let mut value = self.view(user, "machine", &physical, &metadata, &node)?;
            value["created_at"] = json!(created_at);
            value["published_ports"] = self.publications(&physical)?;
            value["owner"] = json!(owner);
            value["role"] = json!(role);
            value["aws_role"] = crate::aws_identity::view(&self.db, &physical)?;
            // An admin attaches from the owner's allow-list, which is what attach checks.
            if (role == "admin" || self.pool_admin(user)?)
                && let Ok(Some(config)) = crate::aws_identity::config(&self.root)
            {
                value["aws_roles"] = json!(config.roles_for(&owner));
            }
            shared.push(value);
        }
        // Pool admins see other boxes for transfer, fork and AWS settings; not guest data.
        let mut others = Vec::new();
        if self.pool_admin(user)? {
            let mut stmt = self.db.prepare("SELECT r.metadata,r.node,u.email,r.name,CAST(strftime('%s',r.created_at) AS INTEGER),r.physical FROM resources r JOIN users u ON u.id=r.owner WHERE r.kind='machine' AND r.owner<>?1 AND NOT EXISTS(SELECT 1 FROM grants g WHERE g.physical=r.physical AND g.grantee=?1) ORDER BY u.email,r.name")?;
            let rows = stmt.query_map([user], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?;
            for row in rows {
                let (metadata, node, owner, name, created_at, physical) = row?;
                let metadata: Value = serde_json::from_str(&metadata)?;
                let mut value = json!({"id":format!("{owner}/{name}"),"owner":owner,"role":"pool-admin","node":node,"created_at":created_at});
                for key in ["state", "image", "memory_mib", "vcpu_count", "disk_bytes"] {
                    value[key] = metadata[key].clone();
                }
                value["aws_role"] = crate::aws_identity::view(&self.db, &physical)?;
                if let Ok(Some(config)) = crate::aws_identity::config(&self.root) {
                    value["aws_roles"] = json!(config.roles_for(&owner));
                }
                others.push(value);
            }
        }
        let mut running = BTreeMap::new();
        let mut pss = 0;
        let mut limit = 0;
        let mut pool = Vec::new();
        if self.root.join("control.sock").exists() {
            pool.push(("local".to_owned(), 4096));
        }
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
        let discovery = self.discovery(user, &nodes)?;
        let aws_identity = match crate::aws_identity::config(&self.root) {
            Ok(Some(config)) => {
                let email: String =
                    self.db
                        .query_row("SELECT email FROM users WHERE id=?1", [user], |r| r.get(0))?;
                json!({"enabled":true,"mode":config.mode.name(),"issuer":config.issuer,"roles":config.roles_for(&email)})
            }
            _ => json!({"enabled":false}),
        };
        Ok(
            json!({"discovery":discovery,"aws_identity":aws_identity,"owner_limits":limits::state(&self.root,&self.db,user)?,"pool_admin":self.pool_admin(user)?,"audit":self.audit_log(user)?,"operations":operations,"workspaces":machines,"shared":shared,"pool":others,"snapshots":snapshots,"nodes":nodes,"stats":{"running":running,"total_pss_kib":pss,"reserved_memory_mib":reserved,"limit_memory_mib":limit,"note":"Offline usage is stale; reservations remain fixed"}}),
        )
    }
    fn discovery(&self, user: i64, inventory: &Value) -> Result<Value> {
        let email: String =
            self.db
                .query_row("SELECT email FROM users WHERE id=?1", [user], |r| r.get(0))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let mut nodes = Vec::new();
        for n in inventory.as_array().unwrap() {
            if n["revoked"] == true {
                continue;
            }
            let id = n["id"].as_str().context("node id")?;
            let (memory, cpus, slots): (u64,u64,u64) = self.db.query_row(
                "SELECT COALESCE(SUM(reserved_mib),0),COALESCE(SUM(reserved_cpus),0),COALESCE(SUM(CASE WHEN reserved_mib>0 THEN 1 ELSE 0 END),0) FROM resources WHERE kind='machine' AND node=?1",
                [id], |r| Ok((r.get::<_,u32>(0)? as u64,r.get::<_,u32>(1)? as u64,r.get::<_,u32>(2)? as u64)))?;
            let (extra_memory, extra_cpus, extra_slots) = self.extra_usage(id)?;
            nodes.push(crate::discovery::node(n, json!({"memory_mib":memory+extra_memory,"vcpus":cpus+extra_cpus,"slots":slots+extra_slots}), now));
        }
        let images = crate::discovery::images(&nodes);
        Ok(
            json!({"schema":1,"server_version":env!("CARGO_PKG_VERSION"),"identity":{"email":email},
            "placement":"automatic","nodes":nodes,"images":images,
            "capacity_note":"Observed admission headroom, not a reservation. Offline or unreported capacity is null; create rechecks all limits. Omit --node for automatic placement."}),
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
        if kind == "snapshot" {
            // A stored snapshot restores or forks from the artifact store on any runner.
            value["artifact"] = json!(self.db.query_row("SELECT a.id FROM artifacts a JOIN resources s ON s.kind='snapshot' AND s.physical=?1 AND a.owner=s.owner WHERE a.kind='snapshot' AND a.local_name=?1 AND a.state='stored' AND a.digest IS NOT NULL ORDER BY a.stored_at DESC LIMIT 1",[physical],|r|r.get::<_,String>(0)).optional()?);
        }
        let online = if node.is_empty() && kind == "snapshot" {
            true
        } else if node == "local" {
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
        tx.execute("DELETE FROM aws_roles WHERE physical=?1", [physical])?;
        tx.execute(
            "DELETE FROM owner_allocations WHERE physical=?1",
            [physical],
        )?;
        tx.execute(&format!("DELETE FROM resources WHERE {OF}"), [physical])?;
        tx.execute(
            "DELETE FROM resources WHERE kind='machine' AND physical=?1",
            [physical],
        )?;
        tx.commit()?;
        crate::aws_identity::revoke(&self.root, physical)?;
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
    /// Whether the machine was never made: its create or fork is pending or failed.
    fn unmade(&self, physical: &str) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT json_extract(metadata,'$.state')='pending' OR json_extract(metadata,'$.unmade') IS NOT NULL FROM resources WHERE kind='machine' AND physical=?1",
            [physical],
            |r| r.get::<_, Option<bool>>(0),
        )? == Some(true))
    }
    /// Whether reconcile found the machine gone from its runner.
    fn missing(&self, physical: &str) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT json_extract(metadata,'$.state')='missing' FROM resources WHERE kind='machine' AND physical=?1",
            [physical],
            |r| r.get::<_, Option<bool>>(0),
        )? == Some(true))
    }
    /// The saved result of a delete (`op`) retried with its key. A key held by another operation
    /// or name conflicts; one whose machine went without a recorded result is uncertain.
    fn delete_replay(
        &self,
        user: i64,
        delete: &str,
        name: &str,
        key: Option<&str>,
    ) -> Result<Option<Value>> {
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
            op == delete && (resource == name || forgotten),
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
    /// Change only a machine's logical name: its physical ID, disk, snapshots, placement,
    /// grants, previews and running state stay. No worker contact.
    fn rename(&mut self, user: i64, request: &Value) -> Result<Value> {
        let address = request["id"].as_str().context("id")?;
        let new = request["name"].as_str().context("new name required")?;
        common::address(address)?;
        common::identifier(new)?;
        let fingerprint = json!({"request":{"op":"rename","id":address,"name":new}}).to_string();
        let key = request["operation_key"].as_str();
        if let Some(key) = key {
            common::identifier(key)?;
        }
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        let renamed = (|| -> Result<Value> {
            // The old name is gone after the first success: answer a retry by its key alone.
            if let Some(key) = key {
                let previous: Option<(String, String, Option<String>)> = self.db.query_row("SELECT op,request,response FROM operations WHERE owner=?1 AND retry_key=?2",params![user,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
                if let Some((op, previous, response)) = previous {
                    ensure!(
                        op == "rename" && previous == fingerprint,
                        "operation key conflicts with original parameters"
                    );
                    return Ok(serde_json::from_str(&response.context("missing result")?)?);
                }
            }
            let physical = self.machine(user, address, "rename")?;
            let (old, email): (String, String) = self.db.query_row("SELECT r.name,u.email FROM resources r JOIN users u ON u.id=r.owner WHERE r.kind='machine' AND r.physical=?1",[&physical],|r|Ok((r.get(0)?,r.get(1)?)))?;
            if old == new {
                return Ok(json!({"id":new,"renamed_from":old}));
            }
            if self.physical(user, "machine", new).is_ok() {
                return Err(limits::Rejection(format!(
                    "you already have a machine named '{new}'; choose another name"
                ))
                .into());
            }
            // A transfer resolves its machine by name between steps.
            let moving: bool = self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM transfer_locks WHERE machine=?1)",
                [&physical],
                |r| r.get(0),
            )?;
            if moving {
                return Err(limits::Rejection(format!(
                    "machine '{old}' is being moved, parked or woken; retry the rename when it finishes"
                ))
                .into());
            }
            self.db.execute("UPDATE resources SET name=?1,updated_at=CURRENT_TIMESTAMP WHERE kind='machine' AND physical=?2",params![new,physical])?;
            // Machine history follows the name; snapshot operations are keyed by snapshot name.
            self.db.execute("UPDATE operations SET resource=?1 WHERE owner=?2 AND resource=?3 AND op NOT IN ('snapshot','snapshot-delete')",params![new,user,old])?;
            self.db.execute("UPDATE operations SET resource=?1||'/'||?2 WHERE resource=?1||'/'||?3 AND op NOT IN ('snapshot','snapshot-delete')",params![email,new,old])?;
            let node = self.placement("machine", &physical)?;
            self.db.execute("INSERT INTO operations(owner,op,resource,state,node,retry_key,request,finished_at) VALUES(?1,'rename',?2,'succeeded',?3,?4,?5,CURRENT_TIMESTAMP)",params![user,new,node,key,fingerprint])?;
            let operation = self.db.last_insert_rowid();
            let value = json!({"id":new,"renamed_from":old,"operation_id":operation});
            self.db.execute(
                "UPDATE operations SET response=?1 WHERE id=?2",
                params![value.to_string(), operation],
            )?;
            Ok(value)
        })();
        match renamed {
            Ok(value) => {
                self.db.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                self.db.execute_batch("ROLLBACK")?;
                Err(error)
            }
        }
    }
    /// Give a machine to another owner. Only its catalog owner changes, with the snapshots its
    /// owner took of it: physical ID, disk, placement, previews, SSH keys and running state
    /// stay, with no worker contact. Grants go (the old owner keeps an admin grant only with
    /// `keep_access`) and the open streams of everyone who lost access close. The owner or a
    /// pool admin may do it; the new owner's name space and limits must have room.
    fn hand_over(&mut self, user: i64, request: &Value) -> Result<Value> {
        let address = request["id"].as_str().context("id")?;
        common::address(address)?;
        let to = request["to"]
            .as_str()
            .context("new owner email required")?
            .to_ascii_lowercase();
        common::email(&to)?;
        let keep = request["keep_access"].as_bool().unwrap_or(false);
        let fingerprint =
            json!({"request":{"op":"transfer","id":address,"to":to,"keep_access":keep}})
                .to_string();
        let key = request["operation_key"].as_str();
        if let Some(key) = key {
            common::identifier(key)?;
        }
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        let moved = (|| -> Result<(Value, Vec<String>, Vec<i64>)> {
            // The address may not resolve after the first success: answer a retry by its key.
            if let Some(key) = key {
                let previous: Option<(String, String, Option<String>)> = self.db.query_row("SELECT op,request,response FROM operations WHERE owner=?1 AND retry_key=?2",params![user,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
                if let Some((op, previous, response)) = previous {
                    ensure!(
                        op == "transfer" && previous == fingerprint,
                        "operation key conflicts with original parameters"
                    );
                    let value = serde_json::from_str(&response.context("missing result")?)?;
                    return Ok((value, Vec::new(), Vec::new()));
                }
            }
            let (physical, _) = self.access(user, address, "transfer")?;
            let (old, name): (i64, String) = self.db.query_row(
                "SELECT owner,name FROM resources WHERE kind='machine' AND physical=?1",
                [&physical],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let new = self.user_by_email(user, &to)?;
            if new == old {
                return Err(Refusal {
                    code: "already_owner",
                    message: format!("{to} already owns this machine."),
                }
                .into());
            }
            let refuse = |why: String| -> Result<(Value, Vec<String>, Vec<i64>)> {
                Err(limits::Rejection(why).into())
            };
            if self.unmade(&physical)? {
                return refuse(format!(
                    "machine '{name}' was never made or is still being created; transfer it once it exists"
                ));
            }
            // A move, park or wake holds a transfer lock and re-authorizes by owner and name. An
            // unresolved effect (its fingerprint names the physical ID) may still land under the
            // old owner, such as a capture's snapshot.
            let busy: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM transfer_locks WHERE machine=?1) OR EXISTS(SELECT 1 FROM operations WHERE state IN ('pending','uncertain') AND op IN ('create','fork','resize','restore','start','snapshot','snapshot-delete','delete') AND (json_extract(request,'$.request.id')=?1 OR json_extract(request,'$.request.child')=?1))",[&physical],|r|r.get(0))?;
            if busy {
                return refuse(format!(
                    "machine '{name}' has an unfinished operation (a move, park, wake, create, fork, snapshot, start, resize, restore or delete); transfer it when that finishes"
                ));
            }
            if self.physical(new, "machine", &name).is_ok() {
                return refuse(format!(
                    "{to} already has a machine named '{name}'; rename one of them first"
                ));
            }
            const SNAPSHOTS: &str =
                "owner=?1 AND kind='snapshot' AND json_extract(metadata,'$.workspace')=?2";
            let clash: Option<String> = self.db.query_row(&format!("SELECT name FROM resources WHERE {SNAPSHOTS} AND name IN (SELECT name FROM resources WHERE owner=?3 AND kind='snapshot') LIMIT 1"),params![old,physical,new],|r|r.get(0)).optional()?;
            if let Some(clash) = clash {
                return refuse(format!(
                    "{to} already has a snapshot named '{clash}', like one of this machine's; delete or keep one of them first"
                ));
            }
            limits::admit_owner(&self.root, &self.db, &physical, new, &to)?;
            let snapshots = self
                .db
                .prepare(&format!(
                    "SELECT name FROM resources WHERE {SNAPSHOTS} ORDER BY name"
                ))?
                .query_map(params![old, physical], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            // The old owner's history stays but stops claiming the names, which are free again.
            self.db.execute("UPDATE operations SET resource='transferred:'||resource WHERE owner=?1 AND resource=?2 AND op<>'snapshot'",params![old,name])?;
            self.db.execute(&format!("UPDATE operations SET resource='transferred:'||resource WHERE owner=?1 AND op='snapshot' AND resource IN (SELECT name FROM resources WHERE {SNAPSHOTS})"),params![old,physical])?;
            // Stored captures and snapshot artifacts carry their owner: store GC roots and
            // managed restores match it, so they move with the rows that name them.
            self.db.execute(&format!("UPDATE artifacts SET owner=?3 WHERE owner=?1 AND (machine=?2 OR (kind='snapshot' AND local_name IN (SELECT physical FROM resources WHERE {SNAPSHOTS})))"),params![old,physical,new])?;
            self.db.execute(
                &format!(
                    "UPDATE resources SET owner=?3,updated_at=CURRENT_TIMESTAMP WHERE {SNAPSHOTS}"
                ),
                params![old, physical, new],
            )?;
            if self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM aws_roles WHERE physical=?1)",
                [&physical],
                |r| r.get::<_, bool>(0),
            )? {
                // The old owner's role leaves the runner before the new owner is committed.
                self.withdraw_aws(&physical, None)?;
                crate::aws_identity::revoke(&self.root, &physical)?;
                self.db
                    .execute("DELETE FROM aws_roles WHERE physical=?1", [&physical])?;
            }
            self.db.execute("UPDATE resources SET owner=?1,updated_at=CURRENT_TIMESTAMP WHERE kind='machine' AND physical=?2",params![new,physical])?;
            let mut cut = self
                .db
                .prepare("SELECT grantee FROM grants WHERE physical=?1 AND grantee<>?2")?
                .query_map(params![physical, new], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            self.db
                .execute("DELETE FROM grants WHERE physical=?1", [&physical])?;
            if keep {
                self.db.execute(
                    "INSERT INTO grants(physical,grantee,role) VALUES(?1,?2,'admin')",
                    params![physical, old],
                )?;
            } else {
                cut.push(old);
            }
            // Preview streams are keyed by port, not machine.
            let previews = self
                .db
                .prepare("SELECT https_port FROM publications WHERE physical=?1 AND active=1")?
                .query_map([&physical], |r| r.get::<_, u16>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let streams = std::iter::once(physical.clone())
                .chain(previews.iter().map(|https| format!("preview:{https}")))
                .collect::<Vec<_>>();
            let node = self.placement("machine", &physical)?;
            self.db.execute("INSERT INTO operations(owner,op,resource,state,node,retry_key,request,finished_at) VALUES(?1,'transfer',?2,'succeeded',?3,?4,?5,CURRENT_TIMESTAMP)",params![user,format!("transferred:{name}"),node,key,fingerprint])?;
            let operation = self.db.last_insert_rowid();
            self.audit(
                operation,
                user,
                "transfer",
                &name,
                Some(&name),
                Some(old),
                new,
                keep,
            )?;
            let id = if new == user {
                name.clone()
            } else {
                format!("{to}/{name}")
            };
            let value = json!({"id":id,"owner":to,"transferred_from":self.email(old)?,"keep_access":keep,"snapshots":snapshots,"operation_id":operation});
            self.db.execute(
                "UPDATE operations SET response=?1 WHERE id=?2",
                params![value.to_string(), operation],
            )?;
            Ok((value, streams, cut))
        })();
        match moved {
            Ok((value, streams, losers)) => {
                self.db.execute_batch("COMMIT")?;
                for loser in losers {
                    for stream in &streams {
                        cut(loser, stream);
                    }
                }
                Ok(value)
            }
            Err(error) => {
                self.db.execute_batch("ROLLBACK")?;
                Err(error)
            }
        }
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
        self.without_runner(user, "delete", name, physical, input, |c| {
            Ok(json!({"deleted":name,"snapshots":c.forget(user, physical)?}))
        })
    }
    fn parked(&self, physical: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT node='' AND placement_managed=1 AND NOT EXISTS(SELECT 1 FROM transfer_locks WHERE machine=?1) FROM resources WHERE kind='machine' AND physical=?1",[physical],|r|r.get(0))?)
    }
    /// A named snapshot that is not on its box's runner: the box moved or was parked. One in
    /// the artifact store goes from the catalog alone; a runner still holding a copy has it
    /// removed by the publication janitor, which owns that runner's publication record. One
    /// not yet stored is removed from its runner first, under that copy's own placement.
    fn delete_stored_snapshot(
        &mut self,
        user: i64,
        name: &str,
        physical: &str,
        input: &Value,
    ) -> Result<Option<Value>> {
        let snapshot = input["name"].as_str().context("snapshot name")?;
        let id = self.physical(user, "snapshot", snapshot)?;
        self.snapshot_source(user, &id, physical)?;
        let (node, here, stored): (String, bool, bool) = self.db.query_row("SELECT s.node,s.node=m.node AND s.node<>'',EXISTS(SELECT 1 FROM artifacts a WHERE a.kind='snapshot' AND a.owner=s.owner AND a.local_name=s.physical AND a.state='stored') FROM resources s JOIN resources m ON m.kind='machine' AND m.physical=?2 WHERE s.kind='snapshot' AND s.physical=?1",params![id,physical],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        if here {
            return Ok(None);
        }
        let moving: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM transfer_locks WHERE machine=?1)",
            [physical],
            |r| r.get(0),
        )?;
        ensure!(
            !moving,
            limits::Rejection(
                "snapshot is in use by a move or fork; retry when it finishes".into()
            )
        );
        if !node.is_empty() && !stored {
            self.remove_snapshot_copy(&node, physical, &id)?;
        }
        let value = self.without_runner(user, "snapshot-delete", snapshot, physical, input, |c| {
            let tx = c.db.transaction()?;
            tx.execute("UPDATE operations SET resource='deleted:'||resource WHERE op='snapshot' AND owner=?1 AND resource=?2",params![user,snapshot])?;
            tx.execute(
                "DELETE FROM resources WHERE kind='snapshot' AND physical=?1",
                [&id],
            )?;
            tx.commit()?;
            Ok(json!({"deleted":snapshot,"workspace":name}))
        })?;
        Ok(Some(value))
    }
    /// A runner other than the box's own that may still hold a copy of the box or its
    /// snapshots: its route and what it knows of the box (null when nothing). None for a
    /// retired or unknown runner, which runs nothing; an offline one refuses, to retry.
    fn old_copy(&self, node: &str, physical: &str) -> Result<Option<(PathBuf, Value)>> {
        let (known, retired): (bool, bool) = self.db.query_row("SELECT EXISTS(SELECT 1 FROM nodes WHERE id=?1 AND revoked=0),EXISTS(SELECT 1 FROM runner_drains WHERE node=?1 AND state IN ('retained','removed'))",[node],|r|Ok((r.get(0)?,r.get(1)?)))?;
        if !known || retired {
            return Ok(None);
        }
        ensure!(
            nodes::online(&self.root, node).is_ok(),
            limits::Rejection(format!(
                "Runner {node} still holds a copy of this box or its snapshots and is offline; retry when it is back, or revoke that runner if it is gone."
            ))
        );
        nodes::managed_lifecycle_capable(&self.root, node)?;
        let route = nodes::route(&self.root, node)?;
        match wire::request(&route, json!({"op":"inspect","id":physical})) {
            Ok(seen) => Ok(Some((route, seen))),
            Err(error) if format!("{error:#}").contains("workspace not found") => {
                Ok(Some((route, Value::Null)))
            }
            Err(error) => Err(error),
        }
    }
    fn remove_snapshot_copy(&self, node: &str, physical: &str, snapshot: &str) -> Result<()> {
        let Some((route, seen)) = self.old_copy(node, physical)? else {
            return Ok(());
        };
        if seen.is_null() {
            return Ok(()); // That runner no longer knows the box; it went with its snapshots.
        }
        let mut request = json!({"op":"snapshot-delete","id":physical,"name":snapshot});
        if !seen["placement"].is_null() {
            request["placement"] = seen["placement"].clone();
        }
        let result = wire::request(&route, request)?;
        ensure!(
            result["deleted"] == snapshot && result["workspace"] == physical,
            "snapshot removal on runner {node} unconfirmed"
        );
        Ok(())
    }
    /// A managed box leaves fenced copies on the runners it was moved or parked from, and
    /// may leave snapshots there not yet in the store. Its delete removes them, each under
    /// that copy's own placement, so nothing of the box is left on a runner.
    fn remove_old_copies(&self, physical: &str, current: &str) -> Result<()> {
        let nodes: Vec<String> = self.db.prepare("SELECT source_node FROM transfers WHERE source=?1 AND source_node NOT IN ('',?2) UNION SELECT node FROM resources WHERE kind='snapshot' AND json_extract(metadata,'$.workspace')=?1 AND node NOT IN ('',?2)")?.query_map(params![physical,current],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for node in nodes {
            let Some((route, seen)) = self.old_copy(&node, physical)? else {
                continue;
            };
            if seen.is_null() {
                continue;
            }
            ensure!(
                !seen["placement"].is_null(),
                "runner {node} holds an unmanaged copy of this box; reconcile it first"
            );
            match seen["lifecycle"]["phase"].as_str() {
                Some(phase @ ("exported" | "cleaned")) => {
                    if phase == "exported" {
                        // Every transfer of the box is finished: its source copy is no fallback.
                        let intent = &seen["lifecycle"]["intent"];
                        wire::request(
                            &route,
                            json!({"op":"export-cleanup","id":physical,"placement":seen["placement"],"transfer":intent["transfer"],"capture":intent["capture"]}),
                        )?;
                    }
                    let result = wire::request(
                        &route,
                        json!({"op":"delete","id":physical,"placement":seen["placement"]}),
                    )?;
                    ensure!(
                        result["deleted"] == physical,
                        "old copy removal on runner {node} unconfirmed"
                    );
                }
                // An excluded import attempt has no disk; only snapshots taken there remain.
                Some("discarded") => {
                    let snapshots: Vec<String> = self.db.prepare("SELECT physical FROM resources WHERE kind='snapshot' AND node=?1 AND json_extract(metadata,'$.workspace')=?2")?.query_map(params![node,physical],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
                    for snapshot in snapshots {
                        self.remove_snapshot_copy(&node, physical, &snapshot)?;
                    }
                }
                _ => {
                    return Err(limits::Rejection(format!(
                        "Runner {node} holds an unfinished copy of this box; check ow status before deleting it."
                    ))
                    .into());
                }
            }
        }
        Ok(())
    }
    /// Record an operation that needs no runner under its key, with the fingerprint a
    /// dispatched one would have, then apply its catalog effect.
    fn without_runner(
        &mut self,
        user: i64,
        op: &str,
        resource: &str,
        physical: &str,
        input: &Value,
        effect: impl FnOnce(&mut Self) -> Result<Value>,
    ) -> Result<Value> {
        ensure!(
            !limits::unresolved(&self.db, physical)?,
            "machine has unresolved operations; reconcile their outcomes first"
        );
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
                self.db.execute("INSERT INTO operations(owner,op,resource,state,node,retry_key,request) VALUES(?1,?2,?3,'pending',?4,?5,?6)",params![user,op,resource,node,key,fingerprint])?;
                self.db.last_insert_rowid()
            }
        };
        let mut value = effect(self)?;
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
        let extra = self
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
            .unwrap_or((0, 0, 0));
        let pending: (u64,u64,u64) = self.db.query_row("SELECT COALESCE(SUM(memory_mib),0),COALESCE(SUM(cpus),0),COUNT(*) FROM transfer_reservations WHERE node=?1",[node],|r|Ok((r.get::<_,i64>(0)? as u64,r.get::<_,i64>(1)? as u64,r.get::<_,i64>(2)? as u64)))?;
        Ok((
            extra.0 + pending.0,
            extra.1 + pending.1,
            extra.2 + pending.2,
        ))
    }
    fn choose(
        &self,
        requested: Option<&str>,
        memory: u64,
        image: &str,
        cpus: u64,
        local_status: &Value,
        disk: Option<u32>,
    ) -> Result<String> {
        let inventory = nodes::inventory(&self.root)?;
        if inventory.as_array().unwrap().is_empty()
            && requested.is_none()
            && self.root.join("control.sock").exists()
        {
            common::guest_shape(local_status, memory, cpus)?;
            return Ok("local".into());
        }
        let mut candidates = Vec::new();
        for n in inventory.as_array().unwrap() {
            let id = n["id"].as_str().unwrap();
            if requested.is_some_and(|r| r != id)
                || (disk.is_some() && n["capabilities"]["disk_resize_v1"] != true)
                || common::guest_shape(&n["capabilities"], memory, cpus).is_err()
                || n["online"] != true
                || n["capabilities"]["drain"] == true
                || common::node_operation(&n["capabilities"], "create").is_err()
                || !(n["capabilities"]["images"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|i| i == image))
                    || (n["capabilities"]["image_store_v1"] == true
                        && crate::image_catalog::resolve(&self.db, image).is_ok()))
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
                let cap = n["capabilities"].clone();
                let transfers: i64 = self.db.query_row(
                    "SELECT COALESCE(SUM(disk_bytes),0) FROM transfer_reservations WHERE node=?1",
                    [id],
                    |r| r.get(0),
                )?;
                let logical: i64 = self.db.query_row("SELECT COALESCE(SUM(a.disk_gib),0) FROM owner_allocations a JOIN resources r ON r.kind='machine' AND r.physical=a.physical WHERE r.node=?1",[id],|r|r.get(0))?;
                let disk_extra = (transfers as u64).saturating_add(
                    (logical as u64 * common::GIB)
                        .saturating_sub(cap["reserved_disk_bytes"].as_u64().unwrap_or(0)),
                );
                candidates.push(crate::placement::Candidate {
                    id: id.to_owned(),
                    online: true,
                    active: cap["controller_drain"] != true && cap["active"] != false,
                    pool: cap["pool"].as_str().unwrap_or("default").into(),
                    free: crate::placement::Capacity {
                        memory_mib: n["memory_mib"]
                            .as_u64()
                            .unwrap()
                            .saturating_sub(used as u64 + extra_mib),
                        cpus: cap["vcpus"]
                            .as_u64()
                            .unwrap_or(0)
                            .saturating_sub(used_cpus as u64 + extra_cpus),
                        slots: n["slots"]
                            .as_u64()
                            .unwrap()
                            .saturating_sub(count as u64 + extra_slots),
                        disk_bytes: cap["available_disk_bytes"]
                            .as_u64()
                            .unwrap_or(0)
                            .saturating_sub(disk_extra),
                    },
                    joined: n["enrollment_order"].as_u64().unwrap_or(0),
                    capabilities: cap,
                });
            }
        }

        if candidates.is_empty() {
            let online: Vec<_> = inventory
                .as_array()
                .unwrap()
                .iter()
                .filter(|n| {
                    requested.is_none_or(|r| n["id"] == r)
                        && n["online"] == true
                        && n["capabilities"]["drain"] != true
                })
                .collect();
            ensure!(!online.is_empty(), "no online node available");
            let prepared: Vec<_> = online
                .into_iter()
                .filter(|n| {
                    n["capabilities"]["images"]
                        .as_array()
                        .is_some_and(|a| a.contains(&json!(image)))
                })
                .collect();
            ensure!(!prepared.is_empty(), "node lacks prepared image");
            ensure!(
                prepared.iter().any(|n| (disk.is_none()
                    || n["capabilities"]["disk_resize_v1"] == true)
                    && common::node_operation(&n["capabilities"], "create").is_ok()),
                "node capability does not support create"
            );
            anyhow::bail!("node capacity exhausted");
        }
        let stored = crate::image_catalog::resolve_stored(&self.db, image)
            .ok()
            .or_else(|| crate::image_catalog::resolve(&self.db, image).ok());
        let demand = crate::placement::Demand {
            requested,
            pool: "default",
            memory_mib: memory,
            cpus,
            disk_bytes: u64::from(disk.unwrap_or(common::DEFAULT_DISK_GIB)) * common::GIB,
            image,
            image_digest: stored.as_ref().map(|i| i.reference.digest.as_str()),
            parent_digest: stored.as_ref().map(|i| i.base_digest.as_str()),
            capture_digest: None,
            compatibility: None,
            managed: false,
            disk_sizing: disk.is_some(),
            now: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs(),
        };
        crate::placement::choose(&candidates, &demand)
            .map(|n| n.id.clone())
            .context("node capacity exhausted")
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
        ensure!(
            n["capabilities"]["drain"] != true
                && n["capabilities"]["controller_drain"] != true
                && n["capabilities"]["active"] != false,
            "node draining or inactive"
        );
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
                .is_some_and(|a| a.iter().any(|i| i == image))
                || (n["capabilities"]["image_store_v1"] == true
                    && crate::image_catalog::resolve(&self.db, image).is_ok()),
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
        if node == "local" && op == "snapshot-delete" {
            common::node_operation(local_status, op)?;
        }
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
        limits::admit(&self.root, &self.db, target, op, request, metadata)?;
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
            self.capacity(node, target, memory, cpus, image, local_status)
                .map_err(|e| {
                    if op == "fork" && e.to_string() == "node capacity exhausted" {
                        fork_refusal(node, cpus, memory)
                    } else {
                        e
                    }
                })?;
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
                "delete",
                "snapshot-delete",
                "rename",
                "aws-role",
                "transfer"
            ]
            .contains(&op.as_str()),
            "unsupported user operation"
        );
        if op == "aws-role" {
            return self.aws_role(user, &request, false);
        }
        // A started box with a role needs credentials when this returns, not at the next pass.
        let _credentials =
            matches!(op.as_str(), "start" | "restore").then_some(crate::aws_identity::Notify);
        if matches!(op.as_str(), "share" | "unshare") {
            return self.share(user, &op, &request);
        }
        if op == "rename" {
            return self.rename(user, &request);
        }
        if op == "transfer" {
            return self.hand_over(user, &request);
        }
        if matches!(op.as_str(), "create" | "resize") {
            common::requested_resources(&request)?;
            common::requested_disk(&request)?;
        }
        let name = request["id"].as_str().context("id")?.to_owned();
        common::address(&name)?;
        // Create or fork for another owner: the new box, its operation and quota are theirs.
        let owner = match request.as_object_mut().context("request")?.remove("owner") {
            None | Some(Value::Null) => user,
            Some(email) => {
                let email = email.as_str().context("owner email")?;
                ensure!(
                    matches!(op.as_str(), "create" | "fork"),
                    "only create and fork take an owner"
                );
                ensure!(
                    op == "create" || request.get("node").is_none(),
                    "a fork for another owner stays on its machine's runner; omit the node"
                );
                if op == "create" && !self.pool_admin(user)? {
                    return Err(limits::Rejection(
                        "only a pool admin can create a machine for someone else".into(),
                    )
                    .into());
                }
                if op == "fork" {
                    self.fork_for_others(user, &name)?;
                }
                self.user_by_email(user, email)?
            }
        };
        if owner != user {
            // Keys live in the owner's operation history. Scope the actor's there, so no actor
            // replays the owner's own operations; a retry without a key is a new intent.
            use sha2::{Digest, Sha256};
            let key = match request["operation_key"].as_str() {
                Some(key) => {
                    common::identifier(key)?;
                    key.to_owned()
                }
                None => common::nonce()?,
            };
            request["operation_key"] =
                json!(format!("{:x}", Sha256::digest(format!("{user}:{key}")))[..32]);
        }
        let input = request.clone();
        // A finished delete has no row left to resolve: answer its key before looking.
        if op == "delete"
            && let Some(cached) =
                self.delete_replay(user, "delete", &name, input["operation_key"].as_str())?
        {
            return Ok(cached);
        }
        if op == "snapshot-delete"
            && let Some(cached) = self.delete_replay(
                user,
                "snapshot-delete",
                input["name"].as_str().context("snapshot name")?,
                input["operation_key"].as_str(),
            )?
        {
            return Ok(cached);
        }
        // Resolve authority before any worker contact, including inventory refresh.
        if op != "create" {
            let physical = self.machine(user, &name, &op)?;
            if op == "ssh-info" {
                let parked:bool=self.db.query_row("SELECT node='' AND placement_managed=1 FROM resources WHERE kind='machine' AND physical=?1",[&physical],|r|r.get(0))?;
                if parked {
                    let raw: String = self.db.query_row(
                        "SELECT ssh_policy FROM resources WHERE kind='machine' AND physical=?1",
                        [&physical],
                        |r| r.get(0),
                    )?;
                    let policy: crate::transfers::PublicSshPolicy = serde_json::from_str(&raw)?;
                    return match policy {
                        crate::transfers::PublicSshPolicy::Enrolled(policy) => {
                            Ok(ow_core::ssh_guest::info(&policy))
                        }
                        _ => anyhow::bail!("SSH is not enrolled"),
                    };
                }
            }
            if op == "delete" && self.parked(&physical)? {
                // No runner runs it; the runners it was parked or moved from keep fenced copies.
                ensure!(
                    !limits::unresolved(&self.db, &physical)?,
                    "machine has unresolved operations; reconcile their outcomes first"
                );
                self.remove_old_copies(&physical, "")?;
                return self.delete_missing(user, &name, &physical, &input);
            }
            if op == "snapshot-delete"
                && let Some(value) = self.delete_stored_snapshot(user, &name, &physical, &input)?
            {
                return Ok(value);
            }
            self.legacy_access(&physical)?;
            if let Some(placement) = self.managed_placement(&physical)? {
                if op == "restore" {
                    return Err(limits::Rejection(format!(
                        "machine '{name}' was moved between runners; restore it from its snapshot's artifact (ow restore {name} --artifact ID --node RUNNER, the ID is in ow snapshots)"
                    ))
                    .into());
                }
                ensure!(
                    matches!(
                        op.as_str(),
                        "start"
                            | "stop"
                            | "exec"
                            | "ssh-info"
                            | "ssh-keys"
                            | "put"
                            | "get"
                            | "hibernate"
                            | "resize"
                            | "snapshot"
                            | "snapshot-delete"
                            | "delete"
                    ),
                    "managed mutation requires lifecycle coordination"
                );
                if matches!(op.as_str(), "delete" | "snapshot-delete") {
                    nodes::managed_lifecycle_capable(&self.root, &placement.node)?;
                }
                request["placement"] = json!(placement);
            }
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
                        let owner = if op == "fork" {
                            self.snapshot_owner(user, &name)?
                        } else {
                            user
                        };
                        let snapshot = self.physical(owner, "snapshot", snapshot)?;
                        ensure!(
                            self.placement("snapshot", &snapshot)? == node,
                            "cross-node snapshot denied"
                        );
                        self.snapshot_source(owner, &snapshot, &physical)?;
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
            self.legacy_access(&physical)?;
            let node = self.placement("machine", &physical)?;
            if self.missing(&physical)? && !self.listed(&node, &physical)? {
                return self.delete_missing(user, &name, &physical, &input);
            }
            // A create or fork that never made the machine: only the catalog row is left.
            if self.unmade(&physical)? && !self.listed(&node, &physical)? {
                if limits::unresolved(&self.db, &physical)? {
                    return Err(limits::Rejection(format!(
                        "machine '{name}' is still being created or forked; delete it when that finishes, or after it fails ({CREATION_TIMEOUT_MINUTES} minutes at most)"
                    ))
                    .into());
                }
                // A create or fork interrupted before the runner registered the box can leave
                // its files there; the runner removes them (or has none) before the row goes.
                let swept = wire::request_envelope(
                    &nodes::route(&self.root, &node)?,
                    &json!({"op":"delete","id":physical}),
                )
                .context("runner unreachable; the machine may be back, try again")?;
                if swept["ok"] != true && swept["error"] != "workspace not found" {
                    eprintln!(
                        "delete of never-made {physical} on {node}: {}",
                        swept["error"]
                            .as_str()
                            .unwrap_or("worker returned no error")
                    );
                    return Err(limits::Rejection(format!(
                        "the runner could not remove what the unfinished creation of machine '{name}' left; retry the delete"
                    ))
                    .into());
                }
                return self.delete_missing(user, &name, &physical, &input);
            }
        }
        if op == "create" {
            let selector = request["image"].as_str().unwrap_or("alpine").to_owned();
            // Keep the first image version without rewriting omitted legacy defaults in retry fingerprints.
            let previous: Option<String> = self.db.query_row("SELECT json_extract(metadata,'$.image') FROM resources WHERE owner=?1 AND kind='machine' AND name=?2", params![owner,name], |r| r.get(0)).optional()?.flatten();
            if let Some(previous) =
                previous.filter(|p| p == &selector || crate::image_catalog::same_name(p, &selector))
            {
                if previous != selector {
                    request["image"] = json!(previous);
                }
            } else if let Ok(image) = crate::image_catalog::resolve(&self.db, &selector) {
                request["image"] = json!(format!("{}@{}", image.name, image.version));
            } else if selector.contains('@') {
                anyhow::bail!("image version is unavailable or retired");
            }
            if request["image"].as_str().is_some_and(|s| s.contains('@')) {
                let image = crate::image_catalog::resolve_stored(
                    &self.db,
                    request["image"].as_str().unwrap(),
                )?;
                request["image_digest"] = json!(image.reference.digest);
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
                self.reserve(owner, "machine", &name)?
            } else {
                self.machine(user, &name, &op)?
            };
            self.legacy_access(&physical)?;
            let metadata: String = self.db.query_row(
                "SELECT metadata FROM resources WHERE kind='machine' AND physical=?1",
                [&physical],
                |r| r.get(0),
            )?;
            let metadata: Value = serde_json::from_str(&metadata)?;
            if op == "create" && metadata["state"] != "pending" {
                let key = input["operation_key"]
                    .as_str()
                    .map_or_else(|| format!("c-{physical}"), str::to_owned);
                let retry: bool = self.db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM operations WHERE owner=?1 AND retry_key=?2)",
                    params![owner, key],
                    |r| r.get(0),
                )?;
                if !retry {
                    return Err(NameTaken(name.clone()).into());
                }
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
                let chosen:bool=self.db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE owner=?1 AND resource=?2 AND op IN ('create','fork'))",params![owner,name],|r|r.get(0))?;
                if !chosen {
                    node = self.choose(
                        request["node"].as_str(),
                        request["memory_mib"].as_u64().unwrap_or(256),
                        request["image"].as_str().unwrap_or("alpine"),
                        request["vcpu_count"].as_u64().unwrap_or(1),
                        &local_status,
                        common::requested_disk(&request)?,
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
                "snapshot-delete" => {
                    let id = self.physical(
                        user,
                        "snapshot",
                        request["name"].as_str().context("snapshot")?,
                    )?;
                    self.snapshot_source(user, &id, &physical)?;
                    ensure!(
                        self.placement("snapshot", &id)? == node,
                        "snapshot placement mismatch"
                    );
                    // A move or publication in flight still needs the sealed files.
                    let busy: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM transfer_locks WHERE machine=?1) OR EXISTS(SELECT 1 FROM artifacts WHERE kind='snapshot' AND local_name=?2 AND state='uploading')",params![physical,id],|r|r.get(0))?;
                    ensure!(
                        !busy,
                        "snapshot is in use by a move or publication; retry when it finishes"
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
                        let snapshots = self.snapshot_owner(user, &name)?;
                        let snapshot = self.physical(snapshots, "snapshot", snapshot)?;
                        ensure!(
                            self.placement("snapshot", &snapshot)? == node,
                            "cross-node fork denied"
                        );
                        self.snapshot_source(snapshots, &snapshot, &physical)?;
                        request["snapshot"] = json!(snapshot);
                    }
                    let child = request["child"].as_str().context("child")?;
                    let new_child = self.physical(owner, "machine", child).is_err();
                    target = self.reserve(owner, "machine", child)?;
                    let existing:bool=self.db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE owner=?1 AND resource=?2 AND op='fork')",params![owner,child],|r|r.get(0))?;
                    // A live fork captures the running box; say so before anything is reserved.
                    if !existing
                        && request["snapshot"].is_null()
                        && let Some(state @ ("stopped" | "hibernated")) = metadata["state"].as_str()
                    {
                        return Err(limits::Rejection(format!(
                            "machine '{name}' is {state}; start the box first (ow start {name}), or fork from one of its snapshots (--snapshot)"
                        ))
                        .into());
                    }
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
                let prior:Option<String>=self.db.query_row("SELECT request FROM operations WHERE owner=?1 AND op=?2 AND resource=?3 AND state<>'failed' ORDER BY id LIMIT 1",params![owner,op,identifier],|r|r.get(0)).optional()?;
                let mut normalized = request.clone();
                normalized.as_object_mut().unwrap().remove("operation_key");
                if let Some(prior) = prior {
                    ensure!(
                        prior == json!({"request":normalized,"node":node}).to_string(),
                        "resource operation conflicts with original parameters"
                    );
                }
            }
            self.legacy_access(&target)?;
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
            let previous:Option<(i64,String,String,Option<String>)>=self.db.query_row("SELECT id,request,state,response FROM operations WHERE owner=?1 AND retry_key=?2",params![owner,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
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
                    return Err(self.outcome_error(owner, id));
                }
                if state == "failed" {
                    return Err(self.outcome_error(owner, id));
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
            } else if matches!(op.as_str(), "snapshot" | "snapshot-delete") {
                input["name"].as_str().unwrap_or(&name)
            } else {
                &name
            };
            self.db.execute("INSERT INTO operations(owner,op,resource,state,node,retry_key,request) VALUES(?1,?2,?3,'pending',?4,?5,?6)",params![owner,op,resource,node,key,fingerprint])?;
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
        let mut diagnostic = "node route unavailable before dispatch";
        let preparation = (|| -> Result<PathBuf> {
            let root = nodes::route(&self.root, &node)?;
            diagnostic = "image pull unavailable before create dispatch";
            let pending_create = op=="create" && self.db.query_row("SELECT json_extract(metadata,'$.state')='pending' FROM resources WHERE kind='machine' AND physical=?1",[&physical],|r|r.get::<_,bool>(0))?;
            if pending_create && request["image"].as_str().is_some_and(|s| s.contains('@')) {
                let selector = request["image"].as_str().unwrap();
                let image = crate::image_catalog::resolve_stored(&self.db, selector)?;
                crate::image_store::dispatch(
                    &self.root,
                    &self.root,
                    &node,
                    json!({"op":"image-pull","image":image}),
                )?;
            }
            Ok(root)
        })();
        let root = match preparation {
            Ok(root) => root,
            Err(error) => {
                eprintln!("operation {operation} preparation: {error:#}");
                let tx = self.db.transaction()?;
                if tx.execute(
                    "UPDATE operations SET state='not_dispatched',response=?2 WHERE id=?1 AND state='pending'",
                    params![operation, json!({"error":diagnostic}).to_string()],
                )? == 1
                {
                    tx.execute("UPDATE resources SET reserved_mib=?1,reserved_cpus=?2 WHERE kind='machine' AND physical=?3",params![prior_reservation.0,prior_reservation.1,target])?;
                }
                tx.commit()?;
                return Err(self.outcome_error(owner, operation));
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
                return Err(self.outcome_error(owner, operation));
            }
        };
        let uncertain = envelope["uncertain"] == true;
        if envelope["ok"] != true {
            let mut envelope = envelope;
            if !uncertain
                && op == "fork"
                && worker_capacity_refusal(envelope["error"].as_str().unwrap_or(""))
            {
                let (memory, cpus): (i64, i64) = self.db.query_row("SELECT reserved_mib,reserved_cpus FROM resources WHERE kind='machine' AND physical=?1",[&target],|r|Ok((r.get(0)?,r.get(1)?)))?;
                envelope["fork_room"] = json!(fork_room(&node, cpus as u64, memory as u64));
            }
            self.db.execute(
                "UPDATE operations SET state=?1,response=?3,finished_at=CASE WHEN ?1='failed' THEN CURRENT_TIMESTAMP ELSE NULL END WHERE id=?2",
                params![if uncertain { "uncertain" } else { "failed" }, operation, envelope.to_string()],
            )?;
            if !uncertain && op == "fork" {
                // No child was made: its reserved row ends failed, never pending forever.
                self.db.execute(&format!("UPDATE resources SET {UNMADE},updated_at=CURRENT_TIMESTAMP WHERE kind='machine' AND physical=?1 AND json_extract(metadata,'$.state')='pending'"),[&target])?;
            }
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
            return Err(self.outcome_error(owner, operation));
        }
        if op == "ssh-keys" && request.get("placement").is_some() {
            let current = wire::request(
                &root,
                json!({"op":"ssh-policy","id":physical,"placement":request["placement"]}),
            )?;
            let policy: crate::transfers::PublicSshPolicy = serde_json::from_value(current)?;
            let generation: i64 = self.db.query_row(
                "SELECT ssh_policy_generation FROM resources WHERE kind='machine' AND physical=?1",
                [&physical],
                |r| r.get(0),
            )?;
            self.set_transfer_ssh_policy(user, &name, generation, &policy)?;
        }
        if op == "ssh-keys" && request.get("placement").is_none() {
            // A concurrent legacy key change invalidates a transfer admission's policy read.
            self.db.execute("UPDATE resources SET ssh_policy=NULL,ssh_policy_generation=ssh_policy_generation+1 WHERE kind='machine' AND physical=?1",[&physical])?;
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
        } else if op == "snapshot-delete" {
            value["deleted"] == request["name"] && value["workspace"] == request["id"]
        } else {
            value["id"] == target
        };
        let correlated = correlated
            && (op != "create"
                || request["image_digest"].is_null()
                || (value["image_digest"] == request["image_digest"]
                    && value["image"] == request["image"]));
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
            return Err(self.outcome_error(owner, operation));
        }
        if request.get("placement").is_some()
            && matches!(op.as_str(), "start" | "stop" | "hibernate" | "resize")
        {
            // Lifecycle replies predate placement fields; observe the actual current copy.
            let observed = (|| -> Result<Value> {
                let observed = wire::request(
                    &root,
                    json!({"op":"inspect","id":physical,"placement":request["placement"]}),
                )?;
                ensure!(
                    observed["id"] == physical && observed["placement"] == request["placement"],
                    "post-effect inspect has contradictory placement"
                );
                Ok(observed)
            })();
            let observed = match observed {
                Ok(observed) => observed,
                Err(error) => {
                    self.db.execute(
                        "UPDATE operations SET state='uncertain',response=?1 WHERE id=?2",
                        params![
                            json!({"error":format!("post-effect observation failed: {error:#}")})
                                .to_string(),
                            operation
                        ],
                    )?;
                    return Err(self.outcome_error(owner, operation));
                }
            };
            if op == "stop" {
                value["workspace"] = observed;
            } else {
                value = observed;
            }
        }
        if matches!(
            op.as_str(),
            "exec" | "put" | "get" | "ssh-keys" | "ssh-info"
        ) {
        } else if op == "snapshot" {
            let id = request["name"].as_str().unwrap();
            if value["disk_bytes"].is_null() {
                // Its store publication and any fork from it are checked against this size.
                value["disk_bytes"] = self.db.query_row("SELECT json_extract(metadata,'$.disk_bytes') FROM resources WHERE kind='machine' AND physical=?1",[&physical],|r|r.get::<_,Option<i64>>(0))?.map_or(Value::Null, |n| json!(n));
            }
            self.save_node(&node, "snapshot", id, &value)?;
            value = self.public(user, "snapshot", &value)?;
        } else if op == "stop" {
            self.save_node(&node, "machine", &physical, &value["workspace"])?;
            value["workspace"] = self.public(user, "machine", &value["workspace"])?;
        } else if op == "delete" {
            if request.get("placement").is_some() {
                // An error leaves this operation pending: its retry finds the tombstone.
                self.remove_old_copies(&physical, &node)?;
            }
            value = json!({"deleted":name,"snapshots":self.forget(user, &physical)?});
        } else if op == "snapshot-delete" {
            let snapshot = input["name"].as_str().unwrap();
            let id = request["name"].as_str().unwrap();
            let tx = self.db.transaction()?;
            // The name is free again: its capture operation stops claiming it.
            tx.execute("UPDATE operations SET resource='deleted:'||resource WHERE op='snapshot' AND owner=?1 AND resource=?2",params![user,snapshot])?;
            tx.execute(
                "DELETE FROM resources WHERE kind='snapshot' AND physical=?1",
                [id],
            )?;
            // This runner's copy is gone; its store publication is no longer a root.
            tx.execute(
                "DELETE FROM drain_publications WHERE node=?1 AND name=?2",
                params![node, id],
            )?;
            tx.commit()?;
            value = json!({"deleted":snapshot,"workspace":name});
        } else {
            self.save_node(&node, "machine", &target, &value)?;
            value = self.public(owner, "machine", &value)?;
        }
        value["operation"] = self.operation_info(owner, operation)?;
        value["operation"]["state"] = json!("succeeded");
        value["operation_id"] = json!(operation);
        value["node"] = json!(node);
        // A box made for someone else, or a pool admin's fork of another owner's box (even one
        // shared with them).
        if owner != user || op == "fork" {
            let (from, source): (i64, String) = self.db.query_row(
                "SELECT owner,name FROM resources WHERE kind='machine' AND physical=?1",
                [&physical],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if owner != user || from != user && self.pool_admin(user)? {
                let fork = op == "fork";
                let child = if fork {
                    input["child"].as_str().unwrap_or(&name)
                } else {
                    &name
                };
                self.audit(
                    operation,
                    user,
                    &op,
                    child,
                    fork.then_some(source.as_str()),
                    fork.then_some(from),
                    owner,
                    false,
                )?;
            }
            if owner != user {
                value["owner"] = json!(self.email(owner)?);
            }
        }
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

    pub(crate) fn preview_name(&self, user: i64, https: u16) -> Result<String> {
        let physical: String = self.db.query_row(
            "SELECT physical FROM publications WHERE https_port=?1 AND active=1",
            [https],
            |r| r.get(0),
        )?;
        let name = self
            .logical(user, "machine", &physical)?
            .context("not shared")?;
        self.machine(user, &name, "preview")?;
        Ok(name)
    }
    pub fn preview_target(&self, user: i64, https: u16) -> Result<crate::preview::Target> {
        let (physical, port): (String,u16) = self.db.query_row("SELECT p.physical,p.guest_port FROM publications p JOIN resources r ON r.kind='machine' AND r.physical=p.physical WHERE p.https_port=?1 AND p.active=1",[https],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let name = self
            .logical(user, "machine", &physical)?
            .context("not shared")?;
        self.legacy_access(&physical)?;
        let node = self.placement("machine", &physical)?;
        nodes::operation_capable(&self.root, &node, "tunnel")?;
        let revoked = self.watched(user, &name, "preview", &physical)?;
        Ok(crate::preview::Target {
            root: nodes::route(&self.root, &node)?,
            id: physical.clone(),
            placement: self.managed_placement(&physical)?,
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
        self.legacy_access(&physical)?;
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
        self.legacy_access(&physical)?;
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
    fn first_use_errors_are_safe_and_distinct() {
        for (diagnostic, code) in [
            ("no online node available", "node_unavailable"),
            ("node lacks prepared image", "unavailable_image"),
            ("node capacity exhausted", "insufficient_capacity"),
            (
                "runner does not support disk sizing; upgrade its worker and agent first (Linux Firecracker only)",
                "unsupported_capability",
            ),
        ] {
            let (actual, message) = first_use_error(diagnostic).unwrap();
            assert_eq!(actual, code);
            assert!(message.contains("ow "));
            assert_eq!(public_operation_error("create", diagnostic), message);
        }
        assert_eq!(
            public_operation_error("exec", "guest command timed out after 30s"),
            COMMAND_TIMEOUT
        );
        assert!(first_use_error("node capacity exhausted: /private/tenant-secret").is_none());
        assert!(
            !public_operation_error("create", "/private/tenant-secret").contains("tenant-secret")
        );
    }

    #[test]
    fn discovery_matches_admission_and_hides_other_owners() {
        let root = fixture(false);
        let mut c = Catalog::open(&root, "https://example.cloudflareaccess.com", None).unwrap();
        let alice = c.user(&identity("alice", "alice@example.test")).unwrap();
        let bob = c
            .user(&identity("bob", "bob-private@example.test"))
            .unwrap();
        let physical = c.reserve(bob, "machine", "secret-project").unwrap();
        c.db.execute("UPDATE resources SET node='runner',reserved_mib=1024,reserved_cpus=1 WHERE physical=?1", [&physical]).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cap = json!({"backend":"firecracker","arch":"x86_64","runtime":common::RUNTIME,"protocol":1,"memory_mib":4096,"vcpus":4,"slots":2,"images":["ubuntu"],"disk_resize_v1":true,"available_disk_bytes":64*common::GIB,"disk_observed_unix_s":now,"max_guest_memory_mib":4096,"max_guest_vcpus":4});
        c.db.execute("INSERT INTO nodes(id,heartbeat,memory_mib,slots,capabilities) VALUES('runner',?1,4096,2,?2)", params![now as i64,cap.to_string()]).unwrap();
        c.db.execute("INSERT INTO node_usage(node,extra_mib,extra_cpus,extra_slots) VALUES('runner',512,1,0)", []).unwrap();
        let inventory = nodes::inventory(&root).unwrap();
        let output = c.discovery(alice, &inventory).unwrap();
        assert_eq!(output["identity"]["email"], "alice@example.test");
        assert_eq!(output["nodes"][0]["reserved"]["memory_mib"], 1536);
        assert_eq!(output["nodes"][0]["available"]["memory_mib"], 2560);
        assert_eq!(output["nodes"][0]["available"]["vcpus"], 2);
        assert_eq!(output["nodes"][0]["available"]["slots"], 1);
        assert!(!output.to_string().contains("secret-project"));
        assert!(!output.to_string().contains("bob-private"));
        assert!(!output.to_string().contains(&physical));
        assert_eq!(
            c.choose(None, 2048, "ubuntu", 2, &json!({}), Some(32))
                .unwrap(),
            "runner"
        );
        assert!(
            c.choose(None, 4096, "ubuntu", 2, &json!({}), Some(32))
                .unwrap_err()
                .to_string()
                .contains("capacity exhausted")
        );
        assert!(
            c.choose(None, 256, "arch", 1, &json!({}), Some(32))
                .unwrap_err()
                .to_string()
                .contains("lacks prepared image")
        );
        c.db.execute("UPDATE nodes SET heartbeat=0", []).unwrap();
        assert!(
            c.choose(None, 256, "ubuntu", 1, &json!({}), Some(32))
                .unwrap_err()
                .to_string()
                .contains("no online node")
        );
        let output = c
            .discovery(alice, &nodes::inventory(&root).unwrap())
            .unwrap();
        assert!(output["nodes"][0]["available"].is_null());
        drop(c);
        std::fs::remove_dir_all(root).unwrap();
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
            os::unix::{fs::PermissionsExt, net::UnixListener},
            sync::{
                Arc,
                atomic::{AtomicBool, AtomicUsize, Ordering},
            },
            time::Duration,
        };
        let root =
            PathBuf::from("/tmp").join(format!("ow-create-review-{}", common::nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        nodes::write_private(
            &root.join("limits.json"),
            &json!({"owner_defaults":{"cpus":16,"memory_mib":32768,"disk_gib":16384,"boxes":8}}),
        )
        .unwrap();
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
        set(&format!(
            "DELETE FROM settings WHERE key='min_reader_schema'; PRAGMA user_version={next}"
        ));
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
        let triggers: Vec<String> =
            c.db.prepare(
                "SELECT name FROM sqlite_master WHERE type='trigger' AND (name LIKE 'cache_guard_%' OR name LIKE 'gc_%' OR name LIKE 'drain_%')",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for trigger in triggers {
            c.db.execute_batch(&format!("DROP TRIGGER {trigger}"))
                .unwrap();
        }
        c.db.execute_batch("DROP TABLE cache_evictions").unwrap();
        c.db.execute_batch(
            "ALTER TABLE resources DROP COLUMN placement_managed; ALTER TABLE resources DROP COLUMN placement_epoch; ALTER TABLE resources DROP COLUMN current_capture; ALTER TABLE resources DROP COLUMN ssh_policy; ALTER TABLE resources DROP COLUMN ssh_policy_generation; DROP TABLE transfer_reservations; DROP TABLE transfer_locks; DROP TABLE transfers; ALTER TABLE resources DROP COLUMN node; ALTER TABLE resources DROP COLUMN reserved_mib; ALTER TABLE resources DROP COLUMN reserved_cpus; DROP INDEX operation_retry; ALTER TABLE operations DROP COLUMN node; ALTER TABLE operations DROP COLUMN retry_key; ALTER TABLE operations DROP COLUMN request; ALTER TABLE operations DROP COLUMN response; CREATE UNIQUE INDEX legacy_physical ON resources(physical); PRAGMA user_version=1;",
        )
        .unwrap();
        drop(c);
        let c = Catalog::open(&root, &owner.issuer, None).unwrap();
        let version: i64 =
            c.db.query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
        assert_eq!(version, nodes::SCHEMA);
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
