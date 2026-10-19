//! Opt-in two-worker Firecracker proof. Never targets a pre-existing worker or workspace.
use super::*;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    path::PathBuf,
    process::{Command, Stdio},
};

struct Guests {
    roots: Vec<PathBuf>,
    binary: PathBuf,
}
impl Drop for Guests {
    fn drop(&mut self) {
        for root in &self.roots {
            // This fixture owns each complete worker; quiesce captures only its disposable guests.
            let _ = Command::new(&self.binary)
                .args(["--local", "--data-dir", root.to_str().unwrap(), "down"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}
fn rpc_vm(root: &Path, request: Value) -> Value {
    wire::request(root, request).unwrap()
}
fn exec_vm(root: &Path, id: &str, placement: Option<common::Placement>, command: &str) -> String {
    let mut r = json!({"op":"exec","id":id,"command":command});
    if let Some(p) = placement {
        r["placement"] = json!(p);
    }
    let v = rpc_vm(root, r);
    assert_eq!(v["exit_code"], 0, "{v}");
    v["output"].as_str().unwrap().into()
}
fn ssh_vm(
    root: &Path,
    runner: &Path,
    id: &str,
    placement: Option<common::Placement>,
    command: &str,
) -> String {
    let envelope = json!({"op":"ssh","id":id,"placement":placement});
    let mut envelope = envelope;
    if envelope["placement"].is_null() {
        envelope.as_object_mut().unwrap().remove("placement");
    }
    fs::write(
        root.join("ssh-request.json"),
        serde_json::to_vec(&envelope).unwrap(),
    )
    .unwrap();
    let output = Command::new("ssh")
        .args([
            "-F",
            "/dev/null",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "HostKeyAlias=move-proof",
            "-o",
            &format!("UserKnownHostsFile={}", root.join("known_hosts").display()),
            "-o",
            &format!(
                "ProxyCommand=python3 {} {} {}",
                root.join("proxy.py").display(),
                runner.join("control.sock").display(),
                root.join("ssh-request.json").display()
            ),
            "-i",
            root.join("key").to_str().unwrap(),
            "dev@move-proof",
            command,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
#[test]
#[ignore = "Requires explicit fresh OW_MOVE_TEST_ROOT, OW_MOVE_TEST_BINARY and OW_ASSET_DIR on a KVM/Btrfs host"]
fn move_fork_real_vm_counter_ssh_grants_and_independent_writes() {
    use std::os::unix::fs::PermissionsExt;
    let root = PathBuf::from(std::env::var_os("OW_MOVE_TEST_ROOT").expect("explicit fresh root"));
    assert!(!root.exists());
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let binary =
        PathBuf::from(std::env::var_os("OW_MOVE_TEST_BINARY").expect("explicit test executable"));
    let roots = vec![root.join("source"), root.join("target")];
    let guests = Guests {
        roots: roots.clone(),
        binary: binary.clone(),
    };
    for runner in &roots {
        assert!(!runner.exists());
        let out = Command::new(&binary)
            .args(["--local", "--data-dir", runner.to_str().unwrap(), "up"])
            .env("OW_MAX_MEMORY_MIB", "1024")
            .env("OW_MAX_RUNNING", "2")
            .env("OW_MAX_VCPUS", "2")
            .env("OW_MIN_FREE_GIB", "1")
            .env("OW_IDLE_HIBERNATE_SECS", "0")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(rpc_vm(runner, json!({"op":"list"})), json!([]));
    }
    let disk_gib: u32 = std::env::var("OW_MOVE_TEST_DISK_GIB")
        .unwrap_or_else(|_| "8".into())
        .parse()
        .unwrap();
    let original = rpc_vm(
        &roots[0],
        json!({"op":"create","id":"move-parent","image":"ubuntu","memory_mib":256,"vcpu_count":1,"disk_gib":disk_gib}),
    );
    assert_eq!(original["state"], "running");
    assert!(
        Command::new("ssh-keygen")
            .args([
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-f",
                root.join("key").to_str().unwrap()
            ])
            .status()
            .unwrap()
            .success()
    );
    let key = fs::read_to_string(root.join("key.pub")).unwrap();
    let enrolled = rpc_vm(
        &roots[0],
        json!({"op":"ssh-keys","id":"move-parent","keys":[key.trim()]}),
    );
    fs::write(
        root.join("known_hosts"),
        format!("move-proof {}\n", enrolled["host_key"].as_str().unwrap()),
    )
    .unwrap();
    fs::write(root.join("proxy.py"),r#"import json,socket,sys,select,os
s=socket.socket(socket.AF_UNIX);s.connect(sys.argv[1]);s.sendall(open(sys.argv[2],'rb').read()+b'\n')
f=s.makefile('rb',buffering=0);line=f.readline();v=json.loads(line);assert v['ok'],v
while True:
 r,_,_=select.select([s,sys.stdin.buffer],[],[],10)
 for x in r:
  b=os.read(x.fileno(),16384)
  if not b:sys.exit(0)
  if x is s:sys.stdout.buffer.write(b);sys.stdout.buffer.flush()
  else:s.sendall(b)
"#).unwrap();
    exec_vm(
        &roots[0],
        "move-parent",
        None,
        "nohup sh -c 'echo $$ > /home/dev/counter-pid; n=0; while :; do n=$((n+1)); echo $n >/home/dev/counter.new; mv /home/dev/counter.new /home/dev/counter; sleep 1; done' </dev/null >/home/dev/counter.log 2>&1 & echo parent >/home/dev/branch",
    );
    std::thread::sleep(Duration::from_secs(2));
    let observe = "cat /home/dev/counter; cat /proc/$(cat /home/dev/counter-pid)/stat; cat /proc/sys/kernel/random/boot_id; cat /etc/machine-id";
    let before = ssh_vm(&root, &roots[0], "move-parent", None, observe);
    fs::create_dir(root.join("controller")).unwrap();
    fs::create_dir(root.join("holder")).unwrap();
    let mut c = Catalog::open(&root.join("controller"), "fixture", None).unwrap();
    let user = c
        .user(&Identity {
            issuer: "fixture".into(),
            subject: "owner".into(),
            email: "owner@example.test".into(),
        })
        .unwrap();
    let teammate = c
        .user(&Identity {
            issuer: "fixture".into(),
            subject: "teammate".into(),
            email: "teammate@example.test".into(),
        })
        .unwrap();
    let source_policy = job(
        &roots[0],
        &json!({"op":"restore-policy","id":"move-parent"}),
    )
    .unwrap();
    c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node,ssh_policy) VALUES(?1,'machine','box','move-parent',?2,'source',?3)",params![user,original.to_string(),source_policy.to_string()]).unwrap();
    c.db.execute(
        "INSERT INTO grants(physical,grantee,role) VALUES('move-parent',?1,'use')",
        [teammate],
    )
    .unwrap();
    fs::write(c.root.join("store-url"), "s3://fixture/root").unwrap();
    for (i, node) in ["source", "target"].iter().enumerate() {
        let mut caps = rpc_vm(&roots[i], json!({"op":"status"}));
        caps["memory_mib"] = json!(1024);
        caps["slots"] = json!(2);
        caps["vcpus"] = json!(2);
        caps["managed_restore_v1"] = json!(true);
        c.db.execute(
            "INSERT INTO nodes(id,memory_mib,slots,heartbeat,capabilities) VALUES(?1,1024,2,?2,?3)",
            params![node, now(), caps.to_string()],
        )
        .unwrap();
    }
    let shared = Arc::new(Mutex::new(c));
    let objects = store::DirectoryObjects(root.join("objects"));
    let published = RefCell::new(BTreeMap::<String, Value>::new());
    let perform = |kind: Kind, actor: i64, key: &str, child: Option<Child>| {
        let t = {
            let c = shared.lock().unwrap();
            let mut input = parking::shape(
                &c,
                "move-parent",
                kind,
                if actor == user {
                    "box"
                } else {
                    "owner@example.test/box"
                },
            )
            .unwrap();
            input.target = Some(
                if kind == Kind::Move {
                    "target"
                } else {
                    "source"
                }
                .into(),
            );
            input.child = child;
            let t = c.begin_transfer(actor, key, &input).unwrap();
            if kind == Kind::Fork {
                c.db.execute(
                    "UPDATE operations SET response=json_object('fork_snapshot',?1) WHERE id=?2",
                    params![format!("{}-fork", name(&t)), t.operation],
                )
                .unwrap();
            }
            t
        };
        for _ in 0..30 {
            {
                let c = shared.lock().unwrap();
                c.db.execute("UPDATE nodes SET heartbeat=?1,capabilities=json_set(capabilities,'$.disk_observed_unix_s',?1)",[now()]).unwrap();
            }
            eprintln!(
                "VM proof {kind:?} transfer={} step={:?}",
                t.id,
                shared.lock().unwrap().transfer(t.id).unwrap().step
            );
            let more = tick(&shared, t.id, now(), &|node, r| {
                let runner = &roots[usize::from(node == "target")];
                match r["op"].as_str().unwrap() {
                    "capture-published" => Ok(published
                        .borrow()
                        .get(r["artifact"]["reference"]["attempt"].as_str().unwrap())
                        .cloned()
                        .unwrap_or(json!({"present":false}))),
                    "capture-push" => {
                        let a: Artifact = serde_json::from_value(r["artifact"].clone())?;
                        let result = capture_upload::publish(&objects, runner, &a)?;
                        published
                            .borrow_mut()
                            .insert(a.reference.attempt, result.clone());
                        Ok(result)
                    }
                    "capture-cache" => {
                        cache(runner, &serde_json::from_value(r["artifact"].clone())?)
                    }
                    "capture-pull" => pull(
                        &objects,
                        runner,
                        &serde_json::from_value(r["artifact"].clone())?,
                        r["attempt"].as_str().unwrap(),
                    ),
                    "restore-policy" => job(runner, &r),
                    _ => wire::request(runner, r),
                }
            })
            .unwrap();
            // Reopen the catalog after every transition, with real worker effects retained.
            let holder = Catalog::open(&root.join("holder"), "fixture", None).unwrap();
            let old = std::mem::replace(&mut *shared.lock().unwrap(), holder);
            drop(old);
            let reopened = Catalog::open(&root.join("controller"), "fixture", None).unwrap();
            let holder = std::mem::replace(&mut *shared.lock().unwrap(), reopened);
            drop(holder);
            if !more {
                break;
            }
        }
        let c = shared.lock().unwrap();
        assert_eq!(c.transfer(t.id).unwrap().step, Step::Succeeded);
        t
    };
    let moved = perform(Kind::Move, user, "move", None);
    let placement = shared
        .lock()
        .unwrap()
        .managed_placement("move-parent")
        .unwrap()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let after = ssh_vm(
        &root,
        &roots[1],
        "move-parent",
        Some(placement.clone()),
        observe,
    );
    let b: Vec<_> = before.lines().collect();
    let a: Vec<_> = after.lines().collect();
    assert!(a[0].parse::<u64>().unwrap() > b[0].parse::<u64>().unwrap());
    assert_eq!(
        a[1].split_whitespace().next(),
        b[1].split_whitespace().next()
    );
    assert_eq!(
        a[1].rsplit(')').next().unwrap().split_whitespace().nth(19),
        b[1].rsplit(')').next().unwrap().split_whitespace().nth(19)
    );
    assert_eq!(&a[2..], &b[2..]);
    let c = shared.lock().unwrap();
    assert_eq!(
        c.machine(teammate, "owner@example.test/box", "exec")
            .unwrap(),
        "move-parent"
    );
    drop(c);
    assert!(wire::request(&roots[0],json!({"op":"start","id":"move-parent","placement":{"epoch":moved.epoch,"node":"source","transfer":name(&moved)}})).is_err());
    let down = Command::new(&binary)
        .args(["--local", "--data-dir", roots[0].to_str().unwrap(), "down"])
        .output()
        .unwrap();
    assert!(down.status.success());
    let up = Command::new(&binary)
        .args(["--local", "--data-dir", roots[0].to_str().unwrap(), "up"])
        .env("OW_MAX_MEMORY_MIB", "1024")
        .env("OW_MAX_RUNNING", "2")
        .env("OW_MAX_VCPUS", "2")
        .env("OW_MIN_FREE_GIB", "1")
        .env("OW_IDLE_HIBERNATE_SECS", "0")
        .output()
        .unwrap();
    assert!(up.status.success());
    assert!(wire::request(&roots[0], json!({"op":"start","id":"move-parent"})).is_err());
    assert!(wire::request(&roots[0],json!({"op":"start","id":"move-parent","placement":{"epoch":moved.epoch,"node":"source","transfer":name(&moved)}})).is_err());
    // A use-role teammate forks to the other worker and owns the new child.
    let child = perform(
        Kind::Fork,
        teammate,
        "fork",
        Some(Child {
            id: "fork-child".into(),
            name: "child".into(),
        }),
    );
    assert_eq!(
        shared
            .lock()
            .unwrap()
            .db
            .query_row(
                "SELECT owner FROM resources WHERE physical='fork-child'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        teammate
    );
    let child_placement = shared
        .lock()
        .unwrap()
        .managed_placement("fork-child")
        .unwrap()
        .unwrap();
    assert!(
        wire::request(
            &roots[0],
            json!({"op":"ssh-info","id":"fork-child","placement":child_placement})
        )
        .is_err()
    );
    exec_vm(
        &roots[0],
        "fork-child",
        Some(child_placement.clone()),
        "echo child >/home/dev/branch",
    );
    assert_eq!(
        exec_vm(
            &roots[1],
            "move-parent",
            Some(placement),
            "cat /home/dev/branch"
        )
        .trim(),
        "parent"
    );
    let child_identity = exec_vm(
        &roots[0],
        "fork-child",
        Some(child_placement.clone()),
        "cat /etc/machine-id",
    );
    assert_ne!(child_identity.trim(), a[3]);
    let child_ssh = rpc_vm(
        &roots[0],
        json!({"op":"ssh-keys","id":"fork-child","placement":child_placement,"keys":[key.trim()]}),
    );
    assert_ne!(child_ssh["host_key"], enrolled["host_key"]);
    assert_ne!(child_ssh["identity"], enrolled["identity"]);
    fs::write(root.join("evidence.json"),json!({"passed":true,"topology":"two dedicated workers on one host","move":moved,"fork":child,"before":before,"after":after,"parent_ssh":enrolled,"child_ssh":child_ssh,"host":std::env::consts::ARCH}).to_string()).unwrap();
    drop(guests);
}
