//! Real controller + agent subprocess loss over the production node/pull/journal paths.
//! The durable worker fixture emulates execution; it is not a VMM or RAM-portability test.
use super::*;
use std::{
    collections::BTreeMap,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Instant,
};

fn gate(root: &Path, label: &str) -> Result<()> {
    let hold = root.join(format!("hold-{label}"));
    if hold.exists() {
        ow_core::runtime::atomic_json(
            &root.join(format!("event-{label}")),
            &json!({"pid":std::process::id()}),
        )?;
        while hold.exists() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(())
}
pub(crate) fn controller_checkpoint(root: &Path, id: i64) -> Result<()> {
    if std::env::var("OW_TRANSFER_CHILD").as_deref() != Ok("controller") {
        return Ok(());
    }
    let db = nodes::db(root)?;
    let step: String =
        db.query_row("SELECT step FROM transfers WHERE id=?1", [id], |r| r.get(0))?;
    gate(root, &format!("controller-{step}"))
}
pub(crate) fn agent_checkpoint(root: &Path, op: &str, boundary: &str) -> Result<()> {
    if std::env::var("OW_TRANSFER_CHILD").as_deref() != Ok("agent") {
        return Ok(());
    }
    gate(root, &format!("agent-{op}-{boundary}"))
}

#[test]
fn subprocess_entry() {
    let Ok(role) = std::env::var("OW_TRANSFER_CHILD") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("OW_TRANSFER_ROOT").unwrap());
    let origin = std::env::var("OW_TRANSFER_ORIGIN").unwrap();
    match role.as_str() {
        "controller" => {
            let shared = Arc::new(Mutex::new(Catalog::open(&root, "test", None).unwrap()));
            recover(&shared);
            nodes::controller(
                &root,
                origin.strip_prefix("http://").unwrap().parse().unwrap(),
                None,
                None,
                true,
            )
            .unwrap();
        }
        "agent" => nodes::agent(
            &root,
            &origin,
            &root.join("credential.json"),
            None,
            None,
            true,
        )
        .unwrap(),
        "worker" => worker(&root).unwrap(),
        _ => panic!("unknown test role"),
    }
}

