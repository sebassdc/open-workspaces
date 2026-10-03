# Multi-node architecture and implementation review

Updated: 2026-10-03 14:48 UTC | Reviewer: node-review
State: final stable commit and independent acceptance reviewed; bounded clearance issued.
Deployment gate: **cleared for owner-only same-host local pilot of commit
`7abe86d37305056558e5e1ebbc1ec16e8f2d3f33`**, subject to the documented dedicated
roots, disjoint worker budgets and backup/rollback procedure. Planner owns merge
and deployment. No public/physical-remote release is implied.
This document records source review and separately attributed runtime evidence.

## Prioritized recommendations

Current verdict: **no remaining blocker for this bounded owner-only same-host
pilot**. Stable committed blobs match all 31 independently accepted product
hashes; final limitations/budget/rollback documentation is reviewed.
Independent acceptance establishes active-stream closure, completed-journal
gateway-loss replay, owner-visible unknown status, browser retry keys,
start/restore rejection reservations, TLS and bounded same-host worker budgets.
Agent crash-pending recovery and physical-host/NAT operation are not established.
The priority table below states required contracts and broader follow-up checks;
it does not reopen resolved historical findings. See the final verdict appendix
for the precise scope and evidence limits.
Read the latest timestamped appendix for current status; earlier entries preserve
the observed issue and correction history rather than asserting all remain open.

| Priority | Required architecture decision/check | Acceptance requirement |
| --- | --- | --- |
| P0 | Keep human ownership and node authentication separate. | A node credential identifies exactly one enrolled node and credential generation. It cannot enter human API routes, select owners, enroll more nodes, claim arbitrary resources, or change catalog ownership. |
| P0 | Commit placement and operation intent before dispatch; preserve unknown results. | Timeout, disconnect and controller restart retain the original node/resource/operation identity and reservation. A lost node never triggers automatic relocation. Unknown exec is never silently replayed. |
| P0 | Fence duplicate connections and revoke live sessions. | One current session generation per node; old-session results, heartbeats, terminal frames and cleanup cannot affect the replacement session. Revocation is durable and terminates/rejects active traffic as well as reconnects. |
| P0 | Make enrollment atomic, expiring and single use. | Concurrent redemption yields one node/credential; expired, consumed and revoked enrollment secrets are rejected across controller restart. Enrollment grants node authority only. |
| P0 | Define the authenticated TLS transport and bounded wire contract. | Verify controller certificate/hostname; never send a node secret through a redirect or query string. Enforce frame/body/queue/concurrency/time bounds before allocation and dispatch. Public edge protection must not make node requests depend on browser cookies. |
| P0 | Pin snapshots, forks, restore and terminal sessions to durable placement. | Snapshot and source workspace must match both owner and node. A fork remains on the source node. An open terminal never switches nodes/sessions on reconnect. |
| P0 | Reserve capacity transactionally and preserve aggregate host headroom. | Simultaneous create/start/fork/resume cannot exceed configured budgets. Two local workers share one host budget, or receive static disjoint budgets whose sum leaves measured host reserve. Unknown outcomes keep reservations. |
| P1 | Scope node visibility and reconcile only node-bound resources. | Owner-facing node routes apply explicit operator/owner authorization; node A cannot report node B's inventory or overwrite metadata by guessing a physical ID. Return limited owner-visible fields; hide other users' activity and host paths. |
| P1 | Preserve existing SQLite/runtime data and local mode. | Upgrade v1/v2 in one transaction, assign existing resources to the dedicated local node without changing ownership or IDs, retain local Unix-worker access, and provide a database-compatible rollback procedure. |
| P1 | Keep existing security gaps explicit. | Remote transport is not evidence of jailer/cgroup confinement, immediate human-session revocation, storage quotas or backup durability. Keep the owner-only prototype boundary and do not claim production readiness. |

## Scope, pattern and evidence

Reviewed the root `AGENTS.md`, `README.md`, `docs/HANDOFF.md`, project planner
overlay/role, `docs/AGENT_TOOLING.md`, `docs/plans/multinode-architecture.md`,
`docs/ARCHITECTURE.md`, `docs/USERS.md`, `docs/TERMINALS.md`,
`docs/adr/0001-local-runtime-and-storage.md` and
`docs/plans/security-review-2026-10-03.md`.

Pattern: extend the existing Rust gateway + durable SQLite ownership catalog +
owner-only Unix-socket worker. Keep scheduling and node transport inside the
existing binary/modules. No separate broker, distributed database, generic
orchestration framework or new hypervisor is needed for this slice.

Source baseline: node-core at `a7333dbb96d2304b3e247c8c4b328e54b6e1d91c`.
At initial inspection its product source had no pending changes; only unrelated
instruction/overlay files were pending. Line references below are relative to
`/home/sebassdc/dev/open-workspaces-lanes/node-core/` at that baseline unless an
implementation appendix states otherwise. The same relevant root source was
read. No build, tests, VM launch, process signals, service changes, cloud writes
or paid resources were used. Existing test claims were read, not reproduced.

## Required narrow contracts

### Enrollment and credential storage

Authenticated human/operator initiation creates a bounded enrollment record:
secret digest, expiry, consumed/revoked state and intended registration scope.
Choose an explicit registration policy: in this private pool, preferably an
operator-managed node allowlist rather than treating every admitted browser
user as a node administrator. A node owner/registrant relation is not ownership
of the workspaces it hosts.

Use cryptographic random enrollment and node secrets with at least 256 bits of
entropy. Store digests at the controller, never plaintext tokens in resource
metadata or logs. Atomically consume enrollment and insert its node credential
in a SQLite transaction with a unique constraint/conditional update; check expiry
inside that transaction. Do not hold a transaction across network I/O.

Return a credential only during enrollment. Agent config must be an owned regular
file, mode 0600, under a private directory; reject symlinks or permissive existing
files and write atomically. Do not pass tokens as ordinary command-line arguments,
URLs, stdout during normal operation, or inherited guest environment. Specify
the response-loss case: if redemption commits but the agent loses the response,
the secret remains consumed; an authenticated operator revokes/re-enrolls rather
than accepting repeated redemption of a bearer secret. Rotation/revocation must
survive controller restart. A controller database backup containing digests and
node associations remains sensitive.

### Authentication, replay, revocation and fencing

Separate node endpoints from Access-authenticated human endpoints before parsing
operations. Resolve node identity from the verified credential record, never
from client-supplied `owner`, `node_id`, session ID or inventory data. A node can
receive controller-authorized requests and return correlated results/inventory
for its own placement only. Nodes have no direct ownership SQL mutation API.
Do not forward human Access assertions or enrollment/administration credentials
to workers or guests.

Document that a reusable bearer node token can be replayed if stolen until revoked;
TLS and session fencing do not provide proof of possession. Reject previously
revoked generations at every connection. Give each accepted connection a fresh
server-assigned generation and controller boot epoch; replace and close the old
session. Match responses against node, generation, operation ID and request
fingerprint. Reject unsolicited/duplicate results and old-generation messages.
An old connection's disconnect must not unregister the replacement connection.
An old heartbeat must not renew its lease. Controller restart starts nodes offline
until they authenticate again; persisted wall-clock heartbeat timestamps alone
are not an active lease. Use monotonic deadlines within a process.

Revocation must stop new dispatch immediately, close active node streams and
terminal bridges, reject late results and reconnect, and retain placed resources
as unavailable. It must not imply guest termination: guests can still run on a
partitioned or revoked host. Fencing controller connections cannot cancel an
operation already accepted by the Unix worker. Reconnect therefore reconciles
stable runtime IDs; it does not automatically resend uncertain side effects.

### TLS, ingress and bounds

Define one outbound channel (bounded polling HTTPS or WSS) and its typed versioned
envelopes. Production URLs require HTTPS/WSS with normal certificate-chain and
hostname checks. Permit plaintext only through an explicit loopback test mode
that refuses non-loopback endpoints and cannot be enabled by a server response.
Disable redirects for enrollment/authentication, or reject any redirect before
credentials can leave the configured origin. Bound token/header and label lengths.

The existing management listener is loopback HTTP behind Access. Adding a node
route there does not by itself establish an externally reachable authenticated
node channel. Specify the dedicated origin path/listener and edge gate before
deployment: any Access exception is restricted to exact node endpoints, where
origin node authentication independently denies anonymous requests. Existing
installer exceptions must not broaden to controller APIs. Changes outside the
authorized project ingress stay behind the architecture's external-resource gate.

