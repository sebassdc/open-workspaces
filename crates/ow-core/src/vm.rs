use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub use crate::common::{nonce, shell_quote};

/// `cgroup.procs` of the guests' cgroup, once the worker split its delegated cgroup.
static GUEST_CGROUP: std::sync::OnceLock<std::ffi::CString> = std::sync::OnceLock::new();

/// The worker's own cgroup when systemd delegated it (`Delegate=` in its unit).
pub fn delegated_cgroup() -> Result<PathBuf> {
    let own = std::fs::read_to_string("/proc/self/cgroup")?;
    let path = own
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .context("cgroup v2 required")?;
    Ok(Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}

/// Splits the worker's delegated cgroup: the worker keeps the subgroup systemd started it
/// in (`DelegateSubgroup=control`: CPU weight 1000, protected memory) and every VMM starts
/// in its sibling `guests` (weight 100), so contention and memory reclaim fall on the
/// guests before the worker that controls them. `control` never enables controllers for
/// children, so a restarted unit can join it while VMMs it left still run (ADR 0011).
pub fn isolate_guests(cgroup: &Path) -> Result<()> {
    let procs = split_cgroup(cgroup)?;
    let _ = GUEST_CGROUP.set(std::ffi::CString::new(
        procs.into_os_string().into_encoded_bytes(),
    )?);
    Ok(())
}
fn split_cgroup(control: &Path) -> Result<PathBuf> {
    let unit = control.parent().context("delegated cgroup has no parent")?;
    let guests = unit.join("guests");
    match std::fs::create_dir(&guests) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        other => other?,
    }
    // The unit's own cgroup holds no process (all are in its subgroups).
    std::fs::write(unit.join("cgroup.subtree_control"), "+cpu +memory")?;
    std::fs::write(control.join("cpu.weight"), "1000")?;
    std::fs::write(control.join("memory.min"), (64u64 << 20).to_string())?;
    std::fs::write(control.join("memory.low"), (256u64 << 20).to_string())?;
    Ok(guests.join("cgroup.procs"))
}

/// Runs in a forked VMM before exec (async-signal-safe calls only): joins the guests'
/// cgroup when there is one.
fn as_guest() -> std::io::Result<()> {
    let write = |path: *const libc::c_char, value: &[u8]| unsafe {
        let fd = libc::open(path, libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let written = libc::write(fd, value.as_ptr().cast(), value.len());
        let error = std::io::Error::last_os_error();
        libc::close(fd);
        if written != value.len() as isize {
            return Err(error);
        }
        Ok(())
    };
    if let Some(procs) = GUEST_CGROUP.get() {
        write(procs.as_ptr(), b"0")?;
    }
    Ok(())
}

fn guest_command(binary: &Path) -> Command {
    // Exec first discards worker credentials and resets /proc ownership; a non-dumpable
    // unprivileged child cannot write its OOM score before exec.
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-ec",
            "printf 500 > /proc/self/oom_score_adj; exec \"$@\"",
            "ow-vmm",
        ])
        .arg(binary);
    command
}

pub fn reflink(source: &Path, destination: &Path) -> Result<()> {
    clone_file(source, destination)?.sync_all()?;
    Ok(())
}

/// A reflink not yet flushed: the caller syncs it, for example after the guest runs again.
fn clone_file(source: &Path, destination: &Path) -> Result<File> {
    let src = File::open(source)?;
    let dst = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let rc = unsafe { libc::ioctl(dst.as_raw_fd(), 0x40049409, src.as_raw_fd()) };
    // Unit fixtures use byte copies on tmpfs; real VM acceptance requires host reflinks.
    #[cfg(test)]
    if rc < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EOPNOTSUPP) {
        // Match reflink semantics: preserve the new disk's mode, not the sealed source's.
        let mut src = src;
        let mut dst = dst;
        std::io::copy(&mut src, &mut dst)?;
        return Ok(dst);
    }
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("disk reflink failed");
    }
    Ok(dst)
}

/// Reflinks an open file (which may already be unlinked) to a new `destination`.
fn reflink_from(src: &File, destination: &Path) -> Result<()> {
    let dst = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let rc = unsafe { libc::ioctl(dst.as_raw_fd(), 0x40049409, src.as_raw_fd()) };
    #[cfg(test)]
    if rc < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EOPNOTSUPP) {
        std::io::copy(&mut &*src, &mut &dst)?;
        return Ok(());
    }
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("memory reflink failed");
    }
    Ok(())
}

/// What the guest's memory pages that are clean (not dirtied since then) hold, so a
/// `Diff` snapshot's dirty pages can be laid over it to make a full memory image.
enum MemoryBase {
    /// No reliable base: the next capture is `Full`.
    Unknown,
    /// Booted with dirty tracking: clean pages are still zero.
    Zero,
    /// The memory image last loaded or captured; held open so it outlives a deleted capture.
    Image(File),
}

/// Writes `diff`'s data ranges (the dirtied pages; the rest is holes) over `memory`.
fn lay_over(diff: &Path, memory: &Path) -> Result<()> {
    use std::os::unix::fs::FileExt;
    let source = File::open(diff)?;
    let target = OpenOptions::new().write(true).open(memory)?;
    let length = source.metadata()?.len();
    ensure!(
        length == target.metadata()?.len(),
        "diff memory size differs from its base"
    );
    let mut buffer = vec![0; 1 << 20];
    let mut offset = 0i64;
    while (offset as u64) < length {
        let start = unsafe { libc::lseek(source.as_raw_fd(), offset, libc::SEEK_DATA) };
        if start < 0 {
            // ENXIO: no data after `offset`.
            break;
        }
        let end = unsafe { libc::lseek(source.as_raw_fd(), start, libc::SEEK_HOLE) };
        ensure!(end > start, "diff memory extent map failed");
        let mut at = start as u64;
        while at < end as u64 {
            let n = source.read_at(&mut buffer[..(end as u64 - at).min(1 << 20) as usize], at)?;
            ensure!(n > 0, "diff memory ended early");
            target.write_all_at(&buffer[..n], at)?;
            at += n as u64;
        }
        offset = end;
    }
    target.sync_all()?;
    Ok(())
}

pub fn host(command: &str, args: &[&str]) -> Result<()> {
    let result = Command::new(command).args(args).output()?;
    ensure!(
        result.status.success(),
        "{command} failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(())
}

/// The fds the VMM inherits and holds itself, so they outlive the worker (ADR 0011): its
/// serial console's PTY master and the read end of its diagnostics pipe.
const PTY_FD: i32 = 3;
const LOG_FD: i32 = 4;
/// `machines/ID/vmm.json`: what a later worker needs to find and adopt this VMM.
pub const RECORD: &str = "vmm.json";
const RECORD_FORMAT: u64 = 1;

/// A VMM process: spawned by this worker, or adopted from an earlier one.
pub enum Vmm {
    Spawned(Child),
    /// Not this process's child: observed and signalled through its pidfd only.
    Adopted {
        pid: u32,
        pidfd: std::os::fd::OwnedFd,
    },
}

impl From<Child> for Vmm {
    fn from(child: Child) -> Self {
        Self::Spawned(child)
    }
}

impl Vmm {
    pub fn id(&self) -> u32 {
        match self {
            Self::Spawned(child) => child.id(),
            Self::Adopted { pid, .. } => *pid,
        }
    }

    /// `Some(())` once the process exited.
    pub fn try_wait(&mut self) -> std::io::Result<Option<()>> {
        match self {
            Self::Spawned(child) => child.try_wait().map(|s| s.map(|_| ())),
            Self::Adopted { pidfd, .. } => Ok(pidfd_exited(pidfd, 0)?.then_some(())),
        }
    }

    pub fn kill(&mut self) -> std::io::Result<()> {
        match self {
            Self::Spawned(child) => child.kill(),
            Self::Adopted { pidfd, .. } => {
                let rc = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        pidfd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    )
                };
                let error = std::io::Error::last_os_error();
                // Exited already (its zombie is its new parent's to reap).
                if rc < 0 && error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
                Ok(())
            }
        }
    }

    pub fn wait(&mut self) -> std::io::Result<()> {
        match self {
            Self::Spawned(child) => child.wait().map(|_| ()),
            Self::Adopted { pidfd, .. } => {
                while !pidfd_exited(pidfd, -1)? {}
                Ok(())
            }
        }
    }
}

