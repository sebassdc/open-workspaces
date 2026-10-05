# Mac agent: join the deployed private pool
Updated: 2026-10-05 | Size: L | State: Linux services deployed; Mac acceptance pending

## Start here
You are the Mac builder, continuing the owner's authorized PR6 integration.
PR6 is merged as `9e42263714a3c884fb745be1f31a5b595f8d90da`. The Linux controller
and dashboard now run a release built from that exact product source.
Read this file, `docs/MAC_HOST.md`, `docs/plans/mac-node-integration.md`, the
updated report/pins and ADR0004. Use a clean main checkout; preserve local work.
No further implementation or unrelated host/cloud changes are requested.

```sh
git fetch origin main
git switch main
git pull --ff-only origin main
git merge-base --is-ancestor 9e42263714a3c884fb745be1f31a5b595f8d90da HEAD
./scripts/build-mac-host.sh
./target/mac-host/ow host doctor
```

Build natively: keep `target/mac-host/ow` beside the signed `ow-vz`. The public
curl installer is not a complete native Mac host package. Use the prepared ARM64
assets from your reviewed acceptance only after matching their manifest/input
hashes to `mac-node-pins.json`; no new image download is required if they match.
Do not reuse a fixture's node root, guest disk or credentials as a live node.

## Invitation and bounded enrollment
Controller: `https://workspaces.sebashurtado.com/_nodes`
Dashboard: `https://workspaces.sebashurtado.com`
Dedicated node ID: `mac-arm64-pilot`; owner cap: 1024MiB,2vCPU,one slot.

Owner has a mode0600 one-use invitation on the Linux controller machine at
`/home/sebassdc/dev/open-workspaces/data/mac-node-rollout/mac-invite.json`.
It expires one hour after issuance. The owner must transfer this file privately
to your Mac (for example an existing trusted SSH channel or local clipboard).
Never commit, post or log its contents. If absent/expired, stop only enrollment,
report the need for a renewed file, and continue independent build/asset checks.
Do not fabricate an invitation or disable authentication. Cloudflare's public
certificate is used; no private CA override/Tailscale requirement is added.

After receiving it, set these variables to real absolute private paths. The
node directory must be fresh and assets must remain outside it:

```sh
OW_MAC_NODE="$HOME/.ow-mac-pool-pilot"
OW_MAC_PREPARED="/absolute/path/to/reviewed/prepared-assets"
OW_MAC_INVITE="/absolute/private/path/mac-invite.json"
chmod 600 "$OW_MAC_INVITE"
OW_MAC_ASSETS="$OW_MAC_PREPARED" ./target/mac-host/ow --data-dir "$OW_MAC_NODE" \
  host join --invite-file "$OW_MAC_INVITE" --accept-shared-pool \
  --memory 1024 --cpus 2 --slots 1 --storage-gib 20 --no-start
./target/mac-host/ow --data-dir "$OW_MAC_NODE" host start
./target/mac-host/ow --data-dir "$OW_MAC_NODE" host status
```

Require `controller_dispatchable:true` and a fresh matching generation, then
report the node label to the owner. The one-worker aggregate lock may reject
participation if your own old fixtures/workers are still active; positively stop
only your owned test processes and preserve disks/evidence. Never kill unrelated
processes or bypass the lock. Preserve a lost-enrollment intent and ask the
planner to revoke/reinvite a fresh node identity instead of blind retries.

## Actual acceptance through human gateway
The owner signs in with Cloudflare Access in the dashboard. The node invitation
does not provide human account access. If testing remote CLI, obtain an authorized
human session through `ow login https://workspaces.sebashurtado.com`; do not use
node credentials as human credentials. Planner's cached session had expired, so
positive live human acceptance is intentionally still pending.

Create `mac-pool-check` explicitly on `mac-arm64-pilot`, image `ubuntu-arm64`,
1024MiB/2vCPU. Verify the selected placement and reported backend/architecture.
In the dashboard terminal (or authenticated remote `ow shell mac-pool-check`):

```sh
cat /etc/os-release
uname -m
nproc
free -h
mkdir -p "$HOME/ow-test"
printf 'mac pool persistence\n' > "$HOME/ow-test/persistencia.txt"
sha256sum "$HOME/ow-test/persistencia.txt"
```

Require Ubuntu/aarch64/two CPUs; record the file hash. Test actual PTY resize and
Ctrl-C, exec exit status, and binary put/get through the gateway. Stop/start the
workspace in the dashboard and check the identical file hash. Stop/start node
participation while the workspace exists, wait for offline/online transitions,
then prove cold persistence and unchanged placement/native identity. Never move
an offline resource to Linux. Respect one real guest at a time; use at most two
retained disks and <=20GiB dedicated storage.

Verify x86 image denial, second-running-guest capacity denial, explicit unsupported
SSH/snapshot/fork/hibernate/network behavior, and owner isolation before worker
contact. Replacement/revocation active-stream tests require planner coordination:
report readiness before revoking a live enrolled node, since revocation requires
a fresh invitation/identity afterward. Prove closure by EOF/Close/reset, not timeout.
Use a dedicated authorized test identity for cross-owner checks; absence is an
unexecuted check, never permission to weaken Access or impersonate another user.

## Return and handoff
Save `docs/plans/mac-node-live-report.md` with exact tested commit and signed
CLI/helper/asset hashes, commands/results, host headroom and source maps. Keep
secrets, disks, private paths/inventory and raw receipts in ignored private storage;
provide a sanitized summary. Distinguish real dashboard/gateway checks from prior
isolated TLS tests. Report any unavailable ownership/revocation/browser check
explicitly. Do not claim official acceptance until those required checks pass.
Leave the node enrolled and participating after successful tests if the owner
wants ongoing use; preserve its credentials/root. Owner decides retention of test
workspaces. Do not push code changes or deploy Linux services from the Mac.

## Linux rollout evidence and rollback
Independent review:55Linux tests passed,5ignored;13offline and4trust tests passed.
Linux static CLI SHA256:
`623bb5c89efb1058b0bfdc31bc650dd0508307824ca667087ccf70fff1a7eac3`.
Static Linux guest SHA256:
`5876d8ca187bdace3aff5cc1bd64ed570608c2d210992dcfb88c1e4b0efc78fb`.
The Linux binaries are not native Mac/helper acceptance evidence.
Controller/gateway restarted without restarting the tunnel/workers/guests.
Origin anonymous401, public installer/compressed manifest200 and invalid public
enrollment401 observed. Existing local nodes a/b reconnected. Four existing
resource placements and Ubuntu PID/start/disk inode were preserved. Remote Linux
frame12 was offline at inspection; no claim of its current live acceptance.
Private rollback/config/process/SQLite backup and deployment records are under
Linux `data/mac-node-rollout/`; use prior binaries/config and explicit SQLite
backup only if a schema rollback is required. Do not restore SQLite over active
writers or discard operations accepted after the backup.
