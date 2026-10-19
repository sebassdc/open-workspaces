//! Off-host copies of sealed snapshots, for backup and for restoring a machine
//! onto a new runner.
//!
//! A snapshot is a directory with `state`, `memory`, `disk.ext4` and
//! `manifest.json` (which holds the SHA-256 of the other three). Stores keep the
//! data files zstd-compressed (sparse disks and idle memory shrink to their
//! used bytes) and write the manifest last, so a stored manifest means a
//! complete snapshot. Downloads are checked against the manifest's hashes and
//! written sparse.
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    process::{Command, Stdio},
};

pub const DATA_FILES: [&str; 3] = ["state", "memory", "disk.ext4"];

pub trait SnapshotStore {
    /// Names of the complete snapshots in the store.
    fn list(&self) -> Result<Vec<String>>;
    /// Uploads one sealed snapshot directory under `name`, manifest last.
    fn push(&self, name: &str, directory: &Path) -> Result<()>;
    /// The sealed manifest of `name`, without store transport metadata; unchecked bytes.
    fn manifest(&self, name: &str) -> Result<Vec<u8>>;
    /// Downloads `name` into `directory`, which must not exist yet. Every file is
    /// checked against the stored manifest; on any failure `directory` is removed.
    fn pull(&self, name: &str, directory: &Path) -> Result<()>;
}

/// S3 or an S3-compatible store (MinIO, R2), through the AWS CLI and its usual
/// credential chain.
pub struct S3Store {
    url: String,
    endpoint: Option<String>,
    incremental: bool,
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
        })
    }

    fn push_full(&self, name: &str, directory: &Path) -> Result<()> {
        crate::common::identifier(name)?;
        let manifest = read_manifest(&directory.join("manifest.json"), name)?;
        for file in DATA_FILES {
            let mut source = File::open(directory.join(file))?;
            let size = source.metadata()?.len();
            let mut child = self
                .aws()
                .args(["s3", "cp", "--only-show-errors", "-"])
                .arg(self.key(name, &format!("{file}.zst")))
                // Multipart part sizes for streams larger than 50 GB.
                .args(["--expected-size", &size.to_string()])
                .stdin(Stdio::piped())
                .spawn()
                .context("the AWS CLI (aws) is required for snapshot stores")?;
            let mut hasher = Sha256::new();
            let encoded = (|| -> Result<()> {
                let mut encoder = zstd::Encoder::new(child.stdin.take().unwrap(), 3)?;
                let mut buffer = vec![0; 1 << 20];
                loop {
                    let n = source.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buffer[..n]);
                    encoder.write_all(&buffer[..n])?;
                }
                encoder.finish()?;
                Ok(())
            })();
            let uploaded = finished(child, &format!("uploading {file}"));
            encoded?;
            uploaded?;
            // The bytes sent are the sealed bytes.
            ensure!(
                manifest["files"][file] == hex(&hasher.finalize()),
                "snapshot {name}: {file} does not match its manifest"
            );
        }
        let status = self
            .aws()
            .args(["s3", "cp", "--only-show-errors"])
            .arg(directory.join("manifest.json"))
            .arg(self.key(name, "manifest.json"))
            .status()?;
        ensure!(status.success(), "uploading manifest failed ({status})");
        Ok(())
    }

    /// Opt-in chunk format for managed recovery. Existing manual backups keep
    /// writing the original representation that older clients can restore.
    pub fn incremental(mut self) -> Self {
        self.incremental = true;
        self
    }

    pub fn identity(&self) -> String {
        hex(&Sha256::digest(
            format!("{}\n{}", self.url, self.endpoint.as_deref().unwrap_or("")).as_bytes(),
        ))
    }

    fn key(&self, name: &str, file: &str) -> String {
        format!("{}/{name}/{file}", self.url)
    }

    /// The AWS CLI with only the checked endpoint: it would otherwise also read
    /// AWS_ENDPOINT_URL* and a profile's `endpoint_url`, which are never checked.
    pub fn aws(&self) -> Command {
        let mut command = Command::new("aws");
        command.env("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "true");
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("AWS_ENDPOINT_URL") {
                command.env_remove(name);
            }
        }
        if let Some(endpoint) = &self.endpoint {
            command.args(["--endpoint-url", endpoint]);
        }
        command
    }

    fn get(&self, key: &str) -> Result<std::process::Child> {
        self.aws()
            .args(["s3", "cp", "--only-show-errors", key, "-"])
            .stdout(Stdio::piped())
            .spawn()
            .context("the AWS CLI (aws) is required for snapshot stores")
    }
}

