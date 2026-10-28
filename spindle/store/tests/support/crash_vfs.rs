//! Power-loss and disk-full simulation for the store's SQLite files, in the style of
//! SQLite's own crash tests (test6.c), plus an `fsync` observer for the files Rust code
//! writes beside them. Linux only; test support, never linked into Spindle itself.
//!
//! `install` registers a VFS that wraps the platform default ("unix") as the process
//! default, so `Store::open` and every other connection use it without hooks. Files
//! outside a registered `Sim` root pass straight through to the real VFS. Inside a root:
//!
//! * Events are writes, truncations and syncs of SQLite's files, and `fsync`/`fdatasync`
//!   calls Rust code makes on files or directories under the root (observed by
//!   interposing libc's `fsync`/`fdatasync` in this test binary and forwarding to the
//!   real system calls). `Plan::CrashAt(n)` cuts power at event `n`: that event never
//!   happens and every later I/O of the root fails, as if the process had died.
//! * File contents: a file's content is durable as of its last sync. Writes and
//!   truncations since then are pending; a crash image keeps each one, drops it, or
//!   tears it, keeping an arbitrary subset of its 512-byte sectors (never touching bytes
//!   outside the write, which is the powersafe-overwrite property SQLite's unix VFS
//!   declares). Files Rust code writes are not intercepted at write level: their content
//!   is "as last fsynced" (empty if never) or "as last seen", or a sector mix of the two.
//! * Names: a directory's entries (creations, deletions, renames) are durable as of the
//!   last fsync of that directory, which covers SQLite's own directory syncs (WAL and
//!   journal creation) and Spindle's `sync_dir`. A crash image takes, per directory,
//!   either that durable listing or the listing at the crash (not every intermediate
//!   one). Files and directories are identified by device, inode and birth time, so a
//!   rename keeps a file's content and a directory's flushed entries.
//! * `Plan::Full` fails SQLite writes with `SQLITE_FULL` (or `SQLITE_IOERR_WRITE`) once a
//!   byte budget is spent, without writing anything, until the plan changes.
//!
//! Not modelled: the shared-memory WAL index beyond keeping or dropping the -shm file
//! (SQLite rebuilds it after a crash), reordering across files beyond what syncs allow,
//! writes that land outside their own range, and drives that acknowledge a flush they did
//! not make (`Sim::lie` simulates one, as a negative control).
#![allow(dead_code)]

use rusqlite::ffi;
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, OsString, c_char, c_int, c_void};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// Granularity at which an interrupted write may land partly.
pub const SECTOR: u64 = 512;

/// A file or directory, stable across renames.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FileId {
    dev: u64,
    ino: u64,
    born: u64,
}

fn file_id(meta: &std::fs::Metadata) -> FileId {
    let born = meta
        .created()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |time| time.as_nanos() as u64);
    FileId {
        dev: meta.dev(),
        ino: meta.ino(),
        born,
    }
}

#[derive(Clone, Debug)]
enum Op {
    Write(u64, Vec<u8>),
    Truncate(u64),
}

fn apply(content: &mut Vec<u8>, op: &Op) {
    match op {
        Op::Write(offset, data) => {
            let offset = *offset as usize;
            if content.len() < offset + data.len() {
                content.resize(offset + data.len(), 0);
            }
            content[offset..offset + data.len()].copy_from_slice(data);
        }
        Op::Truncate(size) => content.resize(*size as usize, 0),
    }
}

/// A SQLite file's content: durable as of its last sync, plus what was done since.
#[derive(Clone, Debug, Default)]
struct Node {
    durable: Vec<u8>,
    pending: Vec<Op>,
}

impl Node {
    fn sync(&mut self) {
        for op in std::mem::take(&mut self.pending) {
            apply(&mut self.durable, &op);
        }
    }
}

/// Directory entries by name.
type Listing = BTreeMap<OsString, (FileId, bool)>;

