# Host onboarding verification — accepted and deployed

Current authoritative state: **V1–V6 passed for final2; reviewed project-only deployment completed and public verification passed.** Ubuntu and original workers preserved; zero test processes/guests. Physical second-host/NAT and snapshot restore remain for owner acceptance. Earlier sections are historical and do not override this state.

| Current gate | Verdict | Execution |
|---|---|---|
| V1 | Passed | Source-matched dynamic debug CLI; signed-owner/native TLS fixture |
| V2 | Passed | Final2 shipped static CLI; guided recovery/Ctrl-C no-VM |
| V3 | Passed | Final2 shipped static CLI; UNKNOWN/stale readiness/empty stop |
| V4 | Passed |24actual offline ingress script checks; complete GET capture and scoped dry-run |
| V5 | Passed | Final2 static CLI + exact host-public-final2 clean bundle |
| V6 | Passed | Final2 static CLI; actual managed TLS worker/agents/guest, PTY, persistence, reconnect/revoke; zero cleanup |

Core ready receipt: `data/host-core-build/ready-receipt-final2.json`.
Source: `c8506e396237bff37b0a546871203ad14a122f2419c55376d4406c48585b2068`.
Static: `c67635c050509feae3a5f2c11ede32dde394061cf6982acdf967f1c7d66bfe89`.
Dynamic V1: `aa70d5182855c96abcd233ffdfa01fae0f352bed573e2855af4c37dc39151962`.
Aggregate: `data/host-verify/minimal-acceptance-final2.json`, `minimal_pending:[]`.
Public default TLS, served CLI/five assets, node denial, protected human routes, actual managed outbound WSS/readiness/revoke and cleanup passed. Physical second-host/NAT and snapshot restore remain unverified.
Planner release commands: `data/host-verify/production-runbook.md`;
`activation-complete-dry-run.json` contains exact private owned operations.
Reviewed project-only live/cloud writes completed under planner release authority; no unrelated mutation. Current deployment receipt: `data/host-verify/release-20261003/deployment-final.json`. Historical sections below do not override this table.

## Historical preparation and acceptance record
Updated: 2026-10-03 | State: focused no-VM acceptance executed; release acceptance pending

Scope: independent focused harness/report and ignored `data/host-verify/` only.
Product lane: `../open-workspaces-lanes/host-core`. No product, task-board, service,
VM, cloud or existing Ubuntu checkpoint changes authorized in this turn.

## Early observations
- Read host verification/core/review briefs, onboarding plan, installed builder
  roles, README/HANDOFF and deployment helpers.
- Core interface and host review reports were absent at initial inspection; both
  were subsequently read, including HR-01 through HR-09.
  Existing native node commands are not evidence that new host commands work.
- Existing publishing helper owns a dedicated `/cli/*` Access application and
  uses exact artifact ingress before the authenticated hostname route. It refuses
  adoption of an unrelated application. Do not run this mutating helper during prep.
- Existing remote-access lifecycle helper manages gateway/tunnel using PID,
  owner, argv and process-start ticks. Acceptance must retain these processes.
- No physical second host is accessible. Synthetic local identities, loopback TLS
  and local guests cannot establish physical-host/NAT/public owner acceptance.

## Gates and next action
Prepare and run no-VM baseline checks now. After core emits its interface, fill
private fixtures with exact routes/commands and compare reviewer findings.
Planner alone grants the exclusive VM turn after coherent reviewed source freeze.
Only then run disjoint workers (each 512 MiB / 2 slots / 2 vCPU), team peak at most
3 guests / 768 MiB. Keep Ubuntu dev and its latest checkpoint untouched.
Live/cloud writes require reviewed release and bounded planner delegation.

## Required executable evidence
Clean-home installed binary host help/doctor/status; wizard/private stdin join;
owner/non-owner invitation mint/revoke; replay, expiry, wrong-node, forged budgets;
node/human credential separation; prefixed HTTPS and WebSocket TLS/redirect denial;
private credential/root permissions; KVM/Btrfs/namespace/tool/capacity diagnostics;
fixed artifact digest/size/corruption/path/symlink/extraction rejection;
start/stop/reconnect/revoke/fencing; real guest disk cold persistence; baseline
service continuity; cleanup absence; stable source and shipped artifact hashes.
Missing cases remain pending, never passed by omission.

## Same-domain rollout preparation (unexecuted)
1. Read private helper state and GET existing Access apps/policies. Match exact
   saved dashboard/CLI IDs, hostname, policies and ingress. Abort on mismatch,
   duplicate routes, missing pagination or an unowned node-path app.
2. Private mode-0700 backup directory: gateway/tunnel/Access state and policy
   exports, current native/public binaries plus hashes, controller metadata and
   catalog using SQLite online backup (copying a live WAL database is insufficient).
   Preserve worker/Ubuntu disks and snapshots; do not touch unrelated services.
3. Reviewer pins final node methods/paths and artifact allowlist. Stage exact
   node Access namespace and exact tunnel regex before the human hostname route,
   native controller `https://127.0.0.1:8790` only, with an explicit trusted
   origin CA and verified matching TLS server name. Never set `noTLSVerify`.
   Gateway human administration/artifacts remain at loopback port 8787. Never use a catch-all node proxy. Keep dashboard
   human Access application/AUD, `/cli/*` policy and unrelated ingress unchanged.
4. After reviewed release only: planner runs owned, idempotent app/route helper,
   publishes hash-verified new CLI/assets, and replaces only project gateway/tunnel
   as required. Acceptance itself must not stop baseline services. Reload is a
   separate release action. No global SSL, DNS-zone or shared listener changes.
5. Verify anonymous `/`, `/api/workspaces`, `/api/nodes`, human PTY remain
   challenged/denied; node endpoints without/with invalid credentials reject at
   origin without human login redirect; valid node HTTPS/WSS works. Test prefix
   lookalikes, encoded traversal, extra slash, unknown method/path, browser token
   at node route and node token at human route. Exact routes await core contract.
