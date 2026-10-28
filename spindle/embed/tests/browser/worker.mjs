// One worker's embedded client over OPFS, driven by the page: each message is an
// action, answered with its result (or the error's kind and message).
import init, * as wasm from '../glue/spindle_embed.js';
import { EmbeddedClient } from '../clients/embedded.mjs';
import { openOpfsPool } from '../clients/opfs.mjs';
import { KEY, call, catalog, scriptedFetch, text } from '../scripted.mjs';

await init({ module_or_path: new URL('../glue/spindle_embed_bg.wasm', import.meta.url) });
let client = null;
const fetch = scriptedFetch([]);
const open = () => {
  client ??= new EmbeddedClient({
    wasm, storage: 'opfs', pool: () => openOpfsPool({ slots: 24 }), catalog: catalog(),
    credentials: async () => ({ authorization: `Bearer ${KEY}` }), fetch,
  });
  return client;
};

const actions = {
  async create({ dir }) {
    const c = open();
    await c.create(dir, 'fake');
    fetch.replies = [call('write_file', { path: 'a.txt', content: 'hello from opfs' }), text('done')];
    return { head: (await c.prompt(dir, 'write a file')).head };
  },
  async status({ dir }) {
    const status = await open().status(dir);
    return { head: status.head, complete: status.complete };
  },
  async reopen({ dir }) {
    const c = open();
    for (let at = 0; at < 80; at++) {
      await c.closeSession(dir);
      const [status, files] = await Promise.all([c.status(dir), c.files(dir)]);
      if (!status.complete || !files.files.some((file) => file.path === 'a.txt')) throw new Error(`reopen ${at} lost the session`);
    }
    return { count: 80 };
  },
  async read({ dir, path }) { return { text: new TextDecoder().decode(await open().read(dir, path)) }; },
  async prompt({ dir }) {
    fetch.replies = [text('second turn')];
    return { head: (await open().prompt(dir, 'again')).head };
  },
  async export({ dir }) {
    const bytes = await open().exportSession(dir);
    let binary = '';
    for (const byte of bytes) binary += String.fromCharCode(byte);
    return { size: bytes.length, base64: btoa(binary) };
  },
  async import({ dir, base64 }) {
    const bytes = Uint8Array.from(atob(base64), (char) => char.charCodeAt(0));
    const status = await open().importSession(dir, bytes);
    return { head: status.head, imported: status.imported };
  },
  // The pool by itself: it grows past its opening count, removes a tree, and a later
  // opening finds every slot a grow added (with the files they hold).
  async pool({ phase }) {
    const pool = await openOpfsPool({ directory: 'pool-test', slots: 2 });
    try {
      const write = (path, text) => {
        const fd = pool.openSync(path, 'w+');
        const bytes = new TextEncoder().encode(text);
        pool.writeSync(fd, bytes, 0, bytes.length, 0);
        pool.fsyncSync(fd);
      };
      if (phase === 'reopen') return { free: pool.free, kept: pool.readFileSync('/keep/a'), gone: pool.existsSync('/t') };
      write('/keep/a', 'kept');
      write('/t/x', '1');
      let full = null;
      try { write('/t/y', '2'); } catch (error) { full = error.code; }
      await pool.grow(3);
      write('/t/y', '2');
      write('/t/z/w', '3');
      const before = pool.free;
      let notDir = null;
      try { pool.rmSync('/t'); } catch (error) { notDir = error.code; }
      pool.rmSync('/t', { recursive: true });
      return { full, before, notDir, after: pool.free, gone: !pool.existsSync('/t'), kept: pool.readFileSync('/keep/a') };
    } finally {
      pool.close();
    }
  },
  async close() {
    await client?.close();
    client = null;
    return {};
  },
};

self.onmessage = async ({ data }) => {
  try {
    self.postMessage({ id: data.id, ok: true, ...(await actions[data.action](data)) });
  } catch (error) {
    self.postMessage({ id: data.id, ok: false, kind: error.kind ?? error.name, message: String(error.message ?? error) });
  }
};
self.postMessage({ ready: true });
