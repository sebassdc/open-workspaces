# Write-once artifacts and shared images

The opt-in image store is separate from `SnapshotStore` backups. Existing backup
commands and snapshot directories continue to work. No service, live disk, timer,
runner or guest is changed merely by installing the source or publishing an image.
This is a foundation, not automatic off-host protection for running boxes.

## Storage contract

`ow_core::store` accepts sealed files and a controller-issued 128-bit upload
attempt. Files are streamed through SHA-256 and zstd, into write-once objects at:

```text
s3://BUCKET/PREFIX/store/v1/u/SHARD/32_HEX_TOKEN/disk.ext4.zst
s3://BUCKET/PREFIX/store/v1/u/SHARD/32_HEX_TOKEN/manifest.json
```

The last object is the commit marker. Its canonical JSON digest is pinned in the
catalog; it records each file's uncompressed digest and sizes. Failed attempts
remain unreferenced. A retry may confirm identical existing bytes; it cannot
replace them. A fresh attempt uses a new token. Format 1 has one object per file,
with no cross-artifact chunk sharing. The existing snapshot adapter can evolve
independently under its own backup namespace.

Object transfers run in process: one `aws configure export-credentials` call
takes the AWS CLI's usual credential chain (refreshed every five minutes), and
each request is signed in process on a shared connection pool, with retries and
backoff for throttling, server errors and lost connections. zstd compresses on
several threads (the same zstd stream format). Uploads send compressed 64 MiB
parts with SHA-256 checksums, several at once (`OW_S3_TRANSFERS / 4`, default 4;
each in-flight part holds 64 MiB of memory), and commit them with a conditional
`CompleteMultipartUpload`; a short object is one conditional `PutObject`. The
10,000 part limit caps a compressed object at 625 GiB in this implementation. A
lost completion response is checked with HEAD, including composite SHA-256 and
length. Downloads read 16 MiB ranges in parallel (`OW_S3_TRANSFERS / 2`, default
8), all pinned with `If-Match` to the ETag first seen, and reassemble them in
order; the uncompressed SHA-256 is still checked against the manifest.

Before image publication/build, the application checks bucket versioning,
lifecycle, and the write-once policy. Missing read permissions or an unsafe bucket
fail closed. There must be no current-object age expiry covering the store, at
least seven days of noncurrent undo, and a conditional-creation deny applying to
all principals. See [ADR 0006](adr/0006-write-once-artifact-store.md).

## Workspace capture adapter

Managed restore consumes committed `capture`/`snapshot` artifact rows already bound
to an existing catalog owner and physical machine. It never enumerates storage to
create that binding. Format-1 `metadata.native_manifest` contains the exact native
snapshot JSON as a string; its bytes are preserved when staging the worker capture.
The adapter validates its name, workspace, memory, CPUs, disk size, complete file
set and hashes against the catalog intent and pinned outer manifest. No owner field
inside either manifest grants ownership. RAM captures require all three files:
`state`, `memory`, `disk.ext4`; disk-only captures contain only `disk.ext4`.

Private `capture-pull` and `capture-cache` node jobs reuse the object store's pinned
reference and validated pull staging. Per-attempt leases/tokens live in the existing
catalog tables; retries inspect the verified cache before allocating another attempt.
These byte transfers and hashes run in the agent, outside the serial worker. Native
`import-prepare` remains unstarted until conditional catalog adoption and activation.
This adapter does not index or convert legacy `SnapshotStore` backups; their existing
publication and retention stay separate. See [restore lifecycle](LIFECYCLE.md).

## Publish once, pull by digest

These are trusted Linux operator commands on an initialized ownership controller.
Use its normal `ow` wrapper, or set `--data-dir` to the host base directory and
`OW_ASSET_DIR` to the worker's runtime assets. AWS credentials stay on the host,
through its instance role or the normal CLI credential chain; they never enter a
guest or a job payload. There is no public image administration endpoint.

```sh
# Publish an already prepared Ubuntu base from this host's runtime assets.
ow image publish ubuntu --to s3://BUCKET/PREFIX

# Display every catalog version and its upload token/digest, including history.
ow image list --catalog

# Run a trusted recipe once on the chosen runner, using an immutable base.
ow image build recipes/tools.sh --publish-to s3://BUCKET/PREFIX \
  --base-image ubuntu@1 --node RUNNER_ID

# The existing local-only builder also continues to work.
ow image build recipes/tools.sh
# To publish a local golden image, explicitly record its pinned base provenance.
ow image publish tools --to s3://BUCKET/PREFIX --base-image ubuntu@1

# Install a catalog image on a local runner; the disk is verified and sealed.
ow image pull tools@1 --runner /PATH/TO/RUNNER
```

Recipe headers retain `# ow-image: base=ubuntu` or `base=arch`. Recipes run as root
in a disposable guest. They must not copy credentials or personal state into a
shared image. Builds are deduplicated by name, recipe SHA-256 and base manifest
digest. Concurrent identical builds share the same durable intent/runner. Changed
bases make prior golden versions `stale` in the catalog list.

The controller stores a monotonically increasing `name@N`. One version is current,
two predecessors are kept for rollback, and older ones are retired. Retired
versions cannot be selected for a new create. Expired uploads become failed
history; retries allocate a fresh version and token. Artifact rows are retained.
An uncertain interrupted recipe is not executed again automatically: inspect its
private `image-jobs/TOKEN` marker and deterministic `img-FIRST_16_TOKEN_HEX`
builder. Sealed build results can be reuploaded without rebuilding. The push
lease lasts two hours; controller recipes are bounded to one hour.

