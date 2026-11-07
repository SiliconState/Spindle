// Freeing a finished task's space (`exportTask`, then `dropTask`), under Node: sessions
// in directories through the Node VFS, then in an OPFS pool (in-memory sync access
// handles stand in for OPFS, a set of names for Web Locks). The task's bundle is kept
// on disk and read back first, imports as a session of its own and reads; then its
// directory goes, its record and answer stay, the session exports whole, opens again and
// goes on; an unknown task is refused.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { EmbeddedClient, nodeClient } from '../../clients/js/embedded-node.mjs';
import { OpfsFs, adoptSlots } from '../../clients/js/opfs.mjs';
import { KEY, call, catalog, scriptedFetch, text } from './scripted.mjs';

const glue = process.argv[2];
if (!glue) throw new Error('usage: node node_task_storage.mjs SPINDLE_EMBED_GLUE.js');
const root = fs.mkdtempSync(join(tmpdir(), 'spindle-node-task-storage-'));
const credentials = async () => ({ authorization: `Bearer ${KEY}` });

/** The flow on one storage: `client(fetch)` opens a client, `dir(name)` names a session,
 * `left(dir)` lists what is left under a session's task directories. */
async function freesATask({ client, dir: named, left }) {
  const dir = named('thread');
  const fetch = scriptedFetch([text('ready')]);
  let c = client(fetch);
  await c.create(dir, 'fake', { tasks: true });
  await c.prompt(dir, 'hello');
  fetch.replies.push(call('write_file', { path: 't.txt', content: 'from the task\n' }), text('task done'));
  assert.match((await c.taskStart(dir, 't', 'write t.txt')).answer, /^task done/);
  let [task] = (await c.tasks(dir)).tasks;
  assert.equal(task.state, 'finished');
  assert.equal(task.store, true);
  assert.equal(task.orphan, false);
  assert.ok(task.bytes > 4096, `its directory's size: ${task.bytes}`);
  assert.ok(left(dir).length > 0);

  // Kept on disk and read back, then imported as a session of its own.
  const bytes = await c.exportTask(dir, 't');
  const file = join(root, `${task.task.split('#').pop()}.spindle`);
  fs.writeFileSync(file, bytes);
  const kept = fs.readFileSync(file);
  assert.equal(c.checkBundle(kept).files >= 1, true);
  const copy = named('task-copy');
  assert.ok((await c.importSession(copy, kept)).imported.head > 0);
  assert.match(JSON.stringify((await c.history(copy)).messages), /task done/);

  // Its space freed: the directory goes, its record and answer stay.
  const before = Math.floor(Date.now() / 1000);
  await assert.rejects(c.dropTask(dir, task.task, { expectedState: '0'.repeat(64) }), (error) => error.kind === 'conflict');
  const { dropped } = await c.dropTask(dir, task.task, { expectedState: c.checkBundle(kept).sourceState });
  assert.equal(dropped.bytes, task.bytes);
  assert.equal(dropped.task, 'app:t');
  assert.equal(dropped.prompt, 'write t.txt');
  assert.ok(dropped.at >= before, `at: ${dropped.at}`);
  assert.deepEqual(left(dir), [], 'nothing of it is left');
  [task] = (await c.tasks(dir)).tasks;
  assert.equal(task.store, false);
  assert.equal(task.bytes, null);
  assert.deepEqual(task.dropped, dropped);
  assert.match(task.answer.content, /^task done/);
  await assert.rejects(c.exportTask(dir, 't'), (error) => error.kind === 'not_found');
  await assert.rejects(c.dropTask(dir, 'nope'), (error) => error.kind === 'not_found');

  // The session exports whole without it, opens again and goes on.
  const whole = await c.exportSession(dir);
  const moved = named('thread-copy');
  await c.importSession(moved, whole);
  assert.deepEqual((await c.tasks(moved)).tasks[0].dropped, dropped);
  await c.close();
  c = client(scriptedFetch([text('going on')]));
  assert.equal((await c.prompt(dir, 'more')).stopped.complete, true);
  await c.close();
  return { bytes: dropped.bytes, file: kept.length };
}

// Node: directories through the VFS.
const onNode = await freesATask({
  client: (fetch) => nodeClient({ glue, catalog: catalog(), credentials, fetch }),
  dir: (name) => join(root, name),
  left: (dir) => [join(dir, 'tasks'), join(dir, 'tasks.removing')]
    .flatMap((path) => (fs.existsSync(path) ? fs.readdirSync(path) : [])),
});

// OPFS: a pool of in-memory slots behind Web Locks.
const wasm = createRequire(import.meta.url)(glue);
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
      const next = new Uint8Array(at + from.length);
      next.set(this.bytes);
      this.bytes = next;
    }
    this.bytes.set(from, at);
    return from.length;
  }
  flush() {}
  close() {}
}
const files = new Map();
const directory = {
  async getFileHandle(name) {
    if (!files.has(name)) files.set(name, new Handle());
    return { createSyncAccessHandle: async () => files.get(name) };
  },
};
let opfs = null;
const open = async () => {
  const slots = Array.from({ length: 24 }, (_, index) => ({ handle: (files.has(`slot-${index}`) || files.set(`slot-${index}`, new Handle()), files.get(`slot-${index}`)), path: '' }));
  opfs = new OpfsFs(slots, directory, adoptSlots(slots));
  return opfs;
};
const onOpfs = await freesATask({
  client: (fetch) => new EmbeddedClient({ wasm, storage: 'opfs', pool: open, catalog: catalog(), credentials, fetch }),
  dir: (name) => `/sessions/${name}`,
  left: (dir) => opfs.files().map(({ path }) => path).filter((path) => path.startsWith(`${dir}/tasks/`) || path.startsWith(`${dir}/tasks.removing/`)),
});
assert.equal(held.size, 0);
fs.rmSync(root, { recursive: true, force: true });

console.log(`node-task-storage passed: a finished task kept in a file (${onNode.file} bytes on Node, ${onOpfs.file} in OPFS), read back and imported as a session of its own; its ${onNode.bytes} (Node) and ${onOpfs.bytes} (OPFS) bytes freed with its record and answer kept; the session exported whole, opened again and went on; an unknown task refused`);
