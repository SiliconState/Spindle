import { WASI } from 'node:wasi';
import { readFile, mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const binary = process.argv[2];
if (!binary) throw new Error('usage: node wasi_probe.mjs STORE_PROBE.wasm');
const dir = await mkdtemp(join(tmpdir(), 'spindle-wasi-probe-'));
try {
  const module = await WebAssembly.compile(await readFile(binary));
  const wasi = new WASI({
    version: 'preview1',
    args: ['store_probe', '/probe/delete.sqlite', '/probe/wal.sqlite'],
    env: {},
    preopens: { '/probe': dir },
    returnOnExit: true,
  });
  const instance = await WebAssembly.instantiate(module, wasi.getImportObject());
  const status = wasi.start(instance);
  if (status !== 0) throw new Error(`WASI store probe exited ${status}`);
} finally {
  await rm(dir, { recursive: true, force: true });
}
