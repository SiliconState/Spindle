#!/usr/bin/env bash
# Builds the Linux that Spindle for Browser runs bash in: Alpine (32-bit x86) with bash,
# coreutils, Python and pip, Node and npm, git and curl, the time zones, booted by v86 (an
# x86 emulator in wasm) over its 9p filesystem, with Spindle's runner agent and web proxy
# as its only services (guest/), its `spindle` command and `spindle-sandbox` (which puts
# each command in namespaces of its own), static Rust programs (command/) built for the
# image with Rust's i686-unknown-linux-musl target. The kernel is built here: Alpine's virt
# configuration with kernel.config's changes (no modules, no boot image: v86 boots it
# straight onto the 9p root), compressed with xz. The image's files are compressed with
# zstd (v86 reads them so). Nothing needs root: Alpine's static apk installs as a normal
# user, and the kernel is built and the files compressed in an Alpine root of their own
# (tools: the same compilers wherever it is built) in a user namespace. Downloads are
# pinned (versions, checksums, commits); Alpine's packages are the branch's current ones,
# checked against its signing keys (manifest.txt lists the image's). What the build itself
# writes gets a fixed time (packages keep their own, which Python's compiled modules
# check), so the same packages give the same identity.
#
# usage: chrome/linux/build.sh [OUT_DIR]     (run from the spindle directory;
#                                             OUT_DIR defaults to chrome/linux/dist)
set -euo pipefail
# What the build writes, with modes that do not depend on who runs it.
umask 022

out=${1:-chrome/linux/dist}
here=$(cd "$(dirname "$0")" && pwd)

ALPINE=3.24.3
ALPINE_BRANCH=v3.24
MINIROOTFS_SHA256=a73bd9ed20a5ed1503a0688957add539e10340c7ca1d9e8c06c34a76ddc5f715
APK_STATIC=apk-tools-static-3.0.9-r0.apk
APK_STATIC_SHA256=0a1a0682375f66a83b7b903f6e4638ccfe312547d50d336b5f2be9fc391281be
V86_VERSION=0.5.472
V86_COMMIT=cc37f35c43bf14865d74ad76879ecf9410f69eb1
V86_TGZ_SHA256=4698fae3f518ad0a6f78d31e8f6f8802ff272a86f07b603b95f6c9fdd428e4fc
# Files of the v86 repository at V86_COMMIT, by their sha256.
V86_FILES="bios/seabios.bin:73e3f359102e3a9982c35fce98eb7cd08f18303ac7f1ba6ebfbe6cdc1c244d98
bios/vgabios.bin:a4bc0d80cc3ca028c73dafa8fee396b8d054ce87ebd8abfbd31b06b437607880
tools/fs2json.py:546b0ce7d0b172fa855587209318a8d473bce6de23fa327ceeb5b6c115fda23f
tools/copy-to-sha256.py:7bfb94736afbb9753b6deea922184eec2f9b205be0214bc5f9866cf990bfa77c"
PACKAGES="bash coreutils findutils grep sed gawk diffutils patch tar gzip xz python3 py3-pip nodejs npm git curl openssl jq ripgrep file less tzdata"
KERNEL=6.18.64
KERNEL_SHA256=020d45510c539972e6f14c5c71566878d3b7ef56fb082b3e09134f2ebaade562
# Alpine's configuration for its virt kernel (main/linux-lts/virt.x86.config) at an aports
# commit, by its sha256; kernel.config says what Spindle changes in it.
APORTS_COMMIT=d65a2d332a8ba2153c1c732411ddd44f8dd2beb8
KERNEL_CONFIG_SHA256=e25b0907879f6292d3e7805390fb2e6d475111c9583885232c39e8a449ff44e2
# The tools root: what builds the kernel (coreutils for a `date` that reads the fixed
# build time: busybox's does not, and the kernel's own small initramfs would take the
# time of the build), and what makes v86's index and compresses the files (Python for
# v86's tools, zstd).
TOOLS="build-base bash bc bison flex perl linux-headers elfutils-dev openssl-dev coreutils python3 xz zstd diffutils findutils"

