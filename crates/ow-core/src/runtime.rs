use crate::common::{DEFAULT_DISK_GIB, GIB};
use crate::disk;
use crate::vm::{self, Vm, shell_quote};
use crate::{Capabilities, ComputeProvider};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

#[path = "runtime_recovery.rs"]
mod recovery_operations;

const MAX_RUNNING: usize = 8;
const MAX_MEMORY_MIB: u32 = 4096;
fn parse_limit(value: &str, maximum: u32) -> Result<u32> {
    let n = value.parse::<u32>().context("invalid worker limit")?;
    ensure!(
        n > 0 && n <= maximum,
        "invalid worker limit; refusing to relax a configured budget"
    );
    Ok(n)
}
fn limit(name: &str, default: u32, maximum: u32) -> u32 {
    match std::env::var_os(name) {
        None => default,
        Some(value) => value
            .to_str()
            .and_then(|s| parse_limit(s, maximum).ok())
            .unwrap_or(0),
    }
}
fn max_memory() -> u32 {
    limit(
        "OW_MAX_MEMORY_MIB",
        MAX_MEMORY_MIB,
        crate::common::HOST_MEMORY_MIB,
    )
}
fn max_running() -> usize {
    limit(
        "OW_MAX_RUNNING",
        MAX_RUNNING as u32,
        crate::common::HOST_SLOTS,
    ) as usize
}
fn max_cpus() -> u32 {
    limit("OW_MAX_VCPUS", 16, crate::common::HOST_CPUS)
}
/// CPU and RAM the host keeps for its controller and node agent, outside the budgets above
/// (which already exclude it): reported only, so placement output can show it.
fn host_reserve() -> Value {
    let read = |name| std::env::var(name).ok().and_then(|v| v.parse::<u32>().ok());
    match (
        read("OW_HOST_RESERVE_CPU_MILLIS"),
        read("OW_HOST_RESERVE_MEMORY_MIB"),
    ) {
        (Some(cpu), Some(memory)) => json!({"cpu_millis":cpu,"memory_mib":memory}),
        _ => Value::Null,
    }
}
fn minimum_free_gib() -> Result<u32> {
    match std::env::var("OW_MIN_FREE_GIB") {
        Ok(v) => {
            let n = v.parse::<u32>()?;
            ensure!(
                (1..=1024).contains(&n),
                "invalid minimum free disk threshold"
            );
            Ok(n)
        }
        Err(std::env::VarError::NotPresent) => Ok(5),
        Err(e) => Err(e.into()),
    }
}
fn minimum_free_bytes() -> Result<u64> {
    Ok((minimum_free_gib()? as u64 * 1024 * 1024 * 1024).max(128 * 1024 * 1024))
}
fn max_disk_gib() -> Result<u32> {
    match std::env::var("OW_MAX_DISK_GIB") {
        Ok(value) => parse_limit(&value, 1_048_576),
        Err(std::env::VarError::NotPresent) => Ok(1024),
        Err(e) => Err(e.into()),
    }
}
const MAX_SNAPSHOTS: usize = 24;
const MAX_WORKSPACES: usize = 32;

fn default_cpus() -> u32 {
    1
}

fn resources(memory: u32, cpus: u32) -> Result<()> {
    crate::common::resources(memory, cpus)
}

fn default_image() -> String {
    "alpine".into()
}
/// Built-in image profiles, prepared from runtime assets.
pub const PROFILES: [&str; 3] = ["alpine", "arch", "ubuntu"];
/// Golden images (`ow image build`) are built on these developer profiles.
pub const GOLDEN_BASES: [&str; 2] = ["ubuntu", "arch"];

/// A built-in profile or the name of a golden image; whether a golden image is
/// built on this runner is checked when a machine is created from it.
pub fn image_profile(image: &str) -> Result<()> {
    crate::store::image_name(image)
        .map(|_| ())
        .context("unsupported image profile")
}

#[cfg(test)]
mod image_tests {
    use super::*;
    #[test]
    fn worker_limits_fail_closed() {
        for value in ["", "garbage", "0", "4097", "-1"] {
            assert!(parse_limit(value, 4096).is_err());
        }
        assert_eq!(parse_limit("512", 4096).unwrap(), 512);
    }
    #[test]
    fn idle_counters_parse_proc_files() {
        assert!(cpu_ticks(std::process::id()).is_some());
        let dev = "Inter-|   Receive\n face |bytes packets\n   ow1: 1000 5 0 0 0 0 0 0 2000 4 0 0 0 0 0 0\n  ow12: 7 1 0 0 0 0 0 0 9 1 0 0 0 0 0 0\n";
        assert_eq!(tap_bytes(dev, "ow1"), Some(3000));
        assert_eq!(tap_bytes(dev, "ow12"), Some(16));
        assert_eq!(tap_bytes(dev, "ow2"), None);
    }
    #[test]
    fn existing_workspaces_keep_the_alpine_profile() {
        let workspace: Workspace = serde_json::from_value(json!({"id":"existing","index":1,"state":"hibernated","memory_mib":256,"source":null,"hibernation":"saved"})).unwrap();
        assert_eq!(workspace.image, "alpine");
        let snapshot: Snapshot = serde_json::from_value(json!({"name":"saved","workspace":"existing","memory_mib":256,"runtime":"firecracker-v1.17.0","cpu":"cpu","kernel_sha256":"hash","files":{}})).unwrap();
        assert_eq!(snapshot.image, "alpine");
    }
    #[test]
    fn a_capture_from_another_host_kernel_or_cpu_is_not_resumable() {
        let snapshot: Snapshot = serde_json::from_value(json!({"name":"sleep-1","workspace":"box","memory_mib":256,"runtime":"firecracker-v1.17.0","cpu":"model\nflags\nhost-kernel:6.8.0-1","kernel_sha256":"guest","files":{}})).unwrap();
        assert!(snapshot.resumable("model\nflags\nhost-kernel:6.8.0-1", "guest"));
        assert!(
            !snapshot.resumable("model\nflags\nhost-kernel:6.8.0-2", "guest"),
            "replaced host with a new kernel"
        );
        assert!(
            !snapshot.resumable("other\nflags\nhost-kernel:6.8.0-1", "guest"),
            "other CPU"
        );
        assert!(
            !snapshot.resumable("model\nflags\nhost-kernel:6.8.0-1", "other"),
            "other guest kernel"
        );
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub index: u32,
    pub state: String,
    pub memory_mib: u32,
    #[serde(default = "default_cpus")]
    pub vcpu_count: u32,
    #[serde(default = "default_image")]
    pub image: String,
    #[serde(default)]
    pub image_digest: Option<String>,
    pub source: Option<String>,
    pub hibernation: Option<String>,
    #[serde(default)]
    pub managed: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    next_index: u32,
    workspaces: BTreeMap<String, Workspace>,
    #[serde(default)]
    placements: BTreeMap<String, Managed>,
}

impl State {
    fn read(bytes: &[u8]) -> Result<Self> {
        let mut value: Value = serde_json::from_slice(bytes)?;
        if let Some(schema) = value.get("worker_state_schema") {
            ensure!(
                (schema == 2 || schema == 3) && value["next_index"].is_null(),
                "unsupported worker state schema"
            );
            let index = value
                .get("managed_next_index")
                .context("missing managed address allocation")?
                .clone();
            value["next_index"] = index;
            let state: Self = serde_json::from_value(value)?;
            ensure!(
                !state.placements.is_empty(),
                "managed state has lost its fences"
            );
            Ok(state)
        } else {
            let state: Self = serde_json::from_value(value)?;
            ensure!(
                state.placements.is_empty(),
                "managed state is missing its reader guard"
            );
            Ok(state)
        }
    }
    fn value(&self) -> Value {
        let mut value = json!(self);
        if !self.placements.is_empty() {
            value["worker_state_schema"] =
                json!(if self.placements.values().any(|m| m.fork_initialized
                    || m.import.as_ref().is_some_and(|i| i.fork_source.is_some())
                    || m.phase == lifecycle::Phase::Discarded)
                {
                    3
                } else {
                    2
                });
            value["managed_next_index"] = json!(self.next_index);
            // Old workers require a u32 here and must refuse the entire managed registry.
            value["next_index"] = Value::Null;
        }
        value
    }
}

mod lifecycle;
use lifecycle::Managed;

#[derive(Serialize, Deserialize)]
struct Snapshot {
    #[serde(default)]
    captured_at: u64,
    #[serde(default)]
    managed_recovery: bool,
    name: String,
    workspace: String,
    #[serde(default)]
    managed: bool,
    memory_mib: u32,
    #[serde(default = "default_cpus")]
    vcpu_count: u32,
    #[serde(default = "default_image")]
    image: String,
    #[serde(default)]
    image_digest: Option<String>,
    runtime: String,
    cpu: String,
    kernel_sha256: String,
    files: BTreeMap<String, String>,
    /// How `files` were hashed; absent is whole-file SHA-256 (see `snapshot_digest`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    digest: Option<String>,
}

impl Snapshot {
    /// Whether this host can resume the captured RAM state: same runtime, CPU
    /// (including the host kernel) and guest kernel.
    fn resumable(&self, cpu: &str, kernel_sha256: &str) -> bool {
        self.runtime == crate::common::RUNTIME
            && self.cpu == cpu
            && self.kernel_sha256 == kernel_sha256
    }

    fn summary(&self) -> Value {
        json!({"name":self.name,"workspace":self.workspace,"memory_mib":self.memory_mib,"vcpu_count":self.vcpu_count,"image":self.image,"image_digest":self.image_digest,"state":"ready"})
    }
}

#[derive(PartialEq, Eq)]
struct Fingerprint {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    mode: u32,
}

fn fingerprints(directory: &Path) -> Result<Vec<Fingerprint>> {
    let snapshot: Snapshot = serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?;
    std::iter::once("manifest.json")
        .chain(snapshot.files.keys().map(String::as_str))
        .map(|name| {
            let metadata = fs::symlink_metadata(directory.join(name))?;
            ensure!(
                metadata.is_file(),
                "snapshot component must be a regular file"
            );
            Ok(Fingerprint {
                device: metadata.dev(),
                inode: metadata.ino(),
                size: metadata.len(),
                modified: (metadata.mtime(), metadata.mtime_nsec()),
                changed: (metadata.ctime(), metadata.ctime_nsec()),
                mode: metadata.mode(),
            })
        })
        .collect()
}

pub use crate::common::identifier;

pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension(format!("tmp-{}", vm::nonce()?));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(path.parent().context("missing parent")?)?.sync_all()?;
    Ok(())
}

fn hash(path: &Path) -> Result<String> {
    let output = Command::new("sha256sum").arg(path).output()?;
    ensure!(output.status.success(), "hash failed");
    Ok(String::from_utf8(output.stdout)?
        .split_whitespace()
        .next()
        .context("missing hash")?
        .to_owned())
}

