// Giving a session's free pages back (`compactSession`), under Node through the VFS. A
// store with free pages (a table made and dropped leaves them, as a migration in place
// does) shrinks to what it uses and keeps its history, head and files, then goes on;
// its tasks' stores shrink with it; a copy a cut-short compaction left goes; a turn
// running refuses it. Needs node:sqlite (Node 22.5+) to make the free pages.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import { EmbeddedClient, nodeClient } from '../../clients/js/embedded-node.mjs';
import { KEY, call, catalog, hold, scriptedFetch, text } from './scripted.mjs';

const glue = process.argv[2];
if (!glue) throw new Error('usage: node node_compact.mjs SPINDLE_EMBED_GLUE.js');
const sqlite = await import('node:sqlite').catch(() => null);
if (!sqlite) {
  console.log(`node-compact skipped: Node ${process.versions.node} has no node:sqlite`);
  process.exit(0);
}
const root = fs.mkdtempSync(join(tmpdir(), 'spindle-node-compact-'));
const credentials = async () => ({ authorization: `Bearer ${KEY}` });
const client = (fetch) => nodeClient({ glue, catalog: catalog(), credentials, fetch });
const MIB = 1024 * 1024;

/** Leaves `mib` MiB of free pages in the store at `database` (closed). */
function fragment(database, mib) {
  const db = new sqlite.DatabaseSync(database);
  db.exec(`CREATE TABLE junk(b BLOB);
    WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ${mib})
    INSERT INTO junk SELECT randomblob(${MIB}) FROM n;
    DROP TABLE junk;`);
  db.exec('PRAGMA wal_checkpoint(TRUNCATE)');
  const free = db.prepare('PRAGMA freelist_count').get().freelist_count;
  db.close();
  for (const end of ['-wal', '-shm']) fs.rmSync(database + end, { force: true });
  return free;
}

const dir = join(root, 'session');
const database = join(dir, 'store', 'authority.sqlite');
const fetch = scriptedFetch([
  call('write_file', { path: 'notes.md', content: 'kept through the compaction\n' }),
  text('first answer'),
]);
let c = client(fetch);
await c.create(dir, 'fake', { tasks: true });
assert.equal((await c.prompt(dir, 'hello')).stopped.complete, true);
fetch.replies.push(call('write_file', { path: 't.txt', content: 'from the task\n' }), text('task done'));
assert.match((await c.taskStart(dir, 't1', 'write t.txt')).answer, /^task done/);
const history = await c.history(dir);
const head = (await c.status(dir)).head;
await c.close();

// Free pages in the session's store and its task's.
const [task] = fs.readdirSync(join(dir, 'tasks'));
const taskDatabase = join(dir, 'tasks', task, 'store', 'authority.sqlite');
assert.ok(fragment(database, 8) > 1000, 'the store has free pages');
assert.ok(fragment(taskDatabase, 4) > 500, "the task's store has free pages");
const [sizeBefore, taskBefore] = [database, taskDatabase].map((path) => fs.statSync(path).size);

c = client(fetch);
assert.equal((await c.status(dir)).head, head, 'the store opens as it was');
const compacted = await c.compactSession(dir);
assert.equal(compacted.stores, 2, 'the session and its task');
assert.ok(compacted.before - compacted.after >= 11 * MIB, `about 12 MiB given back: ${JSON.stringify(compacted)}`);
assert.ok(fs.statSync(database).size <= sizeBefore - 7 * MIB, 'the store file shrank');
assert.ok(fs.statSync(taskDatabase).size <= taskBefore - 3 * MIB, "the task's store file shrank");
for (const path of [database, taskDatabase]) {
  for (const end of ['.compact', '.compact-wal', '.compact-journal', '.compact.lock', '.lock']) {
    assert.ok(!fs.existsSync(path + end), `nothing left: ${path + end}`);
  }
}
// No crash is counted for it (the copy, taken while the store was open, says the store
// closed cleanly, as it then did).
await c.closeSession(dir);
for (const path of [database, taskDatabase]) {
  const db = new sqlite.DatabaseSync(path, { readOnly: true });
  assert.deepEqual({ ...db.prepare('SELECT closed, crashes FROM opens WHERE id=1').get() }, { closed: 1, crashes: 0 }, path);
  db.close();
}
// The same session, and it goes on.
assert.deepEqual(await c.history(dir), history);
assert.equal((await c.status(dir)).head, head);
assert.equal(new TextDecoder().decode(await c.read(dir, 'notes.md')), 'kept through the compaction\n');
fetch.replies.push(text('after the compaction'));
assert.equal((await c.prompt(dir, 'and on')).stopped.complete, true);

// A copy a compaction cut short left (a crash before its rename): the store is the
// store, and the next compaction removes the copy.
await c.closeSession(dir);
fs.writeFileSync(`${database}.compact`, 'half a copy');
const again = await c.compactSession(dir);
assert.ok(!fs.existsSync(`${database}.compact`), 'the stale copy goes');
assert.equal(again.stores, 2);
assert.equal((await c.status(dir)).head > head, true);

// Another owner holding the store (its lock file there): refused, the store untouched.
await c.closeSession(dir);
fs.writeFileSync(`${database}.lock`, 'spindle-embed: another app');
const untouched = fs.readFileSync(database);
await assert.rejects(c.compactSession(dir), /another spindle process|locked|held/i);
assert.ok(fs.readFileSync(database).equals(untouched), 'the store is as it was');
assert.equal(fs.readFileSync(`${database}.lock`, 'utf8'), 'spindle-embed: another app', "the other owner's lock stays");
fs.rmSync(`${database}.lock`);

// A turn running: refused (the turn goes on, here until it is stopped).
fetch.replies.push(hold('thinking'));
const holding = new Promise((resolve) => { fetch.holding = resolve; });
const turn = c.prompt(dir, 'take your time');
await holding;
await assert.rejects(c.compactSession(dir), /running/);
await c.stop(dir);
await turn.catch(() => {});
await c.close();

// A session in memory (kept nowhere on disk): nothing to give back, and it stays open.
const wasm = createRequire(import.meta.url)(glue);
const memory = new EmbeddedClient({ wasm, catalog: catalog(), credentials, fetch: scriptedFetch([text('in memory')]) });
await memory.create('kept-in-memory', 'fake');
await memory.prompt('kept-in-memory', 'hello');
const remembered = await memory.history('kept-in-memory');
assert.deepEqual(await memory.compactSession('kept-in-memory'), { session: 'kept-in-memory', before: 0, after: 0, stores: 0 });
assert.deepEqual(await memory.history('kept-in-memory'), remembered, 'the memory session is still there');
await memory.close();

fs.rmSync(root, { recursive: true, force: true });
console.log('node-compact passed: free pages given back (a session and its task), history, head and files kept, the session goes on; a stale copy goes; a store another owner holds and a running turn refuse; a session in memory stays');
