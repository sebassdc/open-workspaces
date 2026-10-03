# Bounded Linux multi-node prototype

Implemented by node-core, 2026-10-03. Pattern: existing Rust gateway, SQLite
ownership catalog, private Unix worker, Firecracker/Btrfs under ADR 0001.
Architecture decision: [ADR 0002](adr/0002-bounded-linux-multinode.md).
Independent HTTP/guest/browser acceptance and stable-commit source review passed.
The implementation was merged locally as `4a48600`. Actual local deployment,
original-catalog preservation and rollback status are recorded separately in the
[planner handoff](plans/multinode-handoff.md) and
[verification results](plans/multinode-verification-results.md).

## What works

A dedicated controller accepts outbound authenticated Linux node agents. Human
workspace requests still enter the existing Access-authenticated gateway. The
node listener has its own credentials and TLS boundary, independent of Access.
It can be self-hosted with an operator certificate/private CA and no paid node
service or vendor tunnel. The existing human gateway authentication has not been
replaced.

Selected or automatic create reserves a fixed node, physical runtime ID, RAM,
vCPUs and running slot in SQLite before dispatch. Automatic placement chooses
an online x86-64 Firecracker v1.17.0 node with the requested prepared image and
capacity; ties use node ID. New multi-node creates use the enrolled shared pool.
When there are no enrolled nodes, the old local worker remains the default.
Existing local resources keep the `local` route. Local `create --node` is rejected.

Exec, bounded upload/download, start/stop, hibernation, snapshots, restore, fork
and real guest terminals route by ownership plus durable placement. Snapshot
source, owner and node must match. Forks stay on the source node. Pending/offline
resources remain in `/api/state`; no automatic relocation occurs. Node loss or
revocation does not prove guests stopped.

All admitted human users may see and select the shared private pool's safe
selection fields (label, availability, capped capacity and capabilities). They
cannot enroll/revoke nodes. Node administration is local OS-operator CLI only.
This is an explicit shared-pool policy, not per-user private-node RBAC. Node
credentials do not identify a human owner and cannot enter human routes.

## Operator commands (prepare; planner owns deployment)

Build with the installed toolchain; use a new private data root on a
reflink-capable filesystem. Worker roots should be shorter than 60 characters:
opaque physical IDs and nested VM sockets must fit Linux's Unix socket limit.
Do not shorten IDs or bypass socket checks. Do not reuse existing prototype data
for experiments.

```bash
export RUSTUP_HOME=/home/sebassdc/dev/open-workspaces/data/runtime-spike/rustup
export CARGO_HOME=/home/sebassdc/dev/open-workspaces/data/runtime-spike/cargo
export OW_ASSET_DIR=/home/sebassdc/dev/open-workspaces/data/runtime-spike
"$CARGO_HOME/bin/cargo" +1.97.0 build --locked -p ow
OW_BIN="$PWD/target/debug/ow"
# Choose short, dedicated roots; these are examples for a new topology.
CTL=/path/to/private/controller
NODE_A=/path/to/private/a
NODE_B=/path/to/private/b
```

On each worker host, start the existing rootless worker with static admission
budgets. For two workers sharing a host these budgets must be disjoint and their
sum must include all other workers/guests plus VMM, page-cache, network helpers
and desktop reserve. Controller scheduling cannot bound other unmanaged processes.
The tested topology uses 512 MiB/two slots/two vCPUs per worker: sum 1024 MiB/four
slots/four vCPUs. No automatic physical-host grouping is claimed.

```bash
OW_MAX_MEMORY_MIB=512 OW_MAX_RUNNING=2 OW_MAX_VCPUS=2 \
  "$OW_BIN" --local --data-dir "$NODE_A" up
# On node B, use its own root and the same disjoint limits.
# Invalid, zero, over-baseline or malformed limits fail worker startup.
```

Controller and gateway use the same dedicated controller root. TLS is native to
the listener; supply a server certificate and private key. Provision certificates
through the operator's normal process, outside this task. Explicit plaintext test
mode permits loopback only, on both controller and agent.

```bash
"$OW_BIN" --local --data-dir "$CTL" node-controller \
  --listen 127.0.0.1:8790 --tls-cert /private/server.pem --tls-key /private/server.key
# Alternative for a disposable local test only:
# node-controller --listen 127.0.0.1:8790 --insecure-loopback-test
```

Mint one node-bound join file, transport it securely to the node's private
operator directory, and start the outbound agent. `--ttl` is 1–3600 seconds,
`--memory` 256–4096 MiB and `--slots` 1–8. Actual worker capabilities further
reduce controller admission. The operator's shared-host budgets remain mandatory.
The controller needs outbound-reachable TLS; no guest or worker Unix socket is
exposed. The node agent runs on the host, outside the worker network namespace.

