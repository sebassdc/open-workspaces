use super::*;
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

fn session(dir: &Path) -> Ssh {
    Ssh {
        program: fake_ssh(dir),
        control: dir.join("control"),
        args: vec!["--".into(), "runner".into()],
    }
}

#[test]
fn ssh_targets() {
    assert_eq!(
        target("ssh://dev@build-1.example.ts.net:2222").unwrap(),
        (
            ["-p", "2222", "-l", "dev", "--", "build-1.example.ts.net"]
                .map(String::from)
                .to_vec(),
            "build-1".to_owned()
        )
    );
    assert_eq!(
        target("dev@10.0.0.7").unwrap(),
        (
            ["-l", "dev", "--", "10.0.0.7"].map(String::from).to_vec(),
            "10-0-0-7".to_owned()
        )
    );
    assert_eq!(target("ssh://pc").unwrap().1, "pc");
    assert_eq!(target("ssh://[::1]").unwrap().0, ["--", "::1"]);
    for bad in [
        "https://host",
        "ssh://-oProxyCommand=x",
        "ssh://-x@host",
        "ssh://user:secret@host",
        "ssh://host/path",
        "ssh://host?x=1",
        "ssh://host;reboot",
        "ssh://",
    ] {
        assert!(target(bad).is_err(), "{bad} accepted");
    }
}

#[test]
fn default_names_are_valid_ids() {
    assert_eq!(node_name("local"), "runner");
    assert_eq!(node_name("..."), "runner");
    assert_eq!(node_name("My_Box.lan"), "My_Box");
    assert_eq!(node_name(&"a".repeat(40)).len(), 32);
    common::identifier(&node_name("fe80::1")).unwrap();
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
    let script = start_script("/home/o'k/.ow-node", "https://ctl.example:8790", 512, 1, 2);
    assert!(script.contains(r#"d='/home/o'\''k/.ow-node'"#));
    assert!(script.contains("--controller 'https://ctl.example:8790'"));
    assert!(script.contains("OW_MAX_MEMORY_MIB=512 OW_MAX_RUNNING=1 OW_MAX_VCPUS=2"));
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
        tailscale_auth_key: None,
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
            tailscale_auth_key: None,
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
