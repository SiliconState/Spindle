//! Dext-owned image worker implementation and bounded supervisor, not agent policy.
pub use dext_core::image::{ENCODED_BYTE_CAP, LONGEST_SIDE_CAP};
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder as _, ImageFormat, Rgb, RgbImage};
use std::io::{Cursor, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};
pub const SOURCE_BYTE_CAP: usize = 20 * 1024 * 1024;
pub const DECODED_PIXEL_CAP: u64 = 40_000_000;
pub const WORKER_ARG: &str = "--dext-image-worker";
const BACKGROUND: [u8; 3] = [240, 240, 240];
const WORKER_MEMORY_CAP: u64 = 768 * 1024 * 1024;
const WORKER_CPU_SECONDS: u64 = 10;
const WORKER_TIMEOUT: Duration = Duration::from_secs(15);
const WORKER_HEADER_BYTES: usize = 8;
static WORKER_SLOT: Mutex<()> = Mutex::new(());

fn bounded_error(error: &str) -> &str {
    let mut end = error.len().min(4000);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    &error[..end]
}

#[cfg(unix)]
fn worker_resource_limits() -> Result<(), String> {
    for (resource, value) in [
        #[cfg(not(target_os = "macos"))]
        (libc::RLIMIT_AS, WORKER_MEMORY_CAP),
        (libc::RLIMIT_CPU, WORKER_CPU_SECONDS),
        (libc::RLIMIT_CORE, 0),
    ] {
        let mut inherited: libc::rlimit = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrlimit(resource, &mut inherited) } != 0 {
            return Err("could not inspect inherited image worker limits".to_string());
        }
        let cap = if inherited.rlim_cur == libc::RLIM_INFINITY {
            value as libc::rlim_t
        } else {
            (value as libc::rlim_t).min(inherited.rlim_cur)
        };
        let cap = if inherited.rlim_max == libc::RLIM_INFINITY {
            cap
        } else {
            cap.min(inherited.rlim_max)
        };
        let limit = libc::rlimit {
            rlim_cur: cap,
            rlim_max: cap,
        };
        if unsafe { libc::setrlimit(resource, &limit) } != 0 {
            return Err(format!(
                "could not limit image worker: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn worker_resource_limits() -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::*;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err("could not create image worker resource job".to_string());
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_PROCESS_TIME
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    limits.BasicLimitInformation.PerProcessUserTimeLimit = (WORKER_CPU_SECONDS * 10_000_000) as i64;
    limits.ProcessMemoryLimit = WORKER_MEMORY_CAP as usize;
    let configured = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limits) as u32,
        ) != 0
            && AssignProcessToJobObject(job, GetCurrentProcess()) != 0
    };
    if !configured {
        unsafe {
            CloseHandle(job);
        }
        return Err("could not apply image worker resource job".to_string());
    }
    // The worker exits through process::exit; retaining the handle keeps the job alive.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn worker_resource_limits() -> Result<(), String> {
    Err("image worker resource isolation is unsupported on this platform".to_string())
}

pub fn worker_main() -> i32 {
    let result = (|| -> Result<(), String> {
        if std::env::args_os().count() != 2 {
            return Err("image worker accepts no paths or options".to_string());
        }
        worker_resource_limits()?;
        let mut bytes = Vec::new();
        std::io::stdin()
            .take((SOURCE_BYTE_CAP + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("could not read image worker input: {error}"))?;
        if bytes.len() > SOURCE_BYTE_CAP {
            return Err("image worker source byte limit exceeded".to_string());
        }
        let (width, height, encoded) = decode_in_worker(&bytes)?;
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&width.to_be_bytes())
            .and_then(|_| stdout.write_all(&height.to_be_bytes()))
            .and_then(|_| stdout.write_all(&encoded))
            .and_then(|_| stdout.flush())
            .map_err(|error| format!("could not write image worker output: {error}"))
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{}", bounded_error(&error));
            1
        }
    }
}

struct WorkerChild(Child, Option<crate::process_tree::ChildProcessTree>);

