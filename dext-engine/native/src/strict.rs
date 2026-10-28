//! Workspace command runner. Every run gets a private copy of the workspace whose
//! resulting tree is sealed back; commands are isolated by bubblewrap namespaces,
//! or, for a runner built with `StrictExecutor::host`, run directly on the host with
//! no isolation (they can read and change anything the user can and use the network).
//! No provider credentials or ambient checkout are passed either way.
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[cfg(test)]
use crate::limits::windows_name;
pub use crate::limits::{
    JOB_OUTPUT, MAX_FILE, MAX_FILES, MAX_JOB_SECS, MAX_LINK, MAX_NAME, MAX_OUTPUT, MAX_PATH,
    MAX_RUN_SECS, MAX_TREE, valid_path,
};

/// Size of an isolated run's private /tmp (a memory-backed tmpfs).
pub const SANDBOX_TMP: u64 = 1 << 30;
/// Brokered scripts travel as one argv element; Linux caps a single argument at
/// 128 KiB (MAX_ARG_STRLEN), so this leaves headroom for a 64 KiB script.
pub const MAX_BROKERED_ARG: usize = 80 * 1024;
/// One brokered request line; batches of nested calls carry prompts and inline images.
pub const MAX_BROKER_REQUEST: usize = 64 * 1024 * 1024;
/// One broker reply line (a journaled child result is at most 1 MiB).
pub const MAX_BROKER_REPLY: usize = 2 * 1024 * 1024;
/// Concurrent sandboxes per user (effective uid), across all owner processes. Each run
/// holds one flocked slot file, so a killed owner frees its slot with its descriptors.
pub const MAX_CONCURRENT: usize = 4;
/// Free space that must remain on host /tmp beyond a run's materialized input. A run
/// may grow its candidate to twice the tree limit, but never into this headroom.
pub const SCRATCH_HEADROOM: u64 = 256 * 1024 * 1024;

/// Where private candidates and slot locks live: /tmp on Unix (shared, so entries are
/// owner-checked), the per-user temporary directory on Windows.
fn scratch_root() -> PathBuf {
    if cfg!(unix) {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    }
}

struct Slot(#[allow(dead_code)] std::fs::File);
impl Slot {
    fn acquire(deadline: Instant, interrupt: &AtomicBool) -> std::io::Result<Self> {
        let directory = slot_directory()?;
        loop {
            for index in 0..MAX_CONCURRENT {
                let mut options = std::fs::OpenOptions::new();
                options.read(true).write(true).create(true).truncate(false);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
                }
                let slot = options.open(directory.join(format!("slot-{index}")))?;
                match slot.try_lock() {
                    Ok(()) => return Ok(Self(slot)),
                    Err(std::fs::TryLockError::WouldBlock) => {}
                    Err(std::fs::TryLockError::Error(error)) => return Err(error),
                }
            }
            if interrupt.load(Ordering::SeqCst) {
                return Err(invalid("strict invocation interrupted"));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(limit(
                    std::io::ErrorKind::TimedOut,
                    "strict invocation deadline exceeded",
                ));
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(25)));
        }
    }
}

/// Per-user slot directory. On Unix it sits in shared /tmp, so a directory
/// pre-created by another user (or a symlink) could share or starve slots; it is
/// refused rather than trusted.
#[cfg(unix)]
fn slot_directory() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let uid = unsafe { libc::geteuid() };
    let directory = scratch_root().join(format!("spindle-strict-slots-{uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(&directory)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.permissions().mode() & 0o077 != 0 {
        return Err(invalid("untrusted strict slot directory"));
    }
    Ok(directory)
}

#[cfg(not(unix))]
fn slot_directory() -> std::io::Result<PathBuf> {
    let directory = scratch_root().join("spindle-strict-slots");
    std::fs::create_dir_all(&directory)?;
    Ok(directory)
}

/// Refuses a run unless the scratch volume holds its input plus headroom. A
/// point-in-time check: other writers (concurrent runs too) can still consume space.
fn scratch_fits(available: u64, input: u64) -> bool {
    available >= input.saturating_add(SCRATCH_HEADROOM)
}

/// How large a run's candidate may grow (input included): twice the tree limit, but
/// never into the scratch headroom that was free when the run started.
fn candidate_budget(available: u64, limits: &TreeLimits) -> u64 {
    limits
        .tree
        .saturating_mul(2)
        .min(available.saturating_sub(SCRATCH_HEADROOM))
}

#[cfg(unix)]
fn scratch_available() -> std::io::Result<u64> {
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c"/tmp".as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    #[allow(clippy::unnecessary_cast)]
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

#[cfg(windows)]
fn scratch_available() -> std::io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    let root: Vec<u16> = scratch_root()
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut available = 0u64;
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            root.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(available)
}

#[cfg(not(any(unix, windows)))]
fn scratch_available() -> std::io::Result<u64> {
    Ok(u64::MAX)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileValue {
    pub bytes: Vec<u8>,
    pub executable: bool,
}

/// One entry of a run's input tree. File bytes are written by `RunHost::load`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Input {
    File {
        size: u64,
        executable: bool,
    },
    /// A symbolic link, created as is and never followed by the host.
    Link(String),
    /// A directory with no entries.
    Directory,
}

/// One entry of a sealed tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SealedEntry {
    /// A regular file whose inode, size, times and mode are still exactly those of
    /// the materialized input, so its bytes are the input's and were not read again.
    Unchanged {
        executable: bool,
    },
    /// A new or changed regular file; `SealedOutput::read` returns its bytes.
    File {
        size: u64,
        executable: bool,
    },
    Link(String),
    /// A directory with no entries.
    Directory,
}

/// Supplies a tree run's input bytes and answers its brokered requests.
pub trait RunHost {
    /// Writes the bytes of the input file `path` (declared in the tree).
    fn load(&mut self, path: &str, out: &mut dyn Write) -> std::io::Result<()>;
    /// Answers one brokered request; only brokered runs call it.
    fn request(&mut self, _request: &[u8], _remaining: Duration, _cancel: &AtomicBool) -> Vec<u8> {
        b"{\"ok\":false,\"error\":\"no external code policy granted\"}".to_vec()
    }
}

/// Bounds on a run's workspace tree, never above the MAX_* constants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TreeLimits {
    /// Files, links and empty directories.
    pub entries: usize,
    pub file: u64,
    pub tree: u64,
}
impl Default for TreeLimits {
    fn default() -> Self {
        Self {
            entries: MAX_FILES,
            file: MAX_FILE as u64,
            tree: MAX_TREE as u64,
        }
    }
}

/// Why a run ended without a sealed tree, with the output it produced: its candidate
/// was discarded (nothing it changed is kept), so hosts can record this as a failed
/// result over the unchanged base. Carried by the returned `io::Error`; see
/// `run_failure`. Runs stopped by their own deadline or size bounds carry it too
/// (with error kind `TimedOut` or `FileTooLarge`).
pub struct RunFailure {
    pub reason: String,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
impl std::fmt::Debug for RunFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunFailure")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for RunFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}
impl std::error::Error for RunFailure {}

/// The contained failure an executor error carries, if any.
pub fn run_failure(error: &std::io::Error) -> Option<&RunFailure> {
    error.get_ref()?.downcast_ref()
}

fn failure(
    kind: std::io::ErrorKind,
    reason: String,
    stdout: &[u8],
    stderr: &[u8],
) -> std::io::Error {
    let keep = |bytes: &[u8]| bytes[..bytes.len().min(MAX_OUTPUT)].to_vec();
    std::io::Error::new(
        kind,
        RunFailure {
            reason,
            stdout: keep(stdout),
            stderr: keep(stderr),
        },
    )
}

/// Identity of a file as sealed: an unchanged stamp means unchanged bytes. The run
/// cannot set a change time (Unix ctime, Windows ChangeTime) through the tools it
/// uses, and a replaced file has a new identity (device and inode, or volume and file
/// id), so restoring a modification time does not hide a write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Stamp {
    device: u64,
    inode: u128,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    mode: u32,
}
impl Stamp {
    /// The stamp of an open file.
    fn of_file(file: &std::fs::File) -> std::io::Result<Self> {
        let metadata = file.metadata()?;
        #[cfg(windows)]
        {
            Self::identified(&metadata, file)
        }
        #[cfg(not(windows))]
        {
            Ok(Self::of(&metadata))
        }
    }

    /// The stamp of the entry at `path`, never following a link; `metadata` is its
    /// `symlink_metadata`.
    fn of_path(path: &Path, metadata: &std::fs::Metadata) -> std::io::Result<Self> {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            let file = std::fs::OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
                .open(path)?;
            Self::identified(metadata, &file)
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            Ok(Self::of(metadata))
        }
    }

    #[cfg(not(windows))]
    fn of(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                device: metadata.dev(),
                inode: u128::from(metadata.ino()),
                size: metadata.len(),
                modified: (metadata.mtime(), metadata.mtime_nsec()),
                changed: (metadata.ctime(), metadata.ctime_nsec()),
                mode: metadata.mode(),
            }
        }
        #[cfg(not(unix))]
        {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or((0, 0), |time| {
                    (time.as_secs() as i64, i64::from(time.subsec_nanos()))
                });
            Self {
                device: 0,
                inode: 0,
                size: metadata.len(),
                modified,
                changed: modified,
                mode: 0,
            }
        }
    }

    /// Volume and file id, change and write times and attributes from the handle.
    #[cfg(windows)]
    fn identified(metadata: &std::fs::Metadata, file: &std::fs::File) -> std::io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_BASIC_INFO, FILE_ID_INFO, FileBasicInfo, FileIdInfo, GetFileInformationByHandleEx,
        };
        let handle = file.as_raw_handle();
        let mut basic = FILE_BASIC_INFO::default();
        let mut id = FILE_ID_INFO::default();
        let read = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileBasicInfo,
                std::ptr::from_mut(&mut basic).cast(),
                std::mem::size_of_val(&basic) as u32,
            ) != 0
                && GetFileInformationByHandleEx(
                    handle,
                    FileIdInfo,
                    std::ptr::from_mut(&mut id).cast(),
                    std::mem::size_of_val(&id) as u32,
                ) != 0
        };
        if !read {
            return Err(std::io::Error::last_os_error());
        }
        // FILETIME ticks are 100 ns.
        let time = |ticks: i64| {
            (
                ticks.div_euclid(10_000_000),
                ticks.rem_euclid(10_000_000) * 100,
            )
        };
        Ok(Self {
            device: id.VolumeSerialNumber,
            inode: u128::from_le_bytes(id.FileId.Identifier),
            size: metadata.len(),
            modified: time(basic.LastWriteTime),
            changed: time(basic.ChangeTime),
            mode: basic.FileAttributes,
        })
    }
}

pub struct SealedOutput {
    files: BTreeMap<String, FileValue>,
    entries: BTreeMap<String, SealedEntry>,
    stamps: BTreeMap<String, Stamp>,
    /// The locked, no longer writable candidate, kept so changed files are read one at
    /// a time; removed when this output drops.
    candidate: Option<Scratch>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    success: bool,
    helper: [u8; 32],
}
impl std::fmt::Debug for SealedOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealedOutput")
            .field("entries", &self.entries.len())
            .field("success", &self.success)
            .finish_non_exhaustive()
    }
}
impl SealedOutput {
    /// Every regular file with its bytes, for runs given a `FileValue` map
    /// (`execute`, `execute_with`, `execute_brokered`). Tree runs leave it empty and use
    /// `entries` and `read`.
    pub fn files(&self) -> &BTreeMap<String, FileValue> {
        &self.files
    }
    /// The whole sealed tree: files, symbolic links and empty directories.
    pub fn entries(&self) -> &BTreeMap<String, SealedEntry> {
        &self.entries
    }
    /// Bytes of a `SealedEntry::File`, checked to be the exact file that was sealed.
    pub fn read(&self, path: &str) -> std::io::Result<Vec<u8>> {
        let (Some(candidate), Some(stamp)) = (&self.candidate, self.stamps.get(path)) else {
            return Err(invalid("no sealed file at this path"));
        };
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(candidate.workspace().join(path))?;
        if !file.metadata()?.is_file() || Stamp::of_file(&file)? != *stamp {
            return Err(invalid("a sealed file changed after its run ended"));
        }
        let mut bytes = Vec::with_capacity(stamp.size as usize);
        file.take(stamp.size + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 != stamp.size {
            return Err(invalid("a sealed file changed after its run ended"));
        }
        Ok(bytes)
    }
    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }
    pub fn success(&self) -> bool {
        self.success
    }
    pub fn helper_digest(&self) -> &[u8; 32] {
        &self.helper
    }
}

/// Per-run options. Without `network` an isolated run gets a private network namespace
/// with only its own loopback: AF_UNIX, AF_INET/AF_INET6 and route-netlink sockets work
/// (local servers, asyncio, child-process IPC) but nothing outside is reachable.
/// `network` shares the host network namespace (internet and host loopback services)
/// and allows AF_INET/AF_INET6 sockets only: an opted-in run can have external
/// effects that no rewind or discard undoes. `observe` receives raw chunks as they
/// arrive with their stream (1 stdout, 2 stderr); it is a preview, not the sealed
/// result, and runs on reader threads, so it must not block long.
#[derive(Clone, Copy, Default)]
pub struct RunOptions<'a> {
    pub network: bool,
    pub observe: Option<OutputObserver<'a>>,
    pub toolchain: Option<Toolchain<'a>>,
    /// Kept for compatibility: anonymous socketpairs (which Node, Rust std and other
    /// runtimes use to spawn child processes) are now allowed in every run. A pair has
    /// no address, so it cannot reach host sockets.
    pub local_ipc: bool,
}

