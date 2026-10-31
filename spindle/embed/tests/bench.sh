#!/usr/bin/env bash
# Local engine time and memory, native and release wasm: builds the package's release
# wasm with Node glue, runs embed/tests/bench.mjs under Node and store/tests/bench.rs
# natively, and writes both reports to OUT_DIR (wasm.json, native.json). With a second
# directory holding an earlier run's reports, prints each figure beside it. A model
# that answers at once stands in for the provider, so the figures are Spindle's own
# work. Run from the spindle directory.
#
# usage: embed/tests/bench.sh [--quick] WASM_BINDGEN_CLI OUT_DIR [EARLIER_DIR]
set -euo pipefail

quick=
if [ "${1:-}" = --quick ]; then
    quick=--quick
    shift
fi
bindgen=${1:?usage: embed/tests/bench.sh [--quick] WASM_BINDGEN_CLI OUT_DIR [EARLIER_DIR]}
out=${2:?usage: embed/tests/bench.sh [--quick] WASM_BINDGEN_CLI OUT_DIR [EARLIER_DIR]}
earlier=${3:-}
mkdir -p "$out"

cargo build --locked --release -p spindle-embed --target wasm32-unknown-unknown
glue=$(mktemp -d)
trap 'rm -rf "$glue"' EXIT
"$bindgen" --target nodejs --out-dir "$glue" target/wasm32-unknown-unknown/release/spindle_embed.wasm
node embed/tests/bench.mjs "$glue/spindle_embed.js" $quick --out "$out/wasm.json"

if [ -n "$quick" ]; then
    export SPINDLE_BENCH_QUICK=1
fi
SPINDLE_BENCH_OUT="$(cd "$out" && pwd)/native.json" \
    cargo test --locked --release -p spindle-store --features embedded --test bench -- \
    --ignored --nocapture

if [ -n "$earlier" ]; then
    node --input-type=module - "$earlier" "$out" <<'EOF'
import fs from 'node:fs';
const [before, after] = process.argv.slice(2);
// Each number in the reports, with the earlier one and the change.
function walk(a, b, path, rows) {
  if (typeof b === 'number' && typeof a === 'number') {
    const change = a === 0 ? '' : `${b >= a ? '+' : ''}${Math.round((b - a) / a * 100)}%`;
    rows.push([path, a, b, change]);
  } else if (b && typeof b === 'object') {
    for (const key of Object.keys(b)) walk(a?.[key], b[key], path ? `${path}.${key}` : key, rows);
  }
}
for (const host of ['wasm', 'native']) {
  const read = (dir) => JSON.parse(fs.readFileSync(`${dir}/${host}.json`, 'utf8')).results;
  const rows = [];
  walk(read(before), read(after), '', rows);
  console.log(`\n${host}`);
  for (const [path, a, b, change] of rows.filter(([path]) => !/\.n$/.test(path))) {
    console.log(`  ${path.padEnd(36)} ${String(a).padStart(10)} ${String(b).padStart(10)}  ${change}`);
  }
}
EOF
fi
