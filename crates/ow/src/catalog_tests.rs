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
    uncertain: Arc<AtomicBool>,
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
            uncertain: Arc::new(AtomicBool::new(false)),
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
        let uncertain = self.uncertain.clone();
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
                        &json!({"ok":false,"error":*diagnostic.lock().unwrap(),"uncertain":uncertain.load(Ordering::SeqCst)}),
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
                    "image-pull" => {
                        json!({"name":format!("{}@{}",request["image"]["name"].as_str().unwrap(),request["image"]["version"].as_i64().unwrap()),"image_digest":request["image"]["reference"]["digest"]})
                    }
                    "create" => {
                        let m = json!({"id":id,"memory_mib":request["memory_mib"].as_u64().unwrap_or(256),"vcpu_count":request["vcpu_count"].as_u64().unwrap_or(1),"image":request["image"].as_str().unwrap_or("alpine"),"image_digest":request["image_digest"],"state":"running","source":null,"disk_bytes":request["disk_gib"].as_u64().unwrap_or(8)*common::GIB});
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
                    "snapshot-delete" => {
                        let name = request["name"].as_str().unwrap();
                        let existed = snapshots.get(name).is_some_and(|s| s["workspace"] == id);
                        if existed {
                            snapshots.remove(name);
                        }
                        json!({"deleted":name,"workspace":id,"existed":existed})
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

#[tokio::test]
async fn missing_machine_delete_is_not_found_without_worker_contact() {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };

    let fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    let owner = c.user(&identity("alice")).unwrap();
    c.reserve(owner, "machine", "private-box").unwrap();
    let c = Arc::new(Mutex::new(c));
    for name in [
        "missing-box",
        "private-box",
        "alice@example.test/private-box",
    ] {
        let response = crate::dashboard::handle(
            Some(c.clone()),
            None,
            identity("bob"),
            Request::builder()
                .method("POST")
                .uri("/api/operation")
                .header("x-ow-request", "dashboard")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"op":"delete","id":name,"operation_key":"missing-delete"}).to_string(),
                ))
                .unwrap(),
            false,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{name}");
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 16384).await.unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"ok":false,"error_code":"not_found","error":format!("machine '{name}' was not found.")})
        );
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    let c = c.lock().unwrap();
    assert_eq!(
        c.db.query_row("SELECT count(*) FROM operations", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        c.db.query_row("SELECT count(*) FROM resources", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn known_rejections_give_safe_actions_without_worker_details() {
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
        let request = json!({"op":op,"id":"demo","operation_key":common::nonce().unwrap()});
        let error = c.operation(owner, request.clone()).unwrap_err();
        let failure = error.downcast_ref::<OperationFailure>().unwrap();
        assert!(
            failure.message().contains(expected),
            "{}",
            failure.message()
        );
        assert_eq!(failure.0["state"], "failed");
        assert!(!failure.message().contains("/srv/private"));
        assert!(!failure.message().contains("os error"));
        let catalog = Arc::new(Mutex::new(c));
        let response = crate::dashboard::handle(
            Some(catalog.clone()),
            None,
            identity("alice"),
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/operation")
                .header("x-ow-request", "dashboard")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(request.to_string()))
                .unwrap(),
            false,
            None,
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 16384)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"], failure.message());
        assert!(!body.to_string().contains("/srv/private"));
        assert!(!body.to_string().contains("os error"));
        c = Arc::try_unwrap(catalog).ok().unwrap().into_inner().unwrap();
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

#[test]
fn failed_snapshot_retry_with_uncertain_outcome_is_pending() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    let owner = c.user(&identity("alice")).unwrap();
    c.operation(owner, json!({"op":"create","id":"demo","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).unwrap();
    *fake.reject.lock().unwrap() = Some("snapshot".into());
    *fake.diagnostic.lock().unwrap() =
        "storage headroom exhausted: 72 bytes available, 73 bytes required".into();
    let first = json!({"op":"snapshot","id":"demo","name":"saved","operation_key":"first"});
    c.operation(owner, first.clone()).unwrap_err();
    assert_eq!(c.state(owner).unwrap()["snapshots"][0]["state"], "failed");
    // Replaying a known rejection must not make its reservation pending again.
    c.operation(owner, first.clone()).unwrap_err();
    assert_eq!(c.state(owner).unwrap()["snapshots"][0]["state"], "failed");

    fake.uncertain.store(true, Ordering::SeqCst);
    *fake.diagnostic.lock().unwrap() = "simulated lost reply".into();
    let retry = json!({"op":"snapshot","id":"demo","name":"saved","operation_key":"retry"});
    let error = c.operation(owner, retry.clone()).unwrap_err();
    let failure = error.downcast_ref::<OperationFailure>().unwrap();
    assert_eq!(failure.0["state"], "uncertain");
    let operation = failure.0["id"].clone();
    assert_eq!(c.state(owner).unwrap()["snapshots"][0]["state"], "pending");
    drop(c);
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    assert_eq!(c.state(owner).unwrap()["snapshots"][0]["state"], "pending");
    assert_eq!(
        c.operation(owner, retry.clone())
            .unwrap_err()
            .downcast_ref::<OperationFailure>()
            .unwrap()
            .0["id"],
        operation
    );
    // A replay of the older rejection cannot override the unresolved newer capture.
    c.operation(owner, first).unwrap_err();
    assert_eq!(c.state(owner).unwrap()["snapshots"][0]["state"], "pending");

    fake.uncertain.store(false, Ordering::SeqCst);
    *fake.reject.lock().unwrap() = None;
    let saved = c.operation(owner, retry).unwrap();
    assert_eq!(saved["operation_id"], operation);
    assert_eq!(saved["state"], "ready");
    assert_eq!(c.state(owner).unwrap()["snapshots"][0]["state"], "ready");
    // Inventory of an existing seal wins even if another request loses its reply.
    fake.uncertain.store(true, Ordering::SeqCst);
    *fake.reject.lock().unwrap() = Some("snapshot".into());
    c.operation(
        owner,
        json!({"op":"snapshot","id":"demo","name":"saved","operation_key":"sealed-retry"}),
    )
    .unwrap_err();
    assert_eq!(c.state(owner).unwrap()["snapshots"][0]["state"], "ready");
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
            None,
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
fn one_snapshot_is_deleted_by_its_owner_and_its_name_is_free() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    let a = c.user(&identity("alice")).unwrap();
    let b = c.user(&identity("bob")).unwrap();
    for id in ["demo", "other"] {
        c.operation(a, json!({"op":"create","id":id,"node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).unwrap();
    }
    for (id, name) in [("demo", "saved"), ("demo", "kept"), ("other", "theirs")] {
        c.operation(a, json!({"op":"snapshot","id":id,"name":name}))
            .unwrap();
    }
    c.operation(
        a,
        json!({"op":"share","id":"demo","with":"bob@example.test","role":"admin"}),
    )
    .unwrap();
    let delete =
        json!({"op":"snapshot-delete","id":"demo","name":"saved","operation_key":"drop-saved"});
    // A runner that does not advertise the operation is never asked.
    let before = fake.calls.load(Ordering::SeqCst);
    assert!(c.operation(a, delete.clone()).is_err());
    assert_eq!(fake.calls.load(Ordering::SeqCst), before);
    nodes::db(&fake.root).unwrap().execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.snapshot_delete_v1',json('true')) WHERE id='alpha'",[]).unwrap();
    // Snapshot names are per owner: a teammate cannot reach the owner's snapshots,
    // and a snapshot is deleted only through the machine it captured.
    assert!(
        c.operation(
            b,
            json!({"op":"snapshot-delete","id":"alice@example.test/demo","name":"saved"})
        )
        .is_err()
    );
    assert!(
        c.operation(
            a,
            json!({"op":"snapshot-delete","id":"demo","name":"theirs"})
        )
        .is_err()
    );
    let deleted = c.operation(a, delete.clone()).unwrap();
    assert_eq!(
        (&deleted["deleted"], &deleted["workspace"]),
        (&json!("saved"), &json!("demo"))
    );
    // A lost reply is answered from the record, though the snapshot is gone.
    assert_eq!(c.operation(a, delete).unwrap()["deleted"], "saved");
    let names = |c: &mut Catalog| -> Vec<String> {
        let mut names: Vec<String> = c.state(a).unwrap()["snapshots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_owned())
            .collect();
        names.sort();
        names
    };
    assert_eq!(names(&mut c), ["kept", "theirs"]);
    assert!(
        c.operation(
            a,
            json!({"op":"snapshot-delete","id":"demo","name":"saved"})
        )
        .is_err()
    );
    c.operation(a, json!({"op":"snapshot","id":"demo","name":"saved"}))
        .unwrap();
    assert_eq!(names(&mut c), ["kept", "saved", "theirs"]);
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
    nodes::db(&fake.root).unwrap().execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.disk_resize_v1',json('true'),'$.available_disk_bytes',1099511627776,'$.disk_observed_unix_s',CAST(strftime('%s','now') AS INTEGER)) WHERE id='disk-node'", []).unwrap();
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

#[tokio::test]
async fn preview_publication_is_owner_only_durable_and_never_reassigns_origins() {
    let fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    let alice = c.user(&identity("alice")).unwrap();
    let bob = c.user(&identity("bob")).unwrap();
    let eve = c.user(&identity("eve")).unwrap();
    c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata) VALUES(?1,'machine','demo','physical-a','{}')",[alice]).unwrap();
    let request = json!({"op":"publish","id":"demo","port":3000});
    let first = c.publication(alice, &request, |_, _| Ok(())).unwrap();
    let https = first["https_port"].as_u64().unwrap() as u16;
    assert!(c.preview_target(alice, https).is_ok());
    assert!(c.preview_target(bob, https).is_err());
    c.operation(
        alice,
        json!({"op":"share","id":"demo","with":"bob@example.test","role":"admin"}),
    )
    .unwrap();
    let shared = c.preview_target(bob, https).unwrap();
    let mut nonowner = request.clone();
    nonowner["id"] = json!("alice@example.test/demo");
    assert!(
        c.publication(bob, &nonowner, |_, _| panic!("must not configure Serve"))
            .is_err()
    );
    assert!(c.preview_target(eve, https).is_err());
    c.operation(
        alice,
        json!({"op":"unshare","id":"demo","with":"bob@example.test"}),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_millis(100), shared.revoked.notified())
        .await
        .unwrap();
    assert!(c.preview_target(bob, https).is_err());
    c.publication(
        alice,
        &json!({"op":"unpublish","id":"demo","port":3000}),
        |_, _| Err(anyhow::anyhow!("serve offline")),
    )
    .unwrap_err();
    assert!(
        c.preview_target(alice, https).is_err(),
        "fail closed even if route removal fails"
    );
    assert_eq!(
        first,
        c.publication(alice, &request, |_, _| Ok(())).unwrap()
    );
    // A failed creation is reserved but cannot forward anything.
    assert!(
        c.publication(
            alice,
            &json!({"op":"publish","id":"demo","port":4000}),
            |_, _| Err(anyhow::anyhow!("conflict"))
        )
        .is_err()
    );
    assert_eq!(
        c.publications("physical-a")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    drop(c);
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    assert_eq!(
        first,
        c.publication(alice, &request, |_, _| Ok(())).unwrap()
    );
    c.forget(alice, "physical-a").unwrap();
    assert!(c.preview_target(alice, https).is_err());
    c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata) VALUES(?1,'machine','demo','physical-b','{}')",[alice]).unwrap();
    let next = c.publication(alice, &request, |_, _| Ok(())).unwrap();
    assert!(
        next["https_port"].as_u64().unwrap() > https as u64,
        "new box cannot inherit old browser storage"
    );
    for invalid in [
        json!(0),
        json!(65536),
        json!(-1),
        json!("3000"),
        json!(null),
    ] {
        let mut bad = request.clone();
        bad["port"] = invalid;
        assert!(
            c.publication(alice, &bad, |_, _| panic!("invalid port reached helper"))
                .is_err()
        );
    }
}

fn quota_config(root: &Path, caps: Value) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(
        root.join("limits.json"),
        json!({"owner_defaults":caps}).to_string(),
    )
    .unwrap();
    std::fs::set_permissions(
        root.join("limits.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
}
fn quota_node(fake: &Fake) {
    nodes::db(&fake.root).unwrap().execute("UPDATE nodes SET memory_mib=4096,slots=8,capabilities=json_set(capabilities,'$.memory_mib',4096,'$.slots',8,'$.vcpus',8,'$.disk_resize_v1',json('true'),'$.available_disk_bytes',70368744177664,'$.disk_observed_unix_s',CAST(strftime('%s','now') AS INTEGER))",[]).unwrap();
}
#[test]
fn owner_quota_each_boundary_override_and_stopped_accounting() {
    for (field, cap, message) in [
        ("cpus", 1, "CPU"),
        ("memory_mib", 256, "RAM MiB"),
        ("disk_gib", 8, "disk GiB"),
        ("boxes", 1, "boxes"),
    ] {
        let mut fake = Fake::new();
        let mut c = Catalog::open(&fake.root, "test", None).unwrap();
        fake.worker("alpha");
        quota_node(&fake);
        let a = c.user(&identity("alice")).unwrap();
        let b = c.user(&identity("bob")).unwrap();
        let mut caps = json!({"cpus":16,"memory_mib":32768,"disk_gib":256,"boxes":8});
        caps[field] = json!(cap);
        quota_config(&fake.root, caps.clone());
        let create = json!({"op":"create","id":"one","node":"alpha","memory_mib":256,"vcpu_count":1,"disk_gib":8});
        c.operation(a, create.clone()).unwrap();
        // Stable retries neither grow usage nor fail at the exact cap.
        c.operation(a, create).unwrap();
        c.operation(a, json!({"op":"stop","id":"one"})).unwrap();
        let state = c.state(a).unwrap();
        assert_eq!(state["discovery"]["schema"], 1);
        assert_eq!(
            state["discovery"]["identity"]["email"],
            "alice@example.test"
        );
        assert_eq!(state["owner_limits"]["caps"], caps);
        let usage = state["owner_limits"]["usage"].clone();
        assert_eq!(
            usage,
            json!({"cpus":1,"memory_mib":256,"disk_gib":8,"boxes":1})
        );
        let error = c
            .operation(
                a,
                json!({"op":"create","id":"two","memory_mib":256,"vcpu_count":1,"disk_gib":8}),
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&format!("owner {message} limit exceeded")),
            "{error:#}"
        );
        assert!(
            c.physical(a, "machine", "two").is_err(),
            "denied reservation rolled back"
        );
        assert_eq!(c.state(b).unwrap()["owner_limits"]["usage"]["boxes"], 0);
        c.operation(
            b,
            json!({"op":"create","id":"two","memory_mib":256,"vcpu_count":1,"disk_gib":8}),
        )
        .unwrap();
        // Overrides are trusted config keyed by durable owner ID, not request fields.
        std::fs::write(fake.root.join("limits.json"),json!({"owner_defaults":caps,"owners":{a.to_string():{"cpus":16,"memory_mib":32768,"disk_gib":256,"boxes":8}}}).to_string()).unwrap();
        c.operation(a, json!({"op":"fork","id":"one","child":"copy"}))
            .unwrap();
        assert_eq!(c.state(a).unwrap()["owner_limits"]["usage"]["boxes"], 2);
        c.operation(a, json!({"op":"delete","id":"one"})).unwrap();
        assert_eq!(c.state(a).unwrap()["owner_limits"]["usage"]["boxes"], 1);
    }
}
#[test]
fn owner_quota_growth_and_lowered_caps_leave_existing_boxes_usable() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    quota_node(&fake);
    let a = c.user(&identity("alice")).unwrap();
    quota_config(
        &fake.root,
        json!({"cpus":1,"memory_mib":256,"disk_gib":8,"boxes":1}),
    );
    c.operation(
        a,
        json!({"op":"create","id":"one","memory_mib":256,"vcpu_count":1,"disk_gib":8}),
    )
    .unwrap();
    c.operation(a, json!({"op":"stop","id":"one"})).unwrap();
    for (key, n) in [("vcpu_count", 2), ("memory_mib", 512), ("disk_gib", 9)] {
        let mut r = json!({"op":"resize","id":"one"});
        r[key] = json!(n);
        assert!(
            c.operation(a, r)
                .unwrap_err()
                .to_string()
                .contains("limit exceeded")
        );
    }
    quota_config(
        &fake.root,
        json!({"cpus":0,"memory_mib":0,"disk_gib":0,"boxes":0}),
    );
    c.operation(a, json!({"op":"start","id":"one"})).unwrap();
    assert_eq!(
        c.operation(a, json!({"op":"exec","id":"one","command":"true"}))
            .unwrap()["exit_code"],
        0
    );
    c.operation(a, json!({"op":"stop","id":"one"})).unwrap();
    c.operation(a, json!({"op":"resize","id":"one","disk_gib":8}))
        .unwrap();
    c.operation(a, json!({"op":"delete","id":"one"})).unwrap();
    assert_eq!(c.state(a).unwrap()["owner_limits"]["usage"]["boxes"], 0);
}
#[test]
fn owner_quota_failed_create_releases_but_uncertain_survives_restart() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    quota_node(&fake);
    let a = c.user(&identity("alice")).unwrap();
    quota_config(
        &fake.root,
        json!({"cpus":1,"memory_mib":256,"disk_gib":32,"boxes":1}),
    );
    *fake.reject.lock().unwrap() = Some("create".into());
    let r =
        json!({"op":"create","id":"one","memory_mib":256,"vcpu_count":1,"operation_key":"first"});
    assert!(c.operation(a, r.clone()).is_err());
    assert_eq!(c.state(a).unwrap()["owner_limits"]["usage"]["boxes"], 0);
    fake.uncertain.store(true, Ordering::SeqCst);
    let mut r = r;
    r["operation_key"] = json!("second");
    assert!(c.operation(a, r.clone()).is_err());
    drop(c);
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    assert_eq!(c.state(a).unwrap()["owner_limits"]["usage"]["boxes"], 1);
    assert!(
        c.operation(a, json!({"op":"create","id":"two"}))
            .unwrap_err()
            .to_string()
            .contains("limit exceeded")
    );
    *fake.reject.lock().unwrap() = None;
    c.operation(a, r).unwrap();
    assert_eq!(c.state(a).unwrap()["owner_limits"]["usage"]["boxes"], 1);
}
#[test]
fn owner_quota_shared_fork_charges_recipient_and_snapshot_shape() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    quota_node(&fake);
    let a = c.user(&identity("alice")).unwrap();
    let b = c.user(&identity("bob")).unwrap();
    c.operation(
        a,
        json!({"op":"create","id":"one","memory_mib":512,"vcpu_count":1,"disk_gib":8}),
    )
    .unwrap();
    c.operation(a, json!({"op":"snapshot","id":"one","name":"big"}))
        .unwrap();
    c.operation(a, json!({"op":"stop","id":"one"})).unwrap();
    c.operation(a, json!({"op":"resize","id":"one","memory_mib":256}))
        .unwrap();
    quota_config(
        &fake.root,
        json!({"cpus":1,"memory_mib":256,"disk_gib":8,"boxes":1}),
    );
    // Parent now fits; the captured RAM shape does not.
    assert!(
        c.operation(
            a,
            json!({"op":"fork","id":"one","snapshot":"big","child":"copy"})
        )
        .unwrap_err()
        .to_string()
        .contains("limit exceeded")
    );
    assert!(
        c.operation(a, json!({"op":"restore","id":"one","name":"big"}))
            .unwrap_err()
            .to_string()
            .contains("RAM MiB")
    );
    c.operation(
        a,
        json!({"op":"share","id":"one","with":"bob@example.test"}),
    )
    .unwrap();
    c.operation(
        b,
        json!({"op":"fork","id":"alice@example.test/one","child":"mine"}),
    )
    .unwrap();
    assert_eq!(c.state(a).unwrap()["owner_limits"]["usage"]["boxes"], 1);
    assert_eq!(c.state(b).unwrap()["owner_limits"]["usage"]["boxes"], 1);
    assert!(c.operation(b,json!({"op":"fork","id":"alice@example.test/one","child":"extra","owner":a,"admin_override":true})).unwrap_err().to_string().contains("limit exceeded"));
}

#[test]
fn owner_quota_concurrent_transactions_have_one_winner() {
    let fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    let owner = c.user(&identity("alice")).unwrap();
    drop(c);
    quota_config(
        &fake.root,
        json!({"cpus":1,"memory_mib":256,"disk_gib":8,"boxes":1}),
    );
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads:Vec<_>=["one","two"].into_iter().map(|physical|{
        let root=fake.root.clone();let barrier=barrier.clone();
        std::thread::spawn(move||{
            let mut db=nodes::db(&root).unwrap();barrier.wait();
            let tx=db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
            tx.execute("INSERT INTO resources(owner,kind,name,physical,metadata) VALUES(?1,'machine',?2,?2,'{}')",params![owner,physical]).unwrap();
            let admitted=limits::admit(&root,&tx,physical,"create",&json!({"memory_mib":256,"vcpu_count":1,"disk_gib":8}),&json!({})).is_ok();
            if admitted {tx.commit().unwrap();}
            admitted
        })
    }).collect();
    assert_eq!(
        threads
            .into_iter()
            .filter_map(|t| t.join().ok())
            .filter(|won| *won)
            .count(),
        1
    );
    let c = Catalog::open(&fake.root, "test", None).unwrap();
    assert_eq!(
        limits::state(&fake.root, &c.db, owner).unwrap()["usage"]["boxes"],
        1
    );
}
#[test]
fn owner_quota_config_is_private_strict_and_never_silently_falls_back() {
    let fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    let owner = c.user(&identity("alice")).unwrap();
    assert_eq!(
        limits::state(&fake.root, &c.db, owner).unwrap()["caps"]["boxes"],
        8
    );
    quota_config(
        &fake.root,
        json!({"cpus":1,"memory_mib":256,"disk_gib":8,"boxes":1}),
    );
    std::fs::set_permissions(
        fake.root.join("limits.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(limits::load(&fake.root).is_err());
    std::fs::set_permissions(
        fake.root.join("limits.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    for invalid in [
        json!({"owner_default":{}}),
        json!({"pool":{"hours":0}}),
        json!({"owner_defaults":{"cpus":-1}}),
        json!({"owners":{"alice":{}}}),
        json!({"owners":{"01":{"cpus":1,"memory_mib":256,"disk_gib":8,"boxes":1}}}),
    ] {
        std::fs::write(fake.root.join("limits.json"), invalid.to_string()).unwrap();
        assert!(limits::load(&fake.root).is_err(), "{invalid}");
    }
}

#[test]
fn owner_quota_uncertain_growth_and_pre_policy_intents_remain_reserved() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    quota_node(&fake);
    let owner = c.user(&identity("alice")).unwrap();
    quota_config(
        &fake.root,
        json!({"cpus":2,"memory_mib":512,"disk_gib":16,"boxes":2}),
    );
    let created = c
        .operation(
            owner,
            json!({"op":"create","id":"one","memory_mib":256,"vcpu_count":1,"disk_gib":8}),
        )
        .unwrap();
    let physical = c.physical(owner, "machine", "one").unwrap();
    c.operation(owner, json!({"op":"stop","id":"one"})).unwrap();
    *fake.reject.lock().unwrap() = Some("resize".into());
    fake.uncertain.store(true, Ordering::SeqCst);
    assert!(
        c.operation(
            owner,
            json!({"op":"resize","id":"one","memory_mib":512,"disk_gib":16})
        )
        .is_err()
    );
    let usage = c.state(owner).unwrap()["owner_limits"]["usage"].clone();
    assert_eq!(usage["memory_mib"], 512);
    assert_eq!(usage["disk_gib"], 16);
    // A different runner's invented observation cannot reduce this owner's demand.
    c.save_node("other-node", "machine", &physical, &created)
        .unwrap();
    assert_eq!(
        limits::state(&fake.root, &c.db, owner).unwrap()["usage"],
        usage
    );
    drop(c);
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    assert_eq!(c.state(owner).unwrap()["owner_limits"]["usage"], usage);
    c.db.execute("UPDATE operations SET state='failed' WHERE op='resize'", [])
        .unwrap();
    assert_eq!(
        c.state(owner).unwrap()["owner_limits"]["usage"]["memory_mib"],
        256
    );
    // Upgrade a legacy catalog with an uncertain intent but no owner allocation table.
    c.db.execute(
        "UPDATE operations SET state='uncertain' WHERE op='resize'",
        [],
    )
    .unwrap();
    c.db.execute_batch("DROP TABLE owner_allocations").unwrap();
    drop(c);
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    let legacy = c.state(owner).unwrap()["owner_limits"]["usage"].clone();
    assert_eq!(legacy["memory_mib"], common::HOST_MEMORY_MIB);
    assert_eq!(legacy["disk_gib"], common::MAX_DISK_GIB);
    assert!(
        c.operation(owner, json!({"op":"create","id":"two"}))
            .unwrap_err()
            .to_string()
            .contains("limit exceeded")
    );
    // Forgetting an offline row must not release capacity an uncertain effect can reclaim.
    c.db.execute(
        "UPDATE resources SET metadata=json_set(metadata,'$.state','missing') WHERE physical=?1",
        [&physical],
    )
    .unwrap();
    c.db.execute("UPDATE nodes SET heartbeat=0 WHERE id='alpha'", [])
        .unwrap();
    assert!(
        c.operation(owner, json!({"op":"delete","id":"one"}))
            .unwrap_err()
            .to_string()
            .contains("unresolved operations")
    );
    assert_eq!(
        limits::state(&fake.root, &c.db, owner).unwrap()["usage"]["boxes"],
        1
    );
    // An unknown delete cannot recreate a box, so it does not pin a confirmed-gone row.
    c.db.execute("UPDATE operations SET state='failed' WHERE op='resize'", [])
        .unwrap();
    c.db.execute("INSERT INTO operations(owner,op,resource,state,request) VALUES(?1,'delete','one','uncertain',?2)",params![owner,json!({"request":{"op":"delete","id":physical},"node":"alpha"}).to_string()]).unwrap();
    c.operation(
        owner,
        json!({"op":"delete","id":"one","operation_key":"confirm-gone"}),
    )
    .unwrap();
    assert_eq!(
        limits::state(&fake.root, &c.db, owner).unwrap()["usage"]["boxes"],
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_quota_dashboard_error_names_the_resource_and_cap() {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    let mut fake = Fake::new();
    let c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    quota_node(&fake);
    quota_config(
        &fake.root,
        json!({"cpus":0,"memory_mib":0,"disk_gib":0,"boxes":0}),
    );
    let request = Request::builder()
        .method("POST")
        .uri("/api/operation")
        .header("content-type", "application/json")
        .header("x-ow-request", "dashboard")
        .body(Body::from(json!({"op":"create","id":"quota"}).to_string()))
        .unwrap();
    let response = crate::dashboard::handle(
        Some(Arc::new(Mutex::new(c))),
        Some(fake.root.clone()),
        identity("alice"),
        request,
        false,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["ok"], false);
    let error = v["error"].as_str().unwrap();
    assert!(error.contains("owner CPU limit exceeded"));
    assert!(error.contains("limit 0"));
    assert!(!error.contains("/tmp/"));
}

#[test]
fn owner_quota_unknown_native_disk_never_uses_linux_default() {
    let fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    let owner = c.user(&identity("alice")).unwrap();
    let physical = c.reserve(owner, "machine", "native").unwrap();
    let error = limits::admit(
        &fake.root,
        &c.db,
        &physical,
        "create",
        &json!({"image":"ubuntu-arm64","memory_mib":256,"vcpu_count":1}),
        &json!({}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("disk GiB limit exceeded"));
    assert_eq!(
        limits::state(&fake.root, &c.db, owner).unwrap()["usage"]["boxes"],
        0
    );
}

#[test]
fn image_pull_failure_releases_undispatched_capacity_and_retry_readmits() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    let owner = c.user(&identity("alice")).unwrap();
    c.db.execute("UPDATE nodes SET slots=1,memory_mib=512,capabilities=json_set(capabilities,'$.slots',1,'$.memory_mib',512,'$.vcpus',1,'$.image_store_v1',json('true')) WHERE id='alpha'",[]).unwrap();
    let token = "a".repeat(32);
    let digest = "b".repeat(64);
    c.db.execute("INSERT INTO artifacts(id,kind,attempt,digest,local_name,state,store_url) VALUES('a-pull','image',?1,?2,'tools@1','stored','s3://bucket/root')",params![token,digest]).unwrap();
    c.db.execute("INSERT INTO leases(artifact,holder,purpose,token,expires) VALUES('a-pull','local','push',?1,?2)",params![token,nodes_now()+7200]).unwrap();
    c.db.execute("INSERT INTO images(name,version,artifact,base,base_digest,recipe_sha256,status) VALUES('tools',1,'a-pull','alpine',?1,?1,'current')",[digest]).unwrap();
    let create = json!({"op":"create","id":"downloaded","node":"alpha","memory_mib":512,"vcpu_count":1,"image":"tools@1","operation_key":"create-download"});
    for uncertain_create in [false, true] {
        // A failed replay of an already-uncertain create must retain its reservation.
        if uncertain_create {
            *fake.reject.lock().unwrap() = Some("create".into());
            fake.uncertain.store(true, Ordering::SeqCst);
            assert!(c.operation(owner, create.clone()).is_err());
            fake.uncertain.store(false, Ordering::SeqCst);
        }
        *fake.reject.lock().unwrap() = Some("image-pull".into());
        assert!(c.operation(owner, create.clone()).is_err());
        c.reconcile().unwrap();
        let physical = c.physical(owner, "machine", "downloaded").unwrap();
        let reserved: (i64, i64) =
            c.db.query_row(
                "SELECT reserved_mib,reserved_cpus FROM resources WHERE physical=?1",
                [&physical],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let state: String =
            c.db.query_row(
                "SELECT state FROM operations WHERE retry_key='create-download'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if uncertain_create {
            assert_eq!(reserved, (512, 1));
            assert_eq!(state, "uncertain");
            assert!(c.operation(owner,json!({"op":"create","id":"blocked","node":"alpha","memory_mib":512,"vcpu_count":1,"image":"alpine"})).unwrap_err().to_string().contains("node capacity exhausted"));
            break;
        }
        assert_eq!(reserved, (0, 0), "image transfer did not dispatch a create");
        assert_eq!(state, "not_dispatched");
        drop(c);
        c = Catalog::open(&fake.root, "test", None).unwrap();
        assert_eq!(
            limits::state(&fake.root, &c.db, owner).unwrap()["usage"]["boxes"],
            0
        );
        // The old pull might finish independently; retain its own GC lease.
        assert_eq!(
            c.db.query_row::<i64, _, _>(
                "SELECT count(*) FROM leases WHERE purpose='pull'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            1
        );
        let other = json!({"op":"create","id":"prepared","node":"alpha","memory_mib":512,"vcpu_count":1,"image":"alpine"});
        c.operation(owner, other).unwrap();
        *fake.reject.lock().unwrap() = None;
        assert!(
            c.operation(owner, create.clone())
                .unwrap_err()
                .to_string()
                .contains("node capacity exhausted"),
            "same-key retry must reacquire capacity"
        );
        c.operation(owner, json!({"op":"stop","id":"prepared"}))
            .unwrap();
    }
    // A successful worker replay resolves the uncertain pinned create.
    *fake.reject.lock().unwrap() = None;
    let result = c.operation(owner, create).unwrap();
    assert_eq!(result["image"], "tools@1");
}

#[test]
fn catalog_store_images_pin_retry_versions_and_keep_legacy_boxes() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    fake.worker("alpha");
    let owner = c.user(&identity("alice")).unwrap();
    c.operation(owner,json!({"op":"create","id":"legacy","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).unwrap();
    c.db.execute("UPDATE nodes SET slots=8,memory_mib=4096,capabilities=json_set(capabilities,'$.slots',8,'$.memory_mib',4096,'$.vcpus',8,'$.image_store_v1',json('true')) WHERE id='alpha'",[]).unwrap();
    for n in 1..=2 {
        let artifact = format!("a-version-{n}");
        let token = format!("{n:032x}");
        let digest = format!("{n:064x}");
        c.db.execute("INSERT INTO artifacts(id,kind,attempt,digest,local_name,state,store_url) VALUES(?1,'base',?2,?3,?4,'stored','s3://bucket/root')",params![artifact,token,digest,format!("alpine@{n}")]).unwrap();
        c.db.execute("INSERT INTO leases(artifact,holder,purpose,token,expires) VALUES(?1,'local','push',?2,?3)",params![artifact,token,nodes_now()+7200]).unwrap();
        c.db.execute(
            "UPDATE images SET status='kept' WHERE name='alpine' AND status='current'",
            [],
        )
        .unwrap();
        c.db.execute("INSERT INTO images(name,version,artifact,base,base_digest,recipe_sha256,status) VALUES('alpine',?1,?2,'alpine',?3,?3,'current')",params![n,artifact,digest]).unwrap();
        let created=c.operation(owner,json!({"op":"create","id":"pinned","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).unwrap();
        assert_eq!(
            created["image"], "alpine@1",
            "lost-response retry retains first version"
        );
    }
    let legacy=c.operation(owner,json!({"op":"create","id":"legacy","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).unwrap();
    assert_eq!(legacy["image"], "alpine");
    let fresh=c.operation(owner,json!({"op":"create","id":"fresh","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"})).unwrap();
    assert_eq!(fresh["image"], "alpine@2");
    c.db.execute(
        "UPDATE images SET status='retired' WHERE name='alpine' AND version=1",
        [],
    )
    .unwrap();
    assert!(c.operation(owner,json!({"op":"create","id":"retired","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine@1"})).is_err());
    crate::image_store::prefetch(&fake.root, "alpha", &json!([])).unwrap();
    assert_eq!(
        c.db.query_row::<i64, _, _>(
            "SELECT count(*) FROM artifact_copies WHERE node='alpha' AND state='present'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        2
    );
    assert_eq!(
        c.db.query_row::<i64, _, _>(
            "SELECT count(*) FROM leases WHERE purpose='pull'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
}

// T1 contracts use only a private catalog and synthetic artifacts; no VM or store transport.
fn transfer_fixture() -> (Fake, Catalog, i64, String, crate::transfers::Intent) {
    use crate::transfers::*;
    let fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    let user = c.user(&identity("owner")).unwrap();
    let physical = c.reserve(user, "machine", "box").unwrap();
    c.db.execute("UPDATE resources SET node='alpha',reserved_mib=256,reserved_cpus=1,metadata=?1 WHERE physical=?2",params![json!({"id":physical,"state":"running","memory_mib":256,"vcpu_count":1,"disk_bytes":8*common::GIB}).to_string(),physical]).unwrap();
    c.set_transfer_ssh_policy(user, "box", 0, &PublicSshPolicy::Disabled)
        .unwrap();
    c.db.execute("INSERT INTO artifacts(id,kind,owner,machine,local_name,state,digest,store_url) VALUES('capture','capture',?1,?2,'capture','stored',?3,'fixture://store')",params![user,physical,"a".repeat(64)]).unwrap();
    for node in ["alpha", "beta"] {
        c.db.execute("INSERT INTO nodes(id,memory_mib,slots,heartbeat,capabilities) VALUES(?1,512,2,?2,'{\"memory_mib\":512,\"slots\":2,\"vcpus\":2}')",params![node,nodes_now()]).unwrap();
    }
    let intent = Intent {
        kind: Kind::Move,
        machine: "box".into(),
        target: Some("beta".into()),
        capture: Some("capture".into()),
        child: None,
        snapshot: None,
        memory_mib: 256,
        cpus: 1,
        disk_bytes: 8 * common::GIB,
        accept_cold: false,
        explicit_start: false,
    };
    (fake, c, user, physical, intent)
}
fn prepare_transfer(c: &Catalog, t: &crate::transfers::Transfer) {
    use crate::transfers::*;
    let mut step = Step::Preparing;
    if t.kind.needs_fence() {
        c.advance_transfer(
            t.id,
            t.epoch,
            step,
            Step::Exported,
            &Evidence {
                source_fenced: true,
                ..Default::default()
            },
        )
        .unwrap();
        step = Step::Exported;
    }
    if t.kind != Kind::Park {
        c.advance_transfer(t.id, t.epoch, step, Step::Importing, &Evidence::default())
            .unwrap();
        step = Step::Importing;
    }
    c.advance_transfer(
        t.id,
        t.epoch,
        step,
        Step::Prepared,
        &Evidence {
            target_prepared: true,
            ..Default::default()
        },
    )
    .unwrap();
}
fn reservation(c: &Catalog, physical: &str) -> (String, i64, Option<String>, i64, i64) {
    c.db.query_row("SELECT node,placement_epoch,current_capture,reserved_mib,reserved_cpus FROM resources WHERE kind='machine' AND physical=?1",[physical],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap()
}
#[test]
fn transfer_schema_three_migration_preserves_private_catalog_and_reader_floor() {
    let fake = Fake::new();
    let path = fake.root.join("catalog.sqlite3");
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA foreign_keys=ON;
      CREATE TABLE users(id INTEGER PRIMARY KEY,issuer TEXT NOT NULL,subject TEXT,email TEXT NOT NULL,UNIQUE(issuer,subject),UNIQUE(issuer,email));
      CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
      CREATE TABLE resources(owner INTEGER NOT NULL REFERENCES users(id),kind TEXT NOT NULL,name TEXT NOT NULL,physical TEXT NOT NULL,metadata TEXT NOT NULL,created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,node TEXT NOT NULL DEFAULT 'local',reserved_mib INTEGER NOT NULL DEFAULT 0,reserved_cpus INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(owner,kind,name),UNIQUE(kind,physical));
      CREATE TABLE operations(id INTEGER PRIMARY KEY,owner INTEGER NOT NULL REFERENCES users(id),op TEXT NOT NULL,resource TEXT NOT NULL,state TEXT NOT NULL,created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,finished_at TEXT,node TEXT NOT NULL DEFAULT 'local',retry_key TEXT,request TEXT,response TEXT);
      CREATE UNIQUE INDEX operation_retry ON operations(owner,retry_key);
      CREATE TABLE grants(physical TEXT NOT NULL,grantee INTEGER NOT NULL REFERENCES users(id),role TEXT NOT NULL,created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,PRIMARY KEY(physical,grantee));
      INSERT INTO users VALUES(1,'test','owner','owner@example.test'),(2,'test','guest','guest@example.test');
      INSERT INTO resources(owner,kind,name,physical,metadata,node,reserved_mib,reserved_cpus) VALUES(1,'machine','box','m-fixture','{\"id\":\"m-fixture\",\"state\":\"hibernated\",\"keys\":[\"public-fixture\"]}','offline',256,1),(1,'snapshot','saved','s-fixture','{\"workspace\":\"m-fixture\"}','offline',0,0);
      INSERT INTO grants(physical,grantee,role) VALUES('m-fixture',2,'use');
      INSERT INTO settings VALUES('legacy_import','complete'),('fixture_public_keys','[\"fixture-key\"]');
      INSERT INTO operations(id,owner,op,resource,state,retry_key,request,response) VALUES(91,1,'hibernate','box','succeeded','saved-key','{\"input\":1}','{\"ok\":true}');
      PRAGMA user_version=3;").unwrap();
    let rows = |db: &Connection, table: &str| -> Vec<String> {
        db.prepare(&format!("SELECT * FROM {table} ORDER BY 1,2"))
            .unwrap()
            .query_map([], |r| {
                let values = (0..r.as_ref().column_count())
                    .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                    .collect::<Vec<_>>();
                Ok(values.join("|"))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let users = rows(&db, "users");
    let grants = rows(&db, "grants");
    let operations = rows(&db, "operations");
    drop(db);
    let c = Catalog::open(&fake.root, "test", None).unwrap();
    assert_eq!(rows(&c.db, "users"), users);
    assert_eq!(rows(&c.db, "grants"), grants);
    assert_eq!(rows(&c.db, "operations"), operations);
    assert_eq!(
        reservation(&c, "m-fixture"),
        ("offline".into(), 0, None, 256, 1)
    );
    assert!(
        c.db.query_row(
            "SELECT metadata FROM resources WHERE physical='m-fixture'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap()
        .contains("public-fixture")
    );
    assert_eq!(
        c.db.query_row(
            "SELECT value FROM settings WHERE key='fixture_public_keys'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "[\"fixture-key\"]"
    );
    assert_eq!(
        c.db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        nodes::SCHEMA
    );
    assert_eq!(
        c.db.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        c.db.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| r
            .get::<_, i64>(
            0
        ))
        .unwrap(),
        0
    );
    assert!(
        c.db.execute(
            "UPDATE resources SET current_capture='missing' WHERE physical='m-fixture'",
            []
        )
        .is_err()
    );
    for reader in [3, 4, 5] {
        assert!(nodes::readable_by(&c.db, reader).is_err());
    }
    assert!(nodes::readable_by(&c.db, nodes::SCHEMA).is_ok());
    let backup = std::fs::read_dir(&fake.root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("catalog.pre-placement-")
        })
        .unwrap();
    nodes::private(&backup).unwrap();
    let db = Connection::open(backup).unwrap();
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(rows(&db, "operations"), operations);
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o077,
        0
    );
}
#[test]
fn transfer_operation_join_conflict_and_atomic_competing_requests() {
    let (_fake, c, user, physical, intent) = transfer_fixture();
    let copies = (0..2)
        .map(|_| Catalog {
            db: nodes::db(&c.root).unwrap(),
            root: c.root.clone(),
            _lock: c._lock.try_clone().unwrap(),
        })
        .collect::<Vec<_>>();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let results = std::thread::scope(|scope| {
        copies
            .into_iter()
            .enumerate()
            .map(|(i, copy)| {
                let b = barrier.clone();
                let intent = intent.clone();
                scope.spawn(move || {
                    b.wait();
                    copy.begin_transfer(user, &format!("move-{i}"), &intent)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let t = results.into_iter().find_map(Result::ok).unwrap();
    let key: String =
        c.db.query_row(
            "SELECT retry_key FROM operations WHERE id=?1",
            [t.operation],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(c.begin_transfer(user, &key, &intent).unwrap().id, t.id);
    let mut changed = intent.clone();
    changed.accept_cold = true;
    assert!(
        c.begin_transfer(user, &key, &changed)
            .unwrap_err()
            .to_string()
            .contains("conflicts")
    );
    assert_eq!(
        c.db.query_row("SELECT COUNT(*) FROM transfer_reservations", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(c.extra_usage("beta").unwrap(), (256, 1, 1));
    assert_eq!(
        reservation(&c, &physical),
        ("alpha".into(), 0, None, 256, 1)
    );
    assert!(c.db.is_autocommit());
}
#[test]
fn transfer_uncertain_import_abort_and_epoch_never_reused_after_restart() {
    use crate::transfers::*;
    let (fake, c, user, physical, intent) = transfer_fixture();
    let t = c.begin_transfer(user, "first", &intent).unwrap();
    assert!(
        c.advance_transfer(
            t.id,
            t.epoch,
            Step::Preparing,
            Step::Exported,
            &Evidence::default()
        )
        .is_err()
    );
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Preparing,
        Step::Exported,
        &Evidence {
            source_fenced: true,
            ..Default::default()
        },
    )
    .unwrap();
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Exported,
        Step::Importing,
        &Evidence::default(),
    )
    .unwrap();
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Importing,
        Step::ImportUncertain,
        &Evidence::default(),
    )
    .unwrap();
    c.db.execute("UPDATE nodes SET heartbeat=0", []).unwrap();
    drop(c);
    let c = Catalog::open(&fake.root, "test", None).unwrap();
    assert_eq!(c.transfer(t.id).unwrap().step, Step::ImportUncertain);
    assert!(
        c.advance_transfer(
            t.id,
            t.epoch,
            Step::ImportUncertain,
            Step::Aborted,
            &Evidence::default()
        )
        .is_err()
    );
    assert!(
        c.advance_transfer(
            t.id,
            t.epoch,
            Step::ImportUncertain,
            Step::Failed,
            &Evidence::default()
        )
        .is_err()
    );
    assert_eq!(
        reservation(&c, &physical),
        ("alpha".into(), 0, None, 256, 1)
    );
    assert_eq!(c.extra_usage("beta").unwrap(), (256, 1, 1));
    assert!(c.commit_transfer(t.id, t.epoch).is_err());
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::ImportUncertain,
        Step::Aborted,
        &Evidence {
            target_excluded: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(c.extra_usage("beta").unwrap(), (0, 0, 0));
    c.db.execute("UPDATE nodes SET heartbeat=?1", [nodes_now()])
        .unwrap();
    let next = c.begin_transfer(user, "second", &intent).unwrap();
    assert!(next.epoch > t.epoch);
    assert!(
        c.advance_transfer(
            t.id,
            t.epoch,
            Step::ImportUncertain,
            Step::Aborted,
            &Evidence {
                target_excluded: true,
                ..Default::default()
            }
        )
        .is_err()
    );
    assert!(
        c.advance_transfer(
            next.id,
            t.epoch,
            Step::Preparing,
            Step::Failed,
            &Evidence::default()
        )
        .is_err()
    );
    assert_eq!(c.extra_usage("beta").unwrap(), (256, 1, 1));
    assert_eq!(
        c.begin_transfer(user, "first", &intent).unwrap().step,
        Step::Aborted
    );
}
#[test]
fn transfer_commit_is_atomic_and_terminal_transitions_are_forward_only() {
    use crate::transfers::*;
    let (_fake, mut c, user, physical, intent) = transfer_fixture();
    let t = c.begin_transfer(user, "move", &intent).unwrap();
    prepare_transfer(&c, &t);
    assert_eq!(
        reservation(&c, &physical),
        ("alpha".into(), 0, None, 256, 1)
    );
    // Inject a transaction failure after the placement UPDATE: none of the tuple may escape.
    c.db.execute_batch("CREATE TRIGGER fail_commit BEFORE DELETE ON transfer_reservations BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(c.commit_transfer(t.id, t.epoch).is_err());
    assert_eq!(
        reservation(&c, &physical),
        ("alpha".into(), 0, None, 256, 1)
    );
    assert_eq!(c.transfer(t.id).unwrap().step, Step::Prepared);
    assert_eq!(c.extra_usage("beta").unwrap(), (256, 1, 1));
    c.db.execute_batch("DROP TRIGGER fail_commit").unwrap();
    c.commit_transfer(t.id, t.epoch).unwrap();
    assert_eq!(
        reservation(&c, &physical),
        ("beta".into(), t.epoch, Some("capture".into()), 256, 1)
    );
    assert_eq!(c.extra_usage("beta").unwrap(), (0, 0, 0));
    c.save_node("alpha", "machine", &physical, &json!({"state":"stopped"}))
        .unwrap();
    c.save_node("beta", "machine", &physical, &json!({"state":"stopped"}))
        .unwrap();
    assert_eq!(
        reservation(&c, &physical).3,
        256,
        "epochless observations cannot release managed capacity"
    );
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Committed,
        Step::StartUncertain,
        &Evidence::default(),
    )
    .unwrap();
    for terminal in [Step::Aborted, Step::Failed, Step::Succeeded] {
        assert!(
            c.advance_transfer(
                t.id,
                t.epoch,
                Step::StartUncertain,
                terminal,
                &Evidence {
                    target_excluded: true,
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
    assert!(c.begin_transfer(user, "retarget", &intent).is_err());
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::StartUncertain,
        Step::Started,
        &Evidence::default(),
    )
    .unwrap();
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Started,
        Step::Succeeded,
        &Evidence::default(),
    )
    .unwrap();
    assert_eq!(
        c.db.query_row("SELECT COUNT(*) FROM transfer_locks", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(c.commit_transfer(t.id, t.epoch).is_err());
    assert!(c.operation(user, json!({"op":"start","id":"box"})).is_err());
    assert_eq!(c.terminal_target(user, "box").unwrap().1, t.machine);
    assert_eq!(
        c.managed_placement(&t.machine).unwrap().unwrap().epoch,
        t.epoch as u64
    );
    assert!(c.ssh_target(user, "box").is_err()); // The fixture does not advertise guest SSH.
}
#[test]
fn transfer_fork_reserves_child_identity_and_releases_only_its_attempt() {
    use crate::transfers::*;
    let (_fake, mut c, user, physical, mut intent) = transfer_fixture();
    intent.kind = Kind::Fork;
    intent.child = Some(Child {
        id: "m-child".into(),
        name: "child".into(),
    });
    let t = c.begin_transfer(user, "fork", &intent).unwrap();
    assert_eq!(c.physical(user, "machine", "child").unwrap(), "m-child");
    assert_eq!(reservation(&c, "m-child"), ("".into(), 0, None, 0, 0));
    assert!(!c.legacy_machine("m-child").unwrap());
    assert!(
        c.begin_transfer(
            user,
            "other-source",
            &Intent {
                machine: "child".into(),
                ..intent.clone()
            }
        )
        .is_err()
    );
    assert!(
        c.operation(user, json!({"op":"create","id":"child"}))
            .is_err()
    );
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Preparing,
        Step::Failed,
        &Evidence::default(),
    )
    .unwrap();
    assert!(c.physical(user, "machine", "child").is_err());
    assert_eq!(
        c.begin_transfer(user, "fork", &intent).unwrap().step,
        Step::Failed
    );
    let next = c.begin_transfer(user, "fork-next", &intent).unwrap();
    assert!(next.epoch > t.epoch);
    prepare_transfer(&c, &next);
    c.commit_transfer(next.id, next.epoch).unwrap();
    assert_eq!(
        reservation(&c, &physical),
        ("alpha".into(), 0, None, 256, 1)
    );
    assert_eq!(reservation(&c, "m-child").0, "beta");
    assert_eq!(
        c.db.query_row(
            "SELECT ssh_policy FROM resources WHERE physical='m-child'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "\"disabled\""
    );
}
#[test]
fn transfer_permissions_capture_lineage_and_policy_generation_fail_closed() {
    use crate::transfers::*;
    let (_fake, mut c, user, physical, intent) = transfer_fixture();
    let guest = c.user(&identity("guest")).unwrap();
    c.db.execute(
        "INSERT INTO grants(physical,grantee,role) VALUES(?1,?2,'admin')",
        params![physical, guest],
    )
    .unwrap();
    let mut shared = intent.clone();
    shared.machine = "owner@example.test/box".into();
    assert!(c.begin_transfer(guest, "not-owner", &shared).is_err());
    shared.kind = Kind::Fork;
    shared.child = Some(Child {
        id: "m-shared-child".into(),
        name: "copy".into(),
    });
    let fork = c.begin_transfer(guest, "fork", &shared).unwrap();
    c.advance_transfer(
        fork.id,
        fork.epoch,
        Step::Preparing,
        Step::Aborted,
        &Evidence::default(),
    )
    .unwrap();
    let t = c.begin_transfer(user, "move", &intent).unwrap();
    prepare_transfer(&c, &t);
    c.set_transfer_ssh_policy(user, "box", 1, &PublicSshPolicy::Disabled)
        .unwrap();
    assert!(
        c.set_transfer_ssh_policy(user, "box", 1, &PublicSshPolicy::Disabled)
            .is_err()
    );
    assert!(
        c.commit_transfer(t.id, t.epoch)
            .unwrap_err()
            .to_string()
            .contains("policy changed")
    );
    assert_eq!(c.extra_usage("beta").unwrap(), (256, 1, 1));
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Prepared,
        Step::Aborted,
        &Evidence {
            target_excluded: true,
            ..Default::default()
        },
    )
    .unwrap();
    c.db.execute("UPDATE artifacts SET owner=?1 WHERE id='capture'", [guest])
        .unwrap();
    assert!(c.begin_transfer(user, "bad-capture", &intent).is_err());
    c.db.execute(
        "UPDATE artifacts SET owner=?1,machine='other-machine' WHERE id='capture'",
        [user],
    )
    .unwrap();
    assert!(c.begin_transfer(user, "wrong-lineage", &intent).is_err());
}
#[test]
fn transfer_park_preserves_stopped_state_and_wake_requires_hibernation() {
    use crate::transfers::*;
    let (fake, c, user, physical, mut intent) = transfer_fixture();
    c.db.execute(
        "UPDATE resources SET metadata=json_set(metadata,'$.state','stopped') WHERE physical=?1",
        [&physical],
    )
    .unwrap();
    intent.kind = Kind::Park;
    intent.target = None;
    let t = c.begin_transfer(user, "park", &intent).unwrap();
    prepare_transfer(&c, &t);
    c.commit_transfer(t.id, t.epoch).unwrap();
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Committed,
        Step::Succeeded,
        &Evidence::default(),
    )
    .unwrap();
    assert_eq!(
        reservation(&c, &physical),
        ("".into(), t.epoch, Some("capture".into()), 0, 0)
    );
    drop(c);
    let c = Catalog::open(&fake.root, "test", None).unwrap();
    intent.kind = Kind::Wake;
    intent.target = Some("beta".into());
    assert!(
        c.begin_transfer(user, "wake", &intent)
            .unwrap_err()
            .to_string()
            .contains("stopped")
    );
    c.db.execute(
        "UPDATE resources SET metadata=json_set(metadata,'$.state','hibernated') WHERE physical=?1",
        [&physical],
    )
    .unwrap();
    let wake = c.begin_transfer(user, "wake", &intent).unwrap();
    assert_eq!(c.begin_transfer(user, "wake", &intent).unwrap().id, wake.id);
    assert!(c.begin_transfer(user, "competing-wake", &intent).is_err());
    assert_eq!(reservation(&c, &physical).0, "");
    prepare_transfer(&c, &wake);
    c.commit_transfer(wake.id, wake.epoch).unwrap();
    assert!(wake.epoch > t.epoch);
}

#[test]
fn transfer_reservations_block_legacy_admission_and_heartbeat_cannot_release_source() {
    use crate::transfers::*;
    let (mut fake, mut c, user, physical, intent) = transfer_fixture();
    // A synthetic source inventory reports stopped, then absent, while import is uncertain.
    c.db.execute("DELETE FROM nodes WHERE id='alpha'", [])
        .unwrap();
    fake.worker("alpha");
    wire::request(
        &fake.root.join("nodes/alpha"),
        json!({"op":"create","id":physical,"memory_mib":256}),
    )
    .unwrap();
    c.db.execute(
        "UPDATE nodes SET capabilities='{\"memory_mib\":512,\"slots\":2,\"vcpus\":1,\"images\":[\"alpine\"]}' WHERE id='beta'",
        [],
    )
    .unwrap();
    let t = c.begin_transfer(user, "move", &intent).unwrap();
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Preparing,
        Step::Exported,
        &Evidence {
            source_fenced: true,
            ..Default::default()
        },
    )
    .unwrap();
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Exported,
        Step::Importing,
        &Evidence::default(),
    )
    .unwrap();
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Importing,
        Step::ImportUncertain,
        &Evidence::default(),
    )
    .unwrap();
    let error = c
        .capacity("beta", "another-id", 256, 1, "alpine", &json!({}))
        .unwrap_err();
    assert!(
        error.to_string().contains("node capacity exhausted"),
        "{error}"
    );
    wire::request(
        &fake.root.join("nodes/alpha"),
        json!({"op":"stop","id":physical}),
    )
    .unwrap();
    c.reconcile().unwrap();
    assert_eq!(
        reservation(&c, &physical),
        ("alpha".into(), 0, None, 256, 1)
    );
    wire::request(
        &fake.root.join("nodes/alpha"),
        json!({"op":"lose","id":physical}),
    )
    .unwrap();
    c.reconcile().unwrap();
    assert_eq!(reservation(&c, &physical).3, 256);
    assert_eq!(c.extra_usage("beta").unwrap(), (256, 1, 1));
    c.db.execute("UPDATE nodes SET heartbeat=0 WHERE id='alpha'", [])
        .unwrap();
    c.reconcile().unwrap();
    assert_eq!(reservation(&c, &physical).3, 256);
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::ImportUncertain,
        Step::Prepared,
        &Evidence {
            target_prepared: true,
            ..Default::default()
        },
    )
    .unwrap();
    c.commit_transfer(t.id, t.epoch).unwrap();
    wire::request(
        &fake.root.join("nodes/alpha"),
        json!({"op":"recover","id":physical}),
    )
    .unwrap();
    c.db.execute(
        "UPDATE nodes SET heartbeat=?1 WHERE id='alpha'",
        [nodes_now()],
    )
    .unwrap();
    c.reconcile().unwrap();
    assert_eq!(
        reservation(&c, &physical),
        ("beta".into(), t.epoch, Some("capture".into()), 256, 1)
    );
}
#[test]
fn transfer_schema_four_keeps_store_roots_and_failed_preparation_rolls_back() {
    use crate::transfers::*;
    let (fake, c, user, physical, intent) = transfer_fixture();
    c.db.execute("INSERT INTO artifact_copies(artifact,node,state,last_used) VALUES('capture','beta','present',1)",[]).unwrap();
    c.db.execute("INSERT INTO leases(artifact,holder,purpose,expires) VALUES('capture','fixture','pull',9999999999)",[]).unwrap();
    let triggers: Vec<String> =
        c.db.prepare(
            "SELECT name FROM sqlite_master WHERE type='trigger' AND (name LIKE 'cache_guard_%' OR name LIKE 'gc_%' OR name LIKE 'drain_%')",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    for trigger in triggers {
        c.db.execute_batch(&format!("DROP TRIGGER {trigger}"))
            .unwrap();
    }
    c.db.execute_batch("DROP TABLE cache_evictions").unwrap();
    c.db.execute_batch("DROP TABLE transfer_reservations;DROP TABLE transfer_locks;DROP TABLE transfers;
      ALTER TABLE resources DROP COLUMN placement_managed;ALTER TABLE resources DROP COLUMN placement_epoch;
      ALTER TABLE resources DROP COLUMN current_capture;ALTER TABLE resources DROP COLUMN ssh_policy;
      ALTER TABLE resources DROP COLUMN ssh_policy_generation;UPDATE settings SET value='3' WHERE key='min_reader_schema';PRAGMA user_version=4;").unwrap();
    drop(c);
    let c = Catalog::open(&fake.root, "test", None).unwrap();
    assert_eq!(
        c.db.query_row("SELECT COUNT(*) FROM artifact_copies", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        c.db.query_row("SELECT COUNT(*) FROM leases", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    c.set_transfer_ssh_policy(user, "box", 0, &PublicSshPolicy::Disabled)
        .unwrap();
    c.db.execute("UPDATE nodes SET memory_mib=128 WHERE id='beta'", [])
        .unwrap();
    let mut fork = intent.clone();
    fork.kind = Kind::Fork;
    fork.child = Some(Child {
        id: "m-rejected".into(),
        name: "rejected".into(),
    });
    assert!(c.begin_transfer(user, "no-capacity", &fork).is_err());
    assert!(c.physical(user, "machine", "rejected").is_err());
    assert_eq!(
        c.db.query_row("SELECT COUNT(*) FROM transfers", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        c.db.query_row(
            "SELECT COUNT(*) FROM operations WHERE retry_key='no-capacity'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert!(c.db.is_autocommit());
    assert_eq!(reservation(&c, &physical).0, "alpha");
}

#[test]
fn transfer_import_pins_capture_and_commit_rechecks_authority_and_grants() {
    use crate::transfers::*;
    let (_fake, mut c, user, physical, mut intent) = transfer_fixture();
    // Captures cannot be swapped after the import dispatch boundary.
    let t = c.begin_transfer(user, "move", &intent).unwrap();
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Preparing,
        Step::Exported,
        &Evidence {
            source_fenced: true,
            ..Default::default()
        },
    )
    .unwrap();
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Exported,
        Step::Importing,
        &Evidence::default(),
    )
    .unwrap();
    assert!(
        c.advance_transfer(
            t.id,
            t.epoch,
            Step::Importing,
            Step::Prepared,
            &Evidence {
                capture: Some("capture".into()),
                target_prepared: true,
                ..Default::default()
            }
        )
        .is_err()
    );
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Importing,
        Step::Prepared,
        &Evidence {
            target_prepared: true,
            ..Default::default()
        },
    )
    .unwrap();
    // A delayed commit cannot override a newer authority, even with a prepared target.
    c.db.execute(
        "UPDATE resources SET placement_epoch=?1 WHERE physical=?2",
        params![t.epoch + 1, physical],
    )
    .unwrap();
    assert!(c.commit_transfer(t.id, t.epoch).is_err());
    assert_eq!(reservation(&c, &physical).1, t.epoch + 1);
    assert_eq!(c.extra_usage("beta").unwrap(), (256, 1, 1));
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Prepared,
        Step::Aborted,
        &Evidence {
            target_excluded: true,
            ..Default::default()
        },
    )
    .unwrap();
    let guest = c.user(&identity("guest")).unwrap();
    c.db.execute(
        "INSERT INTO grants(physical,grantee,role) VALUES(?1,?2,'use')",
        params![physical, guest],
    )
    .unwrap();
    intent.machine = "owner@example.test/box".into();
    intent.kind = Kind::Fork;
    intent.child = Some(Child {
        id: "m-revoked-child".into(),
        name: "revoked-child".into(),
    });
    let fork = c.begin_transfer(guest, "shared-fork", &intent).unwrap();
    assert!(fork.epoch > t.epoch + 1);
    prepare_transfer(&c, &fork);
    c.db.execute("DELETE FROM grants WHERE grantee=?1", [guest])
        .unwrap();
    assert!(c.commit_transfer(fork.id, fork.epoch).is_err());
    assert!(c.begin_transfer(guest, "shared-fork", &intent).is_err());
    assert_eq!(reservation(&c, "m-revoked-child").0, "");
    assert_eq!(c.extra_usage("beta").unwrap(), (256, 1, 1));
}
#[test]
fn transfer_target_budget_clamps_operator_caps_and_rejects_stale_or_future_beats() {
    use crate::transfers::*;
    let (_fake, c, user, physical, intent) = transfer_fixture();
    c.db.execute(
        "UPDATE nodes SET capabilities=json_set(capabilities,'$.memory_mib',128) WHERE id='beta'",
        [],
    )
    .unwrap();
    assert!(c.begin_transfer(user, "small-worker", &intent).is_err());
    c.db.execute(
        "UPDATE nodes SET capabilities=json_set(capabilities,'$.memory_mib',512) WHERE id='beta'",
        [],
    )
    .unwrap();
    c.db.execute(
        "INSERT INTO node_capacity_limits(node,vcpu_cap) VALUES('beta',1)",
        [],
    )
    .unwrap();
    assert!(
        c.begin_transfer(
            user,
            "operator-cap",
            &Intent {
                cpus: 2,
                ..intent.clone()
            }
        )
        .is_err()
    );
    for beat in [nodes_now() - 60, nodes_now() + 60] {
        c.db.execute("UPDATE nodes SET heartbeat=?1 WHERE id='beta'", [beat])
            .unwrap();
        assert!(c.begin_transfer(user, "bad-heartbeat", &intent).is_err());
    }
    c.db.execute("UPDATE nodes SET heartbeat=?1,capabilities=json_set(capabilities,'$.drain',json('true')) WHERE id='beta'",[nodes_now()]).unwrap();
    assert!(c.begin_transfer(user, "draining", &intent).is_err());
    assert!(
        c.begin_transfer(
            user,
            "implicit-local",
            &Intent {
                target: Some("local".into()),
                ..intent
            }
        )
        .is_err()
    );
    assert_eq!(
        reservation(&c, &physical),
        ("alpha".into(), 0, None, 256, 1)
    );
    assert_eq!(
        c.db.query_row("SELECT COUNT(*) FROM transfers", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn aborted_fork_does_not_leak_grants_to_reused_id() {
    use crate::transfers::*;
    for terminal in [Step::Aborted, Step::Failed] {
        let (_fake, mut c, user, _physical, mut intent) = transfer_fixture();
        let guest = c.user(&identity("guest")).unwrap();
        let other_owner = c.user(&identity("other-owner")).unwrap();
        c.operation(
            user,
            json!({"op":"share","id":"box","with":"other-owner@example.test","role":"use"}),
        )
        .unwrap();
        intent.kind = Kind::Fork;
        intent.child = Some(Child {
            id: "m-reused".into(),
            name: "first-child".into(),
        });
        let first = c.begin_transfer(user, "first-fork", &intent).unwrap();
        c.operation(
            user,
            json!({"op":"share","id":"first-child","with":"guest@example.test","role":"use"}),
        )
        .unwrap();
        c.advance_transfer(
            first.id,
            first.epoch,
            Step::Preparing,
            terminal,
            &Evidence::default(),
        )
        .unwrap();
        let grants: i64 =
            c.db.query_row(
                "SELECT COUNT(*) FROM grants WHERE physical='m-reused'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        println!(
            "After aborted fork: child absent={}, dangling grants={grants}",
            c.physical(user, "machine", "first-child").is_err()
        );
        intent.child = Some(Child {
            id: "m-reused".into(),
            name: "different-child".into(),
        });
        intent.machine = "owner@example.test/box".into();
        let second = c
            .begin_transfer(other_owner, "second-fork", &intent)
            .unwrap();
        prepare_transfer(&c, &second);
        c.commit_transfer(second.id, second.epoch).unwrap();
        let access = c.machine(guest, "other-owner@example.test/different-child", "fork");
        println!(
            "Guest authorized to another owner's new unshared child: {}",
            access.is_ok()
        );
        c.advance_transfer(
            second.id,
            second.epoch,
            Step::Committed,
            Step::Started,
            &Evidence::default(),
        )
        .unwrap();
        c.advance_transfer(
            second.id,
            second.epoch,
            Step::Started,
            Step::Succeeded,
            &Evidence::default(),
        )
        .unwrap();
        c.db.execute("INSERT INTO artifacts(id,kind,owner,machine,local_name,state,digest,store_url) VALUES('replacement-capture','capture',?1,'m-reused','replacement','stored',?2,'fixture://store')",params![other_owner,"b".repeat(64)]).unwrap();
        let illicit = c.begin_transfer(
            guest,
            "unshared-fork",
            &Intent {
                machine: "other-owner@example.test/different-child".into(),
                capture: Some("replacement-capture".into()),
                child: Some(Child {
                    id: "m-unshared-copy".into(),
                    name: "unshared-copy".into(),
                }),
                ..intent.clone()
            },
        );
        println!(
            "Uninvited guest admitted a fork of replacement owner's capture: {}",
            illicit.is_ok()
        );
        assert!(
            illicit.is_err(),
            "aborted child's grant authorized a different owner's replacement"
        );
    }
}

#[test]
fn aborted_fork_name_is_reusable_by_legacy_create() {
    use crate::transfers::*;
    for terminal in [Step::Aborted, Step::Failed] {
        let (mut fake, mut c, user, _physical, mut intent) = transfer_fixture();
        c.db.execute("DELETE FROM nodes WHERE id='alpha'", [])
            .unwrap();
        fake.worker("alpha");
        intent.kind = Kind::Fork;
        intent.child = Some(Child {
            id: "m-aborted".into(),
            name: "child".into(),
        });
        let t = c.begin_transfer(user, "fork-abort", &intent).unwrap();
        c.advance_transfer(
            t.id,
            t.epoch,
            Step::Preparing,
            terminal,
            &Evidence::default(),
        )
        .unwrap();
        let result = c.operation(user,json!({"op":"create","id":"child","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"}));
        println!("Legacy create using released child name: {result:?}");
        let control = c.operation(user,json!({"op":"create","id":"fresh-name","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine"}));
        println!(
            "Legacy create using fresh name: success={}",
            control.is_ok()
        );
        assert!(control.is_ok());
        assert!(
            result.is_ok(),
            "aborted fork left child name unusable to legacy create"
        );
    }
}

#[test]
fn aborted_fork_cleanup_rolls_back_with_an_uncertain_target() {
    use crate::transfers::*;
    let (_fake, mut c, user, _physical, mut intent) = transfer_fixture();
    let guest = c.user(&identity("guest")).unwrap();
    intent.kind = Kind::Fork;
    intent.child = Some(Child {
        id: "m-cleanup".into(),
        name: "child".into(),
    });
    let t = c.begin_transfer(user, "fork-cleanup", &intent).unwrap();
    c.operation(
        user,
        json!({"op":"share","id":"child","with":"guest@example.test","role":"use"}),
    )
    .unwrap();
    c.db.execute(
        "UPDATE owner_allocations SET cpus=1,memory_mib=256,disk_gib=8 WHERE physical='m-cleanup'",
        [],
    )
    .unwrap();
    c.db.execute("INSERT INTO publications(https_port,physical,guest_port,active) VALUES(11000,'m-cleanup',80,1)",[]).unwrap();
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Preparing,
        Step::Importing,
        &Evidence::default(),
    )
    .unwrap();
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Importing,
        Step::ImportUncertain,
        &Evidence::default(),
    )
    .unwrap();
    let metadata = || {
        c.db.query_row("SELECT (SELECT COUNT(*) FROM grants WHERE physical='m-cleanup'),(SELECT COUNT(*) FROM owner_allocations WHERE physical='m-cleanup'),(SELECT active FROM publications WHERE physical='m-cleanup'),(SELECT resource FROM operations WHERE id=?1)",[t.operation],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?))).unwrap()
    };
    assert!(
        c.advance_transfer(
            t.id,
            t.epoch,
            Step::ImportUncertain,
            Step::Failed,
            &Evidence::default()
        )
        .is_err()
    );
    assert_eq!(metadata(), (1, 1, 1, "child".into()));
    c.db.execute_batch("CREATE TRIGGER fail_child_delete BEFORE DELETE ON resources WHEN OLD.kind='machine' AND OLD.physical='m-cleanup' BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let proof = Evidence {
        target_excluded: true,
        ..Default::default()
    };
    assert!(
        c.advance_transfer(t.id, t.epoch, Step::ImportUncertain, Step::Failed, &proof)
            .is_err()
    );
    assert_eq!(metadata(), (1, 1, 1, "child".into()));
    assert_eq!(c.physical(user, "machine", "child").unwrap(), "m-cleanup");
    assert_eq!(c.transfer(t.id).unwrap().step, Step::ImportUncertain);
    assert_eq!(c.extra_usage("beta").unwrap(), (256, 1, 1));
    c.db.execute_batch("DROP TRIGGER fail_child_delete")
        .unwrap();
    c.advance_transfer(t.id, t.epoch, Step::ImportUncertain, Step::Failed, &proof)
        .unwrap();
    assert_eq!(metadata(), (0, 0, 0, "deleted:child".into()));
    assert!(c.physical(user, "machine", "child").is_err());
    assert_eq!(c.extra_usage("beta").unwrap(), (0, 0, 0));
    assert_eq!(
        c.begin_transfer(user, "fork-cleanup", &intent)
            .unwrap()
            .step,
        Step::Failed
    );
    assert!(
        c.machine(guest, "owner@example.test/child", "fork")
            .is_err()
    );
}

#[test]
fn transfer_target_rechecks_controller_drain_independently_of_worker_drain() {
    let (fake, c, user, _, intent) = transfer_fixture();
    c.db.execute(
        "INSERT INTO node_placement_policy(node,controller_drain) VALUES('beta',1)",
        [],
    )
    .unwrap();
    assert!(
        c.begin_transfer(user, "controller-drain", &intent)
            .unwrap_err()
            .to_string()
            .contains("draining")
    );
    c.db.execute("UPDATE node_placement_policy SET controller_drain=0", [])
        .unwrap();
    c.db.execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.drain',json('true')) WHERE id='beta'",[]).unwrap();
    assert!(
        c.begin_transfer(user, "worker-drain", &intent)
            .unwrap_err()
            .to_string()
            .contains("draining")
    );
    drop(c);
    drop(fake);
}

#[test]
fn adopted_restore_preserves_owner_grants_and_routes_exec_and_streams_with_epoch() {
    use crate::transfers::*;
    let (mut fake, mut c, user, physical, mut intent) = transfer_fixture();
    intent.kind = Kind::Restore;
    c.db.execute("DELETE FROM nodes WHERE id='beta'", [])
        .unwrap();
    fake.worker("beta");
    c.db.execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.guest_ssh_v1',json('true')) WHERE id='beta'",[]).unwrap();
    let grantee = c.user(&identity("guest")).unwrap();
    c.db.execute(
        "INSERT INTO grants(physical,grantee,role) VALUES(?1,?2,'use')",
        params![physical, grantee],
    )
    .unwrap();
    let t = c.begin_transfer(user, "restore", &intent).unwrap();
    prepare_transfer(&c, &t);
    c.commit_transfer(t.id, t.epoch).unwrap();
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Committed,
        Step::Started,
        &Evidence::default(),
    )
    .unwrap();
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Started,
        Step::Succeeded,
        &Evidence::default(),
    )
    .unwrap();
    let placement = c.managed_placement(&physical).unwrap().unwrap();
    assert_eq!(placement.node, "beta");
    assert_eq!(placement.epoch, t.epoch as u64);
    let address = "owner@example.test/box";
    assert_eq!(c.state(user).unwrap()["workspaces"][0]["id"], "box");
    assert_eq!(c.state(grantee).unwrap()["shared"][0]["id"], address);
    let result = c
        .operation(
            grantee,
            json!({"op":"exec","id":address,"command":"true","operation_key":"exec-restored"}),
        )
        .unwrap();
    assert_eq!(result["exit_code"], 0);
    let request:String=c.db.query_row("SELECT json_extract(request,'$.request.placement') FROM operations WHERE retry_key='exec-restored'",[],|r|r.get(0)).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&request).unwrap(),
        json!(placement)
    );
    assert_eq!(c.ssh_target(grantee, address).unwrap().1, physical);
    assert_eq!(c.terminal_target(grantee, address).unwrap().1, physical);
    assert!(
        c.operation(grantee, json!({"op":"exec","id":physical,"command":"true"}))
            .is_err()
    );
    c.operation(
        user,
        json!({"op":"unshare","id":"box","with":"guest@example.test"}),
    )
    .unwrap();
    assert!(c.ssh_target(grantee, address).is_err());
}

fn completed_restore_fixture() -> (Fake, Catalog, i64, String, common::Placement) {
    use crate::transfers::*;
    let (fake, c, user, physical, mut intent) = transfer_fixture();
    intent.kind = Kind::Restore;
    c.db.execute(
        "UPDATE nodes SET capabilities=json_set(capabilities,'$.images',json('[\"alpine\"]'))",
        [],
    )
    .unwrap();
    let t = c.begin_transfer(user, "restored", &intent).unwrap();
    prepare_transfer(&c, &t);
    c.commit_transfer(t.id, t.epoch).unwrap();
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Committed,
        Step::Started,
        &Evidence::default(),
    )
    .unwrap();
    c.advance_transfer(
        t.id,
        t.epoch,
        Step::Started,
        Step::Succeeded,
        &Evidence::default(),
    )
    .unwrap();
    let placement = c.managed_placement(&physical).unwrap().unwrap();
    (fake, c, user, physical, placement)
}

#[test]
#[cfg(target_arch = "x86_64")] // Runtime host fingerprinting requires an x86 Firecracker host.
fn adopted_real_runtime_stop_reply_completes_and_refreshes_metadata() {
    let (mut fake, mut c, user, physical, placement) = completed_restore_fixture();
    let root = fake.root.join("nodes/beta");
    let assets = fake.root.join("assets");
    std::fs::create_dir_all(root.join("machines").join(&physical)).unwrap();
    std::fs::create_dir_all(assets.join("downloads")).unwrap();
    std::fs::write(assets.join("downloads/vmlinux-6.1.186"), b"fixture kernel").unwrap();
    std::fs::write(
        root.join("machines").join(&physical).join("disk.ext4"),
        b"disk",
    )
    .unwrap();
    std::fs::write(root.join("state.json"), json!({"next_index":2,"workspaces":{physical.clone():{"id":physical,"index":1,"state":"stopped","memory_mib":256,"vcpu_count":1,"image":"alpine","source":null,"hibernation":null}}}).to_string()).unwrap();
    let mut runtime = ow_core::runtime::Runtime::new(root.clone(), assets).unwrap();
    runtime.request(&json!({"op":"placement-register","id":physical,"placement":placement,"committed":true})).unwrap();
    let listener = UnixListener::bind(root.join("control.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = fake.stopped.clone();
    fake._thread = Some(std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let (mut socket, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
            };
            let request = wire::line(&mut socket).unwrap();
            let result = runtime.request(&request);
            if request["op"] == "stop" {
                assert!(
                    result.as_ref().unwrap()["workspace"]
                        .get("placement")
                        .is_none(),
                    "exercise the real unadorned worker reply"
                );
            }
            wire::send(&mut socket, &wire::response(result)).unwrap();
        }
    }));
    let result = c
        .operation(
            user,
            json!({"op":"stop","id":"box","operation_key":"stop-restored"}),
        )
        .unwrap();
    assert_eq!(result["operation"]["state"], "succeeded");
    let state = c.state(user).unwrap();
    assert_eq!(state["workspaces"][0]["state"], "stopped");
    assert_eq!(state["workspaces"][0]["vmm_running"], false);
    assert_eq!(state["workspaces"][0]["placement"], json!(placement));
    let operation: String =
        c.db.query_row(
            "SELECT state FROM operations WHERE retry_key='stop-restored'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(operation, "succeeded");
}

#[test]
fn adopted_inventory_refreshes_only_exact_committed_node_and_epoch() {
    let (mut fake, mut c, user, physical, placement) = completed_restore_fixture();
    let root = fake.root.join("nodes/beta");
    std::fs::create_dir_all(&root).unwrap();
    let listener = UnixListener::bind(root.join("control.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let observation = Arc::new(Mutex::new(
        json!({"id":physical,"state":"failed","vmm_running":false,"memory_mib":256,"vcpu_count":1,"placement":placement,"lifecycle":{"phase":"active"}}),
    ));
    let alpha_root = fake.root.join("nodes/alpha");
    std::fs::create_dir_all(&alpha_root).unwrap();
    let alpha_listener = UnixListener::bind(alpha_root.join("control.sock")).unwrap();
    alpha_listener.set_nonblocking(true).unwrap();
    let (observed, stop) = (observation.clone(), fake.stopped.clone());
    fake._thread = Some(std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let (mut socket, _) = match [listener.accept(), alpha_listener.accept()]
                .into_iter()
                .find_map(Result::ok)
            {
                Some(pair) => pair,
                None => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
            };
            let request = wire::line(&mut socket).unwrap();
            let result = match request["op"].as_str().unwrap() {
                "list" => json!([observed.lock().unwrap().clone()]),
                "snapshots" => json!([]),
                "stats" => json!({"running":{}}),
                op => panic!("unexpected {op}"),
            };
            wire::send(&mut socket, &json!({"ok":true,"result":result})).unwrap();
        }
    }));
    let state = c.state(user).unwrap();
    assert_eq!(state["workspaces"][0]["state"], "failed");
    assert_eq!(state["workspaces"][0]["vmm_running"], false);
    observation.lock().unwrap()["state"] = json!("stopped");
    assert_eq!(
        c.state(user).unwrap()["workspaces"][0]["state"],
        "stopped",
        "worker restart is positively observed"
    );
    observation.lock().unwrap()["placement"]["epoch"] = json!(placement.epoch + 1);
    observation.lock().unwrap()["state"] = json!("running");
    observation.lock().unwrap()["vmm_running"] = json!(true);
    assert_eq!(
        c.state(user).unwrap()["workspaces"][0]["state"],
        "stopped",
        "future epoch cannot overwrite authority"
    );
    observation.lock().unwrap()["placement"] = json!(placement);
    observation.lock().unwrap()["placement"]["node"] = json!("alpha");
    assert_eq!(
        c.state(user).unwrap()["workspaces"][0]["state"],
        "stopped",
        "wrong-node inventory cannot overwrite authority"
    );
    observation.lock().unwrap()["placement"] = json!(placement);
    c.db.execute("UPDATE nodes SET heartbeat=0 WHERE id='beta'", [])
        .unwrap();
    assert_eq!(
        c.state(user).unwrap()["workspaces"][0]["observed_state"],
        "stopped",
        "a source runner claiming target's exact epoch cannot refresh metadata"
    );
    assert_eq!(c.managed_placement(&physical).unwrap(), Some(placement));
}

#[test]
fn adopted_unadorned_start_and_hibernate_replies_are_verified_after_effect() {
    for (op, fail_inspect) in [("start", false), ("hibernate", false), ("stop", true)] {
        let (mut fake, mut c, user, physical, placement) = completed_restore_fixture();
        let root = fake.root.join("nodes/beta");
        std::fs::create_dir_all(&root).unwrap();
        let listener = UnixListener::bind(root.join("control.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (stop, id, p) = (fake.stopped.clone(), physical.clone(), placement.clone());
        fake._thread = Some(std::thread::spawn(move || {
            let mut state = if op == "start" { "stopped" } else { "running" };
            let mut effected = false;
            while !stop.load(Ordering::SeqCst) {
                let (mut socket, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                };
                let request = wire::line(&mut socket).unwrap();
                let mut summary = json!({"id":id,"state":state,"memory_mib":256,"vcpu_count":1,"disk_bytes":8*common::GIB,"image":"alpine","vmm_running":state=="running","placement":p,"lifecycle":{"phase":"active"}});
                let result = match request["op"].as_str().unwrap() {
                    "list" => json!([summary]),
                    "snapshots" => json!([]),
                    "stats" => json!({"running":{}}),
                    "inspect" => {
                        assert!(effected);
                        if fail_inspect {
                            summary["placement"]["epoch"] = json!(p.epoch + 1);
                        }
                        summary
                    }
                    effect if effect == op => {
                        assert_eq!(request["placement"], json!(p));
                        effected = true;
                        state = match effect {
                            "start" => "running",
                            "hibernate" => "hibernated",
                            _ => "stopped",
                        };
                        summary["state"] = json!(state);
                        summary.as_object_mut().unwrap().remove("placement");
                        if effect == "stop" {
                            json!({"workspace":summary,"sync_error":null})
                        } else {
                            summary
                        }
                    }
                    other => panic!("unexpected {other}"),
                };
                wire::send(&mut socket, &json!({"ok":true,"result":result})).unwrap();
            }
        }));
        let result = c.operation(
            user,
            json!({"op":op,"id":"box","operation_key":"lifecycle-after-restore"}),
        );
        let persisted: String =
            c.db.query_row(
                "SELECT state FROM operations WHERE retry_key='lifecycle-after-restore'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if fail_inspect {
            assert!(result.is_err());
            assert_eq!(
                persisted, "uncertain",
                "post-effect failure must never leave pending"
            );
        } else {
            assert!(result.is_ok(), "{op}: {result:?}");
            assert_eq!(persisted, "succeeded");
            let state = c.state(user).unwrap();
            assert_eq!(
                state["workspaces"][0]["state"],
                if op == "start" {
                    "running"
                } else {
                    "hibernated"
                }
            );
            assert_eq!(state["workspaces"][0]["placement"], json!(placement));
        }
    }
}

#[test]
fn controller_without_worker_has_no_local_fallback_or_capacity() {
    let fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    let owner = c.user(&identity("owner")).unwrap();
    let before = c.state(owner).unwrap();
    assert_eq!(before["stats"]["limit_memory_mib"], 0);
    assert_eq!(before["discovery"]["nodes"], json!([]));
    assert!(
        c.choose(None, 256, "alpine", 1, &json!({}), None)
            .unwrap_err()
            .to_string()
            .contains("no online node")
    );
    assert!(
        c.operation(owner, json!({"op":"create","id":"empty","memory_mib":256}))
            .unwrap_err()
            .to_string()
            .contains("no online node")
    );
    // Absence is not evidence that another node's machine belongs to this host.
    let physical = c.reserve(owner, "machine", "remote").unwrap();
    c.db.execute("UPDATE resources SET node='missing-runner',metadata='{\"state\":\"running\"}' WHERE physical=?1",[&physical]).unwrap();
    let state = c.state(owner).unwrap();
    assert_eq!(state["workspaces"][0]["node"], "missing-runner");
    assert_eq!(state["workspaces"][0]["state"], "offline");
    assert_eq!(c.placement("machine", &physical).unwrap(), "missing-runner");
    assert!(!fake.root.join("control.sock").exists());
}

#[test]
fn controller_private_backup_preserves_all_authority_and_artifact_roots() {
    let (fake, mut c, user, physical, intent) = transfer_fixture();
    let grantee = c.user(&identity("grantee")).unwrap();
    c.share(
        user,
        "share",
        &json!({"id":"box","with":"grantee@example.test","role":"use"}),
    )
    .unwrap();
    c.db.execute("UPDATE nodes SET credential_hash='fixture-hash'", [])
        .unwrap();
    c.db.execute(
        "UPDATE resources SET current_capture='capture' WHERE physical=?1",
        [&physical],
    )
    .unwrap();
    c.db.execute("INSERT INTO artifact_copies(artifact,node,state,last_used) VALUES('capture','beta','present',1)",[]).unwrap();
    c.db.execute("INSERT INTO leases(artifact,holder,purpose,expires) VALUES('capture','fixture','pull',9999999999)",[]).unwrap();
    let transfer = c.begin_transfer(user, "backup-transfer", &intent).unwrap();
    let output = fake.root.join("backup.sqlite3");
    crate::backup::run(
        None,
        crate::BackupAction::Catalog {
            output: output.clone(),
            from: Some(fake.root.clone()),
        },
    )
    .unwrap();
    assert_eq!(
        std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let restored = Fake::new();
    std::fs::copy(&output, restored.root.join("catalog.sqlite3")).unwrap();
    let restored_db = Connection::open(restored.root.join("catalog.sqlite3")).unwrap();
    assert_eq!(
        restored_db
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert_eq!(
        restored_db
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        0
    );
    for table in [
        "users",
        "resources",
        "grants",
        "nodes",
        "node_limits",
        "transfers",
        "transfer_locks",
        "transfer_reservations",
        "artifacts",
        "artifact_copies",
        "leases",
        "settings",
    ] {
        let rows = |db: &Connection| {
            let mut statement = db
                .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                .unwrap();
            let columns = statement.column_count();
            statement
                .query_map([], |r| {
                    Ok((0..columns)
                        .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                        .collect::<Vec<_>>())
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(rows(&c.db), rows(&restored_db), "lost {table}");
    }
    drop(restored_db);
    let copy = Catalog::open(&restored.root, "test", None).unwrap();
    assert_eq!(copy.transfer(transfer.id).unwrap().epoch, transfer.epoch);
    assert!(
        copy.machine(grantee, "owner@example.test/box", "exec")
            .is_ok()
    );
    assert_eq!(copy.placement("machine", &physical).unwrap(), "alpha");
    assert!(!restored.root.join("control.sock").exists());
}

#[test]
fn legacy_key_replacement_invalidates_a_concurrent_transfer_policy_read() {
    let mut fake = Fake::new();
    let mut c = Catalog::open(&fake.root, "test", None).unwrap();
    let user = c.user(&identity("owner")).unwrap();
    fake.worker("alpha");
    c.operation(user, json!({"op":"create","id":"box","node":"alpha"}))
        .unwrap();
    let physical = c.physical(user, "machine", "box").unwrap();
    c.db.execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.guest_ssh_v1',json('true')) WHERE id='alpha'",[]).unwrap();
    c.db.execute(
        "UPDATE resources SET ssh_policy='\"disabled\"' WHERE physical=?1",
        [&physical],
    )
    .unwrap();
    c.operation(
        user,
        json!({"op":"ssh-keys","id":"box","keys":[],"operation_key":"revoke"}),
    )
    .unwrap();
    let (policy, generation): (Option<String>, i64) =
        c.db.query_row(
            "SELECT ssh_policy,ssh_policy_generation FROM resources WHERE physical=?1",
            [&physical],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(policy.is_none());
    assert_eq!(generation, 1);
    assert!(
        c.set_transfer_ssh_policy(user, "box", 0, &crate::transfers::PublicSshPolicy::Disabled)
            .is_err()
    );
    c.set_transfer_ssh_policy(
        user,
        "box",
        generation,
        &crate::transfers::PublicSshPolicy::Disabled,
    )
    .unwrap();
}

#[test]
fn named_fork_shape_is_rechecked_before_atomic_admission() {
    use crate::transfers::*;
    let (fake, c, user, physical, mut input) = transfer_fixture();
    c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node) VALUES(?1,'snapshot','saved','saved-id',?2,'alpha')",params![user,json!({"workspace":physical,"memory_mib":256,"vcpu_count":1,"disk_bytes":input.disk_bytes}).to_string()]).unwrap();
    input.kind = Kind::Fork;
    input.capture = None;
    input.snapshot = Some("saved".into());
    input.child = Some(Child {
        id: "child-id".into(),
        name: "child".into(),
    });
    input.memory_mib = 512;
    assert!(
        c.begin_transfer(user, "named-fork", &input)
            .unwrap_err()
            .to_string()
            .contains("snapshot identity or shape changed")
    );
    assert!(c.physical(user, "machine", "child").is_err());
    assert_eq!(
        c.db.query_row("SELECT COUNT(*) FROM transfer_locks", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    input.memory_mib = 256;
    let t = c.begin_transfer(user, "named-fork", &input).unwrap();
    drop(c);
    let c = Catalog::open(&fake.root, "test", None).unwrap();
    assert_eq!(
        c.db.query_row(
            "SELECT json_extract(response,'$.fork_snapshot') FROM operations WHERE id=?1",
            [t.operation],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "saved-id"
    );
    assert_eq!(
        c.begin_transfer(user, "named-fork", &input).unwrap().id,
        t.id
    );
    input.snapshot = None;
    assert!(c.begin_transfer(user, "named-fork", &input).is_err());
}