fn install_file(machine: &mut Vm, binary: &Path, guest: &str, digest: &str) -> Result<()> {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::{Duration, Instant},
    };
    let address = machine.address.clone();
    let mut octets: [u8; 4] = address.parse::<std::net::Ipv4Addr>()?.octets();
    octets[3] -= 1;
    let host = std::net::Ipv4Addr::from(octets);

    let bytes = fs::read(binary)?;
    ensure!(
        bytes.len() < 8 * 1024 * 1024,
        "guest asset exceeds installation limit"
    );
    let listener = TcpListener::bind((host, 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let expected = address.clone();
    let transfer = std::thread::spawn(move || -> Result<()> {
        let started = Instant::now();
        loop {
            ensure!(
                started.elapsed() < Duration::from_secs(10),
                "agent transfer timeout"
            );
            match listener.accept() {
                Ok((mut stream, peer)) if peer.ip().to_string() == expected => {
                    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                    let mut headers = Vec::new();
                    while !headers.ends_with(b"\r\n\r\n") {
                        ensure!(headers.len() < 4096, "agent transfer request too large");
                        let mut byte = [0];
                        stream.read_exact(&mut byte)?;
                        headers.push(byte[0]);
                    }
                    write!(
                        stream,
                        "HTTP/1.0 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    )?;
                    stream.write_all(&bytes)?;
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(e) => return Err(e.into()),
            }
        }
    });
    let installation = machine.checked(&format!("umask 077; wget -q -O {guest}.tmp http://{host}:{port}/ && test \"$(sha256sum {guest}.tmp | cut -d ' ' -f 1)\" = {digest} && chmod 700 {guest}.tmp && mv {guest}.tmp {guest}"));
    let transmitted = transfer
        .join()
        .map_err(|_| anyhow::anyhow!("agent transfer thread failed"))?;
    installation?;
    transmitted?;
    Ok(())
}

const SLEEP: &str = "sleep-";
const RECOVERY: &str = "sleep-recovery-";

/// utime + stime of a process, in clock ticks.
fn cpu_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command name; utime and stime are fields 14 and 15.
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    Some(fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?)
}

/// Received plus sent bytes of one interface, from `/proc/net/dev` text.
fn tap_bytes(dev: &str, tap: &str) -> Option<u64> {
    let line = dev
        .lines()
        .find_map(|l| l.trim_start().strip_prefix(tap)?.strip_prefix(':'))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .map(|f| f.parse().ok())
        .collect::<Option<_>>()?;
    Some(fields.first()? + fields.get(8)?)
}

pub const BACKEND: &str = "firecracker";
/// Firecracker supports every flag: paired snapshots, live forks, hibernation, exec and SSH.
pub const CAPABILITIES: Capabilities = Capabilities::ALL;

impl ComputeProvider for Runtime {
    fn backend(&self) -> &'static str {
        BACKEND
    }
    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }
    fn call(&mut self, request: &Value) -> Result<Value> {
        self.request(request)
    }
}

pub struct Runtime {
    pub root: PathBuf,
    assets: PathBuf,
    state: State,
    machines: BTreeMap<String, Vm>,
    cpu: String,
    kernel_hash: String,
    verified: BTreeMap<String, Vec<Fingerprint>>,
    pub idle: crate::idle::Idle,
    /// Set by `quiesce` when the worker is going down: no machine may start
    /// (a wake would consume a hibernation the shutdown just captured).
    closing: bool,
    /// Completed capture in this process; never persist Captured before its files are synced.
    capture_ready: BTreeMap<String, bool>,
    #[cfg(test)]
    crash: Option<&'static str>,
}

impl Runtime {
    pub fn ssh(&mut self, id: &str) -> Result<(std::net::TcpStream, String)> {
        self.require_unmanaged(id)?;
        self.ssh_request(&json!({"op":"ssh","id":id}))
    }
    pub fn ssh_request(&mut self, request: &Value) -> Result<(std::net::TcpStream, String)> {
        let id = request["id"].as_str().context("missing ID")?;
        self.authorize(request, true)?;
        identifier(id)?;
        // Waking first: resuming replaces the stream generation in the policy.
        self.wake(id)?;
        let policy = crate::ssh_guest::load(&self.root, id)?;
        ensure!(
            policy.ready && !policy.keys.is_empty(),
            "SSH has no active registered keys"
        );
        let address = self.address_authorized(id)?;
        let tcp = std::net::TcpStream::connect_timeout(
            &format!("{address}:22").parse()?,
            std::time::Duration::from_secs(5),
        )?;
        tcp.set_nodelay(true)?;
        Ok((tcp, policy.generation))
    }
    /// A dedicated guest PTY; never attach an interactive client to management serial.
    pub fn terminal(&mut self, id: &str) -> Result<std::net::TcpStream> {
        self.require_unmanaged(id)?;
        self.terminal_request(&json!({"op":"terminal","id":id}))
    }
    pub fn terminal_request(&mut self, request: &Value) -> Result<std::net::TcpStream> {
        use std::{net::TcpStream, time::Duration};
        let id = request["id"].as_str().context("missing ID")?;
        self.authorize(request, true)?;
        identifier(id)?;
        let address = self.address_authorized(id)?;
        let mut host: std::net::Ipv4Addr = address.parse()?;
        let mut octets = host.octets();
        octets[3] -= 1;
        host = octets.into();
        let binary = self.assets.join("guest/ow-guest");
        let digest = hash(&binary)?;
        let guest = format!("/run/ow-terminal-{digest}");
        let machine = self.running(id)?;
        machine.checked(
            "mkdir -p /dev/pts; mountpoint -q /dev/pts || mount -t devpts devpts /dev/pts",
        )?;
        let (_, installed) = machine.command(&format!(
            "test -f {guest} && test \"$(sha256sum {guest} | cut -d ' ' -f 1)\" = {digest}"
        ))?;
        if installed != 0 {
            install_file(machine, &binary, &guest, &digest)?;
        }
        let ready = format!("/run/ow-pty-{}", vm::nonce()?);
        let launch = format!(
            "umask 077; {guest} {address} {host} {ready} </dev/null >/dev/null 2>&1 & for i in $(seq 1 100); do if [ -s {ready} ]; then cat {ready}; rm {ready}; break; fi; sleep 0.05; done"
        );
        let output = machine.checked(&format!("/bin/sh -c {}", shell_quote(&launch)))?;
        let port: u16 = output
            .trim()
            .parse()
            .context("guest PTY readiness failed")?;
        let tcp = TcpStream::connect_timeout(
            &format!("{address}:{port}").parse()?,
            Duration::from_secs(5),
        )?;
        tcp.set_nodelay(true)?;
        Ok(tcp)
    }
    pub fn new(root: PathBuf, assets: PathBuf) -> Result<Self> {
        for (name, maximum) in [
            ("OW_MAX_MEMORY_MIB", crate::common::HOST_MEMORY_MIB),
            ("OW_MAX_RUNNING", crate::common::HOST_SLOTS),
            ("OW_MAX_VCPUS", crate::common::HOST_CPUS),
        ] {
            if let Some(value) = std::env::var_os(name) {
                parse_limit(value.to_str().context("invalid worker limit")?, maximum)?;
            }
        }
        minimum_free_gib()?;
        max_disk_gib()?;
        for directory in ["machines", "snapshots", "recovery"] {
            fs::create_dir_all(root.join(directory))?;
        }
        let mut state: State = if root.join("state.json").exists() {
            State::read(&fs::read(root.join("state.json"))?)?
        } else {
            State::default()
        };
        Self::recover_state(&mut state)?;
        let cpuinfo = fs::read_to_string("/proc/cpuinfo")?;
        let model = cpuinfo
            .lines()
            .find(|line| line.starts_with("model name"))
            .context("missing CPU model")?;
        let flags = cpuinfo
            .lines()
            .find(|line| line.starts_with("flags"))
            .context("missing CPU flags")?;
        let cpu = format!(
            "{model}\n{flags}\nhost-kernel:{}",
            fs::read_to_string("/proc/sys/kernel/osrelease")?.trim()
        );
        let kernel_hash = hash(&assets.join("downloads/vmlinux-6.1.186"))?;
        let runtime = Self {
            root,
            assets,
            state,
            machines: BTreeMap::new(),
            cpu,
            kernel_hash,
            verified: BTreeMap::new(),
            idle: crate::idle::Idle::from_env()?,
            closing: false,
            capture_ready: BTreeMap::new(),
            #[cfg(test)]
            crash: None,
        };
        runtime.cleanup_pending_snapshots()?;
        runtime.save()?;
        Ok(runtime)
    }

    fn save(&self) -> Result<()> {
        atomic_json(&self.root.join("state.json"), &self.state.value())
    }
    fn machine_dir(&self, id: &str) -> PathBuf {
        self.root.join("machines").join(id)
    }
    fn snapshot_dir(&self, name: &str) -> PathBuf {
        self.root.join("snapshots").join(name)
    }
    fn binary(&self) -> PathBuf {
        self.assets
            .join("official/release-v1.17.0-x86_64/firecracker-v1.17.0-x86_64")
    }
    fn kernel(&self) -> PathBuf {
        self.assets.join("downloads/vmlinux-6.1.186")
    }

    fn capacity(&self, memory: u32, cpus: u32) -> Result<()> {
        ensure!(
            self.machines.len() < max_running(),
            "running workspace limit ({MAX_RUNNING}) reached"
        );
        let used: u32 = self
            .machines
            .keys()
            .map(|id| self.state.workspaces[id].memory_mib)
            .sum();
        let used_cpus: u32 = self
            .machines
            .keys()
            .map(|id| self.state.workspaces[id].vcpu_count)
            .sum();
        ensure!(
            used_cpus + cpus <= max_cpus(),
            "CPU reservation limit (16 vCPUs) reached"
        );
        ensure!(
            used + memory <= max_memory(),
            "memory reservation limit ({MAX_MEMORY_MIB} MiB) reached"
        );
        Ok(())
    }

    fn workspace(&self, id: &str) -> Result<Workspace> {
        identifier(id)?;
        self.state
            .workspaces
            .get(id)
            .cloned()
            .context("workspace not found")
    }

    fn running(&mut self, id: &str) -> Result<&mut Vm> {
        identifier(id)?;
        let vm = self
            .machines
            .get_mut(id)
            .context("workspace is not running; use start")?;
        ensure!(vm.alive(), "VMM exited; stop/start to recover");
        Ok(vm)
    }

    pub fn address(&mut self, id: &str) -> Result<String> {
        self.require_unmanaged(id)?;
        self.address_authorized(id)
    }

    pub fn address_request(&mut self, request: &Value) -> Result<String> {
        self.authorize(request, true)?;
        self.address_authorized(request["id"].as_str().context("missing ID")?)
    }

    fn address_authorized(&mut self, id: &str) -> Result<String> {
        self.wake(id)?;
        Ok(self.running(id)?.address.clone())
    }