fn listing(dir: &Path) -> Listing {
    let mut out = Listing::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        if let Ok(meta) = std::fs::symlink_metadata(entry.path())
            && (meta.is_dir() || meta.is_file())
        {
            out.insert(entry.file_name(), (file_id(&meta), meta.is_dir()));
        }
    }
    out
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Plan {
    /// Count events; nothing fails.
    Run,
    /// Cut power at event `n` (1-based).
    CrashAt(u64),
    /// SQLite writes fail with `code` once `budget` more bytes would have been written.
    Full { budget: u64, code: c_int },
}

struct State {
    plan: Plan,
    events: u64,
    written: u64,
    faults: u64,
    dead: bool,
    /// Syncs succeed without making anything durable: a drive that lies about flushes.
    lying: bool,
    nodes: HashMap<FileId, Node>,
    /// Content of files Rust code wrote, as of their last fsync.
    synced: HashMap<FileId, Vec<u8>>,
    /// Last seen content of files Rust code wrote (at a directory sync or the crash).
    seen: HashMap<FileId, Vec<u8>>,
    /// Listing of each directory as of its last fsync (a rename keeps its entries).
    durable: HashMap<FileId, Listing>,
    trace: Option<Vec<String>>,
    /// Bytes written before each write event (for disk-full budgets).
    offsets: Vec<u64>,
    snapshot: Option<Snapshot>,
}

/// One simulated machine: everything under `root`.
pub struct Sim {
    root: PathBuf,
    root_id: FileId,
    state: Mutex<State>,
}

static SIMS: Mutex<Vec<Arc<Sim>>> = Mutex::new(Vec::new());
static SERIAL: AtomicU64 = AtomicU64::new(0);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn sim_for(path: &Path) -> Option<Arc<Sim>> {
    lock(&SIMS)
        .iter()
        .find(|sim| path.starts_with(&sim.root))
        .cloned()
}

/// A fresh private directory under the temp root, never tracked.
pub fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "spindle-power-{}-{}-{label}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&dir)
        .unwrap();
    dir.canonicalize().unwrap()
}

/// Keeps a `Sim` registered; unregisters it and removes its root when dropped.
pub struct SimGuard(pub Arc<Sim>);

impl std::ops::Deref for SimGuard {
    type Target = Sim;
    fn deref(&self) -> &Sim {
        &self.0
    }
}

impl Drop for SimGuard {
    fn drop(&mut self) {
        lock(&SIMS).retain(|sim| !Arc::ptr_eq(sim, &self.0));
        let _ = std::fs::remove_dir_all(&self.0.root);
    }
}

impl Sim {
    /// A new simulated root (empty unless `seed` fills it first; whatever is there when
    /// tracking starts counts as durable).
    pub fn start(label: &str, plan: Plan, seed: impl FnOnce(&Path)) -> SimGuard {
        install();
        let root = scratch(label);
        seed(&root);
        let mut state = State {
            plan,
            events: 0,
            written: 0,
            faults: 0,
            dead: false,
            lying: false,
            nodes: HashMap::new(),
            synced: HashMap::new(),
            seen: HashMap::new(),
            durable: HashMap::new(),
            trace: None,
            offsets: vec![],
            snapshot: None,
        };
        let mut dirs = vec![root.clone()];
        while let Some(dir) = dirs.pop() {
            let entries = listing(&dir);
            for (name, (id, is_dir)) in &entries {
                if *is_dir {
                    dirs.push(dir.join(name));
                } else {
                    state
                        .synced
                        .insert(*id, std::fs::read(dir.join(name)).unwrap());
                }
            }
            let id = file_id(&std::fs::metadata(&dir).unwrap());
            state.durable.insert(id, entries);
        }
        let root_id = file_id(&std::fs::metadata(&root).unwrap());
        let sim = Arc::new(Sim {
            root,
            root_id,
            state: Mutex::new(state),
        });
        lock(&SIMS).push(sim.clone());
        SimGuard(sim)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn set_plan(&self, plan: Plan) {
        lock(&self.state).plan = plan;
    }
    pub fn events(&self) -> u64 {
        lock(&self.state).events
    }
    pub fn faults(&self) -> u64 {
        lock(&self.state).faults
    }
    pub fn dead(&self) -> bool {
        lock(&self.state).dead
    }
    /// From now on syncs report success but make nothing durable (a negative control).
    pub fn lie(&self) {
        lock(&self.state).lying = true;
    }
    /// Record a description of every event (for naming crash points).
    pub fn trace(&self) {
        lock(&self.state).trace = Some(vec![]);
    }
    /// Bytes SQLite had written to this root before each of its writes, in order: a
    /// `Plan::Full` budget equal to one of them fails exactly that write.
    pub fn write_offsets(&self) -> Vec<u64> {
        lock(&self.state).offsets.clone()
    }
    pub fn written(&self) -> u64 {
        lock(&self.state).written
    }
    pub fn traced(&self) -> Vec<String> {
        lock(&self.state).trace.clone().unwrap_or_default()
    }
    /// The machine as it was when power was cut, if it was.
    pub fn snapshot(&self) -> Option<Snapshot> {
        lock(&self.state).snapshot.clone()
    }

    fn rel(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.root)
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }

