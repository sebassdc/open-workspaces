//! Guided trusted-host participation. Local authority, never human login.
use crate::{HostAction, common, nodes, wire};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, IsTerminal, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

// Fixed wire names and private runtime destinations. No extraction or arbitrary paths.
pub const FILES: [(&str, &str, u64, bool); 7] = [
    (
        "firecracker",
        "official/release-v1.17.0-x86_64/firecracker-v1.17.0-x86_64",
        32 * 1024 * 1024,
        true,
    ),
    (
        "vmlinux",
        "downloads/vmlinux-6.1.186",
        64 * 1024 * 1024,
        false,
    ),
    ("base.ext4", "guest/base.ext4", 256 * 1024 * 1024, false),
    ("ow-guest", "guest/ow-guest", 32 * 1024 * 1024, true),
    ("slirp4netns", "bin/slirp4netns", 32 * 1024 * 1024, true),
    (
        "ubuntu.ext4",
        "guest/ubuntu.ext4",
        8 * 1024 * 1024 * 1024,
        false,
    ),
    (
        "network-tools.tar.gz",
        "guest/network-tools.tar.gz",
        32 * 1024 * 1024,
        false,
    ),
];
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    runtime: String,
    arch: String,
    files: Vec<Asset>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Asset {
    name: String,
    size: u64,
    sha256: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    version: u32,
    controller: String,
    memory: u32,
    slots: u32,
    cpus: u32,
    storage_gib: u32,
    policy: String,
    #[serde(default)]
    ca_cert: Option<PathBuf>,
}

