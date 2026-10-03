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

CC_x86_64_unknown_linux_musl=clang "$cargo_path" +1.97.0 build --locked --release -p ow --target x86_64-unknown-linux-musl -j 2
mkdir -p data/runtime-spike/bin
cp target/x86_64-unknown-linux-musl/release/ow data/runtime-spike/bin/ow-linux-amd64.tmp
strip data/runtime-spike/bin/ow-linux-amd64.tmp
mv data/runtime-spike/bin/ow-linux-amd64.tmp data/runtime-spike/bin/ow-linux-amd64
sha256sum data/runtime-spike/bin/ow-linux-amd64 | cut -d ' ' -f 1 > data/runtime-spike/bin/ow-linux-amd64.sha256.tmp
mv data/runtime-spike/bin/ow-linux-amd64.sha256.tmp data/runtime-spike/bin/ow-linux-amd64.sha256
