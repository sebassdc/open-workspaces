# Placement catalog and transfer lifecycle

The catalog persists transfer intent and executes explicit managed restore, park, wake, move and fork requests.
Existing boxes remain fixed-placement until an owner requests a transfer. Installation
alone does not move, park, start, stop or upgrade a worker. See [ADR 0007](adr/0007-catalog-placement-authority.md).

## Authority and phases

`resources.node`, `placement_epoch`, `current_capture` and reserved CPU/RAM are
the committed placement tuple. An empty node explicitly means parked; it never
means the local worker. Capture references remain roots while parked. Public SSH
policy is stored outside snapshots: null means unreconciled legacy policy,
`disabled` means explicitly unenrolled, and `enrolled` carries the existing validated
public host key, keys and worker policy identity/generation. A catalog generation
compares concurrent updates; it is distinct from the worker's policy generation.
Before managing a non-fork placement, reconcile this policy explicitly.

| Phase | Durable meaning | Capacity and permitted next action |
| --- | --- | --- |
| preparing | Source authority and target proposal captured | Both source and target held; persist next phase before dispatch |
| exported | Positive source execution fence acknowledged | Source tuple unchanged; publish a verified capture or begin import |
| importing / import_uncertain | Target import may have effects | Keep both reservations; inspect or confirm unstarted import |
| prepared | Stored capture and unstarted target confirmed, or parked export ready | Conditional commit; abort only after target exclusion |
| committed | Placement tuple switched atomically | Target owns resource reservation; forward recovery only |
| start_uncertain | Activation reply lost | Retain target capacity and locks; never automatically retarget |
| started | Activation confirmed | Confirm cleanup, then succeed |
| succeeded / aborted / failed | Terminal history, operation result retained | Release this transfer's locks; retries join history |

Move and restore follow preparing → exported → importing → prepared → commit.
Park follows preparing → exported → prepared → commit → succeeded, with no target
reservation. Fork follows preparing → exported (immutable snapshot published) → importing → prepared → commit; its source keeps running. Hibernated wake follows preparing → importing → prepared → commit. Non-park committed transfers confirm started before succeeded. Abort/fail
is allowed only before commit. Before import begins it can release unused target
capacity; afterward positive target exclusion is mandatory, including after an
uncertain reply. Exported sources remain managed and fenced after abort until an
explicit future source-reactivation protocol succeeds. Terminal rows cannot be
advanced, committed again or used to release another attempt's capacity. Releasing
a fork child removes its grants and quota allocation, disables its publications,
and retires its operation-name binding atomically with the child reservation.
Reusing an ID or logical name cannot inherit the released child's authorization
or pin a new create to its old placement.

Stopped and hibernated remain different user states. Parking a stopped disk capture
preserves stopped state. Connection-triggered wake accepts only a parked hibernated
box; a stopped box needs explicit start/wake. A list, name lookup
or card view grants no start authority. Cold memory loss requires explicit consent
in the durable intent and enforcement by both the restore runner and worker.

## Permissions

The public restore API accepts a catalog artifact ID, target node, owner-scoped
operation key and optional explicit cold consent. Park, wake, move and targeted fork use the existing API/CLI surfaces.

| Action | Owner | Shared admin | Shared use | Node / guest |
| --- | --- | --- | --- | --- |
| Move, park, managed restore | Yes | No | No | No |
| Wake hibernated placement | Yes | Existing start permission | Existing start permission | No |
| Fork to a new owned child | Yes | Yes, requester owns child | Yes, requester owns child | No |
| Update public SSH policy | Yes | Existing SSH-key permission | No | No |
| Grant/revoke sharing | Yes | No | No | No |
| Advance/commit transfer | Trusted controller only | No direct access | No direct access | No |

Authorization runs before joining/admitting an intent and again at commit. Future
orchestration must also check it before dispatch and after waiting before granting
access. Revocation remains available while a transfer waits. Policy generation
changes invalidate a prepared commit; a stale copied key set cannot replace newer
policy. Fork starts explicitly unenrolled and must obtain a new host identity;
copying a snapshot does not sanitize guest credentials.

## Migration and rollout

