//! Chunk seals for managed recovery points. A file is fixed 8 MiB chunks and is
//! sealed under [`root`], the SHA-256 of its ordered chunk list, so a capture
//! does not have to read data the previous point already hashed: a chunk is
//! taken from the previous seal only when both files map it to the same
//! physical extents (Btrfs/XFS reflinks) and the previous file is still the one
//! that seal describes; there, holes are zero chunks. Everything else is read.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    path::Path,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

pub const CHUNK: usize = 8 << 20;
/// Manifest `digest` of files sealed by [`root`]; without it a file hash is its whole-file SHA-256.
pub const CHUNKS_V1: &str = "sha256-chunks-8m-v1";
/// The chunk lists of a chunk-sealed snapshot, beside its manifest.
pub const SIDECAR: &str = "chunks.json";
const MAX_CHUNKS: usize = 300_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chunk {
    pub hash: String,
    pub length: usize,
    pub zero: bool,
}

/// One file's chunks and the identity of the file they were taken from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sealed {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    pub chunks: Vec<Chunk>,
}

/// Where a seal's chunks came from: `read` chunks were hashed from disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub chunks: u64,
    pub read: u64,
    pub reused: u64,
    pub holes: u64,
    pub bytes_read: u64,
}

impl std::ops::AddAssign for Stats {
    fn add_assign(&mut self, other: Self) {
        self.chunks += other.chunks;
        self.read += other.read;
        self.reused += other.reused;
        self.holes += other.holes;
        self.bytes_read += other.bytes_read;
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The seal of a file: SHA-256 over its ordered chunk hashes and lengths.
pub fn root(chunks: &[Chunk]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"ow-chunks-v1\n");
    for chunk in chunks {
        hash.update(format!("{} {}\n", chunk.hash, chunk.length).as_bytes());
    }
    hex(&hash.finalize())
}

pub fn zero_hash(length: usize) -> String {
    static FULL: OnceLock<String> = OnceLock::new();
    let compute = || hex(&Sha256::digest(vec![0u8; length]));
    if length == CHUNK {
        FULL.get_or_init(compute).clone()
    } else {
        compute()
    }
}

/// Checks a chunk list: valid descriptors, full chunks except the last, zero flags true.
pub fn check(chunks: &[Chunk]) -> Result<u64> {
    ensure!(
        chunks.len() <= MAX_CHUNKS,
        "snapshot exceeds chunk index limit"
    );
    let mut size = 0u64;
    for (i, chunk) in chunks.iter().enumerate() {
        ensure!(
            crate::snapshot_store::valid_hash(&chunk.hash)
                && (1..=CHUNK).contains(&chunk.length)
                && (chunk.length == CHUNK || i + 1 == chunks.len())
                && (!chunk.zero || chunk.hash == zero_hash(chunk.length)),
            "invalid chunk descriptor"
        );
        size += chunk.length as u64;
    }
    Ok(size)
}

pub fn read_sidecar(directory: &Path) -> Result<BTreeMap<String, Sealed>> {
    let mut bytes = Vec::new();
    File::open(directory.join(SIDECAR))?
        .take(64 << 20)
        .read_to_end(&mut bytes)?;
    let sealed: BTreeMap<String, Sealed> =
        serde_json::from_slice(&bytes).context("invalid chunk sidecar")?;
    for seal in sealed.values() {
        ensure!(check(&seal.chunks)? == seal.size, "invalid chunk sidecar");
    }
    Ok(sealed)
}

/// Writes the sidecar into an unpublished snapshot directory, read-only.
pub fn write_sidecar(directory: &Path, sealed: &BTreeMap<String, Sealed>) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(directory.join(SIDECAR))?;
    file.write_all(&serde_json::to_vec(sealed)?)?;
    file.sync_all()?;
    Ok(())
}

/// Files a capture reflinks; RAM and device state are written whole every time.
const REFLINKED: [&str; 1] = ["disk.ext4"];

