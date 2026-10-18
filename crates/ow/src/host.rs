use crate::runtime::Runtime;
use crate::{Action, Cli, assets, common, network, remote, runtime, terminal, vm, wire};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use ow_core::ComputeProvider;
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    net::{Shutdown, TcpListener, TcpStream},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub(crate) fn data_root(value: Option<PathBuf>) -> Result<PathBuf> {
    let path = value
        .or_else(|| std::env::var_os("OW_DATA_DIR").map(PathBuf::from))
        .unwrap_or(std::env::current_dir()?.join("data/prototype"));
    fs::create_dir_all(&path)?;
    let path = fs::canonicalize(path)?;
    ensure!(
        fs::metadata(&path)?.uid() == unsafe { libc::geteuid() },
        "data directory must belong to the current operator"
    );
    let marker = path.join(".ow-data");
    if !marker.exists() {
        ensure!(
            fs::read_dir(&path)?.next().is_none(),
            "refusing to adopt a nonempty unrelated data directory"
        );
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&marker)?;
        file.write_all(b"open-workspaces local prototype v1\n")?;
        file.sync_all()?;
    }
    ensure!(
        fs::read(&marker)? == b"open-workspaces local prototype v1\n",
        "unrecognized data directory marker"
    );
    ensure!(
        path.join("control.sock").as_os_str().len() < 104,
        "data directory path too long for Unix sockets"
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

pub(crate) fn assert_owner(stream: &UnixStream) -> Result<()> {
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut size,
        )
    };
    ensure!(rc == 0, "peer credentials unavailable");
    ensure!(
        unsafe { credentials.assume_init().uid == libc::getuid() },
        "unauthorized control peer"
    );
    Ok(())
}

fn bridge(mut unix: UnixStream, mut tcp: TcpStream) -> Result<()> {
    unix.set_read_timeout(None)?;
    unix.set_write_timeout(None)?;
    let mut tcp_write = tcp.try_clone()?;
    let mut unix_read = unix.try_clone()?;
    let forward = std::thread::spawn(move || {
        let result = io::copy(&mut unix_read, &mut tcp_write);
        let _ = tcp_write.shutdown(Shutdown::Write);
        result
    });
    let result = io::copy(&mut tcp, &mut unix);
    let _ = unix.shutdown(Shutdown::Write);
    let _ = forward.join();
    result?;
    Ok(())
}

/// Anything that keeps idle bookkeeping: the runtime, or a stand-in in tests.
trait Streams {
    fn idle(&mut self) -> &mut ow_core::idle::Idle;
}
impl Streams for Runtime {
    fn idle(&mut self) -> &mut ow_core::idle::Idle {
        &mut self.idle
    }
}

/// Keeps a machine awake (no automatic hibernation) while one of its streams is open.
struct Awake<T: Streams>(Arc<Mutex<T>>, String);
impl<T: Streams> Awake<T> {
    /// Takes the lock guard and releases it: dropping an `Awake` locks the runtime,
    /// so it must never outlive a guard held on the same thread.
    fn open(runtime: &Arc<Mutex<T>>, mut locked: std::sync::MutexGuard<'_, T>, id: &str) -> Self {
        locked.idle().open(id, Instant::now());
        drop(locked);
        Self(runtime.clone(), id.to_owned())
    }
}
impl<T: Streams> Drop for Awake<T> {
    fn drop(&mut self) {
        if let Ok(mut runtime) = self.0.lock() {
            runtime.idle().close(&self.1, Instant::now());
        }
    }
}

