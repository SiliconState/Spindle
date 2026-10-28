#!/usr/bin/env bash
# Builds the WASI store probe and runs it under Node's WASI preview1.
#
# usage: store/tests/wasi_probe.sh WASI_LIBC_ROOT
#
# WASI_LIBC_ROOT is an extracted wasi-libc package (it holds usr/include/wasm32-wasi*
# and usr/lib/wasm32-wasi*), never the host's /usr: SQLite's C sources must not see
# host glibc headers. Only wasi-libc's emulation archives are linked; Rust's own
# self-contained WASI libc and startup files stay in charge (mixing in the package's
# libc.a fails at link time with __wasi_init_tp). Run from the spindle directory.
set -euo pipefail

root=${1:?usage: store/tests/wasi_probe.sh WASI_LIBC_ROOT}
root=$(cd "$root" && pwd)

first_dir() {
    for dir in "$@"; do
        if [ -d "$dir" ]; then
            echo "$dir"
            return 0
        fi
    done
    return 1
}
include=$(first_dir "$root/usr/include/wasm32-wasi" "$root/usr/include/wasm32-wasip1") ||
    { echo "no wasm32-wasi headers under $root/usr/include" >&2; exit 1; }
lib=$(first_dir "$root/usr/lib/wasm32-wasi" "$root/usr/lib/wasm32-wasip1") ||
    { echo "no wasm32-wasi libraries under $root/usr/lib" >&2; exit 1; }

node -e 'if (+process.versions.node.split(".")[0] < 20) {
  console.error(`Node ${process.versions.node}: WASI preview1 needs Node 20 or newer`);
  process.exit(1);
}'

emulated=$(mktemp -d)
trap 'rm -rf "$emulated"' EXIT
emulations=(mman getpid signal process-clocks)
link=()
for name in "${emulations[@]}"; do
    cp "$lib/libwasi-emulated-$name.a" "$emulated/"
    link+=(-l "static=wasi-emulated-$name")
done

export CC_wasm32_wasip1=${CC_wasm32_wasip1:-clang}
export AR_wasm32_wasip1=${AR_wasm32_wasip1:-ar}
export CFLAGS_wasm32_wasip1="--sysroot=$root/usr -I$include"
export RUSTFLAGS="-L native=$emulated ${link[*]}"
cargo build --locked -p spindle-store --no-default-features --features driver \
    --example store_probe --target wasm32-wasip1
node store/tests/wasi_probe.mjs target/wasm32-wasip1/debug/examples/store_probe.wasm
