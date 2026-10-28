//! Child process-tree lifecycle: detaching children into their own session so
//! they cannot reach the TUI's terminal, and tearing the whole tree down when
//! the root exits.

use std::io;
use std::process::Command;

// Children run in a new *session*, not merely a new process group: setsid()
// also detaches them from Dext's controlling terminal, so nothing they spawn
// can read from or paint over the TUI via /dev/tty (git credential prompts
// did exactly that — the prompt text garbled the input box while git hung on
// a terminal read that could never be answered). setsid() implies a fresh
// process group with pgid == pid, so the pgid-based cleanup in
// terminate_process_group_after_exit keeps working unchanged; setpgid is the
// fallback if setsid is ever refused.
#[cfg(unix)]
fn detach_session_pre_exec() -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    || {
        let setsid_result = unsafe { libc::setsid() };
        if setsid_result != -1 {
            return Ok(());
        }
        if unsafe { libc::setpgid(0, 0) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(unix)]
pub fn configure_std_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    unsafe {
        cmd.pre_exec(detach_session_pre_exec());
    }
}

#[cfg(windows)]
pub fn configure_std_process_group(cmd: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
}

#[cfg(not(any(unix, windows)))]
pub fn configure_std_process_group(_cmd: &mut Command) {}

#[cfg(feature = "tokio-process")]
#[cfg(unix)]
pub fn configure_tokio_process_group(cmd: &mut tokio::process::Command) {
    unsafe {
        cmd.pre_exec(detach_session_pre_exec());
    }
}

#[cfg(feature = "tokio-process")]
#[cfg(windows)]
pub fn configure_tokio_process_group(cmd: &mut tokio::process::Command) {
    cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
}

#[cfg(feature = "tokio-process")]
#[cfg(not(any(unix, windows)))]
pub fn configure_tokio_process_group(_cmd: &mut tokio::process::Command) {}

#[cfg(windows)]
fn resume_windows_process(pid: u32) -> io::Result<()> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut present = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while present {
        if entry.th32OwnerProcessID == pid {
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                let error = io::Error::last_os_error();
                unsafe {
                    CloseHandle(snapshot);
                }
                return Err(error);
            }
            let resumed = unsafe { ResumeThread(thread) };
            let error = (resumed == u32::MAX).then(io::Error::last_os_error);
            unsafe {
                CloseHandle(thread);
                CloseHandle(snapshot);
            }
            return error.map_or(Ok(()), Err);
        }
        present = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    unsafe {
        CloseHandle(snapshot);
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "suspended child thread was not found",
    ))
}

/// Caps a Windows job object holds for a whole tree: committed memory of all its
/// processes together, processes alive at once and each process's user-mode CPU
/// time. Unix callers cap trees with rlimits and cgroups instead; there these are
/// ignored.
#[derive(Clone, Copy, Debug, Default)]
pub struct TreeCaps {
    pub memory: Option<u64>,
    pub processes: Option<u32>,
    pub cpu_per_process: Option<std::time::Duration>,
}

pub struct ChildProcessTree {
    #[cfg(unix)]
    pid: u32,
    #[cfg(unix)]
    armed: std::sync::atomic::AtomicBool,
    #[cfg(windows)]
    job: usize,
    /// Completion port receiving the job's limit notifications (memory caps only).
    #[cfg(windows)]
    port: usize,
    #[cfg(windows)]
    memory_hit: std::sync::atomic::AtomicBool,
}

impl ChildProcessTree {
    pub fn for_std(child: &std::process::Child) -> io::Result<Self> {
        Self::for_std_capped(child, TreeCaps::default())
    }

    /// As `for_std`, with `caps` applied before the (suspended) Windows child resumes.
    pub fn for_std_capped(child: &std::process::Child, caps: TreeCaps) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let _ = caps;
            Ok(Self {
                pid: child.id(),
                armed: std::sync::atomic::AtomicBool::new(true),
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            Self::for_windows_process(child.as_raw_handle() as _, child.id(), caps)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (child, caps);
            Ok(Self {})
        }
    }

    #[cfg(feature = "tokio-process")]
    pub fn for_tokio(child: &tokio::process::Child) -> io::Result<Self> {
        #[cfg(unix)]
        {
            child
                .id()
                .map(|pid| Self {
                    pid,
                    armed: std::sync::atomic::AtomicBool::new(true),
                })
                .ok_or_else(|| io::Error::other("child exited before process-tree setup"))
        }
        #[cfg(windows)]
        {
            let process = child
                .raw_handle()
                .ok_or_else(|| io::Error::other("child exited before process-tree setup"))?;
            let pid = child
                .id()
                .ok_or_else(|| io::Error::other("child exited before process-tree setup"))?;
            Self::for_windows_process(process as _, pid, TreeCaps::default())
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Ok(Self {})
        }
    }

