// Spindle's service worker: the toolbar button opens the panel, it serves the frame
// previews show pages in and a thread's PDFs for Chrome's own viewer, and it does the
// page tools' work for the engine (the engine's document cannot reach tabs). Reading
// needs page reading on; acting needs page actions on as well (chrome.debugger: clicks,
// keys and typing reach the page as the user's would, and a tab's network can be
// watched).
import { ACTING_NAMES, BROWSER, PAGE_ACCESS, PICTURE_ACCESS, pageSwitches, pagesRecord, uncertain } from './page-tools.mjs';
import { REQUESTS, answersFor, read, startEngine, write } from './shared.mjs';

// (A browser without the side panel API still runs the rest.)
chrome.sidePanel?.setPanelBehavior({ openPanelOnActionClick: true }).catch(() => {});

// The switches are kept from the first start: off on a new install. Builds before the
// switches were Spindle's took page reading to be Chrome's access to sites, so a user
// updating from one who had it keeps it (once: the record exists from then on).
// Before acting is Auto unless chosen; an install from when the default was Full keeps
// Full as it updates (its switches never named a mode), so nothing it did unasked starts
// asking. A new install asks before what sends or submits.
const UNREAD = Symbol('unread');
chrome.runtime.onInstalled.addListener(async ({ reason }) => {
  // A read that fails changes nothing (a missing record reads as undefined).
  const kept = await read('kv', 'pages').catch(() => UNREAD);
  if (kept === UNREAD) return;
  if (kept) {
    if (reason === 'update' && !kept.confirm) await write('kv', 'pages', { ...kept, confirm: 'full' }).catch(() => {});
    return;
  }
  const access = reason === 'update' && (await chrome.permissions.contains({ origins: PAGE_ACCESS }));
  await write('kv', 'pages', { reading: access, acting: false, confirm: 'auto' }).catch(() => {});
});

// Pages a thread made show in preview.html (page.mjs), served from here as packaged, with
// a Connection-Allowlist that no page can set or widen: Chrome (152 and later) then makes
// no connection for the page, or for any frame or worker it makes, that the list does not
// name. That reaches what a page's policy cannot (WebRTC, resource hints, navigations).
// The list names nothing, or for a page the user let reach the web (`?reach=web`), HTTPS
// anywhere (WebRTC stays shut). preview.js tells whether the list holds; a frame not
// served from here (one that skipped this worker, or came before it was active) shows
// pages as if it did not.
const PREVIEW = new URL('preview.html', location.href).pathname;
/** Where a thread's PDFs are answered (`pdf`). */
const FILES = '/file/';
const REACH = { none: '()', web: '("https://*:*")' };

// Only where Chrome has static routes (123 and later): there only previews and PDFs come
// to the handler, and every other request of Spindle's pages and workers goes on as if
// there were none (the worker is not woken for each). Without them it serves nothing (no
// Chrome that old holds a Connection-Allowlist anyway): previews show uncontained, and
// PDFs only as downloads (page.mjs's pdfShows).
if ('addRoutes' in (self.InstallEvent?.prototype ?? {})) {
  self.addEventListener('install', (event) => {
    event.waitUntil(event.addRoutes([
      // Spindle's own addresses only (a site's `/file/...`, fetched for a thread, never
      // wakes this worker).
      { condition: { urlPattern: new URLPattern({ pathname: PREVIEW, baseURL: location.href }) }, source: 'fetch-event' },
      { condition: { urlPattern: new URLPattern({ pathname: `${FILES}*`, baseURL: location.href }) }, source: 'fetch-event' },
      { condition: { urlPattern: new URLPattern({}) }, source: 'network' },
    ]).catch((error) => console.warn('Spindle: previews go through the fetch handler with everything else:', error)));
  });
  self.addEventListener('fetch', (event) => {
    const url = new URL(event.request.url);
    if (url.origin !== location.origin || event.request.method !== 'GET') return;
    if (url.pathname === PREVIEW && event.request.mode === 'navigate') event.respondWith(preview(url));
    else if (url.pathname.startsWith(FILES)) event.respondWith(pdf(url));
  });
}

// A thread's PDF (`/file/THREAD/PATH`, page.mjs's pdfAddress) opens in a tab of its own
// as Chrome's own PDF viewer: this worker answers its address with the file, read from
// the engine. Only Spindle's pages reach the address (a site cannot: it is no resource
// the extension shares), only PDFs are given, and only as PDFs (never read as a page).
// It acts as a PDF downloaded and opened does (no Connection-Allowlist: the viewer's own
// interface does not load under one): probed, a form a user submits from it goes out
// with no cookies and an opaque origin either way.
async function pdf(url) {
  const plain = (status, text) => new Response(text, {
    status,
    headers: { 'Content-Type': 'text/plain; charset=utf-8', 'X-Content-Type-Options': 'nosniff', 'Cache-Control': 'no-store' },
  });
  const [thread = '', ...parts] = url.pathname.slice(FILES.length).split('/');
  let names;
  try {
    names = parts.map(decodeURIComponent);
  } catch {
    return plain(400, 'This address names no file.');
  }
  const path = names.join('/');
  // A workspace path, as written: no empty part, no `.` or `..`, no slash inside a name.
  if (!/^[0-9a-f]{16}$/.test(thread) || !names.length || names.some((name) => !name || name === '.' || name === '..' || /[/\\]/.test(name)) || !/\.pdf$/i.test(path)) {
    return plain(404, "Spindle shows a thread's PDFs here, nothing else.");
  }
  try {
    const { bytes } = await fromEngine('read', { id: thread, path });
    return new Response(bytes, {
      headers: {
        'Content-Type': 'application/pdf',
        'Content-Disposition': `inline; filename*=UTF-8''${encodeURIComponent(path.split('/').pop())}`,
        'X-Content-Type-Options': 'nosniff',
        'Cache-Control': 'no-store',
      },
    });
  } catch (error) {
    return plain(404, `${path} could not be read: ${error.message}`);
  }
}

/** `op`'s answer once the engine runs (started here if it is not: it may be starting).
 * Asked on the engine's request channel and answered on a channel of this call's own
 * (open only while it asks), so a file's bytes reach this worker alone. */
async function fromEngine(op, args) {
  const me = `worker-${crypto.randomUUID()}`;
  const requests = new BroadcastChannel(REQUESTS);
  const engine = new BroadcastChannel(answersFor(me));
  const waiting = new Map();
  let asked = 0;
  engine.onmessage = ({ data }) => {
    if (data?.to !== me || !waiting.has(data.id)) return;
    const answer = waiting.get(data.id);
    waiting.delete(data.id);
    clearTimeout(answer.timer);
    if (data.ok) answer.resolve(data.result);
    else answer.reject(new Error(data.error?.message ?? 'the engine failed'));
  };
  const ask = (what, given, timeout) => new Promise((resolve, reject) => {
    const id = ++asked;
    const timer = setTimeout(() => {
      waiting.delete(id);
      reject(Object.assign(new Error("Spindle's engine did not answer"), { kind: 'timeout' }));
    }, timeout);
    waiting.set(id, { resolve, reject, timer });
    requests.postMessage({ to: 'engine', from: me, id, op: what, args: given });
  });
  try {
    await startEngine();
    for (let attempt = 0; ; attempt++) {
      try {
        await ask('ping', {}, 3_000);
        break;
      } catch (error) {
        if (error.kind !== 'timeout' || attempt >= 20) throw error;
      }
    }
    return await ask(op, args, 120_000);
  } finally {
    for (const { timer } of waiting.values()) clearTimeout(timer);
    engine.close();
    requests.close();
  }
}

async function preview(url) {
  const packaged = await fetch(PREVIEW);
  const headers = new Headers(packaged.headers);
  const sandbox = chrome.runtime.getManifest().content_security_policy?.sandbox;
  if (sandbox) headers.set('Content-Security-Policy', sandbox);
  headers.set('Connection-Allowlist', url.searchParams.get('reach') === 'web' ? REACH.web : REACH.none);
  return new Response(packaged.body, { status: packaged.status, headers });
}

const TOOLS = {
  list_tabs: listTabs, read_page: readPage, page_elements: pageElements, page_html: pageHtml, screenshot,
  navigate, click, type, select, press, scroll, wait_for: waitFor, dialog, close_tab: closeTab, network,
  // The engine's own, offered to no thread: what an action would land on, for the check
  // Settings, Pages, Before acting asks for.
  target, tab_of: tabOf,
};

/** How long one call into a page or Chrome's debugger may take (a tool has 30 s). */
const DEADLINE_MS = 20_000;
/** How long read_page scrolls before it answers with what it read. */
const SCROLL_MS = 20_000;
const calls = new Map();
const TOOL_MS = 28_000;

function check(control) {
  if (control && (control.cancelled || Date.now() >= control.until)) throw new Error('the page tool was stopped or timed out');
}

chrome.runtime.onMessage.addListener((message, sender, reply) => {
  if (sender.id !== chrome.runtime.id) return false;
  // A panel changed the switches: page actions off ends what they hold.
  if (message?.type === 'pages' && sender.url?.startsWith(chrome.runtime.getURL(''))) {
    letGo().catch(() => {});
    return false;
  }
  // A panel says until when the user writes in its message box (0: no longer).
  if (message?.type === 'typing' && sender.url?.startsWith(chrome.runtime.getURL(''))) {
    const until = Math.min(Number(message.until) || 0, Date.now() + TYPING_MAX_MS);
    chrome.storage.session.set({ [TYPING]: until }).catch(() => {});
    return false;
  }
  // Only the engine's document asks for tools (no page's content, no other extension).
  if (sender.tab || sender.url !== chrome.runtime.getURL('engine.html')) return false;
  if (message?.type === 'page_tool_cancel') {
    const control = calls.get(message.call);
    if (control) control.cancelled = true;
    return false;
  }
  // A thread's turn is done (or the thread deleted): the tabs Spindle opened for it close.
  if (message?.type === 'tabs_done') {
    closeOpened(String(message.thread ?? ''), { tasks: Boolean(message.tasks) }).catch((error) => console.warn('Spindle: tabs it opened:', error));
    return false;
  }
  if (message?.type !== 'page_tool') return false;
  const owner = typeof message.thread === 'string' && message.thread ? { thread: message.thread, task: Boolean(message.task) } : null;
  run(message.name, message.input ?? {}, message.call, message.allowed, owner).then(reply, (error) => reply({ error: String(error?.message ?? error) }));
  return true;
});

const OFF =
  'Spindle may not read pages: page reading is off. The user can turn it on in the Spindle panel (Settings, Pages).';
const NO_ACTIONS =
  'Spindle may not act on pages: page actions are off. The user can turn them on in the Spindle panel (Settings, Pages).';

async function run(name, input, call = crypto.randomUUID(), allowed = null, owner = null) {
  const work = TOOLS[name];
  if (!work) return { error: `there is no page tool ${name}` };
  // A tab named otherwise than by its number (Chrome would refuse it in words that do not
  // say why; a whole number in quotes was made the number before it came here).
  if (input?.tab != null && !Number.isInteger(input.tab)) return { error: `tab is a tab's number from list_tabs, such as 2054624637, not ${JSON.stringify(input.tab).slice(0, 40)}; nothing was done` };
  // `acted`: input reached the page, or Chrome was told to move or close a tab.
  // `allowed`: what the user was shown when asked before this action (`asAllowed`).
  // `owner`: the thread (and whether a task of it) the call is for: tabs it opens are its.
  const control = { until: Date.now() + TOOL_MS, cancelled: false, acted: false, allowed, owner };
  calls.set(call, control);
  const began = Date.now();
  let tab = Number.isInteger(input?.tab) ? input.tab : null;
  // A window its page opens on this action is the thread's: new tabs are watched for.
  const watched = Boolean(owner) && OPENING.has(name);
  if (watched) {
    opening += 1;
    watchTabs();
  }
  try {
    const result = await deadline((async () => {
      if (!(await reading())) return { error: OFF };
      const viewing = name === 'navigate' && typeof input?.url === 'string' && input.url.startsWith(chrome.runtime.getURL('view.html?'));
      if (ACTING_NAMES.has(name) && !viewing && name !== 'screenshot' && !(await acting())) return { error: NO_ACTIONS };
      check(control);
      return work(input, control);
    })(), TOOL_MS, 'the page tool');
    tab = result?.details?.tab ?? tab;
    return result;
  } catch (error) {
    // A failure once the tool began acting does not say the action did not happen: its
    // outcome is unknown (checked rather than repeated), unless the tool is safe to
    // repeat, such as a read that scrolled. A cancelled or late tool acts no further:
    // every action checks `control` just before it goes.
    if (!control.acted) throw error;
    return uncertain(name, `it began acting on the page, then failed (${error?.message ?? error})`);
  } finally {
    control.cancelled = true;
    calls.delete(call);
    // A tab the page opened on this action (a link to a new tab, say) is the thread's too.
    if (watched) {
      opening -= 1;
      if (control.acted && tab != null) actions.push({ owner, tab, from: began, to: Date.now() + OPENED_GRACE_MS });
      claimOpened();
    }
  }
}

