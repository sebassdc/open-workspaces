//! Node telemetry (docs/TELEMETRY.md). The agent encodes OTLP/HTTP JSON
//! (ExportMetricsServiceRequest, ExportLogsServiceRequest) and sends it over its node
//! channel once the controller offers `telemetry-v1`; the controller validates it, keeps
//! the latest summary and a short event ring per node, raises alerts and may forward
//! everything to an external OTLP endpoint. Settings live on the controller only.
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// A telemetry frame stays under the controller's 32 KiB session message limit.
pub(crate) const FRAME_LIMIT: usize = 28 * 1024;
/// Payloads waiting for the channel; the oldest is dropped (and counted) beyond this.
const QUEUE: usize = 64;
/// Per-box series are bounded: the largest boxes by RSS.
const BOXES: usize = 32;
/// Events kept per node on the controller.
const EVENTS: i64 = 200;
/// Series kept in a node's latest summary.
const SERIES: usize = 256;
/// Telemetry frames a controller session accepts per 3 s tick; more are ignored.
pub(crate) const FRAMES_PER_TICK: u32 = 20;
/// Operations whose duration is reported (captures and restores).
const TIMED: [&str; 7] = [
    "snapshot",
    "hibernate",
    "restore",
    "fork",
    "start",
    "capture-push",
    "capture-pull",
];

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
fn now() -> i64 {
    (unix_nanos() / 1_000_000_000) as i64
}

// ---------------------------------------------------------------------------------------
// Controller-held settings (one source; read on every acknowledgement).

/// Allowed keys and their defaults.
pub const KEYS: [(&str, &str); 5] = [
    ("telemetry.enabled", "true"),
    ("telemetry.interval_s", "15"),
    ("telemetry.forward_endpoint", ""),
    ("alerts.offline_after_s", "60"),
    ("alerts.disk_low_gib", "20"),
];

fn typed(key: &str, value: &str) -> Result<Value> {
    let number = |range: std::ops::RangeInclusive<u64>| -> Result<Value> {
        let n: u64 = value.parse().context("expected a whole number")?;
        ensure!(range.contains(&n), "{key} must be within {range:?}");
        Ok(json!(n))
    };
    match key {
        "telemetry.enabled" => match value {
            "true" => Ok(json!(true)),
            "false" => Ok(json!(false)),
            _ => bail!("telemetry.enabled is true or false"),
        },
        "telemetry.interval_s" => number(5..=300),
        "alerts.offline_after_s" => number(15..=86400),
        "alerts.disk_low_gib" => number(0..=100_000),
        "telemetry.forward_endpoint" => {
            if value.is_empty() {
                return Ok(json!(""));
            }
            let url = reqwest::Url::parse(value).context("invalid URL")?;
            // Credentials never live in configuration (secrets belong in a secret store).
            ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "forward endpoint is an http(s) OTLP base URL without credentials, query or fragment"
            );
            Ok(json!(value.trim_end_matches('/')))
        }
        _ => bail!(
            "unknown setting {key}; known: {}",
            KEYS.map(|(k, _)| k).join(", ")
        ),
    }
}

/// Current settings, typed, with defaults filled in, and their version.
pub fn settings(db: &Connection) -> Result<Value> {
    // One query: it also runs with every acknowledgement.
    let stored: BTreeMap<String, String> = db
        .prepare_cached("SELECT key,value FROM settings WHERE key LIKE 'telemetry.%' OR key LIKE 'alerts.%' OR key='config_version'")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = json!({});
    for (key, default) in KEYS {
        let value = stored.get(key).map_or(default, String::as_str);
        // A stored value a newer binary wrote and this one refuses falls back to the default.
        out[key] = typed(key, value).or_else(|_| typed(key, default))?;
    }
    out["version"] = json!(
        stored
            .get("config_version")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)
    );
    Ok(out)
}

/// `ow config set`: validated, versioned and recorded in `config_changes`.
pub fn set(root: &Path, key: &str, value: &str, actor: &str) -> Result<Value> {
    let typed = typed(key, value)?;
    let stored = typed
        .as_str()
        .map(str::to_owned)
        .unwrap_or(typed.to_string());
    let mut db = crate::nodes::db(root)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, stored],
    )?;
    tx.execute("INSERT INTO settings(key,value) VALUES('config_version','1') ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1",[])?;
    tx.execute(
        "INSERT INTO config_changes(key,value,unix_s,actor) VALUES(?1,?2,?3,?4)",
        params![key, stored, now(), actor],
    )?;
    tx.commit()?;
    settings(&db)
}

/// What the acknowledgement carries to agents.
pub(crate) fn offer(db: &Connection) -> Value {
    settings(db)
        .map(|s| json!({"enabled":s["telemetry.enabled"],"interval_s":s["telemetry.interval_s"],"version":s["version"]}))
        .unwrap_or(json!({"enabled":true,"interval_s":15}))
}

// ---------------------------------------------------------------------------------------
// OTLP JSON encoding.

