# Runtime feasibility spike

Status: runnable local Rust/Firecracker prototype; further feasibility and alpha
hardening checks remain open. KVM works after the owner's firmware change.
The local runtime/storage decision is recorded in [ADR 0001](../adr/0001-local-runtime-and-storage.md).
See [the prototype guide](../LOCAL_PROTOTYPE.md) for commands and limits.

## First milestone agreed with the owner

Use this development machine as the sandbox. Deliver fast remote Linux computers
with shell/exec, file transfer, HTTP access and persistent disk, plus named
RAM+disk snapshots and independent full-state forks in the initial pilot.
Measure resource consumption and concurrent readiness before setting capacity
targets. Graphical desktop and enterprise orchestration remain later work.

Snapshot/fork functionality is part of the first vertical slice, rather than
waiting for the broader stateful beta. Its production hardening remains pending.

## Observed host feasibility

Initial inspection on 2026-10-01; KVM rechecked after reboot on 2026-10-02. This is a sanitized hardware/tool summary; raw local
reports and host configuration are not committed.

| Item | Observation |
| --- | --- |
| OS / kernel | Omarchy 4.0.4 / Linux 7.2.5-3-omarchy, x86_64 |
| CPU | AMD Ryzen 7 3700X, 8 physical cores / 16 threads |
| Memory | About 31 GiB total, 25 GiB available at inspection; changes with desktop use |
| Storage | Btrfs; about 520 GiB available at inspection |
| KVM | Initially absent; after reboot, API v12 and VM creation succeed; AMD KVM modules loaded |
| Firmware diagnosis | Initially `SVM disabled (by BIOS) in MSR_VM_CR`; owner enabled SVM and rebooted |
| CPU exposure | `svm` now exposed |
| Isolation controls | cgroup v2 present; tun device present; dedicated network setup not yet tested |
| Existing activity | Active desktop, Docker-related networking and local listeners; no changes made |
| Privilege access | Initially unavailable; owner requested and authenticated a temporary 12-hour passwordless sudo rule. Runtime does not require sudo. |
| Tools | Python 3.14.7, Git 2.55.0, GCC 16.2.1, btrfs-progs 7.1, e2fsprogs 1.47.4 |
| Experiment Rust | rustc 1.97.0 (2d8144b78); isolated toolchain under ignored data directory |

The firmware diagnosis was observed, not a hypothesis based solely on a missing
device. After the owner's reboot, preflight observes virtualization flags, KVM
API version 12 and successful VM creation. No additional host networking or
desktop configuration changes were needed for runtime execution.

## Pinned runtime evaluations

| Candidate | Revision | License | Evaluation |
| --- | --- | --- | --- |
| Firecracker v1.17.0 | `95f868c8e345b1cc8faccd1a3c910b4989dc3f58` | Apache-2.0 | Source build passed; official binary boot, full-state capture/restore and forks pass |
| Ignition | `5648abf497938b840c93df115577b10048398a9e` | AGPL-3.0 | Unmodified daemon build fails during manifest loading |

Separate upstream checkouts are under ignored `data/runtime-spike/upstream/`.
No Ignition source is incorporated into this repository. Its README describes
a modified guest kernel, automatic snapshot triggers, and missing self-hosting
documentation; these are upstream statements, not validated behavior here.

Ignition command attempted with Rust 1.97.0:

```bash
cargo +1.97.0 build --locked --release --bin ignitiond --features daemon -j 2
```

Observed exit code: 101. Cargo cannot load the optional `lovable-client` path
dependency because `../lovable-client/Cargo.toml` is absent. This happens before
compilation despite not enabling its feature. No fake dependency or local source
patch was supplied; an upstream dependency resolution is needed before assessing
runtime behavior. Build log remains local in ignored data.

Firecracker reproducible build command, with the isolated Rust toolchain installed:

```bash
bash experiments/runtime-spike/build-firecracker.sh
```

