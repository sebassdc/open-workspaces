//! Fixed-size, content-addressed chunks. Manifests commit last; chunks are never
//! overwritten or deleted here. No chain of older recovery points is required.
//!
//! A run sends only chunks the store lacks, in parallel and in process. What the
//! store holds comes from a local index per store (every chunk this runner
//! uploaded or listed), rebuilt from a listing when missing or a day old, so
//! known chunks need no request and a killed run resumes where it stopped.
use super::http::Http;
use super::*;
use crate::snapshot_digest::{CHUNK, CHUNKS_V1, Chunk, check, read_sidecar, root, zero_hash};
use std::collections::{BTreeMap, HashSet};
use std::os::unix::fs::FileExt;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

const MAX_MANIFEST: u64 = 32 << 20;
const TRANSFERS: usize = 8;
const RECONCILE_SECONDS: u64 = 24 * 3600;
const INDEX_HEADER: &str = "ow-chunk-index-v1";

impl S3Store {
    pub(super) fn check_recovery_retention(&self) -> Result<()> {
        let path = self.url.strip_prefix("s3://").unwrap();
        let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
        let version = self
            .aws()
            .args([
                "s3api",
                "get-bucket-versioning",
                "--bucket",
                bucket,
                "--output",
                "json",
            ])
            .output()?;
        ensure!(
            version.status.success(),
            "cannot verify snapshot bucket versioning: {}",
            String::from_utf8_lossy(&version.stderr)
        );
        let version: Value = serde_json::from_slice(&version.stdout)?;
        ensure!(
            version["Status"] == "Enabled",
            "recovery needs bucket versioning for seven-day undo"
        );
        let lifecycle = self
            .aws()
            .args([
                "s3api",
                "get-bucket-lifecycle-configuration",
                "--bucket",
                bucket,
                "--output",
                "json",
            ])
            .output()?;
        let rules = if lifecycle.status.success() {
            serde_json::from_slice::<Value>(&lifecycle.stdout)?
        } else {
            ensure!(
                String::from_utf8_lossy(&lifecycle.stderr)
                    .contains("(NoSuchLifecycleConfiguration)"),
                "cannot verify snapshot lifecycle: {}",
                String::from_utf8_lossy(&lifecycle.stderr)
            );
            serde_json::json!({"Rules":[]})
        };
        retention_rules(&rules, prefix)
    }

