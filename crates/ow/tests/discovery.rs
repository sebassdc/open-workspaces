//! Stdin-closed CLI contract over verified fixture TLS, with no credentials or real VM.
use axum::{
    Json, Router,
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdin_closed_remote_discovery_and_existing_output_contracts() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let root = Fixture(std::env::temp_dir().join(format!("ow-discovery-{}", std::process::id())));
    std::fs::create_dir(&root.0).unwrap();
    assert!(
        Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=localhost",
                "-addext",
                "subjectAltName=DNS:localhost",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-keyout"
            ])
            .arg(root.0.join("key.pem"))
            .arg("-out")
            .arg(root.0.join("cert.pem"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    std::fs::create_dir(root.0.join("open-workspaces")).unwrap();
    std::fs::write(
        root.0.join("open-workspaces/client.json"),
        json!({"server":url,"auth":"tailscale"}).to_string(),
    )
    .unwrap();
    let state = Arc::new(Mutex::new(
        json!({"workspaces":[{"id":"mine","state":"running"}],"shared":[],
        "discovery":{"schema":1,"server_version":"fixture","identity":{"email":"me@example.test"},
        "nodes":[{"id":"remote-a","online":true},{"id":"remote-b","online":false}],
        "images":[{"name":"ubuntu","available":true,"nodes":[{"node":"remote-a","development_choice":{"image":"ubuntu","memory_mib":2048,"vcpu_count":2}}]}]}}),
    ));
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let get_state = state.clone();
    let received = captured.clone();
    let app = Router::new().route("/api/state",get(move || {
        let result=get_state.lock().unwrap().clone();
        async move { if result == "denied" {(StatusCode::FORBIDDEN,Json(json!({"ok":false}))) }
            else {(StatusCode::OK,Json(json!({"ok":true,"result":result})))} }
    })).route("/api/operation",post(move |Json(op):Json<Value>| {
        let mut calls=received.lock().unwrap();
        calls.push(op.clone());
        let create_calls=calls.iter().filter(|c|c["op"]=="create").count();
        async move {
            if op["op"]=="exec" {(StatusCode::OK,Json(json!({"ok":true,"result":{"output":"raw guest output\n","exit_code":23}})))}
            else if create_calls==1 {(StatusCode::SERVICE_UNAVAILABLE,Json(json!({"ok":false,"error":"response uncertain"})))}
            else {(StatusCode::OK,Json(json!({"ok":true,"result":{"id":"demo"}})))}
        }
    }));
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(
        root.0.join("cert.pem"),
        root.0.join("key.pem"),
    )
    .await
    .unwrap();
    let server = tokio::spawn(
        axum_server::from_tcp_rustls(listener, tls)
            .unwrap()
            .serve(app.into_make_service()),
    );
    let run = |args: Vec<String>| {
        let path = root.0.clone();
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_ow"))
                .args(args)
                .env("XDG_CONFIG_HOME", &path)
                .env_remove("OW_SERVER")
                .env("SSL_CERT_FILE", path.join("cert.pem"))
                .stdin(Stdio::null())
                .output()
                .unwrap()
        })
    };
    let operator_root = root.0.join("operator");
    std::fs::create_dir(&operator_root).unwrap();
    std::fs::set_permissions(&operator_root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let remote_args = |command: &[&str]| {
        let mut args = vec![
            "--data-dir".to_owned(),
            operator_root.to_str().unwrap().to_owned(),
            "--server".to_owned(),
            url.clone(),
        ];
        args.extend(command.iter().map(|s| s.to_string()));
        args
    };
    let out = run(remote_args(&["nodes"])).await.unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let nodes: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(nodes[0]["id"], "remote-a");
    assert_eq!(
        nodes.as_array().unwrap().len(),
        2,
        "explicit remote must not fall through to local []"
    );
    let local_root = root.0.join("local");
    std::fs::create_dir(&local_root).unwrap();
    std::fs::set_permissions(&local_root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let out = run(vec![
        "--local".into(),
        "--data-dir".into(),
        local_root.to_str().unwrap().into(),
        "nodes".into(),
    ])
    .await
    .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap(),
        json!([])
    );
    let out = run(remote_args(&["image", "list"])).await.unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()[0]["name"],
        "ubuntu"
    );
    let out = run(vec!["status".into()]).await.unwrap();
    let status: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["server"], url);
    assert_eq!(status["identity"]["email"], "me@example.test");
    assert_eq!(status["compatibility"]["discovery_supported"], true);
    let out = run(remote_args(&["list"])).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap(),
        json!([{"id":"mine","state":"running"}])
    );
    let out = run(remote_args(&["inspect", "mine"])).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["id"],
        "mine"
    );
    let out = run(remote_args(&["exec", "mine", "--", "exit 23"]))
        .await
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(23),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"raw guest output\n");
    let out = run(remote_args(&[
        "create", "demo", "--image", "ubuntu", "--memory", "2048", "--cpus", "2",
    ]))
    .await
    .unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let key = captured.lock().unwrap().last().unwrap()["operation_key"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(String::from_utf8_lossy(&out.stderr).contains(&format!("--operation-key {key}")));
    assert!(
        root.0
            .join("open-workspaces/operations")
            .join(format!("{key}.json"))
            .exists()
    );
    let mut retry = remote_args(&[
        "--operation-key",
        &key,
        "create",
        "demo",
        "--image",
        "ubuntu",
        "--memory",
        "2048",
        "--cpus",
        "2",
    ]);
    let out = run(std::mem::take(&mut retry)).await.unwrap();
    assert!(out.status.success());
    let calls = captured.lock().unwrap();
    assert_eq!(calls[1], calls[2]);
    drop(calls);
    *state.lock().unwrap() = json!({"workspaces":[],"nodes":[{"id":"raw-host-secret"}]});
    let out = run(remote_args(&["nodes"])).await.unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unsupported_capability"));
    let out = run(remote_args(&["status"])).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["compatibility"]["discovery_supported"],
        false
    );
    *state.lock().unwrap() = json!("denied");
    let out = run(remote_args(&["image", "list"])).await.unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("authentication_or_admission"));
    server.abort();
    let out = run(remote_args(&["nodes"])).await.unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("server_unreachable"));
    let out = run(vec!["--help".into()]).await.unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Remote first use"));
}

#[test]
fn explicit_remote_nodes_and_images_validate_remote_target_before_local_dispatch() {
    let root =
        Fixture(std::env::temp_dir().join(format!("ow-discovery-routing-{}", std::process::id())));
    std::fs::create_dir(&root.0).unwrap();
    std::fs::set_permissions(&root.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut diagnostics = Vec::new();
    for command in [vec!["nodes"], vec!["image", "list"]] {
        let out = Command::new(env!("CARGO_BIN_EXE_ow"))
            .arg("--data-dir")
            .arg(&root.0)
            .args(["--server", "http://invalid.example"])
            .args(command)
            .env("XDG_CONFIG_HOME", &root.0)
            .env_remove("OW_SERVER")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        diagnostics.push((
            out.status.success(),
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        ));
    }
    assert!(
        diagnostics.iter().all(|(success, out, error)| !success
            && out.is_empty()
            && error.contains("server must be an HTTPS origin")),
        "{diagnostics:?}"
    );
}
