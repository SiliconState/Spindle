// Local engine time and memory of the embedded package under Node: a model that
// answers at once (nothing waits on a network), sessions in directories through the
// Node VFS: `small`, `long` and `compaction` as store/tests/bench.rs runs them natively,
// and `files` and `concurrent`, which only this one runs. Not a CI test:
// embed/tests/bench.sh builds the release wasm and runs both, and compares two runs.
//
// Each scenario reports milliseconds (p50/p95 over its repeats) and the wasm memory
// after it. Wasm memory only grows, so a scenario's figure is the high water of it and
// of every scenario before it; `grew_kib` is what that scenario added.
//
// usage: node embed/tests/bench.mjs GLUE.js [--quick] [--only small,long,...] [--out FILE] [--keep]
import fs from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { performance } from 'node:perf_hooks';
import { nodeClient } from '../../clients/js/embedded-node.mjs';
import { catalog, sse } from './scripted.mjs';

const args = process.argv.slice(2);
// The glue is the one argument that is neither an option nor an option's value.
const glue = args.find((arg, at) => !arg.startsWith('--') && !['--out', '--only'].includes(args[at - 1]));
if (!glue) throw new Error('usage: node bench.mjs GLUE.js [--quick] [--out FILE]');
const quick = args.includes('--quick');
const out = args.includes('--out') ? args[args.indexOf('--out') + 1] : null;
const SIZES = quick
  ? { small: 5, long: 30, reads: 10, reopens: 4, files: 100, compacting: 12, exports: 2, concurrent: 6 }
  : { small: 20, long: 150, reads: 30, reopens: 10, files: 400, compacting: 30, exports: 3, concurrent: 20 };

// The glue keeps its instance to itself; a copy beside it (so it finds its .wasm) also
// hands out the memory.
const measured = join(dirname(glue), 'spindle_embed.bench.cjs');
fs.writeFileSync(measured, `${fs.readFileSync(glue, 'utf8')}\nexports.__benchMemory = () => wasm.memory;\n`);
// nodeClient requires the same file, so both share the one instance.
const instance = createRequire(import.meta.url)(measured);
const memory = () => instance.__benchMemory().buffer.byteLength;

const root = fs.mkdtempSync(join(tmpdir(), 'spindle-bench-'));

/** Text of `bytes` length, different for each `seed` (so nothing deduplicates). */
function words(seed, bytes) {
  let x = seed * 2654435761 >>> 0;
  let text = '';
  while (text.length < bytes) {
    x = (x * 1103515245 + 12345) >>> 0;
    text += `w${(x >>> 8).toString(36)} `;
  }
  return text.slice(0, bytes);
}

/** A model answering at once: the reply `next(body)` gives, a summary for a compaction. */
function model(next) {
  let requests = 0;
  const encoder = new TextEncoder();
  const fetch = async (url, init) => {
    requests += 1;
    const body = new TextDecoder().decode(init.body);
    const reply = /Summarize the session history below/.test(body)
      ? { content: `SUMMARY ${requests}: ${words(requests, 600)}` }
      : next(body) ?? { content: '(script ended)' };
    const bytes = encoder.encode(sse(reply, requests));
    const stream = new ReadableStream({
      start(controller) {
        for (let at = 0; at < bytes.length; at += 512) controller.enqueue(bytes.subarray(at, at + 512));
        controller.close();
      },
    });
    return new Response(stream, { status: 200, headers: { 'content-type': 'text/event-stream' } });
  };
  fetch.requests = () => requests;
  return fetch;
}

/** Each turn writes a file, then answers: the prompt's file call, the answer after it. */
function filesThenAnswer(size = 1536) {
  let turn = 0;
  return (body) => {
    if (/"role":"tool"/.test(body.slice(body.lastIndexOf('"role":')))) {
      return { content: words(turn * 7 + 1, 800) };
    }
    turn += 1;
    return { call: ['write_file', { path: `notes/n${turn}.md`, content: words(turn, size) }] };
  };
}

function client(fetch) {
  return nodeClient({
    glue: measured, catalog: catalog(), fetch,
    credentials: async () => ({ authorization: 'Bearer bench' }),
  });
}

async function timed(work) {
  const start = performance.now();
  const value = await work();
  return [performance.now() - start, value];
}

function stats(times) {
  if (!times.length) return null;
  const sorted = [...times].sort((a, b) => a - b);
  const at = (q) => sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))];
  const round = (ms) => Math.round(ms * 1000) / 1000;
  return { n: times.length, p50: round(at(0.5)), p95: round(at(0.95)), max: round(sorted.at(-1)) };
}

const results = {};
let before = memory();
function record(name, figures) {
  const now = memory();
  results[name] = { ...figures, wasm_kib: now >> 10, grew_kib: (now - before) >> 10, rss_mib: process.memoryUsage().rss >> 20 };
  before = now;
  console.log(name, JSON.stringify(results[name]));
}

async function small() {
  const fetch = model(() => ({ content: 'hello there' }));
  const c = client(fetch);
  const creates = [], prompts = [];
  for (let i = 0; i < SIZES.small; i++) {
    const dir = join(root, `small-${i}`);
    creates.push((await timed(() => c.create(dir, 'fake')))[0]);
    prompts.push((await timed(() => c.prompt(dir, `hi ${i}`)))[0]);
    await c.closeSession(dir);
  }
  await c.close();
  record('small', { create: stats(creates), prompt: stats(prompts) });
}

