// The embedded client in Node or Electron, with sessions in directories: loads the
// package's Node glue, gives its storage node:fs and resolves `dir`s as StdioClient
// resolves them. `glue` is the wasm-bindgen `--target nodejs` module's path.
import fs from 'node:fs';
import { createRequire } from 'node:module';
import { hostname } from 'node:os';
import { resolve } from 'node:path';
import { EmbeddedClient } from './embedded.mjs';

export { EmbeddedClient, bundleFacts, remoteRunner, SpindleError } from './embedded.mjs';

function started(pid) {
  try {
    const stat = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
    return stat.slice(stat.lastIndexOf(')') + 2).split(/\s+/)[19];
  } catch { return null; }
}

function ownerNote() {
  if (process.platform === 'linux') {
    try {
      const boot = fs.readFileSync('/proc/sys/kernel/random/boot_id', 'utf8').trim();
      const namespace = fs.readlinkSync('/proc/self/ns/pid');
      const start = started(process.pid);
      if (boot && namespace && start) return `pid ${process.pid} on ${hostname()} boot ${boot} ns ${namespace} start ${start}`;
    } catch { /* Without a namespace identity, lock takeover stays disabled. */ }
  }
  return `pid ${process.pid} on ${hostname()}`;
}

/** A missing process or a reused PID proves this exact owner is gone. */
function alive(pid, start) {
  try {
    process.kill(pid, 0);
    const now = started(pid);
    return !now || now === start;
  } catch (error) {
    return error?.code !== 'ESRCH';
  }
}

/** An EmbeddedClient over directories (`storage: 'node'`); other options as its own.
 * Linux lock notes bind boot, PID namespace and process start time for safe stale-owner
 * takeover. Ambiguous owners and platforms without that evidence remain refused. */
export function nodeClient({ glue, journal = 'wal', ...options }) {
  const wasm = createRequire(import.meta.url)(glue);
  wasm.setProcessCheck?.(alive);
  return new EmbeddedClient({
    wasm,
    storage: 'node',
    storageOptions: { journal, note: ownerNote() },
    fs,
    platform: process.platform,
    resolveDir: (dir) => resolve(dir),
    ...options,
  });
}