The placement migration introduced schema 5 / reader floor 5; explicit runner
retirement advances the catalog to schema 6 / reader floor 6. Schema-3 ownership/grants/operations and
schema-4 artifacts/copies/leases survive the additive migration. Foreign keys,
private owned catalog permissions, WAL and FULL synchronous writes remain enabled.
Old readers/writers are refused: they cannot safely ignore parked placement,
epochs or transfer roots. There is no automatic schema downgrade.

Freeze controller and operator mutations before upgrading catalog readers.
Before migrating an existing catalog, the controller creates a private consistent
SQLite copy named `catalog.pre-placement-<nonce>.sqlite3` outside the migration's
writer transaction. Backup failure refuses migration. Keep that copy and the
previous binaries privately; snapshots and required trust/config still need their
normal backups. Verify integrity and ownership before relying on a rollback copy.
Once the new controller has accepted writes, rollback requires a current compatible
catalog or a controlled recovery procedure; an old backup cannot authorize old
placements. Never run two writable controllers. A host-local gateway lock is not
a cross-host fence. Recovery from a stale backup stays non-destructive until the
backup gap and all capture/transfer roots are reconciled.

This is a class 1 controller/catalog change. Update every catalog-reading controller,
dashboard and node-management process together; keep running worker pins unchanged.
A control-plane restart may interrupt streams but must not restart a worker or VM.
Release 0.9.3 declares catalog schema 5 / reader floor 5 as a class 1 change
from v0.9.2. Keep the worker pin unchanged; that catalog foundation alone did not
activate transfer execution.

Restore execution requires worker-enforced placement fences and
replay-safe steps, exact runtime compatibility checks, artifact validation, owner
and disk admission, explicit cold-loss consent and epoch-bearing routing for every
wake/stream path. Legacy runners remain ineligible for managed movement. Test real
execution only on disposable boxes. Synthetic catalog tests do not establish VM
migration, RAM portability, S3 enforcement or recovery of an isolated running VM.

## Restore execution and recovery

`restore` with an artifact uses the phases above; legacy named-snapshot restore
retains its original interface. Controller startup scans nonterminal restore rows.
A private per-transfer file lock coalesces concurrent runners; the SQLite predicates
remain authoritative. Four transfer runners at most execute concurrently. No network
wait, compression, hashing or object transfer holds the catalog mutex or a writer
transaction. Agent pull/cache jobs do not enter the serial worker.

The source's public policy is reconciled from the authorized stopped copy before
admission and checked again after export. Its ownership, shape and physical ID come
from the catalog. A stored artifact must match owner and machine, its pinned manifest
digest/token, native manifest name/subject/shape, exact consistency set and file hashes.
F-store validates paths, codecs, sizes and uncompressed hashes before publishing a
private local capture. Existing captures and disks cannot be overwritten.

A pull lease records each agent attempt before dispatch. Uncertain attempts cannot
be restarted until expiry; a new attempt gets a new token/staging directory. Verified
cache inspection can establish completion sooner. Worker requests use fixed
transfer/step keys; uncertain imports first inspect the epoch-bearing inventory and
native import fingerprint, then resume only their own partial import. A prepared
copy is never activated before conditional catalog commit. Activation first persists
`start_uncertain`; a matching running copy confirms a lost reply, while contradictory
or active-but-nonrunning state stops for reconciliation rather than replaying start.
A responding source is checked for contradictory execution before commit/activation.
A restarted worker may report `exporting` with an `exporting` or `captured`
lifecycle phase. The controller replays export only when the nonrunning source,
placement, transfer ID, capture name and export mode match its persisted intent.
It uses the original worker step key and waits for positive completed fencing;
a partial capture alone never permits publication, import or placement commit.
Source absence before positive fencing never allows progress. An offline source after
its acknowledged durable export cannot restart through legacy requests.

Compatibility is checked before import. Incompatible RAM fails that operation without
removing the stored artifact. A new operation with explicit cold consent can reuse a
freshly inspected export fence from a matching prior catalog intent; it cannot alter
the old operation's fingerprint or re-enable the source. Compatible imported copies
retain memory state; cold copies explicitly discard RAM at import. The worker checks
compatibility again at the trust boundary.

Successfully adopted restore and wake placements enable the epoch-bearing routes.
Unsupported managed mutations remain blocked. Terminal/SSH check their revocation
watch again after opening the worker stream. Ownership and existing grants remain
catalog facts and are rechecked at admission, each step, commit and stream routing.

