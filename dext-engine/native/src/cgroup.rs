//! Linux cgroup v2 descendant evidence. Only an owned cgroup whose `cgroup.events`
//! reports `populated 0` after `cgroup.kill` proves that no process remains in it;
//! anything else is Unknown. This never proves that external effects stopped, and a
//! same-uid descendant that can write the delegated ancestor may migrate out, so
//! adversarial code still needs the strict profile.
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeCleanup {
    /// Kernel reported the owned cgroup empty after kill.
    Emptied,
    /// No proof: unavailable, unreadable, or still populated at the deadline.
    Unknown,
}

pub const DELEGATED_ENV: &str = "DEXT_CGROUP_DELEGATED";

/// The caller's own cgroup, only when the host declares it delegated with
/// `DEXT_CGROUP_DELEGATED=1` (for example under `systemd-run --user --scope -p
/// Delegate=yes`). Writability alone is not delegation: systemd also leaves
/// non-delegated user scopes writable but owns their subtree.
pub fn delegated_root() -> Option<PathBuf> {
    if std::env::var_os(DELEGATED_ENV).as_deref() != Some(std::ffi::OsStr::new("1")) {
        return None;
    }
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let relative = text.lines().find_map(|line| line.strip_prefix("0::"))?;
    let path = Path::new("/sys/fs/cgroup").join(relative.trim().trim_start_matches('/'));
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let writable = unsafe { libc::access(c_path.as_ptr(), libc::W_OK) } == 0;
    (writable && path.join("cgroup.kill").is_file() && path.join("cgroup.events").is_file())
        .then_some(path)
}

pub struct OwnedCgroup {
    path: PathBuf,
    procs: CString,
    removed: bool,
}

impl std::fmt::Debug for OwnedCgroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedCgroup")
            .field("path", &self.path)
            .finish()
    }
}

