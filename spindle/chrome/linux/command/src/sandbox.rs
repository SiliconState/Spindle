//! `spindle-sandbox`: puts one command of the browser Linux in namespaces of its own and
//! starts it. The runner agent (root) starts this with `posix_spawn`: a static program
//! is far cheaper to start in the emulated machine than forks of the agent's Python.
//!
//! usage: spindle-sandbox WORKDIR SOCKET NETWORK COMMAND
//!
//! Standard input is the command's, standard output and error its output; descriptor 3
//! is where this tells the agent, one line each and in no set order: `init PID` (the
//! namespace's first process, whose death ends every process of the command), `exit
//! CODE` (how bash ended: its status, or minus the signal that killed it) and `fail WHY`
//! (what kept it from starting). NETWORK is `4` when descriptor 4 is the command's own
//! network (a namespace the agent made), `-` for this Linux's. Mounts, its own:
//! WORKDIR at /workspace, fresh /tmp, /var/tmp, /dev/shm and home, its /proc, its own
//! /dev/pts (no other command's terminals), and SOCKET (the command's own Spindle
//! socket) at /run/spindle.sock. Bash runs as the commands' user in a session of its
//! own, with this program's environment.

use std::ffi::{CString, c_char, c_int, c_ulong, c_void};
use std::ptr;

const UID: u32 = 1000;
const GID: u32 = 1000;
const STATUS: c_int = 3;
const NETWORK: c_int = 4;

const CLONE_NEWNS: c_int = 0x0002_0000;
const CLONE_NEWIPC: c_int = 0x0800_0000;
const CLONE_NEWPID: c_int = 0x2000_0000;
const CLONE_NEWNET: c_int = 0x4000_0000;
const MS_NOSUID: c_ulong = 2;
const MS_NODEV: c_ulong = 4;
const MS_NOEXEC: c_ulong = 8;
const MS_BIND: c_ulong = 4096;
const MS_REC: c_ulong = 16384;
const MS_PRIVATE: c_ulong = 1 << 18;
const O_RDWR: c_int = 2;
const SIGKILL: c_int = 9;
const SIGPIPE: c_int = 13;
const PR_SET_PDEATHSIG: c_int = 1;
const SIGXFSZ: c_int = 25;
const ECHILD: c_int = 10;
const EINTR: c_int = 4;

const SCRATCH: [(&str, &str); 4] = [
    ("/tmp", "mode=1777"),
    ("/var/tmp", "mode=1777"),
    ("/dev/shm", "mode=1777"),
    ("/home/spindle", "mode=0755,uid=1000,gid=1000"),
];

unsafe extern "C" {
    fn unshare(flags: c_int) -> c_int;
    fn setns(fd: c_int, kind: c_int) -> c_int;
    fn mount(
        source: *const c_char,
        target: *const c_char,
        kind: *const c_char,
        flags: c_ulong,
        data: *const c_void,
    ) -> c_int;
    fn fork() -> c_int;
    fn setsid() -> c_int;
    fn setgroups(size: usize, list: *const u32) -> c_int;
    fn setgid(gid: u32) -> c_int;
    fn setuid(uid: u32) -> c_int;
    fn chdir(path: *const c_char) -> c_int;
    fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    fn dup2(from: c_int, to: c_int) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn write(fd: c_int, bytes: *const c_void, count: usize) -> isize;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn pause() -> c_int;
    fn signal(number: c_int, handler: usize) -> usize;
    fn prctl(option: c_int, ...) -> c_int;
    fn execve(path: *const c_char, argv: *const *const c_char, envp: *const *const c_char)
    -> c_int;
    fn __errno_location() -> *mut c_int;
    fn _exit(code: c_int) -> !;
    static environ: *const *const c_char;
}

fn errno() -> c_int {
    unsafe { *__errno_location() }
}

fn c(text: &str) -> CString {
    CString::new(text).unwrap_or_default()
}

/// Tells the agent one line; its own end gone, nothing more can be told.
fn tell(line: &str) {
    let line = format!("{line}\n");
    unsafe { write(STATUS, line.as_ptr().cast(), line.len()) };
}

