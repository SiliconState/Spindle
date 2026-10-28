// Spindle's wasm Linux: a Linux kernel compiled to WebAssembly (arch/wasm), run with no
// emulator. The machine is one shared memory and one kernel instance per task, each in
// a worker of its own; this side builds the device tree, boots the kernel, starts the
// workers the kernel asks for, runs what it asks the main thread to run, and says how
// the machine ended. It runs in Node (worker_threads) and in the browser (Workers; an
// extension's pages share memory with them without cross-origin isolation).
import { deviceTree } from './devicetree.mjs';
import { kernelImports } from './imports.mjs';
import { Devices } from './virtio.mjs';

const PAGE = 65536;
/** The most memory the kernel may grow to (the device tree's /memory says so). */
const MAX_PAGES = 0xffff;

/** The minimum pages of the memory `bytes` (a wasm module) imports. */
function importedMemoryMinimum(bytes) {
  let at = 8;
  const leb = () => {
    let result = 0;
    let shift = 0;
    for (;;) {
      const byte = bytes[at++];
      result |= (byte & 0x7f) << shift;
      if (!(byte & 0x80)) return result >>> 0;
      shift += 7;
    }
  };
  const name = () => {
    const length = leb();
    at += length;
  };
  while (at < bytes.length) {
    const id = bytes[at++];
    const size = leb();
    const end = at + size;
    if (id === 2) {
      for (let count = leb(); count > 0; count--) {
        name();
        name();
        const kind = bytes[at++];
        if (kind === 0) leb();
        else if (kind === 1) { at++; const flags = leb(); leb(); if (flags & 1) leb(); }
        else if (kind === 2) { const flags = leb(); const minimum = leb(); if (flags & 1) leb(); return minimum; }
        else if (kind === 3) { at += 2; }
        else if (kind === 4) { at++; leb(); }
      }
    }
    at = end;
  }
  throw new Error('the kernel imports no memory');
}

/** Workers started ahead (task.mjs loaded, waiting for their task): starting one is most
 * of what a new process costs, and the kernel starts one for every process and thread. */
const POOL = 8;

/** A platform from `make()` (a new worker): spawn(name) hands out a waiting one and starts
 * another in its place; close() lets the waiting go. */
function pooled(make) {
  const idle = [];
  let closed = false;
  const fill = () => {
    while (!closed && idle.length < POOL) idle.push(make());
  };
  return {
    // The name is the worker's own (for tools that list them) only when none was waiting.
    spawn(name) {
      const worker = idle.shift() ?? make(name);
      setTimeout(fill, 0);
      worker.use?.();
      return worker;
    },
    /** Starts the waiting workers now (before a boot). */
    warm: fill,
    close() {
      closed = true;
      for (const worker of idle.splice(0)) worker.terminate();
    },
  };
}

/** How to start a task's worker here: `spawn(name)` gives {post, onMessage, onError, terminate}. */
export async function nodePlatform() {
  const { Worker } = await import('node:worker_threads');
  return pooled((name = 'task') => {
    const worker = new Worker(new URL('./task.mjs', import.meta.url), { name });
    // A waiting worker keeps no process alive; one with a task does.
    worker.unref();
    return {
      use: () => worker.ref(),
      post: (message) => worker.postMessage(message),
      onMessage: (handle) => worker.on('message', handle),
      onError: (handle) => worker.on('error', handle),
      terminate: () => worker.terminate(),
    };
  });
}

export function browserPlatform() {
  return pooled((name = 'task') => {
    const worker = new Worker(new URL('./task.mjs', import.meta.url), { type: 'module', name });
    return {
      post: (message) => worker.postMessage(message),
      onMessage: (handle) => worker.addEventListener('message', ({ data }) => handle(data)),
      onError: (handle) => worker.addEventListener('error', (event) => handle(event.error ?? new Error(event.message))),
      terminate: () => worker.terminate(),
    };
  });
}

/** Boots `kernel` (vmlinux.wasm's bytes). `console(text)` gets what its boot console
 * prints; `memory`: the most it may use, in bytes; `devices`: its virtio devices
 * (virtio.mjs: consoleDevice, ninepDevice), the kernel's virtio0, virtio1... in that
 * order. Resolves to the machine: {ended (a promise of {clean, kernel, error}: `kernel`
 * when the kernel itself ended it), workers (a count), stop()}. */
