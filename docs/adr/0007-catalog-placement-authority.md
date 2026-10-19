# ADR 0007: durable placement authority and transfer reservations

Status: implemented catalog contract with explicit verified restore, park, wake, move and fork execution.
Pattern: ADR 0002's single-controller SQLite intents, ADR 0005's per-unit upgrade boundary,
and ADR 0006's existing artifact references.

## Decision

Keep committed placement on `resources`, separate from the proposed target in
`transfers`. Schema 5 adds `placement_managed`, `placement_epoch`, `current_capture`,
`ssh_policy` and `ssh_policy_generation`. Epoch zero with an unmanaged marker is
the existing fixed-placement contract. A positively acknowledged export marks its
source managed without changing its committed node or epoch. Aborting that export
does not silently re-enable legacy starts. Only `commit_transfer` switches the
committed node, epoch, capture and CPU/RAM reservations together.

A single `BEGIN IMMEDIATE` transaction admits an owner-scoped operation key and
canonical input fingerprint, a globally increasing attempt epoch, affected-machine
locks, the optional fork child's physical ID and owner-scoped name, and target
capacity. Same key and input joins the original operation, including its terminal
outcome; changed input rejects. A new deliberate retry uses a new operation key.
The committed high-watermark survives abort and child deletion. An allocated epoch
whose transaction was committed is never reused, even if import is uncertain.

Use the existing `artifacts` table for committed captures. Source and proposed
capture references remain distinct roots. Both preparation and commit validate
artifact state, owner and source-machine lineage. Fork children belong to the
requester; they do not inherit the source's SSH identity or enrolled keys. Artifact
rows, copies and leases are reused; no second store schema or transport is added.

`transfer_locks` permits only one unfinished transfer per affected physical ID.
A fork locks both source and child and inserts the reserved child resource in the
requester's namespace. `transfer_reservations` owns CPU, RAM, one slot and logical
disk bytes by exact transfer and epoch. Existing remote create/start admission
includes these CPU/RAM/slot reservations. Target admission uses the intersection
of registered operator budgets and fresh advertised node budgets, including CPU
caps and drain status. It does not infer local controller capacity. Disk bytes
are persisted demand; verified disk headroom, owner quotas and worker capability
validation are additional orchestration gates before dispatch.

## Consequences

A placement epoch is catalog authority, not evidence that an isolated VM stopped.
Move/park/restore must record positive source fencing before importing or committing.
A lost heartbeat, expired lease or offline node never releases source capacity.
An uncertain import retains its target reservation and machine locks across restart.
Abort/failure after import began requires positive exclusion of target effects.
After commit only forward recovery is allowed: a lost start reply may mean running.
There is no timeout-driven abort, retargeting or source reactivation.

Transitions compare transfer ID, epoch and expected phase. Commit additionally
compares source node, committed epoch, capture and SSH-policy generation, and
rechecks current authorization. A concurrent policy revocation prevents the switch.
A failure partway through commit rolls the whole tuple back. Reservations become
committed resource capacity in that transaction; stale results cannot delete another
attempt's reservation. Legacy epochless results, stopped/missing inventories and
snapshot discovery cannot mutate managed or transferring resources. Legacy routing
fails closed for these resources until epoch-bearing orchestration is connected.

All new writer transactions perform SQLite work only. Store transfer, verification,
worker RPCs and waits must occur outside them, with results supplied as explicit
evidence. The transition API is controller-internal; evidence flags are attestations
by the orchestrator after checking worker proof, never public caller parameters.
Explicit managed restore now executes these contracts through the transfer runner;
park/wake/move/targeted fork now reuse this runner. Explicit pre-commit target exclusion and fresh-epoch source return preserve the original authority until a conditional commit. No automatic movement policy is activated.

See [lifecycle contract](../LIFECYCLE.md) for phases, permissions and rollout gates.
