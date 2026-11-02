// Spindle for Browser, end to end in real Chrome: the built extension is loaded into a
// fresh profile and driven through its own panel with real clicks and typing, against a
// real provider (or, with --scripted, a scripted one served here). It checks:
//
//   1. with page reading off, read_page refuses (even for a site the extension may
//      reach); with it on, a thread reads the page the user is on;
//   2. a message written while a turn runs steers it, and the turn finishes while no
//      panel is open;
//   3. Stop ends a turn mid-reply; Continue then refuses an unknown outcome and Retry
//      sends it once;
//   4. Chrome killed mid-turn: after a restart every thread is there, the cut one says
//      so, and Retry finishes it;
//   5. (with SPINDLE_BIN) a thread moves to a file without Chrome's page tools and with
//      native's full tool set, a native host continues it with a turn of its own (and
//      a budget) and moves it back; imported, it gets the page tools again, keeps the
//      budget (counted here as there, and the thread says so), and goes on in Chrome;
//   6. a copy exports and the thread stays; deleting a thread (from its menu or the
//      thread list) removes it and its files; no bundle holds the key;
//   7. bash runs in the browser Linux (Python and Node, a file written back), its
//      scripts make Spindle calls (a nested model, a page tool: code mode through bash),
//      reach a site the user allowed in Settings (GET only, as allowed), run TypeScript;
//      a site's secret header is added to a script's request, never shown to it, kept
//      as text or in the thread, and removed with the site; a file attached to a message
//      lands in the workspace and what a thread made downloads, alone or as a .zip; a
//      thread browses (page_elements, type, click, screenshot, page_html, network,
//      navigate); (scripted) Before acting: Ask asks and No does nothing, Yes after the page reloaded presses nothing, Auto asks only
//      before Enter; (scripted, with the wasm Linux built in) a plain sh command runs in
//      the wasm Linux; (scripted) a command stopped while it runs is kept as stopped,
//      with what it printed, and a reply goes on with the turn;
//      and (with SPINDLE_BIN) it runs in a Spindle host's sandbox once switched to it;
//   8. (scripted) a long thread: a turn's end draws only that turn; back to before a
//      message; a history over its window compacted with a summary the model writes,
//      said, and its earlier messages shown; a new thread started with its files.
//
//   SPINDLE_BROWSER=/path/to/chrome SPINDLE_KEY=... [SPINDLE_BIN=/path/to/spindle] \
//     node chrome/tests/extension_real.mjs DIST --provider ID [--model M] [--catalog FILE]
//   SPINDLE_BROWSER=... [SPINDLE_BIN=...] node chrome/tests/extension_real.mjs DIST --scripted
//
// DIST is chrome/build.sh's output. Nothing about a provider is built in: its endpoint
// comes from the catalog (--catalog, or the package's built-ins). Chrome cannot be
// clicked through a permission prompt headless, so the test's copy of DIST grants the
// provider's site and this test's page in its manifest; the extension asks for them at
// run time. The native leg uses SPINDLE_BIN's own credentials for the provider (in
// --scripted mode, a home of its own). Real mode makes real, billed requests. The work
// folder (a Chrome profile holding the key) is always removed; SPINDLE_SHOTS=DIR keeps
// only screenshots, in DIR.
import assert from 'node:assert/strict';
import { execFile, execFileSync, spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { promisify } from 'node:util';
import { StdioClient } from '../../clients/js/spindle.mjs';
import { attach, launch, open } from './cdp.mjs';

const args = process.argv.slice(2);
const distArg = args.shift();
const options = {};
while (args.length) {
  const arg = args.shift();
  if (arg === '--scripted') options.scripted = true;
  else if (arg.startsWith('--') && args.length) options[arg.slice(2)] = args.shift();
  else throw new Error(`unexpected argument ${arg}`);
}
const { SPINDLE_BROWSER: browserBinary, SPINDLE_BIN: native } = process.env;
const key = options.scripted ? `sk-scripted-${randomBytes(12).toString('hex')}` : process.env.SPINDLE_KEY;
if (!distArg || !browserBinary || !key || !(options.scripted || options.provider)) {
  throw new Error('usage: SPINDLE_BROWSER=... SPINDLE_KEY=... node extension_real.mjs DIST (--provider ID [--model M] [--catalog FILE] | --scripted)');
}

const work = fs.mkdtempSync(join(tmpdir(), 'spindle-chrome-'));
let browser = null;
// However the run ends, no Chrome is left behind, and no profile holding the key.
process.on('exit', () => {
  browser?.child.kill('SIGKILL');
  fs.rmSync(work, { recursive: true, force: true });
});
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => process.exit(130));
const profile = join(work, 'profile');
const downloads = join(work, 'downloads');
const shots = process.env.SPINDLE_SHOTS ? resolve(process.env.SPINDLE_SHOTS) : join(work, 'shots');
fs.mkdirSync(downloads);
fs.mkdirSync(shots, { recursive: true });
const secret = `heron-${randomBytes(3).toString('hex')}`;

// ---- This test's server: the page a thread reads and, in --scripted mode, a provider.

const scripted = { replies: [], seen: [], holding: [] };
/** What reached this test's site (/api/): a script's requests through its grant. */
const siteVisits = [];
/** What reached this test's keyed site (/keyed/): a script's requests with the secret
 * header Spindle adds. */
const keyedVisits = [];
/** What the test page's Go button searched for (a thread clicking and typing on it). */
const pageClicks = [];
/** What a page a thread holds said once open in a tab (its script ran, its data came). */
const rendered = [];
const server = http.createServer(async (request, response) => {
  if (request.method === 'GET' && request.url === '/page.html') {
    // A cookie for this site: a script's request here must go without it.
    response.writeHead(200, { 'content-type': 'text/html; charset=utf-8', 'set-cookie': 'spindle-test=visited; Path=/; SameSite=Lax' });
    response.end(`<!doctype html><title>Spindle test page</title><h1>Field notes</h1><p>The secret word is <b>${secret}</b>.</p>
<label>Search <input id="q"></label> <button onclick="fetch('/clicked?q=' + encodeURIComponent(document.querySelector('#q').value))">Go</button>
<button onclick="if (confirm('Really delete?')) fetch('/clicked?q=confirmed')">Delete</button>
<p><a href="/second.html">Second page</a></p><script>fetch('/ping')</script>`);
    return;
  }
  // What the page itself asks for and does (its own requests, as a user's would be).
  if (request.method === 'GET' && request.url === '/ping') {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end('{"pong":true}');
    return;
  }
  if (request.method === 'GET' && request.url.startsWith('/clicked?')) {
    pageClicks.push(new URL(request.url, 'http://test').searchParams.get('q'));
    response.writeHead(204).end();
    return;
  }
  if (request.method === 'GET' && request.url.startsWith('/rendered?')) {
    rendered.push(new URL(request.url, 'http://test').searchParams.get('word'));
    response.writeHead(204, { 'access-control-allow-origin': '*' }).end();
    return;
  }
  if (request.method === 'GET' && request.url === '/second.html') {
    response.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
    response.end('<!doctype html><title>Second page</title><h1>The second page</h1>');
    return;
  }
  if (request.url.startsWith('/keyed/')) {
    const auth = request.headers.authorization ?? '';
    keyedVisits.push({ url: request.url, authorization: request.headers.authorization ?? null, cookie: request.headers.cookie ?? null });
    // Echoes the header back, as some APIs do in their errors: Spindle takes it out.
    response.writeHead(200, { 'content-type': 'application/json', 'x-echo': auth });
    response.end(JSON.stringify({ auth, token: auth.replace(/^Bearer /, ''), b64: Buffer.from(auth.replace(/^Bearer /, '')).toString('base64') }));
    return;
  }
  if (request.url.startsWith('/api/')) {
    siteVisits.push({ method: request.method, url: request.url, cookie: request.headers.cookie ?? null });
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ notes: ['from the site'] }));
    return;
  }
  if (options.scripted && request.method === 'POST' && request.url === '/v1/chat/completions') {
    const parts = [];
    for await (const part of request) parts.push(part);
    scripted.seen.push({ authorization: request.headers.authorization, body: Buffer.concat(parts).toString() });
    let reply = scripted.replies.shift() ?? { content: '(script ended)' };
    if (reply.after) {
      await reply.after;
      reply = reply.then;
    }
    // A provider refusing the request (a key it does not take, say).
    if (reply.refuse) {
      response.writeHead(reply.refuse, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: { type: 'authentication_error', message: 'this test refuses the request' } }));
      return;
    }
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    if (reply.hold !== undefined) {
      response.write(sse({ content: reply.hold }).split('\n\n')[0] + '\n\n');
      scripted.holding.push(response);
      return;
    }
    response.end(sse(reply));
    return;
  }
  response.writeHead(404).end();
});
const port = await new Promise((done) => server.listen(0, '127.0.0.1', () => done(server.address().port)));
const pageUrl = `http://127.0.0.1:${port}/page.html`;

function sse(reply) {
  // Text first, then a tool call: either or both.
  const deltas = [
    ...(reply.content != null ? [{ content: reply.content }] : []),
    ...(reply.call ? [{ tool_calls: [{ index: 0, id: `call_${scripted.seen.length}`, type: 'function', function: { name: reply.call[0], arguments: JSON.stringify(reply.call[1]) } }] }] : []),
  ];
  const chunks = [
    ...deltas.map((delta) => ({ id: 'c', object: 'chat.completion.chunk', choices: [{ index: 0, delta, finish_reason: null }] })),
    { id: 'c', object: 'chat.completion.chunk', choices: [{ index: 0, delta: {}, finish_reason: reply.call ? 'tool_calls' : 'stop' }] },
    { id: 'c', object: 'chat.completion.chunk', choices: [], usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 } },
  ];
  return `${chunks.map((chunk) => `data: ${JSON.stringify(chunk)}\n\n`).join('')}data: [DONE]\n\n`;
}
const text = (content) => ({ content });
const call = (name, input) => ({ call: [name, input] });

// ---- The provider: from the catalog, never from this file.

let catalogFile = options.catalog ? resolve(options.catalog) : null;
if (options.scripted) {
  catalogFile = join(work, 'providers.json');
  fs.writeFileSync(catalogFile, JSON.stringify({
    version: 4, active_provider: 'scripted', providers: [{
      id: 'scripted', display_name: 'Scripted', api_provider: 'openai', request_contract: 'openai-chat-completions',
      base_url: `http://127.0.0.1:${port}/v1`, default_model: 'scripted-model', models: ['scripted-model', 'scripted-small'],
      requires_api_key: true,
      // Priced, so a native host can keep a budget on it; a small window, so a thread
      // compacts its history soon.
      model_specs: {
        'scripted-model': { context_window: 200000, max_output_tokens: 4096, pricing: { input_usd_per_mtok: 1, output_usd_per_mtok: 2, cache_read_usd_per_mtok: 0.1, cache_create_usd_per_mtok: 0 } },
        'scripted-small': { context_window: 20000, max_output_tokens: 1024, pricing: { input_usd_per_mtok: 1, output_usd_per_mtok: 2, cache_read_usd_per_mtok: 0.1, cache_create_usd_per_mtok: 0 } },
      },
    }],
  }));
}
const providerId = options.scripted ? 'scripted' : options.provider;
const glue = await import(resolve(distArg, 'web', 'spindle_embed.js'));
await glue.default({ module_or_path: fs.readFileSync(resolve(distArg, 'web', 'spindle_embed_bg.wasm')) });
const probe = new glue.Embedded('memory', '{}', catalogFile ? fs.readFileSync(catalogFile, 'utf8') : undefined);
const profileOf = JSON.parse(probe.catalog()).providers.find((p) => p.id === providerId);
probe.free();
if (!profileOf) throw new Error(`provider ${providerId} is not in the catalog`);
const providerOrigin = new URL(profileOf.base_url).origin;

// A Spindle host for the remote runner and threads moved there (with SPINDLE_BIN), in a
// home of its own and a directory only this user reads; scripted, its model is this
// test's provider (the catalog and the key in that home), so nothing is paid for.
let spindleHost = null;
/** Starts the host's `serve` (port 0: any free one but the ports a live host uses). */
async function serveHost(port = 0) {
  for (;;) {
    const serve = spawn(native, ['--dir', spindleHost.dir, 'serve', '--port', String(port)], { env: spindleHost.env, stdio: ['ignore', 'pipe', 'ignore'] });
    const hostPort = Number(await new Promise((done) => serve.stdout.once('data', (text) => done(/127\.0\.0\.1:(\d+)/.exec(String(text))?.[1]))));
    if (port === 0 && [8790, 8799, 8811].includes(hostPort)) {
      serve.kill();
      continue;
    }
    Object.assign(spindleHost, { serve, port: hostPort, url: `http://127.0.0.1:${hostPort}` });
    return;
  }
}
if (native) {
  const home = join(work, 'host-home');
  const hostEnv = { ...process.env, HOME: home, DEXT_HOME: join(home, '.dext') };
  fs.mkdirSync(hostEnv.DEXT_HOME, { recursive: true, mode: 0o700 });
  if (options.scripted) {
    fs.copyFileSync(catalogFile, join(hostEnv.DEXT_HOME, 'providers.json'));
    fs.writeFileSync(join(hostEnv.DEXT_HOME, 'auth.json'), JSON.stringify({ version: 1, providers: { scripted: { type: 'api_key', key } } }), { mode: 0o600 });
  }
  const dir = join(work, 'host');
  fs.mkdirSync(dir, { mode: 0o700 });
  const token = execFileSync(native, ['--dir', dir, 'token', 'new', 'chrome'], { env: hostEnv, encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim();
  spindleHost = { dir, env: hostEnv, token };
  await serveHost();
  const probe = await fetch(`${spindleHost.url}/v1/runner`, { headers: { authorization: `Bearer ${token}` } });
  spindleHost.sandbox = probe.status === 200;
  process.on('exit', () => spindleHost.serve.kill());
}

// The test's copy of the extension, its permissions granted up front: first the
// provider's site and this test's page only (page reading off: the page's own site
// must not be enough), then what the Page reading switch asks for as well.
const dist = join(work, 'extension');
fs.cpSync(resolve(distArg), dist, { recursive: true });
const manifest = JSON.parse(fs.readFileSync(join(dist, 'manifest.json'), 'utf8'));
function grant(pages) {
  const sites = [`${providerOrigin}/*`, `http://127.0.0.1:${port}/*`, ...(spindleHost ? [`${spindleHost.url}/*`] : []),
    ...(pages ? manifest.optional_host_permissions : [])];
  // With pages: Chrome's access to sites, as page reading (or a thread's network) asks
  // for it. The Pages switches themselves are Spindle's, clicked as the user would.
  const permissions = manifest.permissions;
  fs.writeFileSync(join(dist, 'manifest.json'), JSON.stringify({ ...manifest, permissions, host_permissions: [...new Set(sites)] }, null, 2));
}
grant(false);

// ---- Driving Chrome and the panel.

const results = [];
function check(name, run) {
  try {
    run();
    results.push([name, true]);
    console.log(`ok   ${name}`);
  } catch (error) {
    results.push([name, false, error.message]);
    console.log(`FAIL ${name}\n     ${error.message.split('\n').join('\n     ')}`);
  }
}

/** Opens folded parts of Settings (the dialog open), as the user does. */
const unfold = (panel, ...names) => panel.eval(`(() => { for (const name of ${JSON.stringify(names)}) document.querySelector('[data-fold="' + name + '"]').open = true; })()`);

let extensionId = null;
async function startChrome() {
  browser = await launch(browserBinary, { profile, downloads });
  extensionId = (await browser.send('Extensions.loadUnpacked', { path: dist })).id;
  const page = await open(browser, pageUrl, { newWindow: true });
  await page.until(`document.title === 'Spindle test page'`);
  // Chrome's first blank tab would otherwise be the one the user is looking at.
  for (const target of (await browser.send('Target.getTargets')).targetInfos) {
    if (target.type === 'page' && target.url === 'about:blank') await browser.send('Target.closeTarget', { targetId: target.targetId });
  }
  return page;
}

/** The panel's console and uncaught errors, the latest kept, for a failure to show. */
const panelConsole = [];
/** What the frames of a page say on the console ({page, text}): a preview's frame runs
 * in a process of its own, so each is attached as it comes (held until it is heard). */
const frameSaid = [];
async function hearFrames(page) {
  const children = new Set();
  browser.on(({ method, params, sessionId }) => {
    if (method === 'Target.attachedToTarget' && (sessionId === page.sessionId || children.has(sessionId))) {
      const child = params.sessionId;
      children.add(child);
      (async () => {
        await browser.send('Runtime.enable', {}, child).catch(() => {});
        await browser.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: true, flatten: true }, child).catch(() => {});
        if (params.waitingForDebugger) await browser.send('Runtime.runIfWaitingForDebugger', {}, child).catch(() => {});
      })();
    } else if (method === 'Runtime.consoleAPICalled' && children.has(sessionId)) {
      frameSaid.push({ page: page.sessionId, text: params.args.map((arg) => arg.value ?? '').join(' ') });
    }
  });
  await browser.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: true, flatten: true }, page.sessionId);
}
async function openPanel() {
  const panel = await open(browser, `chrome-extension://${extensionId}/panel.html`, { newWindow: true });
  browser.on(({ method, params, sessionId }) => {
    if (sessionId !== panel.sessionId) return;
    if (method === 'Runtime.consoleAPICalled') panelConsole.push(`${params.type}: ${params.args.map((arg) => arg.value ?? arg.description ?? '').join(' ')}`);
    else if (method === 'Runtime.exceptionThrown') panelConsole.push(`exception: ${params.exceptionDetails.exception?.description ?? params.exceptionDetails.text}`);
    else return;
    if (panelConsole.length > 40) panelConsole.shift();
  });
  await panel.size(420, 860);
  await hearFrames(panel);
  await panel.until('globalThis.spindlePanel?.engine', { timeout: 60_000, what: 'the engine to answer the panel' });
  return panel;
}

