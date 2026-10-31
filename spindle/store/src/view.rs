//! Read-only view of a session store for observers such as `spindle serve`. It holds
//! no owner lock, so a UI can read committed history while another process (or
//! thread) owns the session and runs a turn; only `owner_alive` takes the lock, for an
//! instant, when it is free. `View::open` is query-only; `ReadView::from_store` borrows
//! the owner's connection. Native accounting retains metadata through that connection
//! or a separate narrow host transaction, never through a query-only connection.
use crate::rewind::RewindTargetKind;
use crate::workspace::Entry;
use crate::{Error, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::path::Path;
use std::time::Duration;

/// The scope every `spindle` session runs in.
pub const SCOPE: &str = "operator";

/// Upper bound on steps returned by one `steps` call.
const MAX_STEPS: i64 = 8192;

pub type View = ReadView<'static>;

pub struct ReadView<'a> {
    db: ViewConnection<'a>,
    /// The store directory (its owner lock tells whether an owner is alive).
    root: Option<std::path::PathBuf>,
    /// A schema-14 store not yet migrated by its owner keeps values uncompressed.
    legacy: bool,
    /// Its objects have their `size` column (schema 20: some are kept deflated).
    sized: bool,
}

enum ViewConnection<'a> {
    Owned(Connection),
    Borrowed(&'a Connection),
}

impl std::ops::Deref for ViewConnection<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        match self {
            Self::Owned(db) => db,
            Self::Borrowed(db) => db,
        }
    }
}

/// How the store keeps its records: (whether its values are uncompressed (schema 14),
/// whether its objects have their `size` column (schema 20)); refuses anything else.
fn encoding(db: &Connection) -> Result<(bool, bool)> {
    let app: i64 = db.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let schema: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match (app == crate::APP_ID, schema) {
        (true, crate::SCHEMA) => Ok((false, true)),
        // Schema 15 values decode as schema 16 ones (no chunks yet); 14 kept them raw.
        // 16 differs from 17 only by tables it lacks (no app state, read as empty).
        // Before 20 every object is kept as it is.
        (true, 15..=19) => Ok((false, false)),
        (true, 14) => Ok((true, false)),
        _ => Err(Error::Incompatible),
    }
}

/// A continuation, payload or input column in either encoding.
fn stored(
    db: &Connection,
    legacy: bool,
    row: &rusqlite::Row<'_>,
    column: usize,
) -> rusqlite::Result<Vec<u8>> {
    if legacy {
        crate::bounded_blob(row, column, crate::MAX_PAYLOAD)
    } else {
        crate::packed_blob(db, row, column, crate::MAX_PAYLOAD)
    }
}

/// Manifests decoded lately, by digest (across threads: the web host opens a view
/// per request). A manifest is its digest, so a tree kept here is the one decoding it
/// again would give; the larger ones are decoded each time rather than kept.
static DECODED: std::sync::Mutex<Vec<(String, std::sync::Arc<crate::workspace::Tree>)>> =
    std::sync::Mutex::new(Vec::new());
const DECODED_KEPT: usize = 2;
const DECODED_MAX: usize = 8 * 1024 * 1024;

fn decoded(root: &str) -> Option<std::sync::Arc<crate::workspace::Tree>> {
    let kept = DECODED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    kept.iter()
        .find(|(digest, _)| digest == root)
        .map(|(_, tree)| tree.clone())
}

fn remember(root: &str, manifest: usize, tree: &std::sync::Arc<crate::workspace::Tree>) {
    if manifest > DECODED_MAX {
        return;
    }
    let mut kept = DECODED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    kept.retain(|(digest, _)| digest != root);
    kept.truncate(DECODED_KEPT - 1);
    kept.insert(0, (root.to_owned(), tree.clone()));
}

/// One committed revision as recorded.
#[derive(Clone, Debug)]
pub struct Step {
    pub revision: u64,
    /// Proposal id of the commit, such as `commit-prepare-r4` or `adopt-r5`.
    pub proposal: String,
    /// Adapter whose receipt this revision adopted (`dext-model-v1` or a tool name).
    pub adapter: Option<String>,
}

/// A finished rewind: `revision` continues from `target`, and the revisions after
/// `target` up to `abandoned` left the selected branch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fork {
    pub revision: u64,
    pub target: u64,
    pub abandoned: u64,
}

