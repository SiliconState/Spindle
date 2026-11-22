#!/usr/bin/env bash
# Builds Spindle's wasm Linux for the extension: the kernel (build-kernel.sh's
# vmlinux.wasm, built with spindle.config) and its root filesystem in v86's format, as
# chrome/linux/build.sh makes the browser Linux's (fs.json, an index naming every file
# by its digest; flat/, the files by digest, fetched as the guest opens them), files and
# kernel compressed with zstd as the browser Linux's are: busybox (build-userland.sh),
# Python (build-python.sh's program, with the standard library of Alpine's python3 of the
# same version, byte for byte, its compiled modules with it: the browser Linux has those
# very files, so the extension carries them once), ripgrep (the browser Linux's version,
# built here with build-rust.sh's cargo: without PCRE2, whose JIT needs mmap, so `rg -P`
# runs on v86; ripgrep/no-mmap.c in place of mmap, which it reads files without),
# Spindle's agent and `spindle` command (guest/, built here too), its init and users. Its
# identity names
# it as runs record it: the index, the kernel and the runtime that boots it. Nothing
# needs root: Alpine's packages install as a normal user, and the files are compressed
# in that Alpine root in a user namespace.
#
# usage: chrome/wasm-linux/build-image.sh VMLINUX_WASM USERLAND_DIR RUST_DIR PYTHON_DIR [OUT_DIR]
#   USERLAND_DIR, RUST_DIR, PYTHON_DIR: build-userland.sh's, build-rust.sh's and
#   build-python.sh's OUT_DIR; OUT_DIR defaults to chrome/wasm-linux/dist (gitignored).
set -euo pipefail
umask 022

here=$(cd "$(dirname "$0")" && pwd)
kernel=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
userland=$(cd "$2" && pwd)
rust=$(cd "$3" && pwd)
python=$(cd "$4" && pwd)
out=${5:-$here/dist}

V86_COMMIT=cc37f35c43bf14865d74ad76879ecf9410f69eb1
# Alpine as chrome/linux/build.sh pins it (checked below): its python3 and zstd.
ALPINE=3.24.3
ALPINE_BRANCH=v3.24
MINIROOTFS_SHA256=a73bd9ed20a5ed1503a0688957add539e10340c7ca1d9e8c06c34a76ddc5f715
APK_STATIC=apk-tools-static-3.0.9-r0.apk
APK_STATIC_SHA256=0a1a0682375f66a83b7b903f6e4638ccfe312547d50d336b5f2be9fc391281be
# v86's tools at V86_COMMIT (chrome/linux/build.sh pins the same), by their sha256.
V86_FILES="tools/fs2json.py:546b0ce7d0b172fa855587209318a8d473bce6de23fa327ceeb5b6c115fda23f
tools/copy-to-sha256.py:7bfb94736afbb9753b6deea922184eec2f9b205be0214bc5f9866cf990bfa77c"
TARGET=wasm32-spindle-linux-musl
# The browser Linux's ripgrep (Alpine's package; checked below), from its release tarball.
RIPGREP=15.1.0
RIPGREP_SHA256=046fa01a216793b8bd2750f9d68d4ad43986eb9c0d6122600f993906012972e8

[ -f "$kernel" ] || { echo "no kernel at $kernel (build-kernel.sh)" >&2; exit 1; }
[ -x "$userland/root/bin/busybox" ] || { echo "no busybox in $userland/root (build-userland.sh)" >&2; exit 1; }
[ -x "$rust/bin/cargo-wasm-linux" ] || { echo "no Rust for $TARGET in $rust (build-rust.sh)" >&2; exit 1; }
[ -f "$python/VERSION" ] || { echo "no Python in $python (build-python.sh)" >&2; exit 1; }
PYTHON=$(cat "$python/VERSION")
minor=${PYTHON%.*}
[ -x "$python/root/usr/bin/python$minor" ] || { echo "no Python in $python/root (build-python.sh)" >&2; exit 1; }
for pin in "ALPINE=$ALPINE" "ALPINE_BRANCH=$ALPINE_BRANCH" "MINIROOTFS_SHA256=$MINIROOTFS_SHA256" "APK_STATIC=$APK_STATIC" "APK_STATIC_SHA256=$APK_STATIC_SHA256" "V86_COMMIT=$V86_COMMIT"; do
    grep -qxF "$pin" "$here/../linux/build.sh" || { echo "chrome/linux/build.sh does not pin $pin: pin the same Alpine and v86 in both" >&2; exit 1; }
