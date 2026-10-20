use ow_core::snapshot_store::{S3Store, SnapshotStore};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn capture(dir: &Path, name: &str, disk: &[u8]) {
    fs::create_dir(dir).unwrap();
    let mut files = json!({});
    for (name, data) in [
        ("state", b"state".as_slice()),
        ("memory", b"memory".as_slice()),
        ("disk.ext4", disk),
    ] {
        fs::write(dir.join(name), data).unwrap();
        files[name] = json!(digest(data));
    }
    fs::write(
        dir.join("manifest.json"),
        json!({"name":name,"workspace":"box","files":files}).to_string(),
    )
    .unwrap();
}

#[test]
fn incremental_retry_integrity_and_legacy_restore() {
    // Each child has its own environment; parallel tests never mutate this process's.
    if std::env::var_os("OW_STORE_CHILD").is_none() {
        let root =
            std::env::temp_dir().join(format!("ow-transfer-{}", ow_core::common::nonce().unwrap()));
        fs::create_dir(&root).unwrap();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("s3"), include_bytes!("fake_s3.py")).unwrap();
        let mut server = Command::new("python3")
            .arg(bin.join("s3"))
            .arg(&root)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut port = String::new();
        BufReader::new(server.stdout.take().unwrap())
            .read_line(&mut port)
            .unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "incremental_retry_integrity_and_legacy_restore",
                "--nocapture",
            ])
            // The SDK's credential chain finds fixture keys first; no profile, no instance role.
            .env("AWS_ACCESS_KEY_ID", "AKIDFIXTURE")
            .env("AWS_SECRET_ACCESS_KEY", "fixture")
            .env("AWS_REGION", "us-east-1")
            .env("AWS_CONFIG_FILE", root.join("no-config"))
            .env("AWS_SHARED_CREDENTIALS_FILE", root.join("no-credentials"))
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env("OW_STORE_CHILD", "1")
            .env("OW_TEST_STORE", &root)
            .env(
                "OW_TEST_ENDPOINT",
                format!("http://127.0.0.1:{}", port.trim()),
            )
            .status()
            .unwrap();
        server.kill().unwrap();
        server.wait().unwrap();
        assert!(status.success(), "fixture failed at {}", root.display());
        fs::remove_dir_all(root).unwrap();
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("OW_TEST_STORE").unwrap());
    let endpoint = std::env::var("OW_TEST_ENDPOINT").unwrap();
    let store = S3Store::new("s3://bucket/snapshots", Some(&endpoint))
        .unwrap()
        .incremental()
        .index(&root.join("index"));
    let mut disk = vec![0; 24 << 20];
    disk[0] = 1;
    disk[8 << 20] = 2;
    capture(&root.join("one"), "one", &disk);
    for failure in ["no-versioning", "unsafe-lifecycle", "deny-head"] {
        fs::write(root.join(failure), "").unwrap();
        assert!(
            store.push("one", &root.join("one")).is_err(),
            "ignored {failure}"
        );
        assert!(
            !root.join("bucket").exists(),
            "uploaded before safety checks"
        );
        fs::remove_file(root.join(failure)).unwrap();
    }
    store.push("one", &root.join("one")).unwrap();
    let calls = |method: &str| -> usize {
        fs::read_to_string(root.join("calls.jsonl"))
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str::<Vec<String>>(s).unwrap())
            .filter(|a| a[0] == "http" && a[1] == method && a[2].contains("chunks/"))
            .count()
    };
    let count_puts = || calls("PUT");
    assert_eq!(count_puts(), 4); // state, memory, two disk chunks; zeros need no object.
    disk[8 << 20] = 3;
    capture(&root.join("two"), "two", &disk);
    fs::write(root.join("fail-manifest"), "").unwrap();
    assert!(store.push("two", &root.join("two")).is_err());
    assert_eq!(count_puts(), 5, "transferred unchanged bytes");
    assert_eq!(store.list().unwrap(), ["one"], "partial upload was visible");
    fs::remove_file(root.join("fail-manifest")).unwrap();
    store.push("two", &root.join("two")).unwrap();
    store.push("two", &root.join("two")).unwrap();
    assert_eq!(count_puts(), 5, "retry reuploaded chunks");
    assert_eq!(calls("HEAD"), 0, "checked chunks one request at a time");
    let remote = root.join("bucket/snapshots");
    let descriptor: Value =
        serde_json::from_slice(&fs::read(remote.join("two/manifest.json")).unwrap()).unwrap();
    assert!(descriptor["ow_chunks_v2"].is_object());
    let public: Value = serde_json::from_slice(&store.manifest("two").unwrap()).unwrap();
    assert!(public.get("ow_chunks_v2").is_none());
    let other = root.join("another-runner");
    store.pull("two", &other).unwrap();
    assert_eq!(fs::read(other.join("disk.ext4")).unwrap(), disk);
    assert_eq!(
        fs::metadata(other.join("memory"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o400
    );
    assert!(
        store.pull("two", &other).is_err(),
        "overwrote existing destination"
    );
    // An existing name cannot be assigned to different sealed bytes.
    capture(&root.join("collision"), "two", b"other");
    assert!(store.push("two", &root.join("collision")).is_err());
    // Original whole-file representation is still readable.
    let legacy = remote.join("legacy");
    capture(&legacy, "legacy", b"old disk");
    for file in ["state", "memory", "disk.ext4"] {
        let data = fs::read(legacy.join(file)).unwrap();
        fs::write(
            legacy.join(format!("{file}.zst")),
            zstd::encode_all(&data[..], 3).unwrap(),
        )
        .unwrap();
        fs::remove_file(legacy.join(file)).unwrap();
    }
    store.pull("legacy", &root.join("legacy-restored")).unwrap();
    assert_eq!(
        fs::read(root.join("legacy-restored/disk.ext4")).unwrap(),
        b"old disk"
    );
    // Missing/corrupt chunks must leave no apparent successful destination.
    let hash = descriptor["ow_chunks_v2"]["disk.ext4"][0]["hash"]
        .as_str()
        .unwrap();
    fs::write(remote.join(format!("chunks/{hash}.zst")), b"corrupt").unwrap();
    let broken = root.join("broken");
    assert!(store.pull("two", &broken).is_err());
    assert!(!broken.exists());
    killed_runs_resume_and_lost_indexes_reconcile(&root, &store);
    chunk_seals_transfer_without_rereading(&root, &store);
    println!(
        "Observed: first capture 4 nonzero chunks; one changed disk chunk added 1; interrupted commit + replay added 0; no chunk HEAD requests; second-runner bytes and legacy reads verified."
    );
}

/// Distinct nonzero 8 MiB chunks, `count` of them.
fn distinct(count: usize, salt: u8) -> Vec<u8> {
    let mut disk = vec![0; count << 23];
    for i in 0..count {
        disk[i << 23] = i as u8 + 1;
        disk[(i << 23) + 1] = salt;
    }
    disk
}

fn killed_runs_resume_and_lost_indexes_reconcile(root: &Path, store: &S3Store) {
    let disk = distinct(6, 7);
    capture(&root.join("big"), "big", &disk);
    fs::write(root.join("put-budget"), "2").unwrap();
    assert!(store.push("big", &root.join("big")).is_err());
    fs::remove_file(root.join("put-budget")).unwrap();
    assert!(!store.list().unwrap().contains(&"big".to_owned()));
    let resumed = store.push("big", &root.join("big")).unwrap();
    // Six new disk chunks (state and memory are stored already); the killed run sent two.
    assert_eq!(resumed["new"], 4, "{resumed}");
    assert_eq!(resumed["uploaded"], 4, "{resumed}");
    assert_eq!(resumed["index_dropped"], 0);

    // A runner that lost its index lists the store instead of resending.
    fs::remove_dir_all(root.join("index")).unwrap();
    let mut changed = disk.clone();
    changed[(3 << 23) + 9] = 1;
    capture(&root.join("big2"), "big2", &changed);
    let fresh = S3Store::new(
        "s3://bucket/snapshots",
        Some(&std::env::var("OW_TEST_ENDPOINT").unwrap()),
    )
    .unwrap()
    .incremental()
    .index(&root.join("index"));
    let report = fresh.push("big2", &root.join("big2")).unwrap();
    assert_eq!(report["new"], 1, "{report}");
    let back = root.join("big2-restored");
    fresh.pull("big2", &back).unwrap();
    assert_eq!(fs::read(back.join("disk.ext4")).unwrap(), changed);

    // A chunk removed from the store behind the index's back is never trusted:
    // a new point sends it again before its manifest names it ...
    let remote = root.join("bucket/snapshots");
    let descriptor: Value =
        serde_json::from_slice(&fs::read(remote.join("big2/manifest.json")).unwrap()).unwrap();
    let hash = descriptor["ow_chunks_v2"]["disk.ext4"][3]["hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let chunk = remote.join(format!("chunks/{hash}.zst"));
    fs::remove_file(&chunk).unwrap();
    capture(&root.join("big3"), "big3", &changed);
    let report = fresh.push("big3", &root.join("big3")).unwrap();
    assert_eq!(
        (&report["uploaded"], &report["index_dropped"]),
        (&json!(1), &json!(1)),
        "{report}"
    );
    let back = root.join("big3-restored");
    fresh.pull("big3", &back).unwrap();
    assert_eq!(fs::read(back.join("disk.ext4")).unwrap(), changed);
    // ... and a stored point that lost one is repaired from the same sealed bytes.
    fs::remove_file(&chunk).unwrap();
    assert!(fresh.pull("big2", &root.join("big2-broken")).is_err());
    let report = fresh.push("big2", &root.join("big2")).unwrap();
    assert_eq!(
        (&report["already_stored"], &report["uploaded"]),
        (&json!(true), &json!(1)),
        "{report}"
    );
    fresh.pull("big2", &root.join("big2-repaired")).unwrap();
}

/// A worker-sealed point is sent from its chunk sidecar; bytes are still checked.
fn chunk_seals_transfer_without_rereading(root: &Path, store: &S3Store) {
    let dir = root.join("sealed");
    fs::create_dir(&dir).unwrap();
    let disk = distinct(3, 11);
    for (file, data) in [
        ("state", &b"s2"[..]),
        ("memory", &b"m2"[..]),
        ("disk.ext4", &disk[..]),
    ] {
        fs::write(dir.join(file), data).unwrap();
    }
    let (files, _) =
        ow_core::snapshot_digest::seal_directory(&dir, &ow_core::snapshot_store::DATA_FILES, None)
            .unwrap();
    let manifest = json!({"name":"sealed","workspace":"box","files":files,"digest":ow_core::snapshot_digest::CHUNKS_V1});
    fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
    // Bytes changed after sealing are never sent under the sealed hash.
    let original = fs::read(dir.join("disk.ext4")).unwrap();
    let mut tampered = original.clone();
    tampered[(1 << 23) + 2] = 0x55;
    fs::write(dir.join("disk.ext4"), &tampered).unwrap();
    let error = store.push("sealed", &dir).unwrap_err().to_string();
    assert!(error.contains("changed since it was sealed"), "{error}");
    assert!(!store.list().unwrap().contains(&"sealed".to_owned()));
    fs::write(dir.join("disk.ext4"), &original).unwrap();
    store.push("sealed", &dir).unwrap();
    // The next point of unchanged data reads and sends nothing.
    let next = root.join("sealed-next");
    fs::create_dir(&next).unwrap();
    for file in ["state", "memory", "disk.ext4", "chunks.json"] {
        fs::hard_link(dir.join(file), next.join(file)).unwrap();
    }
    let mut renamed = manifest.clone();
    renamed["name"] = json!("sealed-next");
    fs::write(next.join("manifest.json"), renamed.to_string()).unwrap();
    let report = store.push("sealed-next", &next).unwrap();
    assert_eq!(
        (&report["new"], &report["bytes_read"]),
        (&json!(0), &json!(0)),
        "{report}"
    );
    let back = root.join("sealed-restored");
    store.pull("sealed", &back).unwrap();
    assert_eq!(fs::read(back.join("disk.ext4")).unwrap(), original);
    let public: Value = serde_json::from_slice(&store.manifest("sealed").unwrap()).unwrap();
    assert_eq!(public, manifest);
}
