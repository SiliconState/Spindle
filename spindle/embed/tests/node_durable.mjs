// What must hold under Node when storage misbehaves, through the VFS: a move's seal is
// written into the store's held lock file before the bundle is handed out (a seal that
// cannot be written fails the move, and the session stays here); a move in two steps
// (the bundle kept elsewhere first, then `sealSession` at its head; a session that went
// on since is refused); an import cut short leaves its place empty to try again; a
// storage call that throws comes back as an error, and the package goes on; a retry's
// fresh store is made whole before it takes the old one's place (a swap cut short is
// finished as the session opens, with its settings, spend and restarted turn), both
// stores held against another owner throughout; files given as bytes cross into
// the package once (its memory grows by about their size, not base64's several times).
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { bundleFacts, nodeClient } from '../../clients/js/embedded-node.mjs';
import { KEY, call, catalog, scriptedFetch, text } from './scripted.mjs';

const glue = process.argv[2];
if (!glue) throw new Error('usage: node node_durable.mjs SPINDLE_EMBED_GLUE.js');
const root = fs.mkdtempSync(join(tmpdir(), 'spindle-node-durable-'));
const credentials = async () => ({ authorization: `Bearer ${KEY}` });
const client = (options = {}) => nodeClient({ glue, catalog: catalog(), credentials, fetch: scriptedFetch([]), ...options });
const lockOf = (dir) => join(dir, 'store', 'authority.sqlite.lock');
const taskDirs = (dir) => fs.readdirSync(join(dir, 'tasks')).map((key) => join(dir, 'tasks', key));

/** node:fs whose calls throw where one of `rules` says (`(name, args) => code`). */
function faulty() {
  const rules = [];
  const fake = new Proxy(fs, {
    get(target, name) {
      const value = target[name];
      if (typeof value !== 'function') return value;
      return (...args) => {
        for (const rule of rules) {
          const code = rule(name, args);
          if (code) throw Object.assign(new Error(`${code}: refused by the test (${String(name)} ${args[0]})`), { code });
        }
        return value.apply(target, args);
      };
    },
  });
  return { fs: fake, rules };
}

async function aSealThatCannotBeWritten() {
  const dir = join(root, 'seal-fails');
  const { fs: fake, rules } = faulty();
  const c = client({ fs: fake, fetch: scriptedFetch([text('hello back')]) });
  await c.create(dir, 'fake');
  await c.prompt(dir, 'hello');
  const { head } = await c.status(dir);
  // The seal goes into the held lock file whole (a file renamed over it): refused here.
  rules.push((name, args) => name === 'renameSync' && args[1] === lockOf(dir) && 'EPERM');
  await assert.rejects(c.exportSession(dir, { keep: false }), /EPERM/, 'no bundle is handed out');
  rules.length = 0;
  assert.match(fs.readFileSync(lockOf(dir), 'utf8'), /^spindle-embed: /, 'not sealed: the store is still this owner\'s');
  assert.equal((await c.status(dir)).head, head, 'and the session goes on here');
  // Sealed before the bundle is handed out, not when the store closes.
  const bytes = await c.exportSession(dir, { keep: false });
  assert.equal(c.checkBundle(bytes).head, head);
  assert.match(fs.readFileSync(lockOf(dir), 'utf8'), new RegExp(`^spindle-moved: exported at head ${head} `));
  await assert.rejects(c.status(dir), /moved elsewhere/);
  await c.close();
  assert.match(fs.readFileSync(lockOf(dir), 'utf8'), /^spindle-moved: /, 'the seal stays once closed');
}