/** The user's switches, with Chrome's access to sites. A provider's site the extension
 * may reach for its key does not count: without page reading no page is read or listed. */
async function switches() {
  const access = await chrome.permissions.contains({ origins: PAGE_ACCESS });
  return pageSwitches(await read('kv', 'pages').catch(() => null), access);
}
const reading = async () => (await switches()).reading;
const acting = async () => (await switches()).acting;

const pause = (ms) => new Promise((done) => setTimeout(done, ms));

/** `work`, or an error once `ms` pass (the work may still finish later). */
function deadline(work, ms, what) {
  let timer;
  return Promise.race([
    work.finally(() => clearTimeout(timer)),
    new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`${what} did not answer within ${ms / 1000} s`)), ms); }),
  ]);
}

/** The tab the user is looking at: the active one of the last focused window. Spindle's
 * own page open in a tab is not it; then another window's active tab is. For the thread
 * of `control`, a tab it opened behind the one showing (`quietTab`) is, until shown: a
 * tool without a `tab` goes on there, as it would on a tab Spindle opened in front. */
async function lookedAt(control = null) {
  const behind = await quietTab(control?.owner);
  if (behind) return behind;
  const own = (tab) => (tab.url ?? tab.pendingUrl ?? '').startsWith(chrome.runtime.getURL(''));
  const window = await chrome.windows.getLastFocused({ windowTypes: ['normal'] }).catch(() => null);
  if (window) {
    const [tab] = await chrome.tabs.query({ active: true, windowId: window.id });
    if (tab && !own(tab)) return tab;
  }
  const active = await chrome.tabs.query({ active: true, windowType: 'normal' });
  return active.find((tab) => !own(tab)) ?? null;
}

/** The tab a tool works on (`id`, or the one the user is looking at), which must show a
 * web page Chrome lets extensions into. */
async function pageTab(id, control = null) {
  const tab = id != null ? await chrome.tabs.get(id).catch(() => null) : await lookedAt(control);
  if (!tab) throw new Error(id != null ? `there is no tab ${id}` : 'no tab is open');
  if (!tab.url) throw new Error(`${BROWSER} gives Spindle no access to tab ${tab.id} (a new tab, a blank page or a browser page)`);
  if (!/^https?:/.test(tab.url)) throw new Error(`${BROWSER} does not let extensions into ${new URL(tab.url).protocol} pages`);
  return tab;
}

/** A tab the acting tools may move or close: one showing a web page or a blank page
 * (never a browser page, another extension's, or Spindle's own). */
const webTab = (tab) => /^https?:/.test(tab?.url ?? '') || tab?.url === 'about:blank';

/** Runs one of `inPage`'s operations in the tab (Spindle's own isolated world there).
 * A tab showing a dialog answers nothing until the dialog is answered: said at once. */
async function page(tabId, op, args = {}, control) {
  if (!(await reading())) throw new Error(OFF);
  if (['point', 'caret', 'select', 'scroll', 'file_part', 'files'].includes(op) && !(await acting())) throw new Error(NO_ACTIONS);
  if (['caret', 'select', 'scroll', 'files'].includes(op)) await refuseVetoed(tabId);
  check(control);
  refuseWhileDialog(tabId);
  // `point` only measures; these change the page, unless it refuses first (then this
  // changed nothing, and the call stands as it did).
  const was = control?.acted;
  if (control && ['caret', 'select', 'scroll', 'files'].includes(op)) control.acted = true;
  const [answer] = await deadline(
    chrome.scripting.executeScript({ target: { tabId }, func: inPage, args: [op, { ...args, expires: control?.until }], injectImmediately: true }),
    DEADLINE_MS, `tab ${tabId}`,
  );
  const result = answer?.result;
  if (result?.error) {
    if (control) control.acted = was;
    throw new Error(result.error);
  }
  return result?.value;
}

/** Where a tab is, said after an action. */
async function whereNow(tabId) {
  const tab = await chrome.tabs.get(tabId).catch(() => null);
  if (!tab) return `Tab ${tabId} is gone.`;
  const open = dialogs.get(tabId);
  const asking = open ? ` A ${open.type} dialog is open on it: ${JSON.stringify(open.message)} (answer it with dialog).` : '';
  return `Tab ${tabId} is at ${tab.title || '(untitled)'} <${tab.url ?? '?'}>${tab.status === 'loading' ? ' (still loading)' : ''}.${asking}`;
}

/** Once a page reacts: a moment, and the load an action started, if it started one. */
async function settle(tabId, ms = 10_000, control, moment = 400) {
  await pause(moment);
  check(control);
  const until = Date.now() + ms;
  while (Date.now() < until && !dialogs.has(tabId)) {
    check(control);
    const tab = await chrome.tabs.get(tabId).catch(() => null);
    if (!tab || tab.status === 'complete') return;
    await pause(200);
  }
}

// ---- Reading -----------------------------------------------------------------------

async function listTabs(input, control) {
  const tabs = await chrome.tabs.query({ windowType: 'normal' });
  const seen = await lookedAt();
  const behind = await quietTab(control?.owner);
  const lines = tabs.map((tab) => {
    const mark = tab.id === seen?.id ? ' (the user is looking at it)'
      : tab.id === behind?.id ? ' (yours, behind the one showing: tools without a tab act on it)' : '';
    return `tab ${tab.id}${mark}: ${tab.url ? `${tab.title || '(untitled)'} <${tab.url}>` : '(no access)'}`;
  });
  return { content: lines.join('\n'), details: { tabs: tabs.length } };
}

async function readPage({ tab: id, max_chars: max, scroll: screens, find }, control) {
  const limit = Math.min(Math.max(Number(max) || 20000, 1000), 100000);
  // A file of a thread's workspace open in Spindle's own tab (view.html), named or
  // looked at: read as the file, Chrome keeping extensions out of their own pages or not.
  const own = id != null ? await chrome.tabs.get(id).catch(() => null) : await lookedAt(control).catch(() => null);
  const view = viewed(own);
  if (view) return readView(own, view, limit, find);
  const tab = await pageTab(id, control);
  refuseWhileDialog(tab.id);
  if (screens) return readScrolling(tab, limit, screens, control, find);
  // Cut in the page: a huge page's text never crosses over whole (not when lines are
  // looked for: the whole text is searched, and only the matches cross).
  const [{ result }] = await deadline(chrome.scripting.executeScript({
    target: { tabId: tab.id },
    args: [find ? 4 * 1024 * 1024 : limit],
    func: (limit) => {
      const text = document.body?.innerText ?? '';
      const selection = String(getSelection() ?? '');
      return { url: location.href, title: document.title, selection: selection.slice(0, 10000), text: text.slice(0, limit), total: text.length };
    },
  }), DEADLINE_MS, `tab ${tab.id}`);
  const lines = [`Address: ${result.url}`, `Title: ${result.title}`];
  if (result.selection.trim()) lines.push(`Selected text: ${result.selection}`);
  if (find) return { content: `${lines.join('\n')}\n\n${matchingLines(result.text, find, limit)}`, details: { tab: tab.id, url: result.url, title: result.title } };
  const more = result.total - result.text.length;
  const text = more > 0 ? `${result.text}\n[${more} more characters: ask again with a larger max_chars]` : result.text;
  return { content: `${lines.join('\n')}\n\n${text}`, details: { tab: tab.id, url: result.url, title: result.title } };
}

/** A tab showing a file of a thread's workspace in Spindle's own page (view.html):
 * the thread and the path; null for any other tab. */
function viewed(tab) {
  const url = String(tab?.url ?? '');
  if (!url.startsWith(chrome.runtime.getURL('view.html?'))) return null;
  const params = new URL(url).searchParams;
  const thread = params.get('thread') ?? '';
  const path = params.get('path') ?? '';
  return /^[0-9a-f]{16}$/.test(thread) && path ? { thread, path } : null;
}

/** Files a workspace tab shows that have no text to read. */
const PICTURED = /\.(png|jpe?g|gif|webp|svg|avif|bmp|ico|apng|pdf|docx|docm|xlsx|xlsm|pptx|pptm|mp4|m4v|webm|ogv|mov|mp3|m4a|aac|wav|ogg|oga|opus|flac)$/i;

/** The text of the workspace file a tab shows: what its preview frame shows (the page
 * drawn, scripts run as the user let them), when Chrome lets a script into Spindle's
 * own page; else the file itself, a page's markup stripped to its text. */
async function readView(tab, { thread, path }, limit, find) {
  let text = null;
  let how = 'as the preview shows it';
  try {
    const [{ result }] = await deadline(chrome.scripting.executeScript({
      target: { tabId: tab.id },
      args: [find ? 4 * 1024 * 1024 : limit],
      func: (most) => {
        const body = document.querySelector('iframe')?.contentDocument?.body;
        const text = body?.innerText;
        return typeof text === 'string' && text.trim() ? { text: text.slice(0, most), total: text.length } : null;
      },
    }), DEADLINE_MS, `tab ${tab.id}`);
    if (result) text = result;
  } catch {
    // Chrome keeps scripts out of extension pages here: the file itself is read.
  }
  if (!text) {
    if (PICTURED.test(path)) {
      return { error: `${path} is no text (a picture, a document or media): with bash in the browser Linux, \`spindle model --image ${path} "QUESTION"\` has a model look at a picture` };
    }
    const { bytes } = await fromEngine('read', { id: thread, path });
    let whole = new TextDecoder().decode(bytes);
    if (/\.(html?|xml|svg)$/i.test(path)) {
      whole = whole.replace(/<(script|style)\b[\s\S]*?<\/\1\s*>/gi, ' ').replace(/<br\s*\/?>|<\/(p|div|li|tr|h[1-6]|section|article|header|footer|pre|blockquote)>/gi, '\n')
        .replace(/<[^>]+>/g, ' ').replace(/&nbsp;/g, ' ').replace(/&amp;/g, '&').replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&#39;/g, "'")
        .replace(/[ \t]+/g, ' ').replace(/ *\n */g, '\n').replace(/\n{3,}/g, '\n\n').trim();
    }
    text = { text: whole.slice(0, find ? 4 * 1024 * 1024 : limit), total: whole.length };
    how = 'the file itself';
  }
  const lines = [`Address: ${tab.url}`, `Title: ${tab.title || path}`, `Workspace file: ${path} (${how})`];
  if (find) return { content: `${lines.join('\n')}\n\n${matchingLines(text.text, find, limit)}`, details: { tab: tab.id, url: tab.url, title: tab.title } };
  const more = text.total - text.text.length;
  const body = more > 0 ? `${text.text}\n[${more} more characters: ask again with a larger max_chars, or read the file with read_file]` : text.text;
  return { content: `${lines.join('\n')}\n\n${body}`, details: { tab: tab.id, url: tab.url, title: tab.title } };
}

/** What `find` (a page tool's) matches: `/pattern/flags` as a regular expression (case
 * insensitive unless its flags say), anything else as words, case insensitive. */
function matcherOf(find) {
  const text = String(find ?? '');
  const pattern = /^\/(.+)\/([a-z]*)$/s.exec(text);
  if (pattern) {
    try {
      const flags = pattern[2].includes('i') ? pattern[2] : `${pattern[2]}i`;
      const regexp = new RegExp(pattern[1], flags);
      return (line) => regexp.test(line);
    } catch {
      // Not a regular expression Chrome takes: looked for as words.
    }
  }
  const needle = text.toLowerCase();
  return (line) => line.toLowerCase().includes(needle);
}

/** The lines of `text` that `find` matches, each with the line before and after it
 * (runs apart marked by a lone ellipsis), within `limit` characters. */
function matchingLines(text, find, limit) {
  const matches = matcherOf(find);
  const lines = text.split('\n');
  const hits = [];
  lines.forEach((line, at) => { if (line.trim() && matches(line)) hits.push(at); });
  if (!hits.length) return `[no line matches ${JSON.stringify(String(find))}]`;
  const keep = new Set();
  for (const at of hits) for (const near of [at - 1, at, at + 1]) if (near >= 0 && near < lines.length && lines[near].trim()) keep.add(near);
  const out = [];
  let last = -2;
  let size = 0;
  for (const at of [...keep].sort((a, b) => a - b)) {
    if (at !== last + 1 && out.length) out.push('…');
    size += lines[at].length + 1;
    if (size > limit) {
      out.push(`[more matches past ${limit} characters: ask with a larger max_chars, or a narrower find]`);
      break;
    }
    out.push(lines[at]);
    last = at;
  }
  return `[${hits.length} matching line${hits.length === 1 ? '' : 's'}, each with its neighbours]\n${out.join('\n')}`;
}

/** The text of a page that loads as it scrolls (and drops what scrolled away), read
 * while scrolling down `screens` times: each new line once, in order, until the page
 * stops growing, the screens are done or `limit` characters are in. */
