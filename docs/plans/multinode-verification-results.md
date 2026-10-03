# Multi-node independent verification results

Updated: 2026-10-03 | Verifier: node-verify | **Current state: released local deployment running; smoke guests stopped; no commit made.**

## Running deployment and preserved usability

Planner/reviewer released bounded owner-only same-host deployment of core **7abe86d37305056558e5e1ebbc1ec16e8f2d3f33**, merged by planner into root **4a486001066e9ed81a691d1e43ce5c108ab4c6e2**. All **31 tested product-file SHA-256 hashes match both commits and root source after release build**. No product files, task board or other agents' files were edited by verifier. Existing unrelated pending changes are preserved; verifier harness/report remain uncommitted for planner review.

The original **data/prototype** catalog is now schema **3**, upgraded non-destructively from schema 2. Ordered user identity and owner/kind/name/physical-ID mappings were checked against the private pre-migration baseline: **all unchanged, 7 machines and 17 snapshots before and after**, every original resource on node **local**. Existing machines/snapshots remain visible through the existing authenticated project origin. The rejected empty isolated-catalog rollout was not used. No legacy guest was booted, relocated, resized or automatically resumed; legacy disks/checkpoints stayed in their original directories. Catalog metadata that said “running” before startup was not live-VM evidence: host Firecracker count was zero.

Services left running: original legacy worker; dedicated private rootless workers **a** and **b**; native TLS node controller at **127.0.0.1:8790** using the original prototype catalog; two outbound CA-verifying agents; original-root gateway at **127.0.0.1:8787**; existing project Cloudflare tunnel. Gateway/tunnel config bytes match backup exactly; existing Access configuration was retained. No DNS, Access policy, cloud resources, paid writes, global configuration or unrelated service changes. Private tunnel log shows registered connections. Native controller rejects an anonymous enrollment request with 401; gateway rejects anonymous origin requests with configured Host with 401. The public anonymous HTTPS probe returned **403**; this alone does not prove a successful owner login or attribute the denial to a particular edge component.

**Actual client path:** use the existing configured project HTTPS origin in the browser, or run `python3 data/nl/manage.py login` to pass that same privately configured origin to the native client. Then use `./ow list`, `./ow snapshots`, existing workspace operations, or `./ow create NAME --node a --memory 256` / `--node b`. No synthetic identities/JWKS shim are present in deployment. Positive authenticated owner login is still pending; no real owner session was available to verifier. Local operator inventory: `./ow --local --data-dir data/prototype nodes`; original local maintenance remains `./ow --local --data-dir data/prototype ...`. New worker maintenance uses `--data-dir data/nl/a` or `b`; direct operator machines are not tenant catalog assignments.

## Limits and deployment smoke evidence

New workers each explicitly report **512 MiB / 2 running slots / 2 vCPUs**, verified against their real worker `/proc` inherited OW_MAX_* environment. Both have private 0700 roots and distinct rootless network namespaces, separate from host. Their aggregate admitted maximum is **1024 MiB / 4 slots / 4 vCPUs**. Original legacy worker explicitly retains **4096 MiB / 8 slots / 16 vCPUs** to preserve usability of existing shapes, including a 2048-MiB machine. **Total retained worker hard RAM ceilings are 5120 MiB**, including legacy; these caps are admission ceilings, not allocated guest memory or cgroups. Predeployment host RAM: 31993 MiB total / 24707 MiB available. No legacy guest was started for this test.

Deployment smoke was **serial**, actual peak **1 guest / 256 MiB / 1 vCPU**, within the released three-guest/768-MiB planned peak and host four-guest/2048-MiB test bound. Host demand was resolved before launch and between operations, with live missing configurations blocking admission. On each real TLS node route, the operator proxy created a disposable 256-MiB guest, executed `uname -r` and a route-specific nonce, wrote persistent bytes, stopped/cold-started and reread bytes, then stopped it. These are **real routed worker/VM health checks**, not production human catalog placement. Stopped `deployment-smoke-a` / `deployment-smoke-b` disks remain only in new dedicated roots for evidence; no user catalog entries were added. Both agents online afterwards; all three workers report zero running guests; final host count **zero Firecracker guests / zero configured MiB**. No cleanup errors.

