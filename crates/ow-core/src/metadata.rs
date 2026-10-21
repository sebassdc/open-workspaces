//! Metadata mode of per-box AWS access: role credentials served to guests at the EC2
//! instance metadata address, `169.254.169.254`, so the AWS CLI, every SDK and Terraform
//! find them as they do on EC2. See docs/AWS_ACCESS.md and ADR 0010.
//!
//! The controller assumes each box's role and pushes the temporary credentials to the worker
//! that hosts the box (`aws-creds`); they live only in this process's memory, never on disk or
//! in the guest. One listener in the worker's network namespace answers every box: nft
//! redirects `169.254.169.254:80` arriving on a box TAP to [`PORT`], and the box is identified
//! by its peer address, which the TAP's anti-spoofing rule binds to that box. IMDSv2 only, and
//! the real metadata service is never contacted. Nothing here takes the worker's runtime lock:
//! SDKs give instance metadata one second and, in botocore and the CLI, one attempt.
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{Arc, Condvar, LazyLock, Mutex, RwLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

/// The listener in the worker's network namespace; nft redirects the metadata address here.
pub const PORT: u16 = 9169;
/// Concurrent connections one box may hold.
const CONNECTIONS: usize = 8;
/// Requests per second one box may make, after a burst of [`BURST`].
const RATE: f64 = 100.0;
const BURST: f64 = 200.0;
const HEADER_BYTES: usize = 4096;
/// A request's headers must arrive this soon after its first byte.
const HEADER_TIME: Duration = Duration::from_secs(1);
/// An idle keep-alive connection closes after this.
const IDLE: Duration = Duration::from_secs(5);
/// A box whose role is attached but whose credentials have not arrived yet (a fresh attach,
/// a cold start) waits this long before the answer is 404. Below the SDKs' one second.
const HOLD: Duration = Duration::from_millis(800);
/// The longest IMDSv2 session token EC2 issues.
const MAX_TOKEN_TTL: u64 = 21600;

/// One role session of a box. Deliberately not `Debug`; secrets are wiped when dropped.
#[derive(Clone)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: Zeroizing<String>,
    pub session_token: Zeroizing<String>,
    /// Unix seconds.
    pub expiration: u64,
    /// Unix seconds; `LastUpdated` in the answer.
    pub issued: u64,
}

/// A box's attached role. Present without credentials while they are on their way (or the
/// box is stopped): such a box waits up to [`HOLD`] instead of getting 404 at once.
#[derive(Clone)]
pub struct Role {
    pub role_arn: String,
    /// The controller's attachment generation: a move, fork or re-attach advances it.
    pub generation: u64,
    /// IP TTL of answers: 1 keeps containers inside the box (one routed hop) out.
    pub hop_limit: u8,
    pub region: Option<String>,
    pub zone: Option<String>,
    pub credentials: Option<Credentials>,
}

fn word(value: &str, max: usize, extra: &[u8]) -> bool {
    (1..=max).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || extra.contains(&b))
}

impl Role {
    /// Values from the controller, checked before they reach a guest. Errors name no field value.
    pub fn validate(&self) -> Result<()> {
        crate::credential_broker::role_arn(&self.role_arn)?;
        ensure!(
            self.generation > 0 && matches!(self.hop_limit, 1 | 2),
            "invalid AWS role generation or hop limit"
        );
        for place in [&self.region, &self.zone].into_iter().flatten() {
            ensure!(word(place, 40, b"-"), "invalid AWS region");
        }
        if let Some(c) = &self.credentials {
            ensure!(
                word(&c.access_key_id, 128, b"")
                    && word(&c.secret_access_key, 256, b"+/=")
                    && word(&c.session_token, 8192, b"+/=")
                    && c.expiration > c.issued,
                "invalid AWS credentials"
            );
        }
        Ok(())
    }

    fn name(&self) -> &str {
        self.role_arn.rsplit('/').next().unwrap_or_default()
    }
}

/// Which box a TAP index belongs to, with a nonce new at each VMM start of it: IMDSv2 tokens
/// name both, so a token never outlives the VMM it was issued to.
struct Peer {
    id: String,
    nonce: String,
}

#[derive(Default)]
struct State {
    peers: HashMap<u32, Peer>,
    roles: HashMap<String, Role>,
}

