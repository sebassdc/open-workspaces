use super::*;
use std::{collections::BTreeMap, io::Read};
struct Fixture {
    root: PathBuf,
    objects: store::DirectoryObjects,
}
impl Fixture {
    fn new() -> Self {
        let base = std::env::var_os("OW_CACHE_BTRFS_ROOT")
            .map(PathBuf::from)
            .unwrap_or(std::env::temp_dir());
        let root = base.join(format!("ow-cache-{}", store::token().unwrap()));
        fs::create_dir_all(root.join("images")).unwrap();
        Self {
            objects: store::DirectoryObjects(root.join("objects")),
            root,
        }
    }
    fn image(&self, name: &str, data: &[u8]) -> Reference {
        let source = self.root.join("source");
        fs::write(&source, data).unwrap();
        let reference = store::push(
            &self.objects,
            &store::token().unwrap(),
            "image",
            json!({"name":store::image_name(name).unwrap().0,"version":store::image_name(name).unwrap().1}),
            &BTreeMap::from([(
                "disk.ext4".into(),
                store::Source {
                    path: source.clone(),
                    sha256: store::hash_file(&source).unwrap(),
                },
            )]),
            1,
        )
        .unwrap();
        let dir = self.root.join("images").join(name);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("disk.ext4"), data).unwrap();
        fs::write(dir.join("manifest.json"),serde_json::to_vec(&json!({"name":name,"disk_sha256":store::hash_file(&source).unwrap(),"image_digest":reference.digest,"store":{"reference":reference}})).unwrap()).unwrap();
        reference
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn cache_inventory_is_verified_and_corruption_is_not_a_hit() {
    let f = Fixture::new();
    let r = f.image("tools@1", b"sealed");
    let entries = inventory(&f.root).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].digest, r.digest);
    touch(&f.root, "tools@1").unwrap();
    fs::write(f.root.join("images/tools@1/.pin"), b"operator").unwrap();
    let entries = inventory(&f.root).unwrap();
    assert!(entries[0].last_used > 0);
    assert_eq!(entries[0].pins, ["local"]);
    // A same-length rewrite must invalidate verification, including its ctime nanoseconds.
    fs::write(f.root.join("images/tools@1/disk.ext4"), b"broken").unwrap();
    assert!(inventory(&f.root).unwrap().is_empty());
    assert!(verify_image(&f.root, "tools@1").is_err());
    assert!(f.root.join("images/tools@1/disk.ext4").exists());
}
#[test]
fn cache_delete_requires_durable_manifest_and_excludes_new_local_uses() {
    let f = Fixture::new();
    let reference = f.image("tools@1", b"sealed");
    let shared = lock(&f.root, false).unwrap();
    assert!(lock(&f.root, true).is_err());
    drop(shared);
    let exclusive = lock(&f.root, true).unwrap();
    // Even while the durable object read is in flight, a new use cannot take a shared lock.
    struct Gate<'a> {
        inner: &'a dyn Objects,
        root: &'a Path,
    }
    impl Objects for Gate<'_> {
        fn put_once(&self, _: &str, _: &mut dyn Read) -> Result<()> {
            unreachable!()
        }
        fn get(&self, key: &str) -> Result<Box<dyn Read>> {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.root.join("cache.lock"))?;
            assert_ne!(
                unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
                0
            );
            self.inner.get(key)
        }
    }
    let marker = f.objects.0.join(format!(
        "{}manifest.json",
        store::attempt_prefix(&reference.attempt).unwrap()
    ));
    let bytes = fs::read(&marker).unwrap();
    fs::remove_file(&marker).unwrap();
    assert!(evict(&f.root, "tools@1", &reference, &f.objects).is_err());
    assert!(f.root.join("images/tools@1/disk.ext4").exists());
    fs::write(&marker, bytes).unwrap();
    evict(
        &f.root,
        "tools@1",
        &reference,
        &Gate {
            inner: &f.objects,
            root: &f.root,
        },
    )
    .unwrap();
    assert!(!f.root.join("images/tools@1").exists());
    drop(exclusive);
    assert!(lock(&f.root, false).is_ok());
}
#[test]
fn cache_pins_unknown_files_and_live_paths_are_preserved() {
    let f = Fixture::new();
    let reference = f.image("tools@1", b"data");
    let dir = f.root.join("images/tools@1");
    fs::write(dir.join(".pin"), b"pin").unwrap();
    let _lock = lock(&f.root, true).unwrap();
    assert!(evict(&f.root, "tools@1", &reference, &f.objects).is_err());
    fs::remove_file(dir.join(".pin")).unwrap();
    fs::write(dir.join("keep"), b"unknown local data").unwrap();
    assert!(evict(&f.root, "tools@1", &reference, &f.objects).is_err());
    fs::create_dir_all(f.root.join("machines/live")).unwrap();
    fs::write(f.root.join("machines/live/disk.ext4"), b"live").unwrap();
    assert!(evict(&f.root, "../../machines/live", &reference, &f.objects).is_err());
    assert_eq!(
        fs::read(f.root.join("machines/live/disk.ext4")).unwrap(),
        b"live"
    );
}
#[test]
fn cache_interrupted_unlink_is_retried_from_digest_named_trash() {
    let f = Fixture::new();
    let reference = f.image("tools@1", b"data");
    let trash = f
        .root
        .join("images")
        .join(format!(".evict-{}", reference.digest));
    fs::rename(f.root.join("images/tools@1"), &trash).unwrap();
    fs::remove_file(trash.join("manifest.json")).unwrap();
    let _lock = lock(&f.root, true).unwrap();
    evict(&f.root, "tools@1", &reference, &f.objects).unwrap();
    assert!(!trash.exists());
    evict(&f.root, "tools@1", &reference, &f.objects).unwrap();
}
#[test]
fn cache_burst_staging_reserves_aggregate_and_releases_on_drop() {
    let f = Fixture::new();
    let free = available(&f.root).unwrap();
    let first = Reservation::new(&f.root, free - 1024 * 1024, 0).unwrap();
    assert!(Reservation::new(&f.root, 2 * 1024 * 1024, 0).is_err());
    assert_eq!(staging_bytes(&f.root).unwrap(), free - 1024 * 1024);
    drop(first);
    assert_eq!(staging_bytes(&f.root).unwrap(), 0);
    let a = Reservation::new(&f.root, 1024, 0).unwrap();
    let b = Reservation::new(&f.root, 2048, 0).unwrap();
    assert_eq!(staging_bytes(&f.root).unwrap(), 3072);
    drop((a, b));
    assert!(Reservation::new(&f.root, u64::MAX, 1).is_err());
    // An abandoned record loses its OS lock; physical partial staging still counts in statvfs.
    fs::write(f.root.join(".cache-staging/abandoned"), b"100").unwrap();
    assert_eq!(staging_bytes(&f.root).unwrap(), 0);
}
#[test]
fn cache_hysteresis_is_policy_input_not_a_host_limit_change() {
    let gib = crate::common::GIB;
    let p = Policy {
        minimum_free: 5 * gib,
        capture_reserve: 4 * gib,
        trigger: 10 * gib,
        stop: 20 * gib,
    };
    assert_eq!(p.target(18 * gib, 0).unwrap(), Some(29 * gib));
    assert_eq!(p.target(19 * gib, 0).unwrap(), None);
    assert_eq!(p.target(19 * gib, 2 * gib).unwrap(), Some(31 * gib));
    assert!(
        Policy {
            stop: 1,
            trigger: 2,
            ..p
        }
        .target(0, 0)
        .is_err()
    );
}
#[test]
#[ignore = "requires a dedicated Btrfs mount in OW_CACHE_BTRFS_ROOT"]
fn cache_btrfs_shared_extents_use_actual_freed_bytes() {
    assert!(std::env::var_os("OW_CACHE_BTRFS_ROOT").is_some());
    let f = Fixture::new();
    let data = vec![0x71u8; 32 * 1024 * 1024];
    let reference = f.image("tools@1", &data);
    let disk = f.root.join("images/tools@1/disk.ext4");
    let live = f.root.join("live.ext4");
    crate::vm::reflink(&disk, &live).unwrap();
    sync(&f.root).unwrap();
    let shared_exclusive = inventory(&f.root).unwrap()[0].exclusive_bytes.unwrap();
    assert!(
        shared_exclusive < 1024 * 1024,
        "shared data must not be counted as exclusive: {shared_exclusive}"
    );
    let _lock = lock(&f.root, true).unwrap();
    let r = evict(&f.root, "tools@1", &reference, &f.objects).unwrap();
    assert!(
        r["freed_bytes"].as_u64().unwrap() < data.len() as u64 / 2,
        "shared clone still owns data: {r}"
    );
    assert_eq!(store::hash_file(&live).unwrap(), store::sha256(&data));
    let reference = f.image("unique@1", &data);
    sync(&f.root).unwrap();
    let exclusive = inventory(&f.root).unwrap()[0].exclusive_bytes.unwrap();
    assert!(exclusive >= data.len() as u64);
    let r = evict(&f.root, "unique@1", &reference, &f.objects).unwrap();
    assert!(
        r["freed_bytes"].as_u64().unwrap() >= data.len() as u64,
        "actual reclaim was {r}"
    );
    eprintln!(
        "method=btrfs-reflink-then-unlink n=1 logical_bytes={} shared_exclusive_bytes={} unique_reclaim={r}",
        data.len(),
        shared_exclusive
    );
}
#[test]
#[ignore = "requires a dedicated Btrfs mount in OW_CACHE_BTRFS_ROOT"]
fn cache_unlinked_but_mapped_ram_is_not_free_space() {
    let f = Fixture::new();
    let path = f.root.join("memory");
    let bytes = 32 * 1024 * 1024;
    fs::write(&path, vec![0x3au8; bytes]).unwrap();
    let file = File::open(&path).unwrap();
    let mapped = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            bytes,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    assert_ne!(mapped, libc::MAP_FAILED);
    drop(file);
    sync(&f.root).unwrap();
    let before = available(&f.root).unwrap();
    fs::remove_file(&path).unwrap();
    sync(&f.root).unwrap();
    let held = available(&f.root).unwrap();
    assert!(held.saturating_sub(before) < bytes as u64 / 2);
    assert_eq!(unsafe { *(mapped as *const u8) }, 0x3a);
    assert_eq!(unsafe { libc::munmap(mapped, bytes) }, 0);
    sync(&f.root).unwrap();
    let after = available(&f.root).unwrap();
    assert!(
        after.saturating_sub(held) >= bytes as u64,
        "mapped file was prematurely counted free"
    );
}