export async function bootMachine({ kernel, platform, console: print = () => {}, cmdline = '', cpus = 1, initrd = null, memory: most = MAX_PAGES * PAGE, devices = [] }) {
  const bytes = kernel instanceof Uint8Array ? kernel : new Uint8Array(kernel);
  const module = await WebAssembly.compile(bytes);
  const section = (name) => {
    const [found] = WebAssembly.Module.customSections(module, name);
    return found ? new Uint8Array(found) : null;
  };
  const sections = JSON.parse(new TextDecoder().decode(section('.linux.sections') ?? new Uint8Array()) || '{}');
  const minimum = importedMemoryMinimum(bytes);
  // The initrd goes a page above the module's own, clear of the device tree the kernel
  // copies in before it reads the reservation.
  const initrdPages = initrd ? Math.ceil(initrd.length / PAGE) + 1 : 0;
  let maximum = Math.min(MAX_PAGES, Math.floor(most / PAGE));
  let memory;
  // Not every host can reserve 4 GiB up front: half as much, until one can.
  for (;;) {
    try {
      memory = new WebAssembly.Memory({ initial: minimum + initrdPages, maximum, shared: true });
      break;
    } catch (error) {
      if (!(error instanceof RangeError) || maximum <= minimum * 2) throw error;
      maximum = Math.floor(maximum / 2);
    }
  }
  const chosen = {
    bootargs: cmdline,
    ncpus: cpus,
    sections: Object.fromEntries(Object.entries(sections).map(([name, [address, size]]) => [name, [address, size]])),
  };
  const reservations = [];
  if (initrd) {
    const address = (minimum + 1) * PAGE;
    new Uint8Array(memory.buffer).set(initrd, address);
    chosen['linux,initrd-start'] = address;
    chosen['linux,initrd-end'] = address + initrd.length;
    reservations.push({ address, size: initrd.length });
  }
  const host = new Devices(devices);
  const devicetree = deviceTree({
    '#address-cells': 1,
    '#size-cells': 1,
    model: 'Spindle wasm Linux',
    compatible: 'spindle,wasm',
    chosen,
    'memory@0': { device_type: 'memory', reg: [0, maximum * PAGE] },
    ...host.nodes(),
  }, { reservations });

  const workers = new Set();
  let finish;
  const ended = new Promise((done) => { finish = done; });
  let over = false;
  // However it ends, no task runs on (their workers and the shared memory are let go).
  const end = (outcome) => {
    if (over) return;
    over = true;
    finish(outcome);
    for (const worker of workers) worker.terminate();
    workers.clear();
  };
  let instance = null;
  /** Starts a task's worker; a clone of a process takes its `image` (and makes its own
   * copy of it first when `copied`, the word its parent waits on, is given). */
  const start = (fn, arg, name, image = null, copied = null) => {
    if (over) return;
    const worker = platform.spawn(name);
    workers.add(worker);
    worker.onMessage((message) => {
      if (message.type === 'console') print(message.text);
      else if (message.type === 'spawn') start(message.fn, message.arg, message.name, message.image, message.copied);
      else if (message.type === 'main') {
        try {
          instance.exports.__indirect_function_table.get(message.fn)(message.arg);
        } catch (error) {
          end({ clean: false, error: `the main thread's part failed: ${error?.message ?? error}` });
        }
      }
      else if (message.type === 'terminate') end(message.reason === 0 ? { clean: true, kernel: true } : { clean: false, kernel: true, error: 'the kernel panicked' });
      else if (message.type === 'error') end({ clean: false, error: `task ${name} failed: ${message.message}` });
      else if (message.type === 'exit') workers.delete(worker);
    });
    worker.onError((error) => end({ clean: false, error: `task ${name} failed: ${error?.message ?? error}` }));
    worker.post({ module, memory, fn, arg, image, copied });
  };
  instance = await WebAssembly.instantiate(module, kernelImports(memory, {
    devicetree,
    initramfs: section('.linux.initramfs'),
    console: print,
    spawn(fn, arg, name, userMemory) {
      if (userMemory) return -38;
      start(fn, arg, name);
      return 0;
    },
    main() {
      throw new Error('the main thread does not run its own requests');
    },
    terminate: (reason) => end(reason === 0 ? { clean: true, kernel: true } : { clean: false, kernel: true, error: 'the kernel panicked' }),
    virtio: host.imports(),
  }));
  // Devices answer from here: an interrupt is a word the kernel's CPUs wait on.
  host.attach(memory, (irq) => {
    if (!over) instance.exports.trigger_irq(irq);
  });
  instance.exports.boot();
  return {
    ended,
    get workers() {
      return workers.size;
    },
    stop() {
      end({ clean: false, error: 'stopped' });
    },
  };
}
