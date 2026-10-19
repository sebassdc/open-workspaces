use super::*;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

fn private_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ow-selfhost-{name}-{}", common::nonce().unwrap()));
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// Stands in for OpenSSH: drops client options and runs the remote command locally.
fn fake_ssh(dir: &Path) -> PathBuf {
    let path = dir.join("ssh");
    fs::write(
        &path,
        "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in --) shift; break;; -O) exit 0;; *) shift;; esac; done\nshift\nexec sh -c \"$1\"\n",
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn session(dir: &Path) -> Session {
    Session {
        program: fake_ssh(dir),
        control: dir.join("control"),
        args: vec!["--".into(), "runner".into()],
    }
}

#[test]
fn auth_key_sources() {
    let dir = private_dir("key");
    let key = dir.join("key");
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&key)
        .unwrap()
        .write_all(b"tskey-auth-abc123\n")
        .unwrap();
    assert_eq!(
        auth_key(&format!("file:{}", key.display())).unwrap(),
        "tskey-auth-abc123"
    );
    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(auth_key(&format!("file:{}", key.display())).is_err());
    assert!(auth_key("tskey-auth-abc123").is_err());
    assert!(auth_key("env:OW_SELFHOST_TEST_UNSET_KEY").is_err());
    assert_eq!(
        auth_key("env:HOME").unwrap(),
        std::env::var("HOME").unwrap()
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn remote_scripts_quote_and_verify() {
    let dir = private_dir("ssh");
    let ssh = session(&dir);
    // Quoting survives spaces and single quotes in arguments.
    assert_eq!(
        ssh.run(r#"printf %s "$1""#, &["it's a dir"], Stdio::null(), &[])
            .unwrap(),
        "it's a dir"
    );
    assert!(
        ssh.run(PRECHECK, &["relative"], Stdio::null(), &[])
            .is_err()
    );

    let remote = dir.join("remote");
    let source = dir.join("source");
    fs::write(&source, b"runtime bytes").unwrap();
    let digest = sha256(&source).unwrap();
    let upload = |name: &str, digest: &str| {
        ssh.run(
            UPLOAD,
            &[remote.to_str().unwrap(), name, digest, "700"],
            Stdio::from(fs::File::open(&source).unwrap()),
            &[],
        )
    };
    upload("assets/bin/tool", &digest).unwrap();
    let installed = remote.join("assets/bin/tool");
    assert_eq!(fs::read(&installed).unwrap(), b"runtime bytes");
    assert_eq!(fs::metadata(&installed).unwrap().mode() & 0o777, 0o700);
    assert_eq!(
        fs::metadata(remote.join("assets")).unwrap().mode() & 0o077,
        0
    );
    // An identical installed file is kept; a different one is replaced.
    upload("assets/bin/tool", &digest).unwrap();
    fs::write(&source, b"new runtime").unwrap();
    upload("assets/bin/tool", &sha256(&source).unwrap()).unwrap();
    assert_eq!(fs::read(&installed).unwrap(), b"new runtime");
    // A corrupted transfer never replaces the installed file.
    assert!(upload("assets/bin/tool", &digest).is_err());
    assert_eq!(fs::read(&installed).unwrap(), b"new runtime");
    assert!(upload("assets/bin/fresh", &digest).is_err());
    assert!(!remote.join("assets/bin/fresh").exists());
    assert!(!remote.join("assets/bin/tool.part").exists());

    ssh.run(
        WRITE,
        &[remote.to_str().unwrap(), "join.json", "600"],
        Stdio::null(),
        b"{\"secret\":1}",
    )
    .unwrap();
    nodes::private(&remote.join("join.json")).unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn start_script_quotes_paths() {
    let script = start_script(
        "/home/o'k/.ow-node",
        "https://ctl.example:8790",
        512,
        1,
        2,
        30,
    );
    assert!(script.contains(r#"d='/home/o'\''k/.ow-node'"#));
    assert!(script.contains("--controller 'https://ctl.example:8790'"));
    assert!(script.contains(
        "OW_MAX_MEMORY_MIB=512 OW_MAX_RUNNING=1 OW_MAX_VCPUS=2 OW_IDLE_HIBERNATE_SECS=1800"
    ));
}

#[test]
fn node_add_requires_running_controller() {
    let base = private_dir("base");
    let add = || NodeAction::Add {
        target: "ssh://runner".into(),
        name: None,
        controller: None,
        remote_dir: None,
        memory: 512,
        slots: 1,
        cpus: 1,
        ubuntu: false,
        from_store: false,
        tailscale_auth_key: None,
        idle_minutes: 30,
        ow_binary: None,
    };
    let error = node_with(OsStr::new("false"), Some(base.clone()), add()).unwrap_err();
    assert!(error.to_string().contains("ow host up"), "{error}");
    nodes::write_private(
        &base.join("host.json"),
        &json!({"controller":"https://127.0.0.1:8790"}),
    )
    .unwrap();
    let error = node_with(OsStr::new("false"), Some(base.clone()), add()).unwrap_err();
    assert!(error.to_string().contains("not running"), "{error}");
    fs::remove_dir_all(base).unwrap();
}

/// Single-host controller plus one SSH runner on this machine, through the real binary.
#[test]
#[ignore = "requires OW_NODE_TEST_BINARY, KVM, runtime assets (OW_ASSET_DIR) and a short Btrfs OW_SELFHOST_TEST_DIR"]
fn host_up_then_node_add() {
    let binary = PathBuf::from(std::env::var_os("OW_NODE_TEST_BINARY").unwrap());
    let root = PathBuf::from(std::env::var_os("OW_SELFHOST_TEST_DIR").unwrap());
    let base = root.join(format!("b{}", &common::nonce().unwrap()[..6]));
    let remote = root.join(format!("r{}", &common::nonce().unwrap()[..6]));
    struct Cleanup(std::process::Child, PathBuf, Vec<PathBuf>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
            for root in &self.2 {
                let _ = Command::new(&self.1)
                    .args(["--local", "--data-dir"])
                    .arg(root)
                    .arg("down")
                    .output();
            }
        }
    }
    fs::create_dir_all(base.join("controller")).unwrap();
    fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(base.join("controller"), fs::Permissions::from_mode(0o700)).unwrap();
    crate::limits::free_pool(&base.join("controller"));
    let port = 20000 + (std::process::id() % 20000) as u16;
    let _cleanup = Cleanup(
        Command::new(&binary)
            .arg("--data-dir")
            .arg(&base)
            .args(["host", "up", "--listen", &format!("127.0.0.1:{port}")])
            .args([
                "--node", "single", "--memory", "512", "--slots", "1", "--cpus", "1",
            ])
            .spawn()
            .unwrap(),
        binary.clone(),
        vec![base.join("runner"), remote.join("data")],
    );
    let controller = base.join("controller");
    let started = Instant::now();
    while !base.join("host.json").exists() || nodes::online(&controller, "single").is_err() {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "host up runner offline"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    let tools = private_dir("tools");
    let add = |memory: u32| {
        let action = NodeAction::Add {
            target: "ssh://ow-test-runner".into(),
            name: Some("sshnode".into()),
            controller: None,
            remote_dir: Some(remote.to_str().unwrap().into()),
            memory,
            slots: 1,
            cpus: 1,
            ubuntu: false,
            from_store: false,
            tailscale_auth_key: None,
            idle_minutes: 30,
            ow_binary: Some(binary.clone()),
        };
        node_with(fake_ssh(&tools).as_os_str(), Some(base.clone()), action)
    };
    assert_eq!(add(512).unwrap(), 0);
    let inventory = nodes::inventory(&controller).unwrap();
    for id in ["single", "sshnode"] {
        assert!(
            inventory
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["id"] == id && n["online"] == true),
            "{inventory}"
        );
    }
    assert!(
        !remote.join("join.json").exists(),
        "join file must be consumed"
    );
    nodes::private(&remote.join("data/node.json")).unwrap();
    let lock = remote.join("data/node-agent.lock");
    let agent = fs::read_to_string(&lock).unwrap();

    // Re-running against a live runner changes nothing and starts no second agent.
    add(512).unwrap();
    assert_eq!(fs::read_to_string(&lock).unwrap(), agent);
    let log = || fs::read_to_string(remote.join("agent.log")).unwrap_or_default();
    assert!(!log().contains("already running"), "{}", log());

    // A capacity change for an enrolled runner is refused, not reported as applied.
    let error = add(256).unwrap_err().to_string();
    assert!(error.contains("does not change"), "{error}");
    assert_eq!(fs::read_to_string(&lock).unwrap(), agent);
    let entry = nodes::inventory(&controller).unwrap()[1].clone();
    assert_eq!(entry["id"], "sshnode");
    assert_eq!(entry["owner_limits"]["memory"], 512);

    // A stopped agent is started again and must send a fresh heartbeat.
    let pid = serde_json::from_str::<Value>(&agent).unwrap()["pid"]
        .as_i64()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGTERM) }, 0);
    let started = Instant::now();
    while onboarding::held_lock(&lock).unwrap() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "agent did not exit"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    add(512).unwrap();
    let restarted = fs::read_to_string(&lock).unwrap();
    assert_ne!(restarted, agent);
    assert!(onboarding::held_lock(&lock).unwrap());
    assert!(nodes::online(&controller, "sshnode").is_ok());
    fs::remove_dir_all(tools).unwrap();
}

/// Real TLS listener and outbound agents, synthetic workers; run in a non-KVM Linux fixture.
#[test]
#[ignore = "requires x86-64 non-KVM Linux and OW_NODE_TEST_BINARY; boots no VM"]
fn controller_only_boot_enroll_create_and_restart() {
    controller_fixture(false);
}

/// Real TLS/credential reconnect to a different catalog root. Workers are synthetic.
#[test]
#[ignore = "requires x86-64 non-KVM Linux and OW_NODE_TEST_BINARY; boots no VM"]
fn replacement_controller_restores_catalog_and_existing_credentials() {
    controller_fixture(true);
}

fn controller_fixture(replace: bool) {
    use crate::catalog::{Catalog, Identity};
    use std::{
        collections::BTreeMap,
        os::unix::net::UnixListener,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };
    assert!(!Path::new("/dev/kvm").exists(), "use a non-KVM fixture");
    let binary =
        PathBuf::from(std::env::var_os("OW_NODE_TEST_BINARY").expect("built CLI required"));
    let base = private_dir("ctrl");
    let controller = base.join("controller");
    host::data_root(Some(controller.clone())).unwrap();
    nodes::write_private(
        &controller.join("limits.json"),
        &json!({"pool":{"default_type":"self-hosted","hourly_cents":{"self-hosted":0}}}),
    )
    .unwrap();
    let mut catalog = Catalog::open(&controller, "test", None).unwrap();
    let owner = catalog
        .user(&Identity {
            issuer: "test".into(),
            subject: "owner".into(),
            email: "owner@example.test".into(),
        })
        .unwrap();
    assert_eq!(
        catalog.state(owner).unwrap()["stats"]["limit_memory_mib"],
        0
    );
    assert!(
        catalog
            .operation(
                owner,
                json!({"op":"create","id":"no-node","memory_mib":256})
            )
            .unwrap_err()
            .to_string()
            .contains("no online node")
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let start = || {
        Command::new(&binary)
            .arg("--data-dir")
            .arg(&base)
            .args([
                "host",
                "up",
                "--controller-only",
                "--listen",
                &addr.to_string(),
            ])
            .stdout(Stdio::null())
            .spawn()
            .unwrap()
    };
    struct Fixture {
        children: Vec<std::process::Child>,
        stop: Arc<AtomicBool>,
        workers: Vec<std::thread::JoinHandle<()>>,
        base: PathBuf,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for child in &mut self.children {
                let _ = child.kill();
                let _ = child.wait();
            }
            self.stop.store(true, Ordering::SeqCst);
            for worker in self.workers.drain(..) {
                let _ = worker.join();
            }
            let _ = fs::remove_dir_all(&self.base);
        }
    }
    let mut fixture = Fixture {
        children: vec![start()],
        stop: Arc::new(AtomicBool::new(false)),
        workers: Vec::new(),
        base: base.clone(),
    };
    let ready = || {
        let since = Instant::now();
        while TcpStream::connect(addr).is_err() {
            assert!(since.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(25));
        }
    };
    ready();
    assert!(!base.join("runner").exists());
    assert!(!controller.join("control.sock").exists());
    assert_eq!(nodes::inventory(&controller).unwrap(), json!([]));
    let ca = fs::read(base.join("tls/ca.pem")).unwrap();
    let assets = base.join("assets/guest");
    fs::create_dir_all(&assets).unwrap();
    fs::write(assets.join("base.ext4"), b"synthetic image marker").unwrap();
    for node in ["alpha", "beta"] {
        let worker = base.join(node);
        host::data_root(Some(worker.clone())).unwrap();
        let listener = UnixListener::bind(worker.join("control.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = fixture.stop.clone();
        fixture.workers.push(std::thread::spawn(move || {
            let mut machines=BTreeMap::<String,Value>::new();
            while !stop.load(Ordering::SeqCst) {
                let (mut stream,_)=match listener.accept() {Ok(s)=>s,Err(e) if e.kind()==std::io::ErrorKind::WouldBlock=>{std::thread::sleep(Duration::from_millis(2));continue;},Err(e)=>panic!("{e}")};
                let request=crate::wire::line(&mut stream).unwrap();
                let result=match request["op"].as_str().unwrap() {
                    "status"=>json!({"internet":true,"max_memory_mib":512,"max_running":1,"max_vcpus":1,"max_guest_memory_mib":512,"max_guest_vcpus":1}),
                    "list"=>json!(machines.values().collect::<Vec<_>>()),
                    "snapshots"=>json!([]),
                    "stats"=>json!({"running":{}}),
                    "create"=>{let m=json!({"id":request["id"],"memory_mib":256,"vcpu_count":1,"image":"alpine","state":"running","disk_bytes":8*common::GIB});machines.insert(request["id"].as_str().unwrap().into(),m.clone());m},
                    other=>panic!("unexpected worker call {other}"),
                };
                crate::wire::send(&mut stream,&json!({"ok":true,"result":result})).unwrap();
            }
        }));
        let join = worker.join("join.json");
        nodes::invitation(
            &controller,
            node,
            600,
            512,
            1,
            1,
            Some(&format!("https://{addr}")),
            |v| nodes::write_private(&join, v),
        )
        .unwrap();
        fixture.children.push(
            Command::new(&binary)
                .arg("--data-dir")
                .arg(&worker)
                .args([
                    "node-agent",
                    "--controller",
                    &format!("https://{addr}"),
                    "--credential",
                ])
                .arg(worker.join("node.json"))
                .arg("--join")
                .arg(join)
                .arg("--ca-cert")
                .arg(base.join("tls/ca.pem"))
                .env("OW_ASSET_DIR", base.join("assets"))
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        assert!(wait_online(&controller, node, 0, Duration::from_secs(15)));
    }
    let mut selected = Vec::new();
    for name in ["one", "two"] {
        let result = catalog
            .operation(
                owner,
                json!({"op":"create","id":name,"memory_mib":256,"vcpu_count":1,"image":"alpine"}),
            )
            .unwrap();
        selected.push(result["node"].as_str().unwrap().to_owned());
    }
    selected.sort();
    assert_eq!(selected, vec!["alpha", "beta"]);
    assert_eq!(
        catalog.state(owner).unwrap()["stats"]["limit_memory_mib"],
        1024
    );
    let agents = fixture.children[1..]
        .iter()
        .map(|p| p.id())
        .collect::<Vec<_>>();
    // Graceful controller stop returns without waiting on a nonexistent local worker.
    unsafe {
        libc::kill(fixture.children[0].id() as i32, libc::SIGTERM);
    }
    let since = Instant::now();
    while fixture.children[0].try_wait().unwrap().is_none() {
        assert!(since.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(25));
    }
    if replace {
        let credentials =
            ["alpha", "beta"].map(|n| fs::read(base.join(n).join("node.json")).unwrap());
        // Positive stop above; no two listener processes overlap. The stopped
        // source catalog is kept for negative rollback checks, never reopened.
        let final_copy = base.join("final.sqlite3");
        crate::backup::run(
            None,
            crate::BackupAction::Catalog {
                output: final_copy.clone(),
                from: Some(controller.clone()),
            },
        )
        .unwrap();
        drop(catalog);
        let replacement = base.join("r");
        fs::create_dir(&replacement).unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o700)).unwrap();
        let target = replacement.join("controller");
        host::data_root(Some(target.clone())).unwrap();
        fs::copy(&final_copy, target.join("catalog.sqlite3")).unwrap();
        fs::create_dir_all(replacement.join("tls")).unwrap();
        fs::set_permissions(replacement.join("tls"), fs::Permissions::from_mode(0o700)).unwrap();
        for entry in fs::read_dir(base.join("tls")).unwrap() {
            let entry = entry.unwrap();
            fs::copy(
                entry.path(),
                replacement.join("tls").join(entry.file_name()),
            )
            .unwrap();
        }
        crate::backup::run(
            None,
            crate::BackupAction::CatalogCheck {
                catalog: target.join("catalog.sqlite3"),
                against: Some(final_copy.clone()),
            },
        )
        .unwrap();
        fixture.children[0] = Command::new(&binary)
            .arg("--data-dir")
            .arg(&replacement)
            .args([
                "host",
                "up",
                "--controller-only",
                "--listen",
                &addr.to_string(),
            ])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        ready();
        for (i, node) in ["alpha", "beta"].iter().enumerate() {
            assert_eq!(fixture.children[i + 1].id(), agents[i]);
            assert!(fixture.children[i + 1].try_wait().unwrap().is_none());
            assert!(wait_online(
                &target,
                node,
                unix_now(),
                Duration::from_secs(20)
            ));
            assert_eq!(
                fs::read(base.join(node).join("node.json")).unwrap(),
                credentials[i]
            );
            // Worker inventory survives controller loss; this is not VM evidence.
            assert_eq!(
                crate::wire::request(&base.join(node), json!({"op":"list"}))
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }
        // Old job IDs cannot bridge on the new authority, even with valid credentials.
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .add_root_certificate(reqwest::Certificate::from_pem(&ca).unwrap())
            .build()
            .unwrap();
        let credential: Value = serde_json::from_slice(&credentials[0]).unwrap();
        let reply = client
            .get(format!("https://{addr}/job/alpha/stale-operation"))
            .bearer_auth(credential["credential"].as_str().unwrap())
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send()
            .unwrap();
        assert_eq!(reply.status(), reqwest::StatusCode::NOT_FOUND);
        // First replacement write: the pre-cutover catalog must now be rejected.
        let mut recovered = Catalog::open(&target, "test", None).unwrap();
        recovered
            .user(&Identity {
                issuer: "test".into(),
                subject: "new".into(),
                email: "new@example.test".into(),
            })
            .unwrap();
        assert!(
            crate::backup::run(
                None,
                crate::BackupAction::CatalogCheck {
                    catalog: target.join("catalog.sqlite3"),
                    against: Some(final_copy),
                }
            )
            .is_err()
        );
        unsafe {
            libc::kill(fixture.children[0].id() as i32, libc::SIGTERM);
        }
        assert!(fixture.children[0].wait().unwrap().success());
        let latest = base.join("latest.sqlite3");
        crate::backup::run(
            None,
            crate::BackupAction::Catalog {
                output: latest.clone(),
                from: Some(target.clone()),
            },
        )
        .unwrap();
        crate::backup::run(
            None,
            crate::BackupAction::CatalogCheck {
                catalog: latest,
                against: Some(target.join("catalog.sqlite3")),
            },
        )
        .unwrap();
        assert_eq!(
            recovered.state(owner).unwrap()["workspaces"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        return;
    }
    fixture.children[0] = start();
    ready();
    assert_eq!(fs::read(base.join("tls/ca.pem")).unwrap(), ca);
    for (i, node) in ["alpha", "beta"].iter().enumerate() {
        assert_eq!(fixture.children[i + 1].id(), agents[i]);
        assert!(fixture.children[i + 1].try_wait().unwrap().is_none());
        assert!(wait_online(
            &controller,
            node,
            unix_now(),
            Duration::from_secs(20)
        ));
    }
    assert_eq!(
        catalog.state(owner).unwrap()["workspaces"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(!base.join("runner").exists());
}
