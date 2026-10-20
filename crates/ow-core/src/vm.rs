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

/// Splits the worker's delegated cgroup: the worker moves into `control` (CPU weight
/// 1000, protected memory) and every VMM starts in `guests` (weight 100), so contention
/// and memory reclaim fall on the guests before the worker that controls them.
pub fn isolate_guests(cgroup: &Path) -> Result<()> {
    let procs = split_cgroup(cgroup)?;
    let _ = GUEST_CGROUP.set(std::ffi::CString::new(
        procs.into_os_string().into_encoded_bytes(),
    )?);
    Ok(())
}
fn split_cgroup(cgroup: &Path) -> Result<PathBuf> {
    let (control, guests) = (cgroup.join("control"), cgroup.join("guests"));
    for dir in [&control, &guests] {
        match std::fs::create_dir(dir) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            other => other?,
        }
    }
    // A cgroup that enables controllers for its children may hold no process itself.
    for pid in std::fs::read_to_string(cgroup.join("cgroup.procs"))?.lines() {
        // One that exited meanwhile is gone; any other left behind fails the next write.
        let _ = std::fs::write(control.join("cgroup.procs"), pid);
    }
    std::fs::write(cgroup.join("cgroup.subtree_control"), "+cpu +memory")?;
    std::fs::write(control.join("cpu.weight"), "1000")?;
    std::fs::write(control.join("memory.min"), (64u64 << 20).to_string())?;
    std::fs::write(control.join("memory.low"), (256u64 << 20).to_string())?;
    Ok(guests.join("cgroup.procs"))
}

/// Runs in a forked VMM before exec (async-signal-safe calls only): joins the guests'
/// cgroup when there is one and raises its OOM score, so under memory pressure the kernel
/// kills a VM before the worker (-900 in its unit) or the controller and agent.
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
    write(c"/proc/self/oom_score_adj".as_ptr(), b"500")
}

pub fn reflink(source: &Path, destination: &Path) -> Result<()> {
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
        dst.sync_all()?;
        return Ok(());
    }
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("disk reflink failed");
    }
    dst.sync_all()?;
    Ok(())
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

pub struct Vm {
    pub child: Child,
    master: File,
    log: File,
    diagnostics: Option<std::thread::JoinHandle<()>>,
    buffer: Vec<u8>,
    socket: PathBuf,
    pub tap: String,
    pub address: String,
    memory_base: MemoryBase,
}

impl Vm {
    pub fn spawn(binary: &Path, directory: &Path, index: u32) -> Result<Self> {
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
            let parent_pid = std::process::id();
            let mut command = Command::new(binary);
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
                .stderr(Stdio::piped());
            unsafe {
                command.pre_exec(move || {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::getppid() != parent_pid as i32 {
                        return Err(std::io::Error::other("worker exited during spawn"));
                    }
                    as_guest()
                });
            }
            let mut child = command.spawn()?;
            let mut errors = child.stderr.take().context("VMM stderr missing")?;
            let diagnostic_thread = std::thread::spawn(move || {
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
            });
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
            };
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

