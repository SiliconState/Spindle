//! `spindle-agent`: Spindle's runner agent in the wasm Linux, and the sandbox it puts each
//! command in. The browser Linux's agent (chrome/linux/guest/agent.py) on v86 is Python;
//! here there is no Python to run it and no `fork`, so it is this one program, which
//! starts copies of itself (posix_spawn) for the steps a fork made there.
//!
//! It runs the commands the browser asks for over /dev/hvc1 (a console of its own: the
//! kernel's is hvc0), several at once, each with busybox `sh` in /workspace (the
//! command's copy of its thread's workspace, a directory of the 9p root the browser
//! filled, bound there), as the unprivileged user `spindle`, in namespaces of its own:
//! its own mounts (a fresh /tmp, /var/tmp, /dev/shm and home), processes (whatever it
//! leaves running ends with it), System V IPC, host name, and network (only loopback: the
//! wasm Linux has no network card; commands whose thread has network run on v86). Frames
//! are lines: STX, a kind, an id, base64 (the console takes any size, so no frame goes
//! as a file).
//!
//!   browser -> agent  RUN <id> <base64 JSON {command, timeout_s, dir}> | STOP <id> | PING <n>
//!                     | REPLY <id> <base64 JSON {seq, reply}>
//!   agent -> browser  READY <n> | OUT <id> <base64 bytes> | RPC <id> <base64 JSON {seq, request}>
//!                     | END <id> <base64 JSON {code, timed_out, stopped, lingering}>
//!
//! A task takes a signal only when it calls the kernel, and holds its CPU until then (no
//! preemption here), so a program computing without calls cannot be killed: `lingering`
//! says that the command's processes had not all ended when its END went, and the
//! browser restarts the machine to end them. Meanwhile it holds up nothing else: the
//! agent, each command's sandbox and init, and the devices' interrupts (`irqaffinity=0`)
//! keep CPU 0, and each command's shell and what it starts run on two CPUs of their own
//! (the least used pair of the others), so no other command's timers and wakeups wait on
//! a CPU a runaway holds, and one runaway leaves its own command a CPU.
//!
//! While a command runs, its scripts ask Spindle for calls through /run/spindle.sock (a
//! socket of that command's alone), one JSON line each way, relayed one at a time per
//! command as agent.py relays them (the same protocol, numbering each call's children).
//!
//! usage: spindle-agent                              the agent
//!        spindle-agent sandbox CPUS WORKDIR SOCKET COMMAND   a command's namespaces (as root)
//!        spindle-agent init CPUS COMMAND            their first process
//!        spindle-agent shell CPUS COMMAND           the command's shell, on CPUS (`1,2`)

use std::collections::{BTreeMap, VecDeque};
use std::ffi::{CString, c_char, c_int};
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::ptr;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

const PORT: &str = "/dev/hvc1";
const AGENT: &str = "/usr/local/bin/spindle-agent";
const SHELL: &str = "/bin/sh";
/// Where a command's scripts reach Spindle (in its namespace), and where each command's
/// own socket is made (root's, closed to commands).
const SOCKET: &str = "/run/spindle.sock";
const CALLS: &str = "/run/spindle-calls";
/// Where the browser makes commands' copies of their workspaces.
const RUNS: &str = "/runs";
const ENV: [&str; 6] = [
    "HOME=/home/spindle",
    "LANG=C.UTF-8",
    "TERM=dumb",
    "USER=spindle",
    "LOGNAME=spindle",
    "PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
];
const UID: libc::uid_t = 1000;
const GID: libc::gid_t = 1000;
/// The browser keeps 4 MiB of output; one byte more tells it the output was cut.
const MAX_OUT: usize = 4 * 1024 * 1024 + 1;
const CHUNK: usize = 64 * 1024;
/// After the shell exits, how long output something it left still holds open is read.
const DRAIN: Duration = Duration::from_millis(500);
/// After a command is ended, how long its processes have to go before its END says some
/// linger (a computing task takes its SIGKILL only at its next call to the kernel).
const LINGER: Duration = Duration::from_millis(500);
/// How long a command's namespaces may take to start (the agent waits for them, so not
/// long: a sandbox the kernel cannot set up now must not hold up every other command).
const STARTING: Duration = Duration::from_secs(5);
/// Scripts' connections one command may hold at once; the longest request line, the most
/// replies a script may leave untaken, and the most one command's scripts may have the
/// agent hold in all (lines being read, calls waiting their turn, replies untaken). The
/// agent is one process for every command, and a call goes out as several copies of its
/// line: these keep it clear of the most memory it may have (guest/build.rs).
const MAX_CLIENTS: usize = 64;
const MAX_LINE: usize = 16 * 1024 * 1024;
const MAX_PENDING: usize = 16 * 1024 * 1024;
const MAX_HELD: usize = 32 * 1024 * 1024;
/// Calls that are children (numbered as a native script numbers them); a batch counts each.
const CHILDREN: [&str; 3] = ["model", "tool", "http"];
/// The sandbox's descriptors: the agent's status line, as the helper and init have it.
const STATUS: RawFd = 3;
/// Scratch mounts of a command's own.
const SCRATCH: [(&str, &str); 4] = [
    ("/tmp", "mode=1777"),
    ("/var/tmp", "mode=1777"),
    ("/dev/shm", "mode=1777"),
    ("/home/spindle", "mode=0755"),
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        None => agent(),
        Some("sandbox") => match &args[2..] {
            [cpus, workdir, socket, command] => sandbox(cpus, workdir, socket, command),
            _ => fail("usage: spindle-agent sandbox CPUS WORKDIR SOCKET COMMAND"),
        },
        Some("init") => match &args[2..] {
            [cpus, command] => init(cpus, command),
            _ => fail("usage: spindle-agent init CPUS COMMAND"),
        },
        Some("shell") => match &args[2..] {
            [cpus, command] => shell(cpus, command),
            _ => fail("usage: spindle-agent shell CPUS COMMAND"),
        },
        Some(_) => {
            eprintln!(
                "usage: spindle-agent [sandbox CPUS WORKDIR SOCKET COMMAND | init CPUS COMMAND | shell CPUS COMMAND]"
            );
            std::process::exit(2)
        }
    }
}

