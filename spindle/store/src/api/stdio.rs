//! `spindle api`: the public API as JSON-RPC 2.0 over NDJSON on stdin/stdout. One
//! process manages any number of sessions, each named by its directory. Every request
//! is one JSON object on one line; every frame the process writes (responses and
//! `event` notifications) is one JSON object on one line of stdout, and nothing else
//! is written there. docs/API.md is the contract.
//!
//! Each open session has a worker thread that owns its `Session` (and so its owner
//! lock) and runs its jobs in order. The reading thread never waits for a worker: a
//! call that needs the `Session` is queued and answered by the worker when it is done,
//! so `stop`, `steer` and reads keep being answered while a turn runs. Reads that do
//! not need the `Session` (`history`, `files`, `read`, `usage`, and `status` during a
//! turn) go through a read-only `View` of committed state. Work that may be slow
//! (credential checks, importing a directory) never runs on the reading thread.
use super::{base64_encode, files_at, finding_json, head_state, messages_at};
use super::{providers_json, read_at, settings_json, status_json, target_json};
use crate::agent::{self, AppAnswer, AppCall, AppTools, Compaction, Effort};
use crate::agent::{Event, Isolation, Observer, Session, Settings};
use crate::protocol::{
    Backend, Fault, MAX_LINE, MAX_STEERS, Outcome, Params, Reply, SETTINGS, bad, busy, conflict,
    failed, files_param, internal, invalid_request, not_found, not_running, turn_failed,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, mpsc};
use std::time::{Duration, Instant};

/// Optional capabilities of this transport, reported by `info`. Names are only ever
/// added within an API version.
pub const FEATURES: &[&str] = &[
    "events",
    "expected_head",
    "steering",
    "attachments",
    "import",
    "revisions",
    "export",
    "doctor",
    "usage",
    "display",
    "steer_now",
    "app_tools",
    "state",
    "recover",
    "tasks",
    "session_transfer",
];

/// How long end of input waits for interrupted turns to wind down before exiting.
const SHUTDOWN: Duration = Duration::from_secs(10);

type Sessions = Arc<Mutex<BTreeMap<PathBuf, Arc<Entry>>>>;
type Job = Box<dyn FnOnce(&mut Actor) + Send>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The protocol stream. Frames are written whole, one per line, under one lock.
struct Out(Mutex<Box<dyn Write + Send>>);
impl Out {
    fn send(&self, frame: &Value) {
        let mut line = frame.to_string();
        line.push('\n');
        let mut out = lock(&self.0);
        // A client that is gone cannot be told anything; end of input ends the process.
        let _ = out.write_all(line.as_bytes()).and_then(|()| out.flush());
    }
    fn reply(&self, id: &Value, outcome: Outcome) {
        self.send(&crate::protocol::reply_frame(id, outcome));
    }
}

/// Forwards a session's events as `event` notifications, tagged with the session and
/// the id of the request that caused them, and keeps the last stop and rewind for the
/// request's result.
struct Relay {
    request: Arc<Mutex<Value>>,
    stopped: Mutex<Option<Value>>,
    rewound: Mutex<Option<Value>>,
    app: Arc<AppRelay>,
}

/// Sends a session's notifications, and asks the client for app tool answers.
struct AppRelay {
    out: Arc<Out>,
    session: String,
    request: Arc<Mutex<Value>>,
    desk: Arc<AppDesk>,
}

/// The app tool call a session waits on (calls run one at a time), and the answer
/// `app_tool_result` delivers for it.
#[derive(Default)]
struct AppDesk {
    waiting: Mutex<BTreeMap<String, Option<AppAnswer>>>,
    changed: Condvar,
}

impl AppRelay {
    fn notify(&self, value: Value) {
        let request = lock(&self.request).clone();
        self.out.send(&crate::protocol::event_frame(
            &self.session,
            &request,
            value,
        ));
    }
}

impl AppTools for AppRelay {
    fn call(&self, call: &AppCall<'_>, interrupt: &AtomicBool) -> AppAnswer {
        lock(&self.desk.waiting).insert(call.call.to_string(), None);
        self.notify(json!({
            "type": "app_tool",
            "call": call.call,
            "tool_use_id": call.tool_use_id,
            "name": call.name,
            "input": call.input,
            "again": call.again,
            "rerun": if call.rerun { "safe" } else { "no" },
            "timeout_s": call.timeout.as_secs(),
        }));
        let deadline = Instant::now() + call.timeout;
        let mut waiting = lock(&self.desk.waiting);
        let answer = loop {
            if let Some(answer) = waiting.get_mut(call.call).and_then(Option::take) {
                break answer;
            }
            if interrupt.load(Ordering::SeqCst) {
                break AppAnswer::Stopped;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break AppAnswer::NoAnswer;
            }
            waiting = self
                .desk
                .changed
                .wait_timeout(waiting, left.min(Duration::from_millis(100)))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        };
        waiting.remove(call.call);
        drop(waiting);
        let outcome = match &answer {
            AppAnswer::Result { failed: false, .. } => "answered",
            AppAnswer::Result { failed: true, .. } => "failed",
            AppAnswer::Unknown(_) => "unknown",
            AppAnswer::NoAnswer => "no_answer",
            _ => "stopped",
        };
        self.notify(json!({"type": "app_tool_done", "call": call.call, "outcome": outcome}));
        answer
    }
}

impl Observer for Relay {
    fn event(&self, event: Event<'_>) {
        let value = event.to_json();
        match value["type"].as_str() {
            Some("stopped") => *lock(&self.stopped) = Some(value.clone()),
            Some("rewound") => *lock(&self.rewound) = Some(value.clone()),
            _ => {}
        }
        self.app.notify(value);
    }
    fn app_tools(&self) -> Option<Arc<dyn AppTools>> {
        Some(self.app.clone())
    }
}

/// What the reading thread knows about a session while its worker runs.
struct Shared {
    /// The running turn-like operation (prompt, continue, retry, rewind), if any.
    running: Option<&'static str>,
    closing: bool,
    /// The open `Session`'s interrupt flag, steering queue and read-now flag.
    interrupt: Arc<AtomicBool>,
    steering: Arc<Mutex<Vec<String>>>,
    hurry: Arc<AtomicBool>,
    /// App state changes for the running turn (`Session::state_handle`).
    state: Arc<Mutex<Vec<Value>>>,
}

/// One open session: its directory, its worker's job queue and shared state.
struct Entry {
    dir: PathBuf,
    label: String,
    jobs: mpsc::Sender<Job>,
    shared: Mutex<Shared>,
    desk: Arc<AppDesk>,
}

