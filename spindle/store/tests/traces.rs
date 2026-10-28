//! Characterization traces of the turn loop: for fixed scenarios against a loopback
//! fake provider, the exact request bodies the provider received, the event stream
//! (streamed model text and command output merged, the session's directory written as
//! `<dir>`) and a dump of the store's committed records (stored values by digest), each
//! compared with its fixture in `tests/fixtures/traces/`. They pin what the turn loop
//! does today, so a change to how it is structured can be checked byte for byte.
//! `SPINDLE_TRACE_UPDATE=1` rewrites the fixtures (review the diff before keeping it).
#![cfg(all(feature = "agent", target_os = "linux"))]
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use spindle_store::agent::{
    self, AppAnswer, AppCall, AppTools, Event, Observer, Session, Settings,
};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};

/// What the fake provider answers, request by request, in a scenario.
#[derive(Clone)]
enum Reply {
    Text(&'static str),
    LongText(String),
    Hold,
    /// A tool call: name and arguments.
    Call(&'static str, Value),
    /// HTTP 429 with `Retry-After: 0`.
    Refused,
    Overflow,
    /// The connection closes with no reply (an unknown outcome).
    HangUp,
}

/// A scenario's script, and the request bodies it received (as sent).
type Scenario = (Vec<Reply>, Vec<String>);

/// The fake and the root every scenario's session lives under.
struct Fixture {
    root: PathBuf,
    /// The fake's base URL: its port differs run to run, so stored values that name it
    /// (route bindings) are compared with it written as `<base>`.
    base: String,
    scenarios: Mutex<BTreeMap<String, Scenario>>,
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<&'static Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("spindle-traces-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("dext");
        private_dir(&home);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let fixture: &'static Fixture = Box::leak(Box::new(Fixture {
            root: root.clone(),
            base: base.clone(),
            scenarios: Mutex::default(),
        }));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                std::thread::spawn(move || {
                    let Some(body) = read_request(&mut stream) else {
                        return;
                    };
                    let reply = answer(fixture, &body);
                    let _ = stream.write_all(reply.as_bytes());
                    if reply.contains("X-Spindle-Hold: true") {
                        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
                        let _ = stream.read(&mut [0; 1]);
                    }
                });
            }
        });
        let providers = json!({"version": 4, "active_provider": "fake", "providers": [{
            "id": "fake", "display_name": "Fake", "api_provider": "openai",
            "request_contract": "openai-chat-completions", "base_url": base,
            "default_model": "fake-model", "models": ["fake-model"], "requires_api_key": false,
            "model_specs": {"fake-model": {"context_window": 200000, "max_output_tokens": 4096}}
        }]});
        std::fs::write(
            home.join("providers.json"),
            serde_json::to_vec_pretty(&providers).unwrap(),
        )
        .unwrap();
        // SAFETY: set once, before any test thread reads the environment (every test
        // calls `fixture()` first and waits here until it is done).
        unsafe {
            std::env::set_var("DEXT_HOME", &home);
        }
        fixture
    })
}

/// The scenario a request belongs to: the `[t:name]` tag of its first user message.
fn scenario_of(body: &Value) -> Option<String> {
    let messages = body["messages"].as_array()?;
    for message in messages.iter().filter(|m| m["role"] == "user") {
        let text = match &message["content"] {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<String>(),
            _ => continue,
        };
        if let Some(start) = text.find("[t:") {
            let rest = &text[start + 3..];
            return rest.find(']').map(|end| rest[..end].to_string());
        }
    }
    None
}

fn answer(fixture: &Fixture, raw: &[u8]) -> String {
    let body: Value = serde_json::from_slice(raw).unwrap_or(Value::Null);
    let Some(name) = scenario_of(&body) else {
        return status(404, "no scenario");
    };
    let reply = {
        let mut scenarios = fixture.scenarios.lock().unwrap();
        let Some((script, seen)) = scenarios.get_mut(&name) else {
            return status(404, "unknown scenario");
        };
        let index = seen.len();
        seen.push(String::from_utf8_lossy(raw).into_owned());
        script
            .get(index)
            .cloned()
            .unwrap_or(Reply::Text("(script ended)"))
    };
    let index = fixture.scenarios.lock().unwrap()[&name].1.len();
    match reply {
        Reply::Text(text) => sse(json!({"content": text})),
        Reply::LongText(text) => sse(json!({"content": text})),
        Reply::Hold => {
            let chunk = json!({"id":"c","object":"chat.completion.chunk",
                "choices":[{"index":0,"delta":{"content":"unfinished"},"finish_reason":null}]});
            let body = format!("data: {chunk}\n\n");
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nX-Spindle-Hold: true\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len() + 10000
            )
        }
        Reply::Call(name, arguments) => sse(json!({"tool_calls": [{"index": 0,
            "id": format!("call_{index}"), "type": "function",
            "function": {"name": name, "arguments": arguments.to_string()}}]})),
        Reply::Refused => {
            let error = r#"{"error":{"message":"rate limited","type":"rate_limit"}}"#;
            format!(
                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{error}",
                error.len()
            )
        }
        Reply::Overflow => {
            let error = r#"{"error":{"message":"maximum context length exceeded","type":"context_length_exceeded"}}"#;
            format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{error}",
                error.len()
            )
        }
        Reply::HangUp => String::new(),
    }
}

