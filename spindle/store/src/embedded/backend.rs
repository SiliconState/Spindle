//! The public API's methods (docs/API.md) over embedded sessions, behind the same
//! JSON-RPC protocol as `spindle api` (`crate::protocol`). Sessions are named by
//! `dir`, as there: a directory (Node), a name (browser storage) or a key (memory).
//!
//! Besides replies and `event` notifications, the frames carry the app's work:
//! `work` (`{session, id, work}`: something to do for a running turn, answered with
//! `Embedded::deliver`) and `work_cancel` (`{session, id}`: the turn stopped waiting
//! for it; the app ends it). The `EmbeddedClient` in the package does both.
use super::env::Catalog;
use super::host::{self, Dropped, Finished, Host, Progress, Work, WorkAnswer};
use super::storage::Storage;
use crate::broker::RunnerResult;
use crate::driver::app_tools::{self, AppAnswer, BuiltinTools};
use crate::driver::{Compaction, Effort, Isolation, Runner, Settings};
use crate::protocol::{
    Backend, Fault, Outcome, Params, Reply, SETTINGS, TurnOp, bad, base64_decode, base64_encode,
    busy, conflict, event_frame, failed, files_param, not_found, not_running, reply_frame,
};
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Optional capabilities of the embedded transport, reported by `info`.
pub const FEATURES: &[&str] = &[
    "events",
    "expected_head",
    "steering",
    "attachments",
    "revisions",
    "steer_now",
    "app_tools",
    "state",
    "recover",
    "tasks",
    "runner",
    "code_bridge",
    // `authorize` work names claims of the key its headers carry (`{account}`).
    "key_claims",
];

/// Where sessions live: the storage for a `dir`, and whether one is there.
pub trait Places {
    /// The storage of the session named `dir` (refused if the name is not usable).
    fn storage(&self, dir: &str) -> Result<Box<dyn Storage>, String>;
    /// Whether a session is stored under `dir`.
    fn exists(&self, dir: &str) -> bool;
}

struct Session {
    host: Host,
    /// The request a running operation answers when it ends.
    request: Option<Value>,
    /// What a `task_start` adds to its reply.
    task: Option<String>,
}

/// The embedded sessions of one app, answering the API's methods.
pub struct Embedded {
    places: Box<dyn Places>,
    catalog: Arc<Catalog>,
    sessions: RefCell<BTreeMap<String, Session>>,
    frames: RefCell<Vec<Value>>,
    runner: RefCell<Option<[u8; 32]>>,
    /// The sandbox identity the app's own runner declares (None: it isolates nothing).
    app_runner: RefCell<Option<[u8; 32]>>,
    /// What scripts' `http()` may reach: a code policy with the app's `http` grants.
    http: RefCell<Value>,
}

/// A script request's state as the API gives it.
fn code_step(step: host::CodeStep) -> Value {
    match step {
        host::CodeStep::Reply(reply) => json!({"reply": reply}),
        host::CodeStep::Children {
            call,
            children,
            parallel,
        } => json!({"call": call, "children": children, "parallel": parallel}),
        host::CodeStep::Pending(left) => json!({"pending": left}),
    }
}

fn work_json(work: Work) -> Value {
    match work {
        Work::Authorize {
            provider,
            model,
            endpoint,
            contract,
            headers,
            auth,
            claims,
        } => {
            let object = |pairs: Vec<(String, String)>| {
                pairs
                    .into_iter()
                    .map(|(name, value)| (name, Value::String(value)))
                    .collect::<Map<String, Value>>()
            };
            json!({
                "kind": "authorize",
                "provider": provider,
                "model": model,
                "endpoint": endpoint,
                "contract": contract,
                "headers": object(headers),
                "auth": object(auth),
                "key_slot": host::KEY_SLOT,
                "claims": host::claims_json(&claims),
            })
        }
        Work::Fetch {
            endpoint,
            gzip,
            body,
        } => {
            json!({"kind": "fetch", "endpoint": endpoint, "gzip": gzip, "body": base64_encode(&body)})
        }
        Work::Run {
            runner,
            command,
            timeout,
            network,
            files,
            links,
            directories,
        } => json!({
            "kind": "run",
            "runner": runner,
            "command": command,
            "timeout_s": timeout,
            "network": network,
            "files": files.into_iter().map(|(path, size, executable, object)| {
                json!({"path": path, "size": size, "executable": executable, "object": object})
            }).collect::<Vec<_>>(),
            "links": links.into_iter().map(|(path, target)| json!({"path": path, "target": target}))
                .collect::<Vec<_>>(),
            "directories": directories,
        }),
        Work::Wait { ms } => json!({"kind": "wait", "ms": ms}),
        Work::App { call, timeout_s } => {
            json!({"kind": "app", "call": call, "timeout_s": timeout_s})
        }
    }
}

