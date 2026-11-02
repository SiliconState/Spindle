// The OPFS pool cut short at every point of a rename, an open or a deletion, in Node over
// in-memory slots. Each operation's writes are recorded, then replayed onto the slots as
// they were before it, up to a crash: the write it reached is kept up to every one of its
// bytes (a write that stopped part way, then power lost), and with every set of its 512-byte
// sectors (power lost while the disk wrote them, in any order). The pool opened again must
// show each file whole under one of its two names (a directory under one of its two,
// never split between them), no name made of two, nothing lost, and every header the
// older version can read as this one does; a second open changes nothing more. Slots
// written by an older version (no check in their headers) are swept the same way.
import assert from 'node:assert/strict';
import { OpfsFs, adoptSlots } from '../../clients/js/opfs.mjs';

const HEADER = 4096;
const SECTOR = 512;

/** A sync access handle in memory whose writes, truncations and flushes `log` records. */
class Handle {
  constructor(bytes = new Uint8Array(0), log = null) {
    this.bytes = bytes;
    this.log = log;
  }
  getSize() { return this.bytes.length; }
  truncate(size) {
    this.log?.push({ handle: this, size });
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
    // `fail`: a header write loses power before anything is written.
    if (this.fail && at < HEADER) throw new Error('power lost');
    this.log?.push({ handle: this, at, bytes: from.slice() });
    if (at + from.length > this.bytes.length) {
      const next = new Uint8Array(at + from.length);
      next.set(this.bytes);
      this.bytes = next;
    }
    this.bytes.set(from, at);
    return from.length;
  }
  flush() { this.log?.push({ handle: this, flushed: true }); }
  close() {}
}

const text = (bytes) => new TextDecoder().decode(bytes);
const open = (handles) => {
  const pool = handles.map((handle) => ({ handle, path: '' }));
  return { pool, fs: new OpfsFs(pool, null, adoptSlots(pool)) };
};
const put = (fs, path, content) => {
  const fd = fs.openSync(path, 'w');
  const bytes = new TextEncoder().encode(content);
  fs.writeSync(fd, bytes, 0, bytes.length, 0);
};
/** A header as the older version reads it (its own code, kept here): every slot this
 * version names must read the same there, so rolling the package back loses nothing. */
function olderRead(handle) {
  if (handle.getSize() < HEADER) return { path: '', generation: 0 };
  const header = handle.bytes.subarray(0, HEADER);
  if (!new TextEncoder().encode('spindle-opfs-slot-v1').every((byte, at) => header[at] === byte)) return { path: '', generation: 0 };
  const length = header[20] | (header[21] << 8);
  const path = new TextDecoder().decode(header.subarray(22, 22 + length));
  return { path, generation: Number(new DataView(header.buffer, header.byteOffset).getBigUint64(HEADER - 8, true)) };
}

/** What a pool holds: path -> content, and its lost slots. */
const state = (fs, pool) => ({
  files: Object.fromEntries(pool.filter((slot) => slot.path).map((slot) => [slot.path, fs.readFileSync(slot.path)])),
  lost: fs.lost,
  free: fs.free,
});

/** A header as the older version wrote it: no check. */
function legacy(path, generation, content) {
  const header = new Uint8Array(HEADER + content.length);
  header.set(new TextEncoder().encode('spindle-opfs-slot-v1'));
  const name = new TextEncoder().encode(path);
  header[20] = name.length & 0xff;
  header[21] = name.length >> 8;
  header.set(name, 22);
  new DataView(header.buffer).setBigUint64(HEADER - 8, BigInt(generation), true);
  header.set(new TextEncoder().encode(content), HEADER);
  return header;
}

/** The ways the write `step` may have reached the disk when power was lost: up to each of
 * its bytes, or any set of its sectors. */
function* cuts(step) {
  if (step.flushed) return;
  if (!step.bytes) return yield { ...step, size: undefined, skip: true };
  for (let kept = 0; kept < step.bytes.length; kept += 1) yield { ...step, bytes: step.bytes.subarray(0, kept) };
  const sectors = Math.ceil(step.bytes.length / SECTOR);
  if (sectors < 2) return;
  for (let set = 1; set < 2 ** sectors - 1; set += 1) yield { ...step, sectors: set };
}

/** Sweeps `operation` (given the opened fs) over every crash point, from slots holding
 * `before`, checking each reopened pool with `check(state)`. Returns how many it checked. */