// ---------------------------------------------------------------------------------------
// Processes: spawning without fork, ending, reaping.

fn c(text: &str) -> CString {
    CString::new(text).unwrap_or_default()
}

fn errno() -> c_int {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// How a process ended, as the agent reports it: its status, or minus its signal.
fn code(status: c_int) -> i32 {
    if status & 0x7f == 0 {
        (status >> 8) & 0xff
    } else {
        -(status & 0x7f)
    }
}

#[derive(Default)]
struct Spawn<'a> {
    /// The environment (none: this process's).
    env: Option<&'a [&'a str]>,
    /// Descriptors given to the program (from, to).
    dup: &'a [(RawFd, RawFd)],
    close: &'a [RawFd],
    chdir: Option<&'a str>,
    setsid: bool,
}

/// Starts `path` with `args` (posix_spawn: the only way to start a program here); its
/// pid. SIGPIPE and SIGXFSZ start as their defaults (a Rust program ignores SIGPIPE, and
/// what a process ignores its programs would too), and no signal blocked.
fn spawn(path: &str, args: &[&str], how: Spawn) -> io::Result<libc::pid_t> {
    let path = c(path);
    let args: Vec<CString> = args.iter().map(|arg| c(arg)).collect();
    let mut argv: Vec<*mut c_char> = args.iter().map(|arg| arg.as_ptr().cast_mut()).collect();
    argv.push(ptr::null_mut());
    let env: Option<Vec<CString>> = how.env.map(|env| env.iter().map(|pair| c(pair)).collect());
    let mut envp: Vec<*mut c_char> = env
        .iter()
        .flatten()
        .map(|pair| pair.as_ptr().cast_mut())
        .collect();
    envp.push(ptr::null_mut());
    let chdir = how.chdir.map(c);
    unsafe extern "C" {
        static environ: *const *mut c_char;
    }
    unsafe {
        let mut actions: libc::posix_spawn_file_actions_t = std::mem::zeroed();
        let mut attrs: libc::posix_spawnattr_t = std::mem::zeroed();
        libc::posix_spawn_file_actions_init(&mut actions);
        libc::posix_spawnattr_init(&mut attrs);
        for &(from, to) in how.dup {
            libc::posix_spawn_file_actions_adddup2(&mut actions, from, to);
        }
        for &fd in how.close {
            libc::posix_spawn_file_actions_addclose(&mut actions, fd);
        }
        if let Some(dir) = &chdir {
            libc::posix_spawn_file_actions_addchdir_np(&mut actions, dir.as_ptr());
        }
        let mut defaults: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut defaults);
        libc::sigaddset(&mut defaults, libc::SIGPIPE);
        libc::sigaddset(&mut defaults, libc::SIGXFSZ);
        libc::posix_spawnattr_setsigdefault(&mut attrs, &defaults);
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::posix_spawnattr_setsigmask(&mut attrs, &none);
        let mut flags = (libc::POSIX_SPAWN_SETSIGDEF | libc::POSIX_SPAWN_SETSIGMASK) as c_int;
        if how.setsid {
            flags |= libc::POSIX_SPAWN_SETSID as c_int;
        }
        libc::posix_spawnattr_setflags(&mut attrs, flags as _);
        let mut pid = 0;
        let result = libc::posix_spawn(
            &mut pid,
            path.as_ptr(),
            &actions,
            &attrs,
            argv.as_ptr(),
            if how.env.is_some() {
                envp.as_ptr()
            } else {
                environ
            },
        );
        libc::posix_spawn_file_actions_destroy(&mut actions);
        libc::posix_spawnattr_destroy(&mut attrs);
        if result == 0 {
            Ok(pid)
        } else {
            Err(io::Error::from_raw_os_error(result))
        }
    }
}

/// A pipe's ends (read, write), closed on exec.
fn pipe() -> io::Result<(File, File)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { Ok((File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1]))) }
}

fn set_nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

/// A descriptor that becomes readable when `pid` ends (none where pidfds are not).
fn pidfd(pid: libc::pid_t) -> Option<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Whether `pid` (a child) has ended, reaping it if so.
fn reaped(pid: libc::pid_t) -> bool {
    let mut status = 0;
    loop {
        let found = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if found < 0 && errno() == libc::EINTR {
            continue;
        }
        return found == pid || (found < 0 && errno() == libc::ECHILD);
    }
}

// ---------------------------------------------------------------------------------------
// The sandbox: a command's namespaces (the helper, as root) and their first process.

/// Tells the agent one line; its own end gone, nothing more can be told.
fn tell(line: &str) {
    let line = format!("{line}\n");
    unsafe { libc::write(STATUS, line.as_ptr().cast(), line.len()) };
}

/// Says why it could not start the command, and ends.
fn fail(why: &str) -> ! {
    tell(&format!("fail {why}: errno {}", errno()));
    unsafe { libc::_exit(126) }
}

fn check(result: c_int, why: &str) {
    if result < 0 {
        fail(why);
    }
}

fn mounted(
    source: Option<&str>,
    target: &str,
    kind: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) {
    let (source, target, kind, data) = (source.map(c), c(target), kind.map(c), data.map(c));
    let pointer =
        |value: &Option<CString>| value.as_ref().map_or(ptr::null(), |value| value.as_ptr());
    let result = unsafe {
        libc::mount(
            pointer(&source),
            target.as_ptr(),
            pointer(&kind),
            flags,
            pointer(&data).cast(),
        )
    };
    check(result, &format!("mount {}", target.to_string_lossy()));
}

