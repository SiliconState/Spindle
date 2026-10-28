//! Durable storage for Node and Electron: a SQLite VFS over the synchronous `node:fs`
//! calls, and the lock file that fences a store's single owner. Browser workers use
//! the same VFS over an adapter with the same calls on OPFS sync access handles
//! (`clients/js/opfs.mjs`), owned by a Web Lock instead of a lock file.
//!
//! Every `xSync` is an `fsyncSync` of the file. Creating or deleting a file also
//! fsyncs its directory, so a new journal or database exists after a power loss
//! (Node has no directory fsync on Windows; there NTFS's metadata journal is trusted
//! instead). Byte-range locks are not taken: the store runs in exclusive locking mode
//! and its owner holds `<database>.lock`, created with `wx` (fails if it exists), so
//! one connection uses a database at a time. A lock left by a crashed owner is never
//! taken over automatically; opening says `Locked` until an operator removes it.
use rusqlite::{Connection, OpenFlags};
use spindle_store::{ConnectionProfile, Error, Initialization, JournalMode, OwnerExclusion, Store};
use sqlite_wasm_rs as ffi;
use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use wasm_bindgen::prelude::*;

/// The name the VFS is registered under.
pub const VFS: &str = "spindle-node";

#[wasm_bindgen]
extern "C" {
    /// The `node:fs` module, or any object with its synchronous methods (a test may
    /// wrap it to watch calls).
    pub type NodeFs;
    #[wasm_bindgen(method, catch, js_name = openSync)]
    fn open_sync(this: &NodeFs, path: &str, flags: &str, mode: u32) -> Result<i32, JsValue>;
    #[wasm_bindgen(method, catch, js_name = readSync)]
    fn read_sync(
        this: &NodeFs,
        fd: i32,
        buffer: &mut [u8],
        offset: u32,
        length: u32,
        position: f64,
    ) -> Result<u32, JsValue>;
    #[wasm_bindgen(method, catch, js_name = writeSync)]
    fn write_sync(
        this: &NodeFs,
        fd: i32,
        buffer: &[u8],
        offset: u32,
        length: u32,
        position: f64,
    ) -> Result<u32, JsValue>;
    #[wasm_bindgen(method, catch, js_name = fsyncSync)]
    fn fsync_sync(this: &NodeFs, fd: i32) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = ftruncateSync)]
    fn ftruncate_sync(this: &NodeFs, fd: i32, length: f64) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = fstatSync)]
    fn fstat_sync(this: &NodeFs, fd: i32) -> Result<Stats, JsValue>;
    #[wasm_bindgen(method, catch, js_name = closeSync)]
    fn close_sync(this: &NodeFs, fd: i32) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = unlinkSync)]
    fn unlink_sync(this: &NodeFs, path: &str) -> Result<(), JsValue>;
    #[wasm_bindgen(method, js_name = existsSync)]
    fn exists_sync(this: &NodeFs, path: &str) -> bool;
    #[wasm_bindgen(method, catch, js_name = readFileSync)]
    fn read_file_sync(this: &NodeFs, path: &str, encoding: &str) -> Result<String, JsValue>;
    #[wasm_bindgen(method, catch, js_name = mkdirSync)]
    fn mkdir_sync(this: &NodeFs, path: &str, options: &JsValue) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(method, catch, js_name = renameSync)]
    fn rename_sync(this: &NodeFs, from: &str, to: &str) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = readdirSync)]
    fn readdir_sync(this: &NodeFs, path: &str) -> Result<Vec<String>, JsValue>;

    type Stats;
    #[wasm_bindgen(method, getter)]
    fn size(this: &Stats) -> f64;
}

