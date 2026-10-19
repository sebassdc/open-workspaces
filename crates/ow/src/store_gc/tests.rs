use super::*;
use clap::Parser;
use ow_core::store::{Objects, gc::ObjectVersion};
use std::{
    cell::{Cell, RefCell},
    io::Read,
    os::unix::fs::PermissionsExt,
};

const URL: &str = "s3://bucket/root";
struct Fixture {
    root: std::path::PathBuf,
    c: crate::catalog::Catalog,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("ow-gc-{}", store::token().unwrap()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let c = crate::catalog::Catalog::open(&root, "fixture", None).unwrap();
        c.db.execute(
            "INSERT INTO users(id,issuer,email) VALUES(1,'fixture','owner@example.test')",
            [],
        )
        .unwrap();
        Self { root, c }
    }
    fn reopen(&mut self) {
        // Opening another SQL connection, as an operator process after a crash does.
        self.c.db = Connection::open(self.root.join("catalog.sqlite3")).unwrap();
        self.c.db.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Version {
    id: String,
    bytes: Option<Vec<u8>>,
    at: i64,
}
struct Bucket {
    persist: Option<std::path::PathBuf>,
    die_after_delete: bool,
    clock: Cell<i64>,
    versions: RefCell<BTreeMap<String, Vec<Version>>>,
    next: Cell<u64>,
    fail_after_delete: Cell<bool>,
    safe: Cell<bool>,
    before_list: RefCell<Option<Box<dyn FnOnce()>>>,
}
impl Bucket {
    fn new() -> Self {
        Self {
            persist: None,
            die_after_delete: false,
            clock: Cell::new(10_000),
            versions: RefCell::new(BTreeMap::new()),
            next: Cell::new(0),
            fail_after_delete: Cell::new(false),
            safe: Cell::new(true),
            before_list: RefCell::new(None),
        }
    }
    fn version(&self, key: &str, bytes: Option<Vec<u8>>) -> String {
        self.next.set(self.next.get() + 1);
        let id = format!("v{}", self.next.get());
        self.versions
            .borrow_mut()
            .entry(key.into())
            .or_default()
            .push(Version {
                id: id.clone(),
                bytes,
                at: self.clock.get(),
            });
        if let Some(path) = &self.persist {
            std::fs::write(path, serde_json::to_vec(&*self.versions.borrow()).unwrap()).unwrap();
            std::fs::File::open(path).unwrap().sync_all().unwrap();
        }
        id
    }
    fn load(path: &Path) -> Self {
        let mut b = Self::new();
        b.persist = Some(path.to_owned());
        let versions: BTreeMap<String, Vec<Version>> =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        b.next.set(versions.values().map(|v| v.len() as u64).sum());
        *b.versions.borrow_mut() = versions;
        b
    }
    fn undo(&self, key: &str, marker: &str) {
        let mut all = self.versions.borrow_mut();
        let versions = all.get_mut(key).unwrap();
        let index = versions
            .iter()
            .position(|v| v.id == marker && v.bytes.is_none())
            .unwrap();
        versions.remove(index);
    }
    fn present(&self, key: &str) -> bool {
        self.head(key).unwrap().is_some()
    }
    fn markers(&self) -> usize {
        self.versions
            .borrow()
            .values()
            .flatten()
            .filter(|v| v.bytes.is_none())
            .count()
    }
}
impl Objects for Bucket {
    fn put_once(&self, key: &str, source: &mut dyn Read) -> Result<()> {
        let mut bytes = Vec::new();
        source.read_to_end(&mut bytes)?;
        if let Some(last) = self
            .versions
            .borrow()
            .get(key)
            .and_then(|v| v.last())
            .filter(|v| v.bytes.is_some())
        {
            ensure!(last.bytes.as_ref() == Some(&bytes), "write-once conflict");
            return Ok(());
        }
        self.version(key, Some(bytes));
        Ok(())
    }
    fn get(&self, key: &str) -> Result<Box<dyn Read>> {
        let bytes = self
            .versions
            .borrow()
            .get(key)
            .and_then(|v| v.last())
            .and_then(|v| v.bytes.clone())
            .context("missing object")?;
        Ok(Box::new(std::io::Cursor::new(bytes)))
    }
}
impl ControllerObjects for Bucket {
    fn configuration(&self) -> Result<()> {
        ensure!(self.safe.get(), "bad effective policy");
        Ok(())
    }
    fn marker(&self, _run: &str) -> Result<i64> {
        Ok(self.clock.get())
    }
    fn list(&self) -> Result<Vec<ListedObject>> {
        if let Some(hook) = self.before_list.borrow_mut().take() {
            hook();
        }
        Ok(self
            .versions
            .borrow()
            .iter()
            .filter_map(|(key, vs)| {
                let v = vs.last()?;
                let b = v.bytes.as_ref()?;
                Some(ListedObject {
                    key: key.clone(),
                    bytes: b.len() as u64,
                    modified: v.at,
                })
            })
            .collect())
    }
    fn head(&self, key: &str) -> Result<Option<ObjectVersion>> {
        Ok(self
            .versions
            .borrow()
            .get(key)
            .and_then(|v| v.last())
            .and_then(|v| {
                v.bytes.as_ref().map(|b| ObjectVersion {
                    bytes: b.len() as u64,
                    version: v.id.clone(),
                })
            }))
    }
    fn delete(&self, key: &str) -> Result<String> {
        let id = self.version(key, None);
        if self.die_after_delete {
            std::process::exit(86);
        }
        if self.fail_after_delete.replace(false) {
            anyhow::bail!("lost delete reply");
        }
        Ok(id)
    }
}
fn now() -> i64 {
    crate::image_store::now()
}
fn add(f: &Fixture, b: &Bucket, id: &str, kind: &str, parent: Option<&str>) -> Reference {
    let path = f.root.join(format!("source-{id}"));
    std::fs::write(&path, format!("sealed disk {id}")).unwrap();
    let source = store::Source {
        sha256: store::hash_file(&path).unwrap(),
        path,
    };
    let attempt = store::token().unwrap();
    let r = store::push(
        b,
        &attempt,
        kind,
        serde_json::json!({}),
        &BTreeMap::from([("disk.ext4".into(), source)]),
        u64::MAX,
    )
    .unwrap();
    f.c.db.execute("INSERT INTO artifacts(id,kind,owner,machine,local_name,attempt,digest,parent,state,store_url) VALUES(?1,?2,1,'machine',?1,?3,?4,?5,'stored',?6)",params![id,kind,r.attempt,r.digest,parent,URL]).unwrap();
    r
}
fn pin(f: &Fixture, id: &str) {
    f.c.db
        .execute(
            "INSERT INTO leases(artifact,holder,purpose,expires) VALUES(?1,'fixture','pull',?2)",
            params![id, now() + 7200],
        )
        .unwrap();
}
fn observe(f: &mut Fixture, b: &Bucket) {
    prepare(&mut f.c.db, b, URL, now(), false, None).unwrap();
    b.clock.set(b.clock.get() + GRACE + 1);
}
fn live(f: &mut Fixture, b: &Bucket) -> Run {
    prepare(&mut f.c.db, b, URL, now(), true, Some("fixture evidence")).unwrap()
}
fn state(f: &Fixture, id: &str) -> String {
    f.c.db
        .query_row("SELECT state FROM artifacts WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .unwrap()
}

#[test]
fn stale_gc_pauses_beyond_lease_lifetime_with_new_upload_overlapping_run_and_pinned_parent() {
    let mut f = Fixture::new();
    let b = Bucket::new();
    let old = add(&f, &b, "garbage", "image", None);
    let parent = add(&f, &b, "parent", "base", None);
    let child = add(&f, &b, "child", "image", Some("parent"));
    pin(&f, "child");
    let capture = add(&f, &b, "parked", "capture", None);
    f.c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node,current_capture,placement_managed) VALUES(1,'machine','parked','machine','{}','','parked',1)",[]).unwrap();
    let token = store::token().unwrap();
    let prefix = store::attempt_prefix(&token).unwrap();
    b.put_once(&format!("{prefix}disk.ext4.zst"), &mut &b"incomplete"[..])
        .unwrap();
    f.c.db.execute("INSERT INTO artifacts(id,kind,attempt,local_name,state,store_url) VALUES('expired','image',?1,'expired','uploading',?2)",params![token,URL]).unwrap();
    f.c.db.execute("INSERT INTO leases(artifact,holder,purpose,token,expires) VALUES('expired','runner','push',?1,?2)",params![token,now()-1]).unwrap();
    observe(&mut f, &b);
    let slow = live(&mut f, &b);
    assert_eq!(slow.cutoff, 10_001);
    assert_eq!(slow.candidates.len(), 3);
    assert_eq!(
        slow.bytes,
        slow.candidates.iter().map(|o| o.bytes).sum::<u64>()
    );
    assert_eq!(state(&f, "expired"), "deleting");
    b.clock.set(b.clock.get() + 20_000); // Much longer than the 2-hour upload lease.
    let image = crate::image_catalog::reserve(
        &mut f.c.db,
        "tools",
        "ubuntu",
        &parent.digest,
        &"a".repeat(64),
        URL,
        "runner",
        now() + 20_000,
    )
    .unwrap();
    let disk = f.root.join("fresh-disk");
    std::fs::write(&disk, b"fresh committed disk").unwrap();
    let fresh = store::push(
        &b,
        &image.reference.attempt,
        "image",
        crate::image_catalog::metadata(&image, serde_json::json!({})),
        &BTreeMap::from([(
            "disk.ext4".into(),
            store::Source {
                sha256: store::hash_file(&disk).unwrap(),
                path: disk,
            },
        )]),
        0,
    )
    .unwrap();
    crate::image_catalog::commit(
        &mut f.c.db,
        &image,
        &fresh,
        &store::read_manifest(&b, &fresh).unwrap(),
        now() + 20_001,
    )
    .unwrap();
    let overlap = prepare(&mut f.c.db, &b, URL, now() + 20_001, true, Some("fixture")).unwrap();
    sweep(&mut f.c.db, &b, URL, &overlap.id, now() + 20_000).unwrap();
    sweep(&mut f.c.db, &b, URL, &slow.id, now() + 20_000).unwrap();
    for r in [parent, child, capture, fresh] {
        assert!(b.present(&format!(
            "{}manifest.json",
            store::attempt_prefix(&r.attempt).unwrap()
        )));
        store::read_manifest(&b, &r).unwrap();
    }
    assert!(!b.present(&format!(
        "{}manifest.json",
        store::attempt_prefix(&old.attempt).unwrap()
    )));
    assert_eq!(state(&f, "garbage"), "deleted");
    assert_eq!(state(&f, "expired"), "deleted");
    // The rejected content-key scheme reproduces revision 2's counterexample.
    let mut content_keys = BTreeSet::from(["shared-hash"]);
    let stale_candidates = content_keys.clone();
    content_keys.insert("shared-hash");
    for key in stale_candidates {
        content_keys.remove(key);
    }
    assert!(!content_keys.contains("shared-hash"));
}

#[test]
fn live_roots_cover_snapshots_images_transfers_leases_lineage_and_pending_creates() {
    for case in 0..9 {
        let mut f = Fixture::new();
        let b = Bucket::new();
        add(
            &f,
            &b,
            "root",
            if case == 0 { "snapshot" } else { "capture" },
            None,
        );
        match case {
            0 => {
                f.c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata) VALUES(1,'snapshot','named','root','{}')",[]).unwrap();
            }
            1 => {
                f.c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node,current_capture) VALUES(1,'machine','placed','machine','{}','runner','root')",[]).unwrap();
            }
            2 => {
                f.c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata,node,current_capture) VALUES(1,'machine','parked','machine','{}','','root')",[]).unwrap();
            }
            3 => {
                f.c.db.execute("INSERT INTO images(name,version,artifact,base,base_digest,recipe_sha256,status) VALUES('image',1,'root','base','digest','recipe','kept')",[]).unwrap();
            }
            4 | 5 => {
                f.c.db.execute("INSERT INTO operations(id,owner,op,resource,state) VALUES(1,1,'restore','box','pending')",[]).unwrap();
                f.c.db.execute("INSERT INTO transfers(operation,kind,source,machine,source_node,source_epoch,source_policy_generation,epoch,capture,source_capture,step) VALUES(1,'restore','machine','machine','runner',0,0,1,?1,?2,'import_uncertain')",params![if case==4 {Some("root")}else{None},if case==5 {Some("root")}else{None}]).unwrap();
            }
            6 => {
                pin(&f, "root");
            }
            7 => {
                let r = add(&f, &b, "upload", "image", Some("root"));
                f.c.db.execute("INSERT INTO leases(artifact,parent,holder,purpose,token,expires) VALUES('upload','root','runner','push',?1,?2)",params![r.attempt,now()+7200]).unwrap();
            }
            _ => {
                f.c.db.execute("INSERT INTO operations(owner,op,resource,state,request) VALUES(1,'create','box','uncertain','{\"request\":{\"image\":\"root\"}}')",[]).unwrap();
            }
        }
        observe(&mut f, &b);
        let run = live(&mut f, &b);
        assert!(run.candidates.is_empty(), "root case {case}");
        sweep(&mut f.c.db, &b, URL, &run.id, now()).unwrap();
        assert_eq!(state(&f, "root"), "stored");
    }
}

