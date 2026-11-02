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
/** A slot's mark that its headers are checked (SEALED, at SEAL, just before the
 * generation), and each header's check: right after its path, a copy of its generation
 * then a CRC-32 of the header up to there (`NAMED` bytes in all). All of it lies in zeros
 * an older version never reads (it reads the magic, the path and the generation), so a
 * header written here still reads there. A slot keeps its mark in every header written
 * over it, so one cut short (power lost mid-write, or a write that stopped part way) fails
 * its check and is told from a whole one, never read as a name made of two. Path and check
 * share the header's first 512 bytes (for any path under ~470 bytes): a disk that keeps
 * some of a write's sectors and not others still leaves one whole header, old or new. A
 * slot an older version named gets its check, then its mark, before its name changes
 * (`seal`); its others are written as that version would write them until it is free. */
const SEAL = GENERATION - 4;
const SEALED = new TextEncoder().encode('chk1');
const NAMED = 12;
const MAX_PATH = SEAL - MAGIC.length - 2 - NAMED;
/** A rename under way (a file's or a directory's): a slot named MOVE, then the old and
 * new names and, for each file that moves, its slot's index and its name inside the
 * directory (none for a file renamed itself; a path holds no NUL; the names are split over
 * more records if one cannot hold them), written before the first file is renamed and let
 * go of after the last (and after the file a renamed one replaced). A pool opened with one
 * finishes the rename (`adoptSlots`) for the files it began with (older than the record),
 * so a crash midway never leaves a directory split between its two names, a directory
 * made again under the old name since a move failed is not taken with it, and a file
 * whose header was cut short while it was renamed is known by its slot and given its new
 * name: a file is always under one of its two names. */
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
  let generation;
  try {
    generation = adoptSlots(pool);
  } catch (error) {
    // Let the handles go, so opening again is not refused as held elsewhere.
    for (const slot of pool) slot.handle.close();
    throw error;
  }
  return new OpfsFs(pool, dir, generation);
}

/** Reads each slot's header into it (`path`, `generation`), lets go of the older of
 * two slots one replacement left with the same path (a crash between its two steps:
 * the newest generation is the file), and finishes a directory move a crash or a failed
 * write cut short (a file cut short while renamed gets its new name). Another header cut
 * short is let go of if it named nothing yet or was being let go of; one still holding
 * bytes otherwise (damaged, not cut by this package) is kept apart, `lost`: never read,
 * and never reused until `reclaim`. Returns the newest generation given. */
export function adoptSlots(pool) {
  let latest = 0;
  const named = new Map();
  const moves = [];
  const cut = new Map();
  for (const slot of pool) {
    const header = readHeader(slot.handle);
    ({ path: slot.path, generation: slot.generation, sealed: slot.sealed } = header);
    slot.lost = false;
    if (header.cut) {
      cut.set(slot, header.cut);
      continue;
    }
    // Written again whole, so an older version reads the same generation (only those last
    // bytes change: a write cut short there leaves the header as it reads now).
    if (header.settled === false) writePath(slot, slot.path, slot.generation);
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
    const [from, to, ...files] = move.path.slice(MOVE.length).split('\0');
    const listed = new Map();
    for (const file of files) {
      const entry = /^(\d+)(\/.*)?$/s.exec(file);
      if (entry) listed.set(Number(entry[1]), entry[2] ?? '');
    }
    pool.forEach((slot, index) => {
      let inside;
      const header = cut.get(slot);
      if (header) {
        // Cut short while this move renamed it: its bytes are the old name's or the new's.
        inside = listed.get(index);
        if (inside === undefined || !between(header, from + inside, to + inside)) return;
        cut.delete(slot);
      } else {
        if (slot.path !== from && !slot.path.startsWith(`${from}/`)) return;
        // Named there after the move began (it failed, and the directory was made again).
        if (slot.generation > move.generation) return;
        inside = slot.path.slice(from.length);
      }
      latest += 1;
      const path = to + inside;
      if (!slot.sealed && slot.path) seal(slot);
      writePath(slot, path, latest);
      slot.path = path;
      slot.generation = latest;
    });
    release(move);
  }
  // A file a rename finished here replaced another: the newer is the file, as above.
  if (moves.length) {
    named.clear();
    for (const slot of pool) {
      if (!slot.path) continue;
      const other = named.get(slot.path);
      const [kept, gone] = !other || slot.generation > other.generation ? [slot, other] : [other, slot];
      named.set(slot.path, kept);
      if (gone) release(gone);
    }
  }
  // Every rename writes its record first (above), so any other header cut short was a
  // file being let go of (its new header names nothing) or named by an open that had
  // written nothing yet: either way empty now. Else (a slot damaged otherwise) its bytes
  // stay where they are, out of use, until the app reclaims them.
  for (const [slot, header] of cut) {
    const length = header[MAGIC.length] | (header[MAGIC.length + 1] << 8);
    if (length === 0 || slot.handle.getSize() <= HEADER) release(slot);
    else slot.lost = true;
  }
  return latest;
}