/// CPUs each command's processes get of their own: two, so one that computes without end
/// leaves the rest of its command a CPU.
const GROUP: usize = 2;

/// The CPUs commands get on a machine of `cpus`: 1-2, 3-4, ... (CPU 0 is the agent's); a
/// machine of one CPU has only CPU 0 to give.
fn groups(cpus: usize) -> Vec<Vec<usize>> {
    if cpus < 2 {
        return vec![vec![0]];
    }
    (1..cpus)
        .collect::<Vec<_>>()
        .chunks(GROUP)
        .map(<[usize]>::to_vec)
        .collect()
}

/// Keeps this process (and what it starts) to `cpus`; whether it could.
fn pin(cpus: &[usize]) -> bool {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for &cpu in cpus {
            libc::CPU_SET(cpu, &mut set);
        }
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0
    }
}

/// The CPUs this process may use now (all of them, at the agent's start).
fn cpus() -> usize {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) < 0 {
            return 1;
        }
        (libc::CPU_COUNT(&set) as usize).max(1)
    }
}

/// Brings up `lo` in the command's own network (it starts down).
fn loopback_up() {
    unsafe {
        let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        check(sock, "socket");
        let mut request: libc::ifreq = std::mem::zeroed();
        for (at, byte) in b"lo".iter().enumerate() {
            request.ifr_name[at] = *byte as c_char;
        }
        check(
            libc::ioctl(sock, libc::SIOCGIFFLAGS as _, &mut request),
            "SIOCGIFFLAGS",
        );
        request.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
        check(
            libc::ioctl(sock, libc::SIOCSIFFLAGS as _, &request),
            "SIOCSIFFLAGS",
        );
        libc::close(sock);
    }
}

/// The helper the agent starts for a command: descriptor 0 is /dev/null, 1 and 2 the
/// command's output, 3 the status line (`init PID` from the helper, `exit CODE` from its
/// init, `fail WHY` from either, in no set order). It makes the namespaces and mounts, starts their first process and
/// waits for it.
fn sandbox(cpus: &str, workdir: &str, socket: &str, command: &str) -> ! {
    let flags = libc::CLONE_NEWNS
        | libc::CLONE_NEWPID
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWNET;
    check(unsafe { libc::unshare(flags) }, "unshare");
    loopback_up();
    mounted(None, "/", None, libc::MS_REC | libc::MS_PRIVATE, None);
    mounted(Some(workdir), "/workspace", None, libc::MS_BIND, None);
    // Without an MMU tmpfs is ramfs: no size limit, and no uid= or gid= (it drops them),
    // so the home is given to the commands' user after.
    for (path, options) in SCRATCH {
        mounted(Some("tmpfs"), path, Some("tmpfs"), 0, Some(options));
    }
    check(
        unsafe { libc::chown(c("/home/spindle").as_ptr(), UID, GID) },
        "chown /home/spindle",
    );
    // Its own terminals: /dev/ptmx finds the instance at /dev/pts beside it.
    mounted(
        Some("devpts"),
        "/dev/pts",
        Some("devpts"),
        libc::MS_NOSUID | libc::MS_NOEXEC,
        Some("newinstance,ptmxmode=0666,mode=0620"),
    );
    mounted(Some(socket), SOCKET, None, libc::MS_BIND, None);
    // The first process started in the new PID namespace is its init (on the agent's CPU,
    // as this is: it reaps and reports whatever the command's CPUs are busy with).
    let init = match spawn(
        AGENT,
        &["spindle-agent", "init", cpus, command],
        Spawn::default(),
    ) {
        Ok(pid) => pid,
        Err(error) => {
            tell(&format!("fail start its init: {error}"));
            unsafe { libc::_exit(126) }
        }
    };
    tell(&format!("init {init}"));
    unsafe {
        // Its output is the command's alone: closed here, so it ends when theirs does.
        let null = libc::open(c("/dev/null").as_ptr(), libc::O_RDWR);
        libc::dup2(null, 1);
        libc::dup2(null, 2);
        let mut status = 0;
        while libc::waitpid(init, &mut status, 0) < 0 && errno() == libc::EINTR {}
        libc::_exit(0)
    }
}

/// The namespace's first process: its own /proc, then the shell, as the commands' user
/// in a session of its own; reaps what is orphaned to it until the agent kills it (a
/// namespace's init takes no signal from inside it).
fn init(cpus: &str, command: &str) -> ! {
    // First of all: its holder killed (the agent gave up waiting for it to start), it goes
    // too, and with it its namespace.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
    mounted(
        Some("proc"),
        "/proc",
        Some("proc"),
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    );
    unsafe {
        check(libc::setgroups(0, ptr::null()), "setgroups");
        check(libc::setgid(GID), "setgid");
        check(libc::setuid(UID), "setuid");
        // Again after the user changes (which clears it).
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
    }
    let shell = spawn(
        AGENT,
        &["spindle-agent", "shell", cpus, command],
        // The agent's line is not the command's.
        Spawn {
            close: &[STATUS],
            chdir: Some("/workspace"),
            setsid: true,
            ..Spawn::default()
        },
    );
    let shell = match shell {
        Ok(pid) => pid,
        Err(error) => {
            tell(&format!("fail start {SHELL}: {error}"));
            unsafe { libc::_exit(126) }
        }
    };
    unsafe {
        let null = libc::open(c("/dev/null").as_ptr(), libc::O_RDWR);
        libc::dup2(null, 1);
        libc::dup2(null, 2);
        loop {
            let mut status = 0;
            let pid = libc::waitpid(-1, &mut status, 0);
            if pid == shell {
                tell(&format!("exit {}", code(status)));
            } else if pid < 0 && errno() == libc::ECHILD {
                libc::pause();
            }
        }
    }
}

