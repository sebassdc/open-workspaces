# Independent host capacity verification
Updated: 2026-10-04 | Size: S | State: **focused V4 no-VM acceptance passed**.

W8 is closed by the independent executed legacy/new/local HTTP/TLS checks below. This is a bounded verification verdict, not release approval: reviewer and parent own source review, services, artifact selection and rollout. V2/V3 results are superseded and remain preserved privately. No product edits, commits, owner/remote mutations, cloud writes or VM activity were performed by this verifier.

Authority: [verification brief](host-capacity-verify-brief.md), [capacity brief](host-capacity-brief.md), [review](host-capacity-review.md), [rollout](host-capacity-rollout.md). Pattern: ADR 0002 Rust gateway + SQLite owner authority + outbound node agent + private bounded worker. Fixtures are new owned private `data/cv/` roots; report/receipts are under `data/host-capacity-verify/`.

## Exact V4 attribution

| Input | SHA-256 |
|---|---|
| Compiled source manifest | `8411c0c76b9a15582bc211f87a2b3b709b5213a7eaf0f3e8b7223c1a808054fe` |
| Static shipped Linux CLI | `113dc71a6fe879081b0118881ecbce61612af426d13dd0bc9a8802c091b90f55` |
| Dynamic verifier-only CLI | `ec0b111d3b9792906f9bbdfb9057aa7394403a707a0f0f428337895d55eb5970` |
| Guest agent (not executed here) | `1e5d3b4bc924755215a56531e36b26ffb3f0632ad270907d73f3aa9605642cf7` |
| Seven-file bundle manifest | `89f78296c02093e91dc82bfef564381bb74f6ccaca339d1f4a1332178dce95f1` |
| Static build receipt | `0427ffc8ab80bd93b3291cc31e3e07082cfe6f14a97339cd6756df20b8b58ad8` |
| Dynamic build receipt | `eeb259fdc2dd1401cb63a6544f450ed65a579f03f45ec588bada61cda11beebb` |
| Bundle preparation script | `99cfda4e84d9a853e374481fad4e649da6a3c57ec0747b22f1bdb16058db77b0` |
| Ingress script | `7be8eb3a339d6267a7977e0d21d49e0a8f195d8e65324061544bf2f9882c467c` |

Lane: `/home/sebassdc/dev/open-workspaces-lanes/capacity-core`. Read-only artifact inputs: `data/capacity-build/release-v4/build.json` and sibling `ow-linux-amd64`, `dynamic-v4/build.json` and sibling CLI, `bundle-v4/`, `frozen-pins-v4.json`. Static/dynamic receipts have identical source maps. Recomputed source-map fingerprint, every listed current source file, separately pinned scripts and both CLI hashes before/after execution and at final cleanup. All matched. Bundle build receipt equals static receipt. No old dynamic artifact was used with new source.

Final observed lane HEAD: `1ca48437a236bf8ebffe2b771538824e21e71ae7`. Final `git diff --binary HEAD` hash: `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` (empty tracked diff). Parent/core committed the previously dirty freeze; compiled input identity is the manifest above. Toolchain receipt: Rust 1.97.0; static release musl vs dynamic release glibc. Dynamic evidence is not a shipping claim.

## Executed commands and results

All commands ran from `/home/sebassdc/dev/open-workspaces`. Exact child argv/exit/output, HTTP statuses, runner/helper hashes and cleanup are in the private receipts. No broad unit, VM, installer or previously finished source suite was rerun.

| Command | Final result | Private receipt |
|---|---|---|
| `python3 data/host-capacity-verify/static-v4-focused.py` | Exit 0; **20 focused cases passed** | `static-v4-result.json` |
| `python3 data/host-capacity-verify/dynamic-v4-focused.py` | Exit 0; **6 HTTP/TLS groups passed** | `dynamic-v4-result.json` |
| `python3 data/host-capacity-verify/supplement-v4-focused.py` | Exit 0; **2 browser/cache groups passed** | `supplement-v4-result.json` |
| `python3 data/host-capacity-verify/ingress-v4-focused.py` | Exit 0; **20 literal/offline assertions passed** | `ingress-xwyc0tb_/result.json` |
| `python3 data/host-capacity-verify/w8-v4-focused.py` | Exit 1; **first 3 W8 groups passed**, then fixture JSON-format comparison failed; retained, not labeled wholly passing | `w8-v4-result.json` |
| `python3 data/host-capacity-verify/w8-v4-local-caps-focused.py` | Final exit 0; **remaining 2 W8 groups passed** after fixture corrections | `w8-v4-local-caps-result.json` |

