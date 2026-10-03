# Handoff: bounded Linux multi-node implementation
Updated: 2026-10-03 | Size: L | State: done; independent stable-commit review/merge pending

## Goal
Deliver the authorized working Rust Linux multi-node slice in `work/node-core`.

## Where it stands
- Implemented: node enrollment/TLS/outbound transport, generation/revoke, fixed
  placement, ownership/reservations/retry journal, CLI/dashboard and migration.
- Verified: locked/offline workspace 21 passed/0 failed/3 ignored; JS/diff checks;
  core real two-worker suite passed, peak 3 guests/768 MiB, workers sum1024 MiB.
- Independent node-verify: all13 groups passed, including signed human HTTP,
  Chromium reload/reopen, actual accepted lost create/exec replies, RAM/PTY/revoke.
- Product SHA-256 manifest exactly matches independent accepted source. No
  deployment/push or existing data/service changes. All disposable guests stopped.

## Next step
Planner/reviewer inspect scoped branch HEAD, clear final review, then planner
merges and handles authorized local deployment with private backup/rollback.
Remain available for bounded reviewer/verifier corrections; coordinate source
changes and exclusive guest turn first. Root task board is planner-owned.

## Open decision
None. Owner already authorized implementation; deployment is planner-owned review gate.

## Files
- Brief/interface: `docs/plans/node-core-brief.md`, `node-core-interface.md`.
- Operator/ADR: `docs/MULTINODE.md`, `docs/adr/0002-bounded-linux-multinode.md`.
- Full command/evidence report: `docs/plans/node-core-report.md`.
- Core private result: `/home/sebassdc/dev/open-workspaces/data/ncb5ca6775/result.json`.
- Independent result/manifest: `/home/sebassdc/dev/open-workspaces/data/mnv-vqn2fx79/result.json`.
- Review: `/home/sebassdc/dev/open-workspaces/docs/plans/multinode-review.md`.

## Gotchas
Keep short new Btrfs guest roots; long worktree roots exceed Unix socket limits.
Never shorten persistent IDs or adopt existing data. Same-host proof is not
physical/NAT evidence. Agent crash-pending exec stays unknown; bearer theft is
not attestation. Old binary needs the pre-v3 catalog backup for rollback. Older
VM suites can exceed the task ceiling. Uncommitted AGENTS/.agent-protocol are
copied instructions outside the scoped commit; preserve them.
