#[cfg(target_os = "linux")]
mod backup;
#[cfg(any(target_os = "linux", test))]
mod cache_catalog;
#[cfg(target_os = "linux")]
mod catalog;
mod client;
#[cfg(target_os = "linux")]
mod dashboard;
#[cfg(any(target_os = "linux", test))]
mod discovery;
#[cfg(target_os = "linux")]
mod host;
#[cfg(target_os = "linux")]
mod image;
#[cfg(any(target_os = "linux", test))]
mod image_catalog;
#[cfg(target_os = "linux")]
mod image_store;
mod limits;
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
#[cfg(any(target_os = "linux", test))]
mod placement;
#[cfg(target_os = "linux")]
mod preview;
#[cfg(target_os = "linux")]
mod remote;
#[cfg(target_os = "linux")]
mod selfhost;
mod ssh_client;
#[cfg(target_os = "linux")]
mod ssh_gateway;
#[cfg(target_os = "linux")]
mod terminal;
#[cfg(target_os = "linux")]
mod transfers;
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
    about = "Persistent microVM workspaces: local worker and remote HTTPS/WSS client",
    after_help = "Remote first use: login URL [--tailnet] -> status -> image list -> create NAME --image ubuntu --memory 2048 --cpus 2 -> exec NAME -- COMMAND.\nCheck image list development_choice before create; omit --node for automatic placement.\nWorkspace commands: list, inspect, exec, put/get, ssh, publish/unpublish, start/stop/hibernate, snapshot/fork/restore/delete.\nMetadata is JSON; exec prints raw guest output and returns its exit status. See docs/AGENT_CLI.md.\nHost operators: host, node, backup, up/down, dashboard/serve, node-* are local/admin commands."
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
    /// Discover remote prepared images; build/manage local golden images (Linux).
    Image {
        #[command(subcommand)]
        command: ImageAction,
    },
    /// Back up the controller catalog or a runner's snapshots (Linux, this machine).
    Backup {
        #[command(subcommand)]
        command: BackupAction,
    },
    /// Sign in through Cloudflare Access (or, with --tailnet, the tailnet) and save the remote server.
    Login {
        url: String,
        /// The server uses tailnet sign-in (`tailscale serve`); no Cloudflare login.
        #[arg(long)]
        tailnet: bool,
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
    /// Host-only: revoke this controller's node enrollment.
    NodeRevoke { node: String },
    /// JSON placement resources on the selected server (or local node inventory).
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
    /// Local-only: start the prototype worker.
    Up,
    /// Local-only: shut down the prototype worker.
    Down {
        /// Hibernate running machines first, with new starts and wakes refused,
        /// so their processes survive the shutdown.
        #[arg(long)]
        hibernate: bool,
    },
    /// JSON selected server, identity, compatibility and placement discovery.
    Status,
    /// JSON usage for your workspaces.
    Stats,
    /// List your owned and shared workspaces as JSON.
    List,
    /// Inspect one workspace as JSON.
    Inspect { id: String },
    /// Create a persistent workspace; discover remote choices with `image list` first.
    Create {
        id: String,
        /// alpine, ubuntu, arch, ubuntu-arm64 (Mac), or a golden image from `ow image build`.
        #[arg(long, default_value = "alpine")]
        image: String,
        /// RAM in MiB; local prototype default is minimal. Prefer a discovered development choice.
        #[arg(long, default_value_t = 256)]
        memory: u32,
        #[arg(long, default_value_t = 1)]
        cpus: u32,
        /// Root disk size in whole GiB, e.g. 64GiB (Linux default: 32GiB).
        #[arg(long, value_parser = common::parse_disk_size)]
        disk: Option<u32>,
        /// Placement on a connected node (remote controller only).
        #[arg(long)]
        node: Option<String>,
    },
    /// Change a stopped workspace's resources; next start is a cold boot.
    Resize {
        id: String,
        #[arg(long)]
        memory: Option<u32>,
        #[arg(long)]
        cpus: Option<u32>,
        /// Grow the root disk (whole GiB); never shrinks. Applied on next start.
        #[arg(long, value_parser = common::parse_disk_size)]
        disk: Option<u32>,
    },
    /// Start or wake an existing workspace.
    Start { id: String },
    /// Stop a workspace (disk persists; running processes end).
    Stop { id: String },
    /// Run a noninteractive command; raw combined output and guest exit status.
    Exec {
        id: String,
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Open an interactive guest terminal; use exec when stdin is closed.
    Shell { id: String },
    /// Run ordinary OpenSSH into the guest (use -- before ssh options/command).
    Ssh {
        id: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Binary stdio for standard OpenSSH ProxyCommand; no diagnostics on stdout.
    SshProxy { id: String },
    /// Print reviewable OpenSSH config without changing ~/.ssh/config.
    SshConfig { id: String },
    /// Replace this machine's registered public keys; private keys stay local.
    SshAuthorize {
        id: String,
        #[arg(long, required = true)]
        key: Vec<PathBuf>,
        #[arg(long)]
        upgrade: bool,
    },
    /// Remove all registered keys and invalidate existing SSH streams.
    SshRevoke { id: String },
    /// Inspect enrolled host public key and machine-specific identity.
    SshInfo { id: String },
    /// Upload a file (remote API limit: 256 KiB; use SSH/SFTP for larger files).
    Put {
        id: String,
        local: PathBuf,
        guest: String,
    },
    /// Download a file to a new local path (remote API limit: 256 KiB).
    Get {
        id: String,
        guest: String,
        local: PathBuf,
    },
    /// Capture paired RAM and disk under an immutable snapshot name.
    Snapshot { id: String, name: String },
    /// List your snapshots as JSON.
    Snapshots,
    /// Fork a workspace's paired RAM and disk into an independent workspace.
    Fork {
        id: String,
        child: String,
        #[arg(long)]
        snapshot: Option<String>,
    },
    /// Save RAM and disk, then stop compute; start resumes the saved state.
    Hibernate { id: String },
    /// Let a teammate use one of your machines; they address it as `you@example.com/NAME`
    /// (remote server only). A teammate's fork copies everything on it, credentials included.
    Share {
        id: String,
        /// The teammate's login email.
        #[arg(long)]
        with: String,
        /// use: exec, shell, SSH, files, start, hibernate, fork. admin: also stop, snapshot,
        /// restore and SSH keys. Resize and sharing stay yours.
        #[arg(long, default_value = "use", value_parser = ["use", "admin"])]
        role: String,
    },
    /// Take a machine back from a teammate; closes their open shells and SSH sessions.
    Unshare {
        id: String,
        #[arg(long)]
        with: String,
    },
    /// Delete a stopped or hibernated workspace with its disk and its own snapshots (on a
    /// remote server: owner only; a machine the runner no longer has just leaves the list).
    Delete { id: String },
    /// Rewind a workspace to its named snapshot (later changes are replaced).
    Restore {
        id: String,
        #[arg(required_unless_present = "artifact", conflicts_with = "artifact")]
        name: Option<String>,
        /// Restore a committed catalog artifact through a durable managed transfer.
        #[arg(long, requires = "node")]
        artifact: Option<String>,
        #[arg(long, requires = "artifact")]
        node: Option<String>,
        /// Explicitly discard incompatible saved RAM; keep the stored artifact.
        #[arg(long, requires = "artifact")]
        accept_cold: bool,
    },
    /// Open a guest HTTP preview (remote availability depends on the server).
    Publish {
        id: String,
        #[arg(long, default_value_t = 8080)]
        port: u16,
    },
    /// Remove a published guest HTTP preview.
    Unpublish {
        id: String,
        #[arg(long)]
        port: Option<u16>,
    },
    #[command(hide = true)]
    Worker,
    #[command(hide = true)]
    Supervisor,
    #[command(hide = true)]
    Gateway { id: String, port: u16 },
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
        /// Pull the controller's current Ubuntu image from S3 instead of copying 8 GiB.
        #[arg(long, conflicts_with = "ubuntu")]
        from_store: bool,
        /// Join a tailnet first with the auth key held in a 0600 file or env variable.
        #[arg(long, value_name = "file:PATH|env:NAME")]
        tailscale_auth_key: Option<String>,
        /// Hibernate a box after this many idle minutes (no connection, little CPU
        /// or network); a connection wakes it. 0 turns it off.
        #[arg(long, default_value_t = 30)]
        idle_minutes: u64,
        /// Linux x86-64 ow binary to install (default: bin/ow-linux-amd64 in the
        /// asset directory, else this binary).
        #[arg(long)]
        ow_binary: Option<PathBuf>,
    },
    /// List runners and whether each is online.
    List,
}

#[derive(Subcommand)]
enum ImageAction {
    /// Build a golden image from a recipe: a shell script, run as root in a fresh
    /// machine with network access, whose header names its base, e.g.
    /// `# ow-image: base=ubuntu`. The image is named after the file
    /// (`node-dev.sh` builds `node-dev`); use it with `ow create --image node-dev`.
    Build {
        recipe: PathBuf,
        /// Runner data directory (default: the `ow host up` runner).
        #[arg(long)]
        runner: Option<PathBuf>,
        #[arg(long, default_value_t = 2048)]
        memory: u32,
        #[arg(long, default_value_t = 2)]
        cpus: u32,
        /// Give up on a recipe that runs longer than this.
        #[arg(long, default_value_t = 60)]
        timeout_minutes: u64,
        /// Publish a version in this controller's catalog after building on --node.
        #[arg(long)]
        publish_to: Option<String>,
        /// Controller runner ID (default: local); requires --publish-to.
        #[arg(long, requires = "publish_to", conflicts_with = "runner")]
        node: Option<String>,
        /// Pinned catalog base, e.g. ubuntu@2; required for controller builds.
        #[arg(long, requires = "publish_to")]
        base_image: Option<String>,
    },
    /// Publish an existing sealed base/golden image as an immutable catalog version.
    Publish {
        name: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        runner: Option<PathBuf>,
        /// Base provenance for an existing golden image; base profiles need no flag.
        #[arg(long)]
        base_image: Option<String>,
    },
    /// Pull a catalog image version onto a runner, verified by its pinned digest.
    Pull {
        image: String,
        #[arg(long)]
        runner: Option<PathBuf>,
        /// S3 root for a standalone digest-pinned pull (no local catalog required).
        #[arg(long, requires_all = ["attempt", "digest"])]
        from: Option<String>,
        #[arg(long, requires = "from")]
        attempt: Option<String>,
        #[arg(long, requires = "from")]
        digest: Option<String>,
        /// Also atomically install a built-in profile into this runtime asset directory.
        #[arg(long)]
        base_assets: Option<PathBuf>,
    },
    /// JSON remote image availability/resources, or golden images on the local runner.
    List {
        #[arg(long)]
        runner: Option<PathBuf>,
        /// List fleet-wide catalog versions rather than this runner's images.
        #[arg(long, conflicts_with = "runner")]
        catalog: bool,
    },
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
    /// Upload this runner's sealed snapshots that the store does not have yet
    /// (compressed; never deletes remote copies).
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
    /// Capture due recovery points and upload changed chunks (local host command).
    /// Schedule once a minute; running interval 30m, hibernation delay 10m.
    Recover {
        #[arg(long)]
        to: String,
        #[arg(long)]
        from: Option<PathBuf>,
        #[arg(long)]
        endpoint_url: Option<String>,
    },
    /// Download a snapshot from a store onto this runner and bring its machine up
    /// under the same ID (the runner's worker must be running).
    Restore {
        snapshot: String,
        /// s3://bucket[/prefix] the snapshot was uploaded to.
        #[arg(long)]
        from: String,
        /// Runner data directory (default: the `ow host up` runner).
        #[arg(long)]
        to: Option<PathBuf>,
        #[arg(long)]
        endpoint_url: Option<String>,
        /// Boot the snapshot's disk instead of resuming its RAM state (needed when
        /// this host's CPU or kernel differs from the one that captured it).
        #[arg(long)]
        cold: bool,
    },
    /// List the snapshots in a store.
    List {
        /// s3://bucket[/prefix]
        store: String,
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
        /// Hibernate a box after this many idle minutes (no connection, little CPU
        /// or network); a connection wakes it. 0 turns it off.
        #[arg(long, default_value_t = 30)]
        idle_minutes: u64,
        #[arg(long, default_value_t = 2048)]
        memory: u32,
        #[arg(long, default_value_t = 2)]
        slots: u32,
        #[arg(long, default_value_t = 2)]
        cpus: u32,
        /// Use the worker another service runs (`ow supervisor` in its own systemd unit)
        /// instead of starting one, so restarting this command leaves the VMs running.
        /// Idle and budget limits are then the worker's own environment.
        #[arg(long)]
        external_worker: bool,
        /// Controller listener only: no KVM check, worker or local node agent.
        #[arg(long, conflicts_with_all = ["external_worker", "node", "memory", "slots", "cpus", "idle_minutes"])]
        controller_only: bool,
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
        /// Linux CPU admission ratio: 1 for CPU-heavy jobs, 2 for dev overcommit.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=2))]
        vcpu_overcommit: u32,
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
        /// Linux CPU admission ratio: 1 for CPU-heavy jobs, 2 for dev overcommit.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=2))]
        vcpu_overcommit: u32,
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
    if let Action::Image { command } = cli.command {
        let selected = if cli.local_mode
            || (!matches!(command, ImageAction::List { catalog: false, .. })
                && cli.server.is_none())
        {
            None
        } else {
            match cli
                .server
                .clone()
                .or_else(|| std::env::var("OW_SERVER").ok())
            {
                Some(server) => Some(server),
                None => client::saved_server()?,
            }
        };
        if let Some(server) = selected {
            return client::run(&server, Action::Image { command }, cli.operation_key);
        }
        anyhow::ensure!(
            cli.server.is_none(),
            "images are built on this machine's runner; omit --server"
        );
        #[cfg(target_os = "linux")]
        return image::run(cli.data_dir, command);
        #[cfg(not(target_os = "linux"))]
        {
            drop(command);
            anyhow::bail!("ow image runs on the Linux runner machine");
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
            idle_minutes,
            external_worker,
            controller_only,
        } = command
        {
            anyhow::ensure!(cli.server.is_none(), "host up is local; omit --server");
            #[cfg(target_os = "linux")]
            return selfhost::up(
                cli.data_dir,
                listen,
                node,
                memory,
                slots,
                cpus,
                idle_minutes,
                external_worker,
                controller_only,
            );
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (
                    listen,
                    node,
                    memory,
                    slots,
                    cpus,
                    idle_minutes,
                    external_worker,
                    controller_only,
                );
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
    if let Action::Login { url, tailnet } = &cli.command {
        client::login(url, *tailnet)?;
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
            | Action::NodeAgent { .. }
            | Action::Worker
            | Action::Supervisor
            | Action::Gateway { .. }
            | Action::Dashboard { .. }
            | Action::Serve { .. }
    );
    anyhow::ensure!(
        !(internal && !cli.local_mode && server.is_some()),
        "host-only command cannot use a remote server; run it on the host with --local"
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
    anyhow::ensure!(
        !matches!(cli.command, Action::Share { .. } | Action::Unshare { .. }),
        "sharing is per user: run ow login <server> first"
    );
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
    fn preview_cli_accepts_one_or_all_unpublish_ports() {
        assert!(matches!(
            Cli::try_parse_from(["ow", "publish", "demo", "--port", "3000"])
                .unwrap()
                .command,
            Action::Publish { port: 3000, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["ow", "unpublish", "demo"])
                .unwrap()
                .command,
            Action::Unpublish { port: None, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["ow", "unpublish", "demo", "--port", "3000"])
                .unwrap()
                .command,
            Action::Unpublish {
                port: Some(3000),
                ..
            }
        ));
        assert!(Cli::try_parse_from(["ow", "publish", "demo", "--port", "65536"]).is_err());
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