    /// Counts one event; Err (with nothing done) if the machine is or just went down.
    fn event(&self, state: &mut State, what: impl FnOnce() -> String) -> Result<(), ()> {
        if state.dead {
            return Err(());
        }
        state.events += 1;
        if let Some(trace) = &mut state.trace {
            trace.push(what());
        }
        if state.plan == Plan::CrashAt(state.events) {
            state.snapshot = Some(self.capture(state));
            state.dead = true;
            return Err(());
        }
        Ok(())
    }

    fn capture(&self, state: &mut State) -> Snapshot {
        let mut current = HashMap::new();
        let mut dirs = vec![self.root.clone()];
        while let Some(dir) = dirs.pop() {
            let entries = listing(&dir);
            for (name, (id, is_dir)) in &entries {
                if *is_dir {
                    dirs.push(dir.join(name));
                } else if !state.nodes.contains_key(id)
                    && let Ok(bytes) = std::fs::read(dir.join(name))
                {
                    state.seen.insert(*id, bytes);
                }
            }
            if let Ok(meta) = std::fs::metadata(&dir) {
                current.insert(file_id(&meta), entries);
            }
        }
        let mut files = HashMap::new();
        for (id, bytes) in &state.seen {
            files.insert(*id, (state.synced.get(id).cloned(), bytes.clone()));
        }
        for (id, bytes) in &state.synced {
            files
                .entry(*id)
                .or_insert_with(|| (Some(bytes.clone()), bytes.clone()));
        }
        Snapshot {
            event: state.events,
            root: self.root_id,
            durable: state.durable.clone(),
            current,
            nodes: state.nodes.clone(),
            files,
        }
    }

    fn track(&self, id: FileId, path: &Path) {
        let mut state = lock(&self.state);
        if !state.nodes.contains_key(&id) {
            // A file Rust code created: durable as of its last fsync, else as it is.
            let durable = state
                .synced
                .get(&id)
                .cloned()
                .unwrap_or_else(|| std::fs::read(path).unwrap_or_default());
            state.nodes.insert(
                id,
                Node {
                    durable,
                    pending: vec![],
                },
            );
        }
    }

    /// An fsync/fdatasync made outside SQLite's own sync (or SQLite's directory sync).
    fn observe_fsync(&self, fd: c_int, path: &Path, inside_sqlite: bool) -> Result<(), ()> {
        let mut state = lock(&self.state);
        if !inside_sqlite {
            let rel = self.rel(path);
            self.event(&mut state, || format!("fsync {}", rel.display()))?;
        } else if state.dead {
            return Err(());
        }
        if state.lying {
            return Ok(());
        }
        // Through the descriptor: the name may have been removed since it was opened.
        let Ok(meta) = std::fs::metadata(format!("/proc/self/fd/{fd}")) else {
            return Ok(());
        };
        if meta.is_dir() {
            let entries = listing(path);
            for (name, (id, is_dir)) in &entries {
                if !is_dir
                    && !state.nodes.contains_key(id)
                    && let Ok(bytes) = std::fs::read(path.join(name))
                {
                    state.seen.insert(*id, bytes);
                }
            }
            state.durable.insert(file_id(&meta), entries);
        } else {
            let id = file_id(&meta);
            if let Some(node) = state.nodes.get_mut(&id) {
                node.sync();
            } else if let Ok(bytes) = std::fs::read(format!("/proc/self/fd/{fd}")) {
                state.seen.insert(id, bytes.clone());
                state.synced.insert(id, bytes);
            }
        }
        Ok(())
    }
}

