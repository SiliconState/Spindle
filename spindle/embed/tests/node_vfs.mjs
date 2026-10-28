// The Node storage of spindle-embed: stores in files through the
// VFS over node:fs, for both journals. Create/open/reopen keep one identity; every
// commit fsyncs its files and new files fsync their directory; a second owner is
// refused while the lock file exists; an owner killed between reservation and
// receipt leaves the outcome unknown and its lock in place until an operator acts.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const glue = process.argv[2];
if (!glue) throw new Error('usage: node node_vfs.mjs SPINDLE_EMBED_GLUE.js');
const spindle = createRequire(import.meta.url)(glue);

// fs, watched: which paths were opened, and which of them were fsynced.
const paths = new Map();
const synced = [];
const watched = {
  ...fs,
  openSync(path, flags, mode) {
    const fd = fs.openSync(path, flags, mode);
    paths.set(fd, path);
    return fd;
  },
  fsyncSync(fd) {
    synced.push(paths.get(fd));
    return fs.fsyncSync(fd);
  },
};
spindle.installNodeFs(watched, process.platform);

const root = fs.mkdtempSync(join(tmpdir(), 'spindle-node-vfs-'));
const code = (run) => {
  try {
    run();
  } catch (error) {
    return error.code;
  }
  return 'ok';
};

function lifecycle(journal) {
  const dir = join(root, journal);
  fs.mkdirSync(dir);
  const database = join(dir, 'authority.sqlite');
  const lock = `${database}.lock`;

  assert.equal(code(() => spindle.ProbeStore.openNode(database, journal, false, 'test')), 'failed',
    'opening a missing store fails');
  assert.ok(!fs.existsSync(database) && !fs.existsSync(lock), 'a failed open leaves no files');

  synced.length = 0;
  const store = spindle.ProbeStore.openNode(database, journal, true, 'test owner');
  const first = JSON.parse(store.reserve());
  assert.equal(first.revision, 1);
  assert.ok(synced.includes(database), `the database is fsynced (${journal})`);
  assert.ok(synced.includes(dir), `creating files fsyncs their directory (${journal})`);
  const companion = journal === 'wal' ? `${database}-wal` : `${database}-journal`;
  assert.ok(synced.includes(companion), `the ${journal} journal is fsynced`);

  assert.equal(code(() => spindle.ProbeStore.openNode(database, journal, false, 'second')), 'locked',
    'a second owner is refused while the first holds the lock');
  assert.equal(spindle.nodeLockHolder(database), 'spindle-embed: test owner');
  store.close();
  assert.ok(!fs.existsSync(lock), 'closing removes the lock file');

  assert.equal(code(() => spindle.ProbeStore.openNode(database, journal, true, 'again')), 'incompatible',
    'create refuses a store that has history');
  assert.ok(!fs.existsSync(lock), 'a refused create releases its lock');

  const reopened = spindle.ProbeStore.openNode(database, journal, false, 'reopen');
  const seen = JSON.parse(reopened.report());
  assert.equal(seen.identity, first.identity, 'reopening keeps the identity');
  assert.equal(seen.revision, 1);
  assert.ok(seen.epoch > first.epoch, 'a new owner takes a new epoch');
  assert.equal(seen.reserved, true);
  assert.equal(seen.receipt, null, 'a reservation without a receipt stays unknown');
  assert.equal(JSON.parse(reopened.settle('the reply')).conflicting_receipt_refused, true);
  reopened.close();

  const again = spindle.ProbeStore.openNode(database, journal, false, 'third');
  assert.equal(JSON.parse(again.report()).receipt, 'the reply', 'the receipt survives a reopen');
  again.close();
}

function owner(database, journal) {
  const child = spawn(process.execPath, [
    fileURLToPath(new URL('node_vfs_child.mjs', import.meta.url)), glue, database, journal,
  ], { stdio: ['ignore', 'pipe', 'inherit'] });
  return new Promise((resolve, reject) => {
    let out = '';
    child.stdout.on('data', (chunk) => {
      out += chunk;
      const line = out.match(/^reserved (.*)$/m);
      if (line) resolve({ child, seen: JSON.parse(line[1]) });
    });
    child.on('exit', (status) => reject(new Error(`the owner exited (${status}) before reserving: ${out}`)));
  });
}

async function crashCut(journal) {
  const dir = join(root, `crash-${journal}`);
  fs.mkdirSync(dir);
  const database = join(dir, 'authority.sqlite');
  const { child, seen } = await owner(database, journal);
  const ended = new Promise((resolve) => child.once('exit', resolve));
  child.kill('SIGKILL');
  await ended;

  assert.equal(code(() => spindle.ProbeStore.openNode(database, journal, false, 'next')), 'locked',
    'a killed owner\'s lock is never taken over automatically');
  assert.equal(spindle.nodeLockHolder(database), `spindle-embed: pid ${child.pid}`, 'the lock names its owner');
  // The operator's decision, after checking that pid is gone.
  fs.unlinkSync(`${database}.lock`);

  const store = spindle.ProbeStore.openNode(database, journal, false, 'next');
  const after = JSON.parse(store.report());
  assert.equal(after.identity, seen.identity);
  assert.equal(after.revision, 1, 'the committed step survived the kill');
  assert.equal(after.reserved, true, 'the reservation survived the kill');
  assert.equal(after.receipt, null, 'the outcome stays unknown; nothing is resent');
  store.settle('reply from the host');
  assert.equal(JSON.parse(store.report()).receipt, 'reply from the host');
  store.close();
}

try {
  for (const journal of ['delete', 'wal']) {
    lifecycle(journal);
    await crashCut(journal);
  }
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
console.log(
  'node-vfs passed: DELETE and WAL stores via node:fs (create/open/reopen, identity kept, ' +
    'file and directory fsyncs, lock refusal, killed owner leaves unknown + fail-closed lock)',
);