/// Host Rust toolchain exposed to a run: rustup toolchains and cargo binaries
/// read-only, the host crate registry as a copy-on-write overlay, and `cache`, a
/// host-owned persistent directory for build output, downloads and git checkouts.
/// The cache is outside the revisioned workspace: it survives rewinds, is shared by
/// later runs of the same host, has no quota of its own (only the run's file-size
/// limit and the host disk) and is never part of a sealed result. In isolated runs
/// CARGO_HOME itself is a fresh tmpfs per run (only `registry` and `git` persist), so
/// cargo configuration or credentials a run writes there never reach a later run.
/// Builds are offline unless the run also has `network`. `sources` are host
/// directories mounted read-only at `/<name>` beside `/workspace`, so path
/// dependencies such as `../dext-engine` resolve without copying them into the
/// workspace.
#[derive(Clone, Copy)]
pub struct Toolchain<'a> {
    pub rustup_home: &'a Path,
    pub cargo_home: &'a Path,
    pub cache: &'a Path,
    pub sources: &'a [PathBuf],
}

const RESERVED_ROOTS: [&str; 12] = [
    "bin",
    "cache",
    "dev",
    "etc",
    "lib",
    "lib64",
    "opt",
    "proc",
    "sys",
    "tmp",
    "usr",
    "workspace",
];

/// Mount point for a read-only source directory: `/` plus its final component.
pub fn source_mount(source: &Path) -> Option<String> {
    let name = source.file_name()?.to_str()?;
    (valid_path(name) && !RESERVED_ROOTS.contains(&name)).then(|| format!("/{name}"))
}

impl Toolchain<'_> {
    fn validate(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        for path in [self.rustup_home, self.cargo_home, self.cache] {
            if !path.is_absolute() || !std::fs::metadata(path)?.is_dir() {
                return Err(invalid(
                    "toolchain paths must be existing absolute directories",
                ));
            }
        }
        let cache = std::fs::symlink_metadata(self.cache)?;
        #[cfg(unix)]
        let private = cache.uid() == unsafe { libc::geteuid() } && cache.mode() & 0o077 == 0;
        #[cfg(not(unix))]
        let private = true;
        if !cache.is_dir() || !private {
            return Err(invalid(
                "toolchain cache must be a private directory owned by this user",
            ));
        }
        let mut mounts = std::collections::BTreeSet::new();
        for source in self.sources {
            let mount = source_mount(source)
                .ok_or_else(|| invalid("unsupported toolchain source directory name"))?;
            if !source.is_absolute()
                || !std::fs::metadata(source)?.is_dir()
                || !mounts.insert(mount)
            {
                return Err(invalid(
                    "toolchain sources must be distinct absolute directories",
                ));
            }
        }
        for directory in [
            "cargo/git",
            "target",
            "tmp",
            "overlay/upper",
            "overlay/work",
        ] {
            std::fs::create_dir_all(self.cache.join(directory))?;
        }
        Ok(())
    }
}
pub type OutputObserver<'a> = &'a (dyn Fn(u8, &[u8]) + Sync);
pub type BrokerCallback<'a> = dyn FnMut(&[u8], Duration, &AtomicBool) -> Vec<u8> + 'a;

pub struct StrictExecutor {
    /// The sealed bubblewrap helper; absent for host runners.
    #[cfg(target_os = "linux")]
    helper: Option<std::fs::File>,
    digest: [u8; 32],
    limits: TreeLimits,
}

/// Identity recorded for runs of the unisolated host runner.
pub fn host_digest() -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(b"dext-host-runner-v1 (no isolation)").into()
}

/// Attempt implementation label for a runner identity: isolated runs and host runs
/// are labelled differently so receipts always say how a command was run.
pub fn implementation_label(digest: &[u8; 32]) -> String {
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    if *digest == host_digest() {
        format!("dext-host-bash-v1-{hex}")
    } else {
        format!("dext-strict-bash-v1-{hex}")
    }
}
impl std::fmt::Debug for StrictExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StrictExecutor").finish_non_exhaustive()
    }
}

fn invalid(message: &str) -> std::io::Error {
    std::io::Error::other(message)
}
/// A run stopped by its own bounds. These kinds are returned only after the sandbox
/// is killed and its candidate discarded, so hosts may record them as contained.
fn limit(kind: std::io::ErrorKind, message: &str) -> std::io::Error {
    std::io::Error::new(kind, message)
}
/// A byte count for messages: `812 bytes`, `64 MiB`, `2.0 GiB`.
fn size_text(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} bytes"),
        1024..1048576 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        1048576..1073741824 => format!("{:.1} MiB", bytes as f64 / 1048576.0),
        _ => format!("{:.1} GiB", bytes as f64 / 1073741824.0),
    }
}

/// A path for messages, cut to a readable length.
fn path_text(path: &str) -> String {
    if path.chars().count() <= 160 {
        return path.to_string();
    }
    format!("{}...", path.chars().take(160).collect::<String>())
}

/// Checks a run's input tree and returns its byte total: valid paths and link targets,
/// no entry inside a file, link or (necessarily empty) directory entry, within limits.
fn check_input(tree: &BTreeMap<String, Input>, limits: &TreeLimits) -> std::io::Result<u64> {
    let mut total = 0u64;
    if tree.len() > limits.entries {
        return Err(invalid("invalid strict input tree"));
    }
    // Windows filesystems ignore case, so two such paths would be one file.
    let mut folded = std::collections::BTreeSet::new();
    if cfg!(windows) && !tree.keys().all(|path| folded.insert(path.to_lowercase())) {
        return Err(invalid(
            "the workspace has paths that differ only in case, which this host's filesystem cannot keep apart",
        ));
    }
    for (path, input) in tree {
        let parent_taken = path
            .match_indices('/')
            .any(|(offset, _)| tree.contains_key(&path[..offset]));
        let valid = valid_path(path)
            && !parent_taken
            && match input {
                Input::File { size, .. } => {
                    total = total.saturating_add(*size);
                    *size <= limits.file
                }
                Input::Link(target) => {
                    !target.is_empty() && target.len() <= MAX_LINK && !target.contains('\0')
                }
                Input::Directory => true,
            };
        if !valid || total > limits.tree {
            return Err(invalid("invalid strict input tree"));
        }
    }
    Ok(total)
}

impl StrictExecutor {
    pub fn helper_digest(&self) -> &[u8; 32] {
        &self.digest
    }

    /// The attempt label for runs of this runner.
    pub fn implementation(&self) -> String {
        implementation_label(&self.digest)
    }

    /// Whether commands are namespace-isolated (false for the host runner).
    pub fn isolated(&self) -> bool {
        self.digest != host_digest()
    }

