// The workspace as a tree, in real Chrome against a scripted provider: a thread makes a
// Python environment of a few thousand files (`.venv/lib/python3.14/site-packages/...`)
// in the browser Linux, then a second turn adds a report and edits two files. It checks:
//
//   - every folder starts shut, in the chat's workspace card and on the workspace page,
//     and nothing inside a shut folder is drawn; a folder that only holds a folder shares
//     its row (`lib/python3.14/site-packages`);
//   - folders count their files and say what the last turn made or changed (deep ones
//     too), so changes are found without opening anything;
//   - a folder opens and shuts on a click (aria-expanded), and stays as the user left it
//     through the scratch switch, a filter and a turn's redraw; a filter lists matches with
//     their folders; next and previous walk files in shut folders too;
//   - how long the list takes to draw (scratch files shown: every file listed).
//
// Screenshots (Light and Dark, the card and the page) go to SHOTS_DIR.
//
//   SPINDLE_BROWSER=/path/to/chrome node chrome/tests/panel_tree.mjs DIST [SHOTS_DIR]
//
// TREE_MEASURE_ONLY=1 only times the draw (for a build from before the tree).
import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { launch, open } from './cdp.mjs';

const [distArg, shotsArg] = process.argv.slice(2);
const browserBinary = process.env.SPINDLE_BROWSER;
const measureOnly = process.env.TREE_MEASURE_ONLY === '1';
if (!distArg || !browserBinary) throw new Error('usage: SPINDLE_BROWSER=... node chrome/tests/panel_tree.mjs DIST [SHOTS_DIR]');
const work = fs.mkdtempSync(join(tmpdir(), 'spindle-tree-'));
let browser = null;
process.on('exit', () => {
  browser?.child.kill('SIGKILL');
  fs.rmSync(work, { recursive: true, force: true });
});
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => process.exit(130));
const shots = resolve(shotsArg ?? join(work, 'shots'));
fs.mkdirSync(shots, { recursive: true });

// ---- The scripted provider.

const replies = [];
let seen = 0;
const server = http.createServer(async (request, response) => {
  if (request.method === 'POST' && request.url === '/v1/chat/completions') {
    for await (const _ of request);
    seen += 1;
    const reply = replies.shift() ?? { content: '(script ended)' };
    const deltas = [
      ...(reply.content != null ? [{ content: reply.content }] : []),
      ...(reply.call ? [{ tool_calls: [{ index: 0, id: `call_${seen}`, type: 'function', function: { name: reply.call[0], arguments: JSON.stringify(reply.call[1]) } }] }] : []),
    ];
    const chunks = [
      ...deltas.map((delta) => ({ id: 'c', object: 'chat.completion.chunk', choices: [{ index: 0, delta, finish_reason: null }] })),
      { id: 'c', object: 'chat.completion.chunk', choices: [{ index: 0, delta: {}, finish_reason: reply.call ? 'tool_calls' : 'stop' }] },
      { id: 'c', object: 'chat.completion.chunk', choices: [], usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 } },
    ];
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    response.end(`${chunks.map((chunk) => `data: ${JSON.stringify(chunk)}\n\n`).join('')}data: [DONE]\n\n`);
    return;
  }
  response.writeHead(404).end();
});
const port = await new Promise((done) => server.listen(0, '127.0.0.1', () => done(server.address().port)));
const call = (name, input) => ({ call: [name, input] });
const catalogFile = join(work, 'providers.json');
fs.writeFileSync(catalogFile, JSON.stringify({
  version: 4, active_provider: 'scripted', providers: [{
    id: 'scripted', display_name: 'Scripted', api_provider: 'openai', request_contract: 'openai-chat-completions',
    base_url: `http://127.0.0.1:${port}/v1`, default_model: 'deepseek-flash', models: ['deepseek-flash'], requires_api_key: true,
    model_specs: { 'deepseek-flash': { context_window: 400000, max_output_tokens: 4096 } },
  }],
}));

