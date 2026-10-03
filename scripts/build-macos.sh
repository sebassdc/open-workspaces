#!/usr/bin/env bash
set -euo pipefail
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"
export RUSTUP_HOME="$repo_root/data/runtime-spike/rustup"
export CARGO_HOME="$repo_root/data/runtime-spike/cargo"
export PATH="$repo_root/data/macos-build:$CARGO_HOME/bin:$PATH"
export SDKROOT="$repo_root/data/macos-build/MacOSX14.5.sdk"
export CARGO_ZIGBUILD_ZIG_PATH="$repo_root/data/macos-build/zig-x86_64-linux-0.15.2/zig"
export CARGO_ZIGBUILD_CACHE_DIR="$repo_root/data/macos-build/cache"
export MACOSX_DEPLOYMENT_TARGET=11.0
python3 scripts/prepare-macos-build.py
rustup target add --toolchain 1.97.0 aarch64-apple-darwin x86_64-apple-darwin
cargo +1.97.0 zigbuild --locked --release -p ow --target aarch64-apple-darwin --target x86_64-apple-darwin -j 2
mkdir -p data/runtime-spike/bin
for arch in arm64 amd64; do
  target=aarch64-apple-darwin
  if [[ "$arch" == amd64 ]]; then target=x86_64-apple-darwin; fi
  artifact="data/runtime-spike/bin/ow-darwin-$arch"
  cp "target/$target/release/ow" "$artifact.tmp"
  chmod 755 "$artifact.tmp"
  mv "$artifact.tmp" "$artifact"
  sha256sum "$artifact" | cut -d ' ' -f 1 > "$artifact.sha256.tmp"
  mv "$artifact.sha256.tmp" "$artifact.sha256"
done
