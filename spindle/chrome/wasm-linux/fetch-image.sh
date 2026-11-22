#!/usr/bin/env bash
# Fetches the wasm Linux image that image.json pins (a release asset of this repository,
# made by the wasm Linux workflow from build-image.sh), checks its sha256, and unpacks it
# into OUT_DIR for chrome/build.sh (SPINDLE_WASM_LINUX=OUT_DIR). The identity is worked out
# here from the files, as build-image.sh does.
#
# usage: chrome/wasm-linux/fetch-image.sh OUT_DIR
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=${1:?usage: chrome/wasm-linux/fetch-image.sh OUT_DIR}
field() { sed -n "s/^ *\"$1\": \"\\([^\"]*\\)\",*$/\\1/p" "$here/image.json"; }
url=$(field url)
sha=$(field sha256)
[[ $sha =~ ^[0-9a-f]{64}$ ]] && [ -n "$url" ] \
    || { echo "image.json pins no image yet: run the wasm Linux workflow and commit the image.json it prints" >&2; exit 1; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl -fsSL --retry 3 -o "$work/image.tar" "$url"
echo "$sha  $work/image.tar" | sha256sum -c --quiet -
rm -rf "$out"
mkdir -p "$out"
tar -xf "$work/image.tar" -C "$out" --no-same-owner
"$here/identity.sh" "$out" > "$out/identity"
echo "wasm Linux image in $out (Python $(cat "$out/python-version"), identity $(cut -c1-12 "$out/identity"))"
