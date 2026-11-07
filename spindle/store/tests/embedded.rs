//! The embedded host against a scripted model. Each scenario runs
//! twice, through the native session against a loopback fake provider and through
//! the embedded host fed the same response bytes, and both must send byte-identical
//! requests and report the same events (native-only `cost` aside; the embedded
//! transport's own `app_tool`/`app_tool_done` aside). Then embedded-only behavior:
//! the recorded native trace in file-tools mode, reopening across a lost response,
//! runners, tasks, stop and steering, and the API methods.
#![cfg(all(feature = "embedded", feature = "cli", target_os = "linux"))]
use serde_json::{Value, json};
use spindle_store::agent::{
    self, AppAnswer, AppCall, AppTools, Event, Observer, Session, Settings,
};
use spindle_store::embedded::{Catalog, Embedded, MemoryStorage, Places, Storage, answer_of};
use spindle_store::{Error, Store};
use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};

/// What the model answers, request by request.
#[derive(Clone)]
enum Reply {
    Text(&'static str),
    Call(&'static str, Value),
    /// HTTP 429 with `Retry-After: 0`.
    Refused,
    /// The connection closes with no reply (an unknown outcome).
    HangUp,
    /// The reply begins, then the provider streams an error with this message.
    StreamError(&'static str),
}

fn chunks(delta: Value, index: usize) -> String {
    let delta = match delta.get("tool_calls") {
        Some(_) => {
            let mut delta = delta;
            delta["tool_calls"][0]["id"] = json!(format!("call_{index}"));
            delta
        }
        None => delta,
    };
    let finish = if delta.get("tool_calls").is_some() {
        "tool_calls"
    } else {
        "stop"
    };
    let chunks = [
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":finish}]}),
        json!({"id":"c","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":100,"total_tokens":1100,"prompt_tokens_details":{"cached_tokens":200}}}),
    ];
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    body
}

/// The response body (or refusal) a reply is, as (status, retry-after, body).
fn wire(reply: &Reply, index: usize) -> Option<(u16, Option<&'static str>, String)> {
    match reply {
        Reply::Text(text) => Some((200, None, chunks(json!({"content": text}), index))),
        Reply::Call(name, arguments) => Some((
            200,
            None,
            chunks(
                json!({"tool_calls": [{"index": 0, "id": "", "type": "function",
                    "function": {"name": name, "arguments": arguments.to_string()}}]}),
                index,
            ),
        )),
        Reply::Refused => Some((
            429,
            Some("0"),
            r#"{"error":{"message":"rate limited","type":"rate_limit"}}"#.into(),
        )),
        Reply::HangUp => None,
        Reply::StreamError(message) => {
            let began = json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"hel"},"finish_reason":null}]});
            let error = json!({"error": {"code": "server_error", "message": message}});
            Some((200, None, format!("data: {began}\n\ndata: {error}\n\n")))
        }
    }
}

// ---- the native side: a loopback fake provider ----------------------------------------

type Scripts = Mutex<BTreeMap<String, (Vec<Reply>, Vec<String>)>>;

struct Fake {
    root: PathBuf,
    base: String,
    scripts: Scripts,
}

fn scenario_of(body: &Value) -> Option<String> {
    for message in body["messages"]
        .as_array()?
        .iter()
        .filter(|m| m["role"] == "user")
    {
        let text = match &message["content"] {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect(),
            _ => continue,
        };
        if let Some(start) = text.find("[t:") {
            let rest = &text[start + 3..];
            return rest.find(']').map(|end| rest[..end].to_string());
        }
    }
    None
}

fn fake() -> &'static Fake {
    static FAKE: OnceLock<&'static Fake> = OnceLock::new();
    FAKE.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("spindle-embedded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("dext");
        std::fs::create_dir_all(&home).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for dir in [&root, &home] {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let fake: &'static Fake = Box::leak(Box::new(Fake {
            root,
            base: base.clone(),
            scripts: Mutex::default(),
        }));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    let Some(body) = read_request(&mut stream) else {
                        return;
                    };
                    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let name = scenario_of(&value).unwrap_or_default();
                    let reply = {
                        let mut scripts = fake.scripts.lock().unwrap();
                        let Some((script, seen)) = scripts.get_mut(&name) else {
                            return;
                        };
                        seen.push(String::from_utf8_lossy(&body).into_owned());
                        let index = seen.len();
                        script
                            .get(index - 1)
                            .cloned()
                            .map(|reply| (reply, index))
                            .unwrap_or((Reply::Text("(script ended)"), index))
                    };
                    let response = match wire(&reply.0, reply.1) {
                        Some((200, _, body)) => format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        ),
                        Some((status, retry, body)) => format!(
                            "HTTP/1.1 {status} Refused\r\nContent-Type: application/json\r\nRetry-After: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            retry.unwrap_or("0"),
                            body.len()
                        ),
                        None => String::new(),
                    };
                    let _ = stream.write_all(response.as_bytes());
                });
            }
        });
        std::fs::write(home.join("providers.json"), serde_json::to_vec(&catalog_json(&base)).unwrap())
            .unwrap();
        // SAFETY: set once, before any test thread reads the environment.
        unsafe { std::env::set_var("DEXT_HOME", &home) };
        fake
    })
}

/// The fake provider; `small-model`'s window keeps about 37 KiB of history before a
/// turn compacts it.
fn catalog_json(base: &str) -> Value {
    json!({"version": 4, "active_provider": "fake", "providers": [{
        "id": "fake", "display_name": "Fake", "api_provider": "openai",
        "request_contract": "openai-chat-completions", "base_url": base,
        "default_model": "fake-model", "models": ["fake-model", "small-model"],
        "requires_api_key": false,
        "model_specs": {"fake-model": {"context_window": 200000, "max_output_tokens": 4096},
            "small-model": {"context_window": 20000, "max_output_tokens": 4096}}
    }]})
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Vec<u8>> {
    let mut request = vec![];
    let mut chunk = [0; 65536];
    loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        request.extend_from_slice(&chunk[..read]);
        let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())?;
        if request.len() >= end + 4 + length {
            return Some(request[end + 4..end + 4 + length].to_vec());
        }
    }
}

struct Recorder {
    events: Mutex<Vec<Value>>,
    app: Option<Arc<dyn AppTools>>,
}
impl Observer for Recorder {
    fn event(&self, event: Event<'_>) {
        self.events.lock().unwrap().push(event.to_json());
    }
    fn app_tools(&self) -> Option<Arc<dyn AppTools>> {
        self.app.clone()
    }
}

/// Answers the `lookup` tool the same way on both sides.
fn lookup_answer() -> Value {
    json!({"content": "two notes", "files": {"write": {"notes/a.md": "first"}},
        "state": {"found": 2}})
}
struct Lookup;
impl AppTools for Lookup {
    fn call(&self, _: &AppCall<'_>, _: &AtomicBool) -> AppAnswer {
        AppAnswer::from_json(&lookup_answer()).unwrap()
    }
}

fn lookup_tools() -> Value {
    json!([{"name": "lookup", "description": "Looks notes up.", "input_schema":
        {"type": "object", "properties": {"q": {"type": "string"}}}}])
}

/// Runs a scenario natively: the requests the fake received and the events.
fn native(
    name: &str,
    script: Vec<Reply>,
    prepare: impl FnOnce(&mut Settings),
    app: Option<Arc<dyn AppTools>>,
    state: Option<Value>,
    prompt: &str,
) -> (Vec<String>, Vec<Value>, agent::Result<()>) {
    let fake = fake();
    fake.scripts
        .lock()
        .unwrap()
        .insert(name.into(), (script, vec![]));
    let dir = fake.root.join(name);
    let recorder = Arc::new(Recorder {
        events: Mutex::default(),
        app,
    });
    let mut settings = Settings::resolve("fake", Some("fake-model".into())).unwrap();
    settings.isolation = agent::Isolation::Off;
    settings.builtin_tools = agent::BuiltinTools::Files;
    settings.compaction = agent::Compaction::Digest;
    prepare(&mut settings);
    let mut session = Session::create(&dir, settings, vec![], recorder.clone()).unwrap();
    if let Some(state) = state {
        session.state_with_next_prompt(state).unwrap();
    }
    let outcome = session.prompt(&format!("[t:{name}] {prompt}"));
    let requests = fake.scripts.lock().unwrap()[name].1.clone();
    let events = recorder.events.lock().unwrap().clone();
    (requests, events, outcome)
}

// ---- the embedded side ----------------------------------------------------------------

/// Memory sessions (nothing is "there" before `create`).
struct MemoryPlaces;
impl Places for MemoryPlaces {
    fn storage(&self, dir: &str) -> Result<Box<dyn Storage>, String> {
        Ok(Box::new(MemoryStorage::new(dir)))
    }
    fn exists(&self, _: &str) -> bool {
        false
    }
}

/// Sessions in directories, each store opened natively (`Store::open`): enough to
/// reopen across a dropped `Embedded` in a test.
struct DirPlaces(PathBuf);
struct DirStorage(PathBuf);
impl Places for DirPlaces {
    fn storage(&self, dir: &str) -> Result<Box<dyn Storage>, String> {
        Ok(Box::new(DirStorage(self.0.join(dir).join("store"))))
    }
    fn exists(&self, dir: &str) -> bool {
        self.0.join(dir).join("store").exists()
    }
}
impl Storage for DirStorage {
    fn label(&self) -> String {
        self.0.display().to_string()
    }
    fn open(&self, create: bool) -> spindle_store::Result<Store> {
        if create == self.0.exists() {
            return Err(Error::Incompatible);
        }
        if let Some(parent) = self.0.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Store::open(&self.0)
    }
    fn set_aside(&self, store: Store) -> spindle_store::Result<(Store, String)> {
        drop(store);
        let aside = self.0.with_extension("abandoned-1");
        std::fs::rename(&self.0, &aside)?;
        Ok((Store::open(&self.0)?, aside.display().to_string()))
    }
    fn child(&self, key: &str) -> Box<dyn Storage> {
        Box::new(DirStorage(
            self.0
                .parent()
                .unwrap()
                .join("tasks")
                .join(key)
                .join("store"),
        ))
    }
    fn tasks(&self) -> Vec<String> {
        let tasks = self.0.parent().unwrap().join("tasks");
        std::fs::read_dir(tasks)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default()
    }
    fn import_store(&self, image: &[u8]) -> spindle_store::Result<()> {
        if self.0.exists() {
            return Err(Error::Conflict);
        }
        std::fs::create_dir_all(&self.0)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700))?;
        }
        std::fs::write(self.0.join("authority.sqlite"), image)?;
        std::fs::write(self.0.join("owner.lock"), b"")?;
        Ok(())
    }
    fn size(&self) -> u64 {
        fn walk(path: &Path) -> u64 {
            match std::fs::read_dir(path) {
                Ok(entries) => entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| walk(&entry.path()))
                    .sum(),
                Err(_) => std::fs::metadata(path).map_or(0, |meta| meta.len()),
            }
        }
        walk(self.0.parent().unwrap())
    }
    /// As the package's places remove one: renamed out of `tasks/`, then removed. A
    /// `fail-removal` file in the session's directory stops it `before` the rename or
    /// `after` it (a crash there).
    fn remove_task(&self, key: &str) -> spindle_store::Result<()> {
        let dir = self.0.parent().unwrap();
        let task = dir.join("tasks").join(key);
        let fail = std::fs::read_to_string(dir.join("fail-removal")).unwrap_or_default();
        let crash = || Err(std::io::Error::other("the removal stopped here").into());
        if task.exists() {
            if fail == "before" {
                return crash();
            }
            std::fs::create_dir_all(dir.join("tasks.removing"))?;
            std::fs::rename(&task, dir.join("tasks.removing").join(key))?;
            if fail == "after" {
                return crash();
            }
        }
        self.sweep()
    }
    fn sweep(&self) -> spindle_store::Result<()> {
        let removing = self.0.parent().unwrap().join("tasks.removing");
        if removing.exists() {
            std::fs::remove_dir_all(removing)?;
        }
        Ok(())
    }
}

/// A runner: the work, and a reader of the call's files, give the answer.
type RunnerFn<'a> = Box<dyn Fn(&Value, &dyn Fn(&str) -> Vec<u8>) -> Value + 'a>;
/// A fetch hook: (host, session, work id, request index from 1) → frames, if taken.
type FetchHook<'a> = Box<dyn FnMut(&Embedded, &str, u64, usize) -> Option<Vec<Value>> + 'a>;

/// What the app did while driving an embedded run.
#[derive(Default)]
struct Run {
    requests: Vec<String>,
    events: Vec<Value>,
    replies: Vec<Value>,
    works: Vec<Value>,
    cancelled: Vec<Value>,
    /// Commands the host asked the app to stop (`work_stop`).
    stopping: Vec<Value>,
    /// Runner calls left unanswered (the runner gave null): (session, work id).
    held: Vec<(String, u64)>,
}

/// An app driving `Embedded`: performs works with `script` (per session label, in
/// order), answers app tools with `answer` and runner calls with `runner`.
struct App<'a> {
    embedded: &'a Embedded,
    scripts: BTreeMap<String, VecDeque<Reply>>,
    answer: Box<dyn Fn(&Value) -> Value + 'a>,
    runner: RunnerFn<'a>,
    /// Split response bodies into parts of this size (exercises streaming).
    part: usize,
    /// Leave fetches from this one on (1-based) unanswered: a crash before their
    /// response. 0: answer all.
    hold: usize,
    /// Takes a fetch (session, id, request index from 1) before the script does;
    /// Some(frames) when it handled it.
    hook: Option<FetchHook<'a>>,
    run: Run,
    sent: usize,
}

impl<'a> App<'a> {
    fn new(embedded: &'a Embedded) -> Self {
        Self {
            embedded,
            scripts: BTreeMap::new(),
            answer: Box::new(|_| json!({"content": "ok"})),
            runner: Box::new(|_, _| json!({"kind": "ran", "output": "", "success": true})),
            part: 7,
            hold: 0,
            hook: None,
            run: Run::default(),
            sent: 0,
        }
    }

    fn script(mut self, label: &str, replies: Vec<Reply>) -> Self {
        self.scripts.insert(label.into(), replies.into());
        self
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.sent + 1000;
        self.sent += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let frames = self.embedded.handle(line.to_string().as_bytes());
        self.drive(frames);
        let position = self
            .run
            .replies
            .iter()
            .position(|reply| reply["id"] == id)
            .unwrap_or_else(|| panic!("no reply to {method}: {:?}", self.run.replies));
        self.run.replies.remove(position)
    }

    fn drive(&mut self, frames: Vec<Value>) {
        let mut pending: VecDeque<Value> = frames.into();
        while let Some(frame) = pending.pop_front() {
            match frame["method"].as_str() {
                Some("event") => {
                    let event = frame["params"]["event"].clone();
                    self.run.events.push(event.clone());
                    if event["type"] == "app_tool" {
                        let mut answer = (self.answer)(&event);
                        answer["dir"] = frame["params"]["session"].clone();
                        answer["call"] = event["call"].clone();
                        let line = json!({"jsonrpc": "2.0", "id": 1, "method": "app_tool_result",
                            "params": answer});
                        let more = self.embedded.handle(line.to_string().as_bytes());
                        for frame in more {
                            if frame["id"] == 1 {
                                assert!(frame.get("result").is_some(), "answer refused: {frame}");
                            } else {
                                pending.push_back(frame);
                            }
                        }
                    }
                }
                Some("work") => {
                    let session = frame["params"]["session"].as_str().unwrap().to_string();
                    let id = frame["params"]["id"].as_u64().unwrap();
                    let work = frame["params"]["work"].clone();
                    self.run.works.push(work.clone());
                    let more = self.work(&session, id, &work);
                    pending.extend(more);
                }
                Some("work_cancel") => self.run.cancelled.push(frame["params"].clone()),
                Some("work_stop") => self.run.stopping.push(frame["params"].clone()),
                _ => self.run.replies.push(frame),
            }
        }
    }

    fn work(&mut self, session: &str, id: u64, work: &Value) -> Vec<Value> {
        let deliver = |answer: Value| {
            self.embedded
                .deliver(session, id, answer_of(&answer).unwrap())
                .unwrap()
        };
        match work["kind"].as_str().unwrap() {
            "authorize" => deliver(json!({"kind": "authorized"})),
            "fetch" => {
                // The body is read apart, as bytes; the frame carries none.
                assert!(work.get("body").is_none(), "{work}");
                let body = self.embedded.request_body(session, id).unwrap();
                self.run.requests.push(String::from_utf8(body).unwrap());
                let index = self.run.requests.len();
                if self.hold > 0 && index >= self.hold {
                    return vec![];
                }
                if let Some(hook) = &mut self.hook
                    && let Some(frames) = hook(self.embedded, session, id, index)
                {
                    return frames;
                }
                let key = match self.scripts.contains_key(session) {
                    true => session.to_string(),
                    false if session.contains('#') => "#".into(),
                    false => session.to_string(),
                };
                let reply = self
                    .scripts
                    .get_mut(&key)
                    .and_then(VecDeque::pop_front)
                    .unwrap_or(Reply::Text("(script ended)"));
                match wire(&reply, index) {
                    Some((200, _, body)) => {
                        let mut frames = vec![];
                        for part in body.as_bytes().chunks(self.part) {
                            let (more, streaming) = self.embedded.chunk(session, id, part);
                            frames.extend(more);
                            if !streaming {
                                return frames;
                            }
                        }
                        frames.extend(deliver(json!({"kind": "response", "status": 200,
                            "content_type": "text/event-stream"})));
                        frames
                    }
                    Some((status, retry, body)) => deliver(json!({"kind": "response",
                        "status": status, "retry_after": retry,
                        "body": spindle_store::protocol::base64_encode(body.as_bytes())})),
                    None => deliver(json!({"kind": "dropped", "reason": "transport"})),
                }
            }
            "run" => {
                let read = |object: &str| self.embedded.runner_file(session, id, object).unwrap();
                // The run's session label beside its work (a task's is `dir#key`).
                let mut work = work.clone();
                work["session"] = json!(session);
                let answer = (self.runner)(&work, &read);
                // A runner that gives null is still running it (answered later, or never).
                if answer.is_null() {
                    self.run.held.push((session.to_string(), id));
                    return vec![];
                }
                deliver(answer)
            }
            "wait" => deliver(json!({"kind": "waited"})),
            "app" => vec![],
            other => panic!("unexpected work {other}"),
        }
    }
}

fn embedded(places: Box<dyn Places>) -> Embedded {
    let fake = fake();
    Embedded::new(
        places,
        Catalog::from_json(&catalog_json(&fake.base)).unwrap(),
    )
}

/// Events both hosts report, for comparison.
fn shared_events(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| {
            !matches!(
                event["type"].as_str(),
                Some("cost" | "app_tool" | "app_tool_done")
            )
        })
        .cloned()
        .collect()
}

fn parity(
    name: &str,
    script: Vec<Reply>,
    settings: Value,
    prepare: impl FnOnce(&mut Settings),
    app: Option<Arc<dyn AppTools>>,
    state: Option<Value>,
    prompt: &str,
) -> Run {
    let (requests, events, outcome) =
        native(name, script.clone(), prepare, app, state.clone(), prompt);
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script(name, script);
    app.answer = Box::new(|_| lookup_answer());
    let mut create = json!({"dir": name, "provider": "fake", "model": "fake-model"});
    create
        .as_object_mut()
        .unwrap()
        .extend(settings.as_object().unwrap().clone());
    let created = app.call("create", create);
    assert!(created.get("result").is_some(), "{created}");
    let mut params = json!({"dir": name, "text": format!("[t:{name}] {prompt}")});
    if let Some(state) = state {
        params["state"] = state;
    }
    let reply = app.call("prompt", params);
    assert_eq!(
        reply.get("result").is_some(),
        outcome.is_ok(),
        "native {outcome:?}, embedded {reply}"
    );
    assert_eq!(
        app.run.requests.len(),
        requests.len(),
        "{name}: request count"
    );
    for (index, (embedded, native)) in app.run.requests.iter().zip(&requests).enumerate() {
        assert_eq!(embedded, native, "{name}: request {index} differs");
    }
    assert_eq!(
        shared_events(&app.run.events),
        shared_events(&events),
        "{name}: events differ"
    );
    std::mem::take(&mut app.run)
}

#[test]
fn a_plain_reply_matches_the_native_host() {
    parity(
        "plain",
        vec![Reply::Text("Hello.")],
        json!({}),
        |_| {},
        None,
        None,
        "say hello",
    );
}

#[test]
fn file_tools_match_the_native_host() {
    let run = parity(
        "files",
        vec![
            Reply::Call("write_file", json!({"path": "a.txt", "content": "one\n"})),
            Reply::Call(
                "edit_file",
                json!({"path": "a.txt", "old_string": "one", "new_string": "two"}),
            ),
            Reply::Call("read_file", json!({"path": "a.txt"})),
            Reply::Text("done"),
        ],
        json!({}),
        |_| {},
        None,
        None,
        "write, edit and read a file",
    );
    let results: Vec<&Value> = run
        .events
        .iter()
        .filter(|e| e["type"] == "tool_result")
        .collect();
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|r| r["failed"] == false), "{results:?}");
}

#[test]
fn an_app_tool_with_files_and_state_matches_the_native_host() {
    let run = parity(
        "app",
        vec![
            Reply::Call("lookup", json!({"q": "notes"})),
            Reply::Text("found them"),
        ],
        json!({"tools": lookup_tools(), "instructions": "You help with notes."}),
        |settings| {
            settings.tools = agent::app_tools::tools_from_json(&lookup_tools()).unwrap();
            settings.instructions = Some("You help with notes.".into());
        },
        Some(Arc::new(Lookup)),
        Some(json!({"topic": "notes"})),
        "find my notes",
    );
    let asked: Vec<&Value> = run
        .events
        .iter()
        .filter(|e| e["type"] == "app_tool")
        .collect();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0]["name"], "lookup");
    assert_eq!(asked[0]["again"], false);
}

#[test]
fn a_refused_request_sent_again_matches_the_native_host() {
    parity(
        "refused",
        vec![Reply::Refused, Reply::Text("second time")],
        json!({}),
        |_| {},
        None,
        None,
        "try twice",
    );
}

#[test]
fn a_tool_the_session_does_not_offer_is_refused_as_the_recorded_native_trace() {
    let recorded: Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/traces/tool-refused.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script(
        "tool-refused",
        vec![
            Reply::Call("bash", json!({"command": "true"})),
            Reply::Text("refused"),
        ],
    );
    app.call(
        "create",
        json!({"dir": "tool-refused", "provider": "fake", "model": "fake-model"}),
    );
    let reply = app.call(
        "prompt",
        json!({"dir": "tool-refused", "text": "[t:tool-refused] do not run commands"}),
    );
    assert!(reply.get("result").is_some(), "{reply}");
    let expected: Vec<String> = recorded["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        app.run.requests, expected,
        "the native trace's exact requests"
    );
    let events: Vec<Value> = shared_events(recorded["events"].as_array().unwrap())
        .into_iter()
        .map(|mut event| {
            if let Some(text) = event["text"].as_str() {
                event["text"] = json!(text);
            }
            event
        })
        .collect();
    assert_eq!(
        shared_events(&app.run.events),
        events,
        "the native trace's events"
    );
}

