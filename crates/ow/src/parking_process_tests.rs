//! Actual controller/agent SIGKILL with real publication/pull jobs and durable worker fixtures.
use super::*;
fn park_fixture(ram: bool) -> (Processes, i64, i64) {
    let p = fixture();
    let ctl = p.root.join("controller");
    fs::write(ctl.join("store-url"), "s3://bucket/root").unwrap();
    fs::write(p.root.join("bin/aws"),r##"#!/usr/bin/env python3
import sys,json,hashlib,base64,pathlib
root=pathlib.Path(__file__).parent.parent/'objects'
a=sys.argv[1:]
def arg(k):return a[a.index(k)+1]
def digest(b):return base64.b64encode(hashlib.sha256(b).digest()).decode()
if a[:3]==['s3','cp','--only-show-errors']:
 sys.stdout.buffer.write((root/a[3][5:]).read_bytes());sys.exit(0)
assert a[0]=='s3api'
op=a[1]
if op=='get-bucket-versioning':print('{"Status":"Enabled"}')
elif op=='get-bucket-lifecycle-configuration':print('{"Rules":[{"Status":"Enabled","Filter":{"Prefix":"root/store/v1/"},"NoncurrentVersionExpiration":{"NoncurrentDays":7}}]}')
elif op=='get-bucket-policy':print(json.dumps({'Policy':json.dumps({'Statement':[{'Effect':'Deny','Principal':'*','Action':'s3:PutObject','Resource':'arn:aws:s3:::bucket/root/store/v1/*','Condition':{'Null':{'s3:if-none-match':'true'},'Bool':{'s3:ObjectCreationOperation':'true'}}}]})}))
else:
 dest=root/arg('--bucket')/arg('--key')
 if op=='put-object':
  assert arg('--if-none-match')=='*'
  data=pathlib.Path(arg('--body')).read_bytes();assert digest(data)==arg('--checksum-sha256')
  dest.parent.mkdir(parents=True,exist_ok=True)
  try:
   with dest.open('xb') as f:f.write(data);f.flush();__import__('os').fsync(f.fileno())
  except FileExistsError:sys.exit(1)
  print('{}')
 elif op=='head-object':
  data=dest.read_bytes();print(json.dumps({'ChecksumSHA256':digest(data),'ContentLength':len(data)}))
 else:raise Exception(op)
"##).unwrap();
    let c = Catalog::open(&ctl, "test", None).unwrap();
    c.db.execute("DELETE FROM transfer_reservations", [])
        .unwrap();
    c.db.execute("DELETE FROM transfer_locks", []).unwrap();
    c.db.execute(
        "UPDATE transfers SET step='succeeded',target='source' WHERE id=1",
        [],
    )
    .unwrap();
    c.db.execute("UPDATE operations SET state='succeeded'", [])
        .unwrap();
    c.db.execute("UPDATE resources SET placement_managed=1,placement_epoch=1,reserved_mib=256,reserved_cpus=1,metadata=json_set(metadata,'$.state',?1)",[if ram{"hibernated"}else{"stopped"}]).unwrap();
    let user =
        c.db.query_row(
            "SELECT owner FROM resources WHERE physical='physical'",
            [],
            |r| r.get(0),
        )
        .unwrap();
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
    let park = c.begin_transfer(user, "process-park", &input).unwrap();
    drop(c);
    let path = p.root.join("source/fixture-state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["park_fixture"] = json!(true);
    state["ram"] = json!(ram);
    state["workspace"]["state"] = json!(if ram { "hibernated" } else { "stopped" });
    state["workspace"]["placement"] = json!({"epoch":1,"node":"source","transfer":"transfer-1"});
    state["workspace"]["lifecycle"] = json!({"phase":"active"});
    ow_core::runtime::atomic_json(&path, &state).unwrap();
    (p, user, park.id)
}
fn wait_transfer(p: &Processes, id: i64) {
    let start = Instant::now();
    loop {
        let db = nodes::db(&p.root.join("controller")).unwrap();
        let step: String = db
            .query_row("SELECT step FROM transfers WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        if step == "succeeded" {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(45),
            "stuck {id} at {step}: {}",
            p.root.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn killed_phases(p: &mut Processes, id: i64, phases: &[&str]) {
    let ctl = p.root.join("controller");
    for phase in phases {
        fs::write(ctl.join(format!("hold-controller-{phase}")), b"").unwrap();
    }
    p.start_controller();
    for phase in phases {
        let pid = p.controller.as_ref().unwrap().id();
        p.wait_event(&ctl, &format!("controller-{phase}"), pid);
        eprintln!("SIGKILL T4 controller pid={pid} transfer={id} phase={phase}");
        p.kill_controller();
        let db = nodes::db(&ctl).unwrap();
        let step: String = db
            .query_row("SELECT step FROM transfers WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(&step, phase);
        fs::remove_file(ctl.join(format!("hold-controller-{phase}"))).unwrap();
        p.start_controller();
    }
    wait_transfer(p, id);
}
#[test]
#[cfg(target_arch = "x86_64")]
fn sigkill_park_and_wake_at_every_durable_phase() {
    for ram in [false, true] {
        let (mut p, user, park) = park_fixture(ram);
        p.start_agent("source");
        p.start_agent("target");
        killed_phases(
            &mut p,
            park,
            &[
                "preparing",
                "exported",
                "prepared",
                "committed",
                "succeeded",
            ],
        );
        p.kill_controller();
        let ctl = p.root.join("controller");
        let c = Catalog::open(&ctl, "test", None).unwrap();
        assert!(c.db.query_row("SELECT node='' AND current_capture IS NOT NULL AND reserved_mib=0 FROM resources WHERE physical='physical'",[],|r|r.get::<_,bool>(0)).unwrap());
        fs::write(p.root.join("source/drain"), b"").unwrap();
        c.db.execute(
            "UPDATE nodes SET capabilities=json_set(capabilities,'$.drain',1) WHERE id='source'",
            [],
        )
        .unwrap();
        let wake = super::super::parking::admission(
            &c,
            user,
            &json!({"op":if ram{"connect"}else{"start"},"id":"box","operation_key":"process-wake"}),
            Kind::Wake,
        )
        .unwrap();
        drop(c);
        killed_phases(
            &mut p,
            wake.id,
            &[
                "preparing",
                "exported",
                "importing",
                "prepared",
                "committed",
                "start_uncertain",
                "started",
                "succeeded",
            ],
        );
        let c = nodes::db(&ctl).unwrap();
        assert_eq!(
            c.query_row(
                "SELECT node FROM resources WHERE physical='physical'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "target"
        );
        assert_eq!(
            fs::read(p.root.join("target/machines/physical/disk.ext4")).unwrap(),
            b"disk"
        );
        let state: Value =
            serde_json::from_slice(&fs::read(p.root.join("target/fixture-state.json")).unwrap())
                .unwrap();
        assert_eq!(state["counts"]["import-prepare"], 1);
        assert_eq!(state["counts"]["activate"], 1);
        assert_eq!(
            c.query_row("SELECT COUNT(*) FROM transfer_reservations", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
#[test]
#[cfg(target_arch = "x86_64")]
fn sigkill_agent_before_and_after_park_publication() {
    for boundary in ["before-effect", "after-effect"] {
        let (mut p, _, park) = park_fixture(true);
        let source = p.root.join("source");
        let label = format!("agent-capture-push-{boundary}");
        fs::write(source.join(format!("hold-{label}")), b"").unwrap();
        p.start_controller();
        p.start_agent("source");
        p.start_agent("target");
        let pid = p.agents["source"].id();
        p.wait_event(&source, &label, pid);
        eprintln!("SIGKILL T4 agent pid={pid} publication {boundary}");
        p.kill_agent("source");
        p.kill_controller();
        let db = nodes::db(&p.root.join("controller")).unwrap();
        assert!(db.query_row("SELECT node='source' AND reserved_mib=256 FROM resources WHERE physical='physical'",[],|r|r.get::<_,bool>(0)).unwrap());
        fs::remove_file(source.join(format!("hold-{label}"))).unwrap();
        p.start_agent("source");
        p.start_controller();
        wait_transfer(&p, park);
        assert!(db.query_row("SELECT node='' AND current_capture IS NOT NULL FROM resources WHERE physical='physical'",[],|r|r.get::<_,bool>(0)).unwrap());
        assert_eq!(
            fs::read(source.join(format!("snapshots/transfer-{park}-source/memory"))).unwrap(),
            b"ram"
        );
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn real_park_jobs_complete_without_process_loss() {
    let (mut p, _, park) = park_fixture(true);
    p.start_controller();
    p.start_agent("source");
    p.start_agent("target");
    wait_transfer(&p, park);
}
