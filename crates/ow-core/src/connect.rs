//! Pluggable connect: how the CLI reaches and authenticates to a host it manages.
//!
//! Every provider drives the system OpenSSH client, so `~/.ssh/config`, agents and
//! `known_hosts` apply. AWS SSM and Tailscale only change the transport:
//! - `ssh://user@host[:port]`: direct TCP, SSH key or agent.
//! - `ssm://user@i-0123456789abcdef0[:port]`: tunnel through AWS Systems Manager
//!   (`AWS-StartSSHSession`, IAM-authorized; needs the AWS CLI and Session Manager
//!   plugin, region and profile from the usual `AWS_*` variables), then SSH login.
//! - `tailscale://user@host[:port]`: tunnel through `tailscale nc` over the tailnet;
//!   with Tailscale SSH on the host, the tailnet identity is the login.
use crate::common;
use anyhow::{Context, Result, ensure};
use std::{
    io::Write,
    net::IpAddr,
    path::PathBuf,
    process::{Command, Stdio},
};

/// Where a provider connects: optional login user and port, and a validated host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Destination {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

pub trait ConnectAuth {
    /// URL scheme naming the provider: `ssh`, `ssm` or `tailscale`.
    fn scheme(&self) -> &'static str;
    fn destination(&self) -> &Destination;
    /// OpenSSH `ProxyCommand` carrying the connection; `None` is direct TCP.
    fn proxy_command(&self) -> Option<&'static str>;

    /// OpenSSH client arguments: options, then `--` and the host.
    fn ssh_args(&self) -> Vec<String> {
        let d = self.destination();
        let mut args = Vec::new();
        if let Some(proxy) = self.proxy_command() {
            args.extend(["-o".to_owned(), format!("ProxyCommand={proxy}")]);
        }
        if let Some(port) = d.port {
            args.extend(["-p".to_owned(), port.to_string()]);
        }
        if let Some(user) = &d.user {
            args.extend(["-l".to_owned(), user.clone()]);
        }
        args.extend(["--".to_owned(), d.host.clone()]);
        args
    }

    /// Default node ID for this host: its first DNS label.
    fn default_name(&self) -> String {
        node_name(&self.destination().host)
    }
}

pub struct Ssh(pub Destination);
pub struct Ssm(pub Destination);
pub struct Tailscale(pub Destination);

impl ConnectAuth for Ssh {
    fn scheme(&self) -> &'static str {
        "ssh"
    }
    fn destination(&self) -> &Destination {
        &self.0
    }
    fn proxy_command(&self) -> Option<&'static str> {
        None
    }
}

impl ConnectAuth for Ssm {
    fn scheme(&self) -> &'static str {
        "ssm"
    }
    fn destination(&self) -> &Destination {
        &self.0
    }
    fn proxy_command(&self) -> Option<&'static str> {
        // AWS's documented SSH-over-Session-Manager tunnel; %h is a validated instance ID.
        Some(
            "aws ssm start-session --target %h --document-name AWS-StartSSHSession --parameters portNumber=%p",
        )
    }
}

impl ConnectAuth for Tailscale {
    fn scheme(&self) -> &'static str {
        "tailscale"
    }
    fn destination(&self) -> &Destination {
        &self.0
    }
    fn proxy_command(&self) -> Option<&'static str> {
        Some("tailscale nc %h %p")
    }
}

const USAGE: &str = "use ssh://, ssm:// or tailscale://user@host[:port]";

/// Parses a target URL; a bare `user@host` means `ssh://`.
pub fn parse(value: &str) -> Result<Box<dyn ConnectAuth>> {
    let url = url::Url::parse(&if value.contains("://") {
        value.to_owned()
    } else {
        format!("ssh://{value}")
    })
    .context(USAGE)?;
    ensure!(
        matches!(url.path(), "" | "/")
            && url.query().is_none()
            && url.fragment().is_none()
            && url.password().is_none(),
        USAGE
    );
    let host = url
        .host_str()
        .context("host required")?
        .trim_start_matches('[')
        .trim_end_matches(']');
    ensure!(
        !host.is_empty()
            && !host.starts_with('-')
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-_:".contains(&b)),
        "invalid host"
    );
    let user = url.username();
    ensure!(
        !user.starts_with('-')
            && user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "invalid SSH user"
    );
    let destination = Destination {
        user: (!user.is_empty()).then(|| user.to_owned()),
        host: host.to_owned(),
        port: url.port(),
    };
    Ok(match url.scheme() {
        "ssh" => Box::new(Ssh(destination)),
        "ssm" => {
            ensure!(
                instance_id(host),
                "ssm:// takes an EC2 instance ID (i-…) or managed node ID (mi-…)"
            );
            Box::new(Ssm(destination))
        }
        "tailscale" => Box::new(Tailscale(destination)),
        _ => anyhow::bail!(USAGE),
    })
}

