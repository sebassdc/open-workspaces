# Worker placement fencing

Linux workers provide private lifecycle primitives used by the durable controller
transfer runner for explicit restore, park, wake, move and fork. Existing
unmanaged machines retain standalone behavior. No installation automatically
registers a managed machine, transfers a workspace, or upgrades a running worker.

## Authority and execution

Every managed request that can execute or change a machine carries the exact
`placement: {epoch, node, transfer}` from the controller's committed record.
Epochs are positive signed-64-bit integers; node and transfer use the existing
identifier rules. A missing, older, proposed-future, wrong-node or wrong-transfer
authority is refused. Preparing a target does not activate it. The private
controller sends `activate` with `committed: true` only after conditional catalog
commit. That boolean is an authenticated controller assertion, not a replacement
for the catalog transaction. Private local control remains an OS-owner capability.

The worker checks ordinary start, implicit exec/files/key-update wake, SSH,
terminal, and address/preview wake under its runtime lock. Existing direct
SSH/terminal/address methods accept unmanaged workspaces only; request-bearing
methods validate managed authority. A deliberately stopped machine still requires
explicit start. Managed incompatible RAM never silently cold boots; prepare with
explicit `cold: true` to discard RAM for execution while retaining the capture.
Guest SSH host-key and policy verification remain in `ssh_guest`.

## Private worker operations

All steps carry `id` and `placement`; keyed node requests also carry a stable
`operation_key`. These operation names require `placement_fencing_v1: true` from
an actual compatible worker. A newer agent with an old worker advertises false.

- `placement-register`, `committed: true`: bind an existing unmanaged **stopped**
  copy to its committed authority. It does not start or adopt a running VM.
- `export`, `transfer`, `capture`: durably fence the source before capture. A
  running VM is paused and captured; a hibernated VM retains its consistency set;
  a stopped VM seals disk only. Confirm child exit before acknowledgment.
- `seal-disk`, `transfer`, `capture`: the stopped-only, disk-only export form;
  it also fences the source. Use ordinary snapshots for non-transfer captures.
- `import-prepare`, `snapshot`, `manifest_sha256`, optional `cold`: verify the
  native local capture manifest and subject, persist importing intent, build the
  machine directory, then register **prepared and unstarted**. Retry a missing,
  partial or completed intent-owned directory under the same fingerprint.
- `activate`, `committed: true`, optional `start: true`: activate exactly that
  prepared authority; optionally start it. Ordinary epoch-bearing `start` is
  replay-safe once activated. A timeout may mean the VM is already running.
- `inspect`: expose placement, phase and intent with the existing machine summary.
- `export-cleanup`, the exact export `transfer` and `capture`: remove only that
  exported machine directory. Keep all snapshots and the persisted fence. A
  cleaned runner may prepare a later incarnation only at a strictly greater epoch.

Export returns `capture: {snapshot, metadata, manifest_sha256, files}`. Each file
entry contains its private local `path` and lowercase uncompressed `sha256`.
The native metadata includes subject, shape, image and runtime/CPU/kernel tuple.
Exports and ordinary snapshots/hibernation of managed workspaces seal
`managed: true` into that manifest. This provenance is not activation authority.
Legacy import, restore and fork reject these captures before registration or disk
replacement, including on a fresh worker with no local placement record. An
unmanaged workspace cannot wake from a managed hibernation capture either.
Consume a managed capture through `import-prepare` and post-commit activation.
Older unmanaged manifests without the field keep their standalone behavior;
legacy workers that cannot enforce this boundary remain ineligible for moves.
A RAM capture has `state`, `memory` and `disk.ext4` together; a disk-only capture
has `disk.ext4`. These are local worker results, not public API response fields.

## Crash and replay contract

`exporting` is durable before effects. After capture files and their directory
are fsynced, persist `captured`, confirm source process exit, hash and seal the
manifest, rename, fsync the snapshot parent, then persist `exported`. Startup
preserves intent-owned staging and phases. Sealed capture plus missing final
registry update finishes by verification. Completed `captured` staging records whether RAM is required and is sealed
without recapture; missing RAM fails closed. A nonpersistent completed-capture hint
lets an in-process retry finish fsync after a local error without issuing commands
to an already paused VM. That hint never persists a captured phase before fsync. Incomplete `exporting` staging with no VM is discarded and the
surviving disk is sealed; this records disk recovery, never memory continuation.
A still-running source finishes capture. Missing/corrupt completed files fail
closed and remain available for inspection.

Import intent precedes machine-directory creation. Replay rebuilds only a directory
owned by that same intent; prepared/activated retries do not allocate another
network index or register again. Cleanup is idempotent after directory removal.
The worker keeps a cleaned fence rather than allowing a legacy create/import to
recreate the old incarnation. Cleaned fences remain inspectable but do not consume
live workspace slots. Create, legacy import, staged import and status share that
live count; a returning cleaned subject still needs a free slot. Ordinary captures
respect names reserved by lifecycle intents and imports, even before publication.

