//! Real ProxyCommand process over verified TLS; no VM or production login required.
use axum::{
    Router,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    routing::get,
};
use std::{
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const BYTES: usize = 100 * 1024 * 1024;
const FRAME: usize = 16384;

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Proxy(std::process::Child);
impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn transfer(mut ws: WebSocket, upload: bool) {
    if upload {
        let mut count = 0;
        while count < BYTES {
            match ws.recv().await.unwrap().unwrap() {
                Message::Binary(b) => {
                    assert!(!b.is_empty() && b.len() <= FRAME);
                    assert!(b.iter().all(|v| *v == 0xa5));
                    count += b.len();
                }
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("unexpected upload frame {other:?}"),
            }
        }
        assert_eq!(count, BYTES);
        ws.send(Message::Binary(count.to_be_bytes().to_vec().into()))
            .await
            .unwrap();
    } else {
        for _ in 0..BYTES / FRAME {
            ws.send(Message::Binary(vec![0x5a; FRAME].into()))
                .await
                .unwrap();
        }
    }
    ws.send(Message::Close(None)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_proxy_upload_100_mib() {
    check("upload").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_proxy_download_100_mib() {
    check("download").await;
}

async fn check(direction: &'static str) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let root = Fixture(std::env::temp_dir().join(format!(
        "ow-ssh-throughput-{}-{direction}",
        std::process::id()
    )));
    std::fs::create_dir(&root.0).unwrap();
    // Private fixture trust only, passed to the child; never disable certificate verification.
    let status = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=DNS:localhost",
            "-addext",
            "basicConstraints=critical,CA:FALSE",
            "-keyout",
        ])
        .arg(root.0.join("key.pem"))
        .arg("-out")
        .arg(root.0.join("cert.pem"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let config = root.0.join("open-workspaces");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(
        config.join("client.json"),
        serde_json::json!({"server":url,"auth":"tailscale"}).to_string(),
    )
    .unwrap();
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(
        root.0.join("cert.pem"),
        root.0.join("key.pem"),
    )
    .await
    .unwrap();
    let app = Router::new()
        .route(
            "/api/ssh/upload",
            get(|ws: WebSocketUpgrade| async {
                ws.max_message_size(FRAME).on_upgrade(|s| transfer(s, true))
            }),
        )
        .route(
            "/api/ssh/download",
            get(|ws: WebSocketUpgrade| async { ws.on_upgrade(|s| transfer(s, false)) }),
        );
    let server = tokio::spawn(
        axum_server::from_tcp_rustls(listener, tls)
            .unwrap()
            .serve(app.into_make_service()),
    );
    {
        let mut proxy = Proxy(
            Command::new(env!("CARGO_BIN_EXE_ow"))
                .args(["--server", &url, "ssh-proxy", direction])
                .env("XDG_CONFIG_HOME", &root.0)
                .env("SSL_CERT_FILE", root.0.join("cert.pem"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut input = proxy.0.stdin.take().unwrap();
        let mut output = proxy.0.stdout.take().unwrap();
        let started = Instant::now();
        let work = tokio::task::spawn_blocking(move || {
            if direction == "upload" {
                for _ in 0..BYTES / FRAME {
                    input.write_all(&[0xa5; FRAME]).unwrap();
                }
                // Keep stdin open until the peer acknowledges the byte count (SSH's contract).
                let mut count = [0; size_of::<usize>()];
                output.read_exact(&mut count).unwrap();
                assert_eq!(usize::from_be_bytes(count), BYTES);
            } else {
                let mut buf = [0; FRAME];
                let mut count = 0;
                loop {
                    let n = output.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    assert!(buf[..n].iter().all(|v| *v == 0x5a));
                    count += n;
                }
                assert_eq!(count, BYTES);
            }
        });
        match tokio::time::timeout(Duration::from_secs(30), work).await {
            Ok(result) => result.unwrap(),
            Err(_) => panic!("{direction} of {BYTES} bytes exceeded 30 s"),
        }
        let status = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(status) = proxy.0.try_wait().unwrap() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("SSH proxy did not exit after transfer");
        let mut diagnostics = String::new();
        proxy
            .0
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut diagnostics)
            .unwrap();
        assert!(status.success(), "{diagnostics}");
        eprintln!(
            "ssh-proxy {direction}: {BYTES} bytes in {:.3}s",
            started.elapsed().as_secs_f64()
        );
    }
    server.abort();
}
