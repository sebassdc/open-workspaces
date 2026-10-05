//! Bounded native-runtime entry point. Deliberately bypasses saved remote login
//! and Linux participation paths; shared node protocol changes need coordination.
use crate::MacHostAction;
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    os::unix::{fs::MetadataExt, process::CommandExt},
    path::PathBuf,
    process::Command,
};

pub fn run(root: Option<PathBuf>, action: MacHostAction) -> Result<i32> {
    ensure!(
        cfg!(target_arch = "aarch64"),
        "native Mac host requires Apple Silicon"
    );
    let helper = std::env::current_exe()?
        .parent()
        .context("CLI directory missing")?
        .join("ow-vz");
    let info = fs::symlink_metadata(&helper)
        .context("native helper missing; run scripts/build-mac-host.sh")?;
    ensure!(
        info.is_file() && info.uid() == unsafe { libc::geteuid() } && info.mode() & 0o022 == 0,
        "native helper must be an owned regular executable without group/world write access"
    );
    let mut process = Command::new(helper);
    process
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    match action {
        MacHostAction::Capabilities => {
            process.arg("capabilities");
        }
        MacHostAction::MacosProbe => {
            process.arg("macos-probe");
        }
        MacHostAction::Boot | MacHostAction::Restore { .. } => {
            let root =
                root.context("mac-host boot/restore requires --data-dir PRIVATE_FIXTURE_ROOT")?;
            let root = root
                .canonicalize()
                .context("prepared fixture root missing")?;
            let (verb, disk) = match action {
                MacHostAction::Restore { disk } => ("restore", disk.canonicalize()?),
                _ => ("boot", root.join("persist.img")),
            };
            process
                .arg(verb)
                .arg(root.join("Image"))
                .arg(root.join("initramfs-ow.gz"))
                .arg(disk);
        }
    }
    // Replace the CLI process so signals, console and exact exit status are the
    // helper's, with no orphaned intermediary supervisor.
    Err(process.exec()).context("launch native virtualization helper")
}
