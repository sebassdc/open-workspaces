use super::*;
use std::{cell::RefCell, collections::BTreeMap, path::PathBuf};
struct Fixture {
    root: PathBuf,
    shared: Shared,
    user: i64,
    transfer: Transfer,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("ow-transfer-{}", store::token().unwrap()));
        fs::create_dir(&root).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let mut c = Catalog::open(&root, "test", None).unwrap();
        let user = c
            .user(&Identity {
                issuer: "test".into(),
                subject: "owner".into(),
                email: "owner@example.test".into(),
            })
            .unwrap();
        c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node,ssh_policy) VALUES(?1,'machine','box','physical',?2,'source','\"disabled\"')",params![user,json!({"id":"physical","state":"stopped","memory_mib":256,"vcpu_count":1,"disk_bytes":4}).to_string()]).unwrap();
        c.db.execute("INSERT INTO artifacts(id,kind,owner,machine,local_name,state,attempt,digest,store_url) VALUES('capture','capture',?1,'physical','sealed','stored',?2,?3,'s3://bucket/root')",params![user,"a".repeat(32),"b".repeat(64)]).unwrap();
        for node in ["source", "target"] {
            c.db.execute("INSERT INTO nodes(id,memory_mib,slots,heartbeat,capabilities) VALUES(?1,2048,4,?2,?3)",params![node,now(),json!({"memory_mib":2048,"slots":4,"vcpus":4,"placement_fencing_v1":true,"worker_protocol":2,"managed_restore_v1":true,"available_disk_bytes":1024,"disk_observed_unix_s":now(),"guest_ssh_v1":true,"compat":{"runtime":"same","cpu":"right","kernel_sha256":"same"}}).to_string()]).unwrap();
        }
        let transfer = c
            .begin_transfer(
                user,
                "retry",
                &Intent {
                    kind: Kind::Restore,
                    machine: "box".into(),
                    target: Some("target".into()),
                    capture: Some("capture".into()),
                    child: None,
                    snapshot: None,
                    memory_mib: 256,
                    cpus: 1,
                    disk_bytes: 4,
                    accept_cold: false,
                    explicit_start: false,
                },
            )
            .unwrap();
        Self {
            root,
            shared: Arc::new(Mutex::new(c)),
            user,
            transfer,
        }
    }
    fn restart(&mut self) {
        let placeholder =
            std::env::temp_dir().join(format!("ow-transfer-holder-{}", store::token().unwrap()));
        fs::create_dir(&placeholder).unwrap();
        let other = Catalog::open(&placeholder, "test", None).unwrap();
        let previous = std::mem::replace(&mut *self.shared.lock().unwrap(), other);
        drop(previous);
        let reopened = Catalog::open(&self.root, "test", None).unwrap();
        let other = std::mem::replace(&mut *self.shared.lock().unwrap(), reopened);
        drop(other);
        fs::remove_dir_all(placeholder).unwrap();
    }
    fn step(&self) -> Step {
        self.shared
            .lock()
            .unwrap()
            .transfer(self.transfer.id)
            .unwrap()
            .step
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[derive(Default)]
struct Peers {
    source: Option<Value>,
    target: Option<Value>,
    cached: bool,
    fault: Option<(String, bool)>,
    keys: BTreeMap<String, Value>,
    pulls: Vec<String>,
    starts: usize,
}
impl Peers {
    fn call(&mut self, shared: &Shared, node: &str, request: Value) -> Result<Value> {
        if shared.try_lock().is_err() {
            // A foreground connection may briefly inspect status concurrently with this RPC.
            let (tx, rx) = std::sync::mpsc::channel();
            let catalog = shared.clone();
            std::thread::spawn(move || {
                drop(catalog.lock().unwrap());
                let _ = tx.send(());
            });
            assert!(
                rx.recv_timeout(Duration::from_secs(1)).is_ok(),
                "RPC must not hold catalog mutex"
            );
        }
        let op = request["op"].as_str().unwrap();
        let fail = self.fault.as_ref().filter(|(name, _)| name == op).cloned();
        if fail == Some((op.into(), false)) {
            self.fault = None;
            anyhow::bail!("agent killed before effect")
        }
        if let Some(key) = request["operation_key"].as_str() {
            if let Some(previous) = self.keys.insert(key.into(), request.clone()) {
                assert_eq!(
                    previous, request,
                    "worker retry must preserve key and fingerprint"
                )
            }
        }
        let seen = if node == "source" {
            &mut self.source
        } else {
            &mut self.target
        };
        let result = match op {
            "inspect" => seen
                .clone()
                .unwrap_or(json!({"id":"physical","memory_mib":256,"vcpu_count":1,"disk_bytes":4,"state":"stopped","vmm_running":false})),
            "list" => json!(seen.iter().collect::<Vec<_>>()),
            "placement-register" => {
                *seen = Some(
                    json!({"id":"physical","memory_mib":256,"vcpu_count":1,"disk_bytes":4,"placement":request["placement"],"state":"stopped","vmm_running":false,"lifecycle":{"phase":"active"}}),
                );
                seen.clone().unwrap()
            }
            "export" => {
                *seen = Some(
                    json!({"id":"physical","memory_mib":256,"vcpu_count":1,"disk_bytes":4,"placement":request["placement"],"state":"exported","vmm_running":false,"lifecycle":{"phase":"exported","intent":{"transfer":request["transfer"],"capture":request["capture"]}}}),
                );
                seen.clone().unwrap()
            }
            "restore-policy" => json!("disabled"),
            "capture-cache" => {
                json!({"present":self.cached,"digest":"b".repeat(64),"manifest_sha256":"c".repeat(64)})
            }
            "capture-pull" => {
                self.cached = true;
                self.pulls.push(request["attempt"].as_str().unwrap().into());
                json!({"present":true,"digest":"b".repeat(64)})
            }
            "import-prepare" | "fork-prepare" => {
                assert!(self.cached);
                *seen = Some(
                    json!({"id":request["id"],"placement":request["placement"],"state":"prepared","vmm_running":false,"lifecycle":{"phase":"prepared","import":{"snapshot":request["snapshot"],"manifest_sha256":request["manifest_sha256"],"cold":request["cold"]}}}),
                );
                if request["op"] == "fork-prepare" {seen.as_mut().unwrap()["lifecycle"]["import"]["fork_source"]=request["fork_source"].clone();}
                seen.clone().unwrap()
            }
            "activate" => {
                let c = shared.lock().unwrap();
                let id=request["placement"]["transfer"].as_str().unwrap().strip_prefix("transfer-").unwrap().parse().unwrap();
                let t = c.transfer(id).unwrap();
                assert_eq!(t.step, Step::StartUncertain);
                let epoch: i64 =
                    c.db.query_row(
                        "SELECT placement_epoch FROM resources WHERE physical=?1",
                        [&t.machine],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(epoch, t.epoch);
                drop(c);
                self.starts += 1;
                *seen = Some(
                    json!({"id":request["id"],"placement":request["placement"],"state":"running","memory_mib":256,"vcpu_count":1,"disk_bytes":4,"vmm_running":true,"lifecycle":{"phase":"active"}}),
                );
                seen.clone().unwrap()
            }
            _ => panic!("unexpected {op}"),
        };
        if fail.is_some() {
            self.fault = None;
            anyhow::bail!("agent killed after effect")
        }
        Ok(result)
    }
}
fn run(f: &Fixture, peers: &RefCell<Peers>, at: i64) -> Result<bool> {
    tick(&f.shared, f.transfer.id, at, &|node, request| {
        peers.borrow_mut().call(&f.shared, node, request)
    })
}

#[test]
fn controller_restart_and_agent_death_before_and_after_every_effect() {
    for op in [
        "placement-register",
        "export",
        "capture-pull",
        "import-prepare",
        "activate",
    ] {
        for after in [false, true] {
            let mut f = Fixture::new();
            let peers = RefCell::new(Peers {
                fault: Some((op.into(), after)),
                ..Default::default()
            });
            let mut at = now();
            let mut failures = 0;
            for _ in 0..20 {
                match run(&f, &peers, at) {
                    Ok(false) => break,
                    Ok(true) => {}
                    Err(_) => {
                        failures += 1;
                        at += 7201;
                    }
                }
                f.restart();
            }
            assert_eq!(f.step(), Step::Succeeded, "{op} after={after}");
            assert_eq!(failures, 1, "{op} after={after}");
            assert_eq!(peers.borrow().starts, 1);
            let c = f.shared.lock().unwrap();
            assert_eq!(
                c.managed_placement("physical").unwrap().unwrap().node,
                "target"
            );
            assert!(
                c.operation_info(f.user, f.transfer.operation).unwrap()["transfer"]["step"]
                    == "succeeded"
            );
            assert_eq!(
                c.db.query_row("SELECT COUNT(*) FROM transfer_reservations", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
    }
}
#[test]
fn uncertain_pull_waits_for_old_lease_then_uses_a_new_attempt() {
    let f = Fixture::new();
    let peers = RefCell::new(Peers::default());
    let at = now();
    run(&f, &peers, at).unwrap();
    peers.borrow_mut().fault = Some(("capture-pull".into(), false));
    assert!(run(&f, &peers, at).is_err());
    let token: String = f
        .shared
        .lock()
        .unwrap()
        .db
        .query_row("SELECT token FROM leases", [], |r| r.get(0))
        .unwrap();
    assert!(
        run(&f, &peers, at + 1)
            .unwrap_err()
            .to_string()
            .contains("old lease")
    );
    assert!(peers.borrow().pulls.is_empty());
    run(&f, &peers, at + 7201).unwrap();
    assert_ne!(token, peers.borrow().pulls[0]);
    assert_eq!(
        f.shared
            .lock()
            .unwrap()
            .db
            .query_row("SELECT COUNT(*) FROM leases", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
}
#[test]
fn contradictions_never_commit_or_start_and_source_loss_never_authorizes() {
    let f = Fixture::new();
    let peers = RefCell::new(Peers::default());
    peers.borrow_mut().fault = Some(("inspect".into(), false));
    assert!(run(&f, &peers, now()).is_err());
    assert_eq!(f.step(), Step::Preparing);
    run(&f, &peers, now()).unwrap();
    run(&f, &peers, now()).unwrap();
    run(&f, &peers, now()).unwrap();
    peers.borrow_mut().target.as_mut().unwrap()["placement"]["epoch"] = json!(999);
    assert!(
        run(&f, &peers, now())
            .unwrap_err()
            .to_string()
            .contains("changed")
    );
    assert_eq!(f.step(), Step::Prepared);
    assert_eq!(peers.borrow().starts, 0);
    let c = f.shared.lock().unwrap();
    assert_eq!(
        c.db.query_row(
            "SELECT node FROM resources WHERE physical='physical'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "source"
    );
    assert_eq!(
        c.db.query_row("SELECT COUNT(*) FROM transfer_reservations", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
#[test]
fn lost_activation_with_nonrunning_active_copy_does_not_blindly_restart() {
    let f = Fixture::new();
    let peers = RefCell::new(Peers::default());
    while f.step() != Step::StartUncertain {
        run(&f, &peers, now()).unwrap();
    }
    let placement = authority(&f.transfer, "target");
    peers.borrow_mut().target = Some(
        json!({"id":"physical","placement":placement,"state":"stopped","vmm_running":false,"lifecycle":{"phase":"active"}}),
    );
    assert!(
        run(&f, &peers, now())
            .unwrap_err()
            .to_string()
            .contains("explicit reconciliation")
    );
    assert_eq!(peers.borrow().starts, 0);
}

fn stored(root: &Path) -> (store::DirectoryObjects, Artifact) {
    stored_capture(root, false)
}
fn stored_capture(root: &Path, ram: bool) -> (store::DirectoryObjects, Artifact) {
    let objects = store::DirectoryObjects(root.join("objects"));
    let disk = root.join("disk");
    fs::write(&disk, b"disk").unwrap();
    let mut files = BTreeMap::from([(
        "disk.ext4".into(),
        store::Source {
            sha256: store::hash_file(&disk).unwrap(),
            path: disk,
        },
    )]);
    if ram {
        for (name, bytes) in [
            ("state", b"state".as_slice()),
            ("memory", b"memory".as_slice()),
        ] {
            let path = root.join(name);
            fs::write(&path, bytes).unwrap();
            files.insert(
                name.into(),
                store::Source {
                    sha256: store::hash_file(&path).unwrap(),
                    path,
                },
            );
        }
    }
    let hashes: BTreeMap<_, _> = files
        .iter()
        .map(|(name, source)| (name, &source.sha256))
        .collect();
    let native=json!({"name":"sealed","workspace":"physical","managed":true,"memory_mib":256,"vcpu_count":1,"image":"alpine","runtime":common::RUNTIME,"cpu":"fixture","kernel_sha256":"a".repeat(64),"files":hashes}).to_string();
    let reference = store::push(
        &objects,
        &store::token().unwrap(),
        "capture",
        json!({"native_manifest":native}),
        &files,
        1,
    )
    .unwrap();
    (
        objects,
        Artifact {
            id: "capture".into(),
            machine: "physical".into(),
            store_url: "s3://bucket/root".into(),
            reference,
            snapshot: "sealed".into(),
            memory_mib: 256,
            cpus: 1,
            disk_bytes: 4,
        },
    )
}
#[test]
fn verified_pull_checks_pinned_manifest_lineage_files_and_existing_disk() {
    let f = Fixture::new();
    let (objects, a) = stored(&f.root);
    let mut wrong = a.clone();
    wrong.machine = "another".into();
    assert!(pull(&objects, &f.root, &wrong, &store::token().unwrap()).is_err());
    wrong = a.clone();
    wrong.reference.digest = "f".repeat(64);
    assert!(pull(&objects, &f.root, &wrong, &store::token().unwrap()).is_err());
    wrong = a.clone();
    wrong.snapshot = "../escape".into();
    assert!(pull(&objects, &f.root, &wrong, &store::token().unwrap()).is_err());
    assert!(pull(&objects, &f.root, &a, &store::token().unwrap()).unwrap()["present"] == true);
    assert!(cache(&f.root, &a).unwrap()["present"] == true);
    wrong = a.clone();
    wrong.disk_bytes = 5;
    assert!(cache(&f.root, &wrong).is_err());
    use std::os::unix::fs::PermissionsExt;
    let disk = f.root.join("snapshots/sealed/disk.ext4");
    fs::set_permissions(&disk, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&disk, b"evil").unwrap();
    assert!(cache(&f.root, &a).is_err());
    assert!(pull(&objects, &f.root, &a, &store::token().unwrap()).is_err());
    assert_eq!(fs::read(disk).unwrap(), b"evil");
}

struct ObservedObjects<'a> {
    inner: &'a dyn Objects,
    on_data: &'a dyn Fn() -> Result<()>,
}
impl Objects for ObservedObjects<'_> {
    fn put_once(&self, key: &str, source: &mut dyn std::io::Read) -> Result<()> {
        self.inner.put_once(key, source)
    }
    fn get(&self, key: &str) -> Result<Box<dyn std::io::Read>> {
        if !key.ends_with("/manifest.json") {
            (self.on_data)()?;
        }
        self.inner.get(key)
    }
}

#[test]
fn capture_pull_refuses_aggregate_staging_before_reading_objects() {
    let f = Fixture::new();
    let (objects, a) = stored(&f.root);
    let held = ow_core::cache::Reservation::new(&f.root, 2 * common::GIB, 0).unwrap();
    fs::create_dir_all(f.root.join("machines/live")).unwrap();
    let disk = fs::File::create(f.root.join("machines/live/disk.ext4")).unwrap();
    let logical = ow_core::cache::available(&f.root).unwrap() - 10 * common::GIB;
    disk.set_len(logical).unwrap();
    assert!(ow_core::cache::Reservation::new(&f.root, 4, 9 * common::GIB).is_err());
    let reads = std::cell::Cell::new(0);
    let observe = || {
        reads.set(reads.get() + 1);
        Ok(())
    };
    let observed = ObservedObjects {
        inner: &objects,
        on_data: &observe,
    };
    let error = pull(&observed, &f.root, &a, &store::token().unwrap()).unwrap_err();
    assert!(error.to_string().contains("staging headroom"), "{error:#}");
    assert_eq!(reads.get(), 0, "admission must precede object downloads");
    assert!(!f.root.join("snapshots/sealed").exists());
    assert_eq!(disk.metadata().unwrap().len(), logical);
    assert_eq!(
        ow_core::cache::staging_bytes(&f.root).unwrap(),
        2 * common::GIB
    );
    drop(held);
    assert_eq!(ow_core::cache::staging_bytes(&f.root).unwrap(), 0);
}

#[test]
fn capture_pull_reserves_disk_and_ram_until_install_or_error_cleanup() {
    for ram in [false, true] {
        for fail in [false, true] {
            let f = Fixture::new();
            let (objects, a) = stored_capture(&f.root, ram);
            let held = ow_core::cache::Reservation::new(&f.root, 1024, 0).unwrap();
            let bytes = if ram { 15 } else { 4 };
            let reads = std::cell::Cell::new(0);
            let observe = || {
                assert_eq!(
                    ow_core::cache::staging_bytes(&f.root).unwrap(),
                    1024 + bytes
                );
                reads.set(reads.get() + 1);
                ensure!(!fail, "interrupted capture object read");
                Ok(())
            };
            let observed = ObservedObjects {
                inner: &objects,
                on_data: &observe,
            };
            let attempt = store::token().unwrap();
            let result = pull(&observed, &f.root, &a, &attempt);
            assert_eq!(result.is_err(), fail);
            assert_eq!(
                reads.get(),
                if fail {
                    1
                } else if ram {
                    3
                } else {
                    1
                }
            );
            assert!(!f.root.join(format!("snapshots/.pull-{attempt}")).exists());
            assert_eq!(f.root.join("snapshots/sealed").exists(), !fail);
            assert_eq!(ow_core::cache::staging_bytes(&f.root).unwrap(), 1024);
            if !fail {
                assert_eq!(cache(&f.root, &a).unwrap()["present"], true);
                assert_eq!(
                    pull(&observed, &f.root, &a, &store::token().unwrap()).unwrap()["present"],
                    true
                );
            }
            drop(held);
            assert_eq!(ow_core::cache::staging_bytes(&f.root).unwrap(), 0);
        }
    }
}

#[test]
fn authorization_rejects_other_owner_capture_name_and_physical_id_before_rpc() {
    let f = Fixture::new();
    let attacker = Identity {
        issuer: "test".into(),
        subject: "attacker".into(),
        email: "attacker@example.test".into(),
    };
    for id in ["box", "physical", "owner@example.test/box"] {
        assert!(restore(f.shared.clone(),&attacker,&json!({"op":"restore","id":id,"artifact":"capture","node":"target","operation_key":"attack"})).is_err());
    }
    let identity = Identity {
        issuer: "test".into(),
        subject: "owner".into(),
        email: "owner@example.test".into(),
    };
    {
        let mut c = f.shared.lock().unwrap();
        let other = c.user(&attacker).unwrap();
        c.db.execute("UPDATE artifacts SET owner=?1 WHERE id='capture'", [other])
            .unwrap();
    }
    let error=restore(f.shared.clone(),&identity,&json!({"op":"restore","id":"box","artifact":"capture","node":"target","operation_key":"attack"})).unwrap_err();
    assert!(error.to_string().contains("committed artifact"));
}
#[test]
fn policy_revocation_during_wait_invalidates_commit_and_keeps_reservations() {
    let f = Fixture::new();
    let peers = RefCell::new(Peers::default());
    while f.step() != Step::Prepared {
        run(&f, &peers, now()).unwrap();
    }
    f.shared.lock().unwrap().db.execute("UPDATE resources SET ssh_policy_generation=ssh_policy_generation+1 WHERE physical='physical'",[]).unwrap();
    assert!(
        run(&f, &peers, now())
            .unwrap_err()
            .to_string()
            .contains("policy changed")
    );
    assert_eq!(f.step(), Step::Prepared);
    assert_eq!(peers.borrow().starts, 0);
    assert_eq!(
        f.shared
            .lock()
            .unwrap()
            .db
            .query_row("SELECT COUNT(*) FROM transfer_reservations", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
#[test]
fn returning_running_source_is_a_contradiction_not_a_winner() {
    let f = Fixture::new();
    let peers = RefCell::new(Peers::default());
    while f.step() != Step::Prepared {
        run(&f, &peers, now()).unwrap();
    }
    peers.borrow_mut().source.as_mut().unwrap()["vmm_running"] = json!(true);
    assert!(
        run(&f, &peers, now())
            .unwrap_err()
            .to_string()
            .contains("returning source")
    );
    assert_eq!(f.step(), Step::Prepared);
    assert_eq!(peers.borrow().starts, 0);
}
#[test]
fn incompatible_memory_and_import_failure_keep_the_artifact_and_cold_consent() {
    let f = Fixture::new();
    let peers = RefCell::new(Peers::default());
    while f.step() != Step::Importing {
        run(&f, &peers, now()).unwrap();
    }
    let error = tick(&f.shared, f.transfer.id, now(), &|node, request| {
        if request["op"] == "import-prepare" {
            assert_eq!(request["cold"], false);
            anyhow::bail!("incompatible RAM requires explicit cold consent")
        }
        peers.borrow_mut().call(&f.shared, node, request)
    })
    .unwrap_err();
    assert!(error.to_string().contains("cold consent"));
    assert_eq!(f.step(), Step::Importing);
    let c = f.shared.lock().unwrap();
    assert!(!intent(&c, &f.transfer).unwrap().accept_cold);
    assert_eq!(
        c.db.query_row("SELECT state FROM artifacts WHERE id='capture'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "stored"
    );
}

#[test]
fn a_blocked_pull_leaves_another_users_list_and_revocation_responsive() {
    let f = Fixture::new();
    let peers = RefCell::new(Peers::default());
    run(&f, &peers, now()).unwrap();
    let identity = Identity {
        issuer: "test".into(),
        subject: "guest".into(),
        email: "guest@example.test".into(),
    };
    let other = {
        let mut c = f.shared.lock().unwrap();
        let user = c.user(&identity).unwrap();
        c.db.execute(
            "INSERT INTO grants(physical,grantee,role) VALUES('physical',?1,'use')",
            [user],
        )
        .unwrap();
        user
    };
    let (entered, wait) = std::sync::mpsc::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let shared = f.shared.clone();
    let id = f.transfer.id;
    let worker = std::thread::spawn(move || {
        let peers = RefCell::new(Peers::default());
        tick(&shared, id, now(), &|node, request| {
            if request["op"] == "capture-pull" {
                entered.send(()).unwrap();
                blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            peers.borrow_mut().call(&shared, node, request)
        })
        .unwrap()
    });
    wait.recv_timeout(Duration::from_secs(5)).unwrap();
    let start = std::time::Instant::now();
    assert!(f.shared.lock().unwrap().state(other).unwrap()["shared"].is_array());
    let owner = Identity {
        issuer: "test".into(),
        subject: "owner".into(),
        email: "owner@example.test".into(),
    };
    crate::catalog::share_unlocked(
        &f.root,
        &owner,
        &json!({"op":"unshare","id":"box","with":"guest@example.test"}),
    )
    .unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
    release.send(()).unwrap();
    assert!(worker.join().unwrap());
    assert!(
        f.shared
            .lock()
            .unwrap()
            .machine(other, "owner@example.test/box", "exec")
            .is_err()
    );
}

#[test]
fn incompatible_ram_can_retry_with_new_cold_consent_without_unfencing_source() {
    let mut f = Fixture::new();
    let peers = RefCell::new(Peers::default());
    let at = now();
    run(&f, &peers, at).unwrap();
    let warm = |node: &str, request: Value| -> Result<Value> {
        if request["op"] == "capture-cache" {
            let mut result = peers.borrow_mut().call(&f.shared, node, request)?;
            result["ram"] = json!(true);
            result["compat"] =
                json!({"runtime":"other","cpu":"other","kernel_sha256":"d".repeat(64)});
            return Ok(result);
        }
        if request["op"] == "status" {
            return Ok(
                json!({"compat":{"runtime":"current","cpu":"current","kernel_sha256":"a".repeat(64)}}),
            );
        }
        peers.borrow_mut().call(&f.shared, node, request)
    };
    assert!(!tick(&f.shared, f.transfer.id, at, &warm).unwrap());
    assert_eq!(f.step(), Step::Failed);
    let next = {
        let c = f.shared.lock().unwrap();
        let mut input = intent(&c, &f.transfer).unwrap();
        input.accept_cold = true;
        assert!(
            c.begin_transfer(f.user, "retry", &input).is_err(),
            "changed consent cannot rewrite original operation"
        );
        assert_eq!(
            c.db.query_row("SELECT COUNT(*) FROM transfer_reservations", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        c.begin_transfer(f.user, "cold-retry", &input).unwrap()
    };
    let prior_export = peers.borrow().source.clone();
    f.transfer = next;
    while f.step() != Step::Succeeded {
        tick(&f.shared,f.transfer.id,at,&|node,request| {
            if request["op"]=="import-prepare" {assert_eq!(request["cold"],true);}
            if request["op"]=="activate" {
                // Peers' generic activation assertion names the first fixture operation.
                let seen=json!({"id":"physical","placement":request["placement"],"state":"running","memory_mib":256,"vcpu_count":1,"disk_bytes":4,"vmm_running":true,"lifecycle":{"phase":"active"}});
                peers.borrow_mut().target=Some(seen.clone());peers.borrow_mut().starts+=1;return Ok(seen)
            }
            peers.borrow_mut().call(&f.shared,node,request)
        }).unwrap();
    }
    assert_eq!(peers.borrow().source, prior_export);
    assert_eq!(peers.borrow().starts, 1);
}

#[test]
fn policy_install_is_epoch_bound_unstarted_and_preserves_the_public_host_identity() {
    use base64::Engine;
    use std::os::unix::net::UnixListener;
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("machines/physical")).unwrap();
    let listener = UnixListener::bind(f.root.join("control.sock")).unwrap();
    let placement = authority(&f.transfer, "target");
    let actual = placement.clone();
    let worker = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            assert_eq!(wire::line(&mut stream).unwrap()["op"], "inspect");
            wire::send(&mut stream,&json!({"ok":true,"result":{"id":"physical","placement":actual,"state":"prepared","vmm_running":false,"lifecycle":{"phase":"prepared"}}})).unwrap();
        }
    });
    let mut bytes = Vec::from(&11u32.to_be_bytes()[..]);
    bytes.extend(b"ssh-ed25519");
    bytes.extend(32u32.to_be_bytes());
    bytes.extend([7; 32]);
    let key = format!(
        "ssh-ed25519 {}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    );
    let policy = PublicSshPolicy::Enrolled(ow_core::ssh_guest::Policy {
        identity: "original".into(),
        generation: "generation".into(),
        host_key: key.clone(),
        keys: vec![key.clone()],
        ready: true,
    });
    let mut request =
        json!({"op":"restore-policy","id":"physical","placement":placement,"policy":policy});
    request["placement"]["epoch"] = json!(99);
    assert!(job(&f.root, &request).is_err());
    assert!(!ow_core::ssh_guest::path(&f.root, "physical").exists());
    request["placement"] = json!(placement);
    job(&f.root, &request).unwrap();
    let installed = ow_core::ssh_guest::load(&f.root, "physical").unwrap();
    assert_eq!(installed.host_key, key);
    assert_eq!(installed.identity, "original");
    assert!(!installed.ready);
    worker.join().unwrap();
}

fn parked_fixture(state: &str) -> (Fixture, RefCell<Peers>) {
    let mut f = Fixture::new();
    let initial = {
        let c = f.shared.lock().unwrap();
        c.db.execute("DELETE FROM transfer_reservations", [])
            .unwrap();
        c.db.execute("DELETE FROM transfer_locks", []).unwrap();
        c.db.execute("UPDATE transfers SET step='succeeded',target='source'", [])
            .unwrap();
        c.db.execute("UPDATE operations SET state='succeeded'", [])
            .unwrap();
        c.db.execute("UPDATE resources SET placement_managed=1,placement_epoch=?1,reserved_mib=256,reserved_cpus=1,metadata=json_set(metadata,'$.state',?2)",params![f.transfer.epoch,state]).unwrap();
        let auth = authority(&f.transfer, "source");
        let input = Intent {
            kind: Kind::Park,
            machine: "box".into(),
            target: None,
            capture: None,
            child: None,
            snapshot: None,
            memory_mib: 256,
            cpus: 1,
            disk_bytes: 4,
            accept_cold: false,
            explicit_start: false,
        };
        f.transfer = c.begin_transfer(f.user, "park-retry", &input).unwrap();
        json!({"id":"physical","memory_mib":256,"vcpu_count":1,"disk_bytes":4,"placement":auth,"state":state,"vmm_running":false,"lifecycle":{"phase":"active"}})
    };
    fs::write(f.root.join("store-url"), "s3://bucket/root").unwrap();
    (
        f,
        RefCell::new(Peers {
            source: Some(initial),
            ..Default::default()
        }),
    )
}
fn publication(request: &Value, ram: bool) -> Value {
    let a: Artifact = serde_json::from_value(request["artifact"].clone()).unwrap();
    let mut files = BTreeMap::new();
    let mut hashes = serde_json::Map::new();
    for file in if ram {
        vec!["disk.ext4", "memory", "state"]
    } else {
        vec!["disk.ext4"]
    } {
        let hash = store::sha256(file.as_bytes());
        hashes.insert(file.into(), json!(hash));
        files.insert(
            file.into(),
            store::StoredFile {
                size: if file == "disk.ext4" { 4 } else { 3 },
                allocated: 0,
                sha256: hash,
                objects: vec![store::Object {
                    key: format!(
                        "{}{file}.zst",
                        store::attempt_prefix(&a.reference.attempt).unwrap()
                    ),
                    codec: "zstd".into(),
                }],
            },
        );
    }
    let native = json!({"workspace":a.machine,"name":a.snapshot,"memory_mib":256,"vcpu_count":1,"managed":true,"runtime":"same","cpu":"right","kernel_sha256":"same","files":hashes});
    let manifest = store::Manifest {
        format: 1,
        kind: "capture".into(),
        attempt: a.reference.attempt.clone(),
        created_unix: 0,
        metadata: json!({"native_manifest":native.to_string()}),
        files,
    };
    json!({"present":true,"reference":{"attempt":a.reference.attempt,"digest":store::sha256(&manifest.bytes().unwrap())},"manifest":manifest})
}
fn park_call(
    f: &Fixture,
    peers: &RefCell<Peers>,
    ram: bool,
    node: &str,
    request: Value,
) -> Result<Value> {
    match request["op"].as_str().unwrap() {
        "capture-published" => Ok(json!({"present":false})),
        "capture-push" => Ok(publication(&request, ram)),
        _ => peers.borrow_mut().call(&f.shared, node, request),
    }
}
#[test]
fn park_preserves_user_state_and_only_releases_source_after_publication_and_commit() {
    for state in ["stopped", "hibernated"] {
        let (mut f, peers) = parked_fixture(state);
        let ram = state == "hibernated";
        for step in [
            Step::Preparing,
            Step::Exported,
            Step::Prepared,
            Step::Committed,
        ] {
            assert_eq!(f.step(), step);
            let node: String = f
                .shared
                .lock()
                .unwrap()
                .db
                .query_row("SELECT node FROM resources", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                node,
                if step == Step::Committed {
                    ""
                } else {
                    "source"
                }
            );
            tick(&f.shared, f.transfer.id, now(), &|node, request| {
                park_call(&f, &peers, ram, node, request)
            })
            .unwrap();
            f.restart();
        }
        assert_eq!(f.step(), Step::Succeeded);
        let c = f.shared.lock().unwrap();
        assert_eq!(c.db.query_row("SELECT node='' AND reserved_mib=0 AND reserved_cpus=0 AND json_extract(metadata,'$.state')=?1 AND current_capture IS NOT NULL FROM resources",[state],|r|r.get::<_,bool>(0)).unwrap(),true);
        assert_eq!(
            c.db.query_row(
                "SELECT state FROM artifacts WHERE id=?1",
                [format!("park-{}", f.transfer.id)],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "stored"
        );
        assert_eq!(
            parking::admission(
                &c,
                f.user,
                &json!({"op":"park","id":"box","operation_key":"park-retry"}),
                Kind::Park
            )
            .unwrap()
            .id,
            f.transfer.id
        );
        assert_eq!(peers.borrow().starts, 0);
    }
}
#[test]
fn upload_failure_or_lost_reply_keeps_source_and_retries_without_parking() {
    let (mut f, peers) = parked_fixture("hibernated");
    tick(&f.shared, f.transfer.id, now(), &|n, r| {
        park_call(&f, &peers, true, n, r)
    })
    .unwrap();
    let result = RefCell::new(None);
    assert!(
        tick(&f.shared, f.transfer.id, now(), &|n, r| {
            if r["op"] == "capture-push" {
                *result.borrow_mut() = Some(publication(&r, true));
                anyhow::bail!("upload response lost")
            }
            park_call(&f, &peers, true, n, r)
        })
        .is_err()
    );
    f.restart();
    assert_eq!(f.step(), Step::Exported);
    {
        let c = f.shared.lock().unwrap();
        assert!(
            c.db.query_row(
                "SELECT node='source' AND reserved_mib=256 FROM resources",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
        );
        assert_eq!(
            c.db.query_row(
                "SELECT state FROM artifacts WHERE id=?1",
                [format!("park-{}", f.transfer.id)],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "uploading"
        );
    }
    tick(&f.shared, f.transfer.id, now(), &|node, r| {
        if r["op"] == "inspect" {
            return peers.borrow_mut().call(&f.shared, node, r);
        }
        assert_eq!(r["op"], "capture-published");
        Ok(result.borrow().clone().unwrap())
    })
    .unwrap();
    assert_eq!(f.step(), Step::Prepared);
}
fn complete_park(f: &mut Fixture, peers: &RefCell<Peers>, ram: bool) {
    while f.step() != Step::Succeeded {
        tick(&f.shared, f.transfer.id, now(), &|n, r| {
            park_call(f, peers, ram, n, r)
        })
        .unwrap();
        f.restart();
    }
}
#[test]
fn connection_wake_coalesces_queues_through_drain_and_stopped_requires_start() {
    for (state, policy) in ["hibernated", "stopped"].into_iter().flat_map(|state| {
        ["runner-drain", "controller-drain", "inactive"].map(|policy| (state, policy))
    }) {
        let (mut f, peers) = parked_fixture(state);
        complete_park(&mut f, &peers, state == "hibernated");
        let c = f.shared.lock().unwrap();
        let request = json!({"op":"connect","id":"box","operation_key":"connect-one"});
        if state == "stopped" {
            assert!(parking::admission(&c, f.user, &request, Kind::Wake).is_err());
        }
        let request = json!({"op":if state=="stopped"{"start"}else{"connect"},"id":"box","operation_key":"connect-one"});
        let wake = parking::admission(&c, f.user, &request, Kind::Wake).unwrap();
        for caller in ["ssh", "exec", "terminal", "preview"] {
            assert_eq!(
                parking::admission(
                    &c,
                    f.user,
                    &json!({"op":"connect","id":"box","operation_key":caller}),
                    Kind::Wake
                )
                .unwrap()
                .id,
                wake.id
            );
        }
        assert_eq!(
            c.db.query_row("SELECT COUNT(*) FROM transfer_reservations", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        if policy == "runner-drain" {
            c.db.execute(
                "UPDATE nodes SET capabilities=json_set(capabilities,'$.drain',1)",
                [],
            )
            .unwrap();
        } else {
            c.db.execute("INSERT INTO node_placement_policy(node,active,controller_drain) SELECT id,?1,?2 FROM nodes",params![policy != "inactive", policy == "controller-drain"]).unwrap();
        }
        drop(c);
        assert!(
            !tick(&f.shared, wake.id, now(), &|_, _| panic!(
                "no RPC during queued wake"
            ))
            .unwrap()
        );
        f.restart();
        {
            let c = f.shared.lock().unwrap();
            assert!(c.operation_info(f.user, wake.operation).unwrap()["queued"].is_string());
            c.db.execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.drain',0) WHERE id='target'",[]).unwrap();
            c.db.execute(
                "UPDATE node_placement_policy SET active=1,controller_drain=0 WHERE node='target'",
                [],
            )
            .unwrap();
        }
        assert!(tick(&f.shared, wake.id, now(), &|_, _| panic!()).unwrap());
        {
            let c = f.shared.lock().unwrap();
            assert_eq!(
                c.transfer(wake.id).unwrap().target.as_deref(),
                Some("target")
            );
            assert_eq!(
                c.db.query_row("SELECT COUNT(*) FROM transfer_reservations", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
    }
}
#[test]
fn concurrent_connection_admission_has_one_wake_and_one_reservation() {
    let (mut f, peers) = parked_fixture("hibernated");
    complete_park(&mut f, &peers, true);
    let ids = std::thread::scope(|s| {
        let joins:Vec<_>=(0..16).map(|i|{let shared=f.shared.clone();let user=f.user;s.spawn(move||{let c=shared.lock().unwrap();parking::admission(&c,user,&json!({"op":"connect","id":"box","operation_key":format!("connection-{i}")}),Kind::Wake).unwrap().id})}).collect();
        joins
            .into_iter()
            .map(|j| j.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(ids.iter().all(|id| *id == ids[0]));
    tick(&f.shared, ids[0], now(), &|_, _| panic!()).unwrap();
    let c = f.shared.lock().unwrap();
    assert_eq!(
        c.db.query_row("SELECT COUNT(*) FROM transfer_reservations", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

fn wake_call(
    f: &Fixture,
    peers: &RefCell<Peers>,
    ram: bool,
    node: &str,
    request: Value,
) -> Result<Value> {
    match request["op"].as_str().unwrap() {
        "status" => Ok(json!({"compat":{"runtime":"same","cpu":"right","kernel_sha256":"same"}})),
        "capture-cache" => Ok(
            json!({"present":peers.borrow().cached,"digest":request["artifact"]["reference"]["digest"],"manifest_sha256":"c".repeat(64),"ram":ram,"compat":{"runtime":"same","cpu":"right","kernel_sha256":"same"}}),
        ),
        "capture-pull" => {
            let digest = request["artifact"]["reference"]["digest"].clone();
            let mut result = peers.borrow_mut().call(&f.shared, node, request)?;
            result["digest"] = digest;
            Ok(result)
        }
        "export-cleanup" => {
            let mut peers = peers.borrow_mut();
            let seen = if node == "source" {
                peers.source.as_mut().unwrap()
            } else {
                peers.target.as_mut().unwrap()
            };
            assert_eq!(seen["placement"], request["placement"]);
            assert_eq!(seen["lifecycle"]["intent"]["transfer"], request["transfer"]);
            seen["lifecycle"]["phase"] = json!("cleaned");
            Ok(seen.clone())
        }
        _ => peers.borrow_mut().call(&f.shared, node, request),
    }
}
fn admit_wake(f: &mut Fixture, explicit: bool) -> Transfer {
    let c = f.shared.lock().unwrap();
    let request =
        json!({"op":if explicit{"start"}else{"connect"},"id":"box","operation_key":"wake-retry"});
    parking::admission(&c, f.user, &request, Kind::Wake).unwrap()
}
#[test]
fn wake_restart_and_lost_import_or_activation_reply_have_one_execution() {
    for state in ["hibernated", "stopped"] {
        for op in ["capture-pull", "import-prepare", "activate"] {
            for after in [false, true] {
                let (mut f, peers) = parked_fixture(state);
                complete_park(&mut f, &peers, state == "hibernated");
                {
                    let c = f.shared.lock().unwrap();
                    c.db.execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.drain',1) WHERE id='source'",[]).unwrap();
                }
                let wake = admit_wake(&mut f, state == "stopped");
                peers.borrow_mut().fault = Some((op.into(), after));
                let mut at = now();
                let mut errors = 0;
                for _ in 0..20 {
                    match tick(&f.shared, wake.id, at, &|n, r| {
                        wake_call(&f, &peers, state == "hibernated", n, r)
                    }) {
                        Ok(false) => break,
                        Ok(true) => {}
                        Err(_) => {
                            errors += 1;
                            at += 7201;
                        }
                    }
                    f.restart();
                }
                let c = f.shared.lock().unwrap();
                assert_eq!(
                    c.transfer(wake.id).unwrap().step,
                    Step::Succeeded,
                    "{state} {op} {after}"
                );
                assert_eq!(errors, 1);
                assert_eq!(peers.borrow().starts, 1);
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
                assert!(
                    c.db.query_row(
                        "SELECT current_capture IS NOT NULL FROM resources",
                        [],
                        |r| r.get::<_, bool>(0)
                    )
                    .unwrap()
                );
            }
        }
    }
}
#[test]
fn wake_on_cached_source_cleans_only_old_export_and_keeps_capture() {
    let (mut f, peers) = parked_fixture("hibernated");
    complete_park(&mut f, &peers, true);
    peers.borrow_mut().cached = true;
    let wake = admit_wake(&mut f, false);
    for _ in 0..12 {
        if !tick(&f.shared, wake.id, now(), &|n, r| {
            wake_call(&f, &peers, true, n, r)
        })
        .unwrap()
        {
            break;
        }
    }
    let c = f.shared.lock().unwrap();
    assert_eq!(c.transfer(wake.id).unwrap().step, Step::Succeeded);
    assert_eq!(
        c.managed_placement("physical").unwrap().unwrap().node,
        "source"
    );
    assert_eq!(peers.borrow().starts, 1);
    assert!(peers.borrow().pulls.is_empty());
    assert_eq!(
        c.db.query_row(
            "SELECT state FROM artifacts WHERE id=?1",
            [format!("park-{}", f.transfer.id)],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "stored"
    );
}
#[test]
fn revocation_during_wake_pull_blocks_import_and_preserves_retry_roots() {
    let (mut f, peers) = parked_fixture("hibernated");
    complete_park(&mut f, &peers, true);
    let wake = admit_wake(&mut f, false);
    tick(&f.shared, wake.id, now(), &|_, _| panic!()).unwrap();
    let result = tick(&f.shared, wake.id, now(), &|n, r| {
        let revoke = r["op"] == "capture-pull";
        let result = wake_call(&f, &peers, true, n, r)?;
        if revoke {
            f.shared
                .lock()
                .unwrap()
                .db
                .execute(
                    "UPDATE resources SET ssh_policy_generation=ssh_policy_generation+1",
                    [],
                )
                .unwrap();
        }
        Ok(result)
    });
    assert!(result.is_err());
    f.restart();
    assert!(
        tick(&f.shared, wake.id, now(), &|_, _| panic!(
            "revoked operation must not dispatch"
        ))
        .is_err()
    );
    let c = f.shared.lock().unwrap();
    assert!(
        c.db.query_row(
            "SELECT node='' AND current_capture IS NOT NULL FROM resources",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
    assert_eq!(
        c.db.query_row("SELECT COUNT(*) FROM transfer_reservations", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(peers.borrow().starts, 0);
}
#[test]
fn cold_wake_retry_needs_new_explicit_consent_and_keeps_old_ram() {
    let (mut f, peers) = parked_fixture("hibernated");
    complete_park(&mut f, &peers, true);
    let wake = admit_wake(&mut f, false);
    tick(&f.shared, wake.id, now(), &|_, _| panic!()).unwrap();
    f.shared
        .lock()
        .unwrap()
        .db
        .execute(
            "UPDATE operations SET response=?1 WHERE id=?2",
            params![
                json!({"progress":{"bytes_completed":5,"bytes_total":10}}).to_string(),
                wake.operation
            ],
        )
        .unwrap();
    tick(&f.shared, wake.id, now(), &|n, r| {
        if r["op"] == "status" {
            return Ok(json!({"compat":"different"}));
        }
        wake_call(&f, &peers, true, n, r)
    })
    .unwrap();
    let retry = {
        let c = f.shared.lock().unwrap();
        assert_eq!(c.transfer(wake.id).unwrap().step, Step::Failed);
        assert_eq!(
            c.operation_info(f.user, wake.operation).unwrap()["progress"]["bytes_completed"],
            5
        );
        assert!(
            c.db.query_row(
                "SELECT node='' AND current_capture IS NOT NULL FROM resources",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
        );
        assert!(
            parking::admission(
                &c,
                f.user,
                &json!({"op":"wake","id":"box","operation_key":"wake-retry","accept_cold":true}),
                Kind::Wake
            )
            .is_err()
        );
        parking::admission(
            &c,
            f.user,
            &json!({"op":"wake","id":"box","operation_key":"cold-retry","accept_cold":true}),
            Kind::Wake,
        )
        .unwrap()
    };
    for _ in 0..12 {
        if !tick(&f.shared, retry.id, now(), &|n, r| {
            if r["op"] == "import-prepare" {
                assert_eq!(r["cold"], true);
            }
            wake_call(&f, &peers, true, n, r)
        })
        .unwrap()
        {
            break;
        }
    }
    let c = f.shared.lock().unwrap();
    assert_eq!(c.transfer(retry.id).unwrap().step, Step::Succeeded);
    assert_eq!(peers.borrow().starts, 1);
    assert_eq!(
        c.db.query_row(
            "SELECT state FROM artifacts WHERE id=?1",
            [format!("park-{}", f.transfer.id)],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "stored"
    );
}

#[test]
fn timed_out_connection_and_metadata_lookup_do_not_create_a_second_wake() {
    let (mut f, peers) = parked_fixture("hibernated");
    complete_park(&mut f, &peers, true);
    let wake = admit_wake(&mut f, false);
    let identity = Identity {
        issuer: "test".into(),
        subject: "owner".into(),
        email: "owner@example.test".into(),
    };
    for permission in ["ssh", "exec", "terminal", "preview"] {
        let error = parking::wait_awake(
            f.shared.clone(),
            &identity,
            "box",
            permission,
            &wake,
            Duration::ZERO,
        )
        .unwrap_err();
        let failure = error
            .downcast_ref::<crate::catalog::OperationFailure>()
            .unwrap();
        assert_eq!(failure.0["id"], wake.operation);
        assert_eq!(failure.0["transfer"]["step"], "preparing");
    }
    let mut c = f.shared.lock().unwrap();
    assert!(
        c.operation(f.user, json!({"op":"ssh-info","id":"box"}))
            .is_err()
    ); // unenrolled metadata, no worker contact
    assert_eq!(
        c.db.query_row(
            "SELECT COUNT(*) FROM transfers WHERE kind='wake'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        c.db.query_row("SELECT COUNT(*) FROM transfer_reservations", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let input = intent(&c, &c.transfer(1).unwrap()).unwrap();
    assert!(
        !serde_json::to_value(input)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("explicit_start")
    );
}

#[test]
fn park_denies_a_shared_admin_before_any_runner_contact() {
    let f = Fixture::new();
    let guest = Identity {
        issuer: "test".into(),
        subject: "guest".into(),
        email: "guest@example.test".into(),
    };
    {
        let mut c = f.shared.lock().unwrap();
        c.user(&guest).unwrap();
        c.operation(
            f.user,
            json!({"op":"share","id":"box","with":"guest@example.test","role":"admin"}),
        )
        .unwrap();
    }
    let error = park(
        f.shared.clone(),
        &guest,
        &json!({"op":"park","id":"owner@example.test/box","operation_key":"denied"}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("owner"));
}

#[path = "parking_review_tests.rs"]
mod review;

fn moving_fixture(kind: Kind) -> (Fixture, RefCell<Peers>) {
    let (mut f, peers) = parked_fixture("running");
    {
        let c = f.shared.lock().unwrap();
        c.advance_transfer(
            f.transfer.id,
            f.transfer.epoch,
            Step::Preparing,
            Step::Aborted,
            &Evidence::default(),
        )
        .unwrap();
        c.db.execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.move_fork_v1',json('true'),'$.worker_protocol',3)",[]).unwrap();
        let mut input = parking::shape(&c, "physical", kind, "box").unwrap();
        input.target = Some("target".into());
        if kind == Kind::Fork {
            input.child = Some(Child {
                id: "child-id".into(),
                name: "child".into(),
            });
        }
        f.transfer = c.begin_transfer(f.user, "move-or-fork", &input).unwrap();
    }
    peers.borrow_mut().source.as_mut().unwrap()["vmm_running"] = json!(true);
    (f, peers)
}
fn moving_call(f: &Fixture, peers: &RefCell<Peers>, node: &str, request: Value) -> Result<Value> {
    match request["op"].as_str().unwrap() {
        "capture-published" => Ok(json!({"present":false})),
        "capture-push" => Ok(publication(&request, true)),
        "snapshot" => Ok(json!({"name":request["name"],"workspace":request["id"]})),
        "import-discard" => {
            let mut p = peers.borrow_mut();
            let seen = if node == "source" {
                &mut p.source
            } else {
                &mut p.target
            };
            ensure!(
                seen.as_ref().is_none_or(|v| v["vmm_running"] != true),
                "cannot exclude running target"
            );
            *seen = Some(
                json!({"id":request["id"],"placement":request["placement"],"vmm_running":false,"lifecycle":{"phase":"discarded"}}),
            );
            Ok(seen.clone().unwrap())
        }
        _ => wake_call(f, peers, true, node, request),
    }
}
fn move_tick(f: &Fixture, peers: &RefCell<Peers>, at: i64) -> Result<bool> {
    tick(&f.shared, f.transfer.id, at, &|node, r| {
        moving_call(f, peers, node, r)
    })
}
#[test]
fn move_and_fork_restart_or_lost_reply_at_every_effect() {
    for kind in [Kind::Move, Kind::Fork] {
        let ops = if kind == Kind::Move {
            vec![
                "export",
                "capture-published",
                "capture-push",
                "capture-pull",
                "import-prepare",
                "restore-policy",
                "activate",
                "export-cleanup",
            ]
        } else {
            vec![
                "snapshot",
                "capture-published",
                "capture-push",
                "capture-pull",
                "fork-prepare",
                "restore-policy",
                "activate",
            ]
        };
        for op in ops {
            for after in [false, true] {
                let (mut f, peers) = moving_fixture(kind);
                let failed = std::cell::Cell::new(false);
                let mut at = now();
                for _ in 0..30 {
                    let r = tick(&f.shared, f.transfer.id, at, &|node, r| {
                        let hit = r["op"] == op && !failed.get();
                        if hit && !after {
                            failed.set(true);
                            anyhow::bail!("lost request");
                        }
                        let result = moving_call(&f, &peers, node, r)?;
                        if hit {
                            failed.set(true);
                            anyhow::bail!("lost reply");
                        }
                        Ok(result)
                    });
                    if r.is_err() {
                        at += 7201;
                    }
                    f.restart();
                    if f.step() == Step::Succeeded {
                        break;
                    }
                }
                assert!(failed.get(), "{kind:?}/{op}/{after}");
                assert_eq!(f.step(), Step::Succeeded, "{kind:?}/{op}/{after}");
                assert_eq!(peers.borrow().starts, 1);
                let c = f.shared.lock().unwrap();
                assert_eq!(
                    c.managed_placement(&f.transfer.machine)
                        .unwrap()
                        .unwrap()
                        .node,
                    "target"
                );
                if kind == Kind::Move {
                    assert_eq!(
                        peers.borrow().source.as_ref().unwrap()["lifecycle"]["phase"],
                        "cleaned"
                    );
                } else {
                    assert_eq!(peers.borrow().source.as_ref().unwrap()["vmm_running"], true);
                    assert_eq!(
                        c.db.query_row(
                            "SELECT owner FROM resources WHERE physical='child-id'",
                            [],
                            |r| r.get::<_, i64>(0)
                        )
                        .unwrap(),
                        f.user
                    );
                    assert_eq!(
                        c.db.query_row(
                            "SELECT ssh_policy FROM resources WHERE physical='child-id'",
                            [],
                            |r| r.get::<_, String>(0)
                        )
                        .unwrap(),
                        "\"disabled\""
                    );
                }
            }
        }
    }
}
#[test]
fn move_source_loss_never_fences_by_timeout_and_durable_export_continues() {
    for stop in [
        Step::Preparing,
        Step::Exported,
        Step::Importing,
        Step::StartUncertain,
    ] {
        let (mut f, peers) = moving_fixture(Kind::Move);
        while f.step() != stop {
            move_tick(&f, &peers, now()).unwrap();
        }
        // Every source request fails: only a durable publication can make progress.
        for _ in 0..10 {
            let _ = tick(&f.shared, f.transfer.id, now() + 100000, &|node, r| {
                ensure!(node != "source", "source offline");
                moving_call(&f, &peers, node, r)
            });
            f.restart();
        }
        if matches!(stop, Step::Preparing | Step::Exported) {
            assert_eq!(peers.borrow().starts, 0);
            assert_eq!(f.step(), stop);
        } else {
            assert_eq!(f.step(), Step::Succeeded);
            assert_eq!(peers.borrow().starts, 1);
        }
    }
}
#[test]
fn move_retarget_excludes_uncertain_import_and_allocates_fresh_epoch() {
    let (mut f, peers) = moving_fixture(Kind::Move);
    while f.step() != Step::Importing {
        move_tick(&f, &peers, now()).unwrap();
    }
    let _ = tick(&f.shared, f.transfer.id, now(), &|node, r| {
        let hit = r["op"] == "import-prepare";
        let v = moving_call(&f, &peers, node, r)?;
        ensure!(!hit, "lost import reply");
        Ok(v)
    });
    let old = f.shared.lock().unwrap().transfer(f.transfer.id).unwrap();
    f.shared.lock().unwrap().db.execute("UPDATE operations SET response=json_set(response,'$.intervention',json(?1)) WHERE id=?2",params![json!({"epoch":old.epoch,"abort":true,"node":null}).to_string(),old.operation]).unwrap();
    move_tick(&f, &peers, now()).unwrap();
    let next = f.shared.lock().unwrap().transfer(f.transfer.id).unwrap();
    assert!(next.epoch > old.epoch);
    assert_eq!(next.target.as_deref(), Some("source"));
    assert_eq!(
        peers.borrow().target.as_ref().unwrap()["lifecycle"]["phase"],
        "discarded"
    );
    peers.borrow_mut().cached = true;
    for _ in 0..10 {
        if !move_tick(&f, &peers, now()).unwrap() {
            break;
        }
        f.restart();
    }
    assert_eq!(f.step(), Step::Succeeded);
    assert_eq!(peers.borrow().starts, 1);
    assert_eq!(
        f.shared
            .lock()
            .unwrap()
            .managed_placement("physical")
            .unwrap()
            .unwrap()
            .node,
        "source"
    );
    // A start may have happened: further cancellation/retargeting is forbidden.
    let c = f.shared.lock().unwrap();
    let input = intent(&c, &next).unwrap();
    assert!(
        c.retarget_transfer(next.id, next.epoch, "target", &input, &json!({}))
            .is_err()
    );
}
#[test]
fn move_policy_change_or_contradictory_running_source_stops_activation() {
    for changed_policy in [true, false] {
        let (f, peers) = moving_fixture(Kind::Move);
        while f.step() != Step::Prepared {
            move_tick(&f, &peers, now()).unwrap();
        }
        if changed_policy {
            f.shared.lock().unwrap().db.execute("UPDATE resources SET ssh_policy_generation=ssh_policy_generation+1 WHERE physical='physical'",[]).unwrap();
        } else {
            peers.borrow_mut().source.as_mut().unwrap()["vmm_running"] = json!(true);
        }
        assert!(move_tick(&f, &peers, now()).is_err());
        assert_eq!(f.step(), Step::Prepared);
        assert_eq!(peers.borrow().starts, 0);
    }
}

#[test]
fn transfer_locks_block_conflicting_mutations_and_shared_fork_keeps_requester_ownership() {
    let (f, _) = moving_fixture(Kind::Move);
    let mut c = f.shared.lock().unwrap();
    for request in [
        json!({"op":"delete","id":"box"}),
        json!({"op":"resize","id":"box","memory_mib":512}),
        json!({"op":"snapshot","id":"box","name":"conflict"}),
        json!({"op":"ssh-keys","id":"box","keys":[]}),
    ] {
        let error = c.operation(f.user, request).unwrap_err();
        assert!(
            error.to_string().contains("transfer-aware routing"),
            "{error:#}"
        );
    }
    let t = c.transfer(f.transfer.id).unwrap();
    c.advance_transfer(t.id, t.epoch, t.step, Step::Aborted, &Evidence::default())
        .unwrap();
    let other = c
        .user(&Identity {
            issuer: "test".into(),
            subject: "forker".into(),
            email: "forker@example.test".into(),
        })
        .unwrap();
    c.db.execute(
        "INSERT INTO grants(physical,grantee,role) VALUES('physical',?1,'use')",
        [other],
    )
    .unwrap();
    let mut input = parking::shape(&c, "physical", Kind::Fork, "owner@example.test/box").unwrap();
    input.target = Some("target".into());
    input.child = Some(Child {
        id: "requester-child".into(),
        name: "mine".into(),
    });
    let fork = c.begin_transfer(other, "shared-fork", &input).unwrap();
    assert_eq!(
        c.db.query_row(
            "SELECT owner FROM resources WHERE physical='requester-child'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        other
    );
    assert_eq!(c.machine(other, "mine", "exec").unwrap(), fork.machine);
    input.kind = Kind::Move;
    input.child = None;
    assert!(c.begin_transfer(other, "shared-move", &input).is_err());
}

#[test]
fn queued_recovery_blocks_a_racing_commit_and_post_commit_timeout_never_retargets() {
    let (f, peers) = moving_fixture(Kind::Move);
    while f.step() != Step::Prepared {
        move_tick(&f, &peers, now()).unwrap();
    }
    let c = f.shared.lock().unwrap();
    let t = c.transfer(f.transfer.id).unwrap();
    c.db.execute(
        "UPDATE operations SET response=json_set(response,'$.intervention',json(?1)) WHERE id=?2",
        params![
            json!({"epoch":t.epoch,"abort":true}).to_string(),
            t.operation
        ],
    )
    .unwrap();
    assert!(c.commit_transfer(t.id, t.epoch).is_err());
    c.db.execute(
        "UPDATE operations SET response=json_remove(response,'$.intervention') WHERE id=?1",
        [t.operation],
    )
    .unwrap();
    c.commit_transfer(t.id, t.epoch).unwrap();
    let input = intent(&c, &t).unwrap();
    assert!(
        c.retarget_transfer(t.id, t.epoch, "source", &input, &json!({}))
            .is_err()
    );
    assert_eq!(c.transfer(t.id).unwrap().target.as_deref(), Some("target"));
}

#[test]
fn a_running_move_never_silently_cold_boots_an_incomplete_ram_export() {
    let (f, peers) = moving_fixture(Kind::Move);
    while f.step() != Step::Exported {
        move_tick(&f, &peers, now()).unwrap();
    }
    // The worker recovered an incomplete RAM seal as disk-only. This cannot imply cold consent.
    tick(
        &f.shared,
        f.transfer.id,
        now(),
        &|node, r| match r["op"].as_str().unwrap() {
            "capture-push" => Ok(publication(&r, false)),
            _ => moving_call(&f, &peers, node, r),
        },
    )
    .unwrap();
    let result = tick(
        &f.shared,
        f.transfer.id,
        now(),
        &|node, r| match r["op"].as_str().unwrap() {
            "capture-cache" => Ok(
                json!({"present":true,"digest":r["artifact"]["reference"]["digest"],"ram":false}),
            ),
            _ => moving_call(&f, &peers, node, r),
        },
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("cold recovery consent")
    );
    assert_eq!(f.step(), Step::Exported);
    assert_eq!(peers.borrow().starts, 0);
}
#[test]
fn offline_cleanup_does_not_hide_a_running_contradiction_when_the_source_returns() {
    let (f, peers) = moving_fixture(Kind::Move);
    while f.step() != Step::Started {
        move_tick(&f, &peers, now()).unwrap();
    }
    tick(&f.shared, f.transfer.id, now(), &|node, r| {
        ensure!(node != "source", "offline");
        moving_call(&f, &peers, node, r)
    })
    .unwrap();
    assert_eq!(f.step(), Step::Succeeded);
    peers.borrow_mut().source.as_mut().unwrap()["vmm_running"] = json!(true);
    assert!(move_tick(&f, &peers, now()).is_err());
    assert_eq!(f.step(), Step::Started);
    assert!(
        f.shared
            .lock()
            .unwrap()
            .managed_placement("physical")
            .unwrap()
            .is_none()
    );
}

#[test]
fn stopped_move_preserves_stopped_state_without_an_implicit_start() {
    let (f, peers) = moving_fixture(Kind::Move);
    peers.borrow_mut().source.as_mut().unwrap()["state"] = json!("stopped");
    peers.borrow_mut().source.as_mut().unwrap()["vmm_running"] = json!(false);
    for _ in 0..15 {
        let more = tick(&f.shared, f.transfer.id, now(), &|node, r| {
            if r["op"] == "activate" {
                assert_eq!(r["start"], false);
                let mut p = peers.borrow_mut();
                let seen = p.target.as_mut().unwrap();
                seen["state"] = json!("stopped");
                seen["vmm_running"] = json!(false);
                seen["lifecycle"]["phase"] = json!("active");
                return Ok(seen.clone());
            }
            moving_call(&f, &peers, node, r)
        })
        .unwrap();
        if !more {
            break;
        }
    }
    assert_eq!(f.step(), Step::Succeeded);
    assert_eq!(peers.borrow().starts, 0);
    assert_eq!(peers.borrow().target.as_ref().unwrap()["state"], "stopped");
}

#[test]
fn source_owner_can_exclude_and_abort_a_revoked_requesters_fork_without_restoring_the_grant() {
    let (f, peers) = moving_fixture(Kind::Fork);
    let mut c = f.shared.lock().unwrap();
    let requester = c
        .user(&Identity {
            issuer: "test".into(),
            subject: "requester".into(),
            email: "requester@example.test".into(),
        })
        .unwrap();
    let t = c.transfer(f.transfer.id).unwrap();
    c.db.execute("UPDATE operations SET owner=?1,request=json_set(request,'$.transfer.machine','owner@example.test/box') WHERE id=?2",params![requester,t.operation]).unwrap();
    c.db.execute(
        "UPDATE resources SET owner=?1 WHERE physical='child-id'",
        [requester],
    )
    .unwrap();
    // A source owner was authenticated by control(); the attestation is never accepted from public JSON.
    c.db.execute(
        "UPDATE operations SET response=json_set(response,'$.intervention',json(?1)) WHERE id=?2",
        params![
            json!({"epoch":t.epoch,"abort":true,"node":null,"owner_abort":true}).to_string(),
            t.operation
        ],
    )
    .unwrap();
    drop(c);
    move_tick(&f, &peers, now()).unwrap();
    assert_eq!(f.step(), Step::Aborted);
    let c = f.shared.lock().unwrap();
    assert!(
        c.machine(requester, "owner@example.test/box", "fork")
            .is_err()
    );
    assert_eq!(
        c.db.query_row(
            "SELECT COUNT(*) FROM resources WHERE physical='child-id'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(peers.borrow().source.as_ref().unwrap()["vmm_running"], true);
}

#[test]
fn shared_admin_move_is_denied_before_runner_contact() {
    let f = Fixture::new();
    let admin = Identity {
        issuer: "test".into(),
        subject: "admin".into(),
        email: "admin@example.test".into(),
    };
    {
        let mut c = f.shared.lock().unwrap();
        let user = c.user(&admin).unwrap();
        c.db.execute(
            "INSERT INTO grants(physical,grantee,role) VALUES('physical',?1,'admin')",
            [user],
        )
        .unwrap();
    }
    let error=relocate(f.shared.clone(),&admin,&json!({"op":"move","id":"owner@example.test/box","node":"missing-runner","operation_key":"denied"})).unwrap_err();
    assert!(
        error.to_string().contains("move requires the owner"),
        "{error:#}"
    );
}

#[test]
fn a_committed_fork_forward_recovers_as_the_child_owner_after_source_sharing_is_revoked() {
    let (f, peers) = moving_fixture(Kind::Fork);
    let requester = {
        let mut c = f.shared.lock().unwrap();
        let requester = c
            .user(&Identity {
                issuer: "test".into(),
                subject: "committed-forker".into(),
                email: "committed@example.test".into(),
            })
            .unwrap();
        c.db.execute(
            "INSERT INTO grants(physical,grantee,role) VALUES('physical',?1,'use')",
            [requester],
        )
        .unwrap();
        c.db.execute("UPDATE operations SET owner=?1,request=json_set(request,'$.transfer.machine','owner@example.test/box') WHERE id=?2",params![requester,f.transfer.operation]).unwrap();
        c.db.execute(
            "UPDATE resources SET owner=?1 WHERE physical='child-id'",
            [requester],
        )
        .unwrap();
        requester
    };
    while f.step() != Step::Committed {
        move_tick(&f, &peers, now()).unwrap();
    }
    f.shared
        .lock()
        .unwrap()
        .db
        .execute("DELETE FROM grants WHERE grantee=?1", [requester])
        .unwrap();
    for _ in 0..5 {
        if !move_tick(&f, &peers, now()).unwrap() {
            break;
        }
    }
    assert_eq!(f.step(), Step::Succeeded);
    let c = f.shared.lock().unwrap();
    assert!(
        c.machine(requester, "owner@example.test/box", "exec")
            .is_err()
    );
    assert!(c.managed_placement("child-id").unwrap().is_some());
    assert_eq!(peers.borrow().starts, 1);
}

#[test]
fn lost_retarget_reply_joins_the_fresh_epoch_instead_of_excluding_it_again() {
    let (f, _) = moving_fixture(Kind::Move);
    let original = f.transfer.epoch;
    {
        let c = f.shared.lock().unwrap();
        c.db.execute(
            "UPDATE transfers SET epoch=epoch+1 WHERE id=?1",
            [f.transfer.id],
        )
        .unwrap();
        c.db.execute("UPDATE operations SET response=json_set(COALESCE(response,'{}'),'$.last_intervention',json(?1)) WHERE id=?2",params![json!({"epoch":original,"node":"target","abort":false,"owner_abort":false}).to_string(),f.transfer.operation]).unwrap();
    }
    let result=control(f.shared.clone(),&Identity{issuer:"test".into(),subject:"owner".into(),email:"owner@example.test".into()},&json!({"op":"transfer-control","id":"box","transfer":f.transfer.id,"epoch":original,"node":"target"})).unwrap();
    assert_eq!(result["transfer"]["epoch"], original + 1);
    assert_eq!(f.shared.lock().unwrap().db.query_row("SELECT json_extract(response,'$.intervention') IS NULL FROM operations WHERE id=?1",[f.transfer.operation],|r|r.get::<_,bool>(0)).unwrap(),true);
}

#[test]
fn lost_export_reply_rechecks_public_policy_before_reusing_the_fence() {
    let (f, peers) = moving_fixture(Kind::Move);
    assert!(
        tick(&f.shared, f.transfer.id, now(), &|node, r| {
            ensure!(r["op"] != "restore-policy", "policy read reply lost");
            moving_call(&f, &peers, node, r)
        })
        .is_err()
    );
    assert_eq!(f.step(), Step::Preparing);
    assert_eq!(
        peers.borrow().source.as_ref().unwrap()["lifecycle"]["phase"],
        "exported"
    );
    let result = tick(&f.shared, f.transfer.id, now(), &|node, r| {
        if r["op"] == "restore-policy" {
            return Ok(json!({"enrolled":{"keys":[]}}));
        }
        moving_call(&f, &peers, node, r)
    });
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("SSH policy changed")
    );
    assert_eq!(f.step(), Step::Preparing);
    assert_eq!(peers.borrow().starts, 0);
}
// Regressions from interrupted admission, legacy key changes and resized parents.

#[test]
fn fork_recovers_legacy_admission_without_snapshot_binding() {
    let (mut f, peers) = moving_fixture(Kind::Fork);
    {
        let c = f.shared.lock().unwrap();
        // Exact durable state after begin_transfer commits, before relocate's next UPDATE.
        c.db.execute(
            "UPDATE operations SET response=NULL WHERE id=?1",
            [f.transfer.operation],
        )
        .unwrap();
    }
    f.restart();
    move_tick(&f, &peers, now()).unwrap();
    assert_eq!(f.step(), Step::Exported);
    for _ in 0..8 {
        f.restart();
        if !move_tick(&f, &peers, now()).unwrap() {
            break;
        }
    }
    assert_eq!(f.step(), Step::Succeeded);
    assert_eq!(peers.borrow().starts, 1);
    let c = f.shared.lock().unwrap();
    assert_eq!(
        c.db.query_row(
            "SELECT COUNT(*) FROM transfer_locks WHERE transfer=?1",
            [f.transfer.id],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn park_after_legacy_ssh_change() {
    legacy_ssh_admission("park", false);
}
#[test]
fn restore_after_legacy_ssh_change() {
    legacy_ssh_admission("restore", false);
}
#[test]
fn park_rejects_a_key_change_during_policy_read() {
    legacy_ssh_admission("park", true);
}
#[test]
fn restore_rejects_a_key_change_during_policy_read() {
    legacy_ssh_admission("restore", true);
}
fn legacy_ssh_admission(op: &str, concurrent_change: bool) {
    use std::os::unix::net::UnixListener;
    let (f, _) = parked_fixture("stopped");
    {
        let c = f.shared.lock().unwrap();
        let t = c.transfer(f.transfer.id).unwrap();
        c.advance_transfer(t.id, t.epoch, t.step, Step::Aborted, &Evidence::default())
            .unwrap();
        c.db.execute(
            "UPDATE resources SET placement_managed=0,placement_epoch=0 WHERE physical='physical'",
            [],
        )
        .unwrap();
        c.db.execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.park_wake_v1',json('true')) WHERE id='source'",[]).unwrap();
    }
    let route = f.root.join("nodes/source");
    fs::create_dir_all(&route).unwrap();
    let listener = UnixListener::bind(route.join("control.sock")).unwrap();
    let shared = f.shared.clone();
    let worker = std::thread::spawn(move || {
        loop {
            let (mut socket, _) = listener.accept().unwrap();
            let r = wire::line(&mut socket).unwrap();
            let result = match r["op"].as_str().unwrap() {
                "list" => {
                    json!([{"id":"physical","state":"stopped","memory_mib":256,"vcpu_count":1,"disk_bytes":4}])
                }
                "snapshots" => json!([]),
                "ssh-keys" => json!({"updated":true}),
                "restore-policy" => json!("disabled"),
                op => panic!("unexpected fixture operation {op}"),
            };
            if r["op"] == "restore-policy" && concurrent_change {
                shared.lock().unwrap().db.execute("UPDATE resources SET ssh_policy=NULL,ssh_policy_generation=ssh_policy_generation+1 WHERE physical='physical'",[]).unwrap();
            }
            wire::send(&mut socket, &json!({"ok":true,"result":result})).unwrap();
            if r["op"] == "restore-policy" {
                break;
            }
        }
    });
    let identity = Identity {
        issuer: "test".into(),
        subject: "owner".into(),
        email: "owner@example.test".into(),
    };
    {
        let mut c = f.shared.lock().unwrap();
        c.operation(
            f.user,
            json!({"op":"ssh-keys","id":"box","keys":[],"operation_key":"replace-keys"}),
        )
        .unwrap();
        let generation: i64 =
            c.db.query_row(
                "SELECT ssh_policy_generation FROM resources WHERE physical='physical'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        println!("actual catalog ssh-keys succeeded; policy generation={generation}");
    }
    let request = json!({"op":op,"id":"box","operation_key":"after-key-change","node":"target","artifact":"capture"});
    let result = if op == "park" {
        park(f.shared.clone(), &identity, &request)
    } else {
        restore(f.shared.clone(), &identity, &request)
    };
    worker.join().unwrap();
    let c = f.shared.lock().unwrap();
    if concurrent_change {
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SSH policy changed")
        );
        assert!(c.db.query_row("SELECT ssh_policy IS NULL AND ssh_policy_generation=2 FROM resources WHERE physical='physical'",[],|r|r.get::<_,bool>(0)).unwrap());
    } else {
        result.unwrap();
        assert!(c.db.query_row("SELECT ssh_policy IS NOT NULL AND ssh_policy_generation=2 FROM resources WHERE physical='physical'",[],|r|r.get::<_,bool>(0)).unwrap());
    }
}

#[test]
fn targeted_fork_preserves_the_older_snapshot_shape_after_parent_resize() {
    let (mut f, peers) = moving_fixture(Kind::Fork);
    let next_id = {
        let c = f.shared.lock().unwrap();
        let t = c.transfer(f.transfer.id).unwrap();
        c.advance_transfer(t.id, t.epoch, t.step, Step::Aborted, &Evidence::default())
            .unwrap();
        c.db.execute("UPDATE resources SET metadata=json_set(metadata,'$.memory_mib',512,'$.vcpu_count',2,'$.disk_bytes',8) WHERE physical='physical'",[]).unwrap();
        c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node) VALUES(?1,'snapshot','saved','snapshot-id',?2,'source')",params![f.user,json!({"name":"snapshot-id","workspace":"physical","memory_mib":256,"vcpu_count":1,"disk_bytes":4,"state":"ready"}).to_string()]).unwrap();
        c.db.query_row("SELECT MAX(id)+1 FROM transfers", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
    };
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(f.root.join(format!("transfer-{next_id}.lock")))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let identity = Identity {
        issuer: "test".into(),
        subject: "owner".into(),
        email: "owner@example.test".into(),
    };
    let result=relocate(f.shared.clone(),&identity,&json!({"op":"fork","id":"box","child":"snapshot-child","snapshot":"saved","node":"target","operation_key":"snapshot-shape"})).unwrap();
    println!("targeted fork admission: {result}");
    f.transfer = f.shared.lock().unwrap().transfer(next_id).unwrap();
    f.restart();
    {
        let c = f.shared.lock().unwrap();
        let shape: (i64, i64, i64) =
            c.db.query_row(
                "SELECT memory_mib,cpus,disk_bytes FROM transfer_reservations WHERE transfer=?1",
                [next_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(shape, (256, 1, 4));
        assert_eq!(
            intent(&c, &f.transfer).unwrap().snapshot.as_deref(),
            Some("saved")
        );
        assert_eq!(
            c.db.query_row(
                "SELECT json_extract(response,'$.fork_snapshot') FROM operations WHERE id=?1",
                [f.transfer.operation],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "snapshot-id"
        );
    }
    let joined=relocate(f.shared.clone(),&identity,&json!({"op":"fork","id":"box","child":"snapshot-child","snapshot":"saved","node":"target","operation_key":"snapshot-shape"})).unwrap();
    assert_eq!(joined["transfer"]["id"], next_id);
    let source = f.root.join("old-snapshot");
    let snap = source.join("snapshots/snapshot-id");
    fs::create_dir_all(&snap).unwrap();
    fs::write(snap.join("disk.ext4"), b"disk").unwrap();
    let hash = store::hash_file(&snap.join("disk.ext4")).unwrap();
    ow_core::runtime::atomic_json(&snap.join("manifest.json"),&json!({"name":"snapshot-id","workspace":"physical","managed":false,"memory_mib":256,"vcpu_count":1,"image":"alpine","runtime":common::RUNTIME,"cpu":"fixture","kernel_sha256":"a".repeat(64),"files":{"disk.ext4":hash}})).unwrap();
    move_tick(&f, &peers, now()).unwrap();
    let objects = store::DirectoryObjects(f.root.join("snapshot-objects"));
    let published = tick(
        &f.shared,
        next_id,
        now(),
        &|node, r| match r["op"].as_str().unwrap() {
            "capture-published" => Ok(json!({"present":false})),
            "capture-push" => capture_upload::publish(
                &objects,
                &source,
                &serde_json::from_value(r["artifact"].clone())?,
            ),
            _ => moving_call(&f, &peers, node, r),
        },
    );
    println!("publishing saved 256 MiB snapshot of parent resized to 512 MiB: {published:?}");
    assert!(
        published.is_ok(),
        "fork must use the named snapshot resource shape"
    );
}

#[test]
fn fork_admission_binds_snapshot_before_restart_and_replays_a_lost_reply() {
    let (mut f, peers) = moving_fixture(Kind::Fork);
    f.restart();
    let expected = format!("{}-fork", name(&f.transfer));
    let c = f.shared.lock().unwrap();
    let binding: String =
        c.db.query_row(
            "SELECT json_extract(response,'$.fork_snapshot') FROM operations WHERE id=?1",
            [f.transfer.operation],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(binding, expected);
    drop(c);
    let requests = RefCell::new(Vec::new());
    assert!(
        tick(&f.shared, f.transfer.id, now(), &|node, r| {
            if r["op"] == "snapshot" {
                requests.borrow_mut().push(r.clone());
                moving_call(&f, &peers, node, r)?;
                anyhow::bail!("lost snapshot reply");
            }
            moving_call(&f, &peers, node, r)
        })
        .is_err()
    );
    assert_eq!(f.step(), Step::Preparing);
    f.restart();
    tick(&f.shared, f.transfer.id, now(), &|node, r| {
        if r["op"] == "snapshot" {
            requests.borrow_mut().push(r.clone());
        }
        moving_call(&f, &peers, node, r)
    })
    .unwrap();
    assert_eq!(f.step(), Step::Exported);
    let seen = requests.borrow();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0]["name"], expected);
}
