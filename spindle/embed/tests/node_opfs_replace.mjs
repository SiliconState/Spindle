// The OPFS pool's renames under a crash, in Node over in-memory slots. A file that
// replaces another takes the name first (under a newer generation) and the old slot is
// let go of after, so power lost between the two leaves the new file under the name, and
// reopening the pool keeps it and lets the older slot go: never no file. A directory moves
// under a record of the move, so power lost midway is finished by the next open: never a
// directory split between its two names. A handle may also read or write less than asked
// and say so only by its count: a slot's header is still read and written whole, and a
// rename that cannot write it fails before it lets anything go.
import assert from 'node:assert/strict';
import { OpfsFs, adoptSlots, openOpfsPool } from '../../clients/js/opfs.mjs';

const HEADER = 4096;

/** A sync access handle in memory; `fail` makes its next header write lose power, and
 * `headers` is how many header write calls it takes before power is lost. Short counts:
 * `short` is the most one header write call takes, `progress` how many header write calls
 * take anything (then they take nothing), `shortRead` the most one read gives, and nothing
 * is read from `unreadableFrom` on. `flushFails` makes flush throw. */
class Handle {
  bytes = new Uint8Array(0);
  fail = false;
  headers = Infinity;
  short = Infinity;
  progress = Infinity;
  shortRead = Infinity;
  unreadableFrom = Infinity;
  flushFails = false;
  open = false;
  getSize() { return this.bytes.length; }
  truncate(size) {
    const next = new Uint8Array(size);
    next.set(this.bytes.subarray(0, Math.min(size, this.bytes.length)));
    this.bytes = next;
  }
  read(into, { at }) {
    const end = Math.min(at + into.length, at + this.shortRead, Math.max(at, this.unreadableFrom));
    const part = this.bytes.subarray(at, end);
    into.set(part);
    return part.length;
  }
  write(from, { at }) {
    if (at < HEADER) {
      if (this.fail || this.headers-- <= 0) throw new Error('power lost');
      if (this.progress-- <= 0) return 0;
      from = from.subarray(0, this.short);
    }
    if (at + from.length > this.bytes.length) this.truncate(at + from.length);
    this.bytes.set(from, at);
    return from.length;
  }
  flush() {
    if (this.flushFails) throw new Error('flush failed');
  }
  close() { this.open = false; }
}

const handles = Array.from({ length: 12 }, () => new Handle());
const open = (over = handles) => {
  const pool = over.map((handle) => ({ handle, path: '' }));
  return { pool, fs: new OpfsFs(pool, null, adoptSlots(pool)) };
};
const slotOf = (pool, path) => pool.findIndex((slot) => slot.path === path);
const put = (fs, path, text) => {
  const fd = fs.openSync(path, 'w');
  const bytes = new TextEncoder().encode(text);
  fs.writeSync(fd, bytes, 0, bytes.length, 0);
};

// A plain replacement: the new content under the name, one slot holding it.
let { fs } = open();
put(fs, '/s/session.json', 'old');
put(fs, '/s/session.json.tmp', 'new');
fs.renameSync('/s/session.json.tmp', '/s/session.json');
assert.equal(fs.readFileSync('/s/session.json'), 'new');
assert.equal(fs.existsSync('/s/session.json.tmp'), false);
assert.equal(open().fs.readFileSync('/s/session.json'), 'new', 'the replacement survives a reopen');

// Power lost after the new file took the name, before the old slot was let go of.
({ fs } = open());
put(fs, '/s/session.json.tmp', 'newer');
const holder = open().pool.findIndex((slot) => slot.path === '/s/session.json');
handles[holder].fail = true;
assert.throws(() => fs.renameSync('/s/session.json.tmp', '/s/session.json'), /power lost/);
handles[holder].fail = false;
const after = open();
const named = after.pool.filter((slot) => slot.path === '/s/session.json');
assert.equal(named.length, 1, 'one slot holds the name after the reopen');
assert.equal(after.fs.readFileSync('/s/session.json'), 'newer', 'the new file is the one kept');
assert.equal(after.fs.existsSync('/s/session.json.tmp'), false);

// Power lost before the new file took the name: the old file is still there, whole.
({ fs } = open());
put(fs, '/s/task.json.tmp', 'next');
const moving = open().pool.findIndex((slot) => slot.path === '/s/task.json.tmp');
put(fs, '/s/task.json', 'kept');
handles[moving].fail = true;
assert.throws(() => fs.renameSync('/s/task.json.tmp', '/s/task.json'), /power lost/);
handles[moving].fail = false;
const before = open().fs;
assert.equal(before.readFileSync('/s/task.json'), 'kept');
assert.equal(before.readFileSync('/s/task.json.tmp'), 'next');