/// Which surviving image to build from a snapshot.
#[derive(Clone, Copy, Debug)]
pub enum Image {
    /// Every unsynced write and name change reached the disk (a process kill).
    Kept,
    /// None did: only what was synced survives.
    Lost,
    /// Each name change per directory, and each unsynced write, independently kept,
    /// dropped or (writes) torn at sector granularity; reproducible from the seed.
    Random(u64),
}

/// The machine at the instant power was cut.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub event: u64,
    root: FileId,
    durable: HashMap<FileId, Listing>,
    current: HashMap<FileId, Listing>,
    nodes: HashMap<FileId, Node>,
    /// Files Rust code wrote: (content at last fsync, last seen content).
    files: HashMap<FileId, (Option<Vec<u8>>, Vec<u8>)>,
}

/// splitmix64: small, seedable, good enough to pick crash points and fates.
pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e37_79b9_7f4a_7c15)
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// The pieces of a write, split at sector boundaries.
fn sectors(offset: u64, data: &[u8]) -> Vec<(u64, &[u8])> {
    let mut out = vec![];
    let mut start = offset;
    let end = offset + data.len() as u64;
    while start < end {
        let next = ((start / SECTOR + 1) * SECTOR).min(end);
        out.push((
            start,
            &data[(start - offset) as usize..(next - offset) as usize],
        ));
        start = next;
    }
    out
}

impl Snapshot {
    /// Writes the surviving image of the simulated root into `out` (created, 0700).
    pub fn write_image(&self, image: Image, out: &Path) {
        let mut rng = Rng::new(match image {
            Image::Random(seed) => seed,
            _ => 0,
        });
        self.write_dir(self.root, out, image, &mut rng);
    }

    fn write_dir(&self, dir: FileId, out: &Path, image: Image, rng: &mut Rng) {
        std::fs::DirBuilder::new().mode(0o700).create(out).unwrap();
        let durable = self.durable.get(&dir).cloned().unwrap_or_default();
        let current = self.current.get(&dir).cloned().unwrap_or_default();
        let entries = match image {
            Image::Kept => current,
            Image::Lost => durable,
            Image::Random(_) if durable == current || rng.below(2) == 0 => current,
            Image::Random(_) => durable,
        };
        for (name, (id, is_dir)) in entries {
            if is_dir {
                self.write_dir(id, &out.join(&name), image, rng);
            } else {
                let bytes = self.content(id, image, rng);
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(out.join(&name))
                    .and_then(|mut file| std::io::Write::write_all(&mut file, &bytes))
                    .unwrap();
            }
        }
    }

    fn content(&self, id: FileId, image: Image, rng: &mut Rng) -> Vec<u8> {
        if let Some(node) = self.nodes.get(&id) {
            let mut out = node.durable.clone();
            for op in &node.pending {
                match (image, rng.below(3)) {
                    (Image::Lost, _) => {}
                    (Image::Kept, _) | (Image::Random(_), 0) => apply(&mut out, op),
                    (_, 1) => {}
                    (_, _) => match op {
                        Op::Write(offset, data) => {
                            for (at, piece) in sectors(*offset, data) {
                                if rng.below(2) == 0 {
                                    apply(&mut out, &Op::Write(at, piece.to_vec()));
                                }
                            }
                        }
                        truncate => apply(&mut out, truncate),
                    },
                }
            }
            return out;
        }
        let Some((synced, seen)) = self.files.get(&id) else {
            return vec![];
        };
        let synced = synced.clone().unwrap_or_default();
        match (image, rng.below(3)) {
            (Image::Kept, _) => seen.clone(),
            (Image::Lost, _) | (Image::Random(_), 0) => synced,
            (_, 1) => seen.clone(),
            (_, _) => {
                let mut out = synced;
                for (at, piece) in sectors(0, seen) {
                    if rng.below(2) == 0 {
                        apply(&mut out, &Op::Write(at, piece.to_vec()));
                    }
                }
                out
            }
        }
    }
}

