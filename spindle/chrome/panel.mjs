// The panel: Spindle's UI in Chrome's side panel. It holds no session; the engine (an
// offscreen document's worker) runs them, and this page asks it over the channel and
// draws what it reports. Closing the panel never stops a turn.
import { fileTree, markdown, size } from './text.js';
import { BROWSER, PAGE_ACCESS, PICTURE_ACCESS, confirmMode, pageSwitches, pagesRecord } from './page-tools.mjs';
import { bundleFacts } from './clients/embedded.mjs';
import { zip } from './zip.mjs';
import { BUNDLE_MOST } from './host-moves.mjs';
import { FRAMED, containment, documentFrame, downloadName, kindOf, pageOf, pdfShows, pictureBlob, previewFrame, previewNote, saveFile, svgPage, typeOf } from './page.mjs';
import { MAX_DOCUMENT, documentPage } from './documents.mjs';
import { mdNodes } from './markdown.mjs';
import { CHANNEL, accessFor, keepPreviewChoice, keysOf, merge, previewChoice, read, remove, startEngine, take, write, REQUESTS, answersFor } from './shared.mjs';
import { CHATGPT_USAGE, connectInTab, flowOf, planSignIn, signInInTab, signInSites } from './signin.mjs';
import { clock, describe, duration, elapsed, isNotes, isScratch, isSpan, partsFrom, retried, shownName, tabsFrom, taskTitle } from './steps.mjs';

const $ = (id) => document.getElementById(id);
// What the panel's page says of the browser names the one it runs in (Edge, Brave or
// Opera: their prompts and their debugging bar are theirs, not Chrome's).
if (BROWSER !== 'Chrome') {
  const said = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  for (let node = said.nextNode(); node; node = said.nextNode()) {
    if (node.nodeValue.includes('Chrome')) node.nodeValue = node.nodeValue.replaceAll('Chrome', BROWSER);
  }
  for (const node of document.querySelectorAll('[title*="Chrome"], [placeholder*="Chrome"], [aria-label*="Chrome"]')) {
    for (const name of ['title', 'placeholder', 'aria-label']) {
      const value = node.getAttribute(name);
      if (value?.includes('Chrome')) node.setAttribute(name, value.replaceAll('Chrome', BROWSER));
    }
  }
}

const el = (tag, cls, text) => {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
};

/** Parts of the design that wait on what Spindle can do: shown once it can. `siteRules`:
 * Allow, Ask or Never by site for page actions (Settings > Pages), which the engine does
 * not keep or follow yet. */
const FEATURES = { siteRules: false };

/** Line icons on a 24-unit grid, drawn in the text's colour: `d` their paths (`box` and
 * `ring` draw rectangles and circles as paths), `w` their stroke width; `dots` and
 * `square` are filled. */
const box = (x, y, w, h) => `M${x} ${y}h${w}v${h}h${-w}z`;
const ring = (x, y, r) => `M${x - r} ${y}a${r} ${r} 0 1 0 ${2 * r} 0a${r} ${r} 0 1 0 ${-2 * r} 0`;
const ICONS = {
  eye: { d: ['M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z', ring(12, 12, 3)] },
  'eye-off': { d: ['M2 12s3.5-7 10-7c2 0 3.7.6 5.2 1.6M22 12s-3.5 7-10 7c-2 0-3.7-.6-5.2-1.6', 'M3 3l18 18'] },
  download: { d: ['M12 4v11M7 10l5 5 5-5M5 20h14'] },
  upload: { d: ['M12 15V4M7 9l5-5 5 5M5 20h14'], w: 2 },
  workspace: { d: [box(3, 4, 18, 16), 'M9 4v16', 'M12 9h6', 'M12 13h6'] },
  external: { d: ['M14 4h6v6', 'M20 4l-9 9', 'M18 14v6H4V6h6'] },
  taller: { d: ['M12 3v18', 'M8 7l4-4 4 4', 'M8 17l4 4 4-4'] },
  shorter: { d: ['M12 3v7', 'M12 14v7', 'M8 6l4 4 4-4', 'M8 18l4-4 4 4'] },
  copy: { d: [box(8, 8, 12, 12), 'M16 8V4H4v12h4'] },
  check: { d: ['M5 12.5l4.5 4.5L19 7.5'], w: 2.4 },
  send: { d: ['M12 19V5M6 11l6-6 6 6'], w: 2.2 },
  globe: { d: [ring(12, 12, 8.5), 'M3.5 12h17M12 3.5c2.5 2.5 3.5 5.5 3.5 8.5s-1 6-3.5 8.5c-2.5-2.5-3.5-5.5-3.5-8.5s1-6 3.5-8.5z'], w: 2 },
  folder: { d: ['M3 6h6l2 2h10v11H3z'] },
  prev: { d: ['M15 6l-6 6 6 6'], w: 2.2 },
  next: { d: ['M9 6l6 6-6 6'], w: 2.2 },
  back: { d: ['M15 6l-6 6 6 6'], w: 2 },
  down: { d: ['M6 9l6 6 6-6'], w: 2.2 },
  sliders: { d: ['M4 7h10M18 7h2M4 17h4M12 17h8', ring(16, 7, 2), ring(10, 17, 2)] },
  spinner: { d: ['M12 3a9 9 0 1 0 9 9'], w: 2.6 },
  x: { d: ['M6 6l12 12M18 6L6 18'], w: 2.2 },
  retry: { d: ['M20 12a8 8 0 1 1-2.3-5.7', 'M20 4v4h-4'], w: 2.2 },
  rewind: { d: ['M4 12a8 8 0 1 0 2.3-5.7', 'M4 4v4h4'], w: 2 },
  file: { d: ['M14 3H7a1 1 0 0 0-1 1v16a1 1 0 0 0 1 1h10a1 1 0 0 0 1-1V7z', 'M14 3v4h4'] },
  code: { d: ['M9 7l-5 5 5 5M15 7l5 5-5 5'], w: 2 },
  command: { d: ['M5 7l4 5-4 5M12 17h7'], w: 2 },
  tabs: { d: ['M3 9h18v11H3zM3 9l2-4h6l2 4'] },
  task: { d: ['M7 4v16M7 14c0-4 3-6 7-6h4M15 5l3 3-3 3'], w: 2 },
  tool: { d: ['M14.5 6.5a4 4 0 0 0-5 5L4 17l3 3 5.5-5.5a4 4 0 0 0 5-5l-2.5 2.5-2.5-.5-.5-2.5z'] },
  picture: { d: [box(3, 5, 18, 14), ring(9, 10, 1.6), 'M21 16l-5-5-8 8'], w: 1.6 },
  plus: { d: ['M12 5v14M5 12h14'] },
  search: { d: [ring(11, 11, 6.5), 'M16 16l4.5 4.5'] },
  warning: { d: ['M12 4l9 16H3z', 'M12 10v4M12 17v.5'], w: 2 },
  clip: { d: ['M20 11l-8 8a5 5 0 0 1-7-7l8-8a3.5 3.5 0 0 1 5 5l-8 8a2 2 0 0 1-3-3l7-7'] },
  camera: { d: ['M4 8h3l2-3h6l2 3h3v11H4z', ring(12, 13, 3.5)], w: 1.7 },
  ban: { d: [ring(12, 12, 8.5), 'M6 6l12 12'], w: 2 },
  pencil: { d: ['M4 20h4L19 9l-4-4L4 16z'], w: 1.9 },
  signin: { d: ['M14 4h5v16h-5M4 12h10M10 8l4 4-4 4'], w: 2 },
  signout: { d: ['M14 4h5v16h-5M10 8l-4 4 4 4M6 12h10'], w: 2 },
  lock: { d: [box(5, 11, 14, 9), 'M8 11V8a4 4 0 0 1 8 0v3'], w: 1.9 },
  list: { d: ['M9 6h11M9 12h11M9 18h11M4.5 6h.01M4.5 12h.01M4.5 18h.01'], w: 1.9 },
  browser: { d: [box(3, 4, 18, 16), 'M3 9h18'], w: 2 },
  monitor: { d: [box(3, 4, 18, 12), 'M8 20h8M12 16v4'], w: 2 },
  trash: { d: ['M5 7h14M10 7V4h4v3M7 7l1 13h8l1-13'] },
  pause: { d: ['M9 6v12M15 6v12'], w: 2.2 },
  dash: { d: ['M7 12h10'], w: 2.2 },
  more: { dots: true },
  stop: { square: true },
};
function icon(name, size = 16) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  const spec = ICONS[name] ?? {};
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('width', size);
  svg.setAttribute('height', size);
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('class', `icon icon-${name}`);
  const add = (d, attrs = {}) => {
    const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    path.setAttribute('d', d);
    for (const [key, value] of Object.entries(attrs)) path.setAttribute(key, value);
    svg.append(path);
  };
  if (spec.square) {
    add(box(6, 6, 12, 12), { fill: 'currentColor' });
    return svg;
  }
  if (spec.dots) {
    for (const x of [5, 12, 19]) add(ring(x, 12, 1.6), { fill: 'currentColor' });
    return svg;
  }
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', spec.w ?? 1.8);
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  for (const d of spec.d ?? []) add(d);
  return svg;
}
/** Fills the page's icon places (`data-icon`, `data-size`), and gives every folding row of
 * Settings its chevron. */
function fillIcons(root = document) {
  for (const place of root.querySelectorAll('[data-icon]:empty')) place.append(icon(place.dataset.icon, Number(place.dataset.size) || 14));
  for (const summary of root.querySelectorAll('.fold > summary, .subfold > summary')) {
    if (summary.querySelector(':scope > .chev')) continue;
    const chev = el('span', 'chev');
    chev.append(icon('next', summary.parentElement.classList.contains('fold') ? 14 : 12));
    summary.append(chev);
  }
}
/** A button showing only an icon, its words in its tooltip and accessible name. */
function iconButton(name, label, run, cls = 'icon-button', size = 16) {
  const button = el('button', cls);
  button.type = 'button';
  button.title = label;
  button.setAttribute('aria-label', label);
  button.append(icon(name, size));
  if (run) button.onclick = run;
  return button;
}
/** A button copying `text()` to the clipboard, a check for a moment once it did. */
function copyButton(text, label) {
  const button = iconButton('copy', label, async (event) => {
    event.stopPropagation();
    try {
      await navigator.clipboard.writeText(text());
      button.replaceChildren(icon('check', 14));
      button.classList.add('copied');
      setTimeout(() => {
        button.replaceChildren(icon('copy', 14));
        button.classList.remove('copied');
      }, 1400);
    } catch (error) {
      toast(`It was not copied: ${error.message}`);
    }
  }, 'icon-button copy', 14);
  return button;
}

/** When the steps seen running here ran: tool call id -> [start, end] in ms since the epoch
 * (for this browser session; a thread opened later shows what it knows). An older panel
 * kept a duration alone, which is still read. */
const timings = (() => {
  try {
    return new Map(JSON.parse(sessionStorage.getItem('spindle-step-times') || '[]'));
  } catch {
    return new Map();
  }
})();
function keepTiming(id, span) {
  if (!id) return;
  timings.set(id, span);
  while (timings.size > 4000) timings.delete(timings.keys().next().value);
  try {
    sessionStorage.setItem('spindle-step-times', JSON.stringify([...timings]));
  } catch {}
}
/** Tabs the open thread's steps told about (their titles and addresses) and the parts of
 * pages they read, for naming the page (and the button, the field) a browser step acted
 * on. */
let knownTabs = new Map();
let knownParts = new Map();

// ---- The engine ------------------------------------------------------------------

const me = crypto.randomUUID();
const channel = new BroadcastChannel(CHANNEL);
// Requests go to the engine alone, and its answers come back to this page alone.
const requests = new BroadcastChannel(REQUESTS);
const answers = new BroadcastChannel(answersFor(me));
const waiting = new Map();
let next = 0;
/** The engine's last snapshot (catalog, threads, info), null until it answers. */
let engine = null;

/** How long an answer may take: moving whole sessions, and freeing space (a whole
 * store read, written and checked; a moved thread's file checked whole), longer. */
const PATIENCE = { export: 300_000, import: 300_000, earlierOpen: 300_000, catalog: 120_000, compact: 600_000, freeSealed: 300_000, toHost: 300_000, hostConnect: 120_000 };

function ask(op, args = {}, timeout = PATIENCE[op] ?? 60_000) {
  const id = ++next;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      waiting.delete(id);
      reject(Object.assign(new Error("Spindle's engine did not answer"), { kind: 'timeout' }));
      // Past the first contact, silence means the engine stopped working, unless it still
      // answers a ping (freeing space holds it for minutes): then it is only busy.
      if (engine && op !== 'ping') {
        ask('ping', {}, 5_000).then(
          () => toast('Spindle is busy (freeing space, say); try again in a moment.'),
          () => banner("Spindle's engine is not answering.", [['Restart engine', askRestartEngine]]),
        );
      }
    }, timeout);
    waiting.set(id, { resolve, reject, timer });
    requests.postMessage({ to: 'engine', from: me, id, op, args });
  });
}

/** A new engine started (Chrome restarted the old one, or another panel did): what was
 * asked of the old one is never answered. */
function engineRestarted() {
  for (const answer of waiting.values()) {
    clearTimeout(answer.timer);
    answer.reject(Object.assign(new Error("Spindle's engine restarted; try again"), { kind: 'restarted' }));
  }
  waiting.clear();
  engine = null;
  connect().then(() => {
    if (engine && current) openThread(current);
  });
}

channel.onmessage = ({ data }) => {
  if (data?.to === me) {
    const answer = waiting.get(data.id);
    if (!answer) return;
    waiting.delete(data.id);
    clearTimeout(answer.timer);
    if (data.ok) answer.resolve(data.result);
    else answer.reject(Object.assign(new Error(data.error.message), { kind: data.error.kind }));
    return;
  }
  if (data?.to) return;
  if (data?.type === 'event') onEvent(data.thread, data.event);
  else if (data?.type === 'thread') onThread(data.thread);
  else if (data?.type === 'removed') onRemoved(data.id);
  else if (data?.type === 'turn_end') onTurnEnd(data.thread, data.unsent ?? []);
  // A thread on the user's host after a gap in its events (or opened at work): drawn
  // again from what the host holds.
  else if (data?.type === 'resync') { if (data.thread === current && document.body.dataset.view === 'thread') openThread(current); }
  else if (data?.type === 'queued') { if (data.thread === current) renderQueued(data.queued); }
  else if (data?.type === 'approvals') {
    if (engine) engine.approvals = data.approvals;
    renderThreads();
    renderAsking();
  }
  else if (data?.type === 'linux') onLinux(data);
  else if (data?.type === 'host' || data?.type === 'sites' || data?.type === 'relay') {
    if (engine) engine[data.type] = data[data.type];
    if ($('settings-dialog').open) renderCommands();
    if (data.type === 'host') renderBar();
  }
  else if (data?.type === 'vault') {
    // Keys changed in another panel, or a sign-in ended: every panel keeps up.
    if (engine) engine.vault = data.vault;
    keyed = new Set(Object.keys(data.vault?.kinds ?? {}));
    renderBar();
    if ($('settings-dialog').open) {
      renderProviders();
      renderSites();
    }
  }
  else if (data?.type === 'ready') {
    if (engine) engineRestarted();
    else connect();
  }
};
// Answers come on this page's own channel; events on the shared one.
answers.onmessage = (event) => channel.onmessage(event);


let connecting = null;
function connect() {
  connecting ??= (async () => {
    banner(null);
    for (let attempt = 0; attempt < 60 && !engine; attempt++) {
      try {
        apply(await ask('hello', {}, 500));
      } catch (error) {
        if (error.kind !== 'timeout') {
          banner(error.message, [['Restart engine', restartEngine]]);
          return;
        }
        if (attempt === 0) await startEngine().catch((failed) => banner(`Spindle's engine could not start: ${failed.message}`));
      }
    }
    if (!engine) banner("Spindle's engine is not answering.", [['Restart engine', askRestartEngine]]);
  })().finally(() => { connecting = null; });
  return connecting;
}

async function restartEngine() {
  await chrome.offscreen.closeDocument().catch(() => {});
  engineRestarted();
}

/** Restarts the engine once the user agrees, when that cuts running threads off (as
 * Settings asks). */
async function askRestartEngine() {
  const busy = [...threads.values()].filter((thread) => thread.state === 'running' && !thread.home).length;
  if (busy && !(await confirmed('Restart the engine?', `${busy} browser thread(s) are running. Restarting cuts them off; each can be retried after. Threads on your host keep running there.`, 'Restart'))) return;
  await restartEngine();
}

// ---- State -----------------------------------------------------------------------

const threads = new Map();
/** The open thread's id (null: the thread list). */
let current = null;
/** Providers with a saved key (never the keys themselves). */
let keyed = new Set();
const STATES = { idle: 'ready', running: 'running', stopped: 'stopped', failed: 'failed', interrupted: 'interrupted', moved: 'moved out' };

function loadChoice() {
  try {
    return JSON.parse(localStorage.getItem('spindle-new') || '{}');
  } catch {
    return {};
  }
}
let choice = loadChoice();

const providers = () => engine?.catalog?.providers ?? [];
const profileOf = (id) => providers().find((p) => p.id === id) ?? null;
const nameOf = (id) => profileOf(id)?.display_name ?? id;
const needsKey = (profile) => profile.requires_api_key !== false;
/** What a provider's saved credential is, in a few words. */
function credentialState(profile) {
  if (engine?.vault?.kinds?.[profile.id] !== 'signin') return 'key saved';
  if (engine.vault.ended?.[profile.id]) return 'sign-in ended: sign in again in Settings';
  const account = engine.vault.accounts?.[profile.id];
  return account ? `signed in as ${account}` : 'signed in';
}
/** Sign-ins in progress, by provider: one at a time each, whatever redraws the row. */
const signingIn = new Set();

/** The provider and model a new thread starts with: the one chosen last, else the
 * catalog's active provider or the first with a saved key, else the active one. */
function chosen() {
  const list = providers();
  const active = profileOf(engine?.catalog?.active_provider);
  const profile = profileOf(choice.provider)
    ?? [active, ...list].find((p) => p && keyed.has(p.id))
    ?? active
    ?? list[0];
  if (!profile) return { provider: null, model: null, effort: 'default' };
  const same = choice.provider === profile.id && choice.model;
  return { provider: profile.id, model: same ? choice.model : profile.default_model, effort: same && choice.effort ? choice.effort : 'default' };
}

/** The engine names its next steps as the spindle command's; here they are buttons. */
function explain(text) {
  return String(text ?? '')
    .replace(/rewind to an earlier point \(`spindle targets`\) or start a new thread/g,
      'go back to before one of your messages (the back arrow beside it), or start a new thread with its files (the thread menu)')
    .replace(/rewind to a target first \(`spindle targets`\)/g, 'go back to before one of your messages first (the back arrow beside it)')
    .replace(/\b[Rr]un `spindle (retry|continue)`/g, (_, step) => `press ${step === 'retry' ? 'Retry' : 'Continue'}`)
    .replace(/`spindle retry`/g, 'Retry')
    .replace(/`spindle continue`/g, 'Continue');
}

function apply(snapshot) {
  // A catalog of its own may say otherwise of a model's efforts.
  if (JSON.stringify(snapshot.catalog) !== JSON.stringify(engine?.catalog)) {
    effortsAsked.clear();
    effortsKnown.clear();
  }
  const vaultChanged = JSON.stringify(snapshot.vault) !== JSON.stringify(engine?.vault);
  engine = snapshot;
  if (snapshot.vault) keyed = new Set(Object.keys(snapshot.vault.kinds));
  threads.clear();
  for (const thread of snapshot.threads) threads.set(thread.id, thread);
  banner(snapshot.catalogNote ?? null);
  renderThreads();
  renderBar();
  // Keys and their lock as this engine has them (a restarted one is locked again); only
  // when they changed, so what is being typed there stays.
  if (vaultChanged && $('settings-dialog')?.open) renderProviders();
}

// ---- Thread list -----------------------------------------------------------------

/** States in which a thread's last turn stopped before it finished: it waits on the user
 * (Continue, Retry or Take back). */
const HALTED = new Set(['stopped', 'failed', 'interrupted']);

/** What each running thread was last seen doing (from its events, while this panel is
 * open): {text, step, of}. */
const activity = new Map();
/** Threads deleted a moment ago, out of the list until Undo's time is up. */
const hiddenThreads = new Set();
let pendingDelete = null;

/** When the user last saw each thread (its `updated` then): one that changed since, a
 * turn's end say, has a reply they have not read. Kept in this browser. */
const seen = (() => {
  try {
    return new Map(Object.entries(JSON.parse(localStorage.getItem('spindle-seen') || '{}')));
  } catch {
    return new Map();
  }
})();
function markSeen(thread) {
  if (!thread || seen.get(thread.id) === thread.updated) return;
  seen.set(thread.id, thread.updated);
  try {
    localStorage.setItem('spindle-seen', JSON.stringify(Object.fromEntries([...seen].filter(([id]) => threads.has(id)))));
  } catch {}
}
/** Whether a thread finished a turn the user has not seen (a thread never seen before
 * counts as seen: the list starts from now). */
function unread(thread) {
  if (thread.id === current || thread.state !== 'idle') return false;
  if (!seen.has(thread.id)) {
    markSeen(thread);
    return false;
  }
  return seen.get(thread.id) < thread.updated;
}

/** How long ago, as the list shows it: now, 13m, 3h, 2d, then the date. */
function since(ms, now = Date.now()) {
  const minutes = Math.max(0, Math.floor((now - ms) / 60_000));
  if (minutes < 1) return 'now';
  if (minutes < 60) return `${minutes}m`;
  if (minutes < 24 * 60) return `${Math.floor(minutes / 60)}h`;
  if (minutes < 7 * 24 * 60) return `${Math.floor(minutes / 1440)}d`;
  return new Date(ms).toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
}

/** What a thread's row says it is doing (not which model, but for a thread at rest). */
function threadDoing(thread) {
  // Moving to or from the user's host, or there and out of reach: said first.
  if (thread.moving && thread.moving.step !== 'release') return thread.moving.why ? `${thread.moving.text} · waiting: ${thread.moving.why}` : thread.moving.text;
  if (thread.home && thread.away) return `On your host · out of reach: ${thread.away}`;
  const doing = threadState(thread);
  return thread.home ? `On your host · ${doing}` : doing;
}

/** What a thread's row says of its state (wherever it runs). */
function threadState(thread) {
  if (thread.state === 'running') {
    const seenNow = activity.get(thread.id);
    return [seenNow?.text ?? 'Working', seenNow?.of > 1 && `step ${seenNow.step} of ${seenNow.of}`].filter(Boolean).join(' · ');
  }
  if (thread.state === 'moved') return 'Moved to a file';
  if (thread.state === 'failed') return `Failed${thread.note ? `: ${explain(thread.note).split('\n')[0].replace(/^failed:?\s*/i, '')}` : ''}`;
  if (thread.state === 'stopped') return 'Stopped part way: continue, retry or take back';
  if (thread.state === 'interrupted') return 'Cut off part way: continue, retry or take back';
  if (unread(thread)) return `Finished · new reply · ${thread.model}`;
  return `Idle · ${thread.model}`;
}

function renderThreads() {
  const list = $('threads');
  list.replaceChildren();
  const all = [...threads.values()].filter((thread) => !hiddenThreads.has(thread.id)).sort((a, b) => b.updated - a.updated);
  $('intro').hidden = all.length > 0;
  $('search-box').hidden = all.length === 0;
  const query = $('thread-search').value.trim().toLowerCase();
  const shown = query ? all.filter((thread) => `${thread.title} ${thread.model} ${nameOf(thread.provider)}`.toLowerCase().includes(query)) : all;
  const today = new Date().setHours(0, 0, 0, 0);
  const groups = [
    ['Running', shown.filter((thread) => thread.state === 'running')],
    ['Needs you', shown.filter((thread) => HALTED.has(thread.state))],
    ['Today', shown.filter((thread) => !HALTED.has(thread.state) && thread.state !== 'running' && thread.updated >= today)],
    ['Earlier', shown.filter((thread) => !HALTED.has(thread.state) && thread.state !== 'running' && thread.updated < today)],
  ];
  for (const [name, members] of groups) {
    if (!members.length) continue;
    list.append(el('h2', 'thread-group', name));
    members.forEach((thread, index) => list.append(threadRow(thread, index === members.length - 1)));
  }
  if (query && !shown.length) list.append(el('p', 'threads-empty', 'No thread matches.'));
}

/** A thread's row: its state as a shape (a spinner, a warning, an unread dot, nothing at
 * rest), its name and what it is doing, how long ago; ⋯ for more. */
/** The page actions a thread waits on the user's answer for (Settings, Pages, Before
 * acting). */
const askingOf = (id) => (engine?.approvals ?? []).filter((approval) => approval.thread === id);

/** What the asking box shows (the thread and its waiting actions): it is drawn again
 * only when that changes, so a click is never lost to a button drawn anew under it. */
let askingShown = '';

/** The open thread's waiting actions, above the message box: Allow or Deny each. */
function renderAsking() {
  const box = $('asking');
  const waiting = current ? askingOf(current) : [];
  const shown = JSON.stringify([current, waiting.map((approval) => approval.id)]);
  if (shown === askingShown) return;
  askingShown = shown;
  box.replaceChildren();
  box.hidden = !waiting.length;
  for (const approval of waiting) {
    const item = el('div', 'ask-item');
    const allow = el('button', 'primary', 'Allow');
    const deny = el('button', '', 'Deny');
    const answer = (yes) => async () => {
      // One answer: both buttons rest until the engine has it.
      allow.disabled = deny.disabled = true;
      try {
        const { approvals } = await ask('approve', { approval: approval.id, allow: yes });
        if (engine) engine.approvals = approvals;
        renderThreads();
        renderAsking();
      } catch (error) {
        allow.disabled = deny.disabled = false;
        toast(error.message);
      }
    };
    allow.type = 'button';
    allow.onclick = answer(true);
    deny.type = 'button';
    deny.onclick = answer(false);
    const said = el('span', 'ask-text', `Spindle asks before it acts: ${approval.text}`);
    said.id = `ask-${approval.id}`;
    allow.setAttribute('aria-describedby', said.id);
    deny.setAttribute('aria-describedby', said.id);
    item.append(icon('warning', 14), said, deny, allow);
    box.append(item);
  }
}

function threadRow(thread, last) {
  const asking = askingOf(thread.id);
  const needs = HALTED.has(thread.state) || asking.length > 0;
  const fresh = unread(thread);
  const item = el('div', `thread-item${last ? ' last' : ''}${thread.state === 'running' ? ' running' : ''}${needs ? ' needs' : ''}${fresh ? ' unread' : ''}${thread.state === 'moved' ? ' moved' : ''}${thread.home ? ' hosted' : ''}`);
  const row = el('button', `thread-row state-${thread.state}`);
  row.type = 'button';
  row.dataset.id = thread.id;
  const mark = el('span', 'thread-mark');
  mark.setAttribute('aria-hidden', 'true');
  if (asking.length) mark.append(icon('warning', 16));
  else if (thread.state === 'running') mark.append(icon('spinner', 14));
  else if (needs) mark.append(icon('warning', 16));
  else if (fresh) mark.append(el('span', 'dot'));
  // On the user's host: its mark when nothing else is said.
  else if (thread.home) mark.append(icon('monitor', 14));
  const text = el('span', 'thread-text');
  const top = el('span', 'thread-top');
  top.append(el('span', 'thread-name', thread.title), el('span', 'thread-time', since(thread.updated)));
  const doing = asking.length ? `Asks you: ${asking[0].text}` : threadDoing(thread);
  const meta = el('span', 'thread-meta', doing);
  text.append(top, meta);
  row.append(mark, text);
  row.title = [thread.title, `${nameOf(thread.provider)} · ${thread.model}`, thread.home && `On your Spindle host, ${thread.home.url}`, thread.note && explain(thread.note), new Date(thread.updated).toLocaleString()].filter(Boolean).join('\n');
  row.onclick = () => openThread(thread.id);
  const more = el('button', 'thread-more');
  more.type = 'button';
  more.setAttribute('aria-label', `More actions for ${thread.title}`);
  more.title = 'More actions';
  more.setAttribute('aria-haspopup', 'menu');
  more.setAttribute('aria-expanded', 'false');
  more.append(icon('more', 18));
  more.onclick = (event) => {
    event.stopPropagation();
    rowMenu(more, [['Delete', 'trash', () => deleteThread(thread.id), 'danger']]);
  };
  item.append(row, more);
  return item;
}

/** A small menu under `button`: [label, icon, run, class] each. */
function rowMenu(button, entries) {
  const menu = $('row-menu');
  // In the open dialog its button is in (Settings, say): a modal dialog makes all
  // outside it inert, so a menu left outside showed on top but took no click.
  const home = button.closest('dialog[open]') ?? document.body;
  if (menu.parentElement !== home) {
    if (menu.matches(':popover-open')) menu.hidePopover();
    home.append(menu);
  }
  menu.setAttribute('aria-label', home === document.body ? 'Thread actions' : 'Actions');
  menu.replaceChildren(...entries.map(([label, name, run, cls]) => {
    const entry = el('button', cls ?? '');
    entry.type = 'button';
    entry.setAttribute('role', 'menuitem');
    entry.append(icon(name, 14), document.createTextNode(label));
    entry.onclick = () => {
      menu.hidePopover();
      run();
    };
    return entry;
  }));
  const place = button.getBoundingClientRect();
  menu.style.top = `${Math.min(place.bottom + 2, innerHeight - 48 * entries.length - 8)}px`;
  menu.style.left = 'auto';
  menu.style.right = `${Math.max(8, innerWidth - place.right)}px`;
  menu.onbeforetoggle = (event) => {
    button.setAttribute('aria-expanded', String(event.newState === 'open'));
    // Closed from within (Escape, say): the focus goes back to its button.
    if (event.newState === 'closed' && menu.contains(document.activeElement) && button.isConnected) button.focus();
  };
  menu.showPopover();
  menu.querySelector('button')?.focus();
}

/** Deletes a thread, with a moment to undo it: it leaves the list at once, and goes from
 * this browser when the moment passes (or the panel closes). A running one must be
 * stopped first. */
function deleteThread(id) {
  const thread = threads.get(id);
  if (!thread) return;
  if (thread.state === 'running') {
    toast('Stop this thread before deleting it.');
    return;
  }
  finishDelete();
  if (current === id) showHome();
  hiddenThreads.add(id);
  renderThreads();
  pendingDelete = { id, timer: setTimeout(finishDelete, UNDO_MS) };
  toast(`Deleted “${thread.title}”`, { action: ['Undo', () => undoDelete(id)], ms: UNDO_MS });
}
const UNDO_MS = 8000;

