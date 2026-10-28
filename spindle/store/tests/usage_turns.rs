//! Spend attribution and budgets across retries, rewinds and restarts, against a
//! loopback fake provider (no paid calls). Each test runs its own session; they share
//! one fake and one catalog, set up once before any test reads the environment.
#![cfg(all(feature = "agent", target_os = "linux"))]
use dext_transcript::checkpoint::NativeResponse;
use dext_transcript::turn::{Effect, TurnContinuation};
use dext_transcript::{Block, Usage};
use serde_json::{Value, json};
use spindle_store::agent::{self, Event, Observer, Session, Settings};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Per-MTok prices of the fake model: input, output, cache read, cache write.
const PRICES: [f64; 4] = [1.0, 2.0, 0.1, 1.25];
/// Usage of a turn's reply and of a compaction summary: prompt tokens (cached
/// included), cached, output.
const PARENT: (u64, u64, u64) = (1000, 200, 100);
const SUMMARY: (u64, u64, u64) = (5000, 0, 300);
const SUMMARY_SYSTEM: &str = "You summarize the earlier part of a coding session";

fn cost((prompt, cached, output): (u64, u64, u64)) -> f64 {
    ((prompt - cached) as f64 * PRICES[0] + output as f64 * PRICES[1] + cached as f64 * PRICES[2])
        / 1e6
}

fn parent() -> f64 {
    cost(PARENT)
}

fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-12, "{a} != {b}");
}

/// The fake and the root every test's session lives under.
struct Fixture {
    root: PathBuf,
    /// Requests seen, by the tag of the prompt that started the conversation.
    seen: Arc<Mutex<Vec<String>>>,
    /// Tags that already fired once (and "healed" once drops should end).
    fired: Arc<Mutex<BTreeSet<String>>>,
}

/// A prompt may carry tags: `[write]` asks for a write_file call (answered after its
/// result), `[big]` for a long answer, `[429:t]` refuses the first request after a tool
/// result with HTTP 429, `[hangup:t]` cuts the first request off without a reply (an
/// unknown outcome, which the session sends again by itself) and `[drops:t]` cuts every
/// request off until `heal()`. `[slow:t]` streams its first words, then takes `SLOW`
/// to finish (once; `[slowtool:t]` likewise for the request after a tool result).
/// `small-model` has a window small enough to compact.
fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("spindle-usage-turns-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("dext");
        private_dir(&home);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = seen.clone();
        let fired: Arc<Mutex<BTreeSet<String>>> = Arc::default();
        let shared = fired.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                let (log, fired) = (log.clone(), fired.clone());
                std::thread::spawn(move || {
                    let Some(body) = read_request(&mut stream) else {
                        return;
                    };
                    if slow(&body, &log, &fired) {
                        stream_slowly(&mut stream);
                        return;
                    }
                    let reply = answer(&body, &log, &fired);
                    let _ = stream.write_all(reply.as_bytes());
                });
            }
        });
        let pricing = json!({"input_usd_per_mtok": PRICES[0], "output_usd_per_mtok": PRICES[1],
            "cache_read_usd_per_mtok": PRICES[2], "cache_create_usd_per_mtok": PRICES[3]});
        let providers = json!({"version": 4, "active_provider": "fake", "providers": [{
            "id": "fake", "display_name": "Fake", "api_provider": "openai",
            "request_contract": "openai-chat-completions", "base_url": base,
            "default_model": "fake-model", "models": ["fake-model", "small-model"], "requires_api_key": false,
            "model_specs": {"fake-model": {"context_window": 200000, "max_output_tokens": 4096, "pricing": pricing},
                "small-model": {"context_window": 20000, "max_output_tokens": 4096, "pricing": pricing}}
        }]});
        std::fs::write(
            home.join("providers.json"),
            serde_json::to_vec_pretty(&providers).unwrap(),
        )
        .unwrap();
        let policy = root.join("code-policy.json");
        std::fs::write(
            &policy,
            serde_json::to_vec(&json!({"version": 1, "models": [{"provider": "fake"}], "tools": []}))
                .unwrap(),
        )
        .unwrap();
        // SAFETY: set once, before any test thread reads the environment (every test
        // calls `fixture()` first and waits here until it is done).
        unsafe {
            std::env::set_var("DEXT_HOME", &home);
            std::env::set_var("SPINDLE_CODE_POLICY", &policy);
        }
        Fixture {
            root,
            seen,
            fired: shared,
        }
    })
}

