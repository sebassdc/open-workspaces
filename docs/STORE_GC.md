# Controller artifact collection and recovery

`ow store` is a local Linux operator command. Collection is opt in; no timer,
runner job, box operation or deployment is installed. Run on the single ownership
controller, using its usual `--data-dir`. The catalog, never an S3 listing, grants
liveness. This collector covers only format-1 `PREFIX/store/v1/u/` objects. Legacy
`SnapshotStore` snapshots, recovery-point uploads and catalog backups have separate
namespaces and retention; this command cannot delete them.

```sh
ow store gc --to s3://BUCKET/PREFIX --dry-run
ow store verify --to s3://BUCKET/PREFIX
# Only after the activation gates below have been independently checked:
ow store gc --to s3://BUCKET/PREFIX --live --activation /PRIVATE/activation.json
# Resume a crashed live run, retaining its original marker/cutoff and candidate set:
ow store gc --to s3://BUCKET/PREFIX --live --activation /PRIVATE/activation.json --resume RUN
ow store undo-plan RUN
```

Omitting both mode flags selects dry-run. `--server`, operation keys, a remote
login and node job payloads cannot authorize collection. Dry-run outputs JSON
with the run ID, exact relative keys, stored bytes per key, total bytes and server
cutoff. It records history and grace observations without transitioning artifacts
or deleting objects. A live invocation makes a new mark; it never applies an old
dry-run list. `--resume` accepts only a previously recorded live run.

## Liveness and terminal claims

Roots include named format-1 snapshots (owner plus physical snapshot name),
`resources.current_capture` for placed or parked boxes, current/kept image
versions, both capture references of unfinished transfers, pending/uncertain
version-pinned creates, unexpired push/pull/fork/lazy leases and their pinned
parents. Lineage is traversed transitively. Finished transfer/image/copy/lease
history keeps its foreign keys but does not keep an otherwise dead artifact alive.
Building uploads are protected by their valid leases or an unfinished transfer,
not by an expired image build row indefinitely.

Newly unreferenced artifacts get at least one hour of grace from the first
unreferenced observation on S3's clock. A temporary reference between runs resets
that observation, including parent lineage. Existing local `unreferenced_at` or
manifest creation dates never establish object age. First collection can therefore
require a later run before reclaiming anything.

Each run creates a unique conditional marker at
`PREFIX/store/v1/gc/runs/TOKEN.json`. Its HEAD `LastModified` establishes the cutoff
(one hour earlier). Enumeration is explicitly paginated. Only recognized upload
keys with S3 `LastModified` strictly before that cutoff can become candidates.
Pinned manifests are fetched by catalog digest, validated and HEAD-checked before
any terminal claim. Unknown schema/root contracts, incomplete ownership import,
foreign-key violations, missing manifests/data or unavailable bucket configuration
fail closed. Catalog changes during network verification require a fresh run.

An immediate SQLite transaction rechecks authority and marks doomed artifacts
`deleting` before deletion. Database triggers reject new roots/leases/parent uses
of terminal artifacts, expired push renewal, expired-lease commit, identity changes
and row deletion, including from compatible older writers. Image commit additionally
rechecks the exact lease, parent state, store and base digest. Format 1 allows only
objects under the attempt's own token; parent object sharing needs a later format.
Retries allocate fresh tokens; expired attempts never become live again.

A long pause or overlapping collector is safe: old runs retain their original
keep set and protected tokens, and new uploads use fresh keys after the old cutoff.
Claims persist across a crash without timeout-based revival. Manifests are deleted
before data. Every delete is an individual `DeleteObject`, never a version delete.
A slow or failed request stops the run; retry deliberately with `--resume` after
resolving the error. Missing keys count as completed deletions. Remaining objects
keep a row in `deleting`; only an empty attempt becomes a `deleted` tombstone.
Every completed sweep verifies current live/stored objects again.

## Activation gates and evidence

Source installation alone does not authorize live fleet GC. Before activation:

1. Establish one complete, authoritative catalog for this namespace, with all
   current snapshots/captures, lineage, leases and transfers reconciled. An older
   backup with an unresolved write gap is insufficient, even if worker inventory
   looks empty. Stop/isolate the old authority before controller replacement.
