//! Bounded Apple worker. A process-owned pipe prevents orphaned native guests.
use crate::{Action, Cli, common, nodes, wire};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub fn process_start(pid: u32) -> Result<String> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let n = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut _,
            std::mem::size_of_val(&info) as i32,
        )
    };
    ensure!(
        n as usize == std::mem::size_of_val(&info),
        "cannot establish process identity: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        info.pbi_uid == unsafe { libc::geteuid() },
        "process belongs to another user"
    );
    Ok(format!(
        "{}:{}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    ))
}
fn native_processes() -> Result<Vec<u32>> {
    let inventory = Command::new("/bin/ps").args(["-axo", "pid="]).output()?;
    ensure!(
        inventory.status.success(),
        "cannot establish existing native demand"
    );
    let mut active = Vec::new();
    for word in String::from_utf8_lossy(&inventory.stdout).split_whitespace() {
        let pid = word.parse::<u32>()?;
        let mut buffer = [0u8; 4096];
        let n = unsafe {
            libc::proc_pidpath(
                pid as i32,
                buffer.as_mut_ptr() as *mut _,
                buffer.len() as u32,
            )
        };
        if n > 0 {
            let path = std::ffi::CStr::from_bytes_until_nul(&buffer)?.to_string_lossy();
            if Path::new(path.as_ref())
                .file_name()
                .is_some_and(|name| name == "ow-vz")
            {
                active.push(pid);
            }
        }
    }
    Ok(active)
}
pub fn private_dir(root: &Path) -> Result<()> {
    if !root.exists() {
        fs::create_dir_all(root)?;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    }
    let m = fs::symlink_metadata(root)?;
    ensure!(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "owned private data directory required"
    );
    Ok(())
}
pub fn stopped_data(root: &Path) -> Result<()> {
    ensure!(
        native_processes()?.is_empty(),
        "native demand still exists; do not change budgets"
    );
    let database = root.join("mac-worker.sqlite3");
    if !database.exists() {
        ensure!(
            !root.join("m").exists() || fs::read_dir(root.join("m"))?.next().is_none(),
            "unregistered demand without journal"
        );
        return Ok(());
    }
    let db = Connection::open(database)?;
    let mut q = db.prepare("SELECT id,state FROM machines")?;
    let rows = q
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for e in fs::read_dir(root.join("m"))? {
        let e = e?;
        let name = e.file_name();
        let (id, state) = rows
            .iter()
            .find(|(id, _)| Some(id.as_str()) == name.to_str())
            .context("unregistered disk demand")?;
        ensure!(state != "creating", "creation outcome unresolved");
        let dir = root.join("m").join(id);
        ensure!(
            dir.join("vm.json").is_file()
                && dir.join("root.ext4").metadata()?.len() == 4 * 1024 * 1024 * 1024,
            "incomplete disk demand"
        );
        let lock = fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("vm.lock"))?;
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "native disk still active"
        );
    }
    Ok(())
}
pub fn root(value: Option<PathBuf>) -> Result<PathBuf> {
    let p = value.unwrap_or(PathBuf::from(std::env::var("HOME")?).join(".ow-mac"));
    private_dir(&p)?;
    Ok(p.canonicalize()?)
}
pub fn helper() -> Result<PathBuf> {
    let p = std::env::current_exe()?
        .parent()
        .context("CLI directory")?
        .join("ow-vz");
    nodes::private(&p).or_else(|_| {
        let m = fs::symlink_metadata(&p)?;
        ensure!(
            m.is_file() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o022 == 0,
            "owned helper required"
        );
        Ok(())
    })?;
    Ok(p)
}
fn sha(p: &Path) -> Result<String> {
    let mut h = Sha256::new();
    let mut f = fs::File::open(p)?;
    let mut b = [0; 65536];
    loop {
        let n = f.read(&mut b)?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}
pub fn verified_assets(root: &Path) -> Result<PathBuf> {
    let cfg: Value = serde_json::from_slice(&fs::read(root.join("host.json"))?)?;
    let assets = PathBuf::from(
        cfg["assets"]
            .as_str()
            .context("prepare and configure native assets first")?,
    );
    private_dir(&assets)?;
    nodes::private(&assets.join("manifest.json"))?;
    let m: Value = serde_json::from_slice(&fs::read(assets.join("manifest.json"))?)?;
    ensure!(
        m["version"] == 1
            && m["backend"] == "apple-virtualization"
            && m["arch"] == "aarch64"
            && m["runtime"] == "apple-vz-v1"
            && m["image"] == "ubuntu-arm64",
        "invalid native asset contract"
    );
    for (name, max) in [
        ("Image", 64 * 1024 * 1024u64),
        ("initrd", 128 * 1024 * 1024),
        ("root.ext4", 4 * 1024 * 1024 * 1024),
    ] {
        let p = assets.join(name);
        nodes::private(&p)?;
        let size = p.metadata()?.len();
        ensure!(
            size > 0
                && size <= max
                && m["files"][name]["size"] == size
                && m["files"][name]["sha256"] == sha(&p)?,
            "native asset hash/size mismatch: {name}"
        );
    }
    Ok(assets)
}
struct Worker {
    root: PathBuf,
    assets: PathBuf,
    db: Connection,
    children: std::collections::HashMap<String, Child>,
    memory: u64,
    cpus: u64,
    free_gib: u64,
}
impl Worker {
    fn new(root: PathBuf) -> Result<Self> {
        let assets = verified_assets(&root)?;
        private_dir(&root.join("m"))?;
        let cfg: Value = serde_json::from_slice(&fs::read(root.join("host.json"))?)?;
        let memory = cfg["memory"].as_u64().context("memory")?;
        let cpus = cfg["cpus"].as_u64().context("cpus")?;
        let free_gib = cfg["storage_gib"].as_u64().context("storage headroom")?;
        ensure!((20..=1024).contains(&free_gib), "invalid storage headroom");
        ensure!(
            [512, 1024, 2048].contains(&memory) && (1..=2).contains(&cpus) && cfg["slots"] == 1,
            "bounded Mac budget required"
        );
        let path = root.join("mac-worker.sqlite3");
        if !path.exists() {
            fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&path)?;
        }
        nodes::private(&path)?;
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS machines(id TEXT PRIMARY KEY, request TEXT NOT NULL, state TEXT NOT NULL);")?;
        let mut w = Self {
            root,
            assets,
            db,
            children: Default::default(),
            memory,
            cpus,
            free_gib,
        };
        w.reconcile()?;
        Ok(w)
    }
    fn dir(&self, id: &str) -> PathBuf {
        self.root.join("m").join(id)
    }
    fn rows(&self) -> Result<Vec<Value>> {
        let mut q = self
            .db
            .prepare("SELECT id,request,state FROM machines ORDER BY id")?;
        let rows = q.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, request, state) = row?;
            let mut v: Value = serde_json::from_str(&request)?;
            v["id"] = json!(id);
            v["state"] = json!(state);
            v["backend"] = json!("apple-virtualization");
            v["arch"] = json!("aarch64");
            out.push(v);
        }
        Ok(out)
    }
    fn state(&self, id: &str, state: &str) -> Result<()> {
        self.db.execute(
            "UPDATE machines SET state=?1 WHERE id=?2",
            params![state, id],
        )?;
        Ok(())
    }
    fn stopped_lock(&self, id: &str) -> Result<bool> {
        let p = self.dir(id).join("vm.lock");
        let f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(p)?;
        Ok(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0)
    }
    fn reconcile(&mut self) -> Result<()> {
        let exited: Vec<_> = self
            .children
            .iter_mut()
            .filter_map(|(id, c)| c.try_wait().ok().flatten().map(|_| id.clone()))
            .collect();
        for id in exited {
            self.children.remove(&id);
            self.state(&id, "unknown")?;
        }
        let unowned = native_processes()?
            .into_iter()
            .any(|pid| !self.children.values().any(|c| c.id() == pid));
        for v in self.rows()? {
            let id = v["id"].as_str().unwrap();
            if !self.children.contains_key(id)
                && self.dir(id).join("vm.json").is_file()
                && self
                    .dir(id)
                    .join("root.ext4")
                    .metadata()
                    .is_ok_and(|m| m.len() == 4 * 1024 * 1024 * 1024)
                && v["state"] != "creating"
            {
                self.state(
                    id,
                    if !unowned && self.stopped_lock(id)? {
                        "stopped"
                    } else {
                        "unknown"
                    },
                )?;
            }
        }
        Ok(())
    }
    fn unknown_dirs(&self) -> Result<u64> {
        let ids = self.rows()?;
        let mut n = 0;
        for e in fs::read_dir(self.root.join("m"))? {
            let e = e?;
            if !ids
                .iter()
                .any(|v| v["id"].as_str() == e.file_name().to_str())
            {
                n += 1;
            }
        }
        Ok(n)
    }
    fn admission(&mut self, memory: u64, cpus: u64) -> Result<()> {
        self.reconcile()?;
        // Legacy/off-root helpers consume capacity too. Never kill or adopt them.
        ensure!(
            native_processes()?
                .into_iter()
                .all(|pid| self.children.values().any(|c| c.id() == pid)),
            "legacy/unregistered native helper consumes capacity"
        );
        let rows = self.rows()?;
        let used =
            rows.iter().filter(|v| v["state"] != "stopped").count() as u64 + self.unknown_dirs()?;
        ensure!(
            used == 0 && memory <= self.memory && cpus <= self.cpus,
            "local capacity exhausted; unresolved/unregistered demand remains reserved"
        );
        // Dedicated cold disks are bounded too: one base + at most two 4 GiB disks,
        // within the 20 GiB research envelope. Memory headroom is checked live.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let path = std::ffi::CString::new(self.root.as_os_str().as_encoded_bytes())?;
        ensure!(
            unsafe { libc::statvfs(path.as_ptr(), &mut stat) } == 0
                && stat.f_bavail as u64 * stat.f_frsize as u64
                    >= self.free_gib * 1024 * 1024 * 1024,
            "configured free host storage reserve exhausted"
        );
        let output = Command::new("/usr/bin/memory_pressure").output()?;
        let s = String::from_utf8_lossy(&output.stdout);
        let pct = s
            .lines()
            .find_map(|l| {
                l.strip_prefix("System-wide memory free percentage: ")
                    .and_then(|v| v.trim_end_matches('%').parse::<u64>().ok())
            })
            .context("cannot establish memory headroom")?;
        ensure!(pct >= 20, "insufficient live memory headroom");
        Ok(())
    }
    fn machine(&self, id: &str) -> Result<Value> {
        self.rows()?
            .into_iter()
            .find(|v| v["id"] == id)
            .context("unknown workspace")
    }
    fn start(&mut self, id: &str) -> Result<Value> {
        let v = self.machine(id)?;
        if v["state"] == "running" && self.children.contains_key(id) {
            return Ok(v);
        }
        ensure!(v["state"] == "stopped", "workspace effects unresolved");
        self.admission(
            v["memory_mib"].as_u64().unwrap(),
            v["vcpu_count"].as_u64().unwrap(),
        )?;
        let dir = self.dir(id);
        ensure!(self.stopped_lock(id)?, "native helper still owns disk");
        for name in ["rpc.sock", "pty.sock"] {
            let p = dir.join(name);
            if p.exists() {
                let m = fs::symlink_metadata(&p)?;
                use std::os::unix::fs::FileTypeExt;
                ensure!(
                    m.file_type().is_socket() && m.uid() == unsafe { libc::geteuid() },
                    "unsafe stale native endpoint"
                );
                fs::remove_file(p)?;
            }
        }
        self.state(id, "starting")?;
        let mut child = Command::new(helper()?)
            .arg("node")
            .arg(dir.join("vm.json"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stderr = child.stderr.take().context("helper log pipe")?;
        let log_path = dir.join("console.log");
        std::thread::spawn(move || {
            let _ = (|| -> Result<()> {
                let mut file = fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(log_path)?;
                let mut log = Vec::new();
                let mut bytes = [0; 8192];
                loop {
                    let n = stderr.read(&mut bytes)?;
                    if n == 0 {
                        break;
                    }
                    log.extend_from_slice(&bytes[..n]);
                    if log.len() > 262144 {
                        log.drain(..log.len() - 262144);
                    }
                    use std::io::{Seek, SeekFrom};
                    file.seek(SeekFrom::Start(0))?;
                    file.write_all(&log)?;
                    file.set_len(log.len() as u64)?;
                }
                Ok(())
            })();
        });
        let pid = child.id();
        self.children.insert(id.to_owned(), child);
        let identity =
            json!({"pid":pid,"start":process_start(pid)?,"helper_sha256":sha(&helper()?)?});
        nodes::write_private(
            &dir.join(format!("process-{}.json", common::nonce()?)),
            &identity,
        )?;
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            if fs::read(dir.join("console.log"))
                .is_ok_and(|b| String::from_utf8_lossy(&b).contains("Kernel panic"))
            {
                if let Some(c) = self.children.get_mut(id) {
                    c.stdin.take();
                }
                self.state(id, "unknown")?;
                bail!("guest kernel panic; native demand retained until lock reconciliation");
            }
            if let Ok(ready) = self.rpc(id, json!({"op":"ready"})) {
                ensure!(
                    ready["arch"] == "aarch64" && ready["user"] == "dev",
                    "unexpected guest identity"
                );
                self.state(id, "running")?;
                return self.machine(id);
            }
            if self.children.get_mut(id).unwrap().try_wait()?.is_some() {
                self.children.remove(id);
                self.state(id, "unknown")?;
                bail!("native helper exited; inspect private console log");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        // Close the owned parent pipe. Leave reservation until helper lock proves exit.
        if let Some(c) = self.children.get_mut(id) {
            c.stdin.take();
        }
        self.state(id, "unknown")?;
        bail!("native readiness timeout; demand retained")
    }
    fn rpc(&self, id: &str, request: Value) -> Result<Value> {
        let mut s = UnixStream::connect(self.dir(id).join("rpc.sock"))?;
        s.set_read_timeout(Some(Duration::from_secs(35)))?;
        s.set_write_timeout(Some(Duration::from_secs(5)))?;
        wire::send(&mut s, &request)?;
        let r = wire::line(&mut s)?;
        ensure!(
            r["ok"] == true,
            "{}",
            r["error"].as_str().unwrap_or("guest error")
        );
        Ok(r["result"].clone())
    }
    fn stop(&mut self, id: &str) -> Result<Value> {
        self.reconcile()?;
        let v = self.machine(id)?;
        if v["state"] == "stopped" {
            return Ok(v);
        }
        ensure!(
            self.children.contains_key(id),
            "unowned native demand; wait for parent-pipe stop and reconcile"
        );
        self.state(id, "stopping")?;
        self.rpc(id, json!({"op":"exec","command":"sync"}))?;
        self.children.get_mut(id).unwrap().stdin.take();
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if self.children.get_mut(id).unwrap().try_wait()?.is_some() {
                self.children.remove(id);
                ensure!(self.stopped_lock(id)?, "native lock still held");
                self.state(id, "stopped")?;
                return self.machine(id);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        bail!("native stop unresolved; reservation retained")
    }
    fn request(&mut self, r: &Value) -> Result<Value> {
        self.reconcile()?;
        let op = r["op"].as_str().context("op")?;
        ensure!(
            common::MAC_OPERATIONS.contains(&op) || op == "shutdown",
            "unsupported Mac capability: {op}"
        );
        if matches!(op, "status" | "stats") {
            let unknown = self.unknown_dirs()?;
            return Ok(
                json!({"backend":"apple-virtualization","arch":"aarch64","runtime":"apple-vz-v1","max_memory_mib":self.memory,"max_running":1,"max_vcpus":self.cpus,"max_guest_memory_mib":2048,"max_guest_vcpus":2,"guest_ssh_v1":false,"operations":common::MAC_OPERATIONS,"unknown_slots":unknown,"machines":self.rows()?,"networking":false}),
            );
        }
        if op == "list" {
            return Ok(json!(self.rows()?));
        }
        if op == "shutdown" {
            for v in self.rows()? {
                self.stop(v["id"].as_str().unwrap())?;
            }
            return Ok(json!({"stopped":true}));
        }
        let id = r["id"].as_str().context("ID")?;
        common::identifier(id)?;
        ensure!(
            self.dir(id).join("rpc.sock").as_os_str().len() < 104,
            "native Unix endpoint path too long; use a shorter private data directory"
        );
        if op == "create" {
            ensure!(id != "_mac_unknown_demand", "reserved demand ID");
            let (memory, cpus) = common::requested_resources(r)?;
            ensure!(
                [512, 1024, 2048].contains(&memory)
                    && (1..=2).contains(&cpus)
                    && r["image"] == "ubuntu-arm64",
                "Mac requires Ubuntu ARM64 and 512–2048 MiB/1–2 vCPU"
            );
            let request = json!({"image":"ubuntu-arm64","memory_mib":memory,"vcpu_count":cpus});
            let old: Option<String> = self
                .db
                .query_row("SELECT request FROM machines WHERE id=?1", [id], |row| {
                    row.get(0)
                })
                .optional()?;
            if let Some(old) = old {
                ensure!(
                    serde_json::from_str::<Value>(&old)? == request,
                    "ID reused with different shape/image"
                );
                return self.machine(id);
            }
            self.admission(memory as u64, cpus as u64)?;
            ensure!(
                self.rows()?.len() < 2,
                "retained native disk budget exhausted"
            );
            ensure!(
                !self.dir(id).exists(),
                "unregistered native disk; reserve and reconcile first"
            );
            self.db.execute(
                "INSERT INTO machines VALUES(?1,?2,'creating')",
                params![id, request.to_string()],
            )?;
            let dir = self.dir(id);
            private_dir(&dir)?;
            fs::copy(self.assets.join("root.ext4"), dir.join("root.ext4"))?;
            fs::File::open(dir.join("root.ext4"))?.sync_all()?;
            let cfg = json!({"kernel":self.assets.join("Image"),"initrd":self.assets.join("initrd"),"memory_mib":memory,"vcpu_count":cpus});
            nodes::write_private(&dir.join("vm.json"), &cfg)?;
            fs::File::open(&dir)?.sync_all()?;
            self.state(id, "stopped")?;
            return self.start(id);
        }
        if op == "inspect" {
            return self.machine(id);
        }
        if op == "start" {
            return self.start(id);
        }
        if op == "stop" {
            return Ok(json!({"workspace":self.stop(id)?,"sync_error":null}));
        }
        ensure!(
            self.machine(id)?["state"] == "running" && self.children.contains_key(id),
            "workspace is not owned/running"
        );
        self.rpc(id, r.clone())
    }
}
fn peer(s: &UnixStream) -> Result<()> {
    let (mut uid, mut gid) = (0, 0);
    ensure!(
        unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } == 0
            && uid == unsafe { libc::geteuid() },
        "worker caller UID denied"
    );
    Ok(())
}
pub fn bridge(mut a: UnixStream, mut b: UnixStream) -> Result<()> {
    a.set_nonblocking(true)?;
    b.set_nonblocking(true)?;
    let mut buffer = [0; 16384];
    let started = Instant::now();
    let mut idle = Instant::now();
    loop {
        ensure!(
            started.elapsed() < Duration::from_secs(3600)
                && idle.elapsed() < Duration::from_secs(90),
            "native stream deadline"
        );
        let mut fds = [
            libc::pollfd {
                fd: a.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: b.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        unsafe {
            libc::poll(fds.as_mut_ptr(), 2, 100);
        }
        for i in 0..2 {
            if fds[i].revents != 0 {
                let (source, target) = if i == 0 {
                    (&mut a, &mut b)
                } else {
                    (&mut b, &mut a)
                };
                match source.read(&mut buffer) {
                    Ok(0) => return Ok(()),
                    Ok(n) => {
                        let mut written = 0;
                        let deadline = Instant::now() + Duration::from_secs(5);
                        while written < n {
                            match target.write(&buffer[written..n]) {
                                Ok(0) => bail!("stream closed"),
                                Ok(count) => written += count,
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                    ensure!(
                                        Instant::now() < deadline,
                                        "stream backpressure timeout"
                                    );
                                    std::thread::sleep(Duration::from_millis(5));
                                }
                                Err(e) => return Err(e.into()),
                            }
                        }
                        idle = Instant::now();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
}
fn readonly(root: &Path, op: &str) -> Result<Value> {
    let cfg: Value = serde_json::from_slice(&fs::read(root.join("host.json"))?)?;
    let db = Connection::open(root.join("mac-worker.sqlite3"))?;
    db.busy_timeout(Duration::from_secs(5))?;
    let mut q = db.prepare("SELECT id,request,state FROM machines ORDER BY id")?;
    let mut rows = Vec::new();
    for row in q.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (id, request, state) = row?;
        let mut v: Value = serde_json::from_str(&request)?;
        v["id"] = json!(id);
        v["state"] = json!(state);
        v["backend"] = json!("apple-virtualization");
        v["arch"] = json!("aarch64");
        rows.push(v);
    }
    let known = rows.iter().filter(|v| v["state"] != "stopped").count();
    let unregistered = fs::read_dir(root.join("m"))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            !rows
                .iter()
                .any(|v| v["id"].as_str() == e.file_name().to_str())
        })
        .count();
    let unowned_helpers = native_processes()?.len().saturating_sub(known);
    if unregistered + unowned_helpers > 0 {
        rows.push(json!({"id":"_mac_unknown_demand","state":"unknown","synthetic_reservation":true,"memory_mib":cfg["memory"],"vcpu_count":cfg["cpus"],"image":"ubuntu-arm64"}));
    }
    if op == "list" {
        return Ok(json!(rows));
    }
    Ok(
        json!({"backend":"apple-virtualization","arch":"aarch64","runtime":"apple-vz-v1","max_memory_mib":cfg["memory"],"max_running":1,"max_vcpus":cfg["cpus"],"max_guest_memory_mib":2048,"max_guest_vcpus":2,"guest_ssh_v1":false,"operations":common::MAC_OPERATIONS,"machines":rows,"networking":false}),
    )
}
fn worker(root: PathBuf) -> Result<()> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("worker.lock"))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "worker already active"
    );
    let mut lockwrite = &lock;
    lockwrite.set_len(0)?;
    lockwrite.write_all(
        json!({"pid":std::process::id(),"start":process_start(std::process::id())?})
            .to_string()
            .as_bytes(),
    )?;
    lockwrite.sync_all()?;
    let runtime = Arc::new(Mutex::new(Worker::new(root.clone())?));
    let socket = root.join("control.sock");
    if socket.exists() {
        use std::os::unix::fs::FileTypeExt;
        let m = fs::symlink_metadata(&socket)?;
        ensure!(
            m.file_type().is_socket() && m.uid() == unsafe { libc::geteuid() },
            "unsafe stale control endpoint"
        );
        fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
    listener.set_nonblocking(true)?;
    // Managed participation owns stdin's pipe. A supervisor crash closes it;
    // exiting the worker in turn closes every helper's process-owned pipe.
    let mut input: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(0, &mut input) } == 0 && input.st_mode & libc::S_IFMT == libc::S_IFIFO {
        let stop = stopping.clone();
        std::thread::spawn(move || {
            let mut bytes = [0u8; 1];
            loop {
                let n = unsafe { libc::read(0, bytes.as_mut_ptr() as *mut _, 1) };
                if n == 0 {
                    stop.store(true, std::sync::atomic::Ordering::SeqCst);
                    break;
                }
                if n < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    stop.store(true, std::sync::atomic::Ordering::SeqCst);
                    break;
                }
            }
        });
    }
    while !stopping.load(std::sync::atomic::Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut s, _)) => {
                // BSD accept inherits O_NONBLOCK from the listener. Framing
                // needs blocking I/O with the explicit read/write deadlines.
                s.set_nonblocking(false)?;
                peer(&s)?;
                if active
                    .fetch_update(
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                        |n| (n < 16).then_some(n + 1),
                    )
                    .is_err()
                {
                    continue;
                }
                let rt = runtime.clone();
                let view_root = root.clone();
                let count = active.clone();
                let stop = stopping.clone();
                std::thread::spawn(move || {
                    struct Slot(Arc<std::sync::atomic::AtomicUsize>);
                    impl Drop for Slot {
                        fn drop(&mut self) {
                            self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                    let _slot = Slot(count);
                    let session_result = (|| -> Result<()> {
                        s.set_read_timeout(Some(Duration::from_secs(10)))?;
                        s.set_write_timeout(Some(Duration::from_secs(5)))?;
                        let r = wire::line(&mut s)?;
                        if r["op"] == "terminal" {
                            let connection = (|| -> Result<UnixStream> {
                                let w =
                                    rt.lock().map_err(|_| anyhow::anyhow!("worker poisoned"))?;
                                let id = r["id"].as_str().context("ID")?;
                                common::identifier(id)?;
                                ensure!(
                                    w.machine(id)?["state"] == "running"
                                        && w.children.contains_key(id),
                                    "workspace is not running"
                                );
                                Ok(UnixStream::connect(w.dir(id).join("pty.sock"))?)
                            })();
                            match connection {
                                Ok(guest) => {
                                    wire::send(&mut s, &wire::response(Ok(json!({}))))?;
                                    bridge(s, guest)?;
                                }
                                Err(e) => wire::send(&mut s, &wire::response(Err(e)))?,
                            }
                        } else {
                            let result =
                                if matches!(r["op"].as_str(), Some("status" | "stats" | "list")) {
                                    readonly(&view_root, r["op"].as_str().unwrap())
                                } else {
                                    rt.lock()
                                        .map_err(|_| anyhow::anyhow!("worker poisoned"))?
                                        .request(&r)
                                };
                            let shutdown = r["op"] == "shutdown" && result.is_ok();
                            wire::send(&mut s, &wire::response(result))?;
                            if shutdown {
                                stop.store(true, std::sync::atomic::Ordering::SeqCst);
                            }
                        }
                        Ok(())
                    })();
                    if let Err(error) = session_result {
                        eprintln!("Mac worker session: {error:#}");
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => return Err(e.into()),
        }
    }
    fs::remove_file(socket)?;
    Ok(())
}
pub fn run(cli: Cli) -> Result<i32> {
    let root = root(cli.data_dir)?;
    match cli.command {
        Action::Worker => {
            worker(root)?;
            return Ok(0);
        }
        Action::NodeAgent {
            controller,
            credential,
            join,
            ca_cert,
            insecure_loopback_test,
        } => {
            nodes::agent(
                &root,
                &controller,
                &credential,
                join.as_deref(),
                ca_cert.as_deref(),
                insecure_loopback_test,
            )?;
            return Ok(0);
        }
        Action::NodeController {
            listen,
            tls_cert,
            tls_key,
            insecure_loopback_test,
        } => {
            ensure!(
                listen.ip().is_loopback(),
                "Mac controller entry point is for loopback diagnostics; Linux planner owns live controller deployment"
            );
            nodes::controller(&root, listen, tls_cert, tls_key, insecure_loopback_test)?;
            return Ok(0);
        }
        Action::NodeJoin {
            node,
            output,
            ttl,
            memory,
            slots,
            cpus,
            controller,
        } => {
            nodes::invitation(
                &root,
                &node,
                ttl,
                memory,
                slots,
                cpus,
                controller.as_deref(),
                |v| nodes::write_private(&output, v),
            )?;
            return Ok(0);
        }
        Action::NodeRevoke { node } => {
            nodes::revoke(&root, &node)?;
            return Ok(0);
        }
        Action::Nodes => {
            println!("{}", nodes::inventory(&root)?);
            return Ok(0);
        }
        _ => {}
    }
    let r = match cli.command {
        Action::Status => json!({"op":"status"}),
        Action::Stats => json!({"op":"stats"}),
        Action::List => json!({"op":"list"}),
        Action::Inspect { id } => json!({"op":"inspect","id":id}),
        Action::Create {
            id,
            image,
            memory,
            cpus,
            node,
        } => {
            ensure!(node.is_none(), "local placement is this Mac");
            json!({"op":"create","id":id,"image":image,"memory_mib":memory,"vcpu_count":cpus})
        }
        Action::Start { id } => json!({"op":"start","id":id}),
        Action::Stop { id } => json!({"op":"stop","id":id}),
        Action::Exec { id, command } => {
            let text = command
                .iter()
                .map(|s| common::shell_quote(s))
                .collect::<Vec<_>>()
                .join(" ");
            let result = wire::request(&root, json!({"op":"exec","id":id,"command":text}))?;
            print!("{}", result["output"].as_str().unwrap_or(""));
            return Ok(result["exit_code"].as_i64().unwrap_or(1) as i32);
        }
        Action::Put { id, local, guest } => {
            use base64::Engine;
            ensure!(
                local.metadata()?.len() <= 262144,
                "binary file limit 256 KiB"
            );
            json!({"op":"put","id":id,"path":guest,"data":base64::engine::general_purpose::STANDARD.encode(fs::read(local)?)})
        }
        Action::Get { id, guest, local } => {
            use base64::Engine;
            let r = wire::request(&root, json!({"op":"get","id":id,"path":guest}))?;
            fs::write(
                local,
                base64::engine::general_purpose::STANDARD
                    .decode(r["data"].as_str().context("file response")?)?,
            )?;
            return Ok(0);
        }
        Action::Down => json!({"op":"shutdown"}),
        Action::Shell { id } => return crate::local_terminal::local(&root, &id),
        _ => bail!("unsupported Mac capability or local action"),
    };
    println!("{}", wire::request(&root, r)?);
    Ok(0)
}
