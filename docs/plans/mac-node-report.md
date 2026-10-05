# Mac node integration — local acceptance report
Updated: 2026-10-05 | Branch: work/mac-node | State: local slice validated; live pool acceptance pending
Base: main 9acb963. PR6 published after explicit owner authorization. No merge, installer update or live deployment performed.

## Result and scope
The native release CLI, signed Swift helper, durable Mac worker and Linux ARM64
guest agent now implement persistent Ubuntu ARM64 exec/files/PTY through the
existing authenticated outbound node protocol. This is beyond the previous
console-only fixture. Local hardware and isolated real TLS controller tests
passed. Neither test establishes acceptance by the deployed human gateway,
owner dashboard or actual private pool. Those remain required.

Pattern: ADR0002 authenticated outbound nodes and SQLite owner/fixed placement,
ADR0003 Apple Virtualization, supplemented by ADR0004's explicit backend tuple.
The Linux controller and dashboard changes need planner review and x86 execution.

## Implementation
- Managed `host doctor/join/start/status/stop/configure`, explicit shared-pool
  consent, saved verified CA, bounded invitation file and one-use enrollment
  intent. Matching live process identity and fresh generation acknowledgement
  determine dispatchability; a PID or saved credentials alone do not.
- One native helper per VM, fixed virtio-vsock RPC/PTY ports, private Unix sockets
  with peer UID checks, bounded buffers/timeouts/concurrency, owned stdin lifetime
  and positive process/disk-lock exit evidence. No guest NIC, NAT, host mounts,
  host credentials, Docker socket or arbitrary host TCP proxy. Only guest
  loopback is enabled, required by local software/tests.
- Persistent independently copied 4 GiB ext4 disks, stable native machine ID,
  SQLite intent before disk creation/spawn, one concurrent guest, at most two
  retained guest disks. Unknown/incomplete/unregistered/legacy helper demand
  reserves capacity. Stop and worker-crash recovery retain files and identity.
  No APFS-clone or live-fork claim.
- Guest exec runs as dev (UID 1000) with real status, bounded output and session
  cleanup; sudo is deliberately available inside the guest. File operations
  anchor under /home/dev and reject traversal, symlinks and FIFOs. PTY supports
  resize, Ctrl-C, exit frames and actual EOF. Encoded JSON output is separately
  bounded, with a clear error when command effects may already have occurred.
- Exact Mac capability tuple `apple-virtualization/aarch64/apple-vz-v1`, prepared
  `ubuntu-arm64`, 512/1024/2048 MiB, 1–2 CPUs, one slot. The legacy Linux
  Firecracker/x86 tuple and missing optional operation field remain supported.
  Unsupported snapshot/hibernate/restore/fork/publish/SSH/tunnel/resize operations
  are denied. Guest SSH is explicitly false.
- Controller clamping, owner resolution, capability denial before dispatch,
  fixed placement and conservative reconciliation. Mac does not require an
  unsupported snapshot inventory call to become visible. Dashboard exposes the
  ARM64 image and compatible minimum RAM and hides unsupported actions.
- Bulk control reads use bounded peek/read without consuming following PTY bytes.
  macOS accepted Unix streams explicitly return to blocking mode: BSD inherits
  the listener's nonblocking mode, unlike the Linux assumptions.

## Observed evidence
| Check | Observed result | Limit |
| --- | --- | --- |
| Native hardware suite | 43 checks passed | One guest at a time, <=1024 MiB/2 CPUs in this run |
| Native TLS protocol suite | 16 checks passed | Isolated real controller + real guest, not deployed human gateway |
| Native Rust unit tests | 8 passed | Shared capability/framing paths available on Mac |
| Previous offline harness | 13 passed | Subprocess regressions; not VM acceptance |
| Linux ARM64 unit binary in real VM | 53 passed, 2 failed, 5 ignored | See baseline contrast below; not x86/KVM acceptance |
| Linux x86-64 test binary | Cross-build passed | Execution remains planner gate |
| Dashboard JavaScript | `node --check` passed | Browser/live dashboard acceptance pending |

Hardware checks include ARM64 exec exit 7; complete 64 KiB output with exit 3;
dev sudo/CA/package tools; escaped-output limit; 100 KiB binary roundtrip;
traversal/symlink/FIFO denials; exhausted capacity; eight unsupported operations;
actual PTY 99x37 resize, interrupt, exit 13 and EOF; cold file persistence and
machine ID; worker SIGKILL recovery; real externally started unregistered helper
capacity denial followed by owned cleanup; unregistered directory reservation;
a sequential 512 MiB/two-CPU guest; unchanged base disk; owned shutdown.

TLS checks cover wrong CA and hostname, redirect rejection, consumed/expired
invites, exact tuple and owner clamp, create/exec/binary files in the actual VM,
durable operation-key replay, offline guest root, replacement/revocation of an
active PTY proven by EOF, and retained disk/native identity after host stop.
The operator-controlled replacement agent used the same worker; it did not
start a second VM. Temporary controller/agents were cleaned by owned handles.

### Linux failures and baseline contrast
Both unchanged Linux public-asset-cache tests fail in this guest:
`onboarding::tests::queued_verified_cache_hit_rechecks_opened_file_identity` and
`onboarding::tests::verified_public_cache_invalidates_same_length_rewrites_and_replacement`.
A separately built clean **main 9acb963** ARM64 test binary was SHA-verified inside
the same guest and reproduced both failures individually (exit 101). This
establishes a baseline failure in this environment, not a new Mac integration
regression. The exact filesystem/timestamp cause has not been established.
Do not present the Linux suite as green or silently weaken the tests. Planner
must run/review x86 Linux compatibility, including these cache checks. Five real
KVM/browser/SSH environment tests remain ignored, not passed.

