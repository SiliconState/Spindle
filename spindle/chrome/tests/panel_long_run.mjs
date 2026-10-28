// The panel through a long agent run, in real Chrome against a scripted provider: one
// turn of 34 steps (browser steps, files written and edited, commands in the browser
// Linux; some failing, one failing and done again), a message sent while it runs, a
// report written as a page and an answer with a table. Run once in each theme (Paper,
// Light (Paper, plain), Dark, Auto in a light and in a dark system), it checks what the
// panel shows of it:
//
//   - steps in groups: the running turn's group open, its running step spinning and
//     timed; every group shut once the turn ends, its summary counting steps, failures
//     and retries (a failure done again later shows "failed → retried ✓");
//   - steps in plain words in their tense ("Running build_report.py", "Ran …"), browser
//     steps with the page's icon and name, the full command (with a copy button) inside;
//   - the message sent while it ran as a bubble of its own; "Latest" while scrolled up;
//     Send as Stop while the turn runs; a copy button on the answer; short table cells
//     on one line; the composer's chip without the provider;
//   - the workspace: folders, new/edited badges, scratch files hidden until asked for, and
//     its own page in the panel, a file open there with its preview, back and forth;
//   - small labels (SPINDLE, timestamps, the groups' times) at WCAG AA contrast;
//   - nothing on the panel's console.
//
// Screenshots (each theme, 380 and 620 px wide; mid-run and after) go to SHOTS_DIR.
//
//   SPINDLE_BROWSER=/path/to/chrome node chrome/tests/panel_long_run.mjs DIST [SHOTS_DIR]
//
// DIST is chrome/build.sh's output (with SPINDLE_WASM_LINUX the commands run in the wasm
// Linux, in seconds; without it on v86, which boots first).
import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { launch, open } from './cdp.mjs';

const [distArg, shotsArg] = process.argv.slice(2);
const browserBinary = process.env.SPINDLE_BROWSER;
if (!distArg || !browserBinary) throw new Error('usage: SPINDLE_BROWSER=... node chrome/tests/panel_long_run.mjs DIST [SHOTS_DIR]');
const work = fs.mkdtempSync(join(tmpdir(), 'spindle-long-'));
let browser = null;
process.on('exit', () => {
  browser?.child.kill('SIGKILL');
  fs.rmSync(work, { recursive: true, force: true });
});
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => process.exit(130));
const shots = resolve(shotsArg ?? join(work, 'shots'));
fs.mkdirSync(shots, { recursive: true });
const key = `sk-scripted-${randomBytes(8).toString('hex')}`;

// ---- The page the thread reads and the scripted provider.

const replies = [];
let seen = 0;
const server = http.createServer(async (request, response) => {
  if (request.method === 'GET' && request.url === '/page.html') {
    response.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
    response.end('<!doctype html><title>Field notes</title><h1>Field notes</h1><label>Search <input id="q"></label> <button>Go</button><p>Tickers: NVDA, TSM, ASML.</p>');
    return;
  }
  if (request.method === 'POST' && request.url === '/v1/chat/completions') {
    const parts = [];
    for await (const part of request) parts.push(part);
    seen += 1;
    let reply = replies.shift() ?? { content: '(script ended)' };
    if (reply.after) {
      await reply.after;
      reply = reply.then;
    }
    // A step acting on a part of the page: its ref from what page_elements gave back.
    if (reply.needs === 'refs') {
      const told = JSON.stringify(JSON.parse(Buffer.concat(parts).toString()).messages ?? []);
      reply = reply.make({ search: /textbox \\"Search\\"[^\]]*\[(\w+)\]/.exec(told)?.[1] ?? 'x1', go: /button \\"Go\\"[^\]]*\[(\w+)\]/.exec(told)?.[1] ?? 'x2' });
    }
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    response.end(sse(reply));
    return;
  }
  response.writeHead(404).end();
});
const port = await new Promise((done) => server.listen(0, '127.0.0.1', () => done(server.address().port)));
const pageUrl = `http://127.0.0.1:${port}/page.html`;