This adds class 1 controller/agent orchestration, with no schema migration, reader-floor
change or worker runtime change. It requires the T2 protocol-2 worker semantics to
already be installed through their separate class 2 upgrade. Old agents do not
advertise `managed_restore_v1` and are ineligible. Agent capability reflects the
checked worker executable, and JSON additions remain optional for older controllers.
No worker or VM is restarted by installing the controller. Retained source copies,
cache cleanup, automatic recovery from unapproved source loss and legacy-backup
catalog registration are outside this restore executor. See DAILY_USE for commands.

## Observing an adopted workspace

Worker lifecycle replies keep their existing shape: ordinary start, stop and
hibernate responses need not contain placement. After a managed lifecycle effect,
the controller inspects the worker and verifies physical ID and the exact requested
placement before saving metadata and completing the operation. An unavailable or
contradictory post-effect inspect records an uncertain operation, rather than
leaving it pending or attaching a guessed epoch to an old reply.

Completed restores refresh metadata from inventory only when the reporting node
and epoch/node/transfer tuple all match the committed catalog placement. This
includes failed VMMs and a stopped copy after worker restart. Source, stale/future
epoch and transferring-copy reports cannot overwrite current metadata or release
its capacity. Managed reservations release only on a matching positive nonrunning
observation; offline or absent inventory is not proof of source loss.

The x86 Linux unit-test binary includes process-crash acceptance tests:
`cargo test --locked -p ow --bin ow transfers::process_tests -- --test-threads=1`.
They SIGKILL real controller and node-agent subprocesses at persisted phases and
before/after job effects, then invoke startup recovery against durable catalogs,
verified byte caches and node journals. Their separate durable worker fixture
emulates VMM execution; these tests prove controller/agent process recovery, not
hardware isolation or RAM portability. All barriers compile out of release builds.

## Explicit park and shared wake

`ow --server URL --operation-key KEY park BOX` parks a stopped or hibernated box.
It never captures a running box. The source agent must advertise `park_wake_v1`
from an installed protocol-2 fencing worker and have the same configured artifact
store as the controller. A managed hibernated box exports its RAM/disk consistency
set; a stopped box exports disk only. T2 can register an unmanaged **stopped**
copy, but cannot adopt an unmanaged hibernated copy. That case refuses without
booting or discarding RAM; it needs a separate explicit adoption/worker upgrade.

Park follows preparing → exported → prepared → committed → succeeded. It shares
`recovery-upload.lock` with the existing recovery uploader and adds no timer.
The export fences execution before publishing a new format-1 artifact under an
attempt token. Publication checks the configured store's retention/write-once
policy, writes the manifest last, reads it back by digest and validates native
lineage/shape/file hashes. Only then can the catalog commit `node = ''`, the stored
capture root and zero source CPU/RAM reservation. The original stopped/hibernated
state remains visible. Upload failure keeps the original placement, disk, capture
and reservation, with a retryable uncertain operation. A publication receipt and
write-once retry reconcile a lost reply; an expired attempt receives a fresh token
while its old lease remains recorded. Source cache and exported disk are retained.

`ow --server URL --operation-key KEY wake BOX` explicitly places and starts a
parked box. `start BOX` also wakes a parked box, including a deliberately stopped
one. SSH, exec, files, terminal and preview connections wake parked **hibernated**
boxes; connecting to a stopped box refuses until explicit start/wake. List,
inspect, SSH config/key lookup and name resolution never initiate placement.

Explicit start preserves the caller's operation key. Retrying it resolves the
original outcome before examining current placement, including after a later stop
and park. An explicit start/wake that joins another caller's wake records its own
key as a durable `joined` operation pointing to the same transfer; status follows
that execution without creating another reservation. Joined keys keep their
original input and cannot authorize a later park/wake cycle. Owner recovery after
sharing revocation can promote an existing matching join within the same recovery
transaction, preserving the transfer, epoch and uncertain effects.