function finishDelete() {
  if (!pendingDelete) return;
  const { id, timer } = pendingDelete;
  pendingDelete = null;
  clearTimeout(timer);
  ask('remove', { id }).then(() => forgetCameBack(id), (error) => {
    hiddenThreads.delete(id);
    renderThreads();
    toast(`It was not deleted: ${error.message}`);
  });
}

function undoDelete(id) {
  if (pendingDelete?.id !== id) return;
  clearTimeout(pendingDelete.timer);
  pendingDelete = null;
  hiddenThreads.delete(id);
  renderThreads();
  hideToast();
}
// Closing the panel deletes what waits for its undo.
addEventListener('pagehide', finishDelete);
$('thread-search').addEventListener('input', renderThreads);

function onThread(thread) {
  const before = threads.get(thread.id);
  threads.set(thread.id, thread);
  // A move to or from the user's host ended: said, whichever thread is open.
  if (before && !before.home && thread.home) toast(`“${thread.title}” runs on your host now.`);
  else if (before?.home && !thread.home) toast(`“${thread.title}” is back in this browser.`);
  else if (before?.moving && !thread.moving && before.moving.step !== 'release' && thread.note) toast(thread.note);
  if (thread.state !== 'running') {
    activity.delete(thread.id);
    turnStart.delete(thread.id);
  } else if (before && before.state !== 'running') {
    turnStart.set(thread.id, Date.now());
  }
  if (thread.id === current) markSeen(thread);
  renderThreads();
  if (thread.id === current) {
    $('title').textContent = thread.title;
    $('subtitle').textContent = `${nameOf(thread.provider)} · ${thread.model}`;
    if (['state', 'note', 'provider', 'model', 'runner', 'network', 'effort', 'away'].some((key) => before?.[key] !== thread[key])
      || JSON.stringify(before?.moving) !== JSON.stringify(thread.moving)) renderBar();
    if (before?.state !== thread.state) loadSpend(thread.id);
    // Moved out while open: show what is left here, and the file.
    if (thread.state === 'moved' && before && before.state !== 'moved') openThread(thread.id);
    // Moved to the user's host or back, or a move began or ended, or the host is in reach
    // again: drawn from where it lives now.
    const holding = (one) => Boolean(one?.moving && one.moving.step !== 'release');
    if (before && (Boolean(before.home) !== Boolean(thread.home) || holding(before) !== holding(thread) || (before.away && !thread.away))) openThread(thread.id);
  }
}

function onRemoved(id) {
  threads.delete(id);
  hiddenThreads.delete(id);
  activity.delete(id);
  renderThreads();
  if (id === current) showHome();
}

/** What the button at the left of the header says it does (back, in a thread). */
function homeLabel(name, tip) {
  $('home').setAttribute('aria-label', name);
  $('home').title = tip;
}

let historyPaint = 0;

function showHome() {
  historyPaint += 1;
  dropKept();
  pausePlayers();
  // What the user saw of the thread they leave is read.
  if (current) markSeen(threads.get(current));
  current = null;
  restoreCameBack(null);
  live = null;
  renderQueued(null);
  renderTasks(null, []);
  renderSpend(null);
  document.body.dataset.view = 'home';
  $('welcome').hidden = false;
  $('thread').hidden = true;
  $('workspace').hidden = true;
  $('dock').hidden = false;
  homeLabel('Spindle', 'Spindle');
  $('title-button').hidden = true;
  $('title').textContent = 'Spindle';
  $('subtitle').textContent = '';
  $('prompt').placeholder = 'Start a new thread…';
  closeAdd();
  renderThreads();
  renderBar();
}

// ---- A thread --------------------------------------------------------------------

/** The step being written: its article and text so far. */
let live = null;
/** Tool cards waiting for their result, oldest first. */
let pendingCards = [];
let status = null;
/** The object URL of a moved thread's kept file, while its view is open. */
let keptUrl = null;

function dropKept() {
  if (keptUrl) URL.revokeObjectURL(keptUrl);
  keptUrl = null;
}

/** Dollars as the header shows them: $0.38, <$0.01. */
const money = (usd) => (usd > 0 && usd < 0.005 ? '<$0.01' : `$${Number(usd).toFixed(2)}`);

/** The open thread's spend (`spend` from the engine), read now or, `soon`, at most every
 * few seconds while its turn runs. */
let spendTimer = 0;
function loadSpend(id, soon = false) {
  if (!id || (soon && spendTimer)) return;
  clearTimeout(spendTimer);
  spendTimer = setTimeout(() => {
    spendTimer = 0;
    ask('spend', { id }).then((spend) => current === id && renderSpend(spend), () => {});
  }, soon ? 4000 : 0);
}

/** The header's spend button and the bar under it (what it spent against its limit). */
function renderSpend(spend) {
  const button = $('spend');
  const bar = $('spend-bar');
  const limit = spend?.max_usd ?? null;
  button.hidden = !spend || !(spend.calls || limit != null);
  const over = Boolean(spend && limit != null && spend.usd >= limit);
  button.classList.toggle('over', over);
  bar.classList.toggle('over', over);
  $('spend-fill').style.width = spend && limit != null ? `${limit > 0 ? Math.min(100, (spend.usd / limit) * 100) : 100}%` : '0';
  if (!spend) return;
  button.textContent = money(spend.usd);
  const said = limit != null ? `Spent ${money(spend.usd)} of a ${money(limit)} limit` : `Spent ${money(spend.usd)}, no limit set`;
  const others = [spend.subscription && `${spend.subscription} by subscription`, spend.unpriced && `${spend.unpriced} with no known price`, spend.unknown && `${spend.unknown} whose outcome is unknown`].filter(Boolean);
  button.setAttribute('aria-label', said);
  button.title = `${said}: ${spend.calls} model ${spend.calls === 1 ? 'call' : 'calls'}${others.length ? ` (${others.join(', ')})` : ''}. Change the limit`;
}
$('spend').onclick = () => openModel('budget');

async function openThread(id) {
  const paint = ++historyPaint;
  dropKept();
  current = id;
  restoreCameBack(id);
  live = null;
  pendingCards = [];
  status = null;
  shown = { total: 0, prompts: [], turnFrom: null, compacted: false };
  carry(null);
  renderQueued(null);
  renderTasks(null, []);
  const thread = threads.get(id);
  markSeen(thread);
  document.body.dataset.view = 'thread';
  $('welcome').hidden = true;
  $('thread').hidden = false;
  $('workspace').hidden = true;
  $('dock').hidden = false;
  homeLabel('All threads', 'All threads');
  knownTabs = new Map();
  knownParts = new Map();
  pendingSteers = [];
  $('title-button').hidden = false;
  $('title').textContent = thread?.title ?? 'Thread';
  $('subtitle').textContent = thread ? `${nameOf(thread.provider)} · ${thread.model}` : '';
  $('prompt').placeholder = 'Reply…';
  $('history').replaceChildren(el('p', 'note opening', 'Opening…'));
  closeAdd();
  renderSpend(null);
  loadSpend(id);
  renderBar();
  try {
    const data = await ask('open', { id });
    if (current !== id || historyPaint !== paint) return;
    status = data.status;
    renderQueued(data.queued);
    renderHistory(data);
    // Its tasks, for the line above the box and the cards and reports in its history.
    if ((data.thread ?? thread)?.tasks || data.status?.background || $('history').querySelector('.task-block, .task-report')) loadTasks(id, true);
  } catch (error) {
    if (current === id) $('history').replaceChildren(el('p', 'notice state warning', error.message));
  }
  renderBar();
}

function resultText(content) {
  if (typeof content === 'string') return content;
  if (Array.isArray(content)) return content.map((part) => part.text ?? '').join('');
  return JSON.stringify(content ?? '');
}

/** Whether the conversation follows its end as it grows. Only the user stops it, by
 * scrolling up away from the end; scrolling back near it, Latest, a message they send, or
 * a forced scroll (a thread opened) starts it again. What is added never stops it:
 * measured after a tall preview had come in at once, the end was out of reach, so the
 * conversation stayed behind (and a preview left on screen looked read, `readingAt`). */
let following = true;
/** Where the conversation was scrolled to when last seen (a smaller one is the user's
 * scrolling up). */
let scrolledTo = 0;

/** Paths the last turn shown so far wrote with its file tools (each has its card), and
 * those cards (the last turn's open their previews; earlier ones wait to be asked). */
let turnPaths = new Set();
let turnCards = [];

/** The files of players showing (object address -> its player): each let go of once its
 * player is gone from the panel (a card taken away, a thread switched, a preview
 * replaced, the workspace page left). */
const players = new Map();
function sweepPlayers() {
  for (const [url, player] of players) {
    if (player.isConnected) continue;
    player.pause();
    URL.revokeObjectURL(url);
    players.delete(url);
  }
}

/** The conversation out of sight (the start page, the workspace page): what plays in it
 * stops. */
function pausePlayers() {
  for (const player of players.values()) if (!player.closest('#ws-preview')) player.pause();
}

/** What the open thread's view holds of its history: how many messages, the indexes of
 * the user's messages among them, where the running turn started (null: not known),
 * and whether a compaction changed the history since. */
let shown = { total: 0, prompts: [], turnFrom: null, compacted: false };
/** A long history opens on its last turns; the rest shows when asked. */
const TAIL_TURNS = 30;
const COMPACTED = '[Earlier conversation compacted';

/** Draws a thread's history: all of it, or (`from`) only its messages from that index on
 * in place of what was shown of them. False when the view holds no place for `from`
 * (then the whole history is to be drawn). */
function renderHistory({ thread, history, running, bundle, moving, away }, { from = 0 } = {}) {
  const box = $('history');
  pendingCards = [];
  // Moving to or from the user's host (no copy of it is read meanwhile), or on a host
  // that does not answer now.
  if (!history && (moving || away)) {
    box.replaceChildren();
    const notice = el('div', 'notice state');
    notice.append(el('p', '', moving
      ? `${moving.text}.${moving.why ? ` It waits: ${explain(moving.why)}.` : ''} Nothing is lost if ${BROWSER} closes meanwhile: Spindle goes on with it when it starts again.`
      : `This thread is on your Spindle host, which does not answer now (${explain(away)}). Nothing is lost: it is there, and shows here again once your host answers.`));
    box.append(notice);
    return true;
  }
  if (!history) {
    box.replaceChildren();
    const notice = el('div', 'notice state');
    notice.append(el('p', '', 'This thread moved into a .spindle file; this copy is sealed. Continue it in native Spindle with `spindle --dir DIR import-session FILE`, or import the file into Spindle in a browser.'));
    box.append(notice);
    if (bundle) {
      // A real link, so the click itself saves it (Chrome holds back a second
      // download a page starts on its own).
      // Read here, from the extension's own storage: not asked of the engine (its answer
      // would go through the engine's memory first).
      read('bundles', thread.id).then((kept) => {
        if (current !== thread.id || !kept?.bytes) return;
        dropKept();
        keptUrl = URL.createObjectURL(new Blob([kept.bytes], { type: 'application/octet-stream' }));
        const again = el('a', 'button-like wide', 'Save the file again');
        again.href = keptUrl;
        again.download = kept.name;
        notice.append(again);
      }, (error) => toast(error.message));
    }
    return true;
  }
  const messages = history.messages;
  let start = 0;
  if (from > 0) {
    // What was shown from there on (and what the turn drew live) goes.
    const at = from === shown.total ? box.querySelector('.history-end') : box.querySelector(`[data-at="${from}"]`);
    if (!at || (history.total ?? 0) < from) return false;
    while (at.nextSibling) at.nextSibling.remove();
    at.remove();
    shown.prompts = shown.prompts.filter((index) => index < from);
  } else {
    box.replaceChildren();
    shown.prompts = [];
    const prompts = messages.flatMap((message, index) => (isPrompt(message) ? [index] : []));
    if (prompts.length > TAIL_TURNS) {
      start = prompts[prompts.length - TAIL_TURNS];
      box.append(earlierButton(thread, start));
    }
  }
  // One workspace card, for the workspace as it is now: an earlier one goes.
  for (const card of box.querySelectorAll('.workspace-card')) card.remove();
  // Players of cards drawn over stop and let go of their files.
  sweepPlayers();
  turnPaths = new Set();
  turnCards = [];
  selfShown = null;
  showing.clear();
  const pictures = renderMessages(box, thread, messages.slice(start), from + start, { running, main: true });
  const end = el('i', 'history-end');
  end.hidden = true;
  box.append(end);
  shown.total = history.total ?? from + messages.length;
  shown.turnFrom = null;
  shown.compacted = false;
  // The last turn's pictures show (a few); its pages and Markdown files stay folded, to
  // open on demand (they showed while the turn worked on them).
  turnCards.filter((card) => card.preview && card.isConnected && kindOf(card.dataset.path) === 'picture').slice(-3).forEach((card) => card.preview());
  pictures.slice(-4).forEach((card) => card.preview());
  // Once no turn runs: the workspace, just above the turn's final answer (at the end when
  // the turn ended without one). It marks what the turn made or changed.
  if (!running) {
    const card = workspaceCard(thread.id);
    const last = [...box.children].reverse().find((node) => node.matches('article.message-wrap'));
    if (last?.matches('.assistant:not(.steer)')) last.before(card);
    else box.insertBefore(card, end);
  }
  // What the running step has written so far stays on screen.
  if (live) box.append(live.node);
  // Its tasks' cards and reports, as the thread's tasks were last read (read again soon).
  if (box.querySelector('.task-block, .task-report')) renderTasks(thread.id, taskList);
  scrollDown(from === 0);
  return true;
}

/** A user's message: text they wrote (not a tool's result, a summary or steering). */
function isPrompt(message) {
  return message.role === 'user' && (message.content ?? []).some((block) => block.type === 'text' && typeof block.steer !== 'string' && !block.report && block.text.trim() && !block.text.startsWith(COMPACTED));
}

/** Draws `messages` (the first being message `offset` of the history at `revision`, or
 * now) into `box`; returns the pictures attached to them, as cards to open. `main`: the
 * open thread's own history (its user messages counted, its last turn's cards kept). */
function renderMessages(box, thread, messages, offset, { running = false, revision = null, main = false } = {}) {
  // Only the open history's last turn counts its files' cards.
  const ownPaths = turnPaths;
  const results = new Map();
  for (const message of messages) {
    for (const block of message.content ?? []) if (block.type === 'tool_result') results.set(block.tool_use_id, block);
  }
  const pictures = [];
  messages.forEach((message, index) => {
    const at = offset + index;
    for (const block of message.content ?? []) {
      if (block.type === 'text' && message.role === 'user') {
        if (block.report) box.append(reportNode(block));
        else if (typeof block.steer === 'string') box.append(steerNode(block.steer, true));
        else if (block.text.startsWith(COMPACTED)) box.append(compactionNode(thread, block.text, revision));
        else if (block.text.trim()) {
          const node = messageNode('user', block.text);
          node.dataset.at = at;
          node.querySelector('.role-label').append(rewindButton(thread, at, revision));
          box.append(node);
          if (main) {
            shown.prompts.push(at);
            turnPaths = new Set();
            turnCards = [];
            selfShown = null;
          }
          pictures.push(...picturesWith(thread.id, block.text, box));
        }
      } else if (block.type === 'text' && block.text.trim()) {
        box.append(messageNode('assistant', block.text));
      } else if (block.type === 'tool_use') {
        const result = results.get(block.id);
        const card = toolCard(block.name, block.input, block.id);
        const steps = stepsIn(box);
        steps.list.append(card);
        if (result) settle(card, resultText(result.content), Boolean(result.is_error));
        else if (running && main) pendingCards.push(card);
        else settle(card, '(no result was recorded)', true);
        if (block.name === 'task') steps.append(taskBlock(card, result ? resultText(result.content) : null));
        const made = result && !result.is_error && wroteFile(card, false);
        if (made) {
          placeFile(card, made);
          if (main) turnCards.push(made);
        }
      }
    }
  });
  // Every run of steps shut, but the one a running turn is in.
  const groups = [...box.children].filter((child) => child.classList.contains('steps'));
  for (const steps of groups) {
    updateSteps(steps);
    if (!steps.classList.contains('single')) setOpen(steps.group, false);
  }
  const lastGroup = groups.at(-1);
  if (running && main && lastGroup && !lastGroup.nextElementSibling?.matches('.message-wrap.user')) setOpen(lastGroup.group, true);
  if (!main) turnPaths = ownPaths;
  return pictures;
}

/** The run of steps (tool calls) at the end of `box` a new step joins, or a new one; a
 * new one opened (`open`, a running turn's) shuts the ones before it the user did not
 * open themselves. */
function stepsIn(box, open = false) {
  const last = box.lastElementChild;
  if (last?.classList.contains('steps')) return last;
  if (open) {
    for (const other of [...box.children].filter((child) => child.classList.contains('steps'))) {
      if (other.group.open && !other.group.dataset.user && !other.classList.contains('single')) setOpen(other.group, false);
    }
  }
  const steps = el('section', 'steps');
  const group = el('details', 'tool-group');
  const head = el('summary', 'group-head');
  const chevron = el('span', 'group-chevron');
  chevron.append(icon('next', 12));
  head.append(el('span', 'group-status'), el('span', 'group-label'), el('span', 'group-counts'), el('span', 'group-time'), chevron);
  const list = el('div', 'group-list');
  group.append(head, list);
  // Opened or shut otherwise than setOpen() left it: the user's doing, kept from then on.
  group.wanted = false;
  group.addEventListener('toggle', () => {
    if (group.open !== group.wanted) {
      group.dataset.user = '1';
      group.wanted = group.open;
    }
  });
  steps.append(group);
  steps.group = group;
  steps.list = list;
  box.append(steps);
  if (open) setOpen(group, true);
  return steps;
}

/** Opens or shuts a step group without counting it as the user's doing. Shut, the run
 * is done with: the previews its steps opened on their own fold with it. */
function setOpen(group, open) {
  group.wanted = open;
  group.open = open;
  if (!open) for (const card of group.parentElement?.querySelectorAll('.file-card') ?? []) card.unpreview?.();
}

/** A run of steps says what it holds: how many (or which runs now), how many were retried
 * (a failed one done again successfully later) and failed, as icons with numbers, and,
 * done, how long it took on the clock. Repeats of a step merge into its row, and a long
 * run shows its last rows until asked for the earlier ones (`foldLong`). */
function updateSteps(steps) {
  const cards = [...steps.list.children].filter((child) => child.classList.contains('tool'));
  if (!cards.length) return;
  const single = cards.length === 1;
  steps.classList.toggle('single', single);
  if (single) setOpen(steps.group, true);
  const list = cards.map((card) => ({ name: card.dataset.tool, input: card.input, failed: card.classList.contains('failed'), running: card.classList.contains('running') }));
  const again = retried(list);
  cards.forEach((card, index) => {
    if (card.classList.contains('retried') !== again.has(index)) {
      card.classList.toggle('retried', again.has(index));
      stateText(card);
      relabel(card);
    }
  });
  const running = cards.filter((card) => card.classList.contains('running'));
  const runningCard = running[0];
  const failed = list.filter((step, index) => step.failed && !again.has(index)).length;
  // Steps that ran at once (a batch of tasks) overlap: the group took the time any of
  // them was running, not their durations added up.
  const total = elapsed(cards.map((card) => ({ span: card.span, ms: card.ms })));
  const count = `${cards.length} step${cards.length === 1 ? '' : 's'}`;
  steps.querySelector('.group-label').textContent = running.length > 1
    ? `${running.length} at once · ${count}`
    : runningCard ? `Step ${cards.indexOf(runningCard) + 1} of ${cards.length}` : `Ran ${count}`;
  const counts = steps.querySelector('.group-counts');
  counts.replaceChildren();
  const tally = (cls, name, n, words) => {
    if (!n) return;
    const tag = el('span', `count ${cls}`);
    tag.setAttribute('role', 'img');
    tag.setAttribute('aria-label', words);
    tag.title = words;
    tag.append(icon(name, cls === 'failed' ? 11 : 12), document.createTextNode(String(n)));
    counts.append(tag);
  };
  tally('retried', 'retry', again.size, `${again.size} retried`);
  tally('failed', 'x', failed, `${failed} failed`);
  const mark = runningCard ? 'spinner' : failed ? 'x' : 'check';
  const status = steps.querySelector('.group-status');
  if (status.dataset.shows !== mark) {
    status.dataset.shows = mark;
    status.replaceChildren(icon(mark, 14));
  }
  steps.started = Math.min(...cards.map((card) => card.span?.[0] ?? card.startedAt ?? Infinity));
  steps.querySelector('.group-time').textContent = runningCard
    ? (Number.isFinite(steps.started) ? clock(Date.now() - steps.started) : '')
    : total ? duration(total) : '';
  steps.group.querySelector('.group-head').title = runningCard ? 'Steps of the running turn' : `${count}${failed ? `, ${failed} failed` : ''}${again.size ? `, ${again.size} retried` : ''}: show or hide them`;
  steps.classList.toggle('running', Boolean(runningCard));
  steps.classList.toggle('has-failed', failed > 0);
  mergeRepeats(steps);
  foldLong(steps);
}

/** Steps that did the same thing one after another (the same tool, words and page, each
 * done) share one row, "Read HTML ×3", their times added; opening it shows each. */
function mergeRepeats(steps) {
  let head = null;
  let key = null;
  for (const child of steps.list.children) {
    if (!child.classList.contains('tool')) {
      head = null;
      continue;
    }
    // The same tool with the same input, on the same page, done (not running or failed).
    const mine = child.classList.contains('running') || child.classList.contains('failed') || ['task', 'write_file', 'edit_file'].includes(child.dataset.tool)
      ? null : `${child.dataset.tool}\n${JSON.stringify(child.input ?? {})}\n${child.pageUrl ?? ''}`;
    if (head && mine && mine === key) {
      child.classList.add('merged');
      child.classList.toggle('shown', head.open);
      head.followers.push(child);
    } else {
      child.classList.remove('merged', 'shown');
      child.followers = [];
      head = child;
      key = mine;
    }
  }
  for (const card of steps.list.querySelectorAll(':scope > .tool:not(.merged)')) {
    const many = card.followers.length;
    const tag = card.querySelector('.tool-count');
    tag.hidden = !many;
    tag.textContent = `×${many + 1}`;
    paintTime(card);
  }
}

/** Rows a long run shows until the rest is asked for. */
const ROWS_SHOWN = 12;

/** A long run of steps shows its last rows, the newest, and above them a row saying how
 * many earlier ones there are. Always the earlier ones: whether a step is running at the
 * moment (none is while the model thinks between steps) never turns the fold around, so
 * the newest rows stay in sight through the run and after it. */
function foldLong(steps) {
  for (const old of steps.list.querySelectorAll(':scope > .group-more')) old.remove();
  const rows = [];
  for (const child of steps.list.children) {
    if (child.classList.contains('tool') && !child.classList.contains('merged')) rows.push([child]);
    else if (rows.length) rows.at(-1).push(child);
    else rows.push([child]);
  }
  const folded = steps.dataset.all !== '1' && rows.length > ROWS_SHOWN + 3;
  const hidden = new Set(folded ? rows.slice(0, rows.length - ROWS_SHOWN) : []);
  for (const row of rows) for (const child of row) child.classList.toggle('beyond', hidden.has(row));
  if (!hidden.size) return;
  const n = [...hidden].flat().filter((child) => child.classList.contains('tool')).length;
  const more = el('button', 'group-more top');
  more.type = 'button';
  const arrow = icon('down', 14);
  arrow.style.transform = 'rotate(180deg)';
  more.append(arrow, el('span', '', `${n} earlier step${n === 1 ? '' : 's'}`));
  more.onclick = () => {
    steps.dataset.all = '1';
    foldLong(steps);
  };
  steps.list.prepend(more);
}

/** The button that draws the earlier part of a long history (`messages`, from its
 * first) in its place. */
/** The first `count` messages, read when asked for (not held meanwhile: a long thread's
 * early history can be most of its size). */
/** A button that shows (then hides, and shows again) a part of the history read on
 * demand: `load()` fills a section once, the button then toggles it. */
function foldingButton(cls, showing, hiding, load) {
  const button = el('button', cls, showing);
  button.type = 'button';
  const section = el('section', 'earlier');
  let loaded = false;
  button.onclick = async () => {
    if (loaded) {
      section.hidden = !section.hidden;
      button.textContent = section.hidden ? showing : hiding;
      button.setAttribute('aria-expanded', String(!section.hidden));
      return;
    }
    button.disabled = true;
    try {
      const filled = await load(section);
      if (!filled) return;
      loaded = true;
      // Before the button, unless `load` placed it (a compaction's go before its notice).
      if (!section.isConnected) button.before(section);
      button.textContent = hiding;
      button.setAttribute('aria-expanded', 'true');
    } catch (error) {
      toast(error.message);
    } finally {
      button.disabled = false;
    }
  };
  button.setAttribute('aria-expanded', 'false');
  return button;
}

function earlierButton(thread, count) {
  return foldingButton('earlier-button wide', `Show ${count} earlier messages`, `Hide ${count} earlier messages`, async (section) => {
    const data = await ask('open', { id: thread.id });
    if (current !== thread.id) return false;
    const messages = (data.history?.messages ?? []).slice(0, count);
    renderMessages(section, thread, messages, 0);
    shown.prompts = [...messages.flatMap((message, index) => (isPrompt(message) ? [index] : [])), ...shown.prompts];
    // Its tasks' cards and reports, as the thread's tasks were last read.
    renderTasks(thread.id, taskList);
    return true;
  });
}

/** Where a compaction folded earlier messages into a summary: says so, shows the summary
 * the model goes on from, and the messages themselves when asked (from the history at
 * the point before it). */
function compactionNode(thread, text, revision) {
  const node = el('div', 'notice compacted');
  node.append(el('p', '', "Earlier messages were folded into a summary so this thread fits its model's context. The model goes on from the summary; the workspace is as it was."));
  const summary = el('details');
  summary.append(el('summary', '', 'The summary'), el('pre', 'summary-text', text.slice(text.indexOf('\n') + 1).trim()));
  // The messages go before the notice (where they were); the button stays in it.
  const show = foldingButton('', 'Show the earlier messages', 'Hide the earlier messages', async (section) => {
    const earlier = await ask('earlier', { id: thread.id, revision });
    section.append(el('span', 'role-label', 'BEFORE THE SUMMARY'));
    renderMessages(section, thread, earlier.history.messages ?? [], 0, { revision: earlier.revision });
    node.before(section);
    renderTasks(thread.id, taskList);
    return true;
  });
  node.append(summary, show);
  return node;
}

/** The back arrow beside a user's message: back to just before it. */
function rewindButton(thread, at, revision) {
  const button = iconButton('rewind', 'Go back to before this message', () => rewindTo(thread.id, at, revision), 'icon-button rewind', 13);
  button.title = 'Go back to before this message (the conversation and the workspace); the message returns to the box';
  return button;
}

async function rewindTo(id, at, revision) {
  if (threads.get(id)?.state === 'running') {
    toast('Stop this thread before going back.');
    return;
  }
  const ok = await confirmed('Go back to before this message?',
    "The conversation and the workspace return to how they were before it, and the message comes back to the box below, to change or send again. What came after stays in the thread's record but not on screen; what commands did outside the workspace (on the web, say) is not undone.",
    'Go back');
  if (!ok) return;
  try {
    const { text, files } = await ask('rewindTo', { id, at, revision });
    if (current === id) await openThread(id);
    // Into the box only if this thread is still the one open (else kept for it, for when
    // it is): never into another's, whatever was opened meanwhile.
    const here = backToBox(id, text, asAttached(files));
    toast(here ? 'Back to before that message: it is in the box below.' : `${named(id, 'That thread')} went back to before that message: it is in the box when you open it.`);
  } catch (error) {
    toast(error.message);
  }
}

/** A message's files (`{path, bytes}`) as the box holds them. */
const asAttached = (files) => files.map(({ path, bytes }) => ({ name: path, size: bytes.length, file: new File([bytes], path.split('/').pop()) }));

/** A message and its files back in the box, to change or send again: before what the
 * box holds, which stays (a file of the same name is replaced). */
function intoBox(text, files) {
  $('prompt').value = [text, $('prompt').value.trim()].filter(Boolean).join('\n\n');
  autosize();
  attached = [...files, ...attached.filter((kept) => !files.some((file) => file.name === kept.name))];
  renderAttached();
  $('prompt').focus();
}

/** Messages that came back from a thread (Take back, a rewind, a send that failed) while
 * another was open: the box is every thread's, so each goes in only once its own thread
 * is open again, never into another's. Kept in this extension's storage (`came-back:ID`,
 * ID `start` for the start page; its files as they are), the one record every panel
 * shares: added to in one transaction (`merge`) and taken whole in one (`take`), so no
 * panel's message overwrites another's, and only one panel puts it in a box. Gone with
 * its thread (the engine's `remove`). Held here only if storage refused it. */
const cameBack = new Map();
const cameBackKey = (id) => `came-back:${id ?? 'start'}`;

/** A thread as a toast names it: its title in quotes, or `otherwise` (unquoted). */
const named = (id, otherwise) => (threads.get(id)?.title ? `“${threads.get(id).title}”` : otherwise);

/** `back` before what `kept` holds (a file of the same name: the newer one). */
function together(back, kept) {
  if (!kept) return back;
  return {
    text: [back.text, kept.text].filter(Boolean).join('\n\n'),
    files: [...back.files, ...kept.files.filter((file) => !back.files.some((one) => one.name === file.name))],
  };
}

/** Kept for thread `id` (null: the start page) until it is opened. */
function keepBack(id, message) {
  const stored = { text: message.text, files: message.files.map(({ name, size, file }) => ({ name, size, file })) };
  merge('kv', cameBackKey(id), stored, together, 'strict').catch((error) => {
    console.warn('Spindle: a message that came back is kept only while this panel is open:', error);
    cameBack.set(id, together(stored, cameBack.get(id)));
  });
}

/** A message (`files`: as the box holds them) back from thread `id` (null: the start
 * page): into the box if that is what is open now, else kept for when it is. True if it
 * is in the box. */
function backToBox(id, text, files) {
  if (current === id) {
    intoBox(text, files);
    return true;
  }
  keepBack(id, { text, files });
  return false;
}

/** What came back for thread `id` while it was not open, into the box now it is (once:
 * taken from storage; kept again if another thread was opened meanwhile). */
function restoreCameBack(id) {
  const into = (message) => {
    intoBox(message.text, message.files);
    toast('A message that came back while this was not open is in the box below.');
  };
  const held = cameBack.get(id);
  if (held) {
    cameBack.delete(id);
    into(held);
  }
  take('kv', cameBackKey(id)).then((stored) => {
    if (!stored) return;
    if (current === id) into(stored);
    else keepBack(id, stored);
  }, () => {});
}