The catalog denial test additionally asserts no connection reaches either the
local inventory worker or the remote Mac worker socket. Its final strengthened
version is run separately and pinned with its result.

## Native attribution and provenance
Private receipts record exact source maps, CLI/signed helper/kernel/root/guest
hashes, real host OS/chip/build, compiler versions, headroom, commands and owned
process identities. Final hardware and TLS receipts each report unchanged source
through that run and the same CLI/helper hashes. The final catalog test-only
assertion (one targeted test passed) and correction of the hardware suite
docstring to its actual two-CPU budget are separately attributed; neither alters
the native runtime.
[Sanitized pins](mac-node-pins.json) contain hashes/results without private host
inventory, credentials, guest contents or absolute user paths. The Linux planner
has not independently verified the ignored Mac receipts.

The helper is ad hoc signed with the virtualization entitlement. `vtool`
observed linked minimum macOS **14.0**, SDK **26.4**. This does not prove execution
on a macOS 14 machine. No latency or p50/p95 benchmark is claimed.
Ubuntu Noble release-20260926 root/kernel/initrd were verified with their signed
SHA256SUMS and exact UEC fingerprint. ARM64 6.8.0-142 modules were verified through
signed noble-updates InRelease, Packages.xz and package hash. Exact inputs,
derived assets, guest hash and licenses are pinned. Original package ownership
and modes, including sudo setuid, are restored after unprivileged image creation.

Previous native artifacts were matched against the previously merged spike's
source hashes; attribution is retained privately. Superseded failures were kept
in logs/receipts: debug binary startup hashing, absent /dev, overwritten merged
/lib, inherited nonblocking sockets, stripped sudo mode/missing shadow, and
loopback initially down. Their fixes are reflected in the final passing runs.
Generated failed disks were retired only after positive stopped disk locks and
hash receipts; no shared service or guest was stopped. Dedicated retained storage
stayed within the authorized 20 GiB, with one guest concurrent. No host sudo,
cloud infrastructure change or macOS guest download occurred.

## Reproduce / continue
See [operator guide](../MAC_HOST.md) for build, private verified inputs and suites.
Private evidence under ignored `data/mac-node/` includes `local-final/`, `t5/`,
`baseline-cache-results.json`, build logs and failure/retirement receipts.
The final shared test revision and result are in the sanitized pins.

RELEASE and ACCESS gates in [authorized task](mac-node-integration.md) require a
stable local commit first, Linux planner review, a dedicated private invitation
and exact coordinated controller/frontend revision. Do not push/merge/deploy or
change live installers/assets from this lane. Next prove actual owner-scoped
human gateway/dashboard placement, persistence, denials, active stream fencing
and capacity on that revision. SSH/networking remain explicitly unavailable.

## PR6 feedback corrections
The four inline findings on e0af2e8 are addressed:

1. **Release/archive trust:** isolate GnuPG state, parse real primary fingerprints,
   export the pinned release key and require permitted successful `VALIDSIG`
   identities. Derive archive-keyring bytes directly from the authenticated root
   tar; reject a substituted optional external keyring. All trust/checksum
   checks raise explicit errors and remain active under `python -O`.
2. **Incomplete-create replay:** require complete private disk/config and proved
   running/stopped state before returning success. Incomplete/partial effects
   retain their artifacts and emit typed uncertain envelopes through the node
   journal; partial-copy/start errors retain the same uncertain classification.
3. **Owned stop:** guest sync is best-effort, including nonzero exit diagnostics.
   Always close the owned helper pipe, wait for actual exit and check the disk
   lock before releasing capacity. Stop returns the sync error separately.
4. **Aggregate admission:** one trusted user-wide private lock across data roots,
   held for the entire worker lifetime and inherited by its helper through the
   worker-death/host-shutdown interval. No PID-based adoption or signaling added.

Targeted regressions use real generated GnuPG keys (spoofed UID, extra signer,
valid signer, substituted archive keyring), interrupted-create persisted states
before disk creation and during copy with same-key node-journal replay, a Linux
catalog/node-journal uncertainty pipeline, and synchronized two-root fake startup
plus an inherited fake-helper reservation. Only the stop-RPC failure uses a real
VM: unlink the owned RPC endpoint, prove sync_error, helper exit/free disk lock,
retained disk and successful cold restart/persistence. No concurrent real VM is
used to test the admission race.

The existing official release signatures and archive signature were reverified
using the hardened verifier; derived image contents were not changed. A first
resumed hardware attempt exposed a harness assumption that an existing stopped
small guest would be running after idempotent create. Its failure receipt is
retained; the harness now explicitly starts that stopped guest. Review validation
and source/artifact/result hashes are recorded separately in sanitized pins.

Final correction validation: **43 hardware checks, 16 TLS checks, 8 native unit
checks, 4 trust regressions under Python -O and 13 previous harness regressions
passed**. Current Linux ARM64 suite: **53 passed / 2 unchanged baseline failures /
5 ignored**; the new catalog/node-journal replay test passed. Current x86 test
binary cross-build passed, with execution still deferred to the planner. Both
final correction native receipts have unchanged source maps equal to the final
committed source map. No live pool acceptance, merge or deployment was performed.
