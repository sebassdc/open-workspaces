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
            c.db.execute("INSERT INTO nodes(id,memory_mib,slots,heartbeat,capabilities) VALUES(?1,2048,4,?2,?3)",params![node,now(),json!({"memory_mib":2048,"slots":4,"vcpus":4,"placement_fencing_v1":true,"worker_protocol":2,"managed_restore_v1":true,"available_disk_bytes":1024,"disk_observed_unix_s":now(),"guest_ssh_v1":true}).to_string()]).unwrap();
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
            "import-prepare" => {
                assert!(self.cached);
                *seen = Some(
                    json!({"id":"physical","placement":request["placement"],"state":"prepared","vmm_running":false,"lifecycle":{"phase":"prepared","import":{"snapshot":request["snapshot"],"manifest_sha256":request["manifest_sha256"],"cold":request["cold"]}}}),
                );
                seen.clone().unwrap()
            }
            "activate" => {
                let c = shared.lock().unwrap();
                let id=request["placement"]["transfer"].as_str().unwrap().strip_prefix("transfer-").unwrap().parse().unwrap();
                let t = c.transfer(id).unwrap();
                assert_eq!(t.step, Step::StartUncertain);
                let epoch: i64 =
                    c.db.query_row(
                        "SELECT placement_epoch FROM resources WHERE physical='physical'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(epoch, t.epoch);
                drop(c);
                self.starts += 1;
                *seen = Some(
                    json!({"id":"physical","placement":request["placement"],"state":"running","memory_mib":256,"vcpu_count":1,"disk_bytes":4,"vmm_running":true,"lifecycle":{"phase":"active"}}),
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
    let objects = store::DirectoryObjects(root.join("objects"));
    let disk = root.join("disk");
    fs::write(&disk, b"disk").unwrap();
    let hash = store::hash_file(&disk).unwrap();
    let native=json!({"name":"sealed","workspace":"physical","managed":true,"memory_mib":256,"vcpu_count":1,"image":"alpine","runtime":common::RUNTIME,"cpu":"fixture","kernel_sha256":"a".repeat(64),"files":{"disk.ext4":hash}}).to_string();
    let reference = store::push(
        &objects,
        &store::token().unwrap(),
        "capture",
        json!({"native_manifest":native}),
        &BTreeMap::from([(
            "disk.ext4".into(),
            store::Source {
                path: disk,
                sha256: hash,
            },
        )]),
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
    let native = json!({"workspace":a.machine,"name":a.snapshot,"memory_mib":256,"vcpu_count":1,"managed":true,"files":hashes});
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
    for state in ["hibernated", "stopped"] {
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
        c.db.execute(
            "UPDATE nodes SET capabilities=json_set(capabilities,'$.drain',1)",
            [],
        )
        .unwrap();
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
        "status" => Ok(json!({"compat":"compatible"})),
        "capture-cache" => Ok(
            json!({"present":peers.borrow().cached,"digest":request["artifact"]["reference"]["digest"],"manifest_sha256":"c".repeat(64),"ram":ram,"compat":"compatible"}),
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