function sse(reply) {
  const deltas = [
    ...(reply.content != null ? [{ content: reply.content }] : []),
    ...(reply.call ? [{ tool_calls: [{ index: 0, id: `call_${seen}`, type: 'function', function: { name: reply.call[0], arguments: JSON.stringify(reply.call[1]) } }] }] : []),
  ];
  const chunks = [
    ...deltas.map((delta) => ({ id: 'c', object: 'chat.completion.chunk', choices: [{ index: 0, delta, finish_reason: null }] })),
    { id: 'c', object: 'chat.completion.chunk', choices: [{ index: 0, delta: {}, finish_reason: reply.call ? 'tool_calls' : 'stop' }] },
    { id: 'c', object: 'chat.completion.chunk', choices: [], usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 } },
  ];
  return `${chunks.map((chunk) => `data: ${JSON.stringify(chunk)}\n\n`).join('')}data: [DONE]\n\n`;
}
const call = (name, input, content) => ({ call: [name, input], ...(content ? { content } : {}) });

const catalogFile = join(work, 'providers.json');
fs.writeFileSync(catalogFile, JSON.stringify({
  version: 4, active_provider: 'scripted', providers: [{
    id: 'scripted', display_name: 'Scripted', api_provider: 'openai', request_contract: 'openai-chat-completions',
    base_url: `http://127.0.0.1:${port}/v1`, default_model: 'deepseek-flash', models: ['deepseek-flash'], requires_api_key: true,
    model_specs: { 'deepseek-flash': { context_window: 400000, max_output_tokens: 4096 } },
  }],
}));

// The extension, Chrome's access to sites given up front (a prompt cannot be clicked
// headless; the Pages switches are clicked as the user does).
const dist = join(work, 'extension');
fs.cpSync(resolve(distArg), dist, { recursive: true });
const manifest = JSON.parse(fs.readFileSync(join(dist, 'manifest.json'), 'utf8'));
fs.writeFileSync(join(dist, 'manifest.json'), JSON.stringify({ ...manifest, host_permissions: manifest.optional_host_permissions }, null, 2));

// ---- The run a thread makes.

const REPORT = `<!doctype html><html><head><meta charset="utf-8"><title>Chip report</title>
<style>body{font:15px/1.5 system-ui;margin:24px;color:#222}table{border-collapse:collapse}td,th{border:1px solid #ccc;padding:4px 8px}</style></head>
<body><h1>Chip report</h1><p>Three tickers, from the page and the data.</p>
<table><tr><th>Ticker</th><th>Close</th><th>Change</th></tr><tr><td>NVDA</td><td>131.20</td><td>+2.4%</td></tr><tr><td>TSM</td><td>188.05</td><td>-0.7%</td></tr><tr><td>ASML</td><td>702.10</td><td>+1.1%</td></tr></table></body></html>`;
const SCRIPT = "import csv\nrows = list(csv.DictReader(open('data/table.csv')))\nprint(len(rows), 'rows;', ', '.join(row['ticker'] for row in rows))\n";
const ANSWER = `Done. The report is in **report.html** (open it from the workspace), built from the page and \`data/table.csv\`.

| Ticker | Handle | Close | Change | Note |
| --- | --- | --- | --- | --- |
| NVDA | @nvidia | 131.20 | +2.4% | Led the group on data-center demand, again this week |
| TSM | @tsmc | 188.05 | -0.7% | Flat after the monthly sales figures |
| ASML | @asml | 702.10 | +1.1% | Orders recovered |

One step failed and was done again (the script ran before it existed); \`exit 3\` failed on purpose.`;

/** The turn's replies: 34 steps in three runs, the reply after `held` waiting for
 * `gate` (the panel is looked at mid-run), and a `sleep` step to see one running. */