6. Rollback: fence/revoke newly enrolled nodes first; restore backed-up project
   binaries/config/routes and only helper-owned newly created Access app/policy.
   Catalog downgrade requiring owner worker interruption is outside this release.
   Restore metadata/catalog only with relevant writers safely quiesced in a separately
   authorized maintenance step; never overwrite a live DB or discard new disks.
   Recheck human gate and baseline service health. Preserve failed rollout evidence.

## Owner commands
Contract-dependent, not yet distributable: install the reviewed `/cli/install.sh`
from the existing project HTTPS origin, verify the installed Linux artifact hash
matches the release manifest, then run `ow host doctor`, `ow host join`, and
`ow host status`. Default join must accept hidden invitation input carrying origin,
policy/expiry/bounds; it must not require a human Access login. Owner obtains the
private invitation via verified human controls. No secrets in shell arguments,
URLs or logs. Advanced file/flags syntax awaits corrected interface and CPU cap.
`ow host stop` affects only this invited host's root/guests, never the owner's root.
Physical host diagnostics and NAT/guest results remain pending owner execution.

## Executed preparation evidence

- `python3 experiments/runtime-spike/host-onboarding-test.py inspect`: passed
  preparation scan. One running VMM, 2048 MiB/2 vCPU; five baseline service
  descriptors alive with matching UID/start ticks (full argv also checked where
  recorded); a/b workers empty, each512MiB/2slots/2vCPU. Available RAM at first
  scan24462MiB. No pending/uncertain catalog operations and no extra usage on
  local/a/b. This is a point-in-time inventory, not an exclusive reservation.
- Harness demands 4096MiB physical reserve plus planned768MiB, at most4 guests /
  3072MiB including baseline. It blocks unreadable/missing live VMM configs,
  unknown worker demand and unresolved catalog reservations. Planner must rescan
  immediately before its exclusive VM turn; this harness cannot grant that turn.
- Private source manifest records Cargo/crates/scripts hashes while core is
  evolving. It is not a reviewed release pin. Protect owner machines/checkpoint
  metadata without reading/copying guest contents or asserting live disk hashes.
- Cloudflare GETs for the saved dashboard/CLI application IDs and policies
  succeeded. Returned domains match saved hostname/CLI path. Dashboard policy
  is allow; CLI policy is bypass; current private ingress has3 rules. No app-list
  collision/pagination audit or node application clearance yet. Private exports:
  `data/host-verify/cloudflare-readonly.json`. No external mutations.
- `python3 experiments/runtime-spike/host-onboarding-test.py cli --binary
  target/release/ow`: blocked as expected: installed old CLI lacks host command.
  A new gateway source build cannot satisfy installed public CLI acceptance.
- Final `compare` passed: baseline VMM/service identities, ingress/config hashes,
  protected disk/checkpoint inode/size/mode matched. The expected owner guest
  remains running; no extra test VMMs. Metadata matching is not a content hash
  or proof that an active owner disk has no legitimate writes.
- Python syntax compilation and harness help passed. No HTTP, browser, runtime
  artifact, guest, public node or physical-host acceptance has been executed.

## Executable harness contract

`experiments/runtime-spike/host-onboarding-test.py` is executable and stdlib-only.
Output lives in mode0700 `data/host-verify`, result files0600. Modes:

```bash
python3 experiments/runtime-spike/host-onboarding-test.py inspect
python3 experiments/runtime-spike/host-onboarding-test.py compare
python3 experiments/runtime-spike/host-onboarding-test.py cli --binary /ABS/INSTALLED/ow
python3 experiments/runtime-spike/host-onboarding-test.py http --fixture data/host-verify/local-https.json
```

`cli` executes help and doctor in a disposable clean private HOME with cleared
OW/loader environment, checks required help commands and hashes the binary
before/after. Doctor exit/content is an observation, not diagnostic clearance.
`http` requires private file/parent under the owned root, literal loopback HTTPS,
fixture server PID/start ticks/operator identity and an argv path under the
fixture root; it refuses live/default8787/8790/443 ports and saved managed PIDs.
Fixture schema: `origin`, `ca_file`, `server_pid`, `server_ticks`, `source_manifest`
(the exact frozen Cargo/crates/scripts hash mapping), `cases`. Cases have required
acceptance `id`, `path`, optional `method`/`headers`/`json`/`websocket`, mandatory
`expect_status`, optional top-level `expect_json` equality checks. Secret headers
and JSON remain in the private fixture. Requests are bounded, verify CA/hostname,
do not follow redirects, verify101 WebSocket accept, and recheck source/host VMM
identity after all cases. Denial cases cannot accept2xx/redirect/5xx. HTTP/upgrade
success does not prove agent dispatch, fencing, PTY frames or guest persistence.
Missing cases explicitly remain pending. Lifecycle/VM/browser/installer/artifact
adapters must be filled after the corrected interface and stable reviewed binary;
no-vm adapters only launch/stop scratch HTTPS/JWKS/gateway/controller processes;
no worker, agent or VMM is launched, and no existing service is touched.

## Planner corrections and review mapping

Planner selected **one preserved native TLS controller on127.0.0.1:8790**, adding
`/_nodes` aliases there; a/b native origin/credentials and a single session map
remain intact. The interface was later corrected to the single native TLS controller and exact
origin CA/server-name routing (HR-07); public rollout execution remains pending.
Exact paths: POST `/_nodes/enroll`; GET upgrade `/_nodes/node/{node}` and
`/_nodes/job/{node}/{id}`. Tunnel path regex must constrain both identifiers by
the controller's final validated grammar; reject wrong methods at origin.
Access application is dedicated `HOST/_nodes/*`, owned and recorded independently;
the hostname app and CLI app remain untouched. Alias origin TLS CA/hostname
validation is mandatory. Exact asset allowlist must be updated for the final
bundle; the corrected Alpine-only bundle has five direct files and no network archive
consumer (HR-03); public publication and real runtime behavior remain pending.

