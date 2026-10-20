# Recovery-point upload timings

Observed on 2026-10-19/20 (UTC) in a dedicated sandbox AWS account, one host and
one box, before and after the 0.9.20 chunk-seal uploader
([RECOVERY.md](../RECOVERY.md#chunk-seals-reading-only-what-changed)). Every run is
a single sample (n=1), so there are no percentiles. All resources were destroyed
afterwards.

## Setup

| Item | Value |
| --- | --- |
| Instance | `m8i-flex.xlarge` (4 vCPU, 15.7 GiB), us-east-1, nested virtualization |
| Host | Ubuntu 24.04, kernel 7.0.0-1014-aws, Firecracker v1.17.0, AWS CLI 2.38.2 |
| Data volume | gp3 200 GiB, 6000 IOPS, 500 MiB/s, Btrfs at `/srv/ow` |
| Store | S3 bucket in the same region, versioning on, no lifecycle rules |
| Box | Ubuntu image, 4096 MiB RAM, 2 vCPUs, 64 GiB root disk |
| Disk data | 38.1 GiB used: 20 × 1 GiB random files, 15 × 1 GiB log-like text, 3 GiB in 344,064 small files |
| RAM data | 3 GiB random bytes in a guest tmpfs, plus a running counter process |
| Before | released v0.9.19 (worker and CLI) |
| After | 0.9.20 build of this change (worker and CLI swapped through hibernate/resume; RAM payload and counter survived) |

Method: `time ow backup recover --from RUNNER --to s3://BUCKET/PREFIX/snapshots`
as the worker owner, a fresh store prefix for each version. "Full" is the first
point into an empty prefix. "Incremental" follows an in-place overwrite of one
1 GiB random file (`dd conv=notrunc`) with the RAM payload unchanged; the point was
made due by moving the durable capture time back 1900 s. Bytes are the store's
growth under `chunks/`.

## Results (n=1 each)

| | v0.9.19 | 0.9.20 |
| --- | ---: | ---: |
| Full point, wall time | 6647 s (1:50:47) | **205.5 s** |
| Full: new chunk objects / bytes | 5423 / 28.59 GB | 5423 / 28.90 GB |
| Incremental, wall time | 3514 s (58:33) | **29.4 s** |
| Incremental: new chunk objects / bytes | 550 / 4.04 GB | 502 / 3.68 GB |
| Chunk existence checks | one `aws s3api head-object` process per nonzero chunk | none (local chunk index) |

What a point holds: disk 8192 chunks (3254 zero, 4938 nonzero), memory 512 chunks
(9 zero), state 1 chunk; 5423 unique nonzero chunks.

What changes between consecutive points with a 1 GiB disk change: about 135 disk
chunks (128 for the file, the rest filesystem metadata) and 370-410 of the 503
memory chunks, although the guest's RAM payload did not change. RAM therefore
dominates an incremental upload (about 3.4 GB of 3.7 GB).

Where the 0.9.20 time goes:

- Full: capture read 73.0 GB (whole disk, sparse, plus RAM; no earlier seal) in
  about 95 s; the transfer of 5423 chunks took 110.8 s.
- Incremental: capture read 647 chunks (512 RAM + 135 disk) and took 4908 disk
  chunk hashes from the previous seal unread; the transfer of 502 chunks took 10.3 s.
- v0.9.19 incremental: 547 of 550 new chunks were stored after about 25 minutes;
  the remaining ~34 minutes were per-chunk `head-object` calls on stored chunks.

Kill and resume (0.9.20): an upload killed with SIGKILL 3 s after its point sealed
had stored 53 chunk objects and no manifest. The next run (after the documented
one-minute retry wait) reused the sealed point, uploaded only the remaining 444
chunks in 11.5 s wall time, and committed the manifest.

The measured build (commit `7c5856d`) trusted its local chunk index for a day.
The released change instead lists the store's chunks before every point (for this
store, 16 parallel listing requests of about 340 keys each); that listing was not
timed here.

Not measured: a full restore onto a second runner (needs a second worker), restore
time, many boxes per run, slower volumes or networks, and a non-reflink filesystem.
