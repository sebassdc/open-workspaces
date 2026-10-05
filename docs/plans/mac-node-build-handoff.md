# Mac runtime merge/build — planner handoff
Updated: 2026-10-05 | Size: L | State: requested merge/build/docs complete; Mac pool integration ready

## Result
PR5 reviewed head f071bdb merged on GitHub as `3bd91552df50a22b4a57d411434054d84f7b0e37`. Four earlier review findings were corrected; reviewed bounded offline runtime source. Native15-check hardware/two Rust tests remain Mac-agent evidence because private native artifacts/receipts were not accessible to the Linux planner. No independent native attribution or Apple VM execution is claimed. Merge is for an experimental first slice, not full Mac hosting.

The owner authorized the next integration task. Give the Mac builder [mac-node-integration.md](mac-node-integration.md), which includes ordered context, branch/build commands, backend/architecture/ownership contracts, budgets, real acceptance, named stop gates and a copyable initial prompt. It requires a persistent ARM64 guest and actual outbound pool enrollment, not another feasibility-only prototype. Local independent work is authorized; live test invitation/controller deployment stays coordinated through planner.

## Compiled artifacts
From merged source, built in fresh ignored `data/mac-merge-build/`, with existing pinned Rust1.97.0/Zig0.15.2/SDK14.5. No artifact was committed, published to the installer or deployed:

| Artifact | Private path | SHA256 |
| --- | --- | --- |
| Static Linux CLI | linux-release/ow-linux-amd64 | `224144be873353ae02a75e4c7e06552cb41f75c980eaa8ae2cc3638da69c5c3c` |
| x86-64 Linux guest agent | linux-release/ow-guest | `1e5d3b4bc924755215a56531e36b26ffb3f0632ad270907d73f3aa9605642cf7` |
| Mac ARM64 CLI | darwin-release/ow-darwin-arm64 | `41c9fc92835e14798ea90627732a4fa29223fe7dce8a96cbebbd2cab443c6cdf` |
| Mac Intel CLI | darwin-release/ow-darwin-amd64 | `1d9d775cacc3a3eac2c8d82fa69e0d410be7b10b5125626ba61b85b2e2917b39` |

Linux release pair has source/tool/binary pins and per-file compiled-input hashes independently compared to merged Git blobs. Darwin outputs use the same unchanged tree and have checksum sidecars; private result.json records additional embedded provisioning/native inputs and platform inspection. Static ELF has no INTERP/NEEDED. Both Mach-O CLI binaries declare minimum macOS13.0.0 despite requested cross-build target14.0; do not substitute requested settings for observed metadata. ARM64 has a signature load command; native signature validation/notarization is not established here. Compiler emitted10 warnings per Darwin target (unused/dead code); builds succeeded.

The Swift `ow-vz` helper was NOT built on Linux. A CLI alone cannot host VMs. Build/sign it on the Mac with scripts/build-mac-host.sh and keep it beside the native CLI. Save/restore API availability and native deployment target require the Mac agent's validation. The x86 guest agent above is not an ARM64 guest agent.

## Verification
Merged source:49 serialized workspace tests passed/0failed/5real-VM ignored (15.83s);13 Mac harness real-subprocess regressions passed (3.326s). Linux release67s; Darwin cross-build136s. Suite/build durations are not product latency benchmarks. Linux CLI help executes; Mach-O format/architecture/minimum inspected. No extra VM or native Darwin execution. Private logs/receipt: offline-tests.log, linux-build.log, darwin-build.log, result.json. Source remained unchanged through builds. Active manager status was captured successfully; no service/worker/owner disk actions occurred.

## Next step / preservation
Owner's Mac agent updates main and executes mac-node-integration.md on work/mac-node. Current installer and live services remain earlier selected releases; don't describe these private builds as a deployed Mac node or available public SSH upgrade. No need stop existing hosts for this documentation/source handoff.

Pending root AGENTS/README/ROADMAP/handoffs and management files were preserved; only new integration/build-handoff docs belong to this task's scoped metadata commit. README/ROADMAP used a path-scoped temporary stash for fast-forward integration, then restored. No paid resources, unrelated services, global networking/Cloudflare or user guest assets were changed.