async function readScrolling(tab, limit, screens, control, find = null) {
  if (!(await acting())) {
    return { error: 'reading while scrolling moves the page, so it needs page actions on (Settings, Pages); read_page without scroll reads what shows now' };
  }
  const screensMost = Math.min(Math.max(Math.round(Number(screens)) || 1, 1), 30);
  // What was read by then is the answer: the tool's own deadline (30 s) is not far off.
  const until = Date.now() + SCROLL_MS;
  let late = false;
  const seen = new Set();
  const kept = [];
  let chars = 0;
  let quiet = 0;
  let scrolled = 0;
  let last = null;
  for (;;) {
    // Reading an already-dispatched page response can finish, but no further action
    // from this call is admitted after the switches change.
    check(control);
    if (!(await reading())) throw new Error(OFF);
    const [{ result }] = await deadline(chrome.scripting.executeScript({
      target: { tabId: tab.id },
      func: () => ({ url: location.href, title: document.title, text: document.body?.innerText ?? '' }),
    }), DEADLINE_MS, `tab ${tab.id}`);
    last = result;
    let added = 0;
    // Looking for lines: what scrolls by is all kept (the matches alone are answered).
    const most = find ? 4 * 1024 * 1024 : limit;
    for (const line of result.text.split('\n')) {
      const key = line.trim();
      if (!key || seen.has(key) || chars >= most) continue;
      seen.add(key);
      kept.push(line);
      chars += line.length + 1;
      added += 1;
    }
    // Two screens that bring nothing new: the end of what loads.
    quiet = added ? 0 : quiet + 1;
    if (scrolled >= screensMost || quiet >= 2 || chars >= most) break;
    if (Date.now() > until) {
      late = true;
      break;
    }
    await page(tab.id, 'scroll', { to: 'down' }, control);
    scrolled += 1;
    // What a feed loads as it scrolls takes a moment to come.
    await pause(900);
  }
  const why = chars >= most ? `stopped at ${most} characters (ask with a larger max_chars for more)`
    : quiet >= 2 ? 'the page stopped bringing new text'
    : late ? `scrolled ${scrolled} screen${scrolled === 1 ? '' : 's'} in the time a call has (call again for more)`
    : `scrolled ${scrolled} screen${scrolled === 1 ? '' : 's'}`;
  const text = find ? matchingLines(kept.join('\n'), find, limit) : kept.join('\n').slice(0, limit);
  return {
    content: `Address: ${last.url}\nTitle: ${last.title}\nRead while scrolling down (${why}; each line once):\n\n${text}`,
    details: { tab: tab.id, url: last.url, title: last.title, scrolled },
  };
}

async function pageElements({ tab: id, ref, depth, hidden, max_chars: max, find }, control) {
  const tab = await pageTab(id, control);
  const parts = await page(tab.id, 'parts', {
    ref, hidden: Boolean(hidden),
    depth: Math.min(Math.max(Number(depth) || 30, 1), 80),
    max: Math.min(Math.max(Number(max) || 30000, 2000), 100000),
    ...(find != null && String(find) && { find: String(find).slice(0, 500) }),
  }, control);
  return { content: `Address: ${parts.url}\nTitle: ${parts.title} (tab ${tab.id})\n\n${parts.text}`, details: { tab: tab.id, url: parts.url } };
}

async function pageHtml({ tab: id, ref, scripts, max_chars: max }, control) {
  const tab = await pageTab(id, control);
  const html = await page(tab.id, 'html', { ref, scripts: Boolean(scripts), max: Math.min(Math.max(Number(max) || 50000, 1000), 200000) }, control);
  const more = html.total - html.text.length;
  return { content: `${html.text}${more > 0 ? `\n[${more} more characters: ask again with a larger max_chars, or a ref]` : ''}`, details: { tab: tab.id } };
}

/** A picture of a tab: the one showing in its window as Chrome draws it, once the user
 * let Spindle picture pages (no debugger); any tab through Chrome's debugger (page
 * actions on). */
async function screenshot({ tab: id }, control) {
  // Chrome pictures no extension page, its own debugger refused there too: a file of
  // the workspace shown in Spindle's tab (named, or looked at) is read, not pictured.
  const own = id != null ? await chrome.tabs.get(id).catch(() => null) : await lookedAt(control).catch(() => null);
  const view = viewed(own);
  if (view) return { error: `${BROWSER} does not picture extension pages, so tab ${own.id} (${view.path} from the workspace) cannot be pictured; read_page on it gives the file's text as shown, and read_file the file` };
  const tab = await pageTab(id, control);
  refuseWhileDialog(tab.id);
  let base64;
  if (tab.active && (await chrome.permissions.contains({ origins: PICTURE_ACCESS }))) {
    check(control);
    const url = await chrome.tabs.captureVisibleTab(tab.windowId, { format: 'jpeg', quality: 75 });
    // Chrome pictures whatever its window shows: still this tab's web page, or nothing.
    const [shown] = await chrome.tabs.query({ active: true, windowId: tab.windowId });
    if (shown?.id !== tab.id || !/^https?:/.test(shown.url ?? '')) throw new Error(`tab ${tab.id} stopped showing in its window while it was pictured: try again`);
    base64 = url.slice(url.indexOf(',') + 1);
  } else if (await acting()) {
    ({ data: base64 } = await cdp(tab.id, 'Page.captureScreenshot', { format: 'jpeg', quality: 75 }, control));
  } else {
    return {
      error: tab.active
        ? 'Spindle may not picture pages yet: the user lets it with the camera beside the message box (once), or turns page actions on'
        : `tab ${tab.id} is not the one showing in its window: a picture of it needs page actions on (Settings, Pages), or the user to show it`,
    };
  }
  if (!base64) return { error: `tab ${tab.id} gave no picture. ${await whereNow(tab.id)}` };
  // To the millisecond: two pictures never share a name.
  const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\.(\d+)Z$/, '$1');
  const path = `screenshots/${stamp}-tab${tab.id}.jpg`;
  return {
    content: `Saved ${path}: tab ${tab.id}, ${tab.title || '(untitled)'} <${tab.url}>. The user sees it in the panel. To have a model look at it, with bash in this browser's Linux: spindle model --image ${path} "QUESTION"`,
    files: { write_base64: { [path]: base64 } },
    details: { tab: tab.id, screenshot: path },
  };
}

// ---- Acting ------------------------------------------------------------------------

async function navigate({ url, action = 'go', tab: id, new_tab: fresh, keep_open: keep }, control) {
  check(control);
  // A file of the thread's workspace (the engine names its page): opened in a tab of its
  // own for the user, never in place of a page.
  if (typeof url === 'string' && url.startsWith(chrome.runtime.getURL('view.html?'))) {
    const path = new URL(url).searchParams.get('path') ?? '';
    control.acted = true;
    // For the user to read (nothing acts on it after): behind while they write in Spindle.
    const quiet = await typing();
    const tab = await chrome.tabs.create({ url, active: !quiet });
    return { content: `Opened ${path} from the workspace in a tab for the user (tab ${tab.id}${quiet ? `,${QUIET_SAID}` : ''}).`, details: { tab: tab.id } };
  }
  if (url != null) {
    let address;
    try {
      address = new URL(url);
    } catch {
      return { error: `${url} is not an address` };
    }
    if (!/^https?:$/.test(address.protocol)) return { error: 'navigate goes to http(s) addresses only' };
  }
  if (action === 'go' && !url) return { error: 'give the url to go to' };
  let tab;
  let quiet = false;
  if (fresh) {
    if (!url) return { error: 'a new tab needs a url' };
    control.acted = true;
    // Behind only for a thread, which goes on in it (`quietTab`): a call of no thread's
    // would go on in the tab the user looks at instead.
    quiet = Boolean(control.owner) && await typing();
    tab = await chrome.tabs.create({ url, active: !quiet });
    if (control.owner) {
      await noteOpened(tab.id, { ...control.owner, keep: keep === true });
      // The thread's tab behind, or none (one opened in front is the one looked at).
      const key = quietKey(control.owner);
      await (quiet ? chrome.storage.session.set({ [key]: tab.id }) : chrome.storage.session.remove(key)).catch(() => {});
    }
  } else {
    tab = id != null ? await chrome.tabs.get(id).catch(() => null) : await lookedAt(control);
    if (!tab) return { error: id != null ? `there is no tab ${id}` : 'no tab is open' };
    if (!webTab(tab)) return { error: `tab ${tab.id} does not show a web page Spindle may move (a new tab, a browser or extension page): open the address with new_tab` };
    return onTab(tab.id, async () => {
      refuseWhileDialog(tab.id);
      await refuseVetoed(tab.id);
      await asAllowed(tab.id, control);
      check(control);
      control.acted = true;
      try {
        if (action === 'back') await chrome.tabs.goBack(tab.id);
        else if (action === 'forward') await chrome.tabs.goForward(tab.id);
        else if (action === 'reload') await chrome.tabs.reload(tab.id);
        else await chrome.tabs.update(tab.id, { url });
      } catch (error) {
        return { error: `tab ${tab.id} could not go ${action === 'go' ? 'there' : action}: ${error.message}` };
      }
      await settle(tab.id, 20_000, control);
      return { content: await whereNow(tab.id), details: { tab: tab.id } };
    });
  }
  await settle(tab.id, 20_000, control);
  return { content: `${await whereNow(tab.id)}${quiet ? `\n(Opened as tab ${tab.id}${QUIET_SAID}: tools without a tab act on it.)` : ''}`, details: { tab: tab.id } };
}

/** A press and release of `button` at `at` (count: a double click is two). */
async function mouse(tabId, at, button = 'left', count = 1, control) {
  const buttons = { left: 1, right: 2, middle: 4 }[button] ?? 1;
  await cdp(tabId, 'Input.dispatchMouseEvent', { type: 'mouseMoved', x: at.x, y: at.y }, control);
  for (let n = 1; n <= count; n += 1) {
    await cdp(tabId, 'Input.dispatchMouseEvent', { type: 'mousePressed', x: at.x, y: at.y, button, buttons, clickCount: n }, control);
    await cdp(tabId, 'Input.dispatchMouseEvent', { type: 'mouseReleased', x: at.x, y: at.y, button, buttons: 0, clickCount: n }, control);
  }
}

/** Where `ref` is, measured once the debugger is attached (Chrome's bar that says so
 * takes room from the page, moving what is pinned to its bottom). */
async function aim(tabId, ref, control) {
  check(control);
  if (!(await acting())) throw new Error(NO_ACTIONS);
  check(control);
  await attach(tabId);
  await pause(60);
  return page(tabId, 'point', { ref, ...shownOf(control) }, control);
}

/** What the user was shown of a part when asked before acting on it (its description,
 * its words and where it leads), for the page to check it still is that. */
const shownOf = (control) => {
  const allowed = control?.allowed;
  return allowed?.what ? { what: allowed.what, words: allowed.words, dest: allowed.dest } : {};
};

/** What `ref` is (its description, whether a click on it submits a form, its words) and
 * where (the tab's site): read from the page, nothing done to it. */
async function target({ ref, tab: id }, control) {
  const tab = await pageTab(id, control);
  const found = await page(tab.id, 'target', { ref }, control);
  return { content: found.what, details: { ...found, host: new URL(tab.url).host, tab: tab.id } };
}

/** Which tab an action without a `tab` would land on now (the one the user is looking
 * at), and its site and title, so a question asked before it acts names that tab and the
 * action stays there whatever the user looks at by the time they answer. */
async function tabOf({ tab: id, focus: withFocus }, control) {
  const tab = id != null ? await chrome.tabs.get(id).catch(() => null) : await lookedAt(control);
  if (!tab) return { error: id != null ? `there is no tab ${id}` : 'no tab is open' };
  // The page it shows (so the action, once allowed, lands on that page or not at all),
  // and a dialog open on it (so the question says what the dialog asks).
  const document = /^https?:/.test(tab.url ?? '') ? await documentOf(tab.id) : null;
  const open = dialogs.get(tab.id);
  // The part with the focus (where a key such as Enter goes; null: none), when the page
  // says (undefined: it could not be asked).
  const focus = document && withFocus ? await deadline(page(tab.id, 'focused'), 5_000, `tab ${tab.id}`).catch(() => undefined) : undefined;
  return {
    content: `tab ${tab.id}`,
    details: {
      tab: tab.id, host: hostOf(tab.url), url: withoutPart(tab.url), title: tab.title ?? '',
      ...(document && { document }), ...(open && { dialog: { id: open.id, type: open.type, message: open.message } }),
      ...(focus !== undefined && { focus }),
    },
  };
}

/** An address without its `#` part (a page moving within itself changes only that). */
const withoutPart = (url) => String(url ?? '').split('#')[0];

const hostOf = (url) => {
  try {
    return url ? new URL(url).host : '';
  } catch {
    return '';
  }
};

/** The document a tab shows now (Chrome's id for it: another after a reload or a link
 * followed), or null when Spindle cannot look (no access to the page, a dialog holding it). */
async function documentOf(tabId) {
  if (dialogs.has(tabId)) return null;
  try {
    const [shown] = await deadline(chrome.scripting.executeScript({ target: { tabId }, func: () => true, injectImmediately: true }), 5_000, `tab ${tabId}`);
    return shown?.documentId ?? null;
  } catch {
    return null;
  }
}

