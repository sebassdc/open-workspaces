//! The admin commands as a thin wrapper over the admin API (docs/ADMIN_API.md): each command
//! builds one op's request, sends it over HTTPS or the controller's admin.sock, and prints
//! the result. Nothing here reads or writes the controller database.
use crate::{
    Action, ActionsAction, AppKeyAction, ConfigAction, HostAction, LimitsAction, PlacementAction,
    TokenAction, UpgradeAction, admin,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

/// Exit status for a usable answer that reports an API error.
const API_ERROR: i32 = 1;
/// Exit status for a usage error found before anything is sent.
const USAGE: i32 = 2;
/// Exit status when the server cannot be reached or refuses the sign-in.
const UNREACHABLE: i32 = 3;

/// The admin request an admin command sends, or None for any other command. Absent
/// optional flags are left out; `key` is the retry key for ops that take one.
pub(crate) fn request(action: &Action, key: &str) -> Option<Value> {
    let mut request = match action {
        Action::Config { action } => match action {
            ConfigAction::List => json!({"op":"config.list"}),
            ConfigAction::Get { key } => json!({"op":"config.get","key":key}),
            ConfigAction::Set {
                key,
                value,
                expect_version,
            } => {
                json!({"op":"config.set","key":key,"value":value,"expected_version":expect_version})
            }
            ConfigAction::History { key, limit } => {
                json!({"op":"config.history","key":key,"limit":limit})
            }
        },
        Action::Health => json!({"op":"health.get"}),
        Action::Upgrade {
            command: Some(command),
            ..
        } => match command {
            UpgradeAction::Set {
                version,
                runner,
                client,
            } => {
                json!({"op":"upgrade.set","version":version,"runner":runner,"client":client.then_some(true)})
            }
            UpgradeAction::Status => json!({"op":"upgrade.status"}),
        },
        Action::Placement {
            command: PlacementAction::Status { pool },
        } => json!({"op":"placement.status","pool":pool}),
        Action::Actions {
            command: ActionsAction::Status { pool },
        } => json!({"op":"actions.status","pool":pool}),
        Action::Actions {
            command:
                ActionsAction::AppKey {
                    action: AppKeyAction::Set { private_key },
                },
        } => json!({"op":"actions.app_key.set","private_key":private_key}),
        Action::NodePool { node, pool } => json!({"op":"node.pool.set","node":node,"pool":pool}),
        Action::Token { action } => match action {
            TokenAction::Issue { name, owner, pool } => {
                json!({"op":"token.issue","name":name,"owner":owner,"pool":pool,"operation_key":key})
            }
            TokenAction::Revoke { name } => json!({"op":"token.revoke","name":name}),
            TokenAction::List => json!({"op":"token.list"}),
        },
        Action::Limits { action } => match action {
            LimitsAction::Get { owner } => json!({"op":"limits.owner.get","owner":owner}),
            LimitsAction::Set {
                owner,
                cpus,
                memory_mib,
                disk_gib,
                boxes,
            } => {
                json!({"op":"limits.owner.set","owner":owner,"cpus":cpus,"memory_mib":memory_mib,"disk_gib":disk_gib,"boxes":boxes})
            }
        },
        Action::Host {
            command:
                HostAction::Invite {
                    node,
                    ttl,
                    memory,
                    slots,
                    cpus,
                    ..
                },
        }
        | Action::NodeJoin {
            node,
            ttl,
            memory,
            slots,
            cpus,
            ..
        } => {
            json!({"op":"host.invite","node":node,"ttl":ttl,"memory":memory,"slots":slots,"cpus":cpus,"operation_key":key})
        }
        Action::Host {
            command:
                HostAction::Budget {
                    node,
                    memory,
                    slots,
                    cpus,
                },
        } => json!({"op":"host.budget","node":node,"memory":memory,"slots":slots,"cpus":cpus}),
        Action::Host {
            command: HostAction::Revoke { node },
        }
        | Action::NodeRevoke { node } => json!({"op":"host.revoke","node":node}),
        _ => return None,
    };
    request.as_object_mut().unwrap().retain(|_, v| !v.is_null());
    Some(request)
}

/// Where the request goes.
enum Transport {
    Socket(PathBuf),
    Https(String),
}

/// The controller data directory `--local` uses: `--data-dir`, `OW_DATA_DIR`, else the one
/// `ow host up` uses.
fn local_root(data_dir: Option<PathBuf>) -> Option<PathBuf> {
    data_dir
        .or_else(|| std::env::var_os("OW_DATA_DIR").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".ow/controller")))
}

