# Handoff: asset download progress
Updated: 2026-10-04 16:13 America/Bogota | Size: S | State: done

## Goal
Show trustworthy terminal progress during host update-assets, preserving integrity
and stdout. Parent handles public CLI publication via gateway asset selection.

## Where it stands
- Done: presentation-only downloader progress, MiB/GiB single-row terminal bar,
  concise redirected stderr and explicit verification/publication phases.
- Done: pinned static CLI; five actual small trusted TLS cases passed, including
  live PTY and three corrupt/length failure cases. Exact stdout and sparse writes checked.
- Limitation: protected musl debug onboarding unit attempt failed to link SQLite
  UBSAN symbols (exit 101); no unit tests executed. Parent accepted executed CLI
  evidence for this scope; do not expand into toolchain repair.
- Done: formatting/diff/source/artifact checks; scoped branch work/assets-progress.

## Next step
Parent reviews/cherry-picks the scoped commit (`git rev-parse work/assets-progress`
in this lane), then publishes only the reviewed CLI through gateway asset selection.

## Open decision
None. Public publication belongs to parent; this lane performed none.

## Files
- Brief: /home/sebassdc/dev/open-workspaces/docs/plans/assets-progress-brief.md
- Report/pins/commands: docs/plans/assets-progress-report.md
- Changed: crates/ow/src/onboarding.rs, docs/HOSTS.md,
  experiments/assets-progress/tls-progress.py, report and this handoff.
- Static CLI: data/assets-progress-build/release/ow-linux-amd64
- Build receipt: data/assets-progress-build/release/build.json
- Passed evidence: data/assets-progress-build/tls-evidence-final/result.json,
  per-case stdout/stderr/live chunks; data/assets-progress-build/tls-final.log.
- Failed attempts: data/assets-progress-build/tls.log, tls-evidence/pty.*,
  tests.log and build.log. All artifact/evidence paths are relative to this lane.

## Gotchas
The initial TLS fixture incorrectly used CA as leaf; rustls correctly rejected
CaUsedAsEndEntity. Corrected fixture has a separate CA-signed localhost leaf;
trust was never weakened. CLI checks ran inside fresh private user/PID/mount
namespaces to avoid unrelated desktop inventory; no product inventory edits.
Synthetic bytes are not boot evidence. Keep generated TLS keys and all build/test
artifacts ignored. No VM/service/owner/remote/cloud or public-bundle work occurred.