fn text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn answer(body: &Value, log: &Mutex<Vec<String>>, fired: &Mutex<BTreeSet<String>>) -> String {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    if messages
        .first()
        .is_some_and(|m| text(m).starts_with(SUMMARY_SYSTEM))
    {
        log.lock().unwrap().push("summary".into());
        return sse_with(
            json!({"content": "Earlier, a long answer."}),
            "stop",
            SUMMARY,
        );
    }
    let prompts: Vec<String> = messages
        .iter()
        .filter(|m| m["role"] == "user")
        .map(text)
        .collect();
    let latest = prompts.last().cloned().unwrap_or_default();
    log.lock().unwrap().push(latest.clone());
    let last = messages.last().cloned().unwrap_or_default();
    let tag = |kind: &str| -> Option<String> {
        let start = latest.find(&format!("[{kind}:"))?;
        let end = latest[start..].find(']')?;
        Some(latest[start..start + end + 1].to_string())
    };
    let once = |tag: Option<String>| tag.is_some_and(|tag| fired.lock().unwrap().insert(tag));
    if last["role"] == "tool" {
        if once(tag("429")) {
            let error = r#"{"error":{"message":"rate limited","type":"rate_limit"}}"#;
            return format!(
                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{error}",
                error.len()
            );
        }
        return sse(json!({"content": "done"}), "stop");
    }
    if once(tag("hangup")) {
        return String::new();
    }
    // `[drops:t]` hangs up every time: more drops than the session re-sends by itself.
    if tag("drops").is_some() && !fired.lock().unwrap().contains("healed") {
        return String::new();
    }
    if latest.contains("[write]") && last["role"] == "user" {
        let arguments = json!({"path": "a.txt", "content": "hi"}).to_string();
        return sse(
            json!({"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"write_file","arguments":arguments}}]}),
            "tool_calls",
        );
    }
    if latest.contains("[big]") {
        return sse(
            json!({"content": "lorem ipsum dolor sit amet ".repeat(4500)}),
            "stop",
        );
    }
    sse(json!({"content": "ok"}), "stop")
}

/// How long a slow reply takes after its first words.
const SLOW: Duration = Duration::from_secs(4);

/// Whether `body` gets the slow reply: the first request carrying `[slow:t]` in its
/// prompt, or the first after a tool result whose prompt carries `[slowtool:t]`.
fn slow(body: &Value, log: &Mutex<Vec<String>>, fired: &Mutex<BTreeSet<String>>) -> bool {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let Some(last) = messages.last() else {
        return false;
    };
    let latest = messages
        .iter()
        .filter(|m| m["role"] == "user")
        .map(text)
        .next_back()
        .unwrap_or_default();
    let kind = if last["role"] == "tool" {
        "[slowtool:"
    } else {
        "[slow:"
    };
    let Some(start) = latest.find(kind) else {
        return false;
    };
    let Some(end) = latest[start..].find(']') else {
        return false;
    };
    if !fired
        .lock()
        .unwrap()
        .insert(latest[start..start + end + 1].to_string())
    {
        return false;
    }
    log.lock().unwrap().push(latest);
    true
}

fn stream_slowly(stream: &mut std::net::TcpStream) {
    let chunk = |value: Value| format!("data: {value}\n\n");
    let first = chunk(
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Once upon a time"},"finish_reason":null}]}),
    );
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
    if stream
        .write_all(format!("{head}{first}").as_bytes())
        .is_err()
    {
        return;
    }
    let _ = stream.flush();
    std::thread::sleep(SLOW);
    let (prompt, cached, output) = PARENT;
    let rest = [
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":", the end."},"finish_reason":null}]}),
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"id":"c","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":prompt,"completion_tokens":output,"total_tokens":prompt + output,"prompt_tokens_details":{"cached_tokens":cached}}}),
    ];
    let mut body: String = rest.into_iter().map(chunk).collect();
    body.push_str("data: [DONE]\n\n");
    let _ = stream.write_all(body.as_bytes());
}

