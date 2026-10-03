mod client;
mod common;
#[cfg(target_os = "linux")]
mod dashboard;
#[cfg(target_os = "linux")]
mod host;
#[cfg(target_os = "linux")]
mod network;
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
    /// Ignore a saved remote login and use the local worker.
    #[arg(long = "local", global = true, conflicts_with = "server")]
    local_mode: bool,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
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

fn assets() -> PathBuf {
    std::env::var_os("OW_ASSET_DIR")
        .map(PathBuf::from)
        .unwrap_or(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/runtime-spike"))
}

fn main_result() -> anyhow::Result<i32> {
    let cli = Cli::parse();
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
        Action::Worker
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
        return client::run(&server, cli.command);
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