Private actual deployment artifacts: `data/nl/acceptance.json`, `deployment.json`, `release-sha256.txt`, `logs/`, `processes/`, `private/`, `rollback/`. Credentials and TLS private keys are 0600 under 0700 directories; enrollment files removed after successful joins. TLS certificate lifetime is 365 days; rotate before expiry. Existing managed ingress logs/descriptors remain under `data/remote-access/`. None of these ignored private artifacts should be committed or exported.

## Executed build, backup and private process management

Release SHA-256: **e22658f7291f1403cbba7d60d44a7596fc4edc6e0ac59220cda5e0379b6ae2c0**. Release built locked/offline from merged root in a separate target, then copied to `data/nl/bin/ow` and atomically installed at `target/release/ow` so existing gateway manager uses the reviewed binary. Shared guest assets were reused without rebuilding them.

```bash
env RUSTUP_HOME=/home/sebassdc/dev/open-workspaces/data/runtime-spike/rustup CARGO_HOME=/home/sebassdc/dev/open-workspaces/data/runtime-spike/cargo /home/sebassdc/dev/open-workspaces/data/runtime-spike/cargo/bin/cargo +1.97.0 build --locked --offline --release -p ow -j 2 --target-dir /home/sebassdc/dev/open-workspaces/data/nl/build
python3 data/nl/upgrade.py
python3 data/nl/manage.py start
python3 data/nl/smoke.py
python3 data/nl/manage.py status
```

`upgrade.py` is an executed one-shot preparation artifact, **do not rerun against this live deployment**. Before any original migration, confirmed managed gateway/tunnel/legacy worker stopped; checkpointed original SQLite WAL with busy=0; used SQLite backup API; verified integrity/schema2; privately preserved old binary, configs, process descriptors and ordered mapping baseline. Actual old binary backup digest checked against private manifest. Validated a **disposable backup-copy migration** with real new gateway before original startup: schema3, integrity OK, all user/resource mappings/counts unchanged and local route retained. Backup is catalog/config/binary protection, not a copy of VM disks; those are preserved in place.

Private manager commands (run from repository root):

```bash
python3 data/nl/manage.py status
python3 data/nl/manage.py start    # idempotent when owned services are already running
python3 data/nl/manage.py stop     # planned project shutdown, including workers/guests
python3 data/nl/manage.py rollback # offline original binary/schema2 restoration
python3 data/nl/manage.py login    # existing authenticated project origin, read privately
```

Manager verifies UID, PID/start ticks and complete argv before signaling owned controller/agents; existing `scripts/remote-access.py` verifies the original project's gateway/tunnel descriptors. Worker start/down uses each explicit private data root, not broad process-name kills. No global/systemd service installed. Processes are detached, **not automatically restarted after host reboot or unexpected death**; run status/start to recover, inspect private logs on refusal. Start sets each worker/agent's disjoint explicit environment and verifies reported caps before agents. A second `manage.py start` was executed successfully: existing gateway/tunnel retained, no duplicate services or guests. Existing controller/gateway retain original prototype root. Logs: `data/nl/logs/controller.log`, `agent-a.log`, `agent-b.log`, `legacy-cli.log`, `a-cli.log`, `b-cli.log`; worker-private logs inside corresponding runtime roots; ingress logs `data/remote-access/gateway.log`, `tunnel.log`. Stop/rollback are deliberate maintenance operations: quiesce user activity first because worker down stops project guests.

**Rollback prepared, not exercised on the live service.** Manager stops only verified project ingress/agents/controller then all three owned workers, archives schema3 catalog, restores checkpointed schema2 backup and old root binary/config, and removes stale SQLite sidecars only after writers stopped. New node roots/journals/disks and legacy runtime artifacts remain in place for forward recovery. Backup: `data/nl/rollback/prototype-catalog-v2.sqlite3` (integrity OK, schema2), old binary `root-ow`, private config backups, `ownership-before.json`, `manifest.json`. After rollback, use old `./ow --local --data-dir data/prototype up` with prior legacy limits and `python3 scripts/remote-access.py start` to recover original-origin service; do not use new-node manager start with old binary. Restoring an old catalog after new user activity can hide later mappings; the archived schema3 catalog/node artifacts are required for forward recovery. **Not a lossless rollback guarantee.** Original gateway/tunnel/legacy worker were stopped at deployment baseline, so rollback leaves restored originals stopped until deliberately restarted.