/// The app's answer to a work, from its JSON form.
pub fn answer_of(value: &Value) -> Result<WorkAnswer, String> {
    let text = |key: &str| value[key].as_str().map(String::from);
    Ok(match value["kind"].as_str().unwrap_or_default() {
        "authorized" => WorkAnswer::Authorized,
        "unauthorized" => WorkAnswer::Unauthorized(
            text("error").unwrap_or_else(|| "the app has no credentials for this provider".into()),
        ),
        "response" => WorkAnswer::Response {
            status: value["status"]
                .as_u64()
                .and_then(|status| u16::try_from(status).ok())
                .ok_or("a response needs its status")?,
            content_type: text("content_type"),
            retry_after: text("retry_after"),
            body: match value["body"].as_str() {
                Some(body) => base64_decode(body).ok_or("`body` is not standard base64")?,
                None => vec![],
            },
        },
        "dropped" => WorkAnswer::Dropped(match value["reason"].as_str() {
            Some("deadline") => Dropped::Deadline,
            Some("stopped") => Dropped::Stopped,
            _ => Dropped::Transport,
        }),
        "ran" => {
            let writes = files_param(value.get("writes")).map_err(|fault| fault.message)?;
            let strings = |key: &str| -> Result<Vec<String>, String> {
                match &value[key] {
                    Value::Null => Ok(vec![]),
                    Value::Array(items) => items
                        .iter()
                        .map(|item| item.as_str().map(String::from))
                        .collect::<Option<Vec<_>>>()
                        .ok_or(format!("`{key}` lists paths")),
                    _ => Err(format!("`{key}` lists paths")),
                }
            };
            let links = match &value["links"] {
                Value::Null => vec![],
                Value::Array(items) => items
                    .iter()
                    .map(|item| {
                        Some(crate::workspace::Link {
                            path: item["path"].as_str()?.into(),
                            target: item["target"].as_str()?.into(),
                        })
                    })
                    .collect::<Option<Vec<_>>>()
                    .ok_or("`links` lists {path, target}")?,
                _ => return Err("`links` lists {path, target}".into()),
            };
            WorkAnswer::Ran(RunnerResult {
                output: text("output").unwrap_or_default(),
                success: value["success"] == true,
                writes,
                deletes: strings("deletes")?,
                links,
                directories: strings("directories")?,
                failure: text("failure"),
            })
        }
        "waited" => WorkAnswer::Waited,
        "app_timeout" => WorkAnswer::AppTimeout,
        other => return Err(format!("`{other}` is not an answer to a work")),
    })
}

/// New settings for a session on `provider` (its default model unless `model`), as
/// the embedded package starts them: file tools, compaction by a summary the session's
/// model writes (the digest stands in when that request fails), and isolation `auto`
/// (the app's runner runs commands unisolated and says so; a remote runner
/// sandboxes them; a session moved to a native host is sandboxed there where it can be,
/// never run without one because the package could not sandbox).
fn new_settings(catalog: &Catalog, provider: &str, model: Option<&str>) -> Result<Settings, Fault> {
    let route = catalog.route(provider, model).map_err(bad)?;
    let mut settings = Settings::from_json(&json!({
        "provider": route.profile.id,
        "model": route.model,
        "isolation": "auto",
        "compaction": "model",
        "builtin_tools": "files",
    }))
    .map_err(bad)?;
    settings.effort = Effort::Default;
    Ok(settings)
}

