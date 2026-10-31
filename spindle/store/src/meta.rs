//! Session metadata: configuration and accounting, not conversation revisions. The
//! owner uses its own connection; hosts may queue settings or retain priced usage
//! through a separate, bounded transaction without taking over the agent epoch.
use crate::{Error, Result, Store, codec};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::path::Path;
use std::time::Duration;

pub(crate) const TABLE: &str = "CREATE TABLE meta(key TEXT PRIMARY KEY CHECK(key IN ('settings','pending_settings','output_limits','usage')), value BLOB NOT NULL);";
const KEYS: [&str; 4] = ["settings", "pending_settings", "output_limits", "usage"];
pub const MAX_VALUE: usize = crate::MAX_PAYLOAD;

fn check(key: &str, bytes: Option<&[u8]>) -> Result<()> {
    if !KEYS.contains(&key) {
        return Err(Error::Invalid);
    }
    if let Some(bytes) = bytes {
        crate::fits(bytes.len())?;
    }
    Ok(())
}

pub(crate) fn get(db: &Connection, key: &str) -> Result<Option<Vec<u8>>> {
    check(key, None)?;
    let stored = crate::cached_row(db, "SELECT value FROM meta WHERE key=?1", [key], |row| {
        crate::bounded_blob(row, 0, codec::stored_bound(MAX_VALUE))
    })
    .optional()?;
    stored
        .map(|bytes| codec::unpack(&bytes, MAX_VALUE))
        .transpose()
}

pub(crate) fn put(db: &Connection, key: &str, bytes: &[u8]) -> Result<()> {
    check(key, Some(bytes))?;
    if crate::cached_execute(
        db,
        "INSERT INTO meta VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, codec::pack(bytes)],
    )? != 1
    {
        return Err(Error::Incompatible);
    }
    Ok(())
}

pub(crate) fn validate(db: &Connection) -> Result<()> {
    let invalid: bool = crate::cached_row(
        db,
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key NOT IN ('settings','pending_settings','output_limits','usage'))",
        [],
        |row| row.get(0),
    )?;
    if invalid {
        return Err(Error::Incompatible);
    }
    for key in KEYS {
        get(db, key)?;
    }
    Ok(())
}

impl Store {
    /// Configuration/accounting bytes, outside the revision chain (not rewound).
    pub fn metadata(&self, key: &str) -> Result<Option<Vec<u8>>> {
        if self.paused {
            return Err(Error::Paused);
        }
        get(&self.db, key)
    }

    pub fn call_id(&self, effect: &str) -> Result<String> {
        if self.paused {
            return Err(Error::Paused);
        }
        call_id(&self.db, effect)
    }

    /// Retains metadata through the owner connection, with its current epoch fence.
    pub fn set_metadata(&mut self, key: &str, bytes: &[u8]) -> Result<()> {
        if self.paused {
            return Err(Error::Paused);
        }
        check(key, Some(bytes))?;
        let result = (|| {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let epoch: u64 =
                crate::cached_row(&tx, "SELECT epoch FROM realm WHERE id=1", [], |row| {
                    crate::nonnegative_id(row, 0)
                })?;
            if epoch != self.epoch {
                return Err(Error::Stale);
            }
            put(&tx, key, bytes)?;
            tx.commit()?;
            Ok(())
        })();
        if matches!(
            result,
            Err(Error::Sql(_) | Error::Io(_) | Error::Locked | Error::Incompatible)
        ) {
            self.paused = true;
        }
        result
    }
}

pub(crate) fn call_id(db: &Connection, effect: &str) -> Result<String> {
    let identity: Vec<u8> =
        crate::cached_row(db, "SELECT identity FROM instance WHERE id=1", [], |row| {
            crate::bounded_blob(row, 0, 16)
        })?;
    if identity.len() != 16 {
        return Err(Error::Incompatible);
    }
    let namespace: String = identity.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{namespace}/{effect}"))
}

#[derive(Clone, Debug)]
pub struct TaskRun {
    pub task: String,
    pub origin: Option<String>,
    pub input: Vec<u8>,
    pub base: String,
    pub revision: u64,
    pub queued: bool,
    pub state: String,
    pub answer: Option<Vec<u8>>,
    pub delivered: Option<u64>,
    pub lease: Option<f64>,
}

