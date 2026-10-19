# Core traits, capability flags and backups

OS phase 1 of the [roadmap](ROADMAP.md). The `ow-core` crate
(`crates/ow-core`) holds the traits the CLI, worker and controller build on.
The Firecracker runtime moved into it unchanged (`vm`, `runtime`, `network`,
`ssh_guest`, and the shared limits in `common`); the `ow` binary uses it
through the same module paths as before, so behaviour is unchanged.

The dormant [CredentialBroker](CREDENTIAL_BROKER.md) module adds exact per-box
policy, host identity/provider interfaces and fake-tested request dispatch for
temporary AWS sessions and write-only Secrets Manager uploads. It installs no
runtime endpoint; live adapters, lifecycle fencing and IAM/secret approval remain
activation gates.

The dormant [worker placement fence](PLACEMENT_FENCING.md) adds epoch-checked
lifecycle primitives and replay rules; catalog/store integration and rollout remain
explicit activation gates.

## ComputeProvider

A machine backend behind the worker's JSON operation protocol
(`{"op":"create","id":…}`, the same requests `ow` sends over the worker's
control socket and the controller forwards to nodes):

```rust
pub trait ComputeProvider {
    fn backend(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    fn call(&mut self, request: &Value) -> Result<Value>;      // no check
    fn operation(&mut self, request: &Value) -> Result<Value>; // check, then call
}
```

| Provider | Backend | Flags |
| --- | --- | --- |
| `ow_core::runtime::Runtime` (Linux) | `firecracker` | snapshot, live-fork, hibernate, exec, ssh |
| Mac worker (`crates/ow/src/mac_worker.rs`) | `apple-virtualization` | exec |

Both worker loops dispatch requests through `operation`. Terminal and SSH byte
streams stay on each provider's own socket bridge.

## Capability flags

| Flag | Commands |
| --- | --- |
| `snapshot` | `snapshot`, `snapshots`, `restore`, `fork --snapshot` |
| `live-fork` | `fork` without `--snapshot` |
| `hibernate` | `hibernate` |
| `exec` | `exec`, `shell`, `put`, `get` |
| `ssh` | `ssh`, `ssh-proxy`, `ssh-config`, `ssh-info`, `ssh-authorize`, `ssh-revoke` |

Before each workspace command on the local worker, the CLI checks the local
provider's flags and refuses early, for example
`apple-virtualization does not support snapshot (supported: exec)`. Commands
sent to a remote controller (`--server` or a saved login) are checked there,
against the node that holds the machine (its advertised operations).

## Connect providers

`ConnectAuth` is how the CLI reaches and authenticates to a host it manages.
`ow node add` takes any of them; the URL scheme picks one:

| Target | Transport | Authentication |
| --- | --- | --- |
| `ssh://user@host[:port]` (or `user@host`) | direct TCP | SSH key or agent |
| `ssm://user@i-0123456789abcdef0[:port]` | `aws ssm start-session --document-name AWS-StartSSHSession` | IAM for the tunnel, then SSH login |
| `tailscale://user@host[:port]` | `tailscale nc` over the tailnet | Tailscale SSH identity, or the host's sshd |

