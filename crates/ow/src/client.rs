//! Remote operator client: browser Access login, HTTPS API, and WSS native terminal.
use crate::{Action, common};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
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
pub(crate) fn token(url: &str) -> Result<String> {
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
pub(crate) fn request(url: &str, token: &str, operation: Option<Value>) -> Result<Value> {
    if let Some(operation) = &operation {
        let key = operation["operation_key"]
            .as_str()
            .context("operation key required")?;
        let dir = config()?.parent().unwrap().join("operations");
        fs::create_dir_all(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        let path = dir.join(format!("{key}.json"));
        if !path.exists() {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?;
            file.write_all(operation.to_string().as_bytes())?;
            file.sync_all()?;
        }
        eprintln!(
            "Operation retry key: {key} (reuse --operation-key {key} after an uncertain response)"
        );
    }
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
    let status = response.status();
    let mut data = Vec::new();
    response
        .by_ref()
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut data)?;
    ensure!(data.len() <= 4 * 1024 * 1024, "remote response too large");
    let result: Value = serde_json::from_slice(&data)
        .context("expected workspace API response; check server and login")?;
    ensure!(
        status.is_success() && result["ok"] == true,
        "remote operation failed: {} (operation {})",
        result["error"].as_str().unwrap_or("unknown error"),
        result["operation"]
    );
    Ok(result["result"].clone())
}
pub fn host_admin(url: &str, action: crate::HostAction) -> Result<i32> {
    let url = server(url)?;
    let (path, body, output) = match action {
        crate::HostAction::Invite {
            node,
            output,
            ttl,
            memory,
            slots,
            cpus,
        } => (
            "invite",
            json!({"node":node,"ttl":ttl,"memory":memory,"slots":slots,"cpus":cpus}),
            output,
        ),
        crate::HostAction::Budget {
            node,
            memory,
            slots,
            cpus,
        } => (
            "budget",
            json!({"node":node,"memory":memory,"slots":slots,"cpus":cpus}),
            None,
        ),
        crate::HostAction::Revoke { node } => ("revoke", json!({"node":node}), None),
        _ => bail!("invalid host administration command"),
    };
    struct OutputStage {
        path: PathBuf,
        file: fs::File,
    }
    impl Drop for OutputStage {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }
    let mut stage = if let Some(output) = &output {
        ensure!(
            fs::symlink_metadata(output).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "invitation output exists or cannot be inspected"
        );
        let parent = output
            .parent()
            .context("private output directory required")?;
        let m = fs::symlink_metadata(parent)?;
        ensure!(
            m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
            "invitation output directory must be owned/private (0700)"
        );
        let path = parent.join(format!("invite-{}.tmp", common::nonce()?));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        Some(OutputStage { path, file })
    } else {
        if path == "invite" {
            ensure!(
                unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1,
                "use --output PRIVATE_FILE when stdout is not a terminal"
            );
        }
        None
    };
    let token = token(&url)?;
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()?;
    let response = client
        .post(format!("{url}/api/hosts/{path}"))
        .header("cf-access-token", token)
        .header("origin", &url)
        .header("x-ow-request", "dashboard")
        .json(&body)
        .send()?;
    ensure!(
        response.status().is_success(),
        "owner host operation rejected or response uncertain; inspect owner pool state before retrying"
    );
    let mut bytes = Vec::new();
    response.take(4097).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4096, "host response too large");
    let value: Value = serde_json::from_slice(&bytes)?;
    ensure!(value["ok"] == true, "host operation rejected");
    if path == "invite" {
        if let Some(output) = output {
            let mut publish = || -> Result<()> {
                let stage = stage.as_mut().context("private output stage missing")?;
                stage
                    .file
                    .write_all(value["result"].to_string().as_bytes())?;
                stage.file.sync_all()?;
                fs::hard_link(&stage.path, &output)?;
                fs::File::open(output.parent().unwrap())?.sync_all()?;
                Ok(())
            };
            publish().context("server invitation was possibly replaced; local publication failed. Deliberately reissue this unused node invitation to invalidate it")?;
            println!(
                "Private invitation saved. Share it securely with the trusted host operator; it does not grant human login."
            );
        } else {
            println!("{}", value["result"]);
        }
    } else {
        println!("Node revoked; disconnected host effects already accepted cannot be undone.");
    }
    Ok(0)
}