function sweep(before, operation, check) {
  const log = [];
  const handles = before.map((bytes) => new Handle(bytes.slice(), log));
  const { fs } = open(handles);
  log.length = 0;
  try {
    operation(fs);
  } catch (error) {
    throw new Error(`the operation itself failed: ${error.message}`);
  }
  const steps = [...log];
  const index = new Map(handles.map((handle, at) => [handle, at]));
  let checked = 0;
  for (let reached = 0; reached <= steps.length; reached += 1) {
    const partial = reached < steps.length ? cuts(steps[reached]) : [null];
    // Power lost may also drop a write its file had not flushed yet while keeping later
    // ones (a disk orders nothing it was not told to): each such write, left out.
    const unflushed = steps.slice(0, reached).flatMap((step, at) => (step.bytes && !steps.slice(at + 1, reached).some((later) => later.flushed && later.handle === step.handle) ? [at] : []));
    for (const cut of [...partial, ...unflushed.map((dropped) => ({ dropped }))]) {
      const copies = before.map((bytes) => new Handle(bytes.slice()));
      steps.slice(0, reached).forEach((step, at) => { if (at !== cut?.dropped) apply(copies[index.get(step.handle)], step); });
      if (cut?.handle) apply(copies[index.get(cut.handle)], cut);
      const first = open(copies);
      const seen = state(first.fs, first.pool);
      const where = `${reached}/${steps.length}${cut?.dropped != null ? ` without unflushed write ${cut.dropped}` : cut ? ` ${cut.sectors != null ? `sectors ${cut.sectors.toString(2)}` : `${cut.bytes?.length ?? 'skip'} bytes`}` : ' (whole)'}`;
      check(seen, where);
      assert.ok(first.pool.every((slot) => !slot.path.includes('\0')), `no record or cut name is left (${where})`);
      for (const slot of first.pool) {
        if (!slot.path) continue;
        assert.deepEqual(olderRead(slot.handle), { path: slot.path, generation: slot.generation }, `the older version reads ${slot.path} the same (${where})`);
      }
      const again = open(copies);
      assert.deepEqual(state(again.fs, again.pool), seen, `a second open changes nothing (${where})`);
      checked += 1;
    }
  }
  return checked;
}

function apply(handle, step) {
  if (step.skip || step.flushed) return;
  if (step.size != null) return handle.truncate(step.size);
  if (step.sectors == null) return handle.write(step.bytes, { at: step.at });
  for (let sector = 0; sector * SECTOR < step.bytes.length; sector += 1) {
    if (!(step.sectors & (1 << sector))) continue;
    handle.write(step.bytes.subarray(sector * SECTOR, (sector + 1) * SECTOR), { at: step.at + sector * SECTOR });
  }
}

/** Slots holding `files` (path -> content) as this version writes them, plus `spare` empty. */
function made(files, spare = 4) {
  const handles = Array.from({ length: Object.keys(files).length + spare }, () => new Handle());
  const { fs } = open(handles);
  for (const [path, content] of Object.entries(files)) put(fs, path, content);
  return handles.map((handle) => handle.bytes);
}

const store = { '/d/store/authority.sqlite': 'database', '/d/store/authority.sqlite-wal': 'log', '/d/store/fence': 'seal' };
const under = (dir) => Object.fromEntries(Object.entries(store).map(([path, content]) => [path.replace('/d/store/', `${dir}/`), content]));
const others = { '/d/other': 'other', '/d/store.x': 'beside' };
const canonical = (files) => JSON.stringify(Object.entries(files).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)));
const oneOf = (files, ...choices) => choices.some((choice) => canonical(files) === canonical(choice));
let total = 0;

// A store's directory set aside (as Start over does): whole under one name or the other.
for (const [label, before] of [
  ['this version', made({ ...store, ...others })],
  ['an older version', [...Object.entries({ ...store, ...others }).map(([path, content], at) => legacy(path, at + 1, content)), ...Array.from({ length: 4 }, () => new Uint8Array(0))]],
]) {
  total += sweep(before, (fs) => fs.renameSync('/d/store', '/d/store.abandoned-1'), (seen, where) => {
    const rest = Object.fromEntries(Object.entries(seen.files).filter(([path]) => !path.startsWith('/d/store/') && !path.startsWith('/d/store.abandoned-1/')));
    assert.equal(canonical(rest), canonical(others), `${label}: nothing else changes (${where})`);
    const moving = Object.fromEntries(Object.entries(seen.files).filter(([path]) => !(path in rest)));
    assert.ok(oneOf(moving, store, under('/d/store.abandoned-1')), `${label}: the store is whole under one name (${where}): ${JSON.stringify(Object.keys(moving))}`);
    assert.equal(seen.lost, 0, `${label}: nothing lost (${where})`);
  });
}

