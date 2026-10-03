# Multi-node verification and local deployment — bounded task

Owner authorization: 2026-10-03, implement, coordinate, merge and deploy on this machine.
Assigned builder: node-verify. Pattern: existing rootless real-VM regressions plus private project process deployment.

## Scope
Prepare independent acceptance checks while node-core implements in `/home/sebassdc/dev/open-workspaces-lanes/node-core`. Read its eventual `docs/MULTINODE.md` and ADR 0002, and root `docs/plans/multinode-review.md`. Own only a new focused experiment script (if useful), `docs/plans/multinode-verification-results.md`, and deployment artifacts under ignored project data. Do not modify product Rust, shared task board, existing files or credentials. Do not commit until the planner requests a bounded verification commit.

Coordinate through report files; do not type into working agents' composers. Record architecture/commands needed early. Wait for a coherent implementation before running acceptance. Planner will explicitly release deployment after review/merge; do not deploy early.

## Acceptance checks
- Two distinct outbound worker identities, isolated private roots and rootless networks.
- Invalid/expired/replayed join credentials rejected; node revocation and duplicate-session fencing tested.
- Placement on chosen and automatically selected capable nodes; insufficient/offline capacity rejected before side effects.
- Real guest boot/exec/file content, cold stop/start persistence, snapshot/fork on assigned node and routed guest terminal data.
- Cross-owner operations and raw physical-ID guessing denied before node dispatch; node inventory cannot claim other-node assignments.
- Disconnect, reconnect, controller restart and a lost create response retain placement/identity; no blind reschedule, duplicate machine or automatic exec replay.
- Existing catalog upgrade and legacy single-host regressions remain valid.
- Record which cases use real workers/VMs versus simulated failure; two workers on one machine are not independent physical/NAT proof.

## Host and deployment boundaries
At most four running test guests and 2 GiB aggregate configured RAM across workers. Preserve `data/prototype`, desktop, unrelated services, host firewall and public Access configuration. Use shared Rust/assets from root `data/runtime-spike` and separate ignored experiment roots. No privileged installation, paid resources, DNS/Access mutations, remote host access or pushes.

Once planner releases deployment: build reviewed merged code, preserve rollback binary/config and an offline catalog backup before any migration, start only dedicated project processes with private credentials and loopback listeners. Verify controller/node health and a real workspace; retain intended local service and stop disposable test guests. Record process management, stop/start commands, rollback, logs and remaining public/physical-host acceptance. Never print or commit tokens, host identifiers or runtime artifacts.

## Return
Exact check commands/results, reviewed limitations, artifact paths, deployment/rollback status and resumable handoff. Report blockers accurately rather than weakening acceptance or claiming API success establishes a guest.
