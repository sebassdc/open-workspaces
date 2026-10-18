#!/usr/bin/env bash
# A retry can fill missing assets, but must never change published bytes.
set -euo pipefail
tag=$1
assets=$2
existing=$(mktemp -d)
trap 'rm -rf "$existing"' EXIT
remote_names=$(gh release view "$tag" --json assets --jq '.assets[].name')
for asset in "$assets"/*; do
  name=$(basename "$asset")
  if printf '%s\n' "$remote_names" | grep -Fxq "$name"; then
    gh release download "$tag" --pattern "$name" --dir "$existing"
    cmp "$asset" "$existing/$name" || {
      echo "Published $name differs; preserve this release and choose a fresh version" >&2
      exit 1
    }
  else
    gh release upload "$tag" "$asset"
  fi
done
