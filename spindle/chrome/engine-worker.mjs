// The engine: every Spindle thread of this browser profile runs here, in a Dedicated
// Worker of the extension's offscreen document, as a session of the embedded package
// over OPFS. Panels ask over the channel and see every event; turns go on while no
// panel is open. The thread index (titles, times, states) is the engine's to keep.
import init, * as firstGlue from './web/spindle_embed.js';
import { EmbeddedClient, remoteRunner } from './clients/embedded.mjs';
import { linuxRunner } from './linux-runner.mjs';
import { storeRefusal } from './linux-net.mjs';
import { openOpfsPool } from './clients/opfs.mjs';
import { titleFrom } from './text.js';
import { PAGE_TOOLS, uncertain } from './page-tools.mjs';
import { CHANNEL, keysOf, read, remove, removeDurably, update, valuesOf, write, writeDurably } from './shared.mjs';
import { endedText, exchange, flowOf, refreshed } from './signin.mjs';
import * as vault from './vault.mjs';

/** Free pool slots kept before work that may add files (a thread holds a few). */
const HEADROOM = 16;
/** How long a page tool call may wait for Chrome (the package gives up at 30 s). */
const PAGE_TOOL_WAIT = 35_000;

const channel = new BroadcastChannel(CHANNEL);
const send = (message) => channel.postMessage(message);
const failure = (kind, message) => Object.assign(new Error(message), { kind });

let pool = null;
/** The pool being opened: one opening, however many ask at once. */
let poolOpening = null;
let client = null;
/** The package's glue in use (its module holds the wasm instance), the module compiled
 * once, and the instance's memory: wasm memory only grows, so `renewEngine` swaps a grown
 * instance for a fresh one while every thread rests. */
let wasm = firstGlue;
let compiled = null;
let memory = null;
let generation = 0;
/** Memory the instance may keep through a rest before it is renewed, and how long the
 * engine must have rested first. */
const RENEW_ABOVE = 64 * 1024 * 1024;
const RENEW_AFTER_MS = 30_000;
let renewal = null;
/** Set while an instance is renewed: requests wait for the fresh one. */
let renewing = null;
/** Requests being answered now (none may be when an instance is renewed). */
let answering = 0;
/** The remote runner connected (kept over a renewal: its token stays in memory, so a
 * locked vault need not be opened again). */
let remote = null;
let catalog = null;
/** Why the saved providers.json is not in use, when it is not. */
let catalogNote = null;
let broken = null;
/** The Linuxes commands run in, here in the browser; their states as they last said
 * (`wasm`: the wasm Linux's, null when the extension does not carry it). */
let linuxState = { state: 'not started', detail: null, wasm: null };
const linux = linuxRunner({
  onStatus: (state, detail, kind) => {
    linuxState = kind === 'wasm' ? { ...linuxState, wasm: { state, detail } } : { ...linuxState, state, detail };
    send({ type: 'linux', ...linuxState });
  },
  // A script in a command asks for Spindle calls: the package journals each; the
  // client runs them (models with the key, tools with their handlers).
  onCode: (session, request, signal) => client.code(session, request, { signal }),
  onRan: (session, kind) => {
    if (kind === 'v86') usedV86(idOf(session.split('#')[0]));
  },
});
/** How lately a thread must have run a command on v86 for v86 to start when it opens. */
const V86_LATELY_MS = 24 * 60 * 60_000;
/** When each thread's use of v86 was last written down (at most hourly). */
const v86Noted = new Map();

/** A thread ran a command on v86: noted on its record (`v86`, when). */
function usedV86(id) {
  const now = Date.now();
  if (!id || now - (v86Noted.get(id) ?? 0) < 60 * 60_000) return;
  v86Noted.set(id, now);
  update('threads', id, (thread) => ({ ...thread, v86: now })).catch(() => {});
}

/** The Linuxes started for `thread` ahead of its commands: the wasm Linux, and v86 only
 * for a thread that used it lately (otherwise it starts with a command that needs it,
 * and its memory stays free while a thread only talks). */
function warmFor(thread) {
  if (thread?.runner === 'app') linux.warm({ v86: Date.now() - (thread.v86 ?? 0) < V86_LATELY_MS });
}
/** The user's Spindle host, when connected (its sandbox runs `runner: remote` threads). */
let hostState = { url: null, connected: false, error: null };
/** Host changes, one after another (a slower one never undoes a later one). */
let hostChanges = Promise.resolve();
/** Threads moving into a file: they take no new turn meanwhile. */
const moving = new Set();
/** Thread id -> its running turn. */
const running = new Map();
/** Thread id -> how many operations are using its session now. */
const using = new Map();
/** Thread id -> the message (with files) sent while its turn ran: a running turn takes
 * text only, so it goes as the next turn once this one ends (`sendQueued`); `held` says
 * why it did not when it could not. */
const queued = new Map();
/** Page tool call -> the function that answers it. */
const toolCalls = new Map();
/** Short operations serialize per thread; a turn itself stays in the background. */
const lanes = new Map();
let pendingOps = 0;
let changingCatalog = false;

/** Set while space is freed (a compaction): it holds the engine's one wasm thread, so
 * every other operation waits for it, and none starts between its checks and its work. */
let maintenance = null;

async function operate(op, args) {
  while (maintenance) await maintenance;
  if (changingCatalog) throw failure('busy', 'the provider catalog is changing; try again in a moment');
  const threadIds = [args.id, ...(op === 'create' ? [args.from] : [])].filter((id) => id != null);
  for (const id of threadIds) {
    if (typeof id !== 'string' || !/^[0-9a-f]{16}$/.test(id)) throw failure('invalid', 'invalid thread id');
  }
  if (op === 'catalog') {
    if (pendingOps || running.size) throw failure('busy', 'wait for threads and pending operations before changing providers');
    changingCatalog = true;
  }
  pendingOps += 1;
  // Stopping a turn or a task waits behind no slow read of its thread.
  const stopping = op === 'stop' || op === 'taskStop';
  const keys = stopping ? [] : [...new Set(threadIds)];
  // A sign-in waits on the provider's token endpoint: in a lane of its own, so `hello`
  // and other settings are not held up behind it.
  if (!keys.length && !stopping) keys.push(op === 'signIn' ? 'signin' : 'settings');
  const work = Promise.all(keys.map((key) => lanes.get(key))).then(() => ops[op](args));
  const settled = work.catch(() => {});
  for (const key of keys) lanes.set(key, settled);
  try {
    return await work;
  } finally {
    for (const key of keys) if (lanes.get(key) === settled) lanes.delete(key);
    pendingOps -= 1;
    if (op === 'catalog') changingCatalog = false;
  }
}

const dirOf = (id) => `/threads/${id}`;
const idOf = (dir) => /^\/threads\/([0-9a-f]{16})$/.exec(dir)?.[1] ?? null;
/** The OPFS pool. Opening right after an engine stopped can find its handles not yet let
 * go; that passes in a moment. */
function openPool() {
  poolOpening ??= (async () => {
    for (let attempt = 0; ; attempt++) {
      try {
        return (pool = await openOpfsPool());
      } catch (error) {
        if (error.code !== 'EBUSY' || attempt >= 20) throw error;
        await new Promise((done) => setTimeout(done, 250));
      }
    }
  })().catch((error) => {
    poolOpening = null;
    throw error;
  });
  return poolOpening;
}

const PAGE_NAMES = new Set(PAGE_TOOLS.map((tool) => tool.name));

/** What a thread whose commands run in the browser Linux is told (app instructions):
 * the Linux, and Spindle's calls for its scripts (code mode, through bash). */
const GUIDE_START = [
  "Your commands run in a small Linux inside the user's browser (Alpine: bash, coreutils, Python 3 with pip, Node with",
  'npm, git, curl, jq, ripgrep). Each command starts afresh but for the workspace, so install packages into it',
  '(`npm install`, `python3 -m venv .venv`). With network on (the bash tool says whether), commands reach websites over',
  'HTTP and HTTPS, which Chrome fetches through the proxy their environment names (without the user\'s cookies; the',
  "user's computer and local network addresses and names are refused); if the user set a relay, they reach any address through it. Without",
  "network, connections are refused: ask the user to turn the thread's network on (its model menu) rather than",
  'working around it. The Linux is emulated and slow: give a command that runs long, or makes Spindle calls, a',
  'timeout of a few minutes.',
  'Scripts there can ask Spindle for calls, each journaled so a call that was cut off is never sent twice:',
  "from bash, `spindle model PROMPT` prints a model's answer, `spindle tool NAME '{\"json\": \"args\"}'` calls a tool",
  '(such as read_page or list_tabs), `spindle search` lists the tools, `spindle http URL` calls a site the user',
  'allowed (any other is refused, naming those allowed); in Python, `import spindle` gives',
  "spindle.model(prompt)['text'], spindle.models([prompts]) (run together), spindle.tool(name, args), spindle.http(url)",
  "and spindle.batch(calls); in Node, require('spindle') or `import ... from 'spindle'` gives the same, synchronously,",
  'and `node file.ts` runs TypeScript (types are stripped, not checked).',
  'Prefer one script that loops, filters and aggregates (asking models or tools as it goes) and prints only what you',
  'need over many separate tool calls.',
  'What you make for the user (pages, apps, reports, pictures) goes in the workspace as files: each shows in the',
];
const LINUX_GUIDE = [
  ...GUIDE_START,
  'conversation, where the user reads, opens and downloads it; with page tools on, navigate to /workspace/NAME opens',
  'one in a tab for them.',
].join(' ');