    /// Wake on connect: a connection or command resumes a hibernated machine.
    /// A stopped machine stays stopped.
    fn wake(&mut self, id: &str) -> Result<()> {
        self.require_active(id)?;
        if !self.machines.contains_key(id) && self.workspace(id)?.state == "hibernated" {
            self.start(id).context("waking hibernated workspace")?;
        }
        Ok(())
    }

    /// Hibernates running machines that have been idle for the configured
    /// timeout (`OW_IDLE_HIBERNATE_SECS`). Returns each attempt's result.
    pub fn hibernate_idle(&mut self) -> Vec<(String, Result<Value>)> {
        if !self.idle.enabled() {
            return Vec::new();
        }
        let net = fs::read_to_string("/proc/self/net/dev").unwrap_or_default();
        let now = std::time::Instant::now();
        let mut due = Vec::new();
        for (id, machine) in &self.machines {
            let (Some(cpu), Some(bytes)) =
                (cpu_ticks(machine.child.id()), tap_bytes(&net, &machine.tap))
            else {
                continue;
            };
            if self.idle.due(id, cpu, bytes, now) {
                due.push(id.clone());
            }
        }
        due.into_iter()
            .map(|id| {
                let mut request = json!({"op":"hibernate","id":id});
                if let Some(managed) = self.state.placements.get(&id) {
                    request["placement"] = json!(managed.authority);
                }
                let result = self.request(&request);
                if result.is_err() {
                    // Retry after another full timeout, not every check.
                    self.idle.touch(&id, now);
                }
                (id, result)
            })
            .collect()
    }

    fn spawn(&mut self, workspace: &Workspace, snapshot: Option<&Path>, fork: bool) -> Result<()> {
        ensure!(
            !self.closing,
            "the worker is shutting down; no machine can start"
        );
        self.capacity(workspace.memory_mib, workspace.vcpu_count)?;
        if snapshot.is_none() {
            disk::grow_filesystem(&self.machine_dir(&workspace.id).join("disk.ext4"))?;
        }
        let mut machine = Vm::spawn(
            &self.binary(),
            &self.machine_dir(&workspace.id),
            workspace.index,
        )?;
        if let Some(snapshot) = snapshot {
            machine.restore(snapshot)?;
        } else {
            machine.boot(&self.kernel(), workspace.memory_mib, workspace.vcpu_count)?;
        }
        machine.network(&workspace.id, workspace.index, fork)?;
        let profile = crate::store::image_name(&workspace.image)?.0;
        if profile != "alpine" {
            let bundle = self.assets.join("guest/network-tools.tar.gz");
            let digest = hash(&bundle)?;
            let marker = format!("/ow/.network-{digest}");
            if machine.command(&format!("test -f {marker}"))?.1 != 0 {
                let guest = format!("/run/ow-network-{digest}.tar.gz");
                install_file(&mut machine, &bundle, &guest, &digest)?;
                machine.checked(&format!("tar -xzf {guest} -C / && mkdir -p /etc/ssl/certs && {{ test -f /etc/ssl/certs/ca-certificates.crt || cp /ow/etc/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt; }} && touch {marker}; rm -f {guest}"))?;
                if profile == "arch" {
                    machine.checked("if ! grep -q '^Server' /etc/pacman.d/mirrorlist; then printf 'Server = https://geo.mirror.pkgbuild.com/$repo/os/$arch\\n' >> /etc/pacman.d/mirrorlist; fi")?;
                }
            }
        }
        if profile != "alpine" {
            machine.checked("chmod 755 /etc/ssl /etc/ssl/certs /ow /ow/bin /ow/lib /ow/etc; chmod 644 /etc/ssl/certs/ca-certificates.crt")?;
        }
        if crate::ssh_guest::path(&self.root, &workspace.id).exists() {
            let mut policy = crate::ssh_guest::load(&self.root, &workspace.id)?;
            policy.generation = vm::nonce()?;
            policy.ready = false;
            atomic_json(
                &crate::ssh_guest::path(&self.root, &workspace.id),
                &json!(policy),
            )?;
            crate::ssh_guest::apply(&mut machine, &policy)?;
            policy.ready = true;
            atomic_json(
                &crate::ssh_guest::path(&self.root, &workspace.id),
                &json!(policy),
            )?;
        }
        self.machines.insert(workspace.id.clone(), machine);
        self.idle.touch(&workspace.id, std::time::Instant::now());
        Ok(())
    }

    fn disk_reserved(&self) -> Result<u64> {
        crate::cache::logical_disks(&self.root)
    }

    fn reserve_disk(&self, additional: u64) -> Result<()> {
        ensure!(
            self.disk_reserved()?
                .checked_add(additional)
                .context("disk reservation overflow")?
                <= u64::from(max_disk_gib()?) * GIB,
            "host disk cap exhausted (OW_MAX_DISK_GIB)"
        );
        // Shared reflink extents cannot establish exclusive ownership: reserve full sizes.
        self.storage_headroom(
            self.disk_reserved()?
                .checked_add(additional)
                .context("disk reservation overflow")?,
        )
    }

    fn storage_available(&self) -> Result<u64> {
        crate::cache::available(&self.root)
    }

    fn disk_headroom(&self) -> Result<u64> {
        Ok((u64::from(max_disk_gib()?) * GIB)
            .saturating_sub(self.disk_reserved()?)
            .min(
                self.storage_available()?
                    .saturating_sub(minimum_free_bytes()?)
                    .saturating_sub(crate::cache::staging_bytes(&self.root)?)
                    .saturating_sub(self.disk_reserved()?),
            ))
    }

    fn storage_headroom(&self, extra: u64) -> Result<()> {
        let reserve = minimum_free_bytes()?;
        let staging = crate::cache::staging_bytes(&self.root)?;
        let required = extra
            .checked_add(reserve)
            .and_then(|n| n.checked_add(staging))
            .context("disk reservation overflow")?;
        let available = self.storage_available()?;
        ensure!(
            available >= required,
            "storage headroom exhausted: {available} bytes available, {required} bytes required"
        );
        Ok(())
    }

    fn workspace_summary(&self, id: &str) -> Result<Value> {
        let workspace = self.workspace(id)?;
        let mut summary = json!(&workspace);
        let usage = disk::summary(&self.machine_dir(id).join("disk.ext4")).unwrap_or_else(
            |_| json!({"disk_bytes":null,"disk_used_bytes":null,"disk_allocated_bytes":null}),
        );
        summary
            .as_object_mut()
            .unwrap()
            .extend(usage.as_object().unwrap().clone());
        let path = self.root.join("recovery").join(format!("{id}.json"));
        if path.exists() {
            let recovery: crate::recovery::Recovery = serde_json::from_slice(&fs::read(path)?)?;
            summary["durability"] = recovery.summary(crate::recovery::now());
            let sealed_sleep = workspace.state == "hibernated"
                && recovery
                    .durable
                    .as_ref()
                    .is_some_and(|p| p.source.is_some() && p.source == workspace.hibernation);
            summary["durability"]["current_state_durable"] = json!(sealed_sleep);
            if sealed_sleep {
                summary["durability"]["overdue"] = json!(false);
            }
        }
        Ok(summary)
    }
    fn create(
        &mut self,
        id: &str,
        memory: u32,
        cpus: u32,
        image: &str,
        source: Option<String>,
        snapshot: Option<&str>,
        disk_gib: Option<u32>,
    ) -> Result<Value> {
        identifier(id)?;
        image_profile(image)?;
        resources(memory, cpus)?;
        if let Some(existing) = self.state.workspaces.get(id) {
            ensure!(
                existing.memory_mib == memory
                    && existing.vcpu_count == cpus
                    && existing.source == source
                    && existing.image == image
                    && disk_gib.is_none_or(|n| self
                        .machine_dir(id)
                        .join("disk.ext4")
                        .metadata()
                        .is_ok_and(|m| m.len() == u64::from(n) * GIB)),
                "ID already exists with different parameters"
            );
            return self.workspace_summary(id);
        }
        self.capacity(memory, cpus)?;
        self.storage_headroom(0)?;
        ensure!(
            self.workspace_count() < MAX_WORKSPACES,
            "workspace limit ({MAX_WORKSPACES}) reached"
        );
        ensure!(
            self.state.next_index < 8192,
            "prototype network address allocation exhausted"
        );
        let directory = self.machine_dir(id);
        ensure!(
            !directory.exists(),
            "unregistered workspace directory exists; preserve and inspect it"
        );
        let source_disk = if let Some(name) = snapshot {
            self.snapshot_dir(name).join("disk.ext4")
        } else if !PROFILES.contains(&image) {
            let disk = self.root.join("images").join(image).join("disk.ext4");
            ensure!(
                disk.is_file(),
                "image {image} is not built on this runner; run ow image build"
            );
            disk
        } else {
            self.assets.join(if image == "alpine" {
                "guest/base.ext4".to_string()
            } else {
                format!("guest/{image}.ext4")
            })
        };
        ensure!(
            source_disk.is_file(),
            "image profile is not prepared; run prepare-profiles.py first"
        );
        let source_bytes = source_disk.metadata()?.len();
        let disk_bytes = disk_gib.map(|n| u64::from(n) * GIB).unwrap_or(source_bytes);
        ensure!(
            disk_bytes >= source_bytes,
            "requested disk is smaller than the image or snapshot"
        );
        self.reserve_disk(disk_bytes)?;
        if snapshot.is_none() && !PROFILES.contains(&image) {
            crate::cache::verify_image(&self.root, image)?;
        }
        crate::cache::touch(&self.root, image)?;
        fs::create_dir(&directory)?;
        vm::reflink(&source_disk, &directory.join("disk.ext4"))?;
        disk::extend(&directory.join("disk.ext4"), disk_bytes)?;
        let image_digest = if let Some(name) = snapshot {
            serde_json::from_slice::<Snapshot>(&fs::read(
                self.snapshot_dir(name).join("manifest.json"),
            )?)?
            .image_digest
        } else if !PROFILES.contains(&image) {
            let manifest: Value = serde_json::from_slice(&fs::read(
                self.root.join("images").join(image).join("manifest.json"),
            )?)?;
            manifest["image_digest"].as_str().map(str::to_owned)
        } else {
            None
        };
        if let Some(digest) = &image_digest {
            crate::store::valid_digest(digest)?;
        }
        let workspace = Workspace {
            id: id.into(),
            index: self.state.next_index,
            state: "starting".into(),
            memory_mib: memory,
            vcpu_count: cpus,
            image: image.into(),
            image_digest,
            source,
            hibernation: None,
            managed: false,
        };
        self.state.next_index += 1;
        self.state.workspaces.insert(id.into(), workspace.clone());
        self.save()?;
        let snapshot_path = snapshot.map(|name| self.snapshot_dir(name));
        let result = self.spawn(&workspace, snapshot_path.as_deref(), snapshot.is_some());
        self.state.workspaces.get_mut(id).unwrap().state =
            if result.is_ok() { "running" } else { "failed" }.into();
        self.save()?;
        result?;
        self.workspace_summary(id)
    }

