//! Session bundles: one file carrying a session directory's portable
//! files, so a session moves where no directory is shared (a browser's OPFS, memory,
//! another machine). Each store travels as one image (`Store::image`), with the
//! settings mirror (`session.json`) and each task's identity record (`task.json`).
//!
//! The format is Spindle's own: the magic `spindle-session\n`, a format number (u32
//! little-endian), a manifest's length (u32) and the manifest (JSON: format, schema,
//! the session's head and identity, and each file's path, size and SHA-256), the
//! files' bytes in manifest order, and the SHA-256 of everything before it. Reading
//! checks all of it, and every store as a full open would, before a host makes any of
//! it a session: `read` in memory (the package), `unpack_file` from a file into a
//! private staging directory a native host renames into place only once all of it is
//! checked (never holding the bundle's files, or a store, in memory: a store's check
//! holds its page cache and one record at a time, a value of at most 32 MiB, whatever
//! the store's size; a store's settings too large for a sidecar are refused before they
//! are kept, in memory or staged). A bundle names
//! only paths a session directory has. A place's MCP pack session id
//! (`pack-session-id`) travels too, so a pack keeps its session across a move; bundles
//! written before it simply lack the file.
use crate::{ImageFacts, check_image};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::path::Path;

/// How a bundle begins.
pub const MAGIC: &[u8; 16] = b"spindle-session\n";
/// The bundle format this build writes and reads.
pub const FORMAT: u32 = 1;
/// A session's own store, as a bundle names it.
pub const STORE: &str = "store/authority.sqlite";
/// The file a native host keeps a place's MCP pack session id in (`code_host`).
pub const PACK_SESSION: &str = "pack-session-id";
const MAX_MANIFEST: usize = 1024 * 1024;
const MAX_FILES: usize = 4096;
const MAX_SIDECAR: usize = 4 * 1024 * 1024;
/// Deepest task nesting a bundle carries (a session's `tasks.max_depth` is at most 16).
const MAX_NESTING: usize = 16;

/// One file of a session directory, by its path inside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// A checked bundle: the session it moves and its files, the session's own store last
/// (so a host that writes them in order makes the session appear only once the rest is
/// in place).
#[derive(Debug)]
pub struct Bundle {
    pub head: u64,
    pub identity: String,
    pub files: Vec<BundleFile>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether `path` is one a session directory holds and a bundle may carry:
/// `(tasks/<key>/)*` (a key is 20 lowercase hex digits), then `store/authority.sqlite`,
/// `session.json`, `pack-session-id`, or (in a task) `task.json`.
pub fn valid_path(path: &str) -> bool {
    let mut rest = path;
    let mut depth = 0;
    while let Some(after) = rest.strip_prefix("tasks/") {
        let Some((key, tail)) = after.split_once('/') else {
            return false;
        };
        if key.len() != 20 || !key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return false;
        }
        depth += 1;
        if depth > MAX_NESTING {
            return false;
        }
        rest = tail;
    }
    rest == STORE
        || rest == "session.json"
        || rest == PACK_SESSION
        || (depth > 0 && rest == "task.json")
}

