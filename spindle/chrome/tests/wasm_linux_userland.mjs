// Spindle's wasm Linux runs programs (chrome/wasm-linux/user.mjs): the kernel boots in
// Node with an initramfs whose /init is chrome/wasm-linux/tests/hello.c built for
// wasm32-unknown-linux-musl, which says hello through /dev/kmsg, starts a copy of itself
// (posix_spawn: clone with its own memory, then exec), runs a thread on its own memory
// and powers the machine off. Times are printed.
//
// usage: node chrome/tests/wasm_linux_userland.mjs VMLINUX_WASM HELLO_WASM [CPUS]
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { cpio } from '../wasm-linux/cpio.mjs';
import { bootMachine, nodePlatform } from '../wasm-linux/machine.mjs';

const [kernelPath, helloPath, cpus = '2'] = process.argv.slice(2);
assert.ok(kernelPath && helloPath, 'usage: node chrome/tests/wasm_linux_userland.mjs VMLINUX_WASM HELLO_WASM [CPUS]');
const initrd = cpio([
  { path: 'dev', dir: true },
  { path: 'dev/console', char: [5, 1] },
  { path: 'dev/kmsg', char: [1, 11], mode: 0o644 },
  { path: 'dev/null', char: [1, 3], mode: 0o666 },
  { path: 'proc', dir: true },
  { path: 'tmp', dir: true, mode: 0o1777 },
  { path: 'init', bytes: readFileSync(helloPath), mode: 0o755 },
]);
let log = '';
const started = Date.now();
let hello = null;
const machine = await bootMachine({
  kernel: readFileSync(kernelPath),
  platform: await nodePlatform(),
  cpus: Number(cpus),
  initrd,
  cmdline: 'loglevel=8 keep_bootcon',
  console: (text) => {
    if (hello === null && text.includes('spindle-hello: init')) hello = Date.now() - started;
    log += text;
    if (process.env.SHOW) process.stdout.write(text);
  },
});
const outcome = await Promise.race([machine.ended, new Promise((done) => setTimeout(() => done({ timeout: true }), 120_000))]);
machine.stop();
const said = log.split('\n').filter((line) => line.includes('spindle-hello:')).map((line) => line.replace(/^.*spindle-hello: /, ''));
const tail = log.split('\n').slice(-15).join('\n');
assert.ok(!outcome.timeout, `the machine neither ended nor powered off in 120 s; its console ends:\n${tail}`);
assert.match(said[1] ?? '', /^child \d+ says hi$/, `the spawned copy ran:\n${said.join('\n')}\n--\n${tail}`);
assert.deepEqual(said.filter((_, index) => index !== 1), [
  'init 1 starts', 'child exited 7', 'thread sees the arg', 'thread returned 7, shared 42', 'done',
], `what /init said:\n${said.join('\n')}\n--\n${tail}`);
assert.ok(outcome.clean && outcome.kernel, `the machine powered off cleanly: ${JSON.stringify(outcome)}`);
console.log(`wasm-linux-userland passed: /init said hello at ${hello} ms, powered off at ${Date.now() - started} ms`);
process.exit(0);
