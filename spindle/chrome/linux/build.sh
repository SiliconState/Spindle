#!/usr/bin/env bash
# Builds the Linux that Spindle for Browser runs bash in: Alpine (32-bit x86) with bash,
# coreutils, Python and pip, Node and npm, git and curl, booted by v86 (an x86 emulator
# in wasm) over its 9p filesystem, with Spindle's runner agent and web proxy as its only
# services (guest/), its `spindle` command and `spindle-sandbox` (which puts each command
# in namespaces of its own), static Rust programs (command/) built for the image with
# Rust's i686-unknown-linux-musl target. Nothing needs
# root: Alpine's static apk installs as a normal user, and the boot image is made inside
# the image in a user namespace. Downloads are pinned (versions, checksums, a commit);
# Alpine's packages are the branch's current ones, checked against its signing keys
# (manifest.txt lists them). What the build itself writes gets a fixed time (packages
# keep their own, which Python's compiled modules check), so the same packages give the
# same identity.
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
APK_STATIC=apk-tools-static-3.0.8-r0.apk
APK_STATIC_SHA256=c8e2c88c13ba12a12269b79a3543e1190ff8c0ab0beb32b58cadfd5881c619e3
V86_VERSION=0.5.472
V86_COMMIT=cc37f35c43bf14865d74ad76879ecf9410f69eb1
V86_TGZ_SHA256=4698fae3f518ad0a6f78d31e8f6f8802ff272a86f07b603b95f6c9fdd428e4fc
# Files of the v86 repository at V86_COMMIT, by their sha256.
V86_FILES="bios/seabios.bin:73e3f359102e3a9982c35fce98eb7cd08f18303ac7f1ba6ebfbe6cdc1c244d98
bios/vgabios.bin:a4bc0d80cc3ca028c73dafa8fee396b8d054ce87ebd8abfbd31b06b437607880
tools/fs2json.py:546b0ce7d0b172fa855587209318a8d473bce6de23fa327ceeb5b6c115fda23f
tools/copy-to-sha256.py:7bfb94736afbb9753b6deea922184eec2f9b205be0214bc5f9866cf990bfa77c"
PACKAGES="bash coreutils findutils grep sed gawk diffutils patch tar gzip xz python3 py3-pip nodejs npm git curl openssl jq ripgrep file less linux-virt"

# The command is built here (its target, once: rustup target add i686-unknown-linux-musl).
[ -d "$(rustc --print sysroot)/lib/rustlib/i686-unknown-linux-musl" ] \
    || { echo "Rust's i686-unknown-linux-musl target is missing: rustup target add i686-unknown-linux-musl" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
touch "$work/started"
fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
check() { echo "$2  $1" | sha256sum -c --quiet -; }

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
mkdir -p "$work/apk" "$work/root" "$work/cache"
tar xzf "$work/apk-static.apk" -C "$work/apk" sbin/apk.static 2>/dev/null
tar xzf "$work/minirootfs.tar.gz" -C "$work/root" --no-same-owner
root=$work/root
# Packages are checked against the keys the minirootfs carries.
"$work/apk/sbin/apk.static" --root "$root" --arch x86 --usermode --no-interactive \
    --update-cache --cache-dir "$work/cache" add $PACKAGES >/dev/null

echo "Spindle's command ($(rustc --version | cut -d' ' -f2))"
# Paths of the build machine stay out of it: the same sources give the same program.
command_src=$here/command
RUSTFLAGS="-C linker=rust-lld --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}/registry/src=/cargo --remap-path-prefix=$command_src=/spindle-command" \
    cargo build --quiet --locked --release --target i686-unknown-linux-musl \
    --manifest-path "$command_src/Cargo.toml" --target-dir "$work/command"

echo "Spindle's guest and boot image"
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
release=$(cat "$root/usr/share/kernel/virt/kernel.release")
# Root comes over virtio 9p, so the boot image loads its modules.
# Its archive without the build machine's device numbers (cpio --ignore-devno): the same
# packages give the same boot image wherever it is built.
grep -q 'cpio --quiet --renumber-inodes ' "$root/sbin/mkinitfs" \
    || { echo "mkinitfs no longer calls cpio as expected" >&2; exit 1; }
sed 's/cpio --quiet --renumber-inodes /cpio --quiet --renumber-inodes --ignore-devno /' "$root/sbin/mkinitfs" > "$root/mkinitfs-spindle"
chmod 755 "$root/mkinitfs-spindle"
unshare -r chroot "$root" env SOURCE_DATE_EPOCH=0 /mkinitfs-spindle -F "base virtio 9p" -o /boot/initramfs-9p "$release" >/dev/null
rm "$root/mkinitfs-spindle"
mv -f "$root/boot/initramfs-9p" "$root/boot/initramfs-virt"
versions=$("$work/apk/sbin/apk.static" --root "$root" info -v 2>/dev/null | sort)
# v86 boots the one kernel and boot image it finds in /boot; modules are in the boot
# image; manuals and caches are dead weight.
rm -rf "$root"/boot/System.map-* "$root"/boot/config-* "$root/boot/boot" "$root/lib/modules" \
    "$root/usr/share/man" "$root/usr/share/doc" "$root"/var/cache/apk/* "$root/var/log/apk.log"

echo "v86 filesystem"
# What this build wrote (not what packages brought) at one fixed time, and one order, so
# the index depends on the contents only. Paths relative to the root (no ./): fs2json
# makes the tar's top level the root.
find "$root" -newer "$work/started" -exec touch -h -d @0 {} +
tar -C "$root" --owner=0 --group=0 --numeric-owner --sort=name \
    -cf "$work/rootfs.tar" $(ls -A "$root")
rm -rf "$out"
mkdir -p "$out/flat"
python3 "$work/v86src/fs2json.py" --out "$out/fs.json" "$work/rootfs.tar" >/dev/null
python3 "$work/v86src/copy-to-sha256.py" "$work/rootfs.tar" "$out/flat" >/dev/null 2>&1
cp "$work/v86/package/build/libv86.mjs" "$work/v86/package/build/v86.wasm" "$work/v86/package/LICENSE" "$out/"
mv "$out/LICENSE" "$out/LICENSE-v86"
cp "$work/v86src/seabios.bin" "$work/v86src/vgabios.bin" "$out/"
{
    echo "Alpine $ALPINE x86, kernel $release; v86 $V86_VERSION (commit $V86_COMMIT); $(rustc --version)"
    echo "$versions"
} > "$out/manifest.txt"
# The sandbox's identity, as runs record it: the index (which names every file by its
# digest), the emulator and its BIOS.
cat "$out/fs.json" "$out/libv86.mjs" "$out/v86.wasm" "$out/seabios.bin" "$out/vgabios.bin" \
    | sha256sum | cut -c1-64 > "$out/identity"
echo "browser Linux in $out ($(du -sh "$out" | cut -f1), $(find "$out/flat" -type f | wc -l) files)"
