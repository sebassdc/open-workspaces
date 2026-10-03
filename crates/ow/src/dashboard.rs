//! Owner-only workspace controls. Auth, Host and Origin checks run before this handler.
use crate::{runtime, wire};
use anyhow::{Result, bail, ensure};
use axum::{
    Json,
    body::to_bytes,
    extract::Request,
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    op: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    child: Option<String>,
    #[serde(default)]
    snapshot: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    memory_mib: Option<u32>,
    #[serde(default)]
    image: Option<String>,
}

fn validated(bytes: &[u8]) -> Result<Value> {
    let operation: Operation = serde_json::from_slice(bytes)?;
    runtime::identifier(&operation.id)?;
    let mut request = json!({"op":operation.op,"id":operation.id});
    match operation.op.as_str() {
        "create" => {
            let image = operation.image.as_deref().unwrap_or("alpine");
            runtime::image_profile(image)?;
            request["image"] = json!(image);
            let memory = operation.memory_mib.unwrap_or(256);
            ensure!(
                [256, 512, 1024].contains(&memory),
                "Choose 256, 512 or 1024 MiB"
            );
            request["memory_mib"] = json!(memory);
        }
        "start" | "stop" | "hibernate" => {}
        "snapshot" | "restore" => {
            let name = operation
                .name
                .ok_or_else(|| anyhow::anyhow!("Snapshot name required"))?;
            runtime::identifier(&name)?;
            request["name"] = json!(name);
        }
        "fork" => {
            let child = operation
                .child
                .ok_or_else(|| anyhow::anyhow!("Child name required"))?;
            runtime::identifier(&child)?;
            request["child"] = json!(child);
            if let Some(snapshot) = operation.snapshot {
                runtime::identifier(&snapshot)?;
                request["snapshot"] = json!(snapshot);
            }
        }
        "exec" => {
            let command = operation
                .command
                .ok_or_else(|| anyhow::anyhow!("Command required"))?;
            ensure!(
                !command.is_empty() && command.len() <= 2000,
                "Commands must contain 1–2000 bytes"
            );
            request["command"] = json!(command);
        }
        _ => bail!("Unsupported dashboard operation"),
    }
    Ok(request)
}

fn secure(mut response: Response) -> Response {
    let headers = response.headers_mut();
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
        (
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self'; font-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
        ),
    ] {
        headers.insert(name, value.parse().unwrap());
    }
    response
}

pub fn public_path(path: &str) -> bool {
    matches!(
        path,
        "/cli/install.sh"
            | "/cli/ow-linux-amd64"
            | "/cli/ow-linux-amd64.sha256"
            | "/cli/ow-darwin-arm64"
            | "/cli/ow-darwin-arm64.sha256"
            | "/cli/ow-darwin-amd64"
            | "/cli/ow-darwin-amd64.sha256"
    )
}
pub async fn public_download(origin: &str, request: Request) -> Response {
    if !matches!(*request.method(), Method::GET | Method::HEAD)
        || request.uri().query().is_some()
        || request.uri().scheme().is_some()
        || request.uri().authority().is_some()
    {
        return secure((StatusCode::BAD_REQUEST, "Invalid download request").into_response());
    }
    let head = request.method() == Method::HEAD;
    let path = request.uri().path();
    let result: Result<(&str, Vec<u8>)> = match path {
        "/cli/install.sh" => Ok((
            "text/plain; charset=utf-8",
            include_str!("../cli/install.sh")
                .replace("@@ORIGIN@@", origin)
                .into_bytes(),
        )),
        path if public_path(path) => {
            let checksum = path.ends_with(".sha256");
            // The exact whitelist above validates the filename before using it.
            let artifact = crate::assets()
                .join("bin")
                .join(path.trim_start_matches("/cli/"));
            match tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                ensure!(
                    std::fs::metadata(&artifact)?.len() <= 32 * 1024 * 1024,
                    "artifact too large"
                );
                Ok(std::fs::read(artifact)?)
            })
            .await
            {
                Ok(Ok(bytes)) => Ok((
                    if checksum {
                        "text/plain; charset=utf-8"
                    } else {
                        "application/octet-stream"
                    },
                    bytes,
                )),
                _ => Err(anyhow::anyhow!("artifact unavailable")),
            }
        }
        _ => return secure((StatusCode::NOT_FOUND, "Not found").into_response()),
    };
    secure(match result {
        Ok((kind, bytes)) => (
            [("content-type", kind)],
            if head { Vec::new() } else { bytes },
        )
            .into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "CLI build unavailable").into_response(),
    })
}