## Acceptance summary and remaining boundaries

Independent full harness: **13 groups passed, 0 failed**, real TLS/agents/guests and actual Chromium, cleanup zero guests/reservations/errors. Frozen-product workspace unit regression: **21 passed, 0 failed, 3 ignored**; `ow-guest` zero tests. Core-authored transport/reservation mocks and planner-reported core real guest suite are separate evidence. Frozen-product unit command used the separate target:

```bash
env RUSTUP_HOME=/home/sebassdc/dev/open-workspaces/data/runtime-spike/rustup CARGO_HOME=/home/sebassdc/dev/open-workspaces/data/runtime-spike/cargo /home/sebassdc/dev/open-workspaces/data/runtime-spike/cargo/bin/cargo +1.97.0 test --locked --offline --workspace --manifest-path /home/sebassdc/dev/open-workspaces-lanes/node-core/Cargo.toml --target-dir /home/sebassdc/dev/open-workspaces/data/multinode-verify-build/target -- --test-threads=1
```
 Deployment adds original schema2-to3 mapping preservation and serial real guest health on both retained routes.

Remaining acceptance: real authenticated public owner login/reopen on existing origin; second physical host and real NAT; agent crash-pending recovery before durable result commit; an executed live rollback; production VMM jailer/cgroup/storage hardening. Public 403 and synthetic signed HTTP/browser fixtures do not prove real Access login. Gateway-loss-after-agent-journal-completion checks do not prove agent-crash recovery. Same-host namespaces do not prove separate physical host isolation. Existing legacy files/mappings preserved, but verifier did not resume a user machine or restore a user snapshot for a destructive usability test.

## Final independent acceptance evidence


Commands:

```bash
env RUSTUP_HOME=/home/sebassdc/dev/open-workspaces/data/runtime-spike/rustup CARGO_HOME=/home/sebassdc/dev/open-workspaces/data/runtime-spike/cargo /home/sebassdc/dev/open-workspaces/data/runtime-spike/cargo/bin/cargo +1.97.0 build --locked --offline -p ow -j 2 --manifest-path /home/sebassdc/dev/open-workspaces-lanes/node-core/Cargo.toml --target-dir /home/sebassdc/dev/open-workspaces/data/multinode-verify-build/target
python3 experiments/runtime-spike/multinode-test.py --binary data/multinode-verify-build/target/debug/ow --assets data/runtime-spike --source-manifest data/multinode-verify-build/source-manifest.json --vm-turn-released
```

Binary SHA-256: `5b6934a0f84f1fcdb5aef3e1915deb295862a43dc724c5b406b33d9bd0ebc6a2`. Frozen source manifest: `data/multinode-verify-build/source-manifest.json`; copy retained in result JSON under `tested_product_source`. It covers Cargo manifests/lock plus all 31 product files, including new Rust test modules and UI assets. Baseline commit at capture was `a7333dbb96d2304b3e247c8c4b328e54b6e1d91c` with the pending node-core implementation; **no product hashes changed during build/test**. All 31 hashes were subsequently compared against core commit and merged root commit, with zero mismatches.

Private ignored artifacts: `data/mnv-vqn2fx79/result.json`, `browser-result.json`, `browser.log`, per-process logs, private controller/worker catalogs, node effect journals and VM/snapshot artifacts. Credentials/signing keys remain private and must not be committed/exported. Script/result flags describe a conservative planned ceiling of three guests/768 MiB; worker reported caps and inherited environment enforce disjoint 512 MiB/two-guests/two-vCPUs each (aggregate configured hard maximum 1024 MiB/four guests/four vCPUs). Serial fixture scheduling stays below the requested three-guest ceiling. No latency benchmark claim: summed grouped-check execution 52.95s is just run bookkeeping, not p50/p95 performance data.

