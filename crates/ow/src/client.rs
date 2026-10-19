//! Remote operator client: browser Access login, HTTPS API, and WSS native terminal.
use crate::{Action, ImageAction, common};
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
/// With `tailnet`, the server's dashboard uses tailnet sign-in: `tailscale serve`
/// vouches for this device's tailnet user, so there is no token to fetch.
pub fn login(url: &str, tailnet: bool) -> Result<()> {
    let url = server(url)?;
    if tailnet {
        ensure!(
            reqwest::Url::parse(&url)?
                .host_str()
                .is_some_and(|h| h.ends_with(".ts.net")),
            "--tailnet takes the host's MagicDNS name, e.g. https://NAME.TAILNET.ts.net"
        );
        request(&url, "", None).context(
            "the dashboard refused this device: sign in to the tailnet as an allowed login",
        )?;
    } else {
        let status = Command::new(cloudflared())
            .args(["access", "login", &url])
            .stdout(Stdio::null())
            .status()
            .context("install cloudflared on this client for browser login")?;
        ensure!(status.success(), "Cloudflare login failed");
        // Verify the token is cached; never print it or put it in command arguments.
        let _ = token(&url)?;
    }
    let path = config()?;
    fs::create_dir_all(path.parent().unwrap())?;
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700))?;
    let temporary = path.with_extension(format!("{}.tmp", common::nonce()?));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let saved = if tailnet {
        json!({"server":url,"auth":"tailscale"})
    } else {
        json!({"server":url})
    };
    file.write_all(serde_json::to_string_pretty(&saved)?.as_bytes())?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    println!("Signed in to {url}. Use ow list or ow shell <machine>.");
    Ok(())
}
/// A server saved by `ow login --tailnet` takes no token: the gateway ignores
/// `cf-access-token` and trusts only what `tailscale serve` adds.
fn tailnet_server(url: &str) -> Result<bool> {
    let path = config()?;
    Ok(path.exists() && saved_tailnet(&serde_json::from_slice(&fs::read(path)?)?, url))
}
fn saved_tailnet(saved: &Value, url: &str) -> bool {
    saved["auth"] == "tailscale"
        && saved["server"]
            .as_str()
            .and_then(|s| server(s).ok())
            .as_deref()
            == Some(url)
}
pub(crate) fn token(url: &str) -> Result<String> {
    if tailnet_server(url)? {
        return Ok(String::new());
    }
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
        // A capture can take minutes (wire::reply_wait); the gateway waits up to 1260 s.
        .timeout(Duration::from_secs(if operation.is_some() {
            1260
        } else {
            120
        }))
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
    let mut response = request.header("cf-access-token", token).send()
        .context("server_unreachable: check the selected HTTPS server, network/tailnet connection and TLS trust; retry mutations only with their original operation key")?;
    let status = response.status();
    ensure!(
        !matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308 | 401 | 403),
        "authentication_or_admission: run ow login {url} (use --tailnet for a tailnet server); ask the administrator to admit your own identity if access is denied"
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
        status.is_success() && result["ok"] == true,
        "remote operation failed: {} {} (operation {})",
        result["error_code"]
            .as_str()
            .or_else(|| result["operation"]["error_code"].as_str())
            .unwrap_or(""),
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
                | Action::Nodes
                | Action::Image {
                    command: ImageAction::List {
                        runner: None,
                        catalog: false
                    }
                }
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
                | Action::Share { .. }
                | Action::Unshare { .. }
                | Action::Publish { .. }
                | Action::Unpublish { .. }
                | Action::Delete { .. }
        ),
        "unsupported_capability: this command is local-only; use --local for the local worker (remote image list does not take --runner)"
    );
    if let Action::SshProxy { id } = &action {
        return crate::ssh_client::remote_proxy(&url, id);
    }
    let token = token(&url).context("authentication_or_admission: sign in to the selected server as yourself before running remote commands")?;
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
        | Action::Nodes
        | Action::Image {
            command:
                ImageAction::List {
                    runner: None,
                    catalog: false,
                },
        }
        | Action::Snapshots
        | Action::Inspect { .. } => {
            let state = request(&url, &token, None)?;
            let mut machines = state["workspaces"].as_array().cloned().unwrap_or_default();
            // Shared with me: addressed as owner@example.com/name, with owner and role.
            machines.extend(state["shared"].as_array().cloned().unwrap_or_default());
            let result = match action {
                Action::List => json!(machines),
                Action::Snapshots => state["snapshots"].clone(),
                Action::Stats => state["stats"].clone(),
                Action::Status | Action::Nodes | Action::Image { .. } => {
                    discovery_output(&url, &state, &action)?
                }
                Action::Inspect { id } => machines
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
            disk,
            node,
        } => {
            let mut request = json!({"op":"create","id":id,"memory_mib":memory,"image":image,"vcpu_count":cpus,"disk_gib":disk.or((image != "ubuntu-arm64").then_some(crate::common::DEFAULT_DISK_GIB)),"node":node});
            if request["disk_gib"].is_null() {
                request.as_object_mut().unwrap().remove("disk_gib");
            }
            request
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
        Action::Publish { id, port } => json!({"op":"publish","id":id,"port":port}),
        Action::Unpublish { id, port } => {
            let mut v = json!({"op":"unpublish","id":id});
            if let Some(port) = port {
                v["port"] = json!(port);
            }
            v
        }
        Action::Share { id, with, role } => json!({"op":"share","id":id,"with":with,"role":role}),
        Action::Unshare { id, with } => json!({"op":"unshare","id":id,"with":with}),
        Action::Start { id } => json!({"op":"start","id":id}),
        Action::Stop { id } => json!({"op":"stop","id":id}),
        Action::Delete { id } => json!({"op":"delete","id":id}),
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
    let published = operation["op"] == "publish";
    let result = request(&url, &token, Some(operation))?;
    if published {
        println!("{}", result["url"].as_str().context("missing preview URL")?);
    } else {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    Ok(0)
}

fn discovery_output(url: &str, state: &Value, action: &Action) -> Result<Value> {
    let mut discovery = state["discovery"].clone();
    // Newer controllers report provisioned owner quotas separately from runner headroom.
    if let Some(images) = discovery.get_mut("images").and_then(Value::as_array_mut) {
        for image in images {
            let mac = image["name"] == "ubuntu-arm64";
            // Admission charges the maximum when a native clone's disk size is unknown.
            let disk = u64::from(if mac {
                common::MAX_DISK_GIB
            } else {
                common::DEFAULT_DISK_GIB
            });
            if let Some(placements) = image.get_mut("nodes").and_then(Value::as_array_mut) {
                for placement in placements.iter_mut() {
                    if placement["availability"] == "available"
                        && !owner_fits(&state["owner_limits"], if mac { 512 } else { 256 }, 1, disk)
                    {
                        placement["availability"] = json!("insufficient_capacity");
                        placement["development_choice"] = json!(null);
                    }
                    let choice = &placement["development_choice"];
                    if choice.is_object()
                        && !owner_fits(
                            &state["owner_limits"],
                            choice["memory_mib"].as_u64().unwrap_or(0),
                            choice["vcpu_count"].as_u64().unwrap_or(0),
                            if mac {
                                disk
                            } else {
                                choice["disk_gib"].as_u64().unwrap_or(disk)
                            },
                        )
                    {
                        placement["development_choice"] = json!(null);
                    }
                }
                image["available"] =
                    json!(placements.iter().any(|p| p["availability"] == "available"));
            }
        }
    }
    let supported = discovery["schema"] == 1;
    match action {
        Action::Status => Ok(json!({"server":url,"connected":true,
            "machines":state["workspaces"].as_array().map(Vec::len),
            "client_version":env!("CARGO_PKG_VERSION"),"server_version":discovery["server_version"],
            "compatibility":{"discovery_supported":supported,"schema":discovery["schema"],
                "next_action":if supported {json!(null)} else {json!("unsupported_capability: upgrade the server/client together before using remote discovery")}},
            "identity":discovery["identity"],"owner_limits":state["owner_limits"],"operations":state["operations"],"discovery":discovery})),
        Action::Nodes | Action::Image { .. } => {
            ensure!(
                supported,
                "unsupported_capability: selected server does not support discovery schema 1; upgrade the server/client together, then retry status"
            );
            let field = if matches!(action, Action::Nodes) {
                "nodes"
            } else {
                "images"
            };
            ensure!(
                discovery[field].is_array(),
                "unsupported_capability: malformed discovery response; check server/client versions"
            );
            Ok(discovery[field].clone())
        }
        _ => unreachable!(),
    }
}

fn owner_fits(limits: &Value, memory: u64, cpus: u64, disk: u64) -> bool {
    [
        ("memory_mib", memory),
        ("cpus", cpus),
        ("disk_gib", disk),
        ("boxes", 1),
    ]
    .iter()
    .all(|(key, requested)| {
        limits["caps"][key]
            .as_u64()
            .zip(limits["usage"][key].as_u64())
            .is_none_or(|(cap, used)| cap.saturating_sub(used) >= *requested)
    })
}

fn shell(url: &str, token: &str, id: &str) -> Result<i32> {
    common::address(id)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_owner_quotas_bound_choices_without_defaulting_unknown_limits() {
        let mut state = json!({"discovery":{"schema":1,"images":[{"name":"ubuntu","available":true,"nodes":[{"availability":"available","development_choice":{"memory_mib":2048,"vcpu_count":2,"disk_gib":32}}]}]}});
        let action = Action::Image {
            command: ImageAction::List {
                runner: None,
                catalog: false,
            },
        };
        assert!(discovery_output("https://example.test",&state,&action).unwrap()[0]["nodes"][0]["development_choice"].is_object());
        state["owner_limits"] = json!({"caps":{"memory_mib":2048,"cpus":2,"disk_gib":64,"boxes":2},"usage":{"memory_mib":1024,"cpus":0,"disk_gib":0,"boxes":0}});
        let out = discovery_output("https://example.test", &state, &action).unwrap();
        assert_eq!(out[0]["available"], true);
        assert!(out[0]["nodes"][0]["development_choice"].is_null());
        state["owner_limits"]["usage"]["boxes"] = json!(2);
        let out = discovery_output("https://example.test", &state, &action).unwrap();
        assert_eq!(out[0]["available"], false);
        assert_eq!(out[0]["nodes"][0]["availability"], "insufficient_capacity");
        state["discovery"] = json!([]);
        assert!(discovery_output("https://example.test", &state, &action).is_err());
        assert_eq!(
            discovery_output("https://example.test", &state, &Action::Status).unwrap()["compatibility"]
                ["discovery_supported"],
            false
        );
    }

    #[test]
    fn mac_discovery_choices_match_owner_disk_admission() {
        use crate::limits;
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("ow-discovery-quota-{}", common::nonce().unwrap()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE resources(kind TEXT,physical TEXT,owner INTEGER,metadata TEXT);",
        )
        .unwrap();
        limits::initialize(&db).unwrap();
        db.execute_batch("INSERT INTO resources VALUES('machine','native',1,'{}'),('machine','existing',1,'{}');").unwrap();
        let cap = json!({"type":"heartbeat","protocol":common::PROTOCOL,"backend":"apple-virtualization","arch":"aarch64","runtime":"apple-vz-v1","guest_ssh_v1":false,"operations":common::MAC_OPERATIONS,"memory_mib":2048,"slots":1,"vcpus":2,"images":["ubuntu-arm64"],"max_guest_memory_mib":2048,"max_guest_vcpus":2});
        common::node_capabilities(&cap).unwrap();
        let node = crate::discovery::node(
            &json!({"id":"mac","online":true,"memory_mib":2048,"slots":1,"capabilities":cap}),
            json!({"memory_mib":0,"vcpus":0,"slots":0}),
            100,
        );
        let images = crate::discovery::images(&[node]);
        let request = images
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == "ubuntu-arm64")
            .unwrap()["nodes"][0]["development_choice"]
            .clone();
        assert!(request.is_object());
        assert!(request.get("disk_gib").is_none());
        let max = u64::from(common::MAX_DISK_GIB);
        for (cap, used, expected) in [
            (None, 0, false),
            (Some(max - 1), 0, false),
            (Some(max), 0, true),
            (Some(max + 31), 32, false),
            (Some(max + 32), 32, true),
        ] {
            db.execute("DELETE FROM owner_allocations", []).unwrap();
            if used > 0 {
                db.execute(
                    "INSERT INTO owner_allocations VALUES('existing',1,512,?1)",
                    [used],
                )
                .unwrap();
            }
            if let Some(cap) = cap {
                std::fs::write(root.join("limits.json"), json!({"owner_defaults":{"cpus":16,"memory_mib":32768,"disk_gib":cap,"boxes":8}}).to_string()).unwrap();
                std::fs::set_permissions(
                    root.join("limits.json"),
                    std::fs::Permissions::from_mode(0o600),
                )
                .unwrap();
            }
            let owner_limits = limits::state(&root, &db, 1).unwrap();
            let admitted =
                limits::admit(&root, &db, "native", "create", &request, &json!({})).is_ok();
            assert_eq!(admitted, expected, "cap={cap:?}, usage={used}");
            let state =
                json!({"discovery":{"schema":1,"images":images},"owner_limits":owner_limits});
            let status = discovery_output("https://example.test", &state, &Action::Status).unwrap();
            let action = Action::Image {
                command: ImageAction::List {
                    runner: None,
                    catalog: false,
                },
            };
            let output = discovery_output("https://example.test", &state, &action).unwrap();
            assert_eq!(output, status["discovery"]["images"]);
            let image = output
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["name"] == "ubuntu-arm64")
                .unwrap();
            assert_eq!(image["available"], admitted, "cap={cap:?}, usage={used}");
            assert_eq!(
                image["nodes"][0]["development_choice"].is_object(),
                admitted
            );
            assert_eq!(
                image["nodes"][0]["availability"],
                if admitted {
                    "available"
                } else {
                    "insufficient_capacity"
                }
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_a_saved_tailnet_server_skips_the_token() {
        let url = "https://host.example-net.ts.net";
        assert!(saved_tailnet(
            &json!({"server":url,"auth":"tailscale"}),
            url
        ));
        assert!(saved_tailnet(
            &json!({"server":"https://host.example-net.ts.net/","auth":"tailscale"}),
            url
        ));
        assert!(!saved_tailnet(&json!({"server":url}), url), "Access login");
        assert!(!saved_tailnet(
            &json!({"server":"https://other.example-net.ts.net","auth":"tailscale"}),
            url
        ));
        assert!(!saved_tailnet(
            &json!({"server":"http://host.example-net.ts.net","auth":"tailscale"}),
            url
        ));
        assert!(!saved_tailnet(&json!({"auth":"tailscale"}), url));
    }
}