fn temp_root(name: &str) -> PathBuf {
    let root = fake().root.join(format!("embedded-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn kind(reply: &Value) -> Option<&str> {
    reply["error"]["data"]["code"].as_str()
}

#[test]
fn a_response_lost_to_a_crash_stays_unknown_and_is_sent_again_only_by_retry() {
    let root = temp_root("lost");
    {
        let host = embedded(Box::new(DirPlaces(root.clone())));
        let mut app = App::new(&host);
        app.call("create", json!({"dir": "s", "provider": "fake"}));
        app.hold = 1;
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": "prompt",
            "params": {"dir": "s", "text": "[t:lost] hello"}});
        let frames = host.handle(line.to_string().as_bytes());
        app.drive(frames);
        assert_eq!(app.run.requests.len(), 1, "the request went out once");
        assert!(app.run.replies.is_empty(), "the turn waits on its response");
        // The app (and its process) end here, with the response never delivered.
    }
    let host = embedded(Box::new(DirPlaces(root.clone())));
    let mut app = App::new(&host).script("s", vec![Reply::Text("again")]);
    let continued = app.call("continue", json!({"dir": "s"}));
    assert_eq!(kind(&continued), Some("failed"), "{continued}");
    assert!(
        app.run.requests.is_empty(),
        "continue never sends an unknown request again"
    );
    let recover = app.call("recover", json!({"dir": "s"}));
    assert_eq!(recover["result"]["wake"]["work"], "blocked", "{recover}");
    let retried = app.call("retry", json!({"dir": "s"}));
    assert!(retried.get("result").is_some(), "{retried}");
    assert_eq!(
        app.run.requests.len(),
        1,
        "retry sends it once, from a fresh store"
    );
    assert!(app.run.events.iter().any(|e| e["type"] == "retrying_first"));
    assert!(
        root.join("s/store.abandoned-1").exists(),
        "the old store is kept aside"
    );
    let status = app.call("status", json!({"dir": "s"}));
    assert_eq!(status["result"]["complete"], true, "{status}");
}

#[test]
fn a_later_lost_response_is_retried_in_place() {
    let root = temp_root("later");
    {
        let host = embedded(Box::new(DirPlaces(root.clone())));
        let mut app = App::new(&host).script(
            "s",
            vec![Reply::Call(
                "write_file",
                json!({"path": "a.txt", "content": "x"}),
            )],
        );
        app.call("create", json!({"dir": "s", "provider": "fake"}));
        app.hold = 2;
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": "prompt",
            "params": {"dir": "s", "text": "[t:later] write"}});
        let frames = host.handle(line.to_string().as_bytes());
        app.drive(frames);
        assert_eq!(app.run.requests.len(), 2);
        assert!(app.run.replies.is_empty());
    }
    let host = embedded(Box::new(DirPlaces(root.clone())));
    let mut app = App::new(&host).script("s", vec![Reply::Text("done")]);
    let retried = app.call("retry", json!({"dir": "s"}));
    assert!(retried.get("result").is_some(), "{retried}");
    assert_eq!(app.run.requests.len(), 1, "sent once more, in place");
    assert!(app.run.events.iter().any(|e| e["type"] == "retrying"));
    let read = app.call("read", json!({"dir": "s", "path": "a.txt"}));
    assert_eq!(read["result"]["content"], "x", "{read}");
}

#[test]
fn an_app_runner_runs_bash_and_its_changes_are_kept_and_rewound() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script(
        "r",
        vec![
            Reply::Call("write_file", json!({"path": "in.txt", "content": "data"})),
            Reply::Call("bash", json!({"command": "cp in.txt out.txt"})),
            Reply::Text("copied"),
        ],
    );
    app.runner = Box::new(|work, read| {
        assert!(
            work["command"]
                .as_str()
                .unwrap()
                .ends_with("cp in.txt out.txt")
        );
        let file = &work["files"][0];
        assert_eq!(file["path"], "in.txt");
        let bytes = read(file["object"].as_str().unwrap());
        assert_eq!(bytes, b"data");
        json!({"kind": "ran", "output": "copied 4 bytes\n", "success": true,
            "writes": [{"path": "out.txt", "text": "data"}]})
    });
    let created = app.call(
        "create",
        json!({"dir": "r", "provider": "fake", "runner": "app"}),
    );
    assert_eq!(created["result"]["settings"]["runner"], "app", "{created}");
    host.set_runner(None);
    let reply = app.call("prompt", json!({"dir": "r", "text": "[t:runner] copy it"}));
    assert!(reply.get("result").is_some(), "{reply}");
    assert!(
        app.run.requests[0].contains("\"name\":\"bash\""),
        "bash is offered"
    );
    assert!(app.run.events.iter().any(|e| e["type"] == "unisolated"));
    let result = app
        .run
        .events
        .iter()
        .filter(|e| e["type"] == "tool_result")
        .nth(1)
        .unwrap()
        .clone();
    assert_eq!(result["content"], "copied 4 bytes\n");
    let files = app.call("files", json!({"dir": "r"}));
    let paths: Vec<&str> = files["result"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["in.txt", "out.txt"]);
    let targets = app.call("targets", json!({"dir": "r"}));
    let before = targets["result"]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["kind"] == "settled")
        .map(|t| t["revision"].as_u64().unwrap());
    if let Some(target) = before {
        let rewound = app.call("rewind", json!({"dir": "r", "revision": target}));
        assert!(rewound.get("result").is_some(), "{rewound}");
    }
}

#[test]
fn an_app_runner_with_its_own_sandbox_counts_as_isolated_and_work_names_the_runner() {
    let host = embedded(Box::new(MemoryPlaces));
    let seen = std::cell::RefCell::new(Vec::new());
    let mut app = App::new(&host).script(
        "own",
        vec![
            Reply::Call("bash", json!({"command": "true"})),
            Reply::Text("ok"),
        ],
    );
    app.runner = Box::new(|work, _| {
        seen.borrow_mut().push(work["runner"].clone());
        json!({"kind": "ran", "output": "", "success": true})
    });
    app.call(
        "create",
        json!({"dir": "own", "provider": "fake", "runner": "app"}),
    );
    // Required isolation runs on the app's runner once it declares a sandbox (without
    // one it is refused: required_isolation_never_hands_a_command_to_...).
    app.call("settings", json!({"dir": "own", "isolation": "required"}));
    host.set_app_runner(Some([9; 32]));
    let reply = app.call("prompt", json!({"dir": "own", "text": "[t:own] run"}));
    assert!(reply.get("result").is_some(), "{reply}");
    assert_eq!(
        *seen.borrow(),
        [json!("app")],
        "the run names the app's runner"
    );
    assert!(
        !app.run.events.iter().any(|e| e["type"] == "unisolated"),
        "a declared sandbox is not unisolated"
    );
    drop(app);
    // A remote session's runs name the remote runner.
    let seen = std::cell::RefCell::new(Vec::new());
    let mut app = App::new(&host).script(
        "far",
        vec![
            Reply::Call("bash", json!({"command": "true"})),
            Reply::Text("ok"),
        ],
    );
    app.runner = Box::new(|work, _| {
        seen.borrow_mut().push(work["runner"].clone());
        json!({"kind": "ran", "output": "", "success": true})
    });
    host.set_runner(Some([7; 32]));
    app.call(
        "create",
        json!({"dir": "far", "provider": "fake", "runner": "remote"}),
    );
    let reply = app.call("prompt", json!({"dir": "far", "text": "[t:far] run"}));
    assert!(reply.get("result").is_some(), "{reply}");
    assert_eq!(*seen.borrow(), [json!("remote")]);
}

#[test]
fn a_script_reaches_only_what_the_apps_http_grants_allow() {
    let host = embedded(Box::new(MemoryPlaces));
    let refused = host
        .set_http_grants(
            &json!([{"name": "a", "url": "https://a.example/", "risk": "read",
            "headers": {"authorization": "Bearer x"}}]),
        )
        .unwrap_err();
    assert_eq!(refused.kind, "bad_request", "{}", refused.message);
    let invalid = host
        .set_http_grants(&json!([{"name": "a", "url": "ftp://a.example/", "risk": "read"}]))
        .unwrap_err();
    assert_eq!(invalid.kind, "bad_request", "{}", invalid.message);
    host.set_http_grants(
        &json!([{"name": "notes", "url": "https://notes.example/api/",
        "risk": "read", "max_response_bytes": 4096}]),
    )
    .unwrap();
    let direct = |method: &str, params: Value| -> Value {
        let line = json!({"jsonrpc": "2.0", "id": 77, "method": method, "params": params});
        host.handle(line.to_string().as_bytes())
            .into_iter()
            .find(|frame| frame["id"] == 77)
            .expect("a reply")
    };
    let script = |request: Value| direct("code_call", json!({"dir": "web", "request": request}));
    let answer = |asked: &Value, answer: Value| {
        let call = asked["result"]["call"].clone();
        direct(
            "code_answer",
            json!({"dir": "web", "call": call, "index": 0, "answer": answer}),
        )
    };
    let seen = std::cell::RefCell::new(BTreeMap::new());
    let mut app = App::new(&host).script(
        "web",
        vec![
            Reply::Call(
                "bash",
                json!({"command": "spindle http https://notes.example/api/items"}),
            ),
            Reply::Text("done"),
        ],
    );
    app.runner = Box::new(|_, _| {
        let mut seen = seen.borrow_mut();
        // Covered: the app is handed the request to send, and brings back the response.
        let asked = script(json!({"method": "http", "id": 0,
            "input": {"url": "https://notes.example/api/items?page=1"}}));
        seen.insert("children", asked.clone());
        let body = spindle_store::protocol::base64_encode(br#"{"items":[1,2]}"#);
        seen.insert(
            "answered",
            answer(
                &asked,
                json!({"status": 200, "headers": {"content-type": "application/json"},
                "body_base64": body}),
            ),
        );
        // Not covered, or not a method the grant allows: refused, nothing sent.
        seen.insert(
            "elsewhere",
            script(
                json!({"method": "http", "id": 1, "input": {"url": "https://elsewhere.example/"}}),
            ),
        );
        seen.insert(
            "post",
            script(json!({"method": "http", "id": 2,
                "input": {"url": "https://notes.example/api/items", "method": "POST", "json": {}}})),
        );
        // A redirect the app does not follow: the server answered, a known outcome.
        let asked = script(
            json!({"method": "http", "id": 3, "input": {"url": "https://notes.example/api/old"}}),
        );
        seen.insert(
            "redirected",
            answer(
                &asked,
                json!({"error": "the server redirected", "answered": true}),
            ),
        );
        json!({"kind": "ran", "output": "scripted\n", "success": true})
    });
    app.call(
        "create",
        json!({"dir": "web", "provider": "fake", "runner": "app"}),
    );
    let reply = app.call("prompt", json!({"dir": "web", "text": "[t:http] go"}));
    assert!(reply.get("result").is_some(), "{reply}");
    drop(app);
    let seen = seen.into_inner();
    let reply = |name: &str| seen[name]["result"]["reply"].clone();
    let child = &seen["children"]["result"]["children"][0];
    assert_eq!(child["kind"], "http", "{child}");
    assert_eq!(child["method"], "GET");
    assert_eq!(child["url"], "https://notes.example/api/items?page=1");
    assert_eq!(
        child["max_response_bytes"], 4096,
        "the grant's response limit"
    );
    let data = &reply("answered")["data"];
    assert_eq!(data["status"], 200, "{:?}", seen["answered"]);
    assert_eq!(data["json"], json!({"items": [1, 2]}), "{data}");
    assert_eq!(data["redirects"], 0);
    let elsewhere = reply("elsewhere");
    assert_eq!(elsewhere["ok"], false, "{elsewhere}");
    assert!(
        elsewhere["error"]
            .as_str()
            .unwrap()
            .contains("no HTTP grant covers"),
        "{elsewhere}"
    );
    assert!(seen["elsewhere"]["result"].get("children").is_none());
    assert!(
        reply("post")["error"]
            .as_str()
            .unwrap()
            .contains("does not allow POST"),
        "{:?}",
        seen["post"]
    );
    let redirected = reply("redirected");
    assert_eq!(redirected["ok"], false, "{redirected}");
    assert_eq!(redirected["may_have_run"], true);
    assert!(
        redirected.get("unknown").is_none(),
        "answered: {redirected}"
    );
}

#[test]
fn a_grants_secret_headers_are_named_to_the_app_and_never_set_by_scripts() {
    let host = embedded(Box::new(MemoryPlaces));
    // A value the package would hold is refused: the app supplies it as it sends.
    for value in [json!("Bearer x"), json!({"env": "TOKEN"})] {
        let refused = host
            .set_http_grants(
                &json!([{"name": "api", "url": "https://api.example/", "risk": "read",
                "headers": {"Authorization": value}}]),
            )
            .unwrap_err();
        assert!(
            refused.message.contains("the app supplies"),
            "{}",
            refused.message
        );
    }
    host.set_http_grants(
        &json!([{"name": "api", "url": "https://api.example/v1/", "risk": "read",
        "headers": {"Authorization": {"app": true}}}]),
    )
    .unwrap();
    let direct = |method: &str, params: Value| -> Value {
        let line = json!({"jsonrpc": "2.0", "id": 78, "method": method, "params": params});
        host.handle(line.to_string().as_bytes())
            .into_iter()
            .find(|frame| frame["id"] == 78)
            .expect("a reply")
    };
    let script = |request: Value| direct("code_call", json!({"dir": "keyed", "request": request}));
    let seen = std::cell::RefCell::new(BTreeMap::new());
    let mut app = App::new(&host).script(
        "keyed",
        vec![
            Reply::Call(
                "bash",
                json!({"command": "spindle http https://api.example/v1/me"}),
            ),
            Reply::Text("done"),
        ],
    );
    app.runner = Box::new(|_, _| {
        let mut seen = seen.borrow_mut();
        let asked = script(
            json!({"method": "http", "id": 0, "input": {"url": "https://api.example/v1/me"}}),
        );
        seen.insert("children", asked.clone());
        let body = spindle_store::protocol::base64_encode(b"{\"me\":1}");
        let call = asked["result"]["call"].clone();
        seen.insert(
            "answered",
            direct(
                "code_answer",
                json!({"dir": "keyed", "call": call, "index": 0, "answer": {"status": 200,
                    "headers": {"content-type": "application/json"}, "body_base64": body}}),
            ),
        );
        // The script may not set the header itself, whatever its case: nothing is reserved.
        seen.insert(
            "own",
            script(
                json!({"method": "http", "id": 1, "input": {"url": "https://api.example/v1/me",
                "headers": {"authorization": "Bearer guessed"}}}),
            ),
        );
        json!({"kind": "ran", "output": "scripted\n", "success": true})
    });
    app.call(
        "create",
        json!({"dir": "keyed", "provider": "fake", "runner": "app"}),
    );
    let reply = app.call("prompt", json!({"dir": "keyed", "text": "[t:keyed] go"}));
    assert!(reply.get("result").is_some(), "{reply}");
    drop(app);
    let seen = seen.into_inner();
    let child = &seen["children"]["result"]["children"][0];
    assert_eq!(child["grant"], "api", "{child}");
    assert_eq!(
        child["app_headers"],
        json!(["Authorization"]),
        "the app is told what to add"
    );
    assert_eq!(
        child["headers"],
        json!({}),
        "and the script sent none of it"
    );
    assert_eq!(
        seen["answered"]["result"]["reply"]["data"]["json"],
        json!({"me": 1})
    );
    let own = &seen["own"]["result"];
    assert!(own.get("children").is_none(), "nothing to send: {own}");
    assert!(
        own["reply"]["error"]
            .as_str()
            .unwrap()
            .contains("supplied by grant api"),
        "{own}"
    );
}

#[test]
fn a_script_in_the_runner_calls_models_and_tools_through_the_journal() {
    let host = embedded(Box::new(MemoryPlaces));
    // What the app does while the command runs: the script's requests, by the API.
    let direct = |method: &str, params: Value| -> Value {
        let line = json!({"jsonrpc": "2.0", "id": 77, "method": method, "params": params});
        host.handle(line.to_string().as_bytes())
            .into_iter()
            .find(|frame| frame["id"] == 77)
            .expect("a reply")
    };
    let script = |request: Value| direct("code_call", json!({"dir": "code", "request": request}));
    let seen = std::cell::RefCell::new(BTreeMap::new());
    let mut app = App::new(&host).script(
        "code",
        vec![
            Reply::Call("bash", json!({"command": "spindle model hi"})),
            Reply::Text("done"),
        ],
    );
    app.runner = Box::new(|_, _| {
        let mut seen = seen.borrow_mut();
        seen.insert("limits", script(json!({"method": "limits", "input": {}})));
        seen.insert(
            "search",
            script(json!({"method": "search", "input": {"query": "word"}})),
        );
        seen.insert(
            "describe",
            script(json!({"method": "describe", "input": {"name": "lookup"}})),
        );
        // A tool: the app runs it (its handler), the store journals it.
        let tool = json!({"method": "tool", "id": 0, "input": {"name": "lookup", "arguments": {"q": "spindle"}}});
        let asked = script(tool.clone());
        let call = asked["result"]["call"].clone();
        seen.insert("tool children", asked.clone());
        // An answer that is no answer is refused, nothing recorded; none at all, too.
        seen.insert(
            "not an answer",
            direct(
                "code_answer",
                json!({"dir": "code", "call": call, "index": 0, "answer": {"contents": "typo"}}),
            ),
        );
        seen.insert(
            "no answer",
            direct(
                "code_answer",
                json!({"dir": "code", "call": call, "index": 0}),
            ),
        );
        seen.insert(
            "tool",
            direct(
                "code_answer",
                json!({"dir": "code", "call": call, "index": 0,
                "answer": {"content": "a spindle twists fibre"}}),
            ),
        );
        // A model: the app sends the request it is handed, the store reads the reply.
        let asked = script(json!({"method": "model", "id": 1, "input": {"prompt": "hi"}}));
        seen.insert("model children", asked.clone());
        let call = asked["result"]["call"].clone();
        seen.insert(
            "model",
            direct(
                "code_answer",
                json!({"dir": "code", "call": call, "index": 0,
                "answer": {"status": 200, "body": chunks(json!({"content": "hello there"}), 1)}}),
            ),
        );
        // Replayed from the journal, nothing run again; an id reused differently refused.
        seen.insert("replay", script(tool.clone()));
        seen.insert(
            "reused",
            script(json!({"method": "tool", "id": 0, "input": {"name": "lookup", "arguments": {"q": "other"}}})),
        );
        // A refusal the provider streams before any output is known: nothing billed.
        let asked = script(json!({"method": "model", "id": 3, "input": {"prompt": "again"}}));
        let call = asked["result"]["call"].clone();
        seen.insert(
            "refused",
            direct(
                "code_answer",
                json!({"dir": "code", "call": call, "index": 0, "answer": {"status": 200,
                    "body": "data: {\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n"}}),
            ),
        );
        seen.insert(
            "bad input",
            script(json!({"method": "tool", "id": 2, "input": {"name": "lookup", "arguments": {"word": 1}}})),
        );
        json!({"kind": "ran", "output": "scripted\n", "success": true})
    });
    let created = app.call(
        "create",
        json!({"dir": "code", "provider": "fake", "runner": "app", "tools": [{
            "name": "lookup", "description": "Looks a word up",
            "input_schema": {"type": "object", "properties": {"q": {"type": "string"}},
                "required": ["q"], "additionalProperties": false}}]}),
    );
    assert!(created.get("result").is_some(), "{created}");
    let reply = app.call("prompt", json!({"dir": "code", "text": "[t:code] go"}));
    assert!(reply.get("result").is_some(), "{reply}");
    drop(app);
    let seen = seen.into_inner();
    let reply = |name: &str| seen[name]["result"]["reply"].clone();
    assert_eq!(reply("limits")["ok"], true, "{:?}", seen["limits"]);
    assert_eq!(
        reply("limits")["data"]["calls"],
        128,
        "the bridge's own limits"
    );
    let refused = reply("refused");
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(
        refused["may_have_run"], true,
        "as native code mode says it: the request left, its status given: {refused}"
    );
    assert_eq!(refused["status"], 529);
    assert!(refused.get("unknown").is_none(), "{refused}");
    assert_eq!(
        reply("search")["data"][0]["name"],
        "lookup",
        "{:?}",
        seen["search"]
    );
    assert_eq!(
        reply("describe")["data"]["input_schema"]["required"][0],
        "q"
    );
    let children = &seen["tool children"]["result"]["children"];
    assert_eq!(children[0]["kind"], "app", "{children}");
    assert_eq!(
        seen["tool children"]["result"]["parallel"], 1,
        "app tools one at a time, as natively"
    );
    let call = seen["tool children"]["result"]["call"].as_u64().unwrap();
    assert!(call > 1 << 20, "call numbers start at random: {call}");
    for name in ["not an answer", "no answer"] {
        assert_eq!(
            seen[name]["error"]["data"]["code"], "bad_request",
            "{name}: {:?}",
            seen[name]
        );
    }
    assert_eq!(children[0]["input"]["q"], "spindle");
    assert_eq!(
        reply("tool"),
        json!({"ok": true, "data": "a spindle twists fibre"})
    );
    let model = &seen["model children"]["result"]["children"][0];
    assert_eq!(model["kind"], "model", "{model}");
    assert_eq!(
        model["auth"]["authorization"], "Bearer {key}",
        "the key's place, never a key"
    );
    assert!(model["body"]["messages"].is_array(), "{model}");
    assert_eq!(
        reply("model")["data"]["text"],
        "hello there",
        "{:?}",
        seen["model"]
    );
    assert_eq!(
        reply("replay"),
        json!({"ok": true, "data": "a spindle twists fibre"}),
        "replayed"
    );
    assert!(
        seen["replay"]["result"].get("children").is_none(),
        "a replay runs nothing"
    );
    assert!(
        reply("reused")["error"]
            .as_str()
            .is_some_and(|error| error.contains("reused")),
        "{:?}",
        seen["reused"]
    );
    assert_eq!(reply("bad input")["ok"], false, "{:?}", seen["bad input"]);
    // Once the command has ended, its scripts are gone.
    let late = script(json!({"method": "limits", "input": {}}));
    assert!(
        late["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("no command")),
        "{late}"
    );
}

#[test]
fn a_remote_runner_labels_its_attempts_with_the_hosts_sandbox() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script(
        "remote",
        vec![
            Reply::Call("bash", json!({"command": "true"})),
            Reply::Text("ok"),
        ],
    );
    app.call(
        "create",
        json!({"dir": "remote", "provider": "fake", "runner": "remote"}),
    );
    let refused = app.call("prompt", json!({"dir": "remote", "text": "[t:remote] run"}));
    assert_eq!(
        kind(&refused),
        Some("failed"),
        "no remote runner is connected: {refused}"
    );
    host.set_runner(Some([7; 32]));
    let mut app = App::new(&host).script("remote", vec![Reply::Text("ok")]);
    let reply = app.call("continue", json!({"dir": "remote"}));
    assert!(reply.get("result").is_some(), "{reply}");
    assert!(!app.run.events.iter().any(|e| e["type"] == "unisolated"));
}

#[test]
fn a_task_runs_as_a_child_session_and_its_files_merge_into_the_parent() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host)
        .script(
            "t",
            vec![
                Reply::Call("task", json!({"prompt": "write b.txt"})),
                Reply::Text("the task finished"),
            ],
        )
        .script(
            "#",
            vec![
                Reply::Call(
                    "write_file",
                    json!({"path": "b.txt", "content": "from the task"}),
                ),
                Reply::Text("child done"),
            ],
        );
    app.call(
        "create",
        json!({"dir": "t", "provider": "fake", "tasks": true}),
    );
    let reply = app.call("prompt", json!({"dir": "t", "text": "[t:task] delegate"}));
    assert!(reply.get("result").is_some(), "{reply}");
    assert_eq!(app.run.requests.len(), 4, "parent, child, child, parent");
    assert!(app.run.events.iter().any(|e| e["type"] == "task"));
    let last = &app.run.requests[3];
    assert!(
        last.contains("child done"),
        "the task's answer reaches the parent"
    );
    let read = app.call("read", json!({"dir": "t", "path": "b.txt"}));
    assert_eq!(read["result"]["content"], "from the task", "{read}");
}

