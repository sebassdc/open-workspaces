# Node-core implementation report
Updated: 2026-10-03 | State: implemented, tested, ready for independent review/merge
Branch: `work/node-core`; baseline `a7333dbb96d2304b3e247c8c4b328e54b6e1d91c`.
Implementation commit is the scoped branch HEAD delivered to the planner.

## Implementation

- Separate native-TLS outbound controller/agent with private per-node Unix proxies,
  atomic expiring enrollment, 256-bit scoped credentials, live revocation and
  generation-bound one-use job channels; bounded control and guest PTY streams.
- Schema-v3 ownership-preserving migration, immutable placement, RAM/vCPU/slot
  reservations, owner-scoped request fingerprints/result status, durable node
  effect journal and conservative uncertain outcomes; no offline relocation.
- Selected/automatic compatible placement, same-node artifacts, bounded files,
  CLI retry keys/private intent and dashboard placement/reload key retention.
- Static fail-closed worker budgets, registered/unregistered demand accounting,
  free-storage preflight, legacy import safety and exclusive gateway catalog lock.
- Operator contracts: `docs/MULTINODE.md`; architecture: ADR 0002; narrow interfaces
  and bounded acceptance fixtures are included in this lane.

Product source is frozen at the independent verifier's tested manifest. A SHA-256
comparison of every file in `tested_product_source.files` from its result returned
**match: true, mismatched paths: []** before committing. Only documentation was
written after the source freeze. No copied AGENTS/.agent-protocol, credentials,
VM artifacts, external reviewer/verifier files or unrelated planner docs are staged.
Root task board updates belong to the planner; this isolated lane does not copy it.

## Commands and actual results

Working directory: `/home/sebassdc/dev/open-workspaces-lanes/node-core`.
Toolchain: `cargo +1.97.0`, existing installed Rust; no global installs.

```bash
export RUSTUP_HOME=/home/sebassdc/dev/open-workspaces/data/runtime-spike/rustup
export CARGO_HOME=/home/sebassdc/dev/open-workspaces/data/runtime-spike/cargo
export OW_ASSET_DIR=/home/sebassdc/dev/open-workspaces/data/runtime-spike
"$CARGO_HOME/bin/cargo" +1.97.0 build --locked -p ow
"$CARGO_HOME/bin/cargo" +1.97.0 fmt --all
"$CARGO_HOME/bin/cargo" +1.97.0 test --locked --offline --workspace -- --test-threads=1
node --check crates/ow/ui/app.js
git diff --check
```

Final locked/offline workspace suite: **21 passed, 0 failed, 3 ignored** in `ow`,
10.60 seconds; `ow-guest`: 0 tests. JS syntax and diff checks exit 0. The ignored
suites are the dedicated multi-node guest test and two pre-existing browser/user
real-VM suites. The older user/runtime suites were not run unchanged because
some phases exceed this task's four-guest/2-GiB ceiling.

Focused passing fixture groups include: v1 migration/ownership/restart; absent
legacy-worker import refusal; shared logical names/physical-ID denial; two distinct
captures; changed creation/fork fingerprint; immutable foreign-node child/snapshot;
unregistered CPU/RAM demand; caller-visible uncertain transport; start/restore
rejection with positively running/stopped target evidence; fail-closed limits;
eight-way enrollment race, expiry, replay, scoped auth/revocation/private files;
redirect refusal/handshake timeout; node/job/generation binding; active proxy input
and output fencing plus revoke; exhausted proxy permits; oversized/malformed
control/enrollment; persisted effect duplicate/conflict/unknown exec; worker-accepted
create/exec reply deliberately lost through actual controller job transport, with
catalog/journal reopening and exactly one simulated worker effect per intent.
The last case uses a **mock runtime**, not a guest or arbitrary external effect.

