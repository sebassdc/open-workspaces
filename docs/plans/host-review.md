# Host onboarding — independent contract/security review

Updated: 2026-10-03. Reviewer owns this file only. Planner coordinates.

## Final stable bounded clearance — 2026-10-03

**Clear commit `6a6e3d0cc3f70a5c0f16930a17c70a9917685b29` for planner's
scoped merge and project-only deployment of the trusted owner-managed pool.**
No actual blocker remains within the agreed scope. This final verdict supersedes
all earlier pending findings/receipts below; they remain review history.

Reviewer independently checked committed source, final2 build/bundle bytes,
final verifier report, normalized aggregate metadata, receipt hashes and private
approved capture/activation/compensation/rollback/runbook. All **32 compiled
inputs** match committed blobs; **25 scoped files** have no product drift and
exclude AGENTS/protocol/taskboard. All **six V1–V6 groups passed**; aggregate
`minimal_pending` is empty. Every referenced group receipt and core ready receipt
hash matches its actual file; shared V2/V3 and V6 actual static binary attribution
are consistent. V1 uses a separately hashed source-matched dynamic fixture; it
is not mislabeled as shipped-static authentication execution. Static V2/V3/V5/V6
use the accepted CLI. V4 comprises 24 offline cases. V6 is real guest execution,
not a mock/API-only claim, and corrected metadata records vm_run=true.

Accepted pins:

- Source manifest: `c8506e396237bff37b0a546871203ad14a122f2419c55376d4406c48585b2068`.
- Static CLI: `c67635c050509feae3a5f2c11ede32dde394061cf6982acdf967f1c7d66bfe89`.
- Guest: `1e5d3b4bc924755215a56531e36b26ffb3f0632ad270907d73f3aa9605642cf7`.
- Bundle manifest: `506838560beb2a199f6c80eafe8c06bedb943b35f24d13672d6a3619ddc49239`.
- Final aggregate: `3f5da9cf09c34f792c81cad9b012bc04d98c4ec80977c6d1877ad3190667d787`.

Evidence: `docs/plans/host-verification-results.md`, ignored
`data/host-verify/minimal-acceptance-final2.json` and its group receipts; final2
outputs under `data/host-core-build/`. Private `production-runbook.md` now names
the accepted final2 pins. Approved/captured state, exact node and asset plan,
scoped channel identities, compensation and repeat rollback have been reviewed.
Fresh state/identity preconditions must still hold at execution; drift requires
re-planning. Existing serial units are 28 passed/0 failed/3 ignored, attributed
to core/planner reports, distinct from independent V1–V6.

Planner may merge only this scoped commit while preserving unrelated pending
files, select the exact accepted public CLI and clean bundle, and apply only
reviewed project node app/ordered routes plus scoped gateway/controller/agent/
dedicated-tunnel channel restarts. Preserve verified owner issuer/subject/catalog
ID and server CPU/RAM/slot caps, strict native CA/serverName, human Access and one
native 8790 controller. **Do not stop workers/owner Ubuntu, run wholesale manager
stop/rollback, restore catalog wholesale or alter unrelated/global settings.**
Keep backups and perform narrow owned rollback if scoped readback fails.

Independent cleanup records zero test guests/processes and unchanged Ubuntu
baseline. Test peak was one new 256-MiB guest plus existing 2048-MiB Ubuntu,
within the agreed local ceilings. Other invited hosts remain operator-managed
static disjoint budget/reserve commitments, not a hard physical quota claim.

**Post-release acceptance:** actual public Access/tunnel denial and node HTTPS/WSS,
selected/downloaded CLI and asset digest readback, unchanged native a/b rollout
connectivity, owner physical second-host/NAT. These are explicitly subsequent
checks, not prepublication impossibilities. Synthetic human identities/private-CA
edges and local real guests do not establish WAN/public login or physical hosting.
Snapshot restore, malicious-host attestation, broad multi-tenant isolation and
production hardening remain outside this verdict.

Review complete. Only `docs/plans/host-review.md` was written; reviewer ran
read-only source/hash/metadata checks, no tests/builds/VM/services/cloud writes,
commits, dispatch or taskboard changes. No additional owner approval is requested;
planner owns the already authorized scoped merge/deploy and post-release checks.

## Current bounded release priorities — authoritative scope clarification

**Release remains pending, for the trusted owner-managed shared pool only.**
This section supersedes historical severity/status below. No multi-tenant host
framework, physical quota enforcement, malicious-operator attestation or broad
fault campaign is required for this release. Unknown claims remain explicitly
unverified. Planner owns release; reviewer has executed no tests.

Mandatory boundaries: gateway admin checks **verified issuer + subject + configured
catalog owner ID**, server persists/enforces invitation CPU cap alongside RAM/slots,
human routes retain Access, node routes remain exact and separate, one native TLS
8790 controller preserves a/b compatibility, and owner Ubuntu stays running.

### Source fixes now present

Fresh source addresses HR-13's reported windows with atomic marker staging and
`clean_remnants` under lifecycle lock before join/start; cleanup validates exact
nonce names, private ownership and fixed allowed staging contents. HR-14 now
returns unknown on unreadable same-operator process inventory. HR-17 binds ready
to agent instance/generation, held private lock, process start ticks/executable
and matching worker; launch invalidates prior channel state. Agent exit guard
records stopped on ordinary unwinding. These specific source defects are now
corrected pending focused evidence; do not present their old descriptions as
current blockers. HR-10/11/12 are likewise source-corrected.

### Actual remaining release blockers

#### Independent receipt/private-plan inspection — awaiting aggregate finalization

Reviewer independently read the five current private receipts named by planner:
`data/host-verify/no-vm-final2-{dynamic-auth,static}-result.json`,
`artifact-result.json`, `ingress-result.json`, `guest-result.json`; also read
`release-prepare.py`, `scoped-process-stop.py` and `production-runbook.md`.
No harness, prepare helper, test, cloud request or process signal was executed.

V1 receipt records real signed synthetic owner/subject/catalog-ID authorization,
email-spoof/non-owner/wrong-ID/unconfigured denial before mutation, scoped
redemption/replay/expiry, human/node denial, CPU16 heartbeat clamped to 2 and
CPU3 denial with RAM/slots free before effectful dispatch. Empty read-only
reconciliation is explicitly distinguished from mutation. Dynamic fixture binary
is separately hashed `aa70d5182855c96abcd233ffdfa01fae0f352bed573e2855af4c37dc39151962`.
Both V1 and static receipt source maps contain **45 committed inputs matching
stable blobs**, plus one generated `scripts/__pycache__` entry not in git; that
generated cache is not a product source mismatch. Static V2/V3 receipt uses
accepted `c67635...fe89` CLI and contains guided recovery/unknown-stop/PTY evidence.
V4 has **24 offline cases**, no failed cases, including exact policy/trust drift,
partial compensation and repeated narrow rollback. V5 pins actual static/installed
CLI and five-file bundle. V6 receipt records real guest cold persistence, managed
stop/start, prefix/native reconnect, PTY, generation replacement and active revoke;
cleanup reports **zero test processes/guests, owner baseline preserved**.

Final aggregation must reconcile existing generic receipt headers/pending lists:
some still say preparation-only/vm_run=false while recording real V6 execution.
Use explicit group-specific final evidence and exclusions; do not silently call
these generic fields full acceptance. Independently inspected facts above support
the pending bounded verdict; final report attribution still required.

Concrete capture/plan helper is narrow: saved project IDs, complete paginated
GET capture/raw receipts, separate approved baseline, approved native cert trust,
owned exact node POST intent, local ordered ingress staging/validation and recorded
new-app-only DELETE compensation. Channel candidate binds saved bootstrap catalog
subject + ID for review (gateway still enforces verified issuer/subject/ID), retains
native roots/CA/credentials, preserves current disjoint environments, and excludes
workers. Stop helper checks identity and pins lifetime with pidfd; no forced kill.
Review/apply remains planner-owned with fresh fingerprint, exact identity receipts
and Ubuntu continuity. Private plans/approval are not external writes.

Independently compared private `approved-baseline.json`, `captured-complete.json`
and `activation-plan.json`: schema1/complete capture, approved ownership fields and
exact dashboard/CLI app records match; plan before/approved/fingerprint match
that capture. Plan mode0600, one exact new node route, explicit owned node-app
create intent. Read `activation-complete-dry-run.json`, compensation and rollback
dry runs: all report no cloud writes and pin current ingress script; activation
contains only node intent/local config/tunnel plus owner config and four scoped
channels. All four channel candidates use final2 binary; no worker stop/start
operation is present. This is read-only plan inspection, not apply/rollback execution.