done

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
check() { echo "$2  $1" | sha256sum -c --quiet -; }
# Runs a command in the Alpine root, as root of a user namespace (this machine's devices
# and processes seen through), in a clean environment.
in_alpine() {
    unshare -r -m sh -c 'mount --rbind /dev "$0/dev" && mount --rbind /proc "$0/proc" && exec chroot "$0" /usr/bin/env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/root TMPDIR=/tmp LANG=C "$@"' "$alpine" "$@"
}

echo "v86's filesystem tools (commit ${V86_COMMIT:0:7})"
mkdir -p "$work/v86src"
for entry in $V86_FILES; do
    file=${entry%%:*}
    fetch "https://raw.githubusercontent.com/copy/v86/$V86_COMMIT/$file" "$work/v86src/$(basename "$file")"
    check "$work/v86src/$(basename "$file")" "${entry#*:}"
done

echo "Alpine $ALPINE's python3 (its standard library) and zstd"
fetch "https://dl-cdn.alpinelinux.org/alpine/$ALPINE_BRANCH/releases/x86/alpine-minirootfs-$ALPINE-x86.tar.gz" "$work/minirootfs.tar.gz"
check "$work/minirootfs.tar.gz" "$MINIROOTFS_SHA256"
fetch "https://dl-cdn.alpinelinux.org/alpine/$ALPINE_BRANCH/main/x86_64/$APK_STATIC" "$work/apk-static.apk"
check "$work/apk-static.apk" "$APK_STATIC_SHA256"
alpine=$work/alpine
mkdir -p "$work/apk" "$alpine" "$work/cache"
tar xzf "$work/apk-static.apk" -C "$work/apk" sbin/apk.static 2>/dev/null
tar xzf "$work/minirootfs.tar.gz" -C "$alpine" --no-same-owner
# Packages are checked against the keys the minirootfs carries (the browser Linux has
# python3 and python3-pyc, its compiled modules).
"$work/apk/sbin/apk.static" --root "$alpine" --arch x86 --usermode --no-interactive \
    --update-cache --cache-dir "$work/cache" add python3 python3-pyc ripgrep zstd >/dev/null
installed=$("$work/apk/sbin/apk.static" --root "$alpine" info -v 2>/dev/null)
alpine_python=$(echo "$installed" | sed -n 's/^python3-\([0-9][0-9.]*\)-r[0-9]*$/\1/p')
[ "$alpine_python" = "$PYTHON" ] || { echo "Alpine's python3 is $alpine_python, build-python.sh's $PYTHON: build $alpine_python (PYTHON there; the two Linuxes run one Python)" >&2; exit 1; }
alpine_ripgrep=$(echo "$installed" | sed -n 's/^ripgrep-\([0-9][0-9.]*\)-r[0-9]*$/\1/p')
[ "$alpine_ripgrep" = "$RIPGREP" ] || { echo "Alpine's ripgrep is $alpine_ripgrep, this script's $RIPGREP: pin $alpine_ripgrep (RIPGREP; the two Linuxes run one ripgrep)" >&2; exit 1; }

echo "Spindle's agent and command ($TARGET)"
# Paths of the build machine stay out of them (the crates', Spindle's, the Rust library's
# that std is built from, the build's own): the same sources give the same programs.
chrome=$(cd "$here/.." && pwd)
RUSTFLAGS="--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}/registry/src=/cargo --remap-path-prefix=$chrome=/spindle-chrome --remap-path-prefix=$rust=/rust --remap-path-prefix=$work=/build" \
    "$rust/bin/cargo-wasm-linux" build --quiet --locked --release \
    --manifest-path "$here/guest/Cargo.toml" --target-dir "$work/guest" 2>&1 \
    | grep -v 'was not used in the crate graph\|Check that the patched package\|with the dependency requirements\|what is locked in the Cargo.lock\|version. This may also occur' >&2 || true