// A virtual environment as pip leaves one: 100 packages of 40 modules, their data and
// their dist-info, beside the project's own files.
const MAKE_VENV = `python3 - <<'EOF'
import os
site = '.venv/lib/python3.14/site-packages'
def put(path, text):
    os.makedirs(os.path.dirname(path) or '.', exist_ok=True)
    open(path, 'w').write(text)
put('.venv/pyvenv.cfg', 'home = /usr/bin\\nversion = 3.14.0\\n')
put('.venv/bin/activate', '# activate\\n')
for p in range(100):
    name = f'pkg{p:02}'
    for m in range(30):
        put(f'{site}/{name}/mod{m:02}.py', f'VALUE = {m}\\n')
    put(f'{site}/{name}/__init__.py', '')
    put(f'{site}/{name}/py.typed', '')
    for d in range(8):
        put(f'{site}/{name}/data/table{d}.json', '{"rows": []}\\n')
    for meta in ('METADATA', 'RECORD', 'WHEEL', 'INSTALLER'):
        put(f'{site}/{name}-1.0.dist-info/{meta}', f'Name: {name}\\n')
put(f'{site}/pip-26.2.2.dist-info/METADATA', 'Name: pip\\n')
# Compiled single-file modules, as real environments have (not scratch files):
# site-packages then holds more than a list draws at once.
for s in range(210):
    put(f'{site}/native{s:03}.so', '')
put('data/table.csv', 'ticker,close\\nNVDA,131.20\\n')
print('made')
EOF`;

// The extension, its sites given up front (a prompt cannot be clicked headless).
const dist = join(work, 'extension');
fs.cpSync(resolve(distArg), dist, { recursive: true });
const manifest = JSON.parse(fs.readFileSync(join(dist, 'manifest.json'), 'utf8'));
fs.writeFileSync(join(dist, 'manifest.json'), JSON.stringify({ ...manifest, host_permissions: manifest.optional_host_permissions }, null, 2));

const results = [];
function check(name, run) {
  try {
    run();
    results.push([name, true]);
    console.log(`ok   ${name}`);
  } catch (error) {
    results.push([name, false]);
    console.log(`FAIL ${name}\n     ${error.message.split('\n').join('\n     ')}`);
  }
}
const say = (text) => console.log(`...  ${text}`);

say('Chrome');
browser = await launch(browserBinary, { profile: join(work, 'profile'), downloads: join(work, 'downloads') });
const extensionId = (await browser.send('Extensions.loadUnpacked', { path: dist })).id;
const panel = await open(browser, `chrome-extension://${extensionId}/panel.html`, { newWindow: true });
const panelSaid = [];
browser.on(({ method, params, sessionId }) => {
  if (sessionId !== panel.sessionId) return;
  if (method === 'Runtime.consoleAPICalled' && params.type === 'error') panelSaid.push(`error: ${params.args.map((arg) => arg.value ?? arg.description ?? '').join(' ')}`);
  else if (method === 'Runtime.exceptionThrown') panelSaid.push(`exception: ${params.exceptionDetails.exception?.description ?? params.exceptionDetails.text}`);
});
const send = (method, params = {}) => browser.send(method, params, panel.sessionId);
await panel.size(420, 900);
await panel.until('globalThis.spindlePanel?.engine', { timeout: 60_000, what: 'the engine' });
say('the provider');
await panel.files('#catalog-file', [catalogFile]);
await panel.until(`spindlePanel.engine.catalog.providers.some((p) => p.id === 'scripted') && spindlePanel.engine.catalogSaved`, { what: 'the catalog' });
await panel.eval(`document.querySelector('#settings-dialog').open || document.querySelector('#settings').click()`);
const row = '.provider[data-provider="scripted"]';
await panel.click(`${row} .provider-actions button`);
await panel.type(`${row} input`, `sk-scripted-${randomBytes(8).toString('hex')}`);
await panel.click(`${row} button.primary`);
await panel.until(`document.querySelector('${row} .provider-state')?.textContent === 'key saved'`, { what: 'the key' });
await panel.click('#settings-dialog [data-close]');
await panel.click('#model-chip');
await panel.click('#model-provider .picker-field');
await panel.click('#model-provider [role="option"][data-value="scripted"]');
await panel.click('#model-apply');

