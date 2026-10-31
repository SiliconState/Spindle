// Focused extension regressions without a provider, VM image or Chrome profile.
// usage: node chrome/tests/review_regressions.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { local, storeRefusal, wireNetwork } from '../linux-net.mjs';
import { ACTING_NAMES, uncertain } from '../page-tools.mjs';

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
  CHANNEL: 'test', self: {}, linuxRunner: () => ({ warm() {} }), PAGE_TOOLS: [], uncertain, storeRefusal, firstGlue: {},
  read: async (store, key) => records.get(`${store}:${key}`),
  write: async (store, key, value) => records.set(`${store}:${key}`, value),
  writeDurably: async (store, key, value) => records.set(`${store}:${key}`, value),
  keysOf: async (store) => [...records.keys()].filter((name) => name.startsWith(`${store}:`)).map((name) => name.slice(store.length + 1)),
  remove: async (store, key) => records.delete(`${store}:${key}`),
  removeDurably: async (store, key) => records.delete(`${store}:${key}`),
  update: async (store, key, change) => {
    const name = `${store}:${key}`;
    const value = records.get(name);
    if (!value) return;
    const next = change(value);
    records.set(name, next);
    return next;
  },
});
vm.runInContext(`${engineSource}\nglobalThis.test = { operate, ops, running, renew, replaceFirst, creationSettings, pageTool, cancelPageTools, setClient: (value) => { client = value; poolOpening = Promise.resolve({ free: 100, rmSync() {} }); } };`, context);
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
const pageContext = vm.createContext({
  // The service worker's own: where it was loaded from, and its events (previews, unused here).
  location: new URL('chrome-extension://spindle/background.js'), self: { addEventListener() {} },
  console, setTimeout, clearTimeout, URL, crypto: globalThis.crypto, read: async () => ({ reading: true, acting: true }), PAGE_ACCESS: [], ACTING_NAMES, uncertain,
  pageSwitches: () => ({ reading: true, acting: true }),
  chrome: {
    sidePanel: { setPanelBehavior: async () => {} },
    runtime: { onInstalled: event, onMessage: event },
    permissions: { contains: async () => true, onRemoved: event },
    debugger: { onDetach: event, onEvent: event, sendCommand: async (_, method, params) => { sent.push({ method, params }); return { body: 'pong' }; } },
    tabs: { get: async (tab) => ({ id: tab, url: 'https://public.example/' }), onRemoved: event },
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
  const turns = [deferred(), deferred(), deferred()];
  let activity = 'idle';
  let accepts = true;
  engine.setClient({
    async prompt(dir, text, options) { prompts.push({ text, options }); return turns[prompts.length - 1].promise; },
    async steer() { return { queued: 1 }; },
    async status() { return { activity, accepts_prompt: accepts, stop_text: 'stopped', wake: { why: 'an unknown outcome' } }; },
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
  await assert.rejects(engine.operate('send', { id: thread, text: 'more', files: [shot] }), /already waits/);
  turns[0].resolve({ stopped: { complete: true }, unsent: ['unread steer'] });
  await settle();
  assert.equal(prompts.length, 2, 'the waiting message went as the next turn');
  assert.equal(prompts[1].text, 'unread steer\n\nlook at this\n\n[Attached, in the workspace: shot.png (2 bytes)]');
  assert.equal(prompts[1].options.files[0].path, 'shot.png');
  assert.equal(announced(), null);
  const ended = messages.filter((message) => message.type === 'turn_end' && message.thread === thread).at(-1);
  assert.equal(ended.unsent.length, 0, 'what the turn did not read went with it, not back to the box');
  // The thread needs the user when the turn ends: nothing is sent, and it is taken back.
  assert.equal((await engine.operate('send', { id: thread, text: 'again', files: [shot] })).queued, true);
  activity = 'blocked';
  turns[1].resolve({ stopped: { complete: true }, unsent: ['also unread'] });
  await settle();
  assert.equal(prompts.length, 2, 'nothing sent to a thread that needs the user first');
  assert.equal(announced().held, 'an unknown outcome');
  const back = await engine.operate('unqueue', { id: thread });
  assert.match(back.text, /^also unread\n\nagain$/);
  assert.equal(back.files[0].path, 'shot.png');
  assert.equal(announced(), null);
  assert.equal(await engine.operate('unqueue', { id: thread }), null);
  // Nor to one whose last turn did not finish (stopped by the user, say).
  activity = 'idle';
  accepts = false;
  await engine.operate('send', { id: thread, text: 'third' });
  await settle();
  assert.equal((await engine.operate('send', { id: thread, text: 'after the stop', files: [shot] })).queued, true);
  turns[2].resolve({ stopped: { complete: false, reason: 'stopped' } });
  await settle();
  assert.equal(prompts.length, 3, 'nothing sent after a turn that did not finish');
  assert.match(announced().held, /^the last turn did not finish \(stopped\): retry, continue or take it back first$/);
}

console.log('review-regressions passed: operation races, reset safety, early Stop, a message with files waiting for the turn to end, gateway bounds/redirects (the extension store told as closed), IPv6, CDP body IDs, the debugging-bar veto kept (for tabs vetoed at once too), one action per tab, uncertain actions unknown (engine, its document and the background)');