The narrow W8 retry did not repeat the first three passed groups. Historical failures and their fixes are explained below. Gate claims are tied to their actual completed cases, not the failed runner's overall exit.

## Gate findings

**G0 — frozen chain: passed.** Exact source/static/dynamic/guest/manifest/script identity and source stability checked as above. Receipt hashes and final source verification are in `final-v4-summary.json`. Real template bytes/provenance and full 8 GiB transfer remain core evidence, separately attributed.

**G1 — authority, identity and revocation: passed (dynamic gateway, static TLS controller).** Reused signed synthetic JWKS/JWT/certificate transport from `experiments/runtime-spike/multinode-test.py`, restricting process launches to private gateway/controller. Owner budget edits work for the enrolled ID; ordinary user, spoofed email, wrong durable owner ID, node bearer, unknown/local node and revoked node fail. Credential hash/node ID/enrollment count/session generation survive edits and gateway restart; edited owner ceilings persist. Revoked credential cannot reconnect. Invitation/credential/JWT bodies were omitted from receipts/report; synthetic browser JWT resides only in private fixture config.

**G2 — numeric input and browser/API shape: passed.** Null, negative, fractional, string, unsupported shape, maximum+1 and u32/u64 overflow resource inputs reject at create/resize/budget ingress with no new resources/operations/reservations or mock effect dispatch. Missing resource fields retain 256 MiB/1 CPU. Real Chromium submitted default and 4096 MiB/2 CPU create payloads and numeric enrolled-budget fields without expiry; a private relay sent them to the actual signed dynamic gateway/static TLS mock node. Captured larger payload correlates with 4096/2 durable reservation and mock dispatch. Stopped resize accepts numeric 4096/2 and rejects shapes above operator consent. Frozen UI has no machine-resource resize form; API resize is separately tested. Browser/helper adaptation changes only UI path/node fixture name and replaces synthetic API with private authenticated relay.

**G3 — owner/operator budgets and conservative lowering: passed for focused ordered scenarios.** Owner increases do not expand mock operator consent. RAM-only, CPU-only and slots-only effective ceilings each constrain real scheduler admission. Lower while online idle, replay a subsequent larger heartbeat, and admission stays within current durable dimensions. Lowering rejects unknown/running inventory, unregistered RAM/CPU/slots, CPU-only registered reservation, pending/uncertain operation and offline node, preserving budget/demand. Private SQL demand injection is only in the dedicated synthetic catalog. No forced simultaneous heartbeat old-read/write or reservation-vs-lowering interleaving was independently executed; core's focused concurrency evidence stays separate.

**W8 — mixed-version guest maxima: passed.** Legacy enrolled heartbeat with absent `max_guest_memory_mib`/`max_guest_vcpus` reports 2048/4 despite aggregate 8192 MiB/8 CPU. Explicit/automatic 4096 MiB create and >4 CPU create, plus legacy stopped resize beyond either dimension, reject before new durable intent/resource/reservation/effect dispatch. Allowed legacy 2048/4 succeeds. Each missing optional dimension independently retains fallback. Explicit 16384/16 advertisement permits 4096/2 create and stopped resize, with exact reserved shape and mock dispatch. Automatic larger create selects enrolled fixture rather than literal local, including while a local socket exists.

Pinned-local checks simulate an existing stopped legacy resource only in the private catalog and use a synthetic local Unix worker status/list service. Oversized local resize and start reject before new intent/reservations/effects; unavailable status retains legacy fallback, explicit malformed local maximum rejects. Optional heartbeat maxima null/negative/fraction/string/zero/maximum+1/u32 overflow/u64 maximum close the node channel, leave it offline and cannot create expanded admission. All 16 malformed-capability cases observed EOF/Close and no durable intent. Mock maxima/aggregate readiness were awaited explicitly.