/** One turn: `text` sent, `steps` answered, until the thread is idle again. */
async function turn(text, steps) {
  replies.length = 0;
  replies.push(...steps);
  const asked = seen + steps.length;
  await panel.type('#prompt', text);
  await panel.click('#send');
  // Every reply asked for, then the thread at rest.
  for (const started = Date.now(); seen < asked; await new Promise((done) => setTimeout(done, 200))) {
    if (Date.now() - started > 300_000) throw new Error(`the turn "${text}" asked for ${seen} of ${asked} replies`);
  }
  const thread = await panel.eval('spindlePanel.current');
  await panel.until(`(() => { const s = spindlePanel.threads.get(${JSON.stringify(thread)})?.state; return s && s !== 'running' ? s : null; })()`, { timeout: 60_000, what: `the turn "${text}" to end` });
  await panel.until(`document.querySelector('#history .workspace-card .ws-tree') || null`, { timeout: 30_000, what: 'the workspace card' });
  return thread;
}

say('turn 1: a virtual environment');
await turn('Set up the environment.', [call('bash', { command: MAKE_VENV }), { content: 'Made the environment.' }]);
say('turn 2: a report and two edits');
await turn('Write the report.', [
  call('write_file', { path: 'report.html', content: '<!doctype html><title>Report</title><h1>Report</h1>' }),
  call('write_file', { path: 'data/table.csv', content: 'ticker,close\nNVDA,131.20\nTSM,188.05\n' }),
  call('bash', { command: 'echo "Summary: pip" >> .venv/lib/python3.14/site-packages/pip-26.2.2.dist-info/METADATA' }),
  { content: 'The report is in report.html.' },
]);

// ---- The draw, timed: the workspace page's list drawn again (scratch files shown, so
// every file is listed), style and layout included.
await panel.click('#history .workspace-card > summary');
await panel.click('#history .workspace-card .ws-tools .link-like');
await panel.until(`document.body.dataset.view === 'workspace' && document.querySelector('#file-list .file-row, #file-list .ws-folder-head') || null`, { what: 'the workspace page' });
await panel.eval(`(() => { const toggle = document.querySelector('#ws-scratch'); if (toggle.getAttribute('aria-checked') !== 'true') toggle.click(); })()`);
const timing = await panel.eval(`(() => {
  const filter = document.querySelector('#ws-filter');
  const runs = [];
  for (let i = 0; i < 7; i++) {
    const started = performance.now();
    filter.dispatchEvent(new Event('input'));
    document.body.offsetHeight;
    runs.push(performance.now() - started);
  }
  runs.sort((a, b) => a - b);
  return { files: Number(/\\d+/.exec(document.querySelector('#ws-sub').textContent)?.[0]), median: runs[3], max: runs[6], nodes: document.querySelectorAll('#file-list *').length };
})()`);
console.log(`draw: ${timing.files} files, median ${timing.median.toFixed(1)} ms, slowest ${timing.max.toFixed(1)} ms, ${timing.nodes} elements in the list`);
if (measureOnly) {
  await panel.screenshot(join(shots, 'before-workspace.png'));
  process.exit(0);
}
await panel.eval(`(() => { const toggle = document.querySelector('#ws-scratch'); if (toggle.getAttribute('aria-checked') === 'true') toggle.click(); })()`);

// ---- The tree.

const heads = (scope) => `[...document.querySelectorAll('${scope} .ws-folder-head')].filter((h) => h.offsetParent).map((h) => ({ path: h.dataset.folder, name: h.querySelector('.ws-folder-name').textContent, open: h.getAttribute('aria-expanded'), badges: [...h.querySelectorAll('.badge')].map((b) => b.textContent), count: h.querySelector('small').textContent }))`;
const rowsIn = (scope) => `[...document.querySelectorAll('${scope} .file-row')].filter((r) => r.offsetParent).map((r) => r.dataset.path + (r.querySelector('.badge') ? ':' + r.querySelector('.badge').textContent : ''))`;
const page1 = await panel.eval(`({ heads: ${heads('#file-list')}, rows: ${rowsIn('#file-list')}, drawn: document.querySelectorAll('#file-list .file-row').length })`);
await panel.click('#file-list .ws-folder-head[data-folder=".venv"]');
const venv = await panel.eval(`({ heads: ${heads('#file-list')}, rows: ${rowsIn('#file-list')} })`);
await panel.click('#file-list .ws-folder-head[data-folder=".venv/lib/python3.14/site-packages"]');
const site = await panel.eval(`({ heads: ${heads('#file-list')}.length, more: document.querySelector('#file-list .ws-more')?.textContent ?? null })`);
// "Show more" draws the next rows and puts the focus on the first of them.
const shown = site.more
  ? await panel.eval(`(() => { const more = document.querySelector('#file-list .ws-more'); more.focus(); more.click(); const at = document.activeElement; return { heads: ${heads('#file-list')}.length, focus: at?.classList.contains('ws-folder-head') || at?.classList.contains('file-open') || false, left: document.querySelector('#file-list .ws-more')?.textContent ?? null }; })()`)
  : null;
