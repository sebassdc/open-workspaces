# Handoff: Mac live pool acceptance
Updated: 2026-10-05 | Size: M | State: functional checks passed; trust acceptance pending

## Goal
Complete the required live acceptance of the native Mac node without changing Linux services.

## Where it stands
- Done: native build, private one-use enrollment, dispatchable acknowledgement,
  real dashboard/gateway create/exec/files/PTY, cold persistence and node reconnect.
  See [mac-node-live-report.md](mac-node-live-report.md) and [pins](mac-node-live-pins.json).
- Final state: `mac-arm64-pilot` participating; `mac-pool-check` stopped with its disk
  retained. All prior fixtures and three preexisting gateway resources preserved.
- Not started: dedicated second-owner isolation and coordinated active replacement/
  revocation. Browser Ctrl-C remains unconfirmed; actual gateway CLI Ctrl-C passed.

## Next step
Read the report's remaining gates; coordinate an authorized second test identity
and replacement/revocation invitation with the planner before running those checks.
Recheck live status first because participation may change after this report.

## Open decision
None for the completed bounded run. Trust tests require separate coordinated access;
the owner retains the test workspace unless they explicitly choose deletion.

## Files
- Plan: `docs/plans/mac-node-live-acceptance.md`; brief: `docs/plans/mac-node-live-brief.md`.
- Local worktree: `open-workspaces-mac-live`, branch `work/mac-node-live`.
  Tested base: `b5676b3fff8bd3acc438ccf00baed50c2b80b961`. The owner subsequently
  authorized direct publication of these sanitized documents to `main`; no PR.
- Sanitized outputs: report/pins above. Raw native receipt, credentials and command
  logs are private and excluded from git; do not copy them into a PR or transcript.

## Gotchas
Do not re-redeem the invite, reuse fixture enrollment, revoke uncoordinated, or move
offline resources to Linux. Final `host start` was detached in its own POSIX session;
no launch-at-login was installed. Compare the native helper `machine-id` for native
identity, not guest `/etc/machine-id`. Global filtered-network UI wording does not
describe the Mac backend (it has no external NIC).