/** Whether a header that was cut short holds only bytes of `before`'s header or of
 * `after`'s (their checks, which hold generations not known here, aside): one name being
 * written over the other. */
function between(header, before, after) {
  if ([before, after].some((path) => new TextEncoder().encode(path).length > MAX_PATH)) return false;
  const old = headerOf(before, 0, true);
  const now = headerOf(after, 0, true);
  const checks = [before, after].map((path) => MAGIC.length + 2 + new TextEncoder().encode(path).length);
  const unknown = (at) => checks.some((start) => at >= start && at < start + NAMED);
  for (let at = MAGIC.length; at < SEAL; at += 1) {
    if (header[at] !== old[at] && header[at] !== now[at] && !unknown(at)) return false;
  }
  return true;
}

/** The records of a rename from `from` to `to`: MOVE, the two names, then its files
 * (`entries`: each slot's index and name inside the directory, none for a file), in as
 * many records as hold them all (`file`: a file's own rename). */
function moveRecords(from, to, entries, file = false) {
  const bytes = (text) => new TextEncoder().encode(text).length;
  const head = `${MOVE}${from}\0${to}`;
  // A file's two names too long for one header (thousands of bytes): renamed without a
  // record, as an older version did (a cut header then reads as damaged, kept apart).
  if (file && bytes(head) + 1 + bytes(entries[0] ?? '') > MAX_PATH) return [];
  const texts = [];
  let text = head;
  let size = bytes(head);
  for (const entry of entries) {
    const more = 1 + bytes(entry);
    if (size + more > MAX_PATH && text !== head) {
      texts.push(text);
      text = head;
      size = bytes(head);
    }
    if (size + more > MAX_PATH) throw failure('ENAMETOOLONG', `${from}: the record of its move`);
    text += `\0${entry}`;
    size += more;
  }
  texts.push(text);
  return texts;
}

/** Empties a slot: no path, no bytes. */
function release(slot) {
  if (!slot.sealed && slot.path) seal(slot);
  writePath(slot, '', 0);
  slot.handle.truncate(HEADER);
  slot.handle.flush();
  slot.path = '';
  slot.lost = false;
}

/** A slot that holds no file and may be given one. */
const vacant = (slot) => !slot.path && !slot.lost;

/** Opens slots `from`..`to`, in order (all or none: on a failure the ones opened are
 * closed). Their handles open at once: one after another, 48 take ~30 ms, together ~4. */
async function openSlots(dir, from, to) {
  const opening = [];
  for (let index = from; index < to; index++) {
    opening.push(dir.getFileHandle(`slot-${index}`, { create: true }).then((file) => file.createSyncAccessHandle()));
  }
  const opened = await Promise.allSettled(opening);
  const failed = opened.find((outcome) => outcome.status === 'rejected');
  if (failed) {
    for (const outcome of opened) if (outcome.status === 'fulfilled') outcome.value.close();
    throw failed.reason;
  }
  return opened.map((outcome) => ({ handle: outcome.value, path: '' }));
}

/** Reads `into` from `at` whole: a sync access handle may read less than asked. The
 * count read: less only where the handle gives nothing more (the file's end). */
function readFully(handle, into, at) {
  let done = 0;
  while (done < into.length) {
    const read = handle.read(into.subarray(done), { at: at + done });
    if (!(read > 0)) break;
    done += read;
  }
  return done;
}

/** Writes `bytes` at `at` whole: a sync access handle may write less than asked (and
 * say so only by its count), so a write that stops making progress fails. */
function writeFully(handle, bytes, at) {
  let done = 0;
  while (done < bytes.length) {
    const written = handle.write(bytes.subarray(done), { at: at + done });
    if (!(written > 0)) throw failure('EIO', 'an OPFS slot could not be written');
    done += written;
  }
}

