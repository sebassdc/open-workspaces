# Placement catalog and transfer lifecycle

The catalog persists transfer intent and executes explicit managed restore requests.
Existing boxes remain fixed-placement until an owner requests a restore. Installation
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
reservation. Fork and hibernated wake follow preparing → importing → prepared →
commit. Non-park committed transfers confirm started before succeeded. Abort/fail
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
box; explicit start orchestration for a stopped box is separate. A list, name lookup
or card view grants no start authority. Cold memory loss requires explicit consent
in the durable intent and enforcement by both the restore runner and worker.

## Permissions

The public restore API accepts a catalog artifact ID, target node, owner-scoped
operation key and optional explicit cold consent. Other transfer kinds remain
catalog contracts without a public executor.

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

Schema 5 has minimum reader schema 5. Schema-3 ownership/grants/operations and
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
Source absence before positive fencing never allows progress. An offline source after
its acknowledged durable export cannot restart through legacy requests.

Compatibility is checked before import. Incompatible RAM fails that operation without
removing the stored artifact. A new operation with explicit cold consent can reuse a
freshly inspected export fence from a matching prior catalog intent; it cannot alter
the old operation's fingerprint or re-enable the source. Compatible imported copies
retain memory state; cold copies explicitly discard RAM at import. The worker checks
compatibility again at the trust boundary.

Only successfully adopted restore placements enable the new epoch-bearing routes.
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
