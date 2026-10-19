use super::*;
fn database(path: &Path) -> Connection {
    let db = Connection::open(path).unwrap();
    db.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    db.execute_batch("PRAGMA foreign_keys=ON;CREATE TABLE IF NOT EXISTS users(id INTEGER PRIMARY KEY);CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);INSERT OR IGNORE INTO settings VALUES('min_reader_schema','5');CREATE TABLE IF NOT EXISTS resources(current_capture TEXT);CREATE TABLE IF NOT EXISTS transfers(capture TEXT,source_capture TEXT,step TEXT);CREATE TABLE IF NOT EXISTS operations(node TEXT,op TEXT,state TEXT,request TEXT);").unwrap();
    crate::image_catalog::initialize(&db).unwrap();
    initialize(&db).unwrap();
    db.execute_batch("PRAGMA user_version=5;").unwrap();
    db
}
fn artifact(db: &Connection, id: &str, n: u64) {
    db.execute("INSERT INTO artifacts(id,kind,attempt,digest,local_name,state,store_url) VALUES(?1,'image',?2,?3,?4,'stored','s3://bucket/root')",params![id,format!("{n:032x}"),format!("{n:064x}"),format!("tools@{n}")]).unwrap();
    db.execute("INSERT INTO artifact_copies(artifact,node,state,exclusive_bytes,last_used) VALUES(?1,'runner','present',100,?2)",params![id,n as i64]).unwrap();
}
#[test]
fn cache_claim_refuses_unstored_leased_prefetched_placed_and_transfer_copies() {
    for case in 0..9 {
        let mut db = database(Path::new(":memory:"));
        artifact(&db, "a", 1);
        match case {
            0 => {
                db.execute("UPDATE artifacts SET state='uploading'", [])
                    .unwrap();
            }
            1 => {
                db.execute("INSERT INTO leases(artifact,holder,purpose,expires) VALUES('a','holder','pull',1000)",[]).unwrap();
            }
            2 => {
                db.execute("INSERT INTO images(name,version,artifact,base,base_digest,recipe_sha256,prefetch,status) VALUES('tools',1,'a','base','digest','recipe',1,'current')",[]).unwrap();
            }
            3 => {
                db.execute("INSERT INTO resources VALUES('a')", []).unwrap();
            }
            4 => {
                db.execute("INSERT INTO transfers VALUES('a',NULL,'importing')", [])
                    .unwrap();
            }
            5 => {
                db.execute("INSERT INTO transfers VALUES(NULL,'a','exported')", [])
                    .unwrap();
            }
            6 => {
                db.execute("UPDATE artifacts SET kind='capture'", [])
                    .unwrap();
            }
            7 => {
                db.execute("INSERT INTO leases(artifact,parent,holder,purpose,expires) VALUES('a','a','holder','fork',1000)",[]).unwrap();
            }
            _ => {
                db.execute("INSERT INTO operations VALUES('runner','create','pending','{\"request\":{\"image\":\"tools@1\"}}')",[]).unwrap();
            }
        }
        assert!(
            claim(&mut db, "runner", &format!("{:064x}", 1), 100).is_err(),
            "case {case}"
        );
        assert_eq!(
            db.query_row::<String, _, _>("SELECT state FROM artifact_copies", [], |r| r.get(0))
                .unwrap(),
            "present"
        );
    }
}
#[test]
fn cache_claim_excludes_new_leases_roots_and_uses_until_positive_completion() {
    let mut db = database(Path::new(":memory:"));
    artifact(&db, "a", 1);
    let c = claim(&mut db, "runner", &format!("{:064x}", 1), 100).unwrap();
    assert!(
        claim(&mut db, "runner", &c.reference.digest, 999999).is_err(),
        "claim must not expire"
    );
    // Prior schema-5 writers still open this additive schema; the database itself guards them.
    crate::nodes::readable_by(&db, 5).unwrap();
    for sql in [
        "INSERT INTO leases(artifact,holder,purpose,expires) VALUES('a','holder','pull',1000)",
        "INSERT INTO resources VALUES('a')",
        "INSERT INTO transfers VALUES('a',NULL,'preparing')",
        "INSERT INTO images(name,version,artifact,base,base_digest,recipe_sha256,prefetch,status) VALUES('tools',1,'a','base','digest','recipe',1,'current')",
        "INSERT INTO operations VALUES('runner','create','pending','{\"request\":{\"image\":\"tools@1\"}}')",
        "UPDATE artifacts SET state='deleting' WHERE id='a'",
        "UPDATE artifacts SET digest='changed' WHERE id='a'",
        "UPDATE artifact_copies SET state='present'",
        "DELETE FROM artifact_copies",
    ] {
        assert!(db.execute(sql, []).is_err(), "guard missing: {sql}");
    }
    assert!(
        complete(
            &mut db,
            &c,
            &json!({"digest":"wrong","freed_bytes":100,"available_bytes":200})
        )
        .is_err()
    );
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT count(*) FROM cache_evictions", [], |r| r.get(0))
            .unwrap(),
        1
    );
    complete(
        &mut db,
        &c,
        &json!({"digest":c.reference.digest,"freed_bytes":0,"available_bytes":200}),
    )
    .unwrap();
    db.execute(
        "INSERT INTO leases(artifact,holder,purpose,expires) VALUES('a','holder','pull',1000)",
        [],
    )
    .unwrap();
    assert!(
        complete(
            &mut db,
            &c,
            &json!({"digest":c.reference.digest,"freed_bytes":0,"available_bytes":200})
        )
        .is_err()
    );
}
#[test]
fn cache_claim_guards_updates_as_well_as_inserts() {
    let mut db = database(Path::new(":memory:"));
    artifact(&db, "a", 1);
    artifact(&db, "b", 2);
    db.execute(
        "INSERT INTO leases(artifact,holder,purpose,expires) VALUES('b','holder','pull',1000)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO resources VALUES(NULL)", [])
        .unwrap();
    db.execute("INSERT INTO transfers VALUES(NULL,NULL,'preparing')", [])
        .unwrap();
    db.execute(
        "INSERT INTO operations VALUES('runner','create','pending','{\"request\":{\"image\":\"tools@2\"}}')",
        [],
    )
    .unwrap();
    claim(&mut db, "runner", &format!("{:064x}", 1), 100).unwrap();
    for sql in [
        "UPDATE leases SET artifact='a'",
        "UPDATE leases SET parent='a'",
        "UPDATE resources SET current_capture='a'",
        "UPDATE transfers SET capture='a'",
        "UPDATE operations SET request='{\"request\":{\"image\":\"tools@1\"}}'",
    ] {
        assert!(db.execute(sql, []).is_err(), "guard missing: {sql}");
    }
}
#[test]
fn cache_lease_race_and_restart_preserve_the_claim() {
    let root = std::env::temp_dir().join(format!("ow-cache-race-{}", store::token().unwrap()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("catalog.sqlite3");
    let mut db = database(&path);
    artifact(&db, "a", 1);
    let c = claim(&mut db, "runner", &format!("{:064x}", 1), 100).unwrap();
    let result = std::thread::scope(|scope| {
        scope.spawn(||{
        let other=Connection::open(&path).unwrap();other.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
        other.execute("INSERT INTO leases(artifact,holder,purpose,expires) VALUES('a','holder','pull',1000)",[])
    }).join().unwrap()
    });
    assert!(result.is_err());
    drop(db);
    let mut db = database(&path);
    assert!(claim(&mut db, "runner", &c.reference.digest, 100000).is_err());
    complete(
        &mut db,
        &c,
        &json!({"digest":c.reference.digest,"freed_bytes":1,"available_bytes":2}),
    )
    .unwrap();
    assert_eq!(
        db.query_row::<String, _, _>("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap(),
        "ok"
    );
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn cache_lru_stops_on_actual_space_and_fails_without_enough_reclaim() {
    use std::{
        io::{BufRead, Write},
        os::unix::{fs::PermissionsExt, net::UnixListener},
    };
    let root = std::env::temp_dir().join(format!("ow-cache-lru-{}", store::token().unwrap()));
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = root.join("catalog.sqlite3");
    drop(database(&path));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let db = database(&path);
    artifact(&db, "old", 1);
    artifact(&db, "new", 2);
    db.execute("UPDATE artifact_copies SET node='local'", [])
        .unwrap();
    drop(db);
    let socket = UnixListener::bind(root.join("control.sock")).unwrap();
    let mut order = vec![];
    std::thread::scope(|scope| {
        let worker=scope.spawn(||{
            for _ in 0..3 {
                let (mut stream,_)=socket.accept().unwrap();let mut line=String::new();std::io::BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();let r:Value=serde_json::from_str(&line).unwrap();
                let response=if r["op"] == "cache-inventory" {
                    // Reverse arrival order; the controller must choose oldest first.
                    json!({"entries":[{"name":"tools@2","digest":format!("{:064x}",2),"logical_bytes":100,"exclusive_bytes":100,"last_used":20,"pins":[]},{"name":"tools@1","digest":format!("{:064x}",1),"logical_bytes":100,"exclusive_bytes":0,"last_used":10,"pins":[]}],"protected_bytes":50,"staging_bytes":0,"available_bytes":100})
                } else {
                    order.push(r["name"].as_str().unwrap().to_owned());
                    // Both removals share extents; their optimistic byte counts free nothing.
                    json!({"digest":r["reference"]["digest"],"freed_bytes":0,"available_bytes":100})
                };
                writeln!(stream,"{}",json!({"ok":true,"result":response})).unwrap();
            }
        });
        assert!(
            make_room(&root, &root, "local", 100, "incoming")
                .unwrap_err()
                .to_string()
                .contains("cannot free enough")
        );
        worker.join().unwrap();
    });
    assert_eq!(order, ["tools@1", "tools@2"]);
    assert_eq!(
        Connection::open(&path)
            .unwrap()
            .query_row::<i64, _, _>("SELECT count(*) FROM artifact_copies", [], |r| r.get(0))
            .unwrap(),
        0
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn interrupted_verification_reconciles_claim_and_unblocks_other_runner_pull() {
    use std::{
        collections::BTreeMap,
        fs,
        io::{BufRead, Write},
        os::unix::{fs::PermissionsExt, net::UnixListener},
    };
    let root = std::env::temp_dir().join(format!("ow-cache-stamp-{}", store::token().unwrap()));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let path = root.join("catalog.sqlite3");
    let mut db = database(&path);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let runner = root.join("runner");
    let image = runner.join("images/tools@1");
    fs::create_dir_all(&image).unwrap();
    let disk = image.join("disk.ext4");
    fs::write(&disk, b"sealed disk").unwrap();
    let objects = store::DirectoryObjects(root.join("objects"));
    let reference = store::push(
        &objects,
        &format!("{:032x}", 1),
        "image",
        json!({"name":"tools","version":1}),
        &BTreeMap::from([(
            "disk.ext4".into(),
            store::Source {
                path: disk.clone(),
                sha256: store::hash_file(&disk).unwrap(),
            },
        )]),
        1,
    )
    .unwrap();
    fs::write(image.join("manifest.json"),json!({"name":"tools@1","image_digest":reference.digest,"disk_sha256":store::hash_file(&disk).unwrap(),"store":{"reference":reference}}).to_string()).unwrap();
    // Exactly the path atomic_json leaves if interrupted before renaming its verification stamp.
    let temporary = image
        .join(".verified.json")
        .with_extension(format!("tmp-{}", crate::common::nonce().unwrap()));
    fs::write(&temporary, b"{partial stamp").unwrap();
    let inventory = ow_core::cache::inventory(&runner).unwrap();
    assert_eq!(inventory[0].digest, reference.digest);
    assert!(image.join(".verified.json").exists());
    assert!(temporary.exists());
    artifact(&db, "a", 1);
    db.execute("UPDATE artifacts SET digest=?1", [&reference.digest])
        .unwrap();
    db.execute("UPDATE artifact_copies SET node='local'", [])
        .unwrap();
    let c = claim(&mut db, "local", &reference.digest, 100).unwrap();
    // A prior interrupted attempt retains the token and excludes new leases across runners.
    fs::write(image.join(".pin"), b"pin").unwrap();
    {
        let _lock = ow_core::cache::lock(&runner, true).unwrap();
        assert!(
            ow_core::cache::evict_claim(&runner, &c.token, &c.name, &c.reference, &objects)
                .unwrap_err()
                .to_string()
                .contains("local pin")
        );
    }
    let lease = "INSERT INTO leases(artifact,holder,purpose,expires) VALUES('a','other-runner','pull',1000)";
    assert!(
        db.execute(lease, [])
            .unwrap_err()
            .to_string()
            .contains("artifact copy eviction in progress")
    );
    fs::remove_file(image.join(".pin")).unwrap();
    let socket = UnixListener::bind(runner.join("control.sock")).unwrap();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let (mut stream, _) = socket.accept().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["claim"], c.token);
            let _lock = ow_core::cache::lock(&runner, true).unwrap();
            let result = ow_core::cache::evict_claim(
                &runner,
                request["claim"].as_str().unwrap(),
                request["name"].as_str().unwrap(),
                &serde_json::from_value(request["reference"].clone()).unwrap(),
                &objects,
            );
            writeln!(stream, "{}", crate::wire::response(result)).unwrap();
        });
        let result = reconcile(&root, &runner, "local");
        worker.join().unwrap();
        result.unwrap();
    });
    assert!(!image.exists());
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT COUNT(*) FROM cache_evictions", [], |r| r.get(0))
            .unwrap(),
        0
    );
    db.execute(lease, []).unwrap();
    fs::remove_dir_all(root).unwrap();
}
