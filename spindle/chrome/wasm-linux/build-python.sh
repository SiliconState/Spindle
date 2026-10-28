#!/usr/bin/env bash
# Builds CPython for Spindle's wasm Linux with its toolchain (build-toolchain.sh): Python
# 3.13 (PSF license) cross-compiled for wasm32-unknown-linux-musl, every C module linked
# into the one program (there is no dlopen here), with zlib, bzip2 and SQLite (built here
# too); subprocess starts children with posix_spawn (python/subprocess-spawn.patch: there
# is no fork); no ctypes, ssl, readline or curses, and pymalloc rather than mimalloc
# (which needs mprotect; with no mmap here pymalloc takes its arenas from malloc). A
# native Python of the same version is
# built first (a cross build needs one; it also compiles the library). From release
# tarballs pinned by digest; nothing needs root.
#
# usage: chrome/wasm-linux/build-python.sh TOOLCHAIN_DIR OUT_DIR
#   OUT_DIR gets root/ (usr/bin/python3, usr/lib/python3.13: the standard library compiled
#   ahead, without its tests, IDLE, Tk and the like); sources and builds go in OUT_DIR/src
#   and OUT_DIR/build.
set -euo pipefail
umask 022

here=$(cd "$(dirname "$0")" && pwd)
toolchain=$(cd "$1" && pwd)
out=$(mkdir -p "$2" && cd "$2" && pwd)

TARGET=wasm32-unknown-linux-musl
PYTHON=3.13.18
PYTHON_SHA256=abb0102e6d4ae027509bcb82327d9dd85515b44a3ba8ad538cfc5e9847029f68
ZLIB=1.3.2
ZLIB_SHA256=bb329a0a2cd0274d05519d61c667c062e06990d72e125ee2dfa8de64f0119d16
BZIP2=1.0.8
BZIP2_SHA256=ab5a03176ee106d3f0fa90e381da478ddae405918153cca248e682cd0c4a2269
SQLITE=3540100
SQLITE_YEAR=2026
SQLITE_SHA256=926ef8d3eab93fbf6e06011901273eec076b25594a4e1cdadd50c987f4f6d64e

cc=$toolchain/bin/$TARGET-cc
[ -x "$cc" ] || { echo "no toolchain in $toolchain (build-toolchain.sh)" >&2; exit 1; }
export PATH="$toolchain/bin:$toolchain/llvm/bin:/usr/local/bin:/usr/bin:/bin"
jobs=$(nproc)
src=$out/src
build=$out/build
deps=$build/deps
root=$out/root
mkdir -p "$src" "$build" "$deps/include" "$deps/lib"

fetch() {
    local url=$1 file=$src/$(basename "$1") sha=$2
    [ -f "$file" ] || curl -fsSL --retry 3 -o "$file" "$url"
    echo "$sha  $file" | sha256sum -c --quiet -
}
unpack() { rm -rf "$build/$2" && mkdir -p "$build/$2" && tar xf "$src/$1" -C "$build/$2" --strip-components=1; }

fetch "https://www.python.org/ftp/python/$PYTHON/Python-$PYTHON.tar.xz" "$PYTHON_SHA256"
fetch "https://zlib.net/zlib-$ZLIB.tar.gz" "$ZLIB_SHA256"
fetch "https://sourceware.org/pub/bzip2/bzip2-$BZIP2.tar.gz" "$BZIP2_SHA256"
fetch "https://www.sqlite.org/$SQLITE_YEAR/sqlite-autoconf-$SQLITE.tar.gz" "$SQLITE_SHA256"

echo "zlib $ZLIB"
unpack "zlib-$ZLIB.tar.gz" zlib
(cd "$build/zlib" && CC="$cc" CFLAGS=-O2 AR="$TARGET-ar" RANLIB="$TARGET-ranlib" ./configure --static --prefix="$deps" >../zlib.log 2>&1 \
    && make -j"$jobs" libz.a >>../zlib.log 2>&1 && make install >>../zlib.log 2>&1) || { tail -n 20 "$build/zlib.log" >&2; exit 1; }

echo "bzip2 $BZIP2"
unpack "bzip2-$BZIP2.tar.gz" bzip2
make -C "$build/bzip2" -j"$jobs" libbz2.a CC="$cc" AR="$TARGET-ar" RANLIB="$TARGET-ranlib" CFLAGS="-O2 -D_FILE_OFFSET_BITS=64" >"$build/bzip2.log" 2>&1 \
    || { tail -n 20 "$build/bzip2.log" >&2; exit 1; }
cp "$build/bzip2/libbz2.a" "$deps/lib/" && cp "$build/bzip2/bzlib.h" "$deps/include/"