/// A session's worker: owns the `Session` and runs queued jobs one at a time.
struct Actor {
    entry: Arc<Entry>,
    relay: Arc<Relay>,
    session: Option<Session>,
    sessions: Sessions,
    quit: bool,
}
impl Actor {
    /// The open `Session`, opened now if needed (after a retry, doctor or failure).
    fn session(&mut self) -> agent::Result<&mut Session> {
        if self.quit {
            return Err("the session was closed".into());
        }
        if self.session.is_none() {
            let session = Session::open(&self.entry.dir, self.relay.clone())?;
            self.adopt(&session);
            self.session = Some(session);
        }
        Ok(self.session.as_mut().expect("opened above"))
    }
    /// Points stop and steer at a newly opened `Session`, carrying over a stop or
    /// messages that arrived for the operation before it was opened.
    fn adopt(&self, session: &Session) {
        let mut shared = lock(&self.entry.shared);
        let interrupt = session.interrupt_handle();
        if shared.running.is_some() && shared.interrupt.load(Ordering::SeqCst) {
            interrupt.store(true, Ordering::SeqCst);
        }
        let steering = session.steering_handle();
        let waiting = std::mem::take(&mut *lock(&shared.steering));
        lock(&steering).extend(waiting);
        let hurry = session.hurry_handle();
        if shared.running.is_some() && shared.hurry.load(Ordering::SeqCst) {
            hurry.store(true, Ordering::SeqCst);
        }
        let state = session.state_handle();
        let waiting = std::mem::take(&mut *lock(&shared.state));
        lock(&state).extend(waiting);
        shared.interrupt = interrupt;
        shared.steering = steering;
        shared.hurry = hurry;
        shared.state = state;
    }
    /// Lets go of the session and takes it out of the process. The session list's lock
    /// is held while it is marked closing, so nothing is queued after this; calls queued
    /// before it are still answered (`run` drains them).
    fn retire(&mut self) {
        self.session = None;
        self.quit = true;
        let mut sessions = lock(&self.sessions);
        if sessions
            .get(&self.entry.dir)
            .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry))
        {
            sessions.remove(&self.entry.dir);
        }
        lock(&self.entry.shared).closing = true;
    }
    fn run(mut self, jobs: mpsc::Receiver<Job>) {
        while let Ok(job) = jobs.recv() {
            job(&mut self);
            if self.quit {
                break;
            }
        }
        // Calls queued before the session closed get their (refused) answers.
        while let Ok(job) = jobs.try_recv() {
            job(&mut self);
        }
    }
    /// `expected_head`, checked once this worker holds the session (and so its owner
    /// lock): nothing else can move the head between the check and the operation.
    fn head_is(&mut self, expected: Option<u64>) -> std::result::Result<(), Fault> {
        let Some(expected) = expected else {
            return Ok(());
        };
        self.session().map_err(failed)?;
        let head = head_state(&self.entry.dir).map_err(failed)?["head"].clone();
        if head != json!(expected) {
            return Err(Fault::new(
                -32003,
                "head_moved",
                format!("the session's head is {head}, not {expected}"),
            ));
        }
        Ok(())
    }
    /// Runs a turn-like operation to its end and answers its request.
    fn turn(&mut self, op: Op, expected: Option<u64>, id: Option<Value>) {
        *lock(&self.relay.request) = id.clone().unwrap_or(Value::Null);
        *lock(&self.relay.stopped) = None;
        *lock(&self.relay.rewound) = None;
        // What a task adds to the reply: its directory and answer.
        let mut answered = None;
        let outcome = match self.head_is(expected) {
            Err(fault) => Err(fault),
            Ok(()) => {
                let outcome = catch_unwind(AssertUnwindSafe(|| -> agent::Result<()> {
                    match op {
                        Op::Prompt { text, files, state } => {
                            let session = self.session()?;
                            if let Some(patch) = state {
                                session.state_with_next_prompt(patch)?;
                            }
                            session.prompt_with_files(&text, files)
                        }
                        Op::Continue { until_unknown } => {
                            if until_unknown {
                                self.session()?.resume_until_unknown()
                            } else {
                                self.session()?.resume()
                            }
                        }
                        Op::Retry => {
                            self.session()?;
                            let session = self.session.take().expect("opened above");
                            session.retry()
                        }
                        Op::Rewind { revision } => self.session()?.rewind(revision),
                        Op::Task {
                            task,
                            input,
                            report,
                        } => {
                            answered = Some(self.session()?.start_task(&task, &input, &report)?);
                            Ok(())
                        }
                    }
                }));
                // Every operation ends with a fresh open, as the CLI and serve give each
                // command its own `Session`: a new owner epoch reconciles a stopped or
                // failed step, and stop and steer reach the next turn (retry consumed its
                // session anyway).
                self.session = None;
                let _ = self.session();
                match outcome {
                    Ok(result) => result.map_err(turn_failed),
                    Err(_) => Err(internal(
                        "the operation panicked; committed state is intact, check `status`",
                    )),
                }
            }
        };
        let (unsent, stopped) = {
            let mut shared = lock(&self.entry.shared);
            shared.running = None;
            // A stop ends with its operation; none is left over for the next one.
            let stopped = shared.interrupt.swap(false, Ordering::SeqCst);
            shared.hurry.store(false, Ordering::SeqCst);
            (std::mem::take(&mut *lock(&shared.steering)), stopped)
        };
        // State changes sent too late for the turn's last step are kept now, each a step
        // of its own (or refused, `state_refused`), before anything queued after it runs.
        // Later ones are queued behind this job, so their order holds.
        if let Some(session) = self.session.as_mut() {
            let _ = session.commit_waiting_state();
        }
        // A failure after `stop` is the stop taking effect, and says so.
        let outcome = match outcome {
            Err(fault) if stopped && fault.kind == "failed" => Err(Fault {
                code: -32006,
                kind: "stopped",
                ..fault
            }),
            other => other,
        };
        *lock(&self.relay.request) = Value::Null;
        let head = head_state(&self.entry.dir)
            .map(|state| state["head"].clone())
            .unwrap_or(Value::Null);
        let Some(id) = id else {
            return;
        };
        let outcome = match outcome {
            Ok(()) => {
                let mut result = json!({
                    "session": self.entry.label,
                    "head": head,
                    "stopped": lock(&self.relay.stopped).take(),
                    "rewound": lock(&self.relay.rewound).take(),
                    "unsent": unsent,
                });
                if let Some(Value::Object(task)) = answered {
                    result.as_object_mut().expect("an object").extend(task);
                }
                Ok(result)
            }
            Err(fault) => Err(fault.with("head", head).with("unsent", json!(unsent))),
        };
        self.relay.app.out.reply(&id, outcome);
    }
}

