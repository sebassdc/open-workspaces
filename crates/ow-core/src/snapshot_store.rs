//! Off-host copies of sealed snapshots, for backup and for restoring a machine
//! onto a new runner.
//!
//! A snapshot is a directory with `state`, `memory`, `disk.ext4` and
//! `manifest.json` (which holds the SHA-256 of the other three). Stores keep the
//! data files zstd-compressed (sparse disks and idle memory shrink to their
//! used bytes) and write the manifest last, so a stored manifest means a
//! complete snapshot. Downloads are checked against the manifest's hashes and
//! written sparse.
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::OnceLock,
};

pub const DATA_FILES: [&str; 3] = ["state", "memory", "disk.ext4"];

pub trait SnapshotStore {
    /// Names of the complete snapshots in the store.
    fn list(&self) -> Result<Vec<String>>;
    /// Uploads one sealed snapshot directory under `name`, manifest last.
    /// Returns transfer details (null when there are none to report).
    fn push(&self, name: &str, directory: &Path) -> Result<Value>;
    /// The sealed manifest of `name`, without store transport metadata; unchecked bytes.
    fn manifest(&self, name: &str) -> Result<Vec<u8>>;
    /// Downloads `name` into `directory`, which must not exist yet. Every file is
    /// checked against the stored manifest; on any failure `directory` is removed.
    fn pull(&self, name: &str, directory: &Path) -> Result<()>;
    /// Whether [`Self::upload_unsealed`] works for this store.
    fn seals_while_uploading(&self) -> bool {
        false
    }
    /// Seals a capture its worker left unsealed while sending the chunks the store
    /// lacks (each byte read once), for the worker to commit before [`Self::push`].
    /// None when this store cannot.
    #[allow(clippy::type_complexity)]
    fn upload_unsealed(
        &self,
        _directory: &Path,
        _previous: Option<&Path>,
    ) -> Option<Result<(BTreeMap<String, crate::snapshot_digest::Sealed>, Value)>> {
        None
    }
}

/// S3 or an S3-compatible store (MinIO, R2), through the AWS SDK for Rust and
/// its usual credential chain.
pub struct S3Store {
    url: String,
    endpoint: Option<String>,
    incremental: bool,
    index: Option<PathBuf>,
    http: OnceLock<std::sync::Arc<http::Http>>,
    retention: OnceLock<()>,
}

