// Focused extension regressions without a provider, VM image or Chrome profile.
// usage: node chrome/tests/review_regressions.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { local, storeRefusal, wireNetwork } from '../linux-net.mjs';
import { ACTING_NAMES, ASKING_NAMES, actionText, confirmMode, sends, uncertain } from '../page-tools.mjs';
import { refusedAnswer } from '../../clients/js/common.mjs';
import { EventParser, RemoteClient, requestId, worthRepeating } from '../../clients/js/remote.mjs';
import { MOVE, STEP_TEXT, Waiting, hostMoves, moveKey, said as sentence } from '../host-moves.mjs';
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
  CHANNEL: 'test', self: { location: { href: 'chrome-extension://id/engine-worker.mjs' } }, linuxRunner: () => ({ warm() {} }), PAGE_TOOLS: [], uncertain, ASKING_NAMES, actionText, confirmMode, sends, BROWSER: 'Chrome', storeRefusal, firstGlue: {},
  refusedAnswer, RemoteClient, requestId, worthRepeating, MOVE, STEP_TEXT, Waiting, hostMoves, sentence,
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
vm.runInContext(`${engineSource}\nglobalThis.test = { operate, ops, running, snapshot, restoreQueued, queued, renew, replaceFirst, creationSettings, pageTool, cancelPageTools, secretHeader, changeSites, grantsFor, underPrefix, withSecrets, beforeActing, actOnPage, hostedFrame, answerHosted, endHosted, startHosted, admitHosted, sendHostedStop, hostedTurns, followed, pollHosted, setHost: (value) => { hostState = { connected: true, kind: 'browser', url: 'http://host' }; hostRemote = value; }, setClient: (value) => { client = value; poolOpening = Promise.resolve({ free: 100, rmSync() {} }); } };`, context);
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
// What the service worker writes, and its listener for Chrome's site access taken back.
const bgWrites = [];
let accessRemoved = null;
const closedTabs = [];
const pageContext = vm.createContext({
  // The service worker's own: where it was loaded from, and its events (previews, unused here).
  location: new URL('chrome-extension://spindle/background.js'), self: { addEventListener() {} },
  console, setTimeout, clearTimeout, URL, crypto: globalThis.crypto, read: async () => ({ reading: true, acting: true }), PAGE_ACCESS: [], ACTING_NAMES, uncertain, BROWSER: 'Chrome',
  write: async (store, key, value) => { bgWrites.push([store, key, value]); },
  pageSwitches: () => ({ reading: true, acting: true }),
  chrome: {
    sidePanel: { setPanelBehavior: async () => {} },
    runtime: { onInstalled: event, onMessage: event },
    permissions: { contains: async () => true, onRemoved: { addListener(listener) { accessRemoved = listener; } } },
    debugger: { onDetach: event, onEvent: event, sendCommand: async (_, method, params) => { sent.push({ method, params }); return { body: 'pong' }; } },
    tabs: { get: async (tab) => ({ id: tab, url: 'https://public.example/' }), onRemoved: event, remove: async (tab) => { closedTabs.push(tab); } },
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
vm.runInContext(`${background}\nglobalThis.test = { watched, attached, network, recordNetwork, cdp, calls, inPage, attach, veto, unveto, vetoed, onTab, run };`, pageContext);
const bg = pageContext.test;
// Chrome's site access taken back turns both switches off and keeps Before acting.
pageContext.read = async () => ({ reading: true, acting: true, confirm: 'ask' });
pageContext.PAGE_ACCESS = ['https://*/*'];
pageContext.chrome.debugger.getTargets = async () => [];
await accessRemoved({ origins: ['https://*/*'] });
assert.equal(JSON.stringify(bgWrites.at(-1)), JSON.stringify(['kv', 'pages', { reading: false, acting: false, confirm: 'ask' }]), 'Before acting outlives site access');
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
for (const [name, input] of [['close_tab', { tab: 9 }], ['navigate', { tab: 9, action: 'reload' }], ['scroll', { tab: 9 }], ['select', { tab: 9, ref: 'a1', values: ['x'] }]]) {
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
pageContext.chrome.tabs.update = async () => { throw new Error('Tabs cannot be edited right now'); };
assert.match((await bg.run('navigate', { url: 'https://public.example/x', tab: 18 })).error, /could not go there/);
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
  engine.setClient({ status: async () => status, targets: async () => targets, history: async () => ({ messages }), closeSession: async () => {} });
  const first = () => vm.runInContext(`firstStepStopped(${JSON.stringify(thread)})`, context);
  assert.equal(await first(), true, "a step of the thread's first request starts the turn over");
  messages = [messages[0], call('write_file'), result, call('bash'), result];
  assert.equal(await first(), false, 'a later step is asked again from before its request');
  messages = [messages[0], call('bash'), result];
  targets = [{ revision: 2, kind: 'settled' }, ...targets];
  assert.equal(await first(), false, 'a turn after another one goes back to its start');
  status = { accepts_prompt: false, accepts_message: true, phase: 'model_wait' };
  assert.equal(await first(), false, 'a cut model request is sent again');
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

// Settings, Pages, Before acting: Full (the default) asks nothing; Auto asks before an
// action that sends or submits; Ask before every action that changes a page. A waiting
// action shows to panels, goes on when allowed, and is refused (nothing done) when the
// user says no or the turn ends.
{
  assert.equal(confirmMode(null), 'full');
  assert.equal(confirmMode({ confirm: 'auto' }), 'auto');
  assert.equal(confirmMode({ confirm: 'nonsense' }), 'full');
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
  context.self.postMessage = posting;
  records.delete('kv:pages');
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

console.log('review-regressions passed: waiting messages kept on disk (until their turn has taken them; restored at start, held, unless taken), a message stopped before its turn began coming back held, a message to a thread that takes none refused at once, the tab a question is about named and pinned, an unreadable Before acting asking, a script\'s question withdrawn when its command ends, icon buttons named Send asked about, moves rolled back after a lost first request and a refusal only a new key gets (never after a lost answer), a move back waiting on its own export, a snapshot holding threads as they are when it answers, the host /v1 client (event stream parsing and resuming, commands sent again with their request ID, bundles in and out), connect PKCE and its state, moves to the host and back cut at every step (one live copy each time), operation races, Before acting (Full, Auto, Ask; allowed, refused, ended), the Dim theme (the old Dark shows Dim), site secret headers checked (names, values, HTTPS only) and named to the package without their values, reset safety, early Stop, a message with files waiting for the turn to end, a turn stopped part way taking a reply (text alone) and Retry after a stopped step, gateway bounds/redirects (the extension store told as closed), IPv6, CDP body IDs, the debugging-bar veto kept (for tabs vetoed at once too, and for actions without the debugger), Before acting kept when site access is taken back, Return asked about as Enter, one action per tab, uncertain actions unknown (engine, its document and the background)');