Every provider runs the system OpenSSH client with one shared master connection
(`ControlMaster`), so `~/.ssh/config`, agents and `known_hosts` apply; SSM and
Tailscale only set a `ProxyCommand`. Hosts, users and instance IDs are validated
before they reach a command line. Details for runners are in
[single-host mode](SINGLE_HOST.md#connect-over-aws-ssm-or-tailscale).

## Backups

Both run on the Linux machine that holds the data, as the user that owns it.
The defaults are the `ow host up` layout (`~/.ow/controller`, `~/.ow/runner`);
`--from DIR` names another data directory. On the AWS host, run them as the
`ow` user: `sudo /usr/local/bin/ow backup …`, with `--output` under `/srv/ow`.

### Catalog

```sh
ow backup catalog --output ~/catalog-$(date +%F).sqlite3
```

Copies `catalog.sqlite3` with SQLite's `VACUUM INTO` while the controller keeps
running (one read transaction, so uncommitted writes are not copied), into a
new file with mode 0600. It never overwrites a file, and checks the copy with
`PRAGMA integrity_check`. The catalog holds owner identities and credential
hashes: keep backups private.

For a controller-only host, scheduled `ow-backup` copies this catalog only;
sealed captures and recovery points are backed up by the runners. The copy
includes owner/grant/node credentials, placement epochs, transfers/reservations,
SSH policy, artifact roots/copies and leases. TLS trust/config lives outside
SQLite and must be preserved separately and privately.

Restore into an isolated, owned mode-0700 directory, with the listener,
dashboard, admission and all maintenance writers stopped. Preserve the current
directory and backup; do not overwrite the only copy. Keep the same valid
`controller/.ow-data` marker (or initialize an empty controller directory with
`ow --data-dir DIR node-list` before installing the catalog). Install the backup
as `catalog.sqlite3` mode 0600 for its service user, without stale `-wal`/`-shm`
files from another catalog. Never copy a live SQLite file with `cp`; take the
consistent `ow backup catalog` first.

Before starting writers or enabling any future GC/scaler/destructive loop:

1. Run `PRAGMA integrity_check` and `PRAGMA foreign_key_check` on the private
   copy; require `ok` and no foreign-key violations. Check `user_version` and
   `settings.min_reader_schema` against the intended binary.
2. Compare the source backup and restored copy's owners, resources, grants,
   node credential hashes/revocations/limits, placements/epochs, SSH policies,
   transfer phases/locks/reservations and epoch high-watermark. Compare all
   artifacts, current captures, image versions, artifact copies and leases;
   stored/parked captures remain roots even without a local worker or node.
   Inspect private values locally, never publish catalogs or credential hashes.
3. Account for writes after the backup. Keep maintenance disabled until roots
   and ownership across that gap are reconstructed or explicitly resolved.
   Runner inventory alone cannot recover parked boxes or authorize cleanup.
4. Positively stop/isolate the previous authority before starting a replacement.
   Keep the matching private CA, controller URL and config so existing runner
   credentials reconnect. No two writable controllers; a host-local lock is
   not a fence across hosts. After any replacement write, rollback needs the
   latest catalog with both writers stopped, never the stale pre-cutover copy.

The synthetic controller-role tests verify a private restored catalog's owner,
grant, node, transfer/reservation and artifact/copy/lease rows. They do not prove
a live cross-host controller cutover or stale-backup recovery. Those require a
separate disposable-fixture drill; no automatic failover is provided.

### Snapshots

```sh
ow backup snapshots --to s3://my-bucket/ow/runner-1
ow backup restore SNAPSHOT --from s3://my-bucket/ow/runner-1
```

Through the `SnapshotStore` trait (OS phase 2): compressed uploads of the
snapshots the store lacks, and a hash-checked restore onto another runner. See
[snapshot store](DAILY_USE.md#snapshot-store).

## Evidence

Observed on 2026-10-16:

- Unit tests: capability mapping and checks, provider dispatch, target parsing
  for all three schemes, CLI command-to-flag mapping, catalog backup (private,
  consistent under an open write transaction, never overwrites) and, since
  phase 2, snapshot store upload and restore staging. Run with `cargo test -p ow-core -p ow`.
- `scripts/connect-e2e.sh`: real OpenSSH in a Linux container through each
  provider, against sshd on loopback, with stand-in `aws` and `tailscale`
  commands that check their exact arguments and then tunnel. Quoting, stdin
  and master-connection reuse pass for `ssh://`, `ssm://` and `tailscale://`.
- In a Linux container: `ow backup catalog` from a controller with an enrolled
  runner, restored onto a fresh controller directory, which then listed the
  runner; the phase 1 `ow backup snapshots` (`aws s3 sync`, since replaced by
  the snapshot store) against a local S3 server (moto) uploaded the four files
  of a sealed snapshot byte-identical, skipped `.pending-*`, and a plain-HTTP
  non-loopback endpoint was refused.
- Inherited endpoints, with AWS CLI 2.38.1 in a Linux container: for each of
  `AWS_ENDPOINT_URL`, `AWS_ENDPOINT_URL_S3` and a profile `endpoint_url`
  pointing at a plain-HTTP store on a non-loopback address, the bare
  `aws s3 sync` uploaded to that store, while `ow backup snapshots` with the
  same setting sent it no request at all. An explicit loopback `--endpoint-url`
  still uploaded.

- Real AWS SSM, from macOS (AWS CLI 2.34.9 with the Session Manager plugin):
  the same test against a temporary SSM-only EC2 instance (Ubuntu 24.04,
  m8i-flex.large, no inbound rules, a 60-second key from EC2 Instance Connect)
  passed through `ssm://ubuntu@i-…`. The instance and its role were deleted
  afterwards.
- Real tailnet, from macOS: `tailscale://` reached the sshd of a tailnet node
  through `tailscale nc` and completed the SSH key exchange; login was refused
  because no key was authorized there, so the login step over a tailnet is not
  yet shown.

Not yet verified: `ow node add` end to end over `ssm://` or `tailscale://`
(the transport is the only change from `ssh://`, which the SSH runner acceptance tested end to
end), a Tailscale SSH login, and no guest was booted in these tests. Both
connect checks are in the morning test checklist.

Run the real-host variant with your own targets and client options:

```sh
OW_CONNECT_E2E_TARGETS=ssm://ubuntu@i-0123456789abcdef0,tailscale://me@box \
  OW_CONNECT_E2E_SSH=/path/to/ssh-wrapper \
  cargo test -p ow-core -- --ignored real_openssh_over_each_transport
```