// ---- the VFS ------------------------------------------------------------------------

static REAL: AtomicPtr<ffi::sqlite3_vfs> = AtomicPtr::new(std::ptr::null_mut());

thread_local! {
    /// Set while SQLite's own xSync runs, so its fsync calls are not separate events.
    static IN_SYNC: Cell<bool> = const { Cell::new(false) };
}

#[repr(C)]
struct CrashFile {
    base: ffi::sqlite3_file,
    sim: *const Sim,
    id: FileId,
    real: *mut ffi::sqlite3_file,
}

fn real_vfs() -> *mut ffi::sqlite3_vfs {
    REAL.load(Ordering::Acquire)
}

/// Registers the crash VFS as the process default (once).
pub fn install() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| unsafe {
        assert_eq!(ffi::sqlite3_initialize(), ffi::SQLITE_OK);
        let real = ffi::sqlite3_vfs_find(std::ptr::null());
        assert!(!real.is_null());
        REAL.store(real, Ordering::Release);
        let mut vfs: ffi::sqlite3_vfs = std::ptr::read(real);
        vfs.iVersion = 3;
        vfs.szOsFile = (std::mem::size_of::<CrashFile>() as c_int) + (*real).szOsFile;
        vfs.pNext = std::ptr::null_mut();
        vfs.zName = c"spindle-crash".as_ptr();
        vfs.pAppData = std::ptr::null_mut();
        vfs.xOpen = Some(x_open);
        vfs.xDelete = Some(x_delete);
        vfs.xAccess = Some(x_access);
        vfs.xFullPathname = Some(x_full_pathname);
        vfs.xDlOpen = None;
        vfs.xDlError = None;
        vfs.xDlSym = None;
        vfs.xDlClose = None;
        vfs.xRandomness = Some(x_randomness);
        vfs.xSleep = Some(x_sleep);
        vfs.xCurrentTime = Some(x_current_time);
        vfs.xGetLastError = Some(x_get_last_error);
        vfs.xCurrentTimeInt64 = Some(x_current_time_int64);
        vfs.xSetSystemCall = None;
        vfs.xGetSystemCall = None;
        vfs.xNextSystemCall = None;
        let vfs = Box::leak(Box::new(vfs));
        assert_eq!(ffi::sqlite3_vfs_register(vfs, 1), ffi::SQLITE_OK);
    });
}

unsafe fn path_of(name: *const c_char) -> Option<PathBuf> {
    if name.is_null() {
        return None;
    }
    let text = unsafe { CStr::from_ptr(name) }.to_str().ok()?;
    Some(PathBuf::from(text))
}

static METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 2,
    xClose: Some(f_close),
    xRead: Some(f_read),
    xWrite: Some(f_write),
    xTruncate: Some(f_truncate),
    xSync: Some(f_sync),
    xFileSize: Some(f_file_size),
    xLock: Some(f_lock),
    xUnlock: Some(f_unlock),
    xCheckReservedLock: Some(f_check_reserved_lock),
    xFileControl: Some(f_file_control),
    xSectorSize: Some(f_sector_size),
    xDeviceCharacteristics: Some(f_device_characteristics),
    xShmMap: Some(f_shm_map),
    xShmLock: Some(f_shm_lock),
    xShmBarrier: Some(f_shm_barrier),
    xShmUnmap: Some(f_shm_unmap),
    xFetch: None,
    xUnfetch: None,
};