**Minimal finalization issue:** `production-runbook.md` still names invalidated
release-final/ready-receipt/host-public-final, manifest `7f25a7...d2aa3` and CLI
`d4e273...ca4a84`. Correct it to final2 manifest `c8506e...5b2068`, CLI
`c67635...fe89`, final2 receipts/bundle and stable commit `6a6e3d0...85b29` before
execution/clearance. Aggregate report and handoff must use the same pins and
explicit dynamic-vs-static evidence. No source change or new test campaign is
needed unless those final checks reveal actual drift/failure.

Public selected/downloaded artifact verification, actual edge/apply/rollback
behavior, existing a/b rollout reconnect and physical second-host/NAT are
**post-release acceptance**, not impossible prepublication prerequisites. The
pre-release contract must contain the exact accepted bin/bundle selection and
scoped apply/rollback steps; existing public bytes are not required to already
be changed. Final bounded verdict follows once aggregate and runbook pins are
consistent, with these post-release checks explicitly reserved for planner.

#### Completed stable source/artifact audit

Reviewed scoped commit **`6a6e3d0cc3f70a5c0f16930a17c70a9917685b29`**
on `work/host-core`. Independently read final `docs/HOSTS.md`, core interface,
report/handoff and release scripts. Compared **committed blobs** against
`/home/sebassdc/dev/open-workspaces/data/host-core-build/release-final2/build.json`:
all **32 compiled input hashes match**, zero mismatches; source-manifest digest
valid. All **25 changed files** have no working-tree diff against this commit.
AGENTS, .agent-protocol and taskboard are excluded. Commit whitespace check has
no diagnostics. This is reviewer-executed read-only hash/source verification,
not test execution.

Independently hashed release and copied bundle CLI/guest bytes; both copies match
build receipt:

- Linux CLI: `c67635c050509feae3a5f2c11ede32dde394061cf6982acdf967f1c7d66bfe89`.
- Guest agent: `1e5d3b4bc924755215a56531e36b26ffb3f0632ad270907d73f3aa9605642cf7`.
- Compiled input manifest: `c8506e396237bff37b0a546871203ad14a122f2419c55376d4406c48585b2068`.

All five staged runtime files independently match manifest size and SHA-256.
Scripts/runbook are committed and unchanged: fixed clean bundle/pins, approved
policy/owner IDs, exact native node/asset ingress, private CA/serverName and scoped
partial/repeated rollback. HOSTS explicitly distinguishes flat bundle selection
from `OW_ASSET_DIR/bin` Linux CLI selection, preserves macOS files, uses fresh
approved capture/precondition, and prohibits wholesale worker/Ubuntu stop or
routine catalog restore. No remaining source blocker was identified within the
agreed trusted-pool scope at this stable audit.

**Stable-source portion complete; final release verdict remains pending.**
Required next: independent V1–V6 receipts attributable to these frozen inputs/
artifacts; concrete private captured/approved state, generated plan and executable
scoped apply/rollback runbook. General HOSTS preparation instructions and offline
fixture success alone do not establish those deployment-specific facts. Planner
must verify actual public CLI selection during rollout. No cloud/service/VM
action, build or test was run by reviewer; only this report changed.

**Latest evidence update — planner attribution:** planner verified final2 product
manifest with no source diffs. Core `smoke-final4` receipt passed real installed-CLI
guided join, TLS, 256-MiB Alpine exec/cold persistence, managed stop twice, saved CA
and native path. Receipt is ignored `data/host-core-build/smoke-final4/receipt.json`
in the core lane. Planner independently repeated complete admission: three workers,
one Ubuntu at 2048 MiB reserved, one VMM, about 24 GiB available; baseline identity
preserved. Reviewer has not independently opened/validated that receipt or its
manifest and did not execute the smoke/admission. This is planner-verified core
evidence, not independent verifier V1–V6 completion.

Current remaining gates: core scoped stable commit/runbook and exact source/build/
shipped hashes; independent V1–V6 results (verifier currently owns exclusive V6);
reviewer comparison of those receipts with stable committed source. Prior source
findings and invalidated receipts remain historical. Prepare the final trusted-
pool bounded verdict when those files/results arrive; no additional fault campaign
or release approval is requested from this review lane.

**Current narrow V3 gate:** supervisor stop correction is now present and source-
reviewed as acceptable for the trusted pool. Rebuild and prove actual shipped
positive stop plus genuine unknown-demand refusal; no fixture-only exemption.
Earlier final source/artifact receipts are invalidated and cannot clear release.

#### Supervisor fix audit and smoke attribution

Read fresh `unrelated_supervisor`, `supervisor_properties`, `no_guest_processes`
and evolving `host-core-report.md`. Inspected onboarding.rs SHA-256:
`ca290a3bc606330517effc524e994f612c00990e3a6cdb8c0102be95e6d0b1a4`
(working tree, not frozen acceptance hash).

Unreadable cwd now passes only positive independent identity: systemd user-manager
MainPID + exact init cgroup/parent/launch; its PAM child with matching parent/group;
or root-supervised tailscaled SSH wrapper plus exact logind leader/service/remote/
scope/UID. Same UID or process name alone does not pass. Root-owned supervisor
tools run with cleared environment, bounded deadline/output, no shell. Query
errors/mismatches return false and preserve UNKNOWN. Readable cwd under this
root's machines still blocks stop. This is a narrow acceptable source resolution
of the overly broad supervisor classification, without a test-only switch or
general exemption for unreadable processes. Runtime owner identity, released
locks and positive cleanup checks remain required.

Core report states first smoke failed with CA-used-as-end-entity before managed
worker/guest startup; scratch listener cleanup and unchanged Ubuntu baseline
were recorded. Correct fixture uses a separate CA-signed leaf with serverAuth/SAN;
TLS validation remains strict. Reviewer read that report, not executed smoke or
independently inspected its private receipt. This failed setup is not V3 positive
host-stop evidence, successful smoke, or independent V1–V6 acceptance.

Minimal next evidence: rebuilt unmodified CLI stops one disposable managed root
successfully on the real host with these unrelated supervisors present, releases
its worker/agent locks/processes, and preserves Ubuntu identity/demand. One
unreadable same-UID mock that lacks supervisor identity, or failed identity query,
must still produce UNKNOWN/no signal. Repeat confirmed stop safely. Pin exact
source/artifact/result hashes and supersede all earlier ready receipts explicitly;
existing V1–V6/runbook gates and trusted-pool scope are unchanged.

1. **Stable-source/rebuild attribution completed:** committed blobs and final2
   artifacts now match in reviewer audit above. Planner independently inspected the core
   report: the alias fixture failure was detached SQLite fixture teardown ENOTEMPTY
   after protocol assertions; exact fixture-only teardown was corrected. Current
   serial result is **28 passed / 0 failed / 3 ignored**, attributed to planner's
   inspected core report, not reviewer execution or independent V1–V6 acceptance.
   The stale 27/1 result is superseded; core final4 smoke is planner-verified above;
   independent acceptance/result attribution remains pending.
2. **Concrete scoped release/rollback evidence (HR-18/19):** latest source checks
   approved policy/IDs and effective TLS, plus narrow partial/idempotent rollback.
   These old missing checks are source-corrected, not current asserted defects.
   Still obtain executable runbook, private approved/captured state normalization,
   V4 evidence and exact owned deltas; no worker/Ubuntu interruption.
3. **Exact shipped artifact evidence (HR-20):** source now verifies copied CLI,
   source/build manifest and clean fixed bundle in marked dedicated storage.
   Still compare frozen source, accepted build and gateway-selected public CLI;
   no new signing/provenance framework is required.
4. **Independent V1–V6:** owner-ID/subject denial, server CPU cap, guided recovery/
   secret handling, persisted CA, readiness and native/public compatibility against
   frozen source/artifact. Source corrections alone cannot clear the release.

### Latest script/readiness/CA source correction audit

Read stronger ingress/build/publisher source and current formatted onboarding
source. HR-18 now compares approved dashboard/CLI app identity, audience/session
and normalized policies including policy IDs; requires a complete normalized
capture; checks wildcard collisions, inner tunnel identity, inherited trust;
pins CA/server certificate digests and verifies chain/hostname through bounded
OpenSSL. Node route explicitly sets noTLSVerify=false with fixed CA/serverName.
HR-19 now validates approved before/current/result state, permits partial node
state for compensation, recognizes already-restored CLI/node entries, binds deletion
to explicit created app ID and provides fresh-state `--check-plan`. These address
the reported source omissions; independent capture/apply and V4 are still pending.