/// Whether `bytes` are a pack session id as `code_host` keeps one: 8-64 letters,
/// digits or `-` (surrounding whitespace aside).
fn pack_session_ok(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok_and(|text| {
        let id = text.trim();
        (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// The directory part of `path` (`""` for the session's own files, `tasks/<key>/...`).
fn place_of(path: &str) -> &str {
    let name = match path.strip_suffix(STORE) {
        Some(_) => STORE,
        None => path.rsplit('/').next().unwrap_or(path),
    };
    &path[..path.len() - name.len()]
}

/// Where `order` puts the file at `path`: each place's store after its other files,
/// deepest tasks first and the session's own store last.
fn order_key(path: &str) -> (std::cmp::Reverse<usize>, String, bool, String) {
    let place = place_of(path);
    (
        std::cmp::Reverse(place.matches("tasks/").count()),
        place.to_string(),
        path.ends_with(STORE),
        path.to_string(),
    )
}

/// Orders files so each place's store comes after its other files, deepest tasks
/// first and the session's own store last.
fn order(files: &mut [BundleFile]) {
    files.sort_by_key(|file| order_key(&file.path));
}

/// Checks a sidecar's bytes: JSON (an object for a task's identity record), or a pack
/// session id.
fn check_sidecar(path: &str, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > MAX_SIDECAR {
        return Err(format!("{path} is too large for a sidecar"));
    }
    if path.ends_with(PACK_SESSION) {
        return if pack_session_ok(bytes) {
            Ok(())
        } else {
            Err(format!("{path} in the bundle is not a pack session id"))
        };
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| format!("{path} in the bundle is not JSON"))?;
    if path.ends_with("task.json") && !value.is_object() {
        return Err(format!("{path} in the bundle is not a task's identity"));
    }
    Ok(())
}

/// Checks a bundle's layout from each file's path and size alone: from the manifest
/// before any file's bytes are read (so nothing is allocated for a file the bundle could
/// not hold), and from the files a writer is given (`check_files`), so reading and
/// writing refuse the same. Every path is one a session holds, none twice, the
/// session's own store is there, and every other file is a sidecar beside its place's
/// store, within a sidecar's size. A place's `session.json` is held to that size only
/// once its store's settings have replaced it (`check_files`), as they do when the
/// store has them.
fn check_layout(files: &[(&str, usize)]) -> Result<(), String> {
    if files.len() > MAX_FILES {
        return Err(format!("a bundle holds at most {MAX_FILES} files"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for (path, _) in files {
        if !valid_path(path) {
            return Err(format!("`{path}` is not a path a session holds"));
        }
        if !seen.insert(*path) {
            return Err(format!("`{path}` is in the bundle twice"));
        }
    }
    if !seen.contains(STORE) {
        return Err("a bundle carries the session's own store".into());
    }
    for (path, size) in files {
        if path.ends_with(STORE) {
            continue;
        }
        // A task's place holds its store: a sidecar alone moves no task.
        let place = place_of(path);
        let beside = seen.contains(format!("{place}{STORE}").as_str());
        if *size > MAX_SIDECAR && !(beside && *path == format!("{place}session.json")) {
            return Err(format!("{path} is too large for a sidecar"));
        }
        if !beside {
            return Err(format!("{path} has no store beside it"));
        }
    }
    Ok(())
}

/// Why a bundle was not unpacked (`unpack_file`).
#[derive(Debug)]
pub enum UnpackError {
    /// Not a bundle this build reads: why, as `read` says it.
    Refused(String),
    /// Reading the bundle, or writing (or reading back) what it unpacked, failed.
    Io(std::io::Error),
}

impl std::fmt::Display for UnpackError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(why) => formatter.write_str(why),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl From<UnpackError> for String {
    fn from(error: UnpackError) -> Self {
        error.to_string()
    }
}

/// A bundle's files where `checked` finds them: in memory (a writer's, `read`'s), or
/// unpacked into a directory (`unpack_file`). Both are checked by the same code.
trait Files {
    /// Each file's path and size, in order.
    fn layout(&self) -> Vec<(&str, usize)>;
    /// The store at `path` checked: `whole`, or as far as `image_facts` checks.
    fn store(&self, path: &str, whole: bool) -> std::io::Result<crate::Result<ImageFacts>>;
    /// Makes the file at `path` hold `bytes` (added after the others if it is not there).
    fn put(&mut self, path: String, bytes: Vec<u8>) -> std::io::Result<()>;
    /// The bytes of the sidecar at `path`: all of them, or more than a sidecar holds.
    fn sidecar(&self, path: &str) -> std::io::Result<Cow<'_, [u8]>>;
}

impl Files for Vec<BundleFile> {
    fn layout(&self) -> Vec<(&str, usize)> {
        self.iter()
            .map(|file| (file.path.as_str(), file.bytes.len()))
            .collect()
    }

    fn store(&self, path: &str, whole: bool) -> std::io::Result<crate::Result<ImageFacts>> {
        let file = self.iter().find(|file| file.path == path);
        let bytes = file.map_or(&[][..], |file| &file.bytes[..]);
        Ok(if whole {
            check_image(bytes)
        } else {
            crate::image_facts(bytes)
        })
    }

    fn put(&mut self, path: String, bytes: Vec<u8>) -> std::io::Result<()> {
        match self.iter_mut().find(|file| file.path == path) {
            Some(file) => file.bytes = bytes,
            None => self.push(BundleFile { path, bytes }),
        }
        Ok(())
    }

    fn sidecar(&self, path: &str) -> std::io::Result<Cow<'_, [u8]>> {
        let file = self.iter().find(|file| file.path == path);
        Ok(Cow::Borrowed(file.map_or(&[][..], |file| &file.bytes[..])))
    }
}

/// A bundle's files unpacked under `root`: each one's path and size, in order.
struct Unpacking<'a> {
    root: &'a Path,
    files: Vec<(String, usize)>,
}

impl Files for Unpacking<'_> {
    fn layout(&self) -> Vec<(&str, usize)> {
        self.files
            .iter()
            .map(|(path, size)| (path.as_str(), *size))
            .collect()
    }

    /// Always checked whole: an unpacked bundle is an import's. A failure of this host
    /// rather than of the store (`host_failure`) is an I/O failure, not a refusal.
    fn store(&self, path: &str, _whole: bool) -> std::io::Result<crate::Result<ImageFacts>> {
        match crate::check_image_file(&self.root.join(path)) {
            Err(crate::Error::Io(error)) => Err(error),
            Err(error) => match host_failure(&error) {
                Some(kind) => Err(std::io::Error::new(
                    kind,
                    format!("cannot check {path} where it was unpacked: {error:?}"),
                )),
                None => Ok(Err(error)),
            },
            checked => Ok(checked),
        }
    }

    fn put(&mut self, path: String, bytes: Vec<u8>) -> std::io::Result<()> {
        crate::durable::write(&self.root.join(&path), &bytes)?;
        match self.files.iter_mut().find(|(name, _)| *name == path) {
            Some((_, size)) => *size = bytes.len(),
            None => self.files.push((path, bytes.len())),
        }
        Ok(())
    }

    fn sidecar(&self, path: &str) -> std::io::Result<Cow<'_, [u8]>> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(self.root.join(path))?
            .take(MAX_SIDECAR as u64 + 1)
            .read_to_end(&mut bytes)?;
        Ok(Cow::Owned(bytes))
    }
}

/// Whether checking a staged store failed because of this host rather than the store
/// (it could not be opened or read, the disk or memory ran out, something else held it):
/// the kind of I/O failure it is, or None for a store this build does not open.
fn host_failure(error: &crate::Error) -> Option<std::io::ErrorKind> {
    use rusqlite::ErrorCode;
    match error {
        crate::Error::Io(error) => Some(error.kind()),
        crate::Error::Locked => Some(std::io::ErrorKind::ResourceBusy),
        crate::Error::Sql(rusqlite::Error::SqliteFailure(failure, _)) => match failure.code {
            ErrorCode::DiskFull => Some(std::io::ErrorKind::StorageFull),
            ErrorCode::OutOfMemory => Some(std::io::ErrorKind::OutOfMemory),
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => {
                Some(std::io::ErrorKind::ResourceBusy)
            }
            ErrorCode::CannotOpen
            | ErrorCode::SystemIoFailure
            | ErrorCode::PermissionDenied
            | ErrorCode::ReadOnly
            | ErrorCode::FileLockingProtocolFailed
            | ErrorCode::NoLargeFileSupport => Some(std::io::ErrorKind::Other),
            _ => None,
        },
        _ => None,
    }
}

/// Checks `files` (their layout, every store: each `whole`, or its facts only, as
/// `write_copy` does, then the sidecars) and returns the session store's facts (its
/// settings left out: they are the bundle's `session.json`). Each place's
/// `session.json` is its store's own settings (the store is what every host reads; the
/// file only mirrors it): one is filled in, or set right, from them. Settings too large
/// for a sidecar are refused before they are kept anywhere, as the sidecar check would
/// refuse them once kept.
fn checked(files: &mut impl Files, whole: bool) -> Result<ImageFacts, UnpackError> {
    check_layout(&files.layout()).map_err(UnpackError::Refused)?;
    let stores: Vec<String> = files
        .layout()
        .into_iter()
        .filter(|(path, _)| path.ends_with(STORE))
        .map(|(path, _)| path.to_string())
        .collect();
    let mut main = None;
    for path in stores {
        let mut facts = files
            .store(&path, whole)
            .map_err(UnpackError::Io)?
            .map_err(|error| {
                UnpackError::Refused(format!(
                    "{path} is not a store this build opens ({error:?})"
                ))
            })?;
        if let Some(settings) = facts.settings.take() {
            let mirror = format!("{}session.json", place_of(&path));
            if settings.len() > MAX_SIDECAR {
                // What `check_sidecar` says of it once kept: no bundle's stores make a
                // staging directory (or memory) hold more settings than sidecars.
                return Err(UnpackError::Refused(format!(
                    "{mirror} is too large for a sidecar"
                )));
            }
            files.put(mirror, settings).map_err(UnpackError::Io)?;
        }
        if path == STORE {
            main = Some(facts);
        }
    }
    let main = main
        .ok_or_else(|| UnpackError::Refused("a bundle carries the session's own store".into()))?;
    let sidecars: Vec<String> = files
        .layout()
        .into_iter()
        .filter(|(path, _)| !path.ends_with(STORE))
        .map(|(path, _)| path.to_string())
        .collect();
    for path in sidecars {
        let bytes = files.sidecar(&path).map_err(UnpackError::Io)?;
        check_sidecar(&path, &bytes).map_err(UnpackError::Refused)?;
    }
    Ok(main)
}

/// `checked` for files in memory.
fn check_files(files: &mut Vec<BundleFile>, whole: bool) -> Result<ImageFacts, String> {
    Ok(checked(files, whole)?)
}

/// A written bundle: its bytes, the session store's facts (head, identity) and how many
/// files it holds.
pub struct Written {
    pub bytes: Vec<u8>,
    pub facts: ImageFacts,
    pub files: usize,
}

/// Writes `files` (a session directory's portable files) as a bundle, every store in
/// it checked whole first: what a move needs, as the session then goes on only from
/// the bundle.
pub fn write(files: Vec<BundleFile>) -> Result<Written, String> {
    written(files, true)
}

/// `write` for a copy: the session stays where it is, so each store is checked as far
/// as the bundle records it (pages, schema, facts), not record by record; `read` checks
/// every record before an import uses one.
pub fn write_copy(files: Vec<BundleFile>) -> Result<Written, String> {
    written(files, false)
}

fn written(mut files: Vec<BundleFile>, whole: bool) -> Result<Written, String> {
    let main = check_files(&mut files, whole)?;
    order(&mut files);
    let manifest = json!({
        "format": FORMAT,
        "schema": crate::CAPABILITIES.schema,
        "head": main.head,
        "identity": main.identity,
        "files": files.iter().map(|file| json!({
            "path": file.path,
            "size": file.bytes.len(),
            "sha256": hex(&Sha256::digest(&file.bytes)),
        })).collect::<Vec<_>>(),
    })
    .to_string();
    let size = MAGIC.len()
        + 8
        + manifest.len()
        + files.iter().map(|file| file.bytes.len()).sum::<usize>()
        + 32;
    let count = files.len();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT.to_le_bytes());
    out.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
    out.extend_from_slice(manifest.as_bytes());
    // Each file goes once it is in the bundle: a store's image is the whole store.
    for file in files {
        out.extend_from_slice(&file.bytes);
    }
    let sum = Sha256::digest(&out);
    out.extend_from_slice(&sum);
    Ok(Written {
        bytes: out,
        facts: main,
        files: count,
    })
}

/// Reads and checks a bundle: its framing and digests, its manifest, every path and
/// every store. Nothing is written anywhere.
pub fn read(bytes: &[u8]) -> Result<Bundle, String> {
    if bytes.len() < MAGIC.len() + 8 + 32 || !bytes.starts_with(MAGIC) {
        return Err(refused("no bundle header"));
    }
    let (body, sum) = bytes.split_at(bytes.len() - 32);
    if Sha256::digest(body).as_slice() != sum {
        return Err(refused(
            "its checksum does not match (damaged or cut short)",
        ));
    }
    parse(&mut &body[..], body.len())
}

/// A bundle unpacked into a directory and checked there (`unpack_file`): the session
/// it moves, and the path of each file it holds now (each place's settings mirror
/// filled in or set right from its store, as `read` gives them), in `read`'s order.
#[derive(Debug)]
pub struct Unpacked {
    pub head: u64,
    pub identity: String,
    pub files: Vec<String>,
}

/// How much of a bundle's file is in memory at once as it is hashed or copied.
const CHUNK: usize = 1024 * 1024;

/// `read` for a bundle in `file`, unpacked into the directory `root` (an empty, private
/// staging directory) rather than into memory: the same checks, refusing the same
/// bundles with the same reasons, holding no file of the bundle (only a sidecar at a
/// time) and no store in memory. The checksum over the whole file first (nothing is
/// written for a damaged bundle), then the manifest (`check_layout`); then each file is
/// copied to its path under `root` through a fixed buffer, owner-only and synced,
/// checked against its digest as it goes; then every store is checked where it lies
/// (`check_image_file`), each place's settings mirror is filled in or set right from
/// its store, and every sidecar is checked. Directories are left for the caller to
/// sync. Refused or failed, `root` may hold part of the bundle: the caller removes it.
pub fn unpack_file(file: &mut std::fs::File, root: &Path) -> Result<Unpacked, UnpackError> {
    use std::io::{Read, Seek};
    let failed = |error: std::io::Error| {
        UnpackError::Io(std::io::Error::new(
            error.kind(),
            format!("cannot read the bundle: {error}"),
        ))
    };
    let length = usize::try_from(file.metadata().map_err(failed)?.len())
        .map_err(|_| UnpackError::Refused(refused("it is too large")))?;
    if length < MAGIC.len() + 8 + 32 {
        return Err(UnpackError::Refused(refused("no bundle header")));
    }
    let body = length - 32;
    file.seek(std::io::SeekFrom::Start(0)).map_err(failed)?;
    let mut head = [0u8; 16];
    file.read_exact(&mut head).map_err(failed)?;
    if &head != MAGIC {
        return Err(UnpackError::Refused(refused("no bundle header")));
    }
    file.seek(std::io::SeekFrom::Start(0)).map_err(failed)?;
    let mut digest = Sha256::new();
    let mut left = body;
    let mut chunk = vec![0u8; CHUNK];
    while left > 0 {
        let want = left.min(chunk.len());
        file.read_exact(&mut chunk[..want]).map_err(failed)?;
        digest.update(&chunk[..want]);
        left -= want;
    }
    let mut sum = [0u8; 32];
    file.read_exact(&mut sum).map_err(failed)?;
    if digest.finalize().as_slice() != sum {
        return Err(UnpackError::Refused(refused(
            "its checksum does not match (damaged or cut short)",
        )));
    }
    file.seek(std::io::SeekFrom::Start(0)).map_err(failed)?;
    let mut reader = std::io::BufReader::new(Read::take(&mut *file, body as u64));
    let manifest = manifest(&mut reader, body).map_err(UnpackError::Refused)?;
    let mut files = Vec::with_capacity(manifest.entries.len());
    let mut skimmed = Vec::new();
    for entry in &manifest.entries {
        if entry.skimmed() {
            if skim(&mut reader, entry.size).map_err(UnpackError::Refused)? != entry.digest {
                return Err(UnpackError::Refused(mismatch(&entry.path)));
            }
            skimmed.push(entry.path.clone());
            continue;
        }
        unpack_entry(&mut reader, root, entry, &mut chunk)?;
        files.push((entry.path.clone(), entry.size));
    }
    drop(chunk);
    let mut unpacking = Unpacking { root, files };
    let main = checked(&mut unpacking, true)?;
    concluded(&manifest, &main, &skimmed, |path| {
        unpacking.files.iter().any(|(name, _)| name == path)
    })
    .map_err(UnpackError::Refused)?;
    let mut files: Vec<String> = unpacking.files.into_iter().map(|(path, _)| path).collect();
    files.sort_by_key(|path| order_key(path));
    Ok(Unpacked {
        head: main.head,
        identity: main.identity,
        files,
    })
}

/// Copies the next `entry.size` bytes of `reader` to a new file at the entry's path
/// under `root` (owner-only, synced) through `chunk`, hashing them as they go: refused
/// unless they are the entry's digest.
fn unpack_entry(
    reader: &mut impl std::io::Read,
    root: &Path,
    entry: &Entry,
    chunk: &mut [u8],
) -> Result<(), UnpackError> {
    use std::io::Write;
    let path = root.join(&entry.path);
    let folder = path.parent().expect("a bundle path has a directory");
    crate::durable::create_dir(folder).map_err(UnpackError::Io)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut out = options.open(&path).map_err(UnpackError::Io)?;
    let mut digest = Sha256::new();
    let mut left = entry.size;
    while left > 0 {
        let want = left.min(chunk.len());
        reader
            .read_exact(&mut chunk[..want])
            .map_err(|_| UnpackError::Refused(refused("it is cut short")))?;
        digest.update(&chunk[..want]);
        out.write_all(&chunk[..want]).map_err(UnpackError::Io)?;
        left -= want;
    }
    if hex(&digest.finalize()) != entry.digest {
        return Err(UnpackError::Refused(mismatch(&entry.path)));
    }
    out.sync_all().map_err(UnpackError::Io)
}

fn refused(why: &str) -> String {
    format!("not a session bundle this build reads: {why}")
}

fn mismatch(path: &str) -> String {
    refused(&format!("{path} does not match its digest"))
}

/// The next `count` bytes of `reader`.
fn take(reader: &mut impl std::io::Read, count: usize) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0u8; count];
    reader
        .read_exact(&mut bytes)
        .map_err(|_| refused("it is cut short"))?;
    Ok(bytes)
}