/// The command's shell: on the command's CPUs (`1,2`), then `sh -c COMMAND` with
/// pipefail, as v86's bash runs commands (on COMMAND's first line, so its lines keep their
/// numbers).
fn shell(cpus: &str, command: &str) -> ! {
    let cpus: Vec<usize> = cpus.split(',').filter_map(|cpu| cpu.parse().ok()).collect();
    // Left on CPU 0, a command computing without end would hold up the agent and with it
    // every command.
    if !cpus.is_empty() && !pin(&cpus) {
        eprintln!(
            "spindle runner: could not keep the command to CPUs {cpus:?}: {}",
            io::Error::last_os_error()
        );
        unsafe { libc::_exit(126) }
    }
    // As a Rust program this one ignores SIGPIPE; the shell and its programs must not.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        libc::signal(libc::SIGXFSZ, libc::SIG_DFL);
    }
    let script = format!("set -o pipefail; {command}");
    let (path, argv) = (c(SHELL), [c("sh"), c("-c"), c(&script)]);
    let mut pointers: Vec<*const c_char> = argv.iter().map(|arg| arg.as_ptr()).collect();
    pointers.push(ptr::null());
    unsafe extern "C" {
        static environ: *const *const c_char;
    }
    unsafe {
        libc::execve(path.as_ptr(), pointers.as_ptr(), environ);
    }
    eprintln!("spindle runner: {SHELL}: {}", io::Error::last_os_error());
    unsafe { libc::_exit(127) }
}

// ---------------------------------------------------------------------------------------
// The agent: frames on its console, commands, their scripts' calls.

/// The console to the browser: frames out, and the whole frames read so far.
struct Port {
    file: File,
    pending: Vec<u8>,
}

impl Port {
    fn open() -> io::Result<Port> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(PORT)?;
        // Raw: no echo, no line editing, no line length limit, bytes as they are.
        unsafe {
            let mut mode: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(file.as_raw_fd(), &mut mode) == 0 {
                libc::cfmakeraw(&mut mode);
                libc::tcsetattr(file.as_raw_fd(), libc::TCSANOW, &mode);
            }
        }
        Ok(Port {
            file,
            pending: Vec::new(),
        })
    }

    fn send(&self, kind: &str, id: &str, payload: &[u8]) {
        let mut frame = Vec::with_capacity(kind.len() + id.len() + payload.len() * 4 / 3 + 8);
        frame.push(2);
        frame.extend_from_slice(kind.as_bytes());
        frame.push(b' ');
        frame.extend_from_slice(id.as_bytes());
        frame.push(b' ');
        frame.extend_from_slice(STANDARD.encode(payload).as_bytes());
        frame.push(b'\n');
        // The browser gone, nothing more can be told; the machine ends with it.
        let _ = (&self.file).write_all(&frame);
    }

    /// The frames read now, as (kind, id, payload); a line that does not read is dropped.
    fn frames(&mut self) -> Vec<(String, String, Vec<u8>)> {
        let mut buffer = [0u8; CHUNK];
        match (&self.file).read(&mut buffer) {
            Ok(read) => self.pending.extend_from_slice(&buffer[..read]),
            Err(error)
                if error.kind() == ErrorKind::Interrupted
                    || error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => panic!("the console to the browser failed: {error}"),
        }
        let mut found = Vec::new();
        while let Some(end) = self.pending.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let Some(start) = line.iter().position(|&byte| byte == 2) else {
                continue;
            };
            let text = String::from_utf8_lossy(&line[start + 1..line.len() - 1]).into_owned();
            let parts: Vec<&str> = text.split(' ').collect();
            let [kind, id, data] = parts.as_slice() else {
                continue;
            };
            let Ok(payload) = STANDARD.decode(data) else {
                continue;
            };
            found.push((kind.to_string(), id.to_string(), payload));
        }
        found
    }
}

/// One script's connection: what it sent so far and the replies it has not taken.
struct Client {
    stream: UnixStream,
    held: Vec<u8>,
    outgoing: Vec<u8>,
}

/// The scripts of one command: its socket, their connections, the calls waiting their
/// turn, the one with Spindle, and the number the next child takes. Nothing a script
/// does (not reading its replies, a line without end) holds up the agent or another
/// command.
struct Bridge {
    id: String,
    server: UnixListener,
    child: u64,
    seq: u64,
    next_client: u64,
    clients: BTreeMap<u64, Client>,
    /// Calls waiting their turn: the script's connection, the request, its line's length.
    queue: VecDeque<(Option<u64>, Value, usize)>,
    /// The call with Spindle: its seq, and its script's connection (gone: None).
    asking: Option<(u64, Option<u64>)>,
}

impl Bridge {
    fn accept(&mut self) {
        let Ok((stream, _)) = self.server.accept() else {
            return;
        };
        let _ = stream.set_nonblocking(true);
        if self.clients.len() >= MAX_CLIENTS {
            let refusal = json!({"ok": false, "error": format!("at most {MAX_CLIENTS} connections to Spindle at once")});
            let _ = (&stream).write_all(format!("{refusal}\n").as_bytes());
            return;
        }
        self.next_client += 1;
        self.clients.insert(
            self.next_client,
            Client {
                stream,
                held: Vec::new(),
                outgoing: Vec::new(),
            },
        );
    }

