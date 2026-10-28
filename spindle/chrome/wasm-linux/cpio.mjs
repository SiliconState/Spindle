// An initramfs for the wasm Linux: a cpio archive in the kernel's "newc" form, of
// directories, files and device nodes, as the kernel unpacks it into its first root.

const S_IFDIR = 0o040000;
const S_IFREG = 0o100000;
const S_IFCHR = 0o020000;
const S_IFLNK = 0o120000;

/** The archive of `entries`: {path, dir: true} | {path, bytes, mode} | {path, char:
 * [major, minor]} | {path, link: target}, each with an optional `mode` (permissions). */
export function cpio(entries) {
  const parts = [];
  let size = 0;
  let inode = 1;
  const push = (bytes) => {
    parts.push(bytes);
    size += bytes.length;
    const pad = (4 - (size % 4)) % 4;
    if (pad) {
      parts.push(new Uint8Array(pad));
      size += pad;
    }
  };
  const hex = (value) => (value >>> 0).toString(16).padStart(8, '0');
  const record = (path, mode, data = new Uint8Array(0), [major, minor] = [0, 0]) => {
    const name = new TextEncoder().encode(`${path}\0`);
    const header = `070701${[inode++, mode, 0, 0, 1, 0, data.length, 0, 0, major, minor, name.length, 0].map(hex).join('')}`;
    const head = new TextEncoder().encode(header);
    const joined = new Uint8Array(head.length + name.length);
    joined.set(head);
    joined.set(name, head.length);
    push(joined);
    if (data.length) push(data);
  };
  for (const entry of entries) {
    const path = entry.path.replace(/^\/+/, '');
    if (entry.dir) record(path, S_IFDIR | (entry.mode ?? 0o755));
    else if (entry.char) record(path, S_IFCHR | (entry.mode ?? 0o600), undefined, entry.char);
    else if (entry.link) record(path, S_IFLNK | 0o777, new TextEncoder().encode(entry.link));
    else record(path, S_IFREG | (entry.mode ?? 0o644), entry.bytes);
  }
  record('TRAILER!!!', 0);
  const out = new Uint8Array(size);
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}
