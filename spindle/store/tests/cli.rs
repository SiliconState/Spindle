//! The `spindle` command line as a caller (person or agent) uses it, against a scripted
//! loopback provider (no paid calls): `--json` results with the way on, what the model
//! is told (the workspace it starts with, exit codes and stderr, searching retained
//! output, a job's server), reading a session another process runs, `diff` as a patch,
//! and a provider no connection reaches (nothing sent, nothing uncertain).
#![cfg(target_os = "linux")]
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

/// What the scripted model answers, request by request: tool calls, or a final text.
enum Step {
    Calls(Vec<(&'static str, Value)>),
    Say(&'static str),
}

/// The provider's script and every request body it was sent.
#[derive(Clone, Default)]
struct Provider {
    steps: Arc<Mutex<VecDeque<Step>>>,
    seen: Arc<Mutex<Vec<Value>>>,
}

impl Provider {
    /// Serves `steps` in order on a loopback port.
    fn start(steps: Vec<Step>) -> (Self, u16) {
        let provider = Self {
            steps: Arc::new(Mutex::new(steps.into())),
            seen: Arc::default(),
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let serving = provider.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let provider = serving.clone();
                std::thread::spawn(move || provider.answer(stream));
            }
        });
        (provider, port)
    }

    fn answer(&self, mut stream: TcpStream) {
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
        let count = request["messages"].as_array().map_or(0, Vec::len);
        self.seen.lock().unwrap().push(request.clone());
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Step::Say("(no more steps)"));
        let chunk = |delta: Value, finish: Value| {
            json!({"id": "x", "object": "chat.completion.chunk", "model": request["model"],
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
        };
        let chunks = match step {
            Step::Calls(calls) => {
                let calls: Vec<Value> = calls
                    .into_iter()
                    .enumerate()
                    .map(|(index, (name, arguments))| {
                        json!({"index": index, "id": format!("call_{count}_{index}"), "type": "function",
                            "function": {"name": name, "arguments": arguments.to_string()}})
                    })
                    .collect();
                vec![
                    chunk(
                        json!({"role": "assistant", "tool_calls": calls}),
                        Value::Null,
                    ),
                    chunk(json!({}), json!("tool_calls")),
                ]
            }
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

    /// The text of tool result `index` (in call order) in request `request`.
    fn tool_result(&self, request: usize, index: usize) -> String {
        let seen = self.seen.lock().unwrap();
        let results: Vec<String> = seen[request]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(|message| message["content"].as_str().unwrap_or_default().to_string())
            .collect();
        let before: usize = seen[..request]
            .iter()
            .map(|request| {
                request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|message| message["role"] == "tool")
                    .count()
            })
            .max()
            .unwrap_or(0);
        results[before + index].clone()
    }
}

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("spindle-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("home")).unwrap();
    root
}

/// `spindle` with a fake home and every provider request sent to `port`.
fn spindle(root: &Path, port: u16, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spindle"))
        .current_dir(root)
        .args(args)
        .env("HOME", root.join("home"))
        .env("DEXT_HOME", root.join("home/.dext"))
        .env("DEXT_BASE_URL", format!("http://127.0.0.1:{port}"))
        .env_remove("SPINDLE_DIR")
        .env_remove("OPENAI_BASE_URL")
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("SPINDLE_CODE_POLICY")
        .output()
        .unwrap()
}

fn json_of(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "not JSON:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn a_delegated_turn_is_driven_and_read_back_through_the_command_line() {
    let root = scratch("turn");
    let project = root.join("project");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(project.join("src/app.py"), "print('hello')\n").unwrap();
    std::fs::write(project.join("README.md"), "# app\n").unwrap();
    let noisy =
        "seq 1 3000 | sed 's/$/ compiling/'; echo 'error[E0308]: mismatched types'; seq 1 50";
    let (provider, port) = Provider::start(vec![
        Step::Calls(vec![
            ("bash", json!({"command": "echo out; echo err >&2; exit 3"})),
            (
                "bash",
                json!({"command": "echo started; sleep 5", "timeout": 1}),
            ),
            ("bash", json!({"command": noisy})),
            (
                "edit_file",
                json!({"path": "src/app.py", "old_string": "hello", "new_string": "hello, world"}),
            ),
            ("bash", json!({"command": "whoami; ls src"})),
        ]),
        Step::Say("Edited src/app.py."),
    ]);
    let project_arg = project.to_string_lossy().into_owned();
    let output = spindle(
        &root,
        port,
        &[
            "new",
            "--provider",
            "local",
            "--import",
            &project_arg,
            "--json",
            "--dir",
            "s1",
            "Greet the world.",
        ],
    );
    let result = json_of(&output);
    assert!(output.status.success(), "{result}");
    assert_eq!(result["complete"], true, "{result}");
    assert_eq!(result["answer"], "Edited src/app.py.");
    assert_eq!(result["next"], "say");
    // The narration went to stderr, the result alone to stdout.
    assert!(String::from_utf8_lossy(&output.stderr).contains("> Greet the world."));

    let first = provider.seen.lock().unwrap()[0].clone();
    let tools: Vec<&str> = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert!(!tools.contains(&"spindle_source") && !tools.contains(&"spindle_install"));
    let prompt = first["messages"][1]["content"].as_str().unwrap();
    assert!(
        prompt.starts_with("Greet the world.\n\n[The workspace this thread starts with: 2 file(s)"),
        "{prompt}"
    );
    assert!(
        prompt.contains("\nREADME.md  6 bytes\nsrc/app.py  15 bytes"),
        "{prompt}"
    );
    let system = first["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.contains(" Commands have bash with the usual Unix tools"),
        "{system}"
    );
    assert!(!system.contains("  Commands have"), "{system}");

    // What the model was told about each call.
    let failed = provider.tool_result(1, 0);
    assert!(
        failed.contains("out\n[stderr]\nerr\n\n[exit status 3]"),
        "{failed}"
    );
    let timed_out = provider.tool_result(1, 1);
    assert!(
        timed_out.contains("ran past its 1 s timeout") && timed_out.contains("start_job"),
        "{timed_out}"
    );
    let noisy_result = provider.tool_result(1, 2);
    let artifact = noisy_result
        .split("artifact=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("retained output named")
        .to_string();
    let isolated = provider.tool_result(1, 4).starts_with("spindle\n");

    // Read back without the model: the final answer, status, a file at a revision,
    // and the turn's changes as a patch.
    let last = spindle(&root, port, &["--dir", "s1", "log", "--last"]);
    assert_eq!(
        String::from_utf8_lossy(&last.stdout),
        "Edited src/app.py.\n"
    );
    let status = json_of(&spindle(&root, port, &["status", "--dir", "s1", "--json"]));
    assert_eq!(status["next"], "say");
    assert_eq!(status["running"], false);
    assert_eq!(status["complete"], true);
    let before = spindle(
        &root,
        port,
        &["--dir", "s1", "cat", "--revision", "1", "src/app.py"],
    );
    assert_eq!(String::from_utf8_lossy(&before.stdout), "print('hello')\n");
    let patch = spindle(&root, port, &["--dir", "s1", "diff"]);
    let patch = String::from_utf8_lossy(&patch.stdout).into_owned();
    assert!(
        patch.contains("--- a/src/app.py\n+++ b/src/app.py\n@@ -1 +1 @@\n-print('hello')\n+print('hello, world')\n"),
        "{patch}"
    );
    let stat = json_of(&spindle(&root, port, &["--dir", "s1", "diff", "--json"]));
    assert_eq!(stat["files"][0]["path"], "src/app.py");
    assert_eq!(stat["files"][0]["added"], 1);

    // Searching and tailing the retained output, as a later turn's model does.
    let (provider, port) = Provider::start(vec![
        Step::Calls(vec![
            (
                "read_output",
                json!({"artifact": artifact, "find": "error["}),
            ),
            ("read_output", json!({"artifact": artifact, "tail": 12})),
            ("read_output", json!({"artifact": &artifact[..8]})),
        ]),
        Step::Say("Found it."),
    ]);
    let again = spindle(&root, port, &["--dir", "s1", "say", "Where did it fail?"]);
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    let found = provider.tool_result(1, 0);
    assert!(
        found.contains(": error[E0308]: mismatched types\n[1 of 1 line(s) holding `error[`"),
        "{found}"
    );
    let at: usize = found
        .trim_start_matches('@')
        .split(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(at > 30_000);
    assert!(provider.tool_result(1, 1).contains("49\n50\n"));
    assert!(provider.tool_result(1, 2).contains("whole 64-hex digest"));
    if isolated {
        // Sandboxed: no bytecode written into the workspace by Python.
        let files = json_of(&spindle(&root, port, &["--dir", "s1", "files", "--json"]));
        assert!(
            files["files"]
                .as_array()
                .unwrap()
                .iter()
                .all(|file| !file["path"].as_str().unwrap().contains("__pycache__"))
        );
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_provider_no_connection_reaches_is_told_and_nothing_counts_as_uncertain() {
    let root = scratch("unreached");
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let output = spindle(
        &root,
        closed,
        &["--dir", "s1", "new", "--provider", "local", "hello"],
    );
    assert!(!output.status.success());
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("could not connect to the provider") && said.contains("Nothing was sent"),
        "{said}"
    );
    let status = json_of(&spindle(
        &root,
        closed,
        &["--dir", "s1", "status", "--json"],
    ));
    assert_eq!(status["usage"]["uncertain_calls"], 0, "{status}");
    assert_eq!(status["usage"]["refused_calls"], 1, "{status}");
    assert_eq!(status["unresolved"], 0, "{status}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn command_lines_spindle_cannot_take_change_nothing() {
    let root = scratch("args");
    let (provider, port) = Provider::start(vec![]);
    let run = |args: &[&str]| spindle(&root, port, args);
    // Help anywhere is help: nothing is created or sent.
    let help = run(&["--dir", "s1", "new", "--provider", "local", "--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("new --provider ID"));
    assert!(!root.join("s1").exists());
    for (args, says) in [
        (
            &[
                "--dir",
                "s1",
                "new",
                "--provider",
                "local",
                "--modle",
                "x",
                "hi",
            ][..],
            "unknown option `--modle`",
        ),
        (
            &["--dir", "s1", "status", "--model", "x"][..],
            "`--model` does not apply to `status`",
        ),
        (&["--dir", "s1", "status", "now"][..], "unexpected `now`"),
        (
            &["--dir", "s1", "doctor", "--json"][..],
            "--json is not offered for `doctor`",
        ),
        (
            &["--dir", "s1", "new", "--provider", "local"][..],
            "a PROMPT is required",
        ),
    ] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(says),
            "{args:?}"
        );
    }
    assert!(provider.seen.lock().unwrap().is_empty());
    // Steering or stopping a session nothing runs says so.
    let (provider, port) = Provider::start(vec![Step::Say("hi")]);
    assert!(
        spindle(
            &root,
            port,
            &["--dir", "s1", "new", "--provider", "local", "hi"]
        )
        .status
        .success()
    );
    let steer = spindle(&root, port, &["--dir", "s1", "steer", "also this"]);
    assert_eq!(steer.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&steer.stderr).contains("no turn is running"));
    // A prompt from standard input.
    let mut child = Command::new(env!("CARGO_BIN_EXE_spindle"))
        .current_dir(&root)
        .args(["--dir", "s1", "say", "-"])
        .env("HOME", root.join("home"))
        .env("DEXT_HOME", root.join("home/.dext"))
        .env("DEXT_BASE_URL", format!("http://127.0.0.1:{port}"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"a prompt\nover two lines")
        .unwrap();
    assert!(child.wait().unwrap().success());
    let seen = provider.seen.lock().unwrap();
    let last = seen.last().unwrap()["messages"].as_array().unwrap().clone();
    assert!(
        last.iter()
            .any(|message| message["content"] == "a prompt\nover two lines")
    );
    drop(seen);
    let _ = std::fs::remove_dir_all(root);
}
