//! `spindle api` end to end: one process drives a session over JSON-RPC on stdin and
//! stdout against a scripted loopback provider (no paid calls): a prompt whose model
//! step calls bash, its events, status/files/read/history, expected-head refusals, and
//! a turn interrupted while its command runs, with reads and steering answered
//! meanwhile. The client smoke tests (Python, Node) run here too when the
//! interpreter is installed. Unix only: the tests drive the strict sandbox and `serve`.
#![cfg(unix)]
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Requests the fake provider has held open (HANG prompts) and HANGONCE requests seen,
/// by their user text: tests run in parallel, so each counts only prompts of its own.
static HANGING: std::sync::Mutex<BTreeMap<String, usize>> = std::sync::Mutex::new(BTreeMap::new());
static HUNG_ONCE: std::sync::Mutex<BTreeMap<String, usize>> =
    std::sync::Mutex::new(BTreeMap::new());

/// Requests held open so far whose user text contains `key`.
fn hanging(key: &str) -> usize {
    let hanging = HANGING.lock().unwrap();
    hanging
        .iter()
        .filter(|(prompt, _)| prompt.contains(key))
        .map(|(_, count)| count)
        .sum()
}

/// Forgets the requests whose user text contains `key`, so its next HANGONCE hangs.
fn forget_hangs(key: &str) {
    HANGING
        .lock()
        .unwrap()
        .retain(|prompt, _| !prompt.contains(key));
    HUNG_ONCE
        .lock()
        .unwrap()
        .retain(|prompt, _| !prompt.contains(key));
}
/// Requests that offered the app's `lookup` tool (tests run in parallel: each finds
/// its own by its system prompt).
static OFFERED: std::sync::Mutex<Vec<Value>> = std::sync::Mutex::new(Vec::new());

/// A scripted OpenAI chat-completions server on an ephemeral loopback port. After a
/// tool result it answers with text; a prompt containing SLEEP gets `sleep 30`, one
/// containing HANG no answer at all, any other prompt a bash call that writes
/// hello.txt.
fn fake_provider() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || answer(stream));
        }
    });
    port
}

fn answer(mut stream: TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut length = 0;
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() {
        return;
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() || !first.starts_with("POST") {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let last = messages.last().cloned().unwrap_or(Value::Null);
    let prompt = messages
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].to_string())
        .collect::<Vec<_>>()
        .join(" ");
    // HANGONCE: the first such request is held open, any later one answered.
    if prompt.contains("HANGONCE") && {
        let mut seen = HUNG_ONCE.lock().unwrap();
        let count = seen.entry(prompt.clone()).or_default();
        *count += 1;
        *count > 1
    } {
        let reply = chunk_text(&request, "Answered this time.");
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
        return;
    }
    if prompt.contains("HANG") {
        // Held open until the client gives up on it.
        *HANGING.lock().unwrap().entry(prompt.clone()).or_default() += 1;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
        let _ = stream.read(&mut [0; 1]);
        return;
    }
    let chunk = |delta: Value, finish: Value| {
        json!({"id": "x", "object": "chat.completion.chunk", "model": request["model"],
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    };
    let call = |command: &str| {
        json!({"tool_calls": [{"index": 0, "id": format!("call_{}", messages.len()), "type": "function",
            "function": {"name": "bash", "arguments": json!({"command": command}).to_string()}}]})
    };
    if prompt.contains("CHILDWRITE") && last["role"] != "tool" {
        let call = json!({"tool_calls": [{"index": 0, "id": format!("call_{}", messages.len()),
            "type": "function", "function": {"name": "bash",
            "arguments": json!({"command": if prompt.contains("delayed") {"sleep 0.3; echo child > child.txt"} else {"echo child > child.txt"}}).to_string()}}]});
        let mut out = String::new();
        for chunk in [
            chunk(call, Value::Null),
            chunk(json!({}), json!("tool_calls")),
        ] {
            out.push_str(&format!("data: {chunk}\n\n"));
        }
        out.push_str("data: [DONE]\n\n");
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
            out.len()
        );
        return;
    }
    if last["role"] == "user" && last["content"].to_string().contains("[task ") {
        let reply = chunk_text(&request, "Background answer received.");
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
        return;
    }
    if prompt.contains("START_JOB") && last["role"] != "tool" {
        let data = json!({"tool_calls":[{"index":0,"id":"job-start","type":"function","function":{"name":"start_job","arguments":json!({"command":"echo job-started; sleep 30","timeout":60}).to_string()}}]});
        let out = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            chunk(data, Value::Null),
            chunk(json!({}), json!("tool_calls"))
        );
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
            out.len()
        );
        return;
    }
    let offers_task = request["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == "task"));
    if prompt.contains("PARALLEL_CHILD_") && last["role"] != "tool" {
        let name = if prompt.contains("_one") {
            "one"
        } else {
            "two"
        };
        let target = if prompt.contains("CONFLICT") {
            "shared.txt"
        } else if name == "one" {
            "one.txt"
        } else {
            "two.txt"
        };
        let script = format!(
            "test ! -e one.txt && test ! -e two.txt && test ! -e shared.txt || exit 17; sleep 0.2; echo {name} > {target}; chmod +x {target}; mkdir -p empty-{name}; ln -s {target} link-{name}"
        );
        let mut out = String::new();
        for data in [
            chunk(call(&script), Value::Null),
            chunk(json!({}), json!("tool_calls")),
        ] {
            out.push_str(&format!("data: {data}\n\n"));
        }
        out.push_str("data: [DONE]\n\n");
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
            out.len()
        );
        return;
    }
    if offers_task && last["role"] != "tool" && prompt.contains("PARALLEL_PARENT") {
        let conflict = if prompt.contains("CONFLICT") {
            " CONFLICT"
        } else {
            ""
        };
        let calls: Vec<_> = ["one", "two"].iter().enumerate().map(|(index, name)| json!({"index":index,"id":format!("parallel-{name}"),"type":"function","function":{"name":"task","arguments":json!({"prompt":format!("PARALLEL_CHILD_{name}{conflict}"),"background":prompt.contains("MIXED") && index==0}).to_string()}})).collect();
        let mut out = String::new();
        for data in [
            chunk(json!({"tool_calls":calls}), Value::Null),
            chunk(json!({}), json!("tool_calls")),
        ] {
            out.push_str(&format!("data: {data}\n\n"));
        }
        out.push_str("data: [DONE]\n\n");
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
            out.len()
        );
        return;
    }
    if offers_task && last["role"] != "tool" && prompt.contains("DELEGATE") {
        let child = if prompt.contains("slowly") {
            "HANGONCE then write"
        } else if prompt.contains("stuck") {
            "HANGONCE stuck then write"
        } else if prompt.contains("DEEP") {
            "DEEP DELEGATE again"
        } else if prompt.contains("nap") {
            "SLEEP please"
        } else if prompt.contains("delayed") {
            "CHILDWRITE delayed"
        } else {
            "CHILDWRITE it"
        };
        let call = json!({"tool_calls": [{"index": 0, "id": format!("call_{}", messages.len()),
            "type": "function", "function": {"name": "task",
            "arguments": json!({"prompt": child, "background": prompt.contains("BACKGROUND")}).to_string()}}]});
        let mut out = String::new();
        for chunk in [
            chunk(
                json!({"role": "assistant", "content": "Delegating."}),
                Value::Null,
            ),
            chunk(call, Value::Null),
            chunk(json!({}), json!("tool_calls")),
        ] {
            out.push_str(&format!("data: {chunk}\n\n"));
        }
        out.push_str("data: [DONE]\n\n");
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
            out.len()
        );
        return;
    }
    let offers_lookup = request["tools"].as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool["function"]["name"] == "lookup")
    });
    if offers_lookup {
        OFFERED.lock().unwrap().push(request.clone());
    }
    let chunks = if offers_lookup && last["role"] != "tool" && prompt.contains("SCRIPT") {
        let code = "print('script got:', tool('lookup', {'q': 'from code'}))\n";
        vec![
            chunk(
                json!({"role": "assistant", "content": "Scripting."}),
                Value::Null,
            ),
            chunk(
                json!({"tool_calls": [{"index": 0, "id": format!("call_{}", messages.len()),
                    "type": "function", "function": {"name": "run_code",
                    "arguments": json!({"language": "python", "code": code}).to_string()}}]}),
                Value::Null,
            ),
            chunk(json!({}), json!("tool_calls")),
        ]
    } else if offers_lookup && last["role"] != "tool" {
        vec![
            chunk(
                json!({"role": "assistant", "content": "Asking the app."}),
                Value::Null,
            ),
            chunk(
                json!({"tool_calls": [{"index": 0, "id": format!("call_{}", messages.len()),
                    "type": "function", "function": {"name": "lookup",
                    "arguments": json!({"q": "notes"}).to_string()}}]}),
                Value::Null,
            ),
            chunk(json!({}), json!("tool_calls")),
        ]
    } else if last["role"] == "tool" || request.get("tools").is_none() {
        vec![
            chunk(
                json!({"role": "assistant", "content": "All done."}),
                Value::Null,
            ),
            chunk(json!({}), json!("stop")),
        ]
    } else if prompt.contains("SLEEP") {
        vec![
            chunk(
                json!({"role": "assistant", "content": "Waiting."}),
                Value::Null,
            ),
            chunk(call("echo task-sleep-started; sleep 30"), Value::Null),
            chunk(json!({}), json!("tool_calls")),
        ]
    } else {
        vec![
            chunk(
                json!({"role": "assistant", "content": "Writing a file."}),
                Value::Null,
            ),
            chunk(call("echo hello > hello.txt && cat hello.txt"), Value::Null),
            chunk(json!({}), json!("tool_calls")),
        ]
    };
    let mut out = String::new();
    for chunk in chunks {
        out.push_str(&format!("data: {chunk}\n\n"));
    }
    out.push_str("data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n");
    out.push_str("data: [DONE]\n\n");
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
        out.len()
    );
}

