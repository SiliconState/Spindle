//! Background jobs of this process, by session store. A job is a sandboxed command
//! started by a `start_job` call over a snapshot of the workspace; it is never sealed,
//! so it changes nothing durable. What a model learns from it is what `read_job` and
//! `stop_job` record. Jobs live only as long as this process. What each printed last
//! (the newest `KEPT_TAIL` bytes, with its state) is kept in the store's `job-output`
//! folder as it runs, so after a restart `read_job` gives that, said as such, instead
//! of nothing; a session's export never carries it.
use dext_native::strict::{Job, JobState, Loopback};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

/// The folder in a session's store where jobs' last output is kept.
const KEPT_DIR: &str = "job-output";
/// The newest bytes of a job's output that are kept.
const KEPT_TAIL: usize = 64 * 1024;
/// How often a running job's kept output is brought up to date (when it printed more).
const KEPT_EVERY: Duration = Duration::from_secs(2);
/// Kept outputs per store; the oldest go first.
const KEPT_FILES: usize = 64;
/// A draft older than this is one a crash left (a write takes far less).
const KEPT_DRAFT_STALE: Duration = Duration::from_secs(600);

/// Jobs remembered per session; the oldest ended ones are forgotten first.
const KEPT: usize = 32;

pub(crate) struct Entry {
    pub job: Arc<Job>,
    pub command: String,
    pub started: SystemTime,
}

type Sessions = BTreeMap<String, BTreeMap<String, Entry>>;

static JOBS: Mutex<Sessions> = Mutex::new(BTreeMap::new());

fn jobs() -> MutexGuard<'static, Sessions> {
    JOBS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The id a `start_job` effect's job gets: stable for the effect, so a replayed
/// receipt and the live job agree.
pub(crate) fn job_id(effect: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(effect.as_bytes());
    let hex: String = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("job-{hex}")
}