fn sse(delta: Value, finish: &str) -> String {
    sse_with(delta, finish, PARENT)
}

fn sse_with(delta: Value, finish: &str, (prompt, cached, output): (u64, u64, u64)) -> String {
    let chunks = [
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":finish}]}),
        json!({"id":"c","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":prompt,"completion_tokens":output,"total_tokens":prompt + output,"prompt_tokens_details":{"cached_tokens":cached}}}),
    ];
    let mut body: String = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect();
    body.push_str("data: [DONE]\n\n");
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Value> {
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
            return serde_json::from_slice(&request[end + 4..end + 4 + size]).ok();
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

/// Cost events as (call, turn so far, thread) and whether a reply was reused.
#[derive(Default)]
struct Seen {
    costs: Mutex<Vec<(f64, f64, f64)>>,
    reused: Mutex<usize>,
}
impl Observer for Seen {
    fn event(&self, event: Event<'_>) {
        match event {
            Event::Cost {
                usd, turn, total, ..
            } => self.costs.lock().unwrap().push((usd, turn, total)),
            Event::ResponseReused => *self.reused.lock().unwrap() += 1,
            _ => {}
        }
    }
}

fn session(name: &str) -> (PathBuf, Arc<Seen>, Session) {
    session_with(name, "fake-model")
}

fn session_with(name: &str, model: &str) -> (PathBuf, Arc<Seen>, Session) {
    let dir = fixture().root.join(name);
    let seen = Arc::new(Seen::default());
    let mut settings = Settings::resolve("fake", Some(model.into())).unwrap();
    settings.isolation = agent::Isolation::Auto;
    let session = Session::create(&dir, settings, vec![], seen.clone()).unwrap();
    (dir, seen, session)
}

/// Ends every `[drops:t]` hang-up.
fn heal() {
    fixture().fired.lock().unwrap().insert("healed".into());
}

fn sent(tag: &str) -> usize {
    fixture()
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|prompt| prompt.contains(tag))
        .count()
}

/// The head snapshot a phone sees, annotated with spend.
fn snapshot(dir: &Path, session: &mut Session) -> Value {
    let head = session.status().unwrap().revision;
    let mut view = spindle_store::view::View::open(&dir.join("store")).unwrap();
    let forks: Vec<Value> = view
        .read(|view| view.forks())
        .unwrap()
        .into_iter()
        .map(|f| json!({"revision": f.revision, "target": f.target, "abandoned": f.abandoned}))
        .collect();
    let messages: Vec<Value> = session
        .history()
        .unwrap()
        .iter()
        .map(|m| serde_json::to_value(m).unwrap())
        .collect();
    let mut snapshot = json!({"revision": head, "head": head, "phase": "Stopped(Complete)",
        "forks": forks, "messages": messages});
    agent::annotate_snapshot(dir, &mut snapshot);
    snapshot
}

