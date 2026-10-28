// The embedded package under Node against a scripted model (a CI test): the wasm
// build through EmbeddedClient, sessions in directories through the Node VFS. A turn
// with file tools whose credentials never reach the store, an app tool with files and
// state, an app runner, reopening, a second owner refused, an owner killed between
// send and response (unknown, never resent by resume, sent once by retry after the
// operator clears its lock), a "read it now" cut, and stop.
import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { nodeClient, remoteRunner } from '../../clients/js/embedded-node.mjs';
import { KEY, call, catalog, hold, scriptedFetch, text } from './scripted.mjs';

const glue = process.argv[2];
if (!glue) throw new Error('usage: node node_session.mjs SPINDLE_EMBED_GLUE.js');
const root = fs.mkdtempSync(join(tmpdir(), 'spindle-node-session-'));
const credentials = async ({ headers }) => {
  assert.ok(!('authorization' in headers), 'Spindle sends no authentication of its own');
  return { authorization: `Bearer ${KEY}` };
};
const client = (fetch, options = {}) => nodeClient({ glue, catalog: catalog(), credentials, fetch, ...options });

function filesUnder(dir) {
  return fs.readdirSync(dir, { recursive: true, withFileTypes: true })
    .filter((entry) => entry.isFile())
    .map((entry) => join(entry.parentPath ?? entry.path, entry.name));
}

async function aTurnWithFileTools() {
  const dir = join(root, 'files');
  const fetch = scriptedFetch([
    call('write_file', { path: 'a.txt', content: 'one\n' }),
    call('edit_file', { path: 'a.txt', old_string: 'one', new_string: 'two' }),
    text('done'),
  ]);
  const c = client(fetch);
  const events = [];
  c.on('event', (event) => events.push(event));
  await c.create(dir, 'fake');
  const result = await c.prompt(dir, 'write a file');
  assert.equal(result.stopped.complete, true);
  assert.equal(new TextDecoder().decode(await c.read(dir, 'a.txt')), 'two\n');
  assert.equal(fetch.seen.length, 3);
  for (const request of fetch.seen) {
    assert.equal(request.url, 'https://fake.invalid/v1/chat/completions');
    assert.equal(request.headers.authorization, `Bearer ${KEY}`);
    assert.equal(request.headers.accept, 'text/event-stream');
  }
  assert.ok(events.some((e) => e.type === 'model_delta' && e.text === 'done'), 'replies stream as previews');
  assert.equal(events.filter((e) => e.type === 'tool_result').length, 2);
  const history = await c.history(dir);
  assert.equal(history.messages.at(-1).content[0].text, 'done');
  await c.close();
  for (const file of filesUnder(dir)) {
    assert.ok(!fs.readFileSync(file).includes(Buffer.from(KEY)), `the key is in ${file}`);
  }
  assert.ok(!fs.existsSync(join(dir, 'store', 'authority.sqlite.lock')), 'closing removes the lock');
  if (process.platform !== 'win32') {
    assert.equal(fs.statSync(join(dir, 'store')).mode & 0o777, 0o700, 'the store directory is private');
  }
  return dir;
}

async function reopenAndAppTool(dir) {
  const fetch = scriptedFetch([call('lookup', { q: 'notes' }), text('found them')]);
  const c = client(fetch);
  c.tool({
    name: 'lookup', description: 'Looks notes up.',
    inputSchema: { type: 'object', properties: { q: { type: 'string' } } },
    run: async (input) => ({ content: `notes for ${input.q}`, files: { write: { 'notes/a.md': 'first' } }, state: { found: 1 } }),
  });
  const status = await c.status(dir);
  assert.ok(status.head > 0, 'the session reopens with its history');
  await c.settings(dir, { tools: c.tools.definitions() });
  const result = await c.prompt(dir, 'find my notes');
  assert.equal(result.stopped.complete, true);
  assert.equal(new TextDecoder().decode(await c.read(dir, 'notes/a.md')), 'first');
  assert.deepEqual((await c.state(dir)).state, { found: 1 });
  assert.ok(fetch.seen[1].body.includes('notes for notes'), 'the answer reaches the model');

  const second = client(scriptedFetch([]));
  await assert.rejects(second.status(dir), /another spindle process or connection/, 'a second owner is refused while the first holds the store');
  await second.close();
  await c.close();
}

