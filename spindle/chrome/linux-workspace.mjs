// Threads' workspaces on the browser Linux's 9p share. A command runs over a copy of its
// thread's workspace in a directory the guest has never seen (/runs/N); what the share
// still holds of that thread's last copy makes most of the next one. A file whose
// content and mode a command left as they were keeps its inode, moved into the new
// directory: nothing is copied, and nothing the guest cached of it goes stale (the guest
// caches by inode, and its content is the same). Only files the share does not hold come
// from Spindle. After the command, what changed is told by the inodes the guest wrote
// to, so unchanged files are not read back. Used by linux-worker.mjs (v86) and
// wasm-linux/runner.mjs (the wasm Linux), and their tests.

import { mkdirp, removePath, split } from './linux-frames.mjs';

/** The guest user commands run as (chrome/linux/guest/agent.py); their workspace is its. */
export const USER_ID = 1000;
const S_IFMT = 0o170000;
const S_IFREG = 0o100000;
const S_IFDIR = 0o040000;
const S_IFLNK = 0o120000;
// A native runner's limits on what a command leaves.
const MAX_ENTRIES = 50_000;
const MAX_FILE = 64 * 1024 * 1024;
const MAX_TREE = 2 * 1024 * 1024 * 1024;
/** What the share keeps of threads' last copies between their commands: the most bytes,
 * and threads (the least recently used go first). */
const KEPT_BYTES = 256 * 1024 * 1024;
const KEPT_THREADS = 8;
/** Changed files read back at once (each held twice while its digest is made). */
const READING = 4;

/** The mode a workspace file has in a fresh copy. */
const modeOf = (executable) => S_IFREG | (executable ? 0o755 : 0o644);

/** The digest Spindle keeps `bytes` under (a workspace file's object: store/src/artifact.rs
 * `object_digest`). */
export async function objectDigest(bytes) {
  const framed = new Uint8Array(26 + bytes.length);
  framed.set(new TextEncoder().encode('spindle-object-v1\0'));
  new DataView(framed.buffer).setBigUint64(18, BigInt(bytes.length));
  framed.set(bytes, 26);
  const hash = new Uint8Array(await crypto.subtle.digest('SHA-256', framed));
  return Array.from(hash, (byte) => byte.toString(16).padStart(2, '0')).join('');
}

export class Workspaces {
  /** `fs9p`: the machine's share (v86's). */
  constructor(fs9p) {
    this.fs = fs9p;
    this.made = 0;
    /** Thread -> what the share holds of its last copy: {dir, files: path -> {id, digest,
     * executable, size}, bytes, used}. */
    this.kept = new Map();
    /** Inodes of workspace files (kept or in a running command's copy), and those of
     * them the guest has written to since they were placed or kept. */
    this.watched = new Set();
    this.written = new Set();
    /** Threads let go of while a command of theirs was under way: its copy is not kept. */
    this.forgotten = new Set();
    // Every write or truncation from the guest comes through these.
    for (const name of ['Write', 'ChangeSize']) {
      const original = fs9p[name].bind(fs9p);
      fs9p[name] = (id, ...rest) => {
        if (this.watched.has(id)) this.written.add(id);
        return original(id, ...rest);
      };
    }
  }

  /** How `session`'s next command gets its files ({files: [{path, size, executable,
   * object}], links, directories}): which come from what the share holds (`reuse`: path
   * -> {entry, from}, its path in the kept copy), and the objects to ask Spindle for
   * (`need`). The plan holds the thread's kept copy from then on (nothing else lets it
   * go): `build` uses it, or `abandon` lets it go. */
  plan(session, { files }) {
    const kept = this.kept.get(session);
    this.kept.delete(session);
    const reuse = new Map();
    const need = new Set();
    if (!kept) {
      for (const file of files) need.add(file.object);
      return { session, kept, reuse, need };
    }
    const usable = (entry, file) => entry.digest === file.object && entry.executable === Boolean(file.executable)
      && entry.size === file.size && this.unchanged(entry);
    const taken = new Set();
    const rest = [];
    // The same path first, then any file of the same content (a file moved or copied).
    for (const file of files) {
      const entry = kept.files.get(file.path);
      if (entry && usable(entry, file)) {
        reuse.set(file.path, { entry, from: file.path });
        taken.add(entry.id);
      } else {
        rest.push(file);
      }
    }
    // Usable or not is the same for every file of one content: decided once each.
    const byContent = new Map();
    for (const [path, entry] of kept.files) {
      if (taken.has(entry.id) || !entry.digest || !this.unchanged(entry)) continue;
      const key = `${entry.digest}${entry.executable ? 'x' : ''}`;
      if (!byContent.has(key)) byContent.set(key, []);
      byContent.get(key).push({ path, entry });
    }
    for (const file of rest) {
      const candidates = byContent.get(`${file.object}${file.executable ? 'x' : ''}`) ?? [];
      const found = candidates.at(-1);
      if (!found || found.entry.size !== file.size || taken.has(found.entry.id)) {
        need.add(file.object);
        continue;
      }
      candidates.pop();
      reuse.set(file.path, { entry: found.entry, from: found.path });
      taken.add(found.entry.id);
    }
    return { session, kept, reuse, need };
  }

