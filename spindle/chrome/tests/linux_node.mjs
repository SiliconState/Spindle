// The browser Linux (chrome/linux/build.sh's output), booted by v86 in Node: the runner
// agent answers, a command runs with bash, Python and Node in /workspace over files put
// on the 9p share, its writes come back, a timeout stops a command, its scripts' Spindle
// calls (the `spindle` command, Python and Node modules) arrive numbered as native code
// mode numbers them and get their replies, a saved state resumes with the browser's
// clock, and the network: a command without it reaches nothing; with it, real websites
// through the gateway (as Chrome's fetch would, here Node's) over HTTP and HTTPS from
// curl, Python, Node, pip and git, never a local address; and through a relay (a WISP
// server, here a small one in this test) any address, with the guest's own TLS. Times
// are printed: boot, a quick command, python3, node, resume. The network checks need
// the internet.
//
// usage: node chrome/tests/linux_node.mjs LINUX_DIST
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { connect as tcpConnect } from 'node:net';
import { resolve } from 'node:path';
import { readFrames, sendFrame } from '../linux-frames.mjs';
import { wireNetwork } from '../linux-net.mjs';
import { Workspaces, objectDigest } from '../linux-workspace.mjs';

const dist = resolve(process.argv[2] ?? 'chrome/linux/dist');
const { V86 } = await import(`${dist}/libv86.mjs`);

function boot(initial, relay = null) {
  return new V86({
    wasm_path: `${dist}/v86.wasm`,
    bios: { url: `${dist}/seabios.bin` },
    vga_bios: { url: `${dist}/vgabios.bin` },
    memory_size: 512 * 1024 * 1024,
    vga_memory_size: 2 * 1024 * 1024,
    autostart: true,
    filesystem: { baseurl: `${dist}/flat/`, basefs: `${dist}/fs.json` },
    net_device: { type: 'virtio', relay_url: relay ?? 'fetch', mtu: 65520 },
    preserve_mac_from_state_image: true,
    ...(initial
      ? { initial_state: { buffer: initial } }
      : {
          bzimage_initrd_from_filesystem: true,
          cmdline: 'rw root=host9p rootfstype=9p rootflags=trans=virtio,cache=loose,msize=262144 modules=virtio_pci,virtio_net tsc=reliable quiet',
        }),
  });
}

/** The agent's frames (the worker's own framing: large payloads go as files). */
const frames = (vm) => readFrames(vm);
const text = (data) => new TextDecoder().decode(data);

const waitFor = (listeners, want) => new Promise((done) => {
  const listener = (kind, id, data) => {
    if (want(kind, id)) {
      listeners.delete(listener);
      done(data);
    }
  };
  listeners.add(listener);
});

function mkdirp(vm, dir) {
  let parent = 0;
  for (const name of dir.split('/').filter(Boolean)) {
    const found = vm.fs9p.Search(parent, name);
    parent = found >= 0 ? found : vm.fs9p.CreateDirectory(name, parent);
    // As the worker does: the copies are root's and closed; each command's is its user's.
    if (name === 'runs') Object.assign(vm.fs9p.inodes[parent], { uid: 0, gid: 0, mode: 0o40700 });
    else if (dir.split('/').filter(Boolean).indexOf(name) === 1 && dir.includes('runs/')) Object.assign(vm.fs9p.inodes[parent], { uid: 1000, gid: 1000, mode: 0o40755 });
  }
}

let next = 0;
async function run(vm, listeners, dir, command, timeout = 60, answer = null, network = null, now = null) {
  const id = String(++next);
  let output = '';
  const collect = (kind, fid, data) => {
    if (kind === 'OUT' && fid === id) output += text(data);
    // Spindle's side of the bridge: a reply for each script request.
    if (kind === 'RPC' && fid === id && answer) {
      const { seq, request } = JSON.parse(text(data));
      Promise.resolve(answer(request, seq)).then((reply) => sendFrame(vm, 'REPLY', id, JSON.stringify({ seq, reply })));
    }
  };
  listeners.add(collect);
  const ended = waitFor(listeners, (kind, fid) => kind === 'END' && fid === id);
  const started = Date.now();
  await sendFrame(vm, 'RUN', id, JSON.stringify({ command, timeout_s: timeout, dir, network, now: (now ?? Date.now()) / 1000 }));
  const end = JSON.parse(text(await ended));
  listeners.delete(collect);
  return { output: output.trim(), ...end, ms: Date.now() - started };
}

