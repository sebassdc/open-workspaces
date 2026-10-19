//! End-to-end HTTP/WS through the real gateway, with a stand-in worker TCP tunnel.
use super::tests::{capability_answer, capability_config};
use super::*;
use axum::{
    extract::ws::{Message, WebSocketUpgrade},
    routing::get,
};
use serde_json::json;
use std::future::IntoFuture;
use std::{
    os::unix::net::UnixListener,
    sync::atomic::{AtomicUsize, Ordering},
};
use tungstenite::client::IntoClientRequest;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tailnet_preview_http_websocket_and_revocation() {
    preview_http_websocket_and_revocation(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tailnet_capability_preview_http_websocket_and_revocation() {
    preview_http_websocket_and_revocation(true).await;
}

async fn preview_http_websocket_and_revocation(capability: bool) {
    let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend.local_addr().unwrap();
    let guest = tokio::spawn(
        axum::serve(
            backend,
            Router::new()
                .route(
                    "/socket",
                    get(|ws: WebSocketUpgrade| async {
                        ws.protocols(["vite-hmr"]).on_upgrade(|mut s| async move {
                            while let Some(Ok(m)) = s.recv().await {
                                match m {
                                    Message::Text(_) | Message::Binary(_) => {
                                        if s.send(m).await.is_err() {
                                            break;
                                        }
                                    }
                                    Message::Close(_) => break,
                                    _ => {}
                                }
                            }
                        })
                    }),
                )
                .fallback(|r: Request| async move {
                    assert!(!r.headers().contains_key("tailscale-user-login"));
                    assert!(!r.headers().contains_key("authorization"));
                    assert!(!r.headers().contains_key("cookie"));
                    assert!(!r.headers().contains_key("x-forwarded-for"));
                    (
                        [
                            ("set-cookie", "secret=guest"),
                            ("content-type", "text/html"),
                        ],
                        format!("guest {}", r.uri()),
                    )
                }),
        )
        .into_future(),
    );
    let root = std::env::temp_dir().join(format!("ow-preview-{}", crate::common::nonce().unwrap()));
    fs::create_dir(&root).unwrap();
    let mut config = capability_config();
    config.previews = true;
    if !capability {
        config.allowed_capability = None;
        config.allowed_emails = vec![
            "owner@example.test".into(),
            "stranger@example.test".into(),
            "outsider@example.test".into(),
        ];
    }
    let whois_dir = config.whois.cli.parent().unwrap().to_owned();
    for (ip, id, login) in [
        ("100.64.0.1", 42, "owner@example.test"),
        ("100.64.0.2", 43, "stranger@example.test"),
        ("100.64.0.3", 44, "outsider@example.test"),
    ] {
        let mut answer = capability_answer();
        answer["UserProfile"] = json!({"ID":id,"LoginName":login});
        fs::write(whois_dir.join(format!("{ip}.json")), answer.to_string()).unwrap();
    }
    let mut c = crate::catalog::Catalog::open(&root, &config.issuer(), None).unwrap();
    let owner = crate::catalog::Identity {
        issuer: config.issuer(),
        subject: "42".into(),
        email: "owner@example.test".into(),
    };
    let user = c.user(&owner).unwrap();
    let db = rusqlite::Connection::open(root.join("catalog.sqlite3")).unwrap();
    // Revocation watches are process-wide: parallel fixtures need distinct machine IDs and ports.
    let physical = crate::common::nonce().unwrap();
    db.execute("INSERT INTO resources(owner,kind,name,physical,metadata) VALUES(?1,'machine','demo',?2,'{\"state\":\"running\"}')",rusqlite::params![user, physical]).unwrap();
    db.execute(
        "INSERT INTO publications(https_port,physical,guest_port,active) VALUES(?1,?2,3000,0)",
        rusqlite::params![31000 + u16::from(capability), physical],
    )
    .unwrap();
    let row = c
        .publication(
            user,
            &json!({"op":"publish","id":"demo","port":3000}),
            |_, _| Ok(()),
        )
        .unwrap();
    c.operation(
        user,
        json!({"op":"share","id":"demo","with":"stranger@example.test"}),
    )
    .unwrap();
    let port = row["https_port"].as_u64().unwrap();
    let host = format!("{}:{port}", config.hostname);
    let catalog = Arc::new(std::sync::Mutex::new(c));
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let listener = UnixListener::bind(root.join("control.sock")).unwrap();
    let worker = std::thread::spawn(move || {
        for s in listener.incoming() {
            let mut s = s.unwrap();
            let Ok(req) = crate::wire::line(&mut s) else {
                break;
            };
            seen.fetch_add(1, Ordering::SeqCst);
            assert_eq!(req, json!({"op":"tunnel","id":physical,"port":3000}));
            let mut tcp = std::net::TcpStream::connect(backend_addr).unwrap();
            crate::wire::send(&mut s, &json!({"ok":true,"result":{}})).unwrap();
            std::thread::spawn(move || {
                let mut s2 = s.try_clone().unwrap();
                let mut tcp2 = tcp.try_clone().unwrap();
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut s2, &mut tcp2);
                    let _ = tcp2.shutdown(std::net::Shutdown::Write);
                });
                let _ = std::io::copy(&mut tcp, &mut s);
            });
        }
    });
    let app = App {
        dashboard: Some(root.clone()),
        catalog: Some(catalog.clone()),
        config,
        client: reqwest::Client::new(),
        keys: Arc::new(RwLock::new(Keys {
            set: JwkSet { keys: vec![] },
            fetched: Instant::now(),
        })),
        requests: Arc::new(Semaphore::new(64)),
    };
    let gateway = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = gateway.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(gateway, router(app.clone())).into_future());
    let client = reqwest::Client::new();
    let http = |login: &str, peer: &str| {
        client
            .get(format!("http://{addr}/assets/main.js?q=1"))
            .header("host", &host)
            .header("tailscale-user-login", login)
            .header("x-forwarded-for", peer)
    };
    for (login, peer, status) in [
        ("outsider@example.test", "100.64.0.3", 403),
        ("owner@example.test", "100.64.0.2", 401),
        ("", "100.64.0.1", 401),
    ] {
        let r = http(login, peer).send().await.unwrap();
        assert_eq!(r.status().as_u16(), status);
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "denials must precede worker calls"
    );
    for (login, peer) in [
        ("owner@example.test", "100.64.0.1"),
        ("stranger@example.test", "100.64.0.2"),
    ] {
        let r = http(login, peer)
            .header("authorization", "secret")
            .header("cookie", "secret=other")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(!r.headers().contains_key("set-cookie"));
        assert_eq!(r.text().await.unwrap(), "guest /assets/main.js?q=1");
    }
    let count = calls.load(Ordering::SeqCst);
    assert_eq!(
        http("owner@example.test", "100.64.0.1")
            .header("origin", "https://evil.test")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(calls.load(Ordering::SeqCst), count);
    for (method, origin, body, status) in [
        (Method::POST, None, "".to_owned(), StatusCode::FORBIDDEN),
        (
            Method::POST,
            Some(format!("https://{host}")),
            "x".repeat(1024 * 1024 + 1),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            Method::GET,
            Some("https://evil.test".to_owned()),
            "".to_owned(),
            StatusCode::FORBIDDEN,
        ),
    ] {
        let mut r = client
            .request(method, format!("http://{addr}/socket"))
            .header("host", &host)
            .header("tailscale-user-login", "owner@example.test")
            .header("x-forwarded-for", "100.64.0.1");
        if let Some(origin) = origin {
            r = r.header("origin", origin);
        }
        assert_eq!(r.body(body).send().await.unwrap().status(), status);
    }
    assert_eq!(calls.load(Ordering::SeqCst), count);
    // A preview port never dispatches the management API, even for its owner.
    let r = client
        .get(format!("http://{addr}/api/state"))
        .header("host", &host)
        .header("tailscale-user-login", "owner@example.test")
        .header("x-forwarded-for", "100.64.0.1")
        .send()
        .await
        .unwrap();
    assert_eq!(r.text().await.unwrap(), "guest /api/state");
    for reason in ["admission", "share", "unpublish"]
        .into_iter()
        .filter(|reason| capability || *reason != "admission")
    {
        if reason == "unpublish" {
            catalog
                .lock()
                .unwrap()
                .operation(
                    user,
                    json!({"op":"share","id":"demo","with":"stranger@example.test"}),
                )
                .unwrap();
        }
        let h = host.clone();
        let revoking_admission = reason == "admission";
        let mut socket = tokio::task::spawn_blocking(move || {
            let mut req = format!("ws://{addr}/socket").into_client_request().unwrap();
            for (k, v) in [
                ("host", h.clone()),
                ("origin", format!("https://{h}")),
                ("tailscale-user-login", "stranger@example.test".into()),
                ("x-forwarded-for", "100.64.0.2".into()),
                ("sec-websocket-protocol", "vite-hmr".into()),
            ] {
                req.headers_mut().insert(
                    axum::http::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    v.parse().unwrap(),
                );
            }
            let (mut socket, response) = tungstenite::connect(req).unwrap();
            assert_eq!(response.headers()["sec-websocket-protocol"], "vite-hmr");
            if let tungstenite::stream::MaybeTlsStream::Plain(tcp) = socket.get_mut() {
                tcp.set_read_timeout(Some(Duration::from_secs(if revoking_admission {
                    36
                } else {
                    3
                })))
                .unwrap();
            }
            socket
                .send(tungstenite::Message::Text("hot reload".into()))
                .unwrap();
            assert_eq!(
                socket.read().unwrap(),
                tungstenite::Message::Text("hot reload".into())
            );
            socket
        })
        .await
        .unwrap();
        if revoking_admission {
            let mut answer = capability_answer();
            answer["UserProfile"] = json!({"ID":43,"LoginName":"stranger@example.test"});
            answer["CapMap"] = json!({});
            fs::write(whois_dir.join("100.64.0.2.json"), answer.to_string()).unwrap();
        } else if reason == "share" {
            crate::catalog::share_unlocked(
                &root,
                &owner,
                &json!({"op":"unshare","id":"demo","with":"stranger@example.test"}),
            )
            .unwrap();
        } else {
            catalog
                .lock()
                .unwrap()
                .publication(user, &json!({"op":"unpublish","id":"demo"}), |_, _| Ok(()))
                .unwrap();
        }
        tokio::task::spawn_blocking(move || {
            let error=socket.read().unwrap_err();
            assert!(!matches!(error,tungstenite::Error::Io(ref e) if matches!(e.kind(),std::io::ErrorKind::TimedOut|std::io::ErrorKind::WouldBlock)),"revocation did not close websocket: {error}");
        }).await.unwrap();
        assert_eq!(
            http("stranger@example.test", "100.64.0.2")
                .send()
                .await
                .unwrap()
                .status(),
            if revoking_admission {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::FORBIDDEN
            }
        );
        if revoking_admission {
            let mut answer = capability_answer();
            answer["UserProfile"] = json!({"ID":43,"LoginName":"stranger@example.test"});
            fs::write(whois_dir.join("100.64.0.2.json"), answer.to_string()).unwrap();
            assert_eq!(
                http("stranger@example.test", "100.64.0.2")
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::OK
            );
        }
    }
    // Disabled preview mode and Cloudflare auth never expose preview routes.
    for access in [false, true] {
        let mut disabled = app.clone();
        if access {
            disabled.config.auth = Auth::Access;
        } else {
            disabled.config.previews = false;
        }
        let r = handle(
            &disabled,
            Request::builder()
                .uri("/")
                .header("host", &host)
                .header("tailscale-user-login", "owner@example.test")
                .header("x-forwarded-for", "100.64.0.1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!([StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN].contains(&r.status()));
    }
    server.abort();
    guest.abort();
    drop(std::os::unix::net::UnixStream::connect(root.join("control.sock")).unwrap());
    worker.join().unwrap();
    drop(db);
    drop(catalog);
    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(whois_dir).unwrap();
}