/** Earlier wordings of the guide, which threads made before may hold. */
const EARLIER_GUIDES = [[
  ...GUIDE_START,
  'conversation, where the user reads, opens and downloads it; navigate to /workspace/NAME opens one in a tab for them.',
].join(' '), [
  "Your commands run in a small Linux inside the user's browser (Alpine: bash, coreutils, Python 3 with pip, Node with",
  'npm, git, curl, jq, ripgrep). Each command starts afresh but for the workspace, so install packages into it',
  '(`npm install`, `python3 -m venv .venv`). With network on (the bash tool says whether), commands reach websites over',
  'HTTP and HTTPS, which Chrome fetches through the proxy their environment names (without the user\'s cookies; the',
  "user's computer and local network addresses and names are refused); if the user set a relay, they reach any address through it.",
  'Scripts there can ask Spindle for calls, each journaled so a call that was cut off is never sent twice:',
  "from bash, `spindle model PROMPT` prints a model's answer, `spindle tool NAME '{\"json\": \"args\"}'` calls a tool",
  '(such as read_page or list_tabs), `spindle search` lists the tools, `spindle http URL` calls a site the user',
  'allowed (any other is refused, naming those allowed); in Python, `import spindle` gives',
  "spindle.model(prompt)['text'], spindle.models([prompts]) (run together), spindle.tool(name, args), spindle.http(url)",
  "and spindle.batch(calls); in Node, require('spindle') or `import ... from 'spindle'` gives the same, synchronously,",
  'and `node file.ts` runs TypeScript (types are stripped, not checked).',
  'Prefer one script that loops, filters and aggregates (asking models or tools as it goes) and prints only what you',
  'need over many separate tool calls.',
  'The user opens what you make from the workspace (a web page or picture in a browser tab) and downloads it: make',
  'pages, apps and reports as files there.',
].join(' '), [
  "Your commands run in a small Linux inside the user's browser (Alpine: bash, coreutils, Python 3, Node, git, jq,",
  'ripgrep; no network, so nothing can be installed).',
  'Scripts there can ask Spindle for calls, each journaled so a call that was cut off is never sent twice:',
  "from bash, `spindle model PROMPT` prints a model's answer, `spindle tool NAME '{\"json\": \"args\"}'` calls a tool",
  '(such as read_page or list_tabs), `spindle search` lists the tools, `spindle http URL` calls a site the user',
  'allowed (any other is refused, naming those allowed); in Python, `import spindle` gives',
  "spindle.model(prompt)['text'], spindle.models([prompts]) (run together), spindle.tool(name, args), spindle.http(url)",
  "and spindle.batch(calls); in Node, require('spindle') or `import ... from 'spindle'` gives the same, synchronously,",
  'and `node file.ts` runs TypeScript (types are stripped, not checked).',
  'Prefer one script that loops, filters and aggregates (asking models or tools as it goes) and prints only what you',
  'need over many separate tool calls.',
  'The user opens what you make from the workspace (a web page or picture in a browser tab) and downloads it: make',
  'pages, apps and reports as files there.',
].join(' '), [
  "Your commands run in a small Linux inside the user's browser (Alpine: bash, coreutils, Python 3, Node, git, jq,",
  'ripgrep; no network, so nothing can be installed).',
  'Scripts there can ask Spindle for calls, each journaled so a call that was cut off is never sent twice:',
  "from bash, `spindle model PROMPT` prints a model's answer, `spindle tool NAME '{\"json\": \"args\"}'` calls a tool",
  '(such as read_page or list_tabs), `spindle search` lists the tools, `spindle http URL` calls a site the user',
  'allowed (any other is refused, naming those allowed); in Python, `import spindle` gives',
  "spindle.model(prompt)['text'], spindle.models([prompts]) (run together), spindle.tool(name, args), spindle.http(url)",
  "and spindle.batch(calls); in Node, require('spindle') or `import ... from 'spindle'` gives the same, synchronously,",
  'and `node file.ts` runs TypeScript (types are stripped, not checked).',
  'Prefer one script that loops, filters and aggregates (asking models or tools as it goes) and prints only what you',
  'need over many separate tool calls.',
].join(' '), [
  "Your commands run in a small Linux inside the user's browser (Alpine: bash, coreutils, Python 3, Node, git, jq,",
  'ripgrep; no network, so nothing can be installed).',
  'Scripts there can ask Spindle for calls, each journaled so a call that was cut off is never sent twice:',
  "from bash, `spindle model PROMPT` prints a model's answer, `spindle tool NAME '{\"json\": \"args\"}'` calls a tool",
  '(such as read_page or list_tabs), `spindle search` lists the tools; in Python, `import spindle` gives',
  "spindle.model(prompt)['text'], spindle.models([prompts]) (run together), spindle.tool(name, args) and spindle.batch(calls);",
  "in Node, require('spindle') gives the same, synchronously.",
  'Prefer one script that loops, filters and aggregates (asking models or tools as it goes) and prints only what you',
  'need over many separate tool calls.',
].join(' ')];
// Every guide as it read before its network sentence named precisely what is refused.
const NETWORK_NOW = "cookies; the user's computer and local network addresses and names are refused);";
const NETWORK_BEFORE = "cookies; nothing on the user's computer or local network);";
EARLIER_GUIDES.push(...[LINUX_GUIDE, ...EARLIER_GUIDES]
  .filter((guide) => guide.includes(NETWORK_NOW))
  .map((guide) => guide.replace(NETWORK_NOW, NETWORK_BEFORE)));

/** The user's standing instructions in a thread's, between these lines. */
const STANDING_START = "[The user's standing instructions]";
const STANDING_END = "[End of the user's standing instructions]";
const STANDING = /\[The user's standing instructions\]\n([\s\S]*?)\n\[End of the user's standing instructions\]/;

/** What every thread is told about what it writes, before the user's standing
 * instructions (which win). A later wording must still take this one out. */
const STYLE = "Pages you make follow the system's light or dark theme and offer a System/Light/Dark switch; never write em dashes.";

/** A thread's instructions as they are, with the style line, the Linux guide in (`on`)
 * or out and the user's standing instructions as `standing` says (undefined: as they
 * are). */
function guided(instructions, on, standing) {
  let rest = String(instructions ?? '');
  for (const guide of [LINUX_GUIDE, ...EARLIER_GUIDES]) rest = rest.replace(guide, '');
  const kept = STANDING.exec(rest)?.[1] ?? '';
  rest = rest.replace(STANDING, '').replace(STYLE, '').trim();
  const user = (standing ?? kept).trim();
  return [rest, STYLE, user && `${STANDING_START}\n${user}\n${STANDING_END}`, on && LINUX_GUIDE].filter(Boolean).join('\n\n');
}

/** Before a turn, a thread gets what is current, if it differs (if it cannot, the turn
 * goes on as it was): the guide's wording (for a thread made by an earlier build), the
 * user's standing instructions, the page tools as they are now (a thread offered some
 * has all), and, once, summaries its model writes when its history is compacted (a
 * thread made here by an earlier build compacted by digest: Chrome could do no better
 * then; threads made or imported since are marked `summaries` and keep their own). */
async function freshen(dir) {
  try {
    const id = dir.slice(dirOf('').length);
    const settings = await client.settings(dir);
    const instructions = settings.instructions ?? '';
    const tools = settings.tools ?? [];
    const changes = {};
    const guide = instructions.includes(LINUX_GUIDE) || EARLIER_GUIDES.some((earlier) => instructions.includes(earlier));
    const wanted = guided(instructions, guide, (await read('kv', 'standing')) ?? '');
    if (wanted !== instructions.trim()) changes.instructions = wanted;
    const offered = tools.filter((tool) => PAGE_NAMES.has(tool.name));
    const now = client.tools.definitions();
    const current = offered.length === now.length && now.every((tool) => offered.some((had) => had.name === tool.name && had.description === tool.description));
    if (offered.length && !current) changes.tools = [...tools.filter((tool) => !PAGE_NAMES.has(tool.name)), ...now];
    const summarized = (await read('threads', id))?.summaries;
    if (settings.compaction === 'digest' && !summarized) changes.compaction = 'model';
    if (Object.keys(changes).length) await client.settings(dir, changes);
    if (!summarized) await touch(id, { summaries: true });
  } catch (error) {
    console.warn('Spindle: a thread was not brought up to date before its turn:', error);
  }
}

/** What a session from a native host must change before it can run here, and what to
 * tell the user: the package's own answer (`fit`, which an import carries), each of its
 * notes said in the panel's words; `runner` is where its commands run after. */
function fitForChrome(fit, runner) {
  const said = [];
  for (const { setting, to } of fit.notes ?? []) {
    if (setting === 'runner' && to === 'app') said.push("its commands run in this browser's Linux");
    if (setting === 'network' && runner === 'app') said.push("commands keep their network (websites through Chrome, or your relay if you set one; Chrome asks for access to sites when you next send)");
    if (setting === 'network' && runner === 'remote') said.push('commands keep their network (on your Spindle host)');
    if (setting === 'toolchain') said.push('the Rust toolchain off');
    if (setting === 'sources') said.push('host folders unmounted');
    if (setting === 'max_usd') said.push(`its US$${to} spending limit kept (counted here as on the host)`);
  }
  for (const { why } of fit.refused ?? []) said.push(`not here: ${why}`);
  return { changes: fit.changes ?? {}, said };
}

/** Files the user attached ({path, bytes}), as a prompt takes them, and the line that
 * tells the thread they are there. */
function attachments(files) {
  if (!files?.length) return { files: null, note: '' };
  const encode = (bytes) => {
    let binary = '';
    for (let at = 0; at < bytes.length; at += 0x8000) binary += String.fromCharCode(...bytes.subarray(at, at + 0x8000));
    return btoa(binary);
  };
  return {
    files: files.map((file) => ({ path: String(file.path), base64: encode(file.bytes) })),
    note: `\n\n[Attached, in the workspace: ${files.map((file) => `${file.path} (${file.bytes.length} bytes)`).join(', ')}]`,
  };
}

/** Whether a thread's first message started it with another thread's files. */
const STARTED_WITH = /\n\n\[Started with the files of “.*”: \d+ in the workspace\]$/s;

/** A message as the user wrote it, and the files it says were attached (`attachments`'
 * line, taken off). */
function unattached(text) {
  const found = /\n\n\[Attached, in the workspace: (.*)\]$/s.exec(text);
  if (!found) return { text, paths: [] };
  return { text: text.slice(0, found.index), paths: [...found[1].matchAll(/(.+?) \(\d+ bytes\)(?:, |$)/g)].map((match) => match[1]) };
}

/** A thread's effort as its status says (older builds' threads: the default). */
const effortOf = (status) => status.effort ?? status.settings?.effort ?? 'default';

/** Changes a thread's settings (its session open) and its index entry. */
async function changeSettings(id, changes) {
  // Switching where commands run changes what the thread is told about them (the Linux
  // guide, only where it has bash there).
  const now = changes.runner ? await client.settings(dirOf(id)) : null;
  const inLinux = changes.runner === 'app';
  const settings = await client.settings(dirOf(id), now
    ? { ...changes, instructions: guided(now.instructions, inLinux && now.builtin_tools === 'files') }
    : changes);
  const status = await client.status(dirOf(id));
  await touch(id, {
    provider: status.provider, model: status.model, effort: effortOf(status), runner: settings.runner ?? 'none',
    network: Boolean(settings.network), tasks: Boolean(settings.tasks),
  });
  if (changes.runner === 'app') linux.warm();
  return settings;
}

