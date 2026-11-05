//! Admin API (docs/ADMIN_API.md): every operator operation is one op on `POST /api/admin`.
//! The gateway serves it over HTTPS to the designated pool owner; the controller serves it to
//! the host's root or controller user on `<controller data dir>/admin.sock`. Both call
//! [`handle`], so they answer the same. The `ow` admin commands only build the request, send
//! it and print the result (`admin_client.rs`); no CLI code opens the controller database.
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Advertised by `GET /api/version` beside the `admin_ops` list.
pub(crate) const CAPABILITY: &str = "admin_api_v1";
/// Largest admin request body.
pub(crate) const BODY_LIMIT: usize = 16 * 1024;
/// The controller's local admin socket, in its data directory.
pub(crate) const SOCKET: &str = "admin.sock";

/// One admin op: its name, the commands that send it (first one primary, the rest aliases),
/// and how the CLI and the audit log treat it.
pub(crate) struct OpSpec {
    pub name: &'static str,
    #[cfg_attr(not(test), allow(dead_code))]
    pub cli: &'static [&'static [&'static str]],
    pub mutating: bool,
    pub operation_key: bool,
    /// `ow health`: exit 1 while `result.alerts` is not empty.
    pub exit_on_alerts: bool,
}

const fn op(name: &'static str, cli: &'static [&'static [&'static str]], mutating: bool) -> OpSpec {
    OpSpec {
        name,
        cli,
        mutating,
        operation_key: false,
        exit_on_alerts: false,
    }
}

/// The op registry: one entry per op. A test checks it against the CLI, the request enum
/// and docs/ADMIN_API.md.
pub(crate) const OPS: &[OpSpec] = &[
    op("config.list", &[&["config", "list"]], false),
    op("config.get", &[&["config", "get"]], false),
    op("config.set", &[&["config", "set"]], true),
    op("config.history", &[&["config", "history"]], false),
    OpSpec {
        exit_on_alerts: true,
        ..op("health.get", &[&["health"]], false)
    },
    op("node.pool.set", &[&["node-pool"]], true),
    OpSpec {
        operation_key: true,
        ..op("token.issue", &[&["token", "issue"]], true)
    },
    op("token.revoke", &[&["token", "revoke"]], true),
    op("token.list", &[&["token", "list"]], false),
    OpSpec {
        operation_key: true,
        ..op("host.invite", &[&["host", "invite"], &["node-join"]], true)
    },
    op("host.budget", &[&["host", "budget"]], true),
    op(
        "host.revoke",
        &[&["host", "revoke"], &["node-revoke"]],
        true,
    ),
    op("placement.status", &[&["placement", "status"]], false),
    op("actions.status", &[&["actions", "status"]], false),
    op("actions.app_key.set", &[&["actions", "app-key", "set"]], true),
];

pub(crate) fn spec(name: &str) -> Option<&'static OpSpec> {
    OPS.iter().find(|s| s.name == name)
}

/// Every op's name, for `GET /api/version`.
pub(crate) fn names() -> Vec<&'static str> {
    OPS.iter().map(|s| s.name).collect()
}

/// A request: `{"op": NAME, ...fields}`. Unknown fields are refused.
#[derive(Deserialize, Debug, PartialEq)]
#[serde(tag = "op", deny_unknown_fields)]
pub(crate) enum AdminRequest {
    #[serde(rename = "config.list")]
    ConfigList {},
    #[serde(rename = "config.get")]
    ConfigGet { key: String },
    #[serde(rename = "config.set")]
    ConfigSet {
        key: String,
        value: Value,
        expected_version: Option<i64>,
    },
    #[serde(rename = "config.history")]
    ConfigHistory {
        key: Option<String>,
        limit: Option<i64>,
    },
    #[serde(rename = "health.get")]
    HealthGet {},
    #[serde(rename = "node.pool.set")]
    NodePoolSet { node: String, pool: String },
    #[serde(rename = "token.issue")]
    TokenIssue {
        name: String,
        owner: String,
        pool: String,
        operation_key: String,
    },
    #[serde(rename = "token.revoke")]
    TokenRevoke { name: String },
    #[serde(rename = "token.list")]
    TokenList {},
    #[serde(rename = "host.invite")]
    HostInvite {
        node: String,
        ttl: Option<u64>,
        memory: Option<u32>,
        slots: Option<u32>,
        cpus: Option<u32>,
        operation_key: String,
    },
    #[serde(rename = "host.budget")]
    HostBudget {
        node: String,
        memory: u32,
        slots: u32,
        cpus: u32,
    },
    #[serde(rename = "host.revoke")]
    HostRevoke { node: String },
    #[serde(rename = "placement.status")]
    PlacementStatus { pool: Option<String> },
    #[serde(rename = "actions.status")]
    ActionsStatus { pool: Option<String> },
    #[serde(rename = "actions.app_key.set")]
    ActionsAppKeySet { private_key: String },
}