#[test]
fn stop_cuts_a_request_off_and_continue_never_resends_it() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host);
    app.call("create", json!({"dir": "x", "provider": "fake"}));
    app.hold = 1;
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "prompt",
        "params": {"dir": "x", "text": "[t:stop] hello"}});
    let frames = host.handle(line.to_string().as_bytes());
    app.drive(frames);
    let stopped = app.call("stop", json!({"dir": "x"}));
    assert_eq!(stopped["result"]["stopping"], true, "{stopped}");
    assert!(
        !app.run.cancelled.is_empty(),
        "the app is told to end the request"
    );
    let ended = app
        .run
        .replies
        .iter()
        .find(|r| r["id"] == 1)
        .unwrap()
        .clone();
    assert_eq!(kind(&ended), Some("stopped"), "{ended}");
    // The app's late answer to the cancelled request changes nothing.
    let cancelled = app.run.cancelled[0].clone();
    let late = host.deliver(
        cancelled["session"].as_str().unwrap(),
        cancelled["id"].as_u64().unwrap(),
        answer_of(&json!({"kind": "dropped", "reason": "stopped"})).unwrap(),
    );
    assert!(late.unwrap().is_empty(), "a late answer changes nothing");
    app.hold = 0;
    let before = app.run.requests.len();
    let continued = app.call("continue", json!({"dir": "x"}));
    assert_eq!(app.run.requests.len(), before, "{continued}");
}

#[test]
fn steering_reaches_the_next_request_and_send_now_cuts_a_stream() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script(
        "steer",
        vec![
            Reply::Call("write_file", json!({"path": "a.txt", "content": "1"})),
            Reply::Text("after the message"),
        ],
    );
    app.call("create", json!({"dir": "steer", "provider": "fake"}));
    // The second request streams a little; then the user sends a message now.
    app.hook = Some(Box::new(|host, session, id, index| {
        if index != 2 {
            return None;
        }
        let (_, _, stream) = wire(&Reply::Text("unfinished reply that is cut"), 2).unwrap();
        let (mut frames, streaming) = host.chunk(session, id, &stream.as_bytes()[..150]);
        assert!(streaming);
        let steered = json!({"jsonrpc": "2.0", "id": 2, "method": "steer",
            "params": {"dir": "steer", "text": "read this now", "now": true}});
        frames.extend(host.handle(steered.to_string().as_bytes()));
        Some(frames)
    }));
    let reply = app.call("prompt", json!({"dir": "steer", "text": "[t:steer] go"}));
    assert!(reply.get("result").is_some(), "{reply}");
    let steered = app.run.replies.iter().find(|r| r["id"] == 2).unwrap();
    assert_eq!(steered["result"]["queued"], 1, "{steered}");
    assert!(
        app.run.events.iter().any(|e| e["type"] == "reply_cut"),
        "{:?}",
        app.run.events
    );
    assert!(app.run.events.iter().any(|e| e["type"] == "steered"));
    assert!(app.run.cancelled.iter().any(|c| c["session"] == "steer"));
    assert_eq!(
        app.run.requests.len(),
        3,
        "the cut request is prepared again"
    );
    assert!(
        app.run.requests[2].contains("read this now"),
        "the message is read first"
    );
}

#[test]
fn the_api_reads_settings_and_refusals() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script("api", vec![Reply::Text("hi")]);
    let info = app.call("info", json!({}));
    assert_eq!(info["result"]["transport"], "embedded");
    for (settings, why) in [
        (json!({"builtin_tools": "all"}), "sandbox"),
        (json!({"max_usd": -1}), "non-negative"),
        (json!({"sources": ["/tmp"]}), "sources"),
    ] {
        let mut params = json!({"dir": "refused", "provider": "fake"});
        params
            .as_object_mut()
            .unwrap()
            .extend(settings.as_object().unwrap().clone());
        let refused = app.call("create", params);
        assert_eq!(kind(&refused), Some("bad_request"), "{refused}");
        assert!(
            refused["error"]["message"].as_str().unwrap().contains(why),
            "{refused}"
        );
    }
    app.call(
        "create",
        json!({"dir": "api", "provider": "fake",
        "files": [{"path": "seed.txt", "text": "seed"}]}),
    );
    let again = app.call("create", json!({"dir": "api", "provider": "fake"}));
    assert_eq!(kind(&again), Some("conflict"));
    let reply = app.call(
        "prompt",
        json!({"dir": "api", "text": "[t:api] hello",
        "state": {"step": 1}}),
    );
    assert!(reply.get("result").is_some(), "{reply}");
    let head = reply["result"]["head"].as_u64().unwrap();
    let set = app.call(
        "state_set",
        json!({"dir": "api", "patch": {"step": 2}, "expected_head": head}),
    );
    assert!(set.get("result").is_some(), "{set}");
    let moved = app.call(
        "state_set",
        json!({"dir": "api", "patch": {"step": 3}, "expected_head": head}),
    );
    assert_eq!(kind(&moved), Some("head_moved"), "{moved}");
    let state = app.call("state", json!({"dir": "api"}));
    assert_eq!(state["result"]["state"], json!({"step": 2}));
    let history = app.call("history", json!({"dir": "api"}));
    assert_eq!(history["result"]["messages"].as_array().unwrap().len(), 2);
    let read = app.call("read", json!({"dir": "api", "path": "seed.txt"}));
    assert_eq!(read["result"]["content"], "seed");
    let settings = app.call("settings", json!({"dir": "api", "effort": "high"}));
    assert_eq!(settings["result"]["effort"], "high", "{settings}");
    let list = app.call("list", json!({}));
    assert_eq!(list["result"]["sessions"].as_array().unwrap().len(), 1);
    let status = app.call("status", json!({"dir": "api"}));
    assert_eq!(status["result"]["busy"], false);
    assert_eq!(status["result"]["accepts_prompt"], true);
    let closed = app.call("close", json!({"dir": "api"}));
    assert_eq!(closed["result"]["closed"], true);
    let gone = app.call("status", json!({"dir": "api"}));
    assert_eq!(
        kind(&gone),
        Some("not_found"),
        "memory sessions end when closed"
    );
    let unknown = app.call("export", json!({"dir": "api", "target": "/tmp/x"}));
    assert_eq!(kind(&unknown), Some("method_not_found"));
}

#[test]
fn a_dropped_connection_matches_the_native_host() {
    parity(
        "dropped",
        vec![Reply::HangUp, Reply::Text("after the drop")],
        json!({"resend_unknown": true}),
        |settings| settings.resend_unknown = true,
        None,
        None,
        "survive a drop",
    );
}

#[test]
fn an_app_started_task_answers_and_keeps_its_files() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host)
        .script("ts", vec![Reply::Text("ready")])
        .script(
            "#",
            vec![
                Reply::Call(
                    "write_file",
                    json!({"path": "t.txt", "content": "task file"}),
                ),
                Reply::Text("task answer"),
            ],
        );
    app.call(
        "create",
        json!({"dir": "ts", "provider": "fake", "tasks": true,
        "files": [{"path": "seed.txt", "text": "seed"}]}),
    );
    app.call("prompt", json!({"dir": "ts", "text": "[t:ts] hello"}));
    let started = app.call(
        "task_start",
        json!({"dir": "ts", "task": "review-1",
        "prompt": "[t:ts-task] look at seed.txt"}),
    );
    let result = &started["result"];
    let answer = result["answer"].as_str().unwrap();
    assert!(answer.starts_with("task answer"), "{started}");
    assert!(
        answer.ends_with("1 file(s) written, 0 removed]"),
        "as the native host notes them"
    );
    assert_eq!(result["failed"], false);
    assert_eq!(result["state"], "finished");
    assert!(result["task"].as_str().unwrap().starts_with("ts#"));
    let parent = app.call("read", json!({"dir": "ts", "path": "t.txt"}));
    assert_eq!(
        kind(&parent),
        Some("not_found"),
        "the task's files stay in the task"
    );
    let queued = app.call(
        "task_start",
        json!({"dir": "ts", "task": "q", "prompt": "[t:ts-q] in the background",
        "report": "queue"}),
    );
    assert_eq!(queued["result"]["queued"], true, "{queued}");
}

#[test]
fn background_tasks_run_beside_their_session_and_report_when_done() {
    let host = embedded(Box::new(MemoryPlaces));
    // The model starts a task in the background and goes on; the task answers later.
    let mut app = App::new(&host)
        .script(
            "bg",
            vec![
                Reply::Call(
                    "task",
                    json!({"prompt": "[t:bg-task] count the files", "background": true}),
                ),
                Reply::Text("it runs in the background"),
                Reply::Text("thanks for the report"),
            ],
        )
        .script("#", vec![Reply::Text("there are no files")]);
    app.call(
        "create",
        json!({"dir": "bg", "provider": "fake", "tasks": true}),
    );
    let first = app.call(
        "prompt",
        json!({"dir": "bg", "text": "[t:bg] start a task"}),
    );
    assert!(first.get("result").is_some(), "{first}");
    let tool = app
        .run
        .requests
        .iter()
        .find(|body| body.contains("started in the background"));
    assert!(tool.is_some(), "the call is answered at once");
    let tasks = app.call("tasks", json!({"dir": "bg"}))["result"]["tasks"].clone();
    assert_eq!(tasks.as_array().unwrap().len(), 1, "{tasks}");
    assert_eq!(tasks[0]["state"], "finished", "{tasks}");
    assert_eq!(tasks[0]["queued"], true);
    assert!(
        tasks[0]["answer"]["content"]
            .as_str()
            .unwrap()
            .starts_with("there are no files"),
        "{tasks}"
    );
    // The task ended while the turn ran (this app answers at once): the turn's next
    // request read its answer to the model as a queued report.
    assert_eq!(tasks[0]["delivered"], true, "{tasks}");
    let reported = app
        .run
        .requests
        .iter()
        .position(|body| body.contains("there are no files"))
        .expect("the report reaches the model");
    let answered = app
        .run
        .requests
        .iter()
        .position(|body| body.contains("started in the background"))
        .unwrap();
    assert!(reported >= answered, "after the call was answered");
    let label = tasks[0]["task"].as_str().unwrap().to_string();
    assert_eq!(
        app.call("task_wait", json!({"dir": "bg", "task": label}))["result"]["state"],
        "finished"
    );
    assert_eq!(
        app.call("task_stop", json!({"dir": "bg", "task": label}))["result"]["stopping"],
        false,
        "nothing runs to stop"
    );
    let capabilities = app.call("capabilities", json!({}))["result"].clone();
    assert_eq!(capabilities["tasks"], json!(["wait", "queue"]));
}

#[test]
fn a_background_task_left_running_is_taken_up_when_its_session_opens_again() {
    let root = temp_root("background-resume");
    {
        let host = embedded(Box::new(DirPlaces(root.clone())));
        let mut app = App::new(&host).script("br", vec![Reply::Text("ready")]);
        app.call(
            "create",
            json!({"dir": "br", "provider": "fake", "tasks": true}),
        );
        app.call("prompt", json!({"dir": "br", "text": "[t:br] hello"}));
        // The task's request goes and its answer never comes: the app closes.
        app.hold = 2;
        let queued = app.call(
            "task_start",
            json!({"dir": "br", "task": "later", "prompt": "[t:br-task] look", "report": "queue"}),
        );
        assert_eq!(queued["result"]["queued"], true, "{queued}");
        assert_eq!(app.run.requests.len(), 2, "the task's request was sent");
        let tasks = app.call("tasks", json!({"dir": "br"}))["result"]["tasks"].clone();
        assert_eq!(tasks[0]["state"], "running", "{tasks}");
    }
    let host = embedded(Box::new(DirPlaces(root.clone())));
    let mut app = App::new(&host).script("br", vec![Reply::Text("more")]);
    // Read, the session leaves its tasks as they are.
    let tasks = app.call("tasks", json!({"dir": "br"}))["result"]["tasks"].clone();
    assert_eq!(tasks[0]["state"], "running", "{tasks}");
    assert!(app.run.requests.is_empty());
    // A turn takes the task up; its request's outcome is unknown, so it is not sent
    // again: the task cannot go on and stops, saying why, and its report says so once the
    // turn's own work is done (the session goes on: nothing waits for a person).
    let next = app.call("prompt", json!({"dir": "br", "text": "[t:br] again"}));
    assert!(next["error"].is_null(), "{next}");
    let tasks = app.call("tasks", json!({"dir": "br"}))["result"]["tasks"].clone();
    assert_eq!(tasks[0]["state"], "stopped", "{tasks}");
    assert!(
        tasks[0]["answer"]["content"]
            .as_str()
            .is_some_and(|said| said.contains("unknown outcome")),
        "{tasks}"
    );
    assert_eq!(tasks[0]["delivered"], true, "{tasks}");
    assert_eq!(
        app.run.requests.len(),
        2,
        "the turn's own request, then one after the report, never the task's again: {:?}",
        app.run.requests
    );
    assert!(
        app.run.requests[1].contains("[task app:later: stopped]")
            && app.run.requests[1].contains("gave no answer. Why: "),
        "{}",
        app.run.requests[1]
    );
    // Its history shows the report as the task's, not as the user's.
    let history = app.call("history", json!({"dir": "br"}))["result"]["messages"].clone();
    let report = history
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
        .find(|block| block["report"].is_object())
        .expect("the report is marked");
    assert_eq!(report["report"]["state"], "stopped", "{report}");
    assert_eq!(
        report["report"]["task"],
        tasks[0]["task"]
            .as_str()
            .unwrap()
            .rsplit('#')
            .next()
            .unwrap(),
        "{report}"
    );
    assert!(
        app.run.events.iter().any(|event| event["label"]
            .as_str()
            .is_some_and(|label| label.contains("could not go on in the background"))),
        "{:?}",
        app.run.events
    );
}

#[test]
fn a_turn_ends_once_its_background_tasks_have_reported() {
    use std::cell::RefCell;
    use std::rc::Rc;
    let host = embedded(Box::new(MemoryPlaces));
    // The task's request is held: the session's turn finishes first.
    let held: Rc<RefCell<Option<(String, u64)>>> = Rc::default();
    let hold = held.clone();
    let mut app = App::new(&host).script(
        "tw",
        vec![
            Reply::Call(
                "task",
                json!({"prompt": "[t:tw-task] look around", "background": true}),
            ),
            Reply::Text("waiting for it"),
            Reply::Text("it said: all quiet"),
        ],
    );
    app.hook = Some(Box::new(move |_, session: &str, id, _| {
        if session.contains('#') && hold.borrow().is_none() {
            *hold.borrow_mut() = Some((session.to_string(), id));
            return Some(vec![]);
        }
        None
    }));
    app.call(
        "create",
        json!({"dir": "tw", "provider": "fake", "tasks": true}),
    );
    let line = json!({"jsonrpc": "2.0", "id": 77, "method": "prompt",
        "params": {"dir": "tw", "text": "[t:tw] start it"}});
    let frames = host.handle(line.to_string().as_bytes());
    app.drive(frames);
    assert!(
        app.run.replies.iter().all(|reply| reply["id"] != 77),
        "the turn waits for its task: {:?}",
        app.run.replies
    );
    let status = app.call("status", json!({"dir": "tw"}))["result"].clone();
    assert_eq!(status["activity"], "waiting_tasks", "{status}");
    assert_eq!(status["background"], 1);
    // Listed with what it was asked, to name it by.
    let tasks = app.call("tasks", json!({"dir": "tw"}))["result"]["tasks"].clone();
    assert_eq!(tasks[0]["prompt"], "[t:tw-task] look around", "{tasks}");
    assert_eq!(tasks[0]["pending"], true);
    // The task's answer arrives: its report is read to the session, which ends its turn.
    let (session, id) = held
        .borrow_mut()
        .take()
        .expect("the task's request was held");
    let (_, _, body) = wire(&Reply::Text("all quiet"), 9).unwrap();
    let mut frames = vec![];
    for part in body.as_bytes().chunks(64) {
        frames.extend(host.chunk(&session, id, part).0);
    }
    frames.extend(
        host.deliver(
            &session,
            id,
            answer_of(&json!({"kind": "response", "status": 200,
                "content_type": "text/event-stream"}))
            .unwrap(),
        )
        .unwrap(),
    );
    app.drive(frames);
    let done = app
        .run
        .replies
        .iter()
        .find(|reply| reply["id"] == 77)
        .expect("the turn ended");
    assert!(done.get("result").is_some(), "{done}");
    assert!(
        app.run.requests.last().unwrap().contains("all quiet"),
        "the report reached the model"
    );
    let status = app.call("status", json!({"dir": "tw"}))["result"].clone();
    assert_eq!(
        (status["activity"].clone(), status["background"].clone()),
        (json!("idle"), json!(0))
    );
}

/// Where a task's request waits while a test holds it: its session and work id.
type Held = std::rc::Rc<std::cell::RefCell<Option<(String, u64)>>>;

/// A session whose finished turn waits for a background task whose request is held:
/// the app, and where the task's request waits.
fn waiting_on_a_task<'a>(host: &'a Embedded, dir: &str, replies: Vec<Reply>) -> (App<'a>, Held) {
    let held: Held = Held::default();
    let hold = held.clone();
    let mut app = App::new(host).script(dir, replies);
    app.hook = Some(Box::new(move |_, session: &str, id, _| {
        if session.contains('#') && hold.borrow().is_none() {
            *hold.borrow_mut() = Some((session.to_string(), id));
            return Some(vec![]);
        }
        None
    }));
    app.call(
        "create",
        json!({"dir": dir, "provider": "fake", "tasks": true}),
    );
    let line = json!({"jsonrpc": "2.0", "id": 77, "method": "prompt",
        "params": {"dir": dir, "text": format!("[t:{dir}] start it")}});
    let frames = host.handle(line.to_string().as_bytes());
    app.drive(frames);
    let status = app.call("status", json!({"dir": dir}))["result"].clone();
    assert_eq!(status["activity"], "waiting_tasks", "{status}");
    (app, held)
}

#[test]
fn stopping_a_turn_that_waits_for_its_tasks_ends_it_and_they_go_on() {
    let host = embedded(Box::new(MemoryPlaces));
    let (mut app, held) = waiting_on_a_task(
        &host,
        "sw",
        vec![
            Reply::Call(
                "task",
                json!({"prompt": "[t:sw-task] look around", "background": true}),
            ),
            Reply::Text("waiting for it"),
            Reply::Text("it said: all quiet"),
        ],
    );
    let stopped = app.call("stop", json!({"dir": "sw"}));
    assert_eq!(stopped["result"]["stopping"], true, "{stopped}");
    // The turn had finished: it ends as such, not as an error.
    let done = app
        .run
        .replies
        .iter()
        .find(|reply| reply["id"] == 77)
        .expect("the turn ended")
        .clone();
    assert!(done.get("result").is_some(), "{done}");
    assert_eq!(done["result"]["stopped"]["complete"], true, "{done}");
    // Its task goes on, and the session is free meanwhile.
    let status = app.call("status", json!({"dir": "sw"}))["result"].clone();
    assert_eq!(
        (status["activity"].clone(), status["background"].clone()),
        (json!("background"), json!(1)),
        "{status}"
    );
    let (session, id) = held
        .borrow_mut()
        .take()
        .expect("the task's request was held");
    let (_, _, body) = wire(&Reply::Text("all quiet"), 9).unwrap();
    let mut frames = vec![];
    for part in body.as_bytes().chunks(64) {
        frames.extend(host.chunk(&session, id, part).0);
    }
    frames.extend(
        host.deliver(
            &session,
            id,
            answer_of(&json!({"kind": "response", "status": 200,
                "content_type": "text/event-stream"}))
            .unwrap(),
        )
        .unwrap(),
    );
    app.drive(frames);
    let tasks = app.call("tasks", json!({"dir": "sw"}))["result"]["tasks"].clone();
    assert_eq!(
        (tasks[0]["state"].clone(), tasks[0]["delivered"].clone()),
        (json!("finished"), json!(false)),
        "{tasks}"
    );
    // Its report waits for the session's next turn, which `continue` starts.
    let before = app.call("status", json!({"dir": "sw"}))["result"]["head"].clone();
    let requests = app.run.requests.len();
    let resumed = app.call("continue", json!({"dir": "sw"}));
    assert!(resumed.get("result").is_some(), "{resumed}");
    assert_eq!(app.run.requests.len(), requests + 1);
    assert!(app.run.requests.last().unwrap().contains("all quiet"));
    let tasks = app.call("tasks", json!({"dir": "sw"}))["result"]["tasks"].clone();
    assert_eq!(tasks[0]["delivered"], true, "{tasks}");
    // Gone back to before it was read, the report is read again at the next turn.
    let rewound = app.call("rewind", json!({"dir": "sw", "revision": before}));
    assert!(rewound.get("result").is_some(), "{rewound}");
    let tasks = app.call("tasks", json!({"dir": "sw"}))["result"]["tasks"].clone();
    assert_eq!(
        (tasks[0]["state"].clone(), tasks[0]["delivered"].clone()),
        (json!("finished"), json!(false)),
        "{tasks}"
    );
    app.call("continue", json!({"dir": "sw"}));
    assert!(app.run.requests.last().unwrap().contains("all quiet"));
    let tasks = app.call("tasks", json!({"dir": "sw"}))["result"]["tasks"].clone();
    assert_eq!(tasks[0]["delivered"], true, "{tasks}");
}

#[test]
fn stopping_the_task_a_turn_waits_for_tells_the_turn_so_and_it_goes_on() {
    let host = embedded(Box::new(MemoryPlaces));
    let (mut app, _held) = waiting_on_a_task(
        &host,
        "st",
        vec![
            Reply::Call(
                "task",
                json!({"prompt": "[t:st-task] look around", "background": true}),
            ),
            Reply::Text("waiting for it"),
            Reply::Text("it stopped, so I will look myself"),
        ],
    );
    let label = app.call("tasks", json!({"dir": "st"}))["result"]["tasks"][0]["task"]
        .as_str()
        .unwrap()
        .to_string();
    let stopped = app.call("task_stop", json!({"dir": "st", "task": label}));
    assert_eq!(stopped["result"]["stopping"], true, "{stopped}");
    let done = app
        .run
        .replies
        .iter()
        .find(|reply| reply["id"] == 77)
        .expect("the turn went on to its end")
        .clone();
    assert!(done.get("result").is_some(), "{done}");
    assert!(
        app.run.requests.last().unwrap().contains(": stopped]"),
        "the model heard it stopped: {}",
        app.run.requests.last().unwrap()
    );
    let tasks = app.call("tasks", json!({"dir": "st"}))["result"]["tasks"].clone();
    assert_eq!(
        (tasks[0]["state"].clone(), tasks[0]["delivered"].clone()),
        (json!("stopped"), json!(true)),
        "{tasks}"
    );
}

