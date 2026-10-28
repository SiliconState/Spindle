// Spindle's wasm Linux as a runner: a machine booted on a 9p root (its image: busybox,
// the agent and the `spindle` command; guest/), kept running between commands. A task
// takes a signal only when it calls the kernel and holds its CPU until then, so a program
// computing without calls outlives SIGKILL: its agent says so (an END `lingering`), and
// that machine takes no more commands while a fresh one boots beside it; it ends once
// its own commands do, or after RESTART_WAIT_MS (those it cuts off run again on the
// fresh one, unless they made a Spindle call or ran past their own time). Commands run
// as on v86 (linux-worker.mjs): each over a copy of its
// thread's workspace on the root share (linux-workspace.mjs, unchanged: the share is
// v86's filesystem here too), several at once, each on CPUs of its own, the agent
// (guest/src/agent.rs) speaking agent.py's frames over a console of its own (hvc1; hvc0
// is the kernel's). The share is this side's: machines share it, and what it keeps of
// threads' workspaces outlives them.
//
// new WasmLinux({kernel, platform, root, cpus, memory, onCode, onStatus, log})
//   kernel: vmlinux.wasm's bytes; platform: machine.mjs's nodePlatform() or
//   browserPlatform(); root: the image as a v86 filesystem (ninep.mjs `filesystem`, or
//   one loaded from fs.json); cpus: CPU 0 is the agent's, and each command gets two of
//   the others (9: four commands at once, each on CPUs of its own; an idle CPU costs a
//   waiting worker); onCode(id, seq, request) answers a script's Spindle call (through
//   reply()).
import { base64 } from '../linux-frames.mjs';
import { Workspaces } from '../linux-workspace.mjs';
import { consoleDevice } from './console.mjs';
import { entropyDevice } from './entropy.mjs';
import { bootMachine } from './machine.mjs';
import { MOUNT_OPTIONS, ninepDevice } from './ninep.mjs';

/** Most output a command returns (the package keeps far less in the transcript). */
const MAX_OUTPUT = 4 * 1024 * 1024;
/** How long past its own timeout, or after a Stop, a command's END may take before its
 * machine counts as stuck (the agent ends a command within half a second, then syncs the
 * share, which a great deal just written makes slower). */
const GRACE_MS = 10_000;
/** How long a machine a command's processes outlived keeps running its other commands. */
const RESTART_WAIT_MS = 3000;
/** The longest a boot may take before it counts as failed. */
const BOOT_MS = 60_000;
/** While commands run, the agent is asked every PING_MS whether it is there; a machine
 * whose agent has not answered for SILENT_MS is stuck (its commands run again on the
 * next one), whatever the commands' own times. */
const PING_MS = 2000;
const SILENT_MS = 20_000;

const stopped = (failure) => ({ output: '', success: false, failure });

export class WasmLinux {
  constructor({ kernel, platform, root, cpus = 9, memory, onCode = () => {}, onStatus = () => {}, log = () => {} }) {
    Object.assign(this, { kernel, platform, root, cpus, memory, onCode, onStatus, log });
    this.workspaces = new Workspaces(root);
    /** The machine new commands run on: {machine, agent, listeners, gone, end, running,
     * retiring}, or null. */
    this.current = null;
    this.starting = null;
    /** Machines booted so far. */
    this.boots = 0;
    /** Command id -> the machine it runs on (where its scripts' replies go). */
    this.on = new Map();
  }

  /** The machine new commands run on, booted when there is none (the agent answering). */
  start() {
    this.starting ??= this.boot().catch((error) => {
      this.starting = null;
      throw error;
    });
    return this.starting;
  }