HR-20 now checks dedicated marked private build storage, fixed source/tool/binary
provenance, copied CLI digest/help and atomic output; build script compares source
hashes before/after build. Publisher checks pinned runtime/clean image and guest/
CLI build digest. No new source blocker was identified in these inspected fixes.
Frozen source/artifact comparison and actual fixed public selection remain V5.

Readiness still checks matching agent instance/generation, process start/executable,
held lock and matching live worker; prior stale-file finding is source-corrected.
CA join now validates bounded private PEM and copies it atomically to the owned
host root; config permits only that saved trust path. Assets, redemption and later
agent start consume the persisted CA. This is source evidence, not proof that the
native/private-CA or public-origin handshake was executed.

Minimal additions to existing cases: **V2**, join through a disposable private-CA
endpoint with `--ca-cert`, remove the original input CA after enrollment, restart
without that flag and require saved-root trust to work; changed/symlink saved CA
must fail safely. **V3**, recent ready file + no live matching agent/worker remains
false; new authenticated generation becomes ready. **V4**, normalize actual captured
policy metadata once, then test approved allowlist drift, inherited TLS bypass,
app-only compensation, rollback twice and preservation of one unrelated route.
Retain V1 owner-ID/subject and isolated CPU-denial tests unchanged.

No tests, scripts, Cloudflare reads/writes, builds, services or VMs were run by
reviewer. Earlier reports below are historical; current release hold is frozen
source/result/runbook and independent V1–V6 evidence, not an embedded controller
design or a requirement for a multi-tenant/physical-quota framework.

Review state: awaiting final core receipt with stable source/artifact hashes,
scoped release/rollback runbook and independent V1–V6 results. Resume by checking
that receipt and attribution against the frozen files; no repeated historical
re-review or new broad fault campaign is requested. Final clearance remains
pending; planner coordinates the exclusive smoke/VM evidence turn.

### Narrow HR-14/V3 resolution — actual managed stop

Planner reports current admission inventory resolved four unreadable same-UID
supervisors through systemd/loginctl identity and inventory: three workers, one
Ubuntu at 2048 MiB reserved, one VMM and about 24 GiB available. These are
planner-supplied observed inventory facts, not reviewer host inspection. They
resolve local admission evidence; they do not fix shipped stop classification.
Source still rejects any unreadable same-UID cwd in `no_guest_processes`, so V3
positive cleanup cannot pass on the actual managed host.

Acceptable bounded correction: determine relevance to **this dedicated root's**
worker/agent/guest lifecycle from verified process/service identity and ownership.
Recognize demonstrably unrelated supervisors using ordinary systemd/loginctl or
equivalent positive identity evidence, or confirm owned-process exits through
captured root/process/start identity. Do not treat all same-UID processes as guest
demand merely because cwd is unreadable. Conversely, unreadable evidence for a
potentially relevant owned guest/worker remains UNKNOWN. Retain matching worker
config/assets, positive shutdown and released owned locks. No hardcoded PID/name
skip, fixture-only switch, blanket ignored PermissionDenied or unsafe signal.
No requirement to build a general host-wide process inventory service.

Minimal V3 resolution evidence, through the **shipped CLI/code**:

1. Keep the actual known unrelated supervisors and owner Ubuntu running; stop a
   disposable managed host root after acknowledged shutdown. Require success,
   released worker/agent locks and independent absence of that root's processes/
   guests. Repeat stop safely; owner baseline remains unchanged.
2. Retain one negative fixture: unresolved potentially relevant same-UID process
   identity/demand or held owned lock/missing socket. Require UNKNOWN/denial and
   no signal. Also verify worker config mismatch still refuses shutdown.

Core receipt must explain the relevance/identity rule, pin changed source and
rebuilt artifact, and distinguish real positive cleanup from negative fixture.
Planner/verifier reruns V3 narrowly; V1–V6 and final attribution remain the existing
release gates. No extra fault campaign or reviewer-run test/live action is needed.

### Minimal exact verifier set

| Case | Fixture/action | Required result |
| --- | --- | --- |
| V1 owner and CPU authority | Signed owner invite cap=2; non-owner, correct subject/wrong ID, wrong subject/correct ID, missing owner config; redeem then heartbeat CPU=16 and request CPU=3 with RAM/slots otherwise available; reopen DB. | Owner succeeds; denials make zero admin writes; cap remains 2 after reopen and CPU=3 is denied before worker dispatch. |
| V2 guided/recovery/secrets | Clean-home installed CLI, bare `host join` PTY with synthetic invitation/no Access user; Ctrl-C without Enter; one corrupt blob; one failure after persisted credential but before marker completion; repeat join/start with HOME removed and explicit root. | No login, echo restored/no leaked queued secret, corrupt bytes never spawn, local completion uses zero new redemption, no HOME failure. |
| V3 state/stop | Mock owned worker/agent: ack, exit/restart before 15 seconds while blocking new handshake; malformed status/missing socket + held lock; unreadable same-operator cwd; confirmed shutdown. | No stale online; unresolved state UNKNOWN/no signals; only matching owned shutdown reaches confirmed stopped. |
| V4 ingress/rollback | Captured approved state: altered human allowlist, wrong recorded IDs, global noTLSVerify with absent override, conflicting route; valid plan; app-created/route-failed compensation; rollback with unrelated later route; repeat rollback. | Drift denied; exact node/asset routes precede protected fallback; verified fixed native CA/serverName; only owned resource removed/restored; unrelated route survives; repetition no destructive effect. |
| V5 exact artifact | Accepted binary + freshly prepared five-file bundle; corrupt pin/image marker/symlink; compare copied/public CLI digest and run that installed binary's host help/doctor. | Correct fixed bytes only; failures publish nothing unsafe; served/installed CLI is the reviewed host-enabled artifact. |
| V6 bounded real slice — planner exclusive turn | Unchanged native a/b reconnect plus one prefixed node; owner/non-owner gateway requests; one real Alpine guest exec/PTY and cold disk persistence; generation replacement/revoke; scoped cleanup. | Single-controller routing/fencing, human/node auth separation, real guest behavior and owner Ubuntu continuity; no physical-host/NAT claim. |

V1–V5 may use focused disposable mocks/offline fixtures; do not claim them passed
from source. V6 must preserve owner Ubuntu 2048 MiB/2 vCPU, add at most three
new guests/768 MiB and stay under four guests/3072 MiB total. Verifier inventories
configured/reserved demand and denies unresolved local test demand before spawn.
For other invited physical hosts, capacity is the documented operator's static
disjoint-budget/reserve commitment. HR-16 does **not** require adding a general
physical-host quota scheduler to this release.

Broader crash/exhaustion/path matrices below are useful follow-up diagnostics;
they are not automatically additional release work once these minimal cases and
any actual failures are resolved. Second physical-host/NAT, long-run performance
and malicious-host/multi-tenant isolation remain outside the bounded verdict.

## Early verdict

**Hold host-onboarding release.** The inspected host-core lane is still the
prior multi-node baseline, not the host-onboarding implementation. Findings
below identify concrete missing contracts and unsafe reuse boundaries; they do
not establish defects in code that has not yet been written. Existing bounded
multi-node clearance does not clear public invitation onboarding.

Reviewed lane: `../open-workspaces-lanes/host-core`, HEAD
`716115683cef3f258e32c7d36539a21980060184`. At initial inspection, git status
showed only AGENTS/protocol/brief changes, no modified product files.
`host-core-interface.md` was absent. Read host-review/core/onboarding briefs,
ADR 0002, MULTINODE and prior multi-node review. Evidence is source inspection;
no tests, binaries, workers, VMs, services or cloud operations were run.

## Prioritized findings for planner/builder

### HR-01 — High: designated human owner authority is not implemented

`main.rs:79` exposes only local OS-operator `node-join`; `host.rs:533` calls
`nodes::mint` directly. `remote.rs:32-35` defines human allowlist/bootstrap
email but no designated controller-admin subject. Catalog identity uses verified
issuer/subject (`catalog.rs:181-215`); that is a foundation, not an admin policy.
The new browser/remote invitation and revoke flow must establish an explicit
designated owner bound to verified issuer + subject/catalog ID. Reusing the
email allowlist, caller email header or node credential as owner authority would
let the wrong principal administer the shared pool.