/** An action the user was asked about first (Settings, Pages, Before acting) acts only
 * on what the question showed them: the same tab, still on the same site and the same
 * page (Chrome's id for it: a reload or a link followed since makes another; where Chrome
 * does not say, when asked or now, the same address but its `#` part), its part still
 * what it was, and for a dialog the same dialog. Else this throws before anything is
 * done. Checked just before the action goes (after any wait for the tab), so nothing the
 * user did not see is acted on meanwhile; actions not asked about pass at once. */
async function asAllowed(tabId, control) {
  const allowed = control?.allowed;
  if (!allowed) return;
  const notDone = 'so it was not done; it can be asked about again';
  if (allowed.tab != null && allowed.tab !== tabId) throw new Error(`the user allowed this on tab ${allowed.tab}, not tab ${tabId}, ${notDone}`);
  if ('dialog' in allowed) {
    if ((dialogs.get(tabId)?.id ?? 0) !== allowed.dialog) throw new Error(`the dialog on tab ${tabId} is not the one the user allowed answering, ${notDone}`);
    return;
  }
  const tab = await chrome.tabs.get(tabId).catch(() => null);
  if (!tab) throw new Error(`there is no tab ${tabId}`);
  const host = hostOf(tab.url);
  if (allowed.host != null && host !== allowed.host) {
    throw new Error(`tab ${tabId} moved from ${allowed.host || 'the page the user saw'} to ${host || 'another page'} since the user allowed this, ${notDone}`);
  }
  const document = allowed.document ? await documentOf(tabId) : null;
  if (document) {
    if (document !== allowed.document) throw new Error(`tab ${tabId} shows another page than the one the user allowed this on (it reloaded or moved on since), ${notDone}`);
  } else if (allowed.url != null && withoutPart(tab.url) !== allowed.url) {
    throw new Error(`tab ${tabId} is at another address than the one the user allowed this on (it moved on since), ${notDone}`);
  }
  // A key goes to the part with the focus: the one the user was shown having it (Enter
  // in another form submits that form).
  if ('focus' in allowed) {
    const focus = await page(tabId, 'focused', {}, control).catch(() => undefined);
    if (focus === undefined || (focus?.ref ?? null) !== allowed.focus) {
      const now = focus === undefined ? 'Spindle could not tell where it is now' : focus === null ? 'no part has it now' : `it is on ${focus.what} now`;
      throw new Error(`the focus on tab ${tabId} is not where it was when the user allowed this (${now}), ${notDone}`);
    }
  }
}

/** Runs `work` (a click, a key) with Chrome catching any file dialog it opens at once (a
 * button that opens its hidden file field): the dialog is the user's computer's, which
 * Spindle cannot fill and which would hold the window. Caught only while the work goes
 * and the moment `settle` waits first, so the user's own clicks there open theirs; best
 * effort (a Chrome that will not catch them acts as before). Says what was caught. */
async function catching(tabId, work, control) {
  choosers.delete(tabId);
  await cdp(tabId, 'Page.setInterceptFileChooserDialog', { enabled: true }, control).catch(() => {});
  try {
    await work();
    await pause(400);
  } finally {
    // Not waited for past a moment: a page dialog the work opened holds the page (and
    // this command, which goes through once the dialog is answered).
    await deadline(chrome.debugger.sendCommand({ tabId }, 'Page.setInterceptFileChooserDialog', { enabled: false }), 1_000, `tab ${tabId}`).catch(() => {});
  }
  return choosers.delete(tabId)
    ? ': it opened a file dialog, which Spindle caught (nothing was chosen): put workspace files in its file field with select (its ref, files)'
    : '';
}

/** Something else lies over the target: nothing is clicked or typed there. */
const coveredError = (at, ref) => ({
  error: `${at.what} [${ref}] is covered by ${at.covered}, so nothing was done (a click would land on that); close or move what covers it, or act on that instead`,
});

async function click({ ref, tab: id, double, button = 'left' }, control) {
  const tab = await pageTab(id, control);
  return onTab(tab.id, async () => {
    refuseWhileDialog(tab.id);
    const at = await aim(tab.id, ref, control);
    if (at.covered) return coveredError(at, ref);
    // Its dialog is the user's computer's, which Spindle cannot fill (and which holds the
    // window until someone does).
    if (at.file) return { error: `${at.what} [${ref}] opens a file dialog, which Spindle cannot fill: put workspace files in it with select (ref ${ref}, files)` };
    const caught = await catching(tab.id, () => mouse(tab.id, at, button, double ? 2 : 1, control), control);
    await settle(tab.id, 10_000, control, 0);
    return { content: `Clicked ${at.what} [${ref}]${caught}. ${await whereNow(tab.id)}`, details: { tab: tab.id } };
  });
}

async function type(input, control) {
  const tab = await pageTab(input.tab, control);
  return onTab(tab.id, () => typeInto(tab, input, control));
}

async function typeInto(tab, { ref, text, clear, submit }, control) {
  refuseWhileDialog(tab.id);
  const at = await aim(tab.id, ref, control);
  if (!at.field) return { error: `${at.what} [${ref}] is not a field one types into${at.file ? ': put workspace files in it with select (files)' : ''}` };
  if (at.covered) return coveredError(at, ref);
  await mouse(tab.id, at, 'left', 1, control);
  // After what the field holds (the click may have put the caret anywhere), or over it.
  await page(tab.id, 'caret', { ref, all: Boolean(clear) }, control);
  if (text) await cdp(tab.id, 'Input.insertText', { text: String(text) }, control);
  else if (clear) await key(tab.id, 'Backspace', control);
  if (submit) await key(tab.id, 'Enter', control);
  await settle(tab.id, submit ? 10_000 : 0, control);
  const value = await page(tab.id, 'value', { ref }, control).catch(() => null);
  const holds = value == null ? '' : ` It holds ${JSON.stringify(value)}.`;
  return { content: `Typed into ${at.what} [${ref}]${submit ? ' and pressed Enter' : ''}.${holds} ${await whereNow(tab.id)}`, details: { tab: tab.id } };
}

async function select({ ref, values, upload, tab: id }, control) {
  const tab = await pageTab(id, control);
  // `upload`: the files, as the engine read them from the thread's workspace.
  if (Array.isArray(upload)) return onTab(tab.id, () => uploadIn(tab, ref, upload, control));
  if (!Array.isArray(values)) return { error: 'give values to choose, or files to put in a file field' };
  return onTab(tab.id, () => selectIn(tab, ref, values, control));
}

async function selectIn(tab, ref, values, control) {
  const chosen = await page(tab.id, 'select', { ref, values: values.map(String), ...shownOf(control) }, control);
  await settle(tab.id, 5_000, control);
  return { content: `Chose ${chosen.map((text) => JSON.stringify(text)).join(', ')} in [${ref}]. ${await whereNow(tab.id)}`, details: { tab: tab.id } };
}

/** Base64 characters a call takes into the page (a multiple of 4, so each decodes alone). */
const PART = 4 * 1024 * 1024;

/** Puts files in the file field at (or beside) `ref`, as if the user picked them in its
 * dialog: their bytes go into the page in parts, then into the field at once. */
async function uploadIn(tab, ref, upload, control) {
  refuseWhileDialog(tab.id);
  // The debugger on the tab first (refused, nothing goes in): a dialog the page opens
  // as it takes the files (a file too large, say) is then seen and said, as after any
  // action.
  check(control);
  await attach(tab.id);
  const key = crypto.randomUUID();
  let first = true;
  try {
    for (const [index, file] of upload.entries()) {
      const text = String(file.base64 ?? '');
      for (let at = 0; at === 0 || at < text.length; at += PART) {
        await page(tab.id, 'file_part', { key, index, first, data: text.slice(at, at + PART) }, control);
        first = false;
      }
    }
  } catch (error) {
    // Cut off before the field took them: what went in leaves the page (best effort).
    await page(tab.id, 'file_part', { key, index: 0, first: true, data: '', drop: true }).catch(() => {});
    throw error;
  }
  const files = upload.map(({ name, type, size }) => ({ name: String(name), type: String(type ?? ''), size: Number(size) }));
  // Still attached however long the parts took (an idle debugger is let go at 20 s).
  await attach(tab.id);
  keepAttached(tab.id);
  const put = await page(tab.id, 'files', { ref, key, files, ...shownOf(control) }, control);
  await settle(tab.id, 5_000, control);
  const field = put.field === ref ? `[${ref}]` : `[${put.field}] (the file field of [${ref}])`;
  return { content: `Put ${put.put.join(', ')} in ${put.what} ${field}. ${await whereNow(tab.id)}`, details: { tab: tab.id } };
}

async function press({ key: name, tab: id }, control) {
  const tab = await pageTab(id, control);
  return onTab(tab.id, async () => {
    refuseWhileDialog(tab.id);
    await asAllowed(tab.id, control);
    // Checked again once the debugger is attached (that can take a moment): the last
    // thing before the key goes.
    if (control?.allowed) {
      check(control);
      await attach(tab.id);
      await asAllowed(tab.id, control);
    }
    // Enter or Space on a file field or its button opens a file dialog as a click does.
    const caught = await catching(tab.id, () => key(tab.id, name, control), control);
    await settle(tab.id, 5_000, control, 0);
    return { content: `Pressed ${name}${caught}. ${await whereNow(tab.id)}`, details: { tab: tab.id } };
  });
}

async function scroll({ ref, to = 'down', tab: id }, control) {
  const tab = await pageTab(id, control);
  const at = await onTab(tab.id, () => page(tab.id, 'scroll', { ref, to }, control));
  const where = `${Math.round(at.y)} of ${Math.round(at.height)} pixels down, ${Math.round(at.view)} shown`;
  return { content: `${ref ? `Scrolled [${ref}] into view` : `Scrolled ${at.pane ? `${at.pane} ` : ''}${to}`}: ${where}.`, details: { tab: tab.id } };
}

async function waitFor({ text, selector, gone, seconds, tab: id }, control) {
  if (!text && !selector) return { error: 'give text or a selector to wait for' };
  const tab = await pageTab(id, control);
  const limit = Math.min(Math.max(Number(seconds) || 10, 0.5), 25) * 1000;
  const until = Date.now() + limit;
  const what = text ? JSON.stringify(text) : selector;
  for (;;) {
    check(control);
    if (!(await chrome.tabs.get(tab.id).catch(() => null))) return { error: `tab ${tab.id} closed` };
    let there = null;
    try {
      there = await page(tab.id, 'has', { text, selector }, control);
    } catch (error) {
      // A page between documents answers nothing yet; anything else is the answer.
      const between = /frame|document|navigat|did not answer|Cannot access/i.test(error.message);
      if (!between || /selector|chrome-error/i.test(error.message)) throw error;
    }
    if (there != null && there !== Boolean(gone)) return { content: `${what} ${gone ? 'is gone' : 'shows'}. ${await whereNow(tab.id)}`, details: { tab: tab.id } };
    if (Date.now() >= until) return { error: `${what} ${gone ? 'still shows' : 'did not show'} within ${limit / 1000} s. ${await whereNow(tab.id)}` };
    await pause(250);
  }
}

async function dialog({ accept, text, tab: id }, control) {
  const tab = id != null ? await chrome.tabs.get(id).catch(() => null) : await lookedAt(control);
  if (!tab) return { error: id != null ? `there is no tab ${id}` : 'no tab is open' };
  const open = dialogs.get(tab.id);
  if (!open) return { error: `no dialog is open on tab ${tab.id}` };
  await asAllowed(tab.id, control);
  await cdp(tab.id, 'Page.handleJavaScriptDialog', { accept: Boolean(accept), ...(text != null && { promptText: String(text) }) }, control);
  dialogs.delete(tab.id);
  await settle(tab.id, 5_000, control);
  return { content: `${accept ? 'Accepted' : 'Dismissed'} the ${open.type} ${JSON.stringify(open.message)}. ${await whereNow(tab.id)}`, details: { tab: tab.id } };
}

async function closeTab({ tab: id }, control) {
  const tab = await chrome.tabs.get(id).catch(() => null);
  if (!tab) return { error: `there is no tab ${id}` };
  if (!webTab(tab)) return { error: `tab ${id} does not show a web page: Spindle closes only those` };
  await refuseVetoed(id);
  await asAllowed(id, control);
  check(control);
  control.acted = true;
  await chrome.tabs.remove(id);
  return { content: `Closed tab ${id} (${tab.title || tab.url || 'untitled'}).` };
}

// ---- Tabs Spindle opened ------------------------------------------------------------

/** Tabs Spindle opened for a thread's work: with navigate's new_tab, or by a page on
 * Spindle's click, key press or choice there (a link to a new tab, say). Kept in
 * Chrome's session storage, one key per tab (`opened:<tab>` -> {thread, task, keep}), so
 * a resting service worker still knows them; each closes once its thread's turn is done
 * (`closeOpened`), unless the model kept it open for the user (`keep_open`) or the user
 * pinned it. Files of a workspace opened for the user to read are not among them. */
