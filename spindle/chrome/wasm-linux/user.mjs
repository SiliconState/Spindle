// The programs of Spindle's wasm Linux, as one task's worker runs them (the kernel's
// `user.*` imports, arch/wasm/include/asm/wasm_imports.h). A process is a wasm module
// (built for wasm32-unknown-linux-musl) and a shared memory of its own; each of its
// threads is a worker with its own instance of that module on that memory. A program
// imports `env.memory`, and `linux.syscall` and its kin straight from the kernel
// instance of its worker (a syscall is a wasm-to-wasm call), and exports `_start` and its
// function table.
//
// exec: the kernel streams the file in (`compile_begin`, `compile_write`, `compile_end`:
// compiled, its memory made, not yet used), then `instantiate(true)` makes it this
// task's image; the exec syscall returning to the old image's frames `unwind`s them and
// `call` enters the new `_start`. clone: the new task's worker starts with the parent's
// image (its memory shared for a thread, copied for a process) and `switch_entry` names
// the function it enters.
import { Halt } from './imports.mjs';

const ENOEXEC = 8;
const ENOMEM = 12;
const EFAULT = 14;
const EINVAL = 22;
const ENOSYS = 38;
const PAGE = 65536;

/** `memory`'s buffer reaching to `end`: a shared memory another thread grew shows its
 * old length here until it is looked at again (grow(0)). False when it does not. */
export function reaches(memory, end) {
  if (end <= memory.buffer.byteLength) return true;
  memory.grow(0);
  return end <= memory.buffer.byteLength;
}

/** Thrown through a replaced image's frames, back to `call`. */
const RESTART = Symbol('restart at the new image');

/** The memory a program imports ({minimum, maximum, shared}), or null: one memory,
 * `env.memory`, shared, with a maximum (its threads share it; it is made to that size). */
export function programMemory(bytes) {
  let at = 8;
  const leb = () => {
    let result = 0;
    let shift = 0;
    for (;;) {
      const byte = bytes[at++];
      if (byte === undefined) throw new RangeError('cut short');
      result += (byte & 0x7f) * 2 ** shift;
      if (!(byte & 0x80)) return result;
      shift += 7;
    }
  };
  const text = () => {
    const length = leb();
    const value = new TextDecoder().decode(bytes.subarray(at, at + length));
    at += length;
    return value;
  };
  let found = null;
  while (at < bytes.length) {
    const id = bytes[at++];
    const size = leb();
    const end = at + size;
    if (id === 5 && leb() > 0) return null; // a memory of its own
    if (id === 2) {
      for (let count = leb(); count > 0; count--) {
        const module = text();
        const name = text();
        const kind = bytes[at++];
        if (kind === 0) leb();
        else if (kind === 1) { at++; const flags = leb(); leb(); if (flags & 1) leb(); }
        else if (kind === 3) at += 2;
        else if (kind === 4) { at++; leb(); }
        else if (kind === 2) {
          const flags = leb();
          const minimum = leb();
          const maximum = flags & 1 ? leb() : null;
          if (found || module !== 'env' || name !== 'memory' || flags & 4 || !(flags & 2) || maximum === null) return null;
          found = { minimum, maximum, shared: true };
        } else return null;
      }
    }
    at = end;
  }
  return found;
}

/** What a program may import: its memory, and the kernel's calls for programs. */
const LINUX = new Set(['syscall', 'get_thread_area', 'copy_siginfo']);
function importsAllowed(module) {
  return WebAssembly.Module.imports(module).every(({ module: from, name, kind }) =>
    (from === 'env' && name === 'memory' && kind === 'memory') || (from === 'linux' && LINUX.has(name) && kind === 'function'));
}

/** A copy of `image`'s memory (a process cloned without sharing it): the same size and
 * limit, its own. */
export function copyImage(image) {
  const pages = image.memory.buffer.byteLength / PAGE;
  const memory = new WebAssembly.Memory({ initial: pages, maximum: image.maximum, shared: true });
  new Uint8Array(memory.buffer).set(new Uint8Array(image.memory.buffer));
  return { module: image.module, memory, maximum: image.maximum };
}

/** The `user` imports of one task's worker. `kernelMemory`: the machine's memory;
 * `kernel()`: this worker's kernel instance (made after these imports); `image`: the
 * process image the task starts with ({module, memory, maximum}, a clone's), or null. */
