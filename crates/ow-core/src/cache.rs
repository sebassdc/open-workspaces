//! Local sealed-image cache. Catalog claims authorize deletion; local locks exclude uses.
use crate::store::{self, Objects, Reference};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

pub const MAX_ENTRIES: usize = 32;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub name: String,
    pub digest: String,
    pub logical_bytes: u64,
    pub exclusive_bytes: Option<u64>,
    pub last_used: u64,
    pub pins: Vec<String>,
}
impl Entry {
    pub fn validate(&self) -> Result<()> {
        store::image_name(&self.name)?;
        store::valid_digest(&self.digest)?;
        ensure!(
            self.logical_bytes <= 16 * 1024 * crate::common::GIB,
            "cache size out of range"
        );
        ensure!(
            self.exclusive_bytes
                .is_none_or(|n| n <= self.logical_bytes.saturating_add(1024 * 1024)),
            "cache allocation out of range"
        );
        ensure!(
            self.pins.len() <= 8 && self.pins.iter().all(|s| s.len() <= 32),
            "cache pins out of range"
        );
        Ok(())
    }
}

/// Every runtime use and image install takes shared exclusion; eviction takes exclusive.
/// flock releases on process death, including an interrupted delete.
pub fn lock(root: &Path, exclusive: bool) -> Result<File> {
    fs::create_dir_all(root)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("cache.lock"))?;
    ensure!(
        unsafe {
            libc::flock(
                file.as_raw_fd(),
                if exclusive {
                    libc::LOCK_EX | libc::LOCK_NB
                } else {
                    libc::LOCK_SH
                },
            )
        } == 0,
        "cache lock failed"
    );
    Ok(file)
}
fn stamp(path: &Path) -> Result<Value> {
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_file() && m.nlink() == 1,
        "cache must contain independent regular files"
    );
    Ok(json!([
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec()
    ]))
}
fn local(root: &Path, name: &str) -> Result<(Value, PathBuf)> {
    store::image_name(name)?;
    let dir = root.join("images").join(name);
    ensure!(
        fs::symlink_metadata(&dir)?.is_dir(),
        "cache directory required"
    );
    let path = dir.join("manifest.json");
    ensure!(
        fs::symlink_metadata(&path)?.is_file() && path.metadata()?.len() <= 1024 * 1024,
        "cache manifest invalid"
    );
    let value: Value = serde_json::from_slice(&fs::read(path)?)?;
    ensure!(value["name"] == name, "cache name mismatch");
    Ok((value, dir))
}
fn verify(dir: &Path, value: &Value) -> Result<()> {
    let disk = dir.join("disk.ext4");
    let digest = value["disk_sha256"]
        .as_str()
        .context("cache disk digest missing")?;
    store::valid_digest(digest)?;
    let before = stamp(&disk)?;
    let verified = dir.join(".verified.json");
    let expected = json!({"stamp":before,"disk_digest":digest,"digest":value["image_digest"]});
    if fs::read(&verified)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .as_ref()
        != Some(&expected)
    {
        ensure!(
            store::hash_file(&disk)? == digest && stamp(&disk)? == before,
            "cache disk changed or corrupt"
        );
        crate::runtime::atomic_json(&verified, &expected)?;
    }
    Ok(())
}
/// Called with shared or exclusive exclusion. Unknown, corrupt and unstored files are not hits.
pub fn inventory(root: &Path) -> Result<Vec<Entry>> {
    let Ok(entries) = fs::read_dir(root.join("images")) else {
        return Ok(vec![]);
    };
    let mut result = Vec::new();
    let mut count = 0;
    for e in entries {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        count += 1;
        ensure!(count <= MAX_ENTRIES, "cache inventory limit exceeded");
        let checked = (|| -> Result<Entry> {
            let (v, dir) = local(root, &name)?;
            let reference: Reference = serde_json::from_value(v["store"]["reference"].clone())?;
            ensure!(
                v["image_digest"] == reference.digest,
                "cache reference differs"
            );
            store::attempt_prefix(&reference.attempt)?;
            store::valid_digest(&reference.digest)?;
            verify(&dir, &v)?;
            let mut pins = vec![];
            if dir.join(".pin").exists() {
                pins.push("local".into());
            }
            let entry = Entry {
                name,
                digest: reference.digest,
                logical_bytes: dir.join("disk.ext4").metadata()?.len(),
                exclusive_bytes: exclusive_bytes(&dir).ok(),
                last_used: fs::read_to_string(dir.join(".last-used"))
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                pins,
            };
            entry.validate()?;
            Ok(entry)
        })();
        if let Ok(entry) = checked {
            result.push(entry);
        }
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}
pub fn verify_image(root: &Path, name: &str) -> Result<()> {
    let (v, dir) = local(root, name)?;
    if v.get("store").is_some() {
        verify(&dir, &v)?;
    }
    Ok(())
}
pub fn touch(root: &Path, name: &str) -> Result<()> {
    store::image_name(name)?;
    let dir = root.join("images").join(name);
    if dir.exists() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        fs::write(dir.join(".last-used"), now.to_string())?;
    }
    Ok(())
}
/// Btrfs reports exclusive extents, not st_blocks (which includes shared reflinks).
fn exclusive_bytes(path: &Path) -> Result<u64> {
    let out = std::process::Command::new("btrfs")
        .args(["filesystem", "du", "--raw", "-s"])
        .arg(path)
        .output()?;
    ensure!(out.status.success(), "exclusive accounting unavailable");
    String::from_utf8(out.stdout)?
        .lines()
        .last()
        .context("missing Btrfs usage")?
        .split_whitespace()
        .nth(1)
        .context("missing exclusive bytes")?
        .parse()
        .context("invalid Btrfs exclusive bytes")
}
/// Conservative logical reservations include stopped and unmanaged on-disk machines.
/// Caller holds space_lock while changing files or admitting staging.
pub fn logical_disks(root: &Path) -> Result<u64> {
    let mut total = 0u64;
    let entries = match fs::read_dir(root.join("machines")) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    for e in entries {
        let disk = e?.path().join("disk.ext4");
        if disk.exists() {
            total = total
                .checked_add(disk.metadata()?.len())
                .context("disk reservation overflow")?;
        }
    }
    Ok(total)
}

