// The engine: every Spindle thread of this browser profile runs here, in a Dedicated
// Worker of the extension's offscreen document, as a session of the embedded package
// over OPFS. Panels ask over the channel and see every event; turns go on while no
// panel is open. The thread index (titles, times, states) is the engine's to keep.
import init, * as wasm from './web/spindle_embed.js';
import { EmbeddedClient, remoteRunner } from './clients/embedded.mjs';
import { linuxRunner } from './linux-runner.mjs';
import { openOpfsPool } from './clients/opfs.mjs';
import { titleFrom } from './text.js';
import { PAGE_TOOLS } from './page-tools.mjs';
import { CHANNEL, keysOf, read, remove, update, valuesOf, write } from './shared.mjs';
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
});
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
/** Page tool call -> the function that answers it. */
const toolCalls = new Map();
/** Short operations serialize per thread; a turn itself stays in the background. */
const lanes = new Map();
let pendingOps = 0;
let changingCatalog = false;

async function operate(op, args) {
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
  const keys = op === 'stop' ? [] : [...new Set(threadIds)];
  // A sign-in waits on the provider's token endpoint: in a lane of its own, so `hello`
  // and other settings are not held up behind it.
  if (!keys.length && op !== 'stop') keys.push(op === 'signIn' ? 'signin' : 'settings');
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
  'HTTP and HTTPS, which Chrome fetches through the proxy their environment names (without the user\'s cookies; nothing',
  "on the user's computer or local network); if the user set a relay, they reach any address through it. Without",
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
  'HTTP and HTTPS, which Chrome fetches through the proxy their environment names (without the user\'s cookies; nothing',
  "on the user's computer or local network); if the user set a relay, they reach any address through it.",
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

/** The user's standing instructions in a thread's, between these lines. */
const STANDING_START = "[The user's standing instructions]";
const STANDING_END = "[End of the user's standing instructions]";
const STANDING = /\[The user's standing instructions\]\n([\s\S]*?)\n\[End of the user's standing instructions\]/;

/** A thread's instructions as they are, with the Linux guide in (`on`) or out and the
 * user's standing instructions as `standing` says (undefined: as they are). */
function guided(instructions, on, standing) {
  let rest = String(instructions ?? '');
  for (const guide of [LINUX_GUIDE, ...EARLIER_GUIDES]) rest = rest.replace(guide, '');
  const kept = STANDING.exec(rest)?.[1] ?? '';
  rest = rest.replace(STANDING, '').trim();
  const user = (standing ?? kept).trim();
  return [rest, user && `${STANDING_START}\n${user}\n${STANDING_END}`, on && LINUX_GUIDE].filter(Boolean).join('\n\n');
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
 * tell the user. The package checks the same and stays the authority: anything it still
 * refuses, it names. */
function fitForChrome(settings) {
  const changes = {};
  const said = [];
  if (settings.builtin_tools === 'all') changes.builtin_tools = 'files';
  // bash comes with the file tools and a runner (a session offering no built-in tools
  // keeps offering none).
  const files = (changes.builtin_tools ?? settings.builtin_tools) === 'files';
  if (files && settings.runner !== 'app' && settings.runner !== 'remote') {
    changes.runner = 'app';
    said.push("its commands run in this browser's Linux");
  }
  if (settings.network && (changes.runner ?? settings.runner) === 'app') {
    said.push("commands keep their network (websites through Chrome, or your relay if you set one; Chrome asks for access to sites when you next send)");
  }
  if (settings.toolchain) {
    changes.toolchain = false;
    said.push('the Rust toolchain off');
  }
  if (settings.sources?.length) {
    changes.sources = [];
    said.push('host folders unmounted');
  }
  if (settings.max_usd != null) {
    changes.max_usd = null;
    said.push(`its US$${settings.max_usd} budget removed (no spend ledger here)`);
  }
  return { changes, said };
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
    network: Boolean(settings.network),
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
    if (id) send({ type: 'event', thread: id, event });
  });
  for (const name of ['work_error', 'app_tool_error']) made.on(name, (error) => console.error(`Spindle ${name}:`, error));
  return made;
}

async function start() {
  const saved = await read('kv', 'catalog');
  try {
    client = makeClient(saved);
    catalogNote = null;
  } catch (error) {
    client = makeClient(undefined);
    catalogNote = `your providers.json is not in use: ${error.message}`;
  }
  catalog = client.catalog();
  await client.ready;
  // The host connects meanwhile: one that does not answer holds up nothing here.
  const host = await read('kv', 'host');
  if (host) {
    hostState = { url: host.url, connected: false, error: null };
    changeHost(host).catch(() => {});
  }
  try {
    client.setHttpGrants((await read('kv', 'sites')) ?? []);
  } catch (error) {
    console.error('Spindle: the sites scripts may reach are not in use:', error);
  }
  linux.setRelay((await read('kv', 'relay')) ?? null);
}