    fn read(&mut self, client: u64, port: &Port) {
        let Some(found) = self.clients.get_mut(&client) else {
            return;
        };
        let mut buffer = [0u8; CHUNK];
        let read = match (&found.stream).read(&mut buffer) {
            Ok(read) => read,
            Err(error)
                if error.kind() == ErrorKind::WouldBlock
                    || error.kind() == ErrorKind::Interrupted =>
            {
                return;
            }
            Err(_) => 0,
        };
        if read == 0 {
            self.drop_client(client);
            return;
        }
        found.held.extend_from_slice(&buffer[..read]);
        if self.held() > MAX_HELD {
            let error = format!(
                "this command's scripts have sent more than {} MiB Spindle has not yet taken",
                MAX_HELD / 1048576
            );
            self.answer(client, &json!({"ok": false, "error": error}));
            self.drop_client(client);
            return;
        }
        loop {
            let Some(found) = self.clients.get_mut(&client) else {
                return;
            };
            let Some(end) = found.held.iter().position(|&byte| byte == b'\n') else {
                break;
            };
            let line: Vec<u8> = found.held.drain(..=end).collect();
            self.ask(client, &line[..line.len() - 1], port);
        }
        if self
            .clients
            .get(&client)
            .is_some_and(|found| found.held.len() > MAX_LINE)
        {
            let error = format!(
                "a request is one line of at most {} MiB",
                MAX_LINE / 1048576
            );
            self.answer(client, &json!({"ok": false, "error": error}));
            self.drop_client(client);
        }
    }

    fn ask(&mut self, client: u64, line: &[u8], port: &Port) {
        // JSON only (no NaN, no number too large for a float: serde_json refuses both).
        let request = serde_json::from_slice::<Value>(line)
            .ok()
            .and_then(|request| {
                let method = request.get("method")?.as_str()?.to_string();
                let input = request.get("input").cloned().unwrap_or_else(|| json!({}));
                Some(json!({"method": method, "input": input}))
            });
        match request {
            Some(request) => {
                self.queue.push_back((Some(client), request, line.len()));
                self.next(port);
            }
            None => self.answer(
                client,
                &json!({"ok": false, "error": "a request is {\"method\": ..., \"input\": ...}"}),
            ),
        }
    }

    /// Sends the next waiting call once none is with Spindle (numbered as it goes).
    fn next(&mut self, port: &Port) {
        if self.asking.is_some() {
            return;
        }
        let Some((client, mut request, _)) = self.queue.pop_front() else {
            return;
        };
        let method = request["method"].as_str().unwrap_or_default().to_string();
        if CHILDREN.contains(&method.as_str()) || method == "batch" {
            request["id"] = json!(self.child);
            let calls = if method == "batch" {
                request["input"]
                    .get("calls")
                    .and_then(Value::as_array)
                    .map(Vec::len)
            } else {
                None
            };
            self.child += calls.unwrap_or(1) as u64;
        }
        self.seq += 1;
        self.asking = Some((self.seq, client));
        port.send(
            "RPC",
            &self.id,
            json!({"seq": self.seq, "request": request})
                .to_string()
                .as_bytes(),
        );
    }

    fn reply(&mut self, payload: &[u8], port: &Port) -> Result<(), String> {
        let message: Value = serde_json::from_slice(payload).map_err(|error| error.to_string())?;
        let Some((seq, client)) = self.asking else {
            return Ok(());
        };
        if message.get("seq").and_then(Value::as_u64) != Some(seq) {
            return Ok(());
        }
        self.asking = None;
        if let Some(client) = client {
            let reply = message
                .get("reply")
                .filter(|reply| !reply.is_null())
                .cloned()
                .unwrap_or_else(|| json!({"ok": false, "error": "no reply"}));
            self.answer(client, &reply);
        }
        self.next(port);
        Ok(())
    }

    /// Queues a reply for its script, which takes it when it reads.
    fn answer(&mut self, client: u64, reply: &Value) {
        let Some(found) = self.clients.get_mut(&client) else {
            return;
        };
        found
            .outgoing
            .extend_from_slice(format!("{reply}\n").as_bytes());
        self.flush(client);
        if self
            .clients
            .get(&client)
            .is_some_and(|found| found.outgoing.len() > MAX_PENDING)
        {
            self.drop_client(client); // a script that never reads its replies
        }
    }