Required contract: admin identity provisioning/migration, missing-owner denial,
exact owner routes/methods, admission disclosure, and safe invitation output.
Regression cases: owner succeeds; another allowed subject fails; forged email
header fails; same email with different subject fails; wrong issuer/audience and
node credential fail; absent owner configuration fails closed. All denial cases
must precede invitation/revocation database writes.

### HR-02 — High: same-domain node routing/base-path contract is absent

`nodes.rs:501-503` serves `POST /enroll`, `GET /node/{node}` and
`GET /job/{node}/{id}` on the dedicated listener. `nodes.rs:578-595` rejects
any controller base path; `:745` and `:788` concatenate root-level protocol
paths. No `/_nodes/` routes or owned public node ingress preparation exist.
Simply sending the current agent to the human domain cannot implement the
brief. A broad JWT bypass or arbitrary proxy would weaken the human boundary.

Required contract: exact public paths/methods, bounded prefix parsing/stripping,
fixed loopback upstream, TLS trust at edge/origin, upgrade handling, and legacy
native listener compatibility. Human assertion/cookie authority must not become
node authority; preserve the actual node Authorization header.
Regression cases: enrollment/control/job through the configured prefix; old
native origin still works; wrong methods, query, suffix, traversal, encoded
separators, duplicate slashes and absolute URI are denied; `/_nodes/api/state`
and `/api/state` with node bearer never bypass human auth. Test HTTP and WS
redirect refusal, wrong CA/hostname, timeout and disconnect separately.

### HR-03 — High: installed CLI/runtime publication is not host-ready

`main.rs:49-111` has no `host doctor/join/status/start/stop` commands.
`main.rs:205-209` resolves runtime assets through an environment override or
build-time repository path. `dashboard.rs:156-165` and `publish-cli.py:25-26`
publish only fixed CLI artifacts; `cli/install.sh` installs the CLI/login helper,
not Firecracker/kernel/Alpine/slirp assets. Updating gateway source alone cannot
update the independently served `assets/bin/ow-linux-amd64` binary.

Required contract: exact fixed manifest/files/versions/hash/size bounds, private
asset root and atomic verified publication, supported host tools/filesystem/KVM,
and artifact rebuild/selection tied to the reviewed source. No arbitrary URL,
symlink/path-traversal archive or privileged repair fallback.
Regression cases: clean user install outside repo; downloaded CLI really exposes
host commands; corrupt/truncated/missing/oversized assets fail before spawn;
unsafe entries and symlinks fail; failed download preserves prior valid bundle;
unsupported KVM/Btrfs/namespace/tool reports actionable error without mutation.
Keep macOS client installation and existing protected paths compatible.

### HR-04 — Medium: daemon reuse can silently defeat selected budgets

`host.rs:424-426` returns any responsive worker status from `up` without checking
the proposed configuration. Spawn at `:429-440` inherits caller environment;
supervisor uses external commands and asset paths (`:335-385`). This is a
specific unsafe reuse boundary for guided host capacity and restart commands.
New lifecycle code must validate worker identity/root/assets/effective budgets
before declaring success and sanitize child environment, including remote login
selection and loader/tool/runtime overrides. Do not stop a PID by number alone.

Regression cases: existing worker has larger/different budgets, stale PID with
reused process number, concurrent start/stop, absent agent/live guests, inherited
OW_SERVER/data/assets/capacity overrides and loader variables. Repeated start
must either prove matching effective configuration or clearly refuse; stop
must touch only owned matching processes and document whether guests remain.

### HR-05 — Medium: invitation re-mint changes outstanding node limits

`nodes.rs:113-127` permits re-mint for an unenrolled node ID, updates its shared
memory/slot row and inserts another active join secret without invalidating old
ones. `:138` consumes any matching unexpired secret against that current row.
Thus an earlier secret can redeem after a later mint with changed limits. This
is existing observed source behavior; the new invitation contract must choose
replacement invalidation or immutable per-invitation policy, rather than assume
each secret permanently encodes its original capacity/consent.

Also `mint` publishes the private join file before transaction commit (`:129`);
commit failure can leave an unusable file. Enrollment commits before delivering
credentials (`:145`), so lost response is consumed/uncertain, not a safe fresh
retry. Preserve explicit revoke/new-ID recovery or specify a bounded secure
alternative. Regression cases: two invites same node with different budgets,
expiry boundary, parallel redemption (one success), revoked invite, wrong node,
database commit failure, response loss and private credential publication failure.

### HR-06 — High release gate: project-only ingress rollback is unspecified

Existing `publish-cli.py:14-23` checks recorded application ownership and public
policy, but only manages CLI downloads. It does not establish new node Access
application/ingress ownership or rollback. Its legacy CLI route is removed
before the backup is saved (`:29-37`), so that file is not an exact pre-change
backup when upgrading the old route. Do not copy that ordering into node rollout.

Required preparation: validate complete current project-owned state and collisions
before mutations; save exact pre-change application/policy/ingress and binary/
config/catalog backups; define repeat execution, partial failure compensation,
missing-state refusal, and narrowly owned rollback that preserves unrelated rules.
Regression cases use offline fixtures, not cloud calls: foreign conflicting app,
altered route/policy, failure after app creation, repeated apply, missing recorded
resource and rollback with unrelated later ingress edits. Planner owns deployment.

## Existing safeguards to retain

Source-reviewed baseline: transactional node-bound expiring one-use redemption
with revoked check (`nodes.rs:131-145`); hashed bearer authentication (`:187-201`);
generation-bound control/job fencing (`:272-389`); HTTP redirect refusal on
enrollment (`:737-741`); exact GET/HEAD/no-query public CLI allowlist
(`dashboard.rs:156-175`). These are not independently executed host-onboarding
acceptance. Node-reported capacity is not host attestation; honest-host static
disjoint limits and host reserve remain required. Enrollment is not permission
for host operators to manage human workspaces.

## Planner baseline correction (authoritative)

The owner's Ubuntu dev VM is already **running at 2048 MiB / 2 vCPU**. Preserve
its running process, disk and checkpoint. Tests may add at most three guests /
768 MiB, with the existing baseline included in a global ceiling of **four
guests / 3072 MiB**. New static worker budgets together must remain at most
1024 MiB. Inventory every worker and unresolved reservation before admission;
unknown/offline/pending/uncertain demand is not zero. Deny new test admission
when demand cannot be resolved within the ceiling. Measured VMM/helper/cache/
desktop headroom remains necessary; configured guest RAM alone is insufficient.
These are planner-supplied constraints, not reviewer-observed live inventory.

Rollout and rollback must **never run `data/nl/manage.py stop` or `rollback`
wholesale**: those kill the owner VM. Restart only scoped project gateway,
controller and agents, checking process owner/argv/start identity. Leave live
workers and the Ubuntu VM running. The previous multi-node handoff's wholesale
manager instructions are superseded for this release. Catalog restore requiring
worker/VM interruption is a separate maintenance decision, not routine rollback.

## Interface review update

`host-core-interface.md` appeared during review; read after the early findings
were published. It is a proposed contract, with product files still unchanged
at HEAD `716115683cef3f258e32c7d36539a21980060184`. It addresses the intended
owner binding, exact namespace, cleared subprocess environment and effective
baseline caps. HR-01 through HR-06 remain implementation/evidence gates rather
than claims that the builder omitted these proposals.

- **Owner binding:** `node_owner_subject` + `node_owner_user_id` must both match
  the verified issuer/catalog row. Treat the subject and ID as an inseparable
  pair; wrong ID with correct subject and correct ID with wrong subject fail.
  Missing or partially configured owner fails closed. Return owner-control UI
  state only from that verified check; hiding buttons is not authorization.
- **Exact Access/route precedence and owner ID:** record and validate the saved
  dashboard, CLI and node application IDs, account/zone/tunnel ownership and
  effective domain/policy before applying. An Access policy's `precedence` field
  is not proof of application path selection or ordered tunnel ingress. Require
  the exact node regex before the protected hostname fallback, with CLI routes
  still narrowly selected. A `/_nodes/*` edge application alone is not an exact
  origin route allowlist. No `starts_with("/_nodes")` auth exception, broad
  wildcard proxy, default everyone bypass on the hostname, or altered shared
  ingress. Fail closed on foreign owner IDs, missing/duplicate recorded resources,
  overlapping/shadowing routes or policy drift. Fixture checks must exercise
  effective ordering and repeated apply/rollback, not merely regex presence.
