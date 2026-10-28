// Browser storage for the embedded package: sessions in the origin private file
// system (OPFS) of a Dedicated Worker, each owned by an exclusive Web Lock.
//
// OPFS files are written synchronously only through sync access handles, which open
// asynchronously; SQLite needs every file operation synchronous. So a pool of files
// is opened up front (`openOpfsPool`), and each slot's header names the path it
// holds. The pool offers the synchronous calls of node:fs the package's VFS makes
// (openSync, readSync, writeSync, fsyncSync, readdirSync, ...); `flush` makes writes durable. A
// pool is one worker's at a time (a sync access handle is exclusive), and a
// session's Web Lock (`spindle:<dir>`) is taken before any of its files is touched.

const HEADER = 4096;
const MAGIC = new TextEncoder().encode('spindle-opfs-slot-v1');
/** Where a slot's generation is kept: the header's last 8 bytes (zero in older slots).
 * A file that replaces another is named first, under a newer generation, and the old
 * slot let go of after; a crash between leaves two slots with one path, and the newer
 * one is the file (`openOpfsPool` lets the older go). */
const GENERATION = HEADER - 8;
const MAX_PATH = GENERATION - MAGIC.length - 2;
/** A directory moving: a slot named MOVE, then the old and new names (a path holds no
 * NUL), written before its first file moves and let go of after its last. A pool opened
 * with one finishes that move (`adoptSlots`), so a crash midway never leaves a directory
 * split between its two names. */
const MOVE = '\0move\0';
/** The fd `openSync` gives a directory (synced by flushing its slots' headers). */
const DIRECTORY = -2;

function failure(code, message) {
  const error = new Error(`${code}: ${message}`);
  error.code = code;
  return error;
}

/** Opens (creating as needed) the pool files in OPFS directory `directory`: at least
 * `slots`, and every one an earlier `grow` added. Fails if another worker holds the
 * pool. Close it when done (`close`). */
export async function openOpfsPool({ directory = 'spindle', slots = 48 } = {}) {
  const root = await navigator.storage.getDirectory();
  const dir = await root.getDirectoryHandle(directory, { create: true });
  let count = slots;
  for await (const name of dir.keys()) {
    const index = /^slot-(\d+)$/.exec(name);
    if (index) count = Math.max(count, Number(index[1]) + 1);
  }
  let pool;
  try {
    pool = await openSlots(dir, 0, count);
  } catch (error) {
    throw failure('EBUSY', `the OPFS pool is held by another worker or tab (${error.message})`);
  }
  return new OpfsFs(pool, dir, adoptSlots(pool));
}

/** Reads each slot's header into it (`path`, `generation`), lets go of the older of
 * two slots one replacement left with the same path (a crash between its two steps:
 * the newest generation is the file), and finishes a directory move a crash cut short.
 * Returns the newest generation given. */
export function adoptSlots(pool) {
  let latest = 0;
  const named = new Map();
  const moves = [];
  for (const slot of pool) {
    ({ path: slot.path, generation: slot.generation } = readHeader(slot.handle));
    latest = Math.max(latest, slot.generation);
    if (!slot.path) continue;
    if (slot.path.startsWith(MOVE)) {
      moves.push(slot);
      continue;
    }
    const other = named.get(slot.path);
    const [kept, gone] = !other || slot.generation > other.generation ? [slot, other] : [other, slot];
    named.set(slot.path, kept);
    if (gone) release(gone);
  }
  for (const move of moves) {
    const [from, to] = move.path.slice(MOVE.length).split('\0');
    for (const slot of pool) {
      if (slot.path !== from && !slot.path.startsWith(`${from}/`)) continue;
      latest += 1;
      const path = to + slot.path.slice(from.length);
      writePath(slot.handle, path, latest);
      slot.path = path;
      slot.generation = latest;
    }
    release(move);
  }
  return latest;
}

/** Empties a slot: no path, no bytes. */
function release(slot) {
  writePath(slot.handle, '', 0);
  slot.handle.truncate(HEADER);
  slot.handle.flush();
  slot.path = '';
}

