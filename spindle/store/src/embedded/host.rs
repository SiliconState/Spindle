//! One embedded session: the native session's turn loop over the driver, turned
//! inside out. Where the native host blocks on I/O (a model request, an app tool, a
//! command, a backoff), this host parks the need and returns `Work` for the app; the
//! app's answer resumes the loop at the same point. Decisions stay in the driver.
use super::env::{Catalog, Events, Facts, Queue, lock};
use super::storage::{Storage, memory_store};
use crate::artifact::Stage;
use crate::broker::{
    AppReservation, CommittedEffect, DispatchOutcome, DispatchRequest, ModelReservation,
    RunnerReservation, RunnerResult,
};
use crate::bundle::{self, BundleFile};
use crate::completion::Attempt;
use crate::driver::app_tools::{AppAnswer, BuiltinTools};
use crate::driver::task_tool::{self, Workspace};
use crate::driver::{
    self, Answer, CompactCall, CompactReplacement, Compaction, Driver, Event, File, Journaled,
    ModelFailure, ModelNeed, ModelSent, Need, Retry, RunCall, Runner, Settings, Start, StartState,
    SummaryJournal, SummaryNeed, ToolNeed, ToolRan,
};
use crate::meta::TaskRun;
use crate::workspace::Tree;
use crate::{Error, ModelError, Store};
use dext_core::turn::{Phase, Stop};
use dext_native::attempt::{self, AttemptError, WireAttempt};
use dext_native::streaming::StreamUpdate;
use dext_provider_catalog::catalog::RequestContract;
use dext_transcript::Block;
use dext_transcript::checkpoint::NativeResponse;
use dext_transcript::turn::{Effect, TurnContinuation};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub type Result<T> = std::result::Result<T, String>;
const SCOPE: &str = "operator";
/// Where a key goes in an authentication header template (`Work::Authorize::auth`).
pub const KEY_SLOT: &str = "{key}";
/// Where a value read from the key's own claims goes (`Work::Authorize::claims`).
pub const ACCOUNT_SLOT: &str = "{account}";

/// A header value the app reads from the key itself, a JWT (the ChatGPT backend takes
/// the account from its token): `slot` in an `auth` template is replaced by the claim
/// at `path` in the token's payload.
#[derive(Debug, Clone, PartialEq)]
pub struct Claim {
    pub slot: String,
    pub path: Vec<String>,
}

/// A request's headers, split as `Work::Authorize` carries them.
pub(crate) struct HeaderFacts {
    pub headers: Vec<(String, String)>,
    pub auth: Vec<(String, String)>,
    pub claims: Vec<Claim>,
}
/// The settings' mirror beside a native session's store, which a native host also
/// reads as the mark of a session directory.
const SETTINGS_SIDECAR: &str = "session.json";
/// A task directory's identity record: the input a later start must repeat.
const TASK_SIDECAR: &str = "task.json";

/// The identity of runs nothing isolates: the store's own file tools (no process
/// runs) and an app's runner (whatever isolation it has, Spindle cannot vouch for).
/// It is the native host runner's, so their attempts label as host runs.
pub fn host_helper() -> [u8; 32] {
    Sha256::digest(b"dext-host-runner-v1 (no isolation)").into()
}

/// A task's short name: as the native host names its directory.
pub fn task_key(call: &str) -> String {
    Sha256::digest(call.as_bytes())[..10]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Where a host's script call numbers start: at random (kept below 2^52, so a number
/// in JavaScript holds it exactly).
fn first_call() -> u64 {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    u64::from_le_bytes(bytes) >> 12
}

fn store_error(error: Error) -> String {
    crate::driver::store_error(error)
}

fn turn_error(error: dext_transcript::continuation::Error) -> String {
    format!("turn state rejected the step ({error:?})")
}

/// Settings the embedded package can honor, checked when they are set and before a
/// turn: no sandbox (file tools only, `bash` through a runner), no host paths, and no
/// spend ledger. Every setting it cannot honor is named, with its fix, so one
/// `settings` call can change them all (a session made by a native host usually holds
/// several).
pub fn check_settings(settings: &Settings) -> Result<()> {
    let mut problems = Vec::new();
    if settings.builtin_tools == BuiltinTools::All {
        problems.push(
            "this app has no command sandbox: set `builtin_tools` to files or none (with \
             `runner` remote or app for bash)",
        );
    }
    if settings.toolchain {
        problems.push("`toolchain` needs a native Spindle host: set it false");
    }
    if !settings.sources.is_empty() {
        problems.push("`sources` names host directories this app lacks: set it to []");
    }
    if settings.self_edit {
        problems.push("`self_edit` is a native host's own: this session runs only there");
    }
    if settings.max_usd.is_some() {
        problems.push(
            "`max_usd` needs the native host's spend ledger: set it to null and enforce a \
             budget in the app",
        );
    }
    match problems.is_empty() {
        true => Ok(()),
        false => Err(problems.join("; ")),
    }
}

/// What the app does next for a session, then answers with `Host::deliver` (`chunk`
/// takes a response body as it streams).
#[derive(Debug, Clone, PartialEq)]
pub enum Work {
    /// Credentials for a fresh model request, before it is reserved (so a missing
    /// login never leaves a request of unknown outcome). `headers` are the request's
    /// headers that do not depend on the key; `auth` the ones that do, as templates
    /// with `KEY_SLOT` where the key goes (the app fills them in, or adds its own) and
    /// each of `claims`' slots where that claim of the key goes.
    Authorize {
        provider: String,
        model: String,
        endpoint: String,
        contract: String,
        headers: Vec<(String, String)>,
        auth: Vec<(String, String)>,
        claims: Vec<Claim>,
    },
    /// Send the reserved request once: POST `body` to `endpoint` (with the authorized
    /// headers, `content-type: application/json` and `accept: text/event-stream`, and
    /// `content-encoding: gzip` when `gzip`), stream the body into `chunk`, then
    /// answer `Response`; a request that cannot finish is `Dropped`.
    Fetch {
        endpoint: String,
        gzip: bool,
        body: Vec<u8>,
    },
    /// Run `command` (bash) over the call's workspace for the session's runner, within
    /// `timeout` seconds; `network` is the session's grant. Files are read with
    /// `Host::runner_file`. Answer `Ran`.
    Run {
        /// Which of the app's runners the session uses (`app` or `remote`).
        runner: &'static str,
        command: String,
        timeout: u64,
        network: bool,
        files: Vec<(String, u64, bool, String)>,
        links: Vec<(String, String)>,
        directories: Vec<String>,
    },
    /// Wait this long (a stop ends the wait), then answer `Waited`.
    Wait { ms: u64 },
    /// An app tool call is waiting (its `app_tool` event says which); answer with
    /// `App` within `timeout_s`, or `AppTimeout` once it passes.
    App { call: String, timeout_s: u64 },
}

/// How a request failed to finish.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dropped {
    /// The connection failed or closed before the response ended.
    Transport,
    /// A deadline passed.
    Deadline,
    /// The app ended it for a stop.
    Stopped,
}

/// The app's answer to a `Work`.
pub enum WorkAnswer {
    Authorized,
    Unauthorized(String),
    /// The response ended: its status and headers; `body` is the refusal's body for a
    /// non-2xx status (a 2xx body was streamed into `chunk`).
    Response {
        status: u16,
        content_type: Option<String>,
        retry_after: Option<String>,
        body: Vec<u8>,
    },
    Dropped(Dropped),
    Ran(RunnerResult),
    Waited,
    App(AppAnswer),
    AppTimeout,
}

/// Where a session's operation stands after a call.
#[derive(Debug)]
pub enum Progress {
    /// Waiting for the app: the works now outstanding (a session's own, or its
    /// tasks'), each with the session it belongs to and its id.
    Waiting(Vec<(String, u64, Work)>),
    /// The response is still streaming; keep calling `chunk`.
    Streaming,
    /// The operation ended (see `Host::finish`).
    Done,
}

struct Fetch {
    need: Box<ModelNeed>,
    committed: Box<CommittedEffect>,
    attempt: WireAttempt,
    streamed: bool,
    /// The point a "read them now" cut prepares the request again from.
    redo: Option<u64>,
    /// A cut was asked for and could not be made (said once).
    refused: bool,
}

enum Waiting {
    Authorize(Box<ModelNeed>),
    Fetch(Box<Fetch>),
    /// Credentials for a compaction summary's request, before it is reserved.
    SummaryAuthorize(Box<SummaryWait>),
    /// A compaction summary's response, read as it streams; its request is reserved.
    SummaryFetch(Box<SummaryWait>, Box<super::code::Reading>),
    Run(Box<RunWait>),
    Backoff(driver::Backoff),
    App {
        need: Box<driver::AppNeed>,
    },
    Tasks(Box<Batch>),
    /// A task the app started (`task_start`): its child session runs to its end.
    Started(Box<Started>),
}

/// A compaction summary asked of the session's model (`compaction: model`), as the
/// native host asks it: the request, journaled as one nested call.
struct SummaryWait {
    need: Box<SummaryNeed>,
    /// The messages a queued prompt starting with this compaction joined, steered once
    /// its start commits.
    joined: Vec<String>,
    journal: SummaryJournal,
    /// The request (`code_model_request`'s form).
    prepared: Value,
}

/// What a compaction's summary comes to before anything is sent.
enum SummaryPlan {
    /// The summary the compaction uses now: a reply recorded for the same history, or
    /// None (the digest stands in).
    Ready(Option<String>),
    /// A request to send, with the credentials it needs first.
    Ask(SummaryJournal, Value, Box<Work>),
}

/// A nested need answered here, or waiting on the app.
enum Nested {
    /// The need the loop goes on with.
    Next(Need),
    /// A compaction summary's request waits on the app.
    Parked(Progress),
}

struct RunWait {
    need: Box<RunCall>,
    request: DispatchRequest,
    helper: [u8; 32],
    /// A script request of the running command whose children the app is running.
    code: Option<CodeWait>,
}

/// One script request's children, journaled and handed to the app (`code_call`), and
/// the results it brought back so far (`code_answer`).
struct CodeWait {
    call: u64,
    plan: crate::code_mode::CodeChildren,
    results: Vec<Option<Value>>,
}

/// Where a script request stands for the app.
pub enum CodeStep {
    /// The script's reply.
    Reply(Value),
    /// Children for the app to run, at most `parallel` at once, each answered by
    /// `code_answer` with `call`.
    Children {
        call: u64,
        children: Vec<Value>,
        parallel: usize,
    },
    /// Children still running.
    Pending(usize),
}

struct Started {
    child: Box<Host>,
    input: Value,
    base: Workspace,
}

/// A batch of task calls (one model step's), running as child sessions.
struct Batch {
    base: Workspace,
    calls: Vec<TaskCall>,
}

struct TaskCall {
    need: driver::AppNeed,
    reservation: Option<AppReservation>,
    child: Option<Box<Host>>,
    /// The child's answer, or why it cannot go on now.
    result: Option<std::result::Result<AppAnswer, String>>,
}

/// One embedded session.
pub struct Host {
    label: String,
    store: Store,
    driver: Driver,
    facts: Arc<Facts>,
    queue: Arc<Queue>,
    events: Arc<Events>,
    storage: Box<dyn Storage>,
    /// Files the first prompt starts from (`create`).
    initial_files: Option<Vec<File>>,
    start_state: Option<StartState>,
    /// The running operation's name, if any.
    running: Option<&'static str>,
    waiting: Option<(u64, Waiting)>,
    next_work: u64,
    told_unisolated: bool,
    /// The last script call handed to the app (`code_call`); numbering starts at random,
    /// so an answer meant for another host of this session (before a reopen) never matches.
    next_code: u64,
    /// The identity the session's runner gives (`set_runner`), if it has one.
    runner: Option<[u8; 32]>,
    /// Works this session stopped waiting for before the app answered them.
    cancelled: Vec<(String, u64)>,
    /// What an app-started task answered (`task_start`), once its turn ended.
    started: Option<Value>,
    /// The store was moved aside and could not be opened again: this host no longer
    /// holds the session (the app opens it again).
    detached: bool,
}

impl Host {
    fn assemble(
        label: String,
        store: Store,
        storage: Box<dyn Storage>,
        catalog: Arc<Catalog>,
        depth: usize,
        parent: Option<(Arc<Events>, String)>,
    ) -> Self {
        let facts = Arc::new(Facts { catalog, depth });
        let queue = Arc::new(Queue::default());
        let events = Arc::new(Events {
            relay: parent,
            ..Events::default()
        });
        let driver = Driver::new(facts.clone(), queue.clone(), events.clone());
        Self {
            label,
            store,
            driver,
            facts,
            queue,
            events,
            storage,
            initial_files: None,
            start_state: None,
            running: None,
            waiting: None,
            next_work: 0,
            told_unisolated: false,
            next_code: first_call(),
            runner: None,
            cancelled: vec![],
            started: None,
            detached: false,
        }
    }

