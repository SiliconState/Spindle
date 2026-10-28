//! Experimental local admission store, not the Spindle execution authority.
//! Caller authentication and permission-to-retain payload bytes are prerequisites.

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::Duration;

const APP_ID: i64 = 0x53504e44;
const SCHEMA: i64 = 19;
/// Older schemas that migrate in place on open: 14 kept every value uncompressed, 15
/// compressed each value on its own; 16 added shared chunks, a validation watermark
/// and recorded provider refusals; 17 adds app state and the open/crash record.
const LEGACY_SCHEMAS: [i64; 5] = [14, 15, 16, 17, 18];
/// Copy of a schema-14 store taken before migrating it, beside `authority.sqlite`.
pub const LEGACY_BACKUP: &str = "authority.v14.sqlite";
/// Tables schema 16 added; an older store gets them when it migrates.
const NEW_TABLES: &str = "CREATE TABLE chunks(digest BLOB PRIMARY KEY, bytes BLOB NOT NULL);
    CREATE TABLE validated(id INTEGER PRIMARY KEY CHECK(id=1), revision INTEGER NOT NULL, receipt INTEGER NOT NULL);
    INSERT INTO validated VALUES(1,0,0);
    CREATE TABLE attempt_failures(attempt TEXT PRIMARY KEY REFERENCES attempts(id), status INTEGER NOT NULL, detail TEXT NOT NULL);
    CREATE INDEX revisions_workspace ON revisions(workspace);
    CREATE INDEX staging_pins_digest ON staging_pins(digest);
    CREATE INDEX artifact_stages_scope ON artifact_stages(scope);";
/// Tables schema 17 added: each scope's app state, kept with the revision that set it,
/// the state patch an app tool's answer carried (kept with its receipt), and how the
/// store was last opened (its epoch, the head it opened at, whether that
/// owner closed it, and how many owners stopped without closing since the head last
/// moved).
const TABLES_17: &str = "CREATE TABLE app_state(revision INTEGER PRIMARY KEY REFERENCES revisions(revision), value BLOB NOT NULL);
    CREATE TABLE opens(id INTEGER PRIMARY KEY CHECK(id=1), epoch INTEGER NOT NULL CHECK(epoch>=0), head INTEGER NOT NULL CHECK(head>=0), closed INTEGER NOT NULL CHECK(closed IN (0,1)), crashes INTEGER NOT NULL CHECK(crashes>=0));
    INSERT INTO opens VALUES(1,0,0,1,0);
    CREATE TABLE app_patches(attempt TEXT PRIMARY KEY REFERENCES receipts(attempt), patch BLOB NOT NULL);";

const TABLES_19: &str = "CREATE TABLE instance(id INTEGER PRIMARY KEY CHECK(id=1), identity BLOB NOT NULL CHECK(length(identity)=16));
    INSERT INTO instance VALUES(1,zeroblob(16));
    CREATE TABLE app_details(attempt TEXT PRIMARY KEY REFERENCES receipts(attempt), value BLOB NOT NULL);
    CREATE TABLE retry_delays(attempt TEXT PRIMARY KEY REFERENCES attempt_failures(attempt), millis INTEGER NOT NULL CHECK(millis>=0));
    CREATE TABLE task_runs(task TEXT PRIMARY KEY, origin TEXT, input BLOB NOT NULL, base TEXT NOT NULL REFERENCES objects(digest), revision INTEGER NOT NULL REFERENCES revisions(revision), queued INTEGER NOT NULL CHECK(queued IN (0,1)), state TEXT NOT NULL CHECK(state IN ('running','stopped','finished','abandoned')), answer BLOB, delivered INTEGER REFERENCES revisions(revision), lease REAL);";

/// The copy kept beside `authority.sqlite` when a store of `schema` migrates. Builds
/// that read only that schema cannot open the migrated store but can open this copy.
pub fn backup_name(schema: i64) -> String {
    format!("authority.v{schema}.sqlite")
}

#[cfg(feature = "agent")]
pub mod agent;
#[cfg(feature = "agent")]
pub mod api;
#[cfg(feature = "driver")]
pub mod bundle;
#[cfg(feature = "driver")]
pub mod driver;
#[cfg(feature = "agent")]
pub use api::API_VERSION;
pub mod artifact;
#[cfg(feature = "dext-turn")]
pub mod broker;
#[cfg(feature = "agent")]
pub mod code_host;
// HTTP grants: checked by both hosts, sent by a native one (an app sends its own).
#[cfg(any(feature = "agent", feature = "embedded"))]
mod code_http;
// Native code mode and the embedded package journal script children with it.
#[cfg(any(feature = "dext-strict-workspace", feature = "embedded"))]
pub mod code_mode;
pub mod codec;
pub mod completion;
pub mod durable;
#[cfg(feature = "embedded")]
pub mod embedded;
#[cfg(feature = "dext-image")]
pub mod image;
#[cfg(feature = "dext-strict-workspace")]
pub mod jobs;
pub mod meta;
pub mod ownership;
#[cfg(feature = "dext-strict-workspace")]
pub mod pack_runtime;
#[cfg(feature = "driver")]
pub mod protocol;
#[cfg(feature = "driver")]
pub mod reads;
pub mod rewind;
#[cfg(feature = "shape")]
pub(crate) mod shape;
pub mod transition;
#[cfg(feature = "dext-turn")]
pub mod turn;
#[cfg(not(target_family = "wasm"))]
pub mod upgrade;
#[cfg(feature = "dext-turn")]
pub mod view;
pub mod workspace;
/// Largest value one step can hold: an admitted command, a continuation (history plus
/// any request that repeats it), a model request or a reply. Values are stored
/// compressed (`codec`); this bounds their logical size and matches the engine's own
/// checkpoint limit, so the store is never the tighter cap.
pub const MAX_PAYLOAD: usize = 32 * 1024 * 1024;
/// Largest logical proposal record: its continuation and up to eight intent inputs.
pub(crate) const MAX_CANONICAL: usize =
    (1 + transition::MAX_EFFECTS) * (MAX_PAYLOAD + 4) + (1 + 4 + transition::MAX_STATE) + 4096;
/// Revisions (and admissions) one store holds. Steps store only what changed (shared
/// chunks), and opening checks only steps newer than its last full check.
pub const MAX_ADMISSIONS: i64 = 1 << 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    pub schema: u32,
    pub transactional_receipts: bool,
    pub revision_pinned_file_maps: bool,
    pub native_execution: bool,
    pub dext_engine: bool,
    pub durable_rewind: bool,
    pub power_loss_certified: bool,
    pub backup_restore: bool,
}

pub const CAPABILITIES: Capabilities = Capabilities {
    schema: SCHEMA as u32,
    transactional_receipts: true,
    revision_pinned_file_maps: true,
    native_execution: false,
    dext_engine: false,
    durable_rewind: false,
    power_loss_certified: false,
    backup_restore: false,
};

/// Host-neutral failure of a model send; no transport type enters the store API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelError {
    Invalid,
    Unauthorized,
    Interrupted,
    Deadline,
    Transport,
    Http(u16),
    Protocol,
    Exhausted,
}