impl OwnedCgroup {
    /// Creates a fresh child cgroup. An existing directory is refused so a foreign
    /// or stale cgroup is never adopted as evidence.
    pub fn create(parent: &Path, name: &str) -> io::Result<Self> {
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "cgroup name"));
        }
        let path = parent.join(name);
        std::fs::create_dir(&path)?;
        if !path.join("cgroup.kill").is_file() {
            let _ = std::fs::remove_dir(&path);
            return Err(io::Error::new(io::ErrorKind::Unsupported, "cgroup.kill"));
        }
        let procs = CString::new(path.join("cgroup.procs").as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "cgroup path"))?;
        Ok(Self {
            path,
            procs,
            removed: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `pre_exec` hook that moves the child into this cgroup before exec, so every
    /// descendant starts inside it. Uses only async-signal-safe syscalls.
    pub fn enter_hook(&self) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
        let procs = self.procs.clone();
        move || {
            let fd = unsafe { libc::open(procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let written = unsafe { libc::write(fd, b"0".as_ptr().cast(), 1) };
            let error = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            if written == 1 { Ok(()) } else { Err(error) }
        }
    }

    pub fn populated(&self) -> Option<bool> {
        let events = std::fs::read_to_string(self.path.join("cgroup.events")).ok()?;
        events
            .lines()
            .find_map(|line| line.strip_prefix("populated "))
            .and_then(|value| match value.trim() {
                "0" => Some(false),
                "1" => Some(true),
                _ => None,
            })
    }

    pub fn process_count(&self) -> Option<usize> {
        std::fs::read_to_string(self.path.join("cgroup.procs"))
            .ok()
            .map(|procs| procs.lines().filter(|line| !line.trim().is_empty()).count())
    }

    /// Whether the kernel's OOM killer ended a member of this cgroup.
    pub fn oom_killed(&self) -> bool {
        std::fs::read_to_string(self.path.join("memory.events"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split_once(' '))
            .any(|(name, count)| {
                matches!(name, "oom_kill" | "oom_group_kill") && count.trim() != "0"
            })
    }

    /// SIGKILLs every member (including detached/setsid descendants) without waiting.
    pub fn kill(&self) -> bool {
        std::fs::write(self.path.join("cgroup.kill"), b"1").is_ok()
    }

    /// SIGKILLs every member (including detached/setsid descendants) and waits for
    /// kernel-reported emptiness. Callers must still reap their direct child.
    pub fn terminate(&self, timeout: Duration) -> TreeCleanup {
        if !self.kill() {
            return TreeCleanup::Unknown;
        }
        let deadline = Instant::now() + timeout;
        loop {
            match self.populated() {
                Some(false) => return TreeCleanup::Emptied,
                None => return TreeCleanup::Unknown,
                Some(true) if Instant::now() >= deadline => return TreeCleanup::Unknown,
                Some(true) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    /// Removes the cgroup only after kernel-reported emptiness.
    pub fn remove(mut self) -> io::Result<()> {
        if self.populated() != Some(false) {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "cgroup populated",
            ));
        }
        std::fs::remove_dir(&self.path)?;
        self.removed = true;
        Ok(())
    }
}

impl Drop for OwnedCgroup {
    fn drop(&mut self) {
        if !self.removed {
            let _ = self.terminate(Duration::from_secs(1));
            let _ = std::fs::remove_dir(&self.path);
        }
    }
}

/// Resource controllers strict runs are capped with.
const RUN_CONTROLLERS: [&str; 2] = ["memory", "pids"];

/// A fresh cgroup for one strict run under the delegated subtree, so `cgroup.kill`
/// ends every process of the run (setsid'ed descendants included) and, when the
/// controllers could be enabled, `tasks` bounds its processes and threads while all
/// runs together share a memory cap of three quarters of physical memory (the OOM
/// killer then ends the whole run). None without delegation (`DEXT_CGROUP_DELEGATED`).
pub fn run_cgroup(tasks: u64) -> Option<(OwnedCgroup, bool)> {
    let parent = runs_parent()?;
    let mut random = [0u8; 8];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom").ok()?, &mut random).ok()?;
    let name: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let group = OwnedCgroup::create(&parent, &format!("run-{name}")).ok()?;
    let capped = std::fs::write(group.path().join("pids.max"), tasks.to_string()).is_ok();
    let _ = std::fs::write(group.path().join("memory.oom.group"), b"1");
    Some((group, capped))
}

/// `<delegated>/spindle-runs`, created once per process. cgroup v2 hands controllers
/// only from a cgroup without member processes to its children, so when the delegated
/// cgroup has not enabled them yet its members (this process and any wrapper that
/// started it in the delegated scope) first move into the leaf
/// `<delegated>/spindle-host`; owned cgroups such as image workers' are then created
/// under that leaf. Controllers that cannot be enabled leave runs kill-only.
fn runs_parent() -> Option<PathBuf> {
    static PARENT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    PARENT
        .get_or_init(|| {
            let mut root = delegated_root()?;
            // A process started inside another's `spindle-host` leaf shares its subtree
            // instead of nesting a new one inside the leaf.
            if root.file_name() == Some(std::ffi::OsStr::new("spindle-host"))
                && let Some(parent) = root.parent()
                && parent.join("spindle-runs").is_dir()
            {
                root = parent.to_path_buf();
            }
            let runs = root.join("spindle-runs");
            make_dir(&runs).ok()?;
            let leaf = root.join("spindle-host");
            // Members forked meanwhile land in the old cgroup, so a few passes are made.
            for _ in 0..3 {
                if controllers_enabled(&root) || make_dir(&leaf).is_err() {
                    break;
                }
                let members =
                    std::fs::read_to_string(root.join("cgroup.procs")).unwrap_or_default();
                for member in members.split_whitespace() {
                    let _ = std::fs::write(leaf.join("cgroup.procs"), member);
                }
                enable_controllers(&root);
            }
            if controllers_enabled(&root) {
                enable_controllers(&runs);
                if let Some(total) = memory_total() {
                    let _ = std::fs::write(runs.join("memory.max"), (total / 4 * 3).to_string());
                    // Swap would let runs past the cap at the host's expense.
                    let _ = std::fs::write(runs.join("memory.swap.max"), b"0");
                }
            }
            Some(runs)
        })
        .clone()
}

fn make_dir(path: &Path) -> io::Result<()> {
    match std::fs::create_dir(path) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => Err(error),
        _ => Ok(()),
    }
}

fn controllers_enabled(group: &Path) -> bool {
    let enabled = std::fs::read_to_string(group.join("cgroup.subtree_control")).unwrap_or_default();
    RUN_CONTROLLERS
        .iter()
        .all(|name| enabled.split_whitespace().any(|value| value == *name))
}

fn enable_controllers(group: &Path) {
    for name in RUN_CONTROLLERS {
        let _ = std::fs::write(group.join("cgroup.subtree_control"), format!("+{name}"));
    }
}

/// Physical memory in bytes, from /proc/meminfo.
fn memory_total() -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = info
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?;
    let kib: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
    kib.checked_mul(1024)
}