HR-01: owner pair/missing/partial configuration, wrong subject/ID/issuer/email,
node token and absent JWT denial before DB writes. HR-02: prefixed and native
HTTPS/WSS plus query/path/method/redirect/TLS denial. HR-03: actual public CLI,
clean-home download integrity/atomicity and hostile archive rejection. HR-04:
budgets/env/process identity and conservative lifecycle. HR-05: re-mint limits,
parallel one-use, expiry, commit/response-loss recovery. HR-06: collisions, exact
pre-change backups, repeat apply/partial failure and narrow rollback preserving
later unrelated changes. HR-07: concurrent native a/b plus prefixed node, one
controller map, cross-route fencing/revoke and preserved owner VMM. HR-08: owner
CPU ceiling and forged/tampered local/heartbeat CPU denial independent of RAM,
surviving all placement/resize/start/restore/fork/restart paths. HR-09: hidden
join PTY echo restoration/cancel/EOF, invitation-only clean home, shared-pool
consent and local routing despite saved remote state.

## Stop/rollback boundary and observed source race

Never run `data/nl/manage.py stop` or `rollback`: source inspection confirms
`stop()` calls `down` for legacy/a/b and rollback calls stop. This kills the
owner Ubuntu VM. No blanket manager operation is part of this acceptance or
rollout. Its kill loop calls full `managed()` argv validation after SIGTERM;
empty/transitional cmdline during exit can raise `Owned process arguments
mismatch` after a successful signal, matching planner's observed race. No stop
was executed here. A future scoped release helper must fully validate UID,
start ticks and expected argv **before** signaling, then poll stable UID/start
identity and process state for exit, never repeatedly signal or adopt changed
argv/PID. If identity changes, do not signal the replacement. Zombies/disappeared
owned processes count as exited; unexpected live state blocks duplicate start.
Restart only descriptor-verified gateway/controller/agents and empty test-pool
workers when planner releases them; preserve legacy worker/VMM and artifacts.

Owner reports real browser login/create/exec/cold persistence/forks/PTY on the
current deployed site passed. This is owner-reported baseline evidence, not a
host-onboarding verifier run. Snapshot restore remains ambiguous and is pending;
do not touch the owner checkpoint to resolve it during this lane.

## Handoff
Updated2026-10-03 | State: preparation complete; acceptance gated.
Owned files: this report and focused harness only; private evidence under
`data/host-verify/`. No commit/task-board/product/service/cloud writes.
Next: planner obtains corrected interface/reviewed frozen source and shipped CLI;
then verifier fills bounded synthetic HTTPS fixtures and remaining adapters.
VM stage requires planner exclusive turn, immediate inventory/headroom check,
and explicit test-owned roots. Cleanup means no test guests/reservations/processes,
with baseline Ubuntu VMM identity still present; owner guest is not a failure.
Do not use prior broad multi-node harness budget or whole manager stop/rollback.
Open user decision: none. Planner owns coordination and reviewed release.

## VMM name correction receipt

Planner reported Linux-truncated VMM comm. Re-ran the focused read-only detector:
it uses `comm.startswith("firecracker")`, parses machine configuration and
compares PID start ticks before/after the read. It detected the planner-identified
owner VMM at2048MiB/2vCPU, one guest total. Exact comm equality is never used.
The live PID is planner-supplied private evidence; no process was signaled and
no guest artifact was changed. Existing baseline receipt remains private.

## Concrete adapter update and build state

Planner authorized coherent no-VM compilation after reported core unit suite:
22passed/3ignored (planner-reported, not independently rerun). Unit tests do not
establish a normal CLI binary exists. Source capture and a normal locked/offline
1.97.0 debug CLI build now use ignored `data/host-verify/build-20261003/` with two
build jobs. Byte hashes of copied Cargo/crates/scripts files match the captured
lane before compilation; later lane edits are a different build, never silently
accepted as the same evidence. Build output and result remain pending below.

The focused harness now has a `no-vm` mode with concrete execution adapters:
real Linux curl installer/TLS/hash checks and corrupt-update preservation in a
clean HOME (inert preinstalled helper avoids external helper downloads); installed
HostAction help/doctor; saved-human-login and explicit-server routing; invalid
policy/expiry/controller/bounds/private invitation rejection before network;
real hidden PTY cancellation and termios restoration; malformed fixed asset
size/hash/name/unknown-destination/file/parent-symlink and permission rejection;
worker mismatch refusal via bounded fake status socket; actual local HTTPS
manifest/blob failures and `join --no-start` private persistence; actual scratch
Rust gateway and native TLS controller owner/nonowner/forged-email/wrong-owner-ID,
reissue/replay/expiry/wrong-node/parallel-redemption, cross human/node credential,
query/path/method and revoked-session denial. HR-10 output validation before any
POST and HR-11 revoked-unused-ID remint are explicit expected-failure cases.
No test creates a real worker/agent/VMM. Positive download assets are inert;
synthetic TLS enrollment does not prove controller authority. Native auth reuses
only prior certificate/JWKS transport helpers, with launch allowlist restricted
to scratch dashboard/controller; the old harness run/VM/cleanup suite is not used.
No live gateway/controller/worker/agent is stopped or restarted.

Mode inputs: compiled `--binary`, `--lane` pointing to its pinned snapshot, and
private `--ready-receipt` JSON (`unit_build_ready`, `source_manifest`,
`binary_sha256`). The receipt records planner's readiness and actual snapshot /
binary pairing, not a release authorization. Source/binary hashes and owner VMM
identity are checked before/after. Missing or changed pairing blocks execution.
Granular details remain private; failures are actionable current-build findings,
not final review clearance. Successful connection/upgrade does not prove guest
recovery, in-flight revoke/fencing or physical-host/NAT.

Owner-reported actual second-host preflight: Omarchy4.0.4x86_64, KVMrw, HOME
Btrfs,46GiB total/41GiB available RAM,818GiB free disk. This is owner-provided
physical host evidence; verifier has no remote access. Ignored command sheet:
`data/host-verify/second-host-commands.sh` uses the actual saved project origin
privately and a release-digest placeholder that fails closed until planner fills
it. User-space portable CLI install; `host doctor`; bare `host join` with hidden
invitation paste/explicit consent/guided bounds; `host status`. No Tailscale or
human participant login. Owner then creates one disposable256MiB/1vCPU Alpine
on that explicit node, checks execution/PTY and disk cold persistence, and tests
participant stop/start/revoke while preserving Ubuntu. Commands are prepared,
not executed; local fake HOME/loopback cases are never physical-host/NAT results.