The node effect journal retries these tested steps, plus epoch-bearing start,
managed delete and managed named-snapshot deletion.
Arbitrary exec remains uncertain after a lost effect reply. Cached managed replies
are rechecked against current worker authority; on fencing workers this also
covers cached legacy requests without an epoch. Revalidation acknowledges authority
without requiring a machine summary, so a completed unmanaged delete remains
replayable after its record is gone. Controller completion must still
conditionally match its transfer and epoch: a worker reply never changes catalog
authority by itself.

## Managed delete and named snapshots

Workers advertising `managed_lifecycle_v1` accept `delete` for a managed copy with
its exact, activated authority. The VM must not be running. The machine directory,
recovery journal and the box's local snapshots go, except a capture another managed
copy still imports from. The fence stays as a **tombstone**: phase `cleaned` with no
export intent, workspace state `deleted`, kept across restart. A repeated delete
with that authority answers from it, and a cached delete reply revalidates against
it. Every other request, an `export-cleanup` and an `import-prepare` at any newer
epoch included, is refused. Tombstones consume no live workspace slot.

`snapshot-delete` of a named snapshot sealed `managed` needs the box's own exact
authority, which may be fenced (exported or cleaned): removing a named snapshot
never executes or changes the copy. Hibernation, recovery and lifecycle captures,
and the copy's own import capture, are still refused. An unmanaged workspace still
cannot delete a managed capture. Older workers refuse both, so the controller asks
for this capability first.

## Compatibility and remaining integration

Worker interface **3**, agents accept **1–3**; host/node transport remains **1**.
A managed `state.json` uses worker state schema **2** and a null legacy
`next_index`, with the real index in `managed_next_index`. Old workers require an
integer there and refuse the registry; workspaces remain an object for operator
inventory tools. New workers refuse missing placement records and unknown state
schemas. An unmanaged registry keeps the prior readable form. Downgrade after
managed activation requires an explicitly reconciled pre-activation recovery,
never dropping the fence fields.

Worker changes are upgrade class **2**; agent/controller stream envelope support
is class **1**. Preserve per-unit pins, quiesce, compatibility checks and explicit
cold-loss consent. A supervisor may use worker inventory only to stop its own
running copies during quiesce; controller start authority must come from catalog
commit. Existing legacy runners remain fixed-placement and ineligible for moves.
Release 0.8.6 declares interface 2 and these impacts; historical release entries
remain unchanged.

Activation gates: the catalog placement/transfer contract, conditional completion
and reader floor; a format-1 adapter after the artifact-store foundation lands;
and dedicated-runner real VM acceptance before rollout. The adapter maps native
sealed file entries to store `Source`, and retains native compatibility metadata
inside transport metadata. A format-1 transport `manifest.json` is **not** a native
worker snapshot manifest and must never be passed directly to import.
SSH/terminal/preview envelopes accept supplied placement, while current catalog
routes intentionally supply none until catalog authority is wired. This keeps
managed movement inactive and fails closed if a managed copy is reached early.

## Verification boundaries

Focused tests exercise durable-intent/capture/data-fsync/manifest-fsync/rename/
registry crashes; export cases a–e; absent, partial and complete imports; activation
and cleanup interruption; every wake entry; stale journal results; and legacy
reader refusal. A synthetic VMM API/serial fixture uses a real child process and
checks observed exit before export acknowledgment. Unit tests substitute byte
copy only when the test filesystem refuses reflink; production still requires
reflink. These fixtures prove state and retry behavior, not Firecracker RAM
portability, real host reflinks, physical host isolation or cross-node migration.

## Move/fork worker extension

`move_fork_v1` is advertised by interface-3 workers, not inferred from an agent
upgrade. `placement-register` may bind an ordinary running/hibernated source without
starting or stopping it; export still supplies the positive execution fence.
`fork-prepare` binds the pinned source snapshot to a different reserved child ID
and remains unstarted. Activation refreshes guest/network/SSH identity once and
requires independent enrollment. `import-discard` durably excludes an unstarted
attempt before removing its directory, including an attempt whose prepare reply
was lost. Equal/older epochs can never prepare or activate afterward; a fresh epoch
may prepare a new copy. Active copies cannot be discarded. `policy-prepare`
serializes policy installation against activation and rejects a different generation.
Child/exclusion state uses reader-guarded worker schema 3. Existing schema-2 fences
remain readable; older workers cannot read schema 3. Source-only cleanup retains
captures and the old execution fence. These are class-2 changes, requiring the
existing explicit eligible-runner upgrade protocol.
