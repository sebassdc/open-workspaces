# Compressed Ubuntu assets — source/evidence review

Updated: 2026-10-04 | Role: read-only reviewer | State: stable source/evidence cleared; parent publication pending

## Final verdict and stable attribution

**Cleared for parent merge/publication review:**
`62f73513bb5d9ffaae93addae3161f0eb524d3c1` (`Implement bounded compressed Ubuntu
asset delivery`). C1–C3 and the baseline directory-sync finding are resolved.
No remaining blocking source/evidence finding within the bounded brief. This
clearance supersedes the provisional checkpoints below; it does not assert public
delivery, deployment completion, whole-process RSS measurement or production
readiness.

Reviewer inspected **committed blobs**, not merely current files: all 32 compiled
source-map entries match the frozen release build record, and all 10 recorded
script/document entries match that commit. Independently rehashed all 37 entries
in the final-checks artifact/evidence map, including full raw/encoded image bytes;
zero mismatches or missing files. Read ELF headers directly: no INTERP and no
NEEDED entries. Reviewer ran no tests/builds/VMs/services/cloud actions and changed
only this report.

Artifacts below are relative to
`/home/sebassdc/dev/open-workspaces-lanes/compressed-assets/data/compressed-assets-build/`:

| Accepted evidence/artifact | SHA-256 / value |
| --- | --- |
| `release-reviewed/ow-linux-amd64` and matching bundled CLI | `7b07c2d888022c9d1fa250962661db568a1d880aeba26dfced118efd114d8251` |
| Build source-map SHA | `10411b11fb767efa024dea787e5c35f62a4a29c26b4cb044e29d7e0d90b1227f` |
| Committed `crates/ow/src/onboarding.rs` SHA | `3dea64f320a9c54cf07c1c5efd1296bc5c88b009eac899d7a8868a3b90f19d10` |
| `release-reviewed/build.json` | `ffdcdedadf78019cce1661bb28fb2f4fc566928cfc44769d8ddf62ab4fcb8b40` |
| `bundle-reviewed/manifest.json` | `89f78296c02093e91dc82bfef564381bb74f6ccaca339d1f4a1332178dce95f1` |
| `bundle-reviewed/compressed-manifest.json` | `766d83d133515ac981dc9667cf89a255d0f7cc904b750d9f225dc8135dcaf1ee` |
| `bundle-reviewed/ubuntu.ext4.zst` (442,888,634 bytes) | `53a6d38cf7b0d3fed054d0a4c21c47a75187422aadae06b47848949e6346c405` |
| `bundle-reviewed/ubuntu.ext4` (8,589,934,592 bytes) | `87ba61583f33e129f08776d1a4b8c1557322f146a7de0ed44581e2000f3f3f6b` |
| `tls-reviewed/result.json` (21 synthetic cases) | `f142138a8c6758fb174b66e2cf1bf1b4816b9a0d7892d70c755b955f417dc2bb` |
| `tls-pristine/result.json` (actual full static-CLI TLS decode) | `3d1bb0ec31f7b05e72c27530fab394179bfc9354b9f1c887d8df60e272b39346` |
| `tls-pristine/real.decoded.json` | `1f6049dda28aa105f737c8b3a5b549b0e2dfd0327af8ca007a39526ad6056ae2` |
| Final `final-checks.json` including stable commit/tree | `493d4551f6db6a72b52af9b19b23ade4a9c62fb17de9ae917af12167335ddb8b` |

Stable commit tree: `ee92913163dc123457c4d6935ef49aa3d388370f`.
Lane status was clean after the scoped commit. This status is supporting scope
evidence; clearance rests on committed-blob hashes and the artifact/receipt
reconciliation above.

