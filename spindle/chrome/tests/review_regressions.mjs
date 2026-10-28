// Focused extension regressions without a provider, VM image or Chrome profile.
// usage: node chrome/tests/review_regressions.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { local, wireNetwork } from '../linux-net.mjs';

const tick = () => new Promise((done) => setImmediate(done));
const deferred = () => {
  let resolve;
  return { promise: new Promise((done) => { resolve = done; }), resolve: (...args) => resolve(...args) };
};

// Run the actual engine functions with explicit storage/client dependencies.
const engineSource = readFileSync(new URL('../engine-worker.mjs', import.meta.url), 'utf8')
  .replace(/^import .*;\n/gm, '')
  .split('const starting = (async () => {')[0];
const records = new Map();
const messages = [];
const warnings = [];
const context = vm.createContext({
  crypto: globalThis.crypto, console: { ...console, warn: (...args) => warnings.push(args) }, setTimeout, clearTimeout, Uint8Array, URL, URLSearchParams,
  BroadcastChannel: class { postMessage(message) { messages.push(message); } },
  CHANNEL: 'test', self: {}, linuxRunner: () => ({ warm() {} }), PAGE_TOOLS: [],
  read: async (store, key) => records.get(`${store}:${key}`),
  write: async (store, key, value) => records.set(`${store}:${key}`, value),
  remove: async (store, key) => records.delete(`${store}:${key}`),
  update: async (store, key, change) => {
    const name = `${store}:${key}`;
    const value = records.get(name);
    if (!value) return;
    const next = change(value);
    records.set(name, next);
    return next;
  },
});
vm.runInContext(`${engineSource}\nglobalThis.test = { operate, ops, running, renew, replaceFirst, creationSettings, setClient: (value) => { client = value; poolOpening = Promise.resolve({ free: 100, rmSync() {} }); } };`, context);
const engine = context.test;
const id = '0123456789abcdef';
let entered = 0;
const gate = deferred();
engine.ops.probe = async () => { entered += 1; await gate.promise; return entered; };
const first = engine.operate('probe', { id });
const second = engine.operate('probe', { id });
await tick();
assert.equal(entered, 1, 'same-thread operations cannot overlap');
await assert.rejects(engine.operate('catalog', {}), /pending operations/);
gate.resolve();
await Promise.all([first, second]);
assert.equal(entered, 2);
await assert.rejects(engine.operate('probe', { id: '../outside' }), /invalid thread/);
const originalStop = engine.ops.stop;
engine.ops.open = ({ from }) => from;
assert.equal(await engine.operate('open', { id, from: 4 }), 4, 'history offsets are not thread IDs');
let stoppedWhileBusy = false;
engine.ops.stop = async () => { stoppedWhileBusy = true; };
const blocked = deferred();
engine.ops.blocked = () => blocked.promise;
const blocking = engine.operate('blocked', { id });
await engine.operate('stop', { id });
assert.equal(stoppedWhileBusy, true, 'Stop bypasses slow reads in the thread lane');
blocked.resolve();
await blocking;
engine.ops.stop = originalStop;

let created = 0;
let exported = 0;
engine.setClient({
  async create() { created += 1; throw new Error('invalid effort'); },
  async closeSession() {},
  async exportSession() { exported += 1; },
});
await assert.rejects(engine.replaceFirst(id, 'provider', { effort: 'bad' }), /invalid effort/);
assert.equal(created, 1);
assert.equal(exported, 0, 'invalid replacement does not export, delete or resend the original');
const settings = engine.creationSettings({ provider: 'p', model: 'm', compaction: 'digest', tasks: false, self_edit: false });
assert.equal(settings.compaction, 'digest');
assert.equal(settings.builtin_tools, 'all', 'an omitted built-in setting means all, not create\'s files default');
assert.deepEqual(Array.from(settings.tools), []);
assert.equal('self_edit' in settings, false);

let restored = 0;
let installs = 0;
engine.setClient({
  async create() { if (++installs === 2) throw new Error('storage failure'); },
  async closeSession() {},
  async exportSession() { return new Uint8Array([1, 2, 3]); },
  async importSession(dir, bytes) { restored += 1; assert.equal(dir, `/threads/${id}`); assert.equal(bytes[2], 3); },
});
await assert.rejects(engine.replaceFirst(id, 'provider', {}), /storage failure/);
assert.equal(restored, 1, 'a failed replacement restores the original record');
assert.equal(records.has(`kv:reset:${id}`), false);

const host = deferred();
vm.runInContext('hostChanges = globalThis.hostWait;', Object.assign(context, { hostWait: host.promise }));
records.set(`threads:${id}`, { id, runner: 'remote', state: 'idle' });
let retried = 0;
engine.setClient({ async retry() { retried += 1; }, async stop() {}, async closeSession() {} });
await engine.operate('retry', { id });
await engine.operate('stop', { id });
host.resolve();
await tick();
assert.equal(retried, 0, 'Stop during host connection must not send the pending turn later');
assert.equal(records.get(`threads:${id}`).state, 'stopped');