/// Who made a request, as the gateway or the socket verified it.
#[derive(Clone, Debug)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) enum Actor {
    Access {
        subject: String,
        email: String,
    },
    Tailnet {
        subject: String,
        email: String,
    },
    /// A peer on admin.sock; `claimed_user` is the unverified `x-ow-claimed-user` header.
    Host {
        uid: u32,
        claimed_user: Option<String>,
    },
}

impl Actor {
    fn json(&self) -> Value {
        match self {
            Actor::Access { subject, email } => {
                json!({"kind":"access","subject":subject,"email":email})
            }
            Actor::Tailnet { subject, email } => {
                json!({"kind":"tailnet","subject":subject,"email":email})
            }
            Actor::Host { uid, claimed_user } => {
                json!({"kind":"host","uid":uid,"claimed_user":claimed_user})
            }
        }
    }
    /// The `config_changes.actor` text.
    fn label(&self) -> String {
        match self {
            Actor::Access { email, .. } => format!("access:{email}"),
            Actor::Tailnet { email, .. } => format!("tailnet:{email}"),
            Actor::Host { uid, claimed_user } => format!(
                "host:{}",
                claimed_user.clone().unwrap_or_else(|| uid.to_string())
            ),
        }
    }
    fn via(&self) -> &'static str {
        match self {
            Actor::Host { .. } => "local",
            _ => "https",
        }
    }
}

/// Where a request is served: the controller data directory, the verified caller, and the
/// controller URL an invitation names.
pub(crate) struct Context<'a> {
    pub root: &'a Path,
    pub actor: Actor,
    pub controller: Option<String>,
}

