//! Authenticated HTTP and opaque WebSocket upgrades over the existing guest TCP tunnel.
use anyhow::{Result, ensure};
use axum::{
    body::{Body, to_bytes},
    extract::Request,
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
};
use hyper_util::rt::TokioIo;
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Notify, Semaphore};

pub struct Target {
    pub root: PathBuf,
    pub id: String,
    pub port: u16,
    pub revoked: Arc<Notify>,
    pub unpublished: Arc<Notify>,
}

pub fn serve(port: u16, enabled: bool) -> Result<()> {
    ensure!(port >= 11000, "invalid preview HTTPS port");
    // Fixed executable and validated numeric arguments; no user-controlled command or target.
    let status = std::process::Command::new("/usr/bin/sudo")
        .args([
            "-n",
            "/usr/local/lib/open-workspaces/preview-serve",
            &port.to_string(),
            if enabled { "on" } else { "off" },
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()?;
    ensure!(
        status.success(),
        "preview Serve route could not be configured"
    );
    Ok(())
}

pub fn urls(value: &mut serde_json::Value, hostname: &str) {
    if let Some(port) = value["https_port"].as_u64() {
        value["url"] = serde_json::json!(format!("https://{hostname}:{port}/"));
    }
    if let Some(ports) = value["published_ports"].as_array_mut() {
        for port in ports {
            urls(port, hostname);
        }
    }
}

pub async fn proxy(target: Target, mut request: Request, expires: u64, host: &str) -> Response {
    if request.uri().scheme().is_some()
        || request.uri().authority().is_some()
        || matches!(*request.method(), Method::CONNECT | Method::TRACE)
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let origin = format!("https://{host}");
    let supplied = request
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok());
    let websocket = request.headers().get("upgrade").is_some();
    if (supplied.is_some() && supplied != Some(origin.as_str()))
        || ((!matches!(
            *request.method(),
            Method::GET | Method::HEAD | Method::OPTIONS
        ) || websocket)
            && supplied != Some(origin.as_str()))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    if websocket
        && (request.method() != Method::GET
            || !request
                .headers()
                .get("upgrade")
                .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket")))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let Ok(permit) = SLOTS
        .get_or_init(|| Arc::new(Semaphore::new(64)))
        .clone()
        .try_acquire_owned()
    else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let downstream = websocket.then(|| hyper::upgrade::on(&mut request));
    let (parts, body) = request.into_parts();
    let Ok(bytes) = to_bytes(body, 1024 * 1024).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let root = target.root.clone();
    let id = target.id.clone();
    let port = target.port;
    let stream = tokio::task::spawn_blocking(move || {
        crate::wire::connect(
            &root,
            &serde_json::json!({"op":"tunnel","id":id,"port":port}),
        )
    })
    .await;
    let Ok(Ok((stream, _))) = stream else {
        return (StatusCode::BAD_GATEWAY, "Workspace service unavailable").into_response();
    };
    if stream.set_nonblocking(true).is_err() {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let Ok(stream) = tokio::net::UnixStream::from_std(stream) else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let Ok((mut sender, connection)) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await
    else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let revoked = target.revoked;
    let unpublished = target.unpublished;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let lifetime = Duration::from_secs(expires.saturating_sub(now).min(8 * 3600));
    let (mut finished, mut cancelled) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        tokio::select! {
            _ = revoked.notified() => {},
            _ = unpublished.notified() => {},
            _ = tokio::time::sleep(lifetime) => {},
            _ = async {
                // Hyper returns the upgraded stream; keep authorization and the slot for its lifetime.
                let result = connection.with_upgrades().await;
                if result.is_ok() { let _ = (&mut cancelled).await; }
            } => {},
        }
        drop(permit);
    });
    let mut headers = crate::remote::clean_headers(&parts.headers, true);
    // Cookies have no port isolation. Do not let one preview steal another preview's cookies.
    headers.remove("cookie");
    headers.insert("host", host.parse().unwrap());
    if websocket {
        headers.insert("connection", "upgrade".parse().unwrap());
        headers.insert("upgrade", "websocket".parse().unwrap());
    }
    let mut upstream = Request::new(Body::from(bytes));
    *upstream.method_mut() = parts.method;
    *upstream.uri_mut() = parts.uri;
    *upstream.headers_mut() = headers;
    let response =
        tokio::time::timeout(Duration::from_secs(30), sender.send_request(upstream)).await;
    let Ok(Ok(mut response)) = response else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let upgrade = response.status() == StatusCode::SWITCHING_PROTOCOLS;
    if upgrade && !websocket {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let mut headers = crate::remote::clean_headers(response.headers(), false);
    headers.remove("set-cookie");
    headers.remove("access-control-allow-origin");
    headers.remove("access-control-allow-credentials");
    headers.insert("cache-control", "no-store".parse().unwrap());
    headers.insert("referrer-policy", "no-referrer".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    if upgrade {
        let upstream = hyper::upgrade::on(&mut response);
        headers.insert("connection", "upgrade".parse().unwrap());
        headers.insert("upgrade", "websocket".parse().unwrap());
        tokio::spawn(async move {
            let (Ok(downstream), Ok(upstream)) = tokio::join!(downstream.unwrap(), upstream) else {
                return;
            };
            let mut down = TokioIo::new(downstream);
            let mut up = TokioIo::new(upstream);
            let relay = tokio::io::copy_bidirectional(&mut down, &mut up);
            tokio::pin!(relay);
            tokio::select! { _ = &mut relay => {}, _ = finished.closed() => {} }
        });
    } else {
        // The connection task owns the stream and closes on grant revocation or expiry.
        drop(finished);
    }
    let mut result = response.map(Body::new);
    *result.headers_mut() = headers;
    result
}