fn private_dir(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "host root must be an owned private directory, not a symlink"
    );
    Ok(())
}
fn root(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let path = match explicit {
        Some(path) => path,
        None => PathBuf::from(std::env::var_os("HOME").context("HOME required")?).join(".ow-host"),
    };
    ensure!(
        path.is_absolute() && path.as_os_str().len() < 60,
        "choose an absolute private host root shorter than 60 bytes with --data-dir"
    );
    if !path.exists() {
        fs::create_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    }
    private_dir(&path)?;
    let path = fs::canonicalize(path)?;
    ensure!(path.as_os_str().len() < 60, "canonical host root too long");
    Ok(path)
}
fn lock(root: &Path) -> Result<fs::File> {
    let path = root.join("host-lifecycle.lock");
    let f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    nodes::private(&path)?;
    ensure!(
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "another host lifecycle command is running"
    );
    Ok(f)
}
fn read_config(root: &Path) -> Result<Config> {
    private_dir(root)?;
    nodes::private(&root.join("host.json"))?;
    let c: Config = serde_json::from_slice(&bounded_file(&root.join("host.json"), 4096)?)?;
    ensure!(
        c.version == 1 && c.policy == nodes::POOL_POLICY,
        "unsupported host config/consent"
    );
    nodes::origin(&c.controller, false)?;
    limits(c.memory, c.slots, c.cpus, c.storage_gib)?;
    if let Some(path) = &c.ca_cert {
        ensure!(
            *path == root.join("controller-ca.pem"),
            "CA must be the saved private host-root trust file"
        );
        nodes::private(path)?;
        reqwest::Certificate::from_pem(&bounded_file(path, 65536)?)?;
    }
    Ok(c)
}
fn bounded_file(path: &Path, max: u64) -> Result<Vec<u8>> {
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_file() && m.len() <= max,
        "invalid or oversized regular file"
    );
    let mut f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut f)
        .take(max + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "file too large");
    Ok(bytes)
}
fn limits(memory: u32, slots: u32, cpus: u32, storage: u32) -> Result<()> {
    common::host_limits(memory, slots, cpus)?;
    ensure!(
        (1..=1024).contains(&storage),
        "storage reserve out of range"
    );
    Ok(())
}
fn fixed_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut c = Command::new(program);
    c.env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LANG", "C");
    c
}
fn command(root: &Path, c: &Config, action: &str) -> Result<Command> {
    let mut cmd = fixed_command(std::env::current_exe()?);
    cmd.env("OW_HOST_MANAGED", "1")
        .env(
            "OW_HOST_ASSET_HASH",
            format!(
                "{:x}",
                Sha256::digest(bounded_file(&root.join("assets/manifest.json"), 16384)?)
            ),
        )
        .env("OW_ASSET_DIR", root.join("assets"))
        .env("OW_MAX_MEMORY_MIB", c.memory.to_string())
        .env("OW_MAX_RUNNING", c.slots.to_string())
        .env("OW_MAX_VCPUS", c.cpus.to_string())
        .env("OW_MIN_FREE_GIB", c.storage_gib.to_string());
    cmd.arg("--local").arg("--data-dir").arg(root);
    if action == "run" {
        cmd.arg("host").arg("run");
    } else {
        cmd.arg(action);
    }
    Ok(cmd)
}
fn free_bytes(root: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let p = std::ffi::CString::new(root.as_os_str().as_bytes())?;
    let mut info = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    ensure!(
        unsafe { libc::statvfs(p.as_ptr(), info.as_mut_ptr()) } == 0,
        "cannot inspect storage capacity"
    );
    let info = unsafe { info.assume_init() };
    Ok(info.f_bavail * info.f_frsize)
}
fn mem_available() -> Result<u64> {
    let info = fs::read_to_string("/proc/meminfo")?;
    Ok(info
        .lines()
        .find(|l| l.starts_with("MemAvailable:"))
        .context("MemAvailable unavailable")?
        .split_whitespace()
        .nth(1)
        .context("MemAvailable value")?
        .parse::<u64>()?
        / 1024)
}
fn existing_demand() -> Result<Value> {
    // Current global process evidence. RSS includes actual committed guest pages;
    // MemAvailable includes other workers, page cache, desktop and helper demand.
    let mut count = 0u64;
    let mut rss = 0u64;
    for entry in fs::read_dir("/proc")? {
        let path = entry?.path();
        if !path
            .file_name()
            .unwrap()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        if let Ok(name) = fs::read_to_string(path.join("comm"))
            && name.trim().starts_with("firecracker")
        {
            count += 1;
            if let Ok(status) = fs::read_to_string(path.join("status")) {
                rss += status
                    .lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(0)
                    / 1024;
            }
        }
    }
    Ok(
        json!({"existing_vmm_count":count,"existing_vmm_rss_mib":rss,"host_available_mib":mem_available()?,"reservation_inventory":"RSS is not configured RAM. Unresolved/offline reservations require operator inventory; budgets must be disjoint."}),
    )
}
fn doctor(root: &Path) -> Result<Value> {
    let mut errors = Vec::new();
    if std::env::consts::ARCH != "x86_64" {
        errors.push("Linux x86-64 required".to_owned());
    }
    if fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_err()
    {
        errors.push("KVM unavailable: enable virtualization and obtain operator /dev/kvm access; no automatic privileged repair".to_owned());
    }
    for tool in ["ip", "nft", "unshare", "cp", "curl", "sha256sum"] {
        if !fixed_command("sh")
            .args(["-c", &format!("command -v {tool} >/dev/null")])
            .status()?
            .success()
        {
            errors.push(format!(
                "Install host tool {tool} through your OS package manager"
            ));
        }
    }
    if !fixed_command("unshare")
        .args(["--user", "--map-root-user", "--net", "true"])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        errors.push("Unprivileged user/network namespaces unavailable; ask the host administrator about supported policy".to_owned());
    }
    use std::os::unix::ffi::OsStrExt;
    let p = std::ffi::CString::new(root.as_os_str().as_bytes())?;
    let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(p.as_ptr(), info.as_mut_ptr()) } != 0
        || unsafe { info.assume_init() }.f_type != 0x9123683e
    {
        errors.push("Host root must be on Btrfs; choose an existing supported filesystem with --data-dir. No formatting or repartitioning is performed".to_owned());
    }
    let source = root.join(format!("doctor-{}", common::nonce()?));
    let target = source.with_extension("clone");
    let reflink = (|| -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&source)?;
        f.write_all(&[0u8; 4096])?;
        ensure!(
            fixed_command("cp")
                .arg("--reflink=always")
                .arg(&source)
                .arg(&target)
                .stderr(Stdio::null())
                .status()?
                .success(),
            "Btrfs reflink clone unavailable"
        );
        Ok(())
    })();
    let _ = fs::remove_file(source);
    let _ = fs::remove_file(target);
    if reflink.is_err() {
        errors.push(
            "Btrfs reflink probe failed; select a supported private data directory".to_owned(),
        );
    }
    Ok(
        json!({"cpu_logical_available":std::thread::available_parallelism()?.get(),"cpu_topology":fixed_command("lscpu").args(["--json"]).output().ok().filter(|o|o.status.success()).and_then(|o|serde_json::from_slice::<Value>(&o.stdout).ok()),"build_source_sha256":option_env!("OW_BUILD_SOURCE_SHA256"),"supported":errors.is_empty(),"diagnostics":errors,"free_gib":free_bytes(root)?/(1024*1024*1024),"demand":existing_demand()?}),
    )
}
fn preflight(root: &Path, c: &Config) -> Result<()> {
    let report = doctor(root)?;
    ensure!(
        report["supported"] == true,
        "unsupported host: {}",
        report["diagnostics"]
    );
    ensure!(
        mem_available()? > c.memory as u64 + 1024,
        "insufficient measured memory headroom; leave at least 1024 MiB beyond this disjoint worker budget and account for other workers/reservations"
    );
    ensure!(
        free_bytes(root)? > c.storage_gib as u64 * 1024 * 1024 * 1024 + 512 * 1024 * 1024,
        "insufficient disk headroom above selected minimum-free-space threshold"
    );
    Ok(())
}
fn input(prompt: &str, hidden: bool) -> Result<String> {
    ensure!(
        unsafe { libc::isatty(libc::STDIN_FILENO) } == 1,
        "guided input needs a terminal; use --invite-file and explicit capacity/consent flags for private non-interactive input"
    );
    eprint!("{prompt}");
    io::stderr().flush()?;
    static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    extern "C" fn interrupted(_: libc::c_int) {
        INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    struct Restore {
        terminal: libc::termios,
        signals: Vec<(i32, libc::sigaction)>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe {
                libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.terminal);
                for (signal, action) in &self.signals {
                    libc::sigaction(*signal, action, std::ptr::null_mut());
                }
            }
        }
    }
    let mut state = std::mem::MaybeUninit::<libc::termios>::uninit();
    ensure!(
        unsafe { libc::tcgetattr(libc::STDIN_FILENO, state.as_mut_ptr()) } == 0,
        "cannot configure terminal"
    );
    let old = unsafe { state.assume_init() };
    let mut restore = Restore {
        terminal: old,
        signals: Vec::new(),
    };
    let mut next = old;
    INTERRUPTED.store(false, std::sync::atomic::Ordering::SeqCst);
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
        let mut handler: libc::sigaction = unsafe { std::mem::zeroed() };
        handler.sa_sigaction = interrupted as *const () as usize;
        let mut previous = std::mem::MaybeUninit::<libc::sigaction>::uninit();
        ensure!(
            unsafe { libc::sigaction(signal, &handler, previous.as_mut_ptr()) } == 0,
            "cannot install cancellation handler"
        );
        restore
            .signals
            .push((signal, unsafe { previous.assume_init() }));
    }
    let _restore = restore;
    // Treat Ctrl-C as input so echo restoration happens before cancellation.
    next.c_lflag &= !(libc::ISIG | libc::ICANON);
    if hidden {
        next.c_lflag &= !libc::ECHO;
    }
    ensure!(
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &next) } == 0,
        "cannot configure input"
    );
    let mut bytes = Vec::new();
    loop {
        ensure!(
            !INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst),
            "join cancelled by signal"
        );
        let mut b = [0u8];
        let n = unsafe { libc::read(libc::STDIN_FILENO, b.as_mut_ptr().cast(), 1) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        if n == 0 || b[0] == 4 {
            bail!("input cancelled at EOF");
        }
        if b[0] == 3 {
            bail!("join cancelled");
        }
        if b[0] == b'\n' || b[0] == b'\r' {
            break;
        }
        if matches!(b[0], 8 | 127) {
            bytes.pop();
            continue;
        }
        ensure!(
            bytes.len() < 4096,
            "input too long; paste one invitation line"
        );
        bytes.push(b[0]);
    }
    // A pasted second line is never handed back to the shell. Discard queued input.
    unsafe {
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
    if hidden {
        eprintln!();
    }
    Ok(String::from_utf8(bytes)?.trim().to_owned())
}
fn choose(value: Option<u32>, label: &str, default: u32, max: u32, min: u32) -> Result<u32> {
    let v = if let Some(v) = value {
        v
    } else {
        let text = input(
            &format!("{label} [{default}, allowed {min}–{max}]: "),
            false,
        )?;
        if text.is_empty() {
            default
        } else {
            text.parse().context("enter a whole number")?
        }
    };
    ensure!(
        (min..=max).contains(&v),
        "selected {label} exceeds invitation/host bounds"
    );
    Ok(v)
}
fn envelope(v: &Value) -> Result<(String, u32, u32, u32)> {
    let node = v["node"].as_str().context("invitation node missing")?;
    common::identifier(node)?;
    ensure!(node != "local", "reserved node");
    ensure!(
        v["secret"]
            .as_str()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
        "invalid invitation secret"
    );
    ensure!(
        v["policy"] == nodes::POOL_POLICY,
        "missing or unsupported shared-pool policy"
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    ensure!(
        v["expires"]
            .as_u64()
            .is_some_and(|e| e > now && e <= now + 3600),
        "invitation expired or invalid expiry"
    );
    let controller = nodes::origin(
        v["controller"]
            .as_str()
            .context("self-contained invitation controller missing")?,
        false,
    )?;
    let memory = u32::try_from(v["memory"].as_u64().context("invitation RAM missing")?)?;
    let slots = u32::try_from(v["slots"].as_u64().context("invitation slots missing")?)?;
    let cpus = u32::try_from(v["cpus"].as_u64().context("invitation CPU cap missing")?)?;
    limits(memory, slots, cpus, 1)?;
    Ok((controller, memory, slots, cpus))
}
fn selected_files(
    m: &Manifest,
) -> impl Iterator<Item = (&'static str, &'static str, u64, bool)> + '_ {
    FILES
        .into_iter()
        .filter(|(name, _, _, _)| m.files.iter().any(|a| a.name == *name))
}
fn validate_manifest(m: &Manifest) -> Result<()> {
    ensure!(
        (m.version == 1 || m.version == 2)
            && m.runtime == "firecracker-v1.17.0"
            && m.arch == "x86_64",
        "unsupported runtime manifest"
    );
    ensure!(
        m.files.len() == 5 || (m.version == 2 && m.files.len() == 7),
        "unsupported asset set"
    );
    for (name, _, max, _) in FILES {
        let found = m
            .files
            .iter()
            .filter(|a| a.name == name)
            .collect::<Vec<_>>();
        if matches!(name, "ubuntu.ext4" | "network-tools.tar.gz") && found.is_empty() {
            continue;
        }
        ensure!(
            found.len() == 1
                && (!matches!(name, "ubuntu.ext4" | "network-tools.tar.gz") || m.version == 2),
            "manifest missing/duplicate asset"
        );
        let a = found[0];
        ensure!(
            a.size > 0
                && a.size <= max
                && a.sha256.len() == 64
                && a.sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid asset size/hash"
        );
    }
    ensure!(
        m.files.iter().any(|a| a.name == "ubuntu.ext4")
            == m.files.iter().any(|a| a.name == "network-tools.tar.gz"),
        "Ubuntu requires verified network tools"
    );
    ensure!(
        m.files
            .iter()
            .all(|a| FILES.iter().any(|(n, _, _, _)| *n == a.name)),
        "unknown asset"
    );
    Ok(())
}
fn verify_file(path: &Path, size: u64, expected: &str) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    ensure!(m.is_file() && m.len() == size, "invalid asset length/type");
    let mut f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let mut hash = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let n = f.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        ensure!(count <= size, "asset oversized");
        hash.update(&buffer[..n]);
    }
    ensure!(
        count == size && format!("{:x}", hash.finalize()) == expected,
        "runtime asset corrupt or incomplete"
    );
    Ok(())
}
pub(crate) fn verify_assets(root: &Path) -> Result<()> {
    verify_asset_dir(root, &root.join("assets"))
}
fn verify_asset_dir(root: &Path, assets: &Path) -> Result<()> {
    let m: Manifest = serde_json::from_slice(&bounded_file(&assets.join("manifest.json"), 16384)?)?;
    validate_manifest(&m)?;
    private_dir(assets)?;
    for (name, dest, _, executable) in selected_files(&m) {
        let a = m.files.iter().find(|a| a.name == name).unwrap();
        let path = assets.join(dest);
        // Reject symlink components even if the final file looks regular.
        let mut parent = path.parent();
        while let Some(p) = parent {
            if p == root {
                break;
            }
            ensure!(fs::symlink_metadata(p)?.is_dir(), "unsafe asset parent");
            parent = p.parent();
        }
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
            "unsafe asset ownership/permissions"
        );
        verify_file(&path, a.size, &a.sha256).with_context(|| format!("asset {name}"))?;
        ensure!(
            !executable || metadata.mode() & 0o100 != 0,
            "runtime executable permission missing"
        );
    }
    Ok(())
}
pub(crate) fn verified_images(root: &Path) -> Result<Vec<&'static str>> {
    verify_assets(root)?;
    let m: Manifest =
        serde_json::from_slice(&bounded_file(&root.join("assets/manifest.json"), 16384)?)?;
    let mut images = vec!["alpine"];
    if m.files.iter().any(|a| a.name == "ubuntu.ext4") {
        images.push("ubuntu");
    }
    Ok(images)
}
fn download_headroom(available: u64, transfer: u64, reserve_gib: u32) -> bool {
    transfer
        .checked_add(reserve_gib as u64 * 1024 * 1024 * 1024)
        .and_then(|n| n.checked_add(512 * 1024 * 1024))
        .is_some_and(|required| available > required)
}
// Presentation only: stderr failures must not change download integrity or stdout.
struct AssetProgress {
    tty: bool,
    total: u64,
    received: u64,
    started: Instant,
    refreshed: Instant,
    finished: bool,
}
impl AssetProgress {
    fn bytes(bytes: u64) -> String {
        if bytes >= 1024 * 1024 * 1024 {
            format!("{:.2} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
        } else {
            format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
        }
    }
    fn new(total: u64) -> Self {
        let now = Instant::now();
        let progress = Self {
            tty: io::stderr().is_terminal(),
            total,
            received: 0,
            started: now,
            refreshed: now,
            finished: false,
        };
        progress.line(&format!("Downloading assets: {total} bytes"));
        progress
    }
    fn line(&self, message: &str) {
        let mut stderr = io::stderr().lock();
        if self.tty {
            let _ = write!(stderr, "\r\x1b[2K");
        }
        let _ = writeln!(stderr, "{message}");
        let _ = stderr.flush();
    }
    fn render(&mut self, name: &str, force: bool) {
        if !self.tty || (!force && self.refreshed.elapsed() < Duration::from_millis(200)) {
            return;
        }
        self.refreshed = Instant::now();
        let percent = self.received * 100 / self.total;
        let filled = (percent / 10) as usize;
        let rate = self.received as f64 / self.started.elapsed().as_secs_f64().max(0.001);
        let mut stderr = io::stderr().lock();
        let mut display = format!(
            "Download [{}{}] {percent:3}% {}/{} {:.1} MiB/s {name}",
            "#".repeat(filled),
            "-".repeat(10 - filled),
            Self::bytes(self.received),
            Self::bytes(self.total),
            rate / (1024.0 * 1024.0),
        );
        // Keep the ASCII status on one physical row, including narrow terminals.
        let mut window: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(stderr.as_raw_fd(), libc::TIOCGWINSZ, &mut window) } == 0
            && window.ws_col > 0
        {
            display.truncate(usize::from(window.ws_col).saturating_sub(1));
        }
        let _ = write!(stderr, "\r\x1b[2K{display}");
        let _ = stderr.flush();
    }
    fn file(&mut self, name: &str, size: u64) {
        if self.tty {
            self.render(name, true);
        } else {
            self.line(&format!("Downloading {name}: {size} bytes"));
        }
    }
    fn advance(&mut self, name: &str, bytes: u64) {
        self.received += bytes;
        self.render(name, false);
    }
    fn complete(&mut self) {
        self.line("Assets verified and published.");
        self.finished = true;
    }
}
impl Drop for AssetProgress {
    fn drop(&mut self) {
        if !self.finished {
            self.line("Asset update failed; completion was not confirmed.");
        }
    }
}