All connection types/users join one wake under the physical machine's transfer
lock. Queued wake has no target reservation. Fresh, nonrevoked, nondraining fencing
runners with the required agent, disk headroom and CPU/RAM/slot budget are tried
in stable node order. Warm RAM also requires a matching advertised runtime, CPU
and guest-kernel tuple before any reservation. Parking retains the digest-verified
manifest in its durable operation record; older records read the pinned manifest
without holding the catalog lock. Disk-only or explicitly consented cold wake
does not require a RAM match. A compatible runner without available capacity or
one temporarily draining/offline produces visible `queued` status; startup recovery
tries again. Once reserved, a timeout retains that target and reservation and does
not create another operation. The runner probes verified cache, pulls only if
absent, imports unstarted, commits placement and then activates. Source-cache reuse
first applies exact old-export cleanup after checking its completed catalog intent;
this removes only the old machine directory, preserving every capture and fence.
A failed or uncertain wake keeps the artifact root; it never falls back to an
unapproved cold boot or assumes an offline source was fenced.

Permissions are checked before admission/join, each durable step, commit and again
after a connection wait. SSH policy generation changes invalidate the transfer.
Existing stream revocation checks still run after worker stream creation. A joining
user sees safe step/progress information, not another user's operation key/name.
Connection waits are bounded; reconnect joins the same operation. Explicit command
results return the existing operation record and can be followed through `status`'s
operations, including `transfer.step`, `queued`, and `progress`.

Progress reports logical bytes in completed compressed file objects. It is polled
approximately every five seconds during a transfer, and final completion is saved
with the transition. It is not instantaneous wire throughput or proof of import;
the step distinguishes transfer completion from validation/commit/start. Errors
retain the last progress. When no runner matches the stored RAM, wake fails before
reservation or pull, leaving the parked artifact available for a fresh explicit cold decision. Worker
compatibility is rechecked after pull; a changed tuple fails before import and keeps
the old artifact/parked placement. A new operation key with `wake --accept-cold`
records explicit consent; the old RAM artifact stays recoverable through and after
that decision's commit. Ordinary connections cannot supply this consent.

This is class 1 controller/agent orchestration atop already installed T2 worker
primitives. Schema/reader floor remain 5 and no new migration number is claimed.
The optional `explicit_start` intent field is omitted when false, preserving prior
restore fingerprints. Older controllers do not execute park/wake/move/fork or route their completed
placements; rollback fails closed rather than ignoring placement authority.
New private publication/progress jobs require the advertised agent support; worker
protocol, runtime, installed worker pins and release versions are unchanged.
No timer, automatic parking/drain policy or workstation movement is installed.

Focused tests include RAM/disk and disk-only publication, upload failure/lost reply,
controller reopen after each phase, concurrent connection admission, drain queueing,
revocation during pull, source-cache reuse and explicit cold consent. The subprocess
suite kills actual controllers at every park/wake phase and agents around publication
using real node jobs and a local AWS fixture. Its separate durable worker models VM
execution. Real Firecracker continuation, live bucket IAM/policy and guest SSH/process
continuity remain dedicated disposable-fixture acceptance; these tests do not establish
them. Running workspaces must never be used as lifecycle test fixtures.

If sharing is revoked while a shared user initiates wake, the owner can retry with
`wake BOX` and a fresh operation key, or reconnect to a hibernated box. The controller
records a new owner-authorized operation and marks the revoked user's operation failed.
It retains the same transfer, epoch, target, reservation and worker step keys, including
uncertain imports or committed activation; it never discards an uncertain target to
start elsewhere. The grant stays revoked and the original request remains in history.
A changed SSH policy generation still requires reconciliation, and existing cold consent
must be explicitly confirmed by the owner; an implicit connection cannot supply it.

Park may reuse a positive fence left by a failed restore. It publishes the capture named
by that verified worker export, rather than inventing a name from the new park ID. That
binding is persisted in the artifact row. A retry can repair an unpublished incorrect
binding using a fresh attempt token while retaining the old attempt lease. Published
artifacts cannot be rebound to a different capture.

## Explicit move and cross-runner fork

```sh
ow --server URL move BOX --node RUNNER --operation-key move-1
ow --server URL fork BOX CHILD --node RUNNER --operation-key fork-1
ow --server URL status
```