/// statvfs includes unlinked but still mapped RAM files. Never add those bytes to free space.
pub fn available(root: &Path) -> Result<u64> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let p = CString::new(root.as_os_str().as_bytes())?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    ensure!(
        unsafe { libc::statvfs(p.as_ptr(), stats.as_mut_ptr()) } == 0,
        "storage capacity unavailable"
    );
    let s = unsafe { stats.assume_init() };
    Ok(s.f_bavail.saturating_mul(s.f_frsize))
}
pub fn sync(root: &Path) -> Result<()> {
    ensure!(
        unsafe { libc::syncfs(File::open(root)?.as_raw_fd()) } == 0,
        "cache sync failed"
    );
    Ok(())
}
/// A completed token is never executed again, even if a later pull reinstalls the same digest.
/// Caller holds exclusive local exclusion through the durable completion record.
pub fn evict_claim(
    root: &Path,
    token: &str,
    name: &str,
    reference: &Reference,
    objects: &dyn Objects,
) -> Result<Value> {
    store::attempt_prefix(token)?;
    let dir = root.join("cache-evictions");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{token}.json"));
    let intent = json!({"name":name,"reference":reference});
    if path.exists() {
        let recorded: Value = serde_json::from_slice(&fs::read(&path)?)?;
        ensure!(
            recorded["intent"] == intent,
            "eviction token reused with different input"
        );
        if recorded["done"] == true {
            return Ok(
                json!({"digest":reference.digest,"freed_bytes":0,"available_bytes":available(root)?}),
            );
        }
    } else {
        crate::runtime::atomic_json(&path, &json!({"intent":intent,"done":false}))?;
    }
    let result = evict(root, name, reference, objects)?;
    crate::runtime::atomic_json(&path, &json!({"intent":intent,"done":true}))?;
    Ok(result)
}

/// Must run under exclusive local exclusion and a durable controller claim.
/// Only installed image directories are addressable. No caller-supplied filesystem paths.
pub fn evict(
    root: &Path,
    name: &str,
    reference: &Reference,
    objects: &dyn Objects,
) -> Result<Value> {
    store::image_name(name)?;
    let manifest = store::read_manifest(objects, reference)?; // Commit marker, checked immediately before unlink.
    ensure!(
        matches!(manifest.kind.as_str(), "image" | "base"),
        "only sealed stored images are evictable"
    );
    store::valid_digest(&reference.digest)?;
    let (image, version) = store::image_name(name)?;
    ensure!(
        manifest.metadata["name"] == image && manifest.metadata["version"].as_i64() == version,
        "durable image identity differs"
    );
    let target = root.join("images").join(name);
    let trash = root
        .join("images")
        .join(format!(".evict-{}", reference.digest));
    sync(root)?;
    let before = available(root)?;
    if target.exists() {
        ensure!(
            !trash.exists(),
            "ambiguous cache eviction; preserve both copies"
        );
        let (v, dir) = local(root, name)?;
        ensure!(
            v["image_digest"] == reference.digest && v["store"]["reference"] == json!(reference),
            "stale cache eviction"
        );
        ensure!(!dir.join(".pin").exists(), "cache has a local pin");
        ensure!(
            v["disk_sha256"] == manifest.files["disk.ext4"].sha256,
            "durable copy differs"
        );
        verify(&dir, &v)?;
        check_contents(&dir)?;
        fs::rename(&dir, &trash)?;
        File::open(root.join("images"))?.sync_all()?;
    }
    if trash.exists() {
        ensure!(
            fs::symlink_metadata(&trash)?.is_dir(),
            "eviction trash must be a directory"
        );
        check_contents(&trash)?;
        fs::remove_dir_all(&trash)?;
    }
    File::open(root.join("images"))?.sync_all()?;
    sync(root)?;
    let after = available(root)?;
    Ok(
        json!({"digest":reference.digest,"freed_bytes":after.saturating_sub(before),"available_bytes":after}),
    )
}