fn downloads(
    root: &Path,
    controller: &str,
    ca: Option<&Path>,
    ubuntu: bool,
    refresh: bool,
    reserve_gib: u32,
) -> Result<()> {
    if root.join("assets").exists() {
        verify_assets(root)?;
        if !refresh {
            ensure!(
                !ubuntu || verified_images(root)?.contains(&"ubuntu"),
                "use host update-assets --ubuntu-dev to add Ubuntu to existing assets"
            );
            return Ok(());
        }
    }
    let mut u = reqwest::Url::parse(controller)?;
    u.set_path("");
    let origin = u.as_str().trim_end_matches('/');
    let mut builder = reqwest::blocking::Client::builder()
        .no_proxy()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(1800));
    if let Some(ca) = ca {
        builder = builder
            .add_root_certificate(reqwest::Certificate::from_pem(&bounded_file(ca, 65536)?)?);
    }
    let client = builder.build()?;
    let response = client
        .get(format!("{origin}/cli/host-manifest.json"))
        .send()?;
    ensure!(
        response.status().is_success(),
        "runtime manifest unavailable"
    );
    let mut bytes = Vec::new();
    response.take(16385).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 16384, "runtime manifest too large");
    let mut manifest: Manifest = serde_json::from_slice(&bytes)?;
    validate_manifest(&manifest)?;
    if ubuntu {
        ensure!(
            manifest.files.iter().any(|a| a.name == "ubuntu.ext4"),
            "selected bundle lacks Ubuntu dev"
        );
    } else {
        manifest
            .files
            .retain(|a| !matches!(a.name.as_str(), "ubuntu.ext4" | "network-tools.tar.gz"));
    }
    let bytes = serde_json::to_vec(&manifest)?;
    let transfer: u64 = manifest.files.iter().map(|a| a.size).sum();
    ensure!(
        download_headroom(free_bytes(root)?, transfer, reserve_gib),
        "insufficient space for bounded asset download"
    );
    println!(
        "Verified bundle selection: {} bytes to transfer; per-file TLS deadline 1800 seconds",
        transfer
    );
    let staging = root.join(format!("assets-stage-{}", common::nonce()?));
    fs::create_dir(&staging)?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
    let mut progress = AssetProgress::new(transfer);
    let result = (|| -> Result<()> {
        for (name, dest, _, executable) in selected_files(&manifest) {
            let a = manifest.files.iter().find(|a| a.name == name).unwrap();
            progress.file(name, a.size);
            let response = client.get(format!("{origin}/cli/host/{name}")).send()?;
            ensure!(
                response.status().is_success(),
                "runtime asset unavailable: {name}"
            );
            let path = staging.join(dest);
            fs::create_dir_all(path.parent().unwrap())?;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(if executable { 0o700 } else { 0o600 })
                .open(path)?;
            let mut reader = response.take(a.size + 1);
            let mut hash = Sha256::new();
            let mut size = 0u64;
            let mut buffer = [0u8; 65536];
            loop {
                let n = reader.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                ensure!(size <= a.size, "runtime asset oversized");
                hash.update(&buffer[..n]);
                // Preserve sparse zero ranges without weakening hash/length validation.
                if buffer[..n].iter().all(|b| *b == 0) {
                    use std::io::{Seek, SeekFrom};
                    f.seek(SeekFrom::Current(n as i64))?;
                } else {
                    f.write_all(&buffer[..n])?;
                }
                progress.advance(name, n as u64);
            }
            progress.render(name, true);
            progress.line(&format!("Verifying {name}"));
            f.set_len(size)?;
            ensure!(
                size == a.size && format!("{:x}", hash.finalize()) == a.sha256,
                "runtime asset corrupt/incomplete: {name}"
            );
            f.sync_all()?;
        }
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(staging.join("manifest.json"))?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        progress.line(&format!(
            "Download complete: 100%, {transfer} bytes, {:.1} MiB/s; verifying staged assets",
            transfer as f64
                / progress.started.elapsed().as_secs_f64().max(0.001)
                / (1024.0 * 1024.0)
        ));
        verify_asset_dir(root, &staging)?;
        progress.line("Publishing verified assets");
        publish_assets(root, &staging, |_| Ok(()))?;
        progress.complete();
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}
fn worker_matches(root: &Path, status: &Value, c: &Config) -> bool {
    status["max_memory_mib"] == c.memory
        && status["max_running"] == c.slots
        && status["max_vcpus"] == c.cpus
        && status["host_managed"] == true
        && status["min_free_gib"] == c.storage_gib
        && bounded_file(&root.join("assets/manifest.json"), 16384)
            .is_ok_and(|b| status["host_asset_hash"] == format!("{:x}", Sha256::digest(b)))
}
fn held_lock(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    nodes::private(path)?;
    let f = fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    ensure!(
        error.raw_os_error() == Some(libc::EWOULDBLOCK),
        "cannot determine process lock state"
    );
    Ok(true)
}
fn agent_running(root: &Path) -> Result<bool> {
    held_lock(&root.join("node-agent.lock"))
}
// Resolve only independently supervised non-runtime processes. Every other
// unreadable same-operator process remains UNKNOWN; PID/command alone is insufficient.
fn supervisor_properties(
    program: &str,
    args: &[&str],
) -> Result<std::collections::HashMap<String, String>> {
    let metadata = fs::metadata(program)?;
    ensure!(
        metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
        "untrusted supervisor tool"
    );
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if deadline.elapsed() > Duration::from_secs(2) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("supervisor identity query timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    ensure!(status.success(), "supervisor identity query failed");
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .context("supervisor output")?
        .take(4097)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4096, "supervisor output too large");
    let mut result = std::collections::HashMap::new();
    for line in std::str::from_utf8(&bytes)?.lines() {
        let (key, value) = line
            .split_once('=')
            .context("invalid supervisor property")?;
        ensure!(
            result.insert(key.to_owned(), value.to_owned()).is_none(),
            "duplicate supervisor property"
        );
    }
    Ok(result)
}
fn unrelated_supervisor(path: &Path) -> Result<bool> {
    let stat = fs::read_to_string(path.join("stat"))?;
    let fields: Vec<_> = stat
        .rsplit_once(')')
        .context("process stat")?
        .1
        .split_whitespace()
        .collect();
    let parent = *fields.get(1).context("process parent")?;
    let pid = path
        .file_name()
        .context("process PID")?
        .to_str()
        .context("PID encoding")?;
    let comm = fs::read_to_string(path.join("comm"))?;
    let cmd = fs::read(path.join("cmdline"))?;
    let group = fs::read_to_string(path.join("cgroup"))?;
    let Some(group) = group.trim().strip_prefix("0::") else {
        return Ok(false);
    };
    let manager_unit = format!("user@{}.service", unsafe { libc::geteuid() });
    let manager = supervisor_properties(
        "/usr/bin/systemctl",
        &[
            "--no-pager",
            "show",
            &manager_unit,
            "-p",
            "MainPID",
            "-p",
            "ControlGroup",
        ],
    )?;
    let manager_pid = manager.get("MainPID").context("manager PID")?;
    let manager_group = format!(
        "{}/init.scope",
        manager.get("ControlGroup").context("manager cgroup")?
    );
    if group == manager_group {
        if pid == manager_pid
            && parent == "1"
            && comm.trim() == "systemd"
            && cmd == b"/usr/lib/systemd/systemd\0--user\0"
        {
            return Ok(true);
        }
        if parent == manager_pid && comm.trim() == "(sd-pam)" && cmd == b"(sd-pam)\0" {
            return Ok(true);
        }
    }
    if comm.trim() != "tailscaled" || !cmd.starts_with(b"/usr/bin/tailscaled\0be-child\0ssh\0") {
        return Ok(false);
    }
    let tailscale = supervisor_properties(
        "/usr/bin/systemctl",
        &["--no-pager", "show", "tailscaled.service", "-p", "MainPID"],
    )?;
    if tailscale.get("MainPID").is_none_or(|p| p != parent)
        || fs::metadata(format!("/proc/{parent}"))?.uid() != 0
    {
        return Ok(false);
    }
    let Some(scope) = group.rsplit('/').next() else {
        return Ok(false);
    };
    let Some(session) = scope
        .strip_prefix("session-")
        .and_then(|s| s.strip_suffix(".scope"))
    else {
        return Ok(false);
    };
    common::identifier(session)?;
    let login = supervisor_properties(
        "/usr/bin/loginctl",
        &[
            "--no-pager",
            "show-session",
            session,
            "-p",
            "Leader",
            "-p",
            "Service",
            "-p",
            "Remote",
            "-p",
            "Scope",
            "-p",
            "User",
        ],
    )?;
    Ok(login.get("Leader").is_some_and(|v| v == pid)
        && login.get("Service").is_some_and(|v| v == "tailscaled")
        && login.get("Remote").is_some_and(|v| v == "yes")
        && login.get("Scope").is_some_and(|v| v == scope)
        && login
            .get("User")
            .is_some_and(|v| v == &unsafe { libc::geteuid() }.to_string()))
}
fn no_guest_processes(root: &Path) -> Result<bool> {
    for e in fs::read_dir("/proc")? {
        let path = e?.path();
        if !path
            .file_name()
            .unwrap()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        if metadata.uid() != unsafe { libc::geteuid() } {
            continue;
        }
        match fs::read_link(path.join("cwd")) {
            Ok(cwd) => {
                if cwd.starts_with(root.join("machines")) {
                    return Ok(false);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {
                ensure!(
                    unrelated_supervisor(&path).unwrap_or(false),
                    "same-operator process inventory unreadable; guest exit UNKNOWN"
                );
            }
        }
    }
    Ok(true)
}
fn transport_status(root: &Path) -> Value {
    bounded_file(&root.join("node-channel.json"), 4096)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or(json!({"state":"unknown"}))
}
fn ready(root: &Path) -> bool {
    let status = transport_status(root);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if status["state"] != "dispatchable"
        || !status["updated"]
            .as_u64()
            .is_some_and(|n| n <= now && n + 15 > now)
        || !agent_running(root).unwrap_or(false)
    {
        return false;
    }
    let live = (|| -> Result<bool> {
        nodes::private(&root.join("node-agent.lock"))?;
        let agent: Value =
            serde_json::from_slice(&bounded_file(&root.join("node-agent.lock"), 4096)?)?;
        ensure!(
            agent["instance"] == status["instance"] && agent["instance"].as_str().is_some(),
            "stale agent instance"
        );
        common::identifier(
            status["generation"]
                .as_str()
                .context("missing generation")?,
        )?;
        let pid = agent["pid"].as_u64().context("agent pid missing")?;
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let start = stat
            .rsplit_once(')')
            .context("stat")?
            .1
            .split_whitespace()
            .nth(19)
            .context("process start")?;
        ensure!(agent["start"] == start, "agent process identity changed");
        ensure!(
            fs::read_link(format!("/proc/{pid}/exe"))? == std::env::current_exe()?,
            "agent executable differs"
        );
        let credential: Value =
            serde_json::from_slice(&bounded_file(&root.join("node.json"), 4096)?)?;
        ensure!(
            status["node"] == credential["node"] && status["node"].as_str().is_some(),
            "stale node acknowledgement"
        );
        let c = read_config(root)?;
        let worker = wire::request(root, json!({"op":"status"}))?;
        Ok(worker_matches(root, &worker, &c))
    })();
    live.unwrap_or(false)
}
fn start(root: &Path, c: &Config) -> Result<()> {
    verify_assets(root)?;
    if let Ok(status) = wire::request(root, json!({"op":"status"})) {
        ensure!(
            worker_matches(root, &status, c),
            "existing worker does not match managed host assets/budgets; refusing reuse"
        );
        if agent_running(root)? {
            println!(
                "Host worker and agent already running with saved limits. Check host status for controller acknowledgement."
            );
            return Ok(());
        }
    }
    preflight(root, c)?;
    ensure!(
        command(root, c, "up")?
            .stdin(Stdio::null())
            .status()?
            .success(),
        "worker startup failed"
    );
    ensure!(
        worker_matches(root, &wire::request(root, json!({"op":"status"}))?, c),
        "worker effective assets/budgets differ after startup; refusing agent launch"
    );
    let log = root.join("host-agent.log");
    let f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&log)?;
    nodes::private(&log)?;
    let mut cmd = command(root, c, "run")?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(f.try_clone()?))
        .stderr(Stdio::from(f));
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // A prior acknowledgement cannot satisfy startup before the new instance exists.
    if root.join("node-channel.json").exists() {
        nodes::private(&root.join("node-channel.json"))?;
        fs::remove_file(root.join("node-channel.json"))?;
    }
    let mut child = cmd.spawn()?;
    for _ in 0..100 {
        if agent_running(root)? && ready(root) {
            println!(
                "Host online: controller acknowledged dispatchable participation with saved limits."
            );
            return Ok(());
        }
        if child.try_wait()?.is_some() {
            bail!(
                "agent startup failed; inspect private host-agent.log; worker may still be running"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!(
        "Host enrollment saved; participation PENDING: no controller dispatchable acknowledgement yet. Worker/agent may be running. Use ow host status or ow host stop."
    );
    Ok(())
}
fn clean_remnants(root: &Path) -> Result<()> {
    // Lifecycle lock held. Remove only exact nonce staging names owned by this wizard.
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .unwrap()
            .to_str()
            .context("unexpected root filename")?;
        let staging = name.strip_prefix("assets-stage-").is_some_and(nonce_name);
        let temporary = ["node.", "host.pending.", "host.", "marker-"]
            .iter()
            .any(|prefix| {
                name.strip_prefix(prefix)
                    .and_then(|n| n.strip_suffix(".tmp"))
                    .is_some_and(nonce_name)
            });
        if staging {
            private_dir(&path)?;
            fn check(tree: &Path, base: &Path) -> Result<()> {
                for entry in fs::read_dir(tree)? {
                    let p = entry?.path();
                    let m = fs::symlink_metadata(&p)?;
                    ensure!(
                        m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o022 == 0,
                        "unsafe staging owner/permissions"
                    );
                    let rel = p.strip_prefix(base)?;
                    ensure!(
                        FILES.iter().any(|(_, dest, _, _)| Path::new(dest) == rel
                            || Path::new(dest).starts_with(rel))
                            || rel == Path::new("manifest.json"),
                        "unrecognized staging content; preserve for inspection"
                    );
                    if m.is_dir() {
                        check(&p, base)?;
                    } else {
                        ensure!(m.is_file(), "unsafe staging link/type");
                    }
                }
                Ok(())
            }
            check(&path, &path)?;
            fs::remove_dir_all(path)?;
        } else if temporary {
            nodes::private(&path)?;
            fs::remove_file(path)?;
        }
    }
    Ok(())
}
fn nonce_name(value: &str) -> bool {
    value.len() == 24
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn publish_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let root = path.parent().context("parent required")?;
    let temporary = root.join(format!("marker-{}.tmp", common::nonce()?));
    let result = (|| -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::hard_link(&temporary, path)?;
        fs::File::open(root)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}
fn finish_enrollment(root: &Path) -> Result<Config> {
    nodes::private(&root.join("node.json"))?;
    let credential: Value = serde_json::from_slice(&bounded_file(&root.join("node.json"), 4096)?)?;
    common::identifier(credential["node"].as_str().context("node missing")?)?;
    ensure!(
        credential["credential"]
            .as_str()
            .is_some_and(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit())),
        "invalid saved credential"
    );
    if !root.join("host.json").exists() {
        nodes::private(&root.join("host.pending.json"))?;
        let pending: Value =
            serde_json::from_slice(&bounded_file(&root.join("host.pending.json"), 8192)?)?;
        ensure!(
            pending["node"] == credential["node"],
            "saved enrollment does not match pending config"
        );
        nodes::write_private(&root.join("host.json"), &pending["config"])?;
    }
    let c = read_config(root)?;
    verify_assets(root)?;
    let marker = root.join(".ow-data");
    if !marker.exists() {
        publish_bytes(&marker, b"open-workspaces local prototype v1\n")?;
    }
    ensure!(
        bounded_file(&marker, 128)? == b"open-workspaces local prototype v1\n",
        "unexpected data marker"
    );
    let _ = fs::remove_file(root.join("host.pending.json"));
    fs::File::open(root)?.sync_all()?;
    Ok(c)
}
fn sync_asset_tree(root: &Path, assets: &Path) -> Result<()> {
    verify_asset_dir(root, assets)?;
    let m: Manifest = serde_json::from_slice(&bounded_file(&assets.join("manifest.json"), 16384)?)?;
    let mut directories = std::collections::BTreeSet::new();
    directories.insert(assets.to_path_buf());
    for (_, dest, _, _) in selected_files(&m) {
        let path = assets.join(dest);
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?
            .sync_all()?;
        let mut p = path.parent();
        while let Some(dir) = p {
            directories.insert(dir.to_path_buf());
            if dir == assets {
                break;
            }
            p = dir.parent();
        }
    }
    fs::File::open(assets.join("manifest.json"))?.sync_all()?;
    // Children before parents, then publication syncs the host root.
    let mut directories = directories.into_iter().collect::<Vec<_>>();
    directories.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for directory in directories {
        fs::File::open(directory)?.sync_all()?;
    }
    Ok(())
}
// Journaled rename boundaries: before activation recover old, after activation verify new.
fn publish_assets(
    root: &Path,
    staging: &Path,
    mut boundary: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    sync_asset_tree(root, staging)?;
    if root.join("assets").exists() {
        let backup = format!("assets-rollback-{}", common::nonce()?);
        nodes::write_private(
            &root.join("asset-update.pending.json"),
            &json!({"backup":backup}),
        )?;
        boundary("intent")?;
        fs::rename(root.join("assets"), root.join(backup))?;
        fs::File::open(root)?.sync_all()?;
        boundary("old-renamed")?;
    }
    fs::rename(staging, root.join("assets"))?;
    boundary("new-renamed")?;
    fs::File::open(root)?.sync_all()?;
    boundary("synced")?;
    if root.join("asset-update.pending.json").exists() {
        fs::remove_file(root.join("asset-update.pending.json"))?;
        boundary("receipt-removed")?;
        fs::File::open(root)?.sync_all()?;
    }
    Ok(())
}
fn require_stopped(root: &Path) -> Result<()> {
    read_config(root)?;
    nodes::private(&root.join("node.json"))?;
    ensure!(
        !held_lock(&root.join("worker.lock"))?
            && !agent_running(root)?
            && !root.join("control.sock").exists()
            && no_guest_processes(root)?,
        "host must be positively stopped; no configuration signals sent"
    );
    ensure!(
        root.join("worker.lock").exists()
            || (!root.join("worker.log").exists() && !root.join("state.json").exists()),
        "missing worker ownership evidence"
    );
    if root.join("state.json").exists() {
        let state: Value =
            serde_json::from_slice(&bounded_file(&root.join("state.json"), 1024 * 1024)?)?;
        ensure!(
            state["workspaces"].as_object().is_some(),
            "unresolved worker journal"
        );
        // After a crash the journal may still claim running guests; require operator recovery first.
        ensure!(
            state["workspaces"]
                .as_object()
                .unwrap()
                .values()
                .all(|w| matches!(
                    w["state"].as_str(),
                    Some("stopped" | "hibernated" | "failed")
                )),
            "worker journal has unresolved running demand; recover explicitly before configure"
        );
    }
    Ok(())
}
fn recover_asset_update(root: &Path) -> Result<()> {
    let pending = root.join("asset-update.pending.json");
    if !pending.exists() {
        return Ok(());
    }
    require_stopped(root)?;
    nodes::private(&pending)?;
    let v: Value = serde_json::from_slice(&bounded_file(&pending, 4096)?)?;
    let backup = v["backup"].as_str().context("asset recovery backup")?;
    ensure!(
        backup
            .strip_prefix("assets-rollback-")
            .is_some_and(nonce_name),
        "invalid asset recovery path"
    );
    if !root.join("assets").exists() {
        private_dir(&root.join(backup))?;
        fs::rename(root.join(backup), root.join("assets"))?;
    }
    if verify_assets(root).is_err() {
        verify_asset_dir(root, &root.join(backup))?;
        fs::rename(
            root.join("assets"),
            root.join(format!("assets-rejected-{}", common::nonce()?)),
        )?;
        fs::rename(root.join(backup), root.join("assets"))?;
        verify_assets(root)?;
    }
    fs::File::open(root)?.sync_all()?;
    fs::remove_file(pending)?;
    fs::File::open(root)?.sync_all()?;
    Ok(())
}
fn configure(
    root: &Path,
    memory: u32,
    slots: u32,
    cpus: u32,
    storage: Option<u32>,
    consent: bool,
) -> Result<()> {
    require_stopped(root)?;
    ensure!(
        consent,
        "explicit --accept-shared-pool consent required; budgets must be disjoint"
    );
    let mut c = read_config(root)?;
    c.memory = memory;
    c.slots = slots;
    c.cpus = cpus;
    if let Some(storage) = storage {
        c.storage_gib = storage;
    }
    limits(c.memory, c.slots, c.cpus, c.storage_gib)?;
    preflight(root, &c)?;
    verify_assets(root)?;
    let staging = root.join(format!("config-{}.tmp", common::nonce()?));
    nodes::write_private(&staging, &serde_json::to_value(&c)?)?;
    // Retain exact prior consent/config as a private immutable rollback artifact.
    let backup = root.join(format!("host-config-{}.json", common::nonce()?));
    fs::hard_link(root.join("host.json"), &backup)?;
    fs::File::open(root)?.sync_all()?;
    fs::rename(staging, root.join("host.json"))?;
    fs::File::open(root)?.sync_all()?;
    println!(
        "Saved stopped-host budgets; credentials/disks retained. Owner ceilings still apply. Start explicitly when ready."
    );
    Ok(())
}
pub fn run(explicit: Option<PathBuf>, action: HostAction) -> Result<i32> {
    let root = root(explicit)?;
    if matches!(action, HostAction::Doctor) {
        let report = doctor(&root)?;
        println!("{report}");
        return Ok(if report["supported"] == true { 0 } else { 1 });
    }
    if matches!(action, HostAction::Run) {
        let c = read_config(&root)?;
        verify_assets(&root)?;
        nodes::agent(
            &root,
            &c.controller,
            &root.join("node.json"),
            None,
            c.ca_cert.as_deref(),
            false,
        )?;
        return Ok(0);
    }
    let _lock = lock(&root)?;
    recover_asset_update(&root)?;
    if matches!(action, HostAction::Join { .. } | HostAction::Start) {
        clean_remnants(&root)?;
    }
    match action {
        HostAction::Join {
            controller,
            invite_file,
            ca_cert,
            memory,
            slots,
            cpus,
            storage_gib,
            accept_shared_pool,
            no_start,
            ubuntu_dev,
        } => {
            if root.join("node.json").exists() {
                let c = finish_enrollment(&root)?;
                println!("Saved enrollment completed without redeeming another invitation.");
                if !no_start {
                    start(&root, &c)?;
                }
                return Ok(0);
            }
            ensure!(
                !root.join("host.json").exists(),
                "config without credentials: recovery required; do not overwrite enrollment"
            );
            // Never adopt a legacy worker/root or user files.
            for e in fs::read_dir(&root)? {
                let name = e?.file_name();
                ensure!(
                    name == "host-lifecycle.lock"
                        || name == "assets"
                        || name == "host.pending.json"
                        || name == "controller-ca.pem",
                    "join requires a new dedicated host root"
                );
            }
            let v: Value = if let Some(path) = invite_file {
                nodes::private(&path)?;
                private_dir(
                    path.parent()
                        .context("private invitation directory required")?,
                )?;
                serde_json::from_slice(&bounded_file(&path, 4096)?)?
            } else {
                serde_json::from_str(&input("Paste invitation (hidden), then Enter: ", true)?)
                    .context("invalid invitation JSON")?
            };
            let (included, cap_ram, cap_slots, cap_cpu) = envelope(&v)?;
            let origin = if let Some(controller) = controller {
                let c = nodes::origin(&controller, false)?;
                ensure!(
                    c == included,
                    "controller override must match invitation origin"
                );
                c
            } else {
                included
            };
            println!("{}\nController: {origin}", nodes::POOL_POLICY);
            if !accept_shared_pool {
                ensure!(
                    input("Participate in this shared pool? Type yes: ", false)? == "yes",
                    "join cancelled without redeeming invitation"
                );
            }
            let report = doctor(&root)?;
            println!("Host diagnostics: {report}");
            ensure!(
                report["supported"] == true,
                "resolve the diagnostics before joining; no privileged repair performed"
            );
            let available = mem_available()?.saturating_sub(1024).min(cap_ram as u64) as u32;
            ensure!(available >= 256, "not enough measured memory headroom");
            let c = Config {
                version: 1,
                controller: origin,
                memory: choose(
                    memory,
                    "Disjoint guest RAM budget (MiB; leave other workers/desktop reserve)",
                    512.min(available),
                    available,
                    256,
                )?,
                slots: choose(slots, "Running guest slots", 2.min(cap_slots), cap_slots, 1)?,
                cpus: choose(
                    cpus,
                    "Guest vCPU budget",
                    2.min(cap_cpu)
                        .min(std::thread::available_parallelism()?.get() as u32),
                    cap_cpu.min(std::thread::available_parallelism()?.get() as u32),
                    1,
                )?,
                storage_gib: choose(
                    storage_gib,
                    "Minimum free disk GiB to retain (preflight threshold, not a quota)",
                    2,
                    1024,
                    1,
                )?,
                policy: nodes::POOL_POLICY.to_owned(),
                ca_cert,
            };
            let mut c = c;
            preflight(&root, &c)?;
            if let Some(path) = c.ca_cert.take() {
                nodes::private(&path)?;
                let bytes = bounded_file(&path, 65536)?;
                reqwest::Certificate::from_pem(&bytes)?;
                let stored = root.join("controller-ca.pem");
                if stored.exists() {
                    nodes::private(&stored)?;
                    ensure!(
                        bounded_file(&stored, 65536)? == bytes,
                        "saved CA differs; preserve trust and use a new dedicated root"
                    );
                } else {
                    publish_bytes(&stored, &bytes)?;
                }
                c.ca_cert = Some(stored);
            }
            downloads(
                &root,
                &c.controller,
                c.ca_cert.as_deref(),
                ubuntu_dev,
                false,
                c.storage_gib,
            )?;
            let pending = root.join("host.pending.json");
            if pending.exists() {
                nodes::private(&pending)?;
                fs::remove_file(&pending)?;
            }
            nodes::write_private(&pending, &json!({"node":v["node"],"config":c}))?;
            let credentials = nodes::redeem(
                &c.controller,
                &json!({"node":v["node"],"secret":v["secret"]}),
                c.ca_cert.as_deref(),
            )?;
            nodes::write_private(&root.join("node.json"),&credentials).context("enrollment consumed but credential publication failed; ask owner to revoke and re-invite a fresh node ID")?;
            let c=finish_enrollment(&root).context("credentials saved: run host join again to finish locally without another redemption")?;
            println!(
                "Host enrolled; private credentials and limits saved. No human login was requested."
            );
            if !no_start {
                start(&root, &c)?;
            }
        }
        HostAction::Configure {
            memory,
            slots,
            cpus,
            storage_gib,
            accept_shared_pool,
        } => configure(&root, memory, slots, cpus, storage_gib, accept_shared_pool)?,
        HostAction::UpdateAssets { ubuntu_dev } => {
            require_stopped(&root)?;
            let c = read_config(&root)?;
            let ubuntu = ubuntu_dev || verified_images(&root)?.contains(&"ubuntu");
            downloads(
                &root,
                &c.controller,
                c.ca_cert.as_deref(),
                ubuntu,
                true,
                c.storage_gib,
            )?;
            verify_assets(&root)?;
            println!(
                "Runtime assets updated while stopped; immutable assets-rollback-* retained. Start explicitly."
            );
        }
        HostAction::RollbackAssets { revision } => {
            require_stopped(&root)?;
            ensure!(
                revision
                    .strip_prefix("assets-rollback-")
                    .is_some_and(nonce_name),
                "select an exact retained assets-rollback revision name"
            );
            let previous = root.join(revision);
            verify_asset_dir(&root, &previous)?;
            let staging = root.join(format!("assets-stage-{}", common::nonce()?));
            ensure!(
                fixed_command("cp")
                    .args(["-a", "--reflink=always", "--sparse=auto", "--"])
                    .arg(previous)
                    .arg(&staging)
                    .status()?
                    .success(),
                "rollback staging failed; existing assets retained"
            );
            verify_asset_dir(&root, &staging)?;
            publish_assets(&root, &staging, |_| Ok(()))?;
            println!(
                "Verified prior assets selected while stopped; current assets retained for rollback. Guest disks unchanged."
            );
        }
        HostAction::Start => {
            let c = finish_enrollment(&root)?;
            start(&root, &c)?;
        }
        HostAction::Status => {
            let c = read_config(&root)?;
            let worker = wire::request(&root, json!({"op":"status"})).ok();
            println!(
                "{}",
                json!({"worker":worker,"worker_matches_saved_limits":worker.as_ref().is_some_and(|s|worker_matches(&root,s,&c)),"agent_running":agent_running(&root)?,"node_channel":transport_status(&root),"controller_dispatchable":ready(&root),"controller_status":"Not inferred from local PID; owner pool state is authoritative. Offline/revoked channels do not prove guests stopped.","demand":existing_demand()?})
            );
        }
        HostAction::Stop => {
            let c = read_config(&root)?;
            let worker_lock = root.join("worker.lock");
            match wire::request(&root, json!({"op":"status"})) {
                Ok(status) => {
                    ensure!(
                        worker_matches(&root, &status, &c),
                        "refusing to stop an unrelated or mismatched worker"
                    );
                    ensure!(
                        held_lock(&worker_lock)?,
                        "worker ownership lock missing; stop state unknown"
                    );
                    wire::request(&root, json!({"op":"shutdown"}))?;
                }
                Err(_) => {
                    ensure!(
                        !root.join("control.sock").exists()
                            && !held_lock(&worker_lock)?
                            && !agent_running(&root)?
                            && no_guest_processes(&root)?,
                        "worker unavailable with unresolved process demand; stop state UNKNOWN, no signals sent"
                    );
                    ensure!(
                        worker_lock.exists()
                            || (!root.join("worker.log").exists()
                                && !root.join("state.json").exists()),
                        "worker ownership evidence missing; stop state UNKNOWN"
                    );
                }
            }
            for _ in 0..100 {
                if !held_lock(&worker_lock)? && !agent_running(&root)? && no_guest_processes(&root)?
                {
                    println!(
                        "Host participation stopped; owned worker/agent locks released and no guest process in this root; disks retained."
                    );
                    return Ok(0);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            bail!("stop pending; process exit unconfirmed; do not signal persisted PIDs");
        }
        _ => bail!("invalid local host action"),
    }
    Ok(0)
}

pub fn public_path(path: &str) -> bool {
    path == "/cli/host-manifest.json"
        || FILES
            .iter()
            .any(|(n, _, _, _)| path == format!("/cli/host/{n}"))
}
struct AssetBody {
    file: fs::File,
    remaining: u64,
}
impl http_body::Body for AssetBody {
    type Data = axum::body::Bytes;
    type Error = io::Error;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        if self.remaining == 0 {
            return std::task::Poll::Ready(None);
        }
        let mut buffer = vec![0u8; self.remaining.min(65536) as usize];
        match self.file.read(&mut buffer) {
            Ok(0) => std::task::Poll::Ready(Some(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "asset truncated",
            )))),
            Ok(n) => {
                self.remaining -= n as u64;
                buffer.truncate(n);
                std::task::Poll::Ready(Some(Ok(http_body::Frame::data(buffer.into()))))
            }
            Err(e) => std::task::Poll::Ready(Some(Err(e))),
        }
    }
    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(self.remaining)
    }
}
// Bounded cache for immutable selected bundle bytes. Every request opens a fresh FD
// and validates ownership/type/size. Rewrites, chmod, replacement and manifest digest
// changes invalidate via inode/stat/digest identity; no path-only trust or stale FD.
fn verified_public_file(file: &mut fs::File, size: u64, expected: &str) -> Result<()> {
    verified_public_file_before_lock(file, size, expected, || {})
}
fn verified_public_file_before_lock(
    file: &mut fs::File,
    size: u64,
    expected: &str,
    before_lock: impl FnOnce(),
) -> Result<()> {
    use std::collections::BTreeSet;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    fn identity(m: &fs::Metadata, expected: &str) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
            m.uid(),
            m.mode(),
            expected
        )
    }
    let before = file.metadata()?;
    ensure!(
        before.is_file()
            && before.len() == size
            && before.uid() == unsafe { libc::geteuid() }
            && before.mode() & 0o022 == 0,
        "unsafe opened published asset"
    );
    let key = identity(&before, expected);
    // Serialize first verification to avoid simultaneous large-image hash amplification.
    before_lock();
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(BTreeSet::new()))
        .lock()
        .map_err(|_| anyhow::anyhow!("asset verification cache"))?;
    ensure!(
        identity(&file.metadata()?, expected) == key,
        "published asset changed while queued for verification"
    );
    if !cache.contains(&key) {
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        let mut count = 0u64;
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            count += n as u64;
            ensure!(count <= size, "published asset grew");
            hash.update(&buffer[..n]);
        }
        ensure!(
            count == size && format!("{:x}", hash.finalize()) == expected,
            "published asset hash mismatch"
        );
        ensure!(
            identity(&file.metadata()?, expected) == key,
            "published asset changed during verification"
        );
        if cache.len() >= 32 {
            cache.clear();
        }
        cache.insert(key);
    }
    use std::io::Seek;
    file.rewind()?;
    Ok(())
}
pub async fn public_download(path: &str, head: bool) -> axum::response::Response {
    use axum::{body::Body, http::StatusCode, response::IntoResponse};
    let path = path.to_owned();
    let result = tokio::task::spawn_blocking(move || -> Result<(fs::File, u64)> {
        let root = std::env::var_os("OW_HOST_BUNDLE_DIR")
            .map(PathBuf::from)
            .unwrap_or(crate::assets().join("host-public"));
        ensure!(
            fs::symlink_metadata(&root)?.is_dir(),
            "unsafe bundle directory"
        );
        let bytes = bounded_file(&root.join("manifest.json"), 16384)?;
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        validate_manifest(&manifest)?;
        let (name, size, expected) = if path == "/cli/host-manifest.json" {
            ("manifest.json".to_owned(), bytes.len() as u64, None)
        } else {
            let name = path
                .strip_prefix("/cli/host/")
                .context("fixed asset path required")?;
            let a = manifest
                .files
                .iter()
                .find(|a| a.name == name)
                .context("fixed asset required")?;
            (a.name.clone(), a.size, Some(a.sha256.clone()))
        };
        let p = root.join(name);
        let m = fs::symlink_metadata(&p)?;
        ensure!(
            m.is_file()
                && m.len() == size
                && m.mode() & 0o022 == 0
                && m.uid() == unsafe { libc::geteuid() },
            "unsafe published asset"
        );
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(p)?;
        if let Some(expected) = expected {
            verified_public_file(&mut file, size, &expected)?;
        }
        Ok((file, size))
    })
    .await;
    match result {
        Ok(Ok((file, size))) => {
            let mut response = axum::response::Response::new(if head {
                Body::empty()
            } else {
                Body::new(AssetBody {
                    file,
                    remaining: size,
                })
            });
            response
                .headers_mut()
                .insert("content-type", "application/octet-stream".parse().unwrap());
            response
                .headers_mut()
                .insert("content-length", size.to_string().parse().unwrap());
            response
        }
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Host runtime bundle unavailable",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_caps_policy_origin_and_manifest() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let v = json!({"node":"friend","secret":"a".repeat(64),"expires":now+600,"controller":"https://example.test/_nodes","memory":512,"slots":2,"cpus":2,"policy":nodes::POOL_POLICY});
        assert!(envelope(&v).is_ok());
        for (key, value) in [
            ("policy", json!("private")),
            ("controller", json!("https://example.test/evil")),
            ("controller", json!("https://user:pass@example.test/_nodes")),
            ("expires", json!(now)),
            ("cpus", json!(65)),
            ("memory", json!(u64::MAX)),
        ] {
            let mut bad = v.clone();
            bad[key] = value;
            assert!(envelope(&bad).is_err());
        }
        let files = FILES
            .iter()
            .filter(|(name, _, _, _)| !matches!(*name, "ubuntu.ext4" | "network-tools.tar.gz"))
            .map(|(n, _, _, _)| json!({"name":n,"size":1,"sha256":"a".repeat(64)}))
            .collect::<Vec<_>>();
        let good =
            json!({"version":1,"runtime":"firecracker-v1.17.0","arch":"x86_64","files":files});
        assert!(validate_manifest(&serde_json::from_value(good.clone()).unwrap()).is_ok());
        let mut bad = good.clone();
        bad["files"][0]["name"] = json!("../etc/passwd");
        assert!(validate_manifest(&serde_json::from_value(bad).unwrap()).is_err());
        let mut bad = good;
        bad["files"][0]["size"] = json!(u64::MAX);
        assert!(validate_manifest(&serde_json::from_value(bad).unwrap()).is_err());
    }
    fn fixture() -> (PathBuf, Config) {
        let root = std::env::temp_dir().join(format!("ow-h-{}", common::nonce().unwrap()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(root.join("assets")).unwrap();
        fs::set_permissions(root.join("assets"), fs::Permissions::from_mode(0o700)).unwrap();
        let mut files = Vec::new();
        for (name, dest, _, executable) in FILES
            .into_iter()
            .filter(|(name, _, _, _)| !matches!(*name, "ubuntu.ext4" | "network-tools.tar.gz"))
        {
            let p = root.join("assets").join(dest);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, name.as_bytes()).unwrap();
            fs::set_permissions(
                &p,
                fs::Permissions::from_mode(if executable { 0o700 } else { 0o600 }),
            )
            .unwrap();
            files.push(json!({"name":name,"size":name.len(),"sha256":format!("{:x}",Sha256::digest(name.as_bytes()))}));
        }
        nodes::write_private(
            &root.join("assets/manifest.json"),
            &json!({"version":1,"runtime":"firecracker-v1.17.0","arch":"x86_64","files":files}),
        )
        .unwrap();
        let c = Config {
            version: 1,
            controller: "https://pool.example/_nodes".into(),
            memory: 512,
            slots: 2,
            cpus: 2,
            storage_gib: 2,
            policy: nodes::POOL_POLICY.into(),
            ca_cert: None,
        };
        (root, c)
    }
    #[test]
    fn queued_verified_cache_hit_rechecks_opened_file_identity() {
        let (root, _) = fixture();
        let path = root.join("queued-cache");
        fs::write(&path, b"good").unwrap();
        let expected = format!("{:x}", Sha256::digest(b"good"));
        verified_public_file(&mut fs::File::open(&path).unwrap(), 4, &expected).unwrap();
        let opened = fs::File::open(&path).unwrap();
        let (ready, waiting) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        let queued = std::thread::spawn(move || {
            let mut f = opened;
            verified_public_file_before_lock(&mut f, 4, &expected, || {
                ready.send(()).unwrap();
                resume.recv().unwrap();
            })
        });
        waiting.recv().unwrap();
        fs::write(&path, b"evil").unwrap();
        release.send(()).unwrap();
        assert!(queued.join().unwrap().is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn verified_public_cache_invalidates_same_length_rewrites_and_replacement() {
        let (root, _) = fixture();
        let path = root.join("cache-test");
        fs::write(&path, b"good").unwrap();
        let expected = format!("{:x}", Sha256::digest(b"good"));
        for _ in 0..2 {
            let mut f = fs::File::open(&path).unwrap();
            verified_public_file(&mut f, 4, &expected).unwrap();
        }
        fs::write(&path, b"evil").unwrap();
        assert!(verified_public_file(&mut fs::File::open(&path).unwrap(), 4, &expected).is_err());
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"good").unwrap();
        verified_public_file(&mut fs::File::open(&path).unwrap(), 4, &expected).unwrap();
        assert!(
            verified_public_file(&mut fs::File::open(&path).unwrap(), 4, &"f".repeat(64)).is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn downloaded_assets_retain_operator_disk_reserve() {
        let gib = 1024 * 1024 * 1024;
        assert!(!download_headroom(9 * gib, 8 * gib, 2));
        assert!(!download_headroom(10 * gib + gib / 2, 8 * gib, 2));
        assert!(download_headroom(11 * gib, 8 * gib, 2));
        assert!(!download_headroom(u64::MAX, u64::MAX, 2));
    }
    #[test]
    fn unlisted_images_are_never_managed_capabilities() {
        let (root, _) = fixture();
        for name in ["ubuntu.ext4", "arch.ext4"] {
            let path = root.join("assets/guest").join(name);
            fs::write(&path, b"unverified").unwrap();
            assert_eq!(verified_images(&root).unwrap(), vec!["alpine"]);
            fs::remove_file(&path).unwrap();
            fs::create_dir(&path).unwrap();
            assert_eq!(verified_images(&root).unwrap(), vec!["alpine"]);
            fs::remove_dir(&path).unwrap();
            std::os::unix::fs::symlink("/etc/passwd", &path).unwrap();
            assert_eq!(verified_images(&root).unwrap(), vec!["alpine"]);
            fs::remove_file(&path).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn asset_publication_recovers_each_exact_boundary_preserving_credentials() {
        for fail in [
            "intent",
            "old-renamed",
            "new-renamed",
            "synced",
            "receipt-removed",
        ] {
            let (root, c) = fixture();
            nodes::write_private(&root.join("host.json"), &serde_json::to_value(c).unwrap())
                .unwrap();
            nodes::write_private(
                &root.join("node.json"),
                &json!({"node":"friend","credential":"b".repeat(64)}),
            )
            .unwrap();
            let credentials = fs::read(root.join("node.json")).unwrap();
            let config = fs::read(root.join("host.json")).unwrap();
            require_stopped(&root).unwrap();
            let (other, _) = fixture();
            let stage = root.join(format!("assets-stage-{}", common::nonce().unwrap()));
            fs::rename(other.join("assets"), &stage).unwrap();
            let mut m: Manifest =
                serde_json::from_slice(&fs::read(stage.join("manifest.json")).unwrap()).unwrap();
            fs::write(stage.join("guest/base.ext4"), b"new").unwrap();
            let a = m.files.iter_mut().find(|a| a.name == "base.ext4").unwrap();
            a.size = 3;
            a.sha256 = format!("{:x}", Sha256::digest(b"new"));
            fs::write(stage.join("manifest.json"), serde_json::to_vec(&m).unwrap()).unwrap();
            assert!(
                publish_assets(&root, &stage, |point| {
                    if point == fail {
                        bail!("injected {point}")
                    }
                    Ok(())
                })
                .is_err()
            );
            recover_asset_update(&root).unwrap();
            verify_assets(&root).unwrap();
            let selected = fs::read(root.join("assets/guest/base.ext4")).unwrap();
            assert_eq!(
                selected,
                if matches!(fail, "intent" | "old-renamed") {
                    b"base.ext4".as_slice()
                } else {
                    b"new".as_slice()
                }
            );
            assert_eq!(credentials, fs::read(root.join("node.json")).unwrap());
            assert_eq!(config, fs::read(root.join("host.json")).unwrap());
            assert!(!root.join("asset-update.pending.json").exists());
            fs::remove_dir_all(root).unwrap();
            fs::remove_dir_all(other).unwrap();
        }
    }
    #[test]
    fn rollback_preserves_retained_revision_when_reflink_is_unavailable() {
        let (root, c) = fixture();
        nodes::write_private(&root.join("host.json"), &serde_json::to_value(c).unwrap()).unwrap();
        nodes::write_private(
            &root.join("node.json"),
            &json!({"node":"friend","credential":"b".repeat(64)}),
        )
        .unwrap();
        let (other, _) = fixture();
        let revision = format!("assets-rollback-{}", common::nonce().unwrap());
        fs::rename(other.join("assets"), root.join(&revision)).unwrap();
        fs::write(root.join("assets/guest/base.ext4"), b"corrupt").unwrap();
        let probe = root.join("reflink-probe");
        let reflink = fixed_command("cp")
            .args(["--reflink=always", "--"])
            .arg(root.join(&revision).join("guest/base.ext4"))
            .arg(&probe)
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        let result = run(
            Some(root.clone()),
            HostAction::RollbackAssets {
                revision: revision.clone(),
            },
        );
        if reflink {
            result.unwrap();
            verify_assets(&root).unwrap();
        } else {
            assert!(result.is_err());
            assert_eq!(
                fs::read(root.join("assets/guest/base.ext4")).unwrap(),
                b"corrupt"
            );
        }
        assert_eq!(
            fs::read(root.join(&revision).join("guest/base.ext4")).unwrap(),
            b"base.ext4"
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(other).unwrap();
    }
    #[test]
    fn corrupt_new_asset_publication_restores_verified_old_selection() {
        let (root, c) = fixture();
        nodes::write_private(&root.join("host.json"), &serde_json::to_value(c).unwrap()).unwrap();
        nodes::write_private(
            &root.join("node.json"),
            &json!({"node":"friend","credential":"b".repeat(64)}),
        )
        .unwrap();
        let (other, _) = fixture();
        let stage = root.join(format!("assets-stage-{}", common::nonce().unwrap()));
        fs::rename(other.join("assets"), &stage).unwrap();
        assert!(
            publish_assets(&root, &stage, |point| {
                if point == "new-renamed" {
                    bail!("injected")
                }
                Ok(())
            })
            .is_err()
        );
        fs::write(root.join("assets/guest/base.ext4"), b"corrupt").unwrap();
        recover_asset_update(&root).unwrap();
        verify_assets(&root).unwrap();
        assert_eq!(
            fs::read(root.join("assets/guest/base.ext4")).unwrap(),
            b"base.ext4"
        );
        assert!(fs::read_dir(&root).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_str()
                .unwrap()
                .starts_with("assets-rejected-")
        }));
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(other).unwrap();
    }
    #[test]
    fn optional_ubuntu_manifest_and_streamed_verification() {
        let (root, _) = fixture();
        let mut m: Manifest =
            serde_json::from_slice(&fs::read(root.join("assets/manifest.json")).unwrap()).unwrap();
        let path = root.join("assets/guest/ubuntu.ext4");
        let bytes = vec![0u8; 1024 * 1024];
        fs::write(&path, &bytes).unwrap();
        let hash = format!("{:x}", Sha256::digest(&bytes));
        m.files.push(Asset {
            name: "ubuntu.ext4".into(),
            size: bytes.len() as u64,
            sha256: hash,
        });
        assert!(validate_manifest(&m).is_err()); // legacy manifest cannot claim Ubuntu
        m.version = 2;
        assert!(validate_manifest(&m).is_err()); // image alone lacks runtime-required helper
        fs::write(root.join("assets/guest/network-tools.tar.gz"), b"tools").unwrap();
        m.files.push(Asset {
            name: "network-tools.tar.gz".into(),
            size: 5,
            sha256: format!("{:x}", Sha256::digest(b"tools")),
        });
        assert!(validate_manifest(&m).is_ok());
        fs::write(
            root.join("assets/manifest.json"),
            serde_json::to_vec(&m).unwrap(),
        )
        .unwrap();
        assert!(verify_assets(&root).is_ok());
        fs::write(&path, b"truncated").unwrap();
        assert!(verify_assets(&root).is_err());
        m.files.push(Asset {
            name: "ubuntu.ext4".into(),
            size: 1,
            sha256: "a".repeat(64),
        });
        assert!(validate_manifest(&m).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn stopped_configure_refuses_active_unknown_and_unconsented_states() {
        let (root, c) = fixture();
        nodes::write_private(&root.join("host.json"), &serde_json::to_value(c).unwrap()).unwrap();
        nodes::write_private(
            &root.join("node.json"),
            &json!({"node":"friend","credential":"b".repeat(64)}),
        )
        .unwrap();
        let before = fs::read(root.join("host.json")).unwrap();
        assert!(configure(&root, 4096, 2, 2, None, false).is_err());
        let listener = std::os::unix::net::UnixListener::bind(root.join("control.sock")).unwrap();
        assert!(require_stopped(&root).is_err());
        drop(listener);
        fs::remove_file(root.join("control.sock")).unwrap();
        fs::write(
            root.join("state.json"),
            json!({"workspaces":{"guest":{"state":"running","memory_mib":4096}}}).to_string(),
        )
        .unwrap();
        assert!(require_stopped(&root).is_err());
        assert_eq!(before, fs::read(root.join("host.json")).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn persisted_credential_recovers_config_and_marker_without_redemption() {
        let (root, c) = fixture();
        nodes::write_private(
            &root.join("node.json"),
            &json!({"node":"friend","credential":"b".repeat(64)}),
        )
        .unwrap();
        nodes::write_private(
            &root.join("host.pending.json"),
            &json!({"node":"friend","config":c}),
        )
        .unwrap();
        let recovered = finish_enrollment(&root).unwrap();
        assert_eq!(recovered.memory, 512);
        assert!(root.join(".ow-data").exists());
        assert!(!root.join("host.pending.json").exists());
        assert!(finish_enrollment(&root).is_ok());
        let cmd = command(&root, &recovered, "run").unwrap();
        let env = cmd.get_envs().collect::<Vec<_>>();
        assert!(
            env.iter()
                .any(|(k, v)| *k == "OW_MAX_MEMORY_MIB" && *v == Some(std::ffi::OsStr::new("512")))
        );
        assert!(
            !env.iter()
                .any(|(k, _)| *k == "HOME" || *k == "OW_SERVER" || *k == "LD_PRELOAD")
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn corrupt_assets_symlinks_and_exact_crash_remnants_fail_safely() {
        let (root, _) = fixture();
        assert!(verify_assets(&root).is_ok());
        let base = root.join("assets/guest/base.ext4");
        fs::write(&base, b"corrupt").unwrap();
        assert!(verify_assets(&root).is_err());
        fs::remove_file(&base).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", &base).unwrap();
        assert!(verify_assets(&root).is_err());
        let stage = root.join(format!("assets-stage-{}", common::nonce().unwrap()));
        fs::create_dir(&stage).unwrap();
        fs::set_permissions(&stage, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(stage.join("guest")).unwrap();
        fs::write(stage.join("guest/base.ext4"), b"partial").unwrap();
        let tmp = root.join(format!("marker-{}.tmp", common::nonce().unwrap()));
        fs::write(&tmp, b"partial").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)).unwrap();
        let unrelated = root.join("my-notes");
        fs::write(&unrelated, b"keep").unwrap();
        clean_remnants(&root).unwrap();
        assert!(!stage.exists());
        assert!(!tmp.exists());
        assert_eq!(fs::read(unrelated).unwrap(), b"keep");
        assert!(root.join("assets").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn stop_and_ready_do_not_infer_success_from_missing_rpc_or_stale_ack() {
        let (root, c) = fixture();
        nodes::write_private(&root.join("host.json"), &serde_json::to_value(&c).unwrap()).unwrap();
        let worker = root.join("worker.lock");
        let lock = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&worker)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(held_lock(&worker).unwrap());
        assert!(run(Some(root.clone()), HostAction::Stop).is_err());
        nodes::write_private(&root.join("node-channel.json"),&json!({"state":"dispatchable","updated":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),"instance":"old","generation":"gen"})).unwrap();
        assert!(!ready(&root));
        let link = root.join("bad.lock");
        std::os::unix::fs::symlink(&worker, &link).unwrap();
        assert!(held_lock(&link).is_err());
        drop(lock);
        fs::remove_dir_all(root).unwrap();
    }
}
