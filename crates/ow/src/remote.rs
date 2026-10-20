//! Single-app ingress. This process has no worker connection or management routes.
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use serde::Deserialize;
use std::{
    fs,
    net::SocketAddr,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{RwLock, Semaphore};

const REQUEST_LIMIT: usize = 1024 * 1024;
const RESPONSE_LIMIT: usize = 8 * 1024 * 1024;
const KEY_LIFETIME: Duration = Duration::from_secs(3600);
/// Terminal and SSH streams opened with a tailnet login close after this long.
const TAILNET_SESSION: Duration = Duration::from_secs(8 * 3600);

/// Who vouches for the caller: Cloudflare Access JWTs, or `tailscale serve` on this host.
#[derive(Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Auth {
    #[default]
    Access,
    Tailscale,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    hostname: String,
    #[serde(skip)]
    previews: bool,
    #[serde(default)]
    auth: Auth,
    #[serde(default)]
    team_domain: String,
    #[serde(default)]
    audience: String,
    #[serde(default)]
    allowed_emails: Vec<String>,
    #[serde(default)]
    allowed_capability: Option<String>,
    upstream: String,
    #[serde(default)]
    bootstrap_owner_email: Option<String>,
    #[serde(default)]
    node_owner_subject: Option<String>,
    #[serde(default)]
    node_owner_user_id: Option<i64>,
    /// Runtime only: tailnet user lookups (`tailscale whois`), not a config key.
    #[serde(skip)]
    whois: Whois,
}

/// Fresh local-daemon answers keep admission and revocation independent of restarts.
#[derive(Clone)]
struct Whois {
    cli: PathBuf,
}

impl Default for Whois {
    fn default() -> Self {
        Self {
            cli: PathBuf::from("tailscale"),
        }
    }
}

impl Whois {
    async fn user(&self, peer: std::net::IpAddr) -> Option<WhoisUser> {
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::new(&self.cli)
                .args(["whois", "--json", &peer.to_string()])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        if !output.status.success() {
            return None;
        }
        #[derive(Deserialize)]
        struct Answer {
            #[serde(rename = "UserProfile")]
            user: WhoisUser,
            #[serde(rename = "Node")]
            node: Node,
            #[serde(rename = "CapMap", default)]
            capabilities: Option<std::collections::HashMap<String, Option<Vec<serde_json::Value>>>>,
        }
        #[derive(Deserialize)]
        struct Node {
            #[serde(rename = "Tags", default)]
            tags: Option<Vec<String>>,
        }
        let answer: Answer = serde_json::from_slice(&output.stdout).ok()?;
        (answer.user.id != 0 && answer.node.tags.as_ref().is_none_or(Vec::is_empty)).then_some(
            WhoisUser {
                capabilities: answer.capabilities.unwrap_or_default(),
                ..answer.user
            },
        )
    }
}

#[derive(Deserialize)]
struct WhoisUser {
    #[serde(rename = "ID")]
    id: u64,
    #[serde(rename = "LoginName")]
    login: String,
    #[serde(skip)]
    capabilities: std::collections::HashMap<String, Option<Vec<serde_json::Value>>>,
}

impl Config {
    fn validate(&self) -> Result<()> {
        ensure!(valid_hostname(&self.hostname), "invalid public hostname");
        match self.auth {
            Auth::Access => {
                ensure!(
                    self.allowed_capability.is_none(),
                    "allowed_capability requires tailscale auth"
                );
                let suffix = ".cloudflareaccess.com";
                ensure!(
                    self.team_domain.ends_with(suffix)
                        && !self.team_domain.trim_end_matches(suffix).contains('.')
                        && valid_hostname(&self.team_domain),
                    "team_domain must be your single-level Cloudflare Access team domain"
                );
                ensure!(
                    !self.audience.is_empty() && self.audience.len() <= 256,
                    "missing Access audience"
                );
            }
            Auth::Tailscale => {
                ensure!(
                    self.hostname.ends_with(".ts.net") && self.hostname.split('.').count() >= 4,
                    "tailscale auth needs this machine's MagicDNS name (NAME.TAILNET.ts.net)"
                );
                ensure!(
                    self.team_domain.is_empty() && self.audience.is_empty(),
                    "team_domain and audience are Access settings; omit them with tailscale auth"
                );
            }
        }
        ensure!(
            self.allowed_emails
                .iter()
                .all(|e| e.contains('@') && e.len() <= 254),
            "invalid email allowlist"
        );
        if let Some(capability) = &self.allowed_capability {
            ensure!(
                valid_capability(capability),
                "allowed_capability must be domain/path (at most 512 characters)"
            );
        }
        ensure!(
            !self.allowed_emails.is_empty() || self.allowed_capability.is_some(),
            "an explicit email allowlist or tailnet capability is required"
        );
        if let Some(owner) = &self.bootstrap_owner_email {
            ensure!(
                self.allowed_emails
                    .iter()
                    .any(|e| e.eq_ignore_ascii_case(owner)),
                "bootstrap owner must be allowed to log in"
            );
        }
        ensure!(
            self.node_owner_subject.is_some() == self.node_owner_user_id.is_some(),
            "node owner subject and catalog ID must be configured together"
        );
        if let Some(subject) = &self.node_owner_subject {
            ensure!(
                !subject.is_empty()
                    && subject.len() <= 512
                    && self.node_owner_user_id.unwrap_or(0) > 0,
                "invalid node owner binding"
            );
        }
        let url = reqwest::Url::parse(&self.upstream)?;
        ensure!(
            url.scheme() == "http"
                && url.host_str() == Some("127.0.0.1")
                && url.port().is_some_and(|p| p != 0)
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "upstream must be an explicit http://127.0.0.1:PORT guest publication"
        );
        Ok(())
    }

    fn issuer(&self) -> String {
        match self.auth {
            Auth::Access => format!("https://{}", self.team_domain),
            // One identity namespace per tailnet: NAME.TAILNET.ts.net -> tailscale://TAILNET.ts.net
            Auth::Tailscale => format!(
                "tailscale://{}",
                self.hostname
                    .split_once('.')
                    .map_or("", |(_, tailnet)| tailnet)
            ),
        }
    }
}

fn valid_hostname(host: &str) -> bool {
    host.len() <= 253
        && host.contains('.')
        && host.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !part.starts_with('-')
                && !part.ends_with('-')
        })
}

fn valid_capability(capability: &str) -> bool {
    capability.len() <= 512
        && capability.split_once('/').is_some_and(|(domain, path)| {
            valid_hostname(domain)
                && !path.is_empty()
                && path
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
        })
}

struct Keys {
    set: JwkSet,
    fetched: Instant,
}

#[derive(Clone)]
struct App {
    dashboard: Option<PathBuf>,
    catalog: Option<Arc<std::sync::Mutex<crate::catalog::Catalog>>>,
    config: Config,
    client: reqwest::Client,
    keys: Arc<RwLock<Keys>>,
    requests: Arc<Semaphore>,
}

#[derive(Deserialize)]
struct Claims {
    exp: u64,
    email: String,
    sub: String,
}

async fn fetch_keys(client: &reqwest::Client, config: &Config) -> Result<JwkSet> {
    let mut response = client
        .get(format!("{}/cdn-cgi/access/certs", config.issuer()))
        .send()
        .await?
        .error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= REQUEST_LIMIT,
            "oversized Access key response"
        );
        bytes.extend_from_slice(&chunk);
    }
    let set: JwkSet = serde_json::from_slice(&bytes)?;
    ensure!(!set.keys.is_empty(), "Access returned no signing keys");
    Ok(set)
}

/// `tailscale serve` replaces client-sent Tailscale-User-* and X-Forwarded-For
/// headers: it sets the login only for tailnet users (never tagged devices) and
/// X-Forwarded-For to the real peer address. The login is a reusable name, so
/// the catalog subject is the stable user ID that `tailscale whois` reports for
/// that peer, and both must agree. The listener is loopback-only, so these
/// headers are trusted as far as this host's local users are.
async fn tailnet_identity(
    config: &Config,
    headers: &HeaderMap,
) -> Option<(u64, crate::catalog::Identity)> {
    let single = |name: &str| {
        let mut values = headers.get_all(name).iter();
        let value = values.next()?.to_str().ok()?;
        (values.next().is_none() && !value.is_empty() && value.len() <= 254).then_some(value)
    };
    let login = single("tailscale-user-login")?;
    let peer: std::net::IpAddr = single("x-forwarded-for")?.parse().ok()?;
    let user = config.whois.user(peer).await?;
    if !user.login.eq_ignore_ascii_case(login) {
        return None;
    }
    let email_allowed = config
        .allowed_emails
        .iter()
        .any(|email| email.eq_ignore_ascii_case(login));
    let capability_allowed = config
        .allowed_capability
        .as_ref()
        .is_some_and(|capability| user.capabilities.contains_key(capability));
    if !email_allowed && !capability_allowed {
        return None;
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some((
        now + TAILNET_SESSION.as_secs(),
        crate::catalog::Identity {
            issuer: config.issuer(),
            subject: user.id.to_string(),
            email: login.to_owned(),
        },
    ))
}

/// Recheck long-lived connections too; a new owner of the same login is a new session.
#[derive(Clone)]
pub(crate) struct TailnetSession {
    config: Config,
    headers: HeaderMap,
    subject: String,
}

impl TailnetSession {
    pub(crate) async fn revoked(session: Option<Self>) {
        let Some(session) = session else {
            return std::future::pending().await;
        };
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            match tailnet_identity(&session.config, &session.headers).await {
                Some((_, identity)) if identity.subject == session.subject => {}
                _ => return,
            }
        }
    }
}

async fn authorized_until(
    app: &App,
    headers: &HeaderMap,
) -> Option<(u64, crate::catalog::Identity)> {
    if app.config.auth == Auth::Tailscale {
        return tailnet_identity(&app.config, headers).await;
    }
    let mut assertions = headers.get_all("cf-access-jwt-assertion").iter();
    let token = assertions.next().and_then(|v| v.to_str().ok())?;
    if assertions.next().is_some() || token.len() > 16384 {
        return None;
    }
    let Ok(header) = decode_header(token) else {
        return None;
    };
    if header.alg != Algorithm::RS256 {
        return None;
    }
    let kid = header.kid?;
    let keys = app.keys.read().await;
    if keys.fetched.elapsed() >= KEY_LIFETIME {
        return None;
    }
    let jwk = keys.set.find(&kid)?;
    let Ok(key) = DecodingKey::from_jwk(jwk) else {
        return None;
    };
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[app.config.issuer()]);
    validation.set_audience(&[&app.config.audience]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.validate_nbf = true;
    validation.leeway = 0;
    let Ok(token) = decode::<Claims>(token, &key, &validation) else {
        return None;
    };
    let allowed = !token.claims.sub.is_empty()
        && app
            .config
            .allowed_emails
            .iter()
            .any(|email| email.eq_ignore_ascii_case(&token.claims.email));
    allowed.then_some((
        token.claims.exp,
        crate::catalog::Identity {
            issuer: app.config.issuer(),
            subject: token.claims.sub,
            email: token.claims.email,
        },
    ))
}

