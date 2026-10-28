// The embedded package in a real browser against a real provider (nothing scripted):
// the relay from the kit serves the test page and forwards its model requests, counting
// each one, and the browser runs in three launches on one profile.
//
//   1. A real task with several file tools; a stop while the reply streams, then
//      resume; a steering message while it works; then the browser is killed while a
//      request streams.
//   2. The browser restarted: the session is still there, a second worker is refused
//      while the first holds it, `resume` sends nothing (the killed request's outcome is
//      unknown), `retry` sends exactly one request, and the session moves out by bundle
//      (its old place then refused).
//   3. After a native host continued the bundle with its own real turn and moved it
//      back: imported, the native host's change is there, and a real turn goes on.
// Every bundle is checked for the key.
//
//   SPINDLE_KEY=... SPINDLE_REAL_BROWSER=/path/to/chrome SPINDLE_BIN=/path/to/spindle \
//     node embed/tests/browser_real.mjs KIT_DIR --provider ID [--model M] [--catalog FILE]
//
// KIT_DIR is a kit from embed/kit.sh. The browser is Chrome or Edge (a Windows one works
// from WSL); it runs headless on a fresh profile in its own temporary folder, and only
// its processes are stopped. The native leg uses SPINDLE_BIN's own credentials for the
// provider (its logins), as a native host does. Real requests are made and billed.
import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const [kitArg, ...rest] = process.argv.slice(2);
const options = {};
while (rest.length) {
  const arg = rest.shift();
  if (!arg.startsWith('--') || !rest.length) throw new Error(`expected --name value, got ${arg}`);
  options[arg.slice(2)] = rest.shift();
}
const { SPINDLE_KEY: key, SPINDLE_REAL_BROWSER: browser, SPINDLE_BIN: native } = process.env;
if (!kitArg || !options.provider || !key || !browser || !native) {
  throw new Error('usage: SPINDLE_KEY=... SPINDLE_REAL_BROWSER=... SPINDLE_BIN=... node browser_real.mjs KIT_DIR --provider ID [--model M] [--catalog FILE]');
}
const kit = resolve(kitArg);
const here = fileURLToPath(new URL('.', import.meta.url));
const run = `spindle-real-${Date.now()}`;
const work = fs.mkdtempSync(join(tmpdir(), `${run}-`));
const site = join(work, 'site');
const reports = join(work, 'reports.ndjson');
for (const part of ['web', 'clients']) fs.cpSync(join(kit, part), join(site, part), { recursive: true });
fs.cpSync(join(here, 'browser_real'), join(site, 'tests', 'browser_real'), { recursive: true });
fs.writeFileSync(reports, '');

// A Windows browser run from WSL gets a Windows profile folder and Windows paths.
const windows = /^\/mnt\/[a-z]\//.test(browser);
const toWindows = (path) => execFileSync('wslpath', ['-w', path], { encoding: 'utf8' }).trim();
const profile = windows
  ? join(execFileSync('wslpath', ['-u', execFileSync('cmd.exe', ['/c', 'echo %TEMP%'], { cwd: '/mnt/c', encoding: 'utf8' }).trim()], { encoding: 'utf8' }).trim(), run)
  : join(work, 'profile');
fs.mkdirSync(profile, { recursive: true });

// The relay: the kit's own, serving this test's site, counting requests.
const relay = spawn(process.execPath, [join(kit, 'examples', 'relay.mjs'), '--root', site, '--report', reports,
  ...(options.catalog ? ['--catalog', options.catalog] : [])], { env: process.env, stdio: ['ignore', 'pipe', 'inherit'] });
const port = await new Promise((resolvePort, reject) => {
  relay.on('exit', (code) => reject(new Error(`the relay exited (${code})`)));
  relay.stdout.on('data', (text) => {
    const found = /http:\/\/localhost:(\d+)\//.exec(String(text));
    if (found) resolvePort(found[1]);
  });
});
const stats = async () => (await fetch(`http://127.0.0.1:${port}/stats`)).json();

