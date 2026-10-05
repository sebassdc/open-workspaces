# Mac node integration — authorized builder task
Updated: 2026-10-05 | Size: L | State: ready on owner's Mac
Pattern: ADR0002 authenticated outbound nodes + SQLite owner/fixed placement; ADR0003 Apple Virtualization backend; explicit backend capabilities.

## Goal and authorization
Owner authorized this next task after merging PR5. Turn the existing local Mac runtime into a real node in the private workspace pool: invite, `ow host join/start/status`, select Mac in dashboard, create persistent Ubuntu ARM64, exec/files/terminal and cold persistence. Work autonomously within the limits below; no repeated go for implementation. macOS/Tahoe guests, cross-backend state migration and live forks are separate tasks. Do not stop at a design, client compilation or another offline fixture.

## Start on Mac
Use a clean checkout; preserve any existing work before switching branches. Update main, create a separate worktree and give its agent the prompt at the end:

```sh
git switch main
git pull --ff-only origin main
git worktree add ../open-workspaces-mac-node -b work/mac-node main
cd ../open-workspaces-mac-node
./scripts/build-mac-host.sh
python3 scripts/test-mac-host-harness.py
```

The build compiles native Rust and Swift, signs the helper with the virtualization entitlement and writes `target/mac-host/ow` and `ow-vz`. Keep the helper beside the CLI. Existing Linux-built Darwin CLI artifacts do not contain the Swift helper and do not establish native VM acceptance. Pin/document a native helper minimum macOS version compatible with the save/restore APIs; the existing compile script uses the installed SDK/host target. No installer or live pool deployment is implied by these commands.

## Ordered context
Read AGENTS.md, README.md, docs/HANDOFF.md, docs/ARCHITECTURE.md, docs/adr/0001-local-runtime-and-storage.md, docs/adr/0002-bounded-linux-multinode.md, docs/adr/0003-native-mac-runtime-spike.md, docs/MAC_HOST.md, docs/plans/mac-host-report.md, docs/plans/mac-host-handoff.md, docs/HOSTS.md, docs/MULTINODE.md, docs/NETWORKING.md, docs/USERS.md, docs/SSH.md and docs/plans/ssh-guest-contract.md. Read installed/project builder roles if available; absence on the Mac does not block this explicit brief. Never assume untracked management files from the Linux workspace exist in your checkout.

## Current baseline and evidence
PR5 head f071bdb was merged as 3bd91552df50a22b4a57d411434054d84f7b0e37. It is an offline ARM64 runtime spike, not an enrolled node. Guest root is ephemeral, FAT /persist is a fixture, serial console is not product PTY, no network is attached, and product snapshot/hibernate/live-fork/enrollment capabilities are false. Paired local recovery is experimental same-identity behavior only. All four review findings were corrected; parent ran13 subprocess regressions. Mac-agent reports15 actual hardware checks/two native Rust tests, but ignored receipts/native helper hashes were unavailable to the Linux parent. Before building the next slice, record and verify previous native input/helper/CLI/result hashes against the merged tree; mark evidence unavailable if absent, never fabricate it.

Linux node/controller currently assume homogeneous Firecracker x86-64, including hard-coded heartbeat fields and compatibility validation. Existing x86 Ubuntu assets and Firecracker captures must not be used as ARM64 inputs. SSH source is integrated in main but live distribution/services have not been upgraded to it. Additive guest_ssh_v1 means fixed endpoint plus key/info operations, external snapshot-safe policy and identity/fencing behavior; missing capability denies SSH.

## Data flow / deliverable
```mermaid
flowchart LR
 C[Dashboard and CLI]:::old -->|owner scoped requests| G[Gateway and scheduler]:::new
 G -->|existing outbound authenticated jobs| A[Mac node agent]:::new
 A -->|private worker operations| W[Rust Mac worker]:::new
 W -->|bounded native control| V[Signed Apple VM helper]:::old
 V -->|private guest channel| U[Persistent Ubuntu ARM64]:::new
 classDef old fill:#eee,stroke:#aaa,color:#777
 classDef new fill:#dfd,stroke:#282,stroke-width:3px
```
Grey existing runtime/control surfaces; green implemented or extended in this task. No inbound Mac port or public guest listener.

