# Placement catalog and transfer lifecycle

The catalog contract persists transfer intent; it does not execute transfers.
Existing boxes remain fixed-placement. Installation alone does not move, park,
start, stop or upgrade a worker. See [ADR 0007](adr/0007-catalog-placement-authority.md).

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
in the durable intent and enforcement by the future worker orchestrator.

## Permissions

The following are catalog admission contracts, not new public commands.

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
from v0.9.2. Keep the worker pin unchanged; transfer execution remains dormant.

Before enabling transfer execution, require worker-enforced placement fences and
replay-safe steps, exact runtime compatibility checks, artifact validation, owner
and disk admission, explicit cold-loss consent and epoch-bearing routing for every
wake/stream path. Legacy runners remain ineligible for managed movement. Test real
execution only on disposable boxes. Synthetic catalog tests do not establish VM
migration, RAM portability, S3 enforcement or recovery of an isolated running VM.