pub(crate) enum Num {
    Int(i64),
    Double(f64),
}
pub(crate) struct Metric {
    pub name: &'static str,
    pub unit: &'static str,
    /// Monotonic cumulative sum; otherwise a gauge.
    pub sum: bool,
    pub points: Vec<(Vec<(&'static str, String)>, Num)>,
}
fn metric(name: &'static str, unit: &'static str, sum: bool, value: Num) -> Metric {
    Metric {
        name,
        unit,
        sum,
        points: vec![(vec![], value)],
    }
}
fn attributes(attrs: &[(&str, String)]) -> Value {
    json!(
        attrs
            .iter()
            .map(|(k, v)| json!({"key":k,"value":{"stringValue":v}}))
            .collect::<Vec<_>>()
    )
}
fn resource(node: &str) -> Value {
    json!({"attributes":attributes(&[
        ("service.name","ow-node-agent".into()),
        ("service.version",env!("CARGO_PKG_VERSION").into()),
        ("ow.node.id",node.into()),
        ("host.arch",std::env::consts::ARCH.into()),
        ("os.type",std::env::consts::OS.into()),
    ])})
}
fn encode_metric(m: &Metric, start: u128, now: u128) -> Value {
    let points: Vec<Value> = m
        .points
        .iter()
        .map(|(attrs, value)| {
            let mut p = json!({"timeUnixNano":now.to_string(),"attributes":attributes(attrs)});
            match value {
                // int64 is a decimal string in OTLP JSON (proto3 mapping).
                Num::Int(n) => p["asInt"] = json!(n.to_string()),
                Num::Double(d) => p["asDouble"] = json!(d),
            }
            if m.sum {
                p["startTimeUnixNano"] = json!(start.to_string());
            }
            p
        })
        .collect();
    if m.sum {
        json!({"name":m.name,"unit":m.unit,"sum":{"aggregationTemporality":2,"isMonotonic":true,"dataPoints":points}})
    } else {
        json!({"name":m.name,"unit":m.unit,"gauge":{"dataPoints":points}})
    }
}
fn frame(signal: &str, body: Value) -> String {
    json!({"type":"otlp","signal":signal,"body":body}).to_string()
}
/// Metrics as channel frames of at most FRAME_LIMIT bytes each; a single metric larger
/// than that is dropped and counted.
pub(crate) fn encode_metrics(
    node: &str,
    metrics: &[Metric],
    start: u128,
    now: u128,
) -> (Vec<String>, u64) {
    let wrap = |batch: Vec<Value>| {
        frame(
            "metrics",
            json!({"resourceMetrics":[{"resource":resource(node),"scopeMetrics":[{"scope":{"name":"ow","version":env!("CARGO_PKG_VERSION")},"metrics":batch}]}]}),
        )
    };
    let overhead = wrap(vec![]).len();
    let (mut frames, mut dropped, mut batch, mut size) = (vec![], 0, vec![], overhead);
    for m in metrics.iter().filter(|m| !m.points.is_empty()) {
        let encoded = encode_metric(m, start, now);
        let len = encoded.to_string().len() + 1;
        if overhead + len > FRAME_LIMIT {
            dropped += 1;
            continue;
        }
        if size + len > FRAME_LIMIT {
            frames.push(wrap(std::mem::take(&mut batch)));
            size = overhead;
        }
        batch.push(encoded);
        size += len;
    }
    if !batch.is_empty() {
        frames.push(wrap(batch));
    }
    (frames, dropped)
}
fn encode_event(node: &str, severity: &str, name: &str, attrs: &[(&str, String)]) -> String {
    let number = match severity {
        "ERROR" => 17,
        "WARN" => 13,
        _ => 9,
    };
    let mut attrs = attrs.to_vec();
    attrs.push(("event.name", name.to_owned()));
    let now = unix_nanos().to_string();
    frame(
        "logs",
        json!({"resourceLogs":[{"resource":resource(node),"scopeLogs":[{"scope":{"name":"ow"},"logRecords":[{
            "timeUnixNano":now,"observedTimeUnixNano":now,"severityNumber":number,"severityText":severity,
            "body":{"stringValue":name},"attributes":attributes(&attrs)}]}]}]}),
    )
}

// ---------------------------------------------------------------------------------------
// Agent side.

#[derive(Default)]
struct Channel {
    connects: u64,
    disconnects: BTreeMap<&'static str, u64>,
    connect_ms: Option<u64>,
    /// Since when the channel is down, why, and how many attempts failed since.
    down: Option<(Instant, &'static str, u64)>,
}

/// One agent's telemetry state: the bounded queue survives reconnects.
pub(crate) struct Agent {
    node: String,
    /// Secret per runner: hashed box IDs cannot be matched against guessed names.
    salt: String,
    started: u128,
    queue: Mutex<VecDeque<String>>,
    dropped: AtomicU64,
    /// The current channel's controller offered `telemetry-v1`.
    pub negotiated: AtomicBool,
    enabled: AtomicBool,
    interval_s: AtomicU64,
    channel: Mutex<Channel>,
    /// Per operation: last duration (ms), successes, failures.
    ops: Mutex<BTreeMap<String, (u64, u64, u64)>>,
}

/// Why a channel attempt ended, as a fixed class (never the raw error text).
pub(crate) fn reason(error: &anyhow::Error) -> &'static str {
    let text = format!("{error:#}").to_ascii_lowercase();
    if text.contains("no frame from the controller") {
        "silent"
    } else if text.contains("dns") || text.contains("resolve") {
        "dns"
    } else if text.contains("controller closed the channel") {
        "refused"
    } else if text.contains("401") || text.contains("unauthorized") {
        "auth"
    } else if text.contains("worker") {
        "worker"
    } else if text.contains("tls") || text.contains("certificate") || text.contains("handshake") {
        "tls"
    } else if text.contains("connect") || text.contains("refused") || text.contains("timed out") {
        "connect"
    } else {
        "closed"
    }
}

impl Agent {
    pub fn new(node: &str, salt: String) -> Arc<Self> {
        let agent = Arc::new(Agent {
            node: node.to_owned(),
            salt,
            started: unix_nanos(),
            queue: Mutex::new(VecDeque::new()),
            dropped: AtomicU64::new(0),
            negotiated: AtomicBool::new(false),
            enabled: AtomicBool::new(true),
            interval_s: AtomicU64::new(15),
            channel: Mutex::new(Channel::default()),
            ops: Mutex::new(BTreeMap::new()),
        });
        agent.event(
            "INFO",
            "ow.node.agent.started",
            &[("ow.version", env!("CARGO_PKG_VERSION").into())],
        );
        agent
    }
    fn push(&self, frame: String) {
        if !self.enabled.load(Ordering::SeqCst) {
            return;
        }
        let mut queue = self.queue.lock().unwrap();
        if queue.len() >= QUEUE {
            queue.pop_front();
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
        queue.push_back(frame);
    }
    fn event(&self, severity: &str, name: &str, attrs: &[(&str, String)]) {
        self.push(encode_event(&self.node, severity, name, attrs));
    }
    /// The controller's acknowledgement: whether it takes telemetry, and its settings.
    pub fn acknowledged(&self, ack: &Value, connected_at: Instant) {
        let offered = ack["extensions"]
            .as_array()
            .is_some_and(|a| a.iter().any(|x| x == "telemetry-v1"));
        self.negotiated.store(offered, Ordering::SeqCst);
        if offered {
            let enabled = ack["telemetry"]["enabled"].as_bool().unwrap_or(true);
            if !enabled {
                self.queue.lock().unwrap().clear();
            }
            self.enabled.store(enabled, Ordering::SeqCst);
            if let Some(i) = ack["telemetry"]["interval_s"].as_u64() {
                self.interval_s.store(i.clamp(5, 300), Ordering::SeqCst);
            }
        }
        let mut channel = self.channel.lock().unwrap();
        // The first acknowledgement of a connection counts it as connected.
        if channel.connect_ms.is_some() && channel.down.is_none() {
            return;
        }
        channel.connects += 1;
        channel.connect_ms = Some(connected_at.elapsed().as_millis() as u64);
        if let Some((since, reason, attempts)) = channel.down.take() {
            drop(channel);
            self.event(
                "INFO",
                "ow.node.channel.reconnected",
                &[
                    ("reason", reason.into()),
                    ("attempts", attempts.to_string()),
                    ("downtime_s", since.elapsed().as_secs().to_string()),
                ],
            );
        }
    }
    /// A channel attempt ended. Only the first failure after being up is an event.
    pub fn disconnected(&self, error: &anyhow::Error) {
        self.negotiated.store(false, Ordering::SeqCst);
        let reason = reason(error);
        let mut channel = self.channel.lock().unwrap();
        match &mut channel.down {
            Some((_, _, attempts)) => *attempts += 1,
            None => {
                channel.down = Some((Instant::now(), reason, 1));
                *channel.disconnects.entry(reason).or_default() += 1;
                drop(channel);
                self.event(
                    "WARN",
                    "ow.node.channel.disconnected",
                    &[("reason", reason.into())],
                );
            }
        }
    }
    pub fn operation(&self, op: &str, elapsed: Duration, ok: bool) {
        if !TIMED.contains(&op) {
            return;
        }
        let ms = elapsed.as_millis() as u64;
        {
            let mut ops = self.ops.lock().unwrap();
            let entry = ops.entry(op.to_owned()).or_default();
            entry.0 = ms;
            if ok { entry.1 += 1 } else { entry.2 += 1 }
        }
        self.event(
            if ok { "INFO" } else { "WARN" },
            "ow.node.operation",
            &[
                ("op", op.into()),
                ("outcome", if ok { "ok" } else { "error" }.into()),
                ("duration_ms", ms.to_string()),
            ],
        );
    }
    /// The next frame to send on this channel, if the controller takes telemetry.
    pub fn next(&self) -> Option<String> {
        if !self.negotiated.load(Ordering::SeqCst) || !self.enabled.load(Ordering::SeqCst) {
            return None;
        }
        self.queue.lock().unwrap().pop_front()
    }
    /// Times one job; reported when the timer drops, so a job that fails with an error
    /// counts as a failure too.
    pub fn timer(&self, op: &str) -> OpTimer<'_> {
        OpTimer {
            agent: self,
            op: op.to_owned(),
            started: Instant::now(),
            ok: false,
        }
    }
    fn agent_metrics(&self) -> Vec<Metric> {
        let channel = self.channel.lock().unwrap();
        let mut metrics = vec![
            metric(
                "ow.node.channel.connects",
                "{connect}",
                true,
                Num::Int(channel.connects as i64),
            ),
            Metric {
                name: "ow.node.channel.disconnects",
                unit: "{disconnect}",
                sum: true,
                points: channel
                    .disconnects
                    .iter()
                    .map(|(r, n)| (vec![("reason", r.to_string())], Num::Int(*n as i64)))
                    .collect(),
            },
            metric(
                "ow.telemetry.dropped",
                "{payload}",
                true,
                Num::Int(self.dropped.load(Ordering::SeqCst) as i64),
            ),
        ];
        if let Some(ms) = channel.connect_ms {
            metrics.push(metric(
                "ow.node.channel.connect.duration",
                "ms",
                false,
                Num::Int(ms as i64),
            ));
        }
        let ops = self.ops.lock().unwrap();
        metrics.push(Metric {
            name: "ow.node.operation.duration",
            unit: "ms",
            sum: false,
            points: ops
                .iter()
                .map(|(op, (ms, _, _))| (vec![("op", op.clone())], Num::Int(*ms as i64)))
                .collect(),
        });
        metrics.push(Metric {
            name: "ow.node.operation.count",
            unit: "{operation}",
            sum: true,
            points: ops
                .iter()
                .flat_map(|(op, (_, ok, failed))| {
                    [("ok", *ok), ("error", *failed)].map(|(outcome, n)| {
                        (
                            vec![("op", op.clone()), ("outcome", outcome.into())],
                            Num::Int(n as i64),
                        )
                    })
                })
                .collect(),
        });
        metrics
    }
}

pub(crate) struct OpTimer<'a> {
    agent: &'a Agent,
    op: String,
    started: Instant,
    pub ok: bool,
}
impl Drop for OpTimer<'_> {
    fn drop(&mut self) {
        self.agent
            .operation(&self.op, self.started.elapsed(), self.ok);
    }
}

