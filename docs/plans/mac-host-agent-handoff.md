# Handoff: native Mac host support
Updated: 2026-10-04 | Size: L | State: ready for an agent running on the owner's Mac

## Goal and authorization
Owner requested Mac hosting support and authorized a Mac agent to implement and test what is needed. Deliver a real Mac node contributing workspaces to the existing private pool, not just a macOS CLI. Begin with Apple Silicon Linux ARM64 guests; separately evaluate macOS Tahoe guests because the owner also wants those. A Linux-only success must not be labeled macOS guest support.

## Read first
Read AGENTS.md, README.md, docs/HANDOFF.md, docs/ARCHITECTURE.md, docs/adr/0001-local-runtime-and-storage.md, docs/adr/0002-bounded-linux-multinode.md, docs/HOSTS.md, docs/MULTINODE.md, docs/NETWORKING.md, docs/plans/host-capacity-planner-handoff.md and docs/plans/compressed-assets-planner-handoff.md. Read installed builder role and project overlay if present. Use an isolated `work/mac-host` branch/worktree; record exact baseline commit. Coordinate shared protocol changes with the SSH builder through planner; do not merge its lane yourself.

## Current implementation and evidence
Rust gateway + SQLite ownership/fixed placement + outbound node channel + private worker; Linux host runtime is Firecracker/KVM with Btrfs assets. Existing macOS ARM64/Intel binaries are clients only. Current Ubuntu assets and Firecracker state are x86-64, not portable to ARM64. Two same-host Linux workers have bounded automated acceptance; owner reports creating Ubuntu on a second physical Linux host across networks. Its snapshot/restart/fork acceptance is still in progress. Existing guests, disks and credentials must remain unchanged.

## Implementation sequence
1. Inventory the actual Mac: `sw_vers`, `uname -m`, `sysctl -n hw.memsize`, `sysctl -n hw.logicalcpu`, `df -h "$HOME"`, `xcode-select -p`, `swift --version`, `cargo --version`. Record chip, OS/build, free RAM/storage and existing VM/service demand. Do not assume Xcode, root access or entitlements are present.
2. Write an evidence-backed ADR for a backend using Apple's Virtualization framework (preferred candidate; existing VMM, no new hypervisor) or justify another maintained runtime. Validate signing/virtualization entitlement and actual guest boot on this Mac. Rust control may use a narrow Swift helper. Record licenses and pinned artifact provenance. Containers are not equivalent hardware isolation.
3. Implement a vertical slice: isolated ARM64 Linux image/kernel, private dedicated root, create/ready/exec/files/real PTY/stop/start/disk persistence; then outbound enrollment/controller routing with truthful backend/arch/images/resource capabilities and owner/local budgets. Existing typed homogeneous Linux checks may need bounded extension. Reject unsupported architecture and operations; never let an advertised image mask a wrong kernel/disk architecture.
4. Implement safe independent disk fork if demonstrated. Test RAM capture/restore/hibernate support on the actual framework/OS; expose capability denials where unsupported. Firecracker snapshots must not route to the Mac backend. Do not promise cross-architecture or cross-backend full-state portability. Test identity/entropy/key refresh and independent writes.
5. Evaluate Tahoe guest support separately using official Apple restore assets and current licensing. If legally/technically feasible on this hardware within budget, implement/test boot, guest access and cold persistence as a distinct profile. Stop before accepting new licensing terms on the owner's behalf, paid developer resources or unbounded restore downloads. Report precise blockers rather than silently substituting Linux.

## Required acceptance
Actual hardware tests: native CLI execution and verified TLS; bad CA/hostname rejection; enrollment/reconnect/revocation and stale-session fencing; owner-scoped placement and denied foreign requests; real guest `uname -m`, exec status, files and PTY resize/interrupt; cold disk persistence; capacity rejection with existing/unresolved demand counted; independent fork where supported; unsupported snapshot operations denied clearly; process cleanup confirmed, not inferred from disconnect. Keep local runtime evidence separate from remote gateway acceptance. Ask planner for a dedicated private invite when the local slice is ready; do not invent credentials or relax the existing gate.

## Host budget / stop gates
Initially at most ONE new guest, 2 vCPU, 2 GiB RAM, dedicated storage <=20 GiB after checking headroom. Lower the budget if the Mac cannot afford it. Obtain owner allocation before larger macOS guests/downloads. No existing VM/service interruption, host firewall/global route changes, public listener, paid resource, Cloudflare writes, deployment, push, main merge or source reuse with incompatible licensing. No secrets, user files, host IDs, disks or snapshots in commits. Block only the dependent step and continue independent work.

## Return
Scoped commit; docs/plans/mac-host-report.md and mac-host-handoff.md; ADR and operator instructions; exact commands/versions/results, test artifact hashes, timings with sample counts, supported/unsupported matrix and cleanup evidence. No fabricated p50/p95 or production-readiness claims. Planner reviews and integrates.

## Primary sources checked 2026-10-04
- https://developer.apple.com/documentation/virtualization/creating-and-running-a-linux-virtual-machine
- https://developer.apple.com/documentation/virtualization
- https://developer.apple.com/documentation/virtualization/running-macos-in-a-virtual-machine-on-apple-silicon
Recheck API availability on the actual host. Apple describes architecture-specific guest inputs; Rosetta translation of user binaries does not make x86 Firecracker VM snapshots portable.

## Prompt to give the Mac agent
Read docs/plans/mac-host-agent-handoff.md first. You are the builder for native Mac hosting, explicitly authorized by the owner to implement and test this bounded slice on this Mac. Follow its ordered context, budget, stop gates and required evidence. Work on your isolated branch, preserve unrelated services and return scoped commits/report/handoff to the planner. Do not stop at a design or at compiling the client.