Reuse existing message limits where suitable: Unix JSON is capped at 1 MiB,
dashboard operation bodies at 16 KiB, terminal frames at 4096 bytes, 16 terminal
slots and bounded backpressure. Add node connection, pending request, terminal,
inventory, response and aggregate queued-byte bounds; a frame limit alone is not
a memory budget. Reject deeply malformed/unknown message variants and unexpected
request IDs. Bound initial authentication, read/write, worker RPC, heartbeat and
idle time; reconnect uses bounded backoff. A stalled terminal must not block
heartbeats, revocation or other node requests. Reject worker-local maintenance
verbs (`shutdown`, arbitrary tunnel/host paths) on the remote operation surface.

### Visibility and ownership

State an explicit rule for node list/select/enroll/revoke. Do not infer node
administration rights from an email admission allowlist. For the owner-only
pilot, an explicit operator identity is enough; no RBAC framework is required.
If ordinary users can select a shared pool node, return only allowed selection
metadata and capabilities, not its token, filesystem paths, addresses, full
inventory or another user's utilization. Selection must be authorized both for
explicit node selection and automatic scheduling.

Resolve every human resource request by authenticated issuer/subject -> user ->
catalog resource -> immutable node placement before dispatch. Opaque physical IDs
are not authority. Reconcile metadata only for catalog resources registered on
the reporting node; ignore/quarantine unregistered remote artifacts. Preserve
local explicit bootstrap migration rather than automatically importing unknown
remote inventory into an account. Auto-generated hibernation/fork checkpoints
may inherit ownership only from a registered parent on that same node and a
validated artifact relation. A malicious host can lie about its own guest state;
do not describe node authentication as attestation or confidentiality from the
host operator.

### Durable placement and recovery

Persist `(owner, kind, logical_name, physical_id, node_id)` and stable operation
identity/fingerprint before issuing commands. Snapshot kind and machine kind
remain distinct. Scope inventory updates/lookups by node even if generated IDs
are globally random. Use SQLite foreign keys/uniqueness and transactions for
resource, placement, operation and capacity reservation changes.

Distinguish definitely-not-dispatched, explicit runtime rejection, confirmed
completion and unknown-after-dispatch. User cancellation/request timeout does
not cancel accepted worker activity. Once intent can have reached a worker,
leave the operation/reservation pending or unknown until same-node evidence
resolves it. Return a stable operation ID/status so refreshing the UI does not
invite a second execution. Pending resources remain visible with offline/unknown
status instead of disappearing when live inventory is absent.

Idempotency requires a stable key and normalized request fingerprint, scoped to
owner and operation, including shape, image, parent, snapshot and placement.
Same key with changed arguments must conflict. Existing create/capture IDs can
reconcile artifacts, but unknown `exec` cannot be inferred safely from inventory.
Either add a narrow durable worker-side result journal for replayable operations,
or expose unknown and require deliberate new execution. Do not claim exactly-once
external effects. Repeated fork with a different snapshot must not silently return
an existing child; compare the original operation fingerprint.

If a node disappears, retain placement, ownership, checkpoints and reservations;
reject operations requiring it. Do not choose another node even if it has free
capacity. A revoked/re-enrolled host must not accidentally orphan or adopt old
placement: define explicit credential rotation preserving identity versus a new
node record. Lost-disk recovery/migration remains a later operator-controlled
artifact restore with proof the old guest cannot write; local checkpoints are
not off-node backups.

### Snapshot and terminal identities

Snapshot references include fixed node, resource kind, physical ID and registered
source workspace. Check owner/node/source compatibility before runtime dispatch;
retain worker manifest/hash/runtime/CPU/kernel checks. Fork children reserve the
source node; reject explicit destination mismatch. Restore cannot combine a
workspace on A with a same-named checkpoint on B. Hibernation IDs also retain node
association during inventory refresh and restart.

Open terminals bind owner-authorized workspace placement to current node session
generation and a unique stream ID. Stream input, resize, acknowledgment and output
must match that binding. Disconnect/revoke/session replacement closes the bridge;
reopening checks ownership and placement again. Do not reattach an old PTY across
connections or interpret terminal input as management JSON. Keep JWT expiry,
same-origin, byte limits, slot accounting and slow-client backpressure from the
current terminal path. No cross-node snapshots, live migration or socket continuity
is implied by this slice.

### Capability, capacity and host budget

Advertise typed protocol/backend/isolation, prepared image, architecture, runtime,
snapshot compatibility and terminal/file support. Treat advertisement as input
from an authenticated but untrusted host: cap and validate numbers/strings against
controller admission policy. Refuse unsupported combinations explicitly; never
substitute containers under a hardware-isolated label. Worker admission remains
the final runtime check even after scheduler reservation.

Reserve CPU/RAM/running slots and required storage headroom before create/start/
fork/resume; include local operator workspaces and in-flight/unknown operations.
Convert reservation to observed usage without double counting. Release it only
on evidence of rejection or confirmed stop/hibernate, not heartbeat expiry.
Reconstruct reservations on restart before accepting new placement. Snapshot
capture and disk clone need free-space reserve; sparse/reflink file size is not
physical consumption. Resize/restore resource deltas need the same accounting.

For two workers on this physical machine, two independent 4096 MiB/16-vCPU budgets
would multiply the baseline limits. Use static disjoint worker budgets with a
controller aggregate cap for this test topology, or a shared host reservation
mechanism. Controller-only counting cannot cover local Unix operator bypasses
unless those are included/enforced. Record total configured guest memory plus VMM,
network helper, page-cache/snapshot/I/O and desktop reserve. Do not use measured
guest PSS alone as a safe memory ceiling. No requirement to add broad cgroups or
global host tuning within this review's scope; unresolved enforcement blocks the
relevant load/deployment claim.

## Verified baseline source findings

| Finding | Source evidence | Consequence/check needed |
| --- | --- | --- |
| Owned private SQLite catalog, WAL/FULL, bound owner lookups already exist. | `crates/ow/src/catalog.rs:26`, `:43`, `:154` | Preserve these protections during schema migration; node token must never become `Catalog::user` identity. |
| Resources have no node placement; schema version is 2. | `crates/ow/src/catalog.rs:46`, `:49` | Add durable node placement and conservative local-node backfill, preserving legacy IDs and ownership. |
| Metadata save and snapshot-parent inheritance use physical ID without a node predicate. | `crates/ow/src/catalog.rs:188`, `:218`, `:231` | Remote reconciliation must constrain updates/parent discovery to reporting node; test forged foreign IDs and unregistered inventory. |
| Gateway records intent but treats every transport/runtime error as failed. | `crates/ow/src/catalog.rs:321`, `:326`, `:328` | Classify unknown dispatch outcomes; preserve operation ID and placement rather than telling users to blindly retry. |
| State is built from live worker inventory. | `crates/ow/src/catalog.rs:245` | Offline nodes require catalog-backed rows and stale/unknown status; do not hide durable resources. |
| Catalog lock surrounds worker network calls. | `crates/ow/src/dashboard.rs:177`, `crates/ow/src/catalog.rs:326` | Avoid blocking all owners/revocation behind a stalled node; separate short catalog transactions from bounded dispatch/wait. |
| Human JWT checks precede current terminal/operation routing; terminal lookup resolves only physical ID. | `crates/ow/src/remote.rs:141`, `:283`, `:305`; `crates/ow/src/catalog.rs:353` | Keep human gate; introduce node-specific authentication surface and return a node-bound terminal target. |
| Worker is private by directory/socket modes and peer UID; per-root lock prevents a second worker on that root. | `crates/ow/src/host.rs:27`, `:63`, `:113`, `:127` | Preserve Unix boundary; node transport connects locally and cannot accept guest-selected host roots or arbitrary maintenance verbs. |
| Unix wire bounds input and uses 120-second per-I/O timeouts. | `crates/ow/src/wire.rs:10`, `:30` | Preserve bounds and add end-to-end deadline/queue limits; per-byte reads with per-read timeout are not a total RPC deadline. |
| Resource ceilings are per Runtime instance. | `crates/ow/src/runtime.rs:15`, `:324` | Two local roots need aggregate headroom enforcement and pending reservations. |
| Create retries compare shape/image/source; fork existing-child return checks parent only. | `crates/ow/src/runtime.rs:418`, `:775` | A durable controller fingerprint must include snapshot/source/shape; retry test must change the snapshot and expect conflict. |
| Runtime checks snapshot source and restore shape; baseline manifests also validate compatibility/hash. | `crates/ow/src/runtime.rs:571`, `:787`, `:818` | Add node identity to catalog routing without weakening artifact checks or claiming portability. |
| Terminal uses bounded frames/slots, worker connection and limited upstream queue. | `crates/ow/src/terminal.rs:140`, `:157`, `:165`, `:186`, `:209` | Extend the byte-stream abstraction narrowly; retain bounds and bind generation/stream to node placement. |