unsafe extern "C" fn x_open(
    _vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    unsafe {
        let real = real_vfs();
        let crash = file as *mut CrashFile;
        let inner =
            (file as *mut u8).add(std::mem::size_of::<CrashFile>()) as *mut ffi::sqlite3_file;
        (*crash).base.pMethods = std::ptr::null();
        (*crash).sim = std::ptr::null();
        (*crash).real = inner;
        let path = path_of(name);
        let sim = path.as_deref().and_then(sim_for);
        if sim.as_ref().is_some_and(|sim| sim.dead()) {
            return ffi::SQLITE_CANTOPEN;
        }
        let rc = ((*real).xOpen.unwrap())(real, name, inner, flags, out_flags);
        if rc != ffi::SQLITE_OK {
            return rc;
        }
        if let (Some(sim), Some(path)) = (sim, path)
            && let Ok(meta) = std::fs::metadata(&path)
        {
            let id = file_id(&meta);
            sim.track(id, &path);
            (*crash).id = id;
            (*crash).sim = Arc::into_raw(sim);
        }
        (*crash).base.pMethods = &METHODS;
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn x_delete(
    _vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    sync_dir: c_int,
) -> c_int {
    unsafe {
        if let Some(sim) = path_of(name).as_deref().and_then(sim_for)
            && sim.dead()
        {
            return ffi::SQLITE_IOERR_DELETE;
        }
        let real = real_vfs();
        ((*real).xDelete.unwrap())(real, name, sync_dir)
    }
}

unsafe extern "C" fn x_access(
    _vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    unsafe {
        let real = real_vfs();
        ((*real).xAccess.unwrap())(real, name, flags, out)
    }
}

unsafe extern "C" fn x_full_pathname(
    _vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    size: c_int,
    out: *mut c_char,
) -> c_int {
    unsafe {
        let real = real_vfs();
        ((*real).xFullPathname.unwrap())(real, name, size, out)
    }
}

unsafe extern "C" fn x_randomness(
    _vfs: *mut ffi::sqlite3_vfs,
    size: c_int,
    out: *mut c_char,
) -> c_int {
    unsafe {
        let real = real_vfs();
        ((*real).xRandomness.unwrap())(real, size, out)
    }
}

unsafe extern "C" fn x_sleep(_vfs: *mut ffi::sqlite3_vfs, micros: c_int) -> c_int {
    unsafe {
        let real = real_vfs();
        ((*real).xSleep.unwrap())(real, micros)
    }
}

unsafe extern "C" fn x_current_time(_vfs: *mut ffi::sqlite3_vfs, out: *mut f64) -> c_int {
    unsafe {
        let real = real_vfs();
        ((*real).xCurrentTime.unwrap())(real, out)
    }
}

unsafe extern "C" fn x_get_last_error(
    _vfs: *mut ffi::sqlite3_vfs,
    size: c_int,
    out: *mut c_char,
) -> c_int {
    unsafe {
        let real = real_vfs();
        ((*real).xGetLastError.unwrap())(real, size, out)
    }
}

unsafe extern "C" fn x_current_time_int64(
    _vfs: *mut ffi::sqlite3_vfs,
    out: *mut ffi::sqlite3_int64,
) -> c_int {
    unsafe {
        let real = real_vfs();
        ((*real).xCurrentTimeInt64.unwrap())(real, out)
    }
}

unsafe fn parts(
    file: *mut ffi::sqlite3_file,
) -> (*mut ffi::sqlite3_file, Option<&'static Sim>, FileId) {
    unsafe {
        let crash = file as *mut CrashFile;
        let sim = (*crash).sim;
        ((*crash).real, (!sim.is_null()).then(|| &*sim), (*crash).id)
    }
}

unsafe fn methods(real: *mut ffi::sqlite3_file) -> &'static ffi::sqlite3_io_methods {
    unsafe { &*(*real).pMethods }
}

unsafe extern "C" fn f_close(file: *mut ffi::sqlite3_file) -> c_int {
    unsafe {
        let crash = file as *mut CrashFile;
        let real = (*crash).real;
        let rc = (methods(real).xClose.unwrap())(real);
        if !(*crash).sim.is_null() {
            drop(Arc::from_raw((*crash).sim));
            (*crash).sim = std::ptr::null();
        }
        rc
    }
}

unsafe extern "C" fn f_read(
    file: *mut ffi::sqlite3_file,
    buf: *mut c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    unsafe {
        let (real, sim, _) = parts(file);
        if sim.is_some_and(Sim::dead) {
            return ffi::SQLITE_IOERR_READ;
        }
        (methods(real).xRead.unwrap())(real, buf, amount, offset)
    }
}

unsafe extern "C" fn f_write(
    file: *mut ffi::sqlite3_file,
    buf: *const c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    unsafe {
        let (real, sim, id) = parts(file);
        let Some(sim) = sim else {
            return (methods(real).xWrite.unwrap())(real, buf, amount, offset);
        };
        {
            let mut state = lock(&sim.state);
            let what = || {
                format!(
                    "write {amount} bytes at {offset} to {}",
                    name_of(&sim.root, id)
                )
            };
            if sim.event(&mut state, what).is_err() {
                return ffi::SQLITE_IOERR_WRITE;
            }
            let before = state.written;
            state.offsets.push(before);
            if let Plan::Full { budget, code } = state.plan
                && state.written + amount as u64 > budget
            {
                state.faults += 1;
                return code;
            }
            state.written += amount as u64;
        }
        let rc = (methods(real).xWrite.unwrap())(real, buf, amount, offset);
        if rc == ffi::SQLITE_OK {
            let data = std::slice::from_raw_parts(buf as *const u8, amount as usize).to_vec();
            if let Some(node) = lock(&sim.state).nodes.get_mut(&id) {
                node.pending.push(Op::Write(offset as u64, data));
            }
        }
        rc
    }
}

/// The current name of a file under `root`, for event descriptions (only when tracing).
fn name_of(root: &Path, id: FileId) -> String {
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for (name, (entry, is_dir)) in listing(&dir) {
            if is_dir {
                dirs.push(dir.join(&name));
            } else if entry == id {
                return dir
                    .join(name)
                    .strip_prefix(root)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
            }
        }
    }
    format!("inode {}", id.ino)
}

