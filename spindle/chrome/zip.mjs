// A thread's files as one .zip to download: entries stored as they are (no compression;
// what threads make is mostly small or compressed already), names in UTF-8. Without
// ZIP64: fewer than 65,535 files and under 4 GiB in all, or it is refused (not wrapped
// into an archive that does not read).

const TABLE = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});

function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) crc = TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  return (crc ^ 0xffffffff) >>> 0;
}

/** Most a .zip without ZIP64 holds: entries, and bytes (sizes and offsets). */
const MOST_ENTRIES = 0xfffe;
const MOST_BYTES = 0xfffffffe;

/** `files` ({path, bytes}) as a .zip (a Blob: its parts are not copied into one array). */
export function zip(files, when = new Date()) {
  if (files.length > MOST_ENTRIES) throw new Error(`a .zip holds at most ${MOST_ENTRIES.toLocaleString('en')} files, and these are ${files.length.toLocaleString('en')}`);
  const time = (when.getHours() << 11) | (when.getMinutes() << 5) | (when.getSeconds() >> 1);
  const date = ((when.getFullYear() - 1980) << 9) | ((when.getMonth() + 1) << 5) | when.getDate();
  const encoder = new TextEncoder();
  const parts = [];
  const central = [];
  let offset = 0;
  for (const { path, bytes } of files) {
    const name = encoder.encode(path);
    if (offset + 30 + name.length + bytes.length > MOST_BYTES) throw new Error('these files come to 4 GiB or more, more than a .zip holds');
    const crc = crc32(bytes);
    // version, flags (UTF-8 names), method (stored), time, date, crc, sizes, name length.
    const head = (signature, extra) => {
      const view = new DataView(new ArrayBuffer(extra ? 46 : 30));
      let at = 0;
      const u16 = (value) => { view.setUint16(at, value, true); at += 2; };
      const u32 = (value) => { view.setUint32(at, value, true); at += 4; };
      u32(signature);
      if (extra) u16(20);
      u16(20); u16(0x0800); u16(0); u16(time); u16(date); u32(crc); u32(bytes.length); u32(bytes.length);
      u16(name.length); u16(0);
      if (extra) { u16(0); u16(0); u16(0); u32(0); u32(offset); }
      return new Uint8Array(view.buffer);
    };
    const local = head(0x04034b50, false);
    central.push(head(0x02014b50, true), name);
    parts.push(local, name, bytes);
    offset += local.length + name.length + bytes.length;
  }
  const size = central.reduce((total, part) => total + part.length, 0);
  if (offset + size > MOST_BYTES) throw new Error('these files come to 4 GiB or more, more than a .zip holds');
  const end = new DataView(new ArrayBuffer(22));
  end.setUint32(0, 0x06054b50, true);
  end.setUint16(8, files.length, true);
  end.setUint16(10, files.length, true);
  end.setUint32(12, size, true);
  end.setUint32(16, offset, true);
  return new Blob([...parts, ...central, new Uint8Array(end.buffer)], { type: 'application/zip' });
}
