//! Fixed guest OpenSSH endpoint and snapshot-external authorization policy.
use crate::{runtime, vm};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::{Shutdown, TcpStream},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const FRAME: usize = 16384;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub identity: String,
    pub generation: String,
    pub host_key: String,
    pub keys: Vec<String>,
    pub ready: bool,
}
pub fn path(root: &Path, id: &str) -> PathBuf {
    root.join("machines").join(id).join("ssh.json")
}
pub fn load(root: &Path, id: &str) -> Result<Policy> {
    read_policy(&path(root, id))
}
fn read_policy(path: &Path) -> Result<Policy> {
    let file = fs::File::open(path).context("SSH not enrolled; use ow ssh-authorize")?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= 16384,
        "SSH policy too large or not regular"
    );
    let mut bytes = Vec::new();
    file.take(16385).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 16384, "SSH policy too large");
    let p: Policy = serde_json::from_slice(&bytes)?;
    crate::common::identifier(&p.identity)?;
    crate::common::identifier(&p.generation)?;
    ensure!(key(&p.host_key)? == p.host_key, "noncanonical SSH host key");
    ensure!(
        validate_keys(&json!(p.keys))? == p.keys,
        "noncanonical SSH keys"
    );
    Ok(p)
}
pub fn key(input: &str) -> Result<String> {
    ensure!(
        input.len() <= 512 && !input.contains(['\r', '\n']),
        "one public key required"
    );
    let fields: Vec<_> = input.split_whitespace().collect();
    ensure!(
        fields.len() >= 2 && fields[0] == "ssh-ed25519",
        "only plain Ed25519 public keys accepted"
    );
    let bytes = STANDARD.decode(fields[1])?;
    let mut expected = Vec::new();
    expected.extend_from_slice(&11u32.to_be_bytes());
    expected.extend_from_slice(b"ssh-ed25519");
    expected.extend_from_slice(&32u32.to_be_bytes());
    ensure!(
        bytes.len() == 51 && bytes[..19] == expected,
        "malformed Ed25519 public key"
    );
    Ok(format!("ssh-ed25519 {}", STANDARD.encode(bytes)))
}
pub fn validate_keys(v: &Value) -> Result<Vec<String>> {
    let list = v.as_array().context("keys array required")?;
    ensure!(list.len() <= 16, "at most 16 keys");
    let mut keys = Vec::new();
    for k in list {
        let k = key(k.as_str().context("public key required")?)?;
        ensure!(!keys.contains(&k), "duplicate key");
        keys.push(k);
    }
    Ok(keys)
}
pub fn kill(machine: &mut vm::Vm) -> Result<()> {
    machine.checked("if [ -f /etc/ow-ssh-v1 ]; then command -v pkill >/dev/null || exit 1; pkill -x sshd 2>/dev/null || :; pkill -x sshd-session 2>/dev/null || :; pkill -x sshd-auth 2>/dev/null || :; rm -f /run/ow-sshd.pid; fi")?;
    Ok(())
}
pub fn apply(machine: &mut vm::Vm, policy: &Policy) -> Result<()> {
    machine.checked("test -f /etc/ow-ssh-v1 && test -x /usr/sbin/sshd && id dev >/dev/null")?;
    kill(machine)?;
    let content = policy.keys.join("\n") + "\n";
    // root-owned readable file: sshd opens it under dev's effective UID.
    machine.checked(&format!("mkdir -p /run/sshd /dev/pts; mountpoint -q /dev/pts || mount -t devpts devpts /dev/pts; chmod 755 /etc/ow-ssh; umask 077; printf %s {} > /etc/ow-ssh/authorized_keys.tmp; chmod 644 /etc/ow-ssh/authorized_keys.tmp; mv /etc/ow-ssh/authorized_keys.tmp /etc/ow-ssh/authorized_keys; test -s /etc/ssh/ssh_host_ed25519_key || ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key",vm::shell_quote(&content)))?;
    let actual = key(machine
        .checked("cat /etc/ssh/ssh_host_ed25519_key.pub")?
        .trim())?;
    ensure!(
        actual == policy.host_key,
        "SSH host identity changed; refusing access. Inspect restore lineage; never auto-trust changed keys"
    );
    machine.checked(
        "/usr/sbin/sshd -t -f /etc/ow-ssh/sshd_config && /usr/sbin/sshd -f /etc/ow-ssh/sshd_config",
    )?;
    Ok(())
}
pub fn enroll(
    machine: &mut vm::Vm,
    root: &Path,
    id: &str,
    keys: Vec<String>,
    upgrade: bool,
) -> Result<Value> {
    if upgrade {
        ensure!(
            machine.command("test -f /etc/ow-ssh-v1")?.1 != 0,
            "already SSH capable; omit --upgrade"
        );
        // Explicitly requested guest mutation only. No package commands on host.
        machine.checked(&format!(
            "/bin/sh -c {}",
            vm::shell_quote(include_str!("../../../experiments/ssh-guest/provision.sh"))
        ))?;
    }
    machine.checked("test -f /etc/ow-ssh-v1 && test -x /usr/sbin/sshd")?;
    let previous = if path(root, id).exists() {
        Some(load(root, id)?)
    } else {
        None
    };
    machine.checked("test -s /etc/ssh/ssh_host_ed25519_key || ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key")?;
    let host_key = key(machine
        .checked("cat /etc/ssh/ssh_host_ed25519_key.pub")?
        .trim())?;
    if let Some(p) = &previous {
        ensure!(
            p.host_key == host_key,
            "SSH host identity changed; refusing key update"
        );
    }
    let mut p = Policy {
        identity: previous.map(|p| p.identity).unwrap_or(vm::nonce()?),
        generation: vm::nonce()?,
        host_key,
        keys,
        ready: false,
    };
    // Persist fence before mutation; failure denies old streams and future connects.
    runtime::atomic_json(&path(root, id), &json!(p))?;
    apply(machine, &p)?;
    p.ready = true;
    runtime::atomic_json(&path(root, id), &json!(p))?;
    Ok(info(&p))
}
pub fn info(p: &Policy) -> Value {
    json!({"identity":p.identity,"host_key":p.host_key,"key_count":p.keys.len(),"ready":p.ready,"user":"dev","port":22})
}

