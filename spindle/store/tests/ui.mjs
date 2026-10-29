import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {runInNewContext} from 'node:vm';
import test from 'node:test';

const ui = new URL('../ui/', import.meta.url);
const {intent} = await import(new URL('../ui/commands.js', import.meta.url).href);
test('natural language covers every UI control', () => {
  assert.deepEqual(intent('stop'), {action: 'stop'});
  assert.deepEqual(intent('/continue'), {action: 'continue'});
  assert.deepEqual(intent('Retry the model'), {action: 'retry'});
  assert.deepEqual(intent('rewind to r37'), {action: 'rewind', revision: 37});
  assert.deepEqual(intent('undo 12'), {action: 'rewind', revision: 12});
  assert.deepEqual(intent('go back to revision 4'), {action: 'rewind', revision: 4});
  assert.deepEqual(intent('rename this thread to Demo pipeline'), {action: 'rename', name: 'Demo pipeline'});
  assert.deepEqual(intent('delete this thread'), {action: 'delete'});
  assert.deepEqual(intent('show files'), {action: 'files'});
  assert.deepEqual(intent('open settings'), {action: 'settings'});
  assert.deepEqual(intent('set reasoning to low'), {action: 'effort', effort: 'low'});
  assert.deepEqual(intent('effort off'), {action: 'effort', effort: 'off'});
  assert.deepEqual(intent('set reasoning to max'), {action: 'effort', effort: 'max'});
  assert.deepEqual(intent('effort xhigh'), {action: 'effort', effort: 'xhigh'});
  assert.equal(intent('reasoning is hard'), null);
  assert.deepEqual(intent('show history'), {action: 'history'});
  assert.deepEqual(intent('filter threads: demo'), {action: 'filter', query: 'demo'});
  assert.deepEqual(intent('new thread'), {action: 'new'});
  assert.deepEqual(intent('Stop the run'), {action: 'stop'});
  assert.equal(intent('please rewind to r37'), null, 'pleasantry prefix is not hijacked');
  assert.equal(intent('what is r37?'), null);
  assert.equal(intent('rename a variable in app.js'), null);
  assert.deepEqual(intent('/find Cargo.lock'), {action: 'find', query: 'Cargo.lock'});
  assert.deepEqual(intent('/search two words'), {action: 'find', query: 'two words'});
  assert.equal(intent('find the bug in app.js'), null, 'an unslashed request goes to the model');
  assert.deepEqual(intent('export transcript'), {action: 'export'});
  assert.deepEqual(intent('download workspace'), {action: 'download'});
});