  /** Whether a kept file's inode is as it was kept: not written to, and with a fresh
   * copy's mode and owner (a guest that cached it then sees what a fresh copy holds). */
  unchanged(entry) {
    const inode = this.fs.inodes[entry.id];
    return Boolean(inode) && !this.written.has(entry.id) && inode.mode === modeOf(entry.executable)
      && inode.uid === USER_ID && inode.gid === USER_ID && inode.nlinks === 1 && inode.size === entry.size;
  }

  /** Makes the command's copy from `plan` and `bytes` (object -> bytes, for its `need`):
   * returns the run ({dir, top, placed, input}). What the share held of the thread's last
   * copy and this one did not take goes. */
  async build(plan, input, bytes) {
    const fs = this.fs;
    // The commands' copies are root's, closed to others; each command's own is its user's
    // (what is made in it takes that owner), and made afresh (nothing left from before).
    const runsId = mkdirp(fs, 'runs');
    Object.assign(fs.inodes[runsId], { uid: 0, gid: 0, mode: S_IFDIR | 0o700 });
    const name = String(++this.made);
    const dir = `runs/${name}`;
    if (fs.Search(runsId, name) >= 0) await this.remove(dir);
    const top = fs.CreateDirectory(name, runsId);
    Object.assign(fs.inodes[top], { uid: USER_ID, gid: USER_ID, mode: S_IFDIR | 0o755 });
    const parents = new Map([['', top]]);
    const parentOf = (path) => {
      if (!parents.has(path)) parents.set(path, mkdirp(fs, `${dir}/${path}`));
      return parents.get(path);
    };
    const keptParents = new Map();
    const keptParentOf = (path) => {
      if (!keptParents.has(path)) keptParents.set(path, fs.SearchPath(`/${plan.kept.dir}${path ? `/${path}` : ''}`).id);
      return keptParents.get(path);
    };
    const placed = new Map();
    try {
      await this.fill(plan, input, bytes, parentOf, keptParentOf, placed);
    } catch (error) {
      // Half made: it goes (what was moved into it from the kept copy goes with it).
      await this.drop({ dir, placed });
      throw error;
    }
    await this.abandon(plan, placed);
    return { session: plan.session, dir, top, placed, input };
  }

  /** Puts the command's files, links and empty directories in its copy (`placed`: path ->
   * each file as placed). */
  async fill(plan, input, bytes, parentOf, keptParentOf, placed) {
    const fs = this.fs;
    for (const path of input.directories) parentOf(path);
    for (const file of input.files) {
      const [parent, base] = split(file.path);
      const at = parentOf(parent);
      const reused = plan.reuse.get(file.path);
      let id;
      if (reused) {
        // Moved as Rename moves a file (unlinked there, linked here, its version on), not
        // through Rename, which works out the full path of where it was every time.
        const [from, fromBase] = split(reused.from);
        id = reused.entry.id;
        fs.unlink_from_dir(keptParentOf(from), fromBase);
        fs.link_under_dir(at, id, base);
        fs.inodes[id].qid.version++;
      } else {
        const content = bytes.get(file.object);
        if (!content) throw new Error(`no bytes came for ${file.path}`);
        id = await fs.CreateBinaryFile(base, at, content);
        fs.inodes[id].mode = modeOf(file.executable);
      }
      this.watched.add(id);
      this.written.delete(id);
      placed.set(file.path, { id, digest: file.object, executable: Boolean(file.executable), size: file.size });
    }
    for (const link of input.links) {
      const [parent, base] = split(link.path);
      fs.CreateSymlink(base, parentOf(parent), link.target);
    }
  }

  /** Lets the kept copy a plan holds go (`taken`: path -> files moved out of it first). */
  async abandon(plan, taken = new Map()) {
    if (!plan.kept) return;
    const moved = new Set([...taken.values()].map((file) => file.id));
    for (const file of plan.kept.files.values()) {
      if (moved.has(file.id)) continue;
      this.watched.delete(file.id);
      this.written.delete(file.id);
    }
    await this.remove(plan.kept.dir);
    plan.kept = null;
  }

