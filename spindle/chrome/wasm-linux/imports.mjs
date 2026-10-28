// What the wasm kernel imports from its host (arch/wasm/include/asm/wasm_imports.h),
// for one instance of it: the machine's own (the main thread, which boots it) or a
// task's (a worker each, all on the same shared memory). A task runs programs with the
// `user` imports it is given (user.mjs); the machine's own instance runs none. Devices
// (virtio.mjs) answer on the machine's own instance, where the kernel calls them.

const ENOEXEC = 8;
const ENOSYS = 38;

/** Thrown to unwind a worker the kernel is done with (halt_worker). */
export class Halt extends Error {}

const utf8 = new TextDecoder();
/** Text in shared memory (TextDecoder takes no shared view: copied first). */
export const textAt = (memory, at, length) => utf8.decode(new Uint8Array(memory.buffer, at >>> 0, length >>> 0).slice());

const origin = BigInt(Math.round(performance.timeOrigin * 1e6));
/** Unix time in nanoseconds, never going back. */
const now = () => origin + BigInt(Math.round(performance.now() * 1e6));

function refuse(name) {
  return () => {
    throw new Error(`the wasm Linux host has no ${name} yet`);
  };
}

/** No programs here (the machine's own instance): exec says the file is not one it runs. */
const NO_PROGRAMS = {
  compile_begin: () => -ENOEXEC,
  compile_write: () => -ENOEXEC,
  compile_end: () => -ENOEXEC,
  compile_abort() {},
  instantiate: refuse('programs'),
  call: refuse('programs'),
  switch_entry: refuse('programs'),
  call_signal_handler: refuse('programs'),
  call_siginfo_handler: () => -ENOSYS,
  halt_signal_handler: refuse('programs'),
  unwind: refuse('programs'),
  // copy_from_user, copy_to_user and clear_user: the bytes not copied (all of them).
  read: (to, from, length) => length >>> 0,
  write: (to, from, length) => length >>> 0,
  write_zeroes: (to, length) => length >>> 0,
  futex_atomic_op: () => -ENOSYS,
  futex_atomic_cmpxchg: () => -ENOSYS,
};

/** The import object for one kernel instance. `host` answers what reaches beyond it:
 * console(text), spawn(fn, arg, name, userMemory) -> 0 or -errno, main(fn, arg),
 * terminate(reason), `user` (a task's programs: user.mjs), and, on the machine's own
 * instance, devicetree, initramfs and `virtio` (its devices: virtio.mjs). */
export function kernelImports(memory, host) {
  return {
    env: { memory },
    boot: {
      get_devicetree(buf, size) {
        const tree = host.devicetree;
        if (!tree) throw new Error('only the machine boots the kernel');
        if (size >>> 0) new Uint8Array(memory.buffer).set(tree.subarray(0, size >>> 0), buf >>> 0);
        return tree.length;
      },
      get_initramfs(buf, size) {
        const image = host.initramfs ?? new Uint8Array(0);
        const length = Math.min(image.length, size >>> 0);
        new Uint8Array(memory.buffer).set(image.subarray(0, length), buf >>> 0);
        return length;
      },
    },
    kernel: {
      breakpoint() {},
      return_address: () => 0,
      halt_worker() {
        throw new Halt('halted');
      },
      terminate_machine: (reason) => host.terminate(reason),
      boot_console_write: (at, length) => host.console(textAt(memory, at, length)),
      boot_console_close() {},
      get_now_nsec: now,
      get_stacktrace(buf, size) {
        if (size >>> 0) new Uint8Array(memory.buffer)[buf >>> 0] = 0;
      },
      spawn_worker: (fn, arg, name, length, userMemory) => host.spawn(fn >>> 0, arg >>> 0, textAt(memory, name, length), userMemory),
      run_on_main: (fn, arg) => host.main(fn >>> 0, arg >>> 0),
    },
    user: host.user ?? NO_PROGRAMS,
    virtio: host.virtio ?? {
      set_features: refuse('devices'),
      setup: refuse('devices'),
      reset: refuse('devices'),
      enable_vring: refuse('devices'),
      disable_vring: refuse('devices'),
      notify: refuse('devices'),
    },
  };
}