| Passed group | Independent observation and classification |
| --- | --- |
| Worker caps/isolation | Both real rootless workers report and inherit 512 MiB/two guests/two vCPUs; private roots, distinct network namespaces, no host namespace sharing |
| Enrollment | Real outbound agents over private-CA TLS; default CA trust denies enrollment; wrong token lengths, expired join, replay and wrong-node redemption denied; private credential files |
| TLS boundaries | Production client refuses credential redirect, trusted-CA wrong hostname and remote plaintext; local negative HTTP/TLS fixtures |
| Human authentication | Actual HTTP gateway verifies two synthetic RS256 Access-style identities and denies unsigned requests; issuer/JWKS routing shim only in gateway process |
| Guest/artifact behavior | Explicit node A and automatic node B; real guest exec, 4096-byte routed binary transfer, cold disk persistence, two named captures, restored live sleeper process, same-node fork write independence, hibernate/resume/restore; changed snapshot retry and existing child on another node denied without placement/reservation change |
| Ownership/capacity | Cross-owner lifecycle/exec/snapshot/fork/restore/PTY and raw physical-ID denial without mutation journal growth; same names have independent real guest files; node/human credentials cannot cross auth surfaces; memory/CPU denial leaves worker inventory unchanged |
| PTY | Actual gateway WebSocket through node job to guest; isatty and expected owner file output |
| Rejection reservations | Missing disposable child disk causes start rejection, with positive stopped inventory releasing RAM/CPU; temporarily missing capture rejects restore while the original real guest stays reachable and retains 256 MiB/one-vCPU reservation; original artifacts restored |
| Browser keys | Actual Chromium page reload retains exec key after intentionally losing a completed HTTP result; retry uses same key and guest counter stays one; actual restore dialog cancel/reopen reuses key and success clears saved keys |
| Offline/restart | Real agent disconnect rejects operations/placement without new mutation journal effects; reconnect plus controller and gateway restart preserve physical identity, node and real guest bytes |
| Lost gateway exec result | Kill gateway after real agent journal acceptance; observe agent journal completion before retry; restarted HTTP state exposes uncertainty only to owner; same key resolves original result and guest side effect remains one; changed request rejected |
| Lost gateway create result | Same gateway-loss fault with real create, fixed node/physical identity and exactly one matching runtime guest after retry; stop disposable guest |
| Fencing/revocation | Old main session has actual EOF/Close; freeze old real agent with its live guest job open, inject labeled authenticated replacement main session, and observe active guest-stream EOF/Close independent of killing agent job; revoke closes real PTY, blocks dispatch/reconnect and survives controller restart |

Remaining limits: this is **one physical host**, not second-host/NAT/public Access acceptance. Browser edge and JWKS are synthetic test fixtures; active replacement uses a synthetic authenticated main session with a real existing guest job. Lost-result checks demonstrate **gateway loss plus completed agent journals**, not agent crash-pending recovery before journal commit. Malformed/saturation/forged foreign inventory/concurrent-placement and agent-pending cases still rely on separate core/reviewer fixture evidence or remain unexecuted by this harness. VMM jailer/cgroup/storage hardening, production resource isolation, executed production rollback and cross-host snapshot portability are not established. Unit/core VM results are separate evidence, not substituted for the verifier observations above.

## Resumable handoff and scope

Work complete within released bounded deployment scope. Planner review owns documentation/harness review and any scoped commit; **no verifier commit/push**. Verifier tracked-file ownership remains only `experiments/runtime-spike/multinode-test.py` and this report. Private deployment tooling/artifacts are ignored. Do not expose identifiers, credentials, user catalogs, guest disks or snapshot contents in commits. No agents dispatched. Preserve all unrelated pending root files.

The harness is independently executable with `--prepare-only`, `--transport-only`, or `--vm-turn-released`; future guest runs still need coordinated host demand. Its timeout fencing requires actual EOF/Close/reset, missing live VM config blocks preflight, caps are asserted before launch. Existing regression scripts with >4 guests or >2048 MiB must not be run unchanged under this brief. Historical baseline at a7333db was 8 passing unit tests and 2 ignored VM suites; these were not the final acceptance.

Next operator action: use existing origin/login for real owner acceptance; monitor `python3 data/nl/manage.py status` and private logs. No additional VM suite or deployment action is pending from verifier. Planner may review these scoped files; rollout should remain bounded owner-only same-host.