/** Opens slots `from`..`to` (all or none: on a failure the ones opened are closed). */
async function openSlots(dir, from, to) {
  const slots = [];
  try {
    for (let index = from; index < to; index++) {
      const file = await dir.getFileHandle(`slot-${index}`, { create: true });
      slots.push({ handle: await file.createSyncAccessHandle(), path: '' });
    }
  } catch (error) {
    for (const slot of slots) slot.handle.close();
    throw error;
  }
  return slots;
}

function readHeader(handle) {
  if (handle.getSize() < HEADER) return { path: '', generation: 0 };
  const header = new Uint8Array(HEADER);
  handle.read(header, { at: 0 });
  if (!MAGIC.every((byte, at) => header[at] === byte)) return { path: '', generation: 0 };
  const length = header[MAGIC.length] | (header[MAGIC.length + 1] << 8);
  const path = new TextDecoder().decode(header.subarray(MAGIC.length + 2, MAGIC.length + 2 + length));
  return { path, generation: Number(new DataView(header.buffer).getBigUint64(GENERATION, true)) };
}

function writePath(handle, path, generation) {
  const bytes = new TextEncoder().encode(path);
  if (bytes.length > MAX_PATH) throw failure('ENAMETOOLONG', path);
  const header = new Uint8Array(HEADER);
  header.set(MAGIC);
  header[MAGIC.length] = bytes.length & 0xff;
  header[MAGIC.length + 1] = bytes.length >> 8;
  header.set(bytes, MAGIC.length + 2);
  new DataView(header.buffer).setBigUint64(GENERATION, BigInt(generation), true);
  if (handle.getSize() < HEADER) handle.truncate(HEADER);
  handle.write(header, { at: 0 });
  handle.flush();
}