async function anAppRunner() {
  const dir = join(root, 'runner');
  const runs = [];
  const runner = {
    async run({ command, files, read }) {
      runs.push(command);
      const input = files.find((f) => f.path === 'in.txt');
      return { output: 'copied\n', success: true, writes: [{ path: 'out.txt', content: read(input.object) }] };
    },
  };
  const c = client(scriptedFetch([
    call('write_file', { path: 'in.txt', content: 'data' }),
    call('bash', { command: 'cp in.txt out.txt' }),
    text('copied it'),
  ]), { runner });
  await c.create(dir, 'fake', { runner: 'app' });
  const result = await c.prompt(dir, 'copy the file');
  assert.equal(result.stopped.complete, true);
  assert.equal(runs.length, 1);
  assert.ok(runs[0].endsWith('cp in.txt out.txt'));
  assert.equal(new TextDecoder().decode(await c.read(dir, 'out.txt')), 'data');
  await c.close();
}

// Both runners at once: each run goes to the runner its session names, and an app
// runner that declares a sandbox runs a session that requires isolation, unannounced.
async function twoRunners() {
  const ran = [];
  const runner = (kind, identity) => ({
    ...(kind === 'remote' && { kind }),
    ...(identity && { identity: async () => identity }),
    async run({ command }) {
      ran.push([kind, command]);
      return { output: `${kind}\n`, success: true };
    },
  });
  const events = [];
  const fetch = scriptedFetch([
    call('bash', { command: 'echo here' }), text('ran here'),
    call('bash', { command: 'echo there' }), text('ran there'),
  ]);
  const c = client(fetch, { runner: { app: runner('app', 'a'.repeat(64)), remote: runner('remote', 'b'.repeat(64)) } });
  c.on('event', (event) => events.push(event));
  await c.ready;
  const here = join(root, 'two-app');
  const there = join(root, 'two-remote');
  await c.create(here, 'fake', { runner: 'app', isolation: 'required' });
  await c.create(there, 'fake', { runner: 'remote' });
  await c.prompt(here, 'run it here');
  await c.prompt(there, 'run it there');
  assert.deepEqual(ran.map(([kind]) => kind), ['app', 'remote'], 'each run went to its own runner');
  assert.ok(!events.some((event) => event.type === 'unisolated'), 'a declared sandbox is not unisolated');
  // Disconnected, a session's runs are refused rather than sent elsewhere.
  await c.connectRunner('remote', null);
  fetch.replies.push(call('bash', { command: 'echo again' }), text('no'));
  await assert.rejects(c.prompt(there, 'run it there again'), /not connected/);
  assert.equal(ran.length, 2, 'no runner was asked');
  await c.close();
}

// A script in the app's runner asks for Spindle calls (the code-mode bridge), through
// the wasm package: a nested model call and a tool, journaled, then replayed.
async function aScriptsCalls() {
  const dir = join(root, 'bridge');
  const fetch = scriptedFetch([
    call('bash', { command: 'python3 script.py' }),
    text('nested answer'),
    text('done'),
  ]);
  // A site the app grants: what the client's fetch sends there.
  const sent = [];
  const web = async (url, init) => {
    if (!String(url).startsWith('https://notes.example/')) return fetch(url, init);
    sent.push({ url: String(url), init });
    if (String(url).endsWith('/old')) return new Response(null, { status: 302, headers: { location: 'https://elsewhere.example/' } });
    return new Response('{"items":[1,2]}', { headers: { 'content-type': 'application/json' } });
  };
  const replies = [];
  let c;
  const runner = {
    async run({ session }) {
      replies.push(await c.code(session, { method: 'model', id: 0, input: { prompt: 'say it' } }));
      replies.push(await c.code(session, { method: 'tool', id: 1, input: { name: 'lookup', arguments: { q: 'x' } } }));
      replies.push(await c.code(session, { method: 'tool', id: 1, input: { name: 'lookup', arguments: { q: 'x' } } }));
      replies.push(await c.code(session, { method: 'http', id: 2, input: { url: 'https://notes.example/api/items' } }));
      replies.push(await c.code(session, { method: 'http', id: 3, input: { url: 'https://notes.example/api/old' } }));
      replies.push(await c.code(session, { method: 'http', id: 4, input: { url: 'https://elsewhere.example/' } }));
      // The command ended: nothing more starts (its children stay uncertain, never sent).
      const before = fetch.seen.length;
      replies.push(await c.code(session, { method: 'model', id: 5, input: { prompt: 'too late' } }, { signal: AbortSignal.abort() })
        .then(() => 'answered', (error) => `${error.kind}: ${fetch.seen.length - before} sent`));
      return { output: 'ran\n', success: true };
    },
  };
  c = client(web, { runner });
  assert.throws(() => c.setHttpGrants([{ name: 'x', url: 'https://x.example/', risk: 'read', headers: { a: 'b' } }]), /no headers/);
  c.setHttpGrants([{ name: 'notes', url: 'https://notes.example/api/', risk: 'read' }]);
  let looked = 0;
  c.tool({ name: 'lookup', description: 'Looks a word up', inputSchema: { type: 'object', properties: { q: { type: 'string' } }, required: ['q'], additionalProperties: false },
    run: ({ q }) => { looked += 1; return `found ${q}`; } });
  await c.create(dir, 'fake', { runner: 'app' });
  await c.prompt(dir, 'run the script');
  assert.equal(replies[0].ok, true, JSON.stringify(replies[0]));
  assert.equal(replies[0].data.text, 'nested answer');
  assert.deepEqual(replies[1], { ok: true, data: 'found x' });
  assert.deepEqual(replies[2], { ok: true, data: 'found x' }, 'replayed from the journal');
  assert.equal(looked, 1, 'the replay ran nothing');
  assert.deepEqual(replies[3].data.json, { items: [1, 2] }, JSON.stringify(replies[3]));
  assert.equal(replies[3].data.status, 200);
  assert.equal(sent[0].init.credentials, 'omit', 'no cookies go with it');
  assert.equal(sent[0].init.redirect, 'manual', 'no redirect is followed');
  assert.equal(replies[4].ok, false, JSON.stringify(replies[4]));
  assert.match(replies[4].error, /redirect/);
  assert.equal(replies[4].unknown, undefined, 'a redirect is the server answering');
  assert.match(replies[5].error, /no HTTP grant covers/, JSON.stringify(replies[5]));
  assert.equal(sent.length, 2, 'nothing was sent where no grant covers');
  assert.equal(replies[6], 'stopped: 0 sent', 'a stopped command starts no more calls');
  const nested = fetch.seen[1];
  assert.equal(nested.headers.authorization, `Bearer ${KEY}`, 'the nested request carried the key');
  assert.ok(!nested.body.includes('"tools"'), 'and no tools');
  await c.close();
}