This uses the upstream Cargo.lock, GNU target and two build jobs. The Omarchy host
and Ryzen CPU are not automatically certified by a successful build; runtime
compatibility and security configuration remain to be tested.

Observed: Firecracker and jailer release builds completed; both report v1.17.0.
The GNU build warns that it embeds an empty seccomp policy. It is a compilation
probe only; do not use that binary for guest workloads. The pinned official
release artifact is the candidate for boot experiments with upstream default
filtering. Fetch it reproducibly with:

```bash
bash experiments/runtime-spike/fetch-firecracker.sh
```

The script verifies SHA-256
`06094a1108ae9e82aa4c23a775aa92758f53f1175d422270d9d6162cb9ade558`
against the pinned release asset digest obtained from GitHub's release API. This
is artifact integrity verification, not independent provenance attestation.

Observed: the official archive hash matched and its Firecracker binary reports
v1.17.0. The build script was rerun successfully against the clean pinned
checkout. Both shell scripts pass syntax checks; the Python probes compile;
the storage probe passes and preflight's expected blocker exit was verified.
Guest boot and snapshot/fork execution subsequently passed using the official
binary. Jailer operation and filter behavior under adversarial tests remain untested.

## Completed probes

1. KVM preflight: initially reports the firmware blocker with exit 2. After reboot,
   reports usable KVM API v12 and successful VM creation with exit 0.
2. Host storage: one 16 MiB allocated random file cloned using FICLONE. SHA-256
   contents match immediately after clone. Writes to the child preserve the
   parent; later writes to the parent preserve the child. Probe passes and cleans
   up its temporary files. This establishes host-file clone write independence,
   not VM snapshot correctness, shared extent accounting, or measured savings.

See [experiment instructions](../../experiments/runtime-spike/README.md).

## Implemented real-VM experiment

Guest artifacts: Alpine 3.24.2 minirootfs, SHA-256
`c5ca053cfe1d85c5b96dff8b9bc57045f7f184a30ffb6b65776409ca90388677`,
verified against its upstream checksum; Linux 6.1.186 from pinned Firecracker CI
prefix `20260930-a738f18a8db0-0`, observed/pinned SHA-256
`ea0e55d03dbaebc79a58644308e0517b7a33f1530a84848d9edf47ffa61f69c8`.
The kernel hash is a reproducibility pin from the downloaded bytes, not an
independent upstream signature verification. The ext4 image is built locally,
using upstream Alpine files and a minimal init plus a static Rust HTTP fixture.
No user home directory or host credentials are copied into it.

The Python smoke experiment passes actual shell execution, exit codes, file
roundtrip, HTTP readiness, paired snapshot, two concurrent restored children,
independent shell/HTTP counters and disk writes, cold restart, and full-state
restore after all original VMM processes exited. The guest kernel logs entropy
reseeding due to VM fork. Host networking lives in a temporary namespace.

The Rust CLI/worker/gateway prototype has a real-VM end-to-end regression adding
48 KiB binary transfers, operation idempotency, loopback publishing, identities,
guest-root peer-network attempts, hibernation, snapshot rewind, corruption rejection,
graceful/abrupt worker recovery, memory admission exhaustion and PSS statistics.
See `experiments/runtime-spike/prototype-e2e.py`. Guest-root enabling its own IP
forwarding does not enable forwarding in the worker's namespace.

Final regression also passes line-shell working-directory persistence and guest
wall-clock advancement after hibernation. Snapshot compatibility checks include
CPU model/flags, host kernel, guest-kernel hash and pinned runtime. The final
HTTP fixture displays its process counter in the response body as well as its
header; latency and memory tables below were collected before that display-only
change. Subsequent complete regression still passes.

Additional targeted checks reject a non-owner host-root peer at the control
socket and refuse to adopt an unmarked nonempty synthetic directory, preserving
its contents and permissions. Rust Clippy with warnings denied, shell syntax
checks, documentation links and whitespace checks pass. These checks complement
the VM regression; they do not constitute security certification.

