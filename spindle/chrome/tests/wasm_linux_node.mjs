// Spindle's wasm Linux (chrome/wasm-linux), booted in Node: the kernel (vmlinux.wasm,
// built from source) comes up on Spindle's own host runtime, prints to its console,
// starts its CPUs and kernel threads as workers, and, with no programs yet, ends as a
// kernel without an init ends. Times are printed.
//
// usage: node chrome/tests/wasm_linux_node.mjs VMLINUX_WASM [CPUS]
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { bootMachine, nodePlatform } from '../wasm-linux/machine.mjs';

const [kernelPath, cpus = '2'] = process.argv.slice(2);
assert.ok(kernelPath, 'usage: node chrome/tests/wasm_linux_node.mjs VMLINUX_WASM [CPUS]');
let log = '';
const started = Date.now();
let firstLine = null;
let peak = 0;
let machine = null;
machine = await bootMachine({
  kernel: readFileSync(kernelPath),
  platform: await nodePlatform(),
  cpus: Number(cpus),
  cmdline: 'loglevel=8 keep_bootcon',
  console: (text) => {
    firstLine ??= Date.now() - started;
    peak = Math.max(peak, machine?.workers ?? 0);
    log += text;
    if (process.env.SHOW) process.stdout.write(text);
  },
});
const outcome = await Promise.race([machine.ended, new Promise((done) => setTimeout(() => done({ timeout: true }), 120_000))]);
// Counted before the machine lets its workers go.
const workers = peak;
machine.stop();
const tail = log.split('\n').slice(-12).join('\n');
assert.ok(!outcome.timeout, `the machine neither ended nor panicked in 120 s; its console ends:\n${tail}`);
assert.ok(outcome.kernel, `the kernel itself ended the machine, not a failing task: ${outcome.error}`);
assert.match(log, /Linux version 7\./, `the kernel printed its banner:\n${tail}`);
assert.match(log, new RegExp(`Core count: ${cpus}`), 'it read the CPUs from the device tree');
assert.match(log, /No working init found|Run \/init as init process|Kernel panic/, `it looked for an init:\n${tail}`);
console.log(tail);
console.log(`wasm-linux-node passed: banner at ${firstLine} ms, ended ${outcome.clean ? 'clean' : `(${outcome.error})`} at ${Date.now() - started} ms, up to ${workers} workers`);
process.exit(0);