#[test]
fn concurrent_pull_or_fork_either_roots_before_claim_or_rejects_terminal_artifact() {
    for purpose in ["pull", "fork"] {
        let mut f = Fixture::new();
        let b = Bucket::new();
        add(&f, &b, "retired", "image", None);
        observe(&mut f, &b);
        let path = f.root.join("catalog.sqlite3");
        let purpose = purpose.to_owned();
        *b.before_list.borrow_mut() = Some(Box::new(move || {
            std::thread::spawn(move || {
                let db=Connection::open(path).unwrap();db.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
                db.execute("INSERT INTO leases(artifact,holder,purpose,expires) VALUES('retired','racer',?1,?2)",params![purpose,now()+7200]).unwrap();
            }).join().unwrap();
        }));
        assert!(prepare(&mut f.c.db, &b, URL, now(), true, Some("fixture")).is_err());
        assert_eq!(state(&f, "retired"), "stored");
        assert_eq!(b.markers(), 0);
        f.c.db.execute("DELETE FROM leases", []).unwrap();
        observe(&mut f, &b);
        let run = live(&mut f, &b);
        assert_eq!(state(&f, "retired"), "deleting");
        for purpose in ["pull", "fork"] {
            assert!(f.c.db.execute("INSERT INTO leases(artifact,holder,purpose,expires) VALUES('retired','late',?1,?2)",params![purpose,now()+7200]).is_err());
        }
        assert!(f.c.db.execute("INSERT INTO artifacts(id,kind,parent,local_name,state,store_url) VALUES('late','image','retired','late','uploading',?1)",[URL]).is_err());
        sweep(&mut f.c.db, &b, URL, &run.id, now()).unwrap();
        assert!(
            f.c.db
                .execute("UPDATE artifacts SET state='stored' WHERE id='retired'", [])
                .is_err()
        );
    }
}