    /// Creates the session `label` in `storage` with `settings` (checked for this
    /// host); `files` are the workspace its first prompt starts from. Its work and
    /// events name it `label`.
    pub fn create(
        label: &str,
        storage: Box<dyn Storage>,
        settings: Settings,
        files: Vec<File>,
        catalog: Arc<Catalog>,
    ) -> Result<Self> {
        let mut host = Self::create_at(storage, settings, files, catalog, 0, None)?;
        host.label = label.into();
        Ok(host)
    }

    fn create_at(
        storage: Box<dyn Storage>,
        settings: Settings,
        files: Vec<File>,
        catalog: Arc<Catalog>,
        depth: usize,
        parent: Option<(Arc<Events>, String)>,
    ) -> Result<Self> {
        check_settings(&settings)?;
        catalog.route(&settings.provider, Some(&settings.model))?;
        let bytes = serde_json::to_vec_pretty(&settings.to_json()?).map_err(|e| e.to_string())?;
        let mut store = storage.open(true).map_err(store_error)?;
        store
            .set_metadata("settings", &bytes)
            .map_err(store_error)?;
        storage
            .keep_sidecar(SETTINGS_SIDECAR, &bytes)
            .map_err(store_error)?;
        let mut host = Self::assemble(storage.label(), store, storage, catalog, depth, parent);
        host.initial_files = Some(files);
        Ok(host)
    }

    /// Opens the session `label` in `storage`, finishing a rewind its last owner began.
    pub fn open(label: &str, storage: Box<dyn Storage>, catalog: Arc<Catalog>) -> Result<Self> {
        let mut host = Self::open_at(storage, catalog, 0, None)?;
        host.label = label.into();
        Ok(host)
    }

    fn open_at(
        storage: Box<dyn Storage>,
        catalog: Arc<Catalog>,
        depth: usize,
        parent: Option<(Arc<Events>, String)>,
    ) -> Result<Self> {
        let store = storage
            .open(false)
            .map_err(|error| open_error(storage.as_ref(), error))?;
        let mut host = Self::assemble(storage.label(), store, storage, catalog, depth, parent);
        host.settings()?;
        host.mirror_settings()?;
        host.finish_interrupted_rewind()?;
        Ok(host)
    }

    /// Keeps the settings' native mirror (`session.json`) where the place has a native
    /// layout, written only when it is missing or differs (a session this package made
    /// before it kept one, or settings a native host changed without it).
    fn mirror_settings(&mut self) -> Result<()> {
        let bytes = self
            .store
            .metadata("settings")
            .map_err(store_error)?
            .ok_or("session settings are missing")?;
        if self.storage.sidecar(SETTINGS_SIDECAR).as_deref() != Some(bytes.as_slice()) {
            self.storage
                .keep_sidecar(SETTINGS_SIDECAR, &bytes)
                .map_err(store_error)?;
        }
        Ok(())
    }

    /// The session's name in work, events and errors.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether this host lost its store (a retry's move aside failed half way): the
    /// app drops it and opens the session again from its storage.
    pub fn detached(&self) -> bool {
        self.detached
    }

    fn finish_interrupted_rewind(&mut self) -> Result<()> {
        let Some(rewind) = self.store.unfinished_rewind(SCOPE).map_err(store_error)? else {
            return Ok(());
        };
        if rewind.phase == crate::rewind::RewindPhase::Pending {
            self.store
                .finish_rewind(SCOPE, &rewind.command)
                .map_err(store_error)?;
        }
        self.store
            .resume_rewind(SCOPE, &rewind.command)
            .map_err(store_error)
            .map(|_| ())
    }

    /// This session's files for a bundle: its store's image and every task
    /// store's kept here (each opened, so nothing else runs it meanwhile), with their
    /// sidecars. The task stores come back open, with their places, to be sealed once
    /// the bundle is safe (or dropped). `settings` are the copy's instead of the
    /// session's (where it goes runs it differently); this session keeps its own.
    /// Refused while an operation runs.
    #[allow(clippy::type_complexity)]
    pub fn bundle_files(
        &mut self,
        settings: Option<&Settings>,
    ) -> Result<(Vec<BundleFile>, Vec<(Box<dyn Storage>, Store)>)> {
        if let Some(running) = self.running {
            return Err(format!(
                "a {running} is running in this session; wait for it or stop it"
            ));
        }
        let copied = settings
            .map(|settings| {
                serde_json::to_vec_pretty(&settings.to_json()?).map_err(|e| e.to_string())
            })
            .transpose()?;
        let image = match &copied {
            Some(bytes) => self.store.image_with(&[("settings", bytes)]),
            None => self.store.image(),
        };
        let mut files = vec![BundleFile {
            path: bundle::STORE.into(),
            bytes: image.map_err(store_error)?,
        }];
        if let Some(bytes) = copied.or_else(|| self.storage.sidecar(SETTINGS_SIDECAR)) {
            files.push(BundleFile {
                path: SETTINGS_SIDECAR.into(),
                bytes,
            });
        }
        let mut opened = vec![];
        bundle_tasks(self.storage.as_ref(), "", &mut files, &mut opened)?;
        Ok((files, opened))
    }

    /// Seals this session's place as moved elsewhere (`Storage::seal`), in effect once
    /// its store closes.
    pub fn seal(&self, note: &str) -> Result<()> {
        self.storage.seal(note).map_err(store_error)
    }

    /// The identity of the session's command runner, which its attempts are labelled
    /// with: a remote Spindle host's sandbox digest, or `host_helper` for an app's.
    pub fn set_runner(&mut self, helper: Option<[u8; 32]>) {
        self.runner = helper;
    }

    pub fn settings(&self) -> Result<Settings> {
        let bytes = self
            .store
            .metadata("settings")
            .map_err(store_error)?
            .ok_or("session settings are missing")?;
        let value = serde_json::from_slice(&bytes).map_err(|_| "stored settings are not JSON")?;
        Settings::from_json(&value)
    }

    /// Saves new settings between turns.
    pub fn set_settings(&mut self, settings: Settings) -> Result<()> {
        if self.running.is_some() {
            return Err("a turn is running; settings change between turns".into());
        }
        self.ensure_idle()?;
        check_settings(&settings)?;
        self.facts
            .catalog
            .route(&settings.provider, Some(&settings.model))?;
        let bytes = serde_json::to_vec_pretty(&settings.to_json()?).map_err(|e| e.to_string())?;
        self.store
            .set_metadata("settings", &bytes)
            .map_err(store_error)?;
        self.storage
            .keep_sidecar(SETTINGS_SIDECAR, &bytes)
            .map_err(store_error)
    }

