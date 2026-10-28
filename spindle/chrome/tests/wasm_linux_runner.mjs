// Spindle's wasm Linux as a runner (chrome/wasm-linux/runner.mjs, the agent of
// chrome/wasm-linux/guest), in Node: commands run with busybox sh in /workspace over a
// copy of their thread's workspace and their writes come back; exit statuses; commands of
// several threads at once; a thread's next command sees its last one's files; a timeout;
// scripts' Spindle calls (the `spindle` command) arrive numbered and get their replies;
// what a command may reach (no other copy, its own processes, no network); and a loop
// that never calls the kernel, which only a restart ends, after which commands run again.
// Times are printed.
//
// usage: node chrome/tests/wasm_linux_runner.mjs VMLINUX_WASM USERLAND_ROOT GUEST_BIN [CPUS]
//   USERLAND_ROOT: build-userland.sh's root/; GUEST_BIN: where guest/'s programs were
//   built (spindle-agent, spindle).
import assert from 'node:assert/strict';
import { lstatSync, readFileSync, readdirSync, readlinkSync } from 'node:fs';
import { join, relative } from 'node:path';
import { objectDigest } from '../linux-workspace.mjs';
import { nodePlatform } from '../wasm-linux/machine.mjs';
import { filesystem } from '../wasm-linux/ninep.mjs';
import { WasmLinux } from '../wasm-linux/runner.mjs';

const usage = 'usage: node chrome/tests/wasm_linux_runner.mjs VMLINUX_WASM USERLAND_ROOT GUEST_BIN [CPUS]';
const [kernelPath, userlandPath, guestPath, cpus = '9'] = process.argv.slice(2);
assert.ok(kernelPath && userlandPath && guestPath, usage);
const guest = new URL('../wasm-linux/guest/', import.meta.url);

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

const have = new Set(tree(userlandPath).map((entry) => entry.path));
const dirs = ['dev', 'proc', 'sys', 'root', 'run', 'etc', 'usr/local', 'usr/local/bin', 'var', 'home', 'workspace']
  .filter((path) => !have.has(path)).map((path) => ({ path, dir: true }));
const root = await filesystem([
  ...tree(userlandPath),
  ...dirs,
  { path: 'tmp', dir: true, mode: 0o1777 },
  { path: 'var/tmp', dir: true, mode: 0o1777 },
  { path: 'home/spindle', dir: true, uid: 1000, gid: 1000 },
  { path: 'runs', dir: true, mode: 0o700 },
  { path: 'sbin/init', bytes: readFileSync(new URL('init', guest)), mode: 0o755 },
  { path: 'etc/passwd', bytes: readFileSync(new URL('passwd', guest)), mode: 0o644 },
  { path: 'etc/group', bytes: readFileSync(new URL('group', guest)), mode: 0o644 },
  { path: 'usr/local/bin/spindle-agent', bytes: readFileSync(join(guestPath, 'spindle-agent')), mode: 0o755 },
  { path: 'usr/local/bin/spindle', bytes: readFileSync(join(guestPath, 'spindle')), mode: 0o755 },
]);

let log = '';
const calls = [];
const linux = new WasmLinux({
  kernel: readFileSync(kernelPath),
  platform: await nodePlatform(),
  root,
  cpus: Number(cpus),
  log: (text) => {
    log += text;
    if (process.env.SHOW) process.stdout.write(text);
  },
  onStatus: (state, detail) => {
    if (process.env.SHOW) console.log(`[runner] ${state} ${detail ?? ''}`);
  },
  // Spindle's side of the bridge: a reply for each script's call.
  onCode: (id, seq, request) => {
    calls.push({ id, request });
    const reply = request.method === 'tool'
      ? { ok: true, data: { tabs: [{ title: 'Example' }] } }
      : { ok: true, data: { echoed: request } };
    setTimeout(() => linux.reply(id, seq, reply), 5);
  },
});

const objects = new Map();
async function file(path, text, executable = false) {
  const bytes = new TextEncoder().encode(text);
  const object = await objectDigest(bytes);
  objects.set(object, bytes);
  return { path, size: bytes.length, executable, object };
}
let next = 0;
async function run(session, command, { files = [], timeoutS = 30 } = {}) {
  const job = { id: ++next, session, command, timeoutS, files, links: [], directories: [], stopped: false, sent: false };
  const started = Date.now();
  const result = await linux.run(job, async (wanted) => new Map(wanted.map((object) => [object, objects.get(object)])));
  return { ...result, ms: Date.now() - started };
}
const fail = (what, result) => `${what}: ${JSON.stringify({ ...result, writes: result.writes?.map((write) => write.path) })}\n--\n${log.slice(-1500)}`;

const booting = Date.now();
await linux.start();
const bootMs = Date.now() - booting;

// A command over its workspace: what it sees, what it writes.
const first = await run('a', 'echo hello; uname -m; id -u; pwd; cat in.txt; echo made > out.txt; mkdir -p sub && echo deep > sub/d.txt; rm gone.txt',
  { files: [await file('in.txt', 'from the host\n'), await file('gone.txt', 'bye\n')] });
assert.equal(first.output, 'hello\nwasm\n1000\n/workspace\nfrom the host\n', fail('the first command', first));
assert.ok(first.success, fail('the first command', first));
assert.deepEqual(first.writes.map((write) => [write.path, new TextDecoder().decode(write.content)]).sort(), [['out.txt', 'made\n'], ['sub/d.txt', 'deep\n']]);
assert.deepEqual(first.deletes, ['gone.txt']);
const quick = await run('a', 'true');
assert.ok(quick.success, fail('a quick command', quick));