/// Whether a pidfd's process exited, waiting up to `timeout_ms` (-1: until it does).
fn pidfd_exited(pidfd: &std::os::fd::OwnedFd, timeout_ms: i32) -> std::io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready >= 0 {
            return Ok(ready > 0);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// A process's start time in clock ticks since boot (`/proc/PID/stat` field 22): with the
/// PID it names one process, never a later one that reused the PID.
pub fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command name, which may itself hold spaces.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

fn boot_id() -> Option<String> {
    Some(
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()?
            .trim()
            .to_owned(),
    )
}

/// Whether the recorded process still runs: same boot, same PID, same start time.
fn recorded_runs(record: &Value) -> Option<u32> {
    let pid = u32::try_from(record["pid"].as_u64()?).ok()?;
    (record["boot_id"].as_str()? == boot_id()?
        && start_time(pid)? == record["start_time"].as_u64()?)
    .then_some(pid)
}

/// Whether this host lets a later worker take a VMM's fds (`pidfd_getfd`, Linux 5.6+, not
/// refused by Yama's `ptrace_scope` 3); without it a restart must hibernate (ADR 0011).
pub fn handover_supported() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        if std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope")
            .is_ok_and(|s| s.trim() == "3")
        {
            return false;
        }
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, std::process::id(), 0) };
        if raw < 0 {
            return false;
        }
        let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as i32) };
        let taken = unsafe {
            libc::syscall(
                libc::SYS_pidfd_getfd,
                pidfd.as_raw_fd(),
                pidfd.as_raw_fd(),
                0,
            )
        };
        if taken < 0 {
            return false;
        }
        drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(taken as i32) });
        true
    })
}

/// The recorded VMM's PID when that very process still runs.
pub fn recorded_live_pid(directory: &Path) -> Option<u32> {
    let record: Value =
        serde_json::from_slice(&std::fs::read(directory.join(RECORD)).ok()?).ok()?;
    recorded_runs(&record)
}

/// What a worker found of a recorded VMM.
pub enum Adoption {
    /// The recorded process no longer runs.
    Gone,
    /// The recorded VMM runs, verified, but this worker cannot keep it: another binary or
    /// guest kernel, or its console and log could not be taken. The caller ends it through
    /// this handle, so no VMM is left that no worker manages.
    Refused(String, Vmm),
    Adopted(Box<Vm>),
}

pub struct Vm {
    pub child: Vmm,
    master: File,
    log: File,
    diagnostics: Option<std::thread::JoinHandle<()>>,
    buffer: Vec<u8>,
    socket: PathBuf,
    pub tap: String,
    pub address: String,
    memory_base: MemoryBase,
    /// Detached for the next worker: dropping it leaves the VMM, TAP and policy running.
    keep: bool,
}

impl Vm {
    /// `record` carries what the caller adds to `vmm.json` (the hashes of the binary and
    /// guest kernel); the record is written as soon as the process exists.
    pub fn spawn(binary: &Path, directory: &Path, index: u32, record: Value) -> Result<Self> {
        let tap = format!("ow{index}");
        let address = format!("198.18.{}.{}", index / 64, index % 64 * 4 + 2);
        let host_address = format!("198.18.{}.{}/30", index / 64, index % 64 * 4 + 1);
        host("ip", &["tuntap", "add", "dev", &tap, "mode", "tap"])?;
        let result = (|| {
            host("ip", &["addr", "add", &host_address, "dev", &tap])?;
            crate::network::register(index)?;
            let mut master_fd = -1;
            let mut slave_fd = -1;
            let rc = unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            };
            ensure!(rc == 0, "openpty failed");
            // Never inherited by later VMMs or helpers: each console reaches one guest only.
            for fd in [master_fd, slave_fd] {
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
            }
            let master = unsafe { File::from_raw_fd(master_fd) };
            let slave = unsafe { File::from_raw_fd(slave_fd) };
            let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
            ensure!(
                unsafe { libc::tcgetattr(slave_fd, attributes.as_mut_ptr()) } == 0,
                "tcgetattr failed"
            );
            let mut attributes = unsafe { attributes.assume_init() };
            attributes.c_lflag &= !libc::ECHO;
            ensure!(
                unsafe { libc::tcsetattr(slave_fd, libc::TCSANOW, &attributes) } == 0,
                "tcsetattr failed"
            );
            let socket = directory.join("api.sock");
            ensure!(socket.as_os_str().len() < 104, "VM socket path too long");
            if socket.exists() {
                std::fs::remove_file(&socket)?;
            }
            // VMM diagnostics can split guest command markers, particularly when
            // a restored NIC emits packets before its TAP is exposed. Keep them
            // off the authenticated management serial protocol.
            let diagnostics = OpenOptions::new()
                .create(true)
                .append(true)
                .open(directory.join("vmm.log"))?;
            let mut pipe = [-1; 2];
            ensure!(
                unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } == 0,
                "VMM log pipe failed"
            );
            let (errors, writer) =
                unsafe { (File::from_raw_fd(pipe[0]), File::from_raw_fd(pipe[1])) };
            let mut command = guest_command(binary);
            command
                .args([
                    "--log-path",
                    "/proc/self/fd/2",
                    "--level",
                    "Warning",
                    "--api-sock",
                ])
                .arg(&socket)
                .current_dir(directory)
                .stdin(Stdio::from(slave.try_clone()?))
                .stdout(Stdio::from(slave.try_clone()?))
                .stderr(Stdio::from(writer));
            let (pty, reader) = (master.as_raw_fd(), errors.as_raw_fd());
            // No parent-death signal: the VMM outlives a worker restart and the next worker
            // adopts it (ADR 0011). It holds its own console master and log reader as fds 3
            // and 4, so neither hangs up while no worker holds them.
            unsafe {
                command.pre_exec(move || {
                    // Through fds >= 10 first: a source already at 3 or 4 is not clobbered.
                    let pty = libc::fcntl(pty, libc::F_DUPFD_CLOEXEC, 10);
                    let reader = libc::fcntl(reader, libc::F_DUPFD_CLOEXEC, 10);
                    // dup2 leaves the new fds without close-on-exec.
                    if pty < 0
                        || reader < 0
                        || libc::dup2(pty, PTY_FD) < 0
                        || libc::dup2(reader, LOG_FD) < 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    as_guest()
                });
            }
            let child = Vmm::Spawned(command.spawn()?);
            drop(command);
            let diagnostic_thread = drain(errors, diagnostics);
            let log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(directory.join("serial.log"))?;
            let mut vm = Self {
                child,
                master,
                log,
                diagnostics: Some(diagnostic_thread),
                buffer: Vec::new(),
                socket,
                tap: tap.clone(),
                address,
                memory_base: MemoryBase::Unknown,
                keep: false,
            };
            // Before anything can fail or the worker die: a later worker finds this VMM by
            // it (ADR 0011). Any error drops `vm`, which ends the VMM.
            vm.write_record(index, record)?;
            let started = Instant::now();
            while !vm.socket.exists() {
                ensure!(vm.child.try_wait()?.is_none(), "VMM exited during startup");
                ensure!(
                    started.elapsed() < Duration::from_secs(10),
                    "VMM socket timeout"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(vm)
        })();
        if result.is_err() {
            let _ = host("ip", &["link", "del", &tap]);
            crate::network::unregister(index);
        }
        result
    }

    /// `machines/ID/vmm.json`: the process (boot, PID, start time), its network and the
    /// fds it holds, plus the caller's `extra` fields.
    fn write_record(&self, index: u32, extra: Value) -> Result<()> {
        let pid = self.child.id();
        let mut record = json!({
            "format": RECORD_FORMAT,
            "boot_id": boot_id().context("boot ID")?,
            "pid": pid,
            "start_time": start_time(pid).context("VMM start time")?,
            "index": index,
            "tap": self.tap,
            "address": self.address,
            "pty_fd": PTY_FD,
            "log_fd": LOG_FD,
        });
        if let (Some(record), Value::Object(extra)) = (record.as_object_mut(), extra) {
            record.extend(extra);
        }
        let directory = self.socket.parent().context("VMM directory")?;
        let temporary = directory.join(format!("{RECORD}.{}", nonce()?));
        let mut file = File::create(&temporary)?;
        file.write_all(record.to_string().as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, directory.join(RECORD))?;
        File::open(directory)?.sync_all()?;
        Ok(())
    }