// Only these sealed-image files may be discarded, including during interrupted-unlink recovery.
fn check_contents(dir: &Path) -> Result<()> {
    for e in fs::read_dir(dir)? {
        let e = e?;
        let name = e.file_name();
        let name = name.to_string_lossy();
        // atomic_json can leave a partial stamp on interruption; discard it only under eviction exclusion.
        let verification_temp = name.strip_prefix(".verified.tmp-").is_some_and(|nonce| {
            nonce.len() == 24
                && nonce
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        ensure!(
            (["manifest.json", "disk.ext4", ".verified.json", ".last-used"]
                .contains(&name.as_ref())
                || verification_temp)
                && e.file_type()?.is_file(),
            "unmanaged cache contents; preserve them"
        );
    }
    Ok(())
}

/// Optional hysteresis input; it changes no host limits or defaults.
#[derive(Clone, Copy)]
pub struct Policy {
    pub minimum_free: u64,
    pub capture_reserve: u64,
    pub trigger: u64,
    pub stop: u64,
}
impl Policy {
    pub fn target(self, free: u64, staging: u64) -> Result<Option<u64>> {
        ensure!(
            self.stop >= self.trigger,
            "cache stop must be at least trigger"
        );
        let base = self
            .minimum_free
            .checked_add(self.capture_reserve)
            .and_then(|n| n.checked_add(staging))
            .context("cache policy overflow")?;
        let trigger = base
            .checked_add(self.trigger)
            .context("cache trigger overflow")?;
        let stop = base.checked_add(self.stop).context("cache stop overflow")?;
        Ok((free < trigger).then_some(stop))
    }
}

/// Active downloads reserve their full logical size, not their compressed or shared size.
/// The per-reservation flock is held until installation/cleanup, so crash leftovers can be reclaimed.
pub struct Reservation {
    path: PathBuf,
    _file: File,
}
pub fn space_lock(root: &Path) -> Result<File> {
    let dir = root.join(".cache-staging");
    fs::create_dir_all(&dir)?;
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("cache.lock"))?;
    ensure!(
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0,
        "space lock failed"
    );
    Ok(f)
}
pub fn staging_bytes(root: &Path) -> Result<u64> {
    fs::create_dir_all(root.join(".cache-staging"))?;
    staging_locked(root)
}
fn staging_locked(root: &Path) -> Result<u64> {
    let mut total = 0u64;
    for (index, e) in fs::read_dir(root.join(".cache-staging"))?.enumerate() {
        ensure!(index <= 64, "too many staging reservations");
        let e = e?;
        if e.file_name() == "cache.lock" {
            continue;
        }
        ensure!(total < u64::MAX, "staging overflow");
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(e.path())?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            fs::remove_file(e.path())?;
            continue;
        }
        let n: u64 = fs::read_to_string(e.path())?.parse()?;
        total = total.checked_add(n).context("staging overflow")?;
    }
    Ok(total)
}
impl Reservation {
    pub fn new(root: &Path, bytes: u64, protected: u64) -> Result<Self> {
        let _lock = space_lock(root)?;
        let reserved = staging_locked(root)?;
        let required = protected
            .checked_add(logical_disks(root)?)
            .and_then(|n| n.checked_add(reserved))
            .and_then(|n| n.checked_add(bytes))
            .context("staging overflow")?;
        ensure!(
            available(root)? >= required,
            "cache staging headroom exhausted"
        );
        let path = root.join(".cache-staging").join(store::token()?);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0,
            "staging lock failed"
        );
        file.write_all(bytes.to_string().as_bytes())?;
        file.sync_all()?;
        Ok(Self { path, _file: file })
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(root) = self.path.parent().and_then(Path::parent) {
            let _lock = space_lock(root).ok();
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests;