impl S3Store {
    /// `url` is `s3://bucket[/prefix]`. `endpoint` must be https, or http on loopback:
    /// snapshots hold guest memory.
    pub fn new(url: &str, endpoint: Option<&str>) -> Result<Self> {
        ensure!(
            url.strip_prefix("s3://")
                .is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/')),
            "a store is s3://bucket[/prefix]"
        );
        if let Some(endpoint) = endpoint {
            let parsed = url::Url::parse(endpoint).context("--endpoint-url must be a URL")?;
            let host = parsed
                .host_str()
                .unwrap_or_default()
                .trim_matches(['[', ']']);
            let loopback = host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback());
            ensure!(
                parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback),
                "--endpoint-url must use https (http only for loopback)"
            );
        }
        Ok(Self {
            url: url.trim_end_matches('/').to_owned(),
            endpoint: endpoint.map(str::to_owned),
            incremental: false,
            index: None,
            http: OnceLock::new(),
            retention: OnceLock::new(),
        })
    }

    fn push_full(&self, name: &str, directory: &Path) -> Result<Value> {
        crate::common::identifier(name)?;
        let manifest = read_manifest(&directory.join("manifest.json"), name)?;
        let http = self.http()?;
        for file in DATA_FILES {
            let mut hasher = Sha256::new();
            let source = Hashing {
                input: File::open(directory.join(file))?,
                hash: &mut hasher,
            };
            let mut encoder = zstd::stream::read::Encoder::new(source, 3)?;
            http.put_stream(
                &self.object(name, &format!("{file}.zst")),
                &mut encoder,
                http::RANGE,
                false,
            )
            .with_context(|| format!("uploading {file}"))?;
            drop(encoder);
            // The bytes sent are the sealed bytes.
            ensure!(
                manifest["files"][file] == hex(&hasher.finalize()),
                "snapshot {name}: {file} does not match its manifest"
            );
        }
        http.put_stream(
            &self.object(name, "manifest.json"),
            &mut File::open(directory.join("manifest.json"))?,
            http::RANGE,
            false,
        )
        .context("uploading manifest")?;
        Ok(Value::Null)
    }

    /// Once per process: a run checks the bucket's retention once, not once per box.
    fn retention(&self) -> Result<()> {
        if self.retention.get().is_none() {
            self.check_recovery_retention()?;
            let _ = self.retention.set(());
        }
        Ok(())
    }

    /// Opt-in chunk format for managed recovery. Existing manual backups keep
    /// writing the original representation that older clients can restore.
    pub fn incremental(mut self) -> Self {
        self.incremental = true;
        self
    }

    /// Keeps the index of chunks this store holds in `directory`, across runs.
    pub fn index(mut self, directory: &Path) -> Self {
        self.index = Some(directory.to_owned());
        self
    }

    fn http(&self) -> Result<std::sync::Arc<http::Http>> {
        if let Some(http) = self.http.get() {
            return Ok(http.clone());
        }
        let _ = self.http.set(std::sync::Arc::new(http::Http::new(self)?));
        Ok(self.http.get().unwrap().clone())
    }

    pub fn identity(&self) -> String {
        hex(&Sha256::digest(
            format!("{}\n{}", self.url, self.endpoint.as_deref().unwrap_or("")).as_bytes(),
        ))
    }

    /// The object key of `file` of snapshot `name`.
    fn object(&self, name: &str, file: &str) -> String {
        let path = self.url.strip_prefix("s3://").unwrap_or_default();
        match path.split_once('/') {
            Some((_, prefix)) => format!("{prefix}/{name}/{file}"),
            None => format!("{name}/{file}"),
        }
    }

    /// The key under this store's bucket that `url` (`s3://bucket/key`) names.
    fn same_bucket<'a>(&self, url: &'a str) -> Result<&'a str> {
        let bucket = self.url["s3://".len()..]
            .split('/')
            .next()
            .unwrap_or_default();
        url.strip_prefix("s3://")
            .and_then(|rest| rest.strip_prefix(bucket))
            .and_then(|rest| rest.strip_prefix('/').or((rest.is_empty()).then_some(rest)))
            .with_context(|| format!("{url} is not in this store's bucket"))
    }

    /// (Last modified, key) of every object under `url` in this store's bucket.
    pub fn objects(&self, url: &str) -> Result<Vec<(i64, String)>> {
        let prefix = self.same_bucket(url.trim_end_matches('/'))?;
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };
        Ok(self
            .http()?
            .list(&prefix)?
            .into_iter()
            .map(|o| (o.modified, o.key))
            .collect())
    }

    /// Downloads the object `url` (in this store's bucket) into a new private file.
    pub fn download(&self, url: &str, path: &Path) -> Result<()> {
        let key = self.same_bucket(url)?;
        let mut reader = self.http()?.reader(key)?;
        let mut output = private_file(path)?;
        std::io::copy(&mut reader, &mut output)?;
        output.sync_all()?;
        Ok(())
    }
}

/// Hashes what is read through it.
struct Hashing<'a> {
    input: File,
    hash: &'a mut Sha256,
}
impl Read for Hashing<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let n = self.input.read(bytes)?;
        self.hash.update(&bytes[..n]);
        Ok(n)
    }
}

impl SnapshotStore for S3Store {
    fn list(&self) -> Result<Vec<String>> {
        let http = self.http()?;
        let keys = http.list(&http.prefix)?.into_iter().map(|o| o.key);
        Ok(snapshot_names(keys, &http.prefix))
    }

    fn push(&self, name: &str, directory: &Path) -> Result<Value> {
        if self.incremental {
            self.retention()?;
            self.push_chunks(name, directory)
        } else {
            self.push_full(name, directory)
        }
    }

    fn seals_while_uploading(&self) -> bool {
        self.incremental
    }

    fn upload_unsealed(
        &self,
        directory: &Path,
        previous: Option<&Path>,
    ) -> Option<Result<(BTreeMap<String, crate::snapshot_digest::Sealed>, Value)>> {
        self.incremental
            .then(|| S3Store::upload_unsealed(self, directory, previous))
    }

    fn manifest(&self, name: &str) -> Result<Vec<u8>> {
        let mut manifest = self.stored_manifest(name)?;
        manifest
            .as_object_mut()
            .context("invalid manifest")?
            .remove("ow_chunks_v2");
        Ok(serde_json::to_vec(&manifest)?)
    }

