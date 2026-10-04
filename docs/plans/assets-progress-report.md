# Asset download progress — execution report
Updated: 2026-10-04 | Size: S | Branch: work/assets-progress

## Scope and behavior

Presentation-only changes in `crates/ow/src/onboarding.rs`, downloader UX docs
in `docs/HOSTS.md`, and one focused executed CLI/TLS harness in
`experiments/assets-progress/tls-progress.py`. Base commit:
`606cb47a6a3634d1ab36a7dcb16cec65c157f135`. The scoped commit includes this
report and `docs/plans/assets-progress-handoff.md`; resolve its final ID with
`git rev-parse work/assets-progress` in this lane.

Pattern: existing bounded reqwest streaming downloader and verified staged asset
publication. Aggregate progress goes exclusively to stderr, with a 200 ms
streaming refresh interval, forced file-boundary updates, percent, received/total
MiB or GiB, current filename and average MiB/s since transfer start. A terminal
gets a cleared single row, cropped to its reported width. Redirected stderr gets
start/file/verification/publication/end lines without CR or ANSI escapes.
No dependencies added. Existing stdout is byte-for-byte unchanged.

100% is explicitly download completion. Per-file streaming hashes/length checks,
fsync, full staged verification and publication still precede the success line.
Errors after progress starts close the display with failure rather than success;
earlier manifest/preflight errors retain the CLI's normal error output.
TLS trust, redirect refusal, response bounds, sparse writes, size/hash checks,
consent, reserve, deadlines, stopped-host and publication checks are unchanged.
The shared join downloader also receives this presentation naturally.

No VM boot, host/service changes, owner-root, remote/controller/cloud changes,
public bundle preparation, public asset selection or publication occurred.
Synthetic runtime file bytes are integrity fixtures, not bootable VM evidence.

## Static source/build/artifact pins

All paths below are under:
`/home/sebassdc/dev/open-workspaces-lanes/assets-progress/`.

| Item | Path or pin |
| --- | --- |
| Static CLI | `data/assets-progress-build/release/ow-linux-amd64` |
| CLI SHA-256 | `23641ccb9f64980a05a9e5169792046e80d0982c699730f805203ee0ec2f4e6b` |
| Build receipt | `data/assets-progress-build/release/build.json` |
| Receipt SHA-256 | `67549925da08125ee15800054d8e533b728ff0ba42b63b04b88e4632454e68bc` |
| Build source manifest SHA-256 | `f3a5b89316d69f9ebe6fafa7c50634acd430236c1e801bc4140fda2d8145376a` |
| Downloader source SHA-256 | `7df5a2ba5c32ab7f83db7a0256e2fbf2f04c58332ff20bf7e4ce65af11c1fe08` |
| Cargo.lock SHA-256 | `ce5336623d4cc88271140d804b53fce9c0944bd07d323462aee7807b4bc9eb11` |
| Unchanged build script SHA-256 | `0f2ac30bbf52843e3f9af727a65c9978d53e44d24c5abeca694e5a209644a752` |
| Compiler | `rustc 1.97.0 (2d8144b78 2026-07-07)` |
| Target/profile | `x86_64-unknown-linux-musl`, release, locked dependencies |
| Zig | `0.15.2`; archive SHA-256 `02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239` |
| Harness SHA-256 | `ef3ecd9f88aab09ecfb30019309a7a4c422b24c1819e2f671f8a1ed1745b519b` |
| Test receipt | `data/assets-progress-build/tls-evidence-final/result.json` |
| Test receipt SHA-256 | `18f092ad43994441886d8c4933f561ea69610b22f69e8a642b24dd429ac11555` |

`build.json` records SHA-256 for every compiled Rust/manifest/UI/CLI input and
checks source stability during the build. Its source hash is the SHA-256 of the
script's sorted JSON source manifest, not a Git tree hash. Docs/harness are not
part of that compiled-source manifest; the harness is separately pinned above.
The standard script also stages `data/assets-progress-build/release/ow-guest`,
SHA-256 `1e5d3b4bc924755215a56531e36b26ffb3f0632ad270907d73f3aa9605642cf7`;
it is a build-script output, not a requested runtime-bundle update.

`file` reports an x86-64 static PIE ELF. `readelf -l` has no INTERP entry and
`readelf -d` has no NEEDED entries. The CLI actually ran in all five cases below.
Outputs remain private and ignored. Never commit the generated TLS keys or
fixtures. Parent reviews/merges the scoped commit and handles public CLI
publication via gateway asset selection.

## Exact commands and evidence

Run from this lane. Existing pinned tools were read from main's dedicated
storage; target and output were isolated inside this lane's owned 0700 build root.

