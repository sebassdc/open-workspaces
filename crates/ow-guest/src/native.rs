//! Host-initiated private virtio sockets. No guest-to-host services or networking.
use base64::Engine;
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read, Write},
    net::TcpStream,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn listener(port: u32) -> io::Result<i32> {
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as u16;
    addr.svm_cid = libc::VMADDR_CID_ANY;
    addr.svm_port = port;
    if unsafe {
        libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of_val(&addr) as u32,
        )
    } != 0
        || unsafe { libc::listen(fd, 16) } != 0
    {
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(fd);
        }
        return Err(error);
    }
    Ok(fd)
}
fn accept(fd: i32) -> io::Result<TcpStream> {
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of_val(&addr) as u32;
    let child = unsafe {
        libc::accept4(
            fd,
            &mut addr as *mut _ as *mut libc::sockaddr,
            &mut size,
            libc::SOCK_CLOEXEC,
        )
    };
    if child < 0 {
        return Err(io::Error::last_os_error());
    }
    if addr.svm_cid != libc::VMADDR_CID_HOST {
        unsafe {
            libc::close(child);
        }
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "host CID required",
        ));
    }
    // TcpStream's read/write/timeout operations work on this stream socket; no
    // TCP-specific option or address method is used on the vsock descriptor.
    Ok(unsafe { TcpStream::from_raw_fd(child) })
}
fn execute(text: &str) -> io::Result<Value> {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(text)
        .uid(1000)
        .gid(1000)
        .current_dir("/home/dev")
        .env_clear()
        .env("HOME", "/home/dev")
        .env("USER", "dev")
        .env("PATH", "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = super::Session(cmd.spawn()?, false);
    let mut out = child.0.stdout.take().unwrap();
    let mut err = child.0.stderr.take().unwrap();
    use std::os::fd::AsRawFd;
    super::nonblock(out.as_raw_fd())?;
    super::nonblock(err.as_raw_fd())?;
    let started = Instant::now();
    let mut output = Vec::new();
    fn drain<'a>(
        out: &'a mut dyn Read,
        err: &'a mut dyn Read,
        output: &mut Vec<u8>,
    ) -> io::Result<()> {
        let mut buffer = [0u8; 4096];
        for stream in [out, err] {
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if output.len() + n > 262144 {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "command output limit",
                            ));
                        }
                        output.extend_from_slice(&buffer[..n]);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(())
    }
    let result = (|| {
        loop {
            drain(&mut out, &mut err, &mut output)?;
            let mut state: libc::siginfo_t = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.0.id(),
                    &mut state,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            if unsafe { state.si_pid() } != 0 {
                // Keep the owned leader unreaped until session cleanup, so the
                // process-group ID cannot be reused before signaling it.
                drain(&mut out, &mut err, &mut output)?;
                let code = child.finish();
                return Ok(json!({"output":String::from_utf8_lossy(&output),"exit_code":code}));
            }
            if started.elapsed() > Duration::from_secs(30) {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "command timeout"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();

    result
}
fn rpc(mut socket: TcpStream) -> io::Result<()> {
    socket.set_read_timeout(Some(Duration::from_secs(10)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut bytes = Vec::new();
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new((&socket).take(1024 * 1024 + 1));
    reader.read_until(b'\n', &mut bytes)?;
    if bytes.len() > 1024 * 1024 || bytes.last() != Some(&b'\n') {
        return Err(super::invalid());
    }
    bytes.pop();
    let result = (|| -> io::Result<Value> {
        let v: Value = serde_json::from_slice(&bytes).map_err(|_| super::invalid())?;
        match v["op"].as_str().unwrap_or("") {
            "ready" => Ok(json!({"arch":std::env::consts::ARCH,"user":"dev"})),
            "exec" => execute(v["command"].as_str().ok_or_else(super::invalid)?),
            "put" | "get" => {
                let path = v["path"].as_str().ok_or_else(super::invalid)?;
                // Bounded dev-file API. No symlink/traversal escape even inside
                // the guest: arbitrary privileged filesystem access is absent.
                if !path.starts_with("/home/dev/") || path.split('/').any(|p| p == "..") {
                    return Err(super::invalid());
                }
                // Linux fsuid/fsgid are per-thread; drop filesystem authority
                // for this RPC without changing the privileged PTY broker.
                struct FsUser(i32, i32);
                impl Drop for FsUser {
                    fn drop(&mut self) {
                        unsafe {
                            libc::setfsuid(self.0 as u32);
                            libc::setfsgid(self.1 as u32);
                        }
                    }
                }
                let _user = unsafe {
                    let gid = libc::setfsgid(1000);
                    let uid = libc::setfsuid(1000);
                    FsUser(uid, gid)
                };
                if unsafe { libc::setfsuid(u32::MAX) } != 1000 {
                    return Err(super::invalid());
                }
                use std::os::unix::fs::OpenOptionsExt;
                let mut parent = fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                    .open("/home/dev")?;
                let parts: Vec<_> = path
                    .strip_prefix("/home/dev/")
                    .unwrap()
                    .split('/')
                    .collect();
                if parts
                    .iter()
                    .any(|p| p.is_empty() || *p == "." || *p == "..")
                {
                    return Err(super::invalid());
                }
                for part in &parts[..parts.len() - 1] {
                    let name = std::ffi::CString::new(*part).map_err(|_| super::invalid())?;
                    let fd = unsafe {
                        libc::openat(
                            parent.as_raw_fd(),
                            name.as_ptr(),
                            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                        )
                    };
                    if fd < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    parent = unsafe { fs::File::from_raw_fd(fd) };
                }
                let name =
                    std::ffi::CString::new(*parts.last().unwrap()).map_err(|_| super::invalid())?;
                let flags = if v["op"] == "put" {
                    libc::O_WRONLY | libc::O_CREAT
                } else {
                    libc::O_RDONLY
                };
                let fd = unsafe {
                    libc::openat(
                        parent.as_raw_fd(),
                        name.as_ptr(),
                        flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                        0o600,
                    )
                };
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                let file = unsafe { fs::File::from_raw_fd(fd) };
                if !file.metadata()?.is_file() {
                    return Err(super::invalid());
                }
                if v["op"] == "put" {
                    let data = base64::engine::general_purpose::STANDARD
                        .decode(v["data"].as_str().ok_or_else(super::invalid)?)
                        .map_err(|_| super::invalid())?;
                    if data.len() > 262144 {
                        return Err(super::invalid());
                    }
                    file.set_len(0)?;
                    (&file).write_all(&data)?;
                    file.sync_all()?;
                    parent.sync_all()?;
                    Ok(json!({"bytes":data.len()}))
                } else {
                    let mut data = Vec::new();
                    file.take(262145).read_to_end(&mut data)?;
                    if data.len() > 262144 {
                        return Err(super::invalid());
                    }
                    Ok(json!({"data":base64::engine::general_purpose::STANDARD.encode(data)}))
                }
            }
            _ => Err(super::invalid()),
        }
    })();
    let response = match result {
        Ok(v) => json!({"ok":true,"result":v}),
        Err(e) => json!({"ok":false,"error":e.to_string()}),
    };
    let mut encoded = response.to_string();
    if encoded.len() >= 1024 * 1024 {
        encoded=json!({"ok":false,"error":"encoded command result exceeds control budget; command effects may remain"}).to_string();
    }
    socket.write_all(encoded.as_bytes())?;
    socket.write_all(b"\n")
}
pub fn run() -> io::Result<()> {
    let rpc_fd = listener(7000)?;
    let pty_fd = listener(7001)?;
    for (fd, pty) in [(rpc_fd, false), (pty_fd, true)] {
        std::thread::spawn(move || {
            let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            loop {
                if let Ok(socket) = accept(fd) {
                    use std::sync::atomic::Ordering;
                    if active
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                            (n < 16).then_some(n + 1)
                        })
                        .is_err()
                    {
                        continue;
                    }
                    let active = active.clone();
                    std::thread::spawn(move || {
                        let _ = if pty {
                            super::terminal(socket)
                        } else {
                            rpc(socket)
                        };
                        active.fetch_sub(1, Ordering::SeqCst);
                    });
                } else {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        });
    }
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}