    pub fn api(&self, method: &str, endpoint: &str, body: Value) -> Result<()> {
        self.api_waiting(method, endpoint, body, 30)
    }

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
        // Output marker uses a private nonce; this transport supports trusted local workloads only.
        // The begin marker separates this command's output from any a timed-out command left
        // on the channel: read_until discards that, its own end marker included.
        let wire = format!(
            "printf '{marker}:begin\\n'; {command}; ow_rc=$?; printf '\\n{marker}:%s:{marker}\\n' \"$ow_rc\"\n"
        );
        self.master.write_all(wire.as_bytes())?;
        let end = format!(":{marker}");
        let bytes = self.read_until(end.as_bytes(), timeout)?;
        let text = String::from_utf8_lossy(&bytes).replace('\r', "");
        let start = format!("{marker}:begin\n");
        let begin = text.rfind(&start).context("missing begin marker")? + start.len();
        let finish = text
            .rfind(&format!("{marker}:"))
            .context("missing exit marker")?;
        ensure!(begin <= finish, "missing exit marker");
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
        self.api("PUT", "/boot-source", json!({"kernel_image_path":kernel,"boot_args":"console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw init=/init"}))?;
        self.api("PUT", "/drives/rootfs", json!({"drive_id":"rootfs","path_on_host":"disk.ext4","is_root_device":true,"is_read_only":false}))?;
        self.api(
            "PUT",
            "/network-interfaces/net1",
            json!({"iface_id":"net1","host_dev_name":self.tap,"guest_mac":"06:00:00:00:00:01"}),
        )?;
        self.api("PUT", "/actions", json!({"action_type":"InstanceStart"}))?;
        self.read_until(b"OW_GUEST_READY", Duration::from_secs(20))?;
        self.checked("true")?;
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
        self.checked("sync")?;
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
        // Child belongs to this worker; never signal a PID loaded from disk.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(thread) = self.diagnostics.take() {
            let _ = thread.join();
        }
        let _ = host("ip", &["link", "del", &self.tap]);
        if let Ok(index) = self.tap.trim_start_matches("ow").parse() {
            crate::network::unregister(index);
        }
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        os::unix::net::UnixStream,
    };

    fn marker(line: &str) -> String {
        let suffix = line
            .split_once("OW_DONE_")
            .unwrap()
            .1
            .split(':')
            .next()
            .unwrap();
        format!("OW_DONE_{suffix}")
    }

    #[test]
    fn timed_out_command_output_does_not_reach_the_next_command() {
        let (master, guest) = UnixStream::pair().unwrap();
        let guest = std::thread::spawn(move || {
            let mut serial = BufReader::new(guest);
            let mut slow = String::new();
            serial.read_line(&mut slow).unwrap();
            let slow = marker(&slow);
            write!(serial.get_mut(), "{slow}:begin\npartial-from-slow\n").unwrap();
            // The shell reads the next command only after the slow one ends.
            let mut quick = String::new();
            serial.read_line(&mut quick).unwrap();
            assert!(quick.contains("echo next-command"), "{quick}");
            let quick = marker(&quick);
            write!(
                serial.get_mut(),
                "leftover-from-slow\n\n{slow}:0:{slow}\nOW> {quick}:begin\nnext-command\n\n{quick}:0:{quick}\nOW> "
            )
            .unwrap();
        });
        let mut vm = Vm {
            child: Command::new("sleep").arg("30").spawn().unwrap(),
            master: std::os::fd::OwnedFd::from(master).into(),
            log: OpenOptions::new().write(true).open("/dev/null").unwrap(),
            diagnostics: None,
            buffer: Vec::new(),
            socket: PathBuf::new(),
            tap: "command-fixture-no-host-interface".into(),
            memory_base: MemoryBase::Unknown,
            address: String::new(),
        };
        let error = vm
            .command_within(
                "sleep 34; echo leftover-from-slow",
                Duration::from_millis(300),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "guest command timed out after 300ms");
        let (output, code) = vm.command("echo next-command").unwrap();
        assert_eq!((output.as_str(), code), ("next-command\n", 0));
        guest.join().unwrap();
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
                let command = line.split_once("\\n'; ").unwrap().1;
                let command = command.split_once("; ow_rc=").unwrap().0;
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
            child,
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

/// A real child and the real capture API/serial protocol, without KVM or host networking.
#[cfg(test)]
pub(crate) fn capture_fixture(directory: &Path, resume: bool) -> (Vm, std::thread::JoinHandle<()>) {
    capture_fixture_steps(directory, resume, vec![CaptureStep::default()])
}

#[cfg(test)]
#[derive(Default)]
struct CaptureStep {
    snapshot_type: Option<&'static str>,
    resume_fails: bool,
    clock_exit: i32,
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
            let command = command.split_once("\\n'; ").unwrap().1;
            assert!(command.starts_with(prefix), "{command}");
            let suffix = command
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
            if !reply("sync;", 0) {
                return;
            }
            let endpoints = if resume {
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
                let status = if step == 2 && capture.resume_fails {
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
            if resume && !capture.resume_fails {
                assert!(reply("date -u -s ", capture.clock_exit));
            }
        }
    });
    (
        Vm {
            child: Command::new("sleep").arg("60").spawn().unwrap(),
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
mod guest_isolation_tests {
    use super::*;

    #[test]
    fn a_vmm_starts_with_an_oom_score_above_the_worker() {
        let mut command = Command::new("cat");
        command.arg("/proc/self/oom_score_adj");
        unsafe { command.pre_exec(as_guest) };
        let out = command.output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "500");
    }

    #[test]
    fn the_worker_moves_into_control_and_protects_it() {
        // A plain directory stands in for the delegated cgroup: the same writes land.
        let cgroup = std::env::temp_dir().join(format!("ow-cg-{}", nonce().unwrap()));
        std::fs::create_dir(&cgroup).unwrap();
        std::fs::write(cgroup.join("cgroup.procs"), "4242\n").unwrap();
        let procs = split_cgroup(&cgroup).unwrap();
        let read = |p: &str| std::fs::read_to_string(cgroup.join(p)).unwrap();
        assert_eq!(read("control/cgroup.procs"), "4242");
        assert_eq!(read("cgroup.subtree_control"), "+cpu +memory");
        assert_eq!(read("control/cpu.weight"), "1000");
        assert_eq!(read("control/memory.low"), (256u64 << 20).to_string());
        assert!(cgroup.join("guests").is_dir());
        assert_eq!(procs, cgroup.join("guests/cgroup.procs"));
        std::fs::remove_dir_all(cgroup).unwrap();
    }
}