built=$work/guest/$TARGET/release
[ -x "$built/spindle-agent" ] && [ -x "$built/spindle" ] || { echo "the guest's programs did not build" >&2; exit 1; }

echo "ripgrep $RIPGREP ($TARGET)"
fetch "https://github.com/BurntSushi/ripgrep/archive/$RIPGREP/ripgrep-$RIPGREP.tar.gz" "$work/ripgrep.tar.gz"
check "$work/ripgrep.tar.gz" "$RIPGREP_SHA256"
tar xzf "$work/ripgrep.tar.gz" -C "$work"
ripgrep=$work/ripgrep-$RIPGREP
# Its crates take build-rust.sh's libc (this target's definitions), as the guest's do: its
# lock names libc's version from crates.io, which cargo-wasm-linux's patch must replace.
libc=$(echo "$rust"/toolchain/lib/rustlib/src/rust/library/vendor/libc-0.*)
[ -d "$libc" ] || { echo "no single patched libc in $rust (build-rust.sh)" >&2; exit 1; }
"$rust/toolchain/bin/cargo" update --quiet --manifest-path "$ripgrep/Cargo.toml" -p libc --precise "${libc##*-}" \
    --config "patch.crates-io.libc.path=\"$libc\""
cc=$(sed -n 's/^ *"linker": "\(.*\)",$/\1/p' "$rust/$TARGET.json")
"$cc" -O2 -c "$here/ripgrep/no-mmap.c" -o "$work/no-mmap.o"
RUSTFLAGS="--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}/registry/src=/cargo --remap-path-prefix=$rust=/rust --remap-path-prefix=$work=/build -C link-arg=$work/no-mmap.o" \
    "$rust/bin/cargo-wasm-linux" build --quiet --locked --profile release-lto \
    --manifest-path "$ripgrep/Cargo.toml" --target-dir "$work/ripgrep-target"
[ -x "$work/ripgrep-target/$TARGET/release-lto/rg" ] || { echo "ripgrep did not build" >&2; exit 1; }

echo "root filesystem"
root=$work/root
cp -a "$userland/root" "$root"
cp -a "$python/root/." "$root/"
# The standard library: Alpine's, as its package has it (its files keep their times, which
# its compiled modules check), but for what describes this Python's own build
# (_sysconfigdata, _sysconfig_vars, build-details.json), what has no use here (lib-dynload's
# x86 modules: every C module is in the program; the build's config directory) and what
# Alpine leaves out that this Python has (kept, compiled as build-python.sh compiled it).
lib=$root/usr/lib/python$minor
mv "$lib" "$work/own-lib"
cp -a "$alpine/usr/lib/python$minor" "$lib"
rm -rf "$lib"/lib-dynload "$lib"/config-"$minor"-* "$lib"/_sysconfigdata_* "$lib"/_sysconfig_vars_* \
    "$lib"/build-details.json "$lib"/__pycache__/_sysconfigdata_*