/// The revision `revision` continues from: a finished rewind's target, else the
/// revision before it. Always smaller, so a walk down a line ends.
fn predecessor(revision: u64, forks: &std::collections::BTreeMap<u64, u64>) -> u64 {
    forks
        .get(&revision)
        .copied()
        .filter(|target| *target < revision)
        .unwrap_or(revision.saturating_sub(1))
}

/// One workspace file at a revision.
#[derive(Clone, Debug)]
pub struct File {
    pub entry: Entry,
    pub size: u64,
}

pub struct JobEffect {
    pub id: String,
    pub adapter: String,
    pub input: Vec<u8>,
    pub result: Vec<u8>,
}

impl ReadView<'static> {
    /// Open the store directory of a session (`<session>/store`) for reading.
    pub fn open(store: &Path) -> Result<Self> {
        let meta = std::fs::symlink_metadata(store)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(Error::Invalid);
        }
        let db = Connection::open_with_flags(
            store.join("authority.sqlite"),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.busy_timeout(Duration::from_millis(500))?;
        db.pragma_update(None, "query_only", "ON")?;
        db.pragma_update(None, "trusted_schema", "OFF")?;
        let (legacy, sized) = encoding(&db)?;
        Ok(Self {
            db: ViewConnection::Owned(db),
            root: Some(store.to_path_buf()),
            legacy,
            sized,
        })
    }
}

impl<'a> ReadView<'a> {
    /// A read-only facade over the owner's existing connection. It never changes
    /// pragmas or starts/rolls back a transaction on behalf of that owner.
    pub fn from_store(store: &'a crate::Store) -> Self {
        Self {
            db: ViewConnection::Borrowed(&store.db),
            root: None,
            legacy: false,
            sized: true,
        }
    }

    /// Run several reads against one consistent snapshot. The encoding is read inside
    /// it, so an owner migrating the store meanwhile is seen before or after, whole.
    pub fn read<T>(&mut self, reads: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if matches!(self.db, ViewConnection::Borrowed(_)) {
            return reads(self);
        }
        self.db.execute_batch("BEGIN")?;
        let result = encoding(&self.db).and_then(|(legacy, sized)| {
            self.legacy = legacy;
            self.sized = sized;
            reads(self)
        });
        let _ = self.db.execute_batch("ROLLBACK");
        result
    }