#[cfg(feature = "dext-provider")]
impl From<dext_native::model_effect::AttemptError> for ModelError {
    fn from(error: dext_native::model_effect::AttemptError) -> Self {
        use dext_native::model_effect::AttemptError as Native;
        match error {
            Native::Invalid => Self::Invalid,
            Native::Unauthorized => Self::Unauthorized,
            Native::Interrupted => Self::Interrupted,
            Native::Deadline => Self::Deadline,
            Native::Transport => Self::Transport,
            Native::Http(status) => Self::Http(status),
            Native::Protocol => Self::Protocol,
            Native::Exhausted => Self::Exhausted,
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Sql(rusqlite::Error),
    Locked,
    Unsupported,
    Incompatible,
    Invalid,
    /// A value is larger than the store keeps (`MAX_PAYLOAD`): `size` and `limit` bytes.
    TooLarge {
        size: usize,
        limit: usize,
    },
    Conflict,
    Stale,
    Exhausted,
    Paused,
    Cancelled,
    /// The scope or an owner is suspended; resume before new work is admitted.
    Suspended,
    /// A live service (or an ancestor service) was not re-confirmed in the current owner
    /// epoch; confirm it before new work is admitted under it.
    Unconfirmed,
    #[cfg(feature = "dext-turn")]
    Model(ModelError),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { size, limit } => {
                // Exact bytes when rounding would print the same size twice.
                let (size, limit) = if human_size(*size) == human_size(*limit) {
                    (format!("{size} bytes"), format!("{limit} bytes"))
                } else {
                    (human_size(*size), human_size(*limit))
                };
                write!(
                    f,
                    "this record is {size}, more than the {limit} one step can hold"
                )
            }
            other => write!(f, "{other:?}"),
        }
    }
}

/// A byte count for people: `812 bytes`, `243.1 KiB`, `32.0 MiB`.
pub fn human_size(bytes: usize) -> String {
    match bytes {
        0..1024 => format!("{bytes} bytes"),
        1024..1_048_576 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MiB", bytes as f64 / 1_048_576.0),
        _ => format!("{:.1} GiB", bytes as f64 / 1_073_741_824.0),
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<rusqlite::Error> for Error {
    fn from(value: rusqlite::Error) -> Self {
        if matches!(
            value.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        ) {
            Self::Locked
        } else {
            Self::Sql(value)
        }
    }
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Command {
    pub scope: String,
    pub id: String,
    pub lineage: u64,
    pub kind: String,
    /// Already-canonical semantic bytes, not arbitrary JSON or secret material.
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Admission {
    pub sequence: u64,
    pub lineage: u64,
    pub admitted_epoch: u64,
}

/// A host-owned exclusion guard. Implementors assert that no other authority
/// connection can use the same database until this guard is dropped, including
/// other processes/tabs/workers and readers for an exclusive/no-lock VFS.
/// Native guards are `Send` so a store can move between threads; single-threaded
/// Wasm hosts may hold JavaScript objects such as a Web Lock's release handle.
#[cfg(not(target_family = "wasm"))]
pub trait OwnerExclusion: Send {}
/// A host-owned exclusion guard (see the native definition for the contract).
#[cfg(target_family = "wasm")]
pub trait OwnerExclusion {}

/// Supported SQLite journal contracts; durability also depends on the supplied VFS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalMode {
    Delete,
    Wal,
}

struct OwnedConnection {
    db: Connection,
    owner: OwnerLock,
}

/// A persistent host must distinguish first creation from reopening missing state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Initialization {
    Create,
    Open,
}

/// Memory is always a fresh ephemeral authority. Persistent connections require
/// FULL sync and a host VFS whose flush/atomicity contract the caller has reviewed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionProfile {
    Memory,
    Persistent {
        journal: JournalMode,
        initialization: Initialization,
    },
}

enum OwnerLock {
    /// The flock on `owner.lock`, and the fence this owner holds beside the store
    /// (`FENCE`), removed before the flock is released.
    File(File, Option<std::path::PathBuf>),
    Host {
        _guard: Box<dyn OwnerExclusion>,
    },
}

/// The file whose presence fences a store directory from every host: the embedded
/// package cannot see a flock, so a native owner holds this too while it owns the
/// store, and the package creates it (exclusively) for its own.
pub const FENCE: &str = "authority.sqlite.lock";
/// How a native owner's fence begins. The package prefixes its owners' notes with
/// `spindle-embed: `, so an embedded fence never passes for a native one.
pub const NATIVE_FENCE: &str = "spindle-native ";

/// How the fence of a session that moved elsewhere begins (`Store::seal_moved`, or
/// the package's own sealing): no host opens that copy as the session again.
pub const MOVED_FENCE: &str = "spindle-moved: ";

/// The note of the fence beside the store at `root`, if one is there: who holds the
/// store, or where its session moved.
pub fn fence_note(root: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(File::open(root.join(FENCE)).ok()?, 4096),
        &mut bytes,
    )
    .ok()?;
    Some(String::from_utf8_lossy(&bytes).trim().to_string())
}

