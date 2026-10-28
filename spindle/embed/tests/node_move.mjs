// Sessions moving in place between the package and a native host:
// one directory layout, so a session made by either continues in the other once the
// first closes it, each refused while the other holds it, tasks included. Both talk
// to one scripted model over loopback HTTP. Runs when SPINDLE_BIN names a native
// spindle binary.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { StdioClient } from '../../clients/js/spindle.mjs';
import { nodeClient } from '../../clients/js/embedded-node.mjs';
import { call, sse, text } from './scripted.mjs';

const glue = process.argv[2];
if (!glue) throw new Error('usage: node node_move.mjs SPINDLE_EMBED_GLUE.js');
const binary = process.env.SPINDLE_BIN;
if (!binary) {
  console.log('node-move skipped: set SPINDLE_BIN to a native spindle binary');
  process.exit(0);
}

// One scripted model for both hosts: replies in order, each request kept.
const replies = [];
const requests = [];
const model = http.createServer((request, response) => {
  let body = '';
  request.on('data', (chunk) => { body += chunk; });
  request.on('end', () => {
    requests.push(JSON.parse(body));
    const reply = replies.shift() ?? text('(script ended)');
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    response.end(sse(reply, requests.length));
  });
});
await new Promise((resolve) => model.listen(0, '127.0.0.1', resolve));
const port = model.address().port;

const catalog = {
  version: 4, active_provider: 'mover', providers: [{
    id: 'mover', display_name: 'Mover', api_provider: 'openai',
    request_contract: 'openai-chat-completions', base_url: `http://127.0.0.1:${port}/v1`,
    default_model: 'mover-model', models: ['mover-model'], requires_api_key: false,
    model_specs: { 'mover-model': { context_window: 200000, max_output_tokens: 4096 } },
  }],
};
const root = fs.mkdtempSync(join(tmpdir(), 'spindle-node-move-'));
const home = join(root, 'home');
const env = { ...process.env, HOME: home, DEXT_HOME: join(home, '.dext') };
fs.mkdirSync(env.DEXT_HOME, { recursive: true });
fs.writeFileSync(join(env.DEXT_HOME, 'providers.json'), JSON.stringify(catalog));

const pkg = () => nodeClient({ glue, catalog, credentials: async () => ({}), fetch: globalThis.fetch });
const native = () => new StdioClient({ binary, env, stderr: 'ignore' });
const locked = /another spindle process or connection/;
const taskDirs = (dir) => fs.readdirSync(join(dir, 'tasks')).map((key) => join(dir, 'tasks', key));

async function packageToNative() {
  const dir = join(root, 'from-package');
  const app = pkg();
  await app.create(dir, 'mover', { tasks: true });
  replies.push(text('hello from the model'));
  assert.equal((await app.prompt(dir, 'hello')).stopped.complete, true);
  replies.push(call('write_file', { path: 't.txt', content: 'from the task\n' }), text('task done'));
  const task = await app.taskStart(dir, 't1', 'write t.txt');
  assert.match(task.answer, /^task done/);
  await app.close();
  // The native layout: a settings mirror, and a task directory with its identity.
  assert.equal(JSON.parse(fs.readFileSync(join(dir, 'session.json'))).provider, 'mover');
  const [child] = taskDirs(dir);
  assert.ok(fs.existsSync(join(child, 'session.json')), 'the task is a session directory');
  assert.equal(JSON.parse(fs.readFileSync(join(child, 'task.json'))).prompt, 'write t.txt');
  assert.ok(!fs.existsSync(join(dir, 'store', 'authority.sqlite.lock')), 'the package let it go');

  const host = native();
  const head = (await host.status(dir)).head;
  replies.push(text('the native host continues'));
  const turn = await host.prompt(dir, 'carry on natively');
  assert.equal(turn.stopped.complete, true);
  assert.equal(turn.head > head, true);
  const last = requests.at(-1).messages.map((m) => JSON.stringify(m.content)).join(' ');
  assert.match(last, /hello from the model/, 'the moved conversation reaches the model');
  // The same task id, the same input: the native host finds the task the package ran.
  const sent = requests.length;
  const again = await host.taskStart(dir, 't1', 'write t.txt');
  assert.match(again.answer, /^task done/);
  assert.equal(requests.length, sent, 'the finished task is not run again');
  await assert.rejects(host.taskStart(dir, 't1', 'something else'), /task was started with another prompt/);
  // While the native host holds it, the package is refused; once closed, it opens.
  const blocked = pkg();
  await assert.rejects(blocked.status(dir), locked, 'the package sees the native owner');
  await blocked.close();
  await host.closeSession(dir);
  await host.close();
  const back = pkg();
  replies.push(text('back in the package'));
  assert.equal((await back.prompt(dir, 'and back')).stopped.complete, true);
  await back.close();
}

