# Capacity release operator plan
State: deployed and publicly verified, 2026-10-04. Owner authorized limits expansion and adjustments. No guest migration, remote SSH, unrelated services or ingress changes.

## Before release
- Require stable scoped builder commit, independent source verdict, exact source/build manifest and focused executed authority/larger-guest/configuration/image evidence.
- Keep immutable previous accepted release and private online SQLite backup. Fresh private preservation baseline: data/host-verify/capacity-baseline-1791094233416570515/. Original owner Ubuntu and physical remote test guest remain running. Verify their worker/VMM/process/disk identity before/after; no stop/start of either during channel-only rollout.
- Re-capture actual gateway/controller/a-b/tunnel descriptors using PID/start/argv/executable identity. Do not invoke cold-recovery when channels are already live, do not use wholesale manager, and never signal stale descriptors.
- Source changes invalidate prior source-pinned management authority. Create a new immutable release receipt and current-management selection rather than overwriting old acceptance receipts. Future recovery/restart must select current accepted binary/config/source identity.

## Narrow release
- Stage reviewed static CLI and exact prepared images under new private immutable release directory; compare copied digests. Preserve Mac client artifacts. Only new Linux CLI/checksum and node host manifest/blob selection change.
- Restart gateway and controller with recorded same root, issuer/owner config and TLS trust; workers and owner guests continue. Old a/b agents should reconnect with prior small capabilities; reconnect physical host without re-enrollment. Restart a/b agent channels only if the actual release requires it, with exact descriptor validation. Only the dedicated local CLI route adds exact Ubuntu/network-tools filenames; other tunnel routes, apps, DNS and Access policy remain unchanged.
- Public default-TLS curl installer/CLI checksum and manifest verify; valid native path must still avoid human auth redirect; dashboard retains human protection. Check a/b and physical node online and current guest exec; no extra acceptance guest on physical host.

## Owner physical upgrade
- Collect lscpu/nproc, choose conservative disjoint RAM/CPU/slot allowance with desktop headroom. Software ceilings are not physical allocation.
- Owner raises node durable maximum using new authenticated budget API/UI/CLI. Local operator installs updated CLI and explicitly stops managed participation before configure/image update; this stops their test guest and must be clear in instructions. No automatic remote stop by planner.
- Operator configures chosen saved budget and requests Ubuntu assets using exact builder-supported commands, starts participation, checks effective capacity/image readiness. No identity/credentials/root change. New Ubuntu guest is created on that node; current Ubuntu migration remains a separate undecided task.
- Physical Ubuntu4GiB/2CPU, PTY, file persistence, snapshot/fork acceptance remains owner-run, not claimed from local dedicated4GiB test.

## Rollback
Restore exact previous gateway/controller binary/selection/config after live identity verification; retain catalog after new owner operations and current guest disks. Account for larger-host incompatibility with prior controller; do not claim transparent rollback if larger enrolled budgets/images require coordinated operator downgrade. No whole-catalog restoration or snapshot restore over current disk.

## Executed release

Stable core commit `1ca48437a236bf8ebffe2b771538824e21e71ae7` cherry-picked as `3e727e7`; all 32 compiled input hashes match accepted source `8411c0c76b9a15582bc211f87a2b3b709b5213a7eaf0f3e8b7223c1a808054fe`. [Final review](host-capacity-review.md) cleared; [independent verification](host-capacity-verification.md) passed its assigned scope, including mixed-version/pinned-local gates. [Core report](host-capacity-report.md) records 44 passing unit tests, 4 ignored, one separately executed real Ubuntu 4 GiB/2 CPU acceptance and full private-CA TLS download/configure/rollback. Strict Clippy remains failed on six prior style diagnostics.

Immutable current receipt: `data/host-verify/capacity-release-1791097374223248458/accepted.json`. Activation `activate-1791098503684878893/result.json` confirms controller/gateway and dedicated tunnel switched, original workers and a/b agent PIDs unchanged, same Ubuntu VMM/start identity and disk inode/size. Initial health probe omitted required gateway Host and returned421; corrected probe then passed. No product fix or additional guest was needed. Existing Ubuntu read-only exec returned x86_64, 2 CPUs and dev home present. No cloud API, Access/DNS/global SSL write, enrollment change or whole-catalog restore.

Public receipt `public-1791098627358131736/result.json`: default verified HTTPS installer/CLI/checksum/manifest200; static CLI SHA `113dc71a6fe879081b0118881ecbce61612af426d13dd0bc9a8802c091b90f55`; manifest SHA `89f78296c02093e91dc82bfef564381bb74f6ccaca339d1f4a1332178dce95f1`; exact Ubuntu/helper HEAD200 and logical sizes. Human dashboard302; malformed native GET405, without human auth redirect. Large public blobs were HEAD-only; full bytes/hash transfer evidence is the separate core private-CA run.

Physical remote node was already offline at fresh baseline and remained offline; no post-release remote guest continuity/Ubuntu upgrade claim. Its saved budget remains512MiB/2CPU/2slots. Owner must update stopped host and select a budget after CPU inventory. Original Ubuntu was not migrated. Private current pointer and manager: `data/host-verify/current-capacity-release.json`, `capacity-manage.py status|restart ROLE`. All-stopped reboot recovery: `capacity-cold-recover.py` (prepared and syntax checked, not executed after rollout). Retains old workers/agent binaries and current gateway/controller selection. Do not invoke old source-pinned manager or cold recovery while services are live. Previous immutable release and fresh online SQLite backup remain retained for scoped rollback; never restore current guest disks or catalog wholesale.