unsafe extern "C" fn f_truncate(file: *mut ffi::sqlite3_file, size: ffi::sqlite3_int64) -> c_int {
    unsafe {
        let (real, sim, id) = parts(file);
        let Some(sim) = sim else {
            return (methods(real).xTruncate.unwrap())(real, size);
        };
        {
            let mut state = lock(&sim.state);
            let what = || format!("truncate {} to {size}", name_of(&sim.root, id));
            if sim.event(&mut state, what).is_err() {
                return ffi::SQLITE_IOERR_TRUNCATE;
            }
        }
        let rc = (methods(real).xTruncate.unwrap())(real, size);
        if rc == ffi::SQLITE_OK
            && let Some(node) = lock(&sim.state).nodes.get_mut(&id)
        {
            node.pending.push(Op::Truncate(size as u64));
        }
        rc
    }
}

unsafe extern "C" fn f_sync(file: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    unsafe {
        let (real, sim, id) = parts(file);
        let Some(sim) = sim else {
            return (methods(real).xSync.unwrap())(real, flags);
        };
        {
            let mut state = lock(&sim.state);
            let what = || format!("sync {}", name_of(&sim.root, id));
            if sim.event(&mut state, what).is_err() {
                return ffi::SQLITE_IOERR_FSYNC;
            }
        }
        IN_SYNC.with(|inside| inside.set(true));
        let rc = (methods(real).xSync.unwrap())(real, flags);
        IN_SYNC.with(|inside| inside.set(false));
        let mut state = lock(&sim.state);
        if rc == ffi::SQLITE_OK
            && !state.lying
            && let Some(node) = state.nodes.get_mut(&id)
        {
            node.sync();
        }
        rc
    }
}

unsafe extern "C" fn f_file_size(
    file: *mut ffi::sqlite3_file,
    size: *mut ffi::sqlite3_int64,
) -> c_int {
    unsafe {
        let (real, sim, _) = parts(file);
        if sim.is_some_and(Sim::dead) {
            return ffi::SQLITE_IOERR_FSTAT;
        }
        (methods(real).xFileSize.unwrap())(real, size)
    }
}

unsafe extern "C" fn f_lock(file: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    unsafe {
        let (real, _, _) = parts(file);
        (methods(real).xLock.unwrap())(real, level)
    }
}

