#!/usr/bin/env bash
# Builds the kernel of Spindle's wasm Linux: Linux for the wasm architecture (tombl/linux,
# arch/wasm; GPL-2.0, so the binary ships with this source offer: the pinned commit
# below), with its defconfig, by clang and LLD. Nothing needs root: m4, bison and flex
# (kconfig's) are built into TOOLS when they are missing; the LLD comes from LLVM or
# rustup. Downloads are pinned (a commit, checksums). Spindle's fixes (kernel/*.patch)
# go on the pinned commit, and its options (spindle.config: 9p shares, namespaces,
# System V IPC) on top of the defconfig.
#
# usage: chrome/wasm-linux/build-kernel.sh OUT_DIR [TOOLS_DIR]
#   OUT_DIR gets vmlinux.wasm; TOOLS_DIR (default: OUT_DIR/tools) keeps the build tools.
set -euo pipefail
umask 022

here=$(cd "$(dirname "$0")" && pwd)
out=$(mkdir -p "$1" && cd "$1" && pwd)
tools=$(mkdir -p "${2:-$out/tools}" && cd "${2:-$out/tools}" && pwd)

LINUX_REPO=https://github.com/tombl/linux
LINUX_COMMIT=d1911fddcb0169823f16089daf4bb6a904515f36
M4=m4-1.4.19.tar.xz
M4_SHA256=63aede5c6d33b6d9b13511cd0be2cac046f2e70fd0a07aa9573a04a82783af96
BISON=bison-3.8.2.tar.xz
BISON_SHA256=9bba0214ccf7f1079c5d59210045227bcf619519840ebfa80cd3849cff5a5bf2
FLEX=flex-2.6.4.tar.gz
FLEX_SHA256=e87aae032bf07c26f85ac0ed3250998c37621d95f8bd748b31f15b33c45ee995

fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
check() { echo "$2  $1" | sha256sum -c --quiet -; }
# A clean PATH: a Windows one (WSL) has spaces that break configure.
export PATH="$tools/llvm:$tools/bin:/usr/local/bin:/usr/bin:/bin"

# kconfig's tools, built where the build can find them (old gnulib meets new compilers).
build_tool() {
    local name=$1 file=$2 sum=$3 url=$4
    [ -x "$tools/bin/$name" ] && return
    echo "building $name"
    local work
    work=$(mktemp -d)
    fetch "$url" "$work/$file"
    check "$work/$file" "$sum"
    tar xf "$work/$file" -C "$work"
    (cd "$work/${file%.tar.*}" \
        && CFLAGS="-O2 -std=gnu11 -Wno-error=implicit-function-declaration -Wno-error=int-conversion -Wno-error=incompatible-pointer-types" \
           ./configure --prefix="$tools" --disable-nls --disable-shared >/dev/null \
        && make -j"$(nproc)" >/dev/null 2>&1 && make install >/dev/null)
    rm -rf "$work"
}
command -v m4 >/dev/null || build_tool m4 "$M4" "$M4_SHA256" "https://ftp.gnu.org/gnu/m4/$M4"
command -v bison >/dev/null || build_tool bison "$BISON" "$BISON_SHA256" "https://ftp.gnu.org/gnu/bison/$BISON"
command -v flex >/dev/null || build_tool flex "$FLEX" "$FLEX_SHA256" "https://github.com/westes/flex/releases/download/v2.6.4/$FLEX"

# clang and the LLVM tools under their plain names, and a wasm-ld (LLVM's, else rustup's).
mkdir -p "$tools/llvm"
llvm=$(dirname "$(readlink -f "$(command -v clang-21 || command -v clang)")")
for tool in clang llvm-ar llvm-nm llvm-objcopy llvm-objdump llvm-readelf llvm-strip llvm-ranlib; do
    [ -x "$llvm/$tool" ] && ln -sf "$llvm/$tool" "$tools/llvm/$tool"
done
if [ -x "$llvm/wasm-ld" ]; then
    ln -sf "$llvm/wasm-ld" "$tools/llvm/wasm-ld"
else
    rustld=$(ls "$HOME"/.rustup/toolchains/*/lib/rustlib/*/bin/gcc-ld/wasm-ld 2>/dev/null | head -1)
    [ -n "$rustld" ] || { echo "no wasm-ld: install LLD or a Rust toolchain" >&2; exit 1; }
    ln -sf "$rustld" "$tools/llvm/wasm-ld"
fi
command -v wasm2wat >/dev/null || { echo "the kernel build needs wasm2wat (wabt)" >&2; exit 1; }

src="$out/linux"
if [ ! -d "$src/.git" ]; then
    git init -q "$src"
    git -C "$src" remote add origin "$LINUX_REPO"
fi
git -C "$src" fetch -q --depth 1 origin "$LINUX_COMMIT"
git -C "$src" checkout -q --force "$LINUX_COMMIT"
# The tree as the commit has it (files an earlier run's patches added go too), then the
# patches.
git -C "$src" clean -fdq
for patch in "$here"/kernel/*.patch; do
    git -C "$src" apply "$patch"
done

build="$out/build"
mkdir -p "$build/arch/wasm"
# The arch Makefile names its script by a path relative to the tree.
ln -sfn "$src/arch/wasm/scripts" "$build/arch/wasm/scripts"
make -C "$src" O="$build" LLVM=1 HOSTCC=clang HOSTLD=/usr/bin/ld defconfig >/dev/null
# Spindle's options on top (spindle.config): the runner depends on every one (full
# dynticks above all), so one the kernel does not take stops the build.
fragment="$here/spindle.config"
cat "$fragment" >>"$build/.config"
make -C "$src" O="$build" LLVM=1 HOSTCC=clang HOSTLD=/usr/bin/ld olddefconfig >/dev/null
dropped=$(grep '^CONFIG_' "$fragment" | while read -r wanted; do grep -qx "$wanted" "$build/.config" || echo "$wanted"; done)
[ -z "$dropped" ] || { echo "the kernel did not take: $dropped" >&2; exit 1; }
make -C "$src" O="$build" LLVM=1 HOSTCC=clang HOSTLD=/usr/bin/ld -j"$(nproc)" vmlinux.wasm >"$out/build.log" 2>&1 \
    || { tail -n 20 "$out/build.log" >&2; exit 1; }
cp "$build/vmlinux.wasm" "$out/vmlinux.wasm"
echo "Linux for wasm (${LINUX_COMMIT:0:7}) in $out/vmlinux.wasm ($(du -h "$out/vmlinux.wasm" | cut -f1))"