/// An API error: HTTP status, stable code, one plain sentence and an object of details.
#[derive(Clone, Debug)]
pub(crate) struct Failure {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

pub(crate) fn failure(status: u16, code: &'static str, message: impl Into<String>) -> Failure {
    Failure {
        status,
        code,
        message: message.into(),
        details: json!({}),
    }
}

impl Failure {
    fn with(mut self, details: Value) -> Self {
        self.details = details;
        self
    }
    pub(crate) fn body(&self, op: Option<&str>) -> Value {
        json!({"ok":false,"op":op,"error_code":self.code,"error":self.message,"details":self.details})
    }
}

fn invalid_request(problem: impl std::fmt::Display) -> Failure {
    failure(
        400,
        "invalid_request",
        format!("Invalid request: {problem}."),
    )
}
fn invalid_value(field: &str, reason: impl std::fmt::Display) -> Failure {
    let reason = reason.to_string();
    failure(422, "invalid_value", format!("{field}: {reason}."))
        .with(json!({"key":field,"reason":reason}))
}
fn not_found(kind: &str, name: &str) -> Failure {
    failure(404, "not_found", format!("{kind} {name} not found."))
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for Failure {}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        if let Some(failure) = error.downcast_ref::<Failure>() {
            return failure.clone();
        }
        if let Some(rejection) = error.downcast_ref::<crate::limits::Rejection>() {
            return failure(409, "rejected", rejection.to_string());
        }
        if let Some(rusqlite::Error::SqliteFailure(e, _)) = error.downcast_ref::<rusqlite::Error>()
            && matches!(
                e.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
        {
            return failure(503, "unavailable", "Controller busy or unavailable; retry.");
        }
        eprintln!("admin: internal error: {error:#}");
        failure(500, "internal", "Internal error; see the controller log.")
    }
}
impl From<rusqlite::Error> for Failure {
    fn from(error: rusqlite::Error) -> Self {
        anyhow::Error::from(error).into()
    }
}

type Answer = std::result::Result<Value, Failure>;

/// Parses a request body: unknown ops and fields are refused before any handler runs.
pub(crate) fn parse(raw: &Value) -> std::result::Result<AdminRequest, Failure> {
    let Some(object) = raw.as_object() else {
        return Err(invalid_request("body must be a JSON object"));
    };
    let Some(name) = object.get("op").and_then(Value::as_str) else {
        return Err(invalid_request("op missing or not a string"));
    };
    if spec(name).is_none() {
        return Err(failure(
            400,
            "unknown_op",
            format!("Unknown admin operation {name}; see GET /api/version admin_ops."),
        ));
    }
    serde_json::from_value(raw.clone())
        .map_err(|e| invalid_request(e.to_string().trim_end_matches('.')))
}

/// The admin tables: every admin change, and the operation keys of ops that hand out a
/// secret once. Additive; made here, not on every controller database open.
pub(crate) fn tables(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS admin_audit(id INTEGER PRIMARY KEY,unix_s INTEGER NOT NULL,op TEXT NOT NULL,actor TEXT NOT NULL,via TEXT NOT NULL CHECK(via IN ('https','local')),request TEXT NOT NULL,result TEXT NOT NULL,config_version INTEGER,change TEXT);
      CREATE TABLE IF NOT EXISTS admin_operations(key TEXT PRIMARY KEY,op TEXT NOT NULL,request_hash TEXT NOT NULL,unix_s INTEGER NOT NULL);")?;
    Ok(())
}

/// Serves one admin request after the caller passed the admin gate: (HTTP status, body).
pub(crate) fn handle(ctx: &Context, body: &[u8]) -> (u16, Value) {
    static READY: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::OnceLock::new();
    let ready = READY.get_or_init(Default::default);
    if !ready.lock().is_ok_and(|r| r.contains(ctx.root)) {
        match crate::nodes::db(ctx.root).and_then(|db| tables(&db)) {
            Ok(()) => {
                if let Ok(mut r) = ready.lock() {
                    r.insert(ctx.root.to_path_buf());
                }
            }
            Err(error) => return (503, Failure::from(error).body(None)),
        }
    }
    let raw: Value = match serde_json::from_slice(body) {
        Ok(raw) => raw,
        Err(_) => return (400, invalid_request("body is not JSON").body(None)),
    };
    let name = raw.get("op").and_then(Value::as_str).map(str::to_owned);
    let request = parse(&raw);
    // What the audit log keeps: never a secret a request carries.
    let mut raw = raw;
    if let Some(key) = raw.get_mut("private_key") {
        *key = json!("<redacted>");
    }
    let answer = request.and_then(|request| run(ctx, &raw, request));
    match answer {
        Ok(result) => (200, json!({"ok":true,"op":name,"result":result})),
        Err(failure) => {
            // A refused change is audited too, in its own small transaction.
            if let Some(spec) = name.as_deref().and_then(spec).filter(|s| s.mutating)
                && failure.code != "unknown_op"
                && let Err(error) = crate::nodes::db(ctx.root).and_then(|db| {
                    audit(&db, spec.name, &ctx.actor, &raw, failure.code, None, None)
                })
            {
                eprintln!("admin: audit of a refused {} failed: {error:#}", spec.name);
            }
            (failure.status, failure.body(name.as_deref()))
        }
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// One admin_audit row; its id.
fn audit(
    db: &Connection,
    op: &str,
    actor: &Actor,
    request: &Value,
    result: &str,
    config_version: Option<i64>,
    change: Option<&Value>,
) -> Result<i64> {
    db.execute(
        "INSERT INTO admin_audit(unix_s,op,actor,via,request,result,config_version,change) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            now(),
            op,
            actor.json().to_string(),
            actor.via(),
            request.to_string(),
            result,
            config_version,
            change.map(Value::to_string)
        ],
    )?;
    Ok(db.last_insert_rowid())
}

/// The change event every successful admin change emits (OTLP log, controller log).
fn changed(op: &str, actor: &Actor, extra: Value) {
    let mut attrs = json!({"op":op,"via":actor.via()});
    match actor {
        Actor::Access { email, .. } | Actor::Tailnet { email, .. } => {
            attrs["actor_email"] = json!(email)
        }
        Actor::Host { uid, .. } => attrs["actor_uid"] = json!(uid),
    }
    for (k, v) in extra.as_object().into_iter().flatten() {
        attrs[k] = v.clone();
    }
    eprintln!("admin_change {attrs}");
    crate::telemetry::controller_log(None, "INFO", "admin_change", &attrs);
}

/// An admin request refused at the gate (a signed-in user who is not the owner).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn refused(actor_email: &str) {
    let attrs = json!({"actor_email":actor_email});
    eprintln!("admin_refused {attrs}");
    crate::telemetry::controller_log(None, "WARN", "admin_refused", &attrs);
}

/// Operation keys of ops that hand out a secret once: a retry with the same key and request
/// gets `already_exists` instead of a second secret; another request under it is refused.
fn replayed(
    db: &Connection,
    key: &str,
    op: &str,
    hash: &str,
    name: &str,
    kind: &str,
) -> std::result::Result<(), Failure> {
    crate::common::identifier(key).map_err(|e| invalid_value("operation_key", e))?;
    let prior: Option<(String, String, i64)> = db
        .query_row(
            "SELECT op,request_hash,unix_s FROM admin_operations WHERE key=?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    match prior {
        None => Ok(()),
        Some((o, h, at)) if o == op && h == hash => Err(failure(
            409,
            "already_exists",
            format!("{kind} {name} already exists."),
        )
        .with(json!({"name":name,"issued_unix_s":at}))),
        Some(_) => Err(failure(
            409,
            "operation_key_reused",
            format!("Operation key {key} was used for a different request."),
        )),
    }
}

fn remember(db: &Connection, key: &str, op: &str, hash: &str) -> Result<()> {
    db.execute(
        "INSERT INTO admin_operations(key,op,request_hash,unix_s) VALUES(?1,?2,?3,?4)",
        params![key, op, hash, now()],
    )?;
    Ok(())
}

fn immediate(db: &mut Connection) -> Result<rusqlite::Transaction<'_>> {
    Ok(db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?)
}

