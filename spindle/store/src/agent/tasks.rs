//! Tasks: Spindle sessions a session runs for part of its work. A task is a session
//! of its own under `<session>/tasks/<id>/`, with its own store, rewind and recovery;
//! its spend counts against the session that started it (`usage_report` adds it in).
//!
//! The model starts one with the `task` tool (when the session's `tasks` setting is
//! on). The call is reserved and answered like an app tool call: the task's directory
//! is named by the call's effect id, so a call cut off by a crash finds its task again
//! and continues it under the task's own rules (a model request still goes out at most
//! once inside it) instead of starting over. When the task's turn ends, its answer is
//! the call's result and, unless the call said `files: "discard"`, the files it changed
//! land in the session's workspace with that result, so rewinding the session past the
//! call undoes them too. Foreground task calls share their step's original tree,
//! execute concurrently and merge in call order without overwriting conflicts.
//! Background calls retain their files in the child and queue one durable answer.
use super::{AppAnswer, AppCall, AppTools, Event, Observer, Session, Settings};
use crate::agent::Result;
pub(crate) use crate::driver::task_tool::refuse_conflicts;
pub use crate::driver::task_tool::{MAX_DEPTH, NAME, Workspace, definition, refusal};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

/// The directory of the task `key` names (a call's effect id, or an app's task id).
pub fn task_dir(parent: &Path, key: &str) -> PathBuf {
    parent.join("tasks").join(crate::reads::task_name(key))
}

/// The tasks a session started, in name order.
pub fn task_dirs(dir: &Path) -> Result<Vec<PathBuf>> {
    let path = dir.join("tasks");
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
        Ok(_) => return Err("tasks is not a regular directory".into()),
        Err(error) => return Err(format!("cannot inspect tasks: {error}")),
    }
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(&path).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if !kind.is_dir() || kind.is_symlink() {
            return Err("task is not a regular directory".into());
        }
        if dirs.len() >= 4096 {
            return Err("task directory limit exceeded".into());
        }
        dirs.push(entry.path());
    }
    dirs.sort();
    Ok(dirs)
}

/// How many sessions up from `dir` started it (0 for a session that is no task).
pub fn depth(dir: &Path) -> usize {
    let mut depth = 0;
    let mut at = dir;
    while let Some(parent) = at.parent()
        && parent.file_name().is_some_and(|name| name == "tasks")
        && let Some(owner) = parent.parent()
        && owner.join("session.json").is_file()
    {
        depth += 1;
        at = owner;
    }
    depth
}

/// Whether a session in `dir` with `settings` offers the `task` tool.
pub fn offered(settings: &Settings, dir: &Path) -> bool {
    settings.tasks && depth(dir) < settings.task_policy.max_depth
}

/// What a task call came to: an answer to record (`AppAnswer::Stopped` records
/// nothing), or why the task cannot go on by itself now. Then a call in the foreground
/// stays unanswered: the session's next `continue` asks again, once the task's own
/// session is settled (a request of unknown outcome retried, say); one in the background
/// stops, saying why (its session hears so; started again, it goes on).
pub(crate) type Run = std::result::Result<AppAnswer, String>;