// Hop-by-hop headers and credentials must never enter a guest or escape from it.
pub(crate) fn clean_headers(source: &HeaderMap, request: bool) -> HeaderMap {
    let nominated: Vec<String> = source
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|v| v.trim().to_ascii_lowercase())
        .collect();
    let mut clean = HeaderMap::new();
    for (name, value) in source {
        let key = name.as_str();
        if nominated.iter().any(|n| n == key)
            || matches!(
                key,
                "connection"
                    | "keep-alive"
                    | "proxy-authenticate"
                    | "proxy-authorization"
                    | "te"
                    | "trailer"
                    | "transfer-encoding"
                    | "upgrade"
                    | "host"
                    | "content-length"
                    | "forwarded"
                    | "authorization"
            )
            || key.starts_with("cf-")
            || key.starts_with("tailscale-")
            || key.starts_with("x-forwarded-")
        {
            continue;
        }
        if request && key == "cookie" {
            if let Ok(cookies) = value.to_str() {
                let cookies = cookies
                    .split(';')
                    .map(str::trim)
                    .filter(|c| {
                        let key = c.trim().split('=').next().unwrap_or("");
                        !key.eq_ignore_ascii_case("CF_Authorization")
                            && !key.eq_ignore_ascii_case("CF_AppSession")
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                if !cookies.trim().is_empty()
                    && let Ok(value) = HeaderValue::from_str(&cookies)
                {
                    clean.append(name, value);
                }
            }
        } else if !request
            && key == "set-cookie"
            && value.to_str().is_ok_and(|v| {
                let key = v.split('=').next().unwrap_or("").trim();
                key.eq_ignore_ascii_case("CF_Authorization")
                    || key.eq_ignore_ascii_case("CF_AppSession")
            })
        {
            continue;
        } else {
            clean.append(name, value.clone());
        }
    }
    clean
}

async fn proxy(State(app): State<App>, request: Request) -> Response {
    let Ok(_permit) = app.requests.clone().try_acquire_owned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Gateway busy").into_response();
    };
    // Operations may be captures that take minutes (wire::reply_wait); the rest are quick.
    let wait = if request.uri().path() == "/api/operation" {
        1260
    } else {
        45
    };
    match tokio::time::timeout(Duration::from_secs(wait), handle(&app, request)).await {
        Ok(response) => response,
        Err(_) => (StatusCode::GATEWAY_TIMEOUT, "Request timed out").into_response(),
    }
}

async fn handle(app: &App, mut request: Request) -> Response {
    let host = request
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let preview_port = host
        .strip_prefix(&format!("{}:", app.config.hostname))
        .and_then(|p| p.parse::<u16>().ok())
        .filter(|p| *p >= 11000);
    if preview_port.is_none()
        && host != app.config.hostname
        && host != format!("{}:443", app.config.hostname)
    {
        return (StatusCode::MISDIRECTED_REQUEST, "Unknown hostname").into_response();
    }
    // Per-box AWS access: AWS STS fetches the issuer's public keys without signing in.
    if let Some(root) = &app.dashboard
        && preview_port.is_none()
        && matches!(*request.method(), Method::GET | Method::HEAD)
        && request.uri().query().is_none()
        && let Some(document) = crate::aws_identity::public_document(root, request.uri().path())
    {
        return (
            [
                ("content-type", "application/json"),
                ("cache-control", "public, max-age=300"),
                ("x-content-type-options", "nosniff"),
            ],
            document.to_string(),
        )
            .into_response();
    }
    // The CLI signs in through Cloudflare Access only, so tailnet mode offers no installer.
    if app.dashboard.is_some()
        && app.config.auth == Auth::Access
        && crate::dashboard::public_path(request.uri().path())
    {
        return crate::dashboard::public_download(
            &format!("https://{}", app.config.hostname),
            request,
        )
        .await;
    }
    let Some((expires, identity)) = authorized_until(app, request.headers()).await else {
        let login = match app.config.auth {
            Auth::Access => "Cloudflare Access login required",
            Auth::Tailscale => {
                "Tailnet login required: open this through tailscale serve as an allowed user"
            }
        };
        return (StatusCode::UNAUTHORIZED, login).into_response();
    };
    if app.config.auth == Auth::Tailscale {
        let session = TailnetSession {
            config: app.config.clone(),
            headers: request.headers().clone(),
            subject: identity.subject.clone(),
        };
        request.extensions_mut().insert(session);
    }
    if let Some(port) = preview_port {
        if !app.config.previews || app.config.auth != Auth::Tailscale || app.dashboard.is_none() {
            return StatusCode::FORBIDDEN.into_response();
        }
        let wake = match app.catalog.clone() {
            Some(catalog) => {
                let identity = identity.clone();
                tokio::task::spawn_blocking(move || {
                    let name = {
                        let mut c = catalog.lock().unwrap();
                        let user = c.user(&identity)?;
                        c.preview_name(user, port)?
                    };
                    crate::transfers::ensure_awake(catalog, &identity, &name, "preview", false)
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r)
            }
            None => Err(anyhow::anyhow!("ownership catalog unavailable")),
        };
        if let Err(error) = wake {
            if error
                .downcast_ref::<crate::catalog::OperationFailure>()
                .is_some()
            {
                return wake_error(error);
            }
            return StatusCode::FORBIDDEN.into_response();
        }
        let target = crate::dashboard::with_catalog(app.catalog.clone(), identity, move |c, u| {
            c.preview_target(u, port)
        })
        .await;
        let Ok(target) = target else {
            return StatusCode::FORBIDDEN.into_response();
        };
        return crate::preview::proxy(target, request, expires, &host).await;
    }
    if app.dashboard.is_some() && request.uri().path().starts_with("/api/ssh/") {
        if request
            .headers()
            .get("origin")
            .and_then(|v| v.to_str().ok())
            != Some(format!("https://{}", app.config.hostname).as_str())
        {
            return (StatusCode::FORBIDDEN, "Same-origin SSH required").into_response();
        }
        let name = request
            .uri()
            .path()
            .strip_prefix("/api/ssh/")
            .unwrap()
            .to_owned();
        if request.uri().query().is_some() || crate::common::address(&name).is_err() {
            return (StatusCode::BAD_REQUEST, "Invalid SSH target").into_response();
        }
        if let Err(error) =
            wake_connection(app.catalog.clone(), identity.clone(), name.clone(), "ssh").await
        {
            return wake_error(error);
        }
        let target = crate::dashboard::with_catalog(
            app.catalog.clone(),
            identity.clone(),
            move |catalog, user| {
                let (root, id, revoked) = catalog.ssh_target(user, &name)?;
                let placement = catalog.managed_placement(&id)?;
                Ok((root, id, revoked, placement))
            },
        )
        .await;
        let Ok((root, id, revoked, placement)) = target else {
            return (
                StatusCode::NOT_FOUND,
                "Machine not found or SSH node unavailable",
            )
                .into_response();
        };
        return crate::ssh_gateway::upgrade(&root, request, expires, id, placement, revoked).await;
    }
    if let Some(_root) = &app.dashboard
        && request.uri().path().starts_with("/api/terminal/")
    {
        if request
            .headers()
            .get("origin")
            .and_then(|v| v.to_str().ok())
            != Some(format!("https://{}", app.config.hostname).as_str())
        {
            return (StatusCode::FORBIDDEN, "Same-origin terminal required").into_response();
        }
        let name = request
            .uri()
            .path()
            .trim_start_matches("/api/terminal/")
            .to_owned();
        if request.uri().query().is_some() || crate::common::address(&name).is_err() {
            return (StatusCode::BAD_REQUEST, "Invalid terminal target").into_response();
        }
        if let Err(error) = wake_connection(
            app.catalog.clone(),
            identity.clone(),
            name.clone(),
            "terminal",
        )
        .await
        {
            return wake_error(error);
        }
        let id = crate::dashboard::with_catalog(
            app.catalog.clone(),
            identity.clone(),
            move |catalog, user| {
                let (root, id, revoked) = catalog.terminal_target(user, &name)?;
                let placement = catalog.managed_placement(&id)?;
                Ok((root, id, revoked, placement))
            },
        )
        .await;
        let Ok((target_root, id, revoked, placement)) = id else {
            return (StatusCode::NOT_FOUND, "Machine not found").into_response();
        };
        return crate::terminal::upgrade(&target_root, request, expires, id, placement, revoked)
            .await;
    }
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) && request
        .headers()
        .get("origin")
        .and_then(|h| h.to_str().ok())
        != Some(format!("https://{}", app.config.hostname).as_str())
    {
        return (StatusCode::FORBIDDEN, "Same-origin request required").into_response();
    }
    if request.headers().contains_key("upgrade")
        || request.method() == Method::CONNECT
        || request.method() == Method::TRACE
    {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "Streaming upgrades are not supported yet",
        )
            .into_response();
    }
    if request.uri().path().starts_with("/api/hosts/") && app.dashboard.is_some() {
        return host_admin(app, identity, request).await;
    }
    if app.dashboard.is_some() {
        // Owner controls only shape /api/state. Checking them takes the catalog lock, which a
        // slow worker call can hold, so other paths (unshare above all) must not wait for it.
        let controls = request.uri().path() == "/api/state" && owner_controls(app, &identity).await;
        return crate::dashboard::handle(
            app.catalog.clone(),
            app.dashboard.clone(),
            identity,
            request,
            controls,
            (app.config.previews && app.config.auth == Auth::Tailscale)
                .then(|| app.config.hostname.clone()),
        )
        .await;
    }
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    // Reject authority-form/absolute-form requests instead of accepting an SSRF target.
    if parts.uri.scheme().is_some() || parts.uri.authority().is_some() {
        return (StatusCode::BAD_REQUEST, "Invalid request target").into_response();
    }
    let Ok(bytes) = to_bytes(body, REQUEST_LIMIT).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "Request body too large").into_response();
    };
    let mut headers = clean_headers(&parts.headers, true);
    headers.insert("host", HeaderValue::from_str(&app.config.hostname).unwrap());
    let mut upstream = match app
        .client
        .request(
            parts.method.clone(),
            format!("{}{}", app.config.upstream.trim_end_matches('/'), path),
        )
        .headers(headers)
        .body(bytes)
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => {
            return (StatusCode::BAD_GATEWAY, "Workspace service unavailable").into_response();
        }
    };
    let status = upstream.status();
    let mut headers = clean_headers(upstream.headers(), false);
    // Never cache a response carrying an authenticated workspace capability.
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    let mut bytes = Vec::new();
    loop {
        match upstream.chunk().await {
            Ok(Some(chunk)) if bytes.len() + chunk.len() <= RESPONSE_LIMIT => {
                bytes.extend_from_slice(&chunk)
            }
            Ok(None) => break,
            _ => {
                return (
                    StatusCode::BAD_GATEWAY,
                    "Workspace response unavailable or too large",
                )
                    .into_response();
            }
        }
    }
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

