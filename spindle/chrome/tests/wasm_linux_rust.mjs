// Rust on Spindle's wasm Linux (chrome/wasm-linux/build-rust.sh: the target
// wasm32-spindle-linux-musl, std built from a patched library): the std check
// (chrome/wasm-linux/tests/rust-std) runs as /init on the console and says what threads,
// condvar timeouts, sleeping, clocks, files, a spawned child with pipes, the environment
// and Unix sockets did; then the layout checks (tests/rust-layout: layout.c built with
// the C toolchain, the crate with Rust's libc) each print their sizes, offsets and
// constants, which must be the same. Times are printed.
//
// usage: node chrome/tests/wasm_linux_rust.mjs VMLINUX_WASM STD_CHECK LAYOUT_C LAYOUT_RUST [CPUS]
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { consoleDevice } from '../wasm-linux/console.mjs';
import { cpio } from '../wasm-linux/cpio.mjs';
import { entropyDevice } from '../wasm-linux/entropy.mjs';
import { bootMachine, nodePlatform } from '../wasm-linux/machine.mjs';

const usage = 'usage: node chrome/tests/wasm_linux_rust.mjs VMLINUX_WASM STD_CHECK LAYOUT_C LAYOUT_RUST [CPUS]';
const [kernelPath, stdPath, layoutCPath, layoutRustPath, cpus = '2'] = process.argv.slice(2);
assert.ok(kernelPath && stdPath && layoutCPath && layoutRustPath, usage);
const kernel = readFileSync(kernelPath);
const platform = await nodePlatform();

/** Boots with `program` as /init; its console output once it powers the machine off. */
async function run(program) {
  const initrd = cpio([
    { path: 'dev', dir: true },
    { path: 'dev/console', char: [5, 1] },
    { path: 'proc', dir: true },
    { path: 'tmp', dir: true, mode: 0o1777 },
    { path: 'init', bytes: readFileSync(program), mode: 0o755 },
  ]);
  let output = '';
  const decoder = new TextDecoder();
  const started = Date.now();
  const machine = await bootMachine({
    kernel,
    platform,
    cpus: Number(cpus),
    initrd,
    cmdline: 'console=hvc0',
    devices: [consoleDevice({ output: (bytes) => { output += decoder.decode(bytes, { stream: true }); } }), entropyDevice()],
  });
  const outcome = await Promise.race([machine.ended, new Promise((done) => setTimeout(() => done({ timeout: true }), 60_000))]);
  machine.stop();
  const tail = output.split('\n').slice(-15).join('\n');
  assert.ok(!outcome.timeout, `${program} neither ended nor powered off in 60 s; its console ends:\n${tail}`);
  assert.ok(outcome.clean && outcome.kernel, `${program} powered the machine off cleanly: ${JSON.stringify(outcome)}\n${tail}`);
  return { output, ms: Date.now() - started };
}

const std = await run(stdPath);
const checks = Object.fromEntries(std.output.split('\n').filter((line) => line.startsWith('check: '))
  .map((line) => { const [, name, value = ''] = line.trim().match(/^check: (\S+) ?(.*)$/); return [name, value]; }));
assert.deepEqual(checks, {
  args: '1',
  threads: '60 6',
  'condvar-timeout': 'true',
  sleep: 'true',
  clock: 'true',
  fs: 'alpha 6 b.txt,sub',
  process: 'child got an-arg and its input 3',
  env: 'set',
  sockets: 'hello 7',
  done: '',
}, `what the std check said:\n${std.output}`);

const layout = (output) => output.split('\n').filter((line) => /^(size|at|value) /.test(line));
const c = await run(layoutCPath);
const rust = await run(layoutRustPath);
const fromC = layout(c.output);
const fromRust = layout(rust.output);
assert.ok(fromC.length > 150, `layout.c printed ${fromC.length} lines:\n${c.output}`);
assert.deepEqual(fromRust, fromC, "Rust's libc and musl agree on every size, offset and constant");
console.log(`wasm-linux-rust passed: std check ${std.ms} ms, ${fromC.length} layouts and constants the same (${c.ms} + ${rust.ms} ms)`);
process.exit(0);