    pub fn running(&self) -> Option<&'static str> {
        self.running
    }

    /// A request line from a script of the command running now (the code-mode bridge):
    /// answered at once, or its children journaled and handed to the app.
    pub fn code_call(&mut self, request: &Value, http: &Value) -> Result<CodeStep> {
        if !matches!(&self.waiting, Some((_, Waiting::Run(run))) if run.code.is_none()) {
            return Err(match &self.waiting {
                Some((_, Waiting::Run(_))) => "this command's last script call is still running",
                _ => "no command of this session is running",
            }
            .into());
        }
        let settings = self.settings()?;
        let (_, state) = self.head()?;
        let execution = state.execution().to_string();
        let catalog = self.facts.catalog.clone();
        let Some((_, Waiting::Run(run))) = &mut self.waiting else {
            unreachable!("checked above");
        };
        let mut bridge = super::code::Bridge {
            tools: &settings.tools,
            catalog: &catalog,
            provider: &settings.provider,
            model: &settings.model,
            session: &execution,
            http,
        };
        let plan = match self.store.code_plan(
            &run.request,
            request,
            &mut bridge,
            &self.queue.interrupt,
            // The app bounds the command's time (and wasm has no clock to read).
            &|| false,
        ) {
            Ok(crate::code_mode::CodePlan::Reply(reply)) => return Ok(CodeStep::Reply(reply)),
            Ok(crate::code_mode::CodePlan::Children(plan)) => plan,
            Err(error) => return Ok(CodeStep::Reply(self.store.code_failure(&error, false))),
        };
        // What the app needs for each, before any is reserved (a provider whose sign-in
        // the app cannot fill is refused with nothing journaled).
        let mut children = Vec::with_capacity(plan.prepared.len());
        for (index, prepared) in plan.prepared.iter().enumerate() {
            children.push(if prepared["kind"] == "model" {
                let provider = prepared["provider"].as_str().unwrap_or_default();
                let model = prepared["model"].as_str().unwrap_or_default();
                let facts = match Self::facts_of(&catalog, provider, model, &execution) {
                    Ok(facts) => facts,
                    Err(error) => return Ok(CodeStep::Reply(json!({"ok": false, "error": error}))),
                };
                let object = |pairs: Vec<(String, String)>| {
                    Value::Object(pairs.into_iter().map(|(name, value)| (name, Value::String(value))).collect())
                };
                let (headers, auth) = (object(facts.headers), object(facts.auth));
                json!({"index": index, "kind": "model", "provider": provider, "model": model,
                    "endpoint": prepared["endpoint"], "contract": prepared["contract"],
                    "body": prepared["body"], "headers": headers, "auth": auth, "key_slot": KEY_SLOT,
                    "claims": claims_json(&facts.claims)})
            } else if prepared["kind"] == "http" {
                json!({"index": index, "kind": "http", "method": prepared["method"],
                    "url": prepared["url"], "headers": prepared["headers"], "body": prepared["body"],
                    "timeout_seconds": prepared["timeout_seconds"],
                    "max_response_bytes": prepared["max_response_bytes"]})
            } else {
                json!({"index": index, "kind": "app", "name": prepared["name"],
                    "input": prepared["input"], "call": prepared["call"]})
            });
        }
        // All are reserved before the app may run any (it runs them together); the app
        // has none yet, so a failure here ran nothing.
        for index in 0..plan.prepared.len() {
            if let Err(error) = self.store.code_reserve(&run.request, &plan, index) {
                return Ok(CodeStep::Reply(self.store.code_failure(&error, false)));
            }
        }
        self.next_code += 1;
        let call = self.next_code;
        let results = vec![None; plan.prepared.len()];
        // App tools one at a time, as native code mode runs them (the app answers one
        // call at a time); models and requests together.
        let parallel = match plan
            .prepared
            .iter()
            .any(|prepared| prepared["kind"] == "app")
        {
            true => 1,
            false => super::code::LIMITS.parallel,
        };
        run.code = Some(CodeWait {
            call,
            plan,
            results,
        });
        Ok(CodeStep::Children {
            call,
            children,
            parallel,
        })
    }

    /// `code_call` for session `label`: this one, or a running task's.
    pub fn code_call_of(&mut self, label: &str, request: &Value, http: &Value) -> Result<CodeStep> {
        if label == self.label {
            return self.code_call(request, http);
        }
        match self.child_toward(label) {
            Some(child) => child.code_call_of(label, request, http),
            None => Err(format!("no task `{label}` is running")),
        }
    }

    /// `code_answer` for session `label`: this one, or a running task's.
    pub fn code_answer_of(
        &mut self,
        label: &str,
        call: u64,
        index: usize,
        answer: &Value,
    ) -> Result<CodeStep> {
        if label == self.label {
            return self.code_answer(call, index, answer);
        }
        match self.child_toward(label) {
            Some(child) => child.code_answer_of(label, call, index, answer),
            None => Err(format!("no task `{label}` is running")),
        }
    }

    /// What the app brought back for child `index` of script call `call`: journaled,
    /// and once every child is in, the script's reply. An answer that is not one (an app
    /// tool's that `app_tool_result` would refuse) is refused, nothing recorded: the app
    /// answers again.
    pub fn code_answer(&mut self, call: u64, index: usize, answer: &Value) -> Result<CodeStep> {
        let Some((_, Waiting::Run(run))) = &mut self.waiting else {
            return Err("no command of this session is running".into());
        };
        let Some(code) = run.code.as_mut().filter(|code| code.call == call) else {
            return Err("that script call is not waiting".into());
        };
        let prepared = code
            .plan
            .prepared
            .get(index)
            .ok_or("there is no such child")?
            .clone();
        if code.results[index].is_some() {
            return Err("that child is answered already".into());
        }
        let result = match prepared["kind"].as_str() {
            Some("model") => super::code::model_result(&prepared, answer),
            Some("http") => crate::code_http::answered(&prepared, answer),
            _ => {
                let answer = AppAnswer::from_json(answer)
                    .map_err(|error| format!("the app's answer is not one: {error}"))?;
                if let AppAnswer::Result {
                    details: Some(details),
                    ..
                } = &answer
                    && details.to_string().len() > crate::transition::MAX_STATE
                {
                    return Err("the app's answer: `details` are over 1 MiB".into());
                }
                super::code::app_result(&prepared, &answer)
            }
        };
        let recorded = match self
            .store
            .code_record(&run.request, &code.plan, index, result)
        {
            Ok(recorded) => recorded,
            Err(error) => {
                let failed = self.store.code_failure(&error, true);
                run.code = None;
                return Ok(CodeStep::Reply(failed));
            }
        };
        code.results[index] = Some(recorded);
        let left = code
            .results
            .iter()
            .filter(|result| result.is_none())
            .count();
        if left > 0 {
            return Ok(CodeStep::Pending(left));
        }
        let code = run.code.take().expect("answered above");
        let results = code
            .results
            .into_iter()
            .map(Option::unwrap_or_default)
            .collect();
        Ok(CodeStep::Reply(crate::code_mode::reply(
            &code.plan.method,
            results,
        )))
    }

    /// Events since the last call, in their stable JSON form.
    pub fn take_events(&self) -> Vec<Value> {
        self.events.take()
    }

    /// Works this session (or its tasks) stopped waiting for: the app ends them.
    pub fn take_cancelled(&mut self) -> Vec<(String, u64)> {
        let mut cancelled = std::mem::take(&mut self.cancelled);
        match &mut self.waiting {
            Some((_, Waiting::Tasks(batch))) => {
                for call in &mut batch.calls {
                    if let Some(child) = &mut call.child {
                        cancelled.extend(child.take_cancelled());
                    }
                }
            }
            Some((_, Waiting::Started(started))) => {
                cancelled.extend(started.child.take_cancelled());
            }
            _ => {}
        }
        cancelled
    }

    pub(crate) fn store(&mut self) -> &mut Store {
        &mut self.store
    }

    fn emit(&self, event: Event<'_>) {
        driver::Observer::event(self.events.as_ref(), event);
    }

    fn head(&mut self) -> Result<(u64, TurnContinuation)> {
        let snapshot = self.store.snapshot().map_err(store_error)?;
        if snapshot.revision == 0 {
            return Err("this session has no turns yet".into());
        }
        let state = TurnContinuation::restore(&snapshot.continuation).map_err(turn_error)?;
        Ok((snapshot.revision, state))
    }

    fn ensure_idle(&mut self) -> Result<()> {
        let snapshot = self.store.snapshot().map_err(store_error)?;
        if snapshot.revision == 0 {
            return Ok(());
        }
        let prior = TurnContinuation::restore(&snapshot.continuation).map_err(turn_error)?;
        match prior.control().phase() {
            Phase::Stopped(Stop::Complete) => Ok(()),
            Phase::Stopped(stop) => Err(format!(
                "the last turn stopped ({}); rewind to a target first (`targets`)",
                driver::stop_text(stop)
            )),
            _ => Err("a turn is still in progress; continue it first".into()),
        }
    }

    /// Starts a turn-like operation: refused while one runs; the owner moves to a new
    /// epoch, as a native host's fresh open does, so work an earlier operation left
    /// unrecorded is settled from the store, never found in flight.
    fn begin(&mut self, name: &'static str) -> Result<()> {
        if self.detached {
            return Err("this session's store was moved aside; open the session again".into());
        }
        if let Some(running) = self.running {
            return Err(format!(
                "a {running} is running in this session; wait for it or stop it"
            ));
        }
        // A session made by a native host may hold settings this package cannot honor:
        // its turns wait for a change (`settings`); a rewind needs none.
        if name != "rewind" {
            check_settings(&self.settings()?)
                .map_err(|why| format!("this session's settings cannot run here: {why}"))?;
        }
        self.store.renew_epoch().map_err(store_error)?;
        self.running = Some(name);
        *lock(&self.events.stopped) = None;
        *lock(&self.events.rewound) = None;
        Ok(())
    }

    /// Ends the running operation (after `Progress::Done` or an error): steering it
    /// never reached, whether a stop ended it, and its stop and rewind events.
    pub fn finish(&mut self) -> Finished {
        self.running = None;
        if let Some((id, _)) = self.waiting.take() {
            self.cancelled.push((self.label.clone(), id));
        }
        let stopped = self.queue.interrupt.swap(false, Ordering::SeqCst);
        self.queue.hurry.store(false, Ordering::SeqCst);
        let unsent = std::mem::take(&mut *lock(&self.queue.steering));
        // State changes sent too late for the turn's last step are kept now, each a
        // step of its own (or refused with `state_refused`).
        let _ = self.driver.step(&mut self.store).commit_queued_state();
        let head = self.store.snapshot().map(|s| s.revision).unwrap_or(0);
        Finished {
            head,
            stopped: lock(&self.events.stopped).take(),
            rewound: lock(&self.events.rewound).take(),
            unsent,
            interrupted: stopped,
        }
    }

    fn park(&mut self, waiting: Waiting, work: Work) -> Progress {
        self.next_work += 1;
        let id = self.next_work;
        self.waiting = Some((id, waiting));
        Progress::Waiting(vec![(self.label.clone(), id, work)])
    }

    // ---- operations -----------------------------------------------------------

    /// Starts a turn with `text`, `files` added to the workspace first, and `state`
    /// (a JSON merge patch) applied with it.
    pub fn prompt(
        &mut self,
        text: &str,
        files: Vec<File>,
        state: Option<Value>,
    ) -> Result<Progress> {
        self.begin("prompt")?;
        let outcome = (|| {
            if let Some(patch) = state {
                if !patch.is_object() {
                    return Err("`state` is a JSON merge patch: an object".to_string());
                }
                self.start_state = Some(StartState::Patch(patch));
            }
            let first = self.store.snapshot().map_err(store_error)?.revision == 0;
            let start = match self.initial_files.take() {
                Some(mut initial) if first => {
                    for (path, bytes, executable) in files {
                        initial.retain(|(kept, _, _)| *kept != path);
                        initial.push((path, bytes, executable));
                    }
                    Start::Import(initial, vec![], vec![])
                }
                // A first turn that never started still gets the workspace it would
                // have made, or no tool could run.
                _ if first => Start::Import(files, vec![], vec![]),
                _ if files.is_empty() => Start::Same,
                _ => Start::Add(files),
            };
            let need = self.start_turn(text, start)?;
            self.driver.begin_run();
            self.advance(Some(need))
        })();
        self.start_state = None;
        outcome
    }

    /// Continues an interrupted run; `until_unknown` stops at an unknown outcome
    /// instead of sending anything again.
    pub fn resume(&mut self, until_unknown: bool) -> Result<Progress> {
        self.begin("continue")?;
        let need = if until_unknown {
            self.driver.resume_until_unknown(&mut self.store)?
        } else {
            self.driver.resume(&mut self.store)?
        };
        match need {
            Need::Stopped(_) => {
                self.report_stop()?;
                Ok(Progress::Done)
            }
            Need::PackIdle(_) => {
                self.emit(Event::NothingToContinue);
                self.report_stop()?;
                Ok(Progress::Done)
            }
            need => self.advance(Some(need)),
        }
    }

    /// Sends an unresolved model request again, from the point before it.
    pub fn retry(&mut self) -> Result<Progress> {
        self.begin("retry")?;
        match self.driver.retry(&mut self.store)? {
            Retry::FreshStore { prompt } => self.restart_first(prompt),
            Retry::InPlace(need) => {
                self.driver.begin_run();
                self.advance(Some(need))
            }
        }
    }

    /// The first request has no earlier point to rewind to: the old store is kept
    /// aside (its unknown request stays on record there) and a fresh one restarts the
    /// same prompt over the same files, settings and app state.
    fn restart_first(&mut self, prompt: String) -> Result<Progress> {
        if self.store.snapshot().map_err(store_error)?.revision != 1 {
            return Err("no earlier point can be rewound to (`targets`)".into());
        }
        let mut start = Start::Import(vec![], vec![], vec![]);
        if let Some(root) = self.store.snapshot().map_err(store_error)?.workspace {
            let tree = self.store.tree_map(SCOPE, &root).map_err(store_error)?;
            let mut files = vec![];
            for entry in tree.files {
                let bytes = self
                    .store
                    .read_object(SCOPE, &entry.object)
                    .map_err(store_error)?;
                files.push((entry.path, bytes, entry.executable));
            }
            start = Start::Import(files, tree.links, tree.directories);
        }
        let carried = self.store.app_state(SCOPE, 1).map_err(store_error)?;
        let metadata = ["settings", "pending_settings", "output_limits"]
            .into_iter()
            .map(|key| self.store.metadata(key).map(|bytes| (key, bytes)))
            .collect::<crate::Result<Vec<_>>>()
            .map_err(store_error)?;
        let old = std::mem::replace(&mut self.store, memory_store().map_err(store_error)?);
        let (fresh, aside) = match self.storage.set_aside(old) {
            Ok(kept) => kept,
            Err(error) => {
                // Where the old store still is, the session goes on from it; otherwise
                // this host no longer holds it.
                match self.storage.open(false) {
                    Ok(store) => self.store = store,
                    Err(_) => self.detached = true,
                }
                return Err(store_error(error));
            }
        };
        self.store = fresh;
        for (key, bytes) in metadata {
            if let Some(bytes) = bytes {
                self.store.set_metadata(key, &bytes).map_err(store_error)?;
            }
        }
        self.emit(Event::RetryingFirst {
            path: std::path::Path::new(&aside),
        });
        self.start_state = Some(StartState::Exact(carried));
        let outcome = self.start_turn(&prompt, start).and_then(|need| {
            self.driver.begin_run();
            self.advance(Some(need))
        });
        self.start_state = None;
        outcome
    }

    /// Goes back to a rewind target, restoring its workspace and app state.
    pub fn rewind(&mut self, target: u64) -> Result<Progress> {
        self.begin("rewind")?;
        let report = self.store.reconciliation().map_err(store_error)?;
        for (scope, sequence) in report.queued {
            if scope == SCOPE {
                self.store
                    .drop_admission(sequence, &format!("drop-{sequence}"))
                    .map_err(store_error)?;
            }
        }
        let targets = self.store.rewind_targets(SCOPE).map_err(|error| {
            format!(
                "cannot rewind now: {}",
                match error {
                    Error::Conflict => "a command is still unresolved; continue first".into(),
                    other => store_error(other),
                }
            )
        })?;
        let Some(chosen) = targets.iter().find(|item| item.revision == target) else {
            return Err(format!(
                "revision {target} is not a rewind target; see `targets`"
            ));
        };
        let kind = chosen.kind;
        let head = self.store.snapshot().map_err(store_error)?.revision;
        let lineage = self.store.lineage().map_err(store_error)?;
        let cut = self
            .store
            .answered_app_calls(SCOPE, target)
            .map_err(store_error)?
            .iter()
            .map(|id| self.store.call_id(id).map_err(store_error))
            .collect::<Result<Vec<_>>>()?;
        let state_before = self.store.app_state(SCOPE, head).map_err(store_error)?;
        let command = format!("rw-{head}");
        self.store
            .begin_rewind(SCOPE, &command, target, head, lineage)
            .map_err(store_error)?;
        self.store
            .finish_rewind(SCOPE, &command)
            .map_err(store_error)?;
        self.store
            .resume_rewind(SCOPE, &command)
            .map_err(store_error)?;
        let report = self.store.reconciliation().map_err(store_error)?;
        let revision = self.store.snapshot().map_err(store_error)?.revision;
        self.emit(Event::Rewound {
            target,
            revision,
            kind,
            abandoned: report.abandoned_attempts.len(),
        });
        if !cut.is_empty() {
            self.emit(Event::AppCallsRewound { calls: &cut });
        }
        let state_after = self.store.app_state(SCOPE, revision).map_err(store_error)?;
        self.driver
            .step(&mut self.store)
            .emit_state(&state_before, &state_after, revision);
        Ok(Progress::Done)
    }

    /// Commits `patch` to the app state as a step of its own (between turns).
    pub fn set_state(&mut self, patch: &Value) -> Result<u64> {
        if self.running.is_some() {
            return Err("a turn is running; queue the change instead".into());
        }
        self.driver.set_state(&mut self.store, patch)
    }

    /// Queues a state change for the running turn's next step boundary.
    pub fn queue_state(&self, patch: Value) -> Result<usize> {
        let mut queue = lock(&self.queue.state);
        if queue.len() >= crate::protocol::MAX_STEERS {
            return Err(format!(
                "{} state changes are already waiting for the turn",
                crate::protocol::MAX_STEERS
            ));
        }
        queue.push(patch);
        Ok(queue.len())
    }

    /// Queues a steering message for the running turn (`text` None: only `now`);
    /// `now` asks for the messages to be read now, cutting a reply being written.
    /// Returns how many wait, and where the turn stands (an error there is the
    /// turn's, ending it, not the call's).
    pub fn steer(&mut self, text: Option<String>, now: bool) -> Result<(usize, Result<Progress>)> {
        let queued = {
            let mut queue = lock(&self.queue.steering);
            match text {
                Some(_) if queue.len() >= crate::protocol::MAX_STEERS => {
                    return Err(format!(
                        "{} messages are already waiting for the turn",
                        crate::protocol::MAX_STEERS
                    ));
                }
                Some(text) => queue.push(text),
                None if queue.is_empty() => {
                    return Err("no message is waiting for the turn".into());
                }
                None => {}
            }
            queue.len()
        };
        if now {
            self.queue.hurry.store(true, Ordering::SeqCst);
        }
        let progress = self.poll_cut().unwrap_or(Ok(Progress::Streaming));
        Ok((queued, progress))
    }

    /// Asks the running turn to stop at its next safe point. What it waits on now
    /// ends: a request or command is cut off (recorded as not completed, never run
    /// again), a backoff or an app call ends, and tasks are stopped. Refused when
    /// nothing runs; otherwise returns where the turn stands (its error is the turn's).
    pub fn stop(&mut self) -> Result<Result<Progress>> {
        if self.running.is_none() {
            return Err("nothing is running in this session".into());
        }
        self.queue.interrupt.store(true, Ordering::SeqCst);
        Ok(self.stop_waiting())
    }

    fn stop_waiting(&mut self) -> Result<Progress> {
        let Some((id, waiting)) = self.waiting.take() else {
            return Ok(Progress::Streaming);
        };
        self.cancelled.push((self.label.clone(), id));
        match waiting {
            Waiting::Authorize(need) => self.reserve_model(need),
            Waiting::Fetch(fetch) => self.fetch_failed(*fetch, Dropped::Stopped),
            // A summary is never waited for past a stop: the digest stands in (a request
            // sent is recorded as cut off), and the turn stops at its next request.
            Waiting::SummaryAuthorize(wait) => self.summary_done(*wait, None),
            Waiting::SummaryFetch(wait, _) => self.summary_failed(*wait, AttemptError::Interrupted),
            Waiting::Run(run) => {
                let RunWait {
                    need,
                    request,
                    helper,
                    // A script call still waiting ends with its command (its children
                    // stay uncertain in the journal).
                    code: _,
                } = *run;
                self.ran(need, request, helper, Err(Error::Cancelled))
            }
            Waiting::Backoff(backoff) => Err(backoff.stopped),
            Waiting::App { need } => self.app_answered(need, AppAnswer::Stopped),
            Waiting::Tasks(mut batch) => {
                for call in &mut batch.calls {
                    if let Some(mut child) = call.child.take() {
                        let _ = child.stop();
                        child.finish();
                        self.cancelled.extend(child.take_cancelled());
                        call.result = Some(Ok(AppAnswer::Stopped));
                    }
                }
                self.tasks_done(*batch)
            }
            Waiting::Started(mut started) => {
                let _ = started.child.stop();
                started.child.finish();
                self.cancelled.extend(started.child.take_cancelled());
                Err("stopped while the task ran".into())
            }
        }
    }

    // ---- the loop ---------------------------------------------------------------

    /// Starts a turn: the driver's next need (a compaction summary may come before the
    /// turn's first step commits; `advance` asks for it).
    fn start_turn(&mut self, prompt: &str, start: Start) -> Result<Need> {
        let state = self.start_state.take();
        self.driver
            .start(&mut self.store, prompt.to_string(), start, state)
    }

    /// A queued prompt's start (after a stop, or a retry's restart): the messages it
    /// joined are steered once it commits, which may wait for a compaction summary.
    fn start_queued(&mut self, start: driver::StartCall) -> Result<Nested> {
        let driver::StartCall {
            prompt,
            start,
            state,
            joined,
        } = start;
        self.start_state = state;
        match self.start_turn(&prompt, start) {
            // Until its start commits, a queued prompt (and what it joined) is held only
            // here: the digest stands in rather than a request that would hold it over
            // a network round trip.
            Ok(Need::Summarize(need)) => self.summarized(need, joined, None).map(Nested::Next),
            started => self.started(started, joined).map(Nested::Next),
        }
    }

    /// What a queued prompt's start came to: once it commits the messages it joined
    /// are steered; when it fails they wait for the next turn again.
    fn started(&mut self, next: Result<Need>, joined: Vec<String>) -> Result<Need> {
        match next {
            Ok(need) => {
                for text in &joined {
                    self.emit(Event::Steered { text });
                }
                Ok(need)
            }
            Err(error) => {
                driver::Inbox::return_steering(self.queue.as_ref(), joined);
                Err(error)
            }
        }
    }

    fn report_stop(&mut self) -> Result<()> {
        if !matches!(self.head()?.1.control().phase(), Phase::Stopped(_)) {
            return Ok(());
        }
        self.driver.report_stop(&mut self.store)
    }

    /// Runs the loop until the turn needs the app, or ends.
    fn advance(&mut self, mut next: Option<Need>) -> Result<Progress> {
        loop {
            let need = match next.take() {
                Some(need) => need,
                None => self.driver.next(&mut self.store)?,
            };
            match need {
                Need::Continue => {}
                Need::Summarize(need) => match self.summarize(need, vec![])? {
                    Nested::Next(need) => next = Some(need),
                    Nested::Parked(progress) => return Ok(progress),
                },
                Need::Start(start) => match self.start_queued(*start)? {
                    Nested::Next(need) => next = Some(need),
                    Nested::Parked(progress) => return Ok(progress),
                },
                Need::TaskWait => {
                    return Err(
                        "this session waits for background tasks, which this app cannot run".into(),
                    );
                }
                Need::Send(need) => return self.send(need),
                Need::Wait(backoff) => {
                    if backoff.wait.is_zero() {
                        next = Some(
                            self.driver
                                .deliver(&mut self.store, Answer::Waited(backoff))?,
                        );
                        continue;
                    }
                    let ms = backoff.wait.as_millis().min(u64::MAX as u128) as u64;
                    return Ok(self.park(Waiting::Backoff(backoff), Work::Wait { ms }));
                }
                Need::Tool(ToolNeed::Committed) => {}
                Need::Tool(ToolNeed::Run(need)) => {
                    if let Some(progress) = self.run(need)? {
                        return Ok(progress);
                    }
                }
                Need::Tool(ToolNeed::Ask(need) | ToolNeed::Task(need)) => {
                    let progress = if need.call.name == task_tool::NAME {
                        Some(self.tasks(vec![*need])?)
                    } else {
                        self.app(need)?
                    };
                    if let Some(progress) = progress {
                        return Ok(progress);
                    }
                }
                Need::Tool(ToolNeed::Tasks(needs)) => return self.tasks(needs),
                // Session packs are native; this host never reports one active.
                Need::PackIdle(_) | Need::FollowUp { .. } => return Ok(Progress::Done),
                Need::Stopped(stop) => {
                    self.report_stop()?;
                    match self
                        .driver
                        .deliver(&mut self.store, Answer::Stopped(stop))?
                    {
                        Need::Stopped(_) => return Ok(Progress::Done),
                        need => next = Some(need),
                    }
                }
            }
        }
    }

    /// The progress after a delivery that continues the loop, or an error that ends
    /// the operation.
    fn resume_loop(&mut self, next: Result<Need>) -> Result<Progress> {
        let next = next?;
        self.advance(Some(next))
    }

    // ---- model requests -----------------------------------------------------------

    fn send(&mut self, need: Box<ModelNeed>) -> Result<Progress> {
        let Some(route) = &need.route else {
            return self.reserve_model(need);
        };
        let (_, state) = self.head()?;
        let facts = self.header_facts(&route.provider, &route.model, state.execution())?;
        let work = Work::Authorize {
            provider: route.provider.clone(),
            model: route.model.clone(),
            endpoint: route.endpoint.clone(),
            contract: route.contract.as_str().into(),
            headers: facts.headers,
            auth: facts.auth,
            claims: facts.claims,
        };
        Ok(self.park(Waiting::Authorize(need), work))
    }

    /// The request's headers as Dext's provider layer builds them, split in two: the
    /// ones that do not depend on the key, and templates of the ones that do (the key's
    /// place marked `KEY_SLOT`), which the app fills in. Nothing names an
    /// authentication header here: a header is the key's when its value changes with
    /// the key. A provider whose headers read a claim of its key (a ChatGPT sign-in's
    /// account) gets that claim's place marked too, for the app to fill from the key it
    /// holds; one that otherwise transforms the key (rather than placing it) is refused:
    /// the app could not fill it in. Spindle never sees a key.
    fn header_facts(&self, provider: &str, model: &str, execution: &str) -> Result<HeaderFacts> {
        Self::facts_of(&self.facts.catalog, provider, model, execution)
    }

    /// `header_facts` from a catalog (for a nested request, while a command waits).
    fn facts_of(
        catalog: &Catalog,
        provider: &str,
        model: &str,
        execution: &str,
    ) -> Result<HeaderFacts> {
        let route = catalog.route(provider, Some(model))?;
        use dext_provider_catalog::catalog;
        let base_url = route.profile.base_url.trim_end_matches('/').to_string();
        let session = crate::shape::subscription_session(execution);
        let context = catalog::HeaderContext {
            originator: "spindle",
            user_agent: concat!("spindle-embed/", env!("CARGO_PKG_VERSION")),
            request_id: None,
            kimi: None,
        };
        let refused = |why: String| {
            format!(
                "provider `{provider}` signs in a way this app cannot ({why}); use a provider \
                 that takes an API key"
            )
        };
        let build = |key: &str| {
            catalog::provider_headers_with_context(
                route.contract,
                key,
                catalog::is_official_kimi_profile(&route.profile, &base_url),
                false,
                false,
                Some(&session),
                &context,
            )
            .map_err(|error| refused(format!("{error:#}")))
        };
        // A key whose claims the headers read is a token (a JWT): the probe keys are
        // tokens carrying the claim slots' values, so a header taken from a claim shows
        // as that slot.
        let claims = match route.contract {
            RequestContract::ChatGptResponses => vec![Claim {
                slot: ACCOUNT_SLOT.into(),
                path: catalog::CHATGPT_ACCOUNT_CLAIM.map(String::from).to_vec(),
            }],
            _ => vec![],
        };
        let other_account = ACCOUNT_SLOT.replace("account", "other-account");
        let (key, other) = match claims.is_empty() {
            true => (KEY_SLOT.to_string(), KEY_SLOT.replace("key", "other-key")),
            false => (
                claim_token(&claims, ACCOUNT_SLOT, "key"),
                claim_token(&claims, &other_account, "other-key"),
            ),
        };
        // Another key in the same place: what differs between the two depends on it.
        let (with_key, with_other) = (build(&key)?, build(&other)?);
        let mut facts = Vec::new();
        let mut auth = Vec::new();
        for (name, value) in &with_key {
            let text = value
                .to_str()
                .map_err(|_| refused(format!("`{name}` is not text")))?;
            let alternative = with_other.get(name).and_then(|value| value.to_str().ok());
            if alternative == Some(text) {
                facts.push((name.as_str().to_string(), text.to_string()));
            } else if text.contains(&key)
                && alternative == Some(text.replace(&key, &other).as_str())
            {
                auth.push((name.as_str().to_string(), text.replace(&key, KEY_SLOT)));
            } else if !claims.is_empty()
                && text.contains(ACCOUNT_SLOT)
                && alternative == Some(text.replace(ACCOUNT_SLOT, &other_account).as_str())
            {
                auth.push((name.as_str().to_string(), text.to_string()));
            } else {
                return Err(refused(format!("`{name}` is derived from the key")));
            }
        }
        if crate::shape::binds_thinking(&route) {
            match facts.iter_mut().find(|(name, _)| name == "anthropic-beta") {
                Some((_, betas)) => {
                    betas.push(',');
                    betas.push_str(crate::shape::THINKING_BINDING_BETA);
                }
                None => facts.push((
                    "anthropic-beta".into(),
                    crate::shape::THINKING_BINDING_BETA.into(),
                )),
            }
        }
        Ok(HeaderFacts {
            headers: facts,
            auth,
            claims,
        })
    }

    /// Whether a fresh send is cut now: a stop, or messages the user asked to be read
    /// now when there is an earlier point to prepare the request from.
    fn cut_now(
        &mut self,
        fresh: bool,
        redo: Option<u64>,
        refused: &mut bool,
    ) -> Option<Option<u64>> {
        if self.queue.interrupt.load(Ordering::SeqCst) {
            return Some(None);
        }
        if fresh
            && !*refused
            && self.queue.hurry.load(Ordering::SeqCst)
            && !lock(&self.queue.steering).is_empty()
        {
            if let Some(point) = redo {
                return Some(Some(point));
            }
            *refused = true;
            self.emit(Event::ReplyNotCut);
        }
        None
    }

    fn reserve_model(&mut self, need: Box<ModelNeed>) -> Result<Progress> {
        let fresh = need.route.is_some();
        let redo = if fresh { redo_point(&self.store) } else { None };
        let mut refused = false;
        let cut = self.cut_now(fresh, redo, &mut refused);
        let flag = AtomicBool::new(cut.is_some());
        let reserved = self
            .store
            .reserve_turn_model(&need.request, &flag, |_| Ok(()));
        let point = cut.flatten();
        match reserved {
            Ok(ModelReservation::Settled(outcome)) => {
                self.model_sent(need, Ok(outcome), false, None, None)
            }
            Ok(ModelReservation::Reserved(committed)) => {
                let Effect::Model { waiting, .. } = committed.effect() else {
                    return self.model_sent(need, Err(Error::Invalid), false, None, None);
                };
                // An attempt reserved earlier is never sent from here (no route was
                // resolved for it): its outcome is settled from the store.
                if !fresh {
                    return self.model_sent(need, Err(Error::Conflict), false, None, None);
                }
                let request = waiting.request();
                let endpoint = request.identity().endpoint.clone();
                if !attempt::endpoint_allowed(&endpoint) {
                    return self.model_sent(
                        need,
                        Err(Error::Model(ModelError::Invalid)),
                        false,
                        None,
                        None,
                    );
                }
                let wire = match WireAttempt::new(
                    waiting,
                    crate::shape::captures_reasoning(request.body()),
                    false,
                ) {
                    Ok(wire) => wire,
                    Err(error) => {
                        return self.model_sent(need, Err(attempt_error(error)), false, None, None);
                    }
                };
                let work = Work::Fetch {
                    endpoint,
                    gzip: request.encoding() == dext_core::model::Encoding::Gzip,
                    body: request.body().to_vec(),
                };
                let fetch = Fetch {
                    need,
                    committed,
                    attempt: wire,
                    streamed: false,
                    redo,
                    refused,
                };
                Ok(self.park(Waiting::Fetch(Box::new(fetch)), work))
            }
            Err(error) => {
                let cut = point.filter(|_| !self.queue.interrupt.load(Ordering::SeqCst));
                self.model_sent(need, Err(error), false, cut, None)
            }
        }
    }

    /// Hands what the send came to to the driver and continues.
    fn model_sent(
        &mut self,
        need: Box<ModelNeed>,
        outcome: crate::Result<DispatchOutcome>,
        streamed: bool,
        cut: Option<u64>,
        rejection: Option<attempt::Rejection>,
    ) -> Result<Progress> {
        let sent = ModelSent {
            outcome: outcome.map_err(|error| match error {
                Error::Model(ModelError::Http(status)) => ModelFailure::Http(status),
                Error::Model(error @ (ModelError::Transport | ModelError::Deadline)) => {
                    ModelFailure::Dropped(store_error(Error::Model(error)))
                }
                other => ModelFailure::Failed(store_error(other)),
            }),
            streamed,
            cut,
            rejection: rejection.map(|r| driver::Rejection {
                status: r.status,
                retry_after: r.retry_after,
                body: r.body,
            }),
        };
        let next = self
            .driver
            .deliver(&mut self.store, Answer::Model { need, sent });
        self.resume_loop(next)
    }

    /// A "read them now" cut of the reply streaming, when one is asked for and can
    /// be made (or a stop): the send ends and the loop goes on.
    fn poll_cut(&mut self) -> Option<Result<Progress>> {
        let Some((id, Waiting::Fetch(fetch))) = &mut self.waiting else {
            return None;
        };
        let (id, fresh, redo, mut refused) =
            (*id, fetch.need.route.is_some(), fetch.redo, fetch.refused);
        let cut = self.cut_now(fresh, redo, &mut refused);
        if let Some((_, Waiting::Fetch(fetch))) = &mut self.waiting {
            fetch.refused = refused;
        }
        let point = cut?;
        let Some((_, Waiting::Fetch(fetch))) = self.waiting.take() else {
            unreachable!("matched above");
        };
        self.cancelled.push((self.label.clone(), id));
        Some(match point {
            Some(point) if !self.queue.interrupt.load(Ordering::SeqCst) => {
                let Fetch { need, streamed, .. } = *fetch;
                self.model_sent(
                    need,
                    Err(Error::Model(ModelError::Interrupted)),
                    streamed,
                    Some(point),
                    None,
                )
            }
            _ => self.fetch_failed(*fetch, Dropped::Stopped),
        })
    }

    /// The next part of a streaming response body.
    pub fn chunk(&mut self, work: u64, bytes: &[u8]) -> Result<Progress> {
        if let Some(cut) = self.poll_cut() {
            return cut;
        }
        if let Some((id, Waiting::SummaryFetch(_, reading))) = &mut self.waiting {
            if *id != work {
                return Err("that request is no longer waited for".into());
            }
            let Err(error) = reading.push(bytes) else {
                return Ok(Progress::Streaming);
            };
            let Some((id, Waiting::SummaryFetch(wait, _))) = self.waiting.take() else {
                unreachable!("matched above");
            };
            self.cancelled.push((self.label.clone(), id));
            return self.summary_failed(*wait, error);
        }
        let Some((id, Waiting::Fetch(fetch))) = &mut self.waiting else {
            return Err("no response is streaming for this session".into());
        };
        if *id != work {
            return Err("that request is no longer waited for".into());
        }
        match fetch.attempt.push(bytes) {
            Ok(updates) => {
                let mut events = Vec::new();
                for update in &updates {
                    match update {
                        StreamUpdate::TextDelta(text) if !text.is_empty() => {
                            fetch.streamed = true;
                            events.push(json!({"type": "model_delta", "text": text}));
                        }
                        StreamUpdate::TextPreviewDiscarded if fetch.streamed => {
                            events.push(json!({"type": "model_text_withdrawn"}));
                        }
                        _ => {}
                    }
                }
                for event in events {
                    self.events.push(event);
                }
                Ok(Progress::Streaming)
            }
            Err(error) => {
                let Some((id, Waiting::Fetch(mut fetch))) = self.waiting.take() else {
                    unreachable!("matched above");
                };
                self.cancelled.push((self.label.clone(), id));
                let rejection = fetch.attempt.take_rejection();
                let Fetch { need, streamed, .. } = *fetch;
                self.model_sent(need, Err(attempt_error(error)), streamed, None, rejection)
            }
        }
    }

    fn fetched(
        &mut self,
        fetch: Fetch,
        status: u16,
        content_type: Option<String>,
        retry_after: Option<String>,
        body: Vec<u8>,
    ) -> Result<Progress> {
        let Fetch {
            need,
            committed,
            attempt: wire,
            streamed,
            ..
        } = fetch;
        if !(200..300).contains(&status) {
            let rejection = attempt::rejection(status, retry_after.as_deref(), &body);
            return self.model_sent(
                need,
                Err(Error::Model(ModelError::Http(status))),
                streamed,
                None,
                Some(rejection),
            );
        }
        if !attempt::event_stream(content_type.as_deref()) {
            return self.model_sent(
                need,
                Err(Error::Model(ModelError::Protocol)),
                streamed,
                None,
                None,
            );
        }
        let Effect::Model { waiting, .. } = committed.effect() else {
            return self.model_sent(need, Err(Error::Invalid), streamed, None, None);
        };
        let streamed_now = AtomicBool::new(streamed);
        let events = self.events.clone();
        let observe = |update: &StreamUpdate| match update {
            StreamUpdate::TextDelta(text) if !text.is_empty() => {
                streamed_now.store(true, Ordering::SeqCst);
                events.push(json!({"type": "model_delta", "text": text}));
            }
            StreamUpdate::TextPreviewDiscarded if streamed_now.load(Ordering::SeqCst) => {
                events.push(json!({"type": "model_text_withdrawn"}));
            }
            _ => {}
        };
        let outcome = wire
            .finish(waiting, &observe)
            .map_err(attempt_error)
            .and_then(|recorded| {
                self.store
                    .ingest_turn_model(&need.request, &committed, recorded.checkpoint())
            });
        let streamed = streamed_now.load(Ordering::SeqCst);
        self.model_sent(need, outcome, streamed, None, None)
    }

    fn fetch_failed(&mut self, fetch: Fetch, dropped: Dropped) -> Result<Progress> {
        let error = wire_error(dropped_error(dropped));
        let Fetch { need, streamed, .. } = fetch;
        self.model_sent(need, Err(Error::Model(error)), streamed, None, None)
    }

    // ---- compaction summaries ---------------------------------------------------------

    /// A compaction's summary, as the native host makes it: asked of the session's model
    /// when it compacts by model (credentials first, then the request, reserved and sent
    /// once), the reply recorded for the same history, or the digest. `joined`: the
    /// messages a queued prompt starting with this compaction joined.
    fn summarize(&mut self, need: Box<SummaryNeed>, joined: Vec<String>) -> Result<Nested> {
        match self
            .summary_plan(&need.compact)
            .unwrap_or(SummaryPlan::Ready(None))
        {
            SummaryPlan::Ready(summary) => self.summarized(need, joined, summary).map(Nested::Next),
            SummaryPlan::Ask(journal, prepared, work) => {
                let wait = SummaryWait {
                    need,
                    joined,
                    journal,
                    prepared,
                };
                let parked = self.park(Waiting::SummaryAuthorize(Box::new(wait)), *work);
                Ok(Nested::Parked(parked))
            }
        }
    }

    /// What a compaction's summary comes to before anything is sent. None (the digest
    /// stands in): the session compacts by digest, the summary was asked for once with
    /// no reply to reuse, a stop was asked for, or the request cannot be made here.
    fn summary_plan(&mut self, compact: &CompactCall) -> Option<SummaryPlan> {
        let CompactReplacement::Summary {
            dropped, prompt, ..
        } = &compact.replacement
        else {
            return None;
        };
        let settings = self.settings().ok()?;
        if settings.compaction != Compaction::Model {
            return None;
        }
        let (_, state) = self.head().ok()?;
        let catalog = self.facts.catalog.clone();
        let route = catalog
            .route(&settings.provider, Some(&settings.model))
            .ok()?;
        let (source, input) = driver::summary_input(dropped, *prompt);
        let prepared = crate::shape::code_model_request(&input, &route, state.execution(), &|| {
            catalog.image_models()
        })
        .ok()?;
        let journal = match self.store.summary_journal(&compact.command, &source)? {
            Journaled::Fresh(journal) => journal,
            // Its compaction did not commit: the reply, counted when it came, serves.
            Journaled::Reused(reply) => {
                return Some(SummaryPlan::Ready(driver::summary_text(&reply)));
            }
            Journaled::Asked => return None,
        };
        if self.queue.interrupt.load(Ordering::SeqCst)
            || !attempt::endpoint_allowed(&route.endpoint)
        {
            return None;
        }
        let facts = self
            .header_facts(&route.profile.id, &route.model, state.execution())
            .ok()?;
        let work = Work::Authorize {
            provider: route.profile.id.clone(),
            model: route.model.clone(),
            endpoint: route.endpoint.clone(),
            contract: route.contract.as_str().into(),
            headers: facts.headers,
            auth: facts.auth,
            claims: facts.claims,
        };
        Some(SummaryPlan::Ask(journal, prepared, Box::new(work)))
    }

    /// The summary request's credentials are in: it is reserved, then sent once. A stop
    /// asked for meanwhile, or a request that cannot be reserved, sends nothing.
    fn summary_authorized(&mut self, wait: Box<SummaryWait>) -> Result<Progress> {
        if self.queue.interrupt.load(Ordering::SeqCst) {
            return self.summary_done(*wait, None);
        }
        let (Ok(reading), Ok(body)) = (
            super::code::Reading::new(&wait.prepared),
            serde_json::to_vec(&wait.prepared["body"]),
        ) else {
            return self.summary_done(*wait, None);
        };
        let reserved = self.head().is_ok_and(|(_, state)| {
            self.store
                .summary_reserve(&wait.journal, &state, &wait.prepared, None)
                .is_ok()
        });
        if !reserved {
            return self.summary_done(*wait, None);
        }
        let work = Work::Fetch {
            endpoint: wait.prepared["endpoint"]
                .as_str()
                .unwrap_or_default()
                .into(),
            gzip: false,
            body,
        };
        Ok(self.park(Waiting::SummaryFetch(wait, Box::new(reading)), work))
    }

    /// The summary's response ended: read, or refused by its status or type.
    fn summary_fetched(
        &mut self,
        wait: SummaryWait,
        reading: super::code::Reading,
        status: u16,
        content_type: Option<String>,
    ) -> Result<Progress> {
        let read = if !(200..300).contains(&status) {
            Err(AttemptError::Http(status))
        } else if !attempt::event_stream(content_type.as_deref()) {
            Err(AttemptError::Protocol)
        } else {
            reading.finish()
        };
        let reply = summary_reply(&wait.prepared, read);
        self.summary_done(wait, Some(reply))
    }

    /// The summary request that was sent did not finish (it may have run).
    fn summary_failed(&mut self, wait: SummaryWait, error: AttemptError) -> Result<Progress> {
        let reply = summary_reply(&wait.prepared, Err(error));
        self.summary_done(wait, Some(reply))
    }

    /// Where a summary request ended: the reply of one that was sent (None: nothing was)
    /// recorded as the native host records it and its usage reported once (the package
    /// keeps no spend ledger, so no `cost`), then the compaction made with its text or,
    /// failing that, the digest, and the loop goes on.
    fn summary_done(&mut self, wait: SummaryWait, reply: Option<Value>) -> Result<Progress> {
        let SummaryWait {
            need,
            joined,
            journal,
            ..
        } = wait;
        let summary = reply.and_then(|reply| {
            self.store.summary_record(&journal, &reply);
            if let Some((input, output)) = driver::summary_usage(&reply) {
                self.emit(Event::Usage { input, output });
            }
            driver::summary_text(&reply)
        });
        let next = self.summarized(need, joined, summary);
        self.resume_loop(next)
    }

    /// Hands the driver the summary (None: the digest stands in): it compacts, and a
    /// queued prompt starting after it commits (`started`).
    fn summarized(
        &mut self,
        need: Box<SummaryNeed>,
        joined: Vec<String>,
        summary: Option<String>,
    ) -> Result<Need> {
        let next = self
            .driver
            .deliver(&mut self.store, Answer::Summary { need, summary });
        self.started(next, joined)
    }

    // ---- tools -------------------------------------------------------------------------

    /// A workspace tool: the file tools run here, in the store; `bash` goes to the
    /// session's runner.
    fn run(&mut self, need: Box<RunCall>) -> Result<Option<Progress>> {
        let process = need.call.name == "bash";
        let helper = match &need.existing {
            // A recorded attempt keeps the runner that labelled it.
            Some(attempt) => runner_of(&attempt.implementation)
                .ok_or("this step's attempt names a runner this host does not know")?,
            None if process => self
                .runner
                .ok_or("this session's runner is not connected; set one before continuing")?,
            None => host_helper(),
        };
        // As a native host refuses to run unisolated when isolation is required: the
        // app's runner (or a remote host without a sandbox) isolates nothing.
        if process
            && need.existing.is_none()
            && need.config.isolation == driver::Isolation::Required
            && helper == host_helper()
        {
            return Err(
                "isolation is required for this session, but its runner does not isolate \
                 commands; use a remote runner with a sandbox (`runner: remote`), or set \
                 `isolation: auto` to run them without one"
                    .into(),
            );
        }
        let grant = match (process, need.config.runner) {
            (false, _) => "spindle-embed-files",
            (true, Runner::Remote) => "spindle-embed-runner-remote",
            (true, _) => "spindle-embed-runner-app",
        };
        let attempt = need.existing.clone().unwrap_or(Attempt {
            id: format!("a:{}", need.id),
            effect: need.id.clone(),
            owner_epoch: self.store.epoch(),
            implementation: crate::turn::runner_label(&helper),
            grant_reference: grant.into(),
        });
        let (revision, _) = self.head()?;
        let request = DispatchRequest {
            scope: SCOPE.into(),
            owner_epoch: self.store.epoch(),
            expected_revision: revision,
            intent_revision: need.intent_revision,
            intent: need.intent.clone(),
            attempt,
        };
        if !process {
            let outcome = self.store.dispatch_file_workspace(
                &request,
                &self.queue.interrupt,
                &helper,
                |_| Ok(()),
            );
            // As the native host does after every workspace tool.
            self.emit(Event::OutputEnd);
            self.deliver_ran(need, request, helper, outcome)?;
            return Ok(None);
        }
        match self
            .store
            .reserve_runner_call(&request, &self.queue.interrupt, &helper)
        {
            Ok(RunnerReservation::Settled(outcome)) => {
                self.emit(Event::OutputEnd);
                self.deliver_ran(need, request, helper, Ok(outcome))?;
                Ok(None)
            }
            Ok(RunnerReservation::Reserved(call)) => {
                if helper == host_helper() && !self.told_unisolated {
                    self.told_unisolated = true;
                    self.emit(Event::Unisolated {
                        reason: "commands in this session run by the app, which Spindle does \
                                 not isolate",
                    });
                }
                let (files, tree) = {
                    let mut view = crate::view::ReadView::from_store(&self.store);
                    let files = view.files(&call.root).map_err(store_error)?;
                    (files, view.tree(&call.root).map_err(store_error)?)
                };
                let files = files
                    .into_iter()
                    .map(|file| {
                        (
                            file.entry.path,
                            file.size,
                            file.entry.executable,
                            file.entry.object,
                        )
                    })
                    .collect();
                let work = Work::Run {
                    runner: need.config.runner.as_str(),
                    command: call.command,
                    timeout: call.timeout,
                    network: need.config.network,
                    files,
                    links: tree
                        .links
                        .into_iter()
                        .map(|link| (link.path, link.target))
                        .collect(),
                    directories: tree.directories,
                };
                Ok(Some(self.park(
                    Waiting::Run(Box::new(RunWait {
                        need,
                        request,
                        helper,
                        code: None,
                    })),
                    work,
                )))
            }
            Err(error) => {
                self.deliver_ran(need, request, helper, Err(error))?;
                Ok(None)
            }
        }
    }

    fn deliver_ran(
        &mut self,
        need: Box<RunCall>,
        request: DispatchRequest,
        helper: [u8; 32],
        outcome: crate::Result<DispatchOutcome>,
    ) -> Result<()> {
        self.driver
            .deliver(
                &mut self.store,
                Answer::Ran {
                    need,
                    ran: Box::new(ToolRan {
                        request,
                        helper,
                        outcome,
                    }),
                },
            )
            .map(|_| ())
    }

    fn ran(
        &mut self,
        need: Box<RunCall>,
        request: DispatchRequest,
        helper: [u8; 32],
        result: crate::Result<RunnerResult>,
    ) -> Result<Progress> {
        let outcome =
            result.and_then(|result| self.store.record_runner_result(&request, &helper, result));
        self.emit(Event::OutputEnd);
        self.deliver_ran(need, request, helper, outcome)?;
        self.advance(None)
    }

    /// One file of the workspace a runner call runs over, by its object digest.
    pub fn runner_file(&mut self, work: u64, object: &str) -> Result<Vec<u8>> {
        let Some((id, Waiting::Run(run))) = &self.waiting else {
            return Err("no command is waiting to run in this session".into());
        };
        if *id != work {
            return Err("that command is no longer waited for".into());
        }
        self.store
            .runner_file(&run.request, object)
            .map_err(store_error)
    }

    /// An app tool call: reserved first, then asked (the `app_tool` event), its answer
    /// recorded once. A reserved call is asked again (`again`), never re-run blindly.
    fn app(&mut self, need: Box<driver::AppNeed>) -> Result<Option<Progress>> {
        match self.driver.reserve_app(&mut self.store, &need) {
            Ok(AppReservation::Settled(outcome)) => {
                self.deliver_app(need, Ok(outcome), None)?;
                Ok(None)
            }
            Ok(AppReservation::Reserved { call, again }) => {
                let timeout_s = need.app.timeout.as_secs();
                self.events.push(json!({
                    "type": "app_tool",
                    "call": need.call_id,
                    "tool_use_id": call.id,
                    "name": call.name,
                    "input": call.input,
                    "again": again,
                    "rerun": if need.app.rerun { "safe" } else { "no" },
                    "timeout_s": timeout_s,
                }));
                let work = Work::App {
                    call: need.call_id.clone(),
                    timeout_s,
                };
                Ok(Some(self.park(Waiting::App { need }, work)))
            }
            Err(error) => {
                self.deliver_app(need, Err(error), None)?;
                Ok(None)
            }
        }
    }

    fn deliver_app(
        &mut self,
        need: Box<driver::AppNeed>,
        outcome: crate::Result<DispatchOutcome>,
        waiting: Option<String>,
    ) -> Result<()> {
        self.driver
            .deliver(
                &mut self.store,
                Answer::App {
                    need,
                    outcome,
                    waiting,
                },
            )
            .map(|_| ())
    }

    fn app_answered(&mut self, need: Box<driver::AppNeed>, answer: AppAnswer) -> Result<Progress> {
        let outcome_name = match &answer {
            AppAnswer::Result { failed: false, .. } => "answered",
            AppAnswer::Result { failed: true, .. } => "failed",
            AppAnswer::Unknown(_) => "unknown",
            AppAnswer::NoAnswer => "no_answer",
            _ => "stopped",
        };
        let outcome = self.driver.record_app(&mut self.store, &need, answer);
        self.events
            .push(json!({"type": "app_tool_done", "call": need.call_id, "outcome": outcome_name}));
        self.deliver_app(need, outcome, None)?;
        self.advance(None)
    }

    /// The app call this session (or one of its tasks) waits on: the `call` of its
    /// `app_tool` event (a task's carry `tasks/<task>/` before it).
    pub fn waiting_app_call(&self) -> Option<String> {
        match &self.waiting {
            Some((_, Waiting::App { need })) => Some(need.call_id.clone()),
            _ => None,
        }
    }

    // ---- tasks --------------------------------------------------------------------------

    /// A step's task calls: each reserved, then run as a child session over the
    /// step's workspace; their answers are recorded in call order once all end.
    fn tasks(&mut self, needs: Vec<driver::AppNeed>) -> Result<Progress> {
        let base_revision = needs
            .first()
            .ok_or("empty task batch")?
            .request
            .intent_revision;
        let base = self.workspace_at(base_revision)?;
        let settings = self.settings()?;
        let mut calls = Vec::with_capacity(needs.len());
        for need in needs {
            let reservation = self
                .driver
                .reserve_app(&mut self.store, &need)
                .map_err(store_error)?;
            let mut call = TaskCall {
                need,
                reservation: Some(reservation),
                child: None,
                result: None,
            };
            if matches!(call.reservation, Some(AppReservation::Reserved { .. })) {
                match self.start_child(&call.need.id, &call.need.call.input, &base, &settings) {
                    Ok(child) => call.child = Some(Box::new(child)),
                    Err(why) => call.result = Some(Err(why)),
                }
            }
            calls.push(call);
        }
        let mut batch = Batch { base, calls };
        let mut works = Vec::new();
        for call in &mut batch.calls {
            let Some(child) = &mut call.child else {
                continue;
            };
            let input = &call.need.call.input;
            let progress = child.run_task(input, &batch.base);
            match progress {
                Ok(Progress::Waiting(more)) => works.extend(more),
                Ok(_) => {
                    child.finish();
                    call.result = Some(child.task_answer(input, &batch.base));
                }
                Err(why) => {
                    child.finish();
                    call.result = Some(Err(why));
                }
            }
        }
        if works.is_empty() {
            return self.tasks_done(batch);
        }
        self.next_work += 1;
        self.waiting = Some((self.next_work, Waiting::Tasks(Box::new(batch))));
        Ok(Progress::Waiting(works))
    }

    fn start_child(
        &mut self,
        id: &str,
        input: &Value,
        base: &Workspace,
        settings: &Settings,
    ) -> Result<Host> {
        if self.facts.depth >= settings.task_policy.max_depth {
            return Err("this session reached its configured task depth limit".into());
        }
        if let Some(why) =
            task_tool::model_refusal(&settings.task_policy.models, &settings.model, input)
        {
            return Err(why);
        }
        let key = task_key(id);
        let storage = self.storage.child(&key);
        // As a native host checks a task directory before it runs again: the input
        // that created it must be this one.
        let identity = task_tool::identity(input);
        let kept = storage.sidecar(TASK_SIDECAR);
        if let Some(kept) = &kept {
            let kept: Value = serde_json::from_slice(kept).map_err(|_| {
                "cannot verify task identity (its task.json is not JSON); restore its original \
                 record or use a new task id"
                    .to_string()
            })?;
            if kept != identity {
                return Err(
                    "task was started with another prompt, model, effort or files policy; \
                     use a new task id"
                        .into(),
                );
            }
        }
        let depth = self.facts.depth + 1;
        let parent = Some((self.events.clone(), key.clone()));
        let catalog = self.facts.catalog.clone();
        let mut child = match Host::open_at(storage, catalog.clone(), depth, parent.clone()) {
            Ok(child) => child,
            Err(_) => {
                let mut settings = settings.clone();
                let model = input["model"]
                    .as_str()
                    .or(input["settings"]["model"].as_str())
                    .filter(|model| !model.trim().is_empty());
                if let Some(model) = model {
                    settings.model = catalog.route(&settings.provider, Some(model))?.model;
                }
                if let Some(effort) = input["effort"]
                    .as_str()
                    .or(input["settings"]["effort"].as_str())
                {
                    settings.effort = driver::Effort::parse(effort).ok_or("invalid task effort")?;
                }
                settings.self_edit = false;
                Host::create_at(
                    self.storage.child(&key),
                    settings,
                    base.files.clone(),
                    catalog,
                    depth,
                    parent,
                )?
            }
        };
        if kept.is_none() {
            let bytes = serde_json::to_vec(&identity).map_err(|e| e.to_string())?;
            child
                .storage
                .keep_sidecar(TASK_SIDECAR, &bytes)
                .map_err(store_error)?;
        }
        child.runner = self.runner;
        child.label = format!("{}#{key}", self.label);
        self.events
            .push(json!({"type": "nested", "label": format!("task {key}: started")}));
        Ok(child)
    }

    /// Starts (or resumes) a task session's turn: its first prompt over the base
    /// workspace, or the turn it left unfinished.
    fn run_task(&mut self, input: &Value, base: &Workspace) -> Result<Progress> {
        if self.store.snapshot().map_err(store_error)?.revision == 0 {
            let prompt = input["prompt"].as_str().unwrap_or_default().to_string();
            // A task created before a crash, its first turn never committed, is opened
            // with no initial files: it starts from the base all the same.
            let files = self
                .initial_files
                .take()
                .unwrap_or_else(|| base.files.clone());
            self.begin("prompt")?;
            return self
                .start_turn(
                    &prompt,
                    Start::Import(files, base.links.clone(), base.directories.clone()),
                )
                .and_then(|need| {
                    self.driver.begin_run();
                    self.advance(Some(need))
                });
        }
        if matches!(self.head()?.1.control().phase(), Phase::Stopped(_)) {
            self.begin("continue")?;
            return Ok(Progress::Done);
        }
        self.resume(false)
    }

    /// Runs the task `task` (an id the app chooses; the same id finds the same task)
    /// from this session's files at its head to the end of its turn. Its files stay
    /// in its own session; the answer is `take_started`.
    pub fn start_task(&mut self, task: &str, input: &Value) -> Result<Progress> {
        if let Some(reason) = task_tool::refusal(input) {
            return Err(reason);
        }
        self.ensure_idle()?;
        self.begin("task")?;
        let settings = self.settings()?;
        if self.facts.depth >= settings.task_policy.max_depth {
            return Err("this session reached its configured task depth limit".into());
        }
        if let Some(why) =
            task_tool::model_refusal(&settings.task_policy.models, &settings.model, input)
        {
            return Err(why);
        }
        let head = self.store.snapshot().map_err(store_error)?.revision;
        let key = format!("app:{task}");
        let revision = self.register_app_task(&key, input, head)?;
        let base = self.workspace_at(revision)?;
        let mut child = self.start_child(&key, input, &base, &settings)?;
        let progress = child.run_task(input, &base);
        let started = Started {
            child: Box::new(child),
            input: input.clone(),
            base,
        };
        self.next_work += 1;
        self.waiting = Some((self.next_work, Waiting::Started(Box::new(started))));
        let label = format!("{}#{}", self.label, task_key(&key));
        self.after_child(&label, progress)
    }

    /// What the app-started task answered: `{task, answer, failed, state}`.
    pub fn take_started(&mut self) -> Option<Value> {
        self.started.take()
    }

    /// Registers the app-started task `key` as the native host does: its identity
    /// (the input deciding what it does) and base revision are kept in this session's
    /// store, so a later start under the same id must repeat that input, and runs over
    /// the same base. Returns the base revision.
    fn register_app_task(&mut self, key: &str, input: &Value, head: u64) -> Result<u64> {
        let identity = task_tool::identity(input).to_string().into_bytes();
        if let Some(record) = self
            .store
            .task_runs()
            .map_err(store_error)?
            .into_iter()
            .find(|run| run.task == key)
        {
            if record.input != identity
                || record.queued
                || record.origin.is_some()
                || record.state == "abandoned"
            {
                return Err(
                    "task identity or report policy conflicts with its durable record".into(),
                );
            }
            if record.state == "stopped" {
                self.store.resume_task(key).map_err(store_error)?;
            }
            return Ok(record.revision);
        }
        let base = match self
            .store
            .snapshot_at(SCOPE, head)
            .map_err(store_error)?
            .workspace
        {
            Some(root) => root,
            None => {
                let stage = Stage {
                    id: format!("task-base-{}", self.store.epoch()),
                    scope: SCOPE.into(),
                    owner_epoch: self.store.epoch(),
                };
                self.store.begin_stage(&stage).map_err(store_error)?;
                self.store
                    .stage_tree(&stage, &Tree::default())
                    .map_err(store_error)?
            }
        };
        self.store
            .register_task(&TaskRun {
                task: key.into(),
                origin: None,
                input: identity,
                base,
                revision: head,
                queued: false,
                state: "running".into(),
                answer: None,
                delivered: None,
                lease: None,
            })
            .map_err(store_error)?;
        Ok(head)
    }

    /// A finished task's answer: its last reply, and its file changes against the base
    /// unless its call discards them.
    fn task_answer(
        &mut self,
        input: &Value,
        base: &Workspace,
    ) -> std::result::Result<AppAnswer, String> {
        let (revision, state) = self.head()?;
        let Phase::Stopped(stop) = state.control().phase() else {
            return Err("its turn did not finish".into());
        };
        let key = self
            .label
            .rsplit('#')
            .next()
            .unwrap_or_default()
            .to_string();
        let said = state
            .transcript()
            .map_err(turn_error)?
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "assistant")
            .map(|message| crate::shape::text_of(&message.content))
            .unwrap_or_default();
        let now = self.workspace_at(revision)?;
        let (writes, deletes, links, directories) = if input["files"] == "discard" {
            (vec![], vec![], vec![], vec![])
        } else {
            changes(base, &now)
        };
        let complete = stop == Stop::Complete;
        let mut content = said.trim().to_string();
        if !complete {
            content = format!(
                "The task stopped before it finished ({}).\n\n{content}",
                driver::stop_text(stop)
            );
        }
        let (written, removed) = (writes.len(), deletes.len());
        if written + removed > 0 {
            content.push_str(&format!(
                "\n\n[task {key}: {written} file(s) written, {removed} removed]"
            ));
        }
        if content.trim().is_empty() {
            content = format!("[task {key} finished without an answer]");
        }
        Ok(AppAnswer::Result {
            content: content.trim().to_string(),
            failed: !complete,
            writes,
            deletes,
            links,
            directories,
            state: None,
            details: None,
        })
    }

    /// Records each task's answer in call order (conflicts with the parent's changes
    /// since the base are refused), then continues.
    fn tasks_done(&mut self, batch: Batch) -> Result<Progress> {
        let Batch { base, calls } = batch;
        for call in calls {
            let TaskCall {
                mut need,
                reservation,
                result,
                ..
            } = call;
            let mut waiting = None;
            need.request.expected_revision = self.store.snapshot().map_err(store_error)?.revision;
            let outcome = match reservation.ok_or("a task call lost its reservation")? {
                AppReservation::Settled(outcome) => Ok(outcome),
                AppReservation::Reserved { .. } => match result {
                    Some(Ok(mut answer)) => {
                        let current = self.workspace_at(need.request.expected_revision)?;
                        task_tool::refuse_conflicts(&base, &current, &mut answer);
                        self.driver.record_app(&mut self.store, &need, answer)
                    }
                    Some(Err(why)) => {
                        waiting = Some(why);
                        Err(Error::Cancelled)
                    }
                    None => {
                        waiting = Some("its session did not finish".into());
                        Err(Error::Cancelled)
                    }
                },
            };
            self.deliver_app(Box::new(need), outcome, waiting)?;
        }
        self.advance(None)
    }

    fn workspace_at(&mut self, revision: u64) -> Result<Workspace> {
        let root = match revision {
            0 => None,
            _ => {
                self.store
                    .snapshot_at(SCOPE, revision)
                    .map_err(store_error)?
                    .workspace
            }
        };
        let Some(root) = root else {
            return Ok(Workspace::default());
        };
        let tree = self.store.tree_map(SCOPE, &root).map_err(store_error)?;
        let mut files = Vec::with_capacity(tree.files.len());
        for entry in tree.files {
            let bytes = self
                .store
                .read_object(SCOPE, &entry.object)
                .map_err(store_error)?;
            files.push((entry.path, bytes, entry.executable));
        }
        Ok(Workspace {
            files,
            links: tree.links,
            directories: tree.directories,
        })
    }

    /// The name of this session's task that is, or leads to, session `label`
    /// (`<this>#<key>[#...]`).
    fn child_label(&self, label: &str) -> Option<String> {
        let rest = label.strip_prefix(&self.label)?.strip_prefix('#')?;
        let key = rest.split('#').next().filter(|key| !key.is_empty())?;
        Some(format!("{}#{key}", self.label))
    }

    /// The running task of this session that is, or leads to, session `label`. Each
    /// level hands an answer down and then takes its task's progress (`after_child`),
    /// so a task's own tasks finish into it, never past it.
    fn child_toward(&mut self, label: &str) -> Option<&mut Host> {
        let name = self.child_label(label)?;
        match &mut self.waiting {
            Some((_, Waiting::Tasks(batch))) => batch
                .calls
                .iter_mut()
                .find_map(|call| call.child.as_deref_mut().filter(|c| c.label == name)),
            Some((_, Waiting::Started(started))) if started.child.label == name => {
                Some(&mut started.child)
            }
            _ => None,
        }
    }

    /// After a task's operation moved: a finished task gives its answer, and once all
    /// tasks of the batch have, their answers are recorded and this session goes on.
    fn after_child(&mut self, label: &str, progress: Result<Progress>) -> Result<Progress> {
        if let Some((_, Waiting::Started(_))) = &self.waiting {
            if matches!(progress, Ok(Progress::Waiting(_) | Progress::Streaming)) {
                return progress;
            }
            let Some((_, Waiting::Started(mut started))) = self.waiting.take() else {
                unreachable!("matched above");
            };
            started.child.finish();
            self.cancelled.extend(started.child.take_cancelled());
            progress?;
            let answer = started.child.task_answer(&started.input, &started.base)?;
            let AppAnswer::Result {
                content, failed, ..
            } = answer
            else {
                return Err("task stopped before answering".into());
            };
            self.started = Some(json!({"task": started.child.label, "answer": content,
                "failed": failed, "state": "finished"}));
            return Ok(Progress::Done);
        }
        let name = self.child_label(label).ok_or("no such task")?;
        let Some((_, Waiting::Tasks(batch))) = &mut self.waiting else {
            return Err("no task is running in this session".into());
        };
        let index = batch
            .calls
            .iter()
            .position(|call| call.child.as_ref().is_some_and(|c| c.label == name))
            .ok_or("no such task")?;
        let base = batch.base.clone();
        let call = &mut batch.calls[index];
        match progress {
            Ok(Progress::Waiting(works)) => return Ok(Progress::Waiting(works)),
            Ok(Progress::Streaming) => return Ok(Progress::Streaming),
            Ok(Progress::Done) => {
                let mut child = call.child.take().expect("found above");
                child.finish();
                self.cancelled.extend(child.take_cancelled());
                call.result = Some(child.task_answer(&call.need.call.input, &base));
            }
            Err(why) => {
                let mut child = call.child.take().expect("found above");
                child.finish();
                self.cancelled.extend(child.take_cancelled());
                call.result = Some(Err(why));
            }
        }
        if batch.calls.iter().any(|call| call.child.is_some()) {
            return Ok(Progress::Waiting(vec![]));
        }
        let Some((_, Waiting::Tasks(batch))) = self.waiting.take() else {
            unreachable!("matched above");
        };
        self.tasks_done(*batch)
    }

    // ---- the app's answers -----------------------------------------------------------

    /// Whether session `label` (this one or a task's) waits on work `work` now, and
    /// if so whether `answer` is an answer to that work. None: the answer is late (the
    /// work was cancelled, or answered already) and changes nothing.
    pub fn accepts(&mut self, label: &str, work: u64, answer: &WorkAnswer) -> Option<bool> {
        if label != self.label {
            return self.child_toward(label)?.accepts(label, work, answer);
        }
        let (id, waiting) = self.waiting.as_ref()?;
        if *id != work || matches!(waiting, Waiting::Tasks(_) | Waiting::Started(_)) {
            return None;
        }
        Some(matches!(
            (waiting, answer),
            (
                Waiting::Authorize(_) | Waiting::SummaryAuthorize(_),
                WorkAnswer::Authorized | WorkAnswer::Unauthorized(_)
            ) | (
                Waiting::Fetch(_) | Waiting::SummaryFetch(..),
                WorkAnswer::Response { .. } | WorkAnswer::Dropped(_)
            ) | (Waiting::Run(_), WorkAnswer::Ran(_) | WorkAnswer::Dropped(_))
                | (Waiting::Backoff(_), WorkAnswer::Waited)
                | (
                    Waiting::App { .. },
                    WorkAnswer::App(_) | WorkAnswer::AppTimeout
                )
        ))
    }

    /// Whether session `label` waits on work `work` as a response streaming in.
    pub fn streams(&mut self, label: &str, work: u64) -> bool {
        if label != self.label {
            return self
                .child_toward(label)
                .is_some_and(|child| child.streams(label, work));
        }
        matches!(&self.waiting,
            Some((id, Waiting::Fetch(_) | Waiting::SummaryFetch(..))) if *id == work)
    }

    /// The app's answer to work `work` of session `label` (this one or a task's).
    pub fn deliver(&mut self, label: &str, work: u64, answer: WorkAnswer) -> Result<Progress> {
        if label != self.label {
            let progress = match self.child_toward(label) {
                Some(child) => child.deliver(label, work, answer),
                None => return Err(format!("no task `{label}` is running")),
            };
            return self.after_child(label, progress);
        }
        let Some((id, _)) = &self.waiting else {
            return Err("this session is not waiting for that".into());
        };
        if *id != work {
            return Err("that work is no longer waited for".into());
        }
        let (_, waiting) = self.waiting.take().expect("checked above");
        match (waiting, answer) {
            (Waiting::Authorize(need), WorkAnswer::Authorized) => self.reserve_model(need),
            (Waiting::Authorize(_), WorkAnswer::Unauthorized(why)) => Err(why),
            (
                Waiting::Fetch(fetch),
                WorkAnswer::Response {
                    status,
                    content_type,
                    retry_after,
                    body,
                },
            ) => self.fetched(*fetch, status, content_type, retry_after, body),
            (Waiting::Fetch(fetch), WorkAnswer::Dropped(dropped)) => {
                self.fetch_failed(*fetch, dropped)
            }
            (Waiting::SummaryAuthorize(wait), WorkAnswer::Authorized) => {
                self.summary_authorized(wait)
            }
            // No credentials for the summary: the digest stands in (the turn's own
            // request asks for them again).
            (Waiting::SummaryAuthorize(wait), WorkAnswer::Unauthorized(_)) => {
                self.summary_done(*wait, None)
            }
            (
                Waiting::SummaryFetch(wait, reading),
                WorkAnswer::Response {
                    status,
                    content_type,
                    ..
                },
            ) => self.summary_fetched(*wait, *reading, status, content_type),
            (Waiting::SummaryFetch(wait, _), WorkAnswer::Dropped(dropped)) => {
                self.summary_failed(*wait, dropped_error(dropped))
            }
            (Waiting::Run(run), WorkAnswer::Ran(result)) => {
                let RunWait {
                    need,
                    request,
                    helper,
                    // A script call still waiting ends with its command (its children
                    // stay uncertain in the journal).
                    code: _,
                } = *run;
                self.ran(need, request, helper, Ok(result))
            }
            (Waiting::Run(run), WorkAnswer::Dropped(_)) => {
                let RunWait {
                    need,
                    request,
                    helper,
                    // A script call still waiting ends with its command (its children
                    // stay uncertain in the journal).
                    code: _,
                } = *run;
                self.ran(need, request, helper, Err(Error::Cancelled))
            }
            (Waiting::Backoff(backoff), WorkAnswer::Waited) => {
                let next = self
                    .driver
                    .deliver(&mut self.store, Answer::Waited(backoff));
                self.resume_loop(next)
            }
            (Waiting::App { need }, WorkAnswer::App(answer)) => self.app_answered(need, answer),
            (Waiting::App { need }, WorkAnswer::AppTimeout) => {
                self.app_answered(need, AppAnswer::NoAnswer)
            }
            (waiting, _) => {
                self.waiting = Some((work, waiting));
                Err("that answer does not fit the work waited for".into())
            }
        }
    }

    /// A streamed body part for work `work` of session `label`.
    pub fn deliver_chunk(&mut self, label: &str, work: u64, bytes: &[u8]) -> Result<Progress> {
        if label != self.label {
            let progress = match self.child_toward(label) {
                Some(child) => child.deliver_chunk(label, work, bytes),
                None => return Err(format!("no task `{label}` is running")),
            };
            return self.after_child(label, progress);
        }
        self.chunk(work, bytes)
    }

    /// One file of the workspace that work `work` of session `label` (this one or a
    /// task's) runs a command over.
    pub fn runner_file_of(&mut self, label: &str, work: u64, object: &str) -> Result<Vec<u8>> {
        if label != self.label {
            return match self.child_toward(label) {
                Some(child) => child.runner_file_of(label, work, object),
                None => Err(format!("no task `{label}` is running")),
            };
        }
        self.runner_file(work, object)
    }

    /// The answer to app call `call` (from `app_tool_result`), for this session or a
    /// task's (`tasks/<task>/<call>`).
    pub fn answer_app(&mut self, call: &str, answer: AppAnswer) -> Result<Progress> {
        if let Some(rest) = call.strip_prefix("tasks/")
            && let Some((key, inner)) = rest.split_once('/')
        {
            let label = format!("{}#{key}", self.label);
            let progress = match self.child_toward(&label) {
                Some(child) => child.answer_app(inner, answer),
                None => return Err("that app tool call is not waiting for an answer".into()),
            };
            return self.after_child(&label, progress);
        }
        match &self.waiting {
            Some((_, Waiting::App { need })) if need.call_id == call => {}
            _ => return Err("that app tool call is not waiting for an answer".into()),
        }
        let Some((_, Waiting::App { need })) = self.waiting.take() else {
            unreachable!("matched above");
        };
        self.app_answered(need, answer)
    }

    /// Reads of committed state, in the API's stable forms.
    pub fn view(&self) -> crate::view::ReadView<'_> {
        crate::view::ReadView::from_store(&self.store)
    }

    /// `status`: where the session stands.
    pub fn status(&mut self) -> Result<Value> {
        let settings = self.settings()?;
        let mut value = crate::reads::head_on(&mut self.view())?;
        let report = self.store.reconciliation().map_err(store_error)?;
        let state = match self.store.snapshot().map_err(store_error)?.revision {
            0 => "no turns yet".to_string(),
            _ => match self.head()?.1.control().phase() {
                Phase::Stopped(stop) => driver::stop_text(stop).to_string(),
                Phase::ModelWait => "waiting for a model response (continue it)".into(),
                Phase::ToolsWait => "running tools (continue it)".into(),
                _ => "between steps (continue it)".into(),
            },
        };
        value["session"] = json!(self.label);
        value["provider"] = json!(settings.provider);
        value["model"] = json!(settings.model);
        // As the native host reports it: no owner epoch before the first turn.
        value["epoch"] = match value["head"] == 0 {
            true => Value::Null,
            false => json!(self.store.epoch()),
        };
        value["state"] = json!(state);
        value["unresolved"] = json!(report.unresolved_attempts.len());
        value["abandoned"] = json!(report.abandoned_attempts.len());
        value["rejected"] = json!(report.rejected_attempts.len());
        value["isolation"] = json!(settings.isolation.as_str());
        value["effort"] = json!(settings.effort.as_str());
        value["busy"] = json!(self.running.is_some());
        value["running"] = json!(self.running);
        value["settings"] = crate::protocol::settings_json(&settings);
        Ok(value)
    }
}

