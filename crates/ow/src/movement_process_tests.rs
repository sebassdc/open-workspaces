//! Actual controller process loss; the worker is a durable execution fixture, not a VMM.
use super::*;

#[test]
#[cfg(target_arch = "x86_64")]
fn sigkill_move_and_fork_at_every_durable_phase() {
    for kind in [Kind::Move, Kind::Fork] {
        let (mut p, _, t) = moving_process_fixture(kind);
        let path = p.root.join("source/fixture-state.json");
        super::parking::killed_phases(
            &mut p,
            t.id,
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
        let target: Value =
            serde_json::from_slice(&fs::read(p.root.join("target/fixture-state.json")).unwrap())
                .unwrap();
        assert_eq!(target["counts"]["activate"], 1);
        assert_eq!(target["workspace"]["id"], t.machine);
        assert_eq!(
            fs::read(
                p.root
                    .join("target/machines")
                    .join(&t.machine)
                    .join("disk.ext4")
            )
            .unwrap(),
            b"disk"
        );
        let source: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        if kind == Kind::Move {
            assert_eq!(source["workspace"]["lifecycle"]["phase"], "cleaned");
        } else {
            assert_eq!(source["workspace"]["vmm_running"], true);
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn moving_process_fixture(kind: Kind) -> (Processes, i64, Transfer) {
    let (mut p, user, park) = super::parking::park_fixture(true);
    let ctl = p.root.join("controller");
    let c = Catalog::open(&ctl, "test", None).unwrap();
    let t = c.transfer(park).unwrap();
    c.advance_transfer(t.id, t.epoch, t.step, Step::Aborted, &Evidence::default())
        .unwrap();
    c.db.execute("UPDATE resources SET metadata=json_set(metadata,'$.state','running') WHERE physical='physical'",[]).unwrap();
    let mut input = super::super::parking::shape(&c, "physical", kind, "box").unwrap();
    input.target = Some("target".into());
    if kind == Kind::Fork {
        input.child = Some(crate::transfers::Child {
            id: "child-id".into(),
            name: "child".into(),
        });
    }
    let t = c.begin_transfer(user, "process-move-fork", &input).unwrap();
    drop(c);
    let path = p.root.join("source/fixture-state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["workspace"]["state"] = json!("running");
    state["workspace"]["vmm_running"] = json!(true);
    ow_core::runtime::atomic_json(&path, &state).unwrap();
    p.start_agent("source");
    p.start_agent("target");
    (p, user, t)
}

#[test]
#[cfg(target_arch = "x86_64")]
fn managed_ssh_enrollment_replacement_and_revocation_cross_the_node_agent_after_move_and_fork() {
    use base64::Engine;
    let key = |byte| {
        let mut bytes = vec![0, 0, 0, 11];
        bytes.extend_from_slice(b"ssh-ed25519");
        bytes.extend_from_slice(&[0, 0, 0, 32]);
        bytes.extend_from_slice(&[byte; 32]);
        format!(
            "ssh-ed25519 {}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    };
    for kind in [Kind::Move, Kind::Fork] {
        let (mut p, user, t) = moving_process_fixture(kind);
        super::parking::killed_phases(&mut p, t.id, &[]);
        p.kill_controller();
        let ctl = p.root.join("controller");
        // Let this process own the catalog while the real node listener runs separately.
        p.controller = Some(p.child("transport", &ctl));
        let started = Instant::now();
        let ready = |node| {
            nodes::route(&ctl, node)
                .and_then(|route| wire::request(&route, json!({"op":"status"})))
                .is_ok()
        };
        while !ready("source") || !ready("target") {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "node reconnect timeout"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut c = Catalog::open(&ctl, "test", None).unwrap();
        let address = if kind == Kind::Move { "box" } else { "child" };
        let authority = c.managed_placement(&t.machine).unwrap().unwrap();
        assert_eq!(authority.node, "target");
        let host_key = key(1);
        for (index, keys) in [vec![host_key.clone()], vec![key(2)], vec![]]
            .into_iter()
            .enumerate()
        {
            let request = json!({"op":"ssh-keys","id":address,"keys":keys,"operation_key":format!("change-{index}")});
            if index == 0 {
                fs::write(p.root.join("target/drop-ssh-policy-reply"), b"").unwrap();
                assert!(c.operation(user, request.clone()).is_err());
                assert_eq!(
                    c.db.query_row(
                        "SELECT state FROM operations WHERE owner=?1 AND retry_key='change-0'",
                        [user],
                        |r| r.get::<_, String>(0)
                    )
                    .unwrap(),
                    "pending"
                );
                drop(c);
                c = Catalog::open(&ctl, "test", None).unwrap();
            }
            let result = c.operation(user, request.clone()).unwrap();
            assert_eq!(c.operation(user, request).unwrap(), result);
            let (generation, raw): (i64, String) =
                c.db.query_row(
                    "SELECT ssh_policy_generation,ssh_policy FROM resources WHERE physical=?1",
                    [&t.machine],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(generation, index as i64 + 1);
            let policy: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(policy["enrolled"]["keys"], json!(keys));
            assert_eq!(policy["enrolled"]["host_key"], host_key);
            assert_eq!(
                c.db.query_row(
                    "SELECT state FROM operations WHERE owner=?1 AND retry_key=?2",
                    params![user, format!("change-{index}")],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
                "succeeded"
            );
        }
        let state: Value =
            serde_json::from_slice(&fs::read(p.root.join("target/fixture-state.json")).unwrap())
                .unwrap();
        assert_eq!(state["counts"]["ssh-keys"], 3);
        assert_eq!(state["counts"]["ssh-policy"], 4);
        // Replaying an older pending enrollment must read today's revoked policy.
        c.db.execute("UPDATE operations SET state='pending',response=NULL WHERE owner=?1 AND retry_key='change-0'",[user]).unwrap();
        drop(c);
        let mut c = Catalog::open(&ctl, "test", None).unwrap();
        c.operation(
            user,
            json!({"op":"ssh-keys","id":address,"keys":[host_key],"operation_key":"change-0"}),
        )
        .unwrap();
        let raw: String =
            c.db.query_row(
                "SELECT ssh_policy FROM resources WHERE physical=?1",
                [&t.machine],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap()["enrolled"]["keys"],
            json!([])
        );
        let state: Value =
            serde_json::from_slice(&fs::read(p.root.join("target/fixture-state.json")).unwrap())
                .unwrap();
        assert_eq!(state["counts"]["ssh-keys"], 3);
        assert_eq!(state["counts"]["ssh-policy"], 5);
        let route = nodes::route(&ctl, "target").unwrap();
        let mut stale = json!(authority);
        stale["epoch"] = json!(authority.epoch - 1);
        assert!(
            wire::request(
                &route,
                json!({"op":"ssh-policy","id":t.machine,"placement":stale})
            )
            .is_err()
        );
        assert!(wire::request(&route, json!({"op":"status"})).is_ok());
    }
}
