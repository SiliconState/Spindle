//! HTTP API v1 (docs/API.md): `/v1/...` routes as a thin layer over the host's own
//! handlers. Non-browser clients authenticate with `Authorization: Bearer TOKEN`, a
//! paired device token or a named API token (`spindle token new NAME`); browsers may
//! use the pairing cookie under the same same-origin rules as `/api`. Errors carry a
//! stable envelope `{"error": message, "code": name}`. Events are the stable
//! `Event::to_json` forms, kept in their own bounded ring beside the UI's.
use super::{Host, Refused, Result, Watch, auth, err, hash, http, id_ok, lock, now_ms, random};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use spindle_store::agent::{self, Settings};
use spindle_store::api;
use spindle_store::view::View;
use std::collections::VecDeque;
use std::io::Write;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, MutexGuard};
use std::time::{Duration, Instant};

/// Optional capabilities of the HTTP transport, reported by `GET /v1`. Names are only
/// ever added within an API version.
const FEATURES: &[&str] = &[
    "commands",
    "receipts",
    "expected_head",
    "events",
    "steering",
    "attachments",
    "revisions",
    "archive",
    "targets",
    "providers",
    "bearer_tokens",
    "usage",
    "preview",
    "preview_github",
    "models",
    "self_edit",
    "display",
    "steer_now",
    "app_tools",
    "state",
    "recover",
    "tasks",
    "runner",
];
/// Command kinds v1 admits (`/api` also has the self-edit `promote`, `rollback` and
/// `update`).
const KINDS: [&str; 11] = [
    "new",
    "say",
    "continue",
    "retry",
    "rewind",
    "stop",
    "rename",
    "delete",
    "settings",
    "state",
    "task_start",
];
/// Kinds that act on a thread's head: `expected_head` applies, and the host checks it.
const HEADED: [&str; 7] = [
    "say",
    "continue",
    "retry",
    "rewind",
    "settings",
    "state",
    "task_start",
];
/// The host's own refusals that v1 names more precisely than by status.
const BUSY: &str = "session busy; wait or stop the run";
const REUSED: &str = "request_id reused with different payload";
const MOVED: &str = "session changed; refresh before submitting";
const IDLE: &str = "no unstopped worker is running";
/// Named API tokens are device rows whose id starts with this.
const TOKEN_PREFIX: &str = "api:";
const MAX_TOKENS: i64 = 16;
/// Bounds of the stable event ring (the UI's has the same).
const MAX_EVENTS: usize = 1024;
const MAX_BYTES: usize = 512 * 1024;

/// Stable events of a thread, numbered from 1 in this host process (`boot`).
#[derive(Default)]
pub(super) struct Ring {
    seq: u64,
    bytes: usize,
    events: VecDeque<(u64, String)>,
}
impl Ring {
    fn push(&mut self, event: &Value) {
        let text = event.to_string();
        self.seq += 1;
        self.bytes += text.len();
        self.events.push_back((self.seq, text));
        while self.events.len() > MAX_EVENTS || self.bytes > MAX_BYTES {
            if let Some((_, text)) = self.events.pop_front() {
                self.bytes -= text.len();
            }
        }
    }
    /// The retained events, oldest first.
    #[cfg(test)]
    pub(super) fn events(&self) -> impl Iterator<Item = &str> {
        self.events.iter().map(|(_, text)| text.as_str())
    }
    /// The end of a command, from the UI's `done` event. A budget refusal carries the
    /// stable code `budget` (nothing was sent; Continue sends it once the budget allows).
    pub(super) fn done(&mut self, done: &Value) {
        let mut event = done.clone();
        event["type"] = json!("command_done");
        if event["error"]
            .as_str()
            .is_some_and(|error| error.starts_with(spindle_store::protocol::BUDGET_REFUSAL))
        {
            event["code"] = json!("budget");
        }
        self.push(&event);
    }
}
impl Watch {
    /// Adds a stable event (`Event::to_json`) to the thread's v1 ring.
    pub(super) fn push_api(&self, event: Value) {
        self.lock().api.push(&event);
        self.changed.notify_all();
    }
    /// Waits until the v1 ring moves past `cursor`, the thread is deleted, or `until`.
    fn wait_api(&self, cursor: Option<u64>, until: Instant) -> MutexGuard<'_, super::Live> {
        let mut live = self.lock();
        while !live.deleted && cursor == Some(live.api.seq) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            live = match self.changed.wait_timeout(live, left) {
                Ok((live, _)) => live,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        live
    }
}

/// Whether a path belongs to v1.
pub(super) fn path(path: &str) -> bool {
    path == "/v1" || path.starts_with("/v1/")
}

/// A v1 refusal: HTTP status, stable code, message and extra fields.
struct Failure {
    status: u16,
    code: &'static str,
    message: String,
    extra: Map<String, Value>,
}
impl Failure {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            extra: Map::new(),
        }
    }
    fn with(mut self, key: &str, value: Value) -> Self {
        self.extra.insert(key.into(), value);
        self
    }
}
fn bad(message: impl Into<String>) -> Failure {
    Failure::new(400, "bad_request", message)
}
fn missing(message: impl Into<String>) -> Failure {
    Failure::new(404, "not_found", message)
}
fn internal(error: impl std::fmt::Display) -> Failure {
    Failure::new(500, "internal", error.to_string())
}
/// A host refusal in v1 terms. Below 500 nothing was admitted; a 500 from a command
/// means its outcome is unknown (send the same request_id again to learn it).
fn classify(Refused(status, message): Refused, command: bool) -> Failure {
    let code = match (status, message.as_str()) {
        (409, BUSY) => "busy",
        (409, REUSED) => "request_id_reused",
        (409, MOVED) => "head_moved",
        (409, IDLE) => "not_running",
        (409, text) if text.starts_with("four sessions") => "busy",
        (400, _) => "bad_request",
        (401, _) => "unauthorized",
        (403, _) => "forbidden",
        (404, _) => "not_found",
        (409, _) => "conflict",
        (429, _) => "rate_limited",
        (500, _) if command => "outcome_unknown",
        _ => "internal",
    };
    Failure::new(status, code, message)
}
impl From<Refused> for Failure {
    fn from(refused: Refused) -> Self {
        classify(refused, false)
    }
}

type Answer = std::result::Result<(), Failure>;

/// This host's runner for remote runs, opened on first use. Only its sandbox: the
/// host runs its own sessions isolated (`new_settings`), and a token never buys more
/// than they get, so without a sandbox remote runs are unavailable.
fn runner() -> std::result::Result<&'static agent::runner::Runner, String> {
    static RUNNER: std::sync::OnceLock<std::result::Result<agent::runner::Runner, String>> =
        std::sync::OnceLock::new();
    RUNNER
        .get_or_init(|| agent::runner::Runner::open(agent::Isolation::Required))
        .as_ref()
        .map_err(Clone::clone)
}

/// Serves one v1 request. Every outcome is written here in the v1 envelope.
pub(super) fn route(
    host: &Arc<Host>,
    stream: &mut TcpStream,
    mut request: http::Request,
    extra: &mut String,
    public: bool,
    origin: &str,
) -> std::result::Result<(), Refused> {
    let answer = (|| -> Answer {
        let device = authenticate(host, &request, extra, public, origin)?;
        if request.method == "POST" {
            http::read_body(stream, &mut request).map_err(bad)?;
        }
        dispatch(host, stream, &request, extra, &device)
    })();
    if let Err(failure) = answer {
        let mut body = failure.extra;
        body.insert("error".into(), json!(failure.message));
        body.insert("code".into(), json!(failure.code));
        let _ = super::send_json(stream, failure.status, &Value::Object(body), extra);
    }
    Ok(())
}