These are observed source properties, not demonstrated exploits or findings that
the proposed implementation already contains. Existing security review findings
about VMM confinement, cgroups/storage limits and delayed human JWT revocation
remain applicable; this architecture review did not repeat host inspection.

## Required checks before local deployment

Builder owns implementation and unit/mock transport checks; node-verify owns
real-worker/VM acceptance after independent source review. No test was run by
node-review while core designs.

1. **Enrollment:** concurrent redemption, expiry boundary, consumed token after
   restart, response loss, invalid/overlong secret, revoked/rotated credential,
   symlink/wrong owner/permissive token file; no secret in logs or guest files.
2. **Authority:** two human owners, same logical names; cross-owner node-select,
   resource/file/snapshot/fork/restore/terminal denial before worker contact;
   node token rejected on every human route and human token rejected as node
   credential. Forged owner/node IDs and node A reporting node B's physical IDs
   cannot mutate ownership/placement or expose foreign usage.
3. **Transport/session:** real certificate and hostname failure, plaintext remote
   refusal, no credential redirects, anonymous origin denial even with edge
   bypassed, oversize/malformed frames/inventory, queue exhaustion, slow reader,
   auth/write/RPC timeout and reconnect backoff. Duplicate session replacement,
   old-session disconnect/result/heartbeat races, revoke during RPC/PTY and
   controller restart must fence stale traffic and preserve unknown work.
4. **Durability:** cut connection after worker accepts create/capture/fork/exec;
   restart controller before/after SQLite commit and before/after reply. Same-node
   retry yields one guest/artifact for replayable operations; changed fingerprint
   conflicts; exec uncertainty is explicit. Offline list retains ownership and
   placement; another healthy node never receives the lost node's operation.
5. **Artifacts/PTY:** node-local snapshot/fork/restore/hibernate with same names
   on both nodes; foreign-node snapshot denied; RAM+disk pairing and corruption
   rejection remain intact. Terminals prove actual guest TTY and fixed-node
   routing, close on revoke/replacement/loss, enforce output/input/backpressure,
   and cannot replay stream IDs onto the replacement session.
6. **Capacity/headroom:** simultaneous placements with stale heartbeats,
   create/start/fork/resume/resize demand, worker rejection, unknown dispatch,
   restart and unregistered local guests. Demonstrate reservations neither leak
   after confirmed failure nor release during uncertainty. Verify disjoint budgets
   or shared host cap across two workers and free-space reserve for disk/snapshots.
7. **Compatibility/deployment:** migration from populated v1/v2 catalog and worker
   journal; safe fail on newer schema; old single-host CLI/API regression. Review
   only project process changes, exact node ingress auth, private token permissions,
   SQLite backup/checkpoint and schema-compatible rollback before deployment.
   Two workers on one host prove routing/isolation logic, not physical host/NAT
   behavior or cross-host snapshot compatibility.

## Implementation review appendix and handoff

### Initial implementation/interface inspection, 2026-10-03

Node-core began an uncommitted diff after the initial baseline inspection.
Reviewed `git diff` for CLI, remote client, host and dependencies and read its
`docs/plans/node-core-interface.md` and `docs/plans/node-core-brief.md`.
These are evolving source/interface observations, not a completed implementation
review. Planner was notified that the architecture review is ready.

| Status/priority | Verified source/interface observation | Required follow-up/test |
| --- | --- | --- |
| Positive, incomplete | `crates/ow/src/main.rs:65` introduces a dedicated node controller and explicit loopback test flag; `:73` puts join secrets in an output file, and `:83` takes agent credentials from a file. `crates/ow/Cargo.toml:19` adds TLS dependencies. | Verify actual listener/URL enforcement, certificate validation, redirection rejection and private-file checks when `nodes.rs` exists. Flags/dependencies alone prove none of these. |
| Positive | `crates/ow/src/host.rs:597` rejects local `create --node`; `crates/ow/src/client.rs:205` sends placement selection to the remote controller. | Exercise local rejection and explicit/automatic remote selection with owner authorization; reject foreign/offline nodes before dispatch. |
| P0 review gate | Interface notes propose a control WebSocket plus authenticated per-job raw-stream connections and private Unix proxies. | Bind every job to node + current control-session generation + one-time job ID, and accept only an outstanding controller-assigned job. Test node B consuming node A's job ID, duplicate job attachment, expiry, old generation and guessed unsolicited jobs. A node token alone must not authorize arbitrary proxy streams. |
| P0 review gate | Interface notes propose a durable agent journal and uncertain outcomes after agent crash. | Commit request fingerprint/intent before Unix dispatch; record completion durably before returning it; never restart/replay a pending exec. Crash between accepted worker request and journal completion must remain unknown. Verify same-ID create/fork reconciliation rejects changed input. |
| P0 review gate | CLI enrollment policy initially advertises memory/slot budgets (`crates/ow/src/main.rs:77`) but no CPU/storage/physical-host budget in those declarations. | Inspect the implementation for actual accounting of all create/start/fork/resume paths and aggregate local headroom. This is an unanswered contract, not a demonstrated over-admission bug in finished code. |

The new `mod nodes` declaration initially preceded `nodes.rs`; that file appeared
and was inspected in the following pass. Catalog integration and runtime tests
are still pending review. No build/test was attempted against this partial diff.
The independent verifier's root `docs/plans/multinode-verification-results.md`
reports legacy fixture/unit checks and a bounded future VM strategy; those are
verifier claims, not node-review executions or multi-node acceptance evidence.

No multi-node implementation is approved by this document yet. Append findings
against a stable node-core diff/commit with file/line, observed behavior, priority,
required correction and test evidence. Do not replace a pending gate with a claim
based only on a successful API call.

### First transport source review — blockers, 2026-10-03 14:02 UTC

Reviewed the newly written 340-line `crates/ow/src/nodes.rs` against unchanged
baseline HEAD `a7333db`. These references describe the working file at inspection;
later edits may move them. No exploit, deadlock or connection was executed.

| Priority | Finding and evidence | Required correction and targeted test |
| --- | --- | --- |
| P0 | **Credential forwarding on WebSocket redirect.** `nodes.rs:216` puts the bearer secret in the request, then `:228` calls `connect_with_config(..., 3)`. Installed `tungstenite-0.29.0/src/client.rs:81` clones original headers for each redirect and `:95` follows a supplied URI. The original HTTPS-only check is bypassed by redirect destinations, including another origin or plaintext WS. | Disable redirects (zero) or use an explicit single-origin handshake that rejects them. A TLS test origin returns redirects to a second origin and WS endpoint; prove neither endpoint receives the Authorization header or a connection. |
| P0 | **Opposite lock order can deadlock the controller.** `nodes.rs:90` takes jobs then sessions; proxy takes sessions then jobs at `:144`/`:146`; cleanup also nests sessions then jobs at `:131`/`:133`. | Use one consistent lock order or avoid nested locks and revalidate generation atomically. Concurrent job attach, dispatch and session cleanup must finish within a deadline. |
| P0 | **Attached streams lose generation fencing.** Job attachment checks current generation at `nodes.rs:92`, but the job WebSocket delivered at `:151` is then bridged while `:157` checks only node online/revoked status. A replacement session can make the node online again while old attached jobs/PTYs continue. | Carry the captured generation into every proxy bridge and reject/close it when session changes; notify active bridges on revoke/loss. Race reconnect against an attached slow RPC/terminal and require old bytes/results to stop while new jobs succeed. |
| P1, blocks TLS acceptance | **Custom CA is applied after an initial default-trust connection.** `nodes.rs:228` connects without the constructed connector; only on success does `:230` drop that connection and attempt one with custom roots. A private-CA controller fails before that path; a publicly trusted controller receives two authenticated connections, potentially tripping duplicate-session protection. | Make exactly one TCP/TLS handshake using the chosen root set; do not attempt default TLS first. Test private-CA success, wrong-CA/hostname denial, and one observed session connection. |
| P1, blocks bounded-transport gate | **Connect/TLS/upgrade lack a deadline.** `nodes.rs:228`/`:235` connect and handshake before `timeout_socket` is called at `:269`/`:291`. A stalled destination can permanently consume an agent thread; the 16-job cap then becomes unavailable capacity. | Set connect/read/write/handshake deadlines before TLS/HTTP upgrade, including DNS behavior. Test a TCP endpoint that accepts and never completes handshake; require bounded exit/reconnect and slot release. |
| P1 | **Session/frame limits do not bound all accepted connections.** `nodes.rs:83` upgrades authenticated duplicate sessions before detecting duplicates in `session_loop`; the 32-permit semaphore at `:138` applies only to Unix proxies. Anonymous enrollment handlers and stalled upgrades lack a visible connection/concurrency deadline. | Bound authentication/upgrade/enrollment concurrency and lifetime, and reject duplicate active sessions before upgrade where practical. Flood bounded fixture connections while verifying legitimate heartbeat, revoke and dispatch remain responsive. |

