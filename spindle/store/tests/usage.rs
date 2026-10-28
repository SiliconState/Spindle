//! Spend accounting end to end against a loopback fake provider (no paid calls): parent
//! replies, a model-written compaction summary and a nested code-mode `model()` call
//! are each counted once and priced when recorded; a catalog price change never
//! rewrites them; a budget holds a request back until it is raised, then Continue sends
//! it; an unpriced model and a script over its `limits.max_usd` are refused unsent.
#![cfg(all(feature = "agent", target_os = "linux"))]
use serde_json::{Value, json};
use spindle_store::agent::{self, Event, Observer, Session, Settings};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Per-MTok prices of the fake model: input, output, cache read, cache write.
const PRICES: [f64; 4] = [1.0, 2.0, 0.1, 1.25];
const SUMMARY_SYSTEM: &str = "You summarize the earlier part of a coding session";
const NESTED_SYSTEM: &str = "Answer the supplied task.";

/// Each request kind's reported usage: prompt tokens (cached included), cached, output.
const PARENT: (u64, u64, u64) = (1000, 200, 100);
const SUMMARY: (u64, u64, u64) = (5000, 0, 300);
const NESTED: (u64, u64, u64) = (50, 0, 10);

fn cost((prompt, cached, output): (u64, u64, u64), scale: f64) -> f64 {
    ((prompt - cached) as f64 * PRICES[0] + output as f64 * PRICES[1] + cached as f64 * PRICES[2])
        * scale
        / 1e6
}

fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-12, "{a} != {b}");
}

/// A loopback OpenAI-chat provider. Requests are told apart by their system text: a
/// compaction summary, a nested `model()` call, or a turn's own request (a prompt with
/// "big" gets a long answer, "use code" a run_code call, a tool result an answer).
struct Fake {
    base: String,
    seen: Arc<Mutex<Vec<&'static str>>>,
}

/// A "flaky" prompt's request is refused (HTTP 400) the first time, cut off without a
/// reply the second (an unknown outcome) and answered the third.
static FLAKY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Fake {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<&'static str>>> = Arc::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                let log = log.clone();
                std::thread::spawn(move || {
                    let Some(body) = read_request(&mut stream) else {
                        return;
                    };
                    let (kind, reply) = answer(&body);
                    log.lock().unwrap().push(kind);
                    if reply == "refuse" {
                        let error = r#"{"error":{"message":"fixture refusal","type":"invalid_request_error"}}"#;
                        let _ = write!(
                            stream,
                            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{error}",
                            error.len()
                        );
                        return;
                    }
                    if reply == "hang up" {
                        return;
                    }
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                });
            }
        });
        Self { base, seen }
    }
    fn count(&self, kind: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| **seen == kind)
            .count()
    }
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
        let end = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")?;
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

fn sse(delta: Value, finish: &str, (prompt, cached, output): (u64, u64, u64)) -> String {
    let chunks = [
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":finish}]}),
        json!({"id":"c","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":prompt,"completion_tokens":output,"total_tokens":prompt + output,"prompt_tokens_details":{"cached_tokens":cached}}}),
    ];
    let mut out: String = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect();
    out.push_str("data: [DONE]\n\n");
    out
}

