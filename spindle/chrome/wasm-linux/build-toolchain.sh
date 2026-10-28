#!/usr/bin/env bash
# Builds the toolchain for the programs of Spindle's wasm Linux (target
# wasm32-unknown-linux-musl, the ABI of arch/wasm): clang and LLD from tombl/llvm-project
# (LLVM with a wasm Linux target: Linux's predefined macros, its driver and sysroot
# layout, `main` with an environment named apart, the function table from 3 so 1 and 2
# can be SIG_IGN and SIG_HOLD; Apache-2.0 with LLVM exceptions), then a sysroot of
# tombl/musl (MIT), the kernel's userspace headers (from the kernel's own pinned source,
# GPL-2.0 with the syscall note) and compiler-rt's builtins. Everything from source at
# pinned commits; nothing needs root. Ninja (pinned) is fetched when missing.
#
# usage: chrome/wasm-linux/build-toolchain.sh OUT_DIR [TOOLS_DIR]
#   OUT_DIR gets llvm/ (the compilers and tools) and sysroot/; sources and builds go in
#   OUT_DIR/src and OUT_DIR/build. TOOLS_DIR (default OUT_DIR/tools) keeps ninja.
set -euo pipefail
umask 022

out=$(mkdir -p "$1" && cd "$1" && pwd)
tools=$(mkdir -p "${2:-$out/tools}" && cd "${2:-$out/tools}" && pwd)

TARGET=wasm32-unknown-linux-musl
LLVM_REPO=https://github.com/tombl/llvm-project
LLVM_COMMIT=9aaceb42fef4f924a00126e0d66140d01482921c
MUSL_REPO=https://github.com/tombl/musl
MUSL_COMMIT=637b0d25dafa7e4740357f25fb0b5e3949f1ed1f
LINUX_REPO=https://github.com/tombl/linux
LINUX_COMMIT=d1911fddcb0169823f16089daf4bb6a904515f36
NINJA_URL=https://github.com/ninja-build/ninja/releases/download/v1.12.1/ninja-linux.zip
NINJA_SHA256=6f98805688d19672bd699fbbfa2c2cf0fc054ac3df1f0e6a47664d963d530255
# What every program is compiled with: shared-memory atomics (each process's memory is
# shared with its threads), bulk memory, and setjmp/longjmp lowered by LLVM.
WASM_CFLAGS="-matomics -mbulk-memory -mllvm -wasm-enable-sjlj"

fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
check() { echo "$2  $1" | sha256sum -c --quiet -; }
export PATH="$tools/bin:/usr/local/bin:/usr/bin:/bin"
jobs=$(nproc)

# A source tree at a pinned commit (shallow), kept between runs.
checkout() {
    local dir=$1 repo=$2 commit=$3
    if [ ! -d "$dir/.git" ]; then
        git init -q "$dir"
        git -C "$dir" remote add origin "$repo"
    fi
    if [ "$(git -C "$dir" rev-parse -q --verify HEAD 2>/dev/null)" != "$commit" ]; then
        git -C "$dir" fetch -q --depth 1 origin "$commit"
        git -C "$dir" checkout -q --force "$commit"
    fi
}

if ! command -v ninja >/dev/null; then
    echo "ninja"
    mkdir -p "$tools/bin"
    fetch "$NINJA_URL" "$tools/ninja.zip"
    check "$tools/ninja.zip" "$NINJA_SHA256"
    unzip -o -q "$tools/ninja.zip" -d "$tools/bin"
    rm "$tools/ninja.zip"
fi

src=$out/src
build=$out/build
llvm=$out/llvm
sysroot=$out/sysroot
mkdir -p "$src" "$build"

echo "LLVM (clang and LLD for $TARGET, ${LLVM_COMMIT:0:10})"
checkout "$src/llvm-project" "$LLVM_REPO" "$LLVM_COMMIT"
if [ ! -x "$llvm/bin/clang" ] || [ "$(cat "$llvm/commit" 2>/dev/null)" != "$LLVM_COMMIT" ]; then
    # Links are what take memory: two at a time.
    cmake -S "$src/llvm-project/llvm" -B "$build/llvm" -G Ninja -Wno-dev \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++ \
        -DCMAKE_INSTALL_PREFIX="$llvm" \
        -DLLVM_ENABLE_PROJECTS="clang;lld" \
        -DLLVM_TARGETS_TO_BUILD="X86;WebAssembly" \
        -DLLVM_DEFAULT_TARGET_TRIPLE="$TARGET" \
        -DLLVM_ENABLE_ASSERTIONS=OFF \
        -DLLVM_INCLUDE_TESTS=OFF -DLLVM_INCLUDE_BENCHMARKS=OFF -DLLVM_INCLUDE_EXAMPLES=OFF -DLLVM_INCLUDE_DOCS=OFF \
        -DLLVM_ENABLE_ZSTD=OFF -DLLVM_ENABLE_ZLIB=OFF -DLLVM_ENABLE_LIBXML2=OFF -DLLVM_ENABLE_TERMINFO=OFF \
        -DLLVM_ENABLE_LIBEDIT=OFF \
        -DLLVM_PARALLEL_LINK_JOBS=2 \
        -DLLVM_DISTRIBUTION_COMPONENTS="clang;lld;clang-resource-headers;llvm-ar;llvm-ranlib;llvm-nm;llvm-strip;llvm-objcopy;llvm-objdump;llvm-readobj" \
        >"$build/llvm-configure.log"
    ninja -C "$build/llvm" -j"$jobs" install-distribution >"$build/llvm-build.log" 2>&1 \
        || { tail -n 30 "$build/llvm-build.log" >&2; exit 1; }
    ln -sf llvm-ar "$llvm/bin/ar"
    ln -sf llvm-ranlib "$llvm/bin/ranlib"
    ln -sf llvm-readobj "$llvm/bin/llvm-readelf"
    ln -sf clang "$llvm/bin/$TARGET-clang"
    ln -sf clang "$llvm/bin/$TARGET-clang++"
    echo "$LLVM_COMMIT" >"$llvm/commit"