/// `--local` means the socket; then `--server`; then an explicit `--data-dir` (a local
/// controller, as host scripts name it); then `OW_SERVER` or the saved login; otherwise the
/// default socket when it exists.
fn transport(
    data_dir: Option<PathBuf>,
    server: Option<String>,
    local: bool,
) -> Result<Option<Transport>> {
    let explicit_dir = data_dir.is_some();
    let socket = local_root(data_dir).map(|r| r.join(admin::SOCKET));
    if local || (server.is_none() && explicit_dir) {
        return Ok(socket.map(Transport::Socket));
    }
    let server = match server.or_else(|| std::env::var("OW_SERVER").ok()) {
        Some(server) => Some(server),
        None => crate::client::saved_server()?,
    };
    if let Some(server) = server {
        return Ok(Some(Transport::Https(crate::client::origin(&server)?)));
    }
    Ok(socket.filter(|s| s.exists()).map(Transport::Socket))
}

/// No usable sign-in for the server (no cloudflared, or an expired session).
#[derive(Debug)]
struct SignIn(String);
impl std::fmt::Display for SignIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SignIn {}

/// An answer: HTTP status and body bytes.
type Reply = (u16, Vec<u8>);

pub(crate) fn socket_call(
    path: &Path,
    method: &str,
    uri: &str,
    body: Option<&Value>,
) -> Result<Reply> {
    use axum::body::Body;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let stream = tokio::net::UnixStream::connect(path).await?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await?;
        tokio::spawn(connection);
        let mut request = hyper::Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header("x-ow-request", "dashboard");
        if let Some(user) = std::env::var("SUDO_USER")
            .ok()
            .filter(|u| crate::common::identifier(u).is_ok())
        {
            request = request.header("x-ow-claimed-user", user);
        }
        let request = request.body(match body {
            Some(body) => Body::from(body.to_string()),
            None => Body::empty(),
        })?;
        let response = tokio::time::timeout(Duration::from_secs(120), sender.send_request(request))
            .await
            .context("controller did not answer within 120 s")??;
        let status = response.status().as_u16();
        let bytes = axum::body::to_bytes(Body::new(response.into_body()), 4 << 20).await?;
        Ok((status, bytes.to_vec()))
    })
}

fn https_call(url: &str, method: &str, path: &str, body: Option<&Value>) -> Result<Reply> {
    let token = crate::client::token(url).map_err(|e| anyhow::Error::new(SignIn(e.to_string())))?;
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(120))
        .build()?;
    let mut request = if method == "POST" {
        client
            .post(format!("{url}{path}"))
            .header("origin", url)
            .header("x-ow-request", "dashboard")
            .header("content-type", "application/json")
            .body(body.map(Value::to_string).unwrap_or_default())
    } else {
        client.get(format!("{url}{path}"))
    };
    request = request.header("cf-access-token", token);
    let mut response = request.send()?;
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take((4 << 20) + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4 << 20, "admin response too large");
    Ok((status, bytes))
}

fn call(transport: &Transport, method: &str, path: &str, body: Option<&Value>) -> Result<Reply> {
    match transport {
        Transport::Socket(socket) => socket_call(socket, method, path, body),
        Transport::Https(url) => https_call(url, method, path, body),
    }
}

/// The controller these options reach is an HTTPS server, not this host's socket.
pub(crate) fn https(
    data_dir: Option<PathBuf>,
    server: Option<String>,
    local: bool,
) -> Result<bool> {
    Ok(matches!(
        transport(data_dir, server, local)?,
        Some(Transport::Https(_))
    ))
}

/// The release `ow upgrade` installs from the configured controller's `GET /api/version`:
/// `client`, the release its operator chose for clients (true), else `server`, the one it runs.
pub(crate) fn server_version(
    data_dir: Option<PathBuf>,
    server: Option<String>,
    local: bool,
) -> Result<(String, bool)> {
    let transport = transport(data_dir, server, local)?
        .context("no controller configured: run ow login <server>, or pass --version")?;
    let (status, body) = call(&transport, "GET", "/api/version", None)?;
    serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| {
            let result = &v["result"];
            match result["client"].as_str() {
                Some(client) => Some((client.to_owned(), true)),
                None => result["server"].as_str().map(|s| (s.to_owned(), false)),
            }
        })
        .with_context(|| {
            format!("the controller's /api/version has no server version (HTTP {status})")
        })
}

/// Saves the request of an op that takes a retry key, as the operation API does.
fn remember(request: &Value) -> Result<()> {
    let Some(key) = request["operation_key"].as_str() else {
        return Ok(());
    };
    crate::client::remember_operation(key, request)
}

