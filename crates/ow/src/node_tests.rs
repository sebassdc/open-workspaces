//! Focused security/durability tests. Mock runtime evidence is distinct from real guest acceptance.
use crate::{common, nodes::*, wire};

#[test]
fn heartbeats_advertise_aws_metadata_and_disk_only_recovery_independently() {
    let root = root();
    for metadata in [false, true] {
        for recovery in [false, true] {
            let status = json!({"aws_metadata_v1":metadata,"recovery_disk_only_v1":recovery});
            let beat = build_beat(&json!({"images":[]}), &status, &root, true, false).unwrap();
            assert_eq!(beat["aws_metadata_v1"], metadata);
            assert_eq!(beat["recovery_disk_only_v1"], recovery);
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn node_stream_100_mib_each_way() {
    use std::{
        net::{Shutdown, TcpListener},
        os::unix::net::UnixStream,
    };
    const BYTES: usize = 100 * 1024 * 1024;
    const CHUNK: usize = 16384;
    for upload in [true, false] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (mut worker, unix) = UnixStream::pair().unwrap();
        let node = std::thread::spawn(move || {
            let ws = socket(&origin, "/job/test", "fixture", None).unwrap();
            timeout_socket(&ws).unwrap();
            agent_stream(ws, unix).unwrap();
        });
        let (tcp, _) = listener.accept().unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        tcp.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        tcp.set_nodelay(true).unwrap();
        let watchdog = tcp.try_clone().unwrap();
        let (done, wait) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            if matches!(
                wait.recv_timeout(Duration::from_secs(30)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                let _ = watchdog.shutdown(Shutdown::Both);
            }
        });
        let mut controller = tungstenite::accept(tcp).unwrap();
        let guest = std::thread::spawn(move || {
            if upload {
                let mut count = 0;
                let mut buf = [0; CHUNK];
                while count < BYTES {
                    let n = worker.read(&mut buf).unwrap();
                    assert!(n > 0);
                    assert!(buf[..n].iter().all(|b| *b == 0xa5));
                    count += n;
                }
                assert_eq!(count, BYTES);
                worker.write_all(&count.to_be_bytes()).unwrap();
            } else {
                for _ in 0..BYTES / CHUNK {
                    worker.write_all(&[0x5a; CHUNK]).unwrap();
                }
            }
        });
        let start = std::time::Instant::now();
        if upload {
            for _ in 0..BYTES / CHUNK {
                controller
                    .send(tungstenite::Message::Binary(vec![0xa5; CHUNK].into()))
                    .unwrap();
            }
            let count = controller.read().unwrap().into_data();
            assert_eq!(count.as_ref(), &BYTES.to_be_bytes());
        } else {
            let mut count = 0;
            while count < BYTES {
                let bytes = controller.read().unwrap().into_data();
                assert!(!bytes.is_empty() && bytes.len() <= CHUNK);
                assert!(bytes.iter().all(|b| *b == 0x5a));
                count += bytes.len();
            }
            assert_eq!(count, BYTES);
        }
        assert!(start.elapsed() < Duration::from_secs(30));
        eprintln!(
            "node {}: {BYTES} bytes in {:.3}s",
            if upload { "upload" } else { "download" },
            start.elapsed().as_secs_f64()
        );
        let _ = controller.close(None);
        drop(controller);
        guest.join().unwrap();
        node.join().unwrap();
        let _ = done.send(());
    }
}

fn root() -> PathBuf {
    let p = std::env::temp_dir().join(format!("ow-n-{}", common::nonce().unwrap()));
    fs::create_dir(&p).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
    crate::limits::free_pool(&p);
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
    let auth = |node: &str| {
        bearer(node, &headers).is_ok_and(|s| authorized(&db(&root).unwrap(), node, &s).unwrap())
    };
    assert!(auth("alpha"));
    assert!(!auth("beta"));
    assert!(enroll(&root, v).is_err());
    revoke(&root, "alpha").unwrap();
    assert!(!auth("alpha"));
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
fn agent_refuses_a_worker_outside_its_protocol_window() {
    use std::os::unix::net::UnixListener;
    let root = root();
    let listener = UnixListener::bind(root.join("control.sock")).unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert_eq!(wire::line(&mut stream).unwrap()["op"], "status");
        let status = json!({"max_memory_mib":512,"max_running":1,"max_vcpus":1,"max_guest_memory_mib":512,"max_guest_vcpus":1,"worker_protocol":common::WORKER_PROTOCOL + 98});
        wire::send(&mut stream, &json!({"ok":true,"result":status})).unwrap();
    });
    let credential = root.join("node.json");
    write_private(
        &credential,
        &json!({"node":"alpha","credential":"a".repeat(64)}),
    )
    .unwrap();
    let error = agent(&root, "http://127.0.0.1:9", &credential, None, None, true)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(&format!(
            "worker protocol {} unsupported",
            common::WORKER_PROTOCOL + 98
        )),
        "{error}"
    );
    fs::remove_dir_all(root).unwrap();
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
    let first = journal_request(&root, &req, false).unwrap();
    server.join().unwrap();
    assert_eq!(journal_request(&root, &req, false).unwrap(), first);
    let mut changed = req.clone();
    changed["command"] = json!("changed");
    assert!(journal_request(&root, &changed, false).is_err());
    let mut unknown = req.clone();
    unknown["operation_key"] = json!("unknown");
    let db = Connection::open(root.join("node-operations.sqlite3")).unwrap();
    db.execute(
        "INSERT INTO effects(key,request) VALUES('unknown',?1)",
        [unknown.to_string()],
    )
    .unwrap();
    assert_eq!(
        journal_request(&root, &unknown, false).unwrap()["uncertain"],
        true
    );
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transport_auth_job_binding_generation_revocation_and_bounds() {
    let root = root();
    let alpha = enrollment(&root, "alpha");
    let beta = enrollment(&root, "beta");
    let c = Controller::new(&root).unwrap();
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
    let c = Controller::new(&root).unwrap();
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
    let c = Controller::new(&root).unwrap();
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
async fn refused_heartbeat_closes_with_reason_and_drain_is_stored() {
    let root = root();
    let credentials = enrollment(&root, "alpha");
    let c = Controller::new(&root).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let proxy_loop = tokio::spawn(proxies(c.clone()));
    let app = routes(c.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let token = credentials["credential"].as_str().unwrap().to_owned();
    let root1 = root.clone();
    tokio::task::spawn_blocking(move || {
        let beat = |protocol: u64| json!({"type":"heartbeat","protocol":protocol,"backend":"firecracker","arch":"x86_64","runtime":common::RUNTIME,"memory_mib":512,"slots":2,"vcpus":2,"images":["alpine"],"version":"0.5.0","worker":{"version":"0.5.0","protocol":1},"drain":true});
        let mut ws = socket(&origin, "/node/alpha", &token, None).unwrap();
        timeout_socket(&ws).unwrap();
        ws.send(tungstenite::Message::Text(beat(common::PROTOCOL + 1).to_string().into())).unwrap();
        let deadline = std::time::Instant::now();
        let frame = loop {
            match ws.read() {
                Ok(tungstenite::Message::Close(frame)) => break frame.unwrap(),
                Ok(tungstenite::Message::Ping(v)) => {
                    let _ = ws.send(tungstenite::Message::Pong(v));
                }
                Err(e) if timed_out(&e) => assert!(deadline.elapsed() < Duration::from_secs(5)),
                other => panic!("expected a close frame, got {other:?}"),
            }
        };
        assert_eq!(u16::from(frame.code), 4001);
        assert!(frame.reason.contains("unsupported"), "{}", frame.reason);
        // An accepted heartbeat keeps its optional fields; placement reads drain from them.
        let mut ws = socket(&origin, "/node/alpha", &token, None).unwrap();
        timeout_socket(&ws).unwrap();
        ws.send(tungstenite::Message::Text(beat(common::PROTOCOL).to_string().into())).unwrap();
        let deadline = std::time::Instant::now();
        while online(&root1, "alpha").is_err() {
            assert!(deadline.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(10));
        }
        let nodes = inventory(&root1).unwrap();
        assert_eq!(nodes[0]["capabilities"]["drain"], true);
        assert_eq!(nodes[0]["capabilities"]["worker"]["version"], "0.5.0");
    })
    .await
    .unwrap();
    server.abort();
    proxy_loop.abort();
    // Best effort: the aborted session loop may still be writing its last heartbeat row.
    let _ = fs::remove_dir_all(root);
}

/// A stand-in controller: counts heartbeats, sends job IDs on the session, and serves
/// each job's request, keeping the agent's reply. It checks nothing (any architecture).
struct FakeController {
    origin: String,
    beats: Arc<std::sync::atomic::AtomicUsize>,
    jobs: std::sync::mpsc::Sender<String>,
    replies: Arc<Mutex<Vec<Value>>>,
    /// Node channels opened so far.
    sessions: Arc<std::sync::atomic::AtomicUsize>,
    /// While set, open channels stay connected but get no frame (a stalled controller).
    silent: Arc<std::sync::atomic::AtomicBool>,
}
impl FakeController {
    fn start(request: Value) -> Self {
        use std::sync::atomic::Ordering;
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", tcp.local_addr().unwrap());
        let (jobs, ids) = std::sync::mpsc::channel::<String>();
        let ids = Arc::new(Mutex::new(ids));
        let beats = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let replies = Arc::new(Mutex::new(Vec::new()));
        let sessions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let silent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (beats1, replies1, sessions1, silent1) = (
            beats.clone(),
            replies.clone(),
            sessions.clone(),
            silent.clone(),
        );
        std::thread::spawn(move || {
            for stream in tcp.incoming() {
                let stream = stream.unwrap();
                let (ids, beats, replies, request, sessions, silent) = (
                    ids.clone(),
                    beats1.clone(),
                    replies1.clone(),
                    request.clone(),
                    sessions1.clone(),
                    silent1.clone(),
                );
                std::thread::spawn(move || {
                    let mut path = String::new();
                    let Ok(mut ws) = tungstenite::accept_hdr(
                        stream,
                        |req: &tungstenite::handshake::server::Request, resp| {
                            path = req.uri().path().to_owned();
                            Ok(resp)
                        },
                    ) else {
                        return;
                    };
                    ws.get_ref()
                        .set_read_timeout(Some(Duration::from_millis(50)))
                        .unwrap();
                    if path.starts_with("/job/") {
                        ws.send(tungstenite::Message::Binary(
                            format!("{request}\n").into_bytes().into(),
                        ))
                        .unwrap();
                        let mut bytes = Vec::new();
                        while !bytes.ends_with(b"\n") {
                            match ws.read() {
                                Ok(tungstenite::Message::Binary(b)) => bytes.extend_from_slice(&b),
                                Err(e) if timed_out(&e) => {}
                                _ => return,
                            }
                        }
                        replies
                            .lock()
                            .unwrap()
                            .push(serde_json::from_slice(&bytes).unwrap());
                        return;
                    }
                    let node = path.trim_start_matches("/node/").to_owned();
                    sessions.fetch_add(1, Ordering::SeqCst);
                    // As the real controller does: a ping every 3 s.
                    let mut pinged = std::time::Instant::now();
                    loop {
                        if silent.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(50));
                            continue;
                        }
                        if pinged.elapsed() > Duration::from_secs(3) {
                            pinged = std::time::Instant::now();
                            if ws.send(tungstenite::Message::Ping(vec![].into())).is_err() {
                                return;
                            }
                        }
                        match ws.read() {
                            Ok(tungstenite::Message::Text(t)) if t.contains("heartbeat") => {
                                // Acknowledge the first beat, offering the cache extension.
                                if beats.fetch_add(1, Ordering::SeqCst) == 0 {
                                    let ack = json!({"type":"ready","node":node,"generation":"g1","extensions":["cache-inventory-v1"]});
                                    let _ =
                                        ws.send(tungstenite::Message::Text(ack.to_string().into()));
                                }
                            }
                            Ok(tungstenite::Message::Ping(v)) => {
                                let _ = ws.send(tungstenite::Message::Pong(v));
                            }
                            Ok(tungstenite::Message::Pong(_)) => {}
                            Err(e) if timed_out(&e) => {}
                            _ => return,
                        }
                        if let Ok(id) = ids.lock().unwrap().try_recv() {
                            ws.send(tungstenite::Message::Text(id.into())).unwrap();
                        }
                    }
                });
            }
        });
        Self {
            origin,
            beats,
            jobs,
            replies,
            sessions,
            silent,
        }
    }
}

