#!/usr/bin/env bash
set -euo pipefail
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cross_root=${OW_CROSS_BUILD_ROOT:-"$repo_root/data/macos-build"}
export ZIG_GLOBAL_CACHE_DIR="${OW_ZIG_CACHE_DIR:-$cross_root/cache}"
# cc-rs recognizes Clang and adds Rust's four-part triple; Zig uses three parts.
compiler_args=()
for arg in "$@"; do
  if [[ "$arg" == --target=x86_64-unknown-linux-musl ]]; then continue; fi
  compiler_args+=("$arg")
done
exec "$cross_root/zig-x86_64-linux-0.15.2/zig" cc -target x86_64-linux-musl "${compiler_args[@]}"
