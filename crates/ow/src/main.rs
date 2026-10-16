#[cfg(target_os = "linux")]
mod backup;
#[cfg(target_os = "linux")]
mod catalog;
mod client;
#[cfg(target_os = "linux")]
mod dashboard;
#[cfg(target_os = "linux")]
mod host;
#[cfg(target_os = "macos")]
mod local_terminal;
#[cfg(target_os = "macos")]
mod mac_host;
#[cfg(target_os = "macos")]
mod mac_worker;
mod nodes;
#[cfg(target_os = "macos")]
#[path = "mac_onboarding.rs"]
mod onboarding;
#[cfg(target_os = "linux")]
mod onboarding;
#[cfg(target_os = "linux")]
mod remote;
#[cfg(target_os = "linux")]
mod selfhost;
mod ssh_client;
#[cfg(target_os = "linux")]
mod ssh_gateway;
#[cfg(target_os = "linux")]
mod terminal;
mod wire;

// Firecracker runtime and shared limits live in ow-core; keep their `crate::` paths.
use ow_core::common;
#[cfg(target_os = "linux")]
use ow_core::{network, runtime, ssh_guest, vm};

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    version,
    about = "Persistent microVM workspaces: local worker and remote HTTPS/WSS client"
)]
struct Cli {
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Use a remote HTTPS workspace dashboard.
    #[arg(long, global = true)]
    server: Option<String>,
    /// Retry a remote side effect using its original owner-scoped key.
    #[arg(long, global = true)]
    operation_key: Option<String>,
    /// Ignore a saved remote login and use the local worker.
    #[arg(long = "local", global = true, conflicts_with = "server")]
    local_mode: bool,
    #[command(subcommand)]
    command: Action,
}

#[cfg(target_os = "macos")]
#[derive(Subcommand)]
enum MacHostAction {
    /// Fetch official supported macOS guest requirements without downloading an IPSW.
    MacosProbe,
    /// Report native backend capabilities; does not start a guest.
    Capabilities,
    /// Start one offline ARM64 guest in a prepared private fixture root.
    Boot,
    /// Experimental same-machine paired state restore in the fixture root.
    Restore {
        /// Verified paired writable disk copy; never a Firecracker snapshot.
        #[arg(long)]
        disk: PathBuf,
    },
}

