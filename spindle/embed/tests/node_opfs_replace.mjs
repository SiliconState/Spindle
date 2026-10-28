// The OPFS pool's file replacement under a crash, in Node over in-memory slots: a file
// that replaces another takes the name first (under a newer generation) and the old slot
// is let go of after, so power lost between the two leaves the new file under the name,
// and reopening the pool keeps it and lets the older slot go. Never no file.
import assert from 'node:assert/strict';
import { OpfsFs, adoptSlots } from '../../clients/js/opfs.mjs';

/** A sync access handle in memory; `fail` makes its next header write lose power. */
class Handle {
  bytes = new Uint8Array(0);
  fail = false;
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
    if (this.fail && at === 0) throw new Error('power lost');
    if (at + from.length > this.bytes.length) this.truncate(at + from.length);
    this.bytes.set(from, at);
    return from.length;
  }
  flush() {}
  close() {}
}

const handles = Array.from({ length: 4 }, () => new Handle());
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

console.log('node-opfs-replace passed: a replacement takes the name before the old slot goes; power lost at either step leaves a whole file under the name');