fn designated_owner(app: &App, identity: &crate::catalog::Identity) -> bool {
    identity.issuer == app.config.issuer()
        && app.config.node_owner_subject.as_deref() == Some(identity.subject.as_str())
        && app.config.node_owner_user_id.is_some()
}
async fn owner_controls(app: &App, identity: &crate::catalog::Identity) -> bool {
    if !designated_owner(app, identity) {
        return false;
    }
    let Some(catalog) = app.catalog.clone() else {
        return false;
    };
    let identity = identity.clone();
    let expected = app.config.node_owner_user_id.unwrap();
    tokio::task::spawn_blocking(move || {
        catalog
            .lock()
            .ok()
            .is_some_and(|c| c.bound_identity(expected, &identity).unwrap_or(false))
    })
    .await
    .unwrap_or(false)
}
async fn host_admin(app: &App, identity: crate::catalog::Identity, request: Request) -> Response {
    use serde_json::{Value, json};
    // Identity comes only from verified JWT claims. Never bind authority by email.
    if !owner_controls(app, &identity).await {
        return StatusCode::FORBIDDEN.into_response();
    }
    let path = request.uri().path().to_owned();
    if request.uri().query().is_some()
        || request.method() != Method::POST
        || request
            .headers()
            .get("x-ow-request")
            .and_then(|h| h.to_str().ok())
            != Some("dashboard")
        || request
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            != Some("application/json")
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(bytes) = to_bytes(request.into_body(), 4096).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        node: String,
        ttl: Option<u64>,
        memory: Option<u32>,
        slots: Option<u32>,
        cpus: Option<u32>,
    }
    let Ok(raw) = serde_json::from_slice::<Value>(&bytes) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if ["memory", "slots", "cpus", "ttl"]
        .iter()
        .any(|key| raw.get(key).is_some_and(|v| v.as_u64().is_none()))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(input) = serde_json::from_value::<Input>(raw) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let root = app.dashboard.clone().unwrap();
    let controller = format!("https://{}/_nodes", app.config.hostname);
    let catalog = app.catalog.clone().context("catalog").unwrap();
    let result = tokio::task::spawn_blocking(move || -> Result<Value> {
        let _admission = catalog
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog lock"))?;
        match path.as_str() {
            "/api/hosts/invite" => crate::nodes::invitation(
                &root,
                &input.node,
                input.ttl.unwrap_or(600),
                input.memory.unwrap_or(512),
                input.slots.unwrap_or(2),
                input.cpus.unwrap_or(2),
                Some(&controller),
                |_| Ok(()),
            ),
            "/api/hosts/budget" => {
                ensure!(input.ttl.is_none(), "budget does not accept expiry");
                crate::nodes::budget(
                    &root,
                    &input.node,
                    input.memory.context("memory required")?,
                    input.slots.context("slots required")?,
                    input.cpus.context("cpus required")?,
                )
            }
            "/api/hosts/revoke" => {
                ensure!(
                    input.ttl.is_none()
                        && input.memory.is_none()
                        && input.slots.is_none()
                        && input.cpus.is_none(),
                    "invalid revoke"
                );
                crate::common::identifier(&input.node)?;
                crate::nodes::revoke(&root, &input.node)?;
                Ok(json!({"revoked":input.node}))
            }
            _ => anyhow::bail!("unknown host operation"),
        }
    })
    .await;
    let mut response=match result { Ok(Ok(value)) => axum::Json(json!({"ok":true,"result":value})).into_response(), Ok(Err(error)) if error.is::<crate::limits::Rejection>() => (StatusCode::CONFLICT, axum::Json(json!({"ok":false,"error":error.to_string()}))).into_response(), _ => (StatusCode::CONFLICT,axum::Json(json!({"ok":false,"error":"Host operation rejected; check node ID, expiry and capacity."}))).into_response() };
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
}

fn router(app: App) -> Router {
    Router::new().fallback(proxy).with_state(app)
}

async fn wake_connection(
    catalog: Option<Arc<std::sync::Mutex<crate::catalog::Catalog>>>,
    identity: crate::catalog::Identity,
    name: String,
    permission: &'static str,
) -> Result<()> {
    let catalog = catalog.context("ownership catalog unavailable")?;
    tokio::task::spawn_blocking(move || {
        crate::transfers::ensure_awake(catalog, &identity, &name, permission, false)
    })
    .await??;
    Ok(())
}
fn wake_error(error: anyhow::Error) -> Response {
    if let Some(operation) = error.downcast_ref::<crate::catalog::OperationFailure>() {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"ok":false,"operation":operation.0})),
        )
            .into_response()
    } else {
        (
            StatusCode::FORBIDDEN,
            "Workspace unavailable; stopped boxes require explicit start.",
        )
            .into_response()
    }
}