    fn stop(&mut self, id: &str) -> Result<Value> {
        self.workspace(id)?;
        let sync_error = self
            .machines
            .get_mut(id)
            .and_then(|machine| machine.checked("sync").err())
            .map(|e| format!("{e:#}"));
        self.machines.remove(id);
        let workspace = self.state.workspaces.get_mut(id).unwrap();
        workspace.state = "stopped".into();
        workspace.hibernation = None;
        self.save()?;
        Ok(json!({"workspace":self.state.workspaces[id],"sync_error":sync_error}))
    }

    fn delete(&mut self, id: &str) -> Result<Value> {
        self.workspace(id)?;
        ensure!(
            !self.machines.contains_key(id),
            "workspace is running; stop or hibernate it first"
        );
        // Its snapshots go too: a later workspace reusing this ID must not fork them.
        let mut snapshots = Vec::new();
        for item in fs::read_dir(self.root.join("snapshots"))? {
            let directory = item?.path();
            let manifest = directory.join("manifest.json");
            if !manifest.exists() {
                continue;
            }
            let snapshot: Snapshot = serde_json::from_slice(&fs::read(manifest)?)?;
            if snapshot.workspace == id {
                fs::remove_dir_all(&directory)?;
                self.verified.remove(&snapshot.name);
                snapshots.push(snapshot.name);
            }
        }
        // Files first, record last: an interrupted delete can simply be repeated.
        match fs::remove_dir_all(self.machine_dir(id)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            result => result?,
        }
        match fs::remove_file(self.root.join("recovery").join(format!("{id}.json"))) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            result => result?,
        }
        self.state.workspaces.remove(id);
        self.save()?;
        Ok(json!({"deleted":id,"snapshots":snapshots}))
    }

    /// Deletes one named snapshot of `id`. Repeating a finished delete succeeds
    /// (`existed` false); hibernation, recovery and lifecycle captures are refused.
    fn delete_snapshot(&mut self, id: &str, name: &str) -> Result<Value> {
        identifier(name)?;
        self.workspace(id)?;
        ensure!(
            !name.starts_with(SLEEP),
            "only named snapshots can be deleted"
        );
        ensure!(
            !self.lifecycle_capture(name),
            "snapshot name is reserved by a lifecycle operation"
        );
        let directory = self.snapshot_dir(name);
        if !directory.join("manifest.json").is_file() {
            ensure!(
                !directory.exists(),
                "incomplete snapshot exists; preserve and inspect it"
            );
            return Ok(json!({"deleted":name,"workspace":id,"existed":false}));
        }
        let snapshot: Snapshot =
            serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?;
        ensure!(
            snapshot.workspace == id,
            "snapshot belongs to another workspace"
        );
        ensure!(
            !snapshot.managed && !snapshot.managed_recovery,
            "only named snapshots can be deleted"
        );
        fs::remove_dir_all(&directory)?;
        self.verified.remove(name);
        File::open(self.root.join("snapshots"))?.sync_all()?;
        Ok(json!({"deleted":name,"workspace":id,"existed":true}))
    }

    fn start(&mut self, id: &str) -> Result<Value> {
        self.require_active(id)?;
        let workspace = self.workspace(id)?;
        if self.machines.contains_key(id) {
            return Ok(json!(workspace));
        }
        // A box hibernated on another host kernel or CPU (a replaced host) cannot
        // resume its RAM; it boots its hibernated disk cold instead.
        let mut resume = true;
        let snapshot = match workspace.hibernation.as_deref() {
            Some(name) => {
                let manifest: Snapshot = serde_json::from_slice(&fs::read(
                    self.snapshot_dir(name).join("manifest.json"),
                )?)?;
                ensure!(
                    workspace.managed || !manifest.managed,
                    "managed capture requires import-prepare"
                );
                resume = manifest.resumable(&self.cpu, &self.kernel_hash);
                ensure!(
                    resume || !workspace.managed,
                    "managed RAM capture is incompatible; explicit cold preparation required"
                );
                if !resume {
                    eprintln!(
                        "{id} was hibernated on another host kernel or CPU; booting its disk cold"
                    );
                }
                Some(self.verify_snapshot(name, resume)?)
            }
            None => None,
        };
        if let Some(ref directory) = snapshot {
            let replacement = self
                .machine_dir(id)
                .join(format!("disk-{}.ext4", vm::nonce()?));
            vm::reflink(&directory.join("disk.ext4"), &replacement)?;
            fs::rename(replacement, self.machine_dir(id).join("disk.ext4"))?;
        }
        let fork = self.state.placements.get(id).is_some_and(|m| {
            !m.fork_initialized && m.import.as_ref().is_some_and(|i| i.fork_source.is_some())
        });
        self.spawn(&workspace, snapshot.as_deref().filter(|_| resume), fork)?;
        if fork {
            self.state.placements.get_mut(id).unwrap().fork_initialized = true;
        }
        let workspace = self.state.workspaces.get_mut(id).unwrap();
        workspace.state = "running".into();
        let slept = workspace.hibernation.take();
        self.save()?;
        // A resumed hibernation capture is spent; keeping it would leak one per sleep.
        // The running VMM keeps its mapped memory file until it exits.
        if let Some(name) = slept.filter(|n| n.starts_with(SLEEP) && !self.lifecycle_capture(n))
            && let Some(directory) = snapshot
        {
            self.verified.remove(&name);
            if let Err(e) = fs::remove_dir_all(&directory) {
                eprintln!("removing spent hibernation {name}: {e}");
            }
        }
        self.workspace_summary(id)
    }

    /// Seals a RAM+disk snapshot. With `hibernate` the paused VM is dropped right after
    /// the capture, so nothing the guest does while the files are hashed is lost.
    fn capture(&mut self, id: &str, name: &str, hibernate: bool) -> Result<Value> {
        self.capture_inner(id, name, hibernate, false, None)
    }

    fn capture_inner(
        &mut self,
        id: &str,
        name: &str,
        hibernate: bool,
        managed: bool,
        previous: Option<&Path>,
    ) -> Result<Value> {
        let captured_at = crate::recovery::now();
        identifier(name)?;
        ensure!(
            !self.lifecycle_capture(name),
            "snapshot name is reserved by a lifecycle operation"
        );
        let workspace = self.workspace(id)?;
        let destination = self.snapshot_dir(name);
        if destination.join("manifest.json").exists() {
            let previous: Snapshot =
                serde_json::from_slice(&fs::read(destination.join("manifest.json"))?)?;
            ensure!(
                previous.workspace == id,
                "snapshot name belongs to another workspace"
            );
            return Ok(previous.summary());
        }
        // A capture reflinks an existing disk; only RAM is a new data allocation.
        self.storage_headroom(workspace.memory_mib as u64 * 1024 * 1024)?;
        if !hibernate && !managed {
            ensure!(
                self.named_snapshot_count()? < MAX_SNAPSHOTS,
                "snapshot limit ({MAX_SNAPSHOTS}) reached"
            );
        }
        ensure!(
            !destination.exists(),
            "incomplete snapshot exists; preserve and inspect it"
        );
        self.running(id)?;
        let staging = self.snapshot_dir(&format!(".pending-{}", vm::nonce()?));
        fs::create_dir(&staging)?;
        let result = (|| {
            let disk = self.machine_dir(id).join("disk.ext4");
            self.running(id)?.capture(&staging, &disk, !hibernate)?;
            if hibernate {
                self.machines.remove(id);
                // Until the capture is sealed, the live disk is the machine's state.
                self.state.workspaces.get_mut(id).unwrap().state = "stopped".into();
                self.save()?;
            }
            // Recovery points are chunk-sealed so unchanged reflinked extents are not reread.
            let (files, digest, stats) = if managed {
                let (files, stats) = crate::snapshot_digest::seal_directory(
                    &staging,
                    &crate::snapshot_store::DATA_FILES,
                    previous,
                )?;
                (
                    files,
                    Some(crate::snapshot_digest::CHUNKS_V1.into()),
                    Some(stats),
                )
            } else {
                let mut files = BTreeMap::new();
                for file in ["state", "memory", "disk.ext4"] {
                    files.insert(file.into(), hash(&staging.join(file))?);
                }
                (files, None, None)
            };
            let snapshot = Snapshot {
                digest,
                managed_recovery: managed,
                captured_at,
                name: name.into(),
                workspace: id.into(),
                managed: workspace.managed,
                memory_mib: workspace.memory_mib,
                vcpu_count: workspace.vcpu_count,
                image: workspace.image,
                image_digest: workspace.image_digest,
                runtime: crate::common::RUNTIME.into(),
                cpu: self.cpu.clone(),
                kernel_sha256: self.kernel_hash.clone(),
                files,
            };
            atomic_json(&staging.join("manifest.json"), &snapshot)?;
            fs::rename(&staging, &destination)?;
            for file in ["state", "memory", "disk.ext4", "manifest.json"] {
                fs::set_permissions(destination.join(file), fs::Permissions::from_mode(0o400))?;
            }
            File::open(self.root.join("snapshots"))?.sync_all()?;
            self.verified
                .insert(name.into(), fingerprints(&destination)?);
            let mut summary = snapshot.summary();
            if let Some(stats) = stats {
                summary["chunks"] = json!(stats);
            }
            Ok(summary)
        })();
        if result.is_err() && staging.exists() {
            if let Err(error) = fs::remove_dir_all(&staging) {
                eprintln!("removing failed snapshot staging: {error:#}");
            }
        }
        result
    }

    fn named_snapshot_count(&self) -> Result<usize> {
        let mut count = 0;
        for entry in fs::read_dir(self.root.join("snapshots"))? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with('.')
                && !name.starts_with(SLEEP)
                && entry.path().join("manifest.json").is_file()
            {
                count += 1;
            }
        }
        Ok(count)
    }

    // Called only at startup: the exclusive worker lock and dead VM children make
    // unpublished staging directories unreachable by any running capture or guest.
    fn cleanup_pending_snapshots(&self) -> Result<()> {
        let preserved: Vec<_> = self
            .state
            .placements
            .values()
            .filter_map(|m| m.intent.as_ref())
            .map(|i| format!(".pending-export-{}", i.capture))
            .collect();
        for entry in fs::read_dir(self.root.join("snapshots"))? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with(".pending-")
                && entry.file_type()?.is_dir()
                && !preserved
                    .iter()
                    .any(|name| entry.file_name() == name.as_str())
            {
                fs::remove_dir_all(entry.path()).context("removing abandoned snapshot staging")?;
            }
        }
        Ok(())
    }

    fn validate_snapshot(&mut self, name: &str, resume: bool) -> Result<PathBuf> {
        let directory = self.verify_snapshot(name, resume)?;
        let snapshot: Snapshot =
            serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?;
        ensure!(!snapshot.managed, "managed capture requires import-prepare");
        Ok(directory)
    }

    /// Checks a snapshot's files against its manifest and, when its RAM state will be
    /// resumed, that this host's runtime, CPU and guest kernel match its capture.
    fn verify_snapshot(&mut self, name: &str, resume: bool) -> Result<PathBuf> {
        identifier(name)?;
        let directory = self.snapshot_dir(name);
        let snapshot: Snapshot =
            serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?;
        ensure!(snapshot.name == name, "snapshot manifest name mismatch");
        ensure!(
            !resume || snapshot.resumable(&self.cpu, &self.kernel_hash),
            "snapshot runtime, CPU or guest kernel is incompatible; restore it with --cold to boot its disk"
        );
        let expected = if resume {
            vec!["disk.ext4", "memory", "state"]
        } else {
            vec!["disk.ext4"]
        };
        ensure!(
            snapshot
                .files
                .keys()
                .all(|k| ["disk.ext4", "memory", "state"].contains(&k.as_str()))
                && expected.iter().all(|k| snapshot.files.contains_key(*k))
                && (snapshot.files.len() == 1 || snapshot.files.len() == 3),
            "snapshot manifest is incomplete"
        );
        let before = fingerprints(&directory)?;
        if self.verified.get(name) == Some(&before) {
            return Ok(directory);
        }
        for file in snapshot.files.keys() {
            let path = directory.join(file);
            let actual = match snapshot.digest.as_deref() {
                None => hash(&path)?,
                Some(crate::snapshot_digest::CHUNKS_V1) => crate::snapshot_digest::root(
                    &crate::snapshot_digest::seal(&path, None)?.0.chunks,
                ),
                Some(other) => bail!("unsupported snapshot digest {other}"),
            };
            ensure!(
                snapshot.files.get(file) == Some(&actual),
                "snapshot integrity failure: {file}"
            );
        }
        ensure!(
            before == fingerprints(&directory)?,
            "snapshot changed during integrity verification"
        );
        self.verified.insert(name.into(), before);
        Ok(directory)
    }

    /// Brings up a machine from a snapshot copied onto this runner (for example by
    /// `ow backup restore`), as the same machine ID it was captured from: its RAM
    /// state resumed, or with `cold` its disk booted fresh (for a host whose CPU or
    /// kernel differs).
    fn import(&mut self, name: &str, cold: bool) -> Result<Value> {
        let directory = self.validate_snapshot(name, !cold)?;
        let snapshot: Snapshot =
            serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?;
        let id = snapshot.workspace.clone();
        ensure!(
            !self.state.placements.contains_key(&id),
            "managed ID requires import-prepare"
        );
        identifier(&id)?;
        ensure!(
            !self.state.workspaces.contains_key(&id),
            "workspace {id} already exists on this runner; start or delete it"
        );
        ensure!(
            self.workspace_count() < MAX_WORKSPACES,
            "workspace limit ({MAX_WORKSPACES}) reached"
        );
        ensure!(
            self.state.next_index < 8192,
            "prototype network address allocation exhausted"
        );
        self.reserve_disk(directory.join("disk.ext4").metadata()?.len())?;
        let machine = self.machine_dir(&id);
        ensure!(
            !machine.exists(),
            "unregistered workspace directory exists; preserve and inspect it"
        );
        fs::create_dir(&machine)?;
        vm::reflink(&directory.join("disk.ext4"), &machine.join("disk.ext4"))?;
        let workspace = Workspace {
            id: id.clone(),
            index: self.state.next_index,
            state: if cold { "stopped" } else { "hibernated" }.into(),
            memory_mib: snapshot.memory_mib,
            vcpu_count: snapshot.vcpu_count,
            image: snapshot.image,
            image_digest: snapshot.image_digest,
            source: None,
            hibernation: (!cold).then(|| name.to_owned()),
            managed: false,
        };
        self.state.next_index += 1;
        self.state.workspaces.insert(id.clone(), workspace);
        self.save()?;
        self.start(&id)
    }

    /// Golden images built on this runner: `images/NAME/{disk.ext4,manifest.json}`.
    fn images(&self) -> Result<Vec<Value>> {
        let mut images = Vec::new();
        let Ok(entries) = fs::read_dir(self.root.join("images")) else {
            return Ok(images);
        };
        for entry in entries {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path().join("manifest.json");
            if path.is_file() {
                images.push(serde_json::from_slice(&fs::read(path)?)?);
            }
        }
        images.sort_by(|a: &Value, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(images)
    }

    /// Promotes a stopped builder machine's disk to golden image `name`, replacing
    /// an older build of it. Machines already created from the old build keep their
    /// own disks.
    fn save_image(&mut self, id: &str, name: &str, recipe_sha256: &str) -> Result<Value> {
        identifier(name)?;
        ensure!(
            !PROFILES.contains(&name) && name != "ubuntu-arm64",
            "{name} is a built-in image"
        );
        ensure!(
            recipe_sha256.len() == 64 && recipe_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid recipe hash"
        );
        let builder = self.workspace(id)?;
        ensure!(
            !self.machines.contains_key(id) && builder.state == "stopped",
            "stop the builder machine before saving its disk"
        );
        ensure!(
            GOLDEN_BASES.contains(&crate::store::image_name(&builder.image)?.0),
            "golden images are built on {GOLDEN_BASES:?}"
        );
        // Runners advertise every golden image to the controller, which takes 32.
        ensure!(
            self.images()?.iter().filter(|i| i["name"] != name).count() < 32,
            "golden image limit (32) reached"
        );
        let images = self.root.join("images");
        fs::create_dir_all(&images)?;
        let staging = images.join(format!(".pending-{}", vm::nonce()?));
        fs::create_dir(&staging)?;
        let disk = staging.join("disk.ext4");
        vm::reflink(&self.machine_dir(id).join("disk.ext4"), &disk)?;
        let manifest = json!({"name":name,"base":builder.image,"recipe_sha256":recipe_sha256,
            "disk_sha256":hash(&disk)?,"built_unix":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs()});
        atomic_json(&staging.join("manifest.json"), &manifest)?;
        fs::set_permissions(&disk, fs::Permissions::from_mode(0o400))?;
        let target = images.join(name);
        // shortcut: a create during the two renames sees no image and fails; retry it.
        let old = images.join(format!(".old-{}", vm::nonce()?));
        if target.exists() {
            fs::rename(&target, &old)?;
        }
        fs::rename(&staging, &target)?;
        File::open(&images)?.sync_all()?;
        if old.exists() {
            fs::remove_dir_all(&old)?;
        }
        Ok(manifest)
    }

    pub fn request(&mut self, request: &Value) -> Result<Value> {
        let op = request["op"].as_str().context("missing operation")?;
        let _cache = crate::cache::lock(&self.root, op == "cache-evict")?;
        let _space = crate::cache::space_lock(&self.root)?;
        let exited: Vec<_> = self
            .machines
            .iter_mut()
            .filter_map(|(id, vm)| (!vm.alive()).then_some(id.clone()))
            .collect();
        for id in &exited {
            self.machines.remove(id);
            self.state.workspaces.get_mut(id).unwrap().state = "failed".into();
        }
        if !exited.is_empty() {
            self.save()?;
        }
        let op = request["op"].as_str().context("missing operation")?;
        let id = request["id"].as_str().unwrap_or("");
        if crate::common::PLACEMENT_OPERATIONS.contains(&op) {
            return self.lifecycle_request(request);
        }
        if !id.is_empty() && !matches!(op, "inspect" | "placement-check") {
            self.authorize(request, true)?;
            ensure!(
                !self.state.placements.contains_key(id)
                    || !matches!(op, "delete" | "restore" | "fork" | "image-save"),
                "managed operation requires lifecycle coordination"
            );
        }
        if !id.is_empty() && !matches!(op, "hibernate" | "recovery-prepare" | "recovery-result") {
            self.idle.touch(id, std::time::Instant::now());
        }
        if matches!(op, "exec" | "shell-exec" | "put" | "get" | "ssh-keys") {
            self.wake(id)?;
        }
        match op {
            "cache-inventory" => Ok(
                json!({"entries":crate::cache::inventory(&self.root)?,"available_bytes":self.storage_available()?,"protected_bytes":self.disk_reserved()?.saturating_add(minimum_free_bytes()?).saturating_add(u64::from(max_memory())*1024*1024),"staging_bytes":crate::cache::staging_bytes(&self.root)?}),
            ),
            "cache-evict" => {
                let url = fs::read_to_string(self.root.join("store-url"))
                    .ok()
                    .map(|s| s.trim().to_owned())
                    .or_else(|| {
                        fs::read("/etc/open-workspaces/store.json")
                            .ok()
                            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                            .and_then(|v| v["url"].as_str().map(str::to_owned))
                    })
                    .context("runner store is not configured")?;
                let objects = crate::store::S3Objects::new(&url, None)?;
                let claim = request["claim"]
                    .as_str()
                    .context("eviction claim required")?;
                let reference: crate::store::Reference =
                    serde_json::from_value(request["reference"].clone())?;
                crate::cache::evict_claim(
                    &self.root,
                    claim,
                    request["name"].as_str().context("cache name required")?,
                    &reference,
                    &objects,
                )
            }
            "status" => Ok(
                json!({"runtime":"Firecracker v1.17.0","running":self.machines.len(),
                "version":env!("CARGO_PKG_VERSION"),"worker_protocol":crate::common::WORKER_PROTOCOL,
                // What this worker writes into captures and requires to resume one (upgrade gate).
                "compat":{"runtime":crate::common::RUNTIME,"cpu":self.cpu,"kernel_sha256":self.kernel_hash},
                "worker_pid":std::process::id(),"placement_fencing_v1":true,"move_fork_v1":true,"recovery_points_v1":true,"snapshot_delete_v1":true,"guest_ssh_v1":true,"disk_resize_v1":true,"max_disk_gib":max_disk_gib()?,"reserved_disk_bytes":self.disk_reserved()?,
                "available_disk_bytes":self.disk_headroom().ok(),"disk_observed_unix_s":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs(),"workspace_count":self.workspace_count(),
                "cache_eviction_v1":true,"image_store_v1":true,"image_digests":crate::cache::inventory(&self.root)?.iter().map(|i| json!({"name":i.name,"digest":i.digest})).collect::<Vec<_>>(),
                "host_managed":std::env::var("OW_HOST_MANAGED").as_deref()==Ok("1"),"host_asset_hash":std::env::var("OW_HOST_ASSET_HASH").unwrap_or_default(),"min_free_gib":minimum_free_gib()?,"host_reserve":host_reserve(),"max_running":max_running(),"max_memory_mib":max_memory(),"max_vcpus":max_cpus(),"max_workspaces":MAX_WORKSPACES,"max_guest_memory_mib":16384,"max_guest_vcpus":16,
                "max_snapshots":MAX_SNAPSHOTS,"images":self.images()?.iter().map(|i| i["name"].clone()).collect::<Vec<_>>(),"internet":crate::network::enabled(),"network":"filtered rootless IPv4 Internet egress; LAN, host and peer access blocked","prototype":true}),
            ),
            "list" => Ok(json!(
                self.state
                    .workspaces
                    .keys()
                    .map(|id| self.placement_summary(id))
                    .collect::<Result<Vec<_>>>()?
            )),
            "quiesce" => {
                // `"closing": false` lifts it again (a shutdown that was called off).
                self.closing = request["closing"].as_bool().unwrap_or(true);
                Ok(json!({"closing":self.closing}))
            }
            "stats" => {
                let mut running = BTreeMap::new();
                let mut total_pss = 0u64;
                let mut reserved = 0u32;
                for (id, machine) in &self.machines {
                    let text =
                        fs::read_to_string(format!("/proc/{}/smaps_rollup", machine.child.id()))?;
                    let mut fields = BTreeMap::new();
                    for line in text.lines() {
                        if let Some((key, value)) = line.split_once(':')
                            && ["Rss", "Pss", "Private_Dirty"].contains(&key)
                        {
                            fields.insert(
                                key.to_owned(),
                                value
                                    .split_whitespace()
                                    .next()
                                    .context("missing memory value")?
                                    .parse::<u64>()?,
                            );
                        }
                    }
                    let pss = fields.get("Pss").copied().unwrap_or(0);
                    let memory = self.state.workspaces[id].memory_mib;
                    total_pss += pss;
                    reserved += memory;
                    running.insert(id.clone(), json!({"reserved_mib":memory,"pss_kib":pss,
                        "rss_kib":fields.get("Rss"),"private_dirty_kib":fields.get("Private_Dirty")}));
                }
                Ok(
                    json!({"running":running,"total_pss_kib":total_pss,"reserved_memory_mib":reserved,
                    "limit_memory_mib":max_memory(),"note":"PSS is process memory; excludes additional host page cache and kernel overhead"}),
                )
            }
            "inspect" => self.placement_summary(id),
            "placement-check" => {
                self.check_replay(request)?;
                Ok(json!({}))
            }
            "recovery-prepare" | "recovery-result" => self.recovery_operation(request),
            "create" => self.create(
                id,
                u32::try_from(request["memory_mib"].as_u64().unwrap_or(256))?,
                u32::try_from(request["vcpu_count"].as_u64().unwrap_or(1))?,
                request["image"].as_str().unwrap_or("alpine"),
                None,
                None,
                Some(crate::common::requested_disk(request)?.unwrap_or(DEFAULT_DISK_GIB)),
            ),
            "resize" => {
                let existing = self.workspace(id)?;
                let mut shape = request.clone();
                for (key, value) in [
                    ("memory_mib", existing.memory_mib),
                    ("vcpu_count", existing.vcpu_count),
                ] {
                    if shape.get(key).is_none() {
                        shape[key] = json!(value);
                    }
                }
                let (memory, cpus) = crate::common::requested_resources(&shape)?;
                let size = crate::common::requested_disk(request)?;
                ensure!(
                    request.get("memory_mib").is_some()
                        || request.get("vcpu_count").is_some()
                        || size.is_some(),
                    "specify memory_mib, vcpu_count or disk_gib"
                );
                ensure!(
                    !self.machines.contains_key(id) && existing.state == "stopped",
                    "stop the machine before resizing; hibernated checkpoints cannot be resized"
                );
                if let Some(size) = size {
                    let path = self.machine_dir(id).join("disk.ext4");
                    let current = path.metadata()?.len();
                    let bytes = u64::from(size) * GIB;
                    ensure!(bytes >= current, "disk shrinking is not supported");
                    if bytes > current {
                        self.reserve_disk(bytes - current)?;
                        disk::extend(&path, bytes)?;
                    }
                }
                let workspace = self.state.workspaces.get_mut(id).unwrap();
                workspace.memory_mib = memory;
                workspace.vcpu_count = cpus;
                self.save()?;
                self.workspace_summary(id)
            }
            "start" => self.start(id),
            "images" => Ok(json!(self.images()?)),
            "image-save" => self.save_image(
                id,
                request["name"].as_str().context("missing image name")?,
                request["recipe_sha256"]
                    .as_str()
                    .context("missing recipe hash")?,
            ),
            "import" => self.import(
                request["snapshot"]
                    .as_str()
                    .context("missing snapshot name")?,
                request["cold"].as_bool().unwrap_or(false),
            ),
            "stop" => self.stop(id),
            "delete" => self.delete(id),
            "ssh-info" => Ok(crate::ssh_guest::info(&crate::ssh_guest::load(
                &self.root, id,
            )?)),
            "ssh-policy" => Ok(json!({"enrolled":crate::ssh_guest::load(&self.root,id)?})),
            "ssh-keys" => {
                let keys = crate::ssh_guest::validate_keys(&request["keys"])?;
                let root = self.root.clone();
                let upgrade = request["upgrade"].as_bool().unwrap_or(false);
                crate::ssh_guest::enroll(self.running(id)?, &root, id, keys, upgrade)
            }
            "exec" => {
                let command = request["command"].as_str().context("missing command")?;
                let quoted = format!(
                    "OW_COMMAND={}; if [ -f /etc/ow-developer ]; then su - dev -c \"$OW_COMMAND\"; else /bin/sh -c \"$OW_COMMAND\"; fi",
                    shell_quote(command)
                );
                ensure!(
                    quoted.len() <= 2800,
                    "prototype commands are limited to 2800 bytes after shell quoting"
                );
                let (output, code) = self.running(id)?.command(&quoted)?;
                Ok(json!({"output":output,"exit_code":code}))
            }
            "shell-exec" => {
                let command = request["command"].as_str().context("missing command")?;
                ensure!(
                    command.len() <= 2800,
                    "prototype shell commands are limited to 2800 bytes"
                );
                let (output, code) = self.running(id)?.command(command)?;
                Ok(json!({"output":output,"exit_code":code}))
            }
            "put" => {
                let path = request["path"].as_str().context("missing guest path")?;
                ensure!(
                    !path.contains('\n') && path.len() < 500,
                    "invalid guest path"
                );
                let data = STANDARD.decode(request["data"].as_str().context("missing data")?)?;
                ensure!(
                    data.len() <= 256 * 1024,
                    "prototype uploads limited to 256 KiB"
                );
                let staging = format!("{path}.ow-{}", vm::nonce()?);
                let machine = self.running(id)?;
                machine.checked(&format!("umask 077; : > {}", shell_quote(&staging)))?;
                for chunk in data.chunks(1024) {
                    machine.checked(&format!(
                        "printf %s {} | base64 -d >> {}",
                        shell_quote(&STANDARD.encode(chunk)),
                        shell_quote(&staging)
                    ))?;
                }
                machine.checked(&format!(
                    "if [ -f /etc/ow-developer ]; then chown 1000:1000 {}; fi; mv -- {} {}; sync",
                    shell_quote(&staging),
                    shell_quote(&staging),
                    shell_quote(path)
                ))?;
                Ok(json!({"bytes":data.len(),"path":path}))
            }
            "get" => {
                let path = request["path"].as_str().context("missing guest path")?;
                ensure!(
                    !path.contains('\n') && path.len() < 500,
                    "invalid guest path"
                );
                let begin = format!("OW_FILE_{}", vm::nonce()?);
                let output = self.running(id)?.checked(&format!("test -f {p} && test $(wc -c < {p}) -le 262144 && printf '\\n{begin}\\n' && base64 {p}", p=shell_quote(path)))?;
                let data = output.split_once(&begin).context("missing file marker")?.1;
                let clean: String = data.chars().filter(|c| !c.is_whitespace()).collect();
                STANDARD.decode(&clean).context("invalid file response")?;
                Ok(json!({"data":clean}))
            }
            "snapshot" => {
                let name = request["name"].as_str().context("missing snapshot name")?;
                ensure!(
                    !name.starts_with(RECOVERY),
                    "sleep-recovery- is reserved for managed recovery points"
                );
                self.capture(id, name, false)
            }
            "snapshot-delete" => {
                let name = request["name"].as_str().context("missing snapshot name")?;
                self.delete_snapshot(id, name)
            }
            "snapshots" => {
                let mut snapshots = Vec::<Snapshot>::new();
                for item in fs::read_dir(self.root.join("snapshots"))? {
                    let item = item?;
                    let path = item.path().join("manifest.json");
                    // Hibernation captures are internal to their machine.
                    if path.exists()
                        && !item.file_name().to_string_lossy().starts_with('.')
                        && !item.file_name().to_string_lossy().starts_with(SLEEP)
                    {
                        let snapshot: Snapshot = serde_json::from_slice(&fs::read(path)?)?;
                        if !snapshot.managed_recovery {
                            snapshots.push(snapshot);
                        }
                    }
                }
                Ok(json!(
                    snapshots.iter().map(Snapshot::summary).collect::<Vec<_>>()
                ))
            }
            "fork" => {
                let child = request["child"].as_str().context("missing child ID")?;
                identifier(child)?;
                let parent = self.workspace(id)?;
                if let Some(existing) = self.state.workspaces.get(child) {
                    ensure!(
                        existing.source.as_deref() == Some(id),
                        "child ID already exists with another source"
                    );
                    return Ok(json!(existing));
                }
                self.capacity(parent.memory_mib, parent.vcpu_count)?;
                let supplied = request["snapshot"].as_str();
                let generated = format!("fork-{}", vm::nonce()?);
                let name = supplied.unwrap_or(&generated);
                if supplied.is_none() {
                    self.capture(id, name, false)?;
                }
                let directory = self.validate_snapshot(name, true)?;
                let snapshot: Snapshot =
                    serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?;
                ensure!(
                    snapshot.workspace == id,
                    "snapshot belongs to another workspace"
                );
                self.create(
                    child,
                    snapshot.memory_mib,
                    snapshot.vcpu_count,
                    &snapshot.image,
                    Some(id.into()),
                    Some(name),
                    None,
                )
            }
            "hibernate" => {
                let workspace = self.workspace(id)?;
                if workspace.state == "hibernated" {
                    return Ok(json!(workspace));
                }
                let name = format!("{SLEEP}{}", vm::nonce()?);
                self.capture(id, &name, true)?;
                let workspace = self.state.workspaces.get_mut(id).unwrap();
                workspace.state = "hibernated".into();
                workspace.hibernation = Some(name);
                self.save()?;
                self.workspace_summary(id)
            }
            "restore" => {
                let name = request["name"].as_str().context("missing snapshot name")?;
                let directory = self.validate_snapshot(name, true)?;
                let snapshot: Snapshot =
                    serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?;
                let workspace = self.workspace(id)?;
                ensure!(
                    snapshot.workspace == id
                        && snapshot.memory_mib == workspace.memory_mib
                        && snapshot.vcpu_count == workspace.vcpu_count
                        && snapshot.image == workspace.image
                        && directory.join("disk.ext4").metadata()?.len()
                            == self.machine_dir(id).join("disk.ext4").metadata()?.len(),
                    "snapshot belongs to another workspace"
                );
                self.stop(id)?;
                let workspace = self.state.workspaces.get_mut(id).unwrap();
                workspace.state = "hibernated".into();
                workspace.hibernation = Some(name.into());
                self.save()?;
                self.start(id)
            }
            _ => bail!("unsupported operation: {op}"),
        }
    }

    pub fn shutdown(&mut self) {
        let ids: Vec<_> = self.machines.keys().cloned().collect();
        for id in ids {
            let _ = self.stop(&id);
        }
        self.machines.clear();
    }
}