/// What an operation ended with (`Host::finish`).
pub struct Finished {
    pub head: u64,
    pub stopped: Option<Value>,
    pub rewound: Option<Value>,
    pub unsent: Vec<String>,
    /// A stop ended it.
    pub interrupted: bool,
}

/// Why a store would not open, saying who holds it or where its session moved when
/// the place's fence says so.
fn open_error(storage: &dyn Storage, error: Error) -> String {
    if matches!(error, Error::Locked)
        && let Some(note) = storage.holder()
    {
        if let Some(moved) = note.strip_prefix(crate::MOVED_FENCE) {
            return format!(
                "this session moved elsewhere ({moved}); it continues where its bundle was \
                 imported. To use this copy as the session instead, remove its \
                 store/{}",
                crate::FENCE
            );
        }
        return format!("{} (held by: {note})", store_error(error));
    }
    store_error(error)
}

/// Adds the task sessions kept beside `storage` (at bundle path `prefix`) to a bundle's
/// `files`: each store opened, imaged and kept open in `opened`, with its sidecars.
fn bundle_tasks(
    storage: &dyn Storage,
    prefix: &str,
    files: &mut Vec<BundleFile>,
    opened: &mut Vec<(Box<dyn Storage>, Store)>,
) -> Result<()> {
    let mut keys = storage.tasks();
    keys.sort();
    for key in keys {
        let child = storage.child(&key);
        let store = child.open(false).map_err(|error| match error {
            Error::Locked => format!("task {key} is in use; wait for it, then export again"),
            other => store_error(other),
        })?;
        let at = format!("{prefix}tasks/{key}/");
        files.push(BundleFile {
            path: format!("{at}{}", bundle::STORE),
            bytes: store.image().map_err(store_error)?,
        });
        for name in [SETTINGS_SIDECAR, TASK_SIDECAR] {
            if let Some(bytes) = child.sidecar(name) {
                files.push(BundleFile {
                    path: format!("{at}{name}"),
                    bytes,
                });
            }
        }
        bundle_tasks(child.as_ref(), &at, files, opened)?;
        opened.push((child, store));
    }
    Ok(())
}

