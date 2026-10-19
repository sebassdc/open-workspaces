# Local cache and placement

The sealed-image cache extends the write-once [artifact store](ARTIFACT_STORE.md).
It does not change an existing box's placement, delete live machine disks, or
activate managed park/wake/move. Those operations retain the gates in
[LIFECYCLE.md](LIFECYCLE.md).

## Verified hints

A negotiated Linux runner reports at most 32 entries: immutable manifest digest,
local image selector, logical size, exclusive bytes when available, last-use time,
and local pins. Disk digests are verified before advertisement. A verification
stamp includes device, inode, length, modification and change timestamps including
nanoseconds; any change requires hashing again. A corrupt or unknown image is
omitted. A `.pin` file inside a sealed image directory pins that local copy.
Only a trusted host operator can write this private directory.

Btrfs `filesystem du --raw -s` supplies exclusive extent bytes. Other filesystems
report an unknown value; `st_blocks` is never substituted for exclusive ownership.
A reflink shared with a live disk can have almost no exclusive data. Inventory is
a hint: an absent copy is downloaded and digest-verified again before create.
A corrupt existing version fails closed and is preserved for operator inspection.
Existing local snapshots and recovery captures have a separate snapshot-store
contract; they are all excluded from cache deletion. They are not advertised as
format-1 committed capture hits without an adopted artifact reference.

## Claims, deletion and recovery

Before an image pull needs space, the controller asks for a fresh worker inventory
and considers entries in ascending last-use, digest and selector order. A
`BEGIN IMMEDIATE` claim changes a known stored `artifact_copies` row to `evicting`
and persists a unique token. It refuses active leases (including parent leases),
current images with prefetch enabled, pending/uncertain creates that need the
image, placed captures and unfinished transfer roots. Unknown, unstored and local
pinned data cannot become deletion candidates.

SQLite triggers reject new leases, image-prefetch roots, create uses and
placement/transfer references while that claim exists. They also stop a heartbeat
or prefetch result from resetting the copy to `present`. This protection survives
controller restarts and is enforced for prior schema-5 writers as well. No remote
request runs inside a SQLite write transaction.

The worker takes exclusive local `flock` exclusion before deletion. Image installs,
base-template copies and runtime requests hold shared exclusion; runtime disk
allocation and staging admission also share one space-admission lock. A busy
local use rejects eviction. The worker fetches the committed remote manifest by
the pinned digest immediately before unlink, checks selector and local file
identity, and accepts only the known sealed-image filenames, including regular
`.verified.tmp-` files with the verifier's exact 24-character lowercase hexadecimal
nonce. These interrupted verification stamps are discarded only under eviction
exclusion; near-matching names, symlinks and directories remain protected.
It renames the copy
to private digest-named trash before removing it. An interrupted unlink can be
reissued with the same claim. Completed worker claim tokens are durably recorded: a late duplicate cannot delete
a copy reinstalled under a later lease. A lost reply keeps the catalog exclusion indefinitely;
there is no timeout that makes an uncertain copy usable. The next capable pull
reconciles exact pending claims before requesting a new lease.

After each deletion, `syncfs` and `statvfs` measure actual available space. The loop
never credits a file's logical size or predicted exclusive bytes as reclaimed
space. The delta can include unrelated filesystem activity; admission always
rechecks current free space. At most 32 candidates are considered. If they do not
free enough, no download starts; safely deleted cache copies remain recoverable
from their committed artifacts.

## Space admission

Image and capture pulls share one staging budget. Before downloading file objects,
each reserves its full verified uncompressed logical size until installation or
cleanup; captures include disk, VM state and RAM files in that total. Process-held
reservation locks let concurrent pulls sum their outstanding demand and recognize
abandoned records after a crash. Partial files still occupy
physical space. Under the admission lock, a pull re-reads machine disk lengths
rather than trusting a preceding worker status. Existing full logical disk
reservations, the configured minimum free space, and one host-memory-sized capture
reserve remain protected. Runtime create/resize/capture checks include in-flight
staging reservations. `statvfs` also counts unlinked but mapped RAM files: unlinking
one is never treated as freeing its data while a VMM still maps it.