/** Connects the user's Spindle host (`{url, token}`; null disconnects) as the remote
 * runner, after any change asked before it. A host that does not answer leaves the one
 * connected before as it was. Panels hear of the outcome. */
function changeHost(host) {
  const change = hostChanges.then(async () => {
    try {
      await client.connectRunner('remote', host ? remoteRunner({ url: host.url, token: host.token, fetch: reach }) : null);
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
  const held = await vault.secret(facts.provider, (secret) => refreshed(profile, secret, reach));
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

/** fetch, saying what to do when Chrome refuses a provider the extension may not reach. */
async function reach(url, init) {
  try {
    return await fetch(url, init);
  } catch (error) {
    if (init?.signal?.aborted) throw error;
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
      answerTool(call, { error: 'Chrome did not answer the page tool in time' });
    }, PAGE_TOOL_WAIT);
    toolCalls.set(call, Object.assign((answer) => {
      clearTimeout(timer);
      resolve(answer);
    }, { session }));
    self.postMessage({ type: 'page_tool', call, name, input });
  });
}

function cancelPageTools(id) {
  for (const [call, answer] of toolCalls) {
    if (idOf(answer.session?.split('#')[0] ?? '') !== id) continue;
    self.postMessage({ type: 'page_tool_cancel', call });
    answerTool(call, { error: 'the turn ended before the page tool finished' });
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

/** Closes an idle thread's session; the package refuses to close a running one. */
function rest(id) {
  if (!running.has(id) && !using.has(id)) client.closeSession(dirOf(id)).catch(() => {});
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
      send({ type: 'turn_end', thread: id, unsent });
      rest(id);
    }
  })();
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
  await client.closeSession(dirOf(id));
  (await openPool()).rmSync(dirOf(id), { recursive: true, force: true });
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
  const backup = `prior:${id}:${crypto.randomUUID()}`;
  await write('bundles', backup, { bytes: await client.exportSession(dirOf(id), { keep: true }) });
  await write('kv', `reset:${id}`, { type: 'reset', id, backup });
  try {
    await discard(id);
    await client.create(dirOf(id), provider, settings);
    await remove('kv', `reset:${id}`);
  } catch (error) {
    await restoreFirst({ id, backup });
    throw error;
  }
}