function script(tab, gate) {
  return [
    call('list_tabs', {}, 'Looking at your tabs first.'),
    call('read_page', { tab }),
    call('page_elements', { tab }),
    { needs: 'refs', make: (refs) => call('type', { tab, ref: refs.search, text: 'chips' }) },
    { needs: 'refs', make: (refs) => call('click', { tab, ref: refs.go }) },
    call('read_page', { tab: 999999 }),
    call('navigate', { url: 'https://www.example.com/markets/chips', new_tab: false, tab: 999999 }),
    call('page_html', { tab }),
    { content: 'Now the data, in the workspace.', call: ['bash', { command: 'ls -la' }] },
    call('read_file', { path: 'missing.txt' }),
    call('bash', { command: 'python3 build_report.py' }),
    call('write_file', { path: 'data/table.csv', content: 'ticker,close,change\nNVDA,131.20,2.4\nTSM,188.05,-0.7\nASML,702.10,1.1\n' }),
    call('write_file', { path: 'build_report.py', content: SCRIPT }),
    call('bash', { command: 'python3 build_report.py' }),
    call('bash', { command: 'cat data/table.csv | head -3' }),
    call('edit_file', { path: 'build_report.py', old_string: "'rows;'", new_string: "'rows:'" }),
    call('bash', { command: 'python3 build_report.py && wc -l data/table.csv' }),
    call('bash', { command: 'grep -c , data/table.csv' }),
    call('write_file', { path: 'notes.txt', content: 'NVDA leads; TSM flat; ASML recovering.\n' }),
    call('write_file', { path: 'tests/test_report.py', content: "assert open('data/table.csv').read().count('\\n') == 4\nprint('ok')\n" }),
    call('bash', { command: 'python3 tests/test_report.py' }),
    { after: gate, then: call('bash', { command: 'sleep 4; echo slept' }) },
    call('bash', { command: 'mkdir -p out && echo ready > out/status.txt' }),
    call('bash', { command: 'uname -m' }),
    call('bash', { command: 'exit 3' }),
    { content: 'Writing the report.', call: ['write_file', { path: 'report.html', content: REPORT }] },
    call('read_file', { path: 'report.html' }),
    call('bash', { command: 'python3 -c "print(sum(range(10)))"' }),
    call('bash', { command: 'find . -name "*.csv"' }),
    call('write_file', { path: 'summary.md', content: '# Summary\n\n| Ticker | Close |\n| --- | --- |\n| NVDA | 131.20 |\n| TSM | 188.05 |\n' }),
    call('bash', { command: 'ls out data' }),
    call('bash', { command: 'cat out/status.txt' }),
    call('bash', { command: 'du -sh .' }),
    call('bash', { command: 'echo finished >> notes.txt' }),
    { content: ANSWER },
  ];
}

// ---- Driving Chrome.

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
const panelSaid = [];
/** Where the run is (said as it goes, for a run that stalls). */
const say = (text) => console.log(`...  ${text}`);

say('Chrome');
browser = await launch(browserBinary, { profile: join(work, 'profile'), downloads: join(work, 'downloads') });
const extensionId = (await browser.send('Extensions.loadUnpacked', { path: dist })).id;
const page = await open(browser, pageUrl, { newWindow: true });
await page.until(`document.title === 'Field notes'`);
const panel = await open(browser, `chrome-extension://${extensionId}/panel.html`, { newWindow: true });
browser.on(({ method, params, sessionId }) => {
  if (sessionId !== panel.sessionId) return;
  if (method === 'Runtime.consoleAPICalled' && ['error', 'warning'].includes(params.type)) panelSaid.push(`${params.type}: ${params.args.map((arg) => arg.value ?? arg.description ?? '').join(' ')}`);
  else if (method === 'Runtime.exceptionThrown') panelSaid.push(`exception: ${params.exceptionDetails.exception?.description ?? params.exceptionDetails.text}`);
});
const send = (method, params = {}) => browser.send(method, params, panel.sessionId);
await panel.size(420, 900);
await panel.until('globalThis.spindlePanel?.engine', { timeout: 60_000, what: 'the engine' });

