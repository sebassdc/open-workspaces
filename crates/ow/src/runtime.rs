use crate::vm::{self, Vm, shell_quote};
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
        Err(std::env::VarError::NotPresent) => Ok(0),
        Err(e) => Err(e.into()),
    }
}
fn minimum_free_bytes() -> Result<u64> {
    Ok((minimum_free_gib()? as u64 * 1024 * 1024 * 1024).max(128 * 1024 * 1024))
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
pub fn image_profile(image: &str) -> Result<()> {
    ensure!(
        ["alpine", "arch", "ubuntu"].contains(&image),
        "unsupported image profile"
    );
    Ok(())
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
    fn existing_workspaces_keep_the_alpine_profile() {
        let workspace: Workspace = serde_json::from_value(json!({"id":"existing","index":1,"state":"hibernated","memory_mib":256,"source":null,"hibernation":"saved"})).unwrap();
        assert_eq!(workspace.image, "alpine");
        let snapshot: Snapshot = serde_json::from_value(json!({"name":"saved","workspace":"existing","memory_mib":256,"runtime":"firecracker-v1.17.0","cpu":"cpu","kernel_sha256":"hash","files":{}})).unwrap();
        assert_eq!(snapshot.image, "alpine");
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
    pub source: Option<String>,
    pub hibernation: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    next_index: u32,
    workspaces: BTreeMap<String, Workspace>,
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    name: String,
    workspace: String,
    memory_mib: u32,
    #[serde(default = "default_cpus")]
    vcpu_count: u32,
    #[serde(default = "default_image")]
    image: String,
    runtime: String,
    cpu: String,
    kernel_sha256: String,
    files: BTreeMap<String, String>,
}

impl Snapshot {
    fn summary(&self) -> Value {
        json!({"name":self.name,"workspace":self.workspace,"memory_mib":self.memory_mib,"vcpu_count":self.vcpu_count,"image":self.image,"state":"ready"})
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
    ["manifest.json", "state", "memory", "disk.ext4"]
        .iter()
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

pub struct Runtime {
    pub root: PathBuf,
    assets: PathBuf,
    state: State,
    machines: BTreeMap<String, Vm>,
    cpu: String,
    kernel_hash: String,
    verified: BTreeMap<String, Vec<Fingerprint>>,
}

impl Runtime {
    /// A dedicated guest PTY; never attach an interactive client to management serial.
    pub fn terminal(&mut self, id: &str) -> Result<std::net::TcpStream> {
        use std::{net::TcpStream, time::Duration};
        identifier(id)?;
        let address = self.address(id)?;
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
        for directory in ["machines", "snapshots"] {
            fs::create_dir_all(root.join(directory))?;
        }
        let mut state: State = if root.join("state.json").exists() {
            serde_json::from_slice(&fs::read(root.join("state.json"))?)?
        } else {
            State::default()
        };
        // VM children die with their worker. Preserve hibernation; reconcile everything else to stopped.
        for workspace in state.workspaces.values_mut() {
            if workspace.state != "hibernated" {
                workspace.state = "stopped".into();
            }
        }
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
        };
        runtime.save()?;
        Ok(runtime)
    }

    fn save(&self) -> Result<()> {
        atomic_json(&self.root.join("state.json"), &self.state)
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
        Ok(self.running(id)?.address.clone())
    }

    fn spawn(&mut self, workspace: &Workspace, snapshot: Option<&Path>, fork: bool) -> Result<()> {
        self.capacity(workspace.memory_mib, workspace.vcpu_count)?;
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
        if workspace.image != "alpine" {
            let bundle = self.assets.join("guest/network-tools.tar.gz");
            let digest = hash(&bundle)?;
            let marker = format!("/ow/.network-{digest}");
            if machine.command(&format!("test -f {marker}"))?.1 != 0 {
                let guest = format!("/run/ow-network-{digest}.tar.gz");
                install_file(&mut machine, &bundle, &guest, &digest)?;
                machine.checked(&format!("tar -xzf {guest} -C / && mkdir -p /etc/ssl/certs && {{ test -f /etc/ssl/certs/ca-certificates.crt || cp /ow/etc/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt; }} && touch {marker}; rm -f {guest}"))?;
                if workspace.image == "arch" {
                    machine.checked("if ! grep -q '^Server' /etc/pacman.d/mirrorlist; then printf 'Server = https://geo.mirror.pkgbuild.com/$repo/os/$arch\\n' >> /etc/pacman.d/mirrorlist; fi")?;
                }
            }
        }
        if workspace.image != "alpine" {
            machine.checked("chmod 755 /etc/ssl /etc/ssl/certs /ow /ow/bin /ow/lib /ow/etc; chmod 644 /etc/ssl/certs/ca-certificates.crt")?;
        }
        self.machines.insert(workspace.id.clone(), machine);
        Ok(())
    }

    fn storage_headroom(&self, extra: u64) -> Result<()> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let path = CString::new(self.root.as_os_str().as_bytes())?;
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        ensure!(
            unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } == 0,
            "storage capacity unavailable"
        );
        let stats = unsafe { stats.assume_init() };
        ensure!(
            stats.f_bavail.saturating_mul(stats.f_frsize) >= minimum_free_bytes()? + extra,
            "storage headroom exhausted"
        );
        Ok(())
    }
    fn create(
        &mut self,
        id: &str,
        memory: u32,
        cpus: u32,
        image: &str,
        source: Option<String>,
        snapshot: Option<&str>,
    ) -> Result<Value> {
        identifier(id)?;
        image_profile(image)?;
        resources(memory, cpus)?;
        if let Some(existing) = self.state.workspaces.get(id) {
            ensure!(
                existing.memory_mib == memory
                    && existing.vcpu_count == cpus
                    && existing.source == source
                    && existing.image == image,
                "ID already exists with different parameters"
            );
            return Ok(json!(existing));
        }
        self.capacity(memory, cpus)?;
        self.storage_headroom(0)?;
        ensure!(
            self.state.workspaces.len() < MAX_WORKSPACES,
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
        fs::create_dir(&directory)?;
        vm::reflink(&source_disk, &directory.join("disk.ext4"))?;
        let workspace = Workspace {
            id: id.into(),
            index: self.state.next_index,
            state: "starting".into(),
            memory_mib: memory,
            vcpu_count: cpus,
            image: image.into(),
            source,
            hibernation: None,
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
        Ok(json!(self.state.workspaces[id]))
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

    fn start(&mut self, id: &str) -> Result<Value> {
        let workspace = self.workspace(id)?;
        if self.machines.contains_key(id) {
            return Ok(json!(workspace));
        }
        let snapshot = workspace
            .hibernation
            .as_deref()
            .map(|name| self.validate_snapshot(name))
            .transpose()?;
        if let Some(ref directory) = snapshot {
            let replacement = self
                .machine_dir(id)
                .join(format!("disk-{}.ext4", vm::nonce()?));
            vm::reflink(&directory.join("disk.ext4"), &replacement)?;
            fs::rename(replacement, self.machine_dir(id).join("disk.ext4"))?;
        }
        self.spawn(&workspace, snapshot.as_deref(), false)?;
        let workspace = self.state.workspaces.get_mut(id).unwrap();
        workspace.state = "running".into();
        workspace.hibernation = None;
        self.save()?;
        Ok(json!(self.state.workspaces[id]))
    }

    fn capture(&mut self, id: &str, name: &str) -> Result<Value> {
        identifier(name)?;
        let workspace = self.workspace(id)?;
        self.storage_headroom(workspace.memory_mib as u64 * 1024 * 1024)?;
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
        let count = fs::read_dir(self.root.join("snapshots"))?.count();
        ensure!(
            count < MAX_SNAPSHOTS,
            "snapshot limit ({MAX_SNAPSHOTS}) reached"
        );
        ensure!(
            !destination.exists(),
            "incomplete snapshot exists; preserve and inspect it"
        );
        let staging = self.snapshot_dir(&format!(".pending-{}", vm::nonce()?));
        fs::create_dir(&staging)?;
        let disk = self.machine_dir(id).join("disk.ext4");
        self.running(id)?.capture(&staging, &disk)?;
        let mut files = BTreeMap::new();
        for file in ["state", "memory", "disk.ext4"] {
            files.insert(file.into(), hash(&staging.join(file))?);
        }
        let snapshot = Snapshot {
            name: name.into(),
            workspace: id.into(),
            memory_mib: workspace.memory_mib,
            vcpu_count: workspace.vcpu_count,
            image: workspace.image,
            runtime: "firecracker-v1.17.0".into(),
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
        Ok(snapshot.summary())
    }

    fn validate_snapshot(&mut self, name: &str) -> Result<PathBuf> {
        identifier(name)?;
        let directory = self.snapshot_dir(name);
        let snapshot: Snapshot =
            serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?;
        ensure!(
            snapshot.runtime == "firecracker-v1.17.0"
                && snapshot.cpu == self.cpu
                && snapshot.kernel_sha256 == self.kernel_hash,
            "snapshot runtime, CPU or guest kernel is incompatible"
        );
        ensure!(snapshot.files.len() == 3, "snapshot manifest is incomplete");
        let before = fingerprints(&directory)?;
        if self.verified.get(name) == Some(&before) {
            return Ok(directory);
        }
        for file in ["state", "memory", "disk.ext4"] {
            ensure!(
                snapshot.files.get(file) == Some(&hash(&directory.join(file))?),
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

    pub fn request(&mut self, request: &Value) -> Result<Value> {
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
        match op {
            "status" => Ok(
                json!({"runtime":"Firecracker v1.17.0","running":self.machines.len(),
                "worker_pid":std::process::id(),
                "host_managed":std::env::var("OW_HOST_MANAGED").as_deref()==Ok("1"),"host_asset_hash":std::env::var("OW_HOST_ASSET_HASH").unwrap_or_default(),"min_free_gib":minimum_free_gib()?,"max_running":max_running(),"max_memory_mib":max_memory(),"max_vcpus":max_cpus(),"max_workspaces":MAX_WORKSPACES,"max_guest_memory_mib":16384,"max_guest_vcpus":16,
                "max_snapshots":MAX_SNAPSHOTS,"internet":crate::network::enabled(),"network":"filtered rootless IPv4 Internet egress; LAN, host and peer access blocked","prototype":true}),
            ),
            "list" => Ok(json!(self.state.workspaces.values().collect::<Vec<_>>())),
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
            "inspect" => Ok(json!(self.workspace(id)?)),
            "create" => self.create(
                id,
                u32::try_from(request["memory_mib"].as_u64().unwrap_or(256))?,
                u32::try_from(request["vcpu_count"].as_u64().unwrap_or(1))?,
                request["image"].as_str().unwrap_or("alpine"),
                None,
                None,
            ),
            "resize" => {
                let memory =
                    u32::try_from(request["memory_mib"].as_u64().context("missing memory")?)?;
                let cpus = u32::try_from(
                    request["vcpu_count"]
                        .as_u64()
                        .context("missing CPU count")?,
                )?;
                resources(memory, cpus)?;
                let existing = self.workspace(id)?;
                ensure!(
                    !self.machines.contains_key(id) && existing.state == "stopped",
                    "Stop the machine before resizing; RAM checkpoints cannot change CPU or RAM size"
                );
                let workspace = self.state.workspaces.get_mut(id).unwrap();
                workspace.memory_mib = memory;
                workspace.vcpu_count = cpus;
                self.save()?;
                Ok(json!(self.state.workspaces[id]))
            }
            "start" => self.start(id),
            "stop" => self.stop(id),
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
            "snapshot" => self.capture(
                id,
                request["name"].as_str().context("missing snapshot name")?,
            ),
            "snapshots" => {
                let mut snapshots = Vec::<Snapshot>::new();
                for item in fs::read_dir(self.root.join("snapshots"))? {
                    let path = item?.path().join("manifest.json");
                    if path.exists() {
                        snapshots.push(serde_json::from_slice(&fs::read(path)?)?);
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
                    self.capture(id, name)?;
                }
                let directory = self.validate_snapshot(name)?;
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
                )
            }
            "hibernate" => {
                let workspace = self.workspace(id)?;
                if workspace.state == "hibernated" {
                    return Ok(json!(workspace));
                }
                let name = format!("sleep-{}", vm::nonce()?);
                self.capture(id, &name)?;
                self.machines.remove(id);
                let workspace = self.state.workspaces.get_mut(id).unwrap();
                workspace.state = "hibernated".into();
                workspace.hibernation = Some(name);
                self.save()?;
                Ok(json!(self.state.workspaces[id]))
            }
            "restore" => {
                let name = request["name"].as_str().context("missing snapshot name")?;
                let directory = self.validate_snapshot(name)?;
                let snapshot: Snapshot =
                    serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?;
                let workspace = self.workspace(id)?;
                ensure!(
                    snapshot.workspace == id
                        && snapshot.memory_mib == workspace.memory_mib
                        && snapshot.vcpu_count == workspace.vcpu_count
                        && snapshot.image == workspace.image,
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
