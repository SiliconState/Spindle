// One task of the wasm Linux: a worker running its own instance of the kernel on the
// machine's shared memory, entered at the function the kernel named (its CPU, a kernel
// thread, a process or one of its threads). A task cloned from a process starts with
// that process's image: its memory shared (a thread) or copied here before the parent
// goes on (a process). It runs until the kernel halts it. What reaches beyond it goes to
// the machine as messages.
import { Halt, kernelImports } from './imports.mjs';
import { copyImage, userImports } from './user.mjs';

const ENOMEM = 12;
/** How a clone takes its parent's memory (enum wasm_user_memory). */
const SHARE = 1;
const COPY = 2;

const node = typeof process !== 'undefined' && Boolean(process.versions?.node);
const port = node
  ? await import('node:worker_threads').then(({ parentPort }) => ({
      post: (message) => parentPort.postMessage(message),
      once: (handle) => parentPort.once('message', handle),
      close: () => parentPort.close(),
    }))
  : {
      post: (message) => self.postMessage(message),
      once: (handle) => { self.onmessage = ({ data }) => handle(data); },
      close: () => self.close(),
    };

port.once(async ({ module, memory, fn, arg, image, copied }) => {
  // A process cloned without sharing memory: its own copy, made before its parent,
  // waiting on `copied`, goes on.
  if (copied) {
    try {
      image = copyImage(image);
      Atomics.store(copied, 0, 1);
    } catch {
      Atomics.store(copied, 0, -ENOMEM);
    }
    Atomics.notify(copied, 0);
    if (Atomics.load(copied, 0) < 0) {
      port.post({ type: 'exit' });
      port.close();
      return;
    }
  }
  let instance = null;
  const user = userImports(memory, () => instance, image ?? null);
  const imports = kernelImports(memory, {
    console: (text) => port.post({ type: 'console', text }),
    spawn(entry, value, name, userMemory) {
      if (userMemory !== SHARE && userMemory !== COPY) {
        port.post({ type: 'spawn', fn: entry, arg: value, name });
        return 0;
      }
      const done = userMemory === COPY ? new Int32Array(new SharedArrayBuffer(4)) : null;
      port.post({ type: 'spawn', fn: entry, arg: value, name, image: user.image, copied: done });
      if (!done) return 0;
      // The kernel asks for the copy synchronously: the parent waits until it is made.
      Atomics.wait(done, 0, 0);
      const result = Atomics.load(done, 0);
      return result === 1 ? 0 : result;
    },
    main: (entry, value) => port.post({ type: 'main', fn: entry, arg: value }),
    terminate: (reason) => port.post({ type: 'terminate', reason }),
    user: user.imports,
  });
  try {
    instance = await WebAssembly.instantiate(module, imports);
    user.prepare();
    instance.exports.__indirect_function_table.get(fn)(arg);
  } catch (error) {
    if (!(error instanceof Halt)) port.post({ type: 'error', message: String(error?.stack ?? error) });
  }
  port.post({ type: 'exit' });
  port.close();
});