/// The runner identity an attempt label names (`dext-(host|strict)-bash-v1-<hex>`).
fn runner_of(label: &str) -> Option<[u8; 32]> {
    let hex = label
        .strip_prefix("dext-host-bash-v1-")
        .or_else(|| label.strip_prefix("dext-strict-bash-v1-"))?;
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut helper = [0u8; 32];
    for (index, byte) in helper.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * index..2 * index + 2], 16).ok()?;
    }
    (crate::turn::runner_label(&helper) == label).then_some(helper)
}

fn attempt_error(error: AttemptError) -> Error {
    Error::Model(wire_error(error))
}

fn wire_error(error: AttemptError) -> ModelError {
    match error {
        AttemptError::Invalid => ModelError::Invalid,
        AttemptError::Unauthorized => ModelError::Unauthorized,
        AttemptError::Interrupted => ModelError::Interrupted,
        AttemptError::Deadline => ModelError::Deadline,
        AttemptError::Transport => ModelError::Transport,
        AttemptError::Http(status) => ModelError::Http(status),
        AttemptError::Protocol => ModelError::Protocol,
        AttemptError::Exhausted => ModelError::Exhausted,
    }
}

/// Why a request the app could not finish failed, as the native transport says it.
fn dropped_error(dropped: Dropped) -> AttemptError {
    match dropped {
        Dropped::Transport => AttemptError::Transport,
        Dropped::Deadline => AttemptError::Deadline,
        Dropped::Stopped => AttemptError::Interrupted,
    }
}