#[test]
fn a_task_that_cannot_run_here_is_refused_before_anything_is_kept() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script("tr", vec![Reply::Text("ready")]);
    // Tasks may use one model only, one the session's own is not.
    let created = app.call(
        "create",
        json!({"dir": "tr", "provider": "fake", "tasks": {"models": ["only-this"]}}),
    );
    assert!(created.get("result").is_some(), "{created}");
    app.call("prompt", json!({"dir": "tr", "text": "[t:tr] hello"}));
    for (task, input, why) in [
        (
            "t1",
            json!({"prompt": "x"}),
            "not permitted by tasks.models",
        ),
        (
            "t2",
            json!({"prompt": "x", "model": "other"}),
            "not permitted by tasks.models",
        ),
        (
            "t3",
            json!({"effort": "lots", "prompt": "x"}),
            "`effort` must be",
        ),
    ] {
        let mut params = json!({"dir": "tr", "task": task, "report": "queue"});
        params
            .as_object_mut()
            .unwrap()
            .extend(input.as_object().unwrap().clone());
        let refused = app.call("task_start", params);
        assert!(
            refused["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(why)),
            "{refused}"
        );
    }
    let tasks = app.call("tasks", json!({"dir": "tr"}))["result"]["tasks"].clone();
    assert_eq!(tasks, json!([]), "nothing kept, so nothing waits on it");
    let next = app.call("prompt", json!({"dir": "tr", "text": "[t:tr] fine?"}));
    assert!(next.get("result").is_some(), "{next}");
}

#[test]
fn an_app_task_queued_answers_at_once_and_its_report_follows() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host)
        .script("aq", vec![Reply::Text("ready"), Reply::Text("read it")])
        .script("#", vec![Reply::Text("queued answer")]);
    app.call(
        "create",
        json!({"dir": "aq", "provider": "fake", "tasks": true}),
    );
    app.call("prompt", json!({"dir": "aq", "text": "[t:aq] hello"}));
    let queued = app.call(
        "task_start",
        json!({"dir": "aq", "task": "later", "prompt": "[t:aq-task] look", "report": "queue"}),
    );
    assert_eq!(queued["result"]["queued"], true, "{queued}");
    assert!(
        queued["result"]["task"]
            .as_str()
            .unwrap()
            .starts_with("aq#")
    );
    let waited = app.call("task_wait", json!({"dir": "aq", "task": "later"}))["result"].clone();
    assert_eq!(waited["state"], "finished", "{waited}");
    assert!(
        waited["answer"]["content"]
            .as_str()
            .unwrap()
            .starts_with("queued answer")
    );
    // The same id again: the same task, already finished, nothing run twice.
    let again = app.call(
        "task_start",
        json!({"dir": "aq", "task": "later", "prompt": "[t:aq-task] look", "report": "queue"}),
    );
    assert_eq!(again["result"]["queued"], true, "{again}");
    let sent = app.run.requests.len();
    app.call("prompt", json!({"dir": "aq", "text": "[t:aq] anything?"}));
    assert!(
        app.run.requests[sent..]
            .iter()
            .any(|body| body.contains("queued answer")),
        "the next turn reads the report"
    );
}

#[test]
fn a_tasks_own_task_finishes_into_it_and_files_merge_through_both() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host)
        .script(
            "n",
            vec![
                Reply::Call("task", json!({"prompt": "delegate deeper"})),
                Reply::Text("parent done"),
            ],
        )
        .script(
            "#",
            vec![
                // The task, then its own task (served in this order).
                Reply::Call("task", json!({"prompt": "write deep.txt"})),
                Reply::Call(
                    "write_file",
                    json!({"path": "deep.txt", "content": "from the grandchild"}),
                ),
                Reply::Text("grandchild done"),
                Reply::Text("child done"),
            ],
        );
    app.call(
        "create",
        json!({"dir": "n", "provider": "fake", "tasks": true}),
    );
    let reply = app.call("prompt", json!({"dir": "n", "text": "[t:nested] go"}));
    assert!(reply.get("result").is_some(), "{reply}");
    assert_eq!(
        app.run.requests.len(),
        6,
        "parent, child, grandchild x2, child, parent"
    );
    assert!(
        app.run.requests[4].contains("grandchild done"),
        "the grandchild answers its task"
    );
    assert!(
        app.run.requests[5].contains("child done"),
        "the task answers the parent"
    );
    let read = app.call("read", json!({"dir": "n", "path": "deep.txt"}));
    assert_eq!(read["result"]["content"], "from the grandchild", "{read}");
}

/// The dollars a budget refusal names after `after` ("has spent ", "could cost up to ").
fn dollars_after(message: &str, after: &str) -> f64 {
    let rest = &message[message.find(after).unwrap_or_else(|| panic!("{message}")) + after.len()..];
    let rest = rest.trim_start_matches('$');
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    rest[..end].trim_end_matches('.').parse().unwrap()
}

#[test]
fn a_task_trees_spend_still_counts_against_its_budget_after_the_session_reopens() {
    let fake = fake();
    let mut catalog = catalog_json(&fake.base);
    // Every reply costs $1.10; the parent's next request could cost up to $5.23.
    catalog["providers"][0]["model_specs"]["fake-model"]["pricing"] = json!({
        "input_usd_per_mtok": 1000, "output_usd_per_mtok": 1000,
        "cache_read_usd_per_mtok": 1000, "cache_create_usd_per_mtok": 1000});
    let root = temp_root("budget-reopen");
    let host = Embedded::new(
        Box::new(DirPlaces(root.clone())),
        Catalog::from_json(&catalog).unwrap(),
    );
    let mut app = App::new(&host)
        .script("bt", vec![Reply::Text("ready")])
        .script(
            "#",
            vec![
                Reply::Call("task", json!({"prompt": "delegate deeper"})),
                Reply::Text("grandchild done"),
                Reply::Text("child done"),
            ],
        );
    // $9 fits the parent's $1.10, the task's $2.20 and a request, not its own task's
    // $1.10 too.
    app.call(
        "create",
        json!({"dir": "bt", "provider": "fake", "tasks": true, "max_usd": 9}),
    );
    app.call("prompt", json!({"dir": "bt", "text": "[t:bt] hello"}));
    let started = app.call(
        "task_start",
        json!({"dir": "bt", "task": "t", "prompt": "[t:bt-task] go"}),
    );
    assert_eq!(started["result"]["state"], "finished", "{started}");
    assert_eq!(app.run.requests.len(), 4, "parent, task, its task, task");
    let refused = |reply: &Value| {
        reply["error"]["message"]
            .as_str()
            .is_some_and(|message| message.starts_with("spending limit:"))
    };
    let before = app.call("prompt", json!({"dir": "bt", "text": "[t:bt] more"}));
    assert!(refused(&before), "{before}");
    // Only the nested task's $1.10 tips it (else this proves nothing about counting it).
    let said = before["error"]["message"].as_str().unwrap();
    assert!(
        dollars_after(said, "has spent ") + 2.20 + dollars_after(said, "could cost up to ") <= 9.0,
        "{said}"
    );
    // Closed and opened again, the ended tasks' spend still counts.
    app.call("close", json!({"dir": "bt"}));
    let after = app.call("continue", json!({"dir": "bt"}));
    assert!(refused(&after), "{after}");
    assert_eq!(app.run.requests.len(), 4, "nothing was sent");
    // A task whose spend cannot be read fails the turn under any budget, and costs
    // nothing without one.
    app.call("close", json!({"dir": "bt"}));
    let tasks = std::fs::read_dir(root.join("bt/tasks")).unwrap();
    let task = tasks.map(|task| task.unwrap().path()).next().unwrap();
    let nested = std::fs::read_dir(task.join("tasks")).unwrap();
    let nested = nested.map(|task| task.unwrap().path()).next().unwrap();
    std::fs::write(nested.join("store/authority.sqlite"), b"not a database").unwrap();
    app.call("settings", json!({"dir": "bt", "max_usd": 1000}));
    let unread = app.call("continue", json!({"dir": "bt"}));
    assert!(
        unread["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("cannot be read for the budget")),
        "{unread}"
    );
    app.call("settings", json!({"dir": "bt", "max_usd": null}));
    let unbudgeted = app.call("continue", json!({"dir": "bt"}));
    assert!(unbudgeted["error"].is_null(), "{unbudgeted}");
    assert_eq!(app.run.requests.len(), 5);
}

#[test]
fn a_budget_set_again_counts_what_tasks_spent_without_one() {
    let fake = fake();
    let mut catalog = catalog_json(&fake.base);
    // Every reply costs $1.10, as above.
    catalog["providers"][0]["model_specs"]["fake-model"]["pricing"] = json!({
        "input_usd_per_mtok": 1000, "output_usd_per_mtok": 1000,
        "cache_read_usd_per_mtok": 1000, "cache_create_usd_per_mtok": 1000});
    let root = temp_root("budget-again");
    let host = Embedded::new(
        Box::new(DirPlaces(root.clone())),
        Catalog::from_json(&catalog).unwrap(),
    );
    let mut app = App::new(&host)
        .script("ba", vec![Reply::Text("ready")])
        .script(
            "#",
            vec![
                Reply::Call("task", json!({"prompt": "delegate deeper"})),
                Reply::Text("grandchild done"),
                Reply::Text("child done"),
            ],
        );
    app.call(
        "create",
        json!({"dir": "ba", "provider": "fake", "tasks": true, "max_usd": 9}),
    );
    app.call("prompt", json!({"dir": "ba", "text": "[t:ba] hello"}));
    // No budget while the tasks run (they count nothing), then the same budget again,
    // in one open: what they spent counts.
    app.call("settings", json!({"dir": "ba", "max_usd": null}));
    let started = app.call(
        "task_start",
        json!({"dir": "ba", "task": "t", "prompt": "[t:ba-task] go"}),
    );
    assert_eq!(started["result"]["state"], "finished", "{started}");
    app.call("settings", json!({"dir": "ba", "max_usd": 9}));
    let sent = app.run.requests.len();
    let again = app.call("prompt", json!({"dir": "ba", "text": "[t:ba] more"}));
    let said = again["error"]["message"].as_str().unwrap_or_default();
    assert!(said.starts_with("spending limit:"), "{again}");
    // Only the nested task's $1.10 tips it (else this proves nothing about counting it).
    assert!(
        dollars_after(said, "has spent ") + 2.20 + dollars_after(said, "could cost up to ") <= 9.0,
        "{said}"
    );
    assert_eq!(app.run.requests.len(), sent, "nothing was sent");
}

#[test]
fn a_budget_set_while_a_background_task_runs_counts_what_it_spent() {
    let fake = fake();
    let mut catalog = catalog_json(&fake.base);
    catalog["providers"][0]["model_specs"]["fake-model"]["pricing"] = json!({
        "input_usd_per_mtok": 1000, "output_usd_per_mtok": 1000,
        "cache_read_usd_per_mtok": 1000, "cache_create_usd_per_mtok": 1000});
    let root = temp_root("budget-running");
    let host = Embedded::new(
        Box::new(DirPlaces(root.clone())),
        Catalog::from_json(&catalog).unwrap(),
    );
    let mut app = App::new(&host)
        .script("bb", vec![Reply::Text("ready"), Reply::Text("more")])
        .script(
            "#",
            vec![
                Reply::Call("bash", json!({"command": "true"})),
                Reply::Text("task done"),
            ],
        );
    // The task's command never answers: it keeps running in the background.
    app.runner = Box::new(|work, _| {
        if work["session"].as_str().unwrap().contains('#') {
            Value::Null
        } else {
            json!({"kind": "ran", "output": "", "success": true})
        }
    });
    app.call(
        "create",
        json!({"dir": "bb", "provider": "fake", "tasks": true, "runner": "app"}),
    );
    app.call("prompt", json!({"dir": "bb", "text": "[t:bb] hello"}));
    let queued = app.call(
        "task_start",
        json!({"dir": "bb", "task": "bg", "prompt": "[t:bb-task] go", "report": "queue"}),
    );
    assert_eq!(queued["result"]["queued"], true, "{queued}");
    assert_eq!(
        app.run.requests.len(),
        2,
        "the session's request and the task's"
    );
    // A budget set while it runs (none before): a tiny one says what the session spent
    // and what its next request could cost.
    app.call("settings", json!({"dir": "bb", "max_usd": 0.01}));
    let probe = app.call("prompt", json!({"dir": "bb", "text": "[t:bb] more"}));
    let message = probe["error"]["message"].as_str().unwrap_or_default();
    assert!(message.starts_with("spending limit:"), "{probe}");
    // Room for the session's own spend and its request, not for the task's $1.10 too.
    let budget =
        dollars_after(message, "has spent ") + dollars_after(message, "could cost up to ") + 0.05;
    app.call("settings", json!({"dir": "bb", "max_usd": budget}));
    let sent = app.run.requests.len();
    let line = json!({"jsonrpc": "2.0", "id": 77, "method": "continue", "params": {"dir": "bb"}});
    let frames = host.handle(line.to_string().as_bytes());
    app.drive(frames);
    assert_eq!(
        app.run.requests.len(),
        sent,
        "the running task's spend counts: nothing sent over a ${budget} budget"
    );
}

#[test]
fn an_unreadable_finished_tasks_file_offer_reports_the_error_without_waiting_forever() {
    let root = temp_root("finished-task-files");
    {
        let host = embedded(Box::new(DirPlaces(root.clone())));
        let mut app = App::new(&host).script("ft", vec![Reply::Text("ready")]);
        app.call(
            "create",
            json!({"dir": "ft", "provider": "fake", "tasks": true}),
        );
        app.call("prompt", json!({"dir": "ft", "text": "[t:ft] hello"}));
    }
    {
        let mut store = Store::open(&root.join("ft/store")).unwrap();
        let snapshot = store.snapshot().unwrap();
        let input = br#"{"prompt":"write a file"}"#;
        store
            .register_task(&spindle_store::meta::TaskRun {
                task: "app:writer".into(),
                origin: None,
                input: input.to_vec(),
                base: snapshot.workspace.unwrap(),
                revision: snapshot.revision,
                queued: true,
                state: "running".into(),
                answer: None,
                delivered: None,
                lease: None,
            })
            .unwrap();
        store
            .finish_task_run(
                "app:writer",
                input,
                spindle_store::meta::TaskEnd::Answered(br#"{"content":"written","failed":false}"#),
            )
            .unwrap();
    }
    let key = spindle_store::reads::task_name("app:writer");
    let child = root.join("ft/tasks").join(&key).join("store");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::write(child.join("authority.sqlite"), b"not a database").unwrap();
    let host = embedded(Box::new(DirPlaces(root.clone())));
    let mut app = App::new(&host).script("ft", vec![Reply::Text("received")]);
    let resumed = app.call("continue", json!({"dir": "ft"}));
    assert!(resumed.get("result").is_some(), "{resumed}");
    let request = app.run.requests.last().unwrap();
    assert!(request.contains("its files could not be read"), "{request}");
    let tasks = app.call("tasks", json!({"dir": "ft"}))["result"]["tasks"].clone();
    assert_eq!(tasks[0]["delivered"], true, "{tasks}");
    assert_eq!(tasks[0]["state"], "finished", "{tasks}");
    assert!(
        tasks[0]["answer"]["content"]
            .as_str()
            .unwrap()
            .starts_with(&format!("[task {key}:")),
        "{tasks}"
    );
}

#[test]
fn an_unreadable_task_store_leaves_background_tasks_to_resume_later() {
    let fake = fake();
    let mut catalog = catalog_json(&fake.base);
    catalog["providers"][0]["model_specs"]["fake-model"]["pricing"] = json!({
        "input_usd_per_mtok": 1, "output_usd_per_mtok": 1,
        "cache_read_usd_per_mtok": 1, "cache_create_usd_per_mtok": 1});
    let root = temp_root("budget-resume");
    let damaged_key;
    let priced = || {
        Embedded::new(
            Box::new(DirPlaces(root.clone())),
            Catalog::from_json(&catalog).unwrap(),
        )
    };
    {
        let host = priced();
        let mut app = App::new(&host)
            .script("bq", vec![Reply::Text("ready")])
            .script("#", vec![Reply::Text("done")]);
        app.call(
            "create",
            json!({"dir": "bq", "provider": "fake", "tasks": true, "max_usd": 1000}),
        );
        app.call("prompt", json!({"dir": "bq", "text": "[t:bq] hello"}));
        let done = app.call(
            "task_start",
            json!({"dir": "bq", "task": "done", "prompt": "[t:bq-done] go"}),
        );
        assert_eq!(done["result"]["state"], "finished", "{done}");
        let listed = app.call("tasks", json!({"dir": "bq"}))["result"]["tasks"].clone();
        damaged_key = listed[0]["task"]
            .as_str()
            .unwrap()
            .rsplit('#')
            .next()
            .unwrap()
            .to_string();
        // A background task's request goes and its answer never comes: the app closes.
        app.hold = 3;
        let queued = app.call(
            "task_start",
            json!({"dir": "bq", "task": "later", "prompt": "[t:bq-task] look", "report": "queue"}),
        );
        assert_eq!(queued["result"]["queued"], true, "{queued}");
    }
    // The finished task's store is damaged meanwhile.
    let damaged = root.join("bq/tasks").join(&damaged_key);
    std::fs::write(damaged.join("store/authority.sqlite"), b"not a database").unwrap();
    let host = priced();
    let mut app = App::new(&host).script("bq", vec![Reply::Text("more")]);
    let next = app.call("prompt", json!({"dir": "bq", "text": "[t:bq] again"}));
    assert!(
        next["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("cannot be read for the budget")),
        "{next}"
    );
    // Not stopped for it: it waits for an open that can count it.
    let listed = app.call("tasks", json!({"dir": "bq"}))["result"]["tasks"].clone();
    let later = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["prompt"] == "[t:bq-task] look")
        .unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(later["state"], "running", "{listed}");
    assert!(
        app.run.events.iter().any(|event| event["label"]
            .as_str()
            .is_some_and(|label| label.starts_with("background tasks not resumed yet: "))),
        "the thread says why: {:?}",
        app.run.events
    );
    // Without the budget the turn goes on and takes the task up (its request's outcome is
    // unknown, so it stops, saying why).
    app.call("settings", json!({"dir": "bq", "max_usd": null}));
    let resumed = app.call("continue", json!({"dir": "bq"}));
    assert!(resumed["error"].is_null(), "{resumed}");
    let listed = app.call("tasks", json!({"dir": "bq"}))["result"]["tasks"].clone();
    let later = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["prompt"] == "[t:bq-task] look")
        .unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(later["state"], "stopped", "taken up: {listed}");
}

#[test]
fn a_tasks_command_reads_the_tasks_own_files() {
    let host = embedded(Box::new(MemoryPlaces));
    let seen = std::cell::RefCell::new(None);
    let mut app = App::new(&host)
        .script(
            "tb",
            vec![
                Reply::Call("task", json!({"prompt": "look at seed.txt"})),
                Reply::Text("done"),
            ],
        )
        .script(
            "#",
            vec![
                Reply::Call("bash", json!({"command": "cat seed.txt"})),
                Reply::Text("child ran"),
            ],
        );
    app.runner = Box::new(|work, read| {
        let file = &work["files"][0];
        *seen.borrow_mut() = Some(read(file["object"].as_str().unwrap()));
        json!({"kind": "ran", "output": "seed", "success": true})
    });
    app.call(
        "create",
        json!({"dir": "tb", "provider": "fake", "tasks": true, "runner": "app",
            "files": [{"path": "seed.txt", "text": "seed"}]}),
    );
    host.set_runner(None);
    let reply = app.call("prompt", json!({"dir": "tb", "text": "[t:task-bash] go"}));
    assert!(reply.get("result").is_some(), "{reply}");
    drop(app);
    assert_eq!(seen.into_inner().as_deref(), Some(&b"seed"[..]));
}

#[test]
fn a_tasks_command_makes_script_calls_through_its_own_session() {
    let host = embedded(Box::new(MemoryPlaces));
    let seen = std::cell::RefCell::new(Value::Null);
    let mut app = App::new(&host)
        .script(
            "tc",
            vec![
                Reply::Call("task", json!({"prompt": "ask the limits"})),
                Reply::Text("done"),
            ],
        )
        .script(
            "#",
            vec![
                Reply::Call("bash", json!({"command": "spindle limits"})),
                Reply::Text("child ran"),
            ],
        );
    app.runner = Box::new(|work, _| {
        let label = work["session"].as_str().unwrap().to_string();
        let line = json!({"jsonrpc": "2.0", "id": 78, "method": "code_call",
            "params": {"dir": label, "request": {"method": "limits", "input": {}}}});
        let reply = host
            .handle(line.to_string().as_bytes())
            .into_iter()
            .find(|frame| frame["id"] == 78)
            .expect("a reply");
        *seen.borrow_mut() = json!({"label": label, "reply": reply});
        json!({"kind": "ran", "output": "ok", "success": true})
    });
    app.call(
        "create",
        json!({"dir": "tc", "provider": "fake", "tasks": true, "runner": "app"}),
    );
    let reply = app.call("prompt", json!({"dir": "tc", "text": "[t:task-code] go"}));
    assert!(reply.get("result").is_some(), "{reply}");
    drop(app);
    let seen = seen.into_inner();
    assert!(seen["label"].as_str().unwrap().starts_with("tc#"), "{seen}");
    assert_eq!(
        seen["reply"]["result"]["reply"]["data"]["calls"], 128,
        "a task's script reaches its own session: {seen}"
    );
}

#[test]
fn an_answer_that_does_not_fit_its_work_is_refused_and_the_work_still_waits() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script("fit", vec![Reply::Text("fine")]);
    app.hook = Some(Box::new(|embedded, session, id, _| {
        let refused = embedded.deliver(session, id, answer_of(&json!({"kind": "waited"})).unwrap());
        assert!(refused.is_err(), "a timer's answer is no response");
        let (_, streaming) = embedded.chunk(session, id + 1000, b"data: x\n\n");
        assert!(!streaming, "no such response streams");
        None
    }));
    app.call("create", json!({"dir": "fit", "provider": "fake"}));
    let reply = app.call("prompt", json!({"dir": "fit", "text": "[t:fit] hi"}));
    assert!(
        reply.get("result").is_some(),
        "the request went on: {reply}"
    );
}

#[test]
fn an_app_task_id_keeps_its_input_and_tasks_keep_to_their_models() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host)
        .script("id", vec![Reply::Text("ready")])
        .script("#", vec![Reply::Text("task answer")]);
    app.call(
        "create",
        json!({"dir": "id", "provider": "fake", "tasks": true}),
    );
    app.call("prompt", json!({"dir": "id", "text": "[t:id] hello"}));
    let first = app.call(
        "task_start",
        json!({"dir": "id", "task": "t1", "prompt": "[t:id-task] one"}),
    );
    assert!(first.get("result").is_some(), "{first}");
    let other = app.call(
        "task_start",
        json!({"dir": "id", "task": "t1", "prompt": "[t:id-task] another"}),
    );
    assert_eq!(kind(&other), Some("failed"), "{other}");
    assert!(
        other["error"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicts with its durable record"),
        "{other}"
    );
    let models = app.call(
        "settings",
        json!({"dir": "id", "tasks": {"models": ["some-other-model"]}}),
    );
    assert!(models.get("result").is_some(), "{models}");
    let refused = app.call(
        "task_start",
        json!({"dir": "id", "task": "t2", "prompt": "[t:id-task] two"}),
    );
    assert_eq!(kind(&refused), Some("failed"), "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not permitted by tasks.models"),
        "{refused}"
    );
}