impl Drop for WorkerChild {
    fn drop(&mut self) {
        if let Some(tree) = self.1.take() {
            tree.terminate_std_child(&mut self.0);
        } else {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[cfg(target_os = "linux")]
fn pin_worker_owner(owner: libc::pid_t) -> std::io::Result<()> {
    // Linux ties this signal to the creating thread, not the whole process. The
    // synchronous supervisor keeps that thread alive until the worker is reaped.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Owner death before prctl would otherwise leave an unsignalled orphan.
    if unsafe { libc::getppid() } != owner {
        return Err(std::io::Error::from_raw_os_error(libc::ESTALE));
    }
    Ok(())
}

fn worker_command(executable: &Path) -> Command {
    let mut command = Command::new(executable);
    command.arg(WORKER_ARG).env_clear();
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[cfg(target_os = "macos")]
fn worker_memory_within_limit(child: &Child) -> Result<bool, String> {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            child.id() as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            size,
        )
    };
    if read != size {
        return Ok(false);
    }
    if info.pti_resident_size > WORKER_MEMORY_CAP {
        return Err("image worker exceeded its resident-memory limit".to_string());
    }
    Ok(true)
}

#[cfg(any(target_os = "macos", test))]
#[derive(Default)]
struct WorkerMemoryMonitor {
    unavailable_since: Option<Instant>,
}

#[cfg(any(target_os = "macos", test))]
impl WorkerMemoryMonitor {
    fn observe(&mut self, sample: Result<bool, String>, now: Instant) -> Result<(), String> {
        if sample? {
            self.unavailable_since = None;
        } else if now.duration_since(*self.unavailable_since.get_or_insert(now))
            >= Duration::from_millis(50)
        {
            return Err("could not inspect image worker memory".to_string());
        }
        Ok(())
    }
}

pub fn check_interrupt(interrupt: Option<&AtomicBool>) -> Result<(), String> {
    if interrupt.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        Err("image worker interrupted by user".to_string())
    } else {
        Ok(())
    }
}

fn run_worker(
    command: &mut Command,
    bytes: &[u8],
    timeout: Duration,
    interrupt: Option<&AtomicBool>,
) -> Result<Vec<u8>, String> {
    run_worker_tracked(
        command,
        bytes,
        timeout,
        interrupt,
        &mut RootCleanup::NotStarted,
    )
}

fn run_worker_tracked(
    command: &mut Command,
    bytes: &[u8],
    timeout: Duration,
    interrupt: Option<&AtomicBool>,
    cleanup: &mut RootCleanup,
) -> Result<Vec<u8>, String> {
    check_interrupt(interrupt)?;
    if bytes.len() > SOURCE_BYTE_CAP {
        return Err("image worker source byte limit exceeded".to_string());
    }
    let start = Instant::now();
    crate::process_tree::configure_std_process_group(command);
    #[cfg(target_os = "linux")]
    let group = owned_worker_cgroup();
    #[cfg(target_os = "linux")]
    if let Some(group) = &group {
        use std::os::unix::process::CommandExt;
        unsafe { command.pre_exec(group.enter_hook()) };
    }
    std::thread::scope(|scope| {
        // Drop the child before scope joins on an early return or thread-spawn panic.
        *cleanup = RootCleanup::Unknown;
        let mut child = WorkerChild(
            command
                .spawn()
                .map_err(|error| format!("could not start image worker: {error}"))?,
            None,
        );
        child.1 = Some(
            crate::process_tree::ChildProcessTree::for_std(&child.0)
                .map_err(|error| format!("could not contain image worker: {error}"))?,
        );
        let stdin = child.0.stdin.take().ok_or("image worker has no stdin")?;
        let stdout = child.0.stdout.take().ok_or("image worker has no stdout")?;
        let stderr = child.0.stderr.take().ok_or("image worker has no stderr")?;
        let writer = scope.spawn(move || {
            let mut stdin = stdin;
            stdin.write_all(bytes)
        });
        let reader = scope.spawn(move || {
            let mut output = Vec::new();
            stdout
                .take((ENCODED_BYTE_CAP + WORKER_HEADER_BYTES + 1) as u64)
                .read_to_end(&mut output)
                .map(|_| output)
        });
        let errors = scope.spawn(move || {
            let mut output = Vec::new();
            stderr.take(4096).read_to_end(&mut output).map(|_| output)
        });
        #[cfg(target_os = "macos")]
        let mut memory_monitor = WorkerMemoryMonitor::default();
        let status = loop {
            if let Err(error) = check_interrupt(interrupt) {
                break Err(error);
            }
            match child.0.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if start.elapsed() < timeout => {
                    #[cfg(target_os = "macos")]
                    // A worker can exit between try_wait and proc_pidinfo; allow
                    // a bounded reap window without accepting an uninspectable live child.
                    if let Err(error) =
                        memory_monitor.observe(worker_memory_within_limit(&child.0), Instant::now())
                    {
                        break Err(error);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(None) => break Err("image worker exceeded its wall-clock limit".to_string()),
                Err(error) => break Err(format!("could not wait for image worker: {error}")),
            }
        };
        // Descendants must close inherited pipes before the bounded readers are joined.
        if let Some(tree) = child.1.take() {
            if status.is_err() {
                tree.terminate_std_child(&mut child.0);
            } else {
                tree.terminate_after_root_exit();
            }
        }
        // cgroup.kill also reaches setsid/double-forked descendants that escaped
        // the process group, before the bounded pipe readers are joined.
        #[cfg(target_os = "linux")]
        let tree = group
            .as_ref()
            .map(|group| group.terminate(Duration::from_secs(2)));
        #[cfg(not(target_os = "linux"))]
        let tree: Option<()> = None;
        match child.0.wait() {
            Ok(_) => {
                *cleanup = RootCleanup::Reaped;
                #[cfg(target_os = "linux")]
                if tree == Some(crate::cgroup::TreeCleanup::Emptied) {
                    *cleanup = RootCleanup::TreeEmptied;
                }
                #[cfg(not(target_os = "linux"))]
                let _ = tree;
            }
            Err(error) => return Err(format!("could not reap image worker: {error}")),
        }
        let written = writer
            .join()
            .map_err(|_| "image worker input thread failed")?;
        let output = reader
            .join()
            .map_err(|_| "image worker output thread failed")?
            .map_err(|error| format!("could not read image worker output: {error}"))?;
        let errors = errors
            .join()
            .map_err(|_| "image worker error thread failed")?
            .map_err(|error| format!("could not read image worker errors: {error}"))?;
        let status = status?;
        if !status.success() {
            let detail = String::from_utf8_lossy(&errors);
            return Err(format!("image worker failed ({status}): {}", detail.trim()));
        }
        written.map_err(|error| format!("could not write image worker input: {error}"))?;
        check_interrupt(interrupt)?;
        Ok(output)
    })
}

/// Best effort: absent delegation keeps root-only evidence rather than failing.
#[cfg(target_os = "linux")]
fn owned_worker_cgroup() -> Option<crate::cgroup::OwnedCgroup> {
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let root = crate::cgroup::delegated_root()?;
    let name = format!(
        "dext-image-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    );
    crate::cgroup::OwnedCgroup::create(&root, &name).ok()
}

fn worker_output(output: Vec<u8>) -> Result<(u32, u32, Vec<u8>), String> {
    if output.len() <= WORKER_HEADER_BYTES || output.len() > ENCODED_BYTE_CAP + WORKER_HEADER_BYTES
    {
        return Err("image worker output byte limit or framing is invalid".to_string());
    }
    let width = u32::from_be_bytes(output[..4].try_into().expect("worker width"));
    let height = u32::from_be_bytes(output[4..8].try_into().expect("worker height"));
    if width == 0 || height == 0 || width.max(height) > LONGEST_SIDE_CAP {
        return Err("image worker output dimensions are invalid".to_string());
    }
    let encoded = output[WORKER_HEADER_BYTES..].to_vec();
    if !dext_core::image::sanitized_jpeg_matches_dimensions(&encoded, width, height) {
        return Err("image worker output is not a sanitized JPEG".to_string());
    }
    Ok((width, height, encoded))
}

fn acquire_worker_slot<'a>(
    slot: &'a Mutex<()>,
    interrupt: Option<&AtomicBool>,
    timeout: Duration,
) -> Result<MutexGuard<'a, ()>, String> {
    let started = Instant::now();
    loop {
        check_interrupt(interrupt)?;
        match slot.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(_)) => {
                return Err("image worker slot is unavailable".to_string());
            }
            Err(TryLockError::WouldBlock) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(TryLockError::WouldBlock) => {
                return Err("image worker slot wait exceeded its wall-clock limit".to_string());
            }
        }
    }
}