struct TaskSlot(std::fs::File);
impl Drop for TaskSlot {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
fn slot(parent: &Path, key: &str, count: usize, interrupt: &AtomicBool) -> Result<TaskSlot> {
    let dir = parent.join("cache/task-slots");
    super::create_private_dir(&dir)?;
    loop {
        if interrupt.load(Ordering::SeqCst)
            || crate::meta::task_runs(parent)
                .map_err(super::store_error)?
                .iter()
                .any(|run| run.task == key && matches!(run.state.as_str(), "stopped" | "abandoned"))
        {
            return Err("stopped while waiting for a task slot".into());
        }
        for index in 0..count {
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            let file = options
                .open(dir.join(index.to_string()))
                .map_err(|error| error.to_string())?;
            if file.try_lock().is_ok() {
                return Ok(TaskSlot(file));
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct TaskDone<'a>(&'a AtomicBool);
impl Drop for TaskDone<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct LiveTask {
    interrupt: Arc<AtomicBool>,
    finished: Mutex<bool>,
    changed: Condvar,
}
fn live_tasks() -> &'static Mutex<BTreeMap<PathBuf, Arc<LiveTask>>> {
    static LIVE: OnceLock<Mutex<BTreeMap<PathBuf, Arc<LiveTask>>>> = OnceLock::new();
    LIVE.get_or_init(Default::default)
}
fn guard<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn key(parent: &Path, task: &str) -> Result<String> {
    if !(1..=80).contains(&task.len())
        || !task
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    {
        return Err("task is an id of 1-80 letters, digits, -, _ or .".into());
    }
    crate::meta::task_runs(parent)
        .map_err(super::store_error)?
        .into_iter()
        .find(|run| {
            run.task.strip_prefix("app:") == Some(task)
                || task_dir(parent, &run.task)
                    .file_name()
                    .is_some_and(|name| name == task)
        })
        .map(|run| run.task)
        .ok_or_else(|| "task not found".into())
}

pub(crate) fn active(parent: &Path, key: &str) -> bool {
    let path = task_dir(parent, key);
    let live = guard(live_tasks()).get(&path).cloned();
    if live.is_some_and(|live| !*guard(&live.finished)) {
        return true;
    }
    crate::view::View::open(&path.join("store")).is_ok_and(|view| view.owner_alive())
}

pub fn stop(parent: &Path, key: &str) -> Result<bool> {
    let path = task_dir(parent, key);
    let live = guard(live_tasks()).get(&path).cloned();
    let mut db = crate::meta::connection(parent, true)
        .map_err(super::store_error)?
        .ok_or("no task registry")?;
    let tx = db.transaction().map_err(|error| error.to_string())?;
    let changed = tx
        .execute(
            "UPDATE task_runs SET state='stopped' WHERE task=?1 AND state='running'",
            [key],
        )
        .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())?;
    if changed != 0
        && let Some(live) = live
    {
        live.interrupt.store(true, Ordering::SeqCst);
    }
    Ok(changed != 0)
}

pub fn stop_all(parent: &Path) {
    for (path, live) in guard(live_tasks()).iter() {
        if path.starts_with(parent.join("tasks")) {
            live.interrupt.store(true, Ordering::SeqCst);
        }
    }
}

pub(crate) fn spawn_registered(
    parent: &Path,
    record: crate::meta::TaskRun,
    base: Workspace,
    observer: Arc<dyn Observer>,
    credentials: Arc<dyn super::Credentials>,
    until_unknown: bool,
) -> Result<()> {
    let mut lives = guard(live_tasks());
    let path = task_dir(parent, &record.task);
    lives.retain(|_, live| !*guard(&live.finished));
    if lives.get(&path).is_some_and(|live| !*guard(&live.finished)) {
        return Ok(());
    }
    let live = Arc::new(LiveTask {
        interrupt: Arc::new(AtomicBool::new(false)),
        finished: Mutex::new(false),
        changed: Condvar::new(),
    });
    lives.insert(path.clone(), live.clone());
    let failed_parent = parent.to_path_buf();
    let failed_record = (record.task.clone(), record.input.clone());
    let failed_path = path.clone();
    let parent = parent.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("spindle-task".into())
        .spawn(move || {
            let input: std::result::Result<Value, _> = serde_json::from_slice(&record.input);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match input {
                Ok(input) => run(
                    &parent,
                    &record.task,
                    &input,
                    &base,
                    &live.interrupt,
                    Host {
                        observer: &observer,
                        credentials: &credentials,
                        until_unknown,
                    },
                ),
                Err(error) => Err(format!("invalid retained task input: {error}")),
            }))
            .unwrap_or_else(|_| Err("task worker panicked (its session is kept as it was)".into()));
            // Stopped (by a person, or with its session) it has no answer; one that could
            // not go on stops saying why (so its session hears that, and starting it again
            // takes it up).
            let (answered, kept) = match result {
                Ok(AppAnswer::Result {
                    content, failed, ..
                }) => (
                    true,
                    Some(json!({"content":content,"failed":failed}).to_string()),
                ),
                Ok(_) => (false, None),
                Err(why) => (false, Some(crate::turn::stopped_why(&why))),
            };
            let end = match (&kept, answered) {
                (Some(answer), true) => crate::meta::TaskEnd::Answered(answer.as_bytes()),
                (why, _) => crate::meta::TaskEnd::Stopped(why.as_deref().map(str::as_bytes)),
            };
            let mut finished = guard(&live.finished);
            let retained = crate::meta::finish_task(&parent, &record.task, &record.input, end);
            *finished = true;
            live.changed.notify_all();
            drop(finished);
            if let Err(error) = retained {
                observer.event(Event::Nested {
                    label: &format!("task outcome could not be retained: {error}"),
                });
            }
        });
    if let Err(error) = spawned {
        lives.remove(&failed_path);
        let why = format!("cannot start task worker: {error}");
        crate::meta::finish_task(
            &failed_parent,
            &failed_record.0,
            &failed_record.1,
            crate::meta::TaskEnd::Stopped(Some(crate::turn::stopped_why(&why).as_bytes())),
        )
        .map_err(super::store_error)?;
        return Err(why);
    }
    Ok(())
}