fn known_key(key: &str) -> std::result::Result<(), Failure> {
    if crate::telemetry::KEYS.iter().any(|(k, _)| *k == key) {
        Ok(())
    } else {
        Err(failure(
            400,
            "unknown_setting",
            format!(
                "Unknown setting {key}; known: {}.",
                crate::telemetry::KEYS.map(|(k, _)| k).join(", ")
            ),
        ))
    }
}

/// When a changed setting takes effect.
fn effective(key: &str) -> &'static str {
    if key.starts_with("placement.") || key.starts_with("limits.") {
        "next_create"
    } else if key.starts_with("actions.") {
        "next_pass"
    } else if key.starts_with("dashboard.") {
        "next_request"
    } else {
        "next_ack"
    }
}

/// A `config.set` value as the setting's string form: a string goes through the key's
/// grammar (empty means the default); typed JSON must have the key's type.
fn value_text(key: &str, value: &Value) -> std::result::Result<String, Failure> {
    let default = crate::telemetry::default(key);
    match value {
        Value::String(text) if text.is_empty() => Ok(crate::telemetry::KEYS
            .iter()
            .find(|(k, _)| *k == key)
            .map_or("", |(_, d)| d)
            .to_owned()),
        Value::String(text) => Ok(text.clone()),
        Value::Bool(b) if default.is_boolean() => Ok(b.to_string()),
        Value::Array(_) if key == crate::actions::POOLS => {
            crate::actions::from_json(value).map_err(|e| invalid_value(key, format!("{e:#}")))
        }
        Value::Array(_) if default.is_array() => {
            crate::spill::from_json(value).map_err(|e| invalid_value(key, e))
        }
        Value::Number(n) if default.is_u64() && n.is_u64() => Ok(n.to_string()),
        _ => Err(invalid_value(
            key,
            format!(
                "expected {}",
                match default {
                    Value::Bool(_) => "true or false",
                    Value::Number(_) => "a whole number",
                    Value::Array(_) => "a list of rules or their pool:node:max_boxes string",
                    _ => "a string",
                }
            ),
        )),
    }
}