async function aMoveInTwoSteps() {
  const dir = join(root, 'two-steps');
  const fetch = scriptedFetch([text('first'), call('write_file', { path: 't.txt', content: 'from the task\n' }), text('task done'), text('second')]);
  const c = client({ fetch });
  await c.create(dir, 'fake', { tasks: true });
  await c.prompt(dir, 'one');
  assert.match((await c.taskStart(dir, 't1', 'write t.txt')).answer, /^task done/);
  const bytes = await c.exportSession(dir);
  const { head, identity, sourceState } = c.checkBundle(bytes);
  assert.deepEqual(bundleFacts(bytes), { head, identity, sourceState }, 'its manifest names them');
  assert.equal(bundleFacts(new Uint8Array(8)), null);
  assert.equal((await c.status(dir)).head, head, 'exported, the session stays here by default');
  assert.match(fs.readFileSync(lockOf(dir), 'utf8'), /^spindle-embed: /);
  await assert.rejects(c.sealSession(dir), TypeError, 'a seal names the head of the bundle kept');
  // It went on since that bundle: refused, nothing sealed.
  await c.prompt(dir, 'two');
  await assert.rejects(c.sealSession(dir, { expectedHead: head, expectedState: sourceState }), (error) => error.kind === 'head_moved');
  const now = await c.exportSession(dir);
  const moved = c.checkBundle(now).head;
  assert.ok(moved > head);
  fs.writeFileSync(join(root, 'two-steps.spindle'), now); // kept where it goes on, first
  assert.deepEqual(await c.sealSession(dir, { expectedHead: moved, expectedState: bundleFacts(now).sourceState }), { session: dir, head: moved, sealed: true });
  await assert.rejects(c.status(dir), /moved elsewhere/);
  for (const lock of [lockOf(dir), ...taskDirs(dir).map(lockOf)]) {
    assert.match(fs.readFileSync(lock, 'utf8'), new RegExp(`^spindle-moved: exported at head ${moved} `), lock);
  }
  const there = join(root, 'two-steps-there');
  assert.equal((await c.importSession(there, fs.readFileSync(join(root, 'two-steps.spindle')))).head, moved);
  assert.equal((await c.tasks(there)).tasks.length, 1, 'its task goes with it');
  await c.close();
  return now;
}

async function aSealAfterSettingsChange() {
  const dir = join(root, 'settings-fence');
  const c = client({ fetch: scriptedFetch([text('hello')]) });
  await c.create(dir, 'fake');
  await c.prompt(dir, 'hi');
  const bytes = await c.exportSession(dir, { settings: { instructions: 'copy only' } });
  const { head, sourceState } = bundleFacts(bytes);
  await c.settings(dir, { instructions: 'changed at the same head' });
  assert.equal((await c.status(dir)).head, head);
  await assert.rejects(c.sealSession(dir, { expectedHead: head, expectedState: sourceState }), (error) => error.kind === 'head_moved');
  const fresh = bundleFacts(await c.exportSession(dir, { settings: { instructions: 'copy only' } }));
  await c.closeSession(dir);
  await c.sealSession(dir, { expectedHead: fresh.head, expectedState: fresh.sourceState });
  await c.close();
}

async function aSealAfterTaskChange() {
  const dir = join(root, 'task-fence');
  const c = client({ fetch: scriptedFetch([text('ready'), text('task done')]) });
  await c.create(dir, 'fake', { tasks: true });
  await c.prompt(dir, 'hello');
  const first = bundleFacts(await c.exportSession(dir));
  await c.taskStart(dir, 'late', 'answer');
  assert.equal((await c.status(dir)).head, first.head);
  await assert.rejects(c.sealSession(dir, { expectedHead: first.head, expectedState: first.sourceState }), (error) => error.kind === 'head_moved');
  const second = bundleFacts(await c.exportSession(dir));
  await c.dropTask(dir, 'late');
  await assert.rejects(c.sealSession(dir, { expectedHead: second.head, expectedState: second.sourceState }), (error) => error.kind === 'head_moved');
  const last = bundleFacts(await c.exportSession(dir));
  await c.sealSession(dir, { expectedHead: last.head, expectedState: last.sourceState });
  await c.close();
}

