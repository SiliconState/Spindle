// The browser Linux: v86 running Alpine (chrome/linux), in a worker of its own so its
// CPU never stalls the engine. Booted once and saved (OPFS `linux/state-<identity>`);
// later starts resume that state. It runs several commands at once (one per thread at a
// time, and one whose thread has network), each over a copy of its thread's workspace on
// the 9p share (linux-workspace.mjs: made mostly of what the share kept of the thread's
// last copy; files it lacks are asked of the engine), and reports what changed, as a
// native Spindle runner does. Its network card reaches the web only while the command
// whose thread has network runs (linux-net.mjs): through Chrome, or the user's relay.
//
// engine -> worker  { type: 'run', id, session, command, timeoutS, network, files: [{path, size, executable, object}], links, directories }
//                   { type: 'files', id, files: [{object, bytes}] | error } (the bytes it asked for)
//                   { type: 'stop', id } | { type: 'code_reply', id, seq, reply } | { type: 'relay', url }
//                   { type: 'forget', session } (a thread gone: what the share keeps of it goes)
// worker -> engine  { type: 'ran', id, result } | { type: 'status', state, detail }
//                   | { type: 'need', id, objects } (files of its workspace the share lacks)
//                   | { type: 'code', id, seq, request }   (a script's Spindle call)
import { V86 } from './linux/libv86.mjs';
import { wireNetwork } from './linux-net.mjs';
import { readFrames as listen, sendFrame as frame } from './linux-frames.mjs';
import { Workspaces } from './linux-workspace.mjs';

const base = new URL('./linux/', import.meta.url).href;
const MEMORY = 512 * 1024 * 1024;
/** The machine's devices, as its saved state holds them: a state saved with others is
 * not resumed. */
const MACHINE = 'net1';
/** The network card's frames: large, so a TCP stream moves in few round trips. */
const MTU = 65520;
/** Most output a command returns (the package keeps far less in the transcript). */
const MAX_OUTPUT = 4 * 1024 * 1024;
/** How long past its own timeout a command may go before the Linux counts as stuck. */
const GRACE_MS = 20_000;
/** The longest a first boot may take before it counts as failed. */
const BOOT_MS = 180_000;
/** Commands running at once (one CPU: more mostly wait their turn on it). */
const MOST_RUNNING = 4;

const status = (state, detail = null) => postMessage({ type: 'status', state, detail });
let machine = null;
let starting = null;
/** The user's relay (wisp:// or wisps://), or null: the web through Chrome. A machine
 * keeps the one it started with (`machine.relay`); a change restarts it once no command
 * runs. */
let relay = null;
/** Commands waiting their turn, and those running (id -> job). */
const queue = [];
const running = new Map();
/** The running command whose thread has network (one at a time: the card's traffic is
 * that command's). */
let networked = null;
/** Files asked of the engine: run id -> resolve. */
const asked = new Map();

function options(saved, wired) {
  return {
    wasm_path: `${base}v86.wasm`,
    bios: { url: `${base}seabios.bin` },
    vga_bios: { url: `${base}vgabios.bin` },
    memory_size: MEMORY,
    vga_memory_size: 2 * 1024 * 1024,
    autostart: true,
    filesystem: { baseurl: `${base}flat/`, basefs: `${base}fs.json` },
    net_device: { type: 'virtio', relay_url: wired ?? 'fetch', mtu: MTU },
    // The guest keeps the card's address it was saved with (its own is set at boot).
    preserve_mac_from_state_image: true,
    ...(saved
      ? { initial_state: { buffer: saved } }
      : {
          bzimage_initrd_from_filesystem: true,
          cmdline: 'rw root=host9p rootfstype=9p rootflags=trans=virtio,cache=loose,msize=262144 modules=virtio_pci,virtio_net tsc=reliable quiet',
        }),
  };
}

function waitFor(listeners, want, ms) {
  return new Promise((resolve, reject) => {
    const timer = ms ? setTimeout(() => { listeners.delete(listener); reject(new Error('no answer')); }, ms) : null;
    const listener = (kind, id, data) => {
      if (!want(kind, id)) return;
      clearTimeout(timer);
      listeners.delete(listener);
      resolve(data);
    };
    listeners.add(listener);
  });
}

async function stateFile(identity, create) {
  const root = await navigator.storage.getDirectory();
  const dir = await root.getDirectoryHandle('linux', { create: true });
  if (create) {
    // States of other images are dead weight.
    for await (const name of dir.keys()) if (name !== `state-${identity}-${MACHINE}`) await dir.removeEntry(name).catch(() => {});
  }
  return dir.getFileHandle(`state-${identity}-${MACHINE}`, { create });
}