  async boot() {
    this.onStatus('booting');
    const listeners = new Set();
    const agent = agentConsole(listeners);
    let ended;
    const gone = new Promise((_, reject) => { ended = reject; });
    gone.catch(() => {});
    const started = Date.now();
    const machine = await bootMachine({
      kernel: this.kernel,
      platform: this.platform,
      cpus: this.cpus,
      ...(this.memory ? { memory: this.memory } : {}),
      devices: [consoleDevice({ output: (bytes) => this.log(new TextDecoder().decode(bytes)) }), agent.device, ninepDevice(this.root, 'root'), entropyDevice()],
      // Interrupts and the kernel's housekeeping on CPU 0, the agent's: no command runs
      // there (guest/src/agent.rs), and the others are full dynticks (spindle.config).
      cmdline: `console=hvc0 root=root rootfstype=9p rootflags=${MOUNT_OPTIONS} rw init=/sbin/init irqaffinity=0${this.cpus > 1 ? ` nohz_full=1-${this.cpus - 1}` : ''} quiet`,
      console: (text) => this.log(text),
    });
    const mine = { machine, agent, listeners, gone, end: ended, running: 0, retiring: false, timer: 0, over: false, heard: Date.now(), pings: 0 };
    machine.ended.then((outcome) => {
      ended(new Error(`the wasm Linux ended: ${outcome.error ?? 'it powered off'}`));
      this.forget(mine);
      clearTimeout(mine.timer);
      if (!mine.over) this.onStatus('ended', outcome.error ?? 'it powered off');
      mine.over = true;
    });
    const ready = waitFor(listeners, (kind, id) => kind === 'READY' && id === '0', BOOT_MS);
    try {
      await Promise.race([ready, gone]);
    } catch (error) {
      machine.stop();
      throw new Error(`the wasm Linux did not start: ${error.message}`);
    } finally {
      ready.cancel();
    }
    // The agent starting again on a running machine: what it ran will never be told.
    listeners.add((kind, id) => {
      if (kind === 'READY' && id === '0') this.end(mine, 'its agent restarted');
    });
    // Anything from the agent says it is there.
    listeners.add(() => {
      mine.heard = Date.now();
    });
    const heartbeat = setInterval(() => {
      if (mine.over) return clearInterval(heartbeat);
      if (!mine.running) {
        mine.heard = Date.now();
        return;
      }
      if (Date.now() - mine.heard > SILENT_MS) this.end(mine, `stopped answering (no word from its agent in ${SILENT_MS / 1000} s)`);
      else mine.agent.send('PING', String(++mine.pings));
    }, PING_MS);
    machine.ended.then(() => clearInterval(heartbeat));
    this.current = mine;
    this.boots += 1;
    this.onStatus('ready', `booted in ${Date.now() - started} ms`);
  }

  /** No new command goes to `mine` any more. */
  forget(mine) {
    if (this.current !== mine) return;
    this.current = null;
    this.starting = null;
  }

  /** Ends `mine` (every command on it fails, or runs again on the next machine). */
  end(mine, why) {
    if (!mine || mine.over) return;
    mine.over = true;
    this.onStatus('ended', why);
    this.forget(mine);
    clearTimeout(mine.timer);
    mine.end(new Error(`the wasm Linux ${why}`));
    mine.machine.stop();
  }

  /** Ends the machine new commands run on; the next command boots another. */
  restart(why = 'restarted') {
    this.end(this.current, why);
  }

  /** `mine`'s processes outlived a command's END: it takes no more commands, a fresh
   * machine boots for them, and it ends once its own are done, or after RESTART_WAIT_MS. */
  retire(mine) {
    if (mine.retiring) return;
    mine.retiring = true;
    this.forget(mine);
    this.start().catch(() => {});
    mine.timer = setTimeout(() => this.end(mine, 'ended what a command left running'), RESTART_WAIT_MS);
    this.settle(mine);
  }

  settle(mine) {
    if (mine.retiring && !mine.running) this.end(mine, 'ended what a command left running');
  }

