//! Fixed-size, content-addressed chunks. Manifests commit last; chunks are never
//! overwritten or deleted here. No chain of older recovery points is required.
use super::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::DirBuilderExt;

const CHUNK: usize = 8 << 20;
const MAX_MANIFEST: u64 = 32 << 20;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Chunk {
    hash: String,
    length: usize,
    zero: bool,
}

struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("ow-chunks-{}", crate::common::nonce()?));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl S3Store {
    fn object_args(&self, key: &str) -> Result<[String; 4]> {
        let path = key.strip_prefix("s3://").context("invalid S3 key")?;
        let (bucket, key) = path.split_once('/').context("missing S3 key")?;
        Ok(["--bucket".into(), bucket.into(), "--key".into(), key.into()])
    }

    fn exists(&self, key: &str) -> Result<bool> {
        let output = self
            .aws()
            .args(["s3api", "head-object"])
            .args(self.object_args(key)?)
            .output()
            .context("the AWS CLI (aws) is required for snapshot stores")?;
        if output.status.success() {
            return Ok(true);
        }
        let error = String::from_utf8_lossy(&output.stderr);
        ensure!(
            error.contains("(404)")
                || error.contains("(NoSuchKey)")
                || error.contains("(NotFound)"),
            "checking stored object failed ({}): {}",
            output.status,
            error
        );
        Ok(false)
    }

    fn put_once(&self, key: &str, path: &Path) -> Result<()> {
        let output = self
            .aws()
            .args(["s3api", "put-object"])
            .args(self.object_args(key)?)
            .args(["--if-none-match", "*", "--body"])
            .arg(path)
            .output()?;
        if output.status.success() {
            return Ok(());
        }
        // Another writer can finish an identical content-addressed object first.
        let error = String::from_utf8_lossy(&output.stderr);
        ensure!(
            error.contains("(PreconditionFailed)") || error.contains("(412)"),
            "conditional upload failed ({}): {}",
            output.status,
            error
        );
        bail!("object already committed: {key}")
    }

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

    pub(super) fn push_chunks(&self, name: &str, directory: &Path) -> Result<()> {
        crate::common::identifier(name)?;
        let manifest = read_manifest(&directory.join("manifest.json"), name)?;
        ensure!(
            manifest.get("ow_chunks_v2").is_none(),
            "local manifest contains reserved transfer metadata"
        );
        if self.exists(&self.key(name, "manifest.json"))? {
            let stored: Value = serde_json::from_slice(&self.manifest(name)?)?;
            ensure!(
                stored == manifest,
                "store already has a different snapshot named {name}"
            );
            return Ok(());
        }
        let scratch = Scratch::new()?;
        let body = scratch.0.join("body");
        let mut files: BTreeMap<String, Vec<Chunk>> = BTreeMap::new();
        let mut seen = BTreeSet::new();
        let mut buffer = vec![0u8; CHUNK];
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
                let hash = hex(&Sha256::digest(bytes));
                let zero = bytes.iter().all(|b| *b == 0);
                if !zero && seen.insert(hash.clone()) {
                    let key = self.key("chunks", &format!("{hash}.zst"));
                    if !self.exists(&key)? {
                        let mut output = OpenOptions::new()
                            .create(true)
                            .truncate(true)
                            .write(true)
                            .mode(0o600)
                            .open(&body)?;
                        zstd::stream::copy_encode(bytes, &mut output, 3)?;
                        output.sync_all()?;
                        if let Err(error) = self.put_once(&key, &body) {
                            // An identical chunk won the race; integrity is checked when pulled.
                            if !self.exists(&key)? {
                                return Err(error);
                            }
                        }
                    }
                }
                chunks.push(Chunk {
                    hash,
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
        let mut stored = manifest.clone();
        stored["ow_chunks_v2"] = serde_json::to_value(files)?;
        let bytes = serde_json::to_vec(&stored)?;
        ensure!(
            bytes.len() as u64 <= MAX_MANIFEST,
            "snapshot exceeds chunk index limit"
        );
        let mut output = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&body)?;
        output.write_all(&bytes)?;
        output.sync_all()?;
        if let Err(error) = self.put_once(&self.key(name, "manifest.json"), &body) {
            if !self.exists(&self.key(name, "manifest.json"))? {
                return Err(error);
            }
            let remote: Value = serde_json::from_slice(&self.manifest(name)?)?;
            ensure!(
                remote == manifest,
                "concurrent different snapshot named {name}"
            );
        }
        Ok(())
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
        for file in DATA_FILES {
            let mut output = private_file(&directory.join(file))?;
            let mut whole = Sha256::new();
            let mut length = 0u64;
            for chunk in &files[file] {
                ensure!(
                    valid_hash(&chunk.hash) && (1..=CHUNK).contains(&chunk.length),
                    "invalid chunk descriptor"
                );
                let bytes = if chunk.zero {
                    vec![0; chunk.length]
                } else {
                    let mut child =
                        self.get(&self.key("chunks", &format!("{}.zst", chunk.hash)))?;
                    let decoded = (|| -> Result<Vec<u8>> {
                        let mut bytes = Vec::new();
                        zstd::Decoder::new(child.stdout.take().unwrap())?
                            .take(chunk.length as u64 + 1)
                            .read_to_end(&mut bytes)?;
                        Ok(bytes)
                    })();
                    finished(child, "downloading chunk")?;
                    decoded?
                };
                ensure!(
                    bytes.len() == chunk.length && hex(&Sha256::digest(&bytes)) == chunk.hash,
                    "downloaded chunk integrity check failed"
                );
                whole.update(&bytes);
                if chunk.zero {
                    output.seek(SeekFrom::Current(chunk.length as i64))?;
                } else {
                    output.write_all(&bytes)?;
                }
                length = length
                    .checked_add(chunk.length as u64)
                    .context("snapshot length overflow")?;
            }
            output.set_len(length)?;
            output.sync_all()?;
            ensure!(
                manifest["files"][file] == hex(&whole.finalize()),
                "downloaded {file} does not match its manifest"
            );
        }
        Ok(())
    }
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