/// A compaction summary's reply as the native host records one: its text and usage,
/// or why there is none (worded as the native transport words it; once sent, it may
/// have run, and a status says the provider refused it).
fn summary_reply(
    prepared: &Value,
    read: std::result::Result<NativeResponse, AttemptError>,
) -> Value {
    let error = match read {
        Ok(response)
            if response
                .blocks
                .iter()
                .any(|block| matches!(block, Block::ToolUse { .. })) =>
        {
            return json!({"ok": false, "may_have_run": true,
                "error": "nested model returned an unexpected tool call"});
        }
        Ok(response) => {
            // An app signs in with a key; only the ChatGPT backend is a subscription.
            let subscription = prepared["contract"] == RequestContract::ChatGptResponses.as_str();
            return json!({"ok": true, "data": {"text": crate::shape::text_of(&response.blocks),
                "usage": response.usage, "provider": prepared["provider"],
                "model": prepared["model"], "subscription": subscription}});
        }
        Err(error) => error,
    };
    let mut failure = json!({"ok": false, "may_have_run": true,
        "error": driver::model_error(wire_error(error))});
    if let AttemptError::Http(status) = error {
        failure["status"] = json!(status);
    }
    failure
}

/// The newest step before the head when it is a rewind target: where a cut request
/// is prepared again from.
fn redo_point(store: &Store) -> Option<u64> {
    let mut view = crate::view::ReadView::from_store(store);
    let steps = view.recent_steps(32).ok()?;
    let before = steps.get(steps.len().checked_sub(2)?)?.revision;
    matches!(view.targets(&[before]).as_slice(), [(revision, _)] if *revision == before)
        .then_some(before)
}

