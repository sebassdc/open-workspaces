# Handoff: multi-node architecture and local deployment
Updated: 2026-10-03 | Size: L | State: done (bounded same-host deployment)

## Goal
Deliver the owner-authorized Linux multi-node architecture, coordinate Herdr builders, independently verify, merge and deploy locally.

## Where it stands
- Done: implementation commit `7abe86d` cherry-picked into `main` as `4a48600`; all 31 merged product hashes match the independent accepted manifest.
- Done: locked unit suite 21 passed/0 failed/3 ignored; core real two-worker TLS/VM slice passed; independent 13 groups passed, including actual guest persistence, RAM/fork/PTY, owner denial, lost gateway results, browser key reuse, reservation failures and revocation.
- Done: [final review](multinode-review.md) cleared the bounded owner-only same-host pilot. Core and independent test guests stopped; independent cleanup reports zero host guests/reservations.
- Done: merged release installed, original catalog upgraded after offline backup/copy validation; all original user/resource mappings preserved (7 machines/17 snapshots). Gateway/tunnel configurations unchanged. Controller, both TLS agents, gateway/tunnel and three workers are running.
- Done: both deployed routes passed real guest exec and cold persistence; peak one 256-MiB guest, zero guests afterward. Planner independently confirmed service status, both nodes online, mapping equality, SQLite integrity, unchanged configs and matching release/product hashes.
- Budget: new nodes each 512 MiB/2 slots/2 vCPUs; legacy worker 4096 MiB/8 slots/16 vCPUs, total configured RAM 5120 MiB. Smoke guests are stopped; runtime disks remain.
- Not established: second physical host/NAT/public node ingress, real agent crash-pending recovery, production hardening, SSH or Mac workers. No pushes, paid resources or new outside-service writes authorized by this slice.

## Next step
Use the running pilot through the existing authenticated project origin. `python3 data/nl/manage.py status` checks owned processes; `login` opens the configured client login; `start`/`stop` manage this topology. Select node `a` or `b` for a new 256-MiB workspace. Positive public owner login remains untested. A second physical Linux host/NAT acceptance requires a new bounded host-access brief.

## Open decision
None; the authorized local slice is complete. New remote-host access, destructive migration, paid/external resources and unrelated/global host changes remain stop gates.

## Files
- Plan: [multinode-architecture.md](multinode-architecture.md); verification: [multinode-verification.md](multinode-verification.md).
- Architecture/operator guide: [ADR 0002](../adr/0002-bounded-linux-multinode.md), [MULTINODE.md](../MULTINODE.md).
- Builder evidence: [node-core-report.md](node-core-report.md), [node-core-handoff.md](node-core-handoff.md).
- Independent result (ignored): `data/mnv-vqn2fx79/result.json`; source manifest: `data/multinode-verify-build/source-manifest.json`.
- Deployment evidence (ignored): `data/nl/acceptance.json`; process manager: `data/nl/manage.py`; private rollback: `data/nl/rollback/`. `rollback` stops services/guests and restores schema 2 plus old binary/config; inspect the full results first. It archives schema 3, but newer ownership is hidden from old code.
- Prior pending tracked edits preserved privately under ignored `data/planner-multinode/`; preserve unrelated tooling/research files.

## Gotchas
Keep VM roots short on Btrfs; nested Unix sockets can exceed limits. Stop workers explicitly: disconnecting agents/controller leaves guests running. Catalog v3 rollback needs the pre-upgrade catalog and old binary with all writers stopped; never discard ownership of newer resources. A completed-journal lost gateway reply is different from an agent crash before journal completion. Static disjoint budgets require operator accounting for every worker on this host.