#[test]
fn settings_the_package_cannot_honor_wait_for_a_change_before_a_turn() {
    let root = temp_root("native-settings");
    {
        let host = embedded(Box::new(DirPlaces(root.clone())));
        let mut app = App::new(&host).script("m", vec![Reply::Text("first")]);
        app.call("create", json!({"dir": "m", "provider": "fake"}));
        let first = app.call("prompt", json!({"dir": "m", "text": "[t:moved] one"}));
        assert!(first.get("result").is_some(), "{first}");
    }
    // As a native host keeps them by default: every built-in tool (and model
    // compaction, which the package honors), with a budget.
    {
        let mut store = Store::open(&root.join("m/store")).unwrap();
        let mut settings: Value =
            serde_json::from_slice(&store.metadata("settings").unwrap().unwrap()).unwrap();
        settings["builtin_tools"] = json!("all");
        settings["compaction"] = json!("model");
        settings["max_usd"] = json!(1.5);
        store
            .set_metadata("settings", &serde_json::to_vec_pretty(&settings).unwrap())
            .unwrap();
    }
    let host = embedded(Box::new(DirPlaces(root.clone())));
    let mut app = App::new(&host).script("m", vec![Reply::Text("second")]);
    let refused = app.call("prompt", json!({"dir": "m", "text": "[t:moved] two"}));
    assert_eq!(kind(&refused), Some("failed"), "{refused}");
    let message = refused["error"]["message"].as_str().unwrap();
    for named in ["settings cannot run here", "`builtin_tools`"] {
        assert!(
            message.contains(named),
            "every one is named ({named}): {message}"
        );
    }
    assert!(!message.contains("compaction"), "{message}");
    assert!(
        !message.contains("max_usd"),
        "a budget is kept here: {message}"
    );
    assert!(app.run.requests.is_empty(), "nothing was sent");
    let history = app.call("history", json!({"dir": "m"}));
    assert!(
        history.get("result").is_some(),
        "reads still work: {history}"
    );
    // The package says what to change, in one settings call, and why.
    let fit = app.call("fit", json!({"dir": "m"}))["result"].clone();
    assert_eq!(
        fit["changes"],
        json!({"builtin_tools": "files", "runner": "app"}),
        "{fit}"
    );
    let noted: Vec<&str> = fit["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|note| note["setting"].as_str().unwrap())
        .collect();
    assert_eq!(noted, ["builtin_tools", "runner", "max_usd"], "{fit}");
    assert_eq!(fit["refused"], json!([]));
    let mut changes = fit["changes"].clone();
    changes["dir"] = json!("m");
    let changed = app.call("settings", changes);
    assert_eq!(
        changed["result"]["max_usd"], 1.5,
        "the budget carries over: {changed}"
    );
    assert_eq!(
        app.call("fit", json!({"dir": "m"}))["result"]["changes"],
        json!({}),
        "nothing more to change"
    );
    let capabilities = app.call("capabilities", json!({}))["result"].clone();
    assert_eq!(capabilities["host"], "embedded");
    assert_eq!(capabilities["budget"], true);
    assert_eq!(capabilities["builtin_tools"], json!(["files", "none"]));
    // Its model has no price, so under a budget nothing is sent, as natively.
    let unpriced = app.call("prompt", json!({"dir": "m", "text": "[t:moved] two"}));
    assert!(
        unpriced["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("no price for fake/fake-model")),
        "{unpriced}"
    );
    assert!(app.run.requests.is_empty());
    let cleared = app.call("settings", json!({"dir": "m", "max_usd": null}));
    assert_eq!(cleared["result"]["max_usd"], Value::Null, "{cleared}");
    let second = app.call("continue", json!({"dir": "m"}));
    assert!(second.get("result").is_some(), "{second}");
    assert_eq!(app.run.requests.len(), 1);
}

fn quiet() -> Arc<Recorder> {
    Arc::new(Recorder {
        events: Mutex::new(vec![]),
        app: None,
    })
}

fn image_facts(store: &Path) -> spindle_store::ImageFacts {
    spindle_store::check_image(&Store::open(store).unwrap().image().unwrap()).unwrap()
}

#[test]
fn a_session_moves_by_bundle_keeping_its_identity_and_unknown_and_sealing_its_source() {
    let root = temp_root("bundle-move");
    // A session whose second request was sent and its response lost (a crash).
    {
        let host = embedded(Box::new(DirPlaces(root.clone())));
        let mut app = App::new(&host).script("src", vec![Reply::Text("first")]);
        app.call("create", json!({"dir": "src", "provider": "fake"}));
        app.call("prompt", json!({"dir": "src", "text": "[t:moving] one"}));
        app.hold = 2;
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": "prompt",
            "params": {"dir": "src", "text": "[t:moving] two"}});
        let frames = host.handle(line.to_string().as_bytes());
        app.drive(frames);
        assert_eq!(app.run.requests.len(), 2);
    }
    let before = image_facts(&root.join("src/store"));
    // A native host moves it out.
    let bundle = root.join("moved.bundle");
    let mut native = Session::open(&root.join("src"), quiet()).unwrap();
    let exported = native.export_session(&bundle, false).unwrap();
    assert_eq!(
        (exported["moved"].clone(), exported["head"].clone()),
        (json!(true), json!(before.head))
    );
    assert_eq!(exported["warnings"], json!([]), "every copy here sealed");
    drop(native);
    // Its old place refuses every host now, saying where it went.
    let refused = Session::open(&root.join("src"), quiet()).err().unwrap();
    assert!(refused.contains("moved elsewhere"), "{refused}");
    let note = spindle_store::fence_note(&root.join("src/store")).unwrap();
    assert!(note.starts_with(spindle_store::MOVED_FENCE), "{note}");
    let host = embedded(Box::new(DirPlaces(root.clone())));
    let mut app = App::new(&host);
    let status = app.call("status", json!({"dir": "src"}));
    assert!(status.get("error").is_some(), "{status}");
    // Imported elsewhere it is the same session, its unknown still unknown.
    let imported = agent::import_session(&bundle, &root.join("dst")).unwrap();
    assert_eq!(imported["identity"], json!(before.identity));
    let after = image_facts(&root.join("dst/store"));
    assert_eq!(
        (after.head, &after.identity),
        (before.head, &before.identity)
    );
    assert!(
        root.join("dst/session.json").is_file(),
        "a native session directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(root.join("dst"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "private, as native session directories are");
    }
    let host = embedded(Box::new(DirPlaces(root.clone())));
    let mut app = App::new(&host).script("dst", vec![Reply::Text("sent once")]);
    let continued = app.call("continue", json!({"dir": "dst"}));
    assert_eq!(kind(&continued), Some("failed"), "{continued}");
    assert!(
        app.run.requests.is_empty(),
        "continue never resends an unknown request"
    );
    let retried = app.call("retry", json!({"dir": "dst"}));
    assert!(retried.get("result").is_some(), "{retried}");
    assert_eq!(app.run.requests.len(), 1, "retry sends it once");
    drop(app);
    drop(host);
    // Into a place that holds anything, an import is refused.
    let refused = agent::import_session(&bundle, &root.join("dst"))
        .err()
        .unwrap();
    assert!(refused.contains("not empty"), "{refused}");
    // `keep`: a copy; the original stays a session (the bundle names the same one).
    let copy = root.join("copy.bundle");
    let mut native = Session::open(&root.join("dst"), quiet()).unwrap();
    let kept = native.export_session(&copy, true).unwrap();
    assert_eq!(kept["moved"], false);
    let again = native.export_session(&copy, true).err().unwrap();
    assert!(
        again.contains("already exists"),
        "never overwrites a file: {again}"
    );
    drop(native);
    assert!(Session::open(&root.join("dst"), quiet()).is_ok());
}

#[test]
fn an_export_can_carry_settings_for_where_it_goes_and_the_session_keeps_its_own() {
    let root = temp_root("export-settings");
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script("m", vec![Reply::Text("an answer")]);
    app.call(
        "create",
        json!({"dir": "m", "provider": "fake", "builtin_tools": "files", "runner": "app"}),
    );
    app.call("prompt", json!({"dir": "m", "text": "[t:settings] one"}));
    // Settings this app refuses for itself, for a native host to run it with.
    let native_only = json!({"builtin_tools": "all", "runner": "none", "max_usd": 2.5});
    let unknown = host
        .export_session("m", true, Some(&json!({"bogus": 1})))
        .unwrap_err();
    assert_eq!(unknown.kind, "bad_request", "{}", unknown.message);
    let invalid = host
        .export_session("m", true, Some(&json!({"builtin_tools": "most"})))
        .unwrap_err();
    assert_eq!(invalid.kind, "bad_request", "{}", invalid.message);
    let bytes = host.export_session("m", true, Some(&native_only)).unwrap();
    // The copy kept here has its own settings still.
    let kept = app.call("settings", json!({"dir": "m"}))["result"].clone();
    assert_eq!(kept["builtin_tools"], "files", "{kept}");
    assert_eq!(kept["runner"], "app", "{kept}");
    // The native host reads the bundle's.
    let file = root.join("settings.bundle");
    std::fs::write(&file, &bytes).unwrap();
    agent::import_session(&file, &root.join("native")).unwrap();
    let native = Session::open(&root.join("native"), quiet()).unwrap();
    let settings = native.settings().unwrap();
    assert_eq!(settings.builtin_tools, agent::BuiltinTools::All);
    assert_eq!(settings.runner, spindle_store::driver::Runner::None);
    assert_eq!(settings.max_usd, Some(2.5));
    let mirror: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("native/session.json")).unwrap()).unwrap();
    assert_eq!(
        mirror,
        settings.to_json().unwrap(),
        "its native mirror agrees"
    );
}

#[test]
fn the_package_and_a_native_host_exchange_session_bundles_both_ways() {
    let root = temp_root("exchange");
    // A memory session in the package, with an app-started task registered.
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host)
        .script("m", vec![Reply::Text("first answer")])
        .script("#", vec![Reply::Text("task answer")]);
    app.call(
        "create",
        json!({"dir": "m", "provider": "fake", "tasks": true}),
    );
    app.call("prompt", json!({"dir": "m", "text": "[t:exchange] one"}));
    let task = app.call(
        "task_start",
        json!({"dir": "m", "task": "t1", "prompt": "[t:exchange-task] look"}),
    );
    assert!(task.get("result").is_some(), "{task}");
    let head = app.call("status", json!({"dir": "m"}))["result"]["head"].clone();
    let busy = host.export_session("nowhere", false, None);
    assert!(busy.is_err(), "no such session");
    let bytes = host.export_session("m", false, None).unwrap();
    let gone = app.call("status", json!({"dir": "m"}));
    assert!(gone.get("error").is_some(), "moved out of memory: {gone}");
    // A native host continues it from the bundle: the same head, its task registered.
    let file = root.join("from-package.bundle");
    std::fs::write(&file, &bytes).unwrap();
    let imported = agent::import_session(&file, &root.join("native")).unwrap();
    assert_eq!(imported["head"], head);
    let mut native = Session::open(&root.join("native"), quiet()).unwrap();
    assert_eq!(json!(native.status().unwrap().revision), head);
    let back = root.join("from-native.bundle");
    native.export_session(&back, false).unwrap();
    drop(native);
    // And back into the package, where it goes on.
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script("again", vec![Reply::Text("second answer")]);
    let bytes = std::fs::read(&back).unwrap();
    let mut damaged = bytes.clone();
    let middle = damaged.len() / 2;
    damaged[middle] ^= 1;
    let refused = host.import_session("again", &damaged).unwrap_err();
    assert_eq!(refused.kind, "bad_request", "{}", refused.message);
    let status = host.import_session("again", &bytes).unwrap();
    assert_eq!(status["head"], head);
    assert_eq!(status["imported"]["head"], head);
    let conflict = host.import_session("again", &bytes).unwrap_err();
    assert_eq!(conflict.kind, "conflict");
    let reply = app.call(
        "prompt",
        json!({"dir": "again", "text": "[t:exchange] two"}),
    );
    assert!(reply.get("result").is_some(), "{reply}");
    assert!(
        app.run.requests[0].contains("first answer"),
        "the moved conversation reaches the model"
    );
    let again = app.call(
        "task_start",
        json!({"dir": "again", "task": "t1", "prompt": "[t:exchange-task] other"}),
    );
    assert!(
        again["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("conflicts with its durable record")),
        "the task's identity moved with it: {again}"
    );
}

#[test]
fn required_isolation_never_hands_a_command_to_a_runner_that_does_not_isolate() {
    let host = embedded(Box::new(MemoryPlaces));
    let ran = std::cell::Cell::new(false);
    let mut app = App::new(&host).script(
        "iso",
        vec![
            Reply::Call("bash", json!({"command": "true"})),
            Reply::Text("no"),
        ],
    );
    app.runner = Box::new(|_, _| {
        ran.set(true);
        json!({"kind": "ran", "output": "", "success": true})
    });
    let created = app.call(
        "create",
        json!({"dir": "iso", "provider": "fake", "runner": "app"}),
    );
    assert_eq!(
        created["result"]["isolation"], "auto",
        "new sessions: {created}"
    );
    app.call("settings", json!({"dir": "iso", "isolation": "required"}));
    host.set_runner(None);
    let refused = app.call("prompt", json!({"dir": "iso", "text": "[t:iso] run"}));
    assert_eq!(kind(&refused), Some("failed"), "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("isolation is required")),
        "{refused}"
    );
    drop(app);
    assert!(!ran.get(), "the app's runner was never asked");
}

#[cfg(unix)]
#[test]
fn an_export_never_follows_a_link_out_of_the_session() {
    let root = temp_root("export-link");
    {
        let host = embedded(Box::new(DirPlaces(root.clone())));
        let mut app = App::new(&host).script("ln", vec![Reply::Text("hello")]);
        app.call("create", json!({"dir": "ln", "provider": "fake"}));
        app.call("prompt", json!({"dir": "ln", "text": "[t:link] hi"}));
    }
    // A task directory that is a link to somewhere else entirely.
    std::fs::create_dir_all(root.join("elsewhere/store")).unwrap();
    std::fs::create_dir_all(root.join("ln/tasks")).unwrap();
    std::os::unix::fs::symlink(
        root.join("elsewhere"),
        root.join("ln/tasks/0123456789abcdef0123"),
    )
    .unwrap();
    let target = root.join("ln.bundle");
    let mut native = Session::open(&root.join("ln"), quiet()).unwrap();
    let refused = native.export_session(&target, true).err().unwrap();
    assert!(refused.contains("not a regular directory"), "{refused}");
    assert!(!target.exists(), "nothing written");
}

#[test]
fn the_app_is_told_where_its_key_goes_never_what_it_is() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script("auth", vec![Reply::Text("ok")]);
    app.call("create", json!({"dir": "auth", "provider": "fake"}));
    let reply = app.call("prompt", json!({"dir": "auth", "text": "[t:auth] hi"}));
    assert!(reply.get("result").is_some(), "{reply}");
    let authorize = app
        .run
        .works
        .iter()
        .find(|work| work["kind"] == "authorize")
        .unwrap()
        .clone();
    // Derived from the provider's catalog entry (an OpenAI-style one here): a header
    // template with the key's place marked, and no authentication among the others.
    assert_eq!(authorize["key_slot"], "{key}");
    assert_eq!(
        authorize["auth"],
        json!({"authorization": "Bearer {key}"}),
        "{authorize}"
    );
    let headers = authorize["headers"].as_object().unwrap();
    assert!(
        headers
            .values()
            .all(|value| !value.as_str().unwrap().contains("{key}")),
        "{authorize}"
    );
}

#[test]
fn a_sign_in_whose_headers_read_its_token_is_filled_by_the_app_from_that_token() {
    let fake = fake();
    let mut catalog = catalog_json(&fake.base);
    catalog["providers"].as_array_mut().unwrap().push(json!({
        "id": "subscription", "display_name": "Subscription", "api_provider": "chatgpt",
        "request_contract": "chatgpt-responses", "base_url": fake.base,
        "default_model": "sub-model", "models": ["sub-model"], "requires_api_key": true}));
    let host = Embedded::new(
        Box::new(MemoryPlaces),
        Catalog::from_json(&catalog).unwrap(),
    );
    let mut app = App::new(&host);
    app.call("create", json!({"dir": "sub", "provider": "subscription"}));
    // The first thing the turn asks for is the credentials, before anything is reserved.
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "prompt",
        "params": {"dir": "sub", "text": "hi"}});
    let frames = host.handle(line.to_string().as_bytes());
    let authorize = frames
        .iter()
        .find(|frame| frame["method"] == "work" && frame["params"]["work"]["kind"] == "authorize")
        .unwrap_or_else(|| panic!("no authorize work: {frames:?}"))["params"]["work"]
        .clone();
    // The ChatGPT backend reads the account from the token: the app fills that slot from
    // the claim, as it fills the key's, and the package never holds either.
    assert_eq!(
        authorize["auth"],
        json!({"authorization": "Bearer {key}", "chatgpt-account-id": "{account}"}),
        "{authorize}"
    );
    assert_eq!(
        authorize["claims"],
        json!([{"slot": "{account}", "path": ["https://api.openai.com/auth", "chatgpt_account_id"]}])
    );
    let headers = authorize["headers"].as_object().unwrap();
    assert_eq!(
        headers["openai-beta"], "responses=experimental",
        "{authorize}"
    );
    assert!(
        headers.values().all(|value| {
            let value = value.as_str().unwrap();
            !value.contains("{key}") && !value.contains("{account}")
        }),
        "{authorize}"
    );
    // A key placed as it is reads no claim.
    let plain = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&plain).script("plain", vec![Reply::Text("ok")]);
    app.call("create", json!({"dir": "plain", "provider": "fake"}));
    app.call("prompt", json!({"dir": "plain", "text": "[t:plain] hi"}));
    let authorize = app
        .run
        .works
        .iter()
        .find(|work| work["kind"] == "authorize")
        .unwrap();
    assert_eq!(authorize["claims"], json!([]), "{authorize}");
}

// ---- compaction by a model-written summary ----------------------------------------------

const SUMMARY_SYSTEM: &str = "You summarize the earlier part of a coding session";
const SUMMARY_PROMPT: &str = "Summarize the session history below";
const COMPACTED: &str = "[Earlier conversation compacted by spindle";
/// A summary of the length the request asks for (a thin one keeps the digest beside it:
/// driver `THIN_SUMMARY`).
const SUMMARY: &str = concat!(
    "SUMMARY: three long answers were given; carry on with four.",
    " The user asked for three long answers in turn, each written out in full; all three are done.",
    " The next step is the fourth answer, in the same style and at the same length as the others.",
    " Nothing failed, no file was written, no command ran, and nothing is waiting on the user.",
    " Keep the same tone; number the answer four; do not repeat the earlier ones word for word.",
    " The user prefers plain paragraphs with no headings and no lists, and the same opening line.",
    " Earlier answers each began by naming their number, then went on for several paragraphs.",
    " There were no open questions and no decisions left to make; the plan has not changed since.",
    " Each answer so far was accepted as written; the user did not ask for any change to any.",
    " If anything is unclear, the workspace holds no files; the conversation alone carries it.",
    " Carry on with the fourth answer now, then stop and wait for the user's next message.",
    " The user asked for three long answers in turn, each written out in full; all three are done.",
    " The next step is the fourth answer, in the same style and at the same length as the others.",
    " Nothing failed, no file was written, no command ran, and nothing is waiting on the user.",
    " Keep the same tone; number the answer four; do not repeat the earlier ones word for word.",
    " The user prefers plain paragraphs with no headings and no lists, and the same opening line.",
    " Earlier answers each began by naming their number, then went on for several paragraphs.",
    " There were no open questions and no decisions left to make; the plan has not changed since.",
    " Each answer so far was accepted as written; the user did not ask for any change to any.",
    " If anything is unclear, the workspace holds no files; the conversation alone carries it.",
    " Carry on with the fourth answer now, then stop and wait for the user's next message.",
    " The user asked for three long answers in turn, each written out in full; all three are done.",
    " The next step is the fourth answer, in the same style and at the same length as the others.",
    " Nothing failed, no file was written, no command ran, and nothing is waiting on the user.",
);

/// A reply of about 45 KB: three of them take `small-model`'s history past what one
/// compaction must drop, so the fourth prompt compacts first.
fn long(tag: &str) -> &'static str {
    Box::leak(format!("{tag} {}", "lorem ipsum dolor sit amet ".repeat(1700)).into_boxed_str())
}

/// Three long answers, then what the summary request (the fourth) answers, then the
/// fourth turn's reply.
fn compacting_script(summary: Reply) -> Vec<Reply> {
    vec![
        Reply::Text(long("first")),
        Reply::Text(long("second")),
        Reply::Text(long("third")),
        summary,
        Reply::Text("done"),
    ]
}

const TURNS: [&str; 4] = ["one", "two", "three", "four"];

/// Runs prompts natively, one turn each: the requests the fake received and the events.
fn native_turns(
    name: &str,
    script: Vec<Reply>,
    prepare: impl FnOnce(&mut Settings),
    prompts: &[&str],
) -> (Vec<String>, Vec<Value>) {
    let fake = fake();
    fake.scripts
        .lock()
        .unwrap()
        .insert(name.into(), (script, vec![]));
    let recorder = Arc::new(Recorder {
        events: Mutex::default(),
        app: None,
    });
    let mut settings = Settings::resolve("fake", Some("small-model".into())).unwrap();
    settings.isolation = agent::Isolation::Off;
    settings.builtin_tools = agent::BuiltinTools::Files;
    prepare(&mut settings);
    let mut session =
        Session::create(&fake.root.join(name), settings, vec![], recorder.clone()).unwrap();
    for prompt in prompts {
        session.prompt(&format!("[t:{name}] {prompt}")).unwrap();
    }
    let requests = fake.scripts.lock().unwrap()[name].1.clone();
    let events = recorder.events.lock().unwrap().clone();
    (requests, events)
}

/// Runs the same turns natively and in the package (on `small-model`, with `settings`
/// added to the package's new session): both must send the same requests and report
/// the same events. Returns the package's run.
fn compaction_parity(
    name: &str,
    script: Vec<Reply>,
    settings: Value,
    prepare: impl FnOnce(&mut Settings),
) -> Run {
    let (requests, events) = native_turns(name, script.clone(), prepare, &TURNS);
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script(name, script);
    app.part = 4096;
    let mut create = json!({"dir": name, "provider": "fake", "model": "small-model"});
    create
        .as_object_mut()
        .unwrap()
        .extend(settings.as_object().unwrap().clone());
    let created = app.call("create", create);
    assert!(created.get("result").is_some(), "{created}");
    for prompt in TURNS {
        let reply = app.call(
            "prompt",
            json!({"dir": name, "text": format!("[t:{name}] {prompt}")}),
        );
        assert!(reply.get("result").is_some(), "{reply}");
    }
    assert_eq!(
        app.run.requests.len(),
        requests.len(),
        "{name}: request count"
    );
    for (index, (embedded, native)) in app.run.requests.iter().zip(&requests).enumerate() {
        assert_eq!(embedded, native, "{name}: request {index} differs");
    }
    assert_eq!(
        shared_events(&app.run.events),
        shared_events(&events),
        "{name}: events differ"
    );
    std::mem::take(&mut app.run)
}

fn count(events: &[Value], kind: &str) -> usize {
    events.iter().filter(|event| event["type"] == kind).count()
}

#[test]
fn a_long_history_is_compacted_by_a_summary_the_sessions_model_writes() {
    // The package's new session compacts by model unless told otherwise.
    let run = compaction_parity(
        "summary",
        compacting_script(Reply::Text(SUMMARY)),
        json!({}),
        |settings| settings.compaction = agent::Compaction::Model,
    );
    let summaries: Vec<&String> = run
        .requests
        .iter()
        .filter(|request| request.contains(SUMMARY_SYSTEM))
        .collect();
    assert_eq!(summaries.len(), 1, "one summary request");
    assert!(
        run.requests[3].contains(SUMMARY_PROMPT) && run.requests[3].contains("first lorem"),
        "the fourth request asks for the summary of the history"
    );
    let last = &run.requests[4];
    assert!(
        last.contains(COMPACTED) && last.contains(SUMMARY) && !last.contains("first lorem"),
        "the summary stands in for the history it covers"
    );
    assert_eq!(count(&run.events, "compacted"), 1);
    assert_eq!(
        count(&run.events, "usage"),
        5,
        "the summary's usage is reported"
    );
}

