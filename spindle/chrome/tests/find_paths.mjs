// Names each file under DIR that holds any of the STRINGs, a .zst file as it decompresses
// (the Linux images' files are compressed with zstd), and fails when one does: a build
// must name no directory of the machine that made it. The release workflows give theirs.
//
// usage: node chrome/tests/find_paths.mjs DIR STRING...   (Node 22.15 or later: zstd)
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import zlib from 'node:zlib';

const [dir, ...strings] = process.argv.slice(2);
if (!dir) throw new Error('usage: node chrome/tests/find_paths.mjs DIR STRING...');
if (!zlib.zstdDecompressSync) throw new Error(`Node ${process.version} has no zstd (22.15 or later has)`);
const needles = strings.filter(Boolean).map((string) => Buffer.from(string));
let found = 0;
let files = 0;
function walk(at) {
  for (const entry of readdirSync(at, { withFileTypes: true })) {
    const path = join(at, entry.name);
    if (entry.isDirectory()) walk(path);
    else if (entry.isFile()) {
      files += 1;
      const raw = readFileSync(path);
      const bytes = path.endsWith('.zst') ? zlib.zstdDecompressSync(raw) : raw;
      if (needles.some((needle) => bytes.includes(needle))) {
        console.log(path);
        found += 1;
      }
    }
  }
}
walk(dir);
if (found) {
  console.error(`${found} of ${files} files name the build machine's directories`);
  process.exit(1);
}
console.log(`no build machine's directory in ${files} files`);