async function aKilledOwner() {
  const dir = join(root, 'crash');
  const child = spawn(process.execPath, [fileURLToPath(new URL('node_session_child.mjs', import.meta.url)), glue, dir], { stdio: ['ignore', 'pipe', 'inherit'] });
  await new Promise((resolve, reject) => {
    let out = '';
    child.stdout.on('data', (chunk) => { out += chunk; if (out.includes('sent')) resolve(); });
    child.on('exit', (code) => reject(new Error(`the owner exited (${code}) before sending`)));
  });
  const ended = new Promise((resolve) => child.once('exit', resolve));
  child.kill('SIGKILL');
  await ended;
  const fetch = scriptedFetch([text('sent again')]);
  const c = client(fetch);
  await assert.rejects(c.status(dir), /another spindle process or connection/, "a killed owner's lock is never taken over");
  fs.unlinkSync(join(dir, 'store', 'authority.sqlite.lock')); // the operator's decision
  await assert.rejects(c.resume(dir), (error) => error.kind === 'failed', 'resume does not send an unknown request again');
  assert.equal(fetch.seen.length, 0);
  const recover = await c.recover(dir);
  assert.equal(recover.wake.work, 'blocked');
  const retried = await c.retry(dir);
  assert.equal(retried.stopped.complete, true);
  assert.equal(fetch.seen.length, 1, 'retry sends it once');
  assert.ok(fs.existsSync(join(dir, 'store.abandoned-1', 'authority.sqlite')), 'the old store is kept aside');
  await c.close();
}

async function readItNowAndStop() {
  const dir = join(root, 'steer');
  const fetch = scriptedFetch([call('write_file', { path: 'a.txt', content: '1' }), hold('unfinished'), text('after the message')]);
  const c = client(fetch);
  const events = [];
  c.on('event', (event) => events.push(event));
  fetch.holding = () => setTimeout(() => c.steer(dir, 'read this now', { now: true }), 20);
  await c.create(dir, 'fake');
  const result = await c.prompt(dir, 'go');
  assert.equal(result.stopped.complete, true);
  assert.ok(events.some((e) => e.type === 'reply_cut'), 'the streaming reply is cut');
  assert.equal(fetch.seen.length, 3);
  assert.ok(fetch.seen[2].body.includes('read this now'), 'the message is read first');

  const stopFetch = scriptedFetch([hold('working')]);
  const s = client(stopFetch);
  const stopDir = join(root, 'stop');
  stopFetch.holding = () => setTimeout(() => s.stop(stopDir), 20);
  await s.create(stopDir, 'fake');
  await assert.rejects(s.prompt(stopDir, 'go'), (error) => error.kind === 'stopped');
  await assert.rejects(s.resume(stopDir), (error) => error.kind === 'failed');
  assert.equal(stopFetch.seen.length, 1, 'a stopped request is never sent again by resume');
  await s.close();
  await c.close();
}