/// The device behind a request: a bearer token (device or API token; never the
/// pairing key), or the pairing cookie with `/api`'s same-origin rules for POST.
fn authenticate(
    host: &Host,
    request: &http::Request,
    extra: &mut String,
    public: bool,
    origin: &str,
) -> std::result::Result<String, Failure> {
    let unauthorized = |message: &str| Failure::new(401, "unauthorized", message);
    if let Some(header) = request.headers.get("authorization") {
        let token = header
            .strip_prefix("Bearer ")
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| unauthorized("Authorization must be `Bearer TOKEN`"))?;
        // The pairing key only pairs screens; it is never a credential by itself.
        if auth::same(token.as_bytes(), host.token.as_bytes()) {
            return Err(unauthorized("unknown or revoked token"));
        }
        return host
            .authenticate(token, None, public, &mut String::new())
            .map_err(internal)?
            .ok_or_else(|| unauthorized("unknown or revoked token"));
    }
    let token = auth::cookie(request.headers.get("cookie"))
        .ok_or_else(|| unauthorized("send `Authorization: Bearer TOKEN` or pair this browser"))?;
    // Nor as a cookie: `/api` still trades it for a device token, v1 never does.
    if auth::same(token.as_bytes(), host.token.as_bytes()) {
        return Err(unauthorized("Pair this device first"));
    }
    let device = host
        .authenticate(
            token,
            request.headers.get("user-agent").map(String::as_str),
            public,
            extra,
        )
        .map_err(internal)?
        .ok_or_else(|| unauthorized("Pair this device first"))?;
    if request.method == "POST"
        && (request.headers.get("origin").map(String::as_str) != Some(origin)
            || request.headers.get("x-spindle").map(String::as_str) != Some("1"))
    {
        return Err(Failure::new(
            403,
            "forbidden",
            "same-origin command required (or use a bearer token)",
        ));
    }
    Ok(device)
}

fn send(stream: &mut TcpStream, status: u16, body: &Value, extra: &str) -> Answer {
    super::send_json(stream, status, body, extra).map_err(Failure::from)
}

fn dispatch(
    host: &Arc<Host>,
    stream: &mut TcpStream,
    request: &http::Request,
    extra: &str,
    device: &str,
) -> Answer {
    let parts: Vec<&str> = request
        .path
        .trim_end_matches('/')
        .split('/')
        .skip(2)
        .collect();
    let get = request.method == "GET";
    let revision = request
        .query
        .get("revision")
        .map(|text| text.parse::<u64>())
        .transpose()
        .map_err(|_| bad("revision must be a non-negative integer"))?;
    match (request.method.as_str(), parts.as_slice()) {
        ("GET", []) => send(stream, 200, &discovery(host, request), extra),
        ("GET", ["sessions"]) => {
            let sessions: Vec<Value> = host
                .sessions()
                .map_err(internal)?
                .into_iter()
                .map(|row| {
                    // The list's states as codes: "reply saved" is reply_saved.
                    let mut status = row["status"].clone();
                    if let Some(state) = status["state"].as_str() {
                        status["state"] = json!(state.replace(' ', "_"));
                    }
                    json!({"id": row["id"], "name": row["name"], "status": status})
                })
                .collect();
            send(stream, 200, &json!({ "sessions": sessions }), extra)
        }
        // The remote runner of embedded sessions (`agent::runner`).
        ("GET", ["runner"]) => {
            let runner = runner().map_err(|why| Failure::new(503, "unavailable", &why))?;
            send(stream, 200, &runner.identity(), extra)
        }
        ("POST", ["run"]) => {
            let runner = runner().map_err(|why| Failure::new(503, "unavailable", &why))?;
            let body: Value =
                serde_json::from_slice(&request.body).map_err(|_| bad("the body is not JSON"))?;
            let result = runner
                .run(&body, &std::sync::atomic::AtomicBool::new(false))
                .map_err(|why| bad(&why))?;
            send(stream, 200, &result, extra)
        }
        ("GET", ["providers"]) => {
            let providers = api::providers_json().map_err(internal)?;
            send(stream, 200, &json!({ "providers": providers }), extra)
        }
        ("GET", ["models"]) => {
            let models = agent::model_catalog().map_err(internal)?;
            send(stream, 200, &models, extra)
        }
        ("POST", ["commands"]) => {
            let (status, receipt) = command(host, &request.body)?;
            send(stream, status, &receipt, extra)
        }
        ("GET", ["usage"]) => {
            // Minutes east of UTC that "today" is counted in, as /api/usage takes it.
            let offset = request
                .query
                .get("tz")
                .and_then(|tz| tz.parse::<i64>().ok())
                .filter(|tz| tz.abs() <= 18 * 60)
                .unwrap_or(0);
            send(stream, 200, &host.usage(offset).map_err(internal)?, extra)
        }
        ("GET", ["receipts", id]) => match receipt(host, id)? {
            Some(receipt) => send(stream, 200, &receipt, extra),
            None => Err(missing(
                "request_id not admitted (or its receipt has expired)",
            )),
        },
        (_, ["sessions", id, rest @ ..]) => {
            if !id_ok(id) || host.live(id).is_err() {
                return Err(missing("session not found"));
            }
            let dir = host.dir(id);
            match (get, rest) {
                (true, []) => send(stream, 200, &session(host, id)?, extra),
                (true, ["messages"]) => {
                    let mut messages = api::messages_at(&dir, revision, 0).map_err(missing)?;
                    agent::annotate_display(&mut messages["messages"]);
                    agent::annotate_steering(&mut messages["messages"]);
                    send(stream, 200, &messages, extra)
                }
                (true, ["tasks"]) => {
                    let mut tasks = api::tasks_json(&dir).map_err(internal)?;
                    // Over HTTP a task is named by `task`; the host's paths stay its own.
                    for task in tasks["tasks"].as_array_mut().into_iter().flatten() {
                        task.as_object_mut().map(|task| task.remove("dir"));
                    }
                    send(stream, 200, &tasks, extra)
                }
                (true, ["tasks", task, "messages"])
                    if task.len() == 20 && task.bytes().all(|b| b.is_ascii_hexdigit()) =>
                {
                    let child = dir.join("tasks").join(task);
                    let messages = api::messages_at(&child, revision, 0).map_err(missing)?;
                    send(stream, 200, &messages, extra)
                }
                (false, ["tasks", task, "wait"]) => {
                    let key = task_key(&dir, task)?;
                    let data: Value = serde_json::from_slice(&request.body)
                        .map_err(|error| bad(error.to_string()))?;
                    let seconds = match data.get("timeout_s") {
                        None => 600,
                        Some(value) => value
                            .as_u64()
                            .filter(|value| *value <= 86400)
                            .ok_or_else(|| bad("timeout_s must be 0-86400"))?,
                    };
                    let mut result =
                        agent::tasks::wait(&dir, &key, Duration::from_secs(seconds), &false.into())
                            .map_err(internal)?;
                    result["task"] = json!(task);
                    send(stream, 200, &result, extra)
                }
                (false, ["tasks", task, "stop"]) => {
                    let key = task_key(&dir, task)?;
                    let stopped = agent::tasks::stop(&dir, &key).map_err(internal)?;
                    send(stream, 200, &json!({"task":task,"stopping":stopped}), extra)
                }
                (true, ["recovery"]) => {
                    let report = api::recovery(&dir).map_err(internal)?;
                    send(stream, 200, &report, extra)
                }
                (true, ["state"]) => {
                    let state = api::state_at(&dir, revision).map_err(missing)?;
                    send(stream, 200, &state, extra)
                }
                (false, ["state"]) => {
                    let data: Value = serde_json::from_slice(&request.body)
                        .map_err(|e| bad(format!("body: {e}")))?;
                    let patch = data
                        .get("patch")
                        .filter(|patch| {
                            patch.is_object()
                                && patch.to_string().len() <= spindle_store::transition::MAX_STATE
                        })
                        .cloned()
                        .ok_or_else(|| {
                            bad("`patch` is a JSON merge patch: an object of at most 1 MiB")
                        })?;
                    let live = host.live(id).map_err(missing)?;
                    let queued = {
                        let state = live.lock();
                        let Some(queue) = state.app_state.clone() else {
                            return Err(Failure::new(
                                409,
                                "not_running",
                                "no turn is running; send it as a `state` command",
                            ));
                        };
                        let mut queue = queue
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if queue.len() >= 32 {
                            return Err(Failure::new(
                                429,
                                "rate_limited",
                                "32 state changes are already waiting for the turn",
                            ));
                        }
                        queue.push(patch);
                        queue.len()
                    };
                    send(stream, 202, &json!({ "queued": queued }), extra)
                }
                (true, ["usage"]) => {
                    let usage = api::usage_json(&dir).map_err(internal)?;
                    send(stream, 200, &usage, extra)
                }
                (true, ["files"]) => {
                    let files = api::files_at(&dir, revision).map_err(missing)?;
                    send(stream, 200, &files, extra)
                }
                (true, ["file"]) => {
                    let path = request
                        .query
                        .get("path")
                        .ok_or_else(|| bad("path required"))?;
                    let bytes = api::read_at(&dir, revision, path).map_err(missing)?;
                    http::respond(stream, 200, "application/octet-stream", &bytes, extra)
                        .map_err(internal)
                }
                (true, ["archive"]) => {
                    let revision = match revision {
                        Some(revision) => revision,
                        None => head(host, id)?,
                    };
                    super::archive(host, stream, id, revision, extra).map_err(Failure::from)
                }
                (true, ["targets"]) => {
                    // A running command's thread cannot be rewound: no targets meanwhile.
                    let busy = host.live(id).map_err(missing)?.lock().busy;
                    let targets = if busy {
                        vec![]
                    } else {
                        api::recent_targets(&dir).map_err(internal)?
                    };
                    send(stream, 200, &json!({ "targets": targets }), extra)
                }
                (true, ["events"]) => events(host, stream, id, request, extra, device),
                (false, ["steer"]) => {
                    let queued = steer(host, id, &request.body)?;
                    send(stream, 202, &json!({ "queued": queued }), extra)
                }
                (false, ["app_tool_result"]) => {
                    let data: Value = serde_json::from_slice(&request.body)
                        .map_err(|e| bad(format!("body: {e}")))?;
                    let live = host.live(id).map_err(missing)?;
                    super::app_tool::answer(&live, &data).map_err(|refused| match refused.0 {
                        409 => Failure::new(409, "conflict", refused.1),
                        status => Failure::new(status, "bad_request", refused.1),
                    })?;
                    send(stream, 200, &json!({ "accepted": true }), extra)
                }
                (false, ["upload"]) => {
                    let saved = host.upload(id, &request.body).map_err(bad)?;
                    send(stream, 200, &saved, extra)
                }
                (false, ["preview"]) => {
                    let data: Value = serde_json::from_slice(&request.body)
                        .map_err(|e| bad(format!("body: {e}")))?;
                    let granted = host.preview_grant(id, &data).map_err(bad)?;
                    send(stream, 200, &granted, extra)
                }
                (_, ["messages" | "files" | "file" | "archive" | "targets" | "events"] | [])
                | (_, ["steer" | "upload" | "preview" | "app_tool_result"]) => Err(not_allowed()),
                _ => Err(missing("no such route")),
            }
        }
        (_, [] | ["sessions"] | ["providers"] | ["models"] | ["commands"] | ["receipts", _]) => {
            Err(not_allowed())
        }
        _ => Err(missing("no such route")),
    }
}

