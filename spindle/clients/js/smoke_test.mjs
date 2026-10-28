// Smoke test of spindle.mjs against this build, with a scripted loopback provider (no
// paid calls). Run: SPINDLE_BIN=target/debug/spindle node clients/js/smoke_test.mjs
//
// It drives `spindle api` (stdio) through a turn with a bash call (and a listener that
// throws), interrupts a second one, checks a client whose process is gone or never
// started, then `spindle serve`'s HTTP /v1 API with a named token.
import assert from 'node:assert/strict';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { HttpClient, SpindleError, StdioClient } from './spindle.mjs';

// OpenAI chat-completions SSE: a bash call (sleep for a SLEEP prompt), then text.
let unknownSends = 0;
const provider = createServer((request, response) => {
  let body = '';
  request.on('data', (chunk) => { body += chunk; });
  request.on('end', () => {
    const data = JSON.parse(body);
    const last = data.messages.at(-1);
    if (body.includes('SUPERVISE_UNKNOWN')) {
      unknownSends += 1;
      response.destroy();
      return;
    }
    const prompt = data.messages.filter((m) => m.role === 'user').map((m) => JSON.stringify(m.content)).join(' ');
    const chunk = (delta, finish = null) => ({ id: 'x', object: 'chat.completion.chunk', model: data.model, choices: [{ index: 0, delta, finish_reason: finish }] });
    const call = (command) => ({ tool_calls: [{ index: 0, id: `call_${data.messages.length}`, type: 'function', function: { name: 'bash', arguments: JSON.stringify({ command }) } }] });
    const lookup = data.tools?.some((tool) => tool.function.name === 'lookup');
    const asks = { tool_calls: [{ index: 0, id: `call_${data.messages.length}`, type: 'function', function: { name: 'lookup', arguments: JSON.stringify({ q: 'notes' }) } }] };
    const chunks = lookup && last.role !== 'tool'
      ? [chunk({ role: 'assistant', content: 'Asking the app.' }), chunk(asks), chunk({}, 'tool_calls')]
      : last.role === 'tool' || !data.tools
      ? [chunk({ role: 'assistant', content: 'All done.' }), chunk({}, 'stop')]
      : [chunk({ role: 'assistant', content: 'Working.' }), chunk(call(prompt.includes('SLEEP') ? 'sleep 30' : 'echo hello > hello.txt && cat hello.txt')), chunk({}, 'tool_calls')];
    let out = chunks.map((c) => `data: ${JSON.stringify(c)}\n\n`).join('');
    out += 'data: {"id":"x","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}\n\n';
    out += 'data: [DONE]\n\n';
    response.writeHead(200, { 'Content-Type': 'text/event-stream', 'Content-Length': Buffer.byteLength(out) });
    response.end(out);
  });
});
await new Promise((resolve) => provider.listen(0, '127.0.0.1', resolve));

const root = mkdtempSync(join(tmpdir(), 'spindle-js-smoke-'));
const env = { ...process.env, DEXT_BASE_URL: `http://127.0.0.1:${provider.address().port}` };
// A throwaway Dext home (built-in catalog, no logins) unless one is given.
if (!env.DEXT_HOME) {
  env.HOME = join(root, 'home');
  env.DEXT_HOME = join(env.HOME, '.dext');
}
mkdirSync(env.DEXT_HOME, { recursive: true });
const binary = env.SPINDLE_BIN || 'spindle';

async function stdio() {
  const work = env.SPINDLE_SMOKE_DIR || join(root, 'work');
  const api = new StdioClient({ binary, env });
  const seen = [];
  api.on('event', (event) => seen.push(event.type));
  // A listener that throws is reported as uncaught, and replies keep coming.
  const thrown = [];
  const uncaught = (error) => thrown.push(error);
  process.on('uncaughtException', uncaught);
  api.once('event', () => { throw new Error('listener bug'); });
  const info = await api.info();
  assert.equal(info.api_version, 1);
  assert.equal((await api.create(work, 'local', { isolation: 'off' })).phase, 'empty');
  const done = await api.prompt(work, 'write hello.txt');
  assert.equal(done.stopped.complete, true);
  process.off('uncaughtException', uncaught);
  assert.deepEqual(thrown.map((error) => error.message), ['listener bug']);
  for (const kind of ['prompt', 'tool_call', 'output', 'tool_result', 'stopped']) assert.ok(seen.includes(kind), `${kind} in ${seen}`);
  assert.equal((await api.read(work, 'hello.txt')).toString(), 'hello\n');
  // Stop a running command: status answers meanwhile, the turn ends as stopped.
  const sleeping = api.prompt(work, 'SLEEP please', { expectedHead: done.head });
  await new Promise((resolve) => api.on('event', (event) => event.type === 'tool_call' && resolve()));
  assert.equal((await api.status(work)).busy, true);
  await assert.rejects(api.prompt(work, 'again'), (error) => error instanceof SpindleError && error.kind === 'busy');
  await api.stop(work);
  await assert.rejects(sleeping, (error) => error.kind === 'stopped');
  const resumed = await api.resume(work);
  assert.equal(resumed.stopped.complete, true);
  await assert.rejects(api.retry(work, { expectedHead: resumed.head + 1 }), (error) => error.kind === 'head_moved');
  await assert.rejects(api.call('nope'), (error) => error.kind === 'method_not_found');
  await api.closeSession(work);
  assert.equal(await api.close(), 0);
  // Calls after the process ended, or to one that never started, reject at once.
  await assert.rejects(api.info(), (error) => error.kind === 'closed');
  const missing = new StdioClient({ binary: join(root, 'no-such-spindle'), env });
  await assert.rejects(missing.info(), (error) => error.kind === 'closed');
  await assert.rejects(missing.info(), (error) => error.kind === 'closed');
  await missing.close();
  console.log('stdio ok:', seen.length, 'events');
}