    fn flush(&mut self, client: u64) {
        let Some(found) = self.clients.get_mut(&client) else {
            return;
        };
        while !found.outgoing.is_empty() {
            match (&found.stream).write(&found.outgoing) {
                Ok(written) => {
                    found.outgoing.drain(..written);
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => {
                    self.drop_client(client);
                    return;
                }
            }
        }
    }

    /// What its scripts have the agent hold: lines being read, calls waiting, replies.
    fn held(&self) -> usize {
        let clients: usize = self
            .clients
            .values()
            .map(|found| found.held.len() + found.outgoing.len())
            .sum();
        clients + self.queue.iter().map(|(_, _, size)| size).sum::<usize>()
    }

    fn drop_client(&mut self, client: u64) {
        self.clients.remove(&client);
        self.queue.retain(|(waiting, _, _)| *waiting != Some(client));
        // A call already with Spindle still holds the turn until its reply comes.
        if let Some((seq, Some(asking))) = self.asking
            && asking == client
        {
            self.asking = Some((seq, None));
        }
    }
}

/// One running command: its namespaces' processes, its output, its scripts.
struct Command {
    id: String,
    deadline: Instant,
    socket_path: PathBuf,
    bridge: Bridge,
    holder: libc::pid_t,
    holder_ended: Option<OwnedFd>,
    /// Its holder reaped (by `reaped`, or by the agent as init reaping what ended): its pid
    /// may be another process's now, so it is neither waited for nor signalled again.
    holder_gone: bool,
    out: Option<File>,
    status: Option<File>,
    lines: Vec<u8>,
    sent: usize,
    /// When the shell exited, while something it left holds its output open.
    exited: Option<Instant>,
    code: Option<i32>,
    init: Option<libc::pid_t>,
    failure: Option<String>,
    stopped: bool,
    timed_out: bool,
    /// When it was ended, its processes going (its END waits for them, a while).
    ending: Option<Instant>,
    /// The CPUs its processes run on (of `groups`).
    group: usize,
}

impl Command {
    fn start(
        id: &str,
        job: &Value,
        made: u64,
        group: usize,
        cpus: &[usize],
    ) -> Result<Command, String> {
        let dir = job.get("dir").and_then(Value::as_str).ok_or("no dir")?;
        let workdir = Path::new("/").join(dir.trim_start_matches('/'));
        let normal = workdir
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)));
        if !normal || workdir.parent() != Some(Path::new(RUNS)) {
            return Err(format!("{dir} is not a command's copy"));
        }
        let command = job
            .get("command")
            .and_then(Value::as_str)
            .ok_or("no command")?;
        // Arguments cannot hold NUL: such a command would run as an empty one.
        if command.contains('\0') || dir.contains('\0') {
            return Err("a command cannot contain a NUL character".into());
        }
        let timeout = job
            .get("timeout_s")
            .and_then(Value::as_f64)
            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
            .unwrap_or(120.0);
        let socket_path = PathBuf::from(format!("{CALLS}/{made}.sock"));
        let _ = fs::remove_file(&socket_path);
        let server =
            UnixListener::bind(&socket_path).map_err(|error| format!("its socket: {error}"))?;
        let _ = fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o666));
        let _ = server.set_nonblocking(true);
        let bridge = Bridge {
            id: id.to_string(),
            server,
            child: 0,
            seq: 0,
            next_client: 0,
            clients: BTreeMap::new(),
            queue: VecDeque::new(),
            asking: None,
        };
        let started = (|| -> io::Result<(libc::pid_t, File, File)> {
            let (out, out_end) = pipe()?;
            let (status, status_end) = pipe()?;
            let null = File::open("/dev/null")?;
            let socket = socket_path.to_string_lossy().into_owned();
            let holder = spawn(
                AGENT,
                &[
                    "spindle-agent",
                    "sandbox",
                    &cpus
                        .iter()
                        .map(usize::to_string)
                        .collect::<Vec<_>>()
                        .join(","),
                    workdir.to_str().unwrap_or_default(),
                    &socket,
                    command,
                ],
                Spawn {
                    env: Some(&ENV),
                    dup: &[
                        (null.as_raw_fd(), 0),
                        (out_end.as_raw_fd(), 1),
                        (out_end.as_raw_fd(), 2),
                        (status_end.as_raw_fd(), STATUS),
                    ],
                    ..Spawn::default()
                },
            )?;
            Ok((holder, out, status))
        })();
        let (holder, out, status) = match started {
            Ok(started) => started,
            Err(error) => {
                let _ = fs::remove_file(&socket_path);
                return Err(format!("its sandbox did not start: {error}"));
            }
        };
        let mut command = Command {
            id: id.to_string(),
            deadline: Instant::now() + Duration::from_secs_f64(timeout.min(1e9)),
            socket_path,
            bridge,
            holder,
            holder_ended: pidfd(holder),
            holder_gone: false,
            out: Some(out),
            status: Some(status),
            lines: Vec::new(),
            sent: 0,
            exited: None,
            code: None,
            init: None,
            failure: None,
            stopped: false,
            timed_out: false,
            ending: None,
            group,
        };
        // Its init's pid, told as the helper starts it (how the shell ended, or why it
        // could not start, may come first): the command is ended by ending that init, so
        // it is waited for, unless the helper ends without one.
        let end = Instant::now() + STARTING;
        while command.init.is_none() && command.status.is_some() {
            let left = end.saturating_duration_since(Instant::now());
            let fd = command.status.as_ref().map_or(-1, AsRawFd::as_raw_fd);
            if left.is_zero() || !readable(fd, left) {
                command
                    .failure
                    .get_or_insert_with(|| "its namespaces did not start".into());
                break;
            }
            command.read_status();
        }
        if let Some(out) = &command.out {
            set_nonblocking(out.as_raw_fd());
        }
        Ok(command)
    }

    fn read_status(&mut self) {
        let Some(status) = &self.status else { return };
        let mut buffer = [0u8; 512];
        let read = match (&*status).read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted => return,
            Err(_) => 0,
        };
        if read == 0 {
            self.status = None;
            return;
        }
        self.lines.extend_from_slice(&buffer[..read]);
        while let Some(end) = self.lines.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.lines.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            let (kind, value) = line.split_once(' ').unwrap_or((&line, ""));
            match kind {
                "init" => self.init = value.parse().ok(),
                "exit" => {
                    self.code = value.parse().ok();
                    self.exited = Some(Instant::now());
                }
                "fail" => self.failure = Some(value.to_string()),
                _ => {}
            }
        }
    }

    /// What to watch for it: (descriptor, what it is, wants writing).
    fn watched(&self, watch: &mut Vec<(RawFd, Watch, bool)>) {
        if let Some(ended) = &self.holder_ended
            && self.ending.is_some()
        {
            watch.push((ended.as_raw_fd(), Watch::Holder(self.id.clone()), false));
        }
        if self.ending.is_some() {
            return;
        }
        watch.push((
            self.bridge.server.as_raw_fd(),
            Watch::Server(self.id.clone()),
            false,
        ));
        for (&client, found) in &self.bridge.clients {
            watch.push((
                found.stream.as_raw_fd(),
                Watch::Client(self.id.clone(), client),
                !found.outgoing.is_empty(),
            ));
        }
        for (file, what) in [
            (&self.out, Watch::Out(self.id.clone())),
            (&self.status, Watch::Status(self.id.clone())),
        ] {
            if let Some(file) = file {
                watch.push((file.as_raw_fd(), what, false));
            }
        }
    }

    /// How long until this command must be looked at whatever comes.
    fn wait(&self, now: Instant) -> Duration {
        if let Some(ending) = self.ending {
            // Without a pidfd its holder is looked at every so often.
            let step = if self.holder_ended.is_some() {
                LINGER
            } else {
                Duration::from_millis(20)
            };
            return (ending + LINGER).saturating_duration_since(now).min(step);
        }
        let mut left = self.deadline.saturating_duration_since(now);
        if let Some(exited) = self.exited {
            left = left.min((exited + DRAIN).saturating_duration_since(now));
        }
        left
    }

    fn take_output(&mut self, port: &Port) {
        let Some(out) = &self.out else { return };
        let mut buffer = vec![0u8; CHUNK];
        match (&*out).read(&mut buffer) {
            Ok(0) => self.out = None,
            Ok(read) if self.sent < MAX_OUT => {
                let keep = read.min(MAX_OUT - self.sent);
                port.send("OUT", &self.id, &buffer[..keep]);
                self.sent += keep;
            }
            Ok(_) => {}
            Err(error)
                if error.kind() == ErrorKind::WouldBlock
                    || error.kind() == ErrorKind::Interrupted => {}
            Err(_) => self.out = None,
        }
    }

    /// Whether, running, it is over now.
    fn over(&mut self, now: Instant) -> bool {
        if self.failure.is_some() || self.stopped {
            return true;
        }
        if now >= self.deadline {
            self.timed_out = true;
            return true;
        }
        // The shell is done: once its output is (or a moment after, for what it left
        // holding it). Output done while the shell runs on means waiting for the shell.
        if let Some(exited) = self.exited
            && (self.out.is_none() || now >= exited + DRAIN)
        {
            return true;
        }
        // Its holder gone without the shell's end (killed from outside): over.
        self.status.is_none() && self.exited.is_none()
    }

    /// Ends every process of its namespaces: kills their init (or the helper, before
    /// there is one); its END goes once they are gone, or after LINGER.
    fn end(&mut self) {
        // Told late, its init's pid still comes in time to be ended with it.
        if self.init.is_none()
            && let Some(status) = &self.status
        {
            set_nonblocking(status.as_raw_fd());
            self.read_status();
        }
        // A holder gone took its init with it (it waits for its init before it ends), and
        // their pids may be others' now.
        if !self.holder_gone {
            unsafe {
                match self.init {
                    Some(init) => libc::kill(init, libc::SIGKILL),
                    None => libc::kill(self.holder, libc::SIGKILL),
                };
            }
        }
        self.bridge.clients.clear();
        self.bridge.queue.clear();
        // Its socket file stays until its namespace is gone: it is a mount point there,
        // and unlinking one detaches it, which waits for every CPU to pass through the
        // kernel (one a runaway holds never does).
        self.out = None;
        self.status = None;
        self.ending = Some(Instant::now());
    }

    /// Once its processes are gone (or have had their while), sends its END and says
    /// whether they are gone.
    fn ended(&mut self, now: Instant, port: &Port) -> Option<bool> {
        let ending = self.ending?;
        self.holder_gone = self.holder_gone || reaped(self.holder);
        let gone = self.holder_gone;
        if !gone && now < ending + LINGER {
            return None;
        }
        // Its writes on the share before the browser reads them.
        unsafe { libc::sync() };
        let result = if let Some(failure) = &self.failure {
            port.send(
                "OUT",
                &self.id,
                format!("spindle runner: {failure}\n").as_bytes(),
            );
            json!({"code": 126, "timed_out": false, "stopped": false, "lingering": !gone})
        } else {
            // Killed before the shell ended: as a killed process reports.
            let code = self.code.unwrap_or(-libc::SIGKILL);
            json!({"code": code, "timed_out": self.timed_out, "stopped": self.stopped, "lingering": !gone})
        };
        port.send("END", &self.id, result.to_string().as_bytes());
        Some(gone)
    }
}