/// A session's key: its store directory, canonical so every way of naming it agrees.
pub fn session_key(store: &Path) -> String {
    std::fs::canonicalize(store)
        .unwrap_or_else(|_| store.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Stops and forgets the jobs of the store in `store` (a session being deleted, whose
/// store may not be open). Returns how many were still running.
pub fn stop_store_jobs(store: &Path) -> usize {
    let key = session_key(store);
    loopbacks().remove(&key);
    stop_all(&key)
}

/// Each session's loopback network (`Loopback`), shared by its isolated runs and jobs
/// without the network for as long as this process lives (or the session is deleted).
type Loopbacks = BTreeMap<String, Arc<Loopback>>;

static LOOPBACKS: Mutex<Loopbacks> = Mutex::new(BTreeMap::new());

/// Set once this host refused to make one for good (namespaces not permitted or not
/// supported, no /bin/sh to make them in): every run then has a network of its own. A passing failure (no file
/// descriptors or processes left) is tried again next time.
static NO_LOOPBACK: AtomicBool = AtomicBool::new(false);

fn loopbacks() -> MutexGuard<'static, Loopbacks> {
    LOOPBACKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The loopback network `session`'s runs share, made on first use; None where this
/// host cannot make one (each run then gets a network namespace of its own).
pub(crate) fn loopback(session: &str) -> Option<Arc<Loopback>> {
    if NO_LOOPBACK.load(Ordering::SeqCst) {
        return None;
    }
    let mut loopbacks = loopbacks();
    if let Some(loopback) = loopbacks.get(session) {
        return Some(loopback.clone());
    }
    match Loopback::new() {
        Ok(loopback) => {
            let loopback = Arc::new(loopback);
            loopbacks.insert(session.to_string(), loopback.clone());
            Some(loopback)
        }
        Err(error) => {
            #[cfg(unix)]
            let lasting = matches!(
                error.raw_os_error(),
                Some(libc::EPERM | libc::EACCES | libc::EINVAL | libc::ENOSPC | libc::ENOSYS)
            );
            #[cfg(not(unix))]
            let lasting = false;
            let refused = lasting
                || matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::Unsupported
                        | std::io::ErrorKind::NotFound
                );
            if refused {
                NO_LOOPBACK.store(true, Ordering::SeqCst);
            }
            None
        }
    }
}

pub(crate) fn insert(session: &str, id: String, entry: Entry) {
    watch(session, &id, &entry);
    let mut jobs = jobs();
    let session = jobs.entry(session.to_string()).or_default();
    if session.len() >= KEPT {
        let oldest = session
            .iter()
            .filter(|(_, entry)| entry.job.state() != JobState::Running)
            .min_by_key(|(_, entry)| entry.started)
            .map(|(id, _)| id.clone());
        if let Some(oldest) = oldest {
            session.remove(&oldest);
        }
    }
    session.insert(id, entry);
}

pub(crate) fn get(session: &str, id: &str) -> Option<Arc<Job>> {
    jobs().get(session)?.get(id).map(|entry| entry.job.clone())
}

/// Every remembered job of a session, oldest first: id, command, state.
pub fn list(session: &str) -> Vec<(String, String, JobState)> {
    let jobs = jobs();
    let mut listed: Vec<_> = jobs
        .get(session)
        .map(|session| {
            session
                .iter()
                .map(|(id, entry)| {
                    (
                        entry.started,
                        (id.clone(), entry.command.clone(), entry.job.state()),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    listed.sort_by_key(|(started, _)| *started);
    listed.into_iter().map(|(_, job)| job).collect()
}

/// Running jobs of a session.
pub(crate) fn running(session: &str) -> usize {
    jobs().get(session).map_or(0, |session| {
        session
            .values()
            .filter(|entry| entry.job.state() == JobState::Running)
            .count()
    })
}

/// Stops and forgets every job of a session (rewind, restart, deletion). Returns how
/// many were still running.
pub(crate) fn stop_all(session: &str) -> usize {
    let removed = jobs().remove(session).unwrap_or_default();
    let mut stopped = 0;
    for entry in removed.into_values() {
        if entry.job.state() == JobState::Running {
            stopped += 1;
        }
        entry.job.stop();
    }
    stopped
}

/// Whether `id` is what `job_id` makes (`job-` and 8 hex digits): only such a name is
/// ever a file name here.
fn id_ok(id: &str) -> bool {
    id.len() == 12
        && id.starts_with("job-")
        && id[4..]
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn kept_path(session: &str, id: &str) -> Option<PathBuf> {
    id_ok(id).then(|| Path::new(session).join(KEPT_DIR).join(format!("{id}.json")))
}

/// Keeps the newest output of `job` (and how it stands) for `read_job` after a
/// restart. Only in a store that is still there: a session moved away to be deleted
/// gets no folder made in its old place. A cache, not a record: replaced whole, not
/// flushed, mode 0600.
fn keep(session: &str, id: &str, command: &str, job: &Job) {
    let store = Path::new(session);
    let Some(path) = kept_path(session, id) else {
        return;
    };
    if !store.is_dir() {
        return;
    }
    let dir = store.join(KEPT_DIR);
    // Not recursive: a store that is gone stays gone. Private, as its files are.
    let folder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let folder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut folder = folder;
        folder.mode(0o700);
        folder
    };
    if let Err(error) = folder.create(&dir)
        && error.kind() != std::io::ErrorKind::AlreadyExists
    {
        return;
    }
    // Each write its own draft: the same id can have two watchers (one effect run twice
    // in this process), and two writers of one draft could interleave.
    static DRAFTS: AtomicU64 = AtomicU64::new(0);
    let never = AtomicBool::new(false);
    let total = job.read(u64::MAX, 0, Duration::ZERO, &never).total;
    let page = job.read(
        total.saturating_sub(KEPT_TAIL as u64),
        KEPT_TAIL,
        Duration::ZERO,
        &never,
    );
    let record = serde_json::json!({
        "command": command,
        "state": describe(&page.state),
        "running": page.state == JobState::Running,
        "start": page.start,
        "end": page.end,
        "total": page.total,
        "output": String::from_utf8_lossy(&page.bytes),
    });
    let draft = dir.join(format!(
        ".{id}.{}-{}.draft",
        std::process::id(),
        DRAFTS.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(&draft).and_then(|mut file| {
        use std::io::Write;
        file.write_all(record.to_string().as_bytes())
    });
    if written.is_err() || std::fs::rename(&draft, &path).is_err() {
        let _ = std::fs::remove_file(&draft);
        return;
    }
    prune_kept(&dir);
}

/// Keeps at most `KEPT_FILES` outputs in `dir`, dropping the least recently written,
/// and removes drafts a crash left.
fn prune_kept(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut kept: Vec<(SystemTime, PathBuf)> = vec![];
    for entry in entries.flatten() {
        let Some(modified) = entry.metadata().and_then(|meta| meta.modified()).ok() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".json") {
            kept.push((modified, entry.path()));
        } else if name.ends_with(".draft")
            && modified.elapsed().is_ok_and(|age| age > KEPT_DRAFT_STALE)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    if kept.len() <= KEPT_FILES {
        return;
    }
    kept.sort();
    for (_, path) in &kept[..kept.len() - KEPT_FILES] {
        let _ = std::fs::remove_file(path);
    }
}

/// Keeps `entry`'s output while it runs (when it printed more, at most every
/// `KEPT_EVERY`) and once more as it ends, on a thread of its own that ends with it.
/// Where no thread can be made, the job runs unkept (as before restarts kept anything).
fn watch(session: &str, id: &str, entry: &Entry) {
    let (session, id, command, job) = (
        session.to_string(),
        id.to_string(),
        entry.command.clone(),
        entry.job.clone(),
    );
    let watcher = std::thread::Builder::new().name(format!("keep {id}"));
    let _ = watcher.spawn(move || {
        let never = AtomicBool::new(false);
        let mut written = None;
        loop {
            // Waits up to KEPT_EVERY for the job to end.
            let now = job.read(u64::MAX, 0, KEPT_EVERY, &never);
            let ended = now.state != JobState::Running;
            if ended || written != Some(now.total) {
                keep(&session, &id, &command, &job);
                written = Some(now.total);
            }
            if ended {
                return;
            }
        }
    });
}

/// What `read_job` says of a job this process no longer has, from its kept output:
/// None when nothing was kept (or `id` is no job's name).
pub(crate) fn kept(session: &str, id: &str) -> Option<String> {
    let text = std::fs::read_to_string(kept_path(session, id)?).ok()?;
    let record: serde_json::Value = serde_json::from_str(&text).ok()?;
    let command = record["command"].as_str().unwrap_or_default();
    let how = if record["running"] == true {
        "it was still running when Spindle stopped or restarted, which ended it".to_string()
    } else {
        format!("it {}", record["state"].as_str().unwrap_or("ended"))
    };
    let start = record["start"].as_u64().unwrap_or(0);
    let total = record["total"].as_u64().unwrap_or(0);
    let end = record["end"].as_u64().unwrap_or(total);
    Some(format!(
        "{}\n[{id} (`{command}`) is no longer running in this session: {how}. Above is \
         its last output as kept, bytes {start}..{end} of {total}; it cannot be read \
         further or stopped.]",
        record["output"].as_str().unwrap_or_default()
    ))
}

/// How a job stands, in words.
pub fn describe(state: &JobState) -> String {
    match state {
        JobState::Running => "running".into(),
        JobState::Exited(Some(0)) => "exited successfully (code 0)".into(),
        JobState::Exited(Some(code)) => format!("exited with code {code}"),
        JobState::Exited(None) => "was ended by a signal".into(),
        JobState::Ended(reason) => format!("ended: {reason}"),
    }
}