const times = {};
let started = Date.now();
const vm = boot();
const listeners = frames(vm);
await waitFor(listeners, (kind) => kind === 'READY');
times.boot = Date.now() - started;

mkdirp(vm, '/runs/a');
await vm.create_file('/runs/a/in.txt', new TextEncoder().encode('from the browser\n'));
const first = await run(vm, listeners, 'runs/a', 'pwd; cat in.txt; uname -m; python3 -c "print(6*7)"; node -e "console.log(typeof process)"; echo back > out.txt; rm in.txt');
assert.equal(first.code, 0, first.output);
assert.deepEqual(first.output.split('\n'), ['/workspace', 'from the browser', 'i686', '42', 'object']);
assert.equal(new TextDecoder().decode(await vm.read_file('/runs/a/out.txt')), 'back\n', 'a write comes back');
assert.equal(vm.fs9p.SearchPath('/runs/a/in.txt').id, -1, 'a delete comes back');
times.quick = (await run(vm, listeners, 'runs/a', 'true')).ms;
times.python3 = (await run(vm, listeners, 'runs/a', 'python3 -c pass')).ms;
times.node = (await run(vm, listeners, 'runs/a', 'node -e 0')).ms;
const failing = await run(vm, listeners, 'runs/a', 'echo nope >&2; exit 3');
assert.equal(failing.code, 3);
assert.equal(failing.output, 'nope', 'stderr is in the output');
const slow = await run(vm, listeners, 'runs/a', 'sleep 30 & sleep 30; echo never', 2);
assert.equal(slow.timed_out, true, 'the timeout stops it');
assert.ok(slow.ms < 10_000, `stopped in ${slow.ms} ms`);
assert.equal((await run(vm, listeners, 'runs/a', 'echo still here')).output, 'still here', 'the agent goes on');

// Commands run as `spindle`, with a fresh /tmp and home each; nothing they leave runs on,
// whatever group it puts itself in; a job holding the output does not hold the command.
const who = await run(vm, listeners, 'runs/a', 'id -un; echo $HOME; touch /tmp/left ~/left /var/tmp/left; python3 -c "import ctypes; ctypes.CDLL(None).shmget(0x5e1d, 4096, 0o1600)"; setsid sleep 300 </dev/null >/dev/null 2>&1 & (sleep 300 &); echo started');
assert.equal(who.code, 0, who.output);
assert.deepEqual(who.output.split('\n'), ['spindle', '/home/spindle', 'started']);
assert.ok(who.ms < 5_000, `a job holding the output does not hold the command: ${who.ms} ms`);
const after = await run(vm, listeners, 'runs/a', [
  'ls /tmp/left ~/left /var/tmp/left 2>/dev/null | wc -l',
  'ps -o user,args | grep -c "[s]leep 300"',
  'tail -n +2 /proc/sysvipc/shm | wc -l',
  'touch /usr/bin/x 2>/dev/null || echo system-files',
  'mv /opt /opt-moved 2>/dev/null || echo top-level',
  'touch /run/x 2>/dev/null || echo run',
  'ls /runs 2>/dev/null || echo runs',
].join('; '));
assert.deepEqual(after.output.split('\n').map((line) => line.trim()), ['0', '0', '0', 'system-files', 'top-level', 'run', 'runs'],
  'fresh /tmp, /var/tmp and home, nothing left running or in shared memory; the system, the top level, /run and other commands\' copies closed to it');

// Scripts' calls: numbered by the agent in the order they come (children only), replied.
const asked = [];
const bridged = await run(vm, listeners, 'runs/a', [
  'spindle limits',
  'python3 -c "import spindle; print(spindle.model(\'hi\')[\'text\'])"',
  "node -e \"console.log(require('spindle').tool('lookup', {q: 'x'}))\"",
  'spindle search word',
  'python3 -c "import spindle; print([r[\'data\'][\'text\'] for r in spindle.batch([(\'model\', {\'prompt\': \'a\'}), (\'model\', {\'prompt\': \'b\'})])])"',
].join(' && '), 60, (request) => {
  asked.push(request);
  if (request.method === 'model') return { ok: true, data: { text: `answer to ${request.input.prompt}` } };
  if (request.method === 'tool') return { ok: true, data: `looked up ${request.input.arguments.q}` };
  if (request.method === 'batch') return { ok: true, data: request.input.calls.map((call) => ({ ok: true, data: { text: `batched ${call.input.prompt}` } })) };
  return { ok: true, data: { method: request.method } };
});
assert.equal(bridged.code, 0, bridged.output);
assert.deepEqual(bridged.output.split('\n'), [
  JSON.stringify({ method: 'limits' }, null, 2).split('\n'), 'answer to hi', 'looked up x',
  JSON.stringify({ method: 'search' }, null, 2).split('\n'), "['batched a', 'batched b']",
].flat());
assert.deepEqual(asked.map((request) => [request.method, request.id ?? null]),
  [['limits', null], ['model', 0], ['tool', 1], ['search', null], ['batch', 2]], 'children numbered in order; a batch takes one per call');

