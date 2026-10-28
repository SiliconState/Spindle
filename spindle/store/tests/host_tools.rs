//! Host tools end to end through `spindle serve`, against a scripted loopback provider
//! (no paid calls): an ordinary thread checks out Spindle's source into `spindle/`,
//! changes it and asks to install; the user's Install (`promote`) composes from
//! `spindle/` against the checkout (as far as the build gate this fixture cannot pass),
//! is refused with the conflicting file when the host source changed the same lines,
//! and `spindle_source update` / the `update` command merge the host source into
//! `spindle/` only. `show` puts files in the chat as cards (snapshot and event
//! stream), and `settings` queues a change that applies when the turn ends.
#![cfg(target_os = "linux")]
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One model step: a tool call, or a final text.
enum Step {
    Call(&'static str, Value),
    Say(&'static str),
}

/// What the scripted model does next, given the turn's prompt and how many tool
/// results the turn has seen; it may look at the request (and at the host) first.
type Script = dyn Fn(&str, usize, &Value) -> Step + Send + Sync;

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

/// A scripted OpenAI chat-completions model on a loopback port; every request body is
/// kept in `seen`.
fn fake_provider(script: Arc<Script>, seen: Arc<Mutex<Vec<Value>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (script, seen) = (script.clone(), seen.clone());
            std::thread::spawn(move || answer(stream, &*script, &seen));
        }
    });
    port
}

