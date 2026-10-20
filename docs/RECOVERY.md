# Recovery points

Managed recovery is **opt in**, for the Firecracker worker. Existing hosts and
manual backup schedules keep their behavior until an operator enables it.
This is a disk recovery mechanism with optional compatible RAM continuation,
not automatic failover, a live move, or a guarantee that external TCP sessions survive.

## Enable and inspect

On a host whose worker supports `status.recovery_points_v1`, run:

```sh
ow backup recover --from /path/to/runner --to s3://bucket/private-prefix/snapshots
```

Run that command once a minute as the worker owner. It uses the existing AWS
SDK for Rust credential chain (credentials are cached and refreshed by the SDK), with checked
`--endpoint-url` support. Each runner needs its own prefix. A local advisory lock prevents overlapping recovery uploaders;
the worker serializes capture with other runtime operations. Uploading happens
outside the worker lock. Recovery operations do not count as guest activity and
do not prevent idle hibernation.

The AWS Host and Runner components expose `recoveryPoints: true`, requiring
`backupUrl`. This renders `ow-recovery.service` and a minute timer. The default
is false. `backupSchedule` continues to control the separate catalog/manual
snapshot backup. **Keep catalog backup enabled too**: workspace captures alone
do not preserve controller ownership, invitations, SSH authorization or placement.
Setting the flag applies timer/config files without restarting the worker.
Installing the new worker follows the normal class-2 hibernate/upgrade/resume
path; an older worker is refused before capture. No release tag or live upgrade
is implied by these source changes.
The uploader carries the listed placement authority for managed workspaces and
skips fenced or transferring copies. The worker validates that same authority
for prepare and acknowledgment, so a transfer during upload rejects a stale result.

`ow list` and `ow inspect BOX` expose `durability` in their existing JSON. The
dashboard shows its last durable capture time, pending upload, failures and retry
time. Timestamps are Unix seconds in the API/CLI; the dashboard shows the last
durable time as relative ("12 min ago", full local time on hover). A machine
less than an hour old without a point reads "First recovery point pending" in
neutral text; the warning colour appears only once the one-hour target is
missed or an upload error is reported:

- `last_durable_at`: capture **start**, only set after the remote manifest commits.
- `uploaded_at`: completion time; it never makes an older capture appear newer.
- `age_seconds`, `overdue`: age beyond the provisional one-hour disk recovery target.
- `pending`, `error`, `failures`, `retry_at`: persisted upload/capture retry state.
- `retention`, `semantics`: the current retention and recovery contract.

An absent durability field means the worker has not participated in recovery;
it does not establish that a backup exists. An offline runner's metadata may be
stale; the dashboard calculates age from the timestamp even while it is offline.
Failures also produce nonzero command/service exits and service logs:
`journalctl -u ow-recovery.service`. A lost connection to the worker can prevent
per-box error updates; monitor timer/service failure as well as box timestamps.

## Capture and retry

A running box is due immediately on first participation, then 30 minutes after
its last durable capture started. The existing capture syncs the guest, pauses
it, takes paired RAM/disk state and resumes it before hashing the sealed files.
Disk-only recovery is filesystem/crash recovery: applications may replay their
journals, and writes still in RAM require compatible RAM restore to continue.
Application-specific quiescing is not provided.

After ten minutes from the first observation of a sealed hibernation, the worker pins that sealed sleep capture under a
unique managed name. That observation is journaled, so worker restarts preserve
the delay; capture/hash time is not counted as time spent hibernated. A wake may delete the sleep directory while the upload
continues; the pinned file inodes remain valid. Each sleep is uploaded once.
Legacy sleep captures without a capture-start timestamp report zero (unknown) rather than claiming a measured durable age.
Stopped boxes do not create new points, but an already sealed pending upload is retried.

The worker records intent before capture. The same pending name survives capture
failure, interrupted uploads, worker restart and lost acknowledgments. Failed
attempts back off 60, 120, 240, then 300 seconds; an interrupted attempt waits at
least a minute. The timer retries after a run exits. A later success clears its
error. A destination change clears the old store's durable claim. Managed names
start with `sleep-recovery-`, are excluded from the user snapshot list/limit, and
cannot be used by a user snapshot request.
If a running capture failed before sealing and the box is no longer running,
its unusable intent is retired; any replacement uses a fresh name. A later hibernation follows
its own ten-minute sealed delay without inheriting the old attempt's backoff.
Sealed pending captures retain their identity and remain retryable.

The 30-minute interval leaves a provisional 30-minute capture/transfer budget.
A durable point matching the current frozen sleep state stays protected even
when its capture timestamp is older than an hour; no newer writes exist there.
The one-hour target is **not a measured service guarantee**: full RAM capture,
a first full upload, busy disks, serialized captures, queueing, network throughput
or failures can exceed it. One measured box (64 GiB disk, 4 GiB RAM, n=1) took
29 s per incremental point and 205 s for its first full point with 0.9.20
([timings](experiments/recovery-upload-timings.md)). Overdue status stays visible and never advances on
failure. Measure each intended host/cohort including capture, queue and upload
time before relying on that target. A worker upgrade still pauses that worker's
boxes through the existing upgrade policy; adding this timer never restarts it.

