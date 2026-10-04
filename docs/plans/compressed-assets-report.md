# Compressed Ubuntu delivery — implementation report
Updated: 2026-10-04 | Size: M | State: lane implementation and local acceptance complete; parent review/publication pending
Pattern: ADR 0002 gateway + bounded verified staging. Owner authorization supplied in the parent brief; no additional go was requested.

## Result and release scope

Ubuntu shrinks from 8,589,934,592 logical bytes to 442,888,634 encoded bytes (94.84% reduction); complete bundle transfer is 631,333,954 instead of 8,778,379,912 bytes. The actual static CLI decoded and published a sparse 8 GiB file with the original canonical SHA-256. No VM was started. No service/controller/tunnel/worker/owner/remote/cloud configuration or accepted release was changed. All outputs stay in ignored dedicated lane storage. These are local single-run observations, not public-network timings, physical-host acceptance or production-readiness claims.

## Measured codec selection

Host: AMD Ryzen 7 3700X, x86-64; Linux 7.2.5-3-omarchy; 32,761,344 KiB total RAM; Btrfs. Installed zstd **1.5.7**, xz/liblzma **5.8.3**. Benchmarks used nice **10**, affinity to **two CPUs**, inherited RLIMIT_AS **2 GiB**, zstd memory flag **1024 MB**, xz memlimit **1536 MiB** and **two codec threads**. No guests. Sample count **one per configuration**; no p50/p95. Cache state was not controlled. The 268,435,456-byte corpus is the first 128 MiB plus sixteen evenly spaced 8 MiB chunks, including zeros and populated ext4 ranges. Its SHA-256 is `3e0428f0bfbd86f3ba34845830bf90f1154fb0bd75636fe77493a688268ab0a9`; it is a bounded proxy, not a complete-image codec comparison.

| Codec/level | Encoded bytes | Encode seconds | Decode to null seconds |
|---|---:|---:|---:|
| zstd 9 | 25,937,171 | 0.955 | 0.113 |
| zstd 15 | 25,552,309 | 4.786 | 0.099 |
| zstd 19 | 23,313,573 | 14.535 | 0.120 |
| xz 6 | 21,931,064 | 17.106 | 0.563 |

Selected **zstd 19**: about 9% smaller than zstd 15 in this corpus; xz saves another 5.9% but took about 4.7 times the decode time. Preparation happens once; client decode matters for each installation. This is a reasonable measured tradeoff, not a universal best-codec claim. Only the selected codec was run on the complete pristine image.

Full pristine original SHA check took **7.957 s**; encode **435.393 s**; decode plus hashing **12.937 s**. The last timer includes decoded-byte SHA, then encoded-file SHA overhead (the receipt field is named `decode_hash_seconds`); it is not pure decoder latency. Actual static CLI TLS update took **33.226 s**, including transfers, encoded verification, built-in decode/hash, sparse writes, staged verification and publication. Fixture post-publication rehash is outside that CLI duration. Output allocated **1,867,968,512 bytes**, logical **8,589,934,592**. No new full compression was run for preparation; the measured `.zst` was reused and rehashed after copying.

Raw detailed receipts: `data/compressed-assets/benchmark.json`, `full.json`, `full.log`; `data/compressed-assets-build/tls-pristine/result.json` and `real.decoded.json`. Initial executable measurement commands were `python data/compressed-assets/benchmark.py` and `python data/compressed-assets/full.py`; these ignored scripts remain beside the receipts. Parameterized equivalents are checked in under `experiments/compressed-assets/benchmark.py` and `full-image.py`.

## Product contract