pub fn bridge(
    mut unix: UnixStream,
    mut tcp: TcpStream,
    policy_path: PathBuf,
    generation: String,
) -> Result<()> {
    unix.set_read_timeout(Some(Duration::from_secs(5)))?;
    unix.set_write_timeout(Some(Duration::from_secs(5)))?;
    tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(5)))?;
    let start = Instant::now();
    let mut active = Instant::now();
    let mut buf = [0; FRAME];
    let valid = || -> bool {
        read_policy(&policy_path).is_ok_and(|p| p.ready && p.generation == generation)
    };
    let result = (|| -> Result<()> {
        loop {
            ensure!(valid(), "SSH key policy replaced");
            ensure!(
                start.elapsed() < Duration::from_secs(3600)
                    && active.elapsed() < Duration::from_secs(90),
                "SSH stream expired"
            );
            let mut fds = [
                libc::pollfd {
                    fd: unix.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: tcp.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, 200) };
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(std::io::Error::last_os_error().into());
            }
            for i in 0..2 {
                if fds[i].revents != 0 {
                    ensure!(valid(), "stale SSH stream");
                    let n = if i == 0 {
                        unix.read(&mut buf)?
                    } else {
                        tcp.read(&mut buf)?
                    };
                    if n == 0 {
                        return Ok(());
                    }
                    ensure!(valid(), "stale SSH bytes");
                    if i == 0 {
                        tcp.write_all(&buf[..n])?;
                    } else {
                        unix.write_all(&buf[..n])?;
                    }
                    active = Instant::now();
                }
            }
        }
    })();
    let _ = unix.shutdown(Shutdown::Both);
    let _ = tcp.shutdown(Shutdown::Both);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_keys_are_strict() {
        let mut b = Vec::from(&11u32.to_be_bytes()[..]);
        b.extend(b"ssh-ed25519");
        b.extend(32u32.to_be_bytes());
        b.extend([7; 32]);
        let k = format!("ssh-ed25519 {}", STANDARD.encode(b));
        assert_eq!(key(&k).unwrap(), k);
        for bad in [
            format!("command=evil {k}"),
            format!("{k}\n{k}"),
            "ssh-ed25519 AAAA".into(),
            "-----BEGIN OPENSSH PRIVATE KEY-----".into(),
        ] {
            assert!(key(&bad).is_err());
        }
        assert!(validate_keys(&json!([k.clone(), k])).is_err());
        assert!(validate_keys(&json!(["bad"])).is_err());
        let keys: Vec<_> = (0u8..17)
            .map(|v| {
                let mut b = Vec::from(&11u32.to_be_bytes()[..]);
                b.extend(b"ssh-ed25519");
                b.extend(32u32.to_be_bytes());
                b.extend([v; 32]);
                format!("ssh-ed25519 {}", STANDARD.encode(b))
            })
            .collect();
        assert!(validate_keys(&json!(&keys[..16])).is_ok());
        assert!(validate_keys(&json!(keys)).is_err());
    }
    #[test]
    fn policy_replacement_fences_both_byte_directions_and_cleans_up() {
        use std::net::TcpListener;
        let root = std::env::temp_dir().join(format!("ow-ssh-fence-{}", vm::nonce().unwrap()));
        fs::create_dir(&root).unwrap();
        let path = root.join("policy.json");
        let p = Policy {
            identity: "id".into(),
            generation: "first".into(),
            host_key: {
                let mut b = Vec::from(&11u32.to_be_bytes()[..]);
                b.extend(b"ssh-ed25519");
                b.extend(32u32.to_be_bytes());
                b.extend([7; 32]);
                format!("ssh-ed25519 {}", STANDARD.encode(b))
            },
            keys: vec![],
            ready: true,
        };
        runtime::atomic_json(&path, &json!(p)).unwrap();
        let (mut client, unix) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut guest, _) = listener.accept().unwrap();
        guest
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let bridge_path = path.clone();
        let thread = std::thread::spawn(move || bridge(unix, tcp, bridge_path, "first".into()));
        client.write_all(b"client").unwrap();
        let mut b = [0; 6];
        guest.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"client");
        guest.write_all(b"server").unwrap();
        client.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"server");
        let mut next = p;
        next.generation = "replacement".into();
        runtime::atomic_json(&path, &json!(next)).unwrap();
        let _ = client.write_all(b"STALE");
        let _ = guest.write_all(b"STALE");
        assert!(thread.join().unwrap().is_err());
        let closed = |r: std::io::Result<usize>| match r {
            Ok(0) => true,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => true,
            _ => false,
        };
        assert!(
            closed(client.read(&mut b)),
            "client did not receive EOF/reset"
        );
        assert!(
            closed(guest.read(&mut b)),
            "guest did not receive EOF/reset"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
