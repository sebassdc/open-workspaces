# S3 transfer timings: in-process parallel transfers

Observed on 2026-10-20 (UTC) in a dedicated sandbox AWS account, one host and one
box, before and after the 0.9.25 transfer changes
([ARTIFACT_STORE.md](../ARTIFACT_STORE.md), [RECOVERY.md](../RECOVERY.md)).
Unless a row says n=2 or n=3, every figure is a single sample (n=1), so there are
no percentiles. All resources were destroyed afterwards.

## Setup

| Item | Value |
| --- | --- |
| Instance | `m8i-flex.xlarge` (4 vCPU, 15.7 GiB), us-east-1, nested virtualization |
| Host | Ubuntu 24.04, kernel 7.0.0-1014-aws, Firecracker v1.17.0, AWS CLI 2 (credential export only after the change) |
| Data volume | gp3 200 GiB, 6000 IOPS, 500 MiB/s, Btrfs at `/srv/ow` |
| Store | S3 bucket in the same region, versioning on, no lifecycle rules |
| Box | Ubuntu image, 4096 MiB RAM, 2 vCPUs, 64 GiB root disk |
| Disk data | 39 GiB used: 20 × 1 GiB random files, 15 × 1 GiB log-like text, 3 GiB in 344,064 small files (the same generator as [recovery-upload-timings.md](recovery-upload-timings.md)) |
| RAM data | 3 GiB random bytes in a guest tmpfs, plus a running counter process |
| Before | released v0.9.21 (worker, CLI and a small harness built from its `ow-core`) |
| After | this change on the 0.9.21 base (commit `8de34ab` for the recovery runs, `c07bcdf` for the artifact runs), worker and CLI swapped through hibernate/resume; RAM payload and counter survived |
| Merged | this change merged with 0.9.24 (`8fc7e18`), which adds RAM diff snapshots |

Method:

- **Recovery point:** `time ow backup recover --from RUNNER --to s3://BUCKET/PREFIX/snapshots`,
  a fresh prefix per version. "Full" is the first point into an empty prefix,
  "incremental" follows an in-place overwrite of one 1 GiB random file
  (`dd conv=notrunc`) with the durable capture time moved back 1900 s.
- **Recovery restore:** `ow backup restore` of the full point into an empty runner
  directory; the time to the end of the download (the import step then fails
  for lack of a worker there).
- **Artifact store (cross-runner fork, move, park, restore transfer):** a harness
  calls `ow_core::store::push` of a named snapshot (state, 4 GiB memory, 64 GiB
  sparse disk) through `S3Objects`, then `store::pull` into an empty directory,
  with the page cache dropped before each. This is the byte transfer of a fork
  or move; the capture, leases and import around it were not timed.

## Results

| | Before (v0.9.21) | After (0.9.25) |
| --- | ---: | ---: |
| Artifact push (64 GiB sparse disk with 39 GiB of data, 4 GiB RAM) | 698 s | **111 s** |
| Artifact pull of the same | 136 s | **102 s** |
| Artifact push + pull (fork/move transfer) | 834 s | **213 s** |
| Recovery point, full | 210.5 s | 202.7 s; merged 203.2 s |
| Recovery point, incremental | 24.5 s | 22.4 s, 20.1 s (n=2); merged 21.8, 20.4, 21.2 s (n=3) |
| Recovery transfer, incremental (bytes uploaded) | 10.7 s (3.41 GB) | 7.3 s (4.29 GB), 7.3 s (4.38 GB) |
| Recovery restore download (full point) | 128.8 s | **92.3 s** |

Full point details (after): capture read 46.5 GB and skipped 3158 hole chunks
(before: 73.0 GB, no holes skipped); the transfer read 45.5 GB and uploaded
28.9 GB in 110.7 s (before: 116.0 s).

How the artifact push got from 698 s to 111 s, each step measured on the same box:

| Build | Push | Pull |
| --- | ---: | ---: |
| v0.9.21: one AWS CLI process per 64 MiB part, one at a time; download through `aws s3 cp -` | 698 s | 136 s |
| In-process signed requests, 4 parts of 64 MiB in flight, 8 ranges, multithreaded zstd | 166 s | 137 s |
| Reads overlapped with hashing, sparse writes on their own thread | 152 s | 135 s |
| 16 parts (16 MiB, growing) and 16 ranges in flight | 134 s | 105 s |
| Four positional read-ahead threads instead of one | 134 s | 102 s |
| SHA-256 on the read-ahead thread instead of the compressor's | **111 s** | **102 s** |

A push into a discarding store (no network) took 126 s before the last step and
104 s after it, so the local read, hash and compress pipeline sets the push time.

## Where the time goes now

- **Disk floor.** This volume delivers about 520-550 MB/s, from one reader or sixteen. The artifact objects hold
  about 45 GB of non-hole data, so reading it (push) or writing it (pull) takes
  at least about 86 s; push and pull are within 1.3× and 1.2× of that.
- **Full recovery point.** The worker reads every mapped chunk to seal the point
  (46.5 GB), then the uploader reads the new chunks again to send them (45.5 GB):
  two passes over the same data, at least about 174 s on this volume. Upload
  parallelism no longer matters there; a single pass (sending chunks while they
  are sealed) or a faster volume would.
- **Incremental recovery point.** About 13 s is the capture (Firecracker writing
  the RAM image, reading the changed RAM and disk chunks); about 7 s is the
  upload of about 4.3 GB, nearly all of it RAM. Comparing the RAM images of two
  consecutive points: 0.49 GB differ at 4 KiB granularity, 0.81 GB at 64 KiB,
  1.83 GB at 1 MiB and 3.78 GB at the 8 MiB chunk size, so most 8 MiB RAM chunks
  hold at least one changed page. The 0.9.24 RAM diff snapshots did not change
  this (merged row). Smaller RAM chunks, or packing changed ranges into larger
  objects, is the next lever and needs a new seal format.

## Correctness checks

- An object written by this release (multithreaded zstd, parts of growing size)
  was pulled by the v0.9.21 reader: success, every file matched its manifest
  hash, and the 4 GiB RAM file was byte-identical to the source.
- The restored disk images kept their holes (allocated size equal before and
  after).
- Kill-and-resume, lost completion responses, refused parts, ETag-pinned ranged
  reads, one-chunk incrementals and byte-exact restores are covered by the
  loopback S3 tests (`store::s3`, `snapshot_chunks`); they were not repeated
  live here.

Not measured: a full cross-runner fork or move end to end (needs a second
runner), more than one box per run, other volume or instance sizes, and a
non-reflink filesystem.
