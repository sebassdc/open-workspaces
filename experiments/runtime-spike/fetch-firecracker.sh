#!/usr/bin/env bash
set -euo pipefail
umask 077

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
spike_root="$repo_root/data/runtime-spike"
archive_name=firecracker-v1.17.0-x86_64.tgz
expected_sha256=06094a1108ae9e82aa4c23a775aa92758f53f1175d422270d9d6162cb9ade558
mkdir -p "$spike_root/downloads" "$spike_root/official"
archive="$spike_root/downloads/$archive_name"
if [[ ! -f "$archive" ]]; then
  temporary=$(mktemp "$spike_root/downloads/firecracker.XXXXXX")
  trap 'rm -f -- "$temporary"' EXIT
  curl --fail --location --retry 2 --output "$temporary" \
    "https://github.com/firecracker-microvm/firecracker/releases/download/v1.17.0/$archive_name"
  printf '%s  %s\n' "$expected_sha256" "$temporary" | sha256sum --check
  mv -- "$temporary" "$archive"
fi
printf '%s  %s\n' "$expected_sha256" "$archive" | sha256sum --check
tar --extract --gzip --file "$archive" --directory "$spike_root/official" \
  --no-same-owner --no-same-permissions
"$spike_root/official/release-v1.17.0-x86_64/firecracker-v1.17.0-x86_64" --version