// TypeScript runs (Node strips the types) and imports the module as ESM; `spindle http`
// prints a site's body (JSON pretty) and fails on HTTP 400 and up.
const typed = await run(vm, listeners, 'runs/a', [
  "printf 'import { limits } from \"spindle\";\\nconst n: number = 6 * 7;\\nconsole.log(\"ts\", n, typeof limits);\\n' > t.ts",
  'node t.ts 2>&1',
  'spindle http -X POST --json \'{"a": 1}\' https://notes.example/api/items',
  'spindle http https://notes.example/api/missing; echo "missing $?"',
  "printf '\\211PNG\\r\\n\\032\\n0000' > dot.png && spindle model --image dot.png 'what is it?'",
].join(' && '), 60, (request) => {
  if (request.method === 'model') return { ok: true, data: { text: `${request.input.images?.length ?? 0} image ${request.input.images?.[0]?.media_type}` } };
  if (request.method !== 'http') return { ok: false, error: `unexpected ${request.method}` };
  if (request.input.url.endsWith('/missing')) return { ok: true, data: { status: 404, body: 'not here' } };
  return { ok: true, data: { status: 200, json: { method: request.input.method, sent: request.input.json } } };
});
assert.equal(typed.code, 0, typed.output);
assert.deepEqual(typed.output.split('\n'), [
  'ts 42 function',
  JSON.stringify({ method: 'POST', sent: { a: 1 } }, null, 2).split('\n'),
  'not here', 'spindle http: HTTP 404', 'missing 1',
  '1 image image/png',
].flat(), 'no warning from Node; the body as it came, the status on stderr; --image shows a model a picture');

// Calls from several processes wait their turn (one with Spindle at a time, as a native
// script makes them); text crosses as UTF-8 both ways.
let outstanding = 0;
let most = 0;
const turns = await run(vm, listeners, 'runs/a', 'spindle model "un café" & spindle model "naïve ✓" & wait; echo "fin ✓"', 60, async (request) => {
  most = Math.max(most, ++outstanding);
  await new Promise((done) => setTimeout(done, 300));
  outstanding -= 1;
  return { ok: true, data: { text: `→ ${request.input.prompt}` } };
});
assert.equal(turns.code, 0, turns.output);
assert.deepEqual(turns.output.split('\n').sort(), ['fin ✓', '→ naïve ✓', '→ un café'].sort());
assert.equal(most, 1, 'one call with Spindle at a time');

