use super::*;
use crate::catalog::Identity;
use std::path::PathBuf;
struct Fixture {
    root: PathBuf,
    catalog: Option<Catalog>,
}
impl Fixture {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("ow-drain-{}", common::nonce().unwrap()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let c = Catalog::open(&root, "test", None).unwrap();
        for n in ["base", "test-runner", "elsewhere"] {
            c.db.execute("INSERT INTO nodes(id,memory_mib,slots,heartbeat,session,capabilities) VALUES(?1,2048,4,unixepoch(),'connection',?2)",params![n,json!({"memory_mib":2048,"slots":4,"vcpus":4,"drain_inventory_v1":true,"drain":false}).to_string()]).unwrap();
        }
        Self {
            root,
            catalog: Some(c),
        }
    }
    fn c(&self) -> &Catalog {
        self.catalog.as_ref().unwrap()
    }
    fn begin(&self) {
        command(&self.root, "test-runner", "allow", None, None).unwrap();
        command(&self.root, "test-runner", "begin", None, None).unwrap();
        self.c()
            .db
            .execute(
                "UPDATE runner_drains SET state='draining' WHERE node='test-runner'",
                [],
            )
            .unwrap();
    }
    fn inventory(&self) -> Value {
        let seal: String = self
            .c()
            .db
            .query_row(
                "SELECT seal FROM runner_drains WHERE node='test-runner'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        json!({"seal":seal,"worker":{"pid":123,"start":"456","boot":"test-boot"},"running":0,"machines":[],"captures":[],"unmanaged":[]})
    }
    fn cert(&self) -> Value {
        certify(
            &self.c().db,
            "test-runner",
            1,
            revision(&self.c().db).unwrap(),
            "connection",
            &self.inventory(),
        )
        .unwrap()
    }
    fn reopen(&mut self) {
        drop(self.catalog.take());
        self.catalog = Some(Catalog::open(&self.root, "test", None).unwrap());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        drop(self.catalog.take());
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn install_protects_all_runners_and_base_cannot_be_allowed() {
    let f = Fixture::new();
    assert_eq!(
        status(&f.c().db, "test-runner").unwrap()["scale_in_eligible"],
        false
    );
    assert!(command(&f.root, "test-runner", "begin", None, None).is_err());
    assert!(command(&f.root, "base", "allow", None, None).is_err());
    assert!(!blocked(&f.c().db, "test-runner").unwrap());
}
#[test]
fn soft_cancel_preserves_both_independent_drain_authorities() {
    let f = Fixture::new();
    f.c().db.execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.drain',json('true')) WHERE id='test-runner'",[]).unwrap();
    f.c()
        .db
        .execute(
            "INSERT INTO node_placement_policy(node,controller_drain) VALUES('test-runner',1)",
            [],
        )
        .unwrap();
    command(&f.root, "test-runner", "allow", None, None).unwrap();
    command(&f.root, "test-runner", "begin", None, None).unwrap();
    assert!(blocked(&f.c().db, "test-runner").unwrap());
    command(&f.root, "test-runner", "cancel", None, None).unwrap();
    assert!(!blocked(&f.c().db, "test-runner").unwrap());
    let inv = nodes::inventory(&f.root).unwrap();
    let n = inv
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == "test-runner")
        .unwrap();
    assert_eq!(n["capabilities"]["drain"], true);
    assert_eq!(n["capabilities"]["controller_drain"], true);
    f.begin();
    assert!(command(&f.root, "test-runner", "cancel", None, None).is_err());
}
#[test]
fn stale_revision_session_worker_and_drain_generation_reject_certificates() {
    let f = Fixture::new();
    f.begin();
    let v = revision(&f.c().db).unwrap();
    f.c()
        .db
        .execute("UPDATE drain_revision SET version=version+1", [])
        .unwrap();
    assert!(certify(&f.c().db, "test-runner", 1, v, "connection", &f.inventory()).is_err());
    let cert = f.cert();
    let mut changed = f.inventory();
    changed["worker"]["start"] = json!("restarted");
    assert!(consume(&f.c().db, "test-runner", &cert, &changed, None).is_err());
    f.c()
        .db
        .execute(
            "UPDATE nodes SET session='new-generation' WHERE id='test-runner'",
            [],
        )
        .unwrap();
    assert!(
        consume(
            &f.c().db,
            "test-runner",
            &cert,
            &f.inventory(),
            Some(&json!({}))
        )
        .is_err()
    );
    f.c()
        .db
        .execute(
            "UPDATE runner_drains SET generation=generation+1 WHERE node='test-runner'",
            [],
        )
        .unwrap();
    assert!(consume(&f.c().db, "test-runner", &cert, &f.inventory(), None).is_err());
}
#[test]
fn every_unsafe_worker_inventory_retains_runner() {
    let f = Fixture::new();
    f.begin();
    for (key, value) in [
        ("running", json!(1)),
        ("running", Value::Null),
        ("unmanaged", json!(["unknown-disk"])),
        (
            "machines",
            json!([{"id":"box","vmm_running":false,"lifecycle":{"phase":"exported"}}]),
        ),
        (
            "machines",
            json!([{"id":"box","lifecycle":{"phase":"cleaned"}}]),
        ),
        (
            "captures",
            json!([{"name":"unstored","native":{"workspace":"box"}}]),
        ),
    ] {
        let mut inv = f.inventory();
        inv[key] = value;
        assert!(
            certify(
                &f.c().db,
                "test-runner",
                1,
                revision(&f.c().db).unwrap(),
                "connection",
                &inv
            )
            .is_err(),
            "{key}"
        );
    }
    assert_eq!(
        status(&f.c().db, "test-runner").unwrap()["state"],
        "draining"
    );
}
#[test]
fn catalog_reservations_and_unresolved_effects_prevent_empty() {
    let mut f = Fixture::new();
    let user = f
        .catalog
        .as_mut()
        .unwrap()
        .user(&Identity {
            issuer: "test".into(),
            subject: "owner".into(),
            email: "owner@example.test".into(),
        })
        .unwrap();
    f.c().db.execute("INSERT INTO resources(owner,kind,name,physical,node,metadata,reserved_mib) VALUES(?1,'machine','box','physical','test-runner','{\"state\":\"stopped\"}',256)",[user]).unwrap();
    f.begin();
    assert!(catalog_empty(&f.c().db, "test-runner").is_err());
    f.c().db.execute("DELETE FROM resources", []).unwrap();
    f.c().db.execute("INSERT INTO operations(owner,op,resource,node,state) VALUES(?1,'create','box','test-runner','uncertain')",[user]).unwrap();
    assert!(catalog_empty(&f.c().db, "test-runner").is_err());
    f.c()
        .db
        .execute("UPDATE operations SET state='failed'", [])
        .unwrap();
    assert!(catalog_empty(&f.c().db, "test-runner").is_ok());
    f.c()
        .db
        .execute(
            "INSERT INTO node_usage(node,extra_slots) VALUES('test-runner',1)",
            [],
        )
        .unwrap();
    assert!(catalog_empty(&f.c().db, "test-runner").is_err());
}
#[test]
fn placement_after_empty_check_is_fenced_and_remove_requires_retention() {
    let mut f = Fixture::new();
    let user = f
        .catalog
        .as_mut()
        .unwrap()
        .user(&Identity {
            issuer: "test".into(),
            subject: "owner".into(),
            email: "owner@example.test".into(),
        })
        .unwrap();
    f.begin();
    let cert = f.cert();
    assert!(f.c().db.execute("INSERT INTO resources(owner,kind,name,physical,node,metadata) VALUES(?1,'machine','box','physical','test-runner','{}')",[user]).is_err());
    assert!(f.c().db.execute("INSERT INTO operations(owner,op,resource,node,state) VALUES(?1,'create','box','test-runner','pending')",[user]).is_err());
    assert!(consume(&f.c().db, "test-runner", &cert, &f.inventory(), None).is_err());
    consume(
        &f.c().db,
        "test-runner",
        &cert,
        &f.inventory(),
        Some(&json!({"node":"test-runner","dev_disk_retained":true,"proof":"fixture retained directory"})),
    )
    .unwrap();
    consume(&f.c().db, "test-runner", &cert, &f.inventory(), None).unwrap();
    assert_eq!(
        status(&f.c().db, "test-runner").unwrap()["state"],
        "removed"
    );
    assert!(
        !nodes::inventory(&f.root)
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == "base")
            .unwrap()["revoked"]
            .as_bool()
            .unwrap()
    );
    assert!(consume(&f.c().db, "test-runner", &cert, &f.inventory(), None).is_err());
}
#[test]
fn controller_restart_keeps_drain_and_disk_and_refuses_old_reader() {
    let mut f = Fixture::new();
    f.begin();
    fs::write(f.root.join("retained-dev-disk"), b"private fixture").unwrap();
    let inv = f.inventory();
    f.reopen();
    assert!(blocked(&f.c().db, "test-runner").unwrap());
    assert_eq!(
        fs::read(f.root.join("retained-dev-disk")).unwrap(),
        b"private fixture"
    );
    assert_eq!(f.inventory(), inv);
    assert!(nodes::readable_by(&f.c().db, 5).is_err());
}
#[test]
fn job_seal_excludes_concurrent_effects_and_survives_restart() {
    let f = Fixture::new();
    let effect = job_guard(&f.root, &json!({"op":"start"})).unwrap();
    assert!(job_guard(&f.root, &json!({"op":"drain-inventory"})).is_err());
    drop(effect);
    let final_check = job_guard(&f.root, &json!({"op":"drain-inventory"})).unwrap();
    assert!(job_guard(&f.root, &json!({"op":"start"})).is_err());
    fs::write(f.root.join("drain-seal.json"), b"{\"seal\":\"fixture\"}").unwrap();
    drop(final_check);
    for op in [
        "create",
        "start",
        "exec",
        "terminal",
        "capture-push",
        "import-activate",
        "ssh",
    ] {
        assert!(job_guard(&f.root, &json!({"op":op})).is_err(), "{op}");
    }
    assert!(job_guard(&f.root, &json!({"op":"list"})).is_ok());
}
#[test]
fn soft_drain_never_stops_or_migrates_running_or_unknown_workloads() {
    let mut f = Fixture::new();
    command(&f.root, "test-runner", "allow", None, None).unwrap();
    command(&f.root, "test-runner", "begin", None, None).unwrap();
    let c = f.catalog.take().unwrap();
    let shared = Arc::new(Mutex::new(c));
    for inv in [
        json!({"running":1,"unmanaged":[],"machines":[{"id":"dev","state":"running"}]}),
        json!({"running":0,"unmanaged":[],"machines":[{"id":"unknown","state":"failed"}]}),
    ] {
        assert!(
            pass(&shared, "test-runner", &|_, r| {
                assert_eq!(r["op"], "drain-inventory");
                Ok(inv.clone())
            })
            .is_err()
        );
        assert_eq!(
            status(&shared.lock().unwrap().db, "test-runner").unwrap()["state"],
            "soft-draining"
        );
    }
    f.catalog = Some(Arc::try_unwrap(shared).ok().unwrap().into_inner().unwrap());
}

#[test]
fn bounded_agent_inventory_reports_raw_disks_and_persistent_worker_seal() {
    use std::os::unix::net::UnixListener;
    let f = Fixture::new();
    let root = f.root.join("runner");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("machines")).unwrap();
    fs::create_dir(root.join("snapshots")).unwrap();
    let listener = UnixListener::bind(root.join("control.sock")).unwrap();
    let worker = std::thread::spawn(move || {
        for _ in 0..8 {
            let (mut s, _) = listener.accept().unwrap();
            let r = wire::line(&mut s).unwrap();
            let result = match r["op"].as_str().unwrap() {
                "status" => json!({"worker_pid":std::process::id(),"running":0}),
                "list" => json!([]),
                "quiesce" => json!({"closing":true}),
                _ => panic!("unexpected worker effect"),
            };
            wire::send(&mut s, &json!({"ok":true,"result":result})).unwrap();
        }
    });
    fs::create_dir(root.join("machines/orphan")).unwrap();
    fs::write(root.join("machines/orphan/disk.ext4"), b"private disk").unwrap();
    fs::write(root.join("my-notes"), b"unmanaged data").unwrap();
    let inv = agent_inventory(&root, &json!({"op":"drain-inventory"})).unwrap();
    assert!(
        inv["unmanaged"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "machines/orphan")
    );
    assert!(
        inv["unmanaged"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "my-notes")
    );
    fs::remove_dir_all(root.join("machines/orphan")).unwrap();
    fs::remove_file(root.join("my-notes")).unwrap();
    // The recovery uploader's chunk index is a cache, not guest data.
    fs::create_dir(root.join("recovery-index")).unwrap();
    let inv = agent_inventory(
        &root,
        &json!({"op":"drain-inventory","seal":"fixture","close":true}),
    )
    .unwrap();
    assert_eq!(inv["seal"], "fixture");
    assert_eq!(inv["unmanaged"], json!([]));
    let again = agent_inventory(&root, &json!({"op":"drain-inventory","seal":"fixture"})).unwrap();
    assert_eq!(again, inv);
    assert!(job_guard(&root, &json!({"op":"start"})).is_err());
    worker.join().unwrap();
}

