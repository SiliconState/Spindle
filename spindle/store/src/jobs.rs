//! Background jobs of this process, by session store. A job is a sandboxed command
//! started by a `start_job` call over a snapshot of the workspace; it is never sealed,
//! so it changes nothing durable. What a model learns from it is what `read_job` and
//! `stop_job` record. Jobs live only as long as this process: after a restart a job
//! id from history names nothing, and reading it says so.
use dext_native::strict::{Job, JobState};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

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
    stop_all(&session_key(store))
}

pub(crate) fn insert(session: &str, id: String, entry: Entry) {
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