pub(crate) fn tasks_on(db: &Connection) -> Result<Vec<TaskRun>> {
    let mut query = db.prepare("SELECT task,origin,input,base,revision,queued,state,answer,delivered,lease FROM task_runs ORDER BY revision,task LIMIT 4097")?;
    let rows = query
        .query_map([], |row| {
            Ok(TaskRun {
                task: crate::bounded_label(row, 0)?,
                origin: row.get(1)?,
                input: crate::bounded_blob(row, 2, 128 * 1024)?,
                base: crate::bounded_label(row, 3)?,
                revision: crate::nonnegative_id(row, 4)?,
                queued: row.get(5)?,
                state: crate::bounded_label(row, 6)?,
                answer: if row.get_ref(7)? == rusqlite::types::ValueRef::Null {
                    None
                } else {
                    Some(crate::bounded_blob(row, 7, crate::MAX_PAYLOAD)?)
                },
                delivered: row
                    .get::<_, Option<i64>>(8)?
                    .map(|value| u64::try_from(value).map_err(|_| rusqlite::Error::InvalidQuery))
                    .transpose()?,
                lease: row.get(9)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.len() > 4096 {
        return Err(Error::Exhausted);
    }
    Ok(rows)
}

impl Store {
    pub fn task_runs(&self) -> Result<Vec<TaskRun>> {
        if self.paused {
            return Err(Error::Paused);
        }
        tasks_on(&self.db)
    }

    pub fn resume_task(&mut self, task: &str) -> Result<()> {
        if self.paused {
            return Err(Error::Paused);
        }
        let result = (|| {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let epoch = crate::cached_row(&tx, "SELECT epoch FROM realm WHERE id=1", [], |row| {
                crate::nonnegative_id(row, 0)
            })?;
            if epoch != self.epoch {
                return Err(Error::Stale);
            }
            crate::execute_one(
                &tx,
                // Its report that it stopped was read already (or never was), and why it
                // stopped no longer holds: once it ends again, it reports again.
                "UPDATE task_runs SET state='running',answer=NULL,delivered=NULL WHERE task=?1 AND state='stopped'",
                [task],
            )?;
            tx.commit()?;
            Ok(())
        })();
        if matches!(
            result,
            Err(Error::Sql(_) | Error::Io(_) | Error::Locked | Error::Incompatible)
        ) {
            self.paused = true;
        }
        result
    }

    /// A background task's end, kept in its row (as `finish_task` keeps it for a
    /// native one). The driver reads its report to the session: its answer, or that it
    /// stopped (and why, when it says).
    pub fn finish_task_run(&mut self, task: &str, input: &[u8], end: TaskEnd<'_>) -> Result<()> {
        if self.paused {
            return Err(Error::Paused);
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let epoch: u64 = crate::cached_row(&tx, "SELECT epoch FROM realm WHERE id=1", [], |row| {
            crate::nonnegative_id(row, 0)
        })?;
        if epoch != self.epoch {
            return Err(Error::Stale);
        }
        end_task(&tx, task, input, end)?;
        tx.commit()?;
        Ok(())
    }

    pub fn register_task(&mut self, run: &TaskRun) -> Result<()> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !crate::valid_label(&run.task)
            || !crate::artifact::valid_digest(&run.base)
            || run.input.len() > 128 * 1024
            || run
                .lease
                .is_some_and(|lease| !lease.is_finite() || lease < 0.0)
        {
            return Err(Error::Invalid);
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let epoch: u64 = crate::cached_row(&tx, "SELECT epoch FROM realm WHERE id=1", [], |row| {
            crate::nonnegative_id(row, 0)
        })?;
        if epoch != self.epoch {
            return Err(Error::Stale);
        }
        let kept = tasks_on(&tx)?
            .into_iter()
            .find(|kept| kept.task == run.task);
        if let Some(kept) = kept {
            return if kept.state != "abandoned"
                && kept.input == run.input
                && kept.queued == run.queued
                && kept.origin == run.origin
            {
                Ok(())
            } else {
                Err(Error::Conflict)
            };
        }
        crate::workspace::validate_root(&tx, "operator", &run.base)?;
        if tasks_on(&tx)?.len() >= 4096 {
            return Err(Error::Exhausted);
        }
        crate::cached_execute(
            &tx,
            "INSERT INTO task_runs VALUES(?1,?2,?3,?4,?5,?6,'running',NULL,NULL,?7)",
            params![
                run.task,
                run.origin,
                run.input,
                run.base,
                run.revision as i64,
                run.queued,
                run.lease
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

pub fn task_runs(dir: &Path) -> Result<Vec<TaskRun>> {
    let Some(db) = connection(dir, false)? else {
        return Ok(vec![]);
    };
    let schema: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if schema < 19 {
        return Ok(vec![]);
    }
    tasks_on(&db)
}

pub fn finish_task(dir: &Path, task: &str, input: &[u8], end: TaskEnd<'_>) -> Result<()> {
    let mut db = connection(dir, true)?.ok_or(Error::Invalid)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    end_task(&tx, task, input, end)?;
    tx.commit()?;
    Ok(())
}

/// How a task ended, as its row keeps it.
#[derive(Clone, Copy, Debug)]
pub enum TaskEnd<'a> {
    /// Its turn reached its end: its answer (`{content, failed}`); it is finished.
    Answered(&'a [u8]),
    /// It stopped without an answer: by a person, with its session, or because it could
    /// not go on, then saying why (`{content, failed: true}`, `turn::stopped_why`). Its
    /// session hears so; started again by the same id, it goes on from where it was.
    Stopped(Option<&'a [u8]>),
}

/// Keeps a running task's end in its row. One stopped meanwhile (by a person, perhaps in
/// another process) stays as they left it: its worker's own end, after, changes nothing,
/// and the session hears what the person did.
fn end_task(tx: &Connection, task: &str, input: &[u8], end: TaskEnd<'_>) -> Result<()> {
    let (state, answer) = match end {
        TaskEnd::Answered(answer) => ("finished", Some(answer)),
        TaskEnd::Stopped(why) => ("stopped", why),
    };
    if answer.is_some_and(|answer| answer.len() > crate::MAX_PAYLOAD) {
        return Err(Error::Exhausted);
    }
    let changed = crate::cached_execute(
        tx,
        "UPDATE task_runs SET state=?1,answer=?2 WHERE task=?3 AND input=?4 AND state='running'",
        params![state, answer, task, input],
    )?;
    if changed == 1 {
        return Ok(());
    }
    let stopped: bool = crate::cached_row(
        tx,
        "SELECT EXISTS(SELECT 1 FROM task_runs WHERE task=?1 AND input=?2 AND state='stopped')",
        params![task, input],
        |row| row.get(0),
    )?;
    if stopped {
        Ok(())
    } else {
        Err(Error::Conflict)
    }
}

pub(crate) fn validate_tasks(db: &Connection) -> Result<()> {
    for run in tasks_on(db)? {
        if !crate::valid_label(&run.task)
            || !crate::artifact::valid_digest(&run.base)
            || !matches!(
                run.state.as_str(),
                "running" | "stopped" | "finished" | "abandoned"
            )
            || run
                .lease
                .is_some_and(|lease| !lease.is_finite() || lease < 0.0)
            || (run.state == "finished" && run.answer.is_none())
            || (run.delivered.is_some() && !run.queued)
        {
            return Err(Error::Incompatible);
        }
        crate::workspace::validate_root(db, "operator", &run.base)?;
        #[cfg(feature = "dext-turn")]
        {
            let input: serde_json::Value =
                serde_json::from_slice(&run.input).map_err(|_| Error::Incompatible)?;
            if !input.is_object() {
                return Err(Error::Incompatible);
            }
            if let Some(answer) = &run.answer {
                crate::turn::task_report_text(&run.task, answer)?;
            }
            if let Some(delivered) = run.delivered {
                let payload: Vec<u8> = crate::cached_row(
                    db,
                    "SELECT a.payload FROM consumed c JOIN admissions a ON a.sequence=c.admission WHERE c.revision=?1",
                    [delivered as i64],
                    |row| crate::packed_blob(db, row, 0, crate::MAX_PAYLOAD),
                )?;
                if !matches!(crate::turn::Action::decode(&payload)?, crate::turn::Action::TaskReport { task, .. } if task==run.task)
                {
                    return Err(Error::Incompatible);
                }
            }
        }
    }
    Ok(())
}

pub fn external_call_id(dir: &Path, effect: &str) -> Result<String> {
    let db = connection(dir, false)?.ok_or(Error::Incompatible)?;
    call_id(&db, effect)
}

pub(crate) fn connection(dir: &Path, write: bool) -> Result<Option<Connection>> {
    let path = dir.join("store/authority.sqlite");
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() => return Err(Error::Invalid),
        #[cfg(unix)]
        Ok(meta) => {
            use std::os::unix::fs::MetadataExt;
            if meta.nlink() != 1 {
                return Err(Error::Invalid);
            }
        }
        #[cfg(not(unix))]
        Ok(_) => {}
    }
    // A store the package holds (or a session that moved elsewhere) is written by no
    // other host, as no other host opens it: its owner may have closed it only for a
    // moment (a compaction swapping a copy in). A native owner's fence is coordinated by
    // the flock its writers take.
    if write
        && crate::fence_note(&dir.join("store"))
            .is_some_and(|note| !note.starts_with(crate::NATIVE_FENCE))
    {
        return Err(Error::Locked);
    }
    let flags = if write {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let db = Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.pragma_update(None, "trusted_schema", "OFF")?;
    db.pragma_update(None, "foreign_keys", "ON")?;
    let app: i64 = db.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let schema: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if app != crate::APP_ID {
        return Err(Error::Incompatible);
    }
    if [14, 15, 16, 17].contains(&schema) {
        return Ok(None);
    }
    if ![18, 19, crate::SCHEMA].contains(&schema) {
        return Err(Error::Incompatible);
    }
    if write {
        db.pragma_update(None, "synchronous", "FULL")?;
        crate::durable::full_flushes(&db)?;
    } else {
        db.pragma_update(None, "query_only", "ON")?;
    }
    Ok(Some(db))
}

/// Reads current metadata without becoming the agent owner. None also covers a
/// session not migrated yet; the native host then imports its legacy sidecar.
pub fn read(dir: &Path, key: &str) -> Result<Option<Vec<u8>>> {
    check(key, None)?;
    connection(dir, false)?
        .map(|db| get(&db, key))
        .transpose()
        .map(Option::flatten)
}

/// A narrow host metadata transaction, not an agent takeover. Returns false before
/// migration or before the store exists, so the caller can retain the legacy file.
pub fn update(
    dir: &Path,
    key: &str,
    change: impl FnOnce(Option<&[u8]>) -> Result<Vec<u8>>,
) -> Result<bool> {
    check(key, None)?;
    let Some(mut db) = connection(dir, true)? else {
        return Ok(false);
    };
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let before = get(&tx, key)?;
    let after = change(before.as_deref())?;
    put(&tx, key, &after)?;
    tx.commit()?;
    Ok(true)
}

#[cfg(feature = "agent")]
pub(crate) fn seed(store: &mut Store, dir: &Path) -> Result<()> {
    for (key, file, fallback) in [
        ("settings", "session.json", None),
        (
            "pending_settings",
            "pending-settings.json",
            Some(&b"null"[..]),
        ),
        ("output_limits", "output-limits.json", Some(&b"{}"[..])),
        ("usage", "usage.json", None),
    ] {
        if store.metadata(key)?.is_some() {
            continue;
        }
        let bytes = match std::fs::read(dir.join(file)) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => fallback.map(Vec::from),
            Err(error) => return Err(error.into()),
        };
        if let Some(bytes) = bytes {
            store.set_metadata(key, &bytes)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ignored_metadata_writes_are_refused_and_fence_the_owner() {
        let root = std::env::temp_dir().join(format!("spindle-meta-ignore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut owner = Store::open(&root).unwrap();
        owner.set_metadata("settings", b"original").unwrap();
        owner.db.execute_batch("CREATE TEMP TRIGGER ignore_meta BEFORE UPDATE ON meta BEGIN SELECT RAISE(IGNORE); END;").unwrap();
        assert!(matches!(
            owner.set_metadata("settings", b"lost"),
            Err(Error::Incompatible)
        ));
        assert!(matches!(owner.metadata("settings"), Err(Error::Paused)));
        drop(owner);
        let owner = Store::open(&root).unwrap();
        assert_eq!(owner.metadata("settings").unwrap().unwrap(), b"original");
        drop(owner);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn metadata_transactions_are_bounded_atomic_and_do_not_take_over_the_owner() {
        let root = std::env::temp_dir().join(format!("spindle-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut owner = Store::open(&root.join("store")).unwrap();
        let epoch = owner.epoch();
        owner.set_metadata("settings", b"original").unwrap();
        assert!(matches!(
            owner.set_metadata("unknown", b"bad"),
            Err(Error::Invalid)
        ));
        assert!(update(&root, "settings", |_| Err(Error::Conflict)).is_err());
        assert_eq!(owner.metadata("settings").unwrap().unwrap(), b"original");
        update(&root, "settings", |_| Ok(b"changed".to_vec())).unwrap();
        assert_eq!(owner.metadata("settings").unwrap().unwrap(), b"changed");
        assert_eq!(owner.epoch(), epoch);
        assert!(matches!(
            Store::open(&root.join("store")),
            Err(Error::Locked)
        ));
        drop(owner);
        let mut reopened = Store::open(&root.join("store")).unwrap();
        reopened.check_integrity().unwrap();
        assert_eq!(reopened.metadata("settings").unwrap().unwrap(), b"changed");
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }
}