function newId() {
  return [...crypto.getRandomValues(new Uint8Array(8))].map((byte) => byte.toString(16).padStart(2, '0')).join('');
}

function makeClient(saved) {
  const made = new EmbeddedClient({
    wasm, storage: 'opfs', pool: openPool, catalog: saved, credentials, fetch: reach, runner: { app: linux },
  });
  for (const tool of PAGE_TOOLS) {
    made.tool({ ...tool, timeoutS: 30, run: (input, call) => pageTool(tool.name, opened(tool.name, input, call?.session), call?.session) });
  }
  made.on('event', (event, dir) => {
    const id = idOf(dir);
    if (!id) return;
    if (event?.type === 'task') noteTask(id, event);
    send({ type: 'event', thread: id, event });
  });
  for (const name of ['work_error', 'app_tool_error']) made.on(name, (error) => console.error(`Spindle ${name}:`, error));
  return made;
}

/** What each thread's tasks were seen doing while this engine ran: by thread, by task
 * (its short name), `{started, seen, ended, steps, now, asked}`: when it was first and
 * last seen at work and when it ended (ms), how many steps it took, what it is doing now
 * (`{name, input}` for a step, `{model}` while its model writes; null between), and the
 * first line of what it was asked, at most 80 characters (as `tasks` names one). A task
 * the engine took up again after a restart counts from then (and, its first prompt not
 * seen, has no `asked`). */
const taskWork = new Map();
/** The input a task's step is named by, short (a step may write a whole file). */
const STEP_KEYS = ['command', 'path', 'url', 'tab', 'ref', 'key', 'text', 'action', 'new_tab', 'prompt', 'background'];

function noteTask(id, { task, event }) {
  if (typeof task !== 'string' || !task || !event) return;
  let tasks = taskWork.get(id);
  if (!tasks) taskWork.set(id, (tasks = new Map()));
  let work = tasks.get(task);
  if (!work) tasks.set(task, (work = { started: Date.now(), seen: Date.now(), ended: null, steps: 0, now: null }));
  work.seen = Date.now();
  // Started again after it ended: it works again.
  if (['prompt', 'model_step', 'tool_call'].includes(event.type)) work.ended = null;
  if (event.type === 'prompt' && typeof event.text === 'string') {
    work.asked ??= [...(event.text.split(/\r?\n/).find((line) => line.trim()) ?? '').trim()].slice(0, 80).join('');
  }
  if (event.type === 'tool_call') {
    const input = {};
    for (const key of STEP_KEYS) {
      const value = event.input?.[key];
      if (value != null) input[key] = typeof value === 'string' ? value.slice(0, 300) : value;
    }
    work.steps += 1;
    work.now = { name: String(event.name ?? ''), input };
  } else if (event.type === 'model_step') {
    work.now = { model: String(event.model ?? '') };
  } else if (event.type === 'tool_result' || event.type === 'tool_refused') {
    work.now = null;
  } else if (event.type === 'stopped') {
    work.ended ??= Date.now();
    work.now = null;
  }
}

/** After a thread went back: the tasks it ran in the foreground (no row) this engine saw
 * are let go (their calls may be gone with what came after); those with a row stay. */
async function forgetUnlisted(id) {
  const seen = taskWork.get(id);
  if (!seen) return;
  try {
    const rows = new Set(((await client.tasks(dirOf(id))).tasks ?? []).map((task) => String(task.task).split('#').pop()));
    for (const name of [...seen.keys()]) if (!rows.has(name)) seen.delete(name);
  } catch {}
}

/** The client, on the provider list kept here (the built-in one, saying why, when the
 * kept one cannot be used), with the sites scripts may reach. */
async function newClient() {
  const saved = (await read('kv', 'catalog')) ?? undefined;
  try {
    client = makeClient(saved);
    catalogNote = null;
  } catch (error) {
    client = makeClient(undefined);
    catalogNote = `your providers.json is not in use: ${error.message}`;
  }
  catalog = client.catalog();
  await client.ready;
  try {
    client.setHttpGrants((await read('kv', 'sites')) ?? []);
  } catch (error) {
    console.error('Spindle: the sites scripts may reach are not in use:', error);
  }
}

async function start() {
  await newClient();
  // The host connects meanwhile: one that does not answer holds up nothing here.
  connectKeptHost().catch(() => {});
  linux.setRelay((await read('kv', 'relay')) ?? null);
}

/** Connects the host kept here (kv/host's URL, its token in the vault), if there is one.
 * A token an earlier build kept as text beside the URL is sealed in the vault now, or
 * once the vault is unlocked; while a passphrase vault is locked no host is connected
 * (as locking lets it go), so the host waits (disconnected, saying so) until then. */
async function connectKeptHost() {
  const host = await read('kv', 'host');
  if (!host) return;
  const locked = () => disconnectHost(host.url, "Spindle's keys are locked: unlock them to connect it");
  let token = host.token;
  if (token != null) {
    if (!(await vault.usableNow())) return locked();
    await vault.keepHost(token, [['kv', 'host', { url: host.url }]]);
  } else {
    try {
      token = await vault.hostToken();
    } catch (error) {
      if (error?.kind === 'locked') return locked();
      return disconnectHost(host.url, `its token cannot be read (${error?.message ?? error}): connect it again`);
    }
  }
  if (token == null) {
    await disconnectHost(host.url, 'its token is gone (the keys were reset): connect it again');
    return;
  }
  hostState = { url: host.url, connected: false, error: null };
  await changeHost({ url: host.url, token });
}

/** The remote runner let go, the host's URL kept and `why` told. */
function disconnectHost(url, why) {
  return changeHost(null).finally(() => {
    hostState = { url, connected: false, error: why };
    send({ type: 'host', host: hostState });
  });
}

/** Connects the user's Spindle host (`{url, token}`; null disconnects) as the remote
 * runner, after any change asked before it. A host that does not answer leaves the one
 * connected before as it was. Panels hear of the outcome. */
function changeHost(host) {
  const change = hostChanges.then(async () => {
    try {
      const runner = host ? remoteRunner({ url: host.url, token: host.token, fetch: reach }) : null;
      await client.connectRunner('remote', runner);
      remote = runner;
      hostState = { url: host?.url ?? null, connected: Boolean(host), error: null };
    } catch (error) {
      hostState = { ...hostState, error: `${host.url}: ${error.message ?? error}` };
      throw error;
    } finally {
      send({ type: 'host', host: hostState });
    }
  });
  hostChanges = change.catch(() => {});
  return change;
}

/** A request's key, from the vault (a sign-in's token, refreshed first when it is about
 * to expire); the package puts it where the provider's catalog entry says (`auth`). */
async function credentials(facts) {
  const profile = catalog.providers.find((p) => p.id === facts.provider);
  const name = profile?.display_name ?? facts.provider;
  const again = (held) => (held.kind === 'signin' ? 'sign in again' : 'save the key again');
  // A credential goes only where it was saved for: a provider list imported later that
  // points this provider (or its sign-in's token endpoint) elsewhere does not get it.
  const moved = (held, was, now) => new Error(
    `${name}'s address changed since its ${held.kind === 'signin' ? 'sign-in' : 'key'} was saved (${was} -> ${now ?? 'none'}): ${again(held)} to use it there`,
  );
  const api = originOf(profile?.base_url);
  const held = await vault.secret(facts.provider, (secret) => {
    const token = originOf(flowOf(profile)?.token_url);
    if (secret.kind === 'signin' && secret.tokenOrigin && secret.tokenOrigin !== token) throw moved(secret, secret.tokenOrigin, token);
    if (secret.origin && secret.origin !== api) throw moved(secret, secret.origin, api);
    return refreshed(profile, secret, reach);
  });
  if (held?.origin && held.origin !== api) throw moved(held, held.origin, api);
  // Saved before credentials named their origin: bound to the one in use now.
  if (held && !held.origin && api) {
    await vault.keep(facts.provider, {
      ...held, origin: api, ...(held.kind === 'signin' && { tokenOrigin: originOf(flowOf(profile)?.token_url) }),
    });
  }
  if (held?.kind === 'key') return { key: held.key };
  if (held?.kind === 'signin' && held.ended) {
    announceVault();
    throw new Error(endedText(profile));
  }
  if (held?.kind === 'signin') return { key: held.access };
  if (profile?.requires_api_key === false) return {};
  const missing = flowOf(profile) ? 'sign in' : 'add its key';
  throw new Error(`Spindle has no credential for ${profile?.display_name ?? facts.provider}: ${missing} in Settings, Providers`);
}

/** A URL's origin, or null. */
function originOf(url) {
  try {
    return new URL(url).origin;
  } catch {
    return null;
  }
}

/** fetch, saying what to do when Chrome refuses a provider the extension may not reach. */
async function reach(url, init) {
  try {
    return await fetch(url, init);
  } catch (error) {
    if (init?.signal?.aborted) throw error;
    const store = storeRefusal(url);
    if (store) throw new Error(store);
    const { origin } = new URL(url);
    throw new Error(
      `the request to ${origin} did not go through (${error.message}); if Spindle may not reach ${origin} yet, ` +
        'send from the panel to allow it',
    );
  }
}

/** navigate to a file of the thread's workspace (`/workspace/NAME`, as its commands see
 * it, or file:///workspace/NAME) opens it in a tab for the user, as Files does. */
function opened(name, input, session) {
  const id = idOf(session ?? '');
  const named = name === 'navigate' && typeof input?.url === 'string' && /^(?:file:\/\/)?\/workspace\/(.+)$/.exec(input.url.trim());
  if (!id || !named) return input;
  let path;
  try {
    path = decodeURIComponent(named[1]);
  } catch {
    path = named[1];
  }
  const view = new URL('view.html', self.location.href);
  view.search = new URLSearchParams({ thread: id, path }).toString();
  return { ...input, url: view.href, new_tab: true };
}