## Transfer format and retention

`SnapshotStore` remains the interface. `S3Store::incremental()` opts into the
recovery format; ordinary `backup snapshots` still writes the original compressed
whole-file representation so existing backup consumers keep working.

The recovery representation splits each sealed file into fixed 32 MiB chunks (8 MiB for older points),
hashes the uncompressed bytes, and uploads only absent nonzero chunks to
`PREFIX/chunks/SHA256.zst`. Zero chunks are represented without an object. The
unique `PREFIX/RECOVERY_NAME/manifest.json` contains `ow_chunks_v2`, the ordered
chunk hashes/lengths plus the file seals, and commits last.
Conditional `PutObject` prevents overwriting committed keys. Interrupted uploads
have no manifest and are not listed. Reads validate chunk size/hash and the
reconstructed file seal; sparse holes are preserved. There is no dependency
on a chain of older captures.

### Chunk seals: reading only what changed

Since 0.9.20 each managed recovery point has a chunk seal:
it writes the chunk list of every file to `chunks.json` beside the manifest and
seals each file under the SHA-256 of that ordered list (`"digest":
"sha256-chunks-32m-v2"` from 0.9.35, `"sha256-chunks-8m-v1"` in older
points; without that field a file hash is its
whole-file SHA-256, as for every other snapshot). The disk is a reflink of the
live disk, so after both disk files are flushed (pending writes are not in the
extent map until then) a disk chunk is not read when:

- the new disk maps it to no extent (a hole): it is a zero chunk; or
- the new disk and the last durable point's disk map it to exactly the same
  physical extents (Btrfs/XFS reflinks), and that earlier file still has the
  device, inode, size and modification time recorded in its `chunks.json`. Its
  hash is taken from that seal.

The memory file is matched the same way from 0.9.22: a capture lays its changed
pages over a reflink of the previous image, so unchanged RAM shares extents with
it. Anything else is read and hashed in parallel: changed disk and RAM ranges, the
device state file (written whole by every capture), and every range whose
mapping is unknown, delayed, compressed, encrypted or inline. Without an earlier
seal (first point, a changed destination, or a durable point from an older
worker) the disk and RAM files are flushed and only their holes are skipped (zero
chunks, unread, from 0.9.35); every mapped chunk is read, and zero chunks are still
recognized without hashing. This trusts the
filesystem's extent sharing, as changed-block tracking does; a worker that
verifies such a snapshot again (after a restart, or for import) reads every byte
and compares the result with the seal.

From 0.9.35 a capable worker advertises `recovery_client_seal_v1`. The uploader
requests a pinned unpublished capture, then hashes changed chunks on parallel
threads, compresses them with zstd level 3 and sends missing chunks immediately:
one disk read rather than a seal pass followed by an upload pass. It returns the
file seals to the worker, which checks their file identities and complete chunk
lists before committing the local snapshot. Only then does the uploader publish
the remote manifest. An upload interruption leaves no published recovery point;
a retry uses the same pinned capture while the worker is alive. Worker restart
discards unsealed staging and recaptures from an available source; already stored
content-addressed chunks remain reusable. A lost seal response is reconciled
through the committed local manifest. The worker still checks placement authority
for sealing and acknowledgment.

Older clients and workers use the existing seal-then-upload path and 8 MiB seals. Both
`sha256-chunks-8m-v1` and `sha256-chunks-32m-v2` points are readable by this
release, as are original whole-file backups. A format transition hashes the first
point in the new chunk size; subsequent points reuse its unchanged extents.

`OW_S3_TRANSFERS` (1-128, default 16) bounds upload and download parallelism.
Recovery workers are additionally limited to `transfers × 16 MiB / chunk size`
(at least one), so default 32 MiB points use eight workers. Each holds one chunk
and its compressed copy, plus SDK buffers. Hashing and network I/O overlap across
workers; the optional compression and SHA-256 format remain unchanged.
Points already sealed by the worker read only missing chunks, checking each
against its hash before upload. Restore verifies every downloaded chunk and the
file seal, and removes an incomplete destination on failure.

### Chunk index and resumed runs