    #[cfg(windows)]
    fn for_windows_process(
        process: windows_sys::Win32::Foundation::HANDLE,
        pid: u32,
        caps: TreeCaps,
    ) -> io::Result<Self> {
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::{
                IO::CreateIoCompletionPort,
                JobObjects::{
                    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
                    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                    JOB_OBJECT_LIMIT_PROCESS_TIME, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
                    JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                    JobObjectAssociateCompletionPortInformation, JobObjectExtendedLimitInformation,
                    SetInformationJobObject,
                },
            },
        };

        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        // Owned from here: dropping it closes the job (killing what it holds) and port.
        let mut tree = Self {
            job: job as usize,
            port: 0,
            memory_hit: std::sync::atomic::AtomicBool::new(false),
        };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Some(memory) = caps.memory {
            limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            limits.JobMemoryLimit = usize::try_from(memory).unwrap_or(usize::MAX);
        }
        if let Some(processes) = caps.processes {
            limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            limits.BasicLimitInformation.ActiveProcessLimit = processes.max(1);
        }
        if let Some(cpu) = caps.cpu_per_process {
            // In 100-nanosecond units.
            limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_TIME;
            limits.BasicLimitInformation.PerProcessUserTimeLimit =
                i64::try_from(cpu.as_nanos() / 100).unwrap_or(i64::MAX);
        }
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if configured == 0 {
            return Err(io::Error::last_os_error());
        }
        if caps.memory.is_some() {
            // A cap that makes allocations fail is reported as such, not as a crash.
            let port =
                unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
            if port.is_null() {
                return Err(io::Error::last_os_error());
            }
            tree.port = port as usize;
            let association = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
                CompletionKey: job,
                CompletionPort: port,
            };
            let associated = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectAssociateCompletionPortInformation,
                    std::ptr::from_ref(&association).cast(),
                    std::mem::size_of_val(&association) as u32,
                )
            };
            if associated == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        if unsafe { AssignProcessToJobObject(job, process) } == 0 {
            return Err(io::Error::last_os_error());
        }
        resume_windows_process(pid)?;
        Ok(tree)
    }

    /// Whether the tree's memory cap made an allocation fail (Windows job memory caps;
    /// elsewhere callers learn this from their cgroup).
    pub fn memory_exhausted(&self) -> bool {
        #[cfg(windows)]
        {
            use std::sync::atomic::Ordering;
            use windows_sys::Win32::System::{
                IO::GetQueuedCompletionStatus,
                SystemServices::{
                    JOB_OBJECT_MSG_JOB_MEMORY_LIMIT, JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT,
                },
            };
            if self.port != 0 {
                let (mut message, mut key, mut detail) = (0u32, 0usize, std::ptr::null_mut());
                while unsafe {
                    GetQueuedCompletionStatus(
                        self.port as _,
                        &mut message,
                        &mut key,
                        &mut detail,
                        0,
                    )
                } != 0
                {
                    if matches!(
                        message,
                        JOB_OBJECT_MSG_JOB_MEMORY_LIMIT | JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT
                    ) {
                        self.memory_hit.store(true, Ordering::Relaxed);
                    }
                }
            }
            self.memory_hit.load(Ordering::Relaxed)
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    pub fn terminate_after_root_exit(&self) {
        #[cfg(unix)]
        {
            signal_process_group(self.pid, libc::SIGTERM);
            signal_process_group(self.pid, libc::SIGKILL);
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job as _, 1);
        }
        #[cfg(unix)]
        self.armed
            .store(false, std::sync::atomic::Ordering::Release);
    }

    pub fn terminate_std_child(&self, child: &mut std::process::Child) {
        #[cfg(unix)]
        {
            signal_process_group(self.pid, libc::SIGTERM);
            std::thread::sleep(std::time::Duration::from_millis(50));
            signal_process_group(self.pid, libc::SIGKILL);
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job as _, 1);
        }
        let _ = child.kill();
        let _ = child.wait();
        #[cfg(unix)]
        self.armed
            .store(false, std::sync::atomic::Ordering::Release);
    }

    #[cfg(feature = "tokio-process")]
    pub async fn terminate_tokio_child(&self, child: &mut tokio::process::Child) {
        #[cfg(unix)]
        {
            signal_process_group(self.pid, libc::SIGTERM);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            signal_process_group(self.pid, libc::SIGKILL);
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job as _, 1);
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
        #[cfg(unix)]
        self.armed
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

impl Drop for ChildProcessTree {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.armed.load(std::sync::atomic::Ordering::Acquire) {
            signal_process_group(self.pid, libc::SIGTERM);
            signal_process_group(self.pid, libc::SIGKILL);
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job as _);
            if self.port != 0 {
                windows_sys::Win32::Foundation::CloseHandle(self.port as _);
            }
        }
    }
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: libc::c_int) {
    let pgid = -(pid as libc::pid_t);
    unsafe {
        let _ = libc::kill(pgid, signal);
    }
}
