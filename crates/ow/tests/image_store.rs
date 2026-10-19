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
    let aws = root.0.join("aws");
    fs::write(&aws, r#"#!/usr/bin/env python3
import sys,json,pathlib,hashlib,base64
root=pathlib.Path(__file__).parent/'objects'
a=sys.argv[1:]
def arg(k): return a[a.index(k)+1]
def digest(b): return base64.b64encode(hashlib.sha256(b).digest()).decode()
if a[:2]==['s3','cp']:
    sys.stdout.buffer.write((root/a[-2].removeprefix('s3://')).read_bytes());sys.exit(0)
op=a[1]
assert arg('--bucket')=='bucket'
if op=='get-bucket-versioning':out={'Status':'Enabled'}
elif op=='get-bucket-lifecycle-configuration':out={'Rules':[{'Status':'Enabled','Filter':{'Prefix':'root/store/'},'NoncurrentVersionExpiration':{'NoncurrentDays':7}}]}
elif op=='get-bucket-policy':out={'Policy':json.dumps({'Statement':[{'Effect':'Deny','Principal':'*','Action':'s3:PutObject','Resource':'arn:aws:s3:::bucket/root/store/v1/*','Condition':{'Null':{'s3:if-none-match':'true'},'Bool':{'s3:ObjectCreationOperation':'true'}}}]})}
else:
    path=root/'bucket'/arg('--key')
    if op=='put-object':
        assert arg('--if-none-match')=='*'
        data=pathlib.Path(arg('--body')).read_bytes();assert digest(data)==arg('--checksum-sha256')
        path.parent.mkdir(parents=True,exist_ok=True)
        with path.open('xb') as f:f.write(data)
        out={}
    elif op=='head-object':
        data=path.read_bytes();out={'ChecksumSHA256':digest(data),'ContentLength':len(data)}
    else:raise AssertionError(op)
print(json.dumps(out))
"#).unwrap();
    fs::set_permissions(&aws, fs::Permissions::from_mode(0o700)).unwrap();
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