// An app tool answered by the client itself: its text reaches the model and the file
// it writes lands in the workspace with that step.
async function appTools() {
  const work = join(root, 'app');
  const api = new StdioClient({ binary, env });
  api.tool({
    name: 'lookup',
    description: 'Looks notes up.',
    inputSchema: { type: 'object', properties: { q: { type: 'string' } } },
    run: (input) => ({ content: `Found ${input.q}.`, files: { write: { 'notes.md': '# Notes\n' } }, state: { looked: input.q } }),
  });
  await api.create(work, 'local', { builtin_tools: 'none' });
  assert.equal((await api.settings(work)).tools[0].name, 'lookup');
  await api.prompt(work, 'find my notes', { state: { topic: 'notes' } });
  // The prompt's state, then the tool answer's, each with its own step.
  assert.deepEqual((await api.state(work)).state, { topic: 'notes', looked: 'notes' });
  const { revision } = await api.setState(work, { topic: null, count: 1 });
  assert.deepEqual((await api.state(work, revision)).state, { looked: 'notes', count: 1 });
  assert.equal((await api.read(work, 'notes.md')).toString(), '# Notes\n');
  assert.ok(JSON.stringify(await api.history(work)).includes('Found notes.'));
  // A task the app starts is a session of its own, read by its directory; its tool
  // calls still reach this client.
  const taskCalls = [];
  const onTaskCall = (event) => { if (event.type === 'app_tool') taskCalls.push(event.call); };
  api.on('event', onTaskCall);
  const task = await api.taskStart(work, 'look-again', 'find my notes again');
  api.off('event', onTaskCall);
  assert.equal(task.failed, false);
  // A task's calls are named apart from the session's own (same effect ids otherwise).
  assert.ok(taskCalls.length > 0 && taskCalls.every((call) => call.startsWith('tasks/')), String(taskCalls));
  assert.equal((await api.tasks(work)).tasks.length, 1);
  assert.ok(JSON.stringify(await api.history(task.task)).includes('Found notes.'));
  await api.close();
  console.log('app tools ok');
}

// A supervised client outlives its process: killed while the app decides, it starts
// spindle api again, reads the session's recovery and continues it; the app is asked
// the same call again and the turn ends.
async function supervised() {
  const work = join(root, 'supervised');
  const api = new StdioClient({ binary, env, supervise: true });
  let asked = 0;
  let killed = false;
  api.tool({
    name: 'lookup',
    description: 'Looks notes up.',
    run: async () => {
      asked += 1;
      if (!killed) {
        killed = true;
        api.process.kill('SIGKILL');
        return new Promise(() => {});
      }
      return 'Found after a restart.';
    },
    again: () => { asked += 1; return 'Found after a restart.'; },
  });
  await api.create(work, 'local');
  const recovered = new Promise((resolve) => api.on('recovered', (dir, report) => resolve(report)));
  const ended = new Promise((resolve) => api.on('event', (event) => { if (event.type === 'stopped') resolve(event); }));
  await assert.rejects(api.prompt(work, 'find my notes'), (error) => error.kind === 'closed');
  assert.equal((await recovered).wake.work, 'ready');
  assert.equal((await ended).complete, true);
  assert.equal(asked, 2);
  assert.ok(JSON.stringify(await api.history(work)).includes('Found after a restart.'));
  await api.close();
  console.log('supervised ok');
}

