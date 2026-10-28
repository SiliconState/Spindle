// A 9p share of the wasm Linux (virtio-9p, 9P2000.L): v86's 9p server (./v86) on a v86
// filesystem, driven over this machine's virtio transport instead of v86's PCI device.
// The guest mounts it by its tag with MOUNT_OPTIONS (`mount -t 9p -o MOUNT_OPTIONS TAG
// /path`, or as its root: root=TAG rootfstype=9p rootflags=MOUNT_OPTIONS).
import { Virtio9p } from './v86/lib/9p.js';
import { FS } from './v86/lib/filesystem.js';

/** How the guest mounts a share. Cached, and cached whatever a file's version: the
 * kernel reads and writes a file it does not cache (v86 starts every file at version 0)
 * straight to and from the process's pages over 1 KiB, which this architecture cannot
 * map (EFAULT), so `ignoreqv` keeps every file in the page cache. */
export const MOUNT_OPTIONS = 'trans=virtio,cache=loose,msize=262144,ignoreqv';

const VIRTIO_ID_9P = 9;
const VIRTIO_9P_MOUNT_TAG = 1n;
const REQUESTS = 0;

/** The share of `fs` (a v86 FS) under `tag`. */
export function ninepDevice(fs, tag) {
  const name = new TextEncoder().encode(tag);
  const config = new Uint8Array(2 + name.length);
  new DataView(config.buffer).setUint16(0, name.length, true);
  config.set(name, 2);
  // v86's server, without its emulator: its protocol state, a bus nobody listens on,
  // and a queue whose replies go straight back to the driver.
  const server = Object.create(Virtio9p.prototype);
  Object.assign(server, {
    fs,
    bus: { send() {} },
    configspace_tagname: [...name],
    configspace_taglen: name.length,
    VERSION: '9P2000.L',
    BLOCKSIZE: 8192,
    msize: 8192,
    replybuffer: new Uint8Array(16384),
    replybuffersize: 0,
    fids: [],
    virtqueue: { push_reply: (request) => request.done(), flush_replies() {} },
  });
  return {
    id: VIRTIO_ID_9P,
    features: VIRTIO_9P_MOUNT_TAG,
    config,
    notify(index, queue) {
      if (index !== REQUESTS) return;
      for (const request of queue.take()) {
        server.ReceiveRequest(request).catch((error) => console.warn(`9p ${tag}: a request failed:`, error));
      }
    },
    reset() {
      server.fids = [];
    },
    server,
  };
}

const S_IFDIR = 0o040000;
const S_IFREG = 0o100000;

/** A new filesystem (v86's) holding `entries`, as cpio.mjs takes them: {path, dir} |
 * {path, bytes, mode} | {path, link}, parents before what is in them; each root's, or
 * `uid`/`gid`'s. */
export async function filesystem(entries = []) {
  const fs = new FS();
  const parentOf = (path) => {
    const at = path.lastIndexOf('/');
    if (at < 0) return [0, path];
    const parent = fs.SearchPath(`/${path.slice(0, at)}`).id;
    if (parent < 0) throw new Error(`${path}: its directory comes after it`);
    return [parent, path.slice(at + 1)];
  };
  for (const entry of entries) {
    const path = entry.path.replace(/^\/+|\/+$/g, '');
    const [parent, name] = parentOf(path);
    let id;
    if (entry.dir) {
      id = fs.CreateDirectory(name, parent);
      fs.inodes[id].mode = S_IFDIR | (entry.mode ?? 0o755);
    } else if (entry.link) {
      id = fs.CreateSymlink(name, parent, entry.link);
    } else {
      id = await fs.CreateBinaryFile(name, parent, entry.bytes);
      fs.inodes[id].mode = S_IFREG | (entry.mode ?? 0o644);
    }
    Object.assign(fs.inodes[id], { uid: entry.uid ?? 0, gid: entry.gid ?? 0 });
  }
  return fs;
}