static STATE: LazyLock<RwLock<State>> = LazyLock::new(Default::default);
/// Signals waiters in [`HOLD`] that a role changed.
static CHANGED: LazyLock<(Mutex<u64>, Condvar)> = LazyLock::new(Default::default);
/// Signs IMDSv2 tokens; random per worker process, so a restart invalidates them all.
static KEY: LazyLock<[u8; 32]> = LazyLock::new(|| {
    let mut key = [0; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut key))
        .expect("random token key");
    key
});
static SLOTS: LazyLock<Mutex<HashMap<u32, usize>>> = LazyLock::new(Default::default);
static BUCKETS: LazyLock<Mutex<HashMap<u32, (f64, Instant)>>> = LazyLock::new(Default::default);

fn state() -> std::sync::RwLockReadGuard<'static, State> {
    STATE.read().unwrap_or_else(|e| e.into_inner())
}

fn state_mut() -> std::sync::RwLockWriteGuard<'static, State> {
    STATE.write().unwrap_or_else(|e| e.into_inner())
}

fn changed() {
    let (count, signal) = &*CHANGED;
    *count.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    signal.notify_all();
}

/// A VMM of box `id` now owns TAP `ow{index}`.
pub fn attach(index: u32, id: &str) -> Result<()> {
    let nonce = crate::common::nonce()?;
    state_mut().peers.insert(
        index,
        Peer {
            id: id.to_owned(),
            nonce,
        },
    );
    Ok(())
}

/// Called before the TAP goes, so a reused index never answers as the old box.
pub fn detach(index: u32) {
    state_mut().peers.remove(&index);
}

/// Record a box's role; credentials, if any, are served from now on.
pub fn set(id: &str, role: Role) {
    if role.credentials.is_some() {
        undumpable();
    }
    state_mut().roles.insert(id.to_owned(), role);
    changed();
}

/// Forget the box's role and credentials (detach, owner change, delete, move away).
pub fn clear(id: &str) -> bool {
    let removed = state_mut().roles.remove(id).is_some();
    changed();
    removed
}

/// The box stopped: drop its credentials, keep knowing it has a role so a cold start waits
/// for fresh ones instead of answering "no role".
pub fn stopped(id: &str) {
    if let Some(role) = state_mut().roles.get_mut(id) {
        role.credentials = None;
    }
}

/// Whether the box holds current credentials of this generation.
pub fn present(id: &str, generation: u64) -> bool {
    state().roles.get(id).is_some_and(|r| {
        r.generation == generation
            && r.credentials
                .as_ref()
                .is_some_and(|c| c.expiration > unix())
    })
}

/// The worker's `aws-creds` operation for a box it hosts. Without `role_arn` it forgets the
/// box's role; with `check` it reports whether this generation's credentials are held. Keys
/// without a live VMM (`live`: running or hibernated) are not kept: a stopped box only
/// remembers that a role is attached, and gets credentials again when it runs.
pub fn request(request: &serde_json::Value, live: bool) -> Result<serde_json::Value> {
    let id = request["id"].as_str().context("missing ID")?;
    crate::common::identifier(id)?;
    let generation = request["generation"].as_u64();
    if request["check"] == true {
        let generation = generation.context("missing generation")?;
        return Ok(serde_json::json!({"present": present(id, generation)}));
    }
    let Some(role_arn) = request["role_arn"].as_str() else {
        return Ok(serde_json::json!({"cleared": clear(id)}));
    };
    let text = |key: &str| request[key].as_str().map(str::to_owned);
    let credentials = match text("access_key_id") {
        Some(access_key_id) => Some(Credentials {
            access_key_id,
            secret_access_key: Zeroizing::new(
                text("secret_access_key").context("missing AWS credentials")?,
            ),
            session_token: Zeroizing::new(
                text("session_token").context("missing AWS credentials")?,
            ),
            expiration: request["expiration"]
                .as_u64()
                .context("missing AWS credentials")?,
            issued: unix(),
        }),
        None => None,
    };
    let mut role = Role {
        role_arn: role_arn.to_owned(),
        generation: generation.context("missing generation")?,
        hop_limit: u8::try_from(request["hop_limit"].as_u64().unwrap_or(1))?,
        region: text("region"),
        zone: text("availability_zone"),
        credentials,
    };
    role.validate()?;
    if !live {
        role.credentials = None;
        // What a stop left (or a hibernation still sealing holds) stays as it is.
        if state()
            .roles
            .get(id)
            .is_some_and(|r| r.generation == role.generation)
        {
            return Ok(serde_json::json!({"stored": false}));
        }
    }
    let stored = role.credentials.is_some();
    set(id, role);
    Ok(serde_json::json!({"stored": stored}))
}