pub async fn handle(root: &Path, request: Request) -> Response {
    let path = request.uri().path();
    let method = request.method().clone();
    if request.uri().scheme().is_some() || request.uri().authority().is_some() {
        return secure((StatusCode::BAD_REQUEST, "Invalid target").into_response());
    }
    if method == Method::GET {
        let asset = match path {
            "/" => Some(("text/html; charset=utf-8", include_str!("../ui/index.html"))),
            "/app.js" => Some((
                "text/javascript; charset=utf-8",
                include_str!("../ui/app.js"),
            )),
            "/style.css" => Some(("text/css; charset=utf-8", include_str!("../ui/style.css"))),
            "/vendor/xterm.js" => Some((
                "text/javascript; charset=utf-8",
                include_str!("../ui/vendor/xterm.js"),
            )),
            "/vendor/addon-fit.js" => Some((
                "text/javascript; charset=utf-8",
                include_str!("../ui/vendor/addon-fit.js"),
            )),
            "/vendor/xterm.css" => Some((
                "text/css; charset=utf-8",
                include_str!("../ui/vendor/xterm.css"),
            )),
            _ => None,
        };
        if let Some((kind, body)) = asset {
            return secure(([("content-type", kind)], body).into_response());
        }
    }
    let root = root.to_path_buf();
    if path == "/api/state" && method == Method::GET {
        let result = tokio::task::spawn_blocking(move || -> Result<Value> {
            let workspaces = wire::request(&root, json!({"op":"list"}))?;
            let snapshots = wire::request(&root, json!({"op":"snapshots"}))?;
            let stats = wire::request(&root, json!({"op":"stats"}))?;
            Ok(json!({"workspaces":workspaces,"snapshots":snapshots,"stats":stats}))
        })
        .await;
        return secure(match result {
            Ok(Ok(value)) => Json(json!({"ok":true,"result":value})).into_response(),
            _ => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"ok":false,"error":"Workspace worker unavailable. Try refreshing."})),
            )
                .into_response(),
        });
    }
    if path != "/api/operation" {
        return secure((StatusCode::NOT_FOUND, "Not found").into_response());
    }
    if method != Method::POST {
        return secure((StatusCode::METHOD_NOT_ALLOWED, "Use POST").into_response());
    }
    if request
        .headers()
        .get("x-ow-request")
        .and_then(|v| v.to_str().ok())
        != Some("dashboard")
        || request
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            != Some("application/json")
    {
        return secure((StatusCode::FORBIDDEN, "Dashboard JSON request required").into_response());
    }
    let Ok(bytes) = to_bytes(request.into_body(), 16384).await else {
        return secure((StatusCode::PAYLOAD_TOO_LARGE, "Operation too large").into_response());
    };
    let operation = match validated(&bytes) {
        Ok(value) => value,
        Err(_) => {
            return secure(
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"ok":false,"error":"Invalid operation, name or parameters."})),
                )
                    .into_response(),
            );
        }
    };
    let result = tokio::task::spawn_blocking(move || wire::request(&root, operation)).await;
    secure(match result {
        Ok(Ok(value)) => Json(json!({"ok":true,"result":value})).into_response(),
        // Worker diagnostics may contain host paths; keep detailed error text in operator logs.
        Ok(Err(error)) => {
            eprintln!("Dashboard workspace operation failed: {error:#}");
            (StatusCode::CONFLICT,Json(json!({"ok":false,"error":"Operation could not complete. Check machine state, capacity, snapshot ownership and names, then refresh before retrying."}))).into_response()
        }
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok":false,"error":"Worker unavailable; refresh before retrying."})),
        )
            .into_response(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dashboard_cannot_forward_arbitrary_worker_operations() {
        for op in ["shutdown", "tunnel", "put", "get", "shell-exec", "status"] {
            assert!(validated(json!({"op":op,"id":"test"}).to_string().as_bytes()).is_err());
        }
        for body in [
            json!({"op":"create","id":"../host"}),
            json!({"op":"create","id":"test","memory_mib":4294967295_u32}),
            json!({"op":"create","id":"test","image":"../../host"}),
            json!({"op":"exec","id":"test","command":"hi","host_path":"/secret"}),
        ] {
            assert!(validated(body.to_string().as_bytes()).is_err());
        }
        assert!(
            validated(br#"{"op":"fork","id":"parent","child":"child","snapshot":"prepared"}"#)
                .is_ok()
        );
    }
}