/// The SHA-256 (hex) of the next `count` bytes of `reader`, read in chunks, not kept.
fn skim(reader: &mut impl std::io::Read, mut count: usize) -> Result<String, String> {
    let mut digest = Sha256::new();
    let mut chunk = vec![0u8; count.min(64 * 1024)];
    while count > 0 {
        let want = count.min(chunk.len());
        reader
            .read_exact(&mut chunk[..want])
            .map_err(|_| refused("it is cut short"))?;
        digest.update(&chunk[..want]);
        count -= want;
    }
    Ok(hex(&digest.finalize()))
}

/// One file a manifest lists: its path, size and SHA-256 (hex).
struct Entry {
    path: String,
    size: usize,
    digest: String,
}

impl Entry {
    /// A settings mirror too large for a sidecar (`check_layout`): checked against its
    /// digest but never kept, as its store's settings replace it, or it is refused.
    fn skimmed(&self) -> bool {
        self.size > MAX_SIDECAR && !self.path.ends_with(STORE)
    }
}

/// A bundle's manifest, checked (`manifest`): the head and identity it names, and the
/// files it lists.
struct Manifest {
    head: Option<u64>,
    identity: Option<String>,
    entries: Vec<Entry>,
}

/// Reads a bundle's header and manifest from `reader` (at the start of a body of
/// `length` bytes: everything before its checksum, which the caller has checked) and
/// checks them: format, schema, every entry whole, the sizes filling the body exactly,
/// and the layout a session has (`check_layout`), before anything is allocated for, or
/// read of, any file.
fn manifest(reader: &mut impl std::io::Read, length: usize) -> Result<Manifest, String> {
    let head = take(reader, MAGIC.len() + 8)?;
    let word = |at: usize| u32::from_le_bytes(head[at..at + 4].try_into().expect("4 bytes"));
    let format = word(MAGIC.len());
    if format != FORMAT {
        return Err(refused(&format!(
            "format {format} (this build reads {FORMAT})"
        )));
    }
    let manifest_length = word(MAGIC.len() + 4) as usize;
    let start = MAGIC.len() + 8;
    if manifest_length > MAX_MANIFEST || start + manifest_length > length {
        return Err(refused("its manifest does not fit"));
    }
    let manifest: Value = serde_json::from_slice(&take(reader, manifest_length)?)
        .map_err(|_| refused("its manifest is not JSON"))?;
    let schema = manifest["schema"].as_i64().unwrap_or(-1);
    if schema != i64::from(crate::CAPABILITIES.schema) && schema != crate::PREVIOUS_SCHEMA {
        return Err(format!(
            "the bundle holds schema {schema} stores; this build reads schema {}",
            crate::CAPABILITIES.schema
        ));
    }
    let listed = manifest["files"]
        .as_array()
        .ok_or_else(|| refused("its manifest lists no files"))?;
    if listed.len() > MAX_FILES {
        return Err(refused("too many files"));
    }
    let mut at = start + manifest_length;
    let mut entries = Vec::with_capacity(listed.len());
    for entry in listed {
        let (Some(path), Some(size), Some(digest)) = (
            entry["path"].as_str(),
            entry["size"]
                .as_u64()
                .and_then(|size| usize::try_from(size).ok()),
            entry["sha256"].as_str(),
        ) else {
            return Err(refused("a file entry is incomplete"));
        };
        at = at
            .checked_add(size)
            .filter(|end| *end <= length)
            .ok_or_else(|| refused("a file runs past the end"))?;
        entries.push(Entry {
            path: path.into(),
            size,
            digest: digest.into(),
        });
    }
    if at != length {
        return Err(refused("bytes follow its last file"));
    }
    let layout: Vec<(&str, usize)> = entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry.size))
        .collect();
    check_layout(&layout)?;
    Ok(Manifest {
        head: manifest["head"].as_u64(),
        identity: manifest["identity"].as_str().map(String::from),
        entries,
    })
}

