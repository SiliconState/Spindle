#!/usr/bin/env bash
# Builds Spindle's wasm Linux for the extension: the kernel (build-kernel.sh's
# vmlinux.wasm, built with spindle.config) and its root filesystem in v86's format, as
# chrome/linux/build.sh makes the browser Linux's (fs.json, an index naming every file
# by its digest; flat/, the files by digest, fetched as the guest opens them): busybox
# (build-userland.sh), Python (build-python.sh), Spindle's agent and `spindle` command
# (guest/, built here with build-rust.sh's cargo), its init and users. Its identity names
# it as runs record it:
# the index, the kernel and the runtime that boots it. Nothing needs root.
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
# v86's tools at V86_COMMIT (chrome/linux/build.sh pins the same), by their sha256.
V86_FILES="tools/fs2json.py:546b0ce7d0b172fa855587209318a8d473bce6de23fa327ceeb5b6c115fda23f
tools/copy-to-sha256.py:7bfb94736afbb9753b6deea922184eec2f9b205be0214bc5f9866cf990bfa77c"
TARGET=wasm32-spindle-linux-musl

[ -f "$kernel" ] || { echo "no kernel at $kernel (build-kernel.sh)" >&2; exit 1; }
[ -x "$userland/root/bin/busybox" ] || { echo "no busybox in $userland/root (build-userland.sh)" >&2; exit 1; }
[ -x "$rust/bin/cargo-wasm-linux" ] || { echo "no Rust for $TARGET in $rust (build-rust.sh)" >&2; exit 1; }
[ -x "$python/root/usr/bin/python3.13" ] || { echo "no Python in $python/root (build-python.sh)" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
check() { echo "$2  $1" | sha256sum -c --quiet -; }

echo "v86's filesystem tools (commit ${V86_COMMIT:0:7})"
mkdir -p "$work/v86src"
for entry in $V86_FILES; do
    file=${entry%%:*}
    fetch "https://raw.githubusercontent.com/copy/v86/$V86_COMMIT/$file" "$work/v86src/$(basename "$file")"
    check "$work/v86src/$(basename "$file")" "${entry#*:}"
done

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

echo "root filesystem"
root=$work/root
cp -a "$userland/root" "$root"
cp -a "$python/root/." "$root/"
mkdir -p "$root"/{dev,proc,sys,root,run,etc,workspace,usr/local/bin,var}
install -m 755 "$here/guest/init" "$root/sbin/init"
install -m 644 "$here/guest/passwd" "$root/etc/passwd"
install -m 644 "$here/guest/group" "$root/etc/group"
install -m 755 "$built/spindle-agent" "$root/usr/local/bin/spindle-agent"
install -m 755 "$built/spindle" "$root/usr/local/bin/spindle"
# Spindle's calls for Python scripts (the browser Linux's module, one source), compiled
# ahead as the rest of the library is.
install -m 644 "$here/../linux/guest/spindle.py" "$root/usr/lib/python3.13/site-packages/spindle.py"
# Named by their paths in the guest (not this machine's), so the same sources give the
# same files.
"$python/build/python-native/python" -m compileall -q --invalidation-mode unchecked-hash \
    -s "$root" -p / "$root/usr/lib/python3.13/site-packages" >/dev/null
mkdir -p "$root/tmp" "$root/var/tmp" "$root/home/spindle" "$root/runs"
# Owners and modes as the guest needs them, one fixed time and one order: the index
# depends on the contents only. Paths relative to the root (no ./): fs2json makes the
# tar's top level the root.
python3 - "$root" "$work/rootfs.tar" <<'PY'
import os, sys, tarfile
root, out = sys.argv[1], sys.argv[2]
modes = {'tmp': 0o1777, 'var/tmp': 0o1777, 'runs': 0o700, 'home/spindle': 0o755}
owners = {'home/spindle': 1000}
with tarfile.open(out, 'w', format=tarfile.GNU_FORMAT) as tar:
    for top, dirs, files in os.walk(root):
        dirs.sort()
        for name in sorted(dirs) + sorted(files):
            path = os.path.join(top, name)
            rel = os.path.relpath(path, root)
            info = tar.gettarinfo(path, rel)
            info.uid = info.gid = owners.get(rel, 0)
            info.uname = info.gname = ''
            info.mtime = 0
            if rel in modes:
                info.mode = modes[rel]
            if info.isreg():
                with open(path, 'rb') as data:
                    tar.addfile(info, data)
            else:
                tar.addfile(info)
PY
rm -rf "$out"
mkdir -p "$out/flat"
python3 "$work/v86src/fs2json.py" --out "$out/fs.json" "$work/rootfs.tar" >/dev/null
python3 "$work/v86src/copy-to-sha256.py" "$work/rootfs.tar" "$out/flat" >/dev/null 2>&1
cp "$kernel" "$out/vmlinux.wasm"
# The sandbox's identity, as runs record it: the index (which names every file by its
# digest), the kernel, and the runtime that boots it and serves its devices.
cat "$out/fs.json" "$out/vmlinux.wasm" "$here"/*.mjs "$here"/v86/lib/*.js "$here"/v86/src/*.js "$here"/v86/src/browser/*.js \
    | sha256sum | cut -c1-64 > "$out/identity"
echo "wasm Linux in $out ($(du -sh "$out" | cut -f1), $(find "$out/flat" -type f | wc -l) files)"