During implementation, serial CR/LF handling, the absence of Alpine's BusyBox
httpd applet, initial machine ID setup, and Linux parent-death signals tied to a
spawning thread were discovered and corrected. The static Rust fixture replaces
the assumed HTTP applet. VMMs are spawned by the long-lived worker thread and
worker/gateway processes detach into their own sessions. Earlier failed development
runs are retained locally; the repeated validation below uses the corrected code.

## Initial repeated timings

Ten serial end-to-end runs completed successfully, with zero failures. Each run
starts a fresh worker and guest, exercises the full regression and shuts it down.
Host caches are warm; the fresh VM boot is not a host cache-cold experiment.
Workload: tiny Rust HTTP counter; one vCPU, 256 MiB guest RAM, 128 MiB disk.
Hardware/tool/runtime versions are above; percentile method is nearest rank.
With n=10, p95 is the largest observed sample. These are local pilot measurements.

Before cached verification of immutable captures:

| Operation (n=10) | p50 | p95 |
| --- | --- | --- |
| Create request to successful guest shell readiness | 823.45 ms | 828.56 ms |
| Paired snapshot, including artifact SHA-256 hashes and publication | 362.98 ms | 364.11 ms |
| Fork prepared snapshot, including SHA-256 re-verification and shell readiness | 291.84 ms | 298.50 ms |

The create measurement ends when the CLI returns after an actual ready command.
The fork measurement includes disk reflink, VMM startup, restore, an actual guest
command, identity/network refresh and CLI return. It does not include HTTP gateway
publication or an HTTP response. Snapshot timing includes guest sync, pause,
capture, matching disk clone, resume, hashing and manifest publication.
Raw samples and exact reproduction commands remain in ignored local data.

After caching verification of unchanged, read-only snapshot components, another
ten full end-to-end runs passed with zero failures, using the same conditions and
nearest-rank percentiles:

| Operation (n=10) | p50 | p95 |
| --- | --- | --- |
| Create request to successful guest shell readiness | 827.64 ms | 831.45 ms |
| Paired snapshot, including SHA-256 hashes and publication | 362.56 ms | 385.82 ms |
| Fork prepared snapshot with cached integrity verification | 69.47 ms | 71.62 ms |

The cache compares inode/device, size, nanosecond modification/change timestamps
and file mode for the manifest and all three artifacts. Changed metadata triggers
full hashing; the corruption regression still passes. A new worker has no cache
and verifies snapshots again. These observations come from the development
prototype v0.1.0 before/after that change; they are not cross-product benchmarks.

One smoke run measured three tiny-workload VMMs at approximately 100 MiB total
PSS against 768 MiB of configured guest memory. This single observation excludes
additional page cache/kernel costs, is not a density result, and must not set the
admission limit. The host-file CoW and actual VM disk-divergence assertions pass;
Btrfs extent output is retained locally without inferring savings by summing
per-file blocks, which can double-count shared extents.

## Memory-writing fan-out experiment

One additional warm-cache experiment successfully ran eight VMs at once, each
with one vCPU/256 MiB configured RAM and a 64 MiB heap in the Rust HTTP fixture.
The parent was captured after allocation and a successful request. Seven children
restored that checkpoint. Every child then read each 4 KiB heap page through an
HTTP request before measuring the clean/shared condition. A second request to
`/dirty` wrote one byte per heap page in each VM before the next measurement.

| Condition (one sample each) | Sum of VMM process PSS | Reserved guest RAM |
| --- | --- | --- |
| Heap pages read, shared between restored guests | 253.34 MiB | 2,048 MiB |
| Heap pages written independently in every guest | 637.54 MiB | 2,048 MiB |

This demonstrates that clone writes increase private memory cost. PSS excludes
extra host page cache, kernel and controller costs. It is not a maximum-density
test or permission to admit guests using that low initial figure. The experiment
shut down all of its VMs/gateways afterward and retained its data locally.
Reproduce with `python3 experiments/runtime-spike/memory-profile.py`.

