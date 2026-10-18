//! Operator/controller image authority. Additive tables share the ownership catalog.
use anyhow::{Context, Result, ensure};
use ow_core::{
    common,
    store::{self, Manifest, Reference},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

pub fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS artifacts(
      id TEXT PRIMARY KEY,kind TEXT NOT NULL CHECK(kind IN ('base','image','snapshot','capture')),
      attempt TEXT UNIQUE,digest TEXT,format INTEGER NOT NULL DEFAULT 1,
      owner INTEGER REFERENCES users(id),machine TEXT,local_name TEXT NOT NULL,
      parent TEXT REFERENCES artifacts(id),logical_bytes INTEGER,stored_bytes INTEGER,
      state TEXT NOT NULL CHECK(state IN ('uploading','stored','deleting','deleted')),
      store_url TEXT NOT NULL,created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,stored_at TEXT,unreferenced_at TEXT,deleted_at TEXT);
    CREATE TABLE IF NOT EXISTS artifact_copies(
      artifact TEXT NOT NULL REFERENCES artifacts(id),node TEXT NOT NULL,
      state TEXT NOT NULL CHECK(state IN ('pulling','present','evicting')),exclusive_bytes INTEGER,last_used INTEGER NOT NULL,PRIMARY KEY(artifact,node));
    CREATE TABLE IF NOT EXISTS leases(
      id INTEGER PRIMARY KEY,artifact TEXT NOT NULL REFERENCES artifacts(id),holder TEXT NOT NULL,
      purpose TEXT NOT NULL CHECK(purpose IN ('push','pull','fork','lazy')),token TEXT UNIQUE,
      parent TEXT REFERENCES artifacts(id),expires INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS images(
      name TEXT NOT NULL,version INTEGER NOT NULL,artifact TEXT NOT NULL REFERENCES artifacts(id),
      base TEXT NOT NULL,base_digest TEXT NOT NULL,recipe_sha256 TEXT NOT NULL,
      prefetch INTEGER NOT NULL DEFAULT 0,status TEXT NOT NULL CHECK(status IN ('building','current','kept','retired','failed')),
      created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,PRIMARY KEY(name,version));
    INSERT INTO settings(key,value) VALUES('min_reader_schema','3') ON CONFLICT(key) DO NOTHING;
    CREATE UNIQUE INDEX IF NOT EXISTS one_image_build ON images(name) WHERE status='building';
    CREATE UNIQUE INDEX IF NOT EXISTS one_current_image ON images(name) WHERE status='current';")?;
    Ok(())
}
pub fn open(root: &Path) -> Result<Connection> {
    ensure!(
        root.join("catalog.sqlite3").is_file(),
        "run on an initialized controller catalog"
    );
    let db = crate::nodes::db(root)?;
    db.execute_batch("BEGIN IMMEDIATE")?;
    initialize(&db)?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    db.execute_batch(&format!("PRAGMA user_version={}; COMMIT;", version.max(4)))?;
    Ok(db)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Image {
    pub name: String,
    pub version: i64,
    pub artifact: String,
    pub status: String,
    pub store_url: String,
    pub reference: Reference,
    pub base: String,
    pub base_digest: String,
    pub recipe_sha256: String,
    pub node: String,
    pub expires: i64,
    #[serde(default)]
    pub stale: bool,
}
fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Image> {
    Ok(Image {
        name: row.get(0)?,
        version: row.get(1)?,
        artifact: row.get(2)?,
        status: row.get(3)?,
        store_url: row.get(4)?,
        reference: Reference {
            attempt: row.get(5)?,
            digest: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
        },
        base: row.get(7)?,
        base_digest: row.get(8)?,
        recipe_sha256: row.get(9)?,
        node: row.get(10)?,
        expires: row.get(11)?,
        stale: false,
    })
}
const SELECT: &str = "SELECT i.name,i.version,i.artifact,i.status,a.store_url,a.attempt,a.digest,i.base,i.base_digest,i.recipe_sha256,l.holder,l.expires FROM images i JOIN artifacts a ON a.id=i.artifact JOIN leases l ON l.artifact=a.id AND l.purpose='push'";
pub fn list(db: &Connection) -> Result<Vec<Image>> {
    let mut images: Vec<Image> = db
        .prepare(&format!("{SELECT} ORDER BY i.name,i.version DESC"))?
        .query_map([], row)?
        .collect::<rusqlite::Result<_>>()?;
    for image in &mut images {
        image.stale = image.name != image.base
            && resolve(db, &image.base)
                .is_ok_and(|base| base.reference.digest != image.base_digest);
    }
    Ok(images)
}

pub fn resolve(db: &Connection, name: &str) -> Result<Image> {
    let (name, version) = store::image_name(name)?;
    db.query_row(&format!("{SELECT} WHERE i.name=?1 AND (?2 IS NULL AND i.status='current' OR i.version=?2 AND i.status IN ('current','kept')) AND a.state='stored'"), params![name,version], row).optional()?.context("image version is unavailable or retired")
}
/// Serializes versions and joins identical builds. A dead attempt is never revived.
pub fn reserve(
    db: &mut Connection,
    name: &str,
    base: &str,
    base_digest: &str,
    recipe: &str,
    store_url: &str,
    node: &str,
    now: i64,
) -> Result<Image> {
    common::identifier(name)?;
    common::identifier(node)?;
    store::valid_digest(base_digest)?;
    store::valid_digest(recipe)?;
    store::S3Objects::new(store_url, None)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let current = resolve(&tx, name).ok();
    if let Some(current) = current.filter(|i| {
        i.base_digest == base_digest && i.recipe_sha256 == recipe && i.store_url == store_url
    }) {
        tx.commit()?;
        return Ok(current);
    }
    let pending = tx
        .query_row(
            &format!("{SELECT} WHERE i.name=?1 AND i.status='building'"),
            [name],
            row,
        )
        .optional()?;
    if let Some(pending) = pending {
        if pending.expires > now {
            ensure!(
                pending.base_digest == base_digest
                    && pending.recipe_sha256 == recipe
                    && pending.store_url == store_url,
                "a different build of {name} is in progress"
            );
            tx.commit()?;
            return Ok(pending);
        }
        tx.execute(
            "UPDATE images SET status='failed' WHERE artifact=?1",
            [&pending.artifact],
        )?;
        tx.execute(
            "UPDATE artifacts SET unreferenced_at=CURRENT_TIMESTAMP WHERE id=?1",
            [&pending.artifact],
        )?;
    }
    let version: i64 = tx.query_row(
        "SELECT COALESCE(MAX(version),0)+1 FROM images WHERE name=?1",
        [name],
        |r| r.get(0),
    )?;
    let attempt = store::token()?;
    let artifact = format!("a-{}", common::nonce()?);
    let kind = if ["alpine", "arch", "ubuntu"].contains(&name) {
        "base"
    } else {
        "image"
    };
    let parent: Option<String> = if kind == "image" {
        Some(
            tx.query_row(
                "SELECT id FROM artifacts WHERE digest=?1 AND state='stored'",
                [base_digest],
                |r| r.get(0),
            )
            .optional()?
            .context("base digest is not a stored catalog artifact")?,
        )
    } else {
        None
    };
    tx.execute("INSERT INTO artifacts(id,kind,attempt,local_name,state,store_url,parent) VALUES(?1,?2,?3,?4,'uploading',?5,?6)", params![artifact,kind,attempt,format!("{name}@{version}"),store_url,parent])?;
    tx.execute(
        "INSERT INTO leases(artifact,holder,purpose,token,expires,parent) VALUES(?1,?2,'push',?3,?4,?5)",
        params![artifact, node, attempt, now + 7200,parent],
    )?;
    tx.execute("INSERT INTO images(name,version,artifact,base,base_digest,recipe_sha256,status) VALUES(?1,?2,?3,?4,?5,?6,'building')", params![name,version,artifact,base,base_digest,recipe])?;
    let image = tx.query_row(&format!("{SELECT} WHERE i.artifact=?1"), [&artifact], row)?;
    tx.commit()?;
    Ok(image)
}
/// Caller reads the manifest back by digest before entering this transaction.
pub fn commit(
    db: &mut Connection,
    image: &Image,
    reference: &Reference,
    manifest: &Manifest,
    now: i64,
) -> Result<Image> {
    manifest.validate()?;
    ensure!(
        manifest.kind
            == if ["alpine", "arch", "ubuntu"].contains(&image.name.as_str()) {
                "base"
            } else {
                "image"
            },
        "artifact kind mismatch"
    );
    ensure!(
        reference.attempt == image.reference.attempt
            && manifest.attempt == reference.attempt
            && store::sha256(&manifest.bytes()?) == reference.digest,
        "artifact reference mismatch"
    );
    let metadata = &manifest.metadata;
    ensure!(
        metadata["name"] == image.name
            && metadata["version"] == image.version
            && metadata["base_digest"] == image.base_digest
            && metadata["recipe_sha256"] == image.recipe_sha256,
        "image provenance mismatch"
    );
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let current = tx.query_row(
        &format!("{SELECT} WHERE i.artifact=?1"),
        [&image.artifact],
        row,
    )?;
    ensure!(
        current.name == image.name
            && current.version == image.version
            && current.base_digest == image.base_digest
            && current.recipe_sha256 == image.recipe_sha256
            && current.reference.attempt == reference.attempt,
        "build intent differs from catalog"
    );
    if current.status == "current" || current.status == "kept" {
        ensure!(
            current.reference == *reference,
            "committed image digest conflict"
        );
        tx.commit()?;
        return Ok(current);
    }
    ensure!(
        current.status == "building" && current.expires > now,
        "push lease expired or build is no longer current"
    );
    ensure!(tx.execute("UPDATE artifacts SET digest=?1,state='stored',logical_bytes=?2,stored_at=CURRENT_TIMESTAMP WHERE id=?3 AND state='uploading' AND attempt=?4", params![reference.digest,i64::try_from(manifest.files.values().map(|f| f.size).sum::<u64>())?,image.artifact,reference.attempt])? == 1, "artifact is not uploading");
    tx.execute(
        "UPDATE images SET status='kept' WHERE name=?1 AND status='current'",
        [&image.name],
    )?;
    tx.execute(
        "UPDATE images SET status='current',prefetch=1 WHERE artifact=?1",
        [&image.artifact],
    )?;
    tx.execute("UPDATE images SET status='retired' WHERE name=?1 AND status='kept' AND version NOT IN (SELECT version FROM images WHERE name=?1 AND status='kept' ORDER BY version DESC LIMIT 2)", [&image.name])?;
    tx.execute("UPDATE artifacts SET unreferenced_at=CURRENT_TIMESTAMP WHERE id IN (SELECT artifact FROM images WHERE status='retired') AND unreferenced_at IS NULL", [])?;
    tx.commit()?;
    resolve(db, &image.name)
}
#[cfg(test)]
fn fail(db: &Connection, image: &Image) -> Result<()> {
    db.execute(
        "UPDATE images SET status='failed' WHERE artifact=?1 AND status='building'",
        [&image.artifact],
    )?;
    db.execute(
        "UPDATE artifacts SET unreferenced_at=CURRENT_TIMESTAMP WHERE id=?1 AND state='uploading'",
        [&image.artifact],
    )?;
    Ok(())
}
pub fn metadata(image: &Image, local: Value) -> Value {
    serde_json::json!({"name":image.name,"version":image.version,"base":image.base,"base_digest":image.base_digest,"recipe_sha256":image.recipe_sha256,"local":local})
}

#[cfg(test)]
mod tests {
    use super::*;
    use ow_core::store::{Object, StoredFile};
    use std::collections::BTreeMap;
    fn db() -> Connection {
        database(Path::new(":memory:"))
    }
    fn database(path: &Path) -> Connection {
        let db = Connection::open(path).unwrap();
        db.execute_batch("PRAGMA foreign_keys=ON;CREATE TABLE users(id INTEGER PRIMARY KEY);CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);").unwrap();
        initialize(&db).unwrap();
        db.execute("INSERT INTO artifacts(id,kind,attempt,digest,local_name,state,store_url) VALUES('a-base','base',?1,?2,'ubuntu@1','stored','s3://bucket/root')",params![store::token().unwrap(),"a".repeat(64)]).unwrap();
        db
    }
    fn reserve_at(db: &mut Connection, n: u64, now: i64) -> Image {
        reserve(
            db,
            "tools",
            "ubuntu",
            &"a".repeat(64),
            &format!("{n:064x}"),
            "s3://bucket/root",
            "node",
            now,
        )
        .unwrap()
    }
    fn commit_at(db: &mut Connection, i: &Image, now: i64) -> Image {
        let manifest = Manifest {
            format: 1,
            kind: "image".into(),
            attempt: i.reference.attempt.clone(),
            created_unix: 1,
            metadata: metadata(i, serde_json::json!({})),
            files: BTreeMap::from([(
                "disk.ext4".into(),
                StoredFile {
                    size: 4,
                    allocated: 4096,
                    sha256: "b".repeat(64),
                    objects: vec![Object {
                        key: format!(
                            "{}disk.ext4.zst",
                            store::attempt_prefix(&i.reference.attempt).unwrap()
                        ),
                        codec: "zstd".into(),
                    }],
                },
            )]),
        };
        commit(
            db,
            i,
            &Reference {
                attempt: i.reference.attempt.clone(),
                digest: store::sha256(&manifest.bytes().unwrap()),
            },
            &manifest,
            now,
        )
        .unwrap()
    }
    #[test]
    fn image_versions_coalesce_keep_rollback_and_never_reuse_tokens() {
        let mut db = db();
        let first = reserve_at(&mut db, 1, 1);
        assert_eq!(first.reference, reserve_at(&mut db, 1, 2).reference);
        assert!(
            reserve(
                &mut db,
                "tools",
                "ubuntu",
                &"a".repeat(64),
                &"2".repeat(64),
                "s3://bucket/root",
                "node",
                2
            )
            .is_err()
        );
        let current = commit_at(&mut db, &first, 2);
        assert_eq!(reserve_at(&mut db, 1, 3).reference, current.reference);
        for n in 2..=4 {
            let i = reserve_at(&mut db, n, 10);
            commit_at(&mut db, &i, 11);
        }
        assert!(resolve(&db, "tools@1").is_err());
        assert_eq!(resolve(&db, "tools@2").unwrap().status, "kept");
        assert_eq!(resolve(&db, "tools").unwrap().version, 4);
        assert_eq!(
            db.query_row::<i64, _, _>(
                "SELECT count(*) FROM artifacts WHERE kind='image'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            4
        );
        let old = reserve_at(&mut db, 5, 20);
        let new = reserve_at(&mut db, 5, 8000);
        assert_ne!(old.reference.attempt, new.reference.attempt);
        assert_eq!(new.version, 6);
        assert_eq!(
            db.query_row::<String, _, _>("PRAGMA integrity_check", [], |r| r.get(0))
                .unwrap(),
            "ok"
        );
        assert!(
            db.prepare("PRAGMA foreign_key_check")
                .unwrap()
                .query([])
                .unwrap()
                .next()
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn image_list_marks_golden_builds_stale_when_the_base_changes() {
        let mut db = db();
        let image = reserve_at(&mut db, 1, 1);
        commit_at(&mut db, &image, 2);
        db.execute("INSERT INTO leases(artifact,holder,purpose,token,expires) SELECT id,'node','push',attempt,9999 FROM artifacts WHERE id='a-base'",[]).unwrap();
        db.execute("INSERT INTO images(name,version,artifact,base,base_digest,recipe_sha256,status) VALUES('ubuntu',1,'a-base','ubuntu',?1,?1,'current')",["a".repeat(64)]).unwrap();
        assert!(
            !list(&db)
                .unwrap()
                .iter()
                .find(|i| i.name == "tools")
                .unwrap()
                .stale
        );
        db.execute("INSERT INTO artifacts(id,kind,attempt,digest,local_name,state,store_url) VALUES('a-base2','base',?1,?2,'ubuntu@2','stored','s3://bucket/root')",params![store::token().unwrap(),"c".repeat(64)]).unwrap();
        db.execute("INSERT INTO leases(artifact,holder,purpose,token,expires) SELECT id,'node','push',attempt,9999 FROM artifacts WHERE id='a-base2'",[]).unwrap();
        db.execute("UPDATE images SET status='kept' WHERE name='ubuntu'", [])
            .unwrap();
        db.execute("INSERT INTO images(name,version,artifact,base,base_digest,recipe_sha256,status) VALUES('ubuntu',2,'a-base2','ubuntu',?1,?1,'current')",["c".repeat(64)]).unwrap();
        assert!(
            list(&db)
                .unwrap()
                .iter()
                .find(|i| i.name == "tools")
                .unwrap()
                .stale
        );
        let next = reserve(
            &mut db,
            "tools",
            "ubuntu",
            &"c".repeat(64),
            &format!("{:064x}", 1),
            "s3://bucket/root",
            "node",
            10,
        )
        .unwrap();
        assert_eq!(next.version, 2, "same recipe with a new base rebuilds");
    }
    #[test]
    fn image_concurrent_builds_share_one_durable_intent() {
        let root = std::env::temp_dir().join(format!("ow-image-race-{}", store::token().unwrap()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("catalog.sqlite3");
        drop(database(&path));
        let barrier = std::sync::Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = ["node-a", "node-b"]
                .into_iter()
                .map(|node| {
                    let path = &path;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        let mut db = Connection::open(path).unwrap();
                        db.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
                        barrier.wait();
                        reserve(
                            &mut db,
                            "tools",
                            "ubuntu",
                            &"a".repeat(64),
                            &"b".repeat(64),
                            "s3://bucket/root",
                            node,
                            1,
                        )
                        .unwrap()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results[0].reference, results[1].reference);
        assert_eq!(results[0].node, results[1].node);
        assert_eq!(
            Connection::open(path)
                .unwrap()
                .query_row::<i64, _, _>("SELECT count(*) FROM images", [], |r| r.get(0))
                .unwrap(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn image_failed_and_expired_builds_do_not_become_current() {
        let mut db = db();
        let i = reserve_at(&mut db, 1, 0);
        fail(&db, &i).unwrap();
        let next = reserve_at(&mut db, 1, 1);
        assert_ne!(i.reference.attempt, next.reference.attempt);
        let mut wrong = next.clone();
        wrong.reference.attempt = store::token().unwrap();
        // A failed attempt has a history row, never a resolvable current image.
        assert!(resolve(&db, "tools").is_err());
        let manifest = Manifest {
            format: 1,
            kind: "image".into(),
            attempt: next.reference.attempt.clone(),
            created_unix: 1,
            metadata: metadata(&next, serde_json::json!({})),
            files: BTreeMap::from([(
                "disk.ext4".into(),
                StoredFile {
                    size: 0,
                    allocated: 0,
                    sha256: "b".repeat(64),
                    objects: vec![Object {
                        key: format!(
                            "{}disk.ext4.zst",
                            store::attempt_prefix(&next.reference.attempt).unwrap()
                        ),
                        codec: "zstd".into(),
                    }],
                },
            )]),
        };
        let reference = Reference {
            attempt: next.reference.attempt.clone(),
            digest: store::sha256(&manifest.bytes().unwrap()),
        };
        assert!(commit(&mut db, &next, &reference, &manifest, 7201).is_err());
        assert!(commit(&mut db, &wrong, &reference, &manifest, 2).is_err());
        commit(&mut db, &next, &reference, &manifest, 2).unwrap();
        assert_eq!(
            commit(&mut db, &next, &reference, &manifest, 9999)
                .unwrap()
                .reference,
            reference
        );
    }
}

/// Internal replay of an already-pinned create may finish from a retired version.
pub fn resolve_stored(db: &Connection, selector: &str) -> Result<Image> {
    let (name, version) = store::image_name(selector)?;
    ensure!(version.is_some(), "version required");
    db.query_row(
        &format!("{SELECT} WHERE i.name=?1 AND i.version=?2 AND a.state='stored'"),
        params![name, version],
        row,
    )
    .optional()?
    .context("pinned image is no longer stored")
}
pub fn same_name(previous: &str, selector: &str) -> bool {
    selector.find('@').is_none()
        && store::image_name(previous)
            .is_ok_and(|(name, version)| name == selector && version.is_some())
}