- **Invitation policy/consent:** proposed response includes `policy` and
  `expires`, but the private file/node redemption contract must specify whether
  these fields survive export and how consent is tied to the advertised shared
  pool. Reject missing/unsupported policy; never infer consent from `start`.
  Define lost-response recovery for remote invite creation as well as redemption;
  a blind retry cannot silently change limits or multiply valid secrets (HR-05).
- **Runtime archive ambiguity:** the interface lists `network-tools.tar.gz`
  while saying "direct files, no archive extraction." Specify its actual consumer
  and required executable layout. If extraction exists anywhere, reject absolute
  paths, traversal, symlink/hardlink entries, special files, duplicates and expanded
  size overflow before publication. If not extracted, show how the required tools
  are supplied on a clean host. Verify all consumers of the bundle, not just downloader.
- **Storage budget ambiguity:** `--storage-gib N` is named without admission or
  enforcement semantics. State whether it is a quota, reservation, or preflight
  free-space threshold; document exhaustion/restart behavior. Existing ADR disk
  preflight is not a hard quota. Do not promise a host disk cap without evidence.
- **Stop boundary:** `host stop` is proposed to stop that invited host's worker
  and guests. It must refuse an unrelated/legacy controller root and verify root/
  worker/process identity. This is separate from planner rollout, which must never
  invoke it against the owner Ubuntu worker. Status should distinguish local
  worker/agent health, controller reachability, revoked enrollment and guest state;
  an agent disconnect does not prove guests stopped.
- **Installer compatibility:** exported invitation must work without human
  dashboard login. Require `host join` to avoid saved-server routing and not
  require a host participant to join the human email allowlist. New Linux host
  commands must not alter macOS client behavior or the legacy native protocol.

## Pending final review

## Required scope corrections — evolving source review

Planner supplied these requirements after the interface was emitted. Inspected
the current interface and evolving `nodes.rs`, `remote.rs`, `catalog.rs` and
`main.rs` at unchanged HEAD `716115683cef3f258e32c7d36539a21980060184`.
Working-tree product edits now exist; prior "unchanged product" statements above
describe the earlier inspection only. No acceptance tests were run by reviewer.

### HR-07 — High: embedded-only startup breaks deployed native a/b agents

Current interface says to stop the standalone controller before embedding.
`nodes.rs:521-537` acquires the same exclusive root lock, resets heartbeat/session
rows, constructs a fresh controller map and exposes a router only through the
gateway. `remote.rs:514-520` nests that router at `/_nodes`; it does not serve the
native 8790 TLS listener. Leaving the old standalone process running conflicts
on the root lock; stopping it removes the deployed a/b endpoint. Preserving
native route definitions in a separate command is insufficient compatibility.

Required correction: a single root controller owner, shared sessions/jobs/
generation map, proxy-dispatch owner and connection/admission limits across both
the native TLS 8790 listener and the public embedded namespace. Keep a/b's exact
native URL/port, CA/hostname trust, credential files and node IDs unchanged.
No second process/map writing the same root, no heartbeat reset by an auxiliary
listener, and no silent migration of a/b to the public origin. Scoped controller/
gateway/agent restart may interrupt channels; workers and owner Ubuntu stay live.

Regression evidence: enable embedded routes while unchanged a/b agents reconnect
to native 8790; both become dispatchable and retain placement/ownership. Join a
new public-prefix node concurrently. Replacement through either listener fences
the old generation through the other; revoke closes both kinds of stream. Confirm
one controller lock and shared bounded queues, matching job lookup regardless of
listener, retained offline/uncertain demand and owner VM continuity. This is a
release blocker until corrected and independently exercised.

### HR-08 — High: invitation has no server-owned CPU ceiling

Interface invitation JSON has `{node,ttl,memory,slots}` and only local join offers
`--cpus`. Current `remote.rs:430-434` input/call and `nodes.rs:110-141`
invitation persistence likewise omit CPU. Scheduler admission currently uses
heartbeat `capabilities.vcpus` (`catalog.rs:518-528`, `:563-569`). Thus a node
advertising a larger CPU count is not capped by an owner-approved invitation.
A guided CLI CPU limit alone cannot enforce invitation authority.

Required correction: persist validated owner-issued CPU cap with the invitation/
node, export it for guided selection, and cap effective advertised/scheduled CPU
capacity server-side. Local selection may reduce every invited limit, never raise
it. Enforce the cap on automatic and explicit create, resize/start/restore/fork,
inventory reconciliation and reconnect/restart. Define conservative migration
for legacy a/b records without changing their deployed effective budgets.

Regression evidence: forged heartbeat and tampered pasted/file invitation cannot
raise CPU admission; aggregate reservations plus unknown guest demand count;
owner cap survives redemption/restart; changed local config cannot widen it;
re-mint follows HR-05 policy. Verify CPU denial independently of RAM/slots so a
memory failure cannot mask missing CPU enforcement.

### HR-09 — Required UX/authority contract: default guided hidden join

Current interface documents mandatory-looking controller/file/capacity flags;
`main.rs` still has no guided host action. Required default is **`ow host join`**:
hidden invitation paste, pool disclosure and explicit consent, capability
diagnostics, guided bounded capacity selection, then private persistence/start.
File input and flags are advanced alternatives, not prerequisites. Define the
invitation envelope's controller origin/prefix, expiry, policy version and bounds
so the default flow works without a separate copied URL. Validate all parsed
fields and TLS origin before sending secrets; no secret argv/URL/echo/log/debug
output, shell history or clipboard re-publication. Restore terminal echo on
error/interruption; cancel without consuming an invitation or spawning workers.

Participant must not need Cloudflare Access login, cloudflared login helper,
human allowlist admission or a human catalog row. Only invite/revoke requires
the verified designated human owner. Host doctor/join/start/status/stop must
remain local despite saved human login, `OW_SERVER` or `--server`; conflicting
options should clearly fail or use explicitly documented local routing.

Regression evidence: clean-home installed CLI plus invitation alone succeeds
through exact public node HTTPS/WSS without a human cookie; no login prompt;
hidden PTY input and cancellation restore echo; advanced private file works;
invalid/missing/expired policy/bounds fail; unexpected EOF/non-TTY produces a
clear safe alternative; saved remote client state does not redirect host commands.
Do not claim guided acceptance from help output or a flag-only test.

Planner handoff: HR-07 and HR-08 are concrete current contract/source blockers;
HR-09 is a required scope/acceptance correction. Builder must update the interface
and implementation together. No approval question or product change is needed
from this review lane.

## Remaining final evidence

## Source corrections observed — next evolving inspection

Inspected current diffs in `nodes/main/client/remote/host/catalog.rs`; HEAD still
`716115683cef3f258e32c7d36539a21980060184`. This is working-tree source evidence,
not a stable revision or test result. The interface still describes embedding
at this inspection and must be synchronized with the selected native topology.

- **HR-07 topology corrected in source:** `nodes::routes` attaches native
  `/enroll`, `/node/{node}`, `/job/{node}/{id}` and exact `/_nodes/` aliases
  to the same cloned Controller state. The embedded constructor/gateway mount
  is removed; the original exclusive root lock, native TLS listener and proxy
  owner remain. This resolves the second-map/native-listener design defect in
  the inspected code. Public edge routing to native TLS 8790, certificate trust,
  exact Access ownership/precedence and unchanged a/b reconnect/fencing remain
  unexecuted release gates. Do not terminate native 8790 in rollout.
- **HR-08 CPU authority correction present:** new `node_limits.vcpu_cap`
  persists a checked 1–16 CPU cap in the same invitation transaction. Human
  invite JSON accepts `cpus` (default 2); heartbeat storage clamps advertised
  RAM/slots/CPU to server-owned values. Existing catalog automatic/explicit
  admission consumes the clamped capabilities and sums reserved/extra CPU.
  Legacy records without `node_limits` retain fallback 16; old native mint
  deliberately uses 16, preserving its earlier protocol. Verify actual a/b
  advertised/effective caps, every lifecycle admission path and restart behavior
  independently before marking HR-08 closed.
- **HR-05 reissue correction present:** invitation transaction marks older
  secrets for that node consumed before commit. New envelopes carry controller,
  policy, expiry and RAM/slot/CPU bounds. This resolves the previously described
  coexistence of valid secrets with changed limits. Commit/publication and lost
  HTTP response remain separate failure cases; atomicity has not been fault-tested.