## Executed pinned no-VM acceptance — latest result

Normal debug CLI built successfully in42.25s (build bookkeeping, not a latency
benchmark), locked/offline Rust1.97.0, two jobs, from the independent immutable
Cargo/crates/scripts snapshot. Binary SHA256: `c92bf44dbcd96c9da543de09fc98975febedcbffa523dde96b3d694805b8b874`.
No normal binary was assumed from the core's unit pass.

```bash
python3 experiments/runtime-spike/host-onboarding-test.py no-vm --binary data/host-verify/build-20261003/target/debug/ow --lane data/host-verify/build-20261003/source --ready-receipt data/host-verify/build-ready.json
python3 experiments/runtime-spike/host-onboarding-test.py no-vm --binary data/host-verify/build-20261003/target/debug/ow --lane data/host-verify/build-20261003/source --ready-receipt data/host-verify/build-ready.json --cases actual-https-download-and-no-start-join actual-gateway-and-native-controller-auth
```

**12focused groups passed** across the two runs. First run10groups passed; two
stopped on verifier fixture defects (self-signed CA used as endpoint certificate,
which rustls correctly rejected; Python dict shadowed concurrent module). Fixed
only the harness, using separate CA/signed leaf and renamed local variable;
reran only the two affected groups, both passed. No product correction was made
by verifier. Complete private evidence:
`data/host-verify/no-vm-first-result.json`, `no-vm-result.json`, combined
`no-vm-combined-result.json`, `build-ready.json`, `build-source.json`, and referenced
scratch roots/logs. The first failed run is retained, not erased.

Observed: actual Linux installer/curl verified the compiled artifact and refused
a corrupt replacement while retaining prior install. Installed host action help;
structured doctor bypassing saved/ambient human server; explicit-server denial;
non-TTY alternative; hidden PTY Ctrl-C cancellation restored termios and did not
echo marker/save enrollment; nine invalid invitation envelopes plus public-file
rejection before network; ten malformed local asset start denials; start/stop
refused mismatched synthetic worker with only status calls; nine actual local
HTTPS bad-download modes rejected before redemption and cleaned staging. Valid
local TLS downloads plus synthetic `join --no-start` saved0600/0700 state, status
ran, and no worker/agent started. One sample per negative variant; no benchmark.

Actual scratch Rust gateway/controller evidence with synthetic signed humans:
unconfigured owner, Bob, forged email and wrong bound owner ID denied before
node DB mutation; valid owner invite included policy/CPU cap; prefixed native TLS
enrollment passed; wrong node/replay/expired DB row denied; reissue invalidated
older secret; simultaneous redemptions returned exactly200+401. Browser JWT could
not authorize a node upgrade; valid node bearer could not authorize human invite.
Query/encoded/double-slash/suffix/wrong-method aliases rejected; owner revoke
blocked subsequent node session; revoked-unused-ID reissue failed without DB
mutation (HR-11). Client public/missing output parent/non-TTY rejected before any
synthetic HTTPS POST (HR-10). These observations apply to the captured build,
not automatically to later core edits.

The private combined report explicitly lists source differences/new files against
the evolving lane. Final clearance still requires stable reviewed commit blob
comparison, rebuilt release/public installer artifact hashes and source-matched
reruns where changes affect accepted paths. Debug/local installer is not the
actual public shipped release. Initial helper acquisition/macOS, unsupported-host
fault matrix, invitation delivery/commit/response-loss recovery, full guided
success, forged heartbeat CPU admission, successful agent/job streams and
in-flight fencing/recovery remain pending. Expiry server case used an explicitly
expired private fixture DB row; it is not a wall-clock boundary test.

Physical second-host/NAT/public Access/tunnel route/CA precedence, real guests,
cold persistence/snapshots/forks/PTYS on the new host and cleanup of real test
reservations remain pending planner/owner stages. Owner-reported existing-site
browser acceptance remains separate. NoVM baseline comparison confirms protected
owner VMM/process identities and demand before/after, with scratch children
stopped/reaped; final standalone comparison is recorded below. Never count the
running owner guest as a leftover test or shut down its worker.

## Current resume instruction

Read the latest corrected core interface/review and combined pinned no-VM result.
Compare changed lane paths to the tested snapshot; rerun affected groups only
against a newly pinned coherent build as needed. Do not repeat passed unchanged
research or treat evolving source as the same binary. Planner grants the exclusive
VM turn and reviewed live/cloud release separately. Second-host command sheet
still needs final release digest, then owner can install user-space portable CLI
and use hidden bare join. Verifier has no second-host access and does not claim
its actual joining/NAT/guest results. No outstanding user decision in this lane.

Final standalone `compare` passed after both runs: original owner VMM/demand,
baseline service identities, ingress/config hashes and protected artifact metadata
matched; no additional guest VMMs. Core currently differs in node_tests.rs,
nodes.rs, onboarding.rs, remote.rs and adds prepare-host-ingress.py. Those
changes require new coherent source/binary pairing and affected-path verification.

## Current bounded V1–V6 scope and prepared adapters

Reviewer top-of-file V1–V6 is authoritative; broad historical fault campaigns are
not new release requirements. Previous12group successes remain evidence of their
old source snapshot only. Current adapters are prepared for the final corrected
source/installed static CLI and will run only affected V1–V5 groups when coherent
core readiness/freeze permits. Core owns the exclusive guest turn until its
cleanup; verifier has launched **no** guest tests or real worker/agent here.

- V1: actual scratch Rust owner/CPU adapter now sends forged heartbeat16, checks
  persisted/effective CPU2 after reopening DB, and requests CPU3 while RAM512 /
  slots2 remain free, checking rejection without a worker job. Existing owner pair
  denial cases remain. This added CPU path is prepared, not executed yet.