/** A thread deleted: what came back for it goes with it (the engine removes it too). */
function forgetCameBack(id) {
  cameBack.delete(id);
  remove('kv', cameBackKey(id)).catch(() => {});
}

/** The pictures attached to a message (a screenshot the user took, say), as cards after
 * it in `box` (previews to ask for: `card.preview()`). */
function picturesWith(id, text, box) {
  const listed = /\n\n\[Attached, in the workspace: (.*)\]$/s.exec(text)?.[1] ?? '';
  const paths = [...listed.matchAll(/(.+?) \(\d+ bytes\)(?:, |$)/g)].map((match) => match[1]);
  return paths.filter((path) => kindOf(path) === 'picture').map((path) => {
    const card = fileCard(id, path);
    box.append(card);
    return card;
  });
}

function messageNode(role, text, label) {
  // A message sent while a turn ran (`label`) is a bubble of its own, at the right.
  if (label) return steerNode(text, true);
  const article = el('article', `message-wrap ${role}`);
  const head = el('span', 'role-label', role === 'user' ? 'YOU' : 'SPINDLE');
  article.append(head);
  const body = el('div', `message ${role}${role === 'assistant' ? ' md' : ''}`);
  if (role === 'assistant') {
    mdNodes(body, markdown(text));
    tidyTables(body);
    head.append(copyButton(() => text, 'Copy this answer'));
  } else {
    body.textContent = text;
  }
  article.append(body);
  return article;
}

/** Messages sent to the running turn, shown before it picked them up (a check marks them
 * once it did). */
let pendingSteers = [];

/** What the user sent while a turn ran: a bubble at the right, three lines of it until
 * opened; `picked`: the running turn has read it. */
function steerNode(text, picked) {
  const article = el('article', 'message-wrap user steer');
  article.steerText = text;
  const head = el('span', 'role-label', 'YOU · WHILE IT RAN');
  const body = el('div', 'message user');
  const words = el('p', 'steer-text', text);
  body.append(words);
  article.append(head, body);
  if (picked) markPicked(article);
  else article.title = 'Sent: the running turn reads it at its next step';
  // Three lines of it, and an arrow for the rest when there is more (`long`, measured
  // whenever its size changes: shown, hidden, the panel wider or narrower).
  const more = el('button', 'steer-more');
  more.type = 'button';
  more.setAttribute('aria-label', 'Show all of this message');
  more.title = 'Show all of this message';
  more.setAttribute('aria-expanded', 'false');
  more.append(icon('down', 12));
  more.onclick = () => {
    const whole = article.classList.toggle('whole');
    more.setAttribute('aria-expanded', String(whole));
    more.setAttribute('aria-label', whole ? 'Show less of this message' : 'Show all of this message');
    more.title = more.getAttribute('aria-label');
  };
  body.append(more);
  clamped.observe(words);
  return article;
}

/** Whether a steering message's three lines hold all of it (measured once it is laid out;
 * opened, it stays as it was). Measured here, marked a frame later: a class changed inside
 * the callback resizes what is observed, which Chrome reports as a loop. */
const clamped = new ResizeObserver((entries) => {
  const changed = [];
  for (const { target } of entries) {
    if (!target.isConnected) {
      clamped.unobserve(target);
      continue;
    }
    const article = target.closest('.message-wrap');
    if (!article || article.classList.contains('whole') || !target.clientHeight) continue;
    const long = target.scrollHeight > target.clientHeight + 1;
    if (long !== article.classList.contains('long')) changed.push([article, long]);
  }
  if (changed.length) requestAnimationFrame(() => {
    for (const [article, long] of changed) article.classList.toggle('long', long);
  });
});

/** A sent message's check: the running turn read it. */
function markPicked(article) {
  article.removeAttribute('title');
  if (article.querySelector('.picked')) return;
  const mark = el('span', 'picked');
  const check = icon('check', 12);
  check.removeAttribute('aria-hidden');
  check.setAttribute('role', 'img');
  check.setAttribute('aria-label', 'Picked up by the running turn');
  mark.title = 'Picked up by the running turn';
  mark.append(check);
  article.querySelector('.role-label').append(mark);
}

/** Table cells holding a short token (a handle, a ticker, a number) keep it on one line;
 * a table wider than the panel scrolls. */
function tidyTables(box) {
  for (const cell of box.querySelectorAll('td, th')) {
    if (cell.textContent.trim().length <= 28) cell.classList.add('nowrap');
  }
}

/** A step's row: an icon for its kind (its site's, for a page step), what it does in plain
 * words (in the tense of where it is; a page step's address on hover), how long it took;
 * opened, a picture it took, its input and output a line each, and copy, view and
 * download. `id`: its call's id; `live`: seen starting now (it is timed). */
function toolCard(name, input, id = null, live = false) {
  const card = el('details', 'tool running');
  card.dataset.tool = name;
  if (id) card.dataset.id = id;
  card.input = input;
  card.followers = [];
  // On the page's steady clock: a step's time stays right if the system clock is changed.
  if (live) {
    card.started = performance.now();
    card.startedAt = Date.now();
  }
  const summary = el('summary');
  const count = el('span', 'tool-count');
  count.hidden = true;
  summary.append(el('span', 'tool-icon'), el('span', 'tool-label'), el('span', 'tool-where'), count, el('span', 'tool-time'),
    el('span', 'tool-name', name), el('span', 'tool-state', 'running'));
  summary.querySelector('.tool-where').className = 'tool-where sr-only';
  card.append(summary);
  const detail = el('div', 'tool-detail');
  const lines = el('div', 'tool-lines');
  const command = inputText(name, input);
  const first = (text) => String(text ?? '').split('\n').find((line) => line.trim()) ?? '';
  const shown = name === 'write_file' || name === 'edit_file' ? input?.path ?? '' : name === 'bash' ? first(command) : input && Object.keys(input).length ? JSON.stringify(input) : '';
  const inLine = el('code', 'tool-in', shown);
  inLine.title = shown;
  inLine.hidden = !shown;
  const outLine = el('span', 'tool-out');
  outLine.hidden = true;
  const actions = el('div', 'tool-actions');
  const full = el('div', 'tool-full');
  full.hidden = true;
  if (command) {
    const box = el('div', 'tool-input');
    box.append(name === 'edit_file' && typeof input?.old_string === 'string' ? editDiff(input) : el('pre', 'input', command), copyButton(() => command, name === 'bash' ? 'Copy the command' : name === 'write_file' ? 'Copy what it wrote' : name === 'edit_file' ? 'Copy the change' : 'Copy the input'));
    full.append(box);
  }
  actions.append(copyButton(() => command ?? card.resultText ?? '', command ? (name === 'bash' ? 'Copy the command' : name === 'write_file' ? 'Copy what it wrote' : 'Copy the input') : 'Copy its output'));
  const view = iconButton('eye', 'Show all of its input and output', (event) => {
    event.stopPropagation();
    full.hidden = !full.hidden;
    view.replaceChildren(icon(full.hidden ? 'eye' : 'eye-off'));
    const said = full.hidden ? 'Show all of its input and output' : 'Hide its full input and output';
    view.title = said;
    view.setAttribute('aria-label', said);
    view.setAttribute('aria-expanded', String(!full.hidden));
  });
  view.setAttribute('aria-expanded', 'false');
  actions.append(view);
  lines.append(inLine, outLine, actions);
  detail.append(lines, full);
  card.append(detail);
  card.detail = detail;
  card.full = full;
  card.outLine = outLine;
  card.actions = actions;
  // Opened, the steps merged into its row show too.
  card.addEventListener('toggle', () => {
    for (const follower of card.followers ?? []) follower.classList.toggle('shown', card.open);
  });
  relabel(card);
  return card;
}

/** An edit as a diff: the file's path, then the lines it replaced (marked -) and the
 * lines it put there (marked +), each tinted, so what changed reads at a glance. */
function editDiff(input) {
  const pre = el('pre', 'input diff');
  pre.append(el('span', 'diff-path', `${input.path ?? ''}\n\n`));
  const lines = (text, mark, cls) => {
    for (const line of String(text).split('\n')) pre.append(el('span', cls, `${mark} ${line}\n`));
  };
  lines(input.old_string, '-', 'diff-del');
  lines(input.new_string ?? '', '+', 'diff-ins');
  return pre;
}

/** What a step was given, to read: a command as it is; what a file tool wrote, as text
 * (the file's path above it); any other input as JSON. */
function inputText(name, input) {
  if (!input || typeof input !== 'object') return null;
  if (name === 'bash' && typeof input.command === 'string') return input.command;
  if (name === 'write_file' && typeof input.content === 'string') return `${input.path ?? ''}\n\n${input.content}`;
  if (name === 'edit_file' && typeof input.old_string === 'string') {
    const lines = (text, mark) => String(text).split('\n').map((line) => `${mark} ${line}`).join('\n');
    return `${input.path ?? ''}\n\n${lines(input.old_string, '-')}\n${lines(input.new_string ?? '', '+')}`;
  }
  return Object.keys(input).length ? JSON.stringify(input, null, 2) : null;
}

/** The icon each kind of step shows. */
const KIND_ICONS = { command: 'command', code: 'code', file: 'file', folder: 'folder', tabs: 'tabs', task: 'task', web: 'globe', tool: 'tool', page: 'globe' };

/** A step's row says what it does, as it stands. */
function relabel(card) {
  const state = card.classList.contains('running') ? 'running' : card.classList.contains('failed') ? 'failed' : 'done';
  const { label, short, kind, url, title, domain } = describe(card.dataset.tool, card.input ?? {}, state, knownTabs, knownParts);
  card.querySelector('.tool-label').textContent = short;
  card.pageUrl = url ?? null;
  // A page step's page, for screen readers (the row shows its site's icon).
  card.querySelector('.tool-where').textContent = short !== label && (domain || title) ? ` on ${domain || title}` : '';
  const summary = card.querySelector('summary');
  summary.title = [label, title && title !== domain ? title : null, url].filter(Boolean).join('\n');
  const place = card.querySelector('.tool-icon');
  const shows = state === 'running' ? 'spinner'
    : state === 'failed' ? (card.classList.contains('retried') ? 'retry' : 'x')
    : kind === 'page' && url ? `site ${url}` : KIND_ICONS[kind] ?? 'tool';
  if (place.dataset.shows !== shows) {
    place.dataset.shows = shows;
    if (shows.startsWith('site ')) {
      const image = el('img', 'favicon');
      image.alt = '';
      image.src = faviconOf(url);
      image.onerror = () => place.replaceChildren(icon('globe', 14));
      place.replaceChildren(image);
    } else {
      place.replaceChildren(icon(shows, 14));
    }
  }
  paintTime(card);
}

/** A row's time: its own, or with the steps merged into it, theirs added; running, on the
 * clock. */
function paintTime(card) {
  const time = card.querySelector('.tool-time');
  if (card.followers?.length) {
    const all = [card, ...card.followers].map((one) => one.ms).filter(Number.isFinite);
    time.textContent = all.length ? duration(all.reduce((sum, ms) => sum + ms, 0)) : '';
  } else {
    time.textContent = card.ms != null ? duration(card.ms) : card.started != null ? clock(performance.now() - card.started) : '';
  }
}

/** The little icon Chrome keeps for a site (the extension's favicon service). */
const faviconOf = (url) => chrome.runtime.getURL(`_favicon/?pageUrl=${encodeURIComponent(url)}&size=32`);

/** A step's state in words (read by screen readers; its icon shows it). */
function stateText(card) {
  const failed = card.classList.contains('failed');
  card.querySelector('.tool-state').textContent = card.classList.contains('running') ? 'running'
    : failed && card.classList.contains('retried') ? 'failed → retried ✓' : failed ? 'failed' : 'done';
}

function settle(card, text, failed) {
  card.classList.remove('running');
  card.classList.toggle('failed', failed);
  if (card.started != null) {
    // How long it ran (steady clock), placed at when it ended (wall clock), so spans of
    // steps from different pages and panels line up.
    const end = Date.now();
    card.span = [end - (performance.now() - card.started), end];
    keepTiming(card.dataset.id, card.span);
  } else if (card.dataset.id && timings.has(card.dataset.id)) {
    const kept = timings.get(card.dataset.id);
    if (isSpan(kept)) card.span = kept;
    else if (Number.isFinite(kept) && kept >= 0) card.ms = kept;
  }
  if (card.span) card.ms = card.span[1] - card.span[0];
  card.resultText = text;
  card.full.append(el('pre', 'result', text.length > 20000 ? `${text.slice(0, 20000)}\n[…]` : text));
  const first = text.split('\n').find((line) => line.trim()) ?? '';
  const saved = card.dataset.tool === 'screenshot' && /^Saved (screenshots\/\S+\.jpg)/.exec(first)?.[1];
  card.outLine.textContent = saved ? shownName(saved) : first;
  card.outLine.title = first;
  card.outLine.hidden = !first;
  // What a step found out about tabs and pages (one that failed found out nothing).
  if (!failed) {
    const told = [{ name: card.dataset.tool, input: card.input, result: text }];
    for (const [tab, known] of tabsFrom(told)) knownTabs.set(tab, known);
    for (const [ref, part] of partsFrom(told)) knownParts.set(ref, part);
  }
  const shot = card.dataset.tool === 'screenshot' && !failed && /^Saved (screenshots\/\S+\.jpg)/.exec(text)?.[1];
  const path = shot || (['write_file', 'edit_file'].includes(card.dataset.tool) && !failed ? workspacePath(card.input?.path) : null);
  const thread = current;
  const name = path ? path.split('/').pop() : `${card.dataset.tool}-output.txt`;
  card.actions.append(iconButton('download', shot ? 'Download the picture' : path ? `Download ${name}` : 'Download its output', async (event) => {
    event.stopPropagation();
    try {
      if (path) await saveFile(await readBytes(thread, path), name, typeOf(path));
      else await saveFile(new TextEncoder().encode(text), name, 'text/plain');
    } catch (error) {
      toast(`${name} was not downloaded: ${error.message}`);
    }
  }));
  relabel(card);
  stateText(card);
  const steps = card.closest('.steps');
  if (steps) updateSteps(steps);
  if (shot && current) picture(card, current, shot);
}

// The running steps' times go on as they run, and their groups' too.
setInterval(() => {
  for (const card of document.querySelectorAll('#history .tool.running')) {
    if (card.started != null) card.querySelector('.tool-time').textContent = clock(performance.now() - card.started);
  }
  for (const steps of document.querySelectorAll('#history .steps.running')) {
    if (Number.isFinite(steps.started)) steps.querySelector('.group-time').textContent = clock(Date.now() - steps.started);
  }
  tickElapsed();
}, 1000);

/** A screenshot a tool saved, shown in its row's detail as a small picture that opens it
 * (read from the thread's workspace; a just-taken one may land there a moment after its
 * result). */
async function picture(card, id, path, tries = 5) {
  try {
    const { bytes } = await ask('read', { id, path });
    const url = URL.createObjectURL(new Blob([bytes], { type: 'image/jpeg' }));
    const image = el('img', 'shot');
    image.alt = shownName(path);
    image.onload = image.onerror = () => URL.revokeObjectURL(url);
    image.src = url;
    const thumb = el('button', 'tool-thumb');
    thumb.type = 'button';
    thumb.title = 'Open the picture of the tab';
    thumb.setAttribute('aria-label', 'Open the picture of the tab');
    thumb.append(image);
    thumb.onclick = (event) => {
      event.stopPropagation();
      openWorkspace(id, path);
    };
    card.detail.prepend(thumb);
    card.open = true;
  } catch {
    if (tries > 1 && current === id && card.isConnected) setTimeout(() => picture(card, id, path, tries - 1), 1000);
  }
}

let drawing = 0;
function drawLive() {
  drawing = 0;
  if (!live) return;
  live.body.replaceChildren();
  mdNodes(live.body, markdown(live.text));
  scrollDown();
}

function startLive() {
  live = { text: '', node: el('article', 'message-wrap assistant live') };
  live.node.append(el('span', 'role-label', 'SPINDLE · WRITING'));
  live.body = el('div', 'message assistant md body');
  live.node.append(live.body);
  $('history').append(live.node);
}

function endLive(text) {
  if (!live) return;
  const node = live.node;
  live = null;
  if (text != null && text.trim()) node.replaceWith(messageNode('assistant', text));
  else node.remove();
}

/** What a thread is doing, for its row in the list: its step running now (and which of
 * its run), or its model writing. */
function noteActivity(id, event) {
  const now = activity.get(id) ?? { text: 'Working', step: 0, of: 0 };
  if (event.type === 'model_step') Object.assign(now, { text: 'Writing', step: 0, of: 0 });
  else if (event.type === 'tool_call') {
    now.of += 1;
    now.step = now.of;
    now.text = describe(event.name, event.input ?? {}, 'running').short;
  } else return;
  activity.set(id, now);
  if (!current) renderThreadSoon();
}
let threadsDue = 0;
function renderThreadSoon() {
  threadsDue ||= setTimeout(() => {
    threadsDue = 0;
    if (!current) renderThreads();
  }, 500);
}

function onEvent(id, event) {
  noteActivity(id, event);
  if (id !== current) return;
  const box = $('history');
  switch (event.type) {
    // A task's own work, and its report reaching the thread: its line keeps up.
    case 'task':
      loadTasks(id);
      return;
    // A task's report reached the thread: it shows as the task's.
    case 'task_reported': {
      endLive(live?.text);
      const state = /^\[task .*?: (finished|failed|stopped)\]\n/.exec(event.text ?? '')?.[1];
      if (event.name && state) box.append(reportNode({ text: event.text, report: { task: event.name, state } }));
      loadTasks(id, true);
      break;
    }
    case 'prompt':
      historyPaint += 1;
      // A thread's first prompt: nothing was there to read (a history read meanwhile is
      // drawn when the turn ends).
      box.querySelector(':scope > .opening')?.remove();
      box.append(messageNode('user', event.text));
      for (const card of picturesWith(id, event.text, box)) card.preview();
      break;
    case 'steered': {
      // Read by the running turn: the message shown when it was sent gets its check, and
      // comes here, where the turn read it.
      const sent = pendingSteers.findIndex((node) => node.steerText === event.text);
      const node = sent >= 0 ? pendingSteers.splice(sent, 1)[0] : steerNode(event.text, true);
      markPicked(node);
      box.append(node);
      scrollDown(true);
      break;
    }
    case 'model_step':
      // Its model writes: the turn waits for no task now (Stop stops it).
      waitingOnTasks = false;
      renderSend();
      endLive(live?.text);
      startLive();
      phase('Working', 'busy');
      loadSpend(id, true);
      break;
    case 'model_delta':
      if (!live) startLive();
      live.text += event.text;
      drawing ||= requestAnimationFrame(drawLive);
      break;
    case 'model_text_withdrawn':
      if (live) {
        live.text = '';
        drawLive();
      }
      break;
    case 'model_text':
      // The step's text is in. After a streamed reply it comes empty: what streamed
      // stands (a tool call or the turn's end keeps it; a retry or a cut takes it back).
      if (event.text?.trim()) {
        if (!live) startLive();
        live.text = event.text;
        drawLive();
      }
      break;
    case 'reply_cut':
      endLive(null);
      break;
    case 'tool_call': {
      endLive(live?.text);
      const card = toolCard(event.name, event.input, event.id ?? null, true);
      pendingCards.push(card);
      const steps = stepsIn(box, true);
      steps.list.append(card);
      if (event.name === 'task') steps.append(taskBlock(card));
      updateSteps(steps);
      // The running turn's steps show unless the user shut them.
      if (!steps.group.dataset.user) setOpen(steps.group, true);
      phase('Working', 'busy');
      break;
    }
    case 'tool_result': {
      stepTaken();
      const card = pendingCards.shift();
      if (card) {
        settle(card, event.content, event.failed);
        const made = !event.failed && wroteFile(card);
        if (made) placeFile(card, made);
        if (card.block) taskCalled(id, card.block, event.content);
      }
      break;
    }
    case 'tool_refused': {
      stepTaken();
      const card = pendingCards.shift();
      if (card) settle(card, `refused: ${event.reason}`, true);
      if (card?.block) taskCalled(id, card.block, `refused: ${event.reason}`);
      break;
    }
    case 'provider_retry':
      endLive(null);
      phase(`The provider answered ${event.status}; trying again in ${event.seconds}s`, 'busy');
      break;
    case 'stopped':
      endLive(live?.text);
      break;
    case 'nested': {
      // A task's own steps, start and end show on its card; what else befalls it is said
      // by its name.
      const said = TASK_SAID.exec(event.label ?? '');
      if (said && ROUTINE_TASK.test(said[2])) return;
      // A script's own Spindle calls belong with the step running it (not with a lone
      // task step, which its card stands in for).
      const last = box.lastElementChild;
      const steps = last?.classList.contains('steps') && !(last.classList.contains('single') && last.querySelector('.tool[data-tool="task"]')) ? last : null;
      (steps?.list ?? box).append(el('p', 'notice nested', said ? `${taskLabel(said[1])}: ${said[2]}` : event.label));
      break;
    }
    case 'compacted':
      shown.compacted = true;
      box.append(el('p', 'notice', `History compacted (${Math.round(event.before / 1024)} → ${Math.round(event.after / 1024)} KiB): earlier messages are folded into a summary the model goes on from.`));
      break;
    default:
      return;
  }
  scrollDown();
}

async function onTurnEnd(id, unsent) {
  refreshWorkspace(id);
  activity.delete(id);
  if (id !== current) return;
  pendingSteers = [];
  loadSpend(id);
  if (threads.get(id)?.tasks) loadTasks(id, true);
  const paint = ++historyPaint;
  endLive(live?.text);
  // Messages the turn ended before reading go back in the box, to send as the next one.
  if (unsent.length && !$('prompt').value.trim()) {
    $('prompt').value = unsent.join('\n\n');
    autosize();
    toast('The turn ended before reading your message; it is back in the box to send.');
  }
  try {
    // Only what the turn changed is drawn again: from its first message on (all of it
    // after a compaction, which changed what came before).
    const from = shown.compacted ? 0 : (shown.turnFrom ?? shown.prompts.at(-1) ?? 0);
    let data = await ask('open', { id, ...(from && { from }) });
    if (current !== id || historyPaint !== paint) return;
    if (!renderHistory(data, { from })) {
      data = await ask('open', { id });
      if (current !== id || historyPaint !== paint) return;
      renderHistory(data);
    }
    status = data.status;
  } catch {}
  renderBar();
}

// ---- Files in the conversation ------------------------------------------------------

const readBytes = async (id, path) => (await ask('read', { id, path })).bytes;
/** A file tool's path as the workspace names it (the tools take `name`, `./name` and
 * `/workspace/name`). */
const workspacePath = (raw) => typeof raw !== 'string' ? null
  : raw.startsWith('/workspace/') ? raw.slice('/workspace/'.length) : raw.startsWith('./') ? raw.slice(2) : raw;
const viewUrl = (id, path) => chrome.runtime.getURL(`view.html?thread=${encodeURIComponent(id)}&path=${encodeURIComponent(path)}`);
/** Kinds that sit below the run of steps that wrote them, in sight. */
const PRESENTED = new Set(['page', 'picture', 'markdown', 'document']);
/** Of those, the kinds that show themselves while the turn works on them (a page does
 * not: the thread opens it in a tab when it means to show it, and the eye opens it
 * here). One at a time: a file written next folds the one before it, as do the run's end
 * and the turn moving on (`SHOWN_FOR` steps later); one the user opened or shut stays as
 * they left it. The thread's own notes (`isNotes`) are neither presented nor shown:
 * their card sits in the run after the step that wrote them, folded. */
const SELF_SHOWING = new Set(['picture', 'markdown', 'document']);
/** The card whose preview the running turn opened last (`wroteFile`). */
let selfShown = null;
/** How many steps after its own a preview the running turn opened stays open: by then the
 * turn has moved on from it (a run with no words between its steps can go on for
 * hundreds). */
const SHOWN_FOR = 3;
/** The running turn's self-shown cards, each with the steps it has left (`stepTaken`). */
const showing = new Map();

/** A step of the running turn ended: the previews it opened before fold once the turn has
 * gone `SHOWN_FOR` steps past them, but for one the user is reading (`readingAt`), which
 * folds at a later step, once they have left it. */
function stepTaken() {
  for (const [card, left] of showing) {
    if (!card.isConnected) {
      showing.delete(card);
    } else if (left > 1) {
      showing.set(card, left - 1);
    } else if (!readingAt(card)) {
      showing.delete(card);
      card.unpreview?.();
    }
  }
}

/** Whether the user reads `card` where it is: it shows on screen while they have scrolled
 * up from the conversation's end (`following`; folding it would pull it from under them). */
function readingAt(card) {
  if (following) return false;
  const shown = card.getBoundingClientRect();
  const view = $('main').getBoundingClientRect();
  return shown.bottom > view.top && shown.top < view.bottom;
}
/** Kinds the workspace offers to open in a tab of their own (view.html). */
const TABBED = new Set(['page', 'picture', 'pdf', 'document', 'media']);
/** What a kind is called where a file is listed. */
const KIND_NAMES = { page: 'page', picture: 'picture', markdown: 'Markdown', text: 'text', pdf: 'PDF', document: 'document', media: 'audio or video' };

/** Where a file a step wrote shows: a page, picture, Markdown file or document below its
 * run of steps, in sight; any other file, and the thread's own notes (`isNotes`), inside
 * the run, after its step. One card per file in a run: the latest. */
function placeFile(card, made) {
  const steps = card.closest('.steps');
  if (!steps) {
    card.after(made);
    return;
  }
  for (const old of steps.querySelectorAll(`.file-card[data-path="${CSS.escape(made.dataset.path)}"]`)) old.remove();
  if (PRESENTED.has(kindOf(made.dataset.path)) && !isNotes(made.dataset.path)) steps.append(made);
  else card.after(made);
}

/** The card of the file a file tool wrote (write_file, edit_file), or null; `preview`:
 * a page, picture or Markdown file shows itself at once (as a live step's does). */
function wroteFile(card, preview = true) {
  const path = workspacePath(card.input?.path);
  if (!current || !path || !['write_file', 'edit_file'].includes(card.dataset.tool)) return null;
  turnPaths.add(path);
  const kind = kindOf(path);
  const shows = preview && SELF_SHOWING.has(kind) && !isNotes(path);
  const made = fileCard(current, path, { content: card.dataset.tool === 'write_file' ? card.input.content : null, preview: shows });
  // A Markdown file or document takes the one place: the one shown before it folds. A
  // picture folds nothing and holds no place (a chart stays beside the Markdown on it).
  if (shows && kind !== 'picture') {
    if (selfShown !== made) selfShown?.unpreview?.();
    selfShown = made;
  }
  if (shows) showing.set(made, SHOWN_FOR);
  return made;
}

/** A file of thread `id` in the conversation: its icon, name (opening it in the workspace
 * page), folder and type; an eye that shows its preview here (a page or document in a
 * sandboxed frame, a picture, audio or video, Markdown, text), for a PDF a button that
 * opens it in a tab instead, and a download. `content`: its text as a step wrote it,
 * when at hand. */
function fileCard(id, path, { content = null, size: bytes = null, preview = false } = {}) {
  const kind = kindOf(path);
  const card = el('div', `file-card kind-${kind ?? 'bytes'}`);
  card.dataset.path = path;
  const head = el('div', 'file-head');
  const mark = el('span', 'file-icon');
  mark.append(icon(kind === 'picture' ? 'picture' : 'file', 18));
  const text = el('span', 'file-text');
  const at = path.lastIndexOf('/');
  const name = el('button', 'file-name', shownName(path));
  name.type = 'button';
  name.title = `Open ${path} in the workspace`;
  name.onclick = () => openWorkspace(id, path);
  const type = /\.([A-Za-z0-9]{1,8})$/.exec(path)?.[1]?.toLowerCase() ?? ({ ...KIND_NAMES, markdown: 'md' }[kind] ?? 'file');
  const meta = el('small', 'file-meta', [at > 0 ? `${path.slice(0, at)}/` : null, type, bytes != null ? size(bytes) : null].filter(Boolean).join(' · '));
  text.append(name, meta);
  const actions = el('span', 'file-actions');
  const action = (icon, title, run) => {
    const button = iconButton(icon, title, run);
    actions.append(button);
    return button;
  };
  const body = el('div', 'file-body');
  body.hidden = true;
  const view = el('div', 'file-view');
  let filled = false;
  const fill = () => {
    filled = true;
    return previewInto(view, id, path, { content, size: bytes });
  };
  if (FRAMED.has(kind)) {
    // A page's (a document's) preview can show more of itself.
    const what = kind === 'document' ? 'document' : 'page';
    const tools = el('div', 'file-tools');
    const taller = iconButton('taller', `Show more of the ${what} here`, () => {
      card.classList.toggle('tall');
      const tall = card.classList.contains('tall');
      taller.replaceChildren(icon(tall ? 'shorter' : 'taller'));
      taller.title = tall ? `Show less of the ${what} here` : `Show more of the ${what} here`;
      taller.setAttribute('aria-label', taller.title);
    });
    tools.append(taller);
    body.append(tools);
  }
  body.append(view);
  let toggle = null;
  if (kind === 'pdf') {
    // In Chrome's own viewer, a tab of its own (never in the panel: a link clicked in a
    // PDF takes the page around the viewer with it).
    action('external', `Open ${path} in a tab`, () => chrome.tabs.create({ url: viewUrl(id, path) }));
  } else if (kind) {
    const show = (shown) => {
      body.hidden = !shown;
      // Folded away, what plays stops (it would go on unseen).
      if (!shown) for (const player of body.querySelectorAll('audio, video')) player.pause();
      toggle.replaceChildren(icon(shown ? 'eye-off' : 'eye'));
      toggle.title = shown ? `Hide ${path}` : `Preview ${path}`;
      toggle.setAttribute('aria-label', toggle.title);
      toggle.setAttribute('aria-expanded', String(shown));
      if (shown && !filled) fill();
    };
    // The user's doing is kept: a preview they opened or shut is not folded or shown
    // for them again.
    toggle = action('eye', `Preview ${path}`, () => {
      card.dataset.user = '1';
      show(body.hidden);
    });
    toggle.setAttribute('aria-expanded', 'false');
    card.show = show;
  }
  action('download', `Download ${path}`, async () => {
    try {
      await saveFile(await readBytes(id, path), path.split('/').pop(), typeOf(path));
      toast(`Downloaded ${path.split('/').pop()}.`);
    } catch (error) {
      console.warn(`Spindle: ${path} was not downloaded:`, error);
      toast(`${path} was not downloaded: ${error.message}`);
    }
  });
  head.append(mark, text, actions);
  card.append(head, body);
  // Shows its preview (if it has one to show), and folds one it showed itself: neither
  // over what the user did.
  if (toggle && PRESENTED.has(kind)) {
    card.preview = () => {
      if (body.hidden && !card.dataset.user) card.show(true);
    };
    card.unpreview = () => {
      if (!body.hidden && !card.dataset.user) card.show(false);
    };
  }
  if (preview) card.preview?.();
  return card;
}