const OPENED = 'opened:';
/** Until when the user writes in a Spindle panel's message box (said by panel.mjs): tabs
 * Spindle opens meanwhile open behind the one showing, as Chrome gives the keys to a tab
 * it shows, which would take them from the box mid-word. */
const TYPING = 'typing';
/** How far ahead a panel's word holds at most (a panel closed mid-word holds no longer). */
const TYPING_MAX_MS = 60_000;
/** A tab a thread opened behind the one showing (`quiet:THREAD`, `:task` for its tasks'). */
const QUIET = 'quiet:';
const QUIET_SAID = ' behind the one showing, as the user is writing in Spindle';
const quietKey = (owner) => `${QUIET}${owner.thread}${owner.task ? ':task' : ''}`;

/** Whether the user is writing in a Spindle panel now. */
async function typing() {
  const { [TYPING]: until = 0 } = await chrome.storage.session.get(TYPING).catch(() => ({}));
  return Date.now() < until;
}

/** The tab `owner` (a thread, or its tasks) opened behind the one showing, while it is
 * there and still not shown; shown since, or gone, it is let go of. */
async function quietTab(owner) {
  if (!owner?.thread) return null;
  const key = quietKey(owner);
  const { [key]: id } = await chrome.storage.session.get(key).catch(() => ({}));
  if (!Number.isInteger(id)) return null;
  const tab = await chrome.tabs.get(id).catch(() => null);
  if (tab && !tab.active) return tab;
  await chrome.storage.session.remove(key).catch(() => {});
  return null;
}
/** How long after an action a window its page opens still counts as the action's. */
const OPENED_GRACE_MS = 3000;
/** The tools whose action may have a page open a tab. */
const OPENING = new Set(['click', 'press', 'type', 'select', 'dialog']);
/** Recent actions (`{owner, tab, from, to}`), windows pages Spindle's debugger holds said
 * they open (`{source, url, at}`: Page.windowOpen), and tabs Chrome made (`{tab, url,
 * at}`), matched by `claimOpened`; each forgotten after `RECENT_MS`. (A tab's
 * openerTabId does not say: Chrome gives the tab active at the time, not the page.) */
const actions = [];
const opens = [];
const born = [];
const RECENT_MS = 30_000;
/** How far apart a page's word that it opens a window and the tab Chrome makes for it
 * may come. */
const MATCH_MS = 5000;
/** How long a tab made with no address yet is waited on for Chrome to say it. */
const ADDRESS_WAIT_MS = 1500;
/** Opening actions under way: tabs are watched while any is, and until no window one
 * of them may have opened can still come (`watchEnd` ends it then). */
let opening = 0;
let watchEnd = null;
/** Records being written: `closeOpened` waits for them. */
const noting = new Set();

function noteOpened(tabId, owner) {
  const write = chrome.storage.session.set({ [`${OPENED}${tabId}`]: { thread: owner.thread, task: Boolean(owner.task), keep: Boolean(owner.keep) } }).catch(() => {});
  noting.add(write);
  write.finally(() => noting.delete(write));
  return write;
}

/** A tab Chrome made for a window a page opened during (or just after) one of Spindle's
 * actions on that page: the action's thread's. */
function claimOpened() {
  const now = Date.now();
  for (const list of [actions, opens, born]) {
    for (let at = list.length - 1; at >= 0; at--) if ((list[at].to ?? list[at].at) < now - RECENT_MS) list.splice(at, 1);
  }
  for (let at = born.length - 1; at >= 0; at--) {
    const made = born[at];
    let by = null;
    // The same address (a window opened with none is a blank page). A tab whose address
    // Chrome does not say is never matched: it may be the user's own new tab, whose
    // address (chrome://newtab) is hidden from extensions; left open is the safe side.
    const open = opens.find((one) => Math.abs(one.at - made.at) <= MATCH_MS && made.url === (one.url || 'about:blank')
      && (by = actions.find((action) => action.tab === one.source && one.at >= action.from && one.at <= action.to)));
    if (!open) continue;
    born.splice(at, 1);
    opens.splice(opens.indexOf(open), 1);
    noteOpened(made.tab, by.owner);
  }
  // Watched no longer than needed: while the service worker is up, each event a listener
  // takes keeps it up, and tabs change all day.
  clearTimeout(watchEnd);
  watchEnd = null;
  if (opening) return;
  const until = Math.max(0, ...actions.map((action) => action.to + MATCH_MS));
  if (until > now) watchEnd = setTimeout(claimOpened, until - now + 50);
  else unwatchTabs();
}

/** New tabs, watched only while a thread's action may open one, a few seconds at a time
 * (never the whole day: a listener added here does not wake a resting service worker for
 * each tab the user opens, and is taken off again by `claimOpened`). */
function tabMade(tab) {
  if (tab.id == null) return;
  const made = { tab: tab.id, url: tab.pendingUrl || tab.url || '', at: Date.now() };
  born.push(made);
  if (!made.url) {
    // Made before its first address was set (a link's new tab is): Chrome says it once
    // the tab starts loading, or it is looked at once more after a moment.
    made.known = new Promise((resolve) => { made.found = resolve; });
    setTimeout(() => chrome.tabs.get(made.tab).then((now) => addressed(made.tab, now.pendingUrl || now.url), () => addressed(made.tab)), ADDRESS_WAIT_MS);
  }
  claimOpened();
}
function addressed(tabId, url = '') {
  const made = born.find((one) => one.tab === tabId && one.found);
  if (!made) return;
  if (url) made.url = url;
  made.found();
  delete made.found;
  claimOpened();
}
function tabChanged(tabId, change, tab) {
  const url = tab?.pendingUrl || tab?.url || change?.url;
  if (url) addressed(tabId, url);
}
let watching = false;
function watchTabs() {
  if (watching) return;
  watching = true;
  chrome.tabs.onCreated.addListener(tabMade);
  chrome.tabs.onUpdated?.addListener(tabChanged);
}
function unwatchTabs() {
  if (!watching || born.some((made) => made.found)) return;
  watching = false;
  chrome.tabs.onCreated.removeListener(tabMade);
  chrome.tabs.onUpdated?.removeListener(tabChanged);
}

chrome.tabs.onRemoved.addListener((tabId) => { chrome.storage.session.remove(`${OPENED}${tabId}`).catch(() => {}); });

/** Closes the tabs Spindle opened for `thread`: its turns' (`tasks`: its background
 * tasks' too). Each is forgotten; one the model kept open for the user, one the user
 * pinned, or one no longer showing a web page is left open. */
async function closeOpened(thread, { tasks = false } = {}) {
  if (!thread) return;
  // A tab made just now whose address Chrome has yet to say, and records being written,
  // may be this turn's: waited on (a moment at most).
  const pending = born.filter((made) => made.found).map((made) => made.known);
  if (pending.length) await Promise.race([Promise.all(pending), pause(ADDRESS_WAIT_MS + 500)]);
  await Promise.allSettled([...noting]);
  const kept = await chrome.storage.session.get(null);
  const mine = Object.entries(kept).filter(([key, owner]) => key.startsWith(OPENED) && owner?.thread === thread && (tasks || !owner.task));
  if (!mine.length) return;
  await chrome.storage.session.remove(mine.map(([key]) => key));
  let closing = [];
  for (const [key, owner] of mine) {
    if (owner.keep) continue;
    const tab = await chrome.tabs.get(Number(key.slice(OPENED.length))).catch(() => null);
    // A web page, or one still loading its first (its address pending until then).
    if (!tab || tab.pinned || !(webTab(tab) || (!tab.url && /^https?:/.test(tab.pendingUrl ?? '')))) continue;
    closing.push(tab);
  }
  // A browser window holding only these keeps its active one: closing them all would
  // close the window (and the panel beside it). A popup a page opened goes whole.
  for (const windowId of new Set(closing.map((tab) => tab.windowId))) {
    if ((await Promise.resolve(chrome.windows?.get(windowId)).catch(() => null))?.type === 'popup') continue;
    const all = await chrome.tabs.query({ windowId }).catch(() => []);
    const here = closing.filter((tab) => tab.windowId === windowId);
    if (all.length > here.length) continue;
    const stays = here.find((tab) => tab.active) ?? here[0];
    closing = closing.filter((tab) => tab !== stays);
  }
  closing = closing.map((tab) => tab.id);
  if (closing.length) await chrome.tabs.remove(closing).catch(async () => {
    // One already gone takes the others' close with it: each on its own then.
    for (const id of closing) await chrome.tabs.remove(id).catch(() => {});
  });
}

// ---- Chrome's debugger: input as the user's, dialogs, and a tab's network ------------

/** Tabs Spindle's debugger is attached to -> the timer that detaches it when unused (none
 * while their network is watched). Chrome shows that Spindle is debugging meanwhile. */
const attached = new Map();
/** Tabs being attached now -> that attach (one at a time per tab). */
const attaching = new Map();
/** Tabs where the user closed Chrome's debugging bar: Spindle does not act on them again
 * until the user turns page actions off and on (consent given anew). Kept in Chrome's
 * session storage, so a resting service worker does not forget them, one key per tab:
 * closing the bar detaches every tab at once, and their vetoes are written side by side,
 * so none may rewrite a list another is writing. */
const VETOED = 'vetoed-tab:';
async function vetoed(tabId) {
  const key = `${VETOED}${tabId}`;
  const { [key]: vetoedHere = false } = await chrome.storage.session.get(key).catch(() => ({}));
  return vetoedHere;
}
async function veto(tabId) {
  await chrome.storage.session.set({ [`${VETOED}${tabId}`]: true });
}
/** Refuses any action on a tab the user took back by closing the debugging bar there:
 * its input (through the debugger) and what needs no debugger (choosing, scrolling,
 * moving or closing the tab) alike. */
async function refuseVetoed(tabId) {
  if (await vetoed(tabId)) {
    throw new Error(`the user stopped Spindle acting on tab ${tabId} (they closed ${BROWSER}'s debugging bar); Spindle will not act on it again until the user turns page actions off and on in Spindle's settings`);
  }
}
async function unveto(tabId) {
  if (tabId !== undefined) return chrome.storage.session.remove(`${VETOED}${tabId}`);
  const kept = await chrome.storage.session.get(null);
  await chrome.storage.session.remove(Object.keys(kept).filter((key) => key.startsWith(VETOED)));
}
/** Actions on a tab, one at a time: a click or typing is never interleaved with another
 * thread's on the same tab (focus, then insert, stay together). */
const tabTurns = new Map();
function onTab(tabId, work) {
  const run = (tabTurns.get(tabId) ?? Promise.resolve()).then(work);
  const settled = run.catch(() => {});
  tabTurns.set(tabId, settled);
  settled.then(() => { if (tabTurns.get(tabId) === settled) tabTurns.delete(tabId); });
  return run;
}
/** Tabs showing a JavaScript dialog -> {id, type, message, at} (`id`: each dialog's own,
 * never another's, even after this worker restarts, so an answer the user allowed goes to
 * that dialog only); callers waiting for one. */
const dialogs = new Map();
const dialogWaiters = new Map();
/** Tabs where a click opened a file dialog Chrome caught (`click`). */
const choosers = new Set();
/** Tabs whose network is watched -> their requests, oldest first, by entry id. */
const watched = new Map();
const MAX_REQUESTS = 500;

function refuseWhileDialog(tabId) {
  const open = dialogs.get(tabId);
  if (open) throw new Error(`a ${open.type} dialog is open on tab ${tabId}: ${JSON.stringify(open.message)}. Answer it with dialog (accept or dismiss) first`);
}

async function attach(tabId) {
  if (attached.has(tabId)) return;
  await refuseVetoed(tabId);
  if (!attaching.has(tabId)) {
    attaching.set(tabId, (async () => {
      try {
        await chrome.debugger.attach({ tabId }, '1.3');
      } catch (error) {
        // Spindle's own earlier attach (before this worker last rested) still holds.
        if (!/already attached/i.test(error.message)) throw new Error(`${BROWSER} would not let Spindle act on tab ${tabId}: ${error.message}`);
        // A click that worker cut off may have left Chrome catching file dialogs there:
        // off again (not waited for past a moment: a page dialog may hold the page).
        await deadline(chrome.debugger.sendCommand({ tabId }, 'Page.setInterceptFileChooserDialog', { enabled: false }), 1_000, `tab ${tabId}`).catch(() => {});
      }
      attached.set(tabId, setTimeout(() => idleDetach(tabId), 20_000));
      // Dialogs are said as they open (an action waiting on one would wait forever).
      await chrome.debugger.sendCommand({ tabId }, 'Page.enable').catch(() => {});
    })().finally(() => attaching.delete(tabId)));
  }
  await attaching.get(tabId);
}

