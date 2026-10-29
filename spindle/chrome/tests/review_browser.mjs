// Focused fresh-eyes review in a disposable real Chrome profile, no provider key.
// usage: SPINDLE_BROWSER=... node chrome/tests/review_browser.mjs DIST
import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { launch, open } from './cdp.mjs';

const [distArg] = process.argv.slice(2);
assert.ok(distArg && process.env.SPINDLE_BROWSER, 'SPINDLE_BROWSER=... node review_browser.mjs DIST');
const work = fs.mkdtempSync(join(tmpdir(), 'spindle-review-'));
let browser;
const replies = [];
let requests = 0;
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
  const at = replies.findIndex((reply) => !reply.for || body.includes(reply.for));
  let reply = (at >= 0 ? replies.splice(at, 1)[0] : null) ?? { content: 'ready' };
  // A reply that depends on what the model was just told (a ref a tool returned).
  if (reply.from) reply = reply.from(body);
  if (reply.refuse) { response.writeHead(reply.refuse).end('refused'); return; }
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
  browser = await launch(process.env.SPINDLE_BROWSER, { profile: join(work, 'profile') });
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
  const settled = (id) => page.until(`spindlePanel.threads.get(${JSON.stringify(id)})?.state === 'idle'`, { what: `${id} to finish` });
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
  await page.click('#shoot');
  assert.equal(await page.eval('captures'), 0);
  await page.eval('permissionAnswer(false)');
  await page.until(`document.querySelector('#toast').textContent.includes('did not let')`);
  assert.equal(await page.eval('captures'), 0);
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
    await chrome.offscreen.closeDocument();
    await shared.startEngine();
  })()`);
  await page.until(`import('./shared.mjs').then((shared) => shared.read('kv', 'reset:${failed.id}')).then((record) => !record)`, { what: 'reset recovery after engine restart', timeout: 20_000 });
  const recovered = await rpc('open', { id: failed.id });
  assert.deepEqual(recovered.history, old.history);
  assert.equal(await page.eval(`import('./shared.mjs').then((shared) => shared.read('kv', 'reset:${failed.id}')).then(Boolean)`), false);
  console.log('ok interrupted reset restores its saved record after engine restart');
  // A move sealed before the index update must still show as moved after restart.
  const moved = await rpc('create', { provider: 'review', runner: 'none', text: 'move' });
  await settled(moved.id);
  await rpc('export', { id: moved.id });
  await page.eval(`(async () => { const shared = await import('./shared.mjs'); await shared.update('threads', '${moved.id}', (thread) => ({ ...thread, state: 'idle' })); await chrome.offscreen.closeDocument(); await shared.startEngine(); })()`);
  await page.until(`import('./shared.mjs').then((shared) => shared.read('threads', '${moved.id}')).then((thread) => thread.state === 'moved')`, { what: 'sealed move reconciled' });
  assert.equal((await rpc('open', { id: moved.id })).thread.state, 'moved');
  console.log('ok sealed move reconciles a stale thread index');

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
} finally {
  await browser?.close();
  server.close();
}
console.log('review-browser passed');
