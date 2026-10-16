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
    time::{Duration, Instant},
};

pub use crate::common::{nonce, shell_quote};

pub fn reflink(source: &Path, destination: &Path) -> Result<()> {
    let src = File::open(source)?;
    let dst = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let rc = unsafe { libc::ioctl(dst.as_raw_fd(), 0x40049409, src.as_raw_fd()) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("disk reflink failed");
    }
    dst.sync_all()?;
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
                    Ok(())
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
        let output = Command::new("curl")
            .args(["-sS", "--max-time", "30", "--unix-socket"])
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
                "guest command timed out; stop/start to recover the serial channel"
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
                return Err(std::io::Error::last_os_error()).context("poll");
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
        ensure!(command.len() < 512 * 1024, "command too large");
        let marker = format!("OW_DONE_{}", nonce()?);
        // Output marker uses a private nonce; this transport supports trusted local workloads only.
        let wire = format!("{command}; ow_rc=$?; printf '\\n{marker}:%s:{marker}\\n' \"$ow_rc\"\n");
        self.master.write_all(wire.as_bytes())?;
        let end = format!(":{marker}");
        let bytes = self.read_until(end.as_bytes(), Duration::from_secs(30))?;
        let text = String::from_utf8_lossy(&bytes).replace('\r', "");
        let begin = text
            .rfind(&format!("{marker}:"))
            .context("missing exit marker")?;
        let code = text[begin + marker.len() + 1..text.len() - end.len()]
            .trim()
            .parse()?;
        let output = text[..begin]
            .trim_start_matches("\n")
            .trim_start_matches("OW> ")
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
            json!({"vcpu_count":cpus,"mem_size_mib":memory}),
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
        Ok(())
    }

    pub fn restore(&mut self, snapshot: &Path) -> Result<()> {
        self.api(
            "PUT",
            "/snapshot/load",
            json!({"snapshot_path":snapshot.join("state"),
            "mem_backend":{"backend_path":snapshot.join("memory"),"backend_type":"File"},
            "network_overrides":[{"iface_id":"net1","host_dev_name":self.tap}],"resume_vm":false,"clock_realtime":true}),
        )?;
        self.api("PATCH", "/vm", json!({"state":"Resumed"}))?;
        self.checked("true")?;
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

    pub fn capture(&mut self, directory: &Path, disk: &Path) -> Result<()> {
        self.checked("sync")?;
        self.api("PATCH", "/vm", json!({"state":"Paused"}))?;
        let captured = (|| {
            self.api("PUT", "/snapshot/create", json!({"snapshot_type":"Full","snapshot_path":directory.join("state"),"mem_file_path":directory.join("memory")}))?;
            reflink(disk, &directory.join("disk.ext4"))
        })();
        let resumed = self.api("PATCH", "/vm", json!({"state":"Resumed"}));
        captured?;
        resumed
    }

    pub fn alive(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
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