#[test]
fn half_sweep_restart_keeps_foreign_keys_history_and_rehearses_version_undo() {
    let mut f = Fixture::new();
    let b = Bucket::new();
    let r = add(&f, &b, "old", "snapshot", None);
    f.c.db.execute("INSERT INTO images(name,version,artifact,base,base_digest,recipe_sha256,status) VALUES('old',1,'old','base','digest','recipe','retired')",[]).unwrap();
    f.c.db.execute("INSERT INTO artifact_copies(artifact,node,state,last_used) VALUES('old','runner','present',0)",[]).unwrap();
    f.c.db
        .execute(
            "INSERT INTO leases(artifact,holder,purpose,expires) VALUES('old','expired','fork',0)",
            [],
        )
        .unwrap();
    f.c.db.execute("INSERT INTO operations(id,owner,op,resource,state) VALUES(1,1,'restore','old','succeeded')",[]).unwrap();
    f.c.db.execute("INSERT INTO transfers(operation,kind,source,machine,source_node,source_epoch,source_policy_generation,epoch,capture,step) VALUES(1,'restore','machine','machine','runner',0,0,1,'old','succeeded')",[]).unwrap();
    observe(&mut f, &b);
    let run = live(&mut f, &b);
    b.fail_after_delete.set(true);
    assert!(sweep(&mut f.c.db, &b, URL, &run.id, now()).is_err());
    assert_eq!(state(&f, "old"), "deleting");
    let prior: String = f
        .c
        .db
        .query_row(
            "SELECT prior_version FROM gc_candidates WHERE run=?1 AND key LIKE '%/manifest.json'",
            [&run.id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!prior.is_empty());
    let error: String =
        f.c.db
            .query_row("SELECT error FROM gc_runs WHERE id=?1", [&run.id], |r| {
                r.get(0)
            })
            .unwrap();
    assert!(error.contains("lost delete reply"));
    f.reopen();
    sweep(&mut f.c.db, &b, URL, &run.id, now()).unwrap();
    assert_eq!(state(&f, "old"), "deleted");
    assert_eq!(
        f.c.db
            .query_row::<i64, _, _>("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
                .get(0))
            .unwrap(),
        0
    );
    for o in &run.candidates {
        let marker = b.versions.borrow()[&o.key].last().unwrap().id.clone();
        b.undo(&o.key, &marker);
        assert!(b.present(&o.key));
    }
    store::read_manifest(&b, &r).unwrap();
    let path = f.root.join("recovered");
    store::pull(&b, &r, &path).unwrap();
    assert!(
        f.c.db
            .execute("UPDATE artifacts SET state='stored' WHERE id='old'", [])
            .is_err()
    );
    assert!(
        f.c.db
            .execute("DELETE FROM artifacts WHERE id='old'", [])
            .is_err()
    );
    assert_eq!(
        f.c.db
            .query_row::<i64, _, _>("SELECT count(*) FROM transfers", [], |r| r.get(0))
            .unwrap(),
        1
    );
}

#[test]
fn missing_invalid_manifest_bad_policy_and_incomplete_authority_fail_before_deletion() {
    for case in 0..6 {
        let mut f = Fixture::new();
        let b = Bucket::new();
        let r = add(&f, &b, "root", "image", None);
        pin(&f, "root");
        add(&f, &b, "old", "image", None);
        observe(&mut f, &b);
        let manifest = format!(
            "{}manifest.json",
            store::attempt_prefix(&r.attempt).unwrap()
        );
        match case {
            0 => {
                b.delete(&manifest).unwrap();
            }
            1 => {
                b.version(&manifest, Some(b"invalid manifest".to_vec()));
            }
            2 => {
                b.safe.set(false);
            }
            3 => {
                f.c.db
                    .execute("DELETE FROM settings WHERE key='legacy_import'", [])
                    .unwrap();
            }
            4 => {
                f.c.db.execute_batch("PRAGMA foreign_keys=OFF;INSERT INTO leases(artifact,holder,purpose,expires) VALUES('missing','runner','fork',9999999999);PRAGMA foreign_keys=ON;").unwrap();
            }
            _ => {
                f.c.db.execute_batch("PRAGMA user_version=7").unwrap();
            }
        }
        let before = b.markers();
        assert!(
            prepare(&mut f.c.db, &b, URL, now(), true, Some("fixture")).is_err(),
            "case {case}"
        );
        assert_eq!(b.markers(), before);
        assert_eq!(state(&f, "old"), "stored");
    }
}

#[test]
fn s3_clock_cutoff_and_newly_unreferenced_grace_ignore_local_dates() {
    let mut f = Fixture::new();
    let b = Bucket::new();
    let r = add(&f, &b, "old", "image", None);
    f.c.db
        .execute(
            "UPDATE artifacts SET created_at='1970-01-01',unreferenced_at='1970-01-01'",
            [],
        )
        .unwrap();
    let first = prepare(&mut f.c.db, &b, URL, i64::MAX - 10000, false, None).unwrap();
    assert!(first.candidates.is_empty());
    b.clock.set(10000 + GRACE);
    assert!(
        prepare(&mut f.c.db, &b, URL, now(), false, None)
            .unwrap()
            .candidates
            .is_empty()
    );
    pin(&f, "old");
    f.c.db.execute("DELETE FROM leases", []).unwrap();
    b.clock.set(10000 + GRACE + 10);
    assert!(
        prepare(&mut f.c.db, &b, URL, now(), false, None)
            .unwrap()
            .candidates
            .is_empty()
    );
    b.clock.set(b.clock.get() + GRACE + 1);
    let dry = prepare(&mut f.c.db, &b, URL, now(), false, None).unwrap();
    assert_eq!(dry.candidates.len(), 2);
    assert_eq!(state(&f, "old"), "stored");
    assert!(b.present(&format!(
        "{}manifest.json",
        store::attempt_prefix(&r.attempt).unwrap()
    )));
}

#[test]
fn expired_push_cannot_commit_or_renew_and_retry_writes_fresh_keys() {
    let mut f = Fixture::new();
    let b = Bucket::new();
    let base = add(&f, &b, "base", "base", None);
    let i = crate::image_catalog::reserve(
        &mut f.c.db,
        "tools",
        "ubuntu",
        &base.digest,
        &"a".repeat(64),
        URL,
        "runner",
        now() - 7201,
    )
    .unwrap();
    let p = f.root.join("disk");
    std::fs::write(&p, b"tool disk").unwrap();
    let sources = BTreeMap::from([(
        "disk.ext4".into(),
        store::Source {
            sha256: store::hash_file(&p).unwrap(),
            path: p,
        },
    )]);
    let reference = store::push(
        &b,
        &i.reference.attempt,
        "image",
        crate::image_catalog::metadata(&i, serde_json::json!({})),
        &sources,
        0,
    )
    .unwrap();
    let manifest = store::read_manifest(&b, &reference).unwrap();
    assert!(crate::image_catalog::commit(&mut f.c.db, &i, &reference, &manifest, now()).is_err());
    assert!(
        f.c.db
            .execute(
                "UPDATE leases SET expires=?1 WHERE artifact=?2",
                params![now() + 7200, i.artifact]
            )
            .is_err()
    );
    let next = crate::image_catalog::reserve(
        &mut f.c.db,
        "tools",
        "ubuntu",
        &base.digest,
        &"a".repeat(64),
        URL,
        "runner",
        now(),
    )
    .unwrap();
    assert_ne!(i.reference.attempt, next.reference.attempt);
    let next_r = store::push(
        &b,
        &next.reference.attempt,
        "image",
        crate::image_catalog::metadata(&next, serde_json::json!({})),
        &sources,
        0,
    )
    .unwrap();
    let next_m = store::read_manifest(&b, &next_r).unwrap();
    f.c.db
        .execute("UPDATE artifacts SET state='deleting' WHERE id='base'", [])
        .unwrap();
    assert!(crate::image_catalog::commit(&mut f.c.db, &next, &next_r, &next_m, now()).is_err());
}

#[test]
fn post_sweep_verification_preserves_failure_evidence() {
    let mut f = Fixture::new();
    let b = Bucket::new();
    let root = add(&f, &b, "live", "image", None);
    pin(&f, "live");
    add(&f, &b, "dead", "image", None);
    observe(&mut f, &b);
    let run = live(&mut f, &b);
    b.safe.set(false);
    assert!(sweep(&mut f.c.db, &b, URL, &run.id, now()).is_err());
    assert_eq!(b.markers(), 0);
    let error: String =
        f.c.db
            .query_row("SELECT error FROM gc_runs WHERE id=?1", [&run.id], |r| {
                r.get(0)
            })
            .unwrap();
    assert!(error.contains("bad effective policy"));
    b.safe.set(true);
    sweep(&mut f.c.db, &b, URL, &run.id, now()).unwrap();
    b.delete(&format!(
        "{}disk.ext4.zst",
        store::attempt_prefix(&root.attempt).unwrap()
    ))
    .unwrap();
    assert!(verify(&f.c.db, &b, URL, now()).is_err());
}

#[test]
fn activation_and_operator_flags_require_complete_fresh_catalog_bound_evidence() {
    use std::os::unix::fs::MetadataExt;
    let mut f = Fixture::new();
    let b = Bucket::new();
    let dry = prepare(&mut f.c.db, &b, URL, now(), false, None).unwrap();
    assert!(prepare(&mut f.c.db, &b, URL, now(), true, None).is_err());
    let meta = std::fs::metadata(f.root.join("catalog.sqlite3")).unwrap();
    let mut a = Activation {
        catalog_id: f
            .c
            .db
            .query_row(
                "SELECT value FROM settings WHERE key='store_gc_catalog_id'",
                [],
                |r| r.get(0),
            )
            .unwrap(),
        device: meta.dev(),
        inode: meta.ino(),
        store_url: URL.into(),
        dry_run: dry.id,
        expires: now() + 300,
        catalog_authoritative: true,
        policy_evidence_sha256: "a".repeat(64),
        undo_evidence_sha256: "b".repeat(64),
    };
    let path = f.root.join("activation.json");
    let write = |a: &Activation| {
        std::fs::write(&path, serde_json::to_vec(a).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    };
    write(&a);
    assert!(activation(&f.c.db, &f.root, URL, &path, now()).is_ok());
    a.catalog_authoritative = false;
    write(&a);
    assert!(activation(&f.c.db, &f.root, URL, &path, now()).is_err());
    a.catalog_authoritative = true;
    a.inode += 1;
    write(&a);
    assert!(activation(&f.c.db, &f.root, URL, &path, now()).is_err());
    a.inode -= 1;
    a.expires = now() - 1;
    write(&a);
    assert!(activation(&f.c.db, &f.root, URL, &path, now()).is_err());
    for args in [
        vec!["ow", "store", "gc", "--to", URL, "--live"],
        vec![
            "ow",
            "store",
            "gc",
            "--to",
            URL,
            "--live",
            "--activation",
            "proof",
            "--dry-run",
        ],
        vec!["ow", "store", "gc", "--to", URL, "--resume", "run"],
    ] {
        assert!(crate::Cli::try_parse_from(args).is_err());
    }
}

#[test]
fn namespace_alias_cannot_hide_live_roots_from_gc() {
    let mut f = Fixture::new();
    let b = Bucket::new();
    add(&f, &b, "live", "image", None);
    pin(&f, "live");
    observe(&mut f, &b);
    let run = prepare(
        &mut f.c.db,
        &b,
        &format!("{URL}/"),
        now(),
        true,
        Some("fixture"),
    )
    .unwrap();
    assert!(run.candidates.is_empty());
    sweep(&mut f.c.db, &b, URL, &run.id, now()).unwrap();
    assert_eq!(b.markers(), 0);
}

#[test]
fn terminal_guards_and_reader_floor_protect_retirement_roots() {
    let mut f = Fixture::new();
    let b = Bucket::new();
    add(&f, &b, "dead", "snapshot", None);
    observe(&mut f, &b);
    let run = live(&mut f, &b);
    assert!(crate::nodes::readable_by(&f.c.db, 5).is_err());
    for phase in 0..2 {
        for sql in [
            "INSERT INTO resources(owner,kind,name,physical,metadata) VALUES(1,'snapshot','new-name','dead','{}')",
            "INSERT INTO resources(owner,kind,name,physical,metadata,current_capture) VALUES(1,'machine','late','late','{}','dead')",
            "INSERT INTO images(name,version,artifact,base,base_digest,recipe_sha256,status) VALUES('late',1,'dead','base','digest','recipe','current')",
            "INSERT INTO operations(owner,op,resource,state,request) VALUES(1,'create','late','pending','{\"request\":{\"image\":\"dead\"}}')",
            "INSERT INTO leases(artifact,holder,purpose,expires) VALUES('dead','late','pull',9999999999)",
            "UPDATE artifacts SET state='uploading' WHERE id='dead'",
            "DELETE FROM artifacts WHERE id='dead'",
        ] {
            assert!(f.c.db.execute(sql, []).is_err(), "phase {phase}: {sql}");
        }
        if phase == 0 {
            sweep(&mut f.c.db, &b, URL, &run.id, now()).unwrap();
            assert_eq!(state(&f, "dead"), "deleted");
        }
    }
}

#[test]
fn missing_live_data_and_unknown_formats_are_not_deletion_authority() {
    for case in 0..2 {
        let mut f = Fixture::new();
        let b = Bucket::new();
        let r = add(&f, &b, "root", "image", None);
        pin(&f, "root");
        add(&f, &b, "dead", "image", None);
        observe(&mut f, &b);
        if case == 0 {
            b.delete(&format!(
                "{}disk.ext4.zst",
                store::attempt_prefix(&r.attempt).unwrap()
            ))
            .unwrap();
        } else {
            f.c.db
                .execute("UPDATE artifacts SET format=2 WHERE id='root'", [])
                .unwrap();
        }
        let before = b.markers();
        assert!(prepare(&mut f.c.db, &b, URL, now(), true, Some("fixture")).is_err());
        assert_eq!(b.markers(), before);
        assert_eq!(state(&f, "dead"), "stored");
        assert!(f.c.db.query_row::<bool,_,_>("SELECT EXISTS(SELECT 1 FROM gc_events WHERE operation='mark' AND error IS NOT NULL)",[],|r|r.get(0)).unwrap());
    }
}

#[test]
fn gc_process_entry() {
    let Some(root) = std::env::var_os("OW_GC_PROCESS_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let mut db = Connection::open(root.join("catalog.sqlite3")).unwrap();
    db.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    let run: String = db
        .query_row("SELECT id FROM gc_runs WHERE live=1", [], |r| r.get(0))
        .unwrap();
    let mut b = Bucket::load(&root.join("versioned-fixture.json"));
    b.die_after_delete = true;
    sweep(&mut db, &b, URL, &run, now()).unwrap();
    panic!("child should have exited during the delete reply");
}

#[test]
fn actual_collector_process_exit_after_delete_restarts_from_durable_receipts() {
    let mut f = Fixture::new();
    let mut b = Bucket::new();
    b.persist = Some(f.root.join("versioned-fixture.json"));
    add(&f, &b, "dead", "image", None);
    observe(&mut f, &b);
    let run = live(&mut f, &b);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "store_gc::tests::gc_process_entry",
            "--nocapture",
        ])
        .env("OW_GC_PROCESS_ROOT", &f.root)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(86));
    f.reopen();
    let b = Bucket::load(&f.root.join("versioned-fixture.json"));
    assert_eq!(b.markers(), 1);
    assert_eq!(state(&f, "dead"), "deleting");
    let version: Option<String> = f
        .c
        .db
        .query_row(
            "SELECT prior_version FROM gc_candidates WHERE run=?1 AND key LIKE '%/manifest.json'",
            [&run.id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(version.is_some());
    sweep(&mut f.c.db, &b, URL, &run.id, now()).unwrap();
    assert_eq!(state(&f, "dead"), "deleted");
    assert_eq!(b.markers(), 2);
}

#[test]
fn a_temporary_named_snapshot_restarts_grace_for_every_matching_artifact() {
    let mut f = Fixture::new();
    let b = Bucket::new();
    add(&f, &b, "first", "snapshot", None);
    add(&f, &b, "second", "snapshot", None);
    f.c.db
        .execute("UPDATE artifacts SET local_name='named'", [])
        .unwrap();
    observe(&mut f, &b);
    f.c.db.execute("INSERT INTO resources(owner,kind,name,physical,metadata) VALUES(1,'snapshot','saved','named','{}')",[]).unwrap();
    f.c.db
        .execute("DELETE FROM resources WHERE kind='snapshot'", [])
        .unwrap();
    let run = live(&mut f, &b);
    assert!(run.candidates.is_empty());
    assert_eq!(state(&f, "first"), "stored");
    assert_eq!(state(&f, "second"), "stored");
}

#[test]
fn transient_reference_during_mark_restarts_grace_before_terminal_claim() {
    for purpose in ["pull", "fork"] {
        let mut f = Fixture::new();
        let b = Bucket::new();
        let reference = add(&f, &b, "dead", "image", None);
        let prefix = store::attempt_prefix(&reference.attempt).unwrap();
        observe(&mut f, &b);
        let path = f.root.join("catalog.sqlite3");
        *b.before_list.borrow_mut() = Some(Box::new(move || {
            let db = Connection::open(path).unwrap();
            db.execute_batch("PRAGMA foreign_keys=ON").unwrap();
            db.execute("INSERT INTO leases(artifact,holder,purpose,expires) VALUES('dead','transient',?1,?2)", params![purpose,now()+7200]).unwrap();
            assert!(
                !db.query_row::<bool, _, _>(
                    "SELECT EXISTS(SELECT 1 FROM gc_grace WHERE artifact='dead')",
                    [],
                    |r| r.get(0)
                )
                .unwrap()
            );
            db.execute("DELETE FROM leases WHERE holder='transient'", [])
                .unwrap();
        }));
        match prepare(&mut f.c.db, &b, URL, now(), true, Some("fixture")) {
            Err(error) => assert!(error.to_string().contains("grace changed"), "{error:#}"),
            Ok(run) => {
                sweep(&mut f.c.db, &b, URL, &run.id, now()).unwrap();
                panic!(
                    "{purpose} grace reset was ignored: candidates={}, state={}, manifest_present={}",
                    run.candidates.len(),
                    state(&f, "dead"),
                    b.present(&format!("{prefix}manifest.json"))
                );
            }
        }
        assert_eq!(state(&f, "dead"), "stored");
        for name in ["manifest.json", "disk.ext4.zst"] {
            assert!(b.present(&format!("{prefix}{name}")));
        }
        assert_eq!(b.markers(), 0);
        // A fresh mark must observe a new server-clock hour before deletion is eligible.
        for advance in [0, GRACE, 1] {
            b.clock.set(b.clock.get() + advance);
            let run = live(&mut f, &b);
            if advance == 1 {
                assert_eq!(run.candidates.len(), 2);
            } else {
                assert!(run.candidates.is_empty());
            }
            sweep(&mut f.c.db, &b, URL, &run.id, now()).unwrap();
            assert_eq!(
                state(&f, "dead"),
                if advance == 1 { "deleted" } else { "stored" }
            );
        }
    }
}

#[test]
fn retained_drain_publications_are_roots_after_local_source_cleanup() {
    let mut f = Fixture::new();
    let b = Bucket::new();
    add(&f, &b, "retained-capture", "capture", None);
    add(&f, &b, "unreferenced", "capture", None);
    f.c.db.execute("INSERT INTO drain_publications(node,name,artifact,manifest) VALUES('test-runner','local-capture','retained-capture','{}')",[]).unwrap();
    observe(&mut f, &b);
    let run = live(&mut f, &b);
    sweep(&mut f.c.db, &b, URL, &run.id, now()).unwrap();
    assert_eq!(state(&f, "retained-capture"), "stored");
    assert_eq!(state(&f, "unreferenced"), "deleted");
}