/// Seals `directory`'s `files`, reusing unchanged disk chunks of the snapshot at
/// `previous` (if it has a sidecar), and writes the sidecar. Returns the seals.
pub fn seal_directory(
    directory: &Path,
    files: &[&str],
    previous: Option<&Path>,
) -> Result<(BTreeMap<String, String>, Stats)> {
    let earlier = previous.and_then(|p| read_sidecar(p).ok().map(|s| (p, s)));
    let mut roots = BTreeMap::new();
    let mut sealed = BTreeMap::new();
    let mut stats = Stats::default();
    for file in files {
        let prior = earlier
            .as_ref()
            .filter(|_| REFLINKED.contains(file))
            .and_then(|(path, s)| Some((path.join(file), s.get(*file)?)));
        let (seal, counted) = seal(
            &directory.join(file),
            prior.as_ref().map(|(p, s)| (p.as_path(), *s)),
        )?;
        roots.insert((*file).to_owned(), root(&seal.chunks));
        sealed.insert((*file).to_owned(), seal);
        stats += counted;
    }
    write_sidecar(directory, &sealed)?;
    Ok((roots, stats))
}

/// Chunks `path`. With `previous` (an earlier seal and its file), chunks whose
/// extents are identical in both files are taken from that seal unread.
pub fn seal(path: &Path, previous: Option<(&Path, &Sealed)>) -> Result<(Sealed, Stats)> {
    seal_with(path, previous, &extents)
}

type Map = dyn Fn(&File, u64, u64) -> Option<Vec<Extent>> + Sync;

fn seal_with(
    path: &Path,
    previous: Option<(&Path, &Sealed)>,
    map: &Map,
) -> Result<(Sealed, Stats)> {
    let file = File::open(path)?;
    let before = file.metadata()?;
    let size = before.len();
    let count = size.div_ceil(CHUNK as u64) as usize;
    ensure!(count <= MAX_CHUNKS, "snapshot exceeds chunk index limit");
    let previous = previous.and_then(|(path, sealed)| {
        let file = File::open(path).ok()?;
        let metadata = file.metadata().ok()?;
        // Same filesystem, and still the exact file the earlier seal hashed.
        (metadata.dev() == before.dev()
            && identity(&metadata) == (sealed.device, sealed.inode, sealed.size, sealed.modified)
            && check(&sealed.chunks).ok() == Some(sealed.size))
        .then_some((file, sealed))
    });
    // Extents are only compared with an earlier seal. Pending writes are not in the
    // map yet (an overwritten range still shows its old shared extent): flush both.
    let mapped = match &previous {
        Some((earlier, _)) => {
            file.sync_all()?;
            earlier.sync_all()?;
            true
        }
        None => false,
    };
    let next = AtomicUsize::new(0);
    let chunks = Mutex::new(vec![None; count]);
    let stats = Mutex::new(Stats::default());
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, 8)
        .min(count.max(1));
    std::thread::scope(|scope| -> Result<()> {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| -> Result<()> {
                    let mut buffer = vec![0u8; CHUNK];
                    let mut counted = Stats::default();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= count {
                            break;
                        }
                        let offset = i as u64 * CHUNK as u64;
                        let length = (size - offset).min(CHUNK as u64) as usize;
                        let mapped = mapped.then(|| map(&file, offset, length as u64)).flatten();
                        let earlier = previous.as_ref().and_then(|(file, sealed)| {
                            let chunk = sealed.chunks.get(i)?;
                            (chunk.length == length
                                && mapped.is_some()
                                && map(file, offset, length as u64) == mapped)
                                .then_some(chunk)
                        });
                        let chunk = match (&mapped, earlier) {
                            (Some(extents), _) if extents.is_empty() => {
                                counted.holes += 1;
                                Chunk {
                                    hash: zero_hash(length),
                                    length,
                                    zero: true,
                                }
                            }
                            (_, Some(chunk)) => {
                                counted.reused += 1;
                                chunk.clone()
                            }
                            _ => {
                                let bytes = &mut buffer[..length];
                                file.read_exact_at(bytes, offset)?;
                                counted.read += 1;
                                counted.bytes_read += length as u64;
                                let zero = bytes.iter().all(|b| *b == 0);
                                Chunk {
                                    hash: if zero {
                                        zero_hash(length)
                                    } else {
                                        hex(&Sha256::digest(&*bytes))
                                    },
                                    length,
                                    zero,
                                }
                            }
                        };
                        chunks.lock().unwrap()[i] = Some(chunk);
                    }
                    *stats.lock().unwrap() += counted;
                    Ok(())
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("chunk hashing thread panicked")?;
        }
        Ok(())
    })?;
    let after = fs::metadata(path)?;
    ensure!(
        identity(&after) == identity(&before) && after.ino() == before.ino(),
        "{} changed while it was hashed",
        path.display()
    );
    let mut stats = stats.into_inner().unwrap();
    stats.chunks = count as u64;
    let (device, inode, size, modified) = identity(&before);
    Ok((
        Sealed {
            device,
            inode,
            size,
            modified,
            chunks: chunks.into_inner().unwrap().into_iter().flatten().collect(),
        },
        stats,
    ))
}