/// Overwrite the secrets of an `aws-creds` request once it is handled.
pub fn wipe(request: &mut serde_json::Value) {
    use zeroize::Zeroize;
    for key in ["secret_access_key", "session_token"] {
        if let Some(serde_json::Value::String(secret)) = request.get_mut(key) {
            secret.zeroize();
        }
    }
}

/// A worker holding role credentials keeps them out of core dumps and away from same-user
/// ptrace; set once, when the first credentials arrive.
fn undumpable() {
    #[cfg(target_os = "linux")]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| unsafe {
            libc::prctl(libc::PR_SET_DUMPABLE, 0);
        });
    }
}

fn unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `YYYY-MM-DDTHH:MM:SSZ`, as EC2 writes `Expiration` and `LastUpdated`.
fn timestamp(unix: u64) -> String {
    let (days, seconds) = ((unix / 86400) as i64, unix % 86400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}

fn hmac(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A stateless IMDSv2 token: expiry and a MAC over box, VMM nonce, role generation (0
/// without a role) and expiry. A detach, re-attach, move or fork of the box changes the
/// generation, so tokens issued before it stop working.
fn mint(peer: &Peer, generation: u64, expires: u64) -> String {
    let mac = hmac(
        &*KEY,
        format!("{}\n{}\n{generation}\n{expires}", peer.id, peer.nonce).as_bytes(),
    );
    format!("{expires}.{}", hex(&mac))
}

fn valid(peer: &Peer, generation: u64, token: &str, now: u64) -> bool {
    let Some((expires, _)) = token.split_once('.') else {
        return false;
    };
    let Ok(expires) = expires.parse::<u64>() else {
        return false;
    };
    let expected = mint(peer, generation, expires);
    // Constant time over equal lengths; the length itself is public.
    expires > now
        && expires <= now + MAX_TOKEN_TTL
        && token.len() == expected.len()
        && token
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// The box behind a TAP connection: the peer is a guest's `.2` and the local end the host's
/// `.1` of the same /30 (`198.18.0.0/15`, as `network::register` assigns them).
pub fn peer_index(peer: SocketAddr, local: SocketAddr) -> Option<u32> {
    let (SocketAddr::V4(peer), SocketAddr::V4(local)) = (peer, local) else {
        return None;
    };
    let [a, b, c, d] = peer.ip().octets();
    let [la, lb, lc, ld] = local.ip().octets();
    (a == 198 && b == 18 && c < 128 && d % 4 == 2 && [la, lb, lc] == [a, b, c] && ld == d - 1)
        .then(|| u32::from(c) * 64 + u32::from(d) / 4)
}

pub(crate) type Resolve = Arc<dyn Fn(SocketAddr, SocketAddr) -> Option<u32> + Send + Sync>;

/// Listen on [`PORT`] in this (the worker's) network namespace.
pub fn start() -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", PORT)).context("metadata listener")?;
    serve(listener, Arc::new(peer_index));
    Ok(())
}

/// Answer connections on their own threads; `resolve` names the box of a connection.
pub(crate) fn serve(listener: TcpListener, resolve: Resolve) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                // Out of descriptors or similar: do not spin.
                std::thread::sleep(Duration::from_millis(10));
                continue;
            };
            let (Ok(peer), Ok(local)) = (stream.peer_addr(), stream.local_addr()) else {
                continue;
            };
            let Some(index) = resolve(peer, local) else {
                continue;
            };
            let Some(slot) = Slot::take(index) else {
                continue;
            };
            std::thread::spawn(move || {
                let _slot = slot;
                connection(stream, index);
            });
        }
    });
}

struct Slot(u32);