#[derive(Subcommand)]
enum Action {
    /// Experimental local Apple Silicon runtime (separate from pool hosting).
    #[cfg(target_os = "macos")]
    MacHost {
        #[command(subcommand)]
        command: MacHostAction,
    },
    /// Invite or contribute a trusted Linux host to the shared private pool.
    Host {
        #[command(subcommand)]
        command: HostAction,
    },
    /// Runners of the controller started by `ow host up` on this machine.
    Node {
        #[command(subcommand)]
        command: NodeAction,
    },
    /// Back up the controller catalog or a runner's snapshots (Linux, this machine).
    Backup {
        #[command(subcommand)]
        command: BackupAction,
    },
    /// Sign in through Cloudflare Access and save the remote server.
    Login {
        url: String,
    },
    /// Authenticated browser dashboard and scoped workspace API.
    Dashboard {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8787")]
        listen: std::net::SocketAddr,
    },
    /// Serve one guest HTTP app behind Cloudflare Access. Foreground, loopback only.
    Serve {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8787")]
        listen: std::net::SocketAddr,
    },
    /// Dedicated node listener; use TLS except explicit loopback tests.
    NodeController {
        #[arg(long, default_value = "127.0.0.1:8790")]
        listen: std::net::SocketAddr,
        #[arg(long)]
        tls_cert: Option<PathBuf>,
        #[arg(long)]
        tls_key: Option<PathBuf>,
        #[arg(long)]
        insecure_loopback_test: bool,
    },
    /// Mint a private, expiring, single-use enrollment file.
    NodeJoin {
        node: String,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 600)]
        ttl: u64,
        #[arg(long, default_value_t = 1024)]
        memory: u32,
        #[arg(long, default_value_t = 2)]
        slots: u32,
        /// Legacy default remains 16; guided invitations normally cap at 2.
        #[arg(long, default_value_t = 16)]
        cpus: u32,
        /// Include a validated self-contained HTTPS controller endpoint.
        #[arg(long)]
        controller: Option<String>,
    },
    NodeRevoke {
        node: String,
    },
    Nodes,
    /// Connect this worker outbound; credentials are stored outside the guest.
    NodeAgent {
        #[arg(long)]
        controller: String,
        #[arg(long)]
        credential: PathBuf,
        #[arg(long)]
        join: Option<PathBuf>,
        #[arg(long)]
        ca_cert: Option<PathBuf>,
        #[arg(long)]
        insecure_loopback_test: bool,
    },
    Up,
    Down,
    Status,
    Stats,
    List,
    Inspect {
        id: String,
    },
    Create {
        id: String,
        #[arg(long, default_value = "alpine", value_parser = ["alpine", "arch", "ubuntu", "ubuntu-arm64"])]
        image: String,
        #[arg(long, default_value_t = 256)]
        memory: u32,
        #[arg(long, default_value_t = 1)]
        cpus: u32,
        /// Placement on a connected node (remote controller only).
        #[arg(long)]
        node: Option<String>,
    },
    /// Change a stopped workspace's resources; next start is a cold boot.
    Resize {
        id: String,
        #[arg(long)]
        memory: u32,
        #[arg(long)]
        cpus: u32,
    },
    Start {
        id: String,
    },
    Stop {
        id: String,
    },
    Exec {
        id: String,
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<String>,
    },
    Shell {
        id: String,
    },
    /// Run ordinary OpenSSH into the guest (use -- before ssh options/command).
    Ssh {
        id: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Binary stdio for standard OpenSSH ProxyCommand; no diagnostics on stdout.
    SshProxy {
        id: String,
    },
    /// Print reviewable OpenSSH config without changing ~/.ssh/config.
    SshConfig {
        id: String,
    },
    /// Replace this machine's registered public keys; private keys stay local.
    SshAuthorize {
        id: String,
        #[arg(long, required = true)]
        key: Vec<PathBuf>,
        #[arg(long)]
        upgrade: bool,
    },
    /// Remove all registered keys and invalidate existing SSH streams.
    SshRevoke {
        id: String,
    },
    /// Inspect enrolled host public key and machine-specific identity.
    SshInfo {
        id: String,
    },
    Put {
        id: String,
        local: PathBuf,
        guest: String,
    },
    Get {
        id: String,
        guest: String,
        local: PathBuf,
    },
    Snapshot {
        id: String,
        name: String,
    },
    Snapshots,
    Fork {
        id: String,
        child: String,
        #[arg(long)]
        snapshot: Option<String>,
    },
    Hibernate {
        id: String,
    },
    /// Delete a stopped workspace with its disk and its own snapshots (local worker only).
    Delete {
        id: String,
    },
    Restore {
        id: String,
        name: String,
    },
    Publish {
        id: String,
        #[arg(long, default_value_t = 8080)]
        port: u16,
    },
    Unpublish {
        id: String,
        #[arg(long, default_value_t = 8080)]
        port: u16,
    },
    #[command(hide = true)]
    Worker,
    #[command(hide = true)]
    Supervisor,
    #[command(hide = true)]
    Gateway {
        id: String,
        port: u16,
    },
}

