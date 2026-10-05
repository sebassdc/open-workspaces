# Handoff: native Mac host implementation
Updated: 2026-10-04 | Size: L | State: in progress, first local runtime slice passed

## Goal
A native Mac node contributing real ARM64 Linux workspaces to the private pool;
separate Tahoe guest support evaluated within owner-approved allocation.

## Where it stands
- Done: original signed Swift helper, Rust local CLI entry point, pinned ARM64
  preparer, actual 15-check hardware acceptance and two native Rust tests.
- Verified: disk persistence, paired RAM/disk recovery, corruption denials,
  independent disk writes and confirmed guest/helper cleanup. No guest is running.
- Pending: persistent Linux root image, guest-agent exec/files/PTY transport,
  managed worker/admission/recovery, networking, shared capabilities and enrollment.
- Tahoe metadata supports this hardware with a 4 GiB minimum; no guest downloaded
  or installed. Existing initial memory allocation is 2 GiB maximum.

## Next step
Read [report](mac-host-report.md) and [ADR](../adr/0003-native-mac-runtime-spike.md),
then implement a persistent ARM64 Linux root image and the existing guest-agent
command/PTY contract over a private native channel. Coordinate any shared node
schema changes with planner; Linux pool remains homogeneous and untouched.

## Open decision
None for the next independent Linux step. Dedicated invite needed later for
pool acceptance; owner allocation/licensing review needed before Tahoe install.

## Files
Worktree `/Users/sebassdc/dev/open-workspaces-mac-host`, branch `work/mac-host`,
baseline `b1703f1b443c8380273eee3f9823e304170c95e5`.
Brief `docs/plans/mac-host-brief.md`; runtime `native/macos/`;
Rust `crates/ow/src/mac_host.rs`; scripts `scripts/*mac-host*`;
operator guide `docs/MAC_HOST.md`. Private receipts/artifacts/inventory:
`data/mac-host-final/`; prior experiment roots `data/mac-host*` retained.

## Gotchas
Use fresh acceptance roots; never replace captures or reuse host/user disks.
Persist original VM identity for restore. Lock sidecar, not block image itself.
FAT /persist is a fixture; RAM root is ephemeral. Serial tty is not the product
PTY protocol. Local recovery is not a verified concurrent identity-refreshed
live fork. Run one guest at a time across roots. Do not deploy/push/merge or
change existing hosts/services. Existing main remains at baseline.

## PR review follow-up — 2026-10-05
PR #5: https://github.com/sebassdc/open-workspaces/pull/5 . Updated branch includes
origin/main `fdbdf59` and both requested harness fixes. Five real-subprocess
cleanup/output regressions, rebuilt native artifacts, 15 hardware checks and two
Rust tests pass. Review receipts: `data/mac-host-review/`; regression command:
`python3 scripts/test-mac-host-harness.py`. Parent re-review remains pending;
no main merge/deployment, and overall Mac-node task remains in progress.

Second review corrections: independent exact RAM/disk probes and explicit
shutdown mode/status/marker evidence are implemented. 13 harness regressions and
the tightened 15 native hardware checks pass, with two confirmed guest poweroffs
and a separately recorded host stop. Current ignored evidence/pins:
`data/mac-host-evidence-final/`. Native lifecycle status formatting puts markers
on their own line. Parent re-review remains pending; scope unchanged.
