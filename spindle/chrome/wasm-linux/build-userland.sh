#!/usr/bin/env bash
# Builds the programs of Spindle's wasm Linux with its toolchain (build-toolchain.sh):
# busybox (tombl/busybox: busybox 1.37 for machines without an MMU, a clone in place of
# fork; GPL-2.0, so the binary ships with this source offer: the pinned commit below),
# static, with hush as `sh`. From source at pinned commits; nothing needs root.
#
# usage: chrome/wasm-linux/build-userland.sh TOOLCHAIN_DIR OUT_DIR
#   OUT_DIR gets root/ (the programs, at their paths: bin/busybox and its applets' links);
#   sources and builds go in OUT_DIR/src and OUT_DIR/build.
set -euo pipefail
umask 022

toolchain=$(cd "$1" && pwd)
out=$(mkdir -p "$2" && cd "$2" && pwd)

TARGET=wasm32-unknown-linux-musl
BUSYBOX_REPO=https://github.com/tombl/busybox
BUSYBOX_COMMIT=4ad91c87e7efeff15628bea7240b6055b2737644

cc=$toolchain/bin/$TARGET-cc
[ -x "$cc" ] || { echo "no toolchain in $toolchain (build-toolchain.sh)" >&2; exit 1; }
export PATH="$toolchain/bin:$toolchain/llvm/bin:/usr/local/bin:/usr/bin:/bin"
jobs=$(nproc)
src=$out/src
build=$out/build
root=$out/root
mkdir -p "$src" "$build" "$root"

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

echo "busybox (${BUSYBOX_COMMIT:0:10})"
checkout "$src/busybox" "$BUSYBOX_REPO" "$BUSYBOX_COMMIT"
rm -rf "$build/busybox" && cp -r "$src/busybox" "$build/busybox" && rm -rf "$build/busybox/.git"
cd "$build/busybox"
config() {
    sed -i "/^CONFIG_$1=/d; /^# CONFIG_$1 is not set/d" .config
    case $2 in
        y) echo "CONFIG_$1=y" >>.config ;;
        n) echo "# CONFIG_$1 is not set" >>.config ;;
        *) echo "CONFIG_$1=\"$2\"" >>.config ;;
    esac
}
make_busybox() {
    make -j"$jobs" CC="$cc" HOSTCC=/usr/bin/clang AR="$TARGET-ar" STRIP="$TARGET-strip" \
        OBJCOPY="$TARGET-objcopy" OBJDUMP="$TARGET-objdump" "$@"
}
make_busybox defconfig >"$build/busybox.log" 2>&1
config STATIC y
config NOMMU y
config STATIC_LIBGCC n
config CROSS_COMPILER_PREFIX ""
config EXTRA_LDLIBS c
# A shell for scripts (hush; no fork for ash's subshells), and no bash in its name.
config HUSH y
config HUSH_TICK y
config SH_IS_ASH n
config SH_IS_HUSH y
config SH_IS_NONE n
config BASH_IS_ASH n
config BASH_IS_HUSH n
config BASH_IS_NONE y
# What a command sandbox has no use for, or the platform cannot do (no MMU, no
# hardware, no logins).
for applet in INIT LINUXRC BOOTCHARTD CONSPY DEVMEM FBSPLASH HDPARM HEXEDIT INETD NSENTER \
    SWAPOFF SWAPON TC TELNETD SENDMAIL REFORMIME MAKEMIME POPMAILDIR RUNSV RUNSVDIR SVLOGD \
    HWCLOCK RTCWAKE; do
    config "$applet" n
done
make_busybox oldconfig </dev/null >>"$build/busybox.log" 2>&1
make_busybox >>"$build/busybox.log" 2>&1 || { tail -n 30 "$build/busybox.log" >&2; exit 1; }
make_busybox CONFIG_PREFIX="$root" install >>"$build/busybox.log" 2>&1
echo "busybox in $root/bin/busybox ($(du -h "$root/bin/busybox" | cut -f1), $(find "$root" -type l | wc -l) applets)"
