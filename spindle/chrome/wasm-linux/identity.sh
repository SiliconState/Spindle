#!/usr/bin/env bash
# Prints the wasm Linux's identity, as runs record it: the image as the extension carries
# it (its index, which names every file by its digest, and its kernel) and the runtime
# that boots it and serves its devices (this directory's modules and v86's vendored
# files). build-image.sh writes it beside the image; chrome/build.sh works it out again,
# so a build whose runtime changed since the image was made still names it truly.
#
# usage: chrome/wasm-linux/identity.sh IMAGE_DIR
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
image=${1:?usage: chrome/wasm-linux/identity.sh IMAGE_DIR}
cat "$image/fs.json" "$image/vmlinux.wasm.zst" "$here"/*.mjs "$here"/v86/lib/*.js "$here"/v86/src/*.js "$here"/v86/src/browser/*.js \
    | sha256sum | cut -c1-64
