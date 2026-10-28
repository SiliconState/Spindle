// Python on Spindle's wasm Linux (chrome/wasm-linux/build-python.sh), from the image the
// extension carries (build-image.sh's dist: fs.json and flat/, read here from disk as
// the extension fetches them), run as commands through the runner: the version, its C
// modules (zlib, bz2, sqlite3, hashlib, decimal, json, xml), threads, files in the
// workspace coming back, subprocess (started with posix_spawn: there is no fork), a
// missing program raising as it should, asyncio, a Unix socket pair, and Spindle's calls
// from the `spindle` module. Times are printed: a first command (boot included), a Python
// start, a computation.
//
// usage: node chrome/tests/wasm_linux_python.mjs [DIST]   (default chrome/wasm-linux/dist)
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { objectDigest } from '../linux-workspace.mjs';
import { nodePlatform } from '../wasm-linux/machine.mjs';
import { WasmLinux } from '../wasm-linux/runner.mjs';
import { FS } from '../wasm-linux/v86/lib/filesystem.js';

const dist = resolve(process.argv[2] ?? new URL('../wasm-linux/dist', import.meta.url).pathname);
// The image's files as the extension's storage gives them (by digest, read when opened).
const held = new Map();
const storage = {
  async read(sha256, offset, count) {
    if (!held.has(sha256)) held.set(sha256, new Uint8Array(readFileSync(`${dist}/flat/${sha256}`)));
    return held.get(sha256).subarray(offset, offset + count);
  },
  async cache(sha256, data) {
    held.set(sha256, data);
  },
  uncache(sha256) {
    held.delete(sha256);
  },
};
const root = new FS(storage);
root.load_from_json(JSON.parse(readFileSync(`${dist}/fs.json`, 'utf8')));
let log = '';
const calls = [];
const linux = new WasmLinux({
  kernel: readFileSync(`${dist}/vmlinux.wasm`),
  platform: await nodePlatform(),
  root,
  log: (text) => {
    log += text;
  },
  onCode: (id, seq, request) => {
    calls.push(request);
    setTimeout(() => linux.reply(id, seq, { ok: true, data: { text: `echo: ${request.input?.prompt ?? request.method}` } }), 5);
  },
});

const objects = new Map();
async function file(path, text) {
  const bytes = new TextEncoder().encode(text);
  const object = await objectDigest(bytes);
  objects.set(object, bytes);
  return { path, size: bytes.length, executable: false, object };
}
let next = 0;
async function run(command, files = []) {
  const started = Date.now();
  const result = await linux.run({ id: ++next, session: `p${next}`, command, timeoutS: 60, files, links: [], directories: [], stopped: false, sent: false },
    async (wanted) => new Map(wanted.map((object) => [object, objects.get(object)])));
  return { ...result, ms: Date.now() - started };
}
const show = (result) => `${JSON.stringify({ ...result, writes: result.writes?.map((write) => write.path) })}\n--\n${log.slice(-1200)}`;

const first = await run('python3 -c "import sys; print(sys.version_info[:2])"');
assert.equal(first.output, '(3, 13)\n', show(first));
const start = await run('python3 -c pass');
assert.ok(start.success, show(start));

const script = `
import asyncio, bz2, decimal, hashlib, json, os, socket, sqlite3, subprocess, sys, threading, zlib
import xml.etree.ElementTree as ET
said = []
said.append(len(zlib.compress(b"a" * 1000)) < 50 and bz2.decompress(bz2.compress(b"x")) == b"x")
said.append(hashlib.sha256(b"spindle").hexdigest()[:8])
said.append(str(decimal.Decimal(1) / decimal.Decimal(7))[:8])
said.append(json.loads('{"a": [1, 2]}')["a"][1])
said.append(ET.fromstring("<a><b>x</b></a>").find("b").text)
db = sqlite3.connect("notes.db")
db.execute("create table t (v text)")
db.executemany("insert into t values (?)", [("one",), ("two",)])
db.commit()
said.append(db.execute("select count(*) from t").fetchone()[0])
total = []
threads = [threading.Thread(target=lambda n=n: total.append(sum(range(n * 1000)))) for n in range(4)]
[t.start() for t in threads]
[t.join() for t in threads]
said.append(len(total))
child = subprocess.run(["sh", "-c", "echo child $FOO; pwd; exit 3"], capture_output=True, text=True, env={"FOO": "bar", "PATH": os.environ["PATH"]}, cwd="/tmp")
said.append((child.stdout.split(), child.returncode))
said.append(subprocess.run("echo shell | tr a-z A-Z", shell=True, capture_output=True, text=True).stdout.strip())
try:
    subprocess.run(["no-such-program"])
except FileNotFoundError as error:
    said.append(error.filename)
async def main():
    await asyncio.sleep(0.01)
    return "async"
said.append(asyncio.run(main()))
left, right = socket.socketpair()
left.sendall(b"ping")
said.append(right.recv(4).decode())
with open("out.txt", "w") as out:
    out.write(open("in.txt").read().upper())
print(json.dumps(said))
`;
const checks = await run('python3 check.py', [await file('check.py', script), await file('in.txt', 'from the host\n')]);
assert.ok(checks.success, show(checks));
assert.deepEqual(JSON.parse(checks.output), [
  true, '3daaec9e', '0.142857', 2, 'x', 2, 4, [['child', 'bar', '/tmp'], 3], 'SHELL', 'no-such-program', 'async', 'ping',
], show(checks));
const written = Object.fromEntries(checks.writes.map((write) => [write.path, write.content]));
assert.equal(new TextDecoder().decode(written['out.txt']), 'FROM THE HOST\n');
assert.ok(written['notes.db']?.length > 0, 'the SQLite database came back');

// Spindle's calls from Python.
calls.length = 0;
const asking = await run('python3 -c "import spindle; print(spindle.model(\'hi there\')[\'text\'])"');
assert.ok(asking.success, show(asking));
assert.equal(asking.output, 'echo: hi there\n', show(asking));
assert.equal(calls[0]?.method, 'model');

const compute = await run(`python3 -c "print(sum(i * i for i in range(3_000_000)))"`);
assert.equal(compute.output, '8999995500000500000\n', show(compute));

linux.restart('done');
console.log(`wasm-linux-python passed: a first command ${first.ms} ms (boot included), a Python start ${start.ms} ms, the checks ${checks.ms} ms, 3e6 squares summed ${compute.ms} ms`);
process.exit(0);
