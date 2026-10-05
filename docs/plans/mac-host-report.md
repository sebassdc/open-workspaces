# Native Mac host — first implementation report
Updated: 2026-10-04 | Size: L | State: local runtime slice passed; overall task in progress

## Delivered
Isolated branch `work/mac-host`, baseline
`b1703f1b443c8380273eee3f9823e304170c95e5`.
Original Swift Virtualization helper and virtualization entitlement; native Rust
`ow mac-host capabilities|boot|restore|macos-probe`; pinned ARM64 boot preparer;
actual hardware acceptance script; [ADR 0003](../adr/0003-native-mac-runtime-spike.md)
and [operator instructions](../MAC_HOST.md). No shared node protocol changes,
main merge, push, enrollment or deployment.

## Actual evidence
Native Apple Silicon execution with Swift 6.3 / Cargo 1.99.0 on macOS 26.6.2;
Linux guest 6.18.52-0-virt, Alpine 3.24.2 ARM64 netboot inputs. Detailed host
capacity inventory is private ignored storage, not committed.

Commands run from the isolated worktree:

```sh
./scripts/build-mac-host.sh
python3 scripts/prepare-mac-host.py --data-dir data/mac-host-final
python3 scripts/test-mac-host.py --data-dir data/mac-host-final
CARGO_TARGET_DIR=target/mac-host-rust cargo test --locked -p ow
OW_SERVER=https://invalid.example ./target/mac-host/ow mac-host capabilities
./target/mac-host/ow mac-host macos-probe
```

15 hardware checks pass: native CLI boot; aarch64/controlling-console resize;
exit status 7; 1024-byte binary SHA256 round trip; duplicate-launch denial;
Ctrl-C interruption; paired paused RAM/disk capture; guest shutdown with process
exit; RAM-only shell variable and paired disk restore; cold persistence; clone
write independence; host SIGTERM exit; newer unpaired disk denial; corrupt state
denial; unimplemented capabilities disabled. The two macOS-compiled Rust
capacity tests pass. Linux-only controller/worker suites were not executed here.

Final single-run observed request-to-console readiness 0.219 s, paired capture
0.237 s, restore-to-helper acknowledgement 0.293 s. n=1 for each, warm inputs;
no p50/p95 or cache-cold benchmark claim. Restore's subsequent guest command
verifies RAM/disk usability separately. Test allocation is 1 vCPU/512 MiB with
256 MiB persistent block storage; no simultaneous second guest.

Pinned downloaded SHA256:
- vmlinuz-virt: `e45e1f6083d1ed45db6647b422e32b6ae6dc54de7b8190b7b97744fb293412e3`
- initramfs-virt: `ffe65ec5a0c0bf470042ad28f7ce7aa5f842ce8090e4230fb2703a7a34e1bebe`
- unwrapped Image: `e698a107e4d04117db1a7b0daee99bdae5f0647fba2af50f3cd020a666950ab9`

Pins were captured from the official versioned HTTPS release directory; this is
not independent signature validation. Derived fixture hashes and source/helper
pins are retained privately in `data/mac-host-final/`.

## Supported / pending
Working locally: hardware-virtualized ARM64 boot, shell commands/status, fixture
file transfer, controlling serial terminal, /persist cold persistence,
experimental same-identity paired state recovery and independent disk copies.
The RAM root remains ephemeral; FAT /persist is a fixture. Product guest-agent
PTY, persistent developer root image, filtered networking, capacity admission,
worker recovery journal, identity-refreshed concurrent fork and pool integration
remain pending. No Firecracker input or x86 VM state is accepted.

The current capability output deliberately denies pool enrollment, product
memory snapshots, hibernation and live fork. Experimental local capture is a
separate explicit field. No TLS/enrollment/reconnect/revocation/owner-placement
acceptance is claimed for this backend.

## Findings and cleanup
Disk locking must use a separate sidecar: flock on the disk itself prevented
Apple's attachment from starting. Native save succeeds but restore failed until
the original VZGenericMachineIdentifier was persisted and restored. Failed
initial capture artifacts remain private for diagnosis; the final acceptance
used a fresh root. Guest poweroff and owned host stop both wait for the actual
helper exit. No helper/guest remains running after acceptance; dedicated disks
and captures are retained. No unrelated process was stopped.

Apple's metadata-only probe reports supported macOS 26.6.2 restore metadata,
minimum 2 vCPU / 4096 MiB. No IPSW download, installation, license acceptance or
macOS guest boot occurred. This exceeds the initial 2 GiB memory allocation;
owner allocation/licensing gates remain before actual Tahoe installation.

## Next slice
Implement a persistent ARM64 guest profile and reuse the existing guest-agent
exec/files/PTY contract over a private native transport. Then coordinate backend,
architecture and image capability extensions with planner before private-pool
integration. Obtain a dedicated private invite only when the managed local
backend is ready. Remote acceptance and Tahoe remain separate gates.

## PR #5 review corrections — 2026-10-05
Integrated origin/main `fdbdf59` (including additive `guest_ssh_v1` contract)
into this isolated branch, then rebuilt the native helper and Rust CLI. Missing
`guest_ssh_v1` remains unsupported; this Mac slice does not announce SSH support.
No Linux worker or controller behavior was changed by these harness fixes.

P1: graceful shutdown now uses a best-effort nonblocking write. Closed/full stdin
cannot skip bounded terminate/kill/wait, and stream/selector closure runs in
finally. Marker-read failures also clean up their owned helper before raising.
P2: unmatched output is limited to 256 KiB and diagnostics to a 64 KiB tail.
Excess output fails before growing the matching buffer and triggers cleanup.

`python3 scripts/test-mac-host-harness.py`: five real-subprocess regressions pass
(closed stdin with forced kill, failed readiness with closed stdin, full stdin,
excessive output during a marker wait, excessive startup output). Each asserts
confirmed child exit and closed streams; output tests assert retained bounds.
The original PR harness fails the requested regression reproductions; evidence
is retained privately in `data/mac-host-final/review-baseline-regressions.log`.

Rebuilt against current main, then reran the 15-check real Mac hardware acceptance
using fresh `data/mac-host-review/` and one guest at a time: all pass. Two native
Rust tests pass; Python compilation, Rust formatting and diff whitespace pass.
Warm n=1 observed readiness/capture/restore acknowledgement: 0.224/0.266/0.360 s;
not percentile measurements. Private current artifacts/receipts remain ignored.
All test helpers/children have confirmed exits. Overall Mac-node work remains in
progress, with pool enrollment and the other previously listed limitations open.