use crate::protocol::TurnOp as Op;

/// The session directory param, resolved on this host.
trait Dir {
    fn dir(&self) -> std::result::Result<PathBuf, Fault>;
}
impl Dir for Params {
    /// The session directory as an absolute path whose existing part is canonical, so
    /// one directory has one name however it is spelled.
    fn dir(&self) -> std::result::Result<PathBuf, Fault> {
        let raw = Path::new(self.required("dir")?);
        let absolute = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| failed(format!("no current directory: {error}")))?
                .join(raw)
        };
        let mut existing = absolute.as_path();
        let mut rest = vec![];
        while !existing.exists() {
            match (existing.file_name(), existing.parent()) {
                (Some(name), Some(parent)) => {
                    rest.push(name.to_owned());
                    existing = parent;
                }
                _ => return Err(bad("`dir` is not a usable directory path")),
            }
        }
        let mut path = existing
            .canonicalize()
            .map_err(|error| bad(format!("`dir`: {error}")))?;
        if rest.iter().any(|part| part == "." || part == "..") {
            return Err(bad("`dir` is not a usable directory path"));
        }
        path.extend(rest.iter().rev());
        Ok(path)
    }
}

/// Applies the settings params of `create` and `settings` (provider, model, network,
/// toolchain, sources, isolation, effort, compaction, max_usd) as the CLI flags do; `route`
/// false leaves provider and model as already resolved.
fn apply_settings(
    settings: &mut Settings,
    params: &Params,
    route: bool,
) -> std::result::Result<(), Fault> {
    let provider = if route { params.str("provider")? } else { None };
    if let Some(provider) = provider {
        let model = params.str("model")?.map(String::from);
        let resolved = Settings::resolve(provider, model).map_err(bad)?;
        settings.provider = resolved.provider;
        settings.model = resolved.model;
    } else if route && let Some(model) = params.str("model")? {
        let (_, model) = Settings::route_of(&settings.provider, Some(model)).map_err(bad)?;
        settings.model = model;
    }
    if let Some(network) = params.bool("network")? {
        settings.network = network;
    }
    if let Some(toolchain) = params.bool("toolchain")? {
        settings.toolchain = toolchain;
    }
    match params.0.get("sources") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) => {
            let raw = list
                .iter()
                .map(|item| item.as_str().map(PathBuf::from))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| bad("`sources` must be an array of directory paths"))?;
            settings.sources = agent::sources(&raw).map_err(bad)?;
        }
        Some(_) => return Err(bad("`sources` must be an array of directory paths")),
    }
    if let Some(mode) = params.str("isolation")? {
        settings.isolation = Isolation::parse(mode)
            .ok_or_else(|| bad("`isolation` must be auto, required or off"))?;
    }
    if let Some(level) = params.str("effort")? {
        settings.effort = Effort::parse(level)
            .ok_or_else(|| bad("`effort` must be default, off, low, medium, high, xhigh or max"))?;
    }
    if let Some(mode) = params.str("compaction")? {
        settings.compaction =
            Compaction::parse(mode).ok_or_else(|| bad("`compaction` must be model or digest"))?;
    }
    match params.0.get("max_usd") {
        None => {}
        Some(Value::Null) => settings.max_usd = None,
        Some(value) => {
            settings.max_usd = Some(
                value
                    .as_f64()
                    .filter(|usd| usd.is_finite() && *usd >= 0.0)
                    .ok_or_else(|| {
                        bad("`max_usd` must be a non-negative number of dollars, or null")
                    })?,
            )
        }
    }
    agent::app_tools::apply_json(settings, &Value::Object(params.0.clone())).map_err(bad)?;
    agent::describe_effort(&settings.provider, &settings.model, settings.effort).map_err(bad)?;
    Ok(())
}

struct Server {
    out: Arc<Out>,
    sessions: Sessions,
}

impl Backend for Server {
    fn call(&self, method: &str, params: Params, id: Option<Value>) -> Reply {
        Server::call(self, method, params, id)
    }
}

impl Server {
    /// The open session for `dir`, opening it (lazily, in a new worker) if needed.
    fn entry(&self, dir: &Path) -> std::result::Result<Arc<Entry>, Fault> {
        let mut sessions = lock(&self.sessions);
        if let Some(entry) = sessions.get(dir) {
            if lock(&entry.shared).closing {
                return Err(busy("the session is closing"));
            }
            return Ok(entry.clone());
        }
        if !dir.join("session.json").is_file() {
            return Err(not_found(format!(
                "no session in {}; create one first",
                dir.display()
            )));
        }
        Ok(self.spawn(&mut sessions, dir))
    }
    /// Starts a worker for `dir`; its `Session` opens on its first job.
    fn spawn(&self, sessions: &mut BTreeMap<PathBuf, Arc<Entry>>, dir: &Path) -> Arc<Entry> {
        let (jobs, queue) = mpsc::channel::<Job>();
        let label = dir.to_string_lossy().into_owned();
        let desk = Arc::new(AppDesk::default());
        let entry = Arc::new(Entry {
            desk: desk.clone(),
            dir: dir.to_path_buf(),
            label: label.clone(),
            jobs,
            shared: Mutex::new(Shared {
                running: None,
                closing: false,
                interrupt: Arc::default(),
                steering: Arc::default(),
                hurry: Arc::default(),
                state: Arc::default(),
            }),
        });
        let actor = Actor {
            entry: entry.clone(),
            relay: {
                let request = Arc::new(Mutex::new(Value::Null));
                Arc::new(Relay {
                    request: request.clone(),
                    stopped: Mutex::new(None),
                    rewound: Mutex::new(None),
                    app: Arc::new(AppRelay {
                        out: self.out.clone(),
                        session: label,
                        request,
                        desk,
                    }),
                })
            },
            session: None,
            sessions: self.sessions.clone(),
            quit: false,
        };
        std::thread::Builder::new()
            .name("spindle-api-session".into())
            .spawn(move || actor.run(queue))
            .expect("spawn a session worker");
        sessions.insert(dir.to_path_buf(), entry.clone());
        entry
    }
    /// Queues `job` on the session's worker, which answers `id` when it is done.
    fn queue(
        &self,
        entry: &Entry,
        id: Option<Value>,
        job: impl FnOnce(&mut Actor) -> Outcome + Send + 'static,
    ) -> Reply {
        let out = self.out.clone();
        let sent = self.send(
            entry,
            Box::new(move |actor: &mut Actor| {
                let outcome = catch_unwind(AssertUnwindSafe(|| job(actor))).unwrap_or_else(|_| {
                    actor.session = None;
                    Err(internal("the call panicked; committed state is intact"))
                });
                if let Some(id) = id {
                    out.reply(&id, outcome);
                }
            }),
        );
        if sent {
            Reply::Later
        } else {
            Reply::Now(Err(busy("the session is closing")))
        }
    }
    /// Hands a job to the session's worker unless the session is closing, checked under
    /// the session list's lock (which `Actor::retire` holds to close it).
    fn send(&self, entry: &Entry, job: Job) -> bool {
        let _sessions = lock(&self.sessions);
        !lock(&entry.shared).closing && entry.jobs.send(job).is_ok()
    }
    /// Queues a job that needs the session idle (no turn running).
    fn queue_idle(
        &self,
        entry: &Entry,
        id: Option<Value>,
        job: impl FnOnce(&mut Actor) -> Outcome + Send + 'static,
    ) -> Reply {
        if let Some(running) = lock(&entry.shared).running {
            return Reply::Now(Err(busy(format!(
                "a {running} is running in this session; wait for it or stop it"
            ))));
        }
        self.queue(entry, id, job)
    }

