// Which Linux starts ahead of a command (a Node test of linux-runner.mjs with stand-in
// workers): `warm()` starts both, `warm({ v86: false })` the wasm Linux alone, and
// without a wasm Linux in the build it starts nothing; v86 then starts with the first
// command that goes to it, and `onRan` says which Linux ran each command.
import assert from 'node:assert/strict';

const started = [];
class StandIn {
  constructor(url) {
    this.kind = String(url).includes('wasm-linux') ? 'wasm' : 'v86';
    started.push(this);
  }
  postMessage(message) {
    if (message.type !== 'run') return;
    // Answer the command as the Linux would.
    queueMicrotask(() => this.onmessage({ data: { type: 'ran', id: message.id, result: { output: 'ok', success: true } } }));
  }
  terminate() {}
}
globalThis.Worker = StandIn;
let carriesWasm = true;
globalThis.fetch = async (url) => {
  const path = String(url);
  if (path.endsWith('wasm-linux/identity')) return new Response(carriesWasm ? 'wasm-digest' : null, { status: carriesWasm ? 200 : 404 });
  if (path.endsWith('wasm-linux/fs.json')) {
    const index = { fsroot: [['bin', 0, 0, 0o040755, 0, 0, [['sh', 0, 0, 0o100755, 0, 0, null], ['echo', 0, 0, 0o100755, 0, 0, null]]]] };
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
// A command the wasm Linux runs, then one only v86 can (network): v86 starts then.
const job = { session: '/threads/0123456789abcdef', timeoutS: 5, files: [], links: [], directories: [], read: () => new Uint8Array() };
assert.equal((await runner.run({ ...job, command: 'echo hi', network: false })).success, true);
assert.deepEqual(kinds(), ['wasm']);
assert.equal((await runner.run({ ...job, command: 'echo hi', network: true })).success, true);
assert.deepEqual(kinds(), ['v86', 'wasm'], 'v86 starts with the command that needs it');
assert.deepEqual(ran, [[job.session, 'wasm'], [job.session, 'v86']]);

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

console.log('linux-warm passed: v86 starts only when asked or with a command that needs it; the wasm Linux on open; which ran each command told');
process.exit(0);
