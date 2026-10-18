//! Sparse raw ext4 disks. Host tools run only before a cold boot, never on a live disk.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{FileExt, MetadataExt},
    path::Path,
    process::Command,
};

pub fn filesystem_usage(path: &Path) -> Result<(u64, u64)> {
    let file = File::open(path)?;
    let mut sb = [0u8; 1024];
    file.read_exact_at(&mut sb, 1024)?;
    let word = |offset| u32::from_le_bytes(sb[offset..offset + 4].try_into().unwrap());
    ensure!(
        u16::from_le_bytes(sb[56..58].try_into()?) == 0xef53,
        "root disk is not ext4"
    );
    let shift = word(24);
    ensure!(shift <= 6, "invalid ext4 block size");
    let block_size = 1024u64 << shift;
    let wide = word(96) & 0x80 != 0;
    let blocks = u64::from(word(4)) | if wide { u64::from(word(336)) << 32 } else { 0 };
    let free = u64::from(word(12)) | if wide { u64::from(word(344)) << 32 } else { 0 };
    let size = blocks
        .checked_mul(block_size)
        .context("invalid ext4 capacity")?;
    ensure!(
        free <= blocks && size <= file.metadata()?.len(),
        "invalid ext4 counters"
    );
    Ok((size, (blocks - free) * block_size))
}

pub fn summary(path: &Path) -> Result<Value> {
    let m = path.metadata()?;
    Ok(
        json!({"disk_bytes":m.len(), "disk_allocated_bytes":m.blocks().saturating_mul(512),
        "disk_used_bytes":filesystem_usage(path).ok().map(|(_, used)| used)}),
    )
}

pub fn extend(path: &Path, bytes: u64) -> Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    ensure!(
        bytes >= file.metadata()?.len(),
        "disk shrinking is not supported"
    );
    file.set_len(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub fn grow_filesystem(path: &Path) -> Result<()> {
    let (size, _) = filesystem_usage(path)?;
    let pending = path.with_extension("growing");
    if size == path.metadata()?.len() && !pending.exists() {
        return Ok(());
    }
    // Retry fsck after an interrupted resize even if it already wrote the new size.
    File::create(&pending)?.sync_all()?;
    let parent = File::open(path.parent().context("disk parent")?)?;
    parent.sync_all()?;
    let check = Command::new("e2fsck")
        .args(["-p", "-f"])
        .arg(path)
        .output()
        .context("e2fsck is required for disk growth")?;
    ensure!(
        matches!(check.status.code(), Some(0 | 1)),
        "ext4 check failed; disk retained: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    let grow = Command::new("resize2fs")
        .arg(path)
        .output()
        .context("resize2fs is required for disk growth")?;
    ensure!(
        grow.status.success(),
        "ext4 growth failed; disk retained: {}",
        String::from_utf8_lossy(&grow.stderr)
    );
    ensure!(
        filesystem_usage(path)?.0 == path.metadata()?.len(),
        "ext4 did not grow to the disk capacity"
    );
    File::open(path)?.sync_all()?;
    std::fs::remove_file(pending)?;
    parent.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{GIB, nonce};
    #[test]
    #[ignore = "requires Linux e2fsprogs (mkfs.ext4, debugfs, e2fsck, resize2fs)"]
    fn sparse_growth_preserves_files_and_is_idempotent() {
        let root = std::env::temp_dir().join(format!("ow-disk-{}", nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        let disk = root.join("disk.ext4");
        File::create(&disk)
            .unwrap()
            .set_len(64 * 1024 * 1024)
            .unwrap();
        assert!(
            Command::new("mkfs.ext4")
                .args(["-q", "-F"])
                .arg(&disk)
                .status()
                .unwrap()
                .success()
        );
        let content = root.join("hello");
        std::fs::write(&content, "persistent before growth\n").unwrap();
        assert!(
            Command::new("debugfs")
                .args(["-w", "-R"])
                .arg(format!("write {} /hello", content.display()))
                .arg(&disk)
                .status()
                .unwrap()
                .success()
        );
        let used = filesystem_usage(&disk).unwrap().1;
        assert!(used > 0);
        extend(&disk, GIB).unwrap();
        assert_eq!(filesystem_usage(&disk).unwrap().0, 64 * 1024 * 1024);
        grow_filesystem(&disk).unwrap();
        assert_eq!(filesystem_usage(&disk).unwrap().0, GIB);
        grow_filesystem(&disk).unwrap();
        let pending = disk.with_extension("growing");
        File::create(&pending).unwrap();
        grow_filesystem(&disk).unwrap();
        assert!(!pending.exists());
        assert!(extend(&disk, GIB / 2).is_err());
        assert_eq!(disk.metadata().unwrap().len(), GIB);
        let out = Command::new("debugfs")
            .args(["-R", "cat /hello"])
            .arg(&disk)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "persistent before growth\n"
        );
        let summary = summary(&disk).unwrap();
        assert_eq!(summary["disk_bytes"], GIB);
        assert!(summary["disk_allocated_bytes"].as_u64().unwrap() < GIB / 2);
        std::fs::write(&disk, b"invalid disk").unwrap();
        assert!(grow_filesystem(&disk).is_err());
        assert!(summary["disk_used_bytes"].as_u64().unwrap() >= used);
        std::fs::remove_dir_all(root).unwrap();
    }
}
