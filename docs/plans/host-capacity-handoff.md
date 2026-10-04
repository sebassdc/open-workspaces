# Handoff: adjustable host capacity / Ubuntu
Updated: 2026-10-04 | Size: M | State: complete implementation; planner review/merge/rollout pending

## Goal
Owner-authorized enrolled budget changes, larger shapes and clean Ubuntu dev on invited Linux hosts, with independent operator stopped-host consent.

## Where it stands
- Source frozen to build manifest SHA256 `8411c0c76b9a15582bc211f87a2b3b709b5213a7eaf0f3e8b7223c1a808054fe`; branch `work/capacity-core`, baseline `2e4c583`. Scoped commit is the branch HEAD after final report commit.
- Implemented shared ceilings, enrolled owner API/CLI/dashboard budget edit, stopped configure/update/rollback, verified optional seven-file Ubuntu/helper pair, streaming/sparse verification and bounded public hash cache; optional per-guest ceilings with legacy 2 GiB/4 CPU admission fallback. Exact source/build/script/artifact hashes and upgrade commands: `docs/plans/host-capacity-report.md`.
- Verified: 44 serial unit tests; real one-guest Ubuntu 4096 MiB/2 CPU authenticated API create/exec/stopped resize/cold persistence; original VMM unchanged; static full TLS download/configure/rollback; Chromium numeric form serialization; public HEAD/GET readiness; literal ingress regression. Strict Clippy failure (six existing style diagnostics) is explicitly reported, not a passed gate.
- Artifacts/logs private and ignored: lane `data/capacity-build/`, retained stopped real guest `v/`, synthetic stopped participation `h/`. Authoritative static `release-v4/build.json` + `bundle-v4/`; verifier-only same-source dynamic `dynamic-v4/build.json`. V3 stopped test trees are retained in `data/capacity-build/v3-vm-preserved/` and `v3-download-preserved/`. Earlier versions/failed fixtures are retained and superseded. No production/physical-host actions.

## Next step
Planner/reviewer finishes frozen independent source and no-VM acceptance against **v4**, then reviews scoped HEAD, merges and selects matching CLI/guest/bundle/current management descriptor pins. Preserve immutable prior receipts, enrollment, guest disks and later catalog writes. Follow exact owner/operator sequence and local tunnel-only allowlist delta in the report. Re-run new ingress/helper literal matching; no Access app/API/DNS/policy write is required. Deployment/publishing/service signaling remains planner-owned.

## Open decision
None. Planner approved lowering only while online with fresh idle inventory and zero registered/unregistered/pending/uncertain demand; offline decreases reject. Remote chosen budget still awaits owner CPU topology and explicit allocation, outside builder scope.

## Gotchas
- Stop with the **old CLI before installing the new binary**. Binary rollback needs compatible assets; no guest/catalog rollback.
- Lower while online idle **before** host stop/configure/start. Raising owner caps does not increase local consent. No automatic guest stop/restart.
- Short Unix socket paths: lane `data/v` was too long for random catalog IDs; ignored lane root `v/` works. Do not reuse original owner roots. Preserve all owner and retained test artifacts.
- Ubuntu raw disk is logical 8 GiB, about 1.8 GiB physical. Opt-in transfers 8,778,379,912 asset bytes; retained reserve is checked conservatively. Network helper is mandatory with Ubuntu, never host-extracted. Managed capabilities come only from verified manifest entries.
- Application-cache-cold header checks are not OS-cold benchmarks. Root tool outputs/compound exec do not establish dev-user sudo. Physical remote Ubuntu/larger-budget acceptance remains pending.
- Do not commit `.agent-protocol/` or modify/commit AGENTS instructions. No TASK_BOARD exists in this checkout; planner owns task-board reconciliation.
