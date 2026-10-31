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
                let body =
                    spindle_store::protocol::base64_decode(work["body"].as_str().unwrap()).unwrap();
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
const SUMMARY: &str = "SUMMARY: three long answers were given; carry on with four.";

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
