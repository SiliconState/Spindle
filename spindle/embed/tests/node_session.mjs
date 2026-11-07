// The embedded package under Node against a scripted model (a CI test): the wasm
// build through EmbeddedClient, sessions in directories through the Node VFS. A turn
// with file tools whose credentials never reach the store, an app tool with files and
// state, an app runner, reopening, a second owner refused, an owner killed between
// send and response (unknown, never resent by resume, sent once by retry after the
// operator clears its lock), a "read it now" cut, and stop.
import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import { hostname, tmpdir } from 'node:os';
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
  assert.equal(status.activity, 'idle');
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

// The user stops a command the app's runner runs: the runner is told (its signal) and
// its answer, what the command printed, is recorded as stopped. After a reopen a reply
// goes on with the turn, the model reading the stopped result and then the reply, and
// nothing runs the command again. A runner that does not answer is recorded as stopped
// at a second stop.
async function aStoppedCommand() {
  const dir = join(root, 'stopped-command');
  const runs = [];
  let c;
  const runner = {
    run({ command, signal }) {
      runs.push(command);
      setTimeout(() => c.stop(dir), 20);
      return new Promise((done) => signal.addEventListener('abort', () => done({
        output: 'started\n', success: false, failure: 'the command was stopped',
      })));
    },
  };
  const fetch = scriptedFetch([call('bash', { command: 'echo started; sleep 100' }), text('skipped it')]);
  c = client(fetch, { runner });
  await c.create(dir, 'fake', { runner: 'app' });
  await assert.rejects(c.prompt(dir, 'run it'), (error) => error.kind === 'stopped');
  const status = await c.status(dir);
  assert.equal(status.phase, 'after_tools');
  assert.equal(status.accepts_message, true);
  assert.equal(status.accepts_prompt, false);
  await c.close();
  // Reopened, as an app that restarted: a reply goes on with the stopped turn.
  c = client(fetch, { runner });
  const result = await c.prompt(dir, 'skip it and say so');
  assert.equal(result.stopped.complete, true);
  assert.equal(runs.length, 1, 'the stopped command never runs again');
  const next = fetch.seen[1].body;
  const stopped = next.indexOf('started\\n\\n[the user stopped this command');
  assert.ok(stopped >= 0 && stopped < next.indexOf('skip it and say so'), next);
  await c.close();

  const silent = join(root, 'stopped-silent');
  const quiet = { run() { return new Promise(() => {}); } };
  const s = client(scriptedFetch([call('bash', { command: 'sleep 100' }), text('ok')]), { runner: quiet });
  await s.create(silent, 'fake', { runner: 'app' });
  const turn = s.prompt(silent, 'run it');
  setTimeout(() => s.stop(silent), 50);
  setTimeout(() => s.stop(silent), 150);
  await assert.rejects(turn, (error) => error.kind === 'stopped');
  const history = await s.history(silent);
  assert.match(JSON.stringify(history), /the user stopped this command/);
  assert.equal((await s.status(silent)).accepts_message, true);
  await s.close();
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
  assert.throws(() => c.setHttpGrants([{ name: 'x', url: 'https://x.example/', risk: 'read', headers: { a: 'b' } }]), /the app supplies/);
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

// A grant that adds a secret header: the app gives its value as each request goes, and
// the script, the journal and the reply never hold it (an echo of it is taken out, in
// the forms servers send it back in, before any cut); without a usable value nothing
// is sent.
async function aGrantsSecretHeader() {
  const dir = join(root, 'keyed');
  const fetch = scriptedFetch([call('bash', { command: 'python3 keyed.py' }), text('done')]);
  const token = `sk/site+${'0123456789abcdef'.repeat(3)}`;
  const value = `Bearer ${token}`;
  const pct = encodeURIComponent(token);
  const b64 = Buffer.from(`x${token}`).toString('base64');
  const seen = [];
  const web = async (url, init) => {
    if (!String(url).startsWith('https://keyed.example/')) return fetch(url, init);
    seen.push({ path: new URL(url).pathname, authorization: init.headers.authorization });
    const path = new URL(url).pathname;
    if (path === '/api/echo') {
      const body = `{"auth":${JSON.stringify(value)},"slashed":"${token.replaceAll('/', '\\/')}","pct":"${pct}","b64":"${b64}","url64":"${b64.replaceAll('+', '-').replaceAll('/', '_')}"}`;
      return new Response(body, { headers: { 'content-type': 'application/json', 'x-echo': value } });
    }
    const streamed = (...parts) => new Response(new ReadableStream({
      start(controller) {
        for (const part of parts) controller.enqueue(new TextEncoder().encode(part));
        controller.close();
      },
    }));
    // A secret across the response limit, its first part in a chunk that ends past it:
    // read on and taken out whole before the cut.
    if (path === '/api/edge') return streamed(`${'x'.repeat(4096 - 20)}${token.slice(0, 25)}`, `${token.slice(25)}${'y'.repeat(200)}`);
    // Read only so far past the limit, a body can stop partway into a secret, and the
    // room the whole ones before it give back brings that part under the limit.
    if (path === '/api/many') return streamed(`${`${token} `.repeat(100)}${token.slice(0, 30)}`, `${token.slice(30)} rest`);
    return new Response('{}', { headers: { 'content-type': 'application/json' } });
  };
  const asked = [];
  const grantHeaders = async (facts) => {
    asked.push(facts);
    if (facts.url.endsWith('/locked')) throw new Error("Spindle's keys are locked: unlock them");
    if (facts.url.endsWith('/bad')) return { Authorization: 'Bearer a\nb' };
    if (facts.url.endsWith('/short')) return { Authorization: 'Bearer abc' };
    return { authorization: value };
  };
  const replies = [];
  let c;
  const runner = {
    async run({ session }) {
      for (const [id, path, headers] of [[0, 'echo'], [1, 'edge'], [2, 'many'], [3, 'echo', { Authorization: 'Bearer guessed' }], [4, 'locked'], [5, 'bad'], [6, 'short']]) {
        replies.push(await c.code(session, { method: 'http', id, input: { url: `https://keyed.example/api/${path}`, ...(headers && { headers }) } }));
      }
      return { output: 'ran\n', success: true };
    },
  };
  c = client(web, { runner, grantHeaders });
  c.setHttpGrants([{ name: 'keyed', url: 'https://keyed.example/api/', risk: 'read', max_response_bytes: 4096, headers: { Authorization: { app: true } } }]);
  await c.create(dir, 'fake', { runner: 'app' });
  await c.prompt(dir, 'run the script');
  await c.close();
  const [echo, edge, many, own, locked, bad, short] = replies;
  assert.equal(seen[0].authorization, value, 'the request carried the header');
  assert.deepEqual(asked[0], { grant: 'keyed', url: 'https://keyed.example/api/echo', method: 'GET', names: ['Authorization'] });
  assert.equal(echo.ok, true, JSON.stringify(echo));
  const forms = [token, token.slice(8), token.replaceAll('/', '\\/'), pct, b64.slice(4), b64.slice(4).replaceAll('+', '-').replaceAll('/', '_')];
  for (const reply of [echo, edge, many]) {
    const said = JSON.stringify(reply);
    for (const form of forms) assert.ok(!said.includes(form), `${form} in ${said.slice(0, 600)}`);
  }
  assert.equal(echo.data.headers['x-echo'], '[redacted]', JSON.stringify(echo.data.headers));
  assert.equal(echo.data.json.auth, '[redacted]');
  assert.match(echo.data.json.pct, /\[redacted\]/);
  assert.equal(edge.data.truncated, true);
  assert.ok(!edge.data.body.includes(token.slice(0, 6)), `cut through a secret: ${edge.data.body.slice(-40)}`);
  assert.ok(edge.data.body.endsWith('x[redacted]yyyyyyyyyy'), edge.data.body.slice(-40));
  assert.equal(many.data.truncated, true);
  assert.ok(!many.data.body.includes(token.slice(0, 6)), `a part of a secret at a cut: ${many.data.body.slice(-60)}`);
  assert.ok(many.data.body.startsWith('[redacted] [redacted] '), many.data.body.slice(0, 60));
  assert.match(own.error, /supplied by grant keyed/, JSON.stringify(own));
  assert.equal(locked.ok, false);
  assert.equal(locked.may_have_run, false, JSON.stringify(locked));
  assert.match(locked.error, /locked[\s\S]*nothing was sent/, JSON.stringify(locked));
  assert.equal(bad.may_have_run, false, JSON.stringify(bad));
  assert.match(bad.error, /no value it can send for header Authorization/, JSON.stringify(bad));
  assert.equal(short.may_have_run, false, JSON.stringify(short));
  assert.match(short.error, /too short to keep out of what comes back/, 'what cannot be found is not sent');
  assert.deepEqual(seen.map((visit) => visit.path), ['/api/echo', '/api/edge', '/api/many'], 'nothing else was sent');
  // Nothing the session keeps holds the secret, in any form.
  for (const file of filesUnder(dir)) {
    const bytes = fs.readFileSync(file);
    for (const form of forms) assert.ok(!bytes.includes(form), `${form} in ${file}`);
  }
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
  const lock = join(dir, 'store', 'authority.sqlite.lock');
  const left = fs.readFileSync(lock, 'utf8');
  assert.match(left, new RegExp(`^spindle-embed: pid ${child.pid} on `), 'the lock names its owner');
  if (process.platform !== 'linux') fs.unlinkSync(lock);
  // Taken over only where it names a process of this machine that is gone: one whose
  // process runs (here, this test's parent), or of another machine, stays held.
  for (const note of [`pid ${process.ppid} on ${hostname()}`, `pid ${child.pid} on elsewhere.invalid`, 'an app',
    left.replace(/^spindle-embed: /, '').replace(' ns ', ' ns foreign-'),
    left.replace(/^spindle-embed: /, '').replace(' boot ', ' boot foreign-')]) {
    fs.writeFileSync(lock, `spindle-embed: ${note}`);
    await assert.rejects(c.status(dir), (error) => /another spindle process or connection/.test(error.message)
      && error.message.includes('no longer running') && error.message.includes(note), `held: ${note}`);
  }
  if (process.platform === 'linux') {
    // A takeover lock another taker holds (its process runs) keeps it held; one a taker
    // left as its process ended (here, the killed owner's own note) is removed on the
    // same proof, and the takeover goes on.
    const takeover = `${lock}.takeover`;
    const stat = fs.readFileSync(`/proc/${process.ppid}/stat`, 'utf8');
    const started = stat.slice(stat.lastIndexOf(')') + 2).split(/\s+/)[19];
    const running = left.replace(`pid ${child.pid} `, `pid ${process.ppid} `).replace(/ start \S+$/, ` start ${started}`);
    fs.writeFileSync(lock, left);
    fs.writeFileSync(takeover, running);
    await assert.rejects(c.status(dir), (error) => /another spindle process or connection/.test(error.message)
      && error.message.includes('.takeover'), 'a running taker\'s takeover holds');
    assert.equal(fs.readFileSync(takeover, 'utf8'), running);
    fs.writeFileSync(takeover, left);
  } else fs.unlinkSync(lock);
  // The killed owner's lock is taken over (its takeover lock gone again); reopened, the
  // session is this client's.
  await assert.rejects(c.resume(dir), (error) => error.kind === 'failed', 'resume does not send an unknown request again');
  assert.match(fs.readFileSync(lock, 'utf8'), new RegExp(`^spindle-embed: pid ${process.pid} on `));
  assert.ok(!fs.existsSync(`${lock}.takeover`));
  assert.equal(fetch.seen.length, 0);
  const recover = await c.recover(dir);
  assert.equal(recover.wake.work, 'blocked');
  const blocked = await c.status(dir);
  assert.equal(blocked.activity, 'blocked', 'an unknown outcome waits for a person');
  assert.deepEqual(blocked.wake, recover.wake);
  const retried = await c.retry(dir);
  assert.equal(retried.stopped.complete, true);
  assert.equal(fetch.seen.length, 1, 'retry sends it once');
  assert.ok(fs.existsSync(join(dir, 'store.abandoned-1', 'authority.sqlite')), 'the old store is kept aside');
  await c.close();
}

// An owner killed between the user's stop and the stopped command's record (its
// runner never answered): reopened, the call is reconciled as not completed (the
// existing path), never run again, and a message then goes on with the turn.
async function aKilledStopping() {
  const dir = join(root, 'crash-stopping');
  const child = spawn(process.execPath, [fileURLToPath(new URL('node_session_child.mjs', import.meta.url)), glue, dir, 'stop'], { stdio: ['ignore', 'pipe', 'inherit'] });
  await new Promise((resolve, reject) => {
    let out = '';
    child.stdout.on('data', (chunk) => { out += chunk; if (out.includes('stopping')) resolve(); });
    child.on('exit', (code) => reject(new Error(`the owner exited (${code}) before the stop`)));
  });
  const ended = new Promise((resolve) => child.once('exit', resolve));
  child.kill('SIGKILL');
  await ended;
  const runs = [];
  const runner = { async run({ command }) { runs.push(command); return { output: 'ran again', success: true }; } };
  const fetch = scriptedFetch([text('it did not complete'), text('went on')]);
  const c = client(fetch, { runner });
  fs.unlinkSync(join(dir, 'store', 'authority.sqlite.lock')); // the operator's decision
  const before = await c.status(dir);
  assert.equal(before.phase, 'tools_wait');
  assert.equal(before.accepts_message, false, 'an unrecorded call is settled first');
  assert.equal((await c.resume(dir)).stopped.complete, true);
  assert.equal(runs.length, 0, 'the command is never run again');
  assert.match(fetch.seen[0].body, /did not complete/);
  assert.equal((await c.prompt(dir, 'go on without it')).stopped.complete, true);
  assert.equal(runs.length, 0);
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
  const fetch = scriptedFetch([long('first'), long('second'), long('third'), text(`SUMMARY: carry on. ${'The three answers are done; the fourth comes next, in the same style. '.repeat(40)}`), text('done')]);
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

/** A budget, counted from the store's own records as a native host counts them: a
 * request whose worst case would not fit is not sent; raised between steps, the same
 * request goes once; each counted call says what it cost; `usage` totals them. */
async function aBudget() {
  const dir = join(root, 'budget');
  const priced = catalog();
  // Dear enough that one request's worst case ($4.80 here) is over a small budget.
  priced.providers[0].model_specs['fake-model'].pricing = { input_usd_per_mtok: 1000, output_usd_per_mtok: 1000, cache_read_usd_per_mtok: 0, cache_create_usd_per_mtok: 0 };
  const fetch = scriptedFetch([text('within budget')]);
  const c = client(fetch, { catalog: priced });
  const events = [];
  c.on('event', (event) => events.push(event));
  await c.create(dir, 'fake', { max_usd: 0.5 });
  assert.equal((await c.settings(dir)).max_usd, 0.5, 'a budget is kept here');
  await assert.rejects(c.prompt(dir, 'hi'), /spending limit: .* could cost up to \$4\.80, so nothing was sent/);
  assert.equal(fetch.seen.length, 0);
  await c.settings(dir, { max_usd: 100 });
  assert.equal((await c.resume(dir)).stopped.complete, true);
  assert.equal(fetch.seen.length, 1, 'the refused request goes once it fits');
  assert.deepEqual(events.filter((event) => event.type === 'cost'), [{ type: 'cost', billing: 'priced', usd: 0.015, turn: 0.015, total: 0.015 }]);
  const usage = await c.call('usage', { dir });
  assert.equal(usage.calls, 1);
  assert.equal(usage.usd, 0.015);
  assert.equal(usage.models[0].priced, 1);
  await assert.rejects(c.settings(dir, { max_usd: -1 }), /non-negative/);
  await c.close();
}

/** A task in the background: `task_start` with `report: 'queue'` answers at once, the
 * task's own requests go out as work after that answer, and once it ends its answer
 * is read to the session at the next turn. */
async function aBackgroundTask() {
  const dir = join(root, 'background');
  const fetch = scriptedFetch([text('ready'), text('the background answer'), text('noted')]);
  const c = client(fetch);
  await c.create(dir, 'fake', { tasks: true });
  await c.prompt(dir, 'hello');
  const queued = await c.taskStart(dir, 'later', 'look around', { report: 'queue' });
  assert.equal(queued.queued, true);
  let task;
  for (let tries = 0; tries < 200; tries++) {
    task = await c.taskWait(dir, 'later');
    if (task.state === 'finished') break;
    await new Promise((done) => setTimeout(done, 10));
  }
  assert.equal(task.state, 'finished', JSON.stringify(task));
  assert.match(task.answer.content, /^the background answer/);
  assert.equal((await c.tasks(dir)).tasks.length, 1);
  await c.prompt(dir, 'anything new?');
  assert.ok(fetch.seen.slice(2).some((request) => request.body.includes('the background answer')), 'its answer is read to the session');
  assert.equal((await c.taskStop(dir, 'later')).stopping, false);
  await c.close();
}

/** A background task's files come with its report: what it wrote in its own session is
 * in the thread's workspace once the report is delivered (at the next prompt), and the
 * report says how many; a task that discards its files leaves none. */
async function aBackgroundTasksFiles() {
  const dir = join(root, 'background-files');
  // A finished task's report reaches the model at the end of the next turn (after its
  // reply), and the model goes on from it: two replies per turn here.
  const fetch = scriptedFetch([
    text('ready'),
    call('write_file', { path: 'from-task.txt', content: 'made by the task' }), text('the task is done'),
    text('nothing yet'), text('noted'),
    call('write_file', { path: 'scratch.txt', content: 'thrown away' }), text('the scratch task is done'),
    text('nothing more'), text('noted again'),
  ]);
  const c = client(fetch);
  await c.create(dir, 'fake', { tasks: true });
  await c.prompt(dir, 'hello');
  const finished = async (task) => {
    let seen;
    for (let tries = 0; tries < 300; tries++) {
      seen = await c.taskWait(dir, task);
      if (seen.state === 'finished') return seen;
      await new Promise((done) => setTimeout(done, 10));
    }
    throw new Error(`task ${task} did not finish: ${JSON.stringify(seen)}`);
  };
  await c.taskStart(dir, 'maker', 'make a file', { report: 'queue' });
  const made = await finished('maker');
  assert.match(made.answer.content, /\[task \w+: 1 file\(s\) written, 0 removed\]$/, 'its answer counts its files');
  const paths = async () => (await c.files(dir)).files.map((file) => file.path).sort();
  assert.deepEqual(await paths(), [], 'until its report is delivered, the file is the task\'s own');
  await c.prompt(dir, 'anything new?');
  assert.deepEqual(await paths(), ['from-task.txt'], 'the report brings the file');
  assert.equal(new TextDecoder().decode(await c.read(dir, 'from-task.txt')), 'made by the task');
  const report = fetch.seen.at(-1).body;
  assert.ok(report.includes('1 file(s) written') && report.includes('the task is done'), 'the model reads the report with the count');
  await c.taskStart(dir, 'scratch', 'make a scratch file', { report: 'queue', files: 'discard' });
  const scratch = await finished('scratch');
  assert.doesNotMatch(scratch.answer.content, /file\(s\) written/, 'a task that discards its files counts none');
  await c.prompt(dir, 'and now?');
  assert.deepEqual(await paths(), ['from-task.txt'], 'nothing of a discarding task reaches the workspace');
  await c.close();
}

/** A background task the model starts (the task tool with `background`) that finishes
 * while the turn is still under way: its report is read at that turn's end, and brings
 * its files (offered as the task ends, not only as a prompt begins). The task's requests
 * are told from the turn's by their first message. */
async function aBackgroundTaskFinishingMidTurn() {
  const dir = join(root, 'mid-turn');
  const parent = scriptedFetch([
    text('ready'),
    call('task', { prompt: 'make mid.txt', background: true }),
    call('hold', { who: 'turn' }),
    text('held no more'), text('seen the report'),
  ]);
  const child = scriptedFetch([call('write_file', { path: 'mid.txt', content: 'made mid-turn' }), text('mid done')]);
  const fetch = (url, init) => {
    const first = JSON.parse(new TextDecoder().decode(init.body)).messages.find((message) => message.role === 'user');
    return (typeof first?.content === 'string' && first.content === 'make mid.txt' ? child : parent)(url, init);
  };
  const c = client(fetch);
  const held = new Map();
  c.tool({
    name: 'hold', description: 'Waits until it is let go', rerun: 'safe',
    inputSchema: { type: 'object', properties: { who: { type: 'string' } }, required: ['who'], additionalProperties: false },
    run: ({ who }, asked) => new Promise((resolve) => held.set(who, { asked, release: resolve })),
  });
  const until = async (what, why) => {
    for (let tries = 0; tries < 500 && !what(); tries++) await new Promise((done) => setTimeout(done, 10));
    assert.ok(what(), why);
  };
  await c.create(dir, 'fake', { tasks: true });
  await c.prompt(dir, 'hello');
  const turn = c.prompt(dir, 'go');
  await until(() => held.has('turn'), 'the turn waits on its app call, its task started');
  await until(() => child.replies.length === 0, 'the task ran meanwhile');
  let listed;
  for (let tries = 0; tries < 300; tries++) {
    listed = (await c.tasks(dir)).tasks;
    if (listed.some((task) => task.state === 'finished')) break;
    await new Promise((done) => setTimeout(done, 10));
  }
  assert.ok(listed.some((task) => task.state === 'finished'), `the task finished mid-turn: ${JSON.stringify(listed)}`);
  assert.deepEqual((await c.files(dir)).files, [], 'nothing of it in the workspace until its report is read');
  held.get('turn').release('let go');
  await turn;
  const paths = (await c.files(dir)).files.map((file) => file.path);
  assert.deepEqual(paths, ['mid.txt'], 'the file came with the report read at the turn\'s end');
  assert.equal(new TextDecoder().decode(await c.read(dir, 'mid.txt')), 'made mid-turn');
  assert.ok(parent.seen.at(-1).body.includes('mid done') && parent.seen.at(-1).body.includes('1 file(s) written'), 'the model read the report with the count');
  await c.close();
}

/** Stopping a turn ends its own app call (its handler's `call.signal`, and no answer is
 * sent for it) but not a background task's: the task's call goes on, is answered, and
 * the task finishes. */
async function aStopLeavesBackgroundTasksCalls() {
  const dir = join(root, 'stopped-parent');
  const fetch = scriptedFetch([text('ready'), call('hold', { who: 'task' }), call('hold', { who: 'turn' }), text('task done')]);
  const c = client(fetch);
  const held = new Map();
  // An answer the package no longer waits for is not sent (it would be refused).
  const refused = [];
  c.on('app_tool_error', (error) => refused.push(error));
  c.tool({
    name: 'hold', description: 'Waits until it is let go', rerun: 'safe',
    inputSchema: { type: 'object', properties: { who: { type: 'string' } }, required: ['who'], additionalProperties: false },
    run: ({ who }, asked) => new Promise((resolve) => held.set(who, { asked, release: resolve })),
  });
  const until = async (what, why) => {
    for (let tries = 0; tries < 500 && !what(); tries++) await new Promise((done) => setTimeout(done, 10));
    assert.ok(what(), why);
  };
  await c.create(dir, 'fake', { tasks: true });
  await c.prompt(dir, 'hello');
  await c.taskStart(dir, 'later', 'hold on', { report: 'queue' });
  await until(() => held.has('task'), 'the background task asks for its app call');
  assert.match(held.get('task').asked.call, /^tasks\//, 'a task\'s call is named under its task');
  const turn = c.prompt(dir, 'go').then((value) => ({ value }), (error) => ({ error }));
  await until(() => held.has('turn'), 'the turn asks for its own');
  assert.doesNotMatch(held.get('turn').asked.call, /^tasks\//);
  await c.stop(dir);
  await until(() => held.get('turn').asked.signal?.aborted, 'the stopped turn\'s call is told it ended');
  assert.equal(held.get('turn').asked.signal.reason.name, 'AbortError');
  assert.equal(held.get('turn').asked.signal.reason.outcome, 'stopped');
  assert.equal(held.get('task').asked.signal.aborted, false, 'the background task\'s call goes on');
  await turn;
  held.get('turn').release('too late');
  held.get('task').release('let go');
  let task;
  for (let tries = 0; tries < 300; tries++) {
    task = await c.taskWait(dir, 'later');
    if (task.state === 'finished') break;
    await new Promise((done) => setTimeout(done, 10));
  }
  assert.equal(task.state, 'finished', JSON.stringify(task));
  assert.match(task.answer.content, /^task done/);
  assert.ok(fetch.seen.at(-1).body.includes('let go'), 'the task\'s call was answered');
  assert.deepEqual(refused, [], 'no answer was sent for the stopped call');
  await c.close();
}

async function aBusyRemoteRunner() {
  const inputs = { command: 'echo once', timeoutS: 30, files: [], links: [], directories: [], read: () => new Uint8Array() };
  const sent = [];
  const runner = remoteRunner({ url: 'http://runner', token: 't', fetch: async (_url, init) => {
    sent.push(await init.body.text());
    return sent.length === 1 ? Response.json({ code: 'busy', error: 'full' }, { status: 503 }) : Response.json({ success: true, output: 'once' });
  } });
  assert.equal((await runner.run(inputs)).success, true);
  assert.equal(sent.length, 2);
  assert.equal(sent[0], sent[1], 'only a pre-admission busy refusal retries, using the same run id');
  let uncertain = 0;
  const lost = remoteRunner({ url: 'http://runner', token: 't', fetch: async () => { uncertain += 1; throw new Error('lost answer'); } });
  await assert.rejects(lost.run(inputs), /lost answer/);
  assert.equal(uncertain, 1, 'an uncertain run never retries');
  const signal = new AbortController();
  let attempts = 0;
  const busy = remoteRunner({ url: 'http://runner', token: 't', fetch: async () => {
    attempts += 1;
    setTimeout(() => signal.abort(), 10);
    return Response.json({ code: 'busy', error: 'full' }, { status: 503 });
  } });
  assert.equal((await busy.run({ ...inputs, signal: signal.signal })).success, false);
  assert.equal(attempts, 1, 'Stop during backoff never starts a run or sends a Stop tombstone');
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
  // A runner token: it runs commands and reaches nothing else of the host.
  const token = execFileSync(binary, ['--dir', host, 'token', 'new', 'runner', '--runner'], { env, encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim();
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
    const sessions = await fetch(`http://127.0.0.1:${port}/v1/sessions`, { headers: { authorization: `Bearer ${token}` } });
    assert.equal(sessions.status, 403, 'a runner token reads no sessions');
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

    // Stop ends the host's command, and the wait with its answer: what it printed.
    const named = [];
    const watched = remoteRunner({
      url: `http://127.0.0.1:${port}`, token,
      // The request's body is a Blob, built a file at a time.
      fetch: async (url, init) => {
        if (String(url).endsWith('/v1/run')) named.push(JSON.parse(await init.body.text()).run);
        return fetch(url, init);
      },
    });
    const stop = new AbortController();
    const started = Date.now();
    const waiting = watched.run({ command: 'echo started-remote; sleep 30', timeoutS: 60, network: false, files: [], links: [], directories: [], read: () => new Uint8Array(), signal: stop.signal });
    const state = async () => (await (await fetch(`http://127.0.0.1:${port}/v1/run/${named[0]}`, { headers: { authorization: `Bearer ${token}` } })).json()).state;
    while (!named.length || (await state()) !== 'running') {
      assert.ok(Date.now() - started < 20_000, 'the run started');
      await new Promise((done) => setTimeout(done, 50));
    }
    stop.abort();
    const answer = await waiting;
    assert.equal(answer.success, false);
    assert.match(String(answer.failure), /stopped/, JSON.stringify(answer));
    assert.match(answer.output, /started-remote/, 'what it printed before the stop comes back');
    let ended;
    while ((ended = await state()) !== 'stopped' && Date.now() - started < 20_000) await new Promise((done) => setTimeout(done, 50));
    assert.equal(ended, 'stopped', 'the host stopped the command');
    assert.ok(Date.now() - started < 20_000, 'stopped, not run out');
  } finally {
    serve.kill();
  }
}

async function responsesReasoningSnapshots() {
  const profiles = catalog();
  profiles.providers[0].request_contract = 'openai-responses';
  profiles.providers[0].model_specs['fake-model'].context_window = 10_000_000;
  const reasoning = (id, content) => ({ type: 'reasoning', id, encrypted_content: content, summary: [] });
  const first = reasoning('rs_first', 'y'.repeat(3_000_000));
  const second = reasoning('rs_second', 'z'.repeat(1_000_000));
  const terminal = (output) => ({ type: 'response.completed', response: { status: 'completed', output, usage: { input_tokens: 17, output_tokens: 9 } } });
  const answer = (content) => [{ type: 'response.output_text.delta', delta: content }, terminal([])];
  const seen = [];
  const replies = [
    [{ type: 'response.output_item.done', item: reasoning('rs_old', 'old') }, ...answer('old answer')],
    [
      { type: 'response.output_item.done', item: reasoning('rs_first', 'x'.repeat(1_000_000)) },
      { type: 'response.output_item.done', item: reasoning('rs_second', 'x'.repeat(3_000_000)) },
      { type: 'response.output_item.added', item: { type: 'function_call', id: 'fc_write', call_id: 'call_write', name: 'write_file', arguments: '' } },
      terminal([first, second, { type: 'function_call', id: 'fc_write', call_id: 'call_write', name: 'write_file', arguments: JSON.stringify({ path: 'reasoning.txt', content: 'kept' }) }]),
    ],
    answer('current answer'),
    answer('new answer'),
  ];
  const fetch = async (_url, init) => {
    seen.push(JSON.parse(new TextDecoder().decode(init.body)));
    const records = replies.shift();
    assert.ok(records, 'no unexpected automatic provider request');
    const bytes = new TextEncoder().encode(records.map((record) => `data: ${JSON.stringify(record)}\n\n`).join(''));
    let at = 0;
    return new Response(new ReadableStream({ pull(controller) {
      if (at === bytes.length) return controller.close();
      controller.enqueue(bytes.subarray(at, at += Math.min(4093, bytes.length - at)));
    } }), { headers: { 'content-type': 'text/event-stream' } });
  };
  const c = nodeClient({ glue, catalog: profiles, credentials, fetch });
  const dir = join(root, 'responses-reasoning');
  try {
    // Its megabytes of reasoning stay in the history: compacted by the window alone.
    await c.create(dir, 'fake', { compact_tokens: null });
    await c.prompt(dir, 'old task');
    const result = await c.prompt(dir, 'current task');
    assert.equal(result.stopped.complete, true);
    assert.equal(new TextDecoder().decode(await c.read(dir, 'reasoning.txt')), 'kept');
    assert.deepEqual(seen[2].input.filter((item) => item.type === 'reasoning'), [first, second], 'terminal copies, not streamed or previous-turn copies, reach the next request');
    const history = await c.history(dir);
    const kept = history.messages.flatMap((message) => message.content).filter((block) => block.type === 'reasoning');
    assert.equal(kept.length, 3, 'history includes the old and current reasoning placeholders');
    assert.ok(kept.every((block) => !('item' in block) && !('encrypted_content' in block)), 'the public history projection does not expose opaque reasoning');
    await c.prompt(dir, 'next task');
    assert.equal(seen[3].input.some((item) => item.type === 'reasoning'), false, 'a new turn does not replay old encrypted reasoning');
  } finally {
    await c.close();
  }
  for (const placeholderFirst of [true, false]) {
    let calls = 0;
    const item = reasoning('rs_duplicate', 'valid');
    const placeholder = { type: 'reasoning', id: 'rs_duplicate', summary: [] };
    const d = nodeClient({ glue, catalog: profiles, credentials, fetch: async () => {
      calls += 1;
      return new Response(`data: ${JSON.stringify(terminal(placeholderFirst ? [placeholder, item] : [item, placeholder]))}\n\n`, { headers: { 'content-type': 'text/event-stream' } });
    } });
    const dir = join(root, `responses-duplicate-${placeholderFirst}`);
    try {
      await d.create(dir, 'fake');
      await assert.rejects(d.prompt(dir, 'reject a duplicate final reasoning id'));
      assert.equal(calls, 1, 'a duplicate snapshot is a protocol failure, never implicitly resent');
    } finally {
      await d.close();
    }
  }
}

try {
  await responsesReasoningSnapshots();
  const dir = await aTurnWithFileTools();
  await reopenAndAppTool(dir);
  await anAppRunner();
  await aStoppedCommand();
  await twoRunners();
  await aScriptsCalls();
  await aGrantsSecretHeader();
  await aKilledOwner();
  await aKilledStopping();
  await readItNowAndStop();
  await aModelWrittenSummary();
  await aBudget();
  await aBackgroundTask();
  await aBackgroundTasksFiles();
  await aBackgroundTaskFinishingMidTurn();
  await aStopLeavesBackgroundTasksCalls();
  await aBusyRemoteRunner();
  await aRemoteRunner();
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
console.log(
  'node-session passed: wasm package via EmbeddedClient against a scripted model (file tools, ' +
    'Responses reasoning snapshots budgeted as a batch, final copies replayed only within their turn, duplicate placeholders rejected without resend, ' +
    'credentials kept out of the store, app tool with files/state, app runner, a command the user stops (recorded with its output, a reply going on after a reopen, never run again), app and remote runners at once (a declared sandbox isolates), Spindle calls from a script journaled and replayed (http() only where a grant allows; a secret header a grant adds, never kept or shown), reopen, second ' +
    'owner refused, killed owner: its lock taken over (only for a process of this machine that is gone), unknown kept and sent once by retry, killed between a stop and its record: not completed, never run again, read-it-now cut, stop, ' +
    'compaction by a model-written summary, a budget (refused unsent, raised, sent once, costed, usage), a background task (queued, driven after its answer, reported next turn with the files it made, none when it discards them; its app call going on when its parent\'s turn is stopped, while the turn\'s own call is told it ended), ' +
    'remote runner on spindle serve with a runner token, and Stop stopping its command, when SPINDLE_BIN is set)',
);
