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

Restore onto a new controller: stop `ow host up`, then in the controller data
directory remove `catalog.sqlite3`, `catalog.sqlite3-wal` and
`catalog.sqlite3-shm`, and install the backup as `catalog.sqlite3` with mode
0600, then start the controller again. Runner credential hashes are in the
catalog, so runners enrolled before the backup should reconnect when the
controller URL and CA (`~/.ow/tls`) are unchanged; that reconnect is not tested
yet (only the restored catalog's contents are).

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
(the transport is the only change from `ssh://`, which WRK-1 tested end to
end), a Tailscale SSH login, and no guest was booted in these tests. Both
connect checks are in the morning test checklist.

Run the real-host variant with your own targets and client options:

```sh
OW_CONNECT_E2E_TARGETS=ssm://ubuntu@i-0123456789abcdef0,tailscale://me@box \
  OW_CONNECT_E2E_SSH=/path/to/ssh-wrapper \
  cargo test -p ow-core -- --ignored real_openssh_over_each_transport
```