#[derive(Subcommand)]
enum NodeAction {
    /// Turn a Linux machine into a runner over SSH, AWS SSM or Tailscale: check
    /// KVM/Btrfs, install ow and Firecracker, write a single-use join file and start
    /// the node agent.
    Add {
        /// ssh://user@host[:port] (user@host and ~/.ssh/config aliases also work),
        /// ssm://user@i-0123456789abcdef0 (SSH through AWS Session Manager) or
        /// tailscale://user@host (SSH through `tailscale nc`).
        target: String,
        /// Runner ID; defaults to the host's first DNS label.
        #[arg(long)]
        name: Option<String>,
        /// Controller URL the runner dials; defaults to the one saved by `ow host up`.
        #[arg(long)]
        controller: Option<String>,
        /// Remote directory on Btrfs, under 50 bytes (default ~/.ow-node).
        #[arg(long)]
        remote_dir: Option<String>,
        #[arg(long, default_value_t = 2048)]
        memory: u32,
        #[arg(long, default_value_t = 2)]
        slots: u32,
        #[arg(long, default_value_t = 2)]
        cpus: u32,
        /// Also install the Ubuntu developer image (about 8 GiB transfer).
        #[arg(long)]
        ubuntu: bool,
        /// Join a tailnet first with the auth key held in a 0600 file or env variable.
        #[arg(long, value_name = "file:PATH|env:NAME")]
        tailscale_auth_key: Option<String>,
        /// Linux x86-64 ow binary to install (default: bin/ow-linux-amd64 in the
        /// asset directory, else this binary).
        #[arg(long)]
        ow_binary: Option<PathBuf>,
    },
    /// List runners and whether each is online.
    List,
}

#[derive(Subcommand)]
enum BackupAction {
    /// Consistent SQLite copy of the live catalog into a new private file.
    Catalog {
        #[arg(long)]
        output: PathBuf,
        /// Controller data directory (default: the one `ow host up` uses).
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Interim: upload sealed snapshots with `aws s3 sync` (never deletes remote copies).
    Snapshots {
        /// s3://bucket[/prefix]
        #[arg(long)]
        to: String,
        /// Runner data directory (default: the `ow host up` runner).
        #[arg(long)]
        from: Option<PathBuf>,
        /// S3-compatible store such as MinIO or R2 (https; http only on loopback).
        #[arg(long)]
        endpoint_url: Option<String>,
    },
}

#[derive(Subcommand)]
enum HostAction {
    /// Single-host mode: controller and a local runner on this machine, in the foreground.
    Up {
        /// Controller listener (default: this machine's Tailscale IPv4, else loopback).
        #[arg(long)]
        listen: Option<std::net::SocketAddr>,
        /// Local runner ID (default: hostname).
        #[arg(long)]
        node: Option<String>,
        #[arg(long, default_value_t = 2048)]
        memory: u32,
        #[arg(long, default_value_t = 2)]
        slots: u32,
        #[arg(long, default_value_t = 2)]
        cpus: u32,
    },
    /// Check Linux/KVM/storage/tools and current host demand without starting VMs.
    Doctor,
    /// Guided join. Paste a private invitation at the hidden prompt.
    Join {
        #[arg(long)]
        controller: Option<String>,
        #[arg(long)]
        invite_file: Option<PathBuf>,
        /// Additional CA for a self-hosted controller (advanced).
        #[arg(long)]
        ca_cert: Option<PathBuf>,
        #[arg(long)]
        memory: Option<u32>,
        #[arg(long)]
        slots: Option<u32>,
        #[arg(long)]
        cpus: Option<u32>,
        /// Minimum free disk space to retain (preflight threshold, not a quota).
        #[arg(long)]
        storage_gib: Option<u32>,
        #[arg(long)]
        accept_shared_pool: bool,
        /// Prepare enrollment/config only; do not start processes.
        #[arg(long)]
        no_start: bool,
        /// Opt in to the verified Ubuntu developer disk (up to 8 GiB transfer).
        #[arg(long)]
        ubuntu_dev: bool,
    },
    /// Save operator budgets only after participation and guests are fully stopped.
    Configure {
        #[arg(long)]
        memory: u32,
        #[arg(long)]
        slots: u32,
        #[arg(long)]
        cpus: u32,
        #[arg(long)]
        storage_gib: Option<u32>,
        #[arg(long)]
        accept_shared_pool: bool,
    },
    /// Refresh verified runtime assets while stopped; retain previous assets for rollback.
    UpdateAssets {
        #[arg(long)]
        ubuntu_dev: bool,
    },
    /// Select verified retained assets while fully stopped; guest disks are preserved.
    RollbackAssets {
        #[arg(long)]
        revision: String,
    },
    Start,
    Status,
    Stop,
    /// Designated owner only; requires human Access login.
    Invite {
        node: String,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value_t = 600)]
        ttl: u64,
        #[arg(long, default_value_t = 512)]
        memory: u32,
        #[arg(long, default_value_t = 2)]
        slots: u32,
        #[arg(long, default_value_t = 2)]
        cpus: u32,
    },
    /// Designated owner: edit durable upper bounds for an existing enrolled ID.
    Budget {
        node: String,
        #[arg(long)]
        memory: u32,
        #[arg(long)]
        slots: u32,
        #[arg(long)]
        cpus: u32,
    },
    Revoke {
        node: String,
    },
    #[command(hide = true)]
    Run,
}