Positive source observations: atomic consumed/expiry conditional update and
credential insertion inside enrollment transaction (`nodes.rs:48`), digest-only
controller credential storage (`:51`), private regular-file checking (`:27`),
explicit TLS-or-loopback-test listener condition (`:187`), restart heartbeat reset
(`:191`), enrollment redirect refusal (`:254`), node/job/current-generation checks
before pending-job removal (`:92`), bounded frame/message queues (`:11`, `:102`,
`:138`, `:277`) and persist-before-execute agent journal (`:330`–`:338`).
These properties still require tests; they do not clear the blockers above.

Further integration checks: `nodes::db` does not enable foreign keys or check the
catalog schema version (`nodes.rs:22`); confirm it cannot initialize/mutate a newer
unsupported catalog. Heartbeat/online queries use wall-clock seconds (`:13`, `:64`),
whereas the session loop uses monotonic elapsed time; verify clock jumps cannot
produce stale availability/placement. Revocation is polled on three-second ticks
and does not mean an already accepted Unix operation was stopped. The durable
journal deliberately returns uncertainty for pending entries; create/fork recovery
is only a comment here and must be verified in catalog integration before claiming
successful retry recovery. Unknown worker errors must survive `wire::connect`,
which currently discards structured `uncertain` response fields.

### Catalog integration and transport recheck, 2026-10-03 14:07 UTC

Read current `catalog.rs` (513 lines), `dashboard.rs`, `remote.rs`, `wire.rs`,
`runtime.rs`, client/UI call sites and updated `nodes.rs`. Uncommitted work is
still evolving; the following gates describe the inspected source, not a release.

| Priority | Verified current finding | Required correction/test |
| --- | --- | --- |
| P0 | **Unknown operation identity/status is not observable to the caller.** Catalog only adds `operation_id` on success (`catalog.rs:392`); errors embed identity in operator text (`:379`, `:385`). Dashboard discards that text and returns a generic refresh/retry message (`dashboard.rs:289`). `/api/state` returns resources/nodes/stats, no operations (`catalog.rs:297`). CLI/UI do not send retry keys for exec/start/restore. Thus uncertain exec has no discoverable operation ID/key/status, and pressing retry generates a new key. | Return a safe typed error with operation ID, status, fixed node and retry key; expose owner-scoped operation status/list through state or a narrow endpoint. Persist/reuse a key in CLI/UI before sending side effects. Drop response after exec acceptance; require caller-visible uncertainty and same-key follow-up without incrementing the effect counter. Never expose raw worker paths/request contents. |
| P0 | **Routing failure after commit strands `pending`.** `catalog.rs:372` commits intent/reservation, then `:374` uses `nodes::route(...)?` before the error handling at `:377`. A node revoked/offline between preparation and routing leaves pending forever in this process; `:357` rejects all same-key retries as in progress. | Put all post-intent exits inside one status transition path. Record proven non-dispatch rejection separately from unknown after possible dispatch; finalize route failure and release only its new reservation when provably safe. Race revoke/heartbeat expiry immediately after commit and assert an observable terminal/status transition, no worker contact and no stranded pending row. |
| P0 | **Opening a catalog changes another gateway's live operations.** `catalog.rs:73` changes every pending row to uncertain without a process owner/lease/lock. Multi-process preparation is explicitly supported at `:323`, so a second gateway or test connection may open while the first is working. Same-key retry at `:359` then allows resend; the local route has no agent journal (`:375` only attaches keys to nonlocal dispatch), so local exec can run twice. | Either hold a dedicated gateway-process lock for the catalog's lifetime and refuse a second owner, or track durable process/operation ownership with safe dead-owner recovery. Merely opening a connection must not relabel live work. Test second catalog open during blocked RPC; pending stays owned/in progress, and no second dispatch occurs. Unknown local exec must never be resent automatically. |
| P0 | **Reservations omit existing running guests.** Schema adds `reserved_mib=0` (`catalog.rs:63`), legacy import leaves defaults (`:108`), and reconciliation updates metadata/release only (`:247`–`:250`). It never establishes a positive reservation from running inventory. Scheduler sums only these rows (`:306`, `:315`); unknown/unregistered local operator guests are not represented. `capacity(local)` returns immediately (`:312`). | Rebuild registered running/starting reservations conservatively and count unregistered worker usage as host/node demand without adopting ownership. Combine inventory usage and pending reservations without double counting; preserve uncertainty on stale/offline inventory. Populate legacy running fixtures and an unregistered worker guest, then attempt placement against lower budgets; require rejection before dispatch. |
| P0 | **Existing fork child placement can be rewritten.** `reserve` returns existing resource IDs (`catalog.rs:189`). Fork checks only whether a previous **fork** operation exists, then rewrites child node (`:345`–`:346`), even if that child already exists from create on another node. This can leave a live guest on B while catalog points to a new fork on A. | Placement can be set only when inserting a genuinely new resource, in the same transaction; existing target requires same node and compatible immutable source/creation intent. Create child on B, fork parent on A into the same logical child and require conflict with unchanged placement/artifacts/dispatch count. Also test reuse of pending snapshot names across nodes (`:340`). |
| P0 | **Explicit failure can release a pre-existing live reservation.** Create/fork preparation overwrites the target's reservation (`catalog.rs:366`); any non-uncertain worker error then zeros it (`:384`). A create with new key and incompatible parameters against an already-running ID is an explicit runtime rejection but does not prove that ID stopped. Local worker errors can also follow partial effects. | Preserve previous reservation and distinguish newly reserved demand from already-observed activity. Release only proven unaccepted additional demand, or confirmed no-running-runtime evidence; never zero a live resource on a mismatched create/fork rejection. Test existing running resource + changed-key/shape rejection and post-spawn metadata-write failure. |
| P1 | **Failed start/restore reservations have no complete resolution contract.** Error path releases only create/fork (`catalog.rs:384`). Reconcile may later release based on stopped/hibernated/failed inventory after state becomes failed (`:249`), but skips all pending/uncertain operations. Every node `wire::request` error is journaled uncertain, including explicit runtime rejection. | Distinguish a complete worker rejection from transport loss while allowing partial-effect errors. For start/restore, retain on unknown or post-dispatch ambiguity; release additional reservation on positive no-dispatch/rejection/no-running evidence, preserving previously running demand. Test preflight failure, runtime validation failure, dropped reply, restore after stop, and failed catalog save; inspect exact reservation/status before and after inventory refresh. |
| P1 | **Capability/CPU/storage selection and human node visibility policy remain absent.** `choose` filters online memory/slot budgets only (`catalog.rs:303`); no prepared-image/runtime/CPU compatibility is checked. State includes complete node inventory for every authenticated owner (`:297`); an explicit operator/node-visibility rule is not represented. | Document and enforce intended private-pool visibility; return only authorized selection fields. Add minimal typed capability and CPU/headroom policy, or explicitly restrict all enrolled nodes to a validated homogeneous pool and fail unsupported combinations. Test unsupported image/backend/architecture and user denied access to a private node. |

Positive integration observations: schema-v3 node placement is durable with local
defaults (`catalog.rs:62`); preparation uses a short immediate transaction and
request fingerprint (`:325`, `:353`); inventory saves and snapshot-parent owner
lookups now include node (`:234`, `:257`); state lists catalog resources even
offline (`:270`, `:280`); snapshot/fork/restore include node-equality checks
(`:340`–`:347`); `terminal_target` resolves ownership then fixed node (`:397`);
`wire::request_envelope` preserves `uncertain` fields (`wire.rs:56`). Source-level
preservation of the error envelope is resolved, but user-facing observability is
still blocked above.

#### Prior transport findings against current source

