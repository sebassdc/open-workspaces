//! Chunk seals for managed recovery points. A file is fixed-size chunks (32 MiB
//! from [`CHUNKS_V2`], 8 MiB in [`CHUNKS_V1`]) and is sealed under [`root`], the
//! SHA-256 of its ordered chunk list, so a capture
//! does not have to read data the previous point already hashed: a chunk is
//! taken from the previous seal only when both files map it to the same
//! physical extents (Btrfs/XFS reflinks) and the previous file is still the one
//! that seal describes; there, holes are zero chunks. Everything else is read.
use anyhow::{Context, Result, bail, ensure};
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

/// Legacy whole-file and v1 seal chunk size; new recovery clients use [`CHUNK_V2`].
pub const CHUNK: usize = 8 << 20;
/// Manifest `digest` of files sealed by [`root`] over 8 MiB chunks; without a
/// digest a file hash is its whole-file SHA-256.
pub const CHUNKS_V1: &str = "sha256-chunks-8m-v1";
pub const CHUNK_V2: usize = 32 << 20;
/// The same seal over 32 MiB chunks: four times fewer requests per byte.
pub const CHUNKS_V2: &str = "sha256-chunks-32m-v2";

/// The chunk size of a manifest `digest`. Points without one (whole-file hashes)
/// were stored in 8 MiB chunks.
pub fn chunk_size(digest: Option<&str>) -> Result<usize> {
    match digest {
        None | Some(CHUNKS_V1) => Ok(CHUNK),
        Some(CHUNKS_V2) => Ok(CHUNK_V2),
        Some(other) => bail!("unsupported snapshot digest {other}"),
    }
}
/// The chunk lists of a chunk-sealed snapshot, beside its manifest.
pub const SIDECAR: &str = "chunks.json";
/// Uploader seals in private staging, kept out of the bounded control message.
pub const UPLOAD_SEALS: &str = "upload-seals.json";
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
    static FULL: [OnceLock<String>; 2] = [OnceLock::new(), OnceLock::new()];
    let compute = || hex(&Sha256::digest(vec![0u8; length]));
    match length {
        CHUNK => FULL[0].get_or_init(compute).clone(),
        CHUNK_V2 => FULL[1].get_or_init(compute).clone(),
        _ => compute(),
    }
}

/// Checks a chunk list of `size` chunks: valid descriptors, full chunks except
/// the last, zero flags true.
pub fn check(chunks: &[Chunk], size: usize) -> Result<u64> {
    ensure!(
        chunks.len() <= MAX_CHUNKS,
        "snapshot exceeds chunk index limit"
    );
    let mut total = 0u64;
    for (i, chunk) in chunks.iter().enumerate() {
        ensure!(
            crate::snapshot_store::valid_hash(&chunk.hash)
                && (1..=size).contains(&chunk.length)
                && (chunk.length == size || i + 1 == chunks.len())
                && (!chunk.zero || chunk.hash == zero_hash(chunk.length)),
            "invalid chunk descriptor"
        );
        total += chunk.length as u64;
    }
    Ok(total)
}

/// The sidecar of a snapshot sealed in `size` chunks.
pub fn read_sidecar(directory: &Path, size: usize) -> Result<BTreeMap<String, Sealed>> {
    read_seals(&directory.join(SIDECAR), size)
}

/// Reads bounded uploader seals or a committed sidecar.
pub fn read_seals(path: &Path, size: usize) -> Result<BTreeMap<String, Sealed>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(64 << 20).read_to_end(&mut bytes)?;
    let sealed: BTreeMap<String, Sealed> =
        serde_json::from_slice(&bytes).context("invalid chunk sidecar")?;
    for seal in sealed.values() {
        ensure!(
            check(&seal.chunks, size)? == seal.size,
            "invalid chunk sidecar"
        );
    }
    Ok(sealed)
}

impl Sealed {
    /// Whether this seal, in `size` chunks, describes the file at `path` as it is
    /// now: the same inode, size and modification time, and a consistent list.
    pub fn describes(&self, path: &Path, size: usize) -> Result<bool> {
        let metadata = fs::metadata(path)?;
        Ok(
            identity(&metadata) == (self.device, self.inode, self.size, self.modified)
                && check(&self.chunks, size).ok() == Some(self.size),
        )
    }
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

/// Files a capture reflinks: the disk, and RAM, whose changed pages are laid over a
/// reflink of the previous image. Device state is written whole every time.
const REFLINKED: [&str; 2] = ["disk.ext4", "memory"];

/// Seals `directory`'s `files` in the chunks of `digest`, reusing unchanged
/// reflinked chunks of the snapshot at `previous` (if it has a sidecar of the same
/// chunk size), and writes the sidecar. Returns the roots.
pub fn seal_directory(
    directory: &Path,
    files: &[&str],
    previous: Option<&Path>,
    digest: &str,
) -> Result<(BTreeMap<String, String>, Stats)> {
    let (sealed, stats) = seal_files(directory, files, previous, digest, threads(), None)?;
    write_sidecar(directory, &sealed)?;
    Ok((roots(&sealed), stats))
}

/// The root of each file's chunk list.
pub fn roots(sealed: &BTreeMap<String, Sealed>) -> BTreeMap<String, String> {
    sealed
        .iter()
        .map(|(file, seal)| (file.clone(), root(&seal.chunks)))
        .collect()
}

/// Hashing threads by default: one per CPU, at most eight.
pub fn threads() -> usize {
    std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, 8)
}