    fn call(&self, method: &str, params: Params, id: Option<Value>) -> Reply {
        let now = |outcome: Outcome| Reply::Now(outcome);
        let result = (|| -> std::result::Result<Reply, Fault> {
            Ok(match method {
                "info" => {
                    params.allow(&[])?;
                    now(Ok(info()))
                }
                "providers" => {
                    params.allow(&[])?;
                    // Readiness may run credential commands: a helper thread answers.
                    let out = self.out.clone();
                    std::thread::Builder::new()
                        .name("spindle-api-providers".into())
                        .spawn(move || {
                            let outcome = catch_unwind(providers_json)
                                .unwrap_or_else(|_| Err("listing providers panicked".into()))
                                .map_err(failed);
                            if let Some(id) = id {
                                out.reply(&id, outcome);
                            }
                        })
                        .map_err(|error| failed(format!("cannot list providers: {error}")))?;
                    Reply::Later
                }
                "list" => {
                    params.allow(&[])?;
                    let sessions = lock(&self.sessions);
                    let list: Vec<Value> = sessions
                        .values()
                        .map(|entry| {
                            let shared = lock(&entry.shared);
                            json!({"session": entry.label, "busy": shared.running.is_some(), "running": shared.running})
                        })
                        .collect();
                    now(Ok(json!({"sessions": list})))
                }
                "create" => self.create(params, id)?,
                "open" => {
                    params.allow(&["dir"])?;
                    let entry = self.entry(&params.dir()?)?;
                    self.status(&entry, id)
                }
                "close" => {
                    params.allow(&["dir"])?;
                    let dir = params.dir()?;
                    let sessions = lock(&self.sessions);
                    let Some(entry) = sessions.get(&dir).cloned() else {
                        return Ok(now(Ok(
                            json!({"session": dir.to_string_lossy(), "closed": false}),
                        )));
                    };
                    let mut shared = lock(&entry.shared);
                    if shared.closing {
                        return Err(busy("the session is closing"));
                    }
                    if let Some(running) = shared.running {
                        return Err(busy(format!("a {running} is running; stop it first")));
                    }
                    // Nothing more is queued; the worker answers once it let go.
                    shared.closing = true;
                    let out = self.out.clone();
                    let close = move |actor: &mut Actor| {
                        actor.retire();
                        if let Some(id) = id {
                            let closed = json!({"session": actor.entry.label, "closed": true});
                            out.reply(&id, Ok(closed));
                        }
                    };
                    match entry.jobs.send(Box::new(close)) {
                        Ok(()) => Reply::Later,
                        Err(_) => now(Err(failed("the session worker has ended"))),
                    }
                }
                "prompt" | "continue" | "retry" | "rewind" | "task_start" => {
                    self.start(method, params, id)?
                }
                "stop" => {
                    params.allow(&["dir"])?;
                    let entry = self.known(&params.dir()?)?;
                    let shared = lock(&entry.shared);
                    if shared.running.is_none() {
                        return Err(not_running("nothing is running in this session"));
                    }
                    agent::tasks::stop_all(&entry.dir);
                    shared.interrupt.store(true, Ordering::SeqCst);
                    now(Ok(json!({"session": entry.label, "stopping": true})))
                }
                "steer" => {
                    params.allow(&["dir", "text", "now"])?;
                    // `now` cuts the reply being written so the waiting messages are read
                    // first; alone, it asks that of the messages already waiting.
                    let hurry = params.bool("now")? == Some(true);
                    let text = match params.str("text")? {
                        None if hurry => None,
                        text => Some(
                            text.filter(|text| {
                                !text.trim().is_empty() && text.len() <= agent::MAX_STEER
                            })
                            .ok_or_else(|| bad("`text` must be 1 to 65536 bytes and not blank"))?,
                        ),
                    };
                    let entry = self.known(&params.dir()?)?;
                    let shared = lock(&entry.shared);
                    if !matches!(shared.running, Some("prompt" | "continue" | "retry")) {
                        return Err(not_running(
                            "no turn is running in this session; send it with `prompt`",
                        ));
                    }
                    let mut queue = lock(&shared.steering);
                    match text {
                        Some(_) if queue.len() >= MAX_STEERS => {
                            return Err(busy(format!(
                                "{MAX_STEERS} messages are already waiting for the turn"
                            )));
                        }
                        Some(text) => queue.push(text.to_string()),
                        None if queue.is_empty() => {
                            return Err(not_running("no message is waiting for the turn"));
                        }
                        None => {}
                    }
                    if hurry {
                        shared.hurry.store(true, Ordering::SeqCst);
                    }
                    now(Ok(json!({"session": entry.label, "queued": queue.len()})))
                }
                "task_wait" => {
                    params.allow(&["dir", "task", "timeout_s"])?;
                    let dir = self.existing(&params)?;
                    let key =
                        agent::tasks::key(&dir, params.required("task")?).map_err(not_found)?;
                    let timeout = params.u64("timeout_s")?.unwrap_or(600);
                    if timeout > 86400 {
                        return Err(bad("timeout_s must be 0-86400"));
                    }
                    let out = self.out.clone();
                    std::thread::spawn(move || {
                        let result = agent::tasks::wait(
                            &dir,
                            &key,
                            Duration::from_secs(timeout),
                            &AtomicBool::new(false),
                        )
                        .map_err(failed);
                        if let Some(id) = id {
                            out.reply(&id, result);
                        }
                    });
                    Reply::Later
                }
                "task_stop" => {
                    params.allow(&["dir", "task"])?;
                    let dir = self.existing(&params)?;
                    let key =
                        agent::tasks::key(&dir, params.required("task")?).map_err(not_found)?;
                    let stopped = agent::tasks::stop(&dir, &key).map_err(failed)?;
                    now(Ok(json!({"task":key,"stopping":stopped})))
                }
                "tasks" => {
                    params.allow(&["dir"])?;
                    let dir = self.existing(&params)?;
                    now(super::tasks_json(&dir).map_err(failed))
                }
                "recover" => {
                    params.allow(&["dir"])?;
                    let dir = self.existing(&params)?;
                    now(super::recovery(&dir).map_err(failed))
                }
                "state" => {
                    params.allow(&["dir", "revision"])?;
                    let dir = self.existing(&params)?;
                    let revision = params.u64("revision")?;
                    now(super::state_at(&dir, revision).map_err(not_found))
                }
                "state_set" => {
                    params.allow(&["dir", "patch", "expected_head"])?;
                    let patch = params
                        .0
                        .get("patch")
                        .filter(|patch| patch.is_object())
                        .cloned()
                        .ok_or_else(|| bad("`patch` is a JSON merge patch: an object"))?;
                    if patch.to_string().len() > crate::transition::MAX_STATE {
                        return Err(bad("`patch` is over 1 MiB"));
                    }
                    let expected = params.u64("expected_head")?;
                    let entry = self.entry(&params.dir()?)?;
                    {
                        // While a turn runs the change waits for its next step boundary.
                        let shared = lock(&entry.shared);
                        if matches!(shared.running, Some("prompt" | "continue" | "retry")) {
                            if expected.is_some() {
                                return Err(busy(
                                    "a turn is running; `expected_head` needs an idle session",
                                ));
                            }
                            let mut queue = lock(&shared.state);
                            if queue.len() >= MAX_STEERS {
                                return Err(busy(format!(
                                    "{MAX_STEERS} state changes are already waiting for the turn"
                                )));
                            }
                            queue.push(patch);
                            return Ok(now(Ok(
                                json!({"session": entry.label, "queued": queue.len()}),
                            )));
                        }
                    }
                    let request = id.clone().unwrap_or(Value::Null);
                    self.queue_idle(&entry, id, move |actor| {
                        actor.head_is(expected)?;
                        // Its `state` event names this request, as a turn's events do.
                        *lock(&actor.relay.request) = request;
                        let revision = actor
                            .session()
                            .and_then(|session| session.set_state(&patch))
                            .map_err(conflict);
                        *lock(&actor.relay.request) = Value::Null;
                        Ok(json!({"session": actor.entry.label, "revision": revision?}))
                    })
                }
                "app_tool_result" => {
                    params.allow(&[
                        "dir", "call", "content", "error", "failed", "unknown", "files", "state",
                        "details",
                    ])?;
                    let call = params
                        .str("call")?
                        .ok_or_else(|| bad("`call` names the app tool call answered"))?
                        .to_string();
                    let mut answer = params.0.clone();
                    answer.remove("dir");
                    answer.remove("call");
                    let answer = AppAnswer::from_json(&Value::Object(answer)).map_err(bad)?;
                    let entry = self.known(&params.dir()?)?;
                    let mut waiting = lock(&entry.desk.waiting);
                    match waiting.get_mut(&call) {
                        Some(slot) if slot.is_none() => {
                            *slot = Some(answer);
                        }
                        _ => {
                            return Err(conflict(
                                "that app tool call is not waiting for an answer",
                            ));
                        }
                    }
                    drop(waiting);
                    entry.desk.changed.notify_all();
                    now(Ok(
                        json!({"session": entry.label, "call": call, "accepted": true}),
                    ))
                }
                "status" => {
                    params.allow(&["dir"])?;
                    let entry = self.entry(&params.dir()?)?;
                    self.status(&entry, id)
                }
                "history" | "files" => {
                    params.allow(&["dir", "revision", "from"])?;
                    let dir = self.existing(&params)?;
                    let revision = params.u64("revision")?;
                    let from =
                        usize::try_from(params.u64("from")?.unwrap_or(0)).unwrap_or(usize::MAX);
                    let read = if method == "history" {
                        messages_at(&dir, revision, from).map(|mut history| {
                            crate::agent::annotate_display(&mut history["messages"]);
                            crate::agent::annotate_steering(&mut history["messages"]);
                            history
                        })
                    } else {
                        files_at(&dir, revision)
                    };
                    // A revision that was asked for and cannot be read is not committed.
                    now(read.map_err(|error| match revision {
                        Some(_) => not_found(error),
                        None => failed(error),
                    }))
                }
                "read" => {
                    params.allow(&["dir", "path", "revision"])?;
                    let dir = self.existing(&params)?;
                    let path = params.required("path")?;
                    let bytes = read_at(&dir, params.u64("revision")?, path).map_err(not_found)?;
                    let size = bytes.len();
                    let (encoding, content) = match String::from_utf8(bytes) {
                        Ok(text) => ("utf8", text),
                        Err(error) => ("base64", base64_encode(error.as_bytes())),
                    };
                    now(Ok(
                        json!({"path": path, "size": size, "encoding": encoding, "content": content}),
                    ))
                }
                "usage" => {
                    params.allow(&["dir"])?;
                    let dir = self.existing(&params)?;
                    now(super::usage_json(&dir).map_err(failed))
                }
                "targets" => {
                    params.allow(&["dir"])?;
                    let entry = self.entry(&params.dir()?)?;
                    self.queue_idle(&entry, id, |actor| {
                        let targets = actor.session().and_then(Session::targets).map_err(failed)?;
                        Ok(json!({"targets": targets.iter().map(target_json).collect::<Vec<_>>()}))
                    })
                }
                "export_session" => {
                    params.allow(&["dir", "target", "keep"])?;
                    let entry = self.entry(&params.dir()?)?;
                    let target = PathBuf::from(params.required("target")?);
                    let keep = params.bool("keep")?.unwrap_or(false);
                    self.queue_idle(&entry, id, move |actor| {
                        let exported = actor
                            .session()
                            .and_then(|session| session.export_session(&target, keep))
                            .map_err(failed)?;
                        if !keep {
                            // It moved: this host holds it no more (its copy is sealed).
                            actor.retire();
                        }
                        Ok(exported)
                    })
                }
                "import_session" => self.import_session(params, id)?,
                "export" => {
                    params.allow(&["dir", "target"])?;
                    let entry = self.entry(&params.dir()?)?;
                    let target = PathBuf::from(params.required("target")?);
                    self.queue_idle(&entry, id, move |actor| {
                        let count = actor
                            .session()
                            .and_then(|session| session.export(&target))
                            .map_err(failed)?;
                        Ok(json!({"target": target.to_string_lossy(), "files": count}))
                    })
                }
                "settings" => {
                    let mut keys = vec!["dir"];
                    keys.extend(SETTINGS);
                    params.allow(&keys)?;
                    let dir = self.existing(&params)?;
                    if !SETTINGS.iter().any(|key| params.0.contains_key(*key)) {
                        let settings = Settings::load(&dir).map_err(failed)?;
                        return Ok(now(Ok(settings_json(&settings))));
                    }
                    let entry = self.entry(&dir)?;
                    // A new provider's credentials are checked by the worker.
                    self.queue_idle(&entry, id, move |actor| {
                        let mut settings = Settings::load(&actor.entry.dir).map_err(failed)?;
                        apply_settings(&mut settings, &params, true)?;
                        actor
                            .session()
                            .and_then(|session| session.set_settings(settings.clone()))
                            .map_err(conflict)?;
                        Ok(settings_json(&settings))
                    })
                }
                "doctor" => {
                    params.allow(&["dir"])?;
                    let entry = self.entry(&params.dir()?)?;
                    self.queue_idle(&entry, id, |actor| {
                        // Doctor opens the store itself; the worker lets go of it meanwhile.
                        actor.session = None;
                        let findings = agent::doctor(&actor.entry.dir);
                        let _ = actor.session();
                        Ok(json!({
                            "ok": findings.iter().all(|finding| finding.ok),
                            "findings": findings.iter().map(finding_json).collect::<Vec<_>>(),
                        }))
                    })
                }
                _ => {
                    return Err(Fault::new(
                        -32601,
                        "method_not_found",
                        format!("no method `{method}`"),
                    ));
                }
            })
        })();
        result.unwrap_or_else(|fault| Reply::Now(Err(fault)))
    }

