// A flattened device tree (the DTB format, version 17), as the wasm kernel reads its
// machine from one: memory, the command line, CPUs, the initrd, the module's sections
// and, later, its devices.
//
// A node is a plain object: a string value is a string property, a number a 32-bit
// cell, an array of numbers that many cells, a bigint one 64-bit value, a Uint8Array
// raw bytes, and an object a child node (its key is the child's name).

const BEGIN_NODE = 1;
const END_NODE = 2;
const PROP = 3;
const END = 9;

const utf8 = new TextEncoder();

function value(raw) {
  if (typeof raw === 'string') return utf8.encode(`${raw}\0`);
  if (raw instanceof Uint8Array) return raw;
  if (typeof raw === 'bigint') {
    const bytes = new Uint8Array(8);
    new DataView(bytes.buffer).setBigUint64(0, raw);
    return bytes;
  }
  const cells = Array.isArray(raw) ? raw : [raw];
  const bytes = new Uint8Array(cells.length * 4);
  const view = new DataView(bytes.buffer);
  cells.forEach((cell, at) => view.setUint32(at * 4, cell >>> 0));
  return bytes;
}

const isNode = (raw) => raw !== null && typeof raw === 'object' && !Array.isArray(raw) && !(raw instanceof Uint8Array);

/** The DTB of `root`, with `reservations` ({address, size}) in its reservation map. */
export function deviceTree(root, { reservations = [] } = {}) {
  const struct = [];
  const strings = [];
  const offsets = new Map();
  const word = (n) => {
    const bytes = new Uint8Array(4);
    new DataView(bytes.buffer).setUint32(0, n);
    struct.push(bytes);
  };
  const padded = (bytes) => {
    struct.push(bytes);
    const pad = (4 - (bytes.length % 4)) % 4;
    if (pad) struct.push(new Uint8Array(pad));
  };
  const nameOffset = (name) => {
    if (!offsets.has(name)) {
      offsets.set(name, strings.reduce((total, part) => total + part.length, 0));
      strings.push(utf8.encode(`${name}\0`));
    }
    return offsets.get(name);
  };
  const node = (name, props) => {
    word(BEGIN_NODE);
    padded(utf8.encode(`${name}\0`));
    // Properties come before children.
    for (const [key, raw] of Object.entries(props)) {
      if (isNode(raw) || raw === undefined) continue;
      const bytes = value(raw);
      word(PROP);
      word(bytes.length);
      word(nameOffset(key));
      padded(bytes);
    }
    for (const [key, raw] of Object.entries(props)) if (isNode(raw)) node(key, raw);
    word(END_NODE);
  };
  node('', root);
  word(END);

  const join = (parts) => {
    const all = new Uint8Array(parts.reduce((total, part) => total + part.length, 0));
    let at = 0;
    for (const part of parts) {
      all.set(part, at);
      at += part.length;
    }
    return all;
  };
  const structBytes = join(struct);
  const stringBytes = join(strings);
  const reserveBytes = new Uint8Array((reservations.length + 1) * 16);
  const reserve = new DataView(reserveBytes.buffer);
  reservations.forEach(({ address, size }, at) => {
    reserve.setBigUint64(at * 16, BigInt(address));
    reserve.setBigUint64(at * 16 + 8, BigInt(size));
  });

  const HEADER = 40;
  const reserveAt = HEADER;
  const structAt = reserveAt + reserveBytes.length;
  const stringsAt = structAt + structBytes.length;
  const total = stringsAt + stringBytes.length;
  const blob = new Uint8Array(total);
  const header = new DataView(blob.buffer);
  [0xd00dfeed, total, structAt, stringsAt, reserveAt, 17, 16, 0, stringBytes.length, structBytes.length]
    .forEach((field, at) => header.setUint32(at * 4, field));
  blob.set(reserveBytes, reserveAt);
  blob.set(structBytes, structAt);
  blob.set(stringBytes, stringsAt);
  return blob;
}