/// Applies the settings params of `create` and `settings` this host honors.
fn apply_settings(
    settings: &mut Settings,
    params: &Params,
    catalog: &Catalog,
    route: bool,
) -> Result<(), Fault> {
    if route {
        let provider = params.str("provider")?.map(String::from);
        let model = params.str("model")?;
        if provider.is_some() || model.is_some() {
            let provider = provider.unwrap_or_else(|| settings.provider.clone());
            let resolved = catalog.route(&provider, model).map_err(bad)?;
            settings.provider = resolved.profile.id;
            settings.model = resolved.model;
        }
    }
    if let Some(network) = params.bool("network")? {
        settings.network = network;
    }
    if let Some(toolchain) = params.bool("toolchain")? {
        settings.toolchain = toolchain;
    }
    // A session from a native host may hold sources: `[]` clears them; none can be set.
    match params.0.get("sources") {
        None | Some(Value::Null) => {}
        Some(value) if value == &json!([]) => settings.sources.clear(),
        Some(_) => {
            return Err(bad(
                "`sources` names host directories, which this app has none of",
            ));
        }
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
    // Null clears a native session's budget; none can be set here (no spend ledger).
    match params.0.get("max_usd") {
        None => {}
        Some(Value::Null) => settings.max_usd = None,
        Some(_) => {
            return Err(bad(
                "`max_usd` needs the native host's spend ledger; enforce a budget in the app",
            ));
        }
    }
    if let Some(runner) = params.str("runner")? {
        settings.runner =
            Runner::parse(runner).ok_or_else(|| bad("`runner` must be none, remote or app"))?;
    }
    app_tools::apply_json(settings, &Value::Object(params.0.clone())).map_err(bad)?;
    let route = catalog
        .route(&settings.provider, Some(&settings.model))
        .map_err(bad)?;
    crate::shape::reasoning(&route, settings.effort).map_err(bad)?;
    host::check_settings(settings).map_err(bad)
}

impl Embedded {
    pub fn new(places: Box<dyn Places>, catalog: Catalog) -> Self {
        Self {
            places,
            catalog: Arc::new(catalog),
            sessions: RefCell::default(),
            frames: RefCell::default(),
            runner: RefCell::default(),
            app_runner: RefCell::default(),
            http: RefCell::new(json!({})),
        }
    }

    fn send(&self, frame: Value) {
        self.frames.borrow_mut().push(frame);
    }

    /// Handles one request line; returns every frame written since the last call.
    pub fn handle(&self, line: &[u8]) -> Vec<Value> {
        crate::protocol::handle(self, line, |frame| self.send(frame.clone()));
        self.take_frames()
    }

    /// The provider catalog these sessions use (`Catalog::to_json`).
    pub fn catalog(&self) -> Value {
        self.catalog.to_json()
    }

    /// The efforts `provider`'s `model` takes here, `default` first, each with what it
    /// sends on that route: `[{effort, label, short}]` (empty for a route the catalog
    /// does not know).
    pub fn efforts(&self, provider: &str, model: &str) -> Value {
        let Ok(route) = self.catalog.route(provider, Some(model)) else {
            return json!([]);
        };
        Effort::ALL
            .into_iter()
            .filter_map(|effort| {
                let reasoning = crate::shape::reasoning(&route, effort).ok()?;
                Some(json!({
                    "effort": effort.as_str(),
                    "label": reasoning.label,
                    "short": reasoning.short,
                }))
            })
            .collect()
    }

    pub fn take_frames(&self) -> Vec<Value> {
        std::mem::take(&mut *self.frames.borrow_mut())
    }

    /// The remote runner's identity (a Spindle host's sandbox digest), for sessions
    /// whose `runner` is `remote`; None disconnects it.
    pub fn set_runner(&self, helper: Option<[u8; 32]>) {
        *self.runner.borrow_mut() = helper;
        self.rebind();
    }

    /// The sandbox identity of the app's own runner, for sessions whose `runner` is
    /// `app`: its commands then count as isolated (receipts name it, `isolation:
    /// required` accepts it). None: the app's runner isolates nothing, as by default.
    pub fn set_app_runner(&self, helper: Option<[u8; 32]>) {
        *self.app_runner.borrow_mut() = helper;
        self.rebind();
    }

    /// The endpoints scripts' `http()` may reach: grants as a native code policy's
    /// `http` list, checked as native checks them (URL prefix, methods, timeout, response
    /// size). The app sends each request. Grants hold no headers here: the package keeps
    /// no secrets. An empty list grants nothing, as by default.
    pub fn set_http_grants(&self, grants: &Value) -> Result<(), Fault> {
        let policy = json!({"http": grants});
        crate::code_http::validate(&policy).map_err(bad)?;
        if let Some(grant) = grants
            .as_array()
            .into_iter()
            .flatten()
            .find(|grant| grant.get("headers").is_some())
        {
            return Err(bad(format!(
                "http grant {}: grants hold no headers here (the package keeps no secrets)",
                grant["name"].as_str().unwrap_or("?")
            )));
        }
        *self.http.borrow_mut() = policy;
        Ok(())
    }

    fn rebind(&self) {
        let runners = self.runners();
        for session in self.sessions.borrow_mut().values_mut() {
            Self::bind_runner(&mut session.host, runners);
        }
    }

    /// The remote and app runner identities, as bound now.
    fn runners(&self) -> (Option<[u8; 32]>, Option<[u8; 32]>) {
        (*self.runner.borrow(), *self.app_runner.borrow())
    }

    fn bind_runner(host: &mut Host, (remote, app): (Option<[u8; 32]>, Option<[u8; 32]>)) {
        let runner = host.settings().map(|s| s.runner).unwrap_or_default();
        host.set_runner(match runner {
            Runner::None => None,
            Runner::Remote => remote,
            Runner::App => Some(app.unwrap_or_else(host::host_helper)),
        });
    }

    fn top(label: &str) -> &str {
        label.split('#').next().unwrap_or(label)
    }

    /// The app's answer to work `work` of session `label`; returns the new frames. A
    /// late answer (to work cancelled or answered already) changes nothing; one that
    /// is no answer to the work waited for is refused, and the work still waits.
    pub fn deliver(
        &self,
        label: &str,
        work: u64,
        answer: WorkAnswer,
    ) -> Result<Vec<Value>, String> {
        let dir = Self::top(label).to_string();
        let progress = {
            let mut sessions = self.sessions.borrow_mut();
            let Some(session) = sessions.get_mut(&dir) else {
                return Ok(self.take_frames());
            };
            match session.host.accepts(label, work, &answer) {
                None => return Ok(self.take_frames()),
                Some(false) => {
                    return Err(format!(
                        "that answer does not fit work {work} of {label}, which still waits"
                    ));
                }
                Some(true) => session.host.deliver(label, work, answer),
            }
        };
        self.progressed(&dir, progress);
        Ok(self.take_frames())
    }

    /// A streamed part of a response for work `work` of session `label`: the new
    /// frames, and whether the response is still waited for (false: stop reading).
    pub fn chunk(&self, label: &str, work: u64, bytes: &[u8]) -> (Vec<Value>, bool) {
        let dir = Self::top(label).to_string();
        let progress = {
            let mut sessions = self.sessions.borrow_mut();
            let Some(session) = sessions.get_mut(&dir) else {
                return (self.take_frames(), false);
            };
            if !session.host.streams(label, work) {
                return (self.take_frames(), false);
            }
            session.host.deliver_chunk(label, work, bytes)
        };
        let streaming = matches!(progress, Ok(Progress::Streaming));
        self.progressed(&dir, progress);
        (self.take_frames(), streaming)
    }

    /// One file of the workspace a runner call (work `work` of `label`) runs over.
    pub fn runner_file(&self, label: &str, work: u64, object: &str) -> Result<Vec<u8>, String> {
        let dir = Self::top(label).to_string();
        let mut sessions = self.sessions.borrow_mut();
        let session = sessions.get_mut(&dir).ok_or("no such session")?;
        session.host.runner_file_of(label, work, object)
    }

    /// After an operation of `dir` moved: its events and works go out, and an ended
    /// operation answers its request.
    fn progressed(&self, dir: &str, progress: Result<Progress, String>) {
        let mut sessions = self.sessions.borrow_mut();
        let Some(session) = sessions.get_mut(dir) else {
            return;
        };
        let request = session.request.clone().unwrap_or(Value::Null);
        for event in session.host.take_events() {
            self.send(event_frame(dir, &request, event));
        }
        for (label, id) in session.host.take_cancelled() {
            self.send(json!({"jsonrpc": "2.0", "method": "work_cancel",
                "params": {"session": label, "id": id}}));
        }
        let ended = match progress {
            Ok(Progress::Waiting(works)) => {
                for (label, id, work) in works {
                    // A task's app call is named as its `app_tool` event names it.
                    let work = match work {
                        Work::App { call, timeout_s } => Work::App {
                            call: label
                                .split('#')
                                .skip(1)
                                .map(|key| format!("tasks/{key}/"))
                                .collect::<String>()
                                + &call,
                            timeout_s,
                        },
                        work => work,
                    };
                    self.send(json!({"jsonrpc": "2.0", "method": "work",
                        "params": {"session": label, "id": id, "work": work_json(work)}}));
                }
                return;
            }
            Ok(Progress::Streaming) => return,
            Ok(Progress::Done) => Ok(()),
            Err(why) => Err(why),
        };
        let finished = session.host.finish();
        for event in session.host.take_events() {
            self.send(event_frame(dir, &request, event));
        }
        for (label, id) in session.host.take_cancelled() {
            self.send(json!({"jsonrpc": "2.0", "method": "work_cancel",
                "params": {"session": label, "id": id}}));
        }
        let task = session.task.take();
        let request = session.request.take();
        let answered = match (&task, &ended) {
            (Some(_), Ok(())) => session.host.take_started(),
            _ => None,
        };
        // A host that lost its store is dropped: the next call opens the session again.
        if session.host.detached() {
            sessions.remove(dir);
        }
        let Some(request) = request else {
            return;
        };
        let outcome = Self::turn_outcome(dir, ended, finished, task, answered);
        self.send(reply_frame(&request, outcome));
    }

    fn turn_outcome(
        dir: &str,
        ended: Result<(), String>,
        finished: Finished,
        task: Option<String>,
        answered: Option<Value>,
    ) -> Outcome {
        let Finished {
            head,
            stopped,
            rewound,
            unsent,
            interrupted,
        } = finished;
        match ended {
            Ok(()) => {
                let mut result = json!({
                    "session": dir,
                    "head": head,
                    "stopped": stopped,
                    "rewound": rewound,
                    "unsent": unsent,
                });
                if let (Some(_), Some(Value::Object(answer))) = (task, answered) {
                    result.as_object_mut().expect("an object").extend(answer);
                }
                Ok(result)
            }
            Err(why) => {
                let fault = if interrupted {
                    Fault::new(-32006, "stopped", why)
                } else {
                    failed(why)
                };
                Err(fault
                    .with("head", json!(head))
                    .with("unsent", json!(unsent)))
            }
        }
    }

    fn open_session(&self, dir: &str) -> Result<(), Fault> {
        if self.sessions.borrow().contains_key(dir) {
            return Ok(());
        }
        if !self.places.exists(dir) {
            return Err(not_found(format!("no session in {dir}; create one first")));
        }
        let storage = self.places.storage(dir).map_err(bad)?;
        let mut host = Host::open(dir, storage, self.catalog.clone()).map_err(failed)?;
        Self::bind_runner(&mut host, self.runners());
        self.sessions.borrow_mut().insert(
            dir.to_string(),
            Session {
                host,
                request: None,
                task: None,
            },
        );
        Ok(())
    }

    fn dir(params: &Params) -> Result<String, Fault> {
        params.required("dir").map(String::from)
    }

    fn status_of(&self, dir: &str) -> Outcome {
        self.open_session(dir)?;
        let mut sessions = self.sessions.borrow_mut();
        let session = sessions.get_mut(dir).expect("opened above");
        session.host.status().map_err(failed)
    }

    fn with_host<T>(
        &self,
        dir: &str,
        run: impl FnOnce(&mut Host) -> Result<T, Fault>,
    ) -> Result<T, Fault> {
        self.open_session(dir)?;
        let mut sessions = self.sessions.borrow_mut();
        run(&mut sessions.get_mut(dir).expect("opened above").host)
    }

    fn create(&self, params: Params) -> Outcome {
        let mut keys = vec!["dir", "files"];
        keys.extend(SETTINGS);
        keys.push("runner");
        params.allow(&keys)?;
        let dir = Self::dir(&params)?;
        let provider = params.required("provider")?;
        let files = files_param(params.0.get("files"))?;
        if self.sessions.borrow().contains_key(&dir) || self.places.exists(&dir) {
            return Err(conflict(format!(
                "{dir} already holds a session; open it instead"
            )));
        }
        let mut settings = new_settings(&self.catalog, provider, params.str("model")?)?;
        if params.0.get("builtin_tools").is_none() {
            settings.builtin_tools = BuiltinTools::Files;
        }
        apply_settings(&mut settings, &params, &self.catalog, false)?;
        let storage = self.places.storage(&dir).map_err(bad)?;
        let mut host =
            Host::create(&dir, storage, settings, files, self.catalog.clone()).map_err(failed)?;
        Self::bind_runner(&mut host, self.runners());
        self.sessions.borrow_mut().insert(
            dir.clone(),
            Session {
                host,
                request: None,
                task: None,
            },
        );
        let mut status = self.status_of(&dir)?;
        status["imported"] = Value::Null;
        Ok(status)
    }

    fn start(&self, method: &str, params: Params, id: Option<Value>) -> Result<Reply, Fault> {
        let (_, op) = crate::protocol::turn_op(method, &params)?;
        let expected = params.u64("expected_head")?;
        let dir = Self::dir(&params)?;
        self.open_session(&dir)?;
        let progress = {
            let mut sessions = self.sessions.borrow_mut();
            let session = sessions.get_mut(&dir).expect("opened above");
            if let Some(running) = session.host.running() {
                return Err(busy(format!(
                    "a {running} is already running in this session"
                )));
            }
            if let Some(expected) = expected {
                let head = crate::reads::head_on(&mut session.host.view()).map_err(failed)?["head"]
                    .clone();
                if head != json!(expected) {
                    return Err(Fault::new(
                        -32003,
                        "head_moved",
                        format!("the session's head is {head}, not {expected}"),
                    ));
                }
            }
            session.request = Some(id.clone().unwrap_or(Value::Null));
            match op {
                TurnOp::Prompt { text, files, state } => session.host.prompt(&text, files, state),
                TurnOp::Continue { until_unknown } => session.host.resume(until_unknown),
                TurnOp::Retry => session.host.retry(),
                TurnOp::Rewind { revision } => session.host.rewind(revision),
                TurnOp::Task {
                    task,
                    input,
                    report,
                } => {
                    if report != "wait" {
                        Err("tasks here run to their end (`report: wait`)".to_string())
                    } else {
                        let progress = session.host.start_task(&task, &input);
                        session.task = Some(task);
                        progress
                    }
                }
            }
        };
        if id.is_none() {
            // A notification still runs; its end is not answered.
            let mut sessions = self.sessions.borrow_mut();
            if let Some(session) = sessions.get_mut(&dir) {
                session.request = None;
            }
        }
        self.progressed(&dir, progress);
        Ok(Reply::Later)
    }

    /// Writes session `dir` to a session bundle: its store and every task
    /// store kept here, with their sidecars (bytes, not a JSON reply). Unless `keep`,
    /// the session then moves: it closes here and its copy is sealed (`spindle-moved`),
    /// so it continues only where the bundle is imported.
    ///
    /// `settings` (an object of settings keys) change the bundle's copy only, for the
    /// host it goes to: a native host's settings this app refuses (all built-in tools,
    /// a budget) included; they are checked as any host reads stored settings.
    pub fn export_session(
        &self,
        dir: &str,
        keep: bool,
        settings: Option<&Value>,
    ) -> Result<Vec<u8>, Fault> {
        self.open_session(dir)?;
        let mut sessions = self.sessions.borrow_mut();
        let session = sessions.get_mut(dir).expect("opened above");
        if let Some(running) = session.host.running() {
            return Err(busy(format!(
                "a {running} is running in this session; wait for it or stop it"
            )));
        }
        let copied = match settings {
            None => None,
            Some(Value::Object(changes)) => {
                let mut value = session
                    .host
                    .settings()
                    .and_then(|s| s.to_json())
                    .map_err(failed)?;
                for (key, change) in changes {
                    if !SETTINGS.contains(&key.as_str()) && key != "runner" {
                        return Err(bad(format!("`{key}` is not a session setting")));
                    }
                    value[key] = change.clone();
                }
                // Where it goes resolves models from its own catalog: name both.
                if changes.contains_key("provider") != changes.contains_key("model") {
                    return Err(bad(
                        "settings for the copy name `provider` and `model` together",
                    ));
                }
                for key in ["provider", "model"] {
                    if value[key]
                        .as_str()
                        .is_none_or(|text| text.trim().is_empty())
                    {
                        return Err(bad(format!(
                            "settings for the copy: `{key}` must be a name"
                        )));
                    }
                }
                let settings = Settings::from_json(&value);
                Some(settings.map_err(|error| bad(format!("settings for the copy: {error}")))?)
            }
            Some(_) => return Err(bad("`settings` must be an object of settings")),
        };
        let (files, opened) = session.host.bundle_files(copied.as_ref()).map_err(failed)?;
        let written = crate::bundle::write(files).map_err(failed)?;
        if !keep {
            let note = format!(
                "exported at head {} by an app's embedded Spindle",
                written.facts.head
            );
            // The session itself first: if it cannot be sealed, nothing has moved (and
            // the bundle is not handed out). Its tasks' copies after, as best they can.
            session.host.seal(&note).map_err(failed)?;
            for (place, store) in opened {
                let _ = place.seal(&note);
                drop(store);
            }
            sessions.remove(dir);
        }
        Ok(written.bytes)
    }

    /// Imports a session bundle as session `dir` (nothing there yet): the bundle is
    /// checked whole before any file is written, the session's own store last (so a
    /// cut import leaves no session), then the session opens here, the same session
    /// it was (identity, history, unknown outcomes).
    pub fn import_session(&self, dir: &str, bytes: &[u8]) -> Outcome {
        if self.sessions.borrow().contains_key(dir) || self.places.exists(dir) {
            return Err(conflict(format!(
                "{dir} already holds a session; import into a new place"
            )));
        }
        let bundle = crate::bundle::read(bytes).map_err(bad)?;
        let storage = self.places.storage(dir).map_err(bad)?;
        // As natively: into an empty place only (what a cut import left included).
        if !storage.is_empty() {
            return Err(conflict(format!(
                "{dir} is not empty; import into a new place"
            )));
        }
        for file in &bundle.files {
            let mut place: Option<Box<dyn Storage>> = None;
            let mut rest = file.path.as_str();
            while let Some((key, tail)) = rest
                .strip_prefix("tasks/")
                .and_then(|after| after.split_once('/'))
            {
                place = Some(match &place {
                    None => storage.child(key),
                    Some(parent) => parent.child(key),
                });
                rest = tail;
            }
            let at = place.as_deref().unwrap_or(storage.as_ref());
            let written = if rest == crate::bundle::STORE {
                at.import_store(&file.bytes)
            } else {
                at.keep_sidecar(rest, &file.bytes)
            };
            written.map_err(|error| failed(crate::driver::store_error(error)))?;
        }
        let mut host = Host::open(dir, storage, self.catalog.clone()).map_err(failed)?;
        Self::bind_runner(&mut host, self.runners());
        self.sessions.borrow_mut().insert(
            dir.to_string(),
            Session {
                host,
                request: None,
                task: None,
            },
        );
        let mut status = self.status_of(dir)?;
        status["imported"] = json!({"head": bundle.head, "identity": bundle.identity,
            "files": bundle.files.len()});
        Ok(status)
    }

    fn recover(&self, dir: &str) -> Outcome {
        self.with_host(dir, |host| {
            let report = host.store().reconciliation().map_err(|e| failed(format!("{e:?}")))?;
            let mut head = crate::reads::head_on(&mut host.view()).map_err(failed)?;
            let items: Vec<Value> = report
                .unresolved_attempts
                .iter()
                .map(|attempt| {
                    json!({"kind": "attempt", "id": attempt, "outcome": "unknown",
                        "next": "continue"})
                })
                .collect();
            let work = if head["phase"] == "empty" || head["phase"] == "stopped" {
                json!({"work": "none", "why": null})
            } else if items.is_empty() {
                json!({"work": "ready", "why": null})
            } else {
                json!({"work": "blocked",
                    "why": "an outcome is unknown: continue settles it from the store, never sends it again"})
            };
            head["wake"] = work.clone();
            Ok(json!({"head": head["head"], "phase": head["phase"], "items": items,
                "wake": work}))
        })
    }
}

impl Backend for Embedded {
    fn call(&self, method: &str, params: Params, id: Option<Value>) -> Reply {
        let now = Reply::Now;
        let result = (|| -> Result<Reply, Fault> {
            Ok(match method {
                "info" => {
                    params.allow(&[])?;
                    now(Ok(json!({
                        "api_version": 1,
                        "transport": "embedded",
                        "features": FEATURES,
                        "version": env!("CARGO_PKG_VERSION"),
                        "schema": crate::CAPABILITIES.schema,
                        "engine": crate::turn::ENGINE,
                        "build": "embedded",
                    })))
                }
                "providers" => {
                    params.allow(&[])?;
                    now(Ok(json!({"providers": self.catalog.providers_json()})))
                }
                "list" => {
                    params.allow(&[])?;
                    let sessions = self.sessions.borrow();
                    let list: Vec<Value> = sessions
                        .iter()
                        .map(|(dir, session)| {
                            let running = session.host.running();
                            json!({"session": dir, "busy": running.is_some(), "running": running})
                        })
                        .collect();
                    now(Ok(json!({"sessions": list})))
                }
                "create" => now(self.create(params)),
                "open" | "status" => {
                    params.allow(&["dir"])?;
                    now(self.status_of(&Self::dir(&params)?))
                }
                "close" => {
                    params.allow(&["dir"])?;
                    let dir = Self::dir(&params)?;
                    let mut sessions = self.sessions.borrow_mut();
                    let closed = match sessions.get(&dir) {
                        None => false,
                        Some(session) => {
                            if let Some(running) = session.host.running() {
                                return Err(busy(format!("a {running} is running; stop it first")));
                            }
                            sessions.remove(&dir);
                            true
                        }
                    };
                    now(Ok(json!({"session": dir, "closed": closed})))
                }
                "prompt" | "continue" | "retry" | "rewind" | "task_start" => {
                    self.start(method, params, id)?
                }
                "stop" => {
                    params.allow(&["dir"])?;
                    let dir = Self::dir(&params)?;
                    let progress = {
                        let mut sessions = self.sessions.borrow_mut();
                        let session = sessions
                            .get_mut(&dir)
                            .ok_or_else(|| not_running("this session is not open in this app"))?;
                        session.host.stop().map_err(not_running)
                    };
                    let progress = progress?;
                    self.progressed(&dir, progress);
                    now(Ok(json!({"session": dir, "stopping": true})))
                }
                "steer" => {
                    params.allow(&["dir", "text", "now"])?;
                    let hurry = params.bool("now")? == Some(true);
                    let text = match params.str("text")? {
                        None if hurry => None,
                        text => Some(
                            text.filter(|text| {
                                !text.trim().is_empty() && text.len() <= crate::driver::MAX_STEER
                            })
                            .ok_or_else(|| bad("`text` must be 1 to 65536 bytes and not blank"))?
                            .to_string(),
                        ),
                    };
                    let dir = Self::dir(&params)?;
                    let steered = {
                        let mut sessions = self.sessions.borrow_mut();
                        let session = sessions
                            .get_mut(&dir)
                            .ok_or_else(|| not_running("this session is not open in this app"))?;
                        if !matches!(
                            session.host.running(),
                            Some("prompt" | "continue" | "retry")
                        ) {
                            return Err(not_running(
                                "no turn is running in this session; send it with `prompt`",
                            ));
                        }
                        session.host.steer(text, hurry)
                    };
                    let (queued, progress) = steered.map_err(|why| {
                        if why.starts_with("no message") {
                            not_running(why)
                        } else {
                            busy(why)
                        }
                    })?;
                    self.progressed(&dir, progress);
                    now(Ok(json!({"session": dir, "queued": queued})))
                }
                "state" => {
                    params.allow(&["dir", "revision"])?;
                    let dir = Self::dir(&params)?;
                    let revision = params.u64("revision")?;
                    now(self.with_host(&dir, |host| {
                        crate::reads::state_on(&mut host.view(), revision).map_err(not_found)
                    }))
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
                    let dir = Self::dir(&params)?;
                    let request = id.clone().unwrap_or(Value::Null);
                    let outcome = self.with_host(&dir, |host| {
                        if matches!(host.running(), Some("prompt" | "continue" | "retry")) {
                            if expected.is_some() {
                                return Err(busy(
                                    "a turn is running; `expected_head` needs an idle session",
                                ));
                            }
                            let queued = host.queue_state(patch).map_err(busy)?;
                            return Ok(json!({"session": dir, "queued": queued}));
                        }
                        if let Some(running) = host.running() {
                            return Err(busy(format!(
                                "a {running} is running in this session; wait for it or stop it"
                            )));
                        }
                        if let Some(expected) = expected {
                            let head =
                                crate::reads::head_on(&mut host.view()).map_err(failed)?["head"]
                                    .clone();
                            if head != json!(expected) {
                                return Err(Fault::new(
                                    -32003,
                                    "head_moved",
                                    format!("the session's head is {head}, not {expected}"),
                                ));
                            }
                        }
                        let revision = host.set_state(&patch).map_err(conflict)?;
                        Ok(json!({"session": dir, "revision": revision}))
                    });
                    // Its `state` event names this request, as a turn's events do.
                    let mut sessions = self.sessions.borrow_mut();
                    if let Some(session) = sessions.get_mut(&dir) {
                        for event in session.host.take_events() {
                            self.send(event_frame(&dir, &request, event));
                        }
                    }
                    now(outcome)
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
                    let dir = Self::dir(&params)?;
                    let progress = {
                        let mut sessions = self.sessions.borrow_mut();
                        let session = sessions
                            .get_mut(&dir)
                            .ok_or_else(|| not_running("this session is not open in this app"))?;
                        session.host.answer_app(&call, answer)
                    };
                    match progress {
                        Err(why) if why.contains("not waiting") => return Err(conflict(why)),
                        progress => self.progressed(&dir, progress),
                    }
                    now(Ok(json!({"session": dir, "call": call, "accepted": true})))
                }
                "history" | "files" => {
                    params.allow(&["dir", "revision", "from"])?;
                    let dir = Self::dir(&params)?;
                    let revision = params.u64("revision")?;
                    let from =
                        usize::try_from(params.u64("from")?.unwrap_or(0)).unwrap_or(usize::MAX);
                    now(self.with_host(&dir, |host| {
                        let mut view = host.view();
                        let read = if method == "history" {
                            crate::reads::messages_from(&mut view, revision, from).map(
                                |mut history| {
                                    crate::reads::annotate_display(&mut history["messages"]);
                                    crate::reads::annotate_steering(&mut history["messages"]);
                                    history
                                },
                            )
                        } else {
                            crate::reads::files_on(&mut view, revision)
                        };
                        read.map_err(|error| match revision {
                            Some(_) => not_found(error),
                            None => failed(error),
                        })
                    }))
                }
                "read" => {
                    params.allow(&["dir", "path", "revision"])?;
                    let dir = Self::dir(&params)?;
                    let path = params.required("path")?.to_string();
                    let revision = params.u64("revision")?;
                    now(self.with_host(&dir, |host| {
                        let bytes = crate::reads::read_on(&mut host.view(), revision, &path)
                            .map_err(not_found)?;
                        let size = bytes.len();
                        let (encoding, content) = match String::from_utf8(bytes) {
                            Ok(text) => ("utf8", text),
                            Err(error) => ("base64", base64_encode(error.as_bytes())),
                        };
                        Ok(json!({"path": path, "size": size, "encoding": encoding, "content": content}))
                    }))
                }
                "targets" => {
                    params.allow(&["dir", "limit"])?;
                    let dir = Self::dir(&params)?;
                    let listed = params
                        .u64("limit")?
                        .map_or(48, |limit| limit.clamp(1, 8192) as usize);
                    now(self.with_host(&dir, |host| {
                        if let Some(running) = host.running() {
                            return Err(busy(format!(
                                "a {running} is running in this session; wait for it or stop it"
                            )));
                        }
                        let targets = crate::reads::targets_among(&mut host.view(), listed)
                            .map_err(failed)?;
                        Ok(json!({"targets": targets}))
                    }))
                }
                "settings" => {
                    let mut keys = vec!["dir"];
                    keys.extend(SETTINGS);
                    keys.push("runner");
                    params.allow(&keys)?;
                    let dir = Self::dir(&params)?;
                    let catalog = self.catalog.clone();
                    let runners = self.runners();
                    now(self.with_host(&dir, |host| {
                        let mut settings = host.settings().map_err(failed)?;
                        let changes = SETTINGS
                            .iter()
                            .chain(&["runner"])
                            .any(|key| params.0.contains_key(*key));
                        if changes {
                            apply_settings(&mut settings, &params, &catalog, true)?;
                            host.set_settings(settings.clone()).map_err(conflict)?;
                            Self::bind_runner(host, runners);
                        }
                        Ok(crate::protocol::settings_json(&settings))
                    }))
                }
                "recover" => {
                    params.allow(&["dir"])?;
                    now(self.recover(&Self::dir(&params)?))
                }
                // The code-mode bridge: a script of the running command asks for calls.
                "code_call" => {
                    params.allow(&["dir", "request"])?;
                    let dir = Self::dir(&params)?;
                    let request = params
                        .0
                        .get("request")
                        .filter(|request| request.is_object())
                        .cloned()
                        .ok_or_else(|| bad("`request` must be the script's request object"))?;
                    let http = self.http.borrow().clone();
                    // `dir` is the run's session label: a running task's is `dir#key`.
                    now(self.with_host(Self::top(&dir), |host| {
                        host.code_call_of(&dir, &request, &http)
                            .map(code_step)
                            .map_err(failed)
                    }))
                }
                "code_answer" => {
                    params.allow(&["dir", "call", "index", "answer"])?;
                    let dir = Self::dir(&params)?;
                    let call = params
                        .u64("call")?
                        .ok_or_else(|| bad("`call` is required"))?;
                    let index = params
                        .u64("index")?
                        .and_then(|index| usize::try_from(index).ok())
                        .ok_or_else(|| bad("`index` is a child's number"))?;
                    let answer = params
                        .0
                        .get("answer")
                        .cloned()
                        .ok_or_else(|| bad("`answer` is what the child brought back"))?;
                    now(self.with_host(Self::top(&dir), |host| {
                        host.code_answer_of(&dir, call, index, &answer)
                            .map(code_step)
                            .map_err(bad)
                    }))
                }
                _ => {
                    return Err(Fault::new(
                        -32601,
                        "method_not_found",
                        format!("no method `{method}` in this app"),
                    ));
                }
            })
        })();
        result.unwrap_or_else(|fault| Reply::Now(Err(fault)))
    }
}