    fn pull(&self, name: &str, directory: &Path) -> Result<()> {
        crate::common::identifier(name)?;
        fs::create_dir(directory)?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        let result = (|| -> Result<()> {
            let mut stored = self.stored_manifest(name)?;
            let chunks = stored
                .as_object_mut()
                .context("invalid manifest")?
                .remove("ow_chunks_v2");
            let bytes = serde_json::to_vec(&stored)?;
            let manifest_path = directory.join("manifest.json");
            private_file(&manifest_path)?.write_all(&bytes)?;
            let manifest = read_manifest(&manifest_path, name)?;
            if let Some(chunks) = chunks {
                self.pull_chunks(chunks, &manifest, directory)?;
            } else {
                // Original whole-file representation remains readable.
                let http = self.http()?;
                for file in DATA_FILES {
                    let reader = http.reader(&self.object(name, &format!("{file}.zst")))?;
                    let mut output = private_file(&directory.join(file))?;
                    let decoded = write_sparse(zstd::Decoder::new(reader)?, &mut output)?;
                    ensure!(
                        manifest["files"][file] == decoded,
                        "snapshot {name}: downloaded {file} does not match its manifest"
                    );
                }
            }
            for file in DATA_FILES.iter().chain(&["manifest.json"]) {
                fs::set_permissions(directory.join(file), fs::Permissions::from_mode(0o400))?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(directory);
        }
        result
    }
}

#[path = "snapshot_chunks.rs"]
mod chunks;
#[path = "snapshot_http.rs"]
pub(crate) mod http;

pub(crate) fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn private_file(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}

/// A snapshot manifest that names `name` and lists a hash for each data file.
pub fn read_manifest(path: &Path, name: &str) -> Result<Value> {
    let manifest: Value = serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("invalid manifest {}", path.display()))?;
    ensure!(
        manifest["name"] == name
            && manifest["workspace"]
                .as_str()
                .is_some_and(|w| crate::common::identifier(w).is_ok())
            && DATA_FILES
                .iter()
                .all(|f| manifest["files"][f].as_str().is_some_and(|h| valid_hash(h))),
        "manifest {} is not a sealed snapshot named {name}",
        path.display()
    );
    Ok(manifest)
}

use crate::snapshot_digest::hex;

/// Copies `reader` into `output`, seeking over all-zero 4 KiB blocks so the file
/// stays sparse. Returns the SHA-256 of all bytes.
pub fn write_sparse(mut reader: impl Read, output: &mut File) -> Result<String> {
    const BUFFER: usize = 1 << 20;
    let mut hasher = Sha256::new();
    let mut length = 0u64;
    // Read and hash here, write on a second thread: neither waits for the other.
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);
    let out = &mut *output;
    std::thread::scope(|scope| -> Result<()> {
        let writer = scope.spawn(move || -> Result<()> {
            for data in receiver {
                write_runs(out, &data)?;
            }
            Ok(())
        });
        let read = (|| -> Result<()> {
            loop {
                let mut buffer = vec![0u8; BUFFER];
                let mut filled = 0;
                while filled < BUFFER {
                    match reader.read(&mut buffer[filled..])? {
                        0 => break,
                        n => filled += n,
                    }
                }
                buffer.truncate(filled);
                hasher.update(&buffer);
                length += filled as u64;
                // A closed channel means the writer failed; its error is returned.
                if filled == 0 || sender.send(buffer).is_err() || filled < BUFFER {
                    return Ok(());
                }
            }
        })();
        drop(sender);
        writer.join().expect("sparse writer thread panicked")?;
        read
    })?;
    output.set_len(length)?;
    output.sync_all()?;
    Ok(hex(&hasher.finalize()))
}

/// Writes `data` at the current offset: one write or seek per run of data or
/// all-zero 4 KiB blocks, not per block.
fn write_runs(output: &mut File, data: &[u8]) -> Result<()> {
    const BLOCK: usize = 4096;
    let zero = |at: usize| {
        data[at..(at + BLOCK).min(data.len())]
            .iter()
            .all(|b| *b == 0)
    };
    let mut start = 0;
    while start < data.len() {
        let hole = zero(start);
        let mut end = (start + BLOCK).min(data.len());
        while end < data.len() && zero(end) == hole {
            end = (end + BLOCK).min(data.len());
        }
        if hole {
            output.seek(SeekFrom::Current((end - start) as i64))?;
        } else {
            output.write_all(&data[start..end])?;
        }
        start = end;
    }
    Ok(())
}