/// Called on a reading thread with the hash and bytes of each chunk read from
/// disk that holds data.
pub type OnRead<'a> = &'a (dyn Fn(&str, &[u8]) -> Result<()> + Sync);

/// Seals `directory`'s `files` as [`seal_directory`] does, on `threads` threads,
/// passing each chunk read to `on_read`; writes nothing.
pub fn seal_files(
    directory: &Path,
    files: &[&str],
    previous: Option<&Path>,
    digest: &str,
    threads: usize,
    on_read: Option<OnRead>,
) -> Result<(BTreeMap<String, Sealed>, Stats)> {
    let size = chunk_size(Some(digest))?;
    ensure!(digest != CHUNKS_V1 || size == CHUNK, "invalid chunk format");
    let earlier = previous.and_then(|p| read_sidecar(p, size).ok().map(|s| (p, s)));
    let mut sealed = BTreeMap::new();
    let mut stats = Stats::default();
    for file in files {
        let prior = earlier
            .as_ref()
            .filter(|_| REFLINKED.contains(file))
            .and_then(|(path, s)| Some((path.join(file), s.get(*file)?)));
        let how = Sealing {
            size,
            holes: REFLINKED.contains(file),
            threads,
            on_read,
        };
        let (seal, counted) = seal(
            &directory.join(file),
            prior.as_ref().map(|(p, s)| (p.as_path(), *s)),
            &how,
        )?;
        sealed.insert((*file).to_owned(), seal);
        stats += counted;
    }
    Ok((sealed, stats))
}

/// How [`seal`] chunks and reads a file.
pub struct Sealing<'a> {
    pub size: usize,
    /// Chunks the filesystem maps to no extent are zero chunks, unread.
    pub holes: bool,
    pub threads: usize,
    pub on_read: Option<OnRead<'a>>,
}

impl Sealing<'_> {
    /// Every byte read, in chunks of `size`; nothing else.
    pub fn full(size: usize) -> Self {
        Sealing {
            size,
            holes: false,
            threads: threads(),
            on_read: None,
        }
    }
}

/// Chunks `path`. With `previous` (an earlier seal and its file), chunks whose
/// extents are identical in both files are taken from that seal unread.
pub fn seal(
    path: &Path,
    previous: Option<(&Path, &Sealed)>,
    how: &Sealing,
) -> Result<(Sealed, Stats)> {
    if File::open(path).is_ok_and(|f| reflink_filesystem(&f)) {
        seal_with(path, previous, how, &extents)
    } else {
        seal_with(path, previous, how, &|_, _, _| None)
    }
}

/// Btrfs and XFS: filesystems whose extent maps are trusted to prove equal bytes.
#[cfg(target_os = "linux")]
fn reflink_filesystem(file: &File) -> bool {
    use std::os::fd::AsRawFd;
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `fs` is a writable statfs buffer for this open descriptor.
    let known = unsafe { libc::fstatfs(file.as_raw_fd(), &mut fs) } == 0;
    known && matches!(fs.f_type as u64, 0x9123_683E | 0x5846_5342)
}

#[cfg(not(target_os = "linux"))]
fn reflink_filesystem(_: &File) -> bool {
    false
}

type Map = dyn Fn(&File, u64, u64) -> Option<Vec<Extent>> + Sync;