let launched = null;
function launch(phase, dir) {
  const query = new URLSearchParams({ phase, dir, provider: options.provider, ...(options.model && { model: options.model }) });
  const url = `http://localhost:${port}/tests/browser_real/index.html?${query}`;
  launched = spawn(browser, ['--headless=new', '--no-first-run', '--no-default-browser-check', '--disable-gpu',
    `--user-data-dir=${windows ? toWindows(profile) : profile}`, url], { stdio: 'ignore', detached: !windows });
}
/** Stops this run's browser only: the processes on its profile. */
function kill() {
  if (windows) {
    const name = basename(browser);
    execFileSync('powershell.exe', ['-NoProfile', '-Command',
      `Get-CimInstance Win32_Process -Filter "Name='${name}'" | Where-Object { $_.CommandLine -like '*${run}*' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }`],
    { stdio: 'ignore' });
  } else if (launched) {
    try { process.kill(-launched.pid, 'SIGKILL'); } catch {}
  }
}
// Interrupted, it still stops its browser and relay (never leaving them running).
for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
  process.on(signal, () => {
    kill();
    relay.kill();
    process.exit(130);
  });
}
const lines = () => fs.readFileSync(reports, 'utf8').split('\n').filter(Boolean).map((line) => JSON.parse(line));
async function until(phase, steps, seconds) {
  const end = Date.now() + seconds * 1000;
  while (Date.now() < end) {
    const found = lines().find((line) => line.phase === phase && steps.includes(line.step));
    if (found) return found;
    await new Promise((next) => setTimeout(next, 250));
  }
  throw new Error(`phase ${phase}: no ${steps.join(' or ')} within ${seconds}s`);
}
const step = (phase, name) => lines().find((line) => line.phase === phase && line.step === name) ?? {};