#[test]
fn a_request_retried_after_a_429_stays_on_its_turn() {
    // A tool call, then the request after its result is refused with 429 and retried
    // automatically from the point before it, under a fresh execution id.
    let (dir, seen, mut session) = session("retried-429");
    session.prompt("[write] [429:retry] write a.txt").unwrap();
    assert_eq!(sent("[429:retry]"), 3, "tool call, refused request, retry");
    let report = agent::usage_report(&dir).unwrap();
    assert_eq!((report.total.calls, report.total.refused), (2, 1));
    close(report.uncertain_usd, 0.0);
    assert_eq!(report.turns.len(), 1);
    let turn = &report.turns[0];
    assert_eq!(
        (turn.usage.calls, turn.usage.refused, turn.abandoned),
        (2, 1, false),
        "the retried reply belongs to the turn it continues"
    );
    close(turn.usage.usd, 2.0 * parent());
    // The live cost line of the last reply counts the whole turn so far.
    let costs = seen.costs.lock().unwrap().clone();
    assert_eq!(costs.len(), 2, "{costs:?}");
    close(costs[1].1, 2.0 * parent());
    // The phone shows both calls under the prompt.
    let snapshot = snapshot(&dir, &mut session);
    let shown = &snapshot["turn_costs"][0];
    assert_eq!(
        (shown["calls"].as_u64(), shown["refused"].as_u64()),
        (Some(2), Some(1)),
        "{shown}"
    );
}