async function restoreFirst({ id, backup }) {
  const kept = await read('bundles', backup);
  if (!kept) throw failure('broken', `the saved record for thread ${id} is missing`);
  await discard(id);
  await client.importSession(dirOf(id), kept.bytes);
  await client.closeSession(dirOf(id));
  await remove('kv', `reset:${id}`);
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

  async create({ provider, model, effort, text, runner = 'app', network = false, files: given, from }) {
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
        ...(instructions && { instructions }),
      });
      status = await client.status(dir);
    } catch (error) {
      await discard(id).catch(() => {});
      throw error;
    }
    const now = Date.now();
    const thread = {
      id, title: titleFrom(text) || 'New thread', provider: status.provider, model: status.model, effort: effortOf(status),
      runner: status.settings?.runner ?? 'app', network: Boolean(status.settings?.network), created: now, updated: now,
      state: 'idle', note: null, summaries: true,
    };
    await write('threads', id, thread);
    send({ type: 'thread', thread });
    const { files, note } = attachments(given);
    const sent = [...(carried?.files ?? []), ...(files ?? [])];
    // Before the attachments' line, which ends a message.
    const started = carried ? `\n\n[Started with the files of “${carried.title}”: ${carried.files.length} in the workspace]` : '';
    turn(id, () => client.prompt(dir, text + started + note, sent.length ? { files: sent } : {}));
    if (thread.runner === 'app') linux.warm();
    return { id };
  },

  async send({ id, text, files: given }) {
    await workable(id);
    await headroom();
    // Decided after the last wait: a turn that started meanwhile is steered (a message
    // with files waits for its own turn: a running one takes text only).
    if (running.has(id)) {
      if (given?.length) throw failure('busy', 'this thread is running; send the files once it is done (or stop it)');
      return { steered: (await client.steer(dirOf(id), text)).queued };
    }
    const { files, note } = attachments(given);
    turn(id, async (check) => {
      await freshen(dirOf(id));
      check();
      return client.prompt(dirOf(id), text + note, files ? { files } : {});
    });
    return { started: true };
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
    if (thread.runner === 'app') linux.warm();
    return within(id, async () => ({
      thread, status: await client.status(dir), history: await client.history(dir, undefined, { from }), running: running.has(id),
    }));
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
      await touch(id, {
        provider: status.provider, model: status.model, effort: effortOf(status), runner: status.settings?.runner ?? 'none',
        network: Boolean(status.settings?.network), state: 'idle', note: null,
      });
      if (status.settings?.runner === 'app') linux.warm();
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
    for (const key of await keysOf('bundles')) if (key.startsWith(`prior:${id}:`)) await remove('bundles', key);
    await remove('threads', id);
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
        // The file is kept before this copy is sealed: whatever stops the engine, the
        // thread is in one or the other. Kept, it can be saved again (a closed panel or a
        // cancelled save loses nothing).
        await write('bundles', id, { bytes: await client.exportSession(dir, { keep: true, settings: native }), name });
        const bytes = await client.exportSession(dir, { keep: false, settings: native });
        await write('bundles', id, { bytes, name });
        thread = await touch(id, { state: 'moved', note: null });
        return { bytes, name };
      });
    } finally {
      moving.delete(id);
    }
  },

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
    let runner = 'none';
    let network = false;
    try {
      const settings = await client.settings(dir);
      const fit = fitForChrome(settings);
      const tools = [...(settings.tools ?? []).filter((tool) => !PAGE_NAMES.has(tool.name)), ...client.tools.definitions()];
      const after = fit.changes.runner ?? settings.runner;
      const files = (fit.changes.builtin_tools ?? settings.builtin_tools) === 'files';
      const instructions = guided(settings.instructions, after === 'app' && files, (await read('kv', 'standing')) ?? '');
      const fitted = await client.settings(dir, { ...fit.changes, tools, instructions });
      runner = fitted.runner ?? 'none';
      network = Boolean(fitted.network);
      if (fit.said.length) note = `Changed to run in Chrome: ${fit.said.join('; ')}.`;
    } catch (error) {
      note = `Its settings could not be changed for Chrome: ${error.message}`;
    }
    rest(id);
    const first = history.messages
      .find((message) => message.role === 'user' && message.content?.some((block) => block.type === 'text' && block.text.trim()))
      ?.content.find((block) => block.type === 'text').text;
    const now = Date.now();
    const thread = {
      id, title: titleFrom(first ?? '') || String(name ?? '').replace(/\.spindle$/, '') || 'Imported thread',
      provider: status.provider, model: status.model, effort: effortOf(status), runner, network, created: now, updated: now,
      state: 'idle', note, summaries: true,
    };
    await write('threads', id, thread);
    send({ type: 'thread', thread });
    return { id };
  },

  files: ({ id }) => within(id, () => client.files(dirOf(id))),

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
    try {
      await changeHost(next);
    } catch (error) {
      throw failure('host', `the host did not connect (${error.message ?? error}); ${hostState.connected ? `still connected to ${hostState.url}` : 'none is connected'}`);
    }
    // Kept only once it answered.
    if (next) await write('kv', 'host', next);
    else await remove('kv', 'host');
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
    await vault.keep(provider, { kind: 'key', key: text });
    announceVault();
    return snapshot();
  },

  /** Forgets a provider's key or sign-in. */
  async forget({ provider }) {
    await vault.forget(String(provider));
    announceVault();
    return snapshot();
  },

  /** A forgotten passphrase: every saved key and sign-in goes; threads stay. */
  async resetVault() {
    await vault.reset();
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
    await vault.keep(provider, await exchange(profile, { code, verifier, redirect }, reach));
    announceVault();
    return snapshot();
  },

  async unlock({ passphrase }) {
    await vault.unlock(passphrase);
    announceVault();
    return snapshot();
  },

  async lock() {
    await vault.lock();
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
    await init({ module_or_path: new URL('./web/spindle_embed_bg.wasm', import.meta.url) });
    await start();
    // Keys saved by a build that kept them as text are sealed now.
    await vault.sealPlain().catch((error) => console.error('Spindle: keys could not be sealed:', error));
    for (const record of await valuesOf('kv')) if (record?.type === 'reset') await restoreFirst(record);
    // A thread the index still calls running was cut off when Chrome or the engine stopped.
    const known = new Set();
    for (const thread of await threads()) {
      known.add(thread.id);
      if (thread.state === 'running') {
        await touch(thread.id, { state: 'interrupted', note: 'Chrome stopped while it ran' });
      }
      const fence = `/threads/${thread.id}/store/authority.sqlite.lock`;
      const fs = await openPool();
      if (thread.state !== 'moved' && fs.existsSync(fence) && fs.readFileSync(fence).startsWith('spindle-moved: ')) {
        await touch(thread.id, { state: 'moved', note: null });
      }
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
    if (broken) throw failure('broken', broken);
    if (!Object.hasOwn(ops, op)) throw failure('invalid', `there is no operation ${op}`);
    send({ to: from, id, ok: true, result: await operate(op, args ?? {}) });
  } catch (error) {
    send({ to: from, id, ok: false, error: { kind: error.kind ?? 'failed', message: String(error.message ?? error) } });
  }
};

await starting;
send({ type: 'ready', broken });