- **HR-01 authority correction present:** config requires subject + catalog ID
  together; `host_admin` checks verified issuer/subject and `bound_identity`
  matches configured ID/issuer/subject in SQLite before parsing/mutating. Exact
  invite/revoke operations retain prior JWT/Origin checks and add POST/JSON/
  `x-ow-request`/bounded-body checks; response is no-store. Non-owner denial,
  wrong ID/subject combinations and config migration have not been executed.
- **HR-09 entry points present, lifecycle pending:** main now defines bare
  `host join` with optional advanced flags and handles participation before
  saved human-server lookup. Explicit `--server` is rejected for participation;
  ambient `OW_SERVER` and saved login are bypassed. Owner invite/revoke uses
  the human client separately. This supports participant independence in source,
  but `main.rs` declares `mod onboarding` while `onboarding.rs` is absent at
  this inspection. Hidden input/consent/download/process behavior is therefore
  not reviewable yet; CLI declarations alone do not establish guided join.
- Enrollment response handling now refuses redirects/proxies, bounds response
  bytes to 4096 and verifies returned node + hexadecimal credential. Agent
  reconnect/heartbeat additionally checks local worker liveness. Preserve the
  explicit consumed-secret recovery contract and independently test these paths.

## New actionable source findings

### HR-10 — Medium: invitation delivery errors occur after replacement commit

`client::host_admin` checks only `output.exists()` before the HTTP call. On Linux,
owned/private parent checks happen in `nodes::write_private` only **after** the
server commits the new invitation and invalidates the old one. A missing/public/
unwritable parent or failed local publication leaves the operator without the new
secret and invalidates the earlier invite. With no output file, non-TTY stdout
is also rejected only after committing remotely. The non-Linux path writes mode
0600 directly without validating private owned parent or atomic publication.

Correction: validate output parent/type/ownership/privacy and stdout mode before
network mutation on every supported client platform. Stage private output where
possible and publish atomically; do not leak a secret into ordinary redirected
output. Remaining post-commit write/transport failures must explicitly explain
that the invitation was possibly replaced and require a deliberate reissue,
rather than report an ordinary local validation failure. Verify invalid output
paths/non-TTY cause **zero** remote mutations, Mac parent privacy, output collision
and faulted publication recovery. Server-side file publication before commit
remains the distinct local `node-join` failure window described under HR-05.

### HR-11 — Medium: revoked unenrolled node can receive unusable invitation

`nodes::invitation` refuses existing non-null credentials, but does not refuse an
existing revoked row with null credentials. Revoke an unused node, then re-invite
the same ID: UPSERT changes budgets but retains `revoked=1`, commits a new secret
and reports success; `enroll` necessarily rejects it because it requires revoked=0.

Correction: reject all revoked IDs at mint with actionable new-node-ID recovery,
consistent with the current no-reuse contract. Do not silently un-revoke an ID
that may retain placement/authority history. Regression: revoke before redemption,
same-ID reissue fails without publishing/invalidating secrets, fresh-ID succeeds;
revoke after redemption remains denied and existing placement stays fixed.

## Publisher/ingress/build source audit

Read `scripts/prepare-host-bundle.py`, `prepare-host-ingress.py`,
`build-host-cli.sh`, the changed guest preparation recipe and musl wrapper.
No script was imported/executed, no Cloudflare read/write was performed and no
artifact was built/published. Source references below are initial script line
numbers. Lifecycle HR-13/14/17 corrections were not yet present in this snapshot;
retain their current residual findings without re-raising native topology.

### HR-18 — High release gate: protected policy and TLS drift checks incomplete

`prepare-host-ingress.py:29-34` accepts any nonempty dashboard policy list with
no bypass decision. An allow-everyone policy or changed allowlist/requirements/
session policy therefore passes. CLI bypass check omits policy precedence/ID;
node check compares the entire policy to a constructed dictionary without a
documented normalization of real API policy IDs/metadata. Require exact approved
human policy semantics/recorded IDs and an explicit normalized capture schema,
not merely absence of bypass.

`native_trust:73-78` checks only an absolute-looking CA path/server-name string
and absence of noTLSVerify in that dictionary. It does not validate inherited
top-level tunnel originRequest (including noTLSVerify), pin these values to the
existing approved native CA/serverName, or establish the CA file's owned/private/
regular identity and certificate match. An omitted route noTLSVerify may inherit
an unsafe global default. Explicitly reject unsafe effective inheritance without
changing global shared settings; compare native trust with recorded existing
endpoint/CA fingerprint/server name before creating the route.

App collision detection (`:49-54`) covers only exact-host domains; require a
complete captured/paginated app inventory and assess wildcard-host path apps
that match this hostname as well. Ownership currently compares duplicated
account/zone/tunnel/hostname values inside the supplied state; planner must bind
them to independently captured saved helper IDs/API state, not accept caller
self-description as proof. No live account/zone/CA identifiers belong in report.

Offline verifier fixtures: change dashboard allowlist to everyone while keeping
decision=allow; alter require/exclude/precedence/AUD/team; wrong recorded app ID;
real API policy metadata vs normalized snapshot; incomplete app pages; matching
wildcard-host path app; global noTLSVerify=true with route omission; wrong CA/
serverName; relative/nonregular/symlink CA. Each must fail before producing an
applyable plan unless a precisely reviewed normalization makes it equivalent.
Retain exact alias methods/paths, native 8790 target and protected fallback order.

### HR-19 — Medium: rollback/reapply and partial compensation contract incomplete

`prepare` produces a precondition hash but does not apply/check it against fresh
live state; planner's eventual apply must enforce it before each external write.
`rollback:112-140` expects node route and new CLI route to still be present. A
second rollback after successful removal/restoration fails rather than being
idempotent. App-created/ingress-failed state also cannot use that rollback because
it requires the node route; compensation is prose only (`:109`). Current rollback
checks outer owner IDs but not `current['tunnel']['tunnel']`, approved dashboard/
CLI app IDs/policies, trust/fallback drift or resulting route precedence. It can
emit a rollback state containing changed owned policy/inner tunnel identity.

Correction: specify explicit apply/partial compensation/rollback phases with
fresh-state checks and privately recorded returned app ID. Validate each owned
resource, including inner tunnel identity and policy, before mutation; preserve
unrelated later entries while refusing conflicting owned drift. Repeated rollback
must recognize already-restored owned state, with no second deletion. Never feed
the returned whole state to a blanket app/tunnel replacement; apply only reviewed
owned deltas, preserving unrelated resources and the owner VM.

Offline fixtures: clean prepare; create-app-only failure compensation; route write
failure; already-applied prepare; rollback twice; wrong inner tunnel ID with same
outer ID; changed human/CLI policy/AUD; changed owned route/TLS; unrelated later
app/route retained; conflicting shadow route refused. Require exact executable
planner commands/runbook before release; this offline planner is not a completed
Cloudflare deployment helper or executed rollback proof.

### HR-20 — Medium: CLI copy/checksum and build provenance not bound

Bundle runtime blobs are rehashed after copying (`prepare-host-bundle.py:60-62`),
but copied CLI is not: `:67` copies it, then `:69` hashes the **source**. Concurrent
source replacement can produce a served CLI/checksum mismatch, despite the final
"matching Linux CLI" claim. Hash/size-check the copied CLI and publish its digest.
Publisher also accepts any regular CLI and ow-guest (guest hash simply becomes
actual bytes), without a source/build manifest or proof of host commands/version.
The clean-Alpine marker binds image bytes to recipe text, not CLI/guest source.

Build script uses locked Rust 1.97.0/musl and a dedicated caller-specified target,
but does not validate target/output/toolchain against live roots, record reviewed
source hashes/compiler/Zig identity, or atomically stage the two output binaries.
Failure after mkdir/copy leaves a nonempty output that the script refuses to
retry. Its help check executes the native built CLI (no VMs) and cannot prove the
separately served binary was selected for the gateway bin path.

Correction/evidence: produce build provenance for frozen source/tool versions and
both binaries; verify copied CLI/guest identity and help on that exact artifact;
publish through private atomic staging with safe failed-output recovery; require
dedicated paths outside live data. Verifier cases: mutate source CLI between copy/
hash, pass unrelated CLI/guest, fail second copy/help, dangling output/parent symlink,
re-run after partial failure. Assert no mismatched pair, live-path write or claim
that untested CLI installation/join passed. Planner must separately confirm the
actual public `assets/bin/ow-linux-amd64` hash equals accepted artifact.