#[test]
fn cache_staging_rechecks_logical_disks_after_status_and_before_admission() {
    let f = Fixture::new();
    let before = available(&f.root).unwrap();
    fs::create_dir_all(f.root.join("machines/new")).unwrap();
    let disk = File::create(f.root.join("machines/new/disk.ext4")).unwrap();
    disk.set_len(before).unwrap();
    assert!(Reservation::new(&f.root, 4096, 0).is_err());
    assert_eq!(logical_disks(&f.root).unwrap(), before);
    // Cache exclusive accounting never releases this conservative reservation.
    assert_eq!(disk.metadata().unwrap().len(), before);
}

#[test]
fn cache_late_duplicate_claim_never_deletes_a_reinstalled_image() {
    let f = Fixture::new();
    let reference = f.image("tools@1", b"data");
    let token = store::token().unwrap();
    let _lock = lock(&f.root, true).unwrap();
    evict_claim(&f.root, &token, "tools@1", &reference, &f.objects).unwrap();
    // Reinstall exactly the original reference, as a new lease after controller completion may do.
    let manifest = store::read_manifest(&f.objects, &reference).unwrap();
    let dir = f.root.join("images/tools@1");
    store::pull(&f.objects, &reference, &dir).unwrap();
    fs::write(dir.join("manifest.json"),serde_json::to_vec(&json!({"name":"tools@1","disk_sha256":manifest.files["disk.ext4"].sha256,"image_digest":reference.digest,"store":{"reference":reference}})).unwrap()).unwrap();
    let replay = evict_claim(&f.root, &token, "tools@1", &reference, &f.objects).unwrap();
    assert_eq!(replay["freed_bytes"], 0);
    assert!(dir.join("disk.ext4").exists());
    assert!(evict_claim(&f.root, &token, "other@1", &reference, &f.objects).is_err());
}

#[test]
fn cache_eviction_preserves_near_match_and_non_regular_verification_temporaries() {
    for case in 0..5 {
        let f = Fixture::new();
        let reference = f.image("tools@1", b"sealed");
        let dir = f.root.join("images/tools@1");
        let name = match case {
            0 => ".verified.tmp-note".into(),
            1 => format!(".verified.tmp-{}", "A".repeat(24)),
            2 => format!(".verified.tmp-{}.keep", "a".repeat(24)),
            _ => format!(".verified.tmp-{}", crate::common::nonce().unwrap()),
        };
        let path = dir.join(name);
        match case {
            3 => std::os::unix::fs::symlink(f.root.join("source"), &path).unwrap(),
            4 => fs::create_dir(&path).unwrap(),
            _ => fs::write(&path, b"unmanaged data").unwrap(),
        }
        let _lock = lock(&f.root, true).unwrap();
        assert!(
            evict_claim(
                &f.root,
                &store::token().unwrap(),
                "tools@1",
                &reference,
                &f.objects
            )
            .unwrap_err()
            .to_string()
            .contains("unmanaged cache contents")
        );
        assert!(fs::symlink_metadata(path).is_ok());
        assert!(dir.join("disk.ext4").exists());
    }
}
