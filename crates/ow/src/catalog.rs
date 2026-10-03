//! Durable control-plane ownership. The private worker journal remains runtime recovery state.
use crate::{common, wire};
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
}

impl Catalog {
    pub fn open(root: &Path, issuer: &str, bootstrap: Option<&str>) -> Result<Self> {
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
        ensure!(version <= 1, "catalog schema is newer than this binary");
        db.execute_batch("BEGIN IMMEDIATE;
          CREATE TABLE IF NOT EXISTS users(id INTEGER PRIMARY KEY, issuer TEXT NOT NULL, subject TEXT, email TEXT NOT NULL, UNIQUE(issuer,subject), UNIQUE(issuer,email));
          CREATE TABLE IF NOT EXISTS resources(owner INTEGER NOT NULL REFERENCES users(id), kind TEXT NOT NULL CHECK(kind IN ('machine','snapshot')), name TEXT NOT NULL, physical TEXT NOT NULL UNIQUE, metadata TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY(owner,kind,name));
          CREATE TABLE IF NOT EXISTS operations(id INTEGER PRIMARY KEY, owner INTEGER NOT NULL REFERENCES users(id), op TEXT NOT NULL, resource TEXT NOT NULL, state TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, finished_at TEXT);
          CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
          PRAGMA user_version=1; COMMIT;")?;
        let mut catalog = Self {
            db,
            root: root.into(),
        };
        let migrated: bool = catalog.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM settings WHERE key='legacy_import')",
            [],
            |r| r.get(0),
        )?;
        if !migrated {
            let machines = wire::request(root, json!({"op":"list"}))?;
            let snapshots = wire::request(root, json!({"op":"snapshots"}))?;
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
        Ok(catalog)
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
    fn save(&mut self, physical: &str, value: &Value) -> Result<()> {
        self.db.execute(
            "UPDATE resources SET metadata=?1,updated_at=CURRENT_TIMESTAMP WHERE physical=?2",
            params![value.to_string(), physical],
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

    fn reconcile(&mut self) -> Result<(Value, Value)> {
        let machines = wire::request(&self.root, json!({"op":"list"}))?;
        let snapshots = wire::request(&self.root, json!({"op":"snapshots"}))?;
        for m in machines.as_array().context("invalid machines")? {
            self.save(m["id"].as_str().context("invalid machine")?, m)?;
        }
        // Worker-generated hibernation/fork snapshots inherit only their registered parent owner.
        for s in snapshots.as_array().context("invalid snapshots")? {
            let physical = s["name"].as_str().context("invalid snapshot")?;
            let parent = s["workspace"].as_str().context("invalid snapshot parent")?;
            let owner: Option<i64> = self
                .db
                .query_row(
                    "SELECT owner FROM resources WHERE kind='machine' AND physical=?1",
                    [parent],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(owner) = owner {
                // Never overwrite a user's different same-name checkpoint.
                let name=if self.db.query_row("SELECT EXISTS(SELECT 1 FROM resources WHERE owner=?1 AND kind='snapshot' AND name=?2 AND physical<>?2)",params![owner,physical],|r|r.get::<_,bool>(0))? { format!("auto-{}",&common::nonce()?[..24]) } else {physical.to_owned()};
                self.db.execute("INSERT OR IGNORE INTO resources(owner,kind,name,physical,metadata) VALUES(?1,'snapshot',?2,?3,?4)",params![owner,name,physical,s.to_string()])?;
                self.save(physical, s)?;
            }
        }
        Ok((machines, snapshots))
    }
    pub fn state(&mut self, user: i64) -> Result<Value> {
        let (machines, snapshots) = self.reconcile()?;
        let mut owned = Vec::new();
        let mut checkpoints = Vec::new();
        for (kind, values, target) in [
            (
                "machine",
                machines.as_array().context("machines")?,
                &mut owned,
            ),
            (
                "snapshot",
                snapshots.as_array().context("snapshots")?,
                &mut checkpoints,
            ),
        ] {
            for v in values {
                let key = if kind == "machine" { "id" } else { "name" };
                if self
                    .logical(user, kind, v[key].as_str().context("ID")?)?
                    .is_some()
                {
                    target.push(self.public(user, kind, v)?);
                }
            }
        }
        let raw = wire::request(&self.root, json!({"op":"stats"}))?;
        let mut running = BTreeMap::new();
        let mut pss = 0u64;
        let mut reserved = 0u64;
        if let Some(entries) = raw["running"].as_object() {
            for (id, stats) in entries {
                if let Some(name) = self.logical(user, "machine", id)? {
                    pss += stats["pss_kib"].as_u64().unwrap_or(0);
                    reserved += stats["reserved_mib"].as_u64().unwrap_or(0);
                    running.insert(name, stats.clone());
                }
            }
        }
        // Only own usage is exposed; host capacity is a public admission limit, not other users' usage.
        Ok(
            json!({"workspaces":owned,"snapshots":checkpoints,"stats":{"running":running,"total_pss_kib":pss,"reserved_memory_mib":reserved,"limit_memory_mib":raw["limit_memory_mib"],"note":raw["note"]}}),
        )
    }

    pub fn operation(&mut self, user: i64, mut request: Value) -> Result<Value> {
        let op = request["op"]
            .as_str()
            .context("missing operation")?
            .to_owned();
        let name = request["id"].as_str().context("missing ID")?.to_owned();
        let physical = if op == "create" {
            self.reserve(user, "machine", &name)?
        } else {
            self.physical(user, "machine", &name)?
        };
        request["id"] = json!(physical);
        match op.as_str() {
            "create" | "start" | "stop" | "exec" | "hibernate" => {}
            "snapshot" => {
                let name = request["name"].as_str().context("name")?;
                request["name"] = json!(self.reserve(user, "snapshot", name)?);
            }
            "restore" => {
                let name = request["name"].as_str().context("name")?;
                request["name"] = json!(self.physical(user, "snapshot", name)?);
            }
            "fork" => {
                if let Some(snapshot) = request["snapshot"].as_str() {
                    request["snapshot"] = json!(self.physical(user, "snapshot", snapshot)?);
                }
                let name = request["child"].as_str().context("child")?;
                request["child"] = json!(self.reserve(user, "machine", name)?);
            }
            _ => anyhow::bail!("unsupported user operation"),
        }
        self.db.execute(
            "INSERT INTO operations(owner,op,resource,state) VALUES(?1,?2,?3,'pending')",
            params![user, op, name],
        )?;
        let operation = self.db.last_insert_rowid();
        let result = wire::request(&self.root, request);
        self.db.execute(
            "UPDATE operations SET state=?1,finished_at=CURRENT_TIMESTAMP WHERE id=?2",
            params![
                if result.is_ok() {
                    "succeeded"
                } else {
                    "failed"
                },
                operation
            ],
        )?;
        let value = result?;
        self.reconcile()?;
        Ok(if op == "exec" {
            value
        } else if op == "snapshot" {
            self.public(user, "snapshot", &value)?
        } else if op == "stop" {
            let mut value = value;
            value["workspace"] = self.public(user, "machine", &value["workspace"])?;
            value
        } else {
            self.public(user, "machine", &value)?
        })
    }

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
    fn legacy_import_requires_explicit_owner() {
        let root = fixture(true);
        assert!(Catalog::open(&root, "https://example.cloudflareaccess.com", None).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
