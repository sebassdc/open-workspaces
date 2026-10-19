use super::*;
pub(super) fn caps() -> limits::Liability {
    limits::Liability {
        cents: 10000,
        instances: 10,
        disk_bytes: 4096 * common::GIB,
    }
}
pub(super) fn shape(cpus: u64, ram: u64, disk: u64) -> Shape {
    Shape {
        cpus,
        memory_mib: ram,
        slots: 1,
        disk_bytes: disk * common::GIB,
    }
}
pub(super) fn capabilities() -> Value {
    json!({"backend":"firecracker","images":["ubuntu"],"max_guest_memory_mib":16384,"max_guest_vcpus":16,"disk_resize_v1":true,"worker_protocol":3,"placement_fencing_v1":true,"cache_inventory_v1":true,"managed_restore_v1":true,"compat":{"runtime":"r","cpu":"c","kernel_sha256":"k"}})
}
fn preset(name: &str, capacity: Shape, cents: u64) -> Preset {
    Preset {
        name: name.into(),
        capacity,
        capabilities: capabilities(),
        cost: limits::Liability {
            cents,
            instances: 1,
            disk_bytes: capacity.disk_bytes,
        },
    }
}
pub(super) fn pool() -> Pool {
    Pool {
        name: "default".into(),
        floor: 0,
        ceiling: 10,
        cooldown_seconds: 30,
        idle_seconds: 60,
        join_timeout_seconds: 300,
        launch_allowed: true,
        protected_nodes: vec!["base".into(), "workstation".into()],
        replacement_headroom: Some(1),
        presets: vec![
            preset("small", shape(2, 2048, 32), 100),
            preset("large", shape(8, 8192, 128), 400),
        ],
    }
}
fn demand(id: &str, shape: Shape) -> Queued {
    Queued {
        id: id.into(),
        image: "ubuntu".into(),
        shape,
        compatibility: Some(json!({"runtime":"r","cpu":"c","kernel_sha256":"k"})),
        compatibility_known: true,
    }
}
fn runner(id: &str, shape: Shape, at: u64) -> Runner {
    let p = pool();
    let preset = preset("test", shape, 0);
    Runner {
        candidate: placement::Candidate {
            id: id.into(),
            joined: 1,
            ..preset_runner(&p, &preset, at)
        },
        total: shape,
        drain_safe: true,
        soft_draining: false,
    }
}
fn input() -> Input {
    Input {
        now: 100,
        inventory_fresh: true,
        retention_ok: true,
        outcome_resolved: true,
        durability_healthy: true,
        liabilities: limits::Liability::default(),
        pool_instances: 0,
        ready_instances: 0,
        runners: vec![],
        queued: vec![],
        receipt: None,
    }
}
fn step(p: &Pool, s: &State, i: &Input) -> Plan {
    plan(p, s, i, caps()).unwrap()
}
#[test]
fn mixed_shapes_are_packed_per_runner_in_every_dimension() {
    for (index, wanted) in [shape(4, 1024, 16), shape(1, 4096, 16), shape(1, 1024, 64)]
        .into_iter()
        .enumerate()
    {
        let mut i = input();
        i.queued = vec![demand("medium", wanted)];
        i.runners = (0..4)
            .map(|n| runner(&format!("s{n}"), shape(2, 2048, 32), i.now))
            .collect();
        i.ready_instances = 4;
        i.pool_instances = 4;
        i.liabilities.instances = 4;
        let result = step(&pool(), &State::default(), &i);
        assert_eq!(result.decision, "launch", "dimension {index}");
        assert_eq!(result.state.action.unwrap().target, "large");
        assert_eq!(result.queued, ["medium"]);
    }
    let mut i = input();
    i.runners = vec![runner("one", shape(2, 2048, 64), 100)];
    i.queued = vec![
        demand("s1", shape(1, 1024, 16)),
        demand("s2", shape(1, 1024, 16)),
    ];
    // Enough CPU/RAM/disk for two, but only one slot.
    assert_eq!(step(&pool(), &State::default(), &i).queued, ["s2"]);
    i.runners[0].candidate.free.slots = 2;
    assert_eq!(step(&pool(), &State::default(), &i).decision, "wait");
}
#[test]
fn mixed_small_then_large_never_turns_aggregate_spare_into_a_large_slot() {
    let mut i = input();
    i.runners = vec![
        runner("a", shape(2, 2048, 64), 100),
        runner("b", shape(2, 2048, 64), 100),
    ];
    i.queued = vec![
        demand("small", shape(1, 1024, 16)),
        demand("large", shape(4, 4096, 64)),
    ];
    let r = step(&pool(), &State::default(), &i);
    assert_eq!(r.queued, ["large"]);
    assert_eq!(r.state.action.unwrap().target, "large");
}
#[test]
fn shape_compatibility_image_and_fresh_disk_are_required() {
    for reason in 0..5 {
        let mut i = input();
        i.queued = vec![demand("q", shape(2, 2048, 32))];
        let mut p = pool();
        for preset in &mut p.presets {
            match reason {
                0 => preset.capabilities["compat"]["cpu"] = json!("other"),
                1 => preset.capabilities["images"] = json!([]),
                2 => preset.capabilities["max_guest_vcpus"] = json!(1),
                3 => preset.capabilities["managed_restore_v1"] = json!(false),
                _ => i.queued[0].compatibility_known = false,
            }
        }
        assert_eq!(step(&p, &State::default(), &i).decision, "wait", "{reason}");
    }
}
#[test]
fn reserves_maximum_new_cost_including_replacement_and_full_logical_disk() {
    let mut i = input();
    i.queued = vec![demand("q", shape(1, 1024, 16))];
    let r = step(&pool(), &State::default(), &i);
    assert_eq!(
        r.state.action.unwrap().reservation,
        limits::Liability {
            cents: 200,
            instances: 2,
            disk_bytes: 64 * common::GIB
        }
    );
    for cap in [
        limits::Liability {
            cents: 199,
            ..caps()
        },
        limits::Liability {
            instances: 1,
            ..caps()
        },
        limits::Liability {
            disk_bytes: 63 * common::GIB,
            ..caps()
        },
    ] {
        assert_eq!(
            plan(&pool(), &State::default(), &i, cap).unwrap().decision,
            "wait"
        );
    }
    let mut p = pool();
    p.replacement_headroom = None;
    assert!(
        step(&p, &State::default(), &i)
            .reason
            .contains("no enforceable finite bound")
    );
    p.replacement_headroom = Some(1);
    p.ceiling = 1;
    assert_eq!(step(&p, &State::default(), &i).decision, "wait");
}
#[test]
fn count_floor_schedule_and_ceiling_do_not_evict_or_bypass_caps() {
    let mut p = pool();
    p.floor = 1;
    let i = input();
    assert_eq!(step(&p, &State::default(), &i).decision, "launch");
    p.launch_allowed = false;
    assert!(step(&p, &State::default(), &i).reason.contains("schedule"));
    p.launch_allowed = true;
    let mut i = input();
    i.pool_instances = 10;
    i.liabilities.instances = 10;
    // Retained and orphaned liability does not satisfy the ready floor or disappear from caps.
    assert_eq!(step(&p, &State::default(), &i).decision, "wait");
    i.pool_instances = 11;
    i.liabilities.instances = 11;
    assert!(
        step(&p, &State::default(), &i)
            .reason
            .contains("cap violation")
    );
}
#[test]
fn liability_health_and_retention_violations_disable_all_new_actions() {
    for case in 0..7 {
        let mut i = input();
        i.queued = vec![demand("q", shape(1, 1024, 16))];
        match case {
            0 => i.inventory_fresh = false,
            1 => i.retention_ok = false,
            2 => i.outcome_resolved = false,
            3 => i.durability_healthy = false,
            4 => i.liabilities.cents = 10001,
            5 => i.liabilities.disk_bytes = caps().disk_bytes + 1,
            _ => i.liabilities.instances = 11,
        };
        let r = step(&pool(), &State::default(), &i);
        assert_eq!(r.decision, "wait");
        assert!(r.reason.contains("disabled"), "{}", r.reason);
        assert!(r.state.action.is_none());
    }
    assert!(
        limits::Liability {
            cents: u64::MAX,
            ..Default::default()
        }
        .add(limits::Liability {
            cents: 1,
            ..Default::default()
        })
        .is_err()
    );
    assert!(
        limits::Liability {
            disk_bytes: u64::MAX,
            ..Default::default()
        }
        .multiply(2)
        .is_err()
    );
}
#[test]
fn duplicate_ticks_restart_and_lost_reply_never_launch_twice() {
    let p = pool();
    let mut i = input();
    i.queued = vec![demand("q", shape(1, 1024, 16))];
    let first = step(&p, &State::default(), &i);
    assert_eq!(step(&p, &first.state, &i).state, first.state);
    let mut restarted: State =
        serde_json::from_str(&serde_json::to_string(&first.state).unwrap()).unwrap();
    restarted.action.as_mut().unwrap().phase = Phase::Launching;
    let token = restarted.action.as_ref().unwrap().id.clone();
    i.now += 1;
    let lost = step(&p, &restarted, &i);
    assert_eq!(lost.state.sequence, 1);
    assert_eq!(lost.state.action.as_ref().unwrap().id, token);
    i.receipt = Some(Receipt {
        action: token.clone(),
        outcome: Outcome::Unknown,
        node: None,
    });
    let unknown = step(&p, &lost.state, &i); // same tick does not consume an uncommitted observation
    i.now += 1;
    let unknown = step(&p, &unknown.state, &i);
    assert_eq!(
        unknown.state.action.as_ref().unwrap().phase,
        Phase::Uncertain
    );
    i.now = 1000;
    i.receipt = None;
    let retry = step(&p, &unknown.state, &i);
    assert_eq!(retry.state.action.as_ref().unwrap().id, token);
    assert!(retry.reason.contains("actions disabled"));
}
#[test]
fn failed_join_timeout_holds_reservation_until_positive_reconciliation() {
    let p = pool();
    let mut i = input();
    i.queued = vec![demand("q", shape(1, 1024, 16))];
    let mut state = step(&p, &State::default(), &i).state;
    state.action.as_mut().unwrap().phase = Phase::Joining;
    i.now = 401;
    let timed = step(&p, &state, &i);
    assert_eq!(timed.state.action.as_ref().unwrap().phase, Phase::Uncertain);
    let reserve = timed.state.action.as_ref().unwrap().reservation;
    i.now += 1;
    i.receipt = Some(Receipt {
        action: "wrong-attempt".into(),
        outcome: Outcome::Absent,
        node: None,
    });
    let stale = step(&p, &timed.state, &i);
    assert_eq!(stale.state.action.as_ref().unwrap().reservation, reserve);
    i.now += 1;
    i.receipt.as_mut().unwrap().action = stale.state.action.as_ref().unwrap().id.clone();
    let absent = step(&p, &stale.state, &i);
    assert!(absent.state.action.is_none());
    assert_eq!(absent.state.cooldown_until, 433);
    i.now += 1;
    assert_eq!(step(&p, &absent.state, &i).reason, "cooldown");
}
#[test]
fn joined_is_not_ready_until_fresh_capable_enrollment_and_exact_receipt() {
    let p = pool();
    let mut i = input();
    i.queued = vec![demand("q", shape(1, 1024, 16))];
    let mut state = step(&p, &State::default(), &i).state;
    state.action.as_mut().unwrap().phase = Phase::Launching;
    i.now += 1;
    i.receipt = Some(Receipt {
        action: state.action.as_ref().unwrap().id.clone(),
        outcome: Outcome::Joined,
        node: Some("new".into()),
    });
    let waiting = step(&p, &state, &i);
    assert!(waiting.state.action.is_some());
    i.now += 1;
    i.runners = vec![runner("new", shape(2, 2048, 32), i.now)];
    i.runners[0].candidate.online = false;
    let offline = step(&p, &waiting.state, &i);
    assert!(offline.state.action.is_some());
    i.now += 1;
    i.runners[0].candidate.online = true;
    assert!(step(&p, &offline.state, &i).state.action.is_none());
}
#[test]
fn cooldown_idle_hysteresis_protected_boxes_and_demand_cancels_only_soft_intent() {
    let p = pool();
    let mut i = input();
    i.ready_instances = 2;
    i.pool_instances = 2;
    i.liabilities.instances = 2;
    i.runners = vec![
        runner("base", shape(2, 2048, 32), 100),
        runner("workstation", shape(2, 2048, 32), 100),
    ];
    let idle = step(&p, &State::default(), &i);
    assert_eq!(idle.reason, "idle hysteresis");
    i.now = 160;
    assert!(step(&p, &idle.state, &i).state.action.is_none());
    i.runners
        .push(runner("disposable", shape(2, 2048, 32), i.now));
    i.runners.last_mut().unwrap().drain_safe = false;
    assert!(step(&p, &idle.state, &i).state.action.is_none());
    i.runners.last_mut().unwrap().drain_safe = true;
    let drain = step(&p, &idle.state, &i);
    assert_eq!(drain.decision, "drain");
    assert_eq!(drain.state.action.as_ref().unwrap().target, "disposable");
    i.now += 1;
    i.queued = vec![demand("q", shape(1, 1024, 16))];
    let cancel = step(&p, &drain.state, &i);
    assert_eq!(cancel.decision, "cancel_drain");
    assert!(cancel.state.action.is_none());
    let mut sealed = drain.state;
    sealed.action.as_mut().unwrap().phase = Phase::Draining;
    assert!(step(&p, &sealed, &i).state.action.is_some());
    i.queued.clear();
    i.now += 1;
    assert_eq!(step(&p, &cancel.state, &i).reason, "cooldown");
}
#[test]
fn persisted_journal_serializes_pools_reopens_and_preserves_action_identity() {
    let root = std::env::temp_dir().join(format!("ow-scaler-{}", common::nonce().unwrap()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("state.sqlite3");
    let mut db = Connection::open(&path).unwrap();
    initialize(&db).unwrap();
    let mut i = input();
    i.queued = vec![demand("q", shape(1, 1024, 16))];
    let tx = db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let first = record(&tx, &pool(), &i, caps()).unwrap();
    tx.commit().unwrap();
    drop(db);
    let mut db = Connection::open(&path).unwrap();
    initialize(&db).unwrap();
    let tx = db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let again = record(&tx, &pool(), &i, caps()).unwrap();
    assert_eq!(first.state, again.state);
    let mut other = pool();
    other.name = "other".into();
    let tight = limits::Liability {
        cents: 399,
        ..caps()
    };
    assert_eq!(record(&tx, &other, &i, tight).unwrap().decision, "wait");
    tx.commit().unwrap();
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM scaler_actions", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let next = Input { now: 101, ..i };
    let tx = db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    assert_eq!(
        record(&tx, &pool(), &next, caps()).unwrap().state.sequence,
        1
    );
    tx.commit().unwrap();
    assert_eq!(status(&db).unwrap()["pools"].as_array().unwrap().len(), 2);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn invalid_policy_is_rejected_instead_of_silently_changing_capacity() {
    let mut p = pool();
    p.floor = 11;
    assert!(plan(&p, &State::default(), &input(), caps()).is_err());
    p = pool();
    p.presets[0].cost.instances = 0;
    assert!(plan(&p, &State::default(), &input(), caps()).is_err());
    p = pool();
    p.presets[0].cost.disk_bytes = 1;
    assert!(plan(&p, &State::default(), &input(), caps()).is_err());
}

#[test]
fn concurrent_pool_ticks_cannot_both_claim_last_global_headroom() {
    let root = std::env::temp_dir().join(format!("ow-sc-race-{}", common::nonce().unwrap()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("state.sqlite3");
    let db = Connection::open(&path).unwrap();
    initialize(&db).unwrap();
    drop(db);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let jobs: Vec<_> = (0..2)
        .map(|n| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut db = Connection::open(path).unwrap();
                db.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
                let mut p = pool();
                p.name = format!("pool{n}");
                let mut i = input();
                i.queued = vec![demand("q", shape(1, 1024, 16))];
                barrier.wait();
                let tx = db
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .unwrap();
                let r = record(
                    &tx,
                    &p,
                    &i,
                    limits::Liability {
                        cents: 399,
                        ..caps()
                    },
                )
                .unwrap();
                tx.commit().unwrap();
                r.decision
            })
        })
        .collect();
    let decisions: Vec<_> = jobs.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        decisions.iter().filter(|d| d.as_str() == "launch").count(),
        1
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn shadow_replans_changed_demand_after_cooldown_and_uncertain_drain_cannot_join_as_launch() {
    let p = pool();
    let mut i = input();
    i.queued = vec![demand("q", shape(1, 1024, 16))];
    let first = step(&p, &State::default(), &i);
    i.now = 131;
    i.queued = vec![demand("q", shape(4, 4096, 64))];
    let next = step(&p, &first.state, &i);
    assert_eq!(next.state.sequence, 2);
    assert_eq!(next.state.action.as_ref().unwrap().target, "large");
    let mut drain = next.state;
    let action = drain.action.as_mut().unwrap();
    action.kind = ActionKind::Drain;
    action.phase = Phase::Uncertain;
    action.target = "small".into();
    i.now += 1;
    i.runners = vec![runner("small", shape(2, 2048, 32), i.now)];
    i.receipt = Some(Receipt {
        action: action.id.clone(),
        outcome: Outcome::Joined,
        node: Some("small".into()),
    });
    assert!(step(&p, &drain, &i).state.action.is_some());
}