// The thread's next command sees what its last one left (kept on the share).
const second = await run('a', 'cat out.txt sub/d.txt; ls', { files: [await file('in.txt', 'from the host\n'), await file('out.txt', 'made\n'), await file('sub/d.txt', 'deep\n')] });
assert.equal(second.output, 'made\ndeep\nin.txt\nout.txt\nsub\n', fail('the second command', second));

// Exit statuses, and output to standard error.
const failing = await run('a', 'echo to-err >&2; exit 3');
assert.equal(failing.success, false);
assert.equal(failing.code, 3);
assert.equal(failing.output, 'to-err\n');

// Several threads' commands at once.
const together = Date.now();
const three = await Promise.all(['x', 'y', 'z'].map((session) => run(session, `sleep 1; echo ${session}`)));
const togetherMs = Date.now() - together;
assert.deepEqual(three.map((result) => result.output), ['x\n', 'y\n', 'z\n']);
assert.ok(togetherMs < 2500, `three 1 s commands at once took ${togetherMs} ms`);

// What a command reaches: not the other copies, its own processes, no network.
const reach = await run('b', 'ls /runs 2>&1 | head -1; echo "pid1 $(cat /proc/1/comm)"; ps | wc -l; hostname; wget -q -T 2 -O - http://1.1.1.1 >/dev/null 2>&1 && echo net || echo no-net; echo $HOME; touch /etc/x 2>&1 | head -1');
const said = reach.output.split('\n');
assert.match(said[0], /Permission denied/, fail('reach', reach));
assert.equal(said[1], 'pid1 spindle-agent', fail('reach', reach));
assert.ok(Number(said[2]) <= 6, fail('its processes', reach));
assert.equal(said[3], 'spindle');
assert.equal(said[4], 'no-net', fail('network', reach));
assert.equal(said[5], '/home/spindle');
assert.match(said[6], /denied|Read-only/, fail('the root', reach));

// Its home is its user's to write (ramfs takes no uid=); a command holding NUL is refused,
// not run as an empty one.
const home = await run('b', 'touch "$HOME/x" && mkdir -p "$HOME/.cache/a" && stat -c %U "$HOME" && echo home-ok');
assert.ok(home.success && home.output === 'spindle\nhome-ok\n', fail('home', home));
const nul = await run('b', 'echo one\u0000echo two');
assert.ok(!nul.success && /NUL/.test(nul.output), fail('NUL', nul));

// Scripts' Spindle calls.
calls.length = 0;
const asking = await run('c', 'spindle tool list_tabs; spindle call describe \'{"name":"x"}\'');
assert.ok(asking.success, fail('Spindle calls', asking));
assert.match(asking.output, /Example/, fail('Spindle calls', asking));
assert.deepEqual(calls.map((call) => [call.request.method, call.request.id]), [['tool', 0], ['describe', undefined]]);
assert.equal(asking.output.includes('"echoed"'), true, fail('call', asking));

// A timeout ends a command that waits (it calls the kernel: SIGKILL takes).
const slow = await run('d', 'sleep 30', { timeoutS: 1 });
assert.match(slow.failure ?? '', /did not finish within 1 s/, fail('a timeout', slow));
assert.ok(slow.ms < 4000, `the timeout took ${slow.ms} ms`);
const after = await run('d', 'echo still');
assert.equal(after.output, 'still\n');

// A Stop ends a command that waits.
const job = { id: ++next, session: 'd', command: 'sleep 30', timeoutS: 30, files: [], links: [], directories: [], stopped: false, sent: false };
const stopping = linux.run(job, async () => new Map());
setTimeout(() => linux.stop(job), 500);
const stoppedResult = await stopping;
assert.equal(stoppedResult.failure, 'the command was stopped', fail('a Stop', stoppedResult));

// A loop that never calls the kernel outlives SIGKILL: a fresh machine takes over and the
// old one ends, while another command, on another CPU, runs to its end meanwhile.
let boots = linux.boots;
const [spin, beside] = await Promise.all([
  run('e', "awk 'BEGIN { while (1) {} }'", { timeoutS: 1 }),
  run('f', 'sleep 0.5; echo beside'),
]);
assert.match(spin.failure ?? '', /did not finish within 1 s/, fail('a runaway', spin));
assert.equal(beside.output, 'beside\n', fail('beside a runaway', beside));
const restarted = Date.now();
const again = await run('e', 'echo back');
const restartMs = Date.now() - restarted;
assert.equal(again.output, 'back\n', fail('after a restart', again));
assert.equal(linux.boots, boots + 1, 'a fresh machine took over from the one with the loop');

// One left computing after its command ended: the command's result stands, its files
// read once its machine ended.
boots = linux.boots;
const left = await run('g', "awk 'BEGIN { while (1) {} }' & echo left > left.txt; echo done");
assert.equal(left.output, 'done\n', fail('left running', left));
assert.deepEqual(left.writes?.map((write) => write.path), ['left.txt'], fail('left running', left));
const afterLeft = await run('g', 'echo fresh');
assert.equal(afterLeft.output, 'fresh\n');
assert.equal(linux.boots, boots + 1, 'a fresh machine took over from the one with what was left');
// What was kept of a thread survives the restart (the share is this side's).
const kept = await run('a', 'cat out.txt', { files: [await file('in.txt', 'from the host\n'), await file('out.txt', 'made\n'), await file('sub/d.txt', 'deep\n')] });
assert.equal(kept.output, 'made\n', fail('kept across a restart', kept));

linux.restart('done');
console.log(`wasm-linux-runner passed: boot ${bootMs} ms, a command ${quick.ms} ms, three 1 s commands at once ${togetherMs} ms, restart + command ${restartMs} ms`);
process.exit(0);