| Earlier finding | Current status and source | Remaining acceptance |
| --- | --- | --- |
| Redirect forwarding | **Resolved in current source, tests pending.** `nodes.rs:253` uses one `client_tls_with_config` handshake rather than redirect-following `connect_with_config`. | Cross-origin/plaintext redirect test confirms no second destination receives the secret. |
| Lock inversion | **Resolved in current source, tests pending.** Job takes sessions then jobs (`nodes.rs:92`); proxy (`:147`) and cleanup (`:133`) use the same order. | Concurrent attach/dispatch/disconnect deadline test. |
| Custom CA/default double connection | **Resolved in current source, tests pending.** Connector is built at `nodes.rs:222` and used for the only handshake (`:253`). | Private-CA success, wrong CA/hostname rejection and single connection counter. |
| Connect/TLS deadline | **Source correction present, tests pending.** DNS waiter is bounded at `nodes.rs:241`; TCP connect shares five-second budget (`:244`); pre-handshake timeouts and ten-second shutdown watchdog at `:248`–`:253`. | Stalled and slow-drip TCP/TLS/upgrade fixture, resolver exhaustion, permit release. |
| Active job generation | **Partially corrected; still open.** `nodes.rs:161` periodically checks generation and `:167` checks incoming node results before forwarding. Unix -> node input at `:162`–`:164` does not check generation/revocation first. A stale bridge can forward commands/PTY input between ticks, or after a long await before the next tick. | Check captured generation/revocation immediately before each outbound frame/dispatch, bind active bridge cancellation, and document bounded in-flight completion semantics. Race replacement/revoke against queued input and require no newly accepted stale command; never interpret connection closure as proof Unix side effects stopped. |
| Accepted connection bounds | **Partial source correction, open.** Session map capped at 16 (`nodes.rs:107`), agent jobs at 16, controller proxy permits at 32. | Initial HTTP/TLS/auth/upgrade/enrollment connection lifetime/concurrency remains to be verified; map size is not a pre-upgrade connection bound. |

Runtime now accepts smaller per-worker memory/running/vCPU ceilings through
`OW_MAX_MEMORY_MIB`, `OW_MAX_RUNNING`, `OW_MAX_VCPUS` (`runtime.rs:17`–`:20`,
`:328`). That is useful for disjoint same-host budgets but does not configure
their sum or account for existing/unregistered workers automatically. Invalid
values silently fall back to the larger default (`:17`); explicit deployment
budget errors should fail rather than silently expanding admission. Require
budget/config evidence and all-host reservations before VM acceptance.

Subsequent evolving-source observations during this pass:

- `nodes.rs:23` now checks schema version and `:25` enables foreign keys: the
  earlier node-DB compatibility concern is resolved in source, tests pending.
- Agent heartbeat now reports/validates protocol, backend, runtime, architecture,
  memory, slots, vCPUs and image list (`nodes.rs:124`, `:287`). Scheduling must still
  use these advertised constraints and observed unregistered demand. The stored
  heartbeat is the original JSON value, so owner-facing node metadata should use
  a typed allowlist rather than exposing arbitrary extra node-supplied fields.
- **P1 capability bug:** Alpine image detection uses `guest/rootfs.ext4`
  (`nodes.rs:286`), while the actual Alpine create source is `guest/base.ext4`
  (`runtime.rs:450`, line may move). A valid Alpine-only node will advertise no
  Alpine image once capability filtering is enabled. Test image advertisement
  against the runtime's actual prepared template filenames.
- UI dialog submissions now retain one retry key while that dialog stays open
  (`crates/ow/ui/app.js:77`). This improves dialog retry safety; exec submissions
  immediately below still have no key, and generic errors/state still omit
  unknown status. Refresh/reopen/CLI key persistence and user visibility remain
  open. Repeated dialog submission with edited parameters should conflict rather
  than run a new side effect under the old key.
- Fork admission must use the selected snapshot's shape, not only the current
  parent metadata (`catalog.rs:363`). A parent cold-resized after capture may fork
  a larger saved shape; reserve that snapshot memory/CPU demand before dispatch.

### Incremental source recheck, 2026-10-03 14:12 UTC

- **Catalog-open blanket relabeling removed in source** (`catalog.rs:71`–`:78`);
  local pending/uncertain side effects other than create/fork/snapshot now refuse
  replay (`:372`). This resolves the observed unconditional open mutation and
  local exec replay path. Multi-process operation ownership/completion still
  needs tests: remote pending retries are allowed at `:375`; unconditional result
  updates at `:400`/`:410` can race, allowing a late duplicate uncertain result to
  overwrite a succeeded operation. Use one lifetime gateway lock or monotonic
  compare-and-set status transitions; never downgrade a durable success.
- **Post-commit route failure now updates uncertain** (`catalog.rs:391`): the
  stranded-pending exit is corrected conservatively, tests pending. It retains
  reservations even when this particular attempt provably did not dispatch; that
  is safe but may need positive non-dispatch accounting for availability. Public
  observable status is still missing in `dashboard.rs:289` / `catalog.rs:298`.
- **Existing fork/snapshot placement rewrite corrected in source** through
  `new_child`/`new_snapshot` checks (`catalog.rs:347`, `:351`–`:354`). Source also
  checks snapshot parent before dispatch (`:300`, `:348`, `:350`) and compares
  previous create/fork/snapshot fingerprints independent of caller key (`:357`).
  Tests still required for existing child on another node and uncertain/pending
  artifact name reuse. Create against a pending fork child still needs scrutiny:
  create's placement selection is keyed to existence of a create operation only
  (`:338`–`:341`), rather than a genuinely new resource.
- **Legacy memory accounting partially corrected:** startup backfills registered
  running metadata (`catalog.rs:71`), but the CPU default stays zero, fresh bootstrap
  import happens later, and reconcile still does not rebuild positive demand.
  Unregistered usage is still absent from scheduler accounting. CPU reservations
  now exist and capacity checks sum them (`:321`); keep this gate open until
  legacy/unregistered/restart fixtures establish complete demand.
- **Capabilities now used for image and CPU admission** (`catalog.rs:310`, `:322`,
  `:323`), resolving the earlier entirely absent filter in source. Image/backend
  and topology acceptance remains pending; automatic selection considers requested
  CPU shape but not existing CPU sum before picking a candidate, so `capacity` can
  reject the selected node even while another has room. Static host/disjoint
  budget evidence, storage reserve and node visibility policy remain outstanding.
- **Invalid worker limits now fail at Runtime initialization** (`runtime.rs:270`)
  instead of silently admitting the default. Prior fail-open configuration concern
  is resolved in source; test invalid/zero/over-limit/non-UTF8 values and verify
  child worker inheritance. No guest was launched for this recheck.

None of these source corrections establishes tested multi-node acceptance or
deployment permission. Open P0s remain observable uncertainty, reservation
completeness/release and generation fencing on outgoing bridge data; require a
stable reviewed diff plus the targeted tests before clearing the gate.

### Incremental source recheck, 2026-10-03 14:15 UTC

- **Exclusive catalog ownership resolves concurrent-open/completion findings in
  source:** `Catalog` keeps `_lock: File` (`catalog.rs:23`), acquires nonblocking
  `catalog-gateway.lock` before opening/migrating (`:34`–`:36`), then relabels pending
  only after that exclusion (`:83`–`:84`). Verify a second process refuses without
  mutating pending rows, and lock release/reopen after crash preserves uncertainty.
  The comment about supporting multiple gateways should now match the single-owner
  contract; no distributed lease system is required.
- **Uncertainty now reaches API callers in source:** owner-scoped `operation_info`
  includes ID/state/node/retry key (`catalog.rs:251`); state lists the latest 100
  owned operations (`:319`–`:322`); post-dispatch errors return typed
  `OperationFailure`; dashboard exposes the safe operation object (`dashboard.rs:289`).
  This resolves the generic-response-only API path, tests pending. CLI/UI still
  need to actually display that object and avoid hiding it behind their generic
  error strings. Exec has no persisted request key in the reviewed client/UI.
- **Generation checks now cover outgoing bytes in source** (`nodes.rs:172`) as
  well as incoming results (`:176`). Earlier missing outgoing-frame check is
  resolved at this level; race tests and explicit bounded in-flight semantics
  remain required. Ordinary connection fencing does not stop accepted Unix work.
- **Alpine template advertisement corrected in source** (`nodes.rs:292` uses
  `guest/base.ext4`), tests pending. Secrets now use a dedicated 32-byte random
  generator and credential writing checks the private parent before staging a
  file (`nodes.rs:14`, `:34`); inspect atomic publication and failure behavior in
  the final pass.
- **Reservation reconstruction improves in source:** after bootstrap startup
  raises running registered memory/CPU demand (`catalog.rs:132`); reconcile also
  raises both from running inventory (`:269`). This covers registered legacy rows
  more fully, but unregistered worker demand and aggregate host budget remain
  open. Selected snapshot shape is now used for fork admission (`:402`), tests
  pending, including parent cold resize after snapshot capture.
