// The embedded client over the OPFS pool, in Node: in-memory sync access handles stand
// in for OPFS and a map of names for Web Locks. A session with more tasks than the pool
// has free slots exports (every store open at once) and imports into a pool that has
// to grow first (it cannot grow during the call); a move in two steps seals the OPFS
// copy; a call that never opened its session gives the session's Web Lock back when
// the session is closed.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { EmbeddedClient } from '../../clients/js/embedded.mjs';
import { OpfsFs, adoptSlots } from '../../clients/js/opfs.mjs';
import { KEY, call, catalog, scriptedFetch, text } from './scripted.mjs';

const glue = process.argv[2];
if (!glue) throw new Error('usage: node node_opfs_client.mjs SPINDLE_EMBED_GLUE.js');
const wasm = createRequire(import.meta.url)(glue);

/** A sync access handle in memory. */
class Handle {
  bytes = new Uint8Array(0);
  getSize() { return this.bytes.length; }
  truncate(size) {
    const next = new Uint8Array(size);
    next.set(this.bytes.subarray(0, Math.min(size, this.bytes.length)));
    this.bytes = next;
  }
  read(into, { at }) {
    const part = this.bytes.subarray(at, Math.min(at + into.length, this.bytes.length));
    into.set(part);
    return part.length;
  }
  write(from, { at }) {
    if (at + from.length > this.bytes.length) {
      const next = new Uint8Array(Math.max(at + from.length, this.bytes.length * 2));
      next.set(this.bytes);
      this.bytes = next.subarray(0, at + from.length);
    }
    this.bytes.set(from, at);
    return from.length;
  }
  flush() {}
  close() {}
}

/** An OPFS directory of pool slots in memory: `slots` to begin with, growing as asked. */
function pool(slots) {
  const files = new Map();
  const dir = {
    async getFileHandle(name) {
      if (!files.has(name)) files.set(name, new Handle());
      return { createSyncAccessHandle: async () => files.get(name) };
    },
  };
  const opened = { fs: null, calls: 0, slots: () => files.size };
  opened.open = async () => {
    opened.calls += 1;
    await new Promise((resolve) => setImmediate(resolve));
    const slotsNow = Array.from({ length: slots }, (_, index) => ({ handle: files.get(`slot-${index}`) ?? (files.set(`slot-${index}`, new Handle()), files.get(`slot-${index}`)), path: '' }));
    opened.fs = new OpfsFs(slotsNow, dir, adoptSlots(slotsNow));
    return opened.fs;
  };
  return opened;
}

// Web Locks: a name is held until its callback's promise settles.
const held = new Set();
Object.defineProperty(globalThis, 'navigator', {
  configurable: true,
  value: {
    locks: {
      request(name, options, callback) {
        if (held.has(name)) return Promise.resolve().then(() => callback(null));
        held.add(name);
        return Promise.resolve().then(() => callback({ name })).finally(() => held.delete(name));
      },
    },
  },
});

const credentials = async () => ({ authorization: `Bearer ${KEY}` });
const client = (opened, fetch = scriptedFetch([])) => new EmbeddedClient({ wasm, storage: 'opfs', pool: opened.open, catalog: catalog(), credentials, fetch });
const storesUnder = (fs, dir) => fs.files().filter(({ path }) => path.startsWith(`${dir}/`) && path.endsWith('/store/authority.sqlite')).length;
const TASKS = 14;

// A session with many tasks in a pool of 12 slots: the pool grows on its own as the
// tasks' stores name files (clients/js/opfs.mjs LOW_WATER), between the files a step
// names; no caller grows it.
const first = pool(12);
const fetch = scriptedFetch([text('ready')]);
let c = client(first, fetch);
const dir = '/sessions/many';
await c.create(dir, 'fake', { tasks: true });
await c.prompt(dir, 'hello');
for (let index = 0; index < TASKS; index++) {
  fetch.replies.push(call('write_file', { path: `t${index}.txt`, content: `task ${index}\n` }), text(`task ${index} done`));
  assert.match((await c.taskStart(dir, `t${index}`, `write t${index}.txt`)).answer, new RegExp(`^task ${index} done`));
}
assert.equal(storesUnder(first.fs, dir), TASKS + 1);
assert.ok(first.slots() > 12, `the pool grew on its own (${first.slots()} slots)`);
assert.ok(first.fs.free >= 1, `and keeps slots free (${first.fs.free})`);

// Exported (every store open at once): the pool grows first.
let growing = 0;
let concurrent = 0;
const grow = first.fs.grow.bind(first.fs);
first.fs.grow = async (count) => {
  growing += 1;
  concurrent = Math.max(concurrent, growing);
  try {
    await new Promise((resolve) => setTimeout(resolve, 5));
    return await grow(count);
  } finally { growing -= 1; }
};
const [bytes] = await Promise.all([c.exportSession(dir), c.exportSession(dir)]);
assert.ok(concurrent <= 1, `concurrent exports grow the pool one at a time, if at all (${concurrent})`);
const { head, files } = c.checkBundle(bytes);
assert.ok(files > 2 * TASKS, `${files} files`);
assert.equal((await c.status(dir)).head, head, 'kept here');

// Moved in two steps: the bundle kept, then this copy sealed at its head.
assert.deepEqual(await c.sealSession(dir, { expectedHead: head, expectedState: c.checkBundle(bytes).sourceState }), { session: dir, head, sealed: true });
await assert.rejects(c.status(dir), /moved elsewhere/);
const fence = first.fs.readFileSync(`${dir}/store/authority.sqlite.lock`);
assert.match(fence, new RegExp(`^spindle-moved: exported at head ${head} `));

// A call that never opened its session (there is none) holds its Web Lock no longer
// than until the session is closed.
await assert.rejects(c.status('/sessions/none'), (error) => error.kind === 'not_found');
assert.ok(held.has('spindle:/sessions/none'), 'held while the app may still use it');
assert.deepEqual(await c.closeSession('/sessions/none'), { session: '/sessions/none', closed: false });
assert.ok(!held.has('spindle:/sessions/none'), 'given back once closed');
await c.close();
assert.equal(held.size, 0);

// Imported into a small pool: it grows to hold every file first.
const second = pool(12);
c = client(second, scriptedFetch([text('going on')]));
const imported = await c.importSession('/sessions/moved', bytes);
assert.equal(imported.imported.head, head);
assert.equal((await c.tasks('/sessions/moved')).tasks.length, TASKS);
assert.equal(storesUnder(second.fs, '/sessions/moved'), TASKS + 1);
assert.equal((await c.prompt('/sessions/moved', 'go on')).stopped.complete, true);
await c.close();

const third = pool(12);
c = client(third);
await Promise.all([c.create('/sessions/one', 'fake'), c.create('/sessions/two', 'fake')]);
assert.equal(third.calls, 1, 'concurrent first sessions open and install just one pool');
await c.close();

console.log(`node-opfs-client passed: a session of ${TASKS} tasks exported from a pool with fewer slots free than its stores open at once, and imported into a 12-slot pool (${files} files), each pool grown first; a move in two steps sealing the OPFS copy; a session's Web Lock taken by a call that never opened it given back when the session is closed`);
