// A thread's files as one .zip (chrome/zip.mjs): an archive Python's zipfile reads back
// whole (UTF-8 names, stored entries), made as a Blob of its parts; refused, not wrapped
// into one that does not read, past what a .zip without ZIP64 holds (65,534 files, 4 GiB).
//
// usage: node chrome/tests/zip.mjs
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { zip } from '../zip.mjs';

const files = [
  { path: 'notes/résumé.md', bytes: new TextEncoder().encode('# Hello\n') },
  { path: 'data.bin', bytes: Uint8Array.from({ length: 70_000 }, (_, n) => n % 251) },
  { path: 'empty.txt', bytes: new Uint8Array() },
];
const archive = zip(files);
assert.ok(archive instanceof Blob, 'a Blob of its parts, not one copied array');
const work = mkdtempSync(join(tmpdir(), 'spindle-zip-'));
try {
  const path = join(work, 'files.zip');
  writeFileSync(path, new Uint8Array(await archive.arrayBuffer()));
  const read = JSON.parse(execFileSync('python3', ['-I', '-c', `
import json, sys, zipfile
with zipfile.ZipFile(sys.argv[1]) as z:
    assert z.testzip() is None
    print(json.dumps({info.filename: list(z.read(info)) for info in z.infolist()}))`, path], { encoding: 'utf8' }));
  assert.deepEqual(Object.keys(read), files.map((file) => file.path));
  for (const file of files) assert.deepEqual(read[file.path], [...file.bytes], file.path);
} finally {
  rmSync(work, { recursive: true, force: true });
}

// Past the format's limits: refused before anything is made.
const none = new Uint8Array();
assert.ok(zip(Array.from({ length: 65_534 }, (_, n) => ({ path: `f${n}`, bytes: none }))) instanceof Blob, '65,534 files fit');
assert.throws(() => zip(Array.from({ length: 65_535 }, (_, n) => ({ path: `f${n}`, bytes: none }))), /at most 65,534 files/);
// Sizes only (never read: refused first).
const sized = (length) => ({ length, [Symbol.iterator]: function* () {} });
assert.throws(() => zip([{ path: 'huge', bytes: sized(2 ** 32) }]), /4 GiB/);
assert.throws(() => zip([{ path: 'a', bytes: sized(2 ** 31) }, { path: 'b', bytes: sized(2 ** 31) }]), /4 GiB/, 'offsets past 4 GiB too');

console.log('zip passed: an archive zipfile reads back whole, made as a Blob; refused past 65,534 files or 4 GiB');