fn worker_executable() -> Result<PathBuf, String> {
    // current_exe() can name an unlinked inode after cargo install replaces Dext.
    #[cfg(target_os = "linux")]
    {
        Ok(PathBuf::from("/proc/self/exe"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::current_exe().map_err(|error| format!("could not locate image worker: {error}"))
    }
}

pub fn sanitize_current_executable(
    bytes: &[u8],
    interrupt: Option<&AtomicBool>,
) -> Result<(u32, u32, Vec<u8>), String> {
    let _slot = acquire_worker_slot(&WORKER_SLOT, interrupt, WORKER_TIMEOUT)?;
    let mut command = worker_command(&worker_executable()?);
    worker_output(run_worker(&mut command, bytes, WORKER_TIMEOUT, interrupt)?)
}

/// An explicitly approved worker executable, snapshotted into sealed memory on
/// Linux so pathname/inode replacement cannot change the bytes actually executed.
/// This pins identity, not trust: the host must review the expected executable hash.
/// Linux launches arm a parent-death SIGKILL for the root worker only. Approved
/// code must not clear it; descendants and strict OS containment are not covered.
pub struct PinnedWorker {
    #[cfg(target_os = "linux")]
    executable: std::fs::File,
    digest: [u8; 32],
}

impl std::fmt::Debug for PinnedWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedWorker").finish_non_exhaustive()
    }
}