// Large payloads (a page's HTML for a script, a long command) cross as files on the
// share: a burst that size into the serial line wedged the agent.
const page = '<div class="post">naïve ✓ 🚀 a post</div>\n'.repeat(7000);
const heredoc = `cat > long.txt <<'EOF'\n${'line of a long command\n'.repeat(2000)}EOF\nwc -l < long.txt; python3 -c "import spindle; print(len(spindle.tool('page_html', {})['html']))"`;
const large = await run(vm, listeners, 'runs/a', heredoc, 120, () => ({ ok: true, data: { html: page } }));
assert.equal(large.code, 0, large.output);
assert.deepEqual(large.output.split('\n'), ['2000', String([...page].length)], 'a 46 KB command and a 300 KB reply arrive whole (Python counts characters)');
assert.equal((await run(vm, listeners, 'runs/a', 'echo still here')).output, 'still here', 'and the agent goes on');
// The other way: a script's call over 16 KiB goes to the browser as a file too; and at the
// edge, a reply of exactly 16 KiB goes inline and one byte more as a file, both whole.
const sizes = [];
const outward = await run(vm, listeners, 'runs/a', [
  `python3 -c "import spindle; print(spindle.tool('measure', {'text': 'é' * 30000})['n'])"`,
  `python3 -c "import spindle; print(len(spindle.tool('edge', {'extra': 0})['pad']), len(spindle.tool('edge', {'extra': 1})['pad']))"`,
].join(' && '), 120, (request, seq) => {
  if (request.input.name === 'measure') return { ok: true, data: { n: [...request.input.arguments.text].length } };
  // Padded so the REPLY frame's payload is 16 KiB exactly (then one byte more).
  const reply = (pad) => ({ ok: true, data: { pad } });
  const bare = new TextEncoder().encode(JSON.stringify({ seq, reply: reply('') })).length;
  const pad = 'p'.repeat(16 * 1024 - bare + request.input.arguments.extra);
  sizes.push(new TextEncoder().encode(JSON.stringify({ seq, reply: reply(pad) })).length);
  return reply(pad);
});
assert.equal(outward.code, 0, outward.output);
assert.deepEqual(sizes, [16384, 16385], 'the replies sit on either side of the edge');
const [measured, edges] = outward.output.split('\n');
assert.equal(measured, '30000', 'a 60 KB call arrives whole');
const [inline, filed] = edges.split(' ').map(Number);
assert.ok(inline > 16_000 && filed === inline + 1, `both edge replies arrive whole: ${edges}`);
assert.equal(vm.fs9p.GetChildren(vm.fs9p.SearchPath('/runs').id).filter((name) => /^(in|out)-/.test(name)).length, 0, 'no frame file is left on the share, either way');

// The spindle command is a program, not an interpreter starting: a call from bash or
// Node costs about the bridge's round trip.
const calls = await run(vm, listeners, 'runs/a', [
  's=$(date +%s%N); for i in 1 2 3 4 5; do spindle limits > /dev/null; done; e=$(date +%s%N); echo $(( (e - s) / 5000000 ))',
  'node -e "const s = require(\'spindle\'); s.limits(); const t = process.hrtime.bigint(); for (let i = 0; i < 5; i++) s.limits(); console.log(Number((process.hrtime.bigint() - t) / 5000000n))"',
].join(' && '), 120, () => ({ ok: true, data: { calls: 0 } }));
assert.equal(calls.code, 0, calls.output);
[times.spindle_bash, times.spindle_node] = calls.output.split('\n').map(Number);

// Commands run side by side, each in namespaces of its own: one waiting on its script's
// Spindle call holds up no other; neither sees the other's processes, /tmp or files; a
// stop ends one and not the other; what one leaves running ends with it.
mkdirp(vm, '/runs/c1');
mkdirp(vm, '/runs/c2');
let quickDone = 0;
const [waiting, quick] = await Promise.all([
  run(vm, listeners, 'runs/c1', 'touch /tmp/mine; echo one > one.txt; sleep 30 & spindle model slow; ls /tmp; ps -e -o comm | grep -cx sleep', 60,
    () => new Promise((done) => setTimeout(() => done({ ok: true, data: { text: `answered after ${Date.now() - quickDone} ms` } }), 3000))),
  run(vm, listeners, 'runs/c2', 'echo two > two.txt; ls /tmp | wc -l; ps -e -o comm | grep -cx spindle; ls').then((result) => { quickDone = Date.now(); return result; }),
]);
assert.equal(quick.code, 0, quick.output);
assert.deepEqual(quick.output.split('\n').map((line) => line.trim()), ['0', '0', 'two.txt'], 'its own /tmp, processes and workspace');
assert.ok(quick.ms < waiting.ms - 1000, `a command waiting on Spindle held up no other: ${quick.ms} ms against ${waiting.ms} ms`);
assert.equal(waiting.code, 0, waiting.output);
const [answered, ...seen] = waiting.output.split('\n');
assert.match(answered, /^answered after \d+ ms$/);
assert.deepEqual(seen, ['mine', '1'], 'its own /tmp, and its own background job seen');
assert.ok(vm.fs9p.SearchPath('/runs/c1/one.txt').id >= 0 && vm.fs9p.SearchPath('/runs/c1/two.txt').id < 0, 'each wrote its own copy');
const pair = [run(vm, listeners, 'runs/c1', 'sleep 30; echo never'), run(vm, listeners, 'runs/c2', 'sleep 1; echo kept')];
await new Promise((done) => setTimeout(done, 300));
sendFrame(vm, 'STOP', String(next - 1), '');
const [stoppedOne, keptOne] = await Promise.all(pair);
assert.equal(stoppedOne.stopped, true, 'the stopped one stops');
assert.deepEqual([keptOne.code, keptOne.output], [0, 'kept'], 'the other goes on');
times.side_by_side = (await Promise.all([1, 2, 3, 4].map((n) => run(vm, listeners, `runs/c${(n % 2) + 1}`, 'true')))).map((result) => result.ms);
// A script can neither take the agent down (a number JSON has no float for) nor hold
// up another command by leaving a large reply unread; each command has terminals of its
// own.
const odd = await run(vm, listeners, 'runs/c1', `python3 -c "import socket; s = socket.socket(socket.AF_UNIX); s.connect('/run/spindle.sock'); s.sendall(b'{\\"method\\": \\"tool\\", \\"input\\": {\\"x\\": 1e999}}\\\\n'); print(s.recv(4096).decode().strip())"; ls /dev/pts; python3 -c "import os, pty; m, t = pty.openpty(); print(os.ttyname(t))"`);
assert.equal(odd.code, 0, odd.output);
assert.deepEqual(odd.output.split('\n'), ['{"ok": false, "error": "a request is {\\"method\\": ..., \\"input\\": ...}"}', 'ptmx', '/dev/pts/0'], 'refused, the agent goes on; its own /dev/pts');
const [unread, alongside] = await Promise.all([
  run(vm, listeners, 'runs/c1', `python3 -c "import socket, time; s = socket.socket(socket.AF_UNIX); s.connect('/run/spindle.sock'); s.sendall(b'{\\"method\\": \\"tool\\", \\"input\\": {}}\\\\n'); time.sleep(4); print('left it')"`, 60,
    () => ({ ok: true, data: { html: 'x'.repeat(2 * 1024 * 1024) } })),
  new Promise((done) => setTimeout(done, 1500)).then(() => run(vm, listeners, 'runs/c2', 'echo beside')),
]);
assert.equal(unread.output, 'left it', unread.output);
assert.equal(alongside.output, 'beside');
assert.ok(alongside.ms < 2000, `a reply left unread held up no other command: ${alongside.ms} ms`);