/// Says why it could not start the command, and ends.
fn fail(why: &str) -> ! {
    tell(&format!("fail {why}: errno {}", errno()));
    unsafe { _exit(126) }
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
    flags: c_ulong,
    data: Option<&str>,
) {
    let (source, target, kind, data) = (source.map(c), c(target), kind.map(c), data.map(c));
    let pointer =
        |value: &Option<CString>| value.as_ref().map_or(ptr::null(), |value| value.as_ptr());
    let result = unsafe {
        mount(
            pointer(&source),
            target.as_ptr(),
            pointer(&kind),
            flags,
            pointer(&data).cast(),
        )
    };
    check(result, &format!("mount {}", target.to_string_lossy()));
}

/// How a process ended, as the agent reports it: its status, or minus its signal.
fn code(status: c_int) -> c_int {
    if status & 0x7f == 0 {
        (status >> 8) & 0xff
    } else {
        -(status & 0x7f)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, workdir, socket, network, command] = args.as_slice() else {
        fail("usage: spindle-sandbox WORKDIR SOCKET NETWORK COMMAND");
    };
    unsafe {
        check(
            unshare(CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWIPC),
            "unshare",
        );
        if network == "4" {
            check(setns(NETWORK, CLONE_NEWNET), "setns");
            close(NETWORK);
        }
    }
    mounted(None, "/", None, MS_REC | MS_PRIVATE, None);
    mounted(Some(workdir), "/workspace", None, MS_BIND, None);
    for (path, options) in SCRATCH {
        mounted(Some("tmpfs"), path, Some("tmpfs"), 0, Some(options));
    }
    // Its own terminals: /dev/ptmx finds the instance at /dev/pts beside it.
    mounted(
        Some("devpts"),
        "/dev/pts",
        Some("devpts"),
        MS_NOSUID | MS_NOEXEC,
        Some("newinstance,ptmxmode=0666,mode=0620"),
    );
    mounted(Some(socket), "/run/spindle.sock", None, MS_BIND, None);
    let init = unsafe { fork() };
    check(init, "fork");
    if init == 0 {
        first(command);
    }
    tell(&format!("init {init}"));
    // Its output is the command's alone: closed here, so it ends when theirs does.
    let null = unsafe { open(c("/dev/null").as_ptr(), O_RDWR) };
    unsafe {
        dup2(null, 1);
        dup2(null, 2);
        let mut status = 0;
        while waitpid(init, &mut status, 0) < 0 && errno() == EINTR {}
        _exit(0)
    }
}

/// The namespace's first process: its own /proc, then bash; reaps what is orphaned to
/// it until the agent kills it.
fn first(command: &str) -> ! {
    // Its holder killed before the agent knew of it: it goes too, and the command with it.
    unsafe { prctl(PR_SET_PDEATHSIG, SIGKILL) };
    mounted(
        Some("proc"),
        "/proc",
        Some("proc"),
        MS_NOSUID | MS_NODEV | MS_NOEXEC,
        None,
    );
    let bash = unsafe { fork() };
    check(bash, "fork");
    if bash == 0 {
        let path = c("/bin/bash");
        let argv = [
            c("/bin/bash"),
            c("--noprofile"),
            c("--norc"),
            c("-o"),
            c("pipefail"),
            c("-c"),
            c(command),
        ];
        let mut pointers: Vec<*const c_char> = argv.iter().map(|arg| arg.as_ptr()).collect();
        pointers.push(ptr::null());
        unsafe {
            check(setsid(), "setsid");
            check(chdir(c("/workspace").as_ptr()), "chdir");
            check(setgroups(0, ptr::null()), "setgroups");
            check(setgid(GID), "setgid");
            check(setuid(UID), "setuid");
            signal(SIGPIPE, 0);
            signal(SIGXFSZ, 0);
            // The agent's line is not the command's.
            close(STATUS);
            execve(path.as_ptr(), pointers.as_ptr(), environ);
            _exit(127)
        }
    }
    unsafe {
        let null = open(c("/dev/null").as_ptr(), O_RDWR);
        dup2(null, 1);
        dup2(null, 2);
        loop {
            let mut status = 0;
            let pid = waitpid(-1, &mut status, 0);
            if pid == bash {
                tell(&format!("exit {}", code(status)));
            } else if pid < 0 && errno() == ECHILD {
                pause();
            }
        }
    }
}
