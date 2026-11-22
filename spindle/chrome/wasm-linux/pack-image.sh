#!/usr/bin/env bash
# Packs a wasm Linux image (build-image.sh's output) into one tar for a release asset:
# its index, files, kernel and Python version, in one order with no owners or times, so
# the same image gives the same tar. Prints the tar's sha256 (image.json pins it).
#
# usage: chrome/wasm-linux/pack-image.sh IMAGE_DIR OUT_TAR
set -euo pipefail
image=${1:?usage: chrome/wasm-linux/pack-image.sh IMAGE_DIR OUT_TAR}
out=${2:?usage: chrome/wasm-linux/pack-image.sh IMAGE_DIR OUT_TAR}
for part in fs.json flat vmlinux.wasm.zst python-version; do
    [ -e "$image/$part" ] || { echo "no $part in $image (build-image.sh)" >&2; exit 1; }
done
tar -C "$image" --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 --mode=u+rw,go+r,a-x+X \
    -cf "$out" fs.json flat vmlinux.wasm.zst python-version
sha256sum "$out" | cut -c1-64