fn run(ctx: &Context, raw: &Value, request: AdminRequest) -> Answer {
    let root = ctx.root;
    match request {
        AdminRequest::ConfigList {} => {
            let mut settings = crate::telemetry::settings(&crate::nodes::db(root)?)?;
            let version = settings
                .as_object_mut()
                .and_then(|s| s.remove("version"))
                .unwrap_or(json!(0));
            Ok(json!({"version":version,"settings":settings}))
        }
        AdminRequest::ConfigGet { key } => {
            known_key(&key)?;
            let settings = crate::telemetry::settings(&crate::nodes::db(root)?)?;
            Ok(
                json!({"key":key,"value":settings[key.as_str()],"default":crate::telemetry::default(&key),"version":settings["version"]}),
            )
        }
        AdminRequest::ConfigSet {
            key,
            value,
            expected_version,
        } => {
            known_key(&key)?;
            // Read before the transaction: it opens its own connection.
            let inventory = if key.starts_with("placement.") {
                crate::nodes::inventory(root)?
            } else {
                json!([])
            };
            let mut db = crate::nodes::db(root)?;
            let tx = immediate(&mut db)?;
            let current = crate::telemetry::settings(&tx)?;
            let version = current["version"].as_i64().unwrap_or(0);
            if let Some(expected) = expected_version
                && expected != version
            {
                return Err(failure(
                    409,
                    "version_conflict",
                    format!(
                        "Configuration changed since version {expected} (now {version}); read it again."
                    ),
                )
                .with(json!({"expected":expected,"current":version})));
            }
            let text = value_text(&key, &value)?;
            let typed = crate::telemetry::typed(&key, &text)
                .map_err(|e| invalid_value(&key, format!("{e:#}")))?;
            if key == crate::dashboard_ui::SETTING {
                crate::dashboard_ui::selectable(root, &text)
                    .map_err(|e| invalid_value(&key, format!("{e:#}")))?;
            }
            let previous = current[key.as_str()].clone();
            let change = json!({"key":key,"value":typed,"previous":previous});
            // An unchanged value is a safe retry: it is not checked against the runners again.
            let warnings: Vec<String> = vec![];
            if previous == typed {
                let audit_id = audit(
                    &tx,
                    "config.set",
                    &ctx.actor,
                    raw,
                    "unchanged",
                    Some(version),
                    Some(&change),
                )?;
                tx.commit()?;
                return Ok(
                    json!({"key":key,"value":typed,"previous":previous,"changed":false,"version":version,
                    "effective":effective(&key),"warnings":warnings,"audit_id":audit_id}),
                );
            }
            let warnings = crate::spill::check(&tx, &inventory, &key, &typed, &current)
                .map_err(|reason| invalid_value(&key, reason))?;
            let version = crate::telemetry::store_setting(&tx, &key, &typed, &ctx.actor.label())?;
            let audit_id = audit(
                &tx,
                "config.set",
                &ctx.actor,
                raw,
                "ok",
                Some(version),
                Some(&change),
            )?;
            tx.commit()?;
            changed(
                "config.set",
                &ctx.actor,
                json!({"key":key,"version":version}),
            );
            Ok(
                json!({"key":key,"value":typed,"previous":previous,"changed":true,"version":version,
                "effective":effective(&key),"warnings":warnings,"audit_id":audit_id}),
            )
        }
        AdminRequest::ConfigHistory { key, limit } => {
            if let Some(key) = &key {
                known_key(key)?;
            }
            let limit = limit.unwrap_or(50);
            if !(1..=500).contains(&limit) {
                return Err(invalid_request("limit must be within 1-500"));
            }
            let db = crate::nodes::db(root)?;
            let rows: Vec<Value> = db
                .prepare("SELECT id,unix_s,change,config_version,actor,via FROM admin_audit WHERE op='config.set' AND result='ok' AND (?1 IS NULL OR json_extract(change,'$.key')=?1) ORDER BY id DESC LIMIT ?2")?
                .query_map(params![key, limit], |r| {
                    let change: Value = serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default();
                    Ok(json!({"audit_id":r.get::<_,i64>(0)?,"unix_s":r.get::<_,i64>(1)?,"key":change["key"],
                        "value":change["value"],"previous":change["previous"],"version":r.get::<_,Option<i64>>(3)?,
                        "actor":serde_json::from_str::<Value>(&r.get::<_,String>(4)?).unwrap_or_default(),"via":r.get::<_,String>(5)?}))
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(json!({"changes":rows}))
        }
        AdminRequest::HealthGet {} => Ok(crate::telemetry::health(root)?),
        AdminRequest::NodePoolSet { node, pool } => {
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (node, pool);
                Err(failure(
                    409,
                    "rejected",
                    "Node pools are managed on a Linux controller.",
                ))
            }
            #[cfg(target_os = "linux")]
            {
                crate::common::identifier(&pool).map_err(|e| invalid_value("pool", e))?;
                let mut db = crate::nodes::db(root)?;
                let tx = immediate(&mut db)?;
                if !tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=?1)",
                    [&node],
                    |r| r.get::<_, bool>(0),
                )? {
                    return Err(not_found("node", &node));
                }
                let previous: String = tx
                    .query_row(
                        "SELECT pool FROM node_placement_policy WHERE node=?1",
                        [&node],
                        |r| r.get(0),
                    )
                    .optional()?
                    .unwrap_or_else(|| "default".into());
                let change = json!({"node":node,"value":pool,"previous":previous});
                if previous == pool {
                    audit(
                        &tx,
                        "node.pool.set",
                        &ctx.actor,
                        raw,
                        "unchanged",
                        None,
                        Some(&change),
                    )?;
                    tx.commit()?;
                    return Ok(
                        json!({"node":node,"pool":pool,"previous":previous,"changed":false}),
                    );
                }
                tx.execute(
                    "INSERT INTO node_placement_policy(node,pool) VALUES(?1,?2) ON CONFLICT(node) DO UPDATE SET pool=excluded.pool",
                    [&node, &pool],
                )?;
                audit(
                    &tx,
                    "node.pool.set",
                    &ctx.actor,
                    raw,
                    "ok",
                    None,
                    Some(&change),
                )?;
                tx.commit()?;
                changed(
                    "node.pool.set",
                    &ctx.actor,
                    json!({"node":node,"pool":pool}),
                );
                Ok(json!({"node":node,"pool":pool,"previous":previous,"changed":true}))
            }
        }
        AdminRequest::TokenIssue {
            name,
            owner,
            pool,
            operation_key,
        } => {
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (name, owner, pool, operation_key);
                Err(failure(
                    409,
                    "rejected",
                    "Workload tokens are managed on a Linux controller.",
                ))
            }
            #[cfg(target_os = "linux")]
            {
                crate::common::identifier(&name).map_err(|e| invalid_value("name", e))?;
                crate::common::identifier(&pool).map_err(|e| invalid_value("pool", e))?;
                let owner = owner.to_ascii_lowercase();
                crate::common::email(&owner).map_err(|e| invalid_value("owner", e))?;
                let mut db = crate::tokens::open(root)
                    .map_err(|e| failure(503, "unavailable", format!("{e}.")))?;
                let tx = immediate(&mut db)?;
                let hash = crate::nodes::digest(&json!([name, owner, pool]).to_string());
                replayed(&tx, &operation_key, "token.issue", &hash, &name, "token")?;
                if tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM workload_tokens WHERE name=?1)",
                    [&name],
                    |r| r.get::<_, bool>(0),
                )? {
                    return Err(failure(
                        409,
                        "already_exists",
                        format!("token {name} already exists."),
                    )
                    .with(json!({"name":name})));
                }
                let issued = crate::tokens::issue(&tx, &name, &owner, &pool)
                    .map_err(|e| invalid_value("owner", e))?;
                remember(&tx, &operation_key, "token.issue", &hash)?;
                audit(&tx, "token.issue", &ctx.actor, raw, "ok", None, None)?;
                tx.commit()?;
                changed("token.issue", &ctx.actor, json!({"name":name,"pool":pool}));
                Ok(issued)
            }
        }
        AdminRequest::TokenRevoke { name } => {
            #[cfg(not(target_os = "linux"))]
            {
                let _ = name;
                Err(failure(
                    409,
                    "rejected",
                    "Workload tokens are managed on a Linux controller.",
                ))
            }
            #[cfg(target_os = "linux")]
            {
                let mut db = crate::tokens::open(root)
                    .map_err(|e| failure(503, "unavailable", format!("{e}.")))?;
                let tx = immediate(&mut db)?;
                if tx.execute("DELETE FROM workload_tokens WHERE name=?1", [&name])? == 1 {
                    audit(&tx, "token.revoke", &ctx.actor, raw, "ok", None, None)?;
                    tx.commit()?;
                    changed("token.revoke", &ctx.actor, json!({"name":name}));
                    return Ok(json!({"revoked":name,"changed":true}));
                }
                // Tokens are deleted, so an earlier revoke in the audit log is what remembers them.
                if tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM admin_audit WHERE op='token.revoke' AND result='ok' AND json_extract(request,'$.name')=?1)",
                    [&name],
                    |r| r.get::<_, bool>(0),
                )? {
                    audit(&tx, "token.revoke", &ctx.actor, raw, "unchanged", None, None)?;
                    tx.commit()?;
                    return Ok(json!({"revoked":name,"changed":false}));
                }
                Err(not_found("token", &name))
            }
        }
        AdminRequest::TokenList {} => {
            #[cfg(not(target_os = "linux"))]
            {
                Err(failure(
                    409,
                    "rejected",
                    "Workload tokens are managed on a Linux controller.",
                ))
            }
            #[cfg(target_os = "linux")]
            {
                let db = crate::tokens::open(root)
                    .map_err(|e| failure(503, "unavailable", format!("{e}.")))?;
                Ok(json!({"tokens":crate::tokens::list(&db)?}))
            }
        }
        AdminRequest::HostInvite {
            node,
            ttl,
            memory,
            slots,
            cpus,
            operation_key,
        } => {
            let (ttl, memory, slots, cpus) = (
                ttl.unwrap_or(600),
                memory.unwrap_or(512),
                slots.unwrap_or(2),
                cpus.unwrap_or(2),
            );
            crate::common::identifier(&node).map_err(|e| invalid_value("node", e))?;
            if node == "local" {
                return Err(invalid_value("node", "local is reserved"));
            }
            if !(1..=3600).contains(&ttl) {
                return Err(invalid_value("ttl", "must be within 1-3600 seconds"));
            }
            crate::common::host_limits(memory, slots, cpus)
                .map_err(|e| invalid_value("limits", e))?;
            let hash = crate::nodes::digest(&json!([node, ttl, memory, slots, cpus]).to_string());
            let db = crate::nodes::db(root)?;
            // Checked again in the invitation's transaction; here for the plainer answer.
            replayed(
                &db,
                &operation_key,
                "host.invite",
                &hash,
                &node,
                "invitation for",
            )?;
            if db.query_row(
                "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=?1 AND (credential_hash IS NOT NULL OR revoked=1))",
                [&node],
                |r| r.get::<_, bool>(0),
            )? {
                return Err(failure(
                    409,
                    "already_exists",
                    format!("node {node} is already enrolled or revoked; use a new node ID."),
                )
                .with(json!({"name":node})));
            }
            drop(db);
            let invitation = crate::nodes::invitation_with(
                root,
                &node,
                ttl,
                memory,
                slots,
                cpus,
                ctx.controller.as_deref(),
                |_| Ok(()),
                |tx| {
                    replayed(
                        tx,
                        &operation_key,
                        "host.invite",
                        &hash,
                        &node,
                        "invitation for",
                    )?;
                    remember(tx, &operation_key, "host.invite", &hash)?;
                    audit(tx, "host.invite", &ctx.actor, raw, "ok", None, None)?;
                    Ok(())
                },
            )?;
            changed("host.invite", &ctx.actor, json!({"node":node}));
            Ok(invitation)
        }
        AdminRequest::HostBudget {
            node,
            memory,
            slots,
            cpus,
        } => {
            crate::common::identifier(&node).map_err(|e| invalid_value("node", e))?;
            crate::common::host_limits(memory, slots, cpus)
                .map_err(|e| invalid_value("limits", e))?;
            if !node_known(root, &node)? {
                return Err(not_found("node", &node));
            }
            let result = crate::nodes::budget_with(root, &node, memory, slots, cpus, |tx| {
                audit(tx, "host.budget", &ctx.actor, raw, "ok", None, None)?;
                Ok(())
            })
            .map_err(|e| {
                if e.is::<crate::limits::Rejection>()
                    || e.is::<Failure>()
                    || matches!(
                        e.downcast_ref::<rusqlite::Error>(),
                        Some(rusqlite::Error::SqliteFailure(..))
                    )
                {
                    Failure::from(e)
                } else {
                    // The budget's own refusals (an enrolled runner, idle boxes to lower it).
                    invalid_value("node", e)
                }
            })?;
            changed("host.budget", &ctx.actor, json!({"node":node}));
            Ok(result)
        }
        AdminRequest::PlacementStatus { pool } => {
            if let Some(pool) = &pool {
                crate::common::identifier(pool)
                    .map_err(|e| invalid_request(format!("pool: {e}")))?;
            }
            Ok(crate::spill::status(root, pool.as_deref())?)
        }
        AdminRequest::ActionsStatus { pool } => {
            if let Some(pool) = &pool {
                crate::common::identifier(pool)
                    .map_err(|e| invalid_request(format!("pool: {e}")))?;
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(failure(409, "rejected", "Runner pools run on a Linux controller."))
            }
            #[cfg(target_os = "linux")]
            {
                Ok(crate::actions_pool::status(root, pool.as_deref())?)
            }
        }
        AdminRequest::ActionsAppKeySet { private_key } => {
            #[cfg(not(target_os = "linux"))]
            {
                let _ = private_key;
                Err(failure(409, "rejected", "Runner pools run on a Linux controller."))
            }
            #[cfg(target_os = "linux")]
            {
                let result = crate::actions_pool::set_app_key(root, &private_key)
                    .map_err(|e| invalid_value("private_key", format!("{e:#}")))?;
                let db = crate::nodes::db(root)?;
                let audit_id = audit(&db, "actions.app_key.set", &ctx.actor, raw, "ok", None, Some(&result))?;
                changed("actions.app_key.set", &ctx.actor, json!({"fingerprint":result["fingerprint"]}));
                let mut result = result;
                result["audit_id"] = json!(audit_id);
                Ok(result)
            }
        }
        AdminRequest::HostRevoke { node } => {
            crate::common::identifier(&node).map_err(|e| invalid_value("node", e))?;
            let mut db = crate::nodes::db(root)?;
            let tx = immediate(&mut db)?;
            let revoked: Option<bool> = tx
                .query_row("SELECT revoked FROM nodes WHERE id=?1", [&node], |r| {
                    r.get(0)
                })
                .optional()?;
            match revoked {
                None => Err(not_found("node", &node)),
                Some(true) => {
                    audit(&tx, "host.revoke", &ctx.actor, raw, "unchanged", None, None)?;
                    tx.commit()?;
                    Ok(json!({"revoked":node,"changed":false}))
                }
                Some(false) => {
                    tx.execute(
                        "UPDATE nodes SET revoked=1,heartbeat=0 WHERE id=?1",
                        [&node],
                    )?;
                    audit(&tx, "host.revoke", &ctx.actor, raw, "ok", None, None)?;
                    tx.commit()?;
                    changed("host.revoke", &ctx.actor, json!({"node":node}));
                    Ok(json!({"revoked":node,"changed":true}))
                }
            }
        }
    }
}

