// The session runs here, in a Dedicated Worker: Spindle's embedded package over OPFS,
// its model requests sent through the relay that served this page (which adds the key).
import init, * as wasm from '../../web/spindle_embed.js';
import { EmbeddedClient } from '../../clients/embedded.mjs';
import { openOpfsPool } from '../../clients/opfs.mjs';

await init({ module_or_path: new URL('../../web/spindle_embed_bg.wasm', import.meta.url) });
// The relay's catalog (the same providers it forwards to) and its marker: the page holds
// no key, only the marker the relay replaces with it.
const [{ marker }, catalog] = await Promise.all(['/relay/config', '/relay/catalog'].map(async (path) => {
  const response = await fetch(path);
  if (!response.ok) throw new Error(`${path} answered ${response.status}: open this page from the relay`);
  return response.json();
}));
const client = new EmbeddedClient({
  wasm,
  storage: 'opfs',
  pool: () => openOpfsPool(),
  catalog,
  credentials: async () => ({ key: marker }),
  fetch: (url, init) => fetch(`/relay?to=${encodeURIComponent(url)}`, init),
});
client.on('event', (event) => self.postMessage({ event }));

async function session(dir, provider, model) {
  try {
    return await client.open(dir);
  } catch (error) {
    if (error.kind !== 'not_found') throw error;
    return client.create(dir, provider, model ? { model } : {});
  }
}

const actions = {
  async providers() {
    const { providers } = await client.providers();
    return { providers, active: catalog.active_provider ?? null };
  },
  async prompt({ dir, provider, model, text }) {
    await session(dir, provider, model);
    const turn = await client.prompt(dir, text);
    return { head: turn.head, files: (await client.files(dir)).files.map((f) => `${f.size} ${f.path}`) };
  },
  async stop({ dir }) { return client.stop(dir); },
  async status({ dir }) {
    const status = await client.status(dir);
    return { head: status.head, state: status.state, provider: status.provider, model: status.model };
  },
  async export({ dir }) { return { bytes: await client.exportSession(dir) }; },
  async import({ dir, bytes }) {
    const status = await client.importSession(dir, new Uint8Array(bytes));
    return { head: status.head };
  },
};

self.onmessage = async ({ data }) => {
  try {
    self.postMessage({ id: data.id, ok: true, result: await actions[data.action](data) });
  } catch (error) {
    self.postMessage({ id: data.id, ok: false, error: String(error.message ?? error) });
  }
};
self.postMessage({ ready: true });