pub fn run(url: &str, action: Action, retry: Option<String>) -> Result<i32> {
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
                | Action::Ssh { .. }
                | Action::SshProxy { .. }
                | Action::SshConfig { .. }
                | Action::SshAuthorize { .. }
                | Action::SshRevoke { .. }
                | Action::SshInfo { .. }
                | Action::Exec { .. }
                | Action::Put { .. }
                | Action::Get { .. }
                | Action::Create { .. }
                | Action::Resize { .. }
                | Action::Start { .. }
                | Action::Stop { .. }
                | Action::Hibernate { .. }
                | Action::Snapshot { .. }
                | Action::Restore { .. }
                | Action::Fork { .. }
        ),
        "this command is local-only; use --local for the local worker"
    );
    if let Action::SshProxy { id } = &action {
        return crate::ssh_client::remote_proxy(&url, id);
    }
    let token = token(&url)?;
    let key = retry.unwrap_or(common::nonce()?);
    common::identifier(&key)?;
    let mut operation = match action {
        Action::Shell { id } => return shell(&url, &token, &id),
        Action::Ssh { id, args } => {
            let info = request(
                &url,
                &token,
                Some(json!({"op":"ssh-info","id":id,"operation_key":key})),
            )?;
            return crate::ssh_client::launch(Some(&url), None, &id, &info, &args);
        }
        Action::SshConfig { id } => {
            let info = request(
                &url,
                &token,
                Some(json!({"op":"ssh-info","id":id,"operation_key":key})),
            )?;
            crate::ssh_client::config(Some(&url), None, &id, &info)?;
            return Ok(0);
        }
        Action::SshAuthorize { id, key, upgrade } => {
            json!({"op":"ssh-keys","id":id,"keys":crate::ssh_client::public_keys(&key)?,"upgrade":upgrade})
        }
        Action::SshRevoke { id } => json!({"op":"ssh-keys","id":id,"keys":[],"upgrade":false}),
        Action::SshInfo { id } => json!({"op":"ssh-info","id":id}),
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
        Action::Create {
            id,
            memory,
            image,
            cpus,
            node,
        } => {
            json!({"op":"create","id":id,"memory_mib":memory,"image":image,"vcpu_count":cpus,"node":node})
        }
        Action::Resize { id, memory, cpus } => {
            json!({"op":"resize","id":id,"memory_mib":memory,"vcpu_count":cpus})
        }
        Action::Put { id, local, guest } => {
            use base64::Engine;
            ensure!(
                fs::metadata(&local)?.len() <= 262144,
                "uploads limited to 256 KiB"
            );
            json!({"op":"put","id":id,"path":guest,"data":base64::engine::general_purpose::STANDARD.encode(fs::read(local)?)})
        }
        Action::Get { id, guest, local } => {
            use base64::Engine;
            let result = request(
                &url,
                &token,
                Some(json!({"op":"get","id":id,"path":guest,"operation_key":key})),
            )?;
            let data = base64::engine::general_purpose::STANDARD
                .decode(result["data"].as_str().context("missing file bytes")?)?;
            ensure!(data.len() <= 262144, "download too large");
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(local)?;
            file.write_all(&data)?;
            return Ok(0);
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
                Some(json!({"op":"exec","id":id,"command":command,"operation_key":key})),
            )?;
            print!("{}", result["output"].as_str().unwrap_or(""));
            return Ok(result["exit_code"].as_i64().unwrap_or(1) as i32);
        }
        _ => unreachable!(),
    };
    operation["operation_key"] = json!(key);
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