#[cfg(test)]
mod disk_tests {
    use super::*;

    pub(super) fn fixture() -> Runtime {
        let root = std::env::temp_dir().join(format!("ow-disk-runtime-{}", vm::nonce().unwrap()));
        fs::create_dir_all(root.join("machines/legacy")).unwrap();
        fs::create_dir_all(root.join("snapshots")).unwrap();
        File::create(root.join("machines/legacy/disk.ext4"))
            .unwrap()
            .set_len(8 * GIB)
            .unwrap();
        let workspace: Workspace = serde_json::from_value(json!({"id":"legacy","index":1,"state":"stopped","memory_mib":1024,"vcpu_count":2,"source":null,"hibernation":null})).unwrap();
        let mut state = State::default();
        state.workspaces.insert("legacy".into(), workspace);
        Runtime {
            root,
            assets: PathBuf::new(),
            state,
            machines: BTreeMap::new(),
            cpu: String::new(),
            kernel_hash: String::new(),
            verified: BTreeMap::new(),
            idle: crate::idle::Idle::from_env().unwrap(),
            closing: false,
            capture_ready: BTreeMap::new(),
            crash: None,
        }
    }

    #[test]
    fn disk_discovery_headroom_counts_stopped_logical_reservations() {
        let mut runtime = fixture();
        assert!(
            runtime.disk_headroom().unwrap()
                <= (u64::from(max_disk_gib().unwrap()) * GIB).saturating_sub(8 * GIB)
        );
        File::options()
            .write(true)
            .open(runtime.machine_dir("legacy").join("disk.ext4"))
            .unwrap()
            .set_len(u64::from(max_disk_gib().unwrap()) * GIB)
            .unwrap();
        let status = runtime.request(&json!({"op":"status"})).unwrap();
        assert_eq!(status["available_disk_bytes"], 0);
        assert_eq!(
            status["reserved_disk_bytes"],
            u64::from(max_disk_gib().unwrap()) * GIB
        );
        assert!(status["disk_observed_unix_s"].as_u64().unwrap() > 0);
        assert!(runtime.reserve_disk(1).is_err());
        fs::remove_dir_all(&runtime.root).unwrap();
    }

