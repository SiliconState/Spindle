#!/usr/bin/env bash
# Builds Spindle for Browser into one folder to load at chrome://extensions or
# edge://extensions ("Load unpacked"): the extension's files, the package's release wasm
# with its web glue, the JS clients it runs on, the native UI's text helpers and theme
# (store/ui, copied so there is one source), and the Linux threads run bash in
# (chrome/linux/build.sh's output; SPINDLE_LINUX names another). With SPINDLE_WASM_LINUX
# naming a wasm Linux (chrome/wasm-linux/build-image.sh's output) it carries that too, and
# plain sh commands run there (linux-route.mjs); without it every command runs on v86. Run
# from the spindle directory. The icons are the native UI's icon.svg drawn at the
# browser's sizes; THIRD-PARTY.md names the other software a build carries and where its
# source is.
#
# usage: chrome/build.sh [WASM_BINDGEN_CLI] [OUT_DIR]   (OUT_DIR defaults to chrome/dist)
set -euo pipefail

bindgen=${1:-wasm-bindgen}
out=${2:-chrome/dist}
linux=${SPINDLE_LINUX:-chrome/linux/dist}
wasm_linux=${SPINDLE_WASM_LINUX:-}
if [ ! -f "$linux/identity" ]; then
    echo "no browser Linux in $linux: build it first (chrome/linux/build.sh)" >&2
    exit 1
fi
if [ -n "$wasm_linux" ] && [ ! -f "$wasm_linux/identity" ]; then
    echo "no wasm Linux in $wasm_linux: build it first (chrome/wasm-linux/build-image.sh)" >&2
    exit 1
fi
want=$(sed -n 's/^wasm-bindgen = "=\(.*\)"$/\1/p' embed/Cargo.toml)
have=$("$bindgen" --version | awk '{print $2}')
if [ "$have" != "$want" ]; then
    echo "wasm-bindgen CLI $have does not match the crate's $want" >&2
    exit 1
fi

# The wasm names source files in its panic messages: under /src (this checkout's parent,
# holding spindle/ and dext-engine/) and /cargo, not this machine's directories.
RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=$(cd .. && pwd)=/src --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo" \
    cargo build --locked --release -p spindle-embed --target wasm32-unknown-unknown
rm -rf "$out"
mkdir -p "$out/clients" "$out/icons"
"$bindgen" --target web --out-dir "$out/web" target/wasm32-unknown-unknown/release/spindle_embed.wasm
# The engine gives a grown wasm instance's memory back by loading a fresh copy of the
# glue (engine-worker.mjs `renewEngine`): a module is never unloaded, so this one lets go of
# its instance and of the views of its memory. Nothing in it is used after.
for held in '^let wasmModule, wasmInstance, wasm;$' '^let cachedDataViewMemory0 = null;$' \
    '^let cachedUint8ArrayMemory0 = null;$'; do
    grep -q "$held" "$out/web/spindle_embed.js" || {
        echo "the wasm-bindgen glue keeps its instance some other way: check chrome/glue-drop.js" >&2
        exit 1
    }
done
# And only those two views: one more would be kept by the old instance's glue.
views=$(grep -c '^let cached.*Memory0' "$out/web/spindle_embed.js" || true)
[ "$views" = 2 ] || {
    echo "the wasm-bindgen glue keeps $views views of its memory, not 2: check chrome/glue-drop.js" >&2
    exit 1
}
cat chrome/glue-drop.js >> "$out/web/spindle_embed.js"
cp chrome/manifest.json chrome/background.js chrome/engine.html chrome/engine.mjs chrome/engine-worker.mjs \
    chrome/linux-frames.mjs chrome/linux-net.mjs chrome/linux-route.mjs chrome/linux-runner.mjs chrome/linux-worker.mjs chrome/linux-workspace.mjs \
    chrome/wasm-linux-worker.mjs chrome/page-tools.mjs chrome/shared.mjs chrome/signin.mjs chrome/vault.mjs chrome/panel.html \
    chrome/panel.css chrome/panel.mjs chrome/steps.mjs chrome/page.mjs chrome/markdown.mjs chrome/zip.mjs chrome/view.html chrome/view.mjs chrome/preview.html \
    chrome/preview.js "$out/"
cp -r "$linux" "$out/linux"
if [ -n "$wasm_linux" ]; then
    # Its runtime (what boots the kernel and serves its devices; v86's 9p server, vendored)
    # and its image.
    mkdir -p "$out/wasm-linux"
    cp chrome/wasm-linux/{console,devicetree,entropy,imports,machine,ninep,runner,task,user,virtio}.mjs "$out/wasm-linux/"
    cp -r chrome/wasm-linux/v86 "$out/wasm-linux/v86"
    cp -r "$wasm_linux/fs.json" "$wasm_linux/flat" "$wasm_linux/vmlinux.wasm" "$wasm_linux/identity" "$out/wasm-linux/"
fi
cp chrome/icons/*.png "$out/icons/"
cp clients/js/common.mjs clients/js/embedded.mjs clients/js/opfs.mjs "$out/clients/"
cp store/ui/text.js store/ui/theme.js "$out/"
cp chrome/THIRD-PARTY.md "$out/"
# What this build is made of (Settings, About shows it; a report of a fault can name
# it): the sources' commits, marked when they had changes, the tools, the engine wasm's
# digest and each Linux image's identity. It names no directory of the machine.
revision() {
    local rev
    rev=$(git -C "$1" rev-parse --short=12 HEAD 2>/dev/null) || { echo unknown; return; }
    [ -z "$(git -C "$1" status --porcelain -- . 2>/dev/null)" ] || rev="$rev-dirty"
    echo "$rev"
}
# Each one worked out first: a step that fails stops the build, not a blank field.
built_version=$(sed -n 's/^ *"version": "\([^"]*\)",*$/\1/p' chrome/manifest.json | head -1)
built_spindle=$(revision .)
built_engine=$(revision ../dext-engine)
built_rustc=$(rustc -V | cut -d' ' -f2)
built_wasm=$(sha256sum "$out/web/spindle_embed_bg.wasm" | cut -c1-64)
built_linux=$(tr -d '\n' < "$linux/identity")
built_wasm_linux=null
if [ -n "$wasm_linux" ]; then built_wasm_linux="\"$(tr -d '\n' < "$wasm_linux/identity")\""; fi
for value in "$built_version" "$built_rustc" "$built_wasm" "$built_linux"; do
    [ -n "$value" ] || { echo "build.json: a value could not be worked out" >&2; exit 1; }
done
cat > "$out/build.json" <<BUILD
{
  "version": "$built_version",
  "spindle": "$built_spindle",
  "engine": "$built_engine",
  "wasm_bindgen": "$have",
  "rustc": "$built_rustc",
  "wasm": "$built_wasm",
  "linux": "$built_linux",
  "wasm_linux": $built_wasm_linux
}
BUILD
echo "Spindle for Browser in $out ($(du -h "$out/web/spindle_embed_bg.wasm" | cut -f1) wasm, $(du -sh "$out/linux" | cut -f1) Linux$([ -n "$wasm_linux" ] && echo ", $(du -sh "$out/wasm-linux" | cut -f1) wasm Linux"))"
