// Python on Spindle's wasm Linux (chrome/wasm-linux/build-python.sh), from the image the
// extension carries (build-image.sh's dist: fs.json and flat/, read here from disk as
// the extension fetches them, zstd decompressed with v86's decoder as the extension
// does: wasm-linux/zstd.mjs), run as commands through the runner: the version (the
// browser Linux's, 3.14), the standard library being Alpine's own files, ripgrep (piped
// input too), the time zones, its C
// modules (zlib, bz2, sqlite3, hashlib, decimal, json, xml), threads, files in the
// workspace coming back, subprocess (started with posix_spawn: there is no fork), a
// missing program raising as it should, asyncio, a Unix socket pair, and Spindle's calls
// from the `spindle` module. Times are printed: a first command (boot included), a Python
// start, a computation.
//
// usage: node chrome/tests/wasm_linux_python.mjs [DIST [V86_WASM]]
//   (defaults chrome/wasm-linux/dist and chrome/linux/dist/v86.wasm)
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { objectDigest } from '../linux-workspace.mjs';
import { nodePlatform } from '../wasm-linux/machine.mjs';
import { WasmLinux } from '../wasm-linux/runner.mjs';
import { FS } from '../wasm-linux/v86/lib/filesystem.js';
import { contentSize, zstdDecoder } from '../wasm-linux/zstd.mjs';

const dist = resolve(process.argv[2] ?? new URL('../wasm-linux/dist', import.meta.url).pathname);
const decode = await zstdDecoder(readFileSync(resolve(process.argv[3] ?? new URL('../linux/dist/v86.wasm', import.meta.url).pathname)));
const unpack = (bytes, size) => new Uint8Array(decode(size ?? contentSize(bytes), bytes));
// The image's files as the extension's storage gives them (by digest, read when opened).
const held = new Map();
const storage = {
  async read(sha256, offset, count, size) {
    if (!held.has(sha256)) {
      const bytes = new Uint8Array(readFileSync(`${dist}/flat/${sha256}`));
      held.set(sha256, sha256.endsWith('.zst') ? unpack(bytes, size) : bytes);
    }
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
  kernel: unpack(new Uint8Array(readFileSync(`${dist}/vmlinux.wasm.zst`))),
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

const first = await run('python3 -c "import sys, subprocess, os; print(sys.version_info[:2], os.path.getmtime(subprocess.__file__) > 0, subprocess.__cached__)"');
assert.equal(first.output, '(3, 14) True /usr/lib/python3.14/__pycache__/subprocess.cpython-314.pyc\n', show(first));
// Alpine's compiled modules are read as they are (their sources' times are kept), never
// compiled again at each start.
const compiled = await run('python3 -v -c "import json, email.message" 2>&1 | grep -c "bytecode is stale\\|code object from" ; python3 -v -c "import json, email.message" 2>&1 | grep -c "bytecode is stale"');
assert.match(compiled.output, /^[1-9][0-9]*\n0\n$/, show(compiled));
// ripgrep, the browser Linux's version: it searches and lists files (reading them: there is
// no mmap here); `rg -P` says PCRE2 is not in this build (the runner then goes to v86).
const searched = await run('printf "alpha\\nneedle one\\nbeta\\n" > a.txt && mkdir -p d && printf "x needle\\n" > d/b.txt && rg -n needle | sort && rg --files | sort && rg -c a a.txt && rg --version | head -n 1');
assert.equal(searched.output, 'a.txt:2:needle one\nd/b.txt:1:x needle\na.txt\nd/b.txt\n2\nripgrep 15.1.0\n', show(searched));
// What comes in on a pipe, or from a file, is searched (not the directory): ripgrep looks
// at its stdin through a copy of the descriptor, which std makes here as on Linux.
const piped = await run('printf "a\\nneedle in\\n" > in.txt && printf "x\\nneedle piped\\n" | rg needle && rg -c needle < in.txt');
assert.equal(piped.output, 'needle piped\n1\n', show(piped));
// The time zones, read by Python's zoneinfo and by `TZ=... date` (busybox on musl).
const zones = await run(`python3 -c "import datetime, zoneinfo; z = zoneinfo.ZoneInfo('America/New_York'); print(*(datetime.datetime.fromtimestamp(t, z).strftime('%z %Z') for t in (1783000000, 1795000000)))" && TZ=America/New_York date -d @1795000000 '+%z %Z'`);
assert.equal(zones.output, '-0400 EDT -0500 EST\n-0500 EST\n', show(zones));
const pcre = await run('rg -P "needle(?= one)" a.txt');
assert.equal(pcre.code, 2, show(pcre));
assert.match(pcre.output, /PCRE2 is not available in this build of ripgrep/, show(pcre));
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