// A thread's workspace kept on the share between its commands (linux-workspace.mjs, as
// the worker uses it): the next command's copy is made of the files left as they were,
// and only the rest comes from Spindle (here a map standing in for its store); what a
// command changed is found by the inodes written to, whatever way it changed them; a
// result Spindle did not take leaves nothing stale behind.
const workspaces = new Workspaces(vm.fs9p);
const store = new Map();
const thread = { files: new Map() };
const put = async (path, text, executable = false) => {
  const bytes = new TextEncoder().encode(text);
  const object = await objectDigest(bytes);
  store.set(object, bytes);
  thread.files.set(path, { object, executable });
};
const inputOf = (state) => ({
  files: [...state.files].map(([path, file]) => ({ path, size: store.get(file.object).length, executable: file.executable, object: file.object })),
  links: [], directories: [],
});
async function inWorkspace(session, state, command, { apply = true } = {}) {
  const input = inputOf(state);
  const plan = workspaces.plan(session, input);
  const started = Date.now();
  const copy = await workspaces.build(plan, input, new Map([...plan.need].map((object) => [object, store.get(object)])));
  const built = Date.now() - started;
  const result = await run(vm, listeners, copy.dir, command);
  const collecting = Date.now();
  const changes = await workspaces.collect(copy);
  const collected = Date.now() - collecting;
  if (apply) {
    for (const write of changes.writes) {
      const object = await objectDigest(write.content);
      store.set(object, write.content);
      state.files.set(write.path, { object, executable: write.executable });
    }
    for (const path of changes.deletes) state.files.delete(path);
  }
  return { result, changes, need: plan.need, built, collected };
}
const names = (changes) => ({ writes: changes.writes.map((write) => write.path).sort(), deletes: [...changes.deletes].sort() });
await put('a.txt', 'alpha\n');
await put('b.txt', 'bravo\n');
await put('d/c.txt', 'charlie\n');
await put('run.sh', 'echo ran\n', true);
const w1 = await inWorkspace('t1', thread, 'cat a.txt d/c.txt; ./run.sh; echo new > n.txt; rm b.txt; printf ALPHA | dd of=a.txt conv=notrunc status=none');
assert.equal(w1.result.code, 0, w1.result.output);
assert.deepEqual(w1.result.output.split('\n'), ['alpha', 'charlie', 'ran']);
assert.equal(w1.need.size, 4, 'a first command takes every file from Spindle');
assert.deepEqual(names(w1.changes), { writes: ['a.txt', 'n.txt'], deletes: ['b.txt'] }, 'a new file, a delete, and an overwrite of the same size in place');
const w2 = await inWorkspace('t1', thread, 'cat a.txt n.txt; chmod +x n.txt; mv d/c.txt d/moved.txt; ln run.sh hard.sh');
assert.equal(w2.result.code, 0, w2.result.output);
assert.deepEqual(w2.result.output.split('\n'), ['ALPHA', 'new'], 'what the last command left, as Spindle took it');
assert.equal(w2.need.size, 0, 'the next command takes nothing from Spindle: every file was kept');
assert.deepEqual(names(w2.changes), { writes: ['d/moved.txt', 'hard.sh', 'n.txt'], deletes: ['d/c.txt'] }, 'a mode changed, a move, a hard link');
assert.ok(w2.changes.writes.find((write) => write.path === 'n.txt').executable, 'the mode came back');
const w3 = await inWorkspace('t1', thread, 'echo changed > a.txt; echo more >> d/moved.txt', { apply: false });
assert.deepEqual(names(w3.changes), { writes: ['a.txt', 'd/moved.txt'], deletes: [] });
assert.equal(w3.need.size, 1, 'only the hard-linked file comes from Spindle again (a file with two names is not reused)');
const w4 = await inWorkspace('t1', thread, 'cat a.txt d/moved.txt; ls -l run.sh | cut -c1-10');
assert.deepEqual(w4.result.output.split('\n'), ['ALPHA', 'charlie', '-rwxr-xr-x'], 'what Spindle did not take is not seen: its files come again, none stale');
assert.equal(w4.need.size, 2, 'the two files the untaken command changed come from Spindle');
// A larger workspace: its first command against the next (a quick one each).
const big = { files: new Map() };
const filler = 'x'.repeat(16 * 1024);
for (let n = 0; n < 2000; n++) {
  const bytes = new TextEncoder().encode(`${n}\n${filler}`);
  const object = await objectDigest(bytes);
  store.set(object, bytes);
  big.files.set(`src/m${n % 40}/f${n}.txt`, { object, executable: false });
}
const cold = await inWorkspace('t2', big, 'ls -R | wc -l');
const warm = await inWorkspace('t2', big, 'ls -R | wc -l; echo one > one.txt');
assert.equal(warm.need.size, 0);
assert.deepEqual(names(warm.changes), { writes: ['one.txt'], deletes: [] });
times.workspace = { cold: [cold.built, cold.result.ms, cold.collected], warm: [warm.built, warm.result.ms, warm.collected] };
await workspaces.forget('t1');
await workspaces.forget('t2');
assert.equal(vm.fs9p.GetChildren(vm.fs9p.SearchPath('/runs').id).filter((name) => /^\d+$/.test(name)).length, 0, 'a forgotten thread leaves nothing on the share');