fn answer(mut stream: TcpStream, script: &Script, seen: &Mutex<Vec<Value>>) {
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
    seen.lock().unwrap().push(request.clone());
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let prompt_at = messages
        .iter()
        .rposition(|m| m["role"] == "user")
        .unwrap_or(0);
    let prompt = text_of(&messages[prompt_at]);
    let step = messages[prompt_at..]
        .iter()
        .filter(|m| m["role"] == "tool")
        .count();
    let chunk = |delta: Value, finish: Value| {
        json!({"id": "x", "object": "chat.completion.chunk", "model": request["model"],
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    };
    let chunks = match script(&prompt, step, &request) {
        Step::Call(name, arguments) => vec![
            chunk(
                json!({"role": "assistant", "tool_calls": [{"index": 0, "id": format!("call_{}", messages.len()), "type": "function",
                    "function": {"name": name, "arguments": arguments.to_string()}}]}),
                Value::Null,
            ),
            chunk(json!({}), json!("tool_calls")),
        ],
        Step::Say(text) => vec![
            chunk(json!({"role": "assistant", "content": text}), Value::Null),
            chunk(json!({}), json!("stop")),
        ],
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

/// A host's address and paired cookie, for a script that looks at the host mid-turn.
#[derive(Clone, Default)]
struct Address(Arc<Mutex<Option<(u16, String)>>>);

/// One HTTP exchange with a host: (status, head, body).
fn exchange(
    port: u16,
    cookie: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let body = body.map(Value::to_string).unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nX-Spindle: 1\r\nCookie: spindle={cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len(),
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
    /// A supervised-looking host (binary under HOST/bin, SPINDLE_SUPERVISED=1) that
    /// offers `source` for self-edit and sends every provider request to `provider`.
    fn start(root: &Path, source: &Path, provider: u16) -> Self {
        let host = root.join("host");
        std::fs::create_dir_all(host.join("bin")).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let binary = host.join("bin/spindle");
        // Tests run in parallel: a copy still open for writing in a child another
        // thread forked meanwhile cannot be executed (ETXTBSY), so copies and spawns
        // take turns, and a spawn that still meets it is tried again.
        let turn = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        if std::fs::hard_link(env!("CARGO_BIN_EXE_spindle"), &binary).is_err() {
            std::fs::copy(env!("CARGO_BIN_EXE_spindle"), &binary).unwrap();
        }
        let real = PathBuf::from(std::env::var_os("HOME").unwrap());
        let rustup = std::env::var_os("RUSTUP_HOME").map_or(real.join(".rustup"), PathBuf::from);
        let cargo = std::env::var_os("CARGO_HOME").map_or(real.join(".cargo"), PathBuf::from);
        let mut command = Command::new(&binary);
        command
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
            .stderr(Stdio::inherit());
        let mut tries = 0;
        let mut child = loop {
            match command.spawn() {
                Err(error)
                    if error.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 50 =>
                {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(100));
                }
                spawned => break spawned.unwrap(),
            }
        };
        drop(turn);
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
        let key = std::fs::read_to_string(host.join("pairing.key")).unwrap();
        let (status, head, _) = exchange(
            port,
            "",
            "POST",
            "/api/pair",
            Some(&json!({"key": key.trim()})),
        );
        assert_eq!(status, 200, "{head}");
        let cookie = head
            .split("spindle=")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .unwrap()
            .to_string();
        Self {
            child,
            port,
            cookie,
        }
    }

    fn http(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, String, Vec<u8>) {
        exchange(self.port, &self.cookie, method, path, body)
    }

    fn json(&self, path: &str) -> Value {
        let (status, head, body) = self.http("GET", path, None);
        assert_eq!(status, 200, "{path}: {head}");
        serde_json::from_slice(&body).unwrap()
    }

    fn snapshot(&self, session: &str) -> Value {
        self.json(&format!("/api/sessions/{session}/snapshot"))
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

    /// A new thread with `text` as its first prompt, run to its end: its id.
    fn new_thread(&self, request: &str, text: &str) -> String {
        let (status, head, body) = self.http(
            "POST",
            "/api/commands",
            Some(&json!({"request_id": request, "kind": "new", "name": "thread", "provider": "local", "text": text})),
        );
        assert_eq!(status, 202, "{head}");
        let session = serde_json::from_slice::<Value>(&body).unwrap()["session"]
            .as_str()
            .unwrap()
            .to_string();
        let deadline = Instant::now() + Duration::from_secs(300);
        while self.json(&format!("/api/receipts/{request}"))["state"] == "running" {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(100));
        }
        let receipt = self.json(&format!("/api/receipts/{request}"));
        assert_eq!(receipt["state"], "completed", "{receipt}");
        session
    }

    /// Says `text` in a thread and waits for the turn to end.
    fn say(&self, session: &str, request: &str, text: &str) {
        let head = self.snapshot(session)["head"].as_u64().unwrap();
        let (state, error) = self.run(json!({"request_id": request, "session": session, "kind": "say", "text": text, "expected_revision": head}));
        assert_eq!((state.as_str(), error.as_str()), ("completed", ""));
    }

    fn file(&self, session: &str, revision: u64, path: &str) -> Option<String> {
        let (status, _, body) = self.http(
            "GET",
            &format!("/api/sessions/{session}/file?revision={revision}&path={path}"),
            None,
        );
        (status == 200).then(|| String::from_utf8_lossy(&body).into_owned())
    }

    /// Every event of the thread's retained ring, until its last command's `done`.
    fn events(&self, session: &str) -> Vec<Value> {
        let boot = self.json("/api/ping")["boot"].as_str().unwrap().to_string();
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "GET /api/sessions/{session}/events?after={boot}:0 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: spindle={cookie}\r\n\r\n",
            port = self.port,
            cookie = self.cookie,
        )
        .unwrap();
        let mut text = String::new();
        let mut chunk = [0; 65536];
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => text.push_str(&String::from_utf8_lossy(&chunk[..n])),
            }
            if text.contains("\"type\":\"done\"") && text.ends_with("\n\n") {
                break;
            }
        }
        text.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str(data).ok())
            .collect()
    }
}

