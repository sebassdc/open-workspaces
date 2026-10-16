//! Single-host mode (`ow host up`) and SSH runner onboarding (`ow node add`).
//! Both compose the existing controller, worker, single-use join file and node agent.
use crate::{NodeAction, common, host, nodes, onboarding};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs,
    io::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const PORT: u16 = 8790;

fn base(explicit: Option<PathBuf>) -> Result<PathBuf> {
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

fn node_name(host: &str) -> String {
    let label = if host.parse::<IpAddr>().is_ok() {
        host
    } else {
        host.split('.').next().unwrap_or_default()
    };
    let name: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(32)
        .collect();
    if name.trim_matches('-').is_empty() || name == "local" {
        "runner".to_owned()
    } else {
        name
    }
}

fn wait_online(controller: &Path, node: &str, limit: Duration) -> bool {
    let started = Instant::now();
    while nodes::online(controller, node).is_err() {
        if started.elapsed() > limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    true
}

pub fn up(
    base: Option<PathBuf>,
    listen: Option<SocketAddr>,
    node: Option<String>,
    memory: u32,
    slots: u32,
    cpus: u32,
) -> Result<i32> {
    common::host_limits(memory, slots, cpus)?;
    let base = self::base(base)?;
    let controller = host::data_root(Some(base.join("controller")))?;
    let runner = host::data_root(Some(base.join("runner")))?;
    ensure!(
        !onboarding::held_lock(&controller.join("node-controller.lock"))?,
        "a controller is already running for {}",
        base.display()
    );
    let report = onboarding::doctor(&runner)?;
    ensure!(
        report["supported"] == true,
        "this machine cannot run workspaces: {}",
        report["diagnostics"]
    );
    let hostname = fs::read_to_string("/proc/sys/kernel/hostname")?
        .trim()
        .to_owned();
    let node = node.unwrap_or_else(|| node_name(&hostname));
    common::identifier(&node)?;
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
        &json!({"controller":advertised,"local":local,"node":node}),
    )?;

    let started = Command::new(std::env::current_exe()?)
        .arg("--local")
        .arg("--data-dir")
        .arg(&runner)
        .arg("up")
        .env("OW_MAX_MEMORY_MIB", memory.to_string())
        .env("OW_MAX_RUNNING", slots.to_string())
        .env("OW_MAX_VCPUS", cpus.to_string())
        .stdout(Stdio::null())
        .status()?;
    ensure!(
        started.success(),
        "local runner failed to start; see {}",
        runner.join("worker.log").display()
    );
    let credential = runner.join("node.json");
    let join = runner.join("join.json");
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
    {
        let (runner, local, ca) = (runner.clone(), local.clone(), base.join("tls/ca.pem"));
        std::thread::spawn(move || {
            let result = nodes::agent(&runner, &local, &credential, Some(&join), Some(&ca), false);
            let _ = done.send(("local runner agent", result));
        });
    }
    println!("Controller listening on {listen}; runners dial {advertised}");
    if wait_online(&controller, &node, Duration::from_secs(30)) {
        println!("Runner {node} online. Add machines with: ow node add ssh://user@host");
    } else {
        println!("Runner {node} not online yet; check ow node list");
    }
    if listen.ip().is_loopback() {
        println!("Loopback only: other machines cannot reach this controller (no tailnet found).");
    }
    let (what, result) = finished.recv()?;
    result.with_context(|| format!("{what} stopped"))?;
    bail!("{what} stopped")
}

/// Remote shell over the system OpenSSH client; config aliases and agents apply.
struct Ssh {
    program: PathBuf,
    control: PathBuf,
    args: Vec<String>,
}
impl Ssh {
    fn command(&self) -> Command {
        let mut c = Command::new(&self.program);
        c.arg("-o")
            .arg("ControlMaster=auto")
            .arg("-o")
            .arg(format!("ControlPath={}", self.control.display()))
            .arg("-o")
            .arg("ControlPersist=60")
            .args(&self.args);
        c
    }
    /// Runs `script` under remote `sh` with quoted positional arguments.
    fn run(&self, script: &str, args: &[&str], stdin: Stdio, bytes: &[u8]) -> Result<String> {
        let mut remote = format!("sh -c {} sh", common::shell_quote(script));
        for arg in args {
            remote.push(' ');
            remote.push_str(&common::shell_quote(arg));
        }
        let mut child = self
            .command()
            .arg(remote)
            .stdin(if bytes.is_empty() {
                stdin
            } else {
                Stdio::piped()
            })
            .stdout(Stdio::piped())
            .spawn()
            .context("ssh client required")?;
        if !bytes.is_empty() {
            child.stdin.take().context("ssh stdin")?.write_all(bytes)?;
        }
        let output = child.wait_with_output()?;
        ensure!(
            output.status.success(),
            "remote step failed ({})",
            output.status
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }
}
impl Drop for Ssh {
    fn drop(&mut self) {
        let _ = self
            .command()
            .args(["-O", "exit"])
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status();
    }
}

/// `ssh://user@host[:port]` (scheme optional) to client arguments plus a default node ID.
fn target(value: &str) -> Result<(Vec<String>, String)> {
    let url = reqwest::Url::parse(&if value.contains("://") {
        value.to_owned()
    } else {
        format!("ssh://{value}")
    })
    .context("use ssh://user@host[:port]")?;
    ensure!(
        url.scheme() == "ssh"
            && matches!(url.path(), "" | "/")
            && url.query().is_none()
            && url.fragment().is_none()
            && url.password().is_none(),
        "use ssh://user@host[:port]"
    );
    let host = url
        .host_str()
        .context("SSH host required")?
        .trim_start_matches('[')
        .trim_end_matches(']');
    ensure!(
        !host.is_empty()
            && !host.starts_with('-')
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-_:".contains(&b)),
        "invalid SSH host"
    );
    let mut args = Vec::new();
    if let Some(port) = url.port() {
        args.extend(["-p".to_owned(), port.to_string()]);
    }
    let user = url.username();
    if !user.is_empty() {
        ensure!(
            !user.starts_with('-')
                && user
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "invalid SSH user"
        );
        args.extend(["-l".to_owned(), user.to_owned()]);
    }
    args.extend(["--".to_owned(), host.to_owned()]);
    Ok((args, node_name(host)))
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
mkdir -p "$d"
chmod 700 "$d"
[ "$(stat -f -c %T "$d")" = btrfs ] || { echo "$d is not on Btrfs; pass --remote-dir on a Btrfs filesystem" >&2; exit 2; }
command -v sha256sum >/dev/null || { echo "sha256sum required on the runner" >&2; exit 2; }
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

const WRITE: &str = r#"set -eu
umask 077
rm -f "$1/$2.part"
cat > "$1/$2.part"
chmod "$3" "$1/$2.part"
mv -f "$1/$2.part" "$1/$2"
"#;

fn start_script(dir: &str, url: &str, memory: u32, slots: u32, cpus: u32) -> String {
    let (d, url) = (common::shell_quote(dir), common::shell_quote(url));
    format!(
        r#"#!/bin/sh
# Written by `ow node add`. Run again after a reboot to restart this runner.
set -eu
d={d}
# Non-interactive SSH sessions often omit sbin, where nft and ip live.
export PATH="$PATH:/usr/sbin:/sbin"
export OW_ASSET_DIR="$d/assets" OW_MAX_MEMORY_MIB={memory} OW_MAX_RUNNING={slots} OW_MAX_VCPUS={cpus}
"$d/bin/ow" --local --data-dir "$d/data" up >/dev/null
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
    let NodeAction::Add {
        target: destination,
        name,
        controller: url,
        remote_dir,
        memory,
        slots,
        cpus,
        ubuntu,
        tailscale_auth_key,
        ow_binary,
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
    let (args, default_name) = target(&destination)?;
    let name = name.unwrap_or(default_name);
    common::identifier(&name)?;
    ensure!(name != "local", "local is reserved");
    let binary = match ow_binary {
        Some(path) => path,
        None => Some(crate::assets().join("bin/ow-linux-amd64"))
            .filter(|p| p.exists())
            .map_or_else(std::env::current_exe, Ok)?,
    };
    let mut files = vec![(binary, "bin/ow".to_owned(), true)];
    for (wire, path, _, executable) in onboarding::FILES {
        if ubuntu || !matches!(wire, "ubuntu.ext4" | "network-tools.tar.gz") {
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

    let ssh = Ssh {
        program: ssh.into(),
        control: base.join(format!("ssh-{}", common::nonce()?)),
        args,
    };
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
        println!("{name} already enrolled; restarting it");
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
    ssh.run(
        &format!("{WRITE}\nexec sh \"$1/$2\""),
        &[&dir, "start.sh", "700"],
        Stdio::null(),
        start_script(&dir, &url, memory, slots, cpus).as_bytes(),
    )?;
    ensure!(
        wait_online(&controller, &name, Duration::from_secs(45)),
        "runner {name} did not come online; inspect {dir}/agent.log and {dir}/data/worker.log on {destination}"
    );
    println!("Runner {name} is online. After a reboot of {destination} run {dir}/start.sh there.");
    Ok(0)
}

#[cfg(test)]
#[path = "selfhost_tests.rs"]
mod tests;