/** A file's preview into `body`: a page in a sandboxed frame (its scripts running where
 * the frame is contained, and otherwise as the user let this version of it, which also
 * says whether it reaches the web: `previewNote`), a Word, Excel or PowerPoint document
 * drawn as a static page in one, a picture (an SVG that is no picture as a page), audio
 * or video in a player, Markdown, text, and for a PDF a button that opens it in a tab
 * (Chrome's own viewer); with `any`,
 * a file of no known kind shows as text when it is text. `content`: its text, when at
 * hand; `textId`: the id its text block takes. */
async function previewInto(body, id, path, options = {}) {
  const { content = null, any = false, textId = null } = options;
  const kind = kindOf(path);
  // Only the newest preview of this body shows (one asked again before the last came,
  // by Load from the web say): an earlier one that ends after it, ok or not, does not.
  const generation = (body.previewing ?? 0) + 1;
  body.previewing = generation;
  const newest = () => body.previewing === generation;
  // A page's (a document's) preview holds its height from the start: nothing below it
  // jumps (nor what the user is about to click) when it comes.
  body.replaceChildren(el('p', `note${FRAMED.has(kind) ? ' file-waiting' : ''}`, 'Reading…'));
  // What played here before is gone now: its file goes (and any other gone player's).
  sweepPlayers();
  try {
    if (kind === 'page') {
      const contained = containment();
      const { files } = await ask('files', { id });
      // What the user just let it do, or what was kept for it.
      const allowed = options.allowed ?? await previewChoice(id, path);
      const page = await pageOf({ path, files, read: (name) => readBytes(id, name), html: content, allowed, contained: await contained });
      if (!newest()) return;
      // A choice just made is kept for the version it was made on.
      if (options.allowed && page.version && (options.allowed.version ?? page.version) === page.version) await keepPreviewChoice(id, path, page.version, options.allowed).catch(() => {});
      const again = (allowed) => newest() && previewInto(body, id, path, { ...options, allowed });
      const frame = previewFrame(page, path, { refused: () => again(options.allowed) });
      frame.className = 'file-frame';
      body.replaceChildren(frame);
      const note = el('p', 'note file-web');
      if (previewNote(note, page, await contained, again)) body.prepend(note);
    } else if (kind === 'pdf') {
      // Opened in Chrome's own viewer, a tab of its own (view.html becomes it), never in
      // the panel: a link clicked in a PDF takes the page around the viewer with it.
      if (!pdfShows()) {
        body.replaceChildren(el('p', 'note', `PDFs open here in ${BROWSER} 123 and later: download it to open it.`));
        return;
      }
      const open = el('button', 'file-open-tab', 'Open in a tab');
      open.type = 'button';
      open.onclick = () => chrome.tabs.create({ url: viewUrl(id, path) });
      body.replaceChildren(el('p', 'note', `A PDF opens in ${BROWSER}'s own PDF viewer, in a tab of its own.`), open);
    } else if (kind === 'document') {
      // Too large to draw (when its size is known): said before its bytes are read.
      if (options.size > MAX_DOCUMENT) {
        body.replaceChildren(el('p', 'note', `It is ${size(options.size)}: documents up to ${size(MAX_DOCUMENT)} show here. Download it to open it.`));
        return;
      }
      const { html, notes } = await documentPage(path, await readBytes(id, path));
      const frame = await documentFrame(html, path);
      if (!newest()) return;
      frame.className = 'file-frame';
      body.replaceChildren(frame);
      if (notes.length) body.prepend(el('p', 'note file-web', notes.join(' ')));
    } else if (kind === 'picture') {
      const bytes = await readBytes(id, path);
      if (!newest()) return;
      const url = URL.createObjectURL(pictureBlob(path, bytes));
      const image = el('img', 'file-picture');
      image.alt = shownName(path);
      image.onload = () => URL.revokeObjectURL(url);
      image.onerror = async () => {
        URL.revokeObjectURL(url);
        // An SVG whose markup a picture cannot read shows as a page instead.
        if (!/\.svg$/i.test(path) || !newest() || !image.isConnected) return;
        const frame = await documentFrame(svgPage(bytes), path);
        frame.className = 'file-frame';
        if (newest()) body.replaceChildren(frame);
      };
      image.src = url;
      body.replaceChildren(image);
    } else if (kind === 'media') {
      const bytes = await readBytes(id, path);
      if (!newest()) return;
      const type = typeOf(path);
      const url = URL.createObjectURL(new Blob([bytes], { type }));
      const player = el(type.startsWith('audio/') ? 'audio' : 'video', 'file-media');
      Object.assign(player, { controls: true, preload: 'metadata', src: url });
      players.set(url, player);
      player.setAttribute('aria-label', shownName(path));
      player.onerror = () => {
        if (newest()) body.replaceChildren(el('p', 'note', 'This browser cannot play it: download it to open it.'));
        sweepPlayers();
      };
      body.replaceChildren(player);
    } else if (kind || any) {
      const bytes = content == null ? await readBytes(id, path) : null;
      if (!newest()) return;
      const text = content ?? new TextDecoder('utf-8', { fatal: false }).decode(bytes);
      if (!kind && text.includes('\u0000')) {
        body.replaceChildren(el('p', 'note file-binary', `No preview for this file (${size(bytes.length)}): download it to open it.`));
        return;
      }
      const shown = text.length > 60_000 ? `${text.slice(0, 60_000)}\n[… ${size(text.length - 60_000)} more: download it to read it all]` : text;
      if (kind === 'markdown') {
        const md = el('div', 'md file-markdown');
        mdNodes(md, markdown(shown));
        tidyTables(md);
        if (textId) md.id = textId;
        body.replaceChildren(md);
      } else {
        const pre = el('pre', 'code file-text-body', shown);
        if (textId) {
          pre.id = textId;
          pre.tabIndex = 0;
        }
        body.replaceChildren(pre);
      }
    }
  } catch (error) {
    if (newest()) body.replaceChildren(el('p', 'note', `It could not be shown: ${error.message}`));
  }
}

/** The workspace at the end of the conversation: its files by folder, those the last turn
 * made or changed marked, scratch files (notes, scripts, tests) out of sight unless asked
 * for; a file opens in the workspace page, and all of them download as a .zip. */
function workspaceCard(id) {
  const card = el('details', 'workspace-card');
  const summary = el('summary');
  const mark = el('span', 'ws-card-icon');
  mark.append(icon('folder', 14));
  const label = el('span', 'ws-card-label', 'Workspace');
  const chevron = el('span', 'group-chevron');
  chevron.append(icon('next', 12));
  summary.append(mark, label, chevron);
  card.append(summary);
  turnFiles(id).then(({ files, changes }) => {
    if (!files.length) {
      card.remove();
      return;
    }
    const fresh = files.filter((file) => changes.has(file.path)).length;
    // Folded until the user opens it: its summary says what is there, and an open list
    // would stand between the steps and the final answer.
    label.textContent = `Workspace · ${files.length} file${files.length === 1 ? '' : 's'}${fresh ? ` · ${fresh} new or edited` : ''}`;
    const tree = el('div', 'ws-tree');
    const draw = () => drawTree(tree, files, changes, { onOpen: (path) => openWorkspace(id, path), key: id, redraw: draw });
    const toggle = scratchSwitch(draw);
    const tools = el('div', 'ws-tools');
    const open = el('button', 'link-like', 'Open the workspace');
    open.type = 'button';
    open.onclick = () => openWorkspace(id);
    tools.append(open, toggle);
    draw();
    const all = el('button', 'wide', 'Download all (.zip)');
    all.type = 'button';
    all.onclick = () => downloadAll(id, files, all);
    const inside = el('div', 'ws-card-body');
    inside.append(tools, tree, all);
    card.append(inside);
  }, () => card.remove());
  return card;
}

/** The workspace's files and what the last turn did to them: {files, changes: path ->
 * 'new' | 'edited'}. */
async function turnFiles(id) {
  const { files, before } = await ask('turnFiles', { id });
  const had = before ? new Map(before) : null;
  const changes = new Map();
  if (had) {
    for (const file of files) {
      if (!had.has(file.path)) changes.set(file.path, 'new');
      else if (had.get(file.path) !== file.digest) changes.set(file.path, 'edited');
    }
  }
  return { files, changes };
}

/** Whether scratch files show (one choice for every list, kept). */
let showScratch = (() => {
  try {
    return localStorage.getItem('spindle-scratch') === 'shown';
  } catch {
    return false;
  }
})();
/** The switch that shows or hides scratch files; `redraw` after a change. */
function scratchSwitch(redraw, given = null) {
  const button = given ?? el('button', 'mini-switch', 'Scratch');
  button.type = 'button';
  button.setAttribute('role', 'switch');
  button.title = 'Show scratch files (.txt, .py, tests/)';
  const render = () => button.setAttribute('aria-checked', String(showScratch));
  render();
  button.onclick = () => {
    showScratch = !showScratch;
    try {
      localStorage.setItem('spindle-scratch', showScratch ? 'shown' : 'hidden');
    } catch {}
    render();
    redraw();
  };
  return button;
}

/** The folders open in each thread's file lists, by folder path, while the panel is open:
 * a redraw (a turn ending, the scratch switch, the filter cleared) keeps them. */
const openFolders = new Map();
/** Rows a list draws at once; more wait behind "Show more", so 50,000 files stay quick. */
const ROWS_AT_ONCE = 400;

/** `files` as a tree into `box`: the workspace's own files, then its folders, each shut
 * until opened (a folder holding only a folder shares its row: `lib/python3.14`); a
 * folder says how many files it holds and how many the last turn made or changed. Scratch
 * files stay out (counted) unless shown; `filter` lists the files whose path has it, each
 * with its folder; `onOpen(path)` opens one; `links` adds a download (and, for pages and
 * pictures, an open-in-a-tab) link per row; `key` names whose open folders are kept;
 * `redraw` draws the list again (after scratch files are shown from it). Returns every
 * path listed, open folders or not, in order. */
function drawTree(box, files, changes, { onOpen, filter = '', links = null, key = '', redraw = null }) {
  box.replaceChildren();
  const wanted = filter.trim().toLowerCase();
  const matching = wanted ? files.filter((file) => file.path.toLowerCase().includes(wanted)) : files;
  const listed = showScratch ? matching : matching.filter((file) => !isScratch(file.path));
  const hidden = matching.length - listed.length;
  const tree = fileTree(listed, (file) => changes.get(file.path) ?? '');
  if (!openFolders.has(key)) openFolders.set(key, new Set());
  const open = openFolders.get(key);
  const fileRow = (file, where = '') => {
    const at = file.path.lastIndexOf('/');
    const row = el('div', `file-row${isScratch(file.path) ? ' scratch' : ''}`);
    row.dataset.path = file.path;
    const button = el('button', 'file-open');
    button.type = 'button';
    button.title = `Open ${file.path}`;
    const label = el('span', 'file-row-name', shownName(file.path));
    if (where && at > 0) label.append(el('span', 'sr-only', ' in '), el('span', 'file-row-where', file.path.slice(0, at)));
    button.append(label);
    const change = changes.get(file.path);
    if (change) button.append(el('span', `badge ${change}`, change));
    button.append(el('small', '', size(file.size)));
    button.onclick = () => onOpen(file.path);
    row.append(button);
    if (links) row.append(...links(file));
    return row;
  };
  const folderRow = (node) => {
    const group = el('div', 'ws-folder');
    const head = el('button', 'ws-folder-head');
    head.type = 'button';
    head.dataset.folder = node.path;
    head.title = node.path;
    const chevron = el('span', 'ws-chevron');
    chevron.append(icon('next', 12));
    const mark = el('span', 'ws-folder-icon');
    mark.append(icon('folder', 13));
    head.append(chevron, mark, el('span', 'ws-folder-name', node.name));
    for (const [kind, count] of Object.entries(node.marks)) head.append(el('span', `badge ${kind}`, `${count} ${kind}`));
    head.append(el('small', '', `${node.count} file${node.count === 1 ? '' : 's'}`));
    const inside = el('div', 'ws-folder-files');
    let drawn = false;
    const show = (on) => {
      if (on && !drawn) {
        drawn = true;
        entries(inside, node);
      }
      inside.hidden = !on;
      head.setAttribute('aria-expanded', String(on));
      // Every folder the row stands for, so it stays open (or shut) if a later turn
      // joins or splits the chain.
      for (const path of node.paths) {
        if (on) open.add(path);
        else open.delete(path);
      }
    };
    head.onclick = () => show(inside.hidden);
    show(open.has(node.path));
    group.append(head, inside);
    return group;
  };
  // Rows into `into` a few hundred at a time: "Show more" draws the next ones (and moves
  // the focus it had to the first of them).
  const rows = (into, items, draw) => {
    const more = (from) => {
      const first = into.children.length;
      for (const item of items.slice(from, from + ROWS_AT_ONCE)) into.append(draw(item));
      const left = items.length - from - ROWS_AT_ONCE;
      if (left > 0) {
        const button = el('button', 'link-like ws-more', `Show ${Math.min(left, ROWS_AT_ONCE)} more of ${left}`);
        button.type = 'button';
        button.onclick = () => {
          button.remove();
          more(from + ROWS_AT_ONCE)?.focus();
        };
        into.append(button);
      }
      return into.children[first]?.querySelector('button');
    };
    more(0);
  };
  const entries = (into, node) => rows(into, [...node.files, ...node.folders], (item) => (Array.isArray(item.folders) ? folderRow(item) : fileRow(item)));
  // A filter lists what matches, each with its folder; otherwise the tree, shut.
  if (wanted) rows(box, tree.order, (file) => fileRow(file, true));
  else entries(box, tree);
  if (!listed.length) box.append(el('p', 'note', wanted ? 'No file matches.' : files.length ? 'Only scratch files here.' : 'No files yet.'));
  if (hidden) {
    const more = el('button', 'link-like ws-hidden', `${hidden} scratch file${hidden === 1 ? '' : 's'} hidden: show`);
    more.type = 'button';
    more.onclick = () => {
      showScratch = true;
      try {
        localStorage.setItem('spindle-scratch', 'shown');
      } catch {}
      for (const toggle of document.querySelectorAll('.mini-switch')) toggle.setAttribute('aria-checked', 'true');
      if (redraw) redraw();
      else drawTree(box, files, changes, { onOpen, filter, links, key });
    };
    box.append(more);
  }
  return tree.order.map((file) => file.path);
}

function scrollDown(force = false) {
  // The workspace page scrolls in the same place: a turn going on behind it leaves it be.
  if (document.body.dataset.view === 'workspace') return;
  if (force) following = true;
  if (following) toEnd();
  renderJump();
}

/** The conversation scrolled to its end, and that kept as where it was put (scroll events
 * come with the next frame, if one is drawn: a scroll up is told from this, not them). */
function toEnd() {
  const main = $('main');
  main.scrollTop = main.scrollHeight;
  scrolledTo = main.scrollTop;
}

/** "Latest" shows while a turn runs and the conversation is scrolled up from its end. */
function renderJump() {
  const main = $('main');
  const away = main.scrollHeight - main.scrollTop - main.clientHeight > 200;
  const running = Boolean(current && threads.get(current)?.state === 'running');
  $('jump').hidden = !(running && away && !$('thread').hidden);
}
$('main').addEventListener('scroll', () => {
  // (The same element scrolls the thread list and the workspace page: only the
  // conversation's scrolling says whether it is followed.)
  if (document.body.dataset.view === 'thread') {
    const main = $('main');
    if (main.scrollHeight - main.scrollTop - main.clientHeight < 160) following = true;
    else if (main.scrollTop < scrolledTo - 1) following = false;
    scrolledTo = main.scrollTop;
  }
  renderJump();
}, { passive: true });
// While a turn runs, what comes in after its event (a preview read, a picture loaded)
// keeps the end in sight as its event did, and so does the panel growing shorter (a
// smaller window, the message box growing as the user writes).
const keepingEnd = new ResizeObserver(() => {
  if (following && document.body.dataset.view === 'thread' && current && threads.get(current)?.state === 'running') toEnd();
});
keepingEnd.observe($('history'));
keepingEnd.observe($('main'));
$('jump').onclick = () => {
  const main = $('main');
  following = true;
  main.scrollTo({ top: main.scrollHeight, behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' });
  $('prompt').focus({ preventScroll: true });
};

// ---- The bar and the composer ----------------------------------------------------

function phase(text, mode = '') {
  $('phase').textContent = text;
  $('phase').title = '';
  $('bar').dataset.mode = mode;
}

/** When the open thread's running turn started (seen starting here; else its last change,
 * which its start was). */
const turnStart = new Map();

/** Whether the thread's turn, stopped part way, takes a reply that goes on with it: the
 * step the stop cut off is recorded (or its model request was cut), nothing waits. */
function takesReply(thread) {
  return thread.state === 'stopped' && current === thread.id && Boolean(status?.accepts_message) && !status?.accepts_prompt;
}

/** What a turn stopped part way says, and what each way on does. */
function stoppedNote() {
  const step = status?.phase === 'model_wait'
    ? 'You stopped the model’s reply. A reply asks it again with your message read first (the cut request may have been billed).'
    : 'You stopped this step: it is recorded as stopped, with what it printed, and does not run again. A reply goes on from there: the model reads that you stopped it, then your message (say, skip it and do something else).';
  return `${step}\n\nOr Continue (the model decides what to do next), Retry (asks it again from before that step) or Take back (your message returns to the box).`;
}

/** The composer's top row: the open thread's state on the left (a pulsing dot and how long
 * its turn has run, its tasks at work, a report waiting with Continue), the model and its
 * effort on the right (the model page). A stopped turn's notice and its buttons go at the
 * end of the conversation. */
function renderBar() {
  renderAsking();
  const thread = current ? threads.get(current) : null;
  // What the box sends: a reply, or a message the running turn reads as it goes.
  if (thread) $('prompt').placeholder = thread.state === 'running' ? 'Steer the running turn…' : takesReply(thread) ? 'Reply to go on…' : 'Reply…';
  $('retry').title = thread && takesReply(thread) && status?.phase !== 'model_wait'
    ? 'Ask the model again from before the step you stopped'
    : 'Send the last model request again';
  const show = { resume: false, retry: false, takeback: false };
  const notice = $('notice');
  notice.hidden = true;
  let go = null;
  if (!engine) {
    phase('Starting Spindle…');
  } else if (!thread) {
    if (chosen().provider) phase('New thread', 'new');
    else phase('No providers yet: add one in Settings');
  } else if (thread.moving && thread.moving.step !== 'release') {
    // Moving to or from the user's host: what Spindle does next, and what it waits for.
    phase(thread.moving.text, 'busy');
    if (thread.moving.why) {
      notice.className = 'notice state warning';
      notice.textContent = `It waits: ${explain(thread.moving.why)}. Spindle tries again by itself, also after ${BROWSER} restarts.`;
      notice.hidden = false;
    }
  } else if (thread.state === 'running') {
    if (!$('phase').textContent || $('bar').dataset.mode !== 'busy') phase('Working', 'busy');
  } else if (HALTED.has(thread.state)) {
    show.resume = true;
    show.retry = true;
    show.takeback = true;
    phase({ stopped: 'Stopped part way', failed: 'Failed', interrupted: 'Cut off part way' }[thread.state], 'stopped');
    notice.className = 'notice state warning';
    const note = explain(thread.note ?? STATES[thread.state]);
    notice.textContent = takesReply(thread)
      ? stoppedNote()
      : /Retry|Continue/.test(note)
        ? note
        : `${note}\n\nContinue picks up where it left off; Retry sends the last model request again.`;
    notice.hidden = false;
    // A ChatGPT plan's usage limit (refused at once, or ended in the reply's stream):
    // where the user reviews it comes first.
    if (note.includes(CHATGPT_USAGE) || /subscription_sharing_\w*usage_limit_exceeded/.test(note)) {
      go = ['Manage usage', 'Review and limit what Spindle uses of your ChatGPT plan, in ChatGPT', openUsage];
    }
  } else if (thread.state === 'moved') {
    phase('Moved out');
  } else {
    // What waits for the thread's next turn: a task's report, or a task to go on.
    const reports = taskList.filter((task) => task.queued && !task.delivered && ['finished', 'stopped'].includes(task.state));
    const paused = taskList.filter((task) => task.queued && task.state === 'running' && !task.pending);
    if (reports.length || paused.length) {
      phase(reports.length
        ? reports.length === 1 ? 'A task’s report waits' : `${reports.length} task reports wait`
        : paused.length === 1 ? 'A task waits for the next turn' : `${paused.length} tasks wait for the next turn`);
      $('phase').title = reports.length
        ? `${reports.length === 1 ? 'A task’s report waits' : `${reports.length} task reports wait`} for the thread’s next turn: Continue reads ${reports.length === 1 ? 'it' : 'them'} in`
        : `${paused.length === 1 ? 'A task goes' : `${paused.length} tasks go`} on with the thread’s next turn`;
      go = ['Continue', 'Start the thread’s next turn, which reads it in', () => ask('resume', { id: thread.id }).catch((error) => toast(error.message))];
    } else {
      phase(status?.unresolved ? `${status.unresolved} step(s) with an unknown outcome` : 'Ready');
    }
    if (thread.note) {
      notice.className = 'notice state';
      notice.textContent = explain(thread.note);
      notice.hidden = false;
    }
  }
  // On the user's host, which does not answer now: said over anything else.
  if (thread?.home && thread.away && !thread.moving) {
    notice.className = 'notice state warning';
    notice.textContent = `Your host is out of reach: ${explain(thread.away)}.`;
    notice.hidden = false;
  }
  const goButton = $('status-go');
  goButton.hidden = !go;
  if (go) {
    [goButton.textContent, goButton.title, goButton.onclick] = go;
  }
  $('halted').hidden = !show.resume;
  $('resume').hidden = !show.resume;
  $('retry').hidden = !show.retry;
  $('takeback').hidden = !show.takeback;
  const chip = thread ? { provider: thread.provider, model: thread.model, effort: thread.effort ?? 'default' } : chosen();
  const networked = thread ? thread.network && thread.runner !== 'none' : choice.network && choice.runner !== 'none';
  // The effort as what it does on this model (thinking off, a budget, an effort level).
  const effort = chip.provider ? effortOn(chip.provider, chip.model, chip.effort) : null;
  const effortWord = effort ? effort.short.replace(/^effort /, '') : null;
  const button = $('model-chip');
  $('model-words').textContent = chip.provider ? [chip.model, effortWord].filter(Boolean).join(' · ') : 'Choose a provider';
  if (button.dataset.web !== String(Boolean(networked))) {
    button.dataset.web = String(Boolean(networked));
    for (const mark of button.querySelectorAll('svg')) mark.remove();
    if (networked) button.append(icon('globe', 11));
    button.append(icon('down', 9));
  }
  const said = chip.provider ? `Model ${chip.model}${effort ? `, effort ${effort.label}` : ''}${networked ? ', web on' : ''}` : 'Choose a provider';
  button.setAttribute('aria-label', said);
  button.title = [
    chip.provider && `${nameOf(chip.provider)} · ${chip.model}`,
    effort && `Effort: ${effort.label}`,
    networked && (thread ? "This thread's commands reach the web" : "New threads' commands reach the web"),
    'Change the model, its effort, where commands run and their network',
  ].filter(Boolean).join('\n');
  // A thread that moved out, or is moving to or from the user's host, takes nothing now.
  const away = Boolean(thread && (thread.state === 'moved' || (thread.moving && thread.moving.step !== 'release')));
  button.hidden = away;
  $('prompt').disabled = away;
  $('send').disabled = !engine || away;
  renderSend();
  renderTaskChip();
  tickElapsed();
  renderJump();
}

/** How long the open thread's turn has run, beside "Working". */
function tickElapsed() {
  const thread = current ? threads.get(current) : null;
  const running = thread?.state === 'running';
  const start = running ? turnStart.get(thread.id) ?? thread.updated : null;
  $('elapsed').textContent = start ? clock(Date.now() - start) : '';
}

/** The chip beside the state: how many of the open thread's tasks work now (to the first's
 * card). */
function renderTaskChip() {
  const chip = $('task-chip');
  const working = current ? taskList.filter((task) => task.pending) : [];
  chip.hidden = !working.length;
  if (!working.length) return;
  const said = `${working.length} task${working.length === 1 ? '' : 's'} running`;
  if (chip.dataset.count !== String(working.length)) {
    chip.dataset.count = String(working.length);
    chip.replaceChildren(icon('task', 11), document.createTextNode(String(working.length)));
  }
  chip.setAttribute('aria-label', `${said}: show ${working.length === 1 ? 'it' : 'the first'}`);
  chip.title = working.length === 1 ? `Task working: ${taskTitle(working[0].prompt)}` : said;
  // Its card, when there is one on screen (an app's task has none, nor one in a part of
  // the history not drawn): looked up at the click.
  chip.onclick = () => {
    const card = $('history').querySelector(`.task-block[data-task="${CSS.escape(taskName(working[0]))}"]`);
    if (card) showTask(card);
  };
}

/** The send button: Stop while the open thread's turn runs and nothing is written to
 * steer it with (a red square); Send otherwise (to the running turn, when one runs: a
 * message with files waits for the turn to end, as a running turn takes text only). */
function renderSend() {
  const thread = current ? threads.get(current) : null;
  const running = thread?.state === 'running';
  const stop = running && !$('prompt').value.trim() && !attached.length;
  const mode = stop ? (waitingOnTasks ? 'wait' : 'stop') : running && attached.length ? 'queue' : running ? 'steer' : 'send';
  const button = $('send');
  if (button.dataset.mode !== mode) {
    button.dataset.mode = mode;
    button.classList.toggle('stop', stop);
    button.classList.toggle('primary', !stop);
    button.replaceChildren(...(stop ? [] : [icon('send', 16)]));
  }
  const label = {
    stop: 'Stop this turn', wait: 'Stop waiting for its tasks (they go on, and report with the next turn)', queue: 'Send when this turn ends (a running turn takes text only, not files)', steer: 'Send to the running turn', send: current ? 'Send' : 'Start thread',
  }[mode];
  button.setAttribute('aria-label', label);
  button.title = label;
}

/** The + in the box: opened, it shows Upload and Capture beside it and turns into ×. */
function closeAdd() {
  setAdd(false);
}
function setAdd(open) {
  const add = $('add');
  add.setAttribute('aria-expanded', String(open));
  add.replaceChildren(icon(open ? 'x' : 'plus', open ? 16 : 18));
  const said = open ? 'Close the add menu' : 'Add: upload a file or capture this tab';
  add.setAttribute('aria-label', said);
  add.title = said;
  $('add-menu').hidden = !open;
}
$('add').onclick = () => {
  const open = $('add').getAttribute('aria-expanded') !== 'true';
  setAdd(open);
  if (open) $('attach').focus();
};
$('add-menu').addEventListener('keydown', (event) => {
  if (event.key === 'Escape') {
    closeAdd();
    $('add').focus();
  }
});

function autosize() {
  const prompt = $('prompt');
  prompt.style.height = 'auto';
  prompt.style.height = `${Math.min(prompt.scrollHeight, 160)}px`;
}

/** Asks Chrome for a provider's site, or for every site when the thread's commands
 * reach the web through Chrome (`sites`) (needs the click that called it); when Chrome
 * cannot ask here, whether Spindle already may, offering a tab to ask from. */
function reachable(profile, sites = false) {
  if (sites) return mayReach(PAGE_ACCESS, 'websites');
  // A sign-in also refreshes at its token endpoint.
  const flow = flowOf(profile);
  const origins = flow ? [accessFor(profile), `${new URL(flow.token_url).origin}/*`] : accessFor(profile);
  return mayReach(origins, new URL(profile.base_url).host);
}

/** Asks Chrome to let Spindle reach `pattern` (or patterns; needs the click that called
 * it); when Chrome cannot ask here, offers a tab where it can. Resolves whether it may. */
function mayReach(pattern, host) {
  const origins = [pattern].flat();
  return chrome.permissions.request({ origins }).catch(async (error) => {
    const granted = await chrome.permissions.contains({ origins }).catch(() => false);
    if (!granted) {
      banner(`${BROWSER} could not ask for ${host} here (${error.message}). Open Spindle in a tab and try there.`, [['Open in a tab', openTab]]);
    }
    return granted;
  });
}

function openTab() {
  chrome.tabs.create({ url: chrome.runtime.getURL('panel.html') });
}

// ---- Attachments: files that go with the next message, into the thread's workspace.

/** At most this much, per file and in all (they cross to the engine whole). */
const MAX_ATTACHED = 25 * 1024 * 1024;
const MAX_ATTACHED_ALL = 60 * 1024 * 1024;
let attached = [];

function renderAttached() {
  const box = $('attached');
  box.replaceChildren(...attached.map((file, at) => {
    const chip = el('span', 'chip');
    const drop = el('button');
    drop.type = 'button';
    drop.append(icon('x', 12));
    drop.setAttribute('aria-label', `Leave out ${shownName(file.name)}`);
    drop.title = `Leave out ${shownName(file.name)}`;
    drop.onclick = () => {
      attached = attached.filter((_, index) => index !== at);
      renderAttached();
    };
    chip.append(el('span', '', `${shownName(file.name)} · ${size(file.size)}`), drop);
    return chip;
  }));
  box.hidden = !attached.length;
  renderSend();
}

function attach(files) {
  for (const file of files) {
    // A file's name, as a workspace path of its own (no folders from the outside).
    const name = file.name.replace(/[\\/]/g, '_').replace(/^\.+$/, '_') || 'file';
    if (file.size > MAX_ATTACHED) {
      toast(`${name} is over ${size(MAX_ATTACHED)}; it was left out.`);
      continue;
    }
    const going = attached.filter((kept) => kept.name !== name);
    if (going.reduce((total, kept) => total + kept.size, 0) + file.size > MAX_ATTACHED_ALL) {
      toast(`Attachments come to over ${size(MAX_ATTACHED_ALL)}; ${name} was left out.`);
      continue;
    }
    attached = [...going, { name, size: file.size, file }];
  }
  renderAttached();
}

$('attach').onclick = () => $('attach-file').click();

/** A picture of the tab the user is on (as Chrome draws it), attached to the next
 * message. Chrome asks once to let Spindle picture pages (in the click; at once if
 * already given), which the screenshot tool then uses too. */
$('shoot').onclick = () => {
  const asked = mayReach(PICTURE_ACCESS, 'pictures of pages');
  (async () => {
    if (!(await asked)) throw new Error(`${BROWSER} did not let Spindle picture pages`);
    const window = await chrome.windows.getCurrent();
    const [tab] = await chrome.tabs.query({ active: true, windowId: window.id });
    let url;
    try {
      url = await chrome.tabs.captureVisibleTab(window.id, { format: 'png' });
    } catch (error) {
      throw new Error(`${BROWSER} gives no picture of ${tab?.url ? new URL(tab.url).protocol.replace(':', '') + ' pages' : 'this tab'} (${error.message})`);
    }
    const bytes = new Uint8Array(await (await fetch(url)).arrayBuffer());
    const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\.\d+Z$/, '').replace('T', '-');
    const name = `screenshot-${stamp}.png`;
    attach([new File([bytes], name, { type: 'image/png' })]);
    closeAdd();
    $('prompt').focus();
    toast(`A picture of ${tab?.title ? `“${tab.title}”` : 'the tab'} goes with your message.`);
  })().catch((error) => toast(error.message));
};
$('attach-file').onchange = (event) => {
  attach([...event.target.files]);
  event.target.value = '';
  closeAdd();
  $('prompt').focus();
};
// Files dropped anywhere on the panel are attached.
let dragging = 0;
document.addEventListener('dragenter', (event) => {
  if (!event.dataTransfer?.types.includes('Files')) return;
  dragging += 1;
  document.body.classList.add('dropping');
});
document.addEventListener('dragleave', () => {
  dragging = Math.max(0, dragging - 1);
  if (!dragging) document.body.classList.remove('dropping');
});
document.addEventListener('dragover', (event) => {
  if (event.dataTransfer?.types.includes('Files')) event.preventDefault();
});
document.addEventListener('drop', (event) => {
  if (!event.dataTransfer?.files.length) return;
  event.preventDefault();
  dragging = 0;
  document.body.classList.remove('dropping');
  attach([...event.dataTransfer.files]);
});

let sending = null;

function send() {
  if (sending) return;
  const sendId = {};
  const text = $('prompt').value.trim();
  if (!engine) return;
  if (!text) {
    if (attached.length) toast('Write a message to go with what you attached.');
    return;
  }
  const thread = current ? threads.get(current) : null;
  const { provider, model, effort } = thread ? { provider: thread.provider, model: null, effort: null } : chosen();
  const profile = profileOf(provider);
  const network = thread ? Boolean(thread.network) : Boolean(choice.network);
  const runner = thread ? thread.runner : choice.runner ?? 'app';
  // A thread on the user's host runs on the host's credentials: nothing of this
  // browser's is needed to send to it.
  const hosted = Boolean(thread?.home);
  if (!profile && !hosted) {
    toast(provider ? `${provider} is not in your provider list` : 'Choose a provider first');
    return;
  }
  if (!hosted && needsKey(profile) && !keyed.has(profile.id)) {
    openSettings(flowOf(profile) ? `Sign in to ${profile.display_name} to send.` : `Add your ${profile.display_name} key to send.`);
    return;
  }
  if (!hosted && needsKey(profile) && engine?.vault?.locked) {
    openSettings('Your keys are locked: unlock them to send.');
    return;
  }
  if (!hosted && engine?.vault?.ended?.[profile.id]) {
    openSettings(`Your ${profile.display_name} sign-in has ended: sign in again to send.`);
    return;
  }
  const sites = !hosted && network && runner === 'app' && !engine.relay;
  const target = current;
  const from = carrying?.id;
  const going = attached;
  sending = sendId;
  (hosted ? Promise.resolve(true) : reachable(profile, sites)).then(async (granted) => {
    // Access to sites declined for a thread with network: it still talks to its provider
    // if Spindle may reach that; its commands reach no website until Chrome allows.
    if (!granted && sites && (await chrome.permissions.contains({ origins: [accessFor(profile)] }).catch(() => false))) {
      granted = true;
      toast(`Without ${BROWSER}'s access to sites, this thread's commands reach no website; ${BROWSER} asks again when you send.`);
    }
    if (!granted) {
      toast(`Spindle needs to reach ${new URL(profile.base_url).host} to talk to ${profile.display_name}.`);
      return;
    }
    if (current !== target) return;
    let files;
    try {
      files = await Promise.all(going.map(async (kept) => ({ path: kept.name, bytes: new Uint8Array(await kept.file.arrayBuffer()) })));
    } catch (error) {
      toast(`An attachment could not be read: ${error.message}`);
      return;
    }
    if (current !== target) return;
    // Leave edits made while Chrome asked for permission in the composer.
    const cleared = $('prompt').value.trim() === text && attached === going;
    if (cleared) {
      $('prompt').value = '';
      autosize();
      attached = [];
      renderAttached();
    }
    try {
      if (thread) {
        if (thread.state !== 'running' && current === target) shown.turnFrom = shown.total;
        // The user's own message: the conversation follows on from it (set first: the turn's
        // first events may come before the answer).
        if (current === target) following = true;
        const answer = await ask('send', { id: thread.id, text, ...(files.length && { files }) });
        sending = null;
        if (answer.steered != null) {
          // Shown now; checked once the running turn reads it.
          if (current === target) {
            const node = steerNode(text, false);
            pendingSteers.push(node);
            $('history').append(node);
            scrollDown(true);
          }
          toast('Sent to the running turn.');
        }
        if (answer.queued) toast('It goes when this turn ends: a running turn takes text only, not files.');
      } else {
        const { id } = await ask('create', { provider, model, ...(effort && effort !== 'default' && { effort }), text, runner, network, ...(choice.tasks && { tasks: true }), ...(choice.max_usd != null && { max_usd: choice.max_usd }), ...(choice.compact_tokens !== undefined && { compact_tokens: choice.compact_tokens }), ...(files.length && { files }), ...(from && { from }) });
        sending = null;
        if (current === target) {
          carry(null);
          await openThread(id);
        }
      }
    } catch (error) {
      // Back in the box it left (that thread's, or the start page's), never another's.
      const here = !cleared || backToBox(target, text, going);
      toast(here ? error.message : `${error.message} (the message is back in the box when you ${target ? 'open that thread' : 'go back to the start page'})`);
    }
  }).catch((error) => toast(error.message)).finally(() => { if (sending === sendId) sending = null; });
}

// While the user writes here, tabs Spindle opens open behind the one showing
// (background.js `typing`): Chrome gives the keys to a tab it shows, which would take
// them from this box mid-word. Writing: the box has the focus and holds text, or a key
// went into it in the last few seconds. Told once a telling is half spent, and at once
// when it stops (it lasts past itself, so a panel closed mid-word holds no longer).
const WRITING_MS = 8_000;
const HOLDING_MS = 30_000;
/** When a key last went into the box (or it took the focus), and until when that was told. */
let wrote = 0;
let toldUntil = 0;
function tellWriting(event) {
  const box = $('prompt');
  const now = Date.now();
  if (event?.type === 'input' || event?.type === 'focus') wrote = now;
  const focused = document.hasFocus() && document.activeElement === box;
  const until = !focused ? 0 : box.value.trim() ? now + HOLDING_MS : wrote + WRITING_MS > now ? wrote + WRITING_MS : 0;
  if (until ? toldUntil - now > (until - now) / 2 : toldUntil <= now) {
    // Run out (an empty box no key went into): nothing to tell again.
    if (!until) toldUntil = 0;
    return;
  }
  toldUntil = until;
  chrome.runtime.sendMessage({ type: 'typing', until }).catch(() => {});
}
for (const type of ['input', 'focus', 'blur']) $('prompt').addEventListener(type, tellWriting);
addEventListener('blur', tellWriting);
// A box left holding text, focused, is told again while it stays so.
setInterval(() => { if (toldUntil) tellWriting(); }, 10_000);

$('composer').addEventListener('submit', (event) => {
  event.preventDefault();
  if (['stop', 'wait'].includes($('send').dataset.mode)) stopTurn();
  else send();
});
function stopTurn() {
  if (current) ask('stop', { id: current }).catch((error) => toast(error.message));
}
$('prompt').addEventListener('keydown', (event) => {
  if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
    event.preventDefault();
    send();
  }
});
$('prompt').addEventListener('input', () => {
  autosize();
  renderSend();
});
const again = (op) => () => {
  const id = current;
  if (id) ask(op, { id }).catch((error) => toast(error.message));
};
$('resume').onclick = again('resume');
$('retry').onclick = again('retry');
$('home').onclick = () => (document.body.dataset.view === 'workspace' ? closeWorkspace() : showHome());

