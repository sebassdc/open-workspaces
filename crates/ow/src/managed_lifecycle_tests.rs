//! Delete, resize, snapshot and public-key changes of a box after a real move or a
//! cross-runner fork: the durable transfer runs first, then the ordinary operations reach
//! runner sockets that refuse any placement other than the copy's own, as workers do.
use super::*;
use std::{
    os::unix::net::UnixListener,
    sync::atomic::{AtomicBool, Ordering},
};

/// One runner's socket. It keeps the copies a transfer left there and records every request.
struct Runner {
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Runner {
    fn serve(root: &Path, node: &str, copies: Vec<Value>) -> Self {
        Self::serve_with(root, node, copies, Vec::new())
    }
    /// Also holding these named snapshots, as the worker lists them.
    fn serve_with(root: &Path, node: &str, copies: Vec<Value>, held: Vec<Value>) -> Self {
        let path = root.join("nodes").join(node);
        fs::create_dir_all(&path).unwrap();
        let listener = UnixListener::bind(path.join("control.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen, done) = (requests.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let mut machines: BTreeMap<String, Value> = copies
                .into_iter()
                .map(|m| (m["id"].as_str().unwrap().to_owned(), m))
                .collect();
            let mut snapshots: BTreeMap<String, Value> = held
                .into_iter()
                .map(|s| (s["name"].as_str().unwrap().to_owned(), s))
                .collect();
            while !done.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                let request = wire::line(&mut stream).unwrap();
                seen.lock().unwrap().push(request.clone());
                let reply = match answer(&mut machines, &mut snapshots, &request) {
                    Ok(result) => json!({"ok":true,"result":result}),
                    Err(error) => json!({"ok":false,"error":format!("{error:#}")}),
                };
                wire::send(&mut stream, &reply).unwrap();
            }
        });
        Self {
            requests,
            stop,
            thread: Some(thread),
        }
    }
    fn ops(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
    fn last(&self, op: &str) -> Value {
        self.ops()
            .into_iter()
            .rev()
            .find(|r| r["op"] == op)
            .unwrap_or_else(|| panic!("no {op} request"))
    }
}
impl Drop for Runner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn answer(
    machines: &mut BTreeMap<String, Value>,
    snapshots: &mut BTreeMap<String, Value>,
    request: &Value,
) -> Result<Value> {
    let op = request["op"].as_str().unwrap();
    match op {
        "list" => return Ok(json!(machines.values().collect::<Vec<_>>())),
        "snapshots" => return Ok(json!(snapshots.values().collect::<Vec<_>>())),
        "status" | "stats" => return Ok(json!({})),
        _ => {}
    }
    let id = request["id"].as_str().unwrap();
    let m = machines.get_mut(id).context("workspace not found")?;
    if op == "inspect" {
        return Ok(m.clone());
    }
    ensure!(
        request["placement"] == m["placement"],
        "stale or wrong placement epoch, node or transfer"
    );
    let phase = m["lifecycle"]["phase"].clone();
    let deleted = m["state"] == "deleted";
    if op == "export-cleanup" {
        ensure!(
            phase == "exported" && request["transfer"] == m["lifecycle"]["intent"]["transfer"],
            "source has not completed export"
        );
        m["lifecycle"]["phase"] = json!("cleaned");
        return Ok(m.clone());
    }
    if op == "snapshot-delete" {
        let name = request["name"].as_str().unwrap();
        let existed = snapshots.remove(name).is_some();
        return Ok(json!({"deleted":name,"workspace":id,"existed":existed}));
    }
    if op == "delete" && deleted {
        return Ok(json!({"deleted":id,"snapshots":[]}));
    }
    // Like the worker: an old fenced source may still be deleted, nothing else runs on it.
    ensure!(
        phase == "active" || op == "delete" && phase == "cleaned",
        "managed workspace is fenced or not activated"
    );
    Ok(match op {
        "stop" => {
            m["state"] = json!("stopped");
            m["vmm_running"] = json!(false);
            json!({"workspace":m})
        }
        "start" => {
            m["state"] = json!("running");
            m["vmm_running"] = json!(true);
            m.clone()
        }
        "resize" => {
            ensure!(m["state"] == "stopped", "stop the machine before resizing");
            for key in ["memory_mib", "vcpu_count"] {
                if !request[key].is_null() {
                    m[key] = request[key].clone();
                }
            }
            if let Some(gib) = request["disk_gib"].as_u64() {
                m["disk_bytes"] = json!(gib * common::GIB);
            }
            // Like the worker: the reply is the plain summary, without placement.
            let mut summary = m.clone();
            summary.as_object_mut().unwrap().remove("placement");
            summary
        }
        "snapshot" => {
            ensure!(m["state"] == "running", "workspace is not running");
            let name = request["name"].as_str().unwrap();
            let s = json!({"name":name,"workspace":id,"memory_mib":m["memory_mib"],"vcpu_count":m["vcpu_count"],"image":"alpine","state":"ready"});
            snapshots.insert(name.into(), s.clone());
            s
        }
        "delete" => {
            ensure!(
                m["vmm_running"] != true,
                "workspace is running; stop or hibernate it first"
            );
            m["state"] = json!("deleted");
            m["lifecycle"]["phase"] = json!("cleaned");
            let gone: Vec<String> = snapshots
                .iter()
                .filter(|(_, s)| s["workspace"] == id)
                .map(|(n, _)| n.clone())
                .collect();
            snapshots.retain(|_, s| s["workspace"] != id);
            json!({"deleted":id,"snapshots":gone})
        }
        "ssh-keys" => {
            m["keys"] = request["keys"].clone();
            json!({"enrolled":true})
        }
        "ssh-policy" => {
            json!({"enrolled":{"identity":"guest","generation":"g1","host_key":host_key(),"keys":m["keys"],"ready":true}})
        }
        _ => anyhow::bail!("unexpected {op}"),
    })
}
fn host_key() -> String {
    use base64::Engine;
    let mut bytes = Vec::from(&11u32.to_be_bytes()[..]);
    bytes.extend(b"ssh-ed25519");
    bytes.extend(32u32.to_be_bytes());
    bytes.extend([7; 32]);
    format!(
        "ssh-ed25519 {}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// Runs a move (of a stopped or running box) or a cross-runner fork to completion.
fn completed(kind: Kind, stopped: bool) -> (Fixture, RefCell<Peers>) {
    let (f, peers) = moving_fixture(kind);
    if stopped {
        let mut p = peers.borrow_mut();
        let source = p.source.as_mut().unwrap();
        source["state"] = json!("stopped");
        source["vmm_running"] = json!(false);
    }
    for _ in 0..30 {
        let more = tick(&f.shared, f.transfer.id, now(), &|node, r| {
            if stopped && r["op"] == "activate" {
                let mut p = peers.borrow_mut();
                let seen = p.target.as_mut().unwrap();
                seen["state"] = json!("stopped");
                seen["vmm_running"] = json!(false);
                seen["lifecycle"]["phase"] = json!("active");
                return Ok(seen.clone());
            }
            moving_call(&f, &peers, node, r)
        })
        .unwrap();
        if !more {
            break;
        }
    }
    assert_eq!(f.step(), Step::Succeeded);
    let c = f.shared.lock().unwrap();
    // These runners run a worker that keeps deletion tombstones.
    c.db.execute("UPDATE nodes SET heartbeat=?1,capabilities=json_set(capabilities,'$.managed_lifecycle_v1',json('true'),'$.snapshot_delete_v1',json('true'),'$.disk_resize_v1',json('true'),'$.managed_restore_v1',json('true'))",[now()]).unwrap();
    drop(c);
    (f, peers)
}
fn runners(f: &Fixture, peers: &RefCell<Peers>) -> (Runner, Runner) {
    let p = peers.borrow();
    let mut source = p.source.clone().unwrap();
    source["memory_mib"] = json!(256);
    source["vcpu_count"] = json!(1);
    let mut target = p.target.clone().unwrap();
    for key in ["memory_mib", "vcpu_count", "disk_bytes"] {
        if target[key].is_null() {
            target[key] = source[key].clone();
        }
    }
    (
        Runner::serve(&f.root, "source", vec![source]),
        Runner::serve(&f.root, "target", vec![target]),
    )
}
fn operate(f: &Fixture, request: Value) -> Result<Value> {
    f.shared.lock().unwrap().operation(f.user, request)
}
fn placement_of(f: &Fixture, physical: &str) -> Value {
    json!(
        f.shared
            .lock()
            .unwrap()
            .managed_placement(physical)
            .unwrap()
            .unwrap()
    )
}
fn rows(f: &Fixture, sql: &str) -> i64 {
    f.shared
        .lock()
        .unwrap()
        .db
        .query_row(sql, [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_moved_box_resizes_cpu_ram_and_disk_under_its_new_placement() {
    let (f, peers) = completed(Kind::Move, true);
    let (_source, target) = runners(&f, &peers);
    let placement = placement_of(&f, "physical");
    assert_eq!(placement["node"], "target");
    let resized = operate(
        &f,
        json!({"op":"resize","id":"box","memory_mib":512,"vcpu_count":2,"disk_gib":8,"operation_key":"grow"}),
    )
    .unwrap();
    assert_eq!(resized["memory_mib"], 512);
    let sent = target.last("resize");
    assert_eq!(sent["placement"], placement);
    // The reply carries no placement: the catalog saves its own epoch-checked observation.
    assert_eq!(target.last("inspect")["placement"], placement);
    let (meta, reserved): (String, i64) = f
        .shared
        .lock()
        .unwrap()
        .db
        .query_row(
            "SELECT metadata,reserved_mib FROM resources WHERE physical='physical'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let meta: Value = serde_json::from_str(&meta).unwrap();
    assert_eq!(
        (
            meta["memory_mib"].clone(),
            meta["vcpu_count"].clone(),
            meta["disk_bytes"].clone()
        ),
        (json!(512), json!(2), json!(8 * common::GIB))
    );
    assert_eq!(reserved, 0);
    // A retry with the same key answers the recorded result without another effect.
    let before = target.ops().len();
    operate(
        &f,
        json!({"op":"resize","id":"box","memory_mib":512,"vcpu_count":2,"disk_gib":8,"operation_key":"grow"}),
    )
    .unwrap();
    assert!(target.ops()[before..].iter().all(|r| r["op"] != "resize"));
    // A moved box is still moved by its next transfer at the new shape.
    let c = f.shared.lock().unwrap();
    let next = parking::shape(&c, "physical", Kind::Move, "box").unwrap();
    assert_eq!(
        (next.memory_mib, next.cpus, next.disk_bytes),
        (512, 2, 8 * common::GIB)
    );
}

#[test]
fn a_running_moved_box_refuses_resize_and_a_transfer_lock_refuses_every_mutation() {
    let (f, peers) = completed(Kind::Move, false);
    let (_source, target) = runners(&f, &peers);
    let error = operate(&f, json!({"op":"resize","id":"box","memory_mib":512})).unwrap_err();
    assert!(format!("{error:#}").contains("stopped"), "{error:#}");
    assert!(target.ops().iter().all(|r| r["op"] != "resize"));
    // Each of these succeeds on this stopped box with its snapshot, until a transfer holds it.
    operate(&f, json!({"op":"snapshot","id":"box","name":"held"})).unwrap();
    operate(&f, json!({"op":"stop","id":"box"})).unwrap();
    let c = f.shared.lock().unwrap();
    c.db.execute(
        "INSERT INTO transfer_locks(machine,transfer) VALUES('physical',?1)",
        [f.transfer.id],
    )
    .unwrap();
    drop(c);
    let before = target.ops().len();
    for request in [
        json!({"op":"delete","id":"box"}),
        json!({"op":"resize","id":"box","memory_mib":512}),
        json!({"op":"snapshot","id":"box","name":"locked"}),
        json!({"op":"snapshot-delete","id":"box","name":"held"}),
        json!({"op":"ssh-keys","id":"box","keys":[]}),
    ] {
        let error = operate(&f, request.clone()).unwrap_err();
        assert!(
            format!("{error:#}").contains("transfer-aware routing"),
            "{request}: {error:#}"
        );
    }
    // Refused before any runner contact, inventory refresh included.
    assert_eq!(target.ops().len(), before);
}

#[test]
fn a_moved_box_snapshot_is_published_and_forks_from_the_store_on_any_runner() {
    let (f, peers) = completed(Kind::Move, false);
    let (source, target) = runners(&f, &peers);
    let placement = placement_of(&f, "physical");
    operate(&f, json!({"op":"snapshot","id":"box","name":"saved"})).unwrap();
    assert_eq!(target.last("snapshot")["placement"], placement);
    let (node, meta): (String, String) = f
        .shared
        .lock()
        .unwrap()
        .db
        .query_row(
            "SELECT node,metadata FROM resources WHERE kind='snapshot' AND name='saved'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(node, "target");
    assert_eq!(
        serde_json::from_str::<Value>(&meta).unwrap()["disk_bytes"],
        4
    );
    // The recovery loop publishes it once, from the runner that holds it.
    let pushes = RefCell::new(Vec::new());
    let call = |node: &str, r: Value| -> Result<Value> {
        match r["op"].as_str().unwrap() {
            "capture-published" => Ok(json!({"present":false})),
            "capture-push" => {
                pushes
                    .borrow_mut()
                    .push((node.to_owned(), r["artifact"].clone()));
                Ok(publication(&r, true))
            }
            _ => moving_call(&f, &peers, node, r),
        }
    };
    for _ in 0..2 {
        for (item, result) in publication::pass(&f.shared, &call) {
            result.unwrap_or_else(|e| panic!("{item}: {e:#}"));
        }
    }
    let pushed = pushes.borrow().clone();
    assert_eq!(pushed.len(), 1);
    assert_eq!(pushed[0].0, "target");
    assert_eq!(pushed[0].1["machine"], "physical");
    let artifact: String = f
        .shared
        .lock()
        .unwrap()
        .db
        .query_row(
            "SELECT id FROM artifacts WHERE kind='snapshot' AND state='stored' AND local_name=(SELECT physical FROM resources WHERE kind='snapshot' AND name='saved')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let listed = f.shared.lock().unwrap().state(f.user).unwrap();
    assert_eq!(listed["snapshots"][0]["artifact"], json!(artifact));
    // A fork from it on the old runner uses the stored artifact: no new capture or upload.
    let c = f.shared.lock().unwrap();
    let mut input = parking::shape(&c, "physical", Kind::Fork, "box").unwrap();
    input.target = Some("source".into());
    input.child = Some(Child {
        id: "from-store".into(),
        name: "branch".into(),
        owner: None,
    });
    movement::named_snapshot(&c, f.user, "box", "physical", "target", "saved", &mut input).unwrap();
    assert_eq!(input.capture.as_deref(), Some(artifact.as_str()));
    let fork = c.begin_transfer(f.user, "fork-from-store", &input).unwrap();
    drop(c);
    let imported = RefCell::new(Value::Null);
    for _ in 0..30 {
        let more = tick(&f.shared, fork.id, now(), &|node, r| {
            assert!(
                !matches!(r["op"].as_str(), Some("snapshot" | "capture-push")),
                "stored fork took a new capture: {r}"
            );
            if r["op"] == "fork-prepare" {
                assert_eq!(node, "source");
                *imported.borrow_mut() = r.clone();
            }
            moving_call(&f, &peers, node, r)
        })
        .unwrap();
        if !more {
            break;
        }
    }
    let c = f.shared.lock().unwrap();
    assert_eq!(c.transfer(fork.id).unwrap().step, Step::Succeeded);
    let snapshot: String =
        c.db.query_row(
            "SELECT physical FROM resources WHERE kind='snapshot' AND name='saved'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(imported.borrow()["snapshot"], json!(snapshot));
    assert_eq!(imported.borrow()["fork_source"], "physical");
    drop(c);
    drop(source);
}

#[test]
fn a_snapshot_left_on_the_old_runner_is_rehomed_to_the_store_and_its_copy_removed() {
    let (f, peers) = completed(Kind::Move, false);
    // Taken before the move: its row and files are still on the old runner.
    let c = f.shared.lock().unwrap();
    c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node) VALUES(?1,'snapshot','before','s-before',?2,'source')",params![f.user,json!({"name":"s-before","workspace":"physical","memory_mib":256,"vcpu_count":1,"disk_bytes":4,"state":"ready"}).to_string()]).unwrap();
    drop(c);
    let (source, _target) = runners(&f, &peers);
    let call = |node: &str, r: Value| -> Result<Value> {
        match r["op"].as_str().unwrap() {
            "capture-published" => Ok(json!({"present":false})),
            "capture-push" => {
                assert_eq!(node, "source");
                Ok(publication(&r, true))
            }
            _ => rpc(&f.root, node, r),
        }
    };
    for _ in 0..3 {
        for (item, result) in publication::pass(&f.shared, &call) {
            result.unwrap_or_else(|e| panic!("{item}: {e:#}"));
        }
    }
    assert_eq!(
        rows(
            &f,
            "SELECT COUNT(*) FROM resources WHERE kind='snapshot' AND name='before' AND node=''"
        ),
        1
    );
    // The old runner removed its copy under its own fenced placement.
    let removed = source.last("snapshot-delete");
    assert_eq!(removed["name"], "s-before");
    assert_eq!(
        removed["placement"],
        peers.borrow().source.as_ref().unwrap()["placement"]
    );
    assert_eq!(rows(&f, "SELECT COUNT(*) FROM drain_publications"), 0);
    // Still rooted by its row and listed as stored; deleting it needs no runner.
    assert_eq!(
        rows(
            &f,
            "SELECT COUNT(*) FROM artifacts a JOIN resources r ON r.kind='snapshot' AND a.local_name=r.physical AND a.owner=r.owner WHERE a.state='stored'"
        ),
        1
    );
    let listed = f.shared.lock().unwrap().state(f.user).unwrap();
    assert_eq!(listed["snapshots"][0]["node_online"], true);
    let before = source.ops().len();
    operate(
        &f,
        json!({"op":"snapshot-delete","id":"box","name":"before","operation_key":"forget-before"}),
    )
    .unwrap();
    assert_eq!(
        rows(&f, "SELECT COUNT(*) FROM resources WHERE kind='snapshot'"),
        0
    );
    assert!(
        source.ops()[before..]
            .iter()
            .all(|r| r["op"] != "snapshot-delete")
    );
    // A lost reply is answered from the journal.
    assert_eq!(
        operate(
            &f,
            json!({"op":"snapshot-delete","id":"box","name":"before","operation_key":"forget-before"})
        )
        .unwrap()["deleted"],
        "before"
    );
}

#[test]
fn a_moved_box_deletes_on_its_runner_and_releases_rows_reservations_and_roots() {
    let (f, peers) = completed(Kind::Move, true);
    let (source, target) = runners(&f, &peers);
    let placement = placement_of(&f, "physical");
    let c = f.shared.lock().unwrap();
    c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node) VALUES(?1,'snapshot','old','s-old',?2,'source')",params![f.user,json!({"name":"s-old","workspace":"physical","memory_mib":256,"vcpu_count":1,"disk_bytes":4,"state":"ready"}).to_string()]).unwrap();
    c.db.execute("INSERT INTO artifacts(id,kind,owner,machine,local_name,state,attempt,digest,store_url) VALUES('published-old','snapshot',?1,'physical','s-old','stored',?2,?3,'s3://bucket/root')",params![f.user,"c".repeat(32),"d".repeat(64)]).unwrap();
    c.db.execute("INSERT INTO drain_publications(node,name,artifact,manifest) VALUES('source','s-old','published-old','{}')",[]).unwrap();
    drop(c);
    let deleted = operate(
        &f,
        json!({"op":"delete","id":"box","operation_key":"delete-moved"}),
    )
    .unwrap();
    assert_eq!(deleted["deleted"], "box");
    assert_eq!(target.last("delete")["placement"], placement);
    // The old runner's fenced copy goes too, under that copy's own placement.
    assert_eq!(
        source.last("delete")["placement"],
        peers.borrow().source.as_ref().unwrap()["placement"]
    );
    assert_eq!(rows(&f, "SELECT COUNT(*) FROM resources"), 0);
    assert_eq!(
        rows(
            &f,
            "SELECT COUNT(*) FROM operations WHERE retry_key='delete-moved' AND state='succeeded'"
        ),
        1
    );
    // The retry answers from the journal; the box is gone.
    assert_eq!(
        operate(
            &f,
            json!({"op":"delete","id":"box","operation_key":"delete-moved"})
        )
        .unwrap()["deleted"],
        "box"
    );
    // The old runner's snapshot copy goes too; then no artifact of the box is a store root.
    let call = |node: &str, r: Value| rpc(&f.root, node, r);
    for (item, result) in publication::pass(&f.shared, &call) {
        result.unwrap_or_else(|e| panic!("{item}: {e:#}"));
    }
    assert_eq!(source.last("snapshot-delete")["name"], "s-old");
    assert_eq!(rows(&f, "SELECT COUNT(*) FROM drain_publications"), 0);
    assert_eq!(
        rows(
            &f,
            "SELECT COUNT(*) FROM resources WHERE current_capture IS NOT NULL"
        ),
        0
    );
    assert_eq!(rows(&f, "SELECT COUNT(*) FROM transfer_reservations"), 0);
}

#[test]
fn delete_and_snapshot_delete_wait_for_a_runner_that_keeps_tombstones() {
    let (f, peers) = completed(Kind::Move, true);
    let (_source, target) = runners(&f, &peers);
    f.shared
        .lock()
        .unwrap()
        .db
        .execute(
            "UPDATE nodes SET capabilities=json_remove(capabilities,'$.managed_lifecycle_v1')",
            [],
        )
        .unwrap();
    let error = operate(&f, json!({"op":"delete","id":"box"})).unwrap_err();
    assert!(
        format!("{error:#}").contains("upgrade its worker"),
        "{error:#}"
    );
    assert!(target.ops().iter().all(|r| r["op"] != "delete"));
    assert_eq!(
        rows(&f, "SELECT COUNT(*) FROM resources WHERE kind='machine'"),
        1
    );
}

#[test]
fn a_moved_box_changes_public_keys_and_advances_the_policy_generation() {
    let (f, peers) = completed(Kind::Move, false);
    let (_source, target) = runners(&f, &peers);
    let placement = placement_of(&f, "physical");
    let generation = rows(
        &f,
        "SELECT ssh_policy_generation FROM resources WHERE physical='physical'",
    );
    let key = host_key();
    operate(
        &f,
        json!({"op":"ssh-keys","id":"box","keys":[key],"upgrade":false}),
    )
    .unwrap();
    assert_eq!(target.last("ssh-keys")["placement"], placement);
    assert_eq!(target.last("ssh-policy")["placement"], placement);
    assert_eq!(
        rows(
            &f,
            "SELECT ssh_policy_generation FROM resources WHERE physical='physical'"
        ),
        generation + 1
    );
    let policy: String = f
        .shared
        .lock()
        .unwrap()
        .db
        .query_row(
            "SELECT ssh_policy FROM resources WHERE physical='physical'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&policy).unwrap()["enrolled"]["keys"],
        json!([key])
    );
    // Revoke: an empty key list, under the same authority.
    operate(
        &f,
        json!({"op":"ssh-keys","id":"box","keys":[],"upgrade":false}),
    )
    .unwrap();
    assert_eq!(target.last("ssh-keys")["keys"], json!([]));
    assert_eq!(
        rows(
            &f,
            "SELECT ssh_policy_generation FROM resources WHERE physical='physical'"
        ),
        generation + 2
    );
}

#[test]
fn a_cross_runner_fork_child_resizes_snapshots_rekeys_and_deletes_on_its_runner() {
    let (f, peers) = completed(Kind::Fork, false);
    let (source, target) = runners(&f, &peers);
    let child = placement_of(&f, "child-id");
    assert_eq!(child["node"], "target");
    operate(
        &f,
        json!({"op":"ssh-keys","id":"child","keys":[host_key()]}),
    )
    .unwrap();
    assert_eq!(target.last("ssh-keys")["placement"], child);
    operate(
        &f,
        json!({"op":"snapshot","id":"child","name":"child-saved"}),
    )
    .unwrap();
    assert_eq!(target.last("snapshot")["placement"], child);
    operate(&f, json!({"op":"stop","id":"child"})).unwrap();
    operate(&f, json!({"op":"resize","id":"child","memory_mib":512})).unwrap();
    assert_eq!(target.last("resize")["placement"], child);
    operate(
        &f,
        json!({"op":"snapshot-delete","id":"child","name":"child-saved"}),
    )
    .unwrap();
    assert_eq!(target.last("snapshot-delete")["placement"], child);
    operate(&f, json!({"op":"delete","id":"child"})).unwrap();
    assert_eq!(target.last("delete")["placement"], child);
    // The parent keeps running on its own runner, untouched.
    assert!(
        source
            .ops()
            .iter()
            .all(|r| !matches!(r["op"].as_str(), Some("delete" | "resize")))
    );
    assert_eq!(
        rows(&f, "SELECT COUNT(*) FROM resources WHERE kind='machine'"),
        1
    );
    assert!(placement_of(&f, "physical")["node"] == "source");
}

/// A box parked from runner "source": no runner runs it, that runner keeps its fenced export.
fn parked() -> (Fixture, RefCell<Peers>) {
    let (f, peers) = parked_fixture("stopped");
    let c = f.shared.lock().unwrap();
    let t = c.transfer(f.transfer.id).unwrap();
    c.advance_transfer(t.id, t.epoch, t.step, Step::Aborted, &Evidence::default())
        .unwrap();
    c.db.execute("UPDATE resources SET node='',reserved_mib=0,reserved_cpus=0,current_capture='capture' WHERE physical='physical'",[]).unwrap();
    c.db.execute("UPDATE nodes SET heartbeat=?1,capabilities=json_set(capabilities,'$.managed_lifecycle_v1',json('true'))",[now()]).unwrap();
    drop(c);
    peers.borrow_mut().source.as_mut().unwrap()["lifecycle"] =
        json!({"phase":"exported","intent":{"transfer":"transfer-park","capture":"parked"}});
    (f, peers)
}

#[test]
fn a_parked_box_deletes_its_parking_runners_fenced_copy_and_releases_its_capture() {
    let (f, peers) = parked();
    let copy = peers.borrow().source.clone().unwrap();
    let source = Runner::serve(&f.root, "source", vec![copy.clone()]);
    let deleted = operate(
        &f,
        json!({"op":"delete","id":"box","operation_key":"parked"}),
    )
    .unwrap();
    assert_eq!(deleted["deleted"], "box");
    // Its uncleaned export is cleaned first, then deleted, both under its own placement.
    let ops: Vec<String> = source
        .ops()
        .iter()
        .filter(|r| r["placement"] == copy["placement"])
        .map(|r| r["op"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ops, ["export-cleanup", "delete"]);
    assert_eq!(rows(&f, "SELECT COUNT(*) FROM resources"), 0);
    // Its stored capture is no longer a store root; the history row stays for collection.
    assert_eq!(
        rows(&f, "SELECT COUNT(*) FROM artifacts WHERE id='capture'"),
        1
    );
    assert_eq!(
        rows(
            &f,
            "SELECT COUNT(*) FROM resources WHERE current_capture='capture'"
        ) + rows(
            &f,
            "SELECT COUNT(*) FROM transfers WHERE capture='capture' AND step NOT IN ('succeeded','aborted','failed')"
        ),
        0
    );
}

#[test]
fn a_parked_box_deletes_its_stored_snapshot_from_the_catalog_alone() {
    let (f, _peers) = parked();
    let c = f.shared.lock().unwrap();
    c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node) VALUES(?1,'snapshot','kept','s-kept',?2,'')",params![f.user,json!({"name":"s-kept","workspace":"physical","memory_mib":256,"vcpu_count":1,"disk_bytes":4,"state":"ready"}).to_string()]).unwrap();
    c.db.execute("INSERT INTO artifacts(id,kind,owner,machine,local_name,state,attempt,digest,store_url) VALUES('kept-artifact','snapshot',?1,'physical','s-kept','stored',?2,?3,'s3://bucket/root')",params![f.user,"c".repeat(32),"d".repeat(64)]).unwrap();
    drop(c);
    // No runner socket exists: any runner contact would fail this delete.
    operate(&f, json!({"op":"snapshot-delete","id":"box","name":"kept"})).unwrap();
    assert_eq!(
        rows(&f, "SELECT COUNT(*) FROM resources WHERE kind='snapshot'"),
        0
    );
}

#[test]
fn an_unstored_snapshot_on_the_old_runner_is_removed_there_by_snapshot_delete_and_delete() {
    let (f, peers) = completed(Kind::Move, true);
    let c = f.shared.lock().unwrap();
    // Taken before the box was managed: no disk size, so it can never be published.
    for (name, physical) in [("first", "s-first"), ("second", "s-second")] {
        c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node) VALUES(?1,'snapshot',?2,?3,?4,'source')",params![f.user,name,physical,json!({"name":physical,"workspace":"physical","memory_mib":256,"vcpu_count":1,"state":"ready"}).to_string()]).unwrap();
    }
    drop(c);
    let old = peers.borrow().source.as_ref().unwrap()["placement"].clone();
    let (source, target) = runners(&f, &peers);
    // The old runner is offline: nothing is forgotten while its copy may remain.
    let offline = |down: bool| {
        f.shared
            .lock()
            .unwrap()
            .db
            .execute(
                "UPDATE nodes SET heartbeat=?1 WHERE id='source'",
                [if down { 0 } else { now() }],
            )
            .unwrap();
    };
    offline(true);
    let error = operate(
        &f,
        json!({"op":"snapshot-delete","id":"box","name":"first"}),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("offline"), "{error:#}");
    assert_eq!(
        rows(&f, "SELECT COUNT(*) FROM resources WHERE kind='snapshot'"),
        2
    );
    offline(false);
    operate(
        &f,
        json!({"op":"snapshot-delete","id":"box","name":"first"}),
    )
    .unwrap();
    let removed = source.last("snapshot-delete");
    assert_eq!(
        (removed["name"].clone(), removed["placement"].clone()),
        (json!("s-first"), old.clone())
    );
    assert!(target.ops().iter().all(|r| r["op"] != "snapshot-delete"));
    // Deleting the box removes its old fenced copy too, which takes that runner's snapshots.
    let placement = placement_of(&f, "physical");
    offline(true);
    let error = operate(&f, json!({"op":"delete","id":"box","operation_key":"gone"})).unwrap_err();
    assert!(format!("{error:#}").contains("offline"), "{error:#}");
    // Its own runner holds the tombstone already; the catalog keeps the box until the old copy goes.
    assert_eq!(target.last("delete")["placement"], placement);
    assert_eq!(
        rows(&f, "SELECT COUNT(*) FROM resources WHERE kind='machine'"),
        1
    );
    offline(false);
    operate(&f, json!({"op":"delete","id":"box","operation_key":"gone"})).unwrap();
    assert_eq!(source.last("delete")["placement"], old);
    assert_eq!(rows(&f, "SELECT COUNT(*) FROM resources"), 0);
}

#[test]
fn a_runner_copy_with_no_box_record_keeps_its_publication_while_listed() {
    let (f, _peers) = completed(Kind::Move, false);
    let c = f.shared.lock().unwrap();
    c.db.execute("INSERT INTO artifacts(id,kind,owner,machine,local_name,state,attempt,digest,store_url) VALUES('pulled','snapshot',?1,'physical','s-pulled','stored',?2,?3,'s3://bucket/root')",params![f.user,"c".repeat(32),"d".repeat(64)]).unwrap();
    c.db.execute("INSERT INTO drain_publications(node,name,artifact,manifest) VALUES('target','s-pulled','pulled','{}')",[]).unwrap();
    drop(c);
    let listed = std::cell::Cell::new(true);
    let call = |_: &str, r: Value| -> Result<Value> {
        match r["op"].as_str().unwrap() {
            "inspect" => anyhow::bail!("workspace not found"),
            "snapshots" => Ok(if listed.get() {
                json!([{"name":"s-pulled"}])
            } else {
                json!([])
            }),
            other => panic!("unexpected {other}"),
        }
    };
    for (item, result) in publication::pass(&f.shared, &call) {
        result.unwrap_or_else(|e| panic!("{item}: {e:#}"));
    }
    assert_eq!(rows(&f, "SELECT COUNT(*) FROM drain_publications"), 1);
    listed.set(false);
    for (item, result) in publication::pass(&f.shared, &call) {
        result.unwrap_or_else(|e| panic!("{item}: {e:#}"));
    }
    assert_eq!(rows(&f, "SELECT COUNT(*) FROM drain_publications"), 0);
}

#[test]
fn a_snapshot_a_fork_already_uploaded_is_still_published_as_a_snapshot() {
    let (f, peers) = completed(Kind::Fork, false);
    // The fork's own capture was published as its transfer capture; name it as a snapshot.
    let capture: String = f
        .shared
        .lock()
        .unwrap()
        .db
        .query_row(
            "SELECT a.local_name FROM artifacts a JOIN transfers t ON t.capture=a.id WHERE t.id=?1",
            [f.transfer.id],
            |r| r.get(0),
        )
        .unwrap();
    f.shared.lock().unwrap().db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node) VALUES(?1,'snapshot','forked-from',?2,?3,'source')",params![f.user,capture,json!({"name":capture,"workspace":"physical","memory_mib":256,"vcpu_count":1,"disk_bytes":4,"state":"ready"}).to_string()]).unwrap();
    let call = |node: &str, r: Value| -> Result<Value> {
        match r["op"].as_str().unwrap() {
            "capture-published" => Ok(json!({"present":false})),
            "capture-push" => Ok(publication(&r, true)),
            _ => moving_call(&f, &peers, node, r),
        }
    };
    for (item, result) in publication::pass(&f.shared, &call) {
        result.unwrap_or_else(|e| panic!("{item}: {e:#}"));
    }
    assert_eq!(
        rows(
            &f,
            &format!(
                "SELECT COUNT(*) FROM artifacts WHERE kind='snapshot' AND state='stored' AND local_name='{capture}'"
            )
        ),
        1
    );
}

#[test]
fn a_snapshot_recorded_without_a_disk_size_follows_its_moved_box_through_the_store() {
    let (f, peers) = completed(Kind::Move, false);
    // Recorded before captures carried a disk size; the upgraded runner reports it.
    f.shared.lock().unwrap().db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node) VALUES(?1,'snapshot','before-upgrade','s-old-upgrade',?2,'source')",params![f.user,json!({"name":"s-old-upgrade","workspace":"physical","memory_mib":256,"vcpu_count":1,"state":"ready"}).to_string()]).unwrap();
    let mut copy = peers.borrow().source.clone().unwrap();
    copy["memory_mib"] = json!(256);
    copy["vcpu_count"] = json!(1);
    let source = Runner::serve_with(
        &f.root,
        "source",
        vec![copy],
        vec![
            json!({"name":"s-old-upgrade","workspace":"physical","memory_mib":256,"vcpu_count":1,"state":"ready","disk_bytes":4}),
        ],
    );
    let pushes = RefCell::new(Vec::new());
    let call = |node: &str, r: Value| -> Result<Value> {
        match r["op"].as_str().unwrap() {
            "capture-published" => Ok(json!({"present":false})),
            "capture-push" => {
                pushes
                    .borrow_mut()
                    .push((node.to_owned(), r["artifact"]["disk_bytes"].clone()));
                Ok(publication(&r, true))
            }
            _ => rpc(&f.root, node, r),
        }
    };
    for _ in 0..3 {
        for (item, result) in publication::pass(&f.shared, &call) {
            result.unwrap_or_else(|e| panic!("{item}: {e:#}"));
        }
    }
    assert_eq!(*pushes.borrow(), [("source".to_owned(), json!(4))]);
    // Rehomed to the store, its old runner copy removed, and it forks on any runner.
    assert_eq!(
        rows(
            &f,
            "SELECT COUNT(*) FROM resources WHERE kind='snapshot' AND node='' AND json_extract(metadata,'$.disk_bytes')=4"
        ),
        1
    );
    assert_eq!(source.last("snapshot-delete")["name"], "s-old-upgrade");
    let c = f.shared.lock().unwrap();
    let mut input = parking::shape(&c, "physical", Kind::Fork, "box").unwrap();
    input.target = Some("source".into());
    movement::named_snapshot(
        &c,
        f.user,
        "box",
        "physical",
        "target",
        "before-upgrade",
        &mut input,
    )
    .unwrap();
    assert!(input.capture.is_some());
    assert_eq!(input.disk_bytes, 4);
}
