#!/usr/bin/env bash
set -euo pipefail
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"
if [[ -x data/runtime-spike/cargo/bin/cargo ]]; then
  export RUSTUP_HOME="$repo_root/data/runtime-spike/rustup"
  export CARGO_HOME="$repo_root/data/runtime-spike/cargo"
  cargo_path="$CARGO_HOME/bin/cargo"
else
  cargo_path=$(command -v cargo)
fi
"$cargo_path" +1.97.0 build --locked --release -p ow-guest --target x86_64-unknown-linux-musl -j 2
mkdir -p data/runtime-spike/guest
cp target/x86_64-unknown-linux-musl/release/ow-guest data/runtime-spike/guest/ow-guest.tmp
strip data/runtime-spike/guest/ow-guest.tmp
mv data/runtime-spike/guest/ow-guest.tmp data/runtime-spike/guest/ow-guest
"$cargo_path" +1.97.0 build --locked --release -p ow -j 2

# Bundled SQLite needs actual musl headers, not the host's glibc headers.
python3 - <<'PYBUILD'
import importlib.util
spec=importlib.util.spec_from_file_location('crossbuild','scripts/prepare-macos-build.py')
module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
module.ROOT.mkdir(parents=True,exist_ok=True)
module.prepare(module.ARTIFACTS[0])
PYBUILD
CC_x86_64_unknown_linux_musl="$repo_root/scripts/musl-cc.sh" "$cargo_path" +1.97.0 build --locked --release -p ow --target x86_64-unknown-linux-musl -j 2
mkdir -p data/runtime-spike/bin
cp target/x86_64-unknown-linux-musl/release/ow data/runtime-spike/bin/ow-linux-amd64.tmp
strip data/runtime-spike/bin/ow-linux-amd64.tmp
mv data/runtime-spike/bin/ow-linux-amd64.tmp data/runtime-spike/bin/ow-linux-amd64
sha256sum data/runtime-spike/bin/ow-linux-amd64 | cut -d ' ' -f 1 > data/runtime-spike/bin/ow-linux-amd64.sha256.tmp
mv data/runtime-spike/bin/ow-linux-amd64.sha256.tmp data/runtime-spike/bin/ow-linux-amd64.sha256