- **P1, blocks snapshot regression:** artifact fingerprint lookup incorrectly
  keys snapshots by the source machine's logical name. `catalog.rs:382` uses the
  machine name for `snapshot`, and `:411` stores the machine name as operation
  resource. After one successful capture, a second differently named snapshot on
  the same machine conflicts with the first request at `:385`. Key immutable
  artifact intent by the snapshot's own identity/name (retaining source linkage)
  and test two different captures on one workspace, repeated same-name capture,
  changed source and changed retry key.

Post-commit route failure now becomes `not_dispatched` only from `pending`
(`catalog.rs:416`) and returns typed status. Ensure a subsequent failed routing
attempt against an already-uncertain operation does not claim that its original
attempt never reached the worker. Preserve demand until positive evidence
resolves that original attempt.

### Incremental source recheck, 2026-10-03 14:19 UTC

Source corrections, regression evidence pending:

- `catalog.rs:267`–`:283` now records unregistered running memory/CPU/slot demand
  in `node_usage` without granting ownership; `extra_usage` (`:336`) is included
  by automatic and explicit admission (`:347`, `:359`). Operation preparation
  refreshes inventory before the transaction (`:368`). This addresses the missing
  unregistered-demand source path; test stale/offline inventory, dynamic local
  operator changes and no double counting. The worker's configured disjoint budget
  remains the final local bypass constraint; no aggregate host configuration has
  yet been independently reviewed.
- The unconditional create/fork error release is removed. Error path updates status
  then reconciles positive inventory (`catalog.rs:443`–`:447`); existing live demand
  is rebuilt from running inventory. Absent pending machine reservations release
  only if a failed operation exists and no pending/uncertain one exists (`:279`–`:282`).
  Verify start/restore rejection, partial effects, absent new create and existing
  guest mismatched create/fork before claiming the reservation gate closed.
- Pending create placement now checks for both create and fork intent (`:382`),
  and an existing fork target must have fork intent (`:396`). Distinct snapshot
  fingerprint/storage keys now use the snapshot's logical name (`:401`, `:430`).
  These source changes resolve the observed pending fork-child relocation and
  second-capture conflict, tests pending.
- CLI now creates a private operation-intent file and prints its key before sending
  (`client.rs:112`), accepts `--operation-key` (`main.rs:38`) and includes safe
  operation details in error output (`client.rs:144`). UI displays typed failures
  (`crates/ow/ui/app.js:14`) and stores an intent-to-key map in `sessionStorage`
  before send (`:71`). These address caller-visible uncertainty and exec retry
  identity in source. Verify with actual dropped responses and page reload, not
  only direct catalog tests.

Additional cases needed before stable review:

1. **P1 dialog reload retry:** dialog submit still preassigns a new key
   (`crates/ow/ui/app.js:78`); `operate` retains that key rather than consulting
   the saved key. Reopening a failed restore dialog after refresh can issue a
   second restore under a new key. Compute a canonical intent excluding the
   retry key, consult storage first, and retain it until confirmed completion or
   a deliberate new action. Test unknown restore -> reload/reopen -> same intent.
2. **P1 denial ordering:** `catalog.rs:368` reconciles every online node before
   `physical(user,...)` checks at `:373`. A cross-owner/physical-ID denied operation
   therefore makes inventory worker calls before SQL authorization. Validate
   ownership/referenced-artifact scope before refreshing capacity, consistent
   with `docs/USERS.md`; test denied calls with separate inventory/operation
   dispatch counters and state precisely which must remain zero.
3. **Legacy offline bootstrap:** open substitutes empty inventory when no Unix
   socket exists and still marks legacy import complete (`catalog.rs` initial
   import block). If an existing runtime journal/disk set exists without a catalog
   and the worker is stopped, those guests can be permanently skipped by import.
   Distinguish a genuinely empty controller root from unavailable legacy inventory;
   defer/fail import when local artifacts exist. Test stopped legacy worker with
   existing `state.json`, then restart/login and verify ownership is preserved.

Resource-result correlation is now checked before metadata writes
(`catalog.rs:450`), so a node cannot return a different machine/snapshot result
to overwrite another placed row via that path. Keep tests for forged inventory,
foreign node IDs, wrong source and result body kind. Exec/files are correlated
by the one-time node/job stream rather than a returned resource field.

### Test-source and listener-bounds review, 2026-10-03 14:22 UTC

Read the new `catalog_tests.rs` and `node_tests.rs` without running them. Existing
source corrections now have focused fixtures, but a test name is not evidence of
all behaviors in its title. Builder/verifier execution results are still needed.

| Observed fixture coverage | Exact source | Evidence still needed |
| --- | --- | --- |
| Concurrent one-use enrollment, expiry, credential node scope, revoked auth, private-file/symlink rejection, origin policy and newer-schema refusal | `node_tests.rs:10` | Real HTTP enrollment response loss, consumed/revoked behavior after process restart; credential permissions through actual agent CLI. |
| Redirect refusal and stalled plain HTTP upgrade | `node_tests.rs:22` | Actual TLS private-CA/hostname failures and slow-drip watchdog behavior, as opposed to plaintext fixtures. |
| Completed journal replay, changed-command fingerprint denial and preinserted pending exec entry | `node_tests.rs:31` | Kill/crash agent between accepted Unix effect and durable completion, then reconnect; no duplicate external effects and explicit uncertainty. |
| Node auth mismatch, guessed/wrong-node/duplicate job attachment, stale unattached job generation and control-channel revocation | `node_tests.rs:40` | **Active proxy/PTY fencing is not covered:** test inserts a oneshot job (`:52`), opens/closes it (`:54`), then replaces/revokes control session. It never exercises `proxy` forwarding on a still-attached stream. Add active inbound/outbound frame and queued-input tests. Oversize/queue/slot exhaustion is also absent despite the test's `bounds` suffix. |
| Owned placement, unchanged-key cached result, changed shape, two snapshots, cross-node artifact denial, second catalog owner refusal, offline persistence | `catalog_tests.rs:27` | Real HTTP/dashboard error object and owner-scoped operation list, process-level lock/restart, actual worker journal and live artifact behavior. Fork denial comment at `:36` allows inventory calls without separating mutation counters; add explicit no-fork counter. |
| Missing proxy creates uncertain status while preserving fixed placement/reservation | `catalog_tests.rs:43` | **Not a lost successful result:** no worker accepted create in this fixture. Add response loss after worker effect and during controller/agent restart, with exact runtime disk/guest count. |
| Unregistered RAM/CPU demand rejects placement | `catalog_tests.rs:49` | Isolate CPU-only, slot-only and RAM-only boundaries; registered legacy demand, start/restore failure and reservation retention/release; simultaneous admission and configured same-host aggregate headroom. |

Source now prechecks owned machine/snapshot scope before reconciliation
(`catalog.rs:368`–`:372`), resolving the earlier denial ordering for non-create
operations in source; regression fixture checks zero worker calls for denied
owner and cross-node restore. Existing fork-child intent denial after authorized
parent inventory is a narrower case and must prove no mutation dispatch.

Listener connection bounds now have a concrete source implementation:
`BoundedAccept` acquires a permit before its inner accept (`nodes.rs:216`),
`BoundedIo` retains it for the connection (`:200`), controller allocates 128
permits (`:238`), HTTP/1 headers have a ten-second limit/16 KiB parser buffer
(`:243`, `:247`), and router handlers have a ten-second timeout (`:224`). Installed
`axum-server-0.8` RustlsAcceptor sets a ten-second production handshake timeout
(`src/tls_rustls/mod.rs:126`). This corrects the earlier absent-bound source
finding. Test exhaustion/slow headers/TLS and upgraded permit lifetime, including
that legitimate control/revocation remains responsive; mock router tests use
plain `axum::serve` and do not exercise this acceptor.

No source test currently establishes safe stopped/failed start/restore reservation
resolution, offline legacy bootstrap, full browser retry persistence or actual
two-worker guests. Those acceptance gates remain open at this timestamp.

### Incremental source recheck, 2026-10-03 14:26 UTC

The remaining three source cases from the 14:19 pass are corrected, tests pending:

- Dialog submit no longer preassigns a new retry key (`crates/ow/ui/app.js:78`),
  and `operate` excludes the key from the stored intent (`:72`). Same parameters
  can now recover their key from `sessionStorage` after reopen/reload. Confirm
  actual page reload preserves an unknown restore/exec key and displayed status.
  JSON property order must be stable across those entry paths or normalized.
- Non-create ownership and snapshot node/source checks precede inventory refresh
  (`catalog.rs:368`–`:372`), with fixture assertions against wrong owner and
  cross-node restore worker-call counts (`catalog_tests.rs:31`, `:37`).