/// Snapshot names with a stored manifest, from the keys under `prefix` (a key
/// prefix ending in `/`, or empty).
fn snapshot_names(keys: impl Iterator<Item = String>, prefix: &str) -> Vec<String> {
    let mut names: Vec<String> = keys
        .filter_map(|key| {
            Some(
                key.strip_prefix(prefix)?
                    .strip_suffix("/manifest.json")?
                    .to_owned(),
            )
        })
        .filter(|name| crate::common::identifier(name).is_ok())
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_take_s3_urls_and_safe_endpoints_only() {
        assert!(S3Store::new("s3://bucket/ow", None).is_ok());
        assert!(S3Store::new("s3://b", Some("http://127.0.0.1:9000")).is_ok());
        assert!(S3Store::new("s3://b", Some("https://r2.example.com")).is_ok());
        for (url, endpoint) in [
            ("/tmp/x", None),
            ("s3://", None),
            ("s3:///x", None),
            ("s3://b", Some("http://store.example.com")),
            ("s3://b", Some("ftp://127.0.0.1")),
        ] {
            assert!(S3Store::new(url, endpoint).is_err(), "{url} {endpoint:?}");
        }
        let store = S3Store::new("s3://b/p/", None).unwrap();
        assert_eq!(store.object("snap", "state.zst"), "p/snap/state.zst");
        assert_eq!(store.same_bucket("s3://b/p/catalog").unwrap(), "p/catalog");
        assert!(store.same_bucket("s3://bb/p").is_err());
        assert_eq!(
            S3Store::new("s3://b", None)
                .unwrap()
                .object("snap", "state.zst"),
            "snap/state.zst"
        );
    }

    #[test]
    fn listing_keeps_complete_snapshots_under_the_prefix() {
        let keys = [
            "ow/r1/a/manifest.json",
            "ow/r1/a/state.zst",
            "ow/r1/partial/memory.zst",
            "ow/r1/b/manifest.json",
            "ow/r1/x/y/manifest.json",
            "ow/r2/c/manifest.json",
        ];
        let keys = || keys.iter().map(|k| k.to_string());
        assert_eq!(snapshot_names(keys(), "ow/r1/"), ["a", "b"]);
        assert_eq!(
            snapshot_names(["s/manifest.json".to_string()].into_iter(), ""),
            ["s"]
        );
    }

    #[test]
    fn sparse_copy_keeps_bytes_and_holes() {
        use std::os::unix::fs::MetadataExt;
        let dir =
            std::env::temp_dir().join(format!("ow-store-{}", crate::common::nonce().unwrap()));
        fs::create_dir(&dir).unwrap();
        // 64 MiB, mostly zeros, with data at the start, middle and a short tail.
        let mut data = vec![0u8; 64 << 20];
        data[..5].copy_from_slice(b"hello");
        data[32 << 20] = 7;
        data.extend_from_slice(b"tail");
        let mut compressed = Vec::new();
        zstd::stream::copy_encode(&data[..], &mut compressed, 3).unwrap();
        assert!(compressed.len() < 1 << 20);
        let path = dir.join("disk");
        let mut file = private_file(&path).unwrap();
        let digest = write_sparse(zstd::Decoder::new(&compressed[..]).unwrap(), &mut file).unwrap();
        assert_eq!(digest, hex(&Sha256::digest(&data)));
        assert_eq!(fs::read(&path).unwrap(), data);
        assert!(
            fs::metadata(&path).unwrap().blocks() * 512 < 4 << 20,
            "not sparse"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn manifests_must_name_the_snapshot_and_hash_every_file() {
        let dir =
            std::env::temp_dir().join(format!("ow-manifest-{}", crate::common::nonce().unwrap()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("manifest.json");
        let h = "a".repeat(64);
        let good = serde_json::json!({"name":"s1","workspace":"box1","files":{"state":h,"memory":h,"disk.ext4":h}});
        fs::write(&path, good.to_string()).unwrap();
        assert!(read_manifest(&path, "s1").is_ok());
        assert!(read_manifest(&path, "s2").is_err());
        let mut bad = good.clone();
        bad["files"]["memory"] = "short".into();
        fs::write(&path, bad.to_string()).unwrap();
        assert!(read_manifest(&path, "s1").is_err());
        bad = good;
        bad["workspace"] = "../x".into();
        fs::write(&path, bad.to_string()).unwrap();
        assert!(read_manifest(&path, "s1").is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
