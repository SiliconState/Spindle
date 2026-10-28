// The panel: Spindle's UI in Chrome's side panel. It holds no session; the engine (an
// offscreen document's worker) runs them, and this page asks it over the channel and
// draws what it reports. Closing the panel never stops a turn.
import { ago, markdown, size } from './text.js';
import { PAGE_ACCESS, PICTURE_ACCESS, pageSwitches } from './page-tools.mjs';
import { zip } from './zip.mjs';
import { kindOf, pageOf, previewFrame, saveFile, typeOf } from './page.mjs';
import { mdNodes } from './markdown.mjs';
import { CHANNEL, accessFor, keysOf, read, remove, startEngine, write } from './shared.mjs';
import { flowOf, signInInTab, signInSites } from './signin.mjs';
import { describe, duration, isScratch, partsFrom, retried, tabsFrom } from './steps.mjs';

const $ = (id) => document.getElementById(id);
const el = (tag, cls, text) => {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
};

/** Small line icons (24-unit paths, drawn in the text's colour). */
const ICONS = {
  eye: ['M1.5 12s4-7 10.5-7 10.5 7 10.5 7-4 7-10.5 7S1.5 12 1.5 12z', 'M12 9a3 3 0 1 0 0 6 3 3 0 1 0 0-6z'],
  'eye-off': ['M1.5 12s4-7 10.5-7c2 0 3.7.6 5.2 1.6M22.5 12s-4 7-10.5 7c-2 0-3.7-.6-5.2-1.6', 'M3 3l18 18'],
  download: ['M12 4v11', 'M7 10l5 5 5-5', 'M5 20h14'],
  workspace: ['M3 4h18v16H3z', 'M9 4v16', 'M12 9h6', 'M12 13h6'],
  external: ['M14 4h6v6', 'M20 4l-9 9', 'M18 14v6H4V6h6'],
  taller: ['M12 3v18', 'M8 7l4-4 4 4', 'M8 17l4 4 4-4'],
  shorter: ['M12 3v7', 'M12 14v7', 'M8 6l4 4 4-4', 'M8 18l4-4 4 4'],
  copy: ['M9 9h11v11H9z', 'M5 15H4V4h11v1'],
  check: ['M5 12.5l4.5 4.5L19 7'],
  stop: [],
  send: ['M12 19V5', 'M6 11l6-6 6 6'],
  globe: ['M12 3a9 9 0 1 0 0 18 9 9 0 1 0 0-18z', 'M3 12h18', 'M12 3c3 3.5 3 14.5 0 18', 'M12 3c-3 3.5-3 14.5 0 18'],
  folder: ['M3 6.5h6l2 2h10v10.5H3z'],
  prev: ['M15 5l-7 7 7 7'],
  next: ['M9 5l7 7-7 7'],
};
function icon(name, size = 16) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('width', size);
  svg.setAttribute('height', size);
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('class', `icon icon-${name}`);
  if (name === 'stop') {
    const square = document.createElementNS('http://www.w3.org/2000/svg', 'rect');
    for (const [key, value] of Object.entries({ x: 6, y: 6, width: 12, height: 12, fill: 'currentColor' })) square.setAttribute(key, value);
    svg.append(square);
    return svg;
  }
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '1.8');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  for (const d of ICONS[name] ?? []) {
    const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    path.setAttribute('d', d);
    svg.append(path);
  }
  return svg;
}
/** A button showing only an icon, its words in its tooltip and accessible name. */
function iconButton(name, label, run, cls = 'icon-button') {
  const button = el('button', cls);
  button.type = 'button';
  button.title = label;
  button.setAttribute('aria-label', label);
  button.append(icon(name));
  if (run) button.onclick = run;
  return button;
}
/** A button copying `text()` to the clipboard, a check for a moment once it did. */
function copyButton(text, label) {
  const button = iconButton('copy', label, async (event) => {
    event.stopPropagation();
    try {
      await navigator.clipboard.writeText(text());
      button.replaceChildren(icon('check'));
      button.classList.add('copied');
      setTimeout(() => {
        button.replaceChildren(icon('copy'));
        button.classList.remove('copied');
      }, 1400);
    } catch (error) {
      toast(`It was not copied: ${error.message}`);
    }
  }, 'icon-button copy');
  return button;
}

/** How long the steps seen running here took: tool call id -> ms (for this browser
 * session; a thread opened later shows what it knows). */
const timings = (() => {
  try {
    return new Map(JSON.parse(sessionStorage.getItem('spindle-step-times') || '[]'));
  } catch {
    return new Map();
  }
})();
function keepTiming(id, ms) {
  if (!id) return;
  timings.set(id, ms);
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
const waiting = new Map();
let next = 0;
/** The engine's last snapshot (catalog, threads, info), null until it answers. */
let engine = null;

/** How long an answer may take: moving whole sessions, longer. */
const PATIENCE = { export: 300_000, import: 300_000, catalog: 120_000 };

function ask(op, args = {}, timeout = PATIENCE[op] ?? 60_000) {
  const id = ++next;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      waiting.delete(id);
      reject(Object.assign(new Error("Spindle's engine did not answer"), { kind: 'timeout' }));
      // Past the first contact, silence means the engine stopped working.
      if (engine) banner("Spindle's engine is not answering.", [['Restart engine', restartEngine]]);
    }, timeout);
    waiting.set(id, { resolve, reject, timer });
    channel.postMessage({ to: 'engine', from: me, id, op, args });
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
    if ($('settings-dialog').open) renderProviders();
  }
  else if (data?.type === 'ready') {
    if (engine) engineRestarted();
    else connect();
  }
};


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
    if (!engine) banner("Spindle's engine is not answering.", [['Restart engine', restartEngine]]);
  })().finally(() => { connecting = null; });
  return connecting;
}

async function restartEngine() {
  await chrome.offscreen.closeDocument().catch(() => {});
  engineRestarted();
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
      'go back to before one of your messages (↺ beside it), or start a new thread with its files (the thread menu)')
    .replace(/rewind to a target first \(`spindle targets`\)/g, 'go back to before one of your messages first (↺ beside it)')
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

function renderThreads() {
  const list = $('threads');
  list.replaceChildren();
  const sorted = [...threads.values()].sort((a, b) => b.updated - a.updated);
  $('intro').hidden = sorted.length > 0;
  for (const thread of sorted) {
    const item = el('div', 'thread-item');
    const row = el('button', `thread-row state-${thread.state}`);
    row.type = 'button';
    row.dataset.id = thread.id;
    row.append(el('strong', '', thread.title), el('small', '', `${nameOf(thread.provider)} · ${thread.model} · ${STATES[thread.state] ?? thread.state} · ${ago(thread.updated)}`));
    row.onclick = () => openThread(thread.id);
    const drop = el('button', 'thread-remove', '×');
    drop.type = 'button';
    drop.title = 'Delete this thread';
    drop.setAttribute('aria-label', `Delete “${thread.title}”`);
    drop.onclick = () => deleteThread(thread.id);
    item.append(row, drop);
    list.append(item);
  }
}

/** Deletes a thread once the user confirms (a running one must be stopped first). */
async function deleteThread(id) {
  const thread = threads.get(id);
  if (!thread) return;
  if (thread.state === 'running') {
    toast('Stop this thread before deleting it.');
    return;
  }
  const what = thread.state === 'moved'
    ? `“${thread.title}” and the file kept for it are removed from this browser. Wherever you imported the file, the thread goes on.`
    : `“${thread.title}” and its workspace are removed from this browser. This cannot be undone; move it to a file first to keep it.`;
  if (!(await confirmed('Delete this thread?', what, 'Delete'))) return;
  try {
    await ask('remove', { id });
  } catch (error) {
    toast(error.message);
  }
}

function onThread(thread) {
  const before = threads.get(thread.id);
  threads.set(thread.id, thread);
  renderThreads();
  if (thread.id === current) {
    $('title').textContent = thread.title;
    $('subtitle').textContent = `${nameOf(thread.provider)} · ${thread.model}`;
    if (['state', 'note', 'provider', 'model', 'runner', 'network'].some((key) => before?.[key] !== thread[key])) renderBar();
    // Moved out while open: show what is left here, and the file.
    if (thread.state === 'moved' && before && before.state !== 'moved') openThread(thread.id);
  }
}

function onRemoved(id) {
  threads.delete(id);
  renderThreads();
  if (id === current) showHome();
}

/** What the brand button in the header says it does (a back button in a thread). */
function homeLabel(name, tip) {
  $('home').setAttribute('aria-label', name);
  $('home').title = tip;
}

let historyPaint = 0;