Before each point the uploader lists `PREFIX/chunks/` (16 prefix listings with bounded parallelism,
1000 keys per request) and takes that listing, not any local memory, as what the
store holds. It records the listing in `RUNNER/recovery-index/STORE_IDENTITY.chunks`
and appends each chunk as soon as the store confirms it. No chunk is checked with
its own request. A run killed or failed midway therefore resumes: the next run
sends only chunks still missing, then the manifest. A chunk removed from the store
by anything else is sent again before a new manifest names it, and a point whose
manifest is already stored has any missing chunks sent again from its sealed
files (`index_dropped` counts recorded chunks the listing no longer shows).
Nothing in Open Workspaces deletes chunks. Each run prints, per point, what the
capture read (`capture`: chunks read, reused, holes, bytes read) and what the
upload sent (`transfer`: chunks, zero, new, uploaded, bytes uploaded and read,
already_stored, index_dropped, seconds).

Managed points use the existing internal `sleep-` namespace, so an older worker
still excludes them from named-snapshot admission after rollback.
The new reader accepts both formats; old readers cannot download chunk-format
points. Upgrade a recovery destination's CLI before using those points. The
`manifest()` trait method strips transport metadata, preserving original seal
identity for existing callers and runtime import.

Before each incremental push the uploader verifies bucket versioning is enabled,
no overlapping current-version age expiry exists, and matching noncurrent-version
expiry is at least seven days. Missing permission or an unsafe lifecycle fails
visibly before upload; it is never interpreted as an empty store. The host role
needs `s3:GetBucketVersioning` and `s3:GetLifecycleConfiguration` alongside existing
list/read/write permissions, with no object/version delete privilege.

Off-host points and chunks are **kept until explicit catalog-aware garbage
collection**. No deletion/GC implementation is introduced here; shared chunks
must not expire by object creation age. This is deliberately visible retention,
not a claim of bounded S3 cost. Use the artifact-store foundation's safe lifecycle
before enabling recovery; its namespace remains separate under `store/`.
Versioning supplies at least seven-day undo for overwritten/deleted objects;
operators restore a previous object version with their own authorized credentials.
The uploader cannot delete versions. Seven-day undo does not replace live-object
retention. A different lifecycle applied later can invalidate this contract;
subsequent uploads refuse it, and operators must repair the policy promptly.

Locally, successful acknowledgment retains the latest durable managed capture
and one pending capture. A manifest marker identifies managed captures; an older
manually named snapshot whose name resembles a managed name is preserved. Older
managed captures for that box are removed only after success. Named/manual snapshots and active hibernation references are
preserved. Failed transfers retain their pending files; low storage fails capture
rather than deleting the last durable point. Deleting a box removes its local
recovery files/journal; its off-host copies remain subject to the policy above.

## Restore and salvage

On a **different** isolated runner directory, with sufficient disk and RAM:

```sh
ow backup list s3://bucket/private-prefix/snapshots
ow backup restore RECOVERY_NAME --from s3://bucket/private-prefix/snapshots \
  --to /path/to/other-runner --cold
ow --local --data-dir /path/to/other-runner exec PHYSICAL_ID -- 'cat /path/to/marker'
```

Start the destination worker first. `--cold` boots the captured disk and loses
RAM processes; without it, runtime, CPU/host kernel and guest kernel must match.
Import refuses an existing workspace ID. Download verifies bytes before import
and removes a failed destination. Controller catalog adoption and placement
fencing belong to the separate fleet workstream: never start a second copy of a
live workspace ID in a shared production fleet. The source must be fenced before
an operational recovery. Local restore alone does not grant dashboard ownership
or restore guest SSH enrollment.

AWS components use a separate encrypted Btrfs EBS data volume, which is not the
instance's root block device. It survives instance termination/replacement;
`retainOnDelete` also preserves it when Pulumi removes the volume resource.
Detached salvage volumes continue incurring storage cost until an operator
explicitly disposes of them. Preserve the volume ID from outputs, attach it to an
authorized isolated host in the same availability zone, mount it without starting
workers, copy/verify the desired data, then decide whether to import. Never attach
or start a second writer against a live runner's volume. This source change does
not terminate instances, detach volumes or change running workspaces.

## Validation boundary

The credential-free transfer integration test exercises the real adapters
against loopback S3 fixtures: a one-chunk edit
transfers one new chunk, manifest failure/retry transfers no unchanged chunks, no
chunk is checked with its own request and no AWS CLI process runs, a run
killed after two uploads resumes with only the rest, a lost index is rebuilt from
a listing, a chunk-sealed point whose bytes changed after sealing is refused,
another runner directory receives identical bytes, legacy reads work and
corruption is rejected. Unit tests cover chunk reuse only for proven extents
(and its equality with a full read), and a real reflink clone where available. Worker tests cover
sleep pinning through wake, persisted retry, stale result rejection, idempotent
acknowledgment, local rotation and changed destinations. Host render tests prove
opt-in timer configuration leaves the worker unit unchanged.

These checks are not live S3/IAM, real VM restore, RAM compatibility or a measured
one-hour recovery result. Use `scripts/recovery-acceptance.py` with two dedicated
workers and a private authorized store for that acceptance; it verifies a disk
marker, cold restore and compatible RAM continuation separately. Keep live gates
and evidence distinct from source/test completion.
