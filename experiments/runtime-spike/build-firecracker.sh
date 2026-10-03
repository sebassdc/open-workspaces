#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
spike_root="$repo_root/data/runtime-spike"
source_root="$spike_root/upstream/firecracker"
expected_revision=95f868c8e345b1cc8faccd1a3c910b4989dc3f58

if [[ $(git -C "$source_root" rev-parse HEAD) != "$expected_revision" ]]; then
  echo 'Firecracker revision does not match the experiment pin.' >&2
  exit 1
fi
if [[ -n $(git -C "$source_root" status --porcelain) ]]; then
  echo 'Firecracker checkout has local changes; use a clean pinned checkout.' >&2
  exit 1
fi
export RUSTUP_HOME="$spike_root/rustup"
export CARGO_HOME="$spike_root/cargo"
export CARGO_TARGET_DIR="$spike_root/build/firecracker"
cd "$source_root"
"$CARGO_HOME/bin/cargo" +1.97.0 build --locked --release \
  --target x86_64-unknown-linux-gnu -p firecracker -p jailer -j 2