fn temp(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("spindle-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("home/.dext")).unwrap();
    root
}

/// Taken while a test copies the binary and spawns a host, or forks anything else.
static SPAWNING: Mutex<()> = Mutex::new(());

fn clean(root: &Path) {
    let _turn = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
    let _ = Command::new("chmod")
        .args(["-R", "u+rwx"])
        .arg(root)
        .status();
    let _ = std::fs::remove_dir_all(root);
}

/// The content of every tool result in a snapshot's messages, in order.
fn tool_results(snapshot: &Value) -> Vec<Value> {
    snapshot["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|block| block["type"] == "tool_result")
        .collect()
}

#[test]
fn any_thread_checks_out_spindle_asks_to_install_and_updates_its_checkout() {
    let root = temp("checkout");
    let source = root.join("src/spindle");
    for (path, text) in [
        ("a.txt", "1\n2\n3\n"),
        ("b.txt", "b\n"),
        (".git/HEAD", "ref: refs/heads/main\n"),
        ("target/junk", "build output\n"),
    ] {
        std::fs::create_dir_all(source.join(path).parent().unwrap()).unwrap();
        std::fs::write(source.join(path), text).unwrap();
    }
    std::fs::create_dir_all(root.join("src/dext-engine")).unwrap();
    let script: Arc<Script> = Arc::new(|prompt: &str, step: usize, _: &Value| {
        if prompt.contains("CHANGE_SPINDLE") {
            match step {
                0 => Step::Call("spindle_source", json!({"action": "checkout"})),
                // The sandbox has the link, the engine mount and the toolchain.
                1 => Step::Call(
                    "bash",
                    json!({"command": "test -L dext-engine && test -d dext-engine/ && cargo --version >/dev/null && cat spindle/b.txt"}),
                ),
                2 => Step::Call(
                    "write_file",
                    json!({"path": "spindle/a.txt", "content": "1\n2\nthread\n"}),
                ),
                3 => Step::Call(
                    "write_file",
                    json!({"path": "notes.txt", "content": "mine\n"}),
                ),
                4 => Step::Call("spindle_install", json!({"summary": "Change a.txt"})),
                _ => Step::Say("Ready: tap Install."),
            }
        } else if prompt.contains("UPDATE_SPINDLE") && step == 0 {
            Step::Call("spindle_source", json!({"action": "update"}))
        } else if prompt.contains("CHECKOUT_AGAIN") && step == 0 {
            Step::Call("spindle_source", json!({"action": "checkout"}))
        } else if prompt.contains("REQUEST_AGAIN") && step == 0 {
            Step::Call(
                "spindle_install",
                json!({"summary": "Merged with the host"}),
            )
        } else {
            Step::Say("Done.")
        }
    });
    let seen = Arc::new(Mutex::new(vec![]));
    let serve = Serve::start(&root, &source, fake_provider(script, seen.clone()));

    // An ordinary thread (not created as a self-edit thread) checks out the source,
    // changes it and asks to install.
    let session = serve.new_thread("new-1", "CHANGE_SPINDLE: make a.txt say thread");
    let snapshot = serve.snapshot(&session);
    let head = snapshot["head"].as_u64().unwrap();
    assert_eq!(snapshot["complete"], true, "{snapshot}");
    let self_edit = &snapshot["self_edit"];
    assert_eq!(self_edit["prefix"], "spindle/", "{self_edit}");
    assert_eq!(self_edit["changed"], json!(["a.txt"]));
    assert_eq!(self_edit["changed_count"], 1);
    assert_eq!(self_edit["behind"], false);
    assert_eq!(self_edit["conflicts"], json!([]));
    assert_eq!(self_edit["installing"], false);
    assert!(!self_edit["host_build"].as_str().unwrap().is_empty());
    // The checkout's revision holds the base as it is, to diff the changes against.
    let base_revision = self_edit["base_revision"].as_u64().unwrap();
    assert!(base_revision < head);
    assert_eq!(
        serve
            .file(&session, base_revision, "spindle/a.txt")
            .as_deref(),
        Some("1\n2\n3\n")
    );
    let request = &self_edit["request"];
    assert_eq!(request["summary"], "Change a.txt");
    assert!(request["revision"].as_u64().unwrap() < head && request["at"].is_u64());
    for (path, text) in [
        ("spindle/a.txt", Some("1\n2\nthread\n")),
        ("spindle/b.txt", Some("b\n")),
        ("notes.txt", Some("mine\n")),
        ("spindle/.git/HEAD", None),
        ("spindle/target/junk", None),
    ] {
        assert_eq!(serve.file(&session, head, path).as_deref(), text, "{path}");
    }
    let results = tool_results(&snapshot);
    assert!(
        results[0]["content"]
            .as_str()
            .unwrap()
            .starts_with("Checked out Spindle's source"),
        "{}",
        results[0]
    );
    assert_eq!(results[1]["content"], "b\n", "{}", results[1]);
    assert!(
        results[4]["content"]
            .as_str()
            .unwrap()
            .contains("Install requested")
    );
    // spindle/'s ../dext-engine paths resolve to the engine's read-only mount.
    let files = serve.json(&format!("/v1/sessions/{session}/files"));
    assert_eq!(
        files["links"],
        json!([{"path": "dext-engine", "target": "/dext-engine"}]),
        "{files}"
    );
    // Building Spindle is on for the thread's next commands.
    let settings: Value = serde_json::from_slice(
        &std::fs::read(
            root.join("host/sessions")
                .join(&session)
                .join("session.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(settings["toolchain"], true);
    assert_eq!(
        settings["self_edit"], false,
        "not a legacy self-edit thread"
    );
    assert!(
        settings["sources"][0]
            .as_str()
            .unwrap()
            .ends_with("dext-engine"),
        "{settings}"
    );
    {
        let seen = seen.lock().unwrap();
        let system = text_of(&seen[0]["messages"][0]);
        for told in [
            "spindle_source",
            "Spindle change card",
            "/install",
            "never tell the user to restart the host",
        ] {
            assert!(system.contains(told), "{told}: {system}");
        }
        assert!(!system.contains("This is a self-edit thread"), "{system}");
        assert!(!system.contains("Settings"), "{system}");
        let tools = |n: usize| seen[n]["tools"].to_string();
        assert!(!tools(0).contains("cargo/rustc are available"));
        assert!(
            tools(1).contains("cargo/rustc are available"),
            "the toolchain is on from the call after the checkout"
        );
        let names: Vec<&str> = seen[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        for name in ["show", "settings", "spindle_source", "spindle_install"] {
            assert!(names.contains(&name), "{names:?}");
        }
    }

    // The user's Install composes from spindle/ and goes on to the build gates (which
    // this fixture source, not a crate, fails: nothing is installed).
    let promote = |id: &str, head: u64| {
        serve.run(json!({"request_id": id, "session": session, "kind": "promote", "expected_revision": head}))
    };
    let (state, error) = promote("promote-1", head);
    assert_eq!(
        (state.as_str(), error.as_str()),
        ("failed", "cargo fmt --all -- --check failed")
    );
    assert_eq!(
        serve.snapshot(&session)["self_edit"]["request"]["summary"],
        "Change a.txt",
        "a request stays until it is installed or dismissed"
    );

    // The host source moves on: the same line of a.txt, and a new file.
    std::fs::write(source.join("a.txt"), "1\n2\nhost\n").unwrap();
    std::fs::write(source.join("c.txt"), "c\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while serve.snapshot(&session)["self_edit"]["behind"] != true {
        assert!(Instant::now() < deadline, "the host source moved");
        std::thread::sleep(Duration::from_millis(250));
    }
    let (state, error) = promote("promote-2", head);
    assert_eq!(state, "failed");
    assert!(
        error.starts_with("Not promoted:") && error.contains("both changed a.txt."),
        "{error}"
    );
    let self_edit = serve.snapshot(&session)["self_edit"].clone();
    assert_eq!(self_edit["conflicts"], json!(["a.txt"]), "{self_edit}");

    // The request can be dismissed (once).
    let dismiss = json!({"request_id": "dismiss-1", "session": session, "kind": "dismiss_install"});
    let (state, error) = serve.run(dismiss.clone());
    assert_eq!((state.as_str(), error.as_str()), ("completed", ""));
    assert!(serve.snapshot(&session)["self_edit"]["request"].is_null());
    let mut again = dismiss;
    again["request_id"] = json!("dismiss-2");
    let (status, _, _) = serve.http("POST", "/api/commands", Some(&again));
    assert_eq!(status, 400);

    // spindle_source update merges the host source into spindle/ only: conflict
    // markers in a.txt, the host's new file, the thread's own files untouched.
    serve.say(
        &session,
        "say-update",
        "UPDATE_SPINDLE: bring in the host's source",
    );
    let snapshot = serve.snapshot(&session);
    let updated = snapshot["head"].as_u64().unwrap();
    let merged = tool_results(&snapshot).last().cloned().unwrap();
    let content = merged["content"].as_str().unwrap();
    assert!(
        content.starts_with("[spindle_source update]")
            && content.contains("Changed by the host:\n- spindle/c.txt")
            && content.contains("<<<<<<< host source")
            && content.contains("\n- spindle/a.txt"),
        "{content}"
    );
    let marked = serve.file(&session, updated, "spindle/a.txt").unwrap();
    assert!(
        marked.contains("<<<<<<< host source\nhost\n")
            && marked.contains("=======\nthread\n>>>>>>> this thread"),
        "{marked}"
    );
    assert_eq!(
        serve.file(&session, updated, "spindle/c.txt").as_deref(),
        Some("c\n")
    );
    assert_eq!(
        serve.file(&session, updated, "notes.txt").as_deref(),
        Some("mine\n")
    );
    assert_eq!(serve.file(&session, updated, "c.txt"), None);
    let self_edit = &snapshot["self_edit"];
    assert_eq!(
        self_edit["conflicts"],
        json!([]),
        "the refused head is gone"
    );
    assert_eq!(self_edit["behind"], false, "{self_edit}");
    assert_eq!(self_edit["changed"], json!(["a.txt"]));
    assert!(
        self_edit["base_revision"].is_null(),
        "no revision holds the new base as it is: the merge kept the thread's change"
    );

    // The update command scopes to spindle/ too.
    std::fs::write(source.join("b.txt"), "b host\n").unwrap();
    let seen_before = seen.lock().unwrap().len();
    let (state, error) = serve.run(json!({"request_id": "update-1", "session": session, "kind": "update", "expected_revision": updated}));
    assert_eq!((state.as_str(), error.as_str()), ("completed", ""));
    let prompt = text_of(
        seen.lock().unwrap()[seen_before]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
    );
    assert!(
        prompt.starts_with("[Update from host source]")
            && prompt.contains("into spindle/ in this workspace")
            && prompt.contains("Changed by the host:\n- spindle/b.txt"),
        "{prompt}"
    );
    let head = serve.snapshot(&session)["head"].as_u64().unwrap();
    assert_eq!(
        serve.file(&session, head, "spindle/b.txt").as_deref(),
        Some("b host\n")
    );
    assert_eq!(
        serve.file(&session, head, "notes.txt").as_deref(),
        Some("mine\n")
    );
    assert!(
        serve
            .file(&session, head, "spindle/a.txt")
            .unwrap()
            .contains(">>>>>>> this thread")
    );

    // A second checkout is refused; the first stays.
    serve.say(&session, "say-again", "CHECKOUT_AGAIN");
    let last = tool_results(&serve.snapshot(&session))
        .last()
        .cloned()
        .unwrap();
    assert_eq!(last["is_error"], true);
    assert!(
        last["content"]
            .as_str()
            .unwrap()
            .contains("spindle/ already holds Spindle's source"),
        "{last}"
    );

    // A request rewound off the head's line is no longer offered.
    let before = serve.snapshot(&session)["head"].as_u64().unwrap();
    serve.say(&session, "say-request", "REQUEST_AGAIN");
    let snapshot = serve.snapshot(&session);
    assert_eq!(
        snapshot["self_edit"]["request"]["summary"],
        "Merged with the host"
    );
    let head = snapshot["head"].as_u64().unwrap();
    let (state, error) = serve.run(json!({"request_id": "rewind-1", "session": session, "kind": "rewind", "revision": before, "expected_revision": head}));
    assert_eq!((state.as_str(), error.as_str()), ("completed", ""));
    let self_edit = serve.snapshot(&session)["self_edit"].clone();
    assert!(self_edit["request"].is_null(), "{self_edit}");
    assert_eq!(self_edit["prefix"], "spindle/");
    drop(serve);
    clean(&root);
}

/// A rollback is the host's: no thread, no self-edit, the same checks, and when the
/// kept previous binary passes them it is installed and the host exits for its
/// supervisor.
#[test]
fn rollback_is_a_host_command_with_its_checks() {
    let root = temp("rollback");
    let source = root.join("src/spindle");
    std::fs::create_dir_all(&source).unwrap();
    let script: Arc<Script> = Arc::new(|_: &str, _: usize, _: &Value| Step::Say("Done."));
    let mut serve = Serve::start(&root, &source, fake_provider(script, Arc::default()));
    serve.new_thread("new-thread", "hello");
    // Without a kept previous binary the checks refuse it; nothing is installed.
    let (state, error) = serve.run(json!({"request_id": "rollback-1", "kind": "rollback"}));
    assert_eq!(state, "failed");
    assert!(error.contains(".spindle.previous"), "{error}");
    assert_eq!(serve.json("/api/receipts/rollback-1")["session"], "");
    assert!(
        serve.json("/api/ping")["build"].is_string(),
        "still serving"
    );
    {
        let _turn = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        std::fs::copy(
            env!("CARGO_BIN_EXE_spindle"),
            root.join("host/bin/.spindle.previous"),
        )
        .unwrap();
    }
    let (status, head, _) = serve.http(
        "POST",
        "/api/commands",
        Some(&json!({"request_id": "rollback-2", "kind": "rollback"})),
    );
    assert_eq!(status, 202, "{head}");
    let deadline = Instant::now() + Duration::from_secs(240);
    let code = loop {
        if let Some(status) = serve.child.try_wait().unwrap() {
            break status.code();
        }
        assert!(Instant::now() < deadline, "the host did not restart");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(code, Some(75), "exits for the supervisor to restart it");
    assert!(root.join("host/upgrade.pending").is_file());
    drop(serve);
    clean(&root);
}

#[test]
fn show_puts_workspace_files_in_the_chat_as_cards() {
    let root = temp("show");
    let source = root.join("src/spindle");
    std::fs::create_dir_all(&source).unwrap();
    let script: Arc<Script> = Arc::new(|prompt: &str, step: usize, _: &Value| {
        if !prompt.contains("SHOW_FILES") {
            return Step::Say("Done.");
        }
        match step {
            0 => Step::Call(
                "write_file",
                json!({"path": "site/index.html", "content": "<h1>hi</h1>"}),
            ),
            1 => Step::Call(
                "write_file",
                json!({"path": "notes.md", "content": "# Notes\n"}),
            ),
            2 => Step::Call(
                "write_file",
                json!({"path": "main.rs", "content": "fn main() {}\n"}),
            ),
            3 => Step::Call(
                "bash",
                json!({"command": "printf '\\x89PNG\\r\\n\\x1a\\n' > logo.png && printf 'a\\0b' > data.bin"}),
            ),
            4 => Step::Call(
                "show",
                json!({"paths": ["site/", "./logo.png", "notes.md", "main.rs", "data.bin"], "title": "Demo"}),
            ),
            5 => Step::Call("show", json!({"paths": ["missing.txt", "site"]})),
            _ => Step::Say("Shown."),
        }
    });
    let serve = Serve::start(&root, &source, fake_provider(script, Arc::default()));
    let session = serve.new_thread("new-show", "SHOW_FILES");
    let snapshot = serve.snapshot(&session);
    assert!(
        snapshot["self_edit"].is_null(),
        "no checkout: {}",
        snapshot["self_edit"]
    );
    let results = tool_results(&snapshot);
    let shown = &results[4];
    assert!(!shown["is_error"].as_bool().unwrap_or(false), "{shown}");
    let display = shown["display"]
        .as_array()
        .unwrap_or_else(|| panic!("{shown}"));
    let kinds: Vec<(&str, &str)> = display
        .iter()
        .map(|d| (d["path"].as_str().unwrap(), d["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("site/index.html", "html"),
            ("logo.png", "image"),
            ("notes.md", "markdown"),
            ("main.rs", "text"),
            ("data.bin", "file"),
        ]
    );
    let revision = display[0]["revision"].as_u64().unwrap();
    // One caption over several cards names each one too.
    for card in display {
        let name = card["path"].as_str().unwrap().rsplit('/').next().unwrap();
        assert_eq!(card["title"], format!("Demo: {name}").as_str());
        assert_eq!(card["revision"], revision);
    }
    assert_eq!(display[0]["size"], 11);
    assert_eq!(display[4]["size"], 3);
    // The second call names what is missing and shows nothing.
    let refused = &results[5];
    assert_eq!(refused["is_error"], true);
    assert!(refused.get("display").is_none());
    assert!(
        refused["content"]
            .as_str()
            .unwrap()
            .contains("no file `missing.txt` in the workspace"),
        "{refused}"
    );
    // The event stream's result carries the same cards.
    let events = serve.events(&session);
    let streamed: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "result" && e.get("display").is_some())
        .collect();
    assert_eq!(streamed.len(), 1, "{events:?}");
    assert_eq!(streamed[0]["display"], shown["display"]);
    // v1 messages carry them too.
    let messages = serve.json(&format!("/v1/sessions/{session}/messages"));
    let carried = messages["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|b| b.get("display").is_some())
        .count();
    assert_eq!(carried, 1);
    // Raster images come back as images; everything else stays plain text.
    let (status, head, body) = serve.http(
        "GET",
        &format!("/api/sessions/{session}/file?revision={revision}&path=logo.png"),
        None,
    );
    assert_eq!(status, 200);
    assert!(head.contains("Content-Type: image/png"), "{head}");
    assert_eq!(&body[..4], b"\x89PNG");
    let (_, head, _) = serve.http(
        "GET",
        &format!("/api/sessions/{session}/file?revision={revision}&path=site/index.html"),
        None,
    );
    assert!(head.contains("Content-Type: text/plain"), "{head}");
    drop(serve);
    clean(&root);
}

#[test]
fn settings_queue_for_the_end_of_the_turn_and_models_are_listed() {
    let root = temp("settings");
    let source = root.join("src/spindle");
    std::fs::create_dir_all(&source).unwrap();
    let address = Address::default();
    let mid_turn = Arc::new(Mutex::new(Value::Null));
    let script: Arc<Script> = {
        let (address, mid_turn) = (address.clone(), mid_turn.clone());
        Arc::new(move |prompt: &str, step: usize, request: &Value| {
            if !prompt.contains("CHANGE_SETTINGS") {
                return Step::Say("Done.");
            }
            match step {
                0 => Step::Call("settings", json!({})),
                1 => Step::Call("settings", json!({"model": "no-such-model"})),
                2 => Step::Call("settings", json!({"effort": "loud"})),
                3 => Step::Call(
                    "settings",
                    json!({"network": true, "effort": "low", "max_usd": 5, "name": "Renamed"}),
                ),
                _ => {
                    // Mid-turn the change is pending, not applied: this request still
                    // has no network.
                    let (port, cookie) = address.0.lock().unwrap().clone().unwrap();
                    let list = exchange(port, &cookie, "GET", "/api/sessions", None).2;
                    let list: Value = serde_json::from_slice(&list).unwrap();
                    let id = list["sessions"][0]["id"].as_str().unwrap().to_string();
                    // The phone's picker during a run: queued like the tool's change.
                    let (status, _, body) = exchange(
                        port,
                        &cookie,
                        "POST",
                        "/api/commands",
                        Some(&json!({"request_id": "mid-settings", "session": id,
                            "kind": "settings", "toolchain": true})),
                    );
                    let posted = json!({"status": status,
                        "body": String::from_utf8_lossy(&body)});
                    let snapshot = exchange(
                        port,
                        &cookie,
                        "GET",
                        &format!("/api/sessions/{id}/snapshot"),
                        None,
                    )
                    .2;
                    let mut seen: Value = serde_json::from_slice(&snapshot).unwrap();
                    seen["request_tools"] = json!(request["tools"].to_string());
                    seen["posted"] = posted;
                    *mid_turn.lock().unwrap() = seen;
                    Step::Say("Changed.")
                }
            }
        })
    };
    let serve = Serve::start(&root, &source, fake_provider(script, Arc::default()));
    *address.0.lock().unwrap() = Some((serve.port, serve.cookie.clone()));

    // The model catalog for the picker.
    let models = serve.json("/api/models");
    let local = models["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "local")
        .unwrap_or_else(|| panic!("{models}"))
        .clone();
    assert_eq!(local["signed_in"], true);
    assert!(local["subscription"].is_boolean() && local["name"].is_string());
    let default_model = local["default_model"].as_str().unwrap().to_string();
    let model = local["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == default_model.as_str())
        .unwrap_or_else(|| panic!("{local}"));
    assert_eq!(model["label"], default_model.as_str());
    assert_eq!(model["default_effort"], "default");
    let efforts: Vec<&str> = model["efforts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        efforts.starts_with(&["default", "off", "low"]),
        "{efforts:?}"
    );
    assert!(model["image"].is_boolean());
    assert!(model["context"].is_u64() || model["context"].is_null());
    assert!(model["price"].is_null() || model["price"]["input"].is_number());
    for provider in models["providers"].as_array().unwrap() {
        for key in [
            "id",
            "name",
            "signed_in",
            "subscription",
            "models",
            "default_model",
        ] {
            assert!(provider.get(key).is_some(), "{key}: {provider}");
        }
    }
    let v1 = serve.json("/v1/models");
    assert_eq!(
        v1["providers"].as_array().unwrap().len(),
        models["providers"].as_array().unwrap().len()
    );

    let session = serve.new_thread("new-settings", "CHANGE_SETTINGS: use the network");
    let snapshot = serve.snapshot(&session);
    let results = tool_results(&snapshot);
    let read = results[0]["content"].as_str().unwrap();
    assert!(
        read.starts_with("Current settings: provider local")
            && read.contains("\n- local (signed in)")
            && read.contains("Efforts for local/"),
        "{read}"
    );
    let unknown = results[1]["content"].as_str().unwrap();
    assert!(
        results[1]["is_error"] == true
            && unknown.contains("unknown model `no-such-model` for local; its models: ")
            && unknown.contains(&default_model),
        "{unknown}"
    );
    let effort = results[2]["content"].as_str().unwrap();
    assert!(
        results[2]["is_error"] == true && effort.contains("choose one of: default, off, low"),
        "{effort}"
    );
    assert!(
        results[3]["content"]
            .as_str()
            .unwrap()
            .starts_with("Queued:")
    );
    // Mid-turn: pending, and the turn kept its settings.
    let seen = mid_turn.lock().unwrap().clone();
    assert!(
        seen["posted"]["status"].as_u64().is_some_and(|s| s < 300),
        "{}",
        seen["posted"]
    );
    assert_eq!(
        seen["pending_settings"],
        json!({"network": true, "effort": "low", "max_usd": 5, "name": "Renamed",
            "toolchain": true}),
        "{} / {}",
        seen["pending_settings"],
        seen["posted"]
    );
    assert_eq!(seen["settings"]["network"], false);
    assert!(
        seen["request_tools"]
            .as_str()
            .unwrap()
            .contains("Do not use the network")
    );
    // Applied when the turn ended.
    assert!(
        snapshot["pending_settings"].is_null(),
        "{}",
        snapshot["pending_settings"]
    );
    assert_eq!(
        snapshot["settings"],
        json!({"provider": "local", "model": snapshot["settings"]["model"], "effort": "low",
            "network": true, "toolchain": true, "compaction": "model", "max_usd": 5.0})
    );
    let listed = serve.json("/api/sessions");
    assert_eq!(listed["sessions"][0]["name"], "Renamed");
    let events = serve.events(&session);
    assert!(
        events.iter().any(|e| e["type"] == "notice"
            && e["text"]
                .as_str()
                .is_some_and(|t| t.starts_with("Settings changed at the end of the turn"))),
        "{events:?}"
    );
    let session_object = serve.json(&format!("/v1/sessions/{session}"));
    assert!(session_object["pending_settings"].is_null() && session_object["self_edit"].is_null());
    // An idle thread's settings command applies at once, toolchain included.
    let head = serve.snapshot(&session)["head"].clone();
    assert_eq!(
        serve.run(json!({"request_id": "idle-settings", "session": session,
            "kind": "settings", "toolchain": false, "network": false,
            "expected_revision": head})),
        ("completed".to_string(), String::new())
    );
    let now = serve.snapshot(&session)["settings"].clone();
    assert_eq!(
        (&now["toolchain"], &now["network"]),
        (&json!(false), &json!(false))
    );
    drop(serve);
    clean(&root);
}
