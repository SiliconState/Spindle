// The engine's side of the browser Linuxes: a runner as EmbeddedClient takes one. Two
// Linuxes run commands, each in a worker of its own: v86 running Alpine (linux-worker.mjs:
// bash, Python, Node, git, the network; it resumes a saved state) and, when the extension
// carries it, the wasm Linux (wasm-linux-worker.mjs: Linux compiled to WebAssembly with
// busybox `sh`; it boots in under a second and runs at the machine's own speed, several
// threads' commands at once on CPUs of their own). linux-route.mjs decides which runs a
// command: plain POSIX sh with programs the wasm Linux has goes there, anything else to
// v86, and v86 runs again what the wasm Linux turns out to lack, unless the command made
// a Spindle call (journaled, maybe billed). A rerun is safe: a command works on a copy of
// its workspace that counts only when its result is taken. Each worker starts with its
// first command and stops after a while unused, giving its memory back. A command goes
// with its workspace's list of files; a worker asks for the bytes of those its share does
// not already hold. Its identity marks its runs as sandboxed (commands reach neither this
// machine nor the extension's keys, and the web only when their thread has network,
// through Chrome or the user's relay): v86's image digest, or with the wasm Linux both
// images' together, as one runner made of the two.

import { wantsV86, wasmCanRun } from './linux-route.mjs';

/** How long an idle Linux keeps its memory. */
const IDLE_MS = 10 * 60_000;
/** Directories whose programs the wasm Linux's commands can name. */
const PATH = ['bin', 'sbin', 'usr/bin', 'usr/sbin', 'usr/local/bin'];

/** How many times running the wasm Linux may fail in a row (a boot, its worker) before
 * every command goes to v86. */
const WASM_FAILURES = 3;

/** `onCode(session, request, signal)` answers a script's Spindle call (the code-mode
 * bridge); `signal` ends with the command, so no call outlives it. `onStatus(state,
 * detail, kind)` tells a Linux's state, `kind` 'v86' or 'wasm' (told 'not started' once
 * the extension is found to carry it). `route`: 'auto' (linux-route.mjs decides), or
 * 'v86' / 'wasm' to send every command to that one, with no fallback (tests and
 * benchmarks). */
