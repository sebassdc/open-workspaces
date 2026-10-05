# ADR 0003: Apple Virtualization for the native Mac runtime spike

Date: 2026-10-04. Status: accepted for an isolated local ARM64 feasibility
slice; private-pool backend integration is pending.

## Pattern and decision
Retain Rust for control and use a narrow, original Swift helper around Apple's
Virtualization framework. No new hypervisor or third-party orchestration source
is incorporated. This follows the explicit backend-capability direction in
ARCHITECTURE.md, while preserving ADR 0002's currently homogeneous Linux pool.
The Mac helper is not enrolled in that pool and no shared wire types change.

Use pinned Alpine 3.24.2 aarch64 netboot kernel/initramfs for an offline guest.
Extract the gzip payload from Alpine's EFI zboot wrapper; verify the resulting
ARM64 Linux Image header before direct boot. Append an original minimal init
script to the initramfs. A dedicated 256 MiB raw FAT block image exercises
/persist. The root filesystem remains ephemeral: this is not a developer-ready
persistent Linux root disk. FAT is a bounded fixture, not the selected production
workspace filesystem.

## Observed evidence
The signed native helper booted real aarch64 Linux 6.18.52-0-virt. Native Rust CLI
boot/restore, command status 7, binary SHA256 round trip, controlling console
resize and interruption, cold persistence and positive helper exit pass.

A paused VM was saved through Apple's native API, and its paired block image
was independently cloned on the local filesystem before resume. An original
machine identifier is retained privately; restoring without it failed with
invalid argument. With it restored, a shell variable existing only in RAM and
captured disk bytes both reappear. Parent and restored working-copy disk writes
diverge. One guest runs at a time during acceptance. This is same-machine state
recovery, not a verified identity-refreshed concurrent live fork.

The restore manifest verifies backend/architecture, exact OS version, shape,
private machine-identity hash and SHA256 of kernel, initramfs, state and paired
disk. Corrupt state and a newer unpaired disk are denied before restore. Hashes
protect artifact pairing/corruption, not against a malicious host operator.

## Boundaries and consequences
No network, shared directories, host credentials, host management socket or
public listener is attached. One root-wide lifetime lock prevents duplicate
fixture launches. The shape is fixed at 1 vCPU/512 MiB; the test disk is fixed
at 256 MiB. These are a local spike's constraints, not controller budgets.

Product capabilities for enrollment, memory snapshots, hibernation and live fork
remain false. The separate experimental local capture primitive is explicit.
Save/restore tests do not establish cross-host, cross-architecture, cross-backend
or cross-OS portability. The console is a controlling guest serial terminal;
the production guest-agent PTY/resize protocol remains unimplemented here.
Capture requires caller sync/quiescence and is crash-consistent, not a guarantee
of application consistency. Capture failures retain an incomplete directory and
terminate this helper; no crash recovery or automatic retention/GC is claimed.

The metadata-only Apple macOS probe reports supported Tahoe restore metadata
and 4096 MiB minimum RAM on this host. No restore image was downloaded or guest
installed. Its minimum exceeds the handoff's 2 GiB limit: owner allocation and
licensing review remain separate gates.

Apple's framework is supplied by macOS and used under the installed SDK terms.
The original helper/init are Apache-2.0 repository source. Alpine boot artifacts
retain upstream licenses (including Linux and BusyBox GPL terms); bytes are
kept in ignored storage, not committed or published. No Ignition/AGPL code reused.

## Primary sources
- [Apple: Linux VM direct boot](https://developer.apple.com/documentation/virtualization/running-linux-in-a-virtual-machine)
- [Apple: save machine state](https://developer.apple.com/documentation/virtualization/vzvirtualmachine/savemachinestateto(url:completionhandler:))
- [Apple: generic platform identity](https://developer.apple.com/documentation/virtualization/vzgenericplatformconfiguration)
- [Apple: macOS guests](https://developer.apple.com/documentation/virtualization/running-macos-in-a-virtual-machine-on-apple-silicon)
- [Pinned Alpine release directory](https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/aarch64/netboot-3.24.2/)

Installed SDK headers and actual hardware execution are the API and feasibility
evidence. See ../plans/mac-host-report.md for commands, results and limitations.