**G4 — stopped configure/refusal: passed (static shipped CLI).** Positively stopped legacy host config saves 4096 MiB/2 CPU with retained old config. Missing consent, held worker/agent/lifecycle locks, stale/unrelated control socket, running recovery journal and missing worker ownership evidence refuse without altering config/credential/CA/synthetic disk/snapshot bytes. Socket also refuses update-assets. No CLI start/stop/signaling action was invoked. Active guest-process and unreadable-proc inventory were not independently injected; core tests/source review cover those limits separately.

**G5 — assets, refresh, rollback and public cache: passed for focused transport/file cases.** V1 Alpine remains accepted; v2 explicitly selected Ubuntu includes paired `network-tools.tar.gz`, while opt-out excludes both. Exact inert copied image digest is checked. Missing helper, traversal, duplicate, oversized declaration, unknown version, uppercase/wrong hash, truncation/corruption and local symlink refuse; failed refresh preserves old selection and protected credential/trust/disk/snapshot bytes, removes staging/intent, then valid retry succeeds. Supported static rollback restores retained Alpine selection and preserves protected bytes. No inert asset/archive binary was executed or extracted.

Actual small public asset HTTP verification: prime/cache hit, same-length rewrite, path replacement, deletion, symlink and unsafe chmod invalidate success; restoration works, HEAD succeeds without body, extra/unknown paths deny. Ordered HTTP mutation is not a deterministic queued-cache-hit race, power-loss or rename/fsync fault-injection claim. Core separately owns archive member proof, publication-boundary recovery and full prepared-image serving/download acceptance.

**G8 — narrow ingress: passed offline.** Literal installer/base/Ubuntu/network-tools paths and supported CLI/checksum/manifest match; wildcard-dot variants, traversal, suffixes and unknown paths deny. Independently specified prior five-file expanded regex is recognized. Existing node app/route means prepare proposes only local CLI regex replacement, with no app creation/API/DNS/policy change. Repeat prepare is unchanged; rollback and repeat rollback restore predecessor while retaining unrelated later route/app and protected policies. Python regex and offline planning do not establish live cloudflared/RE2 acceptance. Existing old public curl success remains parent-reported; no public requests were made here.

## G6 — core evidence correlation and cleanup

Read-only core V4 evidence: lane `docs/plans/host-capacity-report.md`, `data/capacity-build/vm-test-v4.log`, `v/capacity-result.json`, `preservation-after-v4.json`, `header-readiness-v4.json`, `download-v4-result.json`. Their hashes are saved in `final-v4-summary.json`; VM result SHA-256 `f4843c3c7d108e8a3f89c81da044665c47216efcae6b75d21160da7c07112700`.

Core reports one V4 real Ubuntu 4096 MiB/2 CPU create/exec/stopped resize/cold persistence test passed (3.75 s), observed nproc 2, MemTotal 4034064 kB and retained marker. Its cleanup records dedicated guest/worker stopped, original VMM preserved and no extra guest. Parent previously independently checked equivalent effective shape/tools/marker. Verifier did not launch/query/stop a guest or independently repeat this VM acceptance. Core V4 report also records full static 8,778,379,912-byte TLS transfer/configure/rollback and prepared-image HEAD/GET; these are not inferred from inert fixture passes. Core strict Clippy remains reported failed on six existing style diagnostics, not a passed verification gate.

All verifier services bound only loopback ephemeral HTTP/TLS ports or owned Unix sockets. Exact owned PID/start ticks/executable/argv/cwd were recorded for gateway/controller/Node children and validated before signaling retained children. HTTPS/JWKS servers closed; mock node/job/local-worker threads joined; all owned subprocesses exited. Final read-only `/proc` scan across every V4 fixture root, including failed attempts, found **zero remaining matching processes**. No runtime worker/node-agent/VMM launched. Retained private fixture configs/assets/disks/catalogs/logs/stale socket files support review; they have no running listeners. Do not reuse their roots. Scope authorization for short roots: `short-root-authorization.json`.

## Fixture failures, historical results and remaining limits

No confirmed product defect remains from the focused V4 runs. Three fixture issues were preserved and corrected:

