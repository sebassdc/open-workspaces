//! Controller-only admission policy; no worker restart or guest policy changes.
use crate::{common, nodes};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

/// Only this controller-generated diagnostic is safe to return to an API caller.
#[derive(Debug)]
pub(crate) struct Rejection(pub String);
impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Rejection {}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Resources {
    pub cpus: u64,
    pub memory_mib: u64,
    pub disk_gib: u64,
    pub boxes: u64,
}
impl Default for Resources {
    fn default() -> Self {
        Self {
            cpus: 16,
            memory_mib: 32768,
            disk_gib: 256,
            boxes: 8,
        }
    }
}
impl Resources {
    fn values(self) -> [u64; 4] {
        [self.cpus, self.memory_mib, self.disk_gib, self.boxes]
    }
    fn shape(value: &Value) -> Self {
        Self {
            cpus: value["vcpu_count"]
                .as_u64()
                .unwrap_or(common::HOST_CPUS as u64),
            memory_mib: value["memory_mib"]
                .as_u64()
                .unwrap_or(common::HOST_MEMORY_MIB as u64),
            // Unknown disk capacity is never treated as free space.
            disk_gib: value["disk_bytes"]
                .as_u64()
                .map(|n| n.div_ceil(common::GIB))
                .unwrap_or(common::MAX_DISK_GIB as u64),
            boxes: 1,
        }
    }
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Config {
    owner_defaults: Resources,
    owners: BTreeMap<String, Resources>,
    pool: Pool,
}
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Pool {
    max_estimated_cents: u64,
    hours: u64,
    hourly_cents: BTreeMap<String, u64>,
    node_types: BTreeMap<String, String>,
    default_type: Option<String>,
    retired_nodes: Vec<String>,
    /// Explicit elastic liability caps; absent configuration disables scaler proposals.
    elastic: Option<Liability>,
}
impl Default for Pool {
    fn default() -> Self {
        Self {
            max_estimated_cents: 10000,
            hours: 24,
            hourly_cents: BTreeMap::new(),
            node_types: BTreeMap::new(),
            default_type: None,
            retired_nodes: Vec::new(),
            elastic: None,
        }
    }
}
pub(crate) fn load(root: &Path) -> Result<Config> {
    let path = root.join("limits.json");
    let c = match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            nodes::private(&path).context("limits.json must be private")?;
            ensure!(
                std::fs::metadata(&path)?.len() <= 65536,
                "limits.json exceeds 64 KiB"
            );
            serde_json::from_slice::<Config>(&std::fs::read(&path)?)
                .context("invalid limits.json")?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(e.into()),
    };
    for caps in std::iter::once(&c.owner_defaults).chain(c.owners.values()) {
        ensure!(
            caps.values().iter().all(|n| *n <= i64::MAX as u64 / 4),
            "owner limit out of range"
        );
    }
    ensure!(
        c.owners.keys().all(|id| id
            .parse::<i64>()
            .is_ok_and(|n| n > 0 && id == &n.to_string())),
        "owner overrides require canonical positive catalog user IDs"
    );
    ensure!(
        c.pool.hours > 0 && c.pool.hours <= 87600,
        "pool hours must be 1–87600"
    );
    Ok(c)
}

