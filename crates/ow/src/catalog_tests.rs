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
        self._thread = Some(std::thread::spawn(move || {
            let mut machines = BTreeMap::<String, Value>::new();
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
                        &json!({"ok":false,"error":"simulated runtime rejection"}),
                    )
                    .unwrap();
                    continue;
                }
                let result = match op {
                    "list" => json!(machines.values().collect::<Vec<_>>()),
                    "snapshots" => json!(snapshots.values().collect::<Vec<_>>()),
                    "stats" => json!({"running":{},"reserved_memory_mib":0}),
                    "create" => {
                        let m = json!({"id":id,"memory_mib":request["memory_mib"],"vcpu_count":request["vcpu_count"],"image":request["image"],"state":"running","source":null});
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
                    "stop" => {
                        let m = machines.get_mut(id).unwrap();
                        m["state"] = json!("stopped");
                        json!({"workspace":m})
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