fn seal_with(
    path: &Path,
    previous: Option<(&Path, &Sealed)>,
    how: &Sealing,
    map: &Map,
) -> Result<(Sealed, Stats)> {
    let (holes, step) = (how.holes, how.size);
    let file = File::open(path)?;
    let before = file.metadata()?;
    let size = before.len();
    let count = size.div_ceil(step as u64) as usize;
    ensure!(count <= MAX_CHUNKS, "snapshot exceeds chunk index limit");
    let previous = previous.and_then(|(path, sealed)| {
        let file = File::open(path).ok()?;
        let metadata = file.metadata().ok()?;
        // Same filesystem, and still the exact file the earlier seal hashed.
        (metadata.dev() == before.dev()
            && identity(&metadata) == (sealed.device, sealed.inode, sealed.size, sealed.modified)
            && check(&sealed.chunks, step).ok() == Some(sealed.size))
        .then_some((file, sealed))
    });
    // Extents are compared with an earlier seal, or read for holes. Pending writes
    // are not in the map yet (an overwritten range still shows its old shared
    // extent, a filled hole may still show none): flush first.
    let mapped = match &previous {
        Some((earlier, _)) => {
            file.sync_all()?;
            earlier.sync_all()?;
            true
        }
        None if holes => {
            file.sync_all()?;
            true
        }
        None => false,
    };
    let next = AtomicUsize::new(0);
    let chunks = Mutex::new(vec![None; count]);
    let stats = Mutex::new(Stats::default());
    let failed = std::sync::atomic::AtomicBool::new(false);
    let threads = how.threads.clamp(1, 128).min(count.max(1));
    std::thread::scope(|scope| -> Result<()> {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| -> Result<()> {
                    let mut buffer = vec![0u8; step];
                    let mut counted = Stats::default();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= count || failed.load(Ordering::Relaxed) {
                            break;
                        }
                        let offset = i as u64 * step as u64;
                        let length = (size - offset).min(step as u64) as usize;
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
                                let hash = if zero {
                                    zero_hash(length)
                                } else {
                                    hex(&Sha256::digest(&*bytes))
                                };
                                if let (false, Some(on_read)) = (zero, how.on_read)
                                    && let Err(error) = on_read(&hash, bytes)
                                {
                                    failed.store(true, Ordering::Relaxed);
                                    return Err(error);
                                }
                                Chunk { hash, length, zero }
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

    fn how(holes: bool) -> Sealing<'static> {
        Sealing {
            size: CHUNK,
            holes,
            threads: threads(),
            on_read: None,
        }
    }

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
        let (sealed, stats) = seal(&old, None, &how(false)).unwrap();
        assert_eq!(sealed.chunks.len(), 6);
        assert_eq!(check(&sealed.chunks, CHUNK).unwrap(), data.len() as u64);
        assert_eq!((stats.chunks, stats.reused), (6, 0));
        assert!(sealed.chunks[1].zero && sealed.chunks[3].zero);
        assert_eq!(sealed.chunks[1].hash, zero_hash(CHUNK));
        assert_eq!(sealed.chunks[5].length, 123);

        // A changed-block run: one chunk differs, the rest are proven by extents.
        data[2 * CHUNK + 7] = 9;
        let new = dir.join("new");
        fs::write(&new, &data).unwrap();
        let (changed, stats) =
            seal_with(&new, Some((&old, &sealed)), &how(false), &content_map).unwrap();
        assert_eq!(
            (stats.read, stats.reused, stats.holes),
            (1, 3, 2),
            "{stats:?}"
        );
        assert_eq!(stats.bytes_read, CHUNK as u64);
        let (full, _) = seal(&new, None, &how(false)).unwrap();
        // A first capture reads only mapped chunks: holes are zero chunks, unread.
        let (first, stats) = seal_with(&new, None, &how(true), &content_map).unwrap();
        assert_eq!((stats.read, stats.holes), (4, 2), "{stats:?}");
        assert_eq!(
            first.chunks, full.chunks,
            "hole map diverged from a full read"
        );
        let (_, stats) = seal_with(&new, None, &how(false), &content_map).unwrap();
        assert_eq!(
            (stats.read, stats.holes),
            (6, 0),
            "mapped holes without asking"
        );
        assert_eq!(
            changed.chunks, full.chunks,
            "reuse diverged from a full read"
        );
        assert_ne!(root(&changed.chunks), root(&sealed.chunks));

        // The earlier seal is refused once its file is not the one it hashed.
        fs::remove_file(&old).unwrap();
        fs::write(&old, &data).unwrap();
        let (_, stats) = seal_with(&new, Some((&old, &sealed)), &how(false), &content_map).unwrap();
        assert_eq!(stats.reused, 0, "trusted a replaced previous file");
        // Unknown extents always read.
        let (_, stats) = seal_with(&new, Some((&new, &full)), &how(true), &|_, _, _| None).unwrap();
        assert_eq!((stats.read, stats.reused), (6, 0));

        // Sidecars round-trip and are checked.
        let snapshot = dir.join("snapshot");
        fs::create_dir(&snapshot).unwrap();
        fs::write(snapshot.join("disk.ext4"), &data).unwrap();
        let (roots, _) = seal_directory(&snapshot, &["disk.ext4"], None, CHUNKS_V1).unwrap();
        assert_eq!(roots["disk.ext4"], root(&full.chunks));
        assert_eq!(
            read_sidecar(&snapshot, CHUNK).unwrap()["disk.ext4"].chunks,
            full.chunks
        );
        let mut bad = full.chunks.clone();
        bad[1].zero = false;
        bad[0].zero = true;
        assert!(check(&bad, CHUNK).is_err());
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
        let (sealed, _) = seal(&old, None, &how(true)).unwrap();
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
        let (changed, stats) = seal(&new, Some((&old, &sealed)), &how(true)).unwrap();
        assert_eq!((stats.read, stats.reused), (1, 3), "{stats:?}");
        assert_eq!(
            changed.chunks,
            seal(&new, None, &how(false)).unwrap().0.chunks
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