1. First static invocation stopped before product execution because optional `bundle-pins-v4.json` was absent. Used authoritative v4 bundle build equality plus the exact unchanged seven-file manifest hash instead. `v4-preflight-note.json` records exit 1; final 20-case run passed.
2. First W8 pinned-local assertion compared raw metadata strings: synthetic SQL wrote spaced JSON; normal inventory reconciliation rewrote equivalent compact JSON. Typed metadata, operation rows and zero reservations/effects were unchanged. Narrow retry compares parsed metadata plus complete resource/operation/reservation state; original failed receipt retained.
3. Narrow W8 setup's failed create explicitly targeted enrolled `fixture`, not literal local. Its retained SQLite capabilities prove aggregate consent was still **4096 MiB/2 slots/2 CPU**, while guest maxima were already 16384/16; the requested 2048/4 exceeded aggregate CPU consent and correctly returned 409. The failed root contains zero resources, operations or effect dispatches. It was not a local-placement/W8 defect. Setup awaited only guest-max fields already present in the first heartbeat, before updated aggregate CPU consent arrived. Fixed setup to await aggregate 8192 MiB/8 CPU/4 slots as well. Failed setup receipt `w8-v4-local-caps-setup-failure.json`; final narrow run passed two groups.

Historical pre-freeze/V2/V3 results remain under their original private names, explicitly superseded: `preparation.json`, `static-result.json`, `browser-result.json`, `static-v3-result.json`, `dynamic-v3-result.json`, `supplement-v3-result.json`, original ingress result roots and superseded cleanup receipts. V2 had 18 static cases plus browser/offline checks; V3 had 20 static cases, six dynamic groups, two browser/cache groups and 20 ingress assertions. None is reused as V4 acceptance.

Remaining boundaries: independent deterministic concurrency and publication/power-loss fault injection not run; actual Ubuntu template/provenance/archive/full-size streaming and VM evidence remain core/parent attributed; source reviewer verdict, static shipping/public/physical-host acceptance, chosen remote budgets and rollout are parent/operator work. No benchmark, production readiness, physical remote Ubuntu, migration, sudo, snapshot/fork or public acceptance claim follows from this report.

## Handoff

Verifier work is complete for the assigned focused no-VM scope. Parent reviews this report together with stable source review and core's actual V4 evidence before rollout. Private final index: `data/host-capacity-verify/final-v4-summary.json`. Any compiled source change requires new artifact attribution; do not use superseded receipts. No owner decision or permission is pending. Only this report and private fixtures/receipts were written; task board and product lane remain untouched by verifier.

## Completed bounded pinned-local positive gate

Command: `python3 data/host-capacity-verify/w8-v4-local-positive-focused.py` — **exit 0; one targeted group passed**. Source/static/dynamic manifests matched before/after. Private receipt `data/host-capacity-verify/w8-v4-local-positive-result.json`; root `/home/sebassdc/dev/open-workspaces/data/cv/d21po7vky`; runner SHA-256 `0ec26af7643a803375bddbb33c464840d478583918540b4a846618aba8b96a66`.

An existing owned stopped local resource is modeled only in the private catalog and returned by the actual synthetic local worker list. Actual signed gateway resize **2048 MiB/2 CPU** succeeded, left it stopped with zero running RAM/CPU reservation, and added exactly one durable operation and one mock resize effect. Subsequent **4096 MiB/2 CPU** and **2048 MiB/8 CPU** requests both rejected before new durable operation/resource/reservation/effect dispatch. Parsed semantic metadata and the stopped mock machine stayed unchanged; timestamps/JSON formatting were excluded from the comparison. No worker binary or guest ran. This closes the positive local resize gate without relying on local automatic-create policy.

As parent clarified, automatic create excludes literal local while any enrolled-node inventory exists, including revoked tombstones; this is existing placement policy, not a W8 defect. The local-positive fixture uses a seeded existing owned resource rather than expecting automatic local create in an enrolled catalog. Exact earlier setup failure evidence was inspected in `w8-v4-local-caps-setup-failure.json`: targeted enrolled fixture, retained 2 CPU capability, zero intents/reservations/effects.

Cleanup: all owned gateway/controller children exited, TLS/JWKS server closed, local/mock/job threads joined; final owned-root `/proc` scan found zero processes. Private artifacts remain; final summary now indexes this additional receipt/root. No product/owner/cloud changes.