```bash
export CARGO_HOME=/home/sebassdc/dev/open-workspaces/data/runtime-spike/cargo
export RUSTUP_HOME=/home/sebassdc/dev/open-workspaces/data/runtime-spike/rustup
export OW_HOST_BUILD_ROOT="$PWD/data/assets-progress-build"
export CARGO_TARGET_DIR="$OW_HOST_BUILD_ROOT/target"
export OW_CROSS_BUILD_ROOT=/home/sebassdc/dev/open-workspaces/data/macos-build
export OW_HOST_BUILD_OUTPUT="$OW_HOST_BUILD_ROOT/release"
# Root created mode 0700; .ow-host-build contains this exact line plus newline:
# open-workspaces dedicated host build v1
bash scripts/build-host-cli.sh > data/assets-progress-build/build-final.log 2>&1

export OW_ZIG_CACHE_DIR="$CARGO_TARGET_DIR/zig-cache"
export CC_x86_64_unknown_linux_musl="$PWD/scripts/musl-cc.sh"
"$CARGO_HOME/bin/cargo" +1.97.0 test --locked -p ow \
  --target x86_64-unknown-linux-musl onboarding::tests -- --test-threads=1 \
  > data/assets-progress-build/tests.log 2>&1

python3 experiments/assets-progress/tls-progress.py \
  --binary data/assets-progress-build/release/ow-linux-amd64 \
  --evidence data/assets-progress-build/tls-evidence-final \
  > data/assets-progress-build/tls-final.log 2>&1

"$CARGO_HOME/bin/cargo" +1.97.0 fmt --all -- --check
python3 -m py_compile experiments/assets-progress/tls-progress.py
git diff --check
file data/assets-progress-build/release/ow-linux-amd64
readelf -l data/assets-progress-build/release/ow-linux-amd64
readelf -d data/assets-progress-build/release/ow-linux-amd64
```

Build exit 0, 25.57 seconds in the final incremental build; build script atomically
staged the private pair after confirming frozen inputs. The initial build
(`build.log`) refused publication because the parent UX review MiB/GiB source edit
occurred during compilation; no output from that attempt was selected.

TLS harness exit 0. Actual command per test, with a fresh short owned scratch root:

```bash
unshare --user --map-current-user --pid --fork --mount-proc \
  /home/sebassdc/dev/open-workspaces-lanes/assets-progress/data/assets-progress-build/release/ow-linux-amd64 \
  --local --data-dir /tmp/ow-ap-<fresh-suffix> host update-assets --ubuntu-dev
```

A private user/PID/mount namespace isolates `/proc` inventory from unrelated
same-user desktop processes (including the known 1Password inventory issue),
without changing or weakening product inventory checks. Existing host namespaces/networking are not modified and no host root authority
is used; loopback networking is shared only for the bounded test HTTPS server. The server only serves synthetic
manifest/file responses, never enrollment/control routes.

Each success downloads seven files totaling 2,588,672 bytes, including a 1 MiB
zero-filled synthetic Ubuntu file. Server sends 16 KiB blocks with 25 ms delays.
CA-signed localhost leaf has CA:false, serverAuth, DNS:localhost SAN; saved CA
is explicitly selected by private fixture host config. No insecure TLS flags.

| Case | Exit | Duration (s) | Observed result |
| --- | --- | --- | --- |
| Live stderr PTY, stdout pipe | 0 | 3.829 | 25 timestamped updates; intermediate percent/rate before server completion and while CLI running; bar fits 80-column terminal |
| Non-TTY | 0 | 3.827 | Concise file/phase lines; no CR/ANSI; observed 0.6 MiB/s on Download complete line |
| Same-length corrupt Ubuntu body | 1 | 3.442 | Hash rejection; no success/publication message |
| One-byte truncated Ubuntu body | 1 | 3.440 | Incomplete rejection; no success/publication message |
| One-byte oversized Ubuntu body | 1 | 3.468 | Bounded size rejection; no success/publication message |

One observed live frame at 1.855 s displayed `49% 1.2 MiB/2.5 MiB 0.7 MiB/s
ubuntu.ext4` while the process was running and before server completion.
`pty.live.json` includes timestamps and raw chunks, not merely a final transcript.
The non-TTY rate is printed on the download-completion phase line; the final
`Assets verified and published.` line contains no rate.
Every case asserted exact legacy stdout. Both success cases checked all file
hashes, sparse physical allocation and one retained rollback. Every failure
checked old manifest preservation, no rollback and no leftover staging directory.
Per-case `.stdout`, `.stderr` and `.live.json` remain in the evidence directory.
Scratch roots were removed and the test server/thread stopped.

Initial TLS attempt exit 1: fixture used the CA as the endpoint certificate,
correctly rejected by rustls with `CaUsedAsEndEntity`. Preserved in
`data/assets-progress-build/tls.log` and `tls-evidence/pty.*`. Corrected only the
fixture to use a separate CA-signed server leaf, reran in a new evidence directory.

Protected onboarding unit attempt: exit **101 at link time**, not executed.
The musl debug SQLite object references missing `__ubsan_handle_*` symbols in
GNU ld; this is a test-toolchain link failure, not a passing unit receipt or an
observed downloader defect. Full attempted command is above; preserved output is
`data/assets-progress-build/tests.log`. No toolchain repair or extra compilation
was attempted. The parent accepted the five executed frozen static CLI cases for
this presentation-only Size S gate, preserving established capacity evidence.

Final formatting, Python syntax and whitespace checks passed. The final compiled
source files still match every recorded input in `release/build.json`; ELF has
neither INTERP nor NEEDED entries. See `data/assets-progress-build/final-checks.json`
for the frozen source/artifact check and evidence pins. AGENTS.md and
`.agent-protocol` were preserved; only the five task files are committed.
