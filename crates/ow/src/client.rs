//! Remote operator client: browser Access login, HTTPS API, and WSS native terminal.
use crate::{Action, common};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};
use tungstenite::{Message, client::IntoClientRequest, stream::MaybeTlsStream};

fn server(url: &str) -> Result<String> {
    let u = reqwest::Url::parse(url)?;
    ensure!(
        u.scheme() == "https"
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
            && u.path() == "/",
        "server must be an HTTPS origin, e.g. https://workspaces.example.com"
    );
    Ok(u.as_str().trim_end_matches('/').to_owned())
}
fn config() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|v| PathBuf::from(v).join(".config")))
        .context("HOME or XDG_CONFIG_HOME required")?;
    Ok(base.join("open-workspaces/client.json"))
}
pub fn saved_server() -> Result<Option<String>> {
    let path = config()?;
    if !path.exists() {
        return Ok(None);
    }
    let data: Value = serde_json::from_slice(&fs::read(path)?)?;
    Ok(Some(server(
        data["server"]
            .as_str()
            .context("invalid client configuration")?,
    )?))
}
fn cloudflared() -> PathBuf {
    if let Some(path) = std::env::var_os("OW_CLOUDFLARED") {
        return PathBuf::from(path);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let installed = PathBuf::from(home).join(".local/lib/open-workspaces/cloudflared");
        if installed.is_file() {
            return installed;
        }
    }
    let local = crate::assets().join("../remote-access/bin/cloudflared");
    if local.is_file() {
        local
    } else {
        PathBuf::from("cloudflared")
    }
}
pub fn login(url: &str) -> Result<()> {
    let url = server(url)?;
    let status = Command::new(cloudflared())
        .args(["access", "login", &url])
        .stdout(Stdio::null())
        .status()
        .context("install cloudflared on this client for browser login")?;
    ensure!(status.success(), "Cloudflare login failed");
    // Verify the token is cached; never print it or put it in command arguments.
    let _ = token(&url)?;
    let path = config()?;
    fs::create_dir_all(path.parent().unwrap())?;
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700))?;
    let temporary = path.with_extension(format!("{}.tmp", common::nonce()?));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(serde_json::to_string_pretty(&json!({"server":url}))?.as_bytes())?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    println!("Signed in to {url}. Use ow list or ow shell <machine>.");
    Ok(())
}
fn token(url: &str) -> Result<String> {
    let output = Command::new(cloudflared())
        .args(["access", "token", "--app", url])
        .stderr(Stdio::null())
        .output()
        .context("cloudflared is required; run ow login <server>")?;
    ensure!(
        output.status.success(),
        "Cloudflare session unavailable or expired; run ow login {url}"
    );
    let token = String::from_utf8(output.stdout)?.trim().to_owned();
    ensure!(
        token.len() < 16384
            && token.split('.').count() == 3
            && token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
        "invalid Access token"
    );
    Ok(token)
}
fn request(url: &str, token: &str, operation: Option<Value>) -> Result<Value> {
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(120))
        .build()?;
    let request = if let Some(operation) = operation {
        client
            .post(format!("{url}/api/operation"))
            .header("origin", url)
            .header("x-ow-request", "dashboard")
            .json(&operation)
    } else {
        client.get(format!("{url}/api/state"))
    };
    let mut response = request.header("cf-access-token", token).send()?;
    ensure!(
        response.status().is_success(),
        "remote request returned {}; run ow login {url} if your session expired",
        response.status()
    );
    let mut data = Vec::new();
    response
        .by_ref()
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut data)?;
    ensure!(data.len() <= 4 * 1024 * 1024, "remote response too large");
    let result: Value = serde_json::from_slice(&data)
        .context("expected workspace API response; check server and login")?;
    ensure!(
        result["ok"] == true,
        "remote operation failed: {}",
        result["error"].as_str().unwrap_or("unknown error")
    );
    Ok(result["result"].clone())
}
pub fn run(url: &str, action: Action) -> Result<i32> {
    let url = server(url)?;
    // Validate scope before asking for a credential.
    ensure!(
        matches!(
            action,
            Action::List
                | Action::Stats
                | Action::Status
                | Action::Snapshots
                | Action::Inspect { .. }
                | Action::Shell { .. }
                | Action::Exec { .. }
                | Action::Create { .. }
                | Action::Start { .. }
                | Action::Stop { .. }
                | Action::Hibernate { .. }
                | Action::Snapshot { .. }
                | Action::Restore { .. }
                | Action::Fork { .. }
        ),
        "this command is local-only; use --local for the local worker"
    );
    let token = token(&url)?;
    let operation = match action {
        Action::Shell { id } => return shell(&url, &token, &id),
        Action::List
        | Action::Stats
        | Action::Status
        | Action::Snapshots
        | Action::Inspect { .. } => {
            let state = request(&url, &token, None)?;
            let result = match action {
                Action::List => state["workspaces"].clone(),
                Action::Snapshots => state["snapshots"].clone(),
                Action::Stats => state["stats"].clone(),
                Action::Status => {
                    json!({"server":url,"connected":true,"machines":state["workspaces"].as_array().map(Vec::len)})
                }
                Action::Inspect { id } => state["workspaces"]
                    .as_array()
                    .context("missing machines")?
                    .iter()
                    .find(|w| w["id"] == id)
                    .context("workspace not found")?
                    .clone(),
                _ => unreachable!(),
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
            return Ok(0);
        }
        Action::Create { id, memory, image } => {
            json!({"op":"create","id":id,"memory_mib":memory,"image":image})
        }
        Action::Start { id } => json!({"op":"start","id":id}),
        Action::Stop { id } => json!({"op":"stop","id":id}),
        Action::Hibernate { id } => json!({"op":"hibernate","id":id}),
        Action::Snapshot { id, name } => json!({"op":"snapshot","id":id,"name":name}),
        Action::Restore { id, name } => json!({"op":"restore","id":id,"name":name}),
        Action::Fork {
            id,
            child,
            snapshot,
        } => {
            let mut v = json!({"op":"fork","id":id,"child":child});
            if let Some(snapshot) = snapshot {
                v["snapshot"] = json!(snapshot);
            }
            v
        }
        Action::Exec { id, command } => {
            let command = if command.len() == 1 {
                command[0].clone()
            } else {
                command
                    .iter()
                    .map(|a| common::shell_quote(a))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let result = request(
                &url,
                &token,
                Some(json!({"op":"exec","id":id,"command":command})),
            )?;
            print!("{}", result["output"].as_str().unwrap_or(""));
            return Ok(result["exit_code"].as_i64().unwrap_or(1) as i32);
        }
        _ => unreachable!(),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&request(&url, &token, Some(operation))?)?
    );
    Ok(0)
}

fn shell(url: &str, token: &str, id: &str) -> Result<i32> {
    common::identifier(id)?;
    ensure!(
        unsafe { libc::isatty(0) } == 1,
        "ow shell requires an interactive terminal; use ow exec for scripts"
    );
    let mut request = format!(
        "{}/api/terminal/{id}",
        url.replacen("https://", "wss://", 1)
    )
    .into_client_request()?;
    request.headers_mut().insert("origin", url.parse()?);
    request
        .headers_mut()
        .insert("cf-access-token", token.parse()?);
    let (mut socket, _) = tungstenite::connect(request).map_err(|_| {
        anyhow::anyhow!("secure terminal connection failed; check login and machine state")
    })?;
    let tcp = match socket.get_ref() {
        MaybeTlsStream::Plain(t) => t,
        MaybeTlsStream::Rustls(t) => &t.sock,
        _ => bail!("unsupported TLS transport"),
    };
    tcp.set_read_timeout(Some(Duration::from_millis(100)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(5)))?;
    struct Raw(libc::termios);
    impl Drop for Raw {
        fn drop(&mut self) {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &self.0);
            }
        }
    }
    let mut attrs = std::mem::MaybeUninit::uninit();
    ensure!(
        unsafe { libc::tcgetattr(0, attrs.as_mut_ptr()) } == 0,
        "terminal attributes unavailable"
    );
    let attrs = unsafe { attrs.assume_init() };
    let _raw = Raw(attrs);
    let mut raw = attrs;
    unsafe {
        libc::cfmakeraw(&mut raw);
    }
    ensure!(
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } == 0,
        "could not enter raw terminal mode"
    );
    let mut previous = (0, 0);
    loop {
        let mut size = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut size) } == 0 {
            let next = (size.ws_col.clamp(1, 500), size.ws_row.clamp(1, 300));
            if next != previous {
                socket.send(Message::text(
                    json!({"type":"resize","cols":next.0,"rows":next.1}).to_string(),
                ))?;
                previous = next;
            }
        }
        let mut fd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        ensure!(
            unsafe { libc::poll(&mut fd, 1, 0) } >= 0,
            "terminal input poll failed"
        );
        if fd.revents & libc::POLLIN != 0 {
            let mut bytes = [0; 4096];
            let n = std::io::stdin().read(&mut bytes)?;
            if n == 0 {
                let _ = socket.close(None);
                return Ok(0);
            }
            socket.send(Message::Binary(bytes[..n].to_vec().into()))?;
        }
        match socket.read() {
            Ok(Message::Binary(bytes)) => {
                ensure!(bytes.len() <= 4096, "invalid terminal output");
                std::io::stdout().write_all(&bytes)?;
                std::io::stdout().flush()?;
                socket.send(Message::text(
                    json!({"type":"ack","bytes":bytes.len()}).to_string(),
                ))?;
            }
            Ok(Message::Text(text)) => {
                let v: Value = serde_json::from_str(&text)?;
                if v["type"] == "exit" {
                    return Ok(v["code"].as_i64().context("invalid terminal exit")? as i32);
                }
            }
            Ok(Message::Ping(bytes)) => socket.send(Message::Pong(bytes))?,
            Ok(Message::Pong(_)) => {}
            Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => {
                bail!("terminal disconnected; reconnect with ow shell {id}")
            }
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => bail!("terminal transport failed; reconnect with ow shell {id}"),
            _ => {}
        }
    }
}
