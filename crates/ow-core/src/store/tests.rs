use super::*;
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("ow-store-test-{}", token().unwrap()));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn store(&self) -> DirectoryObjects {
        DirectoryObjects(self.0.join("objects"))
    }
    fn sources(&self) -> BTreeMap<String, Source> {
        let bytes = [vec![0; 8192], b"sealed disk\n".to_vec(), vec![0; 4096]].concat();
        let path = self.0.join("disk");
        fs::write(&path, &bytes).unwrap();
        BTreeMap::from([(
            "disk.ext4".into(),
            Source {
                path,
                sha256: sha256(&bytes),
            },
        )])
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn store_roundtrip_retry_and_new_attempt() {
    let f = Fixture::new();
    let objects = f.store();
    let sources = f.sources();
    let attempt = token().unwrap();
    let reference = push(
        &objects,
        &attempt,
        "image",
        serde_json::json!({"name":"tools","nested":{"z":2,"a":1}}),
        &sources,
        123,
    )
    .unwrap();
    assert_eq!(
        reference,
        push(
            &objects,
            &attempt,
            "image",
            serde_json::json!({"nested":{"a":1,"z":2},"name":"tools"}),
            &sources,
            123
        )
        .unwrap()
    );
    let path = f.0.join("pulled");
    let manifest = pull(&objects, &reference, &path).unwrap();
    assert_eq!(
        manifest.files["disk.ext4"].sha256,
        hash_file(&path.join("disk.ext4")).unwrap()
    );
    assert!(pull(&objects, &reference, &path).is_err());
    assert!(path.exists(), "existing destination preserved");
    assert!(
        push(
            &objects,
            &attempt,
            "image",
            serde_json::json!({"name":"different"}),
            &sources,
            123
        )
        .is_err()
    );
    let next = push(
        &objects,
        &token().unwrap(),
        "image",
        serde_json::json!({}),
        &sources,
        123,
    )
    .unwrap();
    assert_ne!(reference.attempt, next.attempt);
}
#[test]
fn store_rejects_bad_hash_before_manifest_and_cleans_bad_pull() {
    let f = Fixture::new();
    let objects = f.store();
    let mut sources = f.sources();
    let attempt = token().unwrap();
    sources.get_mut("disk.ext4").unwrap().sha256 = "0".repeat(64);
    assert!(
        push(
            &objects,
            &attempt,
            "image",
            serde_json::json!({}),
            &sources,
            1
        )
        .is_err()
    );
    assert!(
        !objects
            .0
            .join(attempt_prefix(&attempt).unwrap())
            .join("manifest.json")
            .exists()
    );
    let reference = push(
        &objects,
        &token().unwrap(),
        "image",
        serde_json::json!({}),
        &f.sources(),
        1,
    )
    .unwrap();
    let manifest = read_manifest(&objects, &reference).unwrap();
    fs::write(
        objects.0.join(&manifest.files["disk.ext4"].objects[0].key),
        zstd::encode_all(&b"evil"[..], 3).unwrap(),
    )
    .unwrap();
    let destination = f.0.join("rejected");
    assert!(pull(&objects, &reference, &destination).is_err());
    assert!(!destination.exists());
    let mut bad = reference;
    bad.digest = "0".repeat(64);
    assert!(read_manifest(&objects, &bad).is_err());
}
#[test]
fn store_refuses_untrusted_keys_formats_and_symlink_sources() {
    let f = Fixture::new();
    let objects = f.store();
    for key in [
        "../x",
        "u/aa/../../x",
        "u/aa/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/disk.ext4.zst/../x",
    ] {
        assert!(objects.put_once(key, &mut &b"x"[..]).is_err());
        assert!(objects.get(key).is_err());
    }
    for attempt in ["", "../x", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"] {
        assert!(attempt_prefix(attempt).is_err());
    }
    let mut sources = f.sources();
    let link = f.0.join("link");
    std::os::unix::fs::symlink(&sources["disk.ext4"].path, &link).unwrap();
    sources.get_mut("disk.ext4").unwrap().path = link;
    assert!(
        push(
            &objects,
            &token().unwrap(),
            "image",
            serde_json::json!({}),
            &sources,
            1
        )
        .is_err()
    );
    for url in ["s3://b/a/../c", "s3://b/a//c", "s3://b/a?b", "s3://b/./a"] {
        assert!(S3Objects::new(url, None).is_err());
    }
    assert!(S3Objects::new("s3://bucket/root", Some("http://example.com")).is_err());
}
#[test]
fn store_manifest_cannot_refer_to_another_attempt() {
    let f = Fixture::new();
    let objects = f.store();
    let reference = push(
        &objects,
        &token().unwrap(),
        "image",
        serde_json::json!({}),
        &f.sources(),
        1,
    )
    .unwrap();
    let mut manifest = read_manifest(&objects, &reference).unwrap();
    manifest.files.get_mut("disk.ext4").unwrap().objects[0].key = format!(
        "{}disk.ext4.zst",
        attempt_prefix(&token().unwrap()).unwrap()
    );
    assert!(manifest.bytes().is_err());
    manifest.format = 2;
    assert!(manifest.validate().is_err());
}