fn assets() -> PathBuf {
    std::env::var_os("OW_ASSET_DIR")
        .map(PathBuf::from)
        .unwrap_or(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/runtime-spike"))
}

fn main_result() -> anyhow::Result<i32> {
    let cli = Cli::parse();
    #[cfg(target_os = "macos")]
    if let Action::MacHost { command } = cli.command {
        anyhow::ensure!(
            cli.server.is_none() && cli.operation_key.is_none(),
            "mac-host is an experimental local backend; omit --server and --operation-key"
        );
        return mac_host::run(cli.data_dir, command);
    }
    if let Action::Node { command } = cli.command {
        anyhow::ensure!(
            cli.server.is_none(),
            "node commands manage this machine's controller; omit --server"
        );
        #[cfg(target_os = "linux")]
        return selfhost::node(cli.data_dir, command);
        #[cfg(not(target_os = "linux"))]
        {
            drop(command);
            anyhow::bail!("ow node runs on the Linux controller machine (ow host up)");
        }
    }
    if let Action::Backup { command } = cli.command {
        anyhow::ensure!(
            cli.server.is_none(),
            "backups run on this machine; omit --server"
        );
        #[cfg(target_os = "linux")]
        return backup::run(cli.data_dir, command);
        #[cfg(not(target_os = "linux"))]
        {
            drop(command);
            anyhow::bail!("ow backup runs on the Linux controller or runner machine");
        }
    }
    if let Action::Host { command } = cli.command {
        if let HostAction::Up {
            listen,
            node,
            memory,
            slots,
            cpus,
        } = command
        {
            anyhow::ensure!(cli.server.is_none(), "host up is local; omit --server");
            #[cfg(target_os = "linux")]
            return selfhost::up(cli.data_dir, listen, node, memory, slots, cpus);
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (listen, node, memory, slots, cpus);
                anyhow::bail!("host up requires Linux x86-64 with KVM");
            }
        }
        if matches!(
            command,
            HostAction::Invite { .. } | HostAction::Revoke { .. } | HostAction::Budget { .. }
        ) {
            let selected = cli.server.or_else(|| std::env::var("OW_SERVER").ok());
            let server = match selected {
                Some(server) => Some(server),
                None => client::saved_server()?,
            }
            .ok_or_else(|| anyhow::anyhow!("owner administration requires ow login <server>"))?;
            anyhow::ensure!(
                !cli.local_mode,
                "owner host administration uses the verified human gateway"
            );
            return client::host_admin(&server, command);
        }
        anyhow::ensure!(
            cli.server.is_none(),
            "host participation is local; omit --server (invitation includes controller)"
        );
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            return onboarding::run(cli.data_dir, command);
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            anyhow::bail!(
                "Host participation requires Linux x86-64; macOS remains a remote client"
            );
        }
    }
    if let Action::Login { url } = &cli.command {
        client::login(url)?;
        return Ok(0);
    }
    let server = cli
        .server
        .clone()
        .or_else(|| std::env::var("OW_SERVER").ok());
    let internal = matches!(
        cli.command,
        Action::NodeController { .. }
            | Action::NodeJoin { .. }
            | Action::NodeRevoke { .. }
            | Action::Nodes
            | Action::NodeAgent { .. }
            | Action::Worker
            | Action::Supervisor
            | Action::Gateway { .. }
            | Action::Dashboard { .. }
            | Action::Serve { .. }
    );
    let server = if cli.local_mode || internal {
        None
    } else if server.is_some() {
        server
    } else {
        client::saved_server()?
    };
    if let Some(server) = server {
        return client::run(&server, cli.command, cli.operation_key);
    }
    // Remote commands are checked by the controller against the machine's node.
    #[cfg(target_os = "linux")]
    let (backend, capabilities) = (runtime::BACKEND, runtime::CAPABILITIES);
    #[cfg(target_os = "macos")]
    let (backend, capabilities) = (mac_worker::BACKEND, mac_worker::CAPABILITIES);
    if let Some(request) = operation(&cli.command) {
        capabilities.check(backend, &request)?;
    }
    #[cfg(target_os = "linux")]
    {
        host::run(cli)
    }
    #[cfg(target_os = "macos")]
    {
        mac_worker::run(cli)
    }
}