    /// A session this process has open (stop and steer act only on those).
    fn known(&self, dir: &Path) -> std::result::Result<Arc<Entry>, Fault> {
        lock(&self.sessions)
            .get(dir)
            .cloned()
            .ok_or_else(|| not_running("this session is not open in this process"))
    }

    /// The directory of an existing session, open here or not (read-only calls).
    fn existing(&self, params: &Params) -> std::result::Result<PathBuf, Fault> {
        let dir = params.dir()?;
        if !dir.join("session.json").is_file() {
            return Err(not_found(format!("no session in {}", dir.display())));
        }
        Ok(dir)
    }

    /// `import_session`: a session bundle written as a new session directory, then
    /// opened here (its worker writes it, so stop and steer elsewhere never wait).
    fn import_session(
        &self,
        params: Params,
        id: Option<Value>,
    ) -> std::result::Result<Reply, Fault> {
        params.allow(&["dir", "source"])?;
        let dir = params.dir()?;
        let source = PathBuf::from(params.required("source")?);
        let mut sessions = lock(&self.sessions);
        if sessions.contains_key(&dir) || dir.join("store").exists() {
            return Err(conflict(format!(
                "{} already holds a session; import into a new directory",
                dir.display()
            )));
        }
        let entry = self.spawn(&mut sessions, &dir);
        drop(sessions);
        Ok(self.queue(&entry, id, move |actor| {
            let imported = match agent::import_session(&source, &actor.entry.dir) {
                Ok(imported) => imported,
                Err(error) => {
                    actor.retire();
                    return Err(failed(error));
                }
            };
            let session = match Session::open(&actor.entry.dir, actor.relay.clone()) {
                Ok(session) => session,
                Err(error) => {
                    actor.retire();
                    return Err(failed(error));
                }
            };
            actor.adopt(&session);
            actor.session = Some(session);
            let mut status = status_of(actor)?;
            status["imported"] = imported;
            Ok(status)
        }))
    }

