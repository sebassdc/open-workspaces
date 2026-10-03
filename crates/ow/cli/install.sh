#!/bin/sh
# Installs only this CLI and its browser-login helper in your user account.
set -eu
umask 077
origin='@@ORIGIN@@'
case "$(uname -s):$(uname -m)" in
  Linux:x86_64) artifact=ow-linux-amd64; helper_asset=cloudflared-linux-amd64; helper_sha=77e26d8d900e0b8469f416239d14b5f296525fdf79fee6f511ef55609e3fbac2 ;;
  Darwin:arm64) artifact=ow-darwin-arm64; helper_asset=cloudflared-darwin-arm64.tgz; helper_sha=587c2cfb1c230fe36c7fa7727da78be459dae028cabe8c001291999350f07095 ;;
  Darwin:x86_64) artifact=ow-darwin-amd64; helper_asset=cloudflared-darwin-amd64.tgz; helper_sha=d1155d0837487f261183b15c1eab6c4ebcad9dc49b94675f1524c3564cea3977 ;;
  *) echo 'Supported: macOS (Apple Silicon/Intel) and Linux x86-64.' >&2; exit 1 ;;
esac
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
staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT HUP INT TERM
curl --proto '=https' --tlsv1.2 -fsSL "$origin/cli/$artifact" -o "$staging/ow"
expected=$(curl --proto '=https' --tlsv1.2 -fsSL "$origin/cli/$artifact.sha256")
case "$expected" in *[!0-9a-f]*|'') echo 'Invalid CLI checksum' >&2; exit 1;; esac
[ "${#expected}" -eq 64 ] || { echo 'Invalid CLI checksum length' >&2; exit 1; }
verify "$expected" "$staging/ow"
# Pin and verify the official helper; never request account administration credentials.
if ! command -v cloudflared >/dev/null 2>&1 && [ ! -x "$helper_dir/cloudflared" ]; then
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
mkdir -p "$install_dir"
chmod 755 "$staging/ow"
mv "$staging/ow" "$install_dir/ow"
printf '\nInstalled: %s/ow\n' "$install_dir"
case ":$PATH:" in *":$install_dir:"*) ;; *) printf 'Add to your shell: export PATH="%s:$PATH"\n' "$install_dir";; esac
printf '\nConnect with:\n  %s/ow login %s\n  %s/ow list\n  %s/ow shell <machine>\n' "$install_dir" "$origin" "$install_dir" "$install_dir"