    /// A runner without isolation, for hosts where namespaces are unavailable (for
    /// example unprivileged containers) or where the embedder provides isolation.
    /// Commands still run over a private copy that is sealed back, with the same
    /// limits and deadlines, and their process group is killed when a run ends; but
    /// they see the real filesystem and network, and a process that starts its own
    /// session can outlive the run.
    pub fn host() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            helper: None,
            digest: host_digest(),
            limits: TreeLimits::default(),
        }
    }

    /// The same runner with narrower tree bounds (never above the MAX_* constants).
    pub fn with_tree_limits(mut self, limits: TreeLimits) -> Self {
        let most = TreeLimits::default();
        self.limits = TreeLimits {
            entries: limits.entries.min(most.entries),
            file: limits.file.min(most.file),
            tree: limits.tree.min(most.tree),
        };
        self
    }

    /// Runs a trivial command to prove this runner works here (for the isolated
    /// runner: that the kernel lets bubblewrap create its namespaces).
    pub fn probe(&self) -> std::io::Result<()> {
        let argv: Vec<String> = if cfg!(windows) {
            vec![system_shell(), "/d".into(), "/c".into(), "exit 0".into()]
        } else {
            vec!["/bin/sh".into(), "-c".into(), "exit 0".into()]
        };
        let output = self.execute(
            &BTreeMap::new(),
            &argv,
            Duration::from_secs(20),
            &AtomicBool::new(false),
        )?;
        if output.success() {
            Ok(())
        } else {
            Err(invalid(&String::from_utf8_lossy(output.stderr())))
        }
    }

    /// Host approval must identify a reviewed bubblewrap 0.11+ executable. Hash
    /// pinning proves bytes, not safety of arbitrary approved code or host runtime.
    pub fn open(helper: &Path, approved: [u8; 32]) -> std::io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            use sha2::{Digest, Sha256};
            use std::os::fd::{AsRawFd, FromRawFd};
            use std::os::unix::fs::OpenOptionsExt;
            let mut input = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(helper)?;
            let metadata = input.metadata()?;
            if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 8 * 1024 * 1024 {
                return Err(invalid("invalid isolation helper"));
            }
            let fd = unsafe {
                libc::memfd_create(
                    c"spindle-isolation".as_ptr(),
                    libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut helper = unsafe { std::fs::File::from_raw_fd(fd) };
            let mut bytes = Vec::new();
            Read::by_ref(&mut input)
                .take(8 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 8 * 1024 * 1024 || <[u8; 32]>::from(Sha256::digest(&bytes)) != approved
            {
                return Err(invalid("isolation helper identity mismatch"));
            }
            helper.write_all(&bytes)?;
            if unsafe { libc::fchmod(fd, 0o500) } != 0
                || unsafe {
                    libc::fcntl(
                        helper.as_raw_fd(),
                        libc::F_ADD_SEALS,
                        libc::F_SEAL_WRITE
                            | libc::F_SEAL_GROW
                            | libc::F_SEAL_SHRINK
                            | libc::F_SEAL_SEAL,
                    )
                } < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self {
                helper: Some(helper),
                digest: approved,
                limits: TreeLimits::default(),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (helper, approved);
            Err(invalid("namespace isolation requires Linux"))
        }
    }

    /// A fresh private materialization is owned until every namespaced process is
    /// gone and its tree is sealed. No in-place host import is implied.
    pub fn execute(
        &self,
        files: &BTreeMap<String, FileValue>,
        argv: &[String],
        timeout: Duration,
        interrupt: &AtomicBool,
    ) -> std::io::Result<SealedOutput> {
        self.execute_with(files, argv, timeout, interrupt, RunOptions::default())
    }

    pub fn execute_with(
        &self,
        files: &BTreeMap<String, FileValue>,
        argv: &[String],
        timeout: Duration,
        interrupt: &AtomicBool,
        options: RunOptions<'_>,
    ) -> std::io::Result<SealedOutput> {
        self.run_files(files, argv, timeout, interrupt, options, &[], None)
    }

    /// A host-authorized, bounded request/reply channel (see `execute_tree`). Requests
    /// are untrusted script input, never authorization. Sources are mounted read-only.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_brokered(
        &self,
        files: &BTreeMap<String, FileValue>,
        argv: &[String],
        timeout: Duration,
        interrupt: &AtomicBool,
        options: RunOptions<'_>,
        sources: &[PathBuf],
        broker: &mut BrokerCallback<'_>,
    ) -> std::io::Result<SealedOutput> {
        self.run_files(
            files,
            argv,
            timeout,
            interrupt,
            options,
            sources,
            Some(broker),
        )
    }

    /// Runs over a tree whose file bytes `host` writes one at a time, so the input is
    /// never held in memory. The sealed output lists every entry; files whose identity
    /// is unchanged since materialization are reported as such without being read,
    /// changed ones are read on demand with `SealedOutput::read`.
    ///
    /// A `brokered` run gets a request/reply channel to `host.request`: on Unix two
    /// pipes whose descriptors are named by `SPINDLE_RPC_FDS=<reply-read>,<request-write>`
    /// (one JSON request per line, answered by one reply line), so ordinary output can
    /// never be mistaken for a request; on Windows request lines start with 0x1E on
    /// stdout and replies arrive on stdin.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_tree(
        &self,
        tree: &BTreeMap<String, Input>,
        argv: &[String],
        timeout: Duration,
        interrupt: &AtomicBool,
        options: RunOptions<'_>,
        sources: &[PathBuf],
        brokered: bool,
        host: &mut dyn RunHost,
    ) -> std::io::Result<SealedOutput> {
        self.run(
            tree, argv, timeout, interrupt, options, sources, brokered, host,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run_files(
        &self,
        files: &BTreeMap<String, FileValue>,
        argv: &[String],
        timeout: Duration,
        interrupt: &AtomicBool,
        options: RunOptions<'_>,
        sources: &[PathBuf],
        broker: Option<&mut BrokerCallback<'_>>,
    ) -> std::io::Result<SealedOutput> {
        struct Files<'a, 'b> {
            files: &'a BTreeMap<String, FileValue>,
            broker: Option<&'a mut BrokerCallback<'b>>,
        }
        impl RunHost for Files<'_, '_> {
            fn load(&mut self, path: &str, out: &mut dyn Write) -> std::io::Result<()> {
                let file = self
                    .files
                    .get(path)
                    .ok_or_else(|| invalid("no input file"))?;
                out.write_all(&file.bytes)
            }
            fn request(
                &mut self,
                request: &[u8],
                remaining: Duration,
                cancel: &AtomicBool,
            ) -> Vec<u8> {
                match self.broker.as_mut() {
                    Some(broker) => broker(request, remaining, cancel),
                    None => b"{\"ok\":false,\"error\":\"no broker\"}".to_vec(),
                }
            }
        }
        let tree = files
            .iter()
            .map(|(path, file)| {
                let input = Input::File {
                    size: file.bytes.len() as u64,
                    executable: file.executable,
                };
                (path.clone(), input)
            })
            .collect();
        let brokered = broker.is_some();
        let mut host = Files { files, broker };
        let mut sealed = self.run(
            &tree, argv, timeout, interrupt, options, sources, brokered, &mut host,
        )?;
        let mut complete = BTreeMap::new();
        for (path, entry) in &sealed.entries {
            match entry {
                SealedEntry::Unchanged { .. } => {
                    complete.insert(path.clone(), files[path].clone());
                }
                SealedEntry::File { executable, .. } => {
                    let bytes = sealed.read(path)?;
                    complete.insert(
                        path.clone(),
                        FileValue {
                            bytes,
                            executable: *executable,
                        },
                    );
                }
                SealedEntry::Link(_) | SealedEntry::Directory => {}
            }
        }
        sealed.files = complete;
        Ok(sealed)
    }

    /// The confined command for a run or job over `workspace`: bubblewrap with the
    /// seccomp filter (isolated) or a direct host command, piped stdout/stderr, its own
    /// cgroup where one is delegated, rlimits, and only the descriptors it must inherit.
    /// `rpc` are the child's ends of a brokered run's pipes (request write, reply read).
    #[allow(clippy::too_many_arguments)]
    fn launch(
        &self,
        scratch: &Scratch,
        workspace: &Path,
        argv: &[String],
        options: &RunOptions<'_>,
        sources: &[PathBuf],
        rpc: Option<(std::fs::File, std::fs::File)>,
        stdin_piped: bool,
        timeout: Duration,
    ) -> std::io::Result<Launch> {
        use std::process::Stdio;
        let isolated = self.sandboxed();
        #[cfg(unix)]
        let rpc_variable = rpc.as_ref().map(|(request, reply)| {
            use std::os::fd::AsRawFd;
            format!("{},{}", reply.as_raw_fd(), request.as_raw_fd())
        });
        #[cfg(not(unix))]
        let rpc_variable: Option<String> = {
            let _ = rpc;
            None
        };
        #[cfg(target_os = "linux")]
        let tasks = run_tasks(options.toolchain.is_some());
        #[cfg(target_os = "linux")]
        let cgroup = crate::cgroup::run_cgroup(tasks);
        #[cfg(target_os = "linux")]
        let pids_capped = cgroup.as_ref().is_some_and(|(_, capped)| *capped);
        #[cfg(all(unix, not(target_os = "linux")))]
        let pids_capped = false;
        // Descriptors the child must inherit; kept open until spawn.
        #[cfg(unix)]
        let mut inherited: Vec<std::fs::File> = vec![];
        #[cfg(target_os = "linux")]
        let mut reports = None;
        let mut command = match () {
            #[cfg(target_os = "linux")]
            () if isolated => {
                use std::os::fd::AsRawFd;
                let helper = self.helper.as_ref().ok_or_else(|| invalid("no helper"))?;
                let directory = std::fs::File::open(workspace)?;
                let filter = network_filter(options.network)?;
                let (status_read, status_write) = pipe()?;
                let command = isolated_command(
                    helper,
                    &Mounts {
                        directory: directory.as_raw_fd(),
                        filter: filter.as_raw_fd(),
                        status: status_write.as_raw_fd(),
                        rpc: rpc_variable.as_deref(),
                    },
                    argv,
                    options,
                    sources,
                );
                inherited.extend([directory, filter, status_write]);
                reports = Some(status_read);
                command
            }
            () => {
                let mut command = host_command(scratch, workspace, argv, options)?;
                if let Some(variable) = &rpc_variable {
                    command.env("SPINDLE_RPC_FDS", variable);
                }
                command
            }
        };
        #[cfg(unix)]
        if let Some((request_write, reply_read)) = rpc {
            inherited.extend([request_write, reply_read]);
        }
        command
            .stdin(if stdin_piped {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(target_os = "linux")]
        if let Some((group, _)) = &cgroup {
            use std::os::unix::process::CommandExt;
            // Registered first, so every process of the run starts inside the cgroup.
            unsafe { command.pre_exec(group.enter_hook()) };
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let inherit: Vec<libc::c_int> = inherited.iter().map(|file| file.as_raw_fd()).collect();
            confine(
                &mut command,
                !isolated,
                inherit,
                ProcessLimits::for_run(options.toolchain.is_some(), timeout, pids_capped),
            );
        }
        #[cfg(windows)]
        let caps = {
            use std::os::windows::process::CommandExt;
            use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
            // Suspended until the job object holds it, so nothing escapes the job.
            command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
            job_caps(options.toolchain.is_some(), timeout)
        };
        #[cfg(not(windows))]
        let caps = crate::process_tree::TreeCaps::default();
        #[cfg(not(unix))]
        let _ = isolated;
        #[cfg(not(any(unix, windows)))]
        let _ = timeout;
        #[cfg(not(target_os = "linux"))]
        let _ = sources;
        Ok(Launch {
            command,
            caps,
            #[cfg(unix)]
            inherited,
            #[cfg(target_os = "linux")]
            reports,
            #[cfg(target_os = "linux")]
            cgroup: cgroup.map(|(group, _)| group),
        })
    }

    /// Isolated runners exist only on Linux; elsewhere every runner is a host runner.
    fn sandboxed(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.helper.is_some()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        tree: &BTreeMap<String, Input>,
        argv: &[String],
        timeout: Duration,
        interrupt: &AtomicBool,
        options: RunOptions<'_>,
        sources: &[PathBuf],
        brokered: bool,
        host: &mut dyn RunHost,
    ) -> std::io::Result<SealedOutput> {
        for source in sources {
            if !source.is_absolute() || !source.is_dir() || source_mount(source).is_none() {
                return Err(invalid("invalid broker source mount"));
            }
        }
        if argv.is_empty()
            || argv.len() > 128
            || argv
                .iter()
                .any(|arg| arg.len() > MAX_BROKERED_ARG || arg.contains('\0'))
            || !Path::new(&argv[0]).is_absolute()
            || timeout.is_zero()
            || timeout > Duration::from_secs(MAX_RUN_SECS)
        {
            return Err(invalid("invalid strict invocation bounds"));
        }
        if interrupt.load(Ordering::SeqCst) {
            return Err(invalid("strict invocation interrupted"));
        }
        let input_bytes = check_input(tree, &self.limits)?;
        // Waiting counts against the invocation deadline, so queuing never extends it.
        let deadline = Instant::now() + timeout;
        let _slot = Slot::acquire(deadline, interrupt)?;
        let available = scratch_available()?;
        if !scratch_fits(available, input_bytes) {
            return Err(failure(
                std::io::ErrorKind::Other,
                format!(
                    "the host's scratch space is full: the run needs {} for its workspace plus {} free, and {} is free; nothing ran",
                    size_text(input_bytes),
                    size_text(SCRATCH_HEADROOM),
                    size_text(available)
                ),
                &[],
                &[],
            ));
        }
        let scratch = Scratch::create()?;
        let workspace = scratch.workspace();
        let baseline = materialize(&workspace, tree, host)?;
        let prepare = |error: std::io::Error| {
            failure(
                std::io::ErrorKind::Other,
                format!("the run could not be prepared: {error}; nothing ran"),
                &[],
                &[],
            )
        };
        if let Some(toolchain) = &options.toolchain {
            toolchain.validate().map_err(prepare)?;
        }
        let isolated = self.sandboxed();
        // The child's ends of the RPC pipes: it reads replies and writes requests.
        #[cfg(unix)]
        let (rpc_parent, rpc_child) = if brokered {
            let (request_read, request_write) = pipe()?;
            let (reply_read, reply_write) = pipe()?;
            (
                Some((request_read, reply_write)),
                Some((request_write, reply_read)),
            )
        } else {
            (None, None)
        };
        #[cfg(not(unix))]
        let rpc_child = None;
        let launch = self.launch(
            &scratch,
            &workspace,
            argv,
            &options,
            sources,
            rpc_child,
            brokered && cfg!(not(unix)),
            timeout,
        )?;
        let mut command = launch.command;
        #[allow(unused_mut)]
        let mut child = StrictChild::spawn(&mut command, !isolated, launch.caps)?;
        #[cfg(target_os = "linux")]
        {
            child.cgroup = launch.cgroup;
        }
        #[cfg(target_os = "linux")]
        let reports = launch.reports;
        #[cfg(unix)]
        drop(launch.inherited);
        drop(command);
        let stdout = child
            .child
            .stdout
            .take()
            .ok_or_else(|| invalid("missing strict stdout"))?;
        let stderr = child
            .child
            .stderr
            .take()
            .ok_or_else(|| invalid("missing strict stderr"))?;
        #[cfg(not(unix))]
        let stdin = child.child.stdin.take();
        let (requests, incoming) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
        let (replies, outgoing) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
        let stopped = AtomicBool::new(false);
        struct StopReaders<'a>(&'a AtomicBool);
        impl Drop for StopReaders<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let flood = AtomicBool::new(false);
        let mut growth = Growth::new(
            &workspace,
            candidate_budget(available, &self.limits),
            input_bytes,
            self.limits.entries.saturating_mul(4),
        );
        let (ended, stdout, stderr, out_of_memory) =
            std::thread::scope(|scope| -> std::io::Result<_> {
                // This guard drops before scope joins, so readers never retain the run on
                // early return or thread-spawn panic.
                let mut child = child;
                let _stop_readers = StopReaders(&stopped);
                let stopped_ref = &stopped;
                let flood_ref = &flood;
                let observe = options.observe;
                #[cfg(unix)]
                let output = scope.spawn(move || {
                    collect_output(1, observe, flood_ref, |sink| {
                        pump(stdout, stopped_ref, sink)
                    })
                });
                #[cfg(not(unix))]
                let output = scope.spawn(move || {
                    if !brokered {
                        return collect_output(1, observe, flood_ref, |sink| {
                            pump(stdout, stopped_ref, sink)
                        });
                    }
                    let stdin = stdin.ok_or_else(|| invalid("missing broker stdin"))?;
                    collect_output(1, observe, flood_ref, |sink| {
                        stdio_broker(
                            stdout,
                            stdin,
                            stopped_ref,
                            interrupt,
                            deadline,
                            requests,
                            outgoing,
                            flood_ref,
                            sink,
                        )
                    })
                });
                let errors = scope.spawn(move || {
                    collect_output(2, observe, flood_ref, |sink| {
                        pump(stderr, stopped_ref, sink)
                    })
                });
                #[cfg(unix)]
                let broker = rpc_parent.map(move |(request_read, reply_write)| {
                    scope.spawn(move || {
                        serve_rpc(
                            request_read,
                            reply_write,
                            stopped_ref,
                            interrupt,
                            deadline,
                            requests,
                            outgoing,
                            flood_ref,
                        )
                    })
                });
                #[cfg(not(unix))]
                let broker: Option<
                    std::thread::ScopedJoinHandle<'_, std::io::Result<()>>,
                > = None;
                let ended = (|| -> std::io::Result<std::process::ExitStatus> {
                    loop {
                        if interrupt.load(Ordering::SeqCst) {
                            return Err(invalid("strict invocation interrupted"));
                        }
                        if let Ok(request) = incoming.try_recv() {
                            let remaining = deadline.saturating_duration_since(Instant::now());
                            let cancel = AtomicBool::new(false);
                            let finished = AtomicBool::new(false);
                            let child_ref = &child;
                            let watch = &mut growth;
                            let reply = std::thread::scope(|nested| -> std::io::Result<Vec<u8>> {
                                let _finish = StopReaders(&finished);
                                let monitor = nested.spawn(|| {
                                    while !finished.load(Ordering::SeqCst) {
                                        if interrupt.load(Ordering::SeqCst)
                                            || flood.load(Ordering::SeqCst)
                                            || Instant::now() >= deadline
                                            || watch.exceeded()
                                        {
                                            cancel.store(true, Ordering::SeqCst);
                                            child_ref.kill_tree();
                                            return;
                                        }
                                        std::thread::sleep(Duration::from_millis(10));
                                    }
                                });
                                let reply = host.request(&request, remaining, &cancel);
                                finished.store(true, Ordering::SeqCst);
                                monitor
                                    .join()
                                    .map_err(|_| invalid("broker monitor failed"))?;
                                if cancel.load(Ordering::SeqCst) {
                                    return Err(limit(
                                        std::io::ErrorKind::TimedOut,
                                        "broker interrupted or sandbox bound exceeded",
                                    ));
                                }
                                Ok(reply)
                            })?;
                            if reply.len() > MAX_BROKER_REPLY {
                                return Err(invalid("broker reply too large"));
                            }
                            // A script that already exited no longer reads replies.
                            let _ = replies.send(reply);
                        }
                        // A finished tree is checked after descendants stop and owner
                        // access is restored; live scans cannot read chmod-000 output.
                        if let Some(status) = child.child.try_wait()? {
                            return Ok(status);
                        }
                        if growth.exceeded() {
                            return Err(limit(
                                std::io::ErrorKind::FileTooLarge,
                                "strict candidate disk limit exceeded",
                            ));
                        }
                        if flood.load(Ordering::SeqCst) {
                            return Err(limit(
                                std::io::ErrorKind::FileTooLarge,
                                "strict output limit exceeded",
                            ));
                        }
                        if Instant::now() >= deadline {
                            return Err(limit(
                                std::io::ErrorKind::TimedOut,
                                "strict invocation deadline exceeded",
                            ));
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                })();
                // End whatever remains: the process group of a host run, every member of the
                // run's cgroup (detached descendants too), or the namespace with its helper.
                child.kill_tree();
                if ended.is_err() {
                    child.stop();
                }
                // Readers stop at EOF; a descendant that escaped the run (an unisolated run
                // without a cgroup) may hold a pipe open forever, so they get a short grace.
                let drain = Instant::now() + Duration::from_millis(250);
                while !(output.is_finished()
                    && errors.is_finished()
                    && broker.as_ref().is_none_or(|broker| broker.is_finished()))
                    && Instant::now() < drain
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                stopped.store(true, Ordering::SeqCst);
                let stdout = output
                    .join()
                    .map_err(|_| invalid("strict stdout thread failed"))??;
                let stderr = errors
                    .join()
                    .map_err(|_| invalid("strict stderr thread failed"))??;
                // An oversized request already stopped the run; a reply the exited script
                // no longer reads changes nothing.
                if let Some(broker) = broker {
                    let _ = broker.join();
                }
                let succeeded = ended.as_ref().is_ok_and(|status| status.success());
                Ok((ended, stdout, stderr, child.out_of_memory(succeeded)))
            })?;
        // An OOM kill ends the whole run (bubblewrap's monitor too), so it is told as
        // what it was, not as a setup failure or an ordinary exit.
        if out_of_memory {
            return Err(failure(
                std::io::ErrorKind::OutOfMemory,
                format!("the run was stopped: it ran out of memory ({MEMORY_CAP})"),
                &stdout,
                &stderr,
            ));
        }
        let status = match ended {
            Ok(status) => status,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::FileTooLarge
                ) =>
            {
                return Err(failure(error.kind(), error.to_string(), &stdout, &stderr));
            }
            Err(error) => return Err(error),
        };
        if stdout.len() > MAX_OUTPUT || stderr.len() > MAX_OUTPUT {
            return Err(failure(
                std::io::ErrorKind::FileTooLarge,
                "strict output limit exceeded".into(),
                &stdout,
                &stderr,
            ));
        }
        // bubblewrap reports the command's exit code only once it actually started, so
        // its absence means the sandbox itself could not be set up.
        #[cfg(target_os = "linux")]
        if let Some(mut reports) = reports {
            let mut report = Vec::new();
            set_nonblocking(&reports)?;
            let _ = Read::by_ref(&mut reports)
                .take(64 * 1024)
                .read_to_end(&mut report);
            if !String::from_utf8_lossy(&report).contains("\"exit-code\"") {
                return Err(failure(
                    std::io::ErrorKind::Other,
                    format!(
                        "strict isolation setup refused: {}",
                        String::from_utf8_lossy(&stderr).trim()
                    ),
                    &stdout,
                    &stderr,
                ));
            }
        }
        let mut skipped = vec![];
        let sealed = seal(&workspace, tree, &baseline, &self.limits, &mut skipped);
        let mut stderr = stderr;
        if !skipped.is_empty() {
            let names: Vec<String> = skipped
                .iter()
                .take(8)
                .map(|path| format!("`{}`", path_text(path)))
                .collect();
            let more = skipped.len().saturating_sub(8);
            stderr.extend_from_slice(
                format!(
                    "\n[not kept: special files (sockets, pipes, devices) {}{}]\n",
                    names.join(", "),
                    if more > 0 {
                        format!(" and {more} more")
                    } else {
                        String::new()
                    }
                )
                .as_bytes(),
            );
        }
        let (entries, stamps) = sealed.map_err(|reason| {
            failure(
                std::io::ErrorKind::InvalidData,
                format!("the run's workspace changes could not be kept: {reason}"),
                &stdout,
                &stderr,
            )
        })?;
        // Candidate paths are private and no longer writable by any live namespace.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&workspace, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(SealedOutput {
            files: BTreeMap::new(),
            entries,
            stamps,
            candidate: Some(scratch),
            stdout,
            stderr,
            success: status.success(),
            helper: self.digest,
        })
    }
}