const text = await import(new URL('../ui/text.js', import.meta.url).href);
test('search, diff, transcript export and labels are plain text functions', () => {
  assert.deepEqual(text.matches('Hello hello HELLO', 'hello'), [0, 6, 12]);
  assert.deepEqual(text.matches('aaaa', 'aa'), [0, 2], 'matches do not overlap');
  assert.deepEqual(text.matches('abc', ''), []);
  assert.deepEqual(text.unified('a\nb\nc\n', 'a\nb\nc\n'), []);
  assert.deepEqual(text.unified('a\nb\nc\n', 'a\nB\nc\nd\n'), [['@', '@@ -1,3 +1,4 @@'], [' ', 'a'], ['-', 'b'], ['+', 'B'], [' ', 'c'], ['+', 'd']]);
  assert.deepEqual(text.unified('', 'new\n'), [['@', '@@ -0,0 +1,1 @@'], ['+', 'new']]);
  const before = Array.from({length: 40}, (_, n) => `line ${n}`).join('\n');
  const after = before.replace('line 3\n', 'line three\n').replace('line 35', 'line thirty-five');
  const hunks = text.unified(before, after).filter(row => row[0] === '@');
  assert.deepEqual(hunks, [['@', '@@ -1,7 +1,7 @@'], ['@', '@@ -33,7 +33,7 @@']], 'distant changes get separate hunks');
  assert.equal(text.unified(before, after.split('\n').reverse().join('\n'), 3, 5), null, 'large rewrites are refused, not computed');
  const markdown = text.transcript({revision: 4, head: 4, provider: 'glm', model: 'm', messages: [
    {role: 'user', content: [{type: 'text', text: 'run it'}]},
    {role: 'assistant', content: [{type: 'tool_use', id: 't', name: 'bash', input: {command: 'echo ```'}}]},
    {role: 'user', content: [{type: 'tool_result', tool_use_id: 't', content: '```', is_error: true}]},
  ]}, 'Demo');
  assert.match(markdown, /^# Demo\n\nr4 of r4 · glm\/m/);
  assert(markdown.includes('## You\n\nrun it'));
  assert(markdown.includes('````sh\necho ```\n````'), 'fences outgrow backticks in the content');
  assert(markdown.includes('### Result (failed)'));
  assert.equal(text.ago(Date.now() - 5 * 60000), '5m ago');
  assert.equal(text.ago(0), '');
  assert.equal(text.size(7.4 * 1024 ** 3), '7.4 GB');
  assert.equal(text.size(512), '512 B');
  assert.equal(text.duration(90061), '1d 1h');
  assert.equal(text.tokens(999), '999');
  assert.equal(text.tokens(1234), '1.2K');
  assert.equal(text.tokens(2500000), '2.5M');
  assert.equal(text.usageLine({calls: 0}), '');
  assert.equal(text.usageLine({calls: 2, input: 5000, output: 300, cache_read: 0, usd: 0, priced: 0}), '2 calls · 5.0K in · 300 out');
  assert.equal(text.usageLine({calls: 2, input: 5000, output: 300, usd: 0, priced: 0}, true), '5.3K tok');
  assert.equal(text.usageLine({calls: 4, input: 1, output: 1, usd: 12.5, priced: 2, subscription: 1, unpriced: 1, unknown: 1}), '4 calls · 1 in · 1 out · $12.50 · 1 subscription · 1 unpriced · 1 may have been billed');
  assert.equal(text.usageLine({calls: 3, input: 10, output: 5, usd: 0, priced: 0, subscription: 3}, true), 'subscription', 'a subscription login is not shown as unpriced tokens');
  assert.equal(text.money(0.4213), '$0.421');
  assert.equal(text.money(0.0042), '$0.0042');
  assert.equal(text.money(12.5), '$12.50');
  assert.equal(text.turnCost({calls: 3, priced: 3, usd: 0.0123}), '$0.012 · 3 calls');
  assert.equal(text.turnCost({calls: 1, priced: 0, subscription: 1, usd: 0}), 'subscription · 1 call');
  assert.equal(text.turnCost({calls: 0, unknown: 1}), '1 may have been billed');
  assert.equal(text.turnCost({calls: 2, priced: 2, usd: 0.5, unknown: 1}, true), '$0.500 · 2 calls', 'a run in flight has no uncertain request yet');
  assert.equal(text.turnCost({calls: 0}), '');
  assert.deepEqual(text.linkParts('Open https://x.example/a?b=1, then paste.'), [{text: 'Open '}, {url: 'https://x.example/a?b=1'}, {text: ', then paste.'}]);
  assert.deepEqual(text.linkParts('no links'), [{text: 'no links'}]);
});

test('messages split at code fences; a thread name comes from the first line', () => {
  assert.deepEqual(text.blocks('Run:\n\n```sh\ncargo test\n\n```\nThen `x`.'), [{text: 'Run:'}, {code: 'cargo test', lang: 'sh'}, {text: 'Then `x`.'}]);
  assert.deepEqual(text.blocks('plain'), [{text: 'plain'}]);
  assert.deepEqual(text.blocks('a\n```\nstill streaming'), [{text: 'a'}, {code: 'still streaming', lang: ''}], 'an unclosed fence runs to the end');
  assert.deepEqual(text.blocks('```js `x`\nnot a fence'), [{text: '```js `x`\nnot a fence'}], 'a backtick info string is inline code');
  assert.deepEqual(text.blocks('~~~~\n```\ninner\n```\n~~~~'), [{code: '```\ninner\n```', lang: ''}], 'only a matching fence closes');
  assert.equal(text.titleFrom('\n  Fix the   flaky test\nmore'), 'Fix the flaky test');
  assert.equal(text.titleFrom('x'.repeat(80)), 'x'.repeat(59) + '…');
  assert.equal(text.titleFrom('word '.repeat(20)).length <= 60, true);
});

const swSource = await readFile(new URL('sw.js', ui), 'utf8');
test('a tapped notification opens its thread in the open tab, or a new window', async () => {
  for (const open of [true, false]) {
    const handlers = {}, posted = [], opened = [];
    let focused = false;
    const client = {url: 'https://spindle.example/#thread/other', postMessage: message => posted.push(message), focus: async () => {focused = true;}};
    const self = {location: {origin: 'https://spindle.example'}, addEventListener: (name, fn) => handlers[name] = fn, skipWaiting() {},
      clients: {matchAll: async () => open ? [client] : [], openWindow: async url => opened.push(url), claim: async () => {}}};
    runInNewContext(swSource, {self, URL});
    let done;
    handlers.notificationclick({notification: {close() {}, data: {url: '/#thread/abc'}}, waitUntil: promise => done = promise});
    await done;
    if (open) {assert.equal(JSON.stringify(posted), JSON.stringify([{open: 'https://spindle.example/#thread/abc'}])); assert(focused);}
    else assert.deepEqual(opened, ['https://spindle.example/#thread/abc']);
    assert.equal(typeof handlers.install, 'function', 'an updated worker takes over at once');
    assert.equal(typeof handlers.fetch, 'undefined', 'nothing is cached or intercepted');
  }
});
const terminalSource = await readFile(new URL('terminal.js', ui), 'utf8');
const {Terminal} = await import(`data:text/javascript;base64,${Buffer.from(terminalSource).toString('base64')}`);
globalThis.requestAnimationFrame = () => 1;
const write = (terminal, text, stream = 1) => terminal.write(new TextEncoder().encode(text), stream);

test('terminal redraw, ANSI and split UTF-8 remain literal text', () => {
  const terminal = new Terminal({});
  write(terminal, 'first\rfinal\n\x1b[31mred\x1b[0m\n');
  assert.equal(terminal.text(), 'final\nred\n');
  write(terminal, '\x1b]8;;https://invalid.example\x07link\x1b]8;;\x07');
  assert(terminal.text().endsWith('link'));
  assert(!terminal.text().includes('https:'));
  const unicode = new Terminal({});
  for (const byte of new TextEncoder().encode('é🙂')) unicode.write([byte]);
  assert.equal(unicode.text(), 'é🙂');
  write(unicode, '<script>alert(1)</script>', 2);
  assert(unicode.text().includes('<script>'));
  assert.equal(unicode.lines[0].at(-1)[1], 'stderr');
});

test('terminal cell, line and column memory are bounded', () => {
  const terminal = new Terminal({});
  for (let n = 0; n < 1000; n++) write(terminal, 'x'.repeat(100) + '\n');
  assert(terminal.cells <= 65536);
  assert(terminal.lines.length <= 800);
  write(terminal, 'x'.repeat(10000));
  assert.equal(terminal.column, 2048);
  write(terminal, '\r\x1b[2K');
  assert(terminal.cells >= 0);
});

const themeSource = await readFile(new URL('theme.js', ui), 'utf8');
function theme(saved, prefersDark = false, texture = null) {
  const storage = new Map([...(saved ? [['spindle-theme', saved]] : []), ...(texture ? [['spindle-texture', texture]] : [])]);
  const root = {dataset: {}};
  const meta = {};
  const media = {matches: prefersDark, addEventListener(_, callback) {this.change = callback;}};
  const context = {
    matchMedia: () => media,
    localStorage: {getItem: key => storage.get(key), setItem: (key, value) => storage.set(key, value)},
    document: {documentElement: root, querySelector: () => meta},
  };
  context.globalThis = context;
  runInNewContext(themeSource, context);
  return {root, meta, media, storage, api: context.spindleTheme};
}
test('Paper is default even on a dark system; selection persists', () => {
  const current = theme(null, true);
  assert.equal(current.root.dataset.theme, 'paper');
  assert.equal(current.meta.content, '#f6f2e9');
  current.api.set('dark');
  assert.equal(current.root.dataset.theme, 'dark');
  assert.equal(current.meta.content, '#111511');
  current.api.set('system');
  assert.equal(current.storage.get('spindle-theme'), 'system');
  assert.equal(current.root.dataset.theme, 'dark', 'System follows the dark system');
  current.media.matches = false;
  current.media.change();
  assert.equal(current.root.dataset.theme, 'light', 'System follows the light system with Light, not Paper');
  assert.equal(current.meta.content, '#f6f7f5');
  current.api.set('light');
  assert.equal(current.storage.get('spindle-theme'), 'light');
  current.media.matches = true;
  current.media.change();
  assert.equal(current.root.dataset.theme, 'light', 'Light chosen stays light on a dark system');
  current.api.set('paper');
  assert.equal(current.storage.get('spindle-theme'), 'paper');
  assert.equal(theme('dark').root.dataset.theme, 'dark');
  assert.equal(theme('invalid').root.dataset.theme, 'paper');
});

test('the background is textured unless this screen chose plain, in any theme', () => {
  const current = theme(null, false);
  assert.equal(current.root.dataset.texture, 'textured');
  current.api.setTexture('plain');
  assert.equal(current.root.dataset.texture, 'plain');
  assert.equal(current.storage.get('spindle-texture'), 'plain');
  current.api.set('dark');
  assert.equal(current.root.dataset.texture, 'plain', 'a theme change keeps the background');
  current.api.setTexture('bogus');
  assert.equal(current.api.texture(), 'plain');
  assert.equal(theme('dark', false, 'plain').root.dataset.texture, 'plain');
  assert.equal(theme(null, false, 'invalid').root.dataset.texture, 'textured');
});

test('settings screen can select a theme directly through the shared setter', () => {
  const current = theme(null, false);
  assert.equal(typeof current.api.set, 'function');
  current.api.set('dark');
  assert.equal(current.root.dataset.theme, 'dark');
  assert.equal(current.api.get(), 'dark');
  current.api.set('bogus');
  assert.equal(current.api.get(), 'dark');
});

test('mobile shell exposes accessible theme, timeline and composer', async () => {
  const html = await readFile(new URL('index.html', ui), 'utf8');
  const app = await readFile(new URL('app.js', ui), 'utf8');
  assert(html.includes('data-theme="paper"'));
  for (const id of ['theme-choice', 'texture-choice', 'prompt', 'stop', 'files', 'settings', 'filter', 'delete', 'rename-form', 'host-info', 'unpair', 'bar', 'hist-chip', 'recover', 'bar-rewind', 'bar-live', 'history-dialog', 'hist-rewind', 'banner', 'findbar', 'find-form', 'devices', 'restart', 'rollback', 'notify', 'download', 'export', 'file-view-diff', 'logins', 'login-form', 'login-credential', 'login-start', 'attach', 'attach-input', 'attachments', 'settings-usage', 'settings-budget', 'file-open', 'model-chip', 'model-dialog', 'model-search', 'model-list', 'effort-choice', 'picker-network', 'picker-toolchain', 'title-button', 'thread-dialog', 'compaction-choice', 'budget-form', 'pending-settings', 'viewer-dialog', 'viewer-back', 'file-html', 'file-image', 'file-md', 'file-content', 'confirm-steps', 'usage-info', 'code-info', 'skin-settings']) assert(html.includes(`id="${id}"`), id);
  // Self-edit is said in the conversation, not set up in a form.
  for (const gone of ['id="dev"', 'id="dev-option"', 'id="promote"', 'id="update-source"', 'id="new-dialog"', 'id="model-form"']) assert(!html.includes(gone), gone);
  // Previews run sandboxed in an opaque origin: never with the host's authority.
  assert.match(html, /<iframe id="file-html"[^>]* sandbox="allow-scripts allow-forms allow-modals" referrerpolicy="no-referrer">/);
  assert(!html.includes('allow-same-origin') && !html.includes('allow-top-navigation'), 'no preview gets the host origin or navigates it');
  assert.match(html, /<a id="file-open"[^>]* target="_blank" rel="noopener noreferrer"/, 'Open leaves no opener or referrer');
  assert.match(html, /<input id="login-credential" type="password" autocomplete="off"/, 'credentials are masked and never autofilled');
  assert(!app.includes("drafts.set('login") && !/drafts\.set\([^)]*credential/.test(app), 'credentials are never written to local storage');
  assert(!html.includes('scrubber') && !html.includes('tape'), 'slider and tape are gone');
  assert(!html.includes('stop-banner') && !html.includes('id="actions"') && !html.includes('historybar'), 'one control row, no banner');
  assert(app.includes('renderBar') && app.includes('runIntent'), 'control row and NL intents wired');
  assert(app.includes('Recover from'), 'stopped threads offer recovery');
  for (const kind of ['delete', 'rename', 'settings']) assert(app.includes(`command('${kind}'`), `${kind} command wired`);
  for (const type of ['nested', 'output_end', 'model_text', 'usage']) assert(app.includes(`case '${type}'`), `${type} live event handled`);
  assert(app.includes('decode(event.data)'), 'output chunks are base64 text');
  assert(app.includes('seq <= catchup'), 'catch-up replay distinguishes past events');
  assert(html.includes('viewport-fit=cover'));
  assert(app.includes("$('prompt').value = ''; fitPrompt()"));
  assert(app.includes("drafts.get(`draft:${id}`);\n  fitPrompt();"), 'restored drafts must be re-fitted');
  assert(!app.includes('innerHTML'));
  const css = await readFile(new URL('style.css', ui), 'utf8');
  assert.match(css, /#prompt \{[^}]*overflow-y: hidden/);
  assert(!/^main \{[^}]*scroll-behavior: smooth/m.test(css), 'an animated follow scroll reads as the reader scrolling away');
  for (const id of ['filter', 'skin-css', 'find-text', 'prompt']) {
    const sizes = [...css.matchAll(new RegExp(`#${id}[^{]*\\{[^}]*font-size: (\\d+)px`, 'g'))].map(m => Number(m[1]));
    assert(sizes.every(size => size >= 16), `${id} must not make iOS zoom (${sizes})`);
  }
  assert(!html.includes(' style="') && !app.includes('style="'), 'style attributes are refused by the CSP (style-src self)');
  assert.match(css, /#prompt\.scrollable \{ overflow-y: auto; \}/);
});

test('dialogs and the toast reset what a skin could use to scramble or hide them', async () => {
  const css = await readFile(new URL('style.css', ui), 'utf8');
  const rule = css.split('\n').find(line => line.startsWith('dialog, #toast {'));
  for (const reset of ['-webkit-text-security: none', 'opacity: 1', 'transform: none', 'filter: none', 'clip-path: none', 'visibility: visible', 'pointer-events: auto', 'font-family: ui-monospace']) assert(rule.includes(reset), reset);
});

test('one typeface, one pill: monospace everywhere, corners square except the jump control', async () => {
  const css = await readFile(new URL('style.css', ui), 'utf8');
  assert.match(css, /^html \{\n  font-family: ui-monospace/m);
  assert(!css.includes('sans-serif'));
  assert(!css.includes('serif;'));
  const radii = css.match(/border-radius:\s*[^;]+;/g).filter(r => !/border-radius:\s*0;/.test(r));
  assert.deepEqual(radii, ['border-radius: 999px;']);
  assert(css.split('\n').find(line => line.includes('border-radius: 999px')).startsWith('#jump'));
});

test('settings, models and self-edit are typed: exact verbs only, known models only', () => {
  const models = [{provider: 'glm', id: 'glm-5.3', label: 'GLM-5.3'}, {provider: 'chatgpt', id: 'gpt-5.6', label: 'GPT-5.6'}];
  assert.deepEqual(intent('use glm-5.3', models), {action: 'model', provider: 'glm', model: 'glm-5.3'});
  assert.deepEqual(intent('switch to GPT-5.6 high', models), {action: 'model', provider: 'chatgpt', model: 'gpt-5.6', effort: 'high'});
  assert.deepEqual(intent('/model glm-5.3 max', models), {action: 'model', provider: 'glm', model: 'glm-5.3', effort: 'max'});
  assert.deepEqual(intent('/model my-finetune', models), {action: 'model', provider: null, model: 'my-finetune'}, 'the slash form takes any id');
  assert.equal(intent('use the async API', models), null);
  assert.equal(intent('use my-finetune', models), null, 'an unknown name without the slash is a message');
  assert.deepEqual(intent('model', models), {action: 'picker'});
  assert.deepEqual(intent('effort auto'), {action: 'effort', effort: 'default'});
  assert.deepEqual(intent('network on'), {action: 'settings', fields: {network: true}});
  assert.deepEqual(intent('disable network access'), {action: 'settings', fields: {network: false}});
  assert.deepEqual(intent('toolchain on'), {action: 'settings', fields: {toolchain: true}});
  assert.deepEqual(intent('set budget to $2.50'), {action: 'settings', fields: {max_usd: 2.5}});
  assert.deepEqual(intent('no budget'), {action: 'settings', fields: {max_usd: null}});
  assert.deepEqual(intent('compaction digest'), {action: 'settings', fields: {compaction: 'digest'}});
  assert.deepEqual(intent('/install'), {action: 'install'});
  assert.deepEqual(intent('install the change'), {action: 'install'});
  assert.equal(intent('install'), null, '"install" alone may mean dependencies: it goes to the model');
  assert.deepEqual(intent('update from host source'), {action: 'update'});
  assert.equal(intent('update the README'), null);
  assert.deepEqual(intent('host settings'), {action: 'host'});
  assert.deepEqual(intent('thread menu'), {action: 'menu'});
  assert.equal(intent('network is down, retry the upload'), null);
});

test('Markdown becomes a tree of blocks and runs; markup and unsafe links stay text', () => {
  const tree = text.markdown('# Title\n\nSome **bold**, *em*, `code` and snake_case_name.\n\n- one\n- two\n  - nested\n\n3. three\n4. four\n\n| a | b |\n|---|--:|\n| 1 | 2 |\n\n> quoted\n\n---\n\n```js\nlet x = 1;\n```\n<script>alert(1)</script> [x](javascript:alert(1)) [ok](https://ok.example/a)');
  assert.deepEqual(tree.map(b => b.type), ['heading', 'paragraph', 'list', 'list', 'table', 'quote', 'rule', 'code', 'paragraph']);
  assert.deepEqual(tree[1].inline.map(r => r.type), ['text', 'strong', 'text', 'em', 'text', 'code', 'text']);
  assert.equal(tree[1].inline.at(-1).text, ' and snake_case_name.', 'intra-word underscores are not emphasis');
  assert.equal(tree[2].items[1][1].type, 'list', 'an indented list nests');
  assert.equal(tree[3].ordered, true); assert.equal(tree[3].start, 3);
  assert.deepEqual(tree[4].align, ['', 'right']);
  assert.deepEqual(tree[7], {type: 'code', lang: 'js', code: 'let x = 1;'});
  const last = tree[8].inline;
  assert(last.some(r => r.type === 'text' && r.text.includes('<script>alert(1)</script>')), 'markup is text');
  assert(last.some(r => r.type === 'text' && r.text.includes('[x](javascript:alert(1))')), 'only http(s) links link');
  assert.deepEqual(last.find(r => r.type === 'link'), {type: 'link', href: 'https://ok.example/a', children: [{type: 'text', text: 'ok'}]});
  assert.deepEqual(text.inline('see https://example.com/a.'), [{type: 'text', text: 'see '}, {type: 'link', href: 'https://example.com/a', children: [{type: 'text', text: 'https://example.com/a'}]}, {type: 'text', text: '.'}]);
  assert.deepEqual(text.inline('2 * 3 * 4'), [{type: 'text', text: '2 * 3 * 4'}], 'a spaced star is not emphasis');
  assert.equal(text.inline('*'.repeat(30000)).length, 1, 'very long runs stay literal');
});

test('paths of the revision become links; tool results get a one-line summary', () => {
  const known = new Set(['src/lib.rs', 'Cargo.toml', 'docs/a b.md']);
  assert.deepEqual(text.pathParts('edit src/lib.rs:41:13, then ./Cargo.toml.', known), [{text: 'edit '}, {path: 'src/lib.rs', text: 'src/lib.rs'}, {text: ':41:13, then '}, {path: 'Cargo.toml', text: './Cargo.toml'}, {text: '.'}]);
  assert.deepEqual(text.pathParts('/workspace/src/lib.rs', known), [{path: 'src/lib.rs', text: '/workspace/src/lib.rs'}]);
  assert.deepEqual(text.pathParts('https://h.example/src/lib.rs and lib.rs', known), [{text: 'https://h.example/src/lib.rs and lib.rs'}], 'a URL or a bare name is not a workspace path');
  assert.deepEqual(text.pathParts('nothing here', new Set()), [{text: 'nothing here'}]);
  assert.equal(text.resultSummary({name: 'bash'}, {content: 'compiling\n\n  test result: ok. 4 passed\n'}), 'test result: ok. 4 passed', 'a command shows its last line');
  assert.equal(text.resultSummary({name: 'bash'}, {content: 'error[E0425]: not found\nmore', is_error: true}), 'error[E0425]: not found');
  assert.equal(text.resultSummary({name: 'edit_file', input: {old_string: 'a\nb', new_string: 'a\nc\nd'}}, {content: 'Edited'}), '+2 −1');
  assert.equal(text.resultSummary({name: 'write_file', input: {content: 'x\ny\n'}}, {content: 'Wrote'}), '2 lines');
  assert.equal(text.resultSummary({name: 'read_file'}, {content: '1\ta\n2\tb\n3\tc'}), '3 lines');
  assert.equal(text.resultSummary({name: 'bash'}, {content: ''}), 'no output');
  assert.equal(text.resultSummary({name: 'bash'}, null), '');
  assert.equal(text.settingsLine({model: 'glm-5.3', effort: 'max'}), 'glm-5.3 · max');
  assert.equal(text.settingsLine({provider: 'glm', effort: 'default', network: false, max_usd: null, compaction: 'digest'}), 'glm default · auto effort · network off · digest compaction · no budget');
  assert.equal(text.priceLine({input: 1.4, output: 4.4}), '$1.40 / $4.40');
  assert.equal(text.priceLine({input: 0.07, output: 0.28}), '$0.07 / $0.28');
  assert.equal(text.priceLine(null), '');
});