// The provider, its key, page reading and actions, as the user sets them.
say('the panel is up; the provider');
await panel.files('#catalog-file', [catalogFile]);
await panel.until(`spindlePanel.engine.catalog.providers.some((p) => p.id === 'scripted') && spindlePanel.engine.catalogSaved`, { what: 'the catalog' });
await panel.eval(`document.querySelector('#settings-dialog').open || document.querySelector('#settings').click()`);
const row = '.provider[data-provider="scripted"]';
await panel.click(`${row} .provider-head button`);
await panel.type(`${row} input`, key);
await panel.click(`${row} button.primary`);
await panel.until(`document.querySelector('${row} .provider-state')?.textContent === 'key saved'`, { what: 'the key' });
await panel.eval(`document.querySelector('[data-fold="pages"]').open = true`);
await panel.click('#page-reading');
await panel.until(`document.querySelector('#page-reading').getAttribute('aria-checked') === 'true'`, { what: 'page reading on' });
await panel.click('#page-actions');
await panel.until(`document.querySelector('#page-actions').getAttribute('aria-checked') === 'true'`, { what: 'page actions on' });
await panel.click('#settings-dialog [data-close]');
await panel.click('#model-chip');
await panel.click('#model-provider .picker-field');
await panel.click('#model-provider [role="option"][data-value="scripted"]');
await panel.click('#model-apply');

say('set up');
const tab = await panel.eval(`chrome.tabs.query({ url: ${JSON.stringify(pageUrl)} }).then((tabs) => tabs[0]?.id)`);
assert.ok(tab, 'the test page is open in a tab');

const THEMES = [
  { name: 'paper', mode: 'paper', texture: 'textured', scheme: 'light' },
  { name: 'light', mode: 'paper', texture: 'plain', scheme: 'light' },
  { name: 'dark', mode: 'dark', texture: 'textured', scheme: 'light' },
  { name: 'auto-light', mode: 'system', texture: 'textured', scheme: 'light' },
  { name: 'auto-dark', mode: 'system', texture: 'textured', scheme: 'dark' },
];
const shot = async (name, width = 420) => {
  await panel.size(width, 900);
  await new Promise((done) => setTimeout(done, 250));
  await panel.screenshot(join(shots, `${name}.png`));
};

/** The contrast of the small labels against what is behind them (WCAG: 4.5 for text). */
const CONTRAST = `(() => {
  const rgb = (text) => (text.match(/[\\d.]+/g) ?? []).map(Number);
  const lum = ([r, g, b]) => { const c = [r, g, b].map((v) => { v /= 255; return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4; }); return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]; };
  const behind = (node) => { for (let at = node; at; at = at.parentElement) { const color = rgb(getComputedStyle(at).backgroundColor); if (color.length === 3 || (color.length === 4 && color[3] > 0.5)) return color.slice(0, 3); } return rgb(getComputedStyle(document.body).backgroundColor).slice(0, 3); };
  const worst = [];
  for (const selector of ['.role-label', '.group-time', '.tool-time', '#phase', '.file-meta', '.file-row small', '.ws-folder-name', '.thread-meta', 'time', '.note']) {
    for (const node of [...document.querySelectorAll(selector)].filter((n) => n.offsetParent && n.textContent.trim()).slice(0, 6)) {
      const a = lum(rgb(getComputedStyle(node).color).slice(0, 3)), b = lum(behind(node));
      const ratio = (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
      if (ratio < 4.5) worst.push(selector + ' ' + ratio.toFixed(2) + ' "' + node.textContent.trim().slice(0, 20) + '"');
    }
  }
  return worst;
})()`;

