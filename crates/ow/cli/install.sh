#!/bin/sh
# Installs the CLI in your user account; Access mode also prepares its helper.
set -eu
umask 077
origin='@@ORIGIN@@'
version=
access=true
if [ "${1:-}" = --version ] && [ "$#" -ge 2 ]; then
  version=$2
  shift 2
  case "$version" in *[!v0-9.]*|'') echo 'Invalid release version' >&2; exit 1 ;; esac
  printf '%s\n' "$version" | LC_ALL=C grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' || {
    echo 'Use an exact release version, e.g. --version v0.8.0' >&2; exit 1;
  }
  access=false
  if [ "${1:-}" = --access ]; then access=true; shift; fi
else
  case "$origin" in
    https://*) ;;
    *) echo 'Usage: sh install.sh --version vX.Y.Z [--access]' >&2; exit 1 ;;
  esac
fi
[ "$#" -eq 0 ] || { echo 'Unexpected installer arguments' >&2; exit 1; }
case "$(uname -s):$(uname -m)" in
  Linux:x86_64) artifact=ow-linux-amd64; helper_asset=cloudflared-linux-amd64; helper_sha=77e26d8d900e0b8469f416239d14b5f296525fdf79fee6f511ef55609e3fbac2 ;;
  Darwin:arm64) artifact=ow-darwin-arm64; helper_asset=cloudflared-darwin-arm64.tgz; helper_sha=587c2cfb1c230fe36c7fa7727da78be459dae028cabe8c001291999350f07095 ;;
  Darwin:x86_64) artifact=ow-darwin-amd64; helper_asset=cloudflared-darwin-amd64.tgz; helper_sha=d1155d0837487f261183b15c1eab6c4ebcad9dc49b94675f1524c3564cea3977 ;;
  *) echo 'Supported: macOS (Apple Silicon/Intel) and Linux x86-64.' >&2; exit 1 ;;
esac
base="$origin/cli"
if [ -n "$version" ]; then
  base="https://github.com/sebassdc/open-workspaces/releases/download/$version"
fi
verify() {
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$2" | awk '{print $1}')
  else
    actual=$(shasum -a 256 "$2" | awk '{print $1}')
  fi
  [ "$actual" = "$1" ] || { echo 'Download checksum mismatch' >&2; exit 1; }
}
install_dir=${OW_INSTALL_DIR:-"$HOME/.local/bin"}
helper_dir="$HOME/.local/lib/open-workspaces"
# Stage on the destination filesystem so replacing a working client is atomic.
mkdir -p "$install_dir"
if [ -L "$install_dir/ow" ] || { [ -e "$install_dir/ow" ] && [ ! -f "$install_dir/ow" ]; }; then
  echo 'Install target must be a regular file, not a directory or symlink' >&2; exit 1;
fi
staging=$(mktemp -d "$install_dir/.ow-install.XXXXXX")
trap 'rm -rf "$staging"' EXIT HUP INT TERM
curl --proto '=https' --tlsv1.2 -fsSL "$base/$artifact" -o "$staging/ow"
expected=$(curl --proto '=https' --tlsv1.2 -fsSL "$base/$artifact.sha256")
case "$expected" in *[!0-9a-f]*|'') echo 'Invalid CLI checksum' >&2; exit 1;; esac
[ "${#expected}" -eq 64 ] || { echo 'Invalid CLI checksum length' >&2; exit 1; }
verify "$expected" "$staging/ow"
if [ -n "$version" ]; then
  header=$(od -An -tx1 -N20 "$staging/ow" | tr -d ' \n')
  case "$artifact:$header" in
    ow-linux-amd64:7f454c46020101??????????????????????3e00*|\
    ow-darwin-arm64:cffaedfe0c000001*|ow-darwin-amd64:cffaedfe07000001*) ;;
    *) echo 'Download platform mismatch' >&2; exit 1 ;;
  esac
  chmod 755 "$staging/ow"
  [ "$("$staging/ow" --version)" = "ow ${version#v}" ] || {
    echo 'Download version mismatch' >&2; exit 1;
  }
fi
# Pin and verify the official helper; never request account administration credentials.
if [ "$access" = true ] && ! command -v cloudflared >/dev/null 2>&1 && [ ! -x "$helper_dir/cloudflared" ]; then
  curl --proto '=https' --tlsv1.2 -fsSL "https://github.com/cloudflare/cloudflared/releases/download/2026.9.3/$helper_asset" -o "$staging/helper-download"
  verify "$helper_sha" "$staging/helper-download"
  case "$helper_asset" in
    *.tgz) tar -xzf "$staging/helper-download" -C "$staging" cloudflared ;;
    *) mv "$staging/helper-download" "$staging/cloudflared" ;;
  esac
  mkdir -p "$helper_dir"
  chmod 755 "$staging/cloudflared"
  mv "$staging/cloudflared" "$helper_dir/cloudflared"
fi
chmod 755 "$staging/ow"
mv "$staging/ow" "$install_dir/ow"
printf '\nInstalled: %s/ow\n' "$install_dir"
case ":$PATH:" in *":$install_dir:"*) ;; *) printf 'Add to your shell: export PATH="%s:$PATH"\n' "$install_dir";; esac
if [ -n "$version" ]; then
  printf '\nInstalled release: %s\n' "$version"
  if [ "$access" = true ]; then
    printf 'Connect: %s/ow login https://YOUR_HOST\n' "$install_dir"
  else
    printf 'Connect: %s/ow login --tailnet https://NAME.TAILNET.ts.net\n' "$install_dir"
  fi
else
  printf '\nConnect with:\n  %s/ow login %s\n  %s/ow list\n  %s/ow shell <machine>\n' "$install_dir" "$origin" "$install_dir" "$install_dir"
fi
