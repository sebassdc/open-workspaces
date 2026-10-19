#!/usr/bin/env bash
set -euo pipefail
umask 077
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"
mkdir -p data/runtime-spike/downloads
export RUSTUP_HOME="$repo_root/data/runtime-spike/rustup"
export CARGO_HOME="$repo_root/data/runtime-spike/cargo"
# A host staging a release's published binaries passes its verified guest agent: no Rust
# build, and a toolchain only for a new base image's test fixture.
prebuilt_guest=${OW_PREBUILT_GUEST:-}
rust=yes
if [[ -n $prebuilt_guest && -f data/runtime-spike/guest/base.ext4 ]]; then
  rust=
fi
if [[ -n $rust && ! -x "$CARGO_HOME/bin/rustup" ]]; then
  installer=data/runtime-spike/downloads/rustup-init
  curl -fsSL --retry 2 https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init -o "$installer"
  # Pin the installer used in this experiment; never execute a changed download.
  printf '%s  %s\n' dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71 "$installer" | sha256sum --check
  chmod 700 "$installer"
  "$installer" -y --no-modify-path --profile minimal --default-toolchain 1.97.0
fi
if [[ -n $rust ]]; then
  "$CARGO_HOME/bin/rustup" toolchain install 1.97.0 --profile minimal
fi
# Controller packaging needs only the native binary, no runtime assets or KVM preflight.
if [[ ${1:-} == --controller-only ]]; then
  "$CARGO_HOME/bin/cargo" +1.97.0 build --locked --release -p ow -j 2
  exit 0
fi
if [[ -n $rust ]]; then
  "$CARGO_HOME/bin/rustup" target add --toolchain 1.97.0 x86_64-unknown-linux-musl
fi
bash experiments/runtime-spike/fetch-firecracker.sh
# The pinned guest kernel, also when a base image already exists (an upgrade's
# asset set starts as a copy of the previous release's).
python3 experiments/runtime-spike/prepare-guest.py --downloads-only
if [[ ! -f data/runtime-spike/guest/base.ext4 ]]; then
  python3 experiments/runtime-spike/prepare-guest.py
fi
python scripts/prepare-network.py
if [[ -n $prebuilt_guest ]]; then
  install -m 755 "$prebuilt_guest" data/runtime-spike/guest/ow-guest.tmp
  mv data/runtime-spike/guest/ow-guest.tmp data/runtime-spike/guest/ow-guest
else
  bash scripts/build-local.sh
fi
python3 experiments/runtime-spike/preflight.py