The optional policy function accepts minimum-free, capture reserve, staging,
trigger and stop bytes. The proposed extra 10 GiB trigger / 20 GiB stop can be
exercised as inputs; it is not enabled as a new default. `OW_MIN_FREE_GIB`,
`OW_MAX_DISK_GIB`, CPU/RAM budgets and slot limits are unchanged.

## Deterministic placement

Eligibility precedes cache scoring: online and active, requested pool, both runner
upgrade drain and controller drain, operation/worker capability, compatibility,
guest shape, CPU, RAM, slots and logical disk headroom. Committed owner allocations
and transfer reservations supplement logical disk demand not yet visible in a
worker report. Unknown or stale disk reports cannot admit an explicitly sized
create. Existing legacy fixed-placement workers keep their established worker-side
admission contract; unknown cache inventory never grants managed-work capability.

Among eligible nodes, prefer the requested capture digest, then an image or parent
digest. Only reports at most 15 seconds old count as hits; a future timestamp does
not count. Then prefer least remaining RAM, CPU, slots and disk bytes, in that
order, followed by older enrollment order and node ID. A stale copy can change
which bytes must be pulled, but cannot grant authority or bypass capacity.

The controller owns optional `node_placement_policy` pool/active/drain values;
heartbeats cannot clear them. No public pool or drain administration is activated
here. Full drain lifecycle, reader-floor requirements when activating that policy,
and managed placement orchestration remain separate lifecycle work.

## Compatibility and release impact

Controller inventory/ranking is class 1. Real cache eviction, verified worker use
and staging accounting are class 2: use the existing quiesce/hibernate/swap/resume
upgrade and compatibility checks before enabling them on a runner. A class-1 agent
update alone cannot grant an older worker `cache_eviction_v1`.

The host-runner protocol remains 1 and the worker interface window remains 1–2.
The extension is explicitly negotiated: a controller advertises
`cache-inventory-v1` in its existing optional `ready` acknowledgment; the agent
sends the larger bounded cache shape only afterward and only for a worker that
advertises enforcement. The node control frame is bounded to 32 KiB and entries
to 24 KiB. Prior controllers receive the prior shape. Legacy runners remain
accepted and never receive the new `cache-inventory` or `cache-evict` operations.
Catalog schema/read floor remains 5: the claim table and guards are additive, and
SQLite enforces their exclusions for older compatible writers. Runtime, guest
kernel, host layout and stage 0 are unchanged from v0.9.4. Release v0.9.7
declares the cache worker semantics as class 2; update the controller, dashboard
and agents, then apply the normal worker upgrade action before using eviction.

## Reproducible checks and timing limits

```sh
cargo test --locked -p ow-core cache::
cargo test --locked -p ow --bin ow cache_catalog::
cargo test --locked -p ow --bin ow placement::
cargo test --locked -p ow --bin ow cache_shape
# Only on a disposable Btrfs filesystem owned by the test:
OW_CACHE_BTRFS_ROOT=/DISPOSABLE/MOUNT cargo test --locked -p ow-core cache_btrfs -- --ignored --nocapture
OW_CACHE_BTRFS_ROOT=/DISPOSABLE/MOUNT cargo test --locked -p ow-core cache_unlinked -- --ignored --nocapture
```

Image-pull replies record `measurement.method`, uncompressed logical bytes, `n=1`
and elapsed milliseconds, distinguishing a verified local image from a verified
zstd pull. This measures image preparation, not guest readiness. No cold-wake
latency is claimed. A future managed cold-wake trial must use disposable boxes and
record binary/runtime/kernel versions, host CPU/filesystem/network, image/capture
sizes, cache state, method, sample count and individual pull/import/start/readiness
timings before reporting p50/p95. Synthetic placement and Btrfs tests do not prove
VM recovery or a latency promise.