- Old raw endpoint, all old fixed file routes and canonical local v1/v2 formats remain. New endpoint is exactly `/cli/host-compressed-manifest.json`; encoded blob is exactly `/cli/host/ubuntu.ext4.zst`.
- Transport wrapper: `{version:1, manifest:<canonical v2>, ubuntu:{name:"ubuntu.ext4.zst",codec:"zstd",size:<encoded>,sha256:<encoded>}}`. Both encoded and expanded lengths/SHA are mandatory. Unknown fields/codec/name, duplicate/missing/unknown assets or invalid lengths/hashes fail closed.
- Negotiation falls back **only HTTP 404**. New gateway with genuine missing compressed metadata returns 404; corrupt/mismatched/unsafe metadata, unavailable assets or other errors return 503. The actual directory-taking gateway handler is tested against raw v1/v2 bundles; async tests do not mutate process-global environment.
- Encoded bytes spool into private exclusive staging and are verified before decoding. Decoder accepts one ordinary dictionary-free frame, enforces header window and native `window_log_max(26)`, including single-segment content-size windows, and checks buffered tail at EOF. Dictionaries (even advertised ID zero), skippable frames, concatenation and trailing bytes are refused. There are no shell codecs on the installed client, no archive extraction or manifest-controlled destination paths.
- Decoder storage is bounded by the 64 MiB window, codec fixed streaming buffers and two 64 KiB Rust buffers; output cannot exceed the canonical asset size (Ubuntu maximum 8 GiB). This is a decoder allocation bound, not a whole-CLI RSS claim. Sparse zero writes and original hash verification precede canonical local manifest publication. Expanded + encoded logical staging is included in admission; retained assets already consume available space. Atomic publication/recovery and stopped-host checks are reused.
- Progress totals and rate use encoded transfer bytes; decode/verify is an explicit stderr phase. Existing stdout selection reports the actual transfer size. 100% transfer is not completion.
- One direct codec dependency: exact `zstd = 0.13.3`, default features disabled. Lock pins `zstd-safe 7.3.0`, `zstd-sys 2.1.0+zstd.1.5.7` and checksums. Registry manifests declare MIT for zstd and BSD-3-Clause for safe/sys; no AGPL code. Native zstd is bundled statically. API behavior is supported by [pinned decoder documentation](https://docs.rs/zstd/0.13.3/zstd/stream/read/struct.Decoder.html) and the cached pinned codec source.

## Acceptance and honest failures

**15 native GNU onboarding tests passed**, 35 filtered out; focused run **1.05 s** (`data/compressed-assets/tests-reviewed.log`). Includes actual raw-v1/v2 missing-capability 404, existing malformed and dangling-symlink metadata 503, raw route 200, transport/raw mismatch 503, route bounds, sparse decode and stream rejection; existing persistence/idempotent recovery/asset publication/cache tests remained passing.

**21 actual reviewed static CLI synthetic TLS cases passed** (`data/compressed-assets-build/tls-reviewed/result.json`): live PTY, non-TTY, encoded corrupt/truncated/oversized, decoded oversized/hash, excessive window, dictionary, skippable, trailing, concatenated, path, codec, malformed manifest, HTTP 503/403/302, malformed JSON, raw v1/v2 404 fallback. Decoder-invalid fixtures recompute encoded metadata so verification reaches the decoder. Failures retain old canonical manifest and disks/credentials, leave no staging or rollback; successes verify sparse output, exact local manifest and one rollback. Fallback request counts prove every non-404 failure made zero raw-manifest requests. PTY progress arrives while the process runs and before transfer completion, with width/ANSI checks. Explicit private CA and proper CA-signed localhost leaf; no insecure TLS.

**Actual pristine TLS/static built-in full decode passed**, exit 0, original SHA and 8 GiB size preserved. The full real bundle was served by the test TLS fixture from files in chunks; it was not loaded into fixture RAM. This TLS fixture covers the client; gateway serving is separately covered by native handler/cache tests. Public delivery is still parent-owned.

**Offline ingress/CLI source checks passed**: six cancellation cases, cleared-HOME explicit root, two owner output denials, ten offline ingress denial/repeat/compensation checks. Exact new paths match; extra suffix, traversal, similar names and unrelated routes do not. No cloud API was called.

`cargo fmt --check`, Python compilation and `git diff --check` passed. Reviewed ELF has **no INTERP and no NEEDED entries**. Input source hashes and copied bundle artifact/manifest hashes are verified in `data/compressed-assets-build/final-checks.json`.

Two intermediate attempts are retained honestly: the first new async test failed compilation on unsafe process-global `set_var/remove_var` (not a passing receipt), then was replaced with the directory-taking helper; a rebuild concurrent with that correction refused publication with `Source changed during build`. Final tests/build used stable source. No musl debug SQLite UBSAN attempt or toolchain repair was made; focused tests used the native GNU target.

## Frozen paths and pins

All paths below are relative to **`/home/sebassdc/dev/open-workspaces-lanes/compressed-assets`**. Build root is owned 0700 with the existing required marker; prior outputs were not overwritten.

| Artifact | Path | SHA-256 |
|---|---|---|
| Static Linux CLI | `data/compressed-assets-build/release-reviewed/ow-linux-amd64` | `7b07c2d888022c9d1fa250962661db568a1d880aeba26dfced118efd114d8251` |
| Build source map | `release-reviewed/build.json` field `source_sha256` | `10411b11fb767efa024dea787e5c35f62a4a29c26b4cb044e29d7e0d90b1227f` |
| Full build receipt | `data/compressed-assets-build/release-reviewed/build.json` | `ffdcdedadf78019cce1661bb28fb2f4fc566928cfc44769d8ddf62ab4fcb8b40` |
| Raw canonical manifest | `data/compressed-assets-build/bundle-reviewed/manifest.json` | `89f78296c02093e91dc82bfef564381bb74f6ccaca339d1f4a1332178dce95f1` |
| Compressed transport manifest | `data/compressed-assets-build/bundle-reviewed/compressed-manifest.json` | `766d83d133515ac981dc9667cf89a255d0f7cc904b750d9f225dc8135dcaf1ee` |
| Encoded Ubuntu | `data/compressed-assets-build/bundle-reviewed/ubuntu.ext4.zst` | `53a6d38cf7b0d3fed054d0a4c21c47a75187422aadae06b47848949e6346c405` |
| Original Ubuntu | `data/compressed-assets-build/bundle-reviewed/ubuntu.ext4` | `87ba61583f33e129f08776d1a4b8c1557322f146a7de0ed44581e2000f3f3f6b` |
| Preserved runtime guest | `data/compressed-assets-build/bundle-reviewed/ow-guest` | `1e5d3b4bc924755215a56531e36b26ffb3f0632ad270907d73f3aa9605642cf7` |

The flat immutable prepared bundle is **`data/compressed-assets-build/bundle-reviewed/`**, including all original raw blobs plus the encoded blob, both manifests and matching CLI/checksum/build receipt. Old raw manifest bytes are exactly retained. The unchanged guest equals the new build pair; no worker/guest upgrade is requested. File-level pins, tool versions and script/evidence digests are in the final checks receipt. Keep all accepted old raw bundles/CLI files available for old clients and rollback.

## Exact successful commands

Working directory: `/home/sebassdc/dev/open-workspaces-lanes/compressed-assets`.

```sh
export CARGO_HOME=/home/sebassdc/dev/open-workspaces/data/runtime-spike/cargo
export RUSTUP_HOME=/home/sebassdc/dev/open-workspaces/data/runtime-spike/rustup
"$CARGO_HOME/bin/cargo" +1.97.0 test --locked -p ow onboarding::tests -- --test-threads=1
export OW_HOST_BUILD_ROOT="$PWD/data/compressed-assets-build"
export CARGO_TARGET_DIR="$PWD/data/compressed-assets-build/target"
export OW_CROSS_BUILD_ROOT=/home/sebassdc/dev/open-workspaces/data/macos-build
export OW_HOST_BUILD_OUTPUT="$PWD/data/compressed-assets-build/release-reviewed"
scripts/build-host-cli.sh
python scripts/prepare-compressed-host-bundle.py \
  --raw-bundle /home/sebassdc/dev/open-workspaces/data/host-verify/capacity-release-1791097374223248458/published/host-bundle \
  --encoded-ubuntu "$PWD/data/compressed-assets/ubuntu.ext4.zst" \
  --linux-cli "$PWD/data/compressed-assets-build/release-reviewed/ow-linux-amd64" \
  --output "$PWD/data/compressed-assets-build/bundle-reviewed" \
  --build-root "$PWD/data/compressed-assets-build"
python experiments/compressed-assets/tls-delivery.py \
  --binary data/compressed-assets-build/release-reviewed/ow-linux-amd64 \
  --evidence data/compressed-assets-build/tls-reviewed
nice -n 10 taskset -c 0,1 python experiments/compressed-assets/tls-delivery.py \
  --binary data/compressed-assets-build/release-reviewed/ow-linux-amd64 \
  --evidence data/compressed-assets-build/tls-pristine \
  --real-bundle data/compressed-assets-build/bundle-reviewed
python experiments/host-onboarding/source-checks.py \
  --binary data/compressed-assets-build/release-reviewed/ow-linux-amd64
```

Build uses Rust **rustc 1.97.0 (2d8144b78 2026-07-07)**, Zig **0.15.2**, two jobs, release profile, musl target. Inactive assets-progress target cache was copied using `cp -a --reflink=auto` into this lane's dedicated target; accepted releases were untouched. Successful logs: `build-reviewed.log`, `prepare-reviewed.log`, `tls-reviewed.log`, `tls-pristine.log`, `data/compressed-assets/source-checks-reviewed.log`. Reproduction must choose fresh output/evidence directories; rerunning the exact output names intentionally fails rather than replacing artifacts. Bounded measurement scripts accept `--image` and `--evidence` for fresh reproduction.

## Parent next step / rollback

Review the stable scoped commit against final pins, then parent selects this bundle and matching CLI under the existing gateway contract. Add only the two exact compressed URLs to the existing CLI ingress route; `RAW_HOST_CLI_REGEX` preserves recognition/rollback for the prior route. No new Access app, cloud API, DNS, SSL, service/controller/worker/owner change is authorized to this lane. CLI bin selection is separate from `OW_HOST_BUNDLE_DIR`; match served CLI/checksum to the accepted build, retain macOS files and all previous raw routes/bundles. Verify public manifest/blob lengths/hashes and old-client raw paths after parent publication. Rollback selects prior gateway/binary/bundle and prior exact CLI ingress while retaining new immutable files; do not restore catalogs or stop unrelated workers/guests.