#[derive(Clone, PartialEq)]
enum Watch {
    Port,
    Server(String),
    Client(String, u64),
    Out(String),
    Status(String),
    Holder(String),
}

/// Whether `fd` becomes readable within `wait`.
fn readable(fd: RawFd, wait: Duration) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = wait.as_millis().min(i32::MAX as u128) as c_int;
    loop {
        let found = unsafe { libc::poll(&mut poll, 1, ms) };
        if found < 0 && errno() == libc::EINTR {
            continue;
        }
        return found > 0;
    }
}

/// The CPUs for a new command: the group (of `groups`) the fewest commands use. Commands
/// whose END has gone count too while they linger: CPUs a runaway holds are the last to
/// be given again.
fn least_used(
    groups: &[Vec<usize>],
    commands: &BTreeMap<String, Command>,
    lingering: &[Lingering],
) -> usize {
    (0..groups.len())
        .min_by_key(|&group| {
            let running = commands
                .values()
                .filter(|command| command.group == group)
                .count();
            let held = lingering
                .iter()
                .filter(|found| found.group == group)
                .count();
            (held, running)
        })
        .unwrap_or(0)
}

fn answer(
    kind: &str,
    id: &str,
    payload: &[u8],
    commands: &mut BTreeMap<String, Command>,
    made: &mut u64,
    port: &Port,
    groups: &[Vec<usize>],
    group: usize,
) {
    match kind {
        "PING" => port.send("READY", id, b""),
        "RUN" if !commands.contains_key(id) => {
            *made += 1;
            let started = serde_json::from_slice::<Value>(payload)
                .map_err(|error| error.to_string())
                .and_then(|job| Command::start(id, &job, *made, group, &groups[group]));
            match started {
                Ok(command) => {
                    commands.insert(id.to_string(), command);
                }
                Err(error) => {
                    // The browser hears of it; the agent goes on.
                    port.send("OUT", id, format!("spindle runner: {error}\n").as_bytes());
                    port.send("END", id, json!({"code": 126, "timed_out": false, "stopped": false, "lingering": false}).to_string().as_bytes());
                }
            }
        }
        "STOP" => {
            if let Some(command) = commands.get_mut(id) {
                command.stopped = true;
            }
        }
        "REPLY" => {
            if let Some(command) = commands.get_mut(id)
                && command.ending.is_none()
                && let Err(error) = command.bridge.reply(payload, port)
            {
                // That command fails; the others go on.
                command.failure = Some(format!("its Spindle call failed here: {error}"));
            }
        }
        _ => {}
    }
}