An authenticated user can create from `tools` (current) or `tools@1` (current/kept).
The first create pins the version, including retries after current changes. The
worker records `image_digest` in workspace/snapshot metadata. Older unversioned
boxes preserve their original image choice. Existing boxes own independent disks
and do not depend on keeping their source image cached.

## Runner distribution

On an unmanaged runner, configure a single store root in its private
`RUNNER_DIRECTORY/store-url`. Managed AWS hosts read their rendered
`/etc/open-workspaces/store.json`. A capable agent advertises verified
`image_digests` alongside the existing string `images` list. Legacy agents remain
accepted with unknown digests; placement does not send them an uncached store
image. A new worker advertises the optional image-store capability.

Current images prefetch after a capable runner joins, at most two at once. A create
on a capable runner without an image pulls it before dispatching create. The
controller holds a pull lease and records successful prefetched copies. Failed
prefetch leaves the node joined and the on-demand path can retry. Files download
to a private new staging directory; bad digests, unsupported formats or unsafe
keys remove staging. Hidden staging/build directories are not advertised.

A standalone pull needs the exact token and manifest digest printed by publish:

```sh
ow image pull ubuntu@1 --from s3://BUCKET/PREFIX \
  --attempt UPLOAD_TOKEN --digest MANIFEST_SHA256 --runner /PATH/TO/RUNNER
```

`--base-assets /PATH/TO/ASSETS` additionally replaces a built-in template by atomic
rename. It checks the artifact's guest-kernel digest against these runtime assets;
existing boxes remain independent. The profile metadata records the manifest
digest. Transfers and recipes run in the agent, outside the serialized worker.

`ow node add TARGET --from-store` uses the controller's current Ubuntu base. It
copies the small runtime/network assets and pulls the compressed disk using the
remote machine's own AWS credentials. It does not transfer local credentials.
`--ubuntu` retains the existing direct-copy path; the flags conflict.

## AWS component and activation

`ArtifactStore` enables versioning, private access and TLS, and adds the write-once
policy under `PREFIX/store/v1/`. Only catalog backups age-expire. Current objects
under `snapshots/`, `runners/` and `store/` remain until explicit controller GC;
noncurrent versions retain at least seven days of undo. The store/runner role has
get, conditional put, abort and bucket safety-read rights, no list or delete.
Backup role access is narrowed to its `catalog/` and `snapshots/` children so it
cannot grant store listing accidentally. Controllers additionally have read/list
access to `runners/*/snapshots/` for existing backup listing/restore commands;
runner roles retain only their own backup namespace. No role receives DeleteObjectVersion.

`Host` and `Runner` have optional `storeUrl` and `imageArtifacts` inputs. A base pin
is `{version, attempt, digest}` keyed by profile. With store enabled, each requested
`developerImages` profile must have a pin; its existing preparation oneshot pulls
it instead of building it on every volume. Missing/bad pins fail visibly. Omitted
store inputs preserve the local-build path. The config change does not alter
stage 0, replace a host or restart its worker.

Activation must first apply safe bucket policy/lifecycle and IAM, then select a
release containing this code, then configure the store/pins. The worker's new
version selectors require a normal class 2 worker upgrade; controller/agent
updates are class 1. After compatible units are active, changing image pins is
class 0: pull and atomically install, with no box restart. Preserve per-unit pins
and the normal hibernate/resume and explicit cold-loss gates in
[UPGRADES.md](UPGRADES.md). No release or deployment is performed by these docs.

The artifact foundation introduced schema 4 with reader floor 3. Its schema-3
SQL readers/writers could retain existing ownership/resource data and preserve
artifact rows and schema version. The subsequent [placement catalog](LIFECYCLE.md)
requires schema 5 / reader floor 5; older writers are refused there. They do not implement image administration; old controllers may refuse
versioned node advertisements. This is not a promise that old workers implement
new image selectors. Release v0.9.0 declares the new catalog and worker impacts,
retaining v0.8.6's worker interface 2 (agent readers 1–2). Managed disk/RAM
exports and prepared imports retain the pinned image digest alongside their
managed provenance; transport adapters and controller movement remain separate.
The upgrader copies the catalog and moves both controller and dashboard around this schema change;
the worker follows its normal class 2 capture gate. Published release entries
remain immutable.

## Limits and acceptance

The runner object interface has no listing or deletion. The opt-in
[controller collector](STORE_GC.md) adds exact dry-run candidates, terminal tombstones,
verification and version-based undo evidence through a separate local operator interface.
Live activation requires complete authority, effective policy and undo proof. Chunk
deduplication and scaling remain deferred. Prefetch currently covers all current images, within the existing
32-image runner ceiling. There is no age-based eviction of working templates.

Local checks cover directory-store round trips, conditional single/multipart
retry and lost responses through a fake CLI, hash/key/format rejection, pull
cleanup, catalog concurrency/expiry/retirement, image installation and schema 3
read/write compatibility. Live AWS IAM enforcement, real fresh-host timing and
controller-placed golden-image guest execution require a dedicated acceptance run.
Local tests do not establish that those live checks have passed.

See [local cache and placement](CACHE.md) for negotiated verified inventory,
lease-excluded eviction, staging reservations and deterministic cache hints.