/** A slot's header: `{path, generation, sealed, settled}` (`sealed`: the slot has its mark;
 * `settled` false: its last bytes were cut short, the rest whole), or `{cut}`, the bytes
 * of one cut short while it was written (nothing is read from it). */
function readHeader(handle) {
  const none = { path: '', generation: 0, sealed: false };
  if (handle.getSize() < HEADER) return none;
  const header = new Uint8Array(HEADER);
  // Not taken for an empty slot, which would be reused and the file in it written over.
  if (readFully(handle, header, 0) < HEADER) throw failure('EIO', 'an OPFS slot header could not be read');
  if (!MAGIC.every((byte, at) => header[at] === byte)) return none;
  const view = new DataView(header.buffer);
  const sealed = SEALED.every((byte, at) => header[SEAL + at] === byte);
  const length = header[MAGIC.length] | (header[MAGIC.length + 1] << 8);
  const end = MAGIC.length + 2 + length;
  const path = end <= GENERATION ? new TextDecoder().decode(header.subarray(MAGIC.length + 2, end)) : null;
  if (sealed) {
    if (length <= MAX_PATH && view.getUint32(end + 8, true) === crc32(header.subarray(0, end + 8))) {
      // Whole up to its check, its last bytes maybe not (a write cut there): the
      // generation an older version reads (GENERATION) may then be the one before.
      const settled = view.getBigUint64(GENERATION, true) === view.getBigUint64(end, true);
      return { path, generation: Number(view.getBigUint64(end, true)), sealed, settled };
    }
    // Not whole: never read (a header an older version wrote over a marked slot, rolled
    // back to and cut short there, is kept apart too: told from one cut here it cannot be).
    return { ...none, sealed, cut: header };
  }
  // An older version's header has no check: one cut short shows as a name running past
  // its end or holding a NUL (no path holds one), or a record of a move that is not its
  // two whole names (that version's records hold no more).
  if (path === null || !older(path)) return { ...none, cut: header };
  return { path, generation: Number(view.getBigUint64(GENERATION, true)), sealed: false };
}

/** Whether `path` can be an older version's name: none, an absolute path holding no NUL,
 * or a record of a move holding its two absolute names only. */
function older(path) {
  if (!path.startsWith(MOVE)) return !path.includes('\0');
  const names = path.slice(MOVE.length).split('\0');
  return names.length === 2 && names.every((name) => name.startsWith('/'));
}

/** The header naming `path` at `generation`: with its check, and the slot's mark
 * (`sealed`), or as an older version wrote it. */
function headerOf(path, generation, sealed) {
  const bytes = new TextEncoder().encode(path);
  if (bytes.length > MAX_PATH) throw failure('ENAMETOOLONG', path);
  const header = new Uint8Array(HEADER);
  header.set(MAGIC);
  header[MAGIC.length] = bytes.length & 0xff;
  header[MAGIC.length + 1] = bytes.length >> 8;
  header.set(bytes, MAGIC.length + 2);
  const view = new DataView(header.buffer);
  view.setBigUint64(GENERATION, BigInt(generation), true);
  if (sealed) {
    checked(header);
    header.set(SEALED, SEAL);
  }
  return header;
}

/** Writes `header`'s check after its path (a copy of its generation, then the CRC-32 of
 * the header up to there); the bytes it covers. */
function checked(header) {
  const view = new DataView(header.buffer, header.byteOffset);
  const end = MAGIC.length + 2 + (header[MAGIC.length] | (header[MAGIC.length + 1] << 8));
  view.setBigUint64(end, view.getBigUint64(GENERATION, true), true);
  view.setUint32(end + 8, crc32(header.subarray(0, end + 8)), true);
  return end;
}

const CRC = Uint32Array.from({ length: 256 }, (_, byte) => {
  let crc = byte;
  for (let bit = 0; bit < 8; bit += 1) crc = crc & 1 ? 0xedb88320 ^ (crc >>> 1) : crc >>> 1;
  return crc;
});
function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) crc = CRC[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  return (crc ^ 0xffffffff) >>> 0;
}

/** Names `slot` `path` at `generation` (whole and flushed, or this throws). Its header is
 * checked if the slot has its mark, or holds no file (then it gets the mark); callers
 * `seal` a file an older version named first (a name too long to seal is written as that
 * version would). */
function writePath(slot, path, generation) {
  const sealed = slot.sealed || !slot.path;
  const header = headerOf(path, generation, sealed);
  const { handle } = slot;
  if (handle.getSize() < HEADER) handle.truncate(HEADER);
  // Whole, or this throws: callers name or let go of a slot in memory only after it.
  writeFully(handle, header, 0);
  handle.flush();
  slot.sealed = sealed;
}