fn answer(body: &Value) -> (&'static str, String) {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let system = messages
        .first()
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default();
    if system.starts_with(SUMMARY_SYSTEM) {
        let text = "The user asked for a long answer and got it.";
        return ("summary", sse(json!({"content": text}), "stop", SUMMARY));
    }
    if system.starts_with(NESTED_SYSTEM) {
        return ("nested", sse(json!({"content": "42"}), "stop", NESTED));
    }
    let last = messages.last().cloned().unwrap_or_default();
    let text = |m: &Value| -> String {
        match &m["content"] {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        }
    };
    let reply = if last["role"] == "tool" {
        json!({"content": format!("done: {}", text(&last).chars().take(200).collect::<String>())})
    } else if text(&last).contains("use code") {
        let code = "try:\n    print('nested says', model('what is six times seven?')['text'])\nexcept Exception as error:\n    print('nested refused:', error)\n";
        let arguments = json!({"language": "python", "code": code}).to_string();
        let call = json!({"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"run_code","arguments":arguments}}]});
        return ("parent", sse(call, "tool_calls", PARENT));
    } else if text(&last).contains("flaky") {
        match FLAKY.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => return ("parent", "refuse".into()),
            1 => return ("parent", "hang up".into()),
            _ => json!({"content": "steady now"}),
        }
    } else if text(&last).contains("big") {
        json!({"content": "lorem ipsum dolor sit amet ".repeat(4500)})
    } else {
        json!({"content": "ok"})
    };
    ("parent", sse(reply, "stop", PARENT))
}

fn catalog(home: &Path, base: &str, scale: f64) {
    let pricing = json!({"input_usd_per_mtok": PRICES[0] * scale, "output_usd_per_mtok": PRICES[1] * scale,
        "cache_read_usd_per_mtok": PRICES[2] * scale, "cache_create_usd_per_mtok": PRICES[3] * scale});
    let providers = json!({"version": 4, "active_provider": "fake", "providers": [{
        "id": "fake", "display_name": "Fake", "api_provider": "openai",
        "request_contract": "openai-chat-completions", "base_url": base,
        "default_model": "fake-model", "models": ["fake-model", "unpriced-model"],
        "requires_api_key": false,
        "model_specs": {
            "fake-model": {"context_window": 20000, "max_output_tokens": 4096, "pricing": pricing},
            "unpriced-model": {"context_window": 20000, "max_output_tokens": 4096}
        }
    }]});
    std::fs::write(
        home.join("providers.json"),
        serde_json::to_vec_pretty(&providers).unwrap(),
    )
    .unwrap();
}

fn policy(path: &Path, max_usd: Option<f64>) {
    let mut policy = json!({"version": 1, "models": [{"provider": "fake"}], "tools": []});
    if let Some(max) = max_usd {
        policy["limits"] = json!({"max_usd": max});
    }
    std::fs::write(path, serde_json::to_vec(&policy).unwrap()).unwrap();
}

#[derive(Default)]
struct Costs(Mutex<Vec<(f64, f64, f64, String)>>);
impl Observer for Costs {
    fn event(&self, event: Event<'_>) {
        if let Event::Cost {
            usd,
            turn,
            total,
            billing,
        } = event
        {
            self.0
                .lock()
                .unwrap()
                .push((usd, turn, total, billing.to_string()));
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

#[test]
fn spend_is_counted_once_priced_at_record_time_and_budgets_hold_requests_back() {
    let root: PathBuf = std::env::temp_dir().join(format!("spindle-usage-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("dext");
    private_dir(&home);
    let fake = Fake::start();
    catalog(&home, &fake.base, 1.0);
    let policy_path = root.join("code-policy.json");
    policy(&policy_path, None);
    // SAFETY: this binary's only test sets them before any thread reads them.
    unsafe {
        std::env::set_var("DEXT_HOME", &home);
        std::env::set_var("SPINDLE_CODE_POLICY", &policy_path);
    }
    let dir = root.join("session");
    let costs = Arc::new(Costs::default());
    let mut settings = Settings::resolve("fake", Some("fake-model".into())).unwrap();
    settings.isolation = agent::Isolation::Auto;
    let mut session = Session::create(&dir, settings, vec![], costs.clone()).unwrap();

    // Turn 1: one long answer, so the next prompt compacts the history first.
    session.prompt("give me a big answer").unwrap();
    // Turn 2: a model-written summary (before the prompt), a run_code call whose script
    // calls model() once, and the answer after the tool result.
    session.prompt("now use code").unwrap();
    assert_eq!(
        (
            fake.count("parent"),
            fake.count("summary"),
            fake.count("nested")
        ),
        (3, 1, 1)
    );
    let history = session.history().unwrap();
    assert!(
        history
            .iter()
            .any(|m| serde_json::to_string(m).unwrap().contains("nested says 42")),
        "the script's nested call answered"
    );

    let report = agent::usage_report(&dir).unwrap();
    let (parent, summary, nested) = (cost(PARENT, 1.0), cost(SUMMARY, 1.0), cost(NESTED, 1.0));
    assert_eq!(report.total.calls, 5);
    assert_eq!(report.total.priced, 5);
    assert_eq!(
        (
            report.parent.calls,
            report.compaction.calls,
            report.nested.calls
        ),
        (3, 1, 1)
    );
    close(report.parent.usd, 3.0 * parent);
    close(report.compaction.usd, summary);
    close(report.nested.usd, nested);
    close(report.total.usd, 3.0 * parent + summary + nested);
    assert_eq!(
        report.total.input,
        3 * (PARENT.0 - PARENT.1) + SUMMARY.0 + NESTED.0
    );
    assert_eq!(report.total.cache_read, 3 * PARENT.1);
    assert_eq!((report.total.unknown, report.total.refused), (0, 0));
    // Per turn: the compaction a prompt needed belongs to that prompt's turn.
    assert_eq!(report.turns.len(), 2);
    assert_eq!(report.turns[0].prompt, "give me a big answer");
    close(report.turns[0].usage.usd, parent);
    assert_eq!(report.turns[1].prompt, "now use code");
    close(report.turns[1].usage.usd, 2.0 * parent + summary + nested);
    assert_eq!(
        (
            report.turns[1].parent.calls,
            report.turns[1].compaction.calls,
            report.turns[1].nested.calls
        ),
        (2, 1, 1)
    );
    assert_eq!(report.models.len(), 1);
    assert_eq!(report.models[0].model, "fake-model");
    // Every call was dated as it was recorded.
    close(
        report.since(report.calls[0].at.unwrap() - 60).usd,
        report.total.usd,
    );
    // Live cost events: each reply, the summary and the script, with the thread total.
    let seen = costs.0.lock().unwrap().clone();
    assert_eq!(seen.len(), 5, "{seen:?}");
    close(seen.last().unwrap().2, report.total.usd);
    assert!(seen.iter().all(|(_, _, _, billing)| billing == "priced"));
    // Counting again finds nothing new (the script's journal and the summary are read
    // once, and reusing them never counts twice).
    close(
        agent::usage_report(&dir).unwrap().total.usd,
        report.total.usd,
    );
    // A malformed persisted price is recounted, never hidden behind the cursors.
    let path = dir.join("usage.json");
    let mut ledger: Value =
        serde_json::from_slice(&spindle_store::meta::read(&dir, "usage").unwrap().unwrap())
            .unwrap();
    ledger["calls"][0]["usd"] = Value::Null;
    spindle_store::meta::update(&dir, "usage", |_| Ok(serde_json::to_vec(&ledger).unwrap()))
        .unwrap();
    let recounted = agent::usage_report(&dir).unwrap();
    assert_eq!(recounted.total.calls, 5);
    close(recounted.total.usd, report.total.usd);

    // A catalog price change never rewrites what was recorded...
    catalog(&home, &fake.base, 10.0);
    let frozen = agent::usage_report(&dir).unwrap();
    close(frozen.total.usd, report.total.usd);
    assert_eq!(frozen.total.calls, 5);
    // The sidecar is no longer authority: deleting it keeps record-time prices.
    std::fs::remove_file(&path).unwrap();
    close(
        agent::usage_report(&dir).unwrap().total.usd,
        report.total.usd,
    );
    // An explicitly lost ledger is rebuilt from receipts at current prices, undated.
    spindle_store::meta::update(&dir, "usage", |_| Ok(b"null".to_vec())).unwrap();
    let rebuilt = agent::usage_report(&dir).unwrap();
    assert_eq!(
        rebuilt.total.calls, 5,
        "summary and nested calls are durable"
    );
    close(rebuilt.total.usd, 10.0 * report.total.usd);
    close(rebuilt.since(0).usd, 0.0);
    catalog(&home, &fake.base, 1.0);
    spindle_store::meta::update(&dir, "usage", |_| Ok(b"null".to_vec())).unwrap();
    let spent = agent::usage_report(&dir).unwrap().total.usd;
    close(spent, report.total.usd);

    // A budget too small for the next request's worst case holds it back, unsent.
    session.set_budget(Some(spent + 0.0001)).unwrap();
    let error = session.prompt("one more").unwrap_err();
    assert!(error.starts_with("spending limit"), "{error}");
    assert_eq!(fake.count("parent"), 3, "nothing was sent");
    let mut snapshot = json!({"revision": 0, "head": 0, "busy": false, "recorded": false,
        "rejected": null, "phase": "ModelWait", "messages": [], "forks": []});
    let head = session.status().unwrap().revision;
    snapshot["revision"] = json!(head);
    snapshot["head"] = json!(head);
    agent::annotate_snapshot(&dir, &mut snapshot);
    assert!(
        snapshot["budget"]["blocked"]
            .as_str()
            .is_some_and(|text| text.starts_with("spending limit")),
        "{}",
        snapshot["budget"]
    );
    close(snapshot["usage"]["usd"].as_f64().unwrap(), spent);
    // Raising it lets Continue send the same request.
    session.set_budget(Some(1.0)).unwrap();
    session.resume().unwrap();
    assert_eq!(fake.count("parent"), 4);
    let report = agent::usage_report(&dir).unwrap();
    assert_eq!(report.turns.len(), 3);
    close(report.total.usd, spent + parent);

    // With a budget, a model the catalog has no price for is refused before sending.
    let mut settings = session.settings().unwrap();
    settings.model = "unpriced-model".into();
    session.set_settings(settings).unwrap();
    let error = session.prompt("anything").unwrap_err();
    assert!(
        error.contains("no price for fake/unpriced-model"),
        "{error}"
    );
    assert_eq!(fake.count("parent"), 4);
    // Without a budget it is sent, and counted as billed but unpriced.
    session.set_budget(None).unwrap();
    session.resume().unwrap();
    assert_eq!(fake.count("parent"), 5);
    let report = agent::usage_report(&dir).unwrap();
    assert_eq!((report.total.calls, report.total.unpriced), (7, 1));
    close(report.total.usd, spent + parent);
    assert_eq!(report.models.len(), 2);
    let mut settings = session.settings().unwrap();
    settings.model = "fake-model".into();
    session.set_settings(settings).unwrap();
    session.set_budget(Some(1.0)).unwrap();

    // A script over the policy's limits.max_usd is refused before its nested call.
    policy(&policy_path, Some(0.000001));
    session.prompt("use code again").unwrap();
    assert_eq!(fake.count("nested"), 1, "the nested call was never sent");
    let history = serde_json::to_string(&session.history().unwrap()).unwrap();
    assert!(history.contains("nested model spend limit"), "{history}");
    let report = agent::usage_report(&dir).unwrap();
    assert_eq!(report.nested.calls, 1);
    let billed = report.total.calls;

    // A refusal is a known outcome (not billed); a request cut off has an unknown one
    // (it may have been billed). Both are counted beside the billed calls.
    session.set_budget(None).unwrap();
    // Consent covers automatic retries after a further uncertain send, not only
    // the explicit retry of the recorded refusal.
    let mut settings = session.settings().unwrap();
    settings.resend_unknown = true;
    session.set_settings(settings).unwrap();
    assert!(session.prompt("a flaky request").is_err());
    let report = agent::usage_report(&dir).unwrap();
    assert_eq!((report.total.refused, report.total.unknown), (1, 0));
    session.retry().unwrap();
    let mut session = Session::open(&dir, costs.clone()).unwrap();
    let report = agent::usage_report(&dir).unwrap();
    assert!(
        report.uncertain_usd > 0.0,
        "the unknown one counts at its worst case against a budget"
    );
    let report = agent::usage_report(&dir).unwrap();
    assert_eq!(report.total.calls, billed + 1);
    assert_eq!((report.total.refused, report.total.unknown), (1, 1));
    // Each retry restarted the turn; the attempts it left behind stay on their own
    // (abandoned) turns, and the phone shows them on the prompt that replaced them.
    let flaky: Vec<_> = report
        .turns
        .iter()
        .filter(|turn| turn.prompt == "a flaky request")
        .collect();
    assert_eq!(flaky.len(), 3);
    assert_eq!(
        flaky
            .iter()
            .map(|t| (t.abandoned, t.usage.calls, t.usage.refused, t.usage.unknown))
            .collect::<Vec<_>>(),
        vec![(true, 0, 1, 0), (true, 0, 0, 1), (false, 1, 0, 0)]
    );
    let head = session.status().unwrap().revision;
    let mut view = spindle_store::view::View::open(&dir.join("store")).unwrap();
    let forks: Vec<Value> = view
        .read(|view| view.forks())
        .unwrap()
        .into_iter()
        .map(|f| json!({"revision": f.revision, "target": f.target, "abandoned": f.abandoned}))
        .collect();
    let messages = json!(
        session
            .history()
            .unwrap()
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect::<Vec<_>>()
    );
    let mut snapshot = json!({"revision": head, "head": head, "phase": "Stopped(Complete)",
        "forks": forks, "messages": messages});
    agent::annotate_snapshot(&dir, &mut snapshot);
    let last = snapshot["turn_costs"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        (
            last["calls"].as_u64(),
            last["refused"].as_u64(),
            last["unknown"].as_u64()
        ),
        (Some(1), Some(1), Some(1)),
        "{last}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