fn not_allowed() -> Failure {
    Failure::new(
        405,
        "method_not_allowed",
        "method not allowed on this route",
    )
}

/// `GET /v1`: what this host speaks.
fn discovery(host: &Host, request: &http::Request) -> Value {
    json!({
        "api": "spindle",
        "api_version": api::API_VERSION,
        "features": FEATURES,
        "version": env!("CARGO_PKG_VERSION"),
        "build": host.build.get(),
        "schema": spindle_store::CAPABILITIES.schema,
        "engine": spindle_store::turn::ENGINE,
        "auth": if request.headers.contains_key("authorization") { "bearer" } else { "cookie" },
        "limits": {
            "body_bytes": 64 * 1024,
            // App tool answers, state changes and commands (which may carry tools).
            "app_body_bytes": 8 * 1024 * 1024,
            "prompt_bytes": 16 * 1024,
            "steer_bytes": 16 * 1024,
            "queued_steers": super::MAX_STEERS,
            "upload_bytes": super::MAX_UPLOAD,
            "uploads_per_day": super::MAX_UPLOADS,
            "attachments": super::MAX_ATTACHMENTS,
            "sessions": super::MAX_SESSIONS,
            "running": 4,
            "receipts": super::MAX_COMMANDS,
            "receipt_days": super::KEEP_RECEIPTS / (24 * 3600 * 1000),
            "events_retained": MAX_EVENTS,
            "event_bytes": MAX_BYTES,
        },
    })
}

/// A thread's committed head, read without its owner lock.
fn head(host: &Host, id: &str) -> std::result::Result<u64, Failure> {
    let mut view = View::open(&host.dir(id).join("store"))
        .map_err(|_| Failure::new(409, "conflict", "the session has no store yet"))?;
    view.read(|view| view.head().map(|(head, _, _)| head))
        .map_err(internal)
}