const checks = [];
const check = (name, fn) => {
  try {
    fn();
    checks.push([true, name]);
  } catch (error) {
    checks.push([false, name, error.message.split('\n')[0]]);
  }
};
const bundles = [];
const dir = `/sessions/${run}`;
try {
  // Phase 1: real work, stop, steer, then the browser killed mid-request.
  launch('1', dir);
  const ready = await until('1', ['crash-ready', 'error'], 600);
  kill();
  assert.equal(ready.step, 'crash-ready', `phase 1 failed: ${ready.message}`);
  for (let at = 0; at < 40 && (await stats()).open > 0; at++) await new Promise((next) => setTimeout(next, 250));

  // Phase 2: restarted; unknown kept, retry once, moved out.
  launch('2', dir);
  await until('2', ['done', 'error'], 600);
  kill();
  const moved = step('2', 'export');
  if (moved.bundle) {
    const bundle = Buffer.from(moved.bundle, 'base64');
    bundles.push(['browser export', bundle]);
    // The native leg: import, a real turn of its own, move it back for phase 3.
    const file = join(work, 'from-browser.spindle');
    fs.writeFileSync(file, bundle);
    const nativeDir = join(work, 'native');
    const spindle = (...args) => execFileSync(native, ['--dir', nativeDir, ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
    const leg = {};
    try {
      leg.imported = spindle('import-session', file);
      leg.turn = spindle('say', 'Using your file tools, append a line "native" to notes.md, then reply with only DONE.');
      leg.notes = spindle('cat', 'notes.md');
      leg.exported = spindle('export-session', join(site, 'back.spindle'));
    } catch (error) {
      leg.error = `${error.message}\n${error.stderr ?? ''}`;
    }
    check('native host imports the browser bundle and runs a real turn on it', () => {
      assert.ok(!leg.error, leg.error);
      assert.match(leg.notes, /native/);
    });
    if (!leg.error) {
      bundles.push(['native export', fs.readFileSync(join(site, 'back.spindle'))]);
      // Phase 3: back into the browser, and on.
      launch('3', dir);
      await until('3', ['done', 'error'], 600);
      kill();
      const back = step('3', 'back');
      if (back.bundle) bundles.push(['browser copy', Buffer.from(back.bundle, 'base64')]);
    }
  }
} catch (error) {
  checks.push([false, 'the run itself', error.message]);
} finally {
  kill();
  try {
    checks.relayed = (await stats()).requests;
  } catch {}
  relay.kill();
}

const s = (phase, name) => step(phase, name);
check('created a session with the provider', () => assert.equal(s('1', 'created').ok, true, s('1', 'created').error));
check('a real task with several file tools completes', () => {
  const tools = s('1', 'tools');
  assert.equal(tools.complete, true, tools.error);
  assert.ok(tools.calls?.length >= 2, `tool calls: ${tools.calls}`);
  assert.match(tools.notes ?? '', /# Notes[\s\S]*one/);
});
check('stop cuts the streaming reply: one request, ended as stopped', () => {
  const stop = s('1', 'stop');
  assert.equal(stop.streamed, true);
  assert.equal(stop.stopOk, true);
  assert.equal(stop.cutKind, 'stopped');
  assert.equal(stop.cutRequests, 1);
});
check('after the stop, resume sends nothing (the cut request is unknown) and retry sends it once', () => {
  const stop = s('1', 'stop');
  assert.equal(stop.resumeOk, false);
  assert.match(stop.resumeError ?? '', /unknown outcome/);
  assert.equal(stop.resumeRequests, 0);
  assert.equal(stop.retryComplete, true, stop.retryError);
  assert.equal(stop.retryRequests, 1);
});
check('a steering message is taken in while it works', () => {
  const steer = s('1', 'steer');
  assert.equal(steer.steerOk, true);
  assert.equal(steer.complete, true, steer.error);
  assert.deepEqual(steer.unsent, []);
  assert.equal(steer.steeredEvent, true);
});
check('the browser is killed while exactly one request streams', () => {
  const ready = s('1', 'crash-ready');
  assert.equal(ready.streaming, true, ready.error);
  assert.equal(ready.requests, 1);
});
check('after the restart the session is there, its turn unfinished', () => {
  const reopened = s('2', 'reopened');
  assert.equal(reopened.ok, true);
  assert.equal(reopened.complete, false, reopened.state);
});
check('a second worker is refused while the first holds the session', () => {
  assert.equal(s('2', 'second worker').ok, false);
  assert.equal(s('2', 'second worker').kind, 'locked');
});
check('resume sends nothing: the killed request stays unknown', () => {
  const resumed = s('2', 'resume unknown');
  assert.equal(resumed.ok, false);
  assert.equal(resumed.requests, 0);
});
check('retry sends it exactly once and completes', () => {
  const retried = s('2', 'retry');
  assert.equal(retried.complete, true, retried.error);
  assert.equal(retried.requests, 1);
});
check('the session moves out by bundle and its old place is refused', () => {
  const moved = s('2', 'export');
  assert.equal(moved.ok, true, moved.error);
  assert.equal(moved.sealedOk, false);
  assert.match(moved.sealedError ?? '', /moved elsewhere/);
});
check('back in the browser: the native change is there and a real turn completes', () => {
  const back = s('3', 'back');
  assert.equal(back.importOk, true, back.importError);
  assert.match(back.notes ?? '', /native/);
  assert.equal(back.complete, true, back.error);
  assert.ok(back.requests >= 1);
});
check('the key is in no bundle', () => {
  assert.ok(bundles.length >= 2, `bundles checked: ${bundles.map(([name]) => name)}`);
  for (const [name, bytes] of bundles) assert.ok(!bytes.includes(Buffer.from(key)), `the key is in the ${name}`);
});

for (const [ok, name, why] of checks) console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${why ? `: ${why}` : ''}`);
const info = { tools: s('1', 'tools').reply, back: s('3', 'back').reply, steeredFiles: s('1', 'steer').files };
console.log(`model replies (not judged): ${JSON.stringify(info)}`);
console.log(`model requests relayed in all: ${checks.relayed ?? 'unknown'}; reports in ${reports}`);
try { fs.rmSync(profile, { recursive: true, force: true }); } catch {}
const failed = checks.filter(([ok]) => !ok).length;
console.log(failed ? `browser-real FAILED: ${failed} of ${checks.length} checks` : `browser-real passed: ${checks.length} checks against a real provider in a real browser`);
process.exitCode = failed ? 1 : 0;
