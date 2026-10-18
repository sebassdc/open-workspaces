//! Write-once sealed artifacts. Catalog references, never object listings, are authority.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

mod s3;
pub use s3::S3Objects;
const MAX_MANIFEST: u64 = 1024 * 1024;
const MAX_FILE: u64 = 16 * 1024 * 1024 * 1024 * 1024;

/// No list or delete: only controller GC may remove remote objects.
pub trait Objects {
    /// Create once; an existing key succeeds only if its bytes match exactly.
    fn put_once(&self, key: &str, source: &mut dyn Read) -> Result<()>;
    fn get(&self, key: &str) -> Result<Box<dyn Read>>;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub attempt: String,
    pub digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub kind: String,
    pub attempt: String,
    pub created_unix: u64,
    /// Image provenance or snapshot compatibility/subject; no local paths or secrets.
    pub metadata: Value,
    pub files: BTreeMap<String, StoredFile>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredFile {
    pub size: u64,
    pub allocated: u64,
    pub sha256: String,
    pub objects: Vec<Object>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Object {
    pub key: String,
    pub codec: String,
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn valid_digest(digest: &str) -> Result<()> {
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid SHA-256 digest"
    );
    Ok(())
}
pub fn attempt_prefix(attempt: &str) -> Result<String> {
    ensure!(
        attempt.len() == 32
            && attempt
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid upload token"
    );
    Ok(format!("u/{}/{attempt}/", &attempt[..2]))
}
pub fn token() -> Result<String> {
    let mut bytes = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn key_path(key: &str) -> Result<()> {
    let parts: Vec<_> = key.split('/').collect();
    ensure!(
        parts.len() == 4 && parts[0] == "u" && parts[1] == parts[2].get(..2).unwrap_or(""),
        "invalid store key"
    );
    attempt_prefix(parts[2])?;
    ensure!(
        matches!(
            parts[3],
            "manifest.json" | "state.zst" | "memory.zst" | "disk.ext4.zst"
        ),
        "invalid store object"
    );
    Ok(())
}
impl Manifest {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.format == 1, "unsupported artifact format");
        ensure!(
            matches!(
                self.kind.as_str(),
                "base" | "image" | "snapshot" | "capture"
            ),
            "invalid artifact kind"
        );
        let prefix = attempt_prefix(&self.attempt)?;
        ensure!(
            !self.files.is_empty() && self.files.len() <= 3,
            "invalid artifact file count"
        );
        for (name, file) in &self.files {
            ensure!(
                matches!(name.as_str(), "state" | "memory" | "disk.ext4"),
                "invalid artifact filename"
            );
            valid_digest(&file.sha256)?;
            ensure!(
                file.size <= MAX_FILE && file.allocated <= MAX_FILE,
                "artifact file too large"
            );
            ensure!(
                file.objects.len() == 1
                    && file.objects[0].codec == "zstd"
                    && file.objects[0].key == format!("{prefix}{name}.zst"),
                "object outside upload token or unsupported codec"
            );
        }
        ensure!(
            self.files.contains_key("disk.ext4"),
            "artifact needs a disk"
        );
        if matches!(self.kind.as_str(), "base" | "image") {
            ensure!(self.files.len() == 1, "images contain disk only");
        }
        Ok(())
    }
    pub fn bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        // Value maps are sorted (serde_json without preserve_order), including metadata.
        let bytes = serde_json::to_vec(&serde_json::to_value(self)?)?;
        ensure!(bytes.len() as u64 <= MAX_MANIFEST, "manifest too large");
        Ok(bytes)
    }
}

/// A sealed file's expected uncompressed digest is checked in the upload stream.
pub struct Source {
    pub path: PathBuf,
    pub sha256: String,
}
struct Hashing<'a> {
    input: File,
    hash: &'a mut Sha256,
    size: &'a mut u64,
}
impl Read for Hashing<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let n = self.input.read(bytes)?;
        self.hash.update(&bytes[..n]);
        *self.size += n as u64;
        Ok(n)
    }
}