(cd "$lib" && find . -type f | sort) > "$work/alpine-files"
mkdir "$lib/lib-dynload"
(cd "$work/own-lib" && find . -type f | sort) | while read -r file; do
    case $file in
        ./lib-dynload/*) ;;
        ./_sysconfigdata_*|./_sysconfig_vars_*|./build-details.json|./__pycache__/_sysconfigdata_*) install -D -m 644 "$work/own-lib/$file" "$lib/$file" ;;
        *) [ -e "$lib/$file" ] || { install -D -m 644 "$work/own-lib/$file" "$lib/$file"; echo "${file#./}" >> "$work/own-only"; } ;;
    esac
done
[ ! -s "$work/own-only" ] || echo "  kept from build-python.sh (not in Alpine's package): $(wc -l < "$work/own-only") files, $(head -n 3 "$work/own-only" | tr '\n' ' ')..."
mkdir -p "$root"/{dev,proc,sys,root,run,etc,workspace,usr/local/bin,var}
install -m 755 "$here/guest/init" "$root/sbin/init"
install -m 644 "$here/guest/passwd" "$root/etc/passwd"
install -m 644 "$here/guest/group" "$root/etc/group"
install -m 755 "$built/spindle-agent" "$root/usr/local/bin/spindle-agent"
install -m 755 "$built/spindle" "$root/usr/local/bin/spindle"
install -m 755 "$work/ripgrep-target/$TARGET/release-lto/rg" "$root/usr/bin/rg"
# Spindle's calls for Python scripts (the browser Linux's module, one source), compiled
# ahead as the rest of the library is.
install -m 644 "$here/../linux/guest/spindle.py" "$lib/site-packages/spindle.py"
# Named by their paths in the guest (not this machine's), so the same sources give the
# same files.
"$python/build/python-native/python" -m compileall -q --invalidation-mode unchecked-hash \
    -s "$root" -p / "$lib/site-packages" >/dev/null
mkdir -p "$root/tmp" "$root/var/tmp" "$root/home/spindle" "$root/runs"
# Owners and modes as the guest needs them, one fixed time and one order: the index
# depends on the contents only. Paths relative to the root (no ./): fs2json makes the
# tar's top level the root.
# Alpine's library files keep their times.
python3 - "$root" "$work/rootfs.tar" "$work/alpine-files" "usr/lib/python$minor" <<'PY'
import os, sys, tarfile
root, out, alpine_files, lib = sys.argv[1:5]
modes = {'tmp': 0o1777, 'var/tmp': 0o1777, 'runs': 0o700, 'home/spindle': 0o755}
owners = {'home/spindle': 1000}
with open(alpine_files) as listed:
    timed = {os.path.normpath(os.path.join(lib, line.strip())) for line in listed if line.strip()}
with tarfile.open(out, 'w', format=tarfile.GNU_FORMAT) as tar:
    for top, dirs, files in os.walk(root):
        dirs.sort()
        for name in sorted(dirs) + sorted(files):
            path = os.path.join(top, name)
            rel = os.path.relpath(path, root)
            info = tar.gettarinfo(path, rel)
            info.uid = info.gid = owners.get(rel, 0)
            info.uname = info.gname = ''
            if rel not in timed:
                info.mtime = 0
            if rel in modes:
                info.mode = modes[rel]
            if info.isreg():
                with open(path, 'rb') as data:
                    tar.addfile(info, data)
            else:
                tar.addfile(info)
PY
# Each file compressed with zstd (level 19; the index names them .bin.zst), and the kernel
# (the frame says its size), in the Alpine root.
mkdir -p "$alpine/build/flat"
mv "$work/rootfs.tar" "$alpine/build/rootfs.tar"
cp "$work/v86src/fs2json.py" "$work/v86src/copy-to-sha256.py" "$alpine/build/"
cp "$kernel" "$alpine/build/vmlinux.wasm"
in_alpine python3 /build/fs2json.py --zstd --out /build/fs.json /build/rootfs.tar >/dev/null
in_alpine python3 /build/copy-to-sha256.py /build/rootfs.tar /build/flat >/dev/null 2>&1
in_alpine sh -c 'cd /build/flat && ls | xargs -P "$0" -n 16 zstd -q -19 --rm && zstd -q -19 --rm /build/vmlinux.wasm' "$(nproc)"
rm -rf "$out"
mkdir -p "$out"
mv "$alpine/build/fs.json" "$alpine/build/flat" "$alpine/build/vmlinux.wasm.zst" "$out/"
echo "$PYTHON" > "$out/python-version"
# The sandbox's identity, as runs record it (identity.sh).
"$here/identity.sh" "$out" > "$out/identity"
echo "wasm Linux in $out ($(du -sh "$out" | cut -f1), $(find "$out/flat" -type f | wc -l) files)"
