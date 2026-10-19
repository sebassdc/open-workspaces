use super::*;

// Drive the durable executor while calling the actual admission/wait entry point.
// Hold its runner lock so the production background launch cannot race the fixture RPCs.
fn with_wake_driver<T>(f: &Fixture, source: &RefCell<Peers>, action: impl FnOnce() -> T) -> T {
    let id: i64 = f
        .shared
        .lock()
        .unwrap()
        .db
        .query_row("SELECT COALESCE((SELECT t.id FROM transfers t JOIN transfer_locks l ON l.transfer=t.id WHERE t.kind='wake'),MAX(id)+1) FROM transfers", [], |r| r.get(0))
        .unwrap();
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(f.root.join(format!("transfer-{id}.lock")))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let state = Peers {
        source: source.borrow().source.clone(),
        cached: true,
        ..Default::default()
    };
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let stop = &stop;
        let driver = scope.spawn(move || {
            let peers = RefCell::new(state);
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                if stop.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let admitted=f.shared.lock().unwrap().db.query_row("SELECT EXISTS(SELECT 1 FROM transfers t JOIN operations o ON o.id=t.operation WHERE t.id=?1 AND o.owner=?2)",params![id,f.user],|r|r.get::<_,bool>(0)).unwrap();
                if admitted {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "real entry point never admitted a wake"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            for _ in 0..12 {
                if !tick(&f.shared, id, now(), &|n, r| {
                    wake_call(f, &peers, true, n, r)
                })
                .unwrap()
                {
                    return;
                }
            }
            panic!("wake did not finish")
        });
        let result = action();
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        driver.join().unwrap();
        result
    })
}
#[test]
fn actual_connection_and_start_entrypoints_admit_bounded_keys() {
    let identity = Identity {
        issuer: "test".into(),
        subject: "owner".into(),
        email: "owner@example.test".into(),
    };
    for permission in ["ssh", "exec", "terminal", "preview", "get", "put", "start"] {
        let (mut f, peers) = parked_fixture("hibernated");
        complete_park(&mut f, &peers, true);
        let result = with_wake_driver(&f, &peers, || {
            ensure_awake(
                f.shared.clone(),
                &identity,
                "box",
                permission,
                permission == "start",
            )
        });
        assert_eq!(
            result.unwrap().unwrap()["state"],
            "succeeded",
            "{permission}"
        );
        let c = f.shared.lock().unwrap();
        let key: String =
            c.db.query_row(
                "SELECT retry_key FROM operations WHERE op='wake'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        common::identifier(&key).unwrap();
        assert_eq!(
            c.db.query_row(
                "SELECT COUNT(*) FROM transfers WHERE kind='wake'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }
    let (mut f, peers) = parked_fixture("stopped");
    complete_park(&mut f, &peers, false);
    for permission in ["ssh", "exec", "terminal", "preview", "get", "put"] {
        assert!(
            ensure_awake(f.shared.clone(), &identity, "box", permission, false)
                .unwrap_err()
                .to_string()
                .contains("stopped")
        );
    }
    assert_eq!(
        with_wake_driver(&f, &peers, || ensure_awake(
            f.shared.clone(),
            &identity,
            "box",
            "start",
            true
        ))
        .unwrap()
        .unwrap()["state"],
        "succeeded"
    );
}
#[test]
fn actual_start_api_wakes_stopped_and_hibernated_boxes() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    for state in ["hibernated", "stopped"] {
        let (mut f, peers) = parked_fixture(state);
        complete_park(&mut f, &peers, state == "hibernated");
        let request = axum::extract::Request::builder()
            .method("POST")
            .uri("/api/operation")
            .header("x-ow-request", "dashboard")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"op":"start","id":"box","operation_key":"start-1"}"#,
            ))
            .unwrap();
        let response = with_wake_driver(&f, &peers, || {
            runtime.block_on(crate::dashboard::handle(
                Some(f.shared.clone()),
                None,
                Identity {
                    issuer: "test".into(),
                    subject: "owner".into(),
                    email: "owner@example.test".into(),
                },
                request,
                false,
                None,
            ))
        });
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = runtime
            .block_on(axum::body::to_bytes(response.into_body(), 1024 * 1024))
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["result"]["state"],
            "succeeded"
        );
    }
}
#[test]
fn owner_recovers_revoked_shared_wake_without_new_placement_or_reservation() {
    let (mut f, peers) = parked_fixture("hibernated");
    complete_park(&mut f, &peers, true);
    let (guest, wake) = {
        let mut c = f.shared.lock().unwrap();
        let guest = c
            .user(&Identity {
                issuer: "test".into(),
                subject: "guest".into(),
                email: "guest@example.test".into(),
            })
            .unwrap();
        c.operation(
            f.user,
            json!({"op":"share","id":"box","with":"guest@example.test","role":"use"}),
        )
        .unwrap();
        let wake = parking::admission(
            &c,
            guest,
            &json!({"op":"connect","id":"owner@example.test/box","operation_key":"guest-connect"}),
            Kind::Wake,
        )
        .unwrap();
        (guest, wake)
    };
    tick(&f.shared, wake.id, now(), &|_, _| panic!()).unwrap();
    assert!(
        tick(&f.shared, wake.id, now(), &|n, r| {
            let revoke = r["op"] == "capture-pull";
            let result = wake_call(&f, &peers, true, n, r)?;
            if revoke {
                f.shared
                    .lock()
                    .unwrap()
                    .operation(
                        f.user,
                        json!({"op":"unshare","id":"box","with":"guest@example.test"}),
                    )
                    .unwrap();
            }
            Ok(result)
        })
        .is_err()
    );
    f.restart();
    let resumed = {
        let c = f.shared.lock().unwrap();
        parking::admission(
            &c,
            f.user,
            &json!({"op":"wake","id":"box","operation_key":"owner-retry"}),
            Kind::Wake,
        )
        .unwrap()
    };
    assert_eq!(
        (resumed.id, resumed.epoch, resumed.target.clone()),
        (wake.id, wake.epoch, Some("source".into()))
    ); // original target comes from the admitted reservation
    assert_ne!(
        resumed.operation, wake.operation,
        "new owner authorization must not rewrite the revoked user's operation"
    );
    f.restart();
    for _ in 0..12 {
        if !tick(&f.shared, resumed.id, now(), &|n, r| {
            wake_call(&f, &peers, true, n, r)
        })
        .unwrap()
        {
            break;
        }
    }
    let c = f.shared.lock().unwrap();
    assert_eq!(c.transfer(resumed.id).unwrap().step, Step::Succeeded);
    assert!(c.machine(guest, "owner@example.test/box", "start").is_err());
    assert_eq!(
        c.db.query_row("SELECT COUNT(*) FROM transfer_locks", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        c.db.query_row("SELECT COUNT(*) FROM transfer_reservations", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(peers.borrow().starts, 1);
    assert_eq!(
        c.operation_info(guest, wake.operation).unwrap()["state"],
        "failed"
    );
}
#[test]
fn park_reuses_the_verified_capture_after_failed_restore() {
    let f = Fixture::new();
    fs::write(f.root.join("store-url"), "not-s3").unwrap();
    let peers = RefCell::new(Peers::default());
    run(&f, &peers, now()).unwrap();
    let park = {
        let c = f.shared.lock().unwrap();
        c.advance_transfer(
            f.transfer.id,
            f.transfer.epoch,
            Step::Exported,
            Step::Failed,
            &Evidence::default(),
        )
        .unwrap();
        parking::admission(
            &c,
            f.user,
            &json!({"op":"park","id":"box","operation_key":"park-retry"}),
            Kind::Park,
        )
        .unwrap()
    };
    tick(&f.shared, park.id, now(), &|n, r| {
        peers.borrow_mut().call(&f.shared, n, r)
    })
    .unwrap();
    tick(
        &f.shared,
        park.id,
        now(),
        &|n, r| match r["op"].as_str().unwrap() {
            "capture-published" => Ok(json!({"present":false})),
            "capture-push" => {
                assert_eq!(r["artifact"]["snapshot"], "transfer-1-source");
                let listener =
                    std::os::unix::net::UnixListener::bind(f.root.join("control.sock")).unwrap();
                let seen = peers.borrow().source.clone().unwrap();
                let worker = std::thread::spawn(move || {
                    let (mut socket, _) = listener.accept().unwrap();
                    assert_eq!(wire::line(&mut socket).unwrap()["op"], "inspect");
                    wire::send(&mut socket, &json!({"ok":true,"result":seen})).unwrap();
                });
                let error = capture_upload::job(&f.root, &r).unwrap_err();
                worker.join().unwrap();
                assert_eq!(
                    error.to_string(),
                    store::S3Objects::new("not-s3", None)
                        .err()
                        .unwrap()
                        .to_string(),
                    "actual publication job must pass its exact fence check"
                );
                Ok(publication(&r, false))
            }
            _ => peers.borrow_mut().call(&f.shared, n, r),
        },
    )
    .unwrap();
    tick(&f.shared, park.id, now(), &|n, r| {
        peers.borrow_mut().call(&f.shared, n, r)
    })
    .unwrap();
    tick(&f.shared, park.id, now(), &|_, _| panic!()).unwrap();
    let c = f.shared.lock().unwrap();
    assert_eq!(c.transfer(park.id).unwrap().step, Step::Succeeded);
    assert_eq!(
        c.db.query_row(
            "SELECT local_name FROM artifacts WHERE id=?1",
            [format!("park-{}", park.id)],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "transfer-1-source"
    );
}

#[test]
fn owner_resume_retains_uncertain_and_committed_effects_and_does_not_restore_access() {
    for stage in [
        Step::Preparing,
        Step::ImportUncertain,
        Step::Prepared,
        Step::Committed,
        Step::StartUncertain,
        Step::Started,
    ] {
        let (mut f, peers) = parked_fixture("hibernated");
        complete_park(&mut f, &peers, true);
        let guest = Identity {
            issuer: "test".into(),
            subject: "guest".into(),
            email: "guest@example.test".into(),
        };
        let (user, wake) = {
            let mut c = f.shared.lock().unwrap();
            let user = c.user(&guest).unwrap();
            c.operation(
                f.user,
                json!({"op":"share","id":"box","with":"guest@example.test","role":"use"}),
            )
            .unwrap();
            let wake=parking::admission(&c,user,&json!({"op":"connect","id":"owner@example.test/box","operation_key":"guest-connect"}),Kind::Wake).unwrap();
            (user, wake)
        };
        loop {
            let current = f.shared.lock().unwrap().transfer(wake.id).unwrap();
            if current.step == stage {
                break;
            }
            if stage == Step::ImportUncertain && current.step == Step::Importing {
                peers.borrow_mut().fault = Some(("import-prepare".into(), true));
                assert!(
                    tick(&f.shared, wake.id, now(), &|n, r| wake_call(
                        &f, &peers, true, n, r
                    ))
                    .is_err()
                );
                f.shared
                    .lock()
                    .unwrap()
                    .advance_transfer(
                        wake.id,
                        wake.epoch,
                        Step::Importing,
                        Step::ImportUncertain,
                        &Evidence::default(),
                    )
                    .unwrap();
                break;
            }
            tick(&f.shared, wake.id, now(), &|n, r| {
                wake_call(&f, &peers, true, n, r)
            })
            .unwrap();
        }
        let (old, request) = {
            let mut c = f.shared.lock().unwrap();
            let old = c.transfer(wake.id).unwrap();
            let request: String =
                c.db.query_row(
                    "SELECT request FROM operations WHERE id=?1",
                    [old.operation],
                    |r| r.get(0),
                )
                .unwrap();
            c.operation(
                f.user,
                json!({"op":"unshare","id":"box","with":"guest@example.test"}),
            )
            .unwrap();
            (old, request)
        };
        f.restart();
        let owner = {
            let c = f.shared.lock().unwrap();
            parking::admission(
                &c,
                f.user,
                &json!({"op":"wake","id":"box","operation_key":"owner-resume"}),
                Kind::Wake,
            )
            .unwrap()
        };
        assert_eq!(
            (owner.id, owner.epoch, owner.target.clone(), owner.step),
            (old.id, old.epoch, old.target.clone(), old.step)
        );
        assert_ne!(owner.operation, old.operation);
        f.restart();
        for _ in 0..12 {
            if !tick(&f.shared, owner.id, now(), &|n, r| {
                wake_call(&f, &peers, true, n, r)
            })
            .unwrap()
            {
                break;
            }
        }
        let c = f.shared.lock().unwrap();
        assert_eq!(
            c.transfer(owner.id).unwrap().step,
            Step::Succeeded,
            "{stage:?}"
        );
        assert_eq!(peers.borrow().starts, 1);
        assert!(c.machine(user, "owner@example.test/box", "start").is_err());
        assert_eq!(
            c.db.query_row(
                "SELECT request FROM operations WHERE id=?1",
                [old.operation],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            request
        );
        let replay = parking::admission(
            &c,
            f.user,
            &json!({"op":"wake","id":"box","operation_key":"owner-resume"}),
            Kind::Wake,
        )
        .unwrap();
        assert_eq!(replay.operation, owner.operation);
        assert_eq!(
            c.db.query_row("SELECT COUNT(*) FROM transfer_reservations", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
#[test]
fn resume_requires_owner_current_policy_and_matching_cold_consent() {
    let (mut f, peers) = parked_fixture("hibernated");
    complete_park(&mut f, &peers, true);
    let mut c = f.shared.lock().unwrap();
    let guest = c
        .user(&Identity {
            issuer: "test".into(),
            subject: "guest".into(),
            email: "guest@example.test".into(),
        })
        .unwrap();
    let other = c
        .user(&Identity {
            issuer: "test".into(),
            subject: "other".into(),
            email: "other@example.test".into(),
        })
        .unwrap();
    for email in ["guest@example.test", "other@example.test"] {
        c.operation(
            f.user,
            json!({"op":"share","id":"box","with":email,"role":"use"}),
        )
        .unwrap();
    }
    let wake=parking::admission(&c,guest,&json!({"op":"wake","id":"owner@example.test/box","operation_key":"guest-cold","accept_cold":true}),Kind::Wake).unwrap();
    c.operation(
        f.user,
        json!({"op":"unshare","id":"box","with":"guest@example.test"}),
    )
    .unwrap();
    assert!(parking::admission(&c,other,&json!({"op":"wake","id":"owner@example.test/box","operation_key":"not-owner","accept_cold":true}),Kind::Wake).is_err());
    assert!(
        parking::admission(
            &c,
            f.user,
            &json!({"op":"wake","id":"box","operation_key":"no-consent"}),
            Kind::Wake
        )
        .is_err()
    );
    c.db.execute(
        "UPDATE resources SET ssh_policy_generation=ssh_policy_generation+1",
        [],
    )
    .unwrap();
    assert!(
        parking::admission(
            &c,
            f.user,
            &json!({"op":"wake","id":"box","operation_key":"changed-policy","accept_cold":true}),
            Kind::Wake
        )
        .is_err()
    );
    assert_eq!(c.transfer(wake.id).unwrap().operation, wake.operation);
    assert_eq!(
        c.db.query_row(
            "SELECT COUNT(*) FROM transfers WHERE kind='wake'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}
#[test]
fn retry_repairs_an_unpublished_park_capture_binding_after_restart() {
    let mut f = Fixture::new();
    fs::write(f.root.join("store-url"), "s3://bucket/root").unwrap();
    let peers = RefCell::new(Peers::default());
    run(&f, &peers, now()).unwrap();
    let park = {
        let c = f.shared.lock().unwrap();
        c.advance_transfer(
            f.transfer.id,
            f.transfer.epoch,
            Step::Exported,
            Step::Failed,
            &Evidence::default(),
        )
        .unwrap();
        parking::admission(
            &c,
            f.user,
            &json!({"op":"park","id":"box","operation_key":"park-retry"}),
            Kind::Park,
        )
        .unwrap()
    };
    tick(&f.shared, park.id, now(), &|n, r| {
        peers.borrow_mut().call(&f.shared, n, r)
    })
    .unwrap();
    {
        let c = f.shared.lock().unwrap();
        let artifact = format!("park-{}", park.id);
        c.db.execute("INSERT INTO artifacts(id,kind,owner,machine,local_name,state,attempt,store_url) VALUES(?1,'capture',?2,'physical',?3,'uploading',?4,'s3://bucket/root')",params![artifact,f.user,format!("transfer-{}-source",park.id),"c".repeat(32)]).unwrap();
        c.db.execute(
            "INSERT INTO leases(artifact,holder,purpose,token,expires) VALUES(?1,?2,'push',?3,?4)",
            params![artifact, name(&park), "c".repeat(32), now() + 7200],
        )
        .unwrap();
        c.db.execute(
            "UPDATE transfers SET capture=?1 WHERE id=?2",
            params![artifact, park.id],
        )
        .unwrap();
    }
    f.restart();
    tick(&f.shared, park.id, now(), &|n, r| {
        if r["op"] == "capture-push" {
            assert_eq!(r["artifact"]["snapshot"], "transfer-1-source");
            assert_ne!(r["artifact"]["reference"]["attempt"], "c".repeat(32));
        }
        park_call(&f, &peers, false, n, r)
    })
    .unwrap();
    f.restart();
    tick(&f.shared, park.id, now(), &|n, r| {
        park_call(&f, &peers, false, n, r)
    })
    .unwrap();
    tick(&f.shared, park.id, now(), &|_, _| panic!()).unwrap();
    let c = f.shared.lock().unwrap();
    assert_eq!(c.transfer(park.id).unwrap().step, Step::Succeeded);
    assert_eq!(
        c.db.query_row(
            "SELECT COUNT(*) FROM leases WHERE purpose='push'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    assert!(
        c.db.query_row(
            "SELECT node='' AND current_capture IS NOT NULL FROM resources",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
}

#[test]
fn owner_connection_can_resume_the_revoked_wake() {
    let (mut f, peers) = parked_fixture("hibernated");
    complete_park(&mut f, &peers, true);
    {
        let mut c = f.shared.lock().unwrap();
        let guest = c
            .user(&Identity {
                issuer: "test".into(),
                subject: "guest".into(),
                email: "guest@example.test".into(),
            })
            .unwrap();
        c.operation(
            f.user,
            json!({"op":"share","id":"box","with":"guest@example.test","role":"use"}),
        )
        .unwrap();
        parking::admission(
            &c,
            guest,
            &json!({"op":"connect","id":"owner@example.test/box","operation_key":"guest-wake"}),
            Kind::Wake,
        )
        .unwrap();
        c.operation(
            f.user,
            json!({"op":"unshare","id":"box","with":"guest@example.test"}),
        )
        .unwrap();
    }
    let result = with_wake_driver(&f, &peers, || {
        ensure_awake(
            f.shared.clone(),
            &Identity {
                issuer: "test".into(),
                subject: "owner".into(),
                email: "owner@example.test".into(),
            },
            "box",
            "ssh",
            false,
        )
    });
    assert_eq!(result.unwrap().unwrap()["state"], "succeeded");
    assert_eq!(
        f.shared
            .lock()
            .unwrap()
            .db
            .query_row(
                "SELECT COUNT(*) FROM transfers WHERE kind='wake'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn interrupted_export_replays_exact_intent_after_worker_and_catalog_restarts() {
    for kind in [Kind::Park, Kind::Restore] {
        for phase in ["exporting", "captured"] {
            let (mut f, peers) = if kind == Kind::Park {
                parked_fixture("hibernated")
            } else {
                (Fixture::new(), RefCell::new(Peers::default()))
            };
            let placement = if kind == Kind::Park {
                peers.borrow().source.as_ref().unwrap()["placement"].clone()
            } else {
                json!(authority(&f.transfer, "source"))
            };
            peers.borrow_mut().source = Some(json!({
                "id":"physical", "memory_mib":256,"vcpu_count":1,"disk_bytes":4,
                "state":"exporting","vmm_running":false,"placement":placement,
                "lifecycle":{"phase":phase,"intent":{"transfer":name(&f.transfer),
                    "capture":format!("{}-source",name(&f.transfer)),"disk_only":false}}
            }));
            // Export can fail repeatedly while its persisted identity remains resumable.
            for _ in 0..2 {
                f.restart();
                peers.borrow_mut().fault = Some(("export".into(), false));
                let error = tick(&f.shared, f.transfer.id, now(), &|n, r| {
                    park_call(&f, &peers, true, n, r)
                })
                .unwrap_err();
                assert!(
                    error.to_string().contains("agent killed before effect"),
                    "{kind:?}/{phase}: {error:#}"
                );
                assert!(peers.borrow().fault.is_none(), "export was never replayed");
                assert_eq!(f.step(), Step::Preparing);
            }
            f.restart();
            assert!(
                tick(&f.shared, f.transfer.id, now(), &|n, r| park_call(
                    &f, &peers, true, n, r
                ))
                .unwrap()
            );
            assert_eq!(f.step(), Step::Exported);
            assert_eq!(peers.borrow().starts, 0);
            assert_eq!(peers.borrow().keys.len(), 1);
            assert_eq!(
                peers.borrow().keys.values().next().unwrap()["operation_key"],
                format!("{}-export", name(&f.transfer))
            );
        }
    }
}

fn start_api(
    runtime: &tokio::runtime::Runtime,
    f: &Fixture,
    address: &str,
    key: &str,
) -> (axum::http::StatusCode, Value) {
    let request = axum::extract::Request::builder()
        .method("POST")
        .uri("/api/operation")
        .header("x-ow-request", "dashboard")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({"op":"start","id":address,"operation_key":key}).to_string(),
        ))
        .unwrap();
    let response = runtime.block_on(crate::dashboard::handle(
        Some(f.shared.clone()),
        None,
        Identity {
            issuer: "test".into(),
            subject: "owner".into(),
            email: "owner@example.test".into(),
        },
        request,
        false,
        None,
    ));
    let status = response.status();
    let body = runtime
        .block_on(axum::body::to_bytes(response.into_body(), 1024 * 1024))
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[test]
fn start_retry_key_survives_a_stop_second_park_and_catalog_restart() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    for coalesced in [false, true] {
        let (mut f, peers) = parked_fixture(if coalesced { "hibernated" } else { "stopped" });
        complete_park(&mut f, &peers, coalesced);
        if coalesced {
            admit_wake(&mut f, false);
        }
        let (status, first) = with_wake_driver(&f, &peers, || {
            start_api(&runtime, &f, "box", "caller-start-1")
        });
        assert_eq!(status, axum::http::StatusCode::OK, "{first}");
        assert_eq!(first["result"]["state"], "succeeded");
        // Model a positively acknowledged intentional worker stop at the committed placement.
        let (node, stopped) = {
            let c = f.shared.lock().unwrap();
            let placement = c.managed_placement("physical").unwrap().unwrap();
            let stopped = json!({"id":"physical","state":"stopped","memory_mib":256,"vcpu_count":1,"disk_bytes":4,
                "placement":placement,"vmm_running":false,"lifecycle":{"phase":"active","intent":null}});
            c.db.execute("UPDATE resources SET metadata=?1,reserved_mib=0,reserved_cpus=0 WHERE physical='physical'",[stopped.to_string()]).unwrap();
            (placement.node, stopped)
        };
        if node == "source" {
            peers.borrow_mut().source = Some(stopped);
        } else {
            peers.borrow_mut().target = Some(stopped);
        }
        f.transfer = {
            let c = f.shared.lock().unwrap();
            parking::admission(
                &c,
                f.user,
                &json!({"op":"park","id":"box","operation_key":"park-again"}),
                Kind::Park,
            )
            .unwrap()
        };
        complete_park(&mut f, &peers, false);
        f.restart();
        let (status, replayed) = with_wake_driver(&f, &peers, || {
            start_api(&runtime, &f, "box", "caller-start-1")
        });
        assert_eq!(status, axum::http::StatusCode::OK, "{replayed}");
        assert_eq!(
            replayed["result"]["id"], first["result"]["id"],
            "retry must replay the original start"
        );
        let c = f.shared.lock().unwrap();
        assert!(c.db.query_row("SELECT node='' AND json_extract(metadata,'$.state')='stopped' FROM resources WHERE physical='physical'",[],|r|r.get::<_,bool>(0)).unwrap());
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
            c.db.query_row(
                "SELECT COUNT(*) FROM operations WHERE owner=?1 AND retry_key='caller-start-1'",
                [f.user],
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
    }
}

#[test]
fn interrupted_export_rejects_mismatched_or_unknown_recovery_before_rpc() {
    for phase in ["exporting", "captured"] {
        for field in [
            "transfer",
            "capture",
            "disk_only",
            "placement",
            "state",
            "vmm_running",
            "phase",
            "id",
            "disk_bytes",
        ] {
            let f = Fixture::new();
            let placement = authority(&f.transfer, "source");
            let mut seen = json!({"id":"physical","memory_mib":256,"vcpu_count":1,"disk_bytes":4,
                "state":"exporting","vmm_running":false,"placement":placement,
                "lifecycle":{"phase":phase,"intent":{"transfer":name(&f.transfer),"capture":format!("{}-source",name(&f.transfer)),"disk_only":false}}});
            match field {
                "transfer" | "capture" => seen["lifecycle"]["intent"][field] = json!("other"),
                "disk_only" => seen["lifecycle"]["intent"][field] = json!(true),
                "placement" => seen["placement"]["epoch"] = json!(999),
                "state" => seen[field] = json!("running"),
                "vmm_running" => seen[field] = json!(true),
                "phase" => seen["lifecycle"][field] = json!("unknown"),
                "id" => seen[field] = json!("other"),
                "disk_bytes" => seen[field] = json!(5),
                _ => unreachable!(),
            }
            let calls = std::cell::Cell::new(0);
            let result = tick(&f.shared, f.transfer.id, now(), &|node, r| {
                calls.set(calls.get() + 1);
                assert_eq!(node, "source");
                assert_eq!(
                    r["op"], "inspect",
                    "{phase}/{field}: mutation must be refused"
                );
                Ok(seen.clone())
            });
            assert!(result.is_err(), "{phase}/{field}");
            assert_eq!(calls.get(), 1);
            assert_eq!(f.step(), Step::Preparing);
        }
    }
}

#[test]
fn concurrent_explicit_start_keys_join_one_wake_and_keep_independent_retry_history() {
    let (mut f, peers) = parked_fixture("hibernated");
    complete_park(&mut f, &peers, true);
    let wake = admit_wake(&mut f, false);
    std::thread::scope(|s| {
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let shared = f.shared.clone();
                let user = f.user;
                s.spawn(move || {
                    let c = shared.lock().unwrap();
                    parking::admission(
                        &c,
                        user,
                        &json!({"op":"start","id":"box","operation_key":format!("caller-{i}")}),
                        Kind::Wake,
                    )
                    .unwrap()
                    .id
                })
            })
            .collect();
        for thread in threads {
            assert_eq!(thread.join().unwrap(), wake.id);
        }
    });
    f.restart();
    {
        let c = f.shared.lock().unwrap();
        for i in 0..8 {
            let key = format!("caller-{i}");
            let alias: i64 =
                c.db.query_row(
                    "SELECT id FROM operations WHERE retry_key=?1",
                    [&key],
                    |r| r.get(0),
                )
                .unwrap();
            let status = c.operation_info(f.user, alias).unwrap();
            assert_eq!(status["state"], "uncertain");
            assert_eq!(status["transfer"]["id"], wake.id);
            assert_eq!(status["id"], alias);
            assert_eq!(
                parking::admission(
                    &c,
                    f.user,
                    &json!({"op":"start","id":"box","operation_key":key}),
                    Kind::Wake
                )
                .unwrap()
                .id,
                wake.id
            );
            assert!(
                parking::admission(
                    &c,
                    f.user,
                    &json!({"op":"wake","id":"box","operation_key":key,"accept_cold":true}),
                    Kind::Wake
                )
                .is_err()
            );
        }
    }
    assert!(tick(&f.shared, wake.id, now(), &|_, _| panic!()).unwrap());
    {
        let c = f.shared.lock().unwrap();
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
            1
        );
    }
    while tick(&f.shared, wake.id, now(), &|n, r| {
        wake_call(&f, &peers, true, n, r)
    })
    .unwrap()
    {
        f.restart();
    }
    let c = f.shared.lock().unwrap();
    assert_eq!(peers.borrow().starts, 1);
    let alias =
        c.db.query_row(
            "SELECT id FROM operations WHERE retry_key='caller-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        c.operation_info(f.user, alias).unwrap()["state"],
        "succeeded"
    );
    assert_eq!(
        parking::admission(
            &c,
            f.user,
            &json!({"op":"start","id":"box","operation_key":"caller-1"}),
            Kind::Wake
        )
        .unwrap()
        .id,
        wake.id
    );
}

#[test]
fn owner_can_resume_revoked_wake_using_an_existing_joined_start_key() {
    let (mut f, peers) = parked_fixture("hibernated");
    complete_park(&mut f, &peers, true);
    let (guest, wake, alias) = {
        let mut c = f.shared.lock().unwrap();
        let guest = c
            .user(&Identity {
                issuer: "test".into(),
                subject: "guest".into(),
                email: "guest@example.test".into(),
            })
            .unwrap();
        c.operation(
            f.user,
            json!({"op":"share","id":"box","with":"guest@example.test","role":"use"}),
        )
        .unwrap();
        let wake = parking::admission(
            &c,
            guest,
            &json!({"op":"connect","id":"owner@example.test/box","operation_key":"shared-wake"}),
            Kind::Wake,
        )
        .unwrap();
        assert_eq!(
            parking::admission(
                &c,
                f.user,
                &json!({"op":"start","id":"box","operation_key":"owner-start"}),
                Kind::Wake
            )
            .unwrap()
            .id,
            wake.id
        );
        let alias =
            c.db.query_row(
                "SELECT id FROM operations WHERE owner=?1 AND retry_key='owner-start'",
                [f.user],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        (guest, wake, alias)
    };
    for _ in 0..2 {
        assert!(
            tick(&f.shared, wake.id, now(), &|n, r| wake_call(
                &f, &peers, true, n, r
            ))
            .unwrap()
        );
    }
    peers.borrow_mut().fault = Some(("import-prepare".into(), true));
    assert!(
        tick(&f.shared, wake.id, now(), &|n, r| wake_call(
            &f, &peers, true, n, r
        ))
        .is_err()
    );
    let target = {
        let mut c = f.shared.lock().unwrap();
        c.advance_transfer(
            wake.id,
            wake.epoch,
            Step::Importing,
            Step::ImportUncertain,
            &Evidence::default(),
        )
        .unwrap();
        c.operation(
            f.user,
            json!({"op":"unshare","id":"box","with":"guest@example.test"}),
        )
        .unwrap();
        c.transfer(wake.id).unwrap().target.unwrap()
    };
    f.restart();
    {
        let c = f.shared.lock().unwrap();
        let resumed = parking::admission(
            &c,
            f.user,
            &json!({"op":"start","id":"box","operation_key":"owner-start"}),
            Kind::Wake,
        )
        .unwrap();
        assert_eq!(resumed.id, wake.id);
        assert_eq!(resumed.operation, alias);
        assert_eq!(resumed.epoch, wake.epoch);
        assert_eq!(resumed.step, Step::ImportUncertain);
        assert_eq!(resumed.target.as_deref(), Some(target.as_str()));
        assert_eq!(
            c.db.query_row(
                "SELECT COUNT(*) FROM transfer_reservations WHERE transfer=?1 AND node=?2",
                params![wake.id, target],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert!(
            parking::admission(
                &c,
                guest,
                &json!({"op":"start","id":"owner@example.test/box","operation_key":"denied"}),
                Kind::Wake
            )
            .is_err()
        );
    }
    while tick(&f.shared, wake.id, now(), &|n, r| {
        wake_call(&f, &peers, true, n, r)
    })
    .unwrap()
    {
        f.restart();
    }
    let c = f.shared.lock().unwrap();
    assert_eq!(c.transfer(wake.id).unwrap().step, Step::Succeeded);
    assert_eq!(peers.borrow().starts, 1);
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
        c.operation_info(f.user, alias).unwrap()["state"],
        "succeeded"
    );
}

#[test]
fn start_retry_replays_placed_history_after_park_without_admitting_wake() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (mut f, peers) = parked_fixture("stopped");
    complete_park(&mut f, &peers, false);
    let original = json!({"id":"physical","state":"running","original_start":true});
    {
        let c = f.shared.lock().unwrap();
        c.db.execute("INSERT INTO operations(owner,op,resource,state,node,retry_key,request,response) VALUES(?1,'start','box','succeeded','source','placed-start',?2,?3)",params![f.user,json!({"request":{"op":"start","id":"physical"},"node":"source"}).to_string(),original.to_string()]).unwrap();
    }
    f.restart();
    let (status, replayed) = start_api(&runtime, &f, "box", "placed-start");
    assert_eq!(status, axum::http::StatusCode::OK, "{replayed}");
    assert_eq!(replayed["result"], original);
    let c = f.shared.lock().unwrap();
    assert!(c.db.query_row("SELECT node='' AND json_extract(metadata,'$.state')='stopped' FROM resources WHERE physical='physical'",[],|r|r.get::<_,bool>(0)).unwrap());
    assert_eq!(
        c.db.query_row(
            "SELECT COUNT(*) FROM transfers WHERE kind='wake'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn wake_skips_incompatible_runner_when_compatible_runner_exists() {
    for field in ["runtime", "cpu", "kernel_sha256", "missing"] {
        let (mut f, peers) = parked_fixture("hibernated");
        complete_park(&mut f, &peers, true);
        let mut wrong = json!({"runtime":"same","cpu":"right","kernel_sha256":"same"});
        if field == "missing" {
            wrong = Value::Null;
        } else {
            wrong[field] = json!("wrong");
        }
        f.shared.lock().unwrap().db.execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.compat',json(?1)) WHERE id='source'",[wrong.to_string()]).unwrap();
        f.restart();
        let wake = admit_wake(&mut f, false);
        assert!(
            tick(&f.shared, wake.id, now(), &|_, _| panic!(
                "no RPC during reservation"
            ))
            .unwrap()
        );
        {
            let c = f.shared.lock().unwrap();
            assert_eq!(
                c.transfer(wake.id).unwrap().target.as_deref(),
                Some("target"),
                "{field}"
            );
            assert_eq!(c.db.query_row("SELECT count(*) FROM transfer_reservations WHERE transfer=?1 AND node='target'",[wake.id],|r|r.get::<_,i64>(0)).unwrap(),1);
        }
        f.restart();
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
        assert_eq!(
            c.transfer(wake.id).unwrap().step,
            Step::Succeeded,
            "{field}"
        );
        assert_eq!(peers.borrow().starts, 1);
        assert_eq!(peers.borrow().pulls.len(), 1);
        assert!(
            c.operation_info(f.user, f.transfer.operation).unwrap()["capture_manifest"].is_null()
        );
    }
}

#[test]
fn compatible_draining_runner_queues_without_reservation_and_retries_same_operation() {
    let (mut f, peers) = parked_fixture("hibernated");
    complete_park(&mut f, &peers, true);
    {
        let c = f.shared.lock().unwrap();
        c.db.execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.compat',json(?1),'$.drain',id='target')",[json!({"runtime":"same","cpu":"wrong","kernel_sha256":"same"}).to_string()]).unwrap();
        c.db.execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.compat',json(?1)) WHERE id='target'",[json!({"runtime":"same","cpu":"right","kernel_sha256":"same"}).to_string()]).unwrap();
    }
    let wake = admit_wake(&mut f, false);
    for _ in 0..2 {
        assert!(
            !tick(&f.shared, wake.id, now(), &|_, _| panic!(
                "no RPC before an eligible reservation"
            ))
            .unwrap()
        );
        f.restart();
        let c = f.shared.lock().unwrap();
        assert_eq!(c.transfer(wake.id).unwrap().step, Step::Preparing);
        assert!(c.transfer(wake.id).unwrap().target.is_none());
        assert!(c.operation_info(f.user, wake.operation).unwrap()["queued"].is_string());
        assert_eq!(
            c.db.query_row("SELECT count(*) FROM transfer_reservations", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            parking::admission(
                &c,
                f.user,
                &json!({"op":"connect","id":"box","operation_key":"join-queued"}),
                Kind::Wake
            )
            .unwrap()
            .id,
            wake.id
        );
    }
    f.shared.lock().unwrap().db.execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.compat',json(?1),'$.drain',0) WHERE id='target'",[json!({"runtime":"same","cpu":"right","kernel_sha256":"same"}).to_string()]).unwrap();
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
        c.transfer(wake.id).unwrap().target.as_deref(),
        Some("target")
    );
    assert_eq!(peers.borrow().starts, 1);
    assert_eq!(
        c.db.query_row(
            "SELECT count(*) FROM transfers WHERE kind='wake'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn disk_only_or_explicit_cold_wake_does_not_require_a_ram_match() {
    for ram in [false, true] {
        let (mut f, peers) = parked_fixture(if ram { "hibernated" } else { "stopped" });
        complete_park(&mut f, &peers, ram);
        let wake = {
            let c = f.shared.lock().unwrap();
            c.db.execute(
                "UPDATE nodes SET capabilities=json_remove(capabilities,'$.compat')",
                [],
            )
            .unwrap();
            parking::admission(
                &c,
                f.user,
                &json!({"op":"wake","id":"box","operation_key":"cold-or-disk","accept_cold":ram}),
                Kind::Wake,
            )
            .unwrap()
        };
        assert!(
            tick(&f.shared, wake.id, now(), &|_, _| panic!(
                "no RPC during reservation"
            ))
            .unwrap()
        );
        let c = f.shared.lock().unwrap();
        assert_eq!(
            c.transfer(wake.id).unwrap().target.as_deref(),
            Some("source")
        );
        assert!(
            c.db.query_row(
                "SELECT node='' AND current_capture=?1 FROM resources WHERE physical='physical'",
                [wake.capture],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
        );
        assert_eq!(peers.borrow().starts, 0);
    }
}

#[test]
fn changed_saved_manifest_cannot_admit_a_wake_reservation() {
    let (mut f, peers) = parked_fixture("hibernated");
    complete_park(&mut f, &peers, true);
    f.shared.lock().unwrap().db.execute("UPDATE operations SET response=json_set(response,'$.capture_manifest.created_unix',123) WHERE id=?1",[f.transfer.operation]).unwrap();
    let wake = admit_wake(&mut f, false);
    let error = tick(&f.shared, wake.id, now(), &|_, _| {
        panic!("no RPC with changed manifest")
    })
    .unwrap_err();
    assert!(
        error.to_string().contains("manifest digest differs"),
        "{error:#}"
    );
    let c = f.shared.lock().unwrap();
    assert_eq!(c.transfer(wake.id).unwrap().step, Step::Preparing);
    assert_eq!(
        c.db.query_row("SELECT count(*) FROM transfer_reservations", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(peers.borrow().starts, 0);
}

#[test]
fn no_compatible_runner_fails_before_pull_and_allows_explicit_cold_retry() {
    let (mut f, peers) = parked_fixture("hibernated");
    complete_park(&mut f, &peers, true);
    f.shared
        .lock()
        .unwrap()
        .db
        .execute(
            "UPDATE nodes SET capabilities=json_remove(capabilities,'$.compat')",
            [],
        )
        .unwrap();
    let warm = admit_wake(&mut f, false);
    assert!(
        !tick(&f.shared, warm.id, now(), &|_, _| panic!(
            "no RPC to an incompatible runner"
        ))
        .unwrap()
    );
    f.restart();
    let cold = {
        let c = f.shared.lock().unwrap();
        assert_eq!(c.transfer(warm.id).unwrap().step, Step::Failed);
        assert!(
            c.operation_info(f.user, warm.operation).unwrap()["error"]
                .as_str()
                .unwrap()
                .contains("--accept-cold")
        );
        assert_eq!(
            c.db.query_row("SELECT count(*) FROM transfer_reservations", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(
            c.db.query_row(
                "SELECT node='' AND current_capture=?1 FROM resources WHERE physical='physical'",
                [&warm.capture],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
        );
        parking::admission(&c,f.user,&json!({"op":"wake","id":"box","operation_key":"explicit-cold-retry","accept_cold":true}),Kind::Wake).unwrap()
    };
    assert_ne!(cold.id, warm.id);
    assert_eq!(peers.borrow().starts, 0);
    for _ in 0..12 {
        if !tick(&f.shared, cold.id, now(), &|n, r| {
            wake_call(&f, &peers, true, n, r)
        })
        .unwrap()
        {
            break;
        }
    }
    let c = f.shared.lock().unwrap();
    assert_eq!(c.transfer(cold.id).unwrap().step, Step::Succeeded);
    assert_eq!(peers.borrow().starts, 1);
    assert_eq!(
        c.db.query_row(
            "SELECT state FROM artifacts WHERE id=?1",
            [&warm.capture],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "stored"
    );
}