/// Fields of `/proc/stat`'s first line: (busy, total) jiffies.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn cpu_times(stat: &str) -> Option<(u64, u64)> {
    let fields: Vec<u64> = stat
        .lines()
        .next()?
        .strip_prefix("cpu ")?
        .split_whitespace()
        .take(8)
        .map(|f| f.parse().ok())
        .collect::<Option<_>>()?;
    let total: u64 = fields.iter().sum();
    // idle and iowait
    let idle = fields.get(3)? + fields.get(4).copied().unwrap_or(0);
    Some((total - idle, total))
}
/// A `kB` field of `/proc/meminfo` or `/proc/PID/status`, in bytes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn kib_field(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
        .map(|kib| kib * 1024)
}
/// The runner's telemetry salt (`telemetry-salt` in its data directory, private),
/// made once; a fresh one per process if it cannot be stored.
pub(crate) fn salt(root: &Path) -> String {
    let path = root.join("telemetry-salt");
    if let Some(salt) = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<String>(&b).ok())
        .filter(|s| s.len() == 64)
    {
        return salt;
    }
    let salt = crate::common::nonce().unwrap_or_default();
    let _ = crate::nodes::write_private(&path, &json!(salt));
    salt
}
/// Box IDs are opaque worker IDs; a legacy ID that may be a user-chosen name is sent
/// as a salted hash.
pub(crate) fn box_id(physical: &str, salt: &str) -> String {
    let opaque = physical.len() == 26
        && physical.starts_with("m-")
        && physical[2..].bytes().all(|b| b.is_ascii_hexdigit());
    if opaque {
        physical.to_owned()
    } else {
        use sha2::Digest;
        let hash = sha2::Sha256::digest(format!("{salt}:{physical}").as_bytes());
        format!(
            "h-{}",
            hash[..8]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )
    }
}
/// Per-box memory from the worker's `stats` reply, the largest BOXES boxes by RSS.
pub(crate) fn box_metrics(stats: &Value, salt: &str) -> Vec<Metric> {
    let mut boxes: Vec<(String, u64, u64)> = stats["running"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(id, v)| {
            (
                box_id(id, salt),
                v["rss_kib"].as_u64().unwrap_or(0) * 1024,
                v["pss_kib"].as_u64().unwrap_or(0) * 1024,
            )
        })
        .collect();
    let running = boxes.len() as i64;
    boxes.sort_by(|a, b| b.1.cmp(&a.1));
    boxes.truncate(BOXES);
    let series = |pick: fn(&(String, u64, u64)) -> u64| {
        boxes
            .iter()
            .map(|b| (vec![("ow.box.id", b.0.clone())], Num::Int(pick(b) as i64)))
            .collect()
    };
    vec![
        metric("ow.box.running", "{box}", false, Num::Int(running)),
        Metric {
            name: "ow.box.memory.rss",
            unit: "By",
            sum: false,
            points: series(|b| b.1),
        },
        Metric {
            name: "ow.box.memory.pss",
            unit: "By",
            sum: false,
            points: series(|b| b.2),
        },
    ]
}

/// Host metrics available on every platform: load and the data root's filesystem.
fn host_metrics(root: &Path) -> Vec<Metric> {
    let mut metrics = vec![];
    let mut load = [0f64; 3];
    if unsafe { libc::getloadavg(load.as_mut_ptr(), 3) } == 3 {
        for (name, value) in [
            ("system.cpu.load_average.1m", load[0]),
            ("system.cpu.load_average.5m", load[1]),
            ("system.cpu.load_average.15m", load[2]),
        ] {
            metrics.push(metric(name, "{thread}", false, Num::Double(value)));
        }
    }
    if let Ok(path) = std::ffi::CString::new(root.as_os_str().as_encoded_bytes()) {
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut s) } == 0 {
            let block = s.f_frsize as u64;
            let (total, free) = (s.f_blocks as u64 * block, s.f_bavail as u64 * block);
            metrics.push(Metric {
                name: "system.filesystem.usage",
                unit: "By",
                sum: false,
                points: vec![
                    (
                        vec![("system.filesystem.state", "used".into())],
                        Num::Int(total.saturating_sub(free) as i64),
                    ),
                    (
                        vec![("system.filesystem.state", "free".into())],
                        Num::Int(free as i64),
                    ),
                ],
            });
        }
    }
    metrics
}

/// CPU, memory and process memory from `/proc`.
#[cfg(target_os = "linux")]
fn proc_metrics(prev_cpu: &mut Option<(u64, u64)>, worker_pid: Option<u64>) -> Vec<Metric> {
    let mut metrics = vec![];
    if let Some((busy, total)) = std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| cpu_times(&s))
    {
        if let Some((b0, t0)) = prev_cpu.replace((busy, total))
            && total > t0
        {
            metrics.push(metric(
                "system.cpu.utilization",
                "1",
                false,
                Num::Double(busy.saturating_sub(b0) as f64 / (total - t0) as f64),
            ));
        }
    }
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo")
        && let (Some(total), Some(available)) = (
            kib_field(&meminfo, "MemTotal"),
            kib_field(&meminfo, "MemAvailable"),
        )
    {
        metrics.push(metric(
            "system.memory.limit",
            "By",
            false,
            Num::Int(total as i64),
        ));
        metrics.push(Metric {
            name: "system.memory.usage",
            unit: "By",
            sum: false,
            points: vec![
                (
                    vec![("system.memory.state", "used".into())],
                    Num::Int(total.saturating_sub(available) as i64),
                ),
                (
                    vec![("system.memory.state", "available".into())],
                    Num::Int(available as i64),
                ),
            ],
        });
    }
    let mut processes = vec![];
    for (role, path) in [
        ("agent", "/proc/self/status".to_owned()),
        (
            "worker",
            worker_pid
                .map(|p| format!("/proc/{p}/status"))
                .unwrap_or_default(),
        ),
    ] {
        if let Some(rss) = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| kib_field(&s, "VmRSS"))
        {
            processes.push((vec![("ow.process", role.into())], Num::Int(rss as i64)));
        }
    }
    metrics.push(Metric {
        name: "process.memory.usage",
        unit: "By",
        sum: false,
        points: processes,
    });
    metrics
}

/// The agent's collector thread: one sample every `interval_s` while the controller
/// takes telemetry. `beat_age` is how old the latest worker status is (seconds).
pub(crate) fn collector(
    agent: Arc<Agent>,
    root: std::path::PathBuf,
    worker: Arc<Mutex<Option<Value>>>,
    jobs: Arc<std::sync::atomic::AtomicUsize>,
    beat_age: impl Fn() -> Option<u64> + Send + 'static,
) {
    std::thread::spawn(move || {
        #[cfg(target_os = "linux")]
        let mut prev_cpu = None;
        let mut last = Instant::now();
        loop {
            // Short sleeps, so a new interval from the controller applies at once.
            std::thread::sleep(Duration::from_secs(1));
            if last.elapsed().as_secs() < agent.interval_s.load(Ordering::SeqCst) {
                continue;
            }
            last = Instant::now();
            if !agent.negotiated.load(Ordering::SeqCst) || !agent.enabled.load(Ordering::SeqCst) {
                continue;
            }
            let status = worker.lock().unwrap().clone();
            let mut metrics = host_metrics(&root);
            #[cfg(target_os = "linux")]
            metrics.extend(proc_metrics(
                &mut prev_cpu,
                status.as_ref().and_then(|s| s["worker_pid"].as_u64()),
            ));
            metrics.extend(agent.agent_metrics());
            metrics.push(metric(
                "ow.node.jobs.active",
                "{job}",
                false,
                Num::Int(jobs.load(Ordering::SeqCst) as i64),
            ));
            if let Some(age) = beat_age() {
                metrics.push(metric(
                    "ow.node.worker.status.age",
                    "s",
                    false,
                    Num::Int(age as i64),
                ));
            }
            // A busy worker (a long capture) answers later; box metrics then skip a round.
            if status.is_some()
                && let Ok((_, stats)) = crate::wire::connect_waiting(
                    &root,
                    &json!({"op":"stats"}),
                    Duration::from_secs(5),
                )
            {
                metrics.extend(box_metrics(&stats, &agent.salt));
            }
            let (frames, dropped) =
                encode_metrics(&agent.node, &metrics, agent.started, unix_nanos());
            agent.dropped.fetch_add(dropped, Ordering::SeqCst);
            frames.into_iter().for_each(|f| agent.push(f));
        }
    });
}

// ---------------------------------------------------------------------------------------
// Controller side: receive, store, alert, forward.

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'_')
}
fn valid_value(value: &str) -> bool {
    value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
/// Validated attributes of one point or record (ow.node.id is never taken from a node).
fn checked_attributes(attrs: &Value) -> Result<Vec<(String, String)>> {
    let attrs = attrs.as_array().map(Vec::as_slice).unwrap_or_default();
    ensure!(attrs.len() <= 8, "too many attributes");
    let mut out = vec![];
    for a in attrs {
        let key = a["key"].as_str().context("attribute key")?;
        let value = &a["value"];
        let value = value["stringValue"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| value["intValue"].as_str().map(str::to_owned))
            .or_else(|| value["intValue"].as_i64().map(|n| n.to_string()))
            .context("attribute value")?;
        ensure!(
            valid_name(key) && valid_value(&value),
            "invalid attribute {key}"
        );
        if key != "ow.node.id" {
            out.push((key.to_owned(), value));
        }
    }
    Ok(out)
}
fn point_value(p: &Value) -> Option<f64> {
    p["asDouble"]
        .as_f64()
        .or_else(|| p["asInt"].as_str().and_then(|s| s.parse().ok()))
        .or_else(|| p["asInt"].as_f64())
}
fn series_key(name: &str, attrs: &[(String, String)]) -> String {
    if attrs.is_empty() {
        return name.to_owned();
    }
    let attrs: Vec<String> = attrs.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!("{name}{{{}}}", attrs.join(","))
}
/// The resource of a forwarded request: the runner's validated attributes and the
/// authenticated node ID.
fn checked_resource(resource: &Value, node: &str) -> Result<Value> {
    let mut attrs = checked_attributes(&resource["attributes"])?;
    attrs.push(("ow.node.id".into(), node.into()));
    Ok(
        json!({"attributes":attrs.iter().map(|(k,v)| json!({"key":k,"value":{"stringValue":v}})).collect::<Vec<_>>()}),
    )
}
fn nanos(value: &Value) -> Result<Value> {
    let text = value.as_str().context("timestamp")?;
    ensure!(
        !text.is_empty() && text.len() <= 20 && text.bytes().all(|b| b.is_ascii_digit()),
        "timestamp"
    );
    Ok(json!(text))
}
fn valid_unit(unit: &str) -> bool {
    unit.len() <= 16
        && unit
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"{}/._".contains(&b))
}
fn attributes_json(attrs: &[(String, String)]) -> Value {
    json!(
        attrs
            .iter()
            .map(|(k, v)| json!({"key":k,"value":{"stringValue":v}}))
            .collect::<Vec<_>>()
    )
}

