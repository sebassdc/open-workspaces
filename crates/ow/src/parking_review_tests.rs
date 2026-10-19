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