const state = (panel, id) => panel.eval(`spindlePanel.threads.get(${JSON.stringify(id)})?.state ?? null`);
async function settled(panel, id, timeout = 240_000) {
  return panel.until(`(() => { const s = spindlePanel.threads.get(${JSON.stringify(id)})?.state; return s && s !== 'running' ? s : null; })()`, { timeout, what: `thread ${id} to finish` });
}
/** The thread's last answer, once the panel has drawn it (a fast turn can end first). */
const lastAnswer = (panel) => panel.until(`(() => { const text = [...document.querySelectorAll('#history .message.assistant')].at(-1)?.innerText ?? ''; return text.trim() ? text : null; })()`, { timeout: 15_000, what: 'an answer on screen' }).catch(() => '');
const toolNames = (panel) => panel.eval(`[...document.querySelectorAll('#history .tool .tool-name')].map((n) => n.textContent)`);
const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

async function sendPrompt(panel, prompt) {
  await panel.type('#prompt', prompt);
  await panel.click('#send');
}

async function newThread(panel, prompt) {
  const before = await panel.eval('spindlePanel.current');
  await sendPrompt(panel, prompt);
  return panel.until(`spindlePanel.current !== ${JSON.stringify(before)} && spindlePanel.current`, { what: 'the new thread to open' });
}

/** The thread's files, as its workspace page lists them (scratch files shown too, every
 * folder opened). */
async function filesOf(panel) {
  await panel.click('#title-button');
  await panel.click('#menu-files');
  await panel.until(`!document.querySelector('#workspace').hidden && !document.querySelector('#ws-list').hidden || null`, { what: 'the workspace page' });
  await panel.eval(`(() => { const toggle = document.querySelector('#ws-scratch'); if (toggle.getAttribute('aria-checked') !== 'true') toggle.click(); })()`);
  // Folders start shut: every one opened, so every file has its row.
  const names = await panel.until(`(() => { for (let shut; (shut = [...document.querySelectorAll('#file-list .ws-folder-head[aria-expanded="false"]')]).length;) for (const head of shut) head.click(); const rows = [...document.querySelectorAll('#file-list .file-row[data-path]')]; return rows.length ? rows.map((row) => row.dataset.path) : /No files yet/.test(document.querySelector('#file-list .note')?.textContent ?? '') ? [] : null; })()`, { what: 'the file list' });
  return names;
}

async function closeFiles(panel) {
  await panel.eval(`document.body.dataset.view === 'workspace' && document.querySelector('#ws-done').click()`);
  await panel.until(`document.body.dataset.view !== 'workspace' || null`, { what: 'the conversation again' });
  return '';
}

/** A file's text, opened from the workspace page (which then closes). */
async function readFile(panel, path) {
  await panel.eval(`document.querySelector('#file-list .file-row[data-path=' + CSS.escape(${JSON.stringify(path)}) + '] .file-open').click()`);
  const content = await panel.until(`(() => { const shown = document.querySelector('#file-content'); return shown && !document.querySelector('#ws-file').hidden && shown.textContent; })()`, { what: `${path} shown` });
  await closeFiles(panel);
  return content;
}

/** The engine's free storage slots, as a newly opened panel is told. */
async function freeSlots() {
  const panel = await openPanel();
  const free = await panel.eval('spindlePanel.engine.free');
  await panel.close();
  return free;
}

/** Clicks `selector` and resolves with the file the click downloads. */
async function downloadBy(panel, selector, named = /\.spindle$/) {
  // The panel in front and shown (a tab a thread opened in its window hides it here; the
  // user's side panel stays shown), so the click reaches it.
  await browser.send('Target.activateTarget', { targetId: panel.targetId }).catch(() => {});
  await panel.until(`document.visibilityState === 'visible'`, { what: 'the panel to show' });
  // Where a click goes and what reaches Chrome's downloads, said if no download comes.
  await panel.eval(`(() => {
    window.spindleProbe = { clicks: [], downloads: [], reads: [] };
    if (!window.spindleProbed) {
      window.spindleProbed = true;
      addEventListener('click', (event) => spindleProbe.clicks.push(event.target?.outerHTML?.slice(0, 90) ?? String(event.target)), true);
      const real = chrome.downloads.download.bind(chrome.downloads);
      chrome.downloads.download = (options) => { spindleProbe.downloads.push(options.filename); return real(options); };
    }
  })()`).catch(() => {});
  const done = browser.nextDownload();
  await panel.click(selector);
  // What the click landed on, said if no download comes.
  const landed = await panel.eval(`(() => { const node = document.querySelector(${JSON.stringify(selector)}); if (!node) return 'gone'; const r = node.getBoundingClientRect(); const at = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2); return at === node ? 'the button' : (at?.outerHTML ?? 'nothing').slice(0, 160); })()`).catch((error) => error.message);
  const file = await done.catch(async (error) => {
    const said = await panel.eval(`document.querySelector('#toast').hidden ? '' : document.querySelector('#toast').textContent`).catch(() => '');
    // How long the engine takes to answer a read now, and what the panel logged.
    const roundTrip = await panel.eval(`new Promise((done) => { const c = new BroadcastChannel('spindle-engine'); const me = 'probe-' + Math.random(); const t = performance.now(); c.onmessage = ({ data }) => { if (data?.to === me) { c.close(); done(Math.round(performance.now() - t) + ' ms ' + (data.ok ? 'ok' : data.error?.message)); } }; c.postMessage({ to: 'engine', from: me, id: 1, op: 'hello', args: {} }); setTimeout(() => done('no answer in 20 s'), 20000); })`).catch((failure) => failure.message);
    // What Chrome's own downloads list says of the latest ones.
    const listed = await panel.eval(`chrome.downloads.search({ orderBy: ['-startTime'], limit: 3 }).then((items) => JSON.stringify(items.map(({ filename, state, danger, error, paused, url, bytesReceived }) => ({ filename, state, danger, error, paused, url: url.slice(0, 60), bytesReceived }))))`).catch((failure) => failure.message);
    const probe = await panel.eval('JSON.stringify(window.spindleProbe)').catch((failure) => failure.message);
    throw new Error(`${error.message}${said ? ` (the panel said: ${said})` : ''} (the click landed on: ${landed}; the engine answers in ${roundTrip}; the panel logged: ${panelConsole.slice(-8).join(' | ') || 'nothing'}; Chrome's downloads: ${listed}; probe: ${probe})`);
  });
  // Through Chrome's downloads API the file lands under its download id here (this
  // test's download behavior names files so): the name asked for is what is checked.
  const asked = await panel.eval('window.spindleProbe?.downloads.at(-1) ?? null').catch(() => null);
  assert.ok(named.test(asked ?? file.name), `downloaded as ${asked ?? file.name}`);
  return file.path;
}

/**
 * The leg on the user's Spindle host (SPINDLE_BIN, scripted): Connect through the host's
 * own page, Run on my host, turns there (a program only the host has, steering, Stop and
 * Retry, a page tool answered by this browser), the engine restarted mid-turn, the host
 * out of reach and back, Bring back (the host's pack session id kept), and a crash or a
 * reload at every step of both moves: one live copy after each. Returns the panel (a
 * crash opens another).
 */
