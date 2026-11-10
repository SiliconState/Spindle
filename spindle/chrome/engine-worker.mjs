// The engine: every Spindle thread of this browser profile runs here, in a Dedicated
// Worker of the extension's offscreen document, as a session of the embedded package
// over OPFS. Panels ask over the channel and see every event; turns go on while no
// panel is open. The thread index (titles, times, states) is the engine's to keep.
import init, * as firstGlue from './web/spindle_embed.js';
import { EmbeddedClient, bundleFacts, remoteRunner } from './clients/embedded.mjs';
import { refusedAnswer } from './clients/common.mjs';
import { RemoteClient, requestId, worthRepeating } from './clients/remote.mjs';
import { BUNDLE_MOST, MOVE, STEP_TEXT, Waiting, hostMoves, said as sentence } from './host-moves.mjs';
import { linuxRunner } from './linux-runner.mjs';
import { storeRefusal } from './linux-net.mjs';
import { openOpfsPool } from './clients/opfs.mjs';
import { titleFrom } from './text.js';
import { ASKING_NAMES, BROWSER, PAGE_TOOLS, UPLOAD, actionText, confirmMode, numbersIn, sends, uncertain } from './page-tools.mjs';
import { typeOf } from './page.mjs';
import { CHANNEL, keysOf, read, remove, removeDurably, merge, update, valuesOf, write, writeDurably, writeTogether, REQUESTS, answersFor, has, keysWith } from './shared.mjs';
import { endedText, exchange, flowOf, planSignIn, refreshed, revoke } from './signin.mjs';
import * as vault from './vault.mjs';

/** Free pool slots kept before work that may add files (a thread holds a few, and each
 * background task it starts a few more; the pool also grows on its own as slots run
 * low, clients/js/opfs.mjs). */
const HEADROOM = 48;
/** How long a page tool call may wait for Chrome (the package gives up at 30 s). */
const PAGE_TOOL_WAIT = 35_000;

const channel = new BroadcastChannel(CHANNEL);
const send = (message) => channel.postMessage(message);
const requests = new BroadcastChannel(REQUESTS);
/** Each asking page's own answer channel (the newest few kept open). */
const answerChannels = new Map();
function answerTo(page) {
  let found = answerChannels.get(page);
  if (!found) {
    found = new BroadcastChannel(answersFor(page));
    answerChannels.set(page, found);
    if (answerChannels.size > 32) {
      const [oldest, gone] = answerChannels.entries().next().value;
      gone.close();
      answerChannels.delete(oldest);
    }
  }
  return found;
}
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
/** The user's Spindle host, when connected (its sandbox runs `runner: remote` threads;
 * with a browser token, `kind: 'browser'`, it also runs threads moved there). */
let hostState = { url: null, connected: false, error: null, kind: null, client: null };
/** The connected host's /v1 (null: none), whatever its token reaches. */
let hostRemote = null;
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
 * why it did not when it could not. Each is kept on disk too (`kv`, its `key`), from
 * before any panel is told it waits until a turn has taken it, so Chrome stopping first
 * loses none of it, files and all (`restoreQueued`). */
const queued = new Map();
/** Thread id -> the waiting message its running turn (here) took: its record stays on
 * disk until that turn has ended and the thread holds it (`afterTurn`). */
const carrying = new Map();
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
  // A panel's `hello` and `ping` answer while space is freed (up to ten minutes): a
  // page that hears nothing would offer to restart the engine.
  if (op !== 'hello' && op !== 'ping') while (maintenance) await maintenance;
  if (changingCatalog) throw failure('busy', 'the provider catalog is changing; try again in a moment');
  const threadIds = [args.id, ...(op === 'create' ? [args.from] : [])].filter((id) => id != null);
  for (const id of threadIds) {
    if (typeof id !== 'string' || !/^[0-9a-f]{16}$/.test(id)) throw failure('invalid', 'invalid thread id');
  }
  if (op === 'catalog') {
    if (pendingOps || running.size) throw failure('busy', 'wait for threads and pending operations before changing providers');
    changingCatalog = true;
    // A new provider list starts a new client, which ends the old one's background tasks
    // mid-request: they are waited for, as freeing space waits for them.
    const { sessions = [] } = await client?.call('list').catch(() => null) ?? {};
    if (sessions.some((session) => session.busy || session.background)) {
      changingCatalog = false;
      throw failure('busy', 'wait for background tasks to finish (or stop them) before changing providers');
    }
  }
  pendingOps += 1;
  // Stopping a turn or a task waits behind no slow read of its thread.
  const stopping = op === 'stop' || op === 'taskStop';
  const keys = stopping ? [] : [...new Set(threadIds)];
  // A sign-in waits on the provider's token endpoint (a sign-out of Sign in with ChatGPT
  // on its revocation endpoint): in a lane of its own, so `hello` and other settings are
  // not held up behind it.
  // An answer to a waiting page action waits behind nothing slow either.
  // So does a connect to the user's host (it waits on the host).
  // A panel's `hello` (and `ping`) waits behind nothing slow (a host connect, an unlock).
  if (!keys.length && !stopping) keys.push(['signIn', 'signInStart', 'signInDeclined', 'forget'].includes(op) ? 'signin' : op === 'approve' ? 'approve' : op === 'hostConnect' ? 'connect' : op === 'hello' || op === 'ping' ? 'hello' : 'settings');
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
/** The guide as it read before packages left the workspace and NOTES.md was named (threads
 * made before hold it; `freshen` gives them the current one). */
