#!/usr/bin/env bash
# Project-local client only. Login stores its credential in ~/.cloudflared.
set -euo pipefail
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
client_dir="$repo_root/data/remote-access/bin"
version=2026.9.3
expected_sha=77e26d8d900e0b8469f416239d14b5f296525fdf79fee6f511ef55609e3fbac2
[[ $(uname -m) == x86_64 ]] || { echo 'This pinned download is for Linux x86_64.' >&2; exit 1; }
umask 077
mkdir -p "$client_dir"
if [[ ! -f "$client_dir/cloudflared" ]]; then
  staging=$(mktemp "$client_dir/.download.XXXXXX")
  trap 'rm -f -- "$staging"' EXIT
  curl --fail --location --silent --show-error \
    "https://github.com/cloudflare/cloudflared/releases/download/$version/cloudflared-linux-amd64" -o "$staging"
  printf '%s  %s\n' "$expected_sha" "$staging" | sha256sum --check --status
  chmod 700 "$staging"
  mv -- "$staging" "$client_dir/cloudflared"
fi
printf '%s  %s\n' "$expected_sha" "$client_dir/cloudflared" | sha256sum --check --status
exec "$client_dir/cloudflared" tunnel login