Move is owner-only and preserves physical ID, logical name, owner, sharing grants,
public SSH key policy and pinned guest host key. A running move captures paired RAM
and disk and resumes processes only on a compatible runtime/CPU/host-kernel/guest-kernel
tuple. Moving a stopped or hibernated copy preserves that state; move is not an
implicit start. `--accept-cold` is an explicit disk-only execution decision and keeps
the stored RAM artifact recoverable. No live resize is included.

A named `--snapshot` uses the saved snapshot's RAM, CPU and disk shape, including
a snapshot taken before the parent was resized. Snapshot selection and its target
reservation commit with admission before any worker effect.

Targeted fork snapshots the source, publishes and verifies the artifact, prepares a
new unstarted child, commits its reserved ID/name and requester ownership, then
activates it. Existing admin/use sharing roles retain fork permission. Ordinary
fork defaults to the current runner after a managed move. Guest machine ID, network
identity and SSH host identity refresh on the child's first activation. It inherits
no enrolled public keys or sharing grants; enroll it separately with `ssh-authorize`.
Guest files and other credentials are copied, as with existing forks.

The source must acknowledge a durable execution fence before a move can import or
commit elsewhere. Offline before acknowledgment waits. Offline after acknowledgment
but before publication also waits; silence never authorizes source-loss recovery.
Once the verified artifact is catalog-stored, the target may continue while the
source is offline. Cleanup removes only that exported machine directory, preserves
snapshots and its durable fence, and retries when the source returns. Status exposes
`cleanup_pending`. Contradictory running reports block reconciliation, including a
lost activation reply; a timeout never selects another target after commit.

Before commit, an owner can explicitly recover a move using the transfer ID and
**current epoch** shown by status:

```sh
ow --server URL move BOX --transfer 17 --epoch 23 --node OTHER
ow --server URL move BOX --transfer 17 --epoch 23 --abort
```

Retargeting first persists worker exclusion of the old unstarted target, then
allocates a fresh epoch/reservation. A delayed prepare or activation at the excluded
epoch is rejected after worker restart. Abort waits for a sealed, stored source
capture, excludes the target and performs a fresh-epoch return to the source before
restoring access. It does not clear an export fence or revive old authority. The
fork requester can use the same recovery command with its source address; abort
retires only its reserved child after exclusion. The source owner may also abort a
revoked requester's pre-commit fork without restoring that grant. Once a fork has
committed, forward recovery checks the new child's ownership; later source-sharing
revocation does not undo the committed independent child or restore source access. Source loss requires a separate
positively attested recovery procedure; this command never infers that permission.
After commit only forward recovery on the committed target is allowed. A pending
explicit recovery decision blocks a racing catalog commit.