engine.setClient({
  async files() { return { revision: 5, files: [{ path: 'kept.txt' }] }; },
  async history() { return { messages: [{ role: 'assistant' }, { role: 'user', content: [{ type: 'text', text: 'next' }] }] }; },
  async targets() { throw new Error('old workspace unavailable'); },
  async closeSession() {},
});
const keptFiles = await engine.operate('turnFiles', { id });
assert.equal(keptFiles.files[0].path, 'kept.txt');
assert.equal(keptFiles.before, null, 'missing comparison history does not hide the workspace');
assert.equal(warnings.length, 1, 'missing history is reported, not silently lost');

// Drive the gateway through its real connection handler, with fetch captured.
function connection() {
  const handlers = new Map();
  const output = [];
  const conn = {
    psrc: [192, 168, 86, 1], sport: 3128, tuple: 'test', state: 'established',
    send_chunk_buf: new Uint8Array(1024), send_buffer: [], net: { tcp_conn: {} },
    pump() {}, process() {}, on(name, run) { handlers.set(name, run); }, accept() {}, release() {}, close() {},
    write(bytes) { output.push(new Uint8Array(bytes)); },
  };
  const adapter = { send() {}, tcp_conn: {} };
  const wired = wireNetwork({ network_adapter: adapter }, { relay: null, allowed: () => true });
  adapter.on_tcp_connection(conn);
  return { conn, wired, input: (data) => handlers.get('data')(data), output: () => Buffer.concat(output).toString() };
}
const originalFetch = globalThis.fetch;
const seen = [];
try {
  globalThis.fetch = async (url, init) => {
    seen.push({ url: String(url), ...init });
    return new Response(null, { status: 302, headers: { location: 'http://127.0.0.1/private' } });
  };
  const redirect = connection();
  redirect.input(Buffer.from('GET https://public.example/start HTTP/1.1\r\nHost: public.example\r\n\r\n'));
  await tick();
  assert.equal(seen.length, 1);
  assert.equal(seen[0].redirect, 'manual', 'GET redirects cannot bypass local-address checks');
  assert.match(redirect.output(), /403 Forbidden/);
  redirect.wired.end({ hard: true });

  const line = connection();
  line.input(Buffer.from(`SPINDLE-ORIGIN ${'x'.repeat(65536)}`));
  assert.match(line.output(), /400 Bad Request/, 'the origin preamble is bounded too');
  line.wired.end({ hard: true });
  const flood = connection();
  flood.input(new Uint8Array(64 * 1024 * 1024 + 65537));
  assert.match(flood.output(), /413 Content Too Large/);
  flood.wired.end({ hard: true });
} finally {
  globalThis.fetch = originalFetch;
}
for (const name of ['::1', '::7f00:1', '::ffff:7f00:1', 'ff02::1', 'fe80::1', 'fc00::1']) assert.equal(local(`[${name}]`), true, name);
assert.equal(local('[2606:4700:4700::1111]'), false);

// Network request IDs contain dots in real CDP; redirect hops cannot name a final body.
const background = readFileSync(new URL('../background.js', import.meta.url), 'utf8').replace(/^import .*;\n/gm, '');
const sent = [];
const event = { addListener() {} };
const pageContext = vm.createContext({
  console, setTimeout, clearTimeout, URL, read: async () => ({ reading: true, acting: true }), PAGE_ACCESS: [],
  pageSwitches: () => ({ reading: true, acting: true }),
  chrome: {
    sidePanel: { setPanelBehavior: async () => {} },
    runtime: { onInstalled: event, onMessage: event },
    permissions: { contains: async () => true, onRemoved: event },
    debugger: { onDetach: event, onEvent: event, sendCommand: async (_, method, params) => { sent.push({ method, params }); return { body: 'pong' }; } },
    tabs: { get: async (tab) => ({ id: tab, url: 'https://public.example/' }) },
  },
});
vm.runInContext(`${background}\nglobalThis.test = { watched, attached, network, recordNetwork, cdp, calls, inPage };`, pageContext);
const bg = pageContext.test;
bg.attached.set(7, null);
bg.watched.set(7, new Map([['1234.56', { id: '1234.56' }]]));
assert.equal((await bg.network({ tab: 7, body: '1234.56' })).content, 'pong');
assert.equal(sent.at(-1).params.requestId, '1234.56');
bg.recordNetwork(7, 'Network.requestWillBeSent', { requestId: '1234.56', redirectResponse: { status: 302 }, request: { method: 'GET', url: 'https://public.example/final' } });
assert.match((await bg.network({ tab: 7, body: '1234.56.1' })).error, /redirect hop/);
const count = sent.length;
await assert.rejects(bg.cdp(7, 'Input.insertText', { text: 'late' }, { until: Date.now() + 1000, cancelled: true }), /stopped/);
assert.equal(sent.length, count, 'cancelled page work cannot dispatch another input');
const attaching = deferred();
pageContext.chrome.debugger.attach = () => attaching.promise;
const control = { until: Date.now() + 10_000, cancelled: false };
const late = bg.cdp(8, 'Input.insertText', { text: 'late' }, control);
await tick();
control.cancelled = true;
attaching.resolve();
await assert.rejects(late, /stopped/);
assert.equal(sent.filter(({ method }) => method === 'Input.insertText').length, 0, 'Stop during debugger attach blocks the input after it');
assert.match(bg.inPage('select', { expires: Date.now() - 1 }).error, /timed out/);
vm.runInContext('for (const timer of attached.values()) clearTimeout(timer);', pageContext);
console.log('review-regressions passed: operation races, reset safety, early Stop, gateway bounds/redirects, IPv6, CDP body IDs');
