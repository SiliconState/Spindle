// The embedded package in a real browser: OPFS storage behind Web
// Locks in Dedicated Workers of headless Chrome. The package (wasm-bindgen
// `--target web` glue in GLUE_DIR), the JS client and the page are laid out as a
// local site and opened from files (no network: Chrome reads files with
// --allow-file-access-from-files); results are read over the DevTools protocol
// (--remote-debugging-pipe). Chrome runs twice on one profile: two workers contend
// for one session (the second is refused until the first closes, then reads its
// work), and after a restart a new worker reopens the session and continues it, and
// compacts a session whose store has free pages (a bundle made with the Node glue beside
// GLUE_DIR, when it and node:sqlite are there).
//
// usage: node browser_opfs.mjs GLUE_DIR   (SPINDLE_BROWSER names Chrome or Chromium)
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { StdioClient } from '../../clients/js/spindle.mjs';
import { nodeClient } from '../../clients/js/embedded-node.mjs';
import { KEY, call, catalog, scriptedFetch, text } from './scripted.mjs';

const glue = process.argv[2];
const browser = process.env.SPINDLE_BROWSER;
if (!glue) throw new Error('usage: node browser_opfs.mjs GLUE_DIR');
if (!browser) {
  console.log('browser-opfs skipped: set SPINDLE_BROWSER to a Chrome or Chromium binary');
  process.exit(0);
}
const here = fileURLToPath(new URL('.', import.meta.url));
const work = mkdtempSync(join(tmpdir(), 'spindle-opfs-browser-'));
const site = join(work, 'site');
const profile = join(work, 'profile');
mkdirSync(site);
cpSync(glue, join(site, 'glue'), { recursive: true });
cpSync(join(here, '../../clients/js'), join(site, 'clients'), { recursive: true });
cpSync(join(here, 'browser'), join(site, 'test'), { recursive: true });
cpSync(join(here, 'scripted.mjs'), join(site, 'scripted.mjs'));

/** A session bundle whose store has free pages (8 MiB of them), made with the Node glue
 * beside GLUE_DIR and node:sqlite, for the browser to compact; null where either is
 * missing. */
async function fragmentedBundle() {
  const nodeGlue = join(glue, '..', 'node', 'spindle_embed.js');
  const sqlite = await import('node:sqlite').catch(() => null);
  if (!sqlite || !existsSync(nodeGlue)) return null;
  const dir = join(work, 'fragmented');
  const fetch = scriptedFetch([call('write_file', { path: 'notes.md', content: 'kept\n' }), text('done')]);
  let client = nodeClient({ glue: nodeGlue, catalog: catalog(), credentials: async () => ({ authorization: `Bearer ${KEY}` }), fetch });
  await client.create(dir, 'fake');
  await client.prompt(dir, 'write notes');
  await client.close();
  const database = join(dir, 'store', 'authority.sqlite');
  const db = new sqlite.DatabaseSync(database);
  db.exec(`CREATE TABLE junk(b BLOB);
    WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 8)
    INSERT INTO junk SELECT randomblob(1048576) FROM n;
    DROP TABLE junk;`);
  db.exec('PRAGMA wal_checkpoint(TRUNCATE)');
  db.close();
  for (const end of ['-wal', '-shm']) rmSync(database + end, { force: true });
  client = nodeClient({ glue: nodeGlue, catalog: catalog(), credentials: async () => ({}), fetch });
  const bytes = await client.exportSession(dir, { keep: true });
  await client.close();
  return bytes;
}
const fragmented = await fragmentedBundle();
if (fragmented) writeFileSync(join(site, 'test', 'fragmented.spindle'), fragmented);