type Changes = (
    Vec<File>,
    Vec<String>,
    Vec<crate::workspace::Link>,
    Vec<String>,
);

/// A task's workspace changes from `base` to `now`: files written, paths removed,
/// links and empty directories added (as the native host computes them).
fn changes(base: &Workspace, now: &Workspace) -> Changes {
    let before: std::collections::HashMap<&str, (&[u8], bool)> = base
        .files
        .iter()
        .map(|(path, bytes, executable)| (path.as_str(), (bytes.as_slice(), *executable)))
        .collect();
    let kept: std::collections::HashSet<&str> =
        now.files.iter().map(|(path, _, _)| path.as_str()).collect();
    let writes = now
        .files
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
    let links = now
        .links
        .iter()
        .filter(|link| !base.links.contains(link))
        .cloned()
        .collect();
    let directories = now
        .directories
        .iter()
        .filter(|path| !base.directories.contains(path))
        .cloned()
        .collect();
    deletes.extend(
        base.links
            .iter()
            .filter(|link| !now.links.iter().any(|kept| kept.path == link.path))
            .map(|link| link.path.clone()),
    );
    deletes.extend(
        base.directories
            .iter()
            .filter(|path| !now.directories.contains(path))
            .cloned(),
    );
    (writes, deletes, links, directories)
}