/// A command's holder whose namespaces outlived its END, with its CPUs and socket.
struct Lingering {
    holder: libc::pid_t,
    group: usize,
    socket: PathBuf,
}

/// Lets go of the lingering whose holders have ended (their CPUs given again).
fn prune(lingering: &mut Vec<Lingering>) {
    lingering.retain(|found| {
        let gone = reaped(found.holder);
        if gone {
            let _ = fs::remove_file(&found.socket);
        }
        !gone
    });
}

/// As init, the agent reaps what is orphaned to it, its own helpers among them: a helper
/// still known is marked gone (its pid may be given again), so it is never waited for or
/// signalled once reaped.
fn reap_orphans(commands: &mut BTreeMap<String, Command>, lingering: &mut Vec<Lingering>) {
    loop {
        let pid = unsafe { libc::waitpid(-1, ptr::null_mut(), libc::WNOHANG) };
        if pid <= 0 {
            return;
        }
        if let Some(command) = commands
            .values_mut()
            .find(|command| command.holder == pid && !command.holder_gone)
        {
            command.holder_gone = true;
        }
        lingering.retain(|found| {
            let gone = found.holder == pid;
            if gone {
                let _ = fs::remove_file(&found.socket);
            }
            !gone
        });
    }
}

fn agent() {
    let mut port = match Port::open() {
        Ok(port) => port,
        Err(error) => {
            eprintln!("spindle-agent: {PORT}: {error}");
            std::process::exit(1)
        }
    };
    let _ = fs::create_dir_all(CALLS);
    let _ = fs::set_permissions(CALLS, fs::Permissions::from_mode(0o700));
    // What each command's socket is bound over (a file; nothing listens at it out here).
    let _ = fs::remove_file(SOCKET);
    let _ = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(SOCKET);
    port.send("READY", "0", b"");
    let mut commands: BTreeMap<String, Command> = BTreeMap::new();
    let mut made = 0u64;
    // Helpers whose namespaces outlived their command's END, with their CPUs: reaped once
    // they end.
    let mut lingering: Vec<Lingering> = Vec::new();
    let init = std::process::id() == 1;
    // CPU 0 the agent's (and the interrupts'); commands get the others.
    let groups = groups(cpus());
    pin(&[0]);
    loop {
        let now = Instant::now();
        let mut watch = vec![(port.file.as_raw_fd(), Watch::Port, false)];
        for command in commands.values() {
            command.watched(&mut watch);
        }
        let mut wait = commands.values().map(|command| command.wait(now)).min();
        // What lingers is looked at now and then, its CPUs and socket let go once it ends.
        if !lingering.is_empty() {
            wait = Some(wait.map_or(LINGER, |wait| wait.min(LINGER)));
        }
        let mut polls: Vec<libc::pollfd> = watch
            .iter()
            .map(|(fd, _, writes)| libc::pollfd {
                fd: *fd,
                events: libc::POLLIN | if *writes { libc::POLLOUT } else { 0 },
                revents: 0,
            })
            .collect();
        let ms = wait.map_or(-1, |wait| wait.as_millis().min(i32::MAX as u128) as c_int);
        let found = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, ms) };
        if found < 0 && errno() != libc::EINTR {
            panic!("poll: {}", io::Error::last_os_error());
        }
        for (poll, (_, what, _)) in polls.iter().zip(&watch) {
            if poll.revents == 0 {
                continue;
            }
            let readable = poll.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
            let writable = poll.revents & libc::POLLOUT != 0;
            match what {
                Watch::Port => {
                    for (kind, id, payload) in port.frames() {
                        prune(&mut lingering);
                        let group = least_used(&groups, &commands, &lingering);
                        answer(
                            &kind,
                            &id,
                            &payload,
                            &mut commands,
                            &mut made,
                            &port,
                            &groups,
                            group,
                        );
                    }
                }
                Watch::Server(id) => {
                    if let Some(command) = commands.get_mut(id) {
                        command.bridge.accept();
                    }
                }
                Watch::Client(id, client) => {
                    if let Some(command) = commands.get_mut(id) {
                        if writable {
                            command.bridge.flush(*client);
                        }
                        if readable {
                            command.bridge.read(*client, &port);
                        }
                    }
                }
                Watch::Out(id) => {
                    if let Some(command) = commands.get_mut(id) {
                        command.take_output(&port);
                    }
                }
                Watch::Status(id) => {
                    if let Some(command) = commands.get_mut(id) {
                        command.read_status();
                    }
                }
                // Looked at below.
                Watch::Holder(_) => {}
            }
        }
        let now = Instant::now();
        let mut done = Vec::new();
        for (id, command) in commands.iter_mut() {
            if command.ending.is_none() && command.over(now) {
                command.end();
            }
            if let Some(gone) = command.ended(now, &port) {
                done.push(id.clone());
                if gone {
                    let _ = fs::remove_file(&command.socket_path);
                } else {
                    lingering.push(Lingering {
                        holder: command.holder,
                        group: command.group,
                        socket: command.socket_path.clone(),
                    });
                }
            }
        }
        for id in done {
            commands.remove(&id);
        }
        prune(&mut lingering);
        if init {
            reap_orphans(&mut commands, &mut lingering);
        }
    }
}