#[test]
fn recovery_journal_metadata_allows_empty_certification_but_pending_or_unknown_data_blocks() {
    use ow_core::recovery::{Point, Recovery};
    use std::os::unix::net::UnixListener;
    let mut f = Fixture::new();
    f.begin();
    let root = f.root.join("runner");
    fs::create_dir_all(root.join("recovery")).unwrap();
    fs::create_dir(root.join("snapshots")).unwrap();
    let empty = json!(Recovery {
        store: "a".repeat(64),
        ..Recovery::default()
    });
    let durable_name = "sleep-recovery-durable";
    let capture = root.join("snapshots").join(durable_name);
    fs::create_dir(&capture).unwrap();
    fs::write(capture.join("disk.ext4"), b"disk").unwrap();
    fs::write(capture.join("chunks.json"), b"{}").unwrap();
    fs::write(capture.join("manifest.json"),json!({"name":durable_name,"workspace":"physical","files":{"disk.ext4":store::sha256(b"disk")}}).to_string()).unwrap();
    let mut durable = empty.clone();
    durable["durable"] = json!(Point {
        snapshot: durable_name.into(),
        captured_at: 1,
        source: None
    });
    durable["uploaded_at"] = json!(2);
    let mut pending = durable.clone();
    pending["pending"] = durable["durable"].clone();
    pending["failures"] = json!(1);
    pending["error"] = json!("Upload failed; retry scheduled.");
    let mut unknown = empty.clone();
    unknown["unrecognized_payload"] = json!("must retain");
    let mut invalid = durable.clone();
    invalid["durable"]["snapshot"] = json!("../outside");
    let mut missing = durable.clone();
    missing["durable"]["snapshot"] = json!("sleep-recovery-missing");
    let cases = vec![
        ("physical.json", empty.to_string(), true),
        ("physical.json", durable.to_string(), true),
        ("physical.json", pending.to_string(), false),
        ("physical.json", unknown.to_string(), false),
        ("orphan.json", empty.to_string(), false),
        ("physical.json", invalid.to_string(), false),
        ("physical.json", missing.to_string(), false),
        ("physical.json", "not-json".into(), false),
        ("physical.json", " ".repeat(65537), false),
    ];
    let listener = UnixListener::bind(root.join("control.sock")).unwrap();
    let calls = cases.len() * 2 + 8;
    let worker = std::thread::spawn(move || {
        for _ in 0..calls {
            let (mut s, _) = listener.accept().unwrap();
            let r = wire::line(&mut s).unwrap();
            let result = match r["op"].as_str().unwrap() {
                "status" => json!({"worker_pid":std::process::id(),"running":0}),
                "list" => {
                    json!([{"id":"physical","state":"exported-cleaned","vmm_running":false,"lifecycle":{"phase":"cleaned"}}])
                }
                "quiesce" => json!({"closing":true}),
                _ => panic!("unexpected worker mutation"),
            };
            wire::send(&mut s, &json!({"ok":true,"result":result})).unwrap();
        }
    });
    for (name, raw, allowed) in &cases {
        let path = root.join("recovery").join(name);
        fs::write(&path, raw).unwrap();
        let result = agent_inventory(&root, &json!({"op":"drain-inventory"}));
        let recognized = result.as_ref().is_ok_and(|v| v["unmanaged"] == json!([]));
        assert_eq!(
            recognized, *allowed,
            "journal {name}: {raw:.100}; result {result:?}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), *raw);
        fs::remove_file(path).unwrap();
    }
    // Only test-owned bytes are removed here; production inventory never deletes journals/captures.
    fs::remove_dir_all(capture).unwrap();
    fs::write(root.join("recovery/physical.json"), empty.to_string()).unwrap();
    let shared = Arc::new(Mutex::new(f.catalog.take().unwrap()));
    pass(&shared, "test-runner", &|_, r| agent_inventory(&root, &r)).unwrap();
    assert_eq!(
        status(&shared.lock().unwrap().db, "test-runner").unwrap()["state"],
        "empty"
    );
    assert_eq!(
        fs::read_to_string(root.join("recovery/physical.json")).unwrap(),
        empty.to_string()
    );
    let certificate: Value = serde_json::from_str(
        &shared
            .lock()
            .unwrap()
            .db
            .query_row(
                "SELECT certificate FROM runner_drains WHERE node='test-runner'",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap(),
    )
    .unwrap();
    let mut changed = empty;
    changed["attempted_at"] = json!(1);
    fs::write(root.join("recovery/physical.json"), changed.to_string()).unwrap();
    let inventory = agent_inventory(&root, &json!({"op":"drain-inventory"})).unwrap();
    worker.join().unwrap();
    assert_eq!(inventory["unmanaged"], json!([]));
    assert!(
        consume(
            &shared.lock().unwrap().db,
            "test-runner",
            &certificate,
            &inventory,
            None
        )
        .unwrap_err()
        .to_string()
        .contains("stale runner")
    );
    f.catalog = Some(Arc::try_unwrap(shared).ok().unwrap().into_inner().unwrap());
}

#[test]
#[cfg(target_arch = "x86_64")] // Production Runtime fingerprints an x86 Firecracker host.
fn stopped_runtime_recovery_prepare_journal_reaches_drain_parking() {
    use std::{cell::RefCell, os::unix::net::UnixListener};
    let mut f = Fixture::new();
    let root = f.root.join("runner");
    let assets = f.root.join("assets");
    fs::create_dir_all(root.join("machines/physical")).unwrap();
    fs::create_dir_all(assets.join("downloads")).unwrap();
    fs::write(assets.join("downloads/vmlinux-6.1.186"), b"fixture kernel").unwrap();
    fs::write(root.join("machines/physical/disk.ext4"), b"dev disk").unwrap();
    fs::write(root.join("state.json"),json!({"next_index":2,"workspaces":{"physical":{"id":"physical","index":1,"state":"stopped","memory_mib":256,"vcpu_count":1,"source":null,"hibernation":null}}}).to_string()).unwrap();
    let mut runtime = ow_core::runtime::Runtime::new(root.clone(), assets).unwrap();
    assert!(
        runtime
            .request(&json!({"op":"recovery-prepare","id":"physical","store":"a".repeat(64)}))
            .unwrap()
            .is_null()
    );
    let journal = fs::read(root.join("recovery/physical.json")).unwrap();
    let saved: ow_core::recovery::Recovery = serde_json::from_slice(&journal).unwrap();
    assert!(saved.pending.is_none() && saved.durable.is_none());
    let owner = f
        .catalog
        .as_mut()
        .unwrap()
        .user(&Identity {
            issuer: "test".into(),
            subject: "owner".into(),
            email: "owner@example.test".into(),
        })
        .unwrap();
    f.c().db.execute("INSERT INTO resources(owner,kind,name,physical,node,metadata,ssh_policy) VALUES(?1,'machine','box','physical','test-runner','{\"state\":\"stopped\"}','\"disabled\"')",[owner]).unwrap();
    command(&f.root, "test-runner", "allow", None, None).unwrap();
    command(&f.root, "test-runner", "begin", None, None).unwrap();
    let listener = UnixListener::bind(root.join("control.sock")).unwrap();
    let worker = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut s, _) = listener.accept().unwrap();
            let r = wire::line(&mut s).unwrap();
            wire::send(&mut s, &wire::response(runtime.request(&r))).unwrap();
        }
    });
    let seen = RefCell::new(Vec::new());
    let shared = Arc::new(Mutex::new(f.catalog.take().unwrap()));
    let error = pass(&shared, "test-runner", &|_, r| {
        seen.borrow_mut().push(r["op"].as_str().unwrap().to_owned());
        if r["op"] == "drain-inventory" {
            agent_inventory(&root, &r)
        } else {
            anyhow::bail!("parking reached; stop before fixture mutation")
        }
    })
    .unwrap_err();
    worker.join().unwrap();
    assert!(error.to_string().contains("parking reached"), "{error:#}");
    assert_eq!(*seen.borrow(), ["drain-inventory", "restore-policy"]);
    assert_eq!(
        status(&shared.lock().unwrap().db, "test-runner").unwrap()["state"],
        "draining"
    );
    assert_eq!(
        fs::read(root.join("recovery/physical.json")).unwrap(),
        journal
    );
    assert_eq!(
        fs::read(root.join("machines/physical/disk.ext4")).unwrap(),
        b"dev disk"
    );
    f.catalog = Some(Arc::try_unwrap(shared).ok().unwrap().into_inner().unwrap());
}