fn status(code: u16, text: &str) -> String {
    format!(
        "HTTP/1.1 {code} Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    )
}

fn sse(delta: Value) -> String {
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
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Vec<u8>> {
    let mut request = vec![];
    let mut chunk = [0; 65536];
    loop {
        let length = stream.read(&mut chunk).ok()?;
        if length == 0 {
            return None;
        }
        request.extend_from_slice(&chunk[..length]);
        let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
        let size = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))?
            .trim()
            .parse::<usize>()
            .ok()?;
        if request.len() >= end + 4 + size {
            return Some(request[end + 4..end + 4 + size].to_vec());
        }
    }
}

fn private_dir(path: &Path) {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .unwrap();
}

/// Every event, in order, with adjacent streamed text (and output of one stream)
/// merged: how a reply is cut into chunks depends on the network, not the loop.
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<Value>>,
    app: Mutex<Option<Arc<dyn AppTools>>>,
    on_event: Mutex<Option<Box<EventHook>>>,
}

type EventHook = dyn Fn(&Value) + Send;

impl Observer for Recorder {
    fn event(&self, event: Event<'_>) {
        let mut value = event.to_json();
        if value["type"] == "output" {
            let bytes = spindle_store::api::base64_decode(value["data"].as_str().unwrap_or(""))
                .unwrap_or_default();
            value = json!({"type": "output", "stream": value["stream"],
                "text": String::from_utf8_lossy(&bytes)});
        }
        if let Some(hook) = self.on_event.lock().unwrap().as_ref() {
            hook(&value);
        }
        let mut events = self.events.lock().unwrap();
        if let Some(last) = events.last_mut() {
            let merges = match value["type"].as_str() {
                Some("model_delta") => last["type"] == "model_delta",
                Some("output") => last["type"] == "output" && last["stream"] == value["stream"],
                _ => false,
            };
            if merges {
                let text = format!(
                    "{}{}",
                    last["text"].as_str().unwrap_or(""),
                    value["text"].as_str().unwrap_or("")
                );
                last["text"] = json!(text);
                return;
            }
        }
        events.push(value);
    }
    fn app_tools(&self) -> Option<Arc<dyn AppTools>> {
        self.app.lock().unwrap().clone()
    }
}