async function hostedLeg(panel) {
  const hostGet = async (path) => (await fetch(`${spindleHost.url}${path}`, { headers: { authorization: `Bearer ${spindleHost.token}` } })).json();
  const engineOp = (op, args = {}) => panel.eval(`new Promise((done, fail) => {
    const channel = new BroadcastChannel('spindle-engine');
    const me = 'test-' + Math.random();
    channel.onmessage = ({ data }) => { if (data?.to === me) { channel.close(); data.ok ? done(data.result) : fail(new Error(data.error.message)); } };
    channel.postMessage({ to: 'engine', from: me, id: 1, op: ${JSON.stringify(op)}, args: ${JSON.stringify(args)} });
    setTimeout(() => fail(new Error('the engine did not answer')), 60_000);
  })`);
  const threadOf = (id) => panel.eval(`spindlePanel.threads.get(${JSON.stringify(id)}) ?? null`);
  const sendHosted = async (prompt) => {
    const before = scripted.seen.length;
    await sendPrompt(panel, prompt);
    await waitFor(() => scripted.seen.length > before, 'the host to admit the new prompt');
  };
  const drawnAnswer = (text) => panel.until(`(() => { const answer = [...document.querySelectorAll('#history .message.assistant')].at(-1)?.innerText ?? ''; return answer.includes(${JSON.stringify(text)}) && answer; })()`, { timeout: 20_000, what: `the new answer ${text} to be drawn` });
  const lastCard = (tool) => panel.eval(`(() => { const card = [...document.querySelectorAll('#history .tool[data-tool=${JSON.stringify(tool)}]')].at(-1); return card ? { failed: card.classList.contains('failed'), text: card.querySelector('pre.result')?.textContent ?? '' } : null; })()`);
  const targets = async () => (await browser.send('Target.getTargets')).targetInfos;
  const waitFor = async (probe, what, timeout = 30_000) => {
    for (const end = Date.now() + timeout; ;) {
      const found = await probe();
      if (found) return found;
      if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
      await sleep(200);
    }
  };
  const openIt = async (id) => {
    await panel.click('#home');
    await panel.click(`#threads .thread-row[data-id="${id}"]`);
    await panel.until(`spindlePanel.current === ${JSON.stringify(id)}`, { what: 'the hosted thread to open' });
  };
  /** Where the thread lives: 'host' or 'here' once no move is under way. */
  const settledHome = async (id, where, timeout = 90_000) => {
    try {
      return await panel.until(`(() => { const t = spindlePanel.threads.get(${JSON.stringify(id)}); return t && !t.moving && (${where === 'host'} ? Boolean(t.home) : !t.home) && t; })()`, { timeout, what: `the thread to live ${where === 'host' ? 'on the host' : 'here'}` });
    } catch (error) {
      // Say where the move stands, as the panel shows it and as the engine holds it.
      const facts = (thread) => JSON.stringify(thread && { moving: thread.moving ?? null, home: thread.home ?? null, state: thread.state, note: thread.note ?? null });
      const shown = await threadOf(id).then(facts).catch((why) => `unreadable (${why.message})`);
      const held = await engineOp('hello').then((state) => facts(state.threads.find((thread) => thread.id === id))).catch((why) => `unreadable (${why.message})`);
      throw new Error(`${error.message}; the panel shows ${shown}; the engine holds ${held}`);
    }
  };
  /** The thread's live copies: the host's sessions not sealed, and its store here. */
  const liveCopies = async (id) => {
    const { sessions } = await hostGet('/v1/sessions');
    const usage = (await engineOp('storage')).threads.find((one) => one.id === id);
    const thread = await threadOf(id);
    return {
      host: sessions.filter((session) => session.status?.state !== 'moved').length,
      sealedThere: sessions.filter((session) => session.status?.state === 'moved').length,
      here: thread && !thread.home && !thread.moving ? 1 : 0,
      storeHere: usage ? usage.database : 0,
      held: usage?.held ?? 0,
      move: usage?.move ?? null,
    };
  };

  // Connect: the host's own page opens in a tab, pairs with the host's key and allows
  // this browser; the extension catches the answer there, closes the tab and keeps a
  // browser token named for this browser.
  step = 'hosted: connect';
  await panel.click('#home');
  await panel.click('#settings');
  await unfold(panel, 'commands', 'host');
  await panel.eval(`document.querySelector('#host-url').value = ''`);
  await panel.type('#host-url', spindleHost.url);
  await panel.click('#host-connect');
  const connectTarget = await waitFor(async () => (await targets()).find((target) => target.type === 'page' && target.url.startsWith(`${spindleHost.url}/connect?`)), 'the connect tab');
  const tab = await attach(browser, connectTarget.targetId);
  await tab.until(`document.readyState === 'complete' && document.querySelector('#connect-card') && true`, { what: 'the connect page' }).catch(async (error) => {
    throw new Error(`${error.message}; the tab shows ${await tab.eval('location.href + " " + document.body?.innerText.slice(0, 300)').catch((failed) => failed.message)}`);
  });
  const askedFor = await tab.eval(`document.querySelector('#connect-card').dataset.client`);
  if (await tab.eval(`!document.querySelector('#connect-pair').hidden`)) {
    await tab.type('#pair-key', fs.readFileSync(join(spindleHost.dir, 'pairing.key'), 'utf8').trim());
    await tab.click('#pair-form button');
  }
  await tab.click('#allow');
  const connected = await panel.until(`spindlePanel.engine.host?.kind === 'browser' && spindlePanel.engine.host`, { timeout: 30_000, what: 'the host to connect with a browser token' });
  const tabClosed = await waitFor(async () => !(await targets()).some((target) => target.targetId === connectTarget.targetId), 'the connect tab to close', 10_000).catch(() => false);
  const tokens = execFileSync(native, ['--dir', spindleHost.dir, 'token', 'list'], { env: spindleHost.env, encoding: 'utf8' });
  const hostSaid = await panel.eval(`document.querySelector('#host-state').textContent`);
  await panel.click('#settings-dialog [data-close]');
  check("Connect: the host's page pairs and allows in a tab; the extension catches the answer, closes the tab and keeps a browser token named for this browser", () => {
    assert.match(askedFor, /^Chrome on Linux [0-9a-f]{6}$/, `the name asked for: ${askedFor}`);
    assert.equal(connected.client, askedFor);
    assert.ok(tabClosed, 'the connect tab closed');
    assert.ok(tokens.includes(askedFor) && /\(browser\)/.test(tokens), `the host's tokens: ${tokens}`);
    assert.match(hostSaid, /threads and commands/, hostSaid);
  });

  // Run on my host: the move, a turn there running a program only the host has, and
  // what is left here.
  step = 'hosted: move';
  scripted.replies.push(text('Ready here.'));
  const id = await newThread(panel, 'Reply with only: ready.');
  await settled(panel, id);
  await panel.click('#title-button');
  await panel.click('#menu-host');
  await panel.until(`document.querySelector('#host-dialog').open`, { what: 'the host dialog' });
  const asksModel = await panel.eval(`!document.querySelector('#host-model-field').hidden`);
  const told = await panel.eval(`document.querySelector('#host-dialog-text').textContent`);
  await panel.click('#host-dialog-yes');
  const home = (await settledHome(id, 'host')).home;
  const there = await liveCopies(id);
  scripted.replies.push(call('bash', { command: 'uname -m && gcc --version | head -1' }), text('Ran on the host.'));
  await sendPrompt(panel, 'Run uname -m and gcc --version with bash.');
  await panel.until(`spindlePanel.threads.get(${JSON.stringify(id)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
  const ran = await settled(panel, id, 120_000);
  const hostCard = await lastCard('bash');
  const ranAnswer = await lastAnswer(panel);
  await panel.screenshot(join(shots, '08-hosted.png'));
  check('Run on my host: the thread moves (one live copy, on the host; here only the note of where it went) and a turn there runs a program only the host has', () => {
    assert.equal(asksModel, false, 'its model is signed in on the host: nothing to choose');
    assert.match(told, /keeps its copy until your host holds the thread/, told);
    assert.equal(home.url, spindleHost.url);
    assert.deepEqual([there.host, there.here, there.storeHere, there.held, there.move], [1, 0, 0, 0, null], JSON.stringify(there));
    assert.equal(ran, 'idle');
    assert.ok(hostCard && !hostCard.failed, JSON.stringify(hostCard));
    assert.match(hostCard.text, /x86_64/, "the host's machine, not the browser Linux's");
    assert.match(hostCard.text, /gcc/i, 'a program the browser Linux does not have');
    assert.match(ranAnswer, /Ran on the host/);
  });

  // Steering, a page tool this browser answers, Stop and Retry: driven as here.
  step = 'hosted: steer';
  let release;
  const gate = new Promise((done) => { release = done; });
  const steer = `Hosted steer ${randomBytes(2).toString('hex')}`;
  scripted.replies.push({ after: gate, then: call('read_page', {}) }, text('Steered and read.'));
  const seenBefore = scripted.seen.length;
  await sendPrompt(panel, 'Wait for my next message, then read the page.');
  await panel.until(`spindlePanel.threads.get(${JSON.stringify(id)})?.state === 'running' || null`, { what: 'the hosted turn to run' });
  await waitFor(() => scripted.seen.length > seenBefore || null, 'the host to ask the model');
  await sendPrompt(panel, steer);
  const steered = await panel.until(`!document.querySelector('#toast').hidden && document.querySelector('#toast').textContent === 'Sent to the running turn.'`, { what: 'the steering message to be taken' }).catch((error) => error.message);
  release();
  const afterSteer = await settled(panel, id, 120_000);
  const pageCard = await lastCard('read_page');
  check('a hosted turn is steered from the panel, and the page tool it calls is answered by this browser', () => {
    assert.equal(steered, true, steered);
    assert.equal(afterSteer, 'idle');
    assert.ok(scripted.seen.slice(seenBefore).some((seen) => seen.body.includes(steer)), 'the steering message reached the model');
    assert.ok(pageCard, 'the page tool ran');
    assert.doesNotMatch(pageCard.text, /no_answer|did not answer|no answer/i, `answered: ${pageCard.text}`);
  });

  step = 'hosted: stop';
  scripted.replies.push({ hold: 'Once upon a time, on a host, ' }, text('Retried on the host.'));
  await sendPrompt(panel, 'Write a long story.');
  await panel.until(`(document.querySelector('#history .live .message')?.innerText ?? '').length > 10`, { timeout: 60_000, what: 'the hosted reply to stream' });
  await panel.until(`document.querySelector('#send').classList.contains('stop') || null`, { what: 'Send to become Stop' });
  await panel.click('#send');
  const stopped = await settled(panel, id, 60_000);
  const stopNote = await panel.eval(`spindlePanel.threads.get(${JSON.stringify(id)})?.note ?? ''`);
  for (const response of scripted.holding.splice(0)) response.destroy();
  // (The steering's toast, still up, lies over the buttons for a few seconds.)
  await panel.until(`document.querySelector('#toast').hidden || null`, { timeout: 10_000, what: 'the toast to go' }).catch(() => {});
  await panel.click('#retry');
  await panel.until(`spindlePanel.threads.get(${JSON.stringify(id)})?.state === 'running' || null`, { timeout: 10_000 }).catch(() => {});
  const retried = await settled(panel, id, 60_000);
  const retriedNote = await panel.eval(`spindlePanel.threads.get(${JSON.stringify(id)})?.note ?? ''`);
  const retriedAnswer = await lastAnswer(panel);
  check('Stop ends a hosted turn mid-reply, and Retry sends it again on the host', () => {
    assert.equal(stopped, 'stopped', `after Stop: ${stopped} (${stopNote})`);
    assert.equal(retried, 'idle', `after Retry: ${retried} (${retriedNote})`);
    assert.match(retriedAnswer, /Retried on the host/);
  });

  // The engine restarted while a hosted turn runs: the new engine finds it running there
  // and follows it to its end.
  step = 'hosted: reload';
  let releaseAfter;
  const afterReload = new Promise((done) => { releaseAfter = done; });
  scripted.replies.push({ after: afterReload, then: text('Finished after the reload.') });
  const seenReload = scripted.seen.length;
  await sendPrompt(panel, 'Take your time.');
  await waitFor(() => scripted.seen.length > seenReload || null, 'the host to ask the model');
  await panel.eval('globalThis.beforeRestart = spindlePanel.engine');
  await panel.click('#settings');
  await unfold(panel, 'about');
  await panel.click('#restart-engine');
  await panel.until('spindlePanel.engine && spindlePanel.engine !== globalThis.beforeRestart', { timeout: 60_000, what: 'a new engine to answer' }).catch(async (error) => {
    throw new Error(`${error.message}; the panel says ${await panel.eval(`document.querySelector('#banner').textContent + ' / ' + document.querySelector('#toast-text').textContent`).catch(() => '?')}; it logged ${panelConsole.slice(-6).join(' | ')}`);
  });
  await panel.click('#settings-dialog [data-close]').catch(() => {});
  const runningAfter = await panel.until(`spindlePanel.threads.get(${JSON.stringify(id)})?.state === 'running' || null`, { timeout: 30_000 }).catch(() => false);
  releaseAfter();
  await openIt(id);
  const reloaded = await settled(panel, id, 90_000);
  const reloadAnswer = await lastAnswer(panel);
  check('the engine restarted mid-turn: the hosted turn goes on there, and the new engine follows it to its end', () => {
    assert.ok(runningAfter, 'still running after the restart');
    assert.equal(reloaded, 'idle');
    assert.match(reloadAnswer, /Finished after the reload/);
  });

  // The host out of reach: the thread says so and loses nothing; back, it goes on.
  step = 'hosted: away';
  spindleHost.serve.kill('SIGTERM');
  await new Promise((done) => spindleHost.serve.once('exit', done));
  await openIt(id);
  const awaySaid = await panel.until(`spindlePanel.threads.get(${JSON.stringify(id)})?.away ?? null`, { timeout: 45_000, what: 'the thread to say its host is away' });
  const awayShown = await panel.eval(`document.querySelector('#history').textContent + ' ' + (document.querySelector('#notice').hidden ? '' : document.querySelector('#notice').textContent)`);
  await serveHost(spindleHost.port);
  await panel.until(`!spindlePanel.threads.get(${JSON.stringify(id)})?.away || null`, { timeout: 45_000, what: 'the host to be in reach again' });
  scripted.replies.push(text('Back in reach.'));
  await sendHosted('Are you there?');
  const backInReach = await settled(panel, id, 60_000);
  await drawnAnswer('Back in reach.');
  const history = await panel.eval(`document.querySelector('#history').innerText`);
  check('the host out of reach: the thread says so, loses nothing, and goes on once the host is back', () => {
    assert.match(awaySaid, /does not answer|could not be reached|not connected/, awaySaid);
    assert.match(awayShown, /out of reach|does not answer/, awayShown);
    assert.equal(backInReach, 'idle');
    assert.match(history, /Ran on the host[\s\S]*Retried on the host[\s\S]*Finished after the reload[\s\S]*Back in reach/, 'its whole history');
  });

  // Stop while the host is offline survives an engine restart without starting work.
  step = 'hosted: offline stop';
  scripted.replies.push({ hold: 'A reply held while the host goes away, ' });
  await sendHosted('Wait until I stop you.');
  const original = (await hostGet(`/v1/sessions/${home.session}`)).running_request;
  assert.ok(original, 'the host exposes the exact running command');
  spindleHost.serve.kill('SIGTERM');
  await new Promise((done) => spindleHost.serve.once('exit', done));
  const stoppedOffline = await engineOp('stop', { id });
  const keptStop = await panel.eval(`import('./shared.mjs').then((shared) => shared.read('kv', 'host-stop:${id}'))`);
  for (const response of scripted.holding.splice(0)) response.destroy();
  await panel.eval('globalThis.beforeRestart = spindlePanel.engine');
  await panel.click('#settings');
  await unfold(panel, 'about');
  await panel.click('#restart-engine');
  await panel.until('spindlePanel.engine && spindlePanel.engine !== globalThis.beforeRestart', { timeout: 60_000, what: 'the engine with a saved Stop to restart' });
  await panel.click('#settings-dialog [data-close]').catch(() => {});
  const beforeRecovery = scripted.seen.length;
  await serveHost(spindleHost.port);
  await panel.until(`import('./shared.mjs').then((shared) => shared.read('kv', 'host-stop:${id}')).then((stop) => !stop)`, { timeout: 60_000, what: 'the original command Stop to reconcile' });
  await settled(panel, id, 60_000);
  const recoverySends = scripted.seen.length - beforeRecovery;
  scripted.replies.push(text('A later turn is not stopped.'));
  await openIt(id);
  await engineOp('send', { id, text: 'Skip the interrupted reply and go on.' });
  const laterState = await settled(panel, id, 60_000);
  check('offline Stop is kept across engine and host restarts, never resends work and cannot stop a later turn', () => {
    assert.equal(stoppedOffline.pending, true);
    assert.equal(keptStop.target, original);
    assert.equal(recoverySends, 0);
    assert.equal(laterState, 'idle');
  });

  // Bring back: the browser's fit said first, the host's pack session id kept with it.
  step = 'hosted: back';
  const session = home.session;
  fs.writeFileSync(join(spindleHost.dir, 'sessions', session, 'pack-session-id'), '1761990000-77-a1b2c3d4e5f6\n');
  await panel.click('#title-button');
  await panel.click('#menu-back');
  const asked = await panel.until(`document.querySelector('#confirm-dialog').open && document.querySelector('#confirm-title').textContent + ' ' + document.querySelector('#confirm-text').textContent`, { what: 'Bring back to ask first' });
  await panel.click('#confirm-yes');
  const back = await settledHome(id, 'here');
  const afterBack = await liveCopies(id);
  scripted.replies.push(call('read_file', { path: 'nothing.txt' }), text('Here again.'));
  await sendPrompt(panel, 'Say you are here again.');
  const hereAgain = await settled(panel, id, 60_000);
  const tools = await panel.eval(`spindlePanel.status?.settings?.tools?.map((tool) => tool.name) ?? []`);
  await panel.click('#title-button');
  const copy = fs.readFileSync(await downloadBy(panel, '#menu-copy'));
  bundles.push(['hosted copy', copy]);
  check('Bring back: asked first, it moves back (the host deletes its copy), runs here with the page tools, and keeps the pack session id', () => {
    assert.match(asked, /Keep this thread here\? .*its commands run in this browser's Linux/, asked);
    assert.match(back.note ?? '', /Back from your host/, back.note);
    assert.deepEqual([afterBack.host, afterBack.sealedThere, afterBack.here, afterBack.held, afterBack.move], [0, 0, 1, 0, null], JSON.stringify(afterBack));
    assert.equal(hereAgain, 'idle');
    assert.ok(tools.includes('read_page'), `tools here: ${tools}`);
    assert.ok(copy.includes(Buffer.from('pack-session-id')) && copy.includes(Buffer.from('1761990000-77-a1b2c3d4e5f6')), 'the pack session id came back and leaves with the thread again');
  });

  // A crash (Chrome killed) or a reload (the engine restarted) at every step of both
  // moves: each finishes by itself once the engine starts again, one live copy after.
  step = 'hosted: cuts';
  const cuts = [
    ['to:recorded', 'reload'], ['back:recorded', 'reload'], ['to:uploaded', 'crash'], ['back:held', 'crash'],
    ['to:session', 'reload'], ['back:imported', 'reload'], ['to:sealed', 'reload'], ['back:released', 'reload'], ['to:home', 'reload'],
  ];
  const outcomes = [];
  for (const [at, how] of cuts) {
    step = `hosted: cut ${at} (${how})`;
    await panel.eval(`new Promise((done, fail) => { const opened = indexedDB.open('spindle', 2); opened.onsuccess = () => { const tx = opened.result.transaction('kv', 'readwrite', { durability: 'strict' }); tx.objectStore('kv').put(${JSON.stringify(at)}, 'cut-move'); tx.oncomplete = () => { opened.result.close(); done(true); }; tx.onerror = () => fail(tx.error); }; opened.onerror = () => fail(opened.error); })`);
    await panel.eval(`globalThis.cutAt = null; globalThis.cutHeard ??= new BroadcastChannel('spindle-engine'); cutHeard.onmessage = ({ data }) => { if (data?.type === 'cut') globalThis.cutAt = data.at; }`);
    await openIt(id);
    await panel.click('#title-button');
    if (at.startsWith('to:')) {
      await panel.click('#menu-host');
      await panel.until(`document.querySelector('#host-dialog').open`, { what: 'the host dialog' });
      await panel.click('#host-dialog-yes');
    } else {
      await panel.click('#menu-back');
      await panel.until(`document.querySelector('#confirm-dialog').open`, { what: 'Bring back to ask' });
      await panel.click('#confirm-yes');
    }
    await panel.until(`globalThis.cutAt === ${JSON.stringify(at)}`, { timeout: 60_000, what: `the move to stop at ${at}` });
    if (how === 'crash') {
      await browser.crash();
      await startChrome();
      panel = await openPanel();
    } else {
      await panel.eval('globalThis.beforeRestart = spindlePanel.engine');
      await panel.click('#settings');
      await unfold(panel, 'about');
      await panel.click('#restart-engine');
      await panel.until('spindlePanel.engine && spindlePanel.engine !== globalThis.beforeRestart', { timeout: 60_000, what: 'a new engine to answer' });
      await panel.click('#settings-dialog [data-close]').catch(() => {});
    }
    const lives = at.startsWith('to:') ? 'host' : 'here';
    await settledHome(id, lives, 120_000);
    outcomes.push([at, how, lives, await liveCopies(id)]);
  }
  // It still works where it ended up (on the host), and comes home once more.
  scripted.replies.push(text('Still whole.'));
  await openIt(id);
  await sendHosted('Still there?');
  const stillWhole = await settled(panel, id, 60_000);
  const wholeAnswer = await drawnAnswer('Still whole.');
  check('a crash or reload at every step of both moves: each move finishes by itself, one live copy after each, and the thread goes on', () => {
    for (const [at, how, lives, copies] of outcomes) {
      const want = lives === 'host' ? [1, 0, 0, 0, 0, null] : [0, 0, 1, 1, 0, null];
      const got = [copies.host, copies.sealedThere, copies.here, Math.min(1, copies.storeHere), copies.held, copies.move];
      assert.deepEqual(got, want, `${how} at ${at}: ${JSON.stringify(copies)}`);
    }
    assert.equal(stillWhole, 'idle');
    assert.match(wholeAnswer, /Still whole/);
  });
  return panel;
}

const bundles = [];
/** Thrown to end a run of only part of the steps (SPINDLE_ONLY). */
const ONLY = Symbol('only');
let step = 'start';
try {
  step = 'setup';
  let page = await startChrome();
  let panel = await openPanel();
  await panel.screenshot(join(shots, '01-first-open.png'));

  // Providers: the catalog file through Settings, then the key typed into its row.
  if (catalogFile) {
    await panel.files('#catalog-file', [catalogFile]);
    await panel.until(`spindlePanel.engine.catalog.providers.some((p) => p.id === ${JSON.stringify(providerId)}) && spindlePanel.engine.catalogSaved`, { what: 'the catalog import' });
    await panel.eval(`document.querySelector('#settings-dialog').open && document.querySelector('#settings-dialog').close()`);
  }
  await panel.click('#settings');
  const row = `.provider[data-provider="${providerId}"]`;
  await panel.click(`${row} .provider-actions button`);
  await panel.type(`${row} input`, key);
  await panel.click(`${row} button.primary`);
  await panel.until(`document.querySelector('${row} .provider-state')?.textContent === 'key saved'`, { what: 'the key to be saved' });
  await panel.screenshot(join(shots, '02-settings.png'));
  await panel.click('#settings-dialog [data-close]');
  // The model for new threads.
  await panel.click('#model-chip');
  await panel.click('#model-provider .picker-field');
  await panel.click(`#model-provider [role="option"][data-value="${providerId}"]`);
  if (options.model) {
    await panel.click('#model-name .picker-field');
    await panel.type('#model-name .picker-filter', options.model);
    await panel.click(`#model-name [role="option"][data-value="${options.model}"]`);
  }
  await panel.screenshot(join(shots, '02b-model.png'));
  await panel.click('#model-apply');
  // SPINDLE_ONLY=hosted: only the leg on the host (working on it), then the end.
  if (process.env.SPINDLE_ONLY === 'hosted') {
    panel = await hostedLeg(panel);
    throw ONLY;
  }

  // 1. The page tool: refused while page reading is off, then reading the page.
  step = 'page reading off';
  scripted.replies.push(call('read_page', {}), text('Page reading is off.'));
  const off = await newThread(panel, 'Use the read_page tool to read the web page I am looking at, then reply with only the secret word written on it.');
  await settled(panel, off);
  const offCard = await panel.eval(`(() => { const card = [...document.querySelectorAll('#history .tool')].find((c) => c.querySelector('.tool-name').textContent === 'read_page'); return card ? { failed: card.classList.contains('failed'), text: card.querySelector('pre.result')?.textContent ?? '' } : null; })()`);
  await panel.click('#settings');
  const switchText = await panel.until(`document.querySelector('#page-reading').getAttribute('aria-checked')`);
  await panel.click('#settings-dialog [data-close]');
  check('with page reading off, read_page refuses, even for a site Spindle may reach', () => {
    assert.ok(offCard, 'the thread called read_page');
    assert.equal(offCard.failed, true, `the call: ${JSON.stringify(offCard)}`);
    assert.match(offCard.text, /page reading is off/i);
    if (options.scripted) assert.ok(!scripted.seen.at(-1).body.includes(secret), 'no page text reached the model');
    assert.equal(switchText, 'false', 'the Page reading switch is off');
  });
  // Chrome's access to sites given (up front: a prompt cannot be clicked headless); then
  // Page actions switched on in Settings, as the user does.
  await browser.close();
  grant(true);
  page = await startChrome();
  panel = await openPanel();

  step = 'page tool';
  await panel.click('#settings');
  await unfold(panel, 'pages');
  const actionsBefore = await panel.until(`document.querySelector('#page-actions').getAttribute('aria-checked')`);
  // Chrome's access to sites alone turns no switch on: page reading is the user's to switch.
  const readingBefore = await panel.until(`document.querySelector('#page-reading').getAttribute('aria-checked')`);
  await panel.click('#page-reading');
  const switchOn = await panel.until(`document.querySelector('#page-reading').getAttribute('aria-checked') === 'true' && 'true'`, { what: 'Page reading to switch on' }).catch((error) => error.message);
  await panel.click('#page-actions');
  const actionsOn = await panel.until(`document.querySelector('#page-actions').getAttribute('aria-checked') === 'true' && 'true'`, { what: 'Page actions to switch on' }).catch((error) => error.message);
  await panel.click('#settings-dialog [data-close]');
  scripted.replies.push(call('read_page', {}), text(`The secret word is ${secret}.`));
  const first = await newThread(panel, 'Use the read_page tool to read the web page I am looking at, then reply with only the secret word written on it.');
  const firstState = await settled(panel, first);
  const firstAnswer = await lastAnswer(panel);
  const firstTools = await toolNames(panel);
  await panel.screenshot(join(shots, '03-page-tool.png'));
  check('a thread reads the page the user is on (read_page) and answers from it', () => {
    assert.equal(firstState, 'idle', `state ${firstState}`);
    assert.ok(firstTools.includes('read_page'), `tools ${firstTools}`);
    if (options.scripted) {
      assert.ok(scripted.seen.at(-1).body.includes(secret), 'the page text reached the model');
      assert.equal(scripted.seen.at(-1).authorization, `Bearer ${key}`, 'the key went where the catalog says');
    }
    assert.ok(firstAnswer.includes(secret), `answer: ${firstAnswer}`);
    assert.equal(readingBefore, 'false', "Chrome's access to sites alone does not turn page reading on");
    assert.equal(switchOn, 'true', 'the Page reading switch turns it on');
    assert.equal(actionsBefore, 'false', 'page actions start off');
    assert.equal(actionsOn, 'true', 'the Page actions switch turns them on');
  });

  // 7a. bash in the browser Linux (its first start boots it).
  step = 'browser linux';
  await panel.click('#home');
  // Scripted, the reply after the bash call waits, so the panel is seen mid-turn.
  let release = () => {};
  const beforeBash = 'Running it in the browser Linux.';
  if (options.scripted) {
    const gate = new Promise((done) => { release = done; });
    scripted.replies.push(
      { content: beforeBash, call: ['bash', { command: 'mkdir -p src/deep && python3 -c "print(6*7)" > src/deep/answer.txt && node -e "console.log(process.platform)" && uname -m && id -un' }] },
      { after: gate, then: text('Ran: linux i686 spindle, src/deep/answer.txt holds 42.') },
    );
  }
  const linuxThread = await newThread(panel, 'Use bash to run exactly: mkdir -p src/deep && python3 -c "print(6*7)" > src/deep/answer.txt && node -e "console.log(process.platform)" && uname -m && id -un. Then reply with only the command\'s output.');
  let keptText = true;
  if (options.scripted) {
    await panel.until(`document.querySelector('#history .tool:not(.running)') && true`, { timeout: 300_000, what: 'the bash call to finish' });
    keptText = await panel.eval(`[...document.querySelectorAll('#history .message.assistant')].some((m) => m.textContent.includes(${JSON.stringify(beforeBash)}))`);
    release();
  }
  const linuxState = await settled(panel, linuxThread, 300_000);
  const bashCard = await panel.eval(`(() => { const card = [...document.querySelectorAll('#history .tool')].find((c) => c.querySelector('.tool-name').textContent === 'bash'); return card ? { failed: card.classList.contains('failed'), text: card.querySelector('pre.result')?.textContent ?? '' } : null; })()`);
  const linuxFiles = await filesOf(panel);
  const answerFile = linuxFiles.includes('src/deep/answer.txt') ? await readFile(panel, 'src/deep/answer.txt') : await closeFiles(panel);
  await panel.screenshot(join(shots, '03b-browser-linux.png'));
  check('bash runs in the browser Linux: Python and Node, its file kept in a folder of the workspace; text before it stays', () => {
    assert.equal(linuxState, 'idle', `state ${linuxState}`);
    assert.ok(keptText, 'what the model wrote before the bash call stays on screen while the turn goes on');
    assert.ok(bashCard && !bashCard.failed, `the bash call: ${JSON.stringify(bashCard)}`);
    assert.match(bashCard.text, /linux[\s\S]*i686[\s\S]*spindle/, 'Node, uname and id answered from the browser Linux, as its unprivileged user');
    assert.match(answerFile, /^42/, `src/deep/answer.txt: ${JSON.stringify(answerFile)} (files: ${linuxFiles})`);
  });

  // 7c. Code mode through bash: a script asks Spindle for a model's answer and a tool.
  step = 'code through bash';
  const script = `python3 -c "import spindle; print(spindle.model('Reply with only the word teal.')['text'].strip()); print(spindle.tool('read_page')[:400])" && spindle model "Reply with only the word plum." && echo "naïve ✓"`;
  scripted.replies.push(call('bash', { command: script }), text('teal'), text('plum'), text('Ran it: teal, the page, plum.'));
  await panel.click('#home');
  await panel.click(`#threads .thread-row[data-id="${linuxThread}"]`);
  await panel.until(`spindlePanel.current === ${JSON.stringify(linuxThread)} && spindlePanel.status`, { what: 'the Linux thread to open' });
  const guide = await panel.eval(`spindlePanel.status.settings.instructions ?? ''`);
  await sendPrompt(panel, `Run exactly this command with bash, then reply with only its output: ${script}`);
  await panel.until(`spindlePanel.threads.get(${JSON.stringify(linuxThread)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
  const bridged = await settled(panel, linuxThread, Number(process.env.SPINDLE_STEP_WAIT ?? 300_000));
  const bridgeCard = await panel.eval(`(() => { const cards = [...document.querySelectorAll('#history .tool')].filter((c) => c.querySelector('.tool-name').textContent === 'bash'); const card = cards.at(-1); return card ? { failed: card.classList.contains('failed'), text: card.querySelector('pre.result')?.textContent ?? '' } : null; })()`);
  await panel.screenshot(join(shots, '03c-code-through-bash.png'));
  check("a bash script makes Spindle calls: a nested model's answer and a page tool (code mode through bash)", () => {
    assert.match(guide, /spindle model/, 'the thread is told about Spindle calls');
    assert.match(guide, /System\/Light\/Dark switch; never write em dashes/, 'the thread is told the style line');
    assert.equal(bridged, 'idle', `state ${bridged}`);
    assert.ok(bridgeCard && !bridgeCard.failed, `the bash call: ${JSON.stringify(bridgeCard)}`);
    assert.match(bridgeCard.text, /teal/i, 'the nested model answered the script');
    assert.ok(bridgeCard.text.includes(secret), 'the page tool answered the script');
    assert.match(bridgeCard.text, /plum/i, 'spindle model answered from bash');
    assert.ok(bridgeCard.text.includes('naïve ✓'), 'UTF-8 crosses to the Linux and back');
    if (options.scripted) {
      const nested = scripted.seen.filter((seen) => !seen.body.includes('"tools"'));
      assert.ok(nested.length >= 2, 'the nested model requests carried no tools');
      assert.equal(nested.at(-1).authorization, `Bearer ${key}`, 'nested calls carry the key the catalog says');
    }
  });

  // 7d. With the wasm Linux in the build (SPINDLE_WASM_LINUX): a plain sh command runs
  // there (uname says wasm), Python with it, and its file comes back.
  if (options.scripted && fs.existsSync(join(dist, 'wasm-linux/identity'))) {
    step = 'wasm linux';
    const plain = 'uname -m && python3 -c "print(sum(i * i for i in range(10 ** 6)))" > squares.txt && cat squares.txt';
    scripted.replies.push(call('bash', { command: plain }), text('Ran it in the wasm Linux.'));
    await sendPrompt(panel, `Run exactly this command with bash: ${plain}`);
    await panel.until(`spindlePanel.threads.get(${JSON.stringify(linuxThread)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
    const ranWasm = await settled(panel, linuxThread, 300_000);
    const wasmCard = await panel.eval(`(() => { const cards = [...document.querySelectorAll('#history .tool')].filter((c) => c.querySelector('.tool-name').textContent === 'bash'); const card = cards.at(-1); return card ? { failed: card.classList.contains('failed'), text: card.querySelector('pre.result')?.textContent ?? '' } : null; })()`);
    const squares = (await filesOf(panel)).includes('squares.txt') ? await readFile(panel, 'squares.txt') : await closeFiles(panel);
    check('a plain sh command runs in the wasm Linux (Python there too), its file kept', () => {
      assert.equal(ranWasm, 'idle', `state ${ranWasm}`);
      assert.ok(wasmCard && !wasmCard.failed, `the bash call: ${JSON.stringify(wasmCard)}`);
      assert.match(wasmCard.text, /^wasm\s+333332833333500000/, `uname and Python answered from the wasm Linux: ${JSON.stringify(wasmCard.text)}`);
      assert.match(squares, /^333332833333500000/, `squares.txt: ${JSON.stringify(squares)}`);
    });
  }

  // 7g. The thread's network: off, a command reaches nothing; switched on in its model
  // menu, its commands reach websites through Chrome (HTTPS opened by the Linux's own
  // proxy, fetched by Chrome), and addresses on this computer stay closed to them.
  step = 'network';
  const offline = 'curl -sS -m 20 https://example.com/ 2>&1 | head -2; echo "exit ${PIPESTATUS[0]}"';
  scripted.replies.push(call('bash', { command: offline }), text('It could not connect.'));
  await sendPrompt(panel, `Run exactly this command with bash, then reply with only its output: ${offline}`);
  await panel.until(`spindlePanel.threads.get(${JSON.stringify(linuxThread)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
  await settled(panel, linuxThread, 300_000);
  const offlineCard = await panel.eval(`(() => { const cards = [...document.querySelectorAll('#history .tool')].filter((c) => c.querySelector('.tool-name').textContent === 'bash'); return cards.at(-1)?.querySelector('pre.result')?.textContent ?? ''; })()`);
  await panel.click('#model-chip');
  await panel.click('#model-network');
  await panel.click('#model-apply');
  const networked = await panel.until(`spindlePanel.threads.get(${JSON.stringify(linuxThread)})?.network === true && document.querySelector('#model-chip').getAttribute('aria-label') + (document.querySelector('#model-chip .icon-globe') ? ' [globe]' : '')`, { what: "the thread's network to be on" }).catch((error) => error.message);
  const online = [
    "curl -sS https://example.com/ | grep -o '<title>[^<]*'",
    "python3 -c \"import urllib.request as u; r = u.urlopen('https://example.com/'); print('python', r.status)\"",
    'curl -sS http://192.168.1.1/',
  ].join(' && ');
  scripted.replies.push(call('bash', { command: online }), text('Example Domain, python 200, local closed.'));
  await sendPrompt(panel, `Run exactly this command with bash, then reply with only its output: ${online}`);
  await panel.until(`spindlePanel.threads.get(${JSON.stringify(linuxThread)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
  const onlineState = await settled(panel, linuxThread, 300_000);
  const onlineCard = await panel.eval(`(() => { const cards = [...document.querySelectorAll('#history .tool')].filter((c) => c.querySelector('.tool-name').textContent === 'bash'); const card = cards.at(-1); return card ? { failed: card.classList.contains('failed'), text: card.querySelector('pre.result')?.textContent ?? '' } : null; })()`);
  await panel.screenshot(join(shots, '03g-network.png'));
  check("a thread's network: off reaches nothing; on (its model menu), websites through Chrome over HTTPS; local addresses stay closed", () => {
    assert.match(offlineCard, /Could not connect|Failed to connect|exit [1-9]/, `without network: ${offlineCard}`);
    assert.match(String(networked), /web on \[globe\]$/, `the model button says (and shows) the thread reaches the web: ${networked}`);
    assert.equal(onlineState, 'idle', `state ${onlineState}`);
    assert.ok(onlineCard && !onlineCard.failed, `the networked call: ${JSON.stringify(onlineCard)}`);
    assert.match(onlineCard.text, /<title>Example Domain/, 'curl fetched an HTTPS page through Chrome');
    assert.match(onlineCard.text, /python 200/, "Python's urllib trusts the Linux's own authority");
    assert.match(onlineCard.text, /closed to it/, 'an address on this computer is refused');
  });

  // 7h. (scripted) Stop a running command, then say what to do: the command is kept as
  // stopped (with what it printed), never runs again, and a reply goes on with the turn,
  // the model reading the stopped result and then the reply.
  if (options.scripted) {
    step = 'stop a command, then reply';
    const long = 'echo started-long; sleep 120; echo never-printed';
    scripted.replies.push(call('bash', { command: long }), text('Skipped it, as you asked.'));
    const bashCards = `[...document.querySelectorAll('#history .tool')].filter((c) => c.querySelector('.tool-name').textContent === 'bash')`;
    const cardsBefore = await panel.eval(`${bashCards}.length`);
    await sendPrompt(panel, `Run exactly this command with bash: ${long}`);
    await panel.until(`${bashCards}.length > ${cardsBefore} && spindlePanel.threads.get(${JSON.stringify(linuxThread)})?.state === 'running' || null`, { timeout: 60_000, what: 'the command to start' });
    // The Linux is warm: the command prints at once, then sleeps.
    await sleep(8000);
    await panel.until(`document.querySelector('#send').classList.contains('stop') || null`, { what: 'Send to become Stop' });
    await panel.click('#send');
    const stoppedCommand = await settled(panel, linuxThread, 60_000);
    const seenAtStop = scripted.seen.length;
    const notice = await panel.until(`(() => { const n = document.querySelector('#notice'); return !n.hidden && /You stopped this step/.test(n.textContent) ? n.textContent : null; })()`, { timeout: 15_000, what: 'the stopped notice' }).catch((error) => error.message);
    const placeholder = await panel.eval(`document.querySelector('#prompt').placeholder`);
    const stopCard = await panel.eval(`(() => { const card = ${bashCards}.at(-1); return card ? { failed: card.classList.contains('failed'), text: card.querySelector('pre.result')?.textContent ?? '' } : null; })()`);
    await panel.screenshot(join(shots, '03h-command-stopped.png'));
    const reply = `Skip that one (${randomBytes(2).toString('hex')}) and say so.`;
    await sendPrompt(panel, reply);
    await panel.until(`spindlePanel.threads.get(${JSON.stringify(linuxThread)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
    const afterReply = await settled(panel, linuxThread, 120_000);
    const replied = await lastAnswer(panel);
    const ran = await panel.eval(`${bashCards}.filter((c) => (c.querySelector('pre.result')?.textContent ?? '').includes('started-long')).length`);
    await panel.screenshot(join(shots, '03h-reply-after-stop.png'));
    check('Stop a running command, then reply: it is kept as stopped with what it printed, never run again, and the turn goes on with the reply', () => {
      assert.equal(stoppedCommand, 'stopped', `after Stop: ${stoppedCommand}`);
      assert.match(String(notice), /You stopped this step/, `notice: ${notice}`);
      assert.match(String(notice), /Continue[\s\S]*Retry[\s\S]*Take back/, 'the notice names the other ways on');
      assert.equal(placeholder, 'Reply to go on…');
      assert.ok(stopCard?.failed, `the stopped call: ${JSON.stringify(stopCard)}`);
      assert.match(stopCard.text, /started-long/, 'what it printed before the stop is kept');
      assert.match(stopCard.text, /the user stopped this command/, stopCard.text);
      assert.ok(!stopCard.text.includes('never-printed'), stopCard.text);
      assert.equal(afterReply, 'idle', `after the reply: ${afterReply}`);
      assert.match(replied, /Skipped it/, `answer: ${replied}`);
      assert.equal(scripted.seen.length, seenAtStop + 1, 'the reply made one request, and nothing ran again');
      const body = scripted.seen.at(-1).body;
      const stoppedAt = body.indexOf('the user stopped this command');
      assert.ok(stoppedAt >= 0 && stoppedAt < body.indexOf('Skip that one'), 'the model read the stopped result, then the reply');
      assert.equal(ran, 1, 'one run of the command');
    });
  }

  // 2. A turn that finishes while no panel is open.
  step = 'panel closed';
  await panel.click('#home');
  // Scripted, the model answers only once the panel is gone.
  let panelGone;
  const gone = new Promise((done) => { panelGone = done; });
  scripted.replies.push({ after: gone, then: call('write_file', { path: 'notes.md', content: 'from chrome\n' }) }, text('DONE'));
  const seenBefore = scripted.seen.length;
  const second = await newThread(panel, 'Using your file tools, create notes.md containing exactly the line "from chrome". Then reply with only DONE.');
  await panel.until(`spindlePanel.threads.get(${JSON.stringify(second)})?.state === 'running'`, { what: 'the turn to start' });
  // A message written while it runs steers it.
  const steer = `Steering check ${randomBytes(2).toString('hex')}: carry on with the same task.`;
  await sendPrompt(panel, steer);
  const steered = await panel.until(`document.querySelector('#toast').textContent === 'Sent to the running turn.' && !document.querySelector('#toast').hidden`, { what: 'the steering message to be taken' }).catch((error) => error.message);
  await panel.close();
  panelGone();
  if (options.scripted) {
    for (let i = 0; i < 150 && scripted.seen.length < seenBefore + 2; i++) await sleep(200);
    await sleep(1000);
  } else {
    await sleep(45_000);
  }
  panel = await openPanel();
  const stateAtReopen = await state(panel, second);
  await panel.click(`#threads .thread-row[data-id="${second}"]`);
  await settled(panel, second);
  const files = await filesOf(panel);
  const notes = files.includes('notes.md') ? await readFile(panel, 'notes.md') : await closeFiles(panel);
  check('a message written while a turn runs steers it', () => {
    assert.equal(steered, true, steered);
    if (options.scripted) assert.ok(scripted.seen.slice(seenBefore).some((seen) => seen.body.includes(steer)), 'the steering message reached the model');
  });
  check('a turn started from the panel finishes while no panel is open', () => {
    assert.equal(stateAtReopen, 'idle', `state when a panel opened again: ${stateAtReopen}`);
    assert.ok(files.includes('notes.md'), `files ${files}`);
    assert.match(notes, /from chrome/);
  });

  // 3. Stop, Continue, Retry.
  step = 'stop';
  await panel.click('#home');
  scripted.replies.push({ hold: 'Once upon a time, a lighthouse keeper ' }, text('Retried: the story, told once.'));
  const third = await newThread(panel, 'Write a 600-word story about a lighthouse keeper.');
  await panel.until(`(document.querySelector('#history .live .message')?.innerText ?? '').length > 20`, { timeout: 120_000, what: 'the reply to start streaming' });
  // While a turn runs (and the box is empty), Send is Stop.
  await panel.until(`document.querySelector('#send').classList.contains('stop') || null`, { what: 'Send to become Stop' });
  await panel.click('#send');
  const stopped = await settled(panel, third, 60_000);
  for (const response of scripted.holding.splice(0)) response.destroy();
  await panel.screenshot(join(shots, '04-stopped.png'));
  await panel.click('#resume');
  await panel.until(`spindlePanel.threads.get(${JSON.stringify(third)})?.state !== 'stopped' || null`, { timeout: 10_000, what: 'Continue to answer' }).catch(() => {});
  const afterContinue = await settled(panel, third, 60_000);
  const continueNote = await panel.eval(`spindlePanel.threads.get(${JSON.stringify(third)})?.note ?? ''`);
  const requestsBeforeRetry = scripted.seen.length;
  await panel.click('#retry');
  await panel.until(`spindlePanel.threads.get(${JSON.stringify(third)})?.state === 'running' || null`, { timeout: 10_000 }).catch(() => {});
  const afterRetry = await settled(panel, third).catch(async (error) => {
    // What the panel shows when a retried turn does not end.
    await panel.screenshot(join(shots, '04b-retry-stuck.png')).catch(() => {});
    const seen = await panel.eval(`({ phase: document.querySelector('#phase')?.textContent, tools: [...document.querySelectorAll('#history .tool')].map((c) => c.dataset.tool + ':' + (c.querySelector('.tool-state')?.textContent ?? '')), tail: (document.querySelector('#history')?.innerText ?? '').slice(-600) })`).catch(() => ({}));
    console.log(`retry did not end: ${JSON.stringify(seen)}`);
    throw error;
  });
  const retryAnswer = await lastAnswer(panel);
  check('Stop ends a turn mid-reply; Continue refuses an unknown outcome; Retry sends it once', () => {
    assert.equal(stopped, 'stopped', `after Stop: ${stopped}`);
    assert.ok(['failed', 'stopped'].includes(afterContinue), `after Continue: ${afterContinue}`);
    assert.match(continueNote, /unknown|retry/i, `Continue said: ${continueNote}`);
    assert.equal(afterRetry, 'idle', `after Retry: ${afterRetry}`);
    if (options.scripted) assert.equal(scripted.seen.length, requestsBeforeRetry + 1, 'Retry sent one request');
    assert.ok(retryAnswer.length > 10, `answer: ${retryAnswer}`);
  });

  // 4. Chrome killed mid-turn.
  step = 'crash';
  await panel.click('#home');
  scripted.replies.push({ hold: '1\n2\n3\n' }, text('Counted, after the restart.'));
  const fourth = await newThread(panel, 'Count from 1 to 300, one number per line, nothing else.');
  await panel.until(`(document.querySelector('#history .live .message')?.innerText ?? '').length > 4`, { timeout: 120_000, what: 'the count to start streaming' });
  await browser.crash();
  for (const response of scripted.holding.splice(0)) response.destroy();
  page = await startChrome();
  panel = await openPanel();
  const afterCrash = await panel.eval('[...spindlePanel.threads.values()].map((t) => [t.id, t.state])');
  await panel.screenshot(join(shots, '05-after-restart.png'));
  await panel.click(`#threads .thread-row[data-id="${fourth}"]`);
  await panel.until(`spindlePanel.current === ${JSON.stringify(fourth)}`, { what: 'the cut thread to open' });
  await panel.click('#retry');
  await panel.until(`spindlePanel.threads.get(${JSON.stringify(fourth)})?.state === 'running' || null`, { timeout: 10_000 }).catch(() => {});
  const recovered = await settled(panel, fourth);
  await panel.click('#home');
  await panel.click(`#threads .thread-row[data-id="${first}"]`);
  await panel.until(`spindlePanel.current === ${JSON.stringify(first)} && document.querySelectorAll('#history .message').length > 1`, { what: 'the first thread to open' });
  const firstAgain = await lastAnswer(panel);
  check('after Chrome is killed mid-turn, every thread is there, the cut one says so, and Retry finishes it', () => {
    const states = new Map(afterCrash);
    for (const id of [off, first, linuxThread, second, third, fourth]) assert.ok(states.has(id), `thread ${id} missing after the restart`);
    assert.equal(states.get(fourth), 'interrupted', `the cut thread: ${states.get(fourth)}`);
    assert.equal(recovered, 'idle', `after Retry: ${recovered}`);
    assert.ok(firstAgain.includes(secret), 'the first thread kept its history');
  });

  // 5. Moving to a native host and back.
  if (native) {
    step = 'move';
    await panel.click('#home');
    await panel.click(`#threads .thread-row[data-id="${second}"]`);
    await panel.until(`spindlePanel.current === ${JSON.stringify(second)}`, { what: 'the notes thread to open' });
    await panel.click('#title-button');
    await panel.click('#menu-export');
    const outFile = await downloadBy(panel, '#confirm-yes');
    bundles.push(['chrome export', fs.readFileSync(outFile)]);
    const movedState = await panel.until(`spindlePanel.threads.get(${JSON.stringify(second)})?.state === 'moved' && 'moved'`, { what: 'the thread to be marked moved' });
    const composerOff = await panel.eval(`document.querySelector('#prompt').disabled`);
    // The moved copy keeps the file, to save again.
    await panel.click('#home');
    await panel.click(`#threads .thread-row[data-id="${second}"]`);
    const again = fs.readFileSync(await downloadBy(panel, '#history .notice a[download]'));

    // The native host: its own credentials (a home of its own when scripted).
    const env = { ...process.env };
    if (options.scripted) {
      const home = join(work, 'home');
      fs.mkdirSync(join(home, '.dext'), { recursive: true, mode: 0o700 });
      fs.copyFileSync(catalogFile, join(home, '.dext', 'providers.json'));
      fs.writeFileSync(join(home, '.dext', 'auth.json'), JSON.stringify({ version: 1, providers: { scripted: { type: 'api_key', key } } }), { mode: 0o600 });
      Object.assign(env, { HOME: home, DEXT_HOME: join(home, '.dext') });
      scripted.replies.push(call('write_file', { path: 'notes.md', content: 'from chrome\nnative\n' }), text('DONE'));
    }
    const nativeDir = join(work, 'native');
    // Not execFileSync: in --scripted mode this process is the provider and must answer.
    const spindle = async (...rest) => (await promisify(execFile)(native, ['--dir', nativeDir, ...rest], { encoding: 'utf8', env })).stdout;
    const leg = {};
    try {
      leg.imported = await spindle('import-session', outFile);
      // What the moved thread offers natively: not Chrome's page tools.
      const host = new StdioClient({ binary: native, env, stderr: 'ignore' });
      try {
        leg.settings = await host.settings(nativeDir);
        leg.tools = (leg.settings.tools ?? []).map((tool) => tool.name);
      } finally {
        await host.close();
      }
      leg.turn = await spindle('say', '--max-usd', '5', 'Using your file tools, append a line "native" to notes.md, then reply with only DONE.');
      leg.notes = await spindle('cat', 'notes.md');
      leg.back = join(work, 'back.spindle');
      await spindle('export-session', leg.back);
      bundles.push(['native export', fs.readFileSync(leg.back)]);
    } catch (error) {
      leg.error = `${error.message}\n${error.stderr ?? ''}`;
    }
    check('a thread moves to a file; a native host continues it with a turn of its own', () => {
      assert.equal(movedState, 'moved');
      assert.equal(composerOff, true, 'the moved copy takes no more input');
      assert.ok(again.equals(fs.readFileSync(outFile)), 'the file saved again is the same bundle');
      assert.ok(!leg.error, leg.error);
      assert.ok(!leg.tools.includes('read_page') && !leg.tools.includes('list_tabs'), `natively offered: ${leg.tools}`);
      assert.equal(leg.settings.builtin_tools, 'all', "native's full tool set (code mode too)");
      assert.equal(leg.settings.runner ?? 'none', 'none', "commands in the host's own sandbox");
      assert.ok(!String(leg.settings.instructions ?? '').includes("inside the user's browser"), 'no browser Linux guide');
      assert.match(leg.notes, /from chrome[\s\S]*native/);
    });
    if (!leg.error) {
      await panel.click('#home');
      await panel.files('#import-file', [leg.back]);
      // What changes for it to run here is asked about before it opens.
      const asked = await panel.until(`document.querySelector('#confirm-dialog').open && document.querySelector('#confirm-text').textContent`, { what: 'the import to ask about its changes' }).catch((error) => error.message);
      await panel.click('#confirm-yes');
      const known = JSON.stringify([off, first, linuxThread, second, third, fourth]);
      const imported = await panel.until(`spindlePanel.current && !${known}.includes(spindlePanel.current) && spindlePanel.current`, { what: 'the import to open' });
      await panel.until(`spindlePanel.status?.settings`, { what: 'the imported thread to open' });
      const importedTools = await panel.eval(`spindlePanel.status.settings.tools.map((tool) => tool.name)`);
      const importedBudget = await panel.eval(`spindlePanel.status.settings.max_usd`);
      const importedRuns = await panel.eval(`[spindlePanel.status.settings.builtin_tools, spindlePanel.status.settings.runner]`);
      const importedNote = await panel.eval(`document.querySelector('#notice').hidden ? '' : document.querySelector('#notice').textContent`);
      scripted.replies.push(call('read_file', { path: 'notes.md' }), text('native'));
      await sendPrompt(panel, 'Read notes.md with your file tools and reply with only its last line.');
      await panel.until(`spindlePanel.threads.get(${JSON.stringify(imported)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
      const goes = await settled(panel, imported);
      const backAnswer = await lastAnswer(panel);
      await panel.screenshot(join(shots, '06-back-from-native.png'));
      check('moved back from the native host, the import asks first, then the thread gets the page tools, keeps its budget (and says so) and goes on in Chrome', () => {
        assert.ok(importedTools.includes('read_page') && importedTools.includes('list_tabs'), `offered here: ${importedTools}`);
        assert.equal(importedBudget, 5, 'the budget carried over');
        assert.match(String(asked), /To run in this browser, .* changes: .*its commands run in this browser's Linux.*Nothing has run yet/, `the import asked first: ${asked}`);
        assert.deepEqual(importedRuns, ['files', 'app'], "file tools, bash in the browser Linux");
        assert.match(importedNote, /its US\$5 spending limit kept/, `the thread said: ${importedNote}`);
        assert.equal(goes, 'idle', `state ${goes}`);
        assert.match(backAnswer, /native/i);
      });
    }
  }

  // 6. A copy, deleting, and keys in bundles.
  step = 'copy';
  await panel.click('#home');
  await panel.click(`#threads .thread-row[data-id="${first}"]`);
  await panel.until(`spindlePanel.current === ${JSON.stringify(first)}`, { what: 'the first thread to open' });
  await panel.click('#title-button');
  const copyFile = await downloadBy(panel, '#menu-copy');
  bundles.push(['chrome copy', fs.readFileSync(copyFile)]);
  const afterCopy = await state(panel, first);
  check('a copy exports to a file and the thread stays in Chrome', () => {
    assert.equal(afterCopy, 'idle', `the thread after a copy: ${afterCopy}`);
    assert.ok(fs.statSync(copyFile).size > 0);
  });

  // 7d. A site the user allows in Settings: a script reaches it as allowed (reading,
  // not changing), and TypeScript runs.
  step = 'sites';
  await panel.click('#home');
  await panel.click('#settings');
  await unfold(panel, 'commands', 'sites');
  await panel.type('#site-url', `http://127.0.0.1:${port}/api/`);
  await panel.click('#site-form button.primary');
  const siteKept = await panel.until(`spindlePanel.engine.sites?.length === 1 && spindlePanel.engine.sites[0]`, { what: 'the site to be kept' });
  const siteRow = await panel.eval(`document.querySelector('#sites').textContent`);
  await panel.click('#settings-dialog [data-close]');
  const site = `http://127.0.0.1:${port}/api/notes.json`;
  const siteScript = `spindle http ${site} && (spindle http -X POST -d x ${site} || echo "post refused") && printf 'const n: number = 6 * 7;\\nconsole.log("ts", n);\\n' > t.ts && node t.ts`;
  // Node in the emulated Linux is slow on a small CI runner: the command says how long
  // it may take, as a model does for a slow one.
  scripted.replies.push(call('bash', { command: siteScript, timeout: 180 }), text('Read the site; POST refused; ts 42.'));
  await panel.click(`#threads .thread-row[data-id="${linuxThread}"]`);
  await panel.until(`spindlePanel.current === ${JSON.stringify(linuxThread)} && spindlePanel.status`, { what: 'the Linux thread to open' });
  await sendPrompt(panel, `Run exactly this command with bash, then reply with only its output: ${siteScript}`);
  await panel.until(`spindlePanel.threads.get(${JSON.stringify(linuxThread)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
  const sited = await settled(panel, linuxThread, Number(process.env.SPINDLE_STEP_WAIT ?? 300_000));
  const siteCard = await panel.eval(`(() => { const cards = [...document.querySelectorAll('#history .tool')].filter((c) => c.querySelector('.tool-name').textContent === 'bash'); const card = cards.at(-1); return card ? { failed: card.classList.contains('failed'), text: card.querySelector('pre.result')?.textContent ?? '' } : null; })()`);
  check('a script reaches a site the user allowed in Settings, only as allowed; TypeScript runs', () => {
    assert.equal(siteKept.risk, 'read', JSON.stringify(siteKept));
    assert.match(siteRow, /127\.0\.0\.1/, `the Settings list: ${siteRow}`);
    assert.equal(sited, 'idle', `state ${sited}`);
    assert.ok(siteCard && !siteCard.failed, `the bash call: ${JSON.stringify(siteCard)}`);
    assert.match(siteCard.text, /from the site/, 'the site answered the script');
    assert.match(siteCard.text, /does not allow POST[\s\S]*post refused/, 'a change the grant does not allow is refused');
    assert.match(siteCard.text, /ts 42/, 'TypeScript ran');
    assert.ok(siteVisits.length >= 1 && siteVisits.every((visit) => visit.method === 'GET'), `the site saw: ${JSON.stringify(siteVisits)}`);
    assert.ok(siteVisits.every((visit) => visit.cookie === null), 'no cookies went with it');
  });

  // 7e. A site with a secret header: Spindle adds it to each request a script makes
  // there, the script never sees it (an echo of it comes back redacted), a script may
  // not set it itself, nothing the extension keeps holds it as text, and removing the
  // site removes it.
  step = 'secret header';
  const siteToken = `sk-site-${randomBytes(16).toString('hex')}`;
  await panel.click('#home');
  await panel.click('#settings');
  await unfold(panel, 'commands', 'sites');
  await panel.type('#site-url', `http://127.0.0.1:${port}/keyed/`);
  await panel.type('#site-header', 'Authorization');
  await panel.type('#site-secret', `Bearer ${siteToken}`);
  await panel.click('#site-form button.primary');
  const keyedSite = await panel.until(`spindlePanel.engine.sites?.find((grant) => grant.url.endsWith('/keyed/')) || null`, { what: 'the keyed site to be kept' });
  await panel.until(`(spindlePanel.engine.vault?.sites ?? []).includes(${JSON.stringify(keyedSite.name)}) || null`, { what: 'its secret to be sealed' });
  const keyedRow = await panel.eval(`[...document.querySelectorAll('#sites .site')].map((row) => row.textContent).join(' | ')`);
  const keyedCleared = await panel.eval(`['site-url', 'site-header', 'site-secret'].every((id) => document.getElementById(id).value === '')`);
  const stored = () => panel.eval(`new Promise((done, failed) => {
    const opening = indexedDB.open('spindle');
    opening.onerror = () => failed(opening.error);
    opening.onsuccess = () => {
      const tx = opening.result.transaction(['kv', 'keys'], 'readonly');
      const sites = tx.objectStore('kv').get('sites');
      const secrets = tx.objectStore('kv').get('site-secrets');
      const names = tx.objectStore('keys').getAllKeys();
      const values = tx.objectStore('keys').getAll();
      tx.oncomplete = () => done({ sites: JSON.stringify(sites.result ?? null), secrets: secrets.result ?? null, keys: names.result.map((name, i) => ({ name: String(name),
        kind: values.result[i]?.kind ?? null, text: typeof values.result[i] === 'string' ? values.result[i] : null,
        sealed: values.result[i]?.data instanceof ArrayBuffer && values.result[i]?.iv?.length === 12 })) });
      tx.onerror = () => failed(tx.error);
    };
  })`);
  const keptKeyed = await stored();
  await panel.click('#settings-dialog [data-close]');
  const keyedUrl = `http://127.0.0.1:${port}/keyed/echo`;
  const keyedScript = `spindle http ${keyedUrl} && (spindle http -H 'Authorization: Bearer mine' ${keyedUrl} || echo "own refused")`;
  scripted.replies.push(call('bash', { command: keyedScript, timeout: 180 }), text('Echoed; own header refused.'));
  await panel.click(`#threads .thread-row[data-id="${linuxThread}"]`);
  await panel.until(`spindlePanel.current === ${JSON.stringify(linuxThread)} && spindlePanel.status`, { what: 'the Linux thread to open' });
  await sendPrompt(panel, `Run exactly this command with bash, then reply with only its output: ${keyedScript}`);
  await panel.until(`spindlePanel.threads.get(${JSON.stringify(linuxThread)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
  const keyedRan = await settled(panel, linuxThread, Number(process.env.SPINDLE_STEP_WAIT ?? 300_000));
  const keyedCard = await panel.eval(`(() => { const cards = [...document.querySelectorAll('#history .tool')].filter((c) => c.querySelector('.tool-name').textContent === 'bash'); const card = cards.at(-1); return card ? { failed: card.classList.contains('failed'), text: card.querySelector('pre.result')?.textContent ?? '' } : null; })()`);
  const keyedHistory = await panel.eval(`document.querySelector('#history').textContent`);
  // The thread as a file: what it keeps (its journal of the script's calls among it).
  await panel.click('#title-button');
  const keyedBundle = fs.readFileSync(await downloadBy(panel, '#menu-copy'));
  const keyedForms = [siteToken, Buffer.from(siteToken).toString('base64').slice(0, 40)];
  check('a site\'s secret header: added to a script\'s request, never shown to it, kept as text or in the thread; a script may not set it', () => {
    assert.deepEqual(keyedSite.secret, ['Authorization'], JSON.stringify(keyedSite));
    assert.match(keyedRow, /Authorization/, `the Settings list: ${keyedRow}`);
    assert.ok(keyedCleared, 'the form forgets what was typed');
    assert.ok(!keptKeyed.sites.includes(siteToken), `kv/sites: ${keptKeyed.sites}`);
    // The list stays as builds before secret headers read it; the header's name is kept
    // beside it, its value only sealed.
    assert.ok(!keptKeyed.sites.includes('"secret"'), `kv/sites: ${keptKeyed.sites}`);
    assert.deepEqual(keptKeyed.secrets, { [keyedSite.name]: { url: keyedSite.url, names: ['Authorization'] } });
    const record = keptKeyed.keys.find((kept) => kept.name === `:site:${keyedSite.name}`);
    assert.ok(record?.sealed && record.text === null && record.kind === 'site', JSON.stringify(keptKeyed.keys));
    assert.equal(keyedRan, 'idle', `state ${keyedRan}`);
    assert.ok(keyedCard && !keyedCard.failed, `the bash call: ${JSON.stringify(keyedCard)}`);
    assert.equal(keyedVisits.length, 1, `the keyed site saw: ${JSON.stringify(keyedVisits)}`);
    assert.equal(keyedVisits[0].authorization, `Bearer ${siteToken}`, 'Spindle added the header');
    assert.equal(keyedVisits[0].cookie, null, 'no cookies went with it');
    assert.match(keyedCard.text, /\[redacted\]/, keyedCard.text);
    assert.match(keyedCard.text, /supplied by grant[\s\S]*own refused/, 'a script may not set it');
    // The copy holds the journaled reply (redacted), so a token in it would be found.
    assert.ok(keyedBundle.includes('[redacted]'), 'the thread\'s file holds the script\'s journaled reply');
    for (const form of keyedForms) {
      assert.ok(!keyedCard.text.includes(form), `the script saw ${form}: ${keyedCard.text}`);
      assert.ok(!keyedHistory.includes(form), 'nor does the thread show it');
      assert.ok(!keyedBundle.includes(form), 'nor does the thread keep it');
    }
  });
  // Removing the site removes its secret.
  await panel.click('#home');
  await panel.click('#settings');
  await unfold(panel, 'commands', 'sites');
  await panel.eval(`[...document.querySelectorAll('#sites .site')].find((row) => row.textContent.includes('/keyed/')).querySelector('button').click()`);
  await panel.until(`!spindlePanel.engine.sites?.some((grant) => grant.url.endsWith('/keyed/')) || null`, { what: 'the keyed site to go' });
  const afterKeyed = await stored();
  await panel.click('#settings-dialog [data-close]');
  check('removing a site removes its secret header', () => {
    assert.ok(!afterKeyed.keys.some((kept) => kept.name.startsWith(':site:')), JSON.stringify(afterKeyed.keys));
    assert.deepEqual(afterKeyed.secrets, {});
  });

  // 7f. Files in and out: a file attached to a message lands in the workspace, bash uses
  // it, and what it made downloads, alone or with the rest as a .zip.
  step = 'files in and out';
  const csv = join(work, 'data.csv');
  fs.writeFileSync(csv, 'a,b\n1,2\n');
  // And a little web app (a page, its script, its data) to open in a tab.
  const app = ['site.html', 'app.js', 'data.json'].map((name) => join(work, name));
  fs.writeFileSync(app[0], '<!doctype html><title>Made here</title><p id="x">static</p><script src="app.js"></script>');
  fs.writeFileSync(app[1], "fetch('data.json').then((r) => r.json()).then((d) => { document.querySelector('#x').textContent = d.word; console.log('spindle-test data ' + d.word); });");
  fs.writeFileSync(app[2], '{"word":"rendered"}');
  await panel.click(`#threads .thread-row[data-id="${linuxThread}"]`).catch(async () => {
    await panel.click('#home');
    await panel.click(`#threads .thread-row[data-id="${linuxThread}"]`);
  });
  await panel.until(`spindlePanel.current === ${JSON.stringify(linuxThread)} && spindlePanel.status`, { what: 'the Linux thread to open' });
  await panel.files('#attach-file', [csv, ...app]);
  const chips = await panel.until(`document.querySelector('#attached').textContent || null`, { what: 'the attachment to show' });
  scripted.replies.push(call('bash', { command: 'wc -l < data.csv > count.txt && cat count.txt' }), text('2'));
  await sendPrompt(panel, 'Use bash to run exactly: wc -l < data.csv > count.txt && cat count.txt. Then reply with only its output.');
  await panel.until(`spindlePanel.threads.get(${JSON.stringify(linuxThread)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
  const filed = await settled(panel, linuxThread, Number(process.env.SPINDLE_STEP_WAIT ?? 300_000));
  const filedNames = await filesOf(panel);
  const one = filedNames.includes('count.txt') ? fs.readFileSync(await downloadBy(panel, '#file-list .file-get[aria-label="Download count.txt"]', /^count\.txt$/)) : null;
  const all = filedNames.length ? fs.readFileSync(await downloadBy(panel, '#files-zip', /\.zip$/)) : null;
  const openLink = await panel.eval(`Boolean(document.querySelector('#file-list .file-get[aria-label="Open site.html in a tab"]'))`);
  await closeFiles(panel);
  // The page opened in a tab runs as it shows, contained: its script fetches its data
  // from the workspace (it says so on the console: a preview reaches nothing outside the
  // workspace), and nothing is said about it.
  const viewer = await open(browser, `chrome-extension://${extensionId}/view.html?thread=${linuxThread}&path=site.html`, { newWindow: false });
  await hearFrames(viewer);
  const tabSaid = () => frameSaid.filter((said) => said.page === viewer.sessionId).map((said) => said.text);
  const shown = await (async () => {
    for (const until = Date.now() + 45_000; Date.now() < until; await sleep(250)) if (tabSaid().includes('spindle-test data rendered')) return true;
    return false;
  })();
  const quietTab = await viewer.eval(`document.querySelector('#view-note').hidden`);
  await browser.send('Target.closeTarget', { targetId: viewer.targetId }).catch(() => {});
  check('a file attached to a message lands in the workspace; what the thread made downloads, alone or as a .zip, and a page of it opens in a tab', () => {
    assert.match(chips, /data\.csv/);
    assert.equal(filed, 'idle', `state ${filed}`);
    assert.ok(filedNames.includes('data.csv') && filedNames.includes('count.txt'), `files: ${filedNames}`);
    assert.equal(one?.toString().trim(), '2', 'count.txt as the thread wrote it');
    assert.ok(all && all.subarray(0, 2).toString() === 'PK' && all.includes(Buffer.from('count.txt')) && all.includes(Buffer.from('a,b\n1,2\n')), 'the .zip holds the workspace');
    assert.ok(openLink, 'a page in the workspace offers to open in a tab');
    assert.ok(shown, `the page opened in a tab ran its script with its data, unasked (it said: ${JSON.stringify(tabSaid())})`);
    assert.ok(quietTab, 'nothing is said about a page that runs contained');
  });

  // 7f2. What a thread makes shows in the conversation: a report it writes gets a card
  // with the page in a preview that reaches nothing outside the workspace (its script
  // runs and its fetch out is refused; what it loads from the web waits for the user's
  // yes), Download saves it, navigate to
  // /workspace/NAME opens it in a tab for the user; a file its command makes is in the
  // one workspace card (marked), which sits just above the turn's final answer.
  step = 'files in the conversation';
  const report = `<!doctype html><title>Report</title><style>h1{color:#3f5b2f}</style><h1>Report</h1><script src="https://cdn.example.invalid/chart.js"></script><script>console.log('spindle-test ran'); fetch('http://127.0.0.1:${port}/rendered?word=leak').then(() => console.log('spindle-test reached'), () => console.log('spindle-test closed'))</script>`;
  const notesCommand = "printf '# Notes\\n\\n- first point\\n' > notes.md";
  scripted.replies.push(
    call('write_file', { path: '/workspace/report.html', content: report }),
    call('navigate', { url: '/workspace/report.html' }),
    call('bash', { command: notesCommand }),
    text('DONE'),
  );
  const reportTabs = async () => (await browser.send('Target.getTargets')).targetInfos.filter((t) => t.url.includes('view.html') && t.url.includes('report.html'));
  const tabsBefore = (await reportTabs()).length;
  const heardBefore = frameSaid.length;
  await sendPrompt(panel, `Do these three steps in order. 1) Use write_file with path /workspace/report.html and exactly this content: ${report} 2) Use navigate with url /workspace/report.html to open it for me. 3) Use bash to run exactly: ${notesCommand}. Then reply with only DONE.`);
  await panel.until(`spindlePanel.threads.get(${JSON.stringify(linuxThread)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
  const conversed = await settled(panel, linuxThread, Number(process.env.SPINDLE_STEP_WAIT ?? 300_000));
  const reportCard = await panel.until(`document.querySelector('#history .file-card[data-path="report.html"] .file-frame') && true`, { what: 'the report card with its preview' }).catch((error) => error.message);
  const cardSaid = () => frameSaid.slice(heardBefore).filter((said) => said.page === panel.sessionId).map((said) => said.text);
  const said = async (word, wait = 20_000) => {
    for (const until = Date.now() + wait; Date.now() < until; await sleep(250)) if (cardSaid().includes(`spindle-test ${word}`)) return true;
    return false;
  };
  // It runs, shut off from the network; what it would load from the web waits for the user.
  const ran = await said('ran');
  const closedOut = await said('closed');
  const webNote = await panel.until(`document.querySelector('#history .file-card[data-path="report.html"] .file-web')?.textContent`, { what: 'the note of what the page would load from the web' }).catch((error) => error.message);
  await panel.click('#history .file-card[data-path="report.html"] .file-web button');
  const webAllowed = await panel.until(`(() => { const note = document.querySelector('#history .file-card[data-path="report.html"] .file-web'); return Boolean(note && /^It loads from /.test(note.textContent) && document.querySelector('#history .file-card[data-path="report.html"] .file-frame')); })()`, { what: 'the preview again, open to the web' }).catch((error) => error.message);
  // What its commands made is in the workspace card (no list of its own), and that card
  // sits just above the turn's final answer.
  const notesListed = await panel.until(`document.querySelector('#history .workspace-card .file-row[data-path="notes.md"]') && true`, { what: 'the command\'s notes.md in the workspace card' }).catch((error) => error.message);
  const commandLists = await panel.eval(`document.querySelectorAll('#history .file-group').length`);
  const cardAboveAnswer = await panel.eval(`(() => { const card = document.querySelector('#history .workspace-card'); const answers = [...document.querySelectorAll('#history article.message-wrap.assistant')]; return Boolean(card) && card.nextElementSibling === answers.at(-1); })()`);
  const cardFolded = await panel.eval(`!document.querySelector('#history .workspace-card').open`);
  // Its summary counts the files once they are listed.
  const workspaceListed = await panel.until(`(() => { const text = document.querySelector('#history .workspace-card summary')?.textContent ?? ''; return /\\d+ files?/.test(text) && text; })()`, { what: 'the workspace card' }).catch((error) => error.message);
  const tabsAfter = await reportTabs();
  for (const tab of tabsAfter) await browser.send('Target.closeTarget', { targetId: tab.targetId }).catch(() => {});
  const saved = fs.readFileSync(await downloadBy(panel, '#history .file-card[data-path="report.html"] button[title="Download report.html"]', /^report\.html$/)).toString();
  await panel.screenshot(join(shots, '03h-files-in-conversation.png'));
  check('what a thread makes shows in the conversation: a report (written to /workspace/report.html) as a page, its script running shut off from the network, the web on a click, Download, opened in a tab by navigate; what its commands made in the one workspace card, folded, just above the final answer', () => {
    assert.equal(conversed, 'idle', `state ${conversed}`);
    assert.equal(reportCard, true, `the report's card shows its page: ${reportCard}`);
    assert.ok(ran, `the page's script ran in the card, unasked (its frames said: ${JSON.stringify(cardSaid())})`);
    assert.ok(closedOut, `its fetch to an outside address was refused (its frames said: ${JSON.stringify(cardSaid())})`);
    assert.ok(!rendered.includes('leak'), 'no preview reached this test\'s server');
    assert.match(String(webNote), /^It would load from .*\bcdn\.example\.invalid\b.*; previews stay offline\. Load from the web$/, `what it would load from the web is said: ${webNote}`);
    assert.equal(webAllowed, true, `Load from the web shows it again, open to the web: ${webAllowed}`);
    assert.equal(tabsAfter.length, tabsBefore + 1, 'navigate to /workspace/report.html opened it in a tab');
    assert.equal(notesListed, true, `the command's notes.md is in the workspace card: ${notesListed}`);
    assert.equal(commandLists, 0, 'no separate list of what the commands made');
    assert.equal(cardAboveAnswer, true, 'the workspace card is just above the final answer');
    assert.equal(cardFolded, true, 'the workspace card stays folded until opened');
    assert.match(String(workspaceListed), /Workspace · \d+ files/, `the workspace card: ${workspaceListed}`);
    assert.equal(saved.trim(), report.trim(), 'Download saves the report as written');
  });

  // 7f3. The model menu: the effort shows and changes; a turn the provider refused is
  // started over (a first turn: afresh) with another effort, its message sent once; and
  // a later refused turn is taken back, its message returned to the box to send again.
  if (options.scripted) {
    step = 'model menu';
    await panel.click('#home');
    scripted.replies.push({ refuse: 401 });
    const refusedThread = await newThread(panel, 'Say ready.');
    const refusedState = await settled(panel, refusedThread);
    const haltedButtons = await panel.until(`!document.querySelector('#takeback').hidden && !document.querySelector('#retry').hidden && true`, { what: 'Take back and Retry' }).catch((error) => error.message);
    await panel.click('#model-chip');
    const haltedSheet = await panel.until(`!document.querySelector('#model-halted').hidden && document.querySelector('#model-apply').textContent`, { what: 'the model sheet for a halted turn' }).catch((error) => error.message);
    await panel.until(`document.querySelectorAll('#model-effort [role="radio"]').length > 1 || null`, { what: 'the efforts of the model' });
    const efforts = await panel.eval(`[...document.querySelectorAll('#model-effort [role="radio"]')].map((b) => b.textContent)`);
    await panel.click('#model-effort [data-effort="high"]');
    const pickedEffort = await panel.eval(`({ name: document.querySelector('#model-effort [aria-checked="true"]')?.textContent, note: document.querySelector('#effort-note').textContent })`);
    await panel.screenshot(join(shots, '03i-model-halted.png'));
    scripted.replies.push(text('ready'));
    const seenBefore = scripted.seen.length;
    await panel.click('#model-apply');
    await panel.until(`spindlePanel.threads.get(${JSON.stringify(refusedThread)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
    const restarted = await settled(panel, refusedThread);
    const restartedAnswer = await lastAnswer(panel);
    const asked = await panel.eval(`[...document.querySelectorAll('#history .message.user')].map((m) => m.innerText.trim())`);
    const chip = await panel.eval(`document.querySelector('#model-chip').textContent`);
    const sentEffort = scripted.seen.slice(seenBefore).map((seen) => JSON.parse(seen.body).reasoning_effort ?? null);
    const threadEffort = await panel.eval(`spindlePanel.threads.get(${JSON.stringify(refusedThread)})?.effort`);
    // A later turn refused: taken back, its message back in the box.
    scripted.replies.push({ refuse: 401 });
    await sendPrompt(panel, 'Say again.');
    await panel.until(`spindlePanel.threads.get(${JSON.stringify(refusedThread)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
    const refusedAgain = await settled(panel, refusedThread);
    await panel.click('#takeback');
    const returned = await panel.until(`document.querySelector('#prompt').value || null`, { what: 'the message back in the box' }).catch((error) => error.message);
    const afterTakeBack = await panel.until(`(() => { const t = spindlePanel.threads.get(${JSON.stringify(refusedThread)}); return t?.state === 'idle' && document.querySelector('#takeback').hidden && t.state; })()`, { what: 'the thread ready again' }).catch((error) => error.message);
    scripted.replies.push(text('again'));
    await panel.click('#send');
    await panel.until(`spindlePanel.threads.get(${JSON.stringify(refusedThread)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
    const sentAgain = await settled(panel, refusedThread);
    const finalAsked = await panel.eval(`[...document.querySelectorAll('#history .message.user')].map((m) => m.innerText.trim())`);
    check('the model menu: efforts to choose; a refused first turn starts over with another effort, its message sent once; a refused later turn is taken back to send again', () => {
      assert.equal(refusedState, 'failed', `the refused turn: ${refusedState}`);
      assert.equal(haltedButtons, true, `Take back and Retry show: ${haltedButtons}`);
      assert.equal(haltedSheet, 'Start the turn over', `the sheet says what a change does: ${haltedSheet}`);
      // The levels the model takes, without "Default" (none chosen is the model's own).
      assert.ok(!efforts.includes('Default') && efforts.includes('Low') && efforts.includes('High'), `efforts offered: ${efforts}`);
      assert.equal(pickedEffort.name, 'High', `picked: ${JSON.stringify(pickedEffort)}`);
      assert.match(pickedEffort.note, /effort high/i, `what it does is said: ${pickedEffort.note}`);
      assert.equal(restarted, 'idle', `started over: ${restarted}`);
      assert.match(restartedAnswer, /ready/, `the answer: ${restartedAnswer}`);
      assert.deepEqual(asked, ['Say ready.'], `the message is there once: ${JSON.stringify(asked)}`);
      assert.deepEqual(sentEffort, ['high'], `the request asked for high effort: ${JSON.stringify(sentEffort)}`);
      assert.equal(threadEffort, 'high');
      assert.match(chip, / · high$/, `the chip says the effort: ${chip}`);
      assert.equal(refusedAgain, 'failed');
      assert.equal(returned, 'Say again.', `the message came back: ${returned}`);
      assert.equal(afterTakeBack, 'idle', `the thread is ready again: ${afterTakeBack}`);
      assert.equal(sentAgain, 'idle');
      assert.deepEqual(finalAsked, ['Say ready.', 'Say again.'], `each message once: ${JSON.stringify(finalAsked)}`);
    });

    // 7f3b. A long thread: a turn's end draws only that turn again; going back to before
    // one of the user's messages takes the conversation and the workspace back and returns
    // the message to the box; a history over its model's window is compacted with a
    // summary the model writes, the panel says so and shows the earlier messages when
    // asked; and a new thread starts with another's files, not its conversation.
    step = 'long thread';
    const pickModel = async (model) => {
      await panel.click('#model-chip');
      await panel.click('#model-name .picker-field');
      await panel.type('#model-name .picker-filter', model);
      await panel.click(`#model-name [role="option"][data-value="${model}"]`);
      await panel.click('#model-apply');
    };
    await panel.click('#home');
    await pickModel('scripted-small');
    scripted.replies.push(call('bash', { command: 'echo one > notes.txt' }), text('Made notes.txt.'));
    const longThread = await newThread(panel, 'Start the notes.');
    await settled(panel, longThread);
    await panel.until(`document.querySelector('#history [data-at="0"]') && true`, { what: 'the first message drawn' });
    await panel.eval(`document.querySelector('#history [data-at="0"]').spindleMark = 'kept'`);
    scripted.replies.push(call('bash', { command: 'echo two >> notes.txt; echo extra > extra.txt' }), text('Added.'));
    await sendPrompt(panel, 'Add more notes.');
    await panel.until(`spindlePanel.threads.get(${JSON.stringify(longThread)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
    await settled(panel, longThread);
    await lastAnswer(panel);
    const keptNode = await panel.until(`document.querySelectorAll('#history .message.user').length === 2 && (document.querySelector('#history [data-at="0"]')?.spindleMark ?? 'drawn again')`, { what: 'the second turn drawn' }).catch((error) => error.message);
    const secondAt = await panel.eval(`[...document.querySelectorAll('#history .message-wrap.user')].at(-1)?.dataset.at`);
    await panel.eval(`document.querySelector('#history [data-at="${secondAt}"] .rewind').click()`);
    await panel.until(`document.querySelector('#confirm-dialog').open || null`, { what: 'the confirmation' });
    await panel.click('#confirm-yes');
    const backInBox = await panel.until(`document.querySelector('#prompt').value || null`, { what: 'the message back in the box' }).catch((error) => error.message);
    const messagesAfterRewind = await panel.eval(`[...document.querySelectorAll('#history .message.user')].map((m) => m.innerText.trim())`);
    const filesAfterRewind = await filesOf(panel);
    const notesAfterRewind = filesAfterRewind.includes('notes.txt') ? await readFile(panel, 'notes.txt') : '(none)';
    await closeFiles(panel);
    await panel.eval(`(() => { const box = document.querySelector('#prompt'); box.value = ''; box.dispatchEvent(new Event('input')); })()`);
    // Three long answers take the history over the small model's window: the next
    // message compacts it first, with a summary the model is asked for.
    const longAnswer = (tag) => `${tag} ${'a long note about the field trip. '.repeat(1400)}`;
    for (const part of ['one', 'two', 'three']) {
      scripted.replies.push(text(longAnswer(part)));
      await sendPrompt(panel, `Write part ${part}.`);
      await panel.until(`spindlePanel.threads.get(${JSON.stringify(longThread)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
      await settled(panel, longThread);
    }
    const seenBeforeCompaction = scripted.seen.length;
    scripted.replies.push(text('SUMMARY: the user is writing notes about a field trip; notes.txt holds them.'), text('Noted part four.'));
    await sendPrompt(panel, 'Write part four.');
    await panel.until(`spindlePanel.threads.get(${JSON.stringify(longThread)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
    const compactedState = await settled(panel, longThread);
    const summaryAsked = scripted.seen.slice(seenBeforeCompaction).map((seen) => /Summarize the session history/.test(seen.body));
    const compactedNotice = await panel.until(`document.querySelector('#history .notice.compacted .summary-text')?.textContent || null`, { what: 'the compaction notice' }).catch((error) => error.message);
    await panel.eval(`document.querySelector('#history .notice.compacted button')?.click()`);
    const earlierShown = await panel.until(`(() => { const section = document.querySelector('#history .earlier'); return section ? [...section.querySelectorAll('.message.user')].map((m) => m.innerText.slice(0, 20)) : null; })()`, { what: 'the earlier messages' }).catch((error) => error.message);
    await panel.screenshot(join(shots, '03j-compacted.png'));
    // A new thread with these files.
    await panel.click('#title-button');
    await panel.click('#menu-fresh');
    const carried = await panel.until(`!document.querySelector('#carry').hidden && document.querySelector('#carry-text').textContent`, { what: 'the files to start with' }).catch((error) => error.message);
    scripted.replies.push(text('Fresh start.'));
    const freshThread = await newThread(panel, 'Carry on from the notes.');
    await settled(panel, freshThread);
    const freshFiles = await filesOf(panel);
    const freshNotes = freshFiles.includes('notes.txt') ? await readFile(panel, 'notes.txt') : '(none)';
    await closeFiles(panel);
    const freshMessages = await panel.eval(`[...document.querySelectorAll('#history .message.user')].length`);
    await panel.click('#home');
    await pickModel('scripted-model');
    check('a long thread: a turn end draws only that turn; back to before a message (conversation, workspace, the message in the box); compaction with a summary the model writes, said, earlier messages on request; a new thread with its files', () => {
      assert.equal(keptNode, 'kept', `the first message was not drawn again: ${keptNode}`);
      assert.equal(backInBox, 'Add more notes.', `the message came back: ${backInBox}`);
      assert.deepEqual(messagesAfterRewind, ['Start the notes.'], `the conversation went back: ${JSON.stringify(messagesAfterRewind)}`);
      assert.deepEqual(filesAfterRewind, ['notes.txt'], `the workspace went back: ${JSON.stringify(filesAfterRewind)}`);
      assert.equal(notesAfterRewind.trim(), 'one', `notes.txt as it was: ${notesAfterRewind}`);
      assert.equal(compactedState, 'idle', `the compacting turn: ${compactedState}`);
      assert.deepEqual(summaryAsked, [true, false], `one summary request, then the turn's own: ${JSON.stringify(summaryAsked)}`);
      assert.match(String(compactedNotice), /SUMMARY: the user is writing notes/, `the summary shows: ${compactedNotice}`);
      assert.ok(Array.isArray(earlierShown) && earlierShown.some((line) => line.startsWith('Start the notes')), `the earlier messages show: ${JSON.stringify(earlierShown)}`);
      assert.match(String(carried), /Starts with the files of/, `the composer says what the thread starts with: ${carried}`);
      assert.deepEqual(freshFiles, ['notes.txt'], `the new thread's files: ${JSON.stringify(freshFiles)}`);
      assert.equal(freshNotes.trim(), 'one');
      assert.equal(freshMessages, 1, 'and none of its conversation');
    });

    // 7f4. The camera button: a picture of the tab the user is on goes with the message
    // and shows with it in the conversation.
    step = 'screenshot';
    await panel.click('#add');
    await panel.click('#shoot');
    const shotChip = await panel.until(`[...document.querySelectorAll('#attached .chip > span')].map((n) => n.textContent).find((t) => /^Screenshot \\d{1,2}:\\d{2}:\\d{2}.* · /.test(t)) || null`, { what: 'the picture attached' }).catch(async (error) => `${error.message} (the panel said: ${await panel.eval(`document.querySelector('#toast').textContent`).catch(() => '?')})`);
    scripted.replies.push(text('a picture'));
    await sendPrompt(panel, 'What is in the picture?');
    await panel.until(`spindlePanel.threads.get(${JSON.stringify(refusedThread)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
    const shotTurn = await settled(panel, refusedThread);
    const shotShown = await panel.until(`(() => { const img = [...document.querySelectorAll('#history .file-card.kind-picture img.file-picture')].find((i) => /^Screenshot /.test(i.alt)); return img?.complete && img.naturalWidth > 0 ? img.naturalWidth : null; })()`, { what: 'the picture shown with the message' }).catch((error) => error.message);
    const shotSaid = scripted.seen.at(-1)?.body ?? '';
    check('the camera button attaches a picture of the tab; it shows with the message, and the thread is told it is in the workspace', () => {
      // Named on screen for when it was taken; the file keeps its stamped name.
      assert.match(String(shotChip), /^Screenshot \d{1,2}:\d{2}:\d{2}/, `attached: ${shotChip}`);
      assert.equal(shotTurn, 'idle');
      assert.ok(Number(shotShown) > 0, `the picture shows: ${shotShown}`);
      assert.match(shotSaid, /Attached, in the workspace: screenshot-\d{8}-\d{6}\.png/, 'the thread is told where the picture is');
    });
  }

  // 7e. Browsing: a thread reads the page's parts, types and clicks on it, pictures it,
  // watches its network through a reload and goes to another address.
  step = 'browsing';
  await panel.click('#home');
  // Standing instructions: set once in Settings, every thread keeps to them.
  const standing = 'Keep answers to one short line.';
  await panel.click('#settings');
  await unfold(panel, 'instructions');
  await panel.type('#standing', standing);
  await panel.click('#standing-form button.primary');
  await panel.until(`spindlePanel.engine.standing === ${JSON.stringify(standing)}`, { what: 'the instructions to be kept' });
  await panel.click('#settings-dialog [data-close]');
  const browsePrompt = 'On the page I am looking at, use your page tools: read its parts with page_elements, type violet into the Search field and click Go, click Delete and accept the confirmation it asks for, take a screenshot, read its HTML with page_html, start watching the network, reload the page with navigate, read the network log filtered by ping, then navigate to the address the Second page link goes to. Reply with only the status code of the ping request.';
  const refsKnown = {};
  if (options.scripted) {
    const next = { after: new Promise((done) => { refsKnown.release = done; }) };
    scripted.replies.push(call('page_elements', {}), next);
    refsKnown.next = next;
  }
  const browseThread = await newThread(panel, browsePrompt);
  if (options.scripted) {
    // The refs the page got: the model's next calls use them, as a real model's would.
    const parts = await panel.until(`(() => { const card = [...document.querySelectorAll('#history .tool:not(.running)')].find((c) => c.dataset.tool === 'page_elements'); return card?.querySelector('pre.result')?.textContent ?? null; })()`, { timeout: 60_000, what: 'the page parts' });
    const field = /textbox "Search"[^\n]*\[([a-z]{3}\d+)\]/.exec(parts)?.[1];
    const go = /button "Go"[^\n]*\[([a-z]{3}\d+)\]/.exec(parts)?.[1];
    const remove = /button "Delete"[^\n]*\[([a-z]{3}\d+)\]/.exec(parts)?.[1];
    refsKnown.next.then = call('type', { ref: field ?? 'e0', text: 'violet' });
    scripted.replies.push(call('click', { ref: go ?? 'e0' }), call('click', { ref: remove ?? 'e0' }), call('dialog', { accept: true }),
      call('screenshot', {}), call('page_html', {}),
      call('network', { action: 'start' }), call('navigate', { action: 'reload' }), call('network', { action: 'read', filter: 'ping' }),
      call('navigate', { url: `http://127.0.0.1:${port}/second.html` }), text('200'));
    refsKnown.release();
  }
  const browsed = await settled(panel, browseThread, Number(process.env.SPINDLE_STEP_WAIT ?? 300_000));
  const browseInstructions = await panel.until(`spindlePanel.status?.settings?.instructions || null`, { what: 'the finished thread status to render' });
  // The screenshot's card shows the picture once it is read from the workspace.
  await panel.until(`document.querySelector('#history .tool[data-tool="screenshot"] img.shot') && true`, { timeout: 15_000, what: 'the screenshot in its card' }).catch(() => {});
  const browseAnswer = await lastAnswer(panel);
  // The screenshot's picture comes from the workspace a moment after its card.
  await panel.until(`document.querySelector('#history .tool[data-tool="screenshot"] img.shot') && true`, { timeout: 15_000, what: 'the screenshot in the panel' }).catch(() => {});
  const cards = await panel.eval(`Object.fromEntries([...document.querySelectorAll('#history .tool')].map((c) => [c.dataset.tool, { failed: c.classList.contains('failed'), text: c.querySelector('pre.result')?.textContent ?? '', picture: Boolean(c.querySelector('img.shot')), out: c.querySelector('.tool-out')?.textContent ?? '' }]))`);
  const browseFiles = await filesOf(panel);
  await closeFiles(panel);
  const pageNow = await page.eval('location.pathname');
  await panel.screenshot(join(shots, '03e-browsing.png'));
  check('a thread browses (reads the parts of a page, types and clicks there, answers its dialog, pictures it, reads its HTML, watches its network, navigates), keeping to the standing instructions', () => {
    assert.equal(browsed, 'idle', `state ${browsed}`);
    for (const name of ['page_elements', 'type', 'click', 'dialog', 'screenshot', 'navigate', 'network']) {
      assert.ok(cards[name] && !cards[name].failed, `${name}: ${JSON.stringify(cards[name])}`);
    }
    assert.match(cards.page_elements.text, /textbox "Search"[\s\S]*button "Go"[\s\S]*link "Second page"/);
    assert.ok(pageClicks.includes('violet'), `the page's Go button searched for: ${JSON.stringify(pageClicks)}`);
    assert.ok(pageClicks.includes('confirmed'), `the confirmation was accepted: ${JSON.stringify(pageClicks)}`);
    assert.ok(browseFiles.some((name) => /^screenshots\/.+\.jpg$/.test(name)), `files: ${browseFiles}`);
    assert.ok(cards.screenshot.picture, 'the panel shows the screenshot');
    // Named on screen for when it was taken, not by its file's stamp and tab number.
    assert.match(cards.screenshot.out, /^Screenshot \d{1,2}:\d{2}:\d{2}/, `the picture's name: ${cards.screenshot.out}`);
    assert.equal(pageNow, '/second.html', 'the tab went to the second page');
    assert.match(browseAnswer, /200/, `answer: ${browseAnswer}`);
    assert.ok(browseInstructions.includes(standing), `the thread keeps the standing instructions: ${browseInstructions.slice(0, 300)}`);
    if (options.scripted) assert.ok(scripted.seen.some((seen) => seen.body.includes(standing)), 'the model was told them');
    if (options.scripted) {
      assert.ok(cards.page_html && cards.page_html.text.includes('<button') && !cards.page_html.text.includes('<script'), 'page_html without scripts');
      assert.match(cards.network.text, /\/ping/, 'the network log has the page\'s ping');
    }
  });

  // 7c. Before acting (Settings, Pages): Ask asks before every action and the user's No
  // does nothing (a known outcome); Auto lets a plain key go and asks before Enter; the
  // user's Yes goes ahead. Full (the default) asked nothing above.
  if (options.scripted) {
    step = 'before acting';
    const confirmTo = async (value) => {
      await panel.click('#home');
      await panel.click('#settings');
      await unfold(panel, 'pages');
      await panel.click(`#act-confirm button[data-value="${value}"]`);
      await panel.until(`document.querySelector('#act-confirm button[data-value="${value}"]').getAttribute('aria-checked') === 'true' || null`, { what: `Before acting: ${value}` });
      await panel.click('#settings-dialog [data-close]');
    };
    const question = () => panel.until(`document.querySelector('#asking:not([hidden]) .ask-text')?.textContent || null`, { timeout: 60_000, what: 'the question' });
    await confirmTo('ask');
    scripted.replies.push(call('navigate', { url: `http://127.0.0.1:${port}/page.html` }), text('Not allowed.'));
    const askThread = await newThread(panel, 'Go back to the first page.');
    const askedText = await question();
    const askedRow = await panel.eval(`(() => { const row = document.querySelector('#threads .thread-row[data-id="${askThread}"] .thread-meta'); return row ? row.textContent : null; })()`);
    await panel.click('#asking button:not(.primary)');
    const denied = await settled(panel, askThread, 120_000);
    const deniedCard = await panel.eval(`(() => { const card = [...document.querySelectorAll('#history .tool')].find((c) => c.dataset.tool === 'navigate'); return card ? card.querySelector('pre.result')?.textContent ?? '' : null; })()`);
    const pageAfterDeny = await page.eval('location.pathname');
    await confirmTo('auto');
    scripted.replies.push(call('press', { key: 'Tab' }), call('press', { key: 'Enter' }), text('Pressed.'));
    const autoThread = await newThread(panel, 'Press Tab, then Enter.');
    const autoAsked = await question();
    await panel.click('#asking button.primary');
    const allowed = await settled(panel, autoThread, 120_000);
    const pressCards = await panel.eval(`[...document.querySelectorAll('#history .tool')].filter((c) => c.dataset.tool === 'press').map((c) => ({ failed: c.classList.contains('failed'), text: c.querySelector('pre.result')?.textContent ?? '' }))`);
    // Asked about the page a tab shows, Yes acts only while it still shows that page: the
    // page reloaded while the question waited, so nothing is pressed (said to the model).
    await confirmTo('ask');
    scripted.replies.push(call('press', { key: 'Enter' }), text('Not pressed.'));
    const staleThread = await newThread(panel, 'Press Enter on that page.');
    const staleAsked = await question();
    await page.eval('window.beforeReload = true; location.reload()');
    await page.until(`!window.beforeReload && document.readyState === 'complete'`, { what: 'the page to reload' });
    await panel.click('#asking button.primary');
    const staleState = await settled(panel, staleThread, 120_000);
    const staleCard = await panel.eval(`(() => { const card = [...document.querySelectorAll('#history .tool')].find((c) => c.dataset.tool === 'press'); return card ? card.querySelector('pre.result')?.textContent ?? '' : null; })()`);
    await confirmTo('full');
    check('Before acting: Ask asks before an action and No does nothing; Auto lets a plain key go and asks before Enter; Yes goes ahead, but not on a page reloaded since the question', () => {
      assert.match(askedText, /Go to http:\/\/127\.0\.0\.1:\d+\/page\.html/, askedText);
      assert.equal(denied, 'idle', `state ${denied}`);
      assert.match(deniedCard ?? '', /said no/, `the navigate card: ${deniedCard}`);
      assert.equal(pageAfterDeny, '/second.html', 'the tab stayed where it was');
      assert.match(autoAsked, /Press Enter/, autoAsked);
      assert.equal(allowed, 'idle', `state ${allowed}`);
      assert.equal(pressCards.length, 2, JSON.stringify(pressCards));
      assert.ok(pressCards.every((card) => !card.failed), JSON.stringify(pressCards));
      if (askedRow !== null) assert.match(askedRow, /Asks you/, askedRow);
      assert.match(staleAsked, /Press Enter in tab \d+/, staleAsked);
      assert.equal(staleState, 'idle', `state ${staleState}`);
      assert.match(staleCard ?? '', /shows another page than the one the user allowed this on[\s\S]*not done/, `the press card: ${staleCard}`);
    });
  }

  if (spindleHost?.sandbox) {
    // 7b. The thread's commands in the Spindle host's sandbox.
    step = 'host runner';
    await panel.click('#home');
    await panel.click('#settings');
    await unfold(panel, 'commands', 'host');
    await panel.eval(`document.querySelector('#host-url').value = ''`);
    await panel.type('#host-url', spindleHost.url);
    await panel.type('#host-token', spindleHost.token);
    await panel.click('#host-token-form button.primary');
    const hostSaid = await panel.until(`spindlePanel.engine.host?.connected || (spindlePanel.engine.host?.error && 'error: ' + spindlePanel.engine.host.error) || (!document.querySelector('#toast').hidden && document.querySelector('#toast').textContent.includes('not connect') && document.querySelector('#toast').textContent)`, { timeout: 30_000, what: 'the host to connect' });
    await panel.click('#settings-dialog [data-close]');
    await panel.click(`#threads .thread-row[data-id="${linuxThread}"]`);
    await panel.until(`spindlePanel.current === ${JSON.stringify(linuxThread)}`, { what: 'the Linux thread to open' });
    await panel.click('#model-chip');
    await panel.click('#model-runner [data-value="remote"]');
    await panel.click('#model-apply');
    await panel.until(`spindlePanel.threads.get(${JSON.stringify(linuxThread)})?.runner === 'remote'`, { what: 'the thread to switch to the host' });
    scripted.replies.push(call('bash', { command: 'uname -m' }), text('x86_64'));
    await sendPrompt(panel, 'Use bash to run exactly: uname -m. Reply with only its output.');
    await panel.until(`spindlePanel.threads.get(${JSON.stringify(linuxThread)})?.state === 'running' || null`, { timeout: 20_000 }).catch(() => {});
    const hostRun = await settled(panel, linuxThread);
    const hostCard = await panel.eval(`(() => { const cards = [...document.querySelectorAll('#history .tool')].filter((c) => c.querySelector('.tool-name').textContent === 'bash'); const card = cards.at(-1); return card ? { failed: card.classList.contains('failed'), text: card.querySelector('pre.result')?.textContent ?? '' } : null; })()`);
    const hostNote = await panel.eval(`spindlePanel.threads.get(${JSON.stringify(linuxThread)})?.note ?? ''`);
    await panel.screenshot(join(shots, '03d-host-runner.png'));
    check("a thread switched to the Spindle host runs bash in the host's sandbox", () => {
      assert.equal(hostSaid, true, `the host: ${hostSaid}`);
      assert.equal(hostRun, 'idle', `state ${hostRun}: ${hostNote}`);
      assert.ok(hostCard && !hostCard.failed, `the bash call: ${JSON.stringify(hostCard)}`);
      assert.match(hostCard.text, /x86_64/, 'uname answered from the host');
    });
  } else {
    console.log(`skip a thread switched to the Spindle host (${spindleHost ? 'the host has no sandbox' : 'no SPINDLE_BIN'})`);
  }
  // 9. Threads on the host: Connect, Run on my host, turns there, Bring back, moves cut.
  if (spindleHost?.sandbox && options.scripted && process.env.SPINDLE_ONLY !== 'browser') panel = await hostedLeg(panel);
  else console.log(`skip threads on the Spindle host (${process.env.SPINDLE_ONLY === 'browser' ? 'separate hosted suite requested' : !spindleHost ? 'no SPINDLE_BIN' : !spindleHost.sandbox ? 'the host has no sandbox' : 'a real provider'})`);

  step = 'restart engine';
  await panel.click('#home');
  await panel.eval('globalThis.beforeRestart = spindlePanel.engine');
  await panel.click('#settings');
  await unfold(panel, 'about');
  await panel.click('#restart-engine');
  await panel.until('spindlePanel.engine && spindlePanel.engine !== globalThis.beforeRestart', { timeout: 60_000, what: 'a new engine to answer' });
  await panel.click(`#threads .thread-row[data-id="${first}"]`);
  await panel.until(`spindlePanel.current === ${JSON.stringify(first)} && document.querySelectorAll('#history .message').length > 1`, { what: 'the first thread to open on the new engine' });
  const afterRestart = await lastAnswer(panel);
  check('Restart engine brings up a new engine that has every thread', () => {
    assert.ok(afterRestart.includes(secret), `answer: ${afterRestart}`);
  });

  step = 'delete';
  await panel.click('#home');
  const before = await panel.eval('spindlePanel.threads.size');
  const freeBefore = await freeSlots();
  await panel.click(`#threads .thread-row[data-id="${third}"]`);
  await panel.until(`spindlePanel.current === ${JSON.stringify(third)}`, { what: 'the story thread to open' });
  await panel.click('#title-button');
  await panel.click('#menu-delete');
  // Deleted with a moment to undo it: it goes once the moment passes.
  await panel.until(`!spindlePanel.threads.has(${JSON.stringify(third)})`, { what: 'the thread to go', timeout: 30_000 });
  // And one from the thread list itself (its row's ⋯ menu): Undo brings it back; deleted
  // again, it goes.
  await panel.until(`document.querySelector('#threads .thread-row[data-id="${fourth}"]') && true`, { what: 'the list to show the cut thread' });
  const deleteFromList = async () => {
    await panel.click(`#threads .thread-item:has(.thread-row[data-id="${fourth}"]) .thread-more`);
    await panel.click('#row-menu button.danger');
  };
  await deleteFromList();
  const leftList = await panel.eval(`!document.querySelector('#threads .thread-row[data-id="${fourth}"]') && document.querySelector('#toast-text').textContent`);
  await panel.click('#toast-action');
  const undone = await panel.until(`document.querySelector('#threads .thread-row[data-id="${fourth}"]') && spindlePanel.threads.has(${JSON.stringify(fourth)}) || null`, { what: 'Undo to bring it back', timeout: 5000 }).catch(() => false);
  await new Promise((done) => setTimeout(done, 9000));
  const stayed = await panel.eval(`spindlePanel.threads.has(${JSON.stringify(fourth)})`);
  await deleteFromList();
  await panel.until(`!spindlePanel.threads.has(${JSON.stringify(fourth)})`, { what: 'the thread deleted from the list to go', timeout: 30_000 });
  check('a thread deleted from the list leaves it at once with Undo, which brings it back', () => {
    assert.match(leftList || '', /^Deleted “/, `left the list: ${leftList}`);
    assert.ok(undone, 'Undo brought it back');
    assert.ok(stayed, 'and it stayed after the moment passed');
  });
  const after = await panel.eval('spindlePanel.threads.size');
  const freeAfter = await freeSlots();
  await panel.screenshot(join(shots, '07-list.png'));
  check('deleting a thread (from its menu or the list) removes it and its files; no bundle holds the key', () => {
    assert.equal(after, before - 2, 'one from its menu, one from the list');
    assert.ok(freeAfter > freeBefore, `free storage slots ${freeBefore} -> ${freeAfter}`);
    assert.ok(bundles.length > 0);
    for (const [name, bytes] of bundles) assert.ok(!bytes.includes(Buffer.from(key)), `${name} holds the key`);
  });
  await page.close().catch(() => {});
} catch (error) {
  if (error !== ONLY) {
    results.push([`step ${step}`, false, error.stack ?? error.message]);
    console.log(`FAIL step ${step}: ${error.stack ?? error.message}`);
  }
} finally {
  await browser?.close().catch(() => {});
  server.close();
  for (const response of scripted.holding) response.destroy();
}
const failed = results.filter(([, ok]) => !ok);
console.log(`${failed.length ? 'extension-real FAILED' : 'extension-real passed'}: ${results.length - failed.length}/${results.length} checks (${options.scripted ? 'scripted provider' : `${providerId}, real requests`})`);
process.exit(failed.length ? 1 : 0);
