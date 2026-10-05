#[cfg(target_os = "linux")]
mod catalog;
mod client;
mod common;
#[cfg(target_os = "linux")]
mod dashboard;
#[cfg(target_os = "linux")]
mod host;
#[cfg(target_os = "macos")]
mod mac_host;
#[cfg(target_os = "linux")]
mod network;
#[cfg(target_os = "linux")]
mod nodes;
#[cfg(target_os = "linux")]
mod onboarding;
#[cfg(target_os = "linux")]
mod remote;
#[cfg(target_os = "linux")]
mod runtime;
#[cfg(target_os = "linux")]
mod terminal;
#[cfg(target_os = "linux")]
mod vm;
#[cfg(target_os = "linux")]
mod wire;

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
        #[arg(long, default_value = "alpine", value_parser = ["alpine", "arch", "ubuntu"])]
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
enum HostAction {
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
    if let Action::Host { command } = cli.command {
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
        #[cfg(target_os = "linux")]
        {
            return onboarding::run(cli.data_dir, command);
        }
        #[cfg(not(target_os = "linux"))]
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
    #[cfg(target_os = "linux")]
    {
        host::run(cli)
    }
    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!("Local VM runtime requires Linux; run ow login <server> to connect remotely")
    }
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
