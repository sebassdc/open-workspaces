//! Actual controller process loss; the worker is a durable execution fixture, not a VMM.
use super::*;

#[test]
#[cfg(target_arch = "x86_64")]
fn sigkill_move_and_fork_at_every_durable_phase() {
    for kind in [Kind::Move, Kind::Fork] {
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
