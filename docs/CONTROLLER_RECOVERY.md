# Replace one controller, keep the runners

This is **single-controller recovery, not HA**. A controller outage interrupts
admission, exec/terminal/SSH streams and preview access. The independent workers
and already-running VMs keep running, subject to their existing idle policy.
It does not preserve TCP sessions. Do not stop the worker, supervisor, instance,
or host to replace its controller. Never apply these steps to a shared host
without a separately reviewed, explicit cutover plan.

Pattern: [ADR 0002](adr/0002-bounded-linux-multinode.md) and
[ADR 0005](adr/0005-change-classes-and-engine-boundary.md), one SQLite authority,
outbound authenticated runners and independent service pins.

## Gates and tools

Land this tooling dormant. Use disposable fixtures first. Destination capacity,
private trust/secret transfer and endpoint/admin policy changes require their own
operator authorization; this guide grants none. Never publish catalogs, receipts,
credentials, configuration, TLS keys or host inventories. Real secrets belong in
the operator's approved secret manager. No tenant-specific deployment wiring is
part of this repository.

`deploy/aws/host/cutover.py` is a private, persistent **checklist**. It never
starts controllers, transfers secrets, configures networking or proves that an
operator's fencing receipt is true. It serializes its own journal only. A
host-local flock, missing heartbeat, expired lease, stopped shell or receipt file
is **not** a cross-host fence. Reject overlapping writable controllers even if
they use different catalogs or ports. Independently verify a provider-level
control-plane isolation or a persistent service **and network** isolation of the
old authority, covering CLI/admin/maintenance writers, reconnects and reboot.
Do not fence the old worker's networking or turn off the old machine.

`ow-host recovery-freeze` installs persistent systemd conditions, then stops
controller/dashboard, backup/recovery/image/upgrade and known GC/scaler units and
timers. It leaves the worker, its pin, node credential, VMMs and guest disks alone.
The marker blocks normal `ow-host` mutation commands and survives reboot. It
cannot stop a root operator invoking a binary directly, another machine or an
external CI job: pause those separately. Inventory/checks are never permission
to delete, adopt or start anything. Add any future maintenance writer to the
freeze inventory before using this procedure with it.

`ow-aws --host NAME recovery-status` reports the stopped units, node and worker
pin. `cross_host_fenced` is always false: independent fencing remains required.
The remote commands print no catalog rows or TLS material. Paths passed to them
are **on the target host**, never local download/upload paths.

## Ordered cutover

Keep a private mode-0700 directory on durable operator storage. Use one checklist
for this cutover. A status check is a dry run: no service or provider effects.

```sh
python3 deploy/aws/host/cutover.py init /private/cutover/journal.json --drill disposable-unique-id
python3 deploy/aws/host/cutover.py status /private/cutover/journal.json
```

Each transition consumes a mode-0600 JSON receipt through
`advance JOURNAL --receipt RECEIPT --ow /path/to/ow`. Receipts include `drill`,
`stage`, `evidence` (private log references) and the fields below. Boolean checks
must be `true`, unresolved counts must be integer zero. Store evidence outside
this repository. Stop on a failed check; never edit the journal to skip a stage.

| Stage | Action and required receipt evidence |
| --- | --- |
| `frozen` | Pause deploy/upgrade/config automation and all out-of-band catalog writers. Freeze **both** controller hosts; check inactive units and no remaining writer processes/listeners. `old_stopped`, `replacement_stopped`, `maintenance_disabled`, `out_of_band_writers_stopped`. |
| `isolated` | Positively exclude old authority return without stopping its worker. `old_autostart_disabled`, `external_fence_verified`, `fence_method` (`provider-control-plane-isolation` or `service-and-network-isolation`), `fence_reference`. |
| `catalogued` | Take the **final consistent** private backup after stopping writers; check every table and schema object. `catalog` path, `backup_gap: "none"`, `all_ownership_and_roots_resolved`. |
| `restored` | Install final catalog into an isolated private replacement controller directory; compare it before starting anything. Preserve approved TLS trust/config separately; matching CA must validate the destination name. `catalog` path, `private_trust_and_config_verified`, `same_runner_credentials`, `reader_compatible`. |
| `write_possible` | Reverify the external fence. `old_fence_rechecked`, `replacement_still_stopped`. **Persist this stage before any replacement process starts**, including a listener whose sessions/heartbeats write SQLite. From here, even a startup crash means latest-replacement-only rollback. |
| `reconciled` | Start only replacement control-plane services with GC/scaler/admission still blocked externally. Repoint existing agents; validate every credential at the intended URL. `credentials_reconnected_to_replacement`, `stale_sessions_rejected`, `stale_operations_rejected`, `all_ownership_and_roots_resolved`; zero `unknown_running_copies`, `contradictory_running_copies`, `unresolved_operations`. |
| `open` | Complete the disposable live checks below and rollback checks, then reopen admission. `worker_and_guest_identity_unchanged`, `strict_host_key_ssh`, `preview_access_and_revocation`, `rollback_before_write_checked`, `rollback_latest_catalog_checked`. Maintenance stays off until its own roots/ownership validation succeeds. |