// A directory moved aside (a session's store, as Start over does): power lost after its
// first file moved. The reopened pool finishes the move and lets its record go.
const store = ['authority.sqlite', 'authority.sqlite-journal', 'fence'];
const slotsOf = (pool, dir) => pool.flatMap((slot, at) => (slot.path.startsWith(`${dir}/`) ? [at] : []));
({ fs } = open());
for (const name of store) put(fs, `/d/store/${name}`, name);
const freeBefore = fs.free;
handles[slotsOf(open().pool, '/d/store')[1]].fail = true;
assert.throws(() => fs.renameSync('/d/store', '/d/store.abandoned-1'), /power lost/);
for (const handle of handles) handle.fail = false;
let moved = open();
assert.deepEqual(slotsOf(moved.pool, '/d/store'), [], 'nothing is left under the old name');
for (const name of store) assert.equal(moved.fs.readFileSync(`/d/store.abandoned-1/${name}`), name);
assert.equal(moved.fs.free, freeBefore, 'the record of the move is let go of');

// Power lost while the record is written: nothing has moved, and nothing does.
({ fs } = moved);
for (const name of store) put(fs, `/d/store/${name}`, `new ${name}`);
const record = moved.pool.findIndex((slot) => !slot.path);
handles[record].fail = true;
assert.throws(() => fs.renameSync('/d/store', '/d/store.abandoned-2'), /power lost/);
handles[record].fail = false;
moved = open();
for (const name of store) assert.equal(moved.fs.readFileSync(`/d/store/${name}`), `new ${name}`);
assert.equal(moved.fs.existsSync('/d/store.abandoned-2'), false);

// Power lost after the last file moved, before the record goes: the open just lets it go.
({ fs } = moved);
const last = moved.pool.findIndex((slot) => !slot.path);
handles[last].headers = 1;
assert.throws(() => fs.renameSync('/d/store', '/d/store.abandoned-2'), /power lost/);
handles[last].headers = Infinity;
moved = open();
for (const name of store) assert.equal(moved.fs.readFileSync(`/d/store.abandoned-2/${name}`), `new ${name}`);
assert.equal(moved.fs.existsSync('/d/store'), false);
assert.equal(moved.fs.free, handles.length - moved.pool.filter((slot) => slot.path).length);
assert.ok(moved.pool.every((slot) => !slot.path.includes('\0')), 'no record is left');

// Short counts, in a pool of their own. A header written in short pieces is written whole.
const spare = Array.from({ length: 6 }, () => new Handle());
let short = open(spare);
put(short.fs, '/s/session.json', 'old');
put(short.fs, '/s/session.json.tmp', 'new');
let source = spare[slotOf(short.pool, '/s/session.json.tmp')];
source.short = 12;
short.fs.renameSync('/s/session.json.tmp', '/s/session.json');
source.short = Infinity;
short = open(spare);
assert.equal(short.fs.readFileSync('/s/session.json'), 'new', 'a header taken in short pieces is whole');
assert.equal(short.fs.existsSync('/s/session.json.tmp'), false);

// A header write that stops making progress, or loses power after a short piece: the
// rename fails, and the file it would have replaced is kept, now and after a reopen
// (the 12 bytes taken are the slot's magic, the same as before).
for (const [stop, why] of [[{ progress: 1 }, /EIO/], [{ headers: 1 }, /power lost/]]) {
  short = open(spare);
  put(short.fs, '/s/session.json.tmp', 'newer');
  source = spare[slotOf(short.pool, '/s/session.json.tmp')];
  Object.assign(source, { short: 12, ...stop });
  assert.throws(() => short.fs.renameSync('/s/session.json.tmp', '/s/session.json'), why);
  Object.assign(source, { short: Infinity, progress: Infinity, headers: Infinity });
  assert.equal(short.fs.readFileSync('/s/session.json'), 'new', 'the old file is kept when the new one cannot take the name');
  assert.equal(short.fs.readFileSync('/s/session.json.tmp'), 'newer');
  const kept = open(spare).fs;
  assert.equal(kept.readFileSync('/s/session.json'), 'new');
  assert.equal(kept.readFileSync('/s/session.json.tmp'), 'newer');
  kept.unlinkSync('/s/session.json.tmp');
}

// The new header written but not flushed: the old slot is not let go of either, and the
// reopen keeps one file under the name.
short = open(spare);
put(short.fs, '/s/session.json.tmp', 'newest');
source = spare[slotOf(short.pool, '/s/session.json.tmp')];
source.flushFails = true;
assert.throws(() => short.fs.renameSync('/s/session.json.tmp', '/s/session.json'), /flush failed/);
source.flushFails = false;
assert.equal(short.fs.readFileSync('/s/session.json'), 'new', 'nothing is let go of after a failed flush');
short = open(spare);
assert.equal(short.pool.filter((slot) => slot.path === '/s/session.json').length, 1);
assert.equal(short.fs.readFileSync('/s/session.json'), 'newest');