/// The app of the `app` scenario: answers `lookup` with text, a file and a state change.
struct Lookup;
impl AppTools for Lookup {
    fn call(&self, call: &AppCall<'_>, _interrupt: &AtomicBool) -> AppAnswer {
        AppAnswer::from_json(&json!({"content": format!("Found {}.", call.input["q"]),
            "files": {"write": {"notes.md": "# Notes\n"}}, "state": {"looked": true}}))
        .unwrap()
    }
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .take(12)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `bytes` with every `from` written as `to`.
fn replaced(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at..].starts_with(from) {
            out.extend_from_slice(to);
            at += from.len();
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    out
}

/// The store's committed records, stored values by the digest of what they hold
/// (decoded, the fake's base URL written as `<base>`), so a change in what is written
/// shows up here and nothing else does.
fn dump(store: &Path, base: &str) -> Value {
    let db = Connection::open_with_flags(
        store.join("authority.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let tables: [(&str, &str, &[&str]); 9] = [
        (
            "admissions",
            "sequence",
            &["sequence", "scope", "command_id", "kind", "payload"],
        ),
        (
            "revisions",
            "revision",
            &["revision", "scope", "engine", "continuation", "workspace"],
        ),
        ("proposals", "revision", &["id", "revision", "canonical"]),
        ("consumed", "admission", &["admission", "revision"]),
        (
            "intents",
            "revision, position",
            &["id", "revision", "position", "adapter", "input"],
        ),
        (
            "attempts",
            "id",
            &[
                "id",
                "effect",
                "owner_epoch",
                "implementation",
                "grant_reference",
            ],
        ),
        ("receipts", "attempt", &["attempt", "result", "workspace"]),
        (
            "attempt_failures",
            "attempt",
            &["attempt", "status", "detail"],
        ),
        ("app_state", "revision", &["revision", "value"]),
    ];
    let mut out = Map::new();
    for (table, order, columns) in tables {
        let sql = format!("SELECT {} FROM {table} ORDER BY {order}", columns.join(","));
        let mut query = db.prepare(&sql).unwrap();
        let rows: Vec<Value> = query
            .query_map([], |row| {
                let mut record = Map::new();
                for (index, column) in columns.iter().enumerate() {
                    let value = match row.get_ref(index)? {
                        rusqlite::types::ValueRef::Null => Value::Null,
                        rusqlite::types::ValueRef::Integer(n) => json!(n),
                        rusqlite::types::ValueRef::Real(n) => json!(n),
                        rusqlite::types::ValueRef::Text(text) => {
                            json!(String::from_utf8_lossy(text))
                        }
                        rusqlite::types::ValueRef::Blob(bytes) => {
                            let held = spindle_store::codec::load(&db, bytes, 1 << 30)
                                .unwrap_or_else(|_| bytes.to_vec());
                            json!(digest(&replaced(&held, base.as_bytes(), b"<base>")))
                        }
                    };
                    record.insert(column.to_string(), value);
                }
                Ok(Value::Object(record))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        out.insert(table.into(), Value::Array(rows));
    }
    Value::Object(out)
}

/// Runs scenario `name` (its first prompt `prompt` carries the tag) against `script`,
/// with `prepare` given the session before the prompt, and checks the trace.
fn trace(
    name: &str,
    script: Vec<Reply>,
    app: Option<Arc<dyn AppTools>>,
    prepare: impl FnOnce(&mut Settings),
    before: impl FnOnce(&mut Session),
    prompt: &str,
) {
    trace_run(name, script, app, prepare, |mut session, _, _| {
        before(&mut session);
        session.prompt(&format!("[t:{name}] {prompt}"))
    });
}

fn trace_run(
    name: &str,
    script: Vec<Reply>,
    app: Option<Arc<dyn AppTools>>,
    prepare: impl FnOnce(&mut Settings),
    run: impl FnOnce(Session, &Path, Arc<Recorder>) -> agent::Result<()>,
) {
    let fixture = fixture();
    fixture
        .scenarios
        .lock()
        .unwrap()
        .insert(name.into(), (script, vec![]));
    let dir = fixture.root.join(name);
    let recorder = Arc::new(Recorder {
        events: Mutex::default(),
        app: Mutex::new(app),
        on_event: Mutex::default(),
    });
    let mut settings = Settings::resolve("fake", Some("fake-model".into())).unwrap();
    settings.isolation = agent::Isolation::Off;
    prepare(&mut settings);
    let session = Session::create(&dir, settings, vec![], recorder.clone()).unwrap();
    let outcome = run(session, &dir, recorder.clone());
    assert!(outcome.is_ok(), "scenario {name} failed: {outcome:?}");
    let requests = fixture.scenarios.lock().unwrap()[name].1.clone();
    let events = recorder.events.lock().unwrap().clone();
    let place = dir.to_string_lossy().into_owned();
    let found = json!({
        "outcome": match outcome { Ok(()) => json!("ok"), Err(error) => json!(error) },
        "requests": requests,
        "events": events,
        "store": dump(&dir.join("store"), &fixture.base),
    });
    let text = serde_json::to_string_pretty(&found)
        .unwrap()
        .replace(&place, "<dir>");
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/traces")
        .join(format!("{name}.json"));
    if std::env::var_os("SPINDLE_TRACE_UPDATE").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{text}\n")).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "no fixture {}: run with SPINDLE_TRACE_UPDATE=1",
            path.display()
        )
    });
    if expected.trim_end() != text {
        let at = expected
            .lines()
            .zip(text.lines())
            .position(|(a, b)| a != b)
            .unwrap_or(expected.lines().count().min(text.lines().count()));
        panic!(
            "trace `{name}` differs from {} at line {}:\n  expected: {}\n  found:    {}",
            path.display(),
            at + 1,
            expected.lines().nth(at).unwrap_or("<end>"),
            text.lines().nth(at).unwrap_or("<end>")
        );
    }
}

fn plain(_: &mut Settings) {}
fn nothing(_: &mut Session) {}

struct EmbeddedCredentials {
    sends: std::sync::atomic::AtomicUsize,
}

impl agent::Credentials for EmbeddedCredentials {
    fn route(&self, provider: &str, model: Option<&str>) -> agent::Result<agent::Route> {
        assert_eq!(provider, "embedded");
        let mut route = agent::Credentials::route(&agent::NativeCredentials, "fake", model)?;
        route.profile.id = "embedded".into();
        Ok(route)
    }
    fn live_headers(
        &self,
        route: &agent::Route,
        _: &str,
    ) -> agent::Result<reqwest::header::HeaderMap> {
        assert_eq!(route.profile.id, "embedded");
        self.sends.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Default::default())
    }
    fn auth_kind(
        &self,
        _: &agent::Route,
    ) -> agent::Result<dext_provider_catalog::provider::RuntimeAuthKind> {
        Err("not a subscription".into())
    }
}