Build receipt pins Rust `1.97.0 (2d8144b78 2026-07-07)`, Zig `0.15.2`, musl
`x86_64-unknown-linux-musl`, release profile. Build script checks source inputs
before/after compilation; stable committed inputs reconcile with that source map.
Builder's focused native onboarding log records **15 passed, zero failed** and is
pinned `22aef6321dcfb13dd8080000958ca2c588487300943b7f24aa2d83ff59287bd6`.
Both accepted TLS receipts bind the exact static CLI above. Reviewed test source
hashes the actual published expanded files before recording decoded identity;
the real-image receipt records 8 GiB logical and **1,867,968,512 allocated bytes**.
This establishes original-byte identity through the built-in decoder and sparse
verified selection. No further VM is required by the review brief.

### Fix reconciliation

- **C1 resolved:** committed `onboarding.rs:2056`–2066 returns absent capability
  only for NotFound; 2135–2144 distinguishes 404 from all other failure paths.
  Native handler test 2177–2231 covers raw v2/v1 missing metadata 404, preserved
  raw routes 200, malformed and dangling-symlink metadata 503. Client still
  falls back only on 404; TLS 403/302/503 and malformed JSON cases assert no
  raw fallback.
- **C2 resolved:** `public_download_from` receives its directory explicitly
  (2039); tests call it directly without environment mutation. Replacement
  reviewed native log passes. Earlier failed compilation and source-changing
  build are retained as failed attempts, not accepted evidence.
- **C3 resolved:** committed compressed preparer 86–95 checks staged raw
  lengths/hashes, byte-identical raw manifest, encoded length/hash and matching
  CLI hash before syncing and renaming. Header/window preflight and private
  immutable output remain. Raw preparer now syncs its populated directory too.

### Release boundary / handoff

Parent selects exactly `bundle-reviewed/` and its matching Linux CLI/checksum,
retains old raw/macOS delivery and prior immutable assets, and verifies public
manifest/blob lengths/hashes, route headers and preserved owner runtime identities
after publication. No controller/agent/worker/guest upgrade, cloud API change or
VM rerun is required by this clearance. Existing decoder allocation cap is a
64 MiB window plus fixed buffers/context, not a measured whole-CLI RSS cap.
Native handler/cache evidence and actual TLS client evidence are complementary;
neither is being called public gateway acceptance.

## Historical checkpoints (superseded by final verdict)

## Scope and attribution

Review contract: `docs/plans/compressed-assets-review-brief.md` and
`docs/plans/compressed-assets-brief.md`. Product lane:
`/home/sebassdc/dev/open-workspaces-lanes/compressed-assets`, branch
`work/compressed-assets`. Reviewer writes only this root report; no tests, builds,
VM/service/cloud actions, product edits or commits.

Initial inspected baseline: `b6e2475931c4e5a7d94e4a7b4cc9e29eb0209383`;
lane was clean and compressed implementation was not yet present. This establishes
the baseline only, not clearance of the proposed implementation. Source line
references below are relative to that lane and this baseline unless superseded.

## Early findings sent to builder

1. **Publication durability gap in existing bundle preparer.**
   `scripts/prepare-host-bundle.py:107`–115 syncs files and the output parent,
   but does not sync the populated bundle directory before renaming it.
   Sync that directory in the scoped compressed bundle publication path so its
   entries are durable before parent-directory publication. This is a baseline
   issue, not yet an observed regression from compressed delivery.
2. **Expanded-byte disk admission required.**
   `crates/ow/src/onboarding.rs:623`–627 and 773–776 currently reserve transfer
   bytes plus the selected reserve and 512 MiB. Replacing raw transfer size with
   encoded size would undercount staging the expanded disk. Reserve expanded
   logical bytes for streaming decode, and encoded staging too if retained by the
   client. Existing retained assets already reduce measured free space; do not
   delete them to satisfy admission. This is an implementation checkpoint, not a
   finding against source that has not arrived.

## Baseline contracts to preserve

- `onboarding.rs:507`–555: exact v1/v2 fixed-name raw manifests, bounded sizes,
  hashes and required Ubuntu/network-tools pairing.