/// What a telemetry frame means for the controller.
pub(crate) enum Received {
    /// Latest values, keyed `name{attr=value,...}`.
    Metrics(BTreeMap<String, f64>),
    /// (when it happened, severity, name, attributes) per record.
    Events(Vec<(i64, String, String, Value)>),
}
/// Validates a node's telemetry frame. Returns what to store and the request to forward,
/// rebuilt from the validated fields only, with the authenticated node ID.
pub(crate) fn receive(node: &str, frame: &Value) -> Result<(Received, &'static str, Value)> {
    let body = &frame["body"];
    match frame["signal"].as_str() {
        Some("metrics") => {
            let mut summary = BTreeMap::new();
            let resources = body["resourceMetrics"]
                .as_array()
                .context("resourceMetrics")?;
            ensure!(resources.len() <= 4, "too many resources");
            let mut out_resources = vec![];
            for r in resources {
                let scopes = r["scopeMetrics"].as_array().context("scopeMetrics")?;
                ensure!(scopes.len() <= 4, "too many scopes");
                let mut out_metrics = vec![];
                for s in scopes {
                    let metrics = s["metrics"].as_array().context("metrics")?;
                    ensure!(metrics.len() <= 128, "too many metrics");
                    for m in metrics {
                        let name = m["name"].as_str().context("metric name")?;
                        ensure!(
                            valid_name(name)
                                && ["system.", "process.", "ow."]
                                    .iter()
                                    .any(|p| name.starts_with(p)),
                            "metric name not accepted"
                        );
                        let unit = m["unit"].as_str().unwrap_or("");
                        ensure!(valid_unit(unit), "metric unit");
                        let sum = m["sum"].is_object();
                        let data = if sum { &m["sum"] } else { &m["gauge"] };
                        let points = data["dataPoints"].as_array().context("dataPoints")?;
                        ensure!(points.len() <= 64, "too many points");
                        let mut out_points = vec![];
                        for p in points {
                            let attrs = checked_attributes(&p["attributes"])?;
                            let value = point_value(p).context("point value")?;
                            let mut point = json!({"timeUnixNano":nanos(&p["timeUnixNano"])?,"attributes":attributes_json(&attrs)});
                            if p["asDouble"].is_number() {
                                point["asDouble"] = json!(value);
                            } else {
                                point["asInt"] = json!((value as i64).to_string());
                            }
                            if sum {
                                point["startTimeUnixNano"] = nanos(&p["startTimeUnixNano"])?;
                            }
                            out_points.push(point);
                            if summary.len() < SERIES {
                                summary.insert(series_key(name, &attrs), value);
                            }
                        }
                        out_metrics.push(if sum {
                            json!({"name":name,"unit":unit,"sum":{"aggregationTemporality":2,"isMonotonic":true,"dataPoints":out_points}})
                        } else {
                            json!({"name":name,"unit":unit,"gauge":{"dataPoints":out_points}})
                        });
                    }
                }
                out_resources.push(json!({"resource":checked_resource(&r["resource"],node)?,
                    "scopeMetrics":[{"scope":{"name":"ow"},"metrics":out_metrics}]}));
            }
            Ok((
                Received::Metrics(summary),
                "metrics",
                json!({"resourceMetrics":out_resources}),
            ))
        }
        Some("logs") => {
            let mut events = vec![];
            let resources = body["resourceLogs"].as_array().context("resourceLogs")?;
            ensure!(resources.len() <= 4, "too many resources");
            let mut out_resources = vec![];
            for r in resources {
                let mut out_records = vec![];
                for s in r["scopeLogs"].as_array().context("scopeLogs")? {
                    let records = s["logRecords"].as_array().context("logRecords")?;
                    ensure!(events.len() + records.len() <= 32, "too many records");
                    for record in records {
                        let attrs = checked_attributes(&record["attributes"])?;
                        let severity = record["severityText"].as_str().unwrap_or("INFO");
                        let number = match severity {
                            "INFO" => 9,
                            "WARN" => 13,
                            "ERROR" => 17,
                            _ => bail!("severity"),
                        };
                        let name = attrs
                            .iter()
                            .find(|(k, _)| k == "event.name")
                            .map(|(_, v)| v.clone())
                            .context("event.name")?;
                        ensure!(valid_name(&name), "event name");
                        let time = nanos(&record["timeUnixNano"])?;
                        out_records.push(json!({"timeUnixNano":time,"observedTimeUnixNano":time,"severityNumber":number,
                            "severityText":severity,"body":{"stringValue":name},"attributes":attributes_json(&attrs)}));
                        let attrs: serde_json::Map<String, Value> = attrs
                            .into_iter()
                            .filter(|(k, _)| k != "event.name")
                            .map(|(k, v)| (k, json!(v)))
                            .collect();
                        events.push((
                            event_time(time.as_str().unwrap_or_default(), now()),
                            severity.to_owned(),
                            name,
                            Value::Object(attrs),
                        ));
                    }
                }
                out_resources.push(json!({"resource":checked_resource(&r["resource"],node)?,
                    "scopeLogs":[{"scope":{"name":"ow"},"logRecords":out_records}]}));
            }
            Ok((
                Received::Events(events),
                "logs",
                json!({"resourceLogs":out_resources}),
            ))
        }
        _ => bail!("unknown telemetry signal"),
    }
}