- V2: installed guided PTY consent/capacity with `--no-start` and explicit advanced
  `--ca-cert`; actual hidden cancellation now sends Ctrl-C **without Enter** and
  checks queued secret suffix absence. Deterministic fixture failure after credential
  persistence/before marker, repeated local join without HOME/redemption. Prepared,
  not yet executed against corrected tree. Old newline cancellation evidence is
  narrower and does not clear this current case.
- V3: current explicit-root mock held-worker-lock/missing RPC UNKNOWN, cached
  acknowledgement without live agent offline and confirmed stopped after lock
  release. Real instance exit/reconnect adds coverage in gated V6. These updated
  adapters remain unexecuted; prior mismatch refusal is separate old-build evidence.
- V4: **24 actual offline script cases passed**, script SHA256
  `30ea23009f6c7b7e6f86a3ded02e89a8e607eb7f3826e1a5454090f6a831d58c`.
  Approved state/normalized application semantics and generated private CA/signed
  leaf satisfy the script's real OpenSSL hostname/chain checks. Cases cover changed
  human allowlist/IDs, inherited TLS bypass, conflicting/partial app/route state,
  ordered exact native node/artifact routing, repeat prepare, app-created/route-failed
  compensation, rollback preserving unrelated later app/route, and repeat rollback
  with no destructive effect. `data/host-verify/ingress-result.json` and its private
  referenced fixture root retain exact source/input/output evidence. Earlier
  evolving-script drift failures were observed before correction; they are not
  current blockers. A later script change requires focused recheck, not automatic
  transfer of this result to the final tree. No Cloudflare/tunnel mutation occurred.
- V5: `artifact` adapter runs actual `prepare-host-bundle.py` against a fresh
  prepared source; checks clean Alpine recipe/image hash, accepted static ELF with
  no PT_INTERP, source record/build/CLI/guest hashes, five-file manifest, staged /
  copied installed CLI digest/help/doctor, failed corrupt image and symlink source
  publication. It compares gateway-selected candidate CLI read-only when supplied.
  It never copies into live publication paths. Static build and clean bundle remain
  pending core's coherent frozen artifact readiness. No old debug digest is a
  final shipped-artifact receipt.
- V6: `guest` adapter requires private planner turn receipt with
  `planner_exclusive_turn:true`, `core_cleanup_complete:true`, expiry and exact
  static binary digest. A dedicated scratch controller fixture must have owned
  marker plus PID/start/executable matching the frozen static CLI. It runs managed
  join with explicit `--ca-cert`, checks512MiB/2slot/2vCPU, one256MiB/1vCPU Alpine
  exec/PTY/cold persistence, managed stop/start, stale-online denial after agent
  exit, advanced native `node-agent --ca-cert` reconnect to the **same** controller,
  generation replacement peer closure and active revoke peer closure. Cleanup stops
  only the new managed root, checks no extra VMM, root argv/cwd processes or socket,
  and preserves owner baseline identities. Existing live a/b descriptors/endpoints
  remain read-only; frozen V1 handles human auth, and actual public ingress/physical
  host results stay separate. Prepared only; missing-release gate was executed and
  refused before any fixture launch.

Explicit trust distinction: parent curl/synthetic JWKS fixtures have local test
trust; final managed join uses the real advanced CA contract, which validates /
copies CA privately for download, redemption and detached agent. Production
agent environment stays sanitized, including no inherited SSL_CERT_FILE. Default
owner second-host join uses public system trust. No TLS verification bypass,
production environment relaxation or fake second-host/NAT claim is allowed.

Final pairing/run inputs:
`no-vm --cases V2-guided-recovery V3-state-unknown-stop hidden-pty-cancellation
actual-gateway-and-native-controller-auth` with final compiled binary/frozen lane /
ready receipt; `ingress --lane FROZEN_TREE`; `artifact --fixture PRIVATE_ARTIFACT_JSON
--ready-receipt PRIVATE_READY_JSON --lane FROZEN_TREE`; gated `guest --binary
STATIC_CLI --lane FROZEN_TREE --ready-receipt PRIVATE_READY_JSON --fixture
PRIVATE_GUEST_JSON --vm-turn PRIVATE_PLANNER_RELEASE_JSON`.
Artifact fixture fields: prepared_source, linux_cli, optional gateway_selected_cli.
Guest fixture fields: ca_cert, invite_file (real scratch-controller authority),
controller_root (new ignored root with `.host-verify-owned` text),
controller_process (pid/ticks), native_controller (exact loopback HTTPS origin).
The scratch HTTPS invitation origin must actually serve selected clean bundle and
forward exact node routes to that one controller; no synthetic credential responder
is used for V6. Owner production origin never goes into tracked fixtures.

Next action: await core coherent frozen source + static CLI/clean bundle, verify
hash pairing and execute only affected V1–V5 paths. Planner releases V6 only after
core cleanup and immediate inventory/headroom scan. No user approval is requested;
this is the already named planner/core stop gate.

## Staged static pair inspection — final execution pairing pending

Core reports28serial unit passes/3ignored and owns exclusive one256MiB real guest
smoke. Verifier did not enter that turn. Actual paths were discovered from the
self-describing private build reports, never inferred from unit pass:
`data/host-core-build/release-v1/build.json` and portable
`release-v1/ow-linux-amd64`; published-bundle candidate
`data/host-core-build/host-public-v1/build.json` plus `manifest.json`;
clean asset source marker `data/host-core-build/assets/guest/clean-alpine.json`.
The final interface/report had not yet listed these paths at inspection.

Both build reports and copied bundle CLI agree on staged CLI SHA256
`18760489520e1a30e629c29d25f4955223e01738f30191b4c235e5a394ed77fb`;
guest SHA256`1e5d3b4bc924755215a56531e36b26ffb3f0632ad270907d73f3aa9605642cf7`.
Recorded32-file source fingerprint:
`0ac5f8d4b7bb629dd0d9537245ae63077d3c4892a91b7d6c4b5980019d952b52`.
All five bundle files match manifest size and SHA256. Clean Alpine marker recipe
is minimal3.24.2-v1; disk digest
`8d71e92f3333fff04a8261717f115878a4b47b64ec0e75c7de1fdb233cd5bbd1`.
Private exact metadata/byte checks: `data/host-verify/staged-static-artifact.json`.
This is staged-byte evidence, not final V5 or public deployed-artifact clearance.