fn worker(root: PathBuf) -> Result<()> {
    unsafe {
        libc::umask(0o077);
    }
    let output = Command::new("ip").args(["-j", "link", "show"]).output()?;
    let links: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        links
            .as_array()
            .is_some_and(|a| a.len() == 1 && a[0]["ifname"] == "lo"),
        "worker requires a fresh isolated network namespace; use ow up"
    );
    network::initialize()?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("worker.lock"))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "worker already owns this data directory"
    );
    let socket = root.join("control.sock");
    if socket.exists() {
        fs::remove_file(&socket)?;
    }
    let runtime = Arc::new(Mutex::new(Runtime::new(root.clone(), assets())?));
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let stopping = Arc::new(AtomicBool::new(false));
    let terminals = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut idle_check = Instant::now();
    while !stopping.load(Ordering::SeqCst) {
        // On this thread: hibernation must not race the VMMs this thread spawns.
        if idle_check.elapsed() >= Duration::from_secs(5) {
            idle_check = Instant::now();
            let results = runtime
                .lock()
                .map_err(|_| anyhow::anyhow!("runtime lock poisoned"))?
                .hibernate_idle();
            for (id, result) in results {
                match result {
                    Ok(_) => eprintln!("idle: hibernated {id}"),
                    Err(e) => eprintln!("idle: hibernating {id} failed: {e:#}"),
                }
            }
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let runtime = runtime.clone();
                let stopping = stopping.clone();
                let result = (|| -> Result<()> {
                    assert_owner(&stream)?;
                    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
                    let request = wire::line(&mut stream)?;
                    let operation = request["op"].as_str().unwrap_or("");
                    if operation == "ssh" {
                        let connection = (|| -> Result<_> {
                            ensure!(
                                request
                                    .as_object()
                                    .is_some_and(|o| o.len() == 2 && o.contains_key("id")),
                                "invalid SSH request"
                            );
                            terminals
                                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                                    (n < 16).then_some(n + 1)
                                })
                                .map_err(|_| anyhow::anyhow!("SSH capacity reached"))?;
                            struct Slot(Arc<std::sync::atomic::AtomicUsize>);
                            impl Drop for Slot {
                                fn drop(&mut self) {
                                    self.0.fetch_sub(1, Ordering::SeqCst);
                                }
                            }
                            let slot = Slot(terminals.clone());
                            let id = request["id"].as_str().context("missing ID")?;
                            let mut locked = runtime
                                .lock()
                                .map_err(|_| anyhow::anyhow!("runtime lock poisoned"))?;
                            let (tcp, generation) = locked.ssh(id)?;
                            let awake = Awake::open(&runtime, locked, id);
                            Ok((
                                tcp,
                                generation,
                                crate::ssh_guest::path(&root, id),
                                (slot, awake),
                            ))
                        })();
                        match connection {
                            Ok((tcp, generation, path, slot)) => {
                                wire::send(&mut stream, &json!({"ok":true,"result":{}}))?;
                                std::thread::spawn(move || {
                                    let _slot = slot;
                                    let _ = crate::ssh_guest::bridge(stream, tcp, path, generation);
                                });
                            }
                            Err(e) => wire::send(&mut stream, &wire::response(Err(e)))?,
                        }
                    } else if operation == "terminal" {
                        struct Slot(Arc<std::sync::atomic::AtomicUsize>);
                        impl Drop for Slot {
                            fn drop(&mut self) {
                                self.0.fetch_sub(1, Ordering::SeqCst);
                            }
                        }
                        let connection = (|| -> Result<(TcpStream, (Slot, Awake<Runtime>))> {
                            terminals
                                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                                    (n < 16).then_some(n + 1)
                                })
                                .map_err(|_| anyhow::anyhow!("terminal capacity reached"))?;
                            let slot = Slot(terminals.clone());
                            let id = request["id"].as_str().context("missing ID")?;
                            let mut locked = runtime
                                .lock()
                                .map_err(|_| anyhow::anyhow!("runtime lock poisoned"))?;
                            let tcp = locked.terminal(id)?;
                            let awake = Awake::open(&runtime, locked, id);
                            Ok((tcp, (slot, awake)))
                        })();
                        match connection {
                            Ok((tcp, slot)) => {
                                wire::send(&mut stream, &json!({"ok":true,"result":{}}))?;
                                std::thread::spawn(move || {
                                    let _slot = slot;
                                    let _ = bridge(stream, tcp);
                                });
                            }
                            Err(error) => wire::send(&mut stream, &wire::response(Err(error)))?,
                        }
                    } else if operation == "tunnel" {
                        let connection = (|| -> Result<(TcpStream, Awake<Runtime>)> {
                            let id = request["id"].as_str().context("missing ID")?;
                            let port = request["port"].as_u64().context("missing port")?;
                            ensure!((1..=65535).contains(&port), "invalid port");
                            let mut locked = runtime
                                .lock()
                                .map_err(|_| anyhow::anyhow!("runtime lock poisoned"))?;
                            let address = locked.address(id)?;
                            let awake = Awake::open(&runtime, locked, id);
                            Ok((
                                TcpStream::connect_timeout(
                                    &format!("{address}:{port}").parse()?,
                                    Duration::from_secs(5),
                                )?,
                                awake,
                            ))
                        })();
                        match connection {
                            Ok((tcp, awake)) => {
                                wire::send(&mut stream, &json!({"ok":true,"result":{}}))?;
                                std::thread::spawn(move || {
                                    let _awake = awake;
                                    let _ = bridge(stream, tcp);
                                });
                            }
                            Err(error) => {
                                wire::send(&mut stream, &wire::response(Err(error)))?;
                            }
                        }
                    } else if operation == "egress-enable" {
                        let result = network::enable(&request["host_addresses"])
                            .map(|_| json!({"enabled":true}));
                        wire::send(&mut stream, &wire::response(result))?;
                    } else if operation == "shutdown" {
                        runtime
                            .lock()
                            .map_err(|_| anyhow::anyhow!("runtime lock poisoned"))?
                            .shutdown();
                        stopping.store(true, Ordering::SeqCst);
                        wire::send(&mut stream, &json!({"ok":true,"result":{"stopped":true}}))?;
                    } else {
                        // VMMs must be spawned by this long-lived worker thread: Linux
                        // parent-death signals are tied to the spawning thread's lifetime.
                        let response = runtime
                            .lock()
                            .map_err(|_| anyhow::anyhow!("runtime lock poisoned"))?
                            .operation(&request);
                        wire::send(&mut stream, &wire::response(response))?;
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    eprintln!("control request: {error:#}");
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => return Err(error.into()),
        }
    }
    fs::remove_file(socket)?;
    Ok(())
}

