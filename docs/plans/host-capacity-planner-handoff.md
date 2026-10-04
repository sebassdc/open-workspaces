# Handoff: adjustable host capacity / Ubuntu
Updated: 2026-10-04 | Size: M | State: done; physical operator upgrade pending

## Goal
Owner authorized larger guests, adjustable node RAM/CPU/slots and optional Ubuntu dev on invited Linux hosts. Local implementation and publication complete; no existing Ubuntu migration selected.

## Where it stands
- Stable builder commit1ca48437a236bf8ebffe2b771538824e21e71ae7 cherry-picked as3e727e7. Accepted source8411c0c…8054fe and static CLI113dc71a…b90f55; all32 merged compiled inputs match. Final source reviewer and independent verifier pass. Core44unit tests, real4GiB/2CPU Ubuntu persistence and full8.778GB privateTLS download passed, with scope/limits in report.
- Public defaultTLS installer/CLI/checksum/7filemanifest and Ubuntu/helper HEAD passed; dashboard still Access302, native malformedGET405. Large public blobs HEAD-only. Controller/gateway and exact project CLI tunnel regex updated; no cloud API/DNS/Access/globalSSL writes.
- Original three workers and a/b agents unchanged; original Ubuntu2GiB/2CPU sameVMM/start/disk inode/size, fresh read-only exec passed. Physical node already offline at release, budget512MiB/2CPU/2slots and disks untouched; no post-release remote VM claim.
- Software ceilings: worker64GiB/64vCPU/8slots; perguest supported256MiB–16GiB/1–16CPU. Durable owner limit and stopped-host operator consent both required. Optional Ubuntu download ~9GB. Legacy guest-max fallback2048MiB/4CPU prevents unsupported admission before intent.

## Next step
Owner supplies `lscpu`/`nproc` on physical Linux host; recommend conservative RAM/CPU/slot allowance with desktop headroom. Owner raises enrolled budget via dashboard Adjust budget or authenticated `ow host budget NODE`. Operator stops participation before installing new published CLI, runs `host update-assets --ubuntu-dev`, explicitly configures agreed budget, then starts/status. Stop affects that host's guests. Create new Ubuntu on selected physical node; do not migrate/restore existing Ubuntu. Physical larger-guest/PTY/persistence acceptance remains owner-run.

## Open decision
None for completed implementation/release. Physical budget needs owner CPU inventory/allocation; no remote SSH or automatic64GiB allocation on46GiB machine.

## Files
[Rollout](host-capacity-rollout.md), [review](host-capacity-review.md), [verification](host-capacity-verification.md), [core report](host-capacity-report.md), [operator docs](../HOSTS.md). Private current selection `data/host-verify/current-capacity-release.json`; accepted/activation/public receipts under `data/host-verify/capacity-release-1791097374223248458/`. Private manager `capacity-manage.py status|restart ROLE`, prepared all-stopped recovery `capacity-cold-recover.py`. Original release retained; fresh online catalog backup under activation/backup.

## Gotchas
No push. Preserve pending AGENTS/README/ROADMAP/multinode and other untracked management files. Do not reuse supersededV1–V3 evidence. No systemd auto-start installed. Old managers are source-pinned to prior release; never wholesale `data/nl/manage.py stop/rollback`, worker down, whole-catalog restore or disk/snapshot rollback. New cold recovery is prepared/syntax-checked only and refuses any live runtime. Physical Ubuntu migration, Mac hosting, production readiness and agent crash-pending recovery remain outside this task. Strict Clippy has six reported prior style failures. Worker sessions can close once final reports are recorded.
