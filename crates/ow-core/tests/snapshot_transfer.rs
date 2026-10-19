use ow_core::snapshot_store::{S3Store, SnapshotStore};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

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
    // Each child has its own PATH; parallel tests never mutate process environment.
    if std::env::var_os("OW_STORE_CHILD").is_none() {
        let root =
            std::env::temp_dir().join(format!("ow-transfer-{}", ow_core::common::nonce().unwrap()));
        fs::create_dir(&root).unwrap();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("aws"), include_bytes!("fake_aws.py")).unwrap();
        fs::set_permissions(bin.join("aws"), fs::Permissions::from_mode(0o700)).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "incremental_retry_integrity_and_legacy_restore",
                "--nocapture",
            ])
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("OW_STORE_CHILD", "1")
            .env("OW_TEST_STORE", &root)
            .status()
            .unwrap();
        assert!(status.success(), "fixture failed at {}", root.display());
        fs::remove_dir_all(root).unwrap();
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("OW_TEST_STORE").unwrap());
    let store = S3Store::new("s3://bucket/snapshots", None)
        .unwrap()
        .incremental();
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
    let count_puts = || -> usize {
        fs::read_to_string(root.join("calls.jsonl"))
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str::<Vec<String>>(s).unwrap())
            .filter(|a| a[1] == "put-object" && a.iter().any(|v| v.contains("chunks/")))
            .count()
    };
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
    println!(
        "Observed: first capture 4 nonzero chunks; one changed disk chunk added 1; interrupted commit + replay added 0; second-runner bytes and legacy reads verified."
    );
}
