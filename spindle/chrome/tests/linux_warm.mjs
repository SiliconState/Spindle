// Which Linux starts ahead of a command (a Node test of linux-runner.mjs with stand-in
// workers): `warm()` starts both, `warm({ v86: false })` the wasm Linux alone, and
// without a wasm Linux in the build it starts nothing; v86 then starts with the first
// command that goes to it, and `onRan` says which Linux ran each command. `prepare()`
// (after an install or update) has v86 make its saved state and then lets the machine
// go, unless a command came for it meanwhile.
import assert from 'node:assert/strict';

const started = [];
class StandIn {
  constructor(url) {
    this.kind = String(url).includes('wasm-linux') ? 'wasm' : 'v86';
    this.got = [];
    this.terminated = false;
    started.push(this);
  }
  postMessage(message) {
    this.got.push(message.type);
    if (message.type !== 'run') return;
    // Answer the command as the Linux would (the wasm Linux's Python loads no compiled
    // module).
    const result = this.kind === 'wasm' && message.command.includes('import fast')
      ? { output: "ImportError: cannot import name '_speed' from partially initialized module 'fast'\n", success: false, code: 1 }
      : { output: 'ok', success: true };
    queueMicrotask(() => this.onmessage({ data: { type: 'ran', id: message.id, result } }));
  }
  terminate() {
    this.terminated = true;
  }
}
globalThis.Worker = StandIn;
let carriesWasm = true;
globalThis.fetch = async (url) => {
  const path = String(url);
  if (path.endsWith('wasm-linux/identity')) return new Response(carriesWasm ? 'wasm-digest' : null, { status: carriesWasm ? 200 : 404 });
  if (path.endsWith('wasm-linux/fs.json')) {
    const index = { fsroot: [['bin', 0, 0, 0o040755, 0, 0, [['sh', 0, 0, 0o100755, 0, 0, null], ['echo', 0, 0, 0o100755, 0, 0, null], ['python3', 0, 0, 0o100755, 0, 0, null]]]] };
    return new Response(carriesWasm ? JSON.stringify(index) : null, { status: carriesWasm ? 200 : 404 });
  }
  if (path.endsWith('linux/identity')) return new Response('v86-digest');
  throw new Error(`unexpected fetch ${path}`);
};

const { linuxRunner } = await import('../linux-runner.mjs');
const settle = () => new Promise((done) => setTimeout(done, 10));
const kinds = () => started.map((worker) => worker.kind).sort();

// A thread that only talks: the wasm Linux only.
let ran = [];
let runner = linuxRunner({ onRan: (session, kind) => ran.push([session, kind]) });
runner.warm({ v86: false });
await settle();
assert.deepEqual(kinds(), ['wasm'], 'v86 stays unstarted');
// Commands the wasm Linux runs (in a thread with network on too, using none of it), then
// one only v86 can (a cache v86 keeps for a thread with network): v86 starts then.
const job = { session: '/threads/0123456789abcdef', timeoutS: 5, files: [], links: [], directories: [], read: () => new Uint8Array() };
assert.equal((await runner.run({ ...job, command: 'echo hi', network: false })).success, true);
assert.equal((await runner.run({ ...job, command: 'echo hi', network: true })).success, true);
assert.deepEqual(kinds(), ['wasm']);
assert.equal((await runner.run({ ...job, command: 'echo hi > .cache/page.html', network: true })).success, true);
assert.deepEqual(kinds(), ['v86', 'wasm'], 'v86 starts with the command that needs it');
assert.deepEqual(ran, [[job.session, 'wasm'], [job.session, 'wasm'], [job.session, 'v86']]);
// A compiled module the wasm Linux cannot load: v86 runs the command again, but only in a
// workspace that holds compiled code (otherwise the import error is the script's).
ran = [];
const fast = { path: 'pylib/fast/__init__.py', size: 1, executable: false, object: 'o1' };
const speed = { path: 'pylib/fast/_speed.cpython-314-i386-linux-musl.so', size: 1, executable: false, object: 'o2' };
const compiledRun = await runner.run({ ...job, command: 'PYTHONPATH=pylib python3 -c "import fast"', files: [fast, speed], read: () => new Uint8Array(1) });
assert.equal(compiledRun.success, true);
assert.deepEqual(ran.map(([, kind]) => kind), ['wasm', 'v86']);
ran = [];
assert.equal((await runner.run({ ...job, command: 'PYTHONPATH=pylib python3 -c "import fast"', files: [fast], read: () => new Uint8Array(1) })).success, false);
assert.deepEqual(ran.map(([, kind]) => kind), ['wasm']);

// A thread that used v86 lately, or the user asking: both.
started.length = 0;
runner = linuxRunner();
runner.warm();
await settle();
assert.deepEqual(kinds(), ['v86', 'wasm']);

// A build without the wasm Linux: nothing starts until a command.
started.length = 0;
carriesWasm = false;
runner = linuxRunner();
runner.warm({ v86: false });
await settle();
assert.deepEqual(kinds(), []);
runner.warm();
await settle();
assert.deepEqual(kinds(), ['v86']);

// prepare(): v86 alone makes its state, then goes (its memory with it), said stopped.
started.length = 0;
carriesWasm = true;
const states = [];
runner = linuxRunner({ onStatus: (state, detail, kind) => states.push(`${kind}:${state}`) });
runner.prepare();
await settle();
assert.deepEqual(kinds(), ['v86']);
let [v86] = started;
assert.ok(v86.got.includes('prepare'));
v86.onmessage({ data: { type: 'status', state: 'booting' } });
v86.onmessage({ data: { type: 'status', state: 'ready' } });
v86.onmessage({ data: { type: 'prepared', did: true } });
assert.equal(v86.terminated, true, 'the machine goes once its state is made');
assert.deepEqual(states.filter((state) => state.startsWith('v86')), ['v86:booting', 'v86:ready', 'v86:stopped']);
// A state already there: nothing booted, the worker goes, no status.
started.length = 0;
states.length = 0;
runner.prepare();
await settle();
[v86] = started;
v86.onmessage({ data: { type: 'prepared', did: false } });
assert.equal(v86.terminated, true);
assert.deepEqual(states.filter((state) => state.startsWith('v86')), []);
// A command for v86 while it prepares: the machine stays.
started.length = 0;
runner.prepare();
await settle();
[v86] = started;
assert.equal((await runner.run({ ...job, command: 'curl example.com', network: true })).success, true);
v86.onmessage({ data: { type: 'prepared', did: true } });
assert.equal(v86.terminated, false, 'a command came: the machine stays');
// And while v86 runs already, prepare() asks nothing of it.
const asked = v86.got.length;
runner.prepare();
await settle();
assert.equal(v86.got.length, asked);
assert.equal(started.length, 1);

console.log('linux-warm passed: v86 starts only when asked or with a command that needs it; the wasm Linux on open; which ran each command told; a compiled module reruns on v86; prepare() makes v86\'s state and lets it go');
process.exit(0);