/// Manifest last. Failed pushes leave unreferenced attempt objects for controller GC.
pub fn push(
    store: &dyn Objects,
    attempt: &str,
    kind: &str,
    metadata: Value,
    sources: &BTreeMap<String, Source>,
    created_unix: u64,
) -> Result<Reference> {
    let prefix = attempt_prefix(attempt)?;
    let mut manifest = Manifest {
        format: 1,
        kind: kind.into(),
        attempt: attempt.into(),
        created_unix,
        metadata,
        files: BTreeMap::new(),
    };
    for (name, source) in sources {
        valid_digest(&source.sha256)?;
        let meta = fs::symlink_metadata(&source.path)?;
        ensure!(
            meta.is_file() && meta.len() <= MAX_FILE,
            "source must be a sealed regular file"
        );
        manifest.files.insert(
            name.clone(),
            StoredFile {
                size: meta.len(),
                allocated: meta.blocks().saturating_mul(512),
                sha256: source.sha256.clone(),
                objects: vec![Object {
                    key: format!("{prefix}{name}.zst"),
                    codec: "zstd".into(),
                }],
            },
        );
    }
    manifest.bytes()?; // Validate all names/keys/metadata before any remote write.
    for (name, source) in sources {
        let input = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&source.path)?;
        let before = input.metadata()?;
        let mut hash = Sha256::new();
        let mut size = 0;
        let reader = Hashing {
            input: input.try_clone()?,
            hash: &mut hash,
            size: &mut size,
        };
        let mut encoded = zstd::stream::read::Encoder::new(reader, 3)?;
        store.put_once(&manifest.files[name].objects[0].key, &mut encoded)?;
        drop(encoded);
        let after = input.metadata()?;
        ensure!(
            before.dev() == after.dev()
                && before.ino() == after.ino()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec()
                && before.ctime() == after.ctime()
                && before.ctime_nsec() == after.ctime_nsec()
                && before.len() == after.len(),
            "sealed source changed while uploading"
        );
        ensure!(
            size == manifest.files[name].size && format!("{:x}", hash.finalize()) == source.sha256,
            "sealed file hash mismatch: {name}"
        );
    }
    let bytes = manifest.bytes()?;
    store.put_once(&format!("{prefix}manifest.json"), &mut bytes.as_slice())?;
    let reference = Reference {
        attempt: attempt.into(),
        digest: sha256(&bytes),
    };
    read_manifest(store, &reference)?;
    Ok(reference)
}

pub fn read_manifest(store: &dyn Objects, reference: &Reference) -> Result<Manifest> {
    valid_digest(&reference.digest)?;
    let prefix = attempt_prefix(&reference.attempt)?;
    let mut bytes = Vec::new();
    store
        .get(&format!("{prefix}manifest.json"))?
        .take(MAX_MANIFEST + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_MANIFEST && sha256(&bytes) == reference.digest,
        "manifest digest mismatch"
    );
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    manifest.validate()?;
    ensure!(
        manifest.attempt == reference.attempt,
        "manifest token mismatch"
    );
    Ok(manifest)
}

/// Private new directory only. No partial image is installed or advertised on error.
pub fn pull(store: &dyn Objects, reference: &Reference, directory: &Path) -> Result<Manifest> {
    let manifest = read_manifest(store, reference)?;
    fs::DirBuilder::new().mode(0o700).create(directory)?;
    let result = (|| {
        for (name, file) in &manifest.files {
            let mut output = private_file(&directory.join(name))?;
            let input = store.get(&file.objects[0].key)?;
            let mut decoder = zstd::Decoder::new(input)?;
            decoder.window_log_max(26)?;
            // Never let a hostile compressed object expand beyond its pinned logical size.
            let mut bounded = decoder.take(file.size + 1);
            let hash = crate::snapshot_store::write_sparse(&mut bounded, &mut output)?;
            ensure!(
                output.metadata()?.len() == file.size && hash == file.sha256,
                "downloaded file mismatch: {name}"
            );
            fs::set_permissions(directory.join(name), fs::Permissions::from_mode(0o400))?;
        }
        private_file(&directory.join("manifest.json"))?.write_all(&manifest.bytes()?)?;
        File::open(directory.join("manifest.json"))?.sync_all()?;
        File::open(directory)?.sync_all()?;
        Ok(manifest)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(directory);
    }
    result
}
use std::os::unix::fs::DirBuilderExt;
pub(super) fn private_file(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}

/// Local fixture/backend with the same immutable-object contract, no deletion API.
pub struct DirectoryObjects(pub PathBuf);
impl Objects for DirectoryObjects {
    fn put_once(&self, key: &str, source: &mut dyn Read) -> Result<()> {
        key_path(key)?;
        let path = self.0.join(key);
        let parent = path.parent().context("object parent")?;
        fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".{}", token()?));
        let result = (|| {
            let mut file = private_file(&temporary)?;
            std::io::copy(source, &mut file)?;
            file.sync_all()?;
            match fs::hard_link(&temporary, &path) {
                Ok(()) => {
                    File::open(parent)?.sync_all()?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    ensure!(
                        hash_file(&temporary)? == hash_file(&path)?,
                        "write-once object mismatch"
                    );
                }
                Err(e) => return Err(e.into()),
            }
            Ok(())
        })();
        let _ = fs::remove_file(temporary);
        result
    }
    fn get(&self, key: &str) -> Result<Box<dyn Read>> {
        key_path(key)?;
        Ok(Box::new(File::open(self.0.join(key))?))
    }
}
pub fn hash_file(path: &Path) -> Result<String> {
    let mut input = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests;

/// An image selector is a local name or an immutable catalog version (`tools@3`).
pub fn image_name(selector: &str) -> Result<(&str, Option<i64>)> {
    let (name, version) = match selector.split_once('@') {
        Some((name, version)) => {
            let n = version.parse::<i64>().context("invalid image version")?;
            ensure!(n > 0 && n.to_string() == version, "invalid image version");
            (name, Some(n))
        }
        None => (selector, None),
    };
    crate::common::identifier(name)?;
    Ok((name, version))
}