fn node_known(root: &Path, node: &str) -> Result<bool> {
    Ok(crate::nodes::db(root)?.query_row(
        "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=?1)",
        [node],
        |r| r.get(0),
    )?)
}

/// The controller URL a local invitation names: the one `ow host up` saved beside the
/// controller data directory, if any.
pub(crate) fn saved_controller(root: &Path) -> Option<String> {
    let saved: Value =
        serde_json::from_slice(&std::fs::read(root.parent()?.join("host.json")).ok()?).ok()?;
    saved["controller"].as_str().map(str::to_owned)
}

/// `GET /api/version` additions.
pub(crate) fn version_fields(value: &mut Value) {
    if let Some(capabilities) = value["capabilities"].as_array_mut() {
        capabilities.push(json!(CAPABILITY));
    } else {
        value["capabilities"] = json!([CAPABILITY]);
    }
    value["admin_ops"] = json!(names());
}

// ---------------------------------------------------------------------------------------
// admin.sock: the same API for the host's root or controller user.

#[derive(Clone, Debug)]
struct Peer {
    uid: u32,
}
impl
    axum::extract::connect_info::Connected<
        axum::serve::IncomingStream<'_, tokio::net::UnixListener>,
    > for Peer
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, tokio::net::UnixListener>) -> Self {
        // An unknown peer gets an impossible uid and is refused below.
        Peer {
            uid: stream.io().peer_cred().map_or(u32::MAX, |c| c.uid()),
        }
    }
}

