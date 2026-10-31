//! Local engine time of the embedded host built natively: the `small`, `long` and
//! `compaction` scenarios of embed/tests/bench.mjs (which runs the release wasm, and
//! `files` and `concurrent` besides), against a model that answers at once, with
//! sessions in directories (each store opened natively, so SQLite's own I/O counts). Not a CI test; embed/tests/bench.sh runs it:
//!
//!   cargo test --release -p spindle-store --features embedded --test bench -- --ignored --nocapture
//!
//! SPINDLE_BENCH_QUICK=1 runs the small sizes; SPINDLE_BENCH_OUT=FILE writes the report.
#![cfg(all(feature = "embedded", not(target_family = "wasm")))]
use serde_json::{Value, json};
use spindle_store::embedded::{Catalog, Embedded, Places, Storage, answer_of};
use spindle_store::{Error, Store};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

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
    fn set_aside(&self, _: Store) -> spindle_store::Result<(Store, String)> {
        Err(Error::Unsupported)
    }
    fn child(&self, key: &str) -> Box<dyn Storage> {
        Box::new(DirStorage(self.0.with_file_name(format!("task-{key}"))))
    }
    fn tasks(&self) -> Vec<String> {
        vec![]
    }
    fn import_store(&self, _: &[u8]) -> spindle_store::Result<()> {
        Err(Error::Unsupported)
    }
}

const SUMMARY_ASK: &str = "Summarize the session history below";