/** A task's start cut short by a full disk (its identity record refused, after its
 * store was made) leaves no half-made store behind; the remains of one from before
 * (a store of no turns and no record, or a store that does not open) are listed as
 * such, hold nothing to save, do not stop the task above them from being saved or
 * freed, and are freed as they are. */
async function aTaskStartCutShort() {
  const dir = join(root, 'task-cut');
  const { fs: fake, rules } = faulty();
  const c = client({ fs: fake, fetch: scriptedFetch([text('ready'), text('task done')]) });
  await c.create(dir, 'fake', { tasks: true });
  await c.prompt(dir, 'hello');
  const dirsOf = () => (fs.existsSync(join(dir, 'tasks')) ? fs.readdirSync(join(dir, 'tasks')) : []);
  rules.push((name, args) => name === 'openSync' && /\/\.task\.json\.tmp$/.test(String(args[0])) && 'ENOSPC');
  await assert.rejects(c.taskStart(dir, 'cut', 'never begins'), /the disk is full \(ENOSPC\), so task [0-9a-f]{20} could not start; nothing of it was kept/, 'the start says the disk is full');
  rules.length = 0;
  assert.deepEqual(dirsOf(), [], 'nothing half-made is left');
  const [cut] = (await c.tasks(dir)).tasks;
  assert.equal(cut.state, 'stopped', `listed stopped, not running: ${JSON.stringify(cut)}`);
  assert.equal(cut.store, false);
  assert.match(JSON.stringify(cut.answer), /disk is full/, 'saying why');
  // One that did begin, then the remains of two planted beside and under it (as a
  // start cut short before this left them): a store of no turns, and an empty file.
  await c.taskStart(dir, 'real', 'answer');
  const real = (await c.tasks(dir)).tasks.find((task) => task.state === 'finished');
  const realKey = real.task.split('#').pop();
  const plant = (under, key, bytes) => {
    fs.mkdirSync(join(under, 'tasks', key, 'store'), { recursive: true });
    fs.writeFileSync(join(under, 'tasks', key, 'store', 'authority.sqlite'), bytes);
  };
  plant(dir, 'e05864da8e54de94ad27', '');
  plant(join(dir, 'tasks', realKey), '3325c8d40a55d1235f16', '');
  const listed = (await c.tasks(dir)).tasks;
  const remains = listed.find((task) => task.task.endsWith('#e05864da8e54de94ad27'));
  assert.ok(remains, JSON.stringify(listed));
  assert.equal(remains.remains, true);
  assert.equal(remains.state, 'finished');
  assert.equal(remains.store, true);
  assert.match(remains.prompt, /never began/);
  assert.equal(listed.find((task) => task.task === real.task).remains, undefined, 'a task that began is no remains');
  await assert.rejects(c.exportTask(dir, 'e05864da8e54de94ad27'), (error) => error.kind === 'not_found' && /never began[\s\S]*nothing to save/.test(error.message));
  // The task above the nested remains saves and frees without them.
  const saved = await c.exportTask(dir, 'real');
  const copy = join(root, 'task-cut-copy');
  await c.importSession(copy, saved);
  assert.deepEqual((await c.tasks(copy)).tasks, [], 'the nested remains are no part of its bundle');
  const { dropped } = await c.dropTask(dir, 'real', { expectedState: bundleFacts(saved).sourceState });
  assert.ok(dropped.bytes > 0);
  const { dropped: freed } = await c.dropTask(dir, 'e05864da8e54de94ad27', { expectedState: '' });
  assert.equal(freed.key, 'e05864da8e54de94ad27');
  assert.deepEqual(dirsOf(), [], 'all freed');
  assert.equal((await c.status(dir)).head > 0, true, 'the session goes on');
  await c.close();
}

