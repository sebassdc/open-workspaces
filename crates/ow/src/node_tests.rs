//! Focused security/durability tests. Mock runtime evidence is distinct from real guest acceptance.
use crate::{common, nodes::*, wire};
fn root() -> PathBuf {
    let p = std::env::temp_dir().join(format!("ow-n-{}", common::nonce().unwrap()));
    fs::create_dir(&p).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
    p
}
fn enrollment(root: &Path, node: &str) -> Value {
    let path = root.join(format!("{node}.join"));
    mint(root, node, &path, 60, 512, 2).unwrap();
    let value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    enroll(root, value).unwrap()
}
#[test]
fn enrollment_atomic_expiring_scoped_private_and_revoked() {
    let root = root();
    let join = root.join("join");
    mint(&root, "alpha", &join, 60, 512, 2).unwrap();
    let v: Value = serde_json::from_slice(&fs::read(&join).unwrap()).unwrap();
    assert_eq!(v["secret"].as_str().unwrap().len(), 64);
    let mut threads = Vec::new();
    for _ in 0..8 {
        let root = root.clone();
        let v = v.clone();
        threads.push(std::thread::spawn(move || enroll(&root, v)));
    }
    let results = threads
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let credential = results.into_iter().find_map(Result::ok).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", credential["credential"].as_str().unwrap())
            .parse()
            .unwrap(),
    );
    assert!(auth(&root, "alpha", &headers).is_ok());
    assert!(auth(&root, "beta", &headers).is_err());
    assert!(enroll(&root, v).is_err());
    revoke(&root, "alpha").unwrap();
    assert!(auth(&root, "alpha", &headers).is_err());
    let join = root.join("expired");
    mint(&root, "beta", &join, 60, 512, 2).unwrap();
    db(&root)
        .unwrap()
        .execute(
            "UPDATE node_joins SET expires=?1 WHERE node='beta'",
            [now()],
        )
        .unwrap();
    let v = serde_json::from_slice(&fs::read(&join).unwrap()).unwrap();
    assert!(enroll(&root, v).is_err());
    fs::set_permissions(&join, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(private(&join).is_err());
    fs::set_permissions(&join, fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(&join, root.join("link")).unwrap();
    assert!(private(&root.join("link")).is_err());
    assert!(origin("http://192.0.2.1:1234", true).is_err());
    assert!(origin("http://localhost:1234", false).is_err());
    assert!(origin("http://localhost:1234", true).is_ok());
    assert!(origin("https://example.test/path", false).is_err());
    db(&root)
        .unwrap()
        .execute_batch("PRAGMA user_version=99")
        .unwrap();
    assert!(db(&root).is_err());
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn authenticated_handshake_refuses_redirect_and_times_out() {
    use std::net::TcpListener;
    let sink = TcpListener::bind("127.0.0.1:0").unwrap();
    sink.set_nonblocking(true).unwrap();
    let source = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", source.local_addr().unwrap());
    let sink_address = sink.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut tcp, _) = source.accept().unwrap();
        let mut data = [0; 4096];
        let n = tcp.read(&mut data).unwrap();
        assert!(
            String::from_utf8_lossy(&data[..n]).contains("Authorization: Bearer")
                || String::from_utf8_lossy(&data[..n])
                    .to_lowercase()
                    .contains("authorization: bearer")
        );
        write!(
            tcp,
            "HTTP/1.1 302 Found\r\nLocation: ws://{sink_address}/leak\r\nContent-Length: 0\r\n\r\n"
        )
        .unwrap();
    });
    assert!(socket(&origin, "/node/alpha", &"a".repeat(64), None).is_err());
    server.join().unwrap();
    assert!(sink.accept().is_err());
    let stalled = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", stalled.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (_tcp, _) = stalled.accept().unwrap();
        std::thread::sleep(Duration::from_secs(7));
    });
    let start = std::time::Instant::now();
    assert!(socket(&origin, "/node/alpha", &"a".repeat(64), None).is_err());
    assert!(start.elapsed() < Duration::from_secs(7));
    server.join().unwrap();
}
#[test]
fn durable_journal_duplicate_conflict_and_unknown_exec() {
    use std::os::unix::net::UnixListener;
    let root = root();
    let listener = UnixListener::bind(root.join("control.sock")).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let req = wire::line(&mut stream).unwrap();
        assert_eq!(req["command"], "effect");
        wire::send(
            &mut stream,
            &json!({"ok":true,"result":{"exit_code":0,"output":"once"}}),
        )
        .unwrap();
    });
    let req = json!({"op":"exec","id":"m-test","command":"effect","operation_key":"test"});
    let first = journal_request(&root, &req).unwrap();
    server.join().unwrap();
    assert_eq!(journal_request(&root, &req).unwrap(), first);
    let mut changed = req.clone();
    changed["command"] = json!("changed");
    assert!(journal_request(&root, &changed).is_err());
    let mut unknown = req.clone();
    unknown["operation_key"] = json!("unknown");
    let db = Connection::open(root.join("node-operations.sqlite3")).unwrap();
    db.execute(
        "INSERT INTO effects(key,request) VALUES('unknown',?1)",
        [unknown.to_string()],
    )
    .unwrap();
    assert_eq!(journal_request(&root, &unknown).unwrap()["uncertain"], true);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transport_auth_job_binding_generation_revocation_and_bounds() {
    let root = root();
    let alpha = enrollment(&root, "alpha");
    let beta = enrollment(&root, "beta");
    let c = Controller {
        root: root.clone(),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        jobs: Arc::new(Mutex::new(HashMap::new())),
        slots: Arc::new(Semaphore::new(32)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let proxy_loop = tokio::spawn(proxies(c.clone()));
    let app = routes(c.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let c1 = c.clone();
    let origin1 = origin.clone();
    let root1 = root.clone();
    tokio::task::spawn_blocking(move || {
        let a=alpha["credential"].as_str().unwrap();let b=beta["credential"].as_str().unwrap();
        assert!(socket(&origin1,"/node/alpha",b,None).is_err());assert!(socket(&origin1,"/job/alpha/guessed",a,None).is_err());
        let mut ws=socket(&origin1,"/node/alpha",a,None).unwrap();timeout_socket(&ws).unwrap();
        let heartbeat=json!({"type":"heartbeat","protocol":1,"backend":"firecracker","arch":"x86_64","runtime":"firecracker-v1.17.0","memory_mib":512,"slots":2,"vcpus":2,"images":["alpine"]}).to_string();ws.send(tungstenite::Message::Text(heartbeat.clone().into())).unwrap();
        let deadline=std::time::Instant::now();while online(&root1,"alpha").is_err(){assert!(deadline.elapsed()<Duration::from_secs(3));std::thread::sleep(Duration::from_millis(10));}
        let generation=c1.sessions.lock().unwrap()["alpha"].generation.clone();
        let (tx,_rx)=oneshot::channel();c1.jobs.lock().unwrap().insert("job".into(),Job{node:"alpha".into(),generation:generation.clone(),tx});
        assert!(socket(&origin1,"/job/beta/job",b,None).is_err());
        let mut attached=socket(&origin1,"/job/alpha/job",a,None).unwrap();let _=attached.close(None);assert!(socket(&origin1,"/job/alpha/job",a,None).is_err());
        let mut newer=socket(&origin1,"/node/alpha",a,None).unwrap();timeout_socket(&newer).unwrap();newer.send(tungstenite::Message::Text(heartbeat.into())).unwrap();
        let deadline=std::time::Instant::now();while c1.sessions.lock().unwrap()["alpha"].generation==generation {assert!(deadline.elapsed()<Duration::from_secs(3));std::thread::sleep(Duration::from_millis(10));}
        let (tx,_rx)=oneshot::channel();c1.jobs.lock().unwrap().insert("old".into(),Job{node:"alpha".into(),generation,tx});assert!(socket(&origin1,"/job/alpha/old",a,None).is_err());
        revoke(&root1,"alpha").unwrap();assert!(online(&root1,"alpha").is_err());assert!(socket(&origin1,"/node/alpha",a,None).is_err());
        let deadline=std::time::Instant::now();loop{match newer.read(){Err(e) if timed_out(&e)=>{assert!(deadline.elapsed()<Duration::from_secs(5));},Ok(tungstenite::Message::Ping(v))=>{let _=newer.send(tungstenite::Message::Pong(v));},_=>break}}
    }).await.unwrap();
    server.abort();
    proxy_loop.abort();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_proxy_fences_both_directions_and_revocation() {
    let root = root();
    let credentials = enrollment(&root, "alpha");
    let c = Controller {
        root: root.clone(),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        jobs: Arc::new(Mutex::new(HashMap::new())),
        slots: Arc::new(Semaphore::new(32)),
    };
    db(&root)
        .unwrap()
        .execute("UPDATE nodes SET heartbeat=?1 WHERE id='alpha'", [now()])
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = routes(c.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    for revoke_case in [false, true] {
        let (tx, mut rx) = mpsc::channel(16);
        c.sessions.lock().unwrap().insert(
            "alpha".into(),
            Session {
                generation: "original".into(),
                tx,
            },
        );
        let (mut local, remote) = std::os::unix::net::UnixStream::pair().unwrap();
        remote.set_nonblocking(true).unwrap();
        local
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let upstream = tokio::net::UnixStream::from_std(remote).unwrap();
        let proxy_task = tokio::spawn(proxy(c.clone(), "alpha".into(), upstream));
        let c1 = c.clone();
        let origin = origin.clone();
        let token = credentials["credential"].as_str().unwrap().to_owned();
        let root1 = root.clone();
        tokio::task::spawn_blocking(move || {
            wire::send(&mut local, &json!({"op":"terminal","id":"m-test"})).unwrap();
            let id = rx.blocking_recv().unwrap();
            let mut attached = socket(&origin, &format!("/job/alpha/{id}"), &token, None).unwrap();
            timeout_socket(&attached).unwrap();
            let first = attached.read().unwrap();
            assert!(matches!(first, tungstenite::Message::Binary(_)));
            if revoke_case {
                revoke(&root1, "alpha").unwrap();
            } else {
                c1.sessions
                    .lock()
                    .unwrap()
                    .get_mut("alpha")
                    .unwrap()
                    .generation = "replacement".into();
            }
            let _ = local.write_all(b"stale-input");
            let _ = attached.send(tungstenite::Message::Binary(
                b"stale-output".to_vec().into(),
            ));
            let deadline = std::time::Instant::now();
            loop {
                match attached.read() {
                    Ok(tungstenite::Message::Binary(_)) => {
                        panic!("stale input crossed the generation/revoke boundary")
                    }
                    Err(e) if timed_out(&e) => assert!(deadline.elapsed() < Duration::from_secs(5)),
                    _ => break,
                }
            }
            let mut bytes = [0; 12];
            match local.read(&mut bytes) {
                Ok(0) => {}
                Err(_) => {}
                Ok(_) => panic!("stale output crossed the generation/revoke boundary"),
            }
        })
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), proxy_task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }
    let permits = c.slots.clone().try_acquire_many_owned(32).unwrap();
    let (a, _b) = tokio::net::UnixStream::pair().unwrap();
    assert!(proxy(c.clone(), "alpha".into(), a).await.is_err());
    drop(permits);
    server.abort();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_oversized_frames_and_enrollment_are_bounded() {
    let root = root();
    let credentials = enrollment(&root, "alpha");
    let c = Controller {
        root: root.clone(),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        jobs: Arc::new(Mutex::new(HashMap::new())),
        slots: Arc::new(Semaphore::new(32)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = routes(c.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    assert_eq!(
        reqwest::Client::new()
            .post(format!("{origin}/enroll"))
            .header("content-type", "application/json")
            .body("x".repeat(5000))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let token = credentials["credential"].as_str().unwrap().to_owned();
    tokio::task::spawn_blocking(move || {
        for payload in ["malformed".to_owned(), "x".repeat(5000)] {
            let mut ws = socket(&origin, "/node/alpha", &token, None).unwrap();
            timeout_socket(&ws).unwrap();
            ws.send(tungstenite::Message::Text(payload.into())).unwrap();
            let deadline = std::time::Instant::now();
            loop {
                match ws.read() {
                    Ok(tungstenite::Message::Ping(v)) => {
                        let _ = ws.send(tungstenite::Message::Pong(v));
                    }
                    Err(e) if timed_out(&e) => assert!(deadline.elapsed() < Duration::from_secs(5)),
                    _ => break,
                }
            }
        }
    })
    .await
    .unwrap();
    server.abort();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_accepted_lost_reply_reopens_with_one_create_and_one_exec() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let root = root();
    let worker = root.join("worker");
    fs::create_dir(&worker).unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let creates = Arc::new(AtomicUsize::new(0));
    let effects = Arc::new(AtomicUsize::new(0));
    let listener = std::os::unix::net::UnixListener::bind(worker.join("control.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (stop1, creates1, effects1) = (stop.clone(), creates.clone(), effects.clone());
    let runtime = std::thread::spawn(move || {
        let mut machines = Vec::<Value>::new();
        while !stop1.load(Ordering::SeqCst) {
            let (mut stream, _) = match listener.accept() {
                Ok(s) => s,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
            };
            let req = wire::line(&mut stream).unwrap();
            let value = match req["op"].as_str().unwrap() {
                "list" => json!(machines),
                "snapshots" => json!([]),
                "stats" => json!({"running":{}}),
                "create" => {
                    creates1.fetch_add(1, Ordering::SeqCst);
                    let m = json!({"id":req["id"],"memory_mib":256,"vcpu_count":1,"image":"alpine","state":"running","source":null});
                    machines.push(m.clone());
                    m
                }
                "exec" => {
                    effects1.fetch_add(1, Ordering::SeqCst);
                    json!({"output":"once","exit_code":0})
                }
                _ => panic!("unexpected mock op"),
            };
            wire::send(&mut stream, &json!({"ok":true,"result":value})).unwrap();
        }
    });
    let credential = enrollment(&root, "alpha");
    let c = Controller {
        root: root.clone(),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        jobs: Arc::new(Mutex::new(HashMap::new())),
        slots: Arc::new(Semaphore::new(32)),
    };
    db(&root)
        .unwrap()
        .execute(
            "UPDATE nodes SET heartbeat=?1,capabilities=?2 WHERE id='alpha'",
            params![
                now(),
                json!({"memory_mib":512,"slots":2,"vcpus":2,"images":["alpine"]}).to_string()
            ],
        )
        .unwrap();
    let (tx, mut dispatch) = mpsc::channel(16);
    c.sessions.lock().unwrap().insert(
        "alpha".into(),
        Session {
            generation: "fixture".into(),
            tx,
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = routes(c.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let proxy_loop = tokio::spawn(proxies(c.clone()));
    let dispatcher = std::thread::spawn(move || {
        let token = credential["credential"].as_str().unwrap();
        let mut lost = std::collections::HashSet::new();
        while let Some(id) = dispatch.blocking_recv() {
            let mut ws = socket(&origin, &format!("/job/alpha/{id}"), token, None).unwrap();
            let mut bytes = Vec::new();
            while !bytes.contains(&b'\n') {
                match ws.read().unwrap() {
                    tungstenite::Message::Binary(b) => bytes.extend_from_slice(&b),
                    _ => panic!("invalid mock dispatch"),
                }
            }
            let req: Value = serde_json::from_slice(&bytes).unwrap();
            let result = journal_request(&worker, &req).unwrap();
            if matches!(req["op"].as_str(), Some("create" | "exec"))
                && lost.insert(req["operation_key"].as_str().unwrap().to_owned())
            {
                drop(ws);
                continue;
            }
            ws.send(tungstenite::Message::Binary(
                format!("{result}\n").into_bytes().into(),
            ))
            .unwrap();
            let _ = ws.close(None);
        }
    });
    let root1 = root.clone();
    tokio::task::spawn_blocking(move || {
        let deadline=std::time::Instant::now();while !root1.join("nodes/alpha/control.sock").exists(){assert!(deadline.elapsed()<Duration::from_secs(3));std::thread::sleep(Duration::from_millis(10));}
        let mut catalog=crate::catalog::Catalog::open(&root1,"test",None).unwrap();let user=catalog.user(&crate::catalog::Identity{issuer:"test".into(),subject:"owner".into(),email:"owner@example.test".into()}).unwrap();
        let create=json!({"op":"create","id":"demo","node":"alpha","memory_mib":256,"vcpu_count":1,"image":"alpine","operation_key":"lost-create"});let error=catalog.operation(user,create.clone()).unwrap_err();assert_eq!(error.downcast_ref::<crate::catalog::OperationFailure>().unwrap().0["state"],"uncertain");drop(catalog);
        let mut catalog=crate::catalog::Catalog::open(&root1,"test",None).unwrap();let result=catalog.operation(user,create).unwrap();assert_eq!(result["node"],"alpha");
        let exec=json!({"op":"exec","id":"demo","command":"effect","operation_key":"lost-exec"});let error=catalog.operation(user,exec.clone()).unwrap_err();assert_eq!(error.downcast_ref::<crate::catalog::OperationFailure>().unwrap().0["state"],"uncertain");drop(catalog);
        let mut catalog=crate::catalog::Catalog::open(&root1,"test",None).unwrap();assert_eq!(catalog.operation(user,exec).unwrap()["output"],"once");
    }).await.unwrap();
    assert_eq!(creates.load(Ordering::SeqCst), 1);
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    c.sessions.lock().unwrap().clear();
    dispatcher.join().unwrap();
    server.abort();
    proxy_loop.abort();
    stop.store(true, Ordering::SeqCst);
    runtime.join().unwrap();
    fs::remove_dir_all(root).unwrap();
}