// A file replacing another (a compacted copy taking the database's name): the name holds
// the old file or the new, never neither, and the new one is under one of its names.
for (const [label, before] of [
  ['this version', made({ '/d/store/authority.sqlite': 'old', '/d/store/authority.sqlite.copy': 'new', ...others })],
  ['an older version', [legacy('/d/store/authority.sqlite', 1, 'old'), legacy('/d/store/authority.sqlite.copy', 2, 'new'), legacy('/d/other', 3, 'other'), ...Array.from({ length: 3 }, () => new Uint8Array(0))]],
]) {
  total += sweep(before, (fs) => fs.renameSync('/d/store/authority.sqlite.copy', '/d/store/authority.sqlite'), (seen, where) => {
    const name = seen.files['/d/store/authority.sqlite'];
    assert.ok(name === 'old' || name === 'new', `${label}: the name holds a whole file (${where}): ${name}`);
    const copy = seen.files['/d/store/authority.sqlite.copy'];
    assert.equal(copy, name === 'new' ? undefined : 'new', `${label}: the copy took the name, or is still the copy (${where})`);
    assert.equal(seen.lost, 0, `${label}: nothing lost (${where})`);
    assert.ok(Object.keys(seen.files).every((path) => path.startsWith('/d/store/authority.sqlite') || path in others), `${label}: no other name (${where})`);
  });
}

// A file renamed to a new name (as write_file renames its `.NAME.tmp`), with bytes or
// empty: under one of its two names, whole, never neither.
for (const content of ['the only copy', '']) {
  for (const [label, before] of [
    ['this version', made({ '/d/store/.authority.sqlite.tmp': content, ...others })],
    ['an older version', [legacy('/d/store/.authority.sqlite.tmp', 1, content), legacy('/d/other', 2, 'other'), legacy('/d/store.x', 3, 'beside'), ...Array.from({ length: 3 }, () => new Uint8Array(0))]],
  ]) {
    total += sweep(before, (fs) => fs.renameSync('/d/store/.authority.sqlite.tmp', '/d/store/authority.sqlite'), (seen, where) => {
      assert.ok(oneOf(seen.files, { ...others, '/d/store/.authority.sqlite.tmp': content }, { ...others, '/d/store/authority.sqlite': content }), `${label}, ${content ? 'bytes' : 'empty'}: under one of its names (${where}): ${JSON.stringify(seen.files)}`);
      assert.equal(seen.lost, 0, `${label}: nothing lost (${where})`);
    });
  }
}

// A file made, and one deleted: there or not, never under another name, nothing lost.
total += sweep(made(others), (fs) => put(fs, '/d/new', ''), (seen, where) => {
  assert.ok(oneOf(seen.files, others, { ...others, '/d/new': '' }), `made: ${where}: ${JSON.stringify(seen.files)}`);
  assert.equal(seen.lost, 0);
});
for (const before of [made({ ...others, '/d/gone': 'bytes' }), [legacy('/d/gone', 1, 'bytes'), legacy('/d/other', 2, 'other'), legacy('/d/store.x', 3, 'beside'), new Uint8Array(0)]]) {
  total += sweep(before, (fs) => fs.unlinkSync('/d/gone'), (seen, where) => {
    assert.ok(oneOf(seen.files, others, { ...others, '/d/gone': 'bytes' }), `deleted: ${where}: ${JSON.stringify(seen.files)}`);
    assert.equal(seen.lost, 0);
  });
}

// A header damaged otherwise (not cut by this package: a byte of its name changed): its
// bytes are kept apart (`lost`, never reused) until the app reclaims them, which frees
// them for good; the other files stay.
{
  const before = made({ '/d/db': 'the database', ...others });
  const index = (() => {
    const { pool } = open(before.map((bytes) => new Handle(bytes.slice())));
    return pool.findIndex((slot) => slot.path === '/d/db');
  })();
  before[index][23] ^= 0x20;
  let reopened = open(before.map((bytes) => new Handle(bytes.slice())));
  const copies = reopened.pool.map((slot) => slot.handle);
  assert.equal(reopened.fs.lost, 1);
  assert.equal(reopened.fs.lostBytes, 'the database'.length);
  assert.equal(reopened.fs.existsSync('/d/db'), false);
  const free = reopened.fs.free;
  assert.equal(reopened.fs.reclaim(), 'the database'.length, 'the bytes freed');
  assert.deepEqual([reopened.fs.lost, reopened.fs.lostBytes, reopened.fs.free], [0, 0, free + 1]);
  assert.equal(canonical(state(reopened.fs, reopened.pool).files), canonical(others));
  reopened = open(copies);
  assert.deepEqual([reopened.fs.lost, reopened.fs.free], [0, free + 1], 'freed for good');
  assert.equal(reopened.fs.reclaim(), 0);
}

