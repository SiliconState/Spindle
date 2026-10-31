// Spindle's service worker: the toolbar button opens the panel, it serves the frame
// previews show pages in, and it does the page tools' work for the engine (the engine's
// document cannot reach tabs). Reading needs page reading on; acting needs page actions
// on as well (chrome.debugger: clicks, keys and typing reach the page as the user's
// would, and a tab's network can be watched).
import { ACTING_NAMES, PAGE_ACCESS, PICTURE_ACCESS, pageSwitches, uncertain } from './page-tools.mjs';
import { read, write } from './shared.mjs';

chrome.sidePanel.setPanelBehavior({ openPanelOnActionClick: true }).catch(() => {});

// The switches are kept from the first start: off on a new install. Builds before the
// switches were Spindle's took page reading to be Chrome's access to sites, so a user
// updating from one who had it keeps it (once: the record exists from then on).
chrome.runtime.onInstalled.addListener(async ({ reason }) => {
  if (await read('kv', 'pages').catch(() => null)) return;
  const access = reason === 'update' && (await chrome.permissions.contains({ origins: PAGE_ACCESS }));
  await write('kv', 'pages', { reading: access, acting: false }).catch(() => {});
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
const REACH = { none: '()', web: '("https://*:*")' };

// Only where Chrome has static routes (123 and later): there only previews come to the
// handler, and every other request of Spindle's pages and workers goes on as if there
// were none (the worker is not woken for each). Without them it serves nothing (no
// Chrome that old holds a Connection-Allowlist anyway): previews show uncontained.
if ('addRoutes' in (self.InstallEvent?.prototype ?? {})) {
  self.addEventListener('install', (event) => {
    event.waitUntil(event.addRoutes([
      { condition: { urlPattern: new URLPattern({ pathname: PREVIEW }) }, source: 'fetch-event' },
      { condition: { urlPattern: new URLPattern({}) }, source: 'network' },
    ]).catch((error) => console.warn('Spindle: previews go through the fetch handler with everything else:', error)));
  });
  self.addEventListener('fetch', (event) => {
    const url = new URL(event.request.url);
    if (url.origin !== location.origin || url.pathname !== PREVIEW || event.request.mode !== 'navigate') return;
    event.respondWith(preview(url));
  });
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
  // Only the engine's document asks for tools (no page's content, no other extension).
  if (sender.tab || sender.url !== chrome.runtime.getURL('engine.html')) return false;
  if (message?.type === 'page_tool_cancel') {
    const control = calls.get(message.call);
    if (control) control.cancelled = true;
    return false;
  }
  if (message?.type !== 'page_tool') return false;
  run(message.name, message.input ?? {}, message.call).then(reply, (error) => reply({ error: String(error?.message ?? error) }));
  return true;
});

const OFF =
  'Spindle may not read pages: page reading is off. The user can turn it on in the Spindle panel (Settings, Pages).';
const NO_ACTIONS =
  'Spindle may not act on pages: page actions are off. The user can turn them on in the Spindle panel (Settings, Pages).';

async function run(name, input, call = crypto.randomUUID()) {
  const work = TOOLS[name];
  if (!work) return { error: `there is no page tool ${name}` };
  // `acted`: input reached the page, or Chrome was told to move or close a tab.
  const control = { until: Date.now() + TOOL_MS, cancelled: false, acted: false };
  calls.set(call, control);
  try {
    return await deadline((async () => {
      if (!(await reading())) return { error: OFF };
      const viewing = name === 'navigate' && typeof input?.url === 'string' && input.url.startsWith(chrome.runtime.getURL('view.html?'));
      if (ACTING_NAMES.has(name) && !viewing && name !== 'screenshot' && !(await acting())) return { error: NO_ACTIONS };
      check(control);
      return work(input, control);
    })(), TOOL_MS, 'the page tool');
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
 * own page open in a tab is not it; then another window's active tab is. */
async function lookedAt() {
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
async function pageTab(id) {
  const tab = id != null ? await chrome.tabs.get(id).catch(() => null) : await lookedAt();
  if (!tab) throw new Error(id != null ? `there is no tab ${id}` : 'no tab is open');
  if (!tab.url) throw new Error(`Chrome gives Spindle no access to tab ${tab.id} (a new tab, a blank page or a browser page)`);
  if (!/^https?:/.test(tab.url)) throw new Error(`Chrome does not let extensions into ${new URL(tab.url).protocol} pages`);
  return tab;
}

/** A tab the acting tools may move or close: one showing a web page or a blank page
 * (never a browser page, another extension's, or Spindle's own). */
const webTab = (tab) => /^https?:/.test(tab?.url ?? '') || tab?.url === 'about:blank';

/** Runs one of `inPage`'s operations in the tab (Spindle's own isolated world there).
 * A tab showing a dialog answers nothing until the dialog is answered: said at once. */
async function page(tabId, op, args = {}, control) {
  if (!(await reading())) throw new Error(OFF);
  if (['point', 'caret', 'select', 'scroll'].includes(op) && !(await acting())) throw new Error(NO_ACTIONS);
  check(control);
  refuseWhileDialog(tabId);
  // `point` only measures; these change the page, unless it refuses first (then this
  // changed nothing, and the call stands as it did).
  const was = control?.acted;
  if (control && ['caret', 'select', 'scroll'].includes(op)) control.acted = true;
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
async function settle(tabId, ms = 10_000, control) {
  await pause(400);
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

async function listTabs() {
  const tabs = await chrome.tabs.query({ windowType: 'normal' });
  const seen = await lookedAt();
  const lines = tabs.map((tab) => {
    const mark = tab.id === seen?.id ? ' (the user is looking at it)' : '';
    return `tab ${tab.id}${mark}: ${tab.url ? `${tab.title || '(untitled)'} <${tab.url}>` : '(no access)'}`;
  });
  return { content: lines.join('\n'), details: { tabs: tabs.length } };
}

async function readPage({ tab: id, max_chars: max, scroll: screens }, control) {
  const tab = await pageTab(id);
  refuseWhileDialog(tab.id);
  const limit = Math.min(Math.max(Number(max) || 20000, 1000), 100000);
  if (screens) return readScrolling(tab, limit, screens, control);
  // Cut in the page: a huge page's text never crosses over whole.
  const [{ result }] = await deadline(chrome.scripting.executeScript({
    target: { tabId: tab.id },
    args: [limit],
    func: (limit) => {
      const text = document.body?.innerText ?? '';
      const selection = String(getSelection() ?? '');
      return { url: location.href, title: document.title, selection: selection.slice(0, 10000), text: text.slice(0, limit), total: text.length };
    },
  }), DEADLINE_MS, `tab ${tab.id}`);
  const more = result.total - result.text.length;
  const text = more > 0 ? `${result.text}\n[${more} more characters: ask again with a larger max_chars]` : result.text;
  const lines = [`Address: ${result.url}`, `Title: ${result.title}`];
  if (result.selection.trim()) lines.push(`Selected text: ${result.selection}`);
  return { content: `${lines.join('\n')}\n\n${text}`, details: { tab: tab.id, url: result.url, title: result.title } };
}

/** The text of a page that loads as it scrolls (and drops what scrolled away), read
 * while scrolling down `screens` times: each new line once, in order, until the page
 * stops growing, the screens are done or `limit` characters are in. */
async function readScrolling(tab, limit, screens, control) {
  if (!(await acting())) {
    return { error: 'reading while scrolling moves the page, so it needs page actions on (Settings, Pages); read_page without scroll reads what shows now' };
  }
  const most = Math.min(Math.max(Math.round(Number(screens)) || 1, 1), 30);
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
    for (const line of result.text.split('\n')) {
      const key = line.trim();
      if (!key || seen.has(key) || chars >= limit) continue;
      seen.add(key);
      kept.push(line);
      chars += line.length + 1;
      added += 1;
    }
    // Two screens that bring nothing new: the end of what loads.
    quiet = added ? 0 : quiet + 1;
    if (scrolled >= most || quiet >= 2 || chars >= limit) break;
    if (Date.now() > until) {
      late = true;
      break;
    }
    await page(tab.id, 'scroll', { to: 'down' }, control);
    scrolled += 1;
    // What a feed loads as it scrolls takes a moment to come.
    await pause(900);
  }
  const why = chars >= limit ? `stopped at ${limit} characters (ask with a larger max_chars for more)`
    : quiet >= 2 ? 'the page stopped bringing new text'
    : late ? `scrolled ${scrolled} screen${scrolled === 1 ? '' : 's'} in the time a call has (call again for more)`
    : `scrolled ${scrolled} screen${scrolled === 1 ? '' : 's'}`;
  const text = kept.join('\n').slice(0, limit);
  return {
    content: `Address: ${last.url}\nTitle: ${last.title}\nRead while scrolling down (${why}; each line once):\n\n${text}`,
    details: { tab: tab.id, url: last.url, title: last.title, scrolled },
  };
}

async function pageElements({ tab: id, ref, depth, hidden, max_chars: max }, control) {
  const tab = await pageTab(id);
  const parts = await page(tab.id, 'parts', {
    ref, hidden: Boolean(hidden),
    depth: Math.min(Math.max(Number(depth) || 30, 1), 80),
    max: Math.min(Math.max(Number(max) || 30000, 2000), 100000),
  }, control);
  return { content: `Address: ${parts.url}\nTitle: ${parts.title} (tab ${tab.id})\n\n${parts.text}`, details: { tab: tab.id, url: parts.url } };
}

async function pageHtml({ tab: id, ref, scripts, max_chars: max }, control) {
  const tab = await pageTab(id);
  const html = await page(tab.id, 'html', { ref, scripts: Boolean(scripts), max: Math.min(Math.max(Number(max) || 50000, 1000), 200000) }, control);
  const more = html.total - html.text.length;
  return { content: `${html.text}${more > 0 ? `\n[${more} more characters: ask again with a larger max_chars, or a ref]` : ''}`, details: { tab: tab.id } };
}

/** A picture of a tab: the one showing in its window as Chrome draws it, once the user
 * let Spindle picture pages (no debugger); any tab through Chrome's debugger (page
 * actions on). */
async function screenshot({ tab: id }, control) {
  const tab = await pageTab(id);
  refuseWhileDialog(tab.id);
  let base64;
  if (tab.active && (await chrome.permissions.contains({ origins: PICTURE_ACCESS }))) {
    check(control);
    const url = await chrome.tabs.captureVisibleTab(tab.windowId, { format: 'jpeg', quality: 75 });
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

async function navigate({ url, action = 'go', tab: id, new_tab: fresh }, control) {
  check(control);
  // A file of the thread's workspace (the engine names its page): opened in a tab of its
  // own for the user, never in place of a page.
  if (typeof url === 'string' && url.startsWith(chrome.runtime.getURL('view.html?'))) {
    const path = new URL(url).searchParams.get('path') ?? '';
    control.acted = true;
    const tab = await chrome.tabs.create({ url, active: true });
    return { content: `Opened ${path} from the workspace in a tab for the user (tab ${tab.id}).`, details: { tab: tab.id } };
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
  if (fresh) {
    if (!url) return { error: 'a new tab needs a url' };
    control.acted = true;
    tab = await chrome.tabs.create({ url, active: true });
  } else {
    tab = id != null ? await chrome.tabs.get(id).catch(() => null) : await lookedAt();
    if (!tab) return { error: id != null ? `there is no tab ${id}` : 'no tab is open' };
    if (!webTab(tab)) return { error: `tab ${tab.id} does not show a web page Spindle may move (a new tab, a browser or extension page): open the address with new_tab` };
    return onTab(tab.id, async () => {
      refuseWhileDialog(tab.id);
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
  return { content: await whereNow(tab.id), details: { tab: tab.id } };
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
  return page(tabId, 'point', { ref }, control);
}

/** Something else lies over the target: nothing is clicked or typed there. */
const coveredError = (at, ref) => ({
  error: `${at.what} [${ref}] is covered by ${at.covered}, so nothing was done (a click would land on that); close or move what covers it, or act on that instead`,
});

async function click({ ref, tab: id, double, button = 'left' }, control) {
  const tab = await pageTab(id);
  return onTab(tab.id, async () => {
    refuseWhileDialog(tab.id);
    const at = await aim(tab.id, ref, control);
    if (at.covered) return coveredError(at, ref);
    await mouse(tab.id, at, button, double ? 2 : 1, control);
    await settle(tab.id, 10_000, control);
    return { content: `Clicked ${at.what} [${ref}]. ${await whereNow(tab.id)}`, details: { tab: tab.id } };
  });
}

async function type(input, control) {
  const tab = await pageTab(input.tab);
  return onTab(tab.id, () => typeInto(tab, input, control));
}

async function typeInto(tab, { ref, text, clear, submit }, control) {
  refuseWhileDialog(tab.id);
  const at = await aim(tab.id, ref, control);
  if (!at.field) return { error: `${at.what} [${ref}] is not a field one types into` };
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

async function select({ ref, values, tab: id }, control) {
  const tab = await pageTab(id);
  return onTab(tab.id, () => selectIn(tab, ref, values, control));
}

async function selectIn(tab, ref, values, control) {
  const chosen = await page(tab.id, 'select', { ref, values: values.map(String) }, control);
  await settle(tab.id, 5_000, control);
  return { content: `Chose ${chosen.map((text) => JSON.stringify(text)).join(', ')} in [${ref}]. ${await whereNow(tab.id)}`, details: { tab: tab.id } };
}

async function press({ key: name, tab: id }, control) {
  const tab = await pageTab(id);
  return onTab(tab.id, async () => {
    refuseWhileDialog(tab.id);
    await key(tab.id, name, control);
    await settle(tab.id, 5_000, control);
    return { content: `Pressed ${name}. ${await whereNow(tab.id)}`, details: { tab: tab.id } };
  });
}

async function scroll({ ref, to = 'down', tab: id }, control) {
  const tab = await pageTab(id);
  const at = await onTab(tab.id, () => page(tab.id, 'scroll', { ref, to }, control));
  const where = `${Math.round(at.y)} of ${Math.round(at.height)} pixels down, ${Math.round(at.view)} shown`;
  return { content: `${ref ? `Scrolled [${ref}] into view` : `Scrolled ${at.pane ? `${at.pane} ` : ''}${to}`}: ${where}.`, details: { tab: tab.id } };
}

async function waitFor({ text, selector, gone, seconds, tab: id }, control) {
  if (!text && !selector) return { error: 'give text or a selector to wait for' };
  const tab = await pageTab(id);
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
  const tab = id != null ? await chrome.tabs.get(id).catch(() => null) : await lookedAt();
  if (!tab) return { error: id != null ? `there is no tab ${id}` : 'no tab is open' };
  const open = dialogs.get(tab.id);
  if (!open) return { error: `no dialog is open on tab ${tab.id}` };
  await cdp(tab.id, 'Page.handleJavaScriptDialog', { accept: Boolean(accept), ...(text != null && { promptText: String(text) }) }, control);
  dialogs.delete(tab.id);
  await settle(tab.id, 5_000, control);
  return { content: `${accept ? 'Accepted' : 'Dismissed'} the ${open.type} ${JSON.stringify(open.message)}. ${await whereNow(tab.id)}`, details: { tab: tab.id } };
}

async function closeTab({ tab: id }, control) {
  const tab = await chrome.tabs.get(id).catch(() => null);
  if (!tab) return { error: `there is no tab ${id}` };
  if (!webTab(tab)) return { error: `tab ${id} does not show a web page: Spindle closes only those` };
  check(control);
  control.acted = true;
  await chrome.tabs.remove(id);
  return { content: `Closed tab ${id} (${tab.title || tab.url || 'untitled'}).` };
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
/** Tabs showing a JavaScript dialog -> {type, message}; callers waiting for one. */
const dialogs = new Map();
const dialogWaiters = new Map();
/** Tabs whose network is watched -> their requests, oldest first, by entry id. */
const watched = new Map();
const MAX_REQUESTS = 500;

function refuseWhileDialog(tabId) {
  const open = dialogs.get(tabId);
  if (open) throw new Error(`a ${open.type} dialog is open on tab ${tabId}: ${JSON.stringify(open.message)}. Answer it with dialog (accept or dismiss) first`);
}

async function attach(tabId) {
  if (attached.has(tabId)) return;
  if (await vetoed(tabId)) {
    throw new Error(`the user stopped Spindle acting on tab ${tabId} (they closed Chrome's debugging bar); Spindle will not act on it again until the user turns page actions off and on in Spindle's settings`);
  }
  if (!attaching.has(tabId)) {
    attaching.set(tabId, (async () => {
      try {
        await chrome.debugger.attach({ tabId }, '1.3');
      } catch (error) {
        // Spindle's own earlier attach (before this worker last rested) still holds.
        if (!/already attached/i.test(error.message)) throw new Error(`Chrome would not let Spindle act on tab ${tabId}: ${error.message}`);
      }
      attached.set(tabId, setTimeout(() => detach(tabId), 20_000));
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
  clearTimeout(attached.get(tabId));
  attached.set(tabId, watched.has(tabId) ? null : setTimeout(() => detach(tabId), 20_000));
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

function forget(tabId) {
  clearTimeout(attached.get(tabId));
  attached.delete(tabId);
  watched.delete(tabId);
  dialogs.delete(tabId);
}

function detach(tabId) {
  forget(tabId);
  chrome.debugger.detach({ tabId }).catch(() => {});
}

// A closed tab's veto goes with it.
chrome.tabs.onRemoved.addListener((tabId) => { unveto(tabId).catch(() => {}); });
chrome.debugger.onDetach.addListener(({ tabId }, reason) => {
  forget(tabId);
  if (reason === 'canceled_by_user') veto(tabId).catch(() => {});
});
chrome.debugger.onEvent.addListener(({ tabId }, method, params) => {
  if (method === 'Page.javascriptDialogOpening') {
    dialogs.set(tabId, { type: params.type, message: String(params.message ?? '').slice(0, 500) });
    for (const resolve of dialogWaiters.get(tabId) ?? []) resolve();
    dialogWaiters.delete(tabId);
  } else if (method === 'Page.javascriptDialogClosed') {
    dialogs.delete(tabId);
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
    await write('kv', 'pages', { reading: false, acting: false }).catch(() => {});
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
    const tab = id != null ? await chrome.tabs.get(id).catch(() => null) : await lookedAt();
    if (!tab) return { error: id != null ? `there is no tab ${id}` : 'no tab is open' };
    if (watched.has(tab.id)) await cdp(tab.id, 'Network.disable', {}, control).catch(() => {});
    detach(tab.id);
    return { content: `Stopped watching tab ${tab.id}'s network.` };
  }
  const tab = await pageTab(id);
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
  if (!log) return { error: `tab ${tab.id}'s network is not watched (start it; Chrome ends a watch when its debugging bar is closed)` };
  if (body) {
    const entry = log.get(body);
    if (!entry) return { error: `there is no request ${body} in tab ${tab.id}'s log` };
    if (entry.redirected) return { error: 'Chrome does not keep a redirect hop\'s body; read the final request instead' };
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
    const style = getComputedStyle(element);
    if (style.display === 'none' || style.visibility === 'hidden' || style.visibility === 'collapse') return false;
    const box = element.getBoundingClientRect();
    return box.width > 0 || box.height > 0 || style.display === 'contents' || element.childElementCount > 0;
  };
  const INPUTS = { checkbox: 'checkbox', radio: 'radio', button: 'button', submit: 'button', reset: 'button', image: 'button', file: 'button', range: 'slider', search: 'searchbox', number: 'spinbutton', hidden: null };
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
  const ACTIONABLE = new Set(['link', 'button', 'textbox', 'searchbox', 'checkbox', 'radio', 'combobox', 'slider', 'option', 'tab', 'menuitem', 'menuitemcheckbox', 'menuitemradio', 'switch', 'listbox', 'spinbutton', 'treeitem', 'clickable']);
  // Parts with nothing to act on inside them (the others are read into when they hold any).
  const LEAVES = new Set(['button', 'textbox', 'searchbox', 'checkbox', 'radio', 'option', 'slider', 'spinbutton', 'switch', 'menuitemcheckbox', 'menuitemradio', 'img']);
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

  try {
    if (op === 'parts') {
      const lines = [];
      let size = 0;
      let cut = false;
      let deeper = false;
      const emit = (indent, text) => {
        if (cut) return;
        const line = `${'  '.repeat(indent)}- ${text}`;
        size += line.length + 1;
        if (size > args.max) cut = true;
        else lines.push(line);
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
          if (!args.hidden && !shown(child)) continue;
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
      if (cut) lines.push('[more of the page: ask with a ref, a lower depth or a larger max_chars]');
      return { value: { url: location.href, title: document.title, text: lines.join('\n') || '(nothing shown)' } };
    }
    if (op === 'point') {
      const element = elementOf(args.ref);
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
      return { value: { x, y, what: describe(element), covered, field: editable(element) } };
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
    if (op === 'value') {
      const element = elementOf(args.ref);
      if (element.type === 'password') return { value: '(a password)' };
      return { value: 'value' in element ? String(element.value).slice(0, 200) : clean(element.innerText, 200) };
    }
    if (op === 'select') {
      const element = elementOf(args.ref);
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
