# Handoff: compressed Ubuntu delivery
Updated: 2026-10-04 21:39 UTC | Size: M | State: lane done; parent review/publication pending

## Goal
Deliver the reviewed pristine Ubuntu image with measured compressed transfer and bounded static CLI decoding, retaining raw old-client delivery and canonical local v2 bytes.

## Where it stands
- Done: product transport/decoder/gateway, codec pins, offline exact route intent, preparation and tests; branch `work/compressed-assets`, scoped commit at HEAD named `Implement bounded compressed Ubuntu asset delivery`.
- Done: 15 native onboarding tests; 21 reviewed static CLI synthetic TLS cases; full pristine built-in TLS decode and sparse 8 GiB publication. Original SHA `87ba61583f33e129f08776d1a4b8c1557322f146a7de0ed44581e2000f3f3f6b`; encoded 442,888,634 bytes; full compression ran once.
- Done: frozen outputs under `/home/sebassdc/dev/open-workspaces-lanes/compressed-assets/data/compressed-assets-build/`: `release-reviewed/ow-linux-amd64`, `bundle-reviewed/` (raw + compressed manifests/blobs, matching CLI/checksum/build). `final-checks.json` binds commit, source/artifacts/scripts/evidence.
- Not started by this lane: independent stable-commit clearance, deployment/public pins/headers and preservation checks. Parent owns them.

## Next step
Parent reviews stable commit + `docs/plans/compressed-assets-report.md` pins and `data/compressed-assets-build/final-checks.json`, selects `bundle-reviewed/` with the matching Linux CLI in the separate gateway bin contract, and adds only `/cli/host-compressed-manifest.json` and `/cli/host/ubuntu.ext4.zst` to the existing CLI ingress route. Retain all raw paths/bundles and macOS artifacts. No cloud APIs or unrelated host/service writes by this lane.

## Open decision
None for lane completion. Parent review/publication is the remaining release gate.

## Files
- Parent brief: `/home/sebassdc/dev/open-workspaces/docs/plans/compressed-assets-brief.md` (owner already authorized; do not ask for go again).
- Report: `docs/plans/compressed-assets-report.md`; operator contract: `docs/HOSTS.md`.
- Changed code/scripts: `crates/ow/src/onboarding.rs`, codec Cargo files, `scripts/prepare-compressed-host-bundle.py`, raw preparer directory fsync, ingress regex/legacy handling; focused experiments under `experiments/compressed-assets/`, existing source-check extensions.
- Successful receipts/logs: `data/compressed-assets/tests-reviewed.log`, `benchmark.json`, `full.json`; `data/compressed-assets-build/build-reviewed.log`, `prepare-reviewed.log`, `tls-reviewed/`, `tls-pristine/`; `data/compressed-assets/source-checks-reviewed.log`.

## Gotchas
- C1–C3 fixed: actual raw v1/v2 handler emits404 only for genuinely absent compressed metadata; unsafe/malformed metadata stays503; tests use directory helper without environment mutation; preparer rehashes staged copies before rename and fsyncs directory.
- Static build embeds source-map pin; do not edit Rust then reuse an old receipt. Native GNU tests avoid known musl-debug SQLite UBSAN link issue. The interrupted intermediate build correctly published nothing.
- Private TLS fixture keys and pristine image/artifacts are ignored storage, never commit them. Whole-image decode/hash timing includes encoded hash overhead; all timings are single run. Outputs are new and refuse overwrite; full zstd19 compression need not be repeated.