fn instance_id(host: &str) -> bool {
    let hex = host
        .strip_prefix("i-")
        .filter(|h| h.len() == 8 || h.len() == 17)
        .or_else(|| host.strip_prefix("mi-").filter(|h| h.len() == 17));
    hex.is_some_and(|h| {
        h.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// A valid node ID from a host name: its first DNS label, or `runner`.
pub fn node_name(host: &str) -> String {
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

/// Remote shell over the system OpenSSH client, sharing one master connection.
pub struct Session {
    pub program: PathBuf,
    pub control: PathBuf,
    pub args: Vec<String>,
}

impl Session {
    pub fn new(program: PathBuf, control: PathBuf, auth: &dyn ConnectAuth) -> Self {
        Self {
            program,
            control,
            args: auth.ssh_args(),
        }
    }

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
    pub fn run(&self, script: &str, args: &[&str], stdin: Stdio, bytes: &[u8]) -> Result<String> {
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

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self
            .command()
            .args(["-O", "exit"])
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(value: &str) -> Vec<String> {
        parse(value).unwrap().ssh_args()
    }

    #[test]
    fn ssh_targets() {
        assert_eq!(
            args("ssh://dev@build-1.example.ts.net:2222"),
            ["-p", "2222", "-l", "dev", "--", "build-1.example.ts.net"]
        );
        assert_eq!(
            parse("ssh://dev@build-1.example.ts.net")
                .unwrap()
                .default_name(),
            "build-1"
        );
        assert_eq!(args("dev@10.0.0.7"), ["-l", "dev", "--", "10.0.0.7"]);
        assert_eq!(parse("dev@10.0.0.7").unwrap().default_name(), "10-0-0-7");
        assert_eq!(parse("ssh://pc").unwrap().default_name(), "pc");
        assert_eq!(parse("ssh://pc").unwrap().scheme(), "ssh");
        assert_eq!(args("ssh://[::1]"), ["--", "::1"]);
        for bad in [
            "https://host",
            "ssh://-oProxyCommand=x",
            "ssh://-x@host",
            "ssh://user:secret@host",
            "ssh://host/path",
            "ssh://host?x=1",
            "ssh://host;reboot",
            "ssh://",
        ] {
            assert!(parse(bad).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn ssm_targets_tunnel_through_session_manager() {
        let ssm = parse("ssm://ubuntu@i-0123456789abcdef0").unwrap();
        assert_eq!(ssm.scheme(), "ssm");
        assert_eq!(ssm.default_name(), "i-0123456789abcdef0");
        assert_eq!(
            ssm.ssh_args(),
            [
                "-o",
                "ProxyCommand=aws ssm start-session --target %h --document-name AWS-StartSSHSession --parameters portNumber=%p",
                "-l",
                "ubuntu",
                "--",
                "i-0123456789abcdef0"
            ]
        );
        assert_eq!(args("ssm://i-0123abcd:2222")[2..4], ["-p", "2222"]);
        assert!(parse("ssm://mi-0123456789abcdef0").is_ok());
        for bad in [
            "ssm://host.example.com",
            "ssm://i-0123",
            "ssm://i-0123456789ABCDEF0",
            "ssm://i-0123456789abcdef0;x",
            "ssm://ubuntu@i-0123456789abcdef0/x",
        ] {
            assert!(parse(bad).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn tailscale_targets_tunnel_through_the_tailnet() {
        let tailnet = parse("tailscale://dev@build-1.tail1234.ts.net").unwrap();
        assert_eq!(tailnet.scheme(), "tailscale");
        assert_eq!(tailnet.default_name(), "build-1");
        assert_eq!(
            tailnet.ssh_args(),
            [
                "-o",
                "ProxyCommand=tailscale nc %h %p",
                "-l",
                "dev",
                "--",
                "build-1.tail1234.ts.net"
            ]
        );
        assert_eq!(args("tailscale://100.64.0.7").last().unwrap(), "100.64.0.7");
        assert!(parse("tailscale://-oX=y").is_err());
    }

    /// Real OpenSSH through each transport; `scripts/connect-e2e.sh` sets this up in a
    /// container. `OW_CONNECT_E2E_TARGETS` (comma-separated) and `OW_CONNECT_E2E_SSH`
    /// (client program) point it at real hosts instead.
    #[test]
    #[ignore = "needs sshd on 127.0.0.1:22 for root and aws/tailscale stand-ins on PATH"]
    fn real_openssh_over_each_transport() {
        let targets = std::env::var("OW_CONNECT_E2E_TARGETS").unwrap_or_else(|_| {
            "ssh://root@127.0.0.1:22,ssm://root@i-0123456789abcdef0,tailscale://root@build-1.tail1234.ts.net".into()
        });
        let program = std::env::var_os("OW_CONNECT_E2E_SSH").unwrap_or_else(|| "ssh".into());
        // Short path: ControlPath sockets must fit the Unix socket limit (104 bytes on macOS).
        let dir = PathBuf::from(format!("/tmp/ow-connect-{}", common::nonce().unwrap()));
        std::fs::create_dir(&dir).unwrap();
        for (i, target) in targets.split(',').enumerate() {
            let auth = parse(target).unwrap();
            let control = dir.join(format!("c{i}"));
            let session = Session::new(program.clone().into(), control.clone(), auth.as_ref());
            let output = session
                .run(
                    r#"echo "$1"; cat"#,
                    &["it's quoted"],
                    Stdio::null(),
                    b"stdin bytes",
                )
                .unwrap();
            assert_eq!(output, "it's quoted\nstdin bytes", "{target}");
            // A second step reuses the master connection the first one opened.
            assert!(control.exists(), "{target}: no master connection");
            assert_eq!(
                session.run("echo again", &[], Stdio::null(), &[]).unwrap(),
                "again"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn default_names_are_valid_ids() {
        assert_eq!(node_name("local"), "runner");
        assert_eq!(node_name("..."), "runner");
        assert_eq!(node_name("My_Box.lan"), "My_Box");
        assert_eq!(node_name(&"a".repeat(40)).len(), 32);
        common::identifier(&node_name("fe80::1")).unwrap();
    }
}