unsafe extern "C" fn f_unlock(file: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    unsafe {
        let (real, _, _) = parts(file);
        (methods(real).xUnlock.unwrap())(real, level)
    }
}

unsafe extern "C" fn f_check_reserved_lock(file: *mut ffi::sqlite3_file, out: *mut c_int) -> c_int {
    unsafe {
        let (real, _, _) = parts(file);
        (methods(real).xCheckReservedLock.unwrap())(real, out)
    }
}

unsafe extern "C" fn f_file_control(
    file: *mut ffi::sqlite3_file,
    op: c_int,
    arg: *mut c_void,
) -> c_int {
    unsafe {
        let (real, _, _) = parts(file);
        (methods(real).xFileControl.unwrap())(real, op, arg)
    }
}

unsafe extern "C" fn f_sector_size(file: *mut ffi::sqlite3_file) -> c_int {
    unsafe {
        let (real, _, _) = parts(file);
        (methods(real).xSectorSize.unwrap())(real)
    }
}

unsafe extern "C" fn f_device_characteristics(file: *mut ffi::sqlite3_file) -> c_int {
    unsafe {
        let (real, _, _) = parts(file);
        (methods(real).xDeviceCharacteristics.unwrap())(real)
    }
}

unsafe extern "C" fn f_shm_map(
    file: *mut ffi::sqlite3_file,
    page: c_int,
    size: c_int,
    extend: c_int,
    out: *mut *mut c_void,
) -> c_int {
    unsafe {
        let (real, _, _) = parts(file);
        (methods(real).xShmMap.unwrap())(real, page, size, extend, out)
    }
}

unsafe extern "C" fn f_shm_lock(
    file: *mut ffi::sqlite3_file,
    offset: c_int,
    n: c_int,
    flags: c_int,
) -> c_int {
    unsafe {
        let (real, _, _) = parts(file);
        (methods(real).xShmLock.unwrap())(real, offset, n, flags)
    }
}

unsafe extern "C" fn f_shm_barrier(file: *mut ffi::sqlite3_file) {
    unsafe {
        let (real, _, _) = parts(file);
        (methods(real).xShmBarrier.unwrap())(real)
    }
}

unsafe extern "C" fn f_shm_unmap(file: *mut ffi::sqlite3_file, delete: c_int) -> c_int {
    unsafe {
        let (real, _, _) = parts(file);
        (methods(real).xShmUnmap.unwrap())(real, delete)
    }
}

// ---- fsync observer -----------------------------------------------------------------

static SQLITE_FSYNCS: AtomicU64 = AtomicU64::new(0);

/// How many fsync calls SQLite's own sync code made through the interposed `fsync`
/// (proof that the C library's calls are observed, not only Rust's).
pub fn sqlite_fsyncs() -> u64 {
    SQLITE_FSYNCS.load(Ordering::Relaxed)
}

fn observe(fd: c_int) -> Result<(), ()> {
    if IN_SYNC.with(Cell::get) {
        SQLITE_FSYNCS.fetch_add(1, Ordering::Relaxed);
    }
    let Ok(path) = std::fs::read_link(format!("/proc/self/fd/{fd}")) else {
        return Ok(());
    };
    match sim_for(&path) {
        Some(sim) => sim.observe_fsync(fd, &path, IN_SYNC.with(Cell::get)),
        None => Ok(()),
    }
}

/// libc's `fsync`, interposed for this test binary: observed, then forwarded to the
/// system call. A dead machine fails it with EIO without syncing anything.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsync(fd: c_int) -> c_int {
    if observe(fd).is_err() {
        unsafe { *libc::__errno_location() = libc::EIO };
        return -1;
    }
    unsafe { libc::syscall(libc::SYS_fsync, fd) as c_int }
}

/// libc's `fdatasync`, interposed like `fsync`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fdatasync(fd: c_int) -> c_int {
    if observe(fd).is_err() {
        unsafe { *libc::__errno_location() = libc::EIO };
        return -1;
    }
    unsafe { libc::syscall(libc::SYS_fdatasync, fd) as c_int }
}
