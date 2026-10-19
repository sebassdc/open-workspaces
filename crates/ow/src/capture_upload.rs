//! Agent-side publication of an already fenced capture using the existing store format.
use super::*;
use std::{collections::BTreeMap, io::Read};

fn receipt(root: &Path, a: &Artifact) -> std::path::PathBuf {
    root.join("snapshots")
        .join(&a.snapshot)
        .join(format!(".published-{}", a.reference.attempt))
}
fn published(root: &Path, a: &Artifact) -> Result<Value> {
    let path = receipt(root, a);
    if !path.exists() {
        return Ok(json!({"present":false}));
    }
    let result: Value = serde_json::from_slice(&fs::read(path)?)?;
    let reference: Reference = serde_json::from_value(result["reference"].clone())?;
    let objects = store::S3Objects::new(&a.store_url, None)?;
    let manifest = store::read_manifest(&objects, &reference)?;
    ensure!(
        reference.attempt == a.reference.attempt,
        "publication token differs"
    );
    let mut pinned = a.clone();
    pinned.reference = reference;
    native(&manifest, &pinned)?;
    Ok(result)
}
pub(super) fn job(root: &Path, request: &Value) -> Result<Value> {
    if request["op"] == "capture-progress" {
        let token = request["attempt"].as_str().context("attempt required")?;
        store::attempt_prefix(token)?;
        let path = root.join("transfer-progress").join(token);
        return if path.exists() {
            Ok(serde_json::from_slice(&fs::read(path)?)?)
        } else {
            Ok(Value::Null)
        };
    }
    let a: Artifact = serde_json::from_value(request["artifact"].clone())?;
    common::identifier(&a.id)?;
    common::identifier(&a.machine)?;
    common::identifier(&a.snapshot)?;
    store::attempt_prefix(&a.reference.attempt)?;
    ensure!(
        crate::image_store::configured_url(root)? == a.store_url,
        "controller store differs from runner configuration"
    );
    if request["op"] == "capture-published" {
        return published(root, &a);
    }
    ensure!(
        request["op"] == "capture-push"
            && request["expires"]
                .as_i64()
                .is_some_and(|at| at > now() && at <= now() + 7200),
        "expired capture publication"
    );
    // Share the sole recovery uploader's lock; explicit park never adds another scheduler.
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join("recovery-upload.lock"))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "recovery upload in progress; park will retry"
    );
    let cached = published(root, &a)?;
    if cached["present"] == true {
        return Ok(cached);
    }
    let seen = wire::request(root, json!({"op":"inspect","id":a.machine}))?;
    ensure!(
        seen["vmm_running"] == false
            && seen["lifecycle"]["phase"] == "exported"
            && seen["lifecycle"]["intent"]["capture"] == a.snapshot,
        "publication requires exact fenced export"
    );
    let objects = store::S3Objects::new(&a.store_url, None)?;
    objects.verify_configuration()?;
    publish(&objects, root, &a)
}
fn publish(objects: &dyn Objects, root: &Path, a: &Artifact) -> Result<Value> {
    let path = root.join("snapshots").join(&a.snapshot);
    ensure!(
        fs::symlink_metadata(&path)?.is_dir(),
        "capture is not a directory"
    );
    let raw = fs::read(path.join("manifest.json"))?;
    ensure!(raw.len() <= 65536, "native manifest too large");
    let metadata: Value = serde_json::from_slice(&raw)?;
    ensure!(
        metadata["workspace"] == a.machine
            && metadata["name"] == a.snapshot
            && metadata["managed"] == true,
        "capture lineage differs"
    );
    let hashes = metadata["files"]
        .as_object()
        .context("native hashes required")?;
    ensure!(
        hashes.len() == 1 && hashes.contains_key("disk.ext4")
            || hashes.len() == 3
                && ["disk.ext4", "state", "memory"]
                    .iter()
                    .all(|k| hashes.contains_key(*k)),
        "incomplete consistency set"
    );
    let mut sources = BTreeMap::new();
    let mut sizes = BTreeMap::new();
    for (file, hash) in hashes {
        let file_path = path.join(file);
        sources.insert(
            file.clone(),
            store::Source {
                path: file_path.clone(),
                sha256: hash.as_str().context("invalid native hash")?.into(),
            },
        );
        sizes.insert(format!("{}.zst", file), fs::metadata(file_path)?.len());
    }
    let progress = Progress::new(objects, root, &a.reference.attempt, sizes)?;
    let reference = store::push(
        &progress,
        &a.reference.attempt,
        "capture",
        json!({"native_manifest":std::str::from_utf8(&raw)?}),
        &sources,
        metadata["captured_at"].as_u64().unwrap_or(0),
    )?;
    let manifest = store::read_manifest(objects, &reference)?;
    let mut pinned = a.clone();
    pinned.reference = reference.clone();
    native(&manifest, &pinned)?;
    let stage = path.join(format!(".artifact-{}", a.reference.attempt));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&stage)?;
    use std::io::Write;
    file.write_all(&manifest.bytes()?)?;
    file.sync_all()?;
    fs::rename(stage, path.join(".artifact.json"))?;
    fs::File::open(&path)?.sync_all()?;
    let result = json!({"present":true,"reference":reference,"manifest":manifest});
    ow_core::runtime::atomic_json(&receipt(root, a), &result)?;
    Ok(result)
}
/// Progress counts completed compressed file objects in logical bytes; manifest validation is separate.
struct Progress<'a> {
    objects: &'a dyn Objects,
    path: std::path::PathBuf,
    sizes: BTreeMap<String, u64>,
    done: Arc<Mutex<u64>>,
}
impl<'a> Progress<'a> {
    fn new(
        objects: &'a dyn Objects,
        root: &Path,
        token: &str,
        sizes: BTreeMap<String, u64>,
    ) -> Result<Self> {
        store::attempt_prefix(token)?;
        let dir = root.join("transfer-progress");
        fs::create_dir_all(&dir)?;
        let p = Self {
            objects,
            path: dir.join(token),
            sizes,
            done: Arc::new(Mutex::new(0)),
        };
        p.save()?;
        Ok(p)
    }
    fn save(&self) -> Result<()> {
        ow_core::runtime::atomic_json(
            &self.path,
            &json!({"bytes_completed":*self.done.lock().unwrap(),"bytes_total":self.sizes.values().sum::<u64>(),"unit":"logical bytes in completed file objects"}),
        )
    }
}
impl Objects for Progress<'_> {
    fn put_once(&self, key: &str, source: &mut dyn Read) -> Result<()> {
        self.objects.put_once(key, source)?;
        if let Some(size) = key.rsplit('/').next().and_then(|name| self.sizes.get(name)) {
            *self.done.lock().unwrap() += size;
            self.save()?;
        }
        Ok(())
    }
    fn get(&self, key: &str) -> Result<Box<dyn Read>> {
        let input = self.objects.get(key)?;
        if let Some(size) = key.rsplit('/').next().and_then(|name| self.sizes.get(name)) {
            Ok(Box::new(Download {
                input,
                path: self.path.clone(),
                done: self.done.clone(),
                size: *size,
                total: self.sizes.values().sum(),
                finished: false,
            }))
        } else {
            Ok(input)
        }
    }
}
struct Download {
    input: Box<dyn Read>,
    path: std::path::PathBuf,
    done: Arc<Mutex<u64>>,
    size: u64,
    total: u64,
    finished: bool,
}
impl Read for Download {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.input.read(buffer)?;
        if count == 0 && !buffer.is_empty() && !self.finished {
            self.finished = true;
            let mut done = self.done.lock().unwrap();
            *done += self.size;
            ow_core::runtime::atomic_json(&self.path,&json!({"bytes_completed":*done,"bytes_total":self.total,"unit":"logical bytes in completed file objects"})).map_err(std::io::Error::other)?;
        }
        Ok(count)
    }
}
pub(super) fn pull(
    objects: &dyn Objects,
    root: &Path,
    a: &Artifact,
    attempt: &str,
    stage: &Path,
) -> Result<store::Manifest> {
    let manifest = store::read_manifest(objects, &a.reference)?;
    let sizes = manifest
        .files
        .iter()
        .map(|(name, file)| (format!("{name}.zst"), file.size))
        .collect();
    let progress = Progress::new(objects, root, attempt, sizes)?;
    let result = store::pull(&progress, &a.reference, stage)?;
    *progress.done.lock().unwrap() = progress.sizes.values().sum();
    progress.save()?;
    Ok(result)
}
#[cfg(test)]
#[path = "capture_upload_tests.rs"]
mod tests;