const GUIDE_START_BEFORE_NOTES = [
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
  'allowed (any other is refused, naming those allowed; a site the user saved with a secret header, such as a token,',
  'gets it from Spindle: never set that header or ask for its value); in Python, `import spindle` gives',
  "spindle.model(prompt)['text'], spindle.models([prompts]) (run together), spindle.tool(name, args), spindle.http(url)",
  "and spindle.batch(calls); in Node, require('spindle') or `import ... from 'spindle'` gives the same, synchronously,",
  'and `node file.ts` runs TypeScript (types are stripped, not checked).',
  'Prefer one script that loops, filters and aggregates (asking models or tools as it goes) and prints only what you',
  'need over many separate tool calls.',
  'What you make for the user (pages, apps, reports, pictures) goes in the workspace as files: each shows in the',
];
const GUIDE_START = [
  "Your commands run in a small Linux inside the user's browser (Alpine: bash, coreutils, Python 3 with pip, Node with",
  'npm, git, curl, jq, ripgrep). Each command starts afresh but for the workspace, so install packages into it',
  '(`npm install`, `python3 -m venv .venv`). Package and cache directories (.venv, node_modules, __pycache__, .cache)',
  'stay on the Linux between commands but are not workspace files (not in its history, not in tasks\' copies): after',
  'a long pause they may be gone, so check before using them, and keep what you make outside them.',
  'Keep NOTES.md in the workspace: what the user wants, decisions, where things are and what is left; its start is',
  'carried into the summary when the history is compacted, so you go on knowing it.',
  'With network on (the bash tool says whether), commands reach websites over',
  'HTTP and HTTPS, which Chrome fetches through the proxy their environment names (without the user\'s cookies; the',
  "user's computer and local network addresses and names are refused); if the user set a relay, they reach any address through it. Without",
  "network, connections are refused: ask the user to turn the thread's network on (its model menu) rather than",
  'working around it. The Linux is emulated and slow: give a command that runs long, or makes Spindle calls, a',
  'timeout of a few minutes.',
  'Scripts there can ask Spindle for calls, each journaled so a call that was cut off is never sent twice:',
  "from bash, `spindle model PROMPT` prints a model's answer, `spindle tool NAME '{\"json\": \"args\"}'` calls a tool",
  '(such as read_page or list_tabs), `spindle search` lists the tools, `spindle http URL` calls a site the user',
  'allowed (any other is refused, naming those allowed; a site the user saved with a secret header, such as a token,',
  'gets it from Spindle: never set that header or ask for its value); in Python, `import spindle` gives',
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
  ...GUIDE_START_BEFORE_NOTES,
  'conversation, where the user reads, opens and downloads it; with page tools on, navigate to /workspace/NAME opens',
  'one in a tab for them.',
].join(' '), [
  ...GUIDE_START_BEFORE_NOTES,
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
// Every guide as it read before it said Spindle adds a site's secret header.
const SITES_NOW = 'allowed (any other is refused, naming those allowed; a site the user saved with a secret header, such as a token, ' +
  'gets it from Spindle: never set that header or ask for its value); in Python';
const SITES_BEFORE = 'allowed (any other is refused, naming those allowed); in Python';
EARLIER_GUIDES.push(...[LINUX_GUIDE, ...EARLIER_GUIDES]
  .filter((guide) => guide.includes(SITES_NOW))
  .map((guide) => guide.replace(SITES_NOW, SITES_BEFORE)));
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

/** What a thread is told of its own setup (`setupOf`), between these lines. */
const SETUP_START = "[This thread's setup]";
const SETUP_END = "[End of this thread's setup]";
const SETUP = /\[This thread's setup\]\n([\s\S]*?)\n\[End of this thread's setup\]/;

/** What every thread is told about what it writes, before the user's standing
 * instructions (which win). A later wording must still take this one out (`EARLIER_STYLES`). */
const STYLE = "Pages you make follow the system's light or dark theme and offer a System/Light/Dark switch as one small "
  + 'icon button in a corner that cycles them (no visible label, no menu); they print in light colours with everything '
  + "shown, and have no print button (Spindle's file tab has one). Never write em dashes.";
/** Earlier wordings of the style line, which threads made before hold. */
const EARLIER_STYLES = ["Pages you make follow the system's light or dark theme and offer a System/Light/Dark switch; never write em dashes."];

/** A thread's instructions as they are, with the style line, the Linux guide in (`on`)
 * or out, the user's standing instructions as `standing` says (undefined: as they are)
 * and the thread's `setup` (`setupOf`; undefined: none, as a thread leaving this browser
 * takes them). */
function guided(instructions, on, standing, setup) {
  let rest = String(instructions ?? '');
  for (const guide of [LINUX_GUIDE, ...EARLIER_GUIDES]) rest = rest.replace(guide, '');
  const kept = STANDING.exec(rest)?.[1] ?? '';
  rest = rest.replace(STANDING, '').replace(SETUP, '');
  for (const style of [STYLE, ...EARLIER_STYLES]) rest = rest.replace(style, '');
  rest = rest.trim();
  const user = (standing ?? kept).trim();
  return [rest, STYLE, user && `${STANDING_START}\n${user}\n${STANDING_END}`, setup && `${SETUP_START}\n${setup}\n${SETUP_END}`, on && LINUX_GUIDE]
    .filter(Boolean).join('\n\n');
}

/** What a thread is told of its own setup before each turn, which the user changes in
 * the panel and the thread cannot (it asks them): its model, where its commands run and
 * their network, its spending limit, the page switches and the sites its scripts may
 * call. Kept in its instructions, so they (and a provider's prompt cache) change only
 * when the user changes one of these. */
async function setupOf(id, settings) {
  const [thread, pages, sites] = await Promise.all([read('threads', id), read('kv', 'pages'), read('kv', 'sites')]);
  const runner = thread?.runner ?? 'app';
  const reading = pages?.reading === true;
  const acting = reading && pages?.acting === true;
  const before = {
    full: 'they act without asking the user',
    auto: 'the user is asked before what reads as sending or submitting (judged by its words: no guarantee) and before files are put in a field',
    ask: 'the user is asked before each one',
  }[confirmMode(pages)];
  return [
    `Model: ${settings.provider}/${settings.model}, effort ${settings.effort ?? 'default'}.`,
    runner === 'none' ? 'Commands: none.'
      : `Commands: ${runner === 'remote' ? "on the user's Spindle host" : "in this browser's Linux"}, network ${settings.network ? 'on' : 'off'}.`,
    `Spending limit: ${settings.max_usd != null ? `US$${settings.max_usd}` : 'none'}.`,
    `Page reading ${reading ? 'on' : 'off'}; page actions ${acting ? `on (${before})` : 'off'}.`,
    // Where its scripts run (none: no sites to name to its provider).
    runner !== 'none' && `Sites scripts may call with spindle http: ${(sites ?? []).map((grant) => grant.url).join(', ') || 'none'}.`,
    'The user changes these in the Spindle panel (the model menu, and Settings); you cannot, so ask them to.',
  ].filter(Boolean).join('\n');
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
    // Unreadable now, the setup it was told stays (nothing else held back for it).
    const setup = await setupOf(id, settings).catch(() => SETUP.exec(instructions)?.[1]);
    const wanted = guided(instructions, guide, (await read('kv', 'standing')) ?? '', setup);
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
    if (setting === 'network' && runner === 'app') said.push(`commands keep their network (websites through ${BROWSER}, or your relay if you set one; ${BROWSER} asks for access to sites when you next send)`);
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
/** The line `attachments` ends a message with, naming its files. */
function attachedNote(files) {
  if (!files?.length) return '';
  return `\n\n[Attached, in the workspace: ${files.map((file) => `${file.path} (${file.bytes.length} bytes)`).join(', ')}]`;
}

function base64Of(bytes) {
  let binary = '';
  for (let at = 0; at < bytes.length; at += 0x8000) binary += String.fromCharCode(...bytes.subarray(at, at + 0x8000));
  return btoa(binary);
}

function attachments(files) {
  if (!files?.length) return { files: null, note: '' };
  return {
    // As bytes: the package passes them beside the request, never as base64 inside it
    // (a thread started with another's files carries up to MAX_CARRIED).
    files: files.map((file) => ({ path: String(file.path), bytes: file.bytes instanceof Uint8Array ? file.bytes : new Uint8Array(file.bytes) })),
    note: attachedNote(files),
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

/** Most characters of a page read kept in the conversation: every later request sends
 * it again, so more goes into a workspace file the thread's file tools read and search
 * (as much as was asked for, nothing lost). */
const INLINE_PAGE = 32_000;
const SPILLED = new Set(['read_page', 'page_elements', 'page_html']);
/** A script's tool call (code mode): its call id. */
const SCRIPT_CALL = /(^|\/)code-[0-9a-f]+$/;

/** A long page read, its first INLINE_PAGE characters kept in the answer and the whole in
 * the workspace (`.pages/`), for a thread that has file tools; any other answer as it is. */
async function spilled(name, answer, call) {
  const content = answer?.content;
  if (!SPILLED.has(name) || typeof content !== 'string' || content.length <= INLINE_PAGE || answer.error != null || answer.unknown) return answer;
  // A script's call keeps its whole answer: the package keeps no files a script's tool
  // call writes, so the file would never be there (the script reads its answer itself).
  if (SCRIPT_CALL.test(String(call?.call ?? ''))) return answer;
  try {
    const settings = await client.settings(String(call?.session ?? '').split('#')[0]);
    if ((settings.builtin_tools ?? 'all') === 'none') return answer;
  } catch {
    return answer;
  }
  // Its time and a random part: calls made together never share a name.
  const path = `.pages/${name.replace('_', '-')}-${new Date().toISOString().replace(/[:.]/g, '-')}-${newId().slice(0, 6)}.${name === 'page_html' ? 'html' : 'txt'}`;
  const more = content.length - INLINE_PAGE;
  return {
    ...answer,
    content: `${content.slice(0, INLINE_PAGE)}\n[${more} more characters: all that was read is in the workspace at ${path}; read or search it there (read_file, or grep in bash) instead of asking for the page again]`,
    files: { write: { [path]: content } },
  };
}

function newId() {
  return [...crypto.getRandomValues(new Uint8Array(8))].map((byte) => byte.toString(16).padStart(2, '0')).join('');
}

function makeClient(saved) {
  const made = new EmbeddedClient({
    wasm, storage: 'opfs', pool: openPool, catalog: saved, credentials, grantHeaders, fetch: reach, runner: { app: linux },
  });
  for (const tool of PAGE_TOOLS) {
    // An action the user is asked about first may wait for them (APPROVAL_WAIT), after
    // looking up what it lands on and before acting (each bounded by PAGE_TOOL_WAIT):
    // the package waits longer than all of that, so the engine always answers first (no
    // action the package gave up on, said unknown, happens after).
    const timeoutS = ASKING_NAMES.has(tool.name) ? 100 + APPROVAL_WAIT / 1000 : 30;
    made.tool({
      ...tool, timeoutS,
      run: async (input, call) => {
        // The turn's page tool generation before the files are read (a host thread's may
        // take a while): a Stop meanwhile is seen.
        const epoch = pageEpoch(call?.session, ownedByTask(call));
        const given = await prepared(tool.name, numbersIn(tool, input), call);
        if (given.error != null) return { error: given.error };
        return spilled(tool.name, await actOnPage(tool.name, given.input, call?.session, call?.signal, ownedByTask(call), epoch), call);
      },
    });
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
    client.setHttpGrants(grantsFor(await sitesKept()));
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
/** Counts the user's host changes (connect, paste, disconnect): a reconnect of the kept
 * host that began before one steps aside (it read what that change replaced). */
let hostGeneration = 0;

async function connectKeptHost() {
  const generation = hostGeneration;
  const host = await read('kv', 'host');
  if (!host) return;
  const locked = () => disconnectHost(host.url, "Spindle's keys are locked: unlock them to connect it");
  let token = host.token;
  if (token != null) {
    if (!(await vault.usableNow())) return locked();
    await vault.keepHost(token, [['kv', 'host', { url: host.url, ...(host.client && { client: host.client }) }]]);
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
  if (generation !== hostGeneration) return;
  hostState = { url: host.url, connected: false, error: null, kind: null, client: host.client ?? null };
  await changeHost({ url: host.url, token, client: host.client ?? null });
}

/** The remote runner let go, the host's URL kept and `why` told. */
function disconnectHost(url, why) {
  return changeHost(null).finally(() => {
    hostState = { url, connected: false, error: why, kind: null, client: null };
    send({ type: 'host', host: hostState });
  });
}

/** What a host token reaches: 'runner' (commands only), 'browser' (the runner, the
 * host's models and the threads it brings there) or 'full' (an API token's whole v1).
 * A host that does not answer is refused, as before. */
async function tokenKind(host) {
  // Only a 403 says what a token may not reach; any other failure (a host error, a
  // proxy's 502, a rate limit) says nothing about it, so the connect fails and is tried
  // again rather than keep a wrong kind.
  try {
    await host.models();
  } catch (error) {
    if (error.status !== 403) throw error;
    return 'runner';
  }
  try {
    await host.request('GET', '/v1/usage');
    return 'full';
  } catch (error) {
    if (error.status !== 403) throw error;
    return 'browser';
  }
}

/** Connects the user's Spindle host (`{url, token, client}`; null disconnects) as the
 * remote runner and, for a browser token, for threads that run there, after any change
 * asked before it. A host that does not answer leaves the one connected before as it
 * was. Panels hear of the outcome. */
function changeHost(host) {
  const change = hostChanges.then(async () => {
    try {
      const reached = host ? new RemoteClient({ url: host.url, token: host.token, fetch: reach }) : null;
      const kind = reached ? await tokenKind(reached) : null;
      const runner = host ? remoteRunner({ url: host.url, token: host.token, fetch: reach }) : null;
      let sandbox = null;
      try {
        await client.connectRunner('remote', runner);
        remote = runner;
      } catch (error) {
        // A host without a sandbox for commands still runs the threads moved there.
        if (kind !== 'browser') throw error;
        await client.connectRunner('remote', null).catch(() => {});
        remote = null;
        sandbox = `its sandbox does not run this browser's commands (${error.message ?? error})`;
      }
      if (hostRemote && (hostRemote.base !== reached?.base || hostRemote.token !== reached?.token)) {
        for (const [id, entry] of followed) {
          entry.controller.abort();
          cancelPageTools(id);
          followed.delete(id);
        }
      }
      hostRemote = reached;
      hostState = { url: host?.url ?? null, connected: Boolean(host), error: sandbox, kind, client: host?.client ?? null };
    } catch (error) {
      hostState = { ...hostState, error: `${host.url}: ${error.message ?? error}` };
      throw error;
    } finally {
      send({ type: 'host', host: hostState });
    }
  });
  hostChanges = change.catch(() => {});
  // Threads on the host, and moves waiting for it, go on once it answers.
  change.then(() => hostedAwake(), () => {});
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
    // Only onto the record it was read from: a sign-out meanwhile is not undone.
    await vault.rebind(facts.provider, held, {
      origin: api, ...(held.kind === 'signin' && { tokenOrigin: originOf(flowOf(profile)?.token_url) }),
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

/** This installation's ID for Sign in with ChatGPT (`ext_agent_host_id`): a random
 * UUID made once and kept, the same for every sign-in from this browser profile; never
 * the user's identifier, and no credential. */
let agentHostMaking = null;
function agentHost() {
  agentHostMaking ??= (async () => {
    const kept = await read('kv', 'agent-host');
    if (typeof kept === 'string' && /^urn:uuid:[0-9a-f-]{36}$/.test(kept)) return kept;
    const made = `urn:uuid:${crypto.randomUUID()}`;
    await writeDurably('kv', 'agent-host', made);
    return made;
  })().finally(() => {
    agentHostMaking = null;
  });
  return agentHostMaking;
}

/** The Sign in with ChatGPT registration kept for `provider` (`{clientId, subject,
 * account, declined?}`), or null: it outlives a sign-out, so signing in again is the same
 * client; a sign-in with another account replaces it. */
async function registrationOf(provider) {
  const kept = await read('kv', 'signin-registrations');
  const registration = kept?.[provider];
  return registration && typeof registration.clientId === 'string' ? registration : null;
}

function keepRegistration(provider, registration) {
  return merge('kv', 'signin-registrations', { [provider]: registration }, (value, old) => ({ ...(old ?? {}), ...value }));
}

/** A URL's origin, or null. */
function originOf(url) {
  try {
    return new URL(url).origin;
  } catch {
    return null;
  }
}

/** The sites scripts may call, each with the names of its secret headers (`secret`). */
async function sitesKept() {
  const [sites, secrets] = await Promise.all([read('kv', 'sites'), read('kv', 'site-secrets')]);
  return withSecrets(sites ?? [], secrets ?? {});
}

/** `sites` (kv/sites: grants as the package takes them, so a build from before secret
 * headers still reads them) with each one's secret header names as `secret`
 * (kv/site-secrets: by site name, `{url, names}`; only for the address they were saved
 * for, so a name an earlier build gave another address since never carries them). */
function withSecrets(sites, secrets) {
  return sites.map((grant) => {
    const kept = secrets[grant.name];
    return kept?.url === grant.url && kept.names?.length ? { ...grant, secret: kept.names } : grant;
  });
}

/** Sites (with `secret`) as the package takes them: a site's secret headers are named as
 * ones the app adds. */
function grantsFor(sites) {
  return sites.map(({ secret, ...grant }) => (secret?.length
    ? { ...grant, headers: Object.fromEntries(secret.map((name) => [name, { app: true }])) }
    : grant));
}

/** Whether `url` is under the site address `prefix`, as the package matches them: the
 * same origin, and the path under the prefix on a segment boundary. */
function underPrefix(prefix, url) {
  try {
    const site = new URL(prefix);
    const asked = new URL(url);
    const path = site.pathname;
    return site.origin === asked.origin
      && (path.endsWith('/') ? asked.pathname.startsWith(path) : asked.pathname === path || asked.pathname.startsWith(`${path}/`));
  } catch {
    return false;
  }
}

/** The secret headers of the site a script's request goes to (its grant, `grant`), from
 * the vault: only to the address they were saved for. Throwing refuses the request
 * (nothing is sent); the client takes every echo of them out of what comes back. */
async function grantHeaders({ grant, url }) {
  const site = (await sitesKept()).find((known) => known.name === grant);
  const where = site ? new URL(site.url).host : grant;
  if (!site?.secret?.length) throw new Error(`${where} no longer has a secret header in Settings, Sites scripts may call`);
  const held = await vault.siteSecret(grant);
  if (!held) throw new Error(`the secret header saved for ${where} is gone (Spindle's keys were reset): add the site again with it in Settings`);
  if (held.url !== site.url || !underPrefix(site.url, url)) {
    throw new Error(`the secret header saved for ${where} is for another address: add the site again with it in Settings`);
  }
  return held.headers;
}

/** Header names a site's secret may not take: those Spindle sets, and those Chrome drops
 * from a script's request without a word. */
const UNSENDABLE = new Set([
  'host', 'content-length', 'transfer-encoding', 'connection', 'upgrade', 'te', 'trailer', 'keep-alive', 'expect',
  'accept-charset', 'accept-encoding', 'access-control-request-headers', 'access-control-request-method',
  'access-control-request-private-network', 'cookie', 'cookie2', 'date', 'dnt', 'origin', 'referer', 'set-cookie', 'via',
]);
/** The fewest characters of a secret Spindle keeps out of what scripts see. */
const SECRET_MIN = 8;

/** A secret header as Settings gives it (`{name, value}`), checked: a name a request can
 * carry, and a value of visible ASCII (the bytes sent are its own) whose secret part
 * (after an auth scheme such as `Bearer`) is long enough to find in what comes back. */
function secretHeader(header) {
  const name = String(header?.name ?? '').trim();
  const value = String(header?.value ?? '').trim();
  const lower = name.toLowerCase();
  if (!/^[!#$%&'*+.^_`|~0-9A-Za-z-]{1,128}$/.test(name)) {
    throw failure('invalid', 'A header name is letters, digits and dashes, such as Authorization or X-Api-Key.');
  }
  if (UNSENDABLE.has(lower) || lower.startsWith('proxy-') || lower.startsWith('sec-')) {
    throw failure('invalid', `${name} is set by ${BROWSER} or Spindle, not by a site's secret: use the header the site's API asks for, such as Authorization.`);
  }
  if (!/^[\t\x20-\x7e]{1,8192}$/.test(value)) {
    throw failure('invalid', "A secret header's value is the text the site gave you (letters, digits, symbols and spaces), at most 8 KiB.");
  }
  const secret = /^[!#$%&'*+.^_`|~0-9A-Za-z-]+[ \t]+(\S[\s\S]*)$/.exec(value)?.[1] ?? value;
  if (secret.length < SECRET_MIN) {
    throw failure('invalid', `That secret is too short to keep out of what scripts see: ${SECRET_MIN} characters at least.`);
  }
  return { name, value };
}

/** Changes to the sites scripts may call, one after another (two panels at once lose
 * neither's change, nor a secret header). */
let siteChanges = Promise.resolve();

/** Adds a site scripts may call (an address already listed is updated: its access, and
 * its secret header when one is given, else the one it has is kept), or removes one
 * (`name`) with its secret header. */
async function changeSites(add, name) {
  const now = await sitesKept();
  let next = now.filter((grant) => grant.name !== name);
  const gone = now.find((grant) => grant.name === name);
  let named = null;
  let sealed = null;
  if (add) {
    const url = new URL(add.url);
    const header = add.header ? secretHeader(add.header) : null;
    const same = now.find((grant) => grant.url === url.href);
    named = same?.name;
    if (!named) {
      // A grant's name: its host, told apart when two share one.
      const host = url.hostname.replace(/[^A-Za-z0-9.-]/g, '-').slice(0, 56) || 'site';
      named = host;
      for (let n = 2; now.some((grant) => grant.name === named); n += 1) named = `${host}-${n}`;
    }
    const secret = header ? [header.name] : same?.secret;
    const entry = {
      name: named, url: url.href, risk: add.write ? 'write' : 'read',
      methods: add.write ? ['GET', 'HEAD', 'POST', 'PUT', 'PATCH', 'DELETE'] : ['GET', 'HEAD'],
      ...(secret?.length && { secret }),
    };
    next = same ? now.map((grant) => (grant === same ? entry : grant)) : [...now, entry];
    if (header) {
      // Over plain HTTP anyone on the network could read it: this computer's own
      // addresses aside, a secret goes only over HTTPS.
      const loopback = ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname) || url.hostname.endsWith('.localhost');
      if (url.protocol !== 'https:' && !loopback) {
        throw failure('invalid', `A secret header goes only to an https:// address (or this computer's own), so no one on the network can read it: use https://${url.host}${url.pathname}`);
      }
      // Kept sealed, so not while the keys are locked.
      if (!(await vault.usableNow())) throw failure('locked', "Spindle's keys are locked: unlock them in Settings, Providers, then add the site's secret header");
      sealed = { url: url.href, headers: { [header.name]: header.value } };
    }
  }
  // The package checks the grants first (and refuses ones that are not valid) before
  // they are kept; the list and a secret header go in one transaction.
  client.setHttpGrants(grantsFor(next));
  try {
    const secrets = Object.fromEntries(next.filter((grant) => grant.secret?.length)
      .map((grant) => [grant.name, { url: grant.url, names: grant.secret }]));
    const kept = [['kv', 'sites', next.map(({ secret, ...grant }) => grant)], ['kv', 'site-secrets', secrets]];
    if (sealed) await vault.keepSite(named, sealed, kept);
    else if (gone) await vault.forgetSite(gone.name, kept);
    else await writeTogether(kept);
  } catch (error) {
    client.setHttpGrants(grantsFor(now));
    throw error;
  }
  send({ type: 'sites', sites: next });
  if (sealed || gone?.secret) announceVault();
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

/** A page tool's input as Chrome's side gets it (`{input}`), or why it may not go
 * (`{error}`): a workspace file to open named by this thread (`opened`; a file viewer
 * address naming another thread's is refused), and for `select` with `files`, the files
 * themselves (`uploadOf`; an `upload` the model wrote itself never passes). */
async function prepared(name, input, call) {
  const id = idOf(String(call?.session ?? '').split('#')[0]);
  const given = opened(name, input, call?.session);
  if (name === 'navigate' && typeof given?.url === 'string' && given.url.startsWith(new URL('view.html?', self.location.href).href)) {
    if (new URL(given.url).searchParams.get('thread') !== id) return { error: "that address opens another thread's file: open a file of this thread's workspace with /workspace/NAME" };
  }
  if (name !== 'select' || !given || typeof given !== 'object') return { input: given };
  const { upload: _, ...rest } = given;
  if (rest.files == null) return { input: rest };
  const found = await uploadOf(rest.files, id, call).catch((error) => ({ error: `the files could not be read: ${error?.message ?? error}` }));
  return found.error != null ? found : { input: { ...rest, upload: found.upload } };
}

/** The files `select` puts in a field (`paths` in thread `id`'s workspace, here or on the
 * host), read when the model asks so the bytes the user is asked about are the bytes
 * that go: `{upload: [{path, name, type, size, base64}]}` or `{error}`. Not from a script
 * (a command writes its files back only when it ends, so its call would find older ones)
 * nor a task (its workspace comes back to the thread when it ends); within `UPLOAD`. */
async function uploadOf(paths, id, call) {
  const mb = (bytes) => `${Math.ceil(bytes / 104857.6) / 10} MB`;
  if (SCRIPT_CALL.test(String(call?.call ?? ''))) return { error: 'a script cannot put files in a page (its command writes its files back only when it ends): call select with files yourself, after the command' };
  if (ownedByTask(call)) return { error: 'a task cannot put files in a page: its thread can, once the task has ended and its files are back' };
  if (!id) return { error: 'files go in a page only from a thread' };
  if (!Array.isArray(paths) || !paths.length || paths.length > UPLOAD.count) return { error: `give 1 to ${UPLOAD.count} files` };
  const wanted = paths.map((path) => String(path).trim().replace(/^\/workspace\//, '').replace(/^\.\//, ''));
  const bad = wanted.find((path) => !path || path.startsWith('/') || path.split('/').some((part) => !part || part === '.' || part === '..'));
  if (bad != null) return { error: `${JSON.stringify(bad)} is no file of the workspace: give its path there, such as resume.pdf` };
  const sizes = new Map(((await ops.files({ id }))?.files ?? []).map((file) => [file.path, file.size]));
  const missing = wanted.find((path) => !sizes.has(path));
  if (missing != null) return { error: `the workspace has no file ${missing}` };
  const big = wanted.find((path) => sizes.get(path) > UPLOAD.file);
  if (big != null) return { error: `${big} is ${mb(sizes.get(big))}: at most ${mb(UPLOAD.file)} a file` };
  if (wanted.reduce((sum, path) => sum + sizes.get(path), 0) > UPLOAD.total) return { error: `the files come to more than ${mb(UPLOAD.total)}, the most one select takes` };
  const upload = [];
  let total = 0;
  for (const path of wanted) {
    // Checked again as read (a file may have grown since it was listed).
    const { bytes } = await ops.read({ id, path });
    total += bytes.length;
    if (bytes.length > UPLOAD.file || total > UPLOAD.total) return { error: `${bytes.length > UPLOAD.file ? `${path} is ${mb(bytes.length)}: at most ${mb(UPLOAD.file)} a file` : `the files come to more than ${mb(UPLOAD.total)}, the most one select takes`}` };
    // A type Spindle does not know goes as none, as Chrome gives one picked in its dialog.
    const type = typeOf(path);
    upload.push({ path, name: path.split('/').pop(), type: type === 'application/octet-stream' ? '' : type, size: bytes.length, base64: base64Of(bytes) });
  }
  return { upload };
}

/** Whether an app call is a task's: its call named under its task (`tasks/KEY/...`, as a
 * task's calls come from the package), or asked in a task's session (`dir#KEY`, a script's).
 * A thread's own turn ending or stopped leaves its background tasks' page tools be: the
 * package ends a task's call itself when it stops the task (`call.signal`). */
const ownedByTask = (call) => String(call?.call ?? '').startsWith('tasks/') || String(call?.session ?? '').includes('#');

/** Each thread's page tool generations: `all` moves on when every call of the thread
 * ends (deleted, moved, its host changed), `own` when its own turn's do (Stop, the turn's
 * end); a call started under one generation that has moved on does nothing. */
const pageEpochs = new Map();
function pageEpoch(session, task = false) {
  const epoch = pageEpochs.get(idOf(session?.split('#')[0] ?? '')) ?? { all: 0, own: 0 };
  return task ? `${epoch.all}` : `${epoch.all}.${epoch.own}`;
}
/** Whether the thread's turn was stopped: here, that holds back its own calls (a task's
 * go on: see `ownedByTask`); on the host, every call (the host tells no call's end). */
function pageStopped(session, task = false) {
  const id = idOf(session?.split('#')[0] ?? '');
  return hostedTurns.get(id)?.stopped || (!task && running.get(id)?.stopped);
}
/** Why a call's `signal` ended: the package settled it without this answer (its reason's
 * `outcome`: `stopped` when its turn or task was stopped), or the command of the script
 * that asked ended (an abort with no outcome). */
function endedWhy(signal) {
  const outcome = signal?.reason?.outcome;
  if (outcome === 'stopped') return 'the turn or task that asked for it was stopped';
  return outcome ? 'Spindle stopped waiting for it' : 'the command that asked for it ended';
}

/** `allowed`: what the user was shown when asked before this action (null: not asked);
 * Chrome's side acts only while the tab still shows it. `signal`: ends the call (its
 * page action is cancelled, its outcome then unknown); `task`: a task's (`ownedByTask`). */
function pageTool(name, input, session, allowed = null, signal = null, task = false) {
  if (pageStopped(session, task)) return Promise.resolve({ error: 'the turn was stopped' });
  if (signal?.aborted) return Promise.resolve({ error: `${endedWhy(signal)} first, so it was not done` });
  const call = crypto.randomUUID();
  if (task && OPENS_TABS.has(name)) tasksOpening.add(idOf(session?.split('#')[0] ?? ''));
  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      self.postMessage({ type: 'page_tool_cancel', call });
      answerTool(call, uncertain(name, `${BROWSER} did not answer the page tool in time`));
    }, PAGE_TOOL_WAIT);
    const stop = () => {
      self.postMessage({ type: 'page_tool_cancel', call });
      answerTool(call, uncertain(name, `${endedWhy(signal)} before the page tool finished`));
    };
    toolCalls.set(call, Object.assign((answer) => {
      clearTimeout(timer);
      signal?.removeEventListener('abort', stop);
      resolve(answer);
    }, { session, tool: name, task }));
    signal?.addEventListener('abort', stop, { once: true });
    // Its thread: tabs the call opens are that thread's, closed when it is done (`tabsDone`).
    self.postMessage({ type: 'page_tool', call, name, input, thread: idOf(session?.split('#')[0] ?? ''), task: Boolean(task), ...(allowed && { allowed }) });
  });
}

/** The page tools whose call may open a tab (navigate's new_tab, or a page opening one). */
const OPENS_TABS = new Set(['navigate', 'click', 'press', 'type', 'select', 'dialog']);
/** Threads whose background tasks may have opened tabs not yet closed. */
const tasksOpening = new Set();

/** Thread `id`'s turn is done (or the thread deleted): the tabs Spindle opened for its
 * turns close (`tasks`: its background tasks' too). */
function tabsDone(id, { tasks = false } = {}) {
  self.postMessage({ type: 'tabs_done', thread: id, tasks });
}

/** Whether a background task is still at work, as its row says: here `pending`; a
 * native host's rows say where the task's session stands (finished or stopped: done). */
const atWork = (task) => task.pending ?? !(task.status?.complete === true || task.status?.stop != null);

/** A turn of thread `id` completed (`tasksOf`: its task rows, here or on the host): the
 * tabs its turns opened close, and its tasks' once none of them is still at work (a
 * task's tab may still be in use; a list that cannot be read keeps them). */
async function turnTabsDone(id, tasksOf = () => client.tasks(dirOf(id))) {
  let tasks = false;
  // Taken out while the rows are read: a task's call meanwhile puts it back.
  if (tasksOpening.delete(id)) {
    try {
      tasks = !((await tasksOf()).tasks ?? []).some(atWork);
    } catch {}
    if (!tasks) tasksOpening.add(id);
  }
  tabsDone(id, { tasks });
}

/** How long a page action the user is asked about waits for their answer. */
const APPROVAL_WAIT = 10 * 60_000;
/** Page actions waiting for the user's answer (Settings, Pages, Before acting): id ->
 * `{id, thread, tool, text, at, task, answer}` (`task`: a task's, `ownedByTask`);
 * `answer(refusal)` settles one (null: go ahead). */
const approvals = new Map();

/** The waiting actions as panels show them. */
function approvalsShown() {
  return [...approvals.values()].map(({ answer, task, ...shown }) => shown);
}

function announceApprovals() {
  send({ type: 'approvals', approvals: approvalsShown() });
}

/** A page tool for a thread: at once under Full; under Auto (the default) or Ask once the
 * user allows it when it is one they asked to be asked about. A refusal is a known
 * outcome: nothing was done. */
async function actOnPage(name, input, session, signal, task = false, epoch = pageEpoch(session, task)) {
  const asked = { task };
  const refusal = await beforeActing(name, input, session, signal, asked);
  if (epoch !== pageEpoch(session, task) || pageStopped(session, task)) return { error: 'the turn ended or was stopped before the page tool started' };
  // A call ended meanwhile (a script's command, or the package cancelled it): no one
  // awaits it.
  if (signal?.aborted) return { error: `${endedWhy(signal)} first, so it was not done` };
  return refusal ? { error: refusal } : pageTool(name, input, session, asked.allowed, signal, task);
}

/** Null when `name` with `input` may go ahead now; else, once the user answers (or does
 * not, or the turn stops), null to go ahead or why it was not done. `asked.task`: a
 * task's call (`ownedByTask`). Allowed by the user, what they were shown goes in
 * `asked.allowed` (the tab, its site and page, the part or the dialog), for Chrome's side
 * to act only while it still holds. */
async function beforeActing(name, input, session, signal, asked = {}) {
  const task = Boolean(asked.task);
  const epoch = pageEpoch(session, task);
  if (signal?.aborted) return `${endedWhy(signal)} first, so it was not done`;
  if (pageStopped(session, task)) return 'the turn was stopped, so it was not done';
  if (!ASKING_NAMES.has(name)) return null;
  // Opening a file of the thread's workspace for the user changes no page.
  if (name === 'navigate' && typeof input?.url === 'string' && input.url.startsWith(new URL('view.html?', self.location.href).href)) return null;
  // The user's choice, read from storage: one that cannot be read asks (never acts
  // unasked on a choice it could not see).
  const kept = await read('kv', 'pages').then((value) => ({ value }), () => null);
  const mode = kept ? confirmMode(kept.value) : 'ask';
  if (epoch !== pageEpoch(session, task) || pageStopped(session, task)) return 'the turn ended or was stopped, so it was not done';
  if (mode === 'full') return null;
  // Under Auto, an action that sends nothing goes ahead unasked, nothing looked up (what
  // a click lands on tells whether it sends, so that one is looked up first).
  if (mode === 'auto' && name !== 'click' && !sends(name, input, null)) return null;
  let target = null;
  if (input?.ref && (name === 'click' || name === 'type' || name === 'select')) {
    const found = await pageTool('target', { ref: input.ref, ...(input.tab != null && { tab: input.tab }) }, session, null, signal, task);
    target = found?.details ?? null;
  } else if (name === 'press' || name === 'dialog' || name === 'close_tab' || (name === 'navigate' && !input?.new_tab)) {
    const found = await pageTool('tab_of', { ...(input?.tab != null && { tab: input.tab }), ...(name === 'press' && { focus: true }) }, session, null, signal, task).catch(() => null);
    target = found?.details ?? null;
    // Which tab it would land on could not be told: asked about no tab, it would go to
    // whatever tab is shown when it runs. Not done; it can be asked about again.
    if (!target && input?.tab == null) return `${BROWSER} did not say which tab this would act on, so it was not done; it can be asked about again`;
  }
  if (mode === 'auto' && !sends(name, input, target)) return null;
  // Asked about one tab, it acts on that tab, whatever the user looks at when they answer.
  if (target?.tab != null && input && typeof input === 'object') input.tab = target.tab;
  const thread = idOf(session?.split('#')[0] ?? '');
  if (epoch !== pageEpoch(session, task) || pageStopped(session, task)) return 'the turn ended or was stopped, so it was not done';
  if (signal?.aborted) return `${endedWhy(signal)} first, so it was not done`;
  const answer = await new Promise((resolve) => {
    const id = crypto.randomUUID();
    const settle = (refusal) => {
      if (!approvals.has(id)) return;
      clearTimeout(timer);
      approvals.delete(id);
      announceApprovals();
      resolve(refusal);
    };
    const timer = setTimeout(() => settle(`the user did not answer within ${APPROVAL_WAIT / 60_000} minutes (Settings, Pages, Before acting asks first), so it was not done`), APPROVAL_WAIT);
    // A task's question says so (it may go on after its thread's turn has stopped).
    approvals.set(id, { id, thread, tool: name, text: `${task ? 'A task asks: ' : ''}${actionText(name, input, target)}`, at: Date.now(), task, answer: settle });
    // A call ended first (a script's command, or the package cancelled it): the question
    // goes, unanswered.
    signal?.addEventListener('abort', () => settle(`${endedWhy(signal)} before the user answered, so it was not done`), { once: true });
    announceApprovals();
  });
  if (answer == null && target) {
    asked.allowed = {
      tab: target.tab ?? null, host: target.host ?? null,
      ...(target.document && { document: target.document }), ...(target.url != null && { url: target.url }),
      // The part as shown: its description, words and where it leads (a link's address,
      // the address a submit button sends its form to).
      ...(input?.ref && target.what && { what: target.what, words: target.words ?? '', dest: target.dest ?? null }),
      ...(name === 'dialog' && { dialog: target.dialog?.id ?? 0 }),
      // A key goes where the focus is: to the part the user was shown having it, or none.
      ...(name === 'press' && 'focus' in target && { focus: target.focus?.ref ?? null }),
    };
  }
  return answer;
}

/** Ends thread `id`'s page tools: its own turn's (`tasks: false`: the turn ended or was
 * stopped; its background tasks' calls go on, the package ending a task's itself), or
 * every one (the thread deleted, moved back, its host changed). */
function cancelPageTools(id, { tasks = true } = {}) {
  const epoch = pageEpochs.get(id) ?? { all: 0, own: 0 };
  pageEpochs.set(id, tasks ? { ...epoch, all: epoch.all + 1 } : { ...epoch, own: epoch.own + 1 });
  // An action still waiting for the user's answer is not done.
  for (const approval of [...approvals.values()]) {
    if (approval.thread === id && (tasks || !approval.task)) approval.answer('the turn ended before the user answered, so it was not done');
  }
  for (const [call, answer] of toolCalls) {
    if (idOf(answer.session?.split('#')[0] ?? '') !== id || (!tasks && answer.task)) continue;
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
  // `sending`: the work got past its last check and handed the message to the package.
  const entry = { stopped: false, sending: false };
  running.set(id, entry);
  entry.done = (async () => {
    let unsent = [];
    let failed = false;
    let completed = false;
    const before = await read('threads', id).catch(() => null);
    try {
      const thread = await touch(id, { state: 'running', note: null });
      // Its commands run on the user's host: once that host has connected (or failed to).
      if (thread?.runner === 'remote') await hostChanges;
      const notYet = () => Object.assign(failure('stopped', 'the turn was stopped before it started'), { beforeStart: true });
      if (entry.stopped) throw notYet();
      const result = await work(() => {
        if (entry.stopped) throw notYet();
        entry.sending = true;
      });
      const cut = result.stopped && !result.stopped.complete;
      unsent = result.unsent ?? [];
      const note = cut
        ? result.stopped.reason
        : unsent.length ? `${unsent.length} message(s) came after the turn's last step and were not read` : null;
      await touch(id, { state: cut ? 'stopped' : 'idle', note });
      completed = !cut;
    } catch (error) {
      if (error.beforeStart) {
        // Stopped before the message went: the thread is as it was (the message comes
        // back, `afterTurn`).
        const was = before && before.state !== 'running' ? before : { state: 'idle', note: null };
        await touch(id, { state: was.state, note: was.note ?? null });
      } else {
        failed = error.kind !== 'stopped';
        await touch(id, { state: error.kind === 'stopped' ? 'stopped' : 'failed', note: String(error.message ?? error) });
      }
    } finally {
      running.delete(id);
      // Its own page tools end with it; its background tasks' go on.
      cancelPageTools(id, { tasks: false });
      const took = carrying.get(id);
      carrying.delete(id);
      await afterTurn(id, unsent, took, { sending: entry.sending, failed, completed }).catch((error) => console.warn('Spindle: after a turn:', error));
    }
  })();
}

/** What follows a turn of a thread here. The waiting message it took (`took`) leaves the
 * disk once the thread holds it; a turn that ended without taking it (stopped before it
 * started, say) puts it back, held, saying why. Messages the turn did not read go with a
 * message waiting for its end, before it; then that message goes. A `completed` turn
 * after which the thread rests (no message goes on at once, none it did not read is
 * held) closes the tabs it opened (`turnTabsDone`); otherwise they stay, to go on in. */
async function afterTurn(id, unsent, took, { sending, failed, completed = false }) {
  if (took) {
    // Not handed over: not taken. Handed over to a turn that went on: taken (the package
    // commits the message first). Handed over to one that failed: its history tells.
    const taken = sending && (!failed || (await tookMessage(id, took).catch(() => false)));
    if (taken) await dropQueued(took).catch(() => {});
    else await requeue(id, took, sending ? 'its turn failed before taking it' : 'you stopped its turn before it began');
  }
  await holdUnsent(id, unsent);
  send({ type: 'turn_end', thread: id, unsent: [] });
  const waiting = queued.get(id);
  if (waiting && !waiting.held) sendQueued(id);
  else {
    if (completed && !unsent?.length) turnTabsDone(id).catch(() => {});
    rest(id);
  }
}

/** Messages a turn ended before reading: with a message waiting for that end, they go
 * with it, before it; otherwise they wait, held, to be sent or taken back (on disk, so
 * neither a closed panel nor a stopped engine loses them). */
async function holdUnsent(id, unsent) {
  if (!unsent?.length) return;
  const waiting = queued.get(id);
  if (waiting && !waiting.held) {
    waiting.text = [...unsent, waiting.text].join('\n\n');
    await keepQueued(waiting).catch(() => {});
    announceQueued(id);
    return;
  }
  await requeue(id, waitingMessage(id, unsent.join('\n\n'), []), 'the turn ended before reading it')
    .catch((error) => console.warn('Spindle: an unread message could not be kept:', error));
}

/** What panels show of a thread's waiting message (null: none). */
function queuedOf(id) {
  const waiting = queued.get(id);
  return waiting ? { text: waiting.text, names: waiting.files.map((file) => String(file.path)), held: waiting.held ?? null } : null;
}

function announceQueued(id) {
  send({ type: 'queued', thread: id, queued: queuedOf(id) });
}

const QUEUED = 'queued:';
/** A message to wait for thread `id`'s turn to end (its disk record's own key). */
function waitingMessage(id, text, files) {
  return { key: `${QUEUED}${id}:${crypto.randomUUID()}`, thread: id, text: String(text), files: files ?? [], held: null, at: Date.now() };
}
/** Writes a waiting message's record as it is now. */
function keepQueued(waiting) {
  return writeDurably('kv', waiting.key, { thread: waiting.thread, text: waiting.text, files: waiting.files, held: waiting.held ?? null, at: waiting.at });
}
/** Removes a waiting message's record. */
function dropQueued(waiting) {
  return removeDurably('kv', waiting.key);
}
/** Queues `waiting` for its thread: on disk before any panel is told it waits (a write
 * that fails leaves nothing queued, and the panel keeps the draft). */
async function enqueue(waiting) {
  const id = waiting.thread;
  if (queued.has(id)) throw failure('busy', 'a message already waits for this turn to end: take it back to change it');
  queued.set(id, waiting);
  try {
    await keepQueued(waiting);
  } catch (error) {
    if (queued.get(id) === waiting) queued.delete(id);
    throw error;
  }
  announceQueued(id);
}
/** Puts `took` (a waiting message no turn took) back for thread `id`, held for `why`: in
 * front of one that waits now, if one does. */
async function requeue(id, took, why) {
  const there = queued.get(id);
  if (there) {
    there.text = `${took.text}\n\n${there.text}`;
    there.files = [...took.files, ...there.files];
    await keepQueued(there);
    await dropQueued(took).catch(() => {});
  } else {
    took.held = why;
    queued.set(id, took);
    await keepQueued(took);
  }
  announceQueued(id);
}
/** Whether thread `id` (here) holds `waiting` as one of its latest user messages: the
 * turn that took it committed it. */
function tookMessage(id, waiting) {
  const said = waiting.text + attachedNote(waiting.files);
  return within(id, async () => {
    const messages = (await client.history(dirOf(id))).messages ?? [];
    return messages.slice(-64).some((message) => message.role === 'user'
      && (message.content ?? []).some((block) => block.type === 'text' && block.text === said));
  });
}
/** Waiting messages a stopped engine left on disk (at start). One its turn had already
 * taken goes; any other waits again, held, for the user to send or take back. */
async function restoreQueued() {
  const records = [];
  for (const key of await keysOf('kv')) {
    // A message that came back for a thread since deleted (its panel closed first): gone
    // with it (panels keep the others, `came-back:ID`, until that thread is opened).
    const cameBack = /^came-back:([0-9a-f]{16})$/.exec(String(key));
    if (cameBack && !(await read('threads', cameBack[1]).catch(() => true))) await remove('kv', key).catch(() => {});
    if (!String(key).startsWith(QUEUED)) continue;
    const record = await read('kv', key).catch(() => null);
    if (record) records.push({ ...record, key: String(key) });
  }
  records.sort((one, other) => (one.at ?? 0) - (other.at ?? 0));
  for (const record of records) {
    const id = record.thread;
    const thread = typeof id === 'string' ? await read('threads', id).catch(() => null) : null;
    if (!thread) {
      await removeDurably('kv', record.key).catch(() => {});
      continue;
    }
    const waiting = { key: record.key, thread: id, text: String(record.text ?? ''), files: record.files ?? [], held: null, at: record.at ?? 0 };
    if (!thread.home && await tookMessage(id, waiting).catch(() => false)) {
      await removeDurably('kv', record.key).catch(() => {});
      continue;
    }
    await requeue(id, waiting, record.held ?? `${BROWSER} stopped before the turn it waited for ended`).catch((error) => {
      console.warn('Spindle: a waiting message could not be restored:', error);
    });
  }
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
    // A turn that did not finish (stopped, say) takes no new turn until it is retried,
    // continued or taken back; one stopped part way takes the text alone, not the files.
    if (status.accepts_prompt === false) {
      throw failure('busy', status.accepts_message
        ? 'the last turn was stopped part way and takes a message without files: send the text alone, or Continue, Retry or Take back first'
        : `the last turn did not finish${status.stop_text ? ` (${status.stop_text})` : ''}: retry, continue or take it back first`);
    }
    const { files, note } = attachments(waiting.files);
    turn(id, async (check) => {
      await freshen(dirOf(id));
      check();
      return client.prompt(dirOf(id), waiting.text + note, files ? { files } : {});
    });
    // Out of the queue once its turn has started (until then a failure holds it); its
    // record stays until that turn has ended (`afterTurn`).
    carrying.set(id, waiting);
    queued.delete(id);
    announceQueued(id);
  } catch (error) {
    if (queued.get(id) !== waiting) return;
    waiting.held = String(error.message ?? error);
    await keepQueued(waiting).catch(() => {});
    announceQueued(id);
    rest(id);
  }
}

/** Whether the thread's turn was stopped between its steps (a stop recorded the call it
 * cut off) in a step its first request made: no point before that request to ask again
 * from (the store's Retry says so), so Retry starts the turn over. */
async function firstStepStopped(id) {
  return within(id, async () => {
    const dir = dirOf(id);
    const status = await client.status(dir);
    if (status.accepts_prompt || !status.accepts_message || status.phase === 'model_wait') return false;
    if ((await client.turn(dir)).completed) return false;
    const messages = (await client.history(dir)).messages ?? [];
    const replies = messages.filter((message) => message.role === 'assistant');
    const calling = replies.filter((message) => message.content?.some((block) => block.type === 'tool_use'));
    return replies.length > 0 && calling.at(-1) === replies[0];
  });
}

/** Why a stopped turn can neither go on nor start over now (its earlier turns cannot be
 * gone back to): a background task still at work holds every point of a thread that
 * has tasks. Nothing is changed. */
async function heldBack(dir) {
  const working = ((await client.tasks(dir).catch(() => null))?.tasks ?? []).filter((task) => task.pending).length;
  return failure('busy', working
    ? `${working === 1 ? 'a background task of this thread is' : `${working} background tasks of this thread are`} still at work, and meanwhile its earlier turns cannot be gone back to: wait for ${working === 1 ? 'it' : 'them'} or stop ${working === 1 ? 'it' : 'them'}, then try again. Nothing was changed`
    : "this thread's earlier turns cannot be gone back to now: Retry or Continue it instead. Nothing was changed");
}

function notRunning(id, doing) {
  if (running.has(id)) throw failure('busy', `stop this thread before ${doing}`);
}

/** A thread that can take work: it exists and has not moved out. */
async function workable(id) {
  const thread = await read('threads', id);
  if (!thread) throw failure('not_found', 'there is no such thread');
  if (thread.state === 'moved') throw failure('moved', 'this thread moved into a bundle file; import the file to go on with it');
  if (moving.has(id)) throw failure('busy', 'this thread is moving into a file');
  // Moving to or from the user's host: it takes no work until the move ends (but for
  // its last step, once it is live here again).
  if (thread.moving && thread.moving.step !== 'release') {
    throw failure('busy', `${STEP_TEXT[thread.moving.step] ?? 'It is moving'}; it takes no work until that is done`);
  }
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
  const info = await client.info();
  const catalogSaved = Boolean(await read('kv', 'catalog'));
  const sites = await sitesKept();
  const standing = (await read('kv', 'standing')) ?? '';
  const relay = (await read('kv', 'relay')) ?? null;
  const vault = await vaultState();
  // Sign in with ChatGPT registrations kept (outliving a sign-out), by provider: the
  // account each is for (a client ID is no secret, but the panel needs only that).
  const registrations = Object.fromEntries(Object.entries((await read('kv', 'signin-registrations')) ?? {})
    .map(([provider, registration]) => [provider, typeof registration?.account === 'string' ? registration.account : null]));
  // The thread list is read last, and nothing is awaited after it: a panel replaces its
  // whole list with this one, so a thread that changed (and was announced) while the
  // reads above waited must be in it. A change committed after this read is announced
  // after this answer goes, and the panel applies it on top.
  const list = await threads();
  return {
    info,
    catalog,
    catalogSaved,
    catalogNote,
    threads: list,
    running: [...running.keys()],
    free: pool?.free ?? null,
    linux: linuxState,
    host: hostState,
    sites,
    approvals: approvalsShown(),
    standing,
    relay,
    vault,
    registrations,
  };
}

/** The vault as panels see it; one that cannot be read still lets Settings open. */
function vaultState() {
  return vault.state().catch((error) => ({
    mode: 'device', locked: false, kinds: {}, accounts: {}, ended: {}, sites: [], error: String(error?.message ?? error),
  }));
}

/** Tells every panel the vault changed (another window's panel keeps up). */
function announceVault() {
  vaultState().then((state) => send({ type: 'vault', vault: state }));
}

/** The last turnFiles answer, by thread and revision (asked again at every turn's end). */
let turnFilesKept = { key: null, value: null };

/** Said when the messages before a summary are asked for while the thread works: a
 * store lists no points to go back to meanwhile. */
const NOT_WHILE_WORKING = 'The messages before the summary can be shown once this thread stops working';

/** Lets what waits on the engine go first (a panel's `ping`, another thread's work)
 * between the reads of a long search: each history read holds the engine's one wasm
 * thread for as long as it takes (tens of milliseconds in a long thread). At most every
 * 20 ms: Chrome holds a timer set from a timer back 4 ms, so one per read of a short
 * history would make a search of thousands of them seconds slower. (By the monotonic
 * clock: a wall clock set back would stop them until it caught up.) */
let othersWent = -Infinity;
async function letOthersGo() {
  if (performance.now() - othersWent < 20) return;
  await new Promise((done) => setTimeout(done, 0));
  othersWent = performance.now();
}

/** The rewind target whose history is `shown`'s first `at` messages: the point just
 * before message `at`, among the last 8192 steps; null when none is. A history only
 * grows until a compaction starts it again from a summary, so each history since both
 * message `at` and `shown`'s last compaction begins as `shown` does through the
 * message, and no other one does. Halving the targets (newest first) on that finds the
 * newest whose history does not, in a few reads however many steps the thread took;
 * compared whole, it is the point before the message, or there is none (as when the
 * turn compacted after its message). Reading every target took over a minute in a long
 * thread, the engine answering nothing meanwhile. A rewind past the message and on
 * again breaks that order and can hide the point: `thorough` then reads every target,
 * newest first (its length first, the whole only for one of that length), as Rewind
 * needs; without it (marking what a turn changed) the point is then not found. */
async function pointBefore(dir, shown, at, { thorough = false } = {}) {
  const before = JSON.stringify(shown.slice(0, at));
  const through = JSON.stringify(shown.slice(0, at + 1));
  const targets = await client.targets(dir, { limit: 8192 });
  const begun = (messages) => messages.length > at && JSON.stringify(messages.slice(0, at + 1)) === through;
  // The histories of targets before `low` begin so, those from `high` on do not;
  // `without` is the one at targets[high], once one is read.
  let low = 0;
  let high = targets.length;
  let without = null;
  while (low < high) {
    const middle = (low + high) >> 1;
    await letOthersGo();
    const messages = (await client.history(dir, targets[middle].revision)).messages ?? [];
    if (begun(messages)) {
      low = middle + 1;
    } else {
      high = middle;
      without = messages;
    }
  }
  if (without && JSON.stringify(without) === before) return targets[high].revision;
  if (!thorough) return null;
  for (const target of targets) {
    await letOthersGo();
    const { total } = await client.history(dir, target.revision, { from: at });
    if (total !== at) continue;
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
  // Named apart from threads: one a stop left behind is cleared at start, never listed.
  const probe = `probe-${newId()}`;
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
  // (Its size kept, not the bytes: they go once written.)
  const size = await client.exportSession(dirOf(id), { keep: true, whole: true })
    .then(async (bytes) => (await writeDurably('bundles', backup, { bytes, size: bytes.byteLength }), bytes.byteLength));
  await writeDurably('kv', `reset:${id}`, { type: 'reset', id, backup });
  try {
    await discard(id);
    await client.create(dirOf(id), provider, settings);
    // The thread as it was stays, as an earlier version (Settings > Storage), named on
    // disk before the reset is done: a crash in between brings the old thread back.
    await keepEarlier(id, backup, Date.now(), size);
    // Gone from disk before the thread takes any work: found again after a crash, it
    // would bring the old thread back over what the new one did.
    await removeDurably('kv', `reset:${id}`);
  } catch (error) {
    await restoreFirst({ id, backup });
    throw error;
  }
}

/** An earlier version's record: kv `earlier:<id>:<backup>` naming its bundle. */
const EARLIER = 'earlier:';
/** How long an earlier version is kept whatever happens, and how many turns the thread
 * must finish after it, before it may be freed (deleting the thread deletes it too). */
const EARLIER_DAYS = 7;
const EARLIER_TURNS = 3;

/** Keeps the bundle `backup` (a whole copy of thread `id` as it was) as an earlier
 * version of it. */
async function keepEarlier(id, backup, made = Date.now(), known = null) {
  // Its size as the export gave it (an earlier build's backup is read once for it).
  const size = known ?? (await read('bundles', backup))?.bytes?.byteLength ?? 0;
  await writeDurably('kv', `${EARLIER}${id}:${backup}`, { type: 'earlier', id, backup, made, size });
}

/** Every earlier version's record. */
async function earlierVersions() {
  const keys = await keysWith('kv', EARLIER);
  return (await Promise.all(keys.map((key) => read('kv', key)))).filter((record) => record?.type === 'earlier');
}

/** Whether an earlier version may be freed: a week after it was kept and three turns
 * the thread finished since (`{free, until}`, `until` saying what is still to come). */
async function freeable(record) {
  const due = record.made + EARLIER_DAYS * 24 * 3600 * 1000;
  let since = 0;
  // A thread whose history is not here (on the user's host, or moved into a file) has no
  // turns to count: its earlier versions wait out the week alone.
  const thread = await read('threads', record.id);
  if (thread?.home || thread?.state === 'moved') since = EARLIER_TURNS;
  else try {
    // Turns since: each request it holds but the one sent again (compaction can only
    // lower this, which keeps the version longer).
    // (Through `within`: the session closes again once it has rested.)
    const status = await within(record.id, () => client.status(dirOf(record.id)));
    since = Math.max(0, (status.prompts ?? 0) - 1 - (status.complete ? 0 : 1));
  } catch {}
  // What is still to come: the day it may go, and the turns the thread must finish first
  // (`until` says both in words, for a refusal).
  const date = Date.now() < due ? due : null;
  const turns = Math.max(0, EARLIER_TURNS - since);
  const waits = [date && new Date(date).toLocaleDateString([], { month: 'short', day: 'numeric' }), turns && `the thread finishes ${turns} more ${turns === 1 ? 'turn' : 'turns'}`].filter(Boolean);
  return { free: waits.length === 0, until: waits.join(' and ') || null, due: date, turns };
}

/** A thread's earlier versions and their bundles go (it is being deleted). */
async function dropPriors(id) {
  for (const key of await keysWith('kv', `${EARLIER}${id}:`)) await remove('kv', key);
  for (const key of await keysOf('bundles')) if (key.startsWith(`prior:${id}:`)) await remove('bundles', key);
}

async function restoreFirst({ id, backup }) {
  const kept = await read('bundles', backup);
  if (!kept) throw failure('broken', `the saved record for thread ${id} is missing`);
  await discard(id);
  await client.importSession(dirOf(id), kept.bytes);
  await client.closeSession(dirOf(id));
  // The thread is that copy again: not an earlier version of itself.
  await remove('kv', `${EARLIER}${id}:${backup}`).catch(() => {});
  // As a finished reset's: gone from disk before the thread takes any work.
  await removeDurably('kv', `reset:${id}`);
  await remove('bundles', backup).catch(() => {});
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

/** An imported session (`status`, as the import answered) made a thread of this browser:
 * it gets Chrome's page tools, and whatever a native host's settings need to run in
 * Chrome. The thread is in either way; what could not change, it says. Resolves with
 * `{note, said, runner, network, tasks}`. */
async function fitHere(dir, status) {
  try {
    const settings = await client.settings(dir);
    const told = status.fit ?? await client.fit(dir);
    const after = told.changes?.runner ?? settings.runner;
    const fit = fitForChrome(told, after);
    const tools = [...(settings.tools ?? []).filter((tool) => !PAGE_NAMES.has(tool.name)), ...client.tools.definitions()];
    const files = (fit.changes.builtin_tools ?? settings.builtin_tools) === 'files';
    const instructions = guided(settings.instructions, after === 'app' && files, (await read('kv', 'standing')) ?? '');
    const fitted = await client.settings(dir, { ...fit.changes, tools, instructions });
    return {
      note: fit.said.length ? `Changed to run in ${BROWSER}: ${fit.said.join('; ')}.` : null,
      said: fit.said, runner: fitted.runner ?? 'none', network: Boolean(fitted.network), tasks: Boolean(fitted.tasks),
    };
  } catch (error) {
    return {
      note: `Its settings could not be changed for ${BROWSER}: ${error.message}`,
      said: [`none could be made (${error.message}), so it may not run as it should`], runner: 'none', network: false, tasks: false,
    };
  }
}

const ops = {
  hello: () => snapshot(),

  /** That the engine answers (a file's tab times its start apart from what it reads). */
  ping: () => true,

  async create({ provider, model, effort, text, runner = 'app', network = false, tasks = false, max_usd: limit = null, compact_tokens: compactAt, files: given, from }) {
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
        // Where history compacts, when the user set one for new threads (else the
        // engine's own default).
        ...(compactAt !== undefined && { compact_tokens: compactAt }),
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
    // On disk before anything else is told of it: an index entry lost to a power cut
    // would leave the thread's files unlisted (found again at the next start).
    await writeDurably('threads', id, thread);
    send({ type: 'thread', thread });
    const { files, note } = attachments(given);
    const sent = [...(carried?.files ?? []), ...(files ?? [])];
    // Before the attachments' line, which ends a message.
    const started = carried ? `\n\n[Started with the files of “${carried.title}”: ${carried.files.length} in the workspace]` : '';
    // Its setup in its instructions from its first turn on (`setupOf`), inside the turn
    // as `send` does, so nothing else starts one meanwhile. Unlike a later message, the
    // first has no box to go back to, so a Stop does not hold it back here (as before).
    turn(id, async () => {
      await freshen(dir);
      return client.prompt(dir, text + started + note, sent.length ? { files: sent } : {});
    });
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
      await enqueue(waitingMessage(id, text, given));
      return { queued: true };
    }
    // A turn stopped part way goes on with the message (the model reads the stopped step,
    // then it), which takes text only; a turn that takes no message now (it did not
    // finish, or needs the user first) refuses it. Refused here, the draft stays.
    const status = await within(id, () => client.status(dirOf(id)));
    if (!status.accepts_prompt) {
      if (!status.accepts_message) {
        throw failure('busy', `The last turn did not finish${status.stop_text ? ` (${status.stop_text})` : ''}: retry, continue or take it back first.`);
      }
      if (given?.length) {
        throw failure('busy', 'This turn was stopped part way and takes a message without files: send the text alone (or Continue it), then the files with your next message.');
      }
    }
    const { files, note } = attachments(given);
    turn(id, async (check) => {
      await freshen(dirOf(id));
      check();
      return client.prompt(dirOf(id), text + note, files ? { files } : {});
    });
    // Should the turn end without it (stopped before it began), the message comes back.
    carrying.set(id, waitingMessage(id, text, given));
    return { started: true };
  },

  /** Takes a thread's waiting message back (`{text, files}`; null when none waits). */
  async unqueue({ id }) {
    const waiting = queued.get(id);
    if (!waiting) return null;
    queued.delete(id);
    // Back in the box: a record left by a failed removal comes back held after a restart
    // (the user sees it twice rather than lose it).
    await dropQueued(waiting).catch(() => {});
    announceQueued(id);
    return { text: waiting.text, files: waiting.files };
  },

  stop: ({ id }) => within(id, () => {
    const entry = running.get(id);
    if (entry) entry.stopped = true;
    // The turn's own page tools end now; the package ends those of the tasks it stops with
    // the turn (their calls' signals), and its background tasks go on, page tools too.
    cancelPageTools(id, { tasks: false });
    return client.stop(dirOf(id));
  }),

  async retry({ id }) {
    await workable(id);
    // A step stopped in the thread's first request has nothing before it to ask again
    // from: the turn starts over instead (taken back, its message sent again).
    if (await firstStepStopped(id)) return ops.restart({ id });
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
      return { thread, status: null, history: null, running: false, bundle: await has('bundles', id) };
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
      const target = await pointBefore(dir, shown, at, { thorough: true });
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
      const targets = await client.targets(dir, { limit: 8192 }).catch((error) => {
        throw error?.kind === 'busy' ? failure('busy', NOT_WHILE_WORKING) : error;
      });
      for (const target of targets) {
        if (revision != null && target.revision >= revision) continue;
        await letOthersGo();
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
      max_usd: settings.max_usd ?? null, compact_tokens: settings.compact_tokens ?? null, usd: usage.usd, calls: usage.calls,
      subscription: usage.subscription, unpriced: usage.unpriced, unknown: usage.unknown,
    };
  }),

  /** The efforts a provider's model takes, each with what it then asks for. */
  efforts: ({ provider, model }) => client.efforts(provider, model),

  /** Goes on with a turn that stopped before it finished, `changes` (provider, model,
   * effort, where commands run, network, tasks, budget) applied: from just before its
   * newest model request when it has one (what the turn did stays; only the request that
   * failed goes again, with the new model); else from the end of the turn before (the
   * turn starts over, its message and the files attached to it sent again; what it did
   * stays in the history behind); a thread's first turn starts afresh, its earlier version
   * kept (Settings > Storage). Never anything else: a thread whose earlier turns cannot be
   * gone back to now says so and is left as it is. `resend` false takes the turn back to
   * the end of the turn before and gives its message back ({text, files}). */
  async restart({ id, changes = {}, resend = true }) {
    await workable(id);
    notRunning(id, 'starting it over');
    const dir = dirOf(id);
    const changing = Object.keys(changes).length > 0;
    const { text, files, fresh, carried = [], goOn = false } = await within(id, async () => {
      const status = await client.status(dir);
      if (status.accepts_prompt) throw failure('invalid', 'this thread has no unfinished turn to start over');
      // Back to the newest completed turn, however far (a long turn has many steps).
      const point = await client.turn(dir);
      if (changing) {
        const settings = await client.settings(dir);
        await checkCreation(changes.provider ?? settings.provider, { ...creationSettings(settings), ...changes });
      }
      const before = resend && point.targets[0]?.kind === 'before_model' ? point.targets[0] : null;
      if (before) {
        if (before.revision !== status.head) {
          await client.rewind(dir, before.revision);
          await forgetUnlisted(id);
        }
        if (changing) await changeSettings(id, changes);
        return { goOn: true };
      }
      const settled = point.targets.find((target) => target.kind === 'settled');
      // Earlier turns that cannot be gone back to now are never wiped for a fresh start.
      if (!settled && point.completed !== false) throw await heldBack(dir);
      if (!settled && status.unresolved) {
        throw failure('conflict', 'its first request may have reached the provider with no reply saved: Retry it, or delete the thread');
      }
      // Its own message (never a summary of earlier ones, a message sent while it ran or
      // a task's report); the files attached to it are read now: taking the turn back
      // takes them out of the workspace.
      if (!point.prompt) throw failure('invalid', "this turn's message was not found");
      const { text, paths } = unattached(point.prompt);
      const files = [];
      for (const path of paths) {
        try {
          files.push({ path, bytes: await client.read(dir, path) });
        } catch {
          // Gone already (a command of the turn removed it): it is not sent again.
        }
      }
      if (settled) {
        await client.rewind(dir, settled.revision);
        await forgetUnlisted(id);
        if (changing) await changeSettings(id, changes);
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
    if (goOn) {
      await touch(id, { state: 'idle', note: null });
      turn(id, async (check) => {
        await freshen(dir);
        check();
        return client.resume(dir);
      });
      return { started: true, from: 'request' };
    }
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
    // Taken out of the history already: should the turn end without it (stopped before it
    // began, or its start failing), the message comes back, with its files.
    carrying.set(id, waitingMessage(id, text, files));
    return { started: true, from: fresh ? 'start' : 'turn' };
  },

  rename: ({ id, title }) => touch(id, { title: String(title).trim().slice(0, 80) || 'Untitled' }),

  async remove({ id }) {
    notRunning(id, 'deleting it');
    // Its background tasks' page actions (and their questions) end with it.
    cancelPageTools(id);
    if (using.has(id)) throw failure('busy', 'this thread is busy; try again in a moment');
    // Closing it here (if it is open) gives its store and Web Lock back first.
    await discard(id);
    await remove('bundles', id);
    bundleSizes.delete(id);
    await dropPriors(id);
    for (const key of await keysOf('kv')) {
      // Its preview choices, waiting messages, and a message that came back for it.
      if (key.startsWith(`preview:${id}:`) || key.startsWith(`${QUEUED}${id}:`) || key === `came-back:${id}`) await remove('kv', key);
    }
    await remove('threads', id);
    queued.delete(id);
    carrying.delete(id);
    linux.forget(dirOf(id));
    // The tabs Spindle opened for it close too.
    tasksOpening.delete(id);
    tabsDone(id, { tasks: true });
    send({ type: 'removed', id });
    return { removed: true };
  },

  async export({ id, keep }) {
    await workable(id);
    notRunning(id, 'exporting it');
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
        // The file is kept (on disk: a strict commit) before this copy is sealed at the
        // file's own head (refused if the thread went on since), so whatever stops the
        // engine, the thread is in one or the other. Kept, it can be saved again (a closed
        // panel or a cancelled save loses nothing). One export: the bundle is built once.
        // A background task at work would refuse the seal: said before a copy is made.
        if ((await client.status(dir)).background) throw failure('busy', 'a task of this thread works in the background: wait for it (or stop it), then move the thread');
        const bytes = await client.exportSession(dir, { keep: true, settings: native });
        await writeDurably('bundles', id, { bytes, name, size: bytes.byteLength });
        const facts = bundleFacts(bytes);
        try {
          await client.sealSession(dir, { expectedHead: facts.head, expectedState: facts.sourceState });
        } catch (error) {
          // Not moved: the thread stays live here, and the copy just kept is not its file.
          await remove('bundles', id).catch(() => {});
          throw error;
        }
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
      if (!usage.has(id)) usage.set(id, { id, bytes: 0, database: 0, log: 0, tasks: 0, aside: 0, other: 0, free: 0, sealed: 0, bundle: 0, held: 0, move: null, hosted: false });
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
      if (thread.home) entry(thread.id).hosted = true;
    }
    // A move to or from the user's host holds the thread's bytes here until it is done.
    for (const move of await moves.list()) {
      if (!known.has(move.id)) continue;
      Object.assign(entry(move.id), { held: move.size, move: { way: move.way, step: move.step, error: move.error } });
    }
    // Earlier versions (a thread as it was before a fresh start), kept in IndexedDB.
    for (const record of await earlierVersions()) {
      if (!known.has(record.id)) continue;
      const kept = entry(record.id);
      kept.earlier = (kept.earlier ?? 0) + record.size;
      kept.bytes += record.size;
      (kept.versions ??= []).push({ backup: record.backup, made: record.made, size: record.size, ...(await freeable(record)) });
    }
    // Bytes no file name leads to: slots whose header was found damaged (opfs.mjs `lost`).
    return { threads: [...usage.values()], lost: { files: fs.lost, bytes: fs.lostBytes } };
  },

  /** An earlier version of a thread, brought back as a thread of its own (the version
   * stays, and so does the thread as it is now). */
  async earlierOpen({ id, backup }) {
    const record = await read('kv', `${EARLIER}${id}:${backup}`);
    const kept = record && await read('bundles', backup);
    if (!kept?.bytes) throw failure('not_found', 'that earlier version is gone');
    const title = (await read('threads', id))?.title ?? 'Thread';
    const opened = await ops.import({ bytes: kept.bytes, name: `${title}.spindle` });
    // Named apart from the thread it was (an import names a thread by its first message).
    await touch(opened.id, { title: `${title} (as it was on ${new Date(record.made).toLocaleString([], { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' })})`.slice(0, 80) });
    return opened;
  },

  /** An earlier version goes, once the thread has gone on past it (`freeable`). */
  async earlierFree({ id, backup }) {
    const record = await read('kv', `${EARLIER}${id}:${backup}`);
    if (!record) return { freed: 0 };
    const why = await freeable(record);
    if (!why.free) throw failure('invalid', `this earlier version is kept until ${why.until}`);
    // The copy first, then its record: a crash between leaves a record whose copy is
    // gone (cleared at start), never a copy no record names (kept as a version again).
    await removeDurably('bundles', backup);
    await removeDurably('kv', `${EARLIER}${id}:${backup}`);
    return { freed: record.size };
  },

  /** Lets go of the bytes `storage` says no file name leads to (`lost`): `{freed}`. */
  async reclaimLost() {
    return { freed: (await openPool()).reclaim() };
  },

  /** Gives a thread's free pages back (its store and its tasks'), checked whole before
   * the copy replaces anything. The engine does nothing else meanwhile, so nothing may
   * be running. */
  compact: ({ id }) => within(id, async () => {
    await workable(id);
    notRunning(id, 'freeing space in it');
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

  /** A moved thread's file goes from this browser once it is saved whole on the user's
   * disk (`filename`, as Chrome's downloads confirmed it): the thread is then in that
   * file alone, and importing it brings it back. Its sealed copy goes too. */
  dropBundle: ({ id, filename }) => within(id, async () => {
    const thread = await read('threads', id);
    if (thread?.state !== 'moved') throw failure('invalid', 'only a thread moved into a file keeps one here');
    if (!filename) throw failure('invalid', 'the saved file is not named');
    const size = await bundleSize(id);
    let freed = 0;
    if (await has('bundles', id)) {
      await removeDurably('bundles', id);
      freed += size;
    }
    bundleSizes.delete(id);
    freed += dropSealed(await openPool(), id);
    await touch(id, { note: `In your file ${filename} alone (saved ${new Date().toLocaleString()}): import it to bring the thread back.`, file: { name: filename, size, at: Date.now() } });
    return { freed };
  }),

  /** A thread's finished tasks whose working copies are still here (`{task, name,
   * bytes}`): what Storage offers to save to a file and free. */
  finishedTasks: ({ id }) => within(id, async () => {
    const { tasks = [] } = await client.tasks(dirOf(id));
    return tasks.filter((task) => task.store && !task.dropped && !task.pending && task.state !== 'running')
      .map((task) => ({ task: task.task, name: task.name ?? String(task.task).split('#').pop(), prompt: task.prompt ?? null, bytes: task.bytes ?? 0, remains: Boolean(task.remains) }));
  }),

  /** A finished task's working copy as a session bundle (it imports as a thread). */
  taskExport: ({ id, task }) => within(id, async () => {
    notRunning(id, 'saving its tasks');
    return { bytes: await client.exportTask(dirOf(id), task) };
  }),

  /** A finished task's working copy goes, once saved (its spend stays counted in the
   * thread, its answer and the files it gave stay in it). */
  taskDrop: ({ id, task, expectedState }) => within(id, async () => {
    notRunning(id, 'freeing its tasks');
    if (typeof expectedState !== 'string') throw failure('invalid', 'the saved task version is missing: save it again before freeing it');
    return client.dropTask(dirOf(id), task, { expectedState });
  }),

  /** The file a moved thread went into, to save again. */
  async bundle({ id }) {
    const kept = await read('bundles', id);
    if (!kept) throw failure('not_found', 'this thread keeps no file');
    return kept;
  },

  async import({ bytes, name }) {
    // As the panel checks before reading the file: a larger one would end the engine (it
    // is held about three times over) rather than fail.
    if (bytes?.byteLength > BUNDLE_MOST) throw failure('too_large', `${name ?? 'this file'} is too large to bring into this browser (over ${BUNDLE_MOST / 2 ** 30} GB)`);
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
    const { note, said, runner, network, tasks } = await fitHere(dir, status);
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
    // On disk before anything else is told of it: an index entry lost to a power cut
    // would leave the thread's files unlisted (found again at the next start).
    await writeDurably('threads', id, thread);
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
    // Kept for this revision unless the store could not look back yet: it lists no
    // earlier points while the thread works (a turn, a task), so it is asked again then.
    let lasting = true;
    if (at === 0) {
      before = [];
    } else if (at > 0) {
      try {
        const revision = await pointBefore(dir, shown, at);
        if (revision != null) before = (await client.files(dir, revision)).files.map((file) => [file.path, file.digest]);
      } catch (error) {
        if (error?.kind === 'busy') lasting = false;
        else console.warn('Spindle: the previous workspace is unavailable:', error);
      }
    }
    const value = { ...now, before };
    if (lasting) turnFilesKept = { key, value };
    return value;
  }),

  read: ({ id, path }) => within(id, async () => ({ bytes: await client.read(dirOf(id), path) })),

  /** Connects the user's Spindle host (`{url, token}`; null disconnects) for threads whose
   * commands run there. */
  async host({ host }) {
    hostGeneration += 1;
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

  /** The user's answer to a page action waiting for it (`approval`: its id; `allow`: go
   * ahead). Not `id`, which names a thread to every operation. */
  approve({ approval: id, allow }) {
    const approval = approvals.get(String(id));
    if (!approval) throw failure('not_found', 'that action is no longer waiting (it was answered, or its turn ended)');
    approval.answer(allow ? null : 'the user said no (Settings, Pages, Before acting asks first), so it was not done');
    return { approvals: approvalsShown() };
  },

  /** Adds (`{url, write, header?: {name, value}}`: a listed address is updated) or
   * removes (`{remove: name}`) a site scripts' `http()` may reach, against the list as
   * kept (other panels may have changed it); see changeSites. */
  async sites({ add, remove: name }) {
    const change = siteChanges.then(() => changeSites(add, name));
    siteChanges = change.catch(() => {});
    await change;
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

  /** Forgets a provider's key or sign-in. A Sign in with ChatGPT session is ended at
   * OpenAI first (its refresh token revoked); its registration stays, so signing in again
   * is the same client (`unconfirmed`: OpenAI did not confirm the end). */
  async forget({ provider }) {
    const profile = catalog.providers.find((p) => p.id === provider);
    let unconfirmed = false;
    if (planSignIn(profile)) {
      // One that cannot be read (the keys locked) cannot be ended there either: said.
      const held = await vault.secret(String(provider)).then((secret) => ({ secret }), () => null);
      unconfirmed = !held || !(await revoke(profile, held.secret, reach).catch(() => false));
    }
    await vault.forget(String(provider));
    announceVault();
    return { ...(await snapshot()), ...(unconfirmed && { unconfirmed }) };
  },

  /** The user declined Sign in with ChatGPT on OpenAI's page: the registration kept
   * asks for the plan permission again on the next sign-in (`prompt=consent`). */
  async signInDeclined({ provider }) {
    const kept = await registrationOf(String(provider));
    if (kept) await keepRegistration(String(provider), { ...kept, declined: true });
    return {};
  },

  /** What a sign-in starts with: for Sign in with ChatGPT this browser's installation ID
   * and the registration it signs in again with (none when `fresh`: another account
   * registers anew). No secret: a client ID and the account's email. */
  async signInStart({ provider, fresh = false }) {
    const profile = catalog.providers.find((p) => p.id === provider);
    if (!flowOf(profile)) throw failure('invalid', `${profile?.display_name ?? provider} does not sign in through a browser here`);
    if (!planSignIn(profile)) return {};
    const registration = fresh ? null : await registrationOf(String(provider));
    return {
      hostId: await agentHost(),
      registration: registration && { clientId: registration.clientId, account: registration.account ?? null, declined: Boolean(registration.declined) },
    };
  },

  /** A forgotten passphrase: every saved key and sign-in goes (the host's token and
   * sites' secret headers too; such a site's requests are refused until it is added
   * again with one); threads stay. */
  async resetVault() {
    await vault.reset();
    await connectKeptHost().catch(() => {});
    announceVault();
    return snapshot();
  },

  /** Exchanges a sign-in the panel caught (`code`, `verifier`, `redirect`; for Sign in
   * with ChatGPT also `nonce` and `clientId`) for tokens and keeps them sealed. A Sign in
   * with ChatGPT one keeps its registration (client, account) for the next, checks a
   * sign-in again with it is that account, and says when it is the first here
   * (`planWelcome`: the panel says what using the plan means, once). */
  async signIn({ provider, code, verifier, redirect, nonce = null, clientId = null }) {
    const profile = catalog.providers.find((p) => p.id === provider);
    if (!flowOf(profile)) throw failure('invalid', `${profile?.display_name ?? provider} does not sign in through a browser here`);
    // A code is good for one exchange: none while its tokens could not be kept.
    if (!(await vault.usableNow())) throw failure('locked', "Spindle's keys are locked: unlock them, then sign in");
    const plan = planSignIn(profile);
    // Signed in again as the client kept: the token must be that account's.
    const kept = plan ? await registrationOf(String(provider)) : null;
    const against = kept && kept.clientId === clientId ? kept : null;
    let made;
    try {
      made = await exchange(profile, { code, verifier, redirect, nonce, clientId }, reach, against);
    } catch (error) {
      // Registered, but the plan permission was declined: the next sign-in asks again as
      // that client.
      if (plan && error?.kind === 'declined' && error.registration) await keepRegistration(String(provider), error.registration);
      // A code refused (`invalid_grant`) as a client just issued (a first sign-in, or
      // another account's): that client is kept, so the next sign-in is it again, not
      // another registration.
      if (plan && error?.code === 'invalid_grant' && kept?.clientId !== clientId && typeof clientId === 'string' && clientId !== flowOf(profile).client_id) {
        await keepRegistration(String(provider), { clientId, subject: null, account: null });
      }
      throw error?.kind === 'declined' ? failure('declined', error.message) : error;
    }
    await vault.keep(provider, { ...made.secret, origin: originOf(profile.base_url), tokenOrigin: originOf(flowOf(profile).token_url) });
    let planWelcome = false;
    if (plan) {
      await keepRegistration(String(provider), made.registration);
      planWelcome = !(await read('kv', 'plan-welcomed'));
      if (planWelcome) await write('kv', 'plan-welcomed', true);
    }
    announceVault();
    return { ...(await snapshot()), ...(planWelcome && { planWelcome }) };
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

// ---- Threads on the user's Spindle host -------------------------------------------
// A thread moved to the host (its record's `home`: {url, session}) runs there: its model
// on the host's credentials, its commands in the host's sandbox. The panel drives it with
// the operations it drives a thread here with; they go to the host's /v1 (RemoteClient).
// Its events come over the session's event stream (the same JSON forms the package's
// are) while it runs or a panel has it open, and the page tools it calls are answered
// here, by the code that answers them for a thread here, while the extension runs.
// Moving it there and back is host-moves.mjs's (durable, resumed at every start).

/** The connected host's client for a thread on the host at `url` (null: whichever is
 * connected), when it is connected with a browser token; otherwise it says what to do. */
function hostedClient(url = null) {
  const away = (why) => Object.assign(new Error(why), { status: 0, kind: 'host' });
  if (!hostState.connected || !hostRemote) {
    throw away(url ? `your host at ${url} is not connected (Settings, Your Spindle host)` : 'no Spindle host is connected: connect one in Settings, Your Spindle host');
  }
  if (url && hostState.url !== url) throw away(`this thread is on the host at ${url}, but ${hostState.url} is connected: connect that host again in Settings`);
  if (hostState.kind !== 'browser') {
    throw away(hostState.kind === 'runner'
      ? 'your host is connected with a runner token, which only runs commands: press Connect in Settings, Your Spindle host'
      : 'your host is connected with a full API token, not this browser\'s own: press Connect in Settings, Your Spindle host');
  }
  return hostRemote;
}

/** A host's refusal as the panel says it (`kind` its code). */
function hostFailure(error) {
  return error?.kind && typeof error.status === 'number' ? failure(error.kind, sentence(error)) : error;
}

/** Tests stop a move at one of its steps (`kv/cut-move` names it) as if the engine died
 * there: nothing after it runs until the engine starts again. */
async function cutHere(step) {
  if ((await read('kv', 'cut-move').catch(() => null)) !== step) return;
  await removeDurably('kv', 'cut-move');
  send({ type: 'cut', at: step });
  await new Promise(() => {});
}

const moves = hostMoves({
  read,
  keysOf,
  update,
  remote: (url) => hostedClient(url),
  local: {
    /** The thread's copy for the host: its page tools kept (the host asks this browser
     * for them while it is open) and the browser Linux's guide left out; the host's own
     * fit does the rest (its sandbox, every tool). */
    copy: (id) => within(id, async () => {
      const dir = dirOf(id);
      const status = await client.status(dir);
      if (status.background) throw failure('busy', 'a task of this thread works in the background: wait for it (or stop it), then move the thread');
      const settings = await client.settings(dir);
      return client.exportSession(dir, { keep: true, settings: { tools: settings.tools ?? [], instructions: guided(settings.instructions, false) } });
    }),
    facts: (bytes) => client.checkBundle(bytes),
    sealed: async (id) => {
      const fs = await openPool();
      const fence = `${dirOf(id)}/store/authority.sqlite.lock`;
      return fs.existsSync(fence) && String(fs.readFileSync(fence)).startsWith('spindle-moved: ');
    },
    // Sealed at the head of the copy the host took (refused if the thread went on since):
    // no second bundle is built.
    seal: (id, head, sourceState) => within(id, async () => {
      let state = sourceState;
      // A move an earlier version recorded has no source state: it is read from a copy made
      // now, at the same head (the thread takes no work while it moves).
      if (!state) {
        const facts = bundleFacts(await client.exportSession(dirOf(id), { keep: true }));
        if (facts.head !== head) throw failure('head_moved', 'this thread went on after its copy went to your host');
        state = facts.sourceState;
      }
      return client.sealSession(dirOf(id), { expectedHead: head, expectedState: state });
    }),
    dropSealed: async (id) => {
      await client.closeSession(dirOf(id)).catch(() => {});
      dropSealed(await openPool(), id);
    },
    clear: (id) => discard(id),
    restore: async (id, bytes) => {
      await headroom();
      const dir = dirOf(id);
      let status;
      try {
        status = await client.importSession(dir, bytes);
      } catch (error) {
        await discard(id).catch(() => {});
        throw error;
      }
      const fitted = await fitHere(dir, status);
      rest(id);
      return {
        fields: { provider: status.provider, model: status.model, effort: effortOf(status), runner: fitted.runner, network: fitted.network, tasks: fitted.tasks, summaries: true },
        said: fitted.said,
      };
    },
  },
  changed: (thread) => send({ type: 'thread', thread }),
  cut: cutHere,
});

/** A move started here goes on by itself; one that waits for the host is taken up again
 * soon (and whenever the host connects). */
function watchMove(done) {
  done.then(() => hostedTick(), (error) => {
    if (!(error instanceof Waiting)) console.warn('Spindle: a move to or from your host stopped:', error);
    hostedTick();
  });
}

/** Hosted threads' turns this engine started: id -> {request, kind}. */
const hostedTurns = new Map();
const admittingHosted = new Map();
const hostedCommandKey = (id) => `host-command:${id}`;
const hostedStopKey = (id) => `host-stop:${id}`;
const stoppingHosted = new Map();

function sendHostedStop(id) {
  if (stoppingHosted.has(id)) return stoppingHosted.get(id);
  const next = (async () => {
    const stop = await read('kv', hostedStopKey(id));
    if (!stop) return;
    const remote = hostedClient(stop.url);
    const info = await remote.info();
    if (!(info.features ?? []).includes('targeted_stop')) throw failure('host', 'update your Spindle host to deliver this saved Stop safely');
    const mine = hostedTurns.get(id);
    if (mine?.request === stop.target) mine.stopped = true;
    try {
      await settledCommand(remote, 'stop', { session: stop.session, target_request: stop.target }, { requestId: stop.request, attempts: 1 });
    } catch (error) {
      if (error?.kind !== 'not_running' && error?.status !== 404) throw error;
    }
    if ((await read('kv', hostedStopKey(id)))?.request === stop.request) await removeDurably('kv', hostedStopKey(id));
  })();
  stoppingHosted.set(id, next);
  next.finally(() => { if (stoppingHosted.get(id) === next) stoppingHosted.delete(id); }).catch(() => {});
  return next;
}

async function forgetHostedCommand(id, request) {
  if ((await read('kv', hostedCommandKey(id)))?.request === request) {
    await removeDurably('kv', hostedCommandKey(id));
    await remove('bundles', hostedFilesKey(id)).catch(() => {});
  }
}

/** A message's files kept here until the host has taken it (`onHost.send`). */
const hostedFilesKey = (id) => `host-files:${id}`;

function admitHosted(id, command) {
  if (admittingHosted.has(id)) return admittingHosted.get(id);
  const next = (async () => {
    const thread = await read('threads', id);
    if (!thread?.home) return;
    const before = hostedTurns.get(id);
    const stop = await read('kv', hostedStopKey(id));
    hostedTurns.set(id, Object.assign(before ?? {}, { request: command.request, kind: command.kind, ...(stop?.target === command.request && { stopped: true }) }));
    const receipt = await hostedClient(thread.home.url).command(command.kind, command.fields, { requestId: command.request, attempts: 1 });
    await forgetHostedCommand(id, command.request);
    if (stop) await sendHostedStop(id).catch((error) => { away(id, `a saved Stop waits for your host (${sentence(error)})`).catch(() => {}); hostedTick(1000); });
    if (receipt.state !== 'running') await endHosted(id, receipt);
  })();
  admittingHosted.set(id, next);
  next.finally(() => { if (admittingHosted.get(id) === next) admittingHosted.delete(id); }).catch(() => {});
  return next;
}

/** Hosted threads' event streams followed now: id -> {controller, answered, opened}. */
const followed = new Map();
/** When a panel last opened each hosted thread: it is followed for a while after. */
const lookedAt = new Map();
const LOOK_MS = 5 * 60_000;
/** Why each hosted thread cannot be reached now (said on it), as last written. */
const awayNow = new Map();

/** Says (or, `why` null, stops saying) that a hosted thread's host cannot be reached;
 * nothing of the thread is lost meanwhile. Its place in the list stays. */
async function away(id, why) {
  if (awayNow.has(id) && awayNow.get(id) === why) return;
  awayNow.set(id, why);
  // Compared with the record too: one kept from before the engine started (Chrome closed
  // while the host was out of reach) is cleared once the host answers.
  let changed = false;
  const next = await update('threads', id, (thread) => {
    changed = (thread.away ?? null) !== why;
    return changed ? { ...thread, away: why } : thread;
  });
  if (next && changed) send({ type: 'thread', thread: next });
}

/** A host list state as this browser's thread states say it: [state, note]; null for a
 * thread moving back here (its move says how it stands). */
function listed(state) {
  if (state === 'running' || state === 'stopping') return ['running', null];
  if (state === 'empty' || state === 'done') return ['idle', null];
  if (state === 'stopped') return ['stopped', 'It stopped part way on your host'];
  if (state === 'reply_saved') return ['interrupted', 'Its reply was saved on your host: Continue goes on from it'];
  if (state === 'needs_attention') return ['interrupted', 'It stopped part way on your host'];
  if (state === 'moved') return null;
  return ['failed', `your host cannot read this thread (${state})`];
}

/** The session object as the panel reads a thread's status. */
function statusOf(object) {
  const settings = object.settings ?? {};
  return { ...object, provider: settings.provider, model: settings.model, effort: settings.effort ?? 'default', unresolved: 0, background: null };
}

/**
 * Follows hosted thread `id`'s events (the poll lets go of one that no longer runs and
 * that no panel has open): each goes to the panels as a thread's here does, a turn's
 * `command_done` ends it, and the page tools it asks for are answered. `quiet`: the
 * panel already holds what the stream's first snapshot says (a turn started here), so it
 * is not told to draw the thread again; after a gap it is.
 */
function follow(id, quiet = false) {
  const known = followed.get(id);
  if (known) return known;
  const entry = { controller: new AbortController(), answered: new Set(), quiet, seen: false };
  entry.opened = new Promise((resolve) => {
    entry.open = resolve;
    setTimeout(resolve, 5000);
  });
  followed.set(id, entry);
  (async () => {
    try {
      const thread = await read('threads', id);
      if (!thread?.home) return;
      await hostedClient(thread.home.url).follow(thread.home.session, {
        signal: entry.controller.signal,
        onFrame: (frame) => {
          entry.open();
          return hostedFrame(id, thread.home, entry, frame);
        },
        onState: (state, error) => {
          if (state === 'waiting') entry.open();
          away(id, state === 'open' ? null : `your host does not answer (${sentence(error)}); Spindle keeps trying, and nothing is lost`).catch(() => {});
        },
      });
    } catch (error) {
      if (error?.status === 404) await away(id, 'your host no longer has this thread').catch(() => {});
      else if (error?.status === 401) await away(id, "your host no longer takes this browser's token: connect it again in Settings").catch(() => {});
      else if (error?.status === 0) await away(id, sentence(error)).catch(() => {});
      else console.warn('Spindle: a hosted thread\'s events stopped:', error);
    } finally {
      entry.open();
      if (followed.get(id) === entry) followed.delete(id);
    }
  })();
  return entry;
}

async function hostedFrame(id, home, entry, { kind, data }) {
  if (entry.controller.signal.aborted || followed.get(id) !== entry) return false;
  if (kind === 'deleted') {
    await away(id, 'your host no longer has this thread');
    return false;
  }
  if (kind === 'snapshot') {
    // Opened on a thread at work, or again after a gap: the panel draws it again.
    if (entry.seen || !entry.quiet) send({ type: 'resync', thread: id });
    entry.seen = true;
    for (const call of data?.app_tools ?? (data?.app_tool ? [data.app_tool] : [])) answerHosted(id, home, entry, call);
    if (data?.busy && (await read('threads', id))?.state !== 'running') await touch(id, { state: 'running', note: null });
    return true;
  }
  entry.seen = true;
  const event = data;
  if (event?.type === 'app_tool') {
    answerHosted(id, home, entry, event);
  } else if (event?.type === 'app_tool_done') {
    entry.answered.delete(event.call);
  } else if (event?.type === 'command_done') {
    // A turn's end (another command's, a rewind's say, is not one).
    const mine = hostedTurns.get(id);
    if (mine ? mine.request === event.request_id : (await read('threads', id))?.state === 'running') await endHosted(id, event);
    return true;
  } else if (event?.type === 'prompt' && !hostedTurns.has(id) && (await read('threads', id))?.state !== 'running') {
    // A turn started elsewhere (the host's own screen) runs here too.
    await touch(id, { state: 'running', note: null });
  }
  if (event?.type === 'task') noteTask(id, event);
  send({ type: 'event', thread: id, event });
  return true;
}

/** Answers a hosted thread's call of one of this browser's tools (the page tools) with
 * the code that answers them for a thread here; another app's tools are its to answer.
 * Each call is answered once, however often it is announced (a snapshot and an event
 * can both name it); one the host no longer waits for is let go. */
const answeringHosted = new Map();
function answerHosted(id, home, entry, event) {
  const key = `host-tool:${id}:${home.session}:${event?.call}`;
  const known = answeringHosted.get(key);
  if (known) return known.entry === entry ? known.done : known.done.catch(() => {}).then(() => answerHosted(id, home, entry, event));
  const next = answerHostedCall(id, home, entry, event, key);
  const active = { entry, done: next };
  answeringHosted.set(key, active);
  next.finally(() => { if (answeringHosted.get(key) === active) answeringHosted.delete(key); }).catch(() => {});
  return next;
}

async function answerHostedCall(id, home, entry, event, key) {
  if (!event?.call || entry.controller?.signal.aborted || entry.answered.has(event.call) || !client.tools.byName.has(event.name)) return;
  entry.answered.add(event.call);
  const session = dirOf(id);
  const epoch = pageEpoch(session);
  const cancelled = () => entry.controller?.signal.aborted || pageEpoch(session) !== epoch || pageStopped(session);
  try {
    const remote = hostedClient(home.url);
    const kept = await read('kv', key);
    let stopping = await read('kv', hostedStopKey(id));
    // A saved Stop holds back only the turn it stops (a later turn, started on the host's
    // own screen say, is not refused its page tools by an older Stop).
    if (stopping?.target) {
      const runs = hostedTurns.get(id)?.request ?? (await remote.session(home.session).catch(() => null))?.running_request ?? null;
      if (runs && runs !== stopping.target) stopping = null;
    }
    if (cancelled()) return;
    if (stopping) {
      await remote.appToolResult(home.session, event.call, { error: 'the user asked to stop this turn; its Stop is being delivered' });
      return;
    }
    if (!kept) {
      await writeDurably('kv', key, { started: true });
    }
    let answer = kept?.answer;
    if (!answer) {
      answer = cancelled() ? { error: 'the turn ended or was stopped before the page tool started' }
        : await client.tools.answer({ ...event, again: Boolean(event.again || kept), session });
      await writeDurably('kv', key, { started: true, answer });
    }
    if (entry.controller?.signal.aborted) return;
    try {
      await remote.appToolResult(home.session, event.call, answer);
    } catch (error) {
      if (error?.status === 409) return;
      if (error?.status !== 400) throw error;
      const refused = refusedAnswer(error);
      await writeDurably('kv', key, { started: true, answer: refused });
      await remote.appToolResult(home.session, event.call, refused);
    }
  } catch (error) {
    entry.answered.delete(event.call);
    if (error?.status !== 409) {
      console.warn('Spindle: a page tool answer did not reach your host:', error);
      if (followed.get(id) === entry && !entry.controller.signal.aborted) {
        setTimeout(() => {
          if (followed.get(id) === entry && !entry.controller.signal.aborted) answerHosted(id, home, entry, event);
        }, 1000);
      }
    }
  }
}

async function clearHostedTools(id) {
  const changes = (await keysWith('kv', `host-tool:${id}:`))
    .map((key) => ['kv', key, undefined]);
  if (changes.length) await writeTogether(changes, 'strict');
}

/** A hosted thread's turn ended (`done`: its `command_done`; null when only the host's
 * state says so): its state as the host has it, and the panels told, as for a turn here. */
const endingHosted = new Map();
function endHosted(id, done) {
  if (endingHosted.has(id)) return endingHosted.get(id);
  const next = endHostedCall(id, done);
  endingHosted.set(id, next);
  next.finally(() => { if (endingHosted.get(id) === next) endingHosted.delete(id); }).catch(() => {});
  return next;
}

async function endHostedCall(id, done) {
  const mine = hostedTurns.get(id);
  const thread = await read('threads', id);
  if (!thread?.home) return;
  let state = 'idle';
  let note = null;
  let complete = false;
  try {
    const remote = hostedClient(thread.home.url);
    const receipt = done ?? (mine ? await remote.receipt(mine.request).catch(() => null) : null);
    const object = await remote.session(thread.home.session);
    if (object.busy) {
      // Still at work (another command of the thread's): its own end tells.
      return;
    }
    complete = Boolean(object.complete);
    if (receipt?.state === 'failed') {
      // Stopped by the user mid-step, the host records the step as cut off: a stop, as
      // a thread here says it.
      state = mine?.stopped ? 'stopped' : 'failed';
      note = receipt.error ?? 'it failed on your host';
    } else if (receipt?.state === 'interrupted') {
      state = 'interrupted';
      note = receipt.error ?? 'your host restarted while it ran';
    } else if (!object.complete && object.head > 0 && !object.accepts_prompt) {
      state = 'stopped';
      note = object.stop_text ?? null;
    }
  } catch (error) {
    // The host went away as the turn ended: the poll says how it ended once it answers.
    await away(id, sentence(error)).catch(() => {});
    return;
  }
  if (hostedTurns.get(id) !== mine) return;
  hostedTurns.delete(id);
  if (mine) await forgetHostedCommand(id, mine.request);
  cancelPageTools(id);
  if (complete) await clearHostedTools(id);
  await touch(id, { state, note });
  await holdUnsent(id, done?.unsent ?? []);
  send({ type: 'turn_end', thread: id, unsent: [] });
  const waiting = queued.get(id);
  if (state !== 'running' && waiting && !waiting.held) sendQueuedHosted(id);
  // Done, and resting (as for a turn here, `afterTurn`): the tabs it opened close, its
  // tasks' once the host says none is at work.
  else if (complete && state === 'idle' && !done?.unsent?.length) turnTabsDone(id, () => hostedClient(thread.home.url).tasks(thread.home.session)).catch(() => {});
}

/** The message (with files) that waited for a hosted thread's turn to end goes now; one
 * the thread cannot take is held, saying why, as for a thread here. */
async function sendQueuedHosted(id) {
  const waiting = queued.get(id);
  if (!waiting) return;
  try {
    const thread = await workable(id);
    const object = await hostedClient(thread.home.url).session(thread.home.session);
    if (queued.get(id) !== waiting) return;
    if (object.busy || hostedTurns.has(id)) return;
    if (!object.accepts_prompt) {
      throw failure('busy', object.accepts_message
        ? 'the last turn was stopped part way and takes a message without files: send the text alone, or Continue, Retry or Take back first'
        : 'the last turn did not finish: retry, continue or take it back first');
    }
    // Its turn's command is saved (and sent again until the host confirms it) before
    // this answers, so the waiting copy may go.
    await onHost.send({ id, text: waiting.text, files: waiting.files }, null, true);
    queued.delete(id);
    await dropQueued(waiting).catch(() => {});
    announceQueued(id);
  } catch (error) {
    if (queued.get(id) !== waiting) return;
    waiting.held = String(error.message ?? error);
    await keepQueued(waiting).catch(() => {});
    announceQueued(id);
  }
}

/** Starts a turn of hosted thread `id` (`kind` say, continue or retry) on the host: its
 * event stream is open first, so no event of the turn is missed, and the command goes
 * with a request ID of its own (sent again, if need be, with the same one). */
async function startHosted(id, kind, fields = {}, keep = null) {
  const epoch = pageEpoch(dirOf(id));
  await endingHosted.get(id);
  const thread = await workable(id);
  if (hostedTurns.has(id) || thread.state === 'running') throw failure('busy', 'this thread is already running');
  if (await read('kv', hostedStopKey(id))) throw failure('busy', 'a saved Stop is still being delivered: wait before starting another turn');
  const remote = hostedClient(thread.home.url);
  const entry = { kind, stopped: false };
  hostedTurns.set(id, entry);
  let object;
  try {
    object = await remote.session(thread.home.session);
    if (object.busy) throw failure('busy', 'this thread is already running on your host');
    await follow(id, true).opened;
    if (entry.stopped || epoch !== pageEpoch(dirOf(id))) throw failure('stopped', 'the turn was stopped before it started');
  } catch (error) {
    if (hostedTurns.get(id) === entry) hostedTurns.delete(id);
    throw error;
  }
  const request = requestId();
  entry.request = request;
  const command = { request, kind, fields: { session: thread.home.session, ...fields, expected_head: object.head } };
  let next;
  try {
    entry.saved = update('threads', id, (current) => ({ ...current, state: 'running', note: null, updated: Date.now() }), {
      changes: [['kv', hostedCommandKey(id), command], ['bundles', hostedFilesKey(id), keep ? { request, ...keep } : undefined]], durability: 'strict',
    });
    next = await entry.saved;
    if (!next) throw failure('not_found', 'there is no such thread');
  } catch (error) {
    if (hostedTurns.get(id) === entry) hostedTurns.delete(id);
    throw error;
  }
  send({ type: 'thread', thread: next });
  try {
    await admitHosted(id, command);
  } catch (error) {
    if (worthRepeating(error)) {
      await away(id, `the host has not confirmed this message; Spindle retries the same request, never a new one (${sentence(error)})`);
      hostedTick(1000);
      return { started: true, pending: true };
    }
    await forgetHostedCommand(id, request);
    hostedTurns.delete(id);
    await touch(id, { state: thread.state, note: thread.note ?? null });
    send({ type: 'turn_end', thread: id, unsent: [] });
    throw hostFailure(error);
  }
  return { started: true };
}

/** A command that runs in the host's worker (a rewind), waited for: its receipt. */
async function settledCommand(remote, kind, fields, options) {
  let receipt = await remote.command(kind, fields, options);
  if (receipt.state === 'running') receipt = await remote.settled(receipt.request_id);
  if (receipt.state !== 'completed') throw failure('failed', receipt.error ?? `the ${kind} did not finish on your host`);
  return receipt;
}

/** The rewind target whose history is `shown`'s first `at` messages, among the targets
 * the host offers (its newest 48 steps), newest first; null when none is, undefined when
 * the host offers none (it lists none while a command runs there). */
async function hostedPointBefore(remote, session, shown, at) {
  const before = JSON.stringify(shown.slice(0, at));
  const targets = await remote.targets(session);
  if (!targets.length) return undefined;
  for (const target of targets) {
    const { messages = [] } = await remote.messages(session, target.revision);
    if (messages.length === at && JSON.stringify(messages) === before) return target.revision;
  }
  return null;
}

/** The last turnFiles answer of a hosted thread, by thread and revision. */
let hostedFilesKept = { key: null, value: null };

/** What the browser's fit would change for a hosted thread's settings to run here (the
 * package's `fit`, said ahead: the import's own decides, and the thread says it). */
function fitAhead(settings) {
  const notes = [];
  let tools = settings.builtin_tools ?? 'all';
  let runs = settings.runner ?? 'none';
  if (tools === 'all') {
    tools = 'files';
    notes.push({ setting: 'builtin_tools', to: 'files' });
  }
  if (tools === 'files' && runs === 'none') {
    runs = 'app';
    notes.push({ setting: 'runner', to: 'app' });
  }
  if (settings.network && runs !== 'none') notes.push({ setting: 'network', to: true });
  if (settings.toolchain) notes.push({ setting: 'toolchain', to: false });
  if (settings.sources?.length) notes.push({ setting: 'sources', to: [] });
  if (settings.max_usd != null) notes.push({ setting: 'max_usd', to: settings.max_usd });
  return fitForChrome({ notes, refused: settings.self_edit ? [{ why: 'a self-edit session runs only on its native host' }] : [] }, runs);
}

/** Refuses what only a thread here can do. */
const hereOnly = (doing) => () => {
  throw failure('invalid', `This thread is on your host: bring it back to this browser to ${doing}`);
};

/** The operations of a hosted thread, by the name of the one they stand in for; each
 * takes the operation's arguments and the operation for a thread here. */
const onHost = {
  async send({ id, text, files: given }, _here, queuedNow = false) {
    const thread = await workable(id);
    const remote = hostedClient(thread.home.url);
    const session = thread.home.session;
    const epoch = pageEpoch(dirOf(id));
    if (!queuedNow && (thread.state === 'running' || hostedTurns.has(id))) {
      if (!given?.length) {
        try {
          return { steered: (await remote.steer(session, text)).queued };
        } catch (error) {
          // Ended meanwhile: the message starts a turn of its own.
          if (error?.kind !== 'not_running') throw hostFailure(error);
        }
      } else {
        await enqueue(waitingMessage(id, text, given));
        return { queued: true };
      }
    }
    // Attachments go up first (each kept by the host a day), then the message names them.
    const note = given?.length ? attachedNote(given) : '';
    const sent = [];
    for (const file of given ?? []) {
      try {
        const { digest } = await remote.upload(session, file.bytes);
        sent.push({ path: String(file.path), digest });
      } catch (error) {
        throw hostFailure(error);
      }
    }
    if (epoch !== pageEpoch(dirOf(id))) throw failure('stopped', 'the turn was stopped before it started');
    // The files stay here too until the host takes the message (saved with its command,
    // under its request ID): refused later, it waits again with them, not as text alone.
    return startHosted(id, 'say', { text: text + note, ...(sent.length && { attachments: sent }) }, given?.length ? { text, files: given } : null);
  },

  async stop({ id }) {
    const thread = await read('threads', id);
    const mine = hostedTurns.get(id);
    if (mine) mine.stopped = true;
    cancelPageTools(id);
    if (mine && !mine.request) return { stopping: true };
    try {
      await mine?.saved;
      const pending = await read('kv', hostedCommandKey(id));
      let target = mine?.request ?? pending?.request;
      if (!target) target = (await hostedClient(thread.home.url).session(thread.home.session)).running_request;
      if (!target) return { stopping: false };
      const kept = await read('kv', hostedStopKey(id));
      // One Stop is kept per thread, for the turn that runs: one kept for an earlier turn
      // (which ended, as a later one runs) gives way to this one.
      if (!kept || kept.target !== target) await writeDurably('kv', hostedStopKey(id), { request: requestId(), target, url: thread.home.url, session: thread.home.session });
      if (pending) await admitHosted(id, pending);
      await sendHostedStop(id);
    } catch (error) {
      if (await read('kv', hostedStopKey(id))) {
        await away(id, `a saved Stop waits for your host (${sentence(error)})`);
        hostedTick(1000);
        return { stopping: true, pending: true };
      }
      if (error?.kind !== 'not_running') throw hostFailure(error);
    }
    return { stopping: true };
  },

  retry: ({ id }) => startHosted(id, 'retry'),
  resume: ({ id }) => startHosted(id, 'continue'),

  async open({ id, from }) {
    const thread = await read('threads', id);
    if (!thread) throw failure('not_found', 'there is no such thread');
    lookedAt.set(id, Date.now());
    if (thread.moving && thread.moving.step !== 'release') return { thread, status: null, history: null, running: false, moving: thread.moving };
    try {
      const remote = hostedClient(thread.home.url);
      const [object, history] = await Promise.all([remote.session(thread.home.session), remote.messages(thread.home.session)]);
      if (object.busy || thread.state === 'running') follow(id);
      await away(id, null);
      const messages = history.messages ?? [];
      return {
        thread: (await read('threads', id)) ?? thread, status: statusOf(object), running: Boolean(object.busy), queued: queuedOf(id),
        history: { messages: from ? messages.slice(from) : messages, total: messages.length, revision: history.revision },
      };
    } catch (error) {
      return { thread, status: null, history: null, running: thread.state === 'running', away: sentence(error) };
    }
  },

  async rewindTo({ id, at, revision }) {
    const thread = await workable(id);
    if (thread.state === 'running' || hostedTurns.has(id)) throw failure('busy', 'stop this thread before taking it back');
    const remote = hostedClient(thread.home.url);
    const session = thread.home.session;
    try {
      const object = await remote.session(session);
      const shown = (await remote.messages(session, revision ?? undefined)).messages ?? [];
      const message = shown[at];
      if (message?.role !== 'user') throw failure('invalid', 'that is not one of your messages');
      const { text, paths } = unattached(message.content.filter((block) => block.type === 'text').map((block) => block.steer ?? block.text).join('\n'));
      if (at === 0) throw failure('invalid', 'On your host a thread cannot go back to before its first message: bring it back to this browser for that, or start a new thread');
      const files = [];
      for (const path of paths) {
        try {
          files.push({ path, bytes: await remote.read(session, path, revision ?? undefined) });
        } catch (error) {
          if (error?.status !== 404) throw error;
        }
      }
      const target = await hostedPointBefore(remote, session, shown, at);
      if (target == null) throw failure('not_found', 'Your host no longer offers the point before that message (it keeps its newest 48 steps to go back to): bring the thread back to this browser to go further back');
      await settledCommand(remote, 'rewind', { session, revision: target, expected_head: object.head });
      await touch(id, { state: 'idle', note: null });
      return { text, files };
    } catch (error) {
      throw hostFailure(error);
    }
  },

  async earlier({ id, revision }) {
    const thread = await workable(id);
    const remote = hostedClient(thread.home.url);
    const session = thread.home.session;
    const shown = (await remote.messages(session, revision ?? undefined)).messages ?? [];
    const summary = JSON.stringify(shown[0] ?? null);
    const targets = await remote.targets(session);
    // None listed: a command runs there (a thread with a summary has steps to go back to).
    if (!targets.length) throw failure('busy', NOT_WHILE_WORKING);
    for (const target of targets) {
      if (revision != null && target.revision >= revision) continue;
      const history = await remote.messages(session, target.revision);
      if (JSON.stringify(history.messages?.[0] ?? null) !== summary) return { revision: target.revision, history: { ...history, total: (history.messages ?? []).length } };
    }
    throw failure('not_found', 'Your host no longer offers the messages before that summary');
  },

  async settings({ id, changes = {} }) {
    const thread = await workable(id);
    const remote = hostedClient(thread.home.url);
    const session = thread.home.session;
    // Its commands run in the host's sandbox while it is there: where they run is not
    // this thread's to choose until it is back.
    const asked = Object.fromEntries(Object.entries(changes).filter(([key]) => ['provider', 'model', 'network', 'effort', 'compaction', 'compact_tokens', 'max_usd', 'tasks'].includes(key)));
    try {
      if (Object.keys(asked).length) await settledCommand(remote, 'settings', { session, ...asked });
      const settings = (await remote.session(session)).settings ?? {};
      await touch(id, { provider: settings.provider, model: settings.model, effort: settings.effort ?? 'default', network: Boolean(settings.network), tasks: Boolean(settings.tasks) });
      return settings;
    } catch (error) {
      throw hostFailure(error);
    }
  },

  async spend({ id }) {
    const thread = await read('threads', id);
    const object = await hostedClient(thread.home.url).session(thread.home.session);
    const usage = object.usage ?? {};
    return {
      max_usd: object.settings?.max_usd ?? null, compact_tokens: object.settings?.compact_tokens ?? null, usd: usage.usd ?? 0, calls: usage.calls ?? 0,
      subscription: usage.subscription ?? 0, unpriced: usage.unpriced ?? 0, unknown: usage.unknown ?? 0,
    };
  },

  /** Goes on with (or takes back, `resend` false) a turn that stopped before it
   * finished, on the host, as a thread here does (`ops.restart`): from just before its
   * newest model request, else from the end of the turn before; never wiped. */
  async restart({ id, changes = {}, resend = true }) {
    const thread = await workable(id);
    if (thread.state === 'running' || hostedTurns.has(id)) throw failure('busy', 'stop this thread before starting it over');
    const remote = hostedClient(thread.home.url);
    const session = thread.home.session;
    try {
      const object = await remote.session(session);
      if (object.accepts_prompt) throw failure('invalid', 'this thread has no unfinished turn to start over');
      // Back to the newest completed turn, however far (an older host lists the newest
      // steps only, and says nothing of the turn's message).
      const point = await remote.turn(session);
      const targets = point.targets ?? [];
      const before = resend && point.prompt !== undefined && targets[0]?.kind === 'before_model' ? targets[0] : null;
      if (before) {
        if (before.revision !== object.head) await settledCommand(remote, 'rewind', { session, revision: before.revision, expected_head: object.head });
        if (Object.keys(changes).length) await onHost.settings({ id, changes });
        await touch(id, { state: 'idle', note: null });
        return { ...(await startHosted(id, 'continue')), from: 'request' };
      }
      const settled = targets.find((target) => target.kind === 'settled');
      if (!settled) {
        throw failure('invalid', point.completed === false
          ? 'On your host a first turn cannot be taken back or started over: bring the thread back to this browser for that, or delete it'
          : "On your host this thread's earlier turns cannot be gone back to now (a background task may still be at work): wait for it, or Retry or Continue the turn. Nothing was changed");
      }
      let found = point.prompt;
      if (found === undefined) {
        const kept = ((await remote.messages(session, settled.revision)).messages ?? []).length;
        const prompt = ((await remote.messages(session)).messages ?? []).slice(kept)
          .find((message) => message.role === 'user' && message.content?.some((block) => block.type === 'text' && block.text.trim()));
        found = prompt && prompt.content.filter((block) => block.type === 'text').map((block) => block.text).join('\n');
      }
      if (!found) throw failure('invalid', "this turn's message was not found");
      const { text, paths } = unattached(found);
      const files = [];
      for (const path of paths) {
        try {
          files.push({ path, bytes: await remote.read(session, path) });
        } catch (error) {
          if (error?.status !== 404) throw error;
        }
      }
      await settledCommand(remote, 'rewind', { session, revision: settled.revision, expected_head: object.head });
      // Taken out of the host's history now: whatever fails before its turn takes it, the
      // message waits here, held, with its files.
      try {
        if (Object.keys(changes).length) await onHost.settings({ id, changes });
        await touch(id, { state: 'idle', note: null });
        if (!resend) return { text, files };
        return { ...(await onHost.send({ id, text, files })), from: 'turn' };
      } catch (error) {
        if (resend) await requeue(id, waitingMessage(id, text, files), sentence(error)).catch(() => {});
        throw error;
      }
    } catch (error) {
      throw hostFailure(error);
    }
  },

  async rename({ id, title }) {
    const thread = await read('threads', id);
    const name = String(title).trim().slice(0, 80) || 'Untitled';
    // The host's name follows when it can; this browser's is the one shown.
    await hostedClient(thread.home.url).command('rename', { session: thread.home.session, name }).catch(() => {});
    return touch(id, { title: name });
  },

  /** Deletes the thread where it lives (the host), then what is left of it here. */
  async remove({ id }, here) {
    const thread = await read('threads', id);
    if (thread.state === 'running' || hostedTurns.has(id)) throw failure('busy', 'stop this thread before deleting it');
    if (thread.moving && thread.moving.step !== 'release') throw failure('busy', 'this thread is moving: wait for it to finish, then delete it');
    try {
      await settledCommand(hostedClient(thread.home.url), 'delete', { session: thread.home.session });
    } catch (error) {
      if (error?.status !== 404) throw failure('host', `it lives on your host, which did not delete it (${sentence(error)})`);
    }
    followed.get(id)?.controller.abort();
    return here({ id });
  },

  async files({ id, revision }) {
    const thread = await read('threads', id);
    return hostedClient(thread.home.url).files(thread.home.session, revision).catch((error) => { throw hostFailure(error); });
  },

  async read({ id, path }) {
    const thread = await read('threads', id);
    return { bytes: await hostedClient(thread.home.url).read(thread.home.session, path).catch((error) => { throw hostFailure(error); }) };
  },

  async turnFiles({ id }) {
    const thread = await read('threads', id);
    const remote = hostedClient(thread.home.url);
    const session = thread.home.session;
    const now = await remote.files(session);
    const key = `${id}@${now.revision}`;
    if (hostedFilesKept.key === key) return hostedFilesKept.value;
    const shown = (await remote.messages(session)).messages ?? [];
    const at = shown.findLastIndex((message) => message.role === 'user' && (message.content ?? []).some((block) =>
      block.type === 'text' && typeof block.steer !== 'string' && block.text.trim() && !block.text.startsWith('[Earlier conversation compacted')));
    let before = null;
    // As turnFiles above: not kept while the host cannot look back yet.
    let lasting = true;
    if (at === 0) {
      before = [];
    } else if (at > 0) {
      const revision = await hostedPointBefore(remote, session, shown, at).catch(() => null);
      if (revision === undefined) lasting = false;
      else if (revision != null) before = (await remote.files(session, revision)).files.map((file) => [file.path, file.digest]);
    }
    const value = { ...now, before };
    if (lasting) hostedFilesKept = { key, value };
    return value;
  },

  // A hosted thread's tasks show as its steps; their cards are this browser's only.
  tasks: async () => ({ tasks: [], activity: null }),
  async taskStop({ id, task }) {
    const thread = await read('threads', id);
    return hostedClient(thread.home.url).taskStop(thread.home.session, String(task));
  },

  export: hereOnly('move it to a file'),
  compact: hereOnly('free space in it'),
  dropAside: hereOnly('remove its first attempts'),
  finishedTasks: async () => [],
  taskExport: hereOnly('save its tasks'),
  taskDrop: hereOnly('free its tasks'),
  freeSealed: hereOnly('free its sealed copy'),
  bundle: hereOnly('save it as a file'),
};

// Every operation on a thread goes to the host for a thread that lives there (and
// `open` also shows a thread in a move as it stands, touching no copy of it).
for (const [name, hosted] of Object.entries(onHost)) {
  const here = ops[name];
  ops[name] = async (args) => {
    const thread = args?.id ? await read('threads', args.id) : null;
    if (thread?.home) return hosted(args, here);
    if (name === 'open' && thread?.moving && thread.moving.step !== 'release') return hosted(args, here);
    return here(args);
  };
}
// A new thread starts with the files of a thread here only.
{
  const here = ops.create;
  ops.create = async (args) => {
    if (args?.from && (await read('threads', args.from))?.home) throw failure('invalid', 'That thread is on your host: bring it back to this browser to start a thread with its files');
    return here(args);
  };
}
// A thread is deleted with its move's record (one that is only removing the host's
// sealed copy tries that first).
{
  const here = ops.remove;
  ops.remove = async (args) => {
    const thread = await read('threads', args.id);
    if (thread?.moving && thread.moving.step !== 'release') throw failure('busy', 'this thread is moving: wait for it to finish, then delete it');
    if (thread?.moving?.step === 'release') {
      notRunning(args.id, 'deleting');
      const move = await read('kv', MOVE(args.id));
      if (move?.session) {
        try {
          await settledCommand(hostedClient(move.url), 'delete', { session: move.session });
        } catch (error) {
          if (error?.status !== 404) throw error;
        }
      }
      // The move ends before the thread goes, in one transaction: a stop between leaves
      // a plain thread here to delete again, never a move whose thread is gone.
      const now = await read('threads', args.id);
      if (now) await writeTogether([['threads', args.id, { ...now, moving: null }], ['kv', MOVE(args.id), undefined], ['bundles', MOVE(args.id), undefined]], 'strict');
    }
    const done = await here(args);
    await clearHostedTools(args.id);
    await writeTogether([['kv', hostedCommandKey(args.id), undefined], ['kv', hostedStopKey(args.id), undefined], ['kv', MOVE(args.id), undefined], ['bundles', MOVE(args.id), undefined], ['bundles', hostedFilesKey(args.id), undefined]], 'strict');
    return done;
  };
}

/** Keeps hosted threads' states as the host says them (GET /v1/sessions): follows one
 * that runs, ends one this engine missed the end of, says when the host cannot be
 * reached; lets go of the streams of threads that no longer run and that no panel has
 * open. True while there are hosted threads. */
async function pollHosted() {
  const hosted = (await threads()).filter((thread) => thread.home);
  for (const [id, entry] of followed) {
    const thread = hosted.find((one) => one.id === id);
    if (!thread || (thread.state !== 'running' && !hostedTurns.has(id) && Date.now() - (lookedAt.get(id) ?? 0) > LOOK_MS)) entry.controller.abort();
  }
  if (!hosted.length) return false;
  let rows = null;
  let why = null;
  try {
    rows = await hostedClient().sessions();
  } catch (error) {
    why = hostState.connected && hostState.kind === 'browser'
      ? `your host does not answer (${sentence(error)}); Spindle keeps trying, and nothing is lost`
      : sentence(error);
  }
  for (const seen of hosted) {
    // As it is now (the awaits for threads before it can take a while): one that began
    // moving back, or came back, meanwhile is its move's to tell.
    const thread = await read('threads', seen.id).catch(() => null);
    if (!thread?.home || thread.moving) continue;
    const command = await read('kv', hostedCommandKey(thread.id));
    if (command) {
      try {
        await admitHosted(thread.id, command);
      } catch (error) {
        if (worthRepeating(error) || error?.status === 401 || error?.kind === 'host') {
          await away(thread.id, `a saved request waits for your host (${sentence(error)})`);
        } else {
          hostedTurns.delete(thread.id);
          await touch(thread.id, { state: 'failed', note: sentence(error) });
          // The refused message waits, held, for the user (whether or not a panel shows
          // this thread now), with the files it was sent with; then its record goes.
          const found = command.kind === 'say' ? await read('bundles', hostedFilesKey(thread.id)).catch(() => null) : null;
          // Only this command's own files (a record of another request is never its).
          const kept = found?.request === command.request ? found : null;
          if (kept?.files?.length) await requeue(thread.id, waitingMessage(thread.id, kept.text, kept.files), sentence(error)).catch((failed) => console.warn('Spindle: a refused message could not be kept:', failed));
          else await holdUnsent(thread.id, command.kind === 'say' ? [command.fields.text] : []);
          await forgetHostedCommand(thread.id, command.request);
          send({ type: 'turn_end', thread: thread.id, unsent: [] });
        }
      }
      continue;
    }
    const stop = await read('kv', hostedStopKey(thread.id));
    if (stop) {
      try { await sendHostedStop(thread.id); }
      catch (error) { await away(thread.id, `a saved Stop waits for your host (${sentence(error)})`); continue; }
    }
    if (!rows || thread.home.url !== hostState.url) {
      await away(thread.id, rows ? `this thread is on the host at ${thread.home.url}, which is not the one connected` : why);
      continue;
    }
    const row = rows.find((one) => one.id === thread.home.session);
    if (!row) {
      await away(thread.id, 'your host no longer has this thread');
      continue;
    }
    if (!followed.has(thread.id)) await away(thread.id, null);
    const [state, note] = listed(row.status?.state) ?? [null, null];
    if (!state) continue;
    if (state === 'running') {
      follow(thread.id);
      if (thread.state !== 'running') await touch(thread.id, { state: 'running', note: null });
    } else if (thread.state === 'running') {
      // It ended while no stream told this engine (a gap, or another screen ran it).
      const mine = hostedTurns.get(thread.id);
      const receipt = mine?.request && hostRemote ? await hostRemote.receipt(mine.request).catch((error) => {
        if (error?.status === 404) return { state: 'interrupted', error: 'the host no longer retains this command receipt' };
        return null;
      }) : null;
      if (!mine || (receipt && receipt.state !== 'running')) await endHosted(thread.id, receipt);
    } else if (state !== thread.state && !(HALTED_STATES.has(state) && HALTED_STATES.has(thread.state))) {
      await touch(thread.id, { state, note });
    }
  }
  return true;
}
/** States in which a turn stopped part way (each keeps its own note). */
const HALTED_STATES = new Set(['stopped', 'failed', 'interrupted']);

/** Every quarter minute while there are hosted threads or moves: their states, and the
 * moves that wait for the host. */
let ticking = null;
/** When the scheduled tick is due, and whether one runs now (one at a time). */
let tickDue = 0;
let tickRunning = false;
/** Brings hosted threads and moves up to date in `ms` (sooner than a tick already set
 * for later), then every 15 seconds while any are left. */
function hostedTick(ms = 15_000) {
  const due = Date.now() + ms;
  if (ticking && tickDue <= due) return;
  clearTimeout(ticking);
  tickDue = due;
  ticking = setTimeout(async () => {
    ticking = null;
    if (tickRunning) {
      hostedTick(1000);
      return;
    }
    tickRunning = true;
    let more = false;
    try {
      if (!hostState.connected && hostState.url && await vault.usableNow()) await connectKeptHost().catch(() => {});
      if ((await moves.list()).length) {
        more = true;
        moves.resume().catch(() => {});
      }
      if (await pollHosted()) more = true;
    } catch (error) {
      more = true;
      console.warn('Spindle: hosted threads were not brought up to date:', error);
    } finally {
      tickRunning = false;
    }
    if (more) hostedTick();
  }, ms);
  ticking?.unref?.();
}

/** The host connected: moves waiting for it go on, and hosted threads catch up. */
function hostedAwake() {
  if (hostState.kind !== 'browser') return;
  moves.resume().catch(() => {});
  pollHosted().catch(() => {});
  hostedTick();
}

Object.assign(ops, {
  /** This browser's name on the user's host (`base`, such as "Edge on Windows", with a
   * short id of this profile's, kept): connecting again keeps its threads. */
  async hostName({ base }) {
    const kept = await read('kv', 'host-name');
    if (kept) return { name: kept };
    const id = [...crypto.getRandomValues(new Uint8Array(3))].map((byte) => byte.toString(16).padStart(2, '0')).join('');
    const name = `${String(base ?? 'Spindle for Browser').replace(/[\u0000-\u001f\u007f]/g, '').trim().slice(0, 50) || 'Spindle for Browser'} ${id}`;
    await write('kv', 'host-name', name);
    return { name };
  },

  /** One-click connect: trades the code the host's page gave (`code`, with its PKCE
   * `verifier`) for a browser token named `client`, keeps it sealed where the host's
   * token lives, and connects the host with it. */
  async hostConnect({ url, code, verifier, client: name }) {
    hostGeneration += 1;
    const base = String(url).replace(/\/+$/, '');
    // A code is good for one trade: none while its token could not be kept.
    if (!(await vault.usableNow())) throw failure('locked', "Spindle's keys are locked: unlock them, then connect your host");
    let token;
    try {
      ({ token } = await new RemoteClient({ url: base, fetch: reach }).connectToken(String(code), String(verifier)));
    } catch (error) {
      throw failure(error.kind ?? 'host', `your host did not give this browser a token (${sentence(error)})`);
    }
    // Kept at once: the trade replaced any earlier token of this browser on that host, so
    // the new one is the only one that works there now.
    await vault.keepHost(token, [['kv', 'host', { url: base, client: name }]]);
    try {
      await changeHost({ url: base, token, client: name });
    } catch (error) {
      const why = error.message ?? error;
      if (!hostState.connected || hostState.url === base) {
        // Its earlier connection (if any) used the replaced token: tried again with the
        // new one shortly.
        hostState = { url: base, connected: false, error: `not connected yet (${why})`, kind: null, client: name };
        send({ type: 'host', host: hostState });
        hostedTick(1000);
        throw failure('host', `your host gave this browser a token (kept here) but did not connect yet (${why}); Spindle tries again shortly`);
      }
      throw failure('host', `your host gave this browser a token (kept here) but did not connect (${why}); still connected to ${hostState.url} until Spindle starts again`);
    }
    return snapshot();
  },

  /** Whether thread `id` can move to the host connected now, and on which model:
   * `{url, fits, provider, model, providers}` (`providers`: the host's signed-in ones
   * and their models; `fits`: the thread's own model is among them), or `{why}`. */
  async hostPreflight({ id }) {
    const thread = await workable(id);
    if (thread.home) throw failure('invalid', 'this thread is on your host already');
    let remote;
    try {
      remote = hostedClient();
    } catch (error) {
      return { why: error.message };
    }
    let info;
    let models;
    try {
      [info, models] = await Promise.all([remote.info(), remote.models()]);
    } catch (error) {
      return { why: `your host at ${hostState.url} does not answer (${sentence(error)})` };
    }
    if (!(info.features ?? []).includes('threads')) return { why: 'your host is older than this extension: update it to run threads there' };
    const providers = (models.providers ?? []).filter((provider) => provider.signed_in).map((provider) => ({
      id: provider.id, name: provider.name ?? provider.id, default_model: provider.default_model,
      models: (provider.models ?? []).map((model) => ({ id: model.id, label: model.label ?? model.id })),
    }));
    const own = providers.find((provider) => provider.id === thread.provider);
    const fits = Boolean(own?.models.some((model) => model.id === thread.model));
    return { url: hostState.url, fits, provider: thread.provider, model: thread.model, providers };
  },

  /** Moves thread `id` to the connected host (`provider` and `model`: the host's to run
   * it on, when its own is not signed in there). Answers once the move is recorded; the
   * thread says how it goes. */
  async toHost({ id, provider, model }) {
    const thread = await workable(id);
    notRunning(id, 'moving it');
    if (thread.home) throw failure('invalid', 'this thread is on your host already');
    if (thread.moving) throw failure('busy', 'the previous move is still finishing: wait for its host cleanup before moving it again');
    if (queued.has(id)) throw failure('busy', 'a message waits for this thread: send it or take it back first');
    hostedClient();
    const route = provider && model ? { provider: String(provider), model: String(model) } : {};
    const { done } = await moves.toHost(id, { url: hostState.url, name: thread.title || 'Untitled', ...route });
    watchMove(done);
    return { moving: true };
  },

  /** What changes for hosted thread `id` to run here, said before it comes back. */
  async backPreview({ id }) {
    const thread = await read('threads', id);
    if (!thread?.home) throw failure('invalid', 'this thread is not on your host');
    const object = await hostedClient(thread.home.url).session(thread.home.session).catch((error) => { throw hostFailure(error); });
    if (object.busy) throw failure('busy', 'it is at work on your host: stop it first');
    return { said: fitAhead(object.settings ?? {}).said };
  },

  /** Brings hosted thread `id` back to this browser (see host-moves.mjs). */
  async backHere({ id }) {
    const thread = await read('threads', id);
    if (!thread?.home) throw failure('invalid', 'this thread is not on your host');
    if (thread.moving) throw failure('busy', 'this thread is moving already');
    if (thread.state === 'running' || hostedTurns.has(id)) throw failure('busy', 'stop this thread before bringing it back');
    hostedClient(thread.home.url);
    if (await read('kv', hostedStopKey(id))) throw failure('busy', 'a saved Stop is still being delivered: wait before moving this thread');
    await headroom();
    const known = followed.get(id);
    known?.controller.abort();
    followed.delete(id);
    cancelPageTools(id);
    const { done } = await moves.back(id);
    watchMove(done);
    return { moving: true };
  },
});

/** A thread found in storage with no index entry, listed again from its own store. */
async function relist(id) {
  const status = await within(id, () => client.status(dirOf(id))).catch(() => null);
  const now = Date.now();
  await writeDurably('threads', id, {
    id, title: 'Found again', created: now, updated: now,
    provider: status?.provider ?? null, model: status?.model ?? null, effort: status ? effortOf(status) : 'default',
    runner: status?.settings?.runner ?? 'none', network: Boolean(status?.settings?.network), tasks: Boolean(status?.settings?.tasks),
    state: status?.accepts_prompt === false ? 'interrupted' : 'idle',
    note: 'This thread was in storage without its entry in the list, so it is listed again (rename it from its menu).',
    summaries: true,
  });
}

const starting = (async () => {
  try {
    // Compiled once: a renewal instantiates it again.
    compiled = await WebAssembly.compile(await (await fetch(new URL('./web/spindle_embed_bg.wasm', import.meta.url))).arrayBuffer());
    memory = (await init({ module_or_path: compiled })).memory;
    await start();
    // Keys saved by a build that kept them as text are sealed now.
    await vault.sealPlain().catch((error) => console.error('Spindle: keys could not be sealed:', error));
    // (By key: not every waiting message's files read to find them.)
    for (const key of await keysWith('kv', 'reset:')) {
      const record = await read('kv', key);
      if (record?.type === 'reset') await restoreFirst(record);
    }
    // A reset backup no record names (an earlier build's, which deleted them) is a whole
    // copy of its thread as it was: kept as an earlier version, never deleted here.
    // An earlier version whose copy is gone (its removal cut short) goes from the list.
    for (const key of await keysWith('kv', EARLIER)) {
      if (!(await has('bundles', key.slice(key.indexOf(':prior:') + 1)))) await remove('kv', key).catch(() => {});
    }
    const named = new Set((await keysWith('kv', EARLIER)).map((key) => key.slice(key.indexOf(':prior:') + 1)));
    for (const key of await keysOf('bundles')) {
      const [, id, time] = key.split(':');
      if (key.startsWith('prior:') && !named.has(key)) await keepEarlier(id, key, Number(time) || Date.now()).catch(() => {});
    }
    // A thread the index still calls running was cut off when Chrome or the engine stopped.
    const known = new Set();
    const files = new Set(await keysOf('bundles'));
    for (const thread of await threads()) {
      known.add(thread.id);
      // (One on the user's host may run there still: the host says, once it answers.)
      if (thread.state === 'running' && !thread.home) {
        await touch(thread.id, { state: 'interrupted', note: `${BROWSER} stopped while it ran` });
      }
      const fence = `/threads/${thread.id}/store/authority.sqlite.lock`;
      const fs = await openPool();
      let moved = thread.state === 'moved';
      // A thread on the user's host, or moving there or back, keeps its fence: its move
      // (below) says where it is.
      if (!moved && !thread.home && !thread.moving && fs.existsSync(fence) && fs.readFileSync(fence).startsWith('spindle-moved: ')) {
        await touch(thread.id, { state: 'moved', note: null });
        moved = true;
      }
      // A file a move wrote before the engine stopped short of sealing the thread: the
      // thread is still here, whole, so the file is a stale copy (tidying only).
      if (!moved && files.has(thread.id)) await remove('bundles', thread.id).catch(() => {});
    }
    // Files no thread owns (an engine stopped between writing a thread and indexing it,
    // or an index lost or damaged): a store there is a thread, listed again, never
    // deleted; a directory holding no store was a thread's start that never took.
    const fs = await openPool();
    for (const name of fs.readdirSync('/threads')) {
      // A settings check cut short (`checkCreation`): never a thread.
      if (name.startsWith('probe-')) {
        fs.rmSync(`/threads/${name}`, { recursive: true, force: true });
        continue;
      }
      if (known.has(name) || !/^[0-9a-f]{16}$/.test(name)) continue;
      if (fs.existsSync(`/threads/${name}/store/authority.sqlite`)) await relist(name).catch((error) => console.error('Spindle: an unlisted thread was not listed again:', error));
      else if (!fs.files().some(({ path }) => path.startsWith(`/threads/${name}/`))) fs.rmSync(`/threads/${name}`, { recursive: true, force: true });
    }
    // Messages that waited for a turn's end when the engine stopped.
    await restoreQueued().catch((error) => console.warn('Spindle: waiting messages:', error));
    // Moves to or from the user's host go on from where they were recorded (those that
    // need the host wait for it to connect).
    moves.resume().catch(() => {});
    hostedTick(1000);
  } catch (error) {
    broken = `the engine could not start: ${error.message ?? error}`;
  }
})();

/** Answers a request: on the asker's own channel when it asked on REQUESTS, else on the
 * shared one (the background's way). */
async function answerRequest(data, privately) {
  if (data?.to !== 'engine') return;
  const { from, id, op, args } = data;
  const reply = privately ? (message) => answerTo(from).postMessage(message) : send;
  try {
    await starting;
    while (renewing) await renewing.catch(() => {});
    if (broken) throw failure('broken', broken);
    if (!Object.hasOwn(ops, op)) throw failure('invalid', `there is no operation ${op}`);
    answering += 1;
    try {
      reply({ to: from, id, ok: true, result: await operate(op, args ?? {}) });
    } finally {
      answering -= 1;
    }
  } catch (error) {
    reply({ to: from, id, ok: false, error: { kind: error.kind ?? 'failed', message: String(error.message ?? error) } });
  }
}
channel.onmessage = ({ data }) => answerRequest(data, false);
requests.onmessage = ({ data }) => answerRequest(data, true);

await starting;
send({ type: 'ready', broken });