fn proxy_paths(root: &Path, id: &str, port: u16) -> (PathBuf, PathBuf) {
    (
        root.join(format!("p-{id}-{port}.sock")),
        root.join(format!("p-{id}-{port}.json")),
    )
}

fn gateway(root: PathBuf, id: String, port: u16) -> Result<()> {
    runtime::identifier(&id)?;
    unsafe {
        libc::umask(0o077);
    }
    let (control_path, descriptor) = proxy_paths(&root, &id, port);
    ensure!(
        control_path.as_os_str().len() < 104,
        "gateway socket path too long"
    );
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(format!("p-{id}-{port}.lock")))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "gateway already running"
    );
    if control_path.exists() {
        fs::remove_file(&control_path)?;
    }
    let control = UnixListener::bind(&control_path)?;
    fs::set_permissions(&control_path, fs::Permissions::from_mode(0o600))?;
    control.set_nonblocking(true)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    runtime::atomic_json(
        &descriptor,
        &json!({"url":format!("http://{}",listener.local_addr()?),"workspace":id,"guest_port":port}),
    )?;
    loop {
        if let Ok((mut client, _)) = control.accept() {
            assert_owner(&client)?;
            client.set_read_timeout(Some(Duration::from_secs(5)))?;
            match wire::line(&mut client) {
                Ok(request) if request["op"] == "shutdown" => {
                    wire::send(&mut client, &json!({"ok":true}))?;
                    break;
                }
                Ok(_) => {
                    wire::send(&mut client, &json!({"ok":true}))?;
                }
                Err(_) => {}
            }
        }
        match listener.accept() {
            Ok((mut client, _)) => {
                let root = root.clone();
                let id = id.clone();
                std::thread::spawn(move || {
                    match wire::connect(&root, &json!({"op":"tunnel","id":id,"port":port})) {
                        Ok((unix, _)) => {
                            let _ = bridge(unix, client);
                        }
                        Err(_) => {
                            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 29\r\nConnection: close\r\n\r\nWorkspace service unavailable");
                        }
                    }
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => return Err(error.into()),
        }
    }
    fs::remove_file(control_path)?;
    fs::remove_file(descriptor)?;
    Ok(())
}

fn unpublish(root: &Path, id: &str, port: u16) -> Result<()> {
    runtime::identifier(id)?;
    let (socket, _) = proxy_paths(root, id, port);
    if socket.exists() {
        let mut client = UnixStream::connect(socket)?;
        client.set_read_timeout(Some(Duration::from_secs(5)))?;
        wire::send(&mut client, &json!({"op":"shutdown"}))?;
        wire::line(&mut client)?;
    }
    Ok(())
}

/// Refuses new starts and wakes, then hibernates every running machine. If one
/// stays running (its capture failed), starts are allowed again and this fails, so
/// the caller does not shut the worker down under it.
fn hibernate_all(root: &Path) -> Result<()> {
    // The AWS unit allows 900 s to stop; the worker captures one machine at a time.
    let deadline = Instant::now() + Duration::from_secs(840);
    // Any request can queue behind a capture already running, so each waits as long as one.
    let request = |value: Value| {
        wire::connect_waiting(root, &value, wire::reply_wait(&json!({"op":"hibernate"})))
            .map(|(_, v)| v)
    };
    let result = (|| -> Result<()> {
        request(json!({"op":"quiesce"}))?;
        let list = request(json!({"op":"list"}))?;
        for workspace in list.as_array().context("workspace list")? {
            let (Some(id), Some("running")) =
                (workspace["id"].as_str(), workspace["state"].as_str())
            else {
                continue;
            };
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_secs(1));
            match wire::connect_waiting(root, &json!({"op":"hibernate","id":id}), wait) {
                Ok(_) => println!("hibernated {id}"),
                Err(e) => eprintln!("could not hibernate {id}: {e:#}"),
            }
        }
        let list = request(json!({"op":"list"}))?;
        let running: Vec<_> = list
            .as_array()
            .context("workspace list")?
            .iter()
            .filter(|w| w["state"] == "running")
            .filter_map(|w| w["id"].as_str())
            .collect();
        ensure!(
            running.is_empty(),
            "could not hibernate {}; the worker keeps running (nothing was shut down)",
            running.join(", ")
        );
        Ok(())
    })();
    if result.is_err() {
        // Called off: the worker keeps running, so it must accept starts and wakes again.
        // Sent even when the quiesce itself failed: a timed-out one may still be applied.
        let _ = request(json!({"op":"quiesce","closing":false}));
    }
    result
}

/// Shut down local HTTP proxies, all of them or only one workspace's.
fn unpublish_all(root: &Path, workspace: Option<&str>) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.extension().is_some_and(|x| x == "json")
            && path
                .file_name()
                .is_some_and(|x| x.to_string_lossy().starts_with("p-"))
        {
            let value: Value = serde_json::from_slice(&fs::read(path)?)?;
            let id = value["workspace"]
                .as_str()
                .context("invalid proxy descriptor")?;
            if workspace.is_none_or(|w| w == id) {
                unpublish(
                    root,
                    id,
                    value["guest_port"]
                        .as_u64()
                        .context("invalid proxy descriptor")? as u16,
                )?;
            }
        }
    }
    Ok(())
}

