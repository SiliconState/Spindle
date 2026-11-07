// The v86 Linux's worker (chrome/linux-worker.mjs) with a stand-in machine and agent (no
// VM image), its timings shortened, its threads' workspaces on a real v86 share
// (linux-workspace.mjs), and the local addresses its web gateway refuses (linux-net.mjs):
//   - a fresh boot that cannot be saved (no room) runs all the same, once; nothing of the
//     file is left to resume from;
//   - a saved state that never loads goes, and the machine boots afresh;
//   - a command that never ends is asked to stop, then let go of alone: another thread's
//     command on the same machine runs on and is answered; so is one the user stopped;
//   - a machine whose agent says nothing restarts, and the next command boots another;
//   - the bytes a file comes with become the share's (a second file of them gets a copy
//     the guest's writes to the first do not reach), and a changed file is read back as
//     one copy, handed on whole;
//   - NAT64 addresses of local IPv4 ones, and 198.18.0.0/15, are local.
//
// usage: node chrome/tests/linux_worker.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { local } from '../linux-net.mjs';
import { Workspaces, objectDigest } from '../linux-workspace.mjs';
import { FS } from '../wasm-linux/v86/lib/filesystem.js';

const IDENTITY = 'test-identity';
const STATE = `state-${IDENTITY}-net1`;
const encode = (text) => new TextEncoder().encode(text);
const sleep = (ms) => new Promise((done) => setTimeout(done, ms));
async function until(check, what, ms = 5000) {
  for (const end = Date.now() + ms; ; await sleep(10)) {
    const value = check();
    if (value) return value;
    if (Date.now() > end) throw new Error(`no ${what} in ${ms} ms`);
  }
}

// ---- the worker's world: its messages, OPFS, the image's identity ----
const messages = [];
globalThis.self = globalThis;
globalThis.postMessage = (message) => messages.push(message);
const opfs = new Map();
let noRoom = false;
const linuxDir = {
  async *keys() { yield* [...opfs.keys()]; },
  async removeEntry(name) {
    if (!opfs.delete(name)) throw new DOMException(name, 'NotFoundError');
  },
  async getFileHandle(name, { create = false } = {}) {
    if (!opfs.has(name)) {
      if (!create) throw new DOMException(name, 'NotFoundError');
      opfs.set(name, new ArrayBuffer(0));
    }
    return {
      getFile: async () => ({ arrayBuffer: async () => opfs.get(name) }),
      createWritable: async () => {
        let written = null;
        return {
          async write(data) {
            if (noRoom) throw new DOMException('no room', 'QuotaExceededError');
            written = data;
          },
          async close() { opfs.set(name, written); },
          async abort() {},
        };
      },
    };
  },
};
Object.defineProperty(globalThis, 'navigator', {
  configurable: true,
  value: { storage: { getDirectory: async () => ({ getDirectoryHandle: async () => linuxDir }) } },
});
globalThis.fetch = async () => ({ text: async () => `${IDENTITY}\n` });

// ---- the stand-in machine and its agent ----
/** How the agent behaves: `resumes` (a saved state loads), `silent` (answers nothing),
 * `onRun(vm, id)` and `onStop(vm, id)`. */
let agent;
const machines = [];
class StandInV86 {
  constructor(options) {
    this.options = options;
    this.listeners = new Map();
    this.destroyed = false;
    this.got = [];
    this.fs9p = new FS();
    this.network_adapter = { send() {} };
    machines.push(this);
    setTimeout(() => {
      if (!options.initial_state) this.say('READY', '0');
      else if (agent.resumes) this.emit('emulator-ready');
    }, 5);
  }
  add_listener(name, listener) {
    if (!this.listeners.has(name)) this.listeners.set(name, []);
    this.listeners.get(name).push(listener);
  }
  emit(name, ...args) {
    for (const listener of this.listeners.get(name) ?? []) listener(...args);
  }
  /** A frame from the agent. */
  say(kind, id, bytes = new Uint8Array()) {
    for (const char of `\x02${kind} ${id} ${Buffer.from(bytes).toString('base64')}\n`) this.emit('serial0-output-byte', char.charCodeAt(0));
  }
  serial0_send(line) {
    const [kind, id] = line.slice(1, -1).split(' ');
    this.got.push([kind, id]);
    if (this.destroyed || agent.silent) return;
    if (kind === 'PING') setTimeout(() => this.say('READY', id), 1);
    else if (kind === 'RUN') agent.onRun?.(this, id);
    else if (kind === 'STOP') agent.onStop?.(this, id);
  }
  async save_state() {
    return new ArrayBuffer(64);
  }
  async destroy() {
    this.destroyed = true;
  }
}
const end = (vm, id, { code = 0, timed_out = false, stopped = false } = {}) => vm.say('END', id, encode(JSON.stringify({ code, timed_out, stopped })));

