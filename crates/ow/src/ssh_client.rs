//! Ordinary OpenSSH launcher and binary-only ProxyCommand transport.
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    io::{Read, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use tungstenite::{Message, client::IntoClientRequest, stream::MaybeTlsStream};
const FRAME: usize = 16384;
pub fn public_keys(paths: &[PathBuf]) -> Result<Vec<String>> {
    ensure!(paths.len() <= 16, "at most 16 public keys");
    paths
        .iter()
        .map(|p| {
            ensure!(
                std::fs::metadata(p)?.len() <= 512,
                "public key file too large"
            );
            let s = std::fs::read_to_string(p)?;
            let s = s.trim_end_matches('\n');
            ensure!(
                !s.contains(['\r', '\n']) && s.starts_with("ssh-ed25519 "),
                "plain Ed25519 public key file required; never supply a private key"
            );
            Ok(s.to_owned())
        })
        .collect()
}
fn proxy_command(server: Option<&str>, root: Option<&Path>, id: &str) -> Result<String> {
    crate::common::identifier(id)?;
    let executable = std::env::current_exe()?;
    let quote = |s: &str| crate::common::shell_quote(&s.replace('%', "%%"));
    let mut cmd = quote(
        executable
            .to_str()
            .context("UTF-8 executable path required")?,
    );
    if let Some(url) = server {
        cmd += &format!(" --server {}", quote(url));
    } else {
        cmd += " --local";
        cmd += &format!(
            " --data-dir {}",
            quote(
                root.context("local root required")?
                    .to_str()
                    .context("UTF-8 root required")?
            )
        );
    }
    cmd += &format!(" ssh-proxy {}", quote(id));
    ensure!(
        !cmd.contains(['\r', '\n']),
        "ProxyCommand paths must not contain line breaks"
    );
    Ok(cmd)
}
fn alias(info: &Value) -> Result<String> {
    let id = info["identity"].as_str().context("SSH identity missing")?;
    crate::common::identifier(id)?;
    Ok(format!("ow-{id}"))
}
pub fn launch(
    server: Option<&str>,
    root: Option<&Path>,
    id: &str,
    info: &Value,
    args: &[String],
) -> Result<i32> {
    let proxy = proxy_command(server, root, id)?;
    let alias = alias(info)?;
    eprintln!(
        "Guest host public key: {}",
        info["host_key"].as_str().context("host key missing")?
    );
    let status = Command::new("ssh")
        .args([
            "-o",
            &format!("ProxyCommand={proxy}"),
            "-o",
            &format!("HostKeyAlias={alias}"),
            "-o",
            "StrictHostKeyChecking=ask",
            "-l",
            "dev",
        ])
        .arg(id)
        .args(args)
        .status()
        .context("OpenSSH ssh client is required")?;
    Ok(status.code().unwrap_or(255))
}
pub fn config(server: Option<&str>, root: Option<&Path>, id: &str, info: &Value) -> Result<()> {
    let proxy = proxy_command(server, root, id)?;
    let alias = alias(info)?;
    println!(
        "# Verify guest host public key: {}\nHost ow-{id}\n    HostName {id}\n    User dev\n    HostKeyAlias {alias}\n    StrictHostKeyChecking ask\n    ProxyCommand {proxy}",
        info["host_key"].as_str().context("host key missing")?
    );
    Ok(())
}
fn input_ready() -> Result<bool> {
    let mut fd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut fd, 1, 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(fd.revents & (libc::POLLIN | libc::POLLHUP) != 0)
}
struct OutputFlags(i32);
impl OutputFlags {
    fn new() -> Result<Self> {
        let flags = unsafe { libc::fcntl(1, libc::F_GETFL) };
        ensure!(
            flags >= 0 && unsafe { libc::fcntl(1, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0,
            "stdout unavailable"
        );
        Ok(Self(flags))
    }
}
impl Drop for OutputFlags {
    fn drop(&mut self) {
        unsafe {
            libc::fcntl(1, libc::F_SETFL, self.0);
        }
    }
}
fn output(mut bytes: &[u8]) -> Result<()> {
    let start = Instant::now();
    while !bytes.is_empty() {
        ensure!(
            start.elapsed() < Duration::from_secs(5),
            "SSH output stalled"
        );
        let mut fd = libc::pollfd {
            fd: 1,
            events: libc::POLLOUT,
            revents: 0,
        };
        ensure!(
            unsafe { libc::poll(&mut fd, 1, 100) } >= 0,
            "SSH output poll failed"
        );
        if fd.revents == 0 {
            continue;
        }
        let n = unsafe { libc::write(1, bytes.as_ptr().cast(), bytes.len()) };
        let written = if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        };
        match written {
            Ok(0) => bail!("SSH stdout closed"),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
#[cfg(target_os = "linux")]
pub fn local_proxy(root: &Path, id: &str) -> Result<i32> {
    crate::common::identifier(id)?;
    let (mut stream, _) = crate::wire::connect(root, &serde_json::json!({"op":"ssh","id":id}))?;
    let _output = OutputFlags::new()?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = [0; FRAME];
    loop {
        let mut fds = [
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        ensure!(
            unsafe { libc::poll(fds.as_mut_ptr(), 2, 200) } >= 0,
            "SSH poll failed"
        );
        if fds[0].revents != 0 {
            let n = std::io::stdin().read(&mut buf)?;
            if n == 0 {
                return Ok(0);
            }
            stream.write_all(&buf[..n])?;
        }
        if fds[1].revents != 0 {
            let n = stream.read(&mut buf)?;
            if n == 0 {
                return Ok(0);
            }
            output(&buf[..n])?;
        }
    }
}
pub fn remote_proxy(url: &str, id: &str) -> Result<i32> {
    crate::common::identifier(id)?;
    let token = crate::client::token(url)?;
    let mut request =
        format!("{}/api/ssh/{id}", url.replacen("https://", "wss://", 1)).into_client_request()?;
    request.headers_mut().insert("origin", url.parse()?);
    request
        .headers_mut()
        .insert("cf-access-token", token.parse()?);
    let cfg = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(FRAME))
        .max_frame_size(Some(FRAME))
        .write_buffer_size(0)
        .max_write_buffer_size(65536);
    // No redirect following. Bound DNS/TCP and absolute TLS/HTTP handshake,
    // including a peer that sends one byte just before each socket timeout.
    use std::net::ToSocketAddrs;
    let parsed = reqwest::Url::parse(url)?;
    let host = parsed
        .host_str()
        .context("SSH gateway host required")?
        .to_owned();
    let port = parsed
        .port_or_known_default()
        .context("SSH gateway port required")?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = (host.as_str(), port)
            .to_socket_addrs()
            .map(|a| a.take(16).collect::<Vec<_>>());
        let _ = tx.send(result);
    });
    let addresses = rx
        .recv_timeout(Duration::from_secs(5))
        .context("SSH DNS deadline")??;
    let started = Instant::now();
    let mut connection = None;
    for address in addresses {
        let remaining = Duration::from_secs(5).saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        if let Ok(tcp) = std::net::TcpStream::connect_timeout(&address, remaining) {
            connection = Some(tcp);
            break;
        }
    }
    let tcp = connection.context("SSH TCP connection deadline")?;
    tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(5)))?;
    let watchdog = tcp.try_clone()?;
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if matches!(
            done_rx.recv_timeout(Duration::from_secs(10)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ) {
            let _ = watchdog.shutdown(std::net::Shutdown::Both);
        }
    });
    let handshake = tungstenite::client_tls_with_config(request, tcp, Some(cfg), None);
    let _ = done_tx.send(());
    let (mut socket, _) = handshake.map_err(|_| {
        anyhow::anyhow!(
            "verified SSH gateway connection failed; check login, enrollment and machine state"
        )
    })?;
    let tcp = match socket.get_ref() {
        MaybeTlsStream::Plain(_) => bail!("SSH requires TLS"),
        MaybeTlsStream::Rustls(s) => &s.sock,
        _ => bail!("unsupported TLS transport"),
    };
    tcp.set_read_timeout(Some(Duration::from_millis(100)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(5)))?;
    let _output = OutputFlags::new()?;
    let mut buf = [0; FRAME];
    let start = Instant::now();
    let mut active = Instant::now();
    loop {
        ensure!(
            start.elapsed() < Duration::from_secs(3600)
                && active.elapsed() < Duration::from_secs(90),
            "SSH proxy expired"
        );
        if input_ready()? {
            let n = std::io::stdin().read(&mut buf)?;
            if n == 0 {
                let _ = socket.close(None);
                return Ok(0);
            }
            socket.send(Message::Binary(buf[..n].to_vec().into()))?;
            active = Instant::now();
        }
        match socket.read() {
            Ok(Message::Binary(b)) => {
                ensure!(!b.is_empty() && b.len() <= FRAME, "invalid SSH frame");
                output(&b)?;
                active = Instant::now();
            }
            Ok(Message::Ping(b)) => socket.send(Message::Pong(b))?,
            Ok(Message::Pong(_)) => {}
            Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => return Ok(0),
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            _ => bail!("SSH stream disconnected"),
        }
    }
}