At latest comparison the current lane differs from the build source in
crates/ow/src/host.rs, crates/ow/src/main.rs, crates/ow/src/node_tests.rs, crates/ow/src/onboarding.rs.
A matching source snapshot/refreshed static pair is required before final V1–V5
execution. An asynchronous request asked planner for that core pairing; no new
permission was requested. Never assign current source fixes to an older compiled
CLI. No-VM fixtures permit concurrently resolved core test guests while protecting
owner VMM identity; V6 still requires core cleanup, fresh exact baseline and
planner release. This avoids mislabeling another lane's legitimate guest as
verifier cleanup failure. Missing/unresolved configured demand still blocks.

Production command sheet remains private and held until reviewed release. It
uses fixed portable CLI HTTPS download+released hash, user-space installation,
`host doctor`, hidden bare `host join`, explicit shared-pool consent/budgets, and
status. Public system trust is default; explicit custom CA is an advanced local
fixture/operator option. Planned release must publish the exact accepted new CLI
and clean bundle, update only helper-owned node app/ordered native TLS route and
preserve human Access + baseline worker/Ubuntu. No live/cloud writes were made.

Next: receive matching frozen source/static bundle report; pin it and run affected
V1–V3/V5 adapters plus exact-script V4 recheck if changed. Preserve earlier24offline
and12old-build group receipts as separately scoped evidence. Core cleanup/planner
release remains mandatory before V6 launch. No owner physical joining/NAT result
has been fabricated.


## Refreshed static execution — subsequently superseded by core stop correction

Planner admitted the then-current release-final tree independently, followed by
core ready-receipt.json and host-public-final. Verifier checked all32 recorded
source files and recomputed source fingerprint
`7f25a7f9f9df783943de192d3947d672e2d3d0e0df1b9ff014db98ee7c9d2aa3`;
static CLI hash
`d4e2738762ffa1ce2fe7cc2ee329ac9f75c365027713b50cda039056f7ca4a84`.
**Core has now invalidated this receipt while correcting scoped supervisor
handling in host stop. All results below apply only to that superseded pair.**
No final release gate is claimed for the evolving replacement.

Affected actual-static no-VM checks: V2 guided hidden PTY consent/budgets with
--no-start passed; injected credential/marker interruption recovered locally
without HOME, network requests or repeat invitation redemption. Hidden Ctrl-C
without Enter passed with restored termios, no queued secret and no credentials.
V3 held worker lock/missing socket correctly returned UNKNOWN; cached ready ack
with no live matching agent was offline. Empty-root final stop refused with
`same-operator process inventory unreadable; guest exit UNKNOWN`; this is a
fail-closed observation, not successful cleanup. Core is resolving unreadable
same-UID inventory and scoped supervisor checks. Private first result:
`data/host-verify/no-vm-final-first-result.json`.

V1 signed-owner adapter could not start its fixture gateway with the static
musl binary because its existing LD_PRELOAD DNS mapping requires dynamic linking.
The gateway deliberately disables proxy routing; an attempted local CONNECT
fixture could not route it and was removed. TLS remained verified; no insecure
option or production trust relaxation was added. Final adapter detects PT_INTERP
before attempting the signed-owner fixture. Planner permits a separately hashed
dynamic binary built from the SAME final frozen source for V1; static V2/V3/V5/V6
must remain tied to the shipped binary. This fixture limitation is not evidence
of a product authentication failure. Current private no-vm-result includes the
failed fixture attempt; it does not replace earlier V2 success.

V5 actual static/clean bundle verification passed after updating the adapter to
the actual publisher's mandatory marked dedicated build-root contract. All
publisher mutations were in new verifier-private storage with reflink copies,
never core assets or live roots. Exact actual publisher executed; ELF64 x86-64
has no dynamic interpreter; copied installed CLI, source/build record, guest
hash, clean Alpine marker, five-file allowlist and every digest/size matched.
Independently published manifest and all five files matched the actual staged
host-public-final, including copied CLI. Corrupted disk and symlink source were
rejected without publishing. Installed help/doctor executed. Result:
`data/host-verify/artifact-result.json`. Public gateway selection/deployed download
remain pending release; passing staged verification does not prove publication.
This receipt is now superseded along with the static pair.

Concrete private production runbook is `data/host-verify/production-runbook.md`:
complete paginated application/policy capture, approved identity/trust state,
fresh --check-plan gate, immutable bundle selection, online SQLite backup,
exact gateway/controller/a-b restart descriptors, same catalog and native TLS,
owned app creation receipt before ordered route write, narrow compensation and
rollback preserving unrelated later changes. It never invokes wholesale
manager stop/rollback or legacy worker down. Prepared executable
`data/host-verify/scoped-process-stop.py` verifies UID/start/executable/argv before
signal, pins lifetime with pidfd, then polls pidfd without post-signal argv reads.
Default is read-only; it has not signaled any process. The signal branch is held
for a reviewed release receipt and exact identity digest. Both syntax checks
passed. Existing project-only release authorization is retained; no owner
artifact refresh decision is requested.

V6 remains unlaunched: core positive cleanup and planner immediate admission
release are both required. Core's first managed fixture attempt is owner-reported
as failing CA-as-leaf validation before worker/guest, then positively cleaned;
verifier has not independently observed a managed guest run. Baseline Ubuntu
VMM/process/ingress/artifact continuity passed before and after verifier no-VM
execution. Await refreshed source/static/bundle receipt; run only affected
checks. Earlier12groups and24offline checks remain scoped historical evidence.


## Final focused acceptance — final2, stable core commit6a6e3d0

All six bounded reviewer gates passed. This aggregate supersedes old pair mismatch
and fixture-preparation failures; it does not promote historical broad pending
cases to acceptance. Core receipt `data/host-core-build/ready-receipt-final2.json`
reports28unit passes/3ignored. Verifier recomputed the paired32-file source record
and static/guest build hashes; source unchanged during final executions.