## Remaining checks

1. Add and validate jailer/cgroup boundaries, CPU/I/O limits and broader guest
   network/access tests before exposing untrusted or public workloads.
2. Add curated developer tool images, remote CLI authentication and explicit internet egress
   policy. Measure actual supported developer/agent workloads.
3. Exercise snapshots with concurrent disk writers and databases; add application
   quiescence hooks and distinguish those guarantees from crash consistency.
4. Test application credentials, userspace entropy, cached identities and external
   session reconnection. The credential-free fixture does not establish arbitrary
   application restore safety.
5. Expand repeated fan-out/throughput, cache-cold and saturation measurements;
   include first HTTP readiness and parent service latency during capture/fork.
6. Inject failures throughout capture publication, registry writes and restore;
   implement safe lineage retention/GC, not just artifact tamper rejection.
7. Demonstrate off-worker backup/replacement-worker recovery and revisit ADR
   constraints as new runtime or storage evidence arrives.

## Resource optimization experiments (proposed)

- Begin with 1 vCPU / 512 MiB test guests, then compare 256 MiB and 1 GiB sizes
  where the same workload runs successfully. These are experiment parameters,
  not a product default or demonstrated minimum.
- Use immutable prepared checkpoints and reflink disk clones. Protect snapshots
  while any restored child references them; bound lineage depth and retention.
- Compare cold boot with prepared restore. Measure first usable command and
  HTTP response, including memory page faults after restore.
- Measure aggregate proportional memory (PSS), private/dirty memory, cgroup
  accounting, CPU time and storage extent growth. Summed RSS can double-count
  shared snapshot pages. Memory writes can erode initial sharing.
- Hibernate idle machines with explicit active-task protection. Restore on demand
  with bounded concurrency. Keep a warm pool only if measured demand warrants its
  ongoing memory cost.
- Reserve host memory for the desktop, controller, page cache and snapshot I/O;
  admit work using conservative memory reservations initially. Do not treat swap
  as predictable low-latency VM capacity. Benchmark before CPU overcommit.
- Rate-limit snapshot/restore I/O, disk growth and concurrent forks. Reject or
  queue excess requests explicitly and collect abandoned artifacts safely.

Record each sample's runtime/guest versions, workload, size, concurrency, cache
condition, failure and elapsed time. Use at least 10 samples per initial condition
and 30 for a follow-up capacity run; report sample count and p50/p95 with the
percentile method. Define cache-cold conditions without dropping the shared
desktop host's global caches. Warm-cache pilot latencies exist above; cache-cold,
memory-intensive density and saturation results remain open.

## Sources (upstream documentation)

- [Firecracker pinned release](https://github.com/firecracker-microvm/firecracker/releases/tag/v1.17.0)
- [Pinned setup guide](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/docs/getting-started.md)
- [Pinned snapshot semantics](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/docs/snapshotting/snapshot-support.md)
- [Pinned Ignition manifest](https://github.com/lttle-cloud/ignition/blob/5648abf497938b840c93df115577b10048398a9e/Cargo.toml)
- [Pinned Ignition README](https://github.com/lttle-cloud/ignition/blob/5648abf497938b840c93df115577b10048398a9e/README.md)

## Interactive terminal and OS-profile continuation

The subsequent owner-authorized slice implements native Rust guest PTYs through
authenticated WebSockets and the local `ow shell` command. Real HTTPS/WSS browser
tests cover interactive vi, signals/job control, resize, flow control, cleanup and
concurrent management exec; see [terminal evidence](../TERMINALS.md). Headless
Arch and Ubuntu Base profiles also pass real-VM terminal, persistence, RAM/disk
snapshot, fork, hibernate and restore tests; see [versions, sources and the small
consumption pilot](../GUEST_IMAGES.md). The original Alpine results above remain
measurements of the earlier implementation and workload.
