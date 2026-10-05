# Mac node live acceptance report
Date: 2026-10-05 | State: live functional checks passed; official acceptance pending

## Outcome
`mac-arm64-pilot` is enrolled and participating in the actual private pool at
1024 MiB, 2 vCPU and one running slot. A fresh controller acknowledgement reports
`controller_dispatchable:true`. `mac-pool-check` was created through the deployed
dashboard with `ubuntu-arm64`, ran on the native Mac worker, and retained its
files, native machine identifier and disk across cold starts and node reconnect.
It is now **stopped, with its disk retained**, while the node remains online.

These are real Cloudflare Access human gateway/dashboard observations, separate
from the previous isolated TLS harness results. Official acceptance remains open
because cross-owner checks and coordinated replacement/revocation were not run.
Browser Ctrl-C is also unconfirmed; Ctrl-C through the actual remote CLI PTY passed.

## Revision and native provenance
Main was fast-forwarded to `b5676b3fff8bd3acc438ccf00baed50c2b80b961` before the
build. PR6 merge `9e42263714a3c884fb745be1f31a5b595f8d90da` is an ancestor.
The complete receipt source map is byte-identical to that product merge and
unchanged throughout this run. No product code was changed.

Native build: `scripts/build-mac-host.sh`; native `ow host doctor` returned zero.
Build host: Apple Silicon, macOS 26.6.2, Swift 6.3, Cargo 1.99.0.
`ow-vz` has minimum macOS 14.0 / SDK 26.4 and the virtualization entitlement; `codesign --verify --strict` passed. Both
artifacts are Mach-O arm64;
the build's ad-hoc signature is not a Developer ID/notarization claim.

- Native CLI SHA256: `cd3816c9f57e080bb7a9377e857602311152970c6f309b97ec323a2c5730c43c`
- Signed native helper SHA256: `f9933fb141151fe42ea955f9ea3f82fbf0768547f98aa7b94ba5725e3673e0c0`
- Prepared manifest SHA256: `cd79cdf25df178c668115f397469b87815445b396eaa2c10c6be45318e3ec8b0`

[mac-node-live-pins.json](mac-node-live-pins.json) contains the exact source map,
guest/kernel/initrd/base-root hashes, input provenance and private receipt digest.
The already-reviewed ARM64 assets matched `mac-node-pins.json` and every prepared
file's size/SHA256. They were reused without regeneration or downloads.
The live disk is a new copy; its mutable final hash is confined to the private receipt.
The deployed Linux revision is reported by `mac-node-live-acceptance.md`, not
independently verified by inspecting Linux processes during this Mac-only run.

## Private enrollment and resource limits
The delivered `mac-invite.json` was collected from Downloads and moved into a
private directory (0700) with file mode 0600. Its contents were never printed,
published or included in this report. The expected node, origin and expiry were
validated privately. Enrollment used a fresh root, separate from all fixtures,
with `--accept-shared-pool --memory 1024 --cpus 2 --slots 1 --storage-gib 20 --no-start`.
The one-use invite was redeemed once; reconnect used saved credentials.

Preflight confirmed stopped prior fixtures using native process checks and
nonblocking VM locks. All prior disks were preserved. Physical retained fixture
storage was about 5.67 GiB; allowing the new 4 GiB disk remained below 20 GiB.
Host free disk was about 144.2 GiB and installed RAM 48 GiB; memory-pressure evidence
is private. Only one real guest ran concurrently; denied creates made no extra
physical machine/disk. The pilot holds one retained disk. These are headroom
observations, not performance benchmarks or guarantees about future workloads.

An initial `host start` returned success, but its detached supervisor did not
survive that tool execution. Its cause was not established. Saved participation
was resumed with supervised `host run`, then deliberately stopped/restarted for
acceptance. The final product `host start` ran in a separate POSIX session and
remained alive across subsequent checks; it is not attached to an active tool
session. No launch-at-login/reboot persistence was installed or tested.

## Observed product checks
Human access used the existing authorized dashboard session and successful
`ow login https://workspaces.sebashurtado.com`. No human login intervention was
needed. Node credentials were not substituted for human credentials.

1. **Dashboard create:** `mac-pool-check`, explicitly placed on `mac-arm64-pilot`,
   Ubuntu ARM64, 1024 MiB/2 vCPU. Gateway inspect confirmed `apple-virtualization`,
   `aarch64`, `ubuntu-arm64` and the selected node.
2. **Exec:** dashboard displayed Ubuntu 24.04.5 LTS, `aarch64`, 2 CPUs and about 957 MiB
   guest RAM. Its command returned exit 7. Actual gateway CLI exec returned exit 9,
   output and UID 1000. No inference from an accepted operation alone.
