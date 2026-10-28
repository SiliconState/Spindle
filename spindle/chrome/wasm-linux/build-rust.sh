#!/usr/bin/env bash
# Builds Rust for the programs of Spindle's wasm Linux (the ABI of arch/wasm; C's target
# wasm32-unknown-linux-musl). Rust has no such target (its wasm32-wali-linux-musl is WALI,
# another ABI: a 64-bit `long`, x86-64's syscall numbers), so: a target spec,
# wasm32-spindle-linux-musl (vendor `spindle`, which is how the patches tell it from
# WALI; it links with build-toolchain.sh's compiler wrapper), a pinned nightly (from
# rustup) copied with its library source patched (rust/patches: core and std take this
# target's paths rather than WALI's, std spawns by posix_spawn only and has no
# stack-overflow handler; the libc crate gains musl's 32-bit wasm32 definitions, 64-bit
# time, asm-generic syscall numbers), a libunwind with nothing to unwind (rust/unwind.c:
# programs abort on panic) and `bin/cargo-wasm-linux`, cargo building a crate and std
# (-Zbuild-std) for the target. Nothing needs root.
#
# usage: chrome/wasm-linux/build-rust.sh TOOLCHAIN_DIR OUT_DIR
#   TOOLCHAIN_DIR: build-toolchain.sh's output. OUT_DIR gets toolchain/ (the nightly, its
#   library patched; its other files hard links to rustup's when they can be),
#   native/libunwind.a, wasm32-spindle-linux-musl.json and bin/cargo-wasm-linux.
#   Then, in a crate: OUT_DIR/bin/cargo-wasm-linux build --release
#   puts its programs in target/wasm32-spindle-linux-musl/release/.
set -euo pipefail
umask 022

here=$(cd "$(dirname "$0")" && pwd)
toolchain=$(cd "$1" && pwd)
out=$(mkdir -p "$2" && cd "$2" && pwd)

C_TARGET=wasm32-unknown-linux-musl
TARGET=wasm32-spindle-linux-musl
NIGHTLY=nightly-2026-08-07
RUSTC_COMMIT=84b36a78a28a63f134171c670be1932ffa2485f8
LIBC=libc-0.2.189

cc=$toolchain/bin/$C_TARGET-cc
[ -x "$cc" ] || { echo "no toolchain in $toolchain (build-toolchain.sh)" >&2; exit 1; }
command -v rustup >/dev/null || { echo "rustup is needed (https://rustup.rs)" >&2; exit 1; }

echo "Rust $NIGHTLY (${RUSTC_COMMIT:0:10}) with its library source"
rustup toolchain install "$NIGHTLY" --profile minimal --component rust-src --no-self-update >/dev/null
from=$(rustup run "$NIGHTLY" rustc --print sysroot)
commit=$("$from/bin/rustc" -vV | sed -n 's/^commit-hash: //p')
[ "$commit" = "$RUSTC_COMMIT" ] || { echo "$NIGHTLY is rustc $commit, not $RUSTC_COMMIT" >&2; exit 1; }

echo "its library, patched for $TARGET"
rm -rf "$out/toolchain"
cp -al "$from" "$out/toolchain" 2>/dev/null || { rm -rf "$out/toolchain"; cp -a "$from" "$out/toolchain"; }
# The library a copy of its own (not links to rustup's files): it is patched in place.
library=$out/toolchain/lib/rustlib/src/rust/library
rm -rf "$library"
cp -r "$from/lib/rustlib/src/rust/library" "$library"
for patch in std libc; do
    patch -s -p1 -d "$library" <"$here/rust/patches/$patch.patch"
done

echo "target spec, libunwind, cargo-wasm-linux"
sed "s|@LINKER@|$cc|" "$here/rust/$TARGET.json" >"$out/$TARGET.json"
mkdir -p "$out/native" "$out/bin"
"$cc" -O2 -c "$here/rust/unwind.c" -o "$out/native/unwind.o"
rm -f "$out/native/libunwind.a"
"$toolchain/bin/$C_TARGET-ar" rcs "$out/native/libunwind.a" "$out/native/unwind.o"
rm "$out/native/unwind.o"
# Cargo builds std against the patched libc only as a patch of crates.io's (it does not
# read the library's own vendored sources); a crate that does not use libc itself gets a
# warning that the patch went unused.
cat >"$out/bin/cargo-wasm-linux" <<WRAPPER
#!/bin/sh
# cargo for Spindle's wasm Linux: cargo-wasm-linux COMMAND [ARGS] (build, check, ...)
[ \$# -gt 0 ] || { echo "usage: cargo-wasm-linux COMMAND [ARGS]" >&2; exit 2; }
command=\$1
shift
export RUSTC="$out/toolchain/bin/rustc"
export RUSTFLAGS="\${RUSTFLAGS:+\$RUSTFLAGS }-L native=$out/native"
exec "$out/toolchain/bin/cargo" "\$command" \\
    -Zbuild-std=std,panic_abort -Zbuild-std-features= -Zjson-target-spec \\
    --target "$out/$TARGET.json" \\
    --config "patch.crates-io.libc.path=\\"$library/vendor/$LIBC\\"" "\$@"
WRAPPER
chmod 755 "$out/bin/cargo-wasm-linux"
echo "Rust for $TARGET in $out: build a crate with $out/bin/cargo-wasm-linux build --release"