/// `GET /v1/sessions/:id`: where a thread stands.
fn session(host: &Host, id: &str) -> std::result::Result<Value, Failure> {
    let (busy, stopping, since) = {
        let live = host.live(id).map_err(missing)?;
        let live = live.lock();
        (live.busy, live.stopping, live.since)
    };
    let name: Option<String> = lock(&host.db)
        .query_row("SELECT name FROM sessions WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .optional()
        .map_err(internal)?;
    let name = name.ok_or_else(|| missing("session not found"))?;
    let dir = host.dir(id);
    let mut value = match api::head_state(&dir) {
        Ok(state) => state,
        // Being created, or the host stopped before its store existed.
        Err(_) if !dir.join("store").join("authority.sqlite").exists() => {
            json!({"head": 0, "phase": if busy { "starting" } else { "missing" }, "stop": null,
                "stop_text": null, "complete": false, "accepts_prompt": false, "prompts": 0,
                "files": 0, "recorded": false, "rejection": null,
                "wake": {"work": "none", "why": null}})
        }
        Err(error) => return Err(internal(error)),
    };
    value["id"] = json!(id);
    value["name"] = json!(name);
    value["busy"] = json!(busy);
    // While the host runs it, nothing was left unfinished by a stopped owner.
    if busy {
        value["wake"] = json!({"work": "running", "why": null});
    }
    value["stopping"] = json!(busy && stopping);
    value["since"] = if busy { json!(since) } else { Value::Null };
    value["settings"] = Settings::load(&dir)
        .map(|settings| api::settings_json(&settings))
        .unwrap_or(Value::Null);
    value["usage"] = agent::usage(&dir)
        .map(|usage| usage.to_json())
        .unwrap_or(Value::Null);
    value["pending_settings"] = agent::pending_settings(&dir).unwrap_or(Value::Null);
    let live = host.live(id).map_err(missing)?;
    // A call waiting for the app's answer, so a client that connects late can answer it.
    let pending: Vec<Value> = live
        .lock()
        .app_tool
        .values()
        .map(|pending| pending.event.clone())
        .collect();
    value["app_tool"] = pending.first().cloned().unwrap_or(Value::Null);
    value["app_tools"] = json!(pending);
    value["self_edit"] = match value["head"].as_u64() {
        Some(head) if head > 0 => {
            super::selfedit::snapshot(host, id, head, &live).map_err(internal)?
        }
        _ => Value::Null,
    };
    Ok(value)
}

/// `POST /v1/sessions/:id/steer`: a message for the running turn; `now` cuts the
/// reply being written so the waiting messages are read first (alone, it asks that of
/// the messages already waiting).
fn steer(host: &Host, id: &str, body: &[u8]) -> std::result::Result<usize, Failure> {
    let data: Value = serde_json::from_slice(body).map_err(|e| bad(err(e)))?;
    let now = data["now"].as_bool() == Some(true);
    let text = match data["text"].as_str() {
        None if now => None,
        text => Some(
            text.filter(|text| !text.trim().is_empty() && text.len() <= 16 * 1024)
                .ok_or_else(|| bad("text must be 1–16384 bytes"))?,
        ),
    };
    host.live(id)
        .map_err(missing)?
        .steer(text, now)
        .map_err(|(status, code, text)| match code {
            "not_running" => {
                Failure::new(409, code, "no turn is running; send it as a `say` command")
            }
            _ => Failure::new(status, code, text),
        })
}

/// JSON with object keys sorted, so a body's digest does not depend on key order.
fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|key| format!("{}:{}", Value::from(key.as_str()), canonical(&map[key])))
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", items.join(","))
        }
        other => other.to_string(),
    }
}

/// The fields each command kind takes besides `request_id` and `kind`.
fn fields(kind: &str) -> &'static [&'static str] {
    match kind {
        "new" => &[
            "text",
            "name",
            "provider",
            "model",
            "network",
            "toolchain",
            "effort",
            "compaction",
            "max_usd",
            "tools",
            "builtin_tools",
            "instructions",
            "resend_unknown",
            "tasks",
            "state",
        ],
        "task_start" => &[
            "session",
            "task",
            "prompt",
            "settings",
            "model",
            "effort",
            "files",
            "report",
            "expected_head",
        ],
        "say" => &["session", "text", "attachments", "state", "expected_head"],
        "state" => &["session", "patch", "expected_head"],
        "continue" => &["session", "expected_head", "until"],
        "retry" => &["session", "expected_head"],
        "rewind" => &["session", "revision", "expected_head"],
        "rename" => &["session", "name"],
        "settings" => &[
            "session",
            "provider",
            "model",
            "network",
            "toolchain",
            "effort",
            "compaction",
            "max_usd",
            "tools",
            "builtin_tools",
            "instructions",
            "resend_unknown",
            "tasks",
            "expected_head",
        ],
        _ => &["session"],
    }
}

fn task_key(dir: &std::path::Path, task: &str) -> std::result::Result<String, Failure> {
    agent::tasks::key(dir, task).map_err(missing)
}

/// `POST /v1/commands`: admits a command (202) or returns the receipt of an already
/// admitted request ID (200). The host checks `expected_head` under its own locks;
/// without one, the head read here is what the command is admitted against.
fn command(host: &Arc<Host>, body: &[u8]) -> std::result::Result<(u16, Value), Failure> {
    let body: Value = serde_json::from_slice(body).map_err(|e| bad(format!("body: {e}")))?;
    let Value::Object(fields_given) = &body else {
        return Err(bad("the body is a JSON object"));
    };
    let request_id = body["request_id"]
        .as_str()
        .filter(|id| id_ok(id))
        .ok_or_else(|| bad("request_id must be 1–80 letters, digits, `-` or `_`"))?;
    let kind = body["kind"]
        .as_str()
        .filter(|kind| KINDS.contains(kind))
        .ok_or_else(|| bad(format!("kind must be one of {}", KINDS.join(", "))))?;
    let allowed = fields(kind);
    if let Some(unknown) = fields_given.keys().find(|key| {
        !matches!(key.as_str(), "request_id" | "kind") && !allowed.contains(&key.as_str())
    }) {
        return Err(bad(format!("`{unknown}` does not apply to `{kind}`")));
    }
    if kind == "continue" && !body["until"].is_null() && body["until"] != "unknown" {
        return Err(bad("until must be unknown"));
    }
    let expected = match body.get("expected_head") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .ok_or_else(|| bad("expected_head must be a revision number"))?,
        ),
    };
    let digest = hash(canonical(&body).as_bytes());
    // An admitted request ID answers with its receipt, whatever the thread did since.
    if let Some(found) = admitted(host, request_id, &digest)? {
        return Ok((200, found));
    }
    let session = if kind == "new" {
        String::new()
    } else {
        let session = body["session"]
            .as_str()
            .ok_or_else(|| bad("session required"))?;
        if !id_ok(session) || host.live(session).is_err() {
            return Err(missing("session not found"));
        }
        session.to_string()
    };
    let mut data = body.clone();
    if let Some(data) = data.as_object_mut() {
        data.remove("expected_head");
    }
    data["api"] = json!("v1");
    data["v1"] = json!(digest);
    if kind == "new" && body.get("name").is_none_or(Value::is_null) {
        let text = body["text"].as_str().unwrap_or_default();
        let name: String = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("API session")
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(60)
            .collect();
        data["name"] = json!(name);
    }
    if kind == "new" && body["provider"].as_str().is_none_or(str::is_empty) {
        return Err(bad("provider required"));
    }
    // Without expected_head the current head is used; if the thread moves between
    // reading it and admission (nothing was admitted), read it again.
    let mut attempt = 0;
    loop {
        if HEADED.contains(&kind) {
            let now = head(host, &session)?;
            if let Some(expected) = expected
                && expected != now
            {
                return Err(Failure::new(
                    409,
                    "head_moved",
                    format!("the session's head is {now}, not {expected}"),
                )
                .with("head", json!(now)));
            }
            data["expected_revision"] = json!(expected.unwrap_or(now));
        }
        match host.submit(&session, data.clone()) {
            Ok(_) => {
                let receipt = receipt(host, request_id)?
                    .ok_or_else(|| internal("the admitted receipt is missing"))?;
                return Ok((202, receipt));
            }
            Err(Refused(409, message)) if message == MOVED && expected.is_none() && attempt < 2 => {
                attempt += 1;
            }
            Err(refused) => {
                let failure = classify(refused, true);
                let failure = if failure.code == "head_moved" {
                    let now = head(host, &session).ok();
                    failure.with("head", json!(now))
                } else {
                    failure
                };
                return Err(failure);
            }
        }
    }
}