/// What every read checks once its files are (`checked`): each settings mirror too
/// large to keep (`skimmed`) was replaced by its store's settings (`kept`) or is refused
/// as it came, too large, and the manifest names the session store's head and identity.
fn concluded(
    manifest: &Manifest,
    main: &ImageFacts,
    skimmed: &[String],
    kept: impl Fn(&str) -> bool,
) -> Result<(), String> {
    if let Some(path) = skimmed.iter().find(|path| !kept(path)) {
        return Err(format!("{path} is too large for a sidecar"));
    }
    if manifest.head != Some(main.head) || manifest.identity.as_deref() != Some(&main.identity) {
        return Err(refused("its manifest does not match the session's store"));
    }
    Ok(())
}

/// Parses a bundle's `body` (`length` bytes: everything before its checksum, which
/// the caller has checked) and checks its manifest, paths and stores. The manifest's
/// layout (`check_layout`, and sizes that fill the body exactly) is checked before any
/// file is read, and no sidecar larger than a sidecar may be is ever held.
fn parse(reader: &mut impl std::io::Read, length: usize) -> Result<Bundle, String> {
    let manifest = manifest(reader, length)?;
    let mut files = Vec::with_capacity(manifest.entries.len());
    let mut skimmed = Vec::new();
    for entry in &manifest.entries {
        if entry.skimmed() {
            if skim(reader, entry.size)? != entry.digest {
                return Err(mismatch(&entry.path));
            }
            skimmed.push(entry.path.clone());
            continue;
        }
        let bytes = take(reader, entry.size)?;
        if hex(&Sha256::digest(&bytes)) != entry.digest {
            return Err(mismatch(&entry.path));
        }
        files.push(BundleFile {
            path: entry.path.clone(),
            bytes,
        });
    }
    let main = check_files(&mut files, true)?;
    concluded(&manifest, &main, &skimmed, |path| {
        files.iter().any(|file| file.path == path)
    })?;
    order(&mut files);
    Ok(Bundle {
        head: main.head,
        identity: main.identity,
        files,
    })
}