async function supervisedUnknownAndLoopGuard() {
  const work = join(root, 'supervised-unknown');
  const api = new StdioClient({ binary, env, supervise: true });
  try {
    await api.create(work, 'local', { builtin_tools: 'none', resend_unknown: true });
    await api.prompt(work, 'seed known history');
    await assert.rejects(api.prompt(work, 'SUPERVISE_UNKNOWN'), (error) => error.message.includes('unknown outcome'));
    const before = unknownSends;
    const recovered = new Promise((resolve) => api.once('recovered', (_dir, report) => resolve(report)));
    api.process.kill('SIGKILL');
    assert.equal((await recovered).wake.work, 'ready');
    await new Promise((resolve) => setTimeout(resolve, 1500));
    assert.equal(unknownSends, before, 'known-only supervision never resends unknown work, even with consent');
  } finally { await api.close(); }
  const guarded = new StdioClient({ binary, env, supervise: true });
  try {
    await guarded.info();
    guarded.ends = Array(5).fill(Date.now());
    const ended = new Promise((resolve) => guarded.once('supervise_ended', resolve));
    const child = guarded.process;
    child.kill('SIGKILL');
    await ended;
    assert.equal(guarded.process, child, 'loop guard did not start another process');
  } finally { await guarded.close(); }
  console.log('supervised unknown and loop guard ok');
}

async function overHttp() {
  const host = join(root, 'host');
  const token = execFileSync(binary, ['--dir', host, 'token', 'new', 'smoke'], { env, encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim();
  const serve = spawn(binary, ['--dir', host, 'serve', '--port', '0'], { env, stdio: ['ignore', 'pipe', 'ignore'] });
  try {
    const port = await new Promise((resolve) => serve.stdout.once('data', (text) => resolve(/127\.0\.0\.1:(\d+)/.exec(String(text))[1])));
    const client = new HttpClient(`http://127.0.0.1:${port}`, token);
    assert.equal((await client.info()).api_version, 1);
    const receipt = await client.command('new', { provider: 'local', text: 'write hello.txt' });
    const again = await client.command('new', { provider: 'local', text: 'write hello.txt' }, receipt.request_id);
    assert.equal(again.session, receipt.session);
    const finished = async (requestId) => {
      for (let tries = 0; tries < 600; tries++) {
        const { state } = await client.receipt(requestId);
        if (state !== 'running') return state;
        await new Promise((resolve) => setTimeout(resolve, 200));
      }
      throw new Error(`${requestId} still running`);
    };
    assert.equal(await finished(receipt.request_id), 'completed');
    const { messages } = await client.messages(receipt.session);
    assert.ok(messages.some((m) => m.content.some((block) => block.type === 'tool_use')));
    await assert.rejects(client.events('nope').next(), (error) => error.kind === 'not_found' && error.status === 404);
    // Subscribe first (the snapshot names the head), then act, then read to the end.
    const kinds = [];
    const stream = client.events(receipt.session);
    const { value: first } = await stream.next();
    assert.equal(first.kind, 'snapshot');
    const upload = await client.upload(receipt.session, Buffer.from('spaced\n'));
    const say = await client.command('say', { session: receipt.session, text: 'once more', expected_head: first.data.head, attachments: [{ path: 'a b+c.txt', digest: upload.digest }] });
    for await (const { kind, data } of stream) {
      if (kind === 'event') kinds.push(data.type);
      if (data.type === 'command_done') break;
    }
    assert.ok(kinds.includes('tool_call') && kinds.at(-1) === 'command_done', String(kinds));
    assert.equal(await finished(say.request_id), 'completed');
    assert.equal((await client.read(receipt.session, 'a b+c.txt')).toString(), 'spaced\n');
    await assert.rejects(client.command('continue', { session: receipt.session, expected_head: 1e6 }), (error) => error.kind === 'head_moved' && error.status === 409);
    // An app tool over HTTP: the client answers from the event stream.
    client.tool({ name: 'lookup', description: 'Looks notes up.', run: (input) => `Found ${input.q} over HTTP.` });
    const asked = await client.command('new', { provider: 'local', text: 'find my notes', tools: client.tools.definitions() });
    const stop = new AbortController();
    const answering = client.answerAppTools(asked.session, { signal: stop.signal });
    assert.equal(await finished(asked.request_id), 'completed');
    stop.abort();
    await answering;
    const answered = await client.messages(asked.session);
    assert.ok(JSON.stringify(answered).includes('Found notes over HTTP.'));
    // App state over HTTP: a `state` command between turns, read back by revision.
    const head = (await client.session(asked.session)).head;
    const changed = await client.command('state', { session: asked.session, patch: { stage: 'drafted' }, expected_head: head });
    assert.equal(await finished(changed.request_id), 'completed');
    assert.deepEqual((await client.state(asked.session)).state, { stage: 'drafted' });
    await assert.rejects(client.queueState(asked.session, { stage: 'x' }), (error) => error.kind === 'not_running');
    assert.deepEqual((await client.tasks(asked.session)).tasks, []);
    console.log('http ok:', kinds.length, 'events');
  } finally {
    serve.kill();
  }
}

try {
  await stdio();
  await appTools();
  await supervised();
  await supervisedUnknownAndLoopGuard();
  await overHttp();
  console.log('ok');
} finally {
  provider.close();
  // Sandbox scratch can leave directories without owner permissions.
  spawnSync('chmod', ['-R', 'u+rwx', root]);
  rmSync(root, { recursive: true, force: true });
}