function pageTool(name, input, session) {
  if (running.get(idOf(session?.split('#')[0] ?? ''))?.stopped) return Promise.resolve({ error: 'the turn was stopped' });
  const call = crypto.randomUUID();
  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      self.postMessage({ type: 'page_tool_cancel', call });
      answerTool(call, uncertain(name, 'Chrome did not answer the page tool in time'));
    }, PAGE_TOOL_WAIT);
    toolCalls.set(call, Object.assign((answer) => {
      clearTimeout(timer);
      resolve(answer);
    }, { session, tool: name }));
    self.postMessage({ type: 'page_tool', call, name, input });
  });
}

function cancelPageTools(id) {
  for (const [call, answer] of toolCalls) {
    if (idOf(answer.session?.split('#')[0] ?? '') !== id) continue;
    self.postMessage({ type: 'page_tool_cancel', call });
    answerTool(call, uncertain(answer.tool, 'the turn ended before the page tool finished'));
  }
}

function answerTool(call, answer) {
  toolCalls.get(call)?.(answer);
  toolCalls.delete(call);
}

self.onmessage = ({ data }) => {
  if (data?.type === 'page_tool_answer') answerTool(data.call, data.answer);
};

let makingHeadroom = Promise.resolve();

function headroom() {
  const growing = makingHeadroom.then(async () => {
    const fs = await openPool();
    if (fs.free < HEADROOM) await fs.grow(HEADROOM * 2);
  });
  makingHeadroom = growing.catch(() => {});
  return growing;
}

async function threads() {
  return (await valuesOf('threads')).sort((a, b) => b.updated - a.updated);
}

/** Changes a thread's index entry (in one transaction) and tells the panels. */
async function touch(id, changes) {
  // A moved thread stays moved (a turn that ended after the move cannot undo it).
  const next = await update('threads', id, (thread) => ({
    ...thread, ...changes, ...(thread.state === 'moved' && { state: 'moved', note: thread.note }), updated: Date.now(),
  }));
  if (next) send({ type: 'thread', thread: next });
  return next ?? null;
}

/** Runs `work` on a thread's session, which closes again once nothing uses it (an open
 * session holds memory a wasm engine never gives back). */
async function within(id, work) {
  using.set(id, (using.get(id) ?? 0) + 1);
  try {
    return await work();
  } finally {
    const left = using.get(id) - 1;
    if (left) using.set(id, left);
    else using.delete(id);
    rest(id);
  }
}

/** How long an idle thread's session stays open: reads one after another (a page's
 * preview reading what it refers to, say) share one opening, not one each. */
const REST_AFTER_MS = 2_000;
const resting = new Map();

/** Closes an idle thread's session once it has rested a moment; the package refuses to
 * close a running one. */
function rest(id) {
  clearTimeout(resting.get(id));
  const timer = setTimeout(() => {
    resting.delete(id);
    if (running.has(id) || using.has(id)) return;
    client.closeSession(dirOf(id)).then(() => {
      if (!running.size && !using.size) renewEngineSoon();
    }, () => {});
  }, REST_AFTER_MS);
  timer?.unref?.();
  resting.set(id, timer);
}

/** Once the engine has rested a while, a grown instance is renewed (`renewEngine`). */
function renewEngineSoon() {
  clearTimeout(renewal);
  renewal = setTimeout(() => {
    renewEngine().catch((error) => console.warn('Spindle: the engine kept its memory:', error));
  }, RENEW_AFTER_MS);
  renewal?.unref?.();
}

/** Gives a grown wasm instance's memory back to the browser: while no thread runs or
 * is open (nor any task in the background), the client closes, a fresh copy of the
 * glue instantiates the module compiled at start, and a new client takes over with
 * the same catalog, sites and runners. Sessions are on OPFS, so nothing is lost; the
 * old instance (and every byte it grew to) goes with the old glue. */
async function renewEngine() {
  if (renewing || !compiled || !memory || memory.buffer.byteLength <= RENEW_ABOVE) return;
  if (running.size || using.size || answering) return;
  renewing = (async () => {
    // A host change under way finishes on the client it began with.
    await hostChanges;
    if ((await client.call('list')).sessions.length || running.size || using.size || answering) return;
    const was = memory.buffer.byteLength;
    await client.close();
    pool?.close();
    pool = null;
    poolOpening = null;
    // The old instance is let go only once a fresh one runs; otherwise the next
    // client is made on it again, and the engine goes on as it was.
    try {
      const glue = await import(`./web/spindle_embed.js?instance=${++generation}`);
      const fresh = (await glue.default({ module_or_path: compiled })).memory;
      const old = wasm;
      wasm = glue;
      memory = fresh;
      old.__spindleDrop?.();
    } catch (error) {
      console.warn('Spindle: no fresh engine instance; the one there was goes on:', error);
    }
    try {
      await newClient();
    } catch (error) {
      broken = `the engine could not start again: ${error.message ?? error}`;
      throw error;
    }
    // The host connected before is connected again; one that does not answer now is
    // let go, saying so (connecting it again from Settings takes it back).
    if (remote) {
      try {
        await client.connectRunner('remote', remote);
      } catch (error) {
        remote = null;
        hostState = { ...hostState, connected: false, error: `${hostState.url}: ${error.message ?? error}` };
        send({ type: 'host', host: hostState });
      }
    }
    console.info(`Spindle: the engine gave back ${Math.round((was - memory.buffer.byteLength) / 1048576)} MiB`);
  })();
  try {
    await renewing;
  } finally {
    renewing = null;
  }
}

/** Runs one turn of a thread in the background; panels follow it by its events. */
function turn(id, work) {
  if (running.has(id)) throw failure('busy', 'this thread is already running');
  if (moving.has(id)) throw failure('busy', 'this thread is moving into a file');
  const entry = { stopped: false };
  running.set(id, entry);
  entry.done = (async () => {
    let unsent = [];
    try {
      const thread = await touch(id, { state: 'running', note: null });
      // Its commands run on the user's host: once that host has connected (or failed to).
      if (thread?.runner === 'remote') await hostChanges;
      if (entry.stopped) throw failure('stopped', 'the turn was stopped before it started');
      const result = await work(() => {
        if (entry.stopped) throw failure('stopped', 'the turn was stopped before it started');
      });
      const cut = result.stopped && !result.stopped.complete;
      unsent = result.unsent ?? [];
      const note = cut
        ? result.stopped.reason
        : unsent.length ? `${unsent.length} message(s) came after the turn's last step and were not read` : null;
      await touch(id, { state: cut ? 'stopped' : 'idle', note });
    } catch (error) {
      await touch(id, { state: error.kind === 'stopped' ? 'stopped' : 'failed', note: String(error.message ?? error) });
    } finally {
      running.delete(id);
      cancelPageTools(id);
      // Messages the turn did not read go with a message waiting for its end, before it.
      const waiting = queued.get(id);
      if (waiting && !waiting.held && unsent.length) {
        waiting.text = [...unsent, waiting.text].join('\n\n');
        unsent = [];
      }
      send({ type: 'turn_end', thread: id, unsent });
      if (waiting && !waiting.held) sendQueued(id);
      else rest(id);
    }
  })();
}

/** What panels show of a thread's waiting message (null: none). */
function queuedOf(id) {
  const waiting = queued.get(id);
  return waiting ? { text: waiting.text, names: waiting.files.map((file) => String(file.path)), held: waiting.held ?? null } : null;
}

function announceQueued(id) {
  send({ type: 'queued', thread: id, queued: queuedOf(id) });
}

/** Sends the message that waited for a thread's turn to end, as its next turn. One the
 * thread cannot take now (it needs the user first, say) is held, saying why, for the
 * user to take back into the box. */
async function sendQueued(id) {
  const waiting = queued.get(id);
  if (!waiting) return;
  try {
    await workable(id);
    await headroom();
    const status = await client.status(dirOf(id));
    // Another turn started meanwhile (another panel's): the message waits for its end.
    if (running.has(id) || queued.get(id) !== waiting) return;
    if (status.activity === 'blocked') {
      throw failure('blocked', status.wake?.why ?? 'the thread needs you to look at it first');
    }
    // A turn that did not finish (stopped, say) takes no new message until it is retried,
    // continued or taken back.
    if (status.accepts_prompt === false) {
      throw failure('busy', `the last turn did not finish${status.stop_text ? ` (${status.stop_text})` : ''}: retry, continue or take it back first`);
    }
    const { files, note } = attachments(waiting.files);
    turn(id, async (check) => {
      await freshen(dirOf(id));
      check();
      return client.prompt(dirOf(id), waiting.text + note, files ? { files } : {});
    });
    // Out of the queue once its turn has started (until then a failure holds it).
    queued.delete(id);
    announceQueued(id);
  } catch (error) {
    if (queued.get(id) !== waiting) return;
    waiting.held = String(error.message ?? error);
    announceQueued(id);
    rest(id);
  }
}

function notRunning(id, doing) {
  if (running.has(id)) throw failure('busy', `stop this thread before ${doing} it`);
}

/** A thread that can take work: it exists and has not moved out. */
async function workable(id) {
  const thread = await read('threads', id);
  if (!thread) throw failure('not_found', 'there is no such thread');
  if (thread.state === 'moved') throw failure('moved', 'this thread moved into a bundle file; import the file to go on with it');
  if (moving.has(id)) throw failure('busy', 'this thread is moving into a file');
  return thread;
}

/** Removes a thread's files (after a failed create or import, or a delete). */
async function discard(id) {
  taskWork.delete(id);
  // Its background tasks end with it (a session that has none, or cannot say, closes
  // as it is).
  try {
    for (const task of (await client.tasks(dirOf(id))).tasks) {
      if (task.pending) await client.taskStop(dirOf(id), task.task);
    }
  } catch {}
  await client.closeSession(dirOf(id));
  (await openPool()).rmSync(dirOf(id), { recursive: true, force: true });
}

// ---- Storage (Settings > Storage) ---------------------------------------------------

/** A store's own file, at a thread's top or a task's (`tasks/<key>/`, nested or not). */
const STORE_FILE = /^(?:tasks\/[0-9a-f]{20}\/)*store\/authority\.sqlite$/;
/** A first attempt kept aside (a first request tried again in a fresh store). */
const ASIDE_DIR = /^((?:tasks\/[0-9a-f]{20}\/)*store\.abandoned-\d+)\//;

/** Which part of a thread a file is, by its path under the thread's directory. */
function partOf(rest) {
  if (ASIDE_DIR.test(rest)) return 'aside';
  if (rest.startsWith('tasks/')) return 'tasks';
  if (rest === 'store/authority.sqlite') return 'database';
  if (rest.startsWith('store/authority.sqlite-')) return 'log';
  return 'other';
}

