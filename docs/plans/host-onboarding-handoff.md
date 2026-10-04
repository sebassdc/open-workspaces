# Handoff: invited Linux host onboarding
Updated: 2026-10-03 | Size: L | State: bounded release deployed; owner physical-host test pending

## Goal
Owner-approved easy Linux host invitations through the existing project domain. This machine coordinates a trusted shared private pool; joining hosts need no Tailscale/router changes or human Access admission.

## Where it stands
- Implemented and merged: core `6a6e3d0cc3f70a5c0f16930a17c70a9917685b29`, root cherry-pick `326204e`. Owner subject/catalog-ID authority, expiring node-bound invitations, enforced RAM/slot/CPU caps, hidden guided join, doctor/start/status/stop, private credentials/saved CA, verified minimal Alpine bundle, one native controller with exact public aliases. See [HOSTS](../HOSTS.md).
- Verified: 28 units passed/3 ignored; core real256MiB managed slice; independent focused V1–V6 passed. V1 uses a source-matched dynamic fixture; static CLI used for other executable gates. Reviewer compared all32 compiled inputs and accepted artifact hashes. [Review](host-review.md), [results](host-verification-results.md).
- Deployed: scoped gateway/controller/a-b agents and dedicated project tunnel/app only. Existing workers and Ubuntu VMM/process/disk/checkpoint preserved. Actual public default-TLS enrollment/WSS/revoke, protected human routes, served CLI/five runtime hashes and native a/b reconnect passed. Zero test workers/agents/guests remain. No push.
- Pending: owner second physical host/NAT workflow, positive owner invite UI after rollout, public job/workspace routing on that host and snapshot restore evidence. New downloaded host bundle advertises Alpine only. Mac hosting/SSH/marketplace remain separate backlog.

## Next step
Owner installs released CLI, runs `ow host doctor`, refreshes dashboard and creates a private invitation for a fresh node ID, then runs bare `ow host join` and `ow host status` on the second machine. Require controller_dispatchable=true/online. Test one Alpine256MiB/1CPU workspace explicitly placed there: exec/PTY, disk across cold restart, host stop/start, then revocation if desired. Preserve Ubuntu dev/checkpoint. Record actual results; do not infer NAT acceptance from local public-edge tests.

## Open decision
None. Owner will execute the second-host commands; no remote access exists. Paid resources/outreach/marketplace/unrelated services/privileged storage changes/push remain out of scope.

## Files
- Briefs: `docs/plans/host-{core,review,verify}-brief.md`, [plan](host-onboarding.md); core interface/report/handoff are committed in this root.
- Harness: `experiments/runtime-spike/host-onboarding-test.py`; final2 outputs `data/host-core-build/{release-final2,host-public-final2,ready-receipt-final2.json}`; earlier artifact pairs superseded.
- Private authority/baseline: `data/planner-host-onboarding/`; final aggregate `data/host-verify/minimal-acceptance-final2.json`.
- Private actual rollout/backups/online SQLite backup/descriptors/public receipt: `data/host-verify/release-20261003/`; operator runbook `data/host-verify/production-runbook.md`; owner command sheet `data/host-verify/second-host-commands.sh`.

## Gotchas
Use `python3 data/host-verify/scoped-channels.py status`; scoped restart/restore requires an exact role and verified descriptors. New channels use a separate immutable accepted binary; live workers' executable inodes remain intact. Never use wholesale `data/nl/manage.py stop/rollback`; it kills Ubuntu and its argv poll races exit. New rollback preserves later catalog writes and unrelated ingress/apps; old pre-cleanup backups cannot restore deleted disks. Real rollback was not executed; offline compensation/repetition tests passed. Firecracker comm is truncated; inspect process/config/locks and unknown demand. Guest test ceiling4/3072MiB includes Ubuntu2048, with new test peak3/768MiB and disjoint512/2slots/2CPU budgets. Keep all IDs/credentials/live config/assets in ignored storage and preserve preexisting pending files.
