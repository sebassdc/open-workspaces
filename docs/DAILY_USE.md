# Wake on connect, snapshot store and golden images

OS phase 2 of the [roadmap](ROADMAP.md): a box sleeps when nobody uses it and
wakes when someone connects; snapshots go to S3 and come back on another runner;
images are built from recipes in a repository.

## Wake on connect

A hibernated box resumes when something reaches it: an SSH connection
(`ow ssh`, `ow ssh-proxy`, an editor's ProxyCommand), a terminal (`ow shell`), a
published port, or `ow exec`, `put`, `get` and `ssh-authorize`. The worker does
this, so every way of getting there wakes the box: plain SSH to the host, SSH
through AWS SSM, or the controller over Tailscale. The client waits for the
resume (about a second) and connects as usual; the guest's processes continue
where they were. A **stopped** box stays stopped: stop is a cold shutdown you
asked for.

The SSH stream starts after the resume, against the box's current SSH policy, so
revoked keys stay revoked.

## Idle boxes hibernate

`ow host up` and `ow node add` start their runner with automatic hibernation
after 30 idle minutes; `--idle-minutes N` changes it and `--idle-minutes 0`
turns it off. A local `ow up` reads `OW_IDLE_HIBERNATE_SECS` (unset or 0: off).

A box is idle when, for the whole timeout:

- it has no open SSH, terminal or published-port stream;
- no operation was sent to it;
- its VM used under 2% of a CPU and its network under 1 KiB/s.

So a build or a test run keeps a box awake after you disconnect, while an idle
shell left in `tmux` does not. An agent that waits quietly for a long reply can
count as idle; raise `--idle-minutes` for such boxes. Hibernation keeps RAM and
disk, so a quiet background process (a 1-second ticker uses about 0.35% of a
CPU) is paused and resumes on the next connection. TCP connections the guest had
open to the outside usually do not survive a long sleep.

Hibernating pauses the VM, writes its RAM state, then drops the paused VM before
hashing the files, so nothing the guest does is lost. Hashing an 8 GiB
developer disk and 1 GiB of RAM took about 25 s on an m8i-flex.xlarge, during
which that worker answers no other request.

A resumed hibernation capture is deleted (before, every hibernation left one
behind), and hibernation captures (`sleep-…`) are no longer listed by
`ow snapshots`.

The 24-snapshot limit counts sealed named captures only; internal sleep captures
and unpublished `.pending-*` staging directories do not consume it. A failed
capture removes its staging files; worker startup removes abandoned staging left
by an interrupted capture, while retaining sealed snapshots and hibernation
captures. The controller reconciles definitively failed snapshot reservations
from `pending` to `failed` after online inventory, and keeps uncertain outcomes
pending. Retry a failed capture with a new operation key and the same name;
retry an uncertain outcome with its original key. A ready snapshot retry returns
the existing seal even if current free space is low.


## Snapshot store

`SnapshotStore` (`ow_core::snapshot_store`) keeps sealed snapshots off the host:

```rust
pub trait SnapshotStore {
    fn list(&self) -> Result<Vec<String>>;
    fn push(&self, name: &str, directory: &Path) -> Result<()>;
    fn pull(&self, name: &str, directory: &Path) -> Result<()>;
}
```

`S3Store` implements it for S3 and S3-compatible stores (MinIO, R2) through the
AWS CLI and its usual credentials. Under `s3://bucket/prefix/NAME/` it writes
`state.zst`, `memory.zst` and `disk.ext4.zst` (zstd, so sparse disks and idle
memory shrink to their used bytes), then `manifest.json` last: a snapshot with a
stored manifest is complete. A download checks every file against the
manifest's SHA-256, writes the disk sparse, and goes through a staging
directory, so a broken or partial copy never becomes a snapshot.

```sh
ow backup snapshots --to s3://my-bucket/ow/runner-1   # upload what the store lacks
ow backup list s3://my-bucket/ow/runner-1
ow backup restore SNAPSHOT --from s3://my-bucket/ow/runner-1 [--to RUNNER_DIR] [--cold]
```

`backup snapshots` uploads every sealed snapshot the store does not have yet,
including hibernated boxes' captures, and never deletes remote copies. Use one
prefix per runner: a same-named snapshot in the store that is not this one
stops the upload instead of being skipped. `--endpoint-url` takes an https store (http only on
loopback); endpoint settings from the environment or a profile are ignored.

`backup list` prints one JSON row per stored snapshot, oldest upload first:
`snapshot`, the box it captured (`workspace`), `uploaded`, and the box's
`owner` and `machine` name. Owner and name come from the newest
`catalog-*.sqlite3` that `ow-backup` stores beside a store ending in
`/snapshots` (in its sibling `catalog/`), downloaded into a private temporary
directory and deleted after reading; they are null for any other store and for
a box the catalog does not know. The last row of a box is its newest capture.

`backup restore` downloads the snapshot onto the runner (default: the
`ow host up` runner, whose worker must be running) and brings its box up under
the same ID: RAM state resumed, or with `--cold` its disk booted fresh, which a
host with a different CPU or kernel needs. The box's ID must not exist on that
runner. A snapshot of that name already on the runner is used only if its
manifest equals the store's; a different one (another box, other data) is
refused rather than restored. Restore is runner-local: the box is not added to a controller's
catalog, so reach it with `ow --local --data-dir RUNNER_DIR`. SSH keys are kept
outside snapshots: run `ow ssh-authorize` on the restored box again.

### Managed remote restore

For an **existing catalog-owned box and a committed format-1 catalog artifact**:

```sh
ow --server https://workspaces.example.test --operation-key restore-01 \
  restore BOX --artifact ARTIFACT_ID --node TARGET_NODE
ow --server https://workspaces.example.test status
```

This returns immediately with a durable operation ID. `status.operations` reports
its transfer ID, placement epoch and current step. Reuse the same operation key
and parameters after a lost response; controller restart also resumes unfinished
operations. A different request under that key is rejected.

The source must be reachable and positively stopped or hibernated; an unmanaged
source must be stopped before it can be registered. Running boxes are refused.
The target needs the updated Linux agent and an already-upgraded protocol-2
placement-fencing worker, configured artifact store, fresh disk headroom and
reserved CPU/RAM/slot capacity. Target import cannot replace an existing disk.
After a verified pull and unstarted import, the catalog conditionally switches
placement, then activates the target. The original logical name, owner and grants
remain; remote list, exec, shell, SSH and previews use the committed epoch.
Public SSH policy is copied separately from the artifact. A guest host-key mismatch
fails closed during boot. Managed resize/delete/snapshot/fork and public-key changes
remain refused until their lifecycle coordination is implemented.

Saved RAM must match the target runtime, CPU/host-kernel identity and guest-kernel
hash. An incompatible capture stays stored and the operation fails before import.
Only an explicit new request with a **new operation key** and `--accept-cold` can
discard that RAM; the previously exported source remains fenced throughout.
No missing heartbeat or expired job lease permits starting another copy. Unknown
or contradictory outcomes retain their reservations for reconciliation. Agent
pull retries wait for the old lease to expire and use a new attempt; worker retries
keep the original key. A lost activation reply is inspected before any action.
An active but nonrunning copy requires explicit operator reconciliation.

Artifact IDs come from the trusted controller artifact catalog, never `backup list`
or an S3 manifest's owner field. This path does not create owners, discover arbitrary
objects or convert legacy `SnapshotStore` backups into catalog authority. Existing
backup publication/retention stays separate. `backup restore` remains the explicit
operator-local command and refuses managed-provenance captures; it does not adopt
anything into a controller. Source exports and durable artifacts are retained;
this slice does not delete the old source disk or enable automatic failover.

Snapshots hold guest memory and disks, including any credentials in them. Use a
private bucket with encryption at rest (default on AWS S3) and credentials that
may write only that bucket.

Managed running/hibernate recovery, incremental transfers, retention and
last-durable status: [Recovery points](RECOVERY.md). Manual backups retain their
original whole-file format; the new reader accepts both formats.

## Golden images

A recipe is a shell script whose header names its base image:

```sh
#!/bin/sh
# ow-image: base=ubuntu
set -eu
apt-get update
apt-get install -y --no-install-recommends tree htop
```

```sh
ow image build images/devtools.sh [--runner RUNNER_DIR]   # builds image "devtools"
ow image list
ow create box --image devtools --memory 1024
```

The image is named after the file. `build` creates a builder machine from the
base (`ubuntu` or `arch`; Alpine is not a base), runs the recipe as root with
network access and the usual umask, removes it, stops the builder and saves its
disk as `images/NAME` on the runner, then deletes the builder. Building the same
recipe again does nothing; a changed recipe replaces the image, and boxes
created from the old build keep their own disks. A failing recipe prints its
last 40 log lines and leaves no image and no builder. Runners advertise their
golden images to the controller, so `ow create --image NAME` through a
controller places the box on a runner that has it. Build on each runner that
should offer the image; up to 32 per runner. [`images/devtools.sh`](../images/devtools.sh)
is an example.

Recipes run as root in the builder and can do anything a guest can; review them
like code. A recipe must not copy secrets into the image: every box created from
it gets them.

## Sharing

`ow share demo --with bob@example.com [--role use|admin]` lets a teammate in;
they use `alice@example.com/demo` with `ow shell`, `ow exec`, `ow ssh` and the
dashboard's "Shared with me" section. `ow unshare demo --with bob@example.com`
takes it back and closes their open shells and SSH sessions. A teammate's fork
copies everything on the machine, credentials included, and stays theirs. Roles,
limits and the credential warning: [USERS.md](USERS.md#sharing-a-machine).

## Evidence

Observed on 2026-10-16 on a dedicated test host from `deploy/aws` (EC2
m8i-flex.xlarge, us-east-1d, Ubuntu 24.04, kernel 7.0.0-1014-aws, Firecracker
1.17.0, AWS CLI 2.38.1, the SSH-capable `ubuntu` developer image, 1 GiB boxes),
one sample each; no p50/p95 is claimed:

| Check | Result |
| --- | --- |
| SSH (strict host key) to a hibernated box, on the host | woke, counter process continued; 1.0–1.2 s connect plus command |
| SSH from a laptop through SSM and the host's `ow ssh-proxy` to a hibernated box | woke; 6.4 s including the SSM session |
| `ow exec` on a hibernated box | woke; 0.9 s |
| Idle timeout 60 s, SSH session open for 100 s | stayed running |
| After that session closed | hibernated about 60–65 s later (the list call waits for the 26 s capture, so it showed 87–91 s) |
| Busy guest (`while :; do :; done`), no connection, 90 s | stayed running |
| `ow hibernate`, 1 GiB RAM, 8 GiB disk | 26.3 s, mostly hashing |
| `ow image build images/devtools.sh` | 36 s (recipe 12 s); same recipe again 0 s; box and fork from the image have the tools; failing recipe: no image, no builder |
| Controller | lists `devtools` in the runner's images after the image appeared, runner stays online |
| `ow backup snapshots` to S3 (2 snapshots) | 56 s; again with nothing new 0.6 s |
| One snapshot: 9.1 GiB logical, 2.7 GiB allocated | 563 MiB in S3 (disk 536 MiB, memory 27 MiB) |
| `ow backup restore` onto a fresh runner | 47 s download and resume; counter continued, disk file kept, 1.8 GiB allocated |
| `--cold` | disk booted, file kept |
| Store copy with a wrong `state` | refused (`does not match its manifest`), nothing left on the runner |

Unit tests cover the idle rules, `/proc` parsing, store URLs and endpoints,
store listings, sparse hash-checked copies, manifests, upload/restore staging,
recipe headers and golden images in heartbeats (`cargo test -p ow-core -p ow`).

Not verified: a connection over a real tailnet (no auth key yet; the path ends
in the same worker operation as the SSM test), a controller-placed create from
a golden image, and restoring a catalog together with its runner's snapshots.

## Open a development server in your browser

Port forwarding works with an enrolled box and the SSH alias generated by
`ow ssh-config` (see [SSH setup](SSH.md)):

```sh
ssh -L 3000:localhost:3000 <ssh-alias>
# Keep SSH open, then open http://localhost:3000 in your browser.
# Example alias from `ow ssh-config demo`: ow-demo
```

This reaches the server's guest loopback address. It needs no publishing or
extra tailnet port; your existing SSH path may use the tailnet or SSM.

### Tailnet HTTPS previews (v0.8.0)

On a host upgraded to v0.8.0, including its dashboard and runner agents:

```sh
ow login https://host.example-net.ts.net --tailnet
# Start your HTTP dev server inside demo; bind it to 0.0.0.0:3000.
ow publish demo --port 3000
# https://host.example-net.ts.net:11000/  (use the URL printed by your host)
ow unpublish demo --port 3000
ow unpublish demo                     # remove every published port on this box
```

The dashboard's **Publish port** button does the same. Published ports show an
**Open preview** link and an **Unpublish** button; teammates see preview links
under **Shared with me**. Only the owner can change publication settings.
Existing `ow share` grants (either role) let teammates open the URL with their
own Tailscale identity. They must also be admitted to the host's dashboard
login list. Other tailnet users receive 401/403 before the guest is contacted.
Unsharing closes that teammate's preview streams; unpublishing closes all
streams on that port. Existing boxes and their processes keep running.

Each box/guest-port pair gets its own HTTPS port and browser origin, separate
from the dashboard. Paths, query strings and WebSocket subprotocols pass
through unchanged, including hot reload. A framework with hostname protection
(for example Vite's `server.allowedHosts`) must allow your host's exact
`.ts.net` hostname. Use same-origin WebSockets at the printed HTTPS port.
The server must listen on the guest interface (`0.0.0.0`), rather than only
`localhost`; use SSH forwarding above for loopback-only servers.

Previews require the host's `tailscale serve`; they never enable Funnel or
public ingress. Tailnet access rules must allow the printed HTTPS port
(allocated from 11000–65535), in addition to dashboard port 443. Host installation
provides a restricted, root-owned Serve helper and sudo rule; the dashboard
continues to listen only on loopback. Existing listeners are preserved; a
conflicting preview port is refused. Unused routes fail closed even if Serve
is temporarily unavailable during removal. Retry unpublish to remove them.

HTTP request bodies are limited to 1 MiB; responses stream from the guest.
Cookies are disabled on previews because browsers share cookies across ports
on one hostname. Use SSH forwarding for apps that require cookie sessions.
Authentication/forwarded identity headers never reach guests. Unsafe HTTP
methods and WebSockets require the preview's exact HTTPS Origin. Active
connections expire after at most eight hours; reconnect to recheck access.
Publications survive dashboard restarts. HTTPS ports are never reassigned to
a different box/port, so cached browser storage cannot cross between boxes.
Deleting a box disables its previews; an unused Serve listener may remain,
but cannot reach the guest. Local `ow --local publish` keeps its original
loopback relay behavior; Cloudflare single-app `ow serve` is unchanged.