2. Inspect effective bucket and role policies on a disposable fixture. Verify
   conditional single/multipart creation, no live current-object expiry, enabled
   versioning and at least seven days of noncurrent retention. Prove runner store
   List/Delete denial; controller allowed-prefix enumeration/current deletion;
   controller deletion outside that prefix and `DeleteObjectVersion` denial.
3. Rehearse an actual version-based restore on disposable objects using a separate
   authorized break-glass identity; verify the restored manifest digest and pull.
   Keep the policy and undo transcripts privately and hash both with SHA-256.
4. Inspect a fresh dry-run and create an owned mode-0600 activation receipt. These
   are operator attestations of the preceding tests, not automated IAM simulation.

The receipt's exact JSON shape is:

```json
{
  "catalog_id": "VALUE_FROM_SETTINGS_store_gc_catalog_id",
  "device": 123,
  "inode": 456,
  "store_url": "s3://BUCKET/PREFIX",
  "dry_run": "EXACT_DRY_RUN_ID",
  "expires": 1800000000,
  "catalog_authoritative": true,
  "policy_evidence_sha256": "SHA256_OF_PRIVATE_EFFECTIVE_POLICY_TRANSCRIPT",
  "undo_evidence_sha256": "SHA256_OF_PRIVATE_UNDO_TRANSCRIPT"
}
```

Use `os.stat` on the controller's `catalog.sqlite3` for `device`/`inode`; read the
catalog ID from `settings`. `expires` is a Unix timestamp no more than one hour
away. Digests must be 64 lowercase hexadecimal characters. The namespace and dry
run must match. A copied replacement catalog has a different inode and requires
fresh authority evidence, dry-run and receipt. Retain transcripts with the catalog
backup and do not grant their attestation to a guest or runner. Restart requires a
fresh valid receipt and rechecks bucket safety and all current stored manifests.

`gc_events` retains private bucket versioning/lifecycle/policy snapshots plus
mark/sweep/verification errors, including failures before a run was created.
`gc_runs` retains the frozen plan, gate receipt and last failure after successful
resume. `gc_candidates` records each object's size and prior version **before**
the request, and the returned delete-marker version. Back up these tables with
the catalog. A lost reply leaves the prior version available for reconciliation;
it never silently erases the failure trail.

## Undo and backups

Freeze collection and catalog mutation first. Preserve the database and effective
bad policy before fixing either. `undo-plan` prints exact prior versions and known
delete-marker IDs. With an independently authorized break-glass identity:

1. Inspect `ListObjectVersions` for each exact key. A missing delete reply requires
   reconciling the marker against the saved prior version; do not guess its ID.
2. Fetch the prior version to private storage and verify the pinned manifest and
   file hashes. Remove only the confirmed delete marker using `DeleteObject` with
   its version ID. That action requires `s3:DeleteObjectVersion`, which the normal
   collector role explicitly denies.
3. Verify current versions, then pull and hash all restored files. Republish needed
   recovered data with fresh tokens and new catalog intent. Never flip a tombstone
   back to `stored`; unreferenced recovered keys can be swept again.

Seven-day version retention is a bounded undo opportunity, **not a backup**.
Lifecycle mistakes, loss of the catalog, expired versions or a compromised
break-glass principal require independent tested backups. Do not wait for the
window to expire while diagnosing a missing object.

## Deployment and compatibility

`Host.storeGc: true` adds optional scoped IAM alongside `storeUrl`, with no new
bucket, endpoint, host configuration, timer or user-data change. Runner inputs do
not expose it. The policy lists only `store/v1/u/`, deletes current keys only
there, writes run markers only under `store/v1/gc/runs/`, and explicitly denies
version deletion throughout the bucket. Deployed effective policy still needs the
fixture checks above; source policy tests cannot establish it.

This is a class 1 controller change and class 0 optional IAM/config change. Worker,
agent transport and guest interfaces are unchanged. The catalog remains schema 5 /
reader floor 5: additive tables and SQLite triggers enforce terminal/reference
rules on prior schema-5 writers, following the existing cache-claim pattern. No
schema number is consumed; GC refuses an unfamiliar newer root contract even if
other readers can open it. Do not roll back by dropping guards or clearing claims.
Versions, release-impact version entries and tags belong to the coordinated release
owner. No live AWS/IAM, VM or production-deletion acceptance is claimed here.