/** The free pages in the store file `path`, in bytes, from its SQLite header (current
 * between sessions: a session's log is taken into the file as it closes). */
function freeIn(fs, path) {
  const fd = fs.openSync(path, 'r');
  const head = new Uint8Array(100);
  if (fs.readSync(fd, head, 0, 100, 0) < 100) return 0;
  if (new TextDecoder().decode(head.subarray(0, 16)) !== 'SQLite format 3\0') return 0;
  const view = new DataView(head.buffer);
  const page = view.getUint16(16) === 1 ? 65536 : view.getUint16(16);
  return view.getUint32(36) * page;
}

/** Removes what a moved thread left besides its fence (the file saying where it went,
 * which startup reads and every open refuses by): its sealed stores, its tasks' and
 * its set-aside ones. The bytes freed. */
function dropSealed(fs, id) {
  const prefix = `${dirOf(id)}/`;
  const fence = `${prefix}store/authority.sqlite.lock`;
  let freed = 0;
  for (const { path, size } of fs.files()) {
    if (!path.startsWith(prefix) || path === fence) continue;
    fs.unlinkSync(path);
    freed += size;
  }
  return freed;
}

/** Whether `a` and `b` hold the same bytes (by SHA-256: one pass each). */
async function sameBytes(a, b) {
  if (a.byteLength !== b.byteLength) return false;
  const [x, y] = await Promise.all([a, b].map((bytes) => crypto.subtle.digest('SHA-256', bytes)));
  return new Uint8Array(x).every((byte, at) => byte === new Uint8Array(y)[at]);
}

/** The size of a moved thread's file (kept with it since it moved; older records are
 * read once). */
const bundleSizes = new Map();
async function bundleSize(id) {
  if (!bundleSizes.has(id)) {
    const kept = await read('bundles', id);
    bundleSizes.set(id, kept?.size ?? kept?.bytes?.byteLength ?? 0);
  }
  return bundleSizes.get(id);
}

function slug(title) {
  return (title || 'thread').replace(/[^\p{L}\p{N}]+/gu, '-').replace(/^-+|-+$/g, '').slice(0, 60) || 'thread';
}

async function snapshot() {
  return {
    info: await client.info(),
    catalog,
    catalogSaved: Boolean(await read('kv', 'catalog')),
    catalogNote,
    threads: await threads(),
    running: [...running.keys()],
    free: pool?.free ?? null,
    linux: linuxState,
    host: hostState,
    sites: (await read('kv', 'sites')) ?? [],
    standing: (await read('kv', 'standing')) ?? '',
    relay: (await read('kv', 'relay')) ?? null,
    vault: await vaultState(),
  };
}

/** The vault as panels see it; one that cannot be read still lets Settings open. */
function vaultState() {
  return vault.state().catch((error) => ({
    mode: 'device', locked: false, kinds: {}, accounts: {}, ended: {}, error: String(error?.message ?? error),
  }));
}

/** Tells every panel the vault changed (another window's panel keeps up). */
function announceVault() {
  vaultState().then((state) => send({ type: 'vault', vault: state }));
}

/** The last turnFiles answer, by thread and revision (asked again at every turn's end). */
let turnFilesKept = { key: null, value: null };

/** The rewind target whose history is `shown`'s first `at` messages: the point just
 * before message `at`. Newest first, among the last 8192 steps (each read only to its
 * count first; the whole history of one with the right count). */
async function pointBefore(dir, shown, at) {
  const before = JSON.stringify(shown.slice(0, at));
  for (const target of await client.targets(dir, { limit: 8192 })) {
    const { messages = [] } = await client.history(dir, target.revision, { from: at - 1 });
    if (messages.length !== 1) continue;
    if (JSON.stringify((await client.history(dir, target.revision)).messages ?? []) === before) return target.revision;
  }
  return null;
}

/** A thread with nothing yet, as it was made (after taking back its first message). */
async function renew(id) {
  const settings = await client.settings(dirOf(id));
  const status = await client.status(dirOf(id));
  if (status.unresolved) throw failure('conflict', 'a request may have reached the provider: Retry it, or delete the thread');
  await replaceFirst(id, settings.provider, creationSettings(settings));
  // A thread begun afresh has none of its tasks.
  taskWork.delete(id);
}

/** Settings accepted by create, including defaults it would otherwise replace. */
function creationSettings(settings) {
  const { self_edit, ...kept } = settings;
  return { ...kept, tools: settings.tools ?? [], builtin_tools: settings.builtin_tools ?? 'all' };
}

async function checkCreation(provider, settings) {
  const probe = newId();
  await headroom();
  try {
    await client.create(dirOf(probe), provider, settings);
  } finally {
    await discard(probe);
  }
}

/** A first turn's old record survives a reset, even if the engine stops during it. */
async function replaceFirst(id, provider, settings) {
  await checkCreation(provider, settings);
  turnFilesKept = { key: null, value: null };
  // A name of its own (its time and a random part): no reset writes over another's.
  const backup = `prior:${id}:${String(Date.now()).padStart(15, '0')}:${crypto.randomUUID()}`;
  // Checked whole: if the reset fails, this is what the thread comes back from. On disk
  // (with the record naming it) before the thread's store is discarded.
  await writeDurably('bundles', backup, { bytes: await client.exportSession(dirOf(id), { keep: true, whole: true }) });
  await writeDurably('kv', `reset:${id}`, { type: 'reset', id, backup });
  try {
    await discard(id);
    await client.create(dirOf(id), provider, settings);
    // Gone from disk before the thread takes any work: found again after a crash, it
    // would bring the old thread back over what the new one did.
    await removeDurably('kv', `reset:${id}`);
  } catch (error) {
    await restoreFirst({ id, backup });
    throw error;
  }
  // Only a reset cut short comes back from its backup: this one is done, so it goes
  // (tidying only: one left behind goes when the engine next starts).
  await dropPriors(id).catch(() => {});
}

/** A thread's reset backups go (none is in use: the thread's reset, if any, is done). */
async function dropPriors(id) {
  for (const key of await keysOf('bundles')) if (key.startsWith(`prior:${id}:`)) await remove('bundles', key);
}

async function restoreFirst({ id, backup }) {
  const kept = await read('bundles', backup);
  if (!kept) throw failure('broken', `the saved record for thread ${id} is missing`);
  await discard(id);
  await client.importSession(dirOf(id), kept.bytes);
  await client.closeSession(dirOf(id));
  // As a finished reset's: gone from disk before the thread takes any work.
  await removeDurably('kv', `reset:${id}`);
}

/** A thread's workspace as prompt files take them ({path, base64, executable}), and its
 * title; for a thread started with them. */
async function workspaceOf(id) {
  const thread = await workable(id);
  return within(id, async () => {
    const dir = dirOf(id);
    const listed = await client.files(dir);
    const total = listed.files.reduce((sum, file) => sum + file.size, 0);
    if (total > MAX_CARRIED) throw failure('too_large', `its files come to ${Math.round(total / 1048576)} MiB; at most ${MAX_CARRIED / 1048576} MiB start a thread`);
    const files = [];
    for (const file of listed.files) {
      const { files: [encoded] } = attachments([{ path: file.path, bytes: await client.read(dir, file.path) }]);
      files.push({ ...encoded, ...(file.executable && { executable: true }) });
    }
    return { title: thread.title, files };
  });
}

/** The most a thread's files may come to, to start another thread with. */
const MAX_CARRIED = 256 * 1024 * 1024;