  /** Runs one command (job: linux-worker.mjs's `run` message, with `stopped` kept up to
   * date by stop()): its copy of the workspace made (`fetchFiles(objects)` gives the
   * bytes of those the share lacks), the command run, what it changed read back. One cut
   * off by its machine's end runs again, once, on the next machine, unless it made a
   * Spindle call (journaled, maybe billed) or was stopped. */
  async run(job, fetchFiles) {
    let missed = 0;
    for (let attempt = 0; ; attempt++) {
      await this.start();
      if (job.stopped) return stopped('the command was stopped before it ran');
      const mine = this.current;
      // A machine gone between its boot and now (or before the command reached it): a
      // fresh one, a few times, without counting it as the command's one run again.
      if (!mine || mine.over || mine.retiring) {
        if (++missed > 4) throw new Error('the wasm Linux kept ending before the command reached it');
        attempt -= 1;
        continue;
      }
      job.sent = false;
      mine.running += 1;
      let result;
      try {
        result = await this.runOn(mine, job, fetchFiles);
      } finally {
        mine.running -= 1;
        this.settle(mine);
      }
      if (result.later) {
        // What it left running could still write to its copy: read once its machine
        // (and so all of that) is gone.
        const { copy, ...told } = result.later;
        this.retire(mine);
        await mine.gone.catch(() => {});
        try {
          return { ...told, ...(await this.workspaces.collect(copy)) };
        } catch (error) {
          return { ...told, success: false, failure: `the workspace it left was not kept: ${error.message}` };
        }
      }
      const { restarted, unsent, ...told } = result;
      if (unsent && ++missed <= 4) {
        attempt -= 1;
        continue;
      }
      if (restarted && !job.called && !job.stopped && attempt === 0) continue;
      return told;
    }
  }

  async runOn(mine, job, fetchFiles) {
    const { workspaces } = this;
    const plan = workspaces.plan(job.session, job);
    let copy;
    try {
      const bytes = plan.need.size ? await fetchFiles([...plan.need]) : new Map();
      if (job.stopped) {
        await workspaces.abandon(plan);
        return stopped('the command was stopped before it ran');
      }
      copy = await workspaces.build(plan, job, bytes);
    } catch (error) {
      await workspaces.abandon(plan);
      throw error;
    }
    if (job.stopped) {
      await workspaces.drop(copy);
      return stopped('the command was stopped before it ran');
    }
    // Its machine retired or ended while the copy was made: the copy goes, the command
    // goes to the next machine.
    if (mine.over || mine.retiring) {
      await workspaces.drop(copy);
      return { ...stopped('the wasm Linux restarted before the command ran'), unsent: true };
    }
    const chunks = [];
    let size = 0;
    const id = String(job.id);
    const collect = (kind, fid, data) => {
      if (fid !== id) return;
      if (kind === 'RPC') {
        const { seq, request } = JSON.parse(new TextDecoder().decode(data));
        job.called = true;
        this.onCode(job.id, seq, request);
        return;
      }
      // The agent sends one byte past the most kept: the mark that output was cut.
      if (kind !== 'OUT' || size > MAX_OUTPUT) return;
      chunks.push(data);
      size += data.length;
    };
    mine.listeners.add(collect);
    this.on.set(id, mine);
    const ended = waitFor(mine.listeners, (kind, fid) => kind === 'END' && fid === id, job.timeoutS * 1000 + GRACE_MS);
    ended.catch(() => {});
    // A Stop the agent does not answer in time: the machine is stuck.
    let stopTimer = 0;
    job.stopping = () => {
      mine.agent.send('STOP', id);
      stopTimer ||= setTimeout(() => this.end(mine, 'did not stop a command'), GRACE_MS);
    };
    let end;
    const sentAt = Date.now();
    try {
      mine.agent.send('RUN', id, JSON.stringify({ command: job.command, timeout_s: job.timeoutS, dir: copy.dir }));
      job.sent = true;
      if (job.stopped) job.stopping();
      end = JSON.parse(new TextDecoder().decode(await Promise.race([ended, mine.gone])));
    } catch {
      // Stuck (or ended for another command): every command on it ends, then its copy goes.
      this.end(mine, `stopped answering (command ${id}, ${Math.round((Date.now() - sentAt) / 1000)} s after it went)`);
      await workspaces.drop(copy);
      if (job.stopped) return stopped('the command was stopped');
      // Past its own time it is a timeout (most likely the very runaway that stalled the
      // machine), not a command to run again.
      if (job.sent && Date.now() >= sentAt + job.timeoutS * 1000) return stopped(`the command did not finish within ${job.timeoutS} s`);
      return { ...stopped('the wasm Linux stopped answering; it was restarted'), restarted: true };
    } finally {
      ended.cancel();
      mine.listeners.delete(collect);
      this.on.delete(id);
      clearTimeout(stopTimer);
      job.stopping = null;
    }
    const output = new TextDecoder().decode(concat(chunks).subarray(0, MAX_OUTPUT)) + (size > MAX_OUTPUT ? '\n[output cut at 4 MiB]' : '');
    if (end.timed_out || end.stopped) {
      // What it left running may still write to its copy until its machine is gone.
      if (end.lingering) {
        this.retire(mine);
        mine.gone.catch(() => {}).then(() => workspaces.drop(copy));
      } else {
        await workspaces.drop(copy);
      }
      return { output, success: false, failure: end.timed_out ? `the command did not finish within ${job.timeoutS} s` : 'the command was stopped' };
    }
    if (end.lingering) return { later: { output, success: end.code === 0, code: end.code, copy } };
    try {
      return { output, success: end.code === 0, code: end.code, ...(await workspaces.collect(copy)) };
    } catch (error) {
      return { output, success: false, failure: `the workspace it left was not kept: ${error.message}` };
    }
  }