async function nativeToPackage() {
  const dir = join(root, 'from-native');
  const host = native();
  await host.create(dir, 'mover', { isolation: 'off' });
  replies.push(text('a native turn'));
  assert.equal((await host.prompt(dir, 'hello natively')).stopped.complete, true);
  // The package is refused while the native host holds the session.
  const blocked = pkg();
  await assert.rejects(blocked.status(dir), locked);
  await blocked.close();
  await host.closeSession(dir);
  await host.close();

  const app = pkg();
  const status = await app.status(dir);
  assert.equal(status.settings.builtin_tools, 'all', 'settings move unchanged');
  const sent = requests.length;
  await assert.rejects(app.prompt(dir, 'in the app'), /settings cannot run here/);
  assert.equal(requests.length, sent, 'nothing was sent');
  await app.settings(dir, { builtin_tools: 'files', compaction: 'digest' });
  replies.push(text('the package continues'));
  assert.equal((await app.prompt(dir, 'in the app')).stopped.complete, true);
  assert.equal(JSON.parse(fs.readFileSync(join(dir, 'session.json'))).builtin_tools, 'files');
  await app.close();
  return dir;
}

/** A native owner killed while it holds the session leaves its lock: the package is
 * refused until a native open takes the lock over. */
async function aKilledNativeOwner(dir) {
  const host = native();
  replies.push(text('then the host dies'));
  await host.prompt(dir, 'one more');
  host.process.kill('SIGKILL');
  await new Promise((resolve) => host.process.once('exit', resolve));
  const note = fs.readFileSync(join(dir, 'store', 'authority.sqlite.lock'), 'utf8');
  assert.match(note, /^spindle-native /);
  const blocked = pkg();
  await assert.rejects(blocked.status(dir), locked, 'fail-closed: the package cannot tell it died');
  await blocked.close();
  const next = native();
  replies.push(text('a native host takes over'));
  assert.equal((await next.prompt(dir, 'take over')).stopped.complete, true);
  await next.closeSession(dir);
  await next.close();
  assert.ok(!fs.existsSync(join(dir, 'store', 'authority.sqlite.lock')));
  const app = pkg();
  assert.equal((await app.status(dir)).complete, true);
  await app.close();
}

/** Moving by session bundle (Step 4): the package seals its copy and a native host
 * continues the bundle (its task found, not run again); the native host moves it back
 * into the package; a kept copy stays usable. */
async function bundlesBothWays() {
  const dir = join(root, 'bundled');
  const app = pkg();
  await app.create(dir, 'mover', { tasks: true });
  replies.push(text('before the move'));
  await app.prompt(dir, 'hello');
  replies.push(call('write_file', { path: 't.txt', content: 'from the task\n' }), text('task done'));
  await app.taskStart(dir, 't1', 'write t.txt');
  const head = (await app.status(dir)).head;
  const bytes = await app.exportSession(dir);
  await assert.rejects(app.status(dir), /moved elsewhere/, 'its old place is sealed');
  await app.close();
  assert.match(fs.readFileSync(join(dir, 'store', 'authority.sqlite.lock'), 'utf8'), /^spindle-moved: /);
  const file = join(root, 'bundled.spindle');
  fs.writeFileSync(file, bytes);
  const host = native();
  await assert.rejects(host.prompt(dir, 'not here'), /moved elsewhere/, 'natively too');
  const moved = join(root, 'bundled-native');
  const imported = await host.importSession(moved, file);
  assert.equal(imported.head, head);
  replies.push(text('after the move'));
  assert.equal((await host.prompt(moved, 'carry on')).stopped.complete, true);
  const last = requests.at(-1).messages.map((m) => JSON.stringify(m.content)).join(' ');
  assert.match(last, /before the move/);
  const sent = requests.length;
  assert.match((await host.taskStart(moved, 't1', 'write t.txt')).answer, /^task done/);
  assert.equal(requests.length, sent, 'the moved task is found, not run again');
  const back = join(root, 'back.spindle');
  const exported = await host.exportSession(moved, back);
  assert.equal(exported.moved, true);
  await host.close();
  const home = pkg();
  const into = join(root, 'bundled-back');
  // As natively, an import needs an empty place.
  fs.mkdirSync(into);
  fs.writeFileSync(join(into, 'stray.txt'), 'left here');
  await assert.rejects(home.importSession(into, fs.readFileSync(back)), (error) => error.kind === 'conflict');
  fs.rmSync(join(into, 'stray.txt'));
  const status = await home.importSession(into, fs.readFileSync(back));
  assert.equal(status.imported.head, exported.head);
  replies.push(text('home again'));
  assert.equal((await home.prompt(into, 'home')).stopped.complete, true);
  const copy = await home.exportSession(into, { keep: true });
  assert.ok(copy.length > 0);
  assert.equal((await home.status(into)).complete, true, 'a kept copy leaves the session here');
  await home.close();
}

try {
  await packageToNative();
  const dir = await nativeToPackage();
  if (process.platform !== 'win32') await aKilledNativeOwner(dir);
  await bundlesBothWays();
} finally {
  model.close();
  fs.rmSync(root, { recursive: true, force: true });
}
console.log(
  'node-move passed: sessions moved in place between the package and native spindle both ways ' +
    '(conversation, task identity and files kept; each refused while the other holds it; a ' +
    "native settings change asked for; a killed native owner's lock taken over natively only); " +
    'and by session bundle both ways (old place sealed, task identity kept, a kept copy)',
);
