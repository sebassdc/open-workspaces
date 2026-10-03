#!/usr/bin/env bash
set -euo pipefail
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
export ZIG_GLOBAL_CACHE_DIR="$repo_root/data/macos-build/cache"
# cc-rs recognizes Clang and adds Rust's four-part triple; Zig uses three parts.
compiler_args=()
for arg in "$@"; do
  if [[ "$arg" == --target=x86_64-unknown-linux-musl ]]; then continue; fi
  compiler_args+=("$arg")
done
exec "$repo_root/data/macos-build/zig-x86_64-linux-0.15.2/zig" cc -target x86_64-linux-musl "${compiler_args[@]}"