/// When a runner's event happened, in Unix seconds: its own timestamp, so an event
/// queued during an outage keeps its time. A clock far off (over 5 min ahead or a week
/// behind) falls back to the time it arrived.
fn event_time(nanos: &str, now: i64) -> i64 {
    nanos
        .parse::<u128>()
        .ok()
        .map(|n| (n / 1_000_000_000) as i64)
        .filter(|at| *at <= now + 300 && *at >= now - 7 * 86400)
        .unwrap_or(now)
}
/// Appends an event that happens now to a node's capped ring.
pub(crate) fn record_event(
    db: &Connection,
    node: &str,
    severity: &str,
    name: &str,
    attrs: &Value,
) -> Result<()> {
    record_event_at(db, node, now(), severity, name, attrs)
}
/// Appends an event to a node's capped ring (the newest EVENTS arrivals stay).
fn record_event_at(
    db: &Connection,
    node: &str,
    at: i64,
    severity: &str,
    name: &str,
    attrs: &Value,
) -> Result<()> {
    db.execute(
        "INSERT INTO node_events(node,unix_s,severity,name,attrs) VALUES(?1,?2,?3,?4,?5)",
        params![node, at, severity, name, attrs.to_string()],
    )?;
    db.execute(
        "DELETE FROM node_events WHERE node=?1 AND id<=(SELECT id FROM node_events WHERE node=?1 ORDER BY id DESC LIMIT 1 OFFSET ?2)",
        params![node, EVENTS],
    )?;
    Ok(())
}
/// A controller session ended. With `seen` (the session stored a heartbeat then) it is
/// the runner's new last-seen time and an event; a session that never stored one only
/// updates the close reason, so a runner that keeps failing still goes offline.
pub(crate) fn session_closed(
    db: &Connection,
    node: &str,
    seen: Option<i64>,
    reason: &str,
) -> Result<()> {
    match seen {
        Some(seen) => {
            db.execute(
                "INSERT INTO node_seen(node,unix_s,reason,closed_unix_s) VALUES(?1,?2,?3,?4) ON CONFLICT(node) DO UPDATE SET unix_s=excluded.unix_s,reason=excluded.reason,closed_unix_s=excluded.closed_unix_s",
                params![node, seen, reason, now()],
            )?;
            record_event(
                db,
                node,
                "WARN",
                "ow.node.session.closed",
                &json!({"reason":reason,"last_seen_unix_s":seen}),
            )
        }
        None => {
            db.execute(
                "INSERT INTO node_seen(node,unix_s,reason,closed_unix_s) VALUES(?1,0,?2,?3) ON CONFLICT(node) DO UPDATE SET reason=excluded.reason,closed_unix_s=excluded.closed_unix_s",
                params![node, reason, now()],
            )?;
            Ok(())
        }
    }
}
/// A fixed class for why the controller ended a session (its log line has the detail).
pub(crate) fn session_reason(reason: &str) -> &'static str {
    let classes = [
        ("replaced", "superseded"),
        ("no heartbeat", "timeout"),
        ("not sent within", "ping"),
        ("invalid", "invalid"),
        ("heartbeat refused", "refused"),
        ("credential", "auth"),
        ("heartbeat not recorded", "store"),
        ("health store", "store"),
    ];
    classes
        .iter()
        .find(|(text, _)| reason.contains(text))
        .map_or("closed", |(_, class)| class)
}
/// Recorded at controller start: offline grace never starts before it.
pub(crate) fn controller_started(db: &Connection) -> Result<()> {
    db.execute(
        "INSERT INTO settings(key,value) VALUES('controller_started_unix_s',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [now().to_string()],
    )?;
    Ok(())
}
pub(crate) fn store(root: &Path, node: &str, received: &Received) -> Result<()> {
    let mut db = crate::nodes::db(root)?;
    match received {
        // Frames of one collection round arrive together and merge; a later round replaces
        // the summary, so a stopped box's series disappear. At most SERIES are kept.
        Received::Metrics(summary) => {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let now = now();
            let mut merged: BTreeMap<String, f64> = tx
                .query_row(
                    "SELECT summary FROM node_telemetry WHERE node=?1 AND unix_s>=?2",
                    params![node, now - 2],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            for (key, value) in summary {
                if merged.len() < SERIES || merged.contains_key(key) {
                    merged.insert(key.clone(), *value);
                }
            }
            tx.execute(
                "INSERT INTO node_telemetry(node,summary,unix_s) VALUES(?1,?2,?3) ON CONFLICT(node) DO UPDATE SET summary=excluded.summary,unix_s=excluded.unix_s",
                params![node, json!(merged).to_string(), now],
            )?;
            tx.commit()?;
        }
        Received::Events(events) => {
            for (at, severity, name, attrs) in events {
                record_event_at(&db, node, *at, severity, name, attrs)?;
            }
        }
    }
    Ok(())
}

/// Node payloads and controller metrics waiting for the forwarder.
static FORWARD: std::sync::OnceLock<tokio::sync::mpsc::Sender<(&'static str, Value)>> =
    std::sync::OnceLock::new();
static FORWARD_DROPPED: AtomicU64 = AtomicU64::new(0);
pub(crate) fn forward(signal: &'static str, body: Value) {
    if let Some(tx) = FORWARD.get()
        && tx.try_send((signal, body)).is_err()
    {
        FORWARD_DROPPED.fetch_add(1, Ordering::SeqCst);
    }
}

/// Every alert kind `alerts` raises.
const ALERTS: [&str; 2] = ["node_offline", "disk_low"];
fn gib(n: u64) -> u64 {
    n * 1024 * 1024 * 1024
}
/// Active alerts for an inventory (`nodes::inventory`), given when each offline node was
/// last seen. Nodes that never connected (an unused invitation) and revoked ones are skipped.
pub(crate) fn alerts(
    inventory: &Value,
    last_seen: &BTreeMap<String, (i64, String)>,
    settings: &Value,
    now: i64,
) -> Vec<Value> {
    let offline_after = settings["alerts.offline_after_s"].as_i64().unwrap_or(60);
    let disk_low = gib(settings["alerts.disk_low_gib"].as_u64().unwrap_or(20));
    let mut out = vec![];
    for n in inventory.as_array().into_iter().flatten() {
        let id = n["id"].as_str().unwrap_or_default();
        if n["revoked"] == true || n["capabilities"]["protocol"].is_null() {
            continue;
        }
        if n["online"] != true {
            let (seen, reason) = last_seen.get(id).cloned().unwrap_or((0, "unknown".into()));
            if seen == 0 || now - seen >= offline_after {
                out.push(json!({"kind":"node_offline","node":id,"severity":"critical",
                    "since_unix_s":(seen>0).then_some(seen),"reason":reason,
                    "message":format!("runner {id} has not reached the controller since {}", if seen>0 {format!("{} s ago", now-seen)} else {"before the controller started".into()})}));
            }
            continue;
        }
        if let Some(free) = n["capabilities"]["free_disk_bytes"].as_u64()
            && free < disk_low
        {
            out.push(json!({"kind":"disk_low","node":id,"severity":"warning","free_bytes":free,
                "message":format!("runner {id} has {} GiB free on its data volume", free / gib(1))}));
        }
    }
    out
}

/// Fleet health: alerts, and per node its latest telemetry and recent events. Shared by
/// `ow health`, `ow nodes`, the dashboard and the controller's alert loop.
pub fn health(root: &Path) -> Result<Value> {
    let inventory = crate::nodes::inventory(root)?;
    let db = crate::nodes::db(root)?;
    let settings = settings(&db)?;
    let started: i64 = db
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM settings WHERE key='controller_started_unix_s'",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let mut last_seen = BTreeMap::new();
    let mut nodes = vec![];
    for n in inventory.as_array().into_iter().flatten() {
        let id = n["id"].as_str().unwrap_or_default().to_owned();
        // Offline grace runs from the last stored heartbeat, but never from before
        // this controller started (runners need a moment to reconnect after a restart).
        let closed: Option<(i64, String)> = db
            .query_row(
                "SELECT unix_s,reason FROM node_seen WHERE node=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (seen, reason) = closed.unwrap_or((0, "unknown".into()));
        last_seen.insert(id.clone(), (seen.max(started), reason));
        let telemetry: Option<(String, i64)> = db
            .query_row(
                "SELECT summary,unix_s FROM node_telemetry WHERE node=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let mut stmt = db.prepare_cached(
            "SELECT unix_s,severity,name,attrs FROM node_events WHERE node=?1 ORDER BY unix_s DESC,id DESC LIMIT 10",
        )?;
        let events = stmt
            .query_map([&id], |r| {
                Ok(json!({"unix_s":r.get::<_,i64>(0)?,"severity":r.get::<_,String>(1)?,"name":r.get::<_,String>(2)?,
                    "attributes":serde_json::from_str::<Value>(&r.get::<_,String>(3)?).unwrap_or_default()}))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // The latest heartbeat the controller stored: live now, or before the last close.
        let heard = n["last_seen_unix_s"].as_i64().unwrap_or(0).max(seen);
        nodes.push(json!({"id":id,"online":n["online"],"revoked":n["revoked"],"last_seen_unix_s":n["last_seen_unix_s"],
            "last_heartbeat_unix_s":(heard>0).then_some(heard),
            "telemetry_v1":n["capabilities"]["telemetry_v1"]==true,
            "telemetry":telemetry.map(|(s,at)| json!({"unix_s":at,"metrics":serde_json::from_str::<Value>(&s).unwrap_or_default()})),
            "events":events}));
    }
    let alerts = alerts(&inventory, &last_seen, &settings, now());
    Ok(json!({"alerts":alerts,"nodes":nodes,"settings":settings}))
}

/// Merges health into `ow nodes` output: alerts and latest telemetry per node.
pub fn annotate(inventory: &mut Value, health: &Value) {
    for n in inventory.as_array_mut().into_iter().flatten() {
        let id = n["id"].clone();
        n["alerts"] = json!(
            health["alerts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|a| a["node"] == id)
                .collect::<Vec<_>>()
        );
        n["telemetry"] = health["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|h| h["id"] == id)
            .map(|h| h["telemetry"].clone())
            .unwrap_or(Value::Null);
    }
}

/// `ow health`: prints fleet health; exit status 1 while any alert is active.
pub fn health_command(root: &Path) -> Result<i32> {
    let health = health(root)?;
    println!("{health:#}");
    Ok(i32::from(
        health["alerts"].as_array().is_some_and(|a| !a.is_empty()),
    ))
}
/// `ow config list|get|set` on the controller host.
pub fn config_command(root: &Path, action: crate::ConfigAction) -> Result<i32> {
    let value = match action {
        crate::ConfigAction::List => settings(&crate::nodes::db(root)?)?,
        crate::ConfigAction::Get { key } => {
            typed(&key, "").or_else(|_| {
                typed(
                    &key,
                    KEYS.iter().find(|(k, _)| *k == key).map_or("", |(_, d)| d),
                )
            })?;
            settings(&crate::nodes::db(root)?)?[key.as_str()].clone()
        }
        crate::ConfigAction::Set { key, value } => {
            let actor = std::env::var("SUDO_USER")
                .or_else(|_| std::env::var("USER"))
                .unwrap_or_else(|_| "local".into());
            set(root, &key, &value, &actor)?
        }
    };
    println!("{value:#}");
    Ok(0)
}

/// Controller-side series for the forwarder: node up and alert state.
fn controller_metrics(health: &Value, start: u128) -> Value {
    let now = unix_nanos();
    let nodes: Vec<&Value> = health["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|n| n["revoked"] != true)
        .collect();
    let up = Metric {
        name: "ow.node.up",
        unit: "1",
        sum: false,
        points: nodes
            .iter()
            .map(|n| {
                (
                    vec![(
                        "ow.node.id",
                        n["id"].as_str().unwrap_or_default().to_owned(),
                    )],
                    Num::Int(i64::from(n["online"] == true)),
                )
            })
            .collect(),
    };
    // Every kind for every runner, 1 or 0: a cleared alert reports 0, so a latest-value
    // backend sees it end instead of keeping the last 1 until the series goes stale.
    let active = |node: &Value, kind: &str| {
        health["alerts"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|a| a["node"] == node["id"] && a["kind"] == kind)
    };
    let alerts = Metric {
        name: "ow.alert.active",
        unit: "1",
        sum: false,
        points: nodes
            .iter()
            .flat_map(|n| {
                ALERTS.map(|kind| {
                    (
                        vec![
                            (
                                "ow.node.id",
                                n["id"].as_str().unwrap_or_default().to_owned(),
                            ),
                            ("ow.alert.kind", kind.to_owned()),
                        ],
                        Num::Int(i64::from(active(n, kind))),
                    )
                })
            })
            .collect(),
    };
    let age = Metric {
        name: "ow.node.heartbeat.age",
        unit: "s",
        sum: false,
        points: nodes
            .iter()
            .filter_map(|n| {
                let heard = n["last_heartbeat_unix_s"].as_i64()?;
                Some((
                    vec![(
                        "ow.node.id",
                        n["id"].as_str().unwrap_or_default().to_owned(),
                    )],
                    Num::Int((now / 1_000_000_000) as i64 - heard),
                ))
            })
            .collect(),
    };
    let dropped = metric(
        "ow.telemetry.forward.dropped",
        "{payload}",
        true,
        Num::Int(FORWARD_DROPPED.load(Ordering::SeqCst) as i64),
    );
    let metrics: Vec<Value> = [up, alerts, age, dropped]
        .iter()
        .filter(|m| !m.points.is_empty())
        .map(|m| encode_metric(m, start, now))
        .collect();
    json!({"resourceMetrics":[{"resource":{"attributes":attributes(&[("service.name","ow-controller".into()),("service.version",env!("CARGO_PKG_VERSION").into())])},
        "scopeMetrics":[{"scope":{"name":"ow","version":env!("CARGO_PKG_VERSION")},"metrics":metrics}]}]})
}

/// Posts queued payloads to `<endpoint>/v1/metrics|logs` when an endpoint is set. One
/// attempt each, 5 s timeout: telemetry is never retried at the cost of newer data.
pub(crate) async fn forwarder(
    root: std::path::PathBuf,
    mut rx: tokio::sync::mpsc::Receiver<(&'static str, Value)>,
) {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    else {
        return;
    };
    let mut endpoint = (Instant::now() - Duration::from_secs(60), String::new());
    while let Some((signal, body)) = rx.recv().await {
        // The endpoint is re-read at most every 3 s, so a change applies within seconds.
        if endpoint.0.elapsed() > Duration::from_secs(3) {
            let root = root.clone();
            let url = tokio::task::spawn_blocking(move || {
                crate::nodes::db(&root)
                    .and_then(|db| settings(&db))
                    .map(|s| {
                        s["telemetry.forward_endpoint"]
                            .as_str()
                            .unwrap_or("")
                            .to_owned()
                    })
            })
            .await;
            endpoint = (
                Instant::now(),
                url.ok().and_then(Result::ok).unwrap_or_default(),
            );
        }
        if endpoint.1.is_empty() {
            continue;
        }
        let sent = client
            .post(format!("{}/v1/{signal}", endpoint.1))
            .json(&body)
            .send()
            .await;
        if !sent.is_ok_and(|r| r.status().is_success()) {
            FORWARD_DROPPED.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// The controller's telemetry tasks: the forwarder and a 15 s alert loop that logs and
/// records alert transitions and forwards controller-side series.
pub(crate) fn spawn_controller_tasks(root: std::path::PathBuf) {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    if FORWARD.set(tx).is_ok() {
        tokio::spawn(forwarder(root.clone(), rx));
    }
    let start = unix_nanos();
    tokio::spawn(async move {
        let mut active: BTreeMap<(String, String), Value> = BTreeMap::new();
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tick.tick().await;
            let root = root.clone();
            let previous = active.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<_> {
                let health = health(&root)?;
                let mut now = BTreeMap::new();
                for a in health["alerts"].as_array().into_iter().flatten() {
                    let key = (
                        a["kind"].as_str().unwrap_or_default().to_owned(),
                        a["node"].as_str().unwrap_or_default().to_owned(),
                    );
                    now.insert(key, a.clone());
                }
                let db = crate::nodes::db(&root)?;
                for ((kind, node), alert) in &now {
                    if !previous.contains_key(&(kind.clone(), node.clone())) {
                        eprintln!(
                            "alert raised: {kind} node={node}: {}",
                            alert["message"].as_str().unwrap_or("")
                        );
                        record_event(&db, node, "ERROR", "ow.alert.raised", &json!({"kind":kind}))?;
                    }
                }
                for (kind, node) in previous.keys() {
                    if !now.contains_key(&(kind.clone(), node.clone())) {
                        eprintln!("alert cleared: {kind} node={node}");
                        record_event(&db, node, "INFO", "ow.alert.cleared", &json!({"kind":kind}))?;
                    }
                }
                Ok((now, health))
            })
            .await;
            if let Ok(Ok((now, health))) = result {
                active = now;
                forward("metrics", controller_metrics(&health, start));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    const SALT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn metrics_encode_as_otlp_json_and_split_under_the_frame_limit() {
        let metrics = vec![
            metric("system.cpu.utilization", "1", false, Num::Double(0.25)),
            metric("ow.node.channel.connects", "{connect}", true, Num::Int(3)),
        ];
        let (frames, dropped) = encode_metrics("alpha", &metrics, 1, 2);
        assert_eq!((frames.len(), dropped), (1, 0));
        let v: Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(v["type"], "otlp");
        assert_eq!(v["signal"], "metrics");
        let rm = &v["body"]["resourceMetrics"][0];
        assert!(
            rm["resource"]["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["key"] == "ow.node.id" && a["value"]["stringValue"] == "alpha")
        );
        let m = &rm["scopeMetrics"][0]["metrics"];
        assert_eq!(m[0]["gauge"]["dataPoints"][0]["asDouble"], 0.25);
        assert_eq!(m[0]["gauge"]["dataPoints"][0]["timeUnixNano"], "2");
        assert_eq!(m[1]["sum"]["aggregationTemporality"], 2);
        assert_eq!(m[1]["sum"]["isMonotonic"], true);
        assert_eq!(m[1]["sum"]["dataPoints"][0]["asInt"], "3");
        assert_eq!(m[1]["sum"]["dataPoints"][0]["startTimeUnixNano"], "1");

        // Many boxes split into several frames, each under the limit; a metric that can
        // never fit is dropped and counted.
        let many = (0..2000)
            .map(|i| (vec![("ow.box.id", format!("m-{i:024x}"))], Num::Int(i)))
            .collect::<Vec<_>>();
        let big = Metric {
            name: "ow.box.memory.rss",
            unit: "By",
            sum: false,
            points: many,
        };
        let small = (0..40)
            .map(|i| Metric {
                name: "ow.box.memory.pss",
                unit: "By",
                sum: false,
                points: (0..32)
                    .map(|j| (vec![("ow.box.id", format!("m-{j:024x}"))], Num::Int(i)))
                    .collect(),
            })
            .collect::<Vec<_>>();
        let (frames, dropped) = encode_metrics("alpha", &small, 1, 2);
        assert!(frames.len() > 1);
        assert!(frames.iter().all(|f| f.len() <= FRAME_LIMIT));
        assert_eq!(dropped, 0);
        let (frames, dropped) = encode_metrics("alpha", &[big], 1, 2);
        assert_eq!((frames.len(), dropped), (0, 1));
    }

    #[test]
    fn queue_is_bounded_drops_oldest_and_waits_for_negotiation() {
        let agent = Agent::new("alpha", SALT.into());
        for _ in 0..QUEUE + 5 {
            agent.event("INFO", "ow.test", &[]);
        }
        assert_eq!(agent.queue.lock().unwrap().len(), QUEUE);
        assert_eq!(agent.dropped.load(Ordering::SeqCst), 6);
        assert!(
            agent.next().is_none(),
            "nothing is sent before the controller offers it"
        );
        agent.acknowledged(&json!({"type":"ready","extensions":["telemetry-v1"],"telemetry":{"enabled":true,"interval_s":2}}), Instant::now());
        assert_eq!(
            agent.interval_s.load(Ordering::SeqCst),
            5,
            "interval is clamped"
        );
        assert!(agent.next().is_some());
        agent.acknowledged(
            &json!({"type":"ready","extensions":["telemetry-v1"],"telemetry":{"enabled":false}}),
            Instant::now(),
        );
        assert!(agent.next().is_none());
        assert!(
            agent.queue.lock().unwrap().is_empty(),
            "disabling clears the queue"
        );
        agent.event("INFO", "ow.test", &[]);
        assert!(
            agent.queue.lock().unwrap().is_empty(),
            "disabled agents queue nothing"
        );
        // An old controller (no extension) gets nothing.
        agent.acknowledged(
            &json!({"type":"ready","extensions":["cache-inventory-v1"]}),
            Instant::now(),
        );
        assert!(!agent.negotiated.load(Ordering::SeqCst));
    }

    #[test]
    fn channel_transitions_are_counted_once_with_a_reason_class() {
        assert_eq!(
            reason(&anyhow::anyhow!("no frame from the controller for 20 s")),
            "silent"
        );
        assert_eq!(session_reason("no heartbeat for 15 s"), "timeout");
        assert_eq!(
            session_reason("heartbeat refused: protocol 9 unsupported"),
            "refused"
        );
        assert_eq!(
            session_reason(
                "the health store no longer holds this session (heartbeat matched no row)"
            ),
            "store"
        );
        assert_eq!(session_reason("channel closed"), "closed");
        let agent = Agent::new("alpha", SALT.into());
        agent.queue.lock().unwrap().clear();
        let up = json!({"type":"ready","extensions":["telemetry-v1"],"telemetry":{"enabled":true}});
        agent.acknowledged(&up, Instant::now());
        agent.acknowledged(&up, Instant::now());
        for _ in 0..3 {
            agent.disconnected(&anyhow::anyhow!("DNS resolver capacity"));
        }
        agent.acknowledged(&up, Instant::now());
        let channel = agent.channel.lock().unwrap();
        assert_eq!(
            channel.connects, 2,
            "repeated acks of one connection count once"
        );
        assert_eq!(channel.disconnects.get("dns"), Some(&1));
        drop(channel);
        let names: Vec<String> = agent
            .queue
            .lock()
            .unwrap()
            .iter()
            .map(|f| {
                let v: Value = serde_json::from_str(f).unwrap();
                let r = &v["body"]["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
                let attrs = checked_attributes(&r["attributes"]).unwrap();
                attrs
                    .into_iter()
                    .find(|(k, _)| k == "event.name")
                    .unwrap()
                    .1
            })
            .collect();
        assert_eq!(
            names,
            [
                "ow.node.channel.disconnected",
                "ow.node.channel.reconnected"
            ]
        );
        assert_eq!(
            reason(&anyhow::anyhow!(
                "controller closed the channel: unsupported"
            )),
            "refused"
        );
        assert_eq!(
            reason(&anyhow::anyhow!("HTTP error: 401 Unauthorized")),
            "auth"
        );
        assert_eq!(
            reason(&anyhow::anyhow!("no compatible worker answers")),
            "worker"
        );
    }

    #[test]
    fn proc_parsers_and_box_ids() {
        assert_eq!(
            cpu_times("cpu  10 0 10 70 10 0 0 0 0 0\ncpu0 1 2 3\n"),
            Some((20, 100))
        );
        assert_eq!(
            kib_field(
                "MemTotal:       16384 kB\nMemAvailable:    4096 kB\n",
                "MemAvailable"
            ),
            Some(4096 * 1024)
        );
        assert_eq!(
            kib_field("Name: ow\nVmRSS:\t  2048 kB\n", "VmRSS"),
            Some(2048 * 1024)
        );
        let id = format!("m-{}", "a".repeat(24));
        assert_eq!(box_id(&id, SALT), id);
        let hashed = box_id("my-secret-project", SALT);
        assert!(hashed.starts_with("h-") && !hashed.contains("secret"));
        let stats = json!({"running":{id.clone():{"rss_kib":10,"pss_kib":5},"legacy":{"rss_kib":20,"pss_kib":7}}});
        let metrics = box_metrics(&stats, SALT);
        assert_eq!(metrics[1].points.len(), 2);
        assert_eq!(
            metrics[1].points[0].0[0].1,
            box_id("legacy", SALT),
            "largest first"
        );
    }

    #[test]
    fn settings_validate_and_version() {
        let root = std::env::temp_dir().join(format!("ow-t-{}", crate::common::nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        let db = crate::nodes::db(&root).unwrap();
        let s = settings(&db).unwrap();
        assert_eq!(
            (
                s["telemetry.enabled"].clone(),
                s["telemetry.interval_s"].clone(),
                s["version"].clone()
            ),
            (json!(true), json!(15), json!(0))
        );
        assert!(set(&root, "telemetry.interval_s", "1", "t").is_err());
        assert!(
            set(
                &root,
                "telemetry.forward_endpoint",
                "https://user:pw@collector.example/",
                "t"
            )
            .is_err()
        );
        assert!(
            set(
                &root,
                "telemetry.forward_endpoint",
                "https://collector.example/?token=x",
                "t"
            )
            .is_err()
        );
        assert!(set(&root, "no.such", "1", "t").is_err());
        let s = set(
            &root,
            "telemetry.forward_endpoint",
            "https://collector.example:4318/",
            "t",
        )
        .unwrap();
        assert_eq!(
            s["telemetry.forward_endpoint"],
            "https://collector.example:4318"
        );
        let s = set(&root, "telemetry.enabled", "false", "t").unwrap();
        assert_eq!(
            (s["telemetry.enabled"].clone(), s["version"].clone()),
            (json!(false), json!(2))
        );
        assert_eq!(offer(&db)["enabled"], false);
        let changes: i64 = db
            .query_row("SELECT COUNT(*) FROM config_changes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(changes, 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn received_frames_are_validated_stamped_and_summarized() {
        let agent = Agent::new("alpha", SALT.into());
        let (frames, _) = encode_metrics(
            "forged",
            &box_metrics(
                &json!({"running":{"m-000000000000000000000001":{"rss_kib":1,"pss_kib":1}}}),
                SALT,
            ),
            1,
            2,
        );
        let mut frame: Value = serde_json::from_str(&frames[0]).unwrap();
        // Fields outside the validated set are not forwarded.
        frame["body"]["resourceMetrics"][0]["schemaUrl"] = json!("https://example.invalid/leak");
        frame["body"]["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["description"] =
            json!("/home/user/project");
        let (received, signal, forwarded) = receive("alpha", &frame).unwrap();
        assert_eq!(signal, "metrics");
        let Received::Metrics(summary) = received else {
            panic!()
        };
        assert_eq!(summary["ow.box.running"], 1.0);
        assert_eq!(
            summary["ow.box.memory.rss{ow.box.id=m-000000000000000000000001}"],
            1024.0
        );
        let attrs = &forwarded["resourceMetrics"][0]["resource"]["attributes"];
        let ids: Vec<_> = attrs
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["key"] == "ow.node.id")
            .collect();
        assert_eq!(ids.len(), 1);
        assert_eq!(
            ids[0]["value"]["stringValue"], "alpha",
            "the authenticated node, not the claimed one"
        );

        let event: Value = serde_json::from_str(&agent.queue.lock().unwrap()[0]).unwrap();
        let (received, signal, _) = receive("alpha", &event).unwrap();
        assert_eq!(signal, "logs");
        let Received::Events(events) = received else {
            panic!()
        };
        assert_eq!(events[0].2, "ow.node.agent.started");

        let mut bad = frame.clone();
        bad["body"]["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["name"] =
            json!("user.home.files");
        assert!(receive("alpha", &bad).is_err());
        let mut bad = frame.clone();
        bad["body"]["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][1]["gauge"]["dataPoints"]
            [0]["attributes"][0]["value"]["stringValue"] = json!("/home/user secret");
        assert!(receive("alpha", &bad).is_err());
        assert!(receive("alpha", &json!({"type":"otlp","signal":"traces","body":{}})).is_err());
    }

    #[test]
    fn forwarded_requests_carry_only_validated_fields() {
        let (frames, _) = encode_metrics(
            "alpha",
            &[metric("ow.box.running", "{box}", false, Num::Int(1))],
            1,
            2,
        );
        let mut frame: Value = serde_json::from_str(&frames[0]).unwrap();
        frame["body"]["resourceMetrics"][0]["schemaUrl"] = json!("leak-1");
        frame["body"]["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["description"] =
            json!("leak-2");
        let (_, _, forwarded) = receive("alpha", &frame).unwrap();
        let text = forwarded.to_string();
        assert!(!text.contains("leak"), "{text}");
        assert_eq!(
            forwarded["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["gauge"]["dataPoints"]
                [0]["asInt"],
            "1"
        );
        let mut bad = frame.clone();
        bad["body"]["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["name"] =
            json!("ow.x\nalert cleared: node_offline");
        let error = receive("alpha", &bad).err().unwrap();
        assert!(!format!("{error:#}").contains('\n'));
    }

    #[test]
    fn a_runner_whose_sessions_keep_failing_goes_offline_and_restarts_give_grace() {
        let root = std::env::temp_dir().join(format!("ow-t-{}", crate::common::nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let db = crate::nodes::db(&root).unwrap();
        db.execute("INSERT INTO nodes(id,heartbeat,memory_mib,slots,capabilities) VALUES('alpha',0,512,2,'{\"protocol\":1}')", []).unwrap();
        // Last stored heartbeat 10 min ago; every later session failed before a heartbeat.
        session_closed(&db, "alpha", Some(now() - 600), "timeout").unwrap();
        for _ in 0..5 {
            session_closed(&db, "alpha", None, "refused").unwrap();
        }
        let health = health(&root).unwrap();
        let alert = &health["alerts"][0];
        assert_eq!(
            (alert["kind"].as_str(), alert["node"].as_str()),
            (Some("node_offline"), Some("alpha"))
        );
        assert_eq!(alert["reason"], "refused", "the latest close reason");
        let events: Vec<_> = health["nodes"][0]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["name"] == "ow.node.session.closed")
            .collect();
        assert_eq!(events.len(), 1, "failed sessions write no events");
        // A controller that just started gives runners the grace period to reconnect.
        controller_started(&db).unwrap();
        assert!(
            super::health(&root).unwrap()["alerts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_latest_summary_is_capped_and_replaced_by_a_later_round() {
        let root = std::env::temp_dir().join(format!("ow-t-{}", crate::common::nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        let many = |prefix: &str| {
            Received::Metrics(
                (0..200)
                    .map(|i| (format!("ow.{prefix}.{i}"), 1.0))
                    .collect(),
            )
        };
        store(&root, "alpha", &many("a")).unwrap();
        store(&root, "alpha", &many("b")).unwrap();
        let db = crate::nodes::db(&root).unwrap();
        let summary: String = db
            .query_row(
                "SELECT summary FROM node_telemetry WHERE node='alpha'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let summary: BTreeMap<String, f64> = serde_json::from_str(&summary).unwrap();
        assert_eq!(summary.len(), SERIES);
        db.execute("UPDATE node_telemetry SET unix_s=unix_s-60", [])
            .unwrap();
        store(
            &root,
            "alpha",
            &Received::Metrics(BTreeMap::from([("ow.c".to_owned(), 2.0)])),
        )
        .unwrap();
        let summary: String = db
            .query_row(
                "SELECT summary FROM node_telemetry WHERE node='alpha'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(summary, r#"{"ow.c":2.0}"#);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn delayed_events_keep_the_time_they_happened() {
        let root = std::env::temp_dir().join(format!("ow-t-{}", crate::common::nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        let agent = Agent::new("alpha", SALT.into());
        agent.disconnected(&anyhow::anyhow!("no frame from the controller for 20 s"));
        let frames: Vec<Value> = agent
            .queue
            .lock()
            .unwrap()
            .iter()
            .map(|f| serde_json::from_str(f).unwrap())
            .collect();
        // Queued during an hour-long outage: the records carry their own time.
        let hour_ago = ((unix_nanos() / 1_000_000_000) as i64 - 3600) as u128 * 1_000_000_000;
        for mut frame in frames {
            let record = &mut frame["body"]["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
            record["timeUnixNano"] = json!(hour_ago.to_string());
            let (received, _, forwarded) = receive("alpha", &frame).unwrap();
            assert_eq!(
                forwarded["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["timeUnixNano"],
                hour_ago.to_string()
            );
            store(&root, "alpha", &received).unwrap();
        }
        // A controller event that happens now.
        record_event(
            &crate::nodes::db(&root).unwrap(),
            "alpha",
            "WARN",
            "ow.alert.raised",
            &json!({"kind":"node_offline"}),
        )
        .unwrap();
        let db = crate::nodes::db(&root).unwrap();
        let stored: Vec<(String, i64)> = db
            .prepare("SELECT name,unix_s FROM node_events WHERE node='alpha' ORDER BY unix_s DESC")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let at = |name: &str| stored.iter().find(|(n, _)| n == name).unwrap().1;
        assert_eq!(
            at("ow.node.channel.disconnected"),
            (hour_ago / 1_000_000_000) as i64
        );
        assert_eq!(
            at("ow.node.agent.started"),
            (hour_ago / 1_000_000_000) as i64
        );
        assert!((at("ow.alert.raised") - now()).abs() <= 2);
        assert_eq!(
            stored[0].0, "ow.alert.raised",
            "listed by when they happened"
        );
        // A clock far off falls back to the time of arrival.
        assert_eq!(event_time("99999999999999999999", 1000), 1000);
        assert_eq!(event_time("not a number", 1000), 1000);
        assert_eq!(
            event_time(&(900u128 * 1_000_000_000).to_string(), 1000),
            900
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn forwarded_alert_series_report_zero_once_cleared() {
        let series = |health: &Value| -> Vec<(String, String, String)> {
            let body = controller_metrics(health, 1);
            body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["name"] == "ow.alert.active")
                .flat_map(|m| m["gauge"]["dataPoints"].as_array().unwrap().clone())
                .map(|p| {
                    let attr = |k: &str| {
                        p["attributes"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|a| a["key"] == k)
                            .unwrap()["value"]["stringValue"]
                            .as_str()
                            .unwrap()
                            .to_owned()
                    };
                    (
                        attr("ow.node.id"),
                        attr("ow.alert.kind"),
                        p["asInt"].as_str().unwrap().to_owned(),
                    )
                })
                .collect()
        };
        let node = |online: bool| json!({"id":"alpha","online":online,"revoked":false,"last_heartbeat_unix_s":now()-120});
        let down = json!({"nodes":[node(false)],"alerts":[{"kind":"node_offline","node":"alpha"}]});
        let up = json!({"nodes":[node(true)],"alerts":[]});
        let entry = |kind: &str, v: &str| ("alpha".to_owned(), kind.to_owned(), v.to_owned());
        assert_eq!(
            series(&down),
            [entry("node_offline", "1"), entry("disk_low", "0")]
        );
        assert_eq!(
            series(&up),
            [entry("node_offline", "0"), entry("disk_low", "0")]
        );
        let body = controller_metrics(&down, 1);
        let age = body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == "ow.node.heartbeat.age")
            .unwrap()
            .clone();
        let age: i64 = age["gauge"]["dataPoints"][0]["asInt"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((119..=122).contains(&age), "{age}");
    }

    #[test]
    fn alerts_for_offline_and_low_disk_runners() {
        let settings = json!({"alerts.offline_after_s":60,"alerts.disk_low_gib":20});
        let inventory = json!([
            {"id":"up","online":true,"revoked":false,"capabilities":{"protocol":1,"free_disk_bytes":gib(100)}},
            {"id":"full","online":true,"revoked":false,"capabilities":{"protocol":1,"free_disk_bytes":gib(5)}},
            {"id":"gone","online":false,"revoked":false,"capabilities":{"protocol":1}},
            {"id":"blip","online":false,"revoked":false,"capabilities":{"protocol":1}},
            {"id":"invited","online":false,"revoked":false,"capabilities":{"memory_mib":0}},
            {"id":"retired","online":false,"revoked":true,"capabilities":{"protocol":1}},
        ]);
        let last_seen = BTreeMap::from([
            ("gone".to_owned(), (1000, "timeout".to_owned())),
            ("blip".to_owned(), (1090, "closed".to_owned())),
        ]);
        let alerts = alerts(&inventory, &last_seen, &settings, 1100);
        let kinds: Vec<(&str, &str)> = alerts
            .iter()
            .map(|a| (a["kind"].as_str().unwrap(), a["node"].as_str().unwrap()))
            .collect();
        assert_eq!(kinds, [("disk_low", "full"), ("node_offline", "gone")]);
        assert_eq!(alerts[1]["reason"], "timeout");
        assert_eq!(alerts[1]["since_unix_s"], 1000);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwarder_posts_to_the_configured_endpoint_only() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = std::env::temp_dir().join(format!("ow-t-{}", crate::common::nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (seen_tx, mut seen) = tokio::sync::mpsc::channel::<String>(8);
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = vec![0; 65536];
                let n = s.read(&mut buf).await.unwrap();
                let _ = seen_tx
                    .send(String::from_utf8_lossy(&buf[..n]).into_owned())
                    .await;
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                    )
                    .await;
            }
        });
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tokio::spawn(forwarder(root.clone(), rx));
        // No endpoint: nothing is posted.
        tx.send(("metrics", json!({"resourceMetrics":[]})))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(500), seen.recv())
                .await
                .is_err()
        );
        set(
            &root,
            "telemetry.forward_endpoint",
            &format!("http://{address}"),
            "t",
        )
        .unwrap();
        tokio::time::sleep(Duration::from_secs(4)).await;
        tx.send(("logs", json!({"resourceLogs":[]}))).await.unwrap();
        let request = tokio::time::timeout(Duration::from_secs(5), seen.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(request.starts_with("POST /v1/logs HTTP/1.1"), "{request}");
        assert!(
            request
                .to_ascii_lowercase()
                .contains("content-type: application/json")
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
