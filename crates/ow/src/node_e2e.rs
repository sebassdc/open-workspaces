use super::*;
use crate::catalog::{Catalog, Identity};
use std::process::{Child, Command, Stdio};
struct Cleanup {
    binary: PathBuf,
    workers: Vec<PathBuf>,
    children: Vec<Child>,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        for root in &self.workers {
            let _ = Command::new(&self.binary)
                .args(["--local", "--data-dir", root.to_str().unwrap(), "down"])
                .output();
        }
    }
}
fn command(binary: &Path, root: &Path, args: &[&str]) -> Value {
    let result = Command::new(binary)
        .args(["--local", "--data-dir", root.to_str().unwrap()])
        .args(args)
        .env("OW_MAX_MEMORY_MIB", "512")
        .env("OW_MAX_RUNNING", "2")
        .env("OW_MAX_VCPUS", "2")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap_or(Value::Null)
}
fn start(binary: &Path, root: &Path, args: &[&str]) -> Child {
    Command::new(binary)
        .args(["--local", "--data-dir", root.to_str().unwrap()])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}
fn wait_online(root: &Path, node: &str) {
    let start = std::time::Instant::now();
    while online(root, node).is_err()
        || !root.join("nodes").join(node).join("control.sock").exists()
    {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "agent did not come online"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
#[test]
#[ignore = "Real two-worker guests; requires built OW_NODE_TEST_BINARY, KVM/assets and exclusive host test turn"]
fn two_worker_tls_real_guests() {
    // Refuse overlapping host acceptance, including unknown configurations.
    let mut running = 0;
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
        {
            if let Ok(comm) = fs::read_to_string(entry.path().join("comm")) {
                if comm.contains("firecracker") {
                    running += 1;
                }
            }
        }
    }
    assert_eq!(
        running, 0,
        "real-node suite requires an empty host guest reservation before starting"
    );
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let binary = std::env::var_os("OW_NODE_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or(repo.join("target/debug/ow"));
    assert!(binary.is_file());
    let base = std::env::var_os("OW_NODE_TEST_BASE")
        .map(PathBuf::from)
        .unwrap_or(crate::assets().parent().unwrap().to_owned());
    let root = std::env::var_os("OW_NODE_TEST_DATA_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| base.join(format!("nc{}", &common::nonce().unwrap()[..8])));
    assert!(
        !root.exists(),
        "test root must be new; existing data is never adopted"
    );
    fs::create_dir_all(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let ctl = root.join("c");
    let alpha = root.join("a");
    let beta = root.join("b");
    let mut cleanup = Cleanup {
        binary: binary.clone(),
        workers: vec![alpha.clone(), beta.clone()],
        children: vec![],
    };
    command(&binary, &alpha, &["up"]);
    command(&binary, &beta, &["up"]);
    assert_eq!(
        wire::request(&alpha, json!({"op":"status"})).unwrap()["max_memory_mib"],
        512
    );
    // Initialize controller root before credentials are written; all three roots are dedicated.
    command(&binary, &ctl, &["nodes"]);
    let cert = root.join("cert.pem");
    let key = root.join("key.pem");
    let result = Command::new("openssl")
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
            "-keyout",
            key.to_str().unwrap(),
            "-out",
            cert.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(result.success());
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let listen = format!("127.0.0.1:{port}");
    let origin = format!("https://localhost:{port}");
    cleanup.children.push(start(
        &binary,
        &ctl,
        &[
            "node-controller",
            "--listen",
            &listen,
            "--tls-cert",
            cert.to_str().unwrap(),
            "--tls-key",
            key.to_str().unwrap(),
        ],
    ));
    std::thread::sleep(Duration::from_millis(400));
    for (node, worker) in [("alpha", &alpha), ("beta", &beta)] {
        let join = worker.join("join.json");
        let credential = worker.join("credential.json");
        mint(&ctl, node, &join, 60, 512, 2).unwrap();
        cleanup.children.push(start(
            &binary,
            worker,
            &[
                "node-agent",
                "--controller",
                &origin,
                "--credential",
                credential.to_str().unwrap(),
                "--join",
                join.to_str().unwrap(),
                "--ca-cert",
                cert.to_str().unwrap(),
            ],
        ));
        wait_online(&ctl, node);
    }
    // Private CA and hostname checks use the real TLS controller.
    let credentials: Value =
        serde_json::from_slice(&fs::read(alpha.join("credential.json")).unwrap()).unwrap();
    let token = credentials["credential"].as_str().unwrap();
    assert!(socket(&origin, "/node/alpha", token, None).is_err());
    assert!(
        socket(
            &format!("https://127.0.0.1:{port}"),
            "/node/alpha",
            token,
            Some(&cert)
        )
        .is_err()
    );
    let mut c = Catalog::open(&ctl, "test", None).unwrap();
    let a = c
        .user(&Identity {
            issuer: "test".into(),
            subject: "alice".into(),
            email: "alice@example.test".into(),
        })
        .unwrap();
    let b = c
        .user(&Identity {
            issuer: "test".into(),
            subject: "bob".into(),
            email: "bob@example.test".into(),
        })
        .unwrap();
    let create = json!({"op":"create","id":"alpha-box","node":"alpha","image":"alpine","memory_mib":256,"vcpu_count":1,"operation_key":"create-alpha"});
    let first = c.operation(a, create.clone()).unwrap();
    let auto=c.operation(a,json!({"op":"create","id":"auto-box","image":"alpine","memory_mib":256,"vcpu_count":1,"operation_key":"auto"})).unwrap();
    assert_eq!(auto["node"], "beta");
    assert_eq!(
        c.operation(a, create).unwrap()["operation_id"],
        first["operation_id"]
    );
    assert!(
        c.operation(b, json!({"op":"exec","id":"alpha-box","command":"true"}))
            .is_err()
    );
    assert!(c.terminal_target(b, "alpha-box").is_err());
    let result=c.operation(a,json!({"op":"exec","id":"alpha-box","command":"uname -s; echo node-alpha","operation_key":"exec-alpha"})).unwrap();
    assert!(result["output"].as_str().unwrap().contains("Linux"));
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(b"persistent-node-data\n");
    c.operation(
        a,
        json!({"op":"put","id":"alpha-box","path":"/root/node-data","data":encoded}),
    )
    .unwrap();
    let bytes = c
        .operation(
            a,
            json!({"op":"get","id":"alpha-box","path":"/root/node-data"}),
        )
        .unwrap();
    assert_eq!(bytes["data"], encoded);
    c.operation(a, json!({"op":"stop","id":"alpha-box"}))
        .unwrap();
    c.operation(
        a,
        json!({"op":"start","id":"alpha-box","operation_key":"cold-start"}),
    )
    .unwrap();
    let result = c
        .operation(
            a,
            json!({"op":"exec","id":"alpha-box","command":"cat /root/node-data"}),
        )
        .unwrap();
    assert!(
        result["output"]
            .as_str()
            .unwrap()
            .contains("persistent-node-data")
    );
    c.operation(a, json!({"op":"snapshot","id":"alpha-box","name":"one"}))
        .unwrap();
    c.operation(a, json!({"op":"snapshot","id":"alpha-box","name":"two"}))
        .unwrap();
    let fork=c.operation(a,json!({"op":"fork","id":"alpha-box","child":"child","snapshot":"one","operation_key":"fork-one"})).unwrap();
    assert_eq!(fork["node"], "alpha");
    assert!(c.operation(a,json!({"op":"fork","id":"alpha-box","child":"child","snapshot":"two","operation_key":"fork-one"})).is_err());
    c.operation(
        a,
        json!({"op":"exec","id":"child","command":"echo independent > /root/node-data"}),
    )
    .unwrap();
    let parent = c
        .operation(
            a,
            json!({"op":"exec","id":"alpha-box","command":"cat /root/node-data"}),
        )
        .unwrap();
    assert!(
        parent["output"]
            .as_str()
            .unwrap()
            .contains("persistent-node-data")
    );
    // Actual guest PTY through the node channel, using existing terminal framing.
    let (target, id) = c.terminal_target(a, "alpha-box").unwrap();
    let (mut pty, _) = wire::connect(&target, &json!({"op":"terminal","id":id})).unwrap();
    pty.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let input = b"tty; echo NODE_PTY_OK; exit\n";
    pty.write_all(&[0]).unwrap();
    pty.write_all(&(input.len() as u32).to_be_bytes()).unwrap();
    pty.write_all(input).unwrap();
    let mut output = Vec::new();
    loop {
        let mut header = [0; 5];
        pty.read_exact(&mut header).unwrap();
        let n = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
        assert!(n <= 4096);
        let mut data = vec![0; n];
        pty.read_exact(&mut data).unwrap();
        if header[0] == 2 {
            break;
        }
        output.extend(data);
        assert!(output.len() < 65536);
    }
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("/dev/pts/"));
    assert!(text.contains("NODE_PTY_OK"));
    // Agent disconnect leaves fixed placement; another live worker never adopts the guest.
    cleanup.children[1].kill().unwrap();
    cleanup.children[1].wait().unwrap();
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        c.operation(a, json!({"op":"exec","id":"alpha-box","command":"true"}))
            .is_err()
    );
    let state = c.state(a).unwrap();
    assert!(
        state["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "alpha-box" && m["node"] == "alpha" && m["state"] == "offline")
    );
    let credential = alpha.join("credential.json");
    cleanup.children.push(start(
        &binary,
        &alpha,
        &[
            "node-agent",
            "--controller",
            &origin,
            "--credential",
            credential.to_str().unwrap(),
            "--ca-cert",
            cert.to_str().unwrap(),
        ],
    ));
    wait_online(&ctl, "alpha");
    // Controller restart and catalog reopen preserve placement and the same operation identity.
    drop(c);
    cleanup.children[0].kill().unwrap();
    cleanup.children[0].wait().unwrap();
    cleanup.children.push(start(
        &binary,
        &ctl,
        &[
            "node-controller",
            "--listen",
            &listen,
            "--tls-cert",
            cert.to_str().unwrap(),
            "--tls-key",
            key.to_str().unwrap(),
        ],
    ));
    std::thread::sleep(Duration::from_secs(1));
    wait_online(&ctl, "alpha");
    wait_online(&ctl, "beta");
    let mut c = Catalog::open(&ctl, "test", None).unwrap();
    let result = c
        .operation(
            a,
            json!({"op":"exec","id":"alpha-box","command":"cat /root/node-data"}),
        )
        .unwrap();
    assert!(
        result["output"]
            .as_str()
            .unwrap()
            .contains("persistent-node-data")
    );
    revoke(&ctl, "alpha").unwrap();
    assert!(
        c.operation(a, json!({"op":"start","id":"alpha-box"}))
            .is_err()
    );
    assert!(c.terminal_target(a, "alpha-box").is_err());
    // Direct operator shutdown is used only for these dedicated disposable worker roots.
    drop(c);
    drop(cleanup);
    fs::write(root.join("result.json"),json!({"passed":true,"topology":"same-host two-worker TLS","max_test_guests":3,"max_test_reserved_mib":768,"configured_worker_budget_sum_mib":1024,"checks":["TLS/private CA/hostname denial","two node enrollment","selected/automatic placement","duplicate create","ownership denial","guest exec/files","cold persistence","two captures","same-node fork independence","changed fork fingerprint","real PTY","offline fixed placement","reconnect","controller/catalog restart","revocation"],"cleanup":"both dedicated workers and all controller/agent children stopped"}).to_string()).unwrap();
    eprintln!("Real-node result: {}", root.join("result.json").display());
}
