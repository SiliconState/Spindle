# v86's 9p server and filesystem, for Spindle's wasm Linux

`lib/9p.js`, `lib/filesystem.js`, `lib/marshall.js` and `src/browser/filestorage.js` are
v86's (https://github.com/copy/v86, commit cc37f35c43bf14865d74ad76879ecf9410f69eb1, the
same v86 the browser Linux runs), unchanged, under v86's BSD 2-Clause license
(`LICENSE`). They serve the wasm Linux's shares over virtio-9p (`../ninep.mjs` drives
them without v86's emulator) on the same filesystem the v86 runner uses, so
`../../linux-workspace.mjs` serves both Linuxes.

The other files under `src/` are Spindle's own stand-ins for the emulator modules those
files import (logging, a hex helper, constants, the PCI virtio device, the CPU and the
bus): just what the four files use.