    /// Takes over the VMM an earlier worker recorded in `directory`, after checking it is
    /// the very process recorded (PID and start time) and runs the binary and guest kernel
    /// this worker runs. Signals nothing it did not verify.
    pub fn adopt(
        directory: &Path,
        vmm_sha256: &str,
        kernel_sha256: &str,
        hash: impl Fn(&Path) -> Result<String>,
    ) -> Result<Adoption> {
        // Only the identity is needed to find the process; anything else that is wrong
        // with a verified, running VMM refuses it (the caller ends it), never leaves it
        // running unmanaged.
        let record: Value = serde_json::from_slice(&std::fs::read(directory.join(RECORD))?)
            .context("invalid VMM record")?;
        let Some(pid) = recorded_runs(&record) else {
            return Ok(Adoption::Gone);
        };
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(Adoption::Gone);
            }
            // Out of fds or memory: it may run still, so keep its record and TAP.
            return Err(error).context(format!("pidfd of VMM {pid}"));
        }
        let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as i32) };
        // The pidfd names whichever process held the PID when it was opened: check again.
        if recorded_runs(&record) != Some(pid) {
            return Ok(Adoption::Gone);
        }
        match Self::take_over(
            directory,
            &record,
            pid,
            pidfd,
            vmm_sha256,
            kernel_sha256,
            hash,
        ) {
            Ok(vm) => Ok(Adoption::Adopted(Box::new(vm))),
            Err((e, pidfd)) => Ok(Adoption::Refused(
                format!("VMM {pid}: {e:#}"),
                Vmm::Adopted { pid, pidfd },
            )),
        }
    }

    /// Takes a verified VMM over, or hands its pidfd back with the reason it cannot.
    fn take_over(
        directory: &Path,
        record: &Value,
        pid: u32,
        pidfd: std::os::fd::OwnedFd,
        vmm_sha256: &str,
        kernel_sha256: &str,
        hash: impl Fn(&Path) -> Result<String>,
    ) -> std::result::Result<Self, (anyhow::Error, std::os::fd::OwnedFd)> {
        let parts = (|| -> Result<_> {
            ensure!(
                record["format"] == RECORD_FORMAT,
                "record format {} is not {RECORD_FORMAT}",
                record["format"]
            );
            let running =
                hash(Path::new(&format!("/proc/{pid}/exe"))).context("reading its binary")?;
            ensure!(
                running == vmm_sha256,
                "runs binary {running}, this worker runs {vmm_sha256}"
            );
            ensure!(
                record["kernel_sha256"] == kernel_sha256,
                "booted guest kernel {}, this worker boots {kernel_sha256}",
                record["kernel_sha256"]
            );
            let tap = record["tap"].as_str().context("record TAP")?.to_owned();
            let address = record["address"]
                .as_str()
                .context("record address")?
                .to_owned();
            let take = |fd: &Value| -> Result<File> {
                let fd = fd.as_i64().context("record fd")? as i32;
                let raw = unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), fd, 0) };
                ensure!(
                    raw >= 0,
                    "taking its fd {fd}: {}",
                    std::io::Error::last_os_error()
                );
                Ok(unsafe { File::from_raw_fd(raw as i32) })
            };
            let master = take(&record["pty_fd"])?;
            let errors = take(&record["log_fd"])?;
            let diagnostics = OpenOptions::new()
                .create(true)
                .append(true)
                .open(directory.join("vmm.log"))?;
            let log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(directory.join("serial.log"))?;
            Ok((master, errors, diagnostics, log, tap, address))
        })();
        match parts {
            Ok((master, errors, diagnostics, log, tap, address)) => Ok(Self {
                child: Vmm::Adopted { pid, pidfd },
                master,
                log,
                diagnostics: Some(drain(errors, diagnostics)),
                buffer: Vec::new(),
                socket: directory.join("api.sock"),
                tap,
                address,
                // The last captured image is not known here: the next capture is a full one.
                memory_base: MemoryBase::Unknown,
                keep: false,
            }),
            Err(e) => Err((e, pidfd)),
        }
    }

    /// Checks an adopted VMM answers: its API (a VM a capture left paused runs again) and
    /// a guest command on its console within `timeout`.
    pub fn healthy(&mut self, timeout: Duration) -> Result<()> {
        let info = self.api_get("/")?;
        match info["state"].as_str() {
            Some("Running") => {}
            Some("Paused") => self.resume()?,
            other => anyhow::bail!("VMM state {other:?}"),
        }
        let (_, code) = self.command_within("true", timeout)?;
        ensure!(code == 0, "guest exit {code}");
        Ok(())
    }

    /// Leaves the VMM, its TAP and network policy running for the next worker.
    pub fn detach(mut self) {
        self.keep = true;
    }

    pub fn api(&self, method: &str, endpoint: &str, body: Value) -> Result<()> {
        self.api_waiting(method, endpoint, body, 30)
    }

    fn api_get(&self, endpoint: &str) -> Result<Value> {
        let output = Command::new("curl")
            .args(["-sS", "--max-time", "10", "--unix-socket"])
            .arg(&self.socket)
            .args(["-w", "\n%{http_code}"])
            .arg(format!("http://localhost{endpoint}"))
            .output()?;
        ensure!(
            output.status.success(),
            "VMM API transport: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout)?;
        let (body, status) = text.rsplit_once('\n').context("invalid VMM response")?;
        ensure!(status == "200", "VMM {endpoint}: HTTP {status}: {body}");
        Ok(serde_json::from_str(body)?)
    }
}

/// Copies the VMM's diagnostics into `vmm.log`, capped at 4 MiB, until the pipe closes.
fn drain(mut errors: File, diagnostics: File) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut diagnostics = diagnostics;
        let mut bytes = [0; 8192];
        let mut recording = true;
        loop {
            let Ok(n) = errors.read(&mut bytes) else {
                break;
            };
            if n == 0 {
                break;
            }
            let remaining = (4 * 1024 * 1024u64).saturating_sub(
                diagnostics
                    .metadata()
                    .map(|m| m.len())
                    .unwrap_or(4 * 1024 * 1024),
            ) as usize;
            if recording && diagnostics.write_all(&bytes[..n.min(remaining)]).is_err() {
                // A full/unavailable diagnostic sink must not block or
                // break the VMM's logging pipe. Drain and discard.
                recording = false;
            }
        }
    })
}

