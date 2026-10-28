#!/usr/bin/env bash
# Builds Spindle for Chrome into one folder to load at chrome://extensions ("Load
# unpacked"): the extension's files, the package's release wasm with its web glue, the
# JS clients it runs on, the native UI's text helpers and theme (store/ui, copied so
# there is one source), and the Linux threads run bash in (chrome/linux/build.sh's
# output; SPINDLE_LINUX names another). With SPINDLE_WASM_LINUX naming a wasm Linux
# (chrome/wasm-linux/build-image.sh's output) it carries that too, and plain sh commands
# run there (linux-route.mjs); without it every command runs on v86. Run from the
# spindle directory. The icons are the native UI's icon.svg drawn at Chrome's sizes.
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

cargo build --locked --release -p spindle-embed --target wasm32-unknown-unknown
rm -rf "$out"
mkdir -p "$out/clients" "$out/icons"
"$bindgen" --target web --out-dir "$out/web" target/wasm32-unknown-unknown/release/spindle_embed.wasm
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
echo "Spindle for Chrome in $out ($(du -h "$out/web/spindle_embed_bg.wasm" | cut -f1) wasm, $(du -sh "$out/linux" | cut -f1) Linux$([ -n "$wasm_linux" ] && echo ", $(du -sh "$out/wasm-linux" | cut -f1) wasm Linux"))"