/// A private scratch directory with an empty Dext home (built-in catalog, no logins).
fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("spindle-api-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("home/.dext")).unwrap();
    root
}

/// `spindle api` with a fake home and every provider request sent to `port`.
fn command(root: &Path, port: u16) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_spindle"));
    command
        .env("HOME", root.join("home"))
        .env("DEXT_HOME", root.join("home/.dext"))
        .env("DEXT_BASE_URL", format!("http://127.0.0.1:{port}"))
        .env_remove("OPENAI_BASE_URL")
        .env_remove("ANTHROPIC_BASE_URL");
    command
}

struct Api {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: mpsc::Receiver<Value>,
    next: u64,
    events: Vec<Value>,
    /// Replies that arrived while waiting for another one.
    early: Vec<Value>,
}

impl Api {
    fn start(root: &Path, port: u16) -> Self {
        let mut child = command(root, port)
            .arg("api")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, frames) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                // stdout carries protocol frames only: every line is one JSON object.
                let frame: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|_| panic!("non-protocol output on stdout: {line:?}"));
                assert_eq!(frame["jsonrpc"], "2.0", "{line}");
                if sender.send(frame).is_err() {
                    break;
                }
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            frames,
            next: 0,
            events: vec![],
            early: vec![],
        }
    }
    fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next += 1;
        let frame = json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params});
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{frame}").unwrap();
        stdin.flush().unwrap();
        self.next
    }
    /// The next frame, keeping event notifications.
    fn frame(&mut self) -> Value {
        let frame = self
            .frames
            .recv_timeout(Duration::from_secs(60))
            .expect("a frame within a minute");
        if frame["method"] == "event" {
            self.events.push(frame.clone());
        }
        frame
    }
    fn wait(&mut self, id: u64) -> Value {
        if let Some(at) = self.early.iter().position(|frame| frame["id"] == id) {
            return self.early.remove(at);
        }
        loop {
            let frame = self.frame();
            if frame["id"] == id {
                return frame;
            }
            if frame["method"] != "event" {
                self.early.push(frame);
            }
        }
    }
    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        self.wait(id)
    }
    fn ok(&mut self, method: &str, params: Value) -> Value {
        let reply = self.call(method, params);
        assert!(reply.get("error").is_none(), "{method}: {reply}");
        reply["result"].clone()
    }
    fn event_types(&self, request: u64) -> Vec<String> {
        self.events
            .iter()
            .filter(|frame| frame["params"]["request"] == request)
            .map(|frame| {
                frame["params"]["event"]["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }
}

#[test]
#[cfg(unix)]
fn a_session_runs_over_stdio_with_events_reads_and_interruption() {
    let port = fake_provider();
    let root = scratch("e2e");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    let info = api.ok("info", json!({}));
    assert_eq!(info["api_version"], 1);
    // Answered by a helper thread (readiness may run credential commands).
    assert!(api.ok("providers", json!({})).is_array());
    assert!(
        info["features"]
            .as_array()
            .unwrap()
            .contains(&json!("steering"))
    );

    let created = api.ok(
        "create",
        json!({"dir": dir, "provider": "local", "isolation": "off",
            "files": [{"path": "notes/seed.txt", "text": "seed\n"}]}),
    );
    let session = created["session"].as_str().unwrap().to_string();
    assert_eq!(created["head"], 0);
    assert_eq!(created["phase"], "empty");
    assert_eq!(created["settings"]["isolation"], "off");
    let again = api.call("create", json!({"dir": dir, "provider": "local"}));
    assert_eq!(again["error"]["data"]["code"], "conflict");

    // A turn: model step, bash call with live output, recorded result, final text.
    let prompt = api.send("prompt", json!({"dir": dir, "text": "write hello.txt"}));
    let done = api.wait(prompt);
    let result = &done["result"];
    assert_eq!(result["stopped"]["complete"], true, "{done}");
    assert_eq!(result["session"], session);
    let head = result["head"].as_u64().unwrap();
    assert!(head > 0);
    let types = api.event_types(prompt);
    for expected in [
        "prompt",
        "model_step",
        "model_delta",
        "tool_call",
        "output",
        "tool_result",
        "model_text",
        "stopped",
    ] {
        assert!(
            types.iter().any(|t| t == expected),
            "{expected} missing from {types:?}"
        );
    }
    assert!(
        api.events
            .iter()
            .all(|frame| frame["params"]["session"] == session)
    );
    let output = api
        .events
        .iter()
        .find(|frame| frame["params"]["event"]["type"] == "output")
        .unwrap();
    assert_eq!(
        output["params"]["event"]["data"], "aGVsbG8K",
        "base64 of hello\\n"
    );

    let status = api.ok("status", json!({"dir": dir}));
    assert_eq!(
        (status["busy"].clone(), status["phase"].clone()),
        (json!(false), json!("stopped"))
    );
    assert_eq!(status["stop"], "complete");
    assert_eq!(status["head"], head);
    assert_eq!(status["unresolved"], 0);
    assert_eq!(status["prompts"], 1);
    let read = api.ok("read", json!({"dir": dir, "path": "hello.txt"}));
    assert_eq!(
        (read["encoding"].clone(), read["content"].clone()),
        (json!("utf8"), json!("hello\n"))
    );
    let files = api.ok("files", json!({"dir": dir}));
    let paths: Vec<&str> = files["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["hello.txt", "notes/seed.txt"]);
    let history = api.ok("history", json!({"dir": dir}));
    assert!(
        history["messages"]
            .to_string()
            .contains(r#""type":"tool_use""#)
    );
    assert_eq!(api.ok("usage", json!({"dir": dir}))["calls"], 2);
    let missing = api.call("read", json!({"dir": dir, "path": "nope"}));
    assert_eq!(missing["error"]["data"]["code"], "not_found");
    // Revision 0 is the empty state before the first turn; an uncommitted one is not found.
    let empty = api.ok("history", json!({"dir": dir, "revision": 0}));
    assert_eq!(
        (empty["revision"].clone(), empty["messages"].clone()),
        (json!(0), json!([]))
    );
    assert_eq!(
        api.ok("files", json!({"dir": dir, "revision": 0}))["files"],
        json!([])
    );
    for method in ["history", "files"] {
        let unknown = api.call(method, json!({"dir": dir, "revision": head + 100}));
        assert_eq!(unknown["error"]["data"]["code"], "not_found", "{unknown}");
    }

    // A stale expected head is refused before anything runs.
    let stale = api.call(
        "prompt",
        json!({"dir": dir, "text": "x", "expected_head": head + 5}),
    );
    assert_eq!(stale["error"]["code"], -32003);
    assert_eq!(stale["error"]["data"]["head"], head);

    // A long command: reads, steering and a second prompt are answered while it runs,
    // then stop ends it long before its 30 s.
    let started = Instant::now();
    let sleeper = api.send(
        "prompt",
        json!({"dir": dir, "text": "SLEEP please", "expected_head": head}),
    );
    loop {
        let frame = api.frame();
        assert!(frame["id"] != sleeper, "the turn ended early: {frame}");
        if frame["params"]["event"]["type"] == "output_end"
            || frame["params"]["event"]["type"] == "tool_call"
        {
            break;
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    let busy = api.ok("status", json!({"dir": dir}));
    assert_eq!(
        (busy["busy"].clone(), busy["running"].clone()),
        (json!(true), json!("prompt"))
    );
    assert!(
        busy["unresolved"].is_null(),
        "store counts are only known when idle"
    );
    api.ok("history", json!({"dir": dir}));
    assert_eq!(
        api.ok("steer", json!({"dir": dir, "text": "also say hi"}))["queued"],
        1
    );
    let second = api.call("prompt", json!({"dir": dir, "text": "another"}));
    assert_eq!(second["error"]["data"]["code"], "busy");
    let settings = api.call("settings", json!({"dir": dir, "network": true}));
    assert_eq!(settings["error"]["data"]["code"], "busy");
    assert_eq!(api.ok("stop", json!({"dir": dir}))["stopping"], true);
    let stopped = api.wait(sleeper);
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "stop did not end the command"
    );
    // Stopped mid-command: an error that says so (or a cancelled stop), with the
    // steering message it never read handed back.
    let leftover = match stopped.get("error") {
        Some(error) => {
            assert_eq!(error["data"]["code"], "stopped", "{stopped}");
            error["data"]["unsent"].clone()
        }
        None => stopped["result"]["unsent"].clone(),
    };
    let steered = api.event_types(sleeper).iter().any(|t| t == "steered");
    assert!(steered || leftover == json!(["also say hi"]), "{stopped}");
    let after = api.ok("status", json!({"dir": dir}));
    assert_eq!(after["busy"], false);
    assert_eq!(
        api.call("stop", json!({"dir": dir}))["error"]["data"]["code"],
        "not_running"
    );
    // Continue records the interrupted command as not completed and finishes the turn.
    let resumed = api.ok(
        "continue",
        json!({"dir": dir, "expected_head": after["head"]}),
    );
    assert_eq!(resumed["stopped"]["complete"], true, "{resumed}");
    let after = api.ok("status", json!({"dir": dir}));
    assert_eq!(after["stop"], "complete");

    // Rewind back to the finished first turn, guarded by the current head.
    let targets = api.ok("targets", json!({"dir": dir}));
    let target = targets["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["revision"] == head)
        .unwrap_or_else(|| panic!("r{head} is a target: {targets}"))
        .clone();
    assert_eq!(target["kind"], "settled");
    let now = after["head"].as_u64().unwrap();
    let rewound = api.ok(
        "rewind",
        json!({"dir": dir, "revision": head, "expected_head": now}),
    );
    assert_eq!(rewound["rewound"]["target"], head);
    assert_eq!(
        api.ok("read", json!({"dir": dir, "path": "hello.txt"}))["content"],
        "hello\n"
    );

    // A close and calls sent right behind it are all answered.
    let close = api.send("close", json!({"dir": dir}));
    let behind = api.send("targets", json!({"dir": dir}));
    let closed = api.wait(close);
    assert_eq!(closed["result"]["closed"], true, "{closed}");
    let behind = api.wait(behind);
    assert!(
        behind.get("result").is_some() || behind["error"]["data"]["code"] == "busy",
        "{behind}"
    );
    // Reopened later, by a new worker.
    assert_eq!(api.ok("status", json!({"dir": dir}))["busy"], false);
    drop(api.stdin.take());
    let started = Instant::now();
    let status = loop {
        if let Some(status) = api.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "spindle api did not exit at end of input"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success());
    let _ = std::fs::remove_dir_all(root);
}

/// The session's first request has nothing to rewind to, so `retry` restarts it in a
/// fresh store. `stop` must reach that restarted turn too, not only the one it replaced.
#[test]
#[cfg(unix)]
fn stop_reaches_a_retried_first_request() {
    let port = fake_provider();
    let root = scratch("retry-first");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok(
        "create",
        json!({"dir": dir, "provider": "local", "isolation": "off"}),
    );
    // Stops a turn once the provider holds its request; its reply says it stopped.
    let stop_when_sent = |api: &mut Api, request: u64, sent: usize| {
        let started = Instant::now();
        while hanging("HANG on") < sent {
            assert!(started.elapsed() < Duration::from_secs(60), "never sent");
            std::thread::sleep(Duration::from_millis(20));
        }
        let started = Instant::now();
        assert_eq!(api.ok("stop", json!({"dir": dir}))["stopping"], true);
        let reply = api.wait(request);
        assert_eq!(reply["error"]["data"]["code"], "stopped", "{reply}");
        assert!(started.elapsed() < Duration::from_secs(20), "stop was late");
    };
    let first = api.send("prompt", json!({"dir": dir, "text": "HANG on"}));
    stop_when_sent(&mut api, first, 1);
    let retry = api.send("retry", json!({"dir": dir}));
    stop_when_sent(&mut api, retry, 2);
    assert!(
        api.event_types(retry).iter().any(|t| t == "retrying_first"),
        "{:?}",
        api.event_types(retry)
    );
    drop(api.stdin.take());
    let _ = api.child.wait();
    let _ = std::fs::remove_dir_all(root);
}

/// Runs a client's smoke test against this build when its interpreter exists.
fn client_smoke(interpreter: &str, script: &str) {
    if Command::new(interpreter).arg("--version").output().is_err() {
        eprintln!("skipped: {interpreter} is not installed");
        return;
    }
    let root = scratch(&format!("client-{interpreter}"));
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(script);
    // The script starts its own fake provider and points spindle at it.
    let output = Command::new(interpreter)
        .arg(&script)
        .env("HOME", root.join("home"))
        .env("DEXT_HOME", root.join("home/.dext"))
        .env("SPINDLE_BIN", env!("CARGO_BIN_EXE_spindle"))
        .env("SPINDLE_SMOKE_DIR", root.join("work"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{script:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn python_client_smoke() {
    client_smoke("python3", "clients/python/smoke_test.py");
}

#[test]
#[cfg(unix)]
fn node_client_smoke() {
    client_smoke("node", "clients/js/smoke_test.mjs");
}

/// The app's tool in these tests: a lookup that may write notes.
fn lookup_tool(timeout: u64) -> Value {
    json!([{"name": "lookup", "description": "Looks notes up.",
        "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}},
        "timeout_s": timeout}])
}

impl Api {
    /// The next `app_tool` event (a call the session waits on the app to answer).
    fn app_call(&mut self) -> Value {
        loop {
            let frame = self.frame();
            if frame["method"] == "event" && frame["params"]["event"]["type"] == "app_tool" {
                return frame["params"]["event"].clone();
            }
            if frame["method"] != "event" {
                self.early.push(frame);
            }
        }
    }
}

#[test]
#[cfg(unix)]
fn app_tools_are_offered_answered_and_kept_with_their_files() {
    let port = fake_provider();
    let root = scratch("app");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    assert!(
        api.ok("info", json!({}))["features"]
            .as_array()
            .unwrap()
            .contains(&json!("app_tools"))
    );
    // Names of Spindle's own tools, and bad definitions, are refused at once.
    let refused = api.call(
        "create",
        json!({"dir": dir, "provider": "local",
        "tools": [{"name": "bash", "description": "x", "input_schema": {"type": "object"}}]}),
    );
    assert_eq!(refused["error"]["data"]["code"], "bad_request", "{refused}");
    api.ok(
        "create",
        json!({"dir": dir, "provider": "local", "tools": lookup_tool(60),
        "builtin_tools": "none", "instructions": "You help with notes."}),
    );
    let settings = api.ok("settings", json!({"dir": dir}));
    assert_eq!(settings["tools"][0]["name"], "lookup");
    assert_eq!(settings["builtin_tools"], "none");

    let prompt = api.send("prompt", json!({"dir": dir, "text": "find my notes"}));
    let call = api.app_call();
    assert_eq!(call["name"], "lookup");
    assert_eq!(call["input"], json!({"q": "notes"}));
    assert_eq!(call["again"], false);
    assert_eq!(call["rerun"], "no");
    let id = call["call"].as_str().unwrap().to_string();
    // No built-ins are offered under the app-only policy.
    let offered = OFFERED
        .lock()
        .unwrap()
        .iter()
        .find(|request| {
            request["messages"][0]["content"]
                .to_string()
                .contains("You help with notes.")
        })
        .cloned()
        .unwrap();
    let names: Vec<_> = offered["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["lookup"]);
    let system = offered["messages"][0]["content"].to_string();
    assert!(
        system.contains("You help with notes.") && system.contains("lookup"),
        "{system}"
    );
    assert!(!system.contains("bash"), "{system}");
    // An answer for another call is refused; this one is accepted once.
    let wrong = api.call(
        "app_tool_result",
        json!({"dir": dir, "call": "nope", "content": "x"}),
    );
    assert_eq!(wrong["error"]["data"]["code"], "conflict", "{wrong}");
    api.ok(
        "app_tool_result",
        json!({"dir": dir, "call": id, "content": "Found 1 note.", "details":{"private":"ui-only-marker"},
        "files": {"write": {"notes/today.md": "# Today\n"}}}),
    );
    let again = api.call(
        "app_tool_result",
        json!({"dir": dir, "call": id, "content": "x"}),
    );
    assert_eq!(again["error"]["data"]["code"], "conflict", "{again}");
    let done = api.wait(prompt);
    assert!(done.get("error").is_none(), "{done}");
    let types = api.event_types(prompt);
    assert!(types.contains(&"app_tool_done".to_string()), "{types:?}");
    let files = api.ok("files", json!({"dir": dir}));
    assert!(files.to_string().contains("notes/today.md"), "{files}");
    let history = api.ok("history", json!({"dir": dir}));
    assert!(history.to_string().contains("Found 1 note."), "{history}");
    assert!(api.events.iter().any(
        |frame| frame["params"]["event"]["type"] == "app_tool_details"
            && frame["params"]["event"]["details"]["private"] == "ui-only-marker"
    ));
    assert!(
        !OFFERED
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.to_string().contains("ui-only-marker"))
    );
}

#[test]
#[cfg(unix)]
fn files_an_app_tool_writes_are_undone_by_rewinding_its_step() {
    let port = fake_provider();
    let root = scratch("app-rewind");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok(
        "create",
        json!({"dir": dir, "provider": "local", "tools": lookup_tool(60)}),
    );
    let first = api.send("prompt", json!({"dir": dir, "text": "find my notes"}));
    let call = api.app_call();
    api.ok(
        "app_tool_result",
        json!({"dir": dir, "call": call["call"], "content": "Nothing yet."}),
    );
    assert!(api.wait(first).get("error").is_none());
    let settled = api.ok("status", json!({"dir": dir}))["head"]
        .as_u64()
        .unwrap();
    let second = api.send("prompt", json!({"dir": dir, "text": "save a note"}));
    let call = api.app_call();
    api.ok(
        "app_tool_result",
        json!({"dir": dir, "call": call["call"], "content": "Saved.", "details":{"private":"rewound-note"},
        "files": {"write": {"notes/today.md": "# Today\n"}}}),
    );
    assert!(api.wait(second).get("error").is_none());
    assert!(
        api.ok("files", json!({"dir": dir}))
            .to_string()
            .contains("notes/today.md")
    );
    let answered_head = api.ok("status", json!({"dir":dir}))["head"]
        .as_u64()
        .unwrap();
    assert!(
        api.ok("history", json!({"dir":dir}))["app_details"]
            .to_string()
            .contains("rewound-note")
    );
    let rewind = api.send("rewind", json!({"dir": dir, "revision": settled}));
    assert!(api.wait(rewind).get("error").is_none());
    let files = api.ok("files", json!({"dir": dir}));
    assert!(!files.to_string().contains("notes/today.md"), "{files}");
    // The app is told which of its calls the rewind took back (only the second turn's).
    let taken: Vec<Value> = api
        .events
        .iter()
        .filter(|frame| {
            frame["params"]["request"] == rewind
                && frame["params"]["event"]["type"] == "app_calls_rewound"
        })
        .map(|frame| frame["params"]["event"]["calls"].clone())
        .collect();
    assert_eq!(taken, [json!([call["call"]])]);
    assert!(
        !api.ok("history", json!({"dir":dir}))["app_details"]
            .to_string()
            .contains("rewound-note")
    );
    assert!(
        api.ok("history", json!({"dir":dir,"revision":answered_head}))["app_details"]
            .to_string()
            .contains("rewound-note")
    );
}

#[test]
#[cfg(unix)]
fn an_app_call_cut_off_by_a_crash_or_stop_is_asked_again_never_dropped() {
    let port = fake_provider();
    let root = scratch("app-again");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok(
        "create",
        json!({"dir": dir, "provider": "local", "tools": lookup_tool(60)}),
    );
    api.send("prompt", json!({"dir": dir, "text": "find my notes"}));
    let first = api.app_call();
    // The process dies while the app is deciding: nothing is recorded for the call.
    api.child.kill().unwrap();
    api.child.wait().unwrap();

    let mut api = Api::start(&root, port);
    let resumed = api.send("continue", json!({"dir": dir}));
    let second = api.app_call();
    assert_eq!(second["call"], first["call"], "the same call keeps its id");
    assert_eq!(second["again"], true);
    // A stop while waiting records nothing either; the next continue asks once more.
    api.ok("stop", json!({"dir": dir}));
    let stopped = api.wait(resumed);
    assert!(stopped.get("error").is_some(), "{stopped}");
    let resumed = api.send("continue", json!({"dir": dir}));
    let third = api.app_call();
    assert_eq!(third["call"], first["call"]);
    assert_eq!(third["again"], true);
    api.ok(
        "app_tool_result",
        json!({"dir": dir, "call": third["call"], "unknown": true}),
    );
    let done = api.wait(resumed);
    assert!(done.get("error").is_none(), "{done}");
    let history = api.ok("history", json!({"dir": dir}));
    assert!(
        history
            .to_string()
            .contains("could not confirm whether this call ran"),
        "{history}"
    );
}

#[test]
#[cfg(unix)]
fn an_app_call_nobody_answers_is_recorded_as_unanswered() {
    let port = fake_provider();
    let root = scratch("app-timeout");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok(
        "create",
        json!({"dir": dir, "provider": "local", "tools": lookup_tool(1)}),
    );
    let prompt = api.send("prompt", json!({"dir": dir, "text": "find my notes"}));
    api.app_call();
    let done = api.wait(prompt);
    assert!(done.get("error").is_none(), "{done}");
    let history = api.ok("history", json!({"dir": dir}));
    assert!(
        history
            .to_string()
            .contains("No answer from the app within 1 s"),
        "{history}"
    );
}

#[test]
#[cfg(unix)]
fn a_script_calls_an_app_tool_as_a_journaled_child() {
    let port = fake_provider();
    let root = scratch("app-code");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok(
        "create",
        json!({"dir": dir, "provider": "local", "tools": lookup_tool(60)}),
    );
    let prompt = api.send("prompt", json!({"dir": dir, "text": "SCRIPT it"}));
    let call = api.app_call();
    assert_eq!(call["name"], "lookup");
    assert_eq!(call["input"], json!({"q": "from code"}));
    // A script's call is named by its journal stage, not by a model tool call.
    assert!(
        call["call"].as_str().unwrap().starts_with("code-"),
        "{call}"
    );
    api.ok(
        "app_tool_result",
        json!({"dir": dir, "call": call["call"], "content": "Found it.", "details":{"private":"code-ui-only-marker"}}),
    );
    let done = api.wait(prompt);
    assert!(done.get("error").is_none(), "{done}");
    let history = api.ok("history", json!({"dir": dir}));
    assert!(
        history.to_string().contains("script got:") && history.to_string().contains("Found it."),
        "{history}"
    );
    assert!(
        !history["messages"]
            .to_string()
            .contains("code-ui-only-marker")
    );
    assert!(api.events.iter().any(
        |frame| frame["params"]["event"]["type"] == "app_tool_details"
            && frame["params"]["event"]["details"]["private"] == "code-ui-only-marker"
    ));
    api.ok("close", json!({"dir":dir}));
    let mut store = spindle_store::Store::open(&dir.join("store")).unwrap();
    assert!(
        store
            .code_receipts("operator")
            .unwrap()
            .iter()
            .any(|record| record["private"]["details"]["private"] == "code-ui-only-marker")
    );
}

#[test]
#[cfg(unix)]
fn app_state_is_committed_with_the_conversation_and_rewound_with_it() {
    let port = fake_provider();
    let root = scratch("app-state");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    assert!(
        api.ok("info", json!({}))["features"]
            .as_array()
            .unwrap()
            .contains(&json!("state"))
    );
    api.ok(
        "create",
        json!({"dir": dir, "provider": "local", "tools": lookup_tool(60)}),
    );
    // Before any turn there is no step to keep state with: it goes with the first prompt.
    let early = api.call("state_set", json!({"dir": dir, "patch": {"a": 1}}));
    assert_eq!(early["error"]["data"]["code"], "conflict", "{early}");
    let first = api.send(
        "prompt",
        json!({"dir": dir, "text": "find my notes",
        "state": {"draft": "hello", "count": 1}}),
    );
    let call = api.app_call();
    // The prompt's state is already committed with its first step.
    let started = api.ok("state", json!({"dir": dir}));
    assert_eq!(
        started["state"],
        json!({"draft": "hello", "count": 1}),
        "{started}"
    );
    // While the turn runs a change waits for the next step boundary.
    let queued = api.ok("state_set", json!({"dir": dir, "patch": {"queued": true}}));
    assert_eq!(queued["queued"], 1);
    // The answer's state change is committed by the same step as its result.
    api.ok(
        "app_tool_result",
        json!({"dir": dir, "call": call["call"], "content": "Found.",
        "state": {"count": 2, "draft": null}}),
    );
    assert!(api.wait(first).get("error").is_none());
    let changes: Vec<Value> = api
        .events
        .iter()
        .filter(|frame| {
            frame["params"]["request"] == first && frame["params"]["event"]["type"] == "state"
        })
        .map(|frame| frame["params"]["event"].clone())
        .collect();
    assert_eq!(changes.len(), 3, "{changes:?}");
    let answered = changes[1]["revision"].as_u64().unwrap();
    let at = api.ok("state", json!({"dir": dir, "revision": answered}));
    assert_eq!(at["state"], json!({"count": 2}), "null removed draft: {at}");
    let history = api.ok("history", json!({"dir": dir, "revision": answered}));
    assert!(
        history.to_string().contains("Found."),
        "the result is in the same revision"
    );
    let before = api.ok("history", json!({"dir": dir, "revision": answered - 1}));
    assert!(!before.to_string().contains("Found."));
    let settled = api.ok("status", json!({"dir": dir}))["head"]
        .as_u64()
        .unwrap();
    assert_eq!(
        api.ok("state", json!({"dir": dir}))["state"],
        json!({"count": 2, "queued": true})
    );

    // Between turns a change is a step of its own; rewinding takes it back.
    let set = api.ok(
        "state_set",
        json!({"dir": dir, "patch": {"count": 3}, "expected_head": settled}),
    );
    assert!(set["revision"].as_u64().unwrap() > settled);
    assert_eq!(api.ok("state", json!({"dir": dir}))["state"]["count"], 3);
    let moved = api.call(
        "state_set",
        json!({"dir": dir, "patch": {"count": 4}, "expected_head": settled}),
    );
    assert_eq!(moved["error"]["data"]["code"], "head_moved", "{moved}");
    api.ok("rewind", json!({"dir": dir, "revision": settled}));
    assert_eq!(api.ok("state", json!({"dir": dir}))["state"]["count"], 2);
}

/// A whole streamed text answer.
fn chunk_text(request: &Value, text: &str) -> String {
    let chunk = |delta: Value, finish: Value| {
        json!({"id": "x", "object": "chat.completion.chunk", "model": request["model"],
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    };
    let mut out = format!(
        "data: {}\n\n",
        chunk(json!({"role": "assistant", "content": text}), Value::Null)
    );
    out.push_str(&format!("data: {}\n\n", chunk(json!({}), json!("stop"))));
    out.push_str("data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n");
    out.push_str("data: [DONE]\n\n");
    out
}

#[test]
#[cfg(unix)]
fn recovery_says_what_a_killed_owner_left_and_what_is_safe_to_resume() {
    let port = fake_provider();
    let root = scratch("recover");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    assert!(
        api.ok("info", json!({}))["features"]
            .as_array()
            .unwrap()
            .contains(&json!("recover"))
    );
    api.ok(
        "create",
        json!({"dir": dir, "provider": "local", "tools": lookup_tool(60)}),
    );
    let idle = api.ok("recover", json!({"dir": dir}));
    assert_eq!(idle["wake"]["work"], "none", "{idle}");

    // Killed while the app decides: known work only, the app is asked again.
    api.send("prompt", json!({"dir": dir, "text": "find my notes"}));
    let call = api.app_call();
    api.child.kill().unwrap();
    api.child.wait().unwrap();
    let mut api = Api::start(&root, port);
    let report = api.ok("recover", json!({"dir": dir}));
    assert_eq!(report["wake"]["work"], "ready", "{report}");
    assert_eq!(report["items"][0]["kind"], "app_tool");
    assert_eq!(report["items"][0]["id"], call["call"]);
    assert_eq!(report["items"][0]["next"], "answer");
    assert_eq!(
        api.ok("status", json!({"dir": dir}))["wake"]["work"],
        "ready"
    );

    // Owners that keep dying at the same step are a crash loop: hosts stop resuming it.
    for _ in 0..5 {
        api.send("continue", json!({"dir": dir}));
        api.app_call();
        api.child.kill().unwrap();
        api.child.wait().unwrap();
        api = Api::start(&root, port);
    }
    let looping = api.ok("recover", json!({"dir": dir}));
    assert!(looping["crashes"].as_u64().unwrap() >= 5, "{looping}");
    assert_eq!(looping["wake"]["work"], "blocked", "{looping}");
    // A person's continue still runs it, and progress clears the count.
    let resumed = api.send("continue", json!({"dir": dir}));
    let call = api.app_call();
    api.ok(
        "app_tool_result",
        json!({"dir": dir, "call": call["call"], "content": "Found."}),
    );
    assert!(api.wait(resumed).get("error").is_none());
    assert_eq!(
        api.ok("recover", json!({"dir": dir}))["wake"]["work"],
        "none"
    );
}

#[test]
#[cfg(unix)]
fn a_request_a_crash_left_unknown_waits_for_a_person_unless_the_session_resends() {
    let port = fake_provider();
    let root = scratch("recover-model");
    for (name, resend) in [("ask", false), ("resend", true)] {
        let dir = root.join(name);
        let mut api = Api::start(&root, port);
        api.ok(
            "create",
            json!({"dir": dir, "provider": "local", "resend_unknown": resend}),
        );
        // A first turn, so the second's request has a point to be sent again from.
        api.ok("prompt", json!({"dir": dir, "text": "write hello"}));
        forget_hangs("HANGONCE please");
        api.send("prompt", json!({"dir": dir, "text": "HANGONCE please"}));
        let started = Instant::now();
        while hanging("HANGONCE please") == 0 && started.elapsed() < Duration::from_secs(30) {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(200));
        api.child.kill().unwrap();
        api.child.wait().unwrap();
        let mut api = Api::start(&root, port);
        let report = api.ok("recover", json!({"dir": dir}));
        assert_eq!(report["items"][0]["kind"], "model", "{report}");
        assert_eq!(report["items"][0]["outcome"], "unknown", "{report}");
        // A person decides, unless the session resends such requests by itself.
        let wake = if resend { "ready" } else { "blocked" };
        assert_eq!(report["wake"]["work"], wake, "{report}");
        let until = api.call("continue", json!({"dir":dir,"until":"unknown"}));
        assert!(
            until["error"]["message"]
                .as_str()
                .unwrap()
                .contains("stopped at unknown outcome"),
            "{until}"
        );
        assert_eq!(
            api.ok("recover", json!({"dir":dir}))["head"],
            report["head"]
        );
        let resumed = api.call("continue", json!({"dir": dir}));
        if resend {
            // The session chose to send it again on its own (it may be billed twice).
            assert!(resumed.get("error").is_none(), "{resumed}");
            let history = api.ok("history", json!({"dir": dir}));
            assert!(
                history.to_string().contains("Answered this time."),
                "{history}"
            );
        } else {
            assert!(
                resumed["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("spindle retry"),
                "{resumed}"
            );
        }
        let _ = api.child.kill();
        let _ = api.child.wait();
    }
}

#[test]
#[cfg(unix)]
fn background_task_answer_is_durable_queued_once_and_survives_reopen() {
    let port = fake_provider();
    let root = scratch("queued-task");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok("create", json!({"dir":dir,"provider":"local"}));
    api.ok("prompt", json!({"dir":dir,"text":"first"}));
    let started = api.ok(
        "task_start",
        json!({"dir":dir,"task":"background","prompt":"CHILDWRITE it","report":"queue"}),
    );
    assert_eq!(started["queued"], true);
    let waited = api.ok(
        "task_wait",
        json!({"dir":dir,"task":"background","timeout_s":10}),
    );
    assert_eq!(waited["state"], "finished", "{waited}");
    assert!(
        waited["answer"]["content"]
            .as_str()
            .unwrap()
            .contains("All done.")
    );
    assert_eq!(
        api.ok("recover", json!({"dir":dir}))["wake"]["work"],
        "ready"
    );
    api.ok("close", json!({"dir":dir}));
    api.ok("continue", json!({"dir":dir}));
    let history = api.ok("history", json!({"dir":dir})).to_string();
    assert_eq!(
        history.matches("task app:background: finished").count(),
        1,
        "{history}"
    );
    api.ok("continue", json!({"dir":dir}));
    assert_eq!(api.ok("history", json!({"dir":dir})).to_string(), history);
    assert!(
        !api.ok("files", json!({"dir":dir}))
            .to_string()
            .contains("child.txt")
    );
    let second = api.ok(
        "task_start",
        json!({"dir":dir,"task":"background","prompt":"CHILDWRITE it","report":"queue"}),
    );
    assert_eq!(second["task"], started["task"]);
}

#[test]
fn model_background_tasks_are_waited_reported_once_and_never_adopt_files() {
    let port = fake_provider();
    let root = scratch("model-background");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok("create", json!({"dir":dir,"provider":"local","tasks":true}));
    api.ok(
        "prompt",
        json!({"dir":dir,"text":"DELEGATE BACKGROUND delayed"}),
    );
    let runs = spindle_store::meta::task_runs(&dir).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].state, "finished");
    assert!(runs[0].delivered.is_some());
    let task = spindle_store::agent::tasks::task_dir(&dir, &runs[0].task);
    let task_id = task.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        api.ok("task_wait", json!({"dir":dir,"task":task_id,"timeout_s":0}))["state"],
        "finished"
    );
    assert_eq!(
        api.ok("task_stop", json!({"dir":dir,"task":task_id}))["stopping"],
        false
    );
    let history = api.ok("history", json!({"dir":dir})).to_string();
    assert_eq!(
        history
            .matches(&format!("task {}: finished", runs[0].task))
            .count(),
        1
    );
    api.ok("continue", json!({"dir":dir}));
    assert_eq!(api.ok("history", json!({"dir":dir})).to_string(), history);
    assert!(
        !api.ok("files", json!({"dir":dir}))
            .to_string()
            .contains("child.txt")
    );
}

#[test]
fn a_stopped_background_task_blocks_wake_and_explicit_restart_recovers_it() {
    let port = fake_provider();
    let root = scratch("background-stop");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok("create", json!({"dir":dir,"provider":"local"}));
    api.ok("prompt", json!({"dir":dir,"text":"first"}));
    let params = json!({"dir":dir,"task":"nap","prompt":"SLEEP please","report":"queue"});
    api.ok("task_start", params.clone());
    loop {
        let frame = api.frame();
        let event = &frame["params"]["event"];
        if event["type"] == "task" && event["event"]["type"] == "output" {
            break;
        }
    }
    api.ok("task_stop", json!({"dir":dir,"task":"nap"}));
    let waited = api.ok("task_wait", json!({"dir":dir,"task":"nap","timeout_s":10}));
    assert_eq!(waited["state"], "stopped", "{waited}");
    let report = api.ok("recover", json!({"dir":dir}));
    assert_eq!(report["wake"]["work"], "blocked", "{report}");
    let resumed = api.call("continue", json!({"dir":dir}));
    assert!(
        resumed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("stopped without an answer"),
        "{resumed}"
    );
    api.ok("task_start", params);
    assert_eq!(
        api.ok("task_wait", json!({"dir":dir,"task":"nap","timeout_s":10}))["state"],
        "finished"
    );
    api.ok("continue", json!({"dir":dir}));
    assert_eq!(
        api.ok("recover", json!({"dir":dir}))["wake"]["work"],
        "none"
    );
}

#[test]
#[cfg(unix)]
fn export_keeps_app_state_separate_from_a_workspace_state_file() {
    let port = fake_provider();
    let root = scratch("export-state");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok("create", json!({"dir":dir,"provider":"local","files":[{"path":"state.json","text":"workspace content"}]}));
    api.ok(
        "prompt",
        json!({"dir":dir,"text":"first","state":{"app":true}}),
    );
    let target = root.join("export");
    api.ok("export", json!({"dir":dir,"target":target}));
    assert_eq!(
        std::fs::read_to_string(target.join("workspace/state.json")).unwrap(),
        "workspace content"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(target.join("state.json")).unwrap())
            .unwrap(),
        json!({"app":true})
    );
}

#[test]
#[cfg(unix)]
fn parallel_tasks_share_the_base_merge_in_order_and_refuse_conflicts() {
    for conflict in [false, true] {
        let port = fake_provider();
        let root = scratch(if conflict {
            "parallel-conflict"
        } else {
            "parallel-merge"
        });
        let dir = root.join("work");
        let mut api = Api::start(&root, port);
        api.ok(
            "create",
            json!({"dir":dir,"provider":"local","tasks":{"max_depth":2,"max_running":2}}),
        );
        let result = api.call("prompt", json!({"dir":dir,"text":if conflict {"PARALLEL_PARENT CONFLICT"} else {"PARALLEL_PARENT"}}));
        assert!(result.get("error").is_none(), "{result}");
        let files = api.ok("files", json!({"dir":dir}));
        if conflict {
            let read = api.ok("read", json!({"dir":dir,"path":"shared.txt"}));
            assert_eq!(read["content"], "one\n");
            assert!(
                api.ok("history", json!({"dir":dir}))
                    .to_string()
                    .contains("task file conflict")
            );
            assert!(!files.to_string().contains("empty-two"));
        } else {
            assert!(
                files.to_string().contains("one.txt") && files.to_string().contains("two.txt"),
                "{files}"
            );
            assert!(files["links"].as_array().unwrap().len() == 2, "{files}");
            assert!(
                files["directories"].as_array().unwrap().len() == 2,
                "{files}"
            );
        }
    }
}

#[test]
fn recovery_reports_a_job_as_lost_after_its_owner_exits_without_restarting_it() {
    let port = fake_provider();
    let root = scratch("job-recovery");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok("create", json!({"dir":dir,"provider":"local"}));
    api.ok(
        "prompt",
        json!({"dir":dir,"text":"START_JOB recovery fixture"}),
    );
    let report = api.ok("recover", json!({"dir":dir}));
    assert!(
        report["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "job" && item["outcome"] == "known"),
        "{report}"
    );
    api.child.kill().unwrap();
    api.child.wait().unwrap();
    let mut api = Api::start(&root, port);
    let report = api.ok("recover", json!({"dir":dir}));
    assert!(
        report["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "job"
                && item["outcome"] == "lost"
                && item["next"] == "none"),
        "{report}"
    );
    assert_eq!(report["wake"]["work"], "none");
    api.ok("continue", json!({"dir":dir,"until":"unknown"}));
    assert_eq!(
        api.ok("recover", json!({"dir":dir}))["head"],
        report["head"]
    );
}

#[test]
fn mixed_background_and_foreground_tasks_share_base_and_keep_background_files_in_child() {
    let port = fake_provider();
    let root = scratch("mixed-tasks");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok(
        "create",
        json!({"dir":dir,"provider":"local","tasks":{"max_running":2}}),
    );
    api.ok("prompt", json!({"dir":dir,"text":"PARALLEL_PARENT MIXED"}));
    let files = api.ok("files", json!({"dir":dir}));
    assert!(!files.to_string().contains("one.txt"), "{files}");
    assert!(
        files["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "two.txt" && file["executable"] == true),
        "{files}"
    );
    let runs = spindle_store::meta::task_runs(&dir).unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(
        runs.iter()
            .filter(|run| run.queued && run.delivered.is_some())
            .count(),
        1
    );
    assert!(runs.iter().all(|run| run.state == "finished"));
}

#[test]
fn task_policy_refuses_models_and_rewind_abandons_queued_work() {
    let port = fake_provider();
    let root = scratch("task-policy");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok(
        "create",
        json!({"dir":dir,"provider":"local","tasks":{"max_running":1,"models":["allowed-only"]}}),
    );
    api.ok("prompt", json!({"dir":dir,"text":"first"}));
    let denied = api.call(
        "task_start",
        json!({"dir":dir,"task":"denied","prompt":"work"}),
    );
    assert!(
        denied["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not permitted"),
        "{denied}"
    );
    assert!(spindle_store::meta::task_runs(&dir).unwrap().is_empty());
    api.ok("settings", json!({"dir":dir,"tasks":{"max_running":1}}));
    let target = api.ok("status", json!({"dir":dir}))["head"]
        .as_u64()
        .unwrap();
    api.ok("prompt", json!({"dir":dir,"text":"second"}));
    api.ok(
        "task_start",
        json!({"dir":dir,"task":"queued","prompt":"CHILDWRITE it","report":"queue"}),
    );
    api.ok(
        "task_wait",
        json!({"dir":dir,"task":"queued","timeout_s":10}),
    );
    api.ok("rewind", json!({"dir":dir,"revision":target}));
    let runs = spindle_store::meta::task_runs(&dir).unwrap();
    assert_eq!(runs[0].state, "abandoned");
    assert!(runs[0].delivered.is_none());
    api.ok("continue", json!({"dir":dir}));
    assert!(
        !api.ok("history", json!({"dir":dir}))
            .to_string()
            .contains("task app:queued: finished")
    );
    assert_eq!(
        api.ok("recover", json!({"dir":dir}))["wake"]["work"],
        "none"
    );
}

#[test]
#[cfg(unix)]
fn a_task_works_on_a_copy_and_its_files_and_spend_come_back_with_its_answer() {
    let port = fake_provider();
    let root = scratch("tasks");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok(
        "create",
        json!({"dir": dir, "provider": "local", "tasks": true,
        "files": [{"path": "notes.md", "text": "# notes\n"}]}),
    );
    assert_eq!(
        api.ok("settings", json!({"dir": dir}))["tasks"]["max_running"],
        4
    );
    let settled = {
        api.ok("prompt", json!({"dir": dir, "text": "first"}));
        api.ok("status", json!({"dir": dir}))["head"]
            .as_u64()
            .unwrap()
    };
    let delegated = api.send("prompt", json!({"dir": dir, "text": "DELEGATE the work"}));
    let done = api.wait(delegated);
    assert!(done.get("error").is_none(), "{done}");
    // The task's file came back with its answer; the session's own files are kept.
    let files = api.ok("files", json!({"dir": dir})).to_string();
    assert!(
        files.contains("child.txt") && files.contains("notes.md"),
        "{files}"
    );
    let history = api.ok("history", json!({"dir": dir})).to_string();
    assert!(history.contains("1 file(s) written"), "{history}");
    let tasks: Vec<_> = std::fs::read_dir(dir.join("tasks"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(tasks.len(), 1, "one task session");
    // Its spend counts as the session's.
    let usage = api.ok("usage", json!({"dir": dir}));
    assert!(usage["tasks"]["calls"].as_u64().unwrap() >= 2, "{usage}");
    // Rewinding past the call undoes the task's files too.
    api.ok("rewind", json!({"dir": dir, "revision": settled}));
    let files = api.ok("files", json!({"dir": dir})).to_string();
    assert!(
        !files.contains("child.txt") && files.contains("notes.md"),
        "{files}"
    );
}

#[test]
#[cfg(unix)]
fn a_task_cut_off_by_a_crash_is_found_again_and_continued_not_restarted() {
    let port = fake_provider();
    let root = scratch("tasks-crash");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    // The task inherits resend_unknown, so its cut-off request is sent again by itself.
    api.ok(
        "create",
        json!({"dir": dir, "provider": "local", "tasks": true, "resend_unknown": true}),
    );
    api.ok("prompt", json!({"dir": dir, "text": "first"}));
    forget_hangs("HANGONCE then write");
    api.send("prompt", json!({"dir": dir, "text": "DELEGATE slowly"}));
    let started = Instant::now();
    while hanging("HANGONCE then write") == 0 && started.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(200));
    api.child.kill().unwrap();
    api.child.wait().unwrap();
    let report = Api::start(&root, port).ok("recover", json!({"dir": dir}));
    // A task's call is resumed in the task's own session (it resends by itself).
    assert_eq!(report["items"][0]["kind"], "task", "{report}");
    assert_eq!(report["items"][0]["name"], "task", "{report}");
    assert_eq!(report["wake"]["work"], "ready", "{report}");
    let mut api = Api::start(&root, port);
    let resumed = api.call("continue", json!({"dir": dir}));
    assert!(resumed.get("error").is_none(), "{resumed}");
    let tasks: Vec<_> = std::fs::read_dir(dir.join("tasks"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(tasks.len(), 1, "the same task, not a second one");
    let history = api.ok("history", json!({"dir": dir})).to_string();
    assert!(history.contains("Answered this time."), "{history}");
}

#[test]
#[cfg(unix)]
fn an_app_starts_a_task_reads_it_and_the_session_keeps_its_own_files() {
    let port = fake_provider();
    let root = scratch("tasks-app");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    assert!(
        api.ok("info", json!({}))["features"]
            .as_array()
            .unwrap()
            .contains(&json!("tasks"))
    );
    api.ok("create", json!({"dir": dir, "provider": "local"}));
    api.ok("prompt", json!({"dir": dir, "text": "first"}));
    let bad = api.call(
        "task_start",
        json!({"dir": dir, "task": "a b", "prompt": "x"}),
    );
    assert_eq!(bad["error"]["data"]["code"], "bad_request", "{bad}");
    let started = api.ok(
        "task_start",
        json!({"dir": dir, "task": "research", "prompt": "CHILDWRITE it"}),
    );
    assert_eq!(started["failed"], false, "{started}");
    let task = started["task"].as_str().unwrap().to_string();
    // The task is a session of its own: every method reads it by its directory.
    let child_files = api.ok("files", json!({"dir": task})).to_string();
    assert!(
        child_files.contains("child.txt") && child_files.contains("hello.txt"),
        "{child_files}"
    );
    let own = api.ok("files", json!({"dir": dir})).to_string();
    assert!(!own.contains("child.txt"), "{own}");
    let listed = api.ok("tasks", json!({"dir": dir}));
    assert_eq!(listed["tasks"].as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed["tasks"][0]["status"]["complete"], true, "{listed}");
    // The same id finds the same task again.
    let again = api.ok(
        "task_start",
        json!({"dir": dir, "task": "research", "prompt": "CHILDWRITE it"}),
    );
    assert_eq!(again["task"], json!(task));
    let record = Path::new(&task).join("task.json");
    let original = std::fs::read(&record).unwrap();
    for bytes in [None, Some(b"{".as_slice()), Some(b"null".as_slice())] {
        match bytes {
            Some(bytes) => std::fs::write(&record, bytes).unwrap(),
            None => std::fs::remove_file(&record).unwrap(),
        }
        let damaged = api.call(
            "task_start",
            json!({"dir":dir,"task":"research","prompt":"different"}),
        );
        assert!(damaged["error"].is_object(), "{damaged}");
        assert!(
            damaged.to_string().contains("cannot verify task identity"),
            "{damaged}"
        );
    }
    std::fs::write(&record, original).unwrap();
    let conflict = api.call(
        "task_start",
        json!({"dir":dir,"task":"research","prompt":"different"}),
    );
    assert!(conflict["error"].is_object(), "{conflict}");
    let again = api.ok(
        "task_start",
        json!({"dir":dir,"task":"research","prompt":"CHILDWRITE it"}),
    );
    assert_eq!(again["task"], json!(task));
}

#[test]
#[cfg(unix)]
fn tasks_nest_two_deep_and_spend_from_what_is_left_of_the_budget() {
    let port = fake_provider();
    let root = scratch("tasks-deep");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok(
        "create",
        json!({"dir": dir, "provider": "local", "tasks": true, "max_usd": 5.0}),
    );
    let delegated = api.send("prompt", json!({"dir": dir, "text": "DEEP DELEGATE"}));
    let done = api.wait(delegated);
    assert!(done.get("error").is_none(), "{done}");
    let only = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir.join("tasks"))
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default()
    };
    let children = only(&dir);
    assert_eq!(children.len(), 1, "{children:?}");
    let grandchildren = only(&children[0]);
    assert_eq!(grandchildren.len(), 1, "{grandchildren:?}");
    // The deepest task is not offered `task`, so it answers on its own.
    assert!(only(&grandchildren[0]).is_empty(), "a third level");
    let child = api_settings_max(&mut api, &children[0]).expect("the task has a budget");
    let grandchild = api_settings_max(&mut api, &grandchildren[0]).expect("so does its task");
    assert!(child <= 5.0 && grandchild <= child, "{child} {grandchild}");
}

fn api_settings_max(api: &mut Api, dir: &Path) -> Option<f64> {
    api.ok("settings", json!({"dir": dir}))["max_usd"].as_f64()
}

#[test]
#[cfg(unix)]
fn stopping_a_session_stops_its_task_and_continue_finds_it_again() {
    let port = fake_provider();
    let root = scratch("tasks-stop");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok(
        "create",
        json!({"dir": dir, "provider": "local", "tasks": true}),
    );
    api.ok("prompt", json!({"dir": dir, "text": "first"}));
    let started = Instant::now();
    let napper = api.send("prompt", json!({"dir": dir, "text": "DELEGATE nap"}));
    loop {
        let frame = api.frame();
        assert!(frame["id"] != napper, "the turn ended early: {frame}");
        let event = &frame["params"]["event"];
        if event["type"] == "nested"
            && event["label"]
                .as_str()
                .is_some_and(|label| label.ends_with(": bash"))
        {
            break;
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(api.ok("stop", json!({"dir": dir}))["stopping"], true);
    let stopped = api.wait(napper);
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "stop did not reach the task: {stopped}"
    );
    assert_eq!(api.ok("status", json!({"dir": dir}))["busy"], false);
    let resumed = api.call("continue", json!({"dir": dir}));
    assert!(resumed.get("error").is_none(), "{resumed}");
    let tasks: Vec<_> = std::fs::read_dir(dir.join("tasks"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(tasks.len(), 1, "the same task, not a second one");
    assert_eq!(api.ok("status", json!({"dir": dir}))["stop"], "complete");
}

#[test]
#[cfg(unix)]
fn an_app_started_task_runs_as_a_turn_that_stop_reaches() {
    let port = fake_provider();
    let root = scratch("tasks-app-stop");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok("create", json!({"dir": dir, "provider": "local"}));
    api.ok("prompt", json!({"dir": dir, "text": "first"}));
    let started = Instant::now();
    let napper = api.send(
        "task_start",
        json!({"dir": dir, "task": "nap", "prompt": "SLEEP please"}),
    );
    loop {
        let frame = api.frame();
        assert!(frame["id"] != napper, "the task ended early: {frame}");
        let event = &frame["params"]["event"];
        if frame["params"]["request"] == napper
            && event["type"] == "nested"
            && event["label"]
                .as_str()
                .is_some_and(|label| label.ends_with(": bash"))
        {
            break;
        }
    }
    let busy = api.ok("status", json!({"dir": dir}));
    assert_eq!(busy["running"], "task", "{busy}");
    let second = api.call("prompt", json!({"dir": dir, "text": "another"}));
    assert_eq!(second["error"]["data"]["code"], "busy", "{second}");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(api.ok("stop", json!({"dir": dir}))["stopping"], true);
    let stopped = api.wait(napper);
    assert_eq!(stopped["error"]["data"]["code"], "stopped", "{stopped}");
    assert!(started.elapsed() < Duration::from_secs(25), "stop was late");
    // The same id finds the task again and finishes it.
    let again = api.ok(
        "task_start",
        json!({"dir": dir, "task": "nap", "prompt": "SLEEP please"}),
    );
    assert_eq!(again["failed"], false, "{again}");
    assert_eq!(
        api.ok("tasks", json!({"dir": dir}))["tasks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
#[cfg(unix)]
fn a_retried_turn_keeps_the_app_state_its_prompt_set() {
    let port = fake_provider();
    let root = scratch("retry-state");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok("create", json!({"dir": dir, "provider": "local"}));
    api.ok("prompt", json!({"dir": dir, "text": "write hello"}));
    forget_hangs("HANGONCE keep");
    api.send(
        "prompt",
        json!({"dir": dir, "text": "HANGONCE keep the state", "state": {"step": "two"}}),
    );
    let started = Instant::now();
    while hanging("HANGONCE keep") == 0 && started.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(200));
    api.child.kill().unwrap();
    api.child.wait().unwrap();
    let mut api = Api::start(&root, port);
    assert_eq!(
        api.ok("state", json!({"dir": dir}))["state"],
        json!({"step": "two"})
    );
    let retried = api.call("retry", json!({"dir": dir}));
    assert!(retried.get("error").is_none(), "{retried}");
    // The restarted turn starts from before the prompt, with the state it brought.
    assert_eq!(
        api.ok("state", json!({"dir": dir}))["state"],
        json!({"step": "two"})
    );
    let history = api.ok("history", json!({"dir": dir})).to_string();
    assert!(history.contains("Answered this time."), "{history}");
}

#[test]
#[cfg(unix)]
fn an_app_answer_whose_state_cannot_be_kept_is_answered_without_it() {
    let port = fake_provider();
    let root = scratch("app-state-big");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok(
        "create",
        json!({"dir": dir, "provider": "local", "tools": lookup_tool(60)}),
    );
    let prompt = api.send("prompt", json!({"dir": dir, "text": "find my notes"}));
    let call = api.app_call();
    // Over the 1 MiB the state may hold: recorded without it, so the step can adopt.
    let big = "x".repeat(1024 * 1024 + 1);
    api.ok(
        "app_tool_result",
        json!({"dir": dir, "call": call["call"], "content": "Found.", "state": {"big": big}}),
    );
    let done = api.wait(prompt);
    assert!(done.get("error").is_none(), "{done}");
    let history = api.ok("history", json!({"dir": dir})).to_string();
    assert!(
        history.contains("state change was not kept"),
        "{}",
        &history[..history.len().min(400)]
    );
    assert_eq!(api.ok("state", json!({"dir": dir}))["state"], json!({}));
}

#[test]
#[cfg(unix)]
fn a_state_step_waits_for_a_turn_cut_off_in_the_middle_of_a_step() {
    let port = fake_provider();
    let root = scratch("state-mid-step");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok("create", json!({"dir": dir, "provider": "local"}));
    api.ok("prompt", json!({"dir": dir, "text": "first"}));
    let sleeper = api.send("prompt", json!({"dir": dir, "text": "SLEEP please"}));
    loop {
        let frame = api.frame();
        assert!(frame["id"] != sleeper, "the turn ended early: {frame}");
        if frame["params"]["event"]["type"] == "tool_call" {
            break;
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    api.ok("stop", json!({"dir": dir}));
    api.wait(sleeper);
    // Stopped in the middle of the command: no step of its own goes there.
    let refused = api.call("state_set", json!({"dir": dir, "patch": {"mode": "a"}}));
    assert_eq!(refused["error"]["data"]["code"], "conflict", "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("middle of a step"),
        "{refused}"
    );
    let resumed = api.call("continue", json!({"dir": dir}));
    assert!(resumed.get("error").is_none(), "{resumed}");
    let set = api.send("state_set", json!({"dir": dir, "patch": {"mode": "a"}}));
    let reply = api.wait(set);
    assert!(reply["result"]["revision"].is_u64(), "{reply}");
    // Its `state` event names the request that caused it.
    assert!(
        api.event_types(set).iter().any(|t| t == "state"),
        "{:?}",
        api.event_types(set)
    );
}

#[test]
#[cfg(unix)]
fn a_first_request_a_crash_left_unknown_waits_for_retry_even_when_the_session_resends() {
    let port = fake_provider();
    let root = scratch("resend-first");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok(
        "create",
        json!({"dir": dir, "provider": "local", "resend_unknown": true}),
    );
    forget_hangs("HANGONCE at first");
    api.send("prompt", json!({"dir": dir, "text": "HANGONCE at first"}));
    let started = Instant::now();
    while hanging("HANGONCE at first") == 0 && started.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(200));
    api.child.kill().unwrap();
    api.child.wait().unwrap();
    let mut api = Api::start(&root, port);
    // Nothing to rewind to: only `retry` (a fresh store) can send it again.
    let report = api.ok("recover", json!({"dir": dir}));
    assert_eq!(report["items"][0]["next"], "retry", "{report}");
    assert_eq!(report["wake"]["work"], "blocked", "{report}");
    let retried = api.call("retry", json!({"dir": dir}));
    assert!(retried.get("error").is_none(), "{retried}");
}

#[test]
#[cfg(unix)]
fn a_fresh_store_retry_keeps_authoritative_metadata_not_stale_sidecars() {
    let port = fake_provider();
    let root = scratch("retry-meta");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok(
        "create",
        json!({"dir": dir, "provider": "local", "isolation": "off"}),
    );
    forget_hangs("HANGONCE metadata");
    api.send("prompt", json!({"dir": dir, "text": "HANGONCE metadata"}));
    let started = Instant::now();
    while hanging("HANGONCE metadata") == 0 {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "request never sent"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    api.child.kill().unwrap();
    api.child.wait().unwrap();
    let mut store = spindle_store::Store::open(&dir.join("store")).unwrap();
    let mut settings: Value =
        serde_json::from_slice(&store.metadata("settings").unwrap().unwrap()).unwrap();
    settings["instructions"] = json!("metadata, not the sidecar");
    settings["network"] = json!(false);
    let key = format!(
        "local/{}",
        settings["model"].as_str().unwrap().to_ascii_lowercase()
    );
    store
        .set_metadata("settings", &serde_json::to_vec(&settings).unwrap())
        .unwrap();
    store.set_metadata("pending_settings", b"null").unwrap();
    store
        .set_metadata(
            "output_limits",
            &serde_json::to_vec(&json!({key.clone(): 2048})).unwrap(),
        )
        .unwrap();
    store
        .set_metadata(
            "usage",
            &serde_json::to_vec(&json!({"version":1,"summaries":{"calls":3}})).unwrap(),
        )
        .unwrap();
    drop(store);
    std::fs::write(dir.join("pending-settings.json"), br#"{"network":true}"#).unwrap();
    std::fs::write(
        dir.join("usage.json"),
        br#"{"version":1,"summaries":{"calls":0}}"#,
    )
    .unwrap();
    let mut api = Api::start(&root, port);
    api.ok("retry", json!({"dir": dir}));
    let settings = api.ok("settings", json!({"dir": dir}));
    assert_eq!(settings["instructions"], "metadata, not the sidecar");
    assert_eq!(settings["network"], false);
    assert_eq!(
        api.ok("usage", json!({"dir": dir}))["compaction"]["calls"],
        3
    );
    api.ok("close", json!({"dir": dir}));
    let store = spindle_store::Store::open(&dir.join("store")).unwrap();
    let limits: Value =
        serde_json::from_slice(&store.metadata("output_limits").unwrap().unwrap()).unwrap();
    assert_eq!(limits[key], 2048);
    drop(store);
    drop(api.stdin.take());
    api.child.wait().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn a_task_that_cannot_go_on_by_itself_leaves_its_call_unanswered_until_it_is_settled() {
    let port = fake_provider();
    let root = scratch("tasks-blocked");
    let dir = root.join("work");
    let mut api = Api::start(&root, port);
    api.ok(
        "create",
        json!({"dir": dir, "provider": "local", "tasks": true}),
    );
    api.ok("prompt", json!({"dir": dir, "text": "first"}));
    forget_hangs("HANGONCE stuck");
    api.send("prompt", json!({"dir": dir, "text": "DELEGATE stuck"}));
    let started = Instant::now();
    while hanging("HANGONCE stuck") == 0 && started.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(200));
    api.child.kill().unwrap();
    api.child.wait().unwrap();
    let mut api = Api::start(&root, port);
    // The task's request may have been billed: the session waits for a person.
    let report = api.ok("recover", json!({"dir": dir}));
    let task = report["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "task")
        .cloned()
        .unwrap_or_else(|| panic!("a task item: {report}"));
    assert_eq!(task["outcome"], "blocked", "{report}");
    assert_eq!(report["wake"]["work"], "blocked", "{report}");
    let child = task["task"].as_str().unwrap().to_string();
    // Continuing records nothing for the call: it says where to settle the task.
    let refused = api.call("continue", json!({"dir": dir}));
    let message = refused["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        message.contains("cannot go on by itself") && message.contains(&child),
        "{refused}"
    );
    // Settled there (and let go of), the session's continue finishes the task's call.
    let retried = api.call("retry", json!({"dir": child}));
    assert!(retried.get("error").is_none(), "{retried}");
    api.ok("close", json!({"dir": child}));
    let resumed = api.call("continue", json!({"dir": dir}));
    assert!(resumed.get("error").is_none(), "{resumed}");
    let history = api.ok("history", json!({"dir": dir})).to_string();
    assert!(history.contains("Answered this time."), "{history}");
}
