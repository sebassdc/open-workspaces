# Per-box root disks

Linux Firecracker boxes created with this release default to a **32 GiB sparse
root disk**. The image stays immutable; each box gets its own Btrfs reflink.
Existing boxes keep their disk size, including through worker upgrades,
hibernation and warm resume. There is no fleet migration.

```sh
ow create work --image ubuntu --disk 64GiB
ow list
ow inspect work
ow stop work
ow resize work --disk 128GiB
ow start work
```

Sizes are whole GiB: `64`, `64G` and `64GiB` mean the same thing. The range is
1–16384 GiB; creation also requires a disk at least as large as the source image.
`ow resize` accepts any combination of `--disk`, `--memory`, `--cpus`, preserving
omitted resources. Disk resize is grow-only, requires a positively stopped box,
and does not start it. Stop a hibernated box first (this discards its saved RAM).
Repeating the same disk size is harmless. Shrinking is always refused.

The dashboard's **New machine** dialog has a root disk size field; an owner's
stopped machine has a **Resize** button. The authenticated remote API uses the
same `/api/operation` endpoint:

```json
{"op":"create","id":"work","image":"ubuntu","disk_gib":64}
{"op":"resize","id":"work","disk_gib":128}
```

Use a new `operation_key` for a new intent and reuse it for a lost-response retry.
Only the owner can resize; a shared machine's `admin` role cannot. A local Unix
socket CLI remains a trusted host-operator interface. Disk sizing is currently
Linux/Firecracker only. Mac requests are explicitly refused. Older runners
without the `disk_resize_v1` status/heartbeat field refuse disk requests at the
controller: upgrade the worker and agent together before using this feature.

## Why grow root rather than add a home disk?

The existing engine pairs one raw ext4 root disk with Firecracker RAM/device
snapshots. Growing that file preserves `/home`, `/persist`, package installations
and the existing snapshot/fork/backup format. A second home disk would require
new attachment, ownership and paired-snapshot semantics for little benefit here.

Resize extends only the sparse raw file. On the **next cold start**, the worker
checks ext4 and, if needed, runs `e2fsck -p -f` then `resize2fs` **before booting**.
No guest image rebuild, guest package installation, live block-device update or
manual guest command is needed. The host requires e2fsprogs on its PATH (Ubuntu
hosts normally already have it); missing tools or a failed check prevent boot
and retain the disk. This works with older prepared images too.

Warm resume/fork keeps the captured disk and device geometry unchanged. A
snapshot from before a disk resize cannot be restored over the larger box:
that would shrink it and mismatch the captured device geometry. Fork that
snapshot into another box instead. Backups retain the existing sparse disk
format and new captures include the grown disk.

## Host capacity

The worker serializes disk admission with its normal operations. It checks both:

- `OW_MAX_DISK_GIB`: sum of logical active box disk sizes, including stopped and
  hibernated boxes (default **1024 GiB**, range 1–1048576).
- Free space on its data volume: enough for **all existing logical box disks
  plus the requested growth/new disk**, and `OW_MIN_FREE_GIB` host reserve
  (default **5 GiB**). Invalid configuration fails worker startup.

This deliberately conservative reservation counts full disk sizes, even when
already allocated or shared by reflinks: `st_blocks` cannot establish exclusive
ownership of shared extents. Creating, growing, forking and importing boxes
cannot overcommit the worker's disk budget. A snapshot reflinks the existing
disk rather than creating another writable box, so its admission requires only
its RAM capture plus the host reserve. Later copy-on-write disk growth still
consumes free space; captures are not a guarantee of capacity for arbitrary
future writes. Deleting a box frees its reservation. A same-size retry or cold start of an existing box does not
require a new reservation.

Use one worker/data root per dedicated host volume, as in the supported host
layout. These checks do not control unrelated host writers, multiple independent
workers on the same volume, or filesystem metadata overhead beyond the reserve.
Monitor Btrfs space and retain enough reserve for metadata, logs and backups.
No runtime quota or snapshot garbage collector is added by this feature.

## Reporting

`ow list`, `ow inspect` and `/api/state` include:

- `disk_bytes`: logical block-device capacity, including growth pending boot.
- `disk_used_bytes`: ext4 blocks in use, including filesystem metadata; an
  approximate observation for running guests because superblock counters lag.
  `null` means usage could not be read, never zero by assumption.
- `disk_allocated_bytes`: host blocks attributed to the sparse file; shared
  extents can appear in more than one file's count.

If a disk is missing or unreadable, its three disk fields are `null`; its
workspace record and other boxes on the runner remain visible. This reporting
fallback does not bypass the disk checks required to start a box or grow its disk.

The dashboard shows capacity and filesystem use on each machine card. Offline
runner values are cached, as with other machine metadata. Inside a resized box,
`df -h /` shows its expanded filesystem after start.

## Release and acceptance

Release 0.7.0 is **class 2**: controller, agent, dashboard and worker changes;
no host replacement is needed. The existing in-place upgrader hibernates running
boxes for the worker switch and resumes their original sizes and paired state.
Owners separately stop and resize boxes they want to grow. Bump consumer pins
after the release tag is published by the merge pipeline.

Focused Linux checks (e2fsprogs required; the runtime reservation test needs
14 GiB free space) are in `ow-core`:

```sh
cargo test --workspace
cargo test -p ow-core disk -- --ignored --nocapture --test-threads=1
```

The filesystem integration test creates ext4, writes a file, grows it sparsely,
checks capacity/data preservation, retries and refuses a shrink. API/catalog
tests cover malformed inputs, owner/admin isolation, stopped state, capability
refusal and disk-only resource preservation. Live KVM acceptance on a dedicated
runner must additionally create a default-sized box, write a file, stop/grow/start,
check `df` and the file, then snapshot/fork/hibernate/resume; do not use an owner's
existing box as a probe.