## Implementation steps
1. Inventory actual chip/OS/SDK/toolchain, available RAM/storage and existing VM/process demand. Write the bounded design/contracts and supplemental ADR before implementation. Start with ONE new guest <=2GiB/2vCPU and <=20GiB dedicated storage after headroom checks. Use a fresh private APFS-capable root; do not assume Btrfs or Linux namespaces.
2. Build a verified official Ubuntu ARM64 persistent root/kernel with `dev`, trusted CA/package tools and architecture-correct guest agent. Record signed upstream provenance where available and exact derived hashes/licenses. Replace console-only fixture with actual command/file/PTY protocol over a private VM channel (e.g. supported virtio socket), including exit status, resize, interrupts and bounded binary data. Do not attach host directories, credentials or management sockets. Guest root must not weaken host policy.
3. Add managed Mac worker lifecycle, durable journal/placement metadata and truthful admission for CPU/RAM/slots/storage including legacy/unregistered/unresolved demand. Preserve disks/keys on host stop/start; unknown effects cannot be treated as stopped. APFS cloning needs real independent-write evidence. Advertise only supported operations; initial product RAM snapshot/hibernate/live-fork stays disabled unless separately implemented and fully validated within scope/budget.
4. Integrate Mac local consent/config/assets and existing one-use invite + outbound verified TLS/WSS enrollment/job channels. Extend shared backend/architecture/image capability contracts without relabeling Mac as Firecracker/x86 or breaking old Linux nodes. Scheduler/owning routes must reject incompatible image/architecture/backend operations before dispatch and never move an existing resource to another node. Route exec/files/PTY by fixed ownership/placement; retain generation fencing in both directions and bounded reconnect/revocation. Existing owner and local budgets both constrain admission. Send shared Rust/controller changes to Linux planner for its tests/review; do not assume Mac-only tests validate those cfg-gated paths.
5. Test local slice first, then deliver a stable scoped commit and request planner's dedicated private test invitation and exact controller revision for pool acceptance. Continue local/security/compatibility work while waiting. After coordinated test deployment, prove from gateway/dashboard that Ubuntu runs on Mac, files survive cold start, owner denials precede worker contact, stale active streams close on replacement/revoke, and capacity/architecture denials are correct. SSH is a follow-on capability inside this task only after backend key/stream/fork/restore rules are met; absence must be explicit, not fabricated success.

## Mandatory acceptance
Separate observed tests from code review, upstream documentation and proposals. Require real VM exec exit codes; binary files; real PTY/resize/Ctrl-C; persistent /home/dev across stop/start and worker recovery; owner-scoped gateway routing; wrong CA/hostname/redirect rejection; expiring single-use invites; active replacement/revoke closure proven by EOF/Close/reset (timeout is not closure); exact runtime and disk identities; exhausted/local/owner capacity rejection with unresolved demand; old Linux protocol compatibility and incompatible ARM/x86 image/snapshot denial; host policy isolation/denied private destinations once networking exists. Mock transport proves transport only. Gate networking on a native host policy that guest root cannot bypass; no unfiltered NAT advertised as equivalent Linux isolation.

Test SSH only with actual OpenSSH exec/PTY/SFTP, explicit public-key enrollment, host verification, external snapshot-safe revocation and per-resource identity. Unsupported SSH returns a clear capability error. Do not accept arbitrary host TCP destinations. Never upload client private keys.

Each native run records commit/source map, signed helper/CLI/kernel/root hashes, chip/OS/build/runtime/tool versions, headroom, exact commands/statuses and cleaned owned PID/start identities. Save private receipts in ignored dedicated storage and provide a sanitized hash/result summary for parent verification. Suite durations are not latency benchmarks; n=1 is not p50/p95. Failed attempts remain honestly attributed.

## Named stop gates
AUTHORITY: need weaker TLS/ownership/generation checks, public listeners, broad TCP proxy, host shared directories or an unverified networking isolation substitute. RESOURCES: existing services/guests affected, budget exceeded, paid tools or license acceptance requested. CONTRACT: shared backend/wire/schema conflict; propose minimal backward-compatible changes for planner review. RELEASE: stable code/evidence ready, stop before live controller/installer/assets changes, merge/push/main deployment. ACCESS: only live pool acceptance waits for a private invite; never invent credentials or disable authentication. Tahoe IPSW download/installation is outside this task.

## Return and done
Scoped `work/mac-node` commit, docs/plans/mac-node-report.md and mac-node-handoff.md, updated docs/MAC_HOST.md/operator guide and ADR/capability matrix, plus sanitized source/artifact/result pins. Planner reviews, verifies Linux-side compatibility and owns integration/deployment. Do not mark complete until actual private-pool acceptance passes, or accurately report remaining blocked acceptance and continue all independent work. No host inventory, secrets, guest disks/snapshots or user filesystem contents in commits. Coordinate SSH/Mac shared contracts; do not merge another lane yourself.

## Prompt for the Mac agent
Read docs/plans/mac-node-integration.md first, then its ordered context. You are the builder for the owner's explicitly authorized Mac-node integration task. Implement and test the full bounded vertical slice on this Mac on work/mac-node. Preserve existing services/guests, follow budgets and named gates, continue independent work without repeated approval, and return scoped source plus real hardware evidence/report/handoff to the Linux planner. Do not stop at the already completed PR5 runtime spike or claim an enrolled node until gateway acceptance passes.