#[test]
fn an_agent_whose_worker_leaves_its_protocol_window_drives_it_no_more() {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    let worker = root();
    let protocol = Arc::new(AtomicU64::new(common::WORKER_PROTOCOL));
    let execs = Arc::new(AtomicUsize::new(0));
    let listener = std::os::unix::net::UnixListener::bind(worker.join("control.sock")).unwrap();
    {
        let (protocol, execs) = (protocol.clone(), execs.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let Ok(req) = wire::line(&mut stream) else {
                    continue;
                };
                let value = match req["op"].as_str().unwrap() {
                    "status" => {
                        json!({"max_memory_mib":512,"max_running":2,"max_vcpus":2,"max_guest_memory_mib":512,"max_guest_vcpus":2,
                        "guest_ssh_v1":true,"internet":true,"images":[],"version":"0.5.0","worker_pid":std::process::id(),
                        "worker_protocol":protocol.load(Ordering::SeqCst)})
                    }
                    "exec" => {
                        execs.fetch_add(1, Ordering::SeqCst);
                        json!({"output":"ran","exit_code":0})
                    }
                    other => panic!("unexpected op {other}"),
                };
                let _ = wire::send(&mut stream, &json!({"ok":true,"result":value}));
            }
        });
    }
    let fake = FakeController::start(
        json!({"op":"exec","id":"box","command":"true","operation_key":"k1"}),
    );
    let credential = worker.join("node.json");
    write_private(
        &credential,
        &json!({"node":"alpha","credential":"a".repeat(64)}),
    )
    .unwrap();
    {
        let (worker, origin, credential) =
            (worker.clone(), fake.origin.clone(), credential.clone());
        std::thread::spawn(move || agent(&worker, &origin, &credential, None, None, true));
    }
    let until = |what: &str, done: &dyn Fn() -> bool| {
        let started = std::time::Instant::now();
        while !done() {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    until("a heartbeat", &|| fake.beats.load(Ordering::SeqCst) > 0);
    // A runner host's upgrader reads agent.json: online with the releases it advertised.
    let status = || -> Value {
        serde_json::from_slice(&fs::read(worker.join("agent.json")).unwrap_or_default())
            .unwrap_or(json!({}))
    };
    until("agent.json online", &|| status()["online"] == true);
    assert_eq!(status()["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(status()["worker"]["version"], "0.5.0");
    fake.jobs.send("j1".into()).unwrap();
    until("the first reply", &|| {
        fake.replies.lock().unwrap().len() == 1
    });
    assert_eq!(
        fake.replies.lock().unwrap()[0]["result"]["output"],
        "ran",
        "a compatible worker is driven"
    );
    // The worker now speaks a protocol outside the window: the agent stops heartbeats and
    // drops its channel (the controller sees it offline at once) ...
    protocol.store(common::WORKER_PROTOCOL + 98, Ordering::SeqCst);
    std::thread::sleep(Duration::from_secs(5));
    let beats = fake.beats.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_secs(5));
    assert_eq!(
        fake.beats.load(Ordering::SeqCst),
        beats,
        "no heartbeat once the worker is incompatible"
    );
    assert_eq!(
        status()["online"],
        false,
        "the dropped channel reads offline"
    );
    // ... and a job that still reaches it is answered with an error, not forwarded.
    for checked in [
        None,
        Some(json!({"worker_protocol": common::WORKER_PROTOCOL + 98})),
    ] {
        let before = fake.replies.lock().unwrap().len();
        agent_job(
            &worker,
            &fake.origin,
            "alpha",
            "j2",
            &"a".repeat(64),
            None,
            Busy::new(),
            &Mutex::new(checked),
        )
        .unwrap();
        until("the refusal", &|| {
            fake.replies.lock().unwrap().len() == before + 1
        });
        let reply = fake.replies.lock().unwrap()[before].clone();
        assert_eq!(reply["ok"], false, "{reply}");
    }
    assert_eq!(
        execs.load(Ordering::SeqCst),
        1,
        "the incompatible worker never ran a job"
    );
    let _ = fs::remove_dir_all(worker);
}

#[test]
fn heartbeats_continue_while_the_worker_and_the_beat_builder_are_blocked() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let worker = root();
    let blocked = Arc::new(AtomicBool::new(false));
    let listener = std::os::unix::net::UnixListener::bind(worker.join("control.sock")).unwrap();
    {
        let blocked = blocked.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let (mut stream, blocked) = (stream.unwrap(), blocked.clone());
                // One connection per thread: a blocked request holds only its own.
                std::thread::spawn(move || {
                    let Ok(req) = wire::line(&mut stream) else {
                        return;
                    };
                    // A long capture: the worker answers nothing else meanwhile.
                    while blocked.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    let value = match req["op"].as_str().unwrap() {
                        "status" => {
                            json!({"max_memory_mib":512,"max_running":2,"max_vcpus":2,"max_guest_memory_mib":512,"max_guest_vcpus":2,
                            "guest_ssh_v1":true,"internet":true,"images":[],"version":"0.5.0","worker_pid":std::process::id(),
                            "worker_protocol":common::WORKER_PROTOCOL,"cache_eviction_v1":true,
                            "host_reserve":{"cpu_millis":500,"memory_mib":1024}})
                        }
                        _ => json!({"output":"forked","exit_code":0}),
                    };
                    let _ = wire::send(&mut stream, &json!({"ok":true,"result":value}));
                });
            }
        });
    }
    let fake = FakeController::start(
        json!({"op":"exec","id":"box","command":"true","operation_key":"k1"}),
    );
    let credential = worker.join("node.json");
    write_private(
        &credential,
        &json!({"node":"alpha","credential":"a".repeat(64)}),
    )
    .unwrap();
    {
        let (worker, origin, credential) =
            (worker.clone(), fake.origin.clone(), credential.clone());
        std::thread::spawn(move || agent(&worker, &origin, &credential, None, None, true));
    }
    let started = std::time::Instant::now();
    while fake.beats.load(Ordering::SeqCst) < 2 {
        assert!(started.elapsed() < Duration::from_secs(20), "no heartbeat");
        std::thread::sleep(Duration::from_millis(50));
    }
    let online = || {
        serde_json::from_slice::<Value>(&fs::read(worker.join("agent.json")).unwrap_or_default())
            .is_ok_and(|v| v["online"] == true)
    };
    // The controller marks a runner offline after 15 s without a beat; one comes every 3 s.
    let beats_during = |what: &str| {
        let before = fake.beats.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_secs(8));
        assert!(
            fake.beats.load(Ordering::SeqCst) >= before + 2,
            "heartbeats stopped while {what}"
        );
        assert!(online(), "agent.json online while {what}");
    };
    // A job in flight (a fork's capture) and every status request block in the worker.
    blocked.store(true, Ordering::SeqCst);
    fake.jobs.send("fork".into()).unwrap();
    beats_during("the worker is blocked");
    blocked.store(false, Ordering::SeqCst);
    let started = std::time::Instant::now();
    while fake.replies.lock().unwrap().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the job never finished"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Building a beat waits for the cache lock (inventory may hash a new image): Linux only.
    #[cfg(target_os = "linux")]
    {
        let held = ow_core::cache::lock(&worker, true).unwrap();
        beats_during("the beat builder is blocked");
        drop(held);
    }
    let _ = fs::remove_dir_all(worker);
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
    let c = Controller::new(&root).unwrap();
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
            let result = journal_request(&worker, &req, false).unwrap();
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