export function linuxRunner({ onStatus = () => {}, onCode = async () => ({ ok: false, error: 'no Spindle calls here' }), route = 'auto' } = {}) {
  const machines = {
    v86: { url: 'linux-worker.mjs', worker: null, idle: 0 },
    wasm: { url: 'wasm-linux-worker.mjs', worker: null, idle: 0 },
  };
  let next = 0;
  let identity = null;
  /** The user's relay for commands' network (null: through Chrome). */
  let relay = null;
  /** Run id -> {done, kind}: the command's end, and the Linux it runs on. */
  const waiting = new Map();
  /** Run id -> the session its command is for, a signal that ends with it, how its files
   * are read, and whether its scripts made a Spindle call. */
  const sessions = new Map();
  /** The wasm Linux as found: {identity, programs} when the extension carries it, null
   * when it does not; false once it cannot run here (it said so, or failed
   * WASM_FAILURES times in a row). */
  let wasm;
  let wasmFailures = 0;
  const wasmFailed = () => {
    if (++wasmFailures >= WASM_FAILURES && wasm) {
      wasm = false;
      onStatus('unavailable', `it failed ${WASM_FAILURES} times in a row; commands go to v86`, 'wasm');
    }
  };
  const wasmFound = (async () => {
    try {
      const [digest, index] = await Promise.all([
        fetch(new URL('wasm-linux/identity', import.meta.url)).then((response) => (response.ok ? response.text() : null)),
        fetch(new URL('wasm-linux/fs.json', import.meta.url)).then((response) => (response.ok ? response.json() : null)),
      ]);
      wasm = digest && index ? { identity: digest.trim(), programs: programsOf(index) } : null;
    } catch {
      wasm = null;
    }
    if (wasm) onStatus('not started', null, 'wasm');
    return wasm;
  })();

  function ensure(kind) {
    const machine = machines[kind];
    if (machine.worker) return machine.worker;
    const worker = new Worker(new URL(machine.url, import.meta.url), { type: 'module' });
    machine.worker = worker;
    if (kind === 'v86') worker.postMessage({ type: 'relay', url: relay });
    worker.onmessage = ({ data }) => {
      if (data.type === 'status') {
        if (kind === 'wasm') {
          if (data.state === 'unavailable') wasm = false;
          else if (data.state === 'ready') wasmFailures = 0;
          else if (data.state === 'failed') wasmFailed();
          if (wasm === false && data.state !== 'unavailable') return;
        }
        onStatus(data.state, data.detail, kind);
      } else if (data.type === 'ran') {
        waiting.get(data.id)?.done(data.result);
        waiting.delete(data.id);
      } else if (data.type === 'need') {
        const run = sessions.get(data.id);
        try {
          if (!run) throw new Error('that command has ended');
          const files = data.objects.map((object) => ({ object, bytes: owned(run.read(object)) }));
          worker.postMessage({ type: 'files', id: data.id, files }, files.map((file) => file.bytes));
        } catch (error) {
          worker.postMessage({ type: 'files', id: data.id, error: String(error?.message ?? error) });
        }
      } else if (data.type === 'code') {
        const run = sessions.get(data.id);
        if (run) run.called = true;
        Promise.resolve(run ? onCode(run.session, data.request, run.ended.signal) : { ok: false, error: 'that command has ended' })
          .catch((error) => ({ ok: false, may_have_run: false, error: String(error?.message ?? error) }))
          .then((reply) => worker.postMessage({ type: 'code_reply', id: data.id, seq: data.seq, reply }));
      }
    };
    worker.onerror = (event) => {
      event.preventDefault?.();
      for (const [id, entry] of waiting) {
        if (entry.kind !== kind) continue;
        // One whose script made a Spindle call does not run again (on v86).
        const called = Boolean(sessions.get(id)?.called);
        entry.done({ output: '', success: false, called, unavailable: kind === 'wasm', failure: `the browser Linux failed: ${event.message}` });
        waiting.delete(id);
      }
      worker.terminate();
      if (machine.worker === worker) machine.worker = null;
      if (kind === 'wasm') wasmFailed();
      if (wasm !== false || kind === 'v86') onStatus('failed', event.message, kind);
    };
    return worker;
  }

  function rest(kind) {
    const machine = machines[kind];
    clearTimeout(machine.idle);
    machine.idle = setTimeout(() => {
      if ([...waiting.values()].some((entry) => entry.kind === kind) || !machine.worker) return;
      machine.worker.terminate();
      machine.worker = null;
      if (kind === 'v86' || wasm) onStatus('stopped', null, kind);
    }, IDLE_MS);
  }

  /** Runs one command on `kind`'s Linux. */
  async function runOn(kind, id, job, signal) {
    clearTimeout(machines[kind].idle);
    const worker = ensure(kind);
    // Stop: the command, and its scripts' calls with it.
    const stop = () => {
      sessions.get(id)?.ended.abort();
      machines[kind].worker?.postMessage({ type: 'stop', id });
    };
    signal?.addEventListener('abort', stop, { once: true });
    try {
      return await new Promise((done) => {
        waiting.set(id, { done, kind });
        worker.postMessage({ type: 'run', id, ...job });
      });
    } finally {
      signal?.removeEventListener('abort', stop);
      rest(kind);
    }
  }

  return {
    async identity() {
      if (identity) return identity;
      const v86 = (await (await fetch(new URL('linux/identity', import.meta.url))).text()).trim();
      const found = await wasmFound;
      identity = found ? await digest(`spindle-browser-linux-v1\0${v86}\0${found.identity}`) : v86;
      return identity;
    },

    async run({ session, command, timeoutS, network, files, links, directories, read, signal }) {
      if (signal?.aborted) return { output: '', success: false, failure: 'the command was stopped before it ran' };
      const job = {
        session, command, timeoutS, network, links, directories,
        files: files.map(({ path, size, executable, object }) => ({ path, size, executable, object })),
      };
      const found = await wasmFound;
      // A Stop while the image was looked for (runOn takes those from here).
      if (signal?.aborted) return { output: '', success: false, failure: 'the command was stopped before it ran' };
      if (route === 'wasm' && !(found && wasm)) return { output: '', success: false, failure: 'the wasm Linux is unavailable here' };
      const toWasm = route === 'wasm' || (route === 'auto' && !network && wasmCanRun(command, found?.programs ?? new Set()));
      if (found && wasm && toWasm) {
        const id = ++next;
        sessions.set(id, { session, ended: new AbortController(), read, called: false });
        let result;
        try {
          result = await runOn('wasm', id, job, signal);
        } finally {
          sessions.get(id)?.ended.abort();
          sessions.delete(id);
        }
        if (route === 'wasm' || signal?.aborted || !wantsV86(result)) return told(result);
      }
      const id = ++next;
      sessions.set(id, { session, ended: new AbortController(), read, called: false });
      try {
        return told(await runOn('v86', id, job, signal));
      } finally {
        sessions.get(id)?.ended.abort();
        sessions.delete(id);
      }
    },

    /** Sends commands' network through `url` (a wisp:// or wisps:// relay), or Chrome. */
    setRelay(url) {
      relay = url ?? null;
      machines.v86.worker?.postMessage({ type: 'relay', url: relay });
    },

    /** A thread gone: what the Linuxes kept of its workspace goes. */
    forget(session) {
      for (const machine of Object.values(machines)) machine.worker?.postMessage({ type: 'forget', session });
    },

    /** Starts (or resumes) the Linuxes ahead of the first command. */
    warm() {
      ensure('v86').postMessage({ type: 'start' });
      rest('v86');
      wasmFound.then((found) => {
        if (!found || wasm === false) return;
        ensure('wasm').postMessage({ type: 'start' });
        rest('wasm');
      });
    },
  };
}

/** A result as the package takes it (without what only the runner looks at). */
function told({ called, code, unavailable, ...result }) {
  return result;
}

/** The names a command can call in the wasm Linux's image (its fs.json): what its PATH
 * directories hold. */
function programsOf(index) {
  const names = new Set();
  const visit = (entries, path) => {
    for (const entry of entries) {
      const [name, , , mode, , , target] = entry;
      const full = path ? `${path}/${name}` : name;
      if ((mode & 0o170000) === 0o040000) visit(target, full);
      else if (PATH.includes(path)) names.add(name);
    }
  };
  visit(index.fsroot ?? [], '');
  return names;
}

async function digest(text) {
  const hash = new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text)));
  return Array.from(hash, (byte) => byte.toString(16).padStart(2, '0')).join('');
}

/** Bytes whose buffer is theirs alone (to hand to the worker without a copy). */
function owned(bytes) {
  return bytes.byteOffset === 0 && bytes.byteLength === bytes.buffer.byteLength ? bytes.buffer : bytes.slice().buffer;
}