async function long() {
  const fetch = model(filesThenAnswer());
  const c = client(fetch);
  const events = [];
  c.on('event', (event) => events.push(event.type));
  const dir = join(root, 'long');
  await c.create(dir, 'fake');
  const turns = [];
  for (let i = 0; i < SIZES.long; i++) {
    turns.push((await timed(() => c.prompt(dir, `turn ${i}: ${words(1000 + i, 200)}`)))[0]);
  }
  const afterTurns = memory();
  const { total } = await c.history(dir);
  const window = [], full = [], status = [], targets = [];
  for (let i = 0; i < SIZES.reads; i++) {
    window.push((await timed(() => c.history(dir, undefined, { from: Math.max(0, total - 30) })))[0]);
    status.push((await timed(() => c.status(dir)))[0]);
    targets.push((await timed(() => c.targets(dir)))[0]);
  }
  for (let i = 0; i < 5; i++) full.push((await timed(() => c.history(dir)))[0]);
  const afterReads = memory();
  const reopen = [];
  for (let i = 0; i < SIZES.reopens; i++) {
    await c.closeSession(dir);
    reopen.push((await timed(() => c.open(dir)))[0]);
  }
  const afterReopen = memory();
  const exports = [];
  let bundle = 0;
  for (let i = 0; i < SIZES.exports; i++) {
    const [ms, bytes] = await timed(() => c.exportSession(dir, { keep: true }));
    exports.push(ms);
    bundle = bytes.length;
  }
  await c.close();
  record('long', {
    turns: SIZES.long, messages: total, requests: fetch.requests(),
    compacted: events.filter((type) => type === 'compacted').length,
    first_turns: stats(turns.slice(0, 20)), last_turns: stats(turns.slice(-20)),
    history_window: stats(window), history_full: stats(full), status: stats(status), targets: stats(targets),
    reopen: stats(reopen), export: stats(exports), bundle_kib: bundle >> 10,
    wasm_kib_after: { turns: afterTurns >> 10, reads: afterReads >> 10, reopens: afterReopen >> 10 },
  });
}

async function files() {
  const fetch = model(() => ({ content: 'noted' }));
  const c = client(fetch);
  const dir = join(root, 'files');
  const given = Array.from({ length: SIZES.files }, (_, i) => ({ path: `src/d${i % 20}/f${i}.txt`, text: words(5000 + i, 2048) }));
  const [create] = await timed(() => c.create(dir, 'fake', { files: given }));
  const [first] = await timed(() => c.prompt(dir, 'look at the files'));
  const list = [], reads = [], prompts = [], reopen = [];
  for (let i = 0; i < SIZES.reads; i++) {
    list.push((await timed(() => c.files(dir)))[0]);
    reads.push((await timed(() => c.read(dir, given[(i * 37) % given.length].path)))[0]);
  }
  for (let i = 0; i < 5; i++) prompts.push((await timed(() => c.prompt(dir, `again ${i}`)))[0]);
  for (let i = 0; i < SIZES.reopens; i++) {
    await c.closeSession(dir);
    reopen.push((await timed(() => c.open(dir)))[0]);
  }
  await c.close();
  record('files', {
    files: SIZES.files, create: Math.round(create), first_prompt: Math.round(first),
    list: stats(list), read: stats(reads), prompt: stats(prompts), reopen: stats(reopen),
  });
}

async function compaction() {
  let turn = 0;
  const fetch = model(() => ({ content: words(turn += 1, 12000) }));
  const c = client(fetch);
  const events = [];
  c.on('event', (event) => events.push(event.type));
  const dir = join(root, 'compaction');
  await c.create(dir, 'fake', { model: 'small-model' });
  const plain = [], compacting = [];
  for (let i = 0; i < SIZES.compacting; i++) {
    const count = events.filter((type) => type === 'compacted').length;
    const [ms] = await timed(() => c.prompt(dir, `turn ${i}`));
    (events.filter((type) => type === 'compacted').length > count ? compacting : plain).push(ms);
  }
  await c.close();
  record('compaction', { turns: SIZES.compacting, compactions: compacting.length, plain: stats(plain), compacting: stats(compacting) });
}

async function concurrent() {
  const fetch = model(filesThenAnswer(1024));
  const c = client(fetch);
  const dirs = [0, 1, 2, 3].map((i) => join(root, `concurrent-${i}`));
  for (const dir of dirs) await c.create(dir, 'fake');
  const run = (dir) => (async () => {
    for (let i = 0; i < SIZES.concurrent; i++) await c.prompt(dir, `turn ${i}`);
  })();
  const [one] = await timed(() => run(dirs[0]));
  const [four] = await timed(() => Promise.all(dirs.map(run)));
  await c.close();
  record('concurrent', { turns_each: SIZES.concurrent, one_session_ms: Math.round(one), four_sessions_ms: Math.round(four) });
}

const only = args.includes('--only') ? args[args.indexOf('--only') + 1].split(',') : null;
for (const scenario of [small, long, files, compaction, concurrent]) {
  if (!only || only.includes(scenario.name)) await scenario();
}
const report = { host: 'wasm', quick, node: process.versions.node, sizes: SIZES, results };
if (out) fs.writeFileSync(out, `${JSON.stringify(report, null, 2)}\n`);
if (args.includes('--keep')) console.log(`kept ${root}`);
else fs.rmSync(root, { recursive: true, force: true });
fs.rmSync(measured, { force: true });