impl Vm {
    /// `api` with a longer limit, for calls that write guest RAM to disk: a 4 GiB
    /// snapshot took over 30 s on a gp3 volume at its baseline throughput.
    fn api_waiting(&self, method: &str, endpoint: &str, body: Value, seconds: u32) -> Result<()> {
        let output = Command::new("curl")
            .args(["-sS", "--max-time", &seconds.to_string(), "--unix-socket"])
            .arg(&self.socket)
            .args([
                "-X",
                method,
                "-H",
                "Content-Type: application/json",
                "--data",
            ])
            .arg(body.to_string())
            .args(["-w", "\n%{http_code}"])
            .arg(format!("http://localhost{endpoint}"))
            .output()?;
        ensure!(
            output.status.success(),
            "VMM API transport: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout)?;
        let (body, status) = text.rsplit_once('\n').context("invalid VMM response")?;
        let code: u16 = status.parse()?;
        ensure!(
            (200..300).contains(&code),
            "VMM {endpoint}: HTTP {code}: {body}"
        );
        Ok(())
    }

    fn read_until(&mut self, marker: &[u8], timeout: Duration) -> Result<Vec<u8>> {
        let started = Instant::now();
        loop {
            if let Some(position) = self.buffer.windows(marker.len()).position(|w| w == marker) {
                return Ok(self.buffer.drain(..position + marker.len()).collect());
            }
            ensure!(
                started.elapsed() < timeout,
                "guest command timed out after {timeout:?}"
            );
            ensure!(
                self.buffer.len() < 2 * 1024 * 1024,
                "guest output limit exceeded"
            );
            let mut descriptor = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut descriptor, 1, 100) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("poll");
            }
            if ready > 0 {
                let mut bytes = [0u8; 65536];
                let count = self.master.read(&mut bytes)?;
                ensure!(count > 0, "guest serial closed");
                let remaining =
                    (4 * 1024 * 1024u64).saturating_sub(self.log.metadata()?.len()) as usize;
                self.log.write_all(&bytes[..count.min(remaining)])?;
                self.buffer.extend_from_slice(&bytes[..count]);
            }
        }
    }

    pub fn command(&mut self, command: &str) -> Result<(String, i32)> {
        self.command_within(command, Duration::from_secs(30))
    }

    fn command_within(&mut self, command: &str, timeout: Duration) -> Result<(String, i32)> {
        ensure!(command.len() < 512 * 1024, "command too large");
        let marker = format!("OW_DONE_{}", nonce()?);
        let timed_out = format!("guest command timed out after {timeout:?}");
        // Output marker uses a private nonce; this transport supports trusted local workloads only.
        // The begin marker separates this command's output from any a timed-out command left
        // on the channel: read_until discards that, its own end marker included.
        // The shell cannot read the host while it waits, so a guest-side watchdog ends a command
        // that outlives the timeout: TERM, then KILL, to its own session (process group when the
        // shell has job control). USR1 interrupts the shell's wait, including when the
        // workload ignores TERM, so cleanup finishes before the next command can begin.
        let wire = format!(
            "ow_t=0; trap 'ow_t=1' USR1; printf '{marker}:begin\\n'; ow_s=; case $- in *m*) ;; *) command -v setsid >/dev/null 2>&1 && ow_s=setsid;; esac; \
             $ow_s /bin/sh -c {} </dev/null & ow_p=$!; \
             (ow_i=0; while kill -0 $ow_p 2>/dev/null; do if [ $ow_i -ge {} ]; then kill -USR1 $$; break; fi; \
             sleep 1; ow_i=$((ow_i+1)); done) & ow_w=$!; \
             wait $ow_p; ow_rc=$?; kill $ow_w 2>/dev/null; wait $ow_w 2>/dev/null; \
             trap - USR1; if [ $ow_t -eq 1 ]; then kill -TERM -$ow_p 2>/dev/null || kill -TERM $ow_p 2>/dev/null; \
             sleep 2; kill -KILL -$ow_p 2>/dev/null || kill -KILL $ow_p 2>/dev/null; wait $ow_p 2>/dev/null; printf '\\n{marker}:timeout\\n'; fi; printf '\\n{marker}:%s:{marker}\\n' \"$ow_rc\"\n",
            shell_quote(command),
            timeout.as_secs().max(1),
        );
        self.master.write_all(wire.as_bytes())?;
        let end = format!(":{marker}");
        // The watchdog needs up to a second to notice, two for TERM to work, then KILL.
        let bytes = match self.read_until(end.as_bytes(), timeout + Duration::from_secs(15)) {
            Err(error) if error.to_string().starts_with("guest command timed out") => {
                anyhow::bail!(timed_out)
            }
            other => other?,
        };
        let text = String::from_utf8_lossy(&bytes).replace('\r', "");
        let start = format!("{marker}:begin\n");
        let begin = text.rfind(&start).context("missing begin marker")? + start.len();
        let finish = text
            .rfind(&format!("{marker}:"))
            .context("missing exit marker")?;
        ensure!(begin <= finish, "missing exit marker");
        ensure!(
            !text[begin..].contains(&format!("\n{marker}:timeout\n")),
            timed_out
        );
        let code = text[finish + marker.len() + 1..text.len() - end.len()]
            .trim()
            .parse()?;
        // Drop the newline the exit marker printf adds.
        let output = text[begin..finish]
            .strip_suffix('\n')
            .unwrap_or(&text[begin..finish])
            .to_string();
        Ok((output, code))
    }

    pub fn checked(&mut self, command: &str) -> Result<String> {
        let (output, code) = self.command(command)?;
        ensure!(code == 0, "guest exit {code}: {output}");
        Ok(output)
    }

    pub fn boot(&mut self, kernel: &Path, memory: u32, cpus: u32) -> Result<()> {
        self.api(
            "PUT",
            "/machine-config",
            // Dirty tracking lets repeat captures write only the pages that changed.
            json!({"vcpu_count":cpus,"mem_size_mib":memory,"track_dirty_pages":true}),
        )?;
        // Discard keeps box disks sparse: blocks the guest frees become holes again, which
        // snapshot hashing and copies skip. Firecracker supports it on the default Sync engine.
        self.api("PUT", "/boot-source", json!({"kernel_image_path":kernel,"boot_args":"console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw rootflags=discard init=/init"}))?;
        self.api("PUT", "/drives/rootfs", json!({"drive_id":"rootfs","path_on_host":"disk.ext4","is_root_device":true,"is_read_only":false,"discard":true}))?;
        self.api(
            "PUT",
            "/network-interfaces/net1",
            json!({"iface_id":"net1","host_dev_name":self.tap,"guest_mac":"06:00:00:00:00:01"}),
        )?;
        self.api("PUT", "/actions", json!({"action_type":"InstanceStart"}))?;
        self.read_until(b"OW_GUEST_READY", Duration::from_secs(20))?;
        self.checked("true")?;
        // Online discard covers blocks freed from now on; one trim in the background returns
        // what was freed before (a disk that ran without discard). Images without fstrim skip it.
        self.checked("(fstrim / || /ow/bin/busybox fstrim /) >/dev/null 2>&1 &")?;
        self.memory_base = MemoryBase::Zero;
        Ok(())
    }

    pub fn restore(&mut self, snapshot: &Path) -> Result<()> {
        let image = File::open(snapshot.join("memory"))?;
        self.api(
            "PUT",
            "/snapshot/load",
            json!({"snapshot_path":snapshot.join("state"),
            "mem_backend":{"backend_path":snapshot.join("memory"),"backend_type":"File"},
            "network_overrides":[{"iface_id":"net1","host_dev_name":self.tap}],"resume_vm":false,"clock_realtime":true,"track_dirty_pages":true}),
        )?;
        // Loaded pages are clean against this image until the guest writes them.
        self.memory_base = MemoryBase::Image(image);
        self.resume()
    }

    fn resume(&mut self) -> Result<()> {
        self.api("PATCH", "/vm", json!({"state":"Resumed"}))?;
        // A restored guest may retain snapshot wall time despite clock_realtime.
        let seconds = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        self.checked(&format!("date -u -s '@{seconds}' >/dev/null"))
            .context("synchronizing guest clock after resume")?;
        Ok(())
    }

    pub fn network(&mut self, id: &str, index: u32, fork: bool) -> Result<()> {
        // Restored sockets/sessions never survive as authorized external access.
        crate::ssh_guest::kill(self)?;
        let mac = format!("02:fc:00:00:{:02x}:{:02x}", index / 256, index % 256);
        let gateway = format!("198.18.{}.{}", index / 64, index % 64 * 4 + 1);
        self.checked(&format!("ip link set eth0 down; ip addr flush dev eth0; ip link set eth0 address {mac}; ip addr add {}/30 dev eth0; ip link set eth0 up; ip route replace default via {gateway}; printf 'nameserver 10.0.2.3\\n' > /etc/resolv.conf; hostname {}", self.address, shell_quote(id)))?;
        self.checked(&format!("sed -i '/^127\\.0\\.1\\.1[[:space:]].*# ow-hostname$/d' /etc/hosts; printf '\\n127.0.1.1 {} # ow-hostname\\n' >> /etc/hosts", id))?;
        if fork {
            // VMGenID reseeds the guest kernel on restore; template has no account credentials.
            self.checked("cat /proc/sys/kernel/random/uuid > /etc/machine-id; rm -f /etc/ssh/ssh_host_* /etc/ow-ssh/authorized_keys; sync")?;
        } else {
            self.checked("if [ ! -s /etc/machine-id ]; then cat /proc/sys/kernel/random/uuid > /etc/machine-id; fi; sync")?;
        }
        host("ip", &["link", "set", &self.tap, "up"])?;
        Ok(())
    }

    /// Pauses the guest and captures RAM state and disk. The guest resumes afterwards,
    /// unless `resume` is false and the capture succeeded (hibernation drops the VM).
    /// Saves RAM and device state and reflinks the disk while the guest is paused; returns
    /// how long it was paused. The reflink stays inside the pause: it pairs disk with RAM.
    ///
    /// When the VMM tracks dirty pages, only pages written since its last load or capture
    /// are saved (a `Diff` snapshot), and after the guest runs again they are laid over a
    /// reflink of that earlier image, so `memory` is still a complete image.
    pub fn capture(&mut self, directory: &Path, disk: &Path, resume: bool) -> Result<Duration> {
        // A hibernation saves RAM and disk paused together, consistent without a sync, so a
        // guest whose console does not answer (an adoption that failed its check) still
        // hibernates; a capture the guest keeps running after needs its disk synced first.
        if let Err(error) = self.checked("sync") {
            ensure!(!resume, error);
            eprintln!("hibernating without a guest sync: {error:#}");
        }
        let memory = directory.join("memory");
        let diff = !matches!(self.memory_base, MemoryBase::Unknown);
        let saved = if diff {
            directory.join("memory.diff")
        } else {
            memory.clone()
        };
        let paused = Instant::now();
        self.api("PATCH", "/vm", json!({"state":"Paused"}))?;
        // Saving may reset the dirty log even if a later step or the API reply fails.
        let base = std::mem::replace(&mut self.memory_base, MemoryBase::Unknown);
        let captured = (|| {
            self.api_waiting("PUT", "/snapshot/create", json!({"snapshot_type":if diff {"Diff"} else {"Full"},"snapshot_path":directory.join("state"),"mem_file_path":saved}), 900)?;
            reflink(disk, &directory.join("disk.ext4"))
        })();
        let paused_for = if captured.is_ok() && !resume {
            paused.elapsed()
        } else {
            let resumed = self.resume();
            let paused_for = paused.elapsed();
            captured?;
            resumed?;
            paused_for
        };
        if !diff {
            // A complete save repairs the base after an interrupted capture.
            self.memory_base = MemoryBase::Image(File::open(&memory)?);
            return Ok(paused_for);
        }
        // The dirty log restarted at this capture: the new image is the next base.
        let merged = (|| {
            match &base {
                MemoryBase::Image(base) => {
                    reflink_from(base, &memory)?;
                    lay_over(&saved, &memory)?;
                    std::fs::remove_file(&saved)?;
                }
                // Since boot, clean pages are zero: the sparse diff is the whole image.
                _ => std::fs::rename(&saved, &memory)?,
            }
            File::open(&memory).map_err(anyhow::Error::from)
        })();
        match merged {
            Ok(image) => {
                self.memory_base = MemoryBase::Image(image);
                Ok(paused_for)
            }
            Err(error) => {
                self.memory_base = MemoryBase::Unknown;
                Err(error.context("merging changed memory pages"))
            }
        }
    }

    /// A crash-consistent copy of the disk alone (an automatic recovery point): the guest
    /// is paused only while the disk is reflinked, and no RAM is written. The clone is
    /// flushed after the guest runs again, and the dirty-page base of the next RAM capture
    /// is left as it was. Returns how long the guest was paused.
    pub fn capture_disk(&mut self, directory: &Path, disk: &Path) -> Result<Duration> {
        self.checked("sync")?;
        // A reflink first writes back the source's dirty pages: do that while the guest runs,
        // so the pause covers only what it wrote since.
        File::open(disk)?.sync_data()?;
        let paused = Instant::now();
        if let Err(error) = self.api("PATCH", "/vm", json!({"state":"Paused"})) {
            // The VMM may have paused before its reply was lost.
            let _ = self.resume();
            return Err(error);
        }
        let cloned = clone_file(disk, &directory.join("disk.ext4"));
        // A guest left paused would stall every later point too: try once more.
        let resumed = self.resume().or_else(|_| self.resume());
        let paused_for = paused.elapsed();
        resumed?;
        cloned?.sync_all()?;
        Ok(paused_for)
    }

    pub fn alive(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    /// Export acknowledgment requires observed child exit, not a best-effort Drop.
    pub fn terminate(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_none() {
            self.child.kill()?;
        }
        self.child.wait()?;
        Ok(())
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if self.keep {
            // Handed to the next worker: its drain thread ends with this process.
            return;
        }
        // Spawned here or adopted through a pidfd; never signal a PID loaded from disk.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(thread) = self.diagnostics.take() {
            let _ = thread.join();
        }
        if let Some(directory) = self.socket.parent()
            && !directory.as_os_str().is_empty()
        {
            let _ = std::fs::remove_file(directory.join(RECORD));
        }
        if let Ok(index) = self.tap.trim_start_matches("ow").parse() {
            // Before the TAP goes: a later box on this index must not answer as this one.
            crate::metadata::detach(index);
            let _ = host("ip", &["link", "del", &self.tap]);
            crate::network::unregister(index);
        } else {
            let _ = host("ip", &["link", "del", &self.tap]);
        }
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    // Exercise both a socket shell and the guest's controlling-terminal/job-control path.
    fn timeout_recovery(terminal: bool, timeout: Duration, developer: bool) {
        let (master, serial): (File, File) = if terminal {
            let (mut master, mut slave) = (-1, -1);
            assert_eq!(
                unsafe {
                    libc::openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null(),
                        std::ptr::null(),
                    )
                },
                0
            );
            let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
            assert_eq!(
                unsafe { libc::tcgetattr(slave, attributes.as_mut_ptr()) },
                0
            );
            let mut attributes = unsafe { attributes.assume_init() };
            attributes.c_lflag &= !libc::ECHO;
            assert_eq!(
                unsafe { libc::tcsetattr(slave, libc::TCSANOW, &attributes) },
                0
            );
            unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) }
        } else {
            let (master, serial) = UnixStream::pair().unwrap();
            (
                std::os::fd::OwnedFd::from(master).into(),
                std::os::fd::OwnedFd::from(serial).into(),
            )
        };
        let log = std::env::temp_dir().join(format!("ow-serial-{}", nonce().unwrap()));
        let mut shell = Command::new("/bin/sh");
        shell
            .arg("-i")
            .env("PS1", "OW> ")
            .stdin(Stdio::from(serial.try_clone().unwrap()))
            .stdout(Stdio::from(serial.try_clone().unwrap()))
            .stderr(Stdio::from(serial));
        if terminal {
            unsafe {
                shell.pre_exec(|| {
                    if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let child = shell.spawn().unwrap();
        let mut vm = Vm {
            child: child.into(),
            keep: false,
            master,
            log: File::create(&log).unwrap(),
            diagnostics: None,
            buffer: Vec::new(),
            socket: PathBuf::new(),
            tap: "command-fixture-no-host-interface".into(),
            memory_base: MemoryBase::Unknown,
            address: String::new(),
        };
        let command = |text: &str| {
            if developer {
                crate::runtime::exec_command(text)
            } else {
                text.to_owned()
            }
        };
        if developer {
            assert_eq!(
                vm.command(&command("id -u; printf '%s\\n' \"$HOME\"; pwd"))
                    .unwrap(),
                ("1000\n/home/dev\n/home/dev\n".into(), 0)
            );
        }
        let started = Instant::now();
        let error = vm
            .command_within(
                &command(&format!(
                    "(sleep {}; echo leftover-from-child) & sleep 65; echo leftover-from-slow",
                    timeout.as_secs() + 4
                )),
                timeout,
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("guest command timed out after {timeout:?}")
        );
        // Ended by the guest watchdog, not left running for 65 s.
        assert!(
            started.elapsed() < timeout + Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let quick = Instant::now();
        let (output, code) = vm.command(&command("sleep 3; echo next-command")).unwrap();
        assert_eq!((output.as_str(), code), ("next-command\n", 0));
        assert!(
            quick.elapsed() < Duration::from_secs(5),
            "{:?}",
            quick.elapsed()
        );
        // A TERM-resistant child must not write into the immediately following command.
        let error = vm
            .command_within(
                &command("(trap '' TERM; sleep 5; echo leftover-from-resistant-child) & sleep 65"),
                Duration::from_secs(1),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "guest command timed out after 1s");
        assert_eq!(
            vm.command(&command("sleep 3; echo after-resistant-child"))
                .unwrap(),
            ("after-resistant-child\n".into(), 0)
        );
        assert_eq!(
            vm.command_within(&command("trap '' TERM; sleep 65"), Duration::from_secs(1))
                .unwrap_err()
                .to_string(),
            "guest command timed out after 1s"
        );
        assert_eq!(
            vm.command(&command("echo last")).unwrap(),
            ("last\n".into(), 0)
        );
        assert_eq!(
            vm.command(&command("cat; echo stdin-closed")).unwrap(),
            ("stdin-closed\n".into(), 0)
        );
        let (output, code) = vm.command(&command("printf 'no newline'; exit 3")).unwrap();
        assert_eq!((output.as_str(), code), ("no newline", 3));
        drop(vm);
        std::fs::remove_file(&log).unwrap();
    }

    #[test]
    fn a_timed_out_command_is_ended_and_the_next_command_gets_only_its_own_output() {
        timeout_recovery(false, Duration::from_secs(1), false);
    }

    #[test]
    #[ignore = "requires a disposable Linux container with root, dev uid 1000 and /etc/ow-developer"]
    fn production_timeout_recovers_for_developer_exec() {
        assert_eq!(unsafe { libc::geteuid() }, 0);
        assert!(Path::new("/etc/ow-developer").exists());
        timeout_recovery(true, Duration::from_secs(30), true);
    }

    #[test]
    fn production_timeout_recovers_with_terminal_job_control() {
        timeout_recovery(true, Duration::from_secs(30), false);
    }
}

#[cfg(test)]
mod clock_tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        os::unix::net::{UnixListener, UnixStream},
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    };

    // Exercise the real API + management-serial path without changing the test host's clock.
    fn clock_after_resume(capture_failure: bool, date_exit: i32) {
        let root = std::env::temp_dir().join(format!("ow-clock-{}", nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        // The memory image a restore loads (and keeps as the base of later diffs).
        std::fs::write(root.join("memory"), b"VM memory").unwrap();
        let socket = root.join("api.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let api = std::thread::spawn(move || {
            let endpoints = if capture_failure {
                vec!["/vm", "/snapshot/create", "/vm"]
            } else {
                vec!["/snapshot/load", "/vm"]
            };
            for endpoint in endpoints {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line.split_whitespace().nth(1), Some(endpoint));
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap();
                if endpoint == "/snapshot/load" {
                    assert_eq!(body["clock_realtime"], true);
                    assert_eq!(body["resume_vm"], false);
                }
                let status = if endpoint == "/snapshot/create" {
                    "500 Failed"
                } else {
                    "204 No Content"
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
        });
        let initial = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 17 * 60;
        let clock = Arc::new(AtomicU64::new(initial));
        let guest_clock = clock.clone();
        let (master, guest) = UnixStream::pair().unwrap();
        guest
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let guest = std::thread::spawn(move || {
            let mut serial = BufReader::new(guest);
            for _ in 0..if capture_failure { 2 } else { 1 } {
                let mut line = String::new();
                serial.read_line(&mut line).unwrap();
                let command = wire_command(&line);
                let command = command.as_str();
                let code = if let Some(time) = command.strip_prefix("date -u -s '@") {
                    let seconds = time.split_once('\'').unwrap().0.parse::<u64>().unwrap();
                    if date_exit == 0 {
                        guest_clock.store(seconds, Ordering::SeqCst);
                    }
                    date_exit
                } else {
                    assert!(command == "sync" || command == "true");
                    0
                };
                let suffix = line
                    .split_once("OW_DONE_")
                    .unwrap()
                    .1
                    .split(':')
                    .next()
                    .unwrap();
                let marker = format!("OW_DONE_{suffix}");
                write!(
                    serial.get_mut(),
                    "{marker}:begin\n\n{marker}:{code}:{marker}\n"
                )
                .unwrap();
            }
        });
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let mut vm = Vm {
            child: child.into(),
            keep: false,
            master: std::os::fd::OwnedFd::from(master).into(),
            log: OpenOptions::new().write(true).open("/dev/null").unwrap(),
            diagnostics: None,
            buffer: Vec::new(),
            socket,
            tap: "clock-fixture-no-host-interface".into(),
            memory_base: MemoryBase::Unknown,
            address: String::new(),
        };
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let result = if capture_failure {
            vm.capture(&root, &root.join("disk.ext4"), true).map(drop)
        } else {
            vm.restore(&root)
        };
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        if capture_failure {
            assert!(result.unwrap_err().to_string().contains("/snapshot/create"));
        } else if date_exit != 0 {
            assert!(format!("{:#}", result.unwrap_err()).contains("synchronizing guest clock"));
        } else {
            result.unwrap();
        }
        let current = clock.load(Ordering::SeqCst);
        if date_exit == 0 {
            assert!(
                (before..=after).contains(&current),
                "stale guest clock {current}"
            );
        } else {
            assert_eq!(current, initial);
        }
        drop(vm);
        guest.join().unwrap();
        api.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restore_resyncs_a_guest_seventeen_minutes_behind() {
        clock_after_resume(false, 0);
    }

    #[test]
    fn restore_rejects_a_failed_guest_clock_update() {
        clock_after_resume(false, 1);
    }

    #[test]
    fn failed_capture_still_resyncs_the_resumed_parent() {
        clock_after_resume(true, 0);
    }
}

/// The command a serial wire line runs, unquoted.
#[cfg(test)]
pub(crate) fn wire_command(line: &str) -> String {
    let quoted = line.split_once("/bin/sh -c ").unwrap().1;
    let quoted = quoted.split_once(" </dev/null & ow_p=").unwrap().0;
    quoted[1..quoted.len() - 1].replace("'\\''", "'")
}

/// A real child and the real capture API/serial protocol, without KVM or host networking.
#[cfg(test)]
pub(crate) fn capture_fixture(directory: &Path, resume: bool) -> (Vm, std::thread::JoinHandle<()>) {
    capture_fixture_steps(directory, resume, vec![CaptureStep::default()])
}

/// As [`capture_fixture`], for one disk-only recovery point.
#[cfg(test)]
pub(crate) fn disk_capture_fixture(directory: &Path) -> (Vm, std::thread::JoinHandle<()>) {
    capture_fixture_steps(
        directory,
        true,
        vec![CaptureStep {
            disk_only: true,
            ..CaptureStep::default()
        }],
    )
}

#[cfg(test)]
#[derive(Default)]
struct CaptureStep {
    snapshot_type: Option<&'static str>,
    resume_fails: bool,
    clock_exit: i32,
    /// A disk-only recovery point: pause and resume, no `/snapshot/create`.
    disk_only: bool,
}

#[cfg(test)]
fn capture_fixture_steps(
    directory: &Path,
    resume: bool,
    steps: Vec<CaptureStep>,
) -> (Vm, std::thread::JoinHandle<()>) {
    use std::{
        io::{BufRead, BufReader},
        os::unix::net::{UnixListener, UnixStream},
    };
    let socket = directory.join("fixture-api.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (master, serial) = UnixStream::pair().unwrap();
    let fixture = std::thread::spawn(move || {
        let mut serial = BufReader::new(serial);
        let mut reply = |prefix: &str, exit: i32| {
            let mut command = String::new();
            if serial.read_line(&mut command).unwrap() == 0 {
                return false;
            }
            let line = command;
            let command = wire_command(&line);
            assert!(command.starts_with(prefix), "{command}");
            let suffix = line
                .split_once("OW_DONE_")
                .unwrap()
                .1
                .split(':')
                .next()
                .unwrap();
            let marker = format!("OW_DONE_{suffix}");
            write!(
                serial.get_mut(),
                "{marker}:begin\n\n{marker}:{exit}:{marker}\n"
            )
            .unwrap();
            true
        };
        for (capture_index, capture) in steps.into_iter().enumerate() {
            if !reply("sync", 0) {
                return;
            }
            let endpoints = if capture.disk_only {
                vec!["/vm", "/vm"]
            } else if resume {
                vec!["/vm", "/snapshot/create", "/vm"]
            } else {
                vec!["/vm", "/snapshot/create"]
            };
            for (step, endpoint) in endpoints.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line.split_whitespace().nth(1), Some(endpoint));
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = n.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap();
                if endpoint == "/snapshot/create" {
                    // A diff goes to its own file and is merged after the guest resumes.
                    let diff = body["mem_file_path"]
                        .as_str()
                        .unwrap()
                        .ends_with("memory.diff");
                    assert_eq!(body["snapshot_type"], if diff { "Diff" } else { "Full" });
                    if let Some(expected) = capture.snapshot_type {
                        assert_eq!(body["snapshot_type"], expected);
                    }
                    std::fs::write(body["snapshot_path"].as_str().unwrap(), b"VM state").unwrap();
                    let memory = body["mem_file_path"].as_str().unwrap();
                    if capture.snapshot_type.is_some() {
                        use std::os::unix::fs::FileExt;
                        let image = File::create(memory).unwrap();
                        image.set_len(3 * 4096).unwrap();
                        if diff {
                            // Dirty log resets on each save: B then C then D, never both.
                            let (page, value) = [(1, b'B'), (2, b'C'), (0, b'D')][capture_index];
                            image.write_all_at(&[value; 4096], page * 4096).unwrap();
                        } else {
                            for (page, value) in [b'A', b'B', b'C'].into_iter().enumerate() {
                                image
                                    .write_all_at(&[value; 4096], page as u64 * 4096)
                                    .unwrap();
                            }
                        }
                    } else {
                        std::fs::write(memory, b"VM memory").unwrap();
                    }
                } else {
                    assert_eq!(body["state"], if step == 0 { "Paused" } else { "Resumed" });
                }
                let status = if step > 0 && endpoint == "/vm" && capture.resume_fails {
                    500
                } else {
                    204
                };
                write!(
                    stream,
                    "HTTP/1.1 {status} Fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
            if (resume || capture.disk_only) && !capture.resume_fails {
                assert!(reply("date -u -s ", capture.clock_exit));
            }
        }
    });
    (
        Vm {
            child: Command::new("sleep").arg("60").spawn().unwrap().into(),
            keep: false,
            master: std::os::fd::OwnedFd::from(master).into(),
            log: OpenOptions::new().write(true).open("/dev/null").unwrap(),
            diagnostics: None,
            buffer: Vec::new(),
            socket,
            tap: "fixture-no-host-interface".into(),
            address: String::new(),
            memory_base: MemoryBase::Unknown,
        },
        fixture,
    )
}

#[cfg(test)]
mod memory_tests {
    use super::*;
    use std::os::unix::fs::FileExt;

    fn scratch() -> PathBuf {
        let root = std::env::temp_dir().join(format!("ow-memory-{}", nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        root
    }

    #[test]
    fn changed_pages_are_laid_over_a_copy_of_the_base_and_holes_keep_it() {
        let root = scratch();
        let base = root.join("base");
        std::fs::write(&base, [b'A'; 3 * 4096]).unwrap();
        let diff = root.join("memory.diff");
        let file = File::create(&diff).unwrap();
        file.set_len(3 * 4096).unwrap();
        file.write_all_at(&[b'B'; 4096], 4096).unwrap();
        let memory = root.join("memory");
        reflink_from(&File::open(&base).unwrap(), &memory).unwrap();
        lay_over(&diff, &memory).unwrap();
        let merged = std::fs::read(&memory).unwrap();
        assert!(merged[..4096].iter().all(|b| *b == b'A'));
        assert!(merged[4096..8192].iter().all(|b| *b == b'B'));
        assert!(merged[8192..].iter().all(|b| *b == b'A'));
        // The base image is never written.
        assert!(std::fs::read(&base).unwrap().iter().all(|b| *b == b'A'));
        std::fs::write(&diff, b"short").unwrap();
        assert!(lay_over(&diff, &memory).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn capture_failure_forces_a_full_retry(failure: &str) {
        let root = scratch();
        let disk = root.join("disk.ext4");
        std::fs::write(&disk, b"disk").unwrap();
        let (mut vm, server) = capture_fixture_steps(
            &root,
            true,
            vec![
                CaptureStep {
                    snapshot_type: Some("Diff"),
                    resume_fails: failure == "resume",
                    clock_exit: if failure == "clock" { 1 } else { 0 },
                    ..Default::default()
                },
                CaptureStep {
                    snapshot_type: Some("Full"),
                    ..Default::default()
                },
                CaptureStep {
                    snapshot_type: Some("Diff"),
                    ..Default::default()
                },
            ],
        );
        let base = root.join("base");
        std::fs::write(&base, [b'A'; 3 * 4096]).unwrap();
        vm.memory_base = MemoryBase::Image(File::open(&base).unwrap());
        let failed = root.join("failed");
        std::fs::create_dir(&failed).unwrap();
        if failure == "disk" {
            // Fail after the RAM save, when creating the disk reflink destination.
            std::fs::create_dir(failed.join("disk.ext4")).unwrap();
        }
        let error = vm.capture(&failed, &disk, true).unwrap_err();
        if failure == "clock" {
            assert!(error.to_string().contains("synchronizing guest clock"));
        }
        assert!(matches!(vm.memory_base, MemoryBase::Unknown));
        // The runtime discards failed staging directories; they cannot become a base.
        std::fs::remove_dir_all(&failed).unwrap();
        let retry = root.join("retry");
        std::fs::create_dir(&retry).unwrap();
        vm.capture(&retry, &disk, true).unwrap();
        let expected: Vec<u8> = [b'A', b'B', b'C']
            .into_iter()
            .flat_map(|b| [b; 4096])
            .collect();
        assert_eq!(std::fs::read(retry.join("memory")).unwrap(), expected);
        assert!(matches!(vm.memory_base, MemoryBase::Image(_)));
        let incremental = root.join("incremental");
        std::fs::create_dir(&incremental).unwrap();
        vm.capture(&incremental, &disk, true).unwrap();
        server.join().unwrap();
        let expected: Vec<u8> = [b'D', b'B', b'C']
            .into_iter()
            .flat_map(|b| [b; 4096])
            .collect();
        assert_eq!(std::fs::read(incremental.join("memory")).unwrap(), expected);
        assert_eq!(std::fs::read(base).unwrap(), [b'A'; 3 * 4096]);
        drop(vm);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_disk_capture_forces_a_full_ram_retry() {
        capture_failure_forces_a_full_retry("disk");
    }

    #[test]
    fn failed_resume_api_forces_a_full_ram_retry() {
        capture_failure_forces_a_full_retry("resume");
    }

    #[test]
    fn failed_resume_clock_update_forces_a_full_ram_retry() {
        capture_failure_forces_a_full_retry("clock");
    }

    #[test]
    fn a_disk_only_point_writes_no_ram_and_keeps_the_next_capture_a_diff() {
        let root = scratch();
        let disk = root.join("disk.ext4");
        std::fs::write(&disk, b"disk").unwrap();
        let (mut vm, server) = disk_capture_fixture(&root);
        let base = root.join("base");
        std::fs::write(&base, b"old pages").unwrap();
        vm.memory_base = MemoryBase::Image(File::open(&base).unwrap());
        let staging = root.join("point");
        std::fs::create_dir(&staging).unwrap();
        // The fixture asserts the API calls: Paused then Resumed, never /snapshot/create.
        let paused = vm.capture_disk(&staging, &disk).unwrap();
        server.join().unwrap();
        assert!(paused < Duration::from_secs(5));
        assert_eq!(std::fs::read(staging.join("disk.ext4")).unwrap(), b"disk");
        assert!(!staging.join("memory").exists() && !staging.join("state").exists());
        // The dirty log was not reset, so the next RAM capture may still save a diff.
        assert!(matches!(vm.memory_base, MemoryBase::Image(_)));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_disk_only_point_resumes_the_guest_when_the_reflink_fails() {
        let root = scratch();
        let disk = root.join("disk.ext4");
        std::fs::write(&disk, b"disk").unwrap();
        let (mut vm, server) = disk_capture_fixture(&root);
        let staging = root.join("point");
        // The clone's destination cannot be created.
        std::fs::create_dir_all(staging.join("disk.ext4")).unwrap();
        // The fixture only finishes after it saw Resumed and the clock resync.
        assert!(vm.capture_disk(&staging, &disk).is_err());
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_repeat_capture_saves_a_diff_and_publishes_a_complete_image() {
        let root = scratch();
        std::fs::write(root.join("disk.ext4"), b"disk").unwrap();
        let (mut vm, server) = capture_fixture(&root, true);
        // As after a load or an earlier capture: the base image the clean pages equal.
        let base = root.join("base");
        std::fs::write(&base, b"old pages").unwrap();
        vm.memory_base = MemoryBase::Image(File::open(&base).unwrap());
        let staging = root.join("capture");
        std::fs::create_dir(&staging).unwrap();
        vm.capture(&staging, &root.join("disk.ext4"), true).unwrap();
        server.join().unwrap();
        assert_eq!(std::fs::read(staging.join("memory")).unwrap(), b"VM memory");
        assert!(!staging.join("memory.diff").exists());
        assert!(matches!(vm.memory_base, MemoryBase::Image(_)));
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod adoption_tests {
    use super::*;

    fn scratch() -> PathBuf {
        let root = std::env::temp_dir().join(format!("ow-adopt-{}", nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        root
    }

    fn pipe() -> (File, File) {
        let mut fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    }

    /// A stand-in VMM: holds a console end at fd 3 and its log pipe's read end at fd 4,
    /// as `Vm::spawn` arranges, writes a diagnostic line, says ready on its console and sleeps.
    fn vmm(console: &File, log: (File, File)) -> Child {
        let (pty, reader) = (console.as_raw_fd(), log.0.as_raw_fd());
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "echo diagnostic >&2; echo ready >&3; exec sleep 60"])
            .stderr(Stdio::from(log.1));
        unsafe {
            command.pre_exec(move || {
                let pty = libc::fcntl(pty, libc::F_DUPFD_CLOEXEC, 10);
                let reader = libc::fcntl(reader, libc::F_DUPFD_CLOEXEC, 10);
                if libc::dup2(pty, PTY_FD) < 0 || libc::dup2(reader, LOG_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().unwrap()
    }

    fn record(directory: &Path, pid: u32, start: u64) {
        let value = json!({"format":RECORD_FORMAT,"boot_id":boot_id().unwrap(),"pid":pid,"start_time":start,"index":7,"tap":"fixture-no-host-interface",
            "address":"198.18.0.30","pty_fd":PTY_FD,"log_fd":LOG_FD,"vmm_sha256":"vmm","kernel_sha256":"kernel"});
        std::fs::write(directory.join(RECORD), value.to_string()).unwrap();
    }

    #[test]
    fn a_process_is_named_by_its_pid_and_start_time() {
        let start = start_time(std::process::id()).unwrap();
        assert_eq!(start_time(std::process::id()), Some(start));
        let directory = scratch();
        record(&directory, std::process::id(), start);
        assert_eq!(recorded_live_pid(&directory), Some(std::process::id()));
        record(&directory, std::process::id(), start + 1);
        assert_eq!(
            recorded_live_pid(&directory),
            None,
            "a reused PID is not the recorded VMM"
        );
        let mut other_boot: Value =
            serde_json::from_slice(&std::fs::read(directory.join(RECORD)).unwrap()).unwrap();
        other_boot["start_time"] = json!(start);
        other_boot["boot_id"] = json!("an-earlier-boot");
        std::fs::write(directory.join(RECORD), other_boot.to_string()).unwrap();
        assert_eq!(
            recorded_live_pid(&directory),
            None,
            "a process of another boot is not it"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_later_worker_takes_the_vmm_fds_and_ends_it_through_its_pidfd() {
        let directory = scratch();
        let (console_read, console_write) = pipe();
        let mut child = vmm(&console_write, pipe());
        drop(console_write);
        let mut ready = [0u8; 6];
        (&console_read).read_exact(&mut ready).unwrap();
        assert_eq!(
            &ready, b"ready\n",
            "its diagnostic line waits in the log pipe"
        );
        let pid = child.id();
        let start = start_time(pid).unwrap();
        record(&directory, pid, start);
        let hash = |_: &Path| Ok("vmm".to_owned());
        let other = |_: &Path| Ok("another".to_owned());
        // Refused: the caller gets a handle to end it with, and nothing signalled it yet.
        assert!(matches!(
            Vm::adopt(&directory, "vmm", "kernel", other).unwrap(),
            Adoption::Refused(reason, process) if reason.contains("another") && process.id() == pid
        ));
        assert!(matches!(
            Vm::adopt(&directory, "vmm", "other-kernel", hash).unwrap(),
            Adoption::Refused(reason, _) if reason.contains("guest kernel")
        ));
        assert!(child.try_wait().unwrap().is_none());
        let Adoption::Adopted(mut vm) = Vm::adopt(&directory, "vmm", "kernel", hash).unwrap()
        else {
            panic!("not adopted");
        };
        assert_eq!(vm.child.id(), pid);
        assert!(vm.child.try_wait().unwrap().is_none());
        // fd 3 of the VMM is now this worker's console.
        vm.master.write_all(b"x").unwrap();
        let mut byte = [0u8; 1];
        (&console_read).read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"x");
        drop(vm);
        // Dropping it ended the process (through the pidfd) and drained its log pipe.
        let status = child.wait().unwrap();
        assert!(!status.success());
        let log = std::fs::read_to_string(directory.join("vmm.log")).unwrap();
        assert!(log.contains("diagnostic"), "{log:?}");
        assert!(
            !directory.join(RECORD).exists(),
            "a reaped VMM's record goes with it"
        );
        record(&directory, pid, start);
        assert!(matches!(
            Vm::adopt(&directory, "vmm", "kernel", hash).unwrap(),
            Adoption::Gone
        ));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_detached_vmm_keeps_running() {
        let directory = scratch();
        let (_console_read, console_write) = pipe();
        let mut child = vmm(&console_write, pipe());
        let pid = child.id();
        record(&directory, pid, start_time(pid).unwrap());
        let Adoption::Adopted(vm) =
            Vm::adopt(&directory, "vmm", "kernel", |_: &Path| Ok("vmm".to_owned())).unwrap()
        else {
            panic!("not adopted");
        };
        vm.detach();
        assert!(
            child.try_wait().unwrap().is_none(),
            "detaching never signals"
        );
        assert!(directory.join(RECORD).exists(), "the next worker finds it");
        child.kill().unwrap();
        child.wait().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[cfg(test)]
mod guest_isolation_tests {
    use super::*;

    #[test]
    fn a_vmm_starts_with_an_oom_score_above_the_worker() {
        let mut command = guest_command(Path::new("cat"));
        command.arg("/proc/self/oom_score_adj");
        unsafe {
            command.pre_exec(|| {
                // Match a worker already holding metadata credentials, including without root.
                if libc::prctl(libc::PR_SET_DUMPABLE, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                as_guest()
            })
        };
        let out = command.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "500");
    }

    #[test]
    fn the_worker_keeps_control_protected_and_guests_are_its_sibling() {
        // Plain directories stand in for the unit's cgroup and its `control` subgroup.
        let unit = std::env::temp_dir().join(format!("ow-cg-{}", nonce().unwrap()));
        std::fs::create_dir_all(unit.join("control")).unwrap();
        let procs = split_cgroup(&unit.join("control")).unwrap();
        let read = |p: &str| std::fs::read_to_string(unit.join(p)).unwrap();
        assert_eq!(read("cgroup.subtree_control"), "+cpu +memory");
        assert_eq!(read("control/cpu.weight"), "1000");
        assert_eq!(read("control/memory.low"), (256u64 << 20).to_string());
        // `control` enables nothing for children: a restarted unit can always join it.
        assert!(!unit.join("control/cgroup.subtree_control").exists());
        assert!(unit.join("guests").is_dir());
        assert_eq!(procs, unit.join("guests/cgroup.procs"));
        std::fs::remove_dir_all(unit).unwrap();
    }
}
