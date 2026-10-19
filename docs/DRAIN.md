# Drain a runner safely

Runner retirement is an explicit local-owner action. Installation leaves every
runner ineligible for scale-in. The first enrolled runner and `local` cannot be
enrolled for retirement. Keep the base and any review/workstation runners
ineligible. No timer initiates drains and no provider resource is destroyed here.

The controller uses the existing SQLite intent/conditional-transition pattern
and the existing transfer reconciler. Running workstations are never stopped,
hibernated or moved by drain. Soft drain excludes new placement and waits for
running work and previously admitted transfers. A transfer reserved before drain
can finish; new wakes reserve another eligible runner or remain visibly queued.
Sleeping connections wait for parking, then join the normal coalesced wake.
Deliberately stopped boxes still need explicit start. Connection waits are bounded;
a timeout leaves the same park/transfer recoverable and asks the client to reconnect.

## Operator commands

Run on the controller host, using the same base as `ow host up` (`BASE/controller`
is the catalog). These are local administrator commands; ordinary workspace users
and node credentials cannot enroll runners for removal.

```sh
ow --data-dir BASE node drain TEST_RUNNER --action status
ow --data-dir BASE node drain TEST_RUNNER --action allow
ow --data-dir BASE node drain TEST_RUNNER --action begin
ow --data-dir BASE node drain TEST_RUNNER --action status
# While still soft-draining only:
ow --data-dir BASE node drain TEST_RUNNER --action cancel
```

The persisted states are `active`, `soft-draining`, `draining`, `empty`, `retained`
and `removed`. Status includes eligibility, generation, last error, certificate
and retention evidence. A separate controller policy or runner upgrade drain is
combined with retirement drain; cancel clears only the retirement soft drain.
The upgrader continues to own its existing `drain` file. The retirement seal uses
`drain-seal.json`, and either file makes the agent advertise drain.

Once work finishes, drain parks stopped/hibernated boxes through the existing
export/publication/commit path. It publishes otherwise-unstored named snapshots
and known sleep captures with per-attempt tokens and manifest-last verification.
Failed capture/upload retains source placement, reservations, disk and capture;
controller restart resumes through durable transfer history. Legacy unmanaged
hibernated boxes cannot be adopted by the existing worker primitives: they retain
the runner and RAM until separately reconciled. Unknown, partial or unmanaged
captures, files, images, recovery records or staging refuse retirement rather
than being deleted. Valid settled recovery journals for known worker copies are
scheduling metadata: they remain on the retained disk and are included in the
inventory digest. Empty journals from polling stopped boxes do not block parking.
Pending recovery uploads still block drain; durable references must have matching
local captures, which pass the same publication checks as other captures. This
conservative refusal is inspectable in `error`.

Cleanup is only `export-cleanup` of a positively fenced, durably published,
completed transfer whose source no longer owns placement. It never invokes
ordinary `delete`, and preserves named snapshots and exported captures. Drain
publication records remain explicit GC roots. Retiring a named snapshot's old
runner does not add automatic cross-node named-snapshot restore; the committed
artifact is available to the existing managed artifact restore path.

## Empty certificate and retained disk

Final inventory takes exclusive admission against **all** node jobs. The agent
persists its retirement seal, quiesces the worker, and rejects subsequent jobs
that can start, mutate or import workloads, including after agent restart. Sealed
inventory reapplies quiesce after worker restart. Existing managed source fences
remain worker-enforced. Privileged local operators remain trusted; this is not a
fence against a host administrator deliberately replacing binaries or storage.

Inventory is bounded by entry count, response size and worker RPC deadlines. It
includes worker PID/start time/boot identity, VM lifecycle state, local capture
manifests, raw directories and owned processes in workspace directories. Missing
or unknown observations fail closed. Empty requires no placed machine, running
or unknown VM, active transfer, unresolved operation/reservation, unstored capture
or named snapshot, unmanaged data or active relevant artifact work. Completed
transfer upload/pull leases release only after positive terminal success.

The certificate binds the exact runner connection, drain generation/seal, worker
identity, inventory digest and catalog revision. Placement/reservation writes are
fenced in SQLite. Prior reserved transfers remain visible until they finish.
The final transaction checks the revision again after network inspection. Consume
repeats fresh sealed inventory and all predicates. Controller/agent/worker
replacement or any relevant catalog write invalidates stale proof. The revision
is deliberately conservative across the catalog, so unrelated catalog activity
can require recertification:

```sh
ow --data-dir BASE node drain TEST_RUNNER --action reconcile
```

An empty certificate alone does not authorize disk deletion. Obtain provider
proof that the dev data disk survives instance removal (for the AWS component,
the dedicated encrypted data volume is already `retainOnDelete`). Keep identifiers
and proof in a private file outside the repository, for example:

```json
{"node":"TEST_RUNNER","dev_disk_retained":true,"proof":"private provider retention verification reference"}
```

Then supply the **exact** token printed by status:

```sh
ow --data-dir BASE node drain TEST_RUNNER --action retain --certificate TOKEN --evidence PRIVATE_FILE
ow --data-dir BASE node drain TEST_RUNNER --action remove --certificate TOKEN
```

`retain` records trusted operator/provider evidence against that certificate; the
core does not inspect cloud configuration. `remove` consumes a still-current
retained certificate and revokes that runner's controller enrollment. It does
**not** terminate a VM host, detach a volume, call a provider API or delete local
data. Provider destruction remains a separately verified action/adapter. Never
interpret enrollment revocation or a missing heartbeat as an empty certificate.

## Compatibility and checks

Release 0.9.13 is class 1: controller, agent, catalog readers and operator client.
Schema 6 raises the reader floor to 6 so older writers cannot ignore retirement
fences or GC roots. Existing private pre-placement backup machinery now also
copies schema-5 catalogs before migration. Coordinate catalog reader upgrades;
no downgrade writer is supported. Worker interface 3, accepted interfaces 1–3,
runtime, guest kernel and host layout remain unchanged. Keep worker pins intact;
this does not install a class-2 worker upgrade on a workstation.

Focused checks: `cargo test --locked -p ow --bin ow drain` and the host upgrade
integration tests. They cover cancellation/independent drain flags, late admission,
pre-admitted wake, stale connection/worker/catalog certificates, failed publication
and controller reopen, named snapshot publication, exact source cleanup, wake on
another runner, retention and removal of only the fixture runner. Durable catalog,
filesystem and socket fixtures do not establish KVM or cloud behavior.

The opt-in hardware fixture uses only fresh dedicated worker roots:
`transfers::vm_tests::drain_real_vm_wake_elsewhere_before_removing_only_the_test_runner`.
Set `OW_DRAIN_TEST_ROOT` to a nonexistent short private Btrfs path,
`OW_MOVE_TEST_BINARY` to the built Linux executable, and `OW_ASSET_DIR` to verified
Ubuntu assets on a KVM host. It creates its own test box and named snapshot, stops
and parks it, wakes it on its second worker, certifies/retains/removes only the
source enrollment, stops only that test worker and verifies target disk contents
and retained source snapshots. Local DirectoryObjects isolate store behavior;
actual provider retention/removal and S3 policy remain live acceptance gates.