/** The machine: resumed from its saved state, or booted (and saved) the first time. */
function start() {
  starting ??= (async () => {
    const wired = relay;
    const identity = (await (await fetch(`${base}identity`)).text()).trim();
    let saved = null;
    try {
      saved = await (await (await stateFile(identity, false)).getFile()).arrayBuffer();
    } catch {}
    if (saved) {
      status('resuming');
      const vm = new V86(options(saved, wired));
      await new Promise((done) => vm.add_listener('emulator-ready', done));
      const listeners = listen(vm);
      // The agent answers a ping once the resumed machine takes input (pings count from
      // 1: READY 0 is the agent starting).
      for (let attempt = 0; ; attempt++) {
        frame(vm, 'PING', attempt + 1);
        try {
          await waitFor(listeners, (kind) => kind === 'READY', 2000);
          break;
        } catch {
          if (attempt >= 15) {
            vm.destroy();
            await (await navigator.storage.getDirectory()).getDirectoryHandle('linux').then((dir) => dir.removeEntry(`state-${identity}-${MACHINE}`)).catch(() => {});
            throw new Error('the saved Linux did not answer; it boots afresh next time');
          }
        }
      }
      machine = { vm, listeners, relay: wired, net: wire(vm, wired), workspaces: new Workspaces(vm.fs9p), ...lifetime() };
      watchAgent(machine);
    } else {
      status('booting', 'once, under a minute');
      const vm = new V86(options(null, wired));
      const listeners = listen(vm);
      try {
        await waitFor(listeners, (kind) => kind === 'READY', BOOT_MS);
      } catch {
        vm.destroy();
        throw new Error('the Linux did not start');
      }
      status('saving');
      const state = await vm.save_state();
      const writable = await (await stateFile(identity, true)).createWritable();
      await writable.write(state);
      await writable.close();
      machine = { vm, listeners, relay: wired, net: wire(vm, wired), workspaces: new Workspaces(vm.fs9p), ...lifetime() };
      watchAgent(machine);
    }
    status('ready');
  })().catch((error) => {
    starting = null;
    throw error;
  });
  return starting;
}

/** The machine's network (a VM it cannot be wired to is let go of). */
function wire(vm, wired) {
  try {
    return wireNetwork(vm, { relay: wired, allowed: () => Boolean(networked) });
  } catch (error) {
    vm.destroy();
    throw error;
  }
}

/** The agent starting again on a running machine (it failed and was restarted): the
 * commands it ran will never be told, and what they left may run on, so the machine
 * restarts. */
function watchAgent(mine) {
  mine.listeners.add((kind, id) => {
    if (kind === 'READY' && id === '0' && machine === mine) restart();
  });
}

/** A machine's end: `gone` fails when it is let go of (`end`), so what waits on it stops. */
function lifetime() {
  let end;
  const gone = new Promise((_, reject) => { end = reject; });
  gone.catch(() => {});
  return { gone, end };
}

function restart() {
  machine?.end(new Error('the browser Linux restarted'));
  machine?.net.end({ hard: true });
  machine?.vm.destroy();
  machine = null;
  starting = null;
}

/** Runs one command: its copy of the workspace made, the command run, what it changed
 * read back. */