/// The receipt of `request_id` if it was admitted with this body's digest; a request
/// ID admitted with another body (or by `/api`) is refused as reused.
fn admitted(
    host: &Host,
    request_id: &str,
    digest: &str,
) -> std::result::Result<Option<Value>, Failure> {
    let stored: Option<Option<String>> = lock(&host.db)
        .query_row(
            "SELECT s.payload FROM commands c LEFT JOIN submissions s ON s.id=c.id WHERE c.id=?1",
            [request_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| classify(Refused(500, err(error)), true))?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    let same = stored
        .and_then(|payload| serde_json::from_str::<Value>(&payload).ok())
        .is_some_and(|payload| payload["v1"] == digest);
    if !same {
        return Err(Failure::new(409, "request_id_reused", REUSED));
    }
    receipt(host, request_id)
}

/// `GET /v1/receipts/:request_id`: what became of an admitted command.
fn receipt(host: &Host, request_id: &str) -> std::result::Result<Option<Value>, Failure> {
    type Row = (String, String, Option<String>, Option<String>, Option<i64>);
    let row: Option<Row> = lock(&host.db)
        .query_row(
            "SELECT c.session,c.state,c.error,s.payload,t.at FROM commands c LEFT JOIN submissions s ON s.id=c.id LEFT JOIN command_times t ON t.id=c.id WHERE c.id=?1",
            [request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()
        .map_err(internal)?;
    Ok(row.map(|(session, state, error, payload, at)| {
        let kind = payload
            .and_then(|payload| serde_json::from_str::<Value>(&payload).ok())
            .and_then(|payload| payload["kind"].as_str().map(String::from));
        json!({
            "request_id": request_id,
            "session": session,
            "kind": kind,
            "state": state,
            "error": error,
            "admitted_at": at,
        })
    }))
}

/// `GET /v1/sessions/:id/events`: Server-Sent Events. Every frame has `id: BOOT:SEQ`;
/// a reconnect with `Last-Event-ID` (or `?after=`) resumes right after it while the
/// ring still holds the next event. Otherwise the stream starts with a `snapshot`
/// (the session object), labelled with the ring position read before it was built.
/// A deleted session's stream ends with `deleted`, after the events still due.
fn events(
    host: &Arc<Host>,
    stream: &mut TcpStream,
    id: &str,
    request: &http::Request,
    extra: &str,
    device: &str,
) -> Answer {
    let live = host.live(id).map_err(missing)?;
    http::headers(
        stream,
        200,
        "text/event-stream",
        &format!("X-Accel-Buffering: no\r\n{extra}"),
        None,
    )
    .map_err(internal)?;
    // From here every outcome is written into the stream; nothing returns an error.
    let previous = request
        .headers
        .get("last-event-id")
        .or_else(|| request.query.get("after"));
    let mut cursor = previous
        .and_then(|text| text.split_once(':'))
        .filter(|(boot, _)| *boot == host.boot)
        .and_then(|(_, seq)| seq.parse::<u64>().ok());
    let started = Instant::now();
    let mut heartbeat = Instant::now();
    while started.elapsed() < Duration::from_secs(300) {
        let (seq, first, batch, deleted) = {
            let live = live.wait_api(cursor, heartbeat + super::HEARTBEAT);
            let batch: Vec<(u64, String)> = live
                .api
                .events
                .iter()
                .filter(|(n, _)| cursor.is_some_and(|c| *n > c))
                .cloned()
                .collect();
            let first = live.api.events.front().map_or(live.api.seq + 1, |e| e.0);
            (live.api.seq, first, batch, live.deleted)
        };
        let mut out = String::new();
        let resumable = cursor.is_some_and(|c| c <= seq && c.saturating_add(1) >= first);
        if !deleted && !resumable {
            // Built again if events arrived meanwhile, so none is both reflected in the
            // snapshot and sent after it. If they keep arriving, the third is labelled
            // with the position read before it: an event that arrived while it was
            // built may then be both, but none is ever skipped.
            let mut attempts = 0;
            let (snapshot, at) = loop {
                let before = live.lock().api.seq;
                let snapshot = session(host, id);
                attempts += 1;
                if snapshot.is_err() || live.lock().api.seq == before || attempts == 3 {
                    break (snapshot, before);
                }
            };
            match snapshot {
                Ok(data) => {
                    out.push_str(&format!(
                        "id: {}:{at}\nevent: snapshot\ndata: {data}\n\n",
                        host.boot
                    ));
                    cursor = Some(at);
                }
                Err(failure) => {
                    let _ = write!(
                        stream,
                        "event: failure\ndata: {}\n\n",
                        json!({"error": failure.message, "code": failure.code})
                    );
                    return Ok(());
                }
            }
        } else {
            // Events of the same wake as a deletion still go out first (a cursor outside
            // the window gets no partial batch, only the deletion).
            let batch = if resumable { batch } else { vec![] };
            for (n, text) in batch {
                out.push_str(&format!(
                    "id: {}:{n}\nevent: event\ndata: {text}\n\n",
                    host.boot
                ));
                cursor = Some(n);
            }
            if deleted {
                out.push_str("event: deleted\ndata: {}\n\n");
            }
        }
        if heartbeat.elapsed() >= super::HEARTBEAT {
            // A revoked device or token loses its open stream too.
            if !host.paired(device) {
                return Ok(());
            }
            out.push_str(": heartbeat\n\n");
            heartbeat = Instant::now();
        }
        if !out.is_empty() && (stream.write_all(out.as_bytes()).is_err() || stream.flush().is_err())
        {
            return Ok(());
        }
        if deleted {
            return Ok(());
        }
    }
    Ok(())
}

/// `spindle --dir HOST token new NAME | list | revoke NAME`: named API tokens for the
/// host's HTTP API. A token is a device row (its SHA-256 only) with id `api:NAME`; it
/// is printed once, works as `Authorization: Bearer TOKEN`, is never evicted by new
/// pairings, and shows (and can be revoked) under Settings > Host like any screen.
pub fn token(root: PathBuf, args: Vec<String>) -> Result<()> {
    const USAGE: &str = "usage: spindle --dir HOST token new NAME | list | revoke NAME";
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let name_ok = |name: &str| {
        !name.is_empty()
            && name.len() <= 40
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    if !root.join("host.sqlite").is_file() {
        if matches!(args.first(), Some(&"new")) {
            // Creates the host directory as `serve` would (it must not be running).
            drop(Host::open(root.clone(), None, None)?);
        } else {
            return Err(format!("no spindle host in {}", root.display()));
        }
    }
    let db = Connection::open(root.join("host.sqlite")).map_err(err)?;
    db.busy_timeout(Duration::from_secs(5)).map_err(err)?;
    match args.as_slice() {
        ["new", name] if name_ok(name) => {
            let count: i64 = db
                .query_row(
                    "SELECT count(*) FROM devices WHERE id LIKE 'api:%'",
                    [],
                    |row| row.get(0),
                )
                .map_err(err)?;
            if count >= MAX_TOKENS {
                return Err(format!("at most {MAX_TOKENS} API tokens; revoke one first"));
            }
            let token = random()?;
            let now = now_ms() as i64;
            let added = db.execute(
                "INSERT OR IGNORE INTO devices VALUES(?1,?2,?3,?4,?4,?4)",
                params![
                    format!("{TOKEN_PREFIX}{name}"),
                    hash(token.as_bytes()),
                    format!("API token {name}"),
                    now
                ],
            );
            if added.map_err(err)? != 1 {
                return Err(format!("a token named `{name}` exists; revoke it first"));
            }
            println!("{token}");
            eprintln!(
                "API token `{name}` created. It is shown only this once; send it as `Authorization: Bearer TOKEN`."
            );
            Ok(())
        }
        ["list"] => {
            let mut statement = db
                .prepare("SELECT id,created,seen FROM devices WHERE id LIKE 'api:%' ORDER BY id")
                .map_err(err)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(err)?;
            let since = |ms: i64| {
                let ago = now_ms().saturating_sub(ms.max(0) as u64) / 1000;
                match ago {
                    0..60 => format!("{ago}s ago"),
                    60..3600 => format!("{}m ago", ago / 60),
                    3600..86400 => format!("{}h ago", ago / 3600),
                    _ => format!("{}d ago", ago / 86400),
                }
            };
            for row in rows {
                let (id, created, seen) = row.map_err(err)?;
                println!(
                    "{:<40} created {:<10} last used {}",
                    &id[TOKEN_PREFIX.len()..],
                    since(created),
                    since(seen)
                );
            }
            Ok(())
        }
        ["revoke", name] => {
            let removed = db
                .execute(
                    "DELETE FROM devices WHERE id=?1",
                    [format!("{TOKEN_PREFIX}{name}")],
                )
                .map_err(err)?;
            if removed == 1 {
                println!("Revoked API token `{name}`.");
                Ok(())
            } else {
                Err(format!("no API token named `{name}`"))
            }
        }
        ["new", _] => Err("a token name is 1–40 letters, digits, `-`, `_` or `.`".into()),
        _ => Err(USAGE.into()),
    }
}

// The host these tests open runs only on Unix.
#[cfg(all(test, unix))]
mod tests {
    use super::super::Watch;
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn temp_host_for_tests(name: &str) -> (PathBuf, Arc<Host>) {
        let root = std::env::temp_dir().join(format!("spindle-host-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let host = Arc::new(Host::open(root.clone(), None, None).unwrap());
        (root, host)
    }
    /// A registered thread `s` with an empty store (head 0).
    fn thread(host: &Host) -> Arc<Watch> {
        lock(&host.db)
            .execute("INSERT INTO sessions VALUES('s','thread',0)", [])
            .unwrap();
        let watch = Watch::new();
        lock(&host.lives).insert("s".into(), watch.clone());
        std::fs::create_dir_all(host.dir("s")).unwrap();
        drop(spindle_store::Store::open(&host.dir("s").join("store")).unwrap());
        watch
    }

    /// One request through `handle`, as a client on a loopback socket sends it.
    fn call(host: &Arc<Host>, raw: &str) -> (u16, String, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server_host = host.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            super::super::handle(&server_host, &mut stream);
        });
        let mut client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        client
            .write_all(raw.replace("HOST", &addr.to_string()).as_bytes())
            .unwrap();
        let mut bytes = Vec::new();
        let _ = client.read_to_end(&mut bytes);
        server.join().unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = head[9..12].parse().unwrap();
        (status, head.to_string(), body.to_string())
    }
    fn get(host: &Arc<Host>, path: &str, token: &str) -> (u16, Value) {
        let (status, _, body) = call(
            host,
            &format!("GET {path} HTTP/1.1\r\nHost: HOST\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }
    fn post(host: &Arc<Host>, path: &str, token: &str, body: &Value) -> (u16, Value) {
        let body = body.to_string();
        let (status, _, reply) = call(
            host,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: HOST\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        );
        (status, serde_json::from_str(&reply).unwrap_or(Value::Null))
    }

    #[cfg(unix)]
    #[test]
    fn the_remote_runner_runs_a_command_over_a_sent_workspace() {
        let (root, host) = temp_host_for_tests("v1-runner");
        let (_, device) = host.pair("Linux · Firefox").unwrap();
        let (status, identity) = get(&host, "/v1/runner", &device);
        if status == 503 {
            // No sandbox on this host: remote runs are refused, never run unisolated.
            assert_eq!(identity["code"], "unavailable", "{identity}");
            let (status, _) = post(&host, "/v1/run", &device, &json!({"command": "true"}));
            assert_eq!(status, 503);
            let _ = std::fs::remove_dir_all(root);
            return;
        }
        assert_eq!(status, 200, "{identity}");
        assert_eq!(identity["helper"].as_str().unwrap().len(), 64);
        assert_eq!(
            identity["isolated"], true,
            "only the sandbox runs remote commands"
        );
        let (status, result) = post(
            &host,
            "/v1/run",
            &device,
            &json!({"command": "cp a b && rm a", "timeout_s": 30,
                "files": [{"path": "a", "base64": api::base64_encode(b"x"), "executable": false}]}),
        );
        assert_eq!(status, 200, "{result}");
        assert_eq!(result["success"], true, "{result}");
        assert_eq!(result["writes"][0]["path"], "b");
        assert_eq!(result["deletes"], json!(["a"]));
        assert_eq!(result["helper"], identity["helper"]);
        let (status, refused) = post(&host, "/v1/run", &device, &json!({"command": ""}));
        assert_eq!(
            (status, refused["code"].clone()),
            (400, json!("bad_request"))
        );
        let (status, _) = post(&host, "/v1/run", "nope", &json!({"command": "true"}));
        assert_eq!(status, 401, "a run needs a token");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bearer_tokens_authenticate_without_origin_and_errors_are_enveloped() {
        let (root, host) = temp_host_for_tests("v1-auth");
        let (_, device) = host.pair("Linux · Firefox").unwrap();
        let (status, info) = get(&host, "/v1", &device);
        assert_eq!(status, 200);
        assert_eq!(info["api_version"], api::API_VERSION);
        assert_eq!(info["auth"], "bearer");
        assert!(
            info["features"]
                .as_array()
                .unwrap()
                .contains(&json!("expected_head"))
        );
        // Neither nothing, a wrong token nor the pairing key itself gets in.
        for (header, why) in [
            ("", "no credentials"),
            ("Authorization: Bearer nope\r\n", "unknown token"),
            ("Authorization: Basic abc\r\n", "wrong scheme"),
            (
                &*format!("Authorization: Bearer {}\r\n", host.token),
                "pairing key",
            ),
        ] {
            let (status, _, body) = call(
                &host,
                &format!("GET /v1/sessions HTTP/1.1\r\nHost: HOST\r\n{header}\r\n"),
            );
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(
                (status, body["code"].clone()),
                (401, json!("unauthorized")),
                "{why}"
            );
            assert!(body["error"].is_string());
        }
        // Nor as a cookie: v1 mints no device for it (`/api` still migrates it).
        let devices = || -> i64 {
            lock(&host.db)
                .query_row("SELECT count(*) FROM devices", [], |r| r.get(0))
                .unwrap()
        };
        let before = devices();
        let (status, head, body) = call(
            &host,
            &format!(
                "GET /v1/sessions HTTP/1.1\r\nHost: HOST\r\nCookie: spindle={}\r\n\r\n",
                host.token
            ),
        );
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!((status, body["code"].clone()), (401, json!("unauthorized")));
        assert!(!head.to_ascii_lowercase().contains("set-cookie"), "{head}");
        assert_eq!(devices(), before);
        let (status, _, _) = call(
            &host,
            &format!("GET /v1 HTTP/1.1\r\nHost: HOST\r\nCookie: spindle={device}\r\n\r\n"),
        );
        assert_eq!(status, 200, "a paired cookie still works");
        // A cookie-authenticated POST still needs the same-origin proof; a bearer does not.
        let (status, _, body) = call(
            &host,
            &format!(
                "POST /v1/commands HTTP/1.1\r\nHost: HOST\r\nCookie: spindle={device}\r\nContent-Length: 2\r\n\r\n{{}}"
            ),
        );
        assert_eq!(status, 403, "{body}");
        let (status, body) = post(&host, "/v1/commands", &device, &json!({}));
        assert_eq!((status, body["code"].clone()), (400, json!("bad_request")));
        let (status, body) = get(&host, "/v1/nowhere", &device);
        assert_eq!((status, body["code"].clone()), (404, json!("not_found")));
        let (status, body) = post(&host, "/v1/sessions", &device, &json!({}));
        assert_eq!(
            (status, body["code"].clone()),
            (405, json!("method_not_allowed"))
        );
        let (status, body) = get(&host, "/v1/sessions/nope/messages", &device);
        assert_eq!((status, body["code"].clone()), (404, json!("not_found")));
        // The /api routes are untouched: still cookie-only, legacy envelope.
        let (status, body) = get(&host, "/api/sessions", &device);
        assert_eq!(
            (status, body["error"].clone()),
            (401, json!("Pair this device first"))
        );
        drop(host);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn named_tokens_are_minted_listed_kept_through_pairings_and_revoked() {
        let (root, host) = temp_host_for_tests("v1-tokens");
        token(root.clone(), vec!["new".into(), "ci".into()]).unwrap();
        assert!(token(root.clone(), vec!["new".into(), "ci".into()]).is_err());
        assert!(token(root.clone(), vec!["new".into(), "bad name".into()]).is_err());
        let stored: String = lock(&host.db)
            .query_row("SELECT hash FROM devices WHERE id='api:ci'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored.len(), 64);
        // Browser pairings beyond the device cap evict screens, never API tokens.
        for n in 0..40 {
            host.pair(&format!("screen {n}")).unwrap();
        }
        assert!(host.paired("api:ci"));
        token(root.clone(), vec!["list".into()]).unwrap();
        token(root.clone(), vec!["revoke".into(), "ci".into()]).unwrap();
        assert!(!host.paired("api:ci"));
        assert!(token(root.clone(), vec!["revoke".into(), "ci".into()]).is_err());
        drop(host);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn commands_are_idempotent_head_checked_and_receipted() {
        let (root, host) = temp_host_for_tests("v1-commands");
        let (_, device) = host.pair("t").unwrap();
        thread(&host);
        let rename =
            json!({"request_id": "r1", "kind": "rename", "session": "s", "name": "renamed"});
        let (status, receipt) = post(&host, "/v1/commands", &device, &rename);
        assert_eq!(status, 202, "{receipt}");
        assert_eq!(receipt["state"], "completed");
        assert_eq!(receipt["kind"], "rename");
        // The same request again: its receipt, now 200; key order does not matter.
        let reordered =
            json!({"session": "s", "name": "renamed", "kind": "rename", "request_id": "r1"});
        let (status, again) = post(&host, "/v1/commands", &device, &reordered);
        assert_eq!((status, again["request_id"].clone()), (200, json!("r1")));
        let changed =
            json!({"request_id": "r1", "kind": "rename", "session": "s", "name": "other"});
        let (status, body) = post(&host, "/v1/commands", &device, &changed);
        assert_eq!(
            (status, body["code"].clone()),
            (409, json!("request_id_reused"))
        );
        let (status, body) = get(&host, "/v1/receipts/r1", &device);
        assert_eq!((status, body["state"].clone()), (200, json!("completed")));
        let (status, body) = get(&host, "/v1/receipts/never", &device);
        assert_eq!((status, body["code"].clone()), (404, json!("not_found")));
        // A stale expected_head is refused before anything is admitted.
        let stale =
            json!({"request_id": "c1", "kind": "continue", "session": "s", "expected_head": 5});
        let (status, body) = post(&host, "/v1/commands", &device, &stale);
        assert_eq!(
            (status, body["code"].clone(), body["head"].clone()),
            (409, json!("head_moved"), json!(0))
        );
        assert!(
            get(&host, "/v1/receipts/c1", &device).0 == 404,
            "nothing admitted"
        );
        let wrong = json!({"request_id": "c2", "kind": "stop", "session": "s", "expected_head": 0});
        assert_eq!(post(&host, "/v1/commands", &device, &wrong).0, 400);
        let unknown =
            json!({"request_id": "c3", "kind": "say", "session": "s", "text": "x", "dev": true});
        assert_eq!(post(&host, "/v1/commands", &device, &unknown).0, 400);
        let stop = json!({"request_id": "c4", "kind": "stop", "session": "s"});
        let (status, body) = post(&host, "/v1/commands", &device, &stop);
        assert_eq!((status, body["code"].clone()), (409, json!("not_running")));
        host.live("s").unwrap().lock().busy = true;
        let say = json!({"request_id": "c5", "kind": "say", "session": "s", "text": "hi"});
        let (status, body) = post(&host, "/v1/commands", &device, &say);
        assert_eq!((status, body["code"].clone()), (409, json!("busy")));
        // Steering only reaches a running turn's queue.
        let (status, body) = post(
            &host,
            "/v1/sessions/s/steer",
            &device,
            &json!({"text": "x"}),
        );
        assert_eq!((status, body["code"].clone()), (409, json!("not_running")));
        let queue = Arc::new(std::sync::Mutex::new(vec![]));
        host.live("s").unwrap().lock().steering = Some(queue.clone());
        let (status, body) = post(
            &host,
            "/v1/sessions/s/steer",
            &device,
            &json!({"text": "x"}),
        );
        assert_eq!((status, body["queued"].clone()), (202, json!(1)));
        assert_eq!(*queue.lock().unwrap(), vec!["x".to_string()]);
        // `now` alone asks the waiting messages to be read at once.
        let hurry = Arc::new(AtomicBool::new(false));
        host.live("s").unwrap().lock().hurry = Some(hurry.clone());
        let (status, body) = post(
            &host,
            "/v1/sessions/s/steer",
            &device,
            &json!({"now": true}),
        );
        assert_eq!((status, body["queued"].clone()), (202, json!(1)));
        assert!(hurry.load(Ordering::SeqCst));
        assert_eq!(
            host.live("s").unwrap().lock().queued(),
            json!({"messages": ["x"], "now": true})
        );
        queue.lock().unwrap().clear();
        let (status, body) = post(
            &host,
            "/v1/sessions/s/steer",
            &device,
            &json!({"now": true}),
        );
        assert_eq!(
            (status, body["code"].clone()),
            (409, json!("nothing_waiting"))
        );
        host.live("s").unwrap().lock().busy = false;
        // The session object and its committed state read without the owner lock.
        let (status, body) = get(&host, "/v1/sessions/s", &device);
        assert_eq!(
            (status, body["head"].clone(), body["phase"].clone()),
            (200, json!(0), json!("empty"))
        );
        assert_eq!(body["name"], "renamed");
        let (status, body) = get(&host, "/v1/sessions/s/messages", &device);
        assert_eq!((status, body["messages"].clone()), (200, json!([])));
        let (status, body) = get(&host, "/v1/sessions/s/files?revision=7", &device);
        assert_eq!((status, body["code"].clone()), (404, json!("not_found")));
        let (status, body) = get(&host, "/v1/sessions", &device);
        assert_eq!(
            (status, body["sessions"][0]["id"].clone()),
            (200, json!("s"))
        );
        drop(host);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn event_stream_resumes_after_its_cursor_without_gaps_or_repeats() {
        let (root, host) = temp_host_for_tests("v1-events");
        let (_, device) = host.pair("t").unwrap();
        let watch = thread(&host);
        for n in 1..=3 {
            watch.push_api(json!({"type": "model_delta", "text": n.to_string()}));
        }
        // Deleting ends every stream after what it has sent, so the reads terminate.
        let read = |cursor: &str| {
            let deleter = watch.clone();
            let ender = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                deleter.lock().deleted = true;
                deleter.changed.notify_all();
            });
            let (_, _, body) = call(
                &host,
                &format!(
                    "GET /v1/sessions/s/events HTTP/1.1\r\nHost: HOST\r\nAuthorization: Bearer {device}\r\n{cursor}\r\n"
                ),
            );
            ender.join().unwrap();
            watch.lock().deleted = false;
            body
        };
        let fresh = read("");
        assert!(
            fresh.starts_with(&format!("id: {}:3\nevent: snapshot\n", host.boot)),
            "{fresh}"
        );
        assert!(
            !fresh.contains("event: event"),
            "a snapshot is not followed by older events"
        );
        let resumed = read(&format!("Last-Event-ID: {}:1\r\n", host.boot));
        let ids: Vec<&str> = resumed
            .lines()
            .filter_map(|line| line.strip_prefix("id: "))
            .collect();
        assert_eq!(
            ids,
            [format!("{}:2", host.boot), format!("{}:3", host.boot)]
        );
        let second = "event: event\ndata: {\"text\":\"2\",\"type\":\"model_delta\"}";
        assert!(resumed.contains(second), "{resumed}");
        assert!(!resumed.contains("event: snapshot"));
        // Another boot's cursor (or an evicted one) starts over with a snapshot.
        assert!(read("Last-Event-ID: elsewhere:2\r\n").contains("event: snapshot"));
        // Command ends reach the stable ring as command_done.
        watch
            .lock()
            .api
            .done(&json!({"type": "done", "request_id": "r", "state": "completed"}));
        let done = read(&format!("Last-Event-ID: {}:3\r\n", host.boot));
        assert!(done.contains("command_done"), "{done}");
        drop(host);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_command_a_budget_refused_ends_with_the_code_budget() {
        let mut ring = Ring::default();
        let refused = format!(
            "{} the next model request could cost more",
            spindle_store::protocol::BUDGET_REFUSAL
        );
        ring.done(&json!({"type": "done", "state": "failed", "error": refused}));
        ring.done(
            &json!({"type": "done", "state": "failed", "error": "the provider returned HTTP 500"}),
        );
        let ended: Vec<Value> = ring
            .events()
            .map(|text| serde_json::from_str(text).unwrap())
            .collect();
        assert_eq!(ended[0]["type"], "command_done");
        assert_eq!(ended[0]["code"], "budget");
        assert!(ended[1].get("code").is_none(), "{}", ended[1]);
    }

    #[test]
    fn a_deletion_sends_its_wake_first_and_a_failed_create_ends_its_streams() {
        let (root, host) = temp_host_for_tests("v1-deleted");
        let (_, device) = host.pair("t").unwrap();
        let watch = thread(&host);
        watch.push_api(json!({"type": "model_delta", "text": "1"}));
        // The last event and the deletion arrive in one wake.
        let deleter = watch.clone();
        let ender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let mut live = deleter.lock();
            live.api
                .push(&json!({"type": "command_done", "state": "completed"}));
            live.deleted = true;
            drop(live);
            deleter.changed.notify_all();
        });
        let (_, _, body) = call(
            &host,
            &format!(
                "GET /v1/sessions/s/events HTTP/1.1\r\nHost: HOST\r\nAuthorization: Bearer {device}\r\nLast-Event-ID: {}:1\r\n\r\n",
                host.boot
            ),
        );
        ender.join().unwrap();
        let at = body
            .find(&format!("id: {}:2\nevent: event\n", host.boot))
            .unwrap_or_else(|| panic!("the wake's event was dropped: {body}"));
        assert!(body[at..].contains("event: deleted"), "{body}");
        // A create that fails leaves no thread; its open streams end as for a deletion.
        let failing = Watch::new();
        lock(&host.db)
            .execute("INSERT INTO sessions VALUES('f','failing',0)", [])
            .unwrap();
        lock(&host.lives).insert("f".into(), failing.clone());
        let data = json!({"kind": "new", "provider": "no-such-provider", "text": "hi"});
        host.clone().work("f", "rf", data, failing.clone());
        assert!(host.live("f").is_err());
        let live = failing.lock();
        assert!(live.deleted, "streams of a failed create would linger");
        let last = live.api.events.back().map(|(_, text)| text.clone());
        assert!(last.is_some_and(|text| text.contains("command_done")));
        drop(live);
        drop(host);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn refusal_messages_keep_their_v1_codes() {
        let (root, host) = temp_host_for_tests("v1-codes");
        thread(&host);
        let code = |refused| classify(refused, true).code;
        let stop = host.submit("s", json!({"request_id": "a", "kind": "stop"}));
        assert_eq!(code(stop.unwrap_err()), "not_running");
        let moved = host.submit(
            "s",
            json!({"request_id": "m", "kind": "continue", "expected_revision": 5}),
        );
        assert_eq!(code(moved.unwrap_err()), "head_moved");
        host.live("s").unwrap().lock().busy = true;
        let busy = host.submit("s", json!({"request_id": "b", "kind": "continue"}));
        assert_eq!(code(busy.unwrap_err()), "busy");
        host.submit("s", json!({"request_id": "c", "kind": "stop"}))
            .unwrap();
        let reused = host.submit("s", json!({"request_id": "c", "kind": "retry"}));
        assert_eq!(code(reused.unwrap_err()), "request_id_reused");
        assert_eq!(code(Refused(500, "x".into())), "outcome_unknown");
        assert_eq!(classify(Refused(500, "x".into()), false).code, "internal");
        drop(host);
        let _ = std::fs::remove_dir_all(root);
    }
}