    fn create(&self, params: Params, id: Option<Value>) -> std::result::Result<Reply, Fault> {
        let mut keys = vec!["dir", "files", "import"];
        keys.extend(SETTINGS);
        params.allow(&keys)?;
        let dir = params.dir()?;
        let provider = params.required("provider")?.to_string();
        let model = params.str("model")?.map(String::from);
        let import = params.str("import")?.map(PathBuf::from);
        let given = files_param(params.0.get("files"))?;
        let mut sessions = lock(&self.sessions);
        if sessions.contains_key(&dir) || dir.join("store").exists() {
            return Err(conflict(format!(
                "{} already holds a session; open it instead",
                dir.display()
            )));
        }
        let entry = self.spawn(&mut sessions, &dir);
        drop(sessions);
        // The worker checks the provider's credentials and reads the import.
        Ok(self.queue(&entry, id, move |actor| {
            let prepared = (|| -> std::result::Result<_, Fault> {
                let mut settings = Settings::resolve(&provider, model).map_err(bad)?;
                apply_settings(&mut settings, &params, false)?;
                let (mut files, imported) = match import {
                    Some(source) => {
                        let (files, skipped) = agent::import_files(&source).map_err(bad)?;
                        let count = files.len();
                        (files, json!({"files": count, "skipped": skipped}))
                    }
                    None => (vec![], Value::Null),
                };
                for (path, bytes, executable) in given {
                    files.retain(|(kept, _, _)| *kept != path);
                    files.push((path, bytes, executable));
                }
                Ok((settings, files, imported))
            })();
            let (settings, files, imported) = match prepared {
                Ok(prepared) => prepared,
                Err(fault) => {
                    actor.retire();
                    return Err(fault);
                }
            };
            let made = Session::create(&actor.entry.dir, settings, files, actor.relay.clone());
            let session = match made {
                Ok(session) => session,
                Err(error) => {
                    actor.retire();
                    return Err(failed(error));
                }
            };
            actor.adopt(&session);
            actor.session = Some(session);
            let mut status = status_of(actor)?;
            status["imported"] = imported;
            Ok(status)
        }))
    }