struct Host {
    fs: NodeFs,
    windows: bool,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
    /// What the last failed file operation said, for the error the caller sees.
    static LAST: RefCell<Option<String>> = const { RefCell::new(None) };
    /// Files open through the VFS: their descriptors belong to the installed `fs`.
    static OPEN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Installs `fs` for the VFS and registers it (once per process). A later call may
/// give another `fs` only while no file is open through the VFS: open descriptors
/// belong to the one they came from (an OPFS pool's slots, say).
pub fn install(fs: NodeFs, platform: &str) -> Result<(), Error> {
    HOST.with(|host| {
        let mut host = host.borrow_mut();
        if let Some(installed) = host.as_ref()
            && OPEN.get() > 0
            && !js_sys::Object::is(installed.fs.as_ref(), fs.as_ref())
        {
            LAST.with(|last| {
                *last.borrow_mut() = Some(
                    "another fs is installed and files are open through it; close those \
                     sessions first"
                        .into(),
                )
            });
            return Err(Error::Conflict);
        }
        *host = Some(Host {
            fs,
            windows: platform == "win32",
        });
        Ok(())
    })?;
    register()
}

fn with_fs<T>(run: impl FnOnce(&NodeFs, bool) -> Result<T, JsValue>) -> Result<T, JsValue> {
    HOST.with(|host| match host.borrow().as_ref() {
        Some(host) => run(&host.fs, host.windows),
        None => Err(JsValue::from_str(
            "the Node storage is not installed (call installNodeFs first)",
        )),
    })
}

fn property(error: &JsValue, name: &str) -> Option<String> {
    js_sys::Reflect::get(error, &JsValue::from_str(name))
        .ok()
        .and_then(|value| value.as_string())
}

/// `error`'s Node code (`EEXIST`, `ENOSPC`, ...), if it has one.
fn code(error: &JsValue) -> String {
    property(error, "code").unwrap_or_default()
}

fn describe(error: &JsValue) -> String {
    property(error, "message")
        .or_else(|| error.as_string())
        .unwrap_or_else(|| "a file operation failed".into())
}

/// Records `error` and returns the SQLite code for it (`fallback` unless the disk is full).
fn failed(error: JsValue, fallback: c_int) -> c_int {
    let full = matches!(code(&error).as_str(), "ENOSPC" | "EDQUOT");
    LAST.with(|last| *last.borrow_mut() = Some(describe(&error)));
    if full { ffi::SQLITE_FULL } else { fallback }
}

/// What the last failed file operation said, taken once.
pub fn take_last_error() -> Option<String> {
    LAST.with(|last| last.borrow_mut().take())
}

fn parent(path: &str) -> &str {
    match path.rfind(['/', '\\']) {
        Some(0) => "/",
        Some(at) => &path[..at],
        None => ".",
    }
}

/// Makes a created or removed directory entry under `path`'s directory durable.
fn sync_dir(path: &str) -> Result<(), JsValue> {
    with_fs(|fs, windows| {
        if windows {
            return Ok(());
        }
        let fd = fs.open_sync(parent(path), "r", 0)?;
        let synced = fs.fsync_sync(fd);
        let closed = fs.close_sync(fd);
        synced.and(closed)
    })
}

fn absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with('/')
        || path.starts_with("\\\\")
        || (bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

/// One open file. SQLite allocates `szOsFile` bytes for it; `base` must come first.
#[repr(C)]
struct File {
    base: ffi::sqlite3_file,
    fd: i32,
    /// The path (a leaked `CString`), kept to delete a delete-on-close file.
    path: *mut c_char,
    delete_on_close: bool,
    lock: c_int,
}

unsafe fn file<'a>(raw: *mut ffi::sqlite3_file) -> &'a mut File {
    unsafe { &mut *raw.cast::<File>() }
}

unsafe extern "C" fn x_open(
    _vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    raw: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    let handle = unsafe { file(raw) };
    handle.base.pMethods = std::ptr::null();
    if name.is_null() {
        LAST.with(|last| {
            *last.borrow_mut() =
                Some("temporary files are not supported; temp_store stays in memory".into())
        });
        return ffi::SQLITE_CANTOPEN;
    }
    let Ok(path) = unsafe { CStr::from_ptr(name) }.to_str() else {
        return ffi::SQLITE_CANTOPEN;
    };
    let readonly = flags & ffi::SQLITE_OPEN_READONLY != 0;
    let create = flags & ffi::SQLITE_OPEN_CREATE != 0;
    let exclusive = flags & ffi::SQLITE_OPEN_EXCLUSIVE != 0;
    let opened = with_fs(|fs, _| {
        if fs.exists_sync(path) && !exclusive {
            return fs.open_sync(path, if readonly { "r" } else { "r+" }, 0);
        }
        if !create {
            return Err(JsValue::from_str(&format!("{path} does not exist")));
        }
        let fd = fs.open_sync(path, "wx+", 0o600)?;
        if let Err(error) = sync_dir(path) {
            let _ = fs.close_sync(fd);
            return Err(error);
        }
        Ok(fd)
    });
    let fd = match opened {
        Ok(fd) => fd,
        Err(error) => return failed(error, ffi::SQLITE_CANTOPEN),
    };
    OPEN.set(OPEN.get() + 1);
    handle.fd = fd;
    handle.path = CString::new(path).map_or(std::ptr::null_mut(), CString::into_raw);
    handle.delete_on_close = flags & ffi::SQLITE_OPEN_DELETEONCLOSE != 0;
    handle.lock = ffi::SQLITE_LOCK_NONE;
    handle.base.pMethods = &METHODS;
    if !out_flags.is_null() {
        unsafe { *out_flags = flags };
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_close(raw: *mut ffi::sqlite3_file) -> c_int {
    let handle = unsafe { file(raw) };
    OPEN.set(OPEN.get().saturating_sub(1));
    let mut result = match with_fs(|fs, _| fs.close_sync(handle.fd)) {
        Ok(()) => ffi::SQLITE_OK,
        Err(error) => failed(error, ffi::SQLITE_IOERR_CLOSE),
    };
    if !handle.path.is_null() {
        let path = unsafe { CString::from_raw(handle.path) };
        handle.path = std::ptr::null_mut();
        if handle.delete_on_close
            && let Ok(path) = path.to_str()
            && let Err(error) = with_fs(|fs, _| fs.unlink_sync(path)).and_then(|()| sync_dir(path))
        {
            result = failed(error, ffi::SQLITE_IOERR_DELETE);
        }
    }
    result
}

unsafe extern "C" fn x_read(
    raw: *mut ffi::sqlite3_file,
    buffer: *mut c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    let handle = unsafe { file(raw) };
    let buffer = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), amount as usize) };
    let mut done = 0usize;
    while done < buffer.len() {
        let rest = &mut buffer[done..];
        let length = rest.len() as u32;
        match with_fs(|fs, _| {
            fs.read_sync(handle.fd, rest, 0, length, (offset as f64) + done as f64)
        }) {
            Ok(0) => break,
            Ok(read) => done += read as usize,
            Err(error) => return failed(error, ffi::SQLITE_IOERR_READ),
        }
    }
    if done < buffer.len() {
        // SQLite requires the unread part zeroed.
        buffer[done..].fill(0);
        return ffi::SQLITE_IOERR_SHORT_READ;
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_write(
    raw: *mut ffi::sqlite3_file,
    buffer: *const c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    let handle = unsafe { file(raw) };
    let buffer = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), amount as usize) };
    let mut done = 0usize;
    while done < buffer.len() {
        let rest = &buffer[done..];
        match with_fs(|fs, _| {
            fs.write_sync(
                handle.fd,
                rest,
                0,
                rest.len() as u32,
                (offset as f64) + done as f64,
            )
        }) {
            Ok(0) => return ffi::SQLITE_IOERR_WRITE,
            Ok(written) => done += written as usize,
            Err(error) => return failed(error, ffi::SQLITE_IOERR_WRITE),
        }
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_truncate(raw: *mut ffi::sqlite3_file, size: ffi::sqlite3_int64) -> c_int {
    let handle = unsafe { file(raw) };
    match with_fs(|fs, _| fs.ftruncate_sync(handle.fd, size as f64)) {
        Ok(()) => ffi::SQLITE_OK,
        Err(error) => failed(error, ffi::SQLITE_IOERR_TRUNCATE),
    }
}

unsafe extern "C" fn x_sync(raw: *mut ffi::sqlite3_file, _flags: c_int) -> c_int {
    let handle = unsafe { file(raw) };
    match with_fs(|fs, _| fs.fsync_sync(handle.fd)) {
        Ok(()) => ffi::SQLITE_OK,
        Err(error) => failed(error, ffi::SQLITE_IOERR_FSYNC),
    }
}

unsafe extern "C" fn x_file_size(
    raw: *mut ffi::sqlite3_file,
    size: *mut ffi::sqlite3_int64,
) -> c_int {
    let handle = unsafe { file(raw) };
    match with_fs(|fs, _| fs.fstat_sync(handle.fd)) {
        Ok(stats) => {
            unsafe { *size = stats.size() as ffi::sqlite3_int64 };
            ffi::SQLITE_OK
        }
        Err(error) => failed(error, ffi::SQLITE_IOERR_FSTAT),
    }
}

/// The level is only noted: the owner's lock file is the exclusion.
unsafe extern "C" fn x_lock(raw: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    unsafe { file(raw) }.lock = level;
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_unlock(raw: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    unsafe { file(raw) }.lock = level;
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_check_reserved_lock(raw: *mut ffi::sqlite3_file, out: *mut c_int) -> c_int {
    unsafe { *out = c_int::from(file(raw).lock >= ffi::SQLITE_LOCK_RESERVED) };
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_file_control(
    _raw: *mut ffi::sqlite3_file,
    _op: c_int,
    _arg: *mut c_void,
) -> c_int {
    ffi::SQLITE_NOTFOUND
}

unsafe extern "C" fn x_sector_size(_raw: *mut ffi::sqlite3_file) -> c_int {
    4096
}

/// No atomic-write or power-safe-overwrite guarantees are claimed.
unsafe extern "C" fn x_device_characteristics(_raw: *mut ffi::sqlite3_file) -> c_int {
    0
}

static METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 1,
    xClose: Some(x_close),
    xRead: Some(x_read),
    xWrite: Some(x_write),
    xTruncate: Some(x_truncate),
    xSync: Some(x_sync),
    xFileSize: Some(x_file_size),
    xLock: Some(x_lock),
    xUnlock: Some(x_unlock),
    xCheckReservedLock: Some(x_check_reserved_lock),
    xFileControl: Some(x_file_control),
    xSectorSize: Some(x_sector_size),
    xDeviceCharacteristics: Some(x_device_characteristics),
    xShmMap: None,
    xShmLock: None,
    xShmBarrier: None,
    xShmUnmap: None,
    xFetch: None,
    xUnfetch: None,
};

unsafe extern "C" fn x_delete(
    _vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    _sync_dir: c_int,
) -> c_int {
    let Ok(path) = unsafe { CStr::from_ptr(name) }.to_str() else {
        return ffi::SQLITE_IOERR_DELETE;
    };
    match with_fs(|fs, _| fs.unlink_sync(path)).and_then(|()| sync_dir(path)) {
        Ok(()) => ffi::SQLITE_OK,
        Err(error) if code(&error) == "ENOENT" => ffi::SQLITE_IOERR_DELETE_NOENT,
        Err(error) => failed(error, ffi::SQLITE_IOERR_DELETE),
    }
}

unsafe extern "C" fn x_access(
    _vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    _flags: c_int,
    out: *mut c_int,
) -> c_int {
    let Ok(path) = unsafe { CStr::from_ptr(name) }.to_str() else {
        return ffi::SQLITE_IOERR_ACCESS;
    };
    match with_fs(|fs, _| Ok(fs.exists_sync(path))) {
        Ok(exists) => {
            unsafe { *out = c_int::from(exists) };
            ffi::SQLITE_OK
        }
        Err(error) => failed(error, ffi::SQLITE_IOERR_ACCESS),
    }
}

/// Paths arrive absolute (the package resolves them) and are kept as given.
unsafe extern "C" fn x_full_pathname(
    _vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    size: c_int,
    out: *mut c_char,
) -> c_int {
    let path = unsafe { CStr::from_ptr(name) };
    let bytes = path.to_bytes_with_nul();
    if !path.to_str().is_ok_and(absolute) || bytes.len() > size as usize {
        return ffi::SQLITE_CANTOPEN;
    }
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr().cast(), out, bytes.len()) };
    ffi::SQLITE_OK
}

/// WAL salts and temp names; the store's own identities come from getrandom too.
unsafe extern "C" fn x_randomness(
    _vfs: *mut ffi::sqlite3_vfs,
    size: c_int,
    out: *mut c_char,
) -> c_int {
    let buffer = unsafe { std::slice::from_raw_parts_mut(out.cast::<u8>(), size as usize) };
    match getrandom::fill(buffer) {
        Ok(()) => size,
        Err(_) => 0,
    }
}

/// Nothing waits: busy timeouts are zero and the owner holds the database alone.
unsafe extern "C" fn x_sleep(_vfs: *mut ffi::sqlite3_vfs, microseconds: c_int) -> c_int {
    microseconds
}

unsafe extern "C" fn x_current_time(_vfs: *mut ffi::sqlite3_vfs, out: *mut f64) -> c_int {
    unsafe { *out = 2_440_587.5 + js_sys::Date::now() / 86_400_000.0 };
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_current_time_int64(
    _vfs: *mut ffi::sqlite3_vfs,
    out: *mut ffi::sqlite3_int64,
) -> c_int {
    unsafe { *out = (210_866_760_000_000.0 + js_sys::Date::now()) as ffi::sqlite3_int64 };
    ffi::SQLITE_OK
}

fn register() -> Result<(), Error> {
    let name = CString::new(VFS).expect("no NUL in the VFS name");
    if !unsafe { ffi::sqlite3_vfs_find(name.as_ptr()) }.is_null() {
        return Ok(());
    }
    let vfs = Box::leak(Box::new(ffi::sqlite3_vfs {
        iVersion: 2,
        szOsFile: std::mem::size_of::<File>() as c_int,
        mxPathname: 4096,
        pNext: std::ptr::null_mut(),
        zName: name.into_raw(),
        pAppData: std::ptr::null_mut(),
        xOpen: Some(x_open),
        xDelete: Some(x_delete),
        xAccess: Some(x_access),
        xFullPathname: Some(x_full_pathname),
        xDlOpen: None,
        xDlError: None,
        xDlSym: None,
        xDlClose: None,
        xRandomness: Some(x_randomness),
        xSleep: Some(x_sleep),
        xCurrentTime: Some(x_current_time),
        xGetLastError: None,
        xCurrentTimeInt64: Some(x_current_time_int64),
        xSetSystemCall: None,
        xGetSystemCall: None,
        xNextSystemCall: None,
    }));
    match unsafe { ffi::sqlite3_vfs_register(vfs, 0) } {
        ffi::SQLITE_OK => Ok(()),
        _ => Err(Error::Unsupported),
    }
}

fn io(error: JsValue) -> Error {
    Error::Io(std::io::Error::other(describe(&error)))
}

/// A note that turns a store's lock file into the seal of a session that moved
/// elsewhere, once the store closes (shared by the place and its open store's owner).
pub type Seal = std::rc::Rc<RefCell<Option<String>>>;

/// The lock file naming a store's owner, held while the store is open. It is removed
/// only when the store closes (after SQLite), or when opening fails; a sealed one
/// stays, holding the seal's note (no host opens that copy again).
pub struct NodeOwner {
    path: String,
    seal: Option<Seal>,
}

impl OwnerExclusion for NodeOwner {}

impl NodeOwner {
    /// The lock file's path for `database`.
    pub fn path(database: &str) -> String {
        format!("{database}.lock")
    }

    /// Creates the lock file holding `note` (who owns it), or `Locked` if one exists
    /// (a package owner's, or a native host's: it holds one while it owns the store).
    /// The note is written as `spindle-embed: <note>`, so it never reads as a native
    /// owner's, which a native host may take over once that owner is gone.
    pub fn acquire(database: &str, note: &str) -> Result<Self, Error> {
        let path = Self::path(database);
        let note = format!("spindle-embed: {note}");
        let fd = with_fs(|fs, _| fs.open_sync(&path, "wx", 0o600)).map_err(|error| {
            if code(&error) == "EEXIST" {
                Error::Locked
            } else {
                io(error)
            }
        })?;
        let owner = Self { path, seal: None };
        let written = with_fs(|fs, _| {
            let bytes = note.as_bytes();
            let mut done = 0usize;
            while done < bytes.len() {
                let rest = &bytes[done..];
                let written = fs.write_sync(fd, rest, 0, rest.len() as u32, done as f64)?;
                if written == 0 {
                    return Err(JsValue::from_str("the lock file could not be written"));
                }
                done += written as usize;
            }
            let synced = fs.fsync_sync(fd);
            let closed = fs.close_sync(fd);
            synced.and(closed)
        })
        .and_then(|()| sync_dir(&owner.path));
        // A failure here drops `owner`, which removes the half-made lock file.
        written.map_err(io)?;
        Ok(owner)
    }

    /// What the lock file of `database` says about its owner, if there is one.
    pub fn holder(database: &str) -> Option<String> {
        with_fs(|fs, _| fs.read_file_sync(&Self::path(database), "utf8")).ok()
    }
}

impl Drop for NodeOwner {
    fn drop(&mut self) {
        let sealed = self.seal.as_ref().and_then(|seal| seal.borrow().clone());
        match sealed {
            // SQLite has closed: the lock file now says where the session went.
            Some(note) => {
                let _ = write_file(&self.path, note.as_bytes());
            }
            None => {
                let _ =
                    with_fs(|fs, _| fs.unlink_sync(&self.path)).and_then(|()| sync_dir(&self.path));
            }
        }
    }
}

/// The names in the directory `path` (none when it cannot be read).
pub fn read_dir(path: &str) -> Vec<String> {
    with_fs(|fs, _| fs.readdir_sync(path)).unwrap_or_default()
}

/// Creates the directories of `path` (a store inside a session directory), private.
pub fn make_dirs(path: &str) -> Result<(), Error> {
    let options = js_sys::Object::new();
    js_sys::Reflect::set(&options, &"recursive".into(), &JsValue::TRUE)
        .map_err(|_| Error::Invalid)?;
    // Private, as a native host keeps (and requires of) a store's directory.
    js_sys::Reflect::set(&options, &"mode".into(), &JsValue::from(0o700))
        .map_err(|_| Error::Invalid)?;
    with_fs(|fs, _| fs.mkdir_sync(path, &options))
        .map(|_| ())
        .map_err(io)
}

/// Creates the empty private file `path` if it is missing (durably), as a native
/// host's store directory has its `owner.lock`.
pub fn ensure_file(path: &str) -> Result<(), Error> {
    with_fs(|fs, _| {
        if fs.exists_sync(path) {
            return Ok(false);
        }
        let fd = fs.open_sync(path, "wx", 0o600)?;
        let synced = fs.fsync_sync(fd);
        let closed = fs.close_sync(fd);
        synced.and(closed).map(|()| true)
    })
    .and_then(|created| if created { sync_dir(path) } else { Ok(()) })
    .map_err(io)
}

/// Whether `path` exists.
pub fn exists(path: &str) -> bool {
    with_fs(|fs, _| Ok(fs.exists_sync(path))).unwrap_or(false)
}

/// Largest sidecar file read (`read_file`).
const MAX_SIDECAR: usize = 4 * 1024 * 1024;

/// The bytes of the small file at `path` (a session's sidecar), if it is there.
pub fn read_file(path: &str) -> Option<Vec<u8>> {
    with_fs(|fs, _| {
        // Checked first: the OPFS adapter opens a missing path read-only as a directory.
        if !fs.exists_sync(path) {
            return Ok(None);
        }
        let fd = fs.open_sync(path, "r", 0)?;
        let read = (|| {
            let size = fs.fstat_sync(fd)?.size() as usize;
            if size > MAX_SIDECAR {
                return Err(JsValue::from_str("the file is too large for a sidecar"));
            }
            let mut bytes = vec![0; size];
            let mut done = 0;
            while done < size {
                let rest = &mut bytes[done..];
                let length = rest.len() as u32;
                match fs.read_sync(fd, rest, 0, length, done as f64)? {
                    0 => break,
                    read => done += read as usize,
                }
            }
            bytes.truncate(done);
            Ok(bytes)
        })();
        let _ = fs.close_sync(fd);
        read.map(Some)
    })
    .ok()
    .flatten()
}

/// Replaces the file at `path` with `bytes` durably: a private temporary file beside
/// it, synced, renamed over it, then the directory synced.
pub fn write_file(path: &str, bytes: &[u8]) -> Result<(), Error> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let pending = format!("{}/.{name}.tmp", parent(path));
    let written = with_fs(|fs, _| {
        if fs.exists_sync(&pending) {
            fs.unlink_sync(&pending)?;
        }
        let fd = fs.open_sync(&pending, "wx", 0o600)?;
        let filled = (|| {
            let mut done = 0usize;
            while done < bytes.len() {
                let rest = &bytes[done..];
                match fs.write_sync(fd, rest, 0, rest.len() as u32, done as f64)? {
                    0 => return Err(JsValue::from_str("the file could not be written")),
                    written => done += written as usize,
                }
            }
            fs.fsync_sync(fd)
        })();
        let closed = fs.close_sync(fd);
        filled.and(closed)?;
        fs.rename_sync(&pending, path)
    })
    .and_then(|()| sync_dir(path));
    if written.is_err() {
        let _ = with_fs(|fs, _| fs.unlink_sync(&pending));
    }
    written.map_err(io)
}

/// Renames `from` to `to` (in the same directory) durably.
pub fn rename(from: &str, to: &str) -> Result<(), Error> {
    with_fs(|fs, _| fs.rename_sync(from, to))
        .and_then(|()| sync_dir(to))
        .map_err(io)
}

/// Opens (or creates) the store at `database` (absolute) through the Node VFS, owned
/// by a lock file holding `note`.
pub fn open(
    database: &str,
    journal: JournalMode,
    initialization: Initialization,
    note: &str,
) -> Result<Store, Error> {
    if !absolute(database) {
        return Err(Error::Invalid);
    }
    open_sealable(database, journal, initialization, note, None)
}

/// `open`, its lock file sealed with `seal`'s note (if one is set by then) when the
/// store closes instead of removed.
pub fn open_sealable(
    database: &str,
    journal: JournalMode,
    initialization: Initialization,
    note: &str,
    seal: Option<Seal>,
) -> Result<Store, Error> {
    if !absolute(database) {
        return Err(Error::Invalid);
    }
    let _ = take_last_error();
    let mut owner = NodeOwner::acquire(database, note)?;
    owner.seal = seal;
    open_owned(database, journal, initialization, owner)
}

/// Opens the store at `database` through this VFS behind a guard the host already
/// holds (a browser's Web Lock), released only after SQLite closes.
pub fn open_owned(
    database: &str,
    journal: JournalMode,
    initialization: Initialization,
    owner: impl OwnerExclusion + 'static,
) -> Result<Store, Error> {
    if !absolute(database) {
        return Err(Error::Invalid);
    }
    // A failure's reason is this open's own, not one an earlier call left.
    let _ = take_last_error();
    let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if initialization == Initialization::Create {
        flags |= OpenFlags::SQLITE_OPEN_CREATE;
    }
    let db = Connection::open_with_flags_and_vfs(database, flags, VFS)?;
    Store::from_connection(
        db,
        ConnectionProfile::Persistent {
            journal,
            initialization,
        },
        owner,
    )
}