const ops = {
  hello: () => snapshot(),

  /** That the engine answers (a file's tab times its start apart from what it reads). */
  ping: () => true,

  async create({ provider, model, effort, text, runner = 'app', network = false, tasks = false, max_usd: limit = null, files: given, from }) {
    await headroom();
    // Started with another thread's files (its workspace now; no history).
    const carried = from ? await workspaceOf(from) : null;
    const id = newId();
    const dir = dirOf(id);
    let status;
    try {
      // Commands in the browser Linux (told how), on the user's host, or none at all.
      const where = ['app', 'remote', 'none'].includes(runner) ? runner : 'app';
      const instructions = guided('', where === 'app', (await read('kv', 'standing')) ?? '');
      await client.create(dir, provider, {
        ...(model && { model }), ...(effort && { effort }), runner: where, network: Boolean(network) && where !== 'none',
        ...(instructions && { instructions }), ...(tasks && { tasks: true }),
        // A spending limit set for new threads (dollars, tasks included).
        ...(Number.isFinite(limit) && limit >= 0 && { max_usd: limit }),
      });
      status = await client.status(dir);
    } catch (error) {
      await discard(id).catch(() => {});
      throw error;
    }
    const now = Date.now();
    const thread = {
      id, title: titleFrom(text) || 'New thread', provider: status.provider, model: status.model, effort: effortOf(status),
      runner: status.settings?.runner ?? 'app', network: Boolean(status.settings?.network), tasks: Boolean(status.settings?.tasks),
      created: now, updated: now, state: 'idle', note: null, summaries: true,
    };
    await write('threads', id, thread);
    send({ type: 'thread', thread });
    const { files, note } = attachments(given);
    const sent = [...(carried?.files ?? []), ...(files ?? [])];
    // Before the attachments' line, which ends a message.
    const started = carried ? `\n\n[Started with the files of “${carried.title}”: ${carried.files.length} in the workspace]` : '';
    turn(id, () => client.prompt(dir, text + started + note, sent.length ? { files: sent } : {}));
    warmFor(thread);
    return { id };
  },

  async send({ id, text, files: given }) {
    await workable(id);
    await headroom();
    // Decided after the last wait: a turn that started meanwhile is steered (a message
    // with files waits for its own turn: a running one takes text only).
    if (running.has(id)) {
      if (!given?.length) return { steered: (await client.steer(dirOf(id), text)).queued };
      if (queued.has(id)) throw failure('busy', 'a message already waits for this turn to end: take it back to change it');
      queued.set(id, { text: String(text), files: given });
      announceQueued(id);
      return { queued: true };
    }
    const { files, note } = attachments(given);
    turn(id, async (check) => {
      await freshen(dirOf(id));
      check();
      return client.prompt(dirOf(id), text + note, files ? { files } : {});
    });
    return { started: true };
  },

  /** Takes a thread's waiting message back (`{text, files}`; null when none waits). */
  unqueue({ id }) {
    const waiting = queued.get(id);
    if (!waiting) return null;
    queued.delete(id);
    announceQueued(id);
    return { text: waiting.text, files: waiting.files };
  },

  stop: ({ id }) => within(id, () => {
    const entry = running.get(id);
    if (entry) entry.stopped = true;
    cancelPageTools(id);
    return client.stop(dirOf(id));
  }),

  async retry({ id }) {
    await workable(id);
    turn(id, () => client.retry(dirOf(id)));
    return { started: true };
  },

  async resume({ id }) {
    await workable(id);
    turn(id, () => client.resume(dirOf(id)));
    return { started: true };
  },

  /** A thread and its history (`from`: only the messages from that index on; `total`
   * counts them all). */
  async open({ id, from }) {
    const dir = dirOf(id);
    const thread = await read('threads', id);
    if (!thread) throw failure('not_found', 'there is no such thread');
    if (thread.state === 'moved') {
      return { thread, status: null, history: null, running: false, bundle: Boolean(await read('bundles', id)) };
    }
    // Its commands run in the browser Linux: started (or resumed) while the user reads
    // and writes, not when the first command waits for it.
    warmFor(thread);
    return within(id, async () => {
      const status = await client.status(dir);
      // Its index entry follows the session's own settings (one made or imported before
      // the entry said whether tasks are on, say).
      // Kept in place (not as a change of the thread: its place in the list stays).
      const tasks = Boolean(status.settings?.tasks);
      let entry = thread;
      if (tasks !== Boolean(thread.tasks)) {
        entry = (await update('threads', id, (kept) => ({ ...kept, tasks }))) ?? thread;
        send({ type: 'thread', thread: entry });
      }
      return { thread: entry, status, history: await client.history(dir, undefined, { from }), running: running.has(id), queued: queuedOf(id) };
    });
  },

  /** Takes a thread back to just before one of its messages (`at`, its index in the
   * history at `revision`, or now): the conversation and workspace as they were then;
   * the message and the files attached to it come back to send again. */
  async rewindTo({ id, at, revision }) {
    await workable(id);
    notRunning(id, 'taking it back');
    const dir = dirOf(id);
    const back = await within(id, async () => {
      const shown = (await client.history(dir, revision ?? undefined)).messages ?? [];
      const message = shown[at];
      if (message?.role !== 'user') throw failure('invalid', 'that is not one of your messages');
      const { text, paths } = unattached(message.content.filter((block) => block.type === 'text').map((block) => block.steer ?? block.text).join('\n'));
      const files = [];
      for (const path of paths) {
        try {
          files.push({ path, bytes: await client.read(dir, path, revision ?? undefined) });
        } catch {
          // Gone already: it is not sent again.
        }
      }
      if (at === 0) {
        // The files it started with came with its first message: going back to before
        // it would leave the thread without them.
        if (STARTED_WITH.test(text)) throw failure('invalid', 'This thread started with the files of another thread, which came with its first message; to begin again with them, start a new thread with these files (the thread menu)');
        return { text, files, first: true };
      }
      const target = await pointBefore(dir, shown, at);
      if (target == null) throw failure('not_found', 'Spindle no longer holds the point before that message (it keeps the last 8192 steps); start a new thread with its files instead');
      await client.rewind(dir, target);
      await forgetUnlisted(id);
      return { text, files, first: false };
    });
    if (back.first) {
      // Its first message: nothing before it to go back to; the thread starts afresh
      // with its settings (as taking back a first turn does).
      await renew(id);
      rest(id);
    }
    await touch(id, { state: 'idle', note: null });
    return { text: back.text, files: back.files };
  },

  /** The messages a compaction folded into a summary: the history at the latest point
   * before it (before `revision`'s, or the head's, first summary), and that point. */
  async earlier({ id, revision }) {
    await workable(id);
    const dir = dirOf(id);
    return within(id, async () => {
      const shown = (await client.history(dir, revision ?? undefined)).messages ?? [];
      const summary = JSON.stringify(shown[0] ?? null);
      for (const target of await client.targets(dir, { limit: 8192 })) {
        if (revision != null && target.revision >= revision) continue;
        const history = await client.history(dir, target.revision);
        if (JSON.stringify(history.messages?.[0] ?? null) !== summary) return { revision: target.revision, history };
      }
      throw failure('not_found', 'Spindle no longer holds the messages before that summary');
    });
  },

  async settings({ id, changes }) {
    await workable(id);
    return within(id, () => changeSettings(id, changes));
  },

  /** A thread's spending limit and what it spent (`usage`: its calls by how they were
   * paid, the dollars of those priced). */
  spend: ({ id }) => within(id, async () => {
    const dir = dirOf(id);
    const [settings, usage] = await Promise.all([client.settings(dir), client.usage(dir)]);
    return {
      max_usd: settings.max_usd ?? null, usd: usage.usd, calls: usage.calls,
      subscription: usage.subscription, unpriced: usage.unpriced, unknown: usage.unknown,
    };
  }),

  /** The efforts a provider's model takes, each with what it then asks for. */
  efforts: ({ provider, model }) => client.efforts(provider, model),

  /** Starts over a turn that stopped before it finished: takes it back (to the end of
   * the turn before, or to a fresh start for a thread's first turn), applies `changes`
   * (provider, model, effort, where commands run, network) and sends its message again
   * with the files attached to it; or, `resend` false, gives it back ({text, files}). */
  async restart({ id, changes = {}, resend = true }) {
    await workable(id);
    notRunning(id, 'starting it over');
    const dir = dirOf(id);
    const { text, files, fresh, carried = [] } = await within(id, async () => {
      const status = await client.status(dir);
      if (status.accepts_prompt) throw failure('invalid', 'this thread has no unfinished turn to start over');
      const settled = (await client.targets(dir)).find((target) => target.kind === 'settled');
      if (!settled && status.unresolved) {
        throw failure('conflict', 'its first request may have reached the provider with no reply saved: Retry it, or delete the thread');
      }
      // Its message: the first the user wrote after the turn before (the files attached to
      // it are read now: taking the turn back takes them out of the workspace).
      const kept = settled ? ((await client.history(dir, settled.revision)).messages ?? []).length : 0;
      const prompt = ((await client.history(dir)).messages ?? []).slice(kept)
        .find((message) => message.role === 'user' && message.content?.some((block) => block.type === 'text' && block.text.trim()));
      if (!prompt) throw failure('invalid', "this turn's message was not found");
      const { text, paths } = unattached(prompt.content.filter((block) => block.type === 'text').map((block) => block.text).join('\n'));
      const files = [];
      for (const path of paths) {
        try {
          files.push({ path, bytes: await client.read(dir, path) });
        } catch {
          // Gone already (a command of the turn removed it): it is not sent again.
        }
      }
      if (settled) {
        if (Object.keys(changes).length) {
          const settings = await client.settings(dir);
          await checkCreation(changes.provider ?? settings.provider, { ...creationSettings(settings), ...changes });
        }
        await client.rewind(dir, settled.revision);
        await forgetUnlisted(id);
        if (Object.keys(changes).length) await changeSettings(id, changes);
        return { text, files, fresh: null };
      }
      // A first turn that started the thread with another thread's files: started over,
      // it goes with them again (as its first revision holds them); taken back alone, the
      // thread would be left without them.
      const carried = [];
      if (STARTED_WITH.test(text)) {
        if (!resend) throw failure('invalid', 'This thread started with the files of another thread, which came with this first message; start the turn over (the model menu), or start a new thread with these files (the thread menu)');
        for (const file of (await client.files(dir, 1)).files) {
          if (paths.includes(file.path)) continue;
          const { files: [encoded] } = attachments([{ path: file.path, bytes: await client.read(dir, file.path, 1) }]);
          carried.push({ ...encoded, ...(file.executable && { executable: true }) });
        }
      }
      return { text, files, fresh: await client.settings(dir), carried };
    });
    if (fresh) {
      // A first turn has nothing before it to go back to: the thread starts afresh with
      // the same settings, as changed.
      const where = changes.runner ?? fresh.runner ?? 'app';
      const source = changes.provider && changes.provider !== fresh.provider;
      const settings = {
        ...creationSettings(fresh),
        ...(source && { model: undefined, effort: 'default' }),
        ...changes,
        runner: where,
        network: Boolean(changes.network ?? fresh.network) && where !== 'none',
        instructions: guided(fresh.instructions, where === 'app' && (fresh.builtin_tools ?? 'files') === 'files', (await read('kv', 'standing')) ?? ''),
      };
      await replaceFirst(id, changes.provider ?? fresh.provider, settings);
      const status = await client.status(dir);
      warmFor(await touch(id, {
        provider: status.provider, model: status.model, effort: effortOf(status), runner: status.settings?.runner ?? 'none',
        network: Boolean(status.settings?.network), tasks: Boolean(status.settings?.tasks), state: 'idle', note: null,
      }));
      rest(id);
    } else {
      await touch(id, { state: 'idle', note: null });
    }
    if (!resend) return { text, files };
    const { files: sent, note } = attachments(files);
    const all = [...carried, ...(sent ?? [])];
    turn(id, async (check) => {
      await freshen(dir);
      check();
      return client.prompt(dir, text + note, all.length ? { files: all } : {});
    });
    return { started: true };
  },

  rename: ({ id, title }) => touch(id, { title: String(title).trim().slice(0, 80) || 'Untitled' }),

  async remove({ id }) {
    notRunning(id, 'deleting');
    if (using.has(id)) throw failure('busy', 'this thread is busy; try again in a moment');
    // Closing it here (if it is open) gives its store and Web Lock back first.
    await discard(id);
    await remove('bundles', id);
    bundleSizes.delete(id);
    await dropPriors(id);
    for (const key of await keysOf('kv')) if (key.startsWith(`preview:${id}:`)) await remove('kv', key);
    await remove('threads', id);
    queued.delete(id);
    linux.forget(dirOf(id));
    send({ type: 'removed', id });
    return { removed: true };
  },

  async export({ id, keep }) {
    await workable(id);
    notRunning(id, 'exporting');
    moving.add(id);
    try {
      return await within(id, async () => {
        // The file is for a native host, so its copy is set up for one (the thread here
        // keeps its own settings): no page tools (a native host could not answer them;
        // its turn would stop at the first call) and no browser Linux guide; a thread
        // with file tools gets native's full tool set (code mode too), run in the host's
        // own sandbox. Coming back here makes it a Chrome thread again.
        const dir = dirOf(id);
        const settings = await client.settings(dir);
        const tools = settings.tools ?? [];
        const instructions = settings.instructions ?? '';
        const builtin = settings.builtin_tools;
        const native = {
          tools: tools.filter((tool) => !PAGE_NAMES.has(tool.name)),
          instructions: guided(instructions, false),
          ...(builtin === 'files' && { builtin_tools: 'all', runner: 'none' }),
        };
        let thread = await read('threads', id);
        const name = `${slug(thread?.title)}.spindle`;
        if (keep) return { bytes: await client.exportSession(dir, { keep: true, settings: native }), name };
        // The file is kept (on disk: a strict commit) before this copy is sealed, so
        // whatever stops the engine, the thread is in one or the other; and again once
        // sealed, before the sealed store goes. Kept, it can be saved again (a closed
        // panel or a cancelled save loses nothing).
        const copy = await client.exportSession(dir, { keep: true, settings: native });
        await writeDurably('bundles', id, { bytes: copy, name, size: copy.byteLength });
        const bytes = await client.exportSession(dir, { keep: false, settings: native });
        await writeDurably('bundles', id, { bytes, name, size: bytes.byteLength });
        bundleSizes.delete(id);
        thread = await touch(id, { state: 'moved', note: null });
        // The file is now the thread's one copy: once it reads back as written, the
        // sealed store goes. Tidying only: whatever stops it (a crash, a failed removal)
        // leaves the sealed store, for Settings > Storage to free; the move is done.
        try {
          const back = await read('bundles', id);
          if (back?.bytes && await sameBytes(back.bytes, bytes)) dropSealed(await openPool(), id);
        } catch (error) {
          console.warn('Spindle: the moved thread keeps its sealed store for now:', error);
        }
        return { bytes, name };
      });
    } finally {
      moving.delete(id);
    }
  },

  /** What each thread keeps in this browser, in bytes: its store, log, tasks' stores,
   * set-aside first attempts and other files, the free pages compacting would give back,
   * and for a moved thread its sealed copy and its file. */
  async storage() {
    const fs = await openPool();
    const known = new Map((await threads()).map((thread) => [thread.id, thread]));
    const usage = new Map();
    const entry = (id) => {
      if (!usage.has(id)) usage.set(id, { id, bytes: 0, database: 0, log: 0, tasks: 0, aside: 0, other: 0, free: 0, sealed: 0, bundle: 0 });
      return usage.get(id);
    };
    for (const { path, size } of fs.files()) {
      const [, top, id, ...parts] = path.split('/');
      if (top !== 'threads' || !known.has(id)) continue;
      const rest = parts.join('/');
      const kept = entry(id);
      kept.bytes += size;
      if (known.get(id).state === 'moved') {
        // Its fence (where it went) stays; the rest is the sealed copy it left.
        kept[rest === 'store/authority.sqlite.lock' ? 'other' : 'sealed'] += size;
        continue;
      }
      kept[partOf(rest)] += size;
      if (STORE_FILE.test(rest)) kept.free += freeIn(fs, path);
    }
    for (const thread of known.values()) {
      if (thread.state === 'moved') entry(thread.id).bundle = await bundleSize(thread.id);
    }
    return { threads: [...usage.values()] };
  },

  /** Gives a thread's free pages back (its store and its tasks'), checked whole before
   * the copy replaces anything. The engine does nothing else meanwhile, so nothing may
   * be running. */
  compact: ({ id }) => within(id, async () => {
    await workable(id);
    notRunning(id, 'freeing space in');
    await headroom();
    // Nothing may be at work anywhere in the engine: no turn and no background task (of
    // any thread); new work waits for the compaction (`maintenance`), and so does
    // another compaction that came in at the same moment.
    while (maintenance) await maintenance;
    let done = null;
    maintenance = new Promise((resolve) => { done = resolve; });
    try {
      const { sessions } = await client.call('list');
      if (running.size || sessions.some((session) => session.busy || session.background)) {
        throw failure('busy', 'space is freed while no thread or background task is at work (it holds the engine meanwhile)');
      }
      return await client.compactSession(dirOf(id));
    } finally {
      maintenance = null;
      done();
    }
  }),

  /** A thread's set-aside first attempts go: each kept a first request that was tried
   * again in a fresh store (what it may have cost is counted in the thread already). */
  dropAside: ({ id }) => within(id, async () => {
    await workable(id);
    const fs = await openPool();
    const prefix = `${dirOf(id)}/`;
    const asides = new Set();
    let freed = 0;
    for (const { path, size } of fs.files()) {
      const found = path.startsWith(prefix) && ASIDE_DIR.exec(path.slice(prefix.length));
      if (!found) continue;
      asides.add(prefix + found[1]);
      freed += size;
    }
    for (const dir of asides) fs.rmSync(dir, { recursive: true, force: true });
    return { freed };
  }),

  /** A moved thread's sealed copy goes, once its file is checked whole (as an import
   * checks it): the file is then its one copy, as the move promised. */
  freeSealed: ({ id }) => within(id, async () => {
    const thread = await read('threads', id);
    if (thread?.state !== 'moved') throw failure('invalid', 'only a thread moved into a file keeps a sealed copy');
    const kept = await read('bundles', id);
    if (!kept?.bytes?.byteLength) throw failure('not_found', 'this thread keeps no file, so its sealed copy stays');
    client.checkBundle(kept.bytes);
    // A strict commit first: the file's own (earlier) write is then on disk too.
    await writeDurably('kv', 'durable', Date.now());
    return { freed: dropSealed(await openPool(), id) };
  }),

  /** The file a moved thread went into, to save again. */
  async bundle({ id }) {
    const kept = await read('bundles', id);
    if (!kept) throw failure('not_found', 'this thread keeps no file');
    return kept;
  },

  async import({ bytes, name }) {
    await headroom();
    const id = newId();
    const dir = dirOf(id);
    let status;
    let history;
    try {
      status = await client.importSession(dir, bytes);
      history = await client.history(dir);
    } catch (error) {
      await discard(id).catch(() => {});
      throw error;
    }
    // Here it gets Chrome's page tools, and whatever a native host's settings need to
    // run in Chrome. The thread is in either way; what could not change, it says.
    let note = null;
    let said = [];
    let runner = 'none';
    let network = false;
    let tasks = false;
    try {
      const settings = await client.settings(dir);
      const told = status.fit ?? await client.fit(dir);
      const after = told.changes?.runner ?? settings.runner;
      const fit = fitForChrome(told, after);
      const tools = [...(settings.tools ?? []).filter((tool) => !PAGE_NAMES.has(tool.name)), ...client.tools.definitions()];
      const files = (fit.changes.builtin_tools ?? settings.builtin_tools) === 'files';
      const instructions = guided(settings.instructions, after === 'app' && files, (await read('kv', 'standing')) ?? '');
      const fitted = await client.settings(dir, { ...fit.changes, tools, instructions });
      runner = fitted.runner ?? 'none';
      network = Boolean(fitted.network);
      tasks = Boolean(fitted.tasks);
      said = fit.said;
      if (said.length) note = `Changed to run in Chrome: ${said.join('; ')}.`;
    } catch (error) {
      note = `Its settings could not be changed for Chrome: ${error.message}`;
      said = [`none could be made (${error.message}), so it may not run as it should`];
    }
    rest(id);
    const first = history.messages
      .find((message) => message.role === 'user' && message.content?.some((block) => block.type === 'text' && block.text.trim()))
      ?.content.find((block) => block.type === 'text').text;
    const now = Date.now();
    const thread = {
      id, title: titleFrom(first ?? '') || String(name ?? '').replace(/\.spindle$/, '') || 'Imported thread',
      provider: status.provider, model: status.model, effort: effortOf(status), runner, network, tasks, created: now, updated: now,
      state: 'idle', note, summaries: true,
    };
    await write('threads', id, thread);
    send({ type: 'thread', thread });
    // What changed for it to run here: the panel asks before the user goes on with it.
    return { id, said };
  },

  files: ({ id }) => within(id, () => client.files(dirOf(id))),

  /** The tasks a thread started (`{tasks: [{task, state, queued, answer, pending,
   * delivered, prompt, work}], activity}`; `pending`: it runs beside the thread now;
   * `work`: what this engine saw it do (`taskWork`), or null; `activity`: the thread's
   * own, `waiting_tasks` while its turn waits for them). Tasks a turn ran in the
   * foreground have no row: those this engine saw come last, by their short name, as
   * `{task, prompt, state, queued: false, pending: false, work}` (`state`: running, then
   * finished once it ended, whatever it came to: its call's result says). */
  tasks: ({ id }) => within(id, async () => {
    const [listed, status] = await Promise.all([client.tasks(dirOf(id)), client.status(dirOf(id))]);
    const seen = taskWork.get(id) ?? new Map();
    const tasks = (listed.tasks ?? []).map((task) => {
      const work = seen.get(String(task.task).split('#').pop()) ?? null;
      // One that ended without a last word (stopped where it was) ended when it was last
      // seen at work.
      if (work && task.pending) work.ended = null;
      else if (work && task.state !== 'running') work.ended ??= work.seen;
      return { ...task, work: work && { ...work } };
    });
    const rows = new Set(tasks.map((task) => String(task.task).split('#').pop()));
    for (const [name, work] of seen) {
      if (rows.has(name)) continue;
      tasks.push({ task: name, prompt: work.asked ?? '', state: work.ended ? 'finished' : 'running', queued: false, pending: false, delivered: false, answer: null, work: { ...work } });
    }
    return { ...listed, tasks, activity: status.activity };
  }),

  /** Stops one of a thread's tasks where it is (`{task, stopping}`). */
  taskStop: ({ id, task }) => within(id, () => client.taskStop(dirOf(id), String(task))),

  /** The workspace now, and as the thread's last turn found it (`before`: [path, digest]
   * pairs; null when that point is not found), to tell what the turn made or changed. */
  turnFiles: ({ id }) => within(id, async () => {
    const dir = dirOf(id);
    const now = await client.files(dir);
    const key = `${id}@${now.revision}`;
    if (turnFilesKept.key === key) return turnFilesKept.value;
    const shown = (await client.history(dir)).messages ?? [];
    // The last message the user wrote (not steering, not a compaction's summary).
    const at = shown.findLastIndex((message) => message.role === 'user' && (message.content ?? []).some((block) =>
      block.type === 'text' && typeof block.steer !== 'string' && block.text.trim() && !block.text.startsWith('[Earlier conversation compacted')));
    let before = null;
    if (at === 0) {
      before = [];
    } else if (at > 0) {
      try {
        const revision = await pointBefore(dir, shown, at);
        if (revision != null) before = (await client.files(dir, revision)).files.map((file) => [file.path, file.digest]);
      } catch (error) {
        console.warn('Spindle: the previous workspace is unavailable:', error);
      }
    }
    const value = { ...now, before };
    turnFilesKept = { key, value };
    return value;
  }),

  read: ({ id, path }) => within(id, async () => ({ bytes: await client.read(dirOf(id), path) })),

  /** Connects the user's Spindle host (`{url, token}`; null disconnects) for threads whose
   * commands run there. */
  async host({ host }) {
    const next = host ? { url: String(host.url).replace(/\/+$/, ''), token: String(host.token) } : null;
    // Its token is kept sealed, so not while the keys are locked.
    if (next && !(await vault.usableNow())) throw failure('locked', "Spindle's keys are locked: unlock them, then connect the host");
    try {
      await changeHost(next);
    } catch (error) {
      throw failure('host', `the host did not connect (${error.message ?? error}); ${hostState.connected ? `still connected to ${hostState.url}` : 'none is connected'}`);
    }
    // Kept only once it answered: the URL as it is, the token in the vault.
    if (next) await vault.keepHost(next.token, [['kv', 'host', { url: next.url }]]);
    else await vault.forgetHost([['kv', 'host', undefined]]);
    return snapshot();
  },

  /** Adds (`{url, write}`) or removes (`{remove: name}`) a site scripts' `http()` may
   * reach, against the list as kept (other panels may have changed it): the package
   * checks the grants (and refuses ones that are not valid) before they are kept. */
  async sites({ add, remove: name }) {
    const now = (await read('kv', 'sites')) ?? [];
    let next = now.filter((grant) => grant.name !== name);
    if (add) {
      const url = new URL(add.url);
      // A grant's name: its host, told apart when two share one.
      const host = url.hostname.replace(/[^A-Za-z0-9.-]/g, '-').slice(0, 56) || 'site';
      let named = host;
      for (let n = 2; now.some((grant) => grant.name === named); n += 1) named = `${host}-${n}`;
      next = [...now, {
        name: named, url: url.href, risk: add.write ? 'write' : 'read',
        methods: add.write ? ['GET', 'HEAD', 'POST', 'PUT', 'PATCH', 'DELETE'] : ['GET', 'HEAD'],
      }];
    }
    client.setHttpGrants(next);
    await write('kv', 'sites', next);
    send({ type: 'sites', sites: next });
    return snapshot();
  },

  /** Sends commands' network through the user's relay (`url`: a wisp:// or wisps://
   * WISP server; null: through Chrome, websites only). The Linux rewires before its
   * next command. */
  async relay({ url }) {
    let next = null;
    if (url) {
      // Only what v86 takes for a WISP relay (wisp://HOST, wisps://HOST): anything else it
      // would treat as another kind of relay, and no command would run.
      let parsed = null;
      try {
        parsed = new URL(String(url).trim());
      } catch {}
      if (!parsed || !['wisp:', 'wisps:'].includes(parsed.protocol) || !parsed.hostname || !/^wisps?:\/\//.test(parsed.href)) {
        throw failure('invalid', `${String(url).trim() || 'that'} is not a relay address: a WISP server's is wisp://HOST or wisps://HOST`);
      }
      next = parsed.href;
    }
    if (next) await write('kv', 'relay', next);
    else await remove('kv', 'relay');
    linux.setRelay(next);
    send({ type: 'relay', relay: next });
    return snapshot();
  },

  /** The user's standing instructions, for every thread (each takes them before its
   * next turn; new ones at once). */
  async standing({ text }) {
    const kept = String(text ?? '').trim().slice(0, 8000);
    if (kept) await write('kv', 'standing', kept);
    else await remove('kv', 'standing');
    return snapshot();
  },

  // ---- credentials: only the engine reads or writes them (vault.mjs) ----

  /** Keeps an API key for a provider (sealed in the vault). */
  async saveKey({ provider, key }) {
    const profile = catalog.providers.find((p) => p.id === provider);
    const text = typeof key === 'string' ? key.trim() : '';
    if (!profile || !text) throw failure('invalid', 'a key needs a provider from the list and some text');
    await vault.keep(provider, { kind: 'key', key: text, origin: originOf(profile.base_url) });
    announceVault();
    return snapshot();
  },

  /** Forgets a provider's key or sign-in. */
  async forget({ provider }) {
    await vault.forget(String(provider));
    announceVault();
    return snapshot();
  },

  /** A forgotten passphrase: every saved key and sign-in goes (the host's token too);
   * threads stay. */
  async resetVault() {
    await vault.reset();
    await connectKeptHost().catch(() => {});
    announceVault();
    return snapshot();
  },

  /** Exchanges a sign-in the panel caught (`code`, `verifier`, `redirect`) for tokens
   * and keeps them sealed. */
  async signIn({ provider, code, verifier, redirect }) {
    const profile = catalog.providers.find((p) => p.id === provider);
    if (!flowOf(profile)) throw failure('invalid', `${profile?.display_name ?? provider} does not sign in through a browser here`);
    // A code is good for one exchange: none while its tokens could not be kept.
    if (!(await vault.usableNow())) throw failure('locked', "Spindle's keys are locked: unlock them, then sign in");
    const tokens = await exchange(profile, { code, verifier, redirect }, reach);
    await vault.keep(provider, { ...tokens, origin: originOf(profile.base_url), tokenOrigin: originOf(flowOf(profile).token_url) });
    announceVault();
    return snapshot();
  },

  async unlock({ passphrase }) {
    await vault.unlock(passphrase);
    // The host waited for its token (or one kept as text is sealed now).
    const kept = await read('kv', 'host');
    if (kept && (!hostState.connected || kept.token != null)) await connectKeptHost().catch(() => {});
    announceVault();
    return snapshot();
  },

  /** Locks a passphrase vault: its keys, and the host's token, unusable until unlocked
   * (the host is let go, so no command reaches it meanwhile). */
  async lock() {
    await vault.lock();
    if (hostState.url && !(await vault.usableNow())) {
      await disconnectHost(hostState.url, "Spindle's keys are locked: unlock them to connect it").catch(() => {});
    }
    announceVault();
    return snapshot();
  },

  /** Sets (or, with `previous`, changes) the passphrase that locks the keys. */
  async protect({ passphrase, previous }) {
    await vault.protect(passphrase, previous);
    announceVault();
    return snapshot();
  },

  /** Keys sealed by this browser alone again (the passphrase checked first). */
  async unprotect({ passphrase }) {
    await vault.unprotect(passphrase);
    announceVault();
    return snapshot();
  },

  /** Starts (or resumes) the browser Linux ahead of a command. */
  async warm() {
    linux.warm();
    return linuxState;
  },

  /** Uses `catalog` (Dext's providers.json format; null: the built-ins) from now on. */
  async catalog({ catalog: next }) {
    if (running.size) throw failure('busy', 'wait for running threads to finish before changing providers');
    if (next) {
      // Checked by the package before anything changes.
      new wasm.Embedded('memory', '{}', JSON.stringify(next)).free();
    }
    await client.close();
    // The client closes the pool only if a session opened it; a delete may have.
    pool?.close();
    pool = null;
    poolOpening = null;
    if (next) await write('kv', 'catalog', next);
    else await remove('kv', 'catalog');
    await start();
    return snapshot();
  },
};