async function anImportCutShort(bytes) {
  const dir = join(root, 'import-cut');
  const { fs: fake, rules } = faulty();
  const c = client({ fs: fake });
  // The session's own store is written last: refused there, after its tasks' files.
  const store = join(dir, 'store', 'authority.sqlite');
  let wrote = 0;
  rules.push((name, args) => {
    if (name === 'renameSync' && String(args[1]).startsWith(`${dir}/tasks/`)) wrote += 1;
    return name === 'renameSync' && args[1] === store && 'ENOSPC';
  });
  await assert.rejects(c.importSession(dir, bytes), /the disk is full \(ENOSPC\), so the session could not be imported; nothing of it was kept/);
  rules.length = 0;
  assert.ok(wrote > 0, 'files were written before it failed');
  assert.deepEqual(fs.readdirSync(dir, { recursive: true, withFileTypes: true }).filter((entry) => entry.isFile()), [], 'what it wrote is gone again');
  const status = await c.importSession(dir, bytes);
  assert.equal(status.imported.head, c.checkBundle(bytes).head, 'and it is tried again there');
  await c.close();
}

async function aStorageCallThatThrows() {
  const { fs: fake, rules } = faulty();
  const c = client({ fs: fake });
  const nowhere = join(root, 'nowhere');
  rules.push((name) => name === 'existsSync' && 'EIO');
  await assert.rejects(c.status(nowhere), (error) => error.kind === 'failed' && /EIO/.test(error.message), 'an error, never taken for no session');
  await assert.rejects(c.create(nowhere, 'fake'), (error) => /EIO/.test(error.message));
  rules.length = 0;
  await assert.rejects(c.status(nowhere), (error) => error.kind === 'not_found', 'the package goes on');
  await c.create(nowhere, 'fake');
  await c.close();
  assert.ok(!fs.existsSync(lockOf(nowhere)), 'closed whole');
}

async function aRetrySwapCutShort() {
  // A swap cut between its renames: the fresh store, whole, takes the name as it opens.
  const dir = join(root, 'swap');
  let c = client({ fetch: scriptedFetch([text('hi')]) });
  await c.create(dir, 'fake', { model: 'small-model' });
  await c.prompt(dir, 'hello');
  const { head } = await c.status(dir);
  await c.closeSession(dir);
  fs.renameSync(join(dir, 'store'), join(dir, 'store.fresh'));
  const status = await c.status(dir);
  assert.equal(status.head, head, 'finished as it opens');
  assert.equal(status.model, 'small-model');
  assert.ok(!fs.existsSync(join(dir, 'store.fresh')));
  await c.close();

  // A retry of a first request whose outcome is unknown (its owner killed after
  // sending it), its swap cut short: the session opens on the fresh store, made whole
  // before the old one moved (its settings, the old request's spend, the restarted
  // turn and its prompt), the old one kept aside.
  const crashed = join(root, 'swap-crash');
  await killedAfterSending(crashed);
  const { fs: fake, rules } = faulty();
  const fetch = scriptedFetch([text('sent once')]);
  c = client({ fs: fake, fetch, catalog: priced() });
  const fresh = join(crashed, 'store.fresh');
  rules.push((name, args) => name === 'renameSync' && args[0] === fresh && 'EIO');
  await assert.rejects(c.retry(crashed), /EIO/);
  rules.length = 0;
  assert.ok(fs.existsSync(join(crashed, 'store.abandoned-1', 'authority.sqlite')), 'the old store is aside');
  assert.ok(!fs.existsSync(join(crashed, 'store.abandoned-1', 'authority.sqlite.lock')), 'its lock file let go where it went');
  assert.ok(!fs.existsSync(join(fresh, 'authority.sqlite.lock')), 'the fresh one\'s let go, for the next open to name it');
  const reopened = await c.status(crashed);
  assert.equal(reopened.head, 1, 'the fresh store, its turn restarted');
  assert.equal(reopened.provider, 'fake', 'with its settings');
  assert.ok(!fs.existsSync(fresh));
  assert.match(JSON.stringify((await c.history(crashed)).messages), /hello/, 'and its prompt');
  assert.equal((await c.usage(crashed)).unknown, 1, 'the set-aside request stays counted');
  assert.equal(fetch.seen.length, 0, 'nothing sent yet');
  assert.equal((await c.resume(crashed)).stopped.complete, true);
  assert.equal(fetch.seen.length, 1, 'continue sends it once');
  await c.close();
}