### Fixed asset provenance and positive source observations

Publisher uses five fixed runtime paths, rejects source/parent symlinks,
nonregular/empty/oversized files, refuses existing output, stages privately and
verifies runtime copies before publishing. Kernel/slirp pins match existing
upstream preparation constants; Firecracker executable pin is new and requires
attribution to the pinned upstream release archive, not an invented verification
claim. Guest disk requires `clean-alpine.json` digest matching a newly built
minimal recipe. The changed preparer refuses existing base image, checks pinned
Alpine/kernel downloads, bounds expanded tar members/bytes and uses data-filter
extraction. These are source safeguards, not executed fresh-image evidence.

Still require a complete dedicated-source preparation runbook: old Firecracker/
network fetch helpers default to shared runtime-spike paths, so do not run them
against live directories to fill missing files. New bundle source must contain
only independently verified upstream assets and freshly built clean guest/agent.
Upstream download hash checks are integrity/provenance pins, not runtime/host
attestation. Verifier must test corrupt pin/image marker, symlink/path traversal,
copy failure and atomic old-bundle preservation. No user disk/snapshot or live
host file may be selected for publication.

Planner handoff: HR-18 policy/effective trust, HR-19 scoped rollback/compensation
and HR-20 exact artifact binding remain release gates. No cloud/service/VM action
or test was performed; only this review report changed.

## Fresh lifecycle correction audit — after 19:19 update

This is the current status; earlier observations are historical. Native TLS 8790
with aliases remains the selected single-controller design. No embedded design
defect is being re-raised. Publisher/runbook and independent evidence are pending.
Reviewer read files only; no tests, VMs or services ran.

Working-tree SHA-256 (not committed/tested hashes): onboarding.rs
`69f1cb76c3348a76dc172ed3bc89e00addda3491bdd02618f21fb45a7bca38cb`;
nodes.rs `a9cbd2ab6bd8b6eba43bac89024fc4e62b90a91ba8adaaf99167750a75d25686`;
client.rs `fea9477c47072ac8cc9affdcdd099066efd640f3a0e8c57bc5712d0d2ac7eaa9`.

| Finding | Current source assessment | Independent verifier cases still required |
| --- | --- | --- |
| HR-10 output after replacement | Corrected: private owned parent/nonexistent output/non-TTY checks before token/network; private staging and atomic link on all platforms; explicit post-commit failure explanation. | Invalid output causes zero HTTP mutations; Mac/Linux permissions, collision and publication failure. |
| HR-11 revoked unused ID | Corrected: invitation query denies credential-present **or revoked** rows before mutation. | Revoke unused ID, deny reissue without publication/mutation, fresh ID succeeds. |
| HR-12 eager HOME | Source defect closed: lazy match reads HOME only without explicit root. | Cleared-HOME explicit-root status/run and detached agent startup. |
| HR-13 partial join | Substantially corrected: assets/pending admitted, config saved before redemption, persisted credentials allow local completion. Crash windows below remain. | Process death during staging/private publication/marker write; response loss; local completion causes zero new redeem requests. |
| HR-14 false stop | Improved: private nofollow locks/errno checks, failed RPC requires absent socket/released locks/root guest scan, shutdown waits for exit evidence. Unreadable process scan still skipped. | Missing socket + held lock; malformed/timeout RPC; unreadable /proc; confirmed shutdown and repeated stop. |
| HR-15 hidden cancellation | Prior buffering/signal defect corrected in source: ICANON/ISIG cleared, signal flag handlers, interrupted read loop, termios/handler restoration and pending-input flush. | Real PTY Ctrl-C without Enter, external SIGINT/TERM, EOF, oversized/multiline input, restored echo and no queued secret. |
| HR-16 demand | Still evidence gate: preflight remains RSS/MemAvailable plus warning, without reservation/global admission enforcement. | Independently resolve configured/offline demand and deny unknown demand under four guests/3072 MiB including owner Ubuntu. |

### Remaining HR-13 crash recovery

`finish_enrollment` now validates saved credentials, completes config from pending,
creates marker and removes pending. Ordinary post-credential config failure can
resume. Marker creation still writes directly to create_new (`onboarding.rs:269`):
a crash/short write leaves an empty/truncated existing marker that repeat
completion refuses. Use atomic staged marker publication or a precise safe
repair contract. Death during download or private publication can also leave
`assets-stage-*`/nonce temporary files; root allowlist (`:288`) excludes these,
so owned crash remnants still block retry. Returned-error cleanup is not crash
cleanup. Never broadly delete arbitrary files to recover.

Verifier: kill disposable join during asset write, pending publication and marker
write; inject short marker write/fsync failure. Assert safe owned-remnant recovery,
unrelated files preserved, credential never overwritten, and post-credential
completion performs zero redemptions. Lost enrollment reply still requires revoke/
fresh ID; runbook must give exact safe root recovery, not imply every failure
can repeat transparently.

### Remaining HR-14 unknown process evidence

`no_guest_processes:228-232` ignores /proc metadata/readlink failures and concludes
absence from readable same-UID cwd paths. Potentially relevant inaccessible
processes cannot be ruled out. Require UNKNOWN or stronger owned-process evidence,
not successful stopped output. Verifier: disposable same-UID mock with unreadable
/proc cwd (controlled dumpability), missing socket and released locks; expect
UNKNOWN/denial, no unsafe signals. Reviewer did not execute this fixture.

### HR-17 — Medium: cached readiness is not tied to live agent instance

Guided ready_ack/controller acknowledgement after accepted clamped heartbeat is
an improvement over PID-only online status and preserves non-opt-in native agents.
But channel JSON contains only state/time. `ready:237-240` accepts dispatchable
within 15 seconds without matching current agent/session. Outer worker-status
failure exits before writing disconnected. Status can report dispatchable after
agent/worker exit; immediate restart can combine a new held agent lock with the
old acknowledgement before the new agent writes connecting.

Bind status to current live agent instance/controller generation; invalidate
before launch and on every exit; require live matching worker/agent evidence for
dispatchable output. Verifier: valid ack then terminate mock worker/agent and
query immediately; restart within 15 seconds while blocking new TLS/WS handshake.
Neither stale status nor old ack may report online. New matching authenticated
ack permits readiness; wrong node/generation/expired ack fails. Old native a/b
must still receive job IDs only, without new ack frames.

### Emerging public asset handler — source only

Fixed manifest/five blob paths inherit dashboard GET/HEAD/no-query/no-authority
checks. Handler bounds manifest, validates regular owned non-group/world-writable
files/size, uses nofollow, hashes blobs and streams bounded chunks. Publisher/
immutable bundle ownership and exact edge preparation remain pending. Each request
rehashes its blob; body polling performs synchronous file reads. Verifier should
exercise concurrency/slow disk for bounded blocking work and runtime responsiveness,
including HEAD verification, before claiming exhaustion resistance. No such
performance result is claimed here.

Planner handoff: HR-10/11/12 source-corrected; residual HR-13/14 and HR-17 require
builder/verifier attention; HR-16 needs independent admission enforcement.
All prior concrete test cases remain applicable. Only this report changed.

## Evidence status and next source review

## Guided lifecycle source review — onboarding.rs first publication

Read the actual 266-line `crates/ow/src/onboarding.rs`, current invitation/client
code, worker shutdown and runtime storage/status changes. Line references below
refer to that initial file publication and may move. All findings are source
reasoning, not reproduced tests. HR-10 and HR-11 remain present in inspected
invitation code. Lifecycle implementation now exists; earlier absence statements
are historical.

### HR-12 — High: detached agent always evaluates missing HOME

`onboarding.rs:33` uses `explicit.unwrap_or(... HOME ... ?)`; `unwrap_or` eagerly
evaluates its argument even when explicit root is supplied. `fixed_command:65-70`
clears environment and does not restore HOME; detached `host run` gets an explicit
`--data-dir` but therefore fails with `HOME required` before reading config or
starting the agent. Worker can remain running after `start` fails (`:218-219`).

Correction: resolve HOME only in the None branch; explicit roots must not depend
on HOME. Preserve the cleared child environment. Executable no-VM regression for
verifier: `env -u HOME ow --local --data-dir PRIVATE_ROOT host status` against a
valid disposable config must reach status/config handling, not HOME failure;
exercise detached run/start with a mock owned worker and verify agent startup
failure cannot leave an unreported worker. Real start remains an exclusive VM gate.