    #[test]
    fn recovery_pins_sleep_retries_after_restart_and_rotates_only_owned_points() {
        use crate::recovery::{Recovery, now};
        let mut runtime = fixture();
        fs::create_dir(runtime.root.join("recovery")).unwrap();
        let sleep = runtime.snapshot_dir("sleep-fixture");
        fs::create_dir(&sleep).unwrap();
        let mut files = BTreeMap::new();
        for file in ["state", "memory", "disk.ext4"] {
            fs::write(sleep.join(file), file).unwrap();
            files.insert(file.to_owned(), hash(&sleep.join(file)).unwrap());
        }
        let snapshot = Snapshot {
            managed_recovery: false,
            captured_at: now() - 601,
            name: "sleep-fixture".into(),
            workspace: "legacy".into(),
            managed: false,
            image_digest: None,
            memory_mib: 1024,
            vcpu_count: 2,
            image: "alpine".into(),
            runtime: crate::common::RUNTIME.into(),
            cpu: "old host".into(),
            kernel_sha256: "old kernel".into(),
            files,
            digest: None,
        };
        atomic_json(&sleep.join("manifest.json"), &snapshot).unwrap();
        let workspace = runtime.state.workspaces.get_mut("legacy").unwrap();
        workspace.state = "hibernated".into();
        workspace.hibernation = Some("sleep-fixture".into());
        let store = "a".repeat(64);
        assert!(
            runtime
                .request(&json!({"op":"recovery-prepare","id":"legacy","store":store}))
                .unwrap()
                .is_null()
        );
        let journal = runtime.root.join("recovery/legacy.json");
        let mut observed: Recovery = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
        observed.sleep_observed_at = now() - 601;
        atomic_json(&journal, &observed).unwrap();
        let point = runtime
            .request(&json!({"op":"recovery-prepare","id":"legacy","store":store}))
            .unwrap();
        let name = point["snapshot"].as_str().unwrap();
        assert_eq!(point["captured_at"], snapshot.captured_at);
        assert_eq!(runtime.named_snapshot_count().unwrap(), 0);
        assert_eq!(
            runtime.request(&json!({"op":"snapshots"})).unwrap(),
            json!([])
        );
        // Wake may remove the original while an upload owns the pinned inode set.
        fs::remove_dir_all(&sleep).unwrap();
        let workspace = runtime.state.workspaces.get_mut("legacy").unwrap();
        workspace.state = "stopped".into();
        workspace.hibernation = None;
        runtime.request(&json!({"op":"recovery-result","id":"legacy","store":store,"snapshot":name,"uploaded":false})).unwrap();
        let journal = runtime.root.join("recovery/legacy.json");
        let mut saved: Recovery = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
        assert_eq!(saved.failures, 1);
        assert!(saved.error.is_some());
        assert!(saved.durable.is_none());
        saved.attempted_at = now() - 301;
        atomic_json(&journal, &saved).unwrap();
        let retry = runtime
            .request(&json!({"op":"recovery-prepare","id":"legacy","store":store}))
            .unwrap();
        assert_eq!(
            (&retry["snapshot"], &retry["captured_at"]),
            (&point["snapshot"], &point["captured_at"]),
            "restart captured a different point"
        );
        assert!(retry.get("chunks").is_none(), "a sealed pin was rehashed");
        assert_eq!(point["chunks"]["read"], 3);
        let pinned: Snapshot = serde_json::from_slice(
            &fs::read(runtime.snapshot_dir(name).join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            pinned.digest.as_deref(),
            Some(crate::snapshot_digest::CHUNKS_V1)
        );
        assert!(runtime.snapshot_dir(name).join("chunks.json").is_file());
        runtime.verified.clear();
        runtime.verify_snapshot(name, false).unwrap();
        assert!(runtime.request(&json!({"op":"recovery-result","id":"legacy","store":store,"snapshot":"other","uploaded":true})).is_err());
        for old in ["sleep-recovery-old", "manual", "recovery-manual"] {
            fs::create_dir(runtime.snapshot_dir(old)).unwrap();
            let mut old_snapshot: Value = serde_json::to_value(&snapshot).unwrap();
            old_snapshot["name"] = json!(old);
            old_snapshot["managed_recovery"] = json!(old == "sleep-recovery-old");
            atomic_json(
                &runtime.snapshot_dir(old).join("manifest.json"),
                &old_snapshot,
            )
            .unwrap();
        }
        let ack = json!({"op":"recovery-result","id":"legacy","store":store,"snapshot":name,"uploaded":true});
        runtime.request(&ack).unwrap();
        runtime.request(&ack).unwrap(); // lost-response replay
        assert!(!runtime.snapshot_dir("sleep-recovery-old").exists());
        assert!(runtime.snapshot_dir("manual").exists());
        assert!(runtime.snapshot_dir("recovery-manual").exists());
        assert_eq!(runtime.named_snapshot_count().unwrap(), 2);
        assert!(runtime.snapshot_dir(name).exists());
        let summary = runtime.workspace_summary("legacy").unwrap();
        assert_eq!(
            summary["durability"]["last_durable_at"],
            snapshot.captured_at
        );
        assert!(summary["durability"]["error"].is_null());
        let workspace = runtime.state.workspaces.get_mut("legacy").unwrap();
        workspace.state = "hibernated".into();
        workspace.hibernation = Some("sleep-fixture".into());
        assert_eq!(
            runtime.workspace_summary("legacy").unwrap()["durability"]["current_state_durable"],
            true
        );
        let workspace = runtime.state.workspaces.get_mut("legacy").unwrap();
        workspace.state = "stopped".into();
        workspace.hibernation = None;

        // A different destination cannot inherit the old store's durable claim.
        runtime
            .request(&json!({"op":"recovery-prepare","id":"legacy","store":"b".repeat(64)}))
            .unwrap();
        assert!(
            runtime.workspace_summary("legacy").unwrap()["durability"]["last_durable_at"].is_null()
        );
        fs::remove_dir_all(runtime.root).unwrap();
    }

    #[test]
    fn one_named_snapshot_can_be_deleted_and_internal_captures_cannot() {
        let mut runtime = fixture();
        let seal = |runtime: &Runtime, name: &str, workspace: &str, managed: bool| {
            let directory = runtime.snapshot_dir(name);
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("disk.ext4"), b"disk").unwrap();
            let snapshot = Snapshot {
                managed_recovery: false,
                captured_at: 1,
                name: name.into(),
                workspace: workspace.into(),
                managed,
                memory_mib: 1024,
                vcpu_count: 2,
                image: "alpine".into(),
                image_digest: None,
                runtime: crate::common::RUNTIME.into(),
                cpu: String::new(),
                kernel_sha256: String::new(),
                files: [(
                    "disk.ext4".into(),
                    hash(&directory.join("disk.ext4")).unwrap(),
                )]
                .into(),
                digest: None,
            };
            atomic_json(&directory.join("manifest.json"), &snapshot).unwrap();
        };
        seal(&runtime, "keep", "legacy", false);
        seal(&runtime, "drop", "legacy", false);
        seal(&runtime, "theirs", "other", false);
        seal(&runtime, "placed", "legacy", true);
        seal(&runtime, "sleep-x", "legacy", false);
        runtime.verify_snapshot("drop", false).unwrap();
        let delete = |name: &str| json!({"op":"snapshot-delete","id":"legacy","name":name});
        assert_eq!(
            runtime.request(&delete("drop")).unwrap(),
            json!({"deleted":"drop","workspace":"legacy","existed":true})
        );
        assert!(!runtime.snapshot_dir("drop").exists());
        assert!(!runtime.verified.contains_key("drop"));
        // A lost response is answered again; the name is simply gone.
        assert_eq!(runtime.request(&delete("drop")).unwrap()["existed"], false);
        for name in ["theirs", "placed", "sleep-x", "../keep"] {
            assert!(runtime.request(&delete(name)).is_err(), "deleted {name}");
        }
        assert!(runtime.snapshot_dir("theirs").exists() && runtime.snapshot_dir("placed").exists());
        fs::create_dir(runtime.snapshot_dir("partial")).unwrap();
        assert!(runtime.request(&delete("partial")).is_err());
        assert!(runtime.snapshot_dir("keep").join("manifest.json").exists());
        assert_eq!(
            runtime.request(&json!({"op":"status"})).unwrap()["snapshot_delete_v1"],
            true
        );
        fs::remove_dir_all(runtime.root).unwrap();
    }