/** node:fs's synchronous calls over a pool of OPFS slots (what the VFS uses). */
export class OpfsFs {
  #pool;
  #dir;
  #closed = false;
  #generation;
  constructor(pool, dir, generation = 0) {
    this.#pool = pool;
    this.#dir = dir;
    this.#generation = generation;
  }
  /** The next slot generation (a later name is a newer one). */
  #next() {
    this.#generation += 1;
    return this.#generation;
  }

  /** How many slots hold no file. */
  get free() { return this.#pool.filter((slot) => !slot.path).length; }

  /** Adds `count` empty slots (sync access handles open asynchronously, so call it
   * between operations, before `free` runs out). */
  async grow(count) {
    if (!this.#dir) throw failure('EINVAL', 'this pool cannot grow');
    if (this.#closed) throw failure('EBADF', 'the pool is closed');
    let added;
    try {
      added = await openSlots(this.#dir, this.#pool.length, this.#pool.length + count);
    } catch (error) {
      throw failure('ENOSPC', `the OPFS pool could not grow (${error.message})`);
    }
    this.#pool.push(...added);
  }

  #find(path) { return this.#pool.findIndex((slot) => slot.path === path); }
  #inside(path) { return this.#pool.some((slot) => slot.path.startsWith(`${path}/`)); }
  #slot(fd) {
    const slot = this.#pool[fd];
    if (!slot || !slot.path) throw failure('EBADF', `no open file ${fd}`);
    return slot;
  }

  openSync(path, flags) {
    const at = this.#find(path);
    if (at >= 0) {
      if (flags.startsWith('wx')) throw failure('EEXIST', path);
      return at;
    }
    // The VFS opens a missing path read-only only to sync a directory (files are
    // checked first), which may hold nothing now that a file in it was deleted.
    if (flags === 'r') return DIRECTORY;
    if (!flags.includes('w') && !flags.includes('a')) throw failure('ENOENT', path);
    const free = this.#pool.findIndex((slot) => !slot.path);
    if (free < 0) throw failure('ENOSPC', 'the OPFS pool is full; open it with more slots');
    const slot = this.#pool[free];
    slot.handle.truncate(HEADER);
    writePath(slot.handle, path, this.#next());
    slot.path = path;
    return free;
  }
  readSync(fd, buffer, offset, length, position) {
    return this.#slot(fd).handle.read(buffer.subarray(offset, offset + length), { at: HEADER + position });
  }
  writeSync(fd, buffer, offset, length, position) {
    return this.#slot(fd).handle.write(buffer.subarray(offset, offset + length), { at: HEADER + position });
  }
  fsyncSync(fd) {
    if (fd === DIRECTORY) return;
    this.#slot(fd).handle.flush();
  }
  ftruncateSync(fd, length) { this.#slot(fd).handle.truncate(HEADER + length); }
  fstatSync(fd) { return { size: Math.max(0, this.#slot(fd).handle.getSize() - HEADER) }; }
  closeSync() {}
  unlinkSync(path) {
    const at = this.#find(path);
    if (at < 0) throw failure('ENOENT', path);
    release(this.#pool[at]);
  }
  existsSync(path) { return this.#find(path) >= 0 || this.#inside(path); }
  /** Removes the file `path`, or with `recursive` everything under it too. */
  rmSync(path, { recursive = false, force = false } = {}) {
    if (!recursive && this.#inside(path)) throw failure('EISDIR', path);
    const doomed = this.#pool.filter((slot) => slot.path === path || (recursive && slot.path.startsWith(`${path}/`)));
    if (!doomed.length && !force) throw failure('ENOENT', path);
    for (const slot of doomed) this.unlinkSync(slot.path);
  }
  renameSync(from, to) {
    const moving = this.#pool.filter((slot) => slot.path === from || slot.path.startsWith(`${from}/`));
    if (!moving.length) throw failure('ENOENT', from);
    // A file replaces a file, as rename(2) does: it takes the name first, under a newer
    // generation, and the old slot is let go of after, so a crash between leaves the
    // new file under the name (the older slot goes when the pool is next opened), never
    // no file. Directories never merge. A directory of several files moves under a record
    // of the move (MOVE), so a crash midway is finished when the pool is next opened.
    const replaced = this.#find(to);
    const file = moving.length === 1 && moving[0].path === from;
    if (replaced >= 0 && !file) throw failure('EEXIST', to);
    if (replaced < 0 && this.existsSync(to)) throw failure('EEXIST', to);
    const old = replaced >= 0 ? this.#pool[replaced] : null;
    let record = null;
    if (moving.length > 1) {
      record = this.#pool.find((slot) => !slot.path);
      if (!record) throw failure('ENOSPC', 'the OPFS pool is full; open it with more slots');
      record.handle.truncate(HEADER);
      writePath(record.handle, `${MOVE}${from}\0${to}`, this.#next());
      record.path = `${MOVE}${from}\0${to}`;
    }
    for (const slot of moving) {
      const path = to + slot.path.slice(from.length);
      writePath(slot.handle, path, this.#next());
      slot.path = path;
    }
    if (old) release(old);
    if (record) release(record);
  }
  mkdirSync() {}
  /** The names directly inside `path` (directories are the paths' prefixes). */
  readdirSync(path) {
    const names = new Set();
    for (const slot of this.#pool) {
      if (slot.path.startsWith(`${path}/`)) names.add(slot.path.slice(path.length + 1).split('/')[0]);
    }
    return [...names];
  }
  readFileSync(path) {
    const slot = this.#slot(this.#find(path));
    const bytes = new Uint8Array(Math.max(0, slot.handle.getSize() - HEADER));
    slot.handle.read(bytes, { at: HEADER });
    return new TextDecoder().decode(bytes);
  }
  /** Releases every slot's handle (the pool can then be opened elsewhere). */
  close() {
    for (const slot of this.#pool) slot.handle.close();
    this.#pool = [];
    this.#closed = true;
  }
}

/** Takes `dir`'s exclusive Web Lock if no one holds it: a function that gives it
 * back and resolves once Chrome released it, or null when another worker holds it. */
export function lockSession(dir) {
  return new Promise((resolve, reject) => {
    let release;
    const released = new Promise((done) => { release = done; });
    const holding = navigator.locks.request(`spindle:${dir}`, { mode: 'exclusive', ifAvailable: true }, (lock) => {
      if (!lock) {
        resolve(null);
        return undefined;
      }
      resolve(() => { release(); return holding; });
      return released;
    });
    holding.catch(reject);
  });
}