function showHome() {
  historyPaint += 1;
  dropKept();
  current = null;
  live = null;
  document.body.dataset.view = 'home';
  $('welcome').hidden = false;
  $('thread').hidden = true;
  $('workspace').hidden = true;
  $('dock').hidden = false;
  homeLabel('Spindle', '');
  $('title-button').hidden = true;
  $('title').textContent = 'Spindle';
  $('subtitle').textContent = 'Agent threads that live in Chrome.';
  $('prompt').placeholder = 'Start a new thread…';
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

async function openThread(id) {
  const paint = ++historyPaint;
  dropKept();
  current = id;
  live = null;
  pendingCards = [];
  status = null;
  shown = { total: 0, prompts: [], turnFrom: null, compacted: false };
  carry(null);
  const thread = threads.get(id);
  document.body.dataset.view = 'thread';
  $('welcome').hidden = true;
  $('thread').hidden = false;
  $('workspace').hidden = true;
  $('dock').hidden = false;
  homeLabel('All threads', 'All threads');
  knownTabs = new Map();
  knownParts = new Map();
  $('title-button').hidden = false;
  $('title').textContent = thread?.title ?? 'Thread';
  $('subtitle').textContent = thread ? `${nameOf(thread.provider)} · ${thread.model}` : '';
  $('prompt').placeholder = 'Reply, or steer the running turn…';
  $('history').replaceChildren(el('p', 'note', 'Opening…'));
  renderBar();
  try {
    const data = await ask('open', { id });
    if (current !== id || historyPaint !== paint) return;
    status = data.status;
    renderHistory(data);
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

/** Paths the last turn shown so far wrote with its file tools (each has its card), and
 * those cards (the last turn's open their previews; earlier ones wait to be asked). */
let turnPaths = new Set();
let turnCards = [];
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
function renderHistory({ thread, history, running, bundle }, { from = 0 } = {}) {
  const box = $('history');
  pendingCards = [];
  if (!history) {
    box.replaceChildren();
    const notice = el('div', 'notice state');
    notice.append(el('p', '', 'This thread moved into a .spindle file; this copy is sealed. Continue it in native Spindle with `spindle --dir DIR import-session FILE`, or import the file into Spindle in a browser.'));
    box.append(notice);
    if (bundle) {
      // A real link, so the click itself saves it (Chrome holds back a second
      // download a page starts on its own).
      ask('bundle', { id: thread.id }).then((kept) => {
        if (current !== thread.id) return;
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
      box.append(earlierButton(thread, messages.slice(0, start)));
    }
  }
  // One workspace card, for the workspace as it is now: an earlier one goes.
  for (const card of box.querySelectorAll('.workspace-card')) card.remove();
  turnPaths = new Set();
  turnCards = [];
  const pictures = renderMessages(box, thread, messages.slice(start), from + start, { running, main: true });
  const end = el('i', 'history-end');
  end.hidden = true;
  box.append(end);
  shown.total = history.total ?? from + messages.length;
  shown.turnFrom = null;
  shown.compacted = false;
  // The last turn's pages, pictures and Markdown open (a few: each page is a frame).
  turnCards.filter((card) => card.preview && card.isConnected).slice(-3).forEach((card) => card.preview());
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
  scrollDown(from === 0);
  return true;
}

/** A user's message: text they wrote (not a tool's result, a summary or steering). */
function isPrompt(message) {
  return message.role === 'user' && (message.content ?? []).some((block) => block.type === 'text' && typeof block.steer !== 'string' && block.text.trim() && !block.text.startsWith(COMPACTED));
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
        if (typeof block.steer === 'string') box.append(messageNode('user', block.steer, 'YOU · WHILE IT RAN'));
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
  head.append(el('span', 'group-status'), el('span', 'group-label'), el('span', 'group-time'), el('span', 'group-chevron'));
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

/** Opens or shuts a step group without counting it as the user's doing. */
function setOpen(group, open) {
  group.wanted = open;
  group.open = open;
}

/** A run of steps says what it holds: how many, how many failed (a failed one done again
 * successfully later counts as retried), and the step running now. */
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
    }
  });
  const runningCard = cards.find((card) => card.classList.contains('running'));
  const failed = list.filter((step, index) => step.failed && !again.has(index)).length;
  const total = cards.reduce((sum, card) => sum + (card.ms ?? 0), 0);
  const count = `${cards.length} step${cards.length === 1 ? '' : 's'}`;
  const words = runningCard
    ? [`${runningCard.querySelector('.tool-label').textContent}…`, `step ${cards.indexOf(runningCard) + 1} of ${cards.length}`]
    : [`Ran ${count}`, failed && `${failed} failed`, again.size && `${again.size} retried`];
  steps.querySelector('.group-label').textContent = words.filter(Boolean).join(' · ');
  steps.querySelector('.group-time').textContent = !runningCard && total ? duration(total) : '';
  steps.group.querySelector('.group-head').title = runningCard ? 'Steps of the running turn' : `${count}${failed ? `, ${failed} failed` : ''}: show or hide them`;
  steps.classList.toggle('running', Boolean(runningCard));
  steps.classList.toggle('has-failed', failed > 0);
}

/** The button that draws the earlier part of a long history (`messages`, from its
 * first) in its place. */
function earlierButton(thread, messages) {
  const button = el('button', 'earlier-button wide', `Show ${messages.length} earlier messages`);
  button.type = 'button';
  button.onclick = () => {
    const part = document.createDocumentFragment();
    renderMessages(part, thread, messages, 0);
    shown.prompts = [...messages.flatMap((message, index) => (isPrompt(message) ? [index] : [])), ...shown.prompts];
    button.replaceWith(part);
  };
  return button;
}

/** Where a compaction folded earlier messages into a summary: says so, shows the summary
 * the model goes on from, and the messages themselves when asked (from the history at
 * the point before it). */
function compactionNode(thread, text, revision) {
  const node = el('div', 'notice compacted');
  node.append(el('p', '', "Earlier messages were folded into a summary so this thread fits its model's context. The model goes on from the summary; the workspace is as it was."));
  const summary = el('details');
  summary.append(el('summary', '', 'The summary'), el('pre', 'summary-text', text.slice(text.indexOf('\n') + 1).trim()));
  const show = el('button', '', 'Show the earlier messages');
  show.type = 'button';
  show.onclick = async () => {
    show.disabled = true;
    show.textContent = 'Reading…';
    try {
      const earlier = await ask('earlier', { id: thread.id, revision });
      const section = el('section', 'earlier');
      section.append(el('span', 'role-label', 'BEFORE THE SUMMARY'));
      renderMessages(section, thread, earlier.history.messages ?? [], 0, { revision: earlier.revision });
      node.before(section);
      show.remove();
    } catch (error) {
      show.disabled = false;
      show.textContent = 'Show the earlier messages';
      toast(error.message);
    }
  };
  node.append(summary, show);
  return node;
}

/** ↺ beside a user's message: back to just before it. */
function rewindButton(thread, at, revision) {
  const button = el('button', 'rewind', '↺');
  button.type = 'button';
  button.title = 'Go back to before this message (the conversation and the workspace); the message returns to the box';
  button.setAttribute('aria-label', 'Go back to before this message');
  button.onclick = () => rewindTo(thread.id, at, revision);
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
    if (current === id) {
      await openThread(id);
      toComposer(text, files);
    }
    toast('Back to before that message: it is in the box below.');
  } catch (error) {
    toast(error.message);
  }
}

/** A message and its files back in the box, to change or send again. */
function toComposer(text, files) {
  $('prompt').value = text;
  autosize();
  attached = files.map(({ path, bytes }) => ({ name: path, size: bytes.length, file: new File([bytes], path.split('/').pop()) }));
  renderAttached();
  $('prompt').focus();
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
  const article = el('article', `message-wrap ${role}${label ? ' steer' : ''}`);
  const head = el('span', 'role-label', label ?? (role === 'user' ? 'YOU' : 'SPINDLE'));
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

/** Table cells holding a short token (a handle, a ticker, a number) keep it on one line;
 * a table wider than the panel scrolls. */
function tidyTables(box) {
  for (const cell of box.querySelectorAll('td, th')) {
    if (cell.textContent.trim().length <= 28) cell.classList.add('nowrap');
  }
}

/** A step's row: what it does in plain words (in the tense of where it is), the page a
 * browser step acted on, how long it took; opened, its full command (to copy) and what it
 * gave back. `id`: its call's id; `live`: seen starting now (it is timed). */
function toolCard(name, input, id = null, live = false) {
  const card = el('details', 'tool running');
  card.dataset.tool = name;
  if (id) card.dataset.id = id;
  card.input = input;
  if (live) card.started = Date.now();
  const summary = el('summary');
  summary.append(el('span', 'tool-status'), el('span', 'tool-icon'), el('span', 'tool-label'), el('span', 'tool-time'),
    el('span', 'tool-name', name), el('span', 'tool-state', 'running'));
  card.append(summary);
  const detail = el('div', 'tool-detail');
  const command = inputText(name, input);
  if (command) {
    const box = el('div', 'tool-input');
    box.append(el('pre', 'input', command), copyButton(() => command, name === 'bash' ? 'Copy the command' : name === 'write_file' ? 'Copy what it wrote' : 'Copy the input'));
    detail.append(box);
  }
  card.append(detail);
  card.detail = detail;
  relabel(card);
  return card;
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

/** A step's row says what it does, as it stands. */
function relabel(card) {
  const state = card.classList.contains('running') ? 'running' : card.classList.contains('failed') ? 'failed' : 'done';
  const { label, url, title, domain } = describe(card.dataset.tool, card.input ?? {}, state, knownTabs, knownParts);
  card.querySelector('.tool-label').textContent = label;
  const summary = card.querySelector('summary');
  summary.title = [label, title && title !== domain ? title : null, url].filter(Boolean).join('\n');
  const place = card.querySelector('.tool-icon');
  const browser = /^(read_page|page_elements|page_html|screenshot|navigate|click|type|select|press|scroll|wait_for|dialog|close_tab|network|list_tabs)$/.test(card.dataset.tool);
  place.hidden = !browser;
  if (browser && url && place.dataset.url !== url) {
    place.dataset.url = url;
    const image = el('img', 'favicon');
    image.alt = '';
    image.src = faviconOf(url);
    image.onerror = () => place.replaceChildren(icon('globe', 14));
    place.replaceChildren(image);
  } else if (browser && !url && !place.firstChild) {
    place.replaceChildren(icon('globe', 14));
  }
  card.querySelector('.tool-time').textContent = card.ms != null ? duration(card.ms) : card.started ? duration(Date.now() - card.started) : '';
}

/** The little icon Chrome keeps for a site (the extension's favicon service). */
const faviconOf = (url) => chrome.runtime.getURL(`_favicon/?pageUrl=${encodeURIComponent(url)}&size=32`);

/** A step's state in words (read by screen readers; shown when it failed). */
function stateText(card) {
  const failed = card.classList.contains('failed');
  card.querySelector('.tool-state').textContent = card.classList.contains('running') ? 'running'
    : failed && card.classList.contains('retried') ? 'failed → retried ✓' : failed ? 'failed' : 'done';
}

function settle(card, text, failed) {
  card.classList.remove('running');
  card.classList.toggle('failed', failed);
  if (card.started) {
    card.ms = Date.now() - card.started;
    keepTiming(card.dataset.id, card.ms);
  } else if (card.dataset.id && timings.has(card.dataset.id)) {
    card.ms = timings.get(card.dataset.id);
  }
  const pre = el('pre', 'result', text.length > 20000 ? `${text.slice(0, 20000)}\n[…]` : text);
  card.detail.append(pre);
  // What a step found out about tabs and pages (one that failed found out nothing).
  if (!failed) {
    const told = [{ name: card.dataset.tool, input: card.input, result: text }];
    for (const [tab, known] of tabsFrom(told)) knownTabs.set(tab, known);
    for (const [ref, part] of partsFrom(told)) knownParts.set(ref, part);
  }
  relabel(card);
  stateText(card);
  const steps = card.closest('.steps');
  if (steps) updateSteps(steps);
  const shot = card.dataset.tool === 'screenshot' && !failed && /^Saved (screenshots\/\S+\.jpg)/.exec(text)?.[1];
  if (shot && current) picture(card, current, shot);
}

// The running steps' times go on as they run.
setInterval(() => {
  for (const card of document.querySelectorAll('#history .tool.running')) {
    if (card.started) card.querySelector('.tool-time').textContent = duration(Date.now() - card.started);
  }
}, 1000);

/** A screenshot a tool saved, shown in its card (read from the thread's workspace; a
 * just-taken one may land there a moment after its result). */
async function picture(card, id, path, tries = 5) {
  try {
    const { bytes } = await ask('read', { id, path });
    const url = URL.createObjectURL(new Blob([bytes], { type: 'image/jpeg' }));
    const image = el('img', 'shot');
    image.alt = `Screenshot ${path}`;
    image.onload = image.onerror = () => URL.revokeObjectURL(url);
    image.src = url;
    card.detail.append(image);
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

function onEvent(id, event) {
  if (id !== current) return;
  const box = $('history');
  switch (event.type) {
    case 'prompt':
      historyPaint += 1;
      box.append(messageNode('user', event.text));
      for (const card of picturesWith(id, event.text, box)) card.preview();
      break;
    case 'steered':
      box.append(messageNode('user', event.text, 'YOU · WHILE IT RAN'));
      scrollDown(true);
      break;
    case 'model_step':
      endLive(live?.text);
      startLive();
      phase(`${event.model} is writing…`, 'busy');
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
      updateSteps(steps);
      // The running turn's steps show unless the user shut them.
      if (!steps.group.dataset.user) setOpen(steps.group, true);
      phase(`${card.querySelector('.tool-label').textContent}…`, 'busy');
      break;
    }
    case 'tool_result': {
      const card = pendingCards.shift();
      if (card) {
        settle(card, event.content, event.failed);
        const made = !event.failed && wroteFile(card);
        if (made) placeFile(card, made);
      }
      break;
    }
    case 'tool_refused': {
      const card = pendingCards.shift();
      if (card) settle(card, `refused: ${event.reason}`, true);
      break;
    }
    case 'provider_retry':
      endLive(null);
      phase(`the provider answered ${event.status}; trying again in ${event.seconds}s`, 'busy');
      break;
    case 'stopped':
      endLive(live?.text);
      break;
    case 'nested': {
      // A script's own Spindle calls belong with the step running it.
      const steps = box.lastElementChild?.classList.contains('steps') ? box.lastElementChild : null;
      (steps?.list ?? box).append(el('p', 'notice nested', event.label));
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
  if (id !== current) return;
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

/** Where a file a step wrote shows: a page, picture or Markdown file below its run of
 * steps, in sight; any other file inside the run, after its step. One card per file in a
 * run: the latest. */
function placeFile(card, made) {
  const steps = card.closest('.steps');
  if (!steps) {
    card.after(made);
    return;
  }
  for (const old of steps.querySelectorAll(`.file-card[data-path="${CSS.escape(made.dataset.path)}"]`)) old.remove();
  if (['page', 'picture', 'markdown'].includes(kindOf(made.dataset.path))) steps.append(made);
  else card.after(made);
}

/** The card of the file a file tool wrote (write_file, edit_file), or null; `preview`:
 * a page, picture or Markdown file shows itself at once (as a live step's does). */
function wroteFile(card, preview = true) {
  const path = workspacePath(card.input?.path);
  if (!current || !path || !['write_file', 'edit_file'].includes(card.dataset.tool)) return null;
  turnPaths.add(path);
  const presentable = ['page', 'picture', 'markdown'].includes(kindOf(path));
  return fileCard(current, path, { content: card.dataset.tool === 'write_file' ? card.input.content : null, preview: preview && presentable });
}

/** A file of thread `id` in the conversation: its name, a preview (a page in a sandboxed
 * frame, a picture, Markdown, text), Open (a page or picture in a tab of its own) and
 * Download. `content`: its text as a step wrote it, when at hand. */
function fileCard(id, path, { content = null, size: bytes = null, preview = false } = {}) {
  const kind = kindOf(path);
  const card = el('div', `file-card kind-${kind ?? 'bytes'}`);
  card.dataset.path = path;
  const head = el('div', 'file-head');
  const name = el('span', 'file-name', path);
  name.title = path;
  const meta = el('small', 'file-meta', [{ page: 'page', picture: 'picture', markdown: 'Markdown', text: 'text' }[kind] ?? 'file', bytes != null ? size(bytes) : null].filter(Boolean).join(' · '));
  const actions = el('span', 'file-actions');
  const action = (name, title, run) => {
    const button = iconButton(name, title, run);
    actions.append(button);
    return button;
  };
  const body = el('div', 'file-body');
  body.hidden = true;
  let filled = false;
  // A page reaches nothing past itself unless the user lets this one load from the web.
  let web = false;
  const fill = () => {
    filled = true;
    return previewInto(body, id, path, { content, web, allow: () => {
      web = true;
      fill();
    } });
  };
  let toggle = null;
  if (kind) {
    toggle = action('eye', `Show ${path} here`, () => {
      body.hidden = !body.hidden;
      const shown = !body.hidden;
      toggle.replaceChildren(icon(shown ? 'eye-off' : 'eye'));
      toggle.title = shown ? `Hide ${path}` : `Show ${path} here`;
      toggle.setAttribute('aria-label', toggle.title);
      toggle.setAttribute('aria-expanded', String(shown));
      if (shown && !filled) fill();
    });
    toggle.setAttribute('aria-expanded', 'false');
  }
  if (kind === 'page') {
    const taller = action('taller', 'Show more of the page here', () => {
      card.classList.toggle('tall');
      const tall = card.classList.contains('tall');
      taller.replaceChildren(icon(tall ? 'shorter' : 'taller'));
      taller.title = tall ? 'Show less of the page here' : 'Show more of the page here';
      taller.setAttribute('aria-label', taller.title);
    });
  }
  // Its own page in Spindle: the thread's workspace, with this file open.
  action('workspace', `Open ${path} in the workspace`, () => openWorkspace(id, path));
  action('download', `Download ${path}`, async () => {
    try {
      await saveFile(await readBytes(id, path), path.split('/').pop(), typeOf(path));
      toast(`Downloaded ${path.split('/').pop()}.`);
    } catch (error) {
      console.warn(`Spindle: ${path} was not downloaded:`, error);
      toast(`${path} was not downloaded: ${error.message}`);
    }
  });
  head.append(name, meta, actions);
  card.append(head, body);
  // Shows its preview (once, if it has one to show).
  if (toggle && ['page', 'picture', 'markdown'].includes(kind)) card.preview = () => body.hidden && toggle.click();
  if (preview) card.preview?.();
  return card;
}

/** A file's preview into `body`: a page in a sandboxed frame (`allow`: the button letting
 * it load from the web, unless `web` already does), a picture, Markdown, text; with
 * `any`, a file of no known kind shows as text when it is text. `content`: its text, when
 * at hand; `textId`: the id its text block takes. */
async function previewInto(body, id, path, { content = null, web = false, allow = null, any = false, textId = null } = {}) {
  const kind = kindOf(path);
  // A page's preview holds its height from the start: nothing below it jumps (nor what
  // the user is about to click) when it comes.
  body.replaceChildren(el('p', `note${kind === 'page' ? ' file-waiting' : ''}`, 'Reading…'));
  try {
    if (kind === 'page') {
      const { files } = await ask('files', { id });
      const page = await pageOf({ path, files, read: (name) => readBytes(id, name), html: content, web });
      const frame = previewFrame(page.html, path);
      frame.className = 'file-frame';
      body.replaceChildren(frame);
      if (page.web.length && !web && allow) body.prepend(webNote(page.web, allow));
    } else if (kind === 'picture') {
      const url = URL.createObjectURL(new Blob([await readBytes(id, path)], { type: typeOf(path) }));
      const image = el('img', 'file-picture');
      image.alt = path;
      image.onload = image.onerror = () => URL.revokeObjectURL(url);
      image.src = url;
      body.replaceChildren(image);
    } else if (kind || any) {
      const bytes = content == null ? await readBytes(id, path) : null;
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
        const pre = el('pre', 'code file-text', shown);
        if (textId) {
          pre.id = textId;
          pre.tabIndex = 0;
        }
        body.replaceChildren(pre);
      }
    }
  } catch (error) {
    body.replaceChildren(el('p', 'note', `It could not be shown: ${error.message}`));
  }
}

/** What a page would load from the web, closed to it, and the button that opens it to
 * the web for this preview (`allow`). */
function webNote(sites, allow) {
  const note = el('p', 'note file-web');
  const shown = sites.slice(0, 3).join(', ') + (sites.length > 3 ? ` and ${sites.length - 3} more` : '');
  note.append(`It would load from ${shown}; previews reach nothing outside the workspace. `);
  const button = el('button', 'link-like', 'Load from the web');
  button.type = 'button';
  button.title = 'Let this preview load from the web over HTTPS (it still cannot reach Spindle)';
  button.onclick = allow;
  note.append(button);
  return note;
}

/** The workspace at the end of the conversation: its files by folder, those the last turn
 * made or changed marked, scratch files (notes, scripts, tests) out of sight unless asked
 * for; a file opens in the workspace page, and all of them download as a .zip. */
function workspaceCard(id) {
  const card = el('details', 'workspace-card');
  const summary = el('summary', '', 'Workspace');
  card.append(summary);
  turnFiles(id).then(({ files, changes }) => {
    if (!files.length) {
      card.remove();
      return;
    }
    const fresh = files.filter((file) => changes.has(file.path)).length;
    // Folded until the user opens it: its summary says what is there, and an open list
    // would stand between the steps and the final answer.
    summary.textContent = `Workspace · ${files.length} file${files.length === 1 ? '' : 's'}${fresh ? ` · ${fresh} new or edited` : ''}`;
    const tree = el('div', 'ws-tree');
    const toggle = scratchSwitch(() => drawTree(tree, files, changes, { onOpen: (path) => openWorkspace(id, path) }));
    const tools = el('div', 'ws-tools');
    const open = el('button', 'link-like', 'Open the workspace');
    open.type = 'button';
    open.onclick = () => openWorkspace(id);
    tools.append(open, toggle);
    drawTree(tree, files, changes, { onOpen: (path) => openWorkspace(id, path) });
    const all = el('button', 'wide', 'Download all (.zip)');
    all.type = 'button';
    all.onclick = () => downloadAll(id, files, all);
    card.append(tools, tree, all);
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

/** `files` by folder into `box`: a row each (its name, new/edited, its size), scratch
 * files kept (hidden) unless shown; `filter` narrows by name; `onOpen(path)` opens one;
 * `links` adds a download (and, for pages and pictures, an open-in-a-tab) link per row.
 * Returns the paths shown, in order. */
function drawTree(box, files, changes, { onOpen, filter = '', links = null }) {
  box.replaceChildren();
  const wanted = filter.trim().toLowerCase();
  const folders = new Map();
  for (const file of [...files].sort((a, b) => a.path.localeCompare(b.path))) {
    const at = file.path.lastIndexOf('/');
    const folder = at < 0 ? '' : file.path.slice(0, at);
    if (!folders.has(folder)) folders.set(folder, []);
    folders.get(folder).push(file);
  }
  const shown = [];
  let hidden = 0;
  // The workspace's own files first, then each folder.
  for (const [folder, list] of [...folders].sort(([a], [b]) => (a === '' ? -1 : b === '' ? 1 : a.localeCompare(b)))) {
    const group = el('div', 'ws-folder');
    if (folder) {
      const head = el('div', 'ws-folder-name');
      const mark = el('span', 'ws-folder-icon');
      mark.append(icon('folder', 13));
      head.append(mark, document.createTextNode(`${folder}/`));
      group.append(head);
    }
    let visible = 0;
    for (const file of list) {
      const name = file.path.slice(folder ? folder.length + 1 : 0);
      const scratch = isScratch(file.path);
      const matches = !wanted || file.path.toLowerCase().includes(wanted);
      const row = el('div', `file-row${scratch ? ' scratch' : ''}`);
      row.dataset.path = file.path;
      row.hidden = !matches || (scratch && !showScratch);
      if (row.hidden && matches && scratch) hidden += 1;
      const open = el('button', 'file-open');
      open.type = 'button';
      open.title = `Open ${file.path}`;
      const label = el('span', 'file-row-name', name);
      open.append(label);
      const change = changes.get(file.path);
      if (change) open.append(el('span', `badge ${change}`, change));
      open.append(el('small', '', size(file.size)));
      open.onclick = () => onOpen(file.path);
      row.append(open);
      if (links) row.append(...links(file));
      group.append(row);
      if (!row.hidden) {
        visible += 1;
        shown.push(file.path);
      }
    }
    group.hidden = visible === 0;
    box.append(group);
  }
  if (!shown.length) box.append(el('p', 'note', wanted ? 'No file matches.' : files.length ? 'Only scratch files here.' : 'No files yet.'));
  if (hidden) {
    const more = el('button', 'link-like ws-hidden', `${hidden} scratch file${hidden === 1 ? '' : 's'} hidden: show`);
    more.type = 'button';
    more.onclick = () => {
      showScratch = true;
      try {
        localStorage.setItem('spindle-scratch', 'shown');
      } catch {}
      for (const toggle of document.querySelectorAll('.mini-switch')) toggle.setAttribute('aria-checked', 'true');
      drawTree(box, files, changes, { onOpen, filter, links });
    };
    box.append(more);
  }
  return shown;
}

function scrollDown(force = false) {
  // The workspace page scrolls in the same place: a turn going on behind it leaves it be.
  if (document.body.dataset.view === 'workspace') return;
  const main = $('main');
  if (force || main.scrollHeight - main.scrollTop - main.clientHeight < 160) main.scrollTop = main.scrollHeight;
  renderJump();
}

/** "Latest" shows while a turn runs and the conversation is scrolled up from its end. */
function renderJump() {
  const main = $('main');
  const away = main.scrollHeight - main.scrollTop - main.clientHeight > 200;
  const running = Boolean(current && threads.get(current)?.state === 'running');
  $('jump').hidden = !(running && away && !$('thread').hidden);
}
$('main').addEventListener('scroll', renderJump, { passive: true });
$('jump').onclick = () => {
  const main = $('main');
  main.scrollTo({ top: main.scrollHeight, behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' });
  $('prompt').focus({ preventScroll: true });
};

// ---- The bar and the composer ----------------------------------------------------

function phase(text, mode = '') {
  $('phase').textContent = text;
  $('bar').dataset.mode = mode;
}

function renderBar() {
  const thread = current ? threads.get(current) : null;
  const show = { resume: false, retry: false, takeback: false };
  const notice = $('notice');
  notice.hidden = true;
  if (!engine) {
    phase('starting Spindle…');
  } else if (!thread) {
    phase(chosen().provider ? '' : 'no providers');
  } else if (thread.state === 'running') {
    if (!$('phase').textContent || $('bar').dataset.mode !== 'busy') phase('working…', 'busy');
  } else if (['stopped', 'failed', 'interrupted'].includes(thread.state)) {
    show.resume = true;
    show.retry = true;
    show.takeback = true;
    phase(STATES[thread.state], 'stopped');
    notice.className = 'notice state warning';
    const note = explain(thread.note ?? STATES[thread.state]);
    notice.textContent = /Retry|Continue/.test(note)
      ? note
      : `${note}\n\nContinue picks up where it left off; Retry sends the last model request again.`;
    notice.hidden = false;
  } else if (thread.state === 'moved') {
    phase('moved out');
  } else {
    phase(status?.unresolved ? `${status.unresolved} step(s) with an unknown outcome` : '');
    if (thread.note) {
      notice.className = 'notice state';
      notice.textContent = explain(thread.note);
      notice.hidden = false;
    }
  }
  $('resume').hidden = !show.resume;
  $('retry').hidden = !show.retry;
  $('takeback').hidden = !show.takeback;
  const chip = thread ? { provider: thread.provider, model: thread.model, effort: thread.effort ?? 'default' } : chosen();
  const networked = thread ? thread.network && thread.runner !== 'none' : choice.network && choice.runner !== 'none';
  // The effort as what it does on this model (thinking off, a budget, an effort level).
  const effort = chip.provider ? effortOn(chip.provider, chip.model, chip.effort) : null;
  const effortWord = effort ? (effort.short === 'default' ? 'default effort' : effort.short) : null;
  // The model and its effort (the provider is in the title above and the tooltip).
  $('model-chip').textContent = chip.provider ? [chip.model, effortWord, networked && 'web'].filter(Boolean).join(' · ') : 'choose a provider';
  $('model-chip').title = [
    chip.provider && `${nameOf(chip.provider)} · ${chip.model}`,
    effort && `Effort: ${effort.label}`,
    networked && "This thread's commands reach the web",
    'Change the model, its effort, where commands run and their network',
  ].filter(Boolean).join('\n');
  $('model-chip').hidden = Boolean(thread && thread.state === 'moved');
  $('prompt').disabled = Boolean(thread && thread.state === 'moved');
  $('send').disabled = !engine || Boolean(thread && thread.state === 'moved');
  renderSend();
  renderJump();
}

/** The send button: Stop while the open thread's turn runs and nothing is written to
 * steer it with; Send otherwise (to the running turn, when one runs). */
function renderSend() {
  const thread = current ? threads.get(current) : null;
  const running = thread?.state === 'running';
  const stop = running && !$('prompt').value.trim() && !attached.length;
  const button = $('send');
  if (button.dataset.mode === (stop ? 'stop' : running ? 'steer' : 'send')) return;
  button.dataset.mode = stop ? 'stop' : running ? 'steer' : 'send';
  button.classList.toggle('stop', stop);
  button.classList.toggle('primary', !stop);
  button.replaceChildren(icon(stop ? 'stop' : 'send', 18));
  const label = stop ? 'Stop this turn' : running ? 'Send to the running turn' : 'Send';
  button.setAttribute('aria-label', label);
  button.title = label;
}

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
      banner(`Chrome could not ask for ${host} here (${error.message}). Open Spindle in a tab and try there.`, [['Open in a tab', openTab]]);
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
    const drop = el('button', '', '×');
    drop.type = 'button';
    drop.setAttribute('aria-label', `Leave out ${file.name}`);
    drop.onclick = () => {
      attached = attached.filter((_, index) => index !== at);
      renderAttached();
    };
    chip.append(el('span', '', `${file.name} · ${size(file.size)}`), drop);
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
    if (!(await asked)) throw new Error('Chrome did not let Spindle picture pages');
    const window = await chrome.windows.getCurrent();
    const [tab] = await chrome.tabs.query({ active: true, windowId: window.id });
    let url;
    try {
      url = await chrome.tabs.captureVisibleTab(window.id, { format: 'png' });
    } catch (error) {
      throw new Error(`Chrome gives no picture of ${tab?.url ? new URL(tab.url).protocol.replace(':', '') + ' pages' : 'this tab'} (${error.message})`);
    }
    const bytes = new Uint8Array(await (await fetch(url)).arrayBuffer());
    const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\.\d+Z$/, '').replace('T', '-');
    const name = `screenshot-${stamp}.png`;
    attach([new File([bytes], name, { type: 'image/png' })]);
    $('prompt').focus();
    toast(`A picture of ${tab?.title ? `“${tab.title}”` : 'the tab'} goes with your message.`);
  })().catch((error) => toast(error.message));
};
$('attach-file').onchange = (event) => {
  attach([...event.target.files]);
  event.target.value = '';
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
  if (!text || !engine) return;
  const thread = current ? threads.get(current) : null;
  const { provider, model, effort } = thread ? { provider: thread.provider, model: null, effort: null } : chosen();
  const profile = profileOf(provider);
  const network = thread ? Boolean(thread.network) : Boolean(choice.network);
  const runner = thread ? thread.runner : choice.runner ?? 'app';
  if (!profile) {
    toast(provider ? `${provider} is not in your provider list` : 'Choose a provider first');
    return;
  }
  if (needsKey(profile) && !keyed.has(profile.id)) {
    openSettings(flowOf(profile) ? `Sign in to ${profile.display_name} to send.` : `Add your ${profile.display_name} key to send.`);
    return;
  }
  if (needsKey(profile) && engine?.vault?.locked) {
    openSettings('Your keys are locked: unlock them to send.');
    return;
  }
  if (engine?.vault?.ended?.[profile.id]) {
    openSettings(`Your ${profile.display_name} sign-in has ended: sign in again to send.`);
    return;
  }
  const sites = network && runner === 'app' && !engine.relay;
  const target = current;
  const from = carrying?.id;
  const going = attached;
  sending = sendId;
  reachable(profile, sites).then(async (granted) => {
    // Access to sites declined for a thread with network: it still talks to its provider
    // if Spindle may reach that; its commands reach no website until Chrome allows.
    if (!granted && sites && (await chrome.permissions.contains({ origins: [accessFor(profile)] }).catch(() => false))) {
      granted = true;
      toast("Without Chrome's access to sites, this thread's commands reach no website; Chrome asks again when you send.");
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
        const answer = await ask('send', { id: thread.id, text, ...(files.length && { files }) });
        sending = null;
        if (answer.steered != null) toast('Sent to the running turn.');
      } else {
        const { id } = await ask('create', { provider, model, ...(effort && effort !== 'default' && { effort }), text, runner, network, ...(files.length && { files }), ...(from && { from }) });
        sending = null;
        if (current === target) {
          carry(null);
          await openThread(id);
        }
      }
    } catch (error) {
      if (current === target && cleared) {
        $('prompt').value = [text, $('prompt').value].filter(Boolean).join('\n\n');
        autosize();
        attached = [...going, ...attached.filter((file) => !going.some((kept) => kept.name === file.name))];
        renderAttached();
      }
      toast(error.message);
    }
  }).catch((error) => toast(error.message)).finally(() => { if (sending === sendId) sending = null; });
}

$('composer').addEventListener('submit', (event) => {
  event.preventDefault();
  if ($('send').dataset.mode === 'stop') stopTurn();
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

/** States in which a thread's last turn stopped before it finished. */
const HALTED = new Set(['stopped', 'failed', 'interrupted']);
const EFFORT_NAMES = { default: 'Default', off: 'Off', low: 'Low', medium: 'Medium', high: 'High', xhigh: 'X-high', max: 'Max' };

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
  const main = el('span', 'picker-main');
  const sub = el('span', 'picker-sub');
  field.append(main, sub);
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
  field.setAttribute('aria-label', label);
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
    sub.textContent = item?.detail ?? '';
    field.title = [item?.label ?? value, item?.detail].filter(Boolean).join(' · ');
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
  }));
}

/** A provider's models, each with what the catalog says of it. */
function modelItems(profile) {
  if (!profile) return [];
  return [...new Set([profile.default_model, ...(profile.models ?? [])].filter(Boolean))].map((name) => {
    const spec = profile.model_specs?.[name] ?? {};
    const context = spec.context_window ?? profile.model_context_windows?.[name] ?? profile.context_window;
    const price = spec.pricing;
    return {
      value: name,
      label: name,
      detail: [
        name === profile.default_model && 'default',
        context && `${tokenCount(context)} context`,
        spec.capabilities?.image_input && 'pictures',
        price && price.input_usd_per_mtok != null && `${dollars(price.input_usd_per_mtok)} in, ${dollars(price.output_usd_per_mtok)} out per M`,
      ].filter(Boolean).join(' · '),
    };
  });
}

/** The model sheet's choices while it is open. */
const sheet = { thread: null, halted: false, effort: 'default', efforts: [], loading: null, runner: 'app', network: false, wasRemote: false };

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

/** The chosen model's efforts, the chosen one kept if it takes it (else its default). */
function loadEfforts() {
  const provider = providerPicker.value;
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

function renderEffort() {
  $('effort-field').hidden = !sheet.efforts.length;
  $('model-effort').replaceChildren(...sheet.efforts.map((entry) => {
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
  const picked = sheet.efforts.find((entry) => entry.effort === sheet.effort);
  $('effort-note').textContent = picked ? `${sentence(picked.label)}.` : '';
}

/** Where commands run and their network, as chosen, and what each means. */
function renderRunner() {
  for (const button of $('model-runner').querySelectorAll('button')) {
    button.setAttribute('aria-checked', String(button.dataset.value === sheet.runner));
    if (button.dataset.value === 'remote') {
      button.disabled = !engine?.host?.connected && !sheet.wasRemote;
      button.title = button.disabled ? 'Connect your Spindle host in Settings first' : '';
    }
  }
  $('runner-note').textContent = {
    app: "In a Linux inside this browser (bash, Python, Node, git): nothing to set up.",
    remote: "On your Spindle host, in its sandbox.",
    none: 'No commands: the thread talks, and reads and writes its files.',
  }[sheet.runner];
  const off = sheet.runner === 'none';
  const network = $('model-network');
  network.disabled = off;
  network.setAttribute('aria-checked', String(sheet.network && !off));
  $('network-note').textContent = off ? 'There are no commands to reach anything.'
    : !sheet.network ? 'Off: commands reach nothing outside.'
    : sheet.runner === 'remote' ? "On: commands reach the internet from your host's sandbox."
    : engine?.relay ? 'On: commands reach any address through your relay.' : 'On: commands reach websites through Chrome.';
  $('model-note').textContent = [
    sheet.thread && !sheet.halted ? 'The next turn uses it.' : '',
    sheet.runner === 'app' && sheet.network && !engine?.relay ? 'Chrome asks once for access to sites (the same access page reading uses).' : '',
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

function openModel() {
  const thread = current ? threads.get(current) : null;
  const now = thread ? { provider: thread.provider, model: thread.model, effort: thread.effort } : chosen();
  Object.assign(sheet, {
    thread: thread?.id ?? null,
    halted: Boolean(thread && HALTED.has(thread.state)),
    effort: now.effort ?? 'default',
    efforts: [],
    runner: (thread ? thread.runner : choice.runner) ?? 'app',
    network: Boolean(thread ? thread.network : choice.network),
  });
  if (!['app', 'remote', 'none'].includes(sheet.runner)) sheet.runner = 'app';
  sheet.wasRemote = sheet.runner === 'remote';
  $('model-label').textContent = thread ? 'This thread' : 'New threads';
  $('model-halted').hidden = !sheet.halted;
  $('model-apply').textContent = sheet.halted ? 'Start the turn over' : 'Use it';
  providerPicker.value = now.provider ?? null;
  modelPicker.value = now.model ?? profileOf(now.provider)?.default_model ?? '';
  renderEffort();
  loadEfforts();
  renderRunner();
  $('model-dialog').showModal();
}

$('model-chip').onclick = openModel;
$('model-apply').onclick = () => {
  const provider = providerPicker.value;
  const profile = profileOf(provider);
  if (!profile) return;
  const model = String(modelPicker.value ?? '').trim() || profile.default_model;
  const runner = sheet.runner;
  const network = runner !== 'none' && sheet.network;
  const thread = sheet.thread ? threads.get(sheet.thread) : null;
  const halted = sheet.halted;
  // Asked now, in the click: a thread's next turn uses it, and network through Chrome
  // needs Chrome's access to sites (a new thread asks again when it is sent).
  const sites = network && runner === 'app' && !engine?.relay;
  const granted = thread || sites ? reachable(profile, sites) : Promise.resolve(true);
  $('model-dialog').close();
  (async () => {
    // The effort the model takes (its list may still have been on its way).
    await sheet.loading;
    const effort = sheet.efforts.some((entry) => entry.effort === sheet.effort) ? sheet.effort : 'default';
    const what = `${profile.display_name} · ${model}${effortOn(provider, model, effort) ? `, ${effortOn(provider, model, effort).short}` : ''}`;
    if (!thread) {
      choice = { provider, model, effort, runner, network };
      try { localStorage.setItem('spindle-new', JSON.stringify(choice)); } catch {}
      renderBar();
      if (!(await granted)) toast("Without Chrome's access to sites, new threads' commands reach no website; Chrome asks again when you send.");
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
    const changes = { provider, model, effort, ...(moved && { runner }), ...(networkChange && { network }) };
    const also = `${moved ? `, ${where}` : ''}${networkChange ? (network ? ', commands with network' : ', commands without network') : ''}.${refused ? " Chrome did not give access to sites, so the thread's network stays off." : ''}`;
    if (halted) {
      await ask('restart', { id: thread.id, changes });
      // The conversation as it is now: the stopped turn gone, its message sent again.
      if (current === thread.id) await openThread(thread.id);
      toast(`The turn starts over with ${what}${also}`);
    } else {
      await ask('settings', { id: thread.id, changes });
      toast(`The next turn uses ${what}${also}`);
    }
  })().catch((error) => toast(error.message));
};

$('takeback').onclick = async () => {
  const id = current;
  if (!id) return;
  try {
    const { text, files } = await ask('restart', { id, resend: false });
    if (current === id) {
      await openThread(id);
      toComposer(text, files);
    }
    toast('The unfinished turn was taken back: its message is in the box below, to change or send again.');
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

async function openSettings(note) {
  $('settings-note').hidden = !note;
  $('settings-note').textContent = note ?? '';
  if (!$('settings-dialog').open) {
    // As left; Providers open while no key is saved, or when the note is about one.
    const kept = folds();
    folding = true;
    for (const fold of document.querySelectorAll('details[data-fold]')) {
      fold.open = kept[fold.dataset.fold] ?? (fold.dataset.fold === 'providers' && !keyed.size);
    }
    if (note) document.querySelector('[data-fold="providers"]').open = true;
    // Toggle events come after this task: remembered toggles start after them.
    setTimeout(() => { folding = false; });
  }
  renderProviders();
  $('catalog-state').textContent = engine?.catalogSaved
    ? 'Using your providers.json.'
    : "Using Spindle's built-in providers (Dext's catalog). Import your ~/.dext/providers.json to use yours.";
  $('catalog-summary').textContent = engine?.catalogSaved ? 'your providers.json' : 'built in';
  renderCommands();
  if (document.activeElement !== $('standing')) $('standing').value = engine?.standing ?? '';
  $('instructions-summary').textContent = engine?.standing ? 'set' : 'none';
  await renderPages();
  renderTheme();
  await renderAbout();
  if (!$('settings-dialog').open) $('settings-dialog').showModal();
}

function renderProviders() {
  const box = $('providers');
  box.replaceChildren();
  const saved = providers().filter((profile) => keyed.has(profile.id)).map((profile) => profile.display_name);
  $('providers-summary').textContent = engine?.vault?.locked ? 'locked'
    : saved.length ? `${saved.join(', ')} ready` : 'none set up yet';
  for (const profile of providers()) {
    const row = el('div', 'provider');
    row.dataset.provider = profile.id;
    // Name, site, state and account stack on the left and wrap; the buttons keep their
    // own column, so a long account never squeezes the row.
    const head = el('div', 'provider-head');
    const name = el('div', 'provider-name');
    const has = keyed.has(profile.id);
    const flow = flowOf(profile);
    const signedIn = engine?.vault?.kinds?.[profile.id] === 'signin';
    const ended = signedIn && Boolean(engine.vault.ended?.[profile.id]);
    const state = el('span', `provider-state${has && !ended ? ' ok' : ''}`,
      ended ? 'sign-in ended' : has ? (signedIn ? 'signed in' : 'key saved')
        : !needsKey(profile) ? 'no key needed' : flow ? 'not signed in' : 'no key');
    name.append(el('strong', '', profile.display_name), el('small', '', new URL(profile.base_url).host), state);
    const account = signedIn ? engine.vault.accounts?.[profile.id] : null;
    if (account) {
      // Breaks only after a dot or the @, never inside a name.
      const line = el('small', 'provider-account');
      for (const part of account.split(/(?<=[.@])/)) line.append(part, document.createElement('wbr'));
      name.append(line);
    }
    const actions = el('div', 'provider-actions');
    head.append(name, actions);
    row.append(head);
    if (flow) {
      // A browser sign-in: the provider's own page in a tab; no key to paste.
      const signIn = el('button', has && !ended ? '' : 'primary', has ? 'Sign in again' : 'Sign in');
      signIn.type = 'button';
      signIn.disabled = signingIn.has(profile.id);
      signIn.onclick = () => signInTo(profile, signIn);
      actions.append(signIn);
      if (has) actions.append(forgetButton(profile, 'Sign out', `Signed out of ${profile.display_name}.`));
    } else {
      const toggle = el('button', '', has ? 'Change' : 'Add key');
      toggle.type = 'button';
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
          if (!(await granted)) toast(`Key saved. Spindle still needs to reach ${new URL(profile.base_url).host}; it asks again when you send.`);
          else toast(`${profile.display_name} key saved.`);
          renderProviders();
        }, (error) => toast(`The key was not saved: ${error.message}`));
      };
    }
    box.append(row);
  }
  renderVault();
}

function forgetButton(profile, label, done) {
  const button = el('button', 'danger', label);
  button.type = 'button';
  button.onclick = async () => {
    try {
      apply(await ask('forget', { provider: profile.id }));
      toast(done);
    } catch (error) {
      toast(error.message);
    }
    renderProviders();
  };
  return button;
}

/** A browser sign-in: Chrome's leave to reach the provider (asked first, in the click),
 * the provider's page in a tab, then the engine keeps what it gives. */
async function signInTo(profile, button) {
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
    toast(`Sign in to ${profile.display_name} in the tab that opened.`);
    const caught = await signInInTab(profile);
    apply(await ask('signIn', { provider: profile.id, ...caught }));
    toast(`Signed in to ${profile.display_name}.`);
  } catch (error) {
    if (error.kind !== 'cancelled') toast(`Not signed in: ${error.message}`);
  } finally {
    signingIn.delete(profile.id);
    button.disabled = false;
    renderProviders();
  }
}

/** Key protection: keys are always sealed in this browser; a passphrase makes a copy of
 * the Chrome profile useless without it. */
function renderVault() {
  const box = $('vault');
  if (!box) return;
  box.replaceChildren();
  const state = engine?.vault;
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
  // Fields one under another, full width, and their buttons beneath them.
  const form = (inputs, buttons) => {
    const box = el('div', 'vault-form');
    const row = el('div', 'confirm-actions');
    row.append(...buttons);
    box.append(...inputs, row);
    return box;
  };
  if (state.mode === 'device') {
    box.append(el('p', 'note', 'Keys and sign-ins are encrypted in this browser. Add a passphrase so that a copy of your Chrome profile cannot read them; you then unlock them once each time Spindle starts.'));
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
      action('Forgot it', '', async () => {
        const sure = await confirmed('Forget every saved key?',
          'Without the passphrase they cannot be opened. Every saved key and sign-in is deleted (threads stay), and you add them again.',
          'Delete keys');
        if (!sure) return;
        apply(await ask('resetVault'));
        toast('Saved keys and sign-ins deleted. Add them again to send.');
      }),
      action('Unlock', 'primary', async () => {
        apply(await ask('unlock', { passphrase: pass.value }, 120_000));
        toast('Keys unlocked.');
      }),
    ]));
    return;
  }
  box.append(el('p', 'note', 'Keys are locked with your passphrase, and unlocked until Spindle restarts.'));
  box.append(form([], [action('Lock now', '', async () => {
    apply(await ask('lock'));
    toast('Keys locked.');
  })]));
  // Changing or removing it is rare: folded away.
  const more = el('details', 'subfold vault-more');
  more.append(el('summary', 'fold-title', 'Change or remove the passphrase'));
  const now = field('Current passphrase');
  const next = field('New passphrase (to change it)');
  const again = field('New passphrase again');
  box.append(more);
  more.append(form([now, next, again], [
    action('Change', '', async () => {
      if (next.value !== again.value) throw new Error('the two new passphrases differ');
      apply(await ask('protect', { passphrase: next.value, previous: now.value }, 120_000));
      toast('Passphrase changed.');
    }),
    action('Remove', 'danger', async () => {
      apply(await ask('unprotect', { passphrase: now.value }, 120_000));
      toast('Keys no longer need a passphrase.');
    }),
  ]));
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
  $('linux-state').textContent = `${LINUX_STATES[linux.state] ?? linux.state}${linux.detail ? ` — ${linux.detail}` : ''}`;
  $('wasm-linux-row').hidden = !linux.wasm;
  if (linux.wasm) $('wasm-linux-state').textContent = `${WASM_STATES[linux.wasm.state] ?? linux.wasm.state}${linux.wasm.detail ? ` — ${linux.wasm.detail}` : ''}`;
  const relay = engine?.relay ?? null;
  const relayHost = relay ? new URL(relay).host : null;
  $('network-state').textContent = relay
    ? `through your relay at ${relayHost}: any address, for threads with network on`
    : 'through Chrome: websites over HTTP and HTTPS, for threads with network on';
  $('commands-summary').textContent = `Linux ${linux.state === 'ready' ? 'running' : linux.state}${relay ? ' · relay' : ''}`;
  $('relay-summary').textContent = relayHost ?? 'none';
  if (document.activeElement !== $('relay-url')) $('relay-url').value = relay ?? '';
  $('relay-off').hidden = !relay;
  const host = engine?.host ?? {};
  if (host.url && document.activeElement !== $('host-url')) $('host-url').value = host.url;
  $('host-state').textContent = host.connected
    ? `Connected to ${host.url}.`
    : host.error ? `Not connected: ${host.error}` : 'Not connected.';
  $('host-summary').textContent = host.connected ? 'connected' : host.url ? 'not connected' : 'none';
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
    toast('Threads with network reach websites through Chrome again.');
  } catch (error) {
    toast(error.message);
  }
  renderCommands();
};

function renderSites() {
  const box = $('sites');
  box.replaceChildren();
  const sites = engine?.sites ?? [];
  for (const grant of sites) {
    const row = el('div', 'provider');
    const head = el('div', 'provider-head');
    const name = el('div', 'provider-name');
    name.append(el('strong', '', new URL(grant.url).host), el('small', '', grant.url));
    const drop = el('button', 'danger', 'Remove');
    drop.type = 'button';
    drop.onclick = () => saveSites({ remove: grant.name }, `Scripts no longer reach ${new URL(grant.url).host}.`);
    head.append(name, el('span', 'provider-state', grant.risk === 'write' ? 'read and change' : 'read'), drop);
    row.append(head);
    box.append(row);
  }
  if (!sites.length) box.append(el('p', 'note', 'None yet: scripts call no site.'));
  $('sites-summary').textContent = sites.length ? String(sites.length) : 'none';
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
  mayReach(`${url.origin}/*`, url.host).then(async (granted) => {
    if (!granted) {
      toast(`Chrome did not let Spindle reach ${url.host}.`);
      return;
    }
    if (await saveSites({ add: { url: url.href, write } }, `Scripts may reach ${url.host}.`)) $('site-url').value = '';
  });
};

for (const button of $('site-access-kind').querySelectorAll('button')) {
  button.onclick = () => {
    for (const other of $('site-access-kind').querySelectorAll('button')) other.setAttribute('aria-checked', String(other === button));
    $('site-access-note').textContent = button.dataset.value === 'write' ? 'GET, POST, PUT, PATCH and DELETE requests.' : 'GET requests only.';
  };
}

function onLinux(state) {
  if (engine) engine.linux = { state: state.state, detail: state.detail, wasm: state.wasm ?? null };
  if ($('settings-dialog').open) renderCommands();
  const thread = current ? threads.get(current) : null;
  if (thread?.state === 'running' && ['booting', 'saving', 'resuming'].includes(state.state)) {
    phase(state.state === 'resuming' ? 'resuming the Linux…' : `starting the Linux${state.detail ? ` (${state.detail})` : ''}…`, 'busy');
  }
}

$('host-form').onsubmit = (event) => {
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
    toast('Paste an API token from your host.');
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
    $('instructions-summary').textContent = engine.standing ? 'set' : 'none';
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
  const set = async (next) => {
    await write('kv', 'pages', next);
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
    asked.then((given) => (given ? set({ reading: true, acting: now.acting }) : toast('Chrome did not give Spindle access to sites.')));
  };
  acting.onclick = () => {
    if (now.acting) {
      set({ reading: true, acting: false });
      return;
    }
    const asked = access ? Promise.resolve(true) : mayReach(PAGE_ACCESS, 'websites');
    asked.then((given) => (given ? set({ reading: true, acting: true }) : toast('Chrome did not give Spindle access to sites.')));
  };
  $('pages-summary').textContent = now.acting ? 'reading and actions on' : now.reading ? 'reading on' : 'off';
  $('site-access').textContent = access
    ? "Chrome lets Spindle reach sites: page tools use it while on, and so do threads' commands with network (through Chrome)."
    : "Chrome does not let Spindle reach sites yet: it asks when you turn page reading on, or a thread's network.";
  $('site-access-remove').hidden = !access;
  $('site-access-remove').onclick = async () => {
    await chrome.permissions.remove({ origins: [...PAGE_ACCESS, ...PICTURE_ACCESS] }).catch(() => false);
    // Giving the access again later (for a thread's network) turns no switch back on.
    await write('kv', 'pages', { reading: false, acting: false });
    chrome.runtime.sendMessage({ type: 'pages' }).catch(() => {});
    toast("Chrome's access to sites is taken back: no page is read or acted on, and commands reach no website through Chrome.");
    renderPages();
  };
}

function renderTheme() {
  const theme = globalThis.spindleTheme;
  const mode = { paper: 'Paper', dark: 'Dark', system: 'System' }[theme?.get()] ?? 'Paper';
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
  if (engine?.info) add('Spindle', `${engine.info.version} · schema ${engine.info.schema}`);
  $('about-summary').textContent = engine?.info ? `${engine.info.version} · schema ${engine.info.schema}` : '';
  add('Threads', String(threads.size));
  try {
    const estimate = await navigator.storage.estimate();
    add('Storage', `${size(estimate.usage ?? 0)} used`);
  } catch {}
}

$('settings').onclick = () => openSettings();
$('open-tab').onclick = openTab;
$('restart-engine').onclick = async () => {
  $('settings-dialog').close();
  const busy = [...threads.values()].filter((thread) => thread.state === 'running').length;
  if (busy && !(await confirmed('Restart the engine?', `${busy} thread(s) are running. Restarting cuts them off; each can be retried after.`, 'Restart'))) return;
  await restartEngine();
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
const ws = { id: null, files: [], changes: new Map(), order: [], path: null, web: false, scrolled: 0, loading: false };

async function openWorkspace(id, path = null) {
  if (!id) return;
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

/** Back to the conversation the workspace page was opened from. */
function closeWorkspace() {
  ws.id = null;
  $('ws-preview').replaceChildren();
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
  $('ws-heading').textContent = `Workspace · ${files.length} file${files.length === 1 ? '' : 's'}`;
  $('ws-note').textContent = fresh
    ? `${fresh} made or changed by the last turn (marked).`
    : 'Files you attach land here; what the thread makes, you can open and download.';
  $('files-zip').hidden = !files.length;
  ws.order = drawTree($('file-list'), files, changes, { onOpen: openFile, filter: $('ws-filter').value, links: fileLinks });
}

/** A file row's links: download, and open in a tab of its own for a page or picture. */
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
  if (!['page', 'picture'].includes(kindOf(file.path))) return [get];
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
  $('ws-preview').replaceChildren();
  $('ws-list').hidden = false;
  $('ws-file').hidden = true;
  // Back on the list: on the row of the file just shown.
  const row = was && $('file-list').querySelector(`.file-row[data-path="${CSS.escape(was)}"] .file-open`);
  if (row && !row.closest('[hidden]')) row.focus();
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
  ws.web = false;
  $('ws-list').hidden = true;
  $('ws-file').hidden = false;
  const parts = path.split('/');
  $('ws-path').replaceChildren(...parts.flatMap((part, index) => [el('span', 'ws-sep', '/'), el(index === parts.length - 1 ? 'strong' : 'span', '', part)]));
  const change = ws.changes.get(path);
  const kind = kindOf(path);
  $('ws-meta').replaceChildren(document.createTextNode([{ page: 'page', picture: 'picture', markdown: 'Markdown', text: 'text' }[kind] ?? 'file', size(file.size)].join(' · ')));
  if (change) $('ws-meta').append(el('span', `badge ${change}`, change));
  const actions = [copyButton(() => path, 'Copy its path')];
  if (kind === 'page' || kind === 'picture') actions.push(iconButton('external', `Open ${path} in a tab`, () => chrome.tabs.create({ url: viewUrl(id, path) })));
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
  const order = ws.order.includes(path) ? ws.order : ws.files.map((one) => one.path).sort((a, b) => a.localeCompare(b));
  const at = order.indexOf(path);
  $('ws-prev').disabled = at <= 0;
  $('ws-next').disabled = at < 0 || at >= order.length - 1;
  $('ws-prev').onclick = () => at > 0 && openFile(order[at - 1]);
  $('ws-next').onclick = () => at < order.length - 1 && openFile(order[at + 1]);
  const preview = $('ws-preview');
  preview.className = `kind-${kind ?? 'bytes'}`;
  const show = () => previewInto(preview, id, path, { web: ws.web, any: true, textId: 'file-content', allow: () => {
    ws.web = true;
    show();
  } });
  show();
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
  const commands = { app: "commands in this browser's Linux", remote: 'commands in your Spindle host' }[thread.runner] ?? 'no commands';
  const facts = [`${nameOf(thread.provider)} · ${thread.model}`, commands, `started ${new Date(thread.created).toLocaleString()}`];
  if (status) facts.push(`revision ${status.head}`);
  $('thread-facts').textContent = facts.join(' · ');
  const moved = thread.state === 'moved';
  $('menu-files').disabled = moved;
  $('menu-fresh').disabled = moved;
  $('menu-export').disabled = moved;
  $('menu-copy').disabled = moved;
  $('thread-dialog').showModal();
}

/** The thread whose files the next new thread starts with ({id, title}), or null. */
let carrying = null;

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
  choice = { provider: thread.provider, model: thread.model, effort: thread.effort ?? 'default', runner: thread.runner, network: Boolean(thread.network) };
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
  try {
    const { id } = await ask('import', { bytes: new Uint8Array(await file.arrayBuffer()), name: file.name });
    await openThread(id);
    toast(`Imported ${file.name}.`);
  } catch (error) {
    toast(`${file.name} was not imported: ${error.message}`);
  }
};

// ---- Little things ---------------------------------------------------------------

for (const button of document.querySelectorAll('[data-close]')) button.onclick = () => $(button.dataset.close).close();

let toastTimer = 0;
/** A short message, above everything (an open dialog too: the toast is a popover, shown
 * again so it is the newest thing in the top layer). */
function toast(text) {
  const node = $('toast');
  node.textContent = explain(text);
  node.hidden = false;
  try {
    if (node.matches(':popover-open')) node.hidePopover();
    node.showPopover();
  } catch {}
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    node.hidden = true;
    try { node.hidePopover(); } catch {}
  }, 5000);
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
keyed = new Set(await keysOf('keys'));
// Another window's panel may have saved a key.
addEventListener('focus', async () => {
  keyed = new Set(await keysOf('keys'));
  renderBar();
});
showHome();
await connect();