// ---- Model -----------------------------------------------------------------------

const EFFORT_NAMES = { default: 'Default', off: 'Off', low: 'Low', medium: 'Med', high: 'High', xhigh: 'X-high', max: 'Max' };

/** The efforts a provider's model takes, as the engine says ({effort, label, short});
 * asked once each (the catalog's change forgets them). */
const effortsAsked = new Map();
const effortsKnown = new Map();
function effortsOf(provider, model) {
  const key = `${provider}\n${model}`;
  if (!effortsAsked.has(key)) {
    effortsAsked.set(key, ask('efforts', { provider, model }).then((efforts) => {
      const fresh = !effortsKnown.has(key);
      effortsKnown.set(key, efforts);
      if (fresh) renderBar();
      return efforts;
    }, () => {
      effortsAsked.delete(key);
      return [];
    }));
  }
  return effortsAsked.get(key);
}
/** What an effort does on a provider's model, if known yet ({label, short}). */
function effortOn(provider, model, effort = 'default') {
  const known = effortsKnown.get(`${provider}\n${model}`);
  if (!known) {
    if (provider && model && engine) effortsOf(provider, model);
    return null;
  }
  return known.find((entry) => entry.effort === effort) ?? null;
}
const sentence = (text) => text.charAt(0).toUpperCase() + text.slice(1);

/** A choice from a list, shown as a field that opens the list in place beneath it (no
 * menu to fall off a narrow panel). The list filters as one types; with `free`, what
 * is typed may be chosen as it is. Arrows, Enter and Escape work it as a select's do.
 * `items()` gives [{value, label, detail, group}]; `chose(value)` hears a choice. */
function picker(host, { label, items, free = false, placeholder, chose }) {
  const field = el('button', 'picker-field');
  field.type = 'button';
  field.setAttribute('aria-haspopup', 'listbox');
  field.setAttribute('aria-expanded', 'false');
  const words = el('span', 'picker-text');
  const main = el('span', 'picker-main');
  const sub = el('span', 'picker-sub');
  words.append(main, sub);
  const ok = el('span', 'picker-ok');
  ok.append(icon('check', 13));
  const chev = el('span', 'picker-chev');
  chev.append(icon('down', 11));
  field.append(words, ok, chev);
  const pop = el('div', 'picker-pop');
  pop.hidden = true;
  const filter = el('input', 'picker-filter');
  Object.assign(filter, { type: 'text', autocomplete: 'off', spellcheck: false, placeholder });
  filter.setAttribute('role', 'combobox');
  filter.setAttribute('aria-label', label);
  const list = el('div', 'picker-list');
  list.id = `${host.id}-list`;
  list.setAttribute('role', 'listbox');
  list.setAttribute('aria-label', label);
  field.setAttribute('aria-controls', list.id);
  filter.setAttribute('aria-controls', list.id);
  pop.append(filter, list);
  // A press in the list (its scrollbar too) leaves the focus in the filter: it stays open.
  pop.addEventListener('mousedown', (event) => {
    if (event.target !== filter) event.preventDefault();
  });
  host.replaceChildren(field, pop);
  let value = null;
  let options = [];
  let active = -1;
  const show = () => {
    const item = items().find((entry) => entry.value === value);
    main.textContent = item?.label ?? value ?? 'Choose…';
    sub.textContent = item?.sub ?? '';
    ok.hidden = !item?.ok;
    field.title = [item?.label ?? value, item?.detail].filter(Boolean).join(' · ');
    field.setAttribute('aria-label', `${label}: ${[item?.label ?? value ?? 'none chosen', item?.detail].filter(Boolean).join(', ')}`);
  };
  const move = (to) => {
    options[active]?.classList.remove('active');
    active = options.length ? (to + options.length) % options.length : -1;
    const option = options[active];
    if (option) {
      option.classList.add('active');
      option.scrollIntoView({ block: 'nearest' });
      filter.setAttribute('aria-activedescendant', option.id);
    } else {
      filter.removeAttribute('aria-activedescendant');
    }
  };
  const render = () => {
    const typed = filter.value.trim();
    const query = typed.toLowerCase();
    const all = items();
    const shown = all.filter((item) => !query || `${item.label} ${item.detail ?? ''}`.toLowerCase().includes(query));
    if (free && typed && !all.some((item) => item.value === typed)) shown.unshift({ value: typed, label: `Use “${typed}”`, detail: 'a name this list does not have' });
    list.replaceChildren();
    options = [];
    let group = null;
    for (const item of shown) {
      if (item.group && item.group !== group) {
        group = item.group;
        list.append(el('div', 'picker-group', group));
      }
      const option = el('div', 'picker-option');
      option.id = `${list.id}-${options.length}`;
      option.setAttribute('role', 'option');
      option.setAttribute('aria-selected', String(item.value === value));
      option.dataset.value = item.value;
      option.append(el('strong', '', item.label));
      if (item.detail) option.append(el('small', '', item.detail));
      option.onclick = () => choose(item.value);
      list.append(option);
      options.push(option);
    }
    if (!options.length) list.append(el('div', 'picker-empty', 'Nothing matches.'));
    move(Math.max(0, options.findIndex((option) => option.dataset.value === value)));
  };
  const open = (typed = '') => {
    pop.hidden = false;
    field.setAttribute('aria-expanded', 'true');
    filter.setAttribute('aria-expanded', 'true');
    filter.value = typed;
    render();
    filter.focus();
  };
  const close = (focus = true) => {
    if (pop.hidden) return;
    pop.hidden = true;
    field.setAttribute('aria-expanded', 'false');
    filter.setAttribute('aria-expanded', 'false');
    if (focus) field.focus();
  };
  const choose = (next) => {
    value = next;
    show();
    close();
    chose?.(next);
  };
  field.onclick = () => (pop.hidden ? open() : close());
  field.onkeydown = (event) => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      open();
    } else if (event.key.length === 1 && event.key !== ' ' && !event.ctrlKey && !event.metaKey && !event.altKey) {
      event.preventDefault();
      open(event.key);
    }
  };
  filter.oninput = render;
  filter.onkeydown = (event) => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      move(active + (event.key === 'ArrowDown' ? 1 : -1));
    } else if (event.key === 'Enter') {
      event.preventDefault();
      if (options[active]) choose(options[active].dataset.value);
    } else if (event.key === 'Escape') {
      // The list closes, not the sheet.
      event.preventDefault();
      event.stopPropagation();
      close();
    }
  };
  host.addEventListener('focusout', (event) => {
    if (!host.contains(event.relatedTarget)) close(false);
  });
  return {
    get value() { return value; },
    set value(next) { value = next; close(false); show(); },
  };
}

const tokenCount = (n) => n >= 1e6 ? `${+(n / 1e6).toFixed(1)}M` : n >= 1e3 ? `${Math.round(n / 1e3)}K` : String(n);
const dollars = (n) => `$${+Number(n).toFixed(n < 1 ? 3 : 2)}`;

/** Providers, those ready to use first (a key saved, or none needed). */
function providerItems() {
  const ready = (profile) => !needsKey(profile) || keyed.has(profile.id);
  const list = providers();
  return [...list.filter(ready), ...list.filter((profile) => !ready(profile))].map((profile) => ({
    value: profile.id,
    label: profile.display_name,
    detail: !needsKey(profile) ? 'no key needed' : keyed.has(profile.id) ? credentialState(profile)
      : flowOf(profile) ? 'needs sign-in: Settings, Providers' : 'needs a key: Settings, Providers',
    group: ready(profile) ? 'Ready' : 'Need a key or sign-in',
    ok: ready(profile) && !engine?.vault?.ended?.[profile.id],
  }));
}

/** A provider's models, each with what the catalog says of it. */
function modelItems(profile) {
  if (!profile) return [];
  return [...new Set([profile.default_model, ...(profile.models ?? [])].filter(Boolean))].map((name) => {
    const spec = profile.model_specs?.[name] ?? {};
    const context = spec.context_window ?? profile.model_context_windows?.[name] ?? profile.context_window;
    const price = spec.pricing;
    const facts = [
      context && tokenCount(context),
      spec.capabilities?.image_input && 'pictures',
      price && price.input_usd_per_mtok != null && `${dollars(price.input_usd_per_mtok)} / ${dollars(price.output_usd_per_mtok)} per M`,
    ].filter(Boolean);
    return {
      value: name,
      label: name,
      sub: facts.join(' · '),
      detail: [name === profile.default_model && 'default', ...facts].filter(Boolean).join(' · '),
    };
  });
}

/** The model page's choices while it is open: what they apply to (`scope`: this thread or
 * new threads) and what is chosen. */
const sheet = { scope: 'new', thread: null, halted: false, effort: 'default', efforts: [], loading: null, runner: 'app', network: false, tasks: false, wasRemote: false, budget: undefined, budgetTyped: false };

const providerPicker = picker($('model-provider'), {
  label: 'Provider',
  placeholder: 'Filter providers',
  items: providerItems,
  chose: () => {
    modelPicker.value = profileOf(providerPicker.value)?.default_model ?? '';
    loadEfforts();
  },
});
const modelPicker = picker($('model-name'), {
  label: 'Model',
  free: true,
  placeholder: 'Filter, or type any model name',
  items: () => modelItems(profileOf(providerPicker.value)),
  chose: () => loadEfforts(),
});

/** The chosen model's efforts, the chosen one kept if it takes it (else its default);
 * and, for Sign in with ChatGPT, that the ChatGPT plan pays (with where to manage it). */
function loadEfforts() {
  const provider = providerPicker.value;
  $('model-plan').hidden = !planSignIn(profileOf(provider));
  const model = modelPicker.value || profileOf(provider)?.default_model;
  const loading = (provider && model ? effortsOf(provider, model) : Promise.resolve([])).then((efforts) => {
    if (sheet.loading !== loading) return;
    sheet.efforts = efforts;
    if (!efforts.some((entry) => entry.effort === sheet.effort)) sheet.effort = 'default';
    renderEffort();
  });
  sheet.loading = loading;
  return loading;
}

/** The efforts as one row of choices, without "default": none chosen is the model's own,
 * which the note names (and a chosen one can go back to). */
function renderEffort() {
  const levels = sheet.efforts.filter((entry) => entry.effort !== 'default');
  $('effort-field').hidden = !levels.length;
  $('model-effort').replaceChildren(...levels.map((entry) => {
    const button = el('button', '', EFFORT_NAMES[entry.effort] ?? entry.effort);
    button.type = 'button';
    button.setAttribute('role', 'radio');
    button.setAttribute('aria-checked', String(entry.effort === sheet.effort));
    button.dataset.effort = entry.effort;
    button.title = sentence(entry.label);
    button.onclick = () => {
      sheet.effort = entry.effort;
      renderEffort();
    };
    return button;
  }));
  const own = sheet.efforts.find((entry) => entry.effort === 'default');
  const picked = sheet.efforts.find((entry) => entry.effort === sheet.effort);
  const note = $('effort-note');
  note.replaceChildren();
  if (sheet.effort === 'default' || !picked) {
    note.textContent = own ? `None chosen: ${own.label}.` : '';
    return;
  }
  note.append(`${sentence(picked.label)}.`);
  if (own) {
    const back = el('button', 'link-like', "Use the model's default");
    back.type = 'button';
    back.title = sentence(own.label);
    back.onclick = () => {
      sheet.effort = 'default';
      renderEffort();
    };
    note.append(' ', back);
  }
}

/** Where commands run and their network, as chosen, and what each means; a choice that
 * cannot be made says why and where to set it up. */
function renderRunner() {
  // A thread already on the host keeps the choice while it is away (it runs there again
  // once it is back); the note says it is not connected either way.
  const connected = Boolean(engine?.host?.connected);
  for (const button of $('model-runner').querySelectorAll('button')) {
    if (!button.querySelector('svg')) button.prepend(icon(button.dataset.icon, 13));
    button.setAttribute('aria-checked', String(button.dataset.value === sheet.runner));
    if (button.dataset.value === 'remote') {
      button.disabled = !connected && !sheet.wasRemote;
      button.title = connected ? 'On your Spindle host, in its sandbox' : 'Your Spindle host is not connected: set it up in Settings';
    }
    // A thread that runs on the user's host runs its commands in the host's sandbox.
    if (sheet.hosted) button.disabled = true;
  }
  const note = $('runner-note');
  if (sheet.hosted) {
    note.textContent = "This thread runs on your Spindle host: its commands run in the host's sandbox until you bring it back to this browser.";
    $('network-note').textContent = sheet.network ? "From your host's sandbox" : 'Commands reach nothing outside';
    $('model-network').disabled = false;
    $('model-network').setAttribute('aria-checked', String(sheet.network));
    $('model-tasks').setAttribute('aria-checked', String(sheet.tasks));
    $('model-note').textContent = sheet.scope === 'thread' && !sheet.halted ? 'The next turn uses it.' : '';
    return;
  }
  note.replaceChildren(document.createTextNode({
    app: 'Linux in this browser: bash, Python, Node, git.',
    remote: "On your Spindle host, in its sandbox.",
    none: 'No commands: the thread talks, and reads and writes its files.',
  }[sheet.runner]));
  if (!connected) {
    const setUp = el('button', 'link-like', 'Set up');
    setUp.type = 'button';
    setUp.onclick = () => {
      $('model-dialog').close();
      openSettings(null, 'host');
    };
    note.append(` Host is not connected · `, setUp);
  }
  const off = sheet.runner === 'none';
  const network = $('model-network');
  network.disabled = off;
  network.setAttribute('aria-checked', String(sheet.network && !off));
  $('network-note').textContent = off ? 'There are no commands to reach anything'
    : !sheet.network ? 'Commands reach nothing outside'
    : sheet.runner === 'remote' ? "From your host's sandbox"
    : engine?.relay ? 'Through your relay: any address' : `Through ${BROWSER}`;
  $('model-tasks').setAttribute('aria-checked', String(sheet.tasks));
  $('tasks-note').textContent = 'Helpers that work beside it and report back';
  $('model-tasks').title = sheet.tasks
    ? 'On: it can start tasks, threads of its own that work beside it and report back (their spend counts against its limit).'
    : 'Off: it does the work itself.';
  $('model-note').textContent = [
    sheet.scope === 'thread' && !sheet.halted ? 'The next turn uses it.' : '',
    sheet.runner === 'app' && sheet.network && !engine?.relay ? `${BROWSER} asks once for access to sites (the same access page reading uses).` : '',
  ].filter(Boolean).join(' ');
}
for (const button of $('model-runner').querySelectorAll('button')) {
  button.onclick = () => {
    sheet.runner = button.dataset.value;
    renderRunner();
  };
}
$('model-network').onclick = () => {
  sheet.network = !sheet.network;
  renderRunner();
};
$('model-tasks').onclick = () => {
  sheet.tasks = !sheet.tasks;
  renderRunner();
};
for (const button of $('model-scope').querySelectorAll('button')) {
  button.onclick = () => {
    if (button.disabled || sheet.scope === button.dataset.scope) return;
    loadSheet(button.dataset.scope);
  };
}

/** The model page, for the open thread (or new threads); `focus`: 'budget' to go to the
 * spend limit. */
function openModel(focus = null) {
  const thread = current ? threads.get(current) : null;
  loadSheet(thread && thread.state !== 'moved' ? 'thread' : 'new');
  $('model-dialog').showModal();
  if (focus === 'budget' && !$('budget-field').hidden) {
    $('model-budget').focus();
    $('budget-field').scrollIntoView({ block: 'center' });
  }
}

/** The page's choices as `scope` (this thread or new threads) has them. */
function loadSheet(scope) {
  const thread = scope === 'thread' && current ? threads.get(current) : null;
  const now = thread ? { provider: thread.provider, model: thread.model, effort: thread.effort } : chosen();
  Object.assign(sheet, {
    scope: thread ? 'thread' : 'new',
    thread: thread?.id ?? null,
    halted: Boolean(thread && HALTED.has(thread.state)),
    effort: now.effort ?? 'default',
    efforts: [],
    runner: (thread ? thread.runner : choice.runner) ?? 'app',
    network: Boolean(thread ? thread.network : choice.network),
    tasks: Boolean(thread ? thread.tasks : choice.tasks),
  });
  if (!['app', 'remote', 'none'].includes(sheet.runner)) sheet.runner = 'app';
  sheet.wasRemote = sheet.runner === 'remote';
  sheet.hosted = Boolean(thread?.home);
  for (const button of $('model-scope').querySelectorAll('button')) {
    button.setAttribute('aria-checked', String(button.dataset.scope === sheet.scope));
    if (button.dataset.scope === 'thread') {
      button.disabled = !(current && threads.has(current) && threads.get(current).state !== 'moved');
      button.title = button.disabled ? 'Open a thread to change it' : threads.get(current)?.title ?? '';
    }
  }
  $('model-label').hidden = !thread;
  $('model-label').textContent = thread?.title ?? '';
  $('model-halted').hidden = !sheet.halted;
  $('model-apply').textContent = sheet.halted ? 'Go on with this' : thread ? 'Save for this thread' : 'Save for new threads';
  providerPicker.value = now.provider ?? null;
  modelPicker.value = now.model ?? profileOf(now.provider)?.default_model ?? '';
  renderEffort();
  loadEfforts();
  renderRunner();
  renderBudget(thread);
}

/** The spending limit (dollars, the thread's tasks included) and, for a thread, what it
 * spent, as the engine counts it (the same ledger a Spindle host keeps). Until the engine
 * says (`sheet.budget` undefined), only a limit typed in changes it. */
function renderBudget(thread) {
  sheet.budget = undefined;
  sheet.budgetTyped = false;
  $('model-budget').value = '';
  // Where history compacts (thousands of tokens shown; null: the window alone). A new
  // thread's default is the engine's (256K) until the user sets one.
  sheet.compact = undefined;
  sheet.compactTyped = false;
  const shownCompact = (tokens) => (tokens ? String(Math.round(tokens / 1000)) : '');
  $('model-compact').value = '';
  if (!thread) {
    sheet.compact = choice.compact_tokens === undefined ? 256_000 : choice.compact_tokens;
    $('model-compact').value = shownCompact(sheet.compact);
  } else {
    ask('spend', { id: thread.id }).then((spend) => {
      if (sheet.thread !== thread.id) return;
      sheet.compact = spend.compact_tokens ?? null;
      if (!sheet.compactTyped) $('model-compact').value = shownCompact(sheet.compact);
    }).catch(() => {});
  }
  $('budget-note').textContent = 'Per thread, tasks included';
  if (!thread) {
    sheet.budget = choice.max_usd ?? null;
    $('model-budget').value = choice.max_usd ?? '';
    return;
  }
  ask('spend', { id: thread.id }).then((spend) => {
    if (sheet.thread !== thread.id) return;
    sheet.budget = spend.max_usd ?? null;
    // What the user typed meanwhile stays.
    if (!sheet.budgetTyped) $('model-budget').value = spend.max_usd ?? '';
    const others = [spend.subscription && `${spend.subscription} by subscription`, spend.unpriced && `${spend.unpriced} with no known price`, spend.unknown && `${spend.unknown} whose outcome is unknown`].filter(Boolean);
    $('budget-note').textContent = `Per thread, tasks included · ${money(spend.usd)} spent`;
    $('budget-note').title = `Spent so far: US$${spend.usd.toFixed(4)} over ${spend.calls} model ${spend.calls === 1 ? 'call' : 'calls'}${others.length ? ` (${others.join(', ')})` : ''}. With a limit, a request that could pass it is not sent.`;
  }).catch(() => {});
}

$('model-chip').onclick = () => openModel();
$('model-usage').onclick = openUsage;
$('model-budget').oninput = () => { sheet.budgetTyped = true; };
$('model-compact').oninput = () => { sheet.compactTyped = true; };
$('model-apply').onclick = () => {
  const provider = providerPicker.value;
  const profile = profileOf(provider);
  if (!profile) return;
  const model = String(modelPicker.value ?? '').trim() || profile.default_model;
  const runner = sheet.runner;
  // (A thread on the user's host has its network there, whatever runner it names.)
  const network = (sheet.hosted || runner !== 'none') && sheet.network;
  const thread = sheet.thread ? threads.get(sheet.thread) : null;
  const halted = sheet.halted;
  // A spending limit is dollars, 0 or more (empty: none); anything else changes nothing
  // (what a number field cannot read shows as empty: it is not taken for none).
  const typed = $('model-budget').value.trim();
  const budget = typed === '' ? null : Number(typed);
  if ($('model-budget').validity.badInput || (budget != null && !(Number.isFinite(budget) && budget >= 0))) {
    toast('A spending limit is a number of US dollars, 0 or more (empty for none).');
    return;
  }
  // Where history compacts: thousands of tokens, 16 or more (empty: the window alone).
  const compactTyped = $('model-compact').value.trim();
  const compactAt = compactTyped === '' ? null : Math.round(Number(compactTyped) * 1000);
  if ($('model-compact').validity.badInput || (compactAt != null && !(Number.isFinite(compactAt) && compactAt >= 16_000 && compactAt <= 100_000_000))) {
    toast('Compact history past takes thousands of tokens, from 16 to 100,000 (empty for the model\'s whole window).');
    return;
  }
  // Asked now, in the click: a thread's next turn uses it, and network through Chrome
  // needs Chrome's access to sites (a new thread asks again when it is sent).
  const sites = network && runner === 'app' && !engine?.relay;
  // A thread on the user's host reaches its model from there: nothing to ask here.
  const granted = thread?.home ? Promise.resolve(true) : thread || sites ? reachable(profile, sites) : Promise.resolve(true);
  $('model-dialog').close();
  (async () => {
    // The effort the model takes (its list may still have been on its way).
    await sheet.loading;
    const effort = sheet.efforts.some((entry) => entry.effort === sheet.effort) ? sheet.effort : 'default';
    const what = `${profile.display_name} · ${model}${effortOn(provider, model, effort) ? `, ${effortOn(provider, model, effort).short}` : ''}`;
    if (!thread) {
      choice = { provider, model, effort, runner, network, tasks: sheet.tasks, max_usd: budget, compact_tokens: compactAt };
      try { localStorage.setItem('spindle-new', JSON.stringify(choice)); } catch {}
      renderBar();
      if (!(await granted)) toast(`Without ${BROWSER}'s access to sites, new threads' commands reach no website; ${BROWSER} asks again when you send.`);
      else if (current) toast(`New threads start with ${what}.`);
      return;
    }
    // Where commands run, and their network, change only when the user changed them.
    const moved = runner !== (thread.runner ?? 'none');
    const netChanged = network !== Boolean(thread.network);
    const where = { app: "commands in this browser's Linux", remote: 'commands on your Spindle host', none: 'no commands' }[runner];
    const given = await granted;
    // Chrome declined: everything else changes; the network stays as it was.
    const refused = sites && netChanged && !given;
    const networkChange = netChanged && !refused;
    // The spending limit, when the user changed it.
    const budgetChanged = sheet.budget === undefined ? budget != null : budget !== sheet.budget;
    const tasksChanged = sheet.tasks !== Boolean(thread.tasks);
    const compactChanged = sheet.compact === undefined ? compactAt != null : compactAt !== sheet.compact;
    const changes = { provider, model, effort, ...(moved && { runner }), ...(networkChange && { network }), ...(budgetChanged && { max_usd: budget }), ...(compactChanged && { compact_tokens: compactAt }), ...(tasksChanged && { tasks: sheet.tasks }) };
    const limit = `${budgetChanged ? (budget == null ? ', no spending limit' : `, a US$${budget} spending limit`) : ''}${compactChanged ? (compactAt ? `, history compacting past ${compactAt / 1000}K tokens` : ', history compacting at the model\'s window') : ''}`;
    const also = `${moved ? `, ${where}` : ''}${networkChange ? (network ? ', commands with network' : ', commands without network') : ''}${tasksChanged ? (sheet.tasks ? ', tasks on' : ', tasks off') : ''}${limit}.${refused ? " ${BROWSER} did not give access to sites, so the thread's network stays off." : ''}`;
    if (halted) {
      const { from } = await ask('restart', { id: thread.id, changes });
      // The conversation as it is now: from the stopped request, or the turn started over.
      if (current === thread.id) await openThread(thread.id);
      toast(from === 'request' ? `The turn goes on with ${what}${also} Its work so far stays; the request that stopped goes again.`
        : from === 'start' ? `The thread starts afresh with ${what}${also} The thread as it was is kept as an earlier version (Settings › Storage).`
        : `The turn starts over with ${what}${also} Your earlier turns stay.`);
    } else {
      await ask('settings', { id: thread.id, changes });
      toast(`The next turn uses ${what}${also}`);
    }
    if (budgetChanged && current === thread.id) loadSpend(thread.id);
  })().catch((error) => toast(error.message));
};

