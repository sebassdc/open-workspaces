# ADR 0006: write-once artifact objects and pinned manifests

Status: accepted for the opt-in format-1 foundation. Deployment and real-S3 acceptance are separate gates.

## Decision

Keep `SnapshotStore` and its existing backup namespace compatible. The new
`ow_core::store` module stores sealed base images, golden images, snapshots and
captures under `PREFIX/store/v1/u/SHARD/UPLOAD_TOKEN/`. Live workspace disks are
never store objects. The controller issues a fresh random 128-bit token for each
attempt. An expired push lease is never revived and a new attempt never reuses
another attempt's keys.

Each data file is one zstd object. The manifest is canonical JSON (sorted object
keys, no whitespace), written last; the catalog records its SHA-256. It contains
the uncompressed size, allocated bytes and SHA-256 of each file and exact object
keys. Format 1 rejects references outside its own token. Chunking and parent
sharing require a later format; they are not silently inferred from filenames.

Writers use `If-None-Match: *` on `PutObject` and `CompleteMultipartUpload`.
The bucket policy requires the header for object-creation operations, exempting
multipart setup and parts. The AWS CLI stages at most one 64 MiB compressed part
at a time. Each part has an S3 SHA-256 checksum. A failed/lost completion response
or existing key succeeds only after HEAD confirms the expected checksum and byte
length; an ETag is never treated as SHA-256. Multipart retry checks the composite
SHA-256, including its part count.
The completion list is spooled to a private scratch JSON file and passed to the
CLI by file URI; it does not occupy a potentially oversized command argument.

A private new directory receives downloads. The reader first checks the manifest
against the catalog digest, validates every key, then verifies each decoded file's
size and SHA-256 while writing sparse files. The decoder is bounded by the pinned
logical size. Only a completed directory becomes visible to the worker.

The transport has no list or delete operation. Catalog references determine
liveness. Runners have exact-object get/put/abort rights, no store listing or
deletion. Only a future controller GC may remove unreferenced store objects;
artifact rows remain tombstones. No current live object is age-expired. Versioning
and at least seven days of noncurrent retention provide an undo window.

## Consequences

Identical unrelated uploads are not deduplicated. This avoids a stale-GC race
where an old sweep deletes a shared key newly adopted by another upload.
Uncommitted objects remain until controller GC is implemented. There is no new
eager snapshot upload, runner eviction, placement fencing or fleet scaling here.

The implementation reuses the existing AWS CLI endpoint validation and credential
chain, SHA-256/zstd dependencies and sparse download writer. No credentials,
deployment values or host identifiers belong in manifests or repository config.

## Sources and acceptance

- [S3 conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html).
- [Enforcing conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes-enforce.html).
- [CompleteMultipartUpload CLI](https://docs.aws.amazon.com/cli/latest/reference/s3api/complete-multipart-upload.html).

Directory-store tests exercise hash/path/format rejection, manifest-last failure,
retry collisions and pull cleanup. A fake AWS CLI exercises conditional single
and multipart writes, checksum mismatch and a lost completion response. These
checks do not establish live AWS policy enforcement, performance, or VM recovery.
