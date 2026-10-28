// Spindle's wasm Linux on 9p shares (chrome/wasm-linux/ninep.mjs: v86's 9p server over
// the machine's virtio): busybox is its root (a share), a second share is mounted at
// /workspace, and a script run as init on its console reads a file the host put there,
// writes one back and changes another, which the host then finds on the share; random
// bytes come from the entropy device. Times are
// printed.
//
// usage: node chrome/tests/wasm_linux_shares.mjs VMLINUX_WASM USERLAND_ROOT [CPUS]
import assert from 'node:assert/strict';
import { lstatSync, readFileSync, readdirSync, readlinkSync } from 'node:fs';
import { join, relative } from 'node:path';
import { consoleDevice } from '../wasm-linux/console.mjs';
import { entropyDevice } from '../wasm-linux/entropy.mjs';
import { bootMachine, nodePlatform } from '../wasm-linux/machine.mjs';
import { MOUNT_OPTIONS, filesystem, ninepDevice } from '../wasm-linux/ninep.mjs';

const [kernelPath, rootPath, cpus = '2'] = process.argv.slice(2);
assert.ok(kernelPath && rootPath, 'usage: node chrome/tests/wasm_linux_shares.mjs VMLINUX_WASM USERLAND_ROOT [CPUS]');

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
export PATH=/sbin:/usr/sbin:/bin:/usr/bin
say() { echo "spindle-share: $*"; }
mount -t proc proc /proc
mount -t devtmpfs dev /dev
mount -t tmpfs tmp /tmp
mount -t 9p -o ${MOUNT_OPTIONS} workspace /workspace
say "root $(awk '$2 == "/" { print $3 }' /proc/mounts)"
say "in $(cat /workspace/in.txt)"
echo "made by the guest" > /workspace/out.txt
sed -i s/old/new/ /workspace/change.txt
mkdir /workspace/dir && echo deep > /workspace/dir/deep.txt
sync
say "tmp $(echo t > /tmp/t && cat /tmp/t)"
say "random $(head -c 16 /dev/urandom | wc -c) $(cat /proc/sys/kernel/random/entropy_avail >/dev/null && echo pool)"
say done
poweroff -f
`;
const root = await filesystem([
  ...tree(rootPath),
  { path: 'dev', dir: true },
  { path: 'proc', dir: true },
  { path: 'tmp', dir: true, mode: 0o1777 },
  { path: 'workspace', dir: true },
  { path: 'init', bytes: new TextEncoder().encode(script), mode: 0o755 },
]);
const workspace = await filesystem([
  { path: 'in.txt', bytes: new TextEncoder().encode('from the host\n') },
  { path: 'change.txt', bytes: new TextEncoder().encode('old line\n') },
]);
let out = '';
let boot = '';
const started = Date.now();
const terminal = consoleDevice({
  output: (bytes) => {
    out += new TextDecoder().decode(bytes);
    if (process.env.SHOW) process.stdout.write(new TextDecoder().decode(bytes));
  },
});
const machine = await bootMachine({
  kernel: readFileSync(kernelPath),
  platform: await nodePlatform(),
  cpus: Number(cpus),
  devices: [terminal, ninepDevice(root, 'host9p'), ninepDevice(workspace, 'workspace'), entropyDevice()],
  cmdline: `console=hvc0 root=host9p rootfstype=9p rootflags=${MOUNT_OPTIONS} rw init=/init`,
  console: (text) => { boot += text; },
});
const outcome = await Promise.race([machine.ended, new Promise((done) => setTimeout(() => done({ timeout: true }), 120_000))]);
machine.stop();
const said = out.split('\n').filter((line) => line.includes('spindle-share:')).map((line) => line.replace(/^.*spindle-share: /, '').replace(/\r$/, ''));
const text = async (fs, path) => {
  const found = fs.SearchPath(path);
  if (found.id < 0) return null;
  return new TextDecoder().decode(await fs.Read(found.id, 0, fs.inodes[found.id].size));
};
assert.ok(!outcome.timeout, `the machine did not power off in 120 s:\n${said.join('\n')}\n--\n${out.slice(-2000)}\n--\n${boot.slice(-1500)}`);
assert.deepEqual(said, ['root 9p', 'in from the host', 'tmp t', 'random 16 pool', 'done'], `${said.join('\n')}\n--\n${out.slice(-2000)}`);
assert.equal(await text(workspace, '/out.txt'), 'made by the guest\n');
assert.equal(await text(workspace, '/change.txt'), 'new line\n');
assert.equal(await text(workspace, '/dir/deep.txt'), 'deep\n');
assert.ok(outcome.clean && outcome.kernel, JSON.stringify(outcome));
console.log(`wasm-linux-shares passed: root and workspace over 9p, powered off at ${Date.now() - started} ms`);
process.exit(0);
