# Handoff: native Mac pool node
Updated: 2026-10-05 | Size: L | State: local implementation validated; live acceptance blocked by ACCESS/RELEASE gates

## Goal
Enroll the Mac in the private workspace pool and prove persistent Ubuntu ARM64 from the human gateway/dashboard.

## Where it stands
- Done: main updated to 9acb963; scoped work/mac-node implementation, native worker/guest/helper, managed onboarding, shared contracts/dashboard. See [report](mac-node-report.md) and [pins](mac-node-pins.json).
- Verified: 43 real hardware checks, 16 isolated TLS checks, 8 native unit tests, 4 trust regressions, 13 prior harness regressions. Linux ARM64: 53 passed, 2 baseline-reproduced cache failures, 5 ignored. x86 tests compile but are not executed.
- Pending: Linux planner review/x86 checks, coordinated test deployment and real human gateway/browser acceptance. Published PR: https://github.com/sebassdc/open-workspaces/pull/6. No merge or live changes.

## Next step
Planner rereviews the four corrected PR6 findings (trust, incomplete replay, sync-independent stop, aggregate admission), runs x86 Linux/controller checks and reviews the shared capability/catalog/dashboard changes; then supplies a dedicated private invitation and exact coordinated controller/frontend revision. Use docs/MAC_HOST.md to enroll a fresh private Mac root and perform the live acceptance in mac-node-integration.md.

## Open decision
NEED: dedicated private test invitation and exact reviewed controller/frontend revision.
WHY NOW: local slice is ready; live pool acceptance requires planner access/deployment.
CONTEXT: Mac VM and isolated authenticated transport pass. This is not deployed gateway acceptance. RELEASE prohibits this lane from pushing or deploying.
IF YES: enroll only that test node and run bounded live acceptance. IF NO: retain source/evidence and stopped private disks. DEFAULT: “ok” means wait for those artifacts, not invent credentials or deploy.

## Files
- Plan: docs/plans/mac-node-integration.md; brief: mac-node-brief.md; ADR: docs/adr/0004-mac-node-private-transport.md.
- Branch: work/mac-node; PR6; source groups: crates/ow*, native/macos, scripts, docs.
- Private ignored evidence: data/mac-node/local-final, t5, baseline-cache-results.json and build/retirement logs. Never commit them.

## Gotchas
Keep signed ow-vz beside release ow. Linux-built Darwin CLI alone is insufficient. Only ubuntu-arm64; no NIC/NAT/SSH/snapshots/live forks. Keep <=one guest, <=2 GiB/2 CPUs and <=20 GiB dedicated storage. Unknown demand blocks admission. Both Linux cache failures reproduce on main; cause remains unproven. Private receipts do not establish planner verification. Preserve the prior work/mac-host worktree and unrelated services.