/// In-process evidence from one successful bounded invocation of a pinned worker.
/// No public constructor, deserializer or mutable byte access can forge provenance.
pub struct SanitizedImage {
    width: u32,
    height: u32,
    bytes: Vec<u8>,
    source_digest: [u8; 32],
    worker_digest: [u8; 32],
}
impl std::fmt::Debug for SanitizedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SanitizedImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.bytes.len())
            .finish_non_exhaustive()
    }
}
impl SanitizedImage {
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn source_digest(&self) -> &[u8; 32] {
        &self.source_digest
    }
    pub fn worker_digest(&self) -> &[u8; 32] {
        &self.worker_digest
    }
}

/// Native worker evidence. Reaped covers the root only; TreeEmptied additionally
/// means the kernel reported the worker's owned cgroup empty after `cgroup.kill`.
/// Neither covers external effects; Unknown never becomes safe because a PID left.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootCleanup {
    NotStarted,
    Reaped,
    TreeEmptied,
    Unknown,
}

/// Created only by the bounded pinned execution path, including failed execution.
/// No constructor/deserializer exposes a way to claim successful root cleanup.
pub struct ImageExecution {
    result: Result<SanitizedImage, String>,
    cleanup: RootCleanup,
    worker: [u8; 32],
}
impl std::fmt::Debug for ImageExecution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageExecution")
            .field("cleanup", &self.cleanup)
            .field("success", &self.result.is_ok())
            .finish_non_exhaustive()
    }
}
impl ImageExecution {
    pub fn cleanup(&self) -> RootCleanup {
        self.cleanup
    }
    pub fn worker_digest(&self) -> &[u8; 32] {
        &self.worker
    }
    pub fn into_result(self) -> Result<SanitizedImage, String> {
        self.result
    }
}

impl PinnedWorker {
    pub const MAX_EXECUTABLE_BYTES: usize = 128 * 1024 * 1024;