- When legacy runtime artifacts exist without `control.sock`, catalog startup
  refuses to finish import (`catalog.rs:99`–`:100`); new fixture
  `absent_legacy_worker_never_completes_import` at `catalog_tests.rs:54` targets
  this case. Inspect execution result and recovery/login after the worker returns.

Deployment remains uncleared while stable code/test evidence and configured
headroom are outstanding. No reviewer tests, VM activity or service mutation
occurred during any pass.

### Independent acceptance harness source review, 2026-10-03 14:29 UTC

Read root `experiments/runtime-spike/multinode-test.py` (verifier-owned); no edits
or execution. It uses dedicated ignored roots, a temporary TLS CA, real outbound
agents/workers, and synthetic RS256 human identities routed through a gateway-only
fixture. It does not claim a real Cloudflare owner login or another physical host.

Budget design is explicit: each worker inherits 512 MiB/two guests/two vCPUs
(`:189`), `worker_caps` checks both reported limits and process inheritance
(`:512`), and `/proc` preflight accounts existing guests before launches (`:57`).
The planned live workload is small and serial; local fork stays on its source
node. This is a suitable narrow real-worker harness once its negative-test
assertions are corrected and exclusive test-turn coordination is honored.

Required harness corrections/evidence:

- **P1 false-positive closure:** session/terminal fencing catches any `OSError`
  as proof of closure (`:714`, `:734`), but `socket.timeout`/`TimeoutError` is also
  an `OSError`. A stream remaining open but silent can therefore pass. Treat
  read timeout as still-open until the deadline, and require EOF, a WebSocket
  Close frame, reset or another established connection-closed error. Include a
  deliberately silent/open fixture that must fail the closure assertion.
- **P1 budget undercount:** host-budget VM inspection catches any
  `FileNotFoundError` as an exited process (`:90`), including a still-running
  Firecracker whose config file disappeared. Confirm the `/proc` process really
  exited before ignoring it; missing/unreadable live configuration is unresolved
  demand and must prohibit guest creation. Test missing-config live-process
  classification without booting additional guests.
- Lost-response tests (`:654`) kill the gateway after the **real agent journal
  entry** exists, then await a durable completed response before retry. They can
  establish successful effect/result replay and stable placement without claiming
  recovery from an agent crash before journal completion. Pending-journal crash
  remains explicitly uncertain/operator reconciliation and requires its own
  safety check. The create fault must actually interrupt before a response;
  its `future.done()` assertion prevents labeling a missed fault as a pass.
- Revocation exercises an active guest PTY (`:721`) and durable credential denial,
  complementing node unit tests. Duplicate-session portion still tests control
  channels separately from an active replacement PTY; require a live-stream
  replacement case and queued stale input, or state that limitation explicitly.
- Harness covers lifecycle/exec/files, two owner identities, cold persistence,
  paired snapshot/fork RAM state, offline/reconnect and TLS. Start/restore failure
  reservation transitions, post-intent non-dispatch fault, forged foreign-node
  inventory, byte/queue exhaustion and browser reload retry remain separate
  required focused checks.

No acceptance result is asserted here; the harness is still being prepared and
the review gate remains open. Only `docs/plans/multinode-review.md` was written by
node-review.

### Builder test expansion reviewed, 2026-10-03 14:32 UTC

Read new active proxy and oversize tests plus `node_e2e.rs`. Builder terminal output
reported `two_worker_tls_real_guests` passed (one test, 21.40 seconds); this is
observed builder output, not a test run by node-review. Final command/result path
and stable commit must be recorded before treating it as reproducible evidence.

- `node_tests.rs:64` now attaches a real proxy byte stream, replaces generation or
  revokes node, attempts stale input/output and requires proxy termination. It
  also checks exhausted proxy permits. This directly addresses the earlier
  active-stream fixture gap; await recorded passing result for the final code.
- `node_tests.rs:85` now checks enrollment over the 4096-byte cap and malformed/
  oversized control messages. Header/TLS acceptor exhaustion, aggregate queued
  bytes and slow-client backpressure still need the stated limits and focused
  acceptance coverage; no generic fuzzing framework is required here.
- `node_e2e.rs:13` boots real workers/guests over private-CA TLS, tests selected/
  automatic placement, changed fork fingerprint, files, cold persistence, two
  captures, fork disk independence, actual guest `/dev/pts` output, disconnect,
  reconnect, controller/catalog restart and revoke. Assertions are source-reviewed.
  It uses direct Catalog identities rather than the human HTTP dashboard gate;
  paired RAM process continuity and lost accepted results remain verifier checks.
- Fixture commands set per-worker 512 MiB/two slots/two vCPUs (`node_e2e.rs:7`)
  and require no existing Firecracker processes (`:15`). Its planned peak is three
  256 MiB guests, configured guest-memory sum 1024 MiB. This is bounded same-host
  validation, not physical-host/NAT acceptance. Only alpha's reported memory cap
  is asserted (`:21`); verifier's inherited/reported checks for both workers give
  stronger budget evidence.
- Cleanup Drop kills only owned controller/agent children and invokes `down` for
  dedicated workers (`node_e2e.rs:5`), but ignores cleanup return status. The
  `result.json` cleanup text (`:53`) alone cannot prove zero guests; require the
  independent post-test host preflight/worker absence observation. Planner
  coordination reported zero remaining host guests and handed the exclusive VM
  test turn to node-verify; node-review did not inspect or signal VM processes.

The physical/socket-path failure preceding this pass was a fixture path-length
failure, not a proven runtime isolation defect. Core used a shorter dedicated
test root rather than shortening persistent physical IDs. Final evidence should
record the successful root configuration and keep private artifacts ignored.

Next action: recheck node-core's corrections against a stable implementation diff
and builder/verifier test evidence, then communicate the deployment verdict to the planner before
node-verify starts deployment. Open decision: none; scope already authorized.
Only this review file is writable by node-review. Shared task board, product
code, unrelated pending files and Git commits remain untouched.

### Recorded core acceptance and focused additions, 2026-10-03 14:36 UTC

Read the owner-provided `data/ncb5ca6775/result.json`: `passed=true`, same-host
two-worker TLS topology, three guests/768 MiB peak, disjoint configured worker
memory budgets totaling 1024 MiB. Its check list includes private-CA/hostname
denial, enrollment, placement, duplicate create, ownership denial, guest
exec/files, cold persistence, two captures, fork independence, changed fork
fingerprint, actual PTY, offline/reconnect, controller/catalog restart and
revocation. These are builder acceptance evidence; the reviewer did not run
tests or interfere with processes. Planner separately confirmed cleanup.
This artifact does not establish all unknown-result, reservation-failure or
active-stream bounds checks, and is not a deployment release.

Current formatted `catalog_tests.rs:245` adds explicit start/restore rejection
checks: a running resource retains 256 MiB/one vCPU after either rejection;
after positively observed stop, either rejection leaves zero reservations.
This directly tests the earlier rejected-operation release issue. It does not
inject a partial side effect followed by an unavailable inventory or transport
loss; unknown-outcome retention remains a separate acceptance requirement.

Verifier harness corrections are now source-observed: `multinode-test.py:179`
waits through timeouts and accepts only EOF/Close/reset, while `:93` refuses
missing configuration for a still-live process. These resolve the earlier
false-positive assertion and host-demand undercount in source. Require recorded
negative fixtures and the final independent acceptance result before relying on
these assertions as verified evidence. Final core source revision, docs and
stable test output remain pending.

### Frozen-source reference map, 2026-10-03 14:40 UTC

Planner instructed core to freeze product source while finishing documentation,
and verifier to hash/rebuild that tree before independent acceptance. Source
corrections reviewed at current formatted lines:

| Earlier finding | Current correction/reference | Evidence still required |
| --- | --- | --- |
| Credential forwarded through WebSocket redirect; double custom-CA handshake; connect deadline missing | `nodes.rs:644` chooses bounded DNS/connect/TLS single handshake, `:738` disables enrollment redirects. | Stable passing redirect, trust, hostname and timeout cases. |
| Active attached proxy survived generation replacement/revoke | `nodes.rs:380` checks online/current generation periodically and before traffic in both directions; `node_tests.rs:188` exercises stale bytes and shutdown. | Independent positive EOF/Close/reset on a live guest stream, plus recorded focused unit pass. |
| Admission did not bound initial TLS/HTTP connections | `nodes.rs:467` holds a connection permit through I/O; `:552` configures 128 connections and 10-second/16-KiB header bounds. | Production acceptor exhaustion/slow-header checks remain distinct from mock router tests. |
| Unknown operation identity hidden by generic dashboard error | `dashboard.rs:328` emits owner-scoped OperationFailure; `catalog.rs:300` supplies identity/state/key; state contains owner operations; `app.js:72` saves stable intent keys. | HTTP owner-only unknown status and actual browser lost-reply/reload/reopen tests. |
| Post-intent route failure stranded pending state | `catalog.rs:865` marks only pending intent not_dispatched and restores prior demand; uncertain intent is not downgraded. | Focused route failure after COMMIT, including an already-uncertain retry. |
| Opening second gateway invalidated live pending operations | `catalog.rs:42` takes lifetime nonblocking exclusive lock before `:103` converts prior pending effects to uncertain. | Second-owner refusal and restart evidence against final source. |
| Legacy/unregistered demand omitted | `catalog.rs:334` adds unregistered running demand and `:343` raises registered reservations before admission; startup backfill retains existing running demand. | Recorded legacy/unregistered RAM/CPU/slot admission checks; actual two-worker disjoint budget proof. |
| Runtime rejection blindly released demand | `catalog.rs:910` reconciles positive inventory; `:350` prohibits release while relevant pending/uncertain operations exist; `catalog_tests.rs:245` distinguishes running and stopped rejection. | Independent disposable-artifact failure checks and unknown transport retention. |

Current heartbeat storage (`nodes.rs:309`) projects only validated known fields,
resolving the earlier raw-extra-field visibility concern. Shared private-pool
node visibility still requires an explicit documented operator policy. No node
authority becomes human account ownership, and placement remains immutable.

### Independent acceptance inspected, 2026-10-03 14:43 UTC

Read `data/mnv-vqn2fx79/result.json` and the harness assertions producing it.
All **13 grouped checks passed**. Reviewer independently compared SHA-256 hashes
of all **31** manifest product files with current node-core files: **no
mismatches**. The separate verifier binary matches recorded SHA-256
`5b6934a0f84f1fcdb5aef3e1915deb295862a43dc724c5b406b33d9bd0ebc6a2`.
Preflight and post-cleanup each record **zero guests/zero reserved MiB**.
Verifier's real worker reported and inherited limits are 512 MiB/two guests/two
vCPUs each, total configured guest memory 1024 MiB, planned peak three guests/
768 MiB; distinct rootless network namespaces were checked before guest boot.

Direct evidence now resolves the earlier runtime gaps for: real TLS/private CA/
hostname denial and enrollment replay; signed human HTTP ownership denial with
zero cross-owner mutation dispatch; RAM-process snapshot/fork continuity and two
captures; same-node restore and preserved cross-node child placement; actual
guest PTY; stopped-child start rejection versus running-parent restore rejection
reservation behavior; actual Chromium reload/dialog reopen stable key reuse and
one guest effect; offline fixed placement/reconnect/restart; lost gateway exec
and create replies with one effect/one physical guest; real active guest stream
replacement and revoke closure proven by EOF/Close, plus durable revoked denial.
Synthetic signed human issuer/JWKS and browser edge interception are explicitly
labeled fixtures, not public Cloudflare login acceptance.

**Recovery distinction:** gateway was killed after agent journal acceptance;
the agent completed and durably stored its result before same-key retry.
This proves lost gateway reply recovery, not replay of an unfinished journal
after agent crash. Pending agent journal remains uncertain and must prohibit
automatic replay/reschedule; existing focused journal test covers refusal,
not real crash reconciliation. Physical separate-host/NAT acceptance was not
performed. Neither limitation blocks a documented owner-only same-host pilot
that preserves uncertainty and static disjoint worker budgets.

Remaining source-reviewed but incompletely fault-executed cases include exact
post-COMMIT route loss/not_dispatched transition, malformed foreign-node result/
inventory attempts, and full production acceptor/queue/slow-peer exhaustion.
Retain focused checks in the follow-up plan; do not label them passed because
unrelated guest lifecycle cases passed. Their reviewed conservative paths and
trusted owner-only private-node scope support a bounded pilot rather than a
general/public service verdict. Final commit/hash attribution and documented
scope/rollback are the current release blockers; no new product defect was
identified in the independently accepted frozen source.

Final runbook/ADR source review: `docs/MULTINODE.md` documents the explicit
shared private-pool visibility policy, separate human/node authentication,
short dedicated worker roots, static disjoint budgets, TLS/private files,
enrollment-response-loss recovery, retry keys, single catalog owner, bounded
fencing, schema-v3 backup/old-binary rollback, cleanup and unsupported cases.
ADR 0002 preserves the narrow Unix-worker design. These address the architecture
documentation gate; final builder report/commit remain pending.

Clarification against earlier evolving-source observations: final
`nodes.rs:975` treats pending **arbitrary effects** as uncertain, but permits
same-key fixed-ID create/fork/snapshot requests to reach runtime artifact
reconciliation (`:987`). `runtime.rs:476` verifies existing create shape/source,
`:584` verifies capture source and rejects incomplete artifacts, and `:833`
requires the existing fork child source. Gateway immutable fingerprint/placement
prevents changing the requested capture/source on retry. This is source-reviewed
bounded reconciliation on the original node, not relocation or independently
verified real agent-crash recovery. Keep that distinction in the final verdict.

## Final stable-commit verdict, 2026-10-03 14:48 UTC

**Clear bounded owner-only same-host local deployment of
`7abe86d37305056558e5e1ebbc1ec16e8f2d3f33` (`work/node-core`).** No remaining
actual product blocker was found within that scope. Reviewed the scoped
`git show --stat` and final diff, MULTINODE, ADR 0002, node-core report and
handoff. Independently hashed **committed blobs**, not just working-tree files:
all **31 accepted product hashes match**, with no mismatches. Stable diff
whitespace check has no diagnostics. Final builder locked/offline results are
**21 passed/0 failed/3 ignored**; builder real TLS/VM suite **1 passed**;
independent verifier **13 groups passed/0 failed**, binary hash and artifact
references above. Reviewer did not execute those suites.

Planner may cherry-pick only this scoped implementation commit while preserving
unrelated root pending files. Deployment scope is dedicated project roots,
trusted owner/operator-controlled nodes on this same host, existing human
authentication and private TLS transport. Preserve the tested disjoint worker
caps: **512 MiB/two slots/two vCPUs each**, combined **1024 MiB/four slots/four
vCPUs**; acceptance ran a maximum planned **three guests/768 MiB**. Account for
all pre-existing guests and worker/VMM/helper/desktop headroom before starting;
do not substitute the larger per-worker defaults. This clearance does not
authorize outside services, ingress changes or public tenant admission.

The runbook's rollback procedure is acceptable: before opening existing data,
stop project writers, checkpoint/backup SQLite privately and retain the old
binary/config; schema v3 cannot be opened by the v2 binary, so rollback uses the
pre-upgrade catalog plus old binary with writers stopped. Never roll back to a
catalog predating new resources and silently adopt their disks. New dedicated
roots avoid changing the existing prototype catalog during pilot setup.
Stopping controller/agent alone leaves guests alive; use only dedicated worker
shutdown and independently verify cleanup. Independent acceptance records zero
guests/reservations and verifier report additionally confirms no residual test
processes/control sockets after cleanup.

Evidence limits and follow-up priorities remain explicit:

- **Gateway-loss recovery is verified** after a real agent journal entry and
  durable completed response; one exec effect and one created guest survive
  same-key retry. **Real agent crash before journal completion is untested**.
  Arbitrary pending effects remain uncertain/operator reconciliation; fixed-ID
  create/fork/capture may reconcile only on the original node through reviewed
  immutable fingerprint/artifact checks. Never automatically replay uncertain
  exec or relocate offline work.
- **Physical separate-host/NAT operation is untested**. Same-host namespaces,
  real TLS, signed HTTP fixtures and browser edge interception do not establish
  public Access login, WAN behavior or cross-host snapshot portability.
- Retain focused follow-up checks for post-COMMIT route failure and already-
  uncertain retry, foreign-node inventory/result corruption, isolated capacity
  axes and production connection/queue/slow-peer exhaustion. Current reviewed
  safeguards support this trusted private pilot; those cases are not claimed
  independently passed by the 13 acceptance groups.
- Existing jailer/cgroup confinement, storage quotas, off-node backup,
  immediate human-session revocation and malicious-host attestation gaps remain
  outside this prototype clearance. Documented bearer theft permits only node
  impersonation until revocation; it must never confer human account ownership.

Review handoff: architecture ready and final stable-commit verdict communicated
to planner. Only `docs/plans/multinode-review.md` was written by node-review;
no product edits, tests/VM interference, task-board edits, commits, deployment,
dispatch or outside-service mutations were performed. No owner decision is
required from this review lane; planner owns the authorized merge/deploy step.