| Gate | Focused observed evidence | Exact execution attribution |
|---|---|---|
| V1 | Verified owner subject/catalog ID; Bob, spoofed email, missing owner and wrong bound ID denied; RAM512/slots2 free, heartbeatCPU16 clamped2; CPU3 rejected; dispatched jobs inspected and only empty read-only reconciliation allowed; no accepted resource/reservation | Separately hashed dynamic debug CLI built from SAME frozen final2 source; actual scratch gateway/native TLS, synthetic signed users/inventory; no real worker or guest |
| V2 | Actual hidden guided PTY consent/bounds --no-start; marker fault after credential persistence recovered locally without HOME/network/redemption; Ctrl-C without Enter restores terminal and leaves no queued secret | Final2 shipped static CLI, disposable local TLS/assets/enrollment fixtures; full managed start covered V6 |
| V3 | Held worker lock/missing socket UNKNOWN; stale ready acknowledgement offline; empty owned root stop confirmed | Final2 shipped static CLI; V6 also verifies real managed agent exit/readiness fencing |
| V4 |24actual offline ingress checks: approved drift, exact native TLS, reapply, compensation, narrow/idempotent rollback | Actual source-pinned prepare-host-ingress.py; no cloud writes |
| V5 | Actual immutable publisher in marked verifier storage; static ELF64 x86-64; source/build/guest/clean Alpine marker/five hashes and sizes; shipped bundle manifest/files/copied installed CLI matched; corrupt disk/symlink rejected | Final2 shipped static CLI and host-public-final2 clean bundle; installed help/doctor; public gateway-selected download still awaits deployment |
| V6 | Actual real managed CLI with explicit CA, prefixed native-controller channel,1x256MiB/1vCPU guest, real exec, cold disk persistence, managed stop/start, PTY, native advanced reconnect, generation replacement closes prior peer, revoke closes active peer; positive exact cleanup | Final2 shipped static CLI, one actual native TLS scratch controller plus dedicated local asset/protocol edge, real clean bundle/worker/agents/guest; no mock enrollment |

Frozen source SHA256: `c8506e396237bff37b0a546871203ad14a122f2419c55376d4406c48585b2068`.
Shipped static CLI SHA256: `c67635c050509feae3a5f2c11ede32dde394061cf6982acdf967f1c7d66bfe89`.
Separate source-matched dynamic CLI SHA256: `aa70d5182855c96abcd233ffdfa01fae0f352bed573e2855af4c37dc39151962`.
Guest artifact SHA256: `1e5d3b4bc924755215a56531e36b26ffb3f0632ad270907d73f3aa9605642cf7`.
Clean bundle manifest SHA256: `506838560beb2a199f6c80eafe8c06bedb943b35f24d13672d6a3619ddc49239`.
Exact build/CLI/bundle paths: `data/host-core-build/release-final2/build.json`,
`release-final2/ow-linux-amd64`, `data/host-core-build/host-public-final2`.

Private aggregate: `data/host-verify/minimal-acceptance-final2.json` contains
per-gate receipt paths/digests, binary/source hashes, `minimal_pending:[]` and
separate broader deferred scope. Static receipt:
`no-vm-final2-static-result.json`; dynamic auth:
`no-vm-final2-dynamic-auth-result.json`; offline ingress:`ingress-result.json`;
artifacts:`artifact-result.json`; managed guest:`guest-result.json`; scratch edge
and controller teardown:`v6-edge-cleanup.json`. Individual minimal-pending fields
now mark their completed named gate; they are not aggregate release verdicts.
Successful V6 `vm_run` metadata was corrected to true after execution; initial
result template had incorrectly retained false despite recorded actual create.
Final output attributions were added from the validated exact execution inputs,
without rerunning or changing observed outcomes.

Fixture corrections were bounded: static DNS preload requires a separate dynamic
V1 build; asynchronous native proxy socket must exist before heartbeat; catalog
reconciliation legitimately dispatches read-only list/snapshot inventory before
CPU selection, so adapter inspected actual job payloads and prohibited effects;
prefixed invitation base must contain /_nodes. The V6 native persistence output
included a known OW> console prompt after PTY; fixture retains exact output and
requires exit0 and exact nonce after removing only that prefix. No TLS bypass,
production trust environment relaxation, product edits or guessed gate success.
All unsuccessful managed attempts positively cleaned before retries.

Planner's exclusive receipt admitted V6 only after core smoke-final4 positive
cleanup. Verifier immediately repeated owner/service/ingress/artifact identity
comparison and authoritative three-worker status before each fixture start.
Peak actual verifier guest demand was one256MiB/1vCPU guest and one512MiB/2slot/
2vCPU worker. Owner Ubuntu retained the same recorded private PID/start identity,2048MiB/2vCPU;
exact baseline is retained in `data/host-verify/inspect-result.json` and
`data/host-verify/v6-edge-cleanup.json`.
Final result confirms zero test processes/guests, no owned control socket and
owner baseline preserved; scratch controller exited and local edge closed.
Disks/results remain privately retained for review; no owner artifact deletion.
Unreadable processes were independently resolved by exact trusted systemctl/
logind identity, cgroup, launch bytes and unchanged process lifetime; unresolved
processes remain blocking, never comm-only skipped.

Concrete production preparation is now executable and privately reviewed:
`release-prepare.py` implements only stored-baseline construction, complete
paginated GET capture/normalize and dry-run; `approved-baseline.json`,
`captured-complete{,.raw}.json`, `activation-complete-dry-run.json`,
`compensation-dry-run.json`, `rollback-dry-run.json` retain exact owned deltas.
The previous stored created-tunnel/process files are0644 within private parent;
read-only preparation validates owner/private directory and reads recorded IDs/
descriptors, never changes permissions or copies saved tunnel token.
Policy uid is normalized only when exactly equal to id; ambiguous/unknown fields
fail closed. Activation includes existing bootstrap owner subject/catalog pair,
exact new same-root gateway/controller/a-b argv/env and dedicated tunnel identity.
Rollback dry-runs preserve an unrelated later route. pidfd helper now binds the
project tunnel to actual saved descriptor, config, tunnel ID and exact dedicated
cloudflared argv; read-only identity execution passed without any signal.
Planner commands and reviewed execution order are in
`data/host-verify/production-runbook.md`. No wholesale manager stop/rollback,
worker restart, live public asset replacement, cloud write or service mutation
was executed. Reviewer final stable clearance is required before planner release.