// The network through Chrome (here Node's fetch, as the worker wires it): only while the
// command's thread has it.
let allowed = false;
const net = wireNetwork(vm, { relay: null, allowed: () => allowed });
const closed = await run(vm, listeners, 'runs/a', 'curl -sS -m 20 https://example.com/ 2>&1 | head -1; curl -sS -m 20 -x http://127.0.0.1:3128 https://example.com/ 2>&1 | head -1');
const [direct, proxied] = closed.output.split('\n');
assert.match(direct, /Could not resolve|Failed to connect|Could not connect/, `no route without the proxy: ${closed.output}`);
assert.match(proxied, /Failed to connect|Could not connect/, `a command without network has a network of its own: not even the proxy: ${closed.output}`);
// Not even while a command with network runs beside it.
allowed = true;
const [beside, alone] = await Promise.all([
  run(vm, listeners, 'runs/c1', "curl -sS -o /dev/null -w 'beside %{http_code}\\n' https://example.com/", 60, null, 'chrome'),
  run(vm, listeners, 'runs/c2', 'curl -sS -m 20 -x http://127.0.0.1:3128 https://example.com/ 2>&1 | head -1'),
]);
assert.equal(beside.output, 'beside 200', beside.output);
assert.match(alone.output, /Failed to connect|Could not connect/, `no network beside one with it: ${alone.output}`);
net.end();
const web = await run(vm, listeners, 'runs/a', [
  "curl -sS https://example.com/ | grep -o '<title>[^<]*'",
  "curl -sS -o /dev/null -w 'http %{http_code}\\n' http://example.com/",
  "python3 -c \"import urllib.request as u; print('python', u.urlopen('https://example.com/').status)\"",
  "node -e \"fetch('https://example.com/').then((r) => console.log('node', r.status))\"",
  'git ls-remote https://github.com/octocat/Hello-World.git HEAD | cut -c41-',
  'curl -sS http://192.168.1.1/',
  'curl -sS http://router/',
  'curl -sS http://nas.lan/',
  'curl -sS -o /dev/null -w "%{redirect_url}\\n" http://github.com/',
].join(' && '), 180, null, 'chrome');
net.end();
assert.equal(web.code, 0, web.output);
assert.deepEqual(web.output.split('\n').map((line) => line.trim()), [
  '<title>Example Domain', 'http 200', 'python 200', 'node 200', 'HEAD',
  'The browser Linux does not reach 192.168.1.1: addresses on this computer or its local network are closed to it.',
  'The browser Linux does not reach router: addresses on this computer or its local network are closed to it.',
  'The browser Linux does not reach nas.lan: addresses on this computer or its local network are closed to it.',
  'https://github.com/',
], 'HTTPS through the guest proxy and plain HTTP, from curl, Python, Node and git; local addresses and names refused; a redirect is the guest\'s to follow');
const pip = await run(vm, listeners, 'runs/a', 'python3 -m venv /tmp/v && /tmp/v/bin/pip install -q six 2>&1 | tail -2; /tmp/v/bin/python -c "import six; print(\'six\', six.__version__)"', 300, null, 'chrome');
net.end();
allowed = false;
assert.match(pip.output, /^six \d/m, `pip installs from PyPI: ${pip.output}`);
times.pip = pip.ms;

