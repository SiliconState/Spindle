// The owner the crash-cut test kills: creates a store through the Node VFS, reserves
// the probe's attempt (no receipt), says so on stdout and waits to be killed.
import fs from 'node:fs';
import { createRequire } from 'node:module';

const [glue, database, journal] = process.argv.slice(2);
const spindle = createRequire(import.meta.url)(glue);
spindle.installNodeFs(fs, process.platform);
const store = spindle.ProbeStore.openNode(database, journal, true, `pid ${process.pid}`);
const seen = store.reserve();
process.stdout.write(`reserved ${seen}\n`);
setInterval(() => {}, 1 << 30);