async function cdp(tabId, method, params = {}, control) {
  if (!(await acting())) throw new Error(NO_ACTIONS);
  check(control);
  await attach(tabId);
  if (!(await acting())) throw new Error(NO_ACTIONS);
  check(control);
  // Input and a dialog's answer act on the page; a picture or the network only look.
  if (control && /^Input\.|^Page\.handleJavaScriptDialog$/.test(method)) control.acted = true;
  keepAttached(tabId);
  // A dialog the command opened ends the wait for it (it answers once the dialog is).
  let opened;
  const dialogOpened = new Promise((resolve) => {
    opened = resolve;
    if (!dialogWaiters.has(tabId)) dialogWaiters.set(tabId, new Set());
    dialogWaiters.get(tabId).add(resolve);
  });
  try {
    return await deadline(Promise.race([chrome.debugger.sendCommand({ tabId }, method, params), dialogOpened.then(() => ({}))]), DEADLINE_MS, `tab ${tabId}`);
  } finally {
    dialogWaiters.get(tabId)?.delete(opened);
    if (!dialogWaiters.get(tabId)?.size) dialogWaiters.delete(tabId);
  }
}

/** The debugger stays on `tabId` for another idle while (none while its network is watched). */
function keepAttached(tabId) {
  clearTimeout(attached.get(tabId));
  attached.set(tabId, watched.has(tabId) ? null : setTimeout(() => idleDetach(tabId), 20_000));
}

function forget(tabId) {
  clearTimeout(attached.get(tabId));
  attached.delete(tabId);
  watched.delete(tabId);
  dialogs.delete(tabId);
  choosers.delete(tabId);
}

function detach(tabId) {
  forget(tabId);
  chrome.debugger.detach({ tabId }).catch(() => {});
}

/** How long an open dialog keeps the debugger on its tab: past the wait for the user's
 * answer to a question about answering it (10 minutes). */
const DIALOG_KEEP_MS = 11 * 60_000;
/** Lets go of an unused tab's debugger, but not while a dialog it saw there is open (its
 * record goes with the debugger: an answer to it, asked about meanwhile, would find none,
 * and reads would no longer be held back by it), for at most DIALOG_KEEP_MS. */
function idleDetach(tabId) {
  const open = dialogs.get(tabId);
  if (open && Date.now() - (open.at ?? 0) < DIALOG_KEEP_MS && attached.has(tabId)) {
    attached.set(tabId, setTimeout(() => idleDetach(tabId), 20_000));
    return;
  }
  detach(tabId);
}

// A closed tab's veto goes with it.
chrome.tabs.onRemoved.addListener((tabId) => { unveto(tabId).catch(() => {}); });
chrome.debugger.onDetach.addListener(({ tabId }, reason) => {
  forget(tabId);
  if (reason === 'canceled_by_user') veto(tabId).catch(() => {});
});
chrome.debugger.onEvent.addListener(({ tabId }, method, params) => {
  if (method === 'Page.fileChooserOpened') {
    choosers.add(tabId);
  } else if (method === 'Page.javascriptDialogOpening') {
    dialogs.set(tabId, { id: crypto.randomUUID(), type: params.type, message: String(params.message ?? '').slice(0, 500), at: Date.now() });
    for (const resolve of dialogWaiters.get(tabId) ?? []) resolve();
    dialogWaiters.delete(tabId);
  } else if (method === 'Page.javascriptDialogClosed') {
    dialogs.delete(tabId);
  } else if (method === 'Page.windowOpen') {
    // A link to a new tab, window.open...: its tab is the thread's if Spindle's action did it.
    opens.push({ source: tabId, url: String(params.url ?? ''), at: Date.now() });
    claimOpened();
  } else {
    recordNetwork(tabId, method, params);
  }
});

/** Page actions off, or Chrome's access taken back: Spindle lets go of every tab (an
 * attach under way finishes first, so it is let go of too). */
async function letGo() {
  if (await acting()) return;
  for (const control of calls.values()) control.cancelled = true;
  await Promise.allSettled([...attaching.values()]);
  // Tabs attached before this worker last rested too (detaching one Spindle does not
  // hold changes nothing).
  const tabs = new Set(attached.keys());
  for (const target of await chrome.debugger.getTargets()) if (target.attached && target.tabId != null) tabs.add(target.tabId);
  for (const tabId of tabs) detach(tabId);
  // Page actions off: turning them on again is the user's consent anew.
  await unveto().catch(() => {});
}
// Chrome's access to sites taken back (in Spindle or Chrome's own settings) turns both
// switches off: giving it again later, for a thread's network, turns nothing back on.
chrome.permissions.onRemoved.addListener(async ({ origins = [] }) => {
  if (origins.some((origin) => PAGE_ACCESS.includes(origin))) {
    // Before acting (`confirm`) is the user's choice for when actions come back: kept.
    const kept = await read('kv', 'pages').catch(() => null);
    await write('kv', 'pages', pagesRecord(kept, { reading: false, acting: false })).catch(() => {});
  }
  await letGo().catch(() => {});
});

/** An address as the log keeps it (data: addresses can be megabytes). */
const logged = (url) => (url.startsWith('data:') ? `${url.slice(0, 40)}… (${url.length} characters)` : url.slice(0, 1000));

function recordNetwork(tabId, method, params) {
  const log = watched.get(tabId);
  if (!log) return;
  if (method === 'Network.requestWillBeSent') {
    // A redirect: the hop it ends stays in the log as its own entry.
    const before = log.get(params.requestId);
    if (before && params.redirectResponse) {
      log.delete(params.requestId);
      const hop = `${params.requestId}.${[...log.keys()].filter((id) => id.startsWith(`${params.requestId}.`)).length + 1}`;
      log.set(hop, { ...before, id: hop, status: params.redirectResponse.status, redirected: true });
    }
    log.set(params.requestId, { id: params.requestId, method: params.request.method, url: logged(params.request.url), type: params.type ?? '', status: '…', size: null });
    while (log.size > MAX_REQUESTS) log.delete(log.keys().next().value);
    return;
  }
  const entry = log.get(params.requestId);
  if (!entry) return;
  if (method === 'Network.responseReceived') {
    entry.status = params.response.status;
    entry.type = params.type ?? entry.type;
  } else if (method === 'Network.loadingFinished') {
    entry.size = params.encodedDataLength;
  } else if (method === 'Network.loadingFailed') {
    entry.status = `failed (${params.errorText})`;
  }
}

async function network({ action, tab: id, filter, body }, control) {
  if (action === 'stop') {
    // Whatever the tab shows now (it may have left the web).
    const tab = id != null ? await chrome.tabs.get(id).catch(() => null) : await lookedAt(control);
    if (!tab) return { error: id != null ? `there is no tab ${id}` : 'no tab is open' };
    if (watched.has(tab.id)) await cdp(tab.id, 'Network.disable', {}, control).catch(() => {});
    detach(tab.id);
    return { content: `Stopped watching tab ${tab.id}'s network.` };
  }
  const tab = await pageTab(id, control);
  if (action === 'start') {
    const log = watched.get(tab.id) ?? new Map();
    watched.set(tab.id, log);
    try {
      await cdp(tab.id, 'Network.enable', {}, control);
    } catch (error) {
      if (!log.size) watched.delete(tab.id);
      throw error;
    }
    return { content: `Watching tab ${tab.id}'s network from now on (reload it with navigate to see its page load).` };
  }
  const log = watched.get(tab.id);
  if (!log) return { error: `tab ${tab.id}'s network is not watched (start it; ${BROWSER} ends a watch when its debugging bar is closed)` };
  if (body) {
    const entry = log.get(body);
    if (!entry) return { error: `there is no request ${body} in tab ${tab.id}'s log` };
    if (entry.redirected) return { error: `${BROWSER} does not keep a redirect hop's body; read the final request instead` };
    const answer = await cdp(tab.id, 'Network.getResponseBody', { requestId: entry.id }, control).catch((error) => ({ error: error.message }));
    if (answer.error || answer.body == null) return { error: `that response's body is not kept (${answer.error ?? 'none was given'})` };
    if (answer.base64Encoded) return { content: `[a binary body of about ${Math.round((answer.body.length * 3) / 4)} bytes]` };
    const text = answer.body.slice(0, 200000);
    return { content: text + (answer.body.length > text.length ? `\n[${answer.body.length - text.length} more characters]` : '') };
  }
  const rows = [...log.values()].filter((entry) => !filter || entry.url.includes(filter));
  const lines = rows.map((entry) => `${entry.id} ${entry.method} ${entry.status} ${entry.type}${entry.size != null ? ` ${entry.size}B` : ''} ${entry.url.slice(0, 300)}`);
  return { content: lines.length ? lines.join('\n') : 'No requests yet.', details: { tab: tab.id, requests: rows.length } };
}

// ---- Keys --------------------------------------------------------------------------

/** Named keys as the user writes them: [Windows key code, key, text it types]. */
const NAMED = {
  Enter: [13, 'Enter', '\r'], Tab: [9, 'Tab'], Escape: [27, 'Escape'], Backspace: [8, 'Backspace'], Delete: [46, 'Delete'],
  ArrowUp: [38, 'ArrowUp'], ArrowDown: [40, 'ArrowDown'], ArrowLeft: [37, 'ArrowLeft'], ArrowRight: [39, 'ArrowRight'],
  Home: [36, 'Home'], End: [35, 'End'], PageUp: [33, 'PageUp'], PageDown: [34, 'PageDown'], Space: [32, ' ', ' '],
  Insert: [45, 'Insert'], ...Object.fromEntries(Array.from({ length: 12 }, (_, n) => [`F${n + 1}`, [112 + n, `F${n + 1}`]])),
};
const ALIASES = { esc: 'Escape', return: 'Enter', up: 'ArrowUp', down: 'ArrowDown', left: 'ArrowLeft', right: 'ArrowRight', del: 'Delete', space: 'Space', pgup: 'PageUp', pgdn: 'PageDown' };
const MODIFIERS = { alt: 1, option: 1, control: 2, ctrl: 2, meta: 4, cmd: 4, command: 4, shift: 8 };
/** A US keyboard: what Shift types on each key, and each punctuation key's codes. */
const SHIFTED = { '!': '1', '@': '2', '#': '3', $: '4', '%': '5', '^': '6', '&': '7', '*': '8', '(': '9', ')': '0', _: '-', '+': '=', '{': '[', '}': ']', '|': '\\', ':': ';', '"': "'", '<': ',', '>': '.', '?': '/', '~': '`' };
const UNSHIFTED = Object.fromEntries(Object.entries(SHIFTED).map(([shifted, plain]) => [plain, shifted]));
const PUNCTUATION = { '-': [189, 'Minus'], '=': [187, 'Equal'], '[': [219, 'BracketLeft'], ']': [221, 'BracketRight'], '\\': [220, 'Backslash'], ';': [186, 'Semicolon'], "'": [222, 'Quote'], ',': [188, 'Comma'], '.': [190, 'Period'], '/': [191, 'Slash'], '`': [192, 'Backquote'] };

/** Chrome's key event for one key with modifiers (null: a character no key types). */
function keyEvent(name, modifiers) {
  const named = NAMED[name] ?? NAMED[ALIASES[name.toLowerCase()]] ?? Object.values(NAMED).find(([, key]) => key.toLowerCase() === name.toLowerCase());
  if (named) {
    const [code, key, text] = named;
    return { key, code: key === ' ' ? 'Space' : key, windowsVirtualKeyCode: code, nativeVirtualKeyCode: code, modifiers, ...(text && !(modifiers & 7) && { text, unmodifiedText: text }) };
  }
  if ([...name].length !== 1) throw new Error(`${name} is not a key Spindle knows (Enter, Escape, Tab, ArrowDown, PageDown, F5, a letter...)`);
  let plain = name;
  let shift = Boolean(modifiers & 8);
  if (SHIFTED[name]) [plain, shift] = [SHIFTED[name], true];
  else if (/[A-Z]/.test(name)) [plain, shift] = [name.toLowerCase(), true];
  let code;
  let dom;
  if (/[a-z]/.test(plain)) [code, dom] = [plain.toUpperCase().charCodeAt(0), `Key${plain.toUpperCase()}`];
  else if (/[0-9]/.test(plain)) [code, dom] = [plain.charCodeAt(0), `Digit${plain}`];
  else if (PUNCTUATION[plain]) [code, dom] = PUNCTUATION[plain];
  else return null;
  const all = modifiers | (shift ? 8 : 0);
  const typed = shift ? (/[a-z]/.test(plain) ? plain.toUpperCase() : UNSHIFTED[plain] ?? plain) : plain;
  return { key: typed, code: dom, windowsVirtualKeyCode: code, nativeVirtualKeyCode: code, modifiers: all, ...(!(all & 7) && { text: typed, unmodifiedText: plain }) };
}