  /** Stops a command (one not yet with the agent stops before it runs). */
  stop(job) {
    job.stopped = true;
    if (job.sent) job.stopping?.();
  }

  /** A script's Spindle call answered. */
  reply(id, seq, reply) {
    this.on.get(String(id))?.agent.send('REPLY', String(id), JSON.stringify({ seq, reply }));
  }
}

/** The agent's console: frames (STX, a kind, an id, base64, a line each) both ways; the
 * frames it sends go to `listeners` (kind, id, bytes). */
function agentConsole(listeners) {
  // A frame can run to tens of megabytes: what came before a chunk is not looked through
  // again for its end.
  let held = [];
  const decoder = new TextDecoder('latin1');
  const device = consoleDevice({
    output(bytes) {
      let chunk = decoder.decode(bytes, { stream: true });
      let at;
      while ((at = chunk.indexOf('\n')) >= 0) {
        const whole = held.join('') + chunk.slice(0, at);
        held = [];
        chunk = chunk.slice(at + 1);
        const start = whole.indexOf('\x02');
        if (start < 0) continue;
        const [kind, id, data] = whole.slice(start + 1).split(' ');
        let payload;
        try {
          payload = Uint8Array.from(atob(data ?? ''), (char) => char.charCodeAt(0));
        } catch {
          continue; // not a frame the agent wrote whole
        }
        for (const listener of [...listeners]) {
          try {
            listener(kind, id, payload);
          } catch (error) {
            console.warn('the wasm Linux sent a frame that did not read:', error);
          }
        }
      }
      if (chunk) held.push(chunk);
    },
  });
  return {
    device,
    send(kind, id, payload = '') {
      device.send(`\x02${kind} ${id} ${base64(new TextEncoder().encode(payload))}\n`);
    },
  };
}

/** The data of the first frame `want` takes, within `ms`; `cancel()` stops waiting. */
function waitFor(listeners, want, ms) {
  let timer = 0;
  let listener = null;
  const promise = new Promise((resolve, reject) => {
    timer = ms ? setTimeout(() => { listeners.delete(listener); reject(new Error('no answer')); }, ms) : 0;
    listener = (kind, id, data) => {
      if (!want(kind, id)) return;
      clearTimeout(timer);
      listeners.delete(listener);
      resolve(data);
    };
    listeners.add(listener);
  });
  promise.cancel = () => {
    clearTimeout(timer);
    listeners.delete(listener);
  };
  return promise;
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
