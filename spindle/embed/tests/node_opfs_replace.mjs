// The OPFS pool's renames under a crash, in Node over in-memory slots. A file that
// replaces another takes the name first (under a newer generation) and the old slot is
// let go of after, so power lost between the two leaves the new file under the name, and
// reopening the pool keeps it and lets the older slot go: never no file. A directory moves
// under a record of the move, so power lost midway is finished by the next open: never a
// directory split between its two names.
import assert from 'node:assert/strict';
import { OpfsFs, adoptSlots } from '../../clients/js/opfs.mjs';

/** A sync access handle in memory; `fail` makes its next header write lose power, and
 * `headers` is how many header writes it takes before power is lost. */
class Handle {
  bytes = new Uint8Array(0);
  fail = false;
  headers = Infinity;
  getSize() { return this.bytes.length; }
  truncate(size) {
    const next = new Uint8Array(size);
    next.set(this.bytes.subarray(0, Math.min(size, this.bytes.length)));
    this.bytes = next;
  }
  read(into, { at }) {
    const part = this.bytes.subarray(at, at + into.length);
    into.set(part);
    return part.length;
  }
  write(from, { at }) {
    if (at === 0 && (this.fail || this.headers-- <= 0)) throw new Error('power lost');
    if (at + from.length > this.bytes.length) this.truncate(at + from.length);
    this.bytes.set(from, at);
    return from.length;
  }
  flush() {}
  close() {}
}

const handles = Array.from({ length: 12 }, () => new Handle());
const open = () => {
  const pool = handles.map((handle) => ({ handle, path: '' }));
  return { pool, fs: new OpfsFs(pool, null, adoptSlots(pool)) };
};
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

console.log('node-opfs-replace passed: a replacement takes the name before the old slot goes, and a directory moves under a record the next open finishes; power lost at any step leaves every file whole under one name');
