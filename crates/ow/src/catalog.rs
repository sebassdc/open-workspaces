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
impl std::fmt::Display for OperationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Workspace operation {} is {}",
            self.0["id"], self.0["state"]
        )
    }
}
impl std::error::Error for OperationFailure {}

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
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(version <= 3, "catalog schema is newer than this binary");
        db.execute_batch("BEGIN IMMEDIATE;
          CREATE TABLE IF NOT EXISTS users(id INTEGER PRIMARY KEY, issuer TEXT NOT NULL, subject TEXT, email TEXT NOT NULL, UNIQUE(issuer,subject), UNIQUE(issuer,email));
          CREATE TABLE IF NOT EXISTS resources(owner INTEGER NOT NULL REFERENCES users(id), kind TEXT NOT NULL CHECK(kind IN ('machine','snapshot')), name TEXT NOT NULL, physical TEXT NOT NULL, metadata TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY(owner,kind,name), UNIQUE(kind,physical));
          CREATE TABLE IF NOT EXISTS operations(id INTEGER PRIMARY KEY, owner INTEGER NOT NULL REFERENCES users(id), op TEXT NOT NULL, resource TEXT NOT NULL, state TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, finished_at TEXT);
          CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
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
        db.execute_batch("UPDATE resources SET reserved_mib=COALESCE(json_extract(metadata,'$.memory_mib'),0) WHERE kind='machine' AND reserved_mib=0 AND json_extract(metadata,'$.state')='running'; PRAGMA user_version=3; COMMIT;")?;
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
        ensure!(
            !identity.subject.is_empty()
                && identity.subject.len() <= 512
                && identity.email.len() <= 254,
            "invalid identity"
        );
        let email = identity.email.to_ascii_lowercase();
        let tx = self.db.transaction()?;
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
                "SELECT name FROM resources WHERE owner=?1 AND kind=?2 AND physical=?3",
                params![user, kind, physical],
                |r| r.get(0),
            )
            .optional()?)
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
        Ok(self.db.query_row("SELECT id,op,resource,state,node,retry_key FROM operations WHERE owner=?1 AND id=?2",params![user,operation],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"op":r.get::<_,String>(1)?,"resource":r.get::<_,String>(2)?,"state":r.get::<_,String>(3)?,"node":r.get::<_,String>(4)?,"retry_key":r.get::<_,Option<String>>(5)?})))?)
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
            let Ok(snapshots) = wire::request(&root, json!({"op":"snapshots"})) else {
                continue;
            };
            let (mut extra_mib, mut extra_cpus, mut extra_slots) = (0i64, 0i64, 0i64);
            for m in machines.as_array().context("invalid machines")? {
                let physical = m["id"].as_str().context("invalid ID")?;
                if m["state"] == "running"
                    && !self.placement("machine", physical).is_ok_and(|n| n == node)
                {
                    extra_mib += m["memory_mib"].as_i64().unwrap_or(4096);
                    extra_cpus += m["vcpu_count"].as_i64().unwrap_or(16);
                    extra_slots += 1;
                }
                self.save_node(&node, "machine", physical, m)?;
                if m["state"] == "running" {
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
            let mut value: Value = serde_json::from_str(&metadata)?;
            value[if kind == "machine" { "id" } else { "name" }] = json!(physical);
            value["node"] = json!(node);
            let online = if node == "local" {
                self.root.join("control.sock").exists()
            } else {
                nodes::online(&self.root, &node).is_ok()
            };
            value["node_online"] = json!(online);
            if !online {
                value["observed_state"] = value["state"].clone();
                value["state"] = json!("offline");
            }
            let value = self.public(user, &kind, &value)?;
            if kind == "machine" {
                reserved += memory;
                machines.push(value);
            } else {
                snapshots.push(value);
            }
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
                        if let Some(name) = self.logical(user, "machine", physical)? {
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
            json!({"operations":operations,"workspaces":machines,"snapshots":snapshots,"nodes":nodes,"stats":{"running":running,"total_pss_kib":pss,"reserved_memory_mib":reserved,"limit_memory_mib":limit,"note":"Offline usage is stale; reservations remain fixed"}}),
        )
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
    ) -> Result<String> {
        let inventory = nodes::inventory(&self.root)?;
        if inventory.as_array().unwrap().is_empty() && requested.is_none() {
            return Ok("local".into());
        }
        let mut candidates = Vec::new();
        for n in inventory.as_array().unwrap() {
            let id = n["id"].as_str().unwrap();
            if requested.is_some_and(|r| r != id)
                || n["online"] != true
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
    ) -> Result<()> {
        if node == "local" {
            return Ok(());
        }
        nodes::online(&self.root, node)?;
        let inventory = nodes::inventory(&self.root)?;
        let n = inventory
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == node)
            .context("node")?;
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
    ) -> Result<()> {
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
            self.capacity(&node, &target, memory, cpus, image)?;
            self.db.execute("UPDATE resources SET reserved_mib=?1,reserved_cpus=?2 WHERE kind='machine' AND physical=?3",params![memory as i64,cpus as i64,target])?;
        }
        Ok(())
    }
    pub fn operation(&mut self, user: i64, mut request: Value) -> Result<Value> {
        let op = request["op"].as_str().context("op")?.to_owned();
        ensure!(
            [
                "create",
                "start",
                "stop",
                "exec",
                "put",
                "get",
                "hibernate",
                "snapshot",
                "fork",
                "restore"
            ]
            .contains(&op.as_str()),
            "unsupported user operation"
        );
        let name = request["id"].as_str().context("id")?.to_owned();
        common::identifier(&name)?;
        let input = request.clone();
        // Resolve authority before any worker contact, including inventory refresh.
        if op != "create" {
            let physical = self.physical(user, "machine", &name)?;
            let node = self.placement("machine", &physical)?;
            nodes::online(&self.root, &node)?;
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
        // BEGIN IMMEDIATE serializes placement, operation fingerprint and memory/slot reservation
        // across gateway processes. No network I/O occurs inside this transaction.
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        let mut prior_reservation = (0i64, 0i64);
        let prepared = (|| -> Result<(String, String, String, i64, Option<Value>)> {
            let physical = if op == "create" {
                self.reserve(user, "machine", &name)?
            } else {
                self.physical(user, "machine", &name)?
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
                ensure!(
                    state != "failed",
                    "operation previously rejected; use a new operation_key for deliberate retry"
                );
                if state == "not_dispatched" {
                    self.admit(&node, &target, &op, &request, &metadata)?;
                    self.db
                        .execute("UPDATE operations SET state='pending' WHERE id=?1", [id])?;
                }
                // Uncertain effects retry only the same durable agent journal entry.
                return Ok((physical, node, target, id, None));
            }
            nodes::online(&self.root, &node)?;
            self.admit(&node, &target, &op, &request, &metadata)?;
            let resource = if op == "fork" {
                input["child"].as_str().unwrap_or(&name)
            } else if op == "snapshot" {
                input["name"].as_str().unwrap_or(&name)
            } else {
                &name
            };
            self.db.execute("INSERT INTO operations(owner,op,resource,state,node,retry_key,request) VALUES(?1,?2,?3,'pending',?4,?5,?6)",params![user,op,resource,node,key,fingerprint])?;
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
            Err(_) => {
                if self.db.execute(
                    "UPDATE operations SET state='not_dispatched' WHERE id=?1 AND state='pending'",
                    [operation],
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
                    "UPDATE operations SET state='uncertain' WHERE id=?1",
                    [operation],
                )?;
                eprintln!("operation {operation} transport: {e:#}");
                return Err(self.outcome_error(user, operation));
            }
        };
        let uncertain = envelope["uncertain"] == true;
        if envelope["ok"] != true {
            self.db.execute(
                "UPDATE operations SET state=?1 WHERE id=?2",
                params![if uncertain { "uncertain" } else { "failed" }, operation],
            )?;
            // A runtime error can follow partial effects. Reconcile positive inventory before
            // releasing demand; the error itself is never proof the target stopped.
            self.reconcile()?;
            return Err(self.outcome_error(user, operation));
        }
        let mut value = envelope["result"].clone();
        let correlated = if matches!(op.as_str(), "exec" | "put" | "get") {
            true
        } else if op == "snapshot" {
            value["name"] == request["name"] && value["workspace"] == request["id"]
        } else if op == "stop" {
            value["workspace"]["id"] == request["id"]
        } else {
            value["id"] == target
        };
        if !correlated {
            self.db.execute(
                "UPDATE operations SET state='uncertain' WHERE id=?1",
                [operation],
            )?;
            return Err(self.outcome_error(user, operation));
        }
        if matches!(op.as_str(), "exec" | "put" | "get") {
        } else if op == "snapshot" {
            let id = request["name"].as_str().unwrap();
            self.save_node(&node, "snapshot", id, &value)?;
            value = self.public(user, "snapshot", &value)?;
        } else if op == "stop" {
            self.save_node(&node, "machine", &physical, &value["workspace"])?;
            value["workspace"] = self.public(user, "machine", &value["workspace"])?;
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
    pub fn terminal_target(&self, user: i64, name: &str) -> Result<(PathBuf, String)> {
        let physical = self.physical(user, "machine", name)?;
        let node = self.placement("machine", &physical)?;
        Ok((nodes::route(&self.root, &node)?, physical))
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