$('takeback').onclick = async () => {
  const id = current;
  if (!id) return;
  try {
    const { text, files } = await ask('restart', { id, resend: false });
    if (current === id) await openThread(id);
    // Into the box only if this thread is still the one open (else kept for it, for when
    // it is): never into another's, whatever was opened meanwhile.
    const here = backToBox(id, text, asAttached(files));
    toast(here ? 'The unfinished turn was taken back: its message is in the box below, to change or send again.'
      : `The unfinished turn of ${named(id, 'that thread')} was taken back: its message is in the box when you open it.`);
  } catch (error) {
    toast(error.message);
  }
};

// ---- Settings --------------------------------------------------------------------

/** Which settings sections are open, as the user left them (this browser only). */
const FOLDS = 'spindle-folds';
function folds() {
  try {
    return JSON.parse(localStorage.getItem(FOLDS) || '{}');
  } catch {
    return {};
  }
}
/** Set while the code opens folds (their toggles are not the user's to remember). */
let folding = false;
for (const fold of document.querySelectorAll('details[data-fold]')) {
  fold.addEventListener('toggle', () => {
    if (folding) return;
    try {
      localStorage.setItem(FOLDS, JSON.stringify({ ...folds(), [fold.dataset.fold]: fold.open }));
    } catch {}
  });
}

async function openSettings(note, fold = null) {
  $('settings-note').hidden = !note;
  $('settings-note').textContent = note ?? '';
  if (!$('settings-dialog').open) {
    // As left; Providers open while no key is saved, or when the note is about one.
    const kept = folds();
    folding = true;
    for (const one of document.querySelectorAll('details[data-fold]')) {
      one.open = kept[one.dataset.fold] ?? (one.dataset.fold === 'providers' && !keyed.size);
    }
    if (note) document.querySelector('[data-fold="providers"]').open = true;
    // Locked keys: the passphrase row is open to unlock them.
    if (engine?.vault?.locked) document.querySelector('[data-fold="vault"]').open = true;
    // Toggle events come after this task: remembered toggles start after them.
    setTimeout(() => { folding = false; });
  }
  renderProviders();
  $('catalog-state').textContent = engine?.catalogSaved
    ? 'Using your providers.json.'
    : "Using Spindle's built-in providers (Dext's catalog). Import your ~/.dext/providers.json to use yours.";
  $('catalog-summary').textContent = engine?.catalogSaved ? 'Your providers.json' : 'Built in';
  renderCommands();
  if (document.activeElement !== $('standing')) $('standing').value = engine?.standing ?? '';
  renderInstructionsSummary();
  await renderPages();
  renderTheme();
  await renderAbout();
  // Drawn as it comes: the page opens without waiting for it.
  renderStorage().catch(() => {});
  if (!$('settings-dialog').open) $('settings-dialog').showModal();
  if (fold) {
    const wanted = document.querySelector(`[data-fold="${fold}"]`);
    if (wanted) {
      wanted.open = true;
      wanted.scrollIntoView({ block: 'start' });
      wanted.querySelector('summary')?.focus();
    }
  }
}

/** Your instructions' row: Add (none yet) or Set. */
function renderInstructionsSummary() {
  const summary = $('instructions-summary');
  summary.textContent = engine?.standing ? 'Set' : 'Add';
  summary.classList.toggle('add', !engine?.standing);
}

/** Providers, one row each: a filled square (ready) or a hollow one (no key), its name and
 * site (or sign-in), and icons to change, add, sign in again or sign out; a key is typed
 * in a row that opens beneath. */
function renderProviders() {
  const box = $('providers');
  box.replaceChildren();
  const saved = providers().filter((profile) => keyed.has(profile.id));
  $('providers-summary').textContent = engine?.vault?.locked ? 'Locked'
    : saved.length ? `${saved.length} ready` : 'None set up yet';
  for (const profile of providers()) {
    const row = el('li', 'provider');
    row.dataset.provider = profile.id;
    const has = keyed.has(profile.id);
    const flow = flowOf(profile);
    const signedIn = engine?.vault?.kinds?.[profile.id] === 'signin';
    const ended = signedIn && Boolean(engine.vault.ended?.[profile.id]);
    row.classList.toggle('ready', has && !ended);
    row.classList.toggle('ended', ended);
    const mark = el('span', 'provider-mark');
    mark.setAttribute('aria-hidden', 'true');
    const name = el('span', 'provider-label', profile.display_name);
    const host = new URL(profile.base_url).host;
    const account = signedIn ? engine.vault.accounts?.[profile.id] : null;
    const detail = account ? el('span', 'provider-detail provider-account', account)
      : el('span', 'provider-detail', ended ? 'sign-in ended' : signedIn ? 'signed in' : !needsKey(profile) ? `${host} · no key` : host);
    detail.title = [host, account].filter(Boolean).join(' · ');
    const state = el('span', 'provider-state',
      ended ? 'sign-in ended' : has ? (signedIn ? 'signed in' : 'key saved')
        : !needsKey(profile) ? 'no key needed' : flow ? 'not signed in' : 'no key');
    const actions = el('span', 'provider-actions');
    row.append(mark, name, detail, state, actions);
    if (flow) {
      // A browser sign-in: the provider's own page in a tab; no key to paste. Sign in
      // with ChatGPT says so as OpenAI asks ("Continue with ChatGPT"), links to where the
      // plan's use is managed and, signed out, can take another account (it registers
      // anew; signed in, its row keeps to three actions).
      const plan = planSignIn(profile);
      const words = plan ? 'Continue with ChatGPT' : has ? `Sign in to ${profile.display_name} again` : `Sign in to ${profile.display_name}`;
      const signIn = iconButton(has ? 'retry' : 'signin', words, null, `icon-button${has && !ended ? '' : ' add'}`);
      signIn.dataset.act = 'signin';
      signIn.disabled = signingIn.has(profile.id);
      signIn.onclick = () => signInTo(profile, signIn);
      actions.append(signIn);
      if (plan && !has && engine?.registrations?.[profile.id] !== undefined) {
        const another = iconButton('plus', 'Use another ChatGPT account', null, 'icon-button');
        another.dataset.act = 'another';
        another.disabled = signingIn.has(profile.id);
        another.onclick = () => signInTo(profile, another, { fresh: true });
        actions.append(another);
      }
      if (plan && has) {
        const usage = iconButton('external', 'Manage usage: what Spindle uses of your ChatGPT plan, in ChatGPT', null, 'icon-button');
        usage.dataset.act = 'usage';
        usage.onclick = openUsage;
        actions.append(usage);
      }
      if (has) actions.append(forgetButton(profile, `Sign out of ${profile.display_name}`, `Signed out of ${profile.display_name}.`, 'signout'));
    } else if (needsKey(profile) || has) {
      const toggle = iconButton(has ? 'pencil' : 'plus', has ? `Change ${profile.display_name} key` : `Add ${profile.display_name} key`, null, `icon-button${has ? '' : ' add'}`);
      toggle.dataset.act = 'key';
      toggle.setAttribute('aria-expanded', 'false');
      actions.append(toggle);
      const form = el('form', 'provider-key');
      form.hidden = true;
      const input = el('input');
      input.type = 'password';
      input.autocomplete = 'off';
      input.placeholder = `${profile.display_name} key`;
      input.setAttribute('aria-label', `${profile.display_name} key`);
      const save = el('button', 'primary', 'Save');
      const forget = forgetButton(profile, 'Remove', `${profile.display_name} key removed.`);
      forget.hidden = !has;
      form.append(input, save, forget);
      row.append(form);
      toggle.onclick = () => {
        form.hidden = !form.hidden;
        toggle.setAttribute('aria-expanded', String(!form.hidden));
        if (!form.hidden) input.focus();
      };
      form.onsubmit = (event) => {
        event.preventDefault();
        const key = input.value.trim();
        if (!key) return;
        const granted = reachable(profile);
        ask('saveKey', { provider: profile.id, key }).then(async (snapshot) => {
          input.value = '';
          apply(snapshot);
          if (!(await granted)) toast(`Key saved. Spindle still needs to reach ${host}; it asks again when you send.`);
          else toast(`${profile.display_name} key saved.`);
          renderProviders();
        }, (error) => toast(`The key was not saved: ${error.message}`));
      };
    }
    box.append(row);
  }
  renderVault();
}

/** Forgets a provider's key or sign-in: a button with `label` (an icon, given `name`). */
function forgetButton(profile, label, done, name = null) {
  const button = name ? iconButton(name, label, null, 'icon-button out') : el('button', 'danger', label);
  button.type = 'button';
  button.onclick = async () => {
    try {
      const after = await ask('forget', { provider: profile.id });
      apply(after);
      toast(after.unconfirmed ? `${done} OpenAI did not confirm the session ended: to be sure, disconnect Spindle in ChatGPT's settings.` : done);
    } catch (error) {
      toast(error.message);
    }
    renderProviders();
  };
  return button;
}

/** A browser sign-in: Chrome's leave to reach the provider (asked first, in the click),
 * the provider's page in a tab, then the engine keeps what it gives. `fresh`: Sign in
 * with ChatGPT for another account (registered anew, not signed in again). */
async function signInTo(profile, button, { fresh = false } = {}) {
  if (engine?.vault?.locked) {
    toast('Your keys are locked: unlock them below, then sign in.');
    return;
  }
  if (signingIn.has(profile.id)) return;
  // Asked in the click itself (Chrome needs the gesture); one sign-in at a time.
  const granted = mayReach(signInSites(profile), new URL(profile.base_url).host);
  signingIn.add(profile.id);
  button.disabled = true;
  if (!(await granted)) {
    signingIn.delete(profile.id);
    button.disabled = false;
    toast(`Spindle needs to reach ${profile.display_name}'s sites to sign in.`);
    return;
  }
  try {
    const plan = planSignIn(profile);
    const start = await ask('signInStart', { provider: profile.id, fresh });
    toast(plan ? 'Continue with ChatGPT in the tab that opened.' : `Sign in to ${profile.display_name} in the tab that opened.`);
    const caught = await signInInTab(profile, start);
    const after = await ask('signIn', { provider: profile.id, ...caught });
    apply(after);
    toast(plan ? `Signed in with ChatGPT${engine?.vault?.accounts?.[profile.id] ? ` as ${engine.vault.accounts[profile.id]}` : ''}.` : `Signed in to ${profile.display_name}.`);
    if (after.planWelcome) planWelcome();
  } catch (error) {
    // Declined on OpenAI's page: the next sign-in asks for the plan permission again.
    if (error.kind === 'declined' && planSignIn(profile)) ask('signInDeclined', { provider: profile.id }).catch(() => {});
    if (error.kind !== 'cancelled') toast(`Not signed in: ${error.message}`);
  } finally {
    signingIn.delete(profile.id);
    button.disabled = false;
    renderProviders();
  }
}

/** Key protection: keys are always sealed in this browser; a passphrase makes a copy of
 * the Chrome profile useless without it. One row (Off, On or Locked) that opens to set,
 * unlock, lock, change or remove it. */
function renderVault() {
  const box = $('vault');
  if (!box) return;
  box.replaceChildren();
  const state = engine?.vault;
  $('vault-summary').textContent = !state ? '' : state.mode === 'device' ? 'Off' : state.locked ? 'Locked' : 'On';
  if (!state) return;
  const field = (label) => {
    const input = el('input');
    input.type = 'password';
    input.autocomplete = 'off';
    input.placeholder = label;
    input.setAttribute('aria-label', label);
    return input;
  };
  const action = (label, kind, run) => {
    const button = el('button', kind, label);
    button.type = 'button';
    button.dataset.vault = label.split(' ')[0].toLowerCase();
    button.onclick = async () => {
      button.disabled = true;
      try {
        await run();
      } catch (error) {
        toast(error.message);
      } finally {
        button.disabled = false;
        renderProviders();
      }
    };
    return button;
  };
  // Fields side by side with their button, in one row.
  const form = (inputs, buttons) => {
    const row = el('div', 'vault-form vault-row');
    row.append(...inputs, ...buttons);
    return row;
  };
  if (state.mode === 'device') {
    box.append(el('p', 'note', `Keys are encrypted in this browser. A passphrase keeps a copy of your ${BROWSER} profile from reading them; you unlock them once each time Spindle starts.`));
    const one = field('New passphrase');
    const two = field('Same passphrase again');
    box.append(form([one, two], [action('Set passphrase', 'primary', async () => {
      if (one.value !== two.value) throw new Error('the two passphrases differ');
      apply(await ask('protect', { passphrase: one.value }, 120_000));
      toast('Keys are locked with your passphrase.');
    })]));
    return;
  }
  if (state.locked) {
    box.append(el('p', 'note', 'Your keys are locked with your passphrase.'));
    const pass = field('Passphrase');
    box.append(form([pass], [
      action('Unlock', 'primary', async () => {
        apply(await ask('unlock', { passphrase: pass.value }, 120_000));
        toast('Keys unlocked.');
      }),
    ]));
    const forgot = action('Forgot it', 'link-like', async () => {
      const sure = await confirmed('Forget every saved key?',
        'Without the passphrase they cannot be opened. Every saved key, sign-in and site secret header is deleted, and so is your Spindle host\'s token, so the host disconnects (threads stay). You add them again, and connect the host again.',
        'Delete keys');
      if (!sure) return;
      apply(await ask('resetVault'));
      toast('Saved keys, sign-ins, secret headers and the host\'s token deleted. Add them again to send, and connect your host again.');
    });
    const line = el('p', 'note');
    line.append(forgot, ': delete every saved key and add them again');
    box.append(line);
    return;
  }
  const line = el('div', 'vault-row');
  line.append(el('p', 'note', 'Unlocked until Spindle restarts.'), action('Lock now', '', async () => {
    apply(await ask('lock'));
    toast('Keys locked.');
  }));
  box.append(line);
  // Changing or removing it is rare: folded away.
  const more = el('details', 'vault-more');
  more.append(el('summary', '', 'Change or remove the passphrase'));
  const now = field('Current passphrase');
  const next = field('New passphrase (to change it)');
  const again = field('New passphrase again');
  const fields = el('div', 'vault-form');
  fields.append(now, next, again);
  const buttons = el('div', 'confirm-actions');
  buttons.append(
    action('Remove', 'danger', async () => {
      apply(await ask('unprotect', { passphrase: now.value }, 120_000));
      toast('Keys no longer need a passphrase.');
    }),
    action('Change', 'primary', async () => {
      if (next.value !== again.value) throw new Error('the two new passphrases differ');
      apply(await ask('protect', { passphrase: next.value, previous: now.value }, 120_000));
      toast('Passphrase changed.');
    }),
  );
  more.append(fields, buttons);
  box.append(more);
}

const LINUX_STATES = {
  'not started': 'not started yet (it starts with the first command)',
  booting: 'starting for the first time…',
  saving: 'saving its first start…',
  resuming: 'resuming…',
  ready: 'running',
  stopped: 'resting (resumes in a moment with the next command)',
  failed: 'failed to start',
};
const WASM_STATES = {
  'not started': 'not started yet (it starts with the first command it can run)',
  booting: 'starting…',
  ready: 'running',
  ended: 'ended (the next command starts another)',
  stopped: 'resting (starts in a moment with the next command)',
  failed: 'failed to start (the next command tries again; until then, commands go to v86)',
  unavailable: 'unavailable here: commands go to v86',
};

function renderCommands() {
  const linux = engine?.linux ?? { state: 'not started' };
  $('linux-state').textContent = `${LINUX_STATES[linux.state] ?? linux.state}${linux.detail ? `: ${linux.detail}` : ''}`;
  $('wasm-linux-row').hidden = !linux.wasm;
  if (linux.wasm) $('wasm-linux-state').textContent = `${WASM_STATES[linux.wasm.state] ?? linux.wasm.state}${linux.wasm.detail ? `: ${linux.wasm.detail}` : ''}`;
  const relay = engine?.relay ?? null;
  const relayHost = relay ? new URL(relay).host : null;
  $('network-state').textContent = relay
    ? `through your relay at ${relayHost}: any address, for threads with network on`
    : `through ${BROWSER}: websites over HTTP and HTTPS, for threads with network on`;
  $('commands-summary').textContent = `Linux ${linux.state === 'ready' ? 'running' : linux.state}${relay ? ' · relay' : ''}`;
  $('relay-summary').textContent = relayHost ?? 'None';
  if (document.activeElement !== $('relay-url')) $('relay-url').value = relay ?? '';
  $('relay-off').hidden = !relay;
  const host = engine?.host ?? {};
  if (host.url && document.activeElement !== $('host-url')) $('host-url').value = host.url;
  const reaches = { browser: 'threads and commands', runner: 'commands only: press Connect to run threads there too', full: 'an API token' }[host.kind];
  $('host-state').textContent = host.connected
    ? `Connected to ${host.url}${host.client ? ` as “${host.client}”` : ''}${reaches ? ` (${reaches})` : ''}.${host.error ? ` ${sentence(host.error)}.` : ''}`
    : host.error ? `Not connected: ${host.error}` : 'Not connected.';
  $('host-summary').textContent = host.connected ? 'Connected' : host.url ? 'Not connected' : 'None';
  $('host-disconnect').hidden = !host.url;
  renderSites();
}

$('relay-form').onsubmit = async (event) => {
  event.preventDefault();
  const url = $('relay-url').value.trim();
  if (!url) return;
  try {
    apply(await ask('relay', { url }));
    toast(`Threads with network reach the internet through ${new URL(url).host} from their next command.`);
  } catch (error) {
    toast(error.message);
  }
  renderCommands();
};
$('relay-off').onclick = async () => {
  try {
    apply(await ask('relay', { url: null }));
    toast(`Threads with network reach websites through ${BROWSER} again.`);
  } catch (error) {
    toast(error.message);
  }
  renderCommands();
};

/** Whether `grant`'s secret header is gone (its sealed record went with a vault reset:
 * its requests are refused until it is added again with one). */
function secretLost(grant) {
  const vaulted = engine?.vault;
  return Boolean(grant?.secret?.length && vaulted && !vaulted.locked && !vaulted.error && !(vaulted.sites ?? []).includes(grant.name));
}

function renderSites() {
  const box = $('sites');
  box.replaceChildren();
  const sites = engine?.sites ?? [];
  for (const grant of sites) {
    const row = el('div', 'site');
    const host = new URL(grant.url).host;
    const main = el('span', 'site-main');
    const name = el('span', 'site-host', grant.url);
    name.title = grant.url;
    main.append(name);
    const drop = iconButton('x', `Stop scripts calling ${host}`, () => saveSites({ remove: grant.name }, `Scripts no longer reach ${host}.`));
    row.append(icon('globe', 14), main, el('span', 'site-kind', grant.risk === 'write' ? 'Read and change' : 'Read'));
    const secret = grant.secret?.length ? grant.secret.join(', ') : null;
    if (secret) {
      const lost = secretLost(grant);
      const mark = el('span', `site-secret${lost ? ' lost' : ''}`);
      mark.append(icon(lost ? 'warning' : 'lock', 12), el('span', 'site-secret-text', lost ? `${secret} header gone` : `${secret} header added`));
      mark.title = lost
        ? `Its secret ${secret} header is gone (Spindle's keys were reset): add the site again with it`
        : `Spindle adds a secret ${secret} header to each request here (sealed with your keys)`;
      main.append(mark);
    }
    row.append(drop);
    box.append(row);
  }
  $('sites-summary').textContent = sites.length ? String(sites.length) : 'None';
}

async function saveSites(change, done) {
  try {
    apply(await ask('sites', change));
    toast(done);
    return true;
  } catch (error) {
    toast(error.message);
    return false;
  } finally {
    renderSites();
  }
}

$('site-form').onsubmit = (event) => {
  event.preventDefault();
  let url;
  try {
    url = new URL($('site-url').value.trim());
  } catch {
    toast('That address is not a URL.');
    return;
  }
  if (url.protocol !== 'https:' && url.protocol !== 'http:') {
    toast('A site is an https:// (or http://) address.');
    return;
  }
  if (url.search || url.username || url.password) {
    toast('A site is an address prefix, without ?query or a user name.');
    return;
  }
  url.hash = '';
  const write = $('site-access-kind').querySelector('[aria-checked="true"]')?.dataset.value === 'write';
  const value = $('site-secret').value.trim();
  const named = $('site-header').value.trim();
  if (named && !value) {
    toast("Type the secret header's value too, or leave both empty.");
    return;
  }
  const header = value ? { name: named || 'Authorization', value } : null;
  // As the engine refuses it (asked here first, so Chrome is not asked for nothing).
  const loopback = ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname) || url.hostname.endsWith('.localhost');
  if (header && url.protocol !== 'https:' && !loopback) {
    toast(`A secret header goes only to an https:// address, so no one on the network can read it: use https://${url.host}${url.pathname}`);
    return;
  }
  const listed = (engine?.sites ?? []).find((grant) => grant.url === url.href);
  let kept = '';
  if (listed && header) kept = ', with its new secret header';
  else if (secretLost(listed)) kept = '; its secret header is still gone: type its value to add it back';
  else if (listed?.secret?.length) kept = ', its secret header kept';
  const done = listed ? `${url.host} updated${kept}.` : `Scripts may reach ${url.host}${header ? ', with a secret header' : ''}.`;
  mayReach(`${url.origin}/*`, url.host).then(async (granted) => {
    if (!granted) {
      toast(`${BROWSER} did not let Spindle reach ${url.host}.`);
      return;
    }
    if (await saveSites({ add: { url: url.href, write, ...(header && { header }) } }, done)) {
      for (const id of ['site-url', 'site-header', 'site-secret']) $(id).value = '';
    }
  });
};

for (const button of $('site-access-kind').querySelectorAll('button')) {
  button.onclick = () => {
    for (const other of $('site-access-kind').querySelectorAll('button')) other.setAttribute('aria-checked', String(other === button));
    $('site-access-note').textContent = button.dataset.value === 'write' ? 'GET, HEAD, POST, PUT, PATCH and DELETE requests.' : 'GET and HEAD requests only.';
  };
}

function onLinux(state) {
  if (engine) engine.linux = { state: state.state, detail: state.detail, wasm: state.wasm ?? null };
  if ($('settings-dialog').open) renderCommands();
  const thread = current ? threads.get(current) : null;
  if (thread?.state === 'running' && ['booting', 'saving', 'resuming'].includes(state.state)) {
    phase(state.state === 'resuming' ? 'Resuming the Linux…' : `Starting the Linux${state.detail ? ` (${state.detail})` : ''}…`, 'busy');
  }
}

/** The computer this browser runs on, in a word (a name for it on the user's host). */
function platformName() {
  const said = navigator.userAgentData?.platform || navigator.platform || '';
  if (/win/i.test(said)) return 'Windows';
  if (/mac/i.test(said)) return 'macOS';
  if (/cros|chrome ?os/i.test(said)) return 'ChromeOS';
  if (/linux/i.test(said)) return 'Linux';
  return 'this computer';
}

/** Connect: the host's own page, in a tab, gives this browser a token of its own (a
 * browser token, named for this browser and kept for this profile, so connecting again
 * keeps its threads); the engine trades and keeps it. */
$('host-form').onsubmit = (event) => {
  event.preventDefault();
  // The engine seals the token it trades for: with the keys locked it would refuse it
  // after the user approved on the host, so the host is not asked at all until then.
  if (engine?.vault?.locked) {
    toast('Unlock Spindle\'s keys first (Settings, Providers, Passphrase), then connect.');
    document.querySelector('[data-fold="vault"]').open = true;
    return;
  }
  const url = $('host-url').value.trim().replace(/\/+$/, '');
  let origin;
  try {
    origin = new URL(url).origin;
  } catch {
    toast('That address is not a URL.');
    return;
  }
  // Asked in the click itself (Chrome needs the gesture): the host's site, whose pages
  // the tab opens and whose address the connect is caught at.
  const granted = mayReach(`${origin}/*`, new URL(url).host);
  const button = $('host-connect');
  (async () => {
    if (!(await granted)) {
      toast(`Spindle needs to reach ${origin} to use your host.`);
      return;
    }
    button.disabled = true;
    try {
      const { name } = await ask('hostName', { base: `${BROWSER} on ${platformName()}` });
      toast(`Approve “${name}” on your host's page, in the tab that opened.`, { ms: 15_000 });
      const { code, verifier } = await connectInTab(url, name);
      apply(await ask('hostConnect', { url, code, verifier, client: name }));
      toast(`Connected to ${url}.`);
    } catch (error) {
      if (error.kind !== 'cancelled') toast(`Not connected: ${error.message}`);
    } finally {
      button.disabled = false;
      renderCommands();
    }
  })();
};

/** Or a token made on the host and pasted (a browser token, or a runner token for
 * commands only). */
$('host-token-form').onsubmit = (event) => {
  event.preventDefault();
  const url = $('host-url').value.trim();
  const token = $('host-token').value.trim();
  let origin;
  try {
    origin = new URL(url).origin;
  } catch {
    toast('That address is not a URL.');
    return;
  }
  if (!token) {
    toast('Paste a token from your host (spindle token new NAME --browser, or --runner).');
    return;
  }
  mayReach(`${origin}/*`, new URL(url).host)
    .then(async (granted) => {
      if (!granted) {
        toast(`Spindle needs to reach ${origin} to use your host.`);
        return;
      }
      try {
        apply(await ask('host', { host: { url, token } }));
        $('host-token').value = '';
        toast(`Connected to ${url}.`);
      } catch (error) {
        toast(error.message);
      }
      renderCommands();
    });
};
$('host-disconnect').onclick = async () => {
  try {
    apply(await ask('host', { host: null }));
    toast('Host disconnected.');
  } catch (error) {
    toast(error.message);
  }
  renderCommands();
};

$('standing-form').onsubmit = async (event) => {
  event.preventDefault();
  try {
    apply(await ask('standing', { text: $('standing').value }));
    renderInstructionsSummary();
    toast(engine.standing ? 'Every thread keeps to your instructions from its next message.' : 'Your instructions are cleared.');
  } catch (error) {
    toast(error.message);
  }
};

/** The Pages switches: Spindle's own (kept in its store), over Chrome's access to sites,
 * which page reading asks for and which the Linux's network through Chrome uses too. */
async function renderPages() {
  const access = await chrome.permissions.contains({ origins: PAGE_ACCESS });
  const kept = await read('kv', 'pages').catch(() => null);
  const now = pageSwitches(kept, access);
  // The switches change together; what Before acting says is kept with them. Written
  // over the record as it is now (the service worker, or another panel, may have changed
  // it since this was drawn), never over what this drawing read.
  const set = async (next) => {
    const fresh = await read('kv', 'pages').catch(() => null);
    await write('kv', 'pages', pagesRecord(fresh, next));
    // The service worker lets go of tabs when actions go off.
    chrome.runtime.sendMessage({ type: 'pages' }).catch(() => {});
    renderPages();
  };
  const reading = $('page-reading');
  const acting = $('page-actions');
  reading.setAttribute('aria-checked', String(now.reading));
  acting.setAttribute('aria-checked', String(now.acting));
  // Turning one on asks Chrome first, in the click; off takes nothing from Chrome.
  reading.onclick = () => {
    if (now.reading) {
      set({ reading: false, acting: false });
      return;
    }
    const asked = access ? Promise.resolve(true) : mayReach(PAGE_ACCESS, 'websites');
    asked.then((given) => (given ? set({ reading: true, acting: false }) : toast(`${BROWSER} did not give Spindle access to sites.`)));
  };
  acting.onclick = () => {
    if (now.acting) {
      set({ acting: false });
      return;
    }
    const asked = access ? Promise.resolve(true) : mayReach(PAGE_ACCESS, 'websites');
    asked.then((given) => (given ? set({ reading: true, acting: true }) : toast(`${BROWSER} did not give Spindle access to sites.`)));
  };
  // Before acting: Auto (the default) asks before it sends or submits; Ask before every
  // action that changes a page; Full acts without asking.
  const mode = confirmMode(kept);
  for (const button of $('act-confirm').querySelectorAll('button')) {
    button.setAttribute('aria-checked', String(button.dataset.value === mode));
    button.onclick = () => set({ confirm: button.dataset.value });
  }
  $('act-confirm-note').textContent = {
    full: 'Acts without asking: the whole task, start to end.',
    auto: 'Asks you first before it sends or submits: Enter, a submit or send button, a page dialog it accepts, files it uploads. It judges by a control\'s words, so it can miss one.',
    ask: 'Asks you first before every click, keystroke, choice, file it uploads, page it opens and tab it closes.',
  }[mode];
  $('pages-summary').textContent = now.acting ? (mode === 'full' ? 'Read · act' : `Read · act · ${mode === 'auto' ? 'Auto' : 'Ask'}`) : now.reading ? 'Read' : 'Off';
  // Rules by site (Allow, Ask, Never) wait for the engine to keep and follow them.
  $('site-rules').hidden = !FEATURES.siteRules;
  $('site-access').textContent = access ? '' : `${BROWSER} asks for access to sites when you turn reading on, or a thread's network.`;
  $('site-access').hidden = access;
  $('site-access-remove').hidden = !access;
  $('site-access-remove').title = `Page tools use ${BROWSER}'s access to sites while on, and so do threads' commands with network (through ${BROWSER})`;
  $('site-access-remove').onclick = async () => {
    await chrome.permissions.remove({ origins: [...PAGE_ACCESS, ...PICTURE_ACCESS] }).catch(() => false);
    // Giving the access again later (for a thread's network) turns no switch back on.
    const fresh = await read('kv', 'pages').catch(() => null);
    await write('kv', 'pages', pagesRecord(fresh, { reading: false, acting: false }));
    chrome.runtime.sendMessage({ type: 'pages' }).catch(() => {});
    toast(`${BROWSER}'s access to sites is taken back: no page is read or acted on, and commands reach no website through ${BROWSER}.`);
    renderPages();
  };
}