async function key(tabId, combination, control) {
  // "+" alone or last ("Control++") is the key itself.
  const parts = String(combination).split('+');
  let name = parts.pop();
  if (name === '' && parts.length) {
    parts.pop();
    name = '+';
  }
  let modifiers = 0;
  for (const part of parts) {
    const bit = MODIFIERS[part.trim().toLowerCase()];
    if (!bit) throw new Error(`${part} is not a modifier (Alt, Control, Meta, Shift)`);
    modifiers |= bit;
  }
  const event = keyEvent(name, modifiers);
  // A character no US key types (é, ✓): typed as text.
  if (!event) {
    if (modifiers & 7) throw new Error(`${combination}: Spindle presses ${name} only as text, without Alt, Control or Meta`);
    await cdp(tabId, 'Input.insertText', { text: name }, control);
    return;
  }
  await cdp(tabId, 'Input.dispatchKeyEvent', { type: event.text ? 'keyDown' : 'rawKeyDown', ...event }, control);
  await cdp(tabId, 'Input.dispatchKeyEvent', { type: 'keyUp', ...event }, control);
}

// ---- In the page ---------------------------------------------------------------------

/** One operation in the page, run in Spindle's isolated world there (where its refs
 * live, as long as the page does; the page never sees them). Self-contained: Chrome
 * sends the function itself. Answers {value} or {error}. */
