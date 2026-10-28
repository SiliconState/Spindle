#!/usr/bin/env bash
# Builds a try-it kit of the embedded package in one folder: the release wasm with
# its Node and browser glue, the JavaScript clients, the example and the guide
# (TRYING.md). Run from the spindle directory.
#
# usage: embed/kit.sh [WASM_BINDGEN_CLI] [OUT_DIR]   (OUT_DIR defaults to embed/kit)
set -euo pipefail

bindgen=${1:-wasm-bindgen}
out=${2:-embed/kit}
want=$(sed -n 's/^wasm-bindgen = "=\(.*\)"$/\1/p' embed/Cargo.toml)
have=$("$bindgen" --version | awk '{print $2}')
if [ "$have" != "$want" ]; then
    echo "wasm-bindgen CLI $have does not match the crate's $want" >&2
    exit 1
fi

cargo build --locked --release -p spindle-embed --target wasm32-unknown-unknown
wasm=target/wasm32-unknown-unknown/release/spindle_embed.wasm
rm -rf "$out"
mkdir -p "$out/clients" "$out/examples"
"$bindgen" --target nodejs --out-dir "$out/node" "$wasm"
"$bindgen" --target web --out-dir "$out/web" "$wasm"
cp clients/js/common.mjs clients/js/embedded.mjs clients/js/embedded-node.mjs \
    clients/js/embedded.d.mts clients/js/opfs.mjs clients/js/spindle.mjs "$out/clients/"
cp embed/examples/try_node.mjs embed/examples/relay.mjs "$out/examples/"
cp -r embed/examples/browser "$out/examples/"
cp embed/TRYING.md "$out/"
echo "kit in $out ($(du -h "$out/node/spindle_embed_bg.wasm" | cut -f1) wasm)"
