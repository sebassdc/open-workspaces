# Snapshot and fork pause timings

Status: **after-change numbers not yet measured on a KVM host.** This page records what was
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
| RAM save while paused | `Full`: every page | `Diff`: pages written since the VMM's last boot, load or capture (KVM dirty-page tracking) |
| Disk while paused | reflink | reflink (unchanged; it pairs disk with RAM) |
| Resume | after save and reflink | after save and reflink; pause reported as `timings.paused_ms` |
| Complete memory image | written by Firecracker | changed pages laid over a reflink of the previous image, after resume |
| Hash `state`, `memory`, `disk.ext4` | worker thread, one after another | separate thread, the three files in parallel |
| Publish manifest | worker thread | under the runtime lock, after hashing |
| Live fork child create | worker thread | worker thread, after the capture is published |

A repeat capture therefore writes only changed RAM pages and no disk data (the
disk is a reflink and the memory image a reflink of the previous one plus the
changed pages). Snapshot digests stay whole-file SHA-256, because backups, the
artifact store and cross-runner moves verify them, so hashing still reads the
whole memory and disk files; that cost is off the worker thread and outside the
pause. Recovery points (chunk-sealed) now also reuse unchanged memory chunks.

Targets from the task: pause under 2 s and total under 15 s at 4 GiB / 32 GiB.
Whether 0.9.22 meets them is unknown until measured on a KVM host. The pause now
scales with RAM written since the previous capture (the first capture after a
boot: the pages the guest touched), not with RAM size. The total is likely to stay
above 15 s for a box with tens of GiB of disk data, because the disk is hashed
whole; an extent-reusing digest for named snapshots would remove that.

Verified so far only without KVM: unit tests of the page merge (holes keep the
base, changed pages replace it, the base file is never written) and of the capture
request sequence against a fake Firecracker API (`Diff` when a base is known,
`Full` otherwise).

Failure-path fixtures also save a RAM diff, inject a disk-copy, resume API or
post-resume clock error, discard the failed staging files, then take two more
captures. The retry must save full RAM and preserve all three pages; the following
incremental save must preserve that repaired base. Before each RAM save request,
the old base is invalidated: even a lost API reply may follow a dirty-log reset.
The base becomes usable only after disk capture, resume (when requested) and memory
assembly succeed. A failed capture therefore costs one full save on retry.

The field names (`track_dirty_pages` on `/machine-config` and
`/snapshot/load`, `snapshot_type: Diff`) were checked against the Firecracker
v1.17.0 API specification.

## Method for the open measurement

On a dedicated runner (record instance type, kernel, Firecracker version, data
volume type/IOPS/throughput, filesystem), with one Ubuntu box of 4096 MiB RAM,
2 vCPUs and a 32 GiB disk holding a few GiB of written data and a RAM payload:

1. In one terminal, `while true; do /usr/bin/time -f %e ow exec BOX -- true; done`
   and, separately, the same loop against a second box on the same runner.
2. In another, `time ow snapshot BOX sN` five times (n=5), then
   `time ow fork BOX childN` five times.
3. Record each reply's `timings.paused_ms` and `timings.total_ms`, the CLI wall
   time, and the longest `exec` gap for both boxes during each capture. Between
   captures, change a known amount of guest RAM (for example rewrite 512 MiB in a
   tmpfs) and record the size of each capture's newly allocated extents.
4. Restore and fork from a second or later capture and check the RAM payload and a
   running counter survive (the merged image must be complete).
5. Report p50/p95 for pause, total and exec gap, before (0.9.21) and after
   (0.9.22), on the same host and box.