    /// The expected SHA-256 comes from a trusted host configuration, never from
    /// an untrusted receipt. Unsupported platforms refuse rather than race a path.
    pub fn open(path: &Path, expected: [u8; 32]) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        {
            use sha2::{Digest, Sha256};
            use std::os::fd::{AsRawFd, FromRawFd};
            use std::os::unix::fs::OpenOptionsExt;
            let mut source = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)
                .map_err(|_| "could not open approved image worker")?;
            let metadata = source
                .metadata()
                .map_err(|_| "could not inspect image worker")?;
            if !metadata.is_file()
                || metadata.len() == 0
                || metadata.len() > Self::MAX_EXECUTABLE_BYTES as u64
            {
                return Err("image worker executable size/type is invalid".into());
            }
            let fd = unsafe {
                libc::memfd_create(
                    c"dext-image-worker".as_ptr(),
                    libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
                )
            };
            if fd < 0 {
                return Err("could not snapshot image worker".into());
            }
            let mut executable = unsafe { std::fs::File::from_raw_fd(fd) };
            let mut hash = Sha256::new();
            let mut total = 0usize;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let count = source
                    .read(&mut buffer)
                    .map_err(|_| "could not read image worker")?;
                if count == 0 {
                    break;
                }
                total = total
                    .checked_add(count)
                    .ok_or("image worker size overflow")?;
                if total > Self::MAX_EXECUTABLE_BYTES {
                    return Err("image worker executable exceeds bound".into());
                }
                hash.update(&buffer[..count]);
                executable
                    .write_all(&buffer[..count])
                    .map_err(|_| "could not snapshot image worker")?;
            }
            let digest: [u8; 32] = hash.finalize().into();
            if digest != expected {
                return Err("image worker executable identity mismatch".into());
            }
            let seals =
                libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
            if unsafe { libc::fchmod(executable.as_raw_fd(), 0o500) } != 0
                || unsafe { libc::fcntl(executable.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0
            {
                return Err("could not seal image worker executable".into());
            }
            Ok(Self { executable, digest })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (path, expected);
            Err("pinned image worker execution is supported only on Linux".into())
        }
    }

    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    /// Reads an already-authorized regular file handle, never reopens its path.
    /// The approved digest binds the exact bounded snapshot sent to the worker.
    pub fn sanitize_file(
        &self,
        file: &std::fs::File,
        approved_source: [u8; 32],
        interrupt: Option<&AtomicBool>,
    ) -> Result<SanitizedImage, String> {
        self.sanitize_file_report(file, approved_source, interrupt)
            .into_result()
    }

    pub fn sanitize_file_report(
        &self,
        file: &std::fs::File,
        approved_source: [u8; 32],
        interrupt: Option<&AtomicBool>,
    ) -> ImageExecution {
        let mut cleanup = RootCleanup::NotStarted;
        let result = self.sanitize_file_tracked(file, approved_source, interrupt, &mut cleanup);
        ImageExecution {
            result,
            cleanup,
            worker: self.digest,
        }
    }

    fn sanitize_file_tracked(
        &self,
        file: &std::fs::File,
        approved_source: [u8; 32],
        interrupt: Option<&AtomicBool>,
        cleanup: &mut RootCleanup,
    ) -> Result<SanitizedImage, String> {
        check_interrupt(interrupt)?;
        #[cfg(target_os = "linux")]
        {
            use sha2::{Digest, Sha256};
            use std::os::unix::fs::FileExt;
            let metadata = file
                .metadata()
                .map_err(|_| "could not inspect image source")?;
            if !metadata.is_file() || metadata.len() == 0 || metadata.len() > SOURCE_BYTE_CAP as u64
            {
                return Err("image source type/size is invalid".into());
            }
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            let mut buffer = [0u8; 16 * 1024];
            loop {
                check_interrupt(interrupt)?;
                let count = match file.read_at(&mut buffer, bytes.len() as u64) {
                    Ok(count) => count,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return Err("could not read image source".into()),
                };
                if count == 0 {
                    break;
                }
                if count > SOURCE_BYTE_CAP.saturating_sub(bytes.len()) {
                    return Err("image source exceeds byte limit".into());
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            if <[u8; 32]>::from(Sha256::digest(&bytes)) != approved_source {
                return Err("image source changed since approval".into());
            }
            self.sanitize_tracked(&bytes, interrupt, cleanup)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (file, approved_source, cleanup);
            Err("pinned image source execution is supported only on Linux".into())
        }
    }

    pub fn sanitize(
        &self,
        bytes: &[u8],
        interrupt: Option<&AtomicBool>,
    ) -> Result<SanitizedImage, String> {
        self.sanitize_tracked(bytes, interrupt, &mut RootCleanup::NotStarted)
    }

    fn sanitize_tracked(
        &self,
        bytes: &[u8],
        interrupt: Option<&AtomicBool>,
        cleanup: &mut RootCleanup,
    ) -> Result<SanitizedImage, String> {
        check_interrupt(interrupt)?;
        if bytes.is_empty() || bytes.len() > SOURCE_BYTE_CAP {
            return Err("image source byte limit is invalid".into());
        }
        #[cfg(target_os = "linux")]
        {
            use sha2::{Digest, Sha256};
            use std::os::fd::AsRawFd;
            use std::os::unix::process::CommandExt;
            let _slot = acquire_worker_slot(&WORKER_SLOT, interrupt, WORKER_TIMEOUT)?;
            let path = PathBuf::from(format!("/proc/self/fd/{}", self.executable.as_raw_fd()));
            let mut command = worker_command(&path);
            let owner = unsafe { libc::getpid() };
            unsafe {
                command.pre_exec(move || pin_worker_owner(owner));
            }
            let (width, height, encoded) = worker_output(run_worker_tracked(
                &mut command,
                bytes,
                WORKER_TIMEOUT,
                interrupt,
                cleanup,
            )?)?;
            Ok(SanitizedImage {
                width,
                height,
                bytes: encoded,
                source_digest: Sha256::digest(bytes).into(),
                worker_digest: self.digest,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = cleanup;
            Err("pinned image worker execution is supported only on Linux".into())
        }
    }
}

pub fn png_is_animated(bytes: &[u8]) -> bool {
    let mut offset = 8usize;
    while let Some(header_end) = offset.checked_add(8).filter(|end| *end <= bytes.len()) {
        let length = u32::from_be_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("PNG chunk length is four bytes"),
        ) as usize;
        let kind = &bytes[offset + 4..header_end];
        if kind == b"acTL" {
            return true;
        }
        let Some(next) = header_end
            .checked_add(length)
            .and_then(|end| end.checked_add(4))
            .filter(|end| *end <= bytes.len())
        else {
            return false;
        };
        offset = next;
        if kind == b"IEND" {
            return false;
        }
    }
    false
}

fn webp_is_animated(bytes: &[u8]) -> bool {
    let mut offset = 12usize;
    while let Some(header_end) = offset.checked_add(8).filter(|end| *end <= bytes.len()) {
        let kind = &bytes[offset..offset + 4];
        let length = u32::from_le_bytes(
            bytes[offset + 4..header_end]
                .try_into()
                .expect("WebP chunk length is four bytes"),
        ) as usize;
        if kind == b"ANIM"
            || kind == b"ANMF"
            || kind == b"VP8X"
                && length >= 1
                && bytes.get(header_end).is_some_and(|flags| flags & 0x02 != 0)
        {
            return true;
        }
        let Some(next) = header_end
            .checked_add(length)
            .and_then(|end| end.checked_add(length & 1))
            .filter(|end| *end <= bytes.len())
        else {
            return false;
        };
        offset = next;
    }
    false
}

fn sniff_format(bytes: &[u8]) -> Result<ImageFormat, String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        if png_is_animated(bytes) {
            return Err("animated PNG is not supported; convert one intended frame explicitly before read_image".to_string());
        }
        Ok(ImageFormat::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Ok(ImageFormat::Jpeg)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        if webp_is_animated(bytes) {
            return Err("animated WebP is not supported; convert one intended frame explicitly before read_image".to_string());
        }
        Ok(ImageFormat::WebP)
    } else {
        Err("unsupported image format; read_image accepts only PNG, JPEG, and WebP. Convert other images explicitly or use OCR".to_string())
    }
}

fn decoder_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(u32::try_from(DECODED_PIXEL_CAP).unwrap_or(u32::MAX));
    limits.max_image_height = Some(u32::try_from(DECODED_PIXEL_CAP).unwrap_or(u32::MAX));
    limits.max_alloc = Some(DECODED_PIXEL_CAP.saturating_mul(5));
    limits
}