/** The scripted catalog, its fake model priced (a request of unknown outcome then
 * counts at its worst case). */
function priced() {
  const priced = catalog();
  priced.providers[0].model_specs['fake-model'].pricing = {
    input_usd_per_mtok: 1000, output_usd_per_mtok: 1000, cache_read_usd_per_mtok: 1000, cache_create_usd_per_mtok: 1000,
  };
  return priced;
}

/** A session in `dir` whose owner was killed once its first request was on its way. */
async function killedAfterSending(dir) {
  const child = spawn(process.execPath, [join(dirname(fileURLToPath(import.meta.url)), 'node_session_child.mjs'), glue, dir], { stdio: ['ignore', 'pipe', 'inherit'] });
  await new Promise((resolve, reject) => {
    let out = '';
    child.stdout.on('data', (chunk) => { out += chunk; if (out.includes('sent')) resolve(); });
    child.on('exit', (code) => reject(new Error(`the owner exited (${code}) before sending`)));
  });
  const ended = new Promise((resolve) => child.once('exit', resolve));
  child.kill('SIGKILL');
  await ended;
}

async function aRetrySwapHeldThroughout() {
  // Every store of a retry's swap stays held from before it closes until its directory
  // has its new name: another owner opening the session is refused before the old store
  // moves, and once an opener names the fresh store itself (as an open between the
  // renames does), never a second live copy. The old store's lock file is let go where
  // it went; the fresh one's went with it and is the session's.
  const dir = join(root, 'swap-held');
  await killedAfterSending(dir);
  const wasm = createRequire(import.meta.url)(resolve(glue));
  const store = join(dir, 'store');
  const fresh = join(dir, 'store.fresh');
  const tried = [];
  const intrude = (when) => {
    try {
      wasm.ProbeStore.openNode(join(store, 'authority.sqlite'), 'wal', false, 'another owner').close();
      tried.push(`${when}: opened`);
    } catch (error) {
      tried.push(`${when}: ${error.code}`);
    }
  };
  const { fs: fake, rules } = faulty();
  rules.push((name, args) => {
    if (name !== 'renameSync') return null;
    if (args[0] === store) intrude('before the old store moves');
    if (args[0] === fresh) {
      fs.renameSync(fresh, store);
      intrude('named by another opener');
    }
    return null;
  });
  const fetch = scriptedFetch([text('sent once')]);
  const c = client({ fs: fake, fetch });
  const retried = await c.retry(dir);
  rules.length = 0;
  assert.deepEqual(tried, ['before the old store moves: locked', 'named by another opener: locked'], 'never a second owner');
  assert.equal(retried.stopped.complete, true, 'the swap went on from the store the opener named');
  assert.equal(fetch.seen.length, 1);
  assert.ok(!fs.existsSync(join(dir, 'store.abandoned-1', 'authority.sqlite.lock')), 'the old lock file let go where it went');
  assert.match(fs.readFileSync(lockOf(dir), 'utf8'), new RegExp(`^spindle-embed: pid ${process.pid} on `), 'the session is this client\'s');
  await c.close();
  assert.ok(!fs.existsSync(lockOf(dir)), 'closed whole');
}

/** The package loaded afresh (its own wasm memory, which only grows), with that memory
 * in view. */
function freshPackage(name) {
  const at = join(root, name);
  fs.mkdirSync(at);
  for (const file of fs.readdirSync(dirname(glue))) fs.copyFileSync(join(dirname(glue), file), join(at, file));
  const copy = join(at, 'spindle_embed.js');
  fs.appendFileSync(copy, '\nmodule.exports.__memory = wasm.memory;\n');
  return copy;
}

