//! Real image CLI, private fake S3 and worker; no AWS credentials or KVM required.
#![cfg(target_os = "linux")]
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct S3(std::process::Child);
impl Drop for S3 {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Worker(Arc<AtomicBool>, Option<std::thread::JoinHandle<()>>);
impl Drop for Worker {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
        let _ = self.1.take().unwrap().join();
    }
}

#[test]
fn default_controller_build_uses_sibling_runner_and_commits_image() {
    let root = Fixture(
        std::env::temp_dir().join(format!("ow-image-cli-{}", ow_core::store::token().unwrap())),
    );
    let controller = root.0.join("controller");
    let runner = root.0.join("runner");
    let assets = root.0.join("assets");
    for path in [
        &controller,
        &runner,
        &assets.join("guest"),
        &assets.join("downloads"),
    ] {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::set_permissions(&root.0, fs::Permissions::from_mode(0o700)).unwrap();
    let catalog = controller.join("catalog.sqlite3");
    let db = rusqlite::Connection::open(&catalog).unwrap();
    db.execute_batch("CREATE TABLE users(id INTEGER PRIMARY KEY); CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL); PRAGMA user_version=3;").unwrap();
    fs::set_permissions(&catalog, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(runner.join("store-url"), "s3://bucket/root").unwrap();
    fs::write(assets.join("guest/ubuntu.ext4"), b"base disk").unwrap();
    fs::write(assets.join("downloads/vmlinux-6.1.186"), b"fixture kernel").unwrap();
    let recipe = root.0.join("tools.sh");
    fs::write(&recipe, "# ow-image: base=ubuntu\ntrue\n").unwrap();
    // Objects move through the loopback S3 fixture over root/objects.
    fs::create_dir_all(root.0.join("objects")).unwrap();
    // The bucket answers as a safe write-once store.
    fs::write(root.0.join("objects/write-once-policy"), b"").unwrap();
    fs::write(
        root.0.join("s3"),
        include_bytes!("../../ow-core/tests/fake_s3.py"),
    )
    .unwrap();
    let mut s3 = S3(Command::new("python3")
        .arg(root.0.join("s3"))
        .arg(root.0.join("objects"))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap());
    let mut port = String::new();
    BufReader::new(s3.0.stdout.take().unwrap())
        .read_line(&mut port)
        .unwrap();
    let endpoint = format!("http://127.0.0.1:{}", port.trim());
    let listener = UnixListener::bind(runner.join("control.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let calls = Arc::new(Mutex::new(Vec::<Value>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (worker_calls, worker_stop, worker_root) = (calls.clone(), stop.clone(), runner.clone());
    let worker = Worker(
        stop,
        Some(std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(&mut stream).read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                worker_calls.lock().unwrap().push(request.clone());
                let result = match request["op"].as_str().unwrap() {
                    "status" => {
                        json!({"reserved_disk_bytes":0,"min_free_gib":5,"max_memory_mib":4096})
                    }
                    "create" => {
                        assert_eq!(request["image"], "ubuntu@1");
                        json!({})
                    }
                    "image-save" => {
                        let name = request["name"].as_str().unwrap();
                        let path = worker_root.join("images").join(name);
                        fs::create_dir(&path).unwrap();
                        fs::write(path.join("disk.ext4"), b"golden disk").unwrap();
                        let local = json!({"name":name,"base":"ubuntu@1","recipe_sha256":request["recipe_sha256"],"disk_sha256":ow_core::store::sha256(b"golden disk")});
                        fs::write(path.join("manifest.json"), local.to_string()).unwrap();
                        local
                    }
                    "shell-exec" => json!({"output":"0\n","exit_code":0}),
                    "put" | "stop" | "delete" => json!({}),
                    other => panic!("unexpected worker operation {other}"),
                };
                writeln!(stream, "{}", json!({"ok":true,"result":result})).unwrap();
            }
        })),
    );
    let run = |args: &[&str]| -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_ow"))
            .arg("--data-dir")
            .arg(&root.0)
            .arg("image")
            .args(args)
            .env("OW_ASSET_DIR", &assets)
            .env("XDG_CONFIG_HOME", root.0.join("client-config"))
            .env("OW_SERVER", "http://fixture.invalid")
            .env("OW_TEST_S3_ENDPOINT", &endpoint)
            // The SDK's credential chain finds fixture keys first; no profile, no instance role.
            .env("AWS_ACCESS_KEY_ID", "AKIDFIXTURE")
            .env("AWS_SECRET_ACCESS_KEY", "fixture")
            .env("AWS_REGION", "us-east-1")
            .env("AWS_CONFIG_FILE", root.0.join("no-config"))
            .env("AWS_SHARED_CREDENTIALS_FILE", root.0.join("no-credentials"))
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env(
                "PATH",
                std::env::join_paths(
                    std::iter::once(root.0.clone())
                        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
                )
                .unwrap(),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    run(&["publish", "ubuntu", "--to", "s3://bucket/root"]);
    run(&["pull", "ubuntu@1"]);
    let args = [
        "build",
        recipe.to_str().unwrap(),
        "--publish-to",
        "s3://bucket/root",
        "--base-image",
        "ubuntu@1",
    ];
    let built = run(&args);
    assert_eq!(built["status"], "current");
    let before = calls.lock().unwrap().len();
    assert_eq!(
        run(&args)["reference"],
        built["reference"],
        "same build is a no-op"
    );
    assert_eq!(calls.lock().unwrap().len(), before);
    run(&["pull", "tools@1"]);
    assert!(
        run(&["list", "--catalog"])
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["name"] == "tools" && i["status"] == "current")
    );
    assert_eq!(
        fs::read(runner.join("images/tools@1/disk.ext4")).unwrap(),
        b"golden disk"
    );
    assert!(
        runner
            .join("image-jobs")
            .join(built["reference"]["attempt"].as_str().unwrap())
            .is_file()
    );
    assert!(!controller.join("image-jobs").exists());
    assert!(!controller.join("store-url").exists());
    assert!(!controller.join("images").exists());
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["op"] == "create")
            .count(),
        1
    );
    drop(worker);
}