/// The head and identity a bundle's manifest names, read from its first bytes only
/// (nothing else is checked: for a bundle this host wrote and checked itself).
pub fn manifest_facts(reader: &mut impl std::io::Read) -> Result<(u64, String), String> {
    let mut head = [0u8; 24];
    reader
        .read_exact(&mut head)
        .map_err(|_| refused("no bundle header"))?;
    if &head[..16] != MAGIC {
        return Err(refused("no bundle header"));
    }
    let length = u32::from_le_bytes(head[20..24].try_into().expect("4 bytes")) as usize;
    if length > MAX_MANIFEST {
        return Err(refused("its manifest does not fit"));
    }
    let mut manifest = vec![0u8; length];
    reader
        .read_exact(&mut manifest)
        .map_err(|_| refused("its manifest does not fit"))?;
    let manifest: Value =
        serde_json::from_slice(&manifest).map_err(|_| refused("its manifest is not JSON"))?;
    match (manifest["head"].as_u64(), manifest["identity"].as_str()) {
        (Some(head), Some(identity)) => Ok((head, identity.to_string())),
        _ => Err(refused("its manifest names no head")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_session_directorys_own_paths_are_carried() {
        let key = "0123456789abcdef0123";
        for good in [
            STORE.to_string(),
            "session.json".into(),
            format!("tasks/{key}/{STORE}"),
            format!("tasks/{key}/task.json"),
            format!("tasks/{key}/tasks/{key}/session.json"),
            PACK_SESSION.into(),
            format!("tasks/{key}/{PACK_SESSION}"),
        ] {
            assert!(valid_path(&good), "{good}");
        }
        for bad in [
            "task.json".to_string(),
            "store/authority.sqlite-wal".into(),
            "../store/authority.sqlite".into(),
            "/session.json".into(),
            "tasks/../session.json".into(),
            format!("tasks/{}/session.json", key.to_uppercase()),
            "tasks/0123/session.json".into(),
            format!("tasks/{key}/usage.json"),
            format!("{}{STORE}", format!("tasks/{key}/").repeat(17)),
            format!("{PACK_SESSION}/x"),
        ] {
            assert!(!valid_path(&bad), "{bad}");
        }
        // A pack session id is what code_host keeps, never anything else.
        let id = |bytes: &[u8]| check_sidecar(PACK_SESSION, bytes);
        assert!(id(b"1761990000-4242-a1b2c3d4e5f6").is_ok());
        assert!(id(b"dir-a1b2c3d4e5f6\n").is_ok());
        for bad in [
            &b"short"[..],
            b"has space inside",
            b"{}",
            &[b'a'; 65],
            b"\xff\xfe\xfd\xfc\xfb\xfa\xf9\xf8",
        ] {
            assert!(id(bad).is_err(), "{bad:?}");
        }
    }

    /// A reader of a bundle's header and manifest only, noting any read past them (into
    /// a file's bytes, which `parse` allocates just before it reads them).
    struct ManifestOnly {
        bytes: std::io::Cursor<Vec<u8>>,
        past: bool,
    }
    impl std::io::Read for ManifestOnly {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let read = self.bytes.read(buf)?;
            self.past |= read == 0 && !buf.is_empty();
            Ok(read)
        }
    }

    /// `parse` of a body whose manifest lists `files` (path, size; no digest matches)
    /// and whose length is their sizes and `extra` more: its answer, and whether it
    /// read past the manifest.
    fn parse_manifest(files: &[(&str, usize)], extra: isize) -> (Result<Bundle, String>, bool) {
        let manifest = json!({
            "format": FORMAT,
            "schema": crate::CAPABILITIES.schema,
            "head": 1,
            "identity": "00",
            "files": files
                .iter()
                .map(|(path, size)| json!({"path": path, "size": size, "sha256": "00"}))
                .collect::<Vec<_>>(),
        })
        .to_string();
        let mut head = MAGIC.to_vec();
        head.extend_from_slice(&FORMAT.to_le_bytes());
        head.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
        head.extend_from_slice(manifest.as_bytes());
        let length = head.len() + files.iter().map(|(_, size)| size).sum::<usize>();
        let mut reader = ManifestOnly {
            bytes: std::io::Cursor::new(head),
            past: false,
        };
        let parsed = parse(&mut reader, length.checked_add_signed(extra).unwrap());
        (parsed, reader.past)
    }

    #[test]
    fn a_manifest_that_lays_out_no_session_is_refused_before_any_file_is_read() {
        let key = "0123456789abcdef0123";
        let task_store = format!("tasks/{key}/{STORE}");
        let task_identity = format!("tasks/{key}/task.json");
        let task_mirror = format!("tasks/{key}/session.json");
        // Files a gigabyte each: nothing is allocated or read for any of them.
        let huge = 1 << 30;
        let over = MAX_SIDECAR + 1;
        for (files, extra, why) in [
            (
                vec![(STORE, huge), ("../escape", huge)],
                0,
                "`../escape` is not a path",
            ),
            (
                vec![(STORE, huge), (STORE, huge)],
                0,
                "is in the bundle twice",
            ),
            (
                vec![(STORE, huge), (PACK_SESSION, over)],
                0,
                "pack-session-id is too large for a sidecar",
            ),
            (
                vec![(STORE, huge), (&task_store, huge), (&task_identity, over)],
                0,
                "task.json is too large for a sidecar",
            ),
            // A settings mirror may be larger only beside the store that replaces it.
            (
                vec![(STORE, huge), (&task_mirror, over)],
                0,
                "session.json is too large for a sidecar",
            ),
            (
                vec![(STORE, huge), (&task_identity, 2)],
                0,
                "has no store beside it",
            ),
            (
                vec![("session.json", 2), (PACK_SESSION, 8)],
                0,
                "carries the session's own store",
            ),
            (vec![(STORE, huge)], 1, "bytes follow its last file"),
            (vec![(STORE, huge)], -1, "a file runs past the end"),
        ] {
            let (parsed, past) = parse_manifest(&files, extra);
            let error = parsed.unwrap_err();
            assert!(error.contains(why), "{why}: {error}");
            assert!(!past, "{why}: a file was read");
        }
        // What a manifest a session could have leads to: its files are read.
        let (parsed, past) = parse_manifest(&[(STORE, 16), ("session.json", 2)], 0);
        assert!(parsed.unwrap_err().contains("cut short"));
        assert!(past);
    }

    /// A staged store this host could not check (it could not open or read it, ran out
    /// of disk or memory, or something else held it) is an I/O failure (a host's 500),
    /// not a refusal of the bundle (its 400); a file that is no store of this build
    /// stays a refusal.
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn a_store_this_host_cannot_check_is_a_failure_not_a_refusal() {
        use rusqlite::ffi;
        let sql =
            |code| crate::Error::Sql(rusqlite::Error::SqliteFailure(ffi::Error::new(code), None));
        for code in [
            ffi::SQLITE_CANTOPEN,
            ffi::SQLITE_IOERR,
            ffi::SQLITE_IOERR_SHORT_READ,
            ffi::SQLITE_FULL,
            ffi::SQLITE_NOMEM,
            ffi::SQLITE_BUSY,
            ffi::SQLITE_LOCKED,
            ffi::SQLITE_PERM,
            ffi::SQLITE_READONLY,
        ] {
            assert!(host_failure(&sql(code)).is_some(), "{code}");
        }
        assert!(host_failure(&crate::Error::Locked).is_some());
        for code in [
            ffi::SQLITE_CORRUPT,
            ffi::SQLITE_NOTADB,
            ffi::SQLITE_ERROR,
            ffi::SQLITE_TOOBIG,
            ffi::SQLITE_CONSTRAINT,
            ffi::SQLITE_MISMATCH,
        ] {
            assert!(host_failure(&sql(code)).is_none(), "{code}");
        }
        for error in [
            crate::Error::Incompatible,
            crate::Error::Unsupported,
            crate::Error::Invalid,
        ] {
            assert!(host_failure(&error).is_none(), "{error:?}");
        }
        // Staged, and held by another connection meanwhile: this host's failure.
        let root =
            std::env::temp_dir().join(format!("spindle-bundle-held-{}", crate::process_id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("store")).unwrap();
        let holder = rusqlite::Connection::open(root.join(STORE)).unwrap();
        holder
            .execute_batch("PRAGMA journal_mode=DELETE; CREATE TABLE t(x); BEGIN EXCLUSIVE;")
            .unwrap();
        let unpacking = Unpacking {
            root: &root,
            files: vec![(STORE.into(), 0)],
        };
        let failed = unpacking.store(STORE, true).unwrap_err();
        assert_eq!(failed.kind(), std::io::ErrorKind::ResourceBusy, "{failed}");
        // Free, it is a database but no store of this build: refused.
        drop(holder);
        assert!(matches!(
            unpacking.store(STORE, true),
            Ok(Err(crate::Error::Incompatible))
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn files_are_ordered_deepest_first_with_each_store_after_its_sidecars() {
        let key = "0123456789abcdef0123";
        let mut files: Vec<BundleFile> = [
            STORE.to_string(),
            "session.json".into(),
            format!("tasks/{key}/{STORE}"),
            format!("tasks/{key}/task.json"),
            format!("tasks/{key}/session.json"),
        ]
        .into_iter()
        .map(|path| BundleFile {
            path,
            bytes: vec![],
        })
        .collect();
        order(&mut files);
        let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                format!("tasks/{key}/session.json").as_str(),
                &format!("tasks/{key}/task.json"),
                &format!("tasks/{key}/{STORE}"),
                "session.json",
                STORE,
            ]
        );
    }
}
