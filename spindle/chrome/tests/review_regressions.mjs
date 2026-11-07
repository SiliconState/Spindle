// Focused extension regressions without a provider, VM image or Chrome profile.
// usage: node chrome/tests/review_regressions.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { local, resolvesLocal, storeRefusal, wireNetwork } from '../linux-net.mjs';
import { ACTING_NAMES, ASKING_NAMES, PAGE_TOOLS, UPLOAD, actionText, confirmMode, numbersIn, pagesRecord, sends, uncertain } from '../page-tools.mjs';
import { typeOf } from '../page.mjs';
import { titleFrom } from '../../store/ui/text.js';
import { refusedAnswer } from '../../clients/js/common.mjs';
import { EventParser, RemoteClient, requestId, worthRepeating } from '../../clients/js/remote.mjs';
import { BUNDLE_MOST, MOVE, STEP_TEXT, Waiting, hostMoves, moveKey, said as sentence } from '../host-moves.mjs';
import { connectAnswer, pkce } from '../signin.mjs';

const tick = () => new Promise((done) => setImmediate(done));
const deferred = () => {
  let resolve;
  return { promise: new Promise((done) => { resolve = done; }), resolve: (...args) => resolve(...args) };
};

// Run the actual engine functions with explicit storage/client dependencies.
const WAIT_LINE = /const PAGE_TOOL_WAIT = [\d_]+;/;
const engineFile = readFileSync(new URL('../engine-worker.mjs', import.meta.url), 'utf8');
assert.match(engineFile, WAIT_LINE);
const engineSource = engineFile
  // Chrome's silence is waited out in a moment here, not 35 s.
  .replace(WAIT_LINE, 'const PAGE_TOOL_WAIT = 200;')
  .replace(/^import .*;\n/gm, '')
  .split('const starting = (async () => {')[0];
const records = new Map();
const messages = [];
const warnings = [];
const context = vm.createContext({
  crypto: globalThis.crypto, console: { ...console, warn: (...args) => warnings.push(args) }, setTimeout, clearTimeout, Uint8Array, URL, URLSearchParams, btoa,
  BroadcastChannel: class { postMessage(message) { messages.push(message); } },
  CHANNEL: 'test', self: { location: { href: 'chrome-extension://id/engine-worker.mjs' } }, linuxRunner: () => ({ warm() {}, forget() {} }), PAGE_TOOLS: [], uncertain, ASKING_NAMES, UPLOAD, typeOf, titleFrom, actionText, confirmMode, numbersIn, sends, BROWSER: 'Chrome', storeRefusal, firstGlue: {},
  refusedAnswer, RemoteClient, requestId, worthRepeating, MOVE, STEP_TEXT, Waiting, hostMoves, sentence, BUNDLE_MOST,
  read: async (store, key) => records.get(`${store}:${key}`),
  write: async (store, key, value) => records.set(`${store}:${key}`, value),
  writeDurably: async (store, key, value) => records.set(`${store}:${key}`, value),
  writeTogether: async (changes) => {
    for (const [store, key, value] of changes) {
      if (value === undefined) records.delete(`${store}:${key}`);
      else records.set(`${store}:${key}`, value);
    }
  },
  valuesOf: async (store) => [...records.entries()].filter(([name]) => name.startsWith(`${store}:`)).map(([, value]) => structuredClone(value)),
  keysOf: async (store) => [...records.keys()].filter((name) => name.startsWith(`${store}:`)).map((name) => name.slice(store.length + 1)),
  keysWith: async (store, prefix) => [...records.keys()].filter((name) => name.startsWith(`${store}:${prefix}`)).map((name) => name.slice(store.length + 1)),
  has: async (store, key) => records.has(`${store}:${key}`),
  REQUESTS: 'test:requests', answersFor: (page) => `test:answers:${page}`,
  remove: async (store, key) => records.delete(`${store}:${key}`),
  removeDurably: async (store, key) => records.delete(`${store}:${key}`),
  update: async (store, key, change, { changes = [] } = {}) => {
    const name = `${store}:${key}`;
    const value = records.get(name);
    if (!value) return;
    const next = change(value);
    records.set(name, next);
    for (const [other, otherKey, value] of changes) {
      if (value === undefined) records.delete(`${other}:${otherKey}`);
      else records.set(`${other}:${otherKey}`, value);
    }
    return next;
  },
});
vm.runInContext(`${engineSource}\nglobalThis.test = { turnTabsDone, tasksOpening, afterTurn, prepared, guided, setupOf, operate, ops, running, snapshot, restoreQueued, queued, renew, replaceFirst, creationSettings, pageTool, cancelPageTools, secretHeader, changeSites, grantsFor, underPrefix, withSecrets, beforeActing, actOnPage, ownedByTask, hostedFrame, answerHosted, endHosted, startHosted, admitHosted, sendHostedStop, hostedTurns, followed, pollHosted, setHost: (value) => { hostState = { connected: true, kind: 'browser', url: 'http://host' }; hostRemote = value; }, setClient: (value) => { client = value; poolOpening = Promise.resolve({ free: 100, rmSync() {} }); } };`, context);
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
records.set(`threads:${id}`, { id, runner: 'remote', state: 'interrupted', note: 'Chrome stopped while it ran' });
let retried = 0;
engine.setClient({
  async retry() { retried += 1; },
  async stop() {},
  async closeSession() {},
  // A cut model request (Retry sends it again), not a stopped step.
  async status() { return { accepts_prompt: false, accepts_message: false, phase: 'model_wait' }; },
});
await engine.operate('retry', { id });
await engine.operate('stop', { id });
host.resolve();
await tick();
assert.equal(retried, 0, 'Stop during host connection must not send the pending turn later');
// Stopped before it began, the turn leaves the thread as it was (Retry still applies).
assert.deepEqual([records.get(`threads:${id}`).state, records.get(`threads:${id}`).note], ['interrupted', 'Chrome stopped while it ran']);

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

// While the thread works (a turn, a task) the store lists no points to go back to: that
// is no warning, and the answer is not kept, so the same revision asked again once the
// store can look back finds the point. The messages before a summary say why they wait.
{
  const busy = Object.assign(new Error('busy: a prompt is running in this session; wait for it or stop it'), { kind: 'busy' });
  let lookedBack = 0;
  let targetsNow = () => { throw busy; };
  const asked = { role: 'user', content: [{ type: 'text', text: 'next' }] };
  engine.setClient({
    async files(dir, revision) { return { revision: revision ?? 6, files: [{ path: 'kept.txt', digest: revision == null ? 'b' : 'a' }] }; },
    async history(dir, revision, options) {
      if (revision == null) return { messages: [{ role: 'assistant' }, asked] };
      return options?.from != null ? { total: 1 } : { messages: [{ role: 'assistant' }] };
    },
    async targets() { lookedBack += 1; return targetsNow(); },
    async closeSession() {},
  });
  const whileBusy = await engine.operate('turnFiles', { id });
  assert.equal(whileBusy.before, null, 'no earlier point while the store is busy');
  assert.equal(warnings.length, 1, 'a store busy with the thread\'s work is no warning');
  targetsNow = () => [{ revision: 4, kind: 'settled' }];
  const lookedAgain = await engine.operate('turnFiles', { id });
  assert.equal(JSON.stringify(lookedAgain.before), JSON.stringify([['kept.txt', 'a']]), 'the busy answer was not kept: the same revision finds the point once the store can look back');
  assert.equal(lookedBack, 2);
  targetsNow = () => { throw busy; };
  await assert.rejects(engine.operate('earlier', { id }), /^Error: The messages before the summary can be shown once this thread stops working$/);
  assert.equal(warnings.length, 1);
}

// A page tool call goes to Chrome and comes back with Chrome's answer; cut off by the
// turn's end, an action's outcome is unknown and a read just failed.
const posted = [];
context.self.postMessage = (message) => posted.push(message);
const toolThread = 'fedcba9876543210';
const clicking = engine.pageTool('click', { ref: 'abc1' }, `/threads/${toolThread}`);
const asked = posted.find((message) => message.type === 'page_tool');
assert.equal(asked?.name, 'click', 'the call reached Chrome');
// As engine.mjs sends Chrome's answer back.
context.self.onmessage({ data: { type: 'page_tool_answer', call: asked.call, answer: { content: 'Clicked' } } });
assert.deepEqual(await clicking, { content: 'Clicked' });
const cutClick = engine.pageTool('click', { ref: 'abc1' }, `/threads/${toolThread}`);
const cutRead = engine.pageTool('read_page', {}, `/threads/${toolThread}#task`);
engine.cancelPageTools(toolThread);
assert.deepEqual(await cutClick, { unknown: true, content: 'the turn ended before the page tool finished' });
assert.deepEqual(await cutRead, { error: 'the turn ended before the page tool finished' });
assert.equal(posted.filter((message) => message.type === 'page_tool_cancel').length, 2);
// Chrome never answering is the same: unknown for an action, a failure for a read.
assert.deepEqual(await engine.pageTool('type', { ref: 'abc1', text: 'x' }, `/threads/${toolThread}`), { unknown: true, content: 'Chrome did not answer the page tool in time' });
assert.deepEqual(await engine.pageTool('list_tabs', {}, `/threads/${toolThread}`), { error: 'Chrome did not answer the page tool in time' });
assert.equal(posted.filter((message) => message.type === 'page_tool_cancel').length, 4);