/// Additive table: existing runtime reservations remain untouched.
pub(crate) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS owner_allocations(physical TEXT PRIMARY KEY,cpus INTEGER NOT NULL CHECK(cpus>=0),memory_mib INTEGER NOT NULL CHECK(memory_mib>=0),disk_gib INTEGER NOT NULL CHECK(disk_gib>=0));")?;
    let mut stmt = db.prepare("SELECT physical,metadata FROM resources WHERE kind='machine' AND physical NOT IN (SELECT physical FROM owner_allocations)")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for (physical, text) in rows {
        let metadata: Value = serde_json::from_str(&text)?;
        let pending = unresolved(db, &physical)?;
        if !pending
            && (matches!(
                metadata["state"].as_str(),
                None | Some("pending" | "missing")
            ) || metadata["unmade"] == true)
        {
            continue;
        }
        // A pre-policy unknown outcome has no trustworthy forecast. Reserve the
        // largest supported shape until replay/reconciliation resolves its effects.
        let shape = if pending {
            Resources {
                cpus: common::HOST_CPUS as u64,
                memory_mib: common::HOST_MEMORY_MIB as u64,
                disk_gib: common::MAX_DISK_GIB as u64,
                boxes: 1,
            }
        } else {
            Resources::shape(&metadata)
        };
        save(db, &physical, shape, false)?;
    }
    Ok(())
}
fn allocation(db: &Connection, physical: &str) -> Result<Resources> {
    Ok(db
        .query_row(
            "SELECT cpus,memory_mib,disk_gib FROM owner_allocations WHERE physical=?1",
            [physical],
            |r| {
                Ok(Resources {
                    cpus: r.get::<_, i64>(0)? as u64,
                    memory_mib: r.get::<_, i64>(1)? as u64,
                    disk_gib: r.get::<_, i64>(2)? as u64,
                    boxes: 1,
                })
            },
        )
        .optional()?
        .unwrap_or(Resources {
            cpus: 0,
            memory_mib: 0,
            disk_gib: 0,
            boxes: 0,
        }))
}
fn usage(db: &Connection, user: i64, excluding: &str) -> Result<Resources> {
    Ok(db.query_row("SELECT COALESCE(SUM(a.cpus),0),COALESCE(SUM(a.memory_mib),0),COALESCE(SUM(a.disk_gib),0),COUNT(*) FROM owner_allocations a JOIN resources r ON r.kind='machine' AND r.physical=a.physical WHERE r.owner=?1 AND r.physical<>?2",params![user,excluding],|r| {
        Ok(Resources {cpus:r.get::<_, i64>(0)? as u64,memory_mib:r.get::<_, i64>(1)? as u64,disk_gib:r.get::<_, i64>(2)? as u64,boxes:r.get::<_, i64>(3)? as u64})
    })?)
}
fn save(db: &Connection, physical: &str, r: Resources, keep_max: bool) -> Result<()> {
    let prior = allocation(db, physical)?;
    let r = if keep_max {
        Resources {
            cpus: r.cpus.max(prior.cpus),
            memory_mib: r.memory_mib.max(prior.memory_mib),
            disk_gib: r.disk_gib.max(prior.disk_gib),
            boxes: 1,
        }
    } else {
        r
    };
    db.execute("INSERT INTO owner_allocations VALUES(?1,?2,?3,?4) ON CONFLICT(physical) DO UPDATE SET cpus=?2,memory_mib=?3,disk_gib=?4",params![physical,i64::try_from(r.cpus)?,i64::try_from(r.memory_mib)?,i64::try_from(r.disk_gib)?])?;
    Ok(())
}
pub(crate) fn unresolved(db: &Connection, physical: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE state IN ('pending','uncertain') AND op IN ('create','fork','resize','restore','start') AND (json_extract(request,'$.request.id')=?1 OR json_extract(request,'$.request.child')=?1))",[physical],|r|r.get(0))?)
}
/// Inventory may replace a forecast only when no effect on this machine is unresolved.
pub(crate) fn observe(db: &Connection, physical: &str, value: &Value) -> Result<()> {
    save(
        db,
        physical,
        Resources::shape(value),
        unresolved(db, physical)?,
    )?;
    Ok(())
}
pub(crate) fn absent(db: &Connection, physical: &str) -> Result<()> {
    if !unresolved(db, physical)? {
        db.execute(
            "DELETE FROM owner_allocations WHERE physical=?1",
            [physical],
        )?;
    }
    Ok(())
}
pub(crate) fn state(root: &Path, db: &Connection, user: i64) -> Result<Value> {
    let c = load(root)?;
    Ok(
        json!({"caps":c.owners.get(&user.to_string()).unwrap_or(&c.owner_defaults),"usage":usage(db,user,"")?,"accounting":"provisioned; stopped and hibernated boxes included"}),
    )
}
/// Called inside the catalog's BEGIN IMMEDIATE, before dispatch. Quotas belong to the
/// target owner (shared restore) or child owner (shared fork), never the acting grantee.
pub(crate) fn admit(
    root: &Path,
    db: &Connection,
    target: &str,
    op: &str,
    request: &Value,
    metadata: &Value,
) -> Result<()> {
    if !matches!(op, "create" | "fork" | "resize" | "restore") {
        return Ok(());
    }
    let c = load(root)?;
    let owner: i64 = db.query_row(
        "SELECT owner FROM resources WHERE kind='machine' AND physical=?1",
        [target],
        |r| r.get(0),
    )?;
    let prior = allocation(db, target)?;
    let mut shape = metadata.clone();
    if matches!(op, "fork" | "restore") {
        let snapshot = if op == "fork" {
            &request["snapshot"]
        } else {
            &request["name"]
        };
        if let Some(snapshot) = snapshot.as_str() {
            let text: String = db.query_row(
                "SELECT metadata FROM resources WHERE kind='snapshot' AND physical=?1",
                [snapshot],
                |r| r.get(0),
            )?;
            let snapshot_shape: Value = serde_json::from_str(&text)?;
            // Legacy snapshot summaries omit disk size. Source disks grow only, so
            // their current size is a conservative upper bound on the capture.
            for key in ["memory_mib", "vcpu_count", "image", "disk_bytes"] {
                if let Some(v) = snapshot_shape.get(key) {
                    shape[key] = v.clone();
                }
            }
        }
    }
    if op == "create" {
        shape["memory_mib"] = json!(request["memory_mib"].as_u64().unwrap_or(256));
        shape["vcpu_count"] = json!(request["vcpu_count"].as_u64().unwrap_or(1));
        shape["disk_bytes"] = if request["image"] == "ubuntu-arm64" {
            // Native runners do not advertise their fixed clone capacity yet.
            Value::Null
        } else {
            json!(
                request["disk_gib"]
                    .as_u64()
                    .unwrap_or(common::DEFAULT_DISK_GIB as u64)
                    * common::GIB
            )
        };
    } else if op == "resize" {
        for key in ["memory_mib", "vcpu_count"] {
            if let Some(v) = request.get(key) {
                shape[key] = v.clone();
            }
        }
        if let Some(n) = request["disk_gib"].as_u64() {
            shape["disk_bytes"] = json!(n * common::GIB);
        }
    }
    let wanted = Resources::shape(&shape);
    let used = usage(db, owner, target)?;
    let caps = c
        .owners
        .get(&owner.to_string())
        .unwrap_or(&c.owner_defaults);
    for (name, next, old, used, cap) in [
        ("CPU", wanted.cpus, prior.cpus, used.cpus, caps.cpus),
        (
            "RAM MiB",
            wanted.memory_mib,
            prior.memory_mib,
            used.memory_mib,
            caps.memory_mib,
        ),
        (
            "disk GiB",
            wanted.disk_gib,
            prior.disk_gib,
            used.disk_gib,
            caps.disk_gib,
        ),
        ("boxes", wanted.boxes, prior.boxes, used.boxes, caps.boxes),
    ] {
        // Lowered limits never evict existing boxes or block reductions in that dimension.
        if next > old && !used.checked_add(next).is_some_and(|sum| sum <= cap) {
            return Err(Rejection(format!("owner {name} limit exceeded: requested total {}, limit {cap}; reduce resources or ask the administrator to change limits", used.saturating_add(next))).into());
        }
    }
    save(db, target, wanted, true)
}