// Persist each emulated worker effect before replying. No state lives in the parent
// test or agent; every reconnect reads the same fsynced worker registry/disk.
fn worker(root: &Path) -> Result<()> {
    let socket = root.join("control.sock");
    if socket.exists() {
        fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    for socket in listener.incoming() {
        let mut socket = socket?;
        let request = wire::line(&mut socket)?;
        let result = worker_request(root, &request);
        let _ = wire::send(&mut socket, &wire::response(result));
    }
    Ok(())
}
fn worker_request(root: &Path, request: &Value) -> Result<Value> {
    let op = request["op"].as_str().context("op")?;
    let path = root.join("fixture-state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path)?)?;
    let status = || json!({"worker_pid":std::process::id(),"worker_protocol":2,"placement_fencing_v1":true,"max_memory_mib":2048,"max_running":4,"max_vcpus":4,"max_guest_memory_mib":2048,"max_guest_vcpus":4,"available_disk_bytes":1073741824,"disk_observed_unix_s":now(),"reserved_disk_bytes":0,"max_disk_gib":128,"min_free_gib":0,"workspace_count":1,"max_workspaces":8,"guest_ssh_v1":true,"disk_resize_v1":true,"compat":{"runtime":common::RUNTIME,"cpu":"fixture","kernel_sha256":store::sha256(b"kernel")}});
    match op {
        "status" => return Ok(status()),
        "inspect" => {
            return state
                .get("workspace")
                .filter(|v| !v.is_null())
                .cloned()
                .context("missing workspace");
        }
        "list" => {
            return Ok(if state["workspace"].is_null() {
                json!([])
            } else {
                json!([state["workspace"]])
            });
        }
        "snapshots" => return Ok(json!([])),
        "stats" => return Ok(json!({"running":{}})),
        "placement-check" => {
            ensure!(
                state["workspace"]["placement"] == request["request"]["placement"],
                "stale replay"
            );
            return Ok(json!({}));
        }
        _ => {}
    }
    let key = request["operation_key"]
        .as_str()
        .context("effect key required")?;
    if let Some(previous) = state["keys"].get(key) {
        ensure!(previous == request, "worker key fingerprint changed");
        return Ok(state["workspace"].clone());
    }
    let placement = common::Placement::from_request(request)?;
    match op {
        "placement-register" => {
            ensure!(
                state["workspace"]["state"] == "stopped",
                "source must be stopped"
            );
            state["workspace"]["placement"] = json!(placement);
            state["workspace"]["lifecycle"] = json!({"phase":"active","intent":null});
        }
        "export" => {
            ensure!(
                state["workspace"]["placement"] == json!(placement),
                "wrong export placement"
            );
            state["workspace"]["state"] = json!("exported");
            state["workspace"]["lifecycle"] = json!({"phase":"exported","intent":{"transfer":request["transfer"],"capture":request["capture"]}});
        }
        "import-prepare" => {
            ensure!(
                state["workspace"].is_null(),
                "existing disk cannot be replaced"
            );
            let snapshot = request["snapshot"].as_str().context("snapshot")?;
            let capture = root.join("snapshots").join(snapshot);
            ensure!(
                store::hash_file(&capture.join("manifest.json"))?
                    == request["manifest_sha256"].as_str().unwrap(),
                "native manifest mismatch"
            );
            let machine = root.join("machines/physical");
            fs::create_dir_all(&machine)?;
            fs::copy(capture.join("disk.ext4"), machine.join("disk.ext4"))?;
            fs::File::open(machine.join("disk.ext4"))?.sync_all()?;
            state["workspace"] = json!({"id":"physical","state":"prepared","memory_mib":256,"vcpu_count":1,"disk_bytes":4,"vmm_running":false,"placement":placement,"lifecycle":{"phase":"prepared","intent":null,"import":{"snapshot":snapshot,"manifest_sha256":request["manifest_sha256"],"cold":request["cold"]}}});
        }
        "activate" => {
            ensure!(
                request["committed"] == true
                    && state["workspace"]["placement"] == json!(placement)
                    && state["workspace"]["lifecycle"]["phase"] == "prepared",
                "activation before adoption"
            );
            state["workspace"]["state"] = json!("running");
            state["workspace"]["vmm_running"] = json!(true);
            state["workspace"]["lifecycle"]["phase"] = json!("active");
        }
        _ => anyhow::bail!("unsupported fixture effect {op}"),
    }
    state["keys"][key] = request.clone();
    state["counts"][op] = json!(state["counts"][op].as_u64().unwrap_or(0) + 1);
    ow_core::runtime::atomic_json(&path, &state)?;
    Ok(state["workspace"].clone())
}

struct Processes {
    root: PathBuf,
    origin: String,
    controller: Option<Child>,
    agents: BTreeMap<String, Child>,
    workers: Vec<Child>,
}
impl Processes {
    fn child(&self, role: &str, root: &Path) -> Child {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join(format!("{role}.log")))
            .unwrap();
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "transfers::process_tests::subprocess_entry",
                "--nocapture",
            ])
            .env("OW_TRANSFER_CHILD", role)
            .env("OW_TRANSFER_ROOT", root)
            .env("OW_TRANSFER_ORIGIN", &self.origin)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root.join("bin").display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap()
    }
    fn start_controller(&mut self) {
        assert!(self.controller.is_none());
        self.controller = Some(self.child("controller", &self.root.join("controller")));
    }
    fn kill_controller(&mut self) {
        if let Some(mut child) = self.controller.take() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }
    fn start_agent(&mut self, node: &str) {
        let child = self.child("agent", &self.root.join(node));
        assert!(self.agents.insert(node.into(), child).is_none());
    }
    fn kill_agent(&mut self, node: &str) {
        let mut child = self.agents.remove(node).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }
    fn wait_event(&mut self, root: &Path, label: &str, pid: u32) {
        let start = Instant::now();
        loop {
            let event = fs::read(root.join(format!("event-{label}")))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            if event.is_some_and(|v| v["pid"] == pid) {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(40),
                "timeout at {label}; logs: {}",
                self.root.display()
            );
            if let Some(child) = &mut self.controller {
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "controller exited: {}",
                    self.root.display()
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    fn wait_done(&mut self) {
        let ctl = self.root.join("controller");
        let start = Instant::now();
        loop {
            let db = nodes::db(&ctl).unwrap();
            let step: String = db
                .query_row("SELECT step FROM transfers WHERE id=1", [], |r| r.get(0))
                .unwrap();
            if step == "succeeded" {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(45),
                "transfer stuck at {step}: {}",
                self.root.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        self.kill_controller();
        let c = Catalog::open(&ctl, "test", None).unwrap();
        assert_eq!(c.transfer(1).unwrap().step, Step::Succeeded);
        assert_eq!(
            c.managed_placement("physical").unwrap().unwrap().node,
            "target"
        );
        assert_eq!(
            c.db.query_row("SELECT COUNT(*) FROM transfer_reservations", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            c.db.query_row("SELECT state FROM operations WHERE id=1", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "succeeded"
        );
        assert_eq!(
            fs::read(self.root.join("target/machines/physical/disk.ext4")).unwrap(),
            b"disk"
        );
        let target: Value =
            serde_json::from_slice(&fs::read(self.root.join("target/fixture-state.json")).unwrap())
                .unwrap();
        assert_eq!(target["counts"]["import-prepare"], 1);
        assert_eq!(target["counts"]["activate"], 1);
        let source: Value =
            serde_json::from_slice(&fs::read(self.root.join("source/fixture-state.json")).unwrap())
                .unwrap();
        assert_eq!(source["workspace"]["lifecycle"]["phase"], "exported");
        assert_eq!(source["counts"]["export"], 1);
        for node in ["source", "target"] {
            let journal =
                rusqlite::Connection::open(self.root.join(node).join("node-operations.sqlite3"))
                    .unwrap();
            let requests: Vec<String> = journal
                .prepare("SELECT request FROM effects")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            for request in requests {
                let request: Value = serde_json::from_str(&request).unwrap();
                assert_eq!(
                    request["operation_key"],
                    format!("transfer-1-{}", request["op"].as_str().unwrap())
                );
            }
        }
    }
}
impl Drop for Processes {
    fn drop(&mut self) {
        if let Some(child) = &mut self.controller {
            let _ = child.kill();
            let _ = child.wait();
        }
        for child in self.agents.values_mut().chain(self.workers.iter_mut()) {
            let _ = child.kill();
            let _ = child.wait();
        }
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
fn fixture() -> Processes {
    let root =
        std::env::temp_dir().join(format!("ow-transfer-process-{}", store::token().unwrap()));
    for name in ["controller", "source", "target", "bin"] {
        fs::create_dir_all(root.join(name)).unwrap();
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    // Local stand-in for AWS CLI reads only immutable fixture objects; no cloud credentials.
    let aws = root.join("bin/aws");
    fs::write(&aws,"#!/bin/sh\nset -eu\ntest \"$1 $2 $3\" = 's3 cp --only-show-errors'\nkey=${4#s3://}\nexec cat \"$(dirname \"$0\")/../objects/$key\"\n").unwrap();
    fs::set_permissions(aws, fs::Permissions::from_mode(0o700)).unwrap();
    let disk = root.join("sealed-disk");
    fs::write(&disk, b"disk").unwrap();
    let native=json!({"name":"sealed","workspace":"physical","managed":true,"memory_mib":256,"vcpu_count":1,"image":"alpine","runtime":common::RUNTIME,"cpu":"fixture","kernel_sha256":store::sha256(b"kernel"),"files":{"disk.ext4":store::sha256(b"disk")}}).to_string();
    let reference = store::push(
        &store::DirectoryObjects(root.join("objects/bucket/root/store/v1")),
        &store::token().unwrap(),
        "capture",
        json!({"native_manifest":native}),
        &BTreeMap::from([(
            "disk.ext4".into(),
            store::Source {
                path: disk,
                sha256: store::sha256(b"disk"),
            },
        )]),
        1,
    )
    .unwrap();
    let ctl = root.join("controller");
    let mut c = Catalog::open(&ctl, "test", None).unwrap();
    let user = c
        .user(&Identity {
            issuer: "test".into(),
            subject: "owner".into(),
            email: "owner@example.test".into(),
        })
        .unwrap();
    c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node,ssh_policy) VALUES(?1,'machine','box','physical',?2,'source','\"disabled\"')",params![user,json!({"id":"physical","state":"stopped","memory_mib":256,"vcpu_count":1,"disk_bytes":4}).to_string()]).unwrap();
    c.db.execute("INSERT INTO artifacts(id,kind,owner,machine,local_name,state,attempt,digest,store_url) VALUES('capture','capture',?1,'physical','sealed','stored',?2,?3,'s3://bucket/root')",params![user,reference.attempt,reference.digest]).unwrap();
    for node in ["source", "target"] {
        let secret = if node == "source" {
            "a".repeat(64)
        } else {
            "b".repeat(64)
        };
        nodes::write_private(
            &root.join(node).join("credential.json"),
            &json!({"node":node,"credential":secret}),
        )
        .unwrap();
        fs::write(root.join(node).join("store-url"), "s3://bucket/root").unwrap();
        c.db.execute("INSERT INTO nodes(id,credential_hash,memory_mib,slots,heartbeat,capabilities) VALUES(?1,?2,2048,4,?3,?4)",params![node,store::sha256(secret.as_bytes()),now(),json!({"memory_mib":2048,"slots":4,"vcpus":4,"placement_fencing_v1":true,"worker_protocol":2,"managed_restore_v1":true,"available_disk_bytes":1073741824,"disk_observed_unix_s":now()}).to_string()]).unwrap();
        let workspace = if node == "source" {
            json!({"id":"physical","state":"stopped","memory_mib":256,"vcpu_count":1,"disk_bytes":4,"vmm_running":false})
        } else {
            Value::Null
        };
        ow_core::runtime::atomic_json(
            &root.join(node).join("fixture-state.json"),
            &json!({"workspace":workspace,"keys":{},"counts":{}}),
        )
        .unwrap();
    }
    c.begin_transfer(
        user,
        "process-restore",
        &Intent {
            kind: Kind::Restore,
            machine: "box".into(),
            target: Some("target".into()),
            capture: Some("capture".into()),
            child: None,
            memory_mib: 256,
            cpus: 1,
            disk_bytes: 4,
            accept_cold: false,
        },
    )
    .unwrap();
    drop(c);
    let mut processes = Processes {
        root,
        origin,
        controller: None,
        agents: BTreeMap::new(),
        workers: Vec::new(),
    };
    for node in ["source", "target"] {
        let path = processes.root.join(node);
        let child = processes.child("worker", &path);
        processes.workers.push(child);
        let start = Instant::now();
        while !path.join("control.sock").exists() {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    processes
}

#[test]
#[cfg(target_arch = "x86_64")] // Production Firecracker agents advertise the actual executable architecture.
fn sigkill_controller_at_every_durable_restore_phase_recovers_from_startup() {
    let mut p = fixture();
    let ctl = p.root.join("controller");
    let phases = [
        "preparing",
        "exported",
        "importing",
        "prepared",
        "committed",
        "start_uncertain",
        "started",
        "succeeded",
    ];
    for phase in phases {
        fs::write(ctl.join(format!("hold-controller-{phase}")), b"").unwrap();
    }
    p.start_controller();
    p.start_agent("source");
    p.start_agent("target");
    for phase in phases {
        let pid = p.controller.as_ref().unwrap().id();
        p.wait_event(&ctl, &format!("controller-{phase}"), pid);
        let db = nodes::db(&ctl).unwrap();
        let reserved: i64 = db
            .query_row("SELECT COUNT(*) FROM transfer_reservations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            reserved,
            if matches!(phase, "preparing" | "exported" | "importing" | "prepared") {
                1
            } else {
                0
            }
        );
        eprintln!("SIGKILL controller pid={pid} at durable phase={phase}");
        p.kill_controller();
        fs::remove_file(ctl.join(format!("hold-controller-{phase}"))).unwrap();
        p.start_controller();
    }
    p.wait_done();
}

#[test]
#[cfg(target_arch = "x86_64")]
fn sigkill_agent_before_and_after_real_jobs_and_journal_effects_recovers() {
    for op in [
        "placement-register",
        "export",
        "capture-pull",
        "import-prepare",
        "activate",
    ] {
        for boundary in ["before-effect", "after-effect"] {
            let mut p = fixture();
            let node = if matches!(op, "placement-register" | "export") {
                "source"
            } else {
                "target"
            };
            let root = p.root.join(node);
            let label = format!("agent-{op}-{boundary}");
            fs::write(root.join(format!("hold-{label}")), b"").unwrap();
            p.start_controller();
            p.start_agent("source");
            p.start_agent("target");
            let pid = p.agents[node].id();
            p.wait_event(&root, &label, pid);
            eprintln!(
                "SIGKILL agent pid={pid} node={node} op={op} boundary={boundary}; restart controller from disk"
            );
            p.kill_agent(node);
            p.kill_controller();
            let ctl = p.root.join("controller");
            let db = nodes::db(&ctl).unwrap();
            if op == "capture-pull" && boundary == "before-effect" {
                let old: String = db
                    .query_row("SELECT token FROM leases", [], |r| r.get(0))
                    .unwrap();
                assert!(!p.root.join("target/snapshots/sealed").exists());
                fs::remove_file(root.join(format!("hold-{label}"))).unwrap();
                p.start_agent(node);
                p.start_controller();
                // Fresh startup sees the old lease and must not dispatch another attempt.
                let start = Instant::now();
                loop {
                    let error: Option<String> = db
                        .query_row(
                            "SELECT json_extract(response,'$.error') FROM operations WHERE id=1",
                            [],
                            |r| r.get(0),
                        )
                        .unwrap();
                    if error.is_some_and(|e| e.contains("old lease")) {
                        break;
                    }
                    assert!(start.elapsed() < Duration::from_secs(30));
                    std::thread::sleep(Duration::from_millis(50));
                }
                assert_eq!(
                    db.query_row("SELECT COUNT(*) FROM leases", [], |r| r.get::<_, i64>(0))
                        .unwrap(),
                    1
                );
                p.kill_controller();
                db.execute("UPDATE leases SET expires=?1", [now() - 1])
                    .unwrap();
                p.start_controller();
                p.wait_done();
                let tokens: Vec<String> = db
                    .prepare("SELECT token FROM leases ORDER BY id")
                    .unwrap()
                    .query_map([], |r| r.get(0))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                assert_eq!(tokens.len(), 2);
                assert_eq!(tokens[0], old);
                assert_ne!(tokens[0], tokens[1]);
            } else {
                fs::remove_file(root.join(format!("hold-{label}"))).unwrap();
                p.start_agent(node);
                p.start_controller();
                p.wait_done();
            }
        }
    }
}