impl Slot {
    fn take(index: u32) -> Option<Self> {
        let mut slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        let used = slots.entry(index).or_default();
        (*used < CONNECTIONS).then(|| {
            *used += 1;
            Self(index)
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(used) = slots.get_mut(&self.0) {
            *used -= 1;
            if *used == 0 {
                slots.remove(&self.0);
            }
        }
    }
}

fn admit(index: u32) -> bool {
    let mut buckets = BUCKETS.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let (tokens, at) = buckets.entry(index).or_insert((BURST, now));
    *tokens = (*tokens + now.duration_since(*at).as_secs_f64() * RATE).min(BURST);
    *at = now;
    if *tokens >= 1.0 {
        *tokens -= 1.0;
        true
    } else {
        false
    }
}

struct Request {
    method: String,
    path: String,
    keep_alive: bool,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

fn read_head(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
    let mut deadline = (!buffer.is_empty()).then(|| Instant::now() + HEADER_TIME);
    loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            ensure!(end + 4 <= HEADER_BYTES, "request headers too large");
            return Ok(Some(buffer.drain(..end + 4).collect()));
        }
        ensure!(buffer.len() < HEADER_BYTES, "request headers too large");
        let now = Instant::now();
        let wait = match deadline {
            Some(deadline) => deadline
                .checked_duration_since(now)
                .context("request headers too slow")?,
            None => IDLE,
        };
        stream.set_read_timeout(Some(wait.max(Duration::from_millis(1))))?;
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        deadline.get_or_insert(now + HEADER_TIME);
        buffer.extend_from_slice(&chunk[..n]);
    }
}

fn parse(head: &[u8]) -> Result<Request> {
    let text = std::str::from_utf8(head)?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(path), Some(version), None) =
        (first.next(), first.next(), first.next(), first.next())
    else {
        bail!("bad request line");
    };
    ensure!(
        matches!(version, "HTTP/1.1" | "HTTP/1.0") && path.starts_with('/'),
        "bad request line"
    );
    let mut headers = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let (name, value) = line.split_once(':').context("bad header")?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    let connection = headers
        .iter()
        .find(|(n, _)| n == "connection")
        .map(|(_, v)| v.to_ascii_lowercase());
    Ok(Request {
        method: method.to_owned(),
        path: path.split('?').next().unwrap_or_default().to_owned(),
        keep_alive: match connection.as_deref() {
            Some("close") => false,
            Some("keep-alive") => true,
            _ => version == "HTTP/1.1",
        },
        headers,
    })
}

struct Response {
    status: u16,
    body: Zeroizing<String>,
    ttl: Option<u64>,
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Too Many Requests",
    }
}

fn status(status: u16) -> Response {
    Response {
        status,
        body: Zeroizing::new(reason(status).to_owned()),
        ttl: None,
    }
}

fn ok(body: String) -> Response {
    Response {
        status: 200,
        body: Zeroizing::new(body),
        ttl: None,
    }
}