/// Forecast all authorized capacity, including offline/revoked nodes. Revocation does
/// not stop cloud billing; only an explicit operator retirement releases its forecast.
pub(crate) fn pool(root: &Path, db: &Connection, candidate: &str) -> Result<()> {
    let c = load(root)?;
    let p = &c.pool;
    ensure!(
        !p.retired_nodes.iter().any(|n| n == candidate),
        "node is retired in pool limits"
    );
    let mut total = 0u64;
    let mut stmt = db.prepare("SELECT id,revoked FROM nodes ORDER BY id")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))? {
        let (node, revoked) = row?;
        if p.retired_nodes.contains(&node) {
            ensure!(revoked, "retired pool node must first be revoked");
            continue;
        }
        let kind = p
            .node_types
            .get(&node)
            .or(p.default_type.as_ref())
            .ok_or_else(|| Rejection(format!("pool price missing for node {node}; ask the administrator to configure pool.node_types")))?;
        let rate = p
            .hourly_cents
            .get(kind)
            .ok_or_else(|| Rejection(format!("pool hourly price missing for instance type {kind}; ask the administrator to configure pool.hourly_cents")))?;
        let cost = rate
            .checked_mul(p.hours)
            .context("pool estimate overflow")?;
        total = total.checked_add(cost).context("pool estimate overflow")?;
    }
    if total > p.max_estimated_cents {
        return Err(Rejection(format!("pool spend cap exceeded: estimated {total} cents over {} hours, cap {} cents; reduce capacity or ask the administrator to change limits",p.hours,p.max_estimated_cents)).into());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn free_pool(root: &Path) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(root.join("limits.json"))
        .unwrap();
    f.write_all(
        br#"{"pool":{"default_type":"test-self-hosted","hourly_cents":{"test-self-hosted":0}}}"#,
    )
    .unwrap();
}

/// Forecast horizon liability, not an elapsed-spend or provider-billing ledger.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Liability {
    pub cents: u64,
    pub instances: u64,
    pub disk_bytes: u64,
}
impl Liability {
    pub(crate) fn add(self, rhs: Self) -> Result<Self> {
        Ok(Self {
            cents: self
                .cents
                .checked_add(rhs.cents)
                .context("cost liability overflow")?,
            instances: self
                .instances
                .checked_add(rhs.instances)
                .context("instance liability overflow")?,
            disk_bytes: self
                .disk_bytes
                .checked_add(rhs.disk_bytes)
                .context("disk liability overflow")?,
        })
    }
    pub(crate) fn multiply(self, n: u64) -> Result<Self> {
        Ok(Self {
            cents: self
                .cents
                .checked_mul(n)
                .context("cost liability overflow")?,
            instances: self
                .instances
                .checked_mul(n)
                .context("instance liability overflow")?,
            disk_bytes: self
                .disk_bytes
                .checked_mul(n)
                .context("disk liability overflow")?,
        })
    }
    pub(crate) fn within(self, cap: Self) -> bool {
        self.cents <= cap.cents
            && self.instances <= cap.instances
            && self.disk_bytes <= cap.disk_bytes
    }
}
/// Shares the existing pool horizon and spend ceiling; no second spend policy.
pub(crate) fn elastic_caps(root: &Path) -> Result<(u64, Liability, Vec<String>)> {
    let p = load(root)?.pool;
    let mut caps = p
        .elastic
        .context("configure limits.pool.elastic before shadow scaling")?;
    caps.cents = caps.cents.min(p.max_estimated_cents);
    Ok((p.hours, caps, p.retired_nodes))
}