Delete, resize, snapshot and SSH-key mutations are blocked while transfer locks are
held. Sharing revocation stays available and is rechecked before effects/commit and
access. Prepared SSH-policy installation runs under the worker lock and refuses a
different persisted policy generation. Subsequent managed key replacement updates
the catalog generation before another transfer can be admitted. Legacy key changes
also advance that generation and invalidate cached policy, so an admission read
cannot survive a concurrent revocation. Export replay rechecks the source policy
before reusing an acknowledged fence. Once no transfer lock is held, a managed box
deletes, resizes, snapshots and changes keys under its committed placement (see
[Lifecycle of a moved box](#lifecycle-of-a-moved-box)). Ordinary named restore stays
refused for it: restore from the snapshot's artifact with `restore --artifact`.
Managed fork uses its own locked snapshot path.

This slice is **class 2**: worker interface 3 adds fork preparation, durable target
exclusion, serialized prepared policy installation and running-source registration.
Agents accept interfaces 1–3, but movement requires `move_fork_v1` from the actual
worker plus the configured artifact jobs. New managed child/exclusion registries
use worker state schema 3; older workers refuse them. Catalog schema/minimum reader
remain 5; no migration number is consumed. Use the existing per-unit quiesce, warm
compatibility and explicit cold-consent upgrade path on dedicated eligible runners.
Installing this release never moves a box, changes a worker pin or deploys capacity.

Fault tests cover before/after replies and catalog reopen at every step. The subprocess
fixture SIGKILLs real controllers at eight move and eight fork boundaries; it models
worker execution and uses a local object store. The opt-in `move_fork_real_vm_counter_ssh_grants_and_independent_writes`
fixture requires explicit fresh roots, a built binary and KVM/reflink-capable assets.
It checks real process/boot continuity, strict pinned-host-key OpenSSH, grants, child
ownership, separate enrollment and independent disk writes on two disposable workers.
Its execution record, rather than its presence or a successful API call, establishes
hardware acceptance. Live cloud-store IAM/policy and fleet rollout remain separate.

## Lifecycle of a moved box

A box that a move, cross-runner fork, wake or managed restore placed (a *managed* box)
keeps its ordinary lifecycle. Every request below carries the exact committed
`placement` (epoch, runner, transfer); the worker refuses any other, and every one is
refused while a transfer lock is held.

| Operation | Managed behavior |
| --- | --- |
| `ssh-authorize`, `ssh-revoke` | Key replacement on the current runner, then the catalog stores the new public policy and advances its generation, which invalidates any transfer admitted against the old one. |
| `resize` | Stopped boxes only, as for any box: CPU, RAM and disk growth. The worker reply has no placement, so the catalog saves its own post-effect `inspect`, which must name the committed placement. A later move or park takes the new shape. |
| `snapshot` | A named capture on the current runner, sealed `managed` like every capture of a managed box. The catalog records the box's disk size with it, for publication and forks. |
| `snapshot-delete` | On the snapshot's runner while the box is there. A snapshot already in the artifact store and no longer on the box's runner is removed from the catalog alone. |
| `delete` | Stopped or hibernated boxes only. The worker removes the disk, recovery journal and the box's local snapshots, and keeps its fence as a **tombstone** (`cleaned`, no export intent, state `deleted`): any later request, including an import at a newer epoch, is refused. A repeated delete answers from the tombstone, so a lost reply is safe to retry. The catalog then drops the box, its grants, quota allocation and snapshot rows. A **parked** box deletes in the catalog alone; its stored capture and any fenced source export stay until store collection and runner drain, as after every park. |

Delete and named-snapshot deletion need `managed_lifecycle_v1` from the runner's
actual worker; the controller refuses them, naming the runner, until that worker is
upgraded. Resize, snapshot and key changes use existing worker primitives.

### Named snapshots outlast their runner

The controller's recovery loop (every five seconds) publishes each ready named
snapshot of a managed box, once, from the runner holding it, to the configured
artifact store. It reuses the drain publication path: a per-attempt token,
manifest-last upload, read-back by digest and native lineage, shape and hash
checks, recorded per runner and name. Once published and once its box has left
that runner (and holds no transfer lock), the snapshot is **rehomed**: its catalog
row no longer names a runner, and the old runner's copy is removed under that
runner's own fenced placement for the box. A copy is also removed after its
snapshot or box is deleted; a retired runner's record is released without contact.
Failures are logged once and retried.

A fork from a stored snapshot imports its artifact on any eligible runner (pulling
it, or using the verified copy the publishing runner kept) without a new capture or
upload. A snapshot not yet stored still forks only on the box's current runner.
Snapshot listings show the stored `artifact` ID, which the owner can restore with
`ow restore BOX --artifact ID --node RUNNER` (managed restore, which needs another
runner than the box's own). A deleted snapshot or box stops rooting its artifact;
the [collector](STORE_GC.md) removes it after its grace period.

Limits: snapshots taken before this release without a recorded disk size are not
published; a shared user's own snapshot of a box is published and rehomed, but forks
from the store only for the box owner; snapshots of boxes that were never managed
keep their runner-bound behavior.

The focused tests run real move and fork transfers, then each operation against
runner sockets that refuse any other placement, including after the move's source
is cleaned and for a cross-runner fork child; worker tests cover the tombstone, its
replay and restart, and named-snapshot removal on an active and a fenced copy. The
dashboard suite drives a moved box's snapshot, stop, resize and delete. Real-VM and
live-store acceptance on a pool remain separate.

## Explicit runner retirement

See [runner drain](DRAIN.md) for schema-6 controller states, protected eligibility,
source-only cleanup and sealed, generation-bound empty certificates. Installation
never drains a runner; provider destruction remains outside the controller slice.

The opt-in [bounded shadow scaler](SCALER.md) consumes queued wake shapes and
existing placement/drain contracts. It records one capped recommendation per pool,
with no provider call or automatic drain; live activation remains unavailable.