/** Opens the page in a fresh Chrome on the shared profile; resolves with `window.result`. */
async function run(phase) {
  const url = `${pathToFileURL(join(site, 'test', 'index.html'))}?phase=${phase}`;
  const chrome = spawn(browser, [
    '--headless', '--no-sandbox', '--disable-gpu', '--no-first-run',
    '--allow-file-access-from-files', '--remote-debugging-pipe', `--user-data-dir=${profile}`, url,
  ], { stdio: ['ignore', 'ignore', 'ignore', 'pipe', 'pipe'] });
  const [write, read] = [chrome.stdio[3], chrome.stdio[4]];
  let buffer = '';
  let next = 0;
  const waiting = new Map();
  read.on('data', (chunk) => {
    buffer += chunk;
    for (let at = buffer.indexOf('\0'); at >= 0; at = buffer.indexOf('\0')) {
      const message = JSON.parse(buffer.slice(0, at));
      buffer = buffer.slice(at + 1);
      waiting.get(message.id)?.(message);
      waiting.delete(message.id);
    }
  });
  const send = (method, params = {}, sessionId) => new Promise((resolve) => {
    const id = ++next;
    waiting.set(id, resolve);
    write.write(`${JSON.stringify({ id, method, params, ...(sessionId && { sessionId }) })}\0`);
  });
  try {
    const { result: { targetInfos } } = await send('Target.getTargets');
    const page = targetInfos.find((target) => target.type === 'page');
    const { result: { sessionId } } = await send('Target.attachToTarget', { targetId: page.targetId, flatten: true });
    const until = Date.now() + 90_000;
    while (Date.now() < until) {
      const reply = await send('Runtime.evaluate', { expression: 'JSON.stringify(window.result ?? null)', returnByValue: true }, sessionId);
      const value = reply.result?.result?.value;
      if (value && value !== 'null') return JSON.parse(value);
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
    throw new Error(`no result from Chrome (${phase})`);
  } finally {
    chrome.kill('SIGKILL');
    await new Promise((resolve) => (chrome.exitCode !== null ? resolve() : chrome.once('exit', resolve)));
  }
}

const step = (result, name) => {
  const found = result.steps.find(([label]) => label === name);
  assert.ok(found, `${name} missing: ${JSON.stringify(result.steps)}`);
  return found[1];
};

try {
  const first = await run('first');
  const created = step(first, 'a.create');
  assert.equal(created.ok, true, JSON.stringify(created));
  const refused = step(first, 'b.status.while.a.holds');
  assert.equal(refused.ok, false, 'the second worker is refused while the first holds the session');
  assert.equal(refused.kind, 'locked', JSON.stringify(refused));
  assert.equal(step(first, 'a.close').ok, true);
  const after = step(first, 'b.status.after.a');
  assert.equal(after.ok, true, JSON.stringify(after));
  assert.equal(after.head, created.head, "the second worker sees the first one's turn");
  assert.equal(step(first, 'b.read').text, 'hello from opfs');
  assert.equal(step(first, 'b.reopen').ok, true, JSON.stringify(step(first, 'b.reopen')));
  assert.equal(step(first, 'b.reopen').count, 80, 'idle close/reopen waits for Chrome to release the same worker\'s Web Lock');
  const { id: _a, ...pool } = step(first, 'b.pool');
  assert.deepEqual(pool, { ok: true, full: 'ENOSPC', before: 1, notDir: 'EISDIR', after: 4, gone: true, kept: 'kept' }, 'the pool grows and removes trees');

  const second = await run('reopen');
  const status = step(second, 'c.status');
  assert.equal(status.ok, true, JSON.stringify(status));
  assert.equal(status.head, created.head, 'the session survives a browser restart');
  assert.equal(step(second, 'c.read').text, 'hello from opfs');
  const prompted = step(second, 'c.prompt');
  assert.equal(prompted.ok, true, JSON.stringify(prompted));
  assert.ok(prompted.head > created.head);
  const exported = step(second, 'c.export');
  assert.equal(exported.ok, true, JSON.stringify(exported));
  const moved = step(second, 'c.status.moved');
  assert.equal(moved.ok, false, 'its old place is sealed');
  assert.match(moved.message, /moved elsewhere/);
  const imported = step(second, 'c.import');
  assert.equal(imported.ok, true, JSON.stringify(imported));
  assert.equal(imported.head, prompted.head, 'the same session, moved');
  assert.equal(step(second, 'c.read.b').text, 'hello from opfs');
  const goes = step(second, 'c.prompt.b');
  assert.equal(goes.ok, true, JSON.stringify(goes));
  assert.ok(goes.head > prompted.head);
  const compacted = step(second, 'c.compact');
  if (fragmented) {
    assert.equal(compacted.ok, true, JSON.stringify(compacted));
    assert.equal(compacted.head, compacted.importedHead, 'the same session, compacted');
    assert.ok(compacted.before - compacted.after >= 7 * 1048576, `the free pages went: ${JSON.stringify(compacted)}`);
    assert.equal(compacted.stores, 1);
    assert.ok(compacted.sameFiles, 'its files are the same');
    assert.equal(compacted.text, 'kept\n');
    assert.equal(compacted.againBefore, compacted.againAfter, 'nothing more to give back');
  }
  const { id: _b, ...grown } = step(second, 'c.pool');
  assert.deepEqual(grown, { ok: true, free: 4, kept: 'kept', gone: false }, 'a reopened pool has the slots a grow added');
  // A native host continues what the browser exported (when SPINDLE_BIN is set).
  if (process.env.SPINDLE_BIN) {
    const home = join(work, 'home');
    const env = { ...process.env, HOME: home, DEXT_HOME: join(home, '.dext') };
    mkdirSync(env.DEXT_HOME, { recursive: true });
    const file = join(work, 'from-browser.spindle');
    writeFileSync(file, Buffer.from(exported.base64, 'base64'));
    const host = new StdioClient({ binary: process.env.SPINDLE_BIN, env, stderr: 'ignore' });
    const status = await host.importSession(join(work, 'native'), file);
    assert.equal(status.head, prompted.head, 'a native host opens the browser session');
    assert.equal((await host.files(join(work, 'native'))).files[0].path, 'a.txt');
    await host.close();
  }
} finally {
  rmSync(work, { recursive: true, force: true });
}
console.log(
  'browser-opfs passed: OPFS sessions in headless Chrome workers (create and turn, second worker ' +
    'refused by the Web Lock, reopened by it after the first closed, reopened after a restart, ' +
    'moved by bundle to another place and, with SPINDLE_BIN, into a native host; the pool grows, ' +
    'removes trees and reopens grown; a session with free pages compacts in OPFS)',
);
