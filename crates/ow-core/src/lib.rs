//! Open Workspaces core: the traits the `ow` CLI, worker and controller build on.
//!
//! - [`ComputeProvider`]: a machine backend behind the worker's JSON operations.
//!   The Firecracker [`runtime::Runtime`] (Linux) is today's implementation.
//! - [`ConnectAuth`]: how the CLI reaches and authenticates to a host it manages
//!   (OpenSSH, OpenSSH through AWS SSM, OpenSSH through Tailscale).
//! - [`Capabilities`]: the flags (`snapshot`, `live-fork`, `hibernate`, `exec`,
//!   `ssh`) a provider supports, checked before each command.
//! - [`SnapshotStore`]: off-host copies of sealed snapshots (S3 and compatible).
pub mod capability;
pub mod common;
pub mod compute;
pub mod connect;
pub mod idle;
#[cfg(target_os = "linux")]
pub mod network;
#[cfg(target_os = "linux")]
pub mod runtime;
pub mod snapshot_store;
#[cfg(target_os = "linux")]
pub mod ssh_guest;
#[cfg(target_os = "linux")]
pub mod vm;

pub use capability::{Capabilities, Capability};
pub use compute::ComputeProvider;
pub use connect::ConnectAuth;
pub use snapshot_store::SnapshotStore;
