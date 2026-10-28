// Spindle's wasm Linux runs a shell: the kernel boots in Node with an initramfs of busybox
// (chrome/wasm-linux/build-userland.sh: hush as sh, coreutils and the rest as applets)
// and a script as /init on a virtio console (hvc0, its stdin and stdout), which reads a
// line typed in, runs pipelines, command substitution, awk and sed, files in /tmp, exit
// statuses and /proc, saying each result, then powers the machine off. Times are
// printed.
//
// usage: node chrome/tests/wasm_linux_busybox.mjs VMLINUX_WASM USERLAND_ROOT [CPUS]
import assert from 'node:assert/strict';
import { lstatSync, readFileSync, readdirSync, readlinkSync } from 'node:fs';
import { join, relative } from 'node:path';
import { cpio } from '../wasm-linux/cpio.mjs';
import { consoleDevice } from '../wasm-linux/console.mjs';
import { bootMachine, nodePlatform } from '../wasm-linux/machine.mjs';

const [kernelPath, rootPath, cpus = '2'] = process.argv.slice(2);
assert.ok(kernelPath && rootPath, 'usage: node chrome/tests/wasm_linux_busybox.mjs VMLINUX_WASM USERLAND_ROOT [CPUS]');

/** The tree under `top` as cpio entries (directories first, then their contents). */
function tree(top) {
  const entries = [];
  const visit = (dir) => {
    for (const name of readdirSync(dir).sort()) {
      const path = join(dir, name);
      const at = relative(top, path);
      const stat = lstatSync(path);
      if (stat.isSymbolicLink()) entries.push({ path: at, link: readlinkSync(path) });
      else if (stat.isDirectory()) {
        entries.push({ path: at, dir: true });
        visit(path);
      } else entries.push({ path: at, bytes: readFileSync(path), mode: stat.mode & 0o777 });
    }
  };
  visit(top);
  return entries;
}

const script = `#!/bin/sh
# The kernel starts /init with no PATH.
export PATH=/sbin:/usr/sbin:/bin:/usr/bin
say() { echo "spindle-sh: $*"; }
mount -t proc proc /proc
echo "spindle-sh-ready"
read -r line; say "read $line"

say "uname $(uname -sm)"
say "applets $(ls /bin /sbin /usr/bin /usr/sbin | wc -l)"
say "pipeline $(echo hello world | sed s/world/wasm/ | tr a-z A-Z)"
say "awk $(seq 1 100 | awk '{ s += $1 } END { print s }')"
cd /tmp && echo data > file && say "file $(cat file) $(wc -c < file)"
sh -c 'exit 3'; say "status $?"
say "subshell $( (cd / && pwd) ) $(pwd)"
say "pid1 $(cat /proc/1/comm)"
say "loop $(i=0; while [ $i -lt 5 ]; do i=$((i + 1)); done; echo $i)"
say done
poweroff -f
`;
const initrd = cpio([
  ...tree(rootPath),
  { path: 'dev', dir: true },
  { path: 'dev/console', char: [5, 1] },
  { path: 'dev/kmsg', char: [1, 11], mode: 0o644 },
  { path: 'dev/null', char: [1, 3], mode: 0o666 },
  { path: 'proc', dir: true },
  { path: 'tmp', dir: true, mode: 0o1777 },
  { path: 'init', bytes: new TextEncoder().encode(script), mode: 0o755 },
]);
let log = '';
let out = '';
const started = Date.now();
// Typed once the shell reads its console (a tty drops what comes before it is open).
let typed = false;
const terminal = consoleDevice({
  output: (bytes) => {
    out += new TextDecoder().decode(bytes);
    if (process.env.SHOW) process.stdout.write(new TextDecoder().decode(bytes));
    if (!typed && out.includes('spindle-sh-ready')) {
      typed = true;
      terminal.send('typed in\n');
    }
  },
});
const machine = await bootMachine({
  devices: [terminal],
  kernel: readFileSync(kernelPath),
  platform: await nodePlatform(),
  cpus: Number(cpus),
  initrd,
  cmdline: 'console=hvc0',
  console: (text) => {
    log += text;
    if (process.env.SHOW) process.stdout.write(text);
  },
});
const outcome = await Promise.race([machine.ended, new Promise((done) => setTimeout(() => done({ timeout: true }), 120_000))]);
machine.stop();
const said = out.split('\n').filter((line) => line.includes('spindle-sh:')).map((line) => line.replace(/^.*spindle-sh: /, '').replace(/\r$/, ''));
const tail = log.split('\n').slice(-15).join('\n');
assert.ok(!outcome.timeout, `the machine neither ended nor powered off in 120 s:\n${said.join('\n')}\n--\n${tail}`);
assert.deepEqual(said.slice(0, 2), ['read typed in', 'uname Linux wasm'], `${said.join('\n')}\n--\n${out.slice(-1500)}\n--\n${tail}`);
assert.match(said[2] ?? '', /^applets \d{3}$/);
assert.deepEqual(said.slice(3), [
  'pipeline HELLO WASM', 'awk 5050', 'file data 5', 'status 3', 'subshell / /tmp', 'pid1 init', 'loop 5', 'done',
], `what the shell said:\n${said.join('\n')}\n--\n${tail}`);
assert.ok(outcome.clean && outcome.kernel, `the machine powered off cleanly: ${JSON.stringify(outcome)}`);
console.log(`wasm-linux-busybox passed: ${said.length} results, powered off at ${Date.now() - started} ms`);
process.exit(0);