// Saved with a clock a month behind: the resumed machine must take the browser's.
const skewed = await run(vm, listeners, 'runs/a', 'date +%s', 30, null, null, Date.now() - 30 * 86400_000);
assert.ok(Math.abs(Number(skewed.output) - (Date.now() / 1000 - 30 * 86400)) < 60, `the clock follows what the browser says: ${skewed.output}`);
const state = await vm.save_state();
times.state_mib = +(state.byteLength / 1048576).toFixed(1);
vm.destroy();
started = Date.now();
const again = boot(state);
await new Promise((done) => again.add_listener('emulator-ready', done));
const resumedListeners = frames(again);
const resumed = await run(again, resumedListeners, 'runs/a', 'cat out.txt; date +%s');
times.resume = Date.now() - started;
const [kept, clock] = resumed.output.split('\n');
assert.equal(kept, 'back');
assert.ok(Math.abs(Number(clock) - Date.now() / 1000) < 10, `the resumed clock is the browser's: ${clock}`);
again.destroy();

// Through a relay: a WISP server (a small one here) carries every connection as it is;
// the guest looks names up over DNS-over-HTTPS and speaks TLS itself.
const relay = await wispServer();
// Name lookups through a relay go over DNS-over-HTTPS (here Node's fetch): counted, to
// see that a command without network makes none.
let lookups = 0;
const realFetch = globalThis.fetch;
globalThis.fetch = (url, init) => {
  if (String(url).includes('dns-query')) lookups += 1;
  return realFetch(url, init);
};
const relayed = boot(state, relay.url);
await new Promise((done) => relayed.add_listener('emulator-ready', done));
const relayedListeners = frames(relayed);
let relayAllowed = false;
wireNetwork(relayed, { relay: relay.url, allowed: () => relayAllowed });
const unrelayed = await run(relayed, relayedListeners, 'runs/a', 'curl -sS -m 20 https://example.com/ 2>&1 | head -1');
assert.match(unrelayed.output, /Failed to connect|Could not connect|Could not resolve|timed out/, `no network, no relay: ${unrelayed.output}`);
assert.equal(relay.seen.length, 0, 'nothing reached the relay');
assert.equal(lookups, 0, 'no name was looked up outside');
relayAllowed = true;
const through = await run(relayed, relayedListeners, 'runs/a', "curl -sS https://example.com/ | grep -o '<title>[^<]*'; echo \"proxy:${https_proxy:-none}\"", 120, null, 'relay');
assert.deepEqual(through.output.split('\n'), ['<title>Example Domain', 'proxy:none'], `through the relay: ${through.output}`);
assert.ok(relay.seen.some((target) => target.endsWith(':443')), `the relay carried the connection: ${relay.seen}`);
assert.ok(lookups > 0, 'with network, names were looked up over DNS-over-HTTPS');
globalThis.fetch = realFetch;
relayed.destroy();
relay.server.close();