Core real guest command (executed before verifier's exclusive turn):

```bash
"$CARGO_HOME/bin/cargo" +1.97.0 test --locked -p ow \
  two_worker_tls_real_guests -- --ignored --test-threads=1 --nocapture
```

Result: **1 passed**, 21.40 seconds. Default fixture base was the shared ignored
root data directory derived from OW_ASSET_DIR; successful private result:
`/home/sebassdc/dev/open-workspaces/data/ncb5ca6775/result.json`.
For reproducible future runs, set `OW_NODE_TEST_DATA_ROOT` to a **new** short
private path below that data directory; an existing root is refused.
`OW_NODE_TEST_BINARY` can select the built binary. KVM/assets/Btrfs and a coordinated
exclusive host test turn are required. This test refuses any pre-existing
Firecracker process before starting.

Observed guest checks: two real rootless workers/agents and private-CA TLS;
wrong/default trust and actual certificate-hostname refusal; chosen/automatic
placement; cached duplicate create; owner denial; Linux exec, file transfer,
cold persistent disk; two named captures; same-node independent fork and changed
snapshot-key refusal; real `/dev/pts` output through the node channel; offline
fixed placement; reconnect, controller/catalog restart and revoke. Peak **3 guests,
768 MiB**. Workers each had 512 MiB/two slots/two vCPUs: configured sum **1024 MiB,
four slots/four vCPUs**. Both dedicated workers and all owned test children were
stopped. A post-test `ps -eo comm | rg firecracker` found no guest processes;
planner independently observed zero remaining host guests before releasing the
verifier's turn.

Earlier attempts: one failed because proxy readiness was not yet established;
one failed with `VM socket path too long` before booting any guest. The fixture
moved to a shorter Btrfs root and nodes become online only after their proxy exists.
Persistent physical IDs/socket checks were preserved. Dedicated failed fixtures
remain ignored; existing prototype data/services were never adopted or altered.
No benchmark distribution is inferred from test duration.

## Independent evidence (attribution)

Node-verify reported **all 13 acceptance groups passed** against the frozen product
source. Private result:
`/home/sebassdc/dev/open-workspaces/data/mnv-vqn2fx79/result.json`.
Its binary SHA-256 is
`5b6934a0f84f1fcdb5aef3e1915deb295862a43dc724c5b406b33d9bd0ebc6a2`.
These are independent verifier results, not executions by node-core.

Result groups cover real inherited/reported caps and distinct rootless networks,
enrollment/TLS/replay and hostname/redirect/plaintext denial, signed RS256 human
fixtures, chosen/automatic lifecycle/files/cold persistence/paired RAM snapshot/
fork/hibernate/restore, ownership with **zero denied-request dispatch delta**, real
routed guest PTY, start/restore reservation faults, Chromium exec reload and restore
dialog reopen reuse, offline/reconnect/controller/gateway restart, actual guest
create/exec accepted lost-gateway replies with one effect, duplicate-session active
stream fencing and durable revoke/PTY closure. It records peak three guests/768 MiB,
configured capacity sum1024 MiB, and zero guests/reservations before and after
cleanup. Lost-result tests observe completed agent journals before retry;
**agent crash before journal completion was not exercised**.

Root reports/harness are owned by other lanes:
`/home/sebassdc/dev/open-workspaces/docs/plans/multinode-review.md`,
`/home/sebassdc/dev/open-workspaces/docs/plans/multinode-verification-results.md`,
`/home/sebassdc/dev/open-workspaces/experiments/runtime-spike/multinode-test.py`.
The reviewer has not yet issued its final stable-commit deployment verdict.

## Operator handoff and limits

Follow `docs/MULTINODE.md` for listener, join/agent, private file, cap, gateway,
CLI, backup/checkpoint/rollback and shutdown commands. Review ADR 0002 and the
stable scoped diff; planner then owns merge and any local deployment. This lane
has performed no deployment/push, existing-catalog migration, firewall/global
privilege change, paid/external write or public ingress change.

This is a same-physical-host Linux proof. Physical remote/NAT behavior remains
unverified. No SSH/Mac hosting/marketplace/migration, cross-node snapshots,
identity rotation or disk adoption, off-node backup, production confinement,
storage quota, public owner Access login, immediate human-session revocation,
exactly-once external-effect guarantee or benchmark claim. Pending arbitrary
agent effects remain explicit uncertainty. Static disjoint worker budgets depend
on operator accounting for other workers/processes on a shared host. Free disk
headroom is a preflight, not a storage quota or atomic multi-process reservation.

## Resumption

Read `docs/plans/node-core-handoff.md`, MULTINODE and ADR 0002, compare the branch
with verifier's source manifest, and obtain the stable source review verdict.
Do not rerun VM suites while another lane owns the exclusive host turn. Make
only bounded reviewer/verifier corrections after coordinating a changed source
revision. Copied instruction/overlay modifications remain intentionally uncommitted.
