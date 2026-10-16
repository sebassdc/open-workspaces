# EC2 nested-virtualization timings

Observed on 2026-10-16 with the [AWS component](../../deploy/aws/README.md)
smoke test (`deploy/aws/examples/smoke/run.sh`) in a dedicated sandbox account.
The stack was destroyed after the run; no resources remain.

## Setup

| Item | Value |
| --- | --- |
| Instance | `m8i-flex.xlarge` (4 vCPU, 2 cores × 2 threads, 16 GiB), us-east-1d, nested virtualization enabled |
| Image | Canonical Ubuntu 24.04 (`ami-0fa5967347d08d2df`), kernel `7.0.0-1014-aws` |
| Storage | 30 GiB gp3 root; 100 GiB gp3 Btrfs data volume (default 3,000 IOPS, 125 MiB/s) at `/srv/ow` |
| Software | `open-workspaces` commit `fba80ca`, Firecracker v1.17.0, Rust 1.97.0, built on the host |
| Guest | Alpine 3.24.2, Linux 6.1.186, 1 vCPU / 256 MiB |
| Method | 5 serial samples; fresh machine per sample; nearest-rank percentiles (p95 is the largest sample at n=5) |

Each step is the wall time of one `ow` CLI call on the host, run as root
through SSM (the `/usr/local/bin/ow` wrapper adds a `runuser` hop). Host caches
were warm after the first sample.

## Workspace operations (n=5)

| Operation | p50 | p95 | What it covers |
| --- | --- | --- | --- |
| Create to first exec | 1,366.7 ms | 1,370.5 ms | `ow create` plus a successful `ow exec true` |
| Connect (exec round trip) | 17.6 ms | 17.7 ms | `ow exec -- uname -r` on a running machine |
| Snapshot | 1,558.6 ms | 1,562.4 ms | paired RAM+disk capture with SHA-256 hashes and publication |
| Fork from snapshot | 134.3 ms | 136.6 ms | child from that snapshot, cached verification, ready to exec |
| Live fork | 2,158.3 ms | 2,183.8 ms | `ow fork` without `--snapshot`: capture of the running parent, then fork |
| Hibernate | 1,583.0 ms | 1,647.4 ms | checkpoint and VMM exit |
| Resume | 128.7 ms | 130.5 ms | `ow start` of the hibernated machine |
| Stop (n=15) | 46.9 ms | 54.9 ms | cold stop of each machine |

Every sample checked state, not only exit codes: a guest process started before
the snapshot kept running and kept counting in the snapshot fork, the live fork,
the live fork after hibernate/resume, and the parent.

## Host lifecycle (single run)

| Phase | Time |
| --- | --- |
| `pulumi up` (8 resources) to instance ID | 52 s |
| Instance to SSM agent online | 1 s |
| Instance to bootstrap done (packages, Btrfs, Rust/Firecracker/guest assets, release build, `ow up`) | 348 s (an earlier run on the same setup: 276 s) |
| `pulumi destroy` | 65 s |

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
- Per-machine delete: the CLI has no delete command yet; the smoke test stops
  its machines and `pulumi destroy` removes the host and the volume.
- Cache-cold timings, larger guests, Ubuntu/Arch images and concurrent load.
- Host replacement keeping the data volume (designed, not exercised).

Raw logs and the JSON summary stay in the operator's ignored results directory.