#[test]
fn rewinding_back_into_a_left_branch_restores_its_turns() {
    // Turn A, turn B, rewind to the end of A, turn C, rewind to the end of B: the history
    // shows A and B again, and C is the one left behind.
    let (dir, _, mut session) = session("rewind-back");
    session.prompt("alpha").unwrap();
    let after_a = session.status().unwrap().revision;
    session.prompt("[write] beta").unwrap();
    let after_b = session.status().unwrap().revision;
    session.rewind(after_a).unwrap();
    session.prompt("gamma").unwrap();
    session.rewind(after_b).unwrap();
    let report = agent::usage_report(&dir).unwrap();
    let turns: Vec<(&str, bool, u64)> = report
        .turns
        .iter()
        .map(|t| (t.prompt.as_str(), t.abandoned, t.usage.calls))
        .collect();
    assert_eq!(
        turns,
        vec![
            ("alpha", false, 1),
            ("[write] beta", false, 2),
            ("gamma", true, 1)
        ]
    );
    let snapshot = snapshot(&dir, &mut session);
    let messages = snapshot["messages"].as_array().unwrap();
    let shown: Vec<(String, String, u64)> = snapshot["turn_costs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|cost| {
            let index = cost["message"].as_u64().unwrap() as usize;
            (
                text(&messages[index]),
                cost["turn"].as_str().unwrap().to_string(),
                cost["calls"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        shown,
        vec![
            ("alpha".to_string(), report.turns[0].turn.clone(), 1),
            ("[write] beta".to_string(), report.turns[1].turn.clone(), 2),
        ],
        "each prompt carries its own turn's cost"
    );
}

#[test]
fn the_budget_is_shown_even_when_spend_cannot_be_read() {
    let (dir, _, mut session) = session("budget-shown");
    session.set_budget(Some(5.0)).unwrap();
    drop(session);
    // The store cannot be read (here: moved away), so no usage can be reported.
    std::fs::rename(dir.join("store"), dir.join("store.away")).unwrap();
    let mut snapshot = json!({"revision": 0, "head": 0, "forks": [], "messages": []});
    agent::annotate_snapshot(&dir, &mut snapshot);
    assert!(snapshot["usage"].is_null());
    assert_eq!(
        snapshot["budget"]["max_usd"].as_f64(),
        Some(5.0),
        "a phone that saves other settings must not see (and clear) an empty budget: {snapshot}"
    );
    std::fs::rename(dir.join("store.away"), dir.join("store")).unwrap();
}

/// A session's steering queue and its read-now flag.
type Handles = (Arc<Mutex<Vec<String>>>, Arc<AtomicBool>);

/// On the first streamed words of a reply, sends a message to the running turn and asks
/// for it to be read now (as the phone's Send now does); records what followed.
#[derive(Default)]
struct SendsNow {
    handles: Mutex<Option<Handles>>,
    events: Mutex<Vec<&'static str>>,
}
impl Observer for SendsNow {
    fn event(&self, event: Event<'_>) {
        let name = match event {
            Event::ModelDelta { .. } => match self.handles.lock().unwrap().take() {
                Some((queue, hurry)) => {
                    queue.lock().unwrap().push("also count the files".into());
                    hurry.store(true, Ordering::SeqCst);
                    "sent now"
                }
                None => return,
            },
            Event::ReplyCut => "cut",
            Event::ReplyNotCut => "not cut",
            Event::Steered { .. } => "steered",
            _ => return,
        };
        self.events.lock().unwrap().push(name);
    }
}

fn sends_now(name: &str) -> (PathBuf, Arc<SendsNow>, Session) {
    let dir = fixture().root.join(name);
    let observer = Arc::new(SendsNow::default());
    let mut settings = Settings::resolve("fake", Some("fake-model".into())).unwrap();
    settings.isolation = agent::Isolation::Auto;
    let session = Session::create(&dir, settings, vec![], observer.clone()).unwrap();
    (dir, observer, session)
}

/// The user's text of each message, steering shown as `steer: <text>`.
fn said(session: &mut Session) -> Vec<String> {
    session
        .history()
        .unwrap()
        .iter()
        .filter_map(|message| match message.content.as_slice() {
            [Block::Text { text }] => Some(match agent::steered_text(text) {
                Some(user) => format!("steer: {user}"),
                None => format!("{}: {text}", message.role),
            }),
            _ => None,
        })
        .collect()
}

#[test]
fn send_now_cuts_the_reply_being_written_and_the_model_reads_the_message_first() {
    let (dir, observer, mut session) = sends_now("send-now");
    session.prompt("hello").unwrap();
    *observer.handles.lock().unwrap() = Some((session.steering_handle(), session.hurry_handle()));
    let started = Instant::now();
    session.prompt("[slow:cut] tell a long story").unwrap();
    assert_eq!(
        *observer.events.lock().unwrap(),
        ["sent now", "cut", "steered"]
    );
    assert!(
        started.elapsed() < SLOW - Duration::from_secs(1),
        "the reply being written was cut, not waited out ({:?})",
        started.elapsed()
    );
    // The turn's first reply is cut: the message joins the prompt it followed, and the
    // turn is asked again with both; the cut reply is gone.
    assert_eq!(
        said(&mut session),
        [
            "user: hello",
            "assistant: ok",
            "user: [slow:cut] tell a long story\n\nalso count the files",
            "assistant: ok"
        ]
    );
    assert!(session.steering_handle().lock().unwrap().is_empty());
    assert!(
        !session.hurry_handle().load(Ordering::SeqCst),
        "cleared once read"
    );
    // The cut request may have been billed: counted as unknown, never as a reply.
    let report = agent::usage_report(&dir).unwrap();
    assert_eq!((report.total.calls, report.total.unknown), (2, 1));
    assert_eq!(session.status().unwrap().unresolved, 0);
}

#[test]
fn send_now_after_a_tool_round_keeps_its_work_and_the_model_reads_the_message_first() {
    let (dir, observer, mut session) = sends_now("send-now-tools");
    session.prompt("hello").unwrap();
    *observer.handles.lock().unwrap() = Some((session.steering_handle(), session.hurry_handle()));
    let started = Instant::now();
    session
        .prompt("[write] [slowtool:cut] write a.txt, then tell a long story")
        .unwrap();
    assert_eq!(
        *observer.events.lock().unwrap(),
        ["sent now", "cut", "steered"]
    );
    assert!(started.elapsed() < SLOW - Duration::from_secs(1));
    // The tool's result stays; the message follows it as a steering update.
    assert_eq!(
        said(&mut session),
        [
            "user: hello",
            "assistant: ok",
            "user: [write] [slowtool:cut] write a.txt, then tell a long story",
            "steer: also count the files",
            "assistant: ok"
        ]
    );
    assert_eq!(
        session.files().unwrap(),
        [("a.txt".to_string(), 2)],
        "the write before the cut is kept"
    );
    let report = agent::usage_report(&dir).unwrap();
    assert_eq!((report.total.calls, report.total.unknown), (3, 1));
    assert!(!session.hurry_handle().load(Ordering::SeqCst));
}

#[test]
fn send_now_waits_for_a_reply_it_cannot_cut_and_says_so() {
    // A session's first request has no earlier point to prepare it from again.
    let (_, observer, mut session) = sends_now("send-now-first");
    *observer.handles.lock().unwrap() = Some((session.steering_handle(), session.hurry_handle()));
    session.prompt("[slow:first] begin").unwrap();
    assert_eq!(
        *observer.events.lock().unwrap(),
        ["sent now", "not cut", "steered"]
    );
    assert_eq!(
        said(&mut session),
        [
            "user: [slow:first] begin",
            "assistant: Once upon a time, the end.",
            "steer: also count the files",
            "assistant: ok"
        ]
    );
    assert!(!session.hurry_handle().load(Ordering::SeqCst));
}

#[test]
fn a_dropped_request_is_sent_again_with_consent_and_counted_as_possibly_billed() {
    let (dir, _, mut session) = session("dropped-once");
    let mut settings = session.settings().unwrap();
    settings.resend_unknown = true;
    session.set_settings(settings).unwrap();
    session.prompt("hello").unwrap();
    // The connection drops once: the turn carries on, sending the request again.
    let before = sent("[hangup:auto]");
    session.prompt("[hangup:auto] go on").unwrap();
    assert_eq!(sent("[hangup:auto]"), before + 2);
    let report = agent::usage_report(&dir).unwrap();
    assert_eq!((report.total.calls, report.total.unknown), (2, 1));
}

#[test]
fn a_live_drop_without_consent_is_counted_once_and_requires_manual_retry() {
    let (dir, _, mut session) = session("drop-no-consent");
    session.prompt("hello").unwrap();
    let before = sent("[hangup:no-consent]");
    let error = session.prompt("[hangup:no-consent] go on").unwrap_err();
    assert!(error.contains("unknown outcome"), "{error}");
    assert_eq!(sent("[hangup:no-consent]"), before + 1);
    let report = agent::usage_report(&dir).unwrap();
    assert_eq!((report.total.calls, report.total.unknown), (1, 1));
    assert!(report.uncertain_usd > 0.0);
    drop(session);
    let mut session = Session::open(&dir, Arc::new(Seen::default())).unwrap();
    assert!(session.resume().unwrap_err().contains("unknown"));
    assert_eq!(sent("[hangup:no-consent]"), before + 1);
    session.retry().unwrap();
    assert_eq!(sent("[hangup:no-consent]"), before + 2);
}

#[test]
fn a_request_with_an_unknown_outcome_counts_at_its_worst_case_against_the_budget() {
    let (dir, _, mut session) = session("uncertain-budget");
    let mut settings = session.settings().unwrap();
    settings.resend_unknown = true;
    session.set_settings(settings).unwrap();
    session.prompt("hello").unwrap();
    // Cut off without a reply every time: after its automatic re-sends the turn stops,
    // each attempt counted as possibly billed.
    let error = session.prompt("[drops:budget] go on").unwrap_err();
    assert!(error.contains("after 3 automatic retries"), "{error}");
    heal();
    let report = agent::usage_report(&dir).unwrap();
    assert_eq!((report.total.calls, report.total.unknown), (1, 4));
    // Not in the totals; a budget counts it at its request's worst case.
    assert!(report.uncertain_usd > 0.0);
    close(
        report.budget_spent(),
        report.total.usd + report.uncertain_usd,
    );
    let before = sent("[drops:budget]");
    // `retry` restarts the turn; a zero budget holds the new request back and says
    // what it could cost.
    session.set_budget(Some(0.0)).unwrap();
    let error = session.retry().unwrap_err();
    let worst: f64 = error
        .rsplit("could cost up to $")
        .next()
        .and_then(|rest| rest.split(',').next())
        .and_then(|amount| amount.parse().ok())
        .unwrap_or_else(|| panic!("no worst case in {error}"));
    // A budget the re-sent request alone would fit, but not beside the lost one (the
    // same request, so about the same worst case): nothing is sent.
    let mut session = Session::open(&dir, Arc::new(Seen::default())).unwrap();
    session
        .set_budget(Some(report.total.usd + 1.5 * worst))
        .unwrap();
    let error = session.resume().unwrap_err();
    assert!(error.starts_with("spending limit"), "{error}");
    assert!(
        error.contains("requests whose outcome is unknown"),
        "{error}"
    );
    assert_eq!(sent("[drops:budget]"), before, "nothing was re-sent");
    session.set_budget(None).unwrap();
    session.resume().unwrap();
    assert_eq!(sent("[drops:budget]"), before + 1);
}

#[test]
fn a_reply_adopted_from_the_store_reports_no_new_cost() {
    let (dir, seen, mut session) = session("reused");
    // Held back by the budget: prepared, never sent.
    session.set_budget(Some(0.0)).unwrap();
    assert!(session.prompt("hello").is_err());
    session.set_budget(None).unwrap();
    drop(session);
    // A reply recorded before a crash, not yet adopted.
    let mut store = spindle_store::Store::open(&dir.join("store")).unwrap();
    let state = TurnContinuation::restore(&store.snapshot().unwrap().continuation).unwrap();
    let Some(Effect::Model { id, waiting }) = state.pending_effects().unwrap().into_iter().next()
    else {
        panic!("a pending model request");
    };
    let response = NativeResponse {
        blocks: vec![Block::Text {
            text: "recorded earlier".into(),
        }],
        stop_reason: Some("stop".into()),
        usage: Usage {
            input: 800,
            output: 100,
            cache_read: 200,
            ..Usage::default()
        },
        unknown_events: 0,
        unfinished_tool_calls: 0,
    };
    let recorded = waiting
        .record(
            waiting.request(),
            waiting.attempt(),
            &response.checkpoint().unwrap(),
        )
        .unwrap();
    let attempt = spindle_store::completion::Attempt {
        id: format!("a:{id}"),
        effect: id.clone(),
        owner_epoch: store.epoch(),
        implementation: "dext-provider-v1".into(),
        grant_reference: "dext-login".into(),
    };
    store.reserve_attempt(&attempt).unwrap();
    store
        .ingest_receipt(&attempt.id, &recorded.checkpoint())
        .unwrap();
    drop(store);
    let mut session = Session::open(&dir, seen.clone()).unwrap();
    session.resume().unwrap();
    assert_eq!(*seen.reused.lock().unwrap(), 1, "nothing was re-sent");
    assert!(
        seen.costs.lock().unwrap().is_empty(),
        "a reused reply is no new spend: {:?}",
        seen.costs.lock().unwrap()
    );
    // It is counted all the same, once.
    let report = agent::usage_report(&dir).unwrap();
    assert_eq!(report.total.calls, 1);
    close(report.total.usd, parent());
}

#[test]
fn a_first_request_retried_in_a_fresh_store_stays_on_record() {
    let (dir, _, mut session) = session("restart-first");
    assert!(session.prompt("[hangup:first] start here").is_err());
    let report = agent::usage_report(&dir).unwrap();
    assert_eq!((report.total.calls, report.total.unknown), (0, 1));
    // The first request has no earlier point: a fresh store restarts the prompt.
    session.retry().unwrap();
    assert!(dir.join("store.abandoned-1").is_dir());
    let report = agent::usage_report(&dir).unwrap();
    assert_eq!(
        (report.total.calls, report.total.unknown),
        (1, 1),
        "the lost request stays counted beside the retried one"
    );
    assert!(report.uncertain_usd > 0.0, "at its worst case");
    assert_eq!(report.turns.len(), 1);
    assert_eq!(
        (report.turns[0].usage.calls, report.turns[0].usage.unknown),
        (1, 1)
    );
}

#[test]
fn a_ledger_beside_another_store_is_recounted() {
    let (one, _, mut first) = session("ledger-one");
    first.prompt("first thread").unwrap();
    let (two, _, mut second) = session("ledger-two");
    second.prompt("[write] second thread").unwrap();
    drop((first, second));
    // Both threads' first replies have the same effect id (`start` is a fixed command).
    std::fs::copy(one.join("usage.json"), two.join("usage.json")).unwrap();
    let report = agent::usage_report(&two).unwrap();
    assert_eq!(report.turns.len(), 1);
    assert_eq!(report.turns[0].prompt, "[write] second thread");
    assert_eq!(report.total.calls, 2);
    close(report.total.usd, 2.0 * parent());
}

#[test]
fn a_compaction_made_for_a_new_prompt_is_that_turns_spend() {
    let (dir, seen, mut session) = session_with("idle-compaction", "small-model");
    session.prompt("[big] one").unwrap();
    session.prompt("two").unwrap();
    // The history is over the window's budget: the third prompt compacts it first.
    session.prompt("three").unwrap();
    let report = agent::usage_report(&dir).unwrap();
    assert_eq!(report.compaction.calls, 1);
    let turns: Vec<(&str, u64)> = report
        .turns
        .iter()
        .map(|t| (t.prompt.as_str(), t.compaction.calls))
        .collect();
    assert_eq!(turns, vec![("[big] one", 0), ("two", 0), ("three", 1)]);
    close(report.turns[2].usage.usd, cost(SUMMARY) + parent());
    // Its cost line counts it as the new turn's first spend, as the report does.
    let costs = seen.costs.lock().unwrap().clone();
    assert_eq!(costs.len(), 4, "{costs:?}");
    close(costs[2].0, cost(SUMMARY));
    close(costs[2].1, cost(SUMMARY));
    close(costs[3].1, cost(SUMMARY) + parent());
}

#[test]
fn a_summary_without_a_recorded_reply_counts_as_unknown_at_its_worst_case() {
    let (dir, _, mut session) = session("summary-unknown");
    session.prompt("hello").unwrap();
    let head = session.status().unwrap().revision;
    let turn = agent::usage_report(&dir).unwrap().turns[0].turn.clone();
    drop(session);
    // Two reservations without a reply: one whose compaction moved on (the reply was
    // never saved), one at the head (it may still be on its way).
    let mut store = spindle_store::Store::open(&dir.join("store")).unwrap();
    for (revision, worst) in [(1, 0.25), (head, 0.5)] {
        let stage = spindle_store::artifact::Stage {
            id: format!("summary-compact-r{revision}"),
            scope: "operator".into(),
            owner_epoch: store.epoch(),
        };
        store.begin_stage(&stage).unwrap();
        let reserved = json!({"kind": "reserved", "command": format!("compact-r{revision}"),
            "revision": revision, "turn": turn, "idle": false, "provider": "fake",
            "model": "fake-model", "contract": "openai-chat-completions", "source": "x",
            "worst": worst});
        store
            .put_object(&stage, &serde_json::to_vec(&reserved).unwrap())
            .unwrap();
    }
    drop(store);
    let report = agent::usage_report(&dir).unwrap();
    assert_eq!((report.compaction.calls, report.compaction.unknown), (0, 2));
    close(report.uncertain_usd, 0.75);
    let kept: Vec<(&str, agent::Billing)> = report
        .calls
        .iter()
        .filter(|call| call.key.starts_with("summary:"))
        .map(|call| (call.key.as_str(), call.billing))
        .collect();
    assert_eq!(
        kept,
        vec![("summary:compact-r1", agent::Billing::Unknown)],
        "final once its compaction moved on; the other is still live"
    );
    assert_eq!(report.turns[0].compaction.unknown, 2);
}