    /// `artifact::STORED`, as a store from before schema 20 answers it (no `size`).
    fn stored_query(&self) -> &'static str {
        if self.sized {
            crate::artifact::STORED
        } else {
            "SELECT bytes,NULL FROM objects WHERE digest=?1"
        }
    }

    /// `artifact::SIZE`, as a store from before schema 20 answers it.
    fn size_query(&self) -> &'static str {
        if self.sized {
            crate::artifact::SIZE
        } else {
            "SELECT length(bytes) FROM objects WHERE digest=?1"
        }
    }

    /// Configuration/accounting bytes, without acquiring the owner lock.
    pub fn metadata(&mut self, key: &str) -> Result<Option<Vec<u8>>> {
        let schema: i64 = self
            .db
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if schema < 18 {
            return Ok(None);
        }
        crate::meta::get(&self.db, key)
    }

    #[cfg(any(feature = "agent", feature = "embedded"))]
    pub(crate) fn retain_metadata(&self, key: &str, bytes: &[u8]) -> Result<()> {
        match &self.db {
            ViewConnection::Borrowed(db) => {
                let tx = db.unchecked_transaction()?;
                crate::meta::put(&tx, key, bytes)?;
                tx.commit()?;
            }
            // An app reads through its own store only.
            #[cfg(not(feature = "agent"))]
            ViewConnection::Owned(_) => return Err(Error::Unsupported),
            #[cfg(feature = "agent")]
            ViewConnection::Owned(_) => {
                let dir = self
                    .root
                    .as_ref()
                    .and_then(|root| root.parent())
                    .ok_or(Error::Invalid)?;
                crate::meta::update(dir, key, |_| Ok(bytes.to_vec()))?;
            }
        }
        Ok(())
    }

    /// Session packs active at the head: their promoted tools and latest view.
    #[cfg(feature = "dext-strict-workspace")]
    pub fn packs(&mut self) -> Result<serde_json::Value> {
        crate::pack_runtime::overview(&self.db, SCOPE)
    }

    /// Whether Continue has session-pack work for the completed turn `execution` at the
    /// head: a decided follow-up the host stopped before delivering, or a turn end
    /// (idle) it never reached.
    #[cfg(feature = "dext-strict-workspace")]
    pub fn pack_follow_up(&mut self, execution: &str) -> Result<bool> {
        crate::pack_runtime::follow_up_pending(&self.db, SCOPE, execution)
    }

    /// The app state of `scope` as of `revision` (`{}` before any is set, and in a
    /// store its owner has not migrated to schema 17 yet).
    pub fn app_state(&mut self, scope: &str, revision: u64) -> Result<Vec<u8>> {
        let current: i64 = self
            .db
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if current < 17 {
            return Ok(b"{}".to_vec());
        }
        crate::transition::app_state_at(&self.db, scope, revision)
    }

    /// The attempt reserved for `effect`: its implementation label, owner epoch and
    /// whether a result is recorded for it.
    pub fn attempt(&mut self, effect: &str) -> Result<Option<(String, u64, bool)>> {
        Ok(self
            .db
            .query_row(
                "SELECT a.implementation,a.owner_epoch,EXISTS(SELECT 1 FROM receipts r WHERE r.attempt=a.id) FROM attempts a WHERE a.effect=?1",
                [effect],
                |row| Ok((row.get(0)?, crate::nonnegative_id(row, 1)?, row.get(2)?)),
            )
            .optional()?)
    }

    /// The store's open record at its current head: owners that stopped without
    /// closing it since the head last moved (`Store::crashes`), and whether the last
    /// owner closed it. `(0, true)` when the head moved since the last open, and for a
    /// store not migrated to schema 17 yet. Whether that last owner is gone without
    /// closing is `!closed` while `owner_alive` says no one holds the lock.
    pub fn open_record(&mut self) -> Result<(u64, bool)> {
        let current: i64 = self
            .db
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if current < 17 {
            return Ok((0, true));
        }
        let row: Option<(u64, i64)> = crate::cached_row(&self.db,
            "SELECT o.crashes,o.closed FROM opens o JOIN head h ON h.id=1 WHERE o.id=1 AND o.head=h.revision",
            [],
            |row| Ok((crate::nonnegative_id(row, 0)?, row.get(1)?)),
        ).optional()?;
        Ok(row.map_or((0, true), |(crashes, closed)| (crashes, closed != 0)))
    }

    /// Whether a process holds the store's owner lock now. When it is free this takes
    /// it for an instant (owners opening meanwhile retry, `take_owner_lock`).
    pub fn owner_alive(&self) -> bool {
        let Some(root) = &self.root else {
            return true;
        };
        let Ok(lock) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join("owner.lock"))
        else {
            return false;
        };
        match lock.try_lock() {
            Ok(()) => {
                let _ = lock.unlock();
                false
            }
            Err(_) => true,
        }
    }

    /// Admitted commands neither committed nor dropped (a prompt or message a stopped
    /// owner admitted and never started).
    pub fn queued(&mut self) -> Result<u64> {
        // A rewind's own admission is the rewind's record, not waiting input.
        Ok(crate::cached_row(
            &self.db,
            &format!(
                "SELECT count(*) FROM admissions a WHERE a.kind!=?1 AND {}",
                crate::rewind::QUEUED
            ),
            [crate::rewind::KIND],
            |row| crate::nonnegative_id(row, 0),
        )?)
    }

    /// The engine's admitted inputs no step has consumed (or dropped) yet, in order:
    /// `(sequence, command, payload)`.
    pub fn queued_inputs(&self) -> Result<Vec<(u64, String, Vec<u8>)>> {
        let mut query = self.db.prepare(&format!(
            "SELECT a.sequence,a.command_id,a.payload FROM admissions a WHERE a.scope=?1 AND a.kind=?2 AND {} ORDER BY a.sequence LIMIT 257",
            crate::rewind::QUEUED
        ))?;
        let rows = query
            .query_map(params![SCOPE, crate::turn::ENGINE], |row| {
                Ok((
                    crate::nonnegative_id(row, 0)?,
                    crate::bounded_label(row, 1)?,
                    stored(&self.db, self.legacy, row, 2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn job_effects(&mut self, head: u64) -> Result<Vec<JobEffect>> {
        let mut query = self.db.prepare("SELECT p.revision,i.id,i.adapter,i.input,q.result FROM applied_receipts p JOIN receipts q ON q.attempt=p.attempt JOIN attempts a ON a.id=p.attempt JOIN intents i ON i.id=a.effect JOIN revisions r ON r.revision=p.revision WHERE r.scope=?1 AND p.revision<=?2 AND i.adapter IN ('start_job','stop_job','read_job') ORDER BY p.revision DESC LIMIT 256")?;
        let rows = query
            .query_map(params![SCOPE, head.min(i64::MAX as u64) as i64], |row| {
                Ok((
                    crate::nonnegative_id(row, 0)?,
                    JobEffect {
                        id: crate::bounded_label(row, 1)?,
                        adapter: crate::bounded_label(row, 2)?,
                        input: stored(&self.db, self.legacy, row, 3)?,
                        result: stored(&self.db, self.legacy, row, 4)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(query);
        if rows.is_empty() {
            return Ok(vec![]);
        }
        let oldest = rows.last().map(|(revision, _)| *revision).unwrap_or(head);
        let mut line = std::collections::BTreeSet::new();
        self.line(head, |revision, _| {
            line.insert(revision);
            revision <= oldest || line.len() >= 8192
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|(revision, effect)| line.contains(&revision).then_some(effect))
            .collect())
    }

    /// The head revision with its continuation and workspace root. Revision 0 means
    /// the session has no turn yet.
    pub fn head(&mut self) -> Result<(u64, Vec<u8>, Option<String>)> {
        let legacy = self.legacy;
        Ok(crate::cached_row(
            &self.db,
            "SELECT r.revision,r.continuation,r.workspace FROM head h JOIN revisions r ON r.revision=h.revision WHERE h.id=1",
            [],
            |row| {
                Ok((
                    crate::nonnegative_id(row, 0)?,
                    stored(&self.db, legacy, row, 1)?,
                    crate::optional_digest(row, 2)?,
                ))
            },
        )?)
    }

    /// The continuation and workspace root of any committed revision of the session,
    /// including revisions a rewind left behind.
    pub fn at(&mut self, revision: u64) -> Result<Option<(Vec<u8>, Option<String>)>> {
        if revision == 0 || revision > i64::MAX as u64 {
            return Ok(None);
        }
        let legacy = self.legacy;
        Ok(self
            .db
            .query_row(
                "SELECT continuation,workspace FROM revisions WHERE revision=?1 AND scope=?2",
                params![revision as i64, SCOPE],
                |row| {
                    Ok((
                        stored(&self.db, legacy, row, 0)?,
                        crate::optional_digest(row, 1)?,
                    ))
                },
            )
            .optional()?)
    }

    /// Committed revisions after `after`, oldest first.
    pub fn steps(&mut self, after: u64) -> Result<Vec<Step>> {
        let mut statement = self.db.prepare(
            "SELECT r.revision,coalesce(p.id,''),i.adapter
             FROM revisions r
             LEFT JOIN proposals p ON p.revision=r.revision
             LEFT JOIN applied_receipts ar ON ar.revision=r.revision
             LEFT JOIN attempts a ON a.id=ar.attempt
             LEFT JOIN intents i ON i.id=a.effect
             WHERE r.revision>?1 AND r.scope=?2 ORDER BY r.revision LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![after.min(i64::MAX as u64) as i64, SCOPE, MAX_STEPS],
            |row| {
                Ok(Step {
                    revision: crate::nonnegative_id(row, 0)?,
                    proposal: row.get(1)?,
                    adapter: row.get(2)?,
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The newest `limit` committed revisions, oldest first.
    pub fn recent_steps(&mut self, limit: u64) -> Result<Vec<Step>> {
        let (head, _, _) = self.head()?;
        self.steps(head.saturating_sub(limit.min(MAX_STEPS as u64)))
    }

    /// Rewind targets among `revisions` with what each would restore. The head is
    /// checked once and candidates by their control header, so listing stays cheap;
    /// a rewind itself re-checks its target in full.
    pub fn targets(&mut self, revisions: &[u64]) -> Vec<(u64, RewindTargetKind)> {
        // Rewind reads only the current encoding; an unmigrated store has none until
        // its next open migrates it.
        if self.legacy {
            return vec![];
        }
        let Ok((head, _, _)) = self.head() else {
            return vec![];
        };
        crate::rewind::listed_targets(&self.db, SCOPE, head, revisions)
    }

    /// Recorded model replies stored after receipt row `after`, oldest first, at most
    /// `limit`: (row, result). Includes replies a rewind later abandoned; each was
    /// billed once.
    pub fn model_receipts_after(
        &mut self,
        after: i64,
        limit: usize,
    ) -> Result<Vec<(i64, Vec<u8>)>> {
        let legacy = self.legacy;
        let mut statement = self.db.prepare(
            "SELECT q.rowid,q.result FROM receipts q JOIN attempts a ON a.id=q.attempt WHERE a.implementation='dext-provider-v1' AND q.rowid>?1 ORDER BY q.rowid LIMIT ?2",
        )?;
        let rows = statement.query_map(params![after, limit.min(4096) as i64], |row| {
            Ok((row.get(0)?, stored(&self.db, legacy, row, 1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// How many model replies are stored at or before receipt row `through`.
    pub fn model_receipt_count(&mut self, through: i64) -> Result<u64> {
        Ok(crate::cached_row(
            &self.db,
            "SELECT count(*) FROM receipts q JOIN attempts a ON a.id=q.attempt WHERE a.implementation='dext-provider-v1' AND q.rowid<=?1",
            [through],
            |row| crate::nonnegative_id(row, 0),
        )?)
    }

    /// The provider's recorded refusal of a pending effect's attempt: status and what
    /// it said. (Stores before schema 16 recorded none.)
    pub fn rejection(&mut self, effect: &str) -> Result<Option<(u16, String)>> {
        let current: i64 = self
            .db
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if current < 16 {
            return Ok(None);
        }
        Ok(self
            .db
            .query_row(
                "SELECT f.status,f.detail FROM attempt_failures f JOIN attempts a ON a.id=f.attempt WHERE a.effect=?1",
                [effect],
                |row| Ok((row.get::<_, u16>(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    pub fn app_details_at(&mut self, revision: u64) -> Result<Vec<(String, Vec<u8>)>> {
        let schema: i64 = self
            .db
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if schema < 19 {
            return Ok(vec![]);
        }
        let mut query = self.db.prepare("SELECT p.revision,a.effect,d.value FROM app_details d JOIN attempts a ON a.id=d.attempt JOIN applied_receipts p ON p.attempt=a.id JOIN revisions r ON r.revision=p.revision WHERE r.scope=?1 AND p.revision<=?2 ORDER BY p.revision")?;
        let rows = query
            .query_map(
                params![SCOPE, revision.min(i64::MAX as u64) as i64],
                |row| {
                    Ok((
                        crate::nonnegative_id(row, 0)?,
                        crate::bounded_label(row, 1)?,
                        crate::packed_blob(&self.db, row, 2, crate::transition::MAX_STATE)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(query);
        if rows.is_empty() {
            return Ok(vec![]);
        }
        let oldest = rows[0].0;
        let mut line = std::collections::BTreeSet::new();
        self.line(revision, |at, _| {
            line.insert(at);
            at <= oldest
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|(at, effect, details)| line.contains(&at).then_some((effect, details)))
            .collect())
    }

    pub fn call_id(&self, effect: &str) -> Result<String> {
        crate::meta::call_id(&self.db, effect)
    }

    /// The keys of the session's tasks (`task_runs` without their inputs and answers).
    pub fn task_keys(&self) -> Result<Vec<String>> {
        let schema: i64 = self
            .db
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if schema < 19 {
            return Ok(vec![]);
        }
        let mut query = self.db.prepare("SELECT task FROM task_runs LIMIT 4097")?;
        let keys = query
            .query_map([], |row| crate::bounded_label(row, 0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(keys)
    }

    pub fn task_runs(&self) -> Result<Vec<crate::meta::TaskRun>> {
        let schema: i64 = self
            .db
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if schema < 19 {
            return Ok(vec![]);
        }
        crate::meta::tasks_on(&self.db)
    }

    pub fn retry_delay(&self, effect: &str) -> Result<Option<Duration>> {
        let schema: i64 = self
            .db
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if schema < 19 {
            return Ok(None);
        }
        let millis = crate::cached_row(&self.db, "SELECT d.millis FROM retry_delays d JOIN attempts a ON a.id=d.attempt WHERE a.effect=?1", [effect], |row| crate::nonnegative_id(row, 0)).optional()?;
        Ok(millis.map(Duration::from_millis))
    }

    pub fn app_details(&self, effect: &str) -> Result<Option<Vec<u8>>> {
        let schema: i64 = self
            .db
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if schema < 19 {
            return Ok(None);
        }
        Ok(crate::cached_row(&self.db, "SELECT d.value FROM app_details d JOIN attempts a ON a.id=d.attempt JOIN intents i ON i.id=a.effect JOIN revisions r ON r.revision=i.revision WHERE a.effect=?1 AND r.scope=?2", params![effect, SCOPE], |row| crate::packed_blob(&self.db, row, 0, crate::transition::MAX_STATE)).optional()?)
    }

    /// Whether a pending effect already has a recorded result, such as a model reply
    /// that Continue adopts without sending anything again.
    pub fn recorded(&mut self, effect: &str) -> Result<bool> {
        Ok(crate::cached_row(
            &self.db,
            "SELECT EXISTS(SELECT 1 FROM attempts a JOIN receipts q ON q.attempt=a.id WHERE a.effect=?1)",
            [effect],
            |row| row.get(0),
        )?)
    }

    /// Finished rewinds, oldest first.
    pub fn forks(&mut self) -> Result<Vec<Fork>> {
        let mut statement = self.db.prepare(
            "SELECT revision,target,old_revision FROM rewinds WHERE scope=?1 AND revision IS NOT NULL ORDER BY revision",
        )?;
        let rows = statement.query_map([SCOPE], |row| {
            Ok(Fork {
                revision: crate::nonnegative_id(row, 0)?,
                target: crate::nonnegative_id(row, 1)?,
                abandoned: crate::nonnegative_id(row, 2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Walks the line that leads to `head`, newest first, giving each revision on it
    /// with its workspace root; `visit` returns true to stop. A finished rewind's
    /// revision continues from its target, so the revisions a rewind left behind are
    /// not on the line (every other revision continues from the one before it).
    pub fn line(
        &mut self,
        head: u64,
        mut visit: impl FnMut(u64, Option<&str>) -> bool,
    ) -> Result<()> {
        let forks: std::collections::BTreeMap<u64, u64> = self
            .forks()?
            .into_iter()
            .map(|fork| (fork.revision, fork.target))
            .collect();
        let mut statement = self
            .db
            .prepare("SELECT workspace FROM revisions WHERE revision=?1 AND scope=?2")?;
        let mut revision = head;
        while revision > 0 {
            let root: Option<String> = statement
                .query_row(
                    params![revision.min(i64::MAX as u64) as i64, SCOPE],
                    |row| crate::optional_digest(row, 0),
                )
                .optional()?
                .ok_or(Error::Invalid)?;
            if visit(revision, root.as_deref()) {
                break;
            }
            revision = predecessor(revision, &forks);
        }
        Ok(())
    }

    /// The files of a workspace root with their sizes. Only the manifest is verified
    /// here; `object` verifies each file's bytes when it is read.
    pub fn files(&mut self, root: &str) -> Result<Vec<File>> {
        let tree = self.shared_tree(root)?;
        let mut out = Vec::with_capacity(tree.files.len());
        for entry in tree.files.iter().cloned() {
            let size: Option<i64> = self
                .db
                .prepare_cached(self.size_query())?
                .query_row([&entry.object], |row| row.get(0))
                .optional()?;
            let size = size.ok_or(Error::Incompatible)?;
            out.push(File {
                entry,
                size: size.max(0) as u64,
            });
        }
        Ok(out)
    }

    /// The links and empty directories of a workspace root (files: `files`).
    pub fn tree(&mut self, root: &str) -> Result<crate::workspace::Tree> {
        Ok((*self.shared_tree(root)?).clone())
    }

    /// The file `path` of a workspace root, if it has one.
    pub fn entry(&mut self, root: &str, path: &str) -> Result<Option<Entry>> {
        let tree = self.shared_tree(root)?;
        Ok(tree.files.iter().find(|entry| entry.path == path).cloned())
    }

    /// How many files a workspace root holds.
    pub fn file_count(&mut self, root: &str) -> Result<usize> {
        Ok(self.shared_tree(root)?.files.len())
    }

    /// The decoded manifest of a workspace root. Files opened one by one (a page's
    /// assets, a download of them all) and every status read decode it once.
    fn shared_tree(&mut self, root: &str) -> Result<std::sync::Arc<crate::workspace::Tree>> {
        if let Some(tree) = decoded(root) {
            return Ok(tree);
        }
        let manifest = self.object(root)?;
        let tree = std::sync::Arc::new(crate::workspace::decode_tree(&manifest)?);
        remember(root, manifest.len(), &tree);
        Ok(tree)
    }

    /// The verified bytes of a stored object (a file or a workspace manifest).
    pub fn object(&mut self, digest: &str) -> Result<Vec<u8>> {
        if !crate::artifact::valid_digest(digest) {
            return Err(Error::Invalid);
        }
        let bytes: Vec<u8> = self
            .db
            .prepare_cached(self.stored_query())?
            .query_row([digest], |row| crate::artifact::object_at(row, 0))
            .optional()?
            .ok_or(Error::Invalid)?;
        if crate::artifact::object_digest(&bytes) != digest {
            return Err(Error::Incompatible);
        }
        Ok(bytes)
    }

    /// What `revision` would restore if the session rewound to it now, or None when
    /// it is not a rewind target (or the head cannot be rewound at the moment).
    /// Whether the session has a rewind target at all, its head's in-flight work aside
    /// (`rewind::has_target`): what decides whether a cut-off request can be sent
    /// again from an earlier point, or only by `retry` in a fresh store.
    pub fn has_target(&mut self) -> bool {
        if self.legacy {
            return false;
        }
        match self.head() {
            Ok((head, _, _)) => crate::rewind::has_target(&self.db, SCOPE, head),
            Err(_) => false,
        }
    }

    pub fn target(&mut self, revision: u64) -> Option<RewindTargetKind> {
        if self.legacy {
            return None;
        }
        crate::rewind::ensure_open(&self.db).ok()?;
        let (head, _, _) = self.head().ok()?;
        crate::rewind::eligible_head(&self.db, SCOPE, head).ok()?;
        crate::rewind::eligible_target(&self.db, SCOPE, revision).ok()
    }
}

/// What usage accounting (`agent::usage_report`) reads besides recorded replies.
impl ReadView<'_> {
    /// Turn starts committed after revision `after`, oldest first: (revision, admitted
    /// command, `start` or `turn-rN`).
    pub fn turn_starts(&mut self, after: u64) -> Result<Vec<(u64, String)>> {
        let mut statement = self.db.prepare(
            "SELECT p.revision,substr(p.id,8) FROM proposals p JOIN revisions r ON r.revision=p.revision
             WHERE p.revision>?1 AND r.scope=?2 AND (p.id='commit-start' OR p.id GLOB 'commit-turn-r[0-9]*')
             ORDER BY p.revision",
        )?;
        let rows = statement
            .query_map(params![after.min(i64::MAX as u64) as i64, SCOPE], |row| {
                Ok((crate::nonnegative_id(row, 0)?, row.get(1)?))
            })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Model attempts without a recorded reply: (effect, the provider's refusal status
    /// when one was recorded). The rest have an unknown outcome (or are in flight).
    pub fn unanswered_model_attempts(&mut self) -> Result<Vec<(String, Option<u16>)>> {
        let current: i64 = self
            .db
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        // Stores before schema 16 recorded no refusals.
        let failures = if current >= 16 {
            "(SELECT f.status FROM attempt_failures f WHERE f.attempt=a.id)"
        } else {
            "NULL"
        };
        let mut statement = self.db.prepare(&format!(
            "SELECT a.effect,{failures} FROM attempts a WHERE a.implementation='dext-provider-v1'
             AND NOT EXISTS(SELECT 1 FROM receipts q WHERE q.attempt=a.id) ORDER BY a.rowid"
        ))?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// `run_code` attempts and whether each is settled (its child journal is then
    /// final): its receipt is recorded, or a finished rewind abandoned it (nothing
    /// resumes an abandoned attempt). Oldest first.
    pub fn code_attempts(&mut self) -> Result<Vec<(String, bool)>> {
        let mut statement = self.db.prepare(
            "SELECT a.id,EXISTS(SELECT 1 FROM receipts q WHERE q.attempt=a.id)
             OR EXISTS(SELECT 1 FROM rewinds w JOIN revisions r ON r.revision=i.revision WHERE w.scope=r.scope
             AND w.state IN (1,2) AND i.revision>w.target AND i.revision<=w.old_revision)
             FROM attempts a JOIN intents i ON i.id=a.effect WHERE i.adapter='run_code' ORDER BY a.rowid",
        )?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The input committed for an effect's intent (a model effect's prepared request).
    pub fn intent_input(&mut self, effect: &str) -> Result<Option<Vec<u8>>> {
        let legacy = self.legacy;
        Ok(self
            .db
            .query_row("SELECT input FROM intents WHERE id=?1", [effect], |row| {
                stored(&self.db, legacy, row, 0)
            })
            .optional()?)
    }

    /// What tells this store from any other: a digest of its first admitted command (a
    /// session's `start`, with the prompt and request it began with). Values are
    /// digested as decoded, so a migration keeps it. None before the first command.
    pub fn incarnation(&mut self) -> Result<Option<String>> {
        use sha2::{Digest, Sha256};
        let legacy = self.legacy;
        let first = self
            .db
            .query_row(
                "SELECT scope,command_id,payload FROM admissions ORDER BY sequence LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        stored(&self.db, legacy, row, 2)?,
                    ))
                },
            )
            .optional()?;
        Ok(first.map(|(scope, command, payload)| {
            let mut hash = Sha256::new();
            hash.update(b"spindle-store-incarnation-v1\0");
            for field in [scope.as_bytes(), command.as_bytes(), &payload] {
                hash.update((field.len() as u64).to_be_bytes());
                hash.update(field);
            }
            hash.finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        }))
    }

    /// Ids of this session's stages that start with `prefix`.
    pub fn stages_named(&mut self, prefix: &str) -> Result<Vec<String>> {
        let mut statement = self.db.prepare(
            "SELECT id FROM artifact_stages WHERE scope=?1 AND substr(id,1,length(?2))=?2 ORDER BY id",
        )?;
        let rows = statement.query_map(params![SCOPE, prefix], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The verified records pinned by these stages of the session: (stage, bytes).
    pub fn stage_records(&mut self, stages: &[String]) -> Result<Vec<(String, Vec<u8>)>> {
        let mut records = Vec::new();
        for chunk in stages.chunks(512) {
            let mut statement = self.db.prepare(&format!(
                "SELECT p.stage,o.digest,o.bytes,{} FROM staging_pins p JOIN objects o ON o.digest=p.digest
                 JOIN artifact_stages s ON s.id=p.stage WHERE s.scope=? AND p.stage IN ({})",
                if self.sized { "o.size" } else { "NULL" },
                vec!["?"; chunk.len()].join(",")
            ))?;
            let rows = statement.query_map(
                rusqlite::params_from_iter(
                    std::iter::once(SCOPE).chain(chunk.iter().map(String::as_str)),
                ),
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        crate::artifact::object_at(row, 2)?,
                    ))
                },
            )?;
            for row in rows {
                let (stage, digest, bytes) = row?;
                if crate::artifact::object_digest(&bytes) != digest {
                    return Err(Error::Incompatible);
                }
                records.push((stage, bytes));
            }
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_beside_the_owner_lock_and_never_writes() {
        let dir = std::env::temp_dir().join(format!("spindle-view-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("store");
        let owner = crate::Store::open(&store).unwrap();
        let mut view = View::open(&store).unwrap();
        let (head, _, root) = view.head().unwrap();
        assert_eq!((head, root), (0, None));
        assert!(view.steps(0).unwrap().is_empty());
        assert!(view.forks().unwrap().is_empty());
        assert!(view.at(0).unwrap().is_none());
        assert!(view.target(0).is_none());
        assert!(
            view.db
                .execute("UPDATE head SET revision=0 WHERE id=1", [])
                .is_err(),
            "the view must be read-only"
        );
        drop(view);
        drop(owner);
        assert!(crate::Store::open(&store).is_ok(), "the owner can reopen");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn borrowing_the_owner_connection_preserves_its_transaction_and_pragmas() {
        let dir = std::env::temp_dir().join(format!("spindle-owner-view-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let owner = crate::Store::open(&dir).unwrap();
        owner.db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let mut view = ReadView::from_store(&owner);
        assert!(view.owner_alive());
        assert_eq!(view.read(|view| view.head()).unwrap().0, 0);
        assert!(
            !owner.db.is_autocommit(),
            "a borrowed read never ends the owner's transaction"
        );
        let query_only: bool = owner
            .db
            .pragma_query_value(None, "query_only", |row| row.get(0))
            .unwrap();
        assert!(!query_only, "borrowing never makes the owner read-only");
        owner.db.execute_batch("ROLLBACK").unwrap();
        drop(owner);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_line_skips_what_a_rewind_left_behind() {
        // r5 rewound to r2 (r3, r4 left the line); r8 rewound to r6.
        let forks = std::collections::BTreeMap::from([(5, 2), (8, 6), (9, 9)]);
        let mut line = vec![];
        let mut revision = 10;
        while revision > 0 {
            line.push(revision);
            revision = predecessor(revision, &forks);
        }
        assert_eq!(line, [10, 9, 8, 6, 5, 2, 1], "a malformed fork never loops");
    }

    #[test]
    fn refuses_a_missing_or_foreign_store() {
        let dir = std::env::temp_dir().join(format!("spindle-view-x-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(View::open(&dir).is_err());
        std::fs::create_dir_all(&dir).unwrap();
        assert!(View::open(&dir).is_err());
        let db = Connection::open(dir.join("authority.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE t(x)").unwrap();
        drop(db);
        assert!(matches!(View::open(&dir), Err(Error::Incompatible)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