pub(super) fn refresh_progress(shared: &Shared, id: i64) -> Result<()> {
    let (root, node, token, operation) = {
        let c = shared.lock().unwrap();
        let t = c.transfer(id)?;
        let row:(String,String)=c.db.query_row("SELECT l.token,CASE WHEN l.purpose='push' THEN t.source_node ELSE t.target END FROM leases l JOIN transfers t ON t.id=?1 WHERE l.artifact=t.capture AND l.holder IN (?2,?3) ORDER BY l.id DESC LIMIT 1",params![id,name(&t),format!("{}-pull",name(&t))],|r|Ok((r.get(0)?,r.get(1)?)))?;
        (c.root.clone(), row.1, row.0, t.operation)
    };
    let progress = rpc(
        &root,
        &node,
        json!({"op":"capture-progress","attempt":token}),
    )?;
    if let (Some(done), Some(total)) = (
        progress["bytes_completed"].as_u64(),
        progress["bytes_total"].as_u64(),
    ) {
        ensure!(done <= total, "invalid progress");
        let c = shared.lock().unwrap();
        c.db.execute("UPDATE operations SET response=json_set(COALESCE(response,'{}'),'$.progress',json(?1)) WHERE id=?2",params![json!({"bytes_completed":done,"bytes_total":total,"unit":"logical bytes in completed file objects"}).to_string(),operation])?;
    }
    Ok(())
}
