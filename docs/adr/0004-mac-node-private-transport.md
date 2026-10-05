# ADR 0004: bounded native Mac pool backend
Date: 2026-10-05 | Status: implemented and locally validated; live acceptance and planner review pending. Evidence: ../plans/mac-node-report.md

Keep the existing node protocol v1 and SQLite fixed placement. Use a separate
Rust Mac worker and managed host onboarding; compile the existing outbound node
transport on macOS, sharing TLS, job journaling and generation fences rather than
creating a new transport. The Linux controller remains planner-reviewed.

The locally signed helper owns one VM and bridges only fixed virtio guest ports to
private per-machine Unix sockets. No guest-initiated host listener, host mounts,
NAT or credential attachment. A persistent Ubuntu ARM64 ext4 disk boots with its
official architecture-correct kernel/initrd. Guest exec/files and PTY use bounded
protocols; PTY reuses the existing guest framing. No SSH capability initially.

Capabilities preserve the legacy Firecracker/x86_64 tuple and accept a new exact
apple-virtualization/aarch64 tuple with prepared ubuntu-arm64 image. Do not alias
architecture-specific names or route Firecracker state to Mac. Missing optional
operation capability retains old Linux behavior; Mac explicitly denies snapshot,
restore, hibernate, fork, publish and SSH. Reject before durable dispatch.

Local operator budget is <=2 GiB/2 CPU/one slot, separately clamped by controller
owner caps. Worker journals intent before spawning, retains unresolved demand,
and binds cleanup/recovery to live executable/PID/start identity. The initial implementation uses full cold disk copies; it makes no APFS clone
or live fork claim. An offline
node never releases placement/demand. Real gateway acceptance requires planner's
coordinated test revision/invite. No production readiness or unfiltered-network
claim; networking stays absent until host-owned isolation is implemented.

Native helper deployment target: macOS 14 minimum for existing save/restore API;
Apple Silicon required. Record actual linked binary minimum, not only requested
compiler flags. Public installer rollout is outside this lane.

## Review hardening (PR6)
Aggregate admission uses the trusted system user home, independent of configured
node roots. One worker holds the reservation throughout its lifetime; its helper
inherits the same flock description until exit, closing the crash-to-shutdown
gap. Unknown demand remains reserved. Completed create replay requires a proved
state and complete private disk/config; incomplete effects produce an uncertain
envelope, preserved by the node and controller journals. Sync failure is retained
as a diagnostic while owned pipe shutdown still runs. Upstream image preparation
restricts actual successful release signers and derives archive trust from the
authenticated root, with non-removable checks.
