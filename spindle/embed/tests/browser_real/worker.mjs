// The real-browser test's worker: the package's EmbeddedClient over OPFS, its model
// requests through the relay that served the page (the relay's catalog, and its marker
// in place of the key). The page calls client methods by name and hears every event.
import init, * as wasm from '../../web/spindle_embed.js';
import { EmbeddedClient } from '../../clients/embedded.mjs';
import { openOpfsPool } from '../../clients/opfs.mjs';

await init({ module_or_path: new URL('../../web/spindle_embed_bg.wasm', import.meta.url) });
const [{ marker }, catalog] = await Promise.all(['/relay/config', '/relay/catalog'].map(async (path) => {
  const response = await fetch(path);
  if (!response.ok) throw new Error(`${path} answered ${response.status}`);
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

self.onmessage = async ({ data: { id, method, args } }) => {
  try {
    let result = await client[method](...args);
    if (method === 'read') result = new TextDecoder().decode(result);
    self.postMessage({ id, ok: true, result });
  } catch (error) {
    self.postMessage({ id, ok: false, kind: error.kind ?? error.name, message: String(error.message ?? error) });
  }
};
self.postMessage({ ready: true });