  /** What the command changed, against what it was given, as a native runner reports
   * it ({writes, deletes, links, directories}); its copy is then what the share keeps of
   * the thread. Throws (the copy dropped) when what it left is over a runner's limits. */
  async collect(run) {
    const fs = this.fs;
    let found;
    try {
      found = walk(fs, run.top);
    } catch (error) {
      await this.drop(run);
      throw error;
    }
    const given = new Map();
    for (const file of run.input.files) given.set(file.path, { kind: 'file', object: file.object, executable: Boolean(file.executable) });
    for (const link of run.input.links) given.set(link.path, { kind: 'link', target: link.target });
    for (const path of run.input.directories) given.set(path, { kind: 'directory' });
    const result = { writes: [], deletes: [], links: [], directories: [] };
    const files = new Map();
    const changed = [];
    for (const [path, entry] of found) {
      const was = given.get(path);
      if (entry.kind === 'file') {
        const placed = run.placed.get(path);
        const inode = fs.inodes[entry.id];
        if (placed && placed.id === entry.id && !this.written.has(entry.id) && inode.mode === modeOf(placed.executable)) {
          files.set(path, placed);
          continue;
        }
        changed.push([path, entry, was]);
      } else if (entry.kind === 'link') {
        if (!(was?.kind === 'link' && was.target === entry.target)) result.links.push({ path, target: entry.target });
      } else if (was?.kind !== 'directory') {
        result.directories.push(path);
      }
    }
    // Read back a few at a time: a command that made a great deal is not held whole twice.
    const readOne = async ([path, entry, was]) => {
      const size = fs.inodes[entry.id].size;
      const bytes = (await fs.get_data(entry.id, 0, size)) ?? new Uint8Array();
      const digest = await objectDigest(bytes);
      files.set(path, { id: entry.id, digest, executable: entry.executable, size });
      if (!(was?.kind === 'file' && was.object === digest && was.executable === entry.executable)) {
        result.writes.push({ path, content: new Uint8Array(bytes), executable: entry.executable });
      }
    };
    await Promise.all(Array.from({ length: READING }, async () => {
      while (changed.length) await readOne(changed.pop());
    }));
    for (const path of given.keys()) if (!found.has(path)) result.deletes.push(path);
    const left = new Set([...files.values()].map((file) => file.id));
    for (const { id } of run.placed.values()) {
      if (left.has(id)) continue;
      this.watched.delete(id);
      this.written.delete(id);
    }
    this.keep(run.session, run.dir, files);
    return result;
  }

  /** Keeps a command's copy as what the share holds of its thread (unless the thread was
   * let go of meanwhile, or its files alone are more than the share keeps). */
  keep(session, dir, files) {
    let bytes = 0;
    for (const file of files.values()) {
      bytes += file.size;
      this.written.delete(file.id);
      this.watched.add(file.id);
    }
    const kept = { dir, files, bytes, used: Date.now() };
    if (this.forgotten.delete(session) || bytes > KEPT_BYTES) {
      this.abandon({ kept }).catch(() => {});
      return;
    }
    this.kept.set(session, kept);
    // The least recently used go first.
    const total = () => [...this.kept.values()].reduce((sum, kept) => sum + kept.bytes, 0);
    while (this.kept.size > KEPT_THREADS || total() > KEPT_BYTES) {
      const [oldest] = [...this.kept.entries()].sort(([, a], [, b]) => a.used - b.used)[0];
      this.forget(oldest).catch(() => {});
    }
  }

  /** Lets what the share holds of `session` go (a thread deleted, or the least used);
   * `running`: a command of the thread is under way (its copy goes when it ends). */
  async forget(session, running = false) {
    const kept = this.kept.get(session);
    if (running) this.forgotten.add(session);
    if (!kept) return;
    this.kept.delete(session);
    await this.abandon({ kept });
  }

  /** Drops a command's copy (it ran over its limits, stopped, or never ran). */
  async drop(run) {
    for (const { id } of run.placed.values()) {
      this.watched.delete(id);
      this.written.delete(id);
    }
    await this.remove(run.dir);
  }

  /** Removes a copy from the share, its bytes too. */
  async remove(dir) {
    try {
      await removePath(this.fs, dir);
    } catch (error) {
      console.warn(`the browser Linux kept ${dir}: ${error.message}`);
    }
  }
}

/** The files, links and empty directories under the inode `top`, by path relative to
 * it, as a native runner seals a run (a directory holding anything is implied by it;
 * sockets and pipes are no workspace content), within a native runner's limits. */
export function walk(fs, top) {
  const found = new Map();
  let bytes = 0;
  const stack = [[top, '']];
  while (stack.length) {
    const [id, prefix] = stack.pop();
    const names = fs.GetChildren(id);
    // A directory with nothing kept in it (sockets and pipes are not) is an empty one.
    const kept = names.filter((name) => [S_IFDIR, S_IFLNK, S_IFREG].includes(fs.inodes[fs.Search(id, name)].mode & S_IFMT));
    if (!kept.length && prefix) found.set(prefix, { kind: 'directory' });
    for (const name of names) {
      const child = fs.Search(id, name);
      const inode = fs.inodes[child];
      const path = prefix ? `${prefix}/${name}` : name;
      const type = inode.mode & S_IFMT;
      if (type === S_IFDIR) {
        stack.push([child, path]);
      } else if (type === S_IFLNK) {
        found.set(path, { kind: 'link', target: inode.symlink });
      } else if (type === S_IFREG) {
        if (inode.size > MAX_FILE) throw new Error(`${path} is over ${MAX_FILE / 1048576} MiB`);
        bytes += inode.size;
        if (bytes > MAX_TREE) throw new Error('its files come to over 2 GiB');
        found.set(path, { kind: 'file', id: child, executable: Boolean(inode.mode & 0o111) });
      }
      if (found.size > MAX_ENTRIES) throw new Error(`it holds over ${MAX_ENTRIES} files, links and empty directories`);
    }
  }
  return found;
}
