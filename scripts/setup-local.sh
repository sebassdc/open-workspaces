#!/usr/bin/env bash
set -euo pipefail
umask 077
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"
mkdir -p data/runtime-spike/downloads
export RUSTUP_HOME="$repo_root/data/runtime-spike/rustup"
export CARGO_HOME="$repo_root/data/runtime-spike/cargo"
if [[ ! -x "$CARGO_HOME/bin/rustup" ]]; then
  installer=data/runtime-spike/downloads/rustup-init
  curl -fsSL --retry 2 https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init -o "$installer"
  # Pin the installer used in this experiment; never execute a changed download.
  printf '%s  %s\n' dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71 "$installer" | sha256sum --check
  chmod 700 "$installer"
  "$installer" -y --no-modify-path --profile minimal --default-toolchain 1.97.0
fi
"$CARGO_HOME/bin/rustup" toolchain install 1.97.0 --profile minimal
"$CARGO_HOME/bin/rustup" target add --toolchain 1.97.0 x86_64-unknown-linux-musl
bash experiments/runtime-spike/fetch-firecracker.sh
if [[ ! -f data/runtime-spike/guest/base.ext4 ]]; then
  python3 experiments/runtime-spike/prepare-guest.py
fi
python scripts/prepare-network.py
bash scripts/build-local.sh
python3 experiments/runtime-spike/preflight.py
