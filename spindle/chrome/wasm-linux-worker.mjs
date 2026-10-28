// The wasm Linux (chrome/wasm-linux: Linux compiled to WebAssembly, run with no
// emulator; its runner wasm-linux/runner.mjs), in a worker of its own beside v86's
// (linux-worker.mjs), speaking the same messages, so linux-runner.mjs sends a command to
// either. Its root filesystem is wasm-linux/fs.json and flat/ (fetched as the guest opens
// files), its kernel wasm-linux/vmlinux.wasm; it boots in under a second, so it is not
// saved. Several commands run at once (one per thread at a time, each on CPUs of its
// own); none has network (linux-runner.mjs sends those to v86). It checks first that this
// browser lets a worker share memory with the workers it starts, and says `unavailable`
// when it does not (or the image is missing): commands then go to v86. A boot that fails
// otherwise says `failed`, and the next command tries again (linux-runner.mjs stops
// sending commands after a few such failures in a row).
//
// engine -> worker  { type: 'run', id, session, command, timeoutS, files, links, directories }
//                   { type: 'files', id, files | error } | { type: 'stop', id }
//                   { type: 'code_reply', id, seq, reply } | { type: 'forget', session } | { type: 'start' }
// worker -> engine  { type: 'ran', id, result } (with `called` when a script made a Spindle
//                   call, `code` its exit status) | { type: 'status', state, detail }
//                   | { type: 'need', id, objects } | { type: 'code', id, seq, request }
import { browserPlatform } from './wasm-linux/machine.mjs';
import { WasmLinux } from './wasm-linux/runner.mjs';
import { FS } from './wasm-linux/v86/lib/filesystem.js';
import { MemoryFileStorage, ServerFileStorageWrapper } from './wasm-linux/v86/src/browser/filestorage.js';

const base = new URL('./wasm-linux/', import.meta.url).href;
/** Commands running at once (the machine gives each two CPUs of its own). */
const MOST_RUNNING = 4;

const status = (state, detail = null) => postMessage({ type: 'status', state, detail });
let linux = null;
let starting = null;
const queue = [];
const running = new Map();
const asked = new Map();

/** Whether this browser lets a worker hand a shared memory to a worker it starts and
 * block on it (the wasm Linux's every task does). */
async function sharesMemory() {
  try {
    const memory = new WebAssembly.Memory({ initial: 1, maximum: 1, shared: true });
    const source = `onmessage = ({ data }) => { const word = new Int32Array(data.buffer); Atomics.store(word, 0, 1); Atomics.notify(word, 0); postMessage(Atomics.wait(word, 1, 0, 1)); };`;
    const url = URL.createObjectURL(new Blob([source], { type: 'text/javascript' }));
    const worker = new Worker(url);
    try {
      const answer = await new Promise((resolve, reject) => {
        worker.onmessage = ({ data }) => resolve(data);
        worker.onerror = (event) => reject(new Error(event.message));
        worker.postMessage(memory);
        setTimeout(() => reject(new Error('no answer')), 5000);
      });
      return answer === 'timed-out' && new Int32Array(memory.buffer)[0] === 1;
    } finally {
      worker.terminate();
      URL.revokeObjectURL(url);
    }
  } catch {
    return false;
  }
}

/** An error that trying again will not mend. */
function lasting(message) {
  return Object.assign(new Error(message), { lasting: true });
}

function start() {
  starting ??= (async () => {
    if (!(await sharesMemory())) throw lasting('this browser does not share memory with workers');
    const [kernel, index] = await Promise.all([
      fetch(`${base}vmlinux.wasm`).then((response) => {
        if (!response.ok) throw lasting(`no wasm Linux kernel (HTTP ${response.status})`);
        return response.arrayBuffer();
      }),
      fetch(`${base}fs.json`).then((response) => {
        if (!response.ok) throw lasting(`no wasm Linux image (HTTP ${response.status})`);
        return response.json();
      }),
    ]);
    const root = new FS(new ServerFileStorageWrapper(new MemoryFileStorage(), `${base}flat/`));
    root.load_from_json(index);
    const platform = browserPlatform();
    platform.warm();
    const made = new WasmLinux({
      kernel: new Uint8Array(kernel),
      platform,
      root,
      cpus: 1 + 2 * MOST_RUNNING,
      onStatus: status,
      onCode: (id, seq, request) => postMessage({ type: 'code', id, seq, request }),
    });
    try {
      await made.start();
    } catch (error) {
      // Its waiting workers go with it; the next command starts afresh.
      platform.close();
      throw error;
    }
    linux = made;
  })().catch((error) => {
    starting = null;
    status(error.lasting ? 'unavailable' : 'failed', error.message);
    throw error;
  });
  return starting;
}

async function run(job) {
  try {
    await start();
  } catch (error) {
    return { output: '', success: false, unavailable: true, failure: `the wasm Linux is unavailable: ${error.message}` };
  }
  try {
    const result = await linux.run(job, (objects) => new Promise((resolve, reject) => {
      asked.set(job.id, ({ files, error }) => {
        if (error) reject(new Error(error));
        else resolve(new Map(files.map((file) => [file.object, new Uint8Array(file.bytes)])));
      });
      postMessage({ type: 'need', id: job.id, objects });
    }));
    return { ...result, called: Boolean(job.called) };
  } catch (error) {
    // Nothing ran when the command never reached the agent (a machine that did not boot).
    return { output: '', success: false, called: Boolean(job.called), unavailable: !job.sent, failure: `the wasm Linux could not run it: ${error.message}` };
  }
}

/** Starts what may run now: commands of threads with none running, a few in all. */
function schedule() {
  for (let at = 0; at < queue.length && running.size < MOST_RUNNING;) {
    const job = queue[at];
    if ([...running.values()].some((other) => other.session === job.session)) {
      at += 1;
      continue;
    }
    queue.splice(at, 1);
    running.set(job.id, job);
    run(job).catch((error) => ({ output: '', success: false, called: Boolean(job.called), failure: `the wasm Linux could not run it: ${error.message}` }))
      .then((result) => {
        running.delete(job.id);
        asked.delete(job.id);
        postMessage({ type: 'ran', id: job.id, result }, result.writes?.map((write) => write.content.buffer) ?? []);
        schedule();
      });
  }
}

self.onmessage = ({ data }) => {
  if (data.type === 'run') {
    queue.push({ ...data, stopped: false, sent: false, called: false });
    schedule();
  } else if (data.type === 'files') {
    asked.get(data.id)?.(data);
    asked.delete(data.id);
  } else if (data.type === 'stop') {
    const waiting = queue.findIndex((job) => job.id === data.id);
    if (waiting >= 0) {
      const [job] = queue.splice(waiting, 1);
      postMessage({ type: 'ran', id: job.id, result: { output: '', success: false, failure: 'the command was stopped before it ran' } });
      return;
    }
    const job = running.get(data.id);
    if (job) linux ? linux.stop(job) : (job.stopped = true);
  } else if (data.type === 'code_reply') {
    linux?.reply(data.id, data.seq, data.reply);
  } else if (data.type === 'forget') {
    const under = [...running.values(), ...queue].some((job) => job.session === data.session);
    linux?.workspaces.forget(data.session, under).catch(() => {});
  } else if (data.type === 'start') {
    start().catch(() => {});
  }
};