pub fn wait(parent: &Path, key: &str, timeout: Duration, interrupt: &AtomicBool) -> Result<Value> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let record = crate::meta::task_runs(parent)
            .map_err(super::store_error)?
            .into_iter()
            .find(|run| run.task == key)
            .ok_or("no such task")?;
        let pending =
            record.state == "running" || (record.state == "stopped" && active(parent, key));
        if !pending || interrupt.load(Ordering::SeqCst) || std::time::Instant::now() >= deadline {
            return Ok(
                json!({"task":task_dir(parent,key),"state":record.state,"answer":record.answer.and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()),"pending":pending}),
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub(crate) struct Host<'a> {
    pub observer: &'a Arc<dyn Observer>,
    pub credentials: &'a Arc<dyn super::Credentials>,
    pub until_unknown: bool,
}

/// Runs or resumes a child over its original workspace under the host's retry policy.
pub(crate) fn run(
    parent: &Path,
    call: &str,
    input: &Value,
    base: &Workspace,
    interrupt: &AtomicBool,
    host: Host<'_>,
) -> Run {
    match run_task(parent, call, input, base, interrupt, host) {
        Ok(run) => run,
        Err(_) if interrupt.load(Ordering::SeqCst) => Ok(AppAnswer::Stopped),
        Err(error) => Err(format!(
            "its session ({}) could not run: {error}",
            task_dir(parent, call).display()
        )),
    }
}

fn answer(content: String, failed: bool, files: Changes) -> AppAnswer {
    AppAnswer::Result {
        content,
        failed,
        writes: files.0,
        deletes: files.1,
        links: files.2,
        directories: files.3,
        state: None,
        details: None,
    }
}

/// The input a task was started with (`task.json` beside its session), which a later
/// start under the same key must repeat: different input under a used key fails.
const INPUT: &str = "task.json";

/// An existing task must prove which input created it before it may run again.
pub fn validate_input(parent: &Path, key: &str, input: &Value) -> Result<()> {
    use std::io::Read;
    let tasks = parent.join("tasks");
    match std::fs::symlink_metadata(&tasks) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
        Ok(_) => return Err("tasks is not a regular directory".into()),
        Err(error) => return Err(format!("cannot inspect task directory: {error}")),
    }
    let dir = task_dir(parent, key);
    match std::fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
        Ok(_) => return Err("task directory is not a regular directory".into()),
        Err(error) => return Err(format!("cannot inspect task identity: {error}")),
    }
    let path = dir.join(INPUT);
    let invalid = |error| {
        format!(
            "cannot verify task identity ({}): {error}; restore its original record or use a new task id",
            path.display()
        )
    };
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(&path)
        .map_err(|error| invalid(error.to_string()))?;
    let meta = file
        .metadata()
        .map_err(|error| invalid(error.to_string()))?;
    if !meta.is_file()
        || !std::fs::symlink_metadata(&path).is_ok_and(|meta| !meta.file_type().is_symlink())
    {
        return Err(invalid("not a regular identity record".into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return Err(invalid("linked identity record".into()));
        }
    }
    const MAX_INPUT: u64 = 128 * 1024;
    let mut bytes = Vec::new();
    file.take(MAX_INPUT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| invalid(error.to_string()))?;
    if bytes.len() as u64 > MAX_INPUT {
        return Err(invalid("identity record exceeds 128 KiB".into()));
    }
    let kept: Value = serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
    if !kept.is_object() || refusal(&kept).is_some() || recorded_input(&kept) != kept {
        return Err(invalid("malformed identity record".into()));
    }
    if kept != recorded_input(input) {
        return Err("task was started with another prompt, model, effort or files policy; use a new task id".into());
    }
    Ok(())
}