#[test]
fn a_refused_or_lost_summary_request_leaves_the_digest_and_the_turn_goes_on() {
    for (name, summary) in [
        ("refused-summary", Reply::Refused),
        ("lost-summary", Reply::HangUp),
    ] {
        let run = compaction_parity(name, compacting_script(summary), json!({}), |settings| {
            settings.compaction = agent::Compaction::Model
        });
        assert_eq!(
            run.requests.len(),
            5,
            "{name}: the summary is asked for once"
        );
        assert!(run.requests[3].contains(SUMMARY_PROMPT), "{name}");
        let last = &run.requests[4];
        assert!(
            last.contains(COMPACTED) && last.contains("User asked: [t:"),
            "{name}: the digest stands in"
        );
        assert_eq!(count(&run.events, "compacted"), 1, "{name}");
        assert_eq!(count(&run.events, "usage"), 4, "{name}: no summary usage");
    }
}

#[test]
fn compaction_by_digest_sends_no_summary_request() {
    let script = vec![
        Reply::Text(long("first")),
        Reply::Text(long("second")),
        Reply::Text(long("third")),
        Reply::Text("done"),
    ];
    let run = compaction_parity(
        "digest-only",
        script,
        json!({"compaction": "digest"}),
        |settings| settings.compaction = agent::Compaction::Digest,
    );
    assert_eq!(run.requests.len(), 4);
    assert!(run.requests.iter().all(|r| !r.contains(SUMMARY_SYSTEM)));
    assert!(run.requests[3].contains(COMPACTED) && run.requests[3].contains("User asked: [t:"));
    assert_eq!(count(&run.events, "compacted"), 1);
}

/// Three long turns of session `dir` (on `small-model`) in `app`, so that the next
/// prompt compacts first.
fn three_long_turns(app: &mut App<'_>, dir: &str) {
    let created = app.call(
        "create",
        json!({"dir": dir, "provider": "fake", "model": "small-model"}),
    );
    assert!(created.get("result").is_some(), "{created}");
    for prompt in &TURNS[..3] {
        let reply = app.call(
            "prompt",
            json!({"dir": dir, "text": format!("[t:{dir}] {prompt}")}),
        );
        assert!(reply.get("result").is_some(), "{reply}");
    }
}

/// The text of the history's first message, where a compaction puts its summary.
fn compacted_text(app: &mut App<'_>, dir: &str) -> String {
    let history = app.call("history", json!({"dir": dir}));
    history["result"]["messages"][0].to_string()
}

#[test]
fn a_summary_request_lost_to_a_crash_is_never_sent_again_and_the_digest_stands_in() {
    let root = temp_root("summary-lost");
    {
        let host = embedded(Box::new(DirPlaces(root.clone())));
        let mut app = App::new(&host).script(
            "s",
            vec![
                Reply::Text(long("first")),
                Reply::Text(long("second")),
                Reply::Text(long("third")),
            ],
        );
        app.part = 4096;
        three_long_turns(&mut app, "s");
        app.hold = 4;
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": "prompt",
            "params": {"dir": "s", "text": "[t:s] four"}});
        let frames = host.handle(line.to_string().as_bytes());
        app.drive(frames);
        assert_eq!(app.run.requests.len(), 4);
        assert!(app.run.requests[3].contains(SUMMARY_PROMPT));
        assert!(app.run.replies.is_empty(), "the turn waits on the summary");
        // The app (and its process) end here, with the summary never answered.
    }
    let host = embedded(Box::new(DirPlaces(root.clone())));
    let mut app = App::new(&host).script("s", vec![Reply::Text("done")]);
    let reply = app.call("prompt", json!({"dir": "s", "text": "[t:s] four"}));
    assert!(reply.get("result").is_some(), "{reply}");
    assert_eq!(app.run.requests.len(), 1, "only the turn's request is sent");
    assert!(
        app.run.requests[0].contains(COMPACTED) && app.run.requests[0].contains("User asked: [t:"),
        "the digest stands in"
    );
    assert_eq!(count(&app.run.events, "compacted"), 1);
    assert_eq!(count(&app.run.events, "usage"), 1);
}

#[test]
fn a_recorded_summary_whose_compaction_was_lost_is_used_after_a_reopen() {
    let root = temp_root("summary-kept");
    let database = root.join("s/store/authority.sqlite");
    {
        let host = embedded(Box::new(DirPlaces(root.clone())));
        let mut app = App::new(&host).script(
            "s",
            vec![
                Reply::Text(long("first")),
                Reply::Text(long("second")),
                Reply::Text(long("third")),
                Reply::Text(SUMMARY),
            ],
        );
        app.part = 4096;
        three_long_turns(&mut app, "s");
        // The summary's reply is recorded, then its compaction cannot be kept: as if
        // the process ended between the two.
        let trigger = database.clone();
        app.hook = Some(Box::new(move |_, _, _, index| {
            if index == 4 {
                let db = rusqlite::Connection::open(&trigger).unwrap();
                db.execute_batch(
                    "CREATE TRIGGER fail_compact BEFORE INSERT ON admissions \
                     WHEN NEW.command_id LIKE 'compact-%' BEGIN SELECT RAISE(ABORT,'injected'); END;",
                )
                .unwrap();
            }
            None
        }));
        let failed = app.call("prompt", json!({"dir": "s", "text": "[t:s] four"}));
        assert!(failed.get("error").is_some(), "{failed}");
        assert_eq!(app.run.requests.len(), 4);
        assert!(app.run.requests[3].contains(SUMMARY_PROMPT));
        assert_eq!(
            count(&app.run.events, "usage"),
            4,
            "the summary's usage, once"
        );
        assert_eq!(count(&app.run.events, "compacted"), 0);
    }
    let db = rusqlite::Connection::open(&database).unwrap();
    db.execute_batch("DROP TRIGGER fail_compact").unwrap();
    drop(db);
    let host = embedded(Box::new(DirPlaces(root.clone())));
    let mut app = App::new(&host).script("s", vec![Reply::Text("done")]);
    let reply = app.call("prompt", json!({"dir": "s", "text": "[t:s] four"}));
    assert!(reply.get("result").is_some(), "{reply}");
    assert_eq!(
        app.run.requests.len(),
        1,
        "the summary is not asked for again"
    );
    assert!(
        app.run.requests[0].contains(COMPACTED) && app.run.requests[0].contains(SUMMARY),
        "the recorded summary stands in"
    );
    assert_eq!(
        count(&app.run.events, "usage"),
        1,
        "counted once, when it came"
    );
    assert_eq!(count(&app.run.events, "compacted"), 1);
}

#[test]
fn a_stop_during_the_summary_request_leaves_the_digest_and_stops_the_turn() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script(
        "x",
        vec![
            Reply::Text(long("first")),
            Reply::Text(long("second")),
            Reply::Text(long("third")),
        ],
    );
    app.part = 4096;
    three_long_turns(&mut app, "x");
    app.hold = 4;
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "prompt",
        "params": {"dir": "x", "text": "[t:x] four"}});
    let frames = host.handle(line.to_string().as_bytes());
    app.drive(frames);
    assert!(app.run.requests[3].contains(SUMMARY_PROMPT));
    let stopped = app.call("stop", json!({"dir": "x"}));
    assert_eq!(stopped["result"]["stopping"], true, "{stopped}");
    let ended = app
        .run
        .replies
        .iter()
        .find(|r| r["id"] == 1)
        .unwrap()
        .clone();
    assert_eq!(kind(&ended), Some("stopped"), "{ended}");
    assert_eq!(app.run.requests.len(), 4, "nothing more is sent");
    assert!(
        app.run.cancelled.iter().any(|c| c["session"] == "x"),
        "the app is told to end the summary request"
    );
    let first = compacted_text(&mut app, "x");
    assert!(
        first.contains(COMPACTED) && first.contains("User asked: [t:"),
        "the digest stands in: {first}"
    );
}

#[test]
fn a_reply_that_cannot_be_read_says_what_was_wrong() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host);
    for dir in ["why1", "why2", "why3"] {
        app.call("create", json!({"dir": dir, "provider": "fake"}));
    }
    let began = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hel\"},\"finish_reason\":null}]}\n\n";
    app.hook = Some(Box::new(move |host, session, id, index| {
        let mut frames = vec![];
        match index {
            // The provider's own error, streamed after the reply began.
            1 => {
                let stream = format!(
                    "{began}data: {{\"error\":{{\"code\":\"server_error\",\"message\":\"overloaded, try again\"}}}}\n\n"
                );
                frames.extend(host.chunk(session, id, stream.as_bytes()).0);
            }
            // A 200 that is not an event stream.
            2 => {}
            // A stream that stops before it is complete.
            _ => frames.extend(host.chunk(session, id, began.as_bytes()).0),
        }
        if index >= 2 {
            let content_type = if index == 2 {
                "application/json"
            } else {
                "text/event-stream"
            };
            let answer = json!({"kind": "response", "status": 200, "content_type": content_type});
            frames.extend(
                host.deliver(session, id, answer_of(&answer).unwrap())
                    .unwrap(),
            );
        }
        Some(frames)
    }));
    let said = |reply: &Value| {
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    let failed = app.call("prompt", json!({"dir": "why1", "text": "hello"}));
    let message = said(&failed);
    assert!(
        message.contains("the provider ended its reply with an error")
            && message.contains("overloaded, try again")
            && !message.contains("could not be parsed"),
        "{failed}"
    );
    let failed = app.call("prompt", json!({"dir": "why2", "text": "hello"}));
    assert!(
        said(&failed)
            .contains("could not be parsed (the reply was application/json, not an event stream)"),
        "{failed}"
    );
    let failed = app.call("prompt", json!({"dir": "why3", "text": "hello"}));
    assert!(
        said(&failed).contains("could not be parsed (") && said(&failed).contains("unexpected EOF"),
        "{failed}"
    );
}

#[test]
fn an_error_streamed_mid_reply_is_said_alike_by_both_hosts() {
    let script = vec![Reply::StreamError("overloaded, try again")];
    let (_, _, outcome) = native("midreply", script.clone(), |_| {}, None, None, "go");
    let native = outcome.unwrap_err().to_string();
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script("midreply", script);
    app.call(
        "create",
        json!({"dir": "midreply", "provider": "fake", "model": "fake-model"}),
    );
    let reply = app.call(
        "prompt",
        json!({"dir": "midreply", "text": "[t:midreply] go"}),
    );
    let embedded = reply["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    for said in [&native, &embedded] {
        assert!(
            said.contains("the provider ended its reply with an error")
                && said.contains("overloaded, try again"),
            "native: {native}\nembedded: {embedded}"
        );
    }
}

// ---- a command the user stops ----------------------------------------------------------

/// The text a stopped command's result ends with.
const STOPPED: &str = "the user stopped this command before it finished";

/// A session `dir` whose turn runs `echo started; sleep 100` in the app's runner, which
/// holds it; `before` are the replies before that call, `after` the ones after it. The
/// prompt (request id 1) is sent; the runner's calls are counted in `runs`.
fn running_command<'a>(
    host: &'a Embedded,
    dir: &str,
    before: Vec<Reply>,
    after: Vec<Reply>,
    runs: &'a std::cell::Cell<usize>,
) -> App<'a> {
    let mut script = before;
    script.push(Reply::Call(
        "bash",
        json!({"command": "echo started; sleep 100"}),
    ));
    script.extend(after);
    let mut app = App::new(host).script(dir, script);
    app.runner = Box::new(move |_, _| {
        runs.set(runs.get() + 1);
        Value::Null
    });
    app.call(
        "create",
        json!({"dir": dir, "provider": "fake", "runner": "app"}),
    );
    host.set_runner(None);
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "prompt",
        "params": {"dir": dir, "text": format!("[t:{dir}] run it")}});
    let frames = host.handle(line.to_string().as_bytes());
    app.drive(frames);
    assert_eq!(
        app.run.held.len(),
        1,
        "the command runs: {:?}",
        app.run.replies
    );
    app
}

/// Stops the held command: the app is asked to stop it (not to drop it), and its answer
/// (what it printed) ends the turn, recorded as stopped.
fn stop_command(host: &Embedded, app: &mut App<'_>, dir: &str) -> Value {
    let stopped = app.call("stop", json!({"dir": dir}));
    assert_eq!(stopped["result"]["stopping"], true, "{stopped}");
    let (session, id) = app.run.held.pop().unwrap();
    assert_eq!(
        app.run.stopping,
        [json!({"session": session, "id": id})],
        "the app is asked to stop the command"
    );
    assert!(app.run.cancelled.is_empty(), "{:?}", app.run.cancelled);
    assert!(
        !app.run.replies.iter().any(|r| r["id"] == 1),
        "the turn waits for the stopped command's answer"
    );
    let frames = host
        .deliver(
            &session,
            id,
            answer_of(
                &json!({"kind": "ran", "output": "started\n", "success": false,
                "failure": "the command was stopped"}),
            )
            .unwrap(),
        )
        .unwrap();
    app.drive(frames);
    let ended = app
        .run
        .replies
        .iter()
        .find(|r| r["id"] == 1)
        .unwrap()
        .clone();
    assert_eq!(kind(&ended), Some("stopped"), "{ended}");
    let result = app
        .run
        .events
        .iter()
        .rfind(|e| e["type"] == "tool_result")
        .unwrap()
        .clone();
    let content = result["content"].as_str().unwrap();
    assert!(content.starts_with("started\n"), "{content}");
    assert!(content.contains(STOPPED), "{content}");
    assert_eq!(result["failed"], true);
    let status = app.call("status", json!({"dir": dir}))["result"].clone();
    assert_eq!(status["phase"], "after_tools", "{status}");
    assert_eq!(status["accepts_message"], true, "{status}");
    assert_eq!(status["accepts_prompt"], false, "{status}");
    result
}

#[test]
fn a_stopped_command_is_recorded_and_a_message_goes_on_with_the_turn() {
    let host = embedded(Box::new(MemoryPlaces));
    let runs = std::cell::Cell::new(0);
    let mut app = running_command(&host, "sm", vec![], vec![Reply::Text("skipped")], &runs);
    stop_command(&host, &mut app, "sm");
    // A message (no Continue first) goes on with the turn: its next request carries the
    // stopped result, then the message.
    let reply = app.call(
        "prompt",
        json!({"dir": "sm", "text": "skip that and say skipped"}),
    );
    assert!(reply.get("result").is_some(), "{reply}");
    assert_eq!(app.run.requests.len(), 2);
    let next = &app.run.requests[1];
    let stopped = next.find(STOPPED).expect("the stopped result is sent");
    let said = next
        .find("skip that and say skipped")
        .expect("the message is sent");
    assert!(stopped < said, "the result, then the message: {next}");
    assert_eq!(runs.get(), 1, "the stopped command never runs again");
    assert!(app.run.events.iter().any(|e| e["type"] == "steered"));
    let status = app.call("status", json!({"dir": "sm"}))["result"].clone();
    assert_eq!(status["complete"], true, "{status}");
    // Files cannot go with such a message: it says so, and nothing changes.
    let runs2 = std::cell::Cell::new(0);
    let mut other = running_command(&host, "sf", vec![], vec![], &runs2);
    stop_command(&host, &mut other, "sf");
    let refused = other.call(
        "prompt",
        json!({"dir": "sf", "text": "with a file", "files": [{"path": "a.txt", "base64": "eA=="}]}),
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("takes a message without files"),
        "{refused}"
    );
}

#[test]
fn continue_after_a_stopped_command_never_runs_it_again() {
    let host = embedded(Box::new(MemoryPlaces));
    let runs = std::cell::Cell::new(0);
    let mut app = running_command(&host, "sc", vec![], vec![Reply::Text("went on")], &runs);
    stop_command(&host, &mut app, "sc");
    let continued = app.call("continue", json!({"dir": "sc"}));
    assert!(continued.get("result").is_some(), "{continued}");
    assert_eq!(
        runs.get(),
        1,
        "Continue does not run the stopped command again"
    );
    assert_eq!(app.run.requests.len(), 2);
    assert!(
        app.run.requests[1].contains(STOPPED),
        "the model reads it was stopped"
    );
}

#[test]
fn the_rest_of_a_stopped_batch_is_not_run() {
    let host = embedded(Box::new(MemoryPlaces));
    let runs = std::cell::Cell::new(0);
    // Two calls in one reply: the first is stopped, the second never starts.
    let mut app = App::new(&host).script("sb", vec![Reply::Text("unused")]);
    app.runner = Box::new(|_, _| {
        runs.set(runs.get() + 1);
        Value::Null
    });
    app.call(
        "create",
        json!({"dir": "sb", "provider": "fake", "runner": "app"}),
    );
    host.set_runner(None);
    let two = {
        let calls = json!({"tool_calls": [
            {"index": 0, "id": "call_a", "type": "function", "function": {"name": "bash",
                "arguments": json!({"command": "sleep 100"}).to_string()}},
            {"index": 1, "id": "call_b", "type": "function", "function": {"name": "write_file",
                "arguments": json!({"path": "b.txt", "content": "b"}).to_string()}}]});
        let chunk = |delta: Value, finish: Value| {
            json!({"id": "c", "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
        };
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            chunk(calls, Value::Null),
            chunk(json!({}), json!("tool_calls"))
        )
    };
    app.hook = Some(Box::new(move |host, session, id, index| {
        (index == 1).then(|| {
            let (mut frames, _) = host.chunk(session, id, two.as_bytes());
            frames.extend(
                host.deliver(
                    session,
                    id,
                    answer_of(&json!({"kind": "response",
                "status": 200, "content_type": "text/event-stream"}))
                    .unwrap(),
                )
                .unwrap(),
            );
            frames
        })
    }));
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "prompt",
        "params": {"dir": "sb", "text": "[t:sb] two at once"}});
    let frames = host.handle(line.to_string().as_bytes());
    app.drive(frames);
    assert_eq!(app.run.held.len(), 1);
    stop_command(&host, &mut app, "sb");
    let refused = app
        .run
        .events
        .iter()
        .find(|e| e["type"] == "tool_refused")
        .expect("the second call is refused");
    assert!(
        refused["reason"]
            .as_str()
            .unwrap()
            .contains("stopped the turn before")
    );
    let files = app.call("files", json!({"dir": "sb"}));
    assert!(
        files["result"]["files"].as_array().unwrap().is_empty(),
        "{files}"
    );
    assert_eq!(runs.get(), 1);
}

#[test]
fn retry_and_take_back_work_after_a_stopped_command() {
    let host = embedded(Box::new(MemoryPlaces));
    let runs = std::cell::Cell::new(0);
    // A turn before (settled), then one whose second request runs the command.
    let mut app = App::new(&host).script(
        "sr",
        vec![
            Reply::Text("first done"),
            Reply::Call("write_file", json!({"path": "w.txt", "content": "w"})),
            Reply::Call("bash", json!({"command": "echo started; sleep 100"})),
            Reply::Text("asked again"),
        ],
    );
    app.runner = Box::new(|_, _| {
        runs.set(runs.get() + 1);
        Value::Null
    });
    app.call(
        "create",
        json!({"dir": "sr", "provider": "fake", "runner": "app"}),
    );
    host.set_runner(None);
    let first = app.call("prompt", json!({"dir": "sr", "text": "[t:sr] first"}));
    assert!(first.get("result").is_some(), "{first}");
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "prompt",
        "params": {"dir": "sr", "text": "write then run"}});
    let frames = host.handle(line.to_string().as_bytes());
    app.drive(frames);
    stop_command(&host, &mut app, "sr");
    // Retry asks the model again from before the request that made the stopped call:
    // the same request as then, without the stopped call or its result.
    let asked = app.run.requests.len();
    let retried = app.call("retry", json!({"dir": "sr"}));
    assert!(retried.get("result").is_some(), "{retried}");
    assert_eq!(app.run.requests.len(), asked + 1);
    let again = &app.run.requests[asked];
    assert!(!again.contains(STOPPED), "{again}");
    assert_eq!(
        again,
        &app.run.requests[asked - 1],
        "the same request again"
    );
    assert_eq!(runs.get(), 1);
    // Taking the turn back works too: no call is left unresolved.
    let runs2 = std::cell::Cell::new(0);
    let mut other = running_command(&host, "st", vec![], vec![], &runs2);
    stop_command(&host, &mut other, "st");
    let targets = other.call("targets", json!({"dir": "st"}));
    let target = targets["result"]["targets"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["revision"]
        .as_u64()
        .unwrap();
    let rewound = other.call("rewind", json!({"dir": "st", "revision": target}));
    assert!(rewound.get("result").is_some(), "{rewound}");
    // The stopped step came from the session's first request: Retry says what to do.
    let runs3 = std::cell::Cell::new(0);
    let mut first = running_command(&host, "sq", vec![], vec![], &runs3);
    stop_command(&host, &mut first, "sq");
    let refused = first.call("retry", json!({"dir": "sq"}));
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("first request"),
        "{refused}"
    );
}

#[test]
fn a_stopped_command_and_a_message_after_it_survive_a_reopen() {
    let root = temp_root("stop-reopen");
    let runs = std::cell::Cell::new(0);
    {
        let host = embedded(Box::new(DirPlaces(root.clone())));
        let mut app = running_command(&host, "so", vec![], vec![], &runs);
        stop_command(&host, &mut app, "so");
    }
    // Another host (the app restarted): the stopped result is in the store, the turn
    // still rests, and a message goes on with it.
    let host = embedded(Box::new(DirPlaces(root.clone())));
    let mut app = App::new(&host).script("so", vec![Reply::Text("after the reopen")]);
    app.runner = Box::new(|_, _| {
        runs.set(runs.get() + 1);
        Value::Null
    });
    let status = app.call("status", json!({"dir": "so"}))["result"].clone();
    assert_eq!(status["accepts_message"], true, "{status}");
    let reply = app.call("prompt", json!({"dir": "so", "text": "go on without it"}));
    assert!(reply.get("result").is_some(), "{reply}");
    let next = &app.run.requests[0];
    assert!(
        next.find(STOPPED).unwrap() < next.find("go on without it").unwrap(),
        "{next}"
    );
    assert_eq!(runs.get(), 1);
}

#[test]
fn a_runner_that_does_not_answer_a_stop_is_recorded_as_stopped_on_a_second_stop() {
    let host = embedded(Box::new(MemoryPlaces));
    let runs = std::cell::Cell::new(0);
    let mut app = running_command(&host, "sn", vec![], vec![Reply::Text("ok")], &runs);
    let first = app.call("stop", json!({"dir": "sn"}));
    assert_eq!(first["result"]["stopping"], true, "{first}");
    assert!(!app.run.replies.iter().any(|r| r["id"] == 1));
    // Asked again: recorded as stopped without the runner's answer, which is dropped.
    let second = app.call("stop", json!({"dir": "sn"}));
    assert!(second.get("result").is_some(), "{second}");
    let ended = app
        .run
        .replies
        .iter()
        .find(|r| r["id"] == 1)
        .unwrap()
        .clone();
    assert_eq!(kind(&ended), Some("stopped"), "{ended}");
    assert_eq!(app.run.cancelled.len(), 1, "the app drops the command now");
    let result = app
        .run
        .events
        .iter()
        .rfind(|e| e["type"] == "tool_result")
        .unwrap();
    assert!(
        result["content"].as_str().unwrap().contains(STOPPED),
        "{result}"
    );
    let reply = app.call("prompt", json!({"dir": "sn", "text": "never mind"}));
    assert!(reply.get("result").is_some(), "{reply}");
    assert_eq!(runs.get(), 1);
}

// ---- the native host: a command the user stops -----------------------------------------