fn identity(metadata: &fs::Metadata) -> (u64, u64, u64, (i64, i64)) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        (metadata.mtime(), metadata.mtime_nsec()),
    )
}

/// A mapped range, relative to the start of the queried window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Extent {
    logical: u64,
    physical: u64,
    length: u64,
    flags: u32,
}

/// Physical extents of `file` in `[start, start + length)`, coalesced; empty is a
/// hole. None when the filesystem cannot say, or says something that cannot
/// prove equal bytes (unknown location, delayed, compressed, encrypted, inline).
#[cfg(target_os = "linux")]
fn extents(file: &File, start: u64, length: u64) -> Option<Vec<Extent>> {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Raw {
        logical: u64,
        physical: u64,
        length: u64,
        reserved64: [u64; 2],
        flags: u32,
        reserved: [u32; 3],
    }
    #[repr(C)]
    struct Request {
        start: u64,
        length: u64,
        flags: u32,
        mapped: u32,
        count: u32,
        reserved: u32,
        extents: [Raw; 64],
    }
    const FS_IOC_FIEMAP: u64 = 0xC020_660B;
    const LAST: u32 = 0x1;
    const UNTRUSTED: u32 = 0x2 | 0x4 | 0x8 | 0x80 | 0x100 | 0x200 | 0x400;
    // Sharing and merging describe bookkeeping, not where the bytes are.
    const BOOKKEEPING: u32 = LAST | 0x1000 | 0x2000;
    let end = start.checked_add(length)?;
    let mut cursor = start;
    let mut out: Vec<Extent> = Vec::new();
    while cursor < end {
        let mut request = Request {
            start: cursor,
            length: end - cursor,
            flags: 0,
            mapped: 0,
            count: 64,
            reserved: 0,
            extents: [Raw::default(); 64],
        };
        // SAFETY: `request` is a properly sized, writable `struct fiemap` with 64 extents.
        if unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP as _, &mut request) } != 0 {
            return None;
        }
        let mapped = &request.extents[..(request.mapped as usize).min(64)];
        for raw in mapped {
            if raw.flags & UNTRUSTED != 0 || raw.length == 0 || raw.physical == 0 {
                return None;
            }
            let from = raw.logical.max(start);
            let to = raw.logical.checked_add(raw.length)?.min(end);
            if from < to {
                let extent = Extent {
                    logical: from - start,
                    physical: raw.physical.checked_add(from - raw.logical)?,
                    length: to - from,
                    flags: raw.flags & !BOOKKEEPING,
                };
                // Files can split one physically contiguous run differently.
                match out.last_mut() {
                    Some(last)
                        if last.logical + last.length == extent.logical
                            && last.physical + last.length == extent.physical
                            && last.flags == extent.flags =>
                    {
                        last.length += extent.length
                    }
                    _ => out.push(extent),
                }
            }
            cursor = cursor.max(raw.logical + raw.length);
        }
        if mapped.len() < 64 || mapped.last().is_some_and(|raw| raw.flags & LAST != 0) {
            break;
        }
    }
    Some(out)
}