/// Text of `bytes` length, different for each `seed` (as the wasm bench makes it).
fn words(seed: u64, bytes: usize) -> String {
    let mut x = (seed.wrapping_mul(2_654_435_761)) as u32;
    let mut text = String::new();
    while text.len() < bytes {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        text.push_str(&format!("w{} ", radix36(x >> 8)));
    }
    text.truncate(bytes);
    text
}
fn radix36(mut n: u32) -> String {
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = vec![];
    loop {
        out.push(digits[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

fn sse(delta: Value, index: usize) -> String {
    let finish = if delta.get("tool_calls").is_some() {
        "tool_calls"
    } else {
        "stop"
    };
    let chunks = [
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":finish}]}),
        json!({"id":"c","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
    ];
    let _ = index;
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    body
}

/// What the model answers to a request body.
type Model = Box<dyn FnMut(&str) -> Value>;

struct Bench {
    host: Embedded,
    model: RefCell<Model>,
    requests: Cell<usize>,
    compacted: Cell<usize>,
    sent: Cell<u64>,
}

impl Bench {
    fn new(root: &std::path::Path, model: Model) -> Self {
        let catalog = json!({"version": 4, "active_provider": "fake", "providers": [{
            "id": "fake", "display_name": "Fake", "api_provider": "openai",
            "request_contract": "openai-chat-completions", "base_url": "https://fake.invalid/v1",
            "default_model": "fake-model", "models": ["fake-model", "small-model"],
            "requires_api_key": false,
            "model_specs": {"fake-model": {"context_window": 200000, "max_output_tokens": 4096},
                "small-model": {"context_window": 20000, "max_output_tokens": 4096}}
        }]});
        Self {
            host: Embedded::new(
                Box::new(DirPlaces(root.to_path_buf())),
                Catalog::from_json(&catalog).unwrap(),
            ),
            model: RefCell::new(model),
            requests: Cell::new(0),
            compacted: Cell::new(0),
            sent: Cell::new(0),
        }
    }

    fn call(&self, method: &str, params: Value) -> Value {
        let id = self.sent.get() + 1000;
        self.sent.set(self.sent.get() + 1);
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut replies = vec![];
        let mut pending: VecDeque<Value> = self.host.handle(line.to_string().as_bytes()).into();
        while let Some(frame) = pending.pop_front() {
            match frame["method"].as_str() {
                Some("event") => {
                    if frame["params"]["event"]["type"] == "compacted" {
                        self.compacted.set(self.compacted.get() + 1);
                    }
                }
                Some("work") => pending.extend(self.work(&frame["params"])),
                Some("work_cancel") => {}
                _ => replies.push(frame),
            }
        }
        let reply = replies
            .into_iter()
            .find(|reply| reply["id"] == id)
            .unwrap_or_else(|| panic!("no reply to {method}"));
        assert!(reply.get("error").is_none(), "{method}: {reply}");
        reply["result"].clone()
    }

    fn work(&self, params: &Value) -> Vec<Value> {
        let session = params["session"].as_str().unwrap();
        let id = params["id"].as_u64().unwrap();
        let work = &params["work"];
        let deliver = |answer: Value| {
            self.host
                .deliver(session, id, answer_of(&answer).unwrap())
                .unwrap()
        };
        match work["kind"].as_str().unwrap() {
            "authorize" => deliver(json!({"kind": "authorized"})),
            "wait" => deliver(json!({"kind": "waited"})),
            "fetch" => {
                let body =
                    spindle_store::protocol::base64_decode(work["body"].as_str().unwrap()).unwrap();
                let body = String::from_utf8(body).unwrap();
                self.requests.set(self.requests.get() + 1);
                let index = self.requests.get();
                let delta = if body.contains(SUMMARY_ASK) {
                    json!({"content": format!("SUMMARY {index}: {}", words(index as u64, 600))})
                } else {
                    (self.model.borrow_mut())(&body)
                };
                let wire = sse(delta, index);
                let mut frames = vec![];
                for part in wire.as_bytes().chunks(512) {
                    let (more, streaming) = self.host.chunk(session, id, part);
                    frames.extend(more);
                    if !streaming {
                        return frames;
                    }
                }
                frames.extend(deliver(json!({"kind": "response", "status": 200,
                    "content_type": "text/event-stream"})));
                frames
            }
            other => panic!("unexpected work {other}"),
        }
    }
}

/// Each turn writes a file, then answers (as the wasm bench's `filesThenAnswer`).
fn files_then_answer(size: usize) -> Model {
    let mut turn = 0u64;
    Box::new(move |body: &str| {
        let last = body.rfind("\"role\":").map_or("", |at| &body[at..]);
        if last.starts_with("\"role\":\"tool\"") {
            return json!({"content": words(turn * 7 + 1, 800)});
        }
        turn += 1;
        let arguments = json!({"path": format!("notes/n{turn}.md"), "content": words(turn, size)});
        json!({"tool_calls": [{"index": 0, "id": format!("call_{turn}"), "type": "function",
            "function": {"name": "write_file", "arguments": arguments.to_string()}}]})
    })
}

fn timed<T>(work: impl FnOnce() -> T) -> (f64, T) {
    let start = Instant::now();
    let value = work();
    (start.elapsed().as_secs_f64() * 1000.0, value)
}

fn stats(times: &[f64]) -> Value {
    if times.is_empty() {
        return Value::Null;
    }
    let mut sorted = times.to_vec();
    sorted.sort_by(f64::total_cmp);
    let at = |q: f64| sorted[((q * sorted.len() as f64) as usize).min(sorted.len() - 1)];
    let round = |ms: f64| (ms * 1000.0).round() / 1000.0;
    json!({"n": times.len(), "p50": round(at(0.5)), "p95": round(at(0.95)), "max": round(sorted[sorted.len() - 1])})
}

/// Peak resident memory since the last reset (Linux: VmHWM, reset through clear_refs).
fn peak_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}
fn reset_peak() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

#[test]
#[ignore = "a benchmark: run with --ignored --nocapture (embed/tests/bench.sh)"]
fn bench() {
    let quick = std::env::var_os("SPINDLE_BENCH_QUICK").is_some();
    let (small, long, reads, reopens, compacting, exports) = if quick {
        (5, 30, 10, 4, 12, 2)
    } else {
        (20, 150, 30, 10, 30, 3)
    };
    let root = std::env::temp_dir().join(format!("spindle-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut results = serde_json::Map::new();
    let mut record = |name: &str, mut figures: Value| {
        figures["peak_rss_kib"] = json!(peak_kib());
        println!("{name} {figures}");
        results.insert(name.into(), figures);
        reset_peak();
    };
    reset_peak();

    // small: one short answer in each of many fresh sessions.
    let bench = Bench::new(&root, Box::new(|_| json!({"content": "hello there"})));
    let (mut creates, mut prompts) = (vec![], vec![]);
    for i in 0..small {
        let dir = format!("small-{i}");
        creates.push(timed(|| bench.call("create", json!({"dir": dir, "provider": "fake"}))).0);
        prompts
            .push(timed(|| bench.call("prompt", json!({"dir": dir, "text": format!("hi {i}")}))).0);
        bench.call("close", json!({"dir": dir}));
    }
    record(
        "small",
        json!({"create": stats(&creates), "prompt": stats(&prompts)}),
    );

    // long: a file written and an answer each turn; then reads, reopens and exports.
    let bench = Bench::new(&root, files_then_answer(1536));
    bench.call("create", json!({"dir": "long", "provider": "fake"}));
    let turns: Vec<f64> = (0..long)
        .map(|i| {
            let text = format!("turn {i}: {}", words(1000 + i as u64, 200));
            timed(|| bench.call("prompt", json!({"dir": "long", "text": text}))).0
        })
        .collect();
    let total = bench.call("history", json!({"dir": "long"}))["total"]
        .as_u64()
        .unwrap();
    let from = total.saturating_sub(30);
    let (mut window, mut status, mut targets, mut full) = (vec![], vec![], vec![], vec![]);
    for _ in 0..reads {
        window.push(timed(|| bench.call("history", json!({"dir": "long", "from": from}))).0);
        status.push(timed(|| bench.call("status", json!({"dir": "long"}))).0);
        targets.push(timed(|| bench.call("targets", json!({"dir": "long"}))).0);
    }
    for _ in 0..5 {
        full.push(timed(|| bench.call("history", json!({"dir": "long"}))).0);
    }
    let reopen: Vec<f64> = (0..reopens)
        .map(|_| {
            bench.call("close", json!({"dir": "long"}));
            timed(|| bench.call("open", json!({"dir": "long"}))).0
        })
        .collect();
    let mut bundle = 0;
    let export: Vec<f64> = (0..exports)
        .map(|_| {
            let (ms, bytes) = timed(|| bench.host.export_session("long", true, None));
            bundle = bytes.expect("exported").len();
            ms
        })
        .collect();
    let first: Vec<f64> = turns.iter().take(20).copied().collect();
    let last: Vec<f64> = turns.iter().rev().take(20).copied().collect();
    record(
        "long",
        json!({"turns": long, "messages": total, "requests": bench.requests.get(),
            "compacted": bench.compacted.get(), "first_turns": stats(&first),
            "last_turns": stats(&last), "history_window": stats(&window),
            "history_full": stats(&full), "status": stats(&status), "targets": stats(&targets),
            "reopen": stats(&reopen), "export": stats(&export), "bundle_kib": bundle >> 10}),
    );

    // compaction: a small window, long answers.
    let turn = Cell::new(0u64);
    let bench = Bench::new(
        &root,
        Box::new(move |_| {
            turn.set(turn.get() + 1);
            json!({"content": words(turn.get(), 12_000)})
        }),
    );
    bench.call(
        "create",
        json!({"dir": "compaction", "provider": "fake", "model": "small-model"}),
    );
    let (mut plain, mut compacting_turns) = (vec![], vec![]);
    for i in 0..compacting {
        let before = bench.compacted.get();
        let (ms, _) = timed(|| {
            bench.call(
                "prompt",
                json!({"dir": "compaction", "text": format!("turn {i}")}),
            )
        });
        if bench.compacted.get() > before {
            compacting_turns.push(ms);
        } else {
            plain.push(ms);
        }
    }
    record(
        "compaction",
        json!({"turns": compacting, "compactions": compacting_turns.len(),
            "plain": stats(&plain), "compacting": stats(&compacting_turns)}),
    );

    let report = json!({"host": "native", "quick": quick, "results": results});
    if let Some(out) = std::env::var_os("SPINDLE_BENCH_OUT") {
        std::fs::write(out, format!("{report:#}\n")).unwrap();
    }
    let _ = std::fs::remove_dir_all(&root);
}