/// The worker operation behind a workspace command, for the capability check.
fn operation(action: &Action) -> Option<serde_json::Value> {
    let op = match action {
        Action::Fork { snapshot, .. } => {
            return Some(serde_json::json!({"op":"fork","snapshot":snapshot}));
        }
        Action::Snapshot { .. } => "snapshot",
        Action::Snapshots => "snapshots",
        Action::Restore { .. } => "restore",
        Action::Hibernate { .. } => "hibernate",
        Action::Exec { .. } => "exec",
        Action::Shell { .. } => "terminal",
        Action::Put { .. } => "put",
        Action::Get { .. } => "get",
        Action::Ssh { .. } | Action::SshProxy { .. } => "ssh",
        Action::SshConfig { .. } | Action::SshInfo { .. } => "ssh-info",
        Action::SshAuthorize { .. } | Action::SshRevoke { .. } => "ssh-keys",
        _ => return None,
    };
    Some(serde_json::json!({"op":op}))
}

fn main() {
    match main_result() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("ow: {error:#}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;
    use ow_core::{Capabilities, Capability};

    fn needed(args: &[&str]) -> Option<Capability> {
        let cli = Cli::try_parse_from([&["ow"], args].concat()).unwrap();
        operation(&cli.command).and_then(|r| Capability::required(&r))
    }

    #[test]
    fn commands_need_their_flags() {
        assert_eq!(needed(&["fork", "a", "b"]), Some(Capability::LiveFork));
        assert_eq!(
            needed(&["fork", "a", "b", "--snapshot", "s"]),
            Some(Capability::Snapshot)
        );
        assert_eq!(needed(&["snapshots"]), Some(Capability::Snapshot));
        assert_eq!(needed(&["restore", "a", "s"]), Some(Capability::Snapshot));
        assert_eq!(needed(&["hibernate", "a"]), Some(Capability::Hibernate));
        assert_eq!(needed(&["exec", "a", "true"]), Some(Capability::Exec));
        assert_eq!(needed(&["shell", "a"]), Some(Capability::Exec));
        assert_eq!(needed(&["ssh-proxy", "a"]), Some(Capability::Ssh));
        assert_eq!(needed(&["ssh-config", "a"]), Some(Capability::Ssh));
        assert_eq!(needed(&["ssh-revoke", "a"]), Some(Capability::Ssh));
        assert_eq!(needed(&["create", "a"]), None);
        assert_eq!(needed(&["list"]), None);
        let exec_only = Capabilities::of(&[Capability::Exec]);
        let request = operation(
            &Cli::try_parse_from(["ow", "hibernate", "a"])
                .unwrap()
                .command,
        );
        assert!(exec_only.check("mac", &request.unwrap()).is_err());
    }
}