const starting = (async () => {
  try {
    // Compiled once: a renewal instantiates it again.
    compiled = await WebAssembly.compile(await (await fetch(new URL('./web/spindle_embed_bg.wasm', import.meta.url))).arrayBuffer());
    memory = (await init({ module_or_path: compiled })).memory;
    await start();
    // Keys saved by a build that kept them as text are sealed now.
    await vault.sealPlain().catch((error) => console.error('Spindle: keys could not be sealed:', error));
    for (const record of await valuesOf('kv')) if (record?.type === 'reset') await restoreFirst(record);
    // Reset backups no reset is using (kept by earlier versions, or a reset finished just
    // before Chrome stopped): every reset cut short was restored above. Tidying only: a
    // backup that cannot go now stays for the next start.
    for (const key of await keysOf('bundles')) if (key.startsWith('prior:')) await remove('bundles', key).catch(() => {});
    // A thread the index still calls running was cut off when Chrome or the engine stopped.
    const known = new Set();
    const files = new Set(await keysOf('bundles'));
    for (const thread of await threads()) {
      known.add(thread.id);
      if (thread.state === 'running') {
        await touch(thread.id, { state: 'interrupted', note: 'Chrome stopped while it ran' });
      }
      const fence = `/threads/${thread.id}/store/authority.sqlite.lock`;
      const fs = await openPool();
      let moved = thread.state === 'moved';
      if (!moved && fs.existsSync(fence) && fs.readFileSync(fence).startsWith('spindle-moved: ')) {
        await touch(thread.id, { state: 'moved', note: null });
        moved = true;
      }
      // A file a move wrote before the engine stopped short of sealing the thread: the
      // thread is still here, whole, so the file is a stale copy (tidying only).
      if (!moved && files.has(thread.id)) await remove('bundles', thread.id).catch(() => {});
    }
    // Files no thread owns (an engine stopped between writing a thread and indexing it).
    const fs = await openPool();
    for (const name of fs.readdirSync('/threads')) {
      if (!known.has(name)) fs.rmSync(`/threads/${name}`, { recursive: true, force: true });
    }
  } catch (error) {
    broken = `the engine could not start: ${error.message ?? error}`;
  }
})();

channel.onmessage = async ({ data }) => {
  if (data?.to !== 'engine') return;
  const { from, id, op, args } = data;
  try {
    await starting;
    while (renewing) await renewing.catch(() => {});
    if (broken) throw failure('broken', broken);
    if (!Object.hasOwn(ops, op)) throw failure('invalid', `there is no operation ${op}`);
    answering += 1;
    try {
      send({ to: from, id, ok: true, result: await operate(op, args ?? {}) });
    } finally {
      answering -= 1;
    }
  } catch (error) {
    send({ to: from, id, ok: false, error: { kind: error.kind ?? 'failed', message: String(error.message ?? error) } });
  }
};

await starting;
send({ type: 'ready', broken });
