// The serial line between the browser and the browser Linux's agent (guest/agent.py):
// frames are lines (STX, a kind, an id, base64). A payload over INLINE goes as a file
// on the 9p share instead, in root's /runs, and its frame (kind + '+') names the file,
// which the reader removes: a burst much larger into the serial line wedges it.

export const INLINE = 16 * 1024;
const S_IFREG = 0o100000;
let files = 0;

/** Bytes as base64. */
export function base64(bytes) {
  let binary = '';
  for (let at = 0; at < bytes.length; at += 0x8000) binary += String.fromCharCode(...bytes.subarray(at, at + 0x8000));
  return btoa(binary);
}

export function mkdirp(fs9p, dir) {
  let parent = 0;
  for (const name of dir.split('/').filter(Boolean)) {
    const found = fs9p.Search(parent, name);
    parent = found >= 0 ? found : fs9p.CreateDirectory(name, parent);
  }
  return parent;
}

export function split(path) {
  const at = path.lastIndexOf('/');
  return at < 0 ? ['', path] : [path.slice(0, at), path.slice(at + 1)];
}

/** Removes `path` (a file, or a directory and all in it) from the share and lets its
 * bytes go: v86 frees a removed file's data when the guest closes it, and a file the
 * guest has already closed (or never opened) is not closed again. */
export async function removePath(fs9p, path) {
  const [parent, name] = split(path);
  const parentId = fs9p.SearchPath(`/${parent}`).id;
  if (parentId < 0 || fs9p.Search(parentId, name) < 0) return;
  const entries = [{ parentid: parentId, name }];
  const top = fs9p.Search(parentId, name);
  if (fs9p.IsDirectory(top)) fs9p.GetRecursiveList(top, entries);
  // The deepest first: a directory goes once it is empty.
  for (const entry of entries.reverse()) {
    const id = fs9p.Search(entry.parentid, entry.name);
    if (id < 0) continue;
    const file = !fs9p.IsDirectory(id);
    fs9p.Unlink(entry.parentid, entry.name);
    if (file && fs9p.inodes[id].nlinks === 0) await fs9p.DeleteData(id);
  }
}

/** Sends a frame (`payload`: text); one over INLINE goes as a file on the share. */
export async function sendFrame(vm, kind, id, payload = '') {
  const bytes = new TextEncoder().encode(payload);
  if (bytes.length <= INLINE) {
    vm.serial0_send(`\x02${kind} ${id} ${base64(bytes)}\n`);
    return;
  }
  const fs9p = vm.fs9p;
  const name = `in-${Date.now().toString(36)}-${++files}`;
  const made = await fs9p.CreateBinaryFile(name, mkdirp(fs9p, 'runs'), bytes);
  Object.assign(fs9p.inodes[made], { uid: 0, gid: 0, mode: S_IFREG | 0o600 });
  vm.serial0_send(`\x02${kind}+ ${id} ${base64(new TextEncoder().encode(`runs/${name}`))}\n`);
}

/** The agent's frames, to the listeners in the returned set (kind, id, bytes), in the
 * order they came (one carried in a file is read from the share first). */
export function readFrames(vm) {
  let line = '';
  const listeners = new Set();
  let delivered = Promise.resolve();
  const deliver = (kind, id, bytes) => {
    for (const listener of [...listeners]) {
      try {
        listener(kind, id, bytes);
      } catch (error) {
        console.warn('the browser Linux sent a frame that did not read:', error);
      }
    }
  };
  vm.add_listener('serial0-output-byte', (byte) => {
    if (byte !== 10) {
      line += String.fromCharCode(byte);
      return;
    }
    const at = line.indexOf('\x02');
    line = at >= 0 ? line.slice(at + 1) : '';
    if (!line) return;
    const [kind, id, data] = line.split(' ');
    line = '';
    let bytes;
    try {
      bytes = Uint8Array.from(atob(data ?? ''), (char) => char.charCodeAt(0));
    } catch {
      return; // a line the agent did not write whole (or a command wrote to the console)
    }
    delivered = delivered.then(async () => {
      if (!kind.endsWith('+')) return deliver(kind, id, bytes);
      const path = new TextDecoder().decode(bytes);
      if (!/^runs\/[^/]+$/.test(path)) return;
      try {
        const held = await vm.read_file(`/${path}`);
        await removePath(vm.fs9p, path);
        deliver(kind.slice(0, -1), id, held);
      } catch (error) {
        console.warn(`the browser Linux sent a frame in ${path} that did not read:`, error);
      }
    });
  });
  return listeners;
}
