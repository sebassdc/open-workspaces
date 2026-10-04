# Handoff: host-core
Updated: 2026-10-03 | Size: L | State: done (bounded builder slice; release pending)

## Goal
Deliver owner-authorized guided Linux shared-pool join through the existing domain.

## Where it stands
- Implemented owner issuer/subject/catalog-ID binding, RAM/slot/CPU invitation caps,
  one native controller with exact public aliases, guided hidden paste, fixed
  verified assets, persisted scoped CA, recovery and identity-bound readiness.
- Current serial units: 28 passed, 0 failed, 3 ignored. Exact fixture teardown
  diagnosis and superseded builds are in `docs/plans/host-core-report.md`.
- Frozen final2 pair/bundle ready in ignored dedicated build storage; independent
  final acceptance remains pending. Core final4 managed smoke passed; both
  actual stops succeeded, Ubuntu identity preserved, all eight scratch roots
  removed. Core guest turn released; no further core guests. Release held.
- Not performed: public/cloud/service writes, push, second-host/NAT acceptance.

## Next step
Finish independent acceptance against final2 source/artifact receipt. Verifier
has the exclusive guest turn following positive core cleanup. Keep product frozen.
Planner owns reviewed merge, exact gateway artifact selection and scoped rollout.

## Open decision
None. Owner already authorized bounded work; public release is held by planner.

## Files
- Brief: `docs/plans/host-core-brief.md`, `docs/plans/host-onboarding.md`.
- Interface/report/runbook: `docs/plans/host-core-interface.md`,
  `docs/plans/host-core-report.md`, `docs/HOSTS.md`.
- Branch: `work/host-core` in `/home/sebassdc/dev/open-workspaces-lanes/host-core`.
- Private outputs: `/home/sebassdc/dev/open-workspaces/data/host-core-build/`;
  `ready-receipt-final2.json`, `release-final2/build.json`, `host-public-final2`,
  `restaged-serial-units.log`, `restaged-source-checks.log`, `smoke-final4/`,
  `cleanup-final.json`. Earlier smoke attempts failed before VM startup.

## Gotchas
Never stop legacy workers/Ubuntu or run the wholesale nl manager stop/rollback.
Keep old fixture evidence. Earlier v1/final outputs are superseded. CA fixtures
need a separate signed server leaf, not a CA end entity. Map drain precedes final
SQLite cleanup in alias fixture. UNKNOWN process evidence stays fail-closed;
only independently verified unrelated systemd/logind supervisors are resolved.
