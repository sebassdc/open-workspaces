# ADR 0001: Firecracker and Btrfs for the local prototype

Date: 2026-10-02. Status: accepted for the local development prototype; production
runtime and deployment suitability require further evidence.

## Context

The first feature set requires fast Linux workspaces, persistent disk, full-state
snapshots and independent forks, with observable resource consumption. This
machine exposes usable KVM after the owner enabled AMD SVM in firmware. Its
Btrfs filesystem passed host-file clone write-independence tests.

## Decision

Use pinned Firecracker v1.17.0 and the official release binary for the local
runtime, with a Rust CLI/worker/gateway. Use Btrfs reflinks for independent active
disk files and paired disk captures. Keep RAM and VM state in immutable named
snapshot directories, with SHA-256 manifests, compatibility checks and cached
verification invalidated by metadata changes. Keep management on owner-only
Unix sockets and guest networks in a dedicated namespace.

No new hypervisor is implemented. Do not use the experimental GNU source build
for workloads: its default seccomp policy is empty. Do not import Ignition code.

## Evidence

The actual host booted Linux 6.1.186 with Alpine 3.24.2 on Firecracker v1.17.0.
Experiments demonstrated guest shell execution and exit status, file transfer,
HTTP response readiness, paired RAM/disk capture, two restored child VMs,
independent process counters and disk writes, cold disk persistence, and
full-state restore after the original VMMs exited.

The Rust prototype's real-VM regression also verifies operation idempotency,
snapshot tamper rejection, memory admission limits, basic guest network separation,
hibernation and registry/disk recovery after abrupt worker loss. See the
[experiment record](../experiments/runtime-spike.md) for revisions, methods,
latencies and limitations.

Pinned Ignition revision `5648abf497938b840c93df115577b10048398a9e` fails unmodified
manifest loading because a sibling `lovable-client` dependency is absent, even
without enabling its optional feature. Its license is AGPL-3.0; its README
describes a modified guest kernel and missing self-hosting documentation. This
supports choosing the working Firecracker path for this prototype, not claiming
Ignition's runtime is slower or inferior. Further evaluation can revisit reuse
under an appropriate licensing decision.

## Consequences and unresolved work

- Full snapshots consume storage; retained artifacts and dirty clone pages need
  explicit budgets, retention and safe garbage collection.
- Snapshot portability is deliberately restricted to the pinned runtime and
  tested CPU model/flags, host kernel and guest-kernel hash. Cross-worker migration
  is not established.
- Synchronization and paired capture provide the tested fixture's consistency;
  application quiescence and external transaction semantics remain separate work.
- Kernel VMGenID reseeding, network identity refresh and a credential-free image
  are sufficient for these tests. Arbitrary application identities and secrets
  need stronger restore hooks.
- Cold-cache behavior, memory-intensive density, storage saturation, jailer/cgroup
  hardening, internet egress, public access and off-worker backups remain open.
- Btrfs is a verified local option, not a requirement for every future worker.
  Other filesystems need their own clone and persistence tests.

No production readiness, security certification or Boxd performance parity is
claimed by this decision.
