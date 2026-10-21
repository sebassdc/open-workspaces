//! Open Workspaces core: the traits the `ow` CLI, worker and controller build on.
//!
//! - [`ComputeProvider`]: a machine backend behind the worker's JSON operations.
//!   The Firecracker [`runtime::Runtime`] (Linux) is today's implementation.
//! - [`ConnectAuth`]: how the CLI reaches and authenticates to a host it manages
//!   (OpenSSH, OpenSSH through AWS SSM, OpenSSH through Tailscale).
//! - [`Capabilities`]: the flags (`snapshot`, `live-fork`, `hibernate`, `exec`,
//!   `ssh`) a provider supports, checked before each command.
//! - [`SnapshotStore`]: off-host copies of sealed snapshots (S3 and compatible).
//! - [`credential_broker`]: dormant per-box role/secret policy and host provider interfaces;
//!   [`metadata`] (instance-metadata endpoint) and [`web_identity`] (identity token files)
//!   are its active per-box AWS role deliveries into guests.
#![recursion_limit = "256"]

#[cfg(target_os = "linux")]
pub mod cache;
pub mod capability;
pub mod common;
pub mod compute;
pub mod connect;
pub mod credential_broker;
#[cfg(target_os = "linux")]
pub mod disk;
#[cfg(test)]
mod fake_s3;
pub mod idle;
pub mod metadata;
#[cfg(target_os = "linux")]
pub mod network;
pub mod recovery;
#[cfg(target_os = "linux")]
pub mod runtime;
pub mod snapshot_digest;
pub mod snapshot_store;
#[cfg(target_os = "linux")]
pub mod ssh_guest;
pub mod store;
pub mod sts;
#[cfg(target_os = "linux")]
pub mod vm;
pub mod web_identity;

pub use capability::{Capabilities, Capability};
pub use compute::ComputeProvider;
pub use connect::ConnectAuth;
pub use snapshot_store::SnapshotStore;