/// Root, or the user the controller runs as: anyone who could already edit its database.
fn peer_allowed(uid: u32) -> bool {
    uid == 0 || uid == unsafe { libc::geteuid() }
}

/// Serves `GET /api/version` and `POST /api/admin` on `<root>/admin.sock` (mode 0600) once it
/// holds `admin.lock`; a socket left by an earlier holder is then stale.
pub(crate) async fn serve_socket(root: PathBuf, fallback_controller: Option<String>) -> Result<()> {
    use axum::{
        Router,
        body::Body,
        extract::{ConnectInfo, State},
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::{get, post},
    };
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt, unix::fs::PermissionsExt};
    // The node controller and the dashboard both offer the socket; whichever holds the lock
    // serves it, and the other takes over if that one stops.
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join("admin.lock"))?;
    while unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    let path = root.join(SOCKET);
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let reply = |status: u16, body: Value| {
        (
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            [("cache-control", "no-store")],
            axum::Json(body),
        )
            .into_response()
    };
    let router = Router::new()
        .route(
            "/api/version",
            get(move |ConnectInfo(peer): ConnectInfo<Peer>| async move {
                if !peer_allowed(peer.uid) {
                    return reply(
                        403,
                        failure(
                            403,
                            "admin_required",
                            "Admin operations need the pool owner's login.",
                        )
                        .body(None),
                    );
                }
                let mut result = json!({"server":env!("CARGO_PKG_VERSION")});
                version_fields(&mut result);
                reply(200, json!({"ok":true,"result":result}))
            }),
        )
        .route(
            "/api/admin",
            post(
                move |State((root, controller)): State<(PathBuf, Option<String>)>,
                      ConnectInfo(peer): ConnectInfo<Peer>,
                      headers: HeaderMap,
                      uri: axum::http::Uri,
                      body: Body| async move {
                    if !peer_allowed(peer.uid) {
                        return reply(
                            403,
                            failure(
                                403,
                                "admin_required",
                                "Admin operations need the pool owner's login.",
                            )
                            .body(None),
                        );
                    }
                    let Ok(body) = axum::body::to_bytes(body, BODY_LIMIT + 1).await else {
                        return reply(
                            413,
                            failure(413, "too_large", "Admin request too large.").body(None),
                        );
                    };
                    if let Some(f) = shape(&headers, uri.query().is_some(), body.len()) {
                        return reply(f.status, f.body(None));
                    }
                    let claimed_user = headers
                        .get("x-ow-claimed-user")
                        .and_then(|v| v.to_str().ok())
                        .filter(|v| crate::common::identifier(v).is_ok())
                        .map(str::to_owned);
                    let answer = tokio::task::spawn_blocking(move || {
                        let ctx = Context {
                            root: &root,
                            actor: Actor::Host {
                                uid: peer.uid,
                                claimed_user,
                            },
                            // Read per request: `ow host up` may save it after this started.
                            controller: saved_controller(&root).or(controller),
                        };
                        handle(&ctx, &body)
                    })
                    .await
                    .unwrap_or_else(|_| {
                        (
                            500,
                            failure(500, "internal", "Internal error; see the controller log.")
                                .body(None),
                        )
                    });
                    reply(answer.0, answer.1)
                },
            ),
        )
        .with_state((root, fallback_controller));
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<Peer>(),
    )
    .await?;
    drop(lock);
    Ok(())
}

/// The request-shape checks both transports share (after the gate): JSON content type, the
/// dashboard request marker, no query string, size.
pub(crate) fn shape(headers: &axum::http::HeaderMap, query: bool, size: usize) -> Option<Failure> {
    if size > BODY_LIMIT {
        return Some(failure(413, "too_large", "Admin request too large."));
    }
    if query {
        return Some(invalid_request("query string not allowed"));
    }
    let json = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("application/json")
        });
    if !json {
        return Some(invalid_request("content-type must be application/json"));
    }
    if headers.get("x-ow-request").and_then(|v| v.to_str().ok()) != Some("dashboard") {
        return Some(failure(
            403,
            "same_origin_required",
            "Same-origin admin request required.",
        ));
    }
    None
}

#[cfg(test)]
#[path = "admin_tests.rs"]
mod tests;