pub(crate) fn identity_input(input: &Value) -> Value {
    recorded_input(input)
}

pub(crate) fn reserved_usd(parent: &Path, exclude: Option<&Path>) -> Result<f64> {
    let mut reserved = 0.0;
    for run in crate::meta::task_runs(parent).map_err(super::store_error)? {
        if !matches!(run.state.as_str(), "running" | "stopped") {
            continue;
        }
        let dir = task_dir(parent, &run.task);
        if exclude == Some(dir.as_path()) {
            continue;
        }
        if let Some(lease) = run.lease {
            let spent = if dir.join("store").is_dir() {
                super::usage_report(&dir)?.budget_spent()
            } else {
                0.0
            };
            reserved += (lease - spent).max(0.0);
        }
    }
    Ok(reserved)
}

/// The part of a `task` input that decides what the task does (as every host keeps it).
fn recorded_input(input: &Value) -> Value {
    crate::driver::task_tool::identity(input)
}

fn run_task(
    parent: &Path,
    call: &str,
    input: &Value,
    base: &Workspace,
    interrupt: &AtomicBool,
    host: Host<'_>,
) -> Result<Run> {
    let Host {
        observer,
        credentials,
        until_unknown,
    } = host;
    let policy = Settings::load(parent)?;
    let _slot = slot(parent, call, policy.task_policy.max_running, interrupt)?;
    if depth(parent) >= policy.task_policy.max_depth {
        return Err("this session reached its configured task depth limit".into());
    }
    let model = input["model"]
        .as_str()
        .or(input["settings"]["model"].as_str())
        .unwrap_or(&policy.model);
    if !policy.task_policy.models.is_empty()
        && !policy
            .task_policy
            .models
            .iter()
            .any(|allowed| allowed == model)
    {
        return Err(format!("model `{model}` is not permitted by tasks.models"));
    }
    validate_input(parent, call, input)?;
    let dir = task_dir(parent, call);
    let label = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let relay = || -> Arc<dyn Observer> {
        Arc::new(Relay {
            parent: observer.clone(),
            label: label.clone(),
        })
    };
    let prompt = input["prompt"].as_str().unwrap_or_default().to_string();
    let mut session = if dir.join("session.json").is_file() {
        Session::open_with_credentials(&dir, relay(), credentials.clone())?
    } else {
        let settings = child_settings(parent, input, credentials.as_ref())?;
        let tasks = parent.join("tasks");
        if !tasks.is_dir() {
            super::create_private_dir(&tasks)?;
        }
        let session =
            Session::create_with_credentials(&dir, settings, vec![], relay(), credentials.clone())?;
        let bytes = serde_json::to_vec(&recorded_input(input)).map_err(|e| e.to_string())?;
        crate::durable::write(&dir.join(INPUT), &bytes).map_err(|e| e.to_string())?;
        session
    };
    if crate::meta::task_runs(parent)
        .map_err(super::store_error)?
        .iter()
        .any(|run| run.task == call && matches!(run.state.as_str(), "stopped" | "abandoned"))
    {
        return Err("task was stopped or abandoned; explicitly restart it before running".into());
    }
    // Its budget is what the session has left (counting all but this task's own
    // spend), worked out again each time it runs: the session may have spent more, or
    // changed its budget, since.
    lease(parent, &dir, input["settings"]["max_usd"].as_f64())?;
    observer.event(Event::Nested {
        label: &format!("task {label}: started"),
    });
    let report = crate::api::recovery(&dir)?;
    let settings = Settings::load(&dir)?;
    let head = report["head"].as_u64().unwrap_or(0);
    let stopped = report["phase"] == "stopped";
    if until_unknown && crate::api::has_unknown(&report) {
        let item = report["items"]
            .as_array()
            .and_then(|items| {
                items.iter().find(|item| {
                    item["next"] != "none"
                        && (item["outcome"] == "unknown" || item["unknown"] == true)
                })
            })
            .ok_or("unknown task outcome has no recovery item")?;
        return Ok(Err(format!(
            "stopped at unknown outcome `{}`; inspect its task session before continuing",
            item["id"].as_str().unwrap_or(call)
        )));
    }
    // A first request a crash left unknown, in a task that resends such requests, is
    // retried as `retry` would: in a fresh store, the old one kept aside.
    let retry_first = !until_unknown
        && settings.resend_unknown
        && report["items"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["kind"] == "model" && item["outcome"] == "unknown" && item["next"] == "retry"
            })
        })
        && session.targets()?.is_empty();
    if head > 0 && !stopped && !retry_first && report["wake"]["work"] == "blocked" {
        return Ok(Err(report["wake"]["why"]
            .as_str()
            .unwrap_or("it waits for a person")
            .to_string()));
    }
    let child_interrupt = session.interrupt_handle();
    let done = AtomicBool::new(false);
    let mut session = Some(session);
    let outcome = std::thread::scope(|scope| {
        // The session's stop reaches the task.
        scope.spawn(|| {
            while !done.load(Ordering::SeqCst) {
                let stopped = crate::meta::task_runs(parent)
                    .map(|runs| {
                        runs.iter().any(|run| {
                            run.task == call
                                && matches!(run.state.as_str(), "stopped" | "abandoned")
                        })
                    })
                    .unwrap_or(true);
                if interrupt.load(Ordering::SeqCst) || stopped {
                    child_interrupt.store(true, Ordering::SeqCst);
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let _done = TaskDone(&done);
        (|| -> Result<()> {
            if retry_first {
                return session.take().expect("open above").retry();
            }
            let session = session.as_mut().expect("open above");
            match head {
                // A task cut off before its first step starts it again, files and all.
                0 => {
                    session.initial_files = None;
                    session.start_turn(
                        &prompt,
                        super::Start::Import(
                            base.files.clone(),
                            base.links.clone(),
                            base.directories.clone(),
                        ),
                    )?;
                    session.driver.begin_run();
                    session.driver.until_unknown = until_unknown;
                    session.drive_from(None)
                }
                // Finished: its answer is read below.
                _ if stopped => Ok(()),
                _ if until_unknown => session.resume_until_unknown(),
                _ => session.resume(),
            }
        })()
    });
    if interrupt.load(Ordering::SeqCst) {
        return Ok(Ok(AppAnswer::Stopped));
    }
    let mut session = match session {
        Some(session) => session,
        None => Session::open_with_credentials(&dir, relay(), credentials.clone())?,
    };
    let status = crate::api::head_state(&dir)?;
    // Only a turn that ended is an answer; one cut off in the middle of a step waits
    // for its session to be settled.
    if status["phase"] != "stopped" {
        let why = match outcome {
            Err(error) => error,
            Ok(()) => "its turn did not finish".into(),
        };
        return Ok(Err(why));
    }
    let said = session
        .history()?
        .iter()
        .rev()
        .find(|message| message.role == "assistant")
        .map(|message| super::text_of(&message.content))
        .unwrap_or_default();
    let files = if input["files"] == "discard" {
        (vec![], vec![], vec![], vec![])
    } else {
        changes(&mut session, base)?
    };
    let (writes, deletes) = (files.0.len(), files.1.len());
    observer.event(Event::Nested {
        label: &format!("task {label}: done, {} files changed", writes + deletes),
    });
    // A turn that ended on a stop of its own (the budget, a provider's filter) did not
    // finish its work: it answers as failed, every time it is asked.
    let complete = status["complete"] == true;
    let mut content = said.trim().to_string();
    if !complete {
        let why = status["stop_text"].as_str().unwrap_or("it stopped");
        content = format!("The task stopped before it finished ({why}).\n\n{content}");
    }
    if writes + deletes > 0 {
        content.push_str(&format!(
            "\n\n[task {label}: {writes} file(s) written, {deletes} removed]"
        ));
    }
    if content.trim().is_empty() {
        content = format!("[task {label} finished without an answer]");
    }
    Ok(Ok(answer(content.trim().to_string(), !complete, files)))
}

/// Sets the task's budget to what `parent` has left, leaving out the task's own spend
/// (which the parent's report includes): no budget when the parent has none.
fn lease(parent: &Path, dir: &Path, requested: Option<f64>) -> Result<()> {
    let max = Settings::load(parent)?.max_usd;
    let lease = match max {
        None => None,
        Some(max) => {
            let other = reserved_usd(parent, Some(dir))?;
            let all = super::usage_report(parent)?.budget_spent();
            let own = super::usage_report(dir)?.budget_spent();
            let remaining = (max - (all - own) - other).max(0.0);
            let reserved = crate::meta::task_runs(parent)
                .map_err(super::store_error)?
                .into_iter()
                .find(|run| task_dir(parent, &run.task) == dir)
                .and_then(|run| run.lease);
            Some(reserved.map_or(remaining, |lease| remaining.min(lease)))
        }
    };
    let lease = match (lease, requested) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (None, cap) | (cap, None) => cap,
    };
    let mut settings = Settings::load(dir)?;
    if settings.max_usd != lease {
        settings.max_usd = lease;
        settings.save(dir)?;
    }
    Ok(())
}

type Changes = (
    Vec<super::File>,
    Vec<String>,
    Vec<crate::workspace::Link>,
    Vec<String>,
);

/// The files the task's workspace has that `base` lacks or holds differently, and the
/// paths of `base` it no longer has.
fn changes(session: &mut Session, base: &Workspace) -> Result<Changes> {
    let head = session.status()?.revision;
    let now = session.files_at(head)?;
    let before: std::collections::HashMap<&str, (&[u8], bool)> = base
        .files
        .iter()
        .map(|(path, bytes, executable)| (path.as_str(), (bytes.as_slice(), *executable)))
        .collect();
    let kept: std::collections::HashSet<&str> =
        now.iter().map(|(path, _, _)| path.as_str()).collect();
    let writes = now
        .iter()
        .filter(|(path, bytes, executable)| {
            before.get(path.as_str()) != Some(&(bytes.as_slice(), *executable))
        })
        .cloned()
        .collect();
    let mut deletes: Vec<String> = base
        .files
        .iter()
        .filter(|(path, _, _)| !kept.contains(path.as_str()))
        .map(|(path, _, _)| path.clone())
        .collect();
    let tree = session.links_and_directories()?;
    let links = tree
        .links
        .iter()
        .filter(|link| !base.links.contains(link))
        .cloned()
        .collect();
    let directories = tree
        .directories
        .iter()
        .filter(|path| !base.directories.contains(path))
        .cloned()
        .collect();
    deletes.extend(
        base.links
            .iter()
            .filter(|link| !tree.links.iter().any(|now| now.path == link.path))
            .map(|link| link.path.clone()),
    );
    deletes.extend(
        base.directories
            .iter()
            .filter(|path| !tree.directories.contains(path))
            .cloned(),
    );
    Ok((writes, deletes, links, directories))
}

/// A task's settings: the session's, with the call's model and effort (its budget is
/// set by `lease`).
fn child_settings(
    parent: &Path,
    input: &Value,
    credentials: &dyn super::Credentials,
) -> Result<Settings> {
    let mut settings = Settings::load(parent)?;
    if let Some(overrides) = input.get("settings") {
        if let Some(model) = overrides["model"].as_str() {
            settings.model = credentials.route(&settings.provider, Some(model))?.model;
        }
        if let Some(effort) = overrides["effort"].as_str() {
            settings.effort = super::Effort::parse(effort).ok_or("invalid task effort")?;
        }
        if let Some(cap) = overrides["max_usd"].as_f64() {
            settings.max_usd = Some(settings.max_usd.map_or(cap, |parent| parent.min(cap)));
        }
    }
    if let Some(model) = input["model"]
        .as_str()
        .filter(|model| !model.trim().is_empty())
    {
        settings.model = credentials.route(&settings.provider, Some(model))?.model;
    }
    if let Some(effort) = input["effort"].as_str().and_then(super::Effort::parse) {
        settings.effort = effort;
    }
    settings.self_edit = false;
    Ok(settings)
}

/// A task's observer: its tool calls show in the session as nested steps; its app tool
/// calls go to the session's app, named `tasks/<task>/<call>` there (a task's first
/// turn has the same effect ids as every other session's). The host's own tools
/// (settings, self-edit) act on the host's thread, so a task gets none.
struct Relay {
    parent: Arc<dyn Observer>,
    label: String,
}

impl Observer for Relay {
    fn event(&self, event: Event<'_>) {
        let value = event.to_json();
        self.parent.event(Event::Task {
            task: &self.label,
            event: &value,
        });
        if let Event::ToolCall { name, .. } = event {
            self.parent.event(Event::Nested {
                label: &format!("task {}: {name}", self.label),
            });
        }
    }
    fn app_tools(&self) -> Option<Arc<dyn AppTools>> {
        let app = self.parent.app_tools()?;
        Some(Arc::new(TaskApp {
            app,
            label: self.label.clone(),
        }))
    }
}

/// The session's app, answering a task's calls under names of their own.
struct TaskApp {
    app: Arc<dyn AppTools>,
    label: String,
}

impl AppTools for TaskApp {
    fn call(&self, call: &AppCall<'_>, interrupt: &AtomicBool) -> AppAnswer {
        let id = format!("tasks/{}/{}", self.label, call.call);
        self.app.call(&AppCall { call: &id, ..*call }, interrupt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn until_unknown_keeps_a_childs_first_uncertain_request_and_store() {
        let root =
            std::env::temp_dir().join(format!("spindle-task-unknown-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let settings = Settings::from_json(&json!({"provider":"local", "model":"fixture", "resend_unknown":true, "builtin_tools":"none", "compaction":"digest"})).unwrap();
        struct Quiet;
        impl Observer for Quiet {
            fn event(&self, _: Event<'_>) {}
        }
        let observer: Arc<dyn Observer> = Arc::new(Quiet);
        let credentials: Arc<dyn super::super::Credentials> =
            Arc::new(super::super::NativeCredentials);
        let parent = Session::create_with_credentials(
            &root,
            settings.clone(),
            vec![],
            observer.clone(),
            credentials.clone(),
        )
        .unwrap();
        let input = json!({"prompt":"unknown child"});
        let dir = task_dir(&root, "key");
        let mut child = Session::create_with_credentials(
            &dir,
            settings,
            vec![],
            observer.clone(),
            credentials.clone(),
        )
        .unwrap();
        std::fs::write(
            dir.join(INPUT),
            serde_json::to_vec(&recorded_input(&input)).unwrap(),
        )
        .unwrap();
        child
            .start_turn(
                "unknown child",
                super::super::Start::Import(vec![], vec![], vec![]),
            )
            .unwrap();
        let need = loop {
            match child.driver.next(&mut child.store).unwrap() {
                crate::driver::Need::Continue => {}
                crate::driver::Need::Send(need) => break need,
                _ => panic!("expected child model request"),
            }
        };
        child
            .store
            .reserve_turn_model(&need.request, &false.into(), |_| Ok(()))
            .unwrap();
        let identity = child.store.call_id(&need.id).unwrap();
        drop(child);
        let result = run(
            &root,
            "key",
            &input,
            &Workspace::default(),
            &AtomicBool::new(false),
            Host {
                observer: &observer,
                credentials: &credentials,
                until_unknown: true,
            },
        );
        let error = result.err().unwrap();
        assert!(error.contains("stopped at unknown outcome"), "{error}");
        let mut child = Session::open_with_credentials(&dir, observer, credentials).unwrap();
        assert_eq!(child.store.call_id(&need.id).unwrap(), identity);
        assert!(
            child
                .store
                .receipt(&need.request.attempt.id)
                .unwrap()
                .is_none()
        );
        drop(child);
        drop(parent);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn panicking_task_releases_its_cancellation_watcher_and_retains_a_stop_saying_why() {
        let root = std::env::temp_dir().join(format!("spindle-task-panic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        struct Panics;
        impl Observer for Panics {
            fn event(&self, event: Event<'_>) {
                if matches!(event, Event::Task { event, .. } if event["type"] == "prompt") {
                    panic!("injected task observer panic");
                }
            }
        }
        let observer: Arc<dyn Observer> = Arc::new(Panics);
        let mut parent = Session::create(&root, Settings::from_json(&json!({"provider":"local", "model":"fixture", "builtin_tools":"none", "compaction":"digest"})).unwrap(), vec![], observer).unwrap();
        parent
            .start_turn(
                "parent",
                super::super::Start::Import(vec![], vec![], vec![]),
            )
            .unwrap();
        let input = json!({"prompt":"child"});
        parent.start_task("panic", &input, "queue").unwrap();
        let result = parent.wait_task("panic", Duration::from_secs(5)).unwrap();
        assert_eq!(result["state"], "stopped", "{result}");
        assert_eq!(result["pending"], false, "{result}");
        assert!(
            result["answer"]["content"]
                .as_str()
                .is_some_and(|said| said.starts_with("task worker panicked")),
            "{result}"
        );
        drop(parent);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_task_identity_is_required_valid_and_unchanged() {
        let root =
            std::env::temp_dir().join(format!("spindle-task-identity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let input = json!({"prompt":"original"});
        assert!(validate_input(&root, "key", &input).is_ok());
        let dir = task_dir(&root, "key");
        std::fs::create_dir_all(&dir).unwrap();
        let record = dir.join(INPUT);
        assert!(
            validate_input(&root, "key", &input)
                .unwrap_err()
                .contains("cannot verify")
        );
        for bytes in [b"{".as_slice(), b"null", b"[]", b"{}"] {
            std::fs::write(&record, bytes).unwrap();
            assert!(validate_input(&root, "key", &input).is_err());
        }
        std::fs::write(&record, serde_json::to_vec(&input).unwrap()).unwrap();
        #[cfg(unix)]
        {
            let linked = root.join("linked-identity");
            std::fs::hard_link(&record, &linked).unwrap();
            assert!(validate_input(&root, "key", &input).is_err());
            std::fs::remove_file(linked).unwrap();
            let kept = root.join("kept-tasks");
            std::fs::rename(root.join("tasks"), &kept).unwrap();
            std::os::unix::fs::symlink(&kept, root.join("tasks")).unwrap();
            assert!(validate_input(&root, "key", &input).is_err());
            std::fs::remove_file(root.join("tasks")).unwrap();
            std::fs::rename(kept, root.join("tasks")).unwrap();
        }
        assert!(validate_input(&root, "key", &input).is_ok());
        assert!(
            validate_input(&root, "key", &json!({"prompt":"different"}))
                .unwrap_err()
                .contains("another prompt")
        );
        std::fs::write(&record, vec![b' '; 128 * 1024 + 1]).unwrap();
        assert!(
            validate_input(&root, "key", &input)
                .unwrap_err()
                .contains("128 KiB")
        );
        std::fs::remove_file(&record).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("missing", &record).unwrap();
            assert!(validate_input(&root, "key", &input).is_err());
            std::fs::remove_file(&record).unwrap();
        }
        std::fs::create_dir(&record).unwrap();
        assert!(validate_input(&root, "key", &input).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tasks_are_named_by_their_call_and_nest_at_most_twice() {
        let root = std::env::temp_dir().join(format!("spindle-tasks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("session.json"), "{}").unwrap();
        let child = task_dir(&root, "e/m/1/t/0");
        assert_eq!(child, task_dir(&root, "e/m/1/t/0"));
        assert_ne!(child, task_dir(&root, "e/m/1/t/1"));
        assert_eq!(depth(&root), 0);
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("session.json"), "{}").unwrap();
        assert_eq!(depth(&child), 1);
        assert_eq!(task_dirs(&root).unwrap(), vec![child.clone()]);
        let grandchild = task_dir(&child, "x");
        std::fs::create_dir_all(&grandchild).unwrap();
        assert_eq!(depth(&grandchild), 2);
        assert!(refusal(&json!({"prompt": "go"})).is_none());
        assert!(refusal(&json!({"prompt": " "})).is_some());
        assert!(refusal(&json!({"prompt": "go", "files": "all"})).is_some());
        let _ = std::fs::remove_dir_all(root);
    }
}
