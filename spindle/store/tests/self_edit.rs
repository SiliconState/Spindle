//! Self-edit threads end to end through `spindle serve`, against a scripted loopback
//! provider (no paid calls). The ideasCon shape: a self-edit thread changes a file that
//! the host source then changes too, beside other host upgrades. Its promotion is
//! refused with the file named and Update from host source offered; the update merges
//! the host source in (conflict markers in the shared file, the host's other upgrades
//! as they are, the sync recorded) and its turn resolves the conflict; the next
//! promotion composes without conflicts and gets as far as the build gate (which this
//! fixture source, not being a crate, cannot pass). A rewind to before the update puts
//! the thread back on its import, and the refusal returns. The model is told how its
//! changes reach the host in its system prompt.
#![cfg(target_os = "linux")]
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Every request body the fake provider received.
static SEEN: Mutex<Vec<Value>> = Mutex::new(Vec::new());
/// Refuse the next Update from host source request once (403), as a provider might.
static REFUSE_UPDATE: AtomicBool = AtomicBool::new(false);

fn text_of(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A scripted OpenAI chat-completions model. After a tool result it says it is done.
/// A prompt with EDIT_A writes the thread's change to a.txt; an Update from host
/// source prompt that lists a.txt as a conflict writes its resolution (refused once
/// first when REFUSE_UPDATE is set).
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
    let (mut first, mut length) = (String::new(), 0);
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
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        return;
    }
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    SEEN.lock().unwrap().push(request.clone());
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let last = messages.last().cloned().unwrap_or(Value::Null);
    let prompt = text_of(&last);
    let chunk = |delta: Value, finish: Value| {
        json!({"id": "x", "object": "chat.completion.chunk", "model": request["model"],
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    };
    let write = |path: &str, content: &str| {
        vec![
            chunk(
                json!({"role": "assistant", "tool_calls": [{"index": 0, "id": format!("call_{}", messages.len()), "type": "function",
                    "function": {"name": "write_file", "arguments": json!({"path": path, "content": content}).to_string()}}]}),
                Value::Null,
            ),
            chunk(json!({}), json!("tool_calls")),
        ]
    };
    let say = |text: &str| {
        vec![
            chunk(json!({"role": "assistant", "content": text}), Value::Null),
            chunk(json!({}), json!("stop")),
        ]
    };
    let update = prompt.contains("[Update from host source]");
    if update && REFUSE_UPDATE.swap(false, Ordering::SeqCst) {
        let refusal = r#"{"error":{"message":"not now"}}"#;
        let _ = write!(
            stream,
            "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{refusal}",
            refusal.len()
        );
        return;
    }
    let chunks = if last["role"] == "tool" {
        say("Done.")
    } else if update && prompt.contains("\n- a.txt") {
        write("a.txt", "1\n2\nhost and thread\n")
    } else if prompt.contains("EDIT_A") {
        write("a.txt", "1\n2\nthread\n")
    } else {
        say("Nothing to do.")
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

struct Serve {
    child: Child,
    port: u16,
    cookie: String,
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Serve {
    /// A supervised-looking host (its binary under HOST/bin, SPINDLE_SUPERVISED=1) with
    /// self-edit from `source`, every provider request sent to `provider`, paired.
    fn start(root: &Path, source: &Path, provider: u16) -> Self {
        let host = root.join("host");
        std::fs::create_dir_all(host.join("bin")).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let binary = host.join("bin/spindle");
        if std::fs::hard_link(env!("CARGO_BIN_EXE_spindle"), &binary).is_err() {
            std::fs::copy(env!("CARGO_BIN_EXE_spindle"), &binary).unwrap();
        }
        let real = PathBuf::from(std::env::var_os("HOME").unwrap());
        let rustup = std::env::var_os("RUSTUP_HOME").map_or(real.join(".rustup"), PathBuf::from);
        let cargo = std::env::var_os("CARGO_HOME").map_or(real.join(".cargo"), PathBuf::from);
        let mut child = Command::new(&binary)
            .arg("--dir")
            .arg(&host)
            .args(["serve", "--port", "0", "--dev-source"])
            .arg(source)
            .env("HOME", root.join("home"))
            .env("DEXT_HOME", root.join("home/.dext"))
            .env("DEXT_BASE_URL", format!("http://127.0.0.1:{provider}"))
            .env("RUSTUP_HOME", rustup)
            .env("CARGO_HOME", cargo)
            .env("SPINDLE_SUPERVISED", "1")
            .env_remove("OPENAI_BASE_URL")
            .env_remove("ANTHROPIC_BASE_URL")
            .env_remove("SPINDLE_CODE_POLICY")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let port = line
            .split("127.0.0.1:")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .and_then(|port| port.parse().ok())
            .unwrap_or_else(|| panic!("no port in {line:?}"));
        let mut serve = Self {
            child,
            port,
            cookie: String::new(),
        };
        let key = std::fs::read_to_string(host.join("pairing.key")).unwrap();
        let (status, head, _) = serve.http("POST", "/api/pair", Some(&json!({"key": key.trim()})));
        assert_eq!(status, 200, "{head}");
        serve.cookie = head
            .split("spindle=")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .unwrap()
            .to_string();
        serve
    }

    fn http(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, String, Vec<u8>) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let body = body.map(Value::to_string).unwrap_or_default();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nX-Spindle: 1\r\nCookie: spindle={cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len(),
            port = self.port,
            cookie = self.cookie,
        )
        .unwrap();
        let mut bytes = vec![];
        stream.read_to_end(&mut bytes).unwrap();
        let split = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&bytes[..split]).into_owned();
        (
            head[9..12].parse().unwrap(),
            head,
            bytes[split + 4..].to_vec(),
        )
    }

    fn json(&self, path: &str) -> Value {
        let (status, head, body) = self.http("GET", path, None);
        assert_eq!(status, 200, "{path}: {head}");
        serde_json::from_slice(&body).unwrap()
    }

    /// Submits a command and waits for its outcome: (state, error).
    fn run(&self, command: Value) -> (String, String) {
        let (status, head, body) = self.http("POST", "/api/commands", Some(&command));
        let receipt: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        assert_eq!(status, 202, "{command}: {head} {receipt}");
        let id = receipt["id"].as_str().unwrap().to_string();
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let receipt = self.json(&format!("/api/receipts/{id}"));
            if receipt["state"] != "running" {
                return (
                    receipt["state"].as_str().unwrap().into(),
                    receipt["error"].as_str().unwrap_or_default().into(),
                );
            }
            assert!(Instant::now() < deadline, "{command} did not finish");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn file(&self, session: &str, revision: u64, path: &str) -> Option<String> {
        let (status, _, body) = self.http(
            "GET",
            &format!("/api/sessions/{session}/file?revision={revision}&path={path}"),
            None,
        );
        (status == 200).then(|| String::from_utf8(body).unwrap())
    }
}

#[test]
fn a_stale_self_edit_thread_updates_from_the_host_source_and_promotes_again() {
    let root = std::env::temp_dir().join(format!("spindle-self-edit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let source = root.join("src/spindle");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(root.join("src/dext-engine")).unwrap();
    std::fs::create_dir_all(root.join("home/.dext")).unwrap();
    std::fs::write(source.join("a.txt"), "1\n2\n3\n").unwrap();
    std::fs::write(source.join("b.txt"), "b\n").unwrap();
    let serve = Serve::start(&root, &source, fake_provider());

    // A self-edit thread whose turn changes a.txt.
    let (status, head, body) = serve.http(
        "POST",
        "/api/commands",
        Some(&json!({"request_id": "new-1", "kind": "new", "name": "self", "provider": "local", "dev": true, "text": "EDIT_A: change a.txt"})),
    );
    assert_eq!(status, 202, "{head}");
    let session = serde_json::from_slice::<Value>(&body).unwrap()["session"]
        .as_str()
        .unwrap()
        .to_string();
    let deadline = Instant::now() + Duration::from_secs(300);
    while serve.json("/api/receipts/new-1")["state"] == "running" {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(100));
    }
    let snapshot = serve.json(&format!("/api/sessions/{session}/snapshot"));
    let first = snapshot["head"].as_u64().unwrap();
    assert_eq!(snapshot["complete"], true, "{snapshot}");
    assert_eq!(
        serve.file(&session, first, "a.txt").as_deref(),
        Some("1\n2\nthread\n")
    );
    // The model was told how its changes reach the host.
    let settings: Value = serde_json::from_slice(
        &std::fs::read(
            root.join("host/sessions")
                .join(&session)
                .join("session.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(settings["self_edit"], true);
    let system = text_of(&SEEN.lock().unwrap()[0]["messages"][0]);
    for told in [
        "self-edit thread",
        "spindle_install",
        "Spindle change card",
        "/install",
        "never the host's binary",
        "never tell the user to restart the host",
        "spindle_source with action update",
        "update from host source",
        "store schema",
    ] {
        assert!(system.contains(told), "{told}: {system}");
    }
    for gone in ["Settings", "Review & promote"] {
        assert!(!system.contains(gone), "{gone}: {system}");
    }

    // The host source moves on: the same line of a.txt, and other upgrades.
    std::fs::write(source.join("a.txt"), "1\n2\nhost\n").unwrap();
    std::fs::write(source.join("b.txt"), "b host\n").unwrap();
    std::fs::write(source.join("added.txt"), "new on the host\n").unwrap();
    let promote = |id: &str, head: u64| {
        serve.run(json!({"request_id": id, "session": session, "kind": "promote", "expected_revision": head}))
    };
    let (state, error) = promote("promote-1", first);
    assert_eq!(state, "failed");
    assert!(
        error.starts_with("Not promoted:")
            && error.contains("both changed a.txt.")
            && error.contains("Tap Update on the Spindle change card")
            && !error.contains("Settings"),
        "{error}"
    );
    let snapshot = serve.json(&format!("/api/sessions/{session}/snapshot"));
    assert_eq!(snapshot["source_moved"]["paths"], json!(["a.txt"]));
    // A legacy self-edit thread holds the source as its whole workspace.
    let self_edit = &snapshot["self_edit"];
    assert_eq!(self_edit["prefix"], "", "{self_edit}");
    assert_eq!(self_edit["changed"], json!(["a.txt"]));
    assert_eq!(self_edit["conflicts"], json!(["a.txt"]));
    assert_eq!(self_edit["base_revision"], 1, "the import");
    assert!(self_edit["request"].is_null());

    // Update from host source: a turn over the merged workspace resolves the conflict.
    // The provider refuses its first request; Retry sends it again from the same
    // merged workspace, so the sync recorded for it still applies.
    let seen = SEEN.lock().unwrap().len();
    REFUSE_UPDATE.store(true, Ordering::SeqCst);
    let (state, error) = serve.run(json!({"request_id": "update-1", "session": session, "kind": "update", "expected_revision": first}));
    assert_eq!(state, "failed");
    assert!(error.contains("refused before any reply"), "{error}");
    let snapshot = serve.json(&format!("/api/sessions/{session}/snapshot"));
    assert_eq!(snapshot["rejected"]["status"], 403, "{snapshot}");
    let refused = snapshot["head"].as_u64().unwrap();
    let (state, error) = serve.run(json!({"request_id": "retry-1", "session": session, "kind": "retry", "expected_revision": refused}));
    assert_eq!((state.as_str(), error.as_str()), ("completed", ""));
    let prompt = text_of(
        SEEN.lock().unwrap()[seen]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
    );
    assert!(
        prompt.starts_with("[Update from host source]")
            && prompt.contains("Changed by the host:\n- added.txt\n- b.txt")
            && prompt.contains("<<<<<<< host source"),
        "{prompt}"
    );
    let start = first + 1;
    let marked = serve.file(&session, start, "a.txt").unwrap();
    assert!(
        marked.contains("<<<<<<< host source\nhost\n")
            && marked.contains("=======\nthread\n>>>>>>> this thread"),
        "{marked}"
    );
    let snapshot = serve.json(&format!("/api/sessions/{session}/snapshot"));
    let updated = snapshot["head"].as_u64().unwrap();
    assert!(snapshot.get("source_moved").is_none(), "{snapshot}");
    assert_eq!(snapshot["complete"], true);
    for (path, text) in [
        ("a.txt", "1\n2\nhost and thread\n"),
        ("b.txt", "b host\n"),
        ("added.txt", "new on the host\n"),
    ] {
        assert_eq!(
            serve.file(&session, updated, path).as_deref(),
            Some(text),
            "{path}"
        );
    }
    let records = std::fs::read_dir(root.join("host/sessions").join(&session).join("host-sync"))
        .unwrap()
        .count();
    assert_eq!(records, 1, "one sync recorded");

    // Promotion now composes without conflicts and goes on to the build gates (which
    // this fixture source, not a crate, fails: nothing is installed).
    let (state, error) = promote("promote-2", updated);
    assert_eq!(state, "failed");
    assert_eq!(error, "cargo fmt --all -- --check failed");
    let snapshot = serve.json(&format!("/api/sessions/{session}/snapshot"));
    assert!(snapshot.get("source_moved").is_none(), "{snapshot}");

    // Rewinding to before the update takes the thread back to its import: the
    // update's sync is off the line, so the conflict is reported again.
    let (state, error) = serve.run(json!({"request_id": "rewind-1", "session": session, "kind": "rewind", "revision": first, "expected_revision": updated}));
    assert_eq!((state.as_str(), error.as_str()), ("completed", ""));
    let rewound = serve.json(&format!("/api/sessions/{session}/snapshot"))["head"]
        .as_u64()
        .unwrap();
    let (state, error) = promote("promote-3", rewound);
    assert_eq!(state, "failed");
    assert!(error.starts_with("Not promoted:"), "{error}");

    // An update needs a self-edit thread that finished its turn.
    let (status, _, body) = serve.http(
        "POST",
        "/api/commands",
        Some(&json!({"request_id": "update-stale", "session": session, "kind": "update", "expected_revision": first})),
    );
    assert_eq!(status, 409, "{}", String::from_utf8_lossy(&body));
    drop(serve);
    let _ = Command::new("chmod")
        .args(["-R", "u+rwx"])
        .arg(&root)
        .status();
    let _ = std::fs::remove_dir_all(&root);
}
