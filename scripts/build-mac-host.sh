#!/bin/sh
set -eu
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || {
    echo 'Native Mac host requires Apple Silicon macOS' >&2; exit 1;
}
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
out="$root/target/mac-host"
mkdir -p "$out"
swiftc -O -framework Virtualization "$root/native/macos/main.swift" -o "$out/ow-vz"
codesign --force --sign - --entitlements "$root/native/macos/virtualization.entitlements" "$out/ow-vz"
"$out/ow-vz" capabilities
cd "$root"
CARGO_TARGET_DIR="$root/target/mac-host-rust" cargo build --locked -p ow
cp "$root/target/mac-host-rust/debug/ow" "$out/ow"
"$out/ow" mac-host capabilities