    /// Starts a turn-like operation; its request is answered when it ends.
    fn start(
        &self,
        method: &str,
        params: Params,
        id: Option<Value>,
    ) -> std::result::Result<Reply, Fault> {
        let (kind, op) = crate::protocol::turn_op(method, &params)?;
        let expected = params.u64("expected_head")?;
        let entry = self.entry(&params.dir()?)?;
        // Checked, queued and marked running in one critical section (session list,
        // then session), so a close or shutdown cannot slip in between. The worker
        // checks `expected_head` once it holds the session.
        let _sessions = lock(&self.sessions);
        let mut shared = lock(&entry.shared);
        if shared.closing {
            return Err(busy("the session is closing"));
        }
        if let Some(running) = shared.running {
            return Err(busy(format!(
                "a {running} is already running in this session"
            )));
        }
        // Cleared before the worker can see the job, so no earlier stop reaches it.
        shared.running = Some(kind);
        shared.interrupt.store(false, Ordering::SeqCst);
        let sent = entry.jobs.send(Box::new(move |actor: &mut Actor| {
            actor.turn(op, expected, id)
        }));
        if sent.is_err() {
            shared.running = None;
            return Err(failed("the session worker has ended"));
        }
        Ok(Reply::Later)
    }

    /// `status`: answered by the worker when idle (with its store reconciliation),
    /// from committed state alone while a turn runs.
    fn status(&self, entry: &Arc<Entry>, id: Option<Value>) -> Reply {
        let running = lock(&entry.shared).running;
        let Some(running) = running else {
            return self.queue(entry, id, status_of);
        };
        let outcome = (|| -> Outcome {
            let settings = Settings::load(&entry.dir).map_err(failed)?;
            let mut status = json!({
                "session": entry.label,
                "provider": settings.provider,
                "model": settings.model,
                "epoch": null,
                "state": format!("running a {running}"),
                "unresolved": null,
                "abandoned": null,
                "rejected": null,
                "isolation": settings.isolation.as_str(),
                "effort": settings.effort.as_str(),
            });
            merge(&mut status, head_state(&entry.dir).map_err(failed)?);
            status["busy"] = json!(true);
            status["running"] = json!(running);
            // Its owner is running it: nothing was left unfinished by a stopped one.
            status["wake"] = json!({"work": "running", "why": null});
            status["settings"] = settings_json(&settings);
            Ok(status)
        })();
        Reply::Now(outcome)
    }

    /// End of input: running turns are asked to stop, every worker lets go of its
    /// session, and the process waits a little for them before it exits. A turn cut
    /// off here resumes with `continue` like any interrupted run.
    fn shutdown(&self) {
        for entry in lock(&self.sessions).values() {
            agent::tasks::stop_all(&entry.dir);
            let mut shared = lock(&entry.shared);
            shared.closing = true;
            shared.interrupt.store(true, Ordering::SeqCst);
            let _ = entry.jobs.send(Box::new(Actor::retire));
        }
        let started = Instant::now();
        while !lock(&self.sessions).is_empty() && started.elapsed() < SHUTDOWN {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Idle `status`: the session's own report merged with its committed head (a session
/// without turns has no report: nothing can be unresolved yet).
fn status_of(actor: &mut Actor) -> Outcome {
    let entry = actor.entry.clone();
    let session = actor.session().map_err(failed)?;
    let state = head_state(&entry.dir).map_err(failed)?;
    let settings = Settings::load(&entry.dir).map_err(failed)?;
    let mut value = if state["head"] == 0 {
        json!({
            "provider": settings.provider,
            "model": settings.model,
            "epoch": null,
            "state": "no turns yet",
            "unresolved": 0,
            "abandoned": 0,
            "rejected": 0,
            "isolation": settings.isolation.as_str(),
            "effort": settings.effort.as_str(),
        })
    } else {
        status_json(&session.status().map_err(failed)?)
    };
    value["session"] = json!(entry.label);
    merge(&mut value, state);
    value["busy"] = json!(false);
    value["running"] = Value::Null;
    value["settings"] = settings_json(&settings);
    Ok(value)
}

fn merge(into: &mut Value, from: Value) {
    if let (Value::Object(into), Value::Object(from)) = (into, from) {
        into.extend(from);
    }
}

/// `info`: API version, transport features and this build.
fn info() -> Value {
    static BUILD: OnceLock<String> = OnceLock::new();
    let build = BUILD.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| crate::upgrade::fingerprint(&exe).ok())
            .unwrap_or_else(|| "unknown".into())
    });
    json!({
        "api_version": super::API_VERSION,
        "transport": "stdio",
        "features": FEATURES,
        "version": env!("CARGO_PKG_VERSION"),
        "schema": crate::CAPABILITIES.schema,
        "engine": crate::turn::ENGINE,
        "build": build,
    })
}

/// One line of input, or why there is none.
enum Line {
    Text(Vec<u8>),
    TooLong,
    End,
}

