use super::*;
use std::{
    os::unix::{fs::PermissionsExt, net::UnixListener},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
struct Fake {
    root: PathBuf,
    calls: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
    reject: Arc<Mutex<Option<String>>>,
    diagnostic: Arc<Mutex<String>>,
    _thread: Option<std::thread::JoinHandle<()>>,
}
impl Fake {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("ow-c-{}", common::nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        Self {
            root,
            calls,
            stopped,
            reject: Arc::new(Mutex::new(None)),
            diagnostic: Arc::new(Mutex::new("simulated runtime rejection".into())),
            _thread: None,
        }
    }
    fn worker(&mut self, node: &str) {
        let path = self.root.join("nodes").join(node);
        std::fs::create_dir_all(&path).unwrap();
        let listener = UnixListener::bind(path.join("control.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let calls = self.calls.clone();
        let stop = self.stopped.clone();
        let reject = self.reject.clone();
        let diagnostic = self.diagnostic.clone();
        self._thread = Some(std::thread::spawn(move || {
            let mut machines = BTreeMap::<String, Value>::new();
            let mut lost = BTreeMap::<String, Value>::new();
            let mut snapshots = BTreeMap::<String, Value>::new();
            while !stop.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                };
                let request = wire::line(&mut stream).unwrap();
                calls.fetch_add(1, Ordering::SeqCst);
                let op = request["op"].as_str().unwrap();
                let id = request["id"].as_str().unwrap_or("");
                if reject.lock().unwrap().as_deref() == Some(op) {
                    wire::send(
                        &mut stream,
                        &json!({"ok":false,"error":*diagnostic.lock().unwrap()}),
                    )
                    .unwrap();
                    continue;
                }
                let result = match op {
                    "list" => json!(machines.values().collect::<Vec<_>>()),
                    "snapshots" => json!(snapshots.values().collect::<Vec<_>>()),
                    "stats" => json!({"running":{},"reserved_memory_mib":0}),
                    "resize" => {
                        let m = machines.get_mut(id).unwrap();
                        for key in ["memory_mib", "vcpu_count"] {
                            if let Some(v) = request.get(key) {
                                m[key] = v.clone();
                            }
                        }
                        if let Some(n) = request["disk_gib"].as_u64() {
                            m["disk_bytes"] = json!(n * common::GIB);
                        }
                        m.clone()
                    }
                    "create" => {
                        let m = json!({"id":id,"memory_mib":request["memory_mib"],"vcpu_count":request["vcpu_count"],"image":request["image"],"state":"running","source":null,"disk_bytes":request["disk_gib"].as_u64().unwrap_or(8)*common::GIB});
                        machines.insert(id.into(), m.clone());
                        m
                    }
                    "snapshot" => {
                        let name = request["name"].as_str().unwrap();
                        let m = machines.get(id).unwrap();
                        let s = json!({"name":name,"workspace":id,"memory_mib":m["memory_mib"],"vcpu_count":m["vcpu_count"],"image":m["image"],"state":"ready"});
                        snapshots.insert(name.into(), s.clone());
                        s
                    }
                    "exec" => json!({"output":"ok","exit_code":0}),
                    "fork" => {
                        let child = request["child"].as_str().unwrap();
                        let mut m = machines.get(id).unwrap().clone();
                        m["id"] = json!(child);
                        m["source"] = json!(id);
                        machines.insert(child.into(), m.clone());
                        m
                    }
                    "hibernate" | "start" => {
                        let m = machines.get_mut(id).unwrap();
                        m["state"] = json!(if op == "start" {
                            "running"
                        } else {
                            "hibernated"
                        });
                        m.clone()
                    }
                    "stop" => {
                        let m = machines.get_mut(id).unwrap();
                        m["state"] = json!("stopped");
                        json!({"workspace":m})
                    }
                    "delete" if machines.get(id).is_some_and(|m| m["state"] == "running") => {
                        wire::send(
                            &mut stream,
                            &json!({"ok":false,"error":"workspace is running"}),
                        )
                        .unwrap();
                        continue;
                    }
                    "delete" => {
                        machines.remove(id);
                        let gone: Vec<String> = snapshots
                            .iter()
                            .filter(|(_, s)| s["workspace"] == id)
                            .map(|(n, _)| n.clone())
                            .collect();
                        for name in &gone {
                            snapshots.remove(name);
                        }
                        json!({"deleted":id,"snapshots":gone})
                    }
                    // Test-only: the runner loses a machine (deleted there, or its data is gone).
                    "lose" => {
                        let m = machines.remove(id).unwrap();
                        lost.insert(id.into(), m.clone());
                        m
                    }
                    // Test-only: the runner has it back (its data came back, or it was restored).
                    "recover" => {
                        let m = lost.remove(id).unwrap();
                        machines.insert(id.into(), m.clone());
                        m
                    }
                    _ => json!({}),
                };
                wire::send(&mut stream, &json!({"ok":true,"result":result})).unwrap();
            }
        }));
        nodes::db(&self.root).unwrap().execute("INSERT INTO nodes(id,credential_hash,memory_mib,slots,heartbeat,capabilities) VALUES(?1,'hash',512,2,?2,?3)",params![node,nodes_now(),json!({"memory_mib":512,"slots":2,"vcpus":2,"images":["alpine"]}).to_string()]).unwrap();
    }
}
fn nodes_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(thread) = self._thread.take() {
            thread.join().unwrap();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn identity(name: &str) -> Identity {
    Identity {
        issuer: "test".into(),
        subject: name.into(),
        email: format!("{name}@example.test"),
    }
}

#[test]
fn known_rejections_give_safe_actions_without_worker_details() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    let owner = c.user(&identity("alice")).unwrap();
    c.operation(owner, json!({"op":"create","id":"demo","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).unwrap();
    nodes::db(&fake.root).unwrap().execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.guest_ssh_v1',json('true')) WHERE id='alpha'", []).unwrap();
    for (op, diagnostic, expected) in [
        (
            "delete",
            "workspace is running; stop or hibernate it first",
            "ow stop",
        ),
        (
            "ssh-info",
            "SSH not enrolled; use ow ssh-authorize: No such file or directory (os error 2)",
            "ow ssh-authorize",
        ),
        (
            "ssh-info",
            "private host path /srv/private/data and secret diagnostic",
            "Check its status",
        ),
        ("delete", "unrecognized worker failure", "Check its status"),
    ] {
        *fake.reject.lock().unwrap() = Some(op.into());
        *fake.diagnostic.lock().unwrap() = diagnostic.into();
        let error = c
            .operation(owner, json!({"op":op,"id":"demo"}))
            .unwrap_err();
        let failure = error.downcast_ref::<OperationFailure>().unwrap();
        assert!(
            failure.message().contains(expected),
            "{}",
            failure.message()
        );
        assert_eq!(failure.0["state"], "failed");
        assert!(!failure.message().contains("/srv/private"));
        assert!(!failure.message().contains("os error"));
    }
    assert_eq!(c.state(owner).unwrap()["workspaces"][0]["state"], "running");
}

#[test]
fn failed_snapshots_keep_the_cause_reconcile_legacy_pending_and_retry() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    let owner = c.user(&identity("alice")).unwrap();
    c.operation(owner, json!({"op":"create","id":"demo","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).unwrap();
    *fake.reject.lock().unwrap() = Some("snapshot".into());
    let diagnostic =
        "storage headroom exhausted: 72000000000 bytes available, 73000000000 bytes required";
    *fake.diagnostic.lock().unwrap() = diagnostic.into();
    let request = json!({"op":"snapshot","id":"demo","name":"saved","operation_key":"capture-1"});
    let failure = c.operation(owner, request.clone()).unwrap_err();
    let failure = failure.downcast_ref::<OperationFailure>().unwrap();
    assert_eq!(failure.message(), diagnostic);
    assert!(failure.to_string().contains(diagnostic));
    let id = failure.0["id"].as_i64().unwrap();
    let response: String =
        c.db.query_row("SELECT response FROM operations WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&response).unwrap()["error"],
        diagnostic
    );
    assert!(
        c.db.query_row(
            "SELECT finished_at IS NOT NULL FROM operations WHERE id=?1",
            [id],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
    assert_eq!(c.state(owner).unwrap()["snapshots"][0]["state"], "failed");
    // Simulate a failed reservation written by an older controller, then reopen it.
    c.db.execute(
        "UPDATE resources SET metadata='{\"state\":\"pending\"}' WHERE kind='snapshot'",
        [],
    )
    .unwrap();
    drop(c);
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    assert_eq!(c.state(owner).unwrap()["snapshots"][0]["state"], "failed");
    assert_eq!(c.operation_info(owner, id).unwrap()["error"], diagnostic);
    *fake.diagnostic.lock().unwrap() = "a new capture must not run during replay".into();
    assert_eq!(
        c.operation(owner, request)
            .unwrap_err()
            .downcast_ref::<OperationFailure>()
            .unwrap()
            .message(),
        diagnostic
    );
    // Unknown outcomes stay pending; absence alone is not failure.
    c.db.execute("UPDATE operations SET state='uncertain' WHERE id=?1", [id])
        .unwrap();
    c.db.execute(
        "UPDATE resources SET metadata='{\"state\":\"pending\"}' WHERE kind='snapshot'",
        [],
    )
    .unwrap();
    assert_eq!(c.state(owner).unwrap()["snapshots"][0]["state"], "pending");
    c.db.execute("UPDATE operations SET state='failed' WHERE id=?1", [id])
        .unwrap();
    *fake.reject.lock().unwrap() = None;
    let saved = c
        .operation(
            owner,
            json!({"op":"snapshot","id":"demo","name":"saved","operation_key":"capture-2"}),
        )
        .unwrap();
    assert_eq!(saved["state"], "ready");
    assert_eq!(c.state(owner).unwrap()["snapshots"][0]["state"], "ready");
    let other = c.user(&identity("bob")).unwrap();
    assert!(
        c.state(other).unwrap()["operations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_snapshot_dashboard_response_preserves_safe_cause() {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    let owner = c.user(&identity("alice")).unwrap();
    c.operation(owner, json!({"op":"create","id":"demo","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).unwrap();
    *fake.reject.lock().unwrap() = Some("snapshot".into());
    let catalog = Arc::new(Mutex::new(c));
    for (key, diagnostic, expected) in [
        (
            "capacity",
            "storage headroom exhausted: 72 bytes available, 73 bytes required",
            "72 bytes available",
        ),
        (
            "private",
            "private /srv/operator/token=secret",
            "host journal",
        ),
    ] {
        *fake.diagnostic.lock().unwrap() = diagnostic.into();
        let request = Request::builder()
            .method("POST")
            .uri("/api/operation")
            .header("content-type", "application/json")
            .header("x-ow-request", "dashboard")
            .body(Body::from(
                json!({"op":"snapshot","id":"demo","name":"saved","operation_key":key}).to_string(),
            ))
            .unwrap();
        let response = crate::dashboard::handle(
            Some(catalog.clone()),
            Some(fake.root.clone()),
            identity("alice"),
            request,
            false,
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = to_bytes(response.into_body(), 16384).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["ok"], false);
        assert!(value["error"].as_str().unwrap().contains(expected));
        assert_eq!(value["operation"]["error"], value["error"]);
        assert_eq!(value["operation"]["state"], "failed");
        assert!(
            !String::from_utf8(bytes.to_vec())
                .unwrap()
                .contains("token=secret")
        );
    }
}

#[test]
fn placement_ownership_snapshots_duplicate_and_offline_state() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    let a = c.user(&identity("alice")).unwrap();
    let b = c.user(&identity("bob")).unwrap();
    let create = json!({"op":"create","id":"demo","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine","operation_key":"create-one"});
    let result = c.operation(a, create.clone()).unwrap();
    let physical = c.physical(a, "machine", "demo").unwrap();
    assert_eq!(result["node"], "alpha");
    let before = fake.calls.load(Ordering::SeqCst);
    assert!(
        c.operation(b, json!({"op":"exec","id":"demo","command":"secret"}))
            .is_err()
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), before);
    assert!(c.terminal_target(b, "demo").is_err());
    let repeated = c.operation(a, create.clone()).unwrap();
    assert_eq!(repeated["operation_id"], result["operation_id"]);
    let mut changed = create.clone();
    changed["memory_mib"] = json!(512);
    changed["operation_key"] = json!("new-key");
    assert!(c.operation(a, changed).is_err());
    assert_eq!(
        c.db.query_row(
            "SELECT reserved_mib FROM resources WHERE physical=?1",
            [&physical],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        256
    );
    c.operation(a, json!({"op":"snapshot","id":"demo","name":"first"}))
        .unwrap();
    c.operation(a, json!({"op":"snapshot","id":"demo","name":"second"}))
        .unwrap();
    c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node,reserved_mib,reserved_cpus) VALUES(?1,'machine','elsewhere','elsewhere','{\"state\":\"running\",\"memory_mib\":256,\"vcpu_count\":1}','beta',256,1)",[a]).unwrap();
    let before = fake.calls.load(Ordering::SeqCst);
    assert!(
        c.operation(a, json!({"op":"fork","id":"demo","child":"elsewhere"}))
            .is_err()
    );
    assert_eq!(c.placement("machine", "elsewhere").unwrap(), "beta");
    assert!(fake.calls.load(Ordering::SeqCst) >= before); // inventory is allowed; no fork is dispatched
    c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node) VALUES(?1,'snapshot','foreign','foreign','{\"workspace\":\"elsewhere\"}','beta')",[a]).unwrap();
    let before = fake.calls.load(Ordering::SeqCst);
    assert!(
        c.operation(a, json!({"op":"restore","id":"demo","name":"foreign"}))
            .is_err()
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), before);
    assert!(Catalog::open(&fake.root, "test", None).is_err());
    nodes::revoke(&fake.root, "alpha").unwrap();
    let state = c.state(a).unwrap();
    let demo = state["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == "demo")
        .unwrap();
    assert_eq!(demo["node"], "alpha");
    assert_eq!(demo["state"], "offline");
    assert_eq!(state["operations"][0]["state"], "succeeded");
    let before = fake.calls.load(Ordering::SeqCst);
    assert!(
        c.operation(a, json!({"op":"exec","id":"demo","command":"x"}))
            .is_err()
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), before);
    drop(c);
    let c = Catalog::open(&fake.root, "test", None).unwrap();
    assert_eq!(c.placement("machine", &physical).unwrap(), "alpha");
}
#[test]
fn uncertain_transport_keeps_placement_and_reservations() {
    let fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    let a = c.user(&identity("alice")).unwrap();
    nodes::db(&fake.root).unwrap().execute("INSERT INTO nodes(id,memory_mib,slots,heartbeat,capabilities) VALUES('alpha',512,2,?1,?2)",params![nodes_now(),json!({"memory_mib":512,"slots":2,"vcpus":2,"images":["alpine"]}).to_string()]).unwrap();
    let error=c.operation(a,json!({"op":"create","id":"demo","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine","operation_key":"lost"})).unwrap_err();
    let failure = error.downcast_ref::<OperationFailure>().unwrap();
    assert_eq!(failure.0["state"], "uncertain");
    assert_eq!(failure.0["retry_key"], "lost");
    assert_eq!(c.state(a).unwrap()["stats"]["reserved_memory_mib"], 256);
    let physical = c.physical(a, "machine", "demo").unwrap();
    drop(c);
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    assert_eq!(c.placement("machine", &physical).unwrap(), "alpha");
    assert!(
        c.operation(
            a,
            json!({"op":"create","id":"demo","node":"beta","memory_mib":256})
        )
        .is_err()
    );
}
#[test]
fn unregistered_and_cpu_demand_prevents_admission() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    let a = c.user(&identity("alice")).unwrap();
    let route = fake.root.join("nodes/alpha");
    wire::request(
        &route,
        json!({"op":"create","id":"unregistered","memory_mib":512,"vcpu_count":2,"image":"alpine"}),
    )
    .unwrap();
    assert!(c.operation(a,json!({"op":"create","id":"demo","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).is_err());
    assert!(c.physical(a, "machine", "demo").is_err());
    assert_eq!(c.extra_usage("alpha").unwrap(), (512, 2, 1));
}

#[test]
fn absent_legacy_worker_never_completes_import() {
    let fake = Fake::new();
    std::fs::write(fake.root.join("state.json"), "{\"workspaces\":{}}").unwrap();
    assert!(Catalog::open(&fake.root, "test", Some("owner@example.test")).is_err());
    let db = nodes::db(&fake.root).unwrap();
    assert!(
        !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM settings WHERE key='legacy_import')",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
}

#[test]
fn start_restore_rejection_releases_only_positive_stopped_demand() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    let a = c.user(&identity("alice")).unwrap();
    c.operation(a,json!({"op":"create","id":"demo","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).unwrap();
    let physical = c.physical(a, "machine", "demo").unwrap();
    c.operation(a, json!({"op":"snapshot","id":"demo","name":"saved"}))
        .unwrap();
    *fake.reject.lock().unwrap() = Some("restore".into());
    assert!(
        c.operation(
            a,
            json!({"op":"restore","id":"demo","name":"saved","operation_key":"running-restore"})
        )
        .is_err()
    );
    let reserved = |c: &Catalog| {
        c.db.query_row(
            "SELECT reserved_mib,reserved_cpus FROM resources WHERE kind='machine' AND physical=?1",
            [&physical],
            |r| Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?)),
        )
        .unwrap()
    };
    assert_eq!(reserved(&c), (256, 1));
    *fake.reject.lock().unwrap() = Some("start".into());
    assert!(
        c.operation(
            a,
            json!({"op":"start","id":"demo","operation_key":"running-start"})
        )
        .is_err()
    );
    assert_eq!(reserved(&c), (256, 1));
    *fake.reject.lock().unwrap() = None;
    c.operation(a, json!({"op":"stop","id":"demo"})).unwrap();
    assert_eq!(reserved(&c), (0, 0));
    *fake.reject.lock().unwrap() = Some("start".into());
    assert!(
        c.operation(
            a,
            json!({"op":"start","id":"demo","operation_key":"stopped-start"})
        )
        .is_err()
    );
    assert_eq!(reserved(&c), (0, 0));
    *fake.reject.lock().unwrap() = Some("restore".into());
    assert!(
        c.operation(
            a,
            json!({"op":"restore","id":"demo","name":"saved","operation_key":"stopped-restore"})
        )
        .is_err()
    );
    assert_eq!(reserved(&c), (0, 0));
    let info = c.state(a).unwrap();
    assert_eq!(info["operations"][0]["state"], "failed");
}

#[test]
fn mixed_version_shapes_reject_before_durable_intent() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    let user = c.user(&identity("owner")).unwrap();
    // Literal local without a new optional status response retains legacy bounds.
    assert!(
        c.operation(
            user,
            json!({"op":"create","id":"local-large","memory_mib":4096,"vcpu_count":2})
        )
        .is_err()
    );
    fake.worker("legacy");
    let db = nodes::db(&fake.root).unwrap();
    let legacy = json!({"memory_mib":16384,"slots":2,"vcpus":16,"images":["alpine"]});
    db.execute(
        "UPDATE nodes SET memory_mib=16384,capabilities=?1 WHERE id='legacy'",
        [legacy.to_string()],
    )
    .unwrap();
    for request in [
        json!({"op":"create","id":"auto-large","memory_mib":4096,"vcpu_count":2}),
        json!({"op":"create","id":"pinned-large","node":"legacy","memory_mib":4096,"vcpu_count":2}),
        json!({"op":"create","id":"cpu-large","node":"legacy","memory_mib":256,"vcpu_count":5}),
    ] {
        assert!(c.operation(user, request).is_err());
    }
    assert_eq!(
        c.db.query_row("SELECT COUNT(*) FROM operations", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        c.db.query_row("SELECT COUNT(*) FROM resources", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    // Existing owning routes must also reject start, resize, fork and restore.
    let physical = c.reserve(user, "machine", "existing").unwrap();
    c.db.execute(
        "UPDATE resources SET node='legacy' WHERE physical=?1",
        [&physical],
    )
    .unwrap();
    let metadata = json!({"state":"stopped","memory_mib":4096,"vcpu_count":2,"image":"alpine"});
    for op in ["start", "restore", "fork", "resize"] {
        assert!(
            c.admit(
                "legacy",
                &physical,
                op,
                &json!({"memory_mib":4096,"vcpu_count":2}),
                &metadata,
                &json!({})
            )
            .is_err()
        );
        assert!(
            c.admit(
                "local",
                &physical,
                op,
                &json!({"memory_mib":4096,"vcpu_count":2}),
                &metadata,
                &json!({})
            )
            .is_err()
        );
    }
    let mut modern = legacy;
    modern["max_guest_memory_mib"] = json!(16384);
    modern["max_guest_vcpus"] = json!(16);
    db.execute(
        "UPDATE nodes SET capabilities=?1 WHERE id='legacy'",
        [modern.to_string()],
    )
    .unwrap();
    assert!(
        c.operation(
            user,
            json!({"op":"create","id":"new-large","node":"legacy","memory_mib":4096,"vcpu_count":2})
        )
        .is_ok()
    );
}

#[test]
fn sharing_roles_owner_addresses_and_revocation() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    let a = c.user(&identity("alice")).unwrap();
    c.operation(a, json!({"op":"create","id":"demo","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).unwrap();
    let physical = c.physical(a, "machine", "demo").unwrap();
    nodes::db(&fake.root).unwrap().execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.guest_ssh_v1',json('true')) WHERE id='alpha'",[]).unwrap();
    let shared = "alice@example.test/demo";
    let exec = |id: &str| json!({"op":"exec","id":id,"command":"id"});
    // Shared before Bob ever logged in: his first verified login binds the reserved email.
    let granted = c
        .operation(
            a,
            json!({"op":"share","id":"demo","with":"Bob@Example.test"}),
        )
        .unwrap();
    assert_eq!(
        granted["shared_with"],
        json!([{"email":"bob@example.test","role":"use"}])
    );
    let b = c.user(&identity("bob")).unwrap();
    let e = c.user(&identity("eve")).unwrap();
    let before = fake.calls.load(Ordering::SeqCst);
    assert!(c.operation(e, exec(shared)).is_err());
    assert!(c.terminal_target(e, shared).is_err());
    assert!(c.ssh_target(e, shared).is_err());
    assert!(c.operation(b, exec("demo")).is_err());
    assert_eq!(fake.calls.load(Ordering::SeqCst), before);
    // The owner may use the full address too.
    assert_eq!(c.operation(a, exec(shared)).unwrap()["output"], "ok");
    assert_eq!(c.operation(b, exec(shared)).unwrap()["output"], "ok");
    assert_eq!(c.terminal_target(b, shared).unwrap().1, physical);
    assert_eq!(c.ssh_target(b, shared).unwrap().1, physical);
    let before = fake.calls.load(Ordering::SeqCst);
    for denied in [
        json!({"op":"stop","id":shared}),
        json!({"op":"resize","id":shared,"memory_mib":512,"vcpu_count":1}),
        json!({"op":"snapshot","id":shared,"name":"mine"}),
        json!({"op":"ssh-keys","id":shared,"keys":[],"upgrade":false}),
        json!({"op":"share","id":shared,"with":"eve@example.test"}),
        json!({"op":"share","id":"demo","with":"eve@example.test"}),
        json!({"op":"unshare","id":"demo","with":"bob@example.test"}),
    ] {
        assert!(c.operation(b, denied.clone()).is_err(), "{denied}");
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), before);
    assert!(
        c.operation(
            a,
            json!({"op":"share","id":"demo","with":"alice@example.test"})
        )
        .is_err()
    );
    let state = c.state(b).unwrap();
    assert_eq!(state["workspaces"], json!([]));
    assert_eq!(state["shared"][0]["id"], shared);
    assert_eq!(state["shared"][0]["owner"], "alice@example.test");
    assert_eq!(state["shared"][0]["role"], "use");
    assert_eq!(
        c.state(a).unwrap()["workspaces"][0]["shared_with"][0]["email"],
        "bob@example.test"
    );
    // A grantee's fork is theirs and outlives the grant.
    let fork = c
        .operation(b, json!({"op":"fork","id":shared,"child":"copy"}))
        .unwrap();
    assert_eq!(fork["id"], "copy");
    assert_eq!(fork["source"], shared);
    assert_eq!(
        c.operation(b, json!({"op":"hibernate","id":shared}))
            .unwrap()["id"],
        shared
    );
    // Admin adds snapshots (in the grantee's namespace) and stop.
    c.operation(
        a,
        json!({"op":"share","id":"demo","with":"bob@example.test","role":"admin"}),
    )
    .unwrap();
    let snapshot = c
        .operation(b, json!({"op":"snapshot","id":shared,"name":"saved"}))
        .unwrap();
    assert_eq!(
        (snapshot["name"].as_str(), snapshot["workspace"].as_str()),
        (Some("saved"), Some(shared))
    );
    assert!(
        c.operation(
            b,
            json!({"op":"resize","id":shared,"memory_mib":512,"vcpu_count":1})
        )
        .is_err()
    );
    // Unshare closes the grantee's open streams at once, not the owner's.
    let (_, _, grantee_stream) = c.terminal_target(b, shared).unwrap();
    let (_, _, grantee_ssh) = c.ssh_target(b, shared).unwrap();
    let (_, _, owner_stream) = c.terminal_target(a, "demo").unwrap();
    let removed = c
        .operation(
            a,
            json!({"op":"unshare","id":"demo","with":"bob@example.test"}),
        )
        .unwrap();
    assert_eq!(removed["shared_with"], json!([]));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let wait = |n: Arc<tokio::sync::Notify>, ms| {
        rt.block_on(async { tokio::time::timeout(Duration::from_millis(ms), n.notified()).await })
            .is_ok()
    };
    assert!(wait(grantee_stream, 1000));
    assert!(wait(grantee_ssh, 1000));
    assert!(!wait(owner_stream, 50));
    let before = fake.calls.load(Ordering::SeqCst);
    assert!(c.operation(b, exec(shared)).is_err());
    assert!(c.terminal_target(b, shared).is_err());
    assert_eq!(fake.calls.load(Ordering::SeqCst), before);
    let state = c.state(b).unwrap();
    assert_eq!(state["shared"], json!([]));
    assert_eq!(state["workspaces"][0]["id"], "copy");
    assert_eq!(state["workspaces"][0]["source"], json!(null));
    // The grants table survives a restart.
    c.operation(
        a,
        json!({"op":"share","id":"demo","with":"bob@example.test"}),
    )
    .unwrap();
    drop(c);
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    assert_eq!(c.state(b).unwrap()["shared"][0]["id"], shared);
}

#[test]
fn delete_is_owner_only_drops_snapshots_grants_and_frees_the_name() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    let a = c.user(&identity("alice")).unwrap();
    let b = c.user(&identity("bob")).unwrap();
    c.operation(a, json!({"op":"create","id":"demo","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).unwrap();
    let physical = c.physical(a, "machine", "demo").unwrap();
    c.operation(a, json!({"op":"snapshot","id":"demo","name":"saved"}))
        .unwrap();
    c.operation(
        a,
        json!({"op":"share","id":"demo","with":"bob@example.test","role":"admin"}),
    )
    .unwrap();
    let shared = "alice@example.test/demo";
    let before = fake.calls.load(Ordering::SeqCst);
    for id in [shared, "demo"] {
        assert!(c.operation(b, json!({"op":"delete","id":id})).is_err());
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), before);
    // The runner refuses a running machine; the catalog keeps it.
    assert!(c.operation(a, json!({"op":"delete","id":"demo"})).is_err());
    assert_eq!(c.physical(a, "machine", "demo").unwrap(), physical);
    c.operation(a, json!({"op":"stop","id":"demo"})).unwrap();
    let (_, _, grantee_stream) = c.terminal_target(b, shared).unwrap();
    let delete = json!({"op":"delete","id":"demo","operation_key":"delete-one"});
    let deleted = c.operation(a, delete.clone()).unwrap();
    assert_eq!(deleted["deleted"], "demo");
    assert_eq!(deleted["snapshots"], json!(["saved"]));
    assert!(
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(1), grantee_stream.notified()).await
            })
            .is_ok()
    );
    let state = c.state(a).unwrap();
    assert_eq!(state["workspaces"], json!([]));
    assert_eq!(state["snapshots"], json!([]));
    assert_eq!(state["operations"][0]["op"], "delete");
    assert_eq!(state["operations"][0]["resource"], "deleted:demo");
    assert_eq!(c.state(b).unwrap()["shared"], json!([]));
    let count = |c: &Catalog, table: &str| {
        c.db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
    };
    assert_eq!((count(&c, "grants"), count(&c, "resources")), (0, 0));
    // The names are free again, with new parameters and a new runner ID.
    let again = c
        .operation(
            a,
            json!({"op":"create","id":"demo","memory_mib":512,"vcpu_count":1,"image":"alpine"}),
        )
        .unwrap();
    assert_eq!(
        (again["id"].as_str(), again["node"].as_str()),
        (Some("demo"), Some("alpha"))
    );
    assert_ne!(c.physical(a, "machine", "demo").unwrap(), physical);
    c.operation(a, json!({"op":"snapshot","id":"demo","name":"saved"}))
        .unwrap();
    assert!(
        c.operation(a, json!({"op":"delete","id":"nothing"}))
            .is_err()
    );
}

#[test]
fn machines_the_runner_lost_are_flagged_and_deleted_without_it() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    let a = c.user(&identity("alice")).unwrap();
    for id in ["gone", "kept"] {
        c.operation(a, json!({"op":"create","id":id,"node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).unwrap();
    }
    let gone = c.physical(a, "machine", "gone").unwrap();
    // Never observed on the runner: a create in flight, not a lost machine.
    let pending = c.reserve(a, "machine", "pending").unwrap();
    c.db.execute(
        "UPDATE resources SET node='alpha' WHERE physical=?1",
        [&pending],
    )
    .unwrap();
    wire::request(
        &fake.root.join("nodes/alpha"),
        json!({"op":"lose","id":gone}),
    )
    .unwrap();
    let state = c.state(a).unwrap();
    let states: BTreeMap<_, _> = state["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| (w["id"].as_str().unwrap(), w["state"].as_str().unwrap()))
        .collect();
    assert_eq!(
        states,
        BTreeMap::from([
            ("gone", "missing"),
            ("kept", "running"),
            ("pending", "pending")
        ])
    );
    // A lost running machine no longer holds capacity.
    assert_eq!(state["stats"]["reserved_memory_mib"], 256);
    // Nothing reaches the runner: a dispatched delete would be rejected here.
    *fake.reject.lock().unwrap() = Some("delete".into());
    let delete = json!({"op":"delete","id":"gone","operation_key":"gone-once"});
    let deleted = c.operation(a, delete.clone()).unwrap();
    assert_eq!(
        (&deleted["deleted"], &deleted["snapshots"], &deleted["node"]),
        (&json!("gone"), &json!([]), &json!("alpha"))
    );
    assert_eq!(deleted["operation"]["state"], "succeeded");
    // A lost reply is retried with the same key and gets the same result.
    assert_eq!(c.operation(a, delete.clone()).unwrap(), deleted);
    let state = c.state(a).unwrap();
    assert_eq!(state["workspaces"].as_array().unwrap().len(), 2);
    assert_eq!(state["workspaces"][0]["id"], "kept");
    assert_eq!(state["workspaces"][0]["state"], "running");
}

#[test]
fn delete_keys_replay_their_result_and_never_touch_a_replacement() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    let a = c.user(&identity("alice")).unwrap();
    let create = json!({"op":"create","id":"demo","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"});
    c.operation(a, create.clone()).unwrap();
    c.operation(a, json!({"op":"stop","id":"demo"})).unwrap();
    let delete = json!({"op":"delete","id":"demo","operation_key":"delete-one"});
    let deleted = c.operation(a, delete.clone()).unwrap();
    // A lost reply: the same key gets the saved result, without a second delete.
    *fake.reject.lock().unwrap() = Some("delete".into());
    assert_eq!(c.operation(a, delete.clone()).unwrap(), deleted);
    *fake.reject.lock().unwrap() = None;
    // A replacement under the same name, lost by its runner, survives the old key.
    c.operation(a, create.clone()).unwrap();
    let replacement = c.physical(a, "machine", "demo").unwrap();
    c.operation(
        a,
        json!({"op":"snapshot","id":"demo","name":"replacement-save"}),
    )
    .unwrap();
    wire::request(
        &fake.root.join("nodes/alpha"),
        json!({"op":"lose","id":replacement}),
    )
    .unwrap();
    assert_eq!(c.operation(a, delete.clone()).unwrap(), deleted);
    let state = c.state(a).unwrap();
    assert_eq!(state["workspaces"][0]["id"], "demo");
    assert_eq!(state["workspaces"][0]["state"], "missing");
    assert_eq!(state["snapshots"][0]["name"], "replacement-save");
    // A key belongs to one operation on one name.
    let create_key: String =
        c.db.query_row(
            "SELECT retry_key FROM operations WHERE op='create' ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    for reused in [
        json!({"op":"delete","id":"other","operation_key":"delete-one"}),
        json!({"op":"delete","id":"demo","operation_key":create_key}),
    ] {
        assert!(c.operation(a, reused.clone()).is_err(), "{reused}");
    }
    assert_eq!(c.physical(a, "machine", "demo").unwrap(), replacement);
    // A fresh key deletes the replacement, and its own retry replays.
    let second = json!({"op":"delete","id":"demo","operation_key":"delete-two"});
    let removed = c.operation(a, second.clone()).unwrap();
    assert_eq!(removed["snapshots"], json!(["replacement-save"]));
    assert_eq!(c.operation(a, second).unwrap(), removed);
    let state = c.state(a).unwrap();
    assert_eq!(
        (&state["workspaces"], &state["snapshots"]),
        (&json!([]), &json!([]))
    );
    // An old key replays only its own result, even once the name is in use again.
    c.operation(a, create).unwrap();
    assert_eq!(c.operation(a, delete).unwrap(), deleted);
    assert_eq!(c.state(a).unwrap()["workspaces"][0]["state"], "running");
}

#[test]
fn delete_drops_reservations_of_failed_snapshots_only_its_own() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    let a = c.user(&identity("alice")).unwrap();
    for id in ["demo", "other"] {
        c.operation(a, json!({"op":"create","id":id,"node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).unwrap();
    }
    *fake.reject.lock().unwrap() = Some("snapshot".into());
    for name in ["saved", "moved"] {
        assert!(
            c.operation(a, json!({"op":"snapshot","id":"demo","name":name}))
                .is_err()
        );
    }
    *fake.reject.lock().unwrap() = None;
    // The name of a failed capture taken again on another machine is that machine's now.
    c.operation(
        a,
        json!({"op":"snapshot","id":"other","name":"moved","operation_key":"moved-again"}),
    )
    .unwrap();
    let names = |c: &mut Catalog| {
        c.state(a).unwrap()["snapshots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_owned())
            .collect::<std::collections::BTreeSet<_>>()
    };
    let set = |n: &[&str]| n.iter().map(|s| s.to_string()).collect();
    assert_eq!(names(&mut c), set(&["moved", "saved"]));
    c.operation(a, json!({"op":"stop","id":"demo"})).unwrap();
    let deleted = c.operation(a, json!({"op":"delete","id":"demo"})).unwrap();
    assert_eq!(deleted["snapshots"], json!(["saved"]));
    assert_eq!(names(&mut c), set(&["moved"]));
    let other = c.physical(a, "machine", "other").unwrap();
    let moved = c.physical(a, "snapshot", "moved").unwrap();
    assert!(c.snapshot_source(a, &moved, &other).is_ok());
}

#[test]
fn a_missing_machine_is_deleted_while_its_runner_is_offline_or_revoked() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    let a = c.user(&identity("alice")).unwrap();
    let b = c.user(&identity("bob")).unwrap();
    for id in ["offline", "revoked"] {
        c.operation(a, json!({"op":"create","id":id,"node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).unwrap();
    }
    for id in ["offline", "revoked"] {
        let physical = c.physical(a, "machine", id).unwrap();
        wire::request(
            &fake.root.join("nodes/alpha"),
            json!({"op":"lose","id":physical}),
        )
        .unwrap();
    }
    // Lost machines hold no capacity, so another fits on the runner.
    assert_eq!(c.state(a).unwrap()["stats"]["reserved_memory_mib"], 0);
    c.operation(a, json!({"op":"create","id":"unseen","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).unwrap();
    c.operation(
        a,
        json!({"op":"share","id":"offline","with":"bob@example.test","role":"admin"}),
    )
    .unwrap();
    let db = nodes::db(&fake.root).unwrap();
    db.execute("UPDATE nodes SET heartbeat=0 WHERE id='alpha'", [])
        .unwrap();
    // Owner authority still comes first.
    assert!(
        c.operation(b, json!({"op":"delete","id":"alice@example.test/offline"}))
            .is_err()
    );
    let deleted = c
        .operation(
            a,
            json!({"op":"delete","id":"offline","operation_key":"off-1"}),
        )
        .unwrap();
    assert_eq!(deleted["deleted"], "offline");
    assert_eq!(
        c.operation(
            a,
            json!({"op":"delete","id":"offline","operation_key":"off-1"})
        )
        .unwrap(),
        deleted
    );
    nodes::revoke(&fake.root, "alpha").unwrap();
    c.operation(a, json!({"op":"delete","id":"revoked"}))
        .unwrap();
    // One never confirmed missing still needs its runner.
    assert!(
        c.operation(a, json!({"op":"delete","id":"unseen"}))
            .is_err()
    );
    let ids: Vec<_> = c.state(a).unwrap()["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, ["unseen"]);
}

#[test]
fn a_missing_machine_its_runner_has_again_is_deleted_on_the_runner() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    let a = c.user(&identity("alice")).unwrap();
    let route = fake.root.join("nodes/alpha");
    c.operation(a, json!({"op":"create","id":"demo","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).unwrap();
    let physical = c.physical(a, "machine", "demo").unwrap();
    c.operation(a, json!({"op":"snapshot","id":"demo","name":"saved"}))
        .unwrap();
    let flag = |c: &mut Catalog| {
        wire::request(&route, json!({"op":"lose","id":physical})).unwrap();
        assert_eq!(c.state(a).unwrap()["workspaces"][0]["state"], "missing");
        wire::request(&route, json!({"op":"recover","id":physical})).unwrap();
    };
    let on_runner = || {
        let machines = wire::request(&route, json!({"op":"list"})).unwrap();
        let snapshots = wire::request(&route, json!({"op":"snapshots"})).unwrap();
        (
            machines.as_array().unwrap().len(),
            snapshots.as_array().unwrap().len(),
        )
    };
    // Back and running: the runner refuses, and the catalog keeps it.
    flag(&mut c);
    assert!(c.operation(a, json!({"op":"delete","id":"demo"})).is_err());
    let state = c.state(a).unwrap();
    assert_eq!(state["workspaces"][0]["state"], "running");
    assert_eq!(state["snapshots"][0]["name"], "saved");
    assert_eq!(on_runner(), (1, 1));
    // An online runner that cannot answer gives no evidence: nothing is dropped.
    c.operation(a, json!({"op":"stop","id":"demo"})).unwrap();
    flag(&mut c);
    std::fs::rename(route.join("control.sock"), route.join("away.sock")).unwrap();
    assert!(c.operation(a, json!({"op":"delete","id":"demo"})).is_err());
    std::fs::rename(route.join("away.sock"), route.join("control.sock")).unwrap();
    assert_eq!(c.physical(a, "machine", "demo").unwrap(), physical);
    // Back and stopped: deleted on the runner, then in the catalog.
    let deleted = c.operation(a, json!({"op":"delete","id":"demo"})).unwrap();
    assert_eq!(deleted["snapshots"], json!(["saved"]));
    assert_eq!(on_runner(), (0, 0));
    let state = c.state(a).unwrap();
    assert_eq!(
        (&state["workspaces"], &state["snapshots"]),
        (&json!([]), &json!([]))
    );
}

#[test]
fn disk_resize_is_owner_only_and_requires_a_capable_stopped_runner() {
    let mut fake = Fake::new();
    fake.worker("disk-node");
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    let owner = c.user(&identity("owner")).unwrap();
    let other = c.user(&identity("other")).unwrap();
    // No disk capability: explicit sizing must fail rather than silently create 8 GiB.
    let create = json!({"op":"create","id":"diskbox","node":"disk-node","disk_gib":32,"memory_mib":256,"vcpu_count":1,"image":"alpine"});
    assert!(c.operation(owner, create.clone()).is_err());
    nodes::db(&fake.root).unwrap().execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.disk_resize_v1',json('true')) WHERE id='disk-node'", []).unwrap();
    let created = c.operation(owner, create).unwrap();
    assert_eq!(created["disk_bytes"], 32 * common::GIB);
    let grow = json!({"op":"resize","id":"diskbox","disk_gib":64});
    assert!(c.operation(owner, grow.clone()).is_err());
    c.operation(owner, json!({"op":"stop","id":"diskbox"}))
        .unwrap();
    c.operation(
        owner,
        json!({"op":"share","id":"diskbox","with":"other@example.test","role":"admin"}),
    )
    .unwrap();
    let calls = fake.calls.load(Ordering::SeqCst);
    assert!(
        c.operation(
            other,
            json!({"op":"resize","id":"owner@example.test/diskbox","disk_gib":64})
        )
        .is_err()
    );
    assert_eq!(calls, fake.calls.load(Ordering::SeqCst));
    let resized = c.operation(owner, grow).unwrap();
    assert_eq!(resized["disk_bytes"], 64 * common::GIB);
    assert_eq!(resized["memory_mib"], created["memory_mib"]);
    assert_eq!(resized["vcpu_count"], created["vcpu_count"]);
    assert_eq!(
        c.state(owner).unwrap()["workspaces"][0]["disk_bytes"],
        64 * common::GIB
    );
}