/// Background jobs one owner process may run at once.
pub const MAX_JOBS: usize = 16;

static RUNNING_JOBS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How a background job stands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JobState {
    Running,
    /// The command exited with this code (`None`: it was ended by a signal).
    Exited(Option<i32>),
    /// The host ended it; the reason says why (stopped, lifetime, disk, setup).
    Ended(String),
}

/// A window of a job's output.
#[derive(Clone, Debug)]
pub struct JobOutput {
    pub bytes: Vec<u8>,
    /// Stream offsets of `bytes`. `start` is past the requested offset when the bytes
    /// there were already dropped (only the newest JOB_OUTPUT are kept).
    pub start: u64,
    pub end: u64,
    /// Bytes the job has printed so far.
    pub total: u64,
    pub state: JobState,
    pub elapsed: Duration,
}

struct JobShared {
    inner: std::sync::Mutex<JobInner>,
    changed: std::sync::Condvar,
    stop: AtomicBool,
    started: Instant,
}

struct JobInner {
    output: std::collections::VecDeque<u8>,
    total: u64,
    state: JobState,
    ended: Option<Instant>,
}

impl JobShared {
    fn inner(&self) -> std::sync::MutexGuard<'_, JobInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn append(&self, chunk: &[u8]) {
        let mut inner = self.inner();
        inner.output.extend(chunk);
        let excess = inner.output.len().saturating_sub(JOB_OUTPUT);
        inner.output.drain(..excess);
        inner.total = inner.total.saturating_add(chunk.len() as u64);
        drop(inner);
        self.changed.notify_all();
    }

    fn finish(&self, state: JobState) {
        let mut inner = self.inner();
        inner.state = state;
        inner.ended = Some(Instant::now());
        drop(inner);
        self.changed.notify_all();
    }
}

/// A command running in the background over a private copy of a workspace. It is
/// never sealed: what it changes in its copy is discarded when it ends. It ends when
/// its command exits, when it is stopped or dropped, when its lifetime or disk
/// allowance runs out, or with its owner process (the thread that spawned it lives
/// until it ends, so the parent-death signal fires only then).
pub struct Job {
    shared: std::sync::Arc<JobShared>,
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job").field("state", &self.state()).finish()
    }
}

impl Job {
    pub fn state(&self) -> JobState {
        self.shared.inner().state.clone()
    }

    /// Up to `limit` bytes of output from stream offset `offset`. With a `wait`, it
    /// first waits (until `interrupt` is set at the latest) for output past `offset`
    /// or for the job to end.
    pub fn read(
        &self,
        offset: u64,
        limit: usize,
        wait: Duration,
        interrupt: &AtomicBool,
    ) -> JobOutput {
        let until = Instant::now() + wait;
        let mut inner = self.shared.inner();
        while inner.total <= offset
            && inner.state == JobState::Running
            && !interrupt.load(Ordering::SeqCst)
        {
            let now = Instant::now();
            if now >= until {
                break;
            }
            inner = self
                .shared
                .changed
                .wait_timeout(inner, (until - now).min(Duration::from_millis(100)))
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        let retained = inner.total - inner.output.len() as u64;
        let start = offset.clamp(retained, inner.total);
        let end = start.saturating_add(limit as u64).min(inner.total);
        let from = (start - retained) as usize;
        let bytes = inner
            .output
            .range(from..from + (end - start) as usize)
            .copied()
            .collect();
        let elapsed = inner
            .ended
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(self.shared.started);
        JobOutput {
            bytes,
            start,
            end,
            total: inner.total,
            state: inner.state.clone(),
            elapsed,
        }
    }

    /// Ends the job (its whole process tree) and waits until it has ended.
    pub fn stop(&self) -> JobState {
        self.shared.stop.store(true, Ordering::SeqCst);
        let thread = self
            .thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
        // A concurrent stop may hold the thread; the state settles when it ends.
        let mut inner = self.shared.inner();
        while inner.state == JobState::Running {
            inner = self
                .shared
                .changed
                .wait_timeout(inner, Duration::from_millis(100))
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        inner.state.clone()
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Releases a job's place in the owner's count when its thread ends.
struct JobPlace;
impl Drop for JobPlace {
    fn drop(&mut self) {
        RUNNING_JOBS.fetch_sub(1, Ordering::SeqCst);
    }
}

impl StrictExecutor {
    /// Starts `argv` in the background over a private copy of `tree` (written by
    /// `host`, as for `execute_tree`), confined like a run: the same sandbox, network
    /// choice, toolchain mounts, rlimits (CPU time follows `lifetime`) and cgroup. The
    /// job does not hold a run slot; at most MAX_JOBS run at once per owner process.
    /// Returns once the command has been spawned.
    pub fn start_job(
        &self,
        tree: &BTreeMap<String, Input>,
        argv: &[String],
        lifetime: Duration,
        options: RunOptions<'_>,
        sources: &[PathBuf],
        host: &mut dyn RunHost,
    ) -> std::io::Result<Job> {
        for source in sources {
            if !source.is_absolute() || !source.is_dir() || source_mount(source).is_none() {
                return Err(invalid("invalid job source mount"));
            }
        }
        if argv.is_empty()
            || argv.len() > 128
            || argv
                .iter()
                .any(|arg| arg.len() > MAX_BROKERED_ARG || arg.contains('\0'))
            || !Path::new(&argv[0]).is_absolute()
            || lifetime.is_zero()
            || lifetime > Duration::from_secs(MAX_JOB_SECS)
        {
            return Err(invalid("invalid job bounds"));
        }
        if RUNNING_JOBS.fetch_add(1, Ordering::SeqCst) >= MAX_JOBS {
            RUNNING_JOBS.fetch_sub(1, Ordering::SeqCst);
            return Err(failure(
                std::io::ErrorKind::Other,
                format!("{MAX_JOBS} background jobs are already running; stop one first"),
                &[],
                &[],
            ));
        }
        let place = JobPlace;
        let input_bytes = check_input(tree, &self.limits)?;
        let available = scratch_available()?;
        if !scratch_fits(available, input_bytes) {
            return Err(failure(
                std::io::ErrorKind::Other,
                format!(
                    "the host's scratch space is full: the job needs {} for its workspace plus {} free, and {} is free; nothing started",
                    size_text(input_bytes),
                    size_text(SCRATCH_HEADROOM),
                    size_text(available)
                ),
                &[],
                &[],
            ));
        }
        let scratch = Scratch::create()?;
        let workspace = scratch.workspace();
        materialize(&workspace, tree, host)?;
        if let Some(toolchain) = &options.toolchain {
            toolchain.validate().map_err(|error| {
                failure(
                    std::io::ErrorKind::Other,
                    format!("the job could not be prepared: {error}; nothing started"),
                    &[],
                    &[],
                )
            })?;
        }
        let isolated = self.sandboxed();
        let launch = self.launch(
            &scratch, &workspace, argv, &options, sources, None, false, lifetime,
        )?;
        let growth = Growth::new(
            &workspace,
            candidate_budget(available, &self.limits),
            input_bytes,
            self.limits.entries.saturating_mul(4),
        );
        let shared = std::sync::Arc::new(JobShared {
            inner: std::sync::Mutex::new(JobInner {
                output: std::collections::VecDeque::new(),
                total: 0,
                state: JobState::Running,
                ended: None,
            }),
            changed: std::sync::Condvar::new(),
            stop: AtomicBool::new(false),
            started: Instant::now(),
        });
        let (ready, spawned) = std::sync::mpsc::sync_channel::<std::io::Result<()>>(1);
        let job = shared.clone();
        let thread = std::thread::Builder::new()
            .name("spindle-job".into())
            .spawn(move || {
                let _place = place;
                let state = watch_job(&job, launch, isolated, lifetime, growth, &ready);
                // The candidate goes before the job reads as ended.
                drop(scratch);
                job.finish(state);
            })?;
        match spawned.recv() {
            Ok(Ok(())) => Ok(Job {
                shared,
                thread: std::sync::Mutex::new(Some(thread)),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err(invalid("the job thread ended before its command started"))
            }
        }
    }
}

/// The job thread: spawns the command (so the parent-death signal is tied to a thread
/// that lives as long as the job), collects its output and ends it when it must.
fn watch_job(
    job: &JobShared,
    launch: Launch,
    isolated: bool,
    lifetime: Duration,
    mut growth: Growth,
    ready: &std::sync::mpsc::SyncSender<std::io::Result<()>>,
) -> JobState {
    let mut command = launch.command;
    let spawned = StrictChild::spawn(&mut command, !isolated, launch.caps);
    #[cfg(unix)]
    drop(launch.inherited);
    drop(command);
    #[allow(unused_mut)]
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            let reason = format!("the job could not start: {error}");
            let _ = ready.send(Err(error));
            return JobState::Ended(reason);
        }
    };
    #[cfg(target_os = "linux")]
    {
        child.cgroup = launch.cgroup;
    }
    let (Some(stdout), Some(stderr)) = (child.child.stdout.take(), child.child.stderr.take())
    else {
        let _ = ready.send(Err(invalid("missing job output pipes")));
        return JobState::Ended("the job's output could not be collected".into());
    };
    let _ = ready.send(Ok(()));
    let stopped = AtomicBool::new(false);
    let deadline = Instant::now() + lifetime;
    let state = std::thread::scope(|scope| {
        let output = scope.spawn(|| {
            pump(stdout, &stopped, &mut |chunk| {
                job.append(chunk);
                true
            })
        });
        let errors = scope.spawn(|| {
            pump(stderr, &stopped, &mut |chunk| {
                job.append(chunk);
                true
            })
        });
        let mut next_growth = Instant::now();
        let state = loop {
            if job.stop.load(Ordering::SeqCst) {
                break JobState::Ended("stopped".into());
            }
            if Instant::now() >= deadline {
                break JobState::Ended(format!("its {} s lifetime ran out", lifetime.as_secs()));
            }
            match child.child.try_wait() {
                Ok(Some(status)) => break JobState::Exited(status.code()),
                Ok(None) => {}
                Err(error) => {
                    break JobState::Ended(format!("the job could not be watched: {error}"));
                }
            }
            if Instant::now() >= next_growth {
                if growth.exceeded() {
                    break JobState::Ended(
                        "its private workspace copy grew past the disk allowance".into(),
                    );
                }
                // Drains the job object's notifications, which queue up over a long job.
                #[cfg(windows)]
                if let Some(tree) = &child.tree {
                    tree.memory_exhausted();
                }
                next_growth = Instant::now() + Duration::from_millis(500);
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let state = if child.out_of_memory(matches!(state, JobState::Exited(Some(0)))) {
            JobState::Ended(format!("it ran out of memory ({MEMORY_CAP})"))
        } else {
            state
        };
        child.kill_tree();
        if !matches!(state, JobState::Exited(_)) {
            child.stop();
        }
        let drain = Instant::now() + Duration::from_millis(250);
        while !(output.is_finished() && errors.is_finished()) && Instant::now() < drain {
            std::thread::sleep(Duration::from_millis(5));
        }
        stopped.store(true, Ordering::SeqCst);
        let _ = output.join();
        let _ = errors.join();
        state
    });
    // bubblewrap reports an exit code only once the command actually started.
    #[cfg(target_os = "linux")]
    if let (JobState::Exited(_), Some(mut reports)) = (&state, launch.reports) {
        let mut report = Vec::new();
        if set_nonblocking(&reports).is_ok() {
            let _ = Read::by_ref(&mut reports)
                .take(64 * 1024)
                .read_to_end(&mut report);
        }
        if !String::from_utf8_lossy(&report).contains("\"exit-code\"") {
            return JobState::Ended("the sandbox could not be set up (see the output)".into());
        }
    }
    state
}

/// A confined command ready to spawn, with what must stay open until it is spawned
/// (inherited descriptors) and what the spawned child takes over.
struct Launch {
    command: std::process::Command,
    /// The job object's caps (Windows).
    caps: crate::process_tree::TreeCaps,
    #[cfg(unix)]
    inherited: Vec<std::fs::File>,
    /// bubblewrap's status reports (isolated runs).
    #[cfg(target_os = "linux")]
    reports: Option<std::fs::File>,
    #[cfg(target_os = "linux")]
    cgroup: Option<crate::cgroup::OwnedCgroup>,
}

/// Writes the input tree into the empty candidate: directories, then files (created
/// new, never through a link), then links, so materializing never follows one.
/// Host load errors are returned as they are; filesystem errors mean nothing ran.
fn materialize(
    workspace: &Path,
    tree: &BTreeMap<String, Input>,
    host: &mut dyn RunHost,
) -> std::io::Result<BTreeMap<String, Stamp>> {
    let prepare = |error: std::io::Error| {
        failure(
            std::io::ErrorKind::Other,
            format!("the workspace could not be prepared for the run: {error}; nothing ran"),
            &[],
            &[],
        )
    };
    let mut stamps = BTreeMap::new();
    for (name, input) in tree {
        let path = workspace.join(name);
        match input {
            Input::Directory => std::fs::create_dir_all(&path).map_err(prepare)?,
            Input::File { size, executable } => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(prepare)?;
                }
                let mut options = std::fs::OpenOptions::new();
                options.create_new(true).write(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options
                        .mode(if *executable { 0o700 } else { 0o600 })
                        .custom_flags(libc::O_NOFOLLOW);
                }
                #[cfg(not(unix))]
                let _ = executable;
                let mut file = options.open(&path).map_err(prepare)?;
                {
                    let mut writer = std::io::BufWriter::with_capacity(1 << 20, &mut file);
                    host.load(name, &mut writer)?;
                    writer.flush().map_err(prepare)?;
                }
                let stamp = Stamp::of_file(&file).map_err(prepare)?;
                if stamp.size != *size {
                    return Err(invalid("input file size does not match its tree entry"));
                }
                stamps.insert(name.clone(), stamp);
            }
            Input::Link(_) => {}
        }
    }
    for (name, input) in tree {
        let Input::Link(target) = input else {
            continue;
        };
        let path = workspace.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(prepare)?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, &path).map_err(prepare)?;
        #[cfg(not(unix))]
        {
            let _ = target;
            return Err(prepare(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "symbolic links cannot be created on this host",
            )));
        }
    }
    settle_clock(workspace, &stamps);
    Ok(stamps)
}

/// Waits until the filesystem clock has moved past every materialized file's change
/// time. Kernels without fine-grained timestamps stamp files from a coarse clock (a
/// few milliseconds per tick), so a run that rewrote a file within the tick it was
/// written in would leave its stamp unchanged and the write would be sealed as
/// unchanged. A probe beside the workspace shows the current tick; this is bounded.
fn settle_clock(workspace: &Path, stamps: &BTreeMap<String, Stamp>) {
    let Some(newest) = stamps.values().map(|stamp| stamp.changed).max() else {
        return;
    };
    let Some(probe) = workspace.parent().map(|scratch| scratch.join("clock")) else {
        return;
    };
    for _ in 0..200 {
        // Created afresh each time: rewriting an empty file need not touch its times.
        let _ = std::fs::remove_file(&probe);
        let now = std::fs::write(&probe, b"")
            .and_then(|()| std::fs::File::open(&probe))
            .and_then(|file| Stamp::of_file(&file))
            .map(|stamp| stamp.changed);
        match now {
            Ok(now) if now > newest => break,
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(_) => break,
        }
    }
    let _ = std::fs::remove_file(&probe);
}

/// Seals the candidate: regular files (hard links are sealed as plain files), symbolic
/// links (never followed) and empty directories. A file whose stamp still equals its
/// materialized input's is reported unchanged and not read. Anything else (special
/// files, non-UTF-8 or overlong names, too many entries or bytes) is the reason the
/// run's changes cannot be kept. Where the platform has no executable bit (Windows), a
/// file keeps the bit it came in with.
#[allow(clippy::type_complexity)]
fn seal(
    root: &Path,
    input: &BTreeMap<String, Input>,
    baseline: &BTreeMap<String, Stamp>,
    limits: &TreeLimits,
    skipped: &mut Vec<String>,
) -> Result<(BTreeMap<String, SealedEntry>, BTreeMap<String, Stamp>), String> {
    let mut entries = BTreeMap::new();
    let mut stamps = BTreeMap::new();
    let (mut total, mut visited) = (0u64, 0usize);
    let too_many = || {
        format!(
            "the run left more than {} workspace entries (files, links and empty directories)",
            limits.entries
        )
    };
    let mut pending = vec![String::new()];
    while let Some(relative) = pending.pop() {
        let directory = root.join(&relative);
        let unreadable = |error: std::io::Error| {
            format!("`{}` could not be read: {error}", path_text(&relative))
        };
        let mut empty = true;
        // Nothing runs any more: a directory the run made unreadable is opened up.
        let listing = match std::fs::read_dir(&directory) {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                owner_access(&directory, 0o700);
                std::fs::read_dir(&directory)
            }
            listing => listing,
        };
        for entry in listing.map_err(unreadable)? {
            let entry = entry.map_err(unreadable)?;
            empty = false;
            visited += 1;
            if visited > limits.entries.saturating_mul(4) {
                return Err(too_many());
            }
            let name = entry.file_name().into_string().map_err(|name| {
                format!(
                    "a file name is not UTF-8 text: {}",
                    path_text(&name.to_string_lossy())
                )
            })?;
            let path = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            if !valid_path(&path) {
                return Err(format!(
                    "`{}` is not a supported workspace path (names up to {MAX_NAME} bytes, paths up to {MAX_PATH})",
                    path_text(&path)
                ));
            }
            #[cfg_attr(not(unix), allow(unused_mut))]
            let mut metadata = std::fs::symlink_metadata(entry.path())
                .map_err(|error| format!("`{}` could not be read: {error}", path_text(&path)))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                // A file the run left unreadable to its owner is made readable (which
                // also marks it changed), so its bytes can be kept.
                if metadata.file_type().is_file() && metadata.mode() & 0o400 == 0 {
                    owner_access(&entry.path(), (metadata.mode() & 0o7777) | 0o600);
                    metadata = std::fs::symlink_metadata(entry.path()).map_err(|error| {
                        format!("`{}` could not be read: {error}", path_text(&path))
                    })?;
                }
            }
            let kind = metadata.file_type();
            if kind.is_dir() {
                pending.push(path);
                continue;
            }
            if entries.len() >= limits.entries {
                return Err(too_many());
            }
            if kind.is_symlink() {
                let target = std::fs::read_link(entry.path())
                    .map_err(|error| format!("`{}` could not be read: {error}", path_text(&path)))?
                    .into_os_string()
                    .into_string()
                    .map_err(|_| {
                        format!(
                            "the link `{}` has a target that is not UTF-8 text",
                            path_text(&path)
                        )
                    })?;
                if target.is_empty() || target.len() > MAX_LINK {
                    return Err(format!(
                        "the link `{}` has a target longer than {MAX_LINK} bytes",
                        path_text(&path)
                    ));
                }
                entries.insert(path, SealedEntry::Link(target));
            } else if kind.is_file() {
                let size = metadata.len();
                if size > limits.file {
                    return Err(format!(
                        "`{}` is {}, more than the {} file limit",
                        path_text(&path),
                        size_text(size),
                        size_text(limits.file)
                    ));
                }
                total = total.saturating_add(size);
                if total > limits.tree {
                    return Err(format!(
                        "the workspace would hold more than {}",
                        size_text(limits.tree)
                    ));
                }
                let stamp = Stamp::of_path(&entry.path(), &metadata).map_err(|error| {
                    format!("`{}` could not be read: {error}", path_text(&path))
                })?;
                let came_in = match input.get(&path) {
                    Some(Input::File { executable, .. }) => Some(*executable),
                    _ => None,
                };
                let entry = match came_in {
                    Some(executable) if baseline.get(&path) == Some(&stamp) => {
                        SealedEntry::Unchanged { executable }
                    }
                    _ => {
                        #[cfg(unix)]
                        let executable = {
                            use std::os::unix::fs::MetadataExt;
                            metadata.mode() & 0o111 != 0
                        };
                        #[cfg(not(unix))]
                        let executable = came_in.unwrap_or(false);
                        stamps.insert(path.clone(), stamp);
                        SealedEntry::File { size, executable }
                    }
                };
                entries.insert(path, entry);
            } else {
                // Sockets and pipes left behind (test servers, IPC) are not workspace
                // content; they are left out and named, the rest of the run is kept.
                skipped.push(path);
            }
        }
        if empty && !relative.is_empty() {
            if entries.len() >= limits.entries {
                return Err(too_many());
            }
            entries.insert(relative, SealedEntry::Directory);
        }
    }
    Ok((entries, stamps))
}

