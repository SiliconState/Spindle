// The owner the crash-cut test kills: creates a store through the Node VFS, reserves
// the probe's attempt (no receipt), says so on stdout and waits to be killed.
import fs from 'node:fs';
import { createRequire } from 'node:module';

const [glue, database, journal, cut] = process.argv.slice(2);
const spindle = createRequire(import.meta.url)(glue);
const paths = new Map();
const halt = () => {
  process.stdout.write(`cut ${cut}\n`);
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0);
};
const watched = cut ? {
  ...fs,
  openSync(path, flags, mode) {
    const fd = fs.openSync(path, flags, mode);
    paths.set(fd, path);
    if (path.endsWith('-draft') && cut === 'created') halt();
    return fd;
  },
  writeSync(fd, bytes, offset, length, position) {
    if (paths.get(fd)?.endsWith('-draft') && cut === 'partial') {
      fs.writeSync(fd, bytes, offset, 4, position);
      halt();
    }
    return fs.writeSync(fd, bytes, offset, length, position);
  },
  fsyncSync(fd) {
    fs.fsyncSync(fd);
    if (paths.get(fd)?.endsWith('-draft') && cut === 'synced') halt();
  },
  linkSync(from, to) {
    fs.linkSync(from, to);
    if (cut === 'linked') halt();
  },
} : fs;
spindle.installNodeFs(watched, process.platform);
const store = spindle.ProbeStore.openNode(database, journal, true, `pid ${process.pid}`);
const seen = store.reserve();
process.stdout.write(`reserved ${seen}\n`);
setInterval(() => {}, 1 << 30);