Example receipt (use actual stop/fence evidence, not this placeholder):

```json
{"drill":"disposable-unique-id","stage":"frozen","evidence":"private stop/process logs",
 "old_stopped":true,"replacement_stopped":true,"maintenance_disabled":true,
 "out_of_band_writers_stopped":true}
```

The journal atomically persists each completed checkpoint. If a crash occurs
before it advances, repeat/check the pending step and its side effects; if after,
`status` names the one next step. Host freeze and same-input runner configuration
are retryable. A crash after starting the replacement always follows the
`write_possible` path; never downgrade that stage. An interrupted unknown
operation stays blocked until reconciled. Do not replay arbitrary exec or resolve
an uncertain start by issuing another start.

### Catalog and trust continuity

On the source (the backup command is a consistent SQLite read, not a live `cp`):

```sh
sudo ow backup catalog --from /srv/ow/.ow/controller --output /srv/ow/final.sqlite3
sudo ow backup catalog-check /srv/ow/final.sqlite3
```

Follow [CORE](CORE.md#catalog) for a private marker-bearing restore directory,
ownership/mode 0600, and exclusion of stale WAL/SHM files. Never overwrite the
only copy. Use the same schema-compatible binaries; schema 6 / reader floor 6
must not be opened by an older writer. This release adds no migration.

```sh
sudo ow backup catalog-check /srv/ow/restored.sqlite3 --against /srv/ow/final.sqlite3
# Or remotely, without moving private contents through SSM output:
ow-aws --host replacement recovery-check /srv/ow/restored.sqlite3 --against /srv/ow/final.sqlite3
```

The check opens read-only, takes one read transaction, checks SQLite integrity,
foreign keys, reader compatibility and the epoch high-watermark, then compares
schema objects and sorted, typed row hashes for **every persisted table**. It
covers owners/names/grants, named snapshots, publication/SSH policy generations,
node credential hashes/revocations, placement epochs/current captures, transfers,
locks/reservations, artifacts/copies/leases/images, drain and GC journals and future
tables. It emits counts/digests, never rows. No equality claim covers writes
that happened after the reference backup or before independently stopping writers.

An older backup is **non-destructive recovery only**. Keep all writers, admission,
GC and scaler off. Missing backup-gap ownership, parked captures, named snapshots,
leases, tombstones or epoch allocations cannot be recovered from worker inventory
alone. Preserve the highest known epoch; do not reset counters, lower the reader
floor or guess owners. Reconstruct **every** root and owner from durable records
and resolve contradictions explicitly, including roots on no runner. Only then
supply a new consistent reconstructed reference with `backup_gap: "resolved"`,
`gap_resolution` identifying the approved resolution and
`resolution_includes_parked_roots: true`. These are operator attestations, not
an automatic reconstruction facility. Unresolved/unknown running copies block
admission and all destructive loops regardless of a successful integrity check.

TLS CA, server key/certificate, dashboard auth/config, limits, store configuration
and transport URLs live outside SQLite. Transfer them only over an approved
private channel after authorization. Retain the CA where possible; mint a leaf
valid for the destination name through the approved process. Never use an
insecure TLS switch. Do not print keys or credential hashes into evidence.

### Preserve the old host as a runner

The old combined host becomes `role: runner`, **not** controller-only. Its runner
node ID and `node.json` credential stay identical. Preserve its worker unit,
worker environment, budget, image/runtime, executable and `run/worker` symlink.
Prepare a private runner config using the existing host config, change only the
role/runner authority and remove dashboard settings. Preserve the existing
release, tailnet, images, idle policy, overcommit and store. Use an approved
existing join-secret reference for the runner schema; the command does **not**
read/redeem that secret or enroll again. Keep deployment's desired config in sync
through its separately authorized workflow before re-enabling automation.

```sh
ow-aws --host old-host recovery-freeze
ow-aws --host old-host recovery-runner --config /private/runner.json \
  --controller https://replacement.example.test:8790 --ca /private/controller-ca.pem
```

`recovery-runner` requires all control writers stopped, checks that installed
worker files match the preserved rendering, rejects credential/node changes and
journals its exact inputs before writes. Retry with identical inputs after a
crash. It discards stale agent readiness reports, writes the URL/CA and config,
removes only obsolete control-plane services and **starts nothing**. Repoint
other enrolled runners with the same command using their existing runner config.
A failed TLS/credential/readiness check is a stop, never a reason to enroll a
second identity or lower host-key/TLS checks.

The recovery marker deliberately has no automatic thaw. After the checklist's
`write_possible` checkpoint and independently verified isolation, operators may
remove only `90-controller-recovery.conf` for the specific agent/dashboard units
being reopened, run `systemctl daemon-reload`, then start **only** those units.
On the old host that agent unit must already render `node-agent`, never `host up`.
Keep the marker and every maintenance drop-in until reconciliation finishes;
then remove that host's marker and the remaining recovery drop-ins under the
reviewed plan. Verify readiness against fresh agent evidence and the destination
catalog. Never run `boot`, `restart --worker`, reboot or instance stop here.

## Rollback, including uncertain first write

Before replacement startup: both writers stopped and fenced, restore/compare the
final pre-write catalog to the old authority; reverse only control-plane config,
reconcile, then reopen it. Do not restart the old combined unit after converting
it to a runner without explicitly restoring its preserved control-plane config.

After `write_possible`: stop **both** authorities and every maintenance writer;
take a fresh consistent catalog from the replacement **after** those stops.
Restore this latest catalog and compatible configuration to the chosen authority.
Never restore the pre-cutover copy, even if the new listener only accepted a
heartbeat or its startup result was lost. Preserve the new epoch allocations,
ownership, SSH/publication revocations and all roots. An inaccessible latest
catalog means rollback is blocked, not permission to use a stale backup.

`rollback-check JOURNAL --receipt RECEIPT --ow BINARY` compares `latest_catalog`
and `restore_catalog`. The receipt requires `drill`, `evidence`, `old_stopped`,
`replacement_stopped`, `maintenance_disabled`, `external_fence_verified`. After
possible writes also require `source: "replacement"` and
`taken_after_both_stopped: true`. It never starts a service or rewinds the journal;
a successful rollback check is not a completed rollback. Use a fresh checklist
and fence evidence before reopening the recovered authority.

## Disposable drill and limits of proof

Automated, no KVM/cloud/real secret use:

```sh
python3 -m unittest discover -s deploy/aws/host -p 'test_controller_recovery.py'
cargo test --locked -p ow --bin ow backup::tests
cargo build --locked -p ow --bin ow
# Run only in a dedicated non-KVM Linux fixture:
OW_NODE_TEST_BINARY=/path/to/ow cargo test --locked -p ow --bin ow \
  replacement_controller_restores_catalog_and_existing_credentials -- --ignored --nocapture
```

The TLS drill restores a consistent catalog into another controller root, stops
the old listener first, reconnects two existing credentials with unchanged agent
PIDs and synthetic worker inventory, rejects a stale job bridge, makes a new
write, rejects the stale pre-cutover catalog and checks a latest backup. Host
fixtures test freeze/conversion interruption and preserve worker pin/files and
credential. Checklist fixtures crash at every checkpoint and reject dual
writability receipts, stale gaps, unknown copies and stale rollback.
**Synthetic inventory is not a guest PID, SSH or preview test.**

For full fleet-foundation acceptance, run these additional steps on separately
authorized dedicated KVM capacity with only disposable boxes; retain private
before/during/after evidence. Do not mark the live `open` receipt or enable a
scaler from synthetic test results:

1. Record host boot ID, worker PID/start ticks, `run/worker` target/executable
   hash, node ID, VMM PID/start ticks and a long-running guest counter/PID. Create
   parked and running boxes, named snapshots, sharing, SSH policy, a preview,
   active artifact lease and a safely paused transfer/reservation fixture.
2. Generate pinned SSH config with `ow ssh-config`; prove
   `ssh -o StrictHostKeyChecking=yes -F PRIVATE_CONFIG ALIAS ...` and preview
   authenticated access. Preserve the known_hosts file unchanged.
3. Freeze/isolate the controller. Observe through the worker host that VMM/PIDs
   and guest counter continue while controller access/control fail. Record the
   failure; do not restart or stop that worker to manufacture recovery.
4. Complete catalog/trust restore and old-host runner conversion; repoint agents
   and reconcile. Compare all catalog roots, credentials and epochs; challenge
   an old session/job and old placement operation. Unknown/contradictory copies
   must remain blocked. Verify all recorded process identities and hashes.
5. Strict-host-key SSH reconnect must succeed with the same known_hosts; guest
   PID/counter continues. Verify preview access, revoke it, and verify denial
   after reconnect. Recheck owner/name/grant/snapshot and SSH policy isolation.
6. Exercise rollback before startup and after a disposable new write, always
   both writers stopped; reject the stale copy after that write. Take another
   latest backup and restore it on the replacement. Reconcile again before
   admission. Record outcomes, versions, fixture host capabilities and remaining
   gaps without publishing private data.

No provider creation/destruction, S3 policy validation, real-VM continuity,
shared-host cutover or automatic stale-backup reconstruction is established by
the repository's synthetic drill. Those remain explicit activation gates.