### HR-13 — High: partial join cannot retry or complete durable enrollment

Join requires root entries to be only lifecycle lock/doctor files (`:228-230`).
Downloads permanently publish `assets/` before redemption (`:198`, `:239-240`).
Any redemption failure leaves assets, so retry fails the new-root check before
the reusable `downloads` verification path is reached. Credential write precedes
config write and marker (`:241-244`); failure between those steps leaves node.json
without host.json, or config without `.ow-data`. Join refuses either file; start
requires config and underlying launcher marker. A lost redemption response still
has the existing consumed-secret recovery problem. The generic "use start/status
or ask owner for a new ID" instruction does not make these states resumable.

Correction: define durable enrollment phases and strictly owned staged artifacts.
Before consumption, allow verified asset-only retries or safely remove only
this attempt's assets. After credentials are persisted, complete saved config/
marker without re-consuming or overwriting credentials; persist selected config
before the irreversible request where practical. Unknown response/write loss
must clearly require owner revoke/fresh ID and a safe dedicated-root recovery
procedure. Never recursively delete arbitrary user roots.

Executable fault cases for verifier: fixed mock asset server + enrollment endpoint;
fail one blob, fail before enrollment dispatch, reject/timeout enrollment, drop
response after server commit, fail node.json write, fail host.json write and fail
marker publication. Repeat join/start for each phase; assert bounded remote redeem
count, no secret overwrite/leak, supported recovery and no worker start before a
complete owned config/credential/marker. Verify fsync/atomic publication ordering.

### HR-14 — High: unknown worker status can be reported as confirmed stop

`Stop:249-250` skips shutdown on **any** failed status request and returns success
when agent lock appears free and another status request errors. Permission error,
malformed reply, timeout or deleted/unreachable control socket does not establish
that the worker or guests stopped. `agent_running:202-205` treats any open error
as absent and any flock error as running without distinguishing errno or validating
lock owner/type. No PID is signaled, which is good, but absence of successful RPC
is not process-identity/termination evidence.

Correction: preserve stopped vs unknown; require positive shutdown completion and
owned worker/guest/process exit evidence, or demonstrable never-started state.
Validate agent lock as an owned private regular file and distinguish held lock,
available lock, missing file and access/I/O errors. Unknown state must not print
"worker and its guests stopped". Retain matching assets/budgets and refuse legacy
roots; do not add PID-number-only kill as a fallback.

Executable no-VM cases: mock worker returns malformed status, delays until timeout,
closes early, or disappears while an owned mock process stays alive; missing socket
plus live process; unreadable/symlink/foreign lock; held lock from wrong mock process;
positive shutdown/exit and repeated confirmed stop. Assert unknown result and zero
unsafe shutdown/signals for mismatched identity; verify no false stopped message.
Reserve actual guest shutdown/cleanup evidence for verifier's authorized VM turn.

### HR-15 — Medium: hidden input retains canonical buffering and pending secret

`input:135-139` clears ISIG/ECHO but leaves ICANON enabled. Ctrl-C is now byte 3,
yet canonical input is not delivered until newline; pressing Ctrl-C during paste
does not promptly cancel. Early error/length/cancellation exits restore termios
with TCSANOW without flushing remaining input, potentially leaving secret suffix
or later pasted lines queued for the parent shell after echo returns. Default
external SIGINT/SIGTERM also bypasses Rust Drop, so RAII alone is insufficient.

Correction: provide bounded immediate cancellation, restore terminal state on
supported signal paths and discard this prompt's pending input on cancellation.
Define single-line vs multiline paste and reject excess input without handing
secret fragments to the shell. Executable PTY cases: paste partial secret then
Ctrl-C **without Enter**, oversized paste, multiline paste, EOF and external
SIGINT/SIGTERM. Confirm timely exit, restored echo and no queued secret bytes,
stdout/stderr/log disclosure or shell execution. Do not use real secrets in fixtures.

### HR-16 — Admission evidence gap: RSS warning does not enforce unresolved demand denial

`existing_demand:86-100` counts process names/RSS and emits an operator warning;
`preflight:120-124` checks MemAvailable/free space only. It neither resolves
configured/reserved/offline demand nor denies it. Sparse guest RSS is not configured
guest RAM; process-name scanning can undercount unreadable/renamed processes.
The current code therefore cannot substantiate the planner's global four-guest /
3072-MiB ceiling or unresolved-demand denial. Do not claim such enforcement from
doctor output. The generic invited host may use documented disjoint operator
budgets, but local verifier/rollout admission must enforce the authoritative
baseline constraints separately if runtime code does not.

Executable verifier cases: baseline 2048-MiB owner demand plus existing/pending/
offline reservations; sparse low-RSS guest with large configured RAM; unreadable
inventory; attempted fourth new guest/excess reserved RAM. Assert denial before
spawn unless complete inventory proves the cap, with Ubuntu remaining live.
The no-test reviewer has not inspected live host inventory or run these cases.

### Source safeguards observed, without executed clearance

The asset list is fixed to five names/destinations; manifest rejects duplicates,
unknown fields, wrong runtime/arch, invalid hashes and excessive sizes. Downloads
refuse redirects/proxies, stream bounded hash-checked blobs into private staging,
sync files and rename after complete verification; no archive is extracted.
Startup rechecks manifest/assets and rejects symlink parent components. Environment
is cleared and worker reuse compares saved budgets, managed marker and manifest
hash. Join validates expiry/controller/policy/bounds and asks for explicit consent;
participation avoids human login. Doctor gives nonprivileged KVM/Btrfs/tools/
namespace diagnostics and cleans its small reflink probe. Runtime now consumes
minimum-free-GiB in storage headroom checks, retaining the non-quota limitation.

Remaining executable asset cases: corrupt/truncated/oversized download, wrong hash,
duplicate name/path traversal, final/parent symlink, missing executable permission,
partial staging cleanup and repeat join after failed download/redemption. Assert
no process spawn, preserved valid bundle and no traversal/privileged repair.
Check ordinary status/doctor command results: Doctor currently prints supported=false
but returns exit code 0; decide/document whether automation should receive failure.

Planner handoff: HR-12/13/14 are actionable startup/recovery/false-stop blockers;
HR-15 requires PTY cancellation correction, HR-16 requires explicit acceptance
admission evidence. Only this report changed; none of the proposed commands/tests
were executed by reviewer.

Interface synchronized during this review: it now explicitly retains the one
native TLS 8790 controller and a/b endpoint/CA/credentials; defines exact ordered
node ingress to that listener with private-CA trust/server name and no TLS bypass;
documents scoped rollback without catalog restore; includes CPU bounds, default
hidden-paste join and participant independence. It removes the network-tools
archive from the Alpine-only bundle and labels storage GiB a minimum-free-space
threshold, with join/start/create/capture enforcement proposed. These resolve
the corresponding contract ambiguities; verify their actual implementations
before closing the source/evidence gates. RAII alone does not restore echo on
default SIGINT termination: inspect the eventual signal/cancellation path and
require a real interrupted-PTY check.

Reviewer ran no compilation, test, VM, service or cloud action. No stable product
hash manifest, installed new binary or executed acceptance is available from this
inspection. Re-read synchronized interface, emerging onboarding implementation,
artifact publisher and scoped TLS tunnel preparation next; retain all corrected
baseline/owner-VM constraints. Source corrections above must not be substituted
for independent verifier evidence.

Pending: executable interface implementation/runbook, invitation consent/admin implementation,
runtime manifest and installer/lifecycle code, dedicated ingress preparation/
rollback, stable product hashes and independent verifier evidence. Final verdict
must compare accepted product manifest hashes with stable committed blobs and
actual installed CLI artifact, and distinguish focused mocks/browser fixtures
from real TLS/VM acceptance. No physical second-host/NAT, production, malicious
host attestation or full isolation claim is supported by this review.

Planner can use HR-01 through HR-06 immediately. No user decision is requested;
this report leaves implementation, verifier coordination and release ownership
with the planner. Only this report was written; no product/taskboard/commit/
service/cloud/VM changes were made.

Resume here: inspect current lane diff and `host-core-interface.md`, evaluate
HR-01 through HR-06 and the interface ambiguities, then inspect the independent
`docs/plans/host-verification-results.md` against frozen source/artifact hashes.
The verifier's current report is preparation, not executed host acceptance.
Open user decision: none. Source corrections and executed evidence must be
recorded separately before a bounded final clearance.
