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
    time::{Duration, Instant},
};
use tokio::sync::{RwLock, Semaphore};

const REQUEST_LIMIT: usize = 1024 * 1024;
const RESPONSE_LIMIT: usize = 8 * 1024 * 1024;
const KEY_LIFETIME: Duration = Duration::from_secs(3600);

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    hostname: String,
    team_domain: String,
    audience: String,
    allowed_emails: Vec<String>,
    upstream: String,
    #[serde(default)]
    bootstrap_owner_email: Option<String>,
}

impl Config {
    fn validate(&self) -> Result<()> {
        ensure!(valid_hostname(&self.hostname), "invalid public hostname");
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
        ensure!(
            !self.allowed_emails.is_empty()
                && self
                    .allowed_emails
                    .iter()
                    .all(|e| e.contains('@') && e.len() <= 254),
            "an explicit email allowlist is required"
        );
        if let Some(owner) = &self.bootstrap_owner_email {
            ensure!(
                self.allowed_emails
                    .iter()
                    .any(|e| e.eq_ignore_ascii_case(owner)),
                "bootstrap owner must be allowed to log in"
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
        format!("https://{}", self.team_domain)
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

async fn authorized_until(
    app: &App,
    headers: &HeaderMap,
) -> Option<(u64, crate::catalog::Identity)> {
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
fn clean_headers(source: &HeaderMap, request: bool) -> HeaderMap {
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
    match tokio::time::timeout(Duration::from_secs(45), handle(&app, request)).await {
        Ok(response) => response,
        Err(_) => (StatusCode::GATEWAY_TIMEOUT, "Request timed out").into_response(),
    }
}

async fn handle(app: &App, request: Request) -> Response {
    let host = request
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if host != app.config.hostname && host != format!("{}:443", app.config.hostname) {
        return (StatusCode::MISDIRECTED_REQUEST, "Unknown hostname").into_response();
    }
    if app.dashboard.is_some() && crate::dashboard::public_path(request.uri().path()) {
        return crate::dashboard::public_download(
            &format!("https://{}", app.config.hostname),
            request,
        )
        .await;
    }
    let Some((expires, identity)) = authorized_until(app, request.headers()).await else {
        return (StatusCode::UNAUTHORIZED, "Cloudflare Access login required").into_response();
    };
    if let Some(root) = &app.dashboard
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
        if request.uri().query().is_some() || crate::common::identifier(&name).is_err() {
            return (StatusCode::BAD_REQUEST, "Invalid terminal target").into_response();
        }
        let id = crate::dashboard::with_catalog(
            app.catalog.clone(),
            identity.clone(),
            move |catalog, user| catalog.terminal_id(user, &name),
        )
        .await;
        let Ok(id) = id else {
            return (StatusCode::NOT_FOUND, "Machine not found").into_response();
        };
        return crate::terminal::upgrade(root, request, expires, id).await;
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
    if app.dashboard.is_some() {
        return crate::dashboard::handle(app.catalog.clone(), identity, request).await;
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

fn router(app: App) -> Router {
    Router::new().fallback(proxy).with_state(app)
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
    let config: Config = serde_json::from_slice(&fs::read(path)?)?;
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
            let keys = fetch_keys(&client, &config)
                .await
                .context("loading Cloudflare Access signing keys")?;
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
            tokio::spawn(async move {
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
            team_domain: "test-team.cloudflareaccess.com".into(),
            audience: "test-audience".into(),
            allowed_emails: vec!["owner@example.test".into()],
            upstream,
            bootstrap_owner_email: None,
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
        for origin in [None, Some("https://guest.example.test")] {
            let mut req = request(Some(&signed), Method::GET, Body::empty());
            *req.uri_mut() = "/api/terminal/test".parse().unwrap();
            if let Some(origin) = origin {
                req.headers_mut().insert("origin", origin.parse().unwrap());
            }
            assert_eq!(
                gateway.clone().oneshot(req).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
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
}