// An older version's file with a name of 256 bytes or more, deleted: there whole, or gone
// (kept apart at most, never under a shorter name).
{
  const long = `/d/${'n'.repeat(300)}`;
  total += sweep([legacy(long, 1, 'bytes'), legacy('/d/other', 2, 'other'), legacy('/d/store.x', 3, 'beside'), new Uint8Array(0)], (fs) => fs.unlinkSync(long), (seen, where) => {
    assert.ok(oneOf(seen.files, others, { ...others, [long]: 'bytes' }), `long name deleted: ${where}: ${JSON.stringify(Object.keys(seen.files))}`);
    assert.ok(seen.lost <= 1);
  });
}

// A header as this version writes it, rebuilt here (magic, length, path, then the check:
// generation copy and CRC-32; the slot's mark; the generation).
const CRC = Uint32Array.from({ length: 256 }, (_, byte) => {
  let crc = byte;
  for (let bit = 0; bit < 8; bit += 1) crc = crc & 1 ? 0xedb88320 ^ (crc >>> 1) : crc >>> 1;
  return crc;
});
const crc32 = (bytes) => {
  let crc = 0xffffffff;
  for (const byte of bytes) crc = CRC[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  return (crc ^ 0xffffffff) >>> 0;
};
function sealedHeader(path, generation) {
  const header = legacy(path, generation, '').subarray(0, HEADER);
  const view = new DataView(header.buffer, header.byteOffset);
  const end = 22 + new TextEncoder().encode(path).length;
  view.setBigUint64(end, BigInt(generation), true);
  view.setUint32(end + 8, crc32(header.subarray(0, end + 8)), true);
  header.set(new TextEncoder().encode('chk1'), HEADER - 12);
  return header;
}

// A header cut short that is not one name written over the other (here, two names no move
// wrote) is never given a move's name, even in a slot its record lists: kept apart.
{
  const handles = Array.from({ length: 8 }, () => new Handle());
  let { pool, fs } = open(handles);
  put(fs, '/t/store/a', 'a');
  put(fs, '/t/store/b', 'b');
  const b = pool.findIndex((slot) => slot.path === '/t/store/b');
  handles[b].fail = true;
  assert.throws(() => fs.renameSync('/t/store', '/t/x'));
  handles[b].fail = false;
  // Its header now: the start of one unrelated name over another, its check failing.
  const mixed = sealedHeader('/q/elsewhere/zz', 9);
  mixed.set(sealedHeader('/q/other/file', 9).subarray(0, 30));
  handles[b].bytes.set(mixed);
  ({ pool, fs } = open(handles));
  assert.equal(fs.readFileSync('/t/x/a'), 'a', 'the move is finished for the file it listed');
  assert.equal(fs.existsSync('/t/x/b'), false, 'no move name for a header it did not write');
  assert.equal(fs.lost, 1);
}

// An older version's record of a move is its two whole names only: one cut short (a
// longer record of this version's, written in a slot without its mark yet, or a name cut
// mid-way) moves nothing, even files from before generations (0).
{
  const before = [legacy('/d/store/a', 0, 'a'), legacy('/d/store/b', 0, 'b'), new Uint8Array(0), new Uint8Array(0)];
  const cutRecord = legacy('\0move\0/d/store\0/d/st', 0, '');
  // Its length says more than was written: the rest reads as NULs.
  cutRecord[20] = 40;
  before[2] = cutRecord.subarray(0, HEADER).slice();
  const { pool, fs } = open(before.map((bytes) => new Handle(bytes.slice())));
  assert.equal(fs.readFileSync('/d/store/a'), 'a');
  assert.equal(fs.readFileSync('/d/store/b'), 'b');
  assert.equal(fs.existsSync('/d/st'), false, 'nothing moved');
  assert.ok(pool.every((slot) => !slot.path.includes('\0')), 'the cut record is let go of');
}

// A directory of more files than one record holds moves under several (whole here; the
// cuts above cover each record's and each file's header writes).
{
  const many = Object.fromEntries(Array.from({ length: 150 }, (_, at) => [`/d/big/${'f'.repeat(20)}-${at}`, `${at}`]));
  const handles = Array.from({ length: 160 }, () => new Handle());
  const { fs, pool } = open(handles);
  for (const [path, content] of Object.entries(many)) put(fs, path, content);
  const free = fs.free;
  fs.renameSync('/d/big', '/d/moved');
  assert.equal(fs.free, free, 'its records are let go of');
  assert.deepEqual(state(fs, pool).files, Object.fromEntries(Object.entries(many).map(([path, content]) => [path.replace('/d/big/', '/d/moved/'), content])));
}

console.log(`node-opfs-torn passed: ${total} crash points (every byte and sector set of each header write of a directory set aside, a file replacing another, a file renamed to a new name with bytes or empty, a file made and one deleted (a 300-byte name too), over slots this version wrote and an older one did) each reopen to every file whole under one name, a directory never split, no name made of two, nothing lost, every header read the same by the older version; a header damaged otherwise kept apart until reclaimed, then freed for good; a move too large for one record goes under several`);