#[cfg(not(target_os = "linux"))]
fn extents(_: &File, _: u64, _: u64) -> Option<Vec<Extent>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ow-digest-{}", crate::common::nonce().unwrap()));
        fs::create_dir(&dir).unwrap();
        dir
    }

    /// A pretend filesystem: each 8 MiB window's "physical" address is its
    /// content hash, as if unchanged data stayed in shared extents.
    fn content_map(file: &File, start: u64, length: u64) -> Option<Vec<Extent>> {
        let mut bytes = vec![0; length as usize];
        file.read_exact_at(&mut bytes, start).ok()?;
        if bytes.iter().all(|b| *b == 0) {
            return Some(vec![]);
        }
        let physical = u64::from_le_bytes(Sha256::digest(&bytes)[..8].try_into().unwrap()) | 1;
        Some(vec![Extent {
            logical: 0,
            physical,
            length,
            flags: 0,
        }])
    }

    #[test]
    fn chunk_seals_reuse_only_proven_extents_and_match_full_reads() {
        let dir = temp();
        let mut data = vec![0u8; 5 * CHUNK + 123];
        data[1] = 1;
        data[2 * CHUNK + 7] = 2;
        data[4 * CHUNK] = 3;
        data[5 * CHUNK + 100] = 4;
        let old = dir.join("old");
        fs::write(&old, &data).unwrap();
        let (sealed, stats) = seal(&old, None).unwrap();
        assert_eq!(sealed.chunks.len(), 6);
        assert_eq!(check(&sealed.chunks).unwrap(), data.len() as u64);
        assert_eq!((stats.chunks, stats.reused), (6, 0));
        assert!(sealed.chunks[1].zero && sealed.chunks[3].zero);
        assert_eq!(sealed.chunks[1].hash, zero_hash(CHUNK));
        assert_eq!(sealed.chunks[5].length, 123);

        // A changed-block run: one chunk differs, the rest are proven by extents.
        data[2 * CHUNK + 7] = 9;
        let new = dir.join("new");
        fs::write(&new, &data).unwrap();
        let (changed, stats) = seal_with(&new, Some((&old, &sealed)), &content_map).unwrap();
        assert_eq!(
            (stats.read, stats.reused, stats.holes),
            (1, 3, 2),
            "{stats:?}"
        );
        assert_eq!(stats.bytes_read, CHUNK as u64);
        let (full, _) = seal(&new, None).unwrap();
        assert_eq!(
            changed.chunks, full.chunks,
            "reuse diverged from a full read"
        );
        assert_ne!(root(&changed.chunks), root(&sealed.chunks));

        // The earlier seal is refused once its file is not the one it hashed.
        fs::remove_file(&old).unwrap();
        fs::write(&old, &data).unwrap();
        let (_, stats) = seal_with(&new, Some((&old, &sealed)), &content_map).unwrap();
        assert_eq!(stats.reused, 0, "trusted a replaced previous file");
        // Unknown extents always read.
        let (_, stats) = seal_with(&new, Some((&new, &full)), &|_, _, _| None).unwrap();
        assert_eq!((stats.read, stats.reused), (6, 0));

        // Sidecars round-trip and are checked.
        let snapshot = dir.join("snapshot");
        fs::create_dir(&snapshot).unwrap();
        fs::write(snapshot.join("disk.ext4"), &data).unwrap();
        let (roots, _) = seal_directory(&snapshot, &["disk.ext4"], None).unwrap();
        assert_eq!(roots["disk.ext4"], root(&full.chunks));
        assert_eq!(
            read_sidecar(&snapshot).unwrap()["disk.ext4"].chunks,
            full.chunks
        );
        let mut bad = full.chunks.clone();
        bad[1].zero = false;
        bad[0].zero = true;
        assert!(check(&bad).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    /// On a reflink-capable filesystem (Btrfs/XFS), a clone with one changed
    /// chunk is read for that chunk only. Skipped elsewhere.
    #[test]
    #[cfg(target_os = "linux")]
    fn reflinked_clone_reads_only_changed_chunks() {
        let dir = std::env::var_os("OW_REFLINK_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("ow-reflink-{}", crate::common::nonce().unwrap()));
        fs::create_dir(&dir).unwrap();
        let old = dir.join("old");
        let mut data: Vec<u8> = (0..4 * CHUNK).map(|i| (i % 251) as u8 + 1).collect();
        fs::write(&old, &data).unwrap();
        let (sealed, _) = seal(&old, None).unwrap();
        let new = dir.join("new");
        let cloned = std::process::Command::new("cp")
            .args(["--reflink=always"])
            .arg(&old)
            .arg(&new)
            .status()
            .is_ok_and(|s| s.success());
        if !cloned {
            eprintln!("skipped: no reflinks in {}", dir.display());
            fs::remove_dir_all(dir).unwrap();
            return;
        }
        // Left unflushed on purpose: until writeback the map still shows the shared extent.
        data[CHUNK + 5] ^= 0xff;
        File::options()
            .write(true)
            .open(&new)
            .unwrap()
            .write_all_at(&data[CHUNK..CHUNK + 4096], CHUNK as u64)
            .unwrap();
        let (changed, stats) = seal(&new, Some((&old, &sealed))).unwrap();
        assert_eq!((stats.read, stats.reused), (1, 3), "{stats:?}");
        assert_eq!(changed.chunks, seal(&new, None).unwrap().0.chunks);
        fs::remove_dir_all(dir).unwrap();
    }
}
