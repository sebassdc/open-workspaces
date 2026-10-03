//! User-scoped workspace controls. Auth, Host and Origin checks run before this handler.
use crate::{
    catalog::{Catalog, Identity},
    runtime,
};
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
use std::sync::{Arc, Mutex};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    op: String,
    #[serde(default)]
    node: Option<String>,
    #[serde(default)]
    operation_key: Option<String>,
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
    path: Option<String>,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    memory_mib: Option<u32>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    vcpu_count: Option<u32>,
}

fn validated(bytes: &[u8]) -> Result<Value> {
    let operation: Operation = serde_json::from_slice(bytes)?;
    ensure!(
        matches!(operation.op.as_str(), "put" | "get") || bytes.len() <= 16384,
        "operation too large"
    );
    runtime::identifier(&operation.id)?;
    let mut request = json!({"op":operation.op,"id":operation.id});
    if let Some(key) = operation.operation_key {
        runtime::identifier(&key)?;
        request["operation_key"] = json!(key);
    }
    if let Some(node) = operation.node {
        runtime::identifier(&node)?;
        request["node"] = json!(node);
    }
    match operation.op.as_str() {
        "create" => {
            let image = operation.image.as_deref().unwrap_or("alpine");
            runtime::image_profile(image)?;
            request["image"] = json!(image);
            let memory = operation.memory_mib.unwrap_or(256);
            ensure!(
                [256, 512, 1024, 2048].contains(&memory),
                "Choose 256, 512, 1024 or 2048 MiB"
            );
            request["memory_mib"] = json!(memory);
            let cpus = operation.vcpu_count.unwrap_or(1);
            ensure!((1..=4).contains(&cpus), "Choose 1–4 CPUs");
            request["vcpu_count"] = json!(cpus);
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
        "put" | "get" => {
            let path = operation
                .path
                .ok_or_else(|| anyhow::anyhow!("guest path required"))?;
            ensure!(
                !path.contains('\n') && path.len() < 500,
                "invalid guest path"
            );
            request["path"] = json!(path);
            if operation.op == "put" {
                use base64::Engine;
                let data = operation
                    .data
                    .ok_or_else(|| anyhow::anyhow!("file bytes required"))?;
                ensure!(
                    data.len() <= 350000
                        && base64::engine::general_purpose::STANDARD
                            .decode(&data)?
                            .len()
                            <= 262144,
                    "file too large"
                );
                request["data"] = json!(data);
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

pub async fn with_catalog<T: Send + 'static>(
    catalog: Option<Arc<Mutex<Catalog>>>,
    identity: Identity,
    operation: impl FnOnce(&mut Catalog, i64) -> Result<T> + Send + 'static,
) -> Result<T> {
    let catalog = catalog.ok_or_else(|| anyhow::anyhow!("ownership catalog unavailable"))?;
    tokio::task::spawn_blocking(move || {
        let mut catalog = catalog
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog lock poisoned"))?;
        let user = catalog.user(&identity)?;
        operation(&mut catalog, user)
    })
    .await?
}

pub async fn handle(
    catalog: Option<Arc<Mutex<Catalog>>>,
    identity: Identity,
    request: Request,
) -> Response {
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
    if path == "/api/state" && method == Method::GET {
        let result = with_catalog(catalog, identity, |catalog, user| catalog.state(user)).await;
        return secure(match result {
            Ok(value) => Json(json!({"ok":true,"result":value})).into_response(),
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
    let Ok(bytes) = to_bytes(request.into_body(), 384 * 1024).await else {
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
    let result = with_catalog(catalog, identity, move |catalog, user| {
        catalog.operation(user, operation)
    })
    .await;
    secure(match result {
        Ok(value) => Json(json!({"ok":true,"result":value})).into_response(),
        // Worker diagnostics may contain host paths; keep detailed error text in operator logs.
        Err(error) => {
            eprintln!("Dashboard workspace operation failed: {error:#}");
            if let Some(failure) = error.downcast_ref::<crate::catalog::OperationFailure>() {
                return secure((StatusCode::CONFLICT,Json(json!({"ok":false,"error":"Operation did not complete. Check its status before retrying; use the same retry key.","operation":failure.0}))).into_response());
            }
            (StatusCode::CONFLICT,Json(json!({"ok":false,"error":"Operation could not complete. Check machine state, capacity, snapshot ownership and names, then refresh before retrying."}))).into_response()
        }
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