/// Records events and stops the session's turn once its command prints `started`.
struct StopOnOutput {
    events: Mutex<Vec<Value>>,
    stop: Mutex<Option<Arc<AtomicBool>>>,
}
impl Observer for StopOnOutput {
    fn event(&self, event: Event<'_>) {
        let json = event.to_json();
        if json["type"] == "output"
            && let Some(stop) = &*self.stop.lock().unwrap()
        {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.events.lock().unwrap().push(json);
    }
}

/// A native session `name` (unisolated runs, the fake model) whose prompt runs a command
/// that counts its runs in `<dir>/../<name>.count`, prints `started` and sleeps; the user
/// stops it as it prints. Returns the session's dir and the observer.
fn native_stopped(
    name: &str,
    before: Vec<Reply>,
    after: Vec<Reply>,
) -> (PathBuf, Arc<StopOnOutput>) {
    let fake = fake();
    let dir = fake.root.join(name);
    let count = fake.root.join(format!("{name}.count"));
    let mut script = before.clone();
    script.push(Reply::Call(
        "bash",
        json!({"command": format!("echo run >> {}; echo started; sleep 30", count.display())}),
    ));
    script.extend(after);
    fake.scripts
        .lock()
        .unwrap()
        .insert(name.into(), (script, vec![]));
    let observer = Arc::new(StopOnOutput {
        events: Mutex::default(),
        stop: Mutex::default(),
    });
    let mut settings = Settings::resolve("fake", Some("fake-model".into())).unwrap();
    settings.isolation = agent::Isolation::Off;
    settings.compaction = agent::Compaction::Digest;
    let mut session = Session::create(&dir, settings, vec![], observer.clone()).unwrap();
    for _ in &before {
        // A turn before the one that runs the command (each answered by one reply).
        session.prompt(&format!("[t:{name}] warm up")).unwrap();
    }
    *observer.stop.lock().unwrap() = Some(session.interrupt_handle());
    let started = std::time::Instant::now();
    let stopped = session.prompt(&format!("[t:{name}] run it")).unwrap_err();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "the stop ended it"
    );
    assert!(stopped.contains("stopped by the user"), "{stopped}");
    let result = observer
        .events
        .lock()
        .unwrap()
        .iter()
        .rfind(|e| e["type"] == "tool_result")
        .unwrap()
        .clone();
    let content = result["content"].as_str().unwrap();
    assert!(
        content.starts_with("started\n"),
        "what it printed is kept: {content}"
    );
    assert!(content.contains(STOPPED), "{content}");
    assert!(!session.accepts_prompt());
    assert!(session.accepts_message());
    *observer.stop.lock().unwrap() = None;
    (dir, observer)
}

fn runs_of(name: &str) -> usize {
    std::fs::read_to_string(fake().root.join(format!("{name}.count")))
        .unwrap_or_default()
        .lines()
        .count()
}

fn requests_of(name: &str) -> Vec<String> {
    fake().scripts.lock().unwrap()[name].1.clone()
}

#[test]
fn native_a_stopped_command_is_recorded_and_a_message_goes_on_after_a_reopen() {
    let (dir, observer) = native_stopped("nm", vec![], vec![Reply::Text("skipped")]);
    // A fresh owner, as `spindle serve` opens one per command.
    let mut session = Session::open(&dir, observer).unwrap();
    assert!(session.accepts_message());
    session.prompt("skip that and say skipped").unwrap();
    let requests = requests_of("nm");
    assert_eq!(requests.len(), 2);
    let next = &requests[1];
    assert!(
        next.find(STOPPED).unwrap() < next.find("skip that and say skipped").unwrap(),
        "{next}"
    );
    assert_eq!(runs_of("nm"), 1, "the stopped command never runs again");
    assert!(session.accepts_prompt());
}

#[test]
fn native_continue_after_a_stopped_command_never_runs_it_again() {
    let (dir, observer) = native_stopped("nc", vec![], vec![Reply::Text("went on")]);
    let mut session = Session::open(&dir, observer).unwrap();
    session.resume().unwrap();
    let requests = requests_of("nc");
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains(STOPPED));
    assert_eq!(runs_of("nc"), 1);
}

#[test]
fn native_retry_and_rewind_work_after_a_stopped_command() {
    let (dir, observer) = native_stopped(
        "nr",
        vec![Reply::Text("warmed")],
        vec![Reply::Text("asked again")],
    );
    let session = Session::open(&dir, observer.clone()).unwrap();
    // Its request was the turn's first: the turn starts again with its prompt.
    session.retry().unwrap();
    let requests = requests_of("nr");
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[2], requests[1],
        "the stopped step's request, asked again"
    );
    assert_eq!(runs_of("nr"), 1);
    let (dir, observer) = native_stopped("nw", vec![Reply::Text("warmed")], vec![]);
    let mut session = Session::open(&dir, observer).unwrap();
    let settled = session
        .targets()
        .unwrap()
        .into_iter()
        .find(|target| target.kind == spindle_store::rewind::RewindTargetKind::Settled)
        .unwrap();
    session.rewind(settled.revision).unwrap();
    assert!(session.accepts_prompt());
}

#[test]
fn a_message_after_a_cut_reply_asks_again_from_before_it_and_the_cut_stays_counted() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script(
        "sk",
        vec![
            Reply::Call("write_file", json!({"path": "a.txt", "content": "a"})),
            Reply::Text("cut, never read"),
            Reply::Text("used the message"),
        ],
    );
    app.call("create", json!({"dir": "sk", "provider": "fake"}));
    // The second request is sent and its reply never comes: the user stops it.
    app.hold = 2;
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "prompt",
        "params": {"dir": "sk", "text": "[t:sk] write a"}});
    let frames = host.handle(line.to_string().as_bytes());
    app.drive(frames);
    app.call("stop", json!({"dir": "sk"}));
    let ended = app
        .run
        .replies
        .iter()
        .find(|r| r["id"] == 1)
        .unwrap()
        .clone();
    assert_eq!(kind(&ended), Some("stopped"), "{ended}");
    let status = app.call("status", json!({"dir": "sk"}))["result"].clone();
    assert_eq!(status["phase"], "model_wait", "{status}");
    assert_eq!(status["accepts_message"], true, "{status}");
    let unknown =
        |app: &mut App<'_>| app.call("usage", json!({"dir": "sk"}))["result"]["unknown"].clone();
    assert_eq!(unknown(&mut app), 1, "the cut request may have been billed");
    // A message: the request is made again from before it, the message read first.
    app.hold = 0;
    let reply = app.call("prompt", json!({"dir": "sk", "text": "and also write b"}));
    assert!(reply.get("result").is_some(), "{reply}");
    assert_eq!(app.run.requests.len(), 3);
    let again = &app.run.requests[2];
    assert!(again.contains("and also write b"), "{again}");
    // What the cut request carried (the write's result), then the message.
    let written = again
        .rfind("\"role\":\"tool\"")
        .expect("the write's result");
    assert!(written < again.find("and also write b").unwrap(), "{again}");
    assert!(!again.contains("cut, never read"), "{again}");
    assert!(app.run.events.iter().any(|e| e["type"] == "retrying"));
    assert_eq!(unknown(&mut app), 1, "the cut request stays counted");
    let status = app.call("status", json!({"dir": "sk"}))["result"].clone();
    assert_eq!(status["complete"], true, "{status}");
    // A saved reply is read in first: a message then is refused, saying so.
    let first = embedded(Box::new(MemoryPlaces));
    let mut cut = App::new(&first).script("sf1", vec![Reply::Text("x")]);
    cut.call("create", json!({"dir": "sf1", "provider": "fake"}));
    cut.hold = 1;
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "prompt",
        "params": {"dir": "sf1", "text": "[t:sf1] first"}});
    let frames = first.handle(line.to_string().as_bytes());
    cut.drive(frames);
    cut.call("stop", json!({"dir": "sf1"}));
    // The session's first request has no point before it: Retry or Take back, so status
    // says no message is taken, and one is refused.
    let status = cut.call("status", json!({"dir": "sf1"}))["result"].clone();
    assert_eq!(status["phase"], "model_wait", "{status}");
    assert_eq!(status["accepts_message"], false, "{status}");
    let refused = cut.call("prompt", json!({"dir": "sf1", "text": "never mind"}));
    assert!(refused.get("error").is_some(), "{refused}");
    assert_eq!(cut.run.requests.len(), 1, "nothing is sent again");
}

/// Stops the session's turn as its first tool call is about to run.
struct StopOnCall {
    events: Mutex<Vec<Value>>,
    stop: Mutex<Option<Arc<AtomicBool>>>,
}
impl Observer for StopOnCall {
    fn event(&self, event: Event<'_>) {
        let json = event.to_json();
        if json["type"] == "tool_call"
            && let Some(stop) = self.stop.lock().unwrap().take()
        {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.events.lock().unwrap().push(json);
    }
}

#[test]
fn native_a_stop_before_a_call_starts_records_it_as_not_run() {
    let fake = fake();
    let name = "nb";
    let marker = fake.root.join("nb.ran");
    fake.scripts.lock().unwrap().insert(
        name.into(),
        (
            vec![
                Reply::Call(
                    "bash",
                    json!({"command": format!("touch {}", marker.display())}),
                ),
                Reply::Text("did something else"),
            ],
            vec![],
        ),
    );
    let observer = Arc::new(StopOnCall {
        events: Mutex::default(),
        stop: Mutex::default(),
    });
    let dir = fake.root.join(name);
    let mut settings = Settings::resolve("fake", Some("fake-model".into())).unwrap();
    settings.isolation = agent::Isolation::Off;
    let mut session = Session::create(&dir, settings, vec![], observer.clone()).unwrap();
    *observer.stop.lock().unwrap() = Some(session.interrupt_handle());
    let stopped = session.prompt("[t:nb] touch it").unwrap_err();
    assert!(stopped.contains("stopped by the user"), "{stopped}");
    let refused = observer
        .events
        .lock()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "tool_refused")
        .cloned()
        .expect("the call is recorded as not run");
    assert!(
        refused["reason"]
            .as_str()
            .unwrap()
            .contains("before this call started")
    );
    assert!(session.accepts_message());
    drop(session);
    let mut session = Session::open(&dir, observer).unwrap();
    session.prompt("do something else instead").unwrap();
    assert!(!marker.exists(), "the stopped call never ran");
    let next = &requests_of(name)[1];
    assert!(
        next.find("before this call started").unwrap() < next.find("do something else").unwrap(),
        "{next}"
    );
}

#[test]
fn a_stopped_turn_goes_on_with_another_model_from_its_last_request() {
    let root = temp_root("switch");
    {
        let host = embedded(Box::new(DirPlaces(root.clone())));
        let mut app = App::new(&host).script(
            "m",
            vec![
                Reply::Text("first done"),
                Reply::Call("write_file", json!({"path": "a.txt", "content": "kept"})),
            ],
        );
        app.call("create", json!({"dir": "m", "provider": "fake"}));
        app.call("prompt", json!({"dir": "m", "text": "[t:switch1] first"}));
        // The second turn's file is written, then its next response is lost (the
        // provider ran out, the connection broke: the outcome is unknown).
        app.hold = 3;
        let line = json!({"jsonrpc": "2.0", "id": 9, "method": "prompt",
            "params": {"dir": "m", "text": "[t:switch2] write it"}});
        let frames = host.handle(line.to_string().as_bytes());
        app.drive(frames);
        assert_eq!(app.run.requests.len(), 3);
    }
    let host = embedded(Box::new(DirPlaces(root.clone())));
    let mut app = App::new(&host).script("m", vec![Reply::Text("finished on the small one")]);
    // Its model cannot change where a request is in flight.
    let refused = app.call("settings", json!({"dir": "m", "model": "small-model"}));
    assert!(refused.get("error").is_some(), "{refused}");
    // Back to the newest completed turn, however far: the point before the lost
    // request, then that turn's end; the turn's own message, not a summary.
    let turn = app.call("targets", json!({"dir": "m", "until": "settled"}));
    let turn = &turn["result"];
    assert_eq!(turn["completed"], true, "{turn}");
    assert_eq!(turn["prompt"], "[t:switch2] write it", "{turn}");
    let kinds: Vec<&str> = turn["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|target| target["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds.first(), Some(&"before_model"), "{turn}");
    assert_eq!(kinds.last(), Some(&"settled"), "{turn}");
    let before = turn["targets"][0]["revision"].as_u64().unwrap();
    let rewound = app.call("rewind", json!({"dir": "m", "revision": before}));
    assert!(rewound.get("result").is_some(), "{rewound}");
    let changed = app.call("settings", json!({"dir": "m", "model": "small-model"}));
    assert_eq!(changed["result"]["model"], "small-model", "{changed}");
    let continued = app.call("continue", json!({"dir": "m"}));
    assert!(continued.get("result").is_some(), "{continued}");
    assert_eq!(app.run.requests.len(), 1, "one fresh request");
    assert!(
        app.run.requests[0].contains("\"small-model\""),
        "{}",
        app.run.requests[0]
    );
    // The turn's work so far stays: its file and its history.
    let read = app.call("read", json!({"dir": "m", "path": "a.txt"}));
    assert_eq!(read["result"]["content"], "kept", "{read}");
    let status = app.call("status", json!({"dir": "m"}));
    assert_eq!(status["result"]["complete"], true, "{status}");
    let first = app.call("targets", json!({"dir": "m", "until": "settled"}));
    assert_eq!(first["result"]["prompt"], "[t:switch2] write it", "{first}");
}

#[test]
fn a_first_turn_says_it_has_no_completed_turn_behind_it() {
    let root = temp_root("firstturn");
    {
        let host = embedded(Box::new(DirPlaces(root.clone())));
        let mut app = App::new(&host);
        app.call("create", json!({"dir": "f", "provider": "fake"}));
        app.hold = 1;
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": "prompt",
            "params": {"dir": "f", "text": "[t:firstturn] hello"}});
        let frames = host.handle(line.to_string().as_bytes());
        app.drive(frames);
    }
    let host = embedded(Box::new(DirPlaces(root.clone())));
    let mut app = App::new(&host);
    let turn = app.call("targets", json!({"dir": "f", "until": "settled"}));
    assert_eq!(turn["result"]["completed"], false, "{turn}");
    assert_eq!(turn["result"]["prompt"], "[t:firstturn] hello", "{turn}");
    let bad = app.call("targets", json!({"dir": "f", "until": "start"}));
    assert!(bad.get("error").is_some(), "{bad}");
}

#[test]
fn a_move_seals_its_copy_only_at_the_head_of_the_bundle_kept() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script("m", vec![Reply::Text("first"), Reply::Text("second")]);
    app.call("create", json!({"dir": "m", "provider": "fake"}));
    let fresh = app.call("status", json!({"dir": "m"}))["result"].clone();
    assert_eq!(fresh["state"], "no turns yet", "{fresh}");
    app.call("prompt", json!({"dir": "m", "text": "one"}));
    // Status says how its head stands from the head it read (a completed turn here).
    let status = app.call("status", json!({"dir": "m"}))["result"].clone();
    assert_eq!(status["phase"], "stopped", "{status}");
    assert_eq!(status["state"], status["stop_text"], "{status}");
    // The bundle is kept (the session stays here); it went on since: nothing sealed.
    let bytes = host.export_session("m", true, None).unwrap();
    let kept = spindle_store::bundle::read(&bytes).unwrap();
    let head = kept.head;
    app.call("prompt", json!({"dir": "m", "text": "two"}));
    let moved_on = host
        .seal_session("m", head, kept.source_state.as_deref().unwrap())
        .unwrap_err();
    assert_eq!(moved_on.kind, "head_moved", "{}", moved_on.message);
    let now = app.call("status", json!({"dir": "m"}))["result"]["head"].clone();
    assert!(now.as_u64().unwrap() > head);
    // At the head of the bundle kept now: sealed, and closed here.
    let bytes = host.export_session("m", true, None).unwrap();
    let kept = spindle_store::bundle::read(&bytes).unwrap();
    let sealed = host
        .seal_session(
            "m",
            now.as_u64().unwrap(),
            kept.source_state.as_deref().unwrap(),
        )
        .unwrap();
    assert_eq!(sealed, json!({"session": "m", "head": now, "sealed": true}));
    let listed = app.call("list", json!({}))["result"]["sessions"].clone();
    assert_eq!(listed, json!([]), "{listed}");
    assert_eq!(
        host.seal_session("nowhere", 0, "").unwrap_err().kind,
        "not_found"
    );
}

#[test]
fn files_cross_beside_the_request_line_each_taken_once() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script("f", vec![Reply::Text("got them")]);
    let line = |id: u64, method: &str, params: Value| {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
    };
    let created = host.handle_with(
        line(
            1,
            "create",
            json!({"dir": "f", "provider": "fake", "files": [
            {"path": "a.bin", "attached": 0}, {"path": "t.txt", "text": "text"}]}),
        )
        .as_bytes(),
        vec![vec![0, 159, 255]],
    );
    assert!(created[0].get("result").is_some(), "{created:?}");
    let frames = host.handle_with(
        line(
            2,
            "prompt",
            json!({"dir": "f", "text": "take them", "files": [
            {"path": "b.bin", "attached": 1, "executable": true},
            {"path": "c.txt", "attached": 0}, {"path": "d.txt", "base64": "ZA=="}]}),
        )
        .as_bytes(),
        vec![b"c".to_vec(), vec![1, 2, 3]],
    );
    app.drive(frames);
    let done = app
        .run
        .replies
        .iter()
        .find(|reply| reply["id"] == 2)
        .cloned();
    assert_eq!(
        done.as_ref()
            .map(|reply| reply["result"]["stopped"]["complete"].clone()),
        Some(json!(true)),
        "{done:?}"
    );
    let files = app.call("files", json!({"dir": "f"}))["result"]["files"].clone();
    let file = |path: &str| {
        files
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["path"] == path)
            .cloned()
            .unwrap_or_else(|| panic!("no {path} in {files}"))
    };
    assert_eq!(
        (
            file("a.bin")["size"].clone(),
            file("a.bin")["executable"].clone()
        ),
        (json!(3), json!(false))
    );
    assert_eq!(
        (
            file("b.bin")["size"].clone(),
            file("b.bin")["executable"].clone()
        ),
        (json!(3), json!(true))
    );
    for (path, content) in [
        ("a.bin", "AJ//"),
        ("c.txt", "c"),
        ("d.txt", "d"),
        ("t.txt", "text"),
    ] {
        let read = app.call("read", json!({"dir": "f", "path": path}))["result"].clone();
        assert_eq!(read["content"], content, "{read}");
    }
    // One the request did not carry, one taken twice, or one with more than its bytes
    // is refused; and none is kept past its request.
    for (files, carried, why) in [
        (json!([{"path": "x", "attached": 1}]), 1, "names no file"),
        (
            json!([{"path": "x", "attached": 0}, {"path": "y", "attached": 0}]),
            1,
            "used twice",
        ),
        (
            json!([{"path": "x", "attached": 0, "base64": "AA=="}]),
            1,
            "not a key",
        ),
        (
            json!([{"path": "../x", "attached": 0}]),
            1,
            "workspace path",
        ),
        (json!([{"path": "x", "attached": 0}]), 0, "names no file"),
    ] {
        let frames = host.handle_with(
            line(
                3,
                "prompt",
                json!({"dir": "f", "text": "again", "files": files}),
            )
            .as_bytes(),
            vec![vec![1]; carried],
        );
        let message = frames[0]["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains(why), "{why}: {frames:?}");
    }
}

// ---- freeing a finished task's space (`export_task`, `task_drop`) ---------------------

/// An app over sessions in `root` whose every reply costs $1.10 (as the budget tests'
/// above).
fn priced(root: &Path) -> Embedded {
    let mut catalog = catalog_json(&fake().base);
    catalog["providers"][0]["model_specs"]["fake-model"]["pricing"] = json!({
        "input_usd_per_mtok": 1000, "output_usd_per_mtok": 1000,
        "cache_read_usd_per_mtok": 1000, "cache_create_usd_per_mtok": 1000});
    Embedded::new(
        Box::new(DirPlaces(root.to_path_buf())),
        Catalog::from_json(&catalog).unwrap(),
    )
}

/// The replies of a task that runs a task of its own: $2.20 for it, $1.10 for its task.
fn nested_task_replies() -> Vec<Reply> {
    vec![
        Reply::Call("task", json!({"prompt": "delegate deeper"})),
        Reply::Text("grandchild done"),
        Reply::Text("child done"),
    ]
}

/// Session `dir` under a $9 budget whose app task `t` ran a task of its own (the
/// session's $1.10, the task's $2.20, its task's $1.10), then a prompt the budget refuses
/// only with the nested task counted: what the refusal said, and the task's label.
fn spent_tree(app: &mut App, dir: &str) -> (String, String) {
    app.call(
        "create",
        json!({"dir": dir, "provider": "fake", "tasks": true, "max_usd": 9}),
    );
    app.call(
        "prompt",
        json!({"dir": dir, "text": format!("[t:{dir}] hello")}),
    );
    let started = app.call(
        "task_start",
        json!({"dir": dir, "task": "t", "prompt": format!("[t:{dir}-task] go")}),
    );
    assert_eq!(started["result"]["state"], "finished", "{started}");
    let refused = app.call(
        "prompt",
        json!({"dir": dir, "text": format!("[t:{dir}] more")}),
    );
    let said = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(said.starts_with("spending limit:"), "{refused}");
    assert!(
        dollars_after(said, "has spent ") + 2.20 + dollars_after(said, "could cost up to ") <= 9.0,
        "only the nested task tips it: {said}"
    );
    (
        said.to_string(),
        started["result"]["task"].as_str().unwrap().to_string(),
    )
}

/// What a budget refusal said the session spent, after `continue` (asserted refused).
fn refused_at(app: &mut App, dir: &str) -> (f64, f64) {
    let reply = app.call("continue", json!({"dir": dir}));
    let said = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(said.starts_with("spending limit:"), "{reply}");
    (
        dollars_after(said, "has spent "),
        dollars_after(said, "could cost up to "),
    )
}

/// `continue` under a budget that fits what was spent and the next request once, not
/// twice: it is sent.
fn sent_once(app: &mut App, dir: &str, spent: f64, cost: f64) {
    app.call(
        "settings",
        json!({"dir": dir, "max_usd": spent + cost + 0.5}),
    );
    let sent = app.requests_after(|app| app.call("continue", json!({"dir": dir})));
    assert!(sent.0["error"].is_null(), "{}", sent.0);
    assert_eq!(sent.1, 1, "the request was sent");
}

impl App<'_> {
    /// `run`'s reply, and how many requests it sent.
    fn requests_after(&mut self, run: impl FnOnce(&mut Self) -> Value) -> (Value, usize) {
        let before = self.run.requests.len();
        let reply = run(self);
        (reply, self.run.requests.len() - before)
    }
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.005
}

