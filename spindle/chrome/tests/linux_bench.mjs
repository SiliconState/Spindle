// How fast the browser's two Linuxes run commands, in real Chrome: the gate for sending
// commands to the wasm Linux (chrome/linux-route.mjs). The same commands go to v86
// (linux-worker.mjs) and to the wasm Linux (wasm-linux-worker.mjs) through
// linux-runner.mjs from an extension page: each Linux's first command (a v86 with no
// saved state boots and saves one; the wasm Linux always boots), then each command a few
// times, the median kept. The page it runs from is written into DIST, which must be a
// test build made with SPINDLE_WASM_LINUX (chrome/build.sh), never one a user loads.
//
// usage: SPINDLE_BROWSER=/path/to/chrome node chrome/tests/linux_bench.mjs DIST [RUNS]
import fs from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { launch, open } from './cdp.mjs';

const [distArg, runsArg = '5'] = process.argv.slice(2);
const browserBinary = process.env.SPINDLE_BROWSER;
if (!distArg || !browserBinary) throw new Error('usage: SPINDLE_BROWSER=... node chrome/tests/linux_bench.mjs DIST [RUNS]');
const dist = resolve(distArg);
if (!fs.existsSync(join(dist, 'wasm-linux/identity'))) throw new Error(`${dist} carries no wasm Linux (chrome/build.sh with SPINDLE_WASM_LINUX)`);
const runs = Number(runsArg);

fs.writeFileSync(join(dist, 'bench.html'), '<!doctype html><title>Linux bench</title><script type="module" src="bench.mjs"></script>');
fs.writeFileSync(join(dist, 'bench.mjs'), `import { linuxRunner } from './linux-runner.mjs';
const runners = { v86: linuxRunner({ route: 'v86' }), wasm: linuxRunner({ route: 'wasm' }) };
window.time = async (kind, command, session) => {
  const started = performance.now();
  const result = await runners[kind].run({ session, command, timeoutS: 300, network: null, files: [], links: [], directories: [], read: () => new Uint8Array() });
  return { ms: Math.round(performance.now() - started), success: result.success, output: result.output.slice(0, 200), failure: result.failure ?? null };
};
window.ready = true;
`);

const commands = [
  ['true', 'true'],
  ['echo + file', 'echo hello > out.txt && cat out.txt'],
  ['ls | wc', 'ls -la /bin | wc -l'],
  ['pipeline sort', 'seq 1 100000 | sort -rn | tail -3'],
  ['30 programs', 'for i in $(seq 1 30); do date > /dev/null; done; echo done'],
  ['200 files', 'mkdir -p d && for i in $(seq 1 200); do echo $i > d/f$i; done && ls d | wc -l'],
  ['CPU (awk)', "awk 'BEGIN { for (i = 0; i < 3000000; i++) s += i * i; print s }'"],
  ['python start', 'python3 -c pass'],
  ['python imports', 'python3 -c "import json, re, collections, datetime, pathlib, subprocess; print(len(json.dumps(list(range(1000)))))"'],
  ['python CPU', 'python3 -c "print(sum(i * i for i in range(3_000_000)))"'],
  ['python 200 files', 'python3 -c "import os; os.makedirs(\'p\', exist_ok=True); [open(f\'p/{i}\', \'w\').write(str(i)) for i in range(200)]; print(len(os.listdir(\'p\')))"'],
];

const work = fs.mkdtempSync(join(tmpdir(), 'spindle-bench-'));
let browser = null;
process.on('exit', () => {
  browser?.child.kill('SIGKILL');
  fs.rmSync(work, { recursive: true, force: true });
});
browser = await launch(browserBinary, { profile: join(work, 'profile') });
const id = (await browser.send('Extensions.loadUnpacked', { path: dist })).id;
const page = await open(browser, `chrome-extension://${id}/bench.html`);
await page.until('window.ready === true');
const time = (kind, command, session) => page.eval(`window.time(${JSON.stringify(kind)}, ${JSON.stringify(command)}, ${JSON.stringify(session)})`);

const median = (values) => [...values].sort((a, b) => a - b)[Math.floor(values.length / 2)];
const rows = [];
for (const kind of ['wasm', 'v86']) {
  const first = await time(kind, 'echo first', `${kind}-first`);
  if (!first.success) throw new Error(`${kind}'s first command failed: ${JSON.stringify(first)}`);
  rows.push({ what: 'first command (start)', kind, ms: first.ms });
}
for (const [label, command] of commands) {
  const results = {};
  for (const kind of ['wasm', 'v86']) {
    const times = [];
    let output = null;
    for (let run = 0; run < runs; run++) {
      const result = await time(kind, command, `${kind}-${label}-${run}`);
      if (!result.success) throw new Error(`${label} on ${kind} failed: ${JSON.stringify(result)}`);
      output ??= result.output;
      times.push(result.ms);
    }
    results[kind] = { ms: median(times), output };
  }
  rows.push({ what: label, kind: 'wasm', ms: results.wasm.ms }, { what: label, kind: 'v86', ms: results.v86.ms });
  if (results.wasm.output.trim() !== results.v86.output.trim() && !['true'].includes(command)) {
    console.log(`  (outputs differ for ${label}: wasm ${JSON.stringify(results.wasm.output)} v86 ${JSON.stringify(results.v86.output)})`);
  }
}
console.log('what'.padEnd(24), 'wasm Linux'.padStart(12), 'v86'.padStart(10), '  v86 / wasm');
for (const what of [...new Set(rows.map((row) => row.what))]) {
  const wasm = rows.find((row) => row.what === what && row.kind === 'wasm').ms;
  const v86 = rows.find((row) => row.what === what && row.kind === 'v86').ms;
  console.log(what.padEnd(24), `${wasm} ms`.padStart(12), `${v86} ms`.padStart(10), `  ${(v86 / Math.max(wasm, 1)).toFixed(1)}x`);
}
await browser.close();
process.exit(0);