/// Sets `mode` on a path of a finished run's candidate without following a link.
fn owner_access(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if let Ok(name) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
            unsafe {
                libc::fchmodat(
                    libc::AT_FDCWD,
                    name.as_ptr(),
                    mode as libc::mode_t,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
        }
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

/// Candidate growth, checked often but cheaply: the scratch filesystem's own usage
/// (one statvfs) bounds the candidate's growth from above, and only when that bound
/// is passed (another writer may share the volume) is the candidate walked, at most
/// about a tenth of the time. A polling bound, not a hard quota: a writer can exceed
/// it by what it writes between checks (each file also stays under RLIMIT_FSIZE).
struct Growth {
    root: PathBuf,
    base: Option<(u64, u64)>,
    budget: u64,
    input: u64,
    nodes: usize,
    next_walk: Instant,
    unreadable_since: Option<Instant>,
}
impl Growth {
    fn new(root: &Path, budget: u64, input: u64, nodes: usize) -> Self {
        Self {
            root: root.to_path_buf(),
            base: filesystem_usage(root),
            budget,
            input,
            nodes,
            next_walk: Instant::now(),
            unreadable_since: None,
        }
    }

    fn exceeded(&mut self) -> bool {
        // Whoever consumed it (concurrent runs, or files unlinked but still open), the
        // scratch headroom is the host's.
        if scratch_available().is_ok_and(|free| free < SCRATCH_HEADROOM / 2) {
            return true;
        }
        if let (Some((bytes, inodes)), Some((now_bytes, now_inodes))) =
            (self.base, filesystem_usage(&self.root))
            && self.unreadable_since.is_none()
            && now_bytes.saturating_sub(bytes) <= self.budget.saturating_sub(self.input)
            && now_inodes.saturating_sub(inodes) <= self.nodes as u64
        {
            return false;
        }
        let started = Instant::now();
        if started < self.next_walk {
            return false;
        }
        let walk = candidate_exceeds(&self.root, self.budget, self.nodes);
        let exceeded = self.check_walk(started, walk);
        self.next_walk = Instant::now() + (started.elapsed() * 10).max(Duration::from_millis(50));
        exceeded
    }

    fn check_walk(&mut self, started: Instant, walk: Result<bool, ()>) -> bool {
        match walk {
            Ok(exceeded) => {
                self.unreadable_since = None;
                exceeded
            }
            Err(()) => {
                // A command may lock down its final output just before exiting. The
                // exit path restores access and seals it; a live hidden tree gets only
                // one polling grace before the monitor fails closed.
                started.duration_since(*self.unreadable_since.get_or_insert(started))
                    >= Duration::from_millis(100)
            }
        }
    }
}

/// Bytes and inodes in use on the filesystem holding `path`.
#[cfg(unix)]
fn filesystem_usage(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(name.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    let bytes = (stat.f_blocks as u64)
        .saturating_sub(stat.f_bfree as u64)
        .saturating_mul(stat.f_frsize as u64);
    #[allow(clippy::unnecessary_cast)]
    let inodes = (stat.f_files as u64).saturating_sub(stat.f_ffree as u64);
    Some((bytes, inodes))
}

#[cfg(not(unix))]
fn filesystem_usage(_: &Path) -> Option<(u64, u64)> {
    None
}

/// A close-on-exec pipe (read end, write end).
#[cfg(unix)]
fn pipe() -> std::io::Result<(std::fs::File, std::fs::File)> {
    use std::os::fd::FromRawFd;
    let mut fds = [0 as libc::c_int; 2];
    #[cfg(target_os = "linux")]
    let created = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == 0;
    #[cfg(not(target_os = "linux"))]
    let created = unsafe { libc::pipe(fds.as_mut_ptr()) } == 0
        && fds
            .iter()
            .all(|&fd| unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0);
    if !created {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe {
        (
            std::fs::File::from_raw_fd(fds[0]),
            std::fs::File::from_raw_fd(fds[1]),
        )
    })
}

#[cfg(unix)]
fn set_nonblocking(file: &impl std::os::fd::AsRawFd) -> std::io::Result<()> {
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Waits up to 20 ms for `events` on `fd`.
#[cfg(unix)]
fn wait_for(fd: libc::c_int, events: libc::c_short) {
    let mut poll = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    unsafe { libc::poll(&mut poll, 1, 20) };
}

/// Collects one output stream (1 stdout, 2 stderr), showing chunks to `observe`; more
/// than MAX_OUTPUT floods the run.
fn collect_output(
    source: u8,
    observe: Option<OutputObserver<'_>>,
    flood: &AtomicBool,
    stream: impl FnOnce(&mut dyn FnMut(&[u8]) -> bool) -> std::io::Result<()>,
) -> std::io::Result<Vec<u8>> {
    let mut output = vec![];
    stream(&mut |chunk: &[u8]| {
        if let Some(observe) = observe {
            observe(source, chunk);
        }
        output.extend_from_slice(chunk);
        if output.len() > MAX_OUTPUT {
            flood.store(true, Ordering::SeqCst);
            return false;
        }
        true
    })?;
    Ok(output)
}

/// Copies a pipe to `sink` until EOF, or until `sink` returns false. Once `stop` is
/// set it returns as soon as nothing more is buffered: a descendant that escaped the
/// run could otherwise hold the pipe open forever.
#[cfg(unix)]
fn pump<S: Read + std::os::fd::AsRawFd>(
    mut stream: S,
    stop: &AtomicBool,
    sink: &mut dyn FnMut(&[u8]) -> bool,
) -> std::io::Result<()> {
    set_nonblocking(&stream)?;
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                if !sink(&chunk[..count]) {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::SeqCst) {
                    return Ok(());
                }
                wait_for(stream.as_raw_fd(), libc::POLLIN);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Windows pipes have no readiness polling; the run's job object ends every process
/// holding them, so plain blocking reads reach EOF.
#[cfg(not(unix))]
fn pump<S: Read>(
    mut stream: S,
    _stop: &AtomicBool,
    sink: &mut dyn FnMut(&[u8]) -> bool,
) -> std::io::Result<()> {
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                if !sink(&chunk[..count]) {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

/// Writes all of `bytes` to a non-blocking pipe unless the run stops first.
#[cfg(unix)]
fn write_until(
    pipe: &mut std::fs::File,
    mut bytes: &[u8],
    stop: &AtomicBool,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    while !bytes.is_empty() {
        match pipe.write(bytes) {
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::SeqCst) {
                    return Err(invalid("run stopped"));
                }
                wait_for(pipe.as_raw_fd(), libc::POLLOUT);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Serves the dedicated RPC pipes: each request line is handed to the run loop and
/// answered with one reply line. A partial request at exit is dropped; an oversized
/// one stops the run.
#[allow(clippy::too_many_arguments)]
#[cfg(unix)]
fn serve_rpc(
    requests: std::fs::File,
    mut replies: std::fs::File,
    stop: &AtomicBool,
    interrupt: &AtomicBool,
    deadline: Instant,
    to_run: std::sync::mpsc::SyncSender<Vec<u8>>,
    from_run: std::sync::mpsc::Receiver<Vec<u8>>,
    flood: &AtomicBool,
) -> std::io::Result<()> {
    set_nonblocking(&replies)?;
    let mut frame = Vec::new();
    let mut failed = None;
    pump(requests, stop, &mut |chunk| {
        for piece in chunk.split_inclusive(|byte| *byte == b'\n') {
            if frame.len() + piece.len() > MAX_BROKER_REQUEST + 1 {
                flood.store(true, Ordering::SeqCst);
                failed = Some(invalid("broker request too large"));
                return false;
            }
            frame.extend_from_slice(piece);
            if frame.last() != Some(&b'\n') {
                continue;
            }
            frame.pop();
            if to_run.send(std::mem::take(&mut frame)).is_err() {
                return false;
            }
            let reply = loop {
                match from_run.recv_timeout(Duration::from_millis(10)) {
                    Ok(reply) => break reply,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                        if !stop.load(Ordering::SeqCst)
                            && !interrupt.load(Ordering::SeqCst)
                            && Instant::now() < deadline => {}
                    _ => return false,
                }
            };
            if let Err(error) = write_until(&mut replies, &reply, stop)
                .and_then(|()| write_until(&mut replies, b"\n", stop))
            {
                failed = Some(error);
                return false;
            }
        }
        true
    })?;
    failed.map_or(Ok(()), Err)
}

/// Windows brokered runs: request lines start with 0x1E on stdout and replies are
/// written to stdin. Ordinary output goes to `sink`; a partial request at exit is
/// dropped.
#[allow(clippy::too_many_arguments)]
#[cfg(not(unix))]
fn stdio_broker(
    stdout: std::process::ChildStdout,
    mut stdin: std::process::ChildStdin,
    stop: &AtomicBool,
    interrupt: &AtomicBool,
    deadline: Instant,
    to_run: std::sync::mpsc::SyncSender<Vec<u8>>,
    from_run: std::sync::mpsc::Receiver<Vec<u8>>,
    flood: &AtomicBool,
    sink: &mut dyn FnMut(&[u8]) -> bool,
) -> std::io::Result<()> {
    let mut frame = None::<Vec<u8>>;
    let mut failed = None;
    pump(stdout, stop, &mut |chunk| {
        let mut text = Vec::new();
        for &byte in chunk {
            if let Some(request) = &mut frame {
                if byte != b'\n' {
                    if request.len() >= MAX_BROKER_REQUEST {
                        flood.store(true, Ordering::SeqCst);
                        failed = Some(invalid("broker request too large"));
                        return false;
                    }
                    request.push(byte);
                    continue;
                }
                if to_run.send(frame.take().unwrap_or_default()).is_err() {
                    return false;
                }
                let reply = loop {
                    match from_run.recv_timeout(Duration::from_millis(10)) {
                        Ok(reply) => break reply,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                            if !stop.load(Ordering::SeqCst)
                                && !interrupt.load(Ordering::SeqCst)
                                && Instant::now() < deadline => {}
                        _ => return false,
                    }
                };
                if let Err(error) = stdin
                    .write_all(&reply)
                    .and_then(|()| stdin.write_all(b"\n"))
                    .and_then(|()| stdin.flush())
                {
                    failed = Some(error);
                    return false;
                }
            } else if byte == 0x1e {
                frame = Some(Vec::new());
            } else {
                text.push(byte);
            }
        }
        sink(&text)
    })?;
    failed.map_or(Ok(()), Err)
}

/// Per-process rlimits (Unix). Workspace files stay bounded by the sealed-tree checks either
/// way; the file-size cap is the private /tmp's size (a toolchain run writes large build
/// artifacts to its cache), and compilers are multi-threaded (CPU time and RLIMIT_NPROC
/// both count threads). RLIMIT_DATA bounds private writable memory only; shared mappings
/// (MAP_SHARED, memfd) count against the address space alone, which stays far larger
/// because runtimes reserve address space they never commit (V8's Oilpan CagedHeap and
/// WebAssembly guard regions, the JVM heap, worker threads) and abort below that. So
/// without a delegated cgroup a run's memory is not reliably bounded; with one, the
/// cgroup's memory.max is. CPU time follows the run's own timeout on every core, so the
/// wall-clock deadline is what ends a run. A delegated cgroup caps processes and
/// aggregate memory more precisely; RLIMIT_NPROC is then not applied.
#[cfg(unix)]
#[derive(Clone, Copy)]
struct ProcessLimits {
    file_size: u64,
    address_space: u64,
    data: u64,
    cpu_seconds: u64,
    processes: Option<u64>,
}

/// Processes and threads one run may add.
fn run_tasks(toolchain: bool) -> u64 {
    if toolchain { 4096 } else { 1024 }
}

/// CPU seconds one process of a run may use: the run's timeout on every core, so the
/// wall-clock deadline is what ends a run.
fn cpu_allowance(timeout: Duration) -> u64 {
    let cores = std::thread::available_parallelism().map_or(1, |cores| cores.get() as u64);
    timeout
        .as_secs()
        .max(1)
        .saturating_mul(cores)
        .saturating_add(10)
}

#[cfg(unix)]
impl ProcessLimits {
    fn for_run(toolchain: bool, timeout: Duration, pids_capped: bool) -> Self {
        let (file_size, data) = if toolchain {
            (4 << 30, 16 << 30)
        } else {
            (SANDBOX_TMP, 4 << 30)
        };
        Self {
            file_size,
            address_space: 256 << 30,
            data,
            cpu_seconds: cpu_allowance(timeout),
            processes: (!pids_capped).then(|| user_tasks().saturating_add(run_tasks(toolchain))),
        }
    }
}

/// Windows has no rlimits or cgroups: each run's job object caps its committed memory
/// at three quarters of physical memory, its live processes and each process's CPU time.
#[cfg(windows)]
fn job_caps(toolchain: bool, timeout: Duration) -> crate::process_tree::TreeCaps {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    let memory = (unsafe { GlobalMemoryStatusEx(&mut status) } != 0 && status.ullTotalPhys > 0)
        .then(|| status.ullTotalPhys / 4 * 3);
    crate::process_tree::TreeCaps {
        memory,
        processes: u32::try_from(run_tasks(toolchain)).ok(),
        cpu_per_process: Some(Duration::from_secs(cpu_allowance(timeout))),
    }
}

/// The memory cap an out-of-memory run hit, as it is reported.
#[cfg(target_os = "linux")]
const MEMORY_CAP: &str = "the limit all runs share";
#[cfg(not(target_os = "linux"))]
const MEMORY_CAP: &str = "the run's limit of three quarters of physical memory";

/// Tasks (threads) this user already runs. RLIMIT_NPROC is checked against the
/// user's total, not the run's own, so the run's allowance is added on top of this
/// point-in-time count; a fixed cap would make every fork fail for a user who
/// already has that many threads. If the user's other tasks grow by more than the
/// allowance during a run, that run's forks fail (a failed run, never a leak). The
/// count reads every process's status, so it is reused for a second.
#[cfg(target_os = "linux")]
fn user_tasks() -> u64 {
    static RECENT: std::sync::Mutex<Option<(Instant, u64)>> = std::sync::Mutex::new(None);
    let mut recent = RECENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((counted, tasks)) = *recent
        && counted.elapsed() < Duration::from_secs(1)
    {
        return tasks;
    }
    let tasks = count_user_tasks();
    *recent = Some((Instant::now(), tasks));
    tasks
}

#[cfg(target_os = "linux")]
fn count_user_tasks() -> u64 {
    let uid = unsafe { libc::getuid() };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let mut tasks = 0u64;
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
        {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let (mut real, mut threads) = (None, 0u64);
        for line in status.lines() {
            if let Some(ids) = line.strip_prefix("Uid:") {
                real = ids.split_whitespace().next().and_then(|id| id.parse().ok());
            } else if let Some(count) = line.strip_prefix("Threads:") {
                threads = count.trim().parse().unwrap_or(0);
            }
        }
        if real == Some(uid) {
            tasks = tasks.saturating_add(threads);
        }
    }
    tasks
}

/// RLIMIT_NPROC is not applied off Linux, so no baseline is needed.
#[cfg(all(unix, not(target_os = "linux")))]
fn user_tasks() -> u64 {
    0
}

/// Unix pre-exec setup. Linux: parent-death signal, owner check, no new privileges,
/// closed descriptors and rlimits for every run. Other Unix systems apply the
/// rlimits they support. Host runs lead their own session so the whole process
/// group can be killed when the run ends.
#[cfg(unix)]
fn confine(
    command: &mut std::process::Command,
    host: bool,
    inherit: Vec<libc::c_int>,
    limits: ProcessLimits,
) {
    use std::os::unix::process::CommandExt;
    #[cfg(target_os = "linux")]
    let owner = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
            #[cfg(target_os = "linux")]
            {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != owner {
                    return Err(std::io::Error::from_raw_os_error(libc::ESTALE));
                }
            }
            if host && libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::syscall(
                    libc::SYS_close_range,
                    3u32,
                    u32::MAX,
                    libc::CLOSE_RANGE_CLOEXEC,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            for &fd in &inherit {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            #[cfg(target_os = "linux")]
            let processes = limits.processes.map(|cap| (libc::RLIMIT_NPROC, cap, true));
            // macOS counts processes per user against a system-wide per-user ceiling,
            // so a run cap would starve the user's own desktop; it is not applied.
            #[cfg(not(target_os = "linux"))]
            let processes: Option<(libc::c_int, u64, bool)> = {
                let _ = limits.processes;
                None
            };
            for (resource, cap, required) in [
                (libc::RLIMIT_CORE, 0, true),
                (libc::RLIMIT_FSIZE, limits.file_size, true),
                (libc::RLIMIT_CPU, limits.cpu_seconds, true),
                // macOS does not reliably honour the address-space limit; Linux must.
                (
                    libc::RLIMIT_AS,
                    limits.address_space,
                    cfg!(target_os = "linux"),
                ),
                (libc::RLIMIT_DATA, limits.data, cfg!(target_os = "linux")),
            ]
            .into_iter()
            .chain(processes)
            {
                let mut inherited: libc::rlimit = std::mem::zeroed();
                if libc::getrlimit(resource, &mut inherited) != 0 {
                    if required {
                        return Err(std::io::Error::last_os_error());
                    }
                    continue;
                }
                #[allow(clippy::unnecessary_cast)]
                let value = (cap as libc::rlim_t)
                    .min(inherited.rlim_cur)
                    .min(inherited.rlim_max);
                let limit = libc::rlimit {
                    rlim_cur: value,
                    rlim_max: value,
                };
                if libc::setrlimit(resource, &limit) != 0 && required {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

/// The spawned run. Host runs own their whole process tree: a process group on
/// Unix, a kill-on-close job object on Windows; ending the run ends the tree. On
/// Linux with a delegated cgroup every run also has its own cgroup, which ends
/// descendants that left the group (setsid) as well.
struct StrictChild {
    child: std::process::Child,
    group: bool,
    #[cfg(target_os = "linux")]
    cgroup: Option<crate::cgroup::OwnedCgroup>,
    #[cfg(windows)]
    tree: Option<crate::process_tree::ChildProcessTree>,
}
impl StrictChild {
    fn spawn(
        command: &mut std::process::Command,
        group: bool,
        caps: crate::process_tree::TreeCaps,
    ) -> std::io::Result<Self> {
        let child = command.spawn()?;
        #[cfg(windows)]
        {
            // The child starts suspended; the tree assigns it to a capped job, then
            // resumes it.
            let mut spawned = Self {
                child,
                group,
                tree: None,
            };
            spawned.tree = Some(crate::process_tree::ChildProcessTree::for_std_capped(
                &spawned.child,
                caps,
            )?);
            Ok(spawned)
        }
        #[cfg(not(windows))]
        {
            let _ = caps;
            Ok(Self {
                child,
                group,
                #[cfg(target_os = "linux")]
                cgroup: None,
            })
        }
    }

    fn kill_group(&self) {
        if !self.group {
            return;
        }
        #[cfg(unix)]
        if let Ok(pid) = libc::pid_t::try_from(self.child.id()) {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        #[cfg(windows)]
        if let Some(tree) = &self.tree {
            tree.terminate_after_root_exit();
        }
    }

    /// Whether the run ran out of memory: on Linux the OOM killer ended part of it (only
    /// known with a cgroup); on Windows the job's cap made an allocation fail, which
    /// counts only if the run did not succeed anyway.
    fn out_of_memory(&self, succeeded: bool) -> bool {
        #[cfg(target_os = "linux")]
        {
            let _ = succeeded;
            self.cgroup.as_ref().is_some_and(|group| group.oom_killed())
        }
        #[cfg(windows)]
        {
            !succeeded
                && self
                    .tree
                    .as_ref()
                    .is_some_and(|tree| tree.memory_exhausted())
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let _ = succeeded;
            false
        }
    }

    /// Kills everything the run left: its process group and its cgroup's members.
    fn kill_tree(&self) {
        self.kill_group();
        #[cfg(target_os = "linux")]
        if let Some(cgroup) = &self.cgroup {
            cgroup.kill();
        }
    }

    fn stop(&mut self) {
        self.kill_tree();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Drop for StrictChild {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The system command interpreter on Windows (`%ComSpec%`, normally cmd.exe).
pub fn system_shell() -> String {
    std::env::var("ComSpec").unwrap_or_else(|_| {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        format!(r"{root}\System32\cmd.exe")
    })
}

/// Interpreters the workspace tools run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Program {
    Bash,
    Python,
    Node,
}

impl Program {
    fn names(self) -> &'static [&'static str] {
        match (self, cfg!(windows)) {
            (Self::Bash, false) => &["bash"],
            (Self::Bash, true) => &["bash.exe"],
            (Self::Python, false) => &["python3"],
            (Self::Python, true) => &["python.exe", "python3.exe"],
            (Self::Node, false) => &["node"],
            (Self::Node, true) => &["node.exe"],
        }
    }
    /// The environment variable that names this interpreter for host runs.
    pub fn variable(self) -> &'static str {
        match self {
            Self::Bash => "SPINDLE_BASH",
            Self::Python => "SPINDLE_PYTHON",
            Self::Node => "SPINDLE_NODE",
        }
    }
}

impl StrictExecutor {
    /// Absolute path of an interpreter for this runner. Sandboxed runs use the
    /// sandbox's /usr; host runs use `SPINDLE_BASH`/`SPINDLE_PYTHON`/`SPINDLE_NODE`
    /// when set, then Git for Windows' bash on Windows, then the host PATH.
    pub fn program(&self, program: Program) -> Option<String> {
        if self.sandboxed() {
            return Some(
                match program {
                    Program::Bash => "/bin/bash",
                    Program::Python => "/usr/bin/python3",
                    Program::Node => "/usr/bin/node",
                }
                .into(),
            );
        }
        host_program(program)
    }
}

/// Host lookup used by unisolated runs and by hosts that run helper commands.
pub fn host_program(program: Program) -> Option<String> {
    // Windows "App Execution Aliases" in WindowsApps are stubs: bash.exe there starts
    // WSL and python.exe opens the Store, so they never count as an interpreter.
    let usable = |path: &Path| {
        path.is_absolute()
            && path.is_file()
            && !path
                .components()
                .any(|part| part.as_os_str().eq_ignore_ascii_case("WindowsApps"))
    };
    if let Some(path) = std::env::var_os(program.variable()).map(PathBuf::from) {
        return usable(&path).then(|| path.to_string_lossy().into_owned());
    }
    if cfg!(windows) && program == Program::Bash {
        for root in ["ProgramFiles", "ProgramW6432", "LocalAppData"] {
            if let Some(root) = std::env::var_os(root) {
                for relative in [r"Git\bin\bash.exe", r"Programs\Git\bin\bash.exe"] {
                    let path = PathBuf::from(&root).join(relative);
                    if usable(&path) {
                        return Some(path.to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|directory| program.names().iter().map(move |name| directory.join(name)))
        .find(|path| usable(path))
        .map(|path| path.to_string_lossy().into_owned())
}

/// Descriptors and variables an isolated run is set up with.
#[cfg(target_os = "linux")]
struct Mounts<'a> {
    /// The candidate workspace directory, bound at /workspace.
    directory: libc::c_int,
    /// The sealed seccomp program.
    filter: libc::c_int,
    /// bubblewrap's JSON status reports (the command's exit code once it started).
    status: libc::c_int,
    /// `SPINDLE_RPC_FDS` for brokered runs.
    rpc: Option<&'a str>,
}

/// bubblewrap invocation: private namespaces, read-only system, the workspace copy
/// at /workspace, the seccomp filter, and the toolchain mounts when requested.
#[cfg(target_os = "linux")]
fn isolated_command(
    helper: &std::fs::File,
    mounts: &Mounts<'_>,
    argv: &[String],
    options: &RunOptions<'_>,
    sources: &[PathBuf],
) -> std::process::Command {
    use std::os::fd::AsRawFd;
    let mut command = std::process::Command::new(format!("/proc/self/fd/{}", helper.as_raw_fd()));
    command
        .env_clear()
        .current_dir("/")
        .args(["--unshare-user", "--unshare-pid"])
        .args((!options.network).then_some("--unshare-net"))
        .args([
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-cgroup",
            "--disable-userns",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--clearenv",
            "--setenv",
            "PATH",
            "/usr/bin:/bin",
            "--setenv",
            "HOME",
            "/tmp",
            "--setenv",
            "TMPDIR",
            "/tmp",
            "--ro-bind",
            "/usr",
            "/usr",
            "--symlink",
            "usr/bin",
            "/bin",
            "--symlink",
            "usr/lib",
            "/lib",
            "--symlink",
            "usr/lib64",
            "/lib64",
            // Debian-style command names (cc, c++, which, awk, ...) resolve here.
            "--ro-bind-try",
            "/etc/alternatives",
            "/etc/alternatives",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--size",
        ])
        .arg(SANDBOX_TMP.to_string())
        .args(["--tmpfs", "/tmp", "--json-status-fd"])
        .arg(mounts.status.to_string())
        .arg("--bind-fd")
        .arg(mounts.directory.to_string())
        .args(["/workspace", "--chdir", "/workspace"]);
    if let Some(rpc) = mounts.rpc {
        command.args(["--setenv", "SPINDLE_RPC_FDS", rpc]);
    }
    for source in sources {
        if let Some(mount) = source_mount(source) {
            command.arg("--ro-bind").arg(source).arg(mount);
        }
    }
    if options.network {
        for path in NETWORK_CONFIG {
            command.args(["--ro-bind-try", path, path]);
        }
    }
    if let Some(toolchain) = &options.toolchain {
        command
            .arg("--ro-bind")
            .arg(toolchain.rustup_home)
            .arg("/opt/rustup")
            .arg("--ro-bind-try")
            .arg(toolchain.cargo_home.join("bin"))
            .arg("/opt/cargo/bin")
            .arg("--bind")
            .arg(toolchain.cache)
            .arg("/cache");
        for source in toolchain.sources {
            if let Some(mount) = source_mount(source) {
                command.arg("--ro-bind").arg(source).arg(mount);
            }
        }
        // CARGO_HOME is fresh per run: configuration or credentials written there never
        // reach a later run. Only downloads (registry, git checkouts) persist.
        command
            .args(["--tmpfs", "/opt/cargo-home", "--bind"])
            .arg(toolchain.cache.join("cargo/git"))
            .arg("/opt/cargo-home/git");
        let registry = toolchain.cargo_home.join("registry");
        if registry.is_dir() {
            // Reads see the host registry; downloads land in the cache's upper layer.
            command
                .arg("--overlay-src")
                .arg(&registry)
                .arg("--overlay")
                .arg(toolchain.cache.join("overlay/upper"))
                .arg(toolchain.cache.join("overlay/work"))
                .arg("/opt/cargo-home/registry");
        }
        command.args([
            "--setenv",
            "PATH",
            "/opt/cargo/bin:/usr/bin:/bin",
            "--setenv",
            "RUSTUP_HOME",
            "/opt/rustup",
            "--setenv",
            "CARGO_HOME",
            "/opt/cargo-home",
            "--setenv",
            "CARGO_TARGET_DIR",
            "/cache/target",
            "--setenv",
            "TMPDIR",
            "/cache/tmp",
        ]);
        if !options.network {
            command.args(["--setenv", "CARGO_NET_OFFLINE", "true"]);
        }
    }
    command
        .arg("--seccomp")
        .arg(mounts.filter.to_string())
        .arg("--")
        .args(argv);
    command
}

/// Host variables a direct run keeps: the program search path and, on Windows, the
/// system locations programs need to start. Everything else (credentials included)
/// is dropped.
const HOST_VARIABLES: [&str; 10] = [
    "PATH",
    "PATHEXT",
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ComSpec",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
    "LANG",
];

/// Direct host invocation in the workspace copy with a clean environment. Nothing is
/// isolated: `network` cannot be withheld, toolchain directories are the host's own
/// (downloads land in the host registry), and sources are linked, not mounted
/// read-only, beside the workspace so `../NAME` path dependencies still resolve.
fn host_command(
    scratch: &Scratch,
    workspace: &Path,
    argv: &[String],
    options: &RunOptions<'_>,
) -> std::io::Result<std::process::Command> {
    let home = scratch.0.join("home");
    let tmp = scratch.0.join("tmp");
    for directory in [&home, &tmp] {
        private_dir(directory)?;
    }
    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]).env_clear().current_dir(workspace);
    for name in HOST_VARIABLES {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.env("PATH", host_search_path(argv, options)?);
    command
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("TMPDIR", &tmp)
        .env("TEMP", &tmp)
        .env("TMP", &tmp);
    if std::env::var_os("LANG").is_none() {
        command.env("LANG", "C.UTF-8");
    }
    if let Some(toolchain) = &options.toolchain {
        command
            .env("RUSTUP_HOME", toolchain.rustup_home)
            .env("CARGO_HOME", toolchain.cargo_home)
            .env("CARGO_TARGET_DIR", toolchain.cache.join("target"))
            .env("TMPDIR", toolchain.cache.join("tmp"));
        if !options.network {
            command.env("CARGO_NET_OFFLINE", "true");
        }
        for source in toolchain.sources {
            if let Some(mount) = source_mount(source) {
                let link = scratch.0.join(&mount[1..]);
                #[cfg(unix)]
                std::os::unix::fs::symlink(source, &link)?;
                #[cfg(windows)]
                std::os::windows::fs::symlink_dir(source, &link).map_err(|error| {
                    std::io::Error::new(
                        error.kind(),
                        format!(
                            "cannot link {} beside the workspace (Windows needs Developer Mode for this): {error}",
                            source.display()
                        ),
                    )
                })?;
            }
        }
    }
    Ok(command)
}

/// The host PATH, after the toolchain's cargo bin when requested. On Windows the
/// directories of the interpreter and of bash come first, plus Git for Windows'
/// `usr\bin` beside its `bin` launcher, so the POSIX tools shipped with bash (cat,
/// sed, sleep, ...) resolve before same-named Windows programs, also for `sh()` in
/// run_code scripts.
fn host_search_path(
    argv: &[String],
    options: &RunOptions<'_>,
) -> std::io::Result<std::ffi::OsString> {
    let mut search: Vec<PathBuf> = options
        .toolchain
        .iter()
        .map(|toolchain| toolchain.cargo_home.join("bin"))
        .collect();
    if cfg!(windows) {
        for program in [Some(argv[0].clone()), host_program(Program::Bash)]
            .into_iter()
            .flatten()
        {
            let Some(directory) = Path::new(&program).parent() else {
                continue;
            };
            search.push(directory.to_path_buf());
            if let Some(tools) = directory
                .parent()
                .map(|root| root.join("usr").join("bin"))
                .filter(|tools| tools.is_dir())
            {
                search.push(tools);
            }
        }
    }
    search.extend(
        std::env::var_os("PATH")
            .iter()
            .flat_map(std::env::split_paths),
    );
    std::env::join_paths(search).map_err(|_| invalid("invalid PATH"))
}

/// Creates a directory only this user can read (on Unix; Windows user temp
/// directories are already per-user).
fn private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::DirBuilder::new().create(path)
    }
}

/// The second field holds an exclusive lock for the owner's lifetime, so an unlocked
/// candidate belongs to a dead owner. On Unix it locks the candidate directory itself
/// (close-on-exec, never inherited); only its `w` child is bound into a sandbox, so a
/// run can never chmod or replace the locked root. Windows cannot lock directories,
/// so it locks a sibling `<name>.lock` file created first.
struct Scratch(PathBuf, #[allow(dead_code)] std::fs::File);
impl Scratch {
    fn workspace(&self) -> PathBuf {
        self.0.join("w")
    }
    fn create() -> std::io::Result<Self> {
        for _ in 0..16 {
            let mut random = [0u8; 16];
            getrandom(&mut random)?;
            let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
            let path = scratch_root().join(format!("spindle-candidate-{suffix}"));
            #[cfg(unix)]
            let created = {
                // Locked under a name the sweeper ignores, then renamed, so a sweep
                // can never observe a live candidate before its lock exists.
                let staging = scratch_root().join(format!("spindle-candidate-{suffix}.new"));
                match private_dir(&staging) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
                let lock = std::fs::File::open(&staging)?;
                if let Err(error) = lock.try_lock() {
                    let _ = std::fs::remove_dir(&staging);
                    return Err(std::io::Error::other(error.to_string()));
                }
                if std::fs::symlink_metadata(&path).is_ok() {
                    let _ = std::fs::remove_dir(&staging);
                    continue;
                }
                std::fs::rename(&staging, &path)?;
                lock
            };
            #[cfg(not(unix))]
            let created = {
                let lock = match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(lock_path(&path))
                {
                    Ok(lock) => lock,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                };
                let locked = lock
                    .try_lock()
                    .map_err(|error| std::io::Error::other(error.to_string()))
                    .and_then(|()| private_dir(&path));
                if let Err(error) = locked {
                    drop(lock);
                    let _ = std::fs::remove_file(lock_path(&path));
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        continue;
                    }
                    return Err(error);
                }
                lock
            };
            let scratch = Self(path, created);
            private_dir(&scratch.workspace())?;
            return Ok(scratch);
        }
        Err(invalid("could not allocate private candidate"))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        // The lock file goes only with its candidate, so a candidate that could not be
        // removed stays recognizable to the sweeper.
        let removed = remove_candidate(&self.0).is_ok();
        #[cfg(not(unix))]
        if removed {
            let _ = std::fs::remove_file(lock_path(&self.0));
        }
        #[cfg(unix)]
        let _ = removed;
    }
}

#[cfg(not(unix))]
fn lock_path(candidate: &Path) -> PathBuf {
    let mut name = candidate.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Random bytes from the operating system.
fn getrandom(buffer: &mut [u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open("/dev/urandom")?.read_exact(buffer)
    }
    #[cfg(windows)]
    {
        // ProcessPrng never fails once loaded (documented by Microsoft).
        #[link(name = "bcryptprimitives", kind = "raw-dylib")]
        unsafe extern "system" {
            fn ProcessPrng(data: *mut u8, length: usize) -> i32;
        }
        unsafe { ProcessPrng(buffer.as_mut_ptr(), buffer.len()) };
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = buffer;
        Err(invalid("no system randomness"))
    }
}

/// The sandbox owns its tree (same uid) and can `chmod 000` its directories, which
/// would make a plain remove_dir_all fail and leak the candidate. Owner access is
/// restored top-down through descriptors opened without following links.
fn remove_candidate(root: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(root.as_os_str().as_bytes())
            .map_err(|_| invalid("invalid candidate path"))?;
        restore_access(libc::AT_FDCWD, &name, 0, &mut 8192);
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    restore_access_paths(root, 0, &mut 8192);
    std::fs::remove_dir_all(root)
}

/// As `restore_access` where /proc is unavailable: path-based and not following links.
#[cfg(all(unix, not(target_os = "linux")))]
fn restore_access_paths(path: &Path, depth: usize, budget: &mut usize) {
    use std::os::unix::fs::PermissionsExt;
    if depth > 128 || *budget == 0 {
        return;
    }
    *budget -= 1;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {}
        _ => return,
    }
    if std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).is_err() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            restore_access_paths(&entry.path(), depth + 1, budget);
        }
    }
}

/// Best effort and bounded (valid trees are at most 120 levels deep): a hostile tree
/// deeper or wider than this can still leak rather than exhaust stack or descriptors.
#[cfg(target_os = "linux")]
fn restore_access(parent: libc::c_int, name: &std::ffi::CStr, depth: usize, budget: &mut usize) {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStringExt;
    if depth > 128 || *budget == 0 {
        return;
    }
    *budget -= 1;
    if unsafe { libc::fchmodat(parent, name.as_ptr(), 0o700, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return;
    }
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return;
    }
    let directory = unsafe { OwnedFd::from_raw_fd(fd) };
    let Ok(entries) = std::fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd())) else {
        return;
    };
    let limit = *budget;
    let children: Vec<_> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .take(limit)
        .filter_map(|entry| std::ffi::CString::new(entry.file_name().into_vec()).ok())
        .collect();
    for child in children {
        restore_access(directory.as_raw_fd(), &child, depth + 1, budget);
    }
}

/// Reclaims candidates abandoned by a dead owner: only same-uid directories whose
/// owner lock is free. Leftovers were never adopted (receipts are), so this only
/// frees disk; it is never evidence about the effect that produced them.
pub fn sweep_abandoned_candidates() -> std::io::Result<usize> {
    let mut removed = 0;
    for entry in std::fs::read_dir(scratch_root())?.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(suffix) = name.strip_prefix("spindle-candidate-") else {
            continue;
        };
        if suffix.len() != 32 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        #[cfg(unix)]
        let lock = {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            if metadata.uid() != unsafe { libc::geteuid() } {
                continue;
            }
            // The locked root is never visible to a sandbox, so it stays readable; a
            // sandbox can only lock down `w` and below, which removal restores first.
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(&path)
        };
        // A creator makes and locks the lock file before the directory, so a directory
        // whose lock file is missing has no live owner; recreating it lets it be swept.
        #[cfg(not(unix))]
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path(&path));
        let Ok(lock) = lock else {
            continue;
        };
        if lock.try_lock().is_err() {
            continue;
        }
        if remove_candidate(&path).is_ok() {
            removed += 1;
            #[cfg(not(unix))]
            {
                drop(lock);
                let _ = std::fs::remove_file(lock_path(&path));
            }
        }
    }
    Ok(removed)
}

/// Walks without following links. Unreadable trees are distinct from measured
/// excess; the monitor allows a bounded exit grace, never an unbounded blind spot.
fn candidate_exceeds(root: &Path, budget: u64, limit: usize) -> Result<bool, ()> {
    let mut pending = vec![root.to_path_buf()];
    let (mut bytes, mut nodes) = (0u64, 0usize);
    while let Some(directory) = pending.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(()),
        };
        for entry in entries {
            let entry = entry.map_err(|_| ())?;
            nodes += 1;
            let metadata = match std::fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err(()),
            };
            #[cfg(unix)]
            let used = {
                use std::os::unix::fs::MetadataExt;
                metadata.len().max(metadata.blocks() * 512)
            };
            #[cfg(not(unix))]
            let used = metadata.len();
            bytes = bytes.saturating_add(used);
            if metadata.is_dir() {
                pending.push(entry.path());
            }
            if nodes > limit || bytes > budget {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Read-only host configuration a networked run needs for DNS and TLS trust.
#[cfg(target_os = "linux")]
const NETWORK_CONFIG: [&str; 6] = [
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/gai.conf",
    "/etc/ssl",
    "/etc/ca-certificates",
];

/// The run's seccomp program. socket() is limited by address family: with `network`
/// (the shared host namespace) to AF_INET/AF_INET6, so host AF_UNIX/abstract and
/// netlink sockets stay unreachable; in a private namespace also to AF_UNIX and
/// route netlink, which reach only the run's own loopback, mounts and interfaces.
/// Anonymous socketpairs are always allowed. Process inspection, mounts (old and new
/// API), namespaces, BPF, io_uring, kernel keyrings, perf events and userfaultfd are
/// refused.
#[cfg(target_os = "linux")]
fn network_filter(network: bool) -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xc000_00b7;
    #[cfg(target_arch = "riscv64")]
    const ARCH: u32 = 0xc000_00f3;
    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )))]
    return Err(invalid("strict seccomp architecture unavailable"));
    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    ))]
    {
        let statement = |code: u16, k: u32| libc::sock_filter {
            code,
            jt: 0,
            jf: 0,
            k,
        };
        let jump = |k: u32, jt: u8, jf: u8| libc::sock_filter {
            code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            jt,
            jf,
            k,
        };
        let load = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
        let ret = (libc::BPF_RET | libc::BPF_K) as u16;
        let deny = statement(ret, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32);
        let allow = statement(ret, libc::SECCOMP_RET_ALLOW);
        let mut filter = vec![
            statement(load, 4),
            jump(ARCH, 1, 0),
            statement(ret, libc::SECCOMP_RET_KILL_PROCESS),
            statement(load, 0),
        ];
        #[cfg(target_arch = "x86_64")]
        filter.extend([
            libc::sock_filter {
                code: (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16,
                jt: 0,
                jf: 1,
                k: 0x4000_0000,
            },
            statement(ret, libc::SECCOMP_RET_KILL_PROCESS),
        ]);
        // socket(): args[0] (the family) and args[2] (the protocol) low words, little
        // endian. Every path out of this block returns.
        let families = if network {
            vec![
                statement(load, 16),
                jump(libc::AF_INET as u32, 2, 0),
                jump(libc::AF_INET6 as u32, 1, 0),
                deny,
                allow,
            ]
        } else {
            vec![
                statement(load, 16),
                jump(libc::AF_INET as u32, 7, 0),
                jump(libc::AF_INET6 as u32, 6, 0),
                jump(libc::AF_UNIX as u32, 5, 0),
                jump(libc::AF_NETLINK as u32, 1, 0),
                deny,
                statement(load, 32),
                jump(libc::NETLINK_ROUTE as u32, 1, 0),
                deny,
                allow,
            ]
        };
        filter.push(jump(libc::SYS_socket as u32, 0, families.len() as u8));
        filter.extend(families);
        for syscall in [
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
            libc::SYS_pidfd_getfd,
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_open_tree,
            libc::SYS_move_mount,
            libc::SYS_fsopen,
            libc::SYS_fsconfig,
            libc::SYS_fsmount,
            libc::SYS_fspick,
            libc::SYS_mount_setattr,
            libc::SYS_unshare,
            libc::SYS_setns,
            libc::SYS_bpf,
            libc::SYS_keyctl,
            libc::SYS_add_key,
            libc::SYS_request_key,
            libc::SYS_perf_event_open,
            libc::SYS_userfaultfd,
        ] {
            filter.extend([jump(syscall as u32, 0, 1), deny]);
        }
        filter.push(statement(ret, libc::SECCOMP_RET_ALLOW));
        let fd = unsafe {
            libc::memfd_create(
                c"spindle-seccomp".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                filter.as_ptr().cast::<u8>(),
                filter.len() * std::mem::size_of::<libc::sock_filter>(),
            )
        };
        file.write_all(bytes)?;
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(0))?;
        if unsafe {
            libc::fcntl(
                fd,
                libc::F_ADD_SEALS,
                libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unreadable_growth_gets_one_exit_grace_but_live_hidden_usage_fails_closed() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("spindle-growth-grace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("locked")).unwrap();
        std::fs::write(root.join("locked/file"), b"small").unwrap();
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o0))
            .unwrap();
        let mut growth = Growth::new(&root, 1024 * 1024, 0, 100);
        let now = Instant::now();
        assert!(!growth.check_walk(now, Err(())));
        assert!(growth.check_walk(now + Duration::from_millis(100), Err(())));
        assert!(!growth.check_walk(now + Duration::from_millis(101), Ok(false)));
        assert!(growth.unreadable_since.is_none());
        growth.base = None;
        if candidate_exceeds(&root, growth.budget, growth.nodes).is_err() {
            assert!(
                !growth.exceeded(),
                "a just-finished command gets an exit grace"
            );
            growth.unreadable_since = Some(Instant::now() - Duration::from_secs(1));
            growth.next_walk = Instant::now();
            assert!(
                growth.exceeded(),
                "live unreadable usage cannot hide indefinitely"
            );
        }
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        assert_eq!(
            candidate_exceeds(&root, growth.budget, growth.nodes),
            Ok(false)
        );
        assert_eq!(candidate_exceeds(&root, 1, growth.nodes), Ok(true));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scratch_preflight_needs_the_input_plus_headroom_and_bounds_growth() {
        let input = 3 << 30;
        assert!(scratch_fits(input + SCRATCH_HEADROOM, input));
        assert!(!scratch_fits(input + SCRATCH_HEADROOM - 1, input));
        assert!(!scratch_fits(0, 0));
        let limits = TreeLimits::default();
        assert_eq!(candidate_budget(u64::MAX, &limits), 2 * MAX_TREE as u64);
        assert_eq!(candidate_budget(SCRATCH_HEADROOM + 5, &limits), 5);
    }

    #[cfg(unix)]
    #[test]
    fn runtimes_keep_their_address_space_while_committed_memory_stays_bounded() {
        let toolchain = ProcessLimits::for_run(true, Duration::from_secs(600), false);
        assert!(toolchain.address_space >= 48 << 30);
        assert_eq!(toolchain.data, 16 << 30);
        let plain = ProcessLimits::for_run(false, Duration::from_secs(30), true);
        assert_eq!((plain.address_space, plain.data), (256 << 30, 4 << 30));
        assert_eq!(plain.file_size, SANDBOX_TMP);
        assert!(plain.cpu_seconds >= 30 && plain.processes.is_none());
        let long = ProcessLimits::for_run(false, Duration::from_secs(MAX_RUN_SECS), false);
        assert!(long.cpu_seconds >= MAX_RUN_SECS && long.processes.is_some());
    }

    #[test]
    fn workspace_paths_accept_linux_names_but_not_escapes() {
        for path in [
            "a b/c@1+2.txt",
            ".git/HEAD",
            "node_modules/@scope/pkg",
            "é/ü",
            "notes (copy).md",
            "backup.txt~",
            "CONFIG/console.log",
        ] {
            assert!(valid_path(path), "{path}");
            assert!(path.split('/').all(windows_name), "{path}");
        }
        // Names Windows would read as something else: kept apart on Linux only.
        for path in [
            "a:b",
            "a\\b",
            "x/what?",
            "a*",
            "tab\there",
            "trailing.",
            "trailing ",
            "CON",
            "nul.txt",
            "Aux .tar.gz",
            "com1",
            "LPT9.log",
            "COM¹",
            "conout$",
            "PROGRA~1",
        ] {
            assert!(!path.split('/').all(windows_name), "{path:?}");
            assert_eq!(valid_path(path), !cfg!(windows), "{path:?}");
        }
        let long_name = "x".repeat(MAX_NAME + 1);
        let long_path = format!("{}/x", "d/".repeat(MAX_PATH / 2));
        for path in [
            "", "/abs", "a//b", "./a", "a/..", "a\0b", &long_name, &long_path,
        ] {
            assert!(!valid_path(path), "{path:?}");
        }
    }

    #[test]
    fn input_trees_refuse_entries_below_files_links_or_directories() {
        let limits = TreeLimits::default();
        let file = Input::File {
            size: 1,
            executable: false,
        };
        let tree = |entries: &[(&str, Input)]| -> BTreeMap<String, Input> {
            entries
                .iter()
                .map(|(path, input)| (path.to_string(), input.clone()))
                .collect()
        };
        assert_eq!(
            check_input(
                &tree(&[
                    ("a/b", file.clone()),
                    ("a/link", Input::Link("../b".into())),
                    ("empty", Input::Directory),
                ]),
                &limits
            )
            .unwrap(),
            1
        );
        for bad in [
            tree(&[("a", file.clone()), ("a/b", file.clone())]),
            tree(&[("a", Input::Link("x".into())), ("a/b", file.clone())]),
            tree(&[("a", Input::Directory), ("a/b", file.clone())]),
            tree(&[("a", Input::Link(String::new()))]),
            tree(&[(
                "big",
                Input::File {
                    size: limits.file + 1,
                    executable: false,
                },
            )]),
        ] {
            assert!(check_input(&bad, &limits).is_err(), "{bad:?}");
        }
    }
}
