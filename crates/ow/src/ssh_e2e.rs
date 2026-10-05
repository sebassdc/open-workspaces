// Included only in remote::tests: synthetic identity issuer is never production code.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Requires planner-exclusive one-guest turn, explicit OW_SSH_TEST_ROOT/BINARY/ASSETS"]
async fn openssh_guest_real_vm() {
    use std::os::unix::fs::PermissionsExt;
    let root =
        PathBuf::from(std::env::var_os("OW_SSH_TEST_ROOT").expect("explicit fresh test root"));
    assert!(!root.exists());
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let status = std::process::Command::new("python")
        .arg(repo.join("experiments/ssh-guest/acceptance.py"))
        .arg("--prepare")
        .arg(&root)
        .status()
        .unwrap();
    assert!(status.success());
    let (key, set) = signing_key();
    let mut c = claims();
    c["exp"] = json!(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 1800
    );
    std::fs::write(root.join("token"), token(&key, &c)).unwrap();
    c["sub"] = json!("foreign-owner");
    c["email"] = json!("foreign@example.test");
    std::fs::write(root.join("foreign-token"), token(&key, &c)).unwrap();
    let ctl = root.join("c");
    std::fs::create_dir(&ctl).unwrap();
    let status = std::process::Command::new(std::env::var_os("OW_SSH_TEST_BINARY").unwrap())
        .args(["--local", "--data-dir", ctl.to_str().unwrap(), "nodes"])
        .status()
        .unwrap();
    assert!(status.success());
    let catalog =
        crate::catalog::Catalog::open(&ctl, "https://test-team.cloudflareaccess.com", None)
            .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let origin = format!("https://localhost:{port}");
    std::fs::write(root.join("origin"), &origin).unwrap();
    let mut cfg = config("http://127.0.0.1:1".into());
    cfg.hostname = format!("localhost:{port}");
    cfg.allowed_emails.push("foreign@example.test".into());
    let app = App {
        dashboard: Some(ctl),
        catalog: Some(Arc::new(std::sync::Mutex::new(catalog))),
        config: cfg,
        client: reqwest::Client::new(),
        keys: Arc::new(RwLock::new(Keys {
            set,
            fetched: Instant::now(),
        })),
        requests: Arc::new(Semaphore::new(64)),
    };
    // Fixture emulates Cloudflare header injection after the client's token was
    // transported over private CA TLS. Signature/issuer/subject still verified.
    static REDIRECT_FOLLOWED: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    async fn inject(mut req: Request, next: axum::middleware::Next) -> Response {
        if req.uri().path() == "/api/ssh/fixture-redirect" {
            return (
                StatusCode::FOUND,
                [(
                    "location",
                    format!(
                        "https://{}/api/ssh/fixture-follow",
                        req.headers().get("host").unwrap().to_str().unwrap()
                    ),
                )],
            )
                .into_response();
        }
        if req.uri().path() == "/api/ssh/fixture-follow" {
            REDIRECT_FOLLOWED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        if req.uri().path() == "/api/ssh/fixture-stall" {
            tokio::time::sleep(Duration::from_secs(12)).await;
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        if let Some(v) = req.headers().get("cf-access-token").cloned() {
            req.headers_mut().insert("cf-access-jwt-assertion", v);
        }
        next.run(req).await
    }
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(
        root.join("leaf.pem"),
        root.join("leaf.key"),
    )
    .await
    .unwrap();
    let task = tokio::spawn(
        axum_server::bind_rustls(std::net::SocketAddr::from(([127, 0, 0, 1], port)), tls).serve(
            router(app)
                .layer(axum::middleware::from_fn(inject))
                .into_make_service(),
        ),
    );
    let run_root = root.clone();
    let run_repo = repo.clone();
    let result = tokio::task::spawn_blocking(move || {
        std::process::Command::new("python")
            .arg(run_repo.join("experiments/ssh-guest/acceptance.py"))
            .arg(&run_root)
            .status()
    })
    .await
    .unwrap()
    .unwrap();
    task.abort();
    assert_eq!(
        REDIRECT_FOLLOWED.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "proxy followed redirect"
    );
    assert!(result.success(), "see dedicated acceptance result/log");
}