fn finished(child: std::process::Child, what: &str) -> Result<()> {
    let output = child.wait_with_output()?;
    ensure!(output.status.success(), "{what} failed ({})", output.status);
    Ok(())
}

impl SnapshotStore for S3Store {
    fn list(&self) -> Result<Vec<String>> {
        let output = self
            .aws()
            .args(["s3", "ls", "--recursive", &format!("{}/", self.url)])
            .stderr(Stdio::inherit())
            .output()
            .context("the AWS CLI (aws) is required for snapshot stores")?;
        // `aws s3 ls` exits 1 for an empty prefix; other failures (credentials,
        // network) must not read as an empty store.
        if !output.status.success()
            && !(output.status.code() == Some(1) && output.stdout.is_empty())
        {
            bail!("listing {} failed ({})", self.url, output.status);
        }
        let prefix = self.url.split_once("s3://").unwrap().1;
        let prefix = prefix.split_once('/').map_or("", |(_, p)| p);
        Ok(parse_listing(
            &String::from_utf8_lossy(&output.stdout),
            prefix,
        ))
    }

    fn push(&self, name: &str, directory: &Path) -> Result<()> {
        if self.incremental {
            self.check_recovery_retention()?;
            self.push_chunks(name, directory)
        } else {
            self.push_full(name, directory)
        }
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
                for file in DATA_FILES {
                    let mut child = self.get(&self.key(name, &format!("{file}.zst")))?;
                    let decoded = (|| -> Result<String> {
                        let mut output = private_file(&directory.join(file))?;
                        let decoder = zstd::Decoder::new(child.stdout.take().unwrap())?;
                        write_sparse(decoder, &mut output)
                    })();
                    finished(child, &format!("downloading {file}"))?;
                    ensure!(
                        manifest["files"][file] == decoded?,
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

fn valid_hash(hash: &str) -> bool {
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Copies `reader` into `output`, seeking over all-zero 4 KiB blocks so the file
/// stays sparse. Returns the SHA-256 of all bytes.
pub fn write_sparse(mut reader: impl Read, output: &mut File) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut length = 0u64;
    let mut block = [0u8; 4096];
    loop {
        let mut filled = 0;
        while filled < block.len() {
            match reader.read(&mut block[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        if filled == 0 {
            break;
        }
        let data = &block[..filled];
        hasher.update(data);
        if data.iter().all(|b| *b == 0) {
            output.seek(SeekFrom::Current(filled as i64))?;
        } else {
            output.write_all(data)?;
        }
        length += filled as u64;
        if filled < block.len() {
            break;
        }
    }
    output.set_len(length)?;
    output.sync_all()?;
    Ok(hex(&hasher.finalize()))
}

/// Snapshot names with a stored manifest, from `aws s3 ls --recursive` output.
fn parse_listing(listing: &str, prefix: &str) -> Vec<String> {
    let prefix = if prefix.is_empty() {
        String::new()
    } else {
        format!("{}/", prefix.trim_end_matches('/'))
    };
    let mut names: Vec<String> = listing
        .lines()
        // "2026-10-16 10:00:00       1234 prefix/name/manifest.json"
        .filter_map(|line| line.split_whitespace().nth(3))
        .filter_map(|key| {
            key.strip_prefix(prefix.as_str())?
                .strip_suffix("/manifest.json")
        })
        .filter(|name| crate::common::identifier(name).is_ok())
        .map(str::to_owned)
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
        assert_eq!(store.key("snap", "state.zst"), "s3://b/p/snap/state.zst");
        let envs: Vec<_> = store
            .aws()
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned())))
            .collect();
        assert!(envs.contains(&(
            "AWS_IGNORE_CONFIGURED_ENDPOINT_URLS".into(),
            Some("true".into())
        )));
    }

    #[test]
    fn listing_keeps_complete_snapshots_under_the_prefix() {
        let listing = "2026-10-16 10:00:00   900 ow/r1/a/manifest.json\n\
            2026-10-16 10:00:00   900 ow/r1/a/state.zst\n\
            2026-10-16 10:00:00   900 ow/r1/partial/memory.zst\n\
            2026-10-16 10:00:00   900 ow/r1/b/manifest.json\n\
            2026-10-16 10:00:00   900 ow/r1/x/y/manifest.json\n\
            2026-10-16 10:00:00   900 ow/r2/c/manifest.json\n";
        assert_eq!(parse_listing(listing, "ow/r1"), ["a", "b"]);
        assert_eq!(
            parse_listing("2026-10-16 10:00:00 9 s/manifest.json\n", ""),
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