    #[test]
    fn recovery_replaces_an_unpinned_sleep_source_after_wake() {
        use crate::recovery::{Point, Recovery};
        let mut runtime = fixture();
        fs::create_dir(runtime.root.join("recovery")).unwrap();
        let store = "a".repeat(64);
        let journal = runtime.root.join("recovery/legacy.json");
        let saved = Recovery {
            store: store.clone(),
            pending: Some(Point {
                snapshot: "sleep-recovery-missing".into(),
                source: Some("sleep-missing".into()),
                captured_at: 1,
            }),
            ..Recovery::default()
        };
        atomic_json(&journal, &saved).unwrap();
        assert!(
            runtime
                .request(&json!({"op":"recovery-prepare","id":"legacy","store":store}))
                .unwrap()
                .is_null()
        );
        let saved: Recovery = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
        assert!(saved.pending.is_none());
        runtime.state.workspaces.get_mut("legacy").unwrap().state = "running".into();
        assert!(
            runtime
                .request(&json!({"op":"recovery-prepare","id":"legacy","store":store}))
                .is_err()
        );
        let saved: Recovery = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
        let pending = saved.pending.unwrap();
        assert_ne!(pending.snapshot, "sleep-recovery-missing");
        assert!(pending.source.is_none());
        fs::remove_dir_all(runtime.root).unwrap();
    }