// Reads in short pieces are read whole; a header or file that cannot be read fails rather
// than read as an empty slot (to be reused) or a shorter file.
for (const handle of spare) handle.shortRead = 5;
short = open(spare);
assert.equal(short.fs.readFileSync('/s/session.json'), 'newest');
for (const handle of spare) handle.shortRead = Infinity;
const held = spare[slotOf(short.pool, '/s/session.json')];
held.unreadableFrom = HEADER + 2;
assert.throws(() => short.fs.readFileSync('/s/session.json'), /EIO/);
held.unreadableFrom = 0;
assert.throws(() => open(spare), /EIO/, 'an unreadable header fails the open');

// A pool whose open fails lets its handles go, so it opens once the header reads again.
const files = new Map(spare.map((handle, index) => [`slot-${index}`, handle]));
const directory = {
  async *keys() { yield* files.keys(); },
  async getFileHandle(name) {
    const handle = files.get(name);
    return {
      async createSyncAccessHandle() {
        if (handle.open) throw new Error('held');
        handle.open = true;
        return handle;
      },
    };
  },
};
const root = { getDirectoryHandle: async () => directory };
Object.defineProperty(globalThis, 'navigator', { value: { storage: { getDirectory: async () => root } }, configurable: true });
await assert.rejects(openOpfsPool({ slots: spare.length }), /EIO/);
held.unreadableFrom = Infinity;
const reopened = await openOpfsPool({ slots: spare.length });
assert.equal(reopened.readFileSync('/s/session.json'), 'newest');
reopened.close();

// The slots open at once: one held elsewhere fails the open, and every handle opened,
// before it or after, is let go of; once it is free, the pool opens whole.
spare[1].open = true;
await assert.rejects(openOpfsPool({ slots: spare.length }), /EBUSY/);
assert.ok(spare.every((handle, index) => index === 1 || !handle.open), 'no handle is left open');
spare[1].open = false;
const ordered = await openOpfsPool({ slots: spare.length });
assert.equal(ordered.readFileSync('/s/session.json'), 'newest');
assert.ok(spare.every((handle) => handle.open), 'every slot opened');
ordered.close();
assert.ok(spare.every((handle) => !handle.open), 'closing lets every handle go');

// A directory move that fails midway (the process goes on): its record stays for the next
// open to finish. Set aside again, the rest goes under another name, and a store is made
// again under the old one: newer than the record, so no open takes it with the move.
const later = Array.from({ length: 8 }, () => new Handle());
let aside = open(later);
put(aside.fs, '/t/store/db', 'old db');
put(aside.fs, '/t/store/journal', 'old journal');
const stuck = later[slotOf(aside.pool, '/t/store/journal')];
stuck.progress = 0;
assert.throws(() => aside.fs.renameSync('/t/store', '/t/store.abandoned-1'), /EIO/);
stuck.progress = Infinity;
aside.fs.renameSync('/t/store', '/t/store.abandoned-2');
put(aside.fs, '/t/store/db', 'new db');
for (let round = 0; round < 2; round++) {
  aside = open(later);
  assert.ok(aside.fs.existsSync('/t/store/db'), 'the store made again stays');
  assert.equal(aside.fs.readFileSync('/t/store/db'), 'new db');
  assert.equal(aside.fs.readFileSync('/t/store.abandoned-1/db'), 'old db');
  assert.equal(aside.fs.readFileSync('/t/store.abandoned-2/journal'), 'old journal');
  assert.ok(aside.pool.every((slot) => !slot.path.includes('\0')), 'no record is left');
}

// A name renamed to itself is left as it is (as rename(2)), a file or a directory; a
// directory cannot move inside itself.
short = open(spare);
put(short.fs, '/d/x/a', 'a');
put(short.fs, '/d/x/b', 'b');
const free = short.fs.free;
short.fs.renameSync('/s/session.json', '/s/session.json');
short.fs.renameSync('/d/x', '/d/x');
assert.throws(() => short.fs.renameSync('/s/none', '/s/none'), /ENOENT/);
assert.throws(() => short.fs.renameSync('/d/x', '/d/x/y'), /EINVAL/);
for (const view of [short.fs, open(spare).fs]) {
  assert.equal(view.readFileSync('/s/session.json'), 'newest');
  assert.equal(view.readFileSync('/d/x/a'), 'a');
  assert.equal(view.readFileSync('/d/x/b'), 'b');
  assert.equal(view.free, free);
}

console.log('node-opfs-replace passed: a replacement takes the name before the old slot goes, and a directory moves under a record the next open finishes; power lost at any step leaves every file whole under one name; short reads and writes are finished or fail before anything is let go of; a directory move that failed is finished with only the files it began with; a rename to itself changes nothing, and none moves a directory inside itself; slots open at once, all or none');