fn checked_dimensions(bytes: &[u8], format: ImageFormat) -> Result<(u32, u32), String> {
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(decoder_limits());
    let (width, height) = reader
        .into_dimensions()
        .map_err(|error| format!("could not read image dimensions: {error}"))?;
    if width == 0 || height == 0 {
        return Err("image dimensions must be non-zero".to_string());
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| "image dimensions overflow the decoded-pixel limit".to_string())?;
    if pixels > DECODED_PIXEL_CAP {
        return Err(format!(
            "image has {pixels} decoded pixels, exceeding the {DECODED_PIXEL_CAP} pixel limit"
        ));
    }
    Ok((width, height))
}

fn composite_neutral(image: DynamicImage) -> RgbImage {
    let rgba = image.into_rgba8();
    let mut rgb = RgbImage::new(rgba.width(), rgba.height());
    for (x, y, pixel) in rgba.enumerate_pixels() {
        let alpha = u16::from(pixel[3]);
        let inverse = 255 - alpha;
        rgb.put_pixel(
            x,
            y,
            Rgb([
                ((u16::from(pixel[0]) * alpha + u16::from(BACKGROUND[0]) * inverse + 127) / 255)
                    as u8,
                ((u16::from(pixel[1]) * alpha + u16::from(BACKGROUND[1]) * inverse + 127) / 255)
                    as u8,
                ((u16::from(pixel[2]) * alpha + u16::from(BACKGROUND[2]) * inverse + 127) / 255)
                    as u8,
            ]),
        );
    }
    rgb
}

fn bounded_jpeg(image: &RgbImage) -> Result<Vec<u8>, String> {
    for quality in [88, 82, 76, 68, 58, 48, 38, 30] {
        let mut encoded = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, quality)
            .encode(
                image.as_raw(),
                image.width(),
                image.height(),
                image::ExtendedColorType::Rgb8,
            )
            .map_err(|error| format!("could not encode sanitized image: {error}"))?;
        if encoded.len() <= ENCODED_BYTE_CAP {
            return Ok(encoded);
        }
    }
    Err(format!(
        "sanitized image still exceeds the {ENCODED_BYTE_CAP} byte transmission limit"
    ))
}

fn decode_bounded(bytes: &[u8], format: ImageFormat) -> Result<DynamicImage, String> {
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(decoder_limits());
    let mut decoder = reader
        .into_decoder()
        .map_err(|error| format!("could not initialize bounded image decoder: {error}"))?;
    let orientation = decoder
        .orientation()
        .map_err(|error| format!("could not read image orientation: {error}"))?;
    let mut decoded = DynamicImage::from_decoder(decoder)
        .map_err(|error| format!("could not decode image: {error}"))?;
    decoded.apply_orientation(orientation);
    Ok(decoded)
}

fn resize_bounded(decoded: DynamicImage) -> DynamicImage {
    if decoded.width().max(decoded.height()) <= LONGEST_SIDE_CAP {
        return decoded;
    }
    // image's separable resize samples vertically first with a full-width f32
    // intermediate. Rotate wide sources so that intermediate uses the short axis.
    if decoded.width() > decoded.height() {
        let rotated = decoded.rotate90();
        drop(decoded);
        rotated
            .resize(LONGEST_SIDE_CAP, LONGEST_SIDE_CAP, FilterType::Lanczos3)
            .rotate270()
    } else {
        decoded.resize(LONGEST_SIDE_CAP, LONGEST_SIDE_CAP, FilterType::Lanczos3)
    }
}