console.log(`linux-node passed (commands as spindle with fresh scratch, nothing left running; side by side in namespaces of their own; scripts' Spindle calls bridged, numbered, one at a time, UTF-8; TypeScript; spindle http; network through Chrome and through a relay, local addresses closed; the clock kept): boot ${times.boot} ms, quick command ${times.quick} ms, python3 ${times.python3} ms, node ${times.node} ms, a spindle call from bash ${times.spindle_bash} ms and from Node ${times.spindle_node} ms, pip install ${times.pip} ms, state ${times.state_mib} MiB, resume to a command ${times.resume} ms, four quick commands at once ${times.side_by_side.join('/')} ms, a 2000-file workspace (make/run/read back, ms) first ${times.workspace.cold.join('/')} then ${times.workspace.warm.join('/')}`);
process.exit(0);

/** A WISP (v1) server over a WebSocket, as small as a test needs: TCP streams only. */
function wispServer() {
  const seen = [];
  const server = createServer();
  server.on('upgrade', (request, socket) => {
    const accept = createHash('sha1').update(`${request.headers['sec-websocket-key']}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`).digest('base64');
    socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
    const streams = new Map();
    const frame = (payload) => {
      const length = payload.length;
      const head = length < 126 ? Buffer.from([0x82, length]) : Buffer.alloc(length < 65536 ? 4 : 10);
      if (length >= 126) {
        head[0] = 0x82;
        if (length < 65536) {
          head[1] = 126;
          head.writeUInt16BE(length, 2);
        } else {
          head[1] = 127;
          head.writeBigUInt64BE(BigInt(length), 2);
        }
      }
      socket.write(Buffer.concat([head, payload]));
    };
    const packet = (type, id, body = Buffer.alloc(0)) => {
      const head = Buffer.alloc(5);
      head[0] = type;
      head.writeUInt32LE(id, 1);
      frame(Buffer.concat([head, body]));
    };
    // CONTINUE on stream 0: how much the client may send (all it likes, here).
    const room = Buffer.alloc(4);
    room.writeUInt32LE(0xffffffff);
    packet(3, 0, room);
    let buffer = Buffer.alloc(0);
    socket.on('data', (chunk) => {
      buffer = Buffer.concat([buffer, chunk]);
      for (;;) {
        if (buffer.length < 2) return;
        let length = buffer[1] & 0x7f;
        let at = 2;
        if (length === 126) {
          if (buffer.length < 4) return;
          length = buffer.readUInt16BE(2);
          at = 4;
        } else if (length === 127) {
          if (buffer.length < 10) return;
          length = Number(buffer.readBigUInt64BE(2));
          at = 10;
        }
        const mask = buffer[1] & 0x80 ? buffer.subarray(at, at + 4) : null;
        if (mask) at += 4;
        if (buffer.length < at + length) return;
        const opcode = buffer[0] & 0x0f;
        const payload = Buffer.from(buffer.subarray(at, at + length));
        buffer = buffer.subarray(at + length);
        if (mask) for (let i = 0; i < payload.length; i++) payload[i] ^= mask[i & 3];
        if (opcode === 8) socket.end();
        if (opcode !== 2 || payload.length < 5) continue;
        const type = payload[0];
        const id = payload.readUInt32LE(1);
        const body = payload.subarray(5);
        if (type === 1) {
          const port = body.readUInt16LE(1);
          const host = body.subarray(3).toString();
          seen.push(`${host}:${port}`);
          const tcp = tcpConnect(port, host);
          streams.set(id, tcp);
          tcp.on('data', (data) => packet(2, id, data));
          tcp.on('error', () => {});
          tcp.on('close', () => {
            if (streams.delete(id)) packet(4, id, Buffer.from([2]));
          });
        } else if (type === 2) {
          streams.get(id)?.write(body);
        } else if (type === 4) {
          streams.get(id)?.destroy();
          streams.delete(id);
        }
      }
    });
    socket.on('error', () => {});
    socket.on('close', () => {
      for (const tcp of streams.values()) tcp.destroy();
    });
  });
  return new Promise((done) => server.listen(0, '127.0.0.1', () => done({ server, seen, url: `wisp://127.0.0.1:${server.address().port}/` })));
}
