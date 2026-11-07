// Focused fresh-eyes review in a disposable real Chrome profile, no provider key.
// usage: SPINDLE_BROWSER=... [SPINDLE_SHOTS=DIR] node chrome/tests/review_browser.mjs DIST
// (SPINDLE_SHOTS: pictures of the tasks' cards, lines and reports, at a side panel's width)
import assert from 'node:assert/strict';
import fs from 'node:fs';
import dgram from 'node:dgram';
import http from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { attach, launch, open } from './cdp.mjs';
import { hostileFixtures, officeFixtures } from './office_fixtures.mjs';

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
/** What the upload page's file field was given (its change handler posts it). */
let uploaded = null;
const server = http.createServer(async (request, response) => {
  if (request.url === '/page') {
    response.writeHead(200, { 'content-type': 'text/html' }).end('<title>Review page</title><p>Review</p>');
    return;
  }
  if (request.url === '/opener') {
    response.writeHead(200, { 'content-type': 'text/html' }).end('<title>Opener page</title><a href="/page?popup" target="_blank" style="margin:40px;display:block">Popup</a>');
    return;
  }
  if (request.url.startsWith('/page?')) {
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
  if (request.url === '/upload') {
    // A styled button beside its hidden file field, as most sites draw one; the page reads
    // the file it is given as a site that uploads at once would.
    response.writeHead(200, { 'content-type': 'text/html' }).end(`<title>Upload page</title>
<div class="drop"><button type="button" onclick="this.nextElementSibling.click()">Upload resume</button>
<input type="file" aria-label="Resume" accept=".pdf" hidden></div>
<script>document.querySelector('input').addEventListener('change', async (event) => {
  const [file] = event.target.files;
  const head = [...new Uint8Array(await file.arrayBuffer())].slice(0, 9);
  fetch('/uploaded', { method: 'POST', body: JSON.stringify({ name: file.name, type: file.type, size: file.size, head, trusted: event.isTrusted }) });
});</script>`);
    return;
  }
  if (request.url === '/uploaded') {
    const parts = [];
    for await (const part of request) parts.push(part);
    uploaded = JSON.parse(Buffer.concat(parts).toString());
    response.writeHead(204).end();
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
    return (op, args = {}, wait = 15000) => new Promise((resolve, reject) => { const id = ++next; const timer = setTimeout(() => { pending.delete(id); reject(new Error(op + ' did not answer in ' + wait / 1000 + ' s')); }, wait); pending.set(id, { resolve, reject, timer }); channel.postMessage({ to: 'engine', from: 'review', id, op, args }); });
  })()`);
  const catalog = { version: 4, active_provider: 'review', providers: [{ id: 'review', display_name: 'Review', api_provider: 'openai', request_contract: 'openai-chat-completions', base_url: `http://127.0.0.1:${port}/v1`, default_model: 'review-model', requires_api_key: false, models: ['review-model'] }] };
  const catalogPath = join(work, 'providers.json');
  fs.writeFileSync(catalogPath, JSON.stringify(catalog));
  await page.files('#catalog-file', [catalogPath]);
  await page.until(`spindlePanel.engine.catalog.providers.some((profile) => profile.id === 'review')`);
  await page.eval(`document.querySelector('#settings-dialog').open && document.querySelector('#settings-dialog').close()`);
  const rpc = (op, args = {}, wait = 15000) => page.eval(`rpc(${JSON.stringify(op)}, ${JSON.stringify(args)}, ${wait})`);
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

  // Move progress and thread updates share one strict read/modify/write transaction.
  const atomic = await page.eval(`(async () => {
    const shared = await import('./shared.mjs');
    const id = 'atomic-review';
    await shared.write('threads', id, { title: 'before', state: 'idle' });
    const merge = shared.update('threads', id, (thread) => ({ ...thread, moving: null }), {
      durability: 'strict', changes: [['kv', 'atomic-sibling', { done: true }]],
    });
    const turn = shared.update('threads', id, (thread) => ({ ...thread, title: 'latest', state: 'running' }));
    await Promise.all([merge, turn]);
    const merged = await shared.read('threads', id);
    let refused = false;
    try {
      await shared.update('threads', id, (thread) => ({ ...thread, title: 'must roll back' }), {
        durability: 'strict', changes: [['kv', 'atomic-sibling', { done: false }], ['bundles', 'unclonable', () => {}]],
      });
    } catch { refused = true; }
    const after = await shared.read('threads', id);
    const sibling = await shared.read('kv', 'atomic-sibling');
    const absent = await shared.update('threads', 'missing-review', () => ({}), { changes: [['kv', 'atomic-missing', true]], durability: 'strict' });
    const missing = await shared.read('kv', 'atomic-missing');
    await shared.remove('threads', id);
    await shared.remove('kv', 'atomic-sibling');
    return { merged, after, sibling, refused, absent: absent === undefined, missing: missing === undefined };
  })()`);
  assert.equal(atomic.merged.title, 'latest');
  assert.equal(atomic.merged.state, 'running');
  assert.deepEqual(atomic.after, atomic.merged, 'a refused sibling write rolls back the thread update too');
  assert.deepEqual(atomic.sibling, { done: true });
  assert.ok(atomic.refused && atomic.absent && atomic.missing);
  console.log('ok strict move/index transactions merge with concurrent updates and abort atomically');

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

  // The tabs a turn opened close once it is done: one it opened, and one a page opened on
  // its click; one it kept open for the user stays, and so does the user's own tab.
  const opener = await open(browser, `http://127.0.0.1:${port}/opener`);
  const openerId = await page.eval(`chrome.tabs.query({}).then((tabs) => tabs.find((tab) => tab.url === 'http://127.0.0.1:${port}/opener').id)`);
  replies.push(
    { for: 'open some tabs', tool: 'navigate', input: { url: `http://127.0.0.1:${port}/page?mine`, new_tab: true } },
    { for: 'open some tabs', tool: 'navigate', input: { url: `http://127.0.0.1:${port}/page?kept`, new_tab: true, keep_open: true } },
    { for: 'open some tabs', tool: 'page_elements', input: { tab: openerId } },
    { for: 'open some tabs', from: (body) => ({ tool: 'click', input: { ref: /link \\"Popup\\"[^[]*\[([a-z]{3}\d+)\]/.exec(body)?.[1] ?? 'k0', tab: openerId } }) },
    { for: 'open some tabs', content: 'done' },
  );
  const tabsAt = (path) => page.eval(`chrome.tabs.query({}).then((tabs) => tabs.filter((tab) => (tab.url || tab.pendingUrl) === 'http://127.0.0.1:${port}${path}').length)`);
  const opening = await rpc('create', { provider: 'review', runner: 'none', text: 'open some tabs' });
  await settled(opening.id);
  const openedSaid = await page.eval(`(async () => (await rpc('open', { id: ${JSON.stringify(opening.id)} })).history.messages.flatMap((m) => m.content ?? []).filter((b) => b.type === 'tool_result').map((b) => JSON.stringify(b.content)).join(' | '))()`);
  assert.match(openedSaid, /Clicked/, `the link was clicked: ${openedSaid.slice(0, 600)}`);
  await page.until(`(async () => {
    const tabs = await chrome.tabs.query({});
    const at = (path) => tabs.filter((tab) => (tab.url || tab.pendingUrl) === 'http://127.0.0.1:${port}' + path).length;
    return at('/page?mine') === 0 && at('/page?popup') === 0 || null;
  })()`, { what: 'the turn\'s tabs to close', timeout: 15_000 }).catch(async (error) => {
    const left = await page.eval(`(async () => ({ tabs: (await chrome.tabs.query({})).map((tab) => [tab.id, tab.url, tab.openerTabId]), opened: await chrome.storage.session.get(null) }))()`);
    throw new Error(`${error.message}\n${JSON.stringify(left)}`);
  });
  assert.equal(await tabsAt('/page?kept'), 1, 'the tab kept open for the user stays');
  assert.equal(await tabsAt('/opener'), 1, 'the user\'s own tab stays');
  console.log('ok the tabs a turn opened close once it is done (not one kept for the user, nor the user\'s)');
  await page.eval(`chrome.tabs.query({}).then((tabs) => Promise.all(tabs.filter((tab) => /\\/page\\?kept$/.test(tab.url ?? '')).map((tab) => chrome.tabs.remove(tab.id))))`);
  await opener.close();

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

  // A file the user attached goes into a site's file field (hidden behind its button), as
  // if picked in its dialog: the page reads the very bytes; no dialog opens.
  const upload = await open(browser, `http://127.0.0.1:${port}/upload`);
  const uploadId = await page.eval(`chrome.tabs.query({}).then((tabs) => tabs.find((tab) => tab.url === 'http://127.0.0.1:${port}/upload').id)`);
  const uploadRef = (body) => /button \\"Upload resume\\" \[([a-z]{3}\d+)\]/.exec(body)?.[1] ?? 'k0';
  replies.push(
    { for: 'upload the resume', tool: 'page_elements', input: { tab: uploadId } },
    // Clicked first, its button opens the file field's dialog: caught, not shown.
    { for: 'upload the resume', from: (body) => ({ tool: 'click', input: { ref: uploadRef(body), tab: uploadId } }) },
    { for: 'upload the resume', from: (body) => ({ tool: 'select', input: { ref: uploadRef(body), files: ['resume.pdf'], tab: uploadId } }) },
    { for: 'upload the resume', content: 'done' },
  );
  const uploadThread = await page.eval(`rpc('create', { provider: 'review', runner: 'none', text: 'upload the resume', files: [{ path: 'resume.pdf', bytes: new TextEncoder().encode('%PDF-1.7 a resume') }] })`);
  // Before acting is Auto (the Pages record above names no mode): the click went without a
  // question, but a file put in a field reaches the site, so that waits for the user's Yes.
  const uploadAsked = await page.until(`spindlePanel.engine.approvals?.find((approval) => approval.thread === ${JSON.stringify(uploadThread.id)}) ?? null`, { what: 'the question before the upload' });
  assert.match(uploadAsked.text, /^Upload “resume\.pdf” \(17 bytes\) to /, uploadAsked.text);
  assert.equal(uploaded, null, 'nothing reached the page before the user said yes');
  await rpc('approve', { approval: uploadAsked.id, allow: true });
  await settled(uploadThread.id);
  const uploadResults = await page.eval(`(async () => (await rpc('open', { id: ${JSON.stringify(uploadThread.id)} })).history.messages.flatMap((m) => m.content ?? []).filter((b) => b.type === 'tool_result').map((b) => JSON.stringify(b.content)))()`);
  assert.match(uploadResults[0], /file \\"Resume\\" accepts=\\".pdf\\" empty hidden \[/, `the hidden file field shows: ${uploadResults[0]?.slice(0, 600)}`);
  assert.match(uploadResults[1], /it opened a file dialog, which Spindle caught \(nothing was chosen\)/, uploadResults[1]);
  assert.match(uploadResults[2], /Put resume\.pdf \(17 bytes\) in file \\"Resume\\" \[[a-z]{3}\d+\] \(the file field of \[/, uploadResults[2]);
  for (const until = Date.now() + 10_000; !uploaded && Date.now() < until;) await new Promise((done) => setTimeout(done, 50));
  assert.deepEqual(uploaded, { name: 'resume.pdf', type: 'application/pdf', size: 17, head: [...Buffer.from('%PDF-1.7 ')], trusted: false }, 'the page read the very file');
  console.log('ok a workspace file goes into a hidden file field beside its button once the user allows it (Auto asks first), read by the page');
  await upload.close();

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
  // A reset that succeeds (the first message taken back) keeps the thread as it was, as
  // an earlier version, never deleted; it survives an engine restart, and Storage lists
  // it, kept until the thread has gone on past it.
  replies.push({ refuse: 401 });
  const over = await rpc('create', { provider: 'review', runner: 'none', text: 'start over' });
  await page.until(`spindlePanel.threads.get('${over.id}')?.state === 'failed'`);
  const taken = await rpc('restart', { id: over.id, resend: false });
  assert.equal(taken.text, 'start over');
  const earlierCount = () => page.eval(`import('./shared.mjs').then((shared) => shared.keysOf('kv')).then((keys) => keys.filter((key) => key.startsWith('earlier:${over.id}:')).length)`);
  assert.equal(await earlierCount(), 1, 'a finished reset keeps the thread as it was');
  assert.equal(await page.eval(`import('./shared.mjs').then((shared) => shared.keysOf('bundles')).then((keys) => keys.filter((key) => key.startsWith('prior:${over.id}:')).length)`), 1, 'with its whole copy');
  const listed = (await rpc('storage', {})).threads.find((entry) => entry.id === over.id);
  assert.equal(listed?.versions?.length, 1);
  assert.equal(listed.versions[0].free, false, 'kept until the thread has gone on past it');
  await page.eval(`(async () => { const shared = await import('./shared.mjs'); await chrome.offscreen.closeDocument(); await shared.startEngine(); })()`);
  await page.until(`import('./shared.mjs').then((shared) => shared.keysOf('kv')).then((keys) => keys.some((key) => key.startsWith('earlier:${over.id}:')))`, { what: 'earlier version after engine restart', timeout: 20_000 });
  assert.equal(await earlierCount(), 1, 'an engine start keeps it');
  // Opened, it is a thread of its own with the history the reset took back.
  // The engine that started again answers (a message sent before it listens is lost).
  await page.until(`rpc('ping', {}, 2000).then(() => true, () => false)`, { what: 'the engine after its restart', timeout: 30_000 });
  const opened = await rpc('earlierOpen', { id: over.id, backup: listed.versions[0].backup }, 60_000);
  const back = await rpc('open', { id: opened.id });
  assert.ok(JSON.stringify(back.history.messages).includes('start over'), 'the earlier version holds the first message');
  console.log('ok a reset that succeeds keeps the thread as it was, as an earlier version that opens as a thread');
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
    await page.eval(`[...document.querySelectorAll('#storage .storage-row')].find((row) => /free inside/.test(row.textContent)).querySelector('.storage-act').click()`);
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
  // Removed from its row's ⋯ menu in Settings, with real clicks (a menu outside the open
  // dialog showed there but took none: the dialog leaves all outside it inert).
  await page.eval(`(() => { document.querySelector('#settings').click(); document.querySelector('[data-fold="storage"]').open = true; })()`);
  await page.until(`(() => {
    const row = [...document.querySelectorAll('#storage .storage-row')].find((row) => row.querySelector('.storage-name')?.textContent === spindlePanel.threads.get('${cut.id}').title && /first attempts/.test(row.textContent));
    row?.querySelector('.storage-more')?.setAttribute('data-probe', 'aside');
    return row ? true : null;
  })()`, { what: 'its Storage row' });
  await page.click('.storage-more[data-probe="aside"]');
  await page.until(`document.querySelector('#row-menu')?.matches(':popover-open') || null`, { what: 'the row menu' });
  await page.click('#row-menu button.danger');
  await page.until(`document.querySelector('#confirm-dialog').open || null`, { what: 'the confirm' });
  await page.click('#confirm-yes');
  await page.until(`/Removed/.test(document.querySelector('#toast')?.textContent ?? '') || null`, { what: 'the first attempts removed', timeout: 30_000 });
  await page.eval(`document.querySelector('#settings-dialog').close()`);
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
    // The first time, held twice mid-run: the summary shows itself once written, and folds
    // three steps later (the turn has moved on); the thread's own NOTES.md never shows.
    const gates = n === 0 ? [0, 1].map(() => { let open; const hold = new Promise((done) => { open = done; }); return { hold, open }; }) : null;
    replies.push({ tool: 'write_file', input: { path: '/workspace/report.html', content: report } },
      { tool: 'navigate', input: { url: '/workspace/report.html' } },
      { tool: 'write_file', input: { path: 'summary.md', content: `# Summary ${n}\n` } },
      { tool: 'write_file', input: { path: 'NOTES.md', content: `# Notes ${n}\n\n- what is left\n` }, ...(gates && { hold: gates[0].hold }) },
      { tool: 'bash', input: { command: "printf '# More\\n' >> summary.md" } },
      { tool: 'read_file', input: { path: 'NOTES.md' } }, { content: `report ${n}`, ...(gates && { hold: gates[1].hold }) });
    await page.eval(`chrome.permissions.request = async () => true;`);
    const requestCount = requests;
    await page.type('#prompt', `make report ${n}`);
    await page.click('#send');
    // (A loaded machine can take a while: the send reads the thread's status first.)
    for (let until = Date.now() + 30_000; requests === requestCount && Date.now() < until;) await new Promise((done) => setTimeout(done, 20));
    if (requests === requestCount) {
      const said = await page.eval(`({ toast: document.querySelector('#toast')?.textContent ?? '', state: spindlePanel.threads.get('${id}')?.state, note: spindlePanel.threads.get('${id}')?.note, box: document.querySelector('#prompt').value, queued: document.querySelector('#queued')?.hidden === false ? document.querySelector('#queued-text')?.textContent : null })`).catch((error) => ({ error: error.message }));
      assert.fail(`report ${n} did not reach the provider: ${JSON.stringify(said)}`);
    }
    if (gates) {
      const cards = `[...document.querySelectorAll('#history .file-card')].filter((card) => ['summary.md', 'NOTES.md'].includes(card.dataset.path)).map((card) => [card.dataset.path, card.querySelector('.file-body').hidden ? 'folded' : 'shown', card.closest('.group-list') ? 'in the run' : 'below it'].join(' '))`;
      // (The cards of this turn only: the thread's earlier turns wrote neither file.)
      await page.until(`${cards}.includes('summary.md shown below it') || null`, { what: 'the summary shown as written', timeout: 60_000 });
      const written = await page.eval(cards);
      gates[0].open();
      await page.until(`${cards}.includes('NOTES.md folded in the run') && document.querySelectorAll('#history .tool').length && !document.querySelector('#history .tool.running') && ${cards}.includes('summary.md folded below it') || null`, { what: 'the summary folded three steps on', timeout: 120_000 }).catch(async (error) => {
        throw new Error(`${error.message}\n${JSON.stringify(await page.eval(cards))}`);
      });
      const movedOn = await page.eval(cards);
      gates[1].open();
      assert.deepEqual(written, ['summary.md shown below it'], 'the summary shows itself once written');
      assert.deepEqual(movedOn, ['NOTES.md folded in the run', 'summary.md folded below it'], 'three steps on the summary folds; NOTES.md sits folded in the run');
      console.log('ok a Markdown file shows as written and folds three steps on; NOTES.md stays folded in its run');
    }
    await page.until(`spindlePanel.threads.get('${id}')?.state === 'idle'`, { timeout: 240_000 });
    await page.activate();
    await page.until(`document.querySelector('#history .workspace-card .ws-tree')`, { what: 'report workspace', timeout: 5000 }).catch(async (error) => {
      const details = await page.eval(`({ current: spindlePanel.current, state: spindlePanel.threads.get('${id}')?.state, text: document.querySelector('#history').innerText.slice(-1500) })`);
      const files = await rpc('turnFiles', { id }).catch((error) => error.message);
      throw new Error(`${error.message}\n${JSON.stringify({ details, files, logs })}`);
    });
    // Once the turn is done, its page and Markdown file sit folded below its steps (the
    // page opened in a tab; the summary showed while the turn worked on it), and its
    // NOTES.md folded in the run; the eye opens each, and a page's preview offers to load
    // from the web.
    // (The last card of each: an earlier turn's stays above.)
    const lastCard = (path) => `[...document.querySelectorAll('#history .file-card[data-path="${path}"]')].at(-1)`;
    const folded = await page.eval(`['report.html', 'summary.md', 'NOTES.md'].map((path) => { const card = [...document.querySelectorAll('#history .file-card[data-path="' + path + '"]')].at(-1); return card ? card.querySelector('.file-body').hidden : null; })`);
    assert.deepEqual(folded, [true, true, true], `the page, the summary and the notes are folded once the turn is done: ${JSON.stringify(folded)}`);
    await page.eval(`${lastCard('summary.md')}.querySelector('.file-actions button[aria-label^="Preview"]').click()`);
    await page.until(`/Summary ${n}/.test(${lastCard('summary.md')}?.querySelector('.file-markdown')?.textContent ?? '')`, { what: 'the summary shown on demand' }).catch(async (error) => {
      const seen = await page.eval(`(() => { const card = ${lastCard('summary.md')}; return { cards: document.querySelectorAll('#history .file-card[data-path="summary.md"]').length, hidden: card?.querySelector('.file-body')?.hidden, eye: card?.querySelector('.file-actions button')?.getAttribute('aria-label'), body: card?.querySelector('.file-body')?.innerHTML?.slice(0, 400) }; })()`);
      throw new Error(`${error.message}\n${JSON.stringify(seen)}`);
    });
    await page.eval(`${lastCard('report.html')}.querySelector('.file-actions button[aria-label^="Preview"]').click()`);
    await page.until(`${lastCard('report.html')}?.querySelector('.file-web button')`, { what: 'report web button' });
    await page.eval(`${lastCard('report.html')}.querySelector('.file-web button').click()`);
    console.log(`ok file-card flow ${n + 1}`);
  }

  // A long run (18 steps, more than a group shows at once) keeps its newest rows in sight
  // and folds the earlier ones: while the model thinks between steps (no step running),
  // and once the turn is done. At a side panel's size, the tall Markdown file it writes
  // near its end shows without stopping the conversation following the run down.
  {
    let open;
    const hold = new Promise((done) => { open = done; });
    for (let n = 0; n < 15; n++) replies.push({ tool: 'read_file', input: { path: n % 2 ? 'NOTES.md' : 'summary.md' } });
    const tall = `# Tall\n\n${Array.from({ length: 60 }, (_, n) => `- line ${n} of a long list, with words enough to wrap at a side panel's width`).join('\n')}\n`;
    replies.push({ tool: 'write_file', input: { path: 'tall.md', content: tall } }, { tool: 'read_file', input: { path: 'NOTES.md' } }, { tool: 'read_file', input: { path: 'summary.md' } });
    replies.push({ content: 'read them', hold });
    await page.activate();
    if (await page.eval(`spindlePanel.current !== '${id}'`)) {
      await page.eval(`document.querySelector('#home').click()`);
      await page.click(`#threads .thread-row[data-id="${id}"]`);
      await page.until(`spindlePanel.current === '${id}' && spindlePanel.status`);
    }
    await page.size(420, 900);
    await page.type('#prompt', 'read them all');
    await page.click('#send');
    const fold = `(() => { const steps = [...document.querySelectorAll('#history > .steps')].at(-1); const rows = [...steps.list.children]; const tools = rows.filter((row) => row.classList.contains('tool'));
      return { more: rows[0].classList.contains('group-more') ? rows[0].textContent : null, moreLast: rows.at(-1).classList.contains('group-more'), tools: tools.length, hidden: tools.map((row, at) => row.classList.contains('beyond') ? at : null).filter((at) => at !== null), running: Boolean(steps.querySelector('.tool.running')) }; })()`;
    const want = { more: '6 earlier steps', moreLast: false, tools: 18, hidden: [0, 1, 2, 3, 4, 5], running: false };
    await page.until(`(() => { const seen = ${fold}; return seen.tools === 18 && !seen.running; })() || null`, { what: 'the 18 steps done, the answer held', timeout: 60_000 });
    await new Promise((done) => setTimeout(done, 300));
    const thinking = await page.eval(fold);
    const followed = await page.eval(`(() => { const main = document.querySelector('main'); const card = [...document.querySelectorAll('#history .file-card[data-path="tall.md"]')].at(-1); return { off: main.scrollHeight - main.scrollTop - main.clientHeight, scrolls: main.scrollHeight > main.clientHeight, latest: !document.querySelector('#jump').hidden, tall: card ? (card.querySelector('.file-body').hidden ? 'folded' : 'shown') : null }; })()`);
    open();
    await page.until(`spindlePanel.threads.get('${id}')?.state === 'idle'`, { timeout: 60_000 });
    await page.until(`document.querySelector('#history .workspace-card') || null`, { what: 'the turn redrawn' });
    const done = await page.eval(fold);
    await page.unsize();
    assert.deepEqual(thinking, want, 'between steps, the earliest rows fold and the newest show');
    assert.deepEqual(done, want, 'done, the same');
    assert.ok(followed.scrolls && followed.off < 160 && !followed.latest && followed.tall === 'shown', `the conversation followed the run down past the tall preview, still shown two steps on: ${JSON.stringify(followed)}`);
    console.log('ok a long run folds its earlier steps, never its newest (between steps and once done); a tall preview does not stop the conversation following it');
  }

  // A report among many unrelated data files, opened in a tab: it runs as it shows,
  // contained (nothing to say, nothing to let), and the data file it uses comes from the
  // workspace when it asks; reloaded fast, one frame shows it, and only the report itself
  // is read to make it ready.
  const reportPage = '<!doctype html><title>Report</title><p>Report</p><script>fetch("data/used.json").then((r) => r.json()).then((d) => parent.postMessage({ review: d.word }, "*"), (e) => parent.postMessage({ review: "failed " + e.message }, "*"));'
    + 'addEventListener("beforeprint", () => parent.postMessage({ review: "printing" }, "*")); print(); alert("a"); parent.postMessage({ review: "dialogs " + confirm("b") + " " + prompt("c") }, "*")</script>';
  // Pages that print, or ask "Leave site?", only as below.
  const printPages = {
    'button.html': '<!doctype html><title>Button</title><button style="position:fixed;inset:0" onclick="print()">Print</button><script>addEventListener("beforeprint", () => parent.postMessage({ review: "printing" }, "*"))</script>',
    'keys.html': '<!doctype html><title>Keys</title><p style="height:100vh">Keys</p><script>addEventListener("beforeprint", () => parent.postMessage({ review: "printing" }, "*"))</script>',
    'leave.html': '<!doctype html><title>Leave</title><body onbeforeunload="return \'leave?\'"><p style="height:100vh">Leave</p><script>addEventListener("beforeunload", (e) => { e.preventDefault(); e.returnValue = "leave?"; }); onbeforeunload = () => "leave?";</script></body>',
    'plain.html': '<!doctype html><title>Plain</title><p>Plain</p>',
  };
  const reportThread = await page.eval(`(async () => {
    const bytes = (text) => new TextEncoder().encode(text);
    const files = [{ path: 'report.html', bytes: bytes(${JSON.stringify(reportPage)}) }, { path: 'data/used.json', bytes: bytes('{"word":"lazy"}') }];
    for (const [path, text] of Object.entries(${JSON.stringify(printPages)})) files.push({ path, bytes: bytes(text) });
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
  // Its frame stays sandboxed whatever it goes to (and is still served contained); modals
  // only so that it may print: alert, confirm and prompt show nothing, as before, and its
  // print() as it loads prints nothing (its beforeprint, told at once, would come first).
  assert.equal(await viewer.eval(`document.querySelector('iframe#view').getAttribute('sandbox')`), 'allow-scripts allow-same-origin allow-modals');
  await viewer.until(`heard.includes('dialogs false null')`, { what: 'alert, confirm and prompt to show nothing' });
  assert.ok(!(await viewer.eval('heard')).includes('printing'), 'a page does not print as it loads');
  // The tab's Print prints the page (its own beforeprint is heard).
  assert.equal(await viewer.eval(`document.querySelector('#view-print').hidden`), false, 'Print offered for a page');
  await viewer.eval(`document.querySelector('#view-print').click()`);
  await viewer.until(`heard.includes('printing')`, { what: 'the page to print from the tab' });
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
  console.log(`ok a report runs as it shows, contained; it shows once however fast it is reloaded and reads only what it uses (${timings}); it prints from the tab, and shows no alert, confirm or prompt`);

  // Printing, and nothing else a modal could do, only as the user asks: a page prints from
  // a button of its own, from Ctrl+P pressed in it or in the tab around it (the page, not
  // the tab: its beforeprint, not the tab's), and never as it loads (the report above);
  // it asks no "Leave site?" however it asks. A page with no scripts of its own prints
  // from the tab's Print too (heard from a world of the test's own in its frame); text
  // has no Print. Each print in a tab of its own: headless Chrome keeps a tab's print
  // preview open, and takes no other print there meanwhile.
  {
    const tabOf = async (path) => {
      const tab = await open(browser, `chrome-extension://${extension}/view.html?thread=${reportThread.id}&path=${path}`);
      await tab.until(`/frame /.test(document.querySelector('#view-path').title) && !document.querySelector('#view-print').hidden`, { what: `${path} shown, with Print` });
      await tab.eval(`globalThis.heard = []; addEventListener('message', ({ data }) => { if (data?.review) heard.push(data.review); }); addEventListener('beforeprint', () => heard.push('the tab printing'))`);
      return tab;
    };
    const ctrlP = async (tab) => {
      for (const type of ['rawKeyDown', 'keyUp']) await browser.send('Input.dispatchKeyEvent', { type, modifiers: 2, key: 'p', code: 'KeyP', windowsVirtualKeyCode: 80, nativeVirtualKeyCode: 80 }, tab.sessionId);
    };
    const printed = async (tab, what) => {
      await tab.until(`heard.includes('printing')`, { what, timeout: 15_000 });
      assert.ok(!(await tab.eval('heard')).includes('the tab printing'), `${what}: the page printed, not the tab`);
      await browser.send('Target.closeTarget', { targetId: tab.targetId }).catch(() => {});
    };
    const button = await tabOf('button.html');
    // While Spindle reads the page to show it, no element it reads carries the page's
    // handler (Edge checks one against Spindle's own policy at once, and lists it as an
    // error of Spindle's); the page shown still has it (its button prints).
    await button.eval(`(() => {
      const parse = DOMParser.prototype.parseFromString;
      globalThis.parsedHandlers = 0;
      DOMParser.prototype.parseFromString = function (...args) {
        const doc = parse.apply(this, args);
        for (const element of doc.querySelectorAll('*')) for (const attribute of element.attributes) if (/^on/i.test(attribute.name)) parsedHandlers += 1;
        return doc;
      };
      document.querySelector('#view-path').title = '';
      document.querySelector('#view-reload').click();
    })()`);
    await button.until(`/frame /.test(document.querySelector('#view-path').title)`, { what: 'button.html shown again' });
    assert.equal(await button.eval('parsedHandlers'), 0, 'no handler on what Spindle reads of a page');
    await button.click('iframe#view');
    await printed(button, 'its own Print button clicked');
    // Focus in the page (a click on it that prints nothing), then Ctrl+P.
    const inPage = await tabOf('keys.html');
    await inPage.click('iframe#view');
    await new Promise((done) => setTimeout(done, 300));
    assert.ok(!(await inPage.eval('heard')).includes('printing'), 'a click in it prints nothing');
    await ctrlP(inPage);
    await printed(inPage, 'Ctrl+P pressed in the page');
    const inTab = await tabOf('keys.html');
    await inTab.click('#view-path');
    await ctrlP(inTab);
    await printed(inTab, 'Ctrl+P pressed in the tab around it');
    // Clicked in (Chrome asks "Leave site?" only of a page the user used), then left.
    const leave = await tabOf('leave.html');
    await leave.click('iframe#view');
    const dialogs = [];
    const watch = ({ method, params, sessionId }) => {
      if (sessionId !== leave.sessionId || method !== 'Page.javascriptDialogOpening') return;
      dialogs.push(params.type);
      browser.send('Page.handleJavaScriptDialog', { accept: true }, leave.sessionId).catch(() => {});
    };
    browser.on(watch);
    await browser.send('Page.enable', {}, leave.sessionId);
    await browser.send('Page.navigate', { url: 'about:blank' }, leave.sessionId);
    await leave.until(`location.href === 'about:blank'`, { what: 'the tab to leave the page' });
    await new Promise((done) => setTimeout(done, 500));
    browser.off(watch);
    assert.deepEqual(dialogs, [], 'no "Leave site?" (its listener, its handler and its body attribute all asked)');
    await browser.send('Target.closeTarget', { targetId: leave.targetId }).catch(() => {});

    const plain = await tabOf('plain.html');
    // The frame is a target of its own (a sandboxed frame in a process of its own, whose
    // parent is the tab) or in the tab's process: a world of the test's own made in it.
    let world = null;
    let tried = null;
    let attached = null;
    for (const until = Date.now() + 15_000; !world; await new Promise((done) => setTimeout(done, 200))) {
      const { targetInfos } = await browser.send('Target.getTargets');
      const target = targetInfos.find((info) => info.type === 'iframe' && info.parentId === plain.targetId && info.url.includes('preview.html'));
      let where = null;
      if (target) {
        if (attached?.targetId !== target.targetId) attached = { targetId: target.targetId, ...(await browser.send('Target.attachToTarget', { targetId: target.targetId, flatten: true })) };
        where = { frameId: target.targetId, sessionId: attached.sessionId };
      } else {
        const { frameTree } = await browser.send('Page.getFrameTree', {}, plain.sessionId);
        const child = frameTree.childFrames?.find((entry) => entry.frame.url.includes('preview.html'));
        if (child) where = { frameId: child.frame.id, sessionId: plain.sessionId };
      }
      tried = where;
      if (where) {
        const made = await browser.send('Page.createIsolatedWorld', { frameId: where.frameId, worldName: 'review-print' }, where.sessionId).catch((error) => ({ error: error.message }));
        const shown = made?.executionContextId && await browser.send('Runtime.evaluate', { contextId: made.executionContextId, expression: 'document.title', returnByValue: true }, where.sessionId).catch((error) => ({ error: error.message }));
        tried = { ...where, made, shown };
        if (shown?.result?.value === 'Plain') world = { contextId: made.executionContextId, sessionId: where.sessionId };
      }
      if (!world && Date.now() > until) throw new Error(`the plain page never showed in its frame: ${JSON.stringify(tried)}`);
    }
    const inWorld = async (expression) => (await browser.send('Runtime.evaluate', { contextId: world.contextId, expression, returnByValue: true }, world.sessionId)).result.value;
    await inWorld(`addEventListener('beforeprint', () => { document.documentElement.dataset.printed = 'yes'; }), true`);
    await plain.eval(`document.querySelector('#view-print').click()`);
    for (const until = Date.now() + 15_000; (await inWorld(`document.documentElement.dataset.printed ?? null`)) !== 'yes'; await new Promise((done) => setTimeout(done, 200))) {
      if (Date.now() > until) throw new Error('a page with no scripts did not print from the tab');
    }
    await browser.send('Target.closeTarget', { targetId: plain.targetId }).catch(() => {});
    const text = await open(browser, `chrome-extension://${extension}/view.html?thread=${reportThread.id}&path=data/used.json`);
    await text.until(`document.querySelector('#view.view-text, #view.view-bytes')`, { what: 'the data file as text' });
    assert.equal(await text.eval(`document.querySelector('#view-print').hidden`), true, 'no Print for text');
    await browser.send('Target.closeTarget', { targetId: text.targetId }).catch(() => {});
    console.log('ok a page prints only as the user asks (its own button, Ctrl+P in it or in its tab; never as it loads), asks no "Leave site?", and prints from the tab without scripts of its own; text has no Print');
  }

  // A model reads a workspace file open in Spindle's own tab (view.html) with read_page
  // (Chrome keeps scripts out of extension pages: the file itself is read then), finds
  // lines in it with `find`, and is told a screenshot of such a tab is impossible; `find`
  // on an ordinary page gives only the matching lines.
  {
    const shown = await open(browser, `chrome-extension://${extension}/view.html?thread=${reportThread.id}&path=plain.html`);
    await shown.until(`/frame /.test(document.querySelector('#view-path').title)`, { what: 'plain.html shown' });
    const shownId = await page.eval(`chrome.tabs.query({}).then((tabs) => tabs.find((tab) => (tab.url ?? '').includes('path=plain.html')).id)`);
    const plainPage = await open(browser, `http://127.0.0.1:${port}/page`);
    const plainId = await page.eval(`chrome.tabs.query({}).then((tabs) => tabs.find((tab) => tab.url === 'http://127.0.0.1:${port}/page').id)`);
    replies.push(
      { for: 'read the shown file', tool: 'read_page', input: { tab: shownId } },
      { for: 'read the shown file', tool: 'read_page', input: { tab: shownId, find: 'plain' } },
      { for: 'read the shown file', tool: 'read_page', input: { tab: shownId, find: '/nothing-here/' } },
      { for: 'read the shown file', tool: 'screenshot', input: { tab: shownId } },
      { for: 'read the shown file', tool: 'read_page', input: { tab: plainId, find: 'review' } },
      // A tab id in quotes, as some models write it, is that tab; anything else is refused
      // in words that say how to name one.
      { for: 'read the shown file', tool: 'read_page', input: { tab: String(plainId), find: 'review' } },
      { for: 'read the shown file', tool: 'read_page', input: { tab: 'the review page' } },
      { for: 'read the shown file', content: 'done' },
    );
    const reading = await rpc('create', { provider: 'review', runner: 'none', text: 'read the shown file' });
    await settled(reading.id);
    const read = await page.eval(`(async () => (await rpc('open', { id: ${JSON.stringify(reading.id)} })).history.messages.flatMap((m) => m.content ?? []).filter((b) => b.type === 'tool_result').map((b) => typeof b.content === 'string' ? b.content : JSON.stringify(b.content)))()`);
    assert.equal(read.length, 7, `seven tool results: ${read.join(' | ').slice(0, 800)}`);
    assert.match(read[0], /Workspace file: plain\.html \((as the preview shows it|the file itself)\)/, `the file's text, however read: ${read[0].slice(0, 300)}`);
    assert.match(read[0], /\nPlain/, `its text: ${read[0].slice(0, 300)}`);
    assert.doesNotMatch(read[0], /<title>/, 'no markup when the file itself is read');
    assert.match(read[1], /Workspace file: plain\.html[\s\S]*\nPlain/, `the matching line: ${read[1].slice(0, 300)}`);
    assert.match(read[2], /\[no line matches "\/nothing-here\/"\]/, `no match says so: ${read[2].slice(0, 300)}`);
    assert.match(read[3], /does not picture extension pages[\s\S]*read_page on it gives the file's text/, `a screenshot of Spindle's own tab is refused with what to do instead: ${read[3].slice(0, 300)}`);
    assert.equal(read[4], `Address: http://127.0.0.1:${port}/page\nTitle: Review page\n\n[1 matching line, each with its neighbours]\nReview`, 'only the matching line of an ordinary page');
    assert.equal(read[5], read[4], 'the tab id in quotes reads the same tab');
    assert.match(read[6], /tab is a tab's number from list_tabs, such as 2054624637, not "the review page"; nothing was done/, read[6].slice(0, 300));
    console.log('ok read_page reads a workspace file shown in Spindle\'s own tab and finds lines with `find`; a screenshot of that tab is refused; `find` on an ordinary page gives its matching lines; a tab id in quotes is that tab, any other non-number refused saying how to name one');
    await browser.send('Target.closeTarget', { targetId: shown.targetId }).catch(() => {});
    await plainPage.close();
  }

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
  // A stylesheet goes in as text that cannot end its element (a `</style>` in it would make
  // what follows markup: a script run under a choice made for other code), and its bytes
  // are part of the version; the version is told before any choice (the first one offered
  // names it), and a choice that gives a page more holds for the version it names only.
  const styled = await plain.eval(`(async () => {
    const { pageOf } = await import('./page.mjs');
    let css = 'p { color: red }</sTyle><script>parent.postMessage({ review: "injected" }, "*")<\/script><style>';
    const files = [{ path: 'styled.html', size: 0, digest: '' }, { path: 'style.css', size: 0, digest: '' }];
    const read = async () => new TextEncoder().encode(css);
    const html = '<!doctype html><link rel="stylesheet" href="style.css"><p>Styled</p><script>void 0<\/script>';
    const show = (allowed) => pageOf({ path: 'styled.html', files, read, html, allowed, contained: false });
    const first = await show(null);
    const run = await show({ scripts: true, version: first.version });
    const scripts = new DOMParser().parseFromString(run.html, 'text/html').querySelectorAll('script').length;
    const unnamed = await show({ scripts: true, version: null });
    const off = await show({ scripts: false, web: false });
    css = 'p { color: blue }';
    const restyled = await show({ scripts: true, version: first.version });
    return { version: typeof first.version, run: run.mode, scripts, unnamed: unnamed.mode, off: off.mode, restyled: [restyled.mode, restyled.lapsed] };
  })()`);
  assert.deepEqual(styled, { version: 'string', run: 'scripts', scripts: 2, unnamed: 'static', off: 'static', restyled: ['static', 'scripts'] },
    'a stylesheet cannot add a script, and a choice holds for the version (stylesheets too) it names');
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

  // Documents. The reader first, in an extension page: what each document comes to.
  const fixtures = officeFixtures();
  const encoded = Object.fromEntries(Object.entries(fixtures).map(([name, bytes]) => [name, Buffer.from(bytes).toString('base64')]));
  const drawn = await page.eval(`(async () => {
    const { documentPage } = await import('./documents.mjs');
    const out = {};
    const decoded = (text) => Uint8Array.from(atob(text), (c) => c.charCodeAt(0));
    for (const name of ['plan.docx', 'budget.xlsx', 'deck.pptx']) out[name] = await documentPage(name, decoded(${JSON.stringify(encoded)}[name]));
    out.damaged = await documentPage('damaged.docx', new Uint8Array([80, 75, 3, 4, 1, 2, 3])).catch((error) => error.message);
    out.renamed = await documentPage('notes.docx', new TextEncoder().encode('just text')).catch((error) => error.message);
    return out;
  })()`);
  const plan = drawn['plan.docx'].html;
  assert.match(plan, /<title>Quarterly &amp; Plan<\/title>/, 'its title from its properties');
  assert.match(plan, /<h1 class="title">Quarterly Plan<\/h1><h1>Goals &lt;2026&gt;<\/h1>/, 'its title and heading styles');
  assert.match(plan, /<b>bold<\/b> and <i><span style="color:#c00000">red italic<\/span><\/i>/, 'its runs formatted');
  assert.match(plan, /<span class="mark">1\.<\/span>First step.*<span class="mark">1\.a\)<\/span>Detail.*<span class="mark">2\.<\/span>Second step.*<span class="mark">•<\/span>A bullet/s, 'its lists numbered as Word numbers them');
  assert.match(plan, /<span class="mark">I\)<\/span>Styled item/, 'a list numbered through its list style');
  assert.match(plan, /<s>struck<\/s>/, 'struck through when either strike is on');
  assert.match(plan, /<span class="link" title="https:\/\/example\.com\/a\?b=1&amp;c=2">the site<\/span> on page 1/, 'a link shown, not followed; a field shows its result');
  assert.doesNotMatch(plan, /deleted words|hidden words|PAGE|<a /, 'no deleted or hidden text, no field code, no link to follow');
  assert.match(plan, /<td colspan="2" style="background:#d9e2f3"><p>Wide head<\/p><\/td><td rowspan="2"><p>Tall<\/p><\/td>/, 'merged cells');
  assert.match(plan, /<img src="data:image\/png;base64,[^"]+" alt="A square" style="width:100px">/, 'its picture put in');
  assert.match(plan, /<section class="notes"><div class="note"><sup>1<\/sup><p> A footnote\.<\/p><\/div><div class="note"><sup>i<\/sup><p> An endnote\.<\/p><\/div><\/section>/, 'its footnote and endnote at the end, numbered apart');
  assert.match(plan, /<span class="mark">3\.<\/span>Third step/, "a numbering of the same list goes on with the list's numbers");
  assert.doesNotMatch(plan, /secret words/, 'text its paragraph style hides stays hidden');
  assert.match(plan, /<ruby>漢字<rt>kanji<\/rt><\/ruby> odd   spaced<sup class="ref">i<\/sup>/, 'a reading over its base text, spaces kept, an endnote mark');
  assert.doesNotMatch(plan, /function|native code/, 'a name a map does not hold (constructor) gives nothing');
  assert.match(plan, /<span class="crop" style="width:100px;aspect-ratio:100\/50"><img src="data:image\/png;base64,[^"]+" alt="Half" style="position:absolute;max-width:none;left:0\.000%;top:0\.000%;width:100\.000%;height:200\.000%"><\/span>/, 'a cropped picture shows only what it keeps');
  const budget = drawn['budget.xlsx'].html;
  assert.doesNotMatch(budget, /Secret|hush/, 'a very hidden sheet stays hidden');
  assert.match(budget, /Notes <small>\(hidden\)<\/small>/, 'a hidden sheet is marked');
  assert.match(budget, /<td>Rent &amp; power<\/td><td class="num">\$1,234\.50<\/td><td class="num">2026-01-01<\/td><td class="num">12\.50%<\/td>/, 'strings, money, dates and percentages as formatted');
  assert.match(budget, /<td colspan="4"><\/td>/, 'merged cells');
  assert.match(budget, /<td>TRUE<\/td><td class="num">0\.3<\/td>/, 'a boolean, and a sum without float noise');
  assert.match(budget, /<th>5<\/th><td class="num">1\.5<\/td><td class="num">1\.23E\+03<\/td>/, 'a fraction as the number it is, a scientific number as Excel writes it');
  assert.match(budget, /<th>6<\/th><td class="num">1\.5M<\/td><td class="num">1,234\.50 €<\/td><td class="num">36:00:00<\/td><td>=SUM\(B2:B4\)<\/td><td>Line\ntwo<\/td>/, 'thousands scaled, a currency symbol kept, elapsed hours, a formula saved without its result, an escaped line break read');
  assert.match(budget, /<th>7<\/th><td class="num">02134<\/td><td class="num">123-45-6789<\/td><td class="num">1,234\.50р\.<\/td><td class="num">Rs\. 1,234<\/td><td class="num">500<\/td>/, 'zeros padded, digits among literals, words around the number, a condition no section meets');
  assert.match(budget, /<th>8<\/th><td class="num">1\.5K<\/td><td><\/td><td><\/td><td>2026-03-04<\/td><td class="num">1900-02-29<\/td>/, "a condition met, ;;; hiding, a formula's empty text, a date held as text, Excel's own 29 February 1900");
  assert.match(budget, /<th>9<\/th><td class="num">90<\/td><td class="num">2\.5M<\/td>/, 'elapsed seconds, the first condition met');
  assert.match(budget, /<h2 id="sheet-3">Huge<\/h2><p class="missing">Not shown: this sheet is too large for a preview; download the file to open it\.<\/p>/, 'a sheet too large to parse says so, and the others show');
  const deck = drawn['deck.pptx'].html;
  assert.match(deck, /Slide 1<\/figcaption>.*justify-content:flex-end"><p style="font-size:6\.250cqw;color:#0e2841;text-align:center">Launch &lt;Day&gt;<\/p>/s, 'a title sized, placed, colored and centered by its layout, master and theme');
  assert.match(deck, /<p style="font-size:2\.500cqw;text-align:center">A subtitle<\/p>/, 'a subtitle unbulleted and centered, as its layout says');
  assert.match(deck, /background:#c1e5f5/, 'a theme color lightened');
  assert.match(deck, /background:#800040/, 'a color just below red darkened in its own hue');
  assert.match(deck, /background:rgba\(0,0,0,0\.400\)/, 'a see-through fill as see-through');
  assert.match(deck, /<div class="shape crop" style="left:30\.000%;top:87\.489%;width:5\.000%;height:10\.000%"><img src="data:image\/png;base64,[^"]+" alt="cut" style="position:absolute;max-width:none;left:-50\.000%;top:0\.000%;width:200\.000%;height:100\.000%"><\/div>/, 'a cropped picture on a slide');
  assert.match(deck, /<span class="mark">•<\/span>Point one.*<span class="mark">–<\/span><span style="font-weight:bold">Sub point<\/span>/s, "bullets as the master's levels set them");
  assert.match(deck, /<img class="shape picture" style="left:82\.021%;top:72\.908%;width:8\.202%;height:14\.582%" src="data:image\/png;base64,/, 'a picture placed');
  assert.doesNotMatch(deck, /invisible shape/, 'a hidden shape stays hidden');
  assert.match(deck, /<p style="font-size:3\.750cqw"><span class="mark">•<\/span>Big<span style="font-size:2\.917cqw"> then normal<\/span><\/p>/, 'a paragraph sized by its first run, another run by its own');
  assert.match(deck, /<img class="shape picture" style="left:50\.000%;top:26\.620%;width:43\.125%;height:63\.449%" src="data:image\/png;base64,[^"]+" alt="placed">/, 'a picture placeholder placed by its layout');
  // Numbers out of every bound: each document still draws at once, its numbers bounded.
  const unruly = await page.eval(`(async () => {
    const { documentPage } = await import('./documents.mjs');
    const out = {};
    for (const [name, text] of Object.entries(${JSON.stringify(Object.fromEntries(Object.entries(hostileFixtures()).map(([name, bytes]) => [name, Buffer.from(bytes).toString('base64')])))})) {
      const started = performance.now();
      const drawn = await documentPage(name, Uint8Array.from(atob(text), (c) => c.charCodeAt(0))).catch((error) => ({ html: 'failed: ' + error.message }));
      out[name] = { html: drawn.html, ms: performance.now() - started };
    }
    return out;
  })()`);
  for (const [name, { ms }] of Object.entries(unruly)) assert.ok(ms < 15000, `${name} drew in ${ms} ms`);
  assert.match(unruly['hostile.docx'].html, /<p class="li" style="margin-left:1\.6em"><span class="mark">g{1261}\)<\/span>Bounded item<\/p>/, 'a start beyond 32767 bounded, an outline level below zero no heading');
  assert.match(unruly['hostile.docx'].html, /<td colspan="1000"><p>wide<\/p><\/td>/, 'a span bounded');
  assert.match(unruly['hostile.xlsx'].html, /<td class="num">1\.2340000000000000000\dE\+03<\/td><td class="num">0<\/td>/, 'decimals and thousands bounded');
  assert.match(unruly['hostile.pptx'].html, /<p style="font-size:[\d.]+cqw"><span class="mark">32767\.<\/span>Low level<\/p>/, 'a level below zero read as the first, a start bounded');
  for (const [name, { notes }] of Object.entries(drawn).filter(([, value]) => value.html)) {
    assert.deepEqual(notes, name === 'budget.xlsx' ? ['Some formulas were saved without their results: they show as formulas.'] : [], `what ${name} leaves out, said`);
  }
  assert.match(drawn.damaged, /zip|damaged|cut short/, `a damaged document says so: ${drawn.damaged}`);
  assert.match(drawn.renamed, /not a zip file/, `a file that is no document says so: ${drawn.renamed}`);
  console.log('ok Word, Excel and PowerPoint documents are drawn as pages: text, styles, lists, tables, sheets, slides, pictures; nothing hidden shown');

  // Then each opened in a tab of its own (view.html), from a thread's workspace.
  replies.push({ for: 'documents', content: 'ok' });
  const docThread = await page.eval(`(async () => {
    const files = Object.entries(${JSON.stringify(encoded)}).map(([path, text]) => ({ path, bytes: Uint8Array.from(atob(text), (c) => c.charCodeAt(0)) }));
    return rpc('create', { provider: 'review', runner: 'none', text: 'documents', files });
  })()`);
  await settled(docThread.id);
  /** What the preview frame `tab` shows says (its text, its policy, whether it holds an
   * SVG): a sandboxed frame, in a process of its own (a target whose parent is the tab), or
   * else in the tab's (read in a world of its own). */
  const frames = new Map();
  const frameText = async (tab) => {
    const expression = `JSON.stringify({ text: document.body?.innerText ?? '', policy: document.querySelector('meta[http-equiv="Content-Security-Policy"]')?.content ?? null, svg: Boolean(document.querySelector('svg')) })`;
    const { targetInfos } = await browser.send('Target.getTargets');
    const target = targetInfos.find((info) => info.type === 'iframe' && info.parentId === tab.targetId && info.url.includes('preview.html'));
    if (target) {
      if (!frames.has(target.targetId)) frames.set(target.targetId, await attach(browser, target.targetId));
      return JSON.parse(await frames.get(target.targetId).eval(expression));
    }
    const { frameTree } = await browser.send('Page.getFrameTree', {}, tab.sessionId);
    const frame = (frameTree.childFrames ?? []).find((child) => child.frame.url.includes('preview.html'))?.frame;
    if (!frame) return null;
    const { executionContextId } = await browser.send('Page.createIsolatedWorld', { frameId: frame.id, worldName: 'review' }, tab.sessionId);
    const { result } = await browser.send('Runtime.evaluate', { contextId: executionContextId, returnByValue: true, expression }, tab.sessionId);
    return JSON.parse(result.value);
  };
  const viewOf = (path) => open(browser, `chrome-extension://${extension}/view.html?thread=${docThread.id}&path=${encodeURIComponent(path)}`);
  /** A picture of `tab` at a side panel's width, with SPINDLE_SHOTS. */
  const docShot = async (tab, name) => {
    if (!shots) return;
    fs.mkdirSync(shots, { recursive: true });
    // In front: a tab behind others may never draw a frame to picture.
    await tab.activate();
    await tab.size(420, 820);
    await new Promise((done) => setTimeout(done, 1200));
    await tab.screenshot(join(shots, name));
  };
  // A PDF's tab becomes Chrome's own viewer of it, served by Spindle's worker.
  const pdfTab = await viewOf('report.pdf');
  await pdfTab.until(`(location.pathname === '/file/${docThread.id}/report.pdf' && document.contentType === 'application/pdf') || null`, { what: "the tab to become the PDF in Chrome's viewer", timeout: 30_000 });
  await docShot(pdfTab, 'doc-report-pdf.png');
  for (const [path, words] of [['plan.docx', /Quarterly Plan[\s\S]*Second step[\s\S]*A footnote\./], ['budget.xlsx', /Rent & power\s+\$1,234\.50/], ['deck.pptx', /Launch <Day>[\s\S]*Agenda/]]) {
    const tab = await viewOf(path);
    const shown = await tab.until(`document.querySelector('iframe#view')?.src.includes('preview.html') || null`, { what: `${path} drawn` })
      .then(async () => { for (const until = Date.now() + 15_000; Date.now() < until; await new Promise((done) => setTimeout(done, 200))) { const seen = await frameText(tab); if (seen?.text) return seen; } return null; });
    assert.ok(shown, `${path} shows in its frame`);
    assert.match(shown.text, words, `${path} reads as it should: ${shown.text.slice(0, 200)}`);
    assert.match(shown.policy ?? '', /script-src 'none'/, `${path} shows with nothing running`);
    await docShot(tab, `doc-${path.replace('.', '-')}.png`);
    await browser.send('Target.closeTarget', { targetId: tab.targetId });
  }
  const svgTab = await viewOf('drawing.svg');
  await svgTab.until(`document.querySelector('#view img')?.naturalWidth > 0 || null`, { what: 'an SVG written without its namespace drawn as a picture' });
  const looseTab = await viewOf('loose.svg');
  await looseTab.until(`document.querySelector('iframe#view')?.src.includes('preview.html') || null`, { what: 'an SVG that is no XML drawn as a page' });
  for (const until = Date.now() + 15_000; !(await frameText(looseTab))?.svg;) {
    assert.ok(Date.now() < until, 'the loose SVG drawn in its frame');
    await new Promise((done) => setTimeout(done, 200));
  }
  const soundTab = await viewOf('tone.wav');
  await soundTab.until(`document.querySelector('#view audio')?.readyState >= 1 || null`, { what: 'the WAV ready to play' });
  await docShot(svgTab, 'doc-drawing-svg.png');
  await docShot(looseTab, 'doc-loose-svg.png');
  await docShot(soundTab, 'doc-tone-wav.png');
  for (const tab of [pdfTab, svgTab, looseTab, soundTab]) await browser.send('Target.closeTarget', { targetId: tab.targetId });
  console.log("ok in a tab: a PDF as Chrome's own viewer (from Spindle's worker), documents drawn with nothing running, an SVG without its namespace, a loose SVG as a page, a WAV to play");

  // And on the workspace page of the panel.
  await page.activate();
  await page.eval(`document.querySelector('#home').click()`);
  await page.click(`#threads .thread-row[data-id="${docThread.id}"]`);
  await page.until(`spindlePanel.current === '${docThread.id}' && spindlePanel.status`);
  await page.click('#title-button');
  await page.click('#menu-files');
  await page.until(`document.querySelector('#file-list .file-row[data-path="report.pdf"]') && true`, { what: 'the workspace page' });
  const shownOn = async (path, selector, ready = `document.querySelector('#ws-preview ${selector}') && true`) => {
    await page.eval(`document.querySelector('#file-list .file-row[data-path=' + CSS.escape(${JSON.stringify(path)}) + '] .file-open').click()`);
    await page.until(ready, { what: `${path} on the workspace page` });
    const tab = await page.eval(`[...document.querySelectorAll('#ws-actions button')].some((button) => /in a tab/.test(button.title))`);
    await page.eval(`document.querySelector('#ws-files').click()`);
    return tab;
  };
  assert.ok(await shownOn('report.pdf', 'button.file-open-tab'), 'a PDF is offered in a tab (never in the panel), from the page and its header');
  assert.ok(await shownOn('plan.docx', 'iframe.file-frame[src*="preview.html"]'), 'a document opens in a tab too');
  // The SVG written without its namespace draws here too (as a picture, not broken).
  assert.ok(await shownOn('drawing.svg', 'img.file-picture', `document.querySelector('#ws-preview img.file-picture')?.naturalWidth > 0 || null`), 'a picture opens in a tab too');
  assert.ok(await shownOn('tone.wav', 'audio.file-media'), 'audio opens in a tab too');
  await page.eval(`document.querySelector('#ws-done').click()`);
  console.log('ok on the workspace page: the same previews (a PDF offered in a tab), each offered in a tab');

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