export function userImports(kernelMemory, kernel, image = null) {
  let current = image;
  let instance = null;
  let compiling = null;
  let pending = null;
  // Results of the siginfo copies of nested SA_SIGINFO handlers (null: not asked).
  const siginfo = [];

  const table = () => instance.exports.__indirect_function_table;
  const start = () => {
    instance.exports._start();
    throw new Error('_start returned without exiting');
  };
  let entry = start;

  /** `length` bytes at `at` of `memory`, or null when they are not all inside it. */
  const bytesOf = (memory, at, length) => (reaches(memory, at + length) ? new Uint8Array(memory.buffer, at, length) : null);
  /** Copies between memories; the bytes not copied (all, or none). */
  const copy = (toMemory, to, fromMemory, from, length) => {
    if (!length) return 0;
    const target = bytesOf(toMemory, to, length);
    const source = bytesOf(fromMemory, from, length);
    if (!target || !source) return length;
    target.set(source);
    return 0;
  };
  /** The user word at `at` (aligned, inside the process's memory) for atomics. */
  const word = (at) => {
    if (!current || at & 3 || !reaches(current.memory, at + 4)) return null;
    return new Int32Array(current.memory.buffer, at, 1);
  };
  const putKernelWord = (at, value) => {
    if (at & 3 || !reaches(kernelMemory, at + 4)) return false;
    new Int32Array(kernelMemory.buffer, at, 1)[0] = value;
    return true;
  };

  function instantiate(fresh) {
    if (fresh) {
      current = pending;
      pending = null;
    }
    const exports = kernel().exports;
    instance = new WebAssembly.Instance(current.module, {
      env: { memory: current.memory },
      linux: {
        syscall: exports.syscall,
        get_thread_area: exports.get_thread_area,
        copy_siginfo(to) {
          const result = exports.copy_siginfo(to);
          if (siginfo.length) siginfo[siginfo.length - 1] = result;
          return result;
        },
      },
    });
  }

  return {
    /** Makes the instance a clone starts with (before the kernel enters the task). */
    prepare() {
      if (current) instantiate(false);
    },
    imports: {
      compile_begin(size) {
        compiling = null;
        pending = null;
        try {
          compiling = new Uint8Array(size >>> 0);
          return 0;
        } catch {
          return -ENOMEM;
        }
      },
      compile_write(buf, offset, size) {
        const from = buf >>> 0;
        const to = offset >>> 0;
        const length = size >>> 0;
        if (!compiling || to + length > compiling.length || !reaches(kernelMemory, from + length)) return -EINVAL;
        compiling.set(new Uint8Array(kernelMemory.buffer, from, length), to);
        return 0;
      },
      compile_end(maximumPages) {
        const bytes = compiling;
        compiling = null;
        if (!bytes) return -EINVAL;
        let module;
        let wanted;
        try {
          wanted = programMemory(bytes);
          if (!wanted) return -ENOEXEC;
          module = new WebAssembly.Module(bytes);
          if (!importsAllowed(module)) return -ENOEXEC;
          const exported = new Set(WebAssembly.Module.exports(module).map(({ name }) => name));
          if (!exported.has('_start') || !exported.has('__indirect_function_table')) return -ENOEXEC;
        } catch {
          return -ENOEXEC;
        }
        const maximum = Math.min(wanted.maximum, maximumPages >>> 0);
        if (maximum < wanted.minimum) return -ENOMEM;
        try {
          pending = { module, memory: new WebAssembly.Memory({ initial: wanted.minimum, maximum, shared: true }), maximum };
        } catch {
          return -ENOMEM;
        }
        return 0;
      },
      compile_abort() {
        compiling = null;
        pending = null;
      },
      instantiate: (fresh) => instantiate(Boolean(fresh)),
      call() {
        for (;;) {
          try {
            entry();
          } catch (error) {
            if (error === RESTART) continue;
            // The kernel halting this worker (the task died) unwinds through here.
            if (error instanceof Halt) throw error;
            // A trap (unreachable, a bad access): the kernel ends the process.
            return;
          }
        }
      },
      switch_entry(fn, arg) {
        entry = () => {
          table().get(fn >>> 0)(arg);
          throw new Error("a thread's function returned without exiting");
        };
      },
      unwind() {
        entry = start;
        throw RESTART;
      },
      call_signal_handler(fn, sig) {
        table().get(fn >>> 0)(sig);
      },
      call_siginfo_handler(trampoline, fn, sig) {
        siginfo.push(null);
        try {
          table().get(trampoline >>> 0)(fn, sig);
          return siginfo.at(-1) ?? -EINVAL;
        } finally {
          try {
            kernel().exports.clear_siginfo();
          } finally {
            siginfo.pop();
          }
        }
      },
      halt_signal_handler() {},
      read: (to, from, length) => (current ? copy(kernelMemory, to >>> 0, current.memory, from >>> 0, length >>> 0) : length >>> 0),
      write: (to, from, length) => (current ? copy(current.memory, to >>> 0, kernelMemory, from >>> 0, length >>> 0) : length >>> 0),
      write_zeroes(to, length) {
        const target = current && bytesOf(current.memory, to >>> 0, length >>> 0);
        if (!target) return length >>> 0;
        target.fill(0);
        return 0;
      },
      futex_atomic_op(oldval, uaddr, op, oparg) {
        const target = word(uaddr >>> 0);
        if (!target) return -EFAULT;
        const operations = [
          () => Atomics.exchange(target, 0, oparg),
          () => Atomics.add(target, 0, oparg),
          () => Atomics.or(target, 0, oparg),
          () => Atomics.and(target, 0, ~oparg),
          () => Atomics.xor(target, 0, oparg),
        ];
        if (!operations[op]) return -ENOSYS;
        return putKernelWord(oldval >>> 0, operations[op]()) ? 0 : -EFAULT;
      },
      futex_atomic_cmpxchg(oldval, uaddr, expected, replacement) {
        const target = word(uaddr >>> 0);
        if (!target) return -EFAULT;
        return putKernelWord(oldval >>> 0, Atomics.compareExchange(target, 0, expected, replacement)) ? 0 : -EFAULT;
      },
    },
    /** The task's process image now (a clone of it takes it along). */
    get image() {
      return current;
    },
  };
}
