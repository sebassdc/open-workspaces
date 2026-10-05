//! One connection, one guest PTY. No host credentials or management capabilities.
mod native;
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const LIMIT: usize = 65536;
struct Session(Child, bool);
impl Session {
    fn finish(&mut self) -> i32 {
        if self.1 {
            return 137;
        }
        // Job control creates additional process groups in this terminal's session.
        // pidfds bind signals to the observed guest process, preventing PID-reuse races.
        if let Ok(entries) = fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|s| s.parse::<i32>().ok())
                else {
                    continue;
                };
                let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
                if fd < 0 {
                    continue;
                }
                let descriptor = unsafe { File::from_raw_fd(fd) };
                if let Ok(stat) = fs::read_to_string(entry.path().join("stat")) {
                    let sid = stat
                        .rsplit_once(')')
                        .and_then(|(_, rest)| rest.split_whitespace().nth(3))
                        .and_then(|s| s.parse::<u32>().ok());
                    if sid == Some(self.0.id()) {
                        unsafe {
                            libc::syscall(
                                libc::SYS_pidfd_send_signal,
                                descriptor.as_raw_fd(),
                                libc::SIGKILL,
                                std::ptr::null::<libc::siginfo_t>(),
                                0,
                            );
                        }
                    }
                }
            }
        }
        // Keep the child unreaped until signaling its owned process group: no reused PID.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let status = self.0.wait().ok();
        self.1 = true;
        status.and_then(|s| s.code()).unwrap_or(137)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if !self.1 {
            self.finish();
        }
    }
}
fn frame(kind: u8, bytes: &[u8]) -> Vec<u8> {
    let mut value = vec![kind];
    value.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    value.extend_from_slice(bytes);
    value
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid terminal frame")
}
fn nonblock(fd: i32) -> io::Result<()> {
    if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn terminal(mut tcp: TcpStream) -> io::Result<()> {
    let (mut master_fd, mut slave_fd) = (-1, -1);
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    unsafe {
        libc::fcntl(master_fd, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(slave_fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    let developer = std::fs::read_to_string("/etc/ow-developer").is_ok_and(|s| s.trim() == "dev")
        && std::path::Path::new("/home/dev").is_dir();
    let home = if developer { "/home/dev" } else { "/root" };
    let user = if developer { "dev" } else { "root" };
    if developer && unsafe { libc::fchown(slave_fd, 1000, 1000) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut command = Command::new(if std::path::Path::new("/bin/bash").is_file() {
        "/bin/bash"
    } else {
        "/bin/sh"
    });
    command
        .arg("-i")
        .env("TERM", "xterm-256color")
        .env("PS1", if developer { "\\w $ " } else { "\\w # " })
        .env("HOME", home)
        .env("USER", user)
        .env("LOGNAME", user)
        .current_dir(home)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    unsafe {
        command.pre_exec(move || {
            // This agent is launched as a background management-console job. Do not
            // inherit that job's ignored signals into the interactive guest shell.
            for signal in [
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGHUP,
                libc::SIGTERM,
                libc::SIGTSTP,
                libc::SIGTTIN,
                libc::SIGTTOU,
                libc::SIGCHLD,
            ] {
                libc::signal(signal, libc::SIG_DFL);
            }
            let mut mask = std::mem::zeroed::<libc::sigset_t>();
            libc::sigemptyset(&mut mask);
            libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            if developer {
                let group: libc::gid_t = 1000;
                if libc::setgroups(1, &group) != 0
                    || libc::setgid(group) != 0
                    || libc::setuid(1000) != 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut session = Session(command.spawn()?, false);
    drop(command); // Release the parent's slave FDs so EOF follows shell exit.
    nonblock(master_fd)?;
    tcp.set_nonblocking(true)?;
    let mut incoming = Vec::new();
    let mut input = Vec::new();
    let mut output = Vec::new();
    let mut ended = false;
    let started = Instant::now();
    let mut last_write = Instant::now();
    let mut exit_code = None;
    loop {
        if exit_code.is_none() {
            let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    session.0.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } == 0
                && unsafe { info.si_pid() } != 0
            {
                exit_code = Some(session.finish());
            }
        }
        if started.elapsed() > Duration::from_secs(3600)
            || (!output.is_empty() && last_write.elapsed() > Duration::from_secs(30))
        {
            break;
        }
        if ended && output.is_empty() {
            break;
        }
        let mut fds = [
            libc::pollfd {
                fd: tcp.as_raw_fd(),
                events: libc::POLLIN | if output.is_empty() { 0 } else { libc::POLLOUT },
                revents: 0,
            },
            libc::pollfd {
                fd: master_fd,
                events: if ended {
                    0
                } else {
                    (if output.len() < LIMIT - 8192 {
                        libc::POLLIN
                    } else {
                        0
                    }) | if input.is_empty() { 0 } else { libc::POLLOUT }
                },
                revents: 0,
            },
        ];
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, 100) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            break;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            let mut bytes = [0; 4096];
            match tcp.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => incoming.extend_from_slice(&bytes[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
            while incoming.len() >= 5 {
                let len = u32::from_be_bytes(incoming[1..5].try_into().unwrap()) as usize;
                if len > 4096 {
                    return Err(invalid());
                }
                if incoming.len() < len + 5 {
                    break;
                }
                match (incoming[0], len) {
                    (0, _) => {
                        if input.len() + len > LIMIT {
                            return Err(invalid());
                        }
                        input.extend_from_slice(&incoming[5..len + 5]);
                    }
                    (1, 4) => {
                        let cols = u16::from_be_bytes(incoming[5..7].try_into().unwrap());
                        let rows = u16::from_be_bytes(incoming[7..9].try_into().unwrap());
                        if !(1..=500).contains(&cols) || !(1..=300).contains(&rows) {
                            return Err(invalid());
                        }
                        let size = libc::winsize {
                            ws_row: rows,
                            ws_col: cols,
                            ws_xpixel: 0,
                            ws_ypixel: 0,
                        };
                        if unsafe { libc::ioctl(master_fd, libc::TIOCSWINSZ, &size) } < 0 {
                            return Err(io::Error::last_os_error());
                        }
                    }
                    _ => return Err(invalid()),
                }
                incoming.drain(..len + 5);
            }
        }
        if fds[1].revents & libc::POLLOUT != 0 && !input.is_empty() {
            match master.write(&input) {
                Ok(n) => {
                    input.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        if !ended
            && fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
            && output.len() < LIMIT - 8192
        {
            let mut bytes = [0; 4096];
            match master.read(&mut bytes) {
                Ok(0) => ended = true,
                Ok(n) => output.extend(frame(0, &bytes[..n])),
                Err(e) if e.raw_os_error() == Some(libc::EIO) => ended = true,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
            if ended {
                let code = exit_code.unwrap_or_else(|| session.finish());
                output.extend(frame(2, &code.to_be_bytes()));
            }
        }
        if fds[0].revents & libc::POLLOUT != 0 && !output.is_empty() {
            match tcp.write(&output) {
                Ok(0) => break,
                Ok(n) => {
                    output.drain(..n);
                    last_write = Instant::now();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        } else if output.is_empty() {
            last_write = Instant::now();
        }
    }
    Ok(())
}
fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a == "--vsock") {
        return native::run();
    }
    if args.len() != 4 {
        return Err(invalid());
    }
    let listener = TcpListener::bind(format!("{}:0", args[1]))?;
    listener.set_nonblocking(true)?;
    fs::write(&args[3], listener.local_addr()?.port().to_string())?;
    let started = Instant::now();
    loop {
        match listener.accept() {
            Ok((tcp, peer)) => {
                if peer.ip().to_string() != args[2] {
                    continue;
                }
                return terminal(tcp);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if started.elapsed() > Duration::from_secs(15) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
}