/// Reads up to the next newline, keeping at most `limit` bytes of it.
fn read_line(input: &mut impl BufRead, limit: usize) -> std::io::Result<Line> {
    let mut line = Vec::new();
    let mut too_long = false;
    loop {
        let buffer = match input.fill_buf() {
            Ok(buffer) => buffer,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if buffer.is_empty() {
            return Ok(match (line.is_empty() && !too_long, too_long) {
                (true, _) => Line::End,
                (_, true) => Line::TooLong,
                _ => Line::Text(line),
            });
        }
        let (part, done) = match buffer.iter().position(|&b| b == b'\n') {
            Some(at) => (&buffer[..at], at + 1),
            None => (buffer, buffer.len()),
        };
        let ended = done > part.len();
        if !too_long {
            if line.len() + part.len() > limit {
                too_long = true;
                line = Vec::new();
            } else {
                line.extend_from_slice(part);
            }
        }
        input.consume(done);
        if ended {
            return Ok(if too_long {
                Line::TooLong
            } else {
                Line::Text(line)
            });
        }
    }
}

/// Serves the protocol: requests from `input`, frames to `output`, until end of input.
pub fn serve(mut input: impl BufRead, output: Box<dyn Write + Send>) -> agent::Result<()> {
    agent::install_host_hooks();
    let server = Server {
        out: Arc::new(Out(Mutex::new(output))),
        sessions: Arc::default(),
    };
    loop {
        match read_line(&mut input, MAX_LINE) {
            Ok(Line::End) | Err(_) => break,
            Ok(Line::TooLong) => server.out.reply(
                &Value::Null,
                Err(invalid_request("a request line is limited to 64 MiB")),
            ),
            Ok(Line::Text(line)) => {
                if !line.iter().all(u8::is_ascii_whitespace) {
                    crate::protocol::handle(&server, &line, |frame| server.out.send(frame));
                }
            }
        }
    }
    server.shutdown();
    Ok(())
}

/// `spindle api`: serves stdin/stdout. File descriptor 1 is pointed at stderr first and
/// fd 0 at /dev/null, with the real ones kept privately (close-on-exec), so nothing
/// else in the process or a child it starts can write into, or read from, the
/// protocol stream.
pub fn run() -> agent::Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::FromRawFd;
        let keep = |fd: i32| {
            // SAFETY: duplicates a descriptor this process owns; checked below.
            let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
            if copy < 0 {
                Err(format!(
                    "cannot keep fd {fd}: {}",
                    std::io::Error::last_os_error()
                ))
            } else {
                Ok(copy)
            }
        };
        let (input, output) = (keep(0)?, keep(1)?);
        // SAFETY: plain descriptor calls on descriptors this process owns.
        unsafe {
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
            if null >= 0 {
                libc::dup2(null, 0);
                libc::close(null);
            }
            libc::dup2(2, 1);
        }
        // SAFETY: each copy is a fresh descriptor owned by nothing else.
        let (input, output) = unsafe {
            (
                std::fs::File::from_raw_fd(input),
                std::fs::File::from_raw_fd(output),
            )
        };
        serve(
            std::io::BufReader::with_capacity(1 << 16, input),
            Box::new(output),
        )
    }
    #[cfg(not(unix))]
    {
        serve(std::io::stdin().lock(), Box::new(std::io::stdout()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn frames(input: &str) -> Vec<Value> {
        let capture = Capture::default();
        serve(input.as_bytes(), Box::new(capture.clone())).unwrap();
        let bytes = lock(&capture.0).clone();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).expect("every frame is one JSON line"))
            .collect()
    }

    #[test]
    fn malformed_input_gets_json_rpc_errors_and_never_ends_the_server() {
        let missing = std::env::temp_dir().join(format!("spindle-api-none-{}", std::process::id()));
        let input = [
            "{not json".to_string(),
            "[1,2]".into(),
            "42".into(),
            r#"{"jsonrpc":"2.0","id":{"x":1},"method":"info"}"#.into(),
            r#"{"jsonrpc":"1.0","id":1,"method":"info"}"#.into(),
            r#"{"jsonrpc":"2.0","id":2,"method":7}"#.into(),
            r#"{"jsonrpc":"2.0","id":3,"method":"nope"}"#.into(),
            r#"{"jsonrpc":"2.0","id":4,"method":"info","params":[1]}"#.into(),
            r#"{"jsonrpc":"2.0","id":5,"method":"info","params":{"x":1}}"#.into(),
            // Built as JSON: a Windows path's backslashes must be escaped.
            json!({"jsonrpc": "2.0", "id": "six", "method": "status",
                "params": {"dir": missing.to_string_lossy()}})
            .to_string(),
            r#"{"jsonrpc":"2.0","id":7,"method":"stop","params":{"dir":"/"}}"#.into(),
            r#"{"jsonrpc":"2.0","method":"info"}"#.into(),
            "".into(),
            "\u{0}\u{ff}".into(),
            r#"{"jsonrpc":"2.0","id":8,"method":"prompt","params":{"dir":"/x","text":"  "}}"#
                .into(),
            r#"{"jsonrpc":"2.0","id":9,"method":"info"}"#.into(),
        ]
        .join("\n");
        let frames = frames(&input);
        let code = |index: usize| frames[index]["error"]["code"].as_i64().unwrap();
        let kind = |index: usize| frames[index]["error"]["data"]["code"].clone();
        assert_eq!(code(0), -32700);
        assert_eq!(frames[0]["id"], Value::Null);
        assert_eq!(code(1), -32600, "batch");
        assert_eq!(code(2), -32600);
        assert_eq!(code(3), -32600, "object id");
        assert_eq!((code(4), frames[4]["id"].clone()), (-32600, json!(1)));
        assert_eq!(code(5), -32600);
        assert_eq!((code(6), kind(6)), (-32601, json!("method_not_found")));
        assert_eq!(code(7), -32602);
        assert_eq!((code(8), kind(8)), (-32602, json!("bad_request")));
        assert_eq!(
            (kind(9), frames[9]["id"].clone()),
            (json!("not_found"), json!("six"))
        );
        assert_eq!(kind(10), json!("not_running"));
        // The notification (no id) gets nothing; the blank line is skipped.
        assert_eq!(code(11), -32700, "control characters are a parse error");
        assert_eq!(kind(12), json!("bad_request"));
        assert_eq!(frames[13]["id"], 9);
        assert_eq!(
            frames[13]["result"]["api_version"],
            super::super::API_VERSION
        );
        assert_eq!(frames.len(), 14);
    }

    /// Every frame of a fixed request set, byte for byte as recorded before the
    /// protocol moved to `crate::protocol` (the build fingerprint aside): malformed
    /// requests, and each method's param checks that need no session.
    #[cfg(unix)]
    #[test]
    fn frames_match_the_recorded_protocol_bytes() {
        let input = include_str!("../../tests/fixtures/protocol/requests.ndjson");
        let capture = Capture::default();
        serve(input.as_bytes(), Box::new(capture.clone())).unwrap();
        let written = String::from_utf8(lock(&capture.0).clone()).unwrap();
        let build = format!("\"build\":{}", info()["build"]);
        assert_eq!(
            written.replace(&build, "\"build\":\"BUILD\""),
            include_str!("../../tests/fixtures/protocol/frames.ndjson")
        );
    }

    #[test]
    fn long_lines_are_refused_and_reading_resumes_after_them() {
        let mut input = std::io::BufReader::with_capacity(4, &b"0123456789\nok\n"[..]);
        assert!(matches!(read_line(&mut input, 5).unwrap(), Line::TooLong));
        assert!(matches!(read_line(&mut input, 5).unwrap(), Line::Text(t) if t == b"ok"));
        assert!(matches!(read_line(&mut input, 5).unwrap(), Line::End));
        let mut last = &b"tail"[..];
        assert!(matches!(read_line(&mut last, 5).unwrap(), Line::Text(t) if t == b"tail"));
    }

    #[test]
    fn files_take_text_or_base64_and_refuse_escaping_paths() {
        let files = files_param(Some(&json!([
            {"path": "a.txt", "text": "hi"},
            {"path": "bin/x", "base64": "AAE=", "executable": true},
        ])))
        .unwrap();
        assert_eq!(files[0], ("a.txt".to_string(), b"hi".to_vec(), false));
        assert_eq!(files[1], ("bin/x".to_string(), vec![0, 1], true));
        for bad in [
            json!([{"path": "../x", "text": ""}]),
            json!([{"path": "a", "text": "x", "base64": "eA=="}]),
            json!([{"path": "a"}]),
            json!([{"path": "a", "text": "x", "mode": 1}]),
            json!({"path": "a"}),
        ] {
            assert_eq!(files_param(Some(&bad)).unwrap_err().code, -32602, "{bad}");
        }
    }
}
