use super::super::tests::{capabilities, pool};
use super::*;
use std::os::unix::fs::PermissionsExt;
struct Fixture {
    c: Catalog,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("ow-sc-{}", common::nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            c: Catalog::open(&root, "test", None).unwrap(),
        }
    }
    fn write(&self, name: &str, value: Value) {
        let temp = self
            .c
            .root
            .join(format!("input-{}", common::nonce().unwrap()));
        nodes::write_private(&temp, &value).unwrap();
        std::fs::rename(temp, self.c.root.join(name)).unwrap();
    }
    fn configure(&self) {
        let mut p = pool();
        p.floor = 1;
        self.write("scaler.json", json!({"mode":"shadow","pools":[p]}));
        self.write("limits.json",json!({"pool":{"hours":24,"max_estimated_cents":10000,"elastic":{"cents":10000,"instances":10,"disk_bytes":4096*common::GIB}}}));
        self.write("scaler-observations.json",json!({"hours":24,"observed_at":100,"complete":true,"retention_ok":true,"outcome_resolved":true,"instances":[],"disks":[],"receipts":{}}));
    }
    fn node(&self, id: &str, at: u64) {
        let mut cap = capabilities();
        cap["memory_mib"] = json!(8192);
        cap["vcpus"] = json!(8);
        cap["slots"] = json!(4);
        cap["available_disk_bytes"] = json!(256 * common::GIB);
        cap["max_disk_gib"] = json!(256);
        cap["workspace_count"] = json!(1);
        cap["drain_inventory_v1"] = json!(true);
        cap["disk_observed_unix_s"] = json!(at);
        self.c.db.execute("INSERT INTO nodes(id,heartbeat,session,capabilities,memory_mib,slots) VALUES(?1,?2,'test-session',?3,8192,4)",params![id,at as i64,cap.to_string()]).unwrap();
        self.c
            .db
            .execute(
                "INSERT INTO runner_drains(node,eligible) VALUES(?1,1)",
                [id],
            )
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.c.root);
    }
}
fn observations(instances: Value, disks: Value) -> Observations {
    serde_json::from_value(json!({"observed_at":100,"hours":24,"complete":true,"retention_ok":true,"outcome_resolved":true,"instances":instances,"disks":disks,"receipts":{}})).unwrap()
}
fn instance(id: &str, state: &str, node: Option<&str>) -> Value {
    json!({"id":id,"pool":"default","node":node,"state":state,"action":null,"hourly_cents":10})
}
#[test]
fn real_catalog_tick_restart_status_and_reader_floor_preserve_shadow_only_state() {
    let mut f = Fixture::new();
    f.configure();
    let before: i64 =
        f.c.db
            .query_row("SELECT COUNT(*) FROM operations", [], |r| r.get(0))
            .unwrap();
    let root = f.c.root.clone();
    let first = attempt(&root, &mut f.c.db, 100).unwrap();
    assert_eq!(first["pools"][0]["decision"], "launch");
    assert!(
        first["pools"][0]["live_blocked"]
            .as_str()
            .unwrap()
            .contains("nothing is dispatched")
    );
    assert_eq!(
        attempt(&root, &mut f.c.db, 100).unwrap()["pools"][0]["state"],
        first["pools"][0]["state"]
    );
    assert!(nodes::readable_by(&f.c.db, 6).is_err());
    let backup_count = std::fs::read_dir(&root).unwrap().count();
    let held = state(&f.c.db, "default").unwrap();
    assert_eq!(held.action.as_ref().unwrap().phase, Phase::ShadowLaunch);
    let db = nodes::db(&root).unwrap();
    assert_eq!(state(&db, "default").unwrap(), held);
    drop(db);
    assert_eq!(
        before,
        f.c.db
            .query_row::<i64, _, _>("SELECT COUNT(*) FROM operations", [], |r| r.get(0))
            .unwrap()
    );
    assert_eq!(backup_count, std::fs::read_dir(&root).unwrap().count());
    assert_eq!(command(&root, false).unwrap()["config_present"], true);
    f.write("scaler.json", json!({"mode":"other","pools":[pool()]}));
    assert!(attempt(&root, &mut f.c.db, 101).is_err());
    assert!(
        status(&f.c.db).unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("invalid private scaler file")
    );
    assert_eq!(state(&f.c.db, "default").unwrap(), held);
}
#[test]
fn live_mode_is_refused_without_activation_evidence_and_never_promotes_shadow_intent() {
    let mut f = Fixture::new();
    f.configure();
    let root = f.c.root.clone();
    let mut p = pool();
    p.floor = 1;
    f.write("scaler.json", json!({"mode":"live","pools":[p]}));
    let error = |f: &Fixture| {
        status(&f.c.db).unwrap()["error"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };
    assert!(attempt(&root, &mut f.c.db, 100).is_err());
    assert!(
        error(&f).contains("scaler-activation.json"),
        "{}",
        error(&f)
    );
    f.write(
        "scaler-activation.json",
        json!({"recovery_drill":"private drill record"}),
    );
    assert!(attempt(&root, &mut f.c.db, 100).is_err());
    assert!(error(&f).contains("pool check"), "{}", error(&f));
    f.write(
        "scaler-provider-check.json",
        json!({"pools":{"default": 101}}),
    );
    assert!(
        attempt(&root, &mut f.c.db, 100).is_err(),
        "a check from the future refuses"
    );
    f.write(
        "scaler-provider-check.json",
        json!({"pools":{"other": 100}}),
    );
    assert!(
        attempt(&root, &mut f.c.db, 100).is_err(),
        "every configured pool needs its own check"
    );
    assert!(
        state(&f.c.db, "default").unwrap().action.is_none(),
        "refused ticks record nothing"
    );
    f.write(
        "scaler-provider-check.json",
        json!({"pools":{"default": 90}}),
    );
    let live = attempt(&root, &mut f.c.db, 100).unwrap();
    assert_eq!(live["mode"], "live");
    let action = state(&f.c.db, "default").unwrap().action.unwrap();
    assert_eq!(
        action.phase,
        Phase::Launching,
        "a newly recorded launch is effectful"
    );
    let now = ow_core::recovery::now();
    f.write(
        "scaler-provider-check.json",
        json!({"pools":{"default": now}}),
    );
    let current = command(&root, false).unwrap();
    assert_eq!(current["mode"], "live");
    assert!(current["error"].is_null());
    assert_eq!(current["pools"][0]["state"]["action"], json!(action));
    // Revoking or expiring evidence must take effect before another tick runs.
    for check in [json!({"pools":{}}), json!({"pools":{"default":now-86401}})] {
        f.write("scaler-provider-check.json", check);
        assert!(
            command(&root, false).unwrap()["error"]
                .as_str()
                .unwrap()
                .contains("no passing provider check")
        );
    }
    std::fs::remove_file(root.join("scaler-activation.json")).unwrap();
    assert!(
        command(&root, false).unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("scaler-activation.json")
    );
    f.write("scaler.json", json!({"mode":"shadow","pools":[pool()]}));
    let shadow = command(&root, false).unwrap();
    assert_eq!(shadow["mode"], "shadow");
    assert_eq!(shadow["pools"][0]["state"]["action"], json!(action));
    assert!(
        live["pools"][0]["live_blocked"]
            .as_str()
            .unwrap()
            .is_empty()
    );
}
#[test]
fn delayed_aws_hook_round_trips_through_the_real_planner_and_adapter() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    let script = r#"
import contextlib, io, json, sys
from unittest.mock import patch
from test_pool import Executor, NODE_A, VOL_A, rp
f = Executor()
f.setUp()
try:
    f.as_controller()
    f.aws.nodes = [{'id': NODE_A, 'online': True, 'capabilities': {'vcpus': 4}}]
    f.aws.drain[NODE_A] = 'removed'
    f.aws.launch('i-0a', VOL_A)
    f.aws.group['DesiredCapacity'] = 1
    f.aws.termination_state = 'Terminating'
    for turn in range(3):
        f.aws.scaler = json.loads(sys.stdin.readline())
        assert f.aws.scaler['pools'][0]['execution_allowed'] is True
        if turn == 1:
            f.aws.member('i-0a')['LifecycleState'] = 'Terminating:Wait'
        with contextlib.redirect_stdout(io.StringIO()), patch.object(rp.h, 'chown'):
            rp.sync(['dev'])
        assert f.aws.group['DesiredCapacity'] == 0
        if turn == 0:
            assert not any('complete-lifecycle-action' in c for c in f.aws.calls)
        print(json.dumps(f.observed()), flush=True)
    completed = [c for c in f.aws.calls if 'complete-lifecycle-action' in c]
    assert len(completed) == 1 and completed[0][completed[0].index('--instance-id') + 1] == 'i-0a'
    assert f.aws.group['Instances'] == []
finally:
    f.tearDown()
"#;
    let mut child = Command::new("python3")
        .args(["-c", script])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/aws/host"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let db = Connection::open_in_memory().unwrap();
    initialize(&db).unwrap();
    let mut p = pool();
    p.name = "dev".into();
    p.idle_seconds = 0;
    let mut i = super::super::tests::input();
    let mut runner =
        super::super::tests::runner("dev-0aaaaaaaaaaaaaaaa", p.presets[0].capacity, i.now);
    runner.candidate.pool = p.name.clone();
    i.runners = vec![runner];
    i.ready_instances = 1;
    i.pool_instances = 1;
    let mut plan = record_mode(&db, &p, &i, super::super::tests::caps(), true).unwrap();
    for turn in 0..3 {
        let mut value = json!(plan);
        value["stale"] = json!(false);
        writeln!(
            stdin,
            "{}",
            json!({"mode":"live","error":null,"pools":[value]})
        )
        .unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        assert!(stdout.read_line(&mut line).unwrap() > 0);
        let observed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(observed["outcome_resolved"], true);
        i.now += 1;
        i.inventory_fresh = observed["complete"] == true;
        i.retention_ok = observed["retention_ok"] == true;
        i.outcome_resolved = observed["outcome_resolved"] == true;
        i.receipt = Some(serde_json::from_value(observed["receipts"]["dev"].clone()).unwrap());
        plan = record_mode(&db, &p, &i, super::super::tests::caps(), true).unwrap();
        assert_eq!(plan.execution_allowed, turn < 2);
    }
    assert!(plan.state.action.is_none());
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[test]
fn stale_inventory_revokes_execution_without_discarding_a_live_launch() {
    let mut f = Fixture::new();
    f.configure();
    let root = f.c.root.clone();
    let now = ow_core::recovery::now();
    let mut p = pool();
    p.floor = 1;
    f.write("scaler.json", json!({"mode":"live","pools":[p]}));
    f.write(
        "scaler-activation.json",
        json!({"recovery_drill":"accepted fixture"}),
    );
    f.write(
        "scaler-provider-check.json",
        json!({"pools":{"default":now-2}}),
    );
    let mut observed = json!({"hours":24,"observed_at":now-1,"complete":true,"retention_ok":true,"outcome_resolved":true,"instances":[],"disks":[],"receipts":{}});
    f.write("scaler-observations.json", observed.clone());
    let launched = attempt(&root, &mut f.c.db, now - 1).unwrap();
    assert_eq!(launched["pools"][0]["execution_allowed"], true);
    observed["observed_at"] = json!(now - 100);
    f.write("scaler-observations.json", observed);
    attempt(&root, &mut f.c.db, now).unwrap();
    let current = command(&root, false).unwrap();
    assert_eq!(current["mode"], "live");
    assert!(current["error"].is_null());
    let plan = &current["pools"][0];
    assert_eq!(plan["stale"], false);
    assert_eq!(
        plan["reason"],
        "provider inventory missing, incomplete or stale; actions disabled"
    );
    assert_eq!(
        plan["state"]["action"],
        launched["pools"][0]["state"]["action"]
    );
    assert_eq!(plan["execution_allowed"], false);
}

#[test]
fn retained_pending_orphaned_instances_and_all_disks_remain_billable_liabilities() {
    let f = Fixture::new();
    f.node("base", 100);
    let o = observations(
        json!([
            instance("one", "ready", Some("base")),
            instance("two", "retained", None),
            instance("three", "pending", None),
            instance("four", "orphaned", None)
        ]),
        json!([{"id":"retained-disk","pool":"default","logical_bytes":500*common::GIB,"hourly_cents":3},{"id":"orphaned-disk","pool":"other","logical_bytes":700*common::GIB,"hourly_cents":4}]),
    );
    let i = inputs_on(&f.c.db, &[pool()], &o, 100, 24, &[])
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        i.liabilities,
        limits::Liability {
            cents: (40 + 7) * 24,
            instances: 4,
            disk_bytes: 1200 * common::GIB
        }
    );
    assert_eq!(i.pool_instances, 4);
    assert_eq!(i.ready_instances, 1);
    assert_eq!(i.runners.len(), 1);
    assert!(!i.runners[0].drain_safe, "base is always protected");
    assert!(
        plan(
            &pool(),
            &State::default(),
            &i,
            limits::Liability {
                cents: 1000,
                instances: 10,
                disk_bytes: u64::MAX
            }
        )
        .unwrap()
        .reason
        .contains("cap violation")
    );
}
#[test]
fn inventory_coverage_staleness_and_duplicate_mapping_fail_closed() {
    let f = Fixture::new();
    f.node("base", 100);
    let empty = observations(json!([]), json!([]));
    assert!(!inputs_on(&f.c.db, &[pool()], &empty, 100, 24, &[]).unwrap()[0].inventory_fresh);
    let mut good = observations(json!([instance("one", "ready", Some("base"))]), json!([]));
    assert!(inputs_on(&f.c.db, &[pool()], &good, 100, 24, &[]).unwrap()[0].inventory_fresh);
    assert!(!inputs_on(&f.c.db, &[pool()], &good, 116, 24, &[]).unwrap()[0].inventory_fresh);
    let retired = vec!["base".to_owned()];
    assert!(inputs_on(&f.c.db, &[pool()], &empty, 100, 24, &retired).is_err());
    f.c.db.execute("UPDATE nodes SET revoked=1", []).unwrap();
    assert!(inputs_on(&f.c.db, &[pool()], &empty, 100, 24, &retired).unwrap()[0].inventory_fresh);
    // Explicit retirement never hides an instance that is still in the provider inventory.
    assert_eq!(
        inputs_on(&f.c.db, &[pool()], &good, 100, 24, &retired).unwrap()[0]
            .liabilities
            .instances,
        1
    );
    good.instances
        .push(serde_json::from_value(instance("two", "ready", Some("base"))).unwrap());
    assert!(inputs_on(&f.c.db, &[pool()], &good, 100, 24, &[]).is_err());
    good.instances.pop();
    f.c.db
        .execute("UPDATE nodes SET capabilities='[]'", [])
        .unwrap();
    assert!(inputs_on(&f.c.db, &[pool()], &good, 100, 24, &[]).is_err());
}
#[test]
fn running_pinned_unknown_stale_and_unmanaged_boxes_never_become_safe_drain_candidates() {
    let f = Fixture::new();
    f.node("base", 100);
    f.node("disposable", 100);
    f.c.db
        .execute(
            "INSERT INTO users(id,issuer,email) VALUES(1,'test','owner@example.test')",
            [],
        )
        .unwrap();
    let o = observations(
        json!([
            instance("one", "ready", Some("base")),
            instance("two", "ready", Some("disposable"))
        ]),
        json!([]),
    );
    for metadata in [
        json!({"state":"running"}),
        json!({"state":"missing"}),
        json!({"state":"stopped","pinned":true}),
    ] {
        f.c.db.execute("INSERT OR REPLACE INTO resources(owner,kind,name,physical,node,metadata,updated_at) VALUES(1,'machine','box','physical','disposable',?1,datetime(100,'unixepoch'))",[metadata.to_string()]).unwrap();
        assert!(!inputs_on(&f.c.db, &[pool()], &o, 100, 24, &[]).unwrap()[0].runners[1].drain_safe);
    }
    f.c.db
        .execute(
            "UPDATE resources SET metadata='{\"state\":\"stopped\"}'",
            [],
        )
        .unwrap();
    assert!(inputs_on(&f.c.db, &[pool()], &o, 100, 24, &[]).unwrap()[0].runners[1].drain_safe);
    assert!(!inputs_on(&f.c.db, &[pool()], &o, 116, 24, &[]).unwrap()[0].runners[1].drain_safe);
    f.c.db
        .execute("INSERT INTO node_usage VALUES('disposable',0,0,1)", [])
        .unwrap();
    assert!(!inputs_on(&f.c.db, &[pool()], &o, 100, 24, &[]).unwrap()[0].runners[1].drain_safe);
}
#[test]
fn admission_budgets_keep_pending_and_logical_disk_reservations_and_unknown_health() {
    let f = Fixture::new();
    f.node("base", 100);
    f.node("disposable", 100);
    f.c.db
        .execute(
            "INSERT INTO users(id,issuer,email) VALUES(1,'test','owner@example.test')",
            [],
        )
        .unwrap();
    f.c.db.execute("INSERT INTO resources(owner,kind,name,physical,node,metadata,reserved_mib,reserved_cpus,updated_at) VALUES(1,'machine','box','physical','disposable','{\"state\":\"hibernated\"}',1024,2,datetime(100,'unixepoch'))",[]).unwrap();
    f.c.db
        .execute(
            "INSERT INTO owner_allocations VALUES('physical',2,1024,64)",
            [],
        )
        .unwrap();
    let o = observations(
        json!([
            instance("one", "ready", Some("base")),
            instance("two", "ready", Some("disposable"))
        ]),
        json!([]),
    );
    let i = inputs_on(&f.c.db, &[pool()], &o, 100, 24, &[])
        .unwrap()
        .pop()
        .unwrap();
    assert!(!i.durability_healthy);
    let free = i.runners[1].candidate.free;
    assert_eq!(free.cpus, 6);
    assert_eq!(free.memory_mib, 7168);
    assert_eq!(free.slots, 3);
    assert_eq!(free.disk_bytes, 192 * common::GIB);
    let durable = json!({"state":"hibernated","durability":{"overdue":false,"last_durable_at":90,"error":null}});
    f.c.db
        .execute("UPDATE resources SET metadata=?1", [durable.to_string()])
        .unwrap();
    assert!(inputs_on(&f.c.db, &[pool()], &o, 100, 24, &[]).unwrap()[0].durability_healthy);
}
#[test]
fn malformed_or_public_config_does_not_erase_a_reservation() {
    let mut f = Fixture::new();
    f.configure();
    let root = f.c.root.clone();
    attempt(&root, &mut f.c.db, 100).unwrap();
    let before = state(&f.c.db, "default").unwrap();
    std::fs::set_permissions(
        root.join("scaler-observations.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(attempt(&root, &mut f.c.db, 101).is_err());
    assert_eq!(state(&f.c.db, "default").unwrap(), before);
}

#[test]
fn schema_six_migration_backs_up_catalog_and_preserves_existing_ownership() {
    let root = std::env::temp_dir().join(format!("ow-sc-migrate-{}", common::nonce().unwrap()));
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let c = Catalog::open(&root, "test", None).unwrap();
    c.db.execute(
        "INSERT INTO users(id,issuer,email) VALUES(1,'test','owner@example.test')",
        [],
    )
    .unwrap();
    c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node) VALUES(1,'machine','parked','physical','{\"state\":\"stopped\"}','')",[]).unwrap();
    c.db.execute_batch("DROP TABLE scaler_state; DROP TABLE scaler_actions; DROP TABLE scaler_health; UPDATE settings SET value='6' WHERE key='min_reader_schema'; PRAGMA user_version=6;").unwrap();
    drop(c);
    let c = Catalog::open(&root, "test", None).unwrap();
    assert_eq!(
        c.db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        7
    );
    assert!(nodes::readable_by(&c.db, 6).is_err());
    assert_eq!(
        c.db.query_row(
            "SELECT physical FROM resources WHERE name='parked'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "physical"
    );
    assert!(std::fs::read_dir(&root).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("catalog.pre-placement-")
    }));
    drop(c);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn real_wake_admission_supplies_one_exact_shape_without_claiming_target_capacity() {
    use crate::transfers::{Intent, Kind};
    let mut f = Fixture::new();
    f.configure();
    f.c.db
        .execute(
            "INSERT INTO users(id,issuer,email) VALUES(1,'test','owner@example.test')",
            [],
        )
        .unwrap();
    f.c.db.execute("INSERT INTO artifacts(id,kind,attempt,digest,owner,machine,local_name,state,store_url) VALUES('capture','capture','attempt',?1,1,'physical','sleep','stored','s3://fixture/private')",["a".repeat(64)]).unwrap();
    f.c.db.execute("INSERT INTO resources(owner,kind,name,physical,node,metadata,current_capture,placement_managed,placement_epoch,ssh_policy) VALUES(1,'machine','box','physical','','{\"state\":\"hibernated\",\"memory_mib\":4096,\"vcpu_count\":4,\"disk_bytes\":68719476736,\"image\":\"ubuntu\"}','capture',1,1,'\"disabled\"')",[]).unwrap();
    let intent = Intent {
        kind: Kind::Wake,
        machine: "box".into(),
        target: None,
        capture: Some("capture".into()),
        child: None,
        snapshot: None,
        memory_mib: 4096,
        cpus: 4,
        disk_bytes: 64 * common::GIB,
        accept_cold: true,
        explicit_start: false,
    };
    let t = f.c.begin_transfer(1, "wake-retry", &intent).unwrap();
    assert_eq!(
        f.c.begin_transfer(1, "wake-retry", &intent).unwrap().id,
        t.id
    );
    let o = observations(json!([]), json!([]));
    let i = inputs_on(&f.c.db, &[pool()], &o, 100, 24, &[])
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(i.queued.len(), 1);
    assert_eq!(
        i.queued[0].shape,
        Shape {
            cpus: 4,
            memory_mib: 4096,
            slots: 1,
            disk_bytes: 128 * common::GIB
        }
    );
    let r = plan(&pool(), &State::default(), &i, super::super::tests::caps()).unwrap();
    assert_eq!(r.decision, "launch");
    assert_eq!(r.state.action.unwrap().target, "large");
    let root = f.c.root.clone();
    attempt(&root, &mut f.c.db, 100).unwrap();
    assert!(f.c.transfer(t.id).unwrap().target.is_none());
    assert_eq!(
        f.c.db
            .query_row("SELECT COUNT(*) FROM transfer_reservations", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn unattended_tick_refreshes_expired_durability_through_read_only_runner_inventory() {
    use std::os::unix::net::UnixListener;
    let f = Fixture::new();
    f.configure();
    let now = ow_core::recovery::now();
    f.node("base", now);
    f.c.db
        .execute(
            "INSERT INTO users(id,issuer,email) VALUES(1,'test','owner@example.test')",
            [],
        )
        .unwrap();
    let stale = json!({"state":"running","pinned":true,"memory_mib":1024,"durability":{"overdue":false,"current_state_durable":false,"last_durable_at":now-16,"error":null}});
    f.c.db.execute("INSERT INTO resources(owner,kind,name,physical,node,metadata,updated_at) VALUES(1,'machine','box','physical','base',?1,datetime(?2,'unixepoch'))",params![stale.to_string(),(now-16) as i64]).unwrap();
    let mut p = pool();
    p.floor = 2;
    f.write("scaler.json", json!({"mode":"shadow","pools":[p]}));
    f.write("scaler-observations.json",json!({"hours":24,"observed_at":now,"complete":true,"retention_ok":true,"outcome_resolved":true,"instances":[instance("one","ready",Some("base"))],"disks":[],"receipts":{}}));
    let node_root = f.c.root.join("nodes/base");
    std::fs::create_dir_all(&node_root).unwrap();
    let socket = UnixListener::bind(node_root.join("control.sock")).unwrap();
    let worker = std::thread::spawn(move || {
        for fail in [false, true] {
            let (mut peer, _) = socket.accept().unwrap();
            assert_eq!(crate::wire::line(&mut peer).unwrap(), json!({"op":"list"}));
            let result = json!([{"id":"physical","state":"running","durability":{"overdue":false,"current_state_durable":false,"last_durable_at":now-16,"error":if fail {json!("upload failed")} else {Value::Null}}}]);
            crate::wire::send(&mut peer, &json!({"ok":true,"result":result})).unwrap();
        }
    });
    let result = command(&f.c.root, true).unwrap();
    assert_eq!(
        result["pools"][0]["decision"], "launch",
        "healthy recovery age must not depend on client activity"
    );
    let metadata: String =
        f.c.db
            .query_row(
                "SELECT metadata FROM resources WHERE physical='physical'",
                [],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&metadata).unwrap()["pinned"],
        true
    );
    let o: Observations = read_private(&f.c.root.join("scaler-observations.json")).unwrap();
    assert!(
        inputs_on(&f.c.db, &[pool()], &o, ow_core::recovery::now(), 24, &[]).unwrap()[0]
            .durability_healthy
    );
    // The next poll must consume an upload failure, even without any client reconcile.
    command(&f.c.root, true).unwrap();
    assert!(
        !inputs_on(&f.c.db, &[pool()], &o, ow_core::recovery::now(), 24, &[]).unwrap()[0]
            .durability_healthy
    );
    worker.join().unwrap();
}

#[test]
fn periodic_observations_refresh_after_sixteen_seconds_but_stale_session_and_epoch_do_not() {
    let f = Fixture::new();
    f.node("base", 100);
    f.c.db
        .execute(
            "INSERT INTO users(id,issuer,email) VALUES(1,'test','owner@example.test')",
            [],
        )
        .unwrap();
    let metadata = json!({"state":"running","durability":{"overdue":false,"last_durable_at":100,"error":null}});
    f.c.db.execute("INSERT INTO resources(owner,kind,name,physical,node,metadata,updated_at) VALUES(1,'machine','box','physical','base',?1,datetime(100,'unixepoch'))",[metadata.to_string()]).unwrap();
    f.c.db.execute("UPDATE nodes SET heartbeat=116,capabilities=json_set(capabilities,'$.disk_observed_unix_s',116)",[]).unwrap();
    let mut o = observations(json!([instance("one", "ready", Some("base"))]), json!([]));
    o.observed_at = 116;
    assert!(!inputs_on(&f.c.db, &[pool()], &o, 116, 24, &[]).unwrap()[0].durability_healthy);
    refresh(&f.c.db,116,&|_|Ok(json!([{"id":"physical","state":"running","durability":{"overdue":false,"last_durable_at":100,"error":null}}]))).unwrap();
    assert!(inputs_on(&f.c.db, &[pool()], &o, 116, 24, &[]).unwrap()[0].durability_healthy);
    for race in ["session", "epoch"] {
        refresh(&f.c.db, 116, &|_| {
            if race == "session" {
                f.c.db
                    .execute("UPDATE nodes SET session='replacement'", [])?;
            } else {
                f.c.db
                    .execute("UPDATE resources SET placement_epoch=1", [])?;
            }
            Ok(json!([{"id":"physical","state":"stopped","durability":null}]))
        })
        .unwrap();
        let state: String =
            f.c.db
                .query_row(
                    "SELECT json_extract(metadata,'$.state') FROM resources",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
        assert_eq!(state, "running", "{race} raced a read-only observation");
    }
    f.c.db
        .execute("UPDATE nodes SET heartbeat=100,session='test-session'", [])
        .unwrap();
    refresh(&f.c.db, 116, &|_| panic!("stale node must not be polled")).unwrap();
    refresh(&f.c.db, 117, &|_| anyhow::bail!("offline")).unwrap();
    assert!(!inputs_on(&f.c.db, &[pool()], &o, 132, 24, &[]).unwrap()[0].durability_healthy);
}

#[test]
fn inventory_refresh_requires_exact_committed_managed_placement_and_no_transfer_lock() {
    let f = Fixture::new();
    f.node("base", 116);
    f.c.db
        .execute(
            "INSERT INTO users(id,issuer,email) VALUES(1,'test','owner@example.test')",
            [],
        )
        .unwrap();
    f.c.db.execute("INSERT INTO resources(owner,kind,name,physical,node,metadata,placement_managed,placement_epoch,updated_at) VALUES(1,'machine','box','physical','base','{\"state\":\"running\"}',1,1,datetime(100,'unixepoch'))",[]).unwrap();
    f.c.db.execute("INSERT INTO operations(id,owner,op,resource,state) VALUES(1,1,'restore','box','succeeded')",[]).unwrap();
    f.c.db.execute("INSERT INTO transfers(id,operation,kind,source,machine,source_node,source_epoch,source_policy_generation,target,epoch,step) VALUES(1,1,'restore','physical','physical','',0,0,'base',1,'succeeded')",[]).unwrap();
    let response = |epoch| json!([{"id":"physical","state":"hibernated","vmm_running":false,"durability":{"current_state_durable":true,"overdue":false,"error":null},"placement":{"node":"base","epoch":epoch,"transfer":"transfer-1"}}]);
    let state = || {
        f.c.db
            .query_row(
                "SELECT json_extract(metadata,'$.state') FROM resources",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
    };
    refresh(&f.c.db, 116, &|_| Ok(response(2))).unwrap();
    assert_eq!(state(), "running");
    refresh(&f.c.db, 116, &|_| Ok(response(1))).unwrap();
    assert_eq!(state(), "hibernated");
    f.c.db
        .execute("INSERT INTO transfer_locks VALUES('physical',1)", [])
        .unwrap();
    refresh(&f.c.db, 117, &|_| {
        panic!("an active transfer must not be polled")
    })
    .unwrap();
    assert_eq!(state(), "hibernated");
}