#[test]
fn embedding_supplies_the_route_and_fresh_credentials() {
    trace_run(
        "credentials",
        vec![Reply::Text("embedded reply")],
        None,
        |settings| settings.provider = "embedded".into(),
        |session, dir, recorder| {
            drop(session);
            let credentials = Arc::new(EmbeddedCredentials {
                sends: Default::default(),
            });
            let mut session = Session::open_with_credentials(dir, recorder, credentials.clone())?;
            session.prompt("[t:credentials] hello")?;
            assert_eq!(
                credentials.sends.load(std::sync::atomic::Ordering::SeqCst),
                1
            );
            Ok(())
        },
    );
}

#[test]
fn a_plain_reply() {
    trace(
        "plain",
        vec![Reply::Text("Hello.")],
        None,
        plain,
        nothing,
        "say hello",
    );
}

#[test]
fn file_tools() {
    trace(
        "files",
        vec![
            Reply::Call("write_file", json!({"path": "a.txt", "content": "hi\n"})),
            Reply::Call("read_file", json!({"path": "a.txt"})),
            Reply::Text("done"),
        ],
        None,
        plain,
        nothing,
        "write and read a file",
    );
}

#[test]
fn a_command_on_the_host() {
    trace(
        "bash",
        vec![
            Reply::Call("bash", json!({"command": "echo one; echo two"})),
            Reply::Text("ran"),
        ],
        None,
        plain,
        nothing,
        "run something",
    );
}

#[test]
fn a_message_sent_while_the_turn_runs() {
    trace(
        "steer",
        vec![
            Reply::Call("bash", json!({"command": "echo one"})),
            Reply::Text("noted"),
        ],
        None,
        plain,
        // Waiting before the turn starts: read at its first step boundary.
        |session| {
            session
                .steering_handle()
                .lock()
                .unwrap()
                .push("also say hi".into())
        },
        "run something",
    );
}

#[test]
fn a_refused_request_sent_again() {
    trace(
        "refused",
        vec![
            Reply::Call("bash", json!({"command": "true"})),
            Reply::Refused,
            Reply::Text("after the refusal"),
        ],
        None,
        plain,
        nothing,
        "run something",
    );
}

#[test]
fn a_dropped_connection_sent_again() {
    trace(
        "dropped",
        vec![
            Reply::Call("bash", json!({"command": "true"})),
            Reply::HangUp,
            Reply::Text("after the drop"),
        ],
        None,
        |settings| settings.resend_unknown = true,
        nothing,
        "run something",
    );
}

#[test]
fn an_app_tool_with_files_and_state() {
    trace(
        "app",
        vec![
            Reply::Call("lookup", json!({"q": "notes"})),
            Reply::Text("found them"),
        ],
        Some(Arc::new(Lookup)),
        |settings| {
            settings.tools = agent::app_tools::tools_from_json(&json!([{"name": "lookup",
                "description": "Looks notes up.", "input_schema": {"type": "object",
                "properties": {"q": {"type": "string"}}}}]))
            .unwrap();
            settings.instructions = Some("You help with notes.".into());
        },
        |session| {
            session
                .state_with_next_prompt(json!({"topic": "notes"}))
                .unwrap()
        },
        "find my notes",
    );
}

#[test]
fn a_tool_the_session_does_not_offer_is_refused_in_order() {
    trace(
        "tool-refused",
        vec![
            Reply::Call("bash", json!({"command": "true"})),
            Reply::Text("refused"),
        ],
        None,
        |settings| settings.builtin_tools = agent::BuiltinTools::Files,
        nothing,
        "do not run commands",
    );
}

