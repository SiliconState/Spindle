# Other software in Spindle for Browser

Spindle for Browser is MIT licensed, like the rest of Spindle. A built extension also
carries software by others, each part under its own license. Paths are relative to the
built folder.

| Part | In the build | License | Source |
| --- | --- | --- | --- |
| Rust crates compiled into the package | `web/spindle_embed_bg.wasm` | Each crate's own: MIT, Apache-2.0 and the other permissive licenses `deny.toml` allows | crates.io, at the versions in `Cargo.lock` |
| SQLite | `web/spindle_embed_bg.wasm` | Public domain | <https://sqlite.org>, the version `sqlite-wasm-rs` bundles |
| v86, the emulator that runs the browser Linux | `linux/libv86.mjs`, `linux/v86.wasm` | BSD-2-Clause (`linux/LICENSE-v86`) | <https://github.com/copy/v86>, at the commit on the first line of `linux/manifest.txt` |
| SeaBIOS and its VGA BIOS | `linux/seabios.bin`, `linux/vgabios.bin` | LGPL-3.0 | The `bios/` folder of v86 at that commit; <https://www.seabios.org> |
| Alpine Linux packages: the Linux kernel (`linux-virt`), bash, coreutils, Python, Node.js, git and the rest | `linux/fs.json`, `linux/flat/` | Each package's own; the kernel is GPL-2.0, and many tools are GPL-2.0 or GPL-3.0 | `linux/manifest.txt` names the Alpine release and every package version. Each package's build recipe is in Alpine's aports (<https://gitlab.alpinelinux.org/alpine/aports>, the release's stable branch), and the sources it builds from are on Alpine's distfiles mirror (<https://distfiles.alpinelinux.org>) and upstream. |

A build made with `SPINDLE_WASM_LINUX` also carries the wasm Linux, in `wasm-linux/`:

| Part | License | Source |
| --- | --- | --- |
| Linux for the wasm architecture | GPL-2.0 | <https://github.com/tombl/linux>, at the commit pinned in `chrome/wasm-linux/build-kernel.sh`, with Spindle's patches in `chrome/wasm-linux/kernel/` |
| busybox | GPL-2.0 | <https://github.com/tombl/busybox>, at the commit pinned in `chrome/wasm-linux/build-userland.sh` |
| musl and compiler-rt's builtins, linked into its programs | MIT; Apache-2.0 with LLVM exceptions | <https://github.com/tombl/musl> and <https://github.com/tombl/llvm-project>, at the commits pinned in `chrome/wasm-linux/build-toolchain.sh` |
| Python with zlib, bzip2 and SQLite | PSF-2.0; Zlib; bzip2's BSD-style license; public domain | The releases pinned in `chrome/wasm-linux/build-python.sh` |
| Rust's standard library, in its programs | MIT or Apache-2.0 | The nightly pinned in `chrome/wasm-linux/build-rust.sh`, with the patches in `chrome/wasm-linux/rust/patches/` |

The `chrome/` paths above are in the Spindle repository: <https://github.com/SiliconState/Spindle>.