    pub(super) fn stored_manifest(&self, name: &str) -> Result<Value> {
        crate::common::identifier(name)?;
        let mut child = self.get(&self.key(name, "manifest.json"))?;
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .unwrap()
            .take(MAX_MANIFEST + 1)
            .read_to_end(&mut bytes)?;
        finished(child, "downloading manifest")?;
        ensure!(
            bytes.len() as u64 <= MAX_MANIFEST,
            "stored manifest too large"
        );
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Uploads one sealed snapshot as chunks, manifest last. Returns what it did.
    pub(super) fn push_chunks(&self, name: &str, directory: &Path) -> Result<Value> {
        let started = std::time::Instant::now();
        crate::common::identifier(name)?;
        let manifest = read_manifest(&directory.join("manifest.json"), name)?;
        ensure!(
            manifest.get("ow_chunks_v2").is_none(),
            "local manifest contains reserved transfer metadata"
        );
        let http = self.http()?;
        let manifest_key = format!("{}{name}/manifest.json", http.prefix);
        if http.exists(&manifest_key)? {
            ensure!(
                public_manifest(http, &manifest_key)? == manifest,
                "store already has a different snapshot named {name}"
            );
            return Ok(json!({"already_stored":true}));
        }
        let mut bytes_read = 0;
        let files: BTreeMap<String, Vec<Chunk>> = match manifest["digest"].as_str() {
            // The worker hashed these at capture; each file is sealed under its list.
            Some(CHUNKS_V1) => {
                let mut sealed = read_sidecar(directory)?;
                let mut files = BTreeMap::new();
                for file in DATA_FILES {
                    let chunks = sealed
                        .remove(file)
                        .context("chunk sidecar misses a file")?
                        .chunks;
                    ensure!(
                        manifest["files"][file] == root(&chunks)
                            && check(&chunks)? == fs::metadata(directory.join(file))?.len(),
                        "snapshot {name}: {file} does not match its manifest"
                    );
                    files.insert(file.to_owned(), chunks);
                }
                files
            }
            None => {
                let (files, read) = whole_file_chunks(name, directory, &manifest)?;
                bytes_read += read;
                files
            }
            Some(other) => bail!("unsupported snapshot digest {other}"),
        };
        let index = Index::open(self.index.as_deref(), &self.identity(), http)?;
        // Each unique nonzero chunk the store lacks, with one place to read it.
        let mut needed = BTreeMap::new();
        let (mut count, mut zero) = (0, 0);
        for (file, chunks) in &files {
            let mut offset = 0u64;
            for chunk in chunks {
                count += 1;
                if chunk.zero {
                    zero += 1;
                } else if !index.has(&chunk.hash) {
                    needed.entry(chunk.hash.as_str()).or_insert((
                        file.as_str(),
                        offset,
                        chunk.length,
                    ));
                }
                offset += chunk.length as u64;
            }
        }
        let sent = upload(http, directory, &needed, &index)?;
        let mut stored = manifest.clone();
        stored["ow_chunks_v2"] = serde_json::to_value(&files)?;
        let body = serde_json::to_vec(&stored)?;
        ensure!(
            body.len() as u64 <= MAX_MANIFEST,
            "snapshot exceeds chunk index limit"
        );
        if !http.put_new(&manifest_key, body)? {
            ensure!(
                public_manifest(http, &manifest_key)? == manifest,
                "concurrent different snapshot named {name}"
            );
        }
        Ok(json!({
            "chunks": count,
            "zero": zero,
            "new": needed.len(),
            "uploaded": sent.created,
            "bytes_uploaded": sent.bytes,
            "bytes_read": bytes_read + sent.read,
            "index": index.source,
            "seconds": (started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        }))
    }

    pub(super) fn pull_chunks(
        &self,
        chunks: Value,
        manifest: &Value,
        directory: &Path,
    ) -> Result<()> {
        let files: BTreeMap<String, Vec<Chunk>> = serde_json::from_value(chunks)?;
        ensure!(
            files.len() == DATA_FILES.len() && DATA_FILES.iter().all(|f| files.contains_key(*f)),
            "invalid chunk file index"
        );
        let chunked = match manifest["digest"].as_str() {
            None => false,
            Some(CHUNKS_V1) => true,
            Some(other) => bail!("unsupported snapshot digest {other}"),
        };
        let http = self.http()?;
        for file in DATA_FILES {
            let chunks = &files[file];
            let length = check(chunks)?;
            ensure!(
                !chunked || manifest["files"][file] == root(chunks),
                "downloaded {file} does not match its manifest"
            );
            let output = private_file(&directory.join(file))?;
            // Zero chunks stay holes.
            output.set_len(length)?;
            let mut jobs = Vec::new();
            let mut offset = 0u64;
            for chunk in chunks {
                if !chunk.zero {
                    jobs.push((offset, chunk));
                }
                offset += chunk.length as u64;
            }
            parallel(jobs.len(), |i| {
                let (offset, chunk) = jobs[i];
                let compressed = http.get(
                    &format!("{}chunks/{}.zst", http.prefix, chunk.hash),
                    CHUNK as u64 + (1 << 20),
                )?;
                let mut bytes = Vec::with_capacity(chunk.length);
                zstd::Decoder::new(&compressed[..])?
                    .take(chunk.length as u64 + 1)
                    .read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() == chunk.length && hex(&Sha256::digest(&bytes)) == chunk.hash,
                    "downloaded chunk integrity check failed"
                );
                output.write_all_at(&bytes, offset)?;
                Ok(())
            })?;
            output.sync_all()?;
            if !chunked {
                ensure!(
                    manifest["files"][file] == crate::store::hash_file(&directory.join(file))?,
                    "downloaded {file} does not match its manifest"
                );
            }
        }
        Ok(())
    }
}

/// A stored manifest without its transport metadata.
fn public_manifest(http: &Http, key: &str) -> Result<Value> {
    let mut stored: Value = serde_json::from_slice(&http.get(key, MAX_MANIFEST)?)?;
    stored
        .as_object_mut()
        .context("invalid manifest")?
        .remove("ow_chunks_v2");
    Ok(stored)
}

/// Chunks of a snapshot sealed with whole-file hashes: one full read, checked.
fn whole_file_chunks(
    name: &str,
    directory: &Path,
    manifest: &Value,
) -> Result<(BTreeMap<String, Vec<Chunk>>, u64)> {
    let mut files = BTreeMap::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut read = 0u64;
    for file in DATA_FILES {
        let mut source = File::open(directory.join(file))?;
        let mut whole = Sha256::new();
        let mut chunks = Vec::new();
        loop {
            let mut size = 0;
            while size < CHUNK {
                let n = source.read(&mut buffer[size..])?;
                if n == 0 {
                    break;
                }
                size += n;
            }
            if size == 0 {
                break;
            }
            let bytes = &buffer[..size];
            whole.update(bytes);
            read += size as u64;
            let zero = bytes.iter().all(|b| *b == 0);
            chunks.push(Chunk {
                hash: if zero {
                    zero_hash(size)
                } else {
                    hex(&Sha256::digest(bytes))
                },
                length: size,
                zero,
            });
            ensure!(
                chunks.len() <= 300_000,
                "snapshot exceeds chunk index limit"
            );
        }
        ensure!(
            manifest["files"][file] == hex(&whole.finalize()),
            "snapshot {name}: {file} does not match its manifest"
        );
        files.insert(file.into(), chunks);
    }
    Ok((files, read))
}

struct Sent {
    created: u64,
    bytes: u64,
    read: u64,
}

/// Uploads `needed` chunks with bounded parallelism (about 16 MiB per transfer),
/// recording each in the index as soon as the store holds it.
fn upload(
    http: &Http,
    directory: &Path,
    needed: &BTreeMap<&str, (&str, u64, usize)>,
    index: &Index,
) -> Result<Sent> {
    let mut sources = BTreeMap::new();
    for file in DATA_FILES {
        sources.insert(file, File::open(directory.join(file))?);
    }
    let jobs: Vec<_> = needed.iter().collect();
    let (created, bytes, read) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    parallel(jobs.len(), |i| {
        let (hash, (file, offset, length)) = jobs[i];
        let mut data = vec![0u8; *length];
        sources[file].read_exact_at(&mut data, *offset)?;
        read.fetch_add(*length as u64, Ordering::Relaxed);
        // Never send bytes other than the sealed ones.
        ensure!(
            hex(&Sha256::digest(&data)) == *hash,
            "{file} changed since it was sealed"
        );
        let body = zstd::stream::encode_all(&data[..], 3)?;
        let size = body.len() as u64;
        if http.put_new(&format!("{}chunks/{hash}.zst", http.prefix), body)? {
            created.fetch_add(1, Ordering::Relaxed);
            bytes.fetch_add(size, Ordering::Relaxed);
        }
        index.add(hash)
    })?;
    Ok(Sent {
        created: created.into_inner(),
        bytes: bytes.into_inner(),
        read: read.into_inner(),
    })
}

/// Runs `job(0..count)` on up to [`TRANSFERS`] threads; stops at the first error.
fn parallel(count: usize, job: impl Fn(usize) -> Result<()> + Sync) -> Result<()> {
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..TRANSFERS.min(count))
            .map(|_| {
                scope.spawn(|| -> Result<()> {
                    while !failed.load(Ordering::Relaxed) {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= count {
                            break;
                        }
                        if let Err(error) = job(i) {
                            failed.store(true, Ordering::Relaxed);
                            return Err(error);
                        }
                    }
                    Ok(())
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("transfer thread panicked"))
            .collect::<Result<Vec<_>>>()
    })?;
    Ok(())
}

/// Chunk hashes the store is known to hold. Entries are only ever added for
/// chunks a request proved stored; nothing in this tool deletes chunks, and a
/// listing replaces the set daily in case anything else did.
struct Index {
    known: Mutex<HashSet<String>>,
    log: Option<Mutex<File>>,
    source: &'static str,
}

impl Index {
    fn open(directory: Option<&Path>, identity: &str, http: &Http) -> Result<Self> {
        let now = crate::recovery::now();
        let path = directory.map(|d| d.join(format!("{identity}.chunks")));
        if let Some(path) = &path
            && let Ok(text) = fs::read_to_string(path)
        {
            let mut lines = text.lines();
            let fresh = lines
                .next()
                .and_then(|h| h.strip_prefix(INDEX_HEADER)?.trim().parse::<u64>().ok())
                .is_some_and(|at| at <= now && now - at < RECONCILE_SECONDS);
            if fresh {
                // A torn final line from a killed run is simply not trusted.
                let known = lines.filter(|l| valid_hash(l)).map(str::to_owned).collect();
                let log = OpenOptions::new().append(true).open(path)?;
                return Ok(Self {
                    known: Mutex::new(known),
                    log: Some(Mutex::new(log)),
                    source: "local",
                });
            }
        }
        let known = listed(http)?;
        let log = match &path {
            Some(path) => {
                fs::create_dir_all(path.parent().unwrap())?;
                let staging = path.with_extension(format!("tmp-{}", crate::common::nonce()?));
                let mut file = private_file(&staging)?;
                let mut text = format!("{INDEX_HEADER} {now}\n");
                for hash in &known {
                    text.push_str(hash);
                    text.push('\n');
                }
                file.write_all(text.as_bytes())?;
                file.sync_all()?;
                fs::rename(&staging, path)?;
                Some(Mutex::new(OpenOptions::new().append(true).open(path)?))
            }
            None => None,
        };
        Ok(Self {
            known: Mutex::new(known),
            log,
            source: "listed",
        })
    }

