//! User-scoped workspace controls. Auth, Host and Origin checks run before this handler.
use crate::{
    catalog::{Catalog, Identity},
    runtime,
};
use anyhow::{Context, Result, bail, ensure};
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
    artifact: Option<String>,
    #[serde(default)]
    accept_cold: Option<bool>,
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
    disk_gib: Option<u32>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    vcpu_count: Option<u32>,
    #[serde(default)]
    keys: Option<Vec<String>>,
    #[serde(default)]
    upgrade: Option<bool>,
    #[serde(default)]
    with: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    port: Option<u16>,
}

fn validated(bytes: &[u8]) -> Result<Value> {
    let raw: Value = serde_json::from_slice(bytes)?;
    if matches!(raw["op"].as_str(), Some("create" | "resize")) {
        crate::common::requested_resources(&raw)?;
        crate::common::requested_disk(&raw)?;
    }
    if matches!(raw["op"].as_str(), Some("publish" | "unpublish"))
        && let Some(port) = raw.get("port")
    {
        ensure!(
            port.as_u64().is_some_and(|p| (1..=65535).contains(&p)),
            "invalid port"
        );
    }
    let operation: Operation = serde_json::from_value(raw)?;
    ensure!(
        matches!(operation.op.as_str(), "put" | "get") || bytes.len() <= 16384,
        "operation too large"
    );
    crate::common::address(&operation.id)?;
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
            if image != "ubuntu-arm64" {
                runtime::image_profile(image)?;
            }
            request["image"] = json!(image);
            let memory = operation.memory_mib.unwrap_or(256);
            let cpus = operation.vcpu_count.unwrap_or(1);
            crate::common::resources(memory, cpus)?;
            request["memory_mib"] = json!(memory);
            request["vcpu_count"] = json!(cpus);
            if let Some(disk) = operation
                .disk_gib
                .or((image != "ubuntu-arm64").then_some(crate::common::DEFAULT_DISK_GIB))
            {
                request["disk_gib"] = json!(disk);
            }
        }
        "resize" => {
            ensure!(
                operation.memory_mib.is_some()
                    || operation.vcpu_count.is_some()
                    || operation.disk_gib.is_some(),
                "specify memory_mib, vcpu_count or disk_gib"
            );
            for (key, value) in [
                ("memory_mib", operation.memory_mib),
                ("vcpu_count", operation.vcpu_count),
                ("disk_gib", operation.disk_gib),
            ] {
                if let Some(value) = value {
                    request[key] = json!(value);
                }
            }
        }
        "start" | "stop" | "hibernate" | "ssh-info" | "delete" => {}
        "publish" | "unpublish" => {
            if let Some(port) = operation.port {
                ensure!(port > 0, "invalid port");
                request["port"] = json!(port);
            } else {
                ensure!(operation.op == "unpublish", "port required");
            }
        }
        "share" | "unshare" => {
            let with = operation.with.context("teammate email required")?;
            crate::common::email(&with)?;
            request["with"] = json!(with);
            if let Some(role) = operation.role.filter(|_| operation.op == "share") {
                ensure!(
                    matches!(role.as_str(), "use" | "admin"),
                    "role must be use or admin"
                );
                request["role"] = json!(role);
            }
        }
        "ssh-keys" => {
            let keys =
                crate::ssh_guest::validate_keys(&json!(operation.keys.context("keys required")?))?;
            request["keys"] = json!(keys);
            request["upgrade"] = json!(operation.upgrade.unwrap_or(false));
        }
        "restore" if operation.artifact.is_some() => {
            ensure!(operation.name.is_none(), "choose a snapshot or artifact");
            let artifact = operation.artifact.unwrap();
            runtime::identifier(&artifact)?;
            ensure!(request["node"].is_string(), "target node required");
            ensure!(
                request["operation_key"].is_string(),
                "operation key required"
            );
            request["artifact"] = json!(artifact);
            request["accept_cold"] = json!(operation.accept_cold.unwrap_or(false));
        }
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
    crate::onboarding::public_path(path)
        || matches!(
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
    if crate::onboarding::public_path(path) {
        return secure(crate::onboarding::public_download(path, head).await);
    }
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
    root: Option<std::path::PathBuf>,
    identity: Identity,
    request: Request,
    host_controls: bool,
    preview_host: Option<String>,
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
        let result = with_catalog(catalog, identity, move |catalog, user| {
            let mut state = catalog.state(user)?;
            state["previews"] = json!(preview_host.is_some());
            for group in ["workspaces", "shared"] {
                for item in state[group].as_array_mut().into_iter().flatten() {
                    if let Some(host) = &preview_host {
                        crate::preview::urls(item, host);
                    } else {
                        item["published_ports"] = json!([]);
                    }
                }
            }
            state["host_controls"] = json!(host_controls);
            state["pool_policy"] = json!(crate::nodes::POOL_POLICY);
            Ok(state)
        })
        .await;
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
    let result = if matches!(operation["op"].as_str(), Some("share" | "unshare")) {
        // Not through with_catalog: a slow worker call there must not delay a revocation.
        match root {
            Some(root) => tokio::task::spawn_blocking(move || {
                crate::catalog::share_unlocked(&root, &identity, &operation)
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r),
            None => Err(anyhow::anyhow!("ownership catalog unavailable")),
        }
    } else if operation["op"] == "restore" && operation["artifact"].is_string() {
        match catalog {
            Some(catalog) => tokio::task::spawn_blocking(move || {
                crate::transfers::restore(catalog, &identity, &operation)
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r),
            None => Err(anyhow::anyhow!("ownership catalog unavailable")),
        }
    } else if matches!(operation["op"].as_str(), Some("publish" | "unpublish")) {
        match preview_host {
            Some(host) => {
                with_catalog(catalog, identity, move |catalog, user| {
                    let mut value = catalog.publication(user, &operation, crate::preview::serve)?;
                    crate::preview::urls(&mut value, &host);
                    Ok(value)
                })
                .await
            }
            None => Err(anyhow::anyhow!("tailnet previews are not configured")),
        }
    } else {
        with_catalog(catalog, identity, move |catalog, user| {
            catalog.operation(user, operation)
        })
        .await
    };
    secure(match result {
        Ok(value) => Json(json!({"ok":true,"result":value})).into_response(),
        // Worker diagnostics may contain host paths; keep detailed error text in operator logs.
        Err(error) => {
            eprintln!("Dashboard workspace operation failed: {error:#}");
            if let Some(missing) = error.downcast_ref::<crate::catalog::ResourceNotFound>() {
                return secure(
                    (
                        StatusCode::NOT_FOUND,
                        Json(json!({"ok":false,"error_code":"not_found","error":missing.to_string()})),
                    )
                        .into_response(),
                );
            }
            if let Some(rejection) = error.downcast_ref::<crate::limits::Rejection>() {
                return secure(
                    (
                        StatusCode::CONFLICT,
                        Json(json!({"ok":false,"error":rejection.to_string()})),
                    )
                        .into_response(),
                );
            }
            if let Some(failure) = error.downcast_ref::<crate::catalog::OperationFailure>() {
                return secure(
                    (
                        StatusCode::CONFLICT,
                        Json(json!({"ok":false,"error":failure.message(),"operation":failure.0})),
                    )
                        .into_response(),
                );
            }
            if let Some((code, message)) = crate::catalog::first_use_error(&error.to_string()) {
                return secure(
                    (
                        StatusCode::CONFLICT,
                        Json(json!({"ok":false,"error_code":code,"error":message})),
                    )
                        .into_response(),
                );
            }
            (StatusCode::CONFLICT,Json(json!({"ok":false,"error":"Operation could not complete. Check machine state, capacity, snapshot ownership and names, then refresh before retrying."}))).into_response()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn managed_restore_accepts_only_explicit_catalog_artifact_target_and_key() {
        let valid = json!({"op":"restore","id":"box","artifact":"capture","node":"target","operation_key":"retry","accept_cold":true});
        assert_eq!(
            validated(&serde_json::to_vec(&valid).unwrap()).unwrap(),
            valid
        );
        for field in ["artifact", "node", "operation_key"] {
            let mut invalid = valid.clone();
            invalid.as_object_mut().unwrap().remove(field);
            assert!(validated(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
        let mut both = valid.clone();
        both["name"] = json!("snapshot");
        assert!(validated(&serde_json::to_vec(&both).unwrap()).is_err());
        let mut invalid = valid.clone();
        invalid["accept_cold"] = json!("yes");
        assert!(validated(&serde_json::to_vec(&invalid).unwrap()).is_err());
        assert!(validated(br#"{"op":"restore","id":"box","name":"local-snapshot"}"#).is_ok());
    }

    #[test]
    fn preview_ports_are_explicit_valid_numbers() {
        assert_eq!(
            validated(br#"{"op":"unpublish","id":"demo"}"#).unwrap(),
            json!({"op":"unpublish","id":"demo"})
        );
        assert!(validated(br#"{"op":"publish","id":"demo","port":3000}"#).is_ok());
        assert!(validated(br#"{"op":"publish","id":"demo"}"#).is_err());
        for op in ["publish", "unpublish"] {
            for port in [
                json!(null),
                json!(0),
                json!(65536),
                json!(-1),
                json!(1.5),
                json!("3000"),
            ] {
                assert!(
                    validated(
                        json!({"op":op,"id":"demo","port":port})
                            .to_string()
                            .as_bytes()
                    )
                    .is_err()
                );
            }
        }
    }
    #[test]
    fn larger_shapes_and_malformed_resources() {
        for op in ["create", "resize"] {
            assert!(
                validated(
                    json!({"op":op,"id":"large","memory_mib":4096,"vcpu_count":2})
                        .to_string()
                        .as_bytes()
                )
                .is_ok()
            );
            for bad in [
                json!(null),
                json!("2"),
                json!(-1),
                json!(1.5),
                json!(u64::MAX),
            ] {
                for key in ["memory_mib", "vcpu_count", "disk_gib"] {
                    let mut v = json!({"op":op,"id":"large","memory_mib":4096,"vcpu_count":2});
                    v[key] = bad.clone();
                    assert!(validated(v.to_string().as_bytes()).is_err(), "{v}");
                }
            }
        }
    }
    #[test]
    fn disk_only_resize_and_default_create() {
        let r = validated(br#"{"op":"resize","id":"box","disk_gib":64}"#).unwrap();
        assert_eq!(r["disk_gib"], 64);
        assert!(r.get("memory_mib").is_none());
        assert!(r.get("vcpu_count").is_none());
        assert!(validated(br#"{"op":"resize","id":"box"}"#).is_err());
        assert_eq!(
            validated(br#"{"op":"create","id":"box"}"#).unwrap()["disk_gib"],
            32
        );
        assert!(
            validated(br#"{"op":"create","id":"box","image":"ubuntu-arm64"}"#)
                .unwrap()
                .get("disk_gib")
                .is_none()
        );
    }
    #[test]
    fn dashboard_cannot_forward_arbitrary_worker_operations() {
        for op in [
            "shutdown",
            "quiesce",
            "recovery-prepare",
            "recovery-result",
            "tunnel",
            "put",
            "get",
            "shell-exec",
            "status",
        ] {
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
        assert_eq!(
            validated(br#"{"op":"delete","id":"old","command":"x"}"#).unwrap(),
            json!({"op":"delete","id":"old"})
        );
    }
}