Local synthetic identities/asset edge/custom CA evidence is distinct from real
managed CLI/custom CA/default public system trust. Actual public Access/tunnel,
public artifact selection and owner physical second-host/NAT remain deferred
until scoped release/owner execution. No physical/NAT result is claimed; owner's
Omarchy preflight and old deployed browser checks remain owner-reported.
Snapshot restore remains unverified. The portable CLI+hidden guided join owner
command sheet is held privately until reviewed public digest readback.


## Completed reviewed project-only deployment — authoritative current result

Planner released the scope with private authority
`data/planner-host-onboarding/reviewed-release.json`, following stable review
clearance of core commit6a6e3d0 merged by planner as326204e. All32 current compiled
inputs, accepted binary and pre-release receipt/review hashes matched authority.
No product source edit, push, unrelated application change or global SSL change
was made by this lane.

Fresh complete paginated apps/detail/policy capture and actual --check-plan passed
at preparation, activation and immediately before the cloud write. Private online
SQLite backup, configs/bins/descriptors/old management file/artifact selections
were preserved. No whole catalog restore, worker down or wholesale manager call.
New gateway/controller/a-b channels execute an immutable accepted binary copy;
original worker executable inodes/bytes and VMM binary remain unchanged. Existing
native a/b credential files predate deployment and retain their exact identities.

Only the reviewed project node child Access application and exact ordered node/
fixed-asset tunnel routes changed. App returned ID was privately saved before
subsequent writes; actual policy readback matched the owned bypass intent.
Dedicated native origin remains verified HTTPS localhost8790 with actual existing
CA/serverName and noTLSVerify:false. Gateway/human/download origin remains8787;
old dashboard/CLI Access IDs/audiences/policies remain approved. One native
controller, original catalog root and native a/b paths/credentials are preserved.
Only descriptor-verified controller/gateway/a-b agents/dedicated tunnel restarted.
Original descriptor paths now hold their new PID/start/argv records. Legacy
worker and a/b worker processes never restarted; Ubuntu retained its same
recorded private PID/start identity,2048MiB/2vCPU, and disk/checkpoint metadata.

| Post-release proof | Result |
|---|---|
| Public trust and human boundary | Default system TLS; dashboard/admin/nonexact paths protected, no human bypass/header/cookie used by joining host |
| Node denial | Empty/invalid enrollment401; ordinary non-Upgraded WS GET400 classified malformed; actual invalid-bearer Upgrade401 without Access redirect |
| Public selected CLI | Separate Linux binary/checksum readback matched accepted static SHA256 c67635c050509feae3a5f2c11ede32dde394061cf6982acdf967f1c7d66bfe89 |
| Clean bundle | Public manifest and all five fixed assets matched accepted sizes/hashes |
| Native compatibility | Original a/b agents reconnected on native TLS; original bounded workers/credentials preserved |
| Actual public managed join | Accepted static CLI downloaded assets, redeemed real node invitation, launched actual sanitized empty worker/detached agent and received public WSS ready acknowledgement using default public trust; no CA override/human Access |
| Revoke and cleanup | Exact test node revoked; dispatchable state fenced; owned host stop confirmed zero test worker/agent/guest, owner continuity and original three-worker inventory |

No public guest was launched: the actual same-domain managed channel proof was
adequate after local V6 real guest/PTY/cold persistence. Public valid job-path
read-only status roundtrip was not separately exercised; it is pending the
physical owner's workflow and is not a release blocker. Invalid public job-path
Upgrade denial was tested. The revoked public test node remains an audit
tombstone with no resources or operations; no identity cleanup was broadened.
Snapshot restore remains unverified. This local machine's public ingress test is
not actual second-host/NAT acceptance.

Final private receipt: `data/host-verify/release-20261003/deployment-final.json`
with exact post-release file hashes; detail:`public-verification.json`,
`preservation-final.json`, `after-cloud.json`, `test-node-tombstone.json`.
Original online/config/bin/descriptor/artifact backups:`release-20261003/backup`.
Immutable binary:`release-20261003/published/ow-linux-amd64`; gateway CLI/checksum
via separate gateway-assets/bin selection; clean bundle via host-bundle selection.
No live host IDs, app IDs, domain, owner identifiers or credentials are written
into this tracked report. They remain in ignored receipts only.

Operational commands:

    python3 data/host-verify/scoped-channels.py status
    python3 data/host-verify/scoped-channels.py restart gateway
    python3 data/host-verify/scoped-channels.py restart controller
    python3 data/host-verify/scoped-channels.py restart agent-a
    python3 data/host-verify/scoped-channels.py restart agent-b
    python3 data/host-verify/scoped-channels.py restart project-tunnel

Status executed and verified all five scoped channels and untouched workers.
Restart commands are future scoped recovery inputs, not additional executed
restarts. They revalidate exact descriptors/process identity and reviewed release,
use pidfd TERM/exit polling, preserve roots/credentials and update original
process descriptor paths. Old manager hardcodes its old gateway selection; use
this descriptor-aware helper, never its wholesale stop/rollback. Fresh narrow
route/app compensation and channel-only restore instructions remain in private
`data/host-verify/production-runbook.md`.

Completed self-contained handoff:`data/host-verify/deployment-handoff.md`.
Owner sheet:`data/host-verify/second-host-commands.sh`, containing the existing
published installer then host doctor, hidden bare host join and status. The
verified static hash is pinned; invitations are pasted only into the hidden
prompt. No human login or Tailscale step is needed for joining. Owner alone
performs physical guest execution/PTY/cold persistence and records second-host/
NAT placement evidence. Root planner owns HOSTS, board and main task handoff.