#[test]
fn send_now_cuts_a_stream_and_reads_the_queued_message() {
    trace_run(
        "cut",
        vec![
            Reply::Call("bash", json!({"command":"true"})),
            Reply::Hold,
            Reply::Text("after the cut"),
        ],
        None,
        plain,
        |mut session, _, recorder| {
            let hurry = session.hurry_handle();
            let queue = session.steering_handle();
            *recorder.on_event.lock().unwrap() = Some(Box::new(move |event| {
                if event["type"] == "model_delta" && event["text"] == "unfinished" {
                    queue.lock().unwrap().push("read this now".into());
                    hurry.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }));
            session.prompt("[t:cut] run something")
        },
    );
}

struct StopApp;
impl AppTools for StopApp {
    fn call(&self, _: &AppCall<'_>, _: &AtomicBool) -> AppAnswer {
        AppAnswer::Stopped
    }
}

#[test]
fn an_unanswered_app_call_is_asked_again_after_reopen() {
    trace_run(
        "reopen-app",
        vec![
            Reply::Call("lookup", json!({"q":"notes"})),
            Reply::Text("found"),
        ],
        Some(Arc::new(StopApp)),
        |settings| {
            settings.tools = agent::app_tools::tools_from_json(&json!([{"name":"lookup","description":"Looks notes up.","input_schema":{"type":"object"}}])).unwrap();
        },
        |mut session, dir, recorder| {
            assert!(session.prompt("[t:reopen-app] look up notes").is_err());
            drop(session);
            *recorder.app.lock().unwrap() = Some(Arc::new(Lookup));
            Session::open(dir, recorder)?.resume()
        },
    );
}

#[test]
fn a_reserved_model_request_is_not_resent_without_consent() {
    reopen_model("reopen-model-no", false);
}

#[test]
fn a_reserved_model_request_is_resent_with_consent() {
    reopen_model("reopen-model-yes", true);
}

fn reopen_model(name: &str, resend: bool) {
    trace_run(
        name,
        vec![
            Reply::Call("bash", json!({"command":"true"})),
            Reply::Hold,
            Reply::Text("after reopen"),
        ],
        None,
        |settings| settings.resend_unknown = resend,
        |mut session, dir, recorder| {
            let stop = session.interrupt_handle();
            *recorder.on_event.lock().unwrap() = Some(Box::new(move |event| {
                if event["type"] == "model_delta" && event["text"] == "unfinished" {
                    stop.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }));
            assert!(
                session
                    .prompt(&format!("[t:{name}] run something"))
                    .is_err()
            );
            drop(session);
            let outcome = Session::open(dir, recorder)?.resume();
            if resend {
                outcome
            } else {
                assert!(outcome.unwrap_err().contains("unknown outcome"));
                Ok(())
            }
        },
    );
}

#[test]
fn a_builtin_reserved_by_an_older_owner_is_never_run_again() {
    trace_run(
        "reopen-tool",
        vec![
            Reply::Call("bash", json!({"command":"printf should-not-run > marker"})),
            Reply::Text("interrupted"),
        ],
        None,
        plain,
        |mut session, dir, recorder| {
            let stop = session.interrupt_handle();
            *recorder.on_event.lock().unwrap() = Some(Box::new(move |event| {
                if event["type"] == "tool_call" {
                    stop.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }));
            assert!(session.prompt("[t:reopen-tool] run something").is_err());
            drop(session);
            *recorder.on_event.lock().unwrap() = None;
            let mut store =
                spindle_store::Store::open(&dir.join("store")).map_err(|e| e.to_string())?;
            let snapshot = store.snapshot().map_err(|e| e.to_string())?;
            let state =
                dext_transcript::turn::TurnContinuation::restore(&snapshot.continuation).unwrap();
            let dext_transcript::turn::Effect::Tool { id, .. } =
                state.pending_effects().unwrap().remove(0)
            else {
                panic!("tool")
            };
            store
                .reserve_attempt(&spindle_store::completion::Attempt {
                    id: format!("a:{id}"),
                    effect: id,
                    owner_epoch: store.epoch(),
                    implementation: dext_native::strict::StrictExecutor::host().implementation(),
                    grant_reference: "spindle-cli-host-unisolated".into(),
                })
                .map_err(|e| e.to_string())?;
            drop(store);
            let mut session = Session::open(dir, recorder)?;
            session.resume()?;
            assert!(session.read("marker").is_err());
            Ok(())
        },
    );
}

#[test]
fn compaction_summarizes_the_overflowed_history() {
    trace_run(
        "compact",
        vec![
            Reply::LongText("Earlier work. ".repeat(6000)),
            Reply::Call("read_file", json!({"path":"missing.txt"})),
            Reply::Overflow,
            Reply::Text("summary"),
            Reply::Text("continued"),
        ],
        None,
        |settings| settings.compaction = agent::Compaction::Model,
        |mut session, _, _| {
            session.prompt("[t:compact] first turn")?;
            session.prompt("[t:compact] continue")
        },
    );
}