// Chrome's access to sites given or taken back elsewhere (its own settings, a thread's
// network): an open Settings shows the switches as they now are.
for (const change of [chrome.permissions.onAdded, chrome.permissions.onRemoved]) {
  change?.addListener(() => { if ($('settings-dialog').open) renderPages().catch(() => {}); });
}

function renderTheme() {
  const theme = globalThis.spindleTheme;
  const mode = { paper: 'Paper', light: 'Light', dim: 'Dim', system: 'System' }[theme?.get()] ?? 'Paper';
  $('appearance-summary').textContent = `${mode} · ${theme?.texture() === 'plain' ? 'plain' : 'textured'}`;
  const marks = [['theme-choice', 'mode', theme?.get(), (value) => theme?.set(value)], ['texture-choice', 'texture', theme?.texture(), (value) => theme?.setTexture(value)]];
  for (const [group, key, value, choose] of marks) {
    for (const button of $(group).querySelectorAll('button')) {
      const on = button.dataset[key] === value;
      button.classList.toggle('active', on);
      button.setAttribute('aria-pressed', String(on));
      button.onclick = () => {
        choose(button.dataset[key]);
        renderTheme();
      };
    }
  }
}

async function renderAbout() {
  const facts = $('about');
  facts.replaceChildren();
  const add = (term, value) => facts.append(el('dt', '', term), el('dd', '', value));
  // The extension's own version, as the store lists it; the engine's package has its own.
  const version = chrome.runtime.getManifest().version;
  add('Version', version);
  if (engine?.info) add('Engine', `${engine.info.version} · schema ${engine.info.schema}`);
  $('about-summary').textContent = version;
  add('Threads', String(threads.size));
  // What this build is made of (chrome/build.sh), to name in a report of a fault.
  try {
    const build = await (await fetch(chrome.runtime.getURL('build.json'))).json();
    add('Build', [`${build.version} · spindle ${build.spindle} · engine ${build.engine}`, build.edition === 'store' && 'store build'].filter(Boolean).join(' · '));
    add('Linux', [`v86 image ${String(build.linux).slice(0, 12)}`, build.wasm_linux ? `wasm Linux ${String(build.wasm_linux).slice(0, 12)}` : 'no wasm Linux'].join(' · '));
  } catch {}
  try {
    const estimate = await navigator.storage.estimate();
    add('Storage', `${size(estimate.usage ?? 0)} used`);
  } catch {}
}

// ---- Settings > Storage -------------------------------------------------------------

/** Free pages below this are not worth a compaction (nor a row's offer of one). */
const WORTH_FREEING = 1024 * 1024;
/** Set while space is being freed: one thing at a time, the rest waits. */
let freeing = false;

/** The Linux snapshot v86 resumes from (OPFS `linux/`): its files and their size. */
async function linuxSnapshot() {
  try {
    const dir = await (await navigator.storage.getDirectory()).getDirectoryHandle('linux');
    const names = [];
    let bytes = 0;
    for await (const [name, handle] of dir.entries()) {
      if (handle.kind !== 'file') continue;
      names.push(name);
      bytes += (await handle.getFile()).size;
    }
    return { dir, names, bytes };
  } catch {
    return { dir: null, names: [], bytes: 0 };
  }
}

/** A text button for a Storage row (its tooltip says more). */
function storageAction(label, title, run, { disabled = false, danger = false } = {}) {
  const button = el('button', `storage-act${danger ? ' danger' : ''}`, label);
  button.type = 'button';
  button.title = title;
  button.disabled = disabled || freeing;
  button.onclick = run;
  return button;
}

/** A Storage row: its name and size, a ⋯ menu for what is done less often (`menu`:
 * [label, icon, run, class]), a short line of what it holds (`parts`) with at most one
 * action beside it (`action`: what gives space back and costs nothing), a hairline
 * of its share of the largest row, and rows of its own beneath (`subs`). */
function storageRow({ name, bytes, parts = [], action = null, menu = [], share = 0, subs = [], cls = '' }) {
  const row = el('li', `storage-row${cls ? ` ${cls}` : ''}`);
  const head = el('div', 'storage-head');
  head.append(el('span', 'storage-name', name), el('span', 'storage-size', size(bytes)));
  if (menu.length) {
    const more = iconButton('more', `More for “${name}”`, (event) => {
      event.stopPropagation();
      rowMenu(more, menu);
    }, 'icon-button storage-more', 16);
    more.setAttribute('aria-haspopup', 'menu');
    more.setAttribute('aria-expanded', 'false');
    more.disabled = freeing;
    head.append(more);
  } else {
    // Its place kept, so sizes line up whether a row has a menu or not.
    head.append(el('span', 'storage-more-place'));
  }
  row.append(head);
  if (parts.length || action) {
    const line = el('div', 'storage-line');
    line.append(el('span', 'storage-detail', parts.filter(Boolean).join(' · ')));
    if (action) line.append(action);
    row.append(line);
  }
  if (share > 0) {
    const bar = el('div', 'storage-bar');
    bar.style.setProperty('--share', `${Math.max(1, Math.round(share * 100))}%`);
    row.append(bar);
  }
  if (subs.length) {
    const list = el('ul', 'storage-subs');
    list.append(...subs);
    row.append(list);
  }
  return row;
}

/** A row beneath a thread's: `text`, a quieter `note`, and its actions. */
function storageSub(text, note, actions = []) {
  const sub = el('li', 'storage-sub');
  const words = el('span', 'storage-sub-text');
  words.append(el('span', '', text));
  if (note) words.append(el('span', 'storage-detail', note));
  const acts = el('span', 'storage-sub-acts');
  acts.append(...actions);
  sub.append(words, acts);
  return sub;
}

/** A day and time, short ("Nov 3, 10:09 PM"). */
const whenShort = (at) => new Date(at).toLocaleString([], { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });

/** Settings > Storage: each thread, largest first, with what can be freed in it and how;
 * the Linux snapshot; and the rest (settings, keys, the browser's own bookkeeping). */
async function renderStorage() {
  const [report, linux, estimate] = await Promise.all([
    ask('storage').catch(() => null),
    linuxSnapshot(),
    navigator.storage.estimate().catch(() => ({})),
  ]);
  const box = $('storage');
  box.replaceChildren();
  $('storage-free-all').hidden = true;
  if (!report) {
    $('storage-summary').textContent = '';
    return;
  }
  // Freeing space holds the engine, so it waits for every running thread.
  const busy = [...threads.values()].some((thread) => thread.state === 'running');
  const rows = report.threads
    .map((usage) => ({ usage, thread: threads.get(usage.id), total: usage.bytes + usage.bundle + (usage.held ?? 0) }))
    .filter(({ thread }) => thread)
    .sort((a, b) => b.total - a.total);
  const largest = Math.max(1, rows[0]?.total ?? 0, report.lost?.bytes ?? 0, linux.bytes ?? 0);
  let freeable = 0;
  let counted = 0;
  for (const { usage, thread, total } of rows) {
    counted += total;
    const title = thread.title || 'Untitled';
    const parts = [];
    const menu = [];
    let action = null;
    if (usage.move) {
      // Moving to or from the user's host: the bytes held here until the other side has it.
      parts.push(usage.move.way === 'to' ? 'Moving to your host' : 'Coming back from your host');
      if (usage.held) parts.push(`${size(usage.held)} held here until it arrives`);
      if (usage.move.error) parts.push(`waits: ${usage.move.error}`);
    } else if (usage.hosted) {
      parts.push('On your host; only a note of it is kept here');
    }
    if (thread.state === 'moved') {
      parts.push(usage.bundle ? `In a file, ${size(usage.bundle)} of it kept here` : thread.file ? `In your file ${thread.file.name}` : 'Moved into a file');
      if (usage.sealed) {
        parts.push(`sealed copy ${size(usage.sealed)}`);
        freeable += usage.sealed;
        action = storageAction(`Free ${size(usage.sealed)}`, `Remove the sealed copy “${title}” left when it moved (its file is checked whole first)`, () => freeSealed(usage.id, title));
      }
      if (usage.bundle) menu.push(['Save and free…', 'download', () => toFile(usage.id, title, false)]);
    } else if (!usage.hosted) {
      const history = (usage.database ?? 0) + (usage.log ?? 0);
      if (history) parts.push(`History ${size(history)}`);
      if (usage.tasks) parts.push(`tasks ${size(usage.tasks)}`);
      if (usage.aside) parts.push(`first attempts ${size(usage.aside)}`);
      if (usage.free >= WORTH_FREEING) {
        parts.push(`${size(usage.free)} free inside`);
        freeable += usage.free;
        action = storageAction(`Free ${size(usage.free)}`, `Give back the free space inside “${title}” (nothing in it changes)`, () => compactThread(usage.id, title), { disabled: busy });
      }
      if (usage.tasks >= WORTH_FREEING) menu.push(['Free tasks…', 'task', () => freeTasks(usage.id, title)]);
      if (!usage.move && thread.state !== 'running') menu.push(['Move to file…', 'download', () => toFile(usage.id, title, true, (usage.versions ?? []).length)]);
      if (usage.aside) menu.push(['Remove first attempts…', 'trash', () => dropAside(usage.id, title), 'danger']);
    }
    // The thread as it was before it started afresh: kept until it has gone on past it.
    const subs = (usage.versions ?? []).map((version) => {
      const when = whenShort(version.made);
      const waits = [version.due && `until ${new Date(version.due).toLocaleDateString([], { month: 'short', day: 'numeric' })}`, version.turns && `${version.turns} more ${version.turns === 1 ? 'turn' : 'turns'}`].filter(Boolean);
      const actions = [storageAction('Open', `Open “${title}” as it was on ${when}, as a thread of its own (this copy stays kept)`, () => openEarlier(usage.id, version.backup, title))];
      if (version.free) actions.push(storageAction('Remove', `Remove the earlier version of “${title}” from ${when} for good`, () => freeEarlier(usage.id, version.backup, title, when), { danger: true }));
      return storageSub(`As it was on ${when}`, `Earlier version · ${size(version.size)}${version.free ? '' : ` · kept ${waits.join(' and ')}`}`, actions);
    });
    box.append(storageRow({ name: title, bytes: total, parts, action, menu, share: total / largest, subs }));
  }
  // Bytes whose slot's name could not be read (its header damaged): no file leads to them,
  // but they may be a thread's: never in Free all, and removed only when asked, for good.
  if (report.lost?.bytes) {
    counted += report.lost.bytes;
    const one = report.lost.files === 1;
    box.append(storageRow({
      name: 'Unreadable', bytes: report.lost.bytes, share: report.lost.bytes / largest,
      parts: [`${one ? 'A file' : `${report.lost.files} files`} whose name in storage was found damaged; nothing reads ${one ? 'it' : 'them'}`],
      action: storageAction('Remove', 'Remove the bytes no file name leads to, for good', () => reclaimLost(report.lost.bytes), { danger: true }),
    }));
  }
  if (linux.bytes) {
    counted += linux.bytes;
    box.append(storageRow({
      name: 'Linux snapshot', bytes: linux.bytes, share: linux.bytes / largest,
      parts: ['v86 resumes from it; rebuilt in under a minute when next needed'],
      action: storageAction('Remove', 'Remove the Linux snapshot (the next command for v86 boots afresh)', () => removeSnapshot(), { danger: true }),
    }));
  }
  const used = estimate.usage ?? 0;
  if (used - counted >= WORTH_FREEING) box.append(storageRow({ name: 'Everything else', bytes: used - counted, parts: ['Settings, keys and the browser’s own bookkeeping'], cls: 'rest' }));
  const worth = freeable >= WORTH_FREEING;
  $('storage-summary').textContent = used ? `${size(used)}${worth ? ` · ${size(freeable)} can be freed` : ''}` : '';
  $('storage-free-all').hidden = !worth;
  $('storage-free-all').textContent = `Free ${size(freeable)}`;
  $('storage-free-all').disabled = busy || freeing;
  $('storage-free-all').title = busy ? 'Space is freed while no thread is running' : 'Give back the free space every thread can, and the sealed copies moved threads left (nothing in them changes)';
  if (!freeing) {
    $('storage-note').hidden = !busy || !worth;
    $('storage-note').textContent = busy && worth ? 'Space is freed while no thread is running.' : '';
  }
}

/** Runs one freeing at a time, saying what it does; the rows are drawn again after. */
async function freeSpace(doing, work) {
  if (freeing) return;
  freeing = true;
  for (const button of document.querySelectorAll('#storage button, #storage-free-all')) button.disabled = true;
  $('storage-note').hidden = false;
  $('storage-note').textContent = doing;
  try {
    await work();
  } catch (error) {
    toast(error.message);
  } finally {
    freeing = false;
    $('storage-note').hidden = true;
    await renderStorage().catch(() => {});
  }
}

function compactThread(id, title) {
  return freeSpace(`Freeing space in “${title}”…`, async () => {
    const { before, after } = await ask('compact', { id });
    toast(`Freed ${size(Math.max(0, before - after))} in “${title}”.`);
  });
}

function freeSealed(id, title) {
  return freeSpace(`Checking the file “${title}” moved into…`, async () => {
    const { freed } = await ask('freeSealed', { id });
    toast(`Freed ${size(freed)}: “${title}” is kept in its file.`);
  });
}

async function dropAside(id, title) {
  if (!(await confirmed('Remove first attempts?', `When its first request was tried again, “${title}” kept the first attempt aside. Removing it frees its space; what it may have cost stays counted.`, 'Remove'))) return;
  return freeSpace(`Removing the first attempts of “${title}”…`, async () => {
    const { freed } = await ask('dropAside', { id });
    toast(`Removed ${size(freed)} of first attempts from “${title}”.`);
  });
}

/** A saved file's own name (Chrome gives its whole path). */
const fileName = (path) => String(path).split(/[\\/]/).pop();

/** A thread's finished tasks into one .zip of session bundles on the user's disk (each
 * imports as a thread to read), then their working copies freed here, once Chrome says
 * the file is saved whole. */
async function freeTasks(id, title) {
  let found;
  try {
    found = await ask('finishedTasks', { id });
  } catch (error) {
    toast(error.message);
    return;
  }
  if (!found.length) {
    toast(`“${title}” has no finished task whose working copy is still here.`);
    return;
  }
  const total = found.reduce((sum, task) => sum + task.bytes, 0);
  const many = found.length === 1 ? 'task' : 'tasks';
  if (!(await confirmed(`Free ${found.length} finished ${many}?`, `Their working copies in “${title}” (${size(total)}: each task's own steps and files) are saved to a .zip in your Downloads, then removed from this browser. Each task's answer and the files it gave stay in the thread, and so does what it spent. A task freed this way, a stopped one included, never runs again. Each file in the .zip imports as a thread of its own.`, 'Save tasks and free'))) return;
  return freeSpace(`Saving the finished ${many} of “${title}”…`, async () => {
    // In .zip files of about 1 GiB each, each freed once it is saved whole: what is held
    // in memory stays bounded, and no .zip nears the 4 GiB the format can hold.
    const stem = `${title.replace(/[^\p{L}\p{N}]+/gu, '-').slice(0, 60) || 'thread'}-tasks`;
    const saved = [];
    let freed = 0;
    let group = [];
    let held = 0;
    const flush = async () => {
      if (!group.length) return;
      const { filename } = await savedOnDisk(zip(group.map(({ file }) => file)), `${stem}${saved.length ? `-${saved.length + 1}` : ''}.zip`);
      saved.push(fileName(filename));
      $('storage-note').textContent = `Freeing the finished ${many}…`;
      for (const { task } of group) freed += (await ask('taskDrop', { id, task: task.task, expectedState: task.sourceState })).dropped?.bytes ?? 0;
      group = [];
      held = 0;
    };
    for (const [index, task] of found.entries()) {
      // The remains of a task that never began (storage ran out as it started) hold
      // nothing to save: freed as they are.
      if (task.remains) {
        freed += (await ask('taskDrop', { id, task: task.task, expectedState: '' })).dropped?.bytes ?? 0;
        continue;
      }
      $('storage-note').textContent = `Saving ${found.length === 1 ? 'the task' : `task ${index + 1} of ${found.length}`}…`;
      const { bytes } = await ask('taskExport', { id, task: task.task }, 300_000);
      task.sourceState = bundleFacts(bytes).sourceState;
      if (group.length && held + bytes.byteLength > 1024 ** 3) await flush();
      // Numbered: two tasks may share a name.
      group.push({ task, file: { path: `${index + 1}-${String(task.name).replace(/[^\p{L}\p{N}._-]+/gu, '-').slice(0, 60) || 'task'}.spindle`, bytes } });
      held += bytes.byteLength;
    }
    await flush();
    toast(`Saved to ${saved.join(', ')}; ${size(freed)} freed here.`);
  });
}

/** A thread into a file on the user's disk (`move`: moved into it first), then its copy
 * here removed once Chrome says the file is saved whole. A save that fails leaves the
 * thread moved, with its file still here to save again. */
async function toFile(id, title, move, versions = 0) {
  const asked = move
    ? confirmed(`Move “${title}” to a file and free its space?`, `It is saved to your Downloads as a .spindle file, and its copy in this browser is removed once Chrome has the file whole. Import the file to bring it back, here or in native Spindle.${versions ? ` Its ${versions === 1 ? 'earlier version stays' : `${versions} earlier versions stay`} here (Settings › Storage), removable after a week.` : ''}`, 'Move and free')
    : confirmed(`Save “${title}” and free its space?`, 'Its file is saved to your Downloads, and the copy kept in this browser is removed once Chrome has the file whole. Import the file to bring it back.', 'Save and free');
  if (!(await asked)) return;
  return freeSpace(`Saving “${title}” to a file…`, async () => {
    const kept = move ? await ask('export', { id, keep: false }) : await read('bundles', id);
    if (!kept?.bytes) throw new Error('This thread keeps no file here.');
    const { filename } = await savedOnDisk(kept.bytes, kept.name);
    const { freed } = await ask('dropBundle', { id, filename });
    toast(`“${title}” is saved as ${fileName(filename)}; ${size(freed)} freed here.`);
  });
}

function openEarlier(id, backup, title) {
  return freeSpace(`Opening the earlier version of “${title}”…`, async () => {
    const { id: opened } = await ask('earlierOpen', { id, backup });
    toast(`Opened as “${threads.get(opened)?.title ?? 'a new thread'}”; the earlier version stays kept here.`);
  });
}

async function freeEarlier(id, backup, title, when) {
  if (!(await confirmed('Remove this earlier version?', `“${title}” as it was on ${when} is deleted for good.`, 'Remove'))) return;
  return freeSpace(`Removing the earlier version of “${title}”…`, async () => {
    const { freed } = await ask('earlierFree', { id, backup });
    toast(`Removed ${size(freed)}.`);
  });
}

async function reclaimLost(bytes) {
  if (!(await confirmed('Remove unreadable bytes?', `${size(bytes)} whose names in storage were found damaged. They may have been part of a thread; once removed they are gone for good.`, 'Remove'))) return;
  return freeSpace('Removing unreadable bytes…', async () => {
    const { freed } = await ask('reclaimLost');
    toast(`Removed ${size(freed)}.`);
  });
}

function removeSnapshot() {
  return freeSpace('Removing the Linux snapshot…', async () => {
    // As it is now (a newer image may have saved another since the page was drawn).
    const { dir, names } = await linuxSnapshot();
    for (const name of names) await dir.removeEntry(name);
    toast('The Linux snapshot is removed; v86 boots afresh when a command next needs it.');
  });
}

/** Free all: every thread with free space worth giving back, one after another, and
 * every moved thread's sealed copy. */
$('storage-free-all').onclick = () => freeSpace('Freeing space…', async () => {
  const { threads: usage } = await ask('storage');
  let freed = 0;
  try {
    for (const entry of usage) {
      const thread = threads.get(entry.id);
      if (!thread) continue;
      const title = thread.title || 'Untitled';
      if (thread.state === 'moved' && entry.sealed) {
        $('storage-note').textContent = `Checking the file “${title}” moved into…`;
        freed += (await ask('freeSealed', { id: entry.id })).freed;
      } else if (thread.state !== 'moved' && entry.free >= WORTH_FREEING) {
        $('storage-note').textContent = `Freeing space in “${title}”…`;
        const { before, after } = await ask('compact', { id: entry.id });
        freed += Math.max(0, before - after);
      }
    }
  } catch (error) {
    // What was freed before it stopped is said too.
    if (freed) throw new Error(`Freed ${size(freed)}, then stopped: ${error.message}`);
    throw error;
  }
  toast(`Freed ${size(freed)}.`);
});
$('settings').onclick = () => openSettings();
$('open-tab').onclick = openTab;
$('restart-engine').onclick = async () => {
  $('settings-dialog').close();
  await askRestartEngine();
};

$('catalog-file').onchange = async (event) => {
  const file = event.target.files[0];
  event.target.value = '';
  if (!file) return;
  try {
    const catalog = JSON.parse(await file.text());
    apply(await ask('catalog', { catalog }));
    toast(`Using ${file.name}: ${providers().length} providers.`);
    openSettings();
  } catch (error) {
    toast(`${file.name} was not used: ${error.message}`);
  }
};
$('catalog-builtin').onclick = async () => {
  try {
    apply(await ask('catalog', { catalog: null }));
    toast('Using the built-in providers.');
    openSettings();
  } catch (error) {
    toast(error.message);
  }
};

// ---- The workspace page ------------------------------------------------------------

/** The workspace page: the open thread's files (by folder; what the last turn made or
 * changed marked; scratch files on request), and one of them open with its preview and
 * the files before and after it a click away. A page of the panel's own: the
 * conversation stays as it was behind it. */
const ws = { id: null, files: [], changes: new Map(), order: [], path: null, scrolled: 0, loading: false };

async function openWorkspace(id, path = null) {
  if (!id) return;
  pausePlayers();
  // Where the conversation was scrolled to, for coming back to it.
  if (document.body.dataset.view !== 'workspace') ws.scrolled = $('main').scrollTop;
  ws.id = id;
  document.body.dataset.view = 'workspace';
  $('welcome').hidden = true;
  $('thread').hidden = true;
  $('workspace').hidden = false;
  $('dock').hidden = true;
  $('jump').hidden = true;
  homeLabel('Back to the conversation', 'Back to the conversation');
  $('ws-heading').textContent = 'Workspace';
  $('ws-sub').textContent = threads.get(id)?.title ?? '';
  $('ws-filter').value = '';
  Object.assign(ws, { files: [], changes: new Map(), order: [], loading: true });
  $('file-list').replaceChildren(el('p', 'note', 'Reading…'));
  $('files-zip').hidden = true;
  showFiles();
  $('main').scrollTop = 0;
  $('ws-heading').focus({ preventScroll: true });
  try {
    const { files, changes } = await turnFiles(id);
    if (ws.id !== id || $('workspace').hidden) return;
    Object.assign(ws, { files, changes, loading: false });
    drawFiles();
    if (path) openFile(path);
  } catch (error) {
    if (ws.id !== id) return;
    ws.loading = false;
    $('file-list').replaceChildren(el('p', 'notice state warning', error.message));
  }
}

/** The workspace page lists the files again (a turn of its thread ended meanwhile). */
async function refreshWorkspace(id) {
  if (ws.id !== id || ws.loading || document.body.dataset.view !== 'workspace') return;
  try {
    const { files, changes } = await turnFiles(id);
    if (ws.id !== id) return;
    Object.assign(ws, { files, changes });
    drawFiles();
  } catch {}
}

/** The workspace page's preview emptied: one still on its way does not land, and what
 * played there stops and lets go of its file. */
function clearPreview() {
  const preview = $('ws-preview');
  preview.previewing = (preview.previewing ?? 0) + 1;
  preview.replaceChildren();
  sweepPlayers();
}

/** Back to the conversation the workspace page was opened from. */
function closeWorkspace() {
  ws.id = null;
  clearPreview();
  if (!current || !threads.has(current)) {
    showHome();
    return;
  }
  document.body.dataset.view = 'thread';
  $('workspace').hidden = true;
  $('thread').hidden = false;
  $('dock').hidden = false;
  homeLabel('All threads', 'All threads');
  $('main').scrollTop = ws.scrolled;
  $('prompt').focus({ preventScroll: true });
  renderJump();
}

function drawFiles() {
  // Still being read: the list says so until they come (and draws them then).
  if (ws.loading) return;
  const { files, changes } = ws;
  const fresh = files.filter((file) => changes.has(file.path)).length;
  $('ws-heading').textContent = 'Workspace';
  $('ws-sub').textContent = [`${files.length} file${files.length === 1 ? '' : 's'}`, threads.get(ws.id)?.title].filter(Boolean).join(' · ');
  $('ws-note').textContent = fresh
    ? `${fresh} made or changed by the last turn (marked).`
    : 'Files you attach land here; what the thread makes, you can open and download.';
  $('files-zip').hidden = !files.length;
  ws.order = drawTree($('file-list'), files, changes, { onOpen: openFile, filter: $('ws-filter').value, links: fileLinks, key: ws.id, redraw: drawFiles });
}

/** A file row's links: download, and open in a tab of its own for a page, picture, PDF,
 * document, audio or video. */
function fileLinks(file) {
  const id = ws.id;
  const get = el('a', 'file-get');
  get.href = '#';
  get.title = `Download ${file.path}`;
  get.setAttribute('aria-label', `Download ${file.path}`);
  get.append(icon('download'));
  get.onclick = async (event) => {
    event.preventDefault();
    try {
      download((await ask('read', { id, path: file.path })).bytes, file.path.split('/').pop());
    } catch (error) {
      toast(error.message);
    }
  };
  if (!TABBED.has(kindOf(file.path))) return [get];
  const open = el('a', 'file-get');
  open.href = '#';
  open.title = `Open ${file.path} in a tab`;
  open.setAttribute('aria-label', `Open ${file.path} in a tab`);
  open.append(icon('external'));
  open.onclick = (event) => {
    event.preventDefault();
    chrome.tabs.create({ url: viewUrl(id, file.path) });
  };
  return [open, get];
}

function showFiles() {
  const was = ws.path;
  ws.path = null;
  clearPreview();
  $('ws-list').hidden = false;
  $('ws-file').hidden = true;
  // Back on the list: on the row of the file just shown, or (its folder shut, or it
  // past "Show more") on the nearest row that holds it; nothing opens by itself.
  if (!was) return;
  const list = $('file-list');
  const shown = (node) => node && !node.closest('[hidden]');
  const row = list.querySelector(`.file-row[data-path="${CSS.escape(was)}"] .file-open`);
  if (shown(row)) {
    row.focus();
    return;
  }
  const holder = [...list.querySelectorAll('.ws-folder-head')]
    .filter((head) => shown(head) && was.startsWith(`${head.dataset.folder}/`))
    .sort((a, b) => b.dataset.folder.length - a.dataset.folder.length)[0];
  (holder ?? [...list.querySelectorAll('.ws-more')].find(shown))?.focus();
}

/** One file open on the workspace page. */
function openFile(path) {
  const file = ws.files.find((one) => one.path === path);
  if (!file) {
    toast(`${path} is not in the workspace now.`);
    return;
  }
  const id = ws.id;
  ws.path = path;
  $('ws-list').hidden = true;
  $('ws-file').hidden = false;
  const parts = path.split('/');
  $('ws-path').replaceChildren(...parts.flatMap((part, index) => [el('span', 'ws-sep', '/'), index === parts.length - 1 ? el('strong', '', shownName(path)) : el('span', '', part)]));
  const change = ws.changes.get(path);
  const kind = kindOf(path);
  $('ws-meta').replaceChildren(document.createTextNode([KIND_NAMES[kind] ?? 'file', size(file.size)].join(' · ')));
  if (change) $('ws-meta').append(el('span', `badge ${change}`, change));
  const actions = [copyButton(() => path, 'Copy its path')];
  if (TABBED.has(kind)) actions.push(iconButton('external', `Open ${path} in a tab`, () => chrome.tabs.create({ url: viewUrl(id, path) })));
  actions.push(iconButton('download', `Download ${path}`, async () => {
    try {
      await saveFile(await readBytes(id, path), path.split('/').pop(), typeOf(path));
      toast(`Downloaded ${path.split('/').pop()}.`);
    } catch (error) {
      toast(`${path} was not downloaded: ${error.message}`);
    }
  }));
  $('ws-actions').replaceChildren(...actions);
  // Before and after: in the list's order (all files when this one is out of it).
  const order = ws.order.includes(path) ? ws.order : fileTree(ws.files).order.map((one) => one.path);
  const at = order.indexOf(path);
  $('ws-prev').disabled = at <= 0;
  $('ws-next').disabled = at < 0 || at >= order.length - 1;
  $('ws-prev').onclick = () => at > 0 && openFile(order[at - 1]);
  $('ws-next').onclick = () => at < order.length - 1 && openFile(order[at + 1]);
  const preview = $('ws-preview');
  preview.className = `kind-${kind ?? 'bytes'}`;
  previewInto(preview, id, path, { any: true, textId: 'file-content', size: file.size });
  $('main').scrollTop = 0;
  if (!$('ws-file').contains(document.activeElement)) $('ws-path').focus({ preventScroll: true });
}