fi
export PATH="$llvm/bin:$PATH"

echo "musl (${MUSL_COMMIT:0:10})"
checkout "$src/musl" "$MUSL_REPO" "$MUSL_COMMIT"
rm -rf "$sysroot"
mkdir -p "$sysroot"
rm -rf "$build/musl" && cp -r "$src/musl" "$build/musl" && rm -rf "$build/musl/.git"
cat >"$build/musl/config.mak" <<EOF
ARCH=wasm32
prefix=$sysroot
syslibdir=$sysroot/lib
EOF
make -C "$build/musl" -j"$jobs" >"$build/musl.log" 2>&1 || { tail -n 20 "$build/musl.log" >&2; exit 1; }
make -C "$build/musl" install-libs install-headers >>"$build/musl.log" 2>&1

echo "the kernel's userspace headers (${LINUX_COMMIT:0:10})"
checkout "$src/linux" "$LINUX_REPO" "$LINUX_COMMIT"
make -C "$src/linux" O="$build/linux-headers" ARCH=wasm LLVM=1 HOSTCC=/usr/bin/clang \
    INSTALL_HDR_PATH="$sysroot" headers_install >"$build/headers.log" 2>&1 \
    || { tail -n 20 "$build/headers.log" >&2; exit 1; }

echo "compiler-rt builtins"
# Its assembly with the exception-handling feature: that is what defines __c_longjmp, the
# tag setjmp and longjmp throw and catch.
cmake -S "$src/llvm-project/compiler-rt" -B "$build/compiler-rt" -G Ninja -Wno-dev \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_SYSTEM_NAME=Linux \
    -DCMAKE_SYSROOT="$sysroot" \
    -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY \
    -DCMAKE_C_COMPILER="$llvm/bin/clang" -DCMAKE_ASM_COMPILER="$llvm/bin/clang" \
    -DCMAKE_AR="$llvm/bin/llvm-ar" -DCMAKE_RANLIB="$llvm/bin/llvm-ranlib" -DCMAKE_NM="$llvm/bin/llvm-nm" \
    -DCMAKE_C_COMPILER_TARGET="$TARGET" -DCMAKE_ASM_COMPILER_TARGET="$TARGET" \
    -DCMAKE_C_FLAGS="$WASM_CFLAGS" \
    -DCMAKE_ASM_FLAGS=-mexception-handling \
    -DCOMPILER_RT_BUILD_BUILTINS=ON -DCOMPILER_RT_BUILD_CRT=OFF \
    -DCOMPILER_RT_BUILD_SANITIZERS=OFF -DCOMPILER_RT_BUILD_XRAY=OFF -DCOMPILER_RT_BUILD_LIBFUZZER=OFF \
    -DCOMPILER_RT_BUILD_PROFILE=OFF -DCOMPILER_RT_BUILD_MEMPROF=OFF -DCOMPILER_RT_BUILD_ORC=OFF \
    -DCOMPILER_RT_BUILD_CTX_PROFILE=OFF \
    -DCOMPILER_RT_DEFAULT_TARGET_ONLY=ON -DLLVM_ENABLE_PER_TARGET_RUNTIME_DIR=ON \
    -DCOMPILER_RT_INSTALL_PATH="$("$llvm/bin/clang" -print-resource-dir)" \
    >"$build/compiler-rt-configure.log"
ninja -C "$build/compiler-rt" -j"$jobs" install-builtins >"$build/compiler-rt.log" 2>&1 \
    || { tail -n 20 "$build/compiler-rt.log" >&2; exit 1; }

echo "compiler wrappers"
# What builds use as their compiler (CC=$out/bin/$TARGET-cc): the target, the sysroot and
# the flags every program needs, and, when linking a program (not compiling, not a
# partial link), how the kernel loads it: its
# memory imported, shared with its threads, with a maximum (each process reserves it:
# 256 MiB unless a program links its own: these go before the program's flags, and the
# linker keeps the last of each), its function table exported, an 8 MiB stack (and no
# warning that a link took no compile flags: Rust links through it too).
mkdir -p "$out/bin"
for tool in cc c++; do
    driver=clang
    [ "$tool" = c++ ] && driver=clang++
    cat >"$out/bin/$TARGET-$tool" <<WRAPPER
#!/bin/sh
link=yes
for arg; do
    case \$arg in -c|-S|-E|-M|-MM|-r|-Wl,-r|-Wl,--relocatable) link=no ;; esac
done
if [ \$link = yes ]; then
    exec "$llvm/bin/$driver" --target=$TARGET --sysroot="$sysroot" $WASM_CFLAGS \\
        -Wno-unused-command-line-argument -Wl,--import-memory,--shared-memory,--max-memory=268435456,--export-table,-z,stack-size=8388608 "\$@"
fi
exec "$llvm/bin/$driver" --target=$TARGET --sysroot="$sysroot" $WASM_CFLAGS "\$@"
WRAPPER
    chmod 755 "$out/bin/$TARGET-$tool"
done
for tool in ar ranlib nm strip objcopy objdump readelf; do
    ln -sf "$llvm/bin/llvm-$tool" "$out/bin/$TARGET-$tool"
done

echo "Toolchain for $TARGET in $llvm, sysroot in $sysroot, compilers $out/bin/$TARGET-cc and -c++"