```bash
"$OW_BIN" --local --data-dir "$CTL" node-join alpha \
  --output /private/alpha-join.json --ttl 600 --memory 512 --slots 2
"$OW_BIN" --local --data-dir "$NODE_A" node-agent \
  --controller https://controller.example:8790 \
  --credential "$NODE_A/node.json" --join "$NODE_A/alpha-join.json" \
  --ca-cert /private/controller-ca.pem
"$OW_BIN" --local --data-dir "$CTL" nodes
"$OW_BIN" --local --data-dir "$CTL" node-revoke alpha
```

Join and credential files must be owned regular files, mode 0600, in private owned
directories. Symlinks/permissive files are rejected. File publication is atomic
and does not overwrite existing files. Agents delete the join file after saving
the enrollment result; subsequent starts use the credential file. Controller
stores only SHA-256 digests of 256-bit random secrets. Redemption is transactional,
expiring and single-use. Losing the enrollment response consumes the secret;
operator recovery revokes that record and enrolls a new node ID. Identity rotation
or adopting the old node's workspaces onto a new ID is intentionally unsupported.
A stolen bearer credential can impersonate its node until revoked; this is not
host attestation or protection from a malicious host operator.

Start the human dashboard using its existing private Access config and the
controller root (`--data-dir "$CTL" dashboard --config FILE --listen LOOPBACK`).
No existing/public ingress changes are needed to prepare the slice. Any real
service replacement remains planner-owned after review.

```bash
ow --server https://workspaces.example create demo --node alpha --memory 256
ow --server https://workspaces.example create automatic --memory 256
ow --server https://workspaces.example --operation-key my-exec-key exec demo -- 'uname -a'
ow --server https://workspaces.example put demo ./hello.txt /tmp/hello.txt
ow --server https://workspaces.example get demo /tmp/hello.txt ./download.txt
ow --server https://workspaces.example shell demo
```

The dashboard's New machine dialog offers automatic/explicit placement and cards
show the node/offline state. Human terminal Origin/JWT expiry and existing PTY
limits are retained. CLI side effects print their retry key before sending and
save private intent under the client config `operations/` directory. Reuse
`--operation-key KEY` with the identical command after an uncertain response;
a new key means a deliberate new execution. Client intents contain user command
text/file bytes, so treat that private directory as sensitive and remove old
intents under the operator's retention policy. Browser pending intents use
sessionStorage and survive reload in the same tab; confirmed success removes the
key. Independent browser reload acceptance is recorded by node-verify.

## Operation and recovery contract

`/api/state` contains the caller's latest 100 operations only, with `id`, `op`,
logical `resource`, `state`, fixed `node` and `retry_key`; request bodies, host
paths and other owners' operations are not included. Typed operation errors carry
the same safe operation object. Successful replies carry `operation_id`,
`operation`, and `node`.

States are pending, succeeded, failed, uncertain and not_dispatched. Routing
failure before dispatch records not_dispatched and restores previous demand;
transport loss after possible dispatch is conservative uncertainty. Reservations
survive uncertainty/offline/restart. Explicit runtime rejection is followed by
positive inventory reconciliation before releasing demand. Already-running
resources keep demand; stopped/hibernated/failed evidence or confirmed absence
of a failed new pending resource permits release. Registered running inventory
raises reservations conservatively; unregistered running guests count as extra
RAM/vCPU/slot usage without acquiring human ownership. Static worker admission
is the final boundary, including operator bypasses between inventory refreshes.

Each owner-scoped key has an immutable normalized request/node fingerprint. Same
key with changed parameters conflicts. Resource creation/fork/capture intents
also conflict when a different key changes the original intent. A persisted node
journal commits intent before calling the Unix worker and commits results before
sending them. Lost completed replies can return the cached result without repeating
exec. Crash-pending arbitrary effects remain uncertain. Only fixed-ID
create/fork/capture may reconcile through the runtime's shape/source/artifact checks;
there is no exactly-once promise for external effects. Unknown legacy local exec
is never silently replayed. One gateway holds a lifetime catalog flock; a second
owner is refused, and only a later owner can relabel abandoned pending intent.

Latest authenticated node connection wins with a server-assigned generation.
Old heartbeat, job attachment, stream input/output and cleanup are fenced. Each
job ID is single-use, node- and generation-bound, and assigned by the controller.
Streams check identity before each direction and close on lease expiry,
replacement/revocation; idle closure is polled within three seconds and writes
are bounded at five seconds. In-flight bytes/effects already accepted cannot be
undone. Terminals never switch to a replacement stream automatically.