$('ws-done').onclick = closeWorkspace;
$('ws-files').onclick = showFiles;
$('ws-filter').addEventListener('input', drawFiles);
scratchSwitch(drawFiles, $('ws-scratch'));
$('files-zip').replaceChildren(icon('download'));
$('files-zip').onclick = () => downloadAll(ws.id, ws.files, $('files-zip'));
$('ws-prev').replaceChildren(icon('prev'));
$('ws-next').replaceChildren(icon('next'));
// Keys on the workspace page (wherever focus is on it, the page itself included).
document.addEventListener('keydown', (event) => {
  if (document.body.dataset.view !== 'workspace' || event.defaultPrevented || document.querySelector('dialog[open]')) return;
  if (event.target.closest?.('input, textarea')) {
    if (event.key === 'Escape' && event.target.value) {
      event.target.value = '';
      drawFiles();
    }
    return;
  }
  if (event.key === 'Escape') {
    event.preventDefault();
    if (ws.path) showFiles();
    else closeWorkspace();
  } else if (event.target.closest?.('pre, .md')) {
    // Arrows scroll what is read there.
  } else if (ws.path && event.key === 'ArrowLeft' && !$('ws-prev').disabled) {
    $('ws-prev').click();
  } else if (ws.path && event.key === 'ArrowRight' && !$('ws-next').disabled) {
    $('ws-next').click();
  }
});

// ---- Thread menu, files, moving --------------------------------------------------

function openThreadMenu() {
  const thread = threads.get(current);
  if (!thread) return;
  $('rename').value = thread.title;
  const commands = thread.home ? `on your Spindle host, ${thread.home.url}`
    : { app: "commands in this browser's Linux", remote: 'commands in your Spindle host' }[thread.runner] ?? 'no commands';
  const facts = [`${nameOf(thread.provider)} · ${thread.model}`, commands, `started ${new Date(thread.created).toLocaleString()}`];
  if (status) facts.push(`revision ${status.head}`);
  $('thread-facts').textContent = facts.join(' · ');
  const moved = thread.state === 'moved';
  const hosted = Boolean(thread.home);
  const moving = Boolean(thread.moving && thread.moving.step !== 'release');
  $('menu-files').disabled = moved || moving;
  $('menu-fresh').disabled = moved || hosted || moving;
  $('menu-export').disabled = moved || hosted || moving;
  $('menu-copy').disabled = moved || hosted || moving;
  // Run on my host: with the host connected by Connect (a browser token: a runner token
  // only runs commands).
  $('menu-host').hidden = moved || hosted || !engine?.host?.connected || engine.host.kind !== 'browser';
  $('menu-host').disabled = moving || thread.state === 'running';
  $('menu-back').hidden = !hosted;
  $('menu-back').disabled = moving || thread.state === 'running';
  $('thread-dialog').showModal();
}

/** The thread whose files the next new thread starts with ({id, title}), or null. */
let carrying = null;

// ---- Tasks ------------------------------------------------------------------------
//
// A task the thread starts shows where it started, as a card below the step that started
// it: what it is called, how it stands (working, with what it does now and Stop; done;
// stopped; failed), its steps and time, and, opened, what it was asked and its answer.
// Its report, when the thread reads it, shows as the task's (folded), not as a message of
// the user's. One line above the box says what goes on meanwhile.

/** The open thread's tasks as last read (`tasks` from the engine). */
let taskList = [];
/** Whether the open thread's turn waits for its tasks now. */
let waitingOnTasks = false;

/** A task's short name, from what `tasks` and events call it (`dir#name`, or `name`). */
const taskName = (task) => String(task?.task ?? task ?? '').split('#').pop();

/** How `tasks` names a task by what it was asked: its first line, at most 80 characters. */
const askedLine = (prompt) => [...(String(prompt ?? '').split(/\r?\n/).find((line) => line.trim()) ?? '').trim()].slice(0, 80).join('');

/** The open thread's tasks, read again at most every `wait` ms however many events come
 * (a task streaming its reply sends one per piece), or `now`; while some run, again
 * every little while (one waiting on its model says nothing meanwhile). */
let tasksTimer = 0;
let tasksDue = false;
function loadTasks(id, now = false, wait = 600) {
  if (tasksDue && !now) return;
  clearTimeout(tasksTimer);
  tasksDue = true;
  tasksTimer = setTimeout(async () => {
    tasksDue = false;
    if (id !== current) return;
    try {
      const { tasks = [], activity } = await ask('tasks', { id });
      if (id !== current) return;
      waitingOnTasks = activity === 'waiting_tasks' && threads.get(id)?.state === 'running';
      // Its model has written all it will until a task reports.
      if (waitingOnTasks) endLive(live?.text);
      renderTasks(id, tasks);
      renderSend();
      if (tasks.some((task) => task.pending)) loadTasks(id, false, 1500);
    } catch {}
  }, now ? 0 : wait);
}

/** The task a background `task` call's result names (its short name), or null. */
const namedIn = (result) => /(?:#|\/tasks\/)([0-9a-f]{20})\b/.exec(result ?? '')?.[1] ?? null;

/** The newest task of the open thread a `task` call (its input) may have started, by
 * what it was asked, among those of its kind (in the background or not) not `taken`; for
 * a call still running, one still at work if there is one. */
function askedBy(input, taken, running) {
  const asked = askedLine(input?.prompt);
  const queued = input?.background === true;
  const fits = (task) => !taken.has(taskName(task)) && Boolean(task.queued) === queued && task.prompt === asked;
  return (running && taskList.findLast((task) => fits(task) && task.state === 'running')) || taskList.findLast(fits) || null;
}

/** The card of a task a `task` call (`card`, its step, and its `result` once it has one)
 * started, below the step. */
function taskBlock(card, result = null) {
  const block = el('details', 'task-block');
  block.card = card;
  block.result = result;
  card.block = block;
  const head = el('summary', 'task-head');
  const title = el('span', 'task-title', taskTitle(card.input?.prompt));
  const stop = el('button', 'task-stop');
  stop.type = 'button';
  stop.hidden = true;
  stop.append(el('span', 'stop-square'));
  stop.setAttribute('aria-label', `Stop the task “${taskTitle(card.input?.prompt)}”`);
  const chevron = el('span', 'group-chevron');
  chevron.append(icon('next', 12));
  head.append(el('span', 'task-status'), el('span', 'task-word', 'Task'), title, el('span', 'task-chip'), stop, chevron);
  const body = el('div', 'task-body');
  const asked = el('div', 'task-asked');
  asked.append(el('span', 'role-label', 'ASKED'), el('p', '', String(card.input?.prompt ?? '')));
  // What else it was given: its model and effort, and what becomes of its files.
  const given = card.input ?? {};
  const how = [
    (given.model ?? given.settings?.model) && `model ${given.model ?? given.settings.model}`,
    (given.effort ?? given.settings?.effort) && `${given.effort ?? given.settings.effort} effort`,
    given.settings?.max_usd != null && `at most $${given.settings.max_usd}`,
    given.files === 'discard' && 'its files are not kept',
  ].filter(Boolean);
  if (how.length) asked.append(el('p', 'task-given', how.join(' · ')));
  body.append(asked);
  block.append(head, body);
  head.dataset.tip = `${taskTitle(card.input?.prompt, 200)}: show or hide what it was asked and its answer`;
  head.title = head.dataset.tip;
  paintTask(block, null);
  return block;
}

/** What a task's card says (`task` as `tasks` lists it; null: not known yet). */
function paintTask(block, task) {
  const card = block.card;
  const calling = card.classList.contains('running');
  const work = task?.work ?? null;
  // A task works while its call runs; one in the foreground ends with its step, one in
  // the background by its row.
  const state = calling || task?.pending ? 'working'
    : !task ? (card.classList.contains('failed') ? 'failed' : 'unknown')
    : !task.queued ? (card.classList.contains('failed') ? 'failed' : 'done')
    : task.state === 'finished' ? (task.answer?.failed ? 'failed' : 'done')
    : task.state === 'stopped' ? 'stopped'
    : task.state === 'abandoned' ? 'dropped'
    : 'paused';
  for (const name of ['working', 'done', 'failed', 'stopped', 'dropped', 'paused', 'unknown']) block.classList.toggle(name, name === state);
  block.task = task;
  block.work = work;
  const chip = block.querySelector('.task-chip');
  chip.textContent = taskChip(state, work);
  chip.title = [TASK_STATES[state], taskWords(state, work)].filter(Boolean).join(': ');
  paintTaskStatus(block.querySelector('.task-status'), state);
  // What it does now, on hover (the row has room for no more).
  const head = block.querySelector('summary');
  const doing = state === 'working' ? taskNow(work) : '';
  head.title = [doing && `Now: ${doing}`, head.dataset.tip].filter(Boolean).join('\n');
  const stop = block.querySelector('.task-stop');
  if (!task?.pending) block.stopping = false;
  stop.hidden = !task?.pending;
  stop.disabled = Boolean(block.stopping);
  stop.title = 'Stop this task where it is (the thread hears it stopped)';
  stop.onclick = (event) => {
    event.preventDefault();
    event.stopPropagation();
    block.stopping = true;
    stop.disabled = true;
    const id = current;
    ask('taskStop', { id, task: task.task }).then(() => loadTasks(id, true), (error) => {
      block.stopping = false;
      stop.disabled = false;
      toast(error.message);
    });
  };
  taskAnswer(block, task);
}

/** A task card's answer, once there is one: its row's (a stopped one's says why it
 * stopped), else its call's result (a task in the foreground answers in it; a call
 * refused, or a background task that could not start, says why). */
function taskAnswer(block, task) {
  const card = block.card;
  const failedCall = card.classList.contains('failed');
  const refused = failedCall && (card.input?.background === true || /^refused: /.test(block.result ?? ''));
  const content = task?.answer?.content ?? ((card.input?.background !== true || failedCall) ? block.result : null);
  if (!content) {
    block.answered = null;
    block.querySelector('.task-answer')?.remove();
    return;
  }
  const failed = task?.answer ? Boolean(task.answer.failed) : failedCall;
  const answer = el('div', 'task-answer');
  const said = task?.answer
    ? task.state === 'stopped' ? 'WHY IT STOPPED' : failed ? 'ITS ANSWER (FAILED)' : 'ITS ANSWER'
    : refused ? 'WHY IT DID NOT START' : failed ? 'ITS ANSWER (FAILED)' : 'ITS ANSWER';
  // Drawn again only when it changed (a task started again answers anew).
  if (block.answered === `${said}\n${content}`) return;
  block.answered = `${said}\n${content}`;
  block.querySelector('.task-answer')?.remove();
  const label = el('span', 'role-label', said);
  label.append(copyButton(() => content, 'Copy it'));
  const body = el('div', 'message md');
  mdNodes(body, markdown(content));
  tidyTables(body);
  answer.append(label, body);
  block.querySelector('.task-body').append(answer);
}

/** A `task` call answered (`result`, or why it was refused): its card says so, and finds
 * its task (a background one's answer names it). */
function taskCalled(id, block, result) {
  block.result = result;
  paintTask(block, block.task ?? null);
  loadTasks(id, true);
}

/** A line about one of the thread's tasks (`task NAME: what`), and those its card says
 * already: its steps (a tool's name), its start and its end. */
const TASK_SAID = /^task ([0-9a-f]{20}): (.*)$/s;
const ROUTINE_TASK = /^(?:[\w.:-]+|done, \d+ files changed|finished in the background|stopped in the background|could not go on in the background \(.*\))$/s;

/** Task `name` as a line about it calls it. */
function taskLabel(name) {
  const known = taskList.find((task) => taskName(task) === name);
  return known ? `Task “${taskTitle(known.prompt)}”` : 'A task';
}

/** A task's state as its row's icon: a spinner at work, a check done, a cross failed or
 * stopped, a dash otherwise. */
function paintTaskStatus(place, state) {
  const shows = { working: 'spinner', done: 'check', failed: 'x', stopped: 'x', paused: 'pause' }[state] ?? 'dash';
  if (place.dataset.shows === shows) return;
  place.dataset.shows = shows;
  place.replaceChildren(icon(shows, 14));
}

/** What each state of a task's card means, for its chip's tooltip. */
const TASK_STATES = {
  working: 'It works beside the thread now',
  done: 'It finished and gave its answer',
  failed: 'It ended without finishing its work',
  stopped: 'It stopped before it finished, without an answer',
  dropped: 'Going back past where it started dropped it',
  paused: 'It goes on when the thread next runs',
};

/** A task card's chip: its steps and time (those this engine saw), "7 · 13:17"; a task
 * that is not at work or done says how it stands. */
function taskChip(state, work) {
  const end = work?.ended ?? (state === 'working' ? Date.now() : null);
  const took = work?.started && end ? clock(end - work.started) : null;
  const words = { failed: 'Failed', stopped: 'Stopped', dropped: 'Dropped', paused: 'Waiting' }[state];
  return [words, work?.steps ? String(work.steps) : null, took].filter(Boolean).join(' · ');
}

/** The chip in words, for its tooltip. */
function taskWords(state, work) {
  const end = work?.ended ?? (state === 'working' ? Date.now() : null);
  return [work?.steps ? `${work.steps} step${work.steps === 1 ? '' : 's'}` : null, work?.started && end ? duration(end - work.started) : null].filter(Boolean).join(', ');
}

/** What a working task does now, in words. */
function taskNow(work) {
  if (work?.now?.name) return `${describe(work.now.name, work.now.input ?? {}, 'running').label}…`;
  if (work?.now?.model) return `${work.now.model} is writing…`;
  return '';
}

/** A task's report as the thread read it (`block`: its text block, `report` from the
 * engine: `{task, state}`): folded, named by its task. */
function reportNode(block) {
  const { task, state } = block.report;
  const node = el('details', `task-report ${state}`);
  node.dataset.task = task;
  const head = el('summary');
  const title = el('span', 'task-title', reportTitle(task));
  const status = el('span', 'task-status');
  paintTaskStatus(status, { finished: 'done', failed: 'failed', stopped: 'stopped' }[state]);
  const chevron = el('span', 'group-chevron');
  chevron.append(icon('next', 12));
  head.append(status, el('span', 'task-word', 'Report'), title, el('span', 'task-chip', { failed: 'Failed', stopped: 'Stopped' }[state] ?? ''), chevron);
  head.title = 'What the thread was told when this task ended';
  const text = block.text.slice(block.text.indexOf('\n') + 1).trim();
  const body = el('div', 'task-report-body');
  const message = el('div', 'message md');
  mdNodes(message, markdown(text));
  tidyTables(message);
  body.append(copyButton(() => text, 'Copy the report'), message);
  node.append(head, body);
  return node;
}

/** A report's task as it is called, once the thread's tasks are read. */
function reportTitle(task) {
  const known = taskList.find((one) => taskName(one) === task);
  return known ? taskTitle(known.prompt) : 'a task';
}

/** The open thread's tasks (`tasks` from the engine): each card in the conversation, each
 * report's name, and the line above the box. */
function renderTasks(id, tasks) {
  taskList = tasks;
  if (!id) waitingOnTasks = false;
  const history = $('history');
  const blocks = [...history.querySelectorAll('.task-block')];
  // A card is its task's by the name its call's result gives (a background task's).
  // Else, worked out again each time, by what it was asked: a call still running, or one
  // in the foreground that was not refused (it ran); the newest card the newest such task.
  for (const block of blocks) block.dataset.task = namedIn(block.result) ?? '';
  const taken = new Set(blocks.map((block) => block.dataset.task).filter(Boolean));
  for (const block of blocks.toReversed()) {
    const card = block.card;
    const running = card.classList.contains('running');
    const asked = running || (card.input?.background !== true && !/^refused: /.test(block.result ?? ''));
    if (block.dataset.task || !asked) continue;
    const task = askedBy(card.input, taken, running);
    if (!task) continue;
    block.dataset.task = taskName(task);
    taken.add(block.dataset.task);
  }
  for (const block of blocks) paintTask(block, tasks.find((task) => taskName(task) === block.dataset.task) ?? null);
  for (const report of history.querySelectorAll('.task-report')) {
    report.querySelector('.task-title').textContent = reportTitle(report.dataset.task);
  }
  renderTaskLine(id, tasks);
}

/** What the composer's top row says of the open thread's tasks: how many work now (a
 * chip, to the first's card), or reports and tasks that wait for its next turn (with
 * Continue). */
function renderTaskLine(id) {
  if (id && id !== current) return;
  renderBar();
  tickTaskLine();
}

/** While the turn waits for its tasks, the row says so, and for how long. */
function tickTaskLine() {
  const working = taskList.filter((task) => task.pending);
  const longest = Math.max(0, ...working.map((task) => (task.work?.started ? Date.now() - task.work.started : 0)));
  if (waitingOnTasks && current && threads.get(current)?.state === 'running') {
    const many = working.length > 1 ? `${working.length} tasks` : 'its task';
    if ($('phase').textContent !== `Waiting for ${many}`) phase(`Waiting for ${many}`, 'busy');
    $('phase').title = `The turn waits for ${many}${longest ? `, at work for ${duration(longest)}` : ''}`;
  }
}

/** Scrolls to a task's card (`block`) and marks it a moment. */
function showTask(block) {
  if (!block.isConnected) return;
  block.scrollIntoView({ block: 'center', behavior: 'smooth' });
  block.classList.remove('flash');
  void block.offsetWidth;
  block.classList.add('flash');
}

// Working tasks' times go on as they run.
setInterval(() => {
  const working = document.querySelectorAll('#history .task-block.working');
  if (!working.length && !taskList.some((task) => task.pending)) return;
  for (const block of working) {
    block.querySelector('.task-chip').textContent = taskChip('working', block.work);
  }
  tickTaskLine();
}, 1000);

/** The open thread's message waiting for its turn to end (`queued` from the engine:
 * `{text, names, held}`; null: none), and taking it back into the box. */
function renderQueued(queued) {
  $('queued').hidden = !queued;
  $('queued').classList.toggle('held', Boolean(queued?.held));
  if (!queued) return;
  const files = queued.names.length ? ` with ${queued.names.join(', ')}` : '';
  const said = `“${queued.text.replace(/\s+/g, ' ').slice(0, 80)}”${files}`;
  $('queued-text').textContent = queued.held ? `Not sent (${queued.held}): ${said}` : `Goes when this turn ends: ${said}`;
  $('queued-text').title = $('queued-text').textContent;
}

$('queued-back').onclick = async () => {
  const id = current;
  try {
    const back = await ask('unqueue', { id });
    if (!back) return;
    const files = back.files.map((file) => ({ name: String(file.path), size: file.bytes.length, file: new File([file.bytes], String(file.path)) }));
    // Into the box only if this thread is still the one open (else kept for it, for when
    // it is): never into another's, and never dropped (the engine let its copy go).
    if (!backToBox(id, back.text, files)) toast(`The waiting message of ${named(id, 'that thread')} was taken back: it is in the box when you open it.`);
  } catch (error) {
    toast(error.message);
  }
};

function carry(thread) {
  carrying = thread ? { id: thread.id, title: thread.title } : null;
  $('carry').hidden = !carrying;
  if (carrying) $('carry-text').textContent = `Starts with the files of “${carrying.title}” (not its conversation)`;
}

$('menu-fresh').onclick = () => {
  const thread = threads.get(current);
  $('thread-dialog').close();
  if (!thread) return;
  // Its model, effort and where its commands run, to change in the model menu if wanted.
  choice = { provider: thread.provider, model: thread.model, effort: thread.effort ?? 'default', runner: thread.runner, network: Boolean(thread.network), tasks: Boolean(thread.tasks), max_usd: choice.max_usd ?? null, compact_tokens: choice.compact_tokens };
  showHome();
  carry(thread);
  $('prompt').focus();
};
$('carry-drop').onclick = () => carry(null);

$('title-button').onclick = openThreadMenu;
$('rename-form').onsubmit = (event) => {
  event.preventDefault();
  ask('rename', { id: current, title: $('rename').value }).catch((error) => toast(error.message));
  $('thread-dialog').close();
};
$('rename').addEventListener('blur', () => {
  const thread = threads.get(current);
  if (thread && $('rename').value.trim() && $('rename').value.trim() !== thread.title) {
    ask('rename', { id: current, title: $('rename').value }).catch((error) => toast(error.message));
  }
});

/** Saves `data` (a Blob, or bytes) as `name` through Chrome's downloads and answers once Chrome says the
 * file is complete and on disk (`{filename}`, its full path), or throws: what removes a
 * copy here on the strength of the file waits for this. */
async function savedOnDisk(data, name) {
  // A Blob as it is (no copy of what may be gigabytes), bytes in one.
  const blob = data instanceof Blob ? data : new Blob([data], { type: 'application/octet-stream' });
  const url = URL.createObjectURL(blob);
  try {
    const id = await chrome.downloads.download({ url, filename: downloadName(name), saveAs: false });
    const done = await new Promise((resolve, reject) => {
      const check = async () => {
        const [item] = await chrome.downloads.search({ id });
        if (item?.state === 'complete') return finish(resolve, item);
        if (!item || item.state === 'interrupted') return finish(reject, new Error(item?.error === 'USER_CANCELED' ? 'The save was cancelled; nothing was removed.' : `The file was not saved${item?.error ? ` (${item.error.toLowerCase().replace(/_/g, ' ')})` : ''}; nothing was removed.`));
      };
      const listener = (change) => { if (change.id === id && change.state) check().catch((error) => finish(reject, error)); };
      const finish = (settle, value) => { chrome.downloads.onChanged.removeListener(listener); settle(value); };
      chrome.downloads.onChanged.addListener(listener);
      check().catch((error) => finish(reject, error));
    });
    const [item] = await chrome.downloads.search({ id });
    if (!item?.exists || item.bytesReceived !== blob.size) throw new Error('The saved file could not be found whole on disk; nothing was removed.');
    return { filename: done.filename };
  } finally {
    setTimeout(() => URL.revokeObjectURL(url), 60_000);
  }
}

/** ChatGPT's own page where the user reviews and limits what apps use of their plan. */
function openUsage() {
  chrome.tabs.create({ url: CHATGPT_USAGE }).catch((error) => toast(error.message));
}

/** Said once, after the first Sign in with ChatGPT here: what using the plan means. */
function planWelcome() {
  const dialog = $('plan-dialog');
  $('plan-usage').onclick = () => {
    openUsage();
    dialog.close();
  };
  dialog.showModal();
}

function confirmed(title, text, yes = 'Confirm') {
  $('confirm-title').textContent = title;
  $('confirm-text').textContent = text;
  $('confirm-yes').textContent = yes;
  const dialog = $('confirm-dialog');
  dialog.returnValue = '';
  dialog.showModal();
  return new Promise((resolve) => dialog.addEventListener('close', () => resolve(dialog.returnValue === 'yes'), { once: true }));
}

/** Every file of a thread's workspace, as one .zip. */
async function downloadAll(id, files, button = $('files-zip')) {
  button.disabled = true;
  try {
    const read = [];
    for (const file of files) read.push({ path: file.path, bytes: (await ask('read', { id, path: file.path })).bytes });
    await download(zip(read), `${(threads.get(id)?.title || 'thread').replace(/[^\p{L}\p{N}]+/gu, '-').replace(/^-+|-+$/g, '').slice(0, 60) || 'thread'}.zip`);
  } catch (error) {
    toast(`The files could not be put together: ${error.message}`);
  } finally {
    button.disabled = false;
  }
}

function download(bytes, name) {
  return saveFile(bytes, name).catch((error) => toast(`${name} was not downloaded: ${error.message}`));
}

async function exportThread(keep) {
  const id = current;
  $('thread-dialog').close();
  if (!keep && !(await confirmed('Move this thread to a file?', 'The thread moves into a .spindle file: the same thread, with its history and workspace. This copy is sealed and stays only as a record. Continue it with `spindle import-session FILE`, or import the file into Spindle in any browser.', 'Move it'))) return;
  try {
    const { bytes, name } = await ask('export', { id, keep });
    download(bytes, name);
    toast(keep ? `Exported a copy: ${name}` : `Moved to ${name}`);
  } catch (error) {
    toast(error.message);
  }
}

$('menu-export').onclick = () => exportThread(false);
$('menu-host').onclick = () => runOnHost();
$('menu-back').onclick = () => bringBack();

/** Run on my host: on the thread's own model when the host has it signed in, else on one
 * of the host's the user picks; then the move, which the thread shows as it goes. */
async function runOnHost() {
  const id = current;
  $('thread-dialog').close();
  if (!id) return;
  let check;
  try {
    check = await ask('hostPreflight', { id });
  } catch (error) {
    toast(error.message);
    return;
  }
  if (check.why) {
    toast(`It cannot move to your host: ${check.why}.`);
    return;
  }
  const select = $('host-model');
  select.replaceChildren();
  if (!check.fits) {
    for (const provider of check.providers) {
      const group = document.createElement('optgroup');
      group.label = provider.name;
      for (const model of provider.models) {
        const option = el('option', '', model.label);
        option.value = JSON.stringify([provider.id, model.id]);
        option.selected = model.id === provider.default_model && !select.value;
        group.append(option);
      }
      select.append(group);
    }
    if (!select.options.length) {
      toast(`Your host has no model signed in: sign in to one on your host, then move the thread.`);
      return;
    }
  }
  $('host-model-field').hidden = check.fits;
  $('host-dialog-text').textContent = [
    `It moves to your host at ${check.url} with its history and workspace, and your host runs it from then on: its model,`,
    `its programs and its sandbox. This panel still shows and drives it, and answers its page tools while ${BROWSER} is open.`,
    check.fits ? '' : `${check.model} is not signed in there: choose the host's model it runs on.`,
    'This browser keeps its copy until your host holds the thread, so nothing is lost if either stops meanwhile.',
    'Bring back to this browser, in its menu, moves it back.',
  ].filter(Boolean).join(' ');
  const dialog = $('host-dialog');
  dialog.returnValue = '';
  dialog.showModal();
  const yes = await new Promise((resolve) => dialog.addEventListener('close', () => resolve(dialog.returnValue === 'yes'), { once: true }));
  if (!yes) return;
  const [provider, model] = check.fits ? [] : JSON.parse(select.value);
  try {
    await ask('toHost', { id, ...(provider && { provider, model }) });
  } catch (error) {
    toast(error.message);
  }
}

/** Bring back to this browser: what changes for it to run here is said first; then the
 * move, which the thread shows as it goes. */
async function bringBack() {
  const id = current;
  $('thread-dialog').close();
  if (!id) return;
  let said = [];
  try {
    ({ said } = await ask('backPreview', { id }));
  } catch (error) {
    toast(error.message);
    return;
  }
  if (!(await confirmed(
    'Keep this thread here?',
    `It comes back from your host with its history and workspace${said.length ? `, and to run in this browser it changes: ${said.join('; ')}` : ''}. Your host keeps its copy, sealed, until this browser holds the thread.`,
    'Bring it back',
  ))) return;
  try {
    await ask('backHere', { id });
  } catch (error) {
    toast(error.message);
  }
}
$('menu-copy').onclick = () => exportThread(true);
$('menu-delete').onclick = () => {
  $('thread-dialog').close();
  deleteThread(current);
};

$('menu-files').onclick = () => {
  $('thread-dialog').close();
  openWorkspace(current);
};

$('import-file').onchange = async (event) => {
  const file = event.target.files[0];
  event.target.value = '';
  if (!file) return;
  // Before it is read (BUNDLE_MOST: a larger one would end the engine rather than fail).
  if (file.size > BUNDLE_MOST) {
    toast(`${file.name} is ${size(file.size)}: a thread file over ${size(BUNDLE_MOST)} is too large to bring into this browser (a native Spindle takes it).`);
    return;
  }
  try {
    const { id, said = [] } = await ask('import', { bytes: new Uint8Array(await file.arrayBuffer()), name: file.name });
    // A thread that runs differently here (one from a native host, or set up for one by
    // a move): said, and kept only on a yes.
    if (said.length && !(await confirmed(
      'Keep this thread here?',
      `To run in this browser, ${file.name} changes: ${said.join('; ')}. Nothing has run yet; not keeping it deletes it here (the file stays).`,
      'Keep it',
    ))) {
      try {
        await ask('remove', { id });
        toast(`${file.name} was not kept.`);
      } catch (error) {
        await openThread(id);
        toast(`${file.name} is still here: it could not be deleted (${error.message}).`);
      }
      return;
    }
    await openThread(id);
    toast(`Imported ${file.name}.`);
  } catch (error) {
    toast(`${file.name} was not imported: ${error.message}`);
  }
};

// ---- Little things ---------------------------------------------------------------

for (const button of document.querySelectorAll('[data-close]')) button.onclick = () => $(button.dataset.close).close();

let toastTimer = 0;
/** A short message, above everything (an open page too: the toast is a popover, shown
 * again so it is the newest thing in the top layer); `action`: [label, run], a button in
 * it (Undo, say); `ms`: how long it stays. */
function toast(text, { action = null, ms = 5000 } = {}) {
  const node = $('toast');
  $('toast-text').textContent = explain(text);
  const button = $('toast-action');
  button.hidden = !action;
  button.textContent = action?.[0] ?? '';
  if (action) {
    button.onclick = () => {
      hideToast();
      action[1]();
    };
  }
  node.hidden = false;
  try {
    if (node.matches(':popover-open')) node.hidePopover();
    node.showPopover();
  } catch {}
  clearTimeout(toastTimer);
  toastTimer = setTimeout(hideToast, ms);
}

function hideToast() {
  clearTimeout(toastTimer);
  const node = $('toast');
  node.hidden = true;
  try { node.hidePopover(); } catch {}
}

function banner(text, actions = []) {
  $('banner').hidden = !text;
  $('banner-text').textContent = text ?? '';
  $('banner-actions').replaceChildren(...actions.map(([label, run]) => {
    const button = el('button', '', label);
    button.type = 'button';
    button.onclick = run;
    return button;
  }));
}

// What tests read: the panel's own state, never a key.
globalThis.spindlePanel = { get engine() { return engine; }, get current() { return current; }, get status() { return status; }, threads };

// "5m ago" keeps up on the thread list.
setInterval(() => { if (!current) renderThreads(); }, 60_000);
// The page's own icons, drawn once.
fillIcons();
$('home').append(icon('back', 18));
$('settings').append(icon('sliders', 18));
$('ws-done').append(icon('back', 18));
$('attach').append(icon('clip', 17));
$('shoot').append(icon('camera', 17));
$('carry-drop').append(icon('x', 12));
$('jump').append(icon('down', 12), document.createTextNode('Latest'));
for (const button of document.querySelectorAll('[data-close]')) button.append(icon(button.closest('dialog.page') ? 'back' : 'x', 18));
setAdd(false);
renderSend();
// The toast keeps above the composer, however tall it grows.
new ResizeObserver(() => document.documentElement.style.setProperty('--dock-h', `${$('dock').offsetHeight}px`)).observe($('dock'));
// Providers' keys only (the vault's own records, such as the host's token, start with `:`).
const providerKeys = async () => new Set((await keysOf('keys')).filter((name) => !name.startsWith(':')));
keyed = await providerKeys();
// Another window's panel may have saved a key.
addEventListener('focus', async () => {
  keyed = await providerKeys();
  renderBar();
});
showHome();
await connect();