async function filesAsBytes() {
  const dir = join(root, 'bytes');
  const c = client({ fetch: scriptedFetch([text('got them'), text('got more')]) });
  const binary = Uint8Array.from({ length: 256 }, (_, i) => i);
  await c.create(dir, 'fake', { files: [{ path: 'all.bin', bytes: binary }, { path: 'a.txt', text: 'text too' }] });
  await c.prompt(dir, 'read them', { files: [{ path: 'more.bin', bytes: binary.subarray(250), executable: true }, { path: 'b.txt', base64: btoa('b') }] });
  assert.deepEqual(await c.read(dir, 'all.bin'), binary);
  assert.deepEqual(await c.read(dir, 'more.bin'), binary.subarray(250));
  assert.equal(new TextDecoder().decode(await c.read(dir, 'a.txt')), 'text too');
  assert.equal(new TextDecoder().decode(await c.read(dir, 'b.txt')), 'b');
  assert.equal((await c.files(dir)).files.find((file) => file.path === 'more.bin').executable, true);
  // An attachment the request did not carry is refused.
  await assert.rejects(c.call('prompt', { dir, text: 'x', files: [{ path: 'x.bin', attached: 0 }] }), (error) => error.kind === 'bad_request');
  await c.close();

  // 16 MiB crossing into the package: as bytes, its memory grows by about the file's
  // size; as base64 in the request line, by several times it (the line, its parse, the
  // decoded copy). The prompt is refused once its files are read (a head it does not
  // have), so what grows is the crossing alone, not the store's own work.
  const size = 16 * 2 ** 20;
  const big = new Uint8Array(size);
  for (let at = 0; at < size; at += 65536) crypto.getRandomValues(big.subarray(at, at + 65536));
  const grown = {};
  for (const form of ['bytes', 'base64']) {
    const copy = freshPackage(`package-${form}`);
    const wasm = createRequire(import.meta.url)(copy);
    const d = nodeClient({ glue: copy, catalog: catalog(), credentials });
    const at = join(root, `big-${form}`);
    await d.create(at, 'fake');
    const before = wasm.__memory.buffer.byteLength;
    const file = form === 'bytes' ? { path: 'big.bin', bytes: big } : { path: 'big.bin', base64: Buffer.from(big).toString('base64') };
    await assert.rejects(d.prompt(at, 'take this', { files: [file], expectedHead: 99 }), (error) => error.kind === 'head_moved');
    grown[form] = wasm.__memory.buffer.byteLength - before;
    await d.close();
  }
  assert.ok(grown.bytes <= 1.5 * size, `as bytes, about the file's size: ${JSON.stringify(grown)}`);
  assert.ok(grown.base64 >= 3 * size, `(as base64, several times it: ${JSON.stringify(grown)})`);
  return grown;
}

let grown;
try {
  await aSealThatCannotBeWritten();
  const bytes = await aMoveInTwoSteps();
  await aSealAfterSettingsChange();
  await aSealAfterTaskChange();
  await aTaskStartCutShort();
  await anImportCutShort(bytes);
  await aStorageCallThatThrows();
  await aRetrySwapCutShort();
  await aRetrySwapHeldThroughout();
  grown = await filesAsBytes();
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
console.log(
  'node-durable passed: a move\'s seal written into the held lock before the bundle is handed out (one that cannot be ' +
    'written fails the move, the session going on here); a move in two steps (kept, then sealed at its head, tasks too; ' +
    'a session that went on refused); an import cut short leaving its place empty to try again; a storage call that ' +
    'throws coming back as an error; a retry\'s swap cut short finished as the session opens, with its settings, ' +
    'spend and restarted turn; a retry\'s swap holding both stores against another owner throughout; files ' +
    `as bytes (16 MiB: memory grown ${(grown.bytes / 2 ** 20).toFixed(0)} MiB, ${(grown.base64 / 2 ** 20).toFixed(0)} MiB as base64)`,
);