#[test]
fn invited_caps_replacement_revoked_and_publication_failure() {
    let root = root();
    let first = invitation(
        &root,
        "friend",
        600,
        512,
        2,
        2,
        Some("https://pool.example/_nodes"),
        |_| Ok(()),
    )
    .unwrap();
    let next = invitation(
        &root,
        "friend",
        600,
        1024,
        3,
        1,
        Some("https://pool.example/_nodes"),
        |_| Ok(()),
    )
    .unwrap();
    assert!(enroll(&root, first).is_err());
    // Publisher failure must roll back replacement and retain the prior invitation.
    assert!(
        invitation(&root, "friend", 600, 2048, 4, 4, None, |_| anyhow::bail!(
            "simulated publication failure"
        ))
        .is_err()
    );
    let cap: u32 = db(&root)
        .unwrap()
        .query_row(
            "SELECT vcpu_cap FROM node_limits WHERE node='friend'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cap, 1);
    assert!(enroll(&root, next).is_ok());
    let unused = invitation(&root, "unused", 600, 512, 2, 2, None, |_| Ok(())).unwrap();
    revoke(&root, "unused").unwrap();
    assert!(invitation(&root, "unused", 600, 512, 2, 2, None, |_| Ok(())).is_err());
    assert!(enroll(&root, unused).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_aliases_share_native_generation_and_clamp_forged_capacity() {
    let root = root();
    let invite = invitation(&root, "friend", 600, 512, 2, 1, None, |_| Ok(())).unwrap();
    let c = Controller::new(&root).unwrap();
    // Simulated proxy presence only; no worker or VM runs in this fixture.
    fs::create_dir_all(root.join("nodes/friend")).unwrap();
    fs::write(root.join("nodes/friend/control.sock"), b"fixture").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = routes(c.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    for path in [
        "/_nodes/api/state",
        "/_nodes/enroll/extra",
        "/_nodes/node/friend/extra",
        "/_nodes//enroll",
        "/_nodes/%65nroll",
        "/_nodes/enroll?secret=x",
    ] {
        let response = client
            .post(format!("{origin}{path}"))
            .json(&invite)
            .send()
            .await
            .unwrap();
        assert!(!response.status().is_success(), "{path}");
    }
    assert_eq!(
        client
            .get(format!("{origin}/_nodes/enroll"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    let response = client
        .post(format!("{origin}/_nodes/enroll"))
        .json(&invite)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let credentials: Value = response.json().await.unwrap();
    let root1 = root.clone();
    let c1 = c.clone();
    tokio::task::spawn_blocking(move||{
        let bearer=credentials["credential"].as_str().unwrap();
        let mut old=socket(&origin,"/node/friend",bearer,None).unwrap();timeout_socket(&old).unwrap();
        let heartbeat=json!({"type":"heartbeat","protocol":1,"backend":"firecracker","arch":"x86_64","runtime":"firecracker-v1.17.0","memory_mib":4096,"slots":8,"vcpus":16,"images":["alpine"]});
        old.send(tungstenite::Message::Text(heartbeat.to_string().into())).unwrap();
        for _ in 0..100{if online(&root1,"friend").is_ok(){break;}std::thread::sleep(Duration::from_millis(10));}
        let inventory=inventory(&root1).unwrap();assert_eq!(inventory[0]["capabilities"]["vcpus"],1);assert_eq!(inventory[0]["memory_mib"],512);assert_eq!(inventory[0]["slots"],2);
        let generation=c1.sessions.lock().unwrap()["friend"].generation.clone();
        let mut public=socket(&origin,"/_nodes/node/friend",bearer,None).unwrap();timeout_socket(&public).unwrap();let mut ack=heartbeat;ack["ready_ack"]=json!(true);public.send(tungstenite::Message::Text(ack.to_string().into())).unwrap();
        let deadline=std::time::Instant::now();loop{match public.read(){Ok(tungstenite::Message::Text(t))=>{let v:Value=serde_json::from_str(&t).unwrap();assert_eq!(v["type"],"ready");assert_ne!(v["generation"],generation);break;},Ok(tungstenite::Message::Ping(v))=>public.send(tungstenite::Message::Pong(v)).unwrap(),Err(e) if timed_out(&e)=>assert!(deadline.elapsed()<Duration::from_secs(5)),other=>panic!("unexpected {other:?}")}}
        assert_ne!(c1.sessions.lock().unwrap()["friend"].generation,generation);
        revoke(&root1,"friend").unwrap();assert!(socket(&origin,"/_nodes/node/friend",bearer,None).is_err());assert!(socket(&origin,"/node/friend",bearer,None).is_err());
    }).await.unwrap();
    for _ in 0..100 {
        if c.sessions.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        c.sessions.lock().unwrap().is_empty(),
        "fixture channels did not drain"
    );
    server.abort();
    // Map removal precedes the final generation-scoped SQLite cleanup write.
    // Delete only this disposable fixture, retrying its teardown race briefly.
    let cleanup = std::time::Instant::now();
    loop {
        match fs::remove_dir_all(&root) {
            Ok(()) => break,
            Err(e)
                if e.kind() == std::io::ErrorKind::DirectoryNotEmpty
                    && cleanup.elapsed() < Duration::from_secs(2) =>
            {
                tokio::time::sleep(Duration::from_millis(20)).await
            }
            Err(e) => panic!("fixture cleanup failed: {e}"),
        }
    }
}

#[test]
fn enrolled_budget_is_durable_preserves_identity_and_reclamps_stale_heartbeat() {
    let root = root();
    enrollment(&root, "friend");
    let before: String = db(&root)
        .unwrap()
        .query_row(
            "SELECT credential_hash FROM nodes WHERE id='friend'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    budget(&root, "friend", 65536, 8, 64).unwrap();
    let connection = db(&root).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT vcpu_cap FROM node_capacity_limits WHERE node='friend'",
                [],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
        64
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT credential_hash FROM nodes WHERE id='friend'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        before
    );
    assert!(budget(&root, "friend", 65537, 8, 64).is_err());
    assert!(budget(&root, "local", 65536, 8, 64).is_err());
    assert!(budget(&root, "missing", 65536, 8, 64).is_err());
    // Offline demand remains unknown, even with a large owner ceiling.
    assert!(budget(&root, "friend", 32768, 8, 32).is_err());
    // Simulate a heartbeat that read old ceilings before an owner edit.
    connection
        .execute(
            "UPDATE nodes SET memory_mib=512,slots=1,capabilities=?1 WHERE id='friend'",
            [json!({"memory_mib":65536,"slots":8,"vcpus":64,"images":["alpine"]}).to_string()],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE node_capacity_limits SET vcpu_cap=2 WHERE node='friend'",
            [],
        )
        .unwrap();
    let n = &inventory(&root).unwrap()[0];
    assert_eq!(n["memory_mib"], 512);
    assert_eq!(n["slots"], 1);
    assert_eq!(n["capabilities"]["vcpus"], 2);
    revoke(&root, "friend").unwrap();
    assert!(budget(&root, "friend", 65536, 8, 64).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn lowering_needs_fresh_idle_inventory_and_zero_all_demand_dimensions() {
    let root = root();
    let _catalog =
        crate::catalog::Catalog::open(&root, "https://example.cloudflareaccess.com", None).unwrap();
    enrollment(&root, "friend");
    budget(&root, "friend", 8192, 4, 16).unwrap();
    let db = db(&root).unwrap();
    db.execute("UPDATE nodes SET heartbeat=?1 WHERE id='friend'", [now()])
        .unwrap();
    fs::create_dir_all(root.join("nodes/friend")).unwrap();
    let listener =
        std::os::unix::net::UnixListener::bind(root.join("nodes/friend/control.sock")).unwrap();
    let mock = std::thread::spawn(move || {
        for i in 0..5 {
            let (mut stream, _) = listener.accept().unwrap();
            assert_eq!(wire::line(&mut stream).unwrap()["op"], "list");
            let machines = if i == 0 {
                json!([{"id":"unknown","state":"running","memory_mib":256,"vcpu_count":1}])
            } else {
                json!([])
            };
            wire::send(&mut stream, &json!({"ok":true,"result":machines})).unwrap();
        }
    });
    assert!(budget(&root, "friend", 4096, 2, 2).is_err());
    for (ram, cpu, slots) in [(1, 0, 0), (0, 1, 0), (0, 0, 1)] {
        db.execute("INSERT INTO node_usage(node,extra_mib,extra_cpus,extra_slots) VALUES('friend',?1,?2,?3) ON CONFLICT(node) DO UPDATE SET extra_mib=?1,extra_cpus=?2,extra_slots=?3",params![ram,cpu,slots]).unwrap();
        assert!(budget(&root, "friend", 4096, 2, 2).is_err());
    }
    db.execute(
        "UPDATE node_usage SET extra_mib=0,extra_cpus=0,extra_slots=0",
        [],
    )
    .unwrap();
    assert!(budget(&root, "friend", 4096, 2, 2).is_ok());
    mock.join().unwrap();
    assert_eq!(
        inventory(&root).unwrap()[0]["owner_limits"],
        json!({"memory":4096,"slots":2,"cpus":2})
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn lowering_inventory_has_absolute_deadline_without_sqlite_writer_hold() {
    let root = root();
    let _catalog =
        crate::catalog::Catalog::open(&root, "https://example.cloudflareaccess.com", None).unwrap();
    enrollment(&root, "friend");
    budget(&root, "friend", 8192, 4, 16).unwrap();
    db(&root)
        .unwrap()
        .execute("UPDATE nodes SET heartbeat=?1 WHERE id='friend'", [now()])
        .unwrap();
    fs::create_dir_all(root.join("nodes/friend")).unwrap();
    let listener =
        std::os::unix::net::UnixListener::bind(root.join("nodes/friend/control.sock")).unwrap();
    let (sent, received) = std::sync::mpsc::channel();
    let mock = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        wire::line(&mut stream).unwrap();
        sent.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(3200));
    });
    let selected = root.clone();
    let start = std::time::Instant::now();
    let edit = std::thread::spawn(move || budget(&selected, "friend", 4096, 2, 2));
    received.recv_timeout(Duration::from_secs(1)).unwrap();
    let writer = std::time::Instant::now();
    db(&root)
        .unwrap()
        .execute("UPDATE nodes SET heartbeat=?1 WHERE id='friend'", [now()])
        .unwrap();
    revoke(&root, "friend").unwrap();
    assert!(
        writer.elapsed() < Duration::from_secs(1),
        "transport held SQLite writer"
    );
    assert!(edit.join().unwrap().is_err());
    assert!(start.elapsed() < Duration::from_secs(4));
    assert_eq!(inventory(&root).unwrap()[0]["owner_limits"]["memory"], 8192);
    mock.join().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn outbound_runner_relays_snapshot_delete_only_to_a_capable_worker() {
    use std::net::TcpListener;
    let worker = root();
    let listener = std::os::unix::net::UnixListener::bind(worker.join("control.sock")).unwrap();
    let runtime = std::thread::spawn(move || {
        let (mut unix, _) = listener.accept().unwrap();
        let request = wire::line(&mut unix).unwrap();
        assert_eq!(request["op"], "snapshot-delete");
        let reply = json!({"deleted":request["name"],"workspace":request["id"],"existed":true});
        wire::send(&mut unix, &json!({"ok":true,"result":reply})).unwrap();
    });
    for capable in [false, true] {
        let controller = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", controller.local_addr().unwrap());
        let path = worker.clone();
        let agent = std::thread::spawn(move || {
            agent_job(
                &path,
                &origin,
                "alpha",
                "job",
                "fixture",
                None,
                Busy::new(),
                &Mutex::new(Some(
                    json!({"worker_protocol":1,"worker_pid":std::process::id(),
                    "snapshot_delete_v1":capable}),
                )),
            )
        });
        let (tcp, _) = controller.accept().unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut socket = tungstenite::accept(tcp).unwrap();
        let request = json!({"op":"snapshot-delete","id":"box","name":"old"});
        socket
            .send(tungstenite::Message::Binary(
                format!("{request}\n").into_bytes().into(),
            ))
            .unwrap();
        if capable {
            let reply: Value = serde_json::from_slice(&socket.read().unwrap().into_data()).unwrap();
            assert_eq!(reply["ok"], true, "{reply}");
            assert_eq!(reply["result"]["deleted"], "old");
            let _ = socket.close(None);
            drop(socket);
            assert!(agent.join().unwrap().is_ok());
        } else {
            drop(socket);
            assert!(agent.join().unwrap().is_err(), "an old worker was asked");
        }
    }
    runtime.join().unwrap();
    fs::remove_dir_all(worker).unwrap();
}

#[test]
fn preview_tunnel_job_uses_the_existing_worker_stream() {
    use std::net::TcpListener;
    let worker = root();
    let listener = std::os::unix::net::UnixListener::bind(worker.join("control.sock")).unwrap();
    let runtime = std::thread::spawn(move || {
        let (mut unix, _) = listener.accept().unwrap();
        assert_eq!(
            wire::line(&mut unix).unwrap(),
            json!({"op":"tunnel","id":"box","port":3000})
        );
        wire::send(&mut unix, &json!({"ok":true,"result":{}})).unwrap();
        let mut bytes = [0; 5];
        unix.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"hello");
        unix.write_all(b"reply").unwrap();
        let mut end = [0; 1];
        let _ = unix.read(&mut end);
    });
    for request in [
        json!({"op":"tunnel","id":"box","port":3000}),
        json!({"op":"tunnel","id":"box","port":0}),
        json!({"op":"tunnel","id":"box","port":3000,"host":"127.0.0.1"}),
    ] {
        let valid = request["port"] == 3000 && request.get("host").is_none();
        let controller = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", controller.local_addr().unwrap());
        let path = worker.clone();
        let agent = std::thread::spawn(move || {
            agent_job(
                &path,
                &origin,
                "alpha",
                "job",
                "fixture",
                None,
                Busy::new(),
                &Mutex::new(Some(
                    json!({"worker_protocol":1,"worker_pid":std::process::id()}),
                )),
            )
        });
        let (tcp, _) = controller.accept().unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut socket = tungstenite::accept(tcp).unwrap();
        socket
            .send(tungstenite::Message::Binary(
                format!("{request}\n").into_bytes().into(),
            ))
            .unwrap();
        if valid {
            let reply = socket.read().unwrap().into_data();
            assert!(serde_json::from_slice::<Value>(&reply).unwrap()["ok"] == true);
            socket
                .send(tungstenite::Message::Binary(b"hello".to_vec().into()))
                .unwrap();
            assert_eq!(socket.read().unwrap().into_data().as_ref(), b"reply");
            let _ = socket.close(None);
            drop(socket);
            assert!(agent.join().unwrap().is_ok());
        } else {
            assert!(agent.join().unwrap().is_err());
        }
    }
    runtime.join().unwrap();
    fs::remove_dir_all(worker).unwrap();
}

fn spend_config(root: &Path, cap: u64, rate: u64, hours: u64, retired: &[&str]) {
    fs::write(root.join("limits.json"), json!({"pool":{"max_estimated_cents":cap,"hours":hours,"default_type":"test-instance","hourly_cents":{"test-instance":rate},"retired_nodes":retired}}).to_string()).unwrap();
    fs::set_permissions(root.join("limits.json"), fs::Permissions::from_mode(0o600)).unwrap();
}
#[test]
fn pool_spend_defaults_unknown_price_exact_cap_and_enrollment_recheck() {
    let root = root();
    fs::remove_file(root.join("limits.json")).unwrap();
    assert!(
        invitation(&root, "alpha", 600, 512, 2, 2, None, |_| Ok(()))
            .unwrap_err()
            .to_string()
            .contains("price missing")
    );
    assert_eq!(inventory(&root).unwrap(), json!([]));
    spend_config(&root, 100, 50, 2, &[]);
    let first = invitation(&root, "alpha", 600, 512, 2, 2, None, |_| Ok(())).unwrap();
    // Replacing an invitation counts the node once, invalidates the previous secret.
    let second = invitation(&root, "alpha", 600, 512, 2, 2, None, |_| Ok(())).unwrap();
    assert!(enroll(&root, first).is_err());
    let published = std::cell::Cell::new(false);
    let error = invitation(&root, "beta", 600, 512, 2, 2, None, |_| {
        published.set(true);
        Ok(())
    })
    .unwrap_err();
    assert!(error.to_string().contains("pool spend cap exceeded"));
    assert!(!published.get());
    assert_eq!(inventory(&root).unwrap().as_array().unwrap().len(), 1);
    // A cap change between mint and consume prevents stale authorization.
    spend_config(&root, 99, 50, 2, &[]);
    assert!(
        enroll(&root, second.clone())
            .unwrap_err()
            .to_string()
            .contains("spend cap")
    );
    spend_config(&root, 100, 50, 2, &[]);
    enroll(&root, second).unwrap();
    // Offline/revoked capacity still costs money until explicitly retired.
    revoke(&root, "alpha").unwrap();
    assert!(invitation(&root, "beta", 600, 512, 2, 2, None, |_| Ok(())).is_err());
    spend_config(&root, 100, 50, 2, &["alpha"]);
    invitation(&root, "beta", 600, 512, 2, 2, None, |_| Ok(())).unwrap();
    spend_config(&root, 100, 50, 2, &["alpha", "beta"]);
    assert!(
        invitation(&root, "gamma", 600, 512, 2, 2, None, |_| Ok(()))
            .unwrap_err()
            .to_string()
            .contains("must first be revoked")
    );
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn pool_spend_concurrent_invitations_have_one_winner_and_overflow_refuses() {
    let root = root();
    spend_config(&root, 100, 100, 1, &[]);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = ["alpha", "beta"]
        .into_iter()
        .map(|node| {
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                invitation(&root, node, 600, 512, 2, 2, None, |_| Ok(())).is_ok()
            })
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .filter_map(|t| t.join().ok())
            .filter(|won| *won)
            .count(),
        1
    );
    spend_config(&root, u64::MAX, u64::MAX, 2, &[]);
    assert!(
        invitation(&root, "overflow", 600, 512, 2, 2, None, |_| Ok(()))
            .unwrap_err()
            .to_string()
            .contains("overflow")
    );
    assert_eq!(inventory(&root).unwrap().as_array().unwrap().len(), 1);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn placement_steps_reconcile_effect_before_reply_and_recheck_cached_authority() {
    use std::os::unix::net::UnixListener;
    for op in
        common::PLACEMENT_OPERATIONS
            .iter()
            .copied()
            .chain(["start", "delete", "snapshot-delete"])
    {
        let root = root();
        let listener = UnixListener::bind(root.join("control.sock")).unwrap();
        let request = json!({"op":op,"id":"box","placement":{"epoch":7,"node":"alpha","transfer":"incarnation"},"operation_key":"step"});
        // Lose the worker reply after the effect: the journal must remain uncertain.
        let worker = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let req = wire::line(&mut first).unwrap();
            assert_eq!(req["op"], op);
            drop(first);
            let (mut retry, _) = listener.accept().unwrap();
            assert_eq!(wire::line(&mut retry).unwrap(), req);
            wire::send(
                &mut retry,
                &json!({"ok":true,"result":{"logical_effects":1,"epoch":7}}),
            )
            .unwrap();
            drop(retry);
            // A cached reply is usable only after checking current worker authority.
            for current in [true, false] {
                let (mut cached, _) = listener.accept().unwrap();
                let check = wire::line(&mut cached).unwrap();
                assert_eq!(check["op"], "placement-check");
                assert_eq!(check["request"], req);
                let reply = if current {
                    json!({"ok":true,"result":{}})
                } else {
                    json!({"ok":false,"error":"stale placement epoch"})
                };
                wire::send(&mut cached, &reply).unwrap();
            }
        });
        assert_eq!(
            journal_request(&root, &request, true).unwrap()["uncertain"],
            true
        );
        let result = journal_request(&root, &request, true).unwrap();
        assert_eq!(result["result"]["logical_effects"], 1);
        assert_eq!(journal_request(&root, &request, true).unwrap(), result);
        let stale = journal_request(&root, &request, true).unwrap();
        assert_eq!(stale["ok"], false);
        assert!(stale["error"].as_str().unwrap().contains("stale"));
        worker.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn a_legacy_cached_reply_cannot_bypass_a_new_managed_fence() {
    use std::os::unix::net::UnixListener;
    let root = root();
    let listener = UnixListener::bind(root.join("control.sock")).unwrap();
    let worker = std::thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        let req = wire::line(&mut first).unwrap();
        assert!(req.get("placement").is_none());
        wire::send(&mut first, &json!({"ok":true,"result":{"state":"running"}})).unwrap();
        drop(first);
        let (mut cached, _) = listener.accept().unwrap();
        let req = wire::line(&mut cached).unwrap();
        assert_eq!(req["op"], "placement-check");
        assert!(req["request"].get("placement").is_none());
        wire::send(
            &mut cached,
            &json!({"ok":false,"error":"missing placement epoch for managed workspace"}),
        )
        .unwrap();
    });
    let req = json!({"op":"start","id":"box","operation_key":"old"});
    assert_eq!(journal_request(&root, &req, false).unwrap()["ok"], true);
    assert_eq!(journal_request(&root, &req, true).unwrap()["ok"], false);
    worker.join().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn managed_stream_envelopes_reach_only_the_named_capable_worker() {
    use std::{net::TcpListener, os::unix::net::UnixListener};
    let root = root();
    let listener = UnixListener::bind(root.join("control.sock")).unwrap();
    let runtime = std::thread::spawn(move || {
        for op in ["ssh", "terminal", "tunnel"] {
            let (mut stream, _) = listener.accept().unwrap();
            let request = wire::line(&mut stream).unwrap();
            assert_eq!(request["op"], op);
            assert_eq!(request["placement"]["epoch"], 7);
            assert_eq!(request["placement"]["node"], "alpha");
            wire::send(&mut stream, &json!({"ok":true,"result":{}})).unwrap();
        }
    });
    for op in ["ssh", "terminal", "tunnel"] {
        for eligible in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let path = root.clone();
            let agent = std::thread::spawn(move || {
                agent_job(
                    &path,
                    &origin,
                    "alpha",
                    "job",
                    "fixture",
                    None,
                    Busy::new(),
                    &Mutex::new(Some(
                        json!({"worker_protocol":2,"placement_fencing_v1":true,"worker_pid":std::process::id()}),
                    )),
                )
            });
            let (tcp, _) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut socket = tungstenite::accept(tcp).unwrap();
            let authority = common::Placement {
                epoch: 7,
                node: if eligible { "alpha" } else { "beta" }.into(),
                transfer: "move1".into(),
            };
            let mut request = wire::workspace_request(op, "box", Some(&authority));
            if op == "tunnel" {
                request["port"] = json!(3000);
            }
            socket
                .send(tungstenite::Message::Binary(
                    format!("{request}\n").into_bytes().into(),
                ))
                .unwrap();
            if eligible {
                assert_eq!(
                    serde_json::from_slice::<Value>(&socket.read().unwrap().into_data()).unwrap()["ok"],
                    true
                );
                drop(socket);
                agent.join().unwrap().unwrap();
            } else {
                assert!(
                    agent
                        .join()
                        .unwrap()
                        .unwrap_err()
                        .to_string()
                        .contains("another node")
                );
            }
        }
    }
    runtime.join().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn review_cached_unmanaged_delete_keeps_its_success_on_a_fencing_worker() {
    use std::os::unix::net::UnixListener;
    let root = root();
    let assets = root.join("assets");
    fs::create_dir_all(assets.join("downloads")).unwrap();
    fs::write(assets.join("downloads/vmlinux-6.1.186"), b"fixture kernel").unwrap();
    fs::create_dir_all(root.join("machines/legacy")).unwrap();
    fs::write(root.join("machines/legacy/disk.ext4"), b"fixture disk").unwrap();
    fs::write(root.join("state.json"), json!({"next_index":2,"workspaces":{"legacy":{
        "id":"legacy","index":1,"state":"stopped","memory_mib":256,"source":null,"hibernation":null
    }}}).to_string()).unwrap();
    let mut runtime = ow_core::runtime::Runtime::new(root.clone(), assets).unwrap();
    let listener = UnixListener::bind(root.join("control.sock")).unwrap();
    let worker = std::thread::spawn(move || {
        for op in ["delete", "placement-check"] {
            let (mut stream, _) = listener.accept().unwrap();
            let request = wire::line(&mut stream).unwrap();
            assert_eq!(request["op"], op);
            wire::send(&mut stream, &wire::response(runtime.request(&request))).unwrap();
        }
    });
    let request = json!({"op":"delete","id":"legacy","operation_key":"delete-once"});
    let first = journal_request(&root, &request, true).unwrap();
    let cached = journal_request(&root, &request, true).unwrap();
    worker.join().unwrap();
    assert_eq!(
        first,
        json!({"ok":true,"result":{"deleted":"legacy","snapshots":[]}})
    );
    assert_eq!(
        cached, first,
        "revalidation must preserve the durable successful delete reply"
    );
    assert!(!root.join("machines/legacy").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn subprocess_exit_interrupt_does_not_disconnect_the_control_channel() {
    assert!(timed_out(&tungstenite::Error::Io(
        std::io::Error::from_raw_os_error(libc::EINTR)
    )));
    assert!(!timed_out(&tungstenite::Error::Io(
        std::io::Error::from_raw_os_error(libc::ECONNRESET)
    )));
}

#[cfg(target_os = "linux")]
#[test]
fn cache_shape_is_withheld_until_controller_negotiates_and_worker_enforces_it() {
    let root =
        std::env::temp_dir().join(format!("ow-cache-negotiate-{}", common::nonce().unwrap()));
    fs::create_dir(&root).unwrap();
    for (negotiated, capable, expected) in [
        (false, true, false),
        (true, false, false),
        (true, true, true),
    ] {
        let mut beat = json!({});
        cache_extension(
            &mut beat,
            &json!({"cache_eviction_v1":capable}),
            &root,
            negotiated,
        )
        .unwrap();
        assert_eq!(beat.get("cache_entries").is_some(), expected);
        assert_eq!(beat["cache_inventory_v1"] == true, expected);
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn controller_placement_policy_survives_runner_drain_changes() {
    let root =
        std::env::temp_dir().join(format!("ow-placement-policy-{}", common::nonce().unwrap()));
    fs::create_dir(&root).unwrap();
    let db = db(&root).unwrap();
    db.execute("INSERT INTO nodes(id,memory_mib,slots,capabilities) VALUES('runner',512,1,'{\"memory_mib\":512,\"slots\":1,\"vcpus\":1,\"drain\":true}')",[]).unwrap();
    db.execute("INSERT INTO node_placement_policy(node,pool,active,controller_drain) VALUES('runner','test-pool',1,1)",[]).unwrap();
    for draining in [false, true, false] {
        db.execute("UPDATE nodes SET capabilities=json_set(capabilities,'$.drain',json(?1),'$.controller_drain',json('false'),'$.pool','untrusted')",[draining.to_string()]).unwrap();
        let n = inventory(&root).unwrap();
        assert_eq!(n[0]["capabilities"]["controller_drain"], true);
        assert_eq!(n[0]["capabilities"]["pool"], "test-pool");
        assert_eq!(n[0]["capabilities"]["drain"], draining);
    }
    drop(db);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn live_health_preserves_catalog_policy_revocation_and_restart_fences() {
    let root = root();
    let c = crate::catalog::Catalog::open(&root, "fixture", None).unwrap();
    let credential = enrollment(&root, "alpha");
    assert!(online(&root, "alpha").is_err());
    let health = liveness_db(&root, "alpha").unwrap();
    health
        .execute(
            "UPDATE health SET session='generation',heartbeat=?1,capabilities=?2",
            params![
                now(),
                json!({"memory_mib":9999,"slots":99,"vcpus":99,"managed_lifecycle_v1":true})
                    .to_string()
            ],
        )
        .unwrap();
    // A catalog connection opened before the health file must discover it too.
    let generation: String =
        c.db.query_row("SELECT session FROM live_nodes WHERE id='alpha'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(generation, "generation");
    assert!(online(&root, "alpha").is_ok());
    assert!(managed_lifecycle_capable(&root, "alpha").is_ok());
    c.db.execute(
        "UPDATE nodes SET memory_mib=256,slots=1 WHERE id='alpha'",
        [],
    )
    .unwrap();
    c.db.execute(
        "UPDATE node_capacity_limits SET vcpu_cap=1 WHERE node='alpha'",
        [],
    )
    .unwrap();
    let inventory = inventory(&root).unwrap();
    for (key, expected) in [("memory_mib", 256), ("slots", 1), ("vcpus", 1)] {
        assert_eq!(inventory[0]["capabilities"][key], expected);
    }
    assert!(authorized(&c.db, "alpha", credential["credential"].as_str().unwrap()).unwrap());
    reset_health(&root).unwrap();
    assert!(online(&root, "alpha").is_err());
    let generation: Option<String> =
        c.db.query_row("SELECT session FROM live_nodes WHERE id='alpha'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(generation.is_none());
    // Even a fresh health record cannot undo durable revocation.
    health
        .execute("UPDATE health SET session='later',heartbeat=?1", [now()])
        .unwrap();
    revoke(&root, "alpha").unwrap();
    assert!(online(&root, "alpha").is_err());
    assert!(!authorized(&c.db, "alpha", credential["credential"].as_str().unwrap()).unwrap());
    drop(health);
    drop(c);
    fs::remove_dir_all(root).unwrap();
}

/// Keep a genuinely issued old job present on the restored authority: an unknown
/// ID would test only lookup failure and could miss a broken generation fence.
#[test]
fn restored_controller_rejects_previously_issued_job_and_old_session_results() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let _entered = rt.enter();
    let old_root = root();
    let replacement_root = root();
    drop(crate::catalog::Catalog::open(&old_root, "fixture", None).unwrap());
    let enrolled = enrollment(&old_root, "alpha");
    let token = enrolled["credential"].as_str().unwrap();
    let controller = |root: &Path| Controller::new(root).unwrap();
    let serve = |c: &Controller| {
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let app = routes(c.clone());
        let server = rt.spawn(async move { axum::serve(listener, app).await.unwrap() });
        let proxies = rt.spawn(proxies(c.clone()));
        (origin, server, proxies)
    };
    let heartbeat = json!({"type":"heartbeat","protocol":1,"backend":"firecracker","arch":"x86_64","runtime":"firecracker-v1.17.0","memory_mib":512,"slots":2,"vcpus":2,"images":["alpine"]}).to_string();
    let connect = |origin: &str, root: &Path| {
        let mut session = socket(origin, "/node/alpha", token, None).unwrap();
        timeout_socket(&session).unwrap();
        let since = std::time::Instant::now();
        while online(root, "alpha").is_err() {
            assert!(since.elapsed() < Duration::from_secs(5));
            session
                .send(tungstenite::Message::Text(heartbeat.clone().into()))
                .unwrap();
            std::thread::sleep(Duration::from_millis(25));
        }
        session
    };
    let dispatch = |c: &Controller, marker: &str| {
        let (mut local, remote) = std::os::unix::net::UnixStream::pair().unwrap();
        local
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        remote.set_nonblocking(true).unwrap();
        let upstream = tokio::net::UnixStream::from_std(remote).unwrap();
        let task = rt.spawn(proxy(c.clone(), "alpha".into(), upstream));
        wire::send(
            &mut local,
            &json!({"op":"exec","id":"fixture","command":marker}),
        )
        .unwrap();
        (local, task)
    };
    let next_job = |session: &mut tungstenite::WebSocket<
        tungstenite::stream::MaybeTlsStream<std::net::TcpStream>,
    >| {
        loop {
            match session.read().unwrap() {
                tungstenite::Message::Text(id) => break id.to_string(),
                tungstenite::Message::Ping(p) => {
                    session.send(tungstenite::Message::Pong(p)).unwrap()
                }
                other => panic!("expected issued job, got {other:?}"),
            }
        }
    };
    let old = controller(&old_root);
    let (origin, server, proxy_loop) = serve(&old);
    let mut old_session = connect(&origin, &old_root);
    let old_generation = old.sessions.lock().unwrap()["alpha"].generation.clone();
    let (mut live_local, live_proxy) = dispatch(&old, "accepted-before-cutover");
    let live_id = next_job(&mut old_session);
    let mut live_job = socket(&origin, &format!("/job/alpha/{live_id}"), token, None).unwrap();
    timeout_socket(&live_job).unwrap();
    assert!(
        String::from_utf8_lossy(&live_job.read().unwrap().into_data())
            .contains("accepted-before-cutover")
    );
    live_job
        .send(tungstenite::Message::Binary(b"before\n".to_vec().into()))
        .unwrap();
    let mut before = [0; 7];
    live_local.read_exact(&mut before).unwrap();
    assert_eq!(
        &before, b"before\n",
        "old session must actually work before fencing"
    );

    let (pending_local, pending_proxy) = dispatch(&old, "issued-but-not-attached");
    let pending_id = next_job(&mut old_session);
    let pending = old
        .jobs
        .lock()
        .unwrap()
        .remove(&pending_id)
        .expect("production proxy issued this job");
    assert_eq!(pending.node, "alpha");
    assert_eq!(pending.generation, old_generation);
    old.sessions.lock().unwrap().clear();
    // Challenge a result on the previously accepted stream AFTER the authority is fenced.
    let _ = live_job.send(tungstenite::Message::Binary(
        b"STALE-RESULT\n".to_vec().into(),
    ));
    let mut late = [0; 32];
    assert_eq!(
        live_local.read(&mut late).unwrap(),
        0,
        "old result reached the caller"
    );
    assert!(rt.block_on(live_proxy).unwrap().is_err());
    pending_proxy.abort();
    let _ = rt.block_on(pending_proxy);
    drop(pending_local);
    let _ = old_session.close(None);
    proxy_loop.abort();
    server.abort();
    let _ = rt.block_on(proxy_loop);
    let _ = rt.block_on(server);
    // Wait for the upgraded old session's final generation-scoped DB write.
    let since = std::time::Instant::now();
    while db(&old_root)
        .unwrap()
        .query_row(
            "SELECT heartbeat FROM live_nodes WHERE id='alpha'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        != 0
    {
        assert!(since.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    crate::backup::run(
        None,
        crate::BackupAction::Catalog {
            output: replacement_root.join("catalog.sqlite3"),
            from: Some(old_root.clone()),
        },
    )
    .unwrap();
    let replacement = controller(&replacement_root);
    let (replacement_origin, replacement_server, replacement_loop) = serve(&replacement);
    let mut new_session = connect(&replacement_origin, &replacement_root);
    let new_generation = replacement.sessions.lock().unwrap()["alpha"]
        .generation
        .clone();
    assert_ne!(new_generation, old_generation);
    replacement
        .jobs
        .lock()
        .unwrap()
        .insert(pending_id.clone(), pending);
    let denied = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{replacement_origin}/job/alpha/{pending_id}"))
        .bearer_auth(token)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .unwrap();
    assert_eq!(denied.status().as_u16(), 404);
    assert!(
        replacement.jobs.lock().unwrap().contains_key(&pending_id),
        "denial must be generation fencing, not unknown-job lookup"
    );

    // A current production-issued job with the SAME credential still works.
    let (mut current_local, current_proxy) = dispatch(&replacement, "current-authority");
    let current_id = next_job(&mut new_session);
    let mut current_job = socket(
        &replacement_origin,
        &format!("/job/alpha/{current_id}"),
        token,
        None,
    )
    .unwrap();
    timeout_socket(&current_job).unwrap();
    assert!(
        String::from_utf8_lossy(&current_job.read().unwrap().into_data())
            .contains("current-authority")
    );
    current_job
        .send(tungstenite::Message::Binary(b"fresh\n".to_vec().into()))
        .unwrap();
    let mut fresh = [0; 6];
    current_local.read_exact(&mut fresh).unwrap();
    assert_eq!(&fresh, b"fresh\n");
    current_proxy.abort();
    let _ = rt.block_on(current_proxy);
    let _ = new_session.close(None);
    replacement_loop.abort();
    replacement_server.abort();
    let _ = rt.block_on(replacement_loop);
    let _ = rt.block_on(replacement_server);
    drop(_entered);
    drop(rt);
    fs::remove_dir_all(old_root).unwrap();
    fs::remove_dir_all(replacement_root).unwrap();
}

/// A minimal worker for an agent: answers `status`, nothing else.
fn status_worker() -> PathBuf {
    let worker = root();
    let listener = std::os::unix::net::UnixListener::bind(worker.join("control.sock")).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let Ok(req) = wire::line(&mut stream) else {
                continue;
            };
            assert_eq!(req["op"], "status");
            let value = json!({"max_memory_mib":512,"max_running":2,"max_vcpus":2,"max_guest_memory_mib":512,"max_guest_vcpus":2,
                "guest_ssh_v1":true,"internet":true,"images":[],"version":"0.5.0","worker_pid":std::process::id(),
                "worker_protocol":common::WORKER_PROTOCOL,"recovery_disk_only_v1":true,
                "host_reserve":{"cpu_millis":500,"memory_mib":1024}});
            let _ = wire::send(&mut stream, &json!({"ok":true,"result":value}));
        }
    });
    worker
}

#[test]
fn an_agent_reconnects_by_itself_when_the_controller_goes_silent() {
    use std::sync::atomic::Ordering;
    let worker = status_worker();
    let fake = FakeController::start(json!({"op":"status"}));
    let credential = worker.join("node.json");
    write_private(
        &credential,
        &json!({"node":"alpha","credential":"a".repeat(64)}),
    )
    .unwrap();
    {
        let (worker, origin, credential) =
            (worker.clone(), fake.origin.clone(), credential.clone());
        std::thread::spawn(move || agent(&worker, &origin, &credential, None, None, true));
    }
    let wait = |what: &str, limit: Duration, done: &dyn Fn() -> bool| {
        let started = std::time::Instant::now();
        while !done() {
            assert!(started.elapsed() < limit, "{what}");
            std::thread::sleep(Duration::from_millis(50));
        }
        started.elapsed()
    };
    wait("no heartbeat", Duration::from_secs(20), &|| {
        fake.beats.load(Ordering::SeqCst) >= 2
    });
    // A controller that pings keeps the channel however long it runs.
    std::thread::sleep(Duration::from_secs(25));
    assert_eq!(
        fake.sessions.load(Ordering::SeqCst),
        1,
        "a live channel was dropped"
    );
    // The controller stalls with the TCP connection still open: no frame reaches the agent.
    fake.silent.store(true, Ordering::SeqCst);
    let took = wait(
        "the agent stayed on a silent controller",
        Duration::from_secs(35),
        &|| fake.sessions.load(Ordering::SeqCst) >= 2,
    );
    fake.silent.store(false, Ordering::SeqCst);
    assert!(
        took >= Duration::from_secs(15),
        "reconnected too eagerly: {took:?}"
    );
    let beats = fake.beats.load(Ordering::SeqCst);
    wait(
        "no heartbeat after reconnecting",
        Duration::from_secs(10),
        &|| fake.beats.load(Ordering::SeqCst) > beats,
    );
    let _ = fs::remove_dir_all(worker);
}

#[test]
fn ip_literals_need_no_lookup_slot() {
    assert_eq!(
        resolve("127.0.0.1", 8790).unwrap(),
        ["127.0.0.1:8790".parse().unwrap()]
    );
    assert_eq!(
        resolve("[::1]", 8790).unwrap(),
        ["[::1]:8790".parse().unwrap()]
    );
    assert!(!resolve("localhost", 8790).unwrap().is_empty());
}

type Channel = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

/// An agent's side of a session for `period`: a heartbeat each second and a pong to each
/// ping. Returns the longest wait between controller frames and whether it closed.
fn drive(ws: &mut Channel, period: Duration) -> (Duration, bool) {
    let heartbeat = json!({"type":"heartbeat","protocol":1,"backend":"firecracker","arch":"x86_64","runtime":"firecracker-v1.17.0","memory_mib":512,"slots":2,"vcpus":2,"images":["alpine"]}).to_string();
    let start = std::time::Instant::now();
    let (mut heard, mut gap) = (start, Duration::ZERO);
    let mut sent = start - Duration::from_secs(2);
    while start.elapsed() < period {
        if sent.elapsed() >= Duration::from_secs(1) {
            sent = std::time::Instant::now();
            if ws
                .send(tungstenite::Message::Text(heartbeat.clone().into()))
                .is_err()
            {
                return (gap, true);
            }
        }
        match ws.read() {
            Ok(tungstenite::Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => {
                return (gap, true);
            }
            Ok(frame) => {
                gap = gap.max(heard.elapsed());
                heard = std::time::Instant::now();
                if let tungstenite::Message::Ping(v) = frame {
                    let _ = ws.send(tungstenite::Message::Pong(v));
                }
            }
            Err(e) if timed_out(&e) => {}
            Err(_) => return (gap.max(heard.elapsed()), true),
        }
    }
    (gap.max(heard.elapsed()), false)
}

fn serve_controller(c: &Controller) -> (String, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let (app, c) = (routes(c.clone()), c.clone());
    let server = tokio::spawn(async move {
        tokio::spawn(proxies(c));
        axum::serve(listener, app).await.unwrap();
    });
    (origin, server)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_health_write_neither_stops_the_controller_nor_strands_a_runner() {
    let root = root();
    let alpha = enrollment(&root, "alpha");
    let c = Controller::new(&root).unwrap();
    let (origin, server) = serve_controller(&c);
    tokio::task::spawn_blocking(move || {
        let token = alpha["credential"].as_str().unwrap();
        let connect = || {
            let ws = socket(&origin, "/node/alpha", token, None).unwrap();
            timeout_socket(&ws).unwrap();
            ws
        };
        let online = || online(&root, "alpha").is_ok();
        let mut ws = connect();
        assert!(!drive(&mut ws, Duration::from_secs(4)).1 && online());

        // The catalog lost this session's mark (as a write lost in a storm would): its
        // heartbeats match no row, so the controller ends it and the runner reconnects.
        liveness_db(&root, "alpha")
            .unwrap()
            .execute("UPDATE health SET session='lost'", [])
            .unwrap();
        assert!(
            drive(&mut ws, Duration::from_secs(8)).1,
            "a session whose heartbeat matched no row stayed open"
        );
        let mut ws = connect();
        assert!(!drive(&mut ws, Duration::from_secs(4)).1 && online());

        // A capture's writeback holds this runner's health writes (its real writer lock, 8 s).
        let held = {
            let root = root.clone();
            std::thread::spawn(move || {
                let db = liveness_db(&root, "alpha").unwrap();
                db.execute_batch("BEGIN IMMEDIATE").unwrap();
                std::thread::sleep(Duration::from_secs(8));
                db.execute_batch("COMMIT").unwrap();
            })
        };
        std::thread::sleep(Duration::from_millis(200));
        // The controller keeps answering: it refuses a wrong credential at once ...
        let asked = std::time::Instant::now();
        assert!(socket(&origin, "/node/alpha", &"0".repeat(64), None).is_err());
        assert!(
            asked.elapsed() < Duration::from_secs(2),
            "{:?}",
            asked.elapsed()
        );
        // ... and keeps pinging the runner whose heartbeat waits for the disk.
        let (gap, closed) = drive(&mut ws, Duration::from_secs(4));
        assert!(!closed && gap < Duration::from_secs(4), "{gap:?}");
        // The write gives up after the busy timeout: that session ends, with a log line,
        // and the runner's reconnect is recorded once the disk answers again.
        assert!(
            drive(&mut ws, Duration::from_secs(6)).1,
            "a session whose heartbeat failed stayed open"
        );
        held.join().unwrap();
        let mut ws = connect();
        assert!(!drive(&mut ws, Duration::from_secs(4)).1 && online());
        let _ = fs::remove_dir_all(&root);
    })
    .await
    .unwrap();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runtime_stall_does_not_drop_the_sessions_it_delayed() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let root = root();
    let alpha = enrollment(&root, "alpha");
    let c = Controller::new(&root).unwrap();
    let (origin, server) = serve_controller(&c);
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let token = alpha["credential"].as_str().unwrap();
        let mut ws = socket(&origin, "/node/alpha", token, None).unwrap();
        timeout_socket(&ws).unwrap();
        assert!(!drive(&mut ws, Duration::from_secs(4)).1);
        let generation = c.sessions.lock().unwrap()["alpha"].generation.clone();
        // Both runtime threads block for 17 s (as on blocking disk I/O), past the 15 s cut-off.
        let started = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            let started = started.clone();
            runtime.spawn(async move {
                started.fetch_add(1, Ordering::SeqCst);
                let since = std::time::Instant::now();
                while started.load(Ordering::SeqCst) < 2 && since.elapsed() < Duration::from_secs(1)
                {
                }
                std::thread::sleep(Duration::from_secs(17));
            });
        }
        let (gap, closed) = drive(&mut ws, Duration::from_secs(24));
        assert_eq!(
            started.load(Ordering::SeqCst),
            2,
            "the stall held one thread only"
        );
        assert!(gap > Duration::from_secs(15), "no stall: {gap:?}");
        assert!(!closed, "the stall dropped the session");
        assert_eq!(c.sessions.lock().unwrap()["alpha"].generation, generation);
        let _ = fs::remove_dir_all(&root);
    })
    .await
    .unwrap();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_blocked_heartbeat_write_leaves_other_runners_online() {
    let root = root();
    // Names no other test uses: the stall hook is process-wide.
    let (stalled, healthy) = (enrollment(&root, "omega"), enrollment(&root, "sigma"));
    let c = Controller::new(&root).unwrap();
    let (origin, server) = serve_controller(&c);
    tokio::task::spawn_blocking(move || {
        let runner = |name: &'static str, credential: Value| {
            let origin = origin.clone();
            std::thread::spawn(move || {
                let token = credential["credential"].as_str().unwrap();
                let mut ws = socket(&origin, &format!("/node/{name}"), token, None).unwrap();
                timeout_socket(&ws).unwrap();
                drive(&mut ws, Duration::from_secs(40))
            })
        };
        let healthy_token = healthy["credential"].as_str().unwrap().to_owned();
        let (omega, sigma) = (runner("omega", stalled), runner("sigma", healthy));
        let started = std::time::Instant::now();
        while online(&root, "omega").is_err() || online(&root, "sigma").is_err() {
            assert!(started.elapsed() < Duration::from_secs(5), "never online");
            std::thread::sleep(Duration::from_millis(50));
        }
        // Omega's heartbeat write blocks in I/O for 23 s, past the 15 s offline cut-off.
        struct Release;
        impl Drop for Release {
            fn drop(&mut self) {
                *STALLED.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
        }
        let release = Release;
        if let Ok(arm) = std::env::var("OW_TEST_WAL_STALL") {
            fs::write(arm, b"armed").unwrap();
        } else {
            *STALLED.lock().unwrap() = Some("omega".into());
        }
        // Prove the fault holds SQLite's real writer lock, rather than a Rust mutex.
        let locked = Connection::open(root.join("nodes/omega/liveness.sqlite3")).unwrap();
        locked.busy_timeout(Duration::ZERO).unwrap();
        let waiting = std::time::Instant::now();
        loop {
            match locked.execute_batch("BEGIN IMMEDIATE") {
                Ok(()) => locked.execute_batch("ROLLBACK").unwrap(),
                Err(e) if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy) => break,
                Err(e) => panic!("unexpected lock error: {e}"),
            }
            assert!(
                waiting.elapsed() < Duration::from_secs(5),
                "the fault never held the WAL writer lock"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        // A catalog writer held too: health/session writes must not need that lock either.
        let catalog = db(&root).unwrap();
        catalog.execute_batch("BEGIN IMMEDIATE").unwrap();
        let stalled_at = std::time::Instant::now();
        let mut dispatched = false;
        while stalled_at.elapsed() < Duration::from_secs(23) {
            assert!(
                online(&root, "sigma").is_ok(),
                "a healthy runner went offline {:?} into another runner's stalled write",
                stalled_at.elapsed()
            );
            if !dispatched && stalled_at.elapsed() > Duration::from_secs(16) {
                let pool = inventory(&root).unwrap();
                assert_eq!(
                    pool.as_array()
                        .unwrap()
                        .iter()
                        .find(|n| n["id"] == "sigma")
                        .unwrap()["online"],
                    true
                );
                let asked = std::time::Instant::now();
                assert!(socket(&origin, "/node/sigma", &"0".repeat(64), None).is_err());
                assert!(
                    asked.elapsed() < Duration::from_secs(2),
                    "controller stopped answering"
                );
                // The real proxy must admit and finish a new job, past the old offline cut-off.
                let (mut local, upstream) = std::os::unix::net::UnixStream::pair().unwrap();
                local
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                upstream.set_nonblocking(true).unwrap();
                let task = tokio::spawn(proxy(
                    c.clone(),
                    "sigma".into(),
                    tokio::net::UnixStream::from_std(upstream).unwrap(),
                ));
                local.write_all(b"probe\n").unwrap();
                let asked = std::time::Instant::now();
                let id = loop {
                    if let Some(id) = c
                        .jobs
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(_, j)| j.node == "sigma")
                        .map(|(id, _)| id.clone())
                    {
                        break id;
                    }
                    assert!(
                        asked.elapsed() < Duration::from_secs(3),
                        "healthy runner refused a new job"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                };
                let mut job =
                    socket(&origin, &format!("/job/sigma/{id}"), &healthy_token, None).unwrap();
                timeout_socket(&job).unwrap();
                assert_eq!(job.read().unwrap().into_data().as_ref(), b"probe\n");
                job.send(tungstenite::Message::Binary(b"ok\n".to_vec().into()))
                    .unwrap();
                let mut reply = [0; 3];
                local.read_exact(&mut reply).unwrap();
                assert_eq!(&reply, b"ok\n");
                task.abort();
                dispatched = true;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        assert!(dispatched);
        catalog.execute_batch("ROLLBACK").unwrap();
        drop(release);
        let released = std::time::Instant::now();
        while online(&root, "omega").is_err() {
            assert!(
                released.elapsed() < Duration::from_secs(5),
                "the stalled runner did not come back"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // Both channels stayed up and kept getting frames throughout.
        for (name, runner) in [("omega", omega), ("sigma", sigma)] {
            let (gap, closed) = runner.join().unwrap();
            assert!(!closed && gap < Duration::from_secs(4), "{name}: {gap:?}");
        }
        let _ = fs::remove_dir_all(&root);
    })
    .await
    .unwrap();
    server.abort();
}