/// Decoder primitive for resource-limited worker entry points. Hosts use the
/// supervisor, never this function as an in-process fallback. This returns no
/// authenticated SanitizedImage token.
pub fn decode_in_worker(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let format = sniff_format(bytes)?;
    checked_dimensions(bytes, format)?;
    let decoded = decode_bounded(bytes, format)?;
    // Composite before resampling so hidden RGB from transparent pixels cannot
    // bleed into visible neighbors or be disclosed by an opaque output.
    let opaque = DynamicImage::ImageRgb8(composite_neutral(decoded));
    let rgb = resize_bounded(opaque).into_rgb8();
    let encoded = bounded_jpeg(&rgb)?;
    Ok((rgb.width(), rgb.height(), encoded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};
    #[test]
    fn worker_command_never_inherits_credentials_or_opt_in() {
        let command = worker_command(Path::new("dext"));
        let environment: Vec<_> = command.get_envs().collect();
        assert!(environment.iter().all(|(key, _)| *key == "SystemRoot"));
        assert_eq!(command.get_args().collect::<Vec<_>>(), [WORKER_ARG]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_owner_fence_refuses_a_parent_that_changed_before_setup() {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("/bin/true");
        unsafe {
            command.pre_exec(|| pin_worker_owner(0));
        }
        assert_eq!(
            command.spawn().unwrap_err().raw_os_error(),
            Some(libc::ESTALE)
        );
    }

    #[test]
    fn worker_output_rejects_bad_framing_dimensions_and_payloads() {
        assert!(worker_output(Vec::new()).is_err());
        assert!(worker_output(vec![0; ENCODED_BYTE_CAP + WORKER_HEADER_BYTES + 1]).is_err());
        let output = |width: u32, height: u32, bytes: &[u8]| {
            [
                width.to_be_bytes().as_slice(),
                height.to_be_bytes().as_slice(),
                bytes,
            ]
            .concat()
        };
        let jpeg = bounded_jpeg(&RgbImage::from_pixel(1, 1, Rgb([12, 34, 56]))).unwrap();
        assert!(worker_output(output(0, 1, &jpeg)).is_err());
        assert!(worker_output(output(LONGEST_SIDE_CAP + 1, 1, &jpeg)).is_err());
        assert!(worker_output(output(1, 1, b"not pixels")).is_err());
        assert!(worker_output(output(1, 1, &[0xff, 0xd8, 0xff, 0xd9])).is_err());
        assert!(worker_output(output(2, 1, &jpeg)).is_err());
        assert!(worker_output(output(1, 1, &jpeg)).is_ok());
        for length in 0..jpeg.len() {
            assert!(worker_output(output(1, 1, &jpeg[..length])).is_err());
        }
        let mut noisy = RgbImage::new(96, 64);
        let mut state = 1u32;
        for pixel in noisy.pixels_mut() {
            for channel in &mut pixel.0 {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *channel = (state >> 24) as u8;
            }
        }
        let noisy = bounded_jpeg(&noisy).unwrap();
        assert!(noisy.windows(2).any(|part| part == [0xff, 0x00]));
        assert!(worker_output(output(96, 64, &noisy)).is_ok());
        let mut metadata = jpeg.clone();
        metadata.splice(2..2, [0xff, 0xe1, 0, 8, b'E', b'x', b'i', b'f', 0, 0]);
        assert!(worker_output(output(1, 1, &metadata)).is_err());
        let mut trailing = jpeg.clone();
        trailing.extend_from_slice(&[0xff, 0xd9]);
        assert!(worker_output(output(1, 1, &trailing)).is_err());
    }

    #[test]
    fn worker_memory_monitor_bounds_missing_samples_and_resets_on_success() {
        let mut monitor = WorkerMemoryMonitor::default();
        let start = Instant::now();
        assert!(monitor.observe(Ok(false), start).is_ok());
        assert!(
            monitor
                .observe(Ok(false), start + Duration::from_millis(49))
                .is_ok()
        );
        assert!(
            monitor
                .observe(Ok(false), start + Duration::from_millis(50))
                .unwrap_err()
                .contains("could not inspect")
        );
        assert!(
            monitor
                .observe(Ok(true), start + Duration::from_millis(51))
                .is_ok()
        );
        assert!(
            monitor
                .observe(Ok(false), start + Duration::from_millis(100))
                .is_ok()
        );
        let error = "image worker exceeded its resident-memory limit";
        assert_eq!(
            monitor.observe(Err(error.into()), start + Duration::from_millis(101)),
            Err(error.into())
        );
    }

    #[test]
    fn worker_supervisor_reaps_a_successful_child() {
        let mut command = Command::new(worker_executable().unwrap());
        command
            .args([
                "--exact",
                "image::tests::worker_command_never_inherits_credentials_or_opt_in",
                "--list",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = run_worker(&mut command, b"", WORKER_TIMEOUT, None).unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("worker_command_never_inherits_credentials_or_opt_in")
        );
    }

    #[test]
    fn worker_slot_wait_is_bounded_and_cancellable() {
        let slot = Mutex::new(());
        let held = slot.lock().unwrap();
        assert!(
            acquire_worker_slot(&slot, None, Duration::from_millis(20))
                .unwrap_err()
                .contains("slot wait")
        );
        let interrupt = AtomicBool::new(true);
        assert!(
            acquire_worker_slot(&slot, Some(&interrupt), WORKER_TIMEOUT)
                .unwrap_err()
                .contains("interrupted")
        );
        drop(held);
        assert!(acquire_worker_slot(&slot, None, WORKER_TIMEOUT).is_ok());
    }

    #[test]
    fn resized_transparent_pixels_cannot_leak_hidden_rgb() {
        let encode = |hidden: [u8; 3]| {
            let mut rgba =
                ImageBuffer::from_pixel(1_600, 2, Rgba([hidden[0], hidden[1], hidden[2], 0]));
            for x in 0..800 {
                for y in 0..2 {
                    rgba.put_pixel(x, y, Rgba([255, 0, 0, 255]));
                }
            }
            let mut output = Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(rgba)
                .write_to(&mut output, ImageFormat::Png)
                .unwrap();
            decode_in_worker(&output.into_inner()).unwrap().2
        };
        assert_eq!(encode([0, 0, 255]), encode([0, 255, 0]));
    }

    #[test]
    fn bounded_resize_preserves_wide_image_orientation() {
        let mut image = RgbImage::from_pixel(2_000, 4, Rgb([255, 0, 0]));
        for x in 1_000..2_000 {
            for y in 0..4 {
                image.put_pixel(x, y, Rgb([0, 0, 255]));
            }
        }
        let resized = resize_bounded(DynamicImage::ImageRgb8(image)).into_rgb8();
        assert_eq!(resized.dimensions(), (1_568, 3));
        assert_eq!(resized.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(resized.get_pixel(1_567, 0).0, [0, 0, 255]);
    }

    #[cfg(unix)]
    #[test]
    fn worker_interrupt_reaps_a_stalled_child_with_blocked_stdin() {
        let interrupt = AtomicBool::new(false);
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "while :; do :; done"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let mut cleanup = RootCleanup::NotStarted;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(50));
                interrupt.store(true, Ordering::Relaxed);
            });
            assert!(
                run_worker_tracked(
                    &mut command,
                    &vec![0; SOURCE_BYTE_CAP],
                    WORKER_TIMEOUT,
                    Some(&interrupt),
                    &mut cleanup,
                )
                .unwrap_err()
                .contains("interrupted")
            );
        });
        assert!(matches!(
            cleanup,
            RootCleanup::Reaped | RootCleanup::TreeEmptied
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn worker_supervisor_reaps_stalls_crashes_and_output_floods() {
        for (script, expected) in [
            ("while :; do :; done", "wall-clock limit"),
            ("(while :; do :; done) & exit 1", "image worker failed"),
            ("kill -KILL $$", "image worker failed"),
            (
                "while :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done",
                "",
            ),
        ] {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", script])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let started = Instant::now();
            let mut cleanup = RootCleanup::NotStarted;
            let result = run_worker_tracked(
                &mut command,
                b"input",
                Duration::from_millis(100),
                None,
                &mut cleanup,
            );
            #[cfg(target_os = "linux")]
            let delegated = crate::cgroup::delegated_root().is_some();
            #[cfg(not(target_os = "linux"))]
            let delegated = false;
            let expected_cleanup = if delegated {
                RootCleanup::TreeEmptied
            } else {
                RootCleanup::Reaped
            };
            assert_eq!(cleanup, expected_cleanup, "{script}");
            if expected.is_empty() {
                assert!(result.and_then(worker_output).is_err());
            } else {
                assert!(result.unwrap_err().contains(expected));
            }
            assert!(started.elapsed() < WORKER_TIMEOUT);
        }
    }

    #[test]
    fn deterministic_malformed_corpus_does_not_panic() {
        let mut seeds = Vec::new();
        for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
            let mut cursor = Cursor::new(Vec::new());
            DynamicImage::ImageRgb8(RgbImage::from_pixel(3, 2, Rgb([1, 2, 3])))
                .write_to(&mut cursor, format)
                .unwrap();
            seeds.push(cursor.into_inner());
        }
        for seed in seeds {
            for length in 0..seed.len() {
                let _ = decode_in_worker(&seed[..length]);
            }
            for index in 0..seed.len() {
                let mut mutated = seed.clone();
                mutated[index] ^= 0xff;
                let _ = decode_in_worker(&mutated);
            }
        }
    }
}