await panel.click('#file-list .ws-folder-head[data-folder=".venv/lib/python3.14/site-packages/pip-26.2.2.dist-info"]');
const pip = await panel.eval(`${rowsIn('#file-list')}.filter((r) => r.includes('pip-26'))`);
// The scratch switch, a filter and its clearing leave the open folders open.
await panel.click('#ws-scratch');
await panel.click('#ws-scratch');
await panel.type('#ws-filter', 'METADATA');
const filtered = await panel.eval(`({ rows: ${rowsIn('#file-list')}.length, heads: ${heads('#file-list')}.length, where: document.querySelector('#file-list .file-row-where')?.textContent ?? null, first: document.querySelector('#file-list .file-row')?.dataset.path })`);
await panel.eval(`(() => { const f = document.querySelector('#ws-filter'); f.value = ''; f.dispatchEvent(new Event('input')); })()`);
const kept = await panel.eval(`${heads('#file-list')}.filter((h) => h.open === 'true').map((h) => h.path)`);
// Next and previous walk every file, folders shut or not: from report.html, the one after
// is the environment's own first file (its folder shut again first).
await panel.click('#file-list .ws-folder-head[data-folder=".venv"]');
await panel.click('#file-list .file-row[data-path="report.html"] .file-open');
await panel.until(`!document.querySelector('#ws-file').hidden || null`, { what: 'report.html open' });
await panel.eval(`document.querySelector('#ws-next').click()`);
const after = await panel.eval(`document.querySelector('#ws-path').textContent`);
await panel.eval(`document.querySelector('#ws-files').click()`);
// Back on the list, the file's folder still shut: the focus is on the row that holds it.
const back = await panel.eval(`document.activeElement?.dataset?.folder ?? document.activeElement?.tagName ?? null`);
const keyboard = await panel.eval(`(() => { const head = document.querySelector('#file-list .ws-folder-head[data-folder="data"]'); return head.tagName === 'BUTTON' && head.type === 'button'; })()`);

// The card in the conversation, shut folders and all.
await panel.eval(`document.querySelector('#ws-done').click()`);
await panel.until(`document.body.dataset.view !== 'workspace' || null`, { what: 'the conversation' });
const card = await panel.eval(`({ heads: ${heads('#history .workspace-card')}, rows: ${rowsIn('#history .workspace-card')}, drawn: document.querySelectorAll('#history .workspace-card .file-row').length })`);