/// Opens the existing fence at `path` to rewrite it in place (never following a link).
fn open_fence(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

/// Replaces a fence's note in place: written over the old one, then cut to length and
/// synced. Never truncated first: a crash part way leaves the new note's beginning (or
/// the old note), never an empty fence that no host could tell apart.
fn rewrite_fence(file: &mut File, note: &str) -> std::io::Result<()> {
    use std::io::{Seek, Write};
    file.seek(std::io::SeekFrom::Start(0))?;
    file.write_all(note.as_bytes())?;
    file.set_len(note.len() as u64)?;
    file.sync_all()
}

/// Holds the directory's fence for a native owner that already holds the flock. A
/// fence a native owner left is taken over (that owner is gone: the flock was free);
/// any other (the package's, which nothing can prove dead) refuses: `Locked`.
fn hold_fence(root: &Path) -> Result<std::path::PathBuf> {
    use std::io::Write;
    let path = root.join(FENCE);
    let note = format!("{NATIVE_FENCE}pid {}\n", process_id());
    let mut create = OpenOptions::new();
    create.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        create.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    match create.open(&path) {
        Ok(file) => {
            // The file closes with this closure, before any removal below.
            let written = (move || {
                let mut file = file;
                file.write_all(note.as_bytes())?;
                file.sync_all()
            })()
            .and_then(|()| sync_dir(root));
            if let Err(error) = written {
                // Not left half made: an empty fence would refuse every host.
                let _ = std::fs::remove_file(&path);
                return Err(error.into());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut file = open_fence(&path)?;
            let mut kept = Vec::new();
            std::io::Read::read_to_end(&mut std::io::Read::take(&mut file, 4096), &mut kept)?;
            if !kept.starts_with(NATIVE_FENCE.as_bytes()) {
                return Err(Error::Locked);
            }
            rewrite_fence(&mut file, &note)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(path)
}

/// A diagnostic/temp-name component, never an owner or uniqueness guarantee.
pub fn process_id() -> u32 {
    #[cfg(target_family = "wasm")]
    {
        0
    }
    #[cfg(not(target_family = "wasm"))]
    {
        std::process::id()
    }
}
/// Takes the owner lock, or `Locked` when another owner holds it. A reader that probes
/// the lock (`View::owner_alive`) holds it only for an instant, so a few short retries
/// keep that from refusing a real owner. Anything else that takes a store's lock (a
/// host changing settings or deleting a session) takes it this way too.
pub fn take_owner_lock(lock: &File) -> Result<()> {
    for _ in 0..10 {
        if lock.try_lock().is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(Error::Locked)
}

impl Drop for Store {
    /// Notes a clean close, so the next open does not count this owner as a crash.
    /// Best effort: a store that cannot be written now is counted as one, and so is an
    /// owner unwinding from a panic (a step that panics every time is a crash loop).
    fn drop(&mut self) {
        if !self.paused && !std::thread::panicking() {
            let _ = self.db.execute(
                "UPDATE opens SET closed=1 WHERE id=1 AND epoch=?1",
                [self.opened as i64],
            );
        }
    }
}

impl Drop for OwnerLock {
    fn drop(&mut self) {
        // Close alone leaves flock held by a concurrent fork's pre-exec copy.
        // Store drops SQLite first; explicit unlock releases the shared description.
        if let Self::File(file, fence) = self {
            // The fence goes first, while this owner still holds the flock.
            if let Some(path) = fence.take()
                && std::fs::remove_file(&path).is_ok()
                && let Some(root) = path.parent()
            {
                let _ = sync_dir(root);
            }
            let _ = file.unlock();
        }
    }
}

pub struct Store {
    db: Connection,
    _lock: OwnerLock,
    epoch: u64,
    /// The epoch this owner opened at, which the `opens` row records (a rewind moves
    /// `epoch` on, not this).
    opened: u64,
    paused: bool,
    /// Owners that stopped without closing the store since its head last moved
    /// (counted at open; see `crashes`).
    crashes: u64,
    #[cfg(feature = "dext-image")]
    image_disclosure: image::DisclosureState,
    /// Session-pack ledger records this handle has read or written.
    #[cfg(feature = "dext-strict-workspace")]
    pack_ledger: pack_runtime::Ledger,
}

fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
}
fn bounded_blob(row: &rusqlite::Row<'_>, column: usize, bound: usize) -> rusqlite::Result<Vec<u8>> {
    match row.get_ref(column)? {
        rusqlite::types::ValueRef::Blob(bytes) if bytes.len() <= bound => Ok(bytes.to_vec()),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

/// A compressed column (`codec`): its logical bytes, at most `bound` of them, reading
/// shared chunks from `db`.
fn packed_blob(
    db: &Connection,
    row: &rusqlite::Row<'_>,
    column: usize,
    bound: usize,
) -> rusqlite::Result<Vec<u8>> {
    match row.get_ref(column)? {
        rusqlite::types::ValueRef::Blob(bytes) if bytes.len() <= codec::stored_bound(bound) => {
            codec::load(db, bytes, bound).map_err(|_| rusqlite::Error::InvalidQuery)
        }
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

/// Refuses a value the store cannot keep, saying how large it is.
pub(crate) fn fits(size: usize) -> Result<()> {
    if size > MAX_PAYLOAD {
        return Err(Error::TooLarge {
            size,
            limit: MAX_PAYLOAD,
        });
    }
    Ok(())
}

fn bounded_label(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<String> {
    match row.get_ref(column)? {
        rusqlite::types::ValueRef::Text(bytes) if bytes.len() <= 128 => {
            let value = std::str::from_utf8(bytes).map_err(|_| rusqlite::Error::InvalidQuery)?;
            if !valid_label(value) {
                return Err(rusqlite::Error::InvalidQuery);
            }
            Ok(value.to_owned())
        }
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn optional_digest(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<Option<String>> {
    if row.get_ref(column)? == rusqlite::types::ValueRef::Null {
        return Ok(None);
    }
    let value = bounded_label(row, column)?;
    if !artifact::valid_digest(&value) {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(Some(value))
}

fn nonnegative_id(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<u64> {
    u64::try_from(row.get::<_, i64>(column)?).map_err(|_| rusqlite::Error::InvalidQuery)
}

fn validate(command: &Command) -> Result<()> {
    if !valid_label(&command.scope)
        || !valid_label(&command.id)
        || !valid_label(&command.kind)
        || command.lineage > i64::MAX as u64
    {
        return Err(Error::Invalid);
    }
    fits(command.payload.len())
}

fn execute_one(db: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<()> {
    if db.execute(sql, params)? != 1 {
        return Err(Error::Incompatible);
    }
    Ok(())
}

pub(crate) fn admit_in_transaction(
    tx: &rusqlite::Transaction<'_>,
    owner: u64,
    command: &Command,
) -> Result<Admission> {
    admit_transaction(tx, owner, command, false)
}

#[cfg(feature = "dext-turn")]
pub(crate) fn admit_rewind_in_transaction(
    tx: &rusqlite::Transaction<'_>,
    owner: u64,
    command: &Command,
) -> Result<Admission> {
    if command.kind != rewind::KIND {
        return Err(Error::Invalid);
    }
    admit_transaction(tx, owner, command, true)
}

fn admit_transaction(
    tx: &rusqlite::Transaction<'_>,
    owner: u64,
    command: &Command,
    rewind_control: bool,
) -> Result<Admission> {
    if command.kind == rewind::KIND && !rewind_control {
        return Err(Error::Invalid);
    }
    let (epoch, lineage): (u64, u64) =
        tx.query_row("SELECT epoch,lineage FROM realm WHERE id=1", [], |row| {
            Ok((nonnegative_id(row, 0)?, nonnegative_id(row, 1)?))
        })?;
    if epoch != owner {
        return Err(Error::Stale);
    }
    let existing = tx.query_row("SELECT sequence,lineage,kind,payload,admitted_epoch FROM admissions WHERE scope=?1 AND command_id=?2", params![command.scope,command.id], |row| {
        Ok((nonnegative_id(row, 0)?, nonnegative_id(row, 1)?, bounded_label(row, 2)?, packed_blob(tx, row, 3, MAX_PAYLOAD)?, nonnegative_id(row, 4)?))
    }).optional()?;
    if let Some((sequence, old_lineage, kind, payload, admitted_epoch)) = existing {
        if sequence == 0 || admitted_epoch == 0 || admitted_epoch > epoch {
            return Err(Error::Incompatible);
        }
        if old_lineage != command.lineage || kind != command.kind || payload != command.payload {
            return Err(Error::Conflict);
        }
        return Ok(Admission {
            sequence,
            lineage: old_lineage,
            admitted_epoch,
        });
    }
    if lineage != command.lineage {
        return Err(Error::Stale);
    }
    if !rewind_control {
        ownership::admit_allowed(tx, &command.scope)?;
    }
    let cancelled: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM cancellations WHERE scope=?1)",
        [&command.scope],
        |row| row.get(0),
    )?;
    if cancelled {
        return Err(Error::Cancelled);
    }
    let count: i64 = tx.query_row("SELECT count(*) FROM admissions", [], |row| row.get(0))?;
    if count >= MAX_ADMISSIONS {
        return Err(Error::Exhausted);
    }
    execute_one(
        tx,
        "INSERT INTO admissions(scope,command_id,lineage,kind,payload,admitted_epoch) VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            command.scope,
            command.id,
            command.lineage as i64,
            command.kind,
            codec::store(tx, &command.payload)?,
            epoch as i64
        ],
    )?;
    Ok(Admission {
        sequence: tx.last_insert_rowid() as u64,
        lineage: command.lineage,
        admitted_epoch: epoch,
    })
}

fn initialize_realm(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE realm (id INTEGER PRIMARY KEY CHECK(id=1), epoch INTEGER NOT NULL CHECK(epoch>=0), lineage INTEGER NOT NULL CHECK(lineage>=0));
        INSERT INTO realm VALUES(1,0,0);
        CREATE TABLE admissions (sequence INTEGER PRIMARY KEY AUTOINCREMENT, scope TEXT NOT NULL, command_id TEXT NOT NULL, lineage INTEGER NOT NULL, kind TEXT NOT NULL, payload BLOB NOT NULL, admitted_epoch INTEGER NOT NULL, UNIQUE(scope,command_id));")?;
    artifact::initialize(tx)?;
    transition::initialize(tx)?;
    ownership::initialize(tx)?;
    rewind::initialize(tx)?;
    tx.execute_batch(NEW_TABLES)?;
    tx.execute_batch(TABLES_17)?;
    tx.execute_batch(meta::TABLE)?;
    tx.execute_batch(TABLES_19)?;
    initialize_identity(tx)?;
    Ok(())
}

fn initialize_identity(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let mut identity = [0; 16];
    getrandom::fill(&mut identity)
        .map_err(|error| Error::Io(std::io::Error::other(error.to_string())))?;
    execute_one(
        tx,
        "UPDATE instance SET identity=?1 WHERE id=1",
        [identity.as_slice()],
    )?;
    Ok(())
}

/// The table definitions must be exactly this build's, or schema `from`'s (without the
/// tables its migration adds).
fn validate_schema(db: &Connection, from: i64) -> Result<()> {
    type Definition = (String, String, String, Option<String>);
    fn definitions(db: &Connection) -> Result<Vec<Definition>> {
        let mut query =
            db.prepare("SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name")?;
        let mut rows = query.query([])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            // The fixed schema has fewer than 128 records. Bound malformed schema
            // inspection before allocating attacker-sized SQL definitions.
            if result.len() >= 128 {
                return Err(Error::Incompatible);
            }
            for column in 0..4 {
                let value = row.get_ref(column)?;
                match value {
                    rusqlite::types::ValueRef::Text(bytes) if bytes.len() <= 8192 => {}
                    rusqlite::types::ValueRef::Null if column == 3 => {}
                    _ => return Err(Error::Incompatible),
                }
            }
            result.push((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?));
        }
        Ok(result)
    }
    let mut reference = Connection::open_in_memory()?;
    let tx = reference.transaction()?;
    initialize_realm(&tx)?;
    if from < 19 {
        tx.execute_batch("DROP TABLE task_runs; DROP TABLE retry_delays; DROP TABLE app_details; DROP TABLE instance;")?;
    }
    if from < 18 {
        tx.execute_batch("DROP TABLE meta;")?;
    }
    if from < 17 {
        tx.execute_batch("DROP TABLE app_patches; DROP TABLE opens; DROP TABLE app_state;")?;
    }
    if from < 16 {
        tx.execute_batch(
            "DROP INDEX artifact_stages_scope; DROP INDEX staging_pins_digest; DROP INDEX revisions_workspace; DROP TABLE attempt_failures; DROP TABLE validated; DROP TABLE chunks;",
        )?;
    }
    tx.commit()?;
    if definitions(db)? != definitions(&reference)? {
        return Err(Error::Incompatible);
    }
    Ok(())
}

fn validate_row_types(db: &Connection) -> Result<()> {
    // Fixed schema identifiers only; never interpolate caller-controlled SQL.
    let tables = [
        ("realm", "id epoch lineage", "", "", ""),
        (
            "admissions",
            "sequence lineage admitted_epoch",
            "scope command_id kind",
            "payload",
            "",
        ),
        (
            "revisions",
            "revision",
            "scope engine",
            "continuation",
            "workspace",
        ),
        ("head", "id revision", "", "", ""),
        ("proposals", "revision", "id", "canonical", ""),
        ("consumed", "admission revision", "", "", ""),
        ("intents", "revision position", "id adapter", "input", ""),
        (
            "attempts",
            "owner_epoch",
            "id effect implementation grant_reference",
            "",
            "",
        ),
        ("receipts", "", "attempt", "result", "workspace"),
        ("applied_receipts", "revision", "attempt", "", ""),
        ("cancellations", "epoch", "scope command_id", "", ""),
        ("image_executions", "state", "attempt worker", "", ""),
        ("strict_receipts", "", "attempt base root helper", "", ""),
        (
            "owned_scopes",
            "kind state admission",
            "scope",
            "",
            "parent",
        ),
        ("owned_waits", "", "waiter target", "", ""),
        ("service_confirmations", "epoch", "scope", "", ""),
        ("dropped_admissions", "admission epoch", "command", "", ""),
        (
            "rewound_tasks",
            "source_revision admission",
            "scope source_scope rewind_command workspace",
            "",
            "",
        ),
        (
            "execution_cuts",
            "revision admission epoch",
            "scope",
            "manifest",
            "",
        ),
        (
            "rewinds",
            "admission target old_revision old_lineage epoch state",
            "scope command",
            "",
            "",
        ),
        ("artifact_stages", "owner_epoch", "id scope", "", ""),
        ("objects", "", "digest", "bytes", ""),
        ("staging_pins", "", "stage digest", "", ""),
        ("chunks", "", "", "digest bytes", ""),
        ("validated", "id revision receipt", "", "", ""),
        ("attempt_failures", "status", "attempt", "", ""),
        ("app_state", "revision", "", "value", ""),
        ("app_patches", "", "attempt", "patch", ""),
        ("opens", "id epoch head closed crashes", "", "", ""),
        ("meta", "", "key", "value", ""),
        (
            "task_runs",
            "revision queued",
            "task base state",
            "input",
            "origin",
        ),
        ("instance", "id", "", "identity", ""),
        ("app_details", "", "attempt", "value", ""),
        ("retry_delays", "millis", "attempt", "", ""),
        (
            "image_provenance",
            "width height owner_epoch",
            "scope digest worker source",
            "",
            "",
        ),
    ];
    for (table, integers, labels, blobs, optional_labels) in tables {
        let mut checks = Vec::new();
        for column in integers.split_whitespace() {
            checks.push(format!("typeof({column})!='integer'"));
        }
        for column in blobs.split_whitespace() {
            checks.push(format!("typeof({column})!='blob'"));
        }
        for column in labels
            .split_whitespace()
            .chain(optional_labels.split_whitespace())
        {
            let condition = format!(
                "typeof({column})!='text' OR length(CAST({column} AS BLOB))>128 OR instr({column},char(0))>0 OR {column} GLOB '*[^A-Za-z0-9_.:/-]*' OR length({column})=0"
            );
            let condition = if table == "revisions" && column == "scope" {
                format!(
                    "NOT(revision=0 AND typeof(scope)='text' AND length(CAST(scope AS BLOB))=0) AND ({condition})"
                )
            } else {
                condition
            };
            checks.push(
                if optional_labels
                    .split_whitespace()
                    .any(|value| value == column)
                {
                    format!("{column} IS NOT NULL AND ({condition})")
                } else {
                    format!("({condition})")
                },
            );
        }
        let invalid: bool = db.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM {table} WHERE {})",
                checks.join(" OR ")
            ),
            [],
            |row| row.get(0),
        )?;
        if invalid {
            return Err(Error::Incompatible);
        }
    }
    Ok(())
}

/// How every SQLite database file begins.
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// What a store image holds (`check_image`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageFacts {
    /// Its head revision.
    pub head: u64,
    /// Its call-identity namespace (the instance identity, hex): the same for a
    /// session wherever it moves.
    pub identity: String,
    /// Its settings metadata, if it has any (a session's store always does).
    pub settings: Option<Vec<u8>>,
}

/// Checks a store image completely before a host makes it a session's store: an
/// unencrypted rollback-journal SQLite file of this build's application and schema,
/// every page and record valid (as a full open checks them), with a real identity.
pub fn check_image(image: &[u8]) -> Result<ImageFacts> {
    if image.len() < 100 || !image.starts_with(SQLITE_HEADER) || image[18..20] != [1, 1] {
        return Err(Error::Incompatible);
    }
    let mut db = Connection::open_in_memory()?;
    db.deserialize_read_exact(rusqlite::MAIN_DB, image, image.len(), true)?;
    db.pragma_update(None, "trusted_schema", "OFF")?;
    let app: i64 = db.pragma_query_value(None, "application_id", |r| r.get(0))?;
    let schema: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if app != APP_ID || schema != SCHEMA {
        return Err(Error::Incompatible);
    }
    validate_schema(&db, SCHEMA)?;
    check_pages(&db)?;
    validate_row_types(&db)?;
    validate_all(&db, true)?;
    let head: i64 = db.query_row("SELECT revision FROM head WHERE id=1", [], |r| r.get(0))?;
    let identity: Vec<u8> =
        db.query_row("SELECT identity FROM instance WHERE id=1", [], |r| r.get(0))?;
    if identity.len() != 16 || identity.iter().all(|byte| *byte == 0) {
        return Err(Error::Incompatible);
    }
    Ok(ImageFacts {
        head: u64::try_from(head).map_err(|_| Error::Incompatible)?,
        identity: identity.iter().map(|byte| format!("{byte:02x}")).collect(),
        settings: meta::get(&db, "settings")?,
    })
}

fn check_pages(db: &Connection) -> Result<()> {
    let integrity: String = db.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if integrity != "ok" {
        return Err(Error::Incompatible);
    }
    Ok(())
}

/// Validates the stored records: in full (every chunk, receipt and admission value
/// too), or (on open) only revisions and receipts newer than those recorded in
/// `validated` by the last open; older ones were checked then, and every chunk is
/// checked against its digest whenever it is read. Returns the head revision and newest
/// receipt row the store now covers.
fn validate_all(db: &Connection, full: bool) -> Result<(i64, i64)> {
    let (since, receipts_since): (i64, i64) = if full {
        (0, 0)
    } else {
        db.query_row(
            "SELECT revision,receipt FROM validated WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?
    };
    let invalid: i64 = db.query_row(
        "SELECT (SELECT count(*) FROM chunks WHERE length(digest)!=32 OR length(bytes)>?1 OR length(bytes)<2)
              + (SELECT count(*) FROM attempt_failures f LEFT JOIN receipts q ON q.attempt=f.attempt WHERE q.attempt IS NOT NULL OR f.status NOT BETWEEN 100 AND 599 OR typeof(f.detail)!='text' OR length(CAST(f.detail AS BLOB))>4096)
              + (SELECT count(*) FROM validated WHERE id!=1 OR revision<0 OR receipt<0 OR revision>(SELECT revision FROM head WHERE id=1))
              + (SELECT count(*) FROM app_state s WHERE s.revision<=0 OR length(s.value)>?2 OR NOT EXISTS(SELECT 1 FROM revisions r WHERE r.revision=s.revision))
              + (SELECT count(*) FROM opens WHERE epoch>(SELECT epoch FROM realm WHERE id=1))
              + (1-(SELECT count(*) FROM opens))
              + (1-(SELECT count(*) FROM instance WHERE id=1 AND length(identity)=16 AND identity!=zeroblob(16)))
              + (SELECT count(*) FROM retry_delays WHERE millis<0)
              + (SELECT count(*) FROM app_details d LEFT JOIN attempts a ON a.id=d.attempt WHERE a.id IS NULL OR substr(a.implementation,1,20)!='spindle-app-tool-v1-' OR length(d.value)>?2)
              + (SELECT count(*) FROM app_patches p LEFT JOIN attempts a ON a.id=p.attempt WHERE a.id IS NULL OR substr(a.implementation,1,20)!='spindle-app-tool-v1-' OR length(p.patch)>?2)",
        params![
            codec::stored_bound(codec::CHUNK_MAX) as i64,
            codec::stored_bound(transition::MAX_STATE) as i64
        ],
        |r| r.get(0),
    )?;
    if invalid != 0 {
        return Err(Error::Incompatible);
    }
    if full {
        // Every chunk against its digest, and the stored values no other check decodes.
        let mut chunks = db.prepare("SELECT digest,bytes FROM chunks")?;
        let mut rows = chunks.query([])?;
        while let Some(row) = rows.next()? {
            let digest: Vec<u8> = row.get(0)?;
            codec::verify_chunk(
                &digest,
                &bounded_blob(row, 1, codec::stored_bound(codec::CHUNK_MAX))?,
            )?;
        }
        drop(rows);
        drop(chunks);
        for (table, column) in [("receipts", "result"), ("admissions", "payload")] {
            // Fixed schema identifiers only.
            let mut values = db.prepare(&format!("SELECT {column} FROM {table}"))?;
            let mut rows = values.query([])?;
            while let Some(row) = rows.next()? {
                packed_blob(db, row, 0, MAX_PAYLOAD)?;
            }
        }
    }
    meta::validate(db)?;
    meta::validate_tasks(db)?;
    transition::validate_storage(db, since, receipts_since)?;
    let mut details = db.prepare("SELECT value FROM app_details")?;
    let mut rows = details.query([])?;
    while let Some(row) = rows.next()? {
        let bytes = packed_blob(db, row, 0, transition::MAX_STATE)?;
        #[cfg(feature = "dext-turn")]
        serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|_| Error::Incompatible)?;
        #[cfg(not(feature = "dext-turn"))]
        let _ = bytes;
    }
    drop(rows);
    drop(details);
    artifact::validate_storage(db)?;
    ownership::validate_storage(db)?;
    rewind::validate_storage(db)?;
    Ok(db.query_row(
        "SELECT (SELECT revision FROM head WHERE id=1),(SELECT coalesce(max(rowid),0) FROM receipts)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

/// Columns holding compressed values (`codec`) and the logical bound of each.
const PACKED_COLUMNS: [(&str, &str, &str, usize); 5] = [
    ("admissions", "sequence", "payload", MAX_PAYLOAD),
    ("revisions", "revision", "continuation", MAX_PAYLOAD),
    ("proposals", "id", "canonical", MAX_CANONICAL),
    ("intents", "id", "input", MAX_PAYLOAD),
    ("receipts", "attempt", "result", MAX_PAYLOAD),
];

/// Schema 14, 15 or 16 -> 17: re-encodes every large value in place (chunked and
/// shared) when `from` is older than 16, adds the tables each later schema brought and
/// validates every record, in one transaction, after copying the store to
/// `backup_name(from)`. Values are only re-encoded, never changed, so the validation
/// sees exactly the data the old schema held.
fn migrate_legacy(db: &mut Connection, root: &Path, from: i64) -> Result<()> {
    let backup = root.join(backup_name(from));
    let staged = root.join(format!("{}.new", backup_name(from)));
    match std::fs::remove_file(&staged) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    {
        let _file = options.open(&staged)?;
    }
    let target = staged.to_str().ok_or(Error::Unsupported)?;
    db.execute("VACUUM INTO ?1", [target])?;
    // Writable: Windows flushes a file only through a handle that may write it.
    OpenOptions::new().write(true).open(&staged)?.sync_all()?;
    std::fs::rename(&staged, &backup)?;
    sync_dir(root)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let schema: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if schema != from {
        return Err(Error::Incompatible);
    }
    if from < 16 {
        tx.execute_batch(NEW_TABLES)?;
    }
    if from < 17 {
        tx.execute_batch(TABLES_17)?;
    }
    if from < 18 {
        tx.execute_batch(meta::TABLE)?;
    }
    tx.execute_batch(TABLES_19)?;
    initialize_identity(&tx)?;
    // 16 already stores values packed; older schemas re-encode each one.
    let packed = if from < 16 { &PACKED_COLUMNS[..] } else { &[] };
    for (table, key, column, bound) in packed {
        // Fixed schema identifiers only; never interpolate caller-controlled SQL.
        let keys: Vec<rusqlite::types::Value> = tx
            .prepare(&format!("SELECT {key} FROM {table}"))?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in keys {
            let value = tx.query_row(
                &format!("SELECT {column} FROM {table} WHERE {key}=?1"),
                [&id],
                |row| match from {
                    14 => bounded_blob(row, 0, *bound),
                    _ => packed_blob(&tx, row, 0, *bound),
                },
            )?;
            execute_one(
                &tx,
                &format!("UPDATE {table} SET {column}=?1 WHERE {key}=?2"),
                params![codec::store(&tx, &value)?, id],
            )?;
        }
    }
    tx.pragma_update(None, "user_version", SCHEMA)?;
    // Every record is validated before the migration commits: a store that fails stays
    // as it was (the old build still opens it), never half-way at the new schema.
    validate_row_types(&tx)?;
    let (revision, receipt) = validate_all(&tx, true)?;
    execute_one(
        &tx,
        "UPDATE validated SET revision=?1,receipt=?2 WHERE id=1",
        params![revision, receipt],
    )?;
    tx.commit()?;
    // Hand the space the uncompressed values held back to the filesystem, once. Best
    // effort: the migration is committed, and a full temp directory must not refuse it.
    // A schema-16 store re-encoded nothing, so there is nothing to give back.
    if from < 16 {
        let _ = db.execute_batch("VACUUM");
    }
    Ok(())
}

/// Whether `root` holds an older-schema store that the next `Store::open` migrates.
/// Read only; a missing or foreign store is not legacy.
pub fn legacy_store(root: &Path) -> bool {
    Connection::open_with_flags(
        root.join("authority.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .and_then(|db| {
        Ok(
            db.pragma_query_value(None, "application_id", |row| row.get::<_, i64>(0))? == APP_ID
                && LEGACY_SCHEMAS.contains(&db.pragma_query_value(
                    None,
                    "user_version",
                    |row| row.get::<_, i64>(0),
                )?),
        )
    })
    .unwrap_or(false)
}

/// Makes a directory's entries durable. Unix syncs the directory itself; Windows
/// cannot open directories as files, and NTFS journals metadata, so it is a no-op.
fn sync_dir(path: &Path) -> std::io::Result<()> {
    if cfg!(unix) {
        File::open(path)?.sync_all()
    } else {
        Ok(())
    }
}

/// Builds a new, initialized store beside `root` and renames the whole directory into
/// place, so a crash leaves no store or a complete one: never an empty or half-made
/// directory, which `open` must refuse because it cannot tell it from a store that lost
/// its files. Another process creating the same store first is not an error.
/// Staging directories a crash left behind (their lock is free) are removed first.
fn create_store(root: &Path) -> Result<()> {
    static STAGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = root.parent().ok_or(Error::Invalid)?;
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(Error::Invalid)?;
    let prefix = format!(".{name}.new-");
    for entry in std::fs::read_dir(parent)?.flatten() {
        let path = entry.path();
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(&prefix))
            && let Ok(lock) = File::open(path.join("owner.lock"))
            && lock.try_lock().is_ok()
        {
            let _ = std::fs::remove_dir_all(path);
            let _ = lock.unlock();
        }
    }
    let staged = parent.join(format!(
        "{prefix}{}-{}",
        process_id(),
        STAGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&staged)?;
    let built = (|| -> Result<File> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(staged.join("owner.lock"))?;
        take_owner_lock(&lock)?;
        let mut db = Connection::open_with_flags(
            staged.join("authority.sqlite"),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        db.pragma_update(None, "trusted_schema", "OFF")?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        durable::full_flushes(&db)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        initialize_realm(&tx)?;
        tx.pragma_update(None, "application_id", APP_ID)?;
        tx.pragma_update(None, "user_version", SCHEMA)?;
        tx.commit()?;
        // Closing folds the WAL into the database file; either way both are synced.
        db.close().map_err(|(_, error)| error)?;
        lock.sync_all()?;
        sync_dir(&staged)?;
        Ok(lock)
    })();
    let lock = match built {
        Ok(lock) => lock,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(error);
        }
    };
    // Unlocked, not just closed: a concurrent fork's pre-exec copy shares the lock.
    let lock = OwnerLock::File(lock, None);
    // Windows cannot rename a directory that holds an open file.
    #[cfg(windows)]
    drop(lock);
    let renamed = std::fs::rename(&staged, root);
    #[cfg(not(windows))]
    drop(lock);
    match renamed {
        Ok(()) => sync_dir(parent)?,
        Err(_) if std::fs::symlink_metadata(root).is_ok() => {
            let _ = std::fs::remove_dir_all(&staged);
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(error.into());
        }
    }
    Ok(())
}

impl Store {
    /// Local-filesystem profile. The caller owns a private, non-shared directory
    /// (enforced by mode on Unix; Windows relies on the per-user profile ACLs).
    /// Same-user hostile processes and dishonest flushes are out of scope.
    pub fn open(root: &Path) -> Result<Self> {
        if cfg!(target_family = "wasm") {
            return Err(Error::Unsupported);
        }
        let root = if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir()?.join(root)
        };
        let root = root.as_path();
        match std::fs::symlink_metadata(root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => create_store(root)?,
            _ => {}
        }
        let mut created = false;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            match std::fs::DirBuilder::new().mode(0o700).create(root) {
                Ok(()) => {
                    created = true;
                    sync_dir(root.parent().ok_or(Error::Invalid)?)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            let meta = std::fs::symlink_metadata(root)?;
            if !meta.is_dir()
                || meta.file_type().is_symlink()
                || meta.permissions().mode() & 0o077 != 0
            {
                return Err(Error::Invalid);
            }
        }
        #[cfg(not(unix))]
        {
            match std::fs::create_dir(root) {
                Ok(()) => created = true,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            let meta = std::fs::symlink_metadata(root)?;
            if !meta.is_dir() || meta.file_type().is_symlink() {
                return Err(Error::Invalid);
            }
        }
        for name in [
            "owner.lock",
            FENCE,
            "authority.sqlite",
            "authority.sqlite-wal",
            "authority.sqlite-shm",
            "authority.sqlite-journal",
        ] {
            match std::fs::symlink_metadata(root.join(name)) {
                Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() => {
                    return Err(Error::Invalid);
                }
                #[cfg(unix)]
                Ok(meta) => {
                    use std::os::unix::fs::MetadataExt;
                    if meta.nlink() != 1 {
                        return Err(Error::Invalid);
                    }
                }
                #[cfg(not(unix))]
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(created)
            .truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(root.join("owner.lock"))?;
        take_owner_lock(&lock)?;
        let mut lock = OwnerLock::File(lock, None);
        // The package's owners hold the fence alone (refused here); this owner holds it
        // too, so the package refuses this store while it is open here.
        let fence = hold_fence(root)?;
        if let OwnerLock::File(_, held) = &mut lock {
            *held = Some(fence);
        }
        let flags = if created {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let mut db = Connection::open_with_flags(root.join("authority.sqlite"), flags)?;
        db.busy_timeout(Duration::from_millis(250))?;
        let app: i64 = db.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let schema: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let tables: i64 = db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        let fresh = app == 0 && schema == 0 && tables == 0;
        let legacy = !fresh && app == APP_ID && LEGACY_SCHEMAS.contains(&schema);
        if (fresh && !created) || (!fresh && (app != APP_ID || (schema != SCHEMA && !legacy))) {
            return Err(Error::Incompatible);
        }
        db.pragma_update(None, "trusted_schema", "OFF")?;
        if !fresh {
            validate_schema(&db, if legacy { schema } else { SCHEMA })?;
        }
        let mode: String = db.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        durable::full_flushes(&db)?;
        let sync: i64 = db.pragma_query_value(None, "synchronous", |r| r.get(0))?;
        let foreign: i64 = db.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
        if mode != "wal" || sync != 2 || foreign != 1 {
            return Err(Error::Unsupported);
        }
        if legacy {
            // A migration reads every page anyway: check them first.
            check_pages(&db)?;
            migrate_legacy(&mut db, root, schema)?;
        }
        if fresh {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            initialize_realm(&tx)?;
            tx.pragma_update(None, "application_id", APP_ID)?;
            tx.pragma_update(None, "user_version", SCHEMA)?;
            tx.commit()?;
        }
        Self::take_over(OwnedConnection { db, owner: lock }, Some(root))
    }

    /// The host must retain exclusive ownership of this database for the returned
    /// handle's lifetime. Dropping `owner` releases that exclusion only after SQLite
    /// closes. This contract is not implemented by SQLite's no-lock WASI VFS.
    /// Existing stores must already use the current schema; migration and backups
    /// remain a native-path operation. Persistent VFS sync semantics are host-owned.
    pub fn from_connection(
        db: Connection,
        profile: ConnectionProfile,
        owner: impl OwnerExclusion + 'static,
    ) -> Result<Self> {
        Self::from_connection_holding(db, profile, owner, false)
    }

    /// A memory authority holding `image` (a store image, checked as `check_image`
    /// checks it), owned by `owner`: a moved session opened where nothing persists.
    pub fn from_image(image: &[u8], owner: impl OwnerExclusion + 'static) -> Result<Self> {
        check_image(image)?;
        let mut db = Connection::open_in_memory()?;
        db.deserialize_read_exact(rusqlite::MAIN_DB, image, image.len(), false)?;
        Self::from_connection_holding(db, ConnectionProfile::Memory, owner, true)
    }

    /// `from_connection`; `image`: a memory connection that already holds a store.
    fn from_connection_holding(
        db: Connection,
        profile: ConnectionProfile,
        owner: impl OwnerExclusion + 'static,
        image: bool,
    ) -> Result<Self> {
        let mut pending = OwnedConnection {
            db,
            owner: OwnerLock::Host {
                _guard: Box::new(owner),
            },
        };
        let db = &mut pending.db;
        if !db.is_autocommit() {
            return Err(Error::Invalid);
        }
        db.busy_timeout(Duration::ZERO)?;
        db.pragma_update(None, "trusted_schema", "OFF")?;
        // WASI's sample VFS has no locks; the host guard is authoritative. Establish
        // SQLite's connection mode before its first schema/page read nevertheless.
        let locking: String =
            db.pragma_update_and_check(None, "locking_mode", "EXCLUSIVE", |r| r.get(0))?;
        if locking != "exclusive" {
            return Err(Error::Unsupported);
        }
        let databases = db
            .prepare("PRAGMA database_list")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if databases
            .iter()
            .any(|name| name != "main" && name != "temp")
            || db.query_row("SELECT count(*) FROM sqlite_temp_schema", [], |row| {
                row.get::<_, i64>(0)
            })? != 0
        {
            return Err(Error::Invalid);
        }
        let file_backed = db.path().is_some_and(|path| !path.is_empty());
        if matches!(profile, ConnectionProfile::Memory) == file_backed {
            return Err(Error::Invalid);
        }
        let app: i64 = db.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let schema: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let tables: i64 = db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        let fresh = app == 0 && schema == 0 && tables == 0;
        let create = !image
            && matches!(
                profile,
                ConnectionProfile::Memory
                    | ConnectionProfile::Persistent {
                        initialization: Initialization::Create,
                        ..
                    }
            );
        if fresh != create || (!fresh && (app != APP_ID || schema != SCHEMA)) {
            return Err(Error::Incompatible);
        }
        if !fresh {
            validate_schema(db, SCHEMA)?;
            check_pages(db)?;
        }
        let journal = match profile {
            ConnectionProfile::Memory => "MEMORY",
            ConnectionProfile::Persistent {
                journal: JournalMode::Delete,
                ..
            } => "DELETE",
            ConnectionProfile::Persistent {
                journal: JournalMode::Wal,
                ..
            } => "WAL",
        };
        let mode: String =
            db.pragma_update_and_check(None, "journal_mode", journal, |r| r.get(0))?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        db.pragma_update(None, "temp_store", "MEMORY")?;
        durable::full_flushes(db)?;
        let sync: i64 = db.pragma_query_value(None, "synchronous", |r| r.get(0))?;
        let foreign: i64 = db.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case(journal) || sync != 2 || foreign != 1 {
            return Err(Error::Unsupported);
        }
        if fresh {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            initialize_realm(&tx)?;
            tx.pragma_update(None, "application_id", APP_ID)?;
            tx.pragma_update(None, "user_version", SCHEMA)?;
            tx.commit()?;
        }
        Self::take_over(pending, None)
    }

    fn take_over(mut pending: OwnedConnection, root: Option<&Path>) -> Result<Self> {
        let db = &mut pending.db;
        let (epoch, crashes) = (|| -> Result<(u64, u64)> {
            validate_row_types(db)?;
            let realm_rows: i64 = db.query_row("SELECT count(*) FROM realm", [], |r| r.get(0))?;
            let invalid: i64 = db.query_row(
            "SELECT count(*) FROM admissions WHERE sequence<=0 OR lineage<0 OR admitted_epoch<=0 OR admitted_epoch>(SELECT epoch FROM realm WHERE id=1) OR length(payload)>?1 OR length(scope) NOT BETWEEN 1 AND 128 OR length(command_id) NOT BETWEEN 1 AND 128 OR length(kind) NOT BETWEEN 1 AND 128",
            [codec::stored_bound(MAX_PAYLOAD) as i64], |r| r.get(0),
        )?;
            let admissions: i64 =
                db.query_row("SELECT count(*) FROM admissions", [], |r| r.get(0))?;
            let max_admission: i64 = db.query_row(
                "SELECT coalesce(max(sequence),0) FROM admissions",
                [],
                |r| r.get(0),
            )?;
            let admission_watermark: i64 = db.query_row(
                "SELECT coalesce((SELECT seq FROM sqlite_sequence WHERE name='admissions'),0)",
                [],
                |r| r.get(0),
            )?;
            if realm_rows != 1
                || invalid != 0
                || admissions > MAX_ADMISSIONS
                || admissions != max_admission
                || max_admission != admission_watermark
            {
                return Err(Error::Incompatible);
            }
            let (revision, receipt) = validate_all(db, false)?;
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let epoch: i64 =
                tx.query_row("SELECT epoch FROM realm WHERE id=1", [], |r| r.get(0))?;
            if epoch < 0 || epoch == i64::MAX {
                return Err(Error::Exhausted);
            }
            execute_one(&tx, "UPDATE realm SET epoch=epoch+1 WHERE id=1", [])?;
            execute_one(
                &tx,
                "UPDATE validated SET revision=?1,receipt=?2 WHERE id=1",
                params![revision, receipt],
            )?;
            // An owner that stopped without closing the store (killed, crashed, its host
            // evicted) at the head this open finds again made no progress: count it.
            let head: i64 =
                tx.query_row("SELECT revision FROM head WHERE id=1", [], |r| r.get(0))?;
            let (last_head, closed, crashes): (i64, i64, i64) = tx.query_row(
                "SELECT head,closed,crashes FROM opens WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let crashes = match (head == last_head, closed) {
                (false, _) => 0,
                (true, 0) => crashes.saturating_add(1),
                (true, _) => crashes,
            };
            execute_one(
                &tx,
                "UPDATE opens SET epoch=?1,head=?2,closed=0,crashes=?3 WHERE id=1",
                params![epoch + 1, head, crashes],
            )?;
            tx.commit()?;
            if let Some(root) = root {
                sync_dir(root)?;
            }
            Ok(((epoch + 1) as u64, crashes.max(0) as u64))
        })()?;
        Ok(Self {
            db: pending.db,
            _lock: pending.owner,
            epoch,
            opened: epoch,
            paused: false,
            crashes,
            #[cfg(feature = "dext-image")]
            image_disclosure: image::DisclosureState::default(),
            #[cfg(feature = "dext-strict-workspace")]
            pack_ledger: pack_runtime::Ledger::default(),
        })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Moves this owner to a new epoch on its open connection, as reopening would:
    /// reservations its earlier operations left without a receipt become an earlier
    /// owner's, which the driver seals or asks about again instead of finding them in
    /// flight. A host that keeps one store open across operations (the embedded
    /// package) calls this before each turn-like operation. Exclusion is unchanged.
    pub fn renew_epoch(&mut self) -> Result<u64> {
        if self.paused {
            return Err(Error::Paused);
        }
        let result = (|| -> Result<u64> {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let epoch: i64 =
                tx.query_row("SELECT epoch FROM realm WHERE id=1", [], |r| r.get(0))?;
            if epoch != self.epoch as i64 {
                return Err(Error::Stale);
            }
            if epoch == i64::MAX {
                return Err(Error::Exhausted);
            }
            execute_one(&tx, "UPDATE realm SET epoch=epoch+1 WHERE id=1", [])?;
            tx.commit()?;
            Ok((epoch + 1) as u64)
        })();
        if matches!(
            result,
            Err(Error::Sql(_) | Error::Io(_) | Error::Locked | Error::Incompatible)
        ) {
            self.paused = true;
        }
        self.epoch = result?;
        #[cfg(feature = "dext-image")]
        self.revoke_image_disclosures();
        Ok(self.epoch)
    }

    /// A consistent copy of this whole store as one SQLite database file, taken
    /// through the owner's connection (a WAL's frames included, nothing left to
    /// replay: the image is a rollback-journal database). What a session bundle
    /// carries. Refused inside a transaction.
    pub fn image(&self) -> Result<Vec<u8>> {
        self.image_with(&[])
    }

    /// `image`, with `metadata` (key, value) in the copy instead of this store's: a
    /// session moving to a host it runs differently on (other settings) carries them,
    /// while this store keeps its own.
    pub fn image_with(&self, metadata: &[(&str, &[u8])]) -> Result<Vec<u8>> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !self.db.is_autocommit() {
            return Err(Error::Invalid);
        }
        let mut image = self.db.serialize(rusqlite::MAIN_DB)?.to_vec();
        if image.len() < 100 || !image.starts_with(SQLITE_HEADER) {
            return Err(Error::Incompatible);
        }
        // File format versions 1/1: a rollback-journal database (2/2 is WAL, which an
        // image never needs: it holds every committed page itself).
        image[18] = 1;
        image[19] = 1;
        // The copy's owner is not this one: it records a clean close, so whoever opens
        // the copy counts no crash for this owner, which is still open here.
        let mut copy = Connection::open_in_memory()?;
        copy.deserialize_read_exact(rusqlite::MAIN_DB, image.as_slice(), image.len(), false)?;
        copy.execute("UPDATE opens SET closed=1 WHERE id=1", [])?;
        for (key, value) in metadata {
            meta::put(&copy, key, value)?;
        }
        let image = copy.serialize(rusqlite::MAIN_DB)?.to_vec();
        Ok(image)
    }

    /// Marks this store's session as moved elsewhere (exported to a bundle it
    /// continues from): the fence this native owner holds stays after it closes,
    /// noted `spindle-moved: <note>`, so no host opens this copy as the session again
    /// (removing the fence makes it one: an operator's choice). The store refuses all
    /// further work here. Native owners only (`Unsupported` otherwise).
    pub fn seal_moved(&mut self, note: &str) -> Result<()> {
        let OwnerLock::File(_, fence) = &mut self._lock else {
            return Err(Error::Unsupported);
        };
        let path = fence.clone().ok_or(Error::Invalid)?;
        rewrite_fence(&mut open_fence(&path)?, &format!("{MOVED_FENCE}{note}\n"))?;
        if let Some(root) = path.parent() {
            sync_dir(root)?;
        }
        // Kept when this owner closes; and nothing more happens here.
        *fence = None;
        self.paused = true;
        Ok(())
    }

    /// How many owners stopped without closing this store since its head last moved
    /// (0 once a step commits; a clean close neither adds to it nor resets it). A count
    /// that keeps growing is a crash loop: whatever runs next at this head keeps
    /// stopping its owner.
    pub fn crashes(&self) -> u64 {
        self.crashes
    }

    /// The app state of `scope` as of `revision` (a JSON object; `{}` before any).
    pub fn app_state(&self, scope: &str, revision: u64) -> Result<Vec<u8>> {
        if self.paused {
            return Err(Error::Paused);
        }
        transition::app_state_at(&self.db, scope, revision)
    }

    /// The full check `doctor` runs: every database page, then every record including
    /// those an earlier open already validated.
    pub fn check_integrity(&mut self) -> Result<()> {
        if self.paused {
            return Err(Error::Paused);
        }
        check_pages(&self.db)?;
        validate_all(&self.db, true).map(|_| ())
    }

    /// Current realm lineage; every new admission must carry it.
    pub fn lineage(&mut self) -> Result<u64> {
        if self.paused {
            return Err(Error::Paused);
        }
        let result = self
            .db
            .query_row("SELECT lineage FROM realm WHERE id=1", [], |row| {
                nonnegative_id(row, 0)
            });
        result.map_err(|error| {
            self.paused = true;
            error.into()
        })
    }

    /// A returned receipt means admission, not execution, publication or completion.
    /// Any SQL failure fences this handle. Reopen and retry the SAME identity; a
    /// transaction error alone cannot prove whether a commit was persisted.
    pub fn admit(&mut self, command: &Command) -> Result<Admission> {
        if self.paused {
            return Err(Error::Paused);
        }
        validate(command)?;
        let result = self.admit_transaction(command);
        if matches!(
            result,
            Err(Error::Sql(_) | Error::Io(_) | Error::Locked | Error::Incompatible)
        ) {
            self.paused = true;
        }
        result
    }

    /// The payload of an admitted command, by scope and command id, if it was admitted.
    pub fn admitted(&mut self, scope: &str, id: &str) -> Result<Option<Vec<u8>>> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(scope) || !valid_label(id) {
            return Err(Error::Invalid);
        }
        let result = self
            .db
            .query_row(
                "SELECT payload FROM admissions WHERE scope=?1 AND command_id=?2",
                params![scope, id],
                |row| packed_blob(&self.db, row, 0, MAX_PAYLOAD),
            )
            .optional();
        result.map_err(|error| {
            self.paused = true;
            error.into()
        })
    }

    fn admit_transaction(&mut self, command: &Command) -> Result<Admission> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let admission = admit_in_transaction(&tx, self.epoch, command)?;
        tx.commit()?;
        Ok(admission)
    }
}