/// The box's role, waiting up to [`HOLD`] while it is attached but has no current
/// credentials. `None` is "no role" (404 at once, so SDKs fall through fast).
fn role(id: &str) -> Option<Role> {
    let deadline = Instant::now() + HOLD;
    let (count, signal) = &*CHANGED;
    let mut seen = count.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let role = state().roles.get(id).cloned()?;
        let current = role
            .credentials
            .as_ref()
            .is_some_and(|c| c.expiration > unix());
        let left = deadline.saturating_duration_since(Instant::now());
        if current || left.is_zero() {
            return current.then_some(role);
        }
        seen = signal
            .wait_timeout(seen, left)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

fn answer(index: u32, request: &Request) -> Response {
    // Real IMDS refuses forwarded requests: a proxy in the box must not relay for others.
    if request.header("x-forwarded-for").is_some() {
        return status(403);
    }
    if request.header("transfer-encoding").is_some()
        || request.header("content-length").is_some_and(|l| l != "0")
    {
        return status(400);
    }
    let now = unix();
    let state = state();
    let Some(peer) = state.peers.get(&index) else {
        return status(404);
    };
    let generation = state.roles.get(&peer.id).map_or(0, |r| r.generation);
    if request.path == "/latest/api/token" {
        if request.method != "PUT" {
            return status(405);
        }
        let Some(ttl) = request
            .header("x-aws-ec2-metadata-token-ttl-seconds")
            .and_then(|t| t.parse::<u64>().ok())
            .filter(|t| (1..=MAX_TOKEN_TTL).contains(t))
        else {
            return status(400);
        };
        let mut response = ok(mint(peer, generation, now + ttl));
        response.ttl = Some(ttl);
        return response;
    }
    if request.method != "GET" {
        return status(405);
    }
    // IMDSv2 only: no token, a wrong or expired one, or another box's, is 401.
    if !request
        .header("x-aws-ec2-metadata-token")
        .is_some_and(|t| valid(peer, generation, t, now))
    {
        return status(401);
    }
    let id = peer.id.clone();
    drop(state);
    const CREDENTIALS: &str = "/latest/meta-data/iam/security-credentials";
    let path = request.path.as_str();
    let Some(role) = (path.starts_with(CREDENTIALS)
        || matches!(
            path,
            "/latest/meta-data/iam/info"
                | "/latest/meta-data/placement/region"
                | "/latest/meta-data/placement/availability-zone"
                | "/latest/dynamic/instance-identity/document"
        ))
    .then(|| role(&id))
    .flatten() else {
        return status(404);
    };
    // Replaced while this request waited for credentials: its token named the old role.
    if role.generation != generation {
        return status(401);
    }
    let credentials = role.credentials.as_ref().expect("current credentials");
    let updated = timestamp(credentials.issued);
    match path.strip_prefix(CREDENTIALS) {
        Some("" | "/") => return ok(role.name().to_owned()),
        Some(name) if name.strip_prefix('/') == Some(role.name()) => {
            // Written straight into a wiped buffer sized up front (no reallocation leaves a
            // copy). Every value is from a JSON-safe alphabet (`Role::validate`).
            let mut body = Zeroizing::new(String::with_capacity(
                256 + credentials.access_key_id.len()
                    + credentials.secret_access_key.len()
                    + credentials.session_token.len(),
            ));
            use std::fmt::Write as _;
            let _ = write!(
                body,
                r#"{{"Code":"Success","LastUpdated":"{updated}","Type":"AWS-HMAC","AccessKeyId":"{}","SecretAccessKey":"{}","Token":"{}","Expiration":"{}"}}"#,
                credentials.access_key_id,
                credentials.secret_access_key.as_str(),
                credentials.session_token.as_str(),
                timestamp(credentials.expiration),
            );
            return Response {
                status: 200,
                body,
                ttl: None,
            };
        }
        Some(_) => return status(404),
        None => {}
    }
    match path {
        // Not a real instance profile: tools that parse `instance-profile/` get a stable name.
        "/latest/meta-data/iam/info" => ok(serde_json::json!({
            "Code": "Success",
            "LastUpdated": updated,
            "InstanceProfileArn": role.role_arn.replacen(":role/", ":instance-profile/", 1),
            "InstanceProfileId": format!("AIPAOW{}", hex(&Sha256::digest(id.as_bytes())[..8]).to_ascii_uppercase()),
        })
        .to_string()),
        "/latest/meta-data/placement/region" => role.region.map_or_else(|| status(404), ok),
        "/latest/meta-data/placement/availability-zone" => role.zone.map_or_else(|| status(404), ok),
        _ => match role.region {
            Some(region) => {
                let mut document = serde_json::json!({"region": region});
                if let Some(zone) = role.zone {
                    document["availabilityZone"] = serde_json::json!(zone);
                }
                ok(document.to_string())
            }
            None => status(404),
        },
    }
}

/// IP TTL of everything sent on this connection.
fn hop_limit(stream: &TcpStream, hops: u8) {
    use std::os::fd::AsRawFd;
    let ttl = libc::c_int::from(hops);
    unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_TTL,
            (&ttl as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

fn connection(mut stream: TcpStream, index: u32) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(IDLE));
    let mut buffer = Vec::new();
    loop {
        let (response, keep_alive) = match read_head(&mut stream, &mut buffer) {
            Ok(Some(head)) => match parse(&head) {
                Ok(request) if admit(index) => (answer(index, &request), request.keep_alive),
                Ok(request) => (status(429), request.keep_alive),
                Err(_) => (status(400), false),
            },
            Ok(None) | Err(_) => return,
        };
        let hops = {
            let state = state();
            state
                .peers
                .get(&index)
                .and_then(|p| state.roles.get(&p.id))
                .map_or(1, |r| r.hop_limit)
        };
        hop_limit(&stream, hops);
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: {}\r\n",
            response.status,
            reason(response.status),
            response.body.len(),
            if keep_alive { "keep-alive" } else { "close" },
        );
        if let Some(ttl) = response.ttl {
            head.push_str(&format!("X-aws-ec2-metadata-token-ttl-seconds: {ttl}\r\n"));
        }
        head.push_str("\r\n");
        if stream.write_all(head.as_bytes()).is_err()
            || stream.write_all(response.body.as_bytes()).is_err()
            || !keep_alive
        {
            return;
        }
    }
}

#[cfg(test)]
#[path = "metadata_tests.rs"]
mod tests;
