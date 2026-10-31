// A grown wasm instance gives its memory back (a Node test of the renewal the engine
// worker does, engine-worker.mjs `renewEngine`): the web glue with chrome/glue-drop.js
// appended, as chrome/build.sh makes it, is loaded twice under two URLs over one
// compiled module; once the first lets go (`__spindleDrop`) and its client closes,
// the first instance's memory goes back (the process's resident memory falls by more
// than 48 MiB of what that instance held), and the second instance serves sessions.
//
// usage: node --expose-gc chrome/tests/engine_renew.mjs WEB_GLUE_DIR
//   (WEB_GLUE_DIR: a build's web/, or `wasm-bindgen --target web` output of the
//   package's wasm)
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { EmbeddedClient } from '../../clients/js/embedded.mjs';
import { catalog, scriptedFetch, text } from '../../embed/tests/scripted.mjs';

const from = process.argv[2];
if (!from || !global.gc) throw new Error('usage: node --expose-gc engine_renew.mjs WEB_GLUE_DIR');
const dir = fs.mkdtempSync(join(tmpdir(), 'spindle-renew-'));
fs.copyFileSync(join(from, 'spindle_embed_bg.wasm'), join(dir, 'spindle_embed_bg.wasm'));
// A build's glue carries the drop already; wasm-bindgen's own is given it here.
const source = fs.readFileSync(join(from, 'spindle_embed.js'), 'utf8');
fs.writeFileSync(join(dir, 'spindle_embed.js'), source.includes('function __spindleDrop')
  ? source : source + fs.readFileSync(new URL('../glue-drop.js', import.meta.url), 'utf8'));
const glueUrl = pathToFileURL(join(dir, 'spindle_embed.js')).href;
const compiled = await WebAssembly.compile(fs.readFileSync(join(dir, 'spindle_embed_bg.wasm')));

async function instance(n) {
  const glue = await import(`${glueUrl}?instance=${n}`);
  const { memory } = await glue.default({ module_or_path: compiled });
  return { glue, memory };
}
const client = (glue, replies) => new EmbeddedClient({
  wasm: glue, storage: 'memory', catalog: catalog(), fetch: scriptedFetch(replies), credentials: async () => ({}),
});

const resident = () => process.memoryUsage().rss;
async function settle() {
  for (let i = 0; i < 10; i++) {
    global.gc();
    await new Promise((done) => setTimeout(done, 30));
  }
}

/** The first instance, grown by sessions and then by 64 MiB it touched, let go. */
async function growAndLetGo() {
  const first = await instance(0);
  const c = client(first.glue, Array.from({ length: 30 }, (_, i) => text(`${i} ${'lorem ipsum '.repeat(3000)}`)));
  await c.create('/s', 'fake');
  for (let i = 0; i < 30; i++) await c.prompt('/s', `turn ${i}`);
  first.memory.grow(1024);
  new Uint8Array(first.memory.buffer).fill(1, first.memory.buffer.byteLength - 64 * 1024 * 1024);
  const grown = first.memory.buffer.byteLength;
  await c.close();
  return { glue: first.glue, grown };
}
const before = resident();
const { glue: old, grown } = await growAndLetGo();
await settle();
const peak = resident();
assert.ok(peak - before > 48 * 1024 * 1024, `the first instance holds its memory (${(peak - before) >> 20} MiB)`);

// Renewed: a fresh copy of the glue, the first one let go.
const second = await instance(1);
old.__spindleDrop();
await settle();
const after = resident();
assert.ok(peak - after > 48 * 1024 * 1024, `its memory went back: ${peak >> 20} MiB, then ${after >> 20} MiB`);
const fresh = client(second.glue, [text('a fresh start')]);
await fresh.create('/t', 'fake');
assert.equal((await fresh.prompt('/t', 'hi')).stopped.complete, true);
assert.ok(second.memory.buffer.byteLength < grown, 'the fresh instance starts small');
await fresh.close();
fs.rmSync(dir, { recursive: true, force: true });
console.log(`engine-renew passed: a ${grown >> 20} MiB instance let go (resident memory ${peak >> 20} -> ${after >> 20} MiB), a fresh one at ${second.memory.buffer.byteLength >> 20} MiB serving sessions`);