echo "SQLite $SQLITE"
unpack "sqlite-autoconf-$SQLITE.tar.gz" sqlite
# The amalgamation alone: one file, no configure needed. Threads as Python uses them;
# nothing loaded at run time (no dlopen); no mmap here, so no memory-mapped I/O and no
# write-ahead log (its index is shared memory).
"$cc" -O2 -c "$build/sqlite/sqlite3.c" -o "$build/sqlite/sqlite3.o" \
    -DSQLITE_THREADSAFE=1 -DSQLITE_OMIT_LOAD_EXTENSION -DSQLITE_ENABLE_FTS5 -DSQLITE_ENABLE_MATH_FUNCTIONS \
    -DSQLITE_OMIT_WAL -DSQLITE_MAX_MMAP_SIZE=0
"$TARGET-ar" rcs "$deps/lib/libsqlite3.a" "$build/sqlite/sqlite3.o"
cp "$build/sqlite/sqlite3.h" "$build/sqlite/sqlite3ext.h" "$deps/include/"

echo "Python $PYTHON (native, for the build)"
unpack "Python-$PYTHON.tar.xz" python
patch -s -p1 -d "$build/python" <"$here/python/subprocess-spawn.patch"
mkdir -p "$build/python-native"
if [ ! -x "$build/python-native/python" ] || [ "$("$build/python-native/python" -c 'import platform; print(platform.python_version())')" != "$PYTHON" ]; then
    (cd "$build/python-native" && ../python/configure --disable-test-modules --without-ensurepip >configure.log 2>&1 \
        && make -j"$jobs" >make.log 2>&1) \
        || { tail -n 20 "$build/python-native/configure.log" $([ -f "$build/python-native/make.log" ] && echo "$build/python-native/make.log") >&2; exit 1; }
fi
native=$build/python-native/python

echo "Python $PYTHON ($TARGET)"
rm -rf "$build/python-target" && mkdir -p "$build/python-target"
# What configure cannot learn by running programs for the target, and what it would get
# wrong: musl's sys/mman.h is there, but there is no mmap (nor dlopen) here.
cat >"$build/python-target/config.site" <<'SITE'
ac_cv_file__dev_ptmx=yes
ac_cv_file__dev_ptc=no
ac_cv_buggy_getaddrinfo=no
ac_cv_func_dlopen=no
ac_cv_lib_dl_dlopen=no
ac_cv_header_sys_mman_h=no
py_cv_module__posixshmem=n/a
SITE
# pkg-config sees the target's libraries alone (the build machine's would pass for them).
mkdir -p "$deps/lib/pkgconfig"
(cd "$build/python-target" && CONFIG_SITE=config.site PKG_CONFIG_LIBDIR="$deps/lib/pkgconfig" PKG_CONFIG_PATH= ../python/configure \
    --host="$TARGET" --build="$(../python/config.guess)" --with-build-python="$native" \
    --prefix=/usr --disable-shared --disable-test-modules --without-ensurepip --disable-ipv6 --without-mimalloc \
    CC="$cc" AR="$TARGET-ar" RANLIB="$TARGET-ranlib" READELF="$TARGET-readelf" CFLAGS=-O2 \
    ZLIB_CFLAGS="-I$deps/include" ZLIB_LIBS="-L$deps/lib -lz" \
    BZIP2_CFLAGS="-I$deps/include" BZIP2_LIBS="-L$deps/lib -lbz2" \
    LIBSQLITE3_CFLAGS="-I$deps/include" LIBSQLITE3_LIBS="-L$deps/lib -lsqlite3" \
    >configure.log 2>&1 && make -j"$jobs" >make.log 2>&1) \
    || { tail -n 30 "$build/python-target/configure.log" "$build/python-target/make.log" >&2; exit 1; }

echo "its install"
rm -rf "$root"
make -C "$build/python-target" DESTDIR="$root" install >"$build/python-install.log" 2>&1 \
    || { tail -n 20 "$build/python-install.log" >&2; exit 1; }
lib=$root/usr/lib/python3.13
# What a command has no use for here: tests, IDLE, Tk, the static library and build
# files, what installs pip (no network), what is gone already in 3.13's own sense.
rm -rf "$lib"/test "$lib"/idlelib "$lib"/tkinter "$lib"/turtledemo "$lib"/turtle.py "$lib"/ensurepip \
    "$lib"/lib2to3 "$lib"/pydoc_data "$lib"/config-3.13* "$root"/usr/lib/pkgconfig "$root"/usr/include \
    "$root"/usr/lib/libpython3.13.a "$root"/usr/share "$root"/usr/bin/idle3* "$root"/usr/bin/pydoc3* "$root"/usr/bin/python3*-config
find "$lib" -name __pycache__ -prune -exec rm -rf {} +
# Compiled ahead and never checked against the sources' times (the image gives every
# file one fixed time, and the library is read-only to commands), named by their paths in
# the guest, not this machine's (the same sources give the same files).
"$native" -m compileall -q -j0 --invalidation-mode unchecked-hash -s "$root" -p / "$lib" >/dev/null
# Its debug information goes (most of its size).
"$TARGET-strip" "$root/usr/bin/python3.13"
ln -sf python3.13 "$root/usr/bin/python3"
ln -sf python3 "$root/usr/bin/python"
echo "Python in $root ($(du -sh "$root" | cut -f1); the program $(du -h "$root/usr/bin/python3.13" | cut -f1))"