fn supervise(root: &Path) -> Result<()> {
    struct Children(Vec<std::process::Child>);
    impl Drop for Children {
        fn drop(&mut self) {
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let output = Command::new("ip")
        .args(["-j", "-4", "addr", "show"])
        .output()?;
    ensure!(output.status.success(), "cannot inspect host addresses");
    let links: Value = serde_json::from_slice(&output.stdout)?;
    let addresses: Vec<Value> = links
        .as_array()
        .context("host addresses")?
        .iter()
        .flat_map(|link| link["addr_info"].as_array().into_iter().flatten())
        .filter_map(|a| a.get("local").cloned())
        .collect();
    fn owned_child(command: &mut Command) -> Result<std::process::Child> {
        let parent = std::process::id();
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0
                    || libc::getppid() != parent as i32
                {
                    return Err(io::Error::other("supervisor exited during spawn"));
                }
                Ok(())
            });
        }
        Ok(command.spawn()?)
    }
    let mut children = Children(vec![owned_child(
        Command::new("unshare")
            .args(["--user", "--map-root-user", "--net"])
            .arg(std::env::current_exe()?)
            .arg("--data-dir")
            .arg(root)
            .arg("worker"),
    )?]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if wire::request(root, json!({"op":"status"})).is_ok() {
            break;
        }
        ensure!(
            children.0[0].try_wait()?.is_none() && Instant::now() < deadline,
            "worker initialization failed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    children.0.push(
        owned_child(
            Command::new(assets().join("bin/slirp4netns"))
                .args([
                    "--configure",
                    "--disable-host-loopback",
                    "--enable-sandbox",
                    "--enable-seccomp",
                ])
                .arg(children.0[0].id().to_string())
                .arg("tap-egress"),
        )
        .context("install pinned slirp4netns using scripts/prepare-network.py")?,
    );
    loop {
        let result = wire::request(
            root,
            json!({"op":"egress-enable","host_addresses":addresses}),
        );
        if result.is_ok() {
            break;
        }
        ensure!(
            children.0[1].try_wait()?.is_none() && Instant::now() < deadline,
            "Internet uplink initialization failed: {}",
            result.unwrap_err()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    loop {
        if children.0[0].try_wait()?.is_some() {
            return Ok(());
        }
        if children.0[1].try_wait()?.is_some() {
            let _ = wire::request(root, json!({"op":"shutdown"}));
            bail!("Internet uplink exited; worker stopped fail-closed");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn up(root: &Path) -> Result<Value> {
    if let Ok(status) = wire::request(root, json!({"op":"status"})) {
        return Ok(status);
    }
    let logs = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("worker.log"))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--data-dir")
        .arg(root)
        .arg("supervisor")
        .stdin(Stdio::null())
        .stdout(Stdio::from(logs.try_clone()?))
        .stderr(Stdio::from(logs));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let started = Instant::now();
    loop {
        if let Ok(status) = wire::request(root, json!({"op":"status"}))
            && status["internet"] == true
        {
            return Ok(status);
        }
        if child.try_wait()?.is_some() {
            bail!(
                "worker failed; inspect {}",
                root.join("worker.log").display()
            );
        }
        if started.elapsed() > Duration::from_secs(15) {
            let _ = child.kill();
            bail!("worker startup timed out");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn publish(root: &Path, id: &str, port: u16) -> Result<Value> {
    runtime::identifier(id)?;
    ensure!(port > 0, "invalid port");
    wire::request(root, json!({"op":"inspect","id":id}))?;
    let (socket, descriptor) = proxy_paths(root, id, port);
    if let Ok(mut stream) = UnixStream::connect(&socket) {
        wire::send(&mut stream, &json!({"op":"status"}))?;
        wire::line(&mut stream)?;
        return Ok(serde_json::from_slice(&fs::read(&descriptor)?)?);
    }
    if descriptor.exists() {
        fs::remove_file(&descriptor)?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join(format!("p-{id}-{port}.log")))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--data-dir")
        .arg(root)
        .args(["gateway", id, &port.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let started = Instant::now();
    loop {
        if descriptor.exists() {
            return Ok(serde_json::from_slice(&fs::read(&descriptor)?)?);
        }
        if child.try_wait()?.is_some() {
            bail!("gateway failed; inspect its log");
        }
        if started.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            bail!("gateway startup timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn execute(root: &Path, id: &str, command: &str, operation: &str) -> Result<i32> {
    let result = wire::request(root, json!({"op":operation,"id":id,"command":command}))?;
    print!("{}", result["output"].as_str().unwrap_or(""));
    io::stdout().flush()?;
    Ok(result["exit_code"].as_i64().unwrap_or(1) as i32)
}

pub fn run(cli: Cli) -> Result<i32> {
    let root = data_root(cli.data_dir)?;
    let request = match cli.command {
        Action::NodeController {
            listen,
            tls_cert,
            tls_key,
            insecure_loopback_test,
        } => {
            crate::nodes::controller(&root, listen, tls_cert, tls_key, insecure_loopback_test)?;
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
            if cpus == 16 && controller.is_none() {
                crate::nodes::mint(&root, &node, &output, ttl, memory, slots)?;
            } else {
                let controller = controller
                    .map(|value| crate::nodes::origin(&value, false))
                    .transpose()?;
                crate::nodes::invitation(
                    &root,
                    &node,
                    ttl,
                    memory,
                    slots,
                    cpus,
                    controller.as_deref(),
                    |v| crate::nodes::write_private(&output, v),
                )?;
            }
            return Ok(0);
        }
        Action::NodeRevoke { node } => {
            crate::nodes::revoke(&root, &node)?;
            return Ok(0);
        }
        Action::Nodes => {
            println!("{}", crate::nodes::inventory(&root)?);
            return Ok(0);
        }
        Action::NodeAgent {
            controller,
            credential,
            join,
            ca_cert,
            insecure_loopback_test,
        } => {
            // SIGTERM (a runner host restarting its control plane): no new jobs, the
            // accepted ones finish, then exit; the worker and its VMs keep running.
            crate::selfhost::stop_on_signal()?;
            let (done, finished) = std::sync::mpsc::channel();
            let agent_root = root.clone();
            std::thread::spawn(move || {
                let _ = done.send(crate::nodes::agent(
                    &agent_root,
                    &controller,
                    &credential,
                    join.as_deref(),
                    ca_cert.as_deref(),
                    insecure_loopback_test,
                ));
            });
            loop {
                match finished.recv_timeout(Duration::from_millis(200)) {
                    Ok(result) => {
                        result?;
                        return Ok(0);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                        if crate::nodes::STOPPING.load(std::sync::atomic::Ordering::SeqCst) =>
                    {
                        crate::selfhost::drain(Duration::from_secs(900));
                        let _ = crate::nodes::agent_status(&root, false, &json!({}));
                        return Ok(0);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Action::Dashboard { config, listen } => {
            remote::run(&config, listen, Some(root))?;
            return Ok(0);
        }
        Action::Serve { config, listen } => {
            remote::run(&config, listen, None)?;
            return Ok(0);
        }
        Action::Login { .. }
        | Action::Host { .. }
        | Action::Node { .. }
        | Action::Backup { .. }
        | Action::Image { .. }
        | Action::Share { .. }
        | Action::Unshare { .. } => unreachable!(),
        Action::Supervisor => {
            supervise(&root)?;
            return Ok(0);
        }
        Action::Worker => {
            worker(root)?;
            return Ok(0);
        }
        Action::Gateway { id, port } => {
            gateway(root, id, port)?;
            return Ok(0);
        }
        Action::Up => {
            println!("{}", serde_json::to_string_pretty(&up(&root)?)?);
            return Ok(0);
        }
        Action::Down { hibernate } => {
            if hibernate && root.join("control.sock").exists() {
                hibernate_all(&root)?;
                if let Err(e) = unpublish_all(&root, None) {
                    // Called off after the hibernation: the worker keeps running and accepts starts.
                    let _ = wire::request(&root, json!({"op":"quiesce","closing":false}));
                    return Err(e);
                }
            } else {
                unpublish_all(&root, None)?;
            }
            if root.join("control.sock").exists() {
                wire::request(&root, json!({"op":"shutdown"}))?;
                let started = Instant::now();
                while root.join("control.sock").exists() {
                    ensure!(
                        started.elapsed() < Duration::from_secs(5),
                        "worker shutdown timed out"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            println!("Local worker stopped; workspace disks and snapshots retained.");
            return Ok(0);
        }
        Action::Status => json!({"op":"status"}),
        Action::Stats => json!({"op":"stats"}),
        Action::List => json!({"op":"list"}),
        Action::Inspect { id } => json!({"op":"inspect","id":id}),
        Action::Create {
            id,
            memory,
            image,
            cpus,
            disk,
            node,
        } => {
            ensure!(node.is_none(), "--node requires a remote controller");
            json!({"op":"create","id":id,"memory_mib":memory,"image":image,"vcpu_count":cpus,"disk_gib":disk.unwrap_or(common::DEFAULT_DISK_GIB)})
        }
        Action::Resize {
            id,
            memory,
            cpus,
            disk,
        } => {
            ensure!(
                memory.is_some() || cpus.is_some() || disk.is_some(),
                "specify --memory, --cpus or --disk"
            );
            let mut request = json!({"op":"resize","id":id,"memory_mib":memory,"vcpu_count":cpus,"disk_gib":disk});
            request
                .as_object_mut()
                .unwrap()
                .retain(|_, value| !value.is_null());
            request
        }
        Action::Start { id } => json!({"op":"start","id":id}),
        Action::Stop { id } => json!({"op":"stop","id":id}),
        Action::Exec { id, command } => {
            let text = if command.len() == 1 {
                command[0].clone()
            } else {
                command
                    .iter()
                    .map(|arg| vm::shell_quote(arg))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            return execute(&root, &id, &text, "exec");
        }
        Action::Shell { id } => return terminal::local(&root, &id),
        Action::SshProxy { id } => return crate::ssh_client::local_proxy(&root, &id),
        Action::Ssh { id, args } => {
            let info = wire::request(&root, json!({"op":"ssh-info","id":id}))?;
            return crate::ssh_client::launch(None, Some(&root), &id, &info, &args);
        }
        Action::SshConfig { id } => {
            let info = wire::request(&root, json!({"op":"ssh-info","id":id}))?;
            crate::ssh_client::config(None, Some(&root), &id, &info)?;
            return Ok(0);
        }
        Action::SshAuthorize { id, key, upgrade } => {
            json!({"op":"ssh-keys","id":id,"keys":crate::ssh_client::public_keys(&key)?,"upgrade":upgrade})
        }
        Action::SshRevoke { id } => json!({"op":"ssh-keys","id":id,"keys":[],"upgrade":false}),
        Action::SshInfo { id } => json!({"op":"ssh-info","id":id}),
        Action::Put { id, local, guest } => {
            ensure!(
                fs::metadata(&local)?.len() <= 256 * 1024,
                "prototype uploads limited to 256 KiB"
            );
            json!({"op":"put","id":id,"path":guest,"data":STANDARD.encode(fs::read(local)?)})
        }
        Action::Get { id, guest, local } => {
            let value = wire::request(&root, json!({"op":"get","id":id,"path":guest}))?;
            let bytes = STANDARD.decode(value["data"].as_str().context("missing file data")?)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(local)
                .context("download destination must not already exist")?;
            file.write_all(&bytes)?;
            println!("Downloaded {} bytes", bytes.len());
            return Ok(0);
        }
        Action::Snapshot { id, name } => json!({"op":"snapshot","id":id,"name":name}),
        Action::Snapshots => json!({"op":"snapshots"}),
        Action::Fork {
            id,
            child,
            snapshot,
        } => json!({"op":"fork","id":id,"child":child,"snapshot":snapshot}),
        Action::Hibernate { id } => json!({"op":"hibernate","id":id}),
        Action::Delete { id } => {
            // Publications close only once the worker has accepted the delete.
            let result = wire::request(&root, json!({"op":"delete","id":id}))?;
            unpublish_all(&root, Some(&id))?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            return Ok(0);
        }
        Action::Restore { id, name } => json!({"op":"restore","id":id,"name":name}),
        Action::Publish { id, port } => {
            let result = publish(&root, &id, port)?;
            println!("{}", result["url"].as_str().context("missing URL")?);
            return Ok(0);
        }
        Action::Unpublish { id, port } => {
            if let Some(port) = port {
                unpublish(&root, &id, port)?;
            } else {
                unpublish_all(&root, Some(&id))?;
            }
            return Ok(0);
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&wire::request(&root, request)?)?
    );
    Ok(0)
}

#[cfg(test)]
mod awake_tests {
    use super::*;

    struct Fake(ow_core::idle::Idle);
    impl Streams for Fake {
        fn idle(&mut self) -> &mut ow_core::idle::Idle {
            &mut self.0
        }
    }

    #[test]
    fn a_failed_connection_after_opening_releases_the_runtime() {
        let runtime = Arc::new(Mutex::new(Fake(ow_core::idle::Idle::new(Some(
            Duration::from_secs(60),
        )))));
        let (done, finished) = std::sync::mpsc::channel();
        let shared = runtime.clone();
        std::thread::spawn(move || {
            // The tunnel path: open, then a refused connection drops the guard.
            let result = (|| -> Result<Awake<Fake>> {
                let locked = shared.lock().unwrap();
                let awake = Awake::open(&shared, locked, "box");
                TcpStream::connect_timeout(&"127.0.0.1:1".parse()?, Duration::from_secs(1))?;
                Ok(awake)
            })();
            let _ = done.send(result.is_err());
        });
        assert_eq!(
            finished.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "worker deadlocked"
        );
        let later = Instant::now() + Duration::from_secs(120);
        // The stream was closed again, so the box can go idle.
        assert!(runtime.lock().unwrap().0.due("box", 0, 0, later));
    }
}

#[cfg(test)]
mod down_tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A stand-in worker on `control.sock`: box `a` runs; its hibernation fails
    /// unless `capture_ok`. Returns the data dir and the requests it saw.
    fn worker(capture_ok: bool) -> (PathBuf, Arc<Mutex<Vec<Value>>>) {
        worker_with(capture_ok, false, false)
    }

    fn worker_with(
        capture_ok: bool,
        second_list_fails: bool,
        quiesce_fails: bool,
    ) -> (PathBuf, Arc<Mutex<Vec<Value>>>) {
        let root = std::env::temp_dir().join(format!("ow-down-{}", vm::nonce().unwrap()));
        fs::create_dir(&root).unwrap();
        let listener = UnixListener::bind(root.join("control.sock")).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            let (mut slept, mut lists) = (false, 0);
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let request = wire::line(&mut stream).unwrap();
                log.lock().unwrap().push(request.clone());
                let state = if slept { "hibernated" } else { "running" };
                let response = match request["op"].as_str().unwrap() {
                    "quiesce" if quiesce_fails && request["closing"] != false => {
                        json!({"ok":false,"error":"timed out"})
                    }
                    "list" if second_list_fails && lists == 1 => {
                        json!({"ok":false,"error":"worker busy"})
                    }
                    "list" => {
                        lists += 1;
                        json!({"ok":true,"result":[{"id":"a","state":state},{"id":"b","state":"stopped"}]})
                    }
                    "hibernate" if capture_ok => {
                        slept = true;
                        json!({"ok":true,"result":{}})
                    }
                    "hibernate" => json!({"ok":false,"error":"VMM /snapshot/create: timed out"}),
                    _ => json!({"ok":true,"result":{}}),
                };
                wire::send(&mut stream, &response).unwrap();
            }
        });
        (root, seen)
    }

    fn ops(seen: &Arc<Mutex<Vec<Value>>>) -> Vec<String> {
        seen.lock()
            .unwrap()
            .iter()
            .map(|r| r["op"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn a_failed_hibernation_calls_off_the_shutdown() {
        let (root, seen) = worker(false);
        let error = hibernate_all(&root).unwrap_err().to_string();
        assert!(error.contains("could not hibernate a"), "{error}");
        assert_eq!(
            ops(&seen),
            ["quiesce", "list", "hibernate", "list", "quiesce"]
        );
        assert_eq!(
            seen.lock().unwrap()[4]["closing"],
            false,
            "starts are allowed again"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_failed_check_after_the_round_also_allows_starts_again() {
        let (root, seen) = worker_with(true, true, false);
        assert!(hibernate_all(&root).is_err());
        assert_eq!(
            ops(&seen),
            ["quiesce", "list", "hibernate", "list", "quiesce"]
        );
        assert_eq!(seen.lock().unwrap()[4]["closing"], false);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_failed_quiesce_is_rolled_back_too() {
        let (root, seen) = worker_with(true, false, true);
        assert!(hibernate_all(&root).is_err());
        assert_eq!(ops(&seen), ["quiesce", "quiesce"]);
        assert_eq!(seen.lock().unwrap()[1]["closing"], false);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn running_machines_are_hibernated_with_starts_refused() {
        let (root, seen) = worker(true);
        hibernate_all(&root).unwrap();
        assert_eq!(ops(&seen), ["quiesce", "list", "hibernate", "list"]);
        assert_eq!(
            seen.lock().unwrap()[2]["id"],
            "a",
            "stopped machines are left alone"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
