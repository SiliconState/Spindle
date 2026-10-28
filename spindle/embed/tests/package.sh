#!/usr/bin/env bash
# The embedded package's runtime tests. Builds spindle-embed for
# wasm32-unknown-unknown, generates its JS glue with the wasm-bindgen CLI pinned in
# embed/Cargo.toml (Node and web targets), runs the Node tests (embed/tests/node_*.mjs,
# helpers named *_child.mjs aside) under Node 20+, then the browser test in headless
# Chrome when SPINDLE_BROWSER names one. SPINDLE_BIN, when set to a native spindle,
# adds the cross-host cases: the remote runner (against `spindle serve`) and
# sessions moving in place between the package and native spindle (node_move.mjs).
# Run from the spindle directory.
#
# usage: embed/tests/package.sh [WASM_BINDGEN_CLI]
set -euo pipefail

bindgen=${1:-wasm-bindgen}
want=$(sed -n 's/^wasm-bindgen = "=\(.*\)"$/\1/p' embed/Cargo.toml)
have=$("$bindgen" --version | awk '{print $2}')
if [ "$have" != "$want" ]; then
    echo "wasm-bindgen CLI $have does not match the crate's $want" >&2
    exit 1
fi
node -e 'if (+process.versions.node.split(".")[0] < 20) {
  console.error(`Node ${process.versions.node}: the package needs Node 20 or newer`);
  process.exit(1);
}'

cargo build --locked -p spindle-embed --target wasm32-unknown-unknown
wasm=target/wasm32-unknown-unknown/debug/spindle_embed.wasm
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
"$bindgen" --target nodejs --out-dir "$out/node" "$wasm"
"$bindgen" --target web --out-dir "$out/web" "$wasm"
for test in embed/tests/node_*.mjs; do
    case "$test" in *_child.mjs) continue ;; esac
    node "$test" "$out/node/spindle_embed.js"
done
node embed/tests/browser_opfs.mjs "$out/web"
