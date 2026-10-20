# Snapshot and fork pause timings

Status: **after-change numbers not yet measured.** This page records what was
observed before 0.9.22, what changed, and the method for the measurement that
remains open. It invents no results.

## Before (0.9.21, observed on a pool runner, 2026-10-19/20)

A box with 4 GiB RAM and a 32 GiB disk did not answer `ow exec` for the whole
of each snapshot: 47–92 s per capture (a handful of captures, no percentiles
recorded). The worker served every request on one thread and hashed the full
RAM file and the full logical disk (holes included) with `sha256sum` before it
answered anything else, so every box on that runner waited, not only the one
being captured.

## What 0.9.22 changes

| Step | 0.9.21 | 0.9.22 |
| --- | --- | --- |
| Guest `sync` | worker thread | worker thread (guest still running) |
| Pause, RAM/state save, disk reflink, resume | worker thread | worker thread; duration reported as `timings.paused_ms` |
| Hash `state`, `memory`, `disk.ext4` | worker thread, one after another | separate thread, the three files in parallel |
| Publish manifest | worker thread | under the runtime lock, after hashing |
| Live fork child create | worker thread | worker thread, after the capture is published |

Unchanged on purpose: the disk reflink stays inside the pause (it pairs disk with
RAM); Firecracker writes the full RAM file every time (`Full` snapshots, no
dirty-page tracking), so the pause still grows with RAM size and data-volume
write speed; snapshot digests stay whole-file SHA-256 because backups, the
artifact store and cross-runner moves verify them. Repeat snapshots therefore
copy no disk data (reflink) but still write and hash all RAM and hash the whole
disk.

Targets from the task: pause under 2 s and total under 15 s at 4 GiB / 32 GiB.
Whether 0.9.22 meets them is unknown until measured. A full 4 GiB RAM write in
under 2 s needs roughly 2 GiB/s of write throughput (page cache or volume); the
whole-disk hash of 32 GiB is likely to keep the total above 15 s on a first
capture. If the measurement misses the targets, the next steps are diff
(dirty-page) RAM snapshots and an extent-reusing digest for named snapshots.

## Method for the open measurement

On a dedicated runner (record instance type, kernel, Firecracker version, data
volume type/IOPS/throughput, filesystem), with one Ubuntu box of 4096 MiB RAM,
2 vCPUs and a 32 GiB disk holding a few GiB of written data and a RAM payload:

1. In one terminal, `while true; do /usr/bin/time -f %e ow exec BOX -- true; done`
   and, separately, the same loop against a second box on the same runner.
2. In another, `time ow snapshot BOX sN` five times (n=5), then
   `time ow fork BOX childN` five times.
3. Record each reply's `timings.paused_ms` and `timings.total_ms`, the CLI wall
   time, and the longest `exec` gap for both boxes during each capture.
4. Report p50/p95 for pause, total and exec gap, before (0.9.21) and after
   (0.9.22), on the same host and box.