// Drive the gateway through its real connection handler, with fetch captured.
function connection(lookup = async () => ({ addresses: ['93.184.215.14'], ttl: 60 })) {
  const handlers = new Map();
  const output = [];
  const conn = {
    psrc: [192, 168, 86, 1], sport: 3128, tuple: 'test', state: 'established',
    send_chunk_buf: new Uint8Array(1024), send_buffer: [], net: { tcp_conn: {} },
    pump() {}, process() {}, on(name, run) { handlers.set(name, run); }, accept() {}, release() {}, close() {},
    write(bytes) { output.push(new Uint8Array(bytes)); },
  };
  const adapter = { send() {}, tcp_conn: {} };
  const wired = wireNetwork({ network_adapter: adapter }, { relay: null, allowed: () => true, lookup });
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

  // The browser's own extension store is closed to every extension (Chrome says CORS):
  // the guest is told so, not to turn on a network it has.
  globalThis.fetch = async () => { throw new TypeError('Failed to fetch'); };
  const store = connection();
  store.input(Buffer.from('GET https://chromewebstore.google.com/search/spindle HTTP/1.1\r\nHost: chromewebstore.google.com\r\n\r\n'));
  await tick();
  assert.match(store.output(), /^HTTP\/1\.1 403 [^]*extension store, which the browser never lets an extension reach/);
  assert.doesNotMatch(store.output(), /network on again/);
  store.wired.end({ hard: true });
  const elsewhere = connection();
  elsewhere.input(Buffer.from('GET https://public.example/down HTTP/1.1\r\nHost: public.example\r\n\r\n'));
  await tick();
  assert.match(elsewhere.output(), /^HTTP\/1\.1 502 [^]*network on again/, 'another site that fails is told as before');
  elsewhere.wired.end({ hard: true });
  assert.ok(storeRefusal('https://chrome.google.com/webstore/detail/x'));
  assert.ok(storeRefusal('https://microsoftedge.microsoft.com/addons/search/x'));
  assert.equal(storeRefusal('https://chrome.google.com/'), null);
  assert.equal(storeRefusal('not a url'), null);

  // A public name that resolves into the local network is refused before Chrome fetches.
  seen.length = 0;
  globalThis.fetch = async (url) => {
    seen.push({ url: String(url) });
    return new Response('reached');
  };
  const rebound = connection(async (name) => ({ addresses: name === 'inward.example' ? ['93.184.215.14', '192.168.1.1'] : ['93.184.215.14'], ttl: 60 }));
  rebound.input(Buffer.from('GET http://inward.example/admin HTTP/1.1\r\nHost: inward.example\r\n\r\n'));
  await tick();
  assert.match(rebound.output(), /^HTTP\/1\.1 403 [^]*leads to an address on this computer or its local network/);
  assert.equal(seen.length, 0, 'nothing was fetched from a name that leads inward');
  rebound.wired.end({ hard: true });
  const outward = connection();
  outward.input(Buffer.from('GET http://outward.example/ HTTP/1.1\r\nHost: outward.example\r\n\r\n'));
  await tick();
  assert.match(outward.output(), /^HTTP\/1\.1 200 /);
  assert.equal(seen.length, 1);
  outward.wired.end({ hard: true });

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
// What a name resolves to: IPv6 answers too; a failed lookup leaves the decision to
// Chrome's own; address literals are not looked up; verdicts are kept for their TTL.
{
  let asked = 0;
  const answers = { 'v6.example': ['2606:4700::1', '::ffff:127.0.0.1'], 'ok.example': ['2606:4700::1'] };
  const lookup = async (name) => {
    asked += 1;
    if (!(name in answers)) throw new Error('no answer');
    return { addresses: answers[name], ttl: 1 };
  };
  assert.equal(await resolvesLocal('v6.example', lookup), true);
  assert.equal(await resolvesLocal('OK.example.', lookup), false);
  assert.equal(await resolvesLocal('down.example', lookup), false, 'a lookup that fails says no');
  assert.equal(await resolvesLocal('93.184.215.14', lookup), false);
  assert.equal(await resolvesLocal('[2606:4700::1]', lookup), false);
  assert.equal(asked, 3, 'address literals are never looked up');
  answers['v6.example'] = ['2606:4700::1'];
  assert.equal(await resolvesLocal('v6.example', lookup), true, 'kept for at least 30 s');
  assert.equal(await resolvesLocal('down.example', lookup), false, 'a failed lookup is kept too');
  assert.equal(asked, 3);
  // DNS-over-HTTPS out of reach: no lookup for a while (each would wait its timeout),
  // and another lookup function keeps its own verdicts.
  let tried = 0;
  const unreachable = async () => {
    tried += 1;
    throw Object.assign(new Error('offline'), { transport: true });
  };
  assert.equal(await resolvesLocal('one.example', unreachable), false);
  assert.equal(await resolvesLocal('two.example', unreachable), false);
  assert.equal(tried, 1, 'lookups pause once DNS-over-HTTPS cannot be reached');
  assert.equal(await resolvesLocal('v6.example', async () => ({ addresses: ['2606:4700::1'], ttl: 60 })), false);
}
// The real lookup's two queries stand alone: a private A record counts though AAAA
// fails; both failing is a failure (a transport one only if neither got through).
{
  const { dohLookup } = await import('../linux-net.mjs');
  const real = globalThis.fetch;
  const json = (body) => new Response(JSON.stringify(body), { headers: { 'content-type': 'application/dns-json' } });
  try {
    globalThis.fetch = async (url) => {
      if (String(url).endsWith('type=AAAA')) throw new TypeError('Failed to fetch');
      return json({ Status: 0, Answer: [{ type: 5, data: 'alias.example.' }, { type: 1, TTL: 300, data: '10.0.0.7' }] });
    };
    const half = await dohLookup('half.example');
    assert.deepEqual(half, { addresses: ['10.0.0.7'], ttl: 300, incomplete: true });
    assert.equal(await resolvesLocal('half.example'), true, 'the A answer alone shows it leads inward');
    globalThis.fetch = async () => json({ Status: 3 });
    assert.deepEqual(await dohLookup('none.example'), { addresses: [], ttl: 0, incomplete: false });
    globalThis.fetch = async () => { throw new TypeError('Failed to fetch'); };
    await assert.rejects(dohLookup('gone.example'), (error) => error.transport === true);
    globalThis.fetch = async (url) => (String(url).endsWith('type=A') ? new Response('', { status: 400 }) : Promise.reject(new TypeError('x')));
    await assert.rejects(dohLookup('bad.example'), (error) => error.transport !== true, 'one refusal is no transport failure');
  } finally {
    globalThis.fetch = real;
  }
}
assert.equal(local('[2606:4700:4700::1111]'), false);

// Network request IDs contain dots in real CDP; redirect hops cannot name a final body.
const background = readFileSync(new URL('../background.js', import.meta.url), 'utf8').replace(/^import .*;\n/gm, '');
const sent = [];
const event = { addListener() {} };
/** The service worker's listener for tabs Chrome opens (none while it watches none). */
let tabCreated = () => {};
let tabsWatched = false;
/** The service worker's listener for Chrome's debugger events (dialogs, file choosers). */
let debuggerEvent = () => {};
// What the service worker writes, and its listener for Chrome's site access taken back.
const bgWrites = [];
let accessRemoved = null;
const closedTabs = [];
// The engine as the service worker reaches it (a thread's PDF): what it was asked, and
// its answers (a file named gone.pdf is not there).
const engineAsked = [];
// The engine as the background reaches it: asked on the request channel, answering on
// the asker's own (`answersFor`); nothing comes back on the request channel itself.
class EngineChannel {
  static made = [];
  constructor(name) { this.name = name; EngineChannel.made.push(this); }
  close() { this.closed = true; }
  postMessage(message) {
    engineAsked.push(message);
    queueMicrotask(() => {
      const missing = message.op === 'read' && message.args.path.endsWith('gone.pdf');
      const reply = EngineChannel.made.find((channel) => channel.name === `answers:${message.from}` && !channel.closed);
      reply?.onmessage?.({ data: missing
        ? { to: message.from, id: message.id, ok: false, error: { message: 'no such file' } }
        : { to: message.from, id: message.id, ok: true, result: message.op === 'read' ? { bytes: new Uint8Array([37, 80, 68, 70]) } : {} } });
    });
  }
}
// The service worker's events (its fetch handler is checked below), in a Chrome with
// static routes.
const workerEvents = {};
/** The service worker's onInstalled listener. */
let installed = null;
/** The service worker's listener for messages (a panel's, the engine's). */
let messageListener = () => false;
const pageContext = vm.createContext({
  // The service worker's own: where it was loaded from, and its events.
  location: new URL('chrome-extension://spindle/background.js'), TextDecoder, TextEncoder,
  self: { InstallEvent: class { addRoutes() {} }, addEventListener(type, listener) { workerEvents[type] = listener; } },
  console, setTimeout, clearTimeout, URL, crypto: globalThis.crypto, read: async () => ({ reading: true, acting: true }), PAGE_ACCESS: [], ACTING_NAMES, uncertain, pagesRecord, BROWSER: 'Chrome',
  BroadcastChannel: EngineChannel, REQUESTS: 'requests', answersFor: (page) => `answers:${page}`, startEngine: async () => {}, Response, queueMicrotask,
  write: async (store, key, value) => { bgWrites.push([store, key, value]); },
  pageSwitches: () => ({ reading: true, acting: true }),
  chrome: {
    sidePanel: { setPanelBehavior: async () => {} },
    runtime: { onInstalled: { addListener(listener) { installed = listener; } }, onMessage: { addListener(listener) { messageListener = listener; } }, getURL: (path) => `chrome-extension://spindle/${path}` },
    permissions: { contains: async () => true, onRemoved: { addListener(listener) { accessRemoved = listener; } } },
    debugger: { onDetach: event, onEvent: { addListener(listener) { debuggerEvent = listener; } }, sendCommand: async (_, method, params) => { sent.push({ method, params }); return { body: 'pong' }; } },
    tabs: { get: async (tab) => ({ id: tab, url: 'https://public.example/' }), onRemoved: event, onCreated: { addListener(listener) { tabCreated = listener; tabsWatched = true; }, removeListener(listener) { if (tabCreated === listener) { tabCreated = () => {}; tabsWatched = false; } } }, remove: async (tab) => { closedTabs.push(tab); } },
    // Chrome's session storage, in memory.
    storage: { session: (() => {
      const kept = {};
      return {
        get: async (key) => (key === null ? { ...kept } : key in kept ? { [key]: kept[key] } : {}),
        set: async (values) => { Object.assign(kept, values); },
        remove: async (keys) => { for (const key of [keys].flat()) delete kept[key]; },
      };
    })() },
  },
});
vm.runInContext(`${background}\nglobalThis.test = { watched, attached, network, recordNetwork, cdp, calls, inPage, attach, veto, unveto, vetoed, onTab, run, pdf, PART, actions, closeOpened, watchTabs, claimOpened };`, pageContext);
const bg = pageContext.test;
// A thread's PDF is answered as a PDF, for Chrome's own viewer, read from the engine;
// nothing else is answered there (no page, no SVG, no path out of the workspace).
{
  const answer = await bg.pdf(new URL('chrome-extension://spindle/file/0123456789abcdef/out%20dir/report.pdf'));
  assert.equal(answer.status, 200);
  assert.equal(answer.headers.get('content-type'), 'application/pdf');
  assert.equal(answer.headers.get('x-content-type-options'), 'nosniff');
  assert.match(answer.headers.get('content-disposition'), /^inline; filename\*=UTF-8''report\.pdf$/);
  assert.deepEqual([...new Uint8Array(await answer.arrayBuffer())], [37, 80, 68, 70]);
  const asked = engineAsked.find((message) => message.op === 'read');
  assert.deepEqual({ ...asked.args }, { id: '0123456789abcdef', path: 'out dir/report.pdf' });
  // (An address's `..` parts are resolved by URL parsing itself, before the worker sees
  // them: `%2E%2E` here becomes /file/report.pdf, which names no thread; an encoded slash
  // stays, and is refused.)
  for (const refused of ['0123456789abcdef/page.html', '0123456789abcdef/drawing.svg', '0123456789abcdef/report.pdf.html', 'NOTAHEXTHREAD000/report.pdf',
    '0123456789abcdef//report.pdf', '0123456789abcdef/%2E%2E/report.pdf', '0123456789abcdef/a%2F..%2F..%2Freport.pdf', '0123456789abcdef/%E0%A4%A.pdf', '0123456789abcdef/']) {
    const before = engineAsked.length;
    const reply = await bg.pdf(new URL(`chrome-extension://spindle/file/${refused}`));
    assert.ok(reply.status >= 400, refused);
    assert.equal(reply.headers.get('content-type'), 'text/plain; charset=utf-8', refused);
    assert.equal(engineAsked.length, before, `nothing read for ${refused}`);
  }
  const gone = await bg.pdf(new URL('chrome-extension://spindle/file/0123456789abcdef/gone.pdf'));
  assert.equal(gone.status, 404);
  assert.match(await gone.text(), /^gone\.pdf could not be read: no such file$/);
  // Each read opened the engine's channel only while it asked (no page's file bytes come
  // to the worker otherwise).
  assert.ok(EngineChannel.made.length >= 2 && EngineChannel.made.every((channel) => channel.closed), 'the channel closed after each read');
  // The fetch handler answers Spindle's own PDF addresses (GET) and previews (navigations)
  // only: a site's `/file/...` a thread fetches, or another method, goes on untouched.
  const fetched = (url, method = 'GET', mode = 'navigate') => {
    let answered = false;
    workerEvents.fetch({ request: { url, method, mode }, respondWith(answer) { answered = true; answer.catch?.(() => {}); } });
    return answered;
  };
  assert.equal(typeof workerEvents.fetch, 'function', 'a fetch handler where Chrome has static routes');
  assert.equal(fetched('chrome-extension://spindle/file/0123456789abcdef/report.pdf'), true);
  assert.equal(fetched('chrome-extension://spindle/file/0123456789abcdef/report.pdf', 'GET', 'no-cors'), true, 'a PDF also for a fetch (the viewer may fetch it again)');
  assert.equal(fetched('https://drive.google.com/file/d/1/view'), false, "a site's /file/ untouched");
  assert.equal(fetched('chrome-extension://spindle/file/0123456789abcdef/report.pdf', 'POST'), false, 'only GET');
  assert.equal(fetched('chrome-extension://spindle/preview.html'), true, 'a preview frame served');
  assert.equal(fetched('chrome-extension://spindle/preview.html', 'GET', 'cors'), false, 'a preview only as a navigation');
  assert.equal(fetched('chrome-extension://spindle/panel.html'), false, "Spindle's other pages untouched");
}
// Installed or updated: a new install's switches are off with Before acting on Auto; an
// install from when Full was the default keeps Full as it updates; one that chose keeps
// its choice; a record that cannot be read is never written over.
for (const [kept, reason, written] of [
  [undefined, 'install', { reading: false, acting: false, confirm: 'auto' }],
  [{ reading: true, acting: true }, 'update', { reading: true, acting: true, confirm: 'full' }],
  [{ reading: true, acting: true, confirm: 'auto' }, 'update', null],
  [{ reading: false, acting: false, confirm: 'auto' }, 'update', null],
  [Error('unreadable'), 'update', null],
]) {
  pageContext.read = async () => {
    if (kept instanceof Error) throw kept;
    return kept;
  };
  const before = bgWrites.length;
  await installed({ reason });
  const wrote = bgWrites.slice(before).filter(([store, key]) => store === 'kv' && key === 'pages').map(([, , value]) => value);
  assert.equal(JSON.stringify(wrote), JSON.stringify(written ? [written] : []), `${reason} over ${JSON.stringify(kept ?? null)}`);
}
// Chrome's site access taken back turns both switches off and keeps Before acting.
pageContext.read = async () => ({ reading: true, acting: true, confirm: 'ask' });
pageContext.PAGE_ACCESS = ['https://*/*'];
pageContext.chrome.debugger.getTargets = async () => [];
await accessRemoved({ origins: ['https://*/*'] });
assert.equal(JSON.stringify(bgWrites.at(-1)), JSON.stringify(['kv', 'pages', { reading: false, acting: false, confirm: 'ask' }]), 'Before acting outlives site access');
// A record it cannot read is still switched off, and its Before acting is written as Ask
// (never left unnamed, which an update would take for a pre-Auto record and make Full).
pageContext.read = async () => {
  throw Error('unreadable');
};
await accessRemoved({ origins: ['https://*/*'] });
assert.equal(JSON.stringify(bgWrites.at(-1)), JSON.stringify(['kv', 'pages', { confirm: 'ask', reading: false, acting: false }]), 'an unreadable choice is written as Ask');
pageContext.read = async () => ({ reading: true, acting: true });
pageContext.PAGE_ACCESS = [];
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
// Closing Chrome's debugging bar keeps Spindle off that tab for every later action, not
// just the next one, until page actions go off and on (letGo clears the veto).
await bg.veto(9);
for (let n = 0; n < 2; n++) await assert.rejects(bg.attach(9), /closed Chrome's debugging bar/, `attempt ${n + 1} is refused`);
// Closing the bar detaches every tab at once: vetoes written side by side all hold.
await Promise.all([bg.veto(11), bg.veto(12), bg.veto(13)]);
for (const tab of [9, 11, 12, 13]) assert.equal(await bg.vetoed(tab), true, `tab ${tab} stays vetoed`);
// And not only what goes through the debugger: choosing, scrolling, moving and closing a
// vetoed tab are refused too, before anything is done (a plain failure, not unknown).
for (const [name, input] of [['close_tab', { tab: 9 }], ['navigate', { tab: 9, action: 'reload' }], ['scroll', { tab: 9 }], ['select', { tab: 9, ref: 'a1', values: ['x'] }], ['select', { tab: 9, ref: 'a1', upload: [{ name: 'x.pdf', type: '', size: 0, base64: '' }] }]]) {
  await assert.rejects(bg.run(name, input), /closed Chrome's debugging bar/, `${name} on a vetoed tab is refused`);
}
assert.deepEqual(closedTabs, [], 'a vetoed tab is not closed');
await bg.unveto(12);
assert.equal(await bg.vetoed(12), false, 'a closed tab drops its own veto');
assert.equal(await bg.vetoed(13), true, 'and only its own');
await bg.unveto();
for (const tab of [9, 11, 13]) assert.equal(await bg.vetoed(tab), false, `page actions off and on clear tab ${tab}`);
// Actions on one tab never interleave: a second waits until the first has finished.
const order = [];
const step = (name, ms) => () => new Promise((done) => { order.push(`${name} starts`); setTimeout(() => { order.push(`${name} ends`); done(); }, ms); });
await Promise.all([bg.onTab(4, step('A', 30)), bg.onTab(4, step('B', 1)), bg.onTab(5, step('C', 1))]);
assert.deepEqual(order.filter((line) => line[0] !== 'C'), ['A starts', 'A ends', 'B starts', 'B ends']);
assert.ok(order.indexOf('C starts') < order.indexOf('A ends'), 'another tab does not wait');
// An action that fails once its input reached the page is unknown (it may have acted); one
// that fails before is a plain failure. A lost answer is unknown for actions only.
pageContext.chrome.debugger.attach = async () => {};
pageContext.chrome.tabs.get = async (tab) => ({ id: tab, url: 'https://public.example/', status: 'complete' });
pageContext.chrome.debugger.sendCommand = async (_, method, params) => {
  if (params?.type === 'keyUp') throw new Error('Target closed');
};
const cut = await bg.run('press', { key: 'Enter', tab: 14 });
assert.equal(cut.unknown, true, JSON.stringify(cut));
assert.match(cut.content, /began acting on the page, then failed \(.*Target closed/);
pageContext.chrome.debugger.attach = async () => { throw new Error('no'); };
await assert.rejects(bg.run('press', { key: 'Enter', tab: 15 }), /would not let Spindle act/, 'nothing sent: a plain failure');
vm.runInContext('for (const timer of attached.values()) clearTimeout(timer);', pageContext);
// The page refusing an in-page change (no such option) changed nothing: a plain failure.
pageContext.chrome.scripting = { executeScript: async () => [{ result: { error: 'no option of abc1 is Z' } }] };
await assert.rejects(bg.run('select', { ref: 'abc1', values: ['Z'], tab: 16 }), /no option/);
// A read safe to repeat that scrolled, then failed, just failed.
let reads = 0;
pageContext.chrome.scripting.executeScript = async ({ args }) => {
  if (args?.[0] === 'scroll') return [{ result: { value: { y: 0, height: 0, view: 0 } } }];
  if (++reads > 1) throw new Error('Frame was removed');
  return [{ result: { url: 'https://public.example/', title: 'Feed', text: 'one' } }];
};
const reread = await bg.run('read_page', { tab: 17, scroll: 3 });
assert.equal(reread.unknown, undefined, JSON.stringify(reread));
assert.match(reread.error, /then failed \(Frame was removed\)/);
// A tab named by anything but its number is refused in words that say how to name one
// (Chrome's own, "No matching signature", do not), Chrome asked nothing; a whole number in
// quotes, as some models write it, was made the number on the engine's side (`numbersIn`).
{
  const getTab = pageContext.chrome.tabs.get;
  let asked = 0;
  pageContext.chrome.tabs.get = async (tab) => { asked += 1; return getTab(tab); };
  for (const tab of ['2054624637', 2054624637.5, 'tab 5', true]) {
    const refused = await bg.run('read_page', { tab });
    assert.match(refused.error ?? '', /^tab is a tab's number from list_tabs, such as 2054624637, not .+; nothing was done$/, JSON.stringify(refused));
  }
  assert.equal(asked, 0, 'Chrome was asked nothing');
  pageContext.chrome.tabs.get = getTab;
  const tool = (name) => PAGE_TOOLS.find((one) => one.name === name);
  const given = { tab: '2054624637', max_chars: ' 28000 ', find: '12' };
  assert.deepEqual(numbersIn(tool('read_page'), given), { tab: 2054624637, max_chars: 28000, find: '12' }, 'quoted whole numbers in number fields become numbers; a text field keeps its digits');
  assert.equal(given.tab, '2054624637', 'the input as given is left as it was');
  for (const [name, input] of [['read_page', { tab: '1.5' }], ['read_page', { tab: 'tab 5' }], ['read_page', { tab: '99999999999999999999' }], ['read_page', { tab: '' }], ['type', { ref: '12', text: '34' }]]) {
    assert.equal(numbersIn(tool(name), input), input, `${name} ${JSON.stringify(input)} stays as given`);
  }
  assert.deepEqual(numbersIn(tool('wait_for'), { seconds: '2.5', tab: '7' }), { seconds: 2.5, tab: 7 }, 'a number field takes a decimal');
  assert.deepEqual(numbersIn(tool('read_page'), { max_chars: '2.5' }), { max_chars: '2.5' }, 'an integer field takes only a whole number');
  for (const odd of [null, 'text', [1]]) assert.equal(numbersIn(tool('read_page'), odd), odd);
  assert.ok(engineFile.includes('prepared(tool.name, numbersIn(tool, input), call)'), 'the engine hands every page tool its numbers as numbers');
  console.log('ok a tab given as a quoted number is taken as that number; any other non-number is refused in words that say how to name one');
}
// A workspace file shown in Spindle's own tab (view.html) is read as the file when
// Chrome keeps scripts out of the tab: `find` gives its matching lines, and a screenshot
// of such a tab is refused with what to do instead.
{
  const getTab = pageContext.chrome.tabs.get;
  pageContext.chrome.tabs.get = async (tab) => ({ id: tab, url: `chrome-extension://spindle/view.html?thread=0123456789abcdef&path=notes/page.html`, title: 'page.html' });
  pageContext.chrome.scripting.executeScript = async () => { throw new Error('Cannot access a chrome-extension:// URL of different extension'); };
  const asked = engineAsked.length;
  const shown = await bg.run('read_page', { tab: 18 });
  assert.equal(shown.error, undefined, JSON.stringify(shown));
  assert.match(shown.content, /^Address: chrome-extension:\/\/spindle\/view\.html\?thread=0123456789abcdef&path=notes\/page\.html\nTitle: page\.html\nWorkspace file: notes\/page\.html \(the file itself\)\n\n%PDF$/, shown.content);
  assert.deepEqual(engineAsked.slice(asked).filter((message) => message.op !== 'ping').map((message) => [message.op, message.args.id, message.args.path]), [['read', '0123456789abcdef', 'notes/page.html']], 'the file read from the engine, by its thread');
  const found = await bg.run('read_page', { tab: 18, find: '/^%pdf/' });
  assert.match(found.content, /Workspace file: notes\/page\.html \(the file itself\)\n\n\[1 matching line, each with its neighbours\]\n%PDF$/, found.content);
  const none = await bg.run('read_page', { tab: 18, find: 'absent' });
  assert.match(none.content, /\[no line matches "absent"\]$/, none.content);
  const pictured = await bg.run('screenshot', { tab: 18 });
  assert.match(pictured.error, /does not picture extension pages, so tab 18 \(notes\/page\.html from the workspace\) cannot be pictured; read_page on it gives the file's text/, JSON.stringify(pictured));
  assert.deepEqual(engineAsked.slice(asked).filter((message) => message.op !== 'ping').map((message) => message.op), ['read', 'read', 'read'], 'the file read once per call, nothing else asked of the engine');
  pageContext.chrome.tabs.get = getTab;
  console.log('ok a workspace file in Spindle\'s own tab is read as the file, found in with `find`, and never pictured');
}
// Moving and closing tabs run through the lines that mark them acting; Chrome refusing a
// move it was asked for is said plainly (nothing moved).
pageContext.chrome.runtime.getURL = (path) => `chrome-extension://spindle/${path}`;
const moved = [];
Object.assign(pageContext.chrome.tabs, {
  update: async (tab, { url }) => { moved.push(`update ${tab} ${url}`); },
  create: async ({ url }) => { moved.push(`create ${url}`); return { id: 31 }; },
  remove: async (tab) => { moved.push(`remove ${tab}`); },
});
assert.match((await bg.run('navigate', { url: 'https://public.example/next', tab: 18 })).content, /^Tab 18 is at/);
assert.match((await bg.run('navigate', { url: 'https://public.example/new', new_tab: true })).content, /^Tab 31 is at/);
assert.match((await bg.run('navigate', { url: 'chrome-extension://spindle/view.html?thread=t&path=a.md' })).content, /^Opened a\.md/);
assert.match((await bg.run('close_tab', { tab: 19 })).content, /^Closed tab 19/);
assert.deepEqual(moved, ['update 18 https://public.example/next', 'create https://public.example/new', 'create chrome-extension://spindle/view.html?thread=t&path=a.md', 'remove 19']);
// The tabs Spindle opened for a thread close once its turn is done: its new tabs and a
// tab a page opened on its action. Not one kept open for the user, one the user pinned,
// another thread's, a task's (until asked), a workspace file's, nor a window's last tab.
{
  const tabs = new Map();
  let next = 40;
  const removed = [];
  const was = { ...pageContext.chrome.tabs };
  Object.assign(pageContext.chrome.tabs, {
    // Loaded at once (else each new tab is waited on for the 20 s a page may take).
    create: async ({ url }) => { const tab = { id: next++, url, windowId: 1, active: true, status: 'complete' }; tabs.set(tab.id, tab); return tab; },
    get: async (id) => tabs.get(id) ?? (id === 18 ? { id, url: 'https://public.example/', windowId: 1 } : Promise.reject(new Error('No tab'))),
    query: async ({ windowId }) => [...tabs.values(), { id: 18, windowId: 1 }].filter((tab) => tab.windowId === windowId),
    remove: async (ids) => { for (const id of [ids].flat()) { removed.push(id); tabs.delete(id); } },
  });
  const mine = { thread: 't1', task: false };
  const opened = async (input, owner = mine) => (await bg.run('navigate', { url: 'https://public.example/a', new_tab: true, ...input }, undefined, null, owner)).details.tab;
  const plain = await opened({});
  const kept = await opened({ keep_open: true });
  const pinned = await opened({});
  tabs.get(pinned).pinned = true;
  const other = await opened({}, { thread: 't2', task: false });
  const tasks = await opened({}, { thread: 't1', task: true });
  const unowned = await opened({}, null);
  await bg.run('navigate', { url: 'chrome-extension://spindle/view.html?thread=t1&path=a.md' }, undefined, null, mine);
  const file = next - 1;
  // A page opening a window on the thread's action (Chrome's debugger says which page
  // and where to, during the action or just after): the tab Chrome makes for it is the
  // thread's, also when Chrome says its address only a moment later, and a blank window's
  // blank tab. Another page's window, one before the action, a tab no page opened, a
  // window to another address than the tab's, and a tab whose address Chrome never says
  // (the user's own new tab, say) are not.
  const now = Date.now();
  bg.watchTabs();
  bg.actions.push({ owner: mine, tab: 18, from: now - 1000, to: now + 3000 }, { owner: mine, tab: 19, from: now + 60_000, to: now + 63_000 });
  for (const [id, source, url, said = url, shown = url] of [
    [90, 18, 'https://public.example/popup'], [91, 77, 'https://public.example/other'], [92, 19, 'https://public.example/later'],
    [93, null, 'https://public.example/mine'], [94, 18, 'https://public.example/elsewhere', 'https://public.example/not-this'],
    [95, 18, 'https://public.example/hidden', 'https://public.example/hidden', ''], [96, 18, 'about:blank', ''],
    [97, 18, 'https://public.example/late', 'https://public.example/late', ''],
  ]) {
    // (The hidden one: the user's own new tab, gone somewhere else by the time Chrome says.)
    tabs.set(id, { id, url: id === 95 ? 'https://public.example/the-users' : url, windowId: 1 });
    if (source != null) debuggerEvent({ tabId: source }, 'Page.windowOpen', { url: said });
    tabCreated({ id, pendingUrl: shown || undefined });
  }
  // The turn ends at once: a tab whose address Chrome has yet to say is waited on.
  await bg.closeOpened('t1');
  assert.deepEqual(removed.sort(), [plain, 90, 96, 97].sort(), 'its turn\'s new tab and the ones its click opened');
  for (const stays of [kept, pinned, other, tasks, unowned, file, 91, 92, 93, 94, 95]) assert.ok(tabs.has(stays), `tab ${stays} stays`);
  // Asked again: nothing more (each was forgotten), until its tasks' are asked for.
  await bg.closeOpened('t1');
  assert.deepEqual(removed.sort(), [plain, 90, 96, 97].sort());
  await bg.closeOpened('t1', { tasks: true });
  assert.ok(removed.includes(tasks) && tabs.has(kept), 'its tasks\' tab, the kept one still not');
  // A window holding only the thread's tabs keeps its active one (closing every tab
  // would close the window and the panel beside it).
  tabs.clear();
  removed.length = 0;
  const alone = [{ id: 60, active: false }, { id: 61, active: true }].map((tab) => ({ ...tab, url: 'https://public.example/', windowId: 2 }));
  for (const tab of alone) tabs.set(tab.id, tab);
  pageContext.chrome.tabs.query = async ({ windowId }) => [...tabs.values()].filter((tab) => tab.windowId === windowId);
  await pageContext.chrome.storage.session.set({ 'opened:60': { thread: 't3' }, 'opened:61': { thread: 't3' } });
  await bg.closeOpened('t3');
  assert.deepEqual(removed, [60], 'the active one stays');
  // A popup window a page opened closes whole; a tab still loading its first page closes.
  tabs.clear();
  removed.length = 0;
  tabs.set(70, { id: 70, url: 'https://public.example/', windowId: 3, active: true });
  tabs.set(71, { id: 71, url: '', pendingUrl: 'https://public.example/slow', windowId: 4, active: false });
  tabs.set(72, { id: 72, url: 'https://public.example/', windowId: 4, active: true });
  pageContext.chrome.windows = { get: async (id) => ({ id, type: id === 3 ? 'popup' : 'normal' }) };
  await pageContext.chrome.storage.session.set({ 'opened:70': { thread: 't4' }, 'opened:71': { thread: 't4' } });
  await bg.closeOpened('t4');
  assert.deepEqual(removed.sort(), [70, 71], 'the popup and the loading tab');
  // New tabs are watched only until no window an action may have opened can still come
  // (a listener left on would keep the service worker up while the user browses).
  bg.actions.length = 0;
  bg.claimOpened();
  assert.ok(!tabsWatched, 'no tabs watched with no action');
  // One under way is watched throughout, whatever comes meanwhile (no dialog is open, so
  // this one ends without acting).
  const acting = bg.run('dialog', { tab: 18, accept: true }, undefined, null, mine).catch(() => {});
  bg.claimOpened();
  assert.ok(tabsWatched, 'watched while an action is under way');
  await acting;
  assert.ok(!tabsWatched && !bg.actions.length, 'not after one that did not act');
  bg.watchTabs();
  bg.actions.push({ owner: mine, tab: 18, from: Date.now() - 6000, to: Date.now() - 4700 });
  bg.claimOpened();
  assert.ok(tabsWatched, 'watched while its window may still come');
  await new Promise((done) => setTimeout(done, 700));
  assert.ok(!tabsWatched, 'and no longer after');
  bg.actions.length = 0;
  delete pageContext.chrome.windows;
  for (const key of Object.keys(pageContext.chrome.tabs)) if (!(key in was)) delete pageContext.chrome.tabs[key];
  Object.assign(pageContext.chrome.tabs, was);
}
console.log('ok the tabs Spindle opened for a thread close once its turn is done, but those it should leave');
// While the user writes in a Spindle panel's box (the panel says until when), a tab
// Spindle opens opens behind the one showing (shown, Chrome would give it the box's
// keys), and the thread's tools without a tab go on in it until it is shown, as they
// would in one opened in front; another thread's, and the tab list, still name the tab
// the user looks at. Only Spindle's own pages say so, and never for longer than a minute.
{
  const tabs = new Map([[60, { id: 60, url: 'https://user.example/', windowId: 1, active: true, status: 'complete' }]]);
  let next = 61;
  const made = [];
  const was = { ...pageContext.chrome.tabs };
  Object.assign(pageContext.chrome.tabs, {
    create: async ({ url, active }) => { const tab = { id: next++, url, windowId: 1, active, status: 'complete' }; made.push(active); tabs.set(tab.id, tab); return tab; },
    get: async (id) => tabs.get(id) ?? Promise.reject(new Error('No tab')),
    query: async ({ active, windowId } = {}) => [...tabs.values()].filter((tab) => (active == null || tab.active === active) && (windowId == null || tab.windowId === windowId)),
  });
  pageContext.chrome.windows = { getLastFocused: async () => ({ id: 1 }) };
  const panel = { id: undefined, url: 'chrome-extension://spindle/panel.html' };
  const says = (until, sender = panel) => messageListener({ type: 'typing', until }, sender, () => {});
  const settledStore = () => new Promise((done) => setTimeout(done, 0));
  const mine = { thread: 'w1', task: false };
  const open = (input = {}, owner = mine) => bg.run('navigate', { url: 'https://public.example/w', new_tab: true, ...input }, undefined, null, owner);
  const lands = async (owner) => (await bg.run('tab_of', {}, undefined, null, owner)).details.tab;
  const front = await open();
  assert.equal(made.at(-1), true, 'not writing: in front, as before');
  tabs.get(60).active = false;
  says(Date.now() + 5_000);
  await settledStore();
  const behind = await open();
  assert.equal(made.at(-1), false, 'writing: behind the one showing');
  assert.match(behind.content, new RegExp(`Opened as tab ${behind.details.tab} behind the one showing, as the user is writing in Spindle: tools without a tab act on it`), 'the model is told');
  assert.equal(await lands(mine), behind.details.tab, "the thread's tools without a tab go on in it");
  assert.equal(await lands({ thread: 'w2', task: false }), front.details.tab, "another thread's land on the tab the user looks at");
  assert.equal(await lands(null), front.details.tab, 'and an unowned call');
  const unowned = await open({}, null);
  assert.equal(made.at(-1), true, 'a call of no thread opens in front even then (nothing would go on in a tab behind)');
  assert.doesNotMatch(unowned.content, /behind the one showing/);
  tabs.delete(unowned.details.tab);
  assert.match((await bg.run('list_tabs', {}, undefined, null, mine)).content, new RegExp(`tab ${front.details.tab} \\(the user is looking at it\\)[\\s\\S]*tab ${behind.details.tab} \\(yours, behind the one showing`), 'the list names both');
  const file = await bg.run('navigate', { url: 'chrome-extension://spindle/view.html?thread=w1&path=a.md' }, undefined, null, mine);
  assert.equal(made.at(-1), false, 'a workspace file opened for the user, behind too');
  assert.match(file.content, /\(tab \d+, behind the one showing, as the user is writing in Spindle\)\.$/);
  // Shown: the tab the user looks at again (it is now), and let go of.
  tabs.get(front.details.tab).active = false;
  tabs.get(behind.details.tab).active = true;
  assert.equal(await lands(mine), behind.details.tab, 'shown: still that tab');
  tabs.get(behind.details.tab).active = false;
  tabs.get(front.details.tab).active = true;
  assert.equal(await lands(mine), front.details.tab, 'once shown, the thread goes back to the tab the user looks at');
  // Closed: let go of. A tab opened in front takes its place.
  says(Date.now() + 5_000);
  await settledStore();
  const again = await open();
  tabs.delete(again.details.tab);
  assert.equal(await lands(mine), front.details.tab, 'closed: the tab the user looks at');
  const behind2 = (await open()).details.tab;
  says(0);
  await settledStore();
  const shown = await open();
  assert.equal(made.at(-1), true, 'the panel said writing stopped: in front again');
  tabs.get(front.details.tab).active = false;
  tabs.get(shown.details.tab).active = true;
  assert.equal(await lands(mine), shown.details.tab, 'a tab opened in front replaces the one behind');
  assert.ok(tabs.has(behind2));
  // A web page cannot say it; a panel's word holds at most a minute; tasks keep their own.
  says(Date.now() + 5_000, { id: undefined, url: 'https://evil.example/' });
  await settledStore();
  await open();
  assert.equal(made.at(-1), true, "a web page's word is not taken");
  says(Date.now() + 10 * 60_000);
  await settledStore();
  const { typing } = await pageContext.chrome.storage.session.get('typing');
  assert.ok(typing <= Date.now() + 60_000, `held a minute at most: ${typing - Date.now()} ms`);
  const task = (await open({}, { thread: 'w1', task: true })).details.tab;
  assert.equal(await lands({ thread: 'w1', task: true }), task, "a task's tab is its own");
  assert.equal(await lands(mine), shown.details.tab, "and not the thread's");
  says(0);
  await settledStore();
  delete pageContext.chrome.windows;
  for (const key of Object.keys(pageContext.chrome.tabs)) if (!(key in was)) delete pageContext.chrome.tabs[key];
  Object.assign(pageContext.chrome.tabs, was);
}
console.log('ok a tab Spindle opens while the user writes in its panel opens behind, and its thread goes on in it until it is shown');
// The panel says so: while its box has the focus and holds text (half a minute ahead,
// told again once half spent), or a key went into it in the last 8 s; at once when the
// box or the panel loses the focus; not at every key.
{
  const panelSource = readFileSync(new URL('../panel.mjs', import.meta.url), 'utf8');
  const start = panelSource.indexOf('const WRITING_MS');
  const code = panelSource.slice(start, panelSource.indexOf('for (const type of', start));
  let now = 1_000_000;
  const told = [];
  const box = { value: '' };
  const page = { focused: true, active: box };
  const run = vm.runInNewContext(`${code}; tellWriting`, {
    Date: { now: () => now }, $: () => box, chrome: { runtime: { sendMessage: async (message) => { told.push(message.until && message.until - now); } } },
    document: { hasFocus: () => page.focused, get activeElement() { return page.active; } },
  });
  run({ type: 'focus' });
  assert.deepEqual(told, [8000], 'the box takes the focus: 8 s');
  box.value = 'go to';
  now += 1000; run({ type: 'input' });
  assert.deepEqual(told, [8000, 30_000], 'text in it: half a minute');
  now += 1000; run({ type: 'input' });
  now += 10_000; run();
  assert.equal(told.length, 2, 'not again while the last telling holds well');
  now += 5000; run();
  assert.deepEqual(told.slice(2), [30_000], 'told again once half spent');
  page.active = null; run({ type: 'blur' });
  assert.deepEqual(told.slice(3), [0], 'the box lost the focus: at once');
  run({ type: 'blur' });
  assert.equal(told.length, 4, 'and once');
  box.value = '';
  page.active = box; run({ type: 'focus' });
  now += 9000; run();
  assert.equal(told.length, 5, 'an empty box no key went into for 8 s: its telling just runs out');
  run({ type: 'input' });
  page.focused = false; run({ type: 'blur' });
  assert.deepEqual(told.slice(5), [8000, 0], "the panel lost the focus (Chrome showing a tab): at once");
}
console.log('ok the panel says while the user writes in its box, and at once when they stop');
pageContext.chrome.tabs.update = async () => { throw new Error('Tabs cannot be edited right now'); };
assert.match((await bg.run('navigate', { url: 'https://public.example/x', tab: 18 })).error, /could not go there/);
// An action the user allowed (Before acting) acts only on what they were shown: the same
// tab, site and page (Chrome's document id: another after a reload or a link followed),
// and for a dialog the same dialog. Anything else is refused before acting (a plain
// failure: nothing done), checked just before the action goes; unasked actions skip it.
{
  const sentNow = [];
  let documentId = 'doc-1';
  let url = 'https://mail.example/inbox';
  pageContext.chrome.tabs.get = async (tab) => ({ id: tab, url, status: 'complete', title: 'Inbox' });
  pageContext.chrome.scripting.executeScript = async () => [{ documentId, result: true }];
  pageContext.chrome.debugger.attach = async () => {};
  pageContext.chrome.debugger.sendCommand = async (_, method) => { sentNow.push(method); return {}; };
  Object.assign(pageContext.chrome.tabs, {
    update: async (tab, change) => { moved.push(`update ${tab} ${change.url}`); },
    reload: async (tab) => { moved.push(`reload ${tab}`); },
    remove: async (tab) => { moved.push(`remove ${tab}`); },
  });
  moved.length = 0;
  const shown = await bg.run('tab_of', { tab: 21 });
  assert.deepEqual([shown.details.tab, shown.details.host, shown.details.document, shown.details.url], [21, 'mail.example', 'doc-1', 'https://mail.example/inbox'], 'tab_of names the page shown and its address');
  const allowed = { tab: 21, host: 'mail.example', document: 'doc-1', url: 'https://mail.example/inbox' };
  assert.match((await bg.run('press', { key: 'Enter', tab: 21 }, 'same', allowed)).content, /^Pressed Enter/, 'the same page: it acts');
  assert.ok(sentNow.includes('Input.dispatchKeyEvent'));
  sentNow.length = 0;
  documentId = 'doc-2';
  for (const [name, input] of [['press', { key: 'Enter', tab: 21 }], ['navigate', { action: 'reload', tab: 21 }], ['close_tab', { tab: 21 }]]) {
    await assert.rejects(bg.run(name, input, `reloaded-${name}`, allowed), /tab 21 shows another page than the one the user allowed this on[\s\S]*not done/, `${name} after a reload`);
  }
  url = 'https://payments.example/';
  await assert.rejects(bg.run('press', { key: 'Enter', tab: 21 }, 'moved', allowed), /moved from mail\.example to payments\.example/);
  await assert.rejects(bg.run('press', { key: 'Enter', tab: 22 }, 'other', allowed), /allowed this on tab 21, not tab 22/);
  // Chrome cannot say which page it shows now (a page that hangs, one it keeps extensions
  // out of): its address then, but its #part. Another address: not done.
  pageContext.chrome.scripting.executeScript = async () => { throw new Error('Cannot access contents of the page'); };
  url = 'https://mail.example/compose';
  await assert.rejects(bg.run('press', { key: 'Enter', tab: 21 }, 'unseen', allowed), /tab 21 is at another address than the one the user allowed this on[\s\S]*not done/);
  assert.ok(!sentNow.includes('Input.dispatchKeyEvent'), 'nothing pressed');
  assert.deepEqual(moved, [], 'nothing reloaded or closed');
  url = 'https://mail.example/inbox#thread-7';
  assert.match((await bg.run('close_tab', { tab: 21 }, 'same-address', allowed)).content, /^Closed tab 21/, 'the same address (a #part aside): a tab that will not answer still closes');
  url = 'https://mail.example/inbox';
  assert.match((await bg.run('press', { key: 'Enter', tab: 21 }, 'unasked')).content, /^Pressed Enter/, 'an action not asked about is not held to a page');
  // A dialog: only the one the question showed is answered; tab_of says what it asks
  // (and reads no page: the dialog holds it).
  sentNow.length = 0;
  vm.runInContext("dialogs.set(23, { id: 7, type: 'confirm', message: 'Pay?', at: Date.now() });", pageContext);
  const asking = await bg.run('tab_of', { tab: 23 });
  assert.deepEqual({ ...asking.details.dialog }, { id: 7, type: 'confirm', message: 'Pay?' });
  assert.equal(asking.details.document, undefined);
  await assert.rejects(bg.run('dialog', { accept: true, tab: 23 }, 'stale', { tab: 23, host: 'mail.example', dialog: 6 }), /not the one the user allowed answering/);
  assert.ok(!sentNow.includes('Page.handleJavaScriptDialog'), 'another dialog is not answered');
  assert.match((await bg.run('dialog', { accept: true, tab: 23 }, 'fresh', { tab: 23, host: 'mail.example', dialog: 7 })).content, /^Accepted the confirm "Pay\?"/);
  // A click the user allowed takes the part's description into the page, where it is
  // measured: changed there, nothing is clicked.
  const measured = [];
  pageContext.chrome.scripting.executeScript = async ({ args }) => {
    measured.push(args);
    return [{ result: { error: `[qx1] is now button "Delete", not the button "Send" the user allowed this on, so it was not done` } }];
  };
  sentNow.length = 0;
  await assert.rejects(bg.run('click', { ref: 'qx1', tab: 21 }, 'click', { tab: 21, host: 'mail.example', what: 'button "Send"' }), /is now button "Delete"/);
  assert.equal(measured.at(-1)[0], 'point');
  assert.equal(measured.at(-1)[1].what, 'button "Send"', 'the description goes with the measure');
  assert.ok(!sentNow.includes('Input.dispatchMouseEvent'), 'nothing clicked');
  vm.runInContext('for (const timer of attached.values()) clearTimeout(timer);', pageContext);
  // In the page, a part the user allowed acting on is acted on only while it is what the
  // question named (the same element, its role and name unchanged).
  const options = [{ value: 'alice', text: 'Alice', selected: false }, { value: 'mallory', text: 'Mallory', selected: false }];
  const list = { tagName: 'SELECT', localName: 'select', isConnected: true, multiple: false, options, label: 'Pay to', getAttribute: (name) => (name === 'aria-label' ? list.label : null), hasAttribute: () => false, dispatchEvent() {} };
  pageContext.Event = Event;
  pageContext.__spindle = { mark: 'qx', next: 1, refs: new Map([['qx1', new WeakRef(list)]]), ids: new WeakMap([[list, 'qx1']]) };
  assert.deepEqual([...bg.inPage('select', { ref: 'qx1', values: ['mallory'], what: 'combobox "Pay to"' }).value], ['Mallory'], 'as shown: chosen');
  options[1].selected = false;
  list.label = 'Send money to';
  assert.match(bg.inPage('select', { ref: 'qx1', values: ['mallory'], what: 'combobox "Pay to"' }).error, /\[qx1\] is now combobox "Send money to", not the combobox "Pay to" the user allowed this on, so it was not done/);
  assert.equal(options[1].selected, false, 'nothing chosen');
  assert.deepEqual([...bg.inPage('select', { ref: 'qx1', values: ['alice'] }).value], ['Alice'], 'not asked about: no check');
  // Numbers count: "Pay $10" become "Pay $10000" is another action (a count that changed
  // only asks again); case and spacing do not.
  list.label = 'Send money to 3 people (2 min ago)';
  assert.match(bg.inPage('select', { ref: 'qx1', values: ['alice'], what: 'combobox "Send money to 4 people (5 min ago)"' }).error, /is now combobox "Send money to 3 people \(2 min ago\)", not the combobox "Send money to 4 people \(5 min ago\)"/);
  assert.deepEqual([...bg.inPage('select', { ref: 'qx1', values: ['alice'], what: 'combobox  "send MONEY to 3 people (2 min ago)"' }).value], ['Alice'], 'case and spacing aside, the same words');
  list.label = 'Send money to';
  // A click or typing measures its part first (`point`): changed, nothing is done to the
  // page (not even scrolled to it).
  let scrolled = 0;
  list.scrollIntoView = () => { scrolled += 1; };
  assert.match(bg.inPage('point', { ref: 'qx1', what: 'combobox "Pay to"' }).error, /\[qx1\] is now combobox "Send money to", not the combobox "Pay to" the user allowed this on/);
  assert.equal(scrolled, 0, 'refused before anything is done to the page');
  // Its words past its short name, and where it leads, are what the user was shown too
  // (an amount at the end of a long label; a link's address changed while asked).
  const form = { action: 'https://shop.example/pay', getAttribute: (name) => (name === 'action' ? form.action : null) };
  const pay = { tagName: 'BUTTON', localName: 'button', isConnected: true, innerText: 'Confirm the order of the three items in your basket and pay the total now: $10', type: 'submit', form, getAttribute: () => null, hasAttribute: () => false, closest: () => null, scrollIntoView() { scrolled += 1; } };
  const go = { tagName: 'A', localName: 'a', isConnected: true, innerText: 'Continue', href: 'https://shop.example/next', getAttribute: (name) => (name === 'href' ? go.href : null), hasAttribute: (name) => name === 'href', scrollIntoView() { scrolled += 1; } };
  go.closest = () => go;
  pageContext.__spindle = { mark: 'qx', next: 2, refs: new Map([['qx1', new WeakRef(pay)], ['qx2', new WeakRef(go)]]), ids: new WeakMap([[pay, 'qx1'], [go, 'qx2']]) };
  const payShown = bg.inPage('target', { ref: 'qx1' }).value;
  assert.deepEqual([payShown.words, payShown.dest], ['Confirm the order of the three items in your basket and pay the total now: $10', 'https://shop.example/pay']);
  pay.innerText = 'Confirm the order of the three items in your basket and pay the total now: $10000';
  scrolled = 0;
  assert.match(bg.inPage('point', { ref: 'qx1', what: payShown.what, words: payShown.words, dest: payShown.dest }).error, /saying "Confirm the order of the three items in your basket and pay the total now: \$10000", not "[^"]*\$10" as the user allowed this on, so it was not done/);
  pay.innerText = payShown.words;
  // As the page wrote it: the form's own action attribute changed.
  form.action = 'https://elsewhere.example/pay';
  assert.match(bg.inPage('point', { ref: 'qx1', ...payShown }).error, /leading to https:\/\/elsewhere\.example\/pay, not https:\/\/shop\.example\/pay/);
  const goShown = bg.inPage('target', { ref: 'qx2' }).value;
  go.href = 'https://evil.example/';
  assert.match(bg.inPage('point', { ref: 'qx2', ...goShown }).error, /leading to https:\/\/evil\.example\/, not https:\/\/shop\.example\/next/);
  assert.equal(scrolled, 0, 'refused before anything is done to the page');
  // The focused part: Enter goes to it.
  pageContext.document = { activeElement: go, body: {}, documentElement: {} };
  assert.deepEqual({ ...bg.inPage('focused', {}).value }, { ref: 'qx2', what: 'link "Continue"' });
  pageContext.document.activeElement = pageContext.document.body;
  assert.equal(bg.inPage('focused', {}).value, null, 'nothing focused');
  delete pageContext.document;
  delete pageContext.__spindle;
}
// Enter the user allowed goes only while the focus is where the question showed it.
{
  const pressed = [];
  let focused = { ref: 'qx5', what: 'textbox "Search"' };
  pageContext.chrome.tabs.get = async (tab) => ({ id: tab, url: 'https://mail.example/inbox', status: 'complete', title: 'Inbox' });
  pageContext.chrome.scripting.executeScript = async ({ args }) => [{ documentId: 'doc-1', result: args?.[0] === 'focused' ? { value: focused } : true }];
  pageContext.chrome.debugger.sendCommand = async (_, method) => { pressed.push(method); return {}; };
  const shown = await bg.run('tab_of', { tab: 21, focus: true });
  assert.deepEqual({ ...shown.details.focus }, focused, 'tab_of says which part has the focus, for a key');
  assert.equal('focus' in (await bg.run('tab_of', { tab: 21 })).details, false, 'and reads it for nothing else');
  const allowed = { tab: 21, host: 'mail.example', document: 'doc-1', url: 'https://mail.example/inbox', focus: 'qx5' };
  focused = { ref: 'qx9', what: 'textbox "Amount"' };
  await assert.rejects(bg.run('press', { key: 'Enter', tab: 21 }, 'moved-focus', allowed), /the focus on tab 21 is not where it was when the user allowed this \(it is on textbox "Amount" now\)[\s\S]*not done/);
  assert.ok(!pressed.includes('Input.dispatchKeyEvent'), 'nothing pressed');
  // Where the focus is cannot be told now: not done either.
  const reads = pageContext.chrome.scripting.executeScript;
  pageContext.chrome.scripting.executeScript = async (given) => {
    if (given.args?.[0] === 'focused') throw new Error('Cannot access contents of the page');
    return reads(given);
  };
  await assert.rejects(bg.run('press', { key: 'Enter', tab: 21 }, 'unseen-focus', allowed), /Spindle could not tell where it is now/);
  pageContext.chrome.scripting.executeScript = reads;
  focused = { ref: 'qx5', what: 'textbox "Search"' };
  assert.match((await bg.run('press', { key: 'Enter', tab: 21 }, 'same-focus', allowed)).content, /^Pressed Enter/);
  // A picture of the window is kept only while the tab it was taken for still shows.
  pageContext.PICTURE_ACCESS = ['<all_urls>'];
  pageContext.chrome.tabs.get = async (tab) => ({ id: tab, url: 'https://mail.example/inbox', status: 'complete', title: 'Inbox', active: true, windowId: 1 });
  pageContext.chrome.tabs.captureVisibleTab = async () => 'data:image/jpeg;base64,AA==';
  pageContext.chrome.tabs.query = async () => [{ id: 99, url: 'chrome-extension://spindle/view.html?thread=x' }];
  await assert.rejects(bg.run('screenshot', { tab: 21 }), /tab 21 stopped showing in its window while it was pictured/);
  vm.runInContext('for (const timer of attached.values()) clearTimeout(timer);', pageContext);
}
// Files go into a file field from the page (no dialog): in parts, then at once, held to
// what the field takes; a click on a file field opens no dialog.
{
  const events = [];
  const field = { tagName: 'INPUT', type: 'file', accept: '.pdf,application/msword', multiple: false, disabled: false, isConnected: true, files: [], labels: [], getAttribute: (name) => (name === 'aria-label' ? 'Resume' : null), hasAttribute: () => false, dispatchEvent: (event) => events.push(event.type) };
  const button = { tagName: 'BUTTON', localName: 'button', isConnected: true, innerText: 'Upload resume', getAttribute: () => null, hasAttribute: () => false, closest: () => null };
  const group = { querySelectorAll: () => [field], parentElement: null };
  button.parentElement = group;
  button.querySelectorAll = () => [];
  pageContext.DataTransfer = class { constructor() { this.files = []; this.items = { add: (file) => this.files.push(file) }; } };
  pageContext.File = File;
  pageContext.atob = atob;
  pageContext.__spindle = { mark: 'qx', next: 2, refs: new Map([['qx1', new WeakRef(button)], ['qx2', new WeakRef(field)]]), ids: new WeakMap([[button, 'qx1'], [field, 'qx2']]) };
  const bytes = Buffer.from('%PDF-1.7 a resume');
  // Cut on a multiple of 4: each part decodes alone.
  const cut = Math.floor(bytes.length / 6) * 4;
  const text = bytes.toString('base64');
  assert.equal(bg.inPage('file_part', { key: 'k', index: 0, first: true, data: text.slice(0, cut) }).value, true);
  bg.inPage('file_part', { key: 'k', index: 0, data: text.slice(cut) });
  const put = bg.inPage('files', { ref: 'qx1', key: 'k', files: [{ name: 'resume.pdf', type: 'application/pdf', size: bytes.length }] });
  assert.equal(put.error, undefined, put.error);
  assert.deepEqual([...put.value.put], [`resume.pdf (${bytes.length} bytes)`]);
  assert.equal(put.value.field, 'qx2', 'the field beside the button took it, named by its own ref');
  assert.equal(Buffer.from(await field.files[0].arrayBuffer()).toString(), '%PDF-1.7 a resume', 'whole, from its parts');
  assert.deepEqual(events, [], 'the page hears it once the script has returned');
  await new Promise((done) => setTimeout(done, 0));
  assert.deepEqual(events, ['input', 'change'], 'as a pick');
  for (const [files, why] of [
    [[{ name: 'notes.txt', type: 'text/plain', size: 0 }], /file "Resume" takes \.pdf,application\/msword, not notes\.txt/],
    [[{ name: 'a.pdf', type: 'application/pdf', size: 0 }, { name: 'b.pdf', type: 'application/pdf', size: 0 }], /takes one file/],
    [[{ name: 'c.pdf', type: 'application/pdf', size: 5 }], /did not reach the page whole/],
  ]) {
    bg.inPage('file_part', { key: 'k2', index: 0, first: true, data: '' });
    assert.match(bg.inPage('files', { ref: 'qx2', key: 'k2', files }).error, why);
  }
  const held = field.files;
  await new Promise((done) => setTimeout(done, 0));
  assert.equal(field.files, held, 'a refused pick changes nothing');
  assert.deepEqual(events, ['input', 'change'], 'and the page hears nothing of it');
  group.querySelectorAll = () => [field, field];
  assert.match(bg.inPage('files', { ref: 'qx1', key: 'k3', files: [] }).error, /has no one file field in or beside it/, 'never a guess between two fields');
  group.querySelectorAll = () => [field];
  // A file field shows in the outline as one, with what it takes and holds.
  assert.match(bg.inPage('target', { ref: 'qx2' }).value.what, /^file "Resume"$/);
  // Measured for a click, a hidden one says it is a file field (not "takes no room").
  assert.equal(bg.inPage('point', { ref: 'qx2' }).value.file, true);
  // Chrome's side: parts, then the field, then where the tab is; a click on it is refused.
  const ops = [];
  pageContext.chrome.tabs.get = async (tab) => ({ id: tab, url: 'https://jobs.example/apply', status: 'complete', title: 'Apply' });
  pageContext.chrome.scripting.executeScript = async ({ args }) => {
    ops.push([args[0], args[1].data?.length ?? null]);
    if (args[0] === 'files') return [{ result: { value: { what: 'file "Resume"', field: 'qx5', put: ['resume.pdf (17 bytes)'] } } }];
    if (args[0] === 'point') return [{ result: { value: { x: 1, y: 1, what: 'file "Resume"', covered: null, field: false, file: true } } }];
    return [{ result: { value: true } }];
  };
  const big = 'A'.repeat(bg.PART + 8);
  const done = await bg.run('select', { ref: 'qx2', tab: 40, upload: [{ path: 'resume.pdf', name: 'resume.pdf', type: 'application/pdf', size: (big.length / 4) * 3, base64: big }] });
  assert.match(done.content, /^Put resume\.pdf \(17 bytes\) in file "Resume" \[qx5\] \(the file field of \[qx2\]\)/);
  assert.deepEqual(ops.map(([op]) => op), ['file_part', 'file_part', 'files'], 'its bytes in parts, then the field');
  assert.equal(ops[0][1] + ops[1][1], big.length);
  // Through the page's own code: two files (the second empty) reach a field that takes
  // several, each whole; only the first part starts afresh.
  field.multiple = true;
  const firsts = [];
  pageContext.chrome.scripting.executeScript = async ({ args }) => {
    if (args[0] === 'file_part') firsts.push(Boolean(args[1].first));
    return [{ result: bg.inPage(args[0], args[1]) }];
  };
  const both = await bg.run('select', { ref: 'qx1', tab: 40, upload: [
    { path: 'a.pdf', name: 'a.pdf', type: 'application/pdf', size: (big.length / 4) * 3, base64: big },
    { path: 'b.pdf', name: 'b.pdf', type: 'application/pdf', size: 0, base64: '' },
  ] });
  assert.match(both.content, /^Put a\.pdf \(\d+ bytes\), b\.pdf \(0 bytes\) in file "Resume" \[qx2\] \(the file field of \[qx1\]\)/, both.content);
  assert.deepEqual(field.files.map((file) => file.size), [(big.length / 4) * 3, 0]);
  assert.deepEqual(firsts, [true, false, false]);
  field.multiple = false;
  assert.match((await bg.run('select', { ref: 'qx2', tab: 40 })).error, /give values to choose, or files/);
  sent.length = 0;
  pageContext.chrome.debugger.sendCommand = async (_, method, params) => { sent.push({ method, params }); return {}; };
  assert.match((await bg.run('click', { ref: 'qx2', tab: 40 })).error, /opens a file dialog, which Spindle cannot fill: put workspace files in it with select/);
  assert.ok(!sent.some(({ method }) => method === 'Input.dispatchMouseEvent'), 'no click, no dialog');
  vm.runInContext('for (const timer of attached.values()) clearTimeout(timer);', pageContext);
  delete pageContext.__spindle;
}
// A click catches a file dialog it opens (said, nothing chosen); a page dialog it opens
// is said at once, even while Chrome holds the command that stops the catching.
{
  const commands = [];
  let opens = null;
  pageContext.chrome.tabs.get = async (tab) => ({ id: tab, url: 'https://jobs.example/apply', status: 'complete', title: 'Apply' });
  pageContext.chrome.scripting.executeScript = async () => [{ result: { value: { x: 5, y: 5, what: 'button "Upload resume"', covered: null, field: false, file: false } } }];
  let intercepting = false;
  pageContext.chrome.debugger.sendCommand = ({ tabId }, method, params) => {
    commands.push(`${method}${params?.enabled != null ? ` ${params.enabled}` : params?.type ? ` ${params.type}` : ''}`);
    if (method === 'Page.setInterceptFileChooserDialog') intercepting = params.enabled;
    // Chrome says a file dialog opened only while it catches them (else it is shown).
    if (method === 'Input.dispatchMouseEvent' && params.type === 'mouseReleased' && opens && (opens[0] !== 'Page.fileChooserOpened' || intercepting)) debuggerEvent({ tabId }, ...opens);
    // Held by a page dialog: never answers here.
    if (method === 'Page.setInterceptFileChooserDialog' && params.enabled === false && opens?.[0] === 'Page.javascriptDialogOpening') return new Promise(() => {});
    return Promise.resolve({});
  };
  opens = ['Page.fileChooserOpened', { mode: 'selectSingle' }];
  const caught = await bg.run('click', { ref: 'qx1', tab: 41 });
  assert.match(caught.content, /^Clicked button "Upload resume" \[qx1\]: it opened a file dialog, which Spindle caught \(nothing was chosen\)/);
  assert.deepEqual(commands.filter((name) => /^(Page\.setIntercept|Input\.dispatchMouseEvent mouse(Pressed|Released))/.test(name)),
    ['Page.setInterceptFileChooserDialog true', 'Input.dispatchMouseEvent mousePressed', 'Input.dispatchMouseEvent mouseReleased', 'Page.setInterceptFileChooserDialog false'], 'caught only while the click goes');
  opens = ['Page.javascriptDialogOpening', { type: 'confirm', message: 'Sure?' }];
  const started = Date.now();
  const asked = await bg.run('click', { ref: 'qx1', tab: 42 });
  assert.ok(Date.now() - started < 5_000, `the click answered at once: ${Date.now() - started} ms`);
  assert.match(asked.content, /A confirm dialog is open on it: "Sure\?"/);
  assert.doesNotMatch(asked.content, /file dialog/);
  vm.runInContext('dialogs.clear(); for (const timer of attached.values()) clearTimeout(timer);', pageContext);
}
assert.deepEqual(uncertain('click', 'lost'), { unknown: true, content: 'lost' });
for (const name of ['read_page', 'screenshot', 'wait_for']) assert.deepEqual(uncertain(name, 'lost'), { error: 'lost' }, name);
// The engine's document carries calls to Chrome and Chrome's answers back; an answer that
// never comes back is unknown for an action, a plain failure for a read.
const relays = [];
globalThis.Worker = class { constructor() { this.posted = []; relays.push(this); } postMessage(message) { this.posted.push(message); } };
let chromeReply = async () => undefined;
const toBackground = [];
globalThis.chrome = { runtime: { sendMessage: async (message) => { toBackground.push(message); return chromeReply(message); } } };
await import('../engine.mjs');
const [relay] = relays;
const relayed = async (name, reply) => {
  chromeReply = reply;
  await relay.onmessage({ data: { type: 'page_tool', call: `relay-${name}`, name, input: {} } });
  return relay.posted.at(-1);
};
assert.deepEqual(await relayed('click', async () => ({ content: 'Clicked' })), { type: 'page_tool_answer', call: 'relay-click', answer: { content: 'Clicked' } });
assert.deepEqual((await relayed('click', async () => { throw new Error('The message port closed'); })).answer, { unknown: true, content: "Chrome's answer to the page tool was lost: The message port closed" });
assert.deepEqual((await relayed('read_page', async () => undefined)).answer, { error: 'Chrome gave no answer' });
// What the user allowed goes on to Chrome's side with the call.
await relay.onmessage({ data: { type: 'page_tool', call: 'relay-press', name: 'press', input: { key: 'Enter' }, allowed: { tab: 4, document: 'doc-1' } } });
assert.deepEqual(toBackground.at(-1).allowed, { tab: 4, document: 'doc-1' });
await relay.onmessage({ data: { type: 'page_tool_cancel', call: 'relay-click' } });
assert.deepEqual(toBackground.at(-1), { type: 'page_tool_cancel', call: 'relay-click' });
delete globalThis.Worker;
delete globalThis.chrome;
// A message with files sent while a turn runs waits for the turn to end, then goes as
// the next turn (after what the turn left unread); one the thread cannot take then is
// held, saying why, for the user to take back into the box.
{
  const thread = 'abcdefabcdefabcd';
  records.set(`threads:${thread}`, { id: thread, runner: 'none', state: 'idle', summaries: true });
  const prompts = [];
  const turns = [deferred(), deferred(), deferred(), deferred()];
  let activity = 'idle';
  let accepts = true;
  let resting = false;
  engine.setClient({
    async prompt(dir, text, options) { prompts.push({ text, options }); return turns[prompts.length - 1].promise; },
    async steer() { return { queued: 1 }; },
    async status() { return { activity, accepts_prompt: accepts, accepts_message: accepts || resting, stop_text: 'stopped', wake: { why: 'an unknown outcome' } }; },
    async settings() { return {}; },
    tools: { definitions: () => [] },
    async closeSession() {},
  });
  vm.runInContext('poolOpening = Promise.resolve({ free: Infinity, rmSync() {} });', context);
  const settle = async () => { for (let i = 0; i < 20; i++) await tick(); };
  const announced = () => messages.filter((message) => message.type === 'queued' && message.thread === thread).at(-1)?.queued;
  const shot = { path: 'shot.png', bytes: new Uint8Array([137, 80]) };
  await engine.operate('send', { id: thread, text: 'first' });
  await settle();
  assert.equal(prompts.length, 1);
  assert.equal((await engine.operate('send', { id: thread, text: 'just text' })).steered, 1, 'text alone steers the running turn');
  assert.equal((await engine.operate('send', { id: thread, text: 'look at this', files: [shot] })).queued, true);
  assert.equal(announced().text, 'look at this');
  assert.equal(announced().names[0], 'shot.png');
  assert.equal(announced().held, null);
  const kept = () => [...records.keys()].filter((key) => key.startsWith(`kv:queued:${thread}:`));
  assert.equal(kept().length, 1, 'a waiting message is on disk before the panel is told it waits');
  assert.equal(records.get(kept()[0]).text, 'look at this');
  await assert.rejects(engine.operate('send', { id: thread, text: 'more', files: [shot] }), /already waits/);
  turns[0].resolve({ stopped: { complete: true }, unsent: ['unread steer'] });
  await settle();
  assert.equal(prompts.length, 2, 'the waiting message went as the next turn');
  assert.equal(prompts[1].text, 'unread steer\n\nlook at this\n\n[Attached, in the workspace: shot.png (2 bytes)]');
  assert.equal(prompts[1].options.files[0].path, 'shot.png');
  assert.equal(announced(), null);
  const ended = messages.filter((message) => message.type === 'turn_end' && message.thread === thread).at(-1);
  assert.equal(ended.unsent.length, 0, 'what the turn did not read went with it, not back to the box');
  assert.equal(kept().length, 1, 'kept on disk while the turn that took it runs');
  // The thread needs the user when the turn ends: nothing is sent, and it is taken back.
  assert.equal((await engine.operate('send', { id: thread, text: 'again', files: [shot] })).queued, true);
  activity = 'blocked';
  turns[1].resolve({ stopped: { complete: true }, unsent: ['also unread'] });
  await settle();
  assert.equal(kept().length, 1, 'once that turn ended, only the message held now is on disk');
  assert.equal(records.get(kept()[0]).text, 'also unread\n\nagain');
  assert.equal(prompts.length, 2, 'nothing sent to a thread that needs the user first');
  assert.equal(announced().held, 'an unknown outcome');
  const back = await engine.operate('unqueue', { id: thread });
  assert.match(back.text, /^also unread\n\nagain$/);
  assert.equal(back.files[0].path, 'shot.png');
  assert.equal(announced(), null);
  assert.equal(await engine.operate('unqueue', { id: thread }), null);
  assert.equal(kept().length, 0, 'taken back, it is gone from the disk too');
  // Nor to one whose last turn did not finish (stopped by the user, say).
  activity = 'idle';
  await engine.operate('send', { id: thread, text: 'third' });
  await settle();
  assert.equal((await engine.operate('send', { id: thread, text: 'after the stop', files: [shot] })).queued, true);
  accepts = false;
  turns[2].resolve({ stopped: { complete: false, reason: 'stopped' } });
  await settle();
  assert.equal(prompts.length, 3, 'nothing sent after a turn that did not finish');
  assert.match(announced().held, /^the last turn did not finish \(stopped\): retry, continue or take it back first$/);
  // A message sent to it then is refused at once (the panel keeps the draft), rather than
  // a turn that fails with it.
  await assert.rejects(engine.operate('send', { id: thread, text: 'keep going' }), /did not finish \(stopped\)/);
  assert.equal(prompts.length, 3);
  // A turn stopped part way (the stop recorded the step it cut off) takes a reply that
  // goes on with it: text alone. Files sent then are refused at once, the draft kept; a
  // message with files that waited for such a turn is held, saying so.
  await engine.operate('unqueue', { id: thread });
  resting = true;
  await assert.rejects(engine.operate('send', { id: thread, text: 'with a file', files: [shot] }), /takes a message without files/);
  assert.equal(prompts.length, 3, 'nothing sent with the files');
  await engine.operate('send', { id: thread, text: 'skip that and go on' });
  await settle();
  assert.equal(prompts.length, 4, 'the reply goes on with the stopped turn');
  assert.equal(prompts[3].text, 'skip that and go on');
  assert.equal((await engine.operate('send', { id: thread, text: 'and this', files: [shot] })).queued, true);
  turns[3].resolve({ stopped: { complete: false, reason: 'stopped' } });
  await settle();
  assert.equal(prompts.length, 4);
  assert.match(announced().held, /stopped part way and takes a message without files/);
}

// Stopped before it began (the message not yet handed over): the thread is as it was,
// and the message comes back, held, on disk, to take back into the box.
{
  const thread = 'abababababababab';
  records.set(`threads:${thread}`, { id: thread, runner: 'none', state: 'idle', note: null, summaries: true });
  const gate = deferred();
  const prompts = [];
  engine.setClient({
    async prompt(dir, text) { prompts.push(text); return { stopped: { complete: true } }; },
    async status() { return { accepts_prompt: true, accepts_message: true }; },
    async settings() { await gate.promise; return {}; },
    async stop() {},
    tools: { definitions: () => [] },
    async closeSession() {},
  });
  await engine.operate('send', { id: thread, text: 'hello there' });
  await tick();
  await engine.operate('stop', { id: thread });
  gate.resolve();
  for (let i = 0; i < 20; i++) await tick();
  assert.equal(prompts.length, 0, 'nothing was sent');
  assert.deepEqual([records.get(`threads:${thread}`).state, records.get(`threads:${thread}`).note], ['idle', null], 'the thread is as it was');
  const back = messages.filter((message) => message.type === 'queued' && message.thread === thread).at(-1)?.queued;
  assert.equal(back.text, 'hello there');
  assert.match(back.held, /stopped its turn before it began/);
  const onDisk = () => [...records.keys()].filter((key) => key.startsWith(`kv:queued:${thread}:`));
  assert.equal(onDisk().length, 1);
  assert.equal((await engine.operate('unqueue', { id: thread })).text, 'hello there');
  assert.equal(onDisk().length, 0);
}

// At start, waiting messages a stopped engine left on disk: one its turn had already
// taken goes; another comes back held; one of a thread that is gone goes.
{
  const taken = 'cdcdcdcdcdcdcdcd';
  const waiting = 'efefefefefefefef';
  records.set(`threads:${taken}`, { id: taken, runner: 'none', state: 'interrupted' });
  records.set(`threads:${waiting}`, { id: waiting, runner: 'none', state: 'interrupted' });
  const shot = { path: 'a.png', bytes: new Uint8Array([1, 2, 3]) };
  records.set(`kv:queued:${taken}:1`, { thread: taken, text: 'went', files: [shot], held: null, at: 1 });
  records.set(`kv:queued:${waiting}:2`, { thread: waiting, text: 'stayed', files: [shot], held: null, at: 2 });
  records.set('kv:queued:0000000000000000:3', { thread: '0000000000000000', text: 'orphan', files: [], held: null, at: 3 });
  engine.setClient({
    async history(dir) {
      const said = dir.endsWith(taken) ? 'went\n\n[Attached, in the workspace: a.png (3 bytes)]' : 'something else';
      return { messages: [{ role: 'user', content: [{ type: 'text', text: said }] }, { role: 'assistant', content: [] }] };
    },
    async closeSession() {},
  });
  await engine.restoreQueued();
  assert.equal(records.has(`kv:queued:${taken}:1`), false, 'a message its turn had taken is not offered again');
  assert.equal(records.has('kv:queued:0000000000000000:3'), false, 'one of a thread that is gone goes');
  assert.equal(records.get(`kv:queued:${waiting}:2`).text, 'stayed');
  assert.match(records.get(`kv:queued:${waiting}:2`).held, /stopped before the turn it waited for ended/);
  assert.equal(engine.queued.get(waiting).files[0].path, 'a.png');
  assert.equal(engine.queued.has(taken), false);
  await engine.operate('unqueue', { id: waiting });
}

// Retry after a step the user stopped: asked again from before that step's request,
// unless the thread's first request made it (nothing before it): then the turn starts
// over (taken back, its message sent again).
{
  const thread = 'f1f1f1f1f1f1f1f1';
  records.set(`threads:${thread}`, { id: thread, runner: 'none', state: 'stopped', summaries: true });
  const call = (name) => ({ role: 'assistant', content: [{ type: 'tool_use', name }] });
  const result = { role: 'user', content: [{ type: 'tool_result' }] };
  let status = { accepts_prompt: false, accepts_message: true, phase: 'after_tools' };
  let targets = [{ revision: 4, kind: 'before_model' }];
  let messages = [{ role: 'user', content: [{ type: 'text', text: 'run it' }] }, call('bash'), result];
  // The whole history is asked (`turn`): a completed turn however far back counts.
  let completed = null;
  engine.setClient({ status: async () => status, turn: async () => ({ targets, completed: completed ?? targets.some((target) => target.kind === 'settled') }), history: async () => ({ messages }), closeSession: async () => {} });
  const first = () => vm.runInContext(`firstStepStopped(${JSON.stringify(thread)})`, context);
  assert.equal(await first(), true, "a step of the thread's first request starts the turn over");
  messages = [messages[0], call('write_file'), result, call('bash'), result];
  assert.equal(await first(), false, 'a later step is asked again from before its request');
  messages = [messages[0], call('bash'), result];
  targets = [{ revision: 2, kind: 'settled' }, ...targets];
  assert.equal(await first(), false, 'a turn after another one goes back to its start');
  status = { accepts_prompt: false, accepts_message: true, phase: 'model_wait' };
  assert.equal(await first(), false, 'a cut model request is sent again');
  // A long turn after earlier ones (the turn before ended past the targets listed): its
  // first step is no first request of the thread, so it is never started afresh.
  status = { accepts_prompt: false, accepts_message: true, phase: 'after_tools' };
  targets = Array.from({ length: 60 }, (_, index) => ({ revision: 300 - 3 * index, kind: 'before_model' }));
  completed = true;
  assert.equal(await first(), false, 'a completed turn however far back counts');
}

// A stopped turn never costs the thread what it did: a change goes on from just before
// the stopped request (the turn's work kept), else starts the turn over from the turn
// before; a thread whose earlier turns cannot be gone back to now is left as it is (a
// background task holds them); only a first turn starts afresh, the thread as it was
// kept as an earlier version (never deleted by a reset or a start).
{
  const thread = 'a0a0a0a0a0a0a0a0';
  const did = [];
  let point;
  const reset = () => {
    did.length = 0;
    records.set(`threads:${thread}`, { id: thread, runner: 'none', state: 'failed', title: 'Macro', summaries: true });
  };
  const mock = {
    tools: { definitions: () => [] },
    async status() { return { accepts_prompt: false, accepts_message: false, phase: 'model_wait', head: 30, prompts: 1, provider: 'p', model: 'm2' }; },
    async turn() { return point; },
    async settings(dir, changes) {
      if (changes && dir === `/threads/${thread}`) did.push(['settings', changes.model ?? Object.keys(changes).join()]);
      return { provider: 'p', model: 'm', runner: 'none', builtin_tools: 'all', instructions: '' };
    },
    async rewind(dir, revision) { did.push(['rewind', revision]); },
    async resume() { did.push(['resume']); return { started: true }; },
    async prompt(dir, text) { did.push(['prompt', text]); return { started: true }; },
    async tasks() { return { tasks: [{ task: 'k', pending: true }] }; },
    async create(dir) { if (dir === `/threads/${thread}`) did.push(['create']); },
    async exportSession() { return new Uint8Array([7, 7, 7]); },
    async closeSession() {},
    async files() { return { files: [] }; },
    async read() { throw new Error('gone'); },
  };
  engine.setClient(mock);
  const pool = 'poolOpening = Promise.resolve({ free: 100, rmSync() {}, files: () => [], lost: 0, lostBytes: 0 });';
  vm.runInContext(pool, context);
  const settle = async () => { for (let i = 0; i < 20; i += 1) await tick(); };
  // What the thread was made to do (its own settings changes only, not its setup's).
  const steps = () => did.filter(([what, arg]) => what !== 'settings' || arg === 'm2').map(([what, arg]) => (arg === undefined ? what : `${what} ${arg}`));

  // From just before the stopped request: rewound there, the new model set, continued.
  reset();
  point = { targets: [{ revision: 28, kind: 'before_model' }, { revision: 12, kind: 'settled' }], completed: true, prompt: 'look into it' };
  const on = await engine.operate('restart', { id: thread, changes: { model: 'm2' } });
  await settle();
  assert.equal(on.from, 'request');
  assert.deepEqual(steps(), ['rewind 28', 'settings m2', 'resume'], 'the turn goes on: nothing taken back, nothing sent again, no fresh start');

  // No such point: the turn starts over from the end of the turn before, with its own
  // message (never the summary of earlier ones).
  reset();
  point = { targets: [{ revision: 12, kind: 'settled' }], completed: true, prompt: 'look into it' };
  const over = await engine.operate('restart', { id: thread, changes: { model: 'm2' } });
  await settle();
  assert.equal(over.from, 'turn');
  assert.ok(did.some(([what, arg]) => what === 'rewind' && arg === 12));
  assert.ok(did.some(([what, arg]) => what === 'prompt' && arg === 'look into it'), JSON.stringify(did));
  assert.ok(!did.some(([what]) => what === 'create'), 'never a fresh start for a thread with earlier turns');

  // Earlier turns held by a background task: refused, and nothing changes at all.
  reset();
  point = { targets: [], completed: true, prompt: 'look into it' };
  await assert.rejects(engine.operate('restart', { id: thread, changes: { model: 'm2' } }), /background task of this thread is still at work.*Nothing was changed/);
  assert.deepEqual(did, [], 'no rewind, no fresh store, no settings');

  reset();
  point = { targets: [], completed: null, prompt: 'look into it' };
  await assert.rejects(engine.operate('restart', { id: thread, changes: { model: 'm2' } }), /Nothing was changed/);
  assert.deepEqual(did, [], 'unknown history never permits a fresh start');

  // A real first turn starts afresh; the thread as it was stays as an earlier version.
  reset();
  point = { targets: [], completed: false, prompt: 'first ask' };
  const fresh = await engine.operate('restart', { id: thread, changes: { model: 'm2' } });
  await settle();
  assert.equal(fresh.from, 'start');
  assert.ok(did.some(([what]) => what === 'create'));
  const earlier = [...records.keys()].filter((key) => key.startsWith(`kv:earlier:${thread}:`));
  assert.equal(earlier.length, 1, 'the reset is kept as an earlier version');
  const kept = records.get(earlier[0]);
  assert.ok(records.get(`bundles:${kept.backup}`)?.bytes, 'its whole copy is kept, not deleted');
  assert.equal(kept.size, 3);
  // Kept whatever happens until the thread has gone on past it.
  const { versions } = (await engine.operate('storage', {})).threads.find((entry) => entry.id === thread);
  assert.equal(versions?.length, 1);
  assert.equal(versions[0].free, false);
  await assert.rejects(engine.operate('earlierFree', { id: thread, backup: kept.backup }), /kept until/);
  assert.ok(records.get(`bundles:${kept.backup}`), 'a protected earlier version cannot be freed');
  // A week on and three turns later it may go, and only then.
  records.set(earlier[0], { ...kept, made: Date.now() - 8 * 24 * 3600 * 1000 });
  engine.setClient({ ...mock, async status() { return { prompts: 4, complete: false }; } });
  vm.runInContext(pool, context);
  await assert.rejects(engine.operate('earlierFree', { id: thread, backup: kept.backup }), /kept until/, 'a started fourth turn is not three finished new turns');
  engine.setClient({ ...mock, async status() { return { prompts: 4, complete: true }; } });
  vm.runInContext(pool, context);
  assert.equal((await engine.operate('earlierFree', { id: thread, backup: kept.backup })).freed, 3);
  assert.equal(records.has(`bundles:${kept.backup}`), false);
  records.delete(`threads:${thread}`);
}

// A long page read keeps its start in the conversation and the whole in a workspace file
// (every later request would send it again); without file tools, or short, it stays as
// it was.
{
  let builtin = 'files';
  engine.setClient({ async settings() { return { builtin_tools: builtin }; }, async closeSession() {} });
  const spill = (name, answer) => vm.runInContext(`spilled(${JSON.stringify(name)}, ${JSON.stringify(answer)}, { session: '/threads/abababababababab#task1' })`, context);
  const long = 'x'.repeat(50_000);
  const out = await spill('page_html', { content: long, details: { tab: 3 } });
  const [path] = Object.keys(out.files.write);
  assert.match(path, /^\.pages\/page-html-.*\.html$/);
  assert.equal(out.files.write[path].length, 50_000, 'the whole of it is kept');
  assert.ok(out.content.length < 33_000 && out.content.includes(path), 'the conversation keeps the start and where the rest is');
  assert.equal(out.details.tab, 3);
  assert.equal((await spill('read_page', { content: 'short' })).files, undefined);
  assert.equal((await spill('click', { content: long })).files, undefined, 'only reads');
  // A script's call keeps its whole answer (the package keeps no files a script's call
  // writes: a pointer to one would lead nowhere).
  const scripted = await vm.runInContext(`spilled('page_html', ${JSON.stringify({ content: long })}, { session: '/threads/abababababababab', call: 'code-1a2b3c' })`, context);
  assert.equal(scripted.content.length, 50_000);
  assert.equal(scripted.files, undefined);
  builtin = 'none';
  assert.equal((await spill('page_html', { content: long })).content.length, 50_000, 'no file tools: as it was');
}

// A site's secret header: checked before anything is kept. A name a request can carry
// (Chrome drops some without a word), visible ASCII, a secret part long enough to find
// in what comes back, and only over HTTPS (this computer's own addresses aside).
{
  // Plain copies: objects made in the engine's context have its prototypes.
  const header = (name, value) => JSON.parse(JSON.stringify(engine.secretHeader({ name, value })));
  assert.deepEqual(header(' Authorization ', ' Bearer sk-12345678 '), { name: 'Authorization', value: 'Bearer sk-12345678' });
  assert.deepEqual(header('X-Api-Key', 'k-12345678'), { name: 'X-Api-Key', value: 'k-12345678' });
  for (const [name, value, why] of [
    ['Cookie', 'session=12345678', /set by Chrome or Spindle/],
    ['Sec-Fetch-Site', '12345678abc', /set by Chrome or Spindle/],
    ['Proxy-Authorization', 'Basic 12345678', /set by Chrome or Spindle/],
    ['Bad Name', '12345678abc', /letters, digits and dashes/],
    ['Authorization', 'Bearer short', /too short/],
    ['Authorization', 'Bearer tök-12345678', /text the site gave you/],
    ['Authorization', 'Bearer a\nb-12345678', /text the site gave you/],
  ]) assert.throws(() => header(name, value), why, `${name}: ${value}`);
  const kept = [];
  engine.setClient({ setHttpGrants: (grants) => kept.push(grants) });
  await assert.rejects(engine.changeSites({ url: 'http://api.example.com/v1/', header: { name: 'Authorization', value: 'Bearer sk-12345678' } }),
    /only to an https:\/\/ address[\s\S]*https:\/\/api\.example\.com\/v1\//);
  assert.equal(kept.length, 0, 'nothing reached the package');
  assert.equal(records.get('kv:sites'), undefined, 'nor was kept');
  // Grants name the headers the app adds, never their values; a prefix matches as the
  // package matches it.
  assert.deepEqual(JSON.parse(JSON.stringify(engine.grantsFor([{ name: 'a', url: 'https://a.example/', risk: 'read', secret: ['Authorization'] }, { name: 'b', url: 'https://b.example/', risk: 'read' }]))),
    [{ name: 'a', url: 'https://a.example/', risk: 'read', headers: { Authorization: { app: true } } }, { name: 'b', url: 'https://b.example/', risk: 'read' }]);
  // Names kept for a site's address only: one an earlier build gave another address
  // since (after a rollback) carries none.
  const secrets = { a: { url: 'https://a.example/', names: ['Authorization'] }, b: { url: 'https://b.example/old/', names: ['X-Api-Key'] } };
  assert.deepEqual(JSON.parse(JSON.stringify(engine.withSecrets([{ name: 'a', url: 'https://a.example/' }, { name: 'b', url: 'https://b.example/new/' }], secrets))),
    [{ name: 'a', url: 'https://a.example/', secret: ['Authorization'] }, { name: 'b', url: 'https://b.example/new/' }]);
  assert.ok(engine.underPrefix('https://a.example/v1', 'https://A.example:443/v1/x'));
  assert.ok(!engine.underPrefix('https://a.example/v1', 'https://a.example/v12'));
  assert.ok(!engine.underPrefix('https://a.example/v1/', 'http://a.example/v1/x'));
  assert.ok(!engine.underPrefix('https://a.example/v1/', 'https://a.example.evil/v1/x'));
}

// The theme (store/ui/theme.js, shared with the native UI): Dim is the dark theme; a
// screen that chose the old Dark shows Dim, and System follows the system (Dim when dark).
{
  const themeSource = readFileSync(new URL('../../store/ui/theme.js', import.meta.url), 'utf8');
  const shown = (saved, dark) => {
    const root = { dataset: {} };
    const meta = { content: '' };
    const store = new Map(saved ? [['spindle-theme', saved]] : []);
    const theming = vm.createContext({
      matchMedia: () => ({ matches: dark, addEventListener() {} }),
      localStorage: { getItem: (key) => store.get(key) ?? null, setItem: (key, value) => store.set(key, value) },
      document: { documentElement: root, querySelector: () => meta },
    });
    vm.runInContext(themeSource, theming);
    return { theme: root.dataset.theme, mode: theming.spindleTheme.get(), color: meta.content };
  };
  assert.deepEqual(shown('dark', false), { theme: 'dim', mode: 'dim', color: '#2c2c2a' }, 'the old Dark shows Dim');
  assert.deepEqual(shown('dim', false), { theme: 'dim', mode: 'dim', color: '#2c2c2a' });
  assert.deepEqual(shown('system', true), { theme: 'dim', mode: 'system', color: '#2c2c2a' }, 'a dark system shows Dim');
  assert.deepEqual(shown('system', false), { theme: 'light', mode: 'system', color: '#f0f0f0' });
  assert.deepEqual(shown(null, true), { theme: 'paper', mode: 'paper', color: '#ece6d8' }, 'Paper unless chosen');
}

// Settings, Pages, Before acting: Auto (the default) asks before an action that sends
// or submits; Ask before every action that changes a page; Full asks nothing. A waiting
// action shows to panels, goes on when allowed, and is refused (nothing done) when the
// user says no or the turn ends.
{
  assert.equal(confirmMode(null), 'auto', 'Auto unless chosen');
  assert.equal(confirmMode({ reading: true, acting: true }), 'auto');
  assert.equal(confirmMode({ confirm: 'full' }), 'full');
  assert.equal(confirmMode({ confirm: 'nonsense' }), 'auto');
  // Every record written names its mode: as read, Auto where there was none, Ask where
  // it could not be read.
  assert.deepEqual(pagesRecord({ reading: true, acting: true, confirm: 'full' }, { acting: false }), { reading: true, acting: false, confirm: 'full' });
  assert.deepEqual(pagesRecord(undefined, { reading: true, acting: false }), { confirm: 'auto', reading: true, acting: false });
  assert.deepEqual(pagesRecord(null, { reading: true, acting: true }), { confirm: 'ask', reading: true, acting: true });
  assert.deepEqual(pagesRecord({ reading: true, acting: true, confirm: 'ask' }, { confirm: 'full' }), { reading: true, acting: true, confirm: 'full' });
  assert.equal(sends('press', { key: 'Enter' }), true);
  assert.equal(sends('press', { key: 'Control+Enter' }), true);
  assert.equal(sends('press', { key: 'Return' }), true, 'Return is pressed as Enter');
  assert.equal(sends('press', { key: 'control+return' }), true);
  assert.equal(sends('press', { key: 'a' }), false);
  assert.equal(sends('type', { text: 'hi', submit: true }), true);
  assert.equal(sends('type', { text: 'hi' }), false);
  assert.equal(sends('dialog', { accept: true }), true);
  assert.equal(sends('dialog', { accept: false }), false);
  assert.equal(sends('click', { ref: 'a1' }, { submits: true, label: 'Go' }), true);
  assert.equal(sends('click', { ref: 'a1' }, { submits: false, label: 'Send message' }), true);
  assert.equal(sends('click', { ref: 'a1' }, { submits: false, label: 'Next page' }), false);
  assert.equal(sends('click', { ref: 'a1' }, { submits: false, label: '➤', what: 'button "Send"' }), true, 'an icon button named Send');
  assert.equal(sends('click', { ref: 'a1' }, null), true, 'what cannot be told is asked about');
  assert.equal(sends('navigate', { url: 'https://x.example/' }), false);
  assert.equal(actionText('click', { ref: 'b7' }, { what: 'button "Send"', host: 'mail.example' }), 'Click button "Send" on mail.example');
  assert.equal(actionText('type', { ref: 'f2', text: 'hello', submit: true }, { what: 'field "To"' }), 'Type “hello” into field "To", then press Enter');
  assert.equal(actionText('navigate', { url: 'https://x.example/' }), 'Go to https://x.example/');
  assert.ok(!ASKING_NAMES.has('screenshot') && !ASKING_NAMES.has('scroll') && !ASKING_NAMES.has('network') && !ASKING_NAMES.has('wait_for'));

  const waiting = () => messages.filter((message) => message.type === 'approvals').at(-1)?.approvals ?? [];
  // What an action without a part lands on is looked up before asking (the tab the user
  // looks at then), named in the question and pinned to the action: the user answers
  // about that tab, whatever they look at by the time they answer.
  const looked = [];
  const posting = context.self.postMessage;
  context.self.postMessage = (message) => {
    if (message.type === 'page_tool' && message.name === 'tab_of') {
      looked.push(message.input);
      queueMicrotask(() => context.self.onmessage({ data: { type: 'page_tool_answer', call: message.call, answer: { content: 'tab 4', details: { tab: 4, host: 'mail.example', title: 'Inbox' } } } }));
      return;
    }
    posting(message);
  };
  const settleAsk = async () => { for (let i = 0; i < 8; i++) await tick(); };
  records.set('kv:pages', { reading: true, acting: true });
  assert.equal(await engine.beforeActing('navigate', { url: 'https://x.example/' }, 's'), null, 'Full asks nothing');
  records.set('kv:pages', { reading: true, acting: true, confirm: 'auto' });
  assert.equal(await engine.beforeActing('press', { key: 'a' }, 's'), null, 'Auto lets a plain key go');
  assert.equal(await engine.beforeActing('navigate', { url: 'https://x.example/' }, 's'), null);
  const enter = { key: 'Enter' };
  const asked = engine.beforeActing('press', enter, 's');
  await settleAsk();
  assert.equal(waiting().length, 1);
  assert.equal(waiting()[0].text, 'Press Enter in tab 4 (“Inbox”, mail.example)');
  await engine.operate('approve', { approval: waiting()[0].id, allow: true });
  assert.equal(await asked, null, 'allowed: it goes ahead');
  assert.equal(enter.tab, 4, 'on the tab it asked about');
  assert.equal(waiting().length, 0);
  records.set('kv:pages', { reading: true, acting: true, confirm: 'ask' });
  const denied = engine.beforeActing('navigate', { url: 'https://y.example/' }, 's');
  await settleAsk();
  assert.equal(waiting()[0].text, 'Go to https://y.example/ in tab 4 (“Inbox”, mail.example)');
  await engine.operate('approve', { approval: waiting()[0].id, allow: false });
  assert.match(await denied, /said no[\s\S]*not done/);
  assert.equal(await engine.beforeActing('navigate', { url: 'chrome-extension://id/view.html?thread=t&path=a.html' }, 's'), null,
    'opening a workspace file for the user asks nothing');
  const ended = engine.beforeActing('dialog', { accept: true }, 's');
  await settleAsk();
  assert.equal(waiting().length, 1);
  engine.cancelPageTools(waiting()[0].thread);
  assert.match(await ended, /turn ended[\s\S]*not done/, 'a stopped turn refuses what waits');
  assert.equal(waiting().length, 0);
  // A script's call whose command ends while it waits: the question goes, nothing is done.
  const command = new AbortController();
  const scripted = engine.beforeActing('press', { key: 'Enter' }, 's', command.signal);
  await settleAsk();
  assert.equal(waiting().length, 1);
  command.abort();
  assert.match(await scripted, /command that asked for it ended[\s\S]*not done/);
  assert.equal(waiting().length, 0, 'its question is withdrawn');
  // A choice that cannot be read asks (never acts unasked on what it could not see).
  const reading = context.read;
  context.read = async (store, key) => { if (store === 'kv' && key === 'pages') throw new Error('storage failed'); return reading(store, key); };
  const unread = engine.beforeActing('press', { key: 'a' }, 's');
  await settleAsk();
  assert.equal(waiting().length, 1, 'an unreadable Before acting asks');
  await engine.operate('approve', { approval: waiting()[0].id, allow: false });
  assert.match(await unread, /said no/);
  context.read = reading;
  // Allowed, an action carries what the user was shown (its tab, site and page, or the
  // dialog): Chrome's side acts only while that holds. A dialog's question says what it
  // asks. Auto looks nothing up for an action that sends nothing, and binds nothing.
  {
    const dispatched = [];
    let lookups = 0;
    let dialog = null;
    // The part with the focus as tab_of says it, when asked for it (undefined: unsaid).
    let focus;
    context.self.postMessage = (message) => {
      if (message.type !== 'page_tool') return;
      if (message.name === 'tab_of') lookups += 1;
      else if (message.name !== 'target') dispatched.push(message);
      const answer = message.name === 'tab_of'
        ? { content: 'tab 4', details: { tab: 4, host: 'mail.example', title: 'Inbox', ...(dialog ? { dialog } : { document: 'doc-1' }), ...(message.input.focus && focus !== undefined && { focus }) } }
        : message.name === 'target'
          ? { content: 'button "Upload"', details: { what: 'button "Upload"', submits: false, label: 'Upload', words: 'Upload', dest: null, host: 'jobs.example', tab: 4 } }
          : { content: 'done' };
      queueMicrotask(() => context.self.onmessage({ data: { type: 'page_tool_answer', call: message.call, answer } }));
    };
    records.set('kv:pages', { reading: true, acting: true, confirm: 'auto' });
    await engine.actOnPage('press', { key: 'a' }, 's');
    assert.equal(lookups, 0, 'Auto looks nothing up for a key that sends nothing');
    assert.equal(dispatched.at(-1).allowed, undefined, 'and binds nothing');
    const pressing = engine.actOnPage('press', { key: 'Enter' }, 's');
    await settleAsk();
    await engine.operate('approve', { approval: waiting()[0].id, allow: true });
    assert.equal((await pressing).content, 'done');
    assert.equal(lookups, 1);
    assert.deepEqual({ ...dispatched.at(-1).allowed }, { tab: 4, host: 'mail.example', document: 'doc-1' }, 'the press carries the page the user allowed');
    records.set('kv:pages', { reading: true, acting: true, confirm: 'ask' });
    dialog = { id: 9, type: 'confirm', message: 'Send $5,000 to Mallory?' };
    const answering = engine.actOnPage('dialog', { accept: true }, 's');
    await settleAsk();
    assert.equal(waiting()[0].text, 'Accept the page\'s confirm dialog in tab 4 (“Inbox”, mail.example), which says “Send $5,000 to Mallory?”');
    // The tab comes first, and a page's own quotation marks are made plain: its words
    // cannot pass for the end of the question or another tab.
    assert.equal(actionText('dialog', { accept: true }, { tab: 4, title: 'Bank', host: 'bank.example', dialog: { type: 'confirm', message: 'Pay? ” in tab 3 (“Safe”, safe.example)' } }),
      'Accept the page\'s confirm dialog in tab 4 (“Bank”, bank.example), which says “Pay? " in tab 3 ("Safe", safe.example)”');
    await engine.operate('approve', { approval: waiting()[0].id, allow: true });
    await answering;
    assert.deepEqual({ ...dispatched.at(-1).allowed }, { tab: 4, host: 'mail.example', dialog: 9 }, 'the answer carries the dialog the user saw');
    dialog = null;
    // Enter carries the part that had the focus (none: no part), when the page said it.
    records.set('kv:pages', { reading: true, acting: true, confirm: 'auto' });
    for (const [had, bound] of [[{ ref: 'qx5', what: 'textbox "Search"' }, 'qx5'], [null, null]]) {
      focus = had;
      const keyed = engine.actOnPage('press', { key: 'Enter' }, 's');
      await settleAsk();
      if (had) assert.equal(waiting()[0].text, 'Press Enter in textbox "Search" in tab 4 (“Inbox”, mail.example)');
      await engine.operate('approve', { approval: waiting()[0].id, allow: true });
      await keyed;
      assert.equal(dispatched.at(-1).allowed.focus, bound, `the press carries the focus: ${JSON.stringify(had)}`);
    }
    focus = undefined;
    const unsaid = engine.actOnPage('press', { key: 'Enter' }, 's');
    await settleAsk();
    await engine.operate('approve', { approval: waiting()[0].id, allow: true });
    await unsaid;
    assert.equal('focus' in dispatched.at(-1).allowed, false, 'a focus the page did not say binds nothing');
    // Files put in a field: asked under Auto, naming each file and its size, and held to
    // the part as shown (its words and where it leads).
    const uploading = engine.actOnPage('select', { ref: 'k1', files: ['a.pdf'], upload: [{ path: 'a.pdf', size: 3 }] }, 's');
    await settleAsk();
    assert.equal(waiting()[0].text, 'Upload “a.pdf” (3 bytes) to button "Upload" on jobs.example');
    await engine.operate('approve', { approval: waiting()[0].id, allow: true });
    await uploading;
    assert.deepEqual({ ...dispatched.at(-1).allowed }, { tab: 4, host: 'jobs.example', what: 'button "Upload"', words: 'Upload', dest: null });
    records.set('kv:pages', { reading: true, acting: true, confirm: 'ask' });
    // Said no: nothing is sent.
    const refused = engine.actOnPage('press', { key: 'Enter' }, 's');
    await settleAsk();
    const before = dispatched.length;
    await engine.operate('approve', { approval: waiting()[0].id, allow: false });
    assert.match((await refused).error, /said no/);
    assert.equal(dispatched.length, before);
  }
  // A thread's own turn stopped or ended ends its own page tools, not its background
  // tasks' (the package ends a task's calls itself, through each call's signal, when it
  // stops that task); deleting the thread ends every one.
  {
    const posted = [];
    const answers = new Map();
    context.self.postMessage = (message) => {
      posted.push(message);
      if (message.type !== 'page_tool') return;
      const answer = message.name === 'tab_of' ? { content: 'tab 4', details: { tab: 4, host: 'mail.example', title: 'Inbox' } } : null;
      if (answer) queueMicrotask(() => context.self.onmessage({ data: { type: 'page_tool_answer', call: message.call, answer } }));
      else answers.set(message.name === 'press' ? message.input.key : message.name, message.call);
    };
    const thread = 'fedcba9876543210';
    const dir = `/threads/${thread}`;
    const asking = () => waiting().filter((one) => one.thread === thread);
    records.set('kv:pages', { reading: true, acting: true, confirm: 'ask' });
    // Waiting for the user: the turn's own question and a background task's.
    const own = engine.actOnPage('press', { key: 'Enter' }, dir);
    const tasks = engine.actOnPage('press', { key: 'Enter' }, dir, undefined, true);
    await settleAsk();
    assert.equal(asking().length, 2);
    assert.equal('task' in asking()[0], false, 'panels are not told whose call it is');
    assert.deepEqual([...asking().map((one) => one.text.startsWith('A task asks: '))], [false, true], 'a task\'s question says so');
    engine.running.set(thread, { stopped: true });
    engine.cancelPageTools(thread, { tasks: false });
    assert.match((await own).error, /the turn ended or was stopped/, 'the turn\'s own question goes');
    assert.equal(asking().length, 1, 'the task\'s question stays');
    await engine.operate('approve', { approval: asking()[0].id, allow: true });
    await tick();
    assert.ok(answers.has('Enter'), 'allowed, the task\'s action goes on while the turn is stopped');
    // In flight: the turn's own action is cancelled (unknown), the task's is not.
    const ownPress = engine.pageTool('press', { key: 'a' }, dir);
    const taskPress = engine.pageTool('press', { key: 'b' }, dir, null, null, true);
    assert.equal((await ownPress).error, 'the turn was stopped', 'nothing new starts for the stopped turn');
    engine.running.delete(thread);
    const ownAgain = engine.pageTool('press', { key: 'c' }, dir);
    await tick();
    engine.cancelPageTools(thread, { tasks: false });
    assert.equal((await ownAgain).unknown, true);
    assert.ok(posted.some((message) => message.type === 'page_tool_cancel' && message.call === answers.get('c')));
    assert.equal(posted.some((message) => message.type === 'page_tool_cancel' && message.call === answers.get('b')), false, 'the task\'s action is not cancelled');
    context.self.onmessage({ data: { type: 'page_tool_answer', call: answers.get('b'), answer: { content: 'pressed b' } } });
    assert.equal((await taskPress).content, 'pressed b');
    // The package cancelling one call (its signal ends): that call alone is cancelled.
    const cancelled = new AbortController();
    const signalled = engine.pageTool('press', { key: 'd' }, dir, null, cancelled.signal, true);
    const other = engine.pageTool('press', { key: 'e' }, dir, null, null, true);
    await tick();
    cancelled.abort(Object.assign(new DOMException('settled', 'AbortError'), { outcome: 'stopped' }));
    const ended = await signalled;
    assert.equal(ended.unknown, true);
    assert.match(ended.content, /the turn or task that asked for it was stopped before the page tool finished/);
    assert.ok(posted.some((message) => message.type === 'page_tool_cancel' && message.call === answers.get('d')));
    // Deleting the thread (every call): the task's too.
    engine.cancelPageTools(thread);
    assert.equal((await other).unknown, true, 'every call ends when the thread goes');
    // A task's call is told by its name (as the package names it) or its session.
    assert.deepEqual([{ call: 'tasks/k1/c9', session: dir }, { call: 'c9', session: `${dir}#k1` }, { call: 'c9', session: dir }, undefined].map((call) => engine.ownedByTask(call)), [true, true, false, false]);
    // Which tab a key would land on cannot be told, and none was named: not asked, not done.
    context.self.postMessage = (message) => {
      if (message.type === 'page_tool' && message.name === 'tab_of') queueMicrotask(() => context.self.onmessage({ data: { type: 'page_tool_answer', call: message.call, answer: { error: 'no tab is open' } } }));
    };
    assert.match(await engine.beforeActing('press', { key: 'Enter' }, dir), /did not say which tab this would act on, so it was not done/);
    assert.equal((await tasks).unknown, true, "the task's allowed action, unanswered, ends with the thread");
  }
  context.self.postMessage = posting;
  records.delete('kv:pages');
}

// ---- The engine's own limits and tidying -------------------------------------------
{
  // A thread file larger than this browser takes is refused before the engine reads it.
  await assert.rejects(engine.ops.import({ bytes: { byteLength: 2 ** 31 }, name: 'big.spindle' }), /big\.spindle is too large to bring into this browser \(over 1 GB\)/);
  // Settings, Storage, Unreadable: the pool's lost slots are freed (the bytes said).
  vm.runInContext('poolOpening = Promise.resolve({ reclaim: () => 4096 })', context);
  assert.deepEqual({ ...(await engine.ops.reclaimLost()) }, { freed: 4096 });
  // At start, a message kept for a thread since deleted goes; one for a thread that is
  // there stays for its panel.
  records.set('kv:came-back:fedcba9876543210', { text: 'for a deleted thread', files: [] });
  records.set(`kv:came-back:${id}`, { text: 'for a thread that is there', files: [] });
  records.set(`threads:${id}`, records.get(`threads:${id}`) ?? { id, state: 'idle' });
  await engine.restoreQueued();
  assert.equal(records.has('kv:came-back:fedcba9876543210'), false);
  assert.equal(records.get(`kv:came-back:${id}`).text, 'for a thread that is there');
  records.delete(`kv:came-back:${id}`);
}

// ---- A message that comes back goes to its own thread's box, never another's -------
// The panel's box is every thread's. Take back, a rewind and a failed send put their message
// in it only while that message's thread is open (checked after reopening it); otherwise it
// waits and goes in when that thread is opened again. What the box held stays.
{
  const panelSource = readFileSync(new URL('../panel.mjs', import.meta.url), 'utf8');
  const cut = (from, to) => {
    const start = panelSource.indexOf(from);
    assert.notEqual(start, -1, from);
    return panelSource.slice(start, panelSource.indexOf(to, start));
  };
  const box = { value: '', focus() {} };
  const button = {};
  const toasts = [];
  let answer;
  let opening;
  // The extension's storage, in memory (what a panel keeps there outlives the panel).
  // Transactions on one store run one after another, in the order they were asked for.
  const kept = new Map();
  let transactions = Promise.resolve();
  const inOrder = (work) => (transactions = transactions.then(work));
  const storage = {
    merge: (store, key, value, combine) => inOrder(() => {
      const name = `${store}:${key}`;
      const next = kept.has(name) ? combine(value, kept.get(name)) : value;
      kept.set(name, next);
      return next;
    }),
    take: (store, key) => inOrder(() => {
      const name = `${store}:${key}`;
      const value = kept.get(name);
      kept.delete(name);
      return value;
    }),
    remove: (store, key) => inOrder(() => { kept.delete(`${store}:${key}`); }),
  };
  const handlers = cut('async function rewindTo(', '/** The pictures attached to a message') + cut("$('takeback').onclick = async () => {", '// ---- Settings')
    + cut("$('queued-back').onclick = async () => {", 'function carry(thread)');
  // Take back of a message waiting for the turn has a button of its own.
  const queuedButton = {};
  const newPanel = (own = box) => {
    const made = vm.createContext({
      current: 'A', attached: [], threads: new Map([['A', { title: 'Alpha', state: 'idle' }], ['B', { title: 'Beta', state: 'idle' }]]), File, console,
      $: (name) => (name === 'prompt' ? own : name === 'queued-back' ? queuedButton : button), autosize() {}, renderAttached() {}, toast: (text) => toasts.push(text),
      ask: () => answer.promise, confirmed: async () => true, ...storage,
      openThread: (id) => { made.current = id; made.restoreCameBack(id); return opening?.promise; },
    });
    vm.runInContext(handlers, made);
    return made;
  };
  let panel = newPanel();
  const takeBack = async (during) => {
    answer = deferred();
    opening = deferred();
    const done = button.onclick();
    await tick();
    await during();
    await done;
  };
  // Switched to B while A reopened: nothing of A's in the box; it waits for A.
  await takeBack(async () => {
    answer.resolve({ text: 'private draft A', files: [{ path: 'notes.txt', bytes: new Uint8Array([1]) }] });
    await tick();
    panel.openThread('B');
    opening.resolve();
  });
  assert.equal(box.value, '', 'B\'s box gets nothing of A\'s');
  assert.equal(panel.attached.length, 0);
  assert.match(toasts.at(-1), /“Alpha” was taken back: its message is in the box when you open it/);
  box.value = 'typed for B';
  panel.openThread('A');
  for (let i = 0; i < 6; i++) await tick();
  assert.equal(box.value, 'private draft A\n\ntyped for B', 'back on A it is in the box, before what the box held');
  assert.deepEqual([...panel.attached.map((file) => file.name)], ['notes.txt']);
  // Switched away before the answer came: A is not reopened, and its message waits too.
  box.value = '';
  panel.attached = [];
  await takeBack(async () => {
    panel.openThread('B');
    answer.resolve({ text: 'second draft A', files: [] });
    opening.resolve();
  });
  assert.equal(box.value, '');
  panel.openThread('A');
  for (let i = 0; i < 6; i++) await tick();
  assert.equal(box.value, 'second draft A');
  // Staying on A: in the box at once.
  box.value = '';
  await takeBack(async () => {
    answer.resolve({ text: 'third draft A', files: [] });
    opening.resolve();
  });
  assert.equal(box.value, 'third draft A');
  assert.match(toasts.at(-1), /its message is in the box below/);
  // A rewind: the same. And what waits is kept in storage: a panel closed meanwhile (a new
  // one here) still puts it in that thread's box, once, and only there.
  box.value = '';
  panel.attached = [];
  answer = deferred();
  opening = deferred();
  const rewinding = panel.rewindTo('A', 3, 7);
  await tick();
  panel.openThread('B');
  answer.resolve({ text: 'rewound A', files: [{ path: 'a.txt', bytes: new Uint8Array([2]) }] });
  opening.resolve();
  await rewinding;
  assert.equal(box.value, '', 'B\'s box gets nothing of A\'s rewound message');
  assert.match(toasts.at(-1), /“Alpha” went back to before that message: it is in the box when you open it/);
  for (let i = 0; i < 4; i++) await tick();
  assert.equal(kept.get('kv:came-back:A')?.text, 'rewound A', 'kept in storage');
  panel = newPanel();
  panel.current = 'B';
  panel.openThread('B');
  for (let i = 0; i < 4; i++) await tick();
  assert.equal(box.value, '', 'nothing of A\'s in B after the panel came back');
  panel.openThread('A');
  for (let i = 0; i < 4; i++) await tick();
  assert.equal(box.value, 'rewound A', 'the new panel puts it in A\'s box');
  assert.deepEqual([...panel.attached.map((file) => file.name)], ['a.txt']);
  assert.equal(kept.has('kv:came-back:A'), false, 'and lets it go');
  box.value = '';
  panel.openThread('B');
  panel.openThread('A');
  for (let i = 0; i < 4; i++) await tick();
  assert.equal(box.value, '', 'once');
  // A message waiting for the turn, taken back: the same. Switched to B before the engine
  // answered (it has let its own copy go), it waits for A, never lost, never in B's box.
  box.value = '';
  panel.attached = [];
  panel.openThread('A');
  answer = deferred();
  const unqueueing = queuedButton.onclick();
  await tick();
  panel.openThread('B');
  answer.resolve({ text: 'waiting for A', files: [{ path: 'w.txt', bytes: new Uint8Array([3]) }] });
  await unqueueing;
  assert.equal(box.value, '', "B's box gets nothing of A's waiting message");
  assert.match(toasts.at(-1), /^The waiting message of “Alpha” was taken back: it is in the box when you open it\.$/);
  panel.openThread('A');
  for (let i = 0; i < 4; i++) await tick();
  assert.equal(box.value, 'waiting for A', 'back on A it is in the box');
  assert.deepEqual([...panel.attached.map((file) => file.name)], ['w.txt']);
  // Staying on A: in the box at once.
  box.value = '';
  panel.attached = [];
  answer = deferred();
  const staying = queuedButton.onclick();
  answer.resolve({ text: 'waiting again', files: [] });
  await staying;
  assert.equal(box.value, 'waiting again');
  box.value = '';
  panel.attached = [];
  panel.openThread('B');
  panel.openThread('A');
  for (let i = 0; i < 4; i++) await tick();
  assert.equal(box.value, '', 'and nothing more after');
  // Two waiting for the same thread go in together, a file of the same name once (the newer).
  panel.attached = [];
  panel.openThread('B');
  panel.backToBox('A', 'first', [{ name: 'x.txt', size: 1 }]);
  panel.backToBox('A', 'second', [{ name: 'x.txt', size: 2 }]);
  panel.openThread('A');
  for (let i = 0; i < 6; i++) await tick();
  assert.equal(box.value, 'second\n\nfirst');
  assert.deepEqual([...panel.attached.map((file) => file.size)], [2]);
  // Two windows: what each keeps for one thread adds up (neither overwrites the other), and
  // only the first to open that thread puts it in its box.
  const boxOne = { value: '', focus() {} };
  const boxTwo = { value: '', focus() {} };
  const one = newPanel(boxOne);
  const two = newPanel(boxTwo);
  one.current = 'B';
  two.current = 'C';
  one.backToBox('A', 'from window one', []);
  two.backToBox('A', 'from window two', []);
  for (let i = 0; i < 6; i++) await tick();
  two.openThread('A');
  one.openThread('A');
  for (let i = 0; i < 6; i++) await tick();
  assert.deepEqual([boxOne.value, boxTwo.value], ['', 'from window two\n\nfrom window one'], 'both kept, put in one box once');
  assert.equal(kept.has('kv:came-back:A'), false);
  // A deleted thread's message goes with it.
  box.value = '';
  panel.attached = [];
  panel.openThread('B');
  panel.backToBox('A', 'for a thread about to go', []);
  for (let i = 0; i < 4; i++) await tick();
  assert.ok(kept.has('kv:came-back:A'));
  panel.forgetCameBack('A');
  for (let i = 0; i < 4; i++) await tick();
  assert.equal(kept.has('kv:came-back:A'), false);
  panel.openThread('A');
  for (let i = 0; i < 4; i++) await tick();
  assert.equal(box.value, '');
}

// ---- Hosted effects survive a lost answer or an engine restart ---------------------
{
  const home = { url: 'http://host', session: 's1' };
  const event = { call: 'call-1', name: 'click', input: {}, again: false };
  const entry = () => ({ answered: new Set() });
  const key = `host-tool:${id}:s1:call-1`;
  let actions = 0;
  const answers = [];
  engine.setClient({ tools: {
    byName: new Map([['click', {}]]),
    async answer(call) {
      assert.ok(records.has(`kv:${key}`), 'the reservation is durable before a browser action');
      if (call.again) return { unknown: true };
      actions += 1;
      return { content: 'clicked' };
    },
  } });
  engine.setHost({ async appToolResult(_session, _call, answer) {
    answers.push(answer);
    if (answers.length === 1) throw Object.assign(new Error('reply lost'), { status: 0 });
  } });
  await engine.answerHosted(id, home, entry(), event);
  assert.equal(actions, 1);
  assert.equal(records.get(`kv:${key}`)?.answer?.content, 'clicked');
  await engine.answerHosted(id, home, entry(), event);
  assert.equal(actions, 1, 'a lost answer resends the saved result, not the action');
  assert.equal(answers.length, 2);
  assert.equal(records.get(`kv:${key}`)?.answer?.content, 'clicked');
  await engine.answerHosted(id, home, entry(), event);
  assert.equal(actions, 1, 'a replayed snapshot cannot repeat a completed action');
  assert.equal(answers.length, 3, 'the saved answer remains available until the host journals it');
  records.set(`kv:${key}`, { started: true });
  await engine.answerHosted(id, home, entry(), event);
  assert.equal(actions, 1, 'a crash between action and answer reconciles as unknown');
  assert.equal(answers.at(-1).unknown, true);
  records.set(`threads:${id}`, { id, state: 'running', home });
  engine.setHost({ async session() { return { busy: false, complete: false, head: 3, accepts_prompt: false }; } });
  await engine.endHosted(id, { state: 'completed' });
  assert.ok(records.has(`kv:${key}`), 'a stopped turn keeps its action record for Continue');
  engine.setHost({ async session() { return { busy: false, complete: true, head: 4, accepts_prompt: true }; } });
  await engine.endHosted(id, { state: 'completed' });
  assert.equal(records.has(`kv:${key}`), false, 'only a finished turn retires its browser action records');
  records.delete(`threads:${id}`);
}
// A turn's end closes the tabs its turns opened, and its background tasks' once none of
// them is at work as the rows say: here `pending`; a native host's rows say where each
// task's session stands (finished or stopped is done). Rows that cannot be read keep
// them; a thread none of whose tasks opened a tab is not asked about.
{
  const posting = context.self.postMessage;
  const said = [];
  context.self.postMessage = (message) => { if (message.type === 'tabs_done') said.push(message); };
  const never = async () => { throw new Error('not asked'); };
  await engine.turnTabsDone('t9', never);
  engine.tasksOpening.add('t9');
  await engine.turnTabsDone('t9', async () => ({ tasks: [{ pending: false }, { pending: true }] }));
  await engine.turnTabsDone('t9', async () => ({ tasks: [{ task: 'a', status: { complete: true, stop: null } }, { task: 'b', status: { complete: false, stop: null } }] }));
  await engine.turnTabsDone('t9', async () => ({ tasks: [{ task: 'a', status: null }] }));
  await engine.turnTabsDone('t9', async () => { throw new Error('host away'); });
  await engine.turnTabsDone('t9', async () => ({ tasks: [{ pending: false }, { task: 'a', status: { complete: true, stop: null } }, { task: 'b', status: { complete: false, stop: 'stopped' } }] }));
  await engine.turnTabsDone('t9', never);
  assert.deepEqual(said.map((message) => [message.thread, message.tasks]), [['t9', false], ['t9', false], ['t9', false], ['t9', false], ['t9', false], ['t9', true], ['t9', false]]);
  // A hosted thread's finished turn asks its host for the task rows.
  said.length = 0;
  engine.tasksOpening.add(id);
  records.set(`threads:${id}`, { id, state: 'running', home: { url: 'http://host', session: 's1' } });
  let listed = null;
  engine.setHost({ async session() { return { busy: false, complete: true, head: 4, accepts_prompt: true }; }, async tasks(session) { listed = session; return { tasks: [{ task: 'a', status: { complete: true, stop: null } }] }; } });
  await engine.endHosted(id, { state: 'completed' });
  await new Promise((done) => setTimeout(done, 0));
  assert.equal(listed, 's1');
  assert.deepEqual(said.map((message) => [message.thread, message.tasks]), [[id, true]]);
  // One that left a message unread (held for the user) keeps them, as here.
  said.length = 0;
  await engine.endHosted(id, { state: 'completed', unsent: ['and one more thing'] });
  await new Promise((done) => setTimeout(done, 0));
  assert.ok(engine.queued.get(id)?.held, 'the unread message is held');
  assert.deepEqual(said, [], 'its tabs stay');
  engine.queued.delete(id);
  for (const key of [...records.keys()]) if (key.startsWith(`kv:queued:${id}:`)) records.delete(key);
  records.delete(`threads:${id}`);
  context.self.postMessage = posting;
}
{
  const key = `kv:host-command:${id}`;
  const sent = [];
  records.set(`threads:${id}`, { id, state: 'idle', home: { url: 'http://host', session: 's1' } });
  engine.followed.set(id, { opened: Promise.resolve() });
  engine.setHost({ async session() { return { head: 7, busy: false }; }, async command(kind, fields, options) {
    assert.ok(records.has(key), 'the request is durable before sending');
    sent.push({ kind, fields: JSON.stringify(fields), request: options.requestId });
    if (sent.length === 1) throw Object.assign(new Error('answer lost'), { status: 0 });
    return { state: 'running' };
  } });
  const started = await engine.startHosted(id, 'say', { text: 'one message' });
  assert.equal(started.pending, true);
  assert.equal(records.get(`threads:${id}`).state, 'running');
  const command = structuredClone(records.get(key));
  engine.hostedTurns.clear();
  await engine.admitHosted(id, command);
  assert.equal(sent[0].fields, JSON.stringify({ session: 's1', text: 'one message', expected_head: 7 }), 'a receipt that expires cannot turn a replay into a fresh turn at a different head');
  assert.deepEqual(sent[0], sent[1], 'after a restart the identical saved request ID is reused');
  assert.equal(records.has(key), false, 'the outbox clears only after admission');
  engine.hostedTurns.clear();
  engine.followed.clear();
  records.delete(`threads:${id}`);
}

// Cancellation before/inside async approval and tool reservation cannot dispatch later.
{
  const thread = 'abababababababab';
  const home = { url: 'http://host', session: 's-late' };
  const entry = () => ({ controller: new AbortController(), answered: new Set() });
  const savedWrite = context.writeDurably;
  let actions = 0;
  let answered = 0;
  engine.setHost({ async appToolResult() { answered += 1; }, async command() { return { state: 'completed' }; } });
  engine.setClient({ tools: { byName: new Map([['click', {}]]), async answer() { actions += 1; return { content: 'clicked' }; } } });
  for (const mode of ['stop', 'disconnect']) {
    const held = deferred();
    let reserved = false;
    context.writeDurably = async (store, key, value) => { reserved = true; await held.promise; return savedWrite(store, key, value); };
    const follow = entry();
    const call = { call: `late-${mode}`, name: 'click', input: {} };
    const key = `kv:host-tool:${thread}:s-late:${call.call}`;
    const pending = engine.answerHosted(thread, home, follow, call);
    await tick();
    assert.equal(reserved, true);
    if (mode === 'stop') engine.cancelPageTools(thread);
    else follow.controller.abort();
    held.resolve();
    await pending;
    assert.equal(actions, 0, `${mode} fences an action waiting on its durable reservation`);
    records.delete(key);
  }
  context.writeDurably = savedWrite;
  engine.hostedTurns.set(thread, { stopped: true });
  assert.match((await engine.pageTool('click', {}, `/threads/${thread}`)).error, /stopped/);
  engine.hostedTurns.delete(thread);
  const savedRead = context.read;
  const permission = deferred();
  context.read = (store, key) => key === 'pages' ? permission.promise : savedRead(store, key);
  const before = posted.length;
  const acting = engine.actOnPage('navigate', { url: 'https://example.com/' }, `/threads/${thread}`);
  engine.cancelPageTools(thread);
  permission.resolve({ confirm: 'full' });
  assert.match((await acting).error, /ended or was stopped/);
  assert.equal(posted.length, before, 'no page request begins after cancellation during the settings read');
  context.read = savedRead;

  const slowAnswer = deferred();
  engine.setClient({ tools: { byName: new Map([['click', {}]]), async answer() { actions += 1; await slowAnswer.promise; return { content: 'once' }; } } });
  const call = { call: 'shared-inflight', name: 'click', input: {} };
  const oldEntry = entry();
  const one = engine.answerHosted(thread, home, oldEntry, call);
  await tick();
  const deliveredBefore = answered;
  const two = engine.answerHosted(thread, home, entry(), call);
  await tick();
  oldEntry.controller.abort();
  slowAnswer.resolve();
  await Promise.all([one, two]);
  assert.equal(actions, 1, 'reconnecting streams share an in-flight action, not a second execution');
  assert.equal(answered - deliveredBefore, 1, 'the replacement stream delivers the saved result after the old follower aborts');
  records.delete(`kv:host-tool:${thread}:s-late:shared-inflight`);
  assert.ok(answered >= 1);
}

// Expired receipts and transient end-state failures cannot strand a finished turn.
{
  const thread = 'bcbcbcbcbcbcbcbc';
  records.set(`threads:${thread}`, { id: thread, state: 'running', home: { url: 'http://host', session: 's-old' } });
  const entry = { request: 'expired', stopped: true };
  engine.hostedTurns.set(thread, entry);
  engine.setHost({ async session() { throw new Error('offline'); } });
  await engine.endHosted(thread, { state: 'failed' });
  assert.equal(engine.hostedTurns.get(thread), entry, 'a failed status refresh retains command identity and Stop');
  engine.setHost({
    async sessions() { return [{ id: 's-old', status: { state: 'done' } }]; },
    async receipt() { throw Object.assign(new Error('expired'), { status: 404 }); },
    async session() { return { head: 12, complete: true, accepts_prompt: true, busy: false }; },
  });
  await engine.pollHosted();
  assert.notEqual(records.get(`threads:${thread}`).state, 'running', 'an expired receipt settles from the current idle session, never resends');
  assert.equal(engine.hostedTurns.has(thread), false);
  records.delete(`threads:${thread}`);
}

// A stopped attachment upload cannot later start a turn.
{
  const thread = 'cdcdcdcdcdcdcdcd';
  records.set(`threads:${thread}`, { id: thread, state: 'idle', home: { url: 'http://host', session: 's-upload' } });
  const upload = deferred();
  let says = 0;
  engine.setHost({ upload: () => upload.promise, async session() { return {}; }, async command(kind) { if (kind === 'say') says += 1; return { state: 'completed' }; } });
  const sending = engine.operate('send', { id: thread, text: 'cancel this', files: [{ path: 'x', bytes: new Uint8Array([1]) }] });
  const rejected = assert.rejects(sending, /stopped before it started/);
  await tick();
  await engine.operate('stop', { id: thread });
  upload.resolve({ digest: 'abc' });
  await rejected;
  assert.equal(says, 0);
  records.delete(`threads:${thread}`);
}

// Hosted take-back/reset use the head read before inspecting history.
{
  const thread = 'dededededededede';
  const first = { role: 'user', content: [{ type: 'text', text: 'first' }] };
  const second = { role: 'user', content: [{ type: 'text', text: 'second' }] };
  records.set(`threads:${thread}`, { id: thread, state: 'stopped', home: { url: 'http://host', session: 's-rewind' } });
  const rewinds = [];
  engine.setHost({
    async session() { return { head: 18, busy: false, accepts_prompt: false }; },
    async messages(_id, revision) { return { messages: revision === 6 ? [first] : [first, second] }; },
    async targets() { return [{ revision: 6, kind: 'settled' }]; },
    // An older host: the newest steps only, and nothing of the turn's message.
    async turn() { return { targets: [{ revision: 6, kind: 'settled' }] }; },
    async command(kind, fields) { rewinds.push(fields); throw Object.assign(new Error('head_moved'), { status: 409, kind: 'head_moved' }); },
  });
  await assert.rejects(engine.operate('rewindTo', { id: thread, at: 1 }), /head_moved/);
  await assert.rejects(engine.operate('restart', { id: thread, resend: false }), /head_moved/);
  assert.ok(rewinds.every((fields) => fields.expected_head === 18), 'a newer turn elsewhere is never silently rewound');
  records.delete(`threads:${thread}`);
}

// A failed file read never silently removes attachments before a destructive rewind.
{
  const thread = 'efefefefefefefef';
  const first = { role: 'user', content: [{ type: 'text', text: 'first' }] };
  const second = { role: 'user', content: [{ type: 'text', text: 'second\n\n[Attached, in the workspace: report.txt (7 bytes)]' }] };
  records.set(`threads:${thread}`, { id: thread, state: 'stopped', home: { url: 'http://host', session: 's-files' } });
  let rewinds = 0;
  engine.setHost({
    async session() { return { head: 18, accepts_prompt: false }; },
    async messages(_id, revision) { return { messages: revision === 6 ? [first] : [first, second] }; },
    async targets() { return [{ revision: 6, kind: 'settled' }]; },
    async turn() { return { targets: [{ revision: 6, kind: 'settled' }], completed: true, prompt: 'second\n\n[Attached, in the workspace: report.txt (7 bytes)]' }; },
    async read() { throw Object.assign(new Error('offline file read'), { status: 0, kind: 'unreachable' }); },
    async command() { rewinds += 1; return { state: 'completed' }; },
  });
  await assert.rejects(engine.operate('rewindTo', { id: thread, at: 1 }), /offline file read/);
  await assert.rejects(engine.operate('restart', { id: thread, resend: false }), /offline file read/);
  assert.equal(rewinds, 0, 'history and workspace stay intact when the draft cannot be recovered');
  records.delete(`threads:${thread}`);
}

// An old stream cannot act on a thread after that stream was replaced.
{
  const thread = 'acacacacacacacac';
  const entry = { controller: new AbortController(), answered: new Set() };
  entry.controller.abort();
  const before = messages.length;
  assert.equal(await engine.hostedFrame(thread, { url: 'http://host', session: 's' }, entry, { kind: 'event', data: { type: 'prompt' } }), false);
  assert.equal(messages.length, before);
}

// A poll and stream reporting the same completion settle once and cancel no live work.
{
  const thread = 'bdbdbdbdbdbdbdbd';
  records.set(`threads:${thread}`, { id: thread, state: 'running', home: { url: 'http://host', session: 's-end' } });
  const mine = { request: 'r-end' };
  engine.hostedTurns.set(thread, mine);
  const status = deferred();
  let reads = 0;
  engine.setHost({ async session() { reads += 1; return status.promise; } });
  const before = messages.filter((one) => one.type === 'turn_end' && one.thread === thread).length;
  const first = engine.endHosted(thread, { request_id: 'r-end', state: 'completed' });
  const second = engine.endHosted(thread, { request_id: 'r-end', state: 'completed' });
  await tick();
  assert.equal(engine.hostedTurns.get(thread), mine, 'completion checks do not expose an admission gap');
  assert.equal(reads, 1, 'concurrent finishers share status reconciliation');
  status.resolve({ busy: false, complete: true, head: 19 });
  await Promise.all([first, second]);
  const after = messages.filter((one) => one.type === 'turn_end' && one.thread === thread).length;
  assert.equal(after - before, 1);
  records.delete(`threads:${thread}`);
}

// Stop during hosted preflight cancels before admission, not a later live command.
{
  const thread = 'eeeeeeeeeeeeeeee';
  records.set(`threads:${thread}`, { id: thread, state: 'idle', home: { url: 'http://host', session: 's-stop' } });
  const preflight = deferred();
  let sends = 0;
  engine.followed.set(thread, { opened: Promise.resolve() });
  engine.setHost({ session: () => preflight.promise, async command() { sends += 1; return { state: 'completed' }; } });
  const starting = engine.operate('send', { id: thread, text: 'do not send' });
  const rejected = assert.rejects(starting, /stopped before it started/);
  await tick();
  await engine.operate('stop', { id: thread });
  preflight.resolve({ busy: false, head: 7 });
  await rejected;
  assert.equal(sends, 0, 'neither a turn nor a premature Stop is dispatched during preflight');
  assert.equal(records.has(`kv:host-command:${thread}`), false);
  assert.equal(engine.hostedTurns.has(thread), false);
  engine.followed.delete(thread);
  records.delete(`threads:${thread}`);
}

// Stop during an initial async admission check cannot slip through before entry creation.
{
  const thread = 'dbdbdbdbdbdbdbdb';
  records.set(`threads:${thread}`, { id: thread, state: 'idle', home: { url: 'http://host', session: 's-start' } });
  const read = context.read;
  const gate = deferred();
  let held = false;
  context.read = async (store, key) => {
    if (key === `host-stop:${thread}` && !held) { held = true; await gate.promise; }
    return read(store, key);
  };
  engine.setHost({ async session() { return {}; } });
  engine.followed.set(thread, { opened: Promise.resolve() });
  const starting = engine.startHosted(thread, 'say', { text: 'cancel before preflight' });
  const rejected = assert.rejects(starting, /stopped before it started/);
  await tick();
  engine.cancelPageTools(thread);
  gate.resolve();
  await rejected;
  context.read = read;
  engine.followed.delete(thread);
  records.delete(`threads:${thread}`);
}

// Stop during admission updates the same entry the sender will later reconcile.
{
  const thread = 'cacacacacacacaca';
  records.set(`threads:${thread}`, { id: thread, state: 'idle', home: { url: 'http://host', session: 's-admit' } });
  engine.followed.set(thread, { opened: Promise.resolve() });
  const admitted = deferred();
  let asks = 0;
  engine.setHost({
    async info() { return { features: ['targeted_stop'] }; },
    async session() { return { head: 7, busy: false, complete: false, accepts_prompt: false }; },
    async command(kind) {
      if (kind === 'say') { asks += 1; return admitted.promise; }
      return { state: 'completed' };
    },
  });
  const starting = engine.startHosted(thread, 'say', { text: 'once' });
  await tick();
  const stopping = engine.operate('stop', { id: thread });
  await tick();
  admitted.resolve({ state: 'failed', error: 'stopped' });
  await Promise.all([starting, stopping]);
  assert.equal(asks, 1);
  assert.equal(records.get(`threads:${thread}`).state, 'stopped', 'admission cannot lose a concurrent Stop flag');
  engine.followed.delete(thread);
  records.delete(`threads:${thread}`);
}

// Offline Stop survives a restart and targets only its original admitted command.
{
  const thread = 'dadadadadadadada';
  const home = { url: 'http://host', session: 's-offline' };
  records.set(`threads:${thread}`, { id: thread, state: 'running', home });
  const command = { request: 'original-message', kind: 'say', fields: { session: home.session, text: 'once', expected_head: 4 } };
  records.set(`kv:host-command:${thread}`, command);
  engine.hostedTurns.set(thread, { request: command.request, kind: 'say' });
  engine.setHost({ async command() { throw Object.assign(new Error('offline'), { status: 0 }); } });
  assert.equal((await engine.operate('stop', { id: thread })).pending, true);
  const kept = structuredClone(records.get(`kv:host-stop:${thread}`));
  assert.equal(kept.target, command.request);
  engine.hostedTurns.clear();
  const stops = [];
  let sends = 0;
  engine.setHost({
    async info() { return { features: ['targeted_stop'] }; },
    async command(kind, fields, options) {
      if (kind === 'say') { sends += 1; return { state: 'running' }; }
      stops.push({ fields, request: options.requestId });
      if (stops.length === 1) throw Object.assign(new Error('lost Stop response'), { status: 0 });
      throw Object.assign(new Error('another turn runs now'), { status: 409, kind: 'not_running' });
    },
  });
  await engine.admitHosted(thread, command);
  assert.equal(records.has(`kv:host-command:${thread}`), false);
  assert.equal(records.get(`kv:host-stop:${thread}`).request, kept.request);
  engine.hostedTurns.clear();
  await engine.sendHostedStop(thread);
  assert.equal(sends, 1, 'resuming a Stop never resends an admitted message');
  assert.equal(stops.length, 2);
  assert.deepEqual(stops[0], stops[1], 'lost Stop answers reuse the exact command ID and target');
  assert.equal(stops[0].fields.target_request, command.request);
  assert.equal(records.has(`kv:host-stop:${thread}`), false, 'a finished or replaced target retires only its old Stop');
  records.set(`kv:host-stop:${thread}`, kept);
  let unsafeStops = 0;
  engine.setHost({ async info() { return { features: [] }; }, async command() { unsafeStops += 1; } });
  await assert.rejects(engine.sendHostedStop(thread), /update your Spindle host/);
  assert.equal(unsafeStops, 0, 'an old host never receives an unbound Stop fallback');
  assert.ok(records.has(`kv:host-stop:${thread}`));
  let actions = 0;
  engine.setClient({ tools: { byName: new Map([['click', {}]]), async answer() { actions += 1; return { content: 'clicked' }; } } });
  engine.setHost({ async appToolResult() {} });
  await engine.answerHosted(thread, home, { answered: new Set(), controller: new AbortController() }, { call: 'blocked-by-saved-stop', name: 'click' });
  assert.equal(actions, 0, 'saved Stop fences actions even after in-memory turn state was lost');
  records.delete(`kv:host-stop:${thread}`);
  records.delete(`threads:${thread}`);
}

// Queued attachments and unread steering stay together after a hosted Stop.
{
  const thread = 'cccccccccccccccc';
  records.set(`threads:${thread}`, { id: thread, state: 'running', home: { url: 'http://host', session: 's-queue' } });
  let uploaded = 0;
  engine.setHost({
    async session() { return { busy: false, head: 8, complete: false, accepts_prompt: false, accepts_message: true }; },
    async upload() { uploaded += 1; throw new Error('attachments should be held'); },
  });
  await engine.operate('send', { id: thread, text: 'with an image', files: [{ path: 'image.png', bytes: new Uint8Array([1]) }] });
  await engine.endHosted(thread, { state: 'completed', unsent: ['unread instruction'] });
  for (let i = 0; i < 10; i++) await tick();
  const queue = messages.filter((message) => message.type === 'queued' && message.thread === thread).at(-1)?.queued;
  assert.match(queue.held, /takes a message without files/);
  const back = await engine.operate('unqueue', { id: thread });
  assert.equal(back.text, 'unread instruction\n\nwith an image');
  assert.equal(back.files[0].path, 'image.png');
  assert.equal(uploaded, 0);
  records.delete(`threads:${thread}`);
}

// Failed receipts never discard the local reference or pretend settings were saved.
{
  const thread = 'dddddddddddddddd';
  const home = { url: 'http://host', session: 's-delete' };
  records.set(`threads:${thread}`, { id: thread, state: 'idle', home });
  engine.setClient({ async closeSession() { throw new Error('local copy was touched'); } });
  engine.setHost({ async command() { return { state: 'running', request_id: 'delete-1' }; }, async settled() { return { state: 'failed', error: 'disk full' }; } });
  await assert.rejects(engine.operate('remove', { id: thread }), /disk full/);
  assert.ok(records.get(`threads:${thread}`)?.home, 'a failed deletion keeps the hosted reference');
  await assert.rejects(engine.operate('settings', { id: thread, changes: { model: 'new-model' } }), /disk full/);
  records.set(`threads:${thread}`, { id: thread, state: 'idle', home: null, moving: { way: 'back', step: 'release' } });
  await assert.rejects(engine.operate('toHost', { id: thread }), /previous move is still finishing/);
  records.set(`kv:${MOVE(thread)}`, { id: thread, way: 'back', step: 'release', url: home.url, session: home.session });
  await assert.rejects(engine.operate('remove', { id: thread }), /disk full/);
  assert.ok(records.has(`threads:${thread}`), 'failed release cleanup keeps the local thread too');
  assert.ok(records.has(`kv:${MOVE(thread)}`), 'the pending release is not forgotten');
  records.delete(`threads:${thread}`);
  records.delete(`kv:${MOVE(thread)}`);
}

// A thread deleted here takes a message kept for it (`came-back:ID`) with it.
{
  const thread = 'eeeeeeeeeeeeeeee';
  records.set(`threads:${thread}`, { id: thread, state: 'idle', home: null });
  records.set(`kv:came-back:${thread}`, { text: 'waiting for it', files: [] });
  records.set('kv:came-back:start', { text: 'for the start page', files: [] });
  engine.setClient({ async closeSession() {}, async tasks() { return { tasks: [] }; } });
  assert.deepEqual({ ...(await engine.operate('remove', { id: thread })) }, { removed: true });
  assert.equal(records.has(`kv:came-back:${thread}`), false);
  assert.equal(records.has('kv:came-back:start'), true, 'not another\'s');
  records.delete('kv:came-back:start');
}
// A completed turn after which its thread rests closes the tabs it opened. One stopped
// or failed keeps them (to go on in), and so does one a waiting message goes on from at
// once, or one that left a message unread (held for the user). Deleting the thread
// closes all of them, its tasks' too.
{
  const posting = context.self.postMessage;
  const said = [];
  context.self.postMessage = (message) => { if (message.type === 'tabs_done') said.push([message.thread, message.tasks]); };
  const resting = 'dddddddddddddddd';
  records.set(`threads:${resting}`, { id: resting, state: 'idle', home: null });
  await engine.afterTurn(resting, [], null, { sending: true, failed: false, completed: true });
  await engine.afterTurn(resting, [], null, { sending: true, failed: true });
  await engine.afterTurn(resting, ['and one more thing'], null, { sending: true, failed: false, completed: true });
  assert.ok(engine.queued.get(resting)?.held, 'the unread message is held for the user');
  // Sent on: it goes at once after the next turn (here its start fails, holding it again).
  engine.queued.get(resting).held = null;
  await engine.afterTurn(resting, [], null, { sending: true, failed: false, completed: true });
  await new Promise((done) => setTimeout(done, 0));
  assert.deepEqual(said, [[resting, false]], 'only the turn after which it rests');
  said.length = 0;
  assert.deepEqual({ ...(await engine.operate('remove', { id: resting })) }, { removed: true });
  assert.deepEqual(said, [[resting, true]], 'deleted: all of its tabs');
  engine.queued.delete(resting);
  context.self.postMessage = posting;
}

// ---- Threads on the user's host: the /v1 client, connect's PKCE, and the moves ----

// (Sessions the tests above opened close on the engine's timers meanwhile.)
engine.setClient({ async closeSession() {} });

// The event stream as the HTML standard reads it: any line end, split anywhere,
// comments and heartbeats dropped, data lines joined, the id kept.
{
  const parser = new EventParser();
  const frames = [
    ...parser.push('﻿id: b:1\r\nevent: snapshot\r\ndata: {"busy":'),
    ...parser.push('true}\r'),
    ...parser.push('\n\r\n: heartbeat\n\nid: b:2\nevent: event\ndata: {"type":"model_delta",\ndata: "text":"hi"}\n\n'),
    ...parser.push('event: deleted\rdata: {}\r'),
    ...parser.push('\r'),
  ];
  assert.deepEqual(frames.map((frame) => [frame.id, frame.kind]), [['b:1', 'snapshot'], ['b:2', 'event'], ['b:2', 'deleted']]);
  assert.deepEqual(frames[0].data, { busy: true });
  assert.deepEqual(frames[1].data, { type: 'model_delta', text: 'hi' }, 'data lines joined with a line feed');
  assert.deepEqual(new EventParser().push('data: not json\n\n')[0].data, 'not json');
}

/** A body that sends `chunks` (strings) and then ends, or fails (`fail`). */
function streamOf(chunks, fail = false) {
  const left = [...chunks];
  return new ReadableStream({
    // One chunk a read, then the end (or the failure, once the chunks were read).
    pull(controller) {
      if (left.length) controller.enqueue(new TextEncoder().encode(left.shift()));
      else if (fail) controller.error(new TypeError('network reset'));
      else controller.close();
    },
  });
}
const sse = (status, chunks, fail) => new Response(streamOf(chunks, fail), { status, headers: { 'content-type': 'text/event-stream' } });

// Following a session's events: a stream the host ends, and one cut mid-way, are opened
// again from the last frame's id (Last-Event-ID), with the token; a deleted session
// ends it.
{
  const asked = [];
  const answers = [
    () => sse(200, ['id: b:1\nevent: snapshot\ndata: {"busy":true}\n\n', 'id: b:2\nevent: event\ndata: {"type":"prompt","text":"go"}\n\n']),
    () => sse(200, ['id: b:3\nevent: event\ndata: {"type":"model_text","text":"done"}\n\n'], true),
    () => { throw new TypeError('connection refused'); },
    () => sse(200, ['event: deleted\ndata: {}\n\n']),
  ];
  const remote = new RemoteClient({
    url: 'http://127.0.0.1:9/', token: 'tok',
    fetch: async (url, init) => {
      asked.push({ url, auth: init.headers.authorization, last: init.headers['last-event-id'] ?? null });
      return answers.shift()();
    },
  });
  const seen = [];
  const states = [];
  const ended = await remote.follow('abc', { onFrame: (frame) => seen.push(`${frame.kind}:${frame.id ?? ''}`), onState: (state) => states.push(state) });
  assert.deepEqual(seen, ['snapshot:b:1', 'event:b:2', 'event:b:3', 'deleted:']);
  assert.deepEqual(asked.map((one) => one.last), [null, 'b:2', 'b:3', 'b:3'], 'each reconnect resumes after the last frame');
  assert.ok(asked.every((one) => one.url === 'http://127.0.0.1:9/v1/sessions/abc/events' && one.auth === 'Bearer tok'));
  assert.ok(states.includes('waiting'), 'a cut stream says it waits');
  assert.equal(ended.ended, 'deleted');
  // A refusal no reconnect can change ends it.
  const gone = new RemoteClient({ url: 'http://h', token: 't', fetch: async () => new Response('{"error":"session not found","code":"not_found"}', { status: 404 }) });
  await assert.rejects(gone.follow('abc', { onFrame() {} }), (error) => error.kind === 'not_found' && error.status === 404);
}

// A frame is acknowledged only once its application succeeds; failed handlers replay it.
{
  const resumes = [];
  const remote = new RemoteClient({ url: 'http://host', token: 't', fetch: async (_url, init) => {
    resumes.push(init.headers['last-event-id'] ?? null);
    return sse(200, ['id: r:1\nevent: event\ndata: {}\n\n']);
  } });
  let calls = 0;
  await remote.follow('s', { onFrame() { if (++calls === 1) throw new Error('storage unavailable'); return false; } });
  assert.deepEqual(resumes, [null, null], 'an unapplied event never advances the reconnect cursor');
}

// A host immediately ending an empty event stream backs off and releases abort listeners.
{
  const controller = new AbortController();
  let added = 0;
  let removed = 0;
  const add = controller.signal.addEventListener.bind(controller.signal);
  const remove = controller.signal.removeEventListener.bind(controller.signal);
  controller.signal.addEventListener = (...args) => { added += 1; return add(...args); };
  controller.signal.removeEventListener = (...args) => { removed += 1; return remove(...args); };
  let calls = 0;
  const started = Date.now();
  const remote = new RemoteClient({ url: 'http://host', token: 't' });
  remote.events = async function* () { if (++calls === 3) yield { kind: 'deleted', data: {} }; };
  await remote.follow('s', { signal: controller.signal, onFrame() {} });
  assert.ok(Date.now() - started >= 1400, 'two immediate EOFs do not cause a reconnect spin');
  assert.equal(added, removed, 'finished pauses remove their abort listener');
}

// Commands: a request that may not have arrived (no answer, outcome_unknown) is sent
// again with the same request ID; a plain refusal is not.
{
  const bodies = [];
  const answers = [
    () => { throw new TypeError('network'); },
    () => new Response('{"error":"may or may not","code":"outcome_unknown"}', { status: 500 }),
    () => new Response('{"request_id":"x","state":"running"}', { status: 202 }),
  ];
  const remote = new RemoteClient({ url: 'http://h', token: 't', fetch: async (url, init) => { bodies.push(JSON.parse(init.body)); return answers.shift()(); } });
  const receipt = await remote.command('say', { session: 's', text: 'hi' });
  assert.equal(receipt.state, 'running');
  assert.equal(bodies.length, 3);
  assert.ok(bodies.every((body) => body.request_id === bodies[0].request_id && body.kind === 'say' && body.text === 'hi'), 'the same request, the same ID');
  let calls = 0;
  const strict = new RemoteClient({ url: 'http://h', token: 't', fetch: async () => { calls += 1; return new Response('{"error":"head is 3","code":"head_moved","head":3}', { status: 409 }); } });
  await assert.rejects(strict.command('say', { session: 's', text: 'x' }), (error) => error.kind === 'head_moved' && error.data.head === 3);
  assert.equal(calls, 1, 'a refusal is final');
  assert.equal(worthRepeating({ status: 0 }), true);
  assert.equal(worthRepeating({ status: 409, kind: 'busy' }), false);
  assert.match(requestId(), /^[0-9a-f]{32}$/);
}

// Bringing a thread in: the raw bundle with its Idempotency-Key and query; a bundle
// cut short on the way back is refused, so it is asked for again.
{
  let sent = null;
  const remote = new RemoteClient({
    url: 'http://h', token: 't',
    fetch: async (url, init) => {
      sent = { url, init };
      return new Response('{"session":{"id":"s1"},"fit":{},"replayed":false}', { status: 201 });
    },
  });
  const reply = await remote.importThread(new Uint8Array([1, 2, 3]), { key: 'k'.repeat(20), name: 'My notes', provider: 'local', model: 'm 1' });
  assert.equal(reply.session.id, 's1');
  assert.equal(sent.url, 'http://h/v1/threads?name=My%20notes&provider=local&model=m%201');
  assert.equal(sent.init.headers['idempotency-key'], 'k'.repeat(20));
  assert.equal(sent.init.headers['content-type'], 'application/octet-stream');
  assert.equal(sent.init.body.size, 3, 'the bytes as they are, with their length');
  const short = new RemoteClient({ url: 'http://h', token: 't', fetch: async () => new Response(new Uint8Array(4), { status: 200, headers: { 'content-length': '10', 'x-spindle-head': '7' } }) });
  await assert.rejects(short.exportThread('s1'), (error) => error.kind === 'cut');
  const whole = new RemoteClient({ url: 'http://h', token: 't', fetch: async () => new Response(new Uint8Array(4), { status: 200, headers: { 'x-spindle-head': '7', 'x-spindle-identity': 'id1' } }) });
  const exported = await whole.exportThread('s1');
  assert.deepEqual([exported.bytes.byteLength, exported.head, exported.identity], [4, 7, 'id1']);
}

// One-click connect's PKCE (S256) and the answer it waits for: only the host's
// /connect/done for this request's state.
{
  const { createHash } = await import('node:crypto');
  const { verifier, challenge, state } = await pkce();
  assert.match(verifier, /^[A-Za-z0-9_-]{43}$/);
  assert.equal(challenge, createHash('sha256').update(verifier).digest('base64url'), 'the challenge is base64url(SHA-256(verifier)), unpadded');
  assert.match(state, /^[A-Za-z0-9_-]{22}$/);
  const base = 'http://127.0.0.1:8790';
  assert.equal(connectAnswer(`${base}/connect?client=x&state=${state}`, base, state), null, 'the approval page is not the answer');
  assert.equal(connectAnswer(`http://127.0.0.1:8791/connect/done?code=c&state=${state}`, base, state), null, 'another origin is not the host');
  assert.deepEqual(connectAnswer(`${base}/connect/done?code=c0de&state=${state}`, base, state), { code: 'c0de' });
  assert.throws(() => connectAnswer(`${base}/connect/done?code=c0de&state=other`, base, state), /another request/);
  assert.throws(() => connectAnswer(`${base}/connect/done?error=denied&state=${state}`, base, state), (error) => error.kind === 'denied');
}

// Moves to the host and back, stopped at every step as if the engine died there, then
// taken up by a fresh engine on the same storage: afterwards the thread is live in
// exactly one place, and nothing of the move is left (also when the host's answer was
// lost on the way, or the host was out of reach a while).
function moveWorld() {
  const records = new Map();
  const world = { cutAt: null, failAt: null, records };
  const io = {
    read: async (name, key) => structuredClone(records.get(`${name}:${key}`)),
    keysOf: async (name) => [...records.keys()].filter((key) => key.startsWith(`${name}:`)).map((key) => key.slice(name.length + 1)),
    update: async (name, key, change, { changes = [] } = {}) => {
      world.beforeWrite?.();
      const before = records.get(`${name}:${key}`);
      if (!before) return;
      const next = change(structuredClone(before));
      records.set(`${name}:${key}`, structuredClone(next));
      for (const [store, otherKey, value] of changes) {
        if (value === undefined) records.delete(`${store}:${otherKey}`);
        else records.set(`${store}:${otherKey}`, structuredClone(value));
      }
      return next;
    },
  };
  // This browser's copy of the thread, and the host's sessions (by id) and keys.
  // `fenced`: its place holds a fence (sealed, then dropped), which an import refuses.
  const local = { live: true, sealed: false, dropped: false, fenced: false, restored: 0 };
  const host = { up: true, sessions: new Map(), keys: new Map(), imports: 0, exports: 0, refuse: null, loseReply: null, busy: false, failExport: null, failDelete: false };
  const fail = (status, kind, text) => Object.assign(new Error(`${kind}: ${text}`), { status, kind });
  const remote = () => {
    if (!host.up) throw fail(0, 'host', 'your host is not connected');
    return {
      async importThread(bytes, { key }) {
        if (!host.up) throw fail(0, 'unreachable', 'no answer');
        if (host.dropRequest) {
          // Lost on the way: the host never saw it.
          host.dropRequest = false;
          throw fail(0, 'unreachable', 'no answer');
        }
        // As the host: refusals of the request itself come before its key is looked up;
        // a bundle it cannot read or a model it cannot send to, only for a new key.
        const late = ['bad_bundle', 'no_credentials'].includes(host.refuse?.kind);
        if (host.refuse && !late) throw host.refuse;
        const digest = bytes.join(',');
        const known = host.keys.get(key);
        let id = known?.session;
        if (known && known.digest !== digest) throw fail(409, 'conflict', 'another bundle');
        if (!known && late) throw host.refuse;
        if (!known) {
          host.imports += 1;
          id = `s${host.imports}`;
          host.sessions.set(id, { bytes, sealed: false });
          host.keys.set(key, { session: id, digest });
        }
        if (host.loseReply === 'import') {
          host.loseReply = null;
          throw fail(0, 'unreachable', 'the reply was lost');
        }
        return { session: { id, settings: { provider: 'hp', model: 'hm', runner: 'none', network: true } }, fit: { changes: {} }, replayed: Boolean(known) };
      },
      // A current host says how large the thread's bundle would be (`bundle_bytes`);
      // an older one says nothing of it (`host.saysSize` false).
      async session(id) {
        const session = host.sessions.get(id);
        if (!session) throw fail(404, 'not_found', 'session not found');
        return { id, ...(host.saysSize !== false && { bundle_bytes: session.size ?? session.bytes.length }) };
      },
      async exportThread(id) {
        const session = host.sessions.get(id);
        if (!session) throw fail(404, 'not_found', 'session not found');
        // As the host: a sealed thread's bundle goes whatever else holds it.
        if (session.sealed && !host.failExport && !host.loseReply) {
          host.exports += 1;
          return { bytes: session.bytes, head: 3, identity: 'thread-identity' };
        }
        if (host.busy) throw fail(409, 'busy', 'a command is running');
        if (host.loseReply === 'export-running') {
          // The export goes on there (writing its bundle) while its answer is lost.
          host.loseReply = null;
          host.busy = true;
          throw fail(0, 'unreachable', 'the reply was lost');
        }
        host.exports += 1;
        session.sealed = true;
        if (host.failExport) {
          const failed = host.failExport;
          host.failExport = null;
          throw failed;
        }
        if (host.loseReply === 'export') {
          host.loseReply = null;
          throw fail(0, 'unreachable', 'the reply was lost');
        }
        return { bytes: session.bytes, head: 3, identity: 'thread-identity' };
      },
      async command(kind, { session }) {
        assert.equal(kind, 'delete');
        if (host.failDelete) return { state: 'failed', error: 'disk full' };
        if (!host.sessions.has(session)) throw fail(404, 'not_found', 'session not found');
        host.sessions.delete(session);
        for (const [key, known] of host.keys) if (known.session === session) host.keys.delete(key);
        return { state: 'completed' };
      },
    };
  };
  const moves = () => hostMoves({
    ...io,
    remote,
    local: {
      copy: async () => new Uint8Array([7, 7, 7]),
      facts: () => ({ head: 3, identity: 'thread-identity' }),
      sealed: async () => local.sealed,
      seal: async () => { Object.assign(local, { sealed: true, live: false, fenced: true }); },
      dropSealed: async () => { local.dropped = true; },
      clear: async () => { Object.assign(local, { live: false, sealed: false, dropped: false, fenced: false }); },
      restore: async () => {
        if (local.fenced) throw new Error('conflict: the place is not empty; import into a new place');
        local.fenced = true;
        local.live = true;
        local.restored += 1;
        return { fields: { runner: 'app' }, said: ["its commands run in this browser's Linux"] };
      },
    },
    cut: async (step) => {
      if (world.failAt === step) {
        world.failAt = null;
        throw new Error(`storage failed at ${step}`);
      }
      if (world.cutAt === step) {
        world.cutAt = null;
        throw Object.assign(new Error(`cut at ${step}`), { kind: 'cut' });
      }
    },
  });
  records.set('threads:t1', { id: 't1', title: 'Notes', state: 'idle' });
  const liveCopies = () => (local.live ? 1 : 0) + [...host.sessions.values()].filter((session) => !session.sealed).length;
  const leftOver = () => [...records.keys()].filter((key) => key.startsWith('kv:host-move:') || key.startsWith('bundles:host-move:'));
  return { world, records, local, host, moves, liveCopies, leftOver, thread: () => records.get('threads:t1') };
}
const crashed = (error) => error?.kind === 'cut';

for (const step of ['to:recorded', 'to:uploaded', 'to:session', 'to:sealed', 'to:home', null]) {
  const w = moveWorld();
  w.world.cutAt = step;
  let done = null;
  try {
    ({ done } = await w.moves().toHost('t1', { url: 'http://host', name: 'Notes' }));
    await done;
  } catch (error) {
    assert.ok(crashed(error), `to the host, cut at ${step}: ${error.message}`);
  }
  if (step) {
    // Meanwhile: one live copy, or a recorded move that holds the thread (no work here).
    assert.ok(w.liveCopies() === 1 || (await w.moves().holding('t1')), `cut at ${step}: one live copy, or a move that holds it`);
    await w.moves().resume();
  }
  assert.equal(w.liveCopies(), 1, `cut at ${step}: one live copy after`);
  assert.equal(w.local.live, false, `cut at ${step}: the thread lives on the host`);
  assert.equal(w.host.imports, 1, `cut at ${step}: imported once`);
  assert.deepEqual(w.thread().home, { url: 'http://host', session: 's1' });
  assert.equal(w.thread().moving, null);
  assert.equal(w.thread().provider, 'hp', "the host's model, as its fit says");
  assert.equal(w.local.dropped, true, `cut at ${step}: the sealed copy here goes`);
  assert.deepEqual(w.leftOver(), [], `cut at ${step}: nothing of the move is left`);
}

// The host's answer lost on the way: the same key brings the same session (no second
// import); out of reach meanwhile: the move waits, saying so, and the thread stays here.
{
  const w = moveWorld();
  w.host.loseReply = 'import';
  const { done } = await w.moves().toHost('t1', { url: 'http://host', name: 'Notes' });
  await assert.rejects(done, (error) => error instanceof Waiting);
  assert.equal(w.local.live, true, 'not sealed before the host answered');
  assert.match(w.thread().moving.why, /reply was lost/);
  w.host.up = false;
  await w.moves().resume();
  assert.equal(w.local.live, true);
  assert.equal(w.records.get('kv:host-move:t1').step, 'upload', 'still recorded, to send again');
  w.host.up = true;
  await w.moves().resume();
  assert.equal(w.host.imports, 1, 'the replay imported nothing again');
  assert.equal(w.liveCopies(), 1);
  assert.equal(w.thread().home.session, 's1');
  assert.deepEqual(w.leftOver(), []);
}

// A crash or lost reply makes later refusals ambiguous: the host may already own it.
{
  const w = moveWorld();
  w.host.loseReply = 'import';
  const started = await w.moves().toHost('t1', { url: 'http://host', name: 'Notes' });
  await assert.rejects(started.done, (error) => error instanceof Waiting);
  w.host.refuse = Object.assign(new Error('bad_request: incomplete body'), { status: 400, kind: 'bad_request' });
  await w.moves().resume();
  assert.equal(await w.moves().holding('t1'), true, 'the local copy stays held after an uncertain upload');
  assert.ok(w.records.has('bundles:host-move:t1'), 'the exact upload remains available');
  w.host.refuse = null;
  await w.moves().resume();
  assert.equal(w.liveCopies(), 1);
}
for (const cut of ['cutAt', 'failAt']) {
  const w = moveWorld();
  w.world[cut] = 'to:uploaded';
  await assert.rejects((await w.moves().toHost('t1', { url: 'http://host' })).done, cut === 'cutAt' ? crashed : /storage failed/);
  assert.equal(w.records.get('kv:host-move:t1').attempted, true, 'error reporting preserves the durable send marker');
  w.host.refuse = Object.assign(new Error('conflict: update in progress'), { status: 409, kind: 'conflict' });
  await w.moves().resume();
  assert.equal(await w.moves().holding('t1'), true, 'a crash before persisting the reply cannot unlock a second live copy');
}

{
  const w = moveWorld();
  w.host.loseReply = 'import';
  await assert.rejects((await w.moves().toHost('t1', { url: 'http://host' })).done, (error) => error instanceof Waiting);
  const held = w.records.get('bundles:host-move:t1');
  w.records.delete('bundles:host-move:t1');
  await w.moves().resume();
  assert.equal(await w.moves().holding('t1'), true, 'missing recovery bytes cannot unlock a second live copy');
  w.records.set('bundles:host-move:t1', held);
  await w.moves().resume();
  assert.equal(w.liveCopies(), 1);
}
{
  const records = new Map();
  const held = { bytes: new Uint8Array([7]) };
  const move = { id: 't1', way: 'to', step: 'upload' };
  const moves = hostMoves({
    read: async (store, key) => records.get(`${store}:${key}`),
    keysOf: async (store) => {
      if (store === 'kv') {
        // A new move commits after the scan's snapshot but before bundle enumeration.
        records.set('kv:host-move:t1', move);
        records.set('bundles:host-move:t1', held);
        return [];
      }
      return ['host-move:t1'];
    },
    update() { throw new Error('not used'); },
    remote() { throw new Error('not used'); }, local: {},
  });
  await moves.resume();
  assert.equal(records.get('bundles:host-move:t1'), held, 'a stale scan never deletes a concurrent move\'s bundle');
}

// A lost answer, then a refusal only a new key can get: the host had imported it under
// that key, so it answers with the session (as the host does), never a rollback.
{
  const w = moveWorld();
  w.host.loseReply = 'import';
  await assert.rejects((await w.moves().toHost('t1', { url: 'http://host' })).done, (error) => error instanceof Waiting);
  w.host.refuse = Object.assign(new Error('no_credentials: this host cannot send to x y'), { status: 409, kind: 'no_credentials' });
  await w.moves().resume();
  assert.equal(w.liveCopies(), 1);
  assert.equal(w.local.live, false, 'it lives on the host');
  assert.equal(w.host.sessions.size, 1);
}
// The first request lost on the way (the host never saw it), then a refusal only a new
// key can get: proof that nothing was imported under the key, so the move rolls back.
{
  const w = moveWorld();
  w.host.dropRequest = true;
  await assert.rejects((await w.moves().toHost('t1', { url: 'http://host' })).done, (error) => error instanceof Waiting);
  assert.equal(w.records.get('kv:host-move:t1').attempted, true);
  w.host.refuse = Object.assign(new Error('no_credentials: this host cannot send to x y'), { status: 409, kind: 'no_credentials' });
  await w.moves().resume();
  assert.equal(w.local.live, true, 'rolled back, not waiting for good');
  assert.equal(w.host.sessions.size, 0);
  assert.match(w.thread().note, /stayed in this browser: your host refused it/);
}
// No host to send to (not connected): nothing is attempted, so the first real send's
// refusal still rolls back; a refusal after an attempt that may have reached the host
// (a too large body: no key looked up) waits instead.
{
  const w = moveWorld();
  w.host.up = false;
  await assert.rejects((await w.moves().toHost('t1', { url: 'http://host' })).done, (error) => error instanceof Waiting);
  assert.equal(w.records.get('kv:host-move:t1').attempted, undefined, 'nothing was sent, so nothing was attempted');
  w.host.up = true;
  w.host.refuse = Object.assign(new Error('too_large: over 2 GiB'), { status: 413, kind: 'too_large' });
  await w.moves().resume();
  assert.equal(w.local.live, true);
  assert.match(w.thread().note, /stayed in this browser/);
  const later = moveWorld();
  later.host.loseReply = 'import';
  await assert.rejects((await later.moves().toHost('t1', { url: 'http://host' })).done, (error) => error instanceof Waiting);
  later.host.refuse = Object.assign(new Error('too_large: over 2 GiB'), { status: 413, kind: 'too_large' });
  await later.moves().resume();
  assert.equal(await later.moves().holding('t1'), true, 'after an attempt that may have imported it, a 413 is no proof');
}
// Back: an export whose answer was lost goes on there; asked again meanwhile, the host is
// busy with that very export, which is no "it stays on your host": the move waits, and
// gets the sealed bundle once the export is done.
{
  const w = await hosted();
  w.host.loseReply = 'export-running';
  await assert.rejects((await w.moves().back('t1')).done, (error) => error instanceof Waiting);
  await assert.rejects(w.moves().resume().then(() => { throw new Waiting('still'); }), (error) => error instanceof Waiting);
  assert.equal(w.records.get('kv:host-move:t1')?.step, 'export', 'still moving back, not ended');
  assert.equal(w.thread().moving?.way, 'back');
  w.host.busy = false;
  await w.moves().resume();
  assert.equal(w.liveCopies(), 1);
  assert.equal(w.local.live, true);
  assert.deepEqual(w.leftOver(), []);
}

// A refusal the same request cannot change rolls the move back: the thread stays here.
{
  const w = moveWorld();
  w.host.refuse = Object.assign(new Error('no_credentials: this host cannot send to x y'), { status: 409, kind: 'no_credentials' });
  const { done } = await w.moves().toHost('t1', { url: 'http://host', name: 'Notes' });
  assert.equal((await done).done, false);
  assert.equal(w.local.live, true);
  assert.equal(w.host.sessions.size, 0);
  assert.equal(w.thread().home, undefined);
  assert.match(w.thread().note, /stayed in this browser: your host refused it: this host cannot send/);
  assert.deepEqual(w.leftOver(), []);
}

/** A world whose thread already lives on the host. */
async function hosted() {
  const w = moveWorld();
  await (await w.moves().toHost('t1', { url: 'http://host', name: 'Notes' })).done;
  w.local.dropped = false;
  return w;
}
// Transfer commits merge with concurrent index updates, including after return.
{
  const w = await hosted();
  w.world.beforeWrite = () => {
    w.records.set('threads:t1', { ...w.thread(), title: 'Renamed meanwhile', state: 'running' });
    w.world.beforeWrite = null;
  };
  w.world.cutAt = 'back:released';
  await assert.rejects((await w.moves().back('t1')).done, crashed);
  w.world.beforeWrite = () => {
    w.records.set('threads:t1', { ...w.thread(), title: 'Latest title', state: 'running' });
    w.world.beforeWrite = null;
  };
  await w.moves().resume();
  assert.equal(w.thread().title, 'Latest title');
  assert.equal(w.thread().state, 'running', 'release cleanup cannot overwrite a turn that started here');
}

for (const step of ['back:recorded', 'back:held', 'back:imported', 'back:released', null]) {
  const w = await hosted();
  w.world.cutAt = step;
  try {
    await (await w.moves().back('t1')).done;
  } catch (error) {
    assert.ok(crashed(error), `back, cut at ${step}: ${error.message}`);
  }
  if (step) {
    assert.ok(w.liveCopies() === 1 || (await w.moves().holding('t1')), `back, cut at ${step}: one live copy, or a move that holds it`);
    await w.moves().resume();
  }
  assert.equal(w.liveCopies(), 1, `back, cut at ${step}: one live copy after`);
  assert.equal(w.local.live, true, `back, cut at ${step}: the thread lives here`);
  assert.equal(w.host.sessions.size, 0, `back, cut at ${step}: the host's sealed copy is deleted`);
  assert.equal(w.thread().home, null);
  assert.equal(w.thread().runner, 'app', "the browser's fit");
  assert.match(w.thread().note, /Back from your host\. Changed to run here: its commands run in this browser's Linux/);
  assert.deepEqual(w.leftOver(), [], `back, cut at ${step}: nothing of the move is left`);
}

// Back: the export's answer lost (the host sealed its copy) is asked for again and sends
// the same bytes; a thread at work on the host stays there; the host's delete waits
// for the host while the thread already works here.
{
  const w = await hosted();
  w.host.loseReply = 'export';
  await assert.rejects((await w.moves().back('t1')).done, (error) => error instanceof Waiting);
  assert.equal(w.liveCopies(), 0, 'sealed there, not yet here: in a recorded move');
  assert.equal(w.records.get('kv:host-move:t1').step, 'export');
  await w.moves().resume();
  assert.equal(w.host.exports, 2);
  assert.equal(w.liveCopies(), 1);
  assert.equal(w.local.live, true);
  assert.deepEqual(w.leftOver(), []);

  // A failure after the host may have sealed its copy is no refusal: asked again.
  const failing = await hosted();
  failing.host.failExport = Object.assign(new Error('internal: the disk is full'), { status: 500, kind: 'internal' });
  await assert.rejects((await failing.moves().back('t1')).done, (error) => error instanceof Waiting);
  assert.equal(failing.records.get('kv:host-move:t1').step, 'export', 'not rolled back');
  await failing.moves().resume();
  assert.equal(failing.liveCopies(), 1);
  assert.equal(failing.local.live, true);
  assert.deepEqual(failing.leftOver(), []);

  const busy = await hosted();
  busy.host.busy = true;
  assert.equal((await (await busy.moves().back('t1')).done).done, false);
  assert.equal(busy.liveCopies(), 1);
  assert.equal(busy.local.live, false, 'it stays on the host');
  assert.match(busy.thread().note, /stays on your host: a command is running/);
  assert.deepEqual(busy.leftOver(), []);

  // Too large for this browser: it stays on the host as it was, nothing sealed or sent
  // there (the host says the size before any export). An older host, saying nothing,
  // is asked for the bundle as before.
  const large = await hosted();
  const [hostedId] = large.host.sessions.keys();
  large.host.sessions.get(hostedId).size = 2 ** 31;
  const exportsBefore = large.host.exports;
  assert.equal((await (await large.moves().back('t1')).done).done, false);
  assert.equal(large.host.exports, exportsBefore, 'no export was asked for');
  assert.equal(large.host.sessions.get(hostedId).sealed, false, 'nothing sealed on the host');
  assert.equal(large.liveCopies(), 1);
  assert.equal(large.local.live, false, 'it stays on the host');
  assert.match(large.thread().note, /stays on your host: it would come back as about 2\.0 GB, more than this browser takes \(1\.0 GB\)/);
  assert.deepEqual(large.leftOver(), []);
  const older = await hosted();
  older.host.saysSize = false;
  await (await older.moves().back('t1')).done;
  assert.equal(older.local.live, true, 'an older host: brought back as before');
  assert.deepEqual(older.leftOver(), []);

  const slow = await hosted();
  slow.world.cutAt = 'back:imported';
  await assert.rejects((await slow.moves().back('t1')).done, crashed);
  slow.host.up = false;
  await slow.moves().resume();
  const left = slow.records.get('kv:host-move:t1');
  assert.equal(left.step, 'release');
  assert.equal(await slow.moves().holding('t1'), false, 'live here again: it takes work while the host is away');
  assert.equal(slow.records.has('bundles:host-move:t1'), false, 'its bytes went once it was in');
  slow.host.up = true;
  await slow.moves().resume();
  assert.equal(slow.host.sessions.size, 0);
  assert.deepEqual(slow.leftOver(), []);
}
{
  const w = await hosted();
  w.host.failDelete = true;
  await assert.rejects((await w.moves().back('t1')).done, (error) => error instanceof Waiting);
  assert.equal(w.records.get('kv:host-move:t1').step, 'release', 'a failed deletion keeps cleanup pending');
  assert.equal(w.local.live, true);
  assert.equal(w.host.sessions.size, 1);
  w.host.failDelete = false;
  await w.moves().resume();
  assert.equal(w.host.sessions.size, 0);
  assert.deepEqual(w.leftOver(), []);
}
assert.match(moveKey('ab:cd/ef', 12), /^abcdef-12-[0-9a-f]{12}$/);

// A panel replaces its whole thread list with a snapshot's, after the broadcasts sent
// before it: a thread that changed while the snapshot's other reads waited (a move that
// finished as a restarted engine was asked for its state) must be in it as it is now,
// or the panel would show the move under way for good.
{
  const moved = '00000000000000ab';
  records.set(`threads:${moved}`, { id: moved, updated: 1, moving: { way: 'to', step: 'upload' }, home: null });
  const vaultRead = deferred();
  context.vault = { state: () => vaultRead.promise };
  engine.setClient({ info: async () => ({}) });
  const taking = engine.snapshot();
  await tick();
  records.set(`threads:${moved}`, { id: moved, updated: 2, moving: null, home: { url: 'http://host', session: 's1' } });
  vaultRead.resolve({ mode: 'device', locked: false, kinds: {}, accounts: {}, ended: {}, sites: [] });
  const taken = (await taking).threads.find((thread) => thread.id === moved);
  assert.deepEqual([taken.moving, taken.home?.session], [null, 's1'], 'the snapshot holds the thread as it was when the snapshot answered');
  records.delete(`threads:${moved}`);
  delete context.vault;
}
assert.equal(sentence(Object.assign(new Error('busy: a command is running'), { kind: 'busy' })), 'a command is running');
// Files a thread puts in a page: its own workspace's, read by the engine when the model
// asks (never a script's or a task's call, never an `upload` the model wrote, never past
// UPLOAD); a file viewer address naming another thread is refused.
{
  const me = 'aaaaaaaaaaaaaaaa';
  const session = `/threads/${me}`;
  const reads = [];
  const savedFiles = engine.ops.files;
  const savedRead = engine.ops.read;
  engine.ops.files = async ({ id: thread }) => ({ files: thread === me ? [{ path: 'resume.pdf', size: 3 }, { path: 'big.pdf', size: UPLOAD.file + 1 }, { path: 'cv/letter.docx', size: 2 }] : [] });
  engine.ops.read = async ({ id: thread, path }) => { reads.push(`${thread}:${path}`); return { bytes: new Uint8Array(path === 'resume.pdf' ? [1, 2, 3] : [4, 5]) }; };
  const model = { call: 'call-1', session };
  const ready = await engine.prepared('select', { ref: 'k1', files: ['/workspace/resume.pdf', './cv/letter.docx'], upload: [{ name: 'stolen', base64: 'AA==' }] }, model);
  assert.equal(ready.error, undefined, ready.error);
  assert.deepEqual(JSON.parse(JSON.stringify(ready.input.upload)), [
    { path: 'resume.pdf', name: 'resume.pdf', type: 'application/pdf', size: 3, base64: 'AQID' },
    { path: 'cv/letter.docx', name: 'letter.docx', type: 'application/vnd.openxmlformats-officedocument.wordprocessingml.document', size: 2, base64: 'BAU=' },
  ], 'its own files, read now; the upload the model wrote is gone');
  assert.deepEqual(reads, [`${me}:resume.pdf`, `${me}:cv/letter.docx`]);
  const stripped = await engine.prepared('select', { ref: 'k1', values: ['a'], upload: [{ name: 'x' }] }, model);
  assert.equal('upload' in stripped.input, false, 'an upload never passes without files');
  for (const [input, call, why] of [
    [{ files: ['resume.pdf'] }, { call: 'code-7f3a', session }, /a script cannot put files in a page/],
    [{ files: ['resume.pdf'] }, { call: 'tasks/t1/code-7ab', session }, /a script cannot/],
    [{ files: ['resume.pdf'] }, { call: 'tasks/code-review/call-2', session }, /a task cannot put files in a page/],
    [{ files: ['resume.pdf'] }, { call: 'tasks/t1/call-2', session }, /a task cannot put files in a page/],
    [{ files: ['../other/secret.txt'] }, model, /is no file of the workspace/],
    [{ files: ['/etc/passwd'] }, model, /is no file of the workspace/],
    [{ files: ['missing.pdf'] }, model, /the workspace has no file missing\.pdf/],
    [{ files: ['big.pdf'] }, model, /big\.pdf is 25\.1 MB: at most 25 MB a file/],
    [{ files: Array(UPLOAD.count + 1).fill('resume.pdf') }, model, /give 1 to 10 files/],
  ]) {
    reads.length = 0;
    assert.match((await engine.prepared('select', { ref: 'k1', ...input }, call)).error, why, JSON.stringify(input));
    assert.deepEqual(reads, [], 'nothing read for a refused call');
  }
  // All of them past UPLOAD.total, listed or as read (a file grown since it was listed).
  engine.ops.files = async () => ({ files: [{ path: 'one.pdf', size: 24 * 1048576 }, { path: 'two.pdf', size: 24 * 1048576 }, { path: 'grew.pdf', size: 3 }] });
  assert.match((await engine.prepared('select', { ref: 'k1', files: ['one.pdf', 'two.pdf'] }, model)).error, /the files come to more than 40 MB/);
  engine.ops.read = async () => ({ bytes: new Uint8Array(UPLOAD.file + 1) });
  assert.match((await engine.prepared('select', { ref: 'k1', files: ['grew.pdf'] }, model)).error, /grew\.pdf is 25\.1 MB: at most 25 MB a file/);
  assert.match((await engine.prepared('navigate', { url: `chrome-extension://id/view.html?thread=bbbbbbbbbbbbbbbb&path=a.md` }, model)).error, /another thread's file/);
  assert.equal((await engine.prepared('navigate', { url: '/workspace/a.md' }, model)).input.url, `chrome-extension://id/view.html?thread=${me}&path=a.md`, 'its own file opens');
  engine.ops.files = savedFiles;
  engine.ops.read = savedRead;
  // Asked about, an upload names each file and its size; under Auto it is asked about.
  assert.equal(sends('select', { files: ['a.pdf'] }), true);
  assert.equal(sends('select', { values: ['a'] }), false);
  assert.equal(sends('select', { files: [] }), false, 'no files, nothing sent');
  assert.equal(actionText('select', ready.input, { what: 'file "Resume"', host: 'jobs.example' }), 'Upload “resume.pdf” (3 bytes), “cv/letter.docx” (2 bytes) to file "Resume" on jobs.example', 'the question names the files the engine read, with their sizes');
  assert.equal(actionText('select', { files: ['x'], upload: [{ path: 'a.pdf” to file "CV" on safe.example', size: 0 }] }, { what: 'file "CV"', host: 'evil.example' }), 'Upload “a.pdf" to file "CV" on safe.example” (0 bytes) to file "CV" on evil.example', "a name's quotation marks cannot close the question's");
  assert.equal(actionText('click', { ref: 'k1' }, { what: 'button "Confirm the order of the three items in your basket and…"', words: 'Confirm the order of the three items in your basket and pay the total now: $10', dest: '/pay', host: 'shop.example' }), 'Click button "Confirm the order of the three items in your basket and…" saying “Confirm the order of the three items in your basket and pay the total now: $10” on shop.example, going to /pay', 'what the action is held to, shown');
  assert.equal(actionText('click', { ref: 'k2' }, { what: 'button "Pay $10"', words: 'Pay $10', dest: null, host: 'shop.example' }), 'Click button "Pay $10" on shop.example', 'words the name holds are not said twice');
  assert.equal(actionText('select', { files: ['a.pdf'], upload: [{ path: 'a.pdf', size: 188_000 }] }, { what: 'file "CV"' }), 'Upload “a.pdf” (184 KB) to file "CV"');
  assert.equal(actionText('press', { key: 'Enter' }, { tab: 3, focus: { what: 'textbox "Amount"' } }), 'Press Enter in textbox "Amount" in tab 3');
  // A thread is told its setup in its instructions (from the user's switches), and a
  // thread leaving this browser takes none.
  records.set(`threads:${me}`, { id: me, runner: 'app' });
  records.set('kv:pages', { reading: true, acting: true, confirm: 'auto' });
  records.set('kv:sites', [{ name: 'gh', url: 'https://api.github.com/' }]);
  const setup = await engine.setupOf(me, { provider: 'deepseek', model: 'v4', effort: 'high', network: true, max_usd: 5 });
  assert.match(setup, /^Model: deepseek\/v4, effort high\.\nCommands: in this browser's Linux, network on\.\nSpending limit: US\$5\.\nPage reading on; page actions on \(the user is asked before what reads as sending or submitting \(judged by its words: no guarantee\) and before files are put in a field\)\.\nSites scripts may call with spindle http: https:\/\/api\.github\.com\/\.\nThe user changes these/);
  const told = engine.guided('Be brief.', true, 'Use metric.', setup);
  assert.equal(engine.guided(told, true, 'Use metric.', setup), told, 'the same setup: the same instructions (the cache stays)');
  const changed = engine.guided(told, true, 'Use metric.', setup.replace('effort high', 'effort low'));
  assert.equal((changed.match(/\[This thread's setup\]/g) ?? []).length, 1, 'replaced, not added');
  assert.ok(!engine.guided(told, false).includes("[This thread's setup]"), 'none for a thread leaving the browser');
  assert.ok(engine.guided(told, false).includes('Use metric.'), 'its standing instructions kept');
  // A thread made under the earlier style line is told the current one, once, in its place.
  const earlierStyle = "Pages you make follow the system's light or dark theme and offer a System/Light/Dark switch; never write em dashes.";
  const restyled = engine.guided(`Be brief.\n\n${earlierStyle}`, false, 'Use metric.');
  assert.ok(!restyled.includes(earlierStyle), 'the earlier style line taken out');
  assert.equal((restyled.match(/one small icon button in a corner/g) ?? []).length, 1, 'the current one told once');
  assert.match(restyled, /have no print button \(Spindle's file tab has one\)/, 'pages are told the file tab prints them');
  assert.equal(engine.guided(restyled, false, 'Use metric.'), restyled, 'and it stays as it is');
  // Sites are named only to a thread whose scripts can call them.
  records.set(`threads:${me}`, { id: me, runner: 'none' });
  assert.doesNotMatch(await engine.setupOf(me, { provider: 'p', model: 'm' }), /spindle http/);
  // A new thread is told its setup from its first turn on (its instructions set inside
  // the turn, before the message goes).
  {
    const order = [];
    let made = null;
    engine.setClient({
      async create(dir, provider, options) { made = options; },
      async status() { return { provider: 'p', model: 'm', settings: { runner: 'none' } }; },
      async settings(dir, changes) {
        if (changes) { order.push(`settings ${changes.instructions ?? ''}`); return {}; }
        return { provider: 'p', model: 'm', effort: 'default', network: false, max_usd: null, instructions: made.instructions, tools: [] };
      },
      async prompt() { order.push('prompt'); return { unsent: [] }; },
      tools: { definitions: () => [] },
      async closeSession() {},
    });
    vm.runInContext('poolOpening = Promise.resolve({ free: 1e12, rmSync() {}, grow() {} });', context);
    const { id: fresh } = await engine.operate('create', { provider: 'p', text: 'hello', runner: 'none' });
    await engine.running.get(fresh)?.done;
    assert.match(order[0] ?? '', /^settings [\s\S]*\[This thread's setup\]\nModel: p\/m, effort default\.\nCommands: none\./, order.join(' | '));
    assert.equal(order[1], 'prompt', 'then the message');
    records.delete(`threads:${fresh}`);
  }
  records.delete(`threads:${me}`);
  records.delete('kv:pages');
  records.delete('kv:sites');
}

console.log('review-regressions passed: waiting messages kept on disk (until their turn has taken them; restored at start, held, unless taken), a message stopped before its turn began coming back held, a message to a thread that takes none refused at once, the tab a question is about named and pinned, an allowed action held to the page, part or dialog the question showed (refused before acting once the tab reloaded, moved on or shows another dialog; Auto looks nothing up for one that sends nothing), a message coming back (Take back, rewind, a failed send) only into its own thread\'s box, an unreadable Before acting asking, a script\'s question withdrawn when its command ends, icon buttons named Send asked about, moves rolled back after a lost first request and a refusal only a new key gets (never after a lost answer), a move back waiting on its own export, a snapshot holding threads as they are when it answers, the host /v1 client (event stream parsing and resuming, commands sent again with their request ID, bundles in and out), connect PKCE and its state, moves to the host and back cut at every step (one live copy each time), operation races, Before acting (Full, Auto, Ask; allowed, refused, ended), the Dim theme (the old Dark shows Dim), site secret headers checked (names, values, HTTPS only) and named to the package without their values, reset safety, early Stop, a message with files waiting for the turn to end, a turn stopped part way taking a reply (text alone) and Retry after a stopped step, gateway bounds/redirects (the extension store told as closed), IPv6, CDP body IDs, the debugging-bar veto kept (for tabs vetoed at once too, and for actions without the debugger), Before acting kept when site access is taken back, Return asked about as Enter, one action per tab, uncertain actions unknown (engine, its document and the background)');