for (const theme of THEMES) {
  const name = theme.name;
  await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: theme.scheme }] });
  await panel.eval(`spindleTheme.set(${JSON.stringify(theme.mode)}); spindleTheme.setTexture(${JSON.stringify(theme.texture)}); document.querySelector('#home').click()`);
  await panel.until(`document.body.dataset.view === 'home' || null`, { what: 'home' });
  let release;
  const gate = new Promise((done) => { release = done; });
  const steps = script(tab, gate);
  replies.length = 0;
  replies.push(...steps);
  say(`${name}: the turn`);
  const before = await panel.eval('spindlePanel.current');
  await panel.type('#prompt', 'Look at the Field notes page, build a chip report from it and data/table.csv, and tell me what you found.');
  await panel.click('#send');
  const thread = await panel.until(`spindlePanel.current !== ${JSON.stringify(before)} && spindlePanel.current`, { what: 'the thread' });
  // Held mid-run: the panel as a running turn shows it.
  say(`${name}: started`);
  await panel.until(`document.querySelectorAll('#history .tool').length >= 21 && !document.querySelector('#history .tool.running') || null`, { timeout: 180_000, what: '21 steps' });
  const midRun = await panel.eval(`(() => {
    const groups = [...document.querySelectorAll('#history > .steps')];
    const last = groups.at(-1);
    return {
      groups: groups.length,
      lastOpen: last?.group.open ?? null,
      earlierShut: groups.slice(0, -1).every((g) => !g.group.open || g.classList.contains('single')),
      send: document.querySelector('#send').dataset.mode,
      sendLabel: document.querySelector('#send').getAttribute('aria-label'),
      stopInBar: Boolean(document.querySelector('#stop')),
      chip: document.querySelector('#model-chip').textContent,
      pageFits: document.scrollingElement.scrollHeight <= innerHeight + 1,
    };
  })()`);
  say(`${name}: held mid-run`);
  await shot(`${name}-1-mid-run`);
  // A message while it runs: Send turns from Stop to "send to the running turn".
  await panel.type('#prompt', 'Also note which ticker moved most.');
  const steerMode = await panel.eval(`document.querySelector('#send').dataset.mode`);
  await panel.click('#send');
  await new Promise((done) => setTimeout(done, 400));
  release();
  // A step running: its spinner and its time.
  await panel.until(`[...document.querySelectorAll('#history .tool.running')].some((c) => /sleep/.test(c.input?.command ?? '')) || null`, { timeout: 60_000, what: 'the sleep step running' });
  await new Promise((done) => setTimeout(done, 1600));
  const running = await panel.eval(`(() => { const card = document.querySelector('#history .tool.running'); return { label: card.querySelector('.tool-label').textContent, time: card.querySelector('.tool-time').textContent, group: card.closest('.steps').querySelector('.group-label').textContent, spinning: getComputedStyle(card.querySelector('.tool-status')).animationName }; })()`);
  await shot(`${name}-2-step-running`);
  // Scrolled up while it runs: "Latest" shows.
  await panel.eval(`document.querySelector('#main').scrollTop = 0; document.querySelector('#main').dispatchEvent(new Event('scroll'))`);
  const jump = await panel.until(`!document.querySelector('#jump').hidden || null`, { timeout: 5000, what: 'Latest' }).catch(() => false);
  await shot(`${name}-3-scrolled-up`);
  if (jump) await panel.click('#jump');
  say(`${name}: waiting for the end`);
  const state = await panel.until(`(() => { const s = spindlePanel.threads.get(${JSON.stringify(thread)})?.state; return s && s !== 'running' ? s : null; })()`, { timeout: 180_000, what: 'the turn to end' });
  await panel.until(`document.querySelector('#history .workspace-card .ws-tree') || null`, { timeout: 30_000, what: 'the workspace card' });
  await new Promise((done) => setTimeout(done, 500));
  const after = await panel.eval(`(() => {
    const groups = [...document.querySelectorAll('#history > .steps')];
    const cards = [...document.querySelectorAll('#history .tool')];
    return {
      steps: cards.length,
      groups: groups.map((g) => ({ open: g.group.open, single: g.classList.contains('single'), label: g.querySelector('.group-label').textContent, time: g.querySelector('.group-time').textContent })),
      failed: cards.filter((c) => c.classList.contains('failed')).map((c) => c.querySelector('.tool-label').textContent + ' = ' + c.querySelector('.tool-state').textContent),
      labels: cards.map((c) => c.querySelector('.tool-label').textContent),
      times: cards.filter((c) => c.querySelector('.tool-time').textContent).length,
      favicons: cards.filter((c) => c.querySelector('.tool-icon img.favicon, .tool-icon svg')).length,
      redBorder: cards.filter((c) => c.classList.contains('failed') && getComputedStyle(c).borderLeftColor === getComputedStyle(document.documentElement).getPropertyValue('--error').trim()).length,
      steer: (() => { const node = document.querySelector('#history .message-wrap.steer'); if (!node) return null; const r = node.getBoundingClientRect(), box = document.querySelector('#history').getBoundingClientRect(); return { label: node.querySelector('.role-label').textContent, right: Math.round(box.right - r.right), left: Math.round(r.left - box.left) }; })(),
      copyAnswer: Boolean([...document.querySelectorAll('#history .message-wrap.assistant')].at(-1)?.querySelector('.role-label .copy')),
      nowrap: [...document.querySelectorAll('#history .message.assistant td.nowrap')].map((td) => td.textContent),
      wrapped: [...document.querySelectorAll('#history .message.assistant td.nowrap')].filter((td) => td.getClientRects().length > 1 || td.scrollHeight > td.clientHeight + 4 && td.offsetHeight > 40).length,
      inputs: cards.filter((c) => c.querySelector('.tool-input pre.input') && c.querySelector('.tool-input .copy')).length,
      tree: [...document.querySelectorAll('#history .workspace-card .file-row')].map((r) => (r.hidden ? '-' : '+') + r.dataset.path + (r.querySelector('.badge') ? ':' + r.querySelector('.badge').textContent : '')),
      folders: [...document.querySelectorAll('#history .workspace-card .ws-folder-name')].map((f) => f.textContent.trim()),
      // Files the steps wrote: pages and Markdown in sight below their run, text files
      // inside it (under their step), one card a file.
      outside: [...document.querySelectorAll('#history > .steps > .file-card')].map((c) => c.dataset.path),
      inside: [...document.querySelectorAll('#history .group-list > .file-card')].map((c) => c.dataset.path),
      jump: !document.querySelector('#jump').hidden,
      send: document.querySelector('#send').dataset.mode,
      pageFits: document.scrollingElement.scrollHeight <= innerHeight + 1 && document.scrollingElement.scrollTop === 0,
    };
  })()`);
  const contrast = await panel.eval(CONTRAST);
  await panel.eval(`document.querySelector('#main').scrollTop = 0`);
  await shot(`${name}-4-after-top`);
  await panel.eval(`document.querySelector('#main').scrollTop = document.querySelector('#main').scrollHeight`);
  await shot(`${name}-5-after-end`);
  await shot(`${name}-5-after-end-380`, 380);
  await shot(`${name}-5-after-end-620`, 620);
  // A group opened: rows, a failed one, retried.
  await panel.eval(`(() => { const g = [...document.querySelectorAll('#history > .steps')].find((s) => s.classList.contains('has-failed') || s.querySelector('.tool.retried')); g.group.open = true; g.scrollIntoView({ block: 'start' }); })()`);
  await shot(`${name}-6-group-open`);
  await shot(`${name}-6-group-open-380`, 380);
  // A step opened: its command and what it gave back.
  await panel.eval(`(() => { const c = [...document.querySelectorAll('#history .tool')].find((c) => c.classList.contains('retried')); c.closest('.steps').group.open = true; c.open = true; c.scrollIntoView({ block: 'start' }); })()`);
  await shot(`${name}-7-step-open`);
  // The workspace page: the list (scratch hidden), then a file open.
  say(`${name}: the workspace page`);
  await panel.click('#history .workspace-card .ws-tools .link-like');
  await panel.until(`document.body.dataset.view === 'workspace' && document.querySelector('#file-list .file-row') || null`, { what: 'the workspace page' });
  const page1 = await panel.eval(`({ rows: [...document.querySelectorAll('#file-list .file-row')].filter((r) => !r.hidden).map((r) => r.dataset.path), hiddenNote: document.querySelector('#file-list .ws-hidden')?.textContent ?? null, heading: document.querySelector('#ws-heading').textContent, back: document.querySelector('#home').getAttribute('aria-label') })`);
  await shot(`${name}-8-workspace`);
  await shot(`${name}-8-workspace-620`, 620);
  await panel.click('#file-list .file-row[data-path="report.html"] .file-open');
  await panel.until(`!document.querySelector('#ws-file').hidden && document.querySelector('#ws-preview iframe, #ws-preview .file-frame') || null`, { what: 'report.html open' });
  await new Promise((done) => setTimeout(done, 800));
  const filePage = await panel.eval(`({ path: document.querySelector('#ws-path').textContent, prev: !document.querySelector('#ws-prev').disabled, next: !document.querySelector('#ws-next').disabled, actions: [...document.querySelectorAll('#ws-actions button')].map((b) => b.getAttribute('aria-label')) })`);
  await shot(`${name}-9-workspace-file`);
  await panel.eval(`document.querySelector('#ws-next').click()`);
  await new Promise((done) => setTimeout(done, 400));
  const nextPath = await panel.eval(`document.querySelector('#ws-path').textContent`);
  await panel.eval(`document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))`);
  const backToList = await panel.until(`!document.querySelector('#ws-list').hidden || null`, { timeout: 3000, what: 'the list again' }).catch(() => false);
  await panel.click('#home');
  const backHome = await panel.until(`document.body.dataset.view === 'thread' || null`, { timeout: 3000, what: 'the conversation again' }).catch(() => false);

  check(`${name}: 34 steps in groups, the running one open, earlier ones shut; Send is Stop while it runs; no stop in the bar; the chip without the provider`, () => {
    assert.equal(state, 'idle', `state ${state}`);
    assert.equal(after.steps, 34, `steps ${after.steps}`);
    assert.ok(midRun.groups >= 2 && midRun.lastOpen === true && midRun.earlierShut, JSON.stringify(midRun));
    assert.equal(midRun.send, 'stop', JSON.stringify(midRun));
    assert.equal(midRun.sendLabel, 'Stop this turn');
    assert.equal(midRun.stopInBar, false);
    assert.equal(steerMode, 'steer');
    assert.match(midRun.chip, /^deepseek-flash/);
    assert.ok(!/Scripted/.test(midRun.chip), midRun.chip);
    assert.ok(midRun.pageFits && after.pageFits, 'the panel itself never scrolls (only the conversation does)');
  });
  check(`${name}: a running step spins and counts its time, in the present tense; its group says which step it is`, () => {
    assert.match(running.label, /^Running a command|^Running sleep|^Running/, JSON.stringify(running));
    assert.match(running.time, /^(\d+ms|\d(\.\d)?s)$/, JSON.stringify(running));
    assert.match(running.group, /step \d+ of \d+/, JSON.stringify(running));
    assert.notEqual(running.spinning, 'none', JSON.stringify(running));
  });
  check(`${name}: after the turn every group is shut with its count, failures and retries; labels in the past tense; times; browser steps with an icon; the command with a copy button`, () => {
    assert.ok(after.groups.length >= 3, JSON.stringify(after.groups));
    assert.ok(after.groups.every((g) => !g.open || g.single), JSON.stringify(after.groups));
    assert.ok(after.groups.some((g) => /^Ran \d+ steps · \d+ failed · 1 retried$/.test(g.label)), JSON.stringify(after.groups));
    assert.ok(after.groups.every((g) => g.single || /^Ran \d+ steps/.test(g.label)), JSON.stringify(after.groups));
    assert.ok(after.failed.some((f) => /failed → retried ✓$/.test(f) && /build_report\.py/.test(f)), JSON.stringify(after.failed));
    assert.ok(after.failed.some((f) => / = failed$/.test(f)), JSON.stringify(after.failed));
    for (const want of ['Listed open tabs', 'Read 127.0.0.1', 'Read the parts of 127.0.0.1', 'Typed into the “Search” field on 127.0.0.1', 'Clicked the “Go” button on 127.0.0.1', 'Ran build_report.py', 'Wrote report.html', 'Edited build_report.py', 'Read table.csv', 'Wrote notes.txt', 'Searched for “,”', 'Made folder out']) {
      assert.ok(after.labels.includes(want), `no "${want}" in ${JSON.stringify(after.labels)}`);
    }
    assert.ok(after.times >= 30, `times on ${after.times} rows`);
    assert.ok(after.favicons >= 8, `icons on ${after.favicons} browser rows`);
    assert.equal(after.redBorder, 0, 'no red border on failed rows');
    assert.ok(after.inputs >= 30, `copyable inputs on ${after.inputs} rows`);
  });
  check(`${name}: the message sent mid-run is a bubble at the right; the answer has a copy button; short table cells stay on one line; Latest showed while scrolled up`, () => {
    assert.ok(after.steer && after.steer.label === 'YOU · WHILE IT RAN' && after.steer.right < after.steer.left, JSON.stringify(after.steer));
    assert.ok(after.copyAnswer);
    assert.ok(after.nowrap.includes('@nvidia') && after.nowrap.includes('+2.4%'), JSON.stringify(after.nowrap));
    assert.equal(after.wrapped, 0);
    assert.ok(jump, 'Latest showed while scrolled up');
    assert.equal(after.jump, false, 'Latest gone once the turn ended');
    assert.equal(after.send, 'send');
  });
  check(`${name}: the workspace by folder, new files marked, scratch files hidden; its own page, a file open there with its preview, next, Escape, back`, () => {
    assert.ok(after.folders.includes('data/') && after.folders.includes('out/'), JSON.stringify(after.folders));
    assert.deepEqual(after.outside.sort(), ['report.html', 'summary.md'], JSON.stringify(after.outside));
    assert.deepEqual(after.inside.sort(), ['build_report.py', 'data/table.csv', 'notes.txt', 'tests/test_report.py'], JSON.stringify(after.inside));
    assert.ok(after.tree.includes('+report.html:new') && after.tree.includes('+data/table.csv:new'), JSON.stringify(after.tree));
    assert.ok(after.tree.includes('-notes.txt:new') && after.tree.includes('-build_report.py:new') && after.tree.some((r) => r.startsWith('-tests/')), JSON.stringify(after.tree));
    assert.ok(page1.rows.includes('report.html') && !page1.rows.includes('notes.txt') && /scratch files? hidden/.test(page1.hiddenNote ?? ''), JSON.stringify(page1));
    assert.equal(page1.back, 'Back to the conversation');
    assert.match(filePage.path, /report\.html/);
    assert.ok(filePage.actions.includes('Copy its path') && filePage.actions.includes('Download report.html') && filePage.actions.includes('Open report.html in a tab'), JSON.stringify(filePage));
    assert.ok(nextPath && nextPath !== filePage.path, `next: ${nextPath}`);
    assert.ok(backToList, 'Escape goes back to the list');
    assert.ok(backHome, 'the back button goes to the conversation');
  });
  check(`${name}: small labels at WCAG AA contrast`, () => {
    assert.deepEqual(contrast, []);
  });
}
// Settings, Commands: both Linuxes' states (the wasm Linux's when the build carries it).
await panel.click('#settings');
await panel.eval(`document.querySelector('[data-fold="commands"]').open = true`);
const linuxFacts = await panel.until(`(() => { const row = document.querySelector('#wasm-linux-row'); return row && !row.hidden ? { v86: document.querySelector('#linux-state').textContent, wasm: document.querySelector('#wasm-linux-state').textContent } : null; })()`, { timeout: 10_000, what: "the wasm Linux's state" }).catch(() => null);
await shot('settings-commands');
await panel.click('#settings-dialog [data-close]');
const carriesWasm = fs.existsSync(join(dist, 'wasm-linux/identity'));
check("Settings shows the wasm Linux's state beside v86's", () => {
  if (!carriesWasm) return;
  assert.ok(linuxFacts, 'the wasm Linux row shows');
  assert.match(linuxFacts.wasm, /^running/, JSON.stringify(linuxFacts));
});
check('nothing on the panel\'s console', () => {
  assert.deepEqual(panelSaid.filter((line) => !/favicon|ResizeObserver/.test(line)), []);
});

const failed = results.filter(([, ok]) => !ok);
console.log(`${failed.length ? 'panel-long-run FAILED' : 'panel-long-run passed'}: ${results.length - failed.length}/${results.length} checks; screenshots in ${shots}`);
process.exit(failed.length ? 1 : 0);