/** Gives a slot an older version named its check, then its mark, before its name
 * changes: each in zeros that version never reads (nothing else changes, so a write cut
 * short leaves a header that reads as it did), the check made durable first, so a mark is
 * never found without one. A header written over it later, cut short, is then told. */
function seal(slot) {
  const header = new Uint8Array(HEADER);
  if (readFully(slot.handle, header, 0) < HEADER) throw failure('EIO', 'an OPFS slot header could not be read');
  const length = header[MAGIC.length] | (header[MAGIC.length + 1] << 8);
  // Its name reaches into where the check goes (no path this package names is so long).
  if (length > MAX_PATH) return;
  const end = checked(header);
  writeFully(slot.handle, header.subarray(end, end + NAMED), end);
  slot.handle.flush();
  writeFully(slot.handle, SEALED, SEAL);
  slot.handle.flush();
  slot.sealed = true;
}

/** node:fs's synchronous calls over a pool of OPFS slots (what the VFS uses). */
export class OpfsFs {
  #pool;
  #dir;
  #closed = false;
  /** Why this pool takes nothing more: a rename failed after its record was written
   * (`renameSync`), so the next open finishes it from disk; null while it works. */
  #stopped = null;
  #stop(why) {
    this.#stopped ??= why;
  }
  /** Throws once the pool has stopped (every call that reads or changes files). */
  #working() {
    if (this.#stopped) {
      throw failure('EIO', `the OPFS pool stopped after a storage error during a rename (${this.#stopped.message ?? this.#stopped}); open it again (restart the engine) to finish that rename`);
    }
  }
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
  get free() { return this.#pool.filter(vacant).length; }

  /** How many slots hold bytes no name leads to (their header found damaged, not cut by
   * this package's own writes): kept, never reused, as `adoptSlots` found them, until
   * `reclaim`. */
  get lost() { return this.#pool.filter((slot) => slot.lost).length; }

  /** The bytes those slots hold. */
  get lostBytes() {
    return this.#pool.reduce((sum, slot) => sum + (slot.lost ? Math.max(0, slot.handle.getSize() - HEADER) : 0), 0);
  }

  /** Lets go of every `lost` slot (its bytes, which no name leads to, go): the bytes freed.
   * The app's to call when the user frees space; nothing reads them meanwhile. */
  reclaim() {
    this.#working();
    let freed = 0;
    for (const slot of this.#pool) {
      if (!slot.lost) continue;
      freed += Math.max(0, slot.handle.getSize() - HEADER);
      release(slot);
    }
    return freed;
  }

  /** Every file the pool holds, with its size in bytes (what it takes on disk, but for
   * its slot's header). */
  files() {
    return this.#pool
      .filter((slot) => slot.path && !slot.path.startsWith(MOVE))
      .map((slot) => ({ path: slot.path, size: Math.max(0, slot.handle.getSize() - HEADER) }));
  }

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
    this.#working();
    const at = this.#find(path);
    if (at >= 0) {
      if (flags.startsWith('wx')) throw failure('EEXIST', path);
      return at;
    }
    // The VFS opens a missing path read-only only to sync a directory (files are
    // checked first), which may hold nothing now that a file in it was deleted.
    if (flags === 'r') return DIRECTORY;
    if (!flags.includes('w') && !flags.includes('a')) throw failure('ENOENT', path);
    const free = this.#pool.findIndex(vacant);
    if (free < 0) throw failure('ENOSPC', 'the OPFS pool is full; open it with more slots');
    const slot = this.#pool[free];
    slot.handle.truncate(HEADER);
    writePath(slot, path, this.#next());
    slot.path = path;
    return free;
  }
  readSync(fd, buffer, offset, length, position) {
    this.#working();
    return this.#slot(fd).handle.read(buffer.subarray(offset, offset + length), { at: HEADER + position });
  }
  writeSync(fd, buffer, offset, length, position) {
    this.#working();
    return this.#slot(fd).handle.write(buffer.subarray(offset, offset + length), { at: HEADER + position });
  }
  fsyncSync(fd) {
    this.#working();
    if (fd === DIRECTORY) return;
    this.#slot(fd).handle.flush();
  }
  ftruncateSync(fd, length) {
    this.#working();
    this.#slot(fd).handle.truncate(HEADER + length);
  }
  fstatSync(fd) {
    this.#working();
    return { size: Math.max(0, this.#slot(fd).handle.getSize() - HEADER) };
  }
  closeSync() {}
  unlinkSync(path) {
    this.#working();
    const at = this.#find(path);
    if (at < 0) throw failure('ENOENT', path);
    release(this.#pool[at]);
  }
  existsSync(path) {
    this.#working();
    return this.#find(path) >= 0 || this.#inside(path);
  }
  /** Removes the file `path`, or with `recursive` everything under it too. */
  rmSync(path, { recursive = false, force = false } = {}) {
    this.#working();
    if (!recursive && this.#inside(path)) throw failure('EISDIR', path);
    const doomed = this.#pool.filter((slot) => slot.path === path || (recursive && slot.path.startsWith(`${path}/`)));
    if (!doomed.length && !force) throw failure('ENOENT', path);
    for (const slot of doomed) this.unlinkSync(slot.path);
  }
  renameSync(from, to) {
    this.#working();
    const moving = this.#pool.filter((slot) => slot.path === from || slot.path.startsWith(`${from}/`));
    if (!moving.length) throw failure('ENOENT', from);
    // As rename(2): a name renamed to itself is left as it is (it would replace itself),
    // and a directory cannot move inside itself (a crash midway would move it again).
    if (from === to) return;
    if (to.startsWith(`${from}/`)) throw failure('EINVAL', `${to} is inside ${from}`);
    // A file replaces a file, as rename(2) does: it takes the name first, under a newer
    // generation, and the old slot is let go of after, so a crash between leaves the
    // new file under the name (the older slot goes when the pool is next opened), never
    // no file. Directories never merge. A rename goes under a record of it (MOVE) naming
    // each moving file's slot, so a crash midway, or a header cut short, is finished when
    // the pool is next opened.
    const replaced = this.#find(to);
    const file = moving.length === 1 && moving[0].path === from;
    if (replaced >= 0 && !file) throw failure('EEXIST', to);
    if (replaced < 0 && this.existsSync(to)) throw failure('EEXIST', to);
    const old = replaced >= 0 ? this.#pool[replaced] : null;
    const texts = moveRecords(from, to, this.#pool.flatMap((slot, index) => (moving.includes(slot) ? [`${index}${slot.path.slice(from.length)}`] : [])), file);
    const records = this.#pool.filter(vacant).slice(0, texts.length);
    if (records.length < texts.length) throw failure('ENOSPC', 'the OPFS pool is full; open it with more slots');
    // Names an older version wrote get their check first (once), so none is cut short unseen.
    for (const slot of [...moving, ...(old ? [old] : [])]) if (!slot.sealed) seal(slot);
    // The records first. One that fails: no file was renamed, so they all go (the next
    // open must not do a rename this one said failed); if even that fails, this pool takes
    // nothing more (`#stop`).
    let written = 0;
    try {
      for (; written < records.length; written += 1) {
        records[written].handle.truncate(HEADER);
        writePath(records[written], texts[written], this.#next());
        records[written].path = texts[written];
      }
    } catch (error) {
      try {
        for (const record of records.slice(0, written + 1)) release(record);
      } catch {
        this.#stop(error);
      }
      throw error;
    }
    // Then the files, and the file a renamed one replaces. One that fails has its record
    // to finish it at the next open, which this process must not disturb meanwhile (its
    // files and its memory of them may now differ): this pool takes nothing more.
    try {
      for (const slot of moving) {
        const path = to + slot.path.slice(from.length);
        writePath(slot, path, this.#next());
        slot.path = path;
      }
      if (old) release(old);
    } catch (error) {
      this.#stop(error);
      throw error;
    }
    // Done: the records go (one left by a failure here is let go of at the next open).
    for (const record of records) {
      try {
        release(record);
      } catch {}
    }
  }
  mkdirSync() {}
  /** The names directly inside `path` (directories are the paths' prefixes). */
  readdirSync(path) {
    this.#working();
    const names = new Set();
    for (const slot of this.#pool) {
      if (slot.path.startsWith(`${path}/`)) names.add(slot.path.slice(path.length + 1).split('/')[0]);
    }
    return [...names];
  }
  readFileSync(path) {
    this.#working();
    const slot = this.#slot(this.#find(path));
    const bytes = new Uint8Array(Math.max(0, slot.handle.getSize() - HEADER));
    if (readFully(slot.handle, bytes, HEADER) < bytes.length) throw failure('EIO', `${path} could not be read`);
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
