# EC2 nested-virtualization timings

Observed on 2026-10-16 with the [AWS component](../../deploy/aws/README.md)
smoke test (`deploy/aws/examples/smoke/run.sh`) in a dedicated sandbox account.
Three five-sample runs on fresh hosts: run A at commit `fba80ca` (local worker
via `ow up`, no delete command yet), run B at `7fd0de3` (adds `ow delete`), and
run C at `d5e8af6`, where the host service is `ow host up`: controller plus
local runner. Run C was one unattended `run.sh` call; before the lifecycle it
checked the service active and the controller reporting its runner online.
All stacks were destroyed afterwards; no resources remain.

## Setup

| Item | Value |
| --- | --- |
| Instance | `m8i-flex.xlarge` (4 vCPU, 2 cores × 2 threads, 16 GiB), us-east-1d, nested virtualization enabled |
| Image | Canonical Ubuntu 24.04 (`ami-0fa5967347d08d2df`), kernel `7.0.0-1014-aws` |
| Storage | 30 GiB gp3 root; 100 GiB gp3 Btrfs data volume (default 3,000 IOPS, 125 MiB/s) at `/srv/ow` |
| Software | `open-workspaces` at the commits above, Firecracker v1.17.0, Rust 1.97.0, built on the host |
| Guest | Alpine 3.24.2, Linux 6.1.186, 1 vCPU / 256 MiB |
| Method | 5 serial samples; fresh machine per sample; nearest-rank percentiles (p95 is the largest sample at n=5) |

Each step is the wall time of one `ow` CLI call on the host, run as root
through SSM (the `/usr/local/bin/ow` wrapper adds a `runuser` hop). Host caches
were warm after the first sample.

## Workspace operations (n=5 per run)

| Operation | Run A p50 / p95 | Run B p50 / p95 | Run C p50 / p95 | What it covers |
| --- | --- | --- | --- | --- |
| Create to first exec | 1,366.7 / 1,370.5 ms | 1,367.8 / 1,373.2 ms | 1,371.5 / 1,386.0 ms | `ow create` plus a successful `ow exec true` |
| Connect (exec round trip) | 17.6 / 17.7 ms | 17.5 / 17.9 ms | 16.8 / 17.7 ms | `ow exec -- uname -r` on a running machine |
| Snapshot | 1,558.6 / 1,562.4 ms | 1,539.4 / 1,661.9 ms | 1,544.8 / 1,627.0 ms | paired RAM+disk capture with SHA-256 hashes and publication |
| Fork from snapshot | 134.3 / 136.6 ms | 133.7 / 139.8 ms | 137.4 / 146.3 ms | child from that snapshot, cached verification, ready to exec |
| Live fork | 2,158.3 / 2,183.8 ms | 2,161.5 / 2,172.6 ms | 2,164.1 / 2,207.2 ms | `ow fork` without `--snapshot`: capture of the running parent, then fork |
| Hibernate | 1,583.0 / 1,647.4 ms | 1,588.8 / 1,601.9 ms | 1,588.6 / 1,982.6 ms | checkpoint and VMM exit |
| Resume | 128.7 / 130.5 ms | 127.7 / 131.7 ms | 128.3 / 130.6 ms | `ow start` of the hibernated machine |
| Stop (n=15) | 46.9 / 54.9 ms | 53.8 / 74.6 ms | 52.6 / 74.8 ms | cold stop of each machine |
| Delete (n=15) | not available | 36.4 / 46.8 ms | 35.8 / 48.2 ms | `ow delete` of a stopped machine, its disk and its snapshots |

Every sample checked state, not only exit codes: a guest process started before
the snapshot kept running and kept counting in the snapshot fork, the live fork,
the live fork after hibernate/resume, and the parent. In run B the parent was
deleted while its snapshot fork still ran; the fork kept counting. After the
deletes, no record and no snapshot of the three machines remained.

Run B shipped the unpushed branch to a host bootstrapped from `fba80ca` as a git
bundle over SSM and rebuilt it there, because a host builds only published refs.
The same host also showed: delete of a running machine is refused; delete of a
hibernated machine removes its checkpoint and named snapshots; a new machine
with the deleted ID starts clean and cannot fork the deleted snapshot.

## Host lifecycle

| Phase | Run A | Run B | Run C |
| --- | --- | --- | --- |
| `pulumi up` (8 resources) to instance ID | 52 s | 54 s | 52 s |
| Instance to SSM agent online | 1 s | 3 s | 1 s |
| Instance to bootstrap done (packages, Btrfs, Rust/Firecracker/guest assets, release build, service start; run C also waits for the runner online in the controller) | 348 s | 277 s | 268 s |
| `pulumi destroy` | 65 s | 51 s | 65 s |

## Compared with the local development machine

The [local measurements](runtime-spike.md#initial-repeated-timings) used a
different host, so this is context, not a controlled comparison:

| Operation | Local p50 | EC2 p50 |
| --- | --- | --- |
| Create to exec | 827.6 ms | 1,366.7 ms |
| Snapshot | 362.6 ms | 1,558.6 ms |
| Fork from snapshot (cached verification) | 69.5 ms | 134.3 ms |
| Live fork | ~430 ms (snapshot + fork) | 2,158.3 ms |

Snapshot, hibernate and live fork write and hash the 256 MiB guest RAM file,
and they slowed down most. Storage throughput (gp3 baseline 125 MiB/s) and
nested virtualization are both plausible causes [inferred]; neither was
isolated. Next experiments: provisioned gp3 throughput or an instance-store
type, and a per-phase breakdown of capture.

## Not covered

- Tailscale join with a real key: the sandbox secret had no value yet, so this
  run only exercised the failure path: the join failed, bootstrap still
  finished `ok tailscale=failed` and SSM kept working.
- Cache-cold timings, larger guests, Ubuntu/Arch images and concurrent load.
- Host replacement keeping the data volume (designed, not exercised).

Raw logs and the JSON summary stay in the operator's ignored results directory.