/** `bash` run by a Spindle host (`spindle serve`, /v1/run) for an embedded session;
 * runs when SPINDLE_BIN names a spindle binary. */
/** A long history compacts by a summary the session's model writes: one more request
 * the client authorizes and fetches like any other, its text in the compacted history. */
async function aModelWrittenSummary() {
  const dir = join(root, 'summary');
  const long = (tag) => text(`${tag} ${'lorem ipsum dolor sit amet '.repeat(1700)}`);
  const fetch = scriptedFetch([long('first'), long('second'), long('third'), text('SUMMARY: carry on'), text('done')]);
  const c = client(fetch);
  const events = [];
  c.on('event', (event) => events.push(event));
  await c.create(dir, 'fake', { model: 'small-model' });
  assert.equal((await c.status(dir)).settings.compaction, 'model', 'new sessions compact by model');
  for (const prompt of ['one', 'two', 'three', 'four']) {
    assert.equal((await c.prompt(dir, prompt)).stopped.complete, true);
  }
  assert.equal(fetch.seen.length, 5, 'three turns, the summary, the fourth turn');
  assert.match(fetch.seen[3].body, /Summarize the session history below/);
  for (const request of fetch.seen) assert.equal(request.headers.authorization, `Bearer ${KEY}`);
  assert.match(fetch.seen[4].body, /SUMMARY: carry on/);
  assert.doesNotMatch(fetch.seen[4].body, /first lorem/);
  assert.equal(events.filter((e) => e.type === 'compacted').length, 1);
  await c.close();
}

async function aRemoteRunner() {
  const binary = process.env.SPINDLE_BIN;
  if (!binary) {
    console.log('remote runner skipped: set SPINDLE_BIN to a spindle binary');
    return;
  }
  const home = join(root, 'home');
  const env = { ...process.env, HOME: home, DEXT_HOME: join(home, '.dext') };
  fs.mkdirSync(env.DEXT_HOME, { recursive: true });
  const host = join(root, 'host');
  const token = execFileSync(binary, ['--dir', host, 'token', 'new', 'runner'], { env, encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim();
  const serve = spawn(binary, ['--dir', host, 'serve', '--port', '0'], { env, stdio: ['ignore', 'pipe', 'ignore'] });
  try {
    const port = await new Promise((resolve) => serve.stdout.once('data', (text) => resolve(/127\.0\.0\.1:(\d+)/.exec(String(text))[1])));
    // A host runs remote commands only in its sandbox (none here: it answers 503).
    const probe = await fetch(`http://127.0.0.1:${port}/v1/runner`, { headers: { authorization: `Bearer ${token}` } });
    if (probe.status === 503) {
      console.log('remote runner skipped: this host has no sandbox, so it runs no remote commands');
      return;
    }
    assert.equal(probe.status, 200);
    const runner = remoteRunner({ url: `http://127.0.0.1:${port}`, token });
    const dir = join(root, 'remote');
    const c = client(scriptedFetch([
      call('write_file', { path: 'in.txt', content: 'from the app' }),
      call('bash', { command: 'tr a-z A-Z < in.txt > out.txt && rm in.txt' }),
      text('done remotely'),
    ]), { runner });
    await c.ready;
    await c.create(dir, 'fake', { runner: 'remote' });
    const result = await c.prompt(dir, 'shout it');
    assert.equal(result.stopped.complete, true);
    assert.equal(new TextDecoder().decode(await c.read(dir, 'out.txt')), 'FROM THE APP');
    await assert.rejects(c.read(dir, 'in.txt'), (error) => error.kind === 'not_found', 'the run removed it');
    await c.close();
  } finally {
    serve.kill();
  }
}

try {
  const dir = await aTurnWithFileTools();
  await reopenAndAppTool(dir);
  await anAppRunner();
  await twoRunners();
  await aScriptsCalls();
  await aKilledOwner();
  await readItNowAndStop();
  await aModelWrittenSummary();
  await aRemoteRunner();
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
console.log(
  'node-session passed: wasm package via EmbeddedClient against a scripted model (file tools, ' +
    'credentials kept out of the store, app tool with files/state, app runner, app and remote runners at once (a declared sandbox isolates), Spindle calls from a script journaled and replayed (http() only where a grant allows), reopen, second ' +
    'owner refused, killed owner: unknown kept and sent once by retry, read-it-now cut, stop, ' +
    'compaction by a model-written summary, ' +
    'remote runner on spindle serve when SPINDLE_BIN is set)',
);