3. **Files:** actual gateway `put`/`get` round-tripped a 102400-byte synthetic binary
   byte-for-byte. Binary SHA256:
   `27783e87963a4efb6829b531c9ba57b44f45797f6770bd637fbf0d807cbdbae0`.
   Dashboard-created `/home/dev/ow-test/persistencia.txt` SHA256:
   `17f8e9bec843a24de61e00fc5eae2040272a125095748a5a361cdc920060ab96`.
4. **Dashboard PTY:** connected to `/dev/pts/0`; commands returned real output,
   `aarch64` and 2 CPUs. Viewport resize changed guest PTY from 40x125 to 32x103.
   Temporary viewport override was reset.
5. **Gateway CLI PTY:** `ow shell mac-pool-check` under a real local PTY;
   24x80 -> 37x99 resize, raw Ctrl-C during `sleep 60` produced shell status 130,
   then guest exit 13 propagated with positive PTY EOF (EIO), not a timeout.
6. **Cold workspace persistence:** dashboard Stop -> Start completed. Both
   text and binary hashes were identical after boot.
7. **Participation reconnect:** product `host stop` closed the browser terminal
   with `Disconnected. Reopen Terminal to reconnect.` The dashboard showed the
   Mac node/resource offline. Product `host start` reused saved enrollment;
   fresh matching channel generation/instance and `controller_dispatchable:true`
   were observed, then dashboard online. The workspace remained on the Mac and
   was started there. Both file hashes, physical machine record, native
   `VZGenericMachineIdentifier` hash, disk device/inode/size and directory inode
   matched. A reopened dashboard terminal printed `DASHBOARD_RECONNECTED`, the
   identical text hash and `aarch64`.
8. **Denials:** x86 Ubuntu on Mac and a second running ARM64 guest returned errors;
   SSH info, snapshot, fork and hibernate each explicitly failed through the real
   human gateway. No extra native guest was created.
9. **Offline network policy:** guest-root inspection showed only loopback, no
   external NIC/routes and no Docker socket. This is the supported no-NIC behavior;
   filtered Internet networking is not available on this Mac backend.
10. **Preservation:** initial/final gateway lists showed unchanged placement,
    state, image and budgets for all three preexisting resources. No Linux service
    changes, deployments, revokes, deletes or fixture retirement were performed.

Final stop was initiated through the dashboard; native SQLite reported stopped
and the VM lock was positively free before disk attribution completed. The source
map remained unchanged. The node stayed host-running and controller-dispatchable.

## Reproduction shape
Private absolute paths are intentionally omitted. These commands describe the
actual product operations; use the existing saved root, never redeem the invite again.

```sh
./target/mac-host/ow --data-dir "$OW_MAC_NODE" host status
./target/mac-host/ow --server https://workspaces.sebashurtado.com inspect mac-pool-check
./target/mac-host/ow --server https://workspaces.sebashurtado.com exec mac-pool-check -- \
  'sha256sum /home/dev/ow-test/persistencia.txt /home/dev/ow-test/gateway-binary.bin'
./target/mac-host/ow --server https://workspaces.sebashurtado.com shell mac-pool-check
./target/mac-host/ow --data-dir "$OW_MAC_NODE" host stop
./target/mac-host/ow --data-dir "$OW_MAC_NODE" host start
```

The workspace is currently stopped; start it through the dashboard before exec/shell.
The private run contains full commands/results, source attribution, process-start
identities and stopped-disk hashes. Private scripts/logs and screenshots are
excluded from git. No tokens, node credentials, host paths or raw inventory are
included in the publishable evidence.

## Remaining gates and known limitations
- **Cross-owner isolation before worker contact:** not executed; needs a dedicated
  authorized second test identity. No Access bypass or impersonation was attempted.
- **Active replacement and revocation:** not executed; requires planner coordination
  and a fresh invitation/identity after revocation. The reconnect close observed
  here does not prove either trust-boundary check.
- **Browser Ctrl-C:** automation keyboard attempts did not interrupt the guest
  sleep; it completed naturally with status 0. Cause is unestablished (browser input
  automation versus product behavior). Do not count this as passed. Actual CLI PTY
  Ctrl-C did pass; human browser keyboard verification remains useful.
- The global dashboard footer says “filtered Internet access” and the image label
  says “developer tools.” These labels do not establish Mac networking or a verified
  developer-tool suite. No UI source change was made in this acceptance lane.
- Reboot/sleep recovery and performance measurements were outside this run.

The enrolled node, credentials, new guest disk and old fixtures are retained.
Owner decides test-workspace retention. The owner subsequently authorized publishing this documentation directly to `main`.
Only the sanitized report, pins, brief and handoff are included; no product code,
private evidence or deployment changes are included.