- `onboarding.rs:814`–831: sparse zero writes, exact length/hash, file sync.
- `onboarding.rs:1316` onward: verified staged tree, nested-directory sync,
  journaled asset rename and rollback. Compressed transport must emit the same
  local canonical expanded manifest and select verified expanded files.
- `onboarding.rs:1803`–1879: fresh opened FD with bounded digest/stat cache,
  queued identity recheck, post-hash identity recheck and rewind. Extend this
  verification to encoded serving without replacing it with path-only trust.
- `scripts/prepare-host-bundle.py:68`–75: independently pinned pristine Ubuntu
  developer-v1 bytes/provenance; no arbitrary workspace or snapshot input.
- Raw manifest and raw image routes must remain available; compressed negotiation
  may fall back only on HTTP 404. New gateway must return that status when its
  selected legacy bundle has no compressed manifest.

## Evidence received so far

Builder's ignored `data/compressed-assets/benchmark.json` and `benchmark.py`
describe a 268,435,456-byte sampled corpus, single runs, nice 10, two-CPU affinity
and 2 GiB address-space limit. Results compare zstd levels 9/15/19 and xz level 6.
This is sampled/tool-decode evidence only. It does not establish full-image byte
identity, bounded embedded Rust decoding, static CLI acceptance, or release pins.
No compression ratio or timing is generalized to all images or hosts.

## Remaining clearance evidence

### Source-arrival checkpoint (provisional)

Reviewed current uncommitted diff against the baseline, including compressed
wrapper/decode/download/public routes, codec lock, ingress and arriving preparer
and TLS test source. These are checkpoint pins, **not a stable release clearance**:

| File | SHA-256 inspected |
| --- | --- |
| `crates/ow/src/onboarding.rs` | `9129da2621a48cdd4a5cd9eb6951a1732b22235b08bf21346c9c4a461f9b275e` |
| `scripts/prepare-host-ingress.py` | `8a67d6f4d8f07a2939625a7141b7ea4cd21ce90ce89d5b151cb53406417752bf` |
| `scripts/prepare-compressed-host-bundle.py` | `b1db76cfaeeff345efdf66109ae1c1080bdebcdbc1de2def616f0613729937a2` |
| `experiments/compressed-assets/tls-delivery.py` | `634266920a93a6cb0c076b90238839fd753c173a581570a814e3c0d917caefb3` |

Findings sent directly to both compressed-assets and ow-planner:

- **C1 — legacy raw-only bundle fails on upgraded gateway (blocking compatibility).**
  `onboarding.rs:2053` reads a nonexistent compressed manifest as an error;
  2121–2125 turns every error into HTTP 503. New downloader 846–859 falls back
  only on 404. Thus valid retained v1/v2 raw-only bundles cannot serve a new CLI,
  including an Alpine-only request. Return 404 specifically for absent compressed
  capability; malformed, corrupt or inaccessible metadata must remain failures.
  Exercise the actual gateway handler with raw-only v1/v2 bundles. The Python TLS
  fixture synthesizes 404 and therefore does not cover this server regression.
- **C2 — new gateway unit test does not compile (acceptance blocker).**
  Builder's `data/compressed-assets/tests-final.log` records E0133 at
  `onboarding.rs:2176`, 2206 and 2208 for process-global `set_var`/`remove_var`.
  Use a directory-taking serving helper or process isolation to avoid global
  environment mutation in concurrent tests; merely wrapping these calls in unsafe
  blocks does not establish safe isolation. Successful replacement test evidence
  must supersede this failed log.
- **C3 — compressed preparer publishes copies without revalidation (publication
  integrity gap).** `scripts/prepare-compressed-host-bundle.py:41`–58 hashes inputs,
  then 63–72 copies them and 79 publishes without checking staged raw, encoded,
  CLI and raw-manifest bytes against the selected pins. Revalidate staged bytes
  before rename, as the raw preparer already does, so source changes or incomplete
  copies cannot create a supposedly verified immutable bundle. External full TLS
  decoder acceptance remains a separate gate.

Positive source observations, without execution claims:

- `onboarding.rs:80`–97 validates exact compressed name/codec, bounded encoded
  size/hash and a valid canonical v2 Ubuntu manifest. Downloader validates both
  encoded length/hash (941–969) and expanded length/hash (139–157); writes sparse
  expanded files; and serializes only the canonical manifest locally.
- Header preflight 105–127 rejects skippable magic, dictionary flags and windows
  above 64 MiB, including single-segment content-size windows. Decoder sets
  `window_log_max(26)` before reads, uses fixed buffers, stops after one frame,
  and rejects unread trailing bytes. Inspected pinned `zstd-0.13.3` read/zio
  source: it consumes only processed input and retains the unread buffered tail,
  so `finish().fill_buf()` does cover buffered concatenated/trailing data.
  Header interpretation checked against the primary
  [Zstandard format specification](https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md).
  No additional decoder defect found in this checkpoint; actual memory evidence
  and hostile-stream executable evidence still need final attribution.
- Admission 899–903 now accounts expanded bytes **plus encoded staging** and
  preserves existing reserve/512 MiB margin. Retained rollback assets remain
  represented by measured free space. Previous disk checkpoint is addressed.
- Encoded serving uses existing `verified_public_file` cache on the same opened
  FD (2097–2098). Raw paths remain intact; compressed/raw canonical manifests are
  checked for equality. Ingress adds only the two exact compressed routes while
  recognizing and restoring prior raw-only ingress.
- Raw preparer 110–114 now syncs the populated bundle directory before rename;
  the new compressed preparer also syncs files, directory and output parent.
  Initial directory durability finding is addressed in current source.
- Codec is exactly `zstd=0.13.3`, default features disabled, with locked
  `zstd-safe=7.3.0` and `zstd-sys=2.1.0+zstd.1.5.7`. Cached package metadata lists
  MIT/BSD-3-Clause licenses. Reqwest has no automatic content-decoding features;
  compressed image delivery is an application asset, not HTTP gzip.

Inspected preliminary TLS evidence: `data/compressed-assets-build/tls-synthetic/result.json`
lists 18 successful assertions using CLI digest
`75b7d9506b85dc6465dd9a2f22c93d64f61655bbc864a856d38a6db2ecc3eb5e`.
Test source recalculates encoded hashes for malformed frame variants, allowing
decoder checks to be reached rather than failing only transport SHA. It checks
retained old selection, cleanup, sentinel disk and credential preservation,
canonical manifest equality, sparse expanded files and no fallback on 503.
However, its build metadata records onboarding source digest
`35f19393337cb6d6f6df4f71107f0c8e2d1667dfeb8d915bbc0b5e561779d277`,
which differs from the current checkpoint. These passing cases cannot clear the
later source. Full compression log was still empty when inspected; full actual
Rust TLS expanded-byte identity and stable release artifacts remain pending.

Review the arriving decoder/manifest/downloader/routes/scripts and exact allowlists.
Require encoded and expanded SHA/length bounds; single frame; dictionary,
skippable, trailing and concatenated rejection; window and memory cap before
allocation; sparse expanded output; reserve, sync and rollback preservation.
Inspect actual TLS acceptance evidence for success and malformed streams,
v1/v2 compatibility and 404-only fallback, without running reviewer tests.

Final attribution must record a stable source commit; static build/source/toolchain
and CLI pins; test source and corresponding executable/log pins; immutable
bundle/manifest/encoded artifact pins; and the actual full expanded byte digest
`87ba61583f33e129f08776d1a4b8c1557322f146a7de0ed44581e2000f3f3f6b`
for 8,589,934,592 pristine bytes. Sample matching or unit-only decoding is
insufficient. A further VM is unnecessary if actual original-byte identity and
sparse verified decoder integration are established. Parent owns merge/publication
and public route/preservation verification.

## Historical next step (now completed)

Corrections, replacement evidence and committed-blob reconciliation are now
complete; see the final verdict at the top. Parent publication is the next step.