# The command is built here (its target, once: rustup target add i686-unknown-linux-musl).
[ -d "$(rustc --print sysroot)/lib/rustlib/i686-unknown-linux-musl" ] \
    || { echo "Rust's i686-unknown-linux-musl target is missing: rustup target add i686-unknown-linux-musl" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
touch "$work/started"
fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
check() { echo "$2  $1" | sha256sum -c --quiet -; }
# Runs a command in the tools root, as root of a user namespace (with this machine's
# devices and processes seen through), in a clean environment.
in_tools() {
    unshare -r -m sh -c 'mount --rbind /dev "$0/dev" && mount --rbind /proc "$0/proc" && exec chroot "$0" /usr/bin/env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/root TMPDIR=/tmp LANG=C "$@"' "$tools" "$@"
}

echo "v86 $V86_VERSION (commit ${V86_COMMIT:0:7})"
(cd "$work" && npm pack "v86@$V86_VERSION" --silent >/dev/null)
check "$work/v86-$V86_VERSION.tgz" "$V86_TGZ_SHA256"
mkdir "$work/v86" && tar xzf "$work/v86-$V86_VERSION.tgz" -C "$work/v86"
grep -q "\"version\": \"$V86_VERSION+g${V86_COMMIT:0:7}\"" "$work/v86/package/package.json" \
    || { echo "v86 $V86_VERSION is not built from commit ${V86_COMMIT:0:7}" >&2; exit 1; }
mkdir -p "$work/v86src"
for entry in $V86_FILES; do
    file=${entry%%:*}
    fetch "https://raw.githubusercontent.com/copy/v86/$V86_COMMIT/$file" "$work/v86src/$(basename "$file")"
    check "$work/v86src/$(basename "$file")" "${entry#*:}"
done

echo "Alpine $ALPINE (x86)"
releases="https://dl-cdn.alpinelinux.org/alpine/$ALPINE_BRANCH/releases/x86"
fetch "$releases/alpine-minirootfs-$ALPINE-x86.tar.gz" "$work/minirootfs.tar.gz"
check "$work/minirootfs.tar.gz" "$MINIROOTFS_SHA256"
fetch "https://dl-cdn.alpinelinux.org/alpine/$ALPINE_BRANCH/main/x86_64/$APK_STATIC" "$work/apk-static.apk"
check "$work/apk-static.apk" "$APK_STATIC_SHA256"
mkdir -p "$work/apk" "$work/root" "$work/tools" "$work/cache"
tar xzf "$work/apk-static.apk" -C "$work/apk" sbin/apk.static 2>/dev/null
tar xzf "$work/minirootfs.tar.gz" -C "$work/root" --no-same-owner
tar xzf "$work/minirootfs.tar.gz" -C "$work/tools" --no-same-owner
root=$work/root
tools=$work/tools
# Packages are checked against the keys the minirootfs carries.
"$work/apk/sbin/apk.static" --root "$root" --arch x86 --usermode --no-interactive \
    --update-cache --cache-dir "$work/cache" add $PACKAGES >/dev/null
"$work/apk/sbin/apk.static" --root "$tools" --arch x86 --usermode --no-interactive \
    --update-cache --cache-dir "$work/cache" add $TOOLS >/dev/null

echo "Linux $KERNEL (Alpine's virt configuration, aports ${APORTS_COMMIT:0:10}, with kernel.config)"
fetch "https://cdn.kernel.org/pub/linux/kernel/v${KERNEL%%.*}.x/linux-$KERNEL.tar.xz" "$work/linux.tar.xz"
check "$work/linux.tar.xz" "$KERNEL_SHA256"
fetch "https://gitlab.alpinelinux.org/alpine/aports/-/raw/$APORTS_COMMIT/main/linux-lts/virt.x86.config" "$work/virt.x86.config"
check "$work/virt.x86.config" "$KERNEL_CONFIG_SHA256"
mkdir -p "$tools/build"
tar xJf "$work/linux.tar.xz" -C "$tools/build"
mv "$tools/build/linux-$KERNEL" "$tools/build/linux"
cp "$work/virt.x86.config" "$tools/build/linux/.config"
cp "$here/kernel.config" "$tools/build/spindle.config"
# For 32-bit x86, with no trace of when, by whom or where it was built. With no modules
# (kernel.config), whatever Alpine builds as one is left out.
cat > "$tools/build/kernel.sh" <<'KERNEL_BUILD'
set -e
cd /build/linux
export ARCH=i386 KBUILD_BUILD_TIMESTAMP='1970-01-01 00:00:00 UTC' KBUILD_BUILD_USER=spindle KBUILD_BUILD_HOST=spindle KBUILD_BUILD_VERSION=1
scripts/kconfig/merge_config.sh -m .config /build/spindle.config >/dev/null
make olddefconfig >/dev/null
sed -i 's/^\(CONFIG_[A-Za-z0-9_]*\)=m$/# \1 is not set/' .config
scripts/config --disable MODULES
make olddefconfig >/dev/null
make -j"$1" bzImage >/build/kernel.log 2>&1 || { tail -n 40 /build/kernel.log >&2; exit 1; }
KERNEL_BUILD
in_tools /bin/sh /build/kernel.sh "$(nproc)"
# What kernel.config asks for is what the kernel got.
config=$tools/build/linux/.config
grep -qx '# CONFIG_MODULES is not set' "$config" || { echo "the kernel kept modules" >&2; exit 1; }
grep '^CONFIG_' "$here/kernel.config" | while read -r want; do
    grep -qxF "$want" "$config" || { echo "the kernel's configuration lacks $want" >&2; exit 1; }
done
grep '^# CONFIG_.* is not set$' "$here/kernel.config" | cut -d' ' -f2 | while read -r off; do
    ! grep -q "^$off=" "$config" || { echo "the kernel's configuration has $off" >&2; exit 1; }
done
cp "$tools/build/linux/arch/x86/boot/bzImage" "$work/bzImage"
rm -rf "$tools/build/linux"

echo "Spindle's command ($(rustc --version | cut -d' ' -f2))"
# Paths of the build machine stay out of it: the same sources give the same program.
command_src=$here/command
RUSTFLAGS="-C linker=rust-lld --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}/registry/src=/cargo --remap-path-prefix=$command_src=/spindle-command" \
    cargo build --quiet --locked --release --target i686-unknown-linux-musl \
    --manifest-path "$command_src/Cargo.toml" --target-dir "$work/command"

echo "Spindle's guest"
cp "$here/guest/inittab" "$root/etc/inittab"
# Commands run as `spindle` (uid 1000; its home is fresh for each command).
echo 'spindle:x:1000:1000:Spindle:/home/spindle:/bin/bash' >> "$root/etc/passwd"
echo 'spindle:x:1000:' >> "$root/etc/group"
echo 'spindle:!::0:::::' >> "$root/etc/shadow"
mkdir -p "$root/home/spindle"
install -D -m 755 "$here/guest/sysinit" "$root/etc/spindle/sysinit"
install -D -m 644 "$here/guest/agent.py" "$root/opt/spindle/agent.py"
install -D -m 644 "$here/guest/proxy.py" "$root/opt/spindle/proxy.py"
install -D -m 644 "$here/guest/openssl.cnf" "$root/etc/spindle/openssl.cnf"
# Spindle's calls for scripts: the command, and the Python and Node modules.
install -D -m 755 "$work/command/i686-unknown-linux-musl/release/spindle" "$root/usr/local/bin/spindle"
# What puts each command in namespaces of its own (the agent starts it, as root).
install -D -m 755 "$work/command/i686-unknown-linux-musl/release/spindle-sandbox" "$root/usr/local/bin/spindle-sandbox"
set -- "$root"/usr/lib/python3.*/site-packages
[ -d "$1" ] || { echo "no Python site-packages in the image" >&2; exit 1; }
install -D -m 644 "$here/guest/spindle.py" "$1/spindle.py"
# At the root's node_modules, so `import ... from 'spindle'` (which skips NODE_PATH)
# finds it from any script, TypeScript included (Node strips the types).
install -D -m 644 "$here/guest/spindle.js" "$root/node_modules/spindle/index.js"
printf '{"name": "spindle", "main": "index.js"}\n' > "$root/node_modules/spindle/package.json"
versions=$("$work/apk/sbin/apk.static" --root "$root" info -v 2>/dev/null | sort)
# Manuals and caches are dead weight (the kernel is beside the image, not in it).
rm -rf "$root/usr/share/man" "$root/usr/share/doc" "$root"/var/cache/apk/* "$root/var/log/apk.log"
# As is what no command here can use: git's login shell for SSH servers (git-shell), its
# push to WebDAV servers ("dumb" HTTP, which no forge takes: pushes go through
# git-remote-http), and Python's turtle graphics (they need tkinter, which the image has
# not). The wasm Linux leaves the same turtle files out (build-image.sh).
rm -rf "$root/usr/bin/git-shell" "$root/usr/libexec/git-core/git-http-push" \
    "$root"/usr/lib/python3.*/turtle.py "$root"/usr/lib/python3.*/__pycache__/turtle.* "$root"/usr/lib/python3.*/turtledemo

echo "v86 filesystem"
# What this build wrote (not what packages brought) at one fixed time, and one order, so
# the index depends on the contents only. Paths relative to the root (no ./): fs2json
# makes the tar's top level the root.
find "$root" -newer "$work/started" -exec touch -h -d @0 {} +
tar -C "$root" --owner=0 --group=0 --numeric-owner --sort=name \
    -cf "$tools/build/rootfs.tar" $(ls -A "$root")
# Each file compressed with zstd (level 19; the index names them .bin.zst), in the tools
# root (v86's copy-to-sha256.py would need a Python with zstd, and Alpine's has none).
cp "$work/v86src/fs2json.py" "$work/v86src/copy-to-sha256.py" "$tools/build/"
mkdir -p "$tools/build/flat"
in_tools python3 /build/fs2json.py --zstd --out /build/fs.json /build/rootfs.tar >/dev/null
in_tools python3 /build/copy-to-sha256.py /build/rootfs.tar /build/flat >/dev/null 2>&1
in_tools sh -c 'cd /build/flat && ls | xargs -P "$0" -n 16 zstd -q -19 --rm' "$(nproc)"
rm -rf "$out"
mkdir -p "$out"
mv "$tools/build/fs.json" "$tools/build/flat" "$out/"
cp "$work/bzImage" "$out/bzImage"
cp "$work/v86/package/build/libv86.mjs" "$work/v86/package/build/v86.wasm" "$work/v86/package/LICENSE" "$out/"
mv "$out/LICENSE" "$out/LICENSE-v86"
cp "$work/v86src/seabios.bin" "$work/v86src/vgabios.bin" "$out/"
{
    echo "Alpine $ALPINE x86; Linux $KERNEL (Alpine's virt configuration at aports $APORTS_COMMIT, with kernel.config); v86 $V86_VERSION (commit $V86_COMMIT); $(rustc --version)"
    echo "$versions"
} > "$out/manifest.txt"
# The sandbox's identity, as runs record it: the index (which names every file by its
# digest), the kernel, the emulator and its BIOS.
cat "$out/fs.json" "$out/bzImage" "$out/libv86.mjs" "$out/v86.wasm" "$out/seabios.bin" "$out/vgabios.bin" \
    | sha256sum | cut -c1-64 > "$out/identity"
echo "browser Linux in $out ($(du -sh "$out" | cut -f1), $(find "$out/flat" -type f | wc -l) files)"