## Transport and resource bounds

| Boundary | Limit |
| --- | --- |
| Accepted node TCP/TLS/HTTP/WebSocket connections | 128 lifetime permits |
| TLS handshake / HTTP headers / handler body-upgrade | 10 seconds each |
| Node sessions / agent jobs | 16 each |
| Controller proxy jobs | 32; no unbounded waiting queue |
| Per-session dispatch queue | 16 job IDs |
| Control frames / enrollment body | 4096 bytes |
| Job frames / write buffers | 65536 bytes / 131072 bytes |
| JSON Unix control request/response | 1 MiB |
| Node initial job request / dispatch wait | 10 seconds |
| DNS / TCP connect / TLS+upgrade agent deadline | 5 / 5 / 10 seconds |
| Reconnect / heartbeat / offline | 2 / 3 / 15 seconds |
| Job write / guest RPC read / terminal idle | 5 / existing 120 / 90 seconds |
| Human guest file / PTY frame | 256 KiB / 4096 bytes |
| Human non-file / file request body | 16 KiB / 384 KiB |
| Runtime free storage reserve | 128 MiB; capture also reserves RAM-sized free space |

Authenticated WebSocket redirects are never followed; custom CA trust applies to
the first and only TLS handshake. No bearer secret is sent in a URL/query or
ordinary command argument. Unknown maintenance/tunnel/shutdown operations are
not admitted by the node agent. Hardware isolation remains Firecracker/KVM;
unsupported backend/architecture/protocol/runtime advertisement is refused.
Storage headroom is a local preflight, not a quota or cross-process disk reservation.

## Migration and rollback

Catalog v1/v2 upgrade to v3 in one SQLite transaction. Existing resource ownership,
logical/physical IDs and snapshot kinds are preserved; placement defaults to local.
Legacy running demand is reconstructed. First-time legacy import still requires
an explicit bootstrap owner and a live worker when runtime data exists. Missing
socket plus existing journal/disks must fail safely, not mark an empty import
complete. Runtime JSON recovery journals and guest disks are not converted.
Newer schemas are refused by both catalog and node administration.

Before deploying on existing data, stop only project gateway/controller/agents,
checkpoint SQLite and use its backup API into a private backup file; preserve the
old binary/config and runtime data. The old v2 binary cannot read v3. Rollback
requires the pre-upgrade catalog backup and old binary, with all writers stopped.
Do not restore a backup that predates newly created workspaces and then silently
adopt their disks. No destructive migration or automatic downgrade is provided.
Builder acceptance used disposable catalogs. The separately authorized local
rollout preserves the original prototype catalog after an offline backup and
copy-upgrade check; consult the deployment records above before rollback.

To stop a disposable topology, terminate only its owned agent/controller/gateway
processes, then `ow --local --data-dir DEDICATED_WORKER_ROOT down` on each worker.
Stopping an agent/controller alone leaves guests running. Check actual worker/VMM
absence independently; never use persisted PIDs to signal unrelated processes.

## Evidence and intentionally unsupported cases

Builder evidence, exact commands and private result location:
[implementation report](plans/node-core-report.md). The same-host suite passed
selected/automatic placement, TLS/private CA/hostname refusal, ownership, files,
cold persistence, two captures, independent same-node fork, actual guest PTY,
offline/reconnect, controller/catalog restart and revocation. Peak three guests,
768 MiB; both worker budgets total 1024 MiB. Fixture path failures occurred before
real guest execution and were fixed with a shorter dedicated Btrfs root.

Focused fixtures additionally cover enrollment races/expiry/replay, wrong node
credentials, unsupported/oversized frames, exhausted permits, active proxy
replacement/revoke, caller-visible uncertainty, one effect after accepted lost
reply with catalog/journal reopen, immutable placement, unregistered demand, and
start/restore rejection against positively running/stopped targets. Mock worker
results are labeled simulated evidence and do not prove guest persistence.

Independent reviewer/verifier reports live in the root repository's
`docs/plans/multinode-review.md` and `multinode-verification-results.md`. That independent pass reported all13 groups passed, including signed human
HTTP, Chromium reload/reopen, accepted lost-result faults on real guests and
paired RAM continuity. Agent crash before journal completion remains unexercised.

No second physical-host/NAT evidence; no SSH/Mac hosting/marketplace, relocation,
portable cross-node snapshot, identity rotation, off-node backup, storage quota,
CPU performance benchmark, jailer/cgroup hardening or production certification.
Existing delayed human-session revocation and trusted host-operator limitations
remain. No throughput/latency percentiles are claimed by a single test duration.