    #[test]
    fn snapshot_admission_does_not_reserve_another_writable_disk() {
        let mut runtime = fixture();
        // Sparse capacity exceeds this test volume; capture adds RAM, not a new disk.
        File::options()
            .write(true)
            .open(runtime.machine_dir("legacy").join("disk.ext4"))
            .unwrap()
            .set_len(1024 * GIB)
            .unwrap();
        assert!(
            runtime
                .reserve_disk(0)
                .unwrap_err()
                .to_string()
                .contains("headroom")
        );
        runtime.storage_headroom(GIB).unwrap();
        assert!(runtime.storage_headroom(1024 * GIB).is_err());
        let error = runtime.capture("legacy", "test", false).unwrap_err();
        assert!(error.to_string().contains("not running"), "{error:#}");
        assert_eq!(
            fs::read_dir(runtime.root.join("snapshots"))
                .unwrap()
                .count(),
            0
        );
        assert!(runtime.storage_headroom(u64::MAX).is_err());
        fs::remove_dir_all(&runtime.root).unwrap();
    }

    #[test]
    fn ready_snapshot_retry_needs_no_new_headroom() {
        let mut runtime = fixture();
        runtime
            .state
            .workspaces
            .get_mut("legacy")
            .unwrap()
            .memory_mib = u32::MAX;
        let snapshot = Snapshot {
            managed_recovery: false,
            captured_at: crate::recovery::now(),
            name: "saved".into(),
            workspace: "legacy".into(),
            managed: false,
            memory_mib: 1024,
            vcpu_count: 2,
            image: "alpine".into(),
            image_digest: None,
            runtime: crate::common::RUNTIME.into(),
            cpu: String::new(),
            kernel_sha256: String::new(),
            files: BTreeMap::new(),
            digest: None,
        };
        let directory = runtime.snapshot_dir("saved");
        fs::create_dir(&directory).unwrap();
        atomic_json(&directory.join("manifest.json"), &snapshot).unwrap();
        assert_eq!(
            runtime.capture("legacy", "saved", false).unwrap()["state"],
            "ready"
        );
        fs::remove_dir_all(&runtime.root).unwrap();
    }

    #[test]
    fn snapshot_limit_ignores_staging_and_sleep_and_startup_cleans_staging() {
        let mut runtime = fixture();
        for i in 0..MAX_SNAPSHOTS {
            for prefix in [".pending-", SLEEP] {
                let dir = runtime.snapshot_dir(&format!("{prefix}{i}"));
                fs::create_dir(&dir).unwrap();
                fs::write(dir.join("memory"), b"capture RAM").unwrap();
                fs::write(dir.join("manifest.json"), b"{}").unwrap();
            }
        }
        assert_eq!(runtime.named_snapshot_count().unwrap(), 0);
        let error = runtime.capture("legacy", "test", false).unwrap_err();
        assert!(error.to_string().contains("not running"), "{error:#}");
        for i in 0..MAX_SNAPSHOTS {
            let dir = runtime.snapshot_dir(&format!("saved-{i}"));
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join("manifest.json"), b"{}").unwrap();
        }
        assert_eq!(runtime.named_snapshot_count().unwrap(), MAX_SNAPSHOTS);
        assert!(
            runtime
                .capture("legacy", "test", false)
                .unwrap_err()
                .to_string()
                .contains("snapshot limit")
        );
        // Hibernation must still be possible at the named-snapshot limit.
        assert!(
            runtime
                .capture("legacy", "sleep-test", true)
                .unwrap_err()
                .to_string()
                .contains("not running")
        );
        runtime.cleanup_pending_snapshots().unwrap();
        assert!(!runtime.snapshot_dir(".pending-0").exists());
        assert!(
            runtime
                .snapshot_dir("sleep-0")
                .join("manifest.json")
                .exists()
        );
        assert!(
            runtime
                .snapshot_dir("saved-0")
                .join("manifest.json")
                .exists()
        );
        fs::remove_dir_all(&runtime.root).unwrap();
    }

    #[test]
    fn unavailable_disk_does_not_hide_other_workspaces() {
        let mut runtime = fixture();
        let mut missing = runtime.workspace("legacy").unwrap();
        missing.id = "missing".into();
        runtime.state.workspaces.insert(missing.id.clone(), missing);
        let healthy = runtime
            .request(&json!({"op":"inspect","id":"legacy"}))
            .unwrap();
        assert_eq!(healthy["disk_bytes"], 8 * GIB);
        let listed = runtime.request(&json!({"op":"list"})).unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 2);
        assert_eq!(
            listed
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["id"] == "legacy")
                .unwrap(),
            &healthy
        );
        let missing = runtime
            .request(&json!({"op":"inspect","id":"missing"}))
            .unwrap();
        assert_eq!(missing["state"], "stopped");
        for key in ["disk_bytes", "disk_used_bytes", "disk_allocated_bytes"] {
            assert_eq!(missing.get(key), Some(&Value::Null));
        }
        assert_eq!(
            listed
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["id"] == "missing")
                .unwrap(),
            &missing
        );
        assert!(
            runtime
                .request(&json!({"op":"inspect","id":"unknown"}))
                .is_err()
        );
        assert!(
            runtime
                .request(&json!({"op":"start","id":"missing"}))
                .is_err()
        );
        assert!(
            runtime
                .request(&json!({"op":"resize","id":"missing","disk_gib":2}))
                .is_err()
        );
        assert_eq!(runtime.workspace("missing").unwrap().state, "stopped");
        fs::create_dir(runtime.machine_dir("missing")).unwrap();
        File::create(runtime.machine_dir("missing").join("disk.ext4"))
            .unwrap()
            .set_len(GIB)
            .unwrap();
        assert_eq!(
            runtime
                .request(&json!({"op":"inspect","id":"missing"}))
                .unwrap()["disk_bytes"],
            GIB
        );
        fs::remove_dir_all(&runtime.root).unwrap();
    }

    #[test]
    #[ignore = "requires 14 GiB free space for conservative full-disk reservation"]
    fn legacy_disk_resize_is_stopped_grow_only_and_keeps_other_resources() {
        let mut runtime = fixture();
        let before = runtime
            .request(&json!({"op":"inspect","id":"legacy"}))
            .unwrap();
        assert_eq!(before["disk_bytes"], 8 * GIB);
        assert_eq!(runtime.disk_reserved().unwrap(), 8 * GIB);
        for state in ["running", "hibernated", "failed"] {
            runtime.state.workspaces.get_mut("legacy").unwrap().state = state.into();
            assert!(
                runtime
                    .request(&json!({"op":"resize","id":"legacy","disk_gib":9}))
                    .is_err()
            );
        }
        runtime.state.workspaces.get_mut("legacy").unwrap().state = "stopped".into();
        for size in [
            json!(7),
            json!(0),
            json!(null),
            json!("16"),
            json!(1.5),
            json!(16385),
        ] {
            assert!(
                runtime
                    .request(&json!({"op":"resize","id":"legacy","disk_gib":size}))
                    .is_err()
            );
        }
        let resized = runtime
            .request(&json!({"op":"resize","id":"legacy","disk_gib":9}))
            .unwrap();
        assert_eq!(resized["disk_bytes"], 9 * GIB);
        assert_eq!(resized["memory_mib"], 1024);
        assert_eq!(resized["vcpu_count"], 2);
        let repeated = runtime
            .request(&json!({"op":"resize","id":"legacy","disk_gib":9}))
            .unwrap();
        assert_eq!(repeated["disk_bytes"], 9 * GIB);
        assert!(
            runtime
                .request(&json!({"op":"resize","id":"legacy"}))
                .is_err()
        );
        let state: State =
            serde_json::from_slice(&fs::read(runtime.root.join("state.json")).unwrap()).unwrap();
        assert_eq!(state.workspaces["legacy"].memory_mib, 1024);
        assert!(runtime.reserve_disk(1024 * GIB).is_err());
        assert!(
            runtime
                .storage_headroom(1_048_576 * GIB)
                .unwrap_err()
                .to_string()
                .contains("headroom")
        );
        assert!(runtime.storage_headroom(u64::MAX).is_err());
        // Restoring a pre-growth capture must fail before stopping/replacing the box.
        let saved = runtime.snapshot_dir("before-growth");
        fs::create_dir(&saved).unwrap();
        File::create(saved.join("disk.ext4"))
            .unwrap()
            .set_len(8 * GIB)
            .unwrap();
        fs::write(saved.join("state"), b"state").unwrap();
        fs::write(saved.join("memory"), b"memory").unwrap();
        let snapshot = Snapshot {
            managed_recovery: false,
            captured_at: crate::recovery::now(),
            name: "before-growth".into(),
            workspace: "legacy".into(),
            managed: false,
            memory_mib: 1024,
            vcpu_count: 2,
            image: "alpine".into(),
            image_digest: None,
            runtime: crate::common::RUNTIME.into(),
            cpu: String::new(),
            kernel_sha256: String::new(),
            files: [
                ("state".into(), String::new()),
                ("memory".into(), String::new()),
                ("disk.ext4".into(), String::new()),
            ]
            .into(),
            digest: None,
        };
        atomic_json(&saved.join("manifest.json"), &snapshot).unwrap();
        runtime
            .verified
            .insert(snapshot.name.clone(), fingerprints(&saved).unwrap());
        runtime.state.workspaces.get_mut("legacy").unwrap().state = "running".into();
        assert!(
            runtime
                .request(&json!({"op":"restore","id":"legacy","name":"before-growth"}))
                .is_err()
        );
        assert_eq!(runtime.workspace("legacy").unwrap().state, "running");
        assert_eq!(
            runtime
                .machine_dir("legacy")
                .join("disk.ext4")
                .metadata()
                .unwrap()
                .len(),
            9 * GIB
        );
        fs::remove_dir_all(&runtime.root).unwrap();
    }
}