/// An invitation's private output file: staged before the request so a refusal leaves
/// nothing, published by hard link only after the answer.
struct OutputStage {
    path: PathBuf,
    file: fs::File,
}
impl Drop for OutputStage {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
fn stage(output: &Path) -> Result<OutputStage> {
    ensure!(
        fs::symlink_metadata(output).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "invitation output exists or cannot be inspected"
    );
    let parent = match output.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let m = fs::symlink_metadata(&parent)?;
    ensure!(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "invitation output directory must be owned/private (0700)"
    );
    let path = parent.join(format!("invite-{}.tmp", crate::common::nonce()?));
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    Ok(OutputStage { path, file })
}

/// Runs one admin command; its exit status.
pub(crate) fn run(
    data_dir: Option<PathBuf>,
    server: Option<String>,
    local: bool,
    operation_key: Option<String>,
    action: Action,
) -> Result<i32> {
    let key = match operation_key {
        Some(key) => key,
        None => crate::common::nonce()?,
    };
    if let Err(error) = crate::common::identifier(&key) {
        eprintln!("usage: --operation-key: {error}");
        return Ok(USAGE);
    }
    let request = request(&action, &key).context("not an admin command")?;
    let op = request["op"].as_str().unwrap_or_default().to_owned();
    let spec = admin::spec(&op).context("unregistered admin op")?;
    let output = match &action {
        Action::Host {
            command: HostAction::Invite { output, .. },
        } => output.clone(),
        Action::NodeJoin { output, .. } => Some(output.clone()),
        _ => None,
    };
    let mut staged = match output.as_deref().map(stage).transpose() {
        Ok(staged) => staged,
        Err(error) => {
            eprintln!("usage: --output: {error:#}");
            return Ok(USAGE);
        }
    };
    if op == "host.invite" && staged.is_none() && unsafe { libc::isatty(libc::STDOUT_FILENO) } != 1
    {
        eprintln!("usage: use --output PRIVATE_FILE when stdout is not a terminal");
        return Ok(USAGE);
    }
    let Some(transport) = transport(data_dir, server, local)? else {
        eprintln!("no_server: run ow login <server>, or use --local on the controller host");
        return Ok(UNREACHABLE);
    };
    if let Transport::Socket(path) = &transport
        && !path.exists()
    {
        eprintln!(
            "controller_unreachable: {} not found; start the controller (ow host up)",
            path.display()
        );
        return Ok(UNREACHABLE);
    }
    if spec.operation_key {
        remember(&request)?;
    }
    let (status, bytes) = match call(&transport, "POST", "/api/admin", Some(&request)) {
        Ok(reply) => reply,
        Err(error) => {
            match &transport {
                Transport::Socket(path) => eprintln!(
                    "controller_unreachable: {}: {error:#}; start the controller (ow host up)",
                    path.display()
                ),
                Transport::Https(url) if error.is::<SignIn>() => eprintln!(
                    "authentication_or_admission: run ow login {url} (use --tailnet for a tailnet server): {error}"
                ),
                Transport::Https(_) => eprintln!(
                    "server_unreachable: check the selected HTTPS server, network/tailnet connection and TLS trust ({error:#})"
                ),
            }
            if spec.operation_key {
                eprintln!(
                    "Operation retry key: {key} (reuse --operation-key {key} after an uncertain response)"
                );
            }
            return Ok(UNREACHABLE);
        }
    };
    let answer = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .filter(|v| v["ok"].is_boolean());
    let Some(answer) = answer else {
        if let Transport::Https(url) = &transport {
            if matches!(status, 301 | 302 | 303 | 307 | 308 | 401 | 403) {
                eprintln!(
                    "authentication_or_admission: run ow login {url} (use --tailnet for a tailnet server)"
                );
                return Ok(UNREACHABLE);
            }
            let supported = call(&transport, "GET", "/api/version", None)
                .ok()
                .and_then(|(_, b)| serde_json::from_slice::<Value>(&b).ok())
                .is_some_and(|v| {
                    v["result"]["capabilities"]
                        .as_array()
                        .is_some_and(|c| c.iter().any(|c| c == admin::CAPABILITY))
                });
            if !supported {
                eprintln!(
                    "unsupported_server: upgrade the controller to use admin commands remotely"
                );
                return Ok(UNREACHABLE);
            }
        }
        bail!("unexpected admin response (HTTP {status})");
    };
    if answer["ok"] != true {
        eprintln!(
            "{}: {}",
            answer["error_code"].as_str().unwrap_or("error"),
            answer["error"].as_str().unwrap_or("request refused")
        );
        return Ok(API_ERROR);
    }
    let result = &answer["result"];
    for warning in result["warnings"].as_array().into_iter().flatten() {
        eprintln!("warning: {}", warning.as_str().unwrap_or_default());
    }
    if let (Some(output), Some(stage)) = (output, staged.as_mut()) {
        let mut publish = || -> Result<()> {
            stage.file.write_all(result.to_string().as_bytes())?;
            stage.file.sync_all()?;
            fs::hard_link(&stage.path, &output)?;
            if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::File::open(parent)?.sync_all()?;
            }
            Ok(())
        };
        publish().context("server invitation was possibly replaced; local publication failed. Deliberately reissue this unused node invitation to invalidate it")?;
        eprintln!(
            "Private invitation saved to {}. Share it securely with the trusted host operator; it does not grant human login.",
            output.display()
        );
        return Ok(0);
    }
    println!("{}", serde_json::to_string_pretty(result)?);
    if spec.exit_on_alerts && result["alerts"].as_array().is_some_and(|a| !a.is_empty()) {
        return Ok(1);
    }
    Ok(0)
}