function inPage(op, args) {
  if (args.expires && Date.now() >= args.expires) return { error: 'the page tool timed out before it reached this page' };
  // Each page's refs carry a mark of their own, so a ref read on one page (or tab) never
  // names an element of another.
  const state = (globalThis.__spindle ??= {
    mark: Array.from(crypto.getRandomValues(new Uint8Array(3)), (byte) => 'abcdefghijkmnpqrstuvwxyz'[byte % 24]).join(''),
    next: 0, refs: new Map(), ids: new WeakMap(),
  });
  const clean = (text, most = 100) => {
    const flat = String(text ?? '').replace(/\s+/g, ' ').trim();
    return flat.length > most ? `${flat.slice(0, most - 1)}…` : flat;
  };
  const isFile = (element) => element?.tagName === 'INPUT' && element.type === 'file';
  const refOf = (element) => {
    let ref = state.ids.get(element);
    if (!ref) {
      if (state.refs.size > 5000) {
        for (const [known, weak] of state.refs) if (!weak.deref()?.isConnected) state.refs.delete(known);
      }
      ref = `${state.mark}${++state.next}`;
      state.ids.set(element, ref);
      state.refs.set(ref, new WeakRef(element));
    }
    return ref;
  };
  const elementOf = (ref) => {
    if (!String(ref).startsWith(state.mark)) {
      throw new Error(`${ref} is not a part of this page (its parts are named ${state.mark}1, ${state.mark}2, ...): read page_elements of this tab first`);
    }
    const element = state.refs.get(ref)?.deref();
    if (!element || !element.isConnected) throw new Error(`${ref} is not on this page any more: read page_elements again`);
    return element;
  };
  const shown = (element) => {
    if (element.closest('[aria-hidden="true"], [hidden]')) return false;
    // checkVisibility (Chrome 105 and later) answers without a style computation of
    // its own; an element with no box of its own (display: contents) is looked at.
    let style = null;
    if (typeof element.checkVisibility === 'function') {
      if (!element.checkVisibility({ visibilityProperty: true })) {
        style = getComputedStyle(element);
        if (style.display !== 'contents' || style.visibility === 'hidden' || style.visibility === 'collapse') return false;
      }
    } else {
      style = getComputedStyle(element);
      if (style.display === 'none' || style.visibility === 'hidden' || style.visibility === 'collapse') return false;
    }
    const box = element.getBoundingClientRect();
    return box.width > 0 || box.height > 0 || element.childElementCount > 0 || (style ?? getComputedStyle(element)).display === 'contents';
  };
  const INPUTS = { checkbox: 'checkbox', radio: 'radio', button: 'button', submit: 'button', reset: 'button', image: 'button', file: 'file', range: 'slider', search: 'searchbox', number: 'spinbutton', hidden: null };
  const TAGS = {
    BUTTON: 'button', SELECT: 'combobox', TEXTAREA: 'textbox', IMG: 'img', H1: 'heading', H2: 'heading', H3: 'heading',
    H4: 'heading', H5: 'heading', H6: 'heading', NAV: 'navigation', MAIN: 'main', HEADER: 'banner', FOOTER: 'contentinfo',
    ASIDE: 'complementary', FORM: 'form', DIALOG: 'dialog', UL: 'list', OL: 'list', LI: 'listitem', TABLE: 'table',
    TR: 'row', TH: 'columnheader', TD: 'cell', SUMMARY: 'button', DETAILS: 'group', OPTION: 'option', FIELDSET: 'group',
  };
  const roleOf = (element) => {
    const given = element.getAttribute('role');
    if (given) return given.trim().split(/\s+/)[0];
    if (element.tagName === 'A') return element.hasAttribute('href') ? 'link' : null;
    if (element.tagName === 'INPUT') return element.type in INPUTS ? INPUTS[element.type] : 'textbox';
    if (element.tagName.toLowerCase() === 'svg') return element.getAttribute('aria-label') ? 'img' : 'skip';
    if (TAGS[element.tagName]) return TAGS[element.tagName];
    if (element.isContentEditable && !element.parentElement?.isContentEditable) return 'textbox';
    if (element.hasAttribute('onclick') || (element.hasAttribute('tabindex') && element.tabIndex >= 0)) return 'clickable';
    return null;
  };
  const ACTIONABLE = new Set(['link', 'button', 'file', 'textbox', 'searchbox', 'checkbox', 'radio', 'combobox', 'slider', 'option', 'tab', 'menuitem', 'menuitemcheckbox', 'menuitemradio', 'switch', 'listbox', 'spinbutton', 'treeitem', 'clickable']);
  // Parts with nothing to act on inside them (the others are read into when they hold any).
  const LEAVES = new Set(['button', 'file', 'textbox', 'searchbox', 'checkbox', 'radio', 'option', 'slider', 'spinbutton', 'switch', 'menuitemcheckbox', 'menuitemradio', 'img']);
  const NAMED_BY_TEXT = new Set(['link', 'button', 'heading', 'option', 'tab', 'menuitem', 'menuitemcheckbox', 'menuitemradio', 'cell', 'columnheader', 'switch', 'treeitem', 'clickable', 'checkbox', 'radio']);
  const FIELDS = new Set(['textbox', 'searchbox', 'spinbutton', 'combobox']);
  const nameOf = (element, role) => {
    const label = element.getAttribute('aria-label');
    if (label?.trim()) return clean(label);
    const by = element.getAttribute('aria-labelledby');
    if (by) {
      const text = clean(by.split(/\s+/).map((id) => document.getElementById(id)?.innerText ?? '').join(' '));
      if (text) return text;
    }
    if (element.labels?.length) {
      const text = clean([...element.labels].map((label) => label.innerText).join(' '));
      if (text) return text;
    }
    if (element.alt) return clean(element.alt);
    if (NAMED_BY_TEXT.has(role)) {
      const text = clean(element.innerText ?? element.textContent);
      if (text) return text;
    }
    if (element.tagName === 'INPUT' && ['submit', 'button', 'reset'].includes(element.type)) return clean(element.value);
    return clean(element.getAttribute('title') || element.getAttribute('placeholder') || '');
  };
  const hrefOf = (element) => {
    const raw = element.getAttribute('href') ?? element.getAttribute('xlink:href');
    if (raw == null) return null;
    try {
      const url = new URL(raw, location.href);
      return url.origin === location.origin ? url.pathname + url.search + url.hash : url.href;
    } catch {
      return null;
    }
  };
  const factsOf = (element, role) => {
    const facts = [];
    if (role === 'heading') {
      const level = element.getAttribute('aria-level') ?? /^H([1-6])$/.exec(element.tagName)?.[1];
      if (level) facts.push(`level=${level}`);
    }
    if (element.disabled || element.getAttribute('aria-disabled') === 'true') facts.push('disabled');
    if (element.tagName === 'INPUT' && (element.type === 'checkbox' || element.type === 'radio')) facts.push(element.checked ? 'checked' : 'unchecked');
    else if (element.hasAttribute('aria-checked')) facts.push(`checked=${element.getAttribute('aria-checked')}`);
    if (element.hasAttribute('aria-expanded')) facts.push(`expanded=${element.getAttribute('aria-expanded')}`);
    if (element.getAttribute('aria-selected') === 'true') facts.push('selected');
    if (element.tagName === 'SELECT') facts.push(`value="${clean(element.selectedOptions[0]?.text, 60)}"`);
    else if (FIELDS.has(role) || role === 'slider') {
      if ('value' in element) facts.push(`value="${element.type === 'password' ? '•'.repeat(Math.min(element.value.length, 8)) : clean(element.value, 80)}"`);
    }
    if (isFile(element)) {
      if (element.accept) facts.push(`accepts="${clean(element.accept, 80)}"`);
      if (element.multiple) facts.push('multiple');
      facts.push(element.files?.length ? `holds="${clean([...element.files].map((file) => file.name).join(', '), 120)}"` : 'empty');
      if (!shown(element)) facts.push('hidden');
    }
    if (element.required) facts.push('required');
    if (role === 'link') {
      const href = hrefOf(element);
      if (href) facts.push(`→ ${href.slice(0, 120)}`);
    }
    return facts;
  };
  const describe = (element) => {
    const role = roleOf(element) ?? element.tagName.toLowerCase();
    const name = nameOf(element, role);
    return `${role}${name ? ` "${clean(name, 60)}"` : ''}`;
  };
  const STRUCTURE = 'a[href], button, input, select, textarea, [role], [tabindex], [contenteditable], [onclick], h1, h2, h3, h4, h5, h6, img[alt], iframe, frame, ul, ol, li, table, tr, td, th, details, summary, dialog, form, nav, main, header, footer, aside, fieldset, label, svg[aria-label]';
  // Holds nothing to act on and no structure (shadow roots included): one line of text.
  const plain = (element) => {
    if (element.shadowRoot || element.querySelector(STRUCTURE)) return false;
    for (const inner of element.querySelectorAll('*')) if (inner.shadowRoot) return false;
    return true;
  };
  const editable = (element) =>
    element.isContentEditable || element.tagName === 'TEXTAREA' ||
    (element.tagName === 'INPUT' && !['checkbox', 'radio', 'button', 'submit', 'reset', 'image', 'file', 'range', 'color', 'hidden'].includes(element.type));
  // The file field a part stands for: itself, the field its label names, or the one file
  // field in it or a few levels around it (a styled button beside its hidden field),
  // never a guess between two.
  const fileFieldOf = (element) => {
    if (isFile(element)) return element;
    const label = element.closest?.('label');
    if (label && isFile(label.control)) return label.control;
    for (let at = element, up = 0; at && up < 4; at = at.parentElement, up += 1) {
      const found = at.querySelectorAll('input[type="file"]');
      if (found.length) return found.length === 1 ? found[0] : null;
    }
    return null;
  };
  // A part's words, as the user is shown them when asked (a field's are not what it
  // holds, which changes as it is typed in), and where it leads: a link's address, or the
  // address a submit button sends its form to, as the page wrote them (not resolved
  // against the page's own address, which a page may change as it goes).
  const wordsOf = (element) => (editable(element) ? '' : clean(element.innerText || (element.type === 'password' ? '' : element.value) || element.getAttribute('aria-label') || element.title || '', 300));
  const destOf = (element) => {
    const link = element.closest?.('a[href], area[href]');
    if (link) return link.getAttribute('href');
    if (!element.form || (element.type !== 'submit' && element.type !== 'image')) return null;
    return element.getAttribute('formaction') ?? element.form.getAttribute('action') ?? '';
  };

  try {
    if (op === 'parts') {
      const lines = [];
      let size = 0;
      let cut = false;
      let deeper = false;
      const put = (indent, text) => {
        if (cut) return;
        const line = `${'  '.repeat(indent)}- ${text}`;
        size += line.length + 1;
        if (size > args.max) cut = true;
        else lines.push(line);
      };
      // `find`: only the parts whose line matches (`/pattern/flags` as a regular
      // expression, else words, case insensitive), each under the parts above it (the
      // last line at each level above, once), so a match keeps its place in the page.
      let matcher = null;
      if (args.find) {
        const pattern = /^\/(.+)\/([a-z]*)$/s.exec(args.find);
        if (pattern) {
          try {
            const regexp = new RegExp(pattern[1], pattern[2].includes('i') ? pattern[2] : `${pattern[2]}i`);
            matcher = (text) => regexp.test(text);
          } catch {
            matcher = null;
          }
        }
        if (!matcher) {
          const needle = String(args.find).toLowerCase();
          matcher = (text) => text.toLowerCase().includes(needle);
        }
      }
      const trail = [];
      let matches = 0;
      const emit = (indent, text) => {
        if (cut) return;
        if (!matcher) return put(indent, text);
        trail.length = indent + 1;
        trail[indent] = { text, shown: false };
        if (!matcher(text)) return;
        matches += 1;
        for (let level = 0; level < indent; level += 1) {
          const above = trail[level];
          if (above && !above.shown) {
            above.shown = true;
            put(level, above.text);
          }
        }
        trail[indent].shown = true;
        put(indent, text);
      };
      // Depth counts the outline's levels (wrappers without a part of their own do not);
      // a guard keeps pathological nesting from overflowing.
      const walk = (node, indent, nesting) => {
        if (cut) return;
        if (indent > args.depth || nesting > 400) {
          deeper = true;
          return;
        }
        const children = [...(node.shadowRoot?.childNodes ?? []), ...node.childNodes];
        for (const child of children) {
          if (cut) return;
          if (child.nodeType === Node.TEXT_NODE) {
            const text = clean(child.textContent, 300);
            if (text.length > 1 || /\w/.test(text)) emit(indent, `text "${text}"`);
            continue;
          }
          if (child.nodeType !== Node.ELEMENT_NODE || ['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'HEAD', 'LINK', 'META'].includes(child.tagName)) continue;
          // A file field hidden behind a styled button still shows: it is what takes files.
          if (!args.hidden && !shown(child) && !isFile(child)) continue;
          if (child.tagName === 'IFRAME' || child.tagName === 'FRAME') {
            emit(indent, `frame "${clean(child.title || child.src, 80)}" (its inside is not read)`);
            continue;
          }
          const role = roleOf(child);
          if (role === 'skip') continue;
          if (role && !['presentation', 'none', 'generic'].includes(role)) {
            const name = nameOf(child, role);
            const facts = factsOf(child, role);
            const ref = ACTIONABLE.has(role) || role === 'heading' ? ` [${refOf(child)}]` : '';
            emit(indent, `${role}${name ? ` "${name}"` : ''}${facts.length ? ` ${facts.join(' ')}` : ''}${ref}`);
            if (child.tagName === 'SELECT') {
              for (const option of [...child.options].slice(0, 30)) emit(indent + 1, `option "${clean(option.text, 60)}"${option.selected ? ' selected' : ''}`);
              if (child.options.length > 30) emit(indent + 1, `(${child.options.length - 30} more options)`);
            } else if (!LEAVES.has(role) && !(NAMED_BY_TEXT.has(role) && plain(child))) {
              walk(child, indent + 1, nesting + 1);
            }
          } else if (plain(child)) {
            const text = clean(child.innerText ?? child.textContent, 300);
            if (text.length > 1 || /\w/.test(text)) emit(indent, `text "${text}"`);
          } else {
            walk(child, indent, nesting + 1);
          }
        }
      };
      const root = args.ref ? elementOf(args.ref) : document.body;
      if (args.ref) emit(0, `${describe(root)} [${args.ref}]`);
      if (root) walk(root, args.ref ? 1 : 0, 0);
      if (deeper) lines.push(`[parts nested deeper than ${args.depth} levels left out: read them with a ref, or a larger depth]`);
      if (cut) lines.push(matcher ? '[more matches: ask with a narrower find, a ref or a larger max_chars]' : '[more of the page: ask with a ref, a lower depth or a larger max_chars]');
      if (matcher) lines.unshift(matches ? `[${matches} part${matches === 1 ? '' : 's'} match ${JSON.stringify(args.find)}, each under the parts above it]` : `[no part matches ${JSON.stringify(args.find)}]`);
      return { value: { url: location.href, title: document.title, text: lines.join('\n') || '(nothing shown)' } };
    }
    // A part the user was asked about is acted on only while it is what they were shown:
    // its role and name, its words (numbers included: "Pay $10" becoming "Pay $10000" is
    // another action, and a count that changed only asks again) and where it leads.
    const gist = (text) => String(text ?? '').toLowerCase().replace(/\s+/g, ' ').trim();
    const asShown = (element) => {
      if (!args.what) return element;
      const now = describe(element);
      const changed = gist(now) !== gist(args.what) ? `${now}, not the ${args.what}`
        : args.words != null && gist(wordsOf(element)) !== gist(args.words) ? `${now} saying ${JSON.stringify(wordsOf(element))}, not ${JSON.stringify(args.words)} as`
        : args.dest !== undefined && destOf(element) !== args.dest ? `${now} leading to ${destOf(element) ?? 'nowhere'}, not ${args.dest ?? 'nowhere'} as`
        : null;
      if (changed) throw new Error(`[${args.ref}] is now ${changed} the user allowed this on, so it was not done; it can be asked about again`);
      return element;
    };
    if (op === 'point') {
      const element = asShown(elementOf(args.ref));
      // A file field (or its label) opens a file dialog: said before it is measured (a
      // hidden one takes no room), nothing scrolled.
      if (isFile(element) || (element.tagName === 'LABEL' && isFile(element.control))) {
        return { value: { what: describe(element), covered: null, field: false, file: true } };
      }
      element.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
      // The first of its boxes that is on screen (a link wrapping two lines has two).
      const boxes = [...element.getClientRects()].filter((box) => box.width && box.height);
      const box = boxes.find((one) => one.bottom > 0 && one.right > 0 && one.top < innerHeight && one.left < innerWidth) ?? boxes[0];
      if (!box) throw new Error(`${args.ref} takes no room on the page (hidden?)`);
      const x = box.left + box.width / 2;
      const y = box.top + box.height / 2;
      // What is there, in the element's own tree (inside a shadow root, its root's).
      const tree = element.getRootNode();
      const top = (typeof tree.elementFromPoint === 'function' ? tree : document).elementFromPoint(x, y);
      const own = top && (top === element || element.contains(top) || (top instanceof HTMLLabelElement && top.control === element));
      const covered = top && !own ? describe(top) : null;
      return { value: { x, y, what: describe(element), covered, field: editable(element), file: false } };
    }
    if (op === 'caret') {
      const element = elementOf(args.ref);
      if (!editable(element)) throw new Error(`${describe(element)} [${args.ref}] is not a field one types into`);
      element.focus();
      if (typeof element.setSelectionRange === 'function' && 'value' in element) {
        try {
          element.setSelectionRange(args.all ? 0 : element.value.length, element.value.length);
        } catch {
          // Some fields (email, number) take no selection: select() covers clearing.
          if (args.all) element.select();
        }
      } else if (element.isContentEditable) {
        const range = document.createRange();
        range.selectNodeContents(element);
        if (!args.all) range.collapse(false);
        getSelection().removeAllRanges();
        getSelection().addRange(range);
      }
      return { value: true };
    }
    if (op === 'target') {
      const element = elementOf(args.ref);
      const kind = (element.getAttribute('type') ?? '').toLowerCase();
      const submits = (element.localName === 'button' && (kind === 'submit' || (!kind && Boolean(element.form))))
        || (element.localName === 'input' && (kind === 'submit' || kind === 'image'));
      // A field's words are its label, never what it holds when that is a password.
      const value = element.type === 'password' ? '' : element.value;
      const label = clean(element.innerText || value || element.getAttribute('aria-label') || element.title || '', 120);
      return { value: { what: describe(element), submits, label, words: wordsOf(element), dest: destOf(element) } };
    }
    if (op === 'focused') {
      let element = document.activeElement;
      while (element?.shadowRoot?.activeElement) element = element.shadowRoot.activeElement;
      if (!element || element === document.body || element === document.documentElement) return { value: null };
      return { value: { ref: refOf(element), what: describe(element) } };
    }
    // A file's bytes come in parts (`first`: a new upload, so what an earlier one left goes).
    if (op === 'file_part') {
      if (args.first || !state.uploads) state.uploads = new Map();
      if (args.drop) return { value: true };
      if (!state.uploads.has(args.key)) state.uploads.set(args.key, []);
      (state.uploads.get(args.key)[args.index] ??= []).push(String(args.data));
      return { value: true };
    }
    if (op === 'files') {
      const parts = state.uploads?.get(args.key) ?? [];
      state.uploads?.delete(args.key);
      const element = asShown(elementOf(args.ref));
      const field = fileFieldOf(element);
      if (!field) throw new Error(`${describe(element)} [${args.ref}] has no one file field in or beside it: read page_elements (file fields show there; one inside a hidden part shows with hidden: true) and give the field's ref`);
      if (field.disabled) throw new Error(`${describe(field)} is disabled`);
      if (args.files.length > 1 && !field.multiple) throw new Error(`${describe(field)} takes one file`);
      const rules = field.accept.split(',').map((rule) => rule.trim().toLowerCase()).filter(Boolean);
      // A type Spindle does not know (none) is not held against a rule naming one.
      const takes = (file) => !rules.length || rules.some((rule) => (rule.startsWith('.') ? file.name.toLowerCase().endsWith(rule)
        : !file.type || (rule.endsWith('/*') ? file.type.startsWith(rule.slice(0, -1)) : rule === file.type)));
      const refused = args.files.find((file) => !takes(file));
      if (refused) throw new Error(`${describe(field)} takes ${field.accept}, not ${refused.name}`);
      const decode = (text) => {
        const binary = atob(text);
        const bytes = new Uint8Array(binary.length);
        for (let at = 0; at < binary.length; at += 1) bytes[at] = binary.charCodeAt(at);
        return bytes;
      };
      const chosen = new DataTransfer();
      args.files.forEach((file, index) => {
        const made = new File((parts[index] ?? []).map(decode), file.name, { type: file.type });
        if (made.size !== file.size) throw new Error(`${file.name} did not reach the page whole`);
        chosen.items.add(made);
      });
      field.files = chosen.files;
      // The page hears it as a pick once this script has returned (a handler that opens
      // a dialog then holds the page, not this script).
      setTimeout(() => {
        field.dispatchEvent(new Event('input', { bubbles: true }));
        field.dispatchEvent(new Event('change', { bubbles: true }));
      }, 0);
      return { value: { what: describe(field), field: refOf(field), put: [...chosen.files].map((file) => `${file.name} (${file.size} bytes)`) } };
    }
    if (op === 'value') {
      const element = elementOf(args.ref);
      if (element.type === 'password') return { value: '(a password)' };
      return { value: 'value' in element ? String(element.value).slice(0, 200) : clean(element.innerText, 200) };
    }
    if (op === 'select') {
      const element = asShown(elementOf(args.ref));
      if (element.tagName !== 'SELECT') throw new Error(`${args.ref} is not a <select> list: click it, then the option you want`);
      const matches = [...element.options].filter((option) => args.values.includes(option.value) || args.values.includes(option.text.trim()));
      if (!matches.length) {
        throw new Error(`no option of ${args.ref} is ${args.values.join(' or ')}; it has ${[...element.options].slice(0, 20).map((option) => JSON.stringify(option.text.trim())).join(', ')}`);
      }
      const chosen = element.multiple ? matches : matches.slice(0, 1);
      for (const option of element.options) option.selected = chosen.includes(option);
      element.dispatchEvent(new Event('input', { bubbles: true }));
      element.dispatchEvent(new Event('change', { bubbles: true }));
      return { value: chosen.map((option) => option.text.trim()) };
    }
    if (op === 'scroll') {
      if (args.ref) {
        elementOf(args.ref).scrollIntoView({ block: 'center', behavior: 'instant' });
        const page = document.scrollingElement ?? document.documentElement;
        return { value: { y: page.scrollTop, height: page.scrollHeight, view: innerHeight } };
      }
      // What scrolls where the user looks: the pane under the middle of the window (many
      // apps scroll one), or the page.
      const scrolls = (element) => {
        const style = getComputedStyle(element);
        return element.scrollHeight > element.clientHeight + 1 && /(auto|scroll|overlay)/.test(style.overflowY);
      };
      let pane = document.elementFromPoint(innerWidth / 2, innerHeight / 2);
      while (pane && pane !== document.body && pane !== document.documentElement && !scrolls(pane)) pane = pane.parentElement;
      const page = !pane || pane === document.body || pane === document.documentElement;
      const target = page ? document.scrollingElement ?? document.documentElement : pane;
      const view = page ? innerHeight : target.clientHeight;
      if (args.to === 'top') target.scrollTo({ top: 0, behavior: 'instant' });
      else if (args.to === 'bottom') target.scrollTo({ top: target.scrollHeight, behavior: 'instant' });
      else target.scrollBy({ top: (args.to === 'up' ? -0.85 : 0.85) * view, behavior: 'instant' });
      return { value: { y: target.scrollTop, height: target.scrollHeight, view, pane: page ? null : describe(target) } };
    }
    if (op === 'has') {
      if (args.selector) {
        // Shown, not only there (a hidden spinner is gone).
        const element = document.querySelector(args.selector);
        return { value: Boolean(element && element.getClientRects().length && getComputedStyle(element).visibility !== 'hidden') };
      }
      return { value: (document.body?.innerText ?? '').includes(args.text) };
    }
    if (op === 'html') {
      const element = args.ref ? elementOf(args.ref) : document.documentElement;
      // Into a document of its own: nothing in the copy runs or loads.
      const copy = document.implementation.createHTMLDocument('').importNode(element, true);
      if (!args.scripts) for (const script of copy.querySelectorAll('script')) script.remove();
      // A password stays out, as in the outline (some pages mirror it into the attribute).
      for (const field of [copy, ...copy.querySelectorAll('input')]) if (field.type === 'password') field.removeAttribute('value');
      const html = copy.outerHTML;
      return { value: { text: html.slice(0, args.max), total: html.length } };
    }
    throw new Error(`no page operation ${op}`);
  } catch (error) {
    return { error: String(error?.message ?? error) };
  }
}