pub fn run(path: &Path, listen: SocketAddr, dashboard: Option<PathBuf>) -> Result<()> {
    ensure!(
        listen.ip().is_loopback(),
        "authenticated gateway must bind to loopback"
    );
    let metadata = fs::metadata(path)?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "gateway config must be an owner-only file (chmod 600)"
    );
    let mut config: Config = serde_json::from_slice(&fs::read(path)?)?;
    config.previews = config.auth == Auth::Tailscale
        && dashboard.is_some()
        && Path::new("/usr/local/lib/open-workspaces/preview-serve").is_file();
    config.validate()?;
    let catalog = dashboard
        .as_ref()
        .map(|root| {
            crate::catalog::Catalog::open(
                root,
                &config.issuer(),
                config.bootstrap_owner_email.as_deref(),
            )
        })
        .transpose()?
        .map(|c| Arc::new(std::sync::Mutex::new(c)));
    if let Some(catalog) = &catalog {
        crate::transfers::recover(catalog);
        crate::scaler::start(catalog);
        crate::aws_identity::start(catalog);
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(async {
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()?;
            let access = config.auth == Auth::Access;
            let keys = if access {
                fetch_keys(&client, &config)
                    .await
                    .context("loading Cloudflare Access signing keys")?
            } else {
                JwkSet { keys: Vec::new() }
            };
            let app = App {
                dashboard: dashboard.clone(),
                catalog,
                config,
                client,
                keys: Arc::new(RwLock::new(Keys {
                    set: keys,
                    fetched: Instant::now(),
                })),
                requests: Arc::new(Semaphore::new(64)),
            };
            let refresh = app.clone();
            // Tailnet sign-in uses no signing keys, so there is nothing to refresh.
            tokio::spawn(async move {
                if !access {
                    return;
                }
                loop {
                    tokio::time::sleep(Duration::from_secs(600)).await;
                    if let Ok(set) = fetch_keys(&refresh.client, &refresh.config).await {
                        *refresh.keys.write().await = Keys {
                            set,
                            fetched: Instant::now(),
                        };
                    } else {
                        eprintln!(
                            "Access key refresh failed; expired cached keys will deny requests"
                        );
                    }
                }
            });
            let listener = tokio::net::TcpListener::bind(listen).await?;
            println!(
                "Authenticated workspace gateway listening on http://{}",
                listener.local_addr()?
            );
            axum::serve(listener, router(app))
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
            Ok(())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::{Value, json};
    use std::{
        io::Write,
        process::{Command, Stdio},
        sync::atomic::{AtomicUsize, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };
    use tower::ServiceExt;

    include!("ssh_e2e.rs");
    fn signing_key() -> (EncodingKey, JwkSet) {
        // Ephemeral test material: no checked-in private key or Cloudflare credential.
        let generated = Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
            ])
            .output()
            .unwrap();
        assert!(generated.status.success());
        let mut child = Command::new("openssl")
            .args(["rsa", "-noout", "-modulus"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&generated.stdout)
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let modulus = String::from_utf8(output.stdout).unwrap();
        let modulus = modulus.trim().strip_prefix("Modulus=").unwrap();
        let bytes: Vec<u8> = (0..modulus.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&modulus[i..i + 2], 16).unwrap())
            .collect();
        let keys = serde_json::from_value(json!({"keys":[{"kty":"RSA","kid":"test-key","alg":"RS256","use":"sig","n":URL_SAFE_NO_PAD.encode(bytes),"e":"AQAB"}]})).unwrap();
        (EncodingKey::from_rsa_pem(&generated.stdout).unwrap(), keys)
    }

    fn config(upstream: String) -> Config {
        Config {
            hostname: "app.example.test".into(),
            auth: Auth::Access,
            team_domain: "test-team.cloudflareaccess.com".into(),
            audience: "test-audience".into(),
            allowed_emails: vec!["owner@example.test".into()],
            allowed_capability: None,
            upstream,
            bootstrap_owner_email: None,
            node_owner_subject: None,
            node_owner_user_id: None,
            previews: false,
            whois: Whois::default(),
        }
    }

    fn claims() -> Value {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        json!({"iss":"https://test-team.cloudflareaccess.com", "aud":["test-audience"],
            "sub":"test-subject", "email":"owner@example.test", "exp":now+300, "nbf":now-1})
    }

    fn token(key: &EncodingKey, claims: &Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-key".into());
        encode(&header, claims, key).unwrap()
    }

    fn request(jwt: Option<&str>, method: Method, body: Body) -> Request {
        let mut request = Request::builder()
            .method(method)
            .uri("/counter?sample=1")
            .header("host", "app.example.test");
        if let Some(jwt) = jwt {
            request = request.header("cf-access-jwt-assertion", jwt);
        }
        request.body(body).unwrap()
    }

    #[test]
    fn refuses_broad_or_ambiguous_configuration() {
        let mut cfg = config("http://127.0.0.1:1234".into());
        assert!(cfg.validate().is_ok());
        for upstream in [
            "http://localhost:1234",
            "http://192.168.1.1:1234",
            "http://127.0.0.1:1234/admin",
            "http://user@127.0.0.1:1234",
            "http://127.0.0.1:0",
        ] {
            cfg.upstream = upstream.into();
            assert!(cfg.validate().is_err());
        }
        cfg = config("http://127.0.0.1:1234".into());
        cfg.allowed_emails.clear();
        assert!(cfg.validate().is_err());
        cfg = config("http://127.0.0.1:1234".into());
        cfg.team_domain = "attacker.test-team.cloudflareaccess.com".into();
        assert!(cfg.validate().is_err());
    }

    pub(super) fn tailnet_config() -> Config {
        // Stand-in `tailscale whois --json ADDR`: two tailnet users, nothing else.
        let dir =
            std::env::temp_dir().join(format!("ow-whois-test-{}", crate::common::nonce().unwrap()));
        fs::create_dir_all(&dir).unwrap();
        let cli = dir.join("tailscale");
        fs::write(
            &cli,
            "#!/bin/sh\ncase \"$3\" in\n\
             100.64.0.1) echo '{\"Node\":{},\"UserProfile\":{\"ID\":42,\"LoginName\":\"Owner@example.test\"}}' ;;\n\
             100.64.0.2) echo '{\"Node\":{},\"UserProfile\":{\"ID\":43,\"LoginName\":\"stranger@example.test\"}}' ;;\n\
             *) exit 1 ;;\nesac\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o755)).unwrap();
        Config {
            hostname: "box-host.example-net.ts.net".into(),
            auth: Auth::Tailscale,
            team_domain: String::new(),
            audience: String::new(),
            whois: Whois { cli },
            ..config("http://127.0.0.1:1234".into())
        }
    }

    #[test]
    fn tailnet_configuration_is_strict() {
        let cfg = tailnet_config();
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.issuer(), "tailscale://example-net.ts.net");
        let mut bad = tailnet_config();
        bad.hostname = "app.example.test".into();
        assert!(bad.validate().is_err());
        bad = tailnet_config();
        bad.hostname = "example-net.ts.net".into();
        assert!(bad.validate().is_err());
        bad = tailnet_config();
        bad.audience = "test-audience".into();
        assert!(bad.validate().is_err());
        bad = tailnet_config();
        bad.allowed_emails.clear();
        assert!(bad.validate().is_err());
        bad.allowed_capability = Some("example.test/cap/open-workspaces".into());
        assert!(bad.validate().is_ok());
        let parsed: Config = serde_json::from_value(json!({
            "auth":"tailscale", "hostname":"box.example-net.ts.net",
            "allowed_capability":"example.test/cap/open-workspaces", "upstream":"http://127.0.0.1:9/"
        })).unwrap();
        assert!(parsed.validate().is_ok());
        for capability in [
            "",
            "cap",
            "example.test/",
            "https://example.test/cap",
            "example.test/cap\nX=1",
            "-bad.test/cap",
        ] {
            bad.allowed_capability = Some(capability.into());
            assert!(bad.validate().is_err(), "{capability:?}");
        }
        bad.allowed_capability = Some("example.test/cap/open-workspaces".into());
        bad.bootstrap_owner_email = Some("owner@example.test".into());
        assert!(
            bad.validate().is_err(),
            "legacy migration still needs an explicitly allowed owner"
        );
        // Access mode still needs its own settings.
        let mut access = config("http://127.0.0.1:1234".into());
        access.allowed_capability = Some("example.test/cap/open-workspaces".into());
        assert!(access.validate().is_err());
        access.allowed_capability = None;
        access.audience.clear();
        assert!(access.validate().is_err());
    }

    pub(super) fn capability_config() -> Config {
        use std::os::unix::fs::PermissionsExt;
        let mut cfg = tailnet_config();
        cfg.allowed_emails.clear();
        cfg.allowed_capability = Some("example.test/cap/open-workspaces".into());
        fs::write(
            &cfg.whois.cli,
            "#!/bin/sh\ncat \"$(dirname \"$0\")/$3.json\"\n",
        )
        .unwrap();
        fs::set_permissions(&cfg.whois.cli, fs::Permissions::from_mode(0o755)).unwrap();
        cfg
    }

    fn whois_answer(cfg: &Config, answer: Value) {
        fs::write(
            cfg.whois.cli.parent().unwrap().join("100.64.0.1.json"),
            answer.to_string(),
        )
        .unwrap();
    }

    pub(super) fn capability_answer() -> Value {
        json!({"Node":{"Tags":[]}, "UserProfile":{"ID":42,"LoginName":"owner@example.test"},
            "CapMap":{"example.test/cap/open-workspaces":[{}]}})
    }

    fn tailnet_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "tailscale-user-login",
            "owner@example.test".parse().unwrap(),
        );
        headers.insert("x-forwarded-for", "100.64.0.1".parse().unwrap());
        headers
    }

    #[tokio::test]
    async fn tailnet_capability_admission_denial_and_revocation() {
        let cfg = capability_config();
        let gateway = router(App {
            dashboard: Some(PathBuf::from("/tmp/ow-no-worker-test")),
            catalog: None,
            config: cfg.clone(),
            client: reqwest::Client::new(),
            keys: Arc::new(RwLock::new(Keys {
                set: JwkSet { keys: Vec::new() },
                fetched: Instant::now(),
            })),
            requests: Arc::new(Semaphore::new(64)),
        });
        let request = || {
            let mut req = Request::builder()
                .uri("/")
                .header("host", &cfg.hostname)
                .body(Body::empty())
                .unwrap();
            req.headers_mut().extend(tailnet_headers());
            // Capabilities in caller-supplied headers have no authority.
            req.headers_mut().insert(
                "tailscale-app-capabilities",
                r#"{"example.test/cap/open-workspaces":[{}]}"#.parse().unwrap(),
            );
            req
        };
        whois_answer(&cfg, capability_answer());
        assert_eq!(
            gateway.clone().oneshot(request()).await.unwrap().status(),
            StatusCode::OK
        );
        let (_, owner) = tailnet_identity(&cfg, &tailnet_headers()).await.unwrap();
        assert_eq!(owner.subject, "42");
        assert_eq!(owner.issuer, "tailscale://example-net.ts.net");
        // Tailscale's PeerCapMap permits grants with no argument values (nil slices).
        for values in [json!([]), Value::Null] {
            let mut answer = capability_answer();
            answer["CapMap"]["example.test/cap/open-workspaces"] = values;
            whois_answer(&cfg, answer);
            assert!(tailnet_identity(&cfg, &tailnet_headers()).await.is_some());
        }
        // Revocation affects the same running gateway and same headers, without a restart.
        for answer in [
            json!({"Node":{}, "UserProfile":{"ID":42,"LoginName":"owner@example.test"}}),
            json!({"Node":{"CapMap":{"example.test/cap/open-workspaces":[{}]}}, "UserProfile":{"ID":42,"LoginName":"owner@example.test"}, "CapMap":{}}),
            {
                let mut a = capability_answer();
                a["CapMap"] = json!({"other.test/cap/open-workspaces":[{}]});
                a
            },
            {
                let mut a = capability_answer();
                a["Node"]["Tags"] = json!(["tag:device"]);
                a
            },
            {
                let mut a = capability_answer();
                a["UserProfile"]["LoginName"] = json!("other@example.test");
                a
            },
            {
                let mut a = capability_answer();
                a["UserProfile"]["ID"] = json!(0);
                a
            },
            {
                let mut a = capability_answer();
                a["Node"] = Value::Null;
                a
            },
            {
                let mut a = capability_answer();
                a["CapMap"] = json!("invalid");
                a
            },
            {
                let mut a = capability_answer();
                a["CapMap"]["example.test/cap/open-workspaces"] = json!(true);
                a
            },
        ] {
            whois_answer(&cfg, answer);
            assert_eq!(
                gateway.clone().oneshot(request()).await.unwrap().status(),
                StatusCode::UNAUTHORIZED
            );
        }
        whois_answer(&cfg, capability_answer());
        assert_eq!(
            gateway.clone().oneshot(request()).await.unwrap().status(),
            StatusCode::OK
        );
        let mut reassigned = capability_answer();
        reassigned["UserProfile"]["ID"] = json!(99);
        whois_answer(&cfg, reassigned);
        let (_, replacement) = tailnet_identity(&cfg, &tailnet_headers()).await.unwrap();
        assert_ne!(owner.subject, replacement.subject);
        // Email admission remains an independent fallback, but never admits tagged devices.
        let mut fallback = cfg.clone();
        fallback.allowed_emails.push(owner.email);
        let mut answer = capability_answer();
        answer["CapMap"] = Value::Null;
        whois_answer(&cfg, answer.clone());
        assert!(
            tailnet_identity(&fallback, &tailnet_headers())
                .await
                .is_some()
        );
        answer["Node"]["Tags"] = json!(["tag:device"]);
        whois_answer(&cfg, answer);
        assert!(
            tailnet_identity(&fallback, &tailnet_headers())
                .await
                .is_none()
        );
        fs::remove_file(cfg.whois.cli.parent().unwrap().join("100.64.0.1.json")).unwrap();
        assert!(tailnet_identity(&cfg, &tailnet_headers()).await.is_none());
        fs::remove_dir_all(cfg.whois.cli.parent().unwrap()).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tailnet_revocation_closes_an_owned_live_terminal() {
        use std::os::unix::fs::PermissionsExt;
        let mut cfg = capability_config();
        whois_answer(&cfg, capability_answer());
        let root = cfg.whois.cli.parent().unwrap().join("data");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(root.join("control.sock")).unwrap();
        let worker = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let req = crate::wire::line(&mut stream).unwrap();
                let result = match req["op"].as_str().unwrap() {
                    "list" => {
                        json!([{"id":"legacy","state":"running","memory_mib":256,"vcpu_count":1,"image":"alpine"}])
                    }
                    "snapshots" => json!([]),
                    "terminal" => json!({}),
                    other => panic!("unexpected worker call: {other}"),
                };
                crate::wire::send(&mut stream, &json!({"ok":true,"result":result})).unwrap();
                if req["op"] == "terminal" {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(40)))
                        .unwrap();
                    let mut sink = [0; 4096];
                    while std::io::Read::read(&mut stream, &mut sink).is_ok_and(|n| n > 0) {}
                }
            }
        });
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("localhost:{}", tcp.local_addr().unwrap().port());
        cfg.hostname = host.clone();
        let mut catalog =
            crate::catalog::Catalog::open(&root, &cfg.issuer(), Some("owner@example.test"))
                .unwrap();
        let (_, identity) = tailnet_identity(&cfg, &tailnet_headers()).await.unwrap();
        let owner = catalog.user(&identity).unwrap();
        assert!(catalog.terminal_id(owner, "legacy").is_ok());
        // Another capability-bearing user cannot inherit the first user's resources.
        let mut other = identity.clone();
        other.subject = "43".into();
        other.email = "other@example.test".into();
        let other = catalog.user(&other).unwrap();
        assert!(catalog.terminal_id(other, "legacy").is_err());
        let gateway = router(App {
            dashboard: Some(root.clone()),
            catalog: Some(Arc::new(std::sync::Mutex::new(catalog))),
            config: cfg.clone(),
            client: reqwest::Client::new(),
            keys: Arc::new(RwLock::new(Keys {
                set: JwkSet { keys: Vec::new() },
                fetched: Instant::now(),
            })),
            requests: Arc::new(Semaphore::new(64)),
        });
        let server = tokio::spawn(async move { axum::serve(tcp, gateway).await });
        let socket = tokio::task::spawn_blocking(move || {
            use tungstenite::client::IntoClientRequest;
            let mut request = format!("ws://{host}/api/terminal/legacy")
                .into_client_request()
                .unwrap();
            request.headers_mut().extend(tailnet_headers());
            request
                .headers_mut()
                .insert("origin", format!("https://{host}").parse().unwrap());
            let (socket, _) = tungstenite::connect(request).unwrap();
            socket
        })
        .await
        .unwrap();
        let mut revoked = capability_answer();
        revoked["CapMap"] = json!({});
        whois_answer(&cfg, revoked);
        assert!(tailnet_identity(&cfg, &tailnet_headers()).await.is_none());
        tokio::time::timeout(
            Duration::from_secs(36),
            tokio::task::spawn_blocking(move || {
                let mut socket = socket;
                if let tungstenite::stream::MaybeTlsStream::Plain(stream) = socket.get_mut() {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(35)))
                        .unwrap();
                }
                loop {
                    match socket.read() {
                        Ok(tungstenite::Message::Close(_)) => break,
                        Ok(_) => {}
                        Err(
                            tungstenite::Error::ConnectionClosed
                            | tungstenite::Error::AlreadyClosed,
                        ) => break,
                        Err(error) => {
                            panic!("stream did not close cleanly after revocation: {error}")
                        }
                    }
                }
            }),
        )
        .await
        .unwrap()
        .unwrap();
        server.abort();
        worker.join().unwrap();
        fs::remove_dir_all(cfg.whois.cli.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn tailnet_login_gates_the_dashboard() {
        let (key, _) = signing_key();
        let gateway = router(App {
            dashboard: Some(PathBuf::from("/tmp/ow-no-worker-test")),
            catalog: None,
            config: tailnet_config(),
            client: reqwest::Client::new(),
            keys: Arc::new(RwLock::new(Keys {
                set: JwkSet { keys: Vec::new() },
                fetched: Instant::now(),
            })),
            requests: Arc::new(Semaphore::new(64)),
        });
        let get_from = |logins: &[&str], peers: &[&str], host: &str| {
            let mut req = Request::builder().uri("/").header("host", host);
            for login in logins {
                req = req.header("tailscale-user-login", *login);
            }
            for peer in peers {
                req = req.header("x-forwarded-for", *peer);
            }
            req.body(Body::empty()).unwrap()
        };
        let get = |logins: &[&str], host: &str| get_from(logins, &["100.64.0.1"], host);
        let host = "box-host.example-net.ts.net";
        for (logins, status) in [
            (&[][..], StatusCode::UNAUTHORIZED),
            (&["stranger@example.test"][..], StatusCode::UNAUTHORIZED),
            (
                &["owner@example.test", "owner@example.test"][..],
                StatusCode::UNAUTHORIZED,
            ),
            (&[""][..], StatusCode::UNAUTHORIZED),
            (&["Owner@Example.test"][..], StatusCode::OK),
        ] {
            let response = gateway.clone().oneshot(get(logins, host)).await.unwrap();
            assert_eq!(response.status(), status, "{logins:?}");
        }
        // The login must belong to the peer: whois decides, and serve sets one address.
        for peers in [
            &[][..],
            &["100.64.0.2"][..],
            &["100.64.0.9"][..],
            &["100.64.0.1, 100.64.0.2"][..],
            &["100.64.0.1", "100.64.0.1"][..],
            &["not-an-address"][..],
        ] {
            let req = get_from(&["owner@example.test"], peers, host);
            assert_eq!(
                gateway.clone().oneshot(req).await.unwrap().status(),
                StatusCode::UNAUTHORIZED,
                "{peers:?}"
            );
        }
        // Ownership is bound to the stable user ID, not to the reusable login.
        let mut headers = HeaderMap::new();
        headers.insert(
            "tailscale-user-login",
            "owner@example.test".parse().unwrap(),
        );
        headers.insert("x-forwarded-for", "100.64.0.1".parse().unwrap());
        let (_, identity) = tailnet_identity(&tailnet_config(), &headers).await.unwrap();
        assert_eq!(identity.subject, "42");
        assert_eq!(identity.issuer, "tailscale://example-net.ts.net");
        // No CLI installer: the CLI signs in through Access only.
        let mut req = get(&[], host);
        *req.uri_mut() = "/cli/install.sh".parse().unwrap();
        assert_eq!(
            gateway.clone().oneshot(req).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        // A valid Access JWT means nothing in tailnet mode.
        let mut req = get(&[], host);
        req.headers_mut().insert(
            "cf-access-jwt-assertion",
            token(&key, &claims()).parse().unwrap(),
        );
        assert_eq!(
            gateway.clone().oneshot(req).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        // Other hostnames are refused before any identity check.
        let response = gateway
            .clone()
            .oneshot(get(&["owner@example.test"], "app.example.test"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
        // Mutations still need the exact tailnet HTTPS origin.
        for origin in [None, Some("https://app.example.test")] {
            let mut req = Request::builder()
                .method(Method::POST)
                .uri("/api/operation")
                .header("host", host)
                .header("x-forwarded-for", "100.64.0.1")
                .header("tailscale-user-login", "owner@example.test");
            if let Some(origin) = origin {
                req = req.header("origin", origin);
            }
            let req = req
                .body(Body::from(r#"{"op":"create","id":"test"}"#))
                .unwrap();
            assert_eq!(
                gateway.clone().oneshot(req).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
        }
        // Tailnet identity headers never reach a guest upstream.
        let mut headers = HeaderMap::new();
        headers.insert(
            "tailscale-user-login",
            "owner@example.test".parse().unwrap(),
        );
        headers.insert("tailscale-user-name", "Owner".parse().unwrap());
        assert!(clean_headers(&headers, true).is_empty());
    }

    #[tokio::test]
    async fn ingress_authentication_and_proxy_boundaries() {
        let count = Arc::new(AtomicUsize::new(0));
        let fixture_count = count.clone();
        let fixture = Router::new().fallback(move |request: Request| {
            let count = fixture_count.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                let headers = request.headers();
                assert!(!headers.contains_key("cf-access-jwt-assertion"));
                assert!(!headers.contains_key("cf-access-authenticated-user-email"));
                assert!(!headers.contains_key("authorization"));
                assert!(!headers.contains_key("x-forwarded-host"));
                assert!(!headers.contains_key("x-secret"));
                assert_eq!(headers.get("cookie").unwrap(), "guest=ok");
                (
                    StatusCode::OK,
                    [
                        ("set-cookie", "CF_Authorization=guest-spoof"),
                        ("cache-control", "public"),
                    ],
                    "guest counter",
                )
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, fixture).await.unwrap();
        });
        let (key, set) = signing_key();
        let app = App {
            dashboard: None,
            catalog: None,
            config: config(upstream),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            keys: Arc::new(RwLock::new(Keys {
                set,
                fetched: Instant::now(),
            })),
            requests: Arc::new(Semaphore::new(64)),
        };
        let gateway = router(app.clone());
        let signed = token(&key, &claims());
        for jwt in [None, Some("forged.jwt.value")] {
            let response = gateway
                .clone()
                .oneshot(request(jwt, Method::GET, Body::empty()))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        for (field, value) in [
            ("iss", json!("https://attacker.cloudflareaccess.com")),
            ("aud", json!(["other-app"])),
            ("email", json!("someone@example.test")),
            ("exp", json!(1)),
            ("nbf", json!(9999999999_u64)),
            ("sub", json!("")),
        ] {
            let mut invalid = claims();
            invalid[field] = value;
            let signed = token(&key, &invalid);
            assert_eq!(
                gateway
                    .clone()
                    .oneshot(request(Some(&signed), Method::GET, Body::empty()))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }
        let (other_key, _) = signing_key();
        let forged = token(&other_key, &claims());
        assert_eq!(
            gateway
                .clone()
                .oneshot(request(Some(&forged), Method::GET, Body::empty()))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let hmac = encode(
            &Header::new(Algorithm::HS256),
            &claims(),
            &EncodingKey::from_secret(b"forged"),
        )
        .unwrap();
        assert_eq!(
            gateway
                .clone()
                .oneshot(request(Some(&hmac), Method::GET, Body::empty()))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let mut duplicate = request(Some(&signed), Method::GET, Body::empty());
        duplicate.headers_mut().append(
            "cf-access-jwt-assertion",
            HeaderValue::from_str(&signed).unwrap(),
        );
        assert_eq!(
            gateway.clone().oneshot(duplicate).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        let mut wrong_host = request(Some(&signed), Method::GET, Body::empty());
        wrong_host
            .headers_mut()
            .insert("host", HeaderValue::from_static("other.example.test"));
        assert_eq!(
            gateway.clone().oneshot(wrong_host).await.unwrap().status(),
            StatusCode::MISDIRECTED_REQUEST
        );
        let post = request(Some(&signed), Method::POST, Body::empty());
        assert_eq!(
            gateway.clone().oneshot(post).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let mut huge = request(
            Some(&signed),
            Method::POST,
            Body::from(vec![0; REQUEST_LIMIT + 1]),
        );
        huge.headers_mut().insert(
            "origin",
            HeaderValue::from_static("https://app.example.test"),
        );
        assert_eq!(
            gateway.clone().oneshot(huge).await.unwrap().status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "denied requests must not reach the guest"
        );
        let mut valid = request(Some(&signed), Method::GET, Body::empty());
        for (name, value) in [
            (
                "cookie",
                "CF_Authorization=secret; guest=ok; CF_AppSession=secret",
            ),
            ("authorization", "Bearer secret"),
            ("cf-access-authenticated-user-email", "owner@example.test"),
            ("x-forwarded-host", "attacker.test"),
            ("connection", "x-secret"),
            ("x-secret", "secret"),
        ] {
            valid
                .headers_mut()
                .insert(name, HeaderValue::from_str(value).unwrap());
        }
        let response = gateway.clone().oneshot(valid).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
        assert!(!response.headers().contains_key("set-cookie"));
        assert_eq!(
            to_bytes(response.into_body(), RESPONSE_LIMIT)
                .await
                .unwrap(),
            "guest counter"
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
        app.keys.write().await.fetched = Instant::now() - KEY_LIFETIME;
        assert_eq!(
            gateway
                .oneshot(request(Some(&signed), Method::GET, Body::empty()))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
        task.abort();
    }
    #[tokio::test]
    async fn dashboard_gate_denies_unauthorized_and_cross_origin_controls() {
        let (key, set) = signing_key();
        let app = App {
            dashboard: Some(PathBuf::from("/tmp/ow-no-worker-test")),
            catalog: None,
            config: config("http://127.0.0.1:1234".into()),
            client: reqwest::Client::new(),
            keys: Arc::new(RwLock::new(Keys {
                set,
                fetched: Instant::now(),
            })),
            requests: Arc::new(Semaphore::new(64)),
        };
        let gateway = router(app);
        for path in [
            "/",
            "/app.js",
            "/style.css",
            "/vendor/xterm.js",
            "/api/state",
            "/api/operation",
            "/api/terminal/test",
            "/api/ssh/test",
            "/cli/api/state",
            "/cli/../api/state",
            "/cli/install.sh/extra",
            "/cli/ow-linux-amd64/extra",
        ] {
            let mut req = request(None, Method::GET, Body::empty());
            *req.uri_mut() = path.parse().unwrap();
            assert_eq!(
                gateway.clone().oneshot(req).await.unwrap().status(),
                StatusCode::UNAUTHORIZED
            );
        }
        let mut req = request(None, Method::GET, Body::empty());
        *req.uri_mut() = "/cli/install.sh".parse().unwrap();
        let response = gateway.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let script = to_bytes(response.into_body(), 16384).await.unwrap();
        assert!(
            std::str::from_utf8(&script)
                .unwrap()
                .contains("origin='https://app.example.test'")
        );
        for (method, path) in [
            (Method::POST, "/cli/install.sh"),
            (Method::GET, "/cli/install.sh?file=/etc/passwd"),
        ] {
            let mut req = request(None, method, Body::empty());
            *req.uri_mut() = path.parse().unwrap();
            assert_eq!(
                gateway.clone().oneshot(req).await.unwrap().status(),
                StatusCode::BAD_REQUEST
            );
        }
        let signed = token(&key, &claims());
        for path in ["/api/terminal/test", "/api/ssh/test"] {
            for origin in [None, Some("https://guest.example.test")] {
                let mut req = request(Some(&signed), Method::GET, Body::empty());
                *req.uri_mut() = path.parse().unwrap();
                if let Some(origin) = origin {
                    req.headers_mut().insert("origin", origin.parse().unwrap());
                }
                assert_eq!(
                    gateway.clone().oneshot(req).await.unwrap().status(),
                    StatusCode::FORBIDDEN
                );
            }
        }
        let mut req = request(Some(&signed), Method::GET, Body::empty());
        *req.uri_mut() = "/".parse().unwrap();
        let response = gateway.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get("content-security-policy")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
        for origin in [None, Some("https://guest.example.test")] {
            let mut req = request(
                Some(&signed),
                Method::POST,
                Body::from(r#"{"op":"create","id":"test"}"#),
            );
            *req.uri_mut() = "/api/operation".parse().unwrap();
            if let Some(origin) = origin {
                req.headers_mut().insert("origin", origin.parse().unwrap());
            }
            assert_eq!(
                gateway.clone().oneshot(req).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
        }
        let mut req = request(
            Some(&signed),
            Method::POST,
            Body::from(r#"{"op":"shutdown","id":"test"}"#),
        );
        *req.uri_mut() = "/api/operation".parse().unwrap();
        for (name, value) in [
            ("origin", "https://app.example.test"),
            ("content-type", "application/json"),
            ("x-ow-request", "dashboard"),
        ] {
            req.headers_mut().insert(name, value.parse().unwrap());
        }
        assert_eq!(
            gateway.oneshot(req).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "Needs prepared real microVM assets and release worker"]
    async fn users_isolated_real_vm() {
        use crate::{
            catalog::{Catalog, Identity},
            wire,
        };
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let root = repo
            .join("data")
            .join(format!("users-{}", &crate::common::nonce().unwrap()[..8]));
        let binary = repo.join("target/release/ow");
        struct Cleanup(PathBuf, PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = Command::new(&self.0)
                    .args(["--local", "--data-dir", self.1.to_str().unwrap(), "down"])
                    .output();
            }
        }
        let _cleanup = Cleanup(binary.clone(), root.clone());
        let result = Command::new(&binary)
            .args(["--local", "--data-dir", root.to_str().unwrap(), "up"])
            .output()
            .unwrap();
        assert!(result.status.success());
        wire::request(&root, json!({"op":"create","id":"legacy","memory_mib":256})).unwrap();
        let (key, keys) = signing_key();
        let mut cfg = config("http://127.0.0.1:1234".into());
        cfg.allowed_emails.push("bob@example.test".into());
        let catalog = Arc::new(std::sync::Mutex::new(
            Catalog::open(&root, &cfg.issuer(), Some("owner@example.test")).unwrap(),
        ));
        let app = App {
            dashboard: Some(root.clone()),
            catalog: Some(catalog.clone()),
            config: cfg.clone(),
            client: reqwest::Client::new(),
            keys: Arc::new(RwLock::new(Keys {
                set: keys,
                fetched: Instant::now(),
            })),
            requests: Arc::new(Semaphore::new(64)),
        };
        let gateway = router(app);
        let alice = token(&key, &claims());
        let mut bob_claims = claims();
        bob_claims["sub"] = json!("bob-subject");
        bob_claims["email"] = json!("bob@example.test");
        let bob = token(&key, &bob_claims);
        async fn api(gateway: &Router, jwt: &str, body: Option<Value>) -> (StatusCode, Value) {
            let mut req = Request::builder()
                .uri(if body.is_some() {
                    "/api/operation"
                } else {
                    "/api/state"
                })
                .method(if body.is_some() {
                    Method::POST
                } else {
                    Method::GET
                })
                .header("host", "app.example.test")
                .header("cf-access-jwt-assertion", jwt)
                .header("origin", "https://app.example.test")
                .header("x-ow-request", "dashboard")
                .header("content-type", "application/json")
                .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
                .unwrap();
            let response = gateway
                .clone()
                .oneshot(std::mem::replace(&mut req, Request::new(Body::empty())))
                .await
                .unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
                .await
                .unwrap();
            (status, serde_json::from_slice(&bytes).unwrap())
        }
        let (_, a) = api(&gateway, &alice, None).await;
        assert_eq!(a["result"]["workspaces"][0]["id"], "legacy");
        let (_, b) = api(&gateway, &bob, None).await;
        assert_eq!(b["result"]["workspaces"], json!([]));
        assert_eq!(b["result"]["stats"]["running"], json!({}));
        for op in [
            "exec",
            "start",
            "stop",
            "hibernate",
            "snapshot",
            "fork",
            "restore",
        ] {
            let (code,_)=api(&gateway,&bob,Some(json!({"op":op,"id":"legacy","name":"checkpoint","child":"stolen","command":"true"}))).await;
            assert_eq!(code, StatusCode::CONFLICT, "{op}");
        }
        let req = Request::builder()
            .uri("/api/terminal/legacy")
            .header("host", "app.example.test")
            .header("cf-access-jwt-assertion", &bob)
            .header("origin", "https://app.example.test")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            gateway.clone().oneshot(req).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
        for (jwt, label) in [(&alice, "ALICE"), (&bob, "BOB")] {
            let (code, value) = api(
                &gateway,
                jwt,
                Some(json!({"op":"create","id":"demo","memory_mib":256})),
            )
            .await;
            assert_eq!(code, StatusCode::OK, "{value}");
            assert_eq!(value["result"]["id"], "demo");
            assert!(value["result"].get("index").is_none());
            let(code,value)=api(&gateway,jwt,Some(json!({"op":"exec","id":"demo","command":format!("echo {label} > /persist/owner; cat /persist/owner")}))).await;
            assert_eq!(code, StatusCode::OK, "{value}");
            assert!(value["result"]["output"].as_str().unwrap().contains(label));
            let (code, value) = api(
                &gateway,
                jwt,
                Some(json!({"op":"snapshot","id":"demo","name":"checkpoint"})),
            )
            .await;
            assert_eq!(code, StatusCode::OK, "{value}");
            assert_eq!(value["result"]["name"], "checkpoint");
            let (code, value) = api(
                &gateway,
                jwt,
                Some(json!({"op":"fork","id":"demo","child":"branch","snapshot":"checkpoint"})),
            )
            .await;
            assert_eq!(code, StatusCode::OK, "{value}");
            assert_eq!(value["result"]["id"], "branch");
            assert_eq!(value["result"]["source"], "demo");
            let (_, value) = api(
                &gateway,
                jwt,
                Some(json!({"op":"exec","id":"branch","command":"cat /persist/owner"})),
            )
            .await;
            assert!(value["result"]["output"].as_str().unwrap().contains(label));
            let (_, state) = api(&gateway, jwt, None).await;
            assert!(
                state["result"]["workspaces"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|m| ["demo", "branch", "legacy"].contains(&m["id"].as_str().unwrap()))
            );
        }
        let (_, value) = api(
            &gateway,
            &alice,
            Some(json!({"op":"snapshot","id":"demo","name":"private"})),
        )
        .await;
        assert_eq!(value["ok"], true);
        for body in [
            json!({"op":"restore","id":"demo","name":"private"}),
            json!({"op":"fork","id":"demo","child":"leak","snapshot":"private"}),
        ] {
            assert_eq!(
                api(&gateway, &bob, Some(body)).await.0,
                StatusCode::CONFLICT
            );
        }
        let physical = {
            let mut c = catalog.lock().unwrap();
            let u = c
                .user(&Identity {
                    issuer: cfg.issuer(),
                    subject: "test-subject".into(),
                    email: "owner@example.test".into(),
                })
                .unwrap();
            c.terminal_id(u, "demo").unwrap()
        };
        assert_eq!(
            api(
                &gateway,
                &bob,
                Some(json!({"op":"exec","id":physical,"command":"true"}))
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let (_, state) = api(&gateway, &bob, None).await;
        assert_eq!(state["result"]["workspaces"].as_array().unwrap().len(), 2);
        assert_eq!(state["result"]["stats"]["reserved_memory_mib"], 512);
        assert_eq!(
            state["result"]["stats"]["running"]
                .as_object()
                .unwrap()
                .len(),
            2
        );
        wire::request(&root, json!({"op":"stop","id":"legacy"})).unwrap();
        drop(gateway);
        drop(catalog);
        let mut reopened = Catalog::open(&root, &cfg.issuer(), Some("bob@example.test")).unwrap();
        let user = reopened
            .user(&Identity {
                issuer: cfg.issuer(),
                subject: "bob-subject".into(),
                email: "bob@example.test".into(),
            })
            .unwrap();
        assert!(reopened.terminal_id(user, "legacy").is_err());
        assert!(reopened.terminal_id(user, "demo").is_ok());
        std::fs::write(root.join("users-result.json"),json!({"passed":true,"checks":["explicit legacy owner import","verified distinct subjects","same machine/snapshot/fork names for two users","independent guest files","all lifecycle and exec cross-owner denial","cross-owner snapshot/fork/restore denial","physical ID denial","WSS ownership denial before worker connection","owner-only stats and metadata","database reopen preserves ownership"]}).to_string()).unwrap();
        println!("User isolation artifacts: {}", root.display());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unshare_closes_a_grantees_live_terminal() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("ow-share-{}", crate::common::nonce().unwrap()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        // Fake worker: one running legacy machine; a terminal stream stays open until closed.
        let listener = std::os::unix::net::UnixListener::bind(root.join("control.sock")).unwrap();
        let terminals = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        // A slow worker exec remains in flight while the owner revokes the grant.
        let slow = Arc::new(AtomicUsize::new(0));
        let (opened, ended, held) = (terminals.clone(), closed.clone(), slow.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let (opened, ended, held) = (opened.clone(), ended.clone(), held.clone());
                std::thread::spawn(move || {
                    let Ok(request) = crate::wire::line(&mut stream) else {
                        return;
                    };
                    if request["command"] == "slow" {
                        held.store(1, Ordering::SeqCst);
                        while held.load(Ordering::SeqCst) == 1 {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                    let result = match request["op"].as_str().unwrap_or("") {
                        "list" => {
                            json!([{"id":"legacy","state":"running","memory_mib":256,"vcpu_count":1,"image":"alpine"}])
                        }
                        "snapshots" => json!([]),
                        "stats" => json!({"running":{}}),
                        "exec" => json!({"output":"shared-ok","exit_code":0}),
                        _ => json!({}),
                    };
                    crate::wire::send(&mut stream, &json!({"ok":true,"result":result})).unwrap();
                    if request["op"] == "terminal" {
                        opened.fetch_add(1, Ordering::SeqCst);
                        let mut sink = [0; 4096];
                        while std::io::Read::read(&mut stream, &mut sink).is_ok_and(|n| n > 0) {}
                        ended.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });
        let mut catalog = crate::catalog::Catalog::open(
            &root,
            "https://test-team.cloudflareaccess.com",
            Some("owner@example.test"),
        )
        .unwrap();
        // The owner is also the configured gateway (host pool) owner.
        let owner = catalog
            .user(&crate::catalog::Identity {
                issuer: "https://test-team.cloudflareaccess.com".into(),
                subject: "test-subject".into(),
                email: "owner@example.test".into(),
            })
            .unwrap();
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = tcp.local_addr().unwrap().port();
        let (key, set) = signing_key();
        let mut cfg = config("http://127.0.0.1:1".into());
        cfg.hostname = format!("localhost:{port}");
        cfg.allowed_emails.push("bob@example.test".into());
        cfg.node_owner_subject = Some("test-subject".into());
        cfg.node_owner_user_id = Some(owner);
        let gateway = router(App {
            dashboard: Some(root.clone()),
            catalog: Some(Arc::new(std::sync::Mutex::new(catalog))),
            config: cfg,
            client: reqwest::Client::new(),
            keys: Arc::new(RwLock::new(Keys {
                set,
                fetched: Instant::now(),
            })),
            requests: Arc::new(Semaphore::new(64)),
        });
        let served = gateway.clone();
        let server = tokio::spawn(async move { axum::serve(tcp, served).await });
        let alice = token(&key, &claims());
        let mut c = claims();
        c["sub"] = json!("bob-subject");
        c["email"] = json!("bob@example.test");
        let bob = token(&key, &c);
        let host = format!("localhost:{port}");
        let api = |jwt: String, body: Value| {
            let gateway = gateway.clone();
            let host = host.clone();
            async move {
                let req = Request::builder()
                    .uri("/api/operation")
                    .method(Method::POST)
                    .header("host", &host)
                    .header("cf-access-jwt-assertion", jwt)
                    .header("origin", format!("https://{host}"))
                    .header("x-ow-request", "dashboard")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap();
                let response = gateway.oneshot(req).await.unwrap();
                let status = response.status();
                let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
                (status, serde_json::from_slice::<Value>(&bytes).unwrap())
            }
        };
        let shared = "owner@example.test/legacy";
        let terminal = |jwt: String| {
            let url = format!("ws://localhost:{port}/api/terminal/{shared}");
            let origin = format!("https://localhost:{port}");
            tokio::task::spawn_blocking(move || {
                use tungstenite::client::IntoClientRequest;
                let mut req = url.into_client_request().unwrap();
                req.headers_mut()
                    .insert("cf-access-jwt-assertion", jwt.parse().unwrap());
                req.headers_mut().insert("origin", origin.parse().unwrap());
                tungstenite::connect(req).map(|(socket, _)| socket)
            })
        };
        let state = |jwt: String| {
            let gateway = gateway.clone();
            let host = host.clone();
            async move {
                let req = Request::builder()
                    .uri("/api/state")
                    .header("host", &host)
                    .header("cf-access-jwt-assertion", jwt)
                    .body(Body::empty())
                    .unwrap();
                let response = gateway.oneshot(req).await.unwrap();
                let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
                serde_json::from_slice::<Value>(&bytes).unwrap()
            }
        };
        assert_eq!(state(alice.clone()).await["result"]["host_controls"], true);
        assert_eq!(state(bob.clone()).await["result"]["host_controls"], false);
        let exec = json!({"op":"exec","id":shared,"command":"id"});
        assert!(terminal(bob.clone()).await.unwrap().is_err());
        assert_eq!(
            api(bob.clone(), exec.clone()).await.0,
            StatusCode::NOT_FOUND
        );
        let (code, value) = api(
            alice.clone(),
            json!({"op":"share","id":"legacy","with":"bob@example.test"}),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{value}");
        let (code, value) = api(bob.clone(), exec.clone()).await;
        assert_eq!(
            (code, value["result"]["output"].as_str()),
            (StatusCode::OK, Some("shared-ok"))
        );
        let mut socket = terminal(bob.clone()).await.unwrap().unwrap();
        if let tungstenite::stream::MaybeTlsStream::Plain(s) = socket.get_mut() {
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        }
        while terminals.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Revocation must close the live terminal without waiting for Bob's slow exec.
        let pending = tokio::spawn(api(
            bob.clone(),
            json!({"op":"exec","id":shared,"command":"slow"}),
        ));
        while slow.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(terminals.load(Ordering::SeqCst), 1);
        assert_eq!(
            closed.load(Ordering::SeqCst),
            0,
            "terminal closed before revocation"
        );
        let started = Instant::now();
        let (code, value) = tokio::time::timeout(
            Duration::from_secs(3),
            api(
                alice.clone(),
                json!({"op":"unshare","id":"legacy","with":"bob@example.test"}),
            ),
        )
        .await
        .expect("unshare waited behind a slow worker request");
        assert_eq!(code, StatusCode::OK, "{value}");
        let ended = tokio::task::spawn_blocking(move || {
            loop {
                match socket.read() {
                    Ok(tungstenite::Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
        ended.await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "unshare did not close the stream"
        );
        while closed.load(Ordering::SeqCst) == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "worker stream left open"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            slow.load(Ordering::SeqCst),
            1,
            "slow exec was still in flight"
        );
        slow.store(2, Ordering::SeqCst);
        pending.await.unwrap();
        assert_eq!(api(bob.clone(), exec).await.0, StatusCode::NOT_FOUND);
        assert!(terminal(bob).await.unwrap().is_err());
        server.abort();
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "Needs KVM, prepared real microVM assets and a release worker (target/release/ow)"]
    async fn sharing_real_vm() {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let root = repo
            .join("data")
            .join(format!("sharing-{}", &crate::common::nonce().unwrap()[..8]));
        let binary = repo.join("target/release/ow");
        struct Cleanup(PathBuf, PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = Command::new(&self.0)
                    .args(["--local", "--data-dir", self.1.to_str().unwrap(), "down"])
                    .output();
            }
        }
        let _cleanup = Cleanup(binary.clone(), root.clone());
        let up = Command::new(&binary)
            .args(["--local", "--data-dir", root.to_str().unwrap(), "up"])
            .output()
            .unwrap();
        assert!(
            up.status.success(),
            "{}",
            String::from_utf8_lossy(&up.stderr)
        );
        let catalog =
            crate::catalog::Catalog::open(&root, "https://test-team.cloudflareaccess.com", None)
                .unwrap();
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = tcp.local_addr().unwrap().port();
        let (key, set) = signing_key();
        let mut cfg = config("http://127.0.0.1:1".into());
        cfg.hostname = format!("localhost:{port}");
        cfg.allowed_emails.push("bob@example.test".into());
        let gateway = router(App {
            dashboard: Some(root.clone()),
            catalog: Some(Arc::new(std::sync::Mutex::new(catalog))),
            config: cfg,
            client: reqwest::Client::new(),
            keys: Arc::new(RwLock::new(Keys {
                set,
                fetched: Instant::now(),
            })),
            requests: Arc::new(Semaphore::new(64)),
        });
        let served = gateway.clone();
        let server = tokio::spawn(async move { axum::serve(tcp, served).await });
        let alice = token(&key, &claims());
        let mut c = claims();
        c["sub"] = json!("bob-subject");
        c["email"] = json!("bob@example.test");
        let bob = token(&key, &c);
        let host = format!("localhost:{port}");
        let api = |jwt: String, body: Option<Value>| {
            let gateway = gateway.clone();
            let host = host.clone();
            async move {
                let req = Request::builder()
                    .uri(if body.is_some() {
                        "/api/operation"
                    } else {
                        "/api/state"
                    })
                    .method(if body.is_some() {
                        Method::POST
                    } else {
                        Method::GET
                    })
                    .header("host", &host)
                    .header("cf-access-jwt-assertion", jwt)
                    .header("origin", format!("https://{host}"))
                    .header("x-ow-request", "dashboard")
                    .header("content-type", "application/json")
                    .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
                    .unwrap();
                let response = gateway.oneshot(req).await.unwrap();
                let status = response.status();
                let bytes = to_bytes(response.into_body(), 4 << 20).await.unwrap();
                (status, serde_json::from_slice::<Value>(&bytes).unwrap())
            }
        };
        let shared = "owner@example.test/box";
        let terminal = |jwt: String| {
            let url = format!("ws://localhost:{port}/api/terminal/{shared}");
            let origin = format!("https://localhost:{port}");
            tokio::task::spawn_blocking(move || {
                use tungstenite::client::IntoClientRequest;
                let mut req = url.into_client_request().unwrap();
                req.headers_mut()
                    .insert("cf-access-jwt-assertion", jwt.parse().unwrap());
                req.headers_mut().insert("origin", origin.parse().unwrap());
                tungstenite::connect(req).map(|(socket, _)| socket)
            })
        };
        let mut checks = Vec::new();
        let (code, value) = api(
            alice.clone(),
            Some(json!({"op":"create","id":"box","memory_mib":256})),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{value}");
        let (code, value) = api(alice.clone(), Some(json!({"op":"exec","id":"box","command":"echo alice-secret > /persist/token; cat /persist/token"}))).await;
        assert_eq!(code, StatusCode::OK, "{value}");
        let exec = |command: &str| json!({"op":"exec","id":shared,"command":command});
        assert_eq!(
            api(bob.clone(), Some(exec("cat /persist/token"))).await.0,
            StatusCode::CONFLICT
        );
        assert!(terminal(bob.clone()).await.unwrap().is_err());
        checks.push("no access before share");
        let (code, value) = api(
            alice.clone(),
            Some(json!({"op":"share","id":"box","with":"bob@example.test"})),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{value}");
        let (_, state) = api(bob.clone(), None).await;
        assert_eq!(state["result"]["shared"][0]["id"], shared, "{state}");
        assert_eq!(state["result"]["workspaces"], json!([]));
        checks.push("shared with me lists owner/name");
        let (code, value) = api(bob.clone(), Some(exec("cat /persist/token"))).await;
        assert_eq!(code, StatusCode::OK, "{value}");
        assert!(
            value["result"]["output"]
                .as_str()
                .unwrap()
                .contains("alice-secret")
        );
        checks.push("grantee exec in the owner's guest");
        let (code, _) = api(
            bob.clone(),
            Some(json!({"op":"resize","id":shared,"memory_mib":512,"vcpu_count":1})),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT);
        checks.push("use role cannot resize");
        let (code, value) = api(
            bob.clone(),
            Some(json!({"op":"fork","id":shared,"child":"copy"})),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{value}");
        let (_, value) = api(
            bob.clone(),
            Some(json!({"op":"exec","id":"copy","command":"cat /persist/token"})),
        )
        .await;
        assert!(
            value["result"]["output"]
                .as_str()
                .unwrap()
                .contains("alice-secret")
        );
        checks.push("grantee fork copies the owner's files");
        let mut socket = terminal(bob.clone()).await.unwrap().unwrap();
        if let tungstenite::stream::MaybeTlsStream::Plain(s) = socket.get_mut() {
            s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        }
        let (mut socket, seen) = tokio::task::spawn_blocking(move || {
            socket
                .send(tungstenite::Message::Binary(
                    b"echo SHARED-$((40+2))\r".to_vec().into(),
                ))
                .unwrap();
            let mut seen = String::new();
            while !seen.contains("SHARED-42") {
                match socket.read() {
                    Ok(tungstenite::Message::Binary(bytes)) => {
                        seen.push_str(&String::from_utf8_lossy(&bytes));
                        socket
                            .send(tungstenite::Message::Text(
                                json!({"type":"ack","bytes":bytes.len()}).to_string().into(),
                            ))
                            .unwrap();
                    }
                    Ok(_) => {}
                    Err(e) => panic!("terminal read failed: {e}; saw {seen:?}"),
                }
            }
            (socket, seen)
        })
        .await
        .unwrap();
        assert!(seen.contains("SHARED-42"));
        checks.push("grantee terminal in the owner's guest");
        let (code, value) = api(
            alice.clone(),
            Some(json!({"op":"unshare","id":"box","with":"bob@example.test"})),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{value}");
        let started = Instant::now();
        tokio::task::spawn_blocking(move || {
            loop {
                match socket.read() {
                    Ok(tungstenite::Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        })
        .await
        .unwrap();
        let cut = started.elapsed();
        assert!(cut < Duration::from_secs(3), "terminal stayed open {cut:?}");
        checks.push("unshare closes the live terminal");
        assert_eq!(
            api(bob.clone(), Some(exec("true"))).await.0,
            StatusCode::CONFLICT
        );
        assert!(terminal(bob.clone()).await.unwrap().is_err());
        let (_, state) = api(bob.clone(), None).await;
        assert_eq!(state["result"]["shared"], json!([]));
        assert_eq!(state["result"]["workspaces"][0]["id"], "copy");
        checks.push("after unshare: exec and terminal refused, fork stays the grantee's");
        let (_, value) = api(
            alice.clone(),
            Some(json!({"op":"exec","id":"box","command":"cat /persist/token"})),
        )
        .await;
        assert!(
            value["result"]["output"]
                .as_str()
                .unwrap()
                .contains("alice-secret")
        );
        checks.push("owner unaffected");
        for (jwt, id) in [(&alice, "box"), (&bob, "copy")] {
            api(jwt.clone(), Some(json!({"op":"stop","id":id}))).await;
        }
        server.abort();
        std::fs::write(
            root.join("sharing-result.json"),
            json!({"passed":true,"unshare_cut_ms":cut.as_millis() as u64,"checks":checks})
                .to_string(),
        )
        .unwrap();
        println!(
            "Sharing artifacts: {} (stream closed {cut:?} after unshare)",
            root.display()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "Real VM/browser integration: build release binary and install project-local Playwright first"]
    async fn dashboard_browser_real_vm() {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let root = repo
            .join("data")
            .join(format!("ui-{}", &crate::vm::nonce().unwrap()[..8]));
        struct Cleanup(PathBuf, PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = Command::new(&self.0)
                    .args(["--data-dir", self.1.to_str().unwrap(), "down"])
                    .output();
            }
        }
        let binary = repo.join("target/release/ow");
        let cleanup = Cleanup(binary.clone(), root.clone());
        let up = Command::new(&binary)
            .args(["--data-dir", root.to_str().unwrap(), "up"])
            .output()
            .unwrap();
        assert!(
            up.status.success(),
            "{}",
            String::from_utf8_lossy(&up.stderr)
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (key, set) = signing_key();
        let signed = token(&key, &claims());
        let mut cfg = config("http://127.0.0.1:1234".into());
        // A local TLS terminator models Cloudflare without bypassing production auth/Origin checks.
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let public_address = reservation.local_addr().unwrap();
        drop(reservation);
        cfg.hostname = public_address.to_string();
        let app = App {
            dashboard: Some(root.clone()),
            catalog: Some(Arc::new(std::sync::Mutex::new(
                crate::catalog::Catalog::open(&root, &cfg.issuer(), None).unwrap(),
            ))),
            config: cfg,
            client: reqwest::Client::new(),
            keys: Arc::new(RwLock::new(Keys {
                set,
                fetched: Instant::now(),
            })),
            requests: Arc::new(Semaphore::new(64)),
        };
        let task = tokio::spawn(async move {
            axum::serve(listener, router(app)).await.unwrap();
        });
        fs::write(root.join("browser.json"),json!({"url":format!("https://{public_address}"),"upstream":format!("http://{address}"),"origin":format!("https://{public_address}"),"assertion":signed,"artifacts":root}).to_string()).unwrap();
        let browser_input = root.join("browser.json");
        let browser_script = repo.join("experiments/runtime-spike/dashboard-browser.cjs");
        let output = tokio::task::spawn_blocking(move || {
            Command::new("node")
                .arg(browser_script)
                .arg(browser_input)
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        task.abort();
        println!("Browser artifacts: {}", root.display());
        println!("{}", String::from_utf8_lossy(&output.stdout));
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        drop(cleanup);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "Requires parent-exclusive one-guest 4 GiB turn, explicit dedicated root/binary/assets and headroom inventory"]
    async fn capacity_ubuntu_4gib_real_vm() {
        let root =
            PathBuf::from(std::env::var("OW_CAPACITY_TEST_ROOT").expect("dedicated root required"));
        let binary =
            PathBuf::from(std::env::var("OW_CAPACITY_BINARY").expect("pinned binary required"));
        let test_parent = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        assert!(
            root.is_absolute()
                && root.parent() == Some(test_parent.as_path())
                && root.file_name().unwrap() == "v"
        );
        assert!(!root.exists(), "new root only; preserve existing artifacts");
        struct Cleanup(PathBuf, PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = Command::new(&self.0)
                    .args(["--local", "--data-dir"])
                    .arg(&self.1)
                    .arg("down")
                    .output();
            }
        }
        let cleanup = Cleanup(binary.clone(), root.clone());
        let local = |action: &str| {
            let o = Command::new(&binary)
                .args(["--local", "--data-dir"])
                .arg(&root)
                .arg(action)
                .output()
                .unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        };
        local("up");
        let (key, set) = signing_key();
        let cfg = config("http://127.0.0.1:1".into());
        let app = App {
            dashboard: Some(root.clone()),
            catalog: Some(Arc::new(std::sync::Mutex::new(
                crate::catalog::Catalog::open(&root, &cfg.issuer(), None).unwrap(),
            ))),
            config: cfg,
            client: reqwest::Client::new(),
            keys: Arc::new(RwLock::new(Keys {
                set,
                fetched: Instant::now(),
            })),
            requests: Arc::new(Semaphore::new(64)),
        };
        async fn operation(app: &App, signed: &str, body: Value) -> Value {
            let req = Request::builder()
                .method(Method::POST)
                .uri("/api/operation")
                .header("host", "app.example.test")
                .header("origin", "https://app.example.test")
                .header("content-type", "application/json")
                .header("x-ow-request", "dashboard")
                .header("cf-access-jwt-assertion", signed)
                .body(Body::from(body.to_string()))
                .unwrap();
            let response = handle(app, req).await;
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let v: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(status, StatusCode::OK, "{v}");
            assert_eq!(v["ok"], true, "{v}");
            v["result"].clone()
        }
        let signed = token(&key, &claims());
        let created=operation(&app,&signed,json!({"op":"create","id":"capacity-ubuntu","image":"ubuntu","memory_mib":4096,"vcpu_count":2})).await;
        assert_eq!(created["memory_mib"], 4096);
        assert_eq!(created["vcpu_count"], 2);
        assert_eq!(created["state"], "running");
        let exec=operation(&app,&signed,json!({"op":"exec","id":"capacity-ubuntu","command":"cat /etc/os-release; nproc; git --version; gcc --version | head -1; mise exec -- node --version; mise exec -- python --version; mise exec -- rustc --version; sudo -n true; printf capacity-persistent > /persist/capacity-proof; cat /persist/capacity-proof"})).await;
        assert_eq!(exec["exit_code"], 0);
        assert!(exec["output"].as_str().unwrap().contains("Ubuntu"));
        assert!(
            exec["output"]
                .as_str()
                .unwrap()
                .contains("capacity-persistent")
        );
        operation(&app, &signed, json!({"op":"stop","id":"capacity-ubuntu"})).await;
        let resized = operation(
            &app,
            &signed,
            json!({"op":"resize","id":"capacity-ubuntu","memory_mib":4096,"vcpu_count":2}),
        )
        .await;
        assert_eq!(resized["state"], "stopped");
        assert_eq!(resized["memory_mib"], 4096);
        local("down");
        local("up");
        let restarted =
            operation(&app, &signed, json!({"op":"start","id":"capacity-ubuntu"})).await;
        assert_eq!(restarted["memory_mib"], 4096);
        assert_eq!(restarted["vcpu_count"], 2);
        let persisted=operation(&app,&signed,json!({"op":"exec","id":"capacity-ubuntu","command":"cat /persist/capacity-proof; nproc; grep MemTotal /proc/meminfo"})).await;
        assert_eq!(persisted["exit_code"], 0);
        assert!(
            persisted["output"]
                .as_str()
                .unwrap()
                .contains("capacity-persistent")
        );
        operation(&app, &signed, json!({"op":"stop","id":"capacity-ubuntu"})).await;
        local("down");
        fs::write(root.join("capacity-result.json"),json!({"passed":true,"requested_memory_mib":4096,"requested_vcpus":2,"max_new_guests":1,"create":created,"dev_exec":exec,"resized":resized,"cold_start":restarted,"persistence":persisted,"cleanup":"dedicated guest/worker stopped; disks and all artifacts retained"}).to_string()).unwrap();
        drop(app);
        drop(cleanup);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn host_admin_requires_verified_subject_and_exact_bound_catalog_id() {
        use crate::catalog::{Catalog, Identity};
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("ow-host-auth-{}", crate::common::nonce().unwrap()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let mut catalog =
            Catalog::open(&root, "https://test-team.cloudflareaccess.com", None).unwrap();
        let identity = Identity {
            issuer: "https://test-team.cloudflareaccess.com".into(),
            subject: "test-subject".into(),
            email: "owner@example.test".into(),
        };
        let owner = catalog.user(&identity).unwrap();
        let (key, set) = signing_key();
        let mut config = config("http://127.0.0.1:1".into());
        config.node_owner_subject = Some(identity.subject.clone());
        config.node_owner_user_id = Some(owner);
        let mut app = App {
            dashboard: Some(root.clone()),
            catalog: Some(Arc::new(std::sync::Mutex::new(catalog))),
            config,
            client: reqwest::Client::new(),
            keys: Arc::new(RwLock::new(Keys {
                set,
                fetched: Instant::now(),
            })),
            requests: Arc::new(Semaphore::new(64)),
        };
        let make = |jwt: Option<String>, node: &str| {
            let mut r = Request::builder()
                .method(Method::POST)
                .uri("/api/hosts/invite")
                .header("host", "app.example.test")
                .header("origin", "https://app.example.test")
                .header("content-type", "application/json")
                .header("x-ow-request", "dashboard")
                .header("cf-access-authenticated-user-email", "owner@example.test");
            if let Some(jwt) = jwt {
                r = r.header("cf-access-jwt-assertion", jwt);
            }
            r.body(Body::from(json!({"node":node,"cpus":1}).to_string()))
                .unwrap()
        };
        for subject in ["other-subject", ""] {
            let mut c = claims();
            c["sub"] = json!(subject);
            let response = handle(&app, make(Some(token(&key, &c)), "denied")).await;
            assert!(matches!(
                response.status(),
                StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
            ));
            let mut request = make(Some(token(&key, &c)), "denied");
            *request.uri_mut() = "/api/hosts/budget".parse().unwrap();
            assert!(matches!(
                handle(&app, request).await.status(),
                StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
            ));
        }
        assert_eq!(
            handle(&app, make(None, "denied")).await.status(),
            StatusCode::UNAUTHORIZED
        );
        app.config.node_owner_user_id = Some(owner + 1);
        assert_eq!(
            handle(&app, make(Some(token(&key, &claims())), "denied"))
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        app.config.node_owner_user_id = None;
        app.config.node_owner_subject = None;
        assert_eq!(
            handle(&app, make(Some(token(&key, &claims())), "denied"))
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(crate::nodes::inventory(&root).unwrap(), json!([]));
        app.config.node_owner_subject = Some(identity.subject);
        app.config.node_owner_user_id = Some(owner);
        crate::limits::free_pool(&root);
        let response = handle(&app, make(Some(token(&key, &claims())), "friend")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["result"]["cpus"], 1);
        assert_eq!(v["result"]["controller"], "https://app.example.test/_nodes");
        crate::nodes::db(&root)
            .unwrap()
            .execute(
                "UPDATE nodes SET credential_hash='synthetic-preserved' WHERE id='friend'",
                [],
            )
            .unwrap();
        let mut request = make(Some(token(&key, &claims())), "friend");
        *request.uri_mut() = "/api/hosts/budget".parse().unwrap();
        *request.body_mut() =
            Body::from(json!({"node":"friend","memory":8192,"slots":4,"cpus":32}).to_string());
        assert_eq!(handle(&app, request).await.status(), StatusCode::OK);
        assert_eq!(
            crate::nodes::inventory(&root).unwrap()[0]["owner_limits"],
            json!({"memory":8192,"slots":4,"cpus":32})
        );
        fs::write(root.join("limits.json"),json!({"pool":{"max_estimated_cents":0,"hours":1,"default_type":"test-instance","hourly_cents":{"test-instance":1}}}).to_string()).unwrap();
        let response = handle(&app, make(Some(token(&key, &claims())), "over-budget")).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        let rejection: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            rejection["error"]
                .as_str()
                .unwrap()
                .contains("pool spend cap exceeded")
        );
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
#[path = "preview_tests.rs"]
mod preview_tests;