async function run(job) {
  if (!machine) await start();
  if (job.stopped) return { output: '', success: false, failure: 'the command was stopped before it ran' };
  const mine = machine;
  const { vm, listeners, workspaces } = mine;
  const plan = workspaces.plan(job.session, job);
  let copy;
  try {
    const bytes = plan.need.size ? await fetchFiles(job, [...plan.need]) : new Map();
    if (job.stopped) {
      await workspaces.abandon(plan);
      return { output: '', success: false, failure: 'the command was stopped before it ran' };
    }
    copy = await workspaces.build(plan, job, bytes);
  } catch (error) {
    await workspaces.abandon(plan);
    throw error;
  }
  if (job.stopped) {
    await workspaces.drop(copy);
    return { output: '', success: false, failure: 'the command was stopped before it ran' };
  }

  const chunks = [];
  let size = 0;
  const collect = (kind, fid, data) => {
    if (fid !== String(job.id)) return;
    if (kind === 'RPC') {
      // A script's Spindle call: the engine answers it (code_reply).
      const { seq, request } = JSON.parse(new TextDecoder().decode(data));
      postMessage({ type: 'code', id: job.id, seq, request });
      return;
    }
    // The agent sends one byte past the most kept: the mark that output was cut.
    if (kind !== 'OUT' || size > MAX_OUTPUT) return;
    chunks.push(data);
    size += data.length;
  };
  listeners.add(collect);
  const ended = waitFor(listeners, (kind, fid) => kind === 'END' && fid === String(job.id), job.timeoutS * 1000 + GRACE_MS);
  // Waited on below unless the RUN frame never went (its machine is then restarted).
  ended.catch(() => {});
  let end;
  try {
    await frame(vm, 'RUN', job.id, JSON.stringify({
      command: job.command, timeout_s: job.timeoutS, dir: copy.dir,
      network: job.network ? (mine.relay ? 'relay' : 'chrome') : null, now: Date.now() / 1000,
    }));
    // A long command goes as a file first: a Stop meanwhile goes once it is there.
    job.sent = true;
    if (job.stopped) frame(vm, 'STOP', job.id);
    end = JSON.parse(new TextDecoder().decode(await Promise.race([ended, mine.gone])));
  } catch {
    listeners.delete(collect);
    // Stuck (or restarted for another command that was): every command on it ends.
    if (machine === mine) restart();
    return { output: '', success: false, failure: 'the browser Linux stopped answering; it was restarted' };
  }
  listeners.delete(collect);
  const output = new TextDecoder().decode(concat(chunks).subarray(0, MAX_OUTPUT)) + (size > MAX_OUTPUT ? '\n[output cut at 4 MiB]' : '');
  if (end.timed_out || end.stopped) {
    await workspaces.drop(copy);
    return { output, success: false, failure: end.timed_out ? `the command did not finish within ${job.timeoutS} s` : 'the command was stopped' };
  }
  // What the command left, against what it was given (as a native runner reports).
  try {
    return { output, success: end.code === 0, ...(await workspaces.collect(copy)) };
  } catch (error) {
    return { output, success: false, failure: `the workspace it left was not kept: ${error.message}` };
  }
}

/** The bytes of `objects` (files of the job's workspace the share lacks), from the engine. */
function fetchFiles(job, objects) {
  return new Promise((resolve, reject) => {
    asked.set(job.id, ({ files, error }) => {
      if (error) reject(new Error(error));
      else resolve(new Map(files.map((file) => [file.object, new Uint8Array(file.bytes)])));
    });
    postMessage({ type: 'need', id: job.id, objects });
  });
}

function concat(chunks) {
  const all = new Uint8Array(chunks.reduce((total, chunk) => total + chunk.length, 0));
  let at = 0;
  for (const chunk of chunks) {
    all.set(chunk, at);
    at += chunk.length;
  }
  return all;
}

/** Starts what may run now: commands of threads with none running, one with network at
 * a time, a few in all; a relay changed waits for the running to end, then restarts. */
function schedule() {
  if (machine && machine.relay !== relay) {
    if (running.size) return;
    restart();
  }
  for (let at = 0; at < queue.length && running.size < MOST_RUNNING;) {
    const job = queue[at];
    const busy = [...running.values()].some((other) => other.session === job.session) || (job.network && networked);
    if (busy) {
      at += 1;
      continue;
    }
    queue.splice(at, 1);
    running.set(job.id, job);
    if (job.network) networked = job;
    run(job).catch((error) => ({ output: '', success: false, failure: `the browser Linux could not run it: ${error.message}` }))
      .then((result) => {
        running.delete(job.id);
        asked.delete(job.id);
        if (networked === job) {
          networked = null;
          // Its connections end with it (its processes have).
          machine?.net.end();
        }
        postMessage({ type: 'ran', id: job.id, result }, result.writes?.map((write) => write.content.buffer) ?? []);
        schedule();
      });
  }
}

self.onmessage = ({ data }) => {
  if (data.type === 'run') {
    queue.push({ ...data, stopped: false, sent: false });
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
    if (!job) return;
    job.stopped = true;
    if (job.sent && machine) frame(machine.vm, 'STOP', data.id);
  } else if (data.type === 'code_reply') {
    if (machine?.vm) {
      frame(machine.vm, 'REPLY', data.id, JSON.stringify({ seq: data.seq, reply: data.reply }))
        .catch((error) => console.warn('a reply did not reach the browser Linux:', error));
    }
  } else if (data.type === 'relay') {
    relay = data.url ?? null;
    schedule();
  } else if (data.type === 'forget') {
    const under = [...running.values(), ...queue].some((job) => job.session === data.session);
    machine?.workspaces.forget(data.session, under).catch(() => {});
  } else if (data.type === 'start') {
    start().catch((error) => status('failed', error.message));
  }
};