    fn has(&self, hash: &str) -> bool {
        self.known.lock().unwrap().contains(hash)
    }

    fn add(&self, hash: &str) -> Result<()> {
        if self.known.lock().unwrap().insert(hash.to_owned())
            && let Some(log) = &self.log
        {
            // No fsync: a lost entry costs one conditional upload that finds the chunk.
            log.lock()
                .unwrap()
                .write_all(format!("{hash}\n").as_bytes())?;
        }
        Ok(())
    }
}

/// Every chunk in the store, listed by first hex digit in parallel.
fn listed(http: &Http) -> Result<HashSet<String>> {
    let base = format!("{}chunks/", http.prefix);
    let known = Mutex::new(HashSet::new());
    let digits: Vec<char> = "0123456789abcdef".chars().collect();
    parallel(digits.len(), |i| {
        let keys = http.list(&format!("{base}{}", digits[i]))?;
        let mut known = known.lock().unwrap();
        known.extend(
            keys.iter()
                .filter_map(|k| k.strip_prefix(&base)?.strip_suffix(".zst"))
                .filter(|h| valid_hash(h))
                .map(str::to_owned),
        );
        Ok(())
    })?;
    Ok(known.into_inner().unwrap())
}

/// Current expiry cannot safely manage shared chunks or the latest durable point.
fn retention_rules(config: &Value, prefix: &str) -> Result<()> {
    let prefix = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    };
    for rule in config["Rules"]
        .as_array()
        .context("invalid bucket lifecycle rules")?
    {
        if rule["Status"] != "Enabled" {
            continue;
        }
        let filter = rule["Filter"]["Prefix"]
            .as_str()
            .or_else(|| rule["Filter"]["And"]["Prefix"].as_str())
            .or_else(|| rule["Prefix"].as_str())
            .unwrap_or("");
        if !prefix.starts_with(filter) && !filter.starts_with(&prefix) {
            continue;
        }
        ensure!(
            rule["Expiration"].get("Days").is_none() && rule["Expiration"].get("Date").is_none(),
            "recovery prefix has current-version age expiry; remove it before enabling recovery"
        );
        if let Some(days) = rule["NoncurrentVersionExpiration"].get("NoncurrentDays") {
            ensure!(
                days.as_u64().is_some_and(|d| d >= 7),
                "recovery needs at least seven days of noncurrent-version undo"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn retention_refuses_shared_chunk_expiry_and_short_undo() {
        let rules = |filter: Value, expiration: Value, undo: u64| json!({"Rules":[{"Status":"Enabled","Filter":filter,"Expiration":expiration,"NoncurrentVersionExpiration":{"NoncurrentDays":undo}}]});
        assert!(
            retention_rules(
                &rules(json!({"Prefix":"ow/snapshots/"}), json!({"Days":30}), 7),
                "ow/snapshots"
            )
            .is_err()
        );
        assert!(
            retention_rules(
                &rules(
                    json!({"And":{"Prefix":"ow/"}}),
                    json!({"Date":"2030-01-01"}),
                    7
                ),
                "ow/runners/a/snapshots"
            )
            .is_err()
        );
        assert!(retention_rules(&rules(json!({}), json!({}), 6), "ow/snapshots").is_err());
        assert!(
            retention_rules(
                &rules(json!({"Prefix":"ow/catalog/"}), json!({"Days":30}), 1),
                "ow/snapshots"
            )
            .is_ok()
        );
        assert!(
            retention_rules(
                &rules(json!({}), json!({"ExpiredObjectDeleteMarker":true}), 7),
                "ow/snapshots"
            )
            .is_ok()
        );
        assert!(retention_rules(&json!({"Rules":[]}), "ow/snapshots").is_ok());
    }
}
