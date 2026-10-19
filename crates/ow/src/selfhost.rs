//! Single-host mode (`ow host up`) and runner onboarding over SSH, AWS SSM or
//! Tailscale (`ow node add`).
//! Both compose the existing controller, worker, single-use join file and node agent.
use crate::{NodeAction, common, host, nodes, onboarding};
use anyhow::{Context, Result, anyhow, bail, ensure};
use ow_core::connect::{self, Session};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const PORT: u16 = 8790;

pub(crate) fn base(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let path = match explicit {
        Some(path) => path,
        None => PathBuf::from(std::env::var_os("HOME").context("HOME required")?).join(".ow"),
    };
    if !path.exists() {
        fs::create_dir_all(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    }
    let path = fs::canonicalize(path)?;
    let m = fs::metadata(&path)?;
    ensure!(
        m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "{} must be a directory you own with mode 0700",
        path.display()
    );
    // Nested VM sockets under the runner root must fit Linux's Unix socket limit.
    ensure!(
        path.as_os_str().len() < 52,
        "choose a --data-dir shorter than 52 bytes"
    );
    Ok(path)
}

fn tailscale() -> Option<(IpAddr, Option<String>)> {
    let output = Command::new("tailscale")
        .args(["status", "--json"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let v: Value = serde_json::from_slice(&output.stdout).ok()?;
    if v["BackendState"] != "Running" {
        return None;
    }
    let ip = v["Self"]["TailscaleIPs"]
        .as_array()?
        .iter()
        .filter_map(|ip| ip.as_str()?.parse::<IpAddr>().ok())
        .find(IpAddr::is_ipv4)?;
    let dns = v["Self"]["DNSName"]
        .as_str()
        .map(|name| name.trim_end_matches('.').to_owned())
        .filter(|name| !name.is_empty());
    Some((ip, dns))
}

/// Private CA kept across restarts (runners trust it); the server cert is reissued each start.
fn tls(base: &Path, names: &[String]) -> Result<()> {
    let san = names
        .iter()
        .map(|n| match n.parse::<IpAddr>() {
            Ok(_) => format!("IP:{n}"),
            Err(_) => format!("DNS:{n}"),
        })
        .collect::<Vec<_>>()
        .join(",");
    let script = r#"set -eu
umask 077
mkdir -p "$1"
cd "$1"
if [ ! -s ca.pem ]; then
  openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out ca.key
  openssl req -x509 -new -key ca.key -sha256 -days 3650 -subj "/CN=open-workspaces controller CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" -out ca.pem
fi
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out server.key.new
openssl req -new -key server.key.new -subj "/CN=open-workspaces controller" -out server.csr
printf 'subjectAltName=%s\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n' "$2" > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -set_serial "0x$3" -days 397 -sha256 \
  -extfile server.ext -out server.pem.new 2>/dev/null
mv server.key.new server.key
mv server.pem.new server.pem
rm -f server.csr server.ext
"#;
    let status = Command::new("sh")
        .args(["-c", script, "sh"])
        .arg(base.join("tls"))
        .arg(&san)
        .arg(common::nonce()?)
        .stdout(Stdio::null())
        .status()?;
    ensure!(
        status.success(),
        "creating the controller's private CA failed; install openssl"
    );
    Ok(())
}

/// Online with a heartbeat at or after `since` (Unix seconds), so a stale one cannot pass.
fn wait_online(controller: &Path, node: &str, since: i64, limit: Duration) -> bool {
    let started = Instant::now();
    let fresh = || -> Result<bool> {
        nodes::online(controller, node)?;
        let heartbeat: i64 = nodes::db(controller)?.query_row(
            "SELECT heartbeat FROM nodes WHERE id=?1",
            [node],
            |r| r.get(0),
        )?;
        Ok(heartbeat >= since)
    };
    while !fresh().unwrap_or(false) {
        if started.elapsed() > limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    true
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Waits up to `limit` for a worker someone else starts (a systemd unit) to answer
/// with its Internet uplink up.
fn wait_worker(runner: &Path, limit: Duration) -> Result<()> {
    let started = Instant::now();
    loop {
        if crate::wire::request(runner, json!({"op":"status"}))
            .is_ok_and(|status| status["internet"] == true)
        {
            return Ok(());
        }
        ensure!(
            started.elapsed() < limit,
            "no worker answers on {}; start it first (open-workspaces-worker.service)",
            runner.join("control.sock").display()
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// SIGTERM or SIGINT: stop taking jobs and let the ones in flight finish.
pub(crate) fn stop_on_signal() -> Result<()> {
    extern "C" fn stopping(_: libc::c_int) {
        nodes::STOPPING.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let mut handler: libc::sigaction = unsafe { std::mem::zeroed() };
        handler.sa_sigaction = stopping as *const () as usize;
        ensure!(
            unsafe { libc::sigaction(signal, &handler, std::ptr::null_mut()) } == 0,
            "cannot install the stop handler"
        );
    }
    Ok(())
}

/// After a stop signal: the controller already refuses new jobs; wait for the agent's
/// accepted non-stream jobs (their replies are journaled) up to `limit`.
pub(crate) fn drain(limit: Duration) {
    use std::sync::atomic::Ordering;
    let started = Instant::now();
    // A job ID sent just before the signal may still be in flight to the agent.
    std::thread::sleep(Duration::from_secs(1));
    while nodes::BUSY.load(Ordering::SeqCst) > 0 && started.elapsed() < limit {
        std::thread::sleep(Duration::from_millis(100));
    }
    let left = nodes::BUSY.load(Ordering::SeqCst);
    if left > 0 {
        eprintln!("stopping with {left} job(s) still running after {limit:?}");
    } else {
        println!("Stopped: no jobs in flight.");
    }
}

#[allow(clippy::too_many_arguments)]
pub fn up(
    base: Option<PathBuf>,
    listen: Option<SocketAddr>,
    node: Option<String>,
    memory: u32,
    slots: u32,
    cpus: u32,
    idle_minutes: u64,
    external_worker: bool,
    controller_only: bool,
) -> Result<i32> {
    if !controller_only {
        common::host_limits(memory, slots, cpus)?;
    }
    let base = self::base(base)?;
    let controller = host::data_root(Some(base.join("controller")))?;
    ensure!(
        !onboarding::held_lock(&controller.join("node-controller.lock"))?,
        "a controller is already running for {}",
        base.display()
    );
    let runner = base.join("runner");
    if controller_only {
        ensure!(
            !runner.exists() && !controller.join("control.sock").exists(),
            "controller-only requires a base without local runner data; use the controller cutover procedure"
        );
    } else {
        let runner = host::data_root(Some(runner.clone()))?;
        let report = onboarding::doctor(&runner)?;
        ensure!(
            report["supported"] == true,
            "this machine cannot run workspaces: {}",
            report["diagnostics"]
        );
    }
    let hostname = fs::read_to_string("/proc/sys/kernel/hostname")?
        .trim()
        .to_owned();
    let node = node.unwrap_or_else(|| connect::node_name(&hostname));
    if !controller_only {
        common::identifier(&node)?;
    }
    let tailnet = tailscale();
    // Never a public listener by default: the tailnet address, else loopback only.
    let listen = listen.unwrap_or_else(|| {
        SocketAddr::new(
            tailnet
                .as_ref()
                .map_or(Ipv4Addr::LOCALHOST.into(), |(ip, _)| *ip),
            PORT,
        )
    });
    let local_ip = if listen.ip().is_unspecified() {
        Ipv4Addr::LOCALHOST.into()
    } else {
        listen.ip()
    };
    let local = format!("https://{}", SocketAddr::new(local_ip, listen.port()));
    let advertised = match &tailnet {
        Some((ip, Some(dns))) if *ip == listen.ip() => format!("https://{dns}:{}", listen.port()),
        _ if listen.ip().is_unspecified() => format!("https://{hostname}:{}", listen.port()),
        _ => local.clone(),
    };
    let mut names = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        hostname,
        local_ip.to_string(),
    ];
    if let Some((ip, dns)) = &tailnet {
        names.push(ip.to_string());
        names.extend(dns.clone());
    }
    names.sort();
    names.dedup();
    tls(&base, &names)?;
    let saved = base.join("host.json");
    if saved.exists() {
        fs::remove_file(&saved)?;
    }
    nodes::write_private(
        &saved,
        &json!({"controller":advertised,"local":local,"node":if controller_only { None } else { Some(&node) }}),
    )?;

    let credential = runner.join("node.json");
    let join = runner.join("join.json");
    if !controller_only {
        if external_worker {
            // The worker has its own unit and limits; restarting this process leaves its VMs running.
            wait_worker(&runner, Duration::from_secs(60))?;
        } else {
            let started = Command::new(std::env::current_exe()?)
                .arg("--local")
                .arg("--data-dir")
                .arg(&runner)
                .arg("up")
                .env("OW_MAX_MEMORY_MIB", memory.to_string())
                .env("OW_MAX_RUNNING", slots.to_string())
                .env("OW_MAX_VCPUS", cpus.to_string())
                .env(ow_core::idle::ENV, (idle_minutes * 60).to_string())
                .stdout(Stdio::null())
                .status()?;
            ensure!(
                started.success(),
                "local runner failed to start; see {}",
                runner.join("worker.log").display()
            );
        }
        if !credential.exists() {
            if join.exists() {
                fs::remove_file(&join)?;
            }
            nodes::invitation(
                &controller,
                &node,
                600,
                memory,
                slots,
                cpus,
                Some(&advertised),
                |v| nodes::write_private(&join, v),
            )
            .with_context(|| format!("enrolling local runner {node}; pass --node NEW_ID"))?;
        }
    }

    stop_on_signal()?;
    let (done, finished) = std::sync::mpsc::channel::<(&str, Result<()>)>();
    {
        let (done, controller) = (done.clone(), controller.clone());
        let (cert, key) = (base.join("tls/server.pem"), base.join("tls/server.key"));
        std::thread::spawn(move || {
            let result = nodes::controller(&controller, listen, Some(cert), Some(key), false);
            let _ = done.send(("controller", result));
        });
    }
    let since = Instant::now();
    while TcpStream::connect_timeout(
        &SocketAddr::new(local_ip, listen.port()),
        Duration::from_millis(200),
    )
    .is_err()
    {
        if let Ok((what, result)) = finished.try_recv() {
            return Err(result.err().unwrap_or_else(|| anyhow!("exited")))
                .with_context(|| format!("{what} on {listen}"));
        }
        ensure!(
            since.elapsed() < Duration::from_secs(10),
            "controller did not listen on {listen}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("Controller listening on {listen}; runners dial {advertised}");
    if !controller_only {
        {
            let (runner, local, ca) = (runner.clone(), local.clone(), base.join("tls/ca.pem"));
            std::thread::spawn(move || {
                let result =
                    nodes::agent(&runner, &local, &credential, Some(&join), Some(&ca), false);
                let _ = done.send(("local runner agent", result));
            });
        }
        if wait_online(&controller, &node, unix_now(), Duration::from_secs(30)) {
            println!("Runner {node} online. Add machines with: ow node add ssh://user@host");
        } else {
            println!("Runner {node} not online yet; check ow node list");
        }
    }
    if listen.ip().is_loopback() {
        println!("Loopback only: other machines cannot reach this controller (no tailnet found).");
    }
    let (what, result) = loop {
        match finished.recv_timeout(Duration::from_millis(200)) {
            Ok(done) => break done,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                if nodes::STOPPING.load(std::sync::atomic::Ordering::SeqCst) =>
            {
                // Under systemd's 960 s stop timeout; a capture in flight takes up to 900 s.
                drain(Duration::from_secs(900));
                return Ok(0);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => return Err(e.into()),
        }
    };
    result.with_context(|| format!("{what} stopped"))?;
    bail!("{what} stopped")
}

/// The key never appears in arguments, logs or saved files.
fn auth_key(source: &str) -> Result<String> {
    let key = if let Some(path) = source.strip_prefix("file:") {
        nodes::private(Path::new(path))
            .with_context(|| format!("{path} must be your own file with mode 0600"))?;
        fs::read_to_string(path)?
    } else if let Some(name) = source.strip_prefix("env:") {
        std::env::var(name).with_context(|| format!("environment variable {name} not set"))?
    } else {
        bail!("--tailscale-auth-key takes file:PATH or env:NAME, never the key itself")
    };
    let key = key.trim().to_owned();
    ensure!(
        !key.is_empty() && key.len() <= 512 && key.bytes().all(|b| b.is_ascii_graphic()),
        "Tailscale auth key is empty or malformed"
    );
    Ok(key)
}

fn sha256(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    std::io::copy(&mut fs::File::open(path)?, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

const PRECHECK: &str = r#"set -eu
d="${1:-$HOME/.ow-node}"
case "$d" in /*) ;; *) echo "remote directory must be absolute: $d" >&2; exit 2;; esac
[ "$(uname -s)/$(uname -m)" = Linux/x86_64 ] || { echo "runner must be Linux x86-64, found $(uname -sm)" >&2; exit 2; }
[ -r /dev/kvm ] && [ -w /dev/kvm ] || { echo "KVM unavailable: /dev/kvm must be readable and writable by $(id -un)" >&2; exit 2; }
[ -d "$d" ] || { mkdir -p "$d"; chmod 700 "$d"; }
case "$(stat -c %a "$d")" in ?00) ;; *) echo "$d must be private (chmod 700) or a new directory" >&2; exit 2;; esac
[ "$(stat -f -c %T "$d")" = btrfs ] || { echo "$d is not on Btrfs; pass --remote-dir on a Btrfs filesystem" >&2; exit 2; }
for t in sha256sum flock; do command -v $t >/dev/null || { echo "$t required on the runner" >&2; exit 2; }; done
cd "$d"
pwd -P
"#;

const TAILSCALE: &str = r#"set -eu
umask 077
f=$(mktemp)
trap 'rm -f "$f"' EXIT
cat > "$f"
if ! command -v tailscale >/dev/null; then
  curl -fsSL https://tailscale.com/install.sh | sudo -n sh >&2
fi
if tailscale status >/dev/null 2>&1; then
  echo "Tailscale already connected" >&2
else
  sudo -n tailscale up --auth-key="file:$f" --hostname="$1" >&2
fi
"#;

// Identical files are kept; anything else is replaced only after its digest matches.
const UPLOAD: &str = r#"set -eu
t="$1/$2"
if [ -f "$t" ] && [ "$(sha256sum < "$t" | cut -c1-64)" = "$3" ]; then exit 0; fi
umask 077
mkdir -p "${t%/*}"
cat > "$t.part"
[ "$(sha256sum < "$t.part" | cut -c1-64)" = "$3" ] || { rm -f "$t.part"; echo "transfer of $2 corrupted" >&2; exit 3; }
chmod "$4" "$t.part"
mv -f "$t.part" "$t"
"#;

// Enrolled runner: node ID, whether its agent holds its lock, worker status (may be empty).
const STATE: &str = r#"d=$1
grep -o '"node":"[^"]*"' "$d/data/node.json" | cut -d'"' -f4
if [ -e "$d/data/node-agent.lock" ] && ! flock -n "$d/data/node-agent.lock" true; then echo running; else echo stopped; fi
"$d/bin/ow" --local --data-dir "$d/data" status 2>/dev/null | tr -d '\n'
"#;

const WRITE: &str = r#"set -eu
umask 077
rm -f "$1/$2.part"
cat > "$1/$2.part"
chmod "$3" "$1/$2.part"
mv -f "$1/$2.part" "$1/$2"
"#;

fn start_script(
    dir: &str,
    url: &str,
    memory: u32,
    slots: u32,
    cpus: u32,
    idle_minutes: u64,
) -> String {
    let idle = idle_minutes * 60;
    let (d, url) = (common::shell_quote(dir), common::shell_quote(url));
    format!(
        r#"#!/bin/sh
# Written by `ow node add`. Run again after a reboot to restart this runner.
set -eu
d={d}
# Non-interactive SSH sessions often omit sbin, where nft and ip live.
export PATH="$PATH:/usr/sbin:/sbin"
export OW_ASSET_DIR="$d/assets" OW_MAX_MEMORY_MIB={memory} OW_MAX_RUNNING={slots} OW_MAX_VCPUS={cpus} OW_IDLE_HIBERNATE_SECS={idle}
"$d/bin/ow" --local --data-dir "$d/data" up >/dev/null
if [ -e "$d/data/node-agent.lock" ] && ! flock -n "$d/data/node-agent.lock" true; then
  echo "node agent already running" >&2
  exit 0
fi
setsid nohup "$d/bin/ow" --local --data-dir "$d/data" node-agent --controller {url} \
  --credential "$d/data/node.json" --join "$d/join.json" --ca-cert "$d/controller-ca.pem" \
  >>"$d/agent.log" 2>&1 </dev/null &
"#
    )
}

pub fn node(base: Option<PathBuf>, action: NodeAction) -> Result<i32> {
    node_with(OsStr::new("ssh"), base, action)
}

fn node_with(ssh: &OsStr, base: Option<PathBuf>, action: NodeAction) -> Result<i32> {
    let base = self::base(base)?;
    let controller = host::data_root(Some(base.join("controller")))?;
    if let NodeAction::Drain {
        node,
        action,
        evidence,
        certificate,
    } = action
    {
        println!(
            "{}",
            serde_json::to_string_pretty(&crate::drain::command(
                &controller,
                &node,
                &action,
                evidence.as_deref(),
                certificate.as_deref()
            )?)?
        );
        return Ok(0);
    }
    let NodeAction::Add {
        target: destination,
        name,
        controller: url,
        remote_dir,
        memory,
        slots,
        cpus,
        ubuntu,
        from_store,
        tailscale_auth_key,
        ow_binary,
        idle_minutes,
    } = action
    else {
        println!(
            "{}",
            serde_json::to_string_pretty(&nodes::inventory(&controller)?)?
        );
        return Ok(0);
    };
    common::host_limits(memory, slots, cpus)?;
    let saved: Value = serde_json::from_slice(
        &fs::read(base.join("host.json")).context("run `ow host up` on this machine first")?,
    )?;
    ensure!(
        onboarding::held_lock(&controller.join("node-controller.lock"))?,
        "the controller is not running; start `ow host up` first"
    );
    let url = nodes::origin(
        url.as_deref()
            .or(saved["controller"].as_str())
            .context("controller URL missing")?,
        false,
    )?;
    let key = tailscale_auth_key.as_deref().map(auth_key).transpose()?;
    let auth = connect::parse(&destination)?;
    let name = name.unwrap_or_else(|| auth.default_name());
    common::identifier(&name)?;
    ensure!(name != "local", "local is reserved");
    let binary = match ow_binary {
        Some(path) => path,
        None => Some(crate::assets().join("bin/ow-linux-amd64"))
            .filter(|p| p.exists())
            .map_or_else(std::env::current_exe, Ok)?,
    };
    let store_image = if from_store {
        let db = crate::image_catalog::open(&controller)?;
        Some(crate::image_catalog::resolve(&db, "ubuntu")?)
    } else {
        None
    };
    let mut files = vec![(binary, "bin/ow".to_owned(), true)];
    for (wire, path, _, executable) in onboarding::FILES {
        if ubuntu
            || from_store && wire == "network-tools.tar.gz"
            || !matches!(wire, "ubuntu.ext4" | "network-tools.tar.gz")
        {
            files.push((
                crate::assets().join(path),
                format!("assets/{path}"),
                executable,
            ));
        }
    }
    files.push((
        base.join("tls/ca.pem"),
        "controller-ca.pem".to_owned(),
        false,
    ));
    let digests = files
        .iter()
        .map(|(path, _, _)| {
            sha256(path).with_context(|| {
                format!(
                    "{} unavailable; prepare runtime assets or set OW_ASSET_DIR",
                    path.display()
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let ssh = Session::new(
        ssh.into(),
        base.join(format!("ssh-{}", common::nonce()?)),
        auth.as_ref(),
    );
    println!("Checking {destination}: Linux x86-64, KVM, Btrfs");
    let dir = ssh.run(
        PRECHECK,
        &[remote_dir.as_deref().unwrap_or_default()],
        Stdio::null(),
        &[],
    )?;
    ensure!(
        dir.starts_with('/') && dir.len() < 50 && !dir.contains('\n'),
        "remote directory must be an absolute path shorter than 50 bytes; pass --remote-dir"
    );
    if let Some(key) = key {
        println!("Joining the tailnet as {name}");
        ssh.run(TAILSCALE, &[&name], Stdio::null(), key.as_bytes())?;
    }
    println!("Installing ow and Firecracker into {dir}");
    for ((path, remote, executable), digest) in files.iter().zip(&digests) {
        let mode = if *executable { "700" } else { "600" };
        ssh.run(
            UPLOAD,
            &[&dir, remote, digest, mode],
            Stdio::from(fs::File::open(path)?),
            &[],
        )?;
    }
    if let Some(image) = &store_image {
        ssh.run(
            r#"mkdir -p "$1/data"; chmod 700 "$1/data"; printf '%s\n' "$3" >"$1/data/store-url"; chmod 600 "$1/data/store-url"; OW_ASSET_DIR="$1/assets" "$1/bin/ow" image pull "$2" --from "$3" --attempt "$4" --digest "$5" --runner "$1/data" --base-assets "$1/assets""#,
            &[&dir, &format!("{}@{}",image.name,image.version), &image.store_url, &image.reference.attempt, &image.reference.digest],
            Stdio::null(), &[],
        )?;
    }
    let report = ssh.run(
        r#""$1/bin/ow" --data-dir "$1/data" host doctor || true"#,
        &[&dir],
        Stdio::null(),
        &[],
    )?;
    let report: Value = serde_json::from_str(&report).context(
        "installed ow does not run on the runner; pass --ow-binary with a static Linux x86-64 build",
    )?;
    ensure!(
        report["supported"] == true,
        "runner cannot run workspaces: {}",
        report["diagnostics"]
    );
    let enrolled = ssh.run(
        r#"[ -e "$1/data/node.json" ] && echo yes || true"#,
        &[&dir],
        Stdio::null(),
        &[],
    )? == "yes";
    if enrolled {
        let state = ssh.run(STATE, &[&dir], Stdio::null(), &[])?;
        let mut lines = state.lines();
        let (remote, agent) = (lines.next().unwrap_or_default(), lines.next());
        ensure!(
            remote == name,
            "{dir} on {destination} is already enrolled as {remote}; pass --name {remote}"
        );
        let entry = nodes::inventory(&controller)?
            .as_array()
            .and_then(|all| all.iter().find(|n| n["id"] == name).cloned())
            .with_context(|| format!("{name} is enrolled with another controller"))?;
        ensure!(
            entry["revoked"] == false,
            "{name} is revoked; add the machine under a new --name with a new --remote-dir"
        );
        let owner = &entry["owner_limits"];
        ensure!(
            *owner == json!({"memory":memory,"slots":slots,"cpus":cpus}),
            "{name} is enrolled with --memory {} --slots {} --cpus {}; node add does not change an enrolled runner's capacity. Re-run with those values, or revoke it and add the machine under a new --name",
            owner["memory"],
            owner["slots"],
            owner["cpus"]
        );
        if let Ok(worker) = serde_json::from_str::<Value>(lines.next().unwrap_or_default()) {
            ensure!(
                worker["max_memory_mib"] == memory
                    && worker["max_running"] == slots
                    && worker["max_vcpus"] == cpus,
                "the worker on {destination} runs with other limits; stop it there with {dir}/bin/ow --local --data-dir {dir}/data down, then re-run"
            );
        }
        if agent == Some("running") {
            ensure!(
                wait_online(&controller, &name, 0, Duration::from_secs(20)),
                "{name}'s agent is running on {destination} but not online; inspect {dir}/agent.log there"
            );
            println!("Runner {name} is already running and online; nothing was restarted.");
            return Ok(0);
        }
        println!("{name} is enrolled but stopped; starting it");
    } else {
        nodes::invitation(
            &controller,
            &name,
            600,
            memory,
            slots,
            cpus,
            Some(&url),
            |v| {
                ssh.run(
                    WRITE,
                    &[&dir, "join.json", "600"],
                    Stdio::null(),
                    v.to_string().as_bytes(),
                )
                .map(drop)
            },
        )
        .with_context(|| format!("enrolling {name}; pass --name NEW_ID"))?;
    }
    let since = unix_now();
    ssh.run(
        &format!("{WRITE}\nexec sh \"$1/$2\""),
        &[&dir, "start.sh", "700"],
        Stdio::null(),
        start_script(&dir, &url, memory, slots, cpus, idle_minutes).as_bytes(),
    )?;
    ensure!(
        wait_online(&controller, &name, since, Duration::from_secs(45)),
        "runner {name} did not come online; inspect {dir}/agent.log and {dir}/data/worker.log on {destination}"
    );
    println!("Runner {name} is online. After a reboot of {destination} run {dir}/start.sh there.");
    Ok(0)
}

#[cfg(test)]
#[path = "selfhost_tests.rs"]
mod tests;