#[test]
fn a_finished_tasks_store_is_kept_in_a_file_then_removed_and_its_spend_counts_once() {
    let root = temp_root("task-drop");
    let host = priced(&root);
    let mut app = App::new(&host)
        .script("dt", vec![Reply::Text("ready")])
        .script("#", nested_task_replies());
    let (said, label) = spent_tree(&mut app, "dt");
    let cost = dollars_after(&said, "could cost up to ");
    let key = label.rsplit('#').next().unwrap().to_string();
    let listed = app.call("tasks", json!({"dir": "dt"}))["result"]["tasks"].clone();
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    let (store, orphan) = (&listed[0]["store"], &listed[0]["orphan"]);
    assert_eq!((store, orphan), (&json!(true), &json!(false)), "{listed}");
    assert!(listed[0]["dropped"].is_null(), "{listed}");
    let bytes = listed[0]["bytes"].as_u64().unwrap();
    assert!(bytes > 8192, "its store and its task's: {listed}");

    // Kept in a file first: a session of its own, its task's store with it, that
    // imports and reads as one.
    let file = host.export_task("dt", "t").unwrap();
    let bundle = spindle_store::bundle::read(&file).unwrap();
    let paths: Vec<&str> = bundle.files.iter().map(|file| file.path.as_str()).collect();
    assert!(paths.contains(&"store/authority.sqlite"), "{paths:?}");
    assert!(
        paths
            .iter()
            .any(|path| path.starts_with("tasks/") && path.ends_with("/store/authority.sqlite")),
        "{paths:?}"
    );
    host.import_session("dt-task", &file).unwrap();
    let read = app.call("history", json!({"dir": "dt-task"}))["result"].to_string();
    assert!(read.contains("child done"), "{read}");

    // Removed: its directory goes, its record stays with its answer.
    let dropped = app.call(
        "task_drop",
        json!({"dir": "dt", "task": label, "at": 1_700_000_000u64}),
    );
    let record = dropped["result"]["dropped"].clone();
    assert_eq!(record["bytes"], bytes, "{dropped}");
    assert_eq!(
        (&record["at"], &record["task"], &record["key"]),
        (&json!(1_700_000_000u64), &json!("app:t"), &json!(key)),
        "{dropped}"
    );
    assert_eq!(record["prompt"], "[t:dt-task] go");
    assert!(
        !root.join("dt/tasks").join(&key).exists(),
        "its files are gone"
    );
    assert!(
        !root.join("dt/tasks.removing").exists(),
        "nothing is left over"
    );
    let listed = app.call("tasks", json!({"dir": "dt"}))["result"]["tasks"].clone();
    assert_eq!(listed[0]["store"], false, "{listed}");
    assert!(listed[0]["bytes"].is_null(), "{listed}");
    assert_eq!(listed[0]["dropped"], record, "{listed}");
    assert_eq!(listed[0]["state"], "finished");
    assert!(
        listed[0]["answer"]["content"]
            .as_str()
            .is_some_and(|answer| answer.starts_with("child done")),
        "the answer stays: {listed}"
    );
    let again = app.call("task_drop", json!({"dir": "dt", "task": "t"}));
    assert_eq!(
        again["result"]["dropped"], record,
        "dropping again changes nothing"
    );

    // Its spend is the session's own now: counted once, in this open and the next.
    let (spent, cost_now) = refused_at(&mut app, "dt");
    assert!(near(spent, 4.40), "the tree's $4.40: {spent}");
    assert!(near(cost_now, cost));
    sent_once(&mut app, "dt", spent, cost);
    app.call("close", json!({"dir": "dt"}));
    let more = app.call("prompt", json!({"dir": "dt", "text": "[t:dt] again"}));
    let said = more["error"]["message"].as_str().unwrap_or_default();
    assert!(said.starts_with("spending limit:"), "{more}");
    let (spent, cost) = (
        dollars_after(said, "has spent "),
        dollars_after(said, "could cost up to "),
    );
    assert!(near(spent, 5.50), "reopened, still counted: {spent}");
    sent_once(&mut app, "dt", spent, cost);
    let usage = app.call("usage", json!({"dir": "dt"}))["result"].clone();
    assert!(
        near(usage["usd"].as_f64().unwrap(), 6.60),
        "its own three replies and the task tree's: {usage}"
    );

    // The rest of the session reads and moves without it.
    let history = app.call("history", json!({"dir": "dt"}));
    assert!(history["error"].is_null(), "{history}");
    let whole = host.export_session("dt", true, None).unwrap();
    let bundle = spindle_store::bundle::read(&whole).unwrap();
    assert!(
        bundle
            .files
            .iter()
            .all(|file| !file.path.starts_with("tasks/")),
        "a whole export carries no removed task"
    );
    let copy = host.import_session("dt-copy", &whole).unwrap();
    assert!(copy["head"].as_u64().unwrap() > 0, "{copy}");
    let listed = app.call("tasks", json!({"dir": "dt-copy"}))["result"]["tasks"].clone();
    assert_eq!(
        listed[0]["dropped"], record,
        "the record moves with it: {listed}"
    );
    let compacted = app.call("compact", json!({"dir": "dt"}));
    assert!(compacted["error"].is_null(), "{compacted}");
    let status = app.call("status", json!({"dir": "dt"}));
    assert!(status["error"].is_null(), "it opens again: {status}");
    // It never runs again, nor starts over in its place; its file is gone too.
    let rerun = app.call(
        "task_start",
        json!({"dir": "dt", "task": "t", "prompt": "[t:dt-task] go"}),
    );
    let why = rerun["error"]["message"].as_str().unwrap_or_default();
    assert!(why.contains("working copy was removed"), "{rerun}");
    assert_eq!(
        host.export_task("dt", "t").unwrap_err().kind,
        "not_found",
        "nothing is left to export"
    );
}

#[test]
fn a_removal_cut_short_after_the_move_keeps_the_spend_and_the_next_open_finishes_it() {
    let root = temp_root("task-drop-crash");
    let host = priced(&root);
    let mut app = App::new(&host)
        .script("dc", vec![Reply::Text("ready")])
        .script("#", nested_task_replies());
    let (said, label) = spent_tree(&mut app, "dc");
    let cost = dollars_after(&said, "could cost up to ");
    let key = label.rsplit('#').next().unwrap().to_string();
    // A crash once its directory was moved out of `tasks/`, before it was removed.
    std::fs::write(root.join("dc/fail-removal"), "after").unwrap();
    let cut = app.call("task_drop", json!({"dir": "dc", "task": "t"}));
    assert_eq!(kind(&cut), Some("failed"), "{cut}");
    let why = cut["error"]["message"].as_str().unwrap();
    assert!(why.contains("spend is kept"), "{cut}");
    assert!(!root.join("dc/tasks").join(&key).exists());
    assert!(root.join("dc/tasks.removing").join(&key).exists());
    // Its spend was kept before anything moved: counted once, here and opened again.
    let (spent, _) = refused_at(&mut app, "dc");
    assert!(near(spent, 4.40), "{spent}");
    std::fs::remove_file(root.join("dc/fail-removal")).unwrap();
    app.call("close", json!({"dir": "dc"}));
    let (spent, _) = refused_at(&mut app, "dc");
    assert!(near(spent, 4.40), "{spent}");
    assert!(
        !root.join("dc/tasks.removing").exists(),
        "the open finished the removal"
    );
    sent_once(&mut app, "dc", spent, cost);
    let listed = app.call("tasks", json!({"dir": "dc"}))["result"]["tasks"].clone();
    assert_eq!(listed[0]["store"], false, "{listed}");
    assert!(listed[0]["dropped"].is_object(), "{listed}");
}

#[test]
fn a_removal_cut_short_before_the_move_counts_as_done_and_finishes_when_asked_again() {
    let root = temp_root("task-drop-early");
    let host = priced(&root);
    let mut app = App::new(&host)
        .script("de", vec![Reply::Text("ready")])
        .script("#", nested_task_replies());
    let (said, label) = spent_tree(&mut app, "de");
    let cost = dollars_after(&said, "could cost up to ");
    let key = label.rsplit('#').next().unwrap().to_string();
    // A crash once its spend was kept, before its directory moved: still in `tasks/`.
    std::fs::write(root.join("de/fail-removal"), "before").unwrap();
    let cut = app.call("task_drop", json!({"dir": "de", "task": "t"}));
    assert_eq!(kind(&cut), Some("failed"), "{cut}");
    assert!(root.join("de/tasks").join(&key).exists());
    let listed = app.call("tasks", json!({"dir": "de"}))["result"]["tasks"].clone();
    assert_eq!(
        listed[0]["store"], true,
        "its files are still there: {listed}"
    );
    assert!(
        listed[0]["dropped"].is_object(),
        "and it counts as removed: {listed}"
    );
    // Nothing carries it or counts it twice meanwhile.
    let whole = host.export_session("de", true, None).unwrap();
    let bundle = spindle_store::bundle::read(&whole).unwrap();
    assert!(
        bundle
            .files
            .iter()
            .all(|file| !file.path.starts_with("tasks/")),
        "an export leaves it out"
    );
    assert_eq!(host.export_task("de", "t").unwrap_err().kind, "not_found");
    app.call("close", json!({"dir": "de"}));
    let (spent, _) = refused_at(&mut app, "de");
    assert!(near(spent, 4.40), "{spent}");
    sent_once(&mut app, "de", spent, cost);
    // Asked again, it finishes.
    std::fs::remove_file(root.join("de/fail-removal")).unwrap();
    let done = app.call("task_drop", json!({"dir": "de", "task": "t"}));
    assert!(done["error"].is_null(), "{done}");
    assert!(!root.join("de/tasks").join(&key).exists());
}

#[test]
fn a_task_still_running_or_needed_is_neither_exported_nor_removed() {
    let root = temp_root("task-drop-refused");
    let host = embedded(Box::new(DirPlaces(root.clone())));
    // A turn waits on a task it called (its request held: the app has not answered it).
    let held: Held = Held::default();
    let hold = held.clone();
    let mut app = App::new(&host)
        .script(
            "dw",
            vec![Reply::Call(
                "task",
                json!({"prompt": "[t:dw-task] look around"}),
            )],
        )
        .script(
            "df",
            vec![
                Reply::Call("task", json!({"prompt": "[t:df-task] write b.txt"})),
                Reply::Text("the task finished"),
            ],
        )
        .script(
            "#",
            vec![
                Reply::Call(
                    "write_file",
                    json!({"path": "b.txt", "content": "from the task"}),
                ),
                Reply::Text("child done"),
            ],
        );
    app.hook = Some(Box::new(move |_, session: &str, id, _| {
        if session.starts_with("dw#") && hold.borrow().is_none() {
            *hold.borrow_mut() = Some((session.to_string(), id));
            return Some(vec![]);
        }
        None
    }));
    app.call(
        "create",
        json!({"dir": "dw", "provider": "fake", "tasks": true}),
    );
    let line = json!({"jsonrpc": "2.0", "id": 77, "method": "prompt",
        "params": {"dir": "dw", "text": "[t:dw] start it"}});
    let frames = host.handle(line.to_string().as_bytes());
    app.drive(frames);
    let label = held.borrow().clone().expect("the task waits").0;
    // The turn waits on it: it is running, and listed so (no row names a task a turn
    // waits for).
    let listed = app.call("tasks", json!({"dir": "dw"}))["result"]["tasks"].clone();
    assert_eq!(listed[0]["task"], label.as_str(), "{listed}");
    assert_eq!(
        (&listed[0]["orphan"], &listed[0]["state"]),
        (&json!(true), &json!("running")),
        "{listed}"
    );
    let refused = app.call("task_drop", json!({"dir": "dw", "task": label}));
    assert_eq!(kind(&refused), Some("busy"), "{refused}");
    assert_eq!(host.export_task("dw", &label).unwrap_err().kind, "busy");
    // Its request lost: the turn stops, waiting for the task to be settled in its own
    // session. Its store is still needed.
    let (session, id) = held.borrow().clone().unwrap();
    let frames = host
        .deliver(
            &session,
            id,
            answer_of(&json!({"kind": "dropped"})).unwrap(),
        )
        .unwrap();
    app.drive(frames);
    assert!(app.run.replies.iter().any(|reply| reply["id"] == 77));
    let listed = app.call("tasks", json!({"dir": "dw"}))["result"]["tasks"].clone();
    assert_eq!(listed[0]["state"], "running", "{listed}");
    let refused = app.call("task_drop", json!({"dir": "dw", "task": label}));
    assert_eq!(kind(&refused), Some("conflict"), "{refused}");
    assert_eq!(host.export_task("dw", &label).unwrap_err().kind, "conflict");
    let unknown = app.call("task_drop", json!({"dir": "dw", "task": "nope"}));
    assert_eq!(kind(&unknown), Some("not_found"), "{unknown}");
    assert!(root.join("dw/tasks").read_dir().unwrap().count() == 1);

    // A task a finished turn waited for: its directory goes, the files it merged stay.
    app.call(
        "create",
        json!({"dir": "df", "provider": "fake", "tasks": true}),
    );
    let done = app.call("prompt", json!({"dir": "df", "text": "[t:df] delegate"}));
    assert!(done["error"].is_null(), "{done}");
    let listed = app.call("tasks", json!({"dir": "df"}))["result"]["tasks"].clone();
    assert_eq!(
        (
            &listed[0]["orphan"],
            &listed[0]["state"],
            &listed[0]["store"]
        ),
        (&json!(true), &json!("finished"), &json!(true)),
        "{listed}"
    );
    let label = listed[0]["task"].as_str().unwrap().to_string();
    let file = host.export_task("df", &label).unwrap();
    assert!(spindle_store::bundle::read(&file).is_ok());
    let dropped = app.call("task_drop", json!({"dir": "df", "task": label}));
    assert!(dropped["result"]["dropped"]["task"].is_null(), "{dropped}");
    let key = label.rsplit('#').next().unwrap();
    assert!(!root.join("df/tasks").join(key).exists());
    let read = app.call("read", json!({"dir": "df", "path": "b.txt"}));
    assert_eq!(read["result"]["content"], "from the task", "{read}");
    let listed = app.call("tasks", json!({"dir": "df"}))["result"]["tasks"].clone();
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(
        (
            &listed[0]["orphan"],
            &listed[0]["store"],
            &listed[0]["state"]
        ),
        (&json!(true), &json!(false), &json!("finished")),
        "listed from its record: {listed}"
    );
}

// ---- durability of a retry's swap, a move's seal and a task's removal -----------------

/// Sessions in directories whose retry swaps its store as the package's places do: the
/// fresh store made whole beside the old one first (`store.fresh`), then the renames. A
/// `fail-swap` file in the session's directory stops a swap between its renames (a crash
/// there): nothing finishes it until the file goes, then the next open does.
struct SwapPlaces(PathBuf);
struct SwapStorage(DirStorage);
impl Places for SwapPlaces {
    fn storage(&self, dir: &str) -> Result<Box<dyn Storage>, String> {
        Ok(Box::new(SwapStorage(DirStorage(
            self.0.join(dir).join("store"),
        ))))
    }
    fn exists(&self, dir: &str) -> bool {
        let dir = self.0.join(dir);
        dir.join("store").exists() || dir.join("store.fresh").exists()
    }
}
impl SwapStorage {
    fn fresh(&self) -> PathBuf {
        self.0.0.with_extension("fresh")
    }
    fn stopped(&self) -> bool {
        self.0.0.parent().unwrap().join("fail-swap").exists()
    }
}
impl Storage for SwapStorage {
    fn label(&self) -> String {
        self.0.label()
    }
    fn open(&self, create: bool) -> spindle_store::Result<Store> {
        if !create && !self.0.0.exists() && self.fresh().exists() && !self.stopped() {
            std::fs::rename(self.fresh(), &self.0.0)?;
        }
        self.0.open(create)
    }
    fn set_aside(&self, store: Store) -> spindle_store::Result<(Store, String)> {
        self.0.set_aside(store)
    }
    fn replace_store(
        &self,
        store: Store,
        prepare: &mut dyn FnMut(&mut Store, &str) -> spindle_store::Result<()>,
    ) -> spindle_store::Result<(Store, String)> {
        let (fresh, aside) = (self.fresh(), self.0.0.with_extension("abandoned-1"));
        let name = aside.display().to_string();
        let _ = std::fs::remove_dir_all(&fresh);
        let mut made = Store::open(&fresh)?;
        prepare(&mut made, &name)?;
        drop(made);
        drop(store);
        std::fs::rename(&self.0.0, &aside)?;
        if self.stopped() {
            return Err(std::io::Error::other("the swap stopped between its renames").into());
        }
        std::fs::rename(&fresh, &self.0.0)?;
        Ok((Store::open(&self.0.0)?, name))
    }
    fn child(&self, key: &str) -> Box<dyn Storage> {
        self.0.child(key)
    }
    fn tasks(&self) -> Vec<String> {
        self.0.tasks()
    }
    fn import_store(&self, image: &[u8]) -> spindle_store::Result<()> {
        self.0.import_store(image)
    }
}

/// An app over `places` whose every reply costs $1.10 (as `priced`).
fn priced_in(places: Box<dyn Places>) -> Embedded {
    let mut catalog = catalog_json(&fake().base);
    catalog["providers"][0]["model_specs"]["fake-model"]["pricing"] = json!({
        "input_usd_per_mtok": 1000, "output_usd_per_mtok": 1000,
        "cache_read_usd_per_mtok": 1000, "cache_create_usd_per_mtok": 1000});
    Embedded::new(places, Catalog::from_json(&catalog).unwrap())
}

#[test]
fn a_retried_first_request_keeps_its_spend_and_prompt_through_a_swap_cut_short() {
    let root = temp_root("retry-swap");
    {
        let host = priced_in(Box::new(SwapPlaces(root.clone())));
        let mut app = App::new(&host);
        app.call("create", json!({"dir": "rs", "provider": "fake"}));
        app.hold = 1;
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": "prompt",
            "params": {"dir": "rs", "text": "[t:rs] hello"}});
        let frames = host.handle(line.to_string().as_bytes());
        app.drive(frames);
        assert_eq!(app.run.requests.len(), 1, "the request went out once");
        // The app ends here, the response never delivered.
    }
    {
        // The retry's swap stops between its renames: the old store is aside, the fresh
        // one not yet named.
        let host = priced_in(Box::new(SwapPlaces(root.clone())));
        let mut app = App::new(&host);
        std::fs::write(root.join("rs/fail-swap"), "").unwrap();
        let cut = app.call("retry", json!({"dir": "rs"}));
        assert!(cut.get("error").is_some(), "{cut}");
        assert!(app.run.requests.is_empty(), "nothing was sent");
        assert!(root.join("rs/store.abandoned-1").exists());
        assert!(root.join("rs/store.fresh").exists() && !root.join("rs/store").exists());
    }
    std::fs::remove_file(root.join("rs/fail-swap")).unwrap();
    let host = priced_in(Box::new(SwapPlaces(root.clone())));
    let mut app = App::new(&host).script("rs", vec![Reply::Text("again")]);
    // Opened, the fresh store holds the restarted turn and the old request's spend.
    let status = app.call("status", json!({"dir": "rs"}))["result"].clone();
    assert_eq!(status["head"], 1, "the restarted turn: {status}");
    let history = app.call("history", json!({"dir": "rs"}))["result"].clone();
    assert!(
        history["messages"].to_string().contains("[t:rs] hello"),
        "its prompt: {history}"
    );
    let unknown =
        |app: &mut App<'_>| app.call("usage", json!({"dir": "rs"}))["result"]["unknown"].clone();
    assert_eq!(
        unknown(&mut app),
        1,
        "the set-aside request may have been billed"
    );
    let sent = app.call("continue", json!({"dir": "rs"}));
    assert!(sent.get("result").is_some(), "{sent}");
    assert_eq!(app.run.requests.len(), 1, "sent once, from the fresh store");
    let status = app.call("status", json!({"dir": "rs"}))["result"].clone();
    assert_eq!(status["complete"], true, "{status}");
    assert_eq!(unknown(&mut app), 1, "still counted once");
}

#[test]
fn a_kept_bundle_seals_after_the_app_reads_its_spend() {
    let host = embedded(Box::new(MemoryPlaces));
    let mut app = App::new(&host).script("ks", vec![Reply::Text("first")]);
    app.call("create", json!({"dir": "ks", "provider": "fake"}));
    app.call("prompt", json!({"dir": "ks", "text": "one"}));
    let bytes = host.export_session("ks", true, None).unwrap();
    let kept = spindle_store::bundle::read(&bytes).unwrap();
    // Reading the spend keeps the ledger in the store: the seal's check sees the same.
    let usage = app.call("usage", json!({"dir": "ks"}));
    assert!(usage.get("result").is_some(), "{usage}");
    let sealed = host
        .seal_session("ks", kept.head, kept.source_state.as_deref().unwrap())
        .unwrap();
    assert_eq!(sealed["sealed"], true, "{sealed}");
}

#[test]
fn an_app_task_left_running_is_neither_exported_nor_removed() {
    let root = temp_root("task-left");
    {
        let host = embedded(Box::new(DirPlaces(root.clone())));
        let mut app = App::new(&host).script("tl", vec![Reply::Text("ready")]);
        app.call(
            "create",
            json!({"dir": "tl", "provider": "fake", "tasks": true}),
        );
        app.call("prompt", json!({"dir": "tl", "text": "[t:tl] hello"}));
        // The app's task sends its request; the app ends before it is answered.
        app.hold = 2;
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": "task_start",
            "params": {"dir": "tl", "task": "t", "prompt": "[t:tl-task] look"}});
        let frames = host.handle(line.to_string().as_bytes());
        app.drive(frames);
        assert_eq!(app.run.requests.len(), 2, "the task's request went out");
        assert!(app.run.replies.is_empty());
    }
    let host = embedded(Box::new(DirPlaces(root.clone())));
    let mut app = App::new(&host);
    let listed = app.call("tasks", json!({"dir": "tl"}))["result"]["tasks"].clone();
    assert_eq!(
        (&listed[0]["state"], &listed[0]["queued"]),
        (&json!("running"), &json!(false)),
        "{listed}"
    );
    // `task_start` with its id takes it up again: its store is still needed.
    let refused = app.call("task_drop", json!({"dir": "tl", "task": "t"}));
    assert_eq!(kind(&refused), Some("conflict"), "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("can still go on"),
        "{refused}"
    );
    assert_eq!(host.export_task("tl", "t").unwrap_err().kind, "conflict");
    let label = listed[0]["task"].as_str().unwrap();
    let key = label.rsplit('#').next().unwrap();
    assert!(root.join("tl/tasks").join(key).exists());
}

#[test]
fn a_removed_tasks_directory_left_in_tasks_goes_when_the_session_opens() {
    let root = temp_root("task-drop-open");
    let host = embedded(Box::new(DirPlaces(root.clone())));
    let mut app = App::new(&host)
        .script("do", vec![Reply::Text("ready")])
        .script("#", vec![Reply::Text("task done")]);
    app.call(
        "create",
        json!({"dir": "do", "provider": "fake", "tasks": true}),
    );
    app.call("prompt", json!({"dir": "do", "text": "[t:do] hello"}));
    let started = app.call(
        "task_start",
        json!({"dir": "do", "task": "t", "prompt": "[t:do-task] go"}),
    );
    assert_eq!(started["result"]["state"], "finished", "{started}");
    let label = started["result"]["task"].as_str().unwrap().to_string();
    let key = label.rsplit('#').next().unwrap();
    // A stop once the removal's record was kept, before the directory moved: a native
    // host, which reads no such record, would find the task there.
    std::fs::write(root.join("do/fail-removal"), "before").unwrap();
    let cut = app.call("task_drop", json!({"dir": "do", "task": "t"}));
    assert_eq!(kind(&cut), Some("failed"), "{cut}");
    assert!(root.join("do/tasks").join(key).exists());
    std::fs::remove_file(root.join("do/fail-removal")).unwrap();
    app.call("close", json!({"dir": "do"}));
    let status = app.call("status", json!({"dir": "do"}));
    assert!(status.get("result").is_some(), "{status}");
    assert!(
        !root.join("do/tasks").join(key).exists(),
        "the open moved it out of tasks/ and removed it"
    );
    assert!(!root.join("do/tasks.removing").exists());
    let listed = app.call("tasks", json!({"dir": "do"}))["result"]["tasks"].clone();
    assert_eq!(listed[0]["store"], false, "{listed}");
    assert!(listed[0]["dropped"].is_object(), "{listed}");
}