check('every folder starts shut; nothing inside a shut folder is drawn; root files first', () => {
  assert.deepEqual(page1.heads.map((h) => [h.name, h.open]), [['.venv', 'false'], ['data', 'false']], JSON.stringify(page1.heads));
  assert.deepEqual(page1.rows, ['report.html:new'], JSON.stringify(page1));
  assert.equal(page1.drawn, 1, JSON.stringify(page1));
  assert.deepEqual(card.heads.map((h) => [h.name, h.open]), [['.venv', 'false'], ['data', 'false']], JSON.stringify(card));
  assert.equal(card.drawn, 1, JSON.stringify(card));
});
check('folders count their files and carry what the last turn changed, however deep', () => {
  const [dotVenv, data] = page1.heads;
  assert.deepEqual(data.badges, ['1 edited'], JSON.stringify(data));
  assert.equal(data.count, '1 file');
  assert.deepEqual(dotVenv.badges, ['1 edited'], JSON.stringify(dotVenv));
  assert.match(dotVenv.count, /^\d+ files$/);
});
check('a folder holding only a folder shares its row; a click opens one level', () => {
  const chain = venv.heads.find((h) => h.path === '.venv/lib/python3.14/site-packages');
  assert.ok(chain && chain.name === 'lib/python3.14/site-packages' && chain.open === 'false', JSON.stringify(venv.heads));
  assert.ok(venv.heads.find((h) => h.name === '.venv').open === 'true', JSON.stringify(venv.heads));
  assert.ok(venv.heads.some((h) => h.name === 'bin'), JSON.stringify(venv.heads));
  assert.ok(venv.rows.includes('.venv/pyvenv.cfg'), JSON.stringify(venv.rows));
  assert.ok((shown?.heads ?? site.heads) >= 200, `site-packages shows its folders: ${JSON.stringify([site, shown])}`);
  assert.deepEqual(pip, ['.venv/lib/python3.14/site-packages/pip-26.2.2.dist-info/METADATA:edited'], JSON.stringify(pip));
});
check('a filter lists matches with their folders; cleared, the open folders are as they were', () => {
  assert.equal(filtered.heads, 0, JSON.stringify(filtered));
  assert.equal(filtered.rows, 101, JSON.stringify(filtered));
  assert.match(filtered.where ?? '', /^\.venv\/lib\/python3\.14\/site-packages\//, JSON.stringify(filtered));
  assert.deepEqual(kept.sort(), ['.venv', '.venv/lib/python3.14/site-packages', '.venv/lib/python3.14/site-packages/pip-26.2.2.dist-info'], JSON.stringify(kept));
});
check('next walks into shut folders; folder rows are buttons; back on the list, the focus is on its folder', () => {
  assert.match(after, /\.venv.*pyvenv\.cfg/, after);
  assert.ok(keyboard);
  assert.equal(back, '.venv', 'the shut folder holding the file just shown has the focus');
});
check('a long folder draws a few hundred rows at a time; "Show more" moves the focus to the next', () => {
  assert.match(site.more ?? '', /^Show \d+ more of \d+$/, JSON.stringify(site));
  assert.ok(shown && shown.heads > site.heads && shown.focus, JSON.stringify(shown));
});
check('nothing on the panel console', () => assert.deepEqual(panelSaid, []));

// ---- Screenshots: the card with a folder open, the page with the environment open.
for (const mode of ['light', 'dark']) {
  await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: mode }] });
  await panel.eval(`spindleTheme.set(${JSON.stringify(mode)}); spindleTheme.setTexture('plain')`);
  await panel.eval(`(() => { const card = document.querySelector('#history .workspace-card'); card.open = true; card.scrollIntoView({ block: 'start' }); })()`);
  await new Promise((done) => setTimeout(done, 300));
  await panel.screenshot(join(shots, `${mode}-card.png`));
  await panel.click('#history .workspace-card .ws-tools .link-like');
  await panel.until(`document.body.dataset.view === 'workspace' && document.querySelector('#file-list .ws-folder-head') || null`, { what: 'the workspace page' });
  await new Promise((done) => setTimeout(done, 300));
  await panel.screenshot(join(shots, `${mode}-workspace.png`));
  // Opened by hand, a level at a time, then shut again.
  const OPENED = ['.venv', '.venv/lib/python3.14/site-packages', '.venv/lib/python3.14/site-packages/pip-26.2.2.dist-info'];
  const setOpen = (on) => panel.eval(`for (const path of ${JSON.stringify(on ? OPENED : [...OPENED].reverse())}) { const head = document.querySelector('#file-list .ws-folder-head[data-folder="' + path + '"]'); if (head && head.getAttribute('aria-expanded') !== '${on}') head.click(); }`);
  await setOpen(true);
  await new Promise((done) => setTimeout(done, 300));
  await panel.screenshot(join(shots, `${mode}-workspace-open.png`));
  await setOpen(false);
  await panel.type('#ws-filter', 'METADATA');
  await new Promise((done) => setTimeout(done, 200));
  await panel.screenshot(join(shots, `${mode}-filter.png`));
  await panel.eval(`(() => { const f = document.querySelector('#ws-filter'); f.value = ''; f.dispatchEvent(new Event('input')); document.querySelector('#ws-done').click(); })()`);
  await panel.until(`document.body.dataset.view !== 'workspace' || null`, { what: 'the conversation' });
}

const failed = results.filter(([, ok]) => !ok).length;
console.log(`${results.length - failed}/${results.length} checks passed; screenshots in ${shots}`);
process.exit(failed ? 1 : 0);