/// A probe key shaped as a token (a JWT) whose payload holds `value` at each claim's
/// path, signed `signature` (never checked: only the header layer reads it).
fn claim_token(claims: &[Claim], value: &str, signature: &str) -> String {
    let mut payload = Value::Object(Default::default());
    for claim in claims {
        let mut place = &mut payload;
        for (depth, step) in claim.path.iter().enumerate() {
            let last = depth + 1 == claim.path.len();
            let next = match last {
                true => Value::String(value.into()),
                false => Value::Object(Default::default()),
            };
            place = place
                .as_object_mut()
                .expect("claim paths name objects")
                .entry(step.clone())
                .or_insert(next);
        }
    }
    let part = |bytes: &[u8]| base64_url(bytes);
    format!(
        "{}.{}.{signature}",
        part(b"{}"),
        part(payload.to_string().as_bytes())
    )
}

/// Base64url without padding, as JWTs write their parts.
fn base64_url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (index, byte)| {
            n | u32::from(*byte) << (16 - 8 * index)
        });
        for index in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * index) & 63) as usize] as char);
        }
    }
    out
}

/// `claims` as `Work::Authorize` carries them to the app.
pub(crate) fn claims_json(claims: &[Claim]) -> Value {
    Value::Array(
        claims
            .iter()
            .map(|claim| json!({"slot": claim.slot, "path": claim.path}))
            .collect(),
    )
}
