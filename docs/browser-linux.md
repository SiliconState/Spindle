# Browser Linux

Spindle for Browser runs a thread's `bash` commands in a Linux that runs inside the extension
(see [chrome.md](chrome.md)). The extension can carry two of them. The **v86 Linux** is 32-bit
Alpine Linux emulated by v86, with bash, Python, Node, git and network access through Chrome. The
optional **wasm Linux** is the Linux kernel compiled to WebAssembly and run without an emulator,
with busybox `sh` and Python and no network. For each command the engine decides which Linux
runs it, and it reruns a command on v86 when the wasm Linux turns out to lack something. Every
command runs as an unprivileged user in namespaces of its own, over a copy of its thread's
workspace that is synced through a 9p share. A command whose thread has network reaches the web
through a gateway in the extension that makes requests with Chrome's `fetch`, or through a WISP
relay the user runs. Scripts in either Linux can ask Spindle for model calls, tool calls and HTTP
requests through the `spindle` command and its Python and Node modules. Each of these calls is
journaled by the embedded package, as native code mode journals its calls.

## Contents

- [The two Linuxes](#the-two-linuxes)
- [Routing a command](#routing-a-command)
- [Running a command](#running-a-command)
- [Per-command isolation](#per-command-isolation)
- [Workspace sync](#workspace-sync)
- [Network](#network)
- [The code bridge](#the-code-bridge)
- [Boot, resume and idle](#boot-resume-and-idle)
- [Runner identity](#runner-identity)
- [Building the images](#building-the-images)
- [Tests](#tests)

## The two Linuxes

| | v86 Linux | wasm Linux |
| --- | --- | --- |
| What it is | Alpine Linux, 32-bit x86, run by the v86 emulator (compiled to wasm) | Linux for the `wasm` architecture (`arch/wasm`), compiled to WebAssembly; one kernel instance per task, each in a worker, all on one shared memory |
| Sources | [`chrome/linux/`](../spindle/chrome/linux/), [`linux-worker.mjs`](../spindle/chrome/linux-worker.mjs), [`linux-net.mjs`](../spindle/chrome/linux-net.mjs), [`linux-frames.mjs`](../spindle/chrome/linux-frames.mjs) | [`chrome/wasm-linux/`](../spindle/chrome/wasm-linux/), [`wasm-linux-worker.mjs`](../spindle/chrome/wasm-linux-worker.mjs) |
| Shell and programs | bash, coreutils, findutils, grep, sed, gawk, diffutils, patch, tar, gzip, xz, Python 3 with pip (and, since 0.1.10, Pillow), Node with npm, git, curl, openssl, jq, ripgrep, file, less; since 0.1.10 the time zones (Alpine's tzdata, for Python's `zoneinfo` and `TZ=...`), and without what no command here can use: `git-shell` (an SSH login shell), git's WebDAV push (`git-http-push`; pushes over HTTPS go through `git-remote-http`) and Python's `turtle` (it needs tkinter, which neither Linux has) | busybox (with hush as `sh`), Python 3.14 since 0.1.10, the v86 Linux's version with Alpine's own standard library files (3.13 with its own library in 0.1.8) (no `ssl`, `ctypes`, `lzma` or pip, and no loadable C extensions: there is no `dlopen`), and since 0.1.10 ripgrep, the v86 Linux's version without PCRE2 (`rg -P` runs on v86); since 0.1.10 the same time zones; only the programs the image was built with run (see [Programs a command saved](#programs-a-command-saved)) |
| Network | Through Chrome or a relay, for threads with network on | None |
| Agent | [`guest/agent.py`](../spindle/chrome/linux/guest/agent.py) with the static [`spindle-sandbox`](../spindle/chrome/linux/command/src/sandbox.rs) | [`guest/src/agent.rs`](../spindle/chrome/wasm-linux/guest/src/agent.rs) (`spindle-agent`, agent and sandbox in one program) |
| Agent channel | Serial line `/dev/ttyS0`; payloads over 16 KiB go as files on the share | Virtio console `/dev/hvc1` (`hvc0` is the kernel's) |
| Files | `linux/fs.json` (the index); the files in `linux/flat/`, named by digest, each compressed with zstd since 0.1.10 | `wasm-linux/fs.json`; since 0.1.10 its files in the same `linux/flat/` (what it shares with v86 is carried once) and its kernel as `wasm-linux/vmlinux.wasm.zst`, decompressed with v86's own zstd decoder ([`zstd.mjs`](../spindle/chrome/wasm-linux/zstd.mjs)) |
| Start | First start boots and saves the machine state to OPFS (since 0.1.10, in the background right after an install, or an update that brings a new image); later starts resume it | Boots every time; nothing is saved |
| Concurrency | Up to 4 commands, one per thread, one with network | Up to 4 commands, one per thread, each pinned to two CPUs of its own (9 CPUs in all) |
| Carried | Always (`chrome/build.sh` refuses to build without it) | When built with `SPINDLE_WASM_LINUX`: in releases since 0.1.10, the image [`image.json`](../spindle/chrome/wasm-linux/image.json) pins (see [Building the images](#building-the-images)); 0.1.8 and earlier releases did not carry it |

Both Linuxes use the same workspace code ([`linux-workspace.mjs`](../spindle/chrome/linux-workspace.mjs))
over a v86 9p filesystem, and both agents speak the same frame protocol and the same code-bridge
protocol. The `spindle` command is built from the same source for both.

Commands reach the Linuxes through the runner that [`linux-runner.mjs`](../spindle/chrome/linux-runner.mjs)
returns (`linuxRunner`). The engine connects it to its `EmbeddedClient` as the runner for
`runner: 'app'` threads (see [embedded.md](embedded.md) and [clients.md](clients.md)).

### Measured speed

The wasm Linux runs at the machine's own speed with no emulator in between, which is why
routing sends it every command it can run. [`linux_bench.mjs`](../spindle/chrome/tests/linux_bench.mjs)
runs the same commands on both Linuxes through `linuxRunner` with a forced route, in headless
Chrome 153, and keeps the median of five runs (the first command is timed once). Measured on
0.1.10 (v86 on its own trimmed kernel with no boot image, Python 3.14 on both):

| Workload | v86 Linux | wasm Linux | Faster by |
| --- | ---: | ---: | ---: |
| Shell: `true`, a file write and read, a directory listing piped to `wc`, a sort pipeline, 30 program starts, 200 files, an `awk` loop | varies by command | varies by command | 1.9x to 9.2x |
| `python3 -c pass` | 447 ms | 121 ms | 3.7x |
| Python importing `json`, `re`, `collections`, `datetime`, `pathlib`, `subprocess` | 1,288 ms | 189 ms | 6.8x |
| Python summing 3 million squares | 8,391 ms | 336 ms | 25.0x |
| Python writing 200 files | 840 ms | 343 ms | 2.4x |
| First start (v86 boots and saves its state once per image) | 15.3 s (21 s in 0.1.8) | 0.65 s | about 24x |

v86 resumes its saved state on later starts, so the last row is a one-time cost, and since 0.1.10
it is paid in the background right after an install or an update that brings a new image (see
[Boot, resume and idle](#boot-resume-and-idle)). What the wasm Linux cannot run (bash, Node,
git, the network, C extensions, programs a command saved) stays on v86.

## Routing a command

`linuxRunner().run` decides where each command runs. The user has no switch for it.

1. If the extension carries the wasm Linux (its `wasm-linux/identity` and `wasm-linux/fs.json`
   load), `wasmCanRun(command, programs)` accepts the command, and `needsV86` finds no reason to
   keep it on v86 (see [What keeps a command on v86](#what-keeps-a-command-on-v86)), it runs on
   the wasm Linux. `programs` is the set of names in the image's `bin`, `sbin`, `usr/bin`,
   `usr/sbin` and `usr/local/bin`, read from `fs.json`; for a thread with network on, busybox's
   applets that reach the network (`wget`, `nc`, `nslookup` and the rest of `NETWORK_PROGRAMS`)
   are left out. Through 0.1.11, every command of a thread with network ran on v86.
2. Otherwise it runs on v86.
3. After a wasm run, `wantsV86(result, {network, compiled})` decides whether to run the command
   again on v86. A command the user stopped is never rerun.

```mermaid
flowchart TD
  cmd(["A bash command of a runner: app thread"]) --> carried{"wasm Linux carried<br/>and not disabled?"}
  carried -- no --> v86["Run on v86"]
  carried -- yes --> parse{"wasmCanRun: plain POSIX sh<br/>and every program in the image?"}
  parse -- no --> v86
  parse -- yes --> needs{"needsV86: a kept directory named,<br/>or network use when the thread has network?"}
  needs -- yes --> v86
  needs -- no --> wasm["Run on the wasm Linux"]
  wasm --> stopped{"Stopped by the user?"}
  stopped -- yes --> result(["Result to the package"])
  stopped -- no --> wants{"wantsV86 on the result"}
  wants -->|"no: success, a Spindle call was made,<br/>timeout or other failure"| result
  wants -->|"yes: wasm Linux unavailable, program not found or saved,<br/>sh syntax error, GNU-only option, missing Python module,<br/>missing network, compiled module"| v86
  v86 --> result
```

### What the wasm Linux accepts

[`linux-route.mjs`](../spindle/chrome/linux-route.mjs) tokenizes the command cautiously. It
sends the command to v86 if any of the following holds:

- the command does not tokenize, for example because a quote is left open;
- it uses bash-only syntax: `[[ ]]`, `(( ))`, process substitution, `<<<`, `&>`, `|&`, `${!x}`,
  arrays (`a=(...)`, `${a[i]}`), `${x^}` and `${x,}`, brace expansion, `$'...'` and `$"..."`, or
  words such as `function`, `select`, `coproc`, `shopt`, `declare`, `typeset`, `mapfile`,
  `source`, `let`, `pushd`;
- it uses a tilde prefix, which hush does not expand;
- a command name is not a hush builtin and not one of `programs`, including commands inside
  `$( )`, backticks and unquoted heredoc bodies;
- a command name is built at run time (from an expansion), or the command uses `eval`;
- a wrapped command is unknown. `env`, `nice`, `nohup`, `time`, `exec`, `stdbuf`, `timeout`,
  `xargs`, `find -exec`/`-execdir`/`-ok`/`-okdir`, `busybox APPLET`, `command`, and `sh -c SCRIPT`
  (also `ash`, `hush`, `dash`) are looked through to the command they run.

### What keeps a command on v86

`needsV86(command, files, read, {network})` (since 0.1.12) keeps a command on v86 that the wasm
Linux could run. It reads the command (heredoc bodies and `python3 -c` included) and the
workspace's Python the command reaches: the scripts it names (`x.py`, `./x.py`,
`/workspace/x.py`, `python3 -m pkg.mod`), the modules its own Python imports, and what those
import in turn from the workspace, found by name anywhere in it (relative imports by their
place). It reads at most 256 files and 16 MiB, each file once per object digest, and a command
that reaches further, or a file it cannot read, stays on v86. It returns true when:

- the command or a script it reaches names a package or cache directory that a later command
  may use (`.venv`, `venv` or `*-venv`, `node_modules`, `.cache`, `.npm`, `.tox`, `.nox`,
  `.ipynb_checkpoints`: what `ignored` in
  [`linux-workspace.mjs`](../spindle/chrome/linux-workspace.mjs) leaves out of the workspace). Each Linux
  keeps its own copy of these between commands and none of them is a workspace file, so what
  pip, npm or curl put there on v86 is not in the wasm Linux's copy. Python's and its tools' own
  caches (`__pycache__`, `.pytest_cache`, `.mypy_cache`) do not count, since only the program
  that made one reads it;
- the thread has network on and the command or a script it reaches may use it
  (`NETWORK_CODE`): it imports a module that connects (`socket`, `ssl`, `urllib.request`,
  `http.client`, `requests`, `httpx`, `urllib3` and others), calls `urlopen`,
  `create_connection`, `getaddrinfo` or the like, names a program that connects in quotes (for
  `subprocess` or `os.system`: `curl`, `wget`, `git`, `pip`, `ssh` and others), or runs
  `-m pip`. In a package pip installed into the workspace (a directory holding its
  `*.dist-info`, as `pip install --target .packages` makes), only the modules it imports count
  (`NETWORK_IMPORTS`), since its text names programs and calls in data and documentation.

The static check matters for a script that would catch the wasm Linux's failed connection and
carry on with a fallback. A network use it does not see fails loudly on the wasm Linux and
runs again on v86 (next section), after the wasm Linux's name lookup gives up (about 5 s).

### When a wasm result is run again on v86

`wantsV86` returns false when the run succeeded or a script in it made a Spindle call
(`called`). That call is journaled and may have been billed, so the command's result stands.
It also returns false for any other runner failure, such as a timeout or a stop. It returns true
when:

| Result | Example |
| --- | --- |
| `unavailable` | the wasm Linux could not start, the command never reached its agent, or the wasm worker failed |
| exit 127 with "not found" or "applet not found", or `can't execute '...': No such file or directory` | a program the image lacks |
| exit 126 with "Permission denied" or "Exec format error" (also as `can't execute '...'`) | a program a command saved, or a binary built for another machine |
| `sh: ... syntax error` | syntax hush does not take |
| `unrecognized option`, `invalid option`, or a BusyBox usage text | `grep -P`, `find -printf` |
| `date: invalid date` | `date -d yesterday` |
| `ModuleNotFoundError: No module named` | a module only v86's Python has, or one pip would install there |
| `python3: No module named` (since 0.1.10) | `python3 -m pip`, `python3 -m pytest`: pip and what it installs are v86's |
| `Error: Command '[...'-m', 'ensurepip'...` (since 0.1.10) | `python3 -m venv .venv`: a venv's pip can only come on v86, which makes the venv afresh |
| `PCRE2 is not available in this build of ripgrep` (since 0.1.10) | `rg -P`, or `rg --engine auto` with a pattern only PCRE2 takes |
| `MemoryError`, `memory allocation of N bytes failed`, or `...: out of memory` (since 0.1.10) | a program that needed more than the 256 MiB each wasm Linux program may have; v86's machine has 512 MiB |
| Python `subprocess` raising `FileNotFoundError` or `PermissionError` | a program Python tried to start that the image lacks or may not run |
| with network on (since 0.1.12): `[Errno -3] Try again`, `[Errno 101] Network unreachable`, `unknown url type: https`, `SSL module is not available`, or a busybox network applet's `bad address` | a connection the wasm Linux, which has no network, could not make; nothing was sent, so the rerun is safe |
| in a workspace holding `.so` files (since 0.1.12): `ImportError` | a compiled module pip put in the workspace, which the wasm Linux's Python cannot load (`from . import _speedups` says `ImportError`; a module imported by its full name says `ModuleNotFoundError`, above) |

A rerun is safe because each command works on a copy of its thread's workspace, and that copy
counts only when its result is returned to the package (see [Workspace sync](#workspace-sync)).

### Programs a command saved

The wasm Linux runs only the programs its image was built with. What a command writes, in its
workspace, `/tmp`, `/var/tmp`, `/dev/shm` or its home, is data there and never a program: the
agent mounts each of them `noexec`, `nosuid` and `nodev` (`DATA_ONLY` in
[`agent.rs`](../spindle/chrome/wasm-linux/guest/src/agent.rs)), and `/sbin/init` mounts `/run`
the same way and writable by root alone. So nothing a command saved, or fetched on v86, is ever
compiled and run as WebAssembly by the extension. A command that names such a program directly
(`./tool`) goes to v86 from the start, since the name is not one of the image's programs. One
that starts it some other way (a Python script's `subprocess`, say) is refused there with exit
126 or a `PermissionError`, and runs again on v86. A script still runs when the image's own
`sh` or `python3` reads it (`sh build.sh`, `python3 tool.py`).

### When the wasm Linux is disabled

- The wasm worker reports `unavailable` when the browser does not let a worker share a
  `WebAssembly.Memory` with workers it starts and block on it (`sharesMemory`), or when the
  kernel or image cannot be fetched. Commands then go to v86.
- After 3 failures in a row (boots or worker errors), the runner stops sending commands to it
  (`WASM_FAILURES`).
- `linuxRunner({ route: 'v86' | 'wasm' })` forces every command to one Linux with no fallback.
  Tests and benchmarks use it; the extension uses `'auto'`.

## Running a command

The engine's runner posts `{type: 'run', id, session, command, timeoutS, network, files, links,
directories}` to the chosen worker. `files` lists the workspace (path, size, executable flag,
object digest); the worker asks for the bytes of files its share does not already hold with
`need`, and the runner reads them through the package (`runnerFile`). Each worker queues
commands and starts one when its thread has no other command running, at most 4 at a time and,
on v86, only one whose thread has network.

The runner and each worker exchange these messages:

| Direction | Message | Meaning |
| --- | --- | --- |
| runner to worker | `run` | A command, as above |
| runner to worker | `files` | The bytes a `need` asked for, or an error |
| runner to worker | `stop` | Stop a command (one still queued ends at once) |
| runner to worker | `code_reply` | The answer to a script's Spindle call |
| runner to worker | `forget` | A thread is gone; its kept workspace copy goes |
| runner to worker | `start` | Start or resume the Linux ahead of a command (`warm`) |
| runner to v86 worker | `relay` | The relay URL, or null for Chrome |
| worker to runner | `ran` | A command's result |
| worker to runner | `status` | The Linux's state and a detail |
| worker to runner | `need` | Objects of the workspace the share lacks |
| worker to runner | `code` | A script's Spindle call |

Each agent takes commands as frames, which are lines of the form `STX KIND ID BASE64`:

| Direction | Frame | Payload |
| --- | --- | --- |
| browser to agent | `RUN id` | JSON `{command, timeout_s, dir}`; on v86 also `network` (`'chrome'`, `'relay'` or null) and `now` (the browser's clock) |
| browser to agent | `STOP id`, `PING n` | none |
| browser to agent | `REPLY id` | JSON `{seq, reply}`: the answer to a script's Spindle call |
| agent to browser | `READY n` | `READY 0` when the agent starts; `READY n` answers `PING n` |
| agent to browser | `OUT id` | Output bytes (stdout and stderr together) |
| agent to browser | `RPC id` | JSON `{seq, request}`: a script's Spindle call |
| agent to browser | `END id` | JSON `{code, timed_out, stopped}`; the wasm agent adds `lingering` |

On v86, a payload over 16 KiB goes as a file in root's `/runs` on the 9p share, and its frame's
kind ends with `+` and names the file, because a much larger burst into the serial line wedges
it. The wasm Linux's console takes any size.

A command's result is its output, `success` (exit code 0), and what it changed in the workspace.
The browser keeps at most 4 MiB of output and appends `[output cut at 4 MiB]` beyond that. The
agent enforces the command's timeout (`timeout_s`). If no `END` arrives within the timeout plus a
grace period (20 s on v86, 10 s on the wasm Linux), the machine counts as stuck and is
restarted. A stop sends `STOP`; the agent then kills the command's PID-namespace init, which
ends every process the command started. A stopped or timed-out command's workspace copy is
dropped.

On v86, the agent sets the guest clock from `now` whenever it is more than a second off, because
a resumed machine wakes with the time it was saved at. The wasm kernel reads the browser's clock
directly (`get_now_nsec` in [`imports.mjs`](../spindle/chrome/wasm-linux/imports.mjs)).

## Per-command isolation

Each agent runs as root and starts a sandbox helper per command: on v86, `spindle-sandbox
WORKDIR SOCKET NETWORK COMMAND`
([`sandbox.rs`](../spindle/chrome/linux/command/src/sandbox.rs)), started with `posix_spawn`
because forking the Python agent costs far more in the emulator; on the wasm Linux, `spindle-agent
sandbox CPUS WORKDIR SOCKET COMMAND`. The helper:

| | v86 | wasm Linux |
| --- | --- | --- |
| Namespaces | mount, PID, IPC; network for threads without network | mount, PID, IPC, UTS, network |
| Network namespace | Without network: a fresh namespace with only loopback up, prepared while the agent is idle. With network: the Linux's own network | Always a fresh one with only loopback (the wasm Linux has no network card) |
| Mounts | the command's workspace copy bind-mounted at `/workspace`; fresh tmpfs at `/tmp`, `/var/tmp`, `/dev/shm` and the `spindle` user's home; its own `/proc`; a new `devpts` instance; its own Spindle socket at `/run/spindle.sock` | the same (tmpfs is ramfs on this no-MMU kernel), with the workspace and every scratch mount `noexec`, `nosuid` and `nodev` |
| User | `spindle` (uid and gid 1000), supplementary groups dropped | the same |
| Shell | `/bin/bash --noprofile --norc -o pipefail -c COMMAND` in `/workspace`, in a session of its own | `/bin/sh -c 'set -o pipefail; COMMAND'` in `/workspace`, pinned to two CPUs |
| Environment | `HOME` (the `spindle` user's home), `LANG=C.UTF-8`, `TERM=dumb`, `USER`, `LOGNAME`, `PATH`; plus the proxy variables for network through Chrome | the same, without proxy variables |

Only the workspace carries over from one command to the next. The command's first process in its
PID namespace reaps orphans; when the agent kills it, every process left in the namespace dies
with it. The parent-death signal is set, so if the helper is killed, the command goes too.

On the wasm Linux a task takes a signal only when it calls the kernel, so a program that computes
without system calls cannot be killed. The agent then reports `lingering: true` in `END`, and the
runner retires that machine: it takes no new commands, a fresh machine boots for them, and the
old one ends when its own commands finish or after 3 s. Commands it cuts off run again once on
the fresh machine, unless they made a Spindle call, were stopped, or had run past their own
timeout. CPU 0 keeps the agent, every sandbox helper and init, and the device interrupts
(`irqaffinity=0`; the others are `nohz_full`), and each command's shell is pinned to two other
CPUs, so a runaway holds only its own command's CPUs.

## Workspace sync

Both Linuxes keep a thread's workspace on their 9p share between commands
([`linux-workspace.mjs`](../spindle/chrome/linux-workspace.mjs), class `Workspaces`):

1. **Plan.** For the next command, files of the thread's kept copy whose digest, size and
   executable flag match the package's file list, and whose mode, owner and link count are
   still those of a fresh copy, are reused: by the same path first,
   then any kept file with the same content (a move or copy). The guest has not written to these
   files since they were kept. All other objects are asked for (`need`).
2. **Build.** A fresh directory `/runs/N` (root's `/runs` is mode 0700) is made for the command
   and owned by uid 1000. Reused files are moved into it by relinking their inodes, so nothing is
   copied and nothing the guest cached goes stale. New files are created from the bytes the
   engine sent. Symlinks and empty directories are created as listed. The kept copy's leftovers
   are removed.
3. **Run.** The agent bind-mounts `/runs/N` at `/workspace`. The share wraps v86's `Write` and
   `ChangeSize`, so every inode the guest writes to is recorded.
4. **Collect.** After the command, the copy is walked. Files that were placed and never written
   are unchanged and are not read back. Changed files are read, at most 4 at a time, and hashed
   with Spindle's object digest (`objectDigest`, as in `store/src/artifact.rs`). The result is
   `{writes, deletes, links, directories}`, the same shape a native runner reports. Sockets and
   pipes are ignored. The walk fails, and the copy is dropped, past a native runner's limits:
   64 MiB per file, 2 GiB in all, 50,000 entries.
5. **Keep.** The copy becomes the thread's kept copy. The share keeps at most 8 threads and
   256 MiB, and evicts the least recently used. Deleting a thread (`forget`) removes its copy.

A command's copy counts only when the package accepts its result. A copy dropped after a stop or
timeout, or a result that is not taken, leaves nothing stale, because the next plan reuses a kept
file only when its digest matches what the package lists. Since 0.1.10 a dropped copy (over the
limits, stopped, timed out, or never run) keeps its package and cache directories (`.venv`,
`node_modules`, `__pycache__` and the like) as the thread's kept copy, so an `npm install` cut off
by its time goes on from what it fetched; only a command that may still be running (one the v86
Linux could not end) loses them too.

## Network

A thread's network is off until the user turns it on (the thread's model page; see
[chrome.md](chrome.md#settings)). Only v86 has a network: a command of such a thread that may
use it runs there, and since 0.1.12 the thread's other commands run on the wasm Linux when it
can run them (see [Routing a command](#routing-a-command)). The v86 worker runs one
command with network at a time, and its network card passes traffic only while that command runs
(`allowed()` in `wireNetwork`). When the command ends, its connections are cut. A command without
network is in a network namespace with only loopback, so it cannot reach even the Linux's own
proxy.

Inside the guest, [`sysinit`](../spindle/chrome/linux/guest/sysinit) gives `eth0` the address
`192.168.86.100/24` with `192.168.86.1` (the extension) as router and DNS server. The v86 card
uses 65520-byte frames. [`linux-net.mjs`](../spindle/chrome/linux-net.mjs) wraps v86's TCP
connections (`steady`) so that each segment fits the window the guest last offered and is resent
if not acknowledged.

### Through Chrome

With no relay set, a command with network reaches HTTP and HTTPS sites, and Chrome makes every
request:

```mermaid
flowchart LR
  subgraph guest["v86 guest"]
    cmd["Command: curl, pip, git, Python, Node"]
    proxy["proxy.py on 127.0.0.1:3128<br/>HTTPS ends here with a certificate<br/>from the Linux's own CA"]
    nic["virtio NIC 192.168.86.100"]
  end
  subgraph worker["linux-worker.mjs"]
    stack["v86 network adapter"]
    gw["Gateway in linux-net.mjs<br/>192.168.86.1:3128 and port 80"]
  end
  fetch["Chrome fetch<br/>credentials omit, redirect manual"]
  site["Public website"]
  cmd -->|"http_proxy and https_proxy"| proxy
  proxy -->|"HTTP/1.1 requests,<br/>SPINDLE-ORIGIN line for HTTPS"| nic
  nic --> stack --> gw
  gw -->|"public http and https only"| fetch --> site
```

1. **Environment.** For a command with network through Chrome, the agent sets `http_proxy`,
   `https_proxy` (and upper-case forms) to `http://127.0.0.1:3128`, `no_proxy` for loopback,
   `NODE_USE_ENV_PROXY=1` (so Node's own `fetch` uses the proxy), `NODE_EXTRA_CA_CERTS`,
   `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` and `PIP_CERT`.
2. **The Linux's own CA.** At the first boot, `sysinit` generates a P-256 certificate authority
   (`CN=Spindle browser Linux`) and a site key in `/etc/spindle`. The keys are created under
   `umask 077`, so only root can read them (commands run as uid 1000). The CA certificate is
   appended to the system bundle, so commands trust it. The CA is made completely or not at all.
3. **The proxy** ([`proxy.py`](../spindle/chrome/linux/guest/proxy.py), root, restarted by init if
   it stops) passes plain-HTTP requests to the gateway at `192.168.86.1:3128` as they came. For
   `CONNECT host:port`, it answers `200`, ends TLS itself with a certificate for that host signed
   by the Linux's CA (made on demand, valid 397 days, written to `/run/spindle-certs` and reused while the
   proxy runs), sends
   `SPINDLE-ORIGIN https://host:port`, and then forwards the decrypted HTTP/1.1 requests. If the
   gateway refuses the connection, because no command with network is running, the proxy answers
   `403` and says the thread has no network.
4. **The gateway** (`serve` in `linux-net.mjs`) accepts connections to `192.168.86.1:3128`, or to
   port 80 of any address (plain HTTP, site taken from `Host`), and only while a command with
   network runs. It parses HTTP/1.1 (heads up to 64 KiB, bodies up to 64 MiB, chunked or not,
   `Expect: 100-continue`), drops hop-by-hop headers, and calls `fetch` with `credentials:
   'omit'`, `cache: 'no-store'`, `redirect: 'manual'` and `referrerPolicy: 'no-referrer'`. It
   streams the response back, pausing while more than 1 MiB waits for the guest.
5. **Refused addresses.** `local(hostname)` refuses, with `403`, names without a dot; names that are, or end in a dot and,
   `localhost`, `local`, `internal`, `lan`, `localdomain` or `home.arpa`; IPv4 `0/8`, `10/8`,
   `127/8`, `169.254/16`, `172.16/12`, `192.168/16`, `100.64/10` and `224/4` and above; and IPv6
   `::`-prefixed, `fc00::/7`, `fe80::/10` and `ff00::/8` addresses, NAT64 encodings of
   local IPv4 addresses, and benchmark range `198.18.0.0/15`. Before fetching a public
   name, `resolvesLocal` checks A and AAAA answers over Cloudflare DNS-over-HTTPS
   (Cloudflare sees the name); any returned local address is refused. Verdicts cache
   for 30 seconds to five minutes. Failed or slow lookups pass; a transport failure
   pauses checks for a minute. Chrome resolves the name again, so rebinding is still
   possible. This is a best-effort check, not a private-network firewall. A request to the browser's
   own extension store (Chrome's or Edge's) also gets a `403`, which says the browser never
   lets an extension reach it.
6. **Redirects** are never followed by the gateway. If Chrome hides a redirect's target
   (`opaqueredirect`), the gateway answers `502` and asks for the final address. If the target
   is visible (a non-browser `fetch`), it is passed to the client only when it is a public http(s)
   address.

The gateway works only for clients that use a proxy from the environment, or that speak plain
HTTP to port 80. Without the proxy, other ports and protocols (ssh, `git://`, databases)
are not reachable through Chrome. Requests carry no
cookies.

### Through a relay

When the user sets a relay (`wisp://HOST` or `wisps://HOST`, Settings, Relay), the v86 card is
wired to that WISP server instead of the gateway. Every TCP connection of a command with network
goes to the relay as it is, with the guest's own TLS, so any protocol over TCP works (ssh,
git over SSH, databases). Connections to the
guest's own subnet (`192.168.86.*`) are refused, and a connection whose unread data exceeds 64 MiB
is cut. Name lookups use DNS-over-HTTPS (cloudflare-dns.com) while a command with network runs; at other times they
get answers that lead nowhere. The agent sets no proxy variables for such a command. Changing
the relay restarts the v86 machine once no command is running.

### Sites scripts may call

`spindle http URL` does not use the network card. It is a Spindle call (see
[The code bridge](#the-code-bridge)) that the engine sends with `fetch` for sites the user listed
under Settings, Sites scripts may call. It therefore works whatever the thread's network setting. The package
checks each request against the grants (address prefix and methods), and the request carries no
cookies and follows no redirect. A site saved with a secret header gets that header added to
each such request; the script never sees its value, and echoes of it in the answer read
`[redacted]` (see [chrome.md](chrome.md#secret-headers-for-sites)).

## The code bridge

Scripts in either Linux can ask Spindle for calls, as native `run_code` scripts do (see
[tools.md](tools.md)).

```mermaid
sequenceDiagram
  participant S as Script in a command
  participant A as Agent in the guest
  participant W as Linux worker
  participant E as Engine EmbeddedClient
  participant P as spindle-embed package
  S->>A: JSON line on /run/spindle.sock
  A->>W: RPC frame with seq and request
  W->>E: code message
  E->>P: code_call: check and journal
  P-->>E: children to run
  E->>E: model fetch, app tool, or granted http
  E->>P: code_answer per child
  P-->>E: reply
  E->>W: code_reply
  W->>A: REPLY frame
  A->>S: JSON line
```

- **Socket.** The agent makes a Unix socket for each command under root's `/run/spindle-calls`
  and bind-mounts it at `/run/spindle.sock` in the command's namespace, so a command can reach
  only its own socket. Requests and replies are one JSON line each (`{"method", "input"}`).
- **Ordering.** The agent relays one call at a time per command. Calls from several processes
  wait their turn. It numbers the children (`model`, `tool`, `http`; each call in a `batch`
  counts) in the order they go, as native code mode numbers them.
- **Journaling.** The engine passes each request to `EmbeddedClient.code`. The package checks and
  journals it (`code_call`) before anything runs. The engine runs the children (a model request
  with the user's key, an app tool such as a page tool, or an HTTP request a grant allows) and
  records each answer (`code_answer`). A call that was cut off is never sent again by itself, and
  its error ends with `[may_have_run]`. When the command ends, its abort signal stops further
  calls, and children that never ran stay uncertain in the journal.
- **Limits per command** (`LIMITS` in [`store/src/embedded/code.rs`](../spindle/store/src/embedded/code.rs)):
  128 calls, 32 of them model calls, and up to 4 children of a `batch` at once (one at a time
  when it includes a tool call), within a nested-model allowance of 1,048,576 (input bytes plus
  requested output tokens). `spindle limits` prints them.
- **Agent bounds.** At most 64 connections per command. A request line may be up to 64 MiB on
  v86 and 16 MiB on the wasm Linux. A script that leaves too many replies unread is disconnected.

The interfaces:

| From | Calls |
| --- | --- |
| bash and any program: `spindle` ([`main.rs`](../spindle/chrome/linux/command/src/main.rs), a static Rust binary) | `spindle model [--provider P] [--model M] [--system S] [--max-tokens N] [--effort E] [--image FILE]... PROMPT` (prints the text; `-` reads the prompt from stdin); `spindle tool NAME [JSON]`; `spindle search [QUERY]`; `spindle describe NAME`; `spindle limits`; `spindle http [-X METHOD] [-H 'NAME: VALUE']... [-d BODY \| --json JSON] URL` (prints the body, fails on HTTP 400 and up); `spindle call METHOD [JSON]` (the raw reply) |
| Python: `import spindle` ([`spindle.py`](../spindle/chrome/linux/guest/spindle.py), both Linuxes) | `model(prompt, **options)`, `models(prompts, **options)`, `tool(name, args)`, `tools.search`/`describe`/`call`, `batch(calls)`, `limits()`, `http(url, **options)`, `image(path)`; failures raise `SpindleError` with `may_have_run` |
| Node: `require('spindle')` or `import ... from 'spindle'` ([`spindle.js`](../spindle/chrome/linux/guest/spindle.js), v86 only) | The same functions, synchronous, each call running `spindle call METHOD -` (`image` reads the file itself); failures throw an `Error` whose message ends with `[may_have_run]` when it may have run; installed at `/node_modules/spindle` so ES module imports and `node file.ts` find it |

Being a static binary, `spindle` makes a call cost only the round trip, with no interpreter
start. The Python module talks to the socket directly.

## Boot, resume and idle

The engine warms the wasm Linux (in every release since 0.1.10) when a browser-runner
thread is opened or created. It warms v86 then only if that thread used v86 in the last
day; a chat-only thread leaves v86 and its 512 MiB unstarted. A command that needs v86
starts it then.
Choosing This browser as a thread's runner explicitly warms both (`warm()`;
`warm({v86: false})` skips v86). The engine's `onRan` callback notes on the thread when it
last ran a command on v86. A Linux worker that has run nothing for 10 minutes is terminated
(`IDLE_MS`), giving its memory back. Sources:
[`engine-worker.mjs`](../spindle/chrome/engine-worker.mjs) and
[`linux-runner.mjs`](../spindle/chrome/linux-runner.mjs).

**v86** ([`linux-worker.mjs`](../spindle/chrome/linux-worker.mjs)):

- **Prepared ahead** (since 0.1.10). On an install or update, the background service worker
  looks in OPFS for this image's saved state (`linuxStateName` in
  [`shared.mjs`](../spindle/chrome/shared.mjs)); when there is none it asks the engine for
  `prepareLinux` (starting the engine's document), and the runner's `prepare()` has a v86
  worker make the state (a first start, below), then ends the worker and its memory unless a
  command or a warm-up came for it meanwhile. An update that kept the image starts nothing.
- **First start.** v86 boots its kernel (since 0.1.10 `linux/bzImage`, Spindle's own build with
  the 9p root and virtio built in, straight onto the 9p root with no boot image; in 0.1.8 the
  kernel and initramfs it found in the 9p root's `/boot`) (512 MiB of memory). Once
  the agent reports `READY 0` (within 180 s), the worker saves the machine state to OPFS as
  `linux/state-IDENTITY-net1` and removes states of other images. The agent waits for the proxy
  to listen and prepares a spare network namespace before it reports ready, so a resumed machine
  has both at once.
- **Later starts** resume that state and ping the agent (up to 16 tries, 2 s each). If the agent
  never answers, the state is deleted and the next start boots afresh.
- **Recovery.** A `READY 0` from a running machine means the agent restarted, so the machine is
  restarted. A command with no `END` past its grace period also restarts the machine, and every
  command on it fails with "the browser Linux stopped answering".

**wasm Linux** ([`wasm-linux/runner.mjs`](../spindle/chrome/wasm-linux/runner.mjs), `WasmLinux`):

- **Boot.** `bootMachine` ([`machine.mjs`](../spindle/chrome/wasm-linux/machine.mjs)) compiles
  `vmlinux.wasm` and reserves a shared memory of up to 4 GiB, halving the maximum until the host
  can reserve it. It builds a device tree (CPUs, memory, command line, virtio devices) and boots
  the kernel. The kernel runs on 9 CPUs with `console=hvc0 root=root rootfstype=9p ...
  irqaffinity=0 nohz_full=1-8`. Its devices are two virtio consoles, the 9p root share, and a
  virtio entropy source fed from `crypto.getRandomValues`. Every kernel task (a CPU, kernel
  thread, process or thread) runs in a worker of its own ([`task.mjs`](../spindle/chrome/wasm-linux/task.mjs)),
  and 8 workers are kept started ahead. `/sbin/init` mounts `/proc`, `/dev` and `/run` (root's
  alone, `noexec`) and `exec`s `spindle-agent`. The boot fails after 60 s without `READY 0`.
- **Heartbeat.** While commands run, the runner pings the agent every 2 s. After 20 s without a
  word from the agent, the machine is ended; commands it cuts off run again once on the next
  machine, under the rules in [Per-command isolation](#per-command-isolation).
- **Programs.** A program is a wasm module built for `wasm32-unknown-linux-musl` with a shared
  memory of its own. Threads share that memory; a cloned process gets a copy. There is no `fork`,
  so programs start others with `posix_spawn` ([`user.mjs`](../spindle/chrome/wasm-linux/user.mjs)).

The panel shows each Linux's state (Settings, Commands):

| State | v86 | wasm Linux |
| --- | --- | --- |
| `not started` | not started yet | not started yet (only when the extension carries it) |
| `booting` | first start | starting |
| `saving`, `resuming` | saving its first start; resuming the saved state | not used |
| `ready` | running | running |
| `stopped` | resting after 10 idle minutes | resting |
| `ended` | not used | a machine ended; the next command starts another |
| `failed` | failed to start | failed to start; the next command tries again |
| `unavailable` | not used | not usable in this browser, or disabled after repeated failures; commands go to v86 |

## Runner identity

`linuxRunner().identity()` gives the runner's sandbox identity, a 64-hex-digit digest. The
embedded client binds it with `setAppRunner`, so the package records runs as sandboxed.

- The v86 identity (`linux/identity`) is the SHA-256 of `fs.json` (which names every file by
  digest), `libv86.mjs`, `v86.wasm` and both BIOS images.
- The wasm identity (`wasm-linux/identity`) is the SHA-256 of `fs.json`, `vmlinux.wasm`, and the
  runtime that boots it (the `wasm-linux/*.mjs` files and everything under `v86/`: the vendored
  files and Spindle's stand-ins).
- When both are carried, the runner's identity is the SHA-256 of
  `spindle-browser-linux-v1\0V86\0WASM`, where V86 and WASM are the two images' identities
  (hex). Otherwise it is the v86 identity.

## Building the images

Neither build runs as root (on Ubuntu 23.10 and later an administrator may first have to allow
unprivileged user namespaces; see [v86 Linux](#v86-linux)). Downloads are pinned by version, commit or checksum, except Alpine's
packages, which come from the branch's current index and are checked against its signing keys.
Run the builds from the `spindle/` directory, then build the extension with them (see
[chrome.md](chrome.md#build-and-load)):

```sh
chrome/linux/build.sh                                  # v86 Linux into chrome/linux/dist
SPINDLE_WASM_LINUX=chrome/wasm-linux/dist chrome/build.sh /path/to/wasm-bindgen
```

### v86 Linux

[`chrome/linux/build.sh [OUT_DIR]`](../spindle/chrome/linux/build.sh) (default `chrome/linux/dist`)
needs curl, npm, xz (0.1.8: python3, not xz), user namespaces (`unshare -r` and, since 0.1.10, `unshare -r -m`;
Ubuntu 23.10 and later restrict them through AppArmor until
`sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`), and Rust with the
`i686-unknown-linux-musl` target. As it is since 0.1.10, it:

1. Fetches the v86 npm package at a pinned version, checks its SHA-256, and checks that it was
   built from the pinned v86 commit. It also fetches the SeaBIOS and VGA BIOS images and v86's
   `fs2json.py` and `copy-to-sha256.py` from that commit, each checked by SHA-256.
2. Fetches the Alpine x86 minirootfs and `apk-tools-static` at pinned versions with checksums,
   then installs the packages listed in [The two Linuxes](#the-two-linuxes) with
   `apk.static --usermode`, and into a second root of their own, the tools: a compiler and
   what a kernel build needs, Python and zstd. Packages come from the branch's current index and
   are checked against the signing keys in the minirootfs; `manifest.txt` records the image's.
3. Builds the kernel in the tools root (`unshare -r -m` and `chroot`, with this machine's
   `/dev` and `/proc` seen through), so it is compiled by the same Alpine compiler wherever it
   is built: the kernel.org release pinned by SHA-256, configured with Alpine's virt
   configuration (`main/linux-lts/virt.x86.config` at a pinned aports commit, checked by SHA-256)
   and [`kernel.config`](../spindle/chrome/linux/kernel.config) merged over it: the 9p root, its
   virtio transport, the network card and loop built in (the modules the old boot image loaded),
   no modules at all (the image never carried any), xz compression, no GCC plugins, no debug
   information, and fixed build user, host and time. Since 0.1.10 it also leaves out what an
   emulated PC never has or needs: other hypervisors' guest support, disks and RAID (the root
   is 9p), USB, audit, AppArmor, tracing and debugging aids, suspend, encrypted and verity
   files, multipath TCP, the firewall, BPF programs, traffic shaping, multicast and policy
   routing, the kernel's own DHCP, a mouse, crypto hardware, the CPU mitigations (an emulated
   CPU does not speculate) and all but one CPU (v86 emulates one); commands use namespaces
   only. `bzImage` is 3.7 MB instead of 5.7 MB, and commands run faster (measured in
   `linux_node.mjs`: Python and Node start 8-11% sooner, `pip install` 15% and four commands
   at once about 20% faster, a saved state 9 MiB smaller). The build checks the result has
   every setting `kernel.config` asks for.
4. Builds [`command/`](../spindle/chrome/linux/command/) (`spindle` and `spindle-sandbox`) as static
   i686 musl binaries, with build paths remapped out of them.
5. Installs the guest files: [`inittab`](../spindle/chrome/linux/guest/inittab) (`sysinit`, then
   `agent.py` and `proxy.py`, each restarted if it stops), `sysinit`,
   [`openssl.cnf`](../spindle/chrome/linux/guest/openssl.cnf), the user `spindle` (uid 1000), and
   the Python and Node modules.
6. Gives every file the build wrote a fixed time, writes a sorted tar, and converts it with v86's
   tools, in the tools root, into `fs.json` (the index) and `flat/` (files named by SHA-256,
   fetched as the guest opens them), each file then compressed with zstd at level 19 (named
   `.bin.zst`; v86 decompresses them itself).

Output: `fs.json`, `flat/`, `bzImage`, `libv86.mjs`, `v86.wasm`, `seabios.bin`, `vgabios.bin`,
`LICENSE-v86`, `manifest.txt` and `identity` (the index, kernel, emulator and BIOS). In 0.1.8 the
build installed Alpine's `linux-virt` kernel, made an initramfs that loaded the virtio and 9p
modules inside the image, and kept the files uncompressed.

### wasm Linux

The wasm Linux is built in stages, each a script in [`chrome/wasm-linux/`](../spindle/chrome/wasm-linux/)
whose header documents its arguments. With `W` as a build directory:

```sh
chrome/wasm-linux/build-kernel.sh   $W/kernel
chrome/wasm-linux/build-toolchain.sh $W/toolchain
chrome/wasm-linux/build-userland.sh $W/toolchain $W/userland
chrome/wasm-linux/build-rust.sh     $W/toolchain $W/rust
chrome/wasm-linux/build-python.sh   $W/toolchain $W/python
chrome/wasm-linux/build-image.sh    $W/kernel/vmlinux.wasm $W/userland $W/rust $W/python   # into chrome/wasm-linux/dist
```

| Script | Builds | Notes |
| --- | --- | --- |
| [`build-kernel.sh`](../spindle/chrome/wasm-linux/build-kernel.sh) | `vmlinux.wasm` | The `arch/wasm` Linux port at a pinned commit, plus Spindle's patches ([`kernel/`](../spindle/chrome/wasm-linux/kernel/): context tracking for `NO_HZ_FULL`, an `rcu_barrier` fix for offloaded CPUs, a netfs remote-size fix), configured with its defconfig plus [`spindle.config`](../spindle/chrome/wasm-linux/spindle.config) (9p over virtio, namespaces, System V IPC, `NO_HZ_FULL`). The build stops if the kernel drops any of these options. Needs clang, an LLD `wasm-ld` (LLVM's or rustup's) and `wasm2wat`; builds m4, bison and flex if missing. Since 0.1.10, `KERNEL_LLVM=DIR` compiles the kernel with another LLVM (such as `build-toolchain.sh`'s `llvm/bin`, where the machine's clang is older than 21), host programs still with the machine's clang. |
| [`build-toolchain.sh`](../spindle/chrome/wasm-linux/build-toolchain.sh) | clang, LLD, and a `wasm32-unknown-linux-musl` sysroot | From source at pinned commits: an LLVM with a wasm Linux target, a musl port, the kernel's userspace headers, and compiler-rt builtins. Programs are built with atomics, bulk memory and LLVM-lowered setjmp/longjmp. Fetches ninja if missing. |
| [`build-userland.sh`](../spindle/chrome/wasm-linux/build-userland.sh) | busybox | A busybox 1.37 port for machines without an MMU, static, with hush as `sh`. |
| [`build-rust.sh`](../spindle/chrome/wasm-linux/build-rust.sh) | Rust for `wasm32-spindle-linux-musl` | A pinned nightly from rustup, checked by its rustc commit, with `core`, `std` and `libc` patched ([`rust/patches/`](../spindle/chrome/wasm-linux/rust/patches/)) for this ABI: 32-bit `long`, asm-generic syscall numbers, spawning by `posix_spawn` only, and (since 0.1.10) file descriptors duplicated with `fcntl` and `dup2` as on Linux (std assumes no wasm32 target can, so ripgrep could not tell that input was piped to it). Adds the target spec [`rust/wasm32-spindle-linux-musl.json`](../spindle/chrome/wasm-linux/rust/wasm32-spindle-linux-musl.json), a stub libunwind (programs abort on panic), and `bin/cargo-wasm-linux`. |
| [`build-python.sh`](../spindle/chrome/wasm-linux/build-python.sh) | CPython 3.14 (3.13 in 0.1.8), and `VERSION` | The version of Alpine's python3 in the v86 Linux. Cross-compiled with zlib, bzip2 and SQLite (release tarballs pinned by SHA-256), all C modules linked into one program (there is no `dlopen`), `_posixsubprocess` patched to start children with `posix_spawn` ([`python/subprocess-spawn.patch`](../spindle/chrome/wasm-linux/python/subprocess-spawn.patch)), pymalloc, no remote debugging (3.14's, which maps other processes' memory), no `ctypes`, `ssl`, `readline` or `curses`. |
| [`build-image.sh`](../spindle/chrome/wasm-linux/build-image.sh) | `dist/`: `vmlinux.wasm.zst`, `fs.json`, `flat/`, `python-version`, `identity` | Builds [`guest/`](../spindle/chrome/wasm-linux/guest/) (`spindle-agent`, allowed up to 1 GiB of memory, and `spindle` from `chrome/linux/command`) with `cargo-wasm-linux`, and since 0.1.10 ripgrep at the version of Alpine's `ripgrep` (it refuses another; the release tarball pinned by SHA-256, its lock moved to `build-rust.sh`'s patched `libc`, its `release-lto` profile, without PCRE2, whose JIT needs `mmap`, and with [`ripgrep/no-mmap.c`](../spindle/chrome/wasm-linux/ripgrep/no-mmap.c) failing `mmap` so it reads files instead), adds `init`, `passwd`, `group`, `spindle.py` (precompiled), sets owners, modes and a fixed time, and converts the tree with v86's tools (same pinned commit as the v86 build). Since 0.1.10 it installs Alpine's `python3` and `python3-pyc` (the Alpine release `chrome/linux/build.sh` pins; it refuses another) into a root of their own and takes the standard library from there, byte for byte and with its files' times (its compiled modules check them), keeping only this Python's own build description (`_sysconfigdata`, `_sysconfig_vars`, `build-details.json`) and an empty `lib-dynload`; it refuses when Alpine's python3 is not `build-python.sh`'s version. Files and kernel are compressed with zstd in that root. |
| [`identity.sh`](../spindle/chrome/wasm-linux/identity.sh) | the identity | Since 0.1.10: the SHA-256 of the image as carried (`fs.json`, `vmlinux.wasm.zst`) and the runtime (this directory's modules and v86's vendored files). `build-image.sh` writes it; `chrome/build.sh` works it out again. |
| [`pack-image.sh`](../spindle/chrome/wasm-linux/pack-image.sh), [`fetch-image.sh`](../spindle/chrome/wasm-linux/fetch-image.sh) | a release asset, and back | Since 0.1.10: `pack-image.sh DIST TAR` packs an image into one tar with no owners or times (the same image, the same tar) and prints its SHA-256; `fetch-image.sh DIR` downloads the tar [`image.json`](../spindle/chrome/wasm-linux/image.json) names, checks its SHA-256 and unpacks it for `SPINDLE_WASM_LINUX`. The `wasm Linux image` workflow (`.github/workflows/wasm-linux.yml`, run by hand) builds every stage above on GitHub's runner, checks Python on the image, publishes the tar as a prerelease asset `wasm-linux-SHA12` with an attestation, and prints the `image.json` to commit; the browser release fetches that image for both zips. |

`chrome/build.sh` copies the runtime (`console.mjs`, `devicetree.mjs`, `entropy.mjs`,
`imports.mjs`, `machine.mjs`, `ninep.mjs`, `runner.mjs`, `task.mjs`, `user.mjs`,
`virtio.mjs` and, since 0.1.10, `zstd.mjs`), the vendored `v86/` directory, and the image's
index and kernel into the extension's `wasm-linux/` folder; since 0.1.10 the image's files go
into `linux/flat/` beside the v86 Linux's (both name files by digest, so a file both have is
carried once: all of Python's standard library), and it warns when the two Pythons'
versions differ.

**Licenses.** Per the build scripts' headers: the kernel and busybox are GPL-2.0, and the source
offer for each binary is the pinned commit in its build script, plus, for the kernel, Spindle's
patches in [`kernel/`](../spindle/chrome/wasm-linux/kernel/). The LLVM toolchain is Apache-2.0
with LLVM exceptions, musl is MIT, and Python is under the PSF license (its standard library,
since 0.1.10, being the files of Alpine's python3 package). zlib, bzip2 and SQLite
(in Python) are under the Zlib license, a BSD-style license and public domain; Rust's standard
library is MIT or Apache-2.0. [`chrome/THIRD-PARTY.md`](../spindle/chrome/THIRD-PARTY.md) lists
everything a build carries.
[`wasm-linux/v86/`](../spindle/chrome/wasm-linux/v86/) vendors four of v86's files unchanged
(`lib/9p.js`, `lib/filesystem.js`, `lib/marshall.js`, `src/browser/filestorage.js`) under v86's
BSD 2-Clause license ([`LICENSE`](../spindle/chrome/wasm-linux/v86/LICENSE)); its
[`README.md`](../spindle/chrome/wasm-linux/v86/README.md) records the commit. The other files under
`v86/src/` are Spindle's own stand-ins for the emulator modules those four import.
[`ninep.mjs`](../spindle/chrome/wasm-linux/ninep.mjs) drives v86's 9p server over the wasm machine's
virtio transport ([`virtio.mjs`](../spindle/chrome/wasm-linux/virtio.mjs), split rings), mounted with
`cache=loose,msize=262144,ignoreqv`. Because the wasm Linux's shares are v86 filesystems served
by v86's own 9p code, both Linuxes share `linux-workspace.mjs`.

## Tests

The tests are in [`spindle/chrome/tests/`](../spindle/chrome/tests/). Each names its usage in its
header (those that need arguments print it when one is missing), and most print timings. All
but `linux_bench.mjs` run in Node alone; none needs a provider; all but `linux_route.mjs` and
`linux_warm.mjs` need a built image or its parts. See also
[chrome.md](chrome.md#tests) and [testing.md](testing.md).

| Test | Usage | Checks |
| --- | --- | --- |
| [`linux_node.mjs`](../spindle/chrome/tests/linux_node.mjs) | `node --max-old-space-size=4096 --wasm-num-compilation-tasks=2 chrome/tests/linux_node.mjs [LINUX_DIST]` | The v86 Linux in Node: commands with bash, Python and Node over a 9p workspace, writes coming back, timeouts, commands side by side in their own namespaces, the workspace kept between commands, the code bridge from all three interfaces, resuming with the browser's clock, and the network: none without it; HTTP and HTTPS through the gateway from curl, Python, Node, pip and git; never a local address; a test WISP relay. The network checks need the internet. |
| [`linux_route.mjs`](../spindle/chrome/tests/linux_route.mjs) | `node chrome/tests/linux_route.mjs` | `wasmCanRun` and `wantsV86`, including the results a saved program refused on the wasm Linux gives; since 0.1.12, `needsV86` (kept directories with network on and off, network use in a command and in the workspace scripts and modules it reaches, a pip-installed package) and the reruns for a missing network and a compiled module. |
| [`linux_warm.mjs`](../spindle/chrome/tests/linux_warm.mjs) | `node chrome/tests/linux_warm.mjs` | Which Linux starts ahead of a command, with stand-in workers: `warm()` starts both, `warm({ v86: false })` the wasm Linux alone, nothing without a wasm Linux; v86 starts with the first command that needs it; `onRan` says which Linux ran each command; since 0.1.10, `prepare()` makes v86's state and lets the machine go, unless a command came meanwhile; since 0.1.12, a plain command of a thread with network runs on the wasm Linux, and a compiled module reruns on v86 only in a workspace holding compiled code. |
| [`wasm_linux_runner.mjs`](../spindle/chrome/tests/wasm_linux_runner.mjs) | `node chrome/tests/wasm_linux_runner.mjs VMLINUX_WASM USERLAND_ROOT GUEST_BIN [CPUS]` | `WasmLinux` as a runner: workspaces, exit statuses, threads side by side, Spindle calls, timeouts, Stop, isolation (no program a command saved in its workspace, `/tmp` or home runs, `/run` takes nothing, a script run through `sh` does), and a runaway loop that only a restart ends. |
| [`wasm_linux_python.mjs`](../spindle/chrome/tests/wasm_linux_python.mjs) | `node chrome/tests/wasm_linux_python.mjs [DIST [V86_WASM]]` | Python from the built image (since 0.1.10 its files zstd-decompressed with `V86_WASM`'s decoder, default `chrome/linux/dist/v86.wasm`): the version, Alpine's compiled modules used as they are, C modules, threads, files, `subprocess` via `posix_spawn`, asyncio, Unix sockets, the `spindle` module; since 0.1.10 also ripgrep (searching what is piped to it too), and `rg -P` refused as v86's rerun expects. |
| [`find_paths.mjs`](../spindle/chrome/tests/find_paths.mjs) | `node chrome/tests/find_paths.mjs DIR STRING...` | Since 0.1.10: no file under `DIR`, a `.zst` one as it decompresses, holds any `STRING` (the release workflows give their machine's directories). Needs Node 22.15 or later. |
| [`wasm_linux_rust.mjs`](../spindle/chrome/tests/wasm_linux_rust.mjs) | `node chrome/tests/wasm_linux_rust.mjs VMLINUX_WASM STD_CHECK LAYOUT_C LAYOUT_RUST [CPUS]` | Rust `std` on the target, and C and Rust struct layouts agreeing ([`wasm-linux/tests/`](../spindle/chrome/wasm-linux/tests/)). |
| [`wasm_linux_shares.mjs`](../spindle/chrome/tests/wasm_linux_shares.mjs) | `node chrome/tests/wasm_linux_shares.mjs VMLINUX_WASM USERLAND_ROOT [CPUS]` | A 9p root and a second share at `/workspace`; the entropy device. |
| [`wasm_linux_busybox.mjs`](../spindle/chrome/tests/wasm_linux_busybox.mjs) | `node chrome/tests/wasm_linux_busybox.mjs VMLINUX_WASM USERLAND_ROOT [CPUS]` | busybox `sh` from an initramfs on the console. |
| [`wasm_linux_userland.mjs`](../spindle/chrome/tests/wasm_linux_userland.mjs) | `node chrome/tests/wasm_linux_userland.mjs VMLINUX_WASM HELLO_WASM [CPUS]` | A C program as `/init`: `posix_spawn` of itself, a thread. |
| [`wasm_linux_node.mjs`](../spindle/chrome/tests/wasm_linux_node.mjs) | `node chrome/tests/wasm_linux_node.mjs VMLINUX_WASM [CPUS]` | The kernel boots on Spindle's runtime and ends as a kernel without init ends. |
| [`linux_bench.mjs`](../spindle/chrome/tests/linux_bench.mjs) | `SPINDLE_BROWSER=... node chrome/tests/linux_bench.mjs DIST [RUNS]` | Times the same commands on both Linuxes in real Chrome through `linuxRunner` with a forced route. It writes a page into `DIST`, so use a test build made with `SPINDLE_WASM_LINUX`. |

`extension_real.mjs` ([chrome.md](chrome.md#tests)) also runs bash, Spindle calls and a granted
site through the panel and, in `--scripted` mode with the wasm Linux built in, a plain `sh`
command on the wasm Linux.