// ---- the worker, loaded afresh for each case (its state is its module's) ----
const chrome = new URL('../', import.meta.url).href;
const original = readFileSync(new URL('../linux-worker.mjs', import.meta.url), 'utf8');
const LIBV86 = "import { V86 } from './linux/libv86.mjs';";
const BASE = "new URL('./linux/', import.meta.url).href";
assert.ok(original.includes(LIBV86) && original.includes(BASE), 'linux-worker.mjs imports v86 and finds its image as this test expects');
const source = original
  .replace(LIBV86, 'const { V86 } = globalThis.__linuxStandIn;')
  .replace(BASE, JSON.stringify(`${chrome}linux/`))
  .replaceAll("from './", `from '${chrome}`)
  .replace(/const GRACE_MS = [\d_]+;/, 'const GRACE_MS = 300;')
  .replace(/const PING_MS = [\d_]+;/, 'const PING_MS = 50;')
  .replace(/const SILENT_MS = [\d_]+;/, 'const SILENT_MS = 500;')
  .replace(/const BOOT_MS = [\d_]+;/, 'const BOOT_MS = 2000;')
  .replace(/const RESUME_MS = [\d_]+;/, 'const RESUME_MS = 300;');
globalThis.__linuxStandIn = { V86: StandInV86 };
let loaded = 0;
async function freshWorker(behaviour) {
  agent = { resumes: true, silent: false, ...behaviour };
  machines.length = 0;
  messages.length = 0;
  await import(`data:text/javascript,${encodeURIComponent(`${source}\n// worker ${++loaded}`)}`);
}
const send = (data) => self.onmessage({ data });
let next = 0;
const run = (session, { timeoutS = 30 } = {}) => {
  const id = ++next;
  send({ type: 'run', id, session, command: 'true', timeoutS, network: false, files: [], links: [], directories: [] });
  return id;
};
const ran = (id, ms) => until(() => messages.find((message) => message.type === 'ran' && message.id === id)?.result, `an answer for command ${id}`, ms);
const answer = (vm, id, text) => {
  vm.say('OUT', id, encode(text));
  end(vm, id);
};

// A fresh boot that cannot be saved: the machine runs anyway, and is the only one.
opfs.clear();
noRoom = true;
await freshWorker({ onRun: (vm, id) => setTimeout(() => answer(vm, id, `ran ${id}`), 5) });
const first = await ran(run('a'));
assert.deepEqual([first.success, first.output], [true, 'ran 1'], first.failure);
const second = await ran(run('a'));
assert.deepEqual([second.success, second.output], [true, 'ran 2'], second.failure);
assert.equal(machines.length, 1, 'one boot: a machine not saved is kept, not booted again for each command');
assert.equal(machines[0].destroyed, false);
assert.ok(messages.some((message) => message.type === 'status' && message.state === 'ready' && /not saved/.test(message.detail ?? '')), 'it says it was not saved');
assert.equal(opfs.has(STATE), false, 'nothing of the state is left to resume from');
noRoom = false;

// A saved state that never loads: let go of, and the machine boots afresh (and is saved).
opfs.clear();
opfs.set(STATE, new ArrayBuffer(64));
await freshWorker({ resumes: false, onRun: (vm, id) => setTimeout(() => answer(vm, id, 'fresh'), 5) });
const fresh = await ran(run('a'));
assert.deepEqual([fresh.success, fresh.output], [true, 'fresh'], fresh.failure);
assert.deepEqual(machines.map((vm) => Boolean(vm.options.initial_state)), [true, false], 'resumed, then booted');
assert.equal(opfs.get(STATE)?.byteLength, 64, 'the fresh boot was saved');

// A command that never ends, beside another thread's: it is asked to stop, then let go of
// alone; the other runs on, on the same machine, and its answer comes.
await freshWorker();
const stuck = run('a', { timeoutS: 0.1 });
const beside = run('b');
agent.onRun = (vm, id) => { if (id === String(beside)) setTimeout(() => answer(vm, id, 'still here'), 1200); };
const stuckResult = await ran(stuck);
assert.equal(stuckResult.success, false);
assert.match(stuckResult.failure, /did not finish within 0.1 s/);
assert.ok(machines[0].got.some(([kind, id]) => kind === 'STOP' && id === String(stuck)), 'the agent was asked to stop it');
const besideResult = await ran(beside);
assert.deepEqual([besideResult.success, besideResult.output], [true, 'still here'], besideResult.failure);
assert.deepEqual([machines.length, machines[0].destroyed], [1, false], 'the machine was not restarted');

// The user's Stop of a command the agent does not end: that command alone ends.
agent.onRun = null;
const stopped = run('a');
await until(() => machines[0].got.some(([kind, id]) => kind === 'RUN' && id === String(stopped)), 'the command sent');
send({ type: 'stop', id: stopped });
const stoppedResult = await ran(stopped);
assert.deepEqual([stoppedResult.success, stoppedResult.failure], [false, 'the command was stopped']);
assert.deepEqual([machines.length, machines[0].destroyed], [1, false], 'the machine was not restarted');

// A machine whose agent says nothing: restarted, whatever its commands' own times; the
// next command boots another.
const hung = run('a', { timeoutS: 600 });
await until(() => machines[0].got.some(([kind, id]) => kind === 'RUN' && id === String(hung)), 'the command sent');
agent.silent = true;
const hungResult = await ran(hung, 3000);
assert.match(hungResult.failure, /stopped answering; it was restarted/);
assert.equal(machines[0].destroyed, true);
agent.silent = false;
agent.onRun = (vm, id) => setTimeout(() => answer(vm, id, 'again'), 5);
const again = await ran(run('a'));
assert.deepEqual([again.success, again.output, machines.length], [true, 'again', 2], again.failure);
console.log('ok the worker: an unsaved boot is kept, a state that never loads boots afresh, one stuck command ends alone, a silent machine restarts');

// ---- the share: a file's bytes taken as they came, and read back as one copy ----
const fs9p = new FS();
const workspaces = new Workspaces(fs9p);
const content = encode('same bytes');
const object = await objectDigest(content);
const files = [
  { path: 'a.txt', size: content.length, executable: false, object },
  { path: 'b.txt', size: content.length, executable: false, object },
];
const plan = workspaces.plan('t', { files });
const copy = await workspaces.build(plan, { files, links: [], directories: [] }, new Map([[object, content]]));
const [a, b] = ['a.txt', 'b.txt'].map((name) => copy.placed.get(name).id);
assert.equal(fs9p.inodedata[a], content, 'the bytes that came are the file\'s, not a copy');
assert.notEqual(fs9p.inodedata[b], content, 'a second file of the same bytes gets a copy of its own');
await fs9p.Write(a, 0, 4, encode('SAME'));
assert.equal(new TextDecoder().decode(await fs9p.get_data(b, 0, fs9p.inodes[b].size)), 'same bytes', 'the guest writing one leaves the other');
const collected = await workspaces.collect(copy);
assert.deepEqual(collected.writes.map((write) => write.path), ['a.txt']);
const [write] = collected.writes;
assert.equal(new TextDecoder().decode(write.content), 'SAME bytes');
assert.equal(write.content.buffer.byteLength, write.content.length + 26, 'read back as the one copy its digest was made over');
assert.equal(workspaces.kept.get('t').files.get('a.txt').digest, await objectDigest(write.content));
console.log('ok the share: bytes that came are taken as they are (a second file copies them), and a change is read back as one copy');

// ---- the gateway: local addresses, however written ----
const hostOf = (url) => new URL(url).hostname;
for (const url of ['http://[64:ff9b::10.0.0.1]/', 'http://[64:ff9b::127.0.0.1]/', 'http://[64:ff9b::192.168.1.1]/', 'http://[64:ff9b::0.0.0.0]/',
  'http://[64:ff9b::100.64.0.1]/', 'http://[64:ff9b:1::a00:1]/', 'http://198.18.0.1/', 'http://198.19.255.254/']) {
  assert.equal(local(hostOf(url)), true, url);
}
for (const url of ['http://[64:ff9b::8.8.8.8]/', 'http://[64:ff9b::1.1.1.1]/', 'http://198.17.0.1/', 'http://198.20.0.1/', 'http://[2606:4700::1111]/']) {
  assert.equal(local(hostOf(url)), false, url);
}
console.log('ok the gateway: NAT64 addresses of local IPv4 ones, and 198.18.0.0/15, are local');
console.log('linux-worker passed');
process.exit(0);
