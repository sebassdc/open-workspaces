use crate::runtime::Runtime;
use crate::{Action, Cli, assets, network, remote, runtime, terminal, vm, wire};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
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

fn data_root(value: Option<PathBuf>) -> Result<PathBuf> {
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
    while !stopping.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let runtime = runtime.clone();
                let stopping = stopping.clone();
                let result = (|| -> Result<()> {
                    assert_owner(&stream)?;
                    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
                    let request = wire::line(&mut stream)?;
                    let operation = request["op"].as_str().unwrap_or("");
                    if operation == "terminal" {
                        struct Slot(Arc<std::sync::atomic::AtomicUsize>);
                        impl Drop for Slot {
                            fn drop(&mut self) {
                                self.0.fetch_sub(1, Ordering::SeqCst);
                            }
                        }
                        let connection = (|| -> Result<(TcpStream, Slot)> {
                            terminals
                                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                                    (n < 16).then_some(n + 1)
                                })
                                .map_err(|_| anyhow::anyhow!("terminal capacity reached"))?;
                            let slot = Slot(terminals.clone());
                            let id = request["id"].as_str().context("missing ID")?;
                            let tcp = runtime
                                .lock()
                                .map_err(|_| anyhow::anyhow!("runtime lock poisoned"))?
                                .terminal(id)?;
                            Ok((tcp, slot))
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
                        let connection = (|| -> Result<TcpStream> {
                            let id = request["id"].as_str().context("missing ID")?;
                            let port = request["port"].as_u64().context("missing port")?;
                            ensure!((1..=65535).contains(&port), "invalid port");
                            let address = runtime
                                .lock()
                                .map_err(|_| anyhow::anyhow!("runtime lock poisoned"))?
                                .address(id)?;
                            Ok(TcpStream::connect_timeout(
                                &format!("{address}:{port}").parse()?,
                                Duration::from_secs(5),
                            )?)
                        })();
                        match connection {
                            Ok(tcp) => {
                                wire::send(&mut stream, &json!({"ok":true,"result":{}}))?;
                                std::thread::spawn(move || {
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
                            .request(&request);
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
        } => {
            crate::nodes::mint(&root, &node, &output, ttl, memory, slots)?;
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
            crate::nodes::agent(
                &root,
                &controller,
                &credential,
                join.as_deref(),
                ca_cert.as_deref(),
                insecure_loopback_test,
            )?;
            return Ok(0);
        }
        Action::Dashboard { config, listen } => {
            remote::run(&config, listen, Some(root))?;
            return Ok(0);
        }
        Action::Serve { config, listen } => {
            remote::run(&config, listen, None)?;
            return Ok(0);
        }
        Action::Login { .. } => unreachable!(),
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
        Action::Down => {
            for entry in fs::read_dir(&root)? {
                let path = entry?.path();
                if path.extension().is_some_and(|x| x == "json")
                    && path
                        .file_name()
                        .is_some_and(|x| x.to_string_lossy().starts_with("p-"))
                {
                    let value: Value = serde_json::from_slice(&fs::read(path)?)?;
                    unpublish(
                        &root,
                        value["workspace"]
                            .as_str()
                            .context("invalid proxy descriptor")?,
                        value["guest_port"]
                            .as_u64()
                            .context("invalid proxy descriptor")? as u16,
                    )?;
                }
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
            node,
        } => {
            ensure!(node.is_none(), "--node requires a remote controller");
            json!({"op":"create","id":id,"memory_mib":memory,"image":image,"vcpu_count":cpus})
        }
        Action::Resize { id, memory, cpus } => {
            json!({"op":"resize","id":id,"memory_mib":memory,"vcpu_count":cpus})
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
        Action::Restore { id, name } => json!({"op":"restore","id":id,"name":name}),
        Action::Publish { id, port } => {
            let result = publish(&root, &id, port)?;
            println!("{}", result["url"].as_str().context("missing URL")?);
            return Ok(0);
        }
        Action::Unpublish { id, port } => {
            unpublish(&root, &id, port)?;
            return Ok(0);
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&wire::request(&root, request)?)?
    );
    Ok(0)
}
