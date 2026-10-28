// The embedded client in Node or Electron, with sessions in directories: loads the
// package's Node glue, gives its storage node:fs and resolves `dir`s as StdioClient
// resolves them. `glue` is the wasm-bindgen `--target nodejs` module's path.
import fs from 'node:fs';
import { createRequire } from 'node:module';
import { hostname } from 'node:os';
import { resolve } from 'node:path';
import { EmbeddedClient } from './embedded.mjs';

export { EmbeddedClient, remoteRunner, SpindleError } from './embedded.mjs';

/** An EmbeddedClient over directories (`storage: 'node'`); other options as its own. */
export function nodeClient({ glue, journal = 'wal', ...options }) {
  const wasm = createRequire(import.meta.url)(glue);
  return new EmbeddedClient({
    wasm,
    storage: 'node',
    storageOptions: { journal, note: `pid ${process.pid} on ${hostname()}` },
    fs,
    platform: process.platform,
    resolveDir: (dir) => resolve(dir),
    ...options,
  });
}
