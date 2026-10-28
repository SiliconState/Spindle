//! Session packs across whole turns: idle and `continue` follow-ups, steering as a
//! queued update, rewind consistency and a killed host. A loopback fake provider
//! (OpenAI chat completions) stands in for the model; nothing paid is called.
#![cfg(all(feature = "agent", target_os = "linux"))]
use serde_json::{Value, json};
use spindle_store::agent::{Event, Isolation, Observer, Session, Settings};
use spindle_store::pack_runtime::{PackUi, UI_METHODS, UiRequest, UiResponse};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const FIXTURE: &str = include_str!("fixtures/pack_fixture.py");
const CHILD: &str = "SPINDLE_PACK_CHILD";
static SERIAL: AtomicU64 = AtomicU64::new(0);

/// The process-wide test home: Dext catalog with the fake provider, the fixture pack
/// and the policy granting it. A child process inherits it through the environment.
struct Home {
    root: PathBuf,
    log: PathBuf,
}

fn home() -> &'static Home {
    static HOME: OnceLock<Home> = OnceLock::new();
    HOME.get_or_init(|| {
        if let Ok(root) = std::env::var("SPINDLE_PACK_TEST_HOME") {
            let root = PathBuf::from(root);
            return Home {
                log: root.join("counter.log"),
                root,
            };
        }
        // Homes of earlier runs whose process is gone are removed (statics never drop).
        for entry in std::fs::read_dir(std::env::temp_dir()).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(pid) = name.strip_prefix("spindle-pack-session-")
                && !Path::new("/proc").join(pid).exists()
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        let root = std::env::temp_dir().join(format!("spindle-pack-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("dext")).unwrap();
        let port = fake_provider();
        let catalog = json!({"version":1,"active_provider":"fakepack","providers":[{
            "id":"fakepack","display_name":"Fake","api_provider":"openai","request_contract":"openai-chat-completions",
            "base_url":format!("http://127.0.0.1:{port}"),"default_model":"fake-model","models":["fake-model"],
            "model_aliases":{},"env_vars":["SPINDLE_FAKE_PACK_KEY"],"requires_api_key":true,
            "model_defaults":{"context_window":200000,"max_output_tokens":4096,"effort_levels":[],"reasoning_modes":[],
                "capabilities":{"tools":true,"reasoning":false,"image_input":false,"prompt_cache":false},"pricing":null},
            "model_specs":{}}]});
        std::fs::write(root.join("dext/providers.json"), catalog.to_string()).unwrap();
        let pack = root.join("packs/counter");
        std::fs::create_dir_all(pack.join("bin")).unwrap();
        std::fs::write(pack.join("bin/run"), FIXTURE).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(pack.join("bin/run"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(pack.join("PACK.md"), "# counter\n").unwrap();
        let log = root.join("counter.log");
        let schema = json!({"type":"object","properties":{
            "by":{"type":"integer"},"steer":{"type":"string"},"continue":{"type":"string"},"delay_ms":{"type":"integer"},
            "view":{"type":"string"},"promote":{"type":"string"},"ask":{"type":"string"},"idle_continue":{"type":"boolean"},
            "sleep":{"type":"number"}},"additionalProperties":false});
        let manifest = json!({"version":1,"command":"bin/run","args":[log],"timeout_seconds":5,"ui_protocol":1,"tool_protocol":1,
            "max_continuations":20,"tools":[{"name":"bump","description":"Bump the counter","risk":"write","input_schema":schema}]});
        std::fs::write(pack.join("runtime.json"), manifest.to_string()).unwrap();
        let candidate = spindle_store::code_host::pack_candidate(&pack.join("runtime.json")).unwrap();
        let tools: Vec<Value> = candidate["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| {
                let mut tool = tool.clone();
                tool["lifecycle"] = json!("session");
                tool
            })
            .collect();
        std::fs::write(
            root.join("policy.json"),
            json!({"version":1,"models":[],"tools":tools}).to_string(),
        )
        .unwrap();
        // SAFETY: set once, before any session or child starts; every test reads them
        // only after this initializer returns.
        unsafe {
            std::env::set_var("DEXT_HOME", root.join("dext"));
            std::env::set_var("SPINDLE_FAKE_PACK_KEY", "fake-key");
            std::env::set_var("SPINDLE_CODE_POLICY", root.join("policy.json"));
            std::env::set_var("SPINDLE_PACK_TEST_HOME", &root);
        }
        Home { root, log }
    })
}

fn sse(chunks: &[Value]) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn text_of(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The model: a user message holding ```python blocks calls run_code with each; any
/// other user message is answered "seen: <text>", a tool result "result: <content>".
fn reply(body: &Value, calls: &AtomicUsize) -> String {
    let last = body["messages"]
        .as_array()
        .and_then(|m| m.last())
        .cloned()
        .unwrap_or_default();
    let text = text_of(&last);
    let usage = json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3}});
    if last["role"] == "user" && text.contains("```python\n") {
        // One run_code call per block, in order, in one response.
        let mut tool_calls = vec![];
        let mut rest = text.as_str();
        while let Some(start) = rest.find("```python\n") {
            let code = &rest[start + 10..];
            let end = code.find("```").unwrap_or(code.len());
            let id = format!("call_{}", calls.fetch_add(1, Ordering::SeqCst));
            tool_calls.push(json!({"index":tool_calls.len(),"id":id,"type":"function","function":{"name":"run_code","arguments":json!({"language":"python","code":&code[..end]}).to_string()}}));
            rest = &code[(end + 3).min(code.len())..];
        }
        return sse(&[
            json!({"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":tool_calls}}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
            usage,
        ]);
    }
    let answer = match last["role"].as_str() {
        Some("tool") => format!("result: {}", text.chars().take(600).collect::<String>()),
        _ => format!("seen: {}", text.chars().take(600).collect::<String>()),
    };
    sse(&[
        json!({"choices":[{"index":0,"delta":{"role":"assistant","content":answer}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        usage,
    ])
}

fn fake_provider() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let calls = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let calls = calls.clone();
            std::thread::spawn(move || {
                let mut request = vec![];
                let mut chunk = [0; 65536];
                let (start, length) = loop {
                    let Ok(n) = stream.read(&mut chunk) else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (end + 4, length);
                    }
                };
                while request.len() < start + length {
                    let Ok(n) = stream.read(&mut chunk) else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..n]);
                }
                let body: Value =
                    serde_json::from_slice(&request[start..start + length]).unwrap_or_default();
                let answer = reply(&body, &calls);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                    answer.len()
                );
            });
        }
    });
    port
}

#[derive(Default)]
struct Watch {
    prompts: Mutex<Vec<String>>,
    steered: Mutex<Vec<String>>,
    views: Mutex<Vec<String>>,
    notes: Mutex<Vec<String>>,
    ui: Option<Arc<dyn PackUi>>,
}
impl Observer for Watch {
    fn event(&self, event: Event<'_>) {
        match event {
            Event::Prompt { text } => self.prompts.lock().unwrap().push(text.into()),
            Event::Steered { text } => self.steered.lock().unwrap().push(text.into()),
            Event::PackView { title, .. } => self.views.lock().unwrap().push(title.into()),
            Event::Nested { label } => self.notes.lock().unwrap().push(label.into()),
            _ => {}
        }
    }
    fn pack_ui(&self) -> Option<Arc<dyn PackUi>> {
        self.ui.clone()
    }
}

/// Ends this process's session the way a crash would: a panic at the `nth` event
/// `at` matches, when everything before it is durable and nothing after it ran.
struct CrashAt {
    watch: Arc<Watch>,
    at: fn(&Event<'_>) -> bool,
    nth: usize,
    seen: AtomicUsize,
}
impl CrashAt {
    fn new(watch: Arc<Watch>, at: fn(&Event<'_>) -> bool, nth: usize) -> Arc<Self> {
        Arc::new(Self {
            watch,
            at,
            nth,
            seen: AtomicUsize::new(0),
        })
    }
}
impl Observer for CrashAt {
    fn event(&self, event: Event<'_>) {
        if (self.at)(&event) && self.seen.fetch_add(1, Ordering::SeqCst) + 1 == self.nth {
            panic!("simulated crash");
        }
        self.watch.event(event);
    }
    fn pack_ui(&self) -> Option<Arc<dyn PackUi>> {
        self.watch.pack_ui()
    }
}

/// Runs `prompt` until the simulated crash; the session is gone afterwards.
fn crash(mut session: Session, prompt: &str) {
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.prompt(prompt)));
    assert!(
        crashed.is_err(),
        "the session never reached the crash point"
    );
    drop(session);
}

struct Answer;
impl PackUi for Answer {
    fn methods(&self) -> Vec<String> {
        UI_METHODS.iter().map(|m| m.to_string()).collect()
    }
    fn ask(&self, _: &str, _: &UiRequest, _: &AtomicBool, _: Instant) -> UiResponse {
        UiResponse::Ok(json!({"name":"Grace"}))
    }
}

fn session_dir(label: &str) -> PathBuf {
    home().root.join(format!(
        "{label}-{}",
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ))
}

fn create(dir: &Path, watch: Arc<dyn Observer>) -> Session {
    let mut settings = Settings::resolve("fakepack", Some("fake-model".into())).unwrap();
    settings.isolation = Isolation::Off;
    Session::create(dir, settings, vec![], watch).unwrap()
}

fn script(code: &str) -> String {
    format!("run this\n```python\n{code}\n```")
}

/// Every message text of the head's history, in order.
fn texts(session: &mut Session) -> Vec<String> {
    session
        .history()
        .unwrap()
        .iter()
        .flat_map(|m| {
            m.content.iter().filter_map(|b| match b {
                dext_transcript::Block::Text { text } => Some(format!("{}: {text}", m.role)),
                dext_transcript::Block::ToolResult { content, .. } => {
                    Some(format!("tool: {content}"))
                }
                _ => None,
            })
        })
        .collect()
}

fn pack_state(dir: &Path) -> Value {
    let mut store = spindle_store::Store::open(&dir.join("store")).unwrap();
    store.pack_states("operator", None).unwrap()["counter"]
        .state
        .clone()
}

fn log_lines() -> Vec<String> {
    std::fs::read_to_string(&home().log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// `event session_id turn_id` for each pack invocation.
fn id_lines() -> Vec<Vec<String>> {
    std::fs::read_to_string(home().log.with_extension("log.ids"))
        .unwrap_or_default()
        .lines()
        .map(|line| line.split(' ').map(str::to_string).collect())
        .collect()
}

/// What the phone's snapshot says: Continue has pack work for the completed head.
fn follow_up_pending(dir: &Path) -> bool {
    let mut view = spindle_store::view::View::open(&dir.join("store")).unwrap();
    view.read(|view| {
        let (_, continuation, _) = view.head()?;
        let state = dext_transcript::turn::TurnContinuation::restore(&continuation)
            .map_err(|_| spindle_store::Error::Incompatible)?;
        view.pack_follow_up(state.execution())
    })
    .unwrap()
}

fn continued(session: &mut Session, prompt: &str) -> usize {
    texts(session)
        .iter()
        .filter(|t| t.starts_with(&format!("user: [pack-runtime continue] counter\n{prompt}")))
        .count()
}

/// Packs share one log, so tests that count its lines run one at a time.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn steer_reaches_the_running_turn_and_continue_starts_one_follow_up_after_idle() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let _serial = serial();
    let dir = session_dir("follow");
    let watch = Arc::new(Watch {
        ui: Some(Arc::new(Answer)),
        ..Watch::default()
    });
    let mut session = create(&dir, watch.clone());
    let before = log_lines().len();
    session
        .prompt(&script(
            "print(tool('counter/bump', {'steer':'note the count','continue':'please summarize','delay_ms':40,'view':'# one'}))",
        ))
        .unwrap();
    let history = texts(&mut session);
    // Steering arrived as a queued update inside the turn, before its final answer.
    assert_eq!(watch.steered.lock().unwrap().len(), 1);
    assert!(
        watch.steered.lock().unwrap()[0].contains("[pack-runtime steer] counter\nnote the count")
    );
    let update = history
        .iter()
        .position(|t| {
            t.starts_with("user: [queued-user-update]")
                && t.ends_with("[pack-runtime steer] counter\nnote the count")
        })
        .expect("the steer is a queued update");
    assert!(
        history[update + 1].starts_with("assistant: seen: [queued-user-update]"),
        "{history:#?}"
    );
    // One follow-up turn after the turn ended, carrying the continuation.
    let follow: Vec<&String> = history
        .iter()
        .filter(|t| t.starts_with("user: [pack-runtime continue] counter\nplease summarize"))
        .collect();
    assert_eq!(follow.len(), 1, "{history:#?}");
    assert_eq!(watch.prompts.lock().unwrap().len(), 2);
    assert_eq!(*watch.views.lock().unwrap(), ["Counter"]);
    let calls: Vec<String> = log_lines()[before..].to_vec();
    assert_eq!(
        calls
            .iter()
            .map(|l| l.split(' ').next().unwrap())
            .collect::<Vec<_>>(),
        ["activate", "tool", "idle", "idle"],
        "{calls:?}"
    );
    // Nothing more to deliver: continue finds no follow-up, idle does not run again.
    session.resume().unwrap();
    assert_eq!(watch.prompts.lock().unwrap().len(), 2);
    drop(session);
    let state = pack_state(&dir);
    assert_eq!(
        (state["count"].clone(), state["idles"].clone()),
        (json!(1), json!(2))
    );
    // The phone's snapshot sees the latest view.
    let mut view = spindle_store::view::View::open(&dir.join("store")).unwrap();
    let packs = view.read(|v| v.packs()).unwrap();
    assert_eq!(packs[0]["view"]["markdown"], "# one");
}

#[test]
fn rewinding_restores_the_pack_state_and_promoted_tools_of_the_target() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let _serial = serial();
    let dir = session_dir("rewind");
    let mut session = create(&dir, Arc::new(Watch::default()));
    session
        .prompt(&script("print(tool('counter/bump', {'by':1}))"))
        .unwrap();
    let first = session.status().unwrap().revision;
    session
        .prompt(&script(
            "print(tool('counter/bump', {'by':10,'promote':'lookup'}))\nprint(tool('counter/lookup', {'key':'k'}))",
        ))
        .unwrap();
    let second = session.status().unwrap().revision;
    assert!(
        texts(&mut session)
            .iter()
            .any(|t| t.contains("lookup k count=11")),
        "{:#?}",
        texts(&mut session)
    );
    session.rewind(first).unwrap();
    session
        .prompt(&script(
            "try:\n    tool('counter/lookup', {'key':'k'})\nexcept RuntimeError as error:\n    print('no lookup:', error)\nprint(tool('counter/bump', {'by':1}))",
        ))
        .unwrap();
    let history = texts(&mut session);
    let result = history
        .iter()
        .rev()
        .find(|t| t.starts_with("tool:"))
        .unwrap();
    assert!(
        result.contains("no lookup:") && result.contains("count=2"),
        "{result}"
    );
    drop(session);
    assert_eq!(pack_state(&dir)["count"], 2);
    // Back to the abandoned branch's end: its state comes back with it.
    let mut session = Session::open(&dir, Arc::new(Watch::default())).unwrap();
    assert!(
        session
            .targets()
            .unwrap()
            .iter()
            .any(|t| t.revision == second)
    );
    session.rewind(second).unwrap();
    // The branch's turn ended where it came from: nothing to continue, no idle again.
    assert!(!follow_up_pending(&dir));
    let idles = log_lines().iter().filter(|l| l.starts_with("idle")).count();
    session.resume().unwrap();
    assert_eq!(
        log_lines().iter().filter(|l| l.starts_with("idle")).count(),
        idles
    );
    drop(session);
    assert_eq!(pack_state(&dir)["count"], 11);
}

/// Run inside a child process that the test kills: one pack call finishes, the next
/// is killed mid-call (phase "call"), or the host dies while a follow-up waits
/// (phase "follow").
#[test]
fn pack_host_child() {
    let Some(dir) = std::env::var_os(CHILD).map(PathBuf::from) else {
        return;
    };
    let watch = Arc::new(Watch::default());
    match std::env::var("SPINDLE_PACK_CHILD_PHASE").as_deref() {
        Ok("call") => {
            let mut session = create(&dir, watch);
            let _ = session.prompt(&script(
                "print(tool('counter/bump', {}))\nprint(tool('counter/bump', {'sleep':4}))",
            ));
        }
        _ => {
            let mut session = Session::open(&dir, watch).unwrap();
            let _ = session.prompt(&script(
                "print(tool('counter/bump', {'continue':'after the restart','delay_ms':4000}))",
            ));
        }
    }
    std::process::exit(3);
}

fn child(dir: &Path, phase: &str, until: impl Fn() -> bool, settle: Duration) {
    let mut process = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "pack_host_child",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, dir)
        .env("SPINDLE_PACK_CHILD_PHASE", phase)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !until() {
        assert!(Instant::now() < deadline, "the child never reached the cut");
        assert!(
            process.try_wait().unwrap().is_none(),
            "the child exited before the cut"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(settle);
    assert!(
        process.try_wait().unwrap().is_none(),
        "the child exited before the cut"
    );
    process.kill().unwrap();
    process.wait().unwrap();
}

#[test]
fn a_killed_host_keeps_pack_state_never_reruns_a_call_and_delivers_a_follow_up_once() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let _serial = serial();
    let dir = session_dir("killed");
    let before = log_lines().len();
    let bumps = move || {
        log_lines()[before..]
            .iter()
            .filter(|l| l.starts_with("tool bump"))
            .count()
    };
    // Killed while the second call runs: the first call's state is kept.
    child(&dir, "call", || bumps() == 2, Duration::ZERO);
    let watch = Arc::new(Watch::default());
    let mut session = Session::open(&dir, watch.clone()).unwrap();
    session.resume().unwrap();
    assert_eq!(bumps(), 2, "the interrupted pack call was not run again");
    drop(session);
    assert_eq!(pack_state(&dir)["count"], 1);
    // Killed while the decided follow-up waits: delivered exactly once after reopen.
    let idles = move || {
        log_lines()[before..]
            .iter()
            .filter(|l| l.starts_with("idle"))
            .count()
    };
    let seen = idles();
    // The follow-up is decided and recorded right after idle; then it waits 4 s.
    child(
        &dir,
        "follow",
        || idles() > seen,
        Duration::from_millis(800),
    );
    assert_eq!(bumps(), 3);
    assert!(
        follow_up_pending(&dir),
        "a recorded follow-up offers Continue"
    );
    let mut session = Session::open(&dir, watch.clone()).unwrap();
    let delivered = |session: &mut Session| {
        texts(session)
            .iter()
            .filter(|t| t.starts_with("user: [pack-runtime continue] counter\nafter the restart"))
            .count()
    };
    assert_eq!(delivered(&mut session), 0);
    session.resume().unwrap();
    assert_eq!(delivered(&mut session), 1);
    session.resume().unwrap();
    assert_eq!(delivered(&mut session), 1);
    drop(session);
    assert!(!follow_up_pending(&dir));
    assert_eq!(pack_state(&dir)["count"], 2);
}

#[test]
fn a_stop_during_the_follow_up_delay_cancels_it_for_good() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let _serial = serial();
    let dir = session_dir("cancel");
    let mut session = create(&dir, Arc::new(Watch::default()));
    let interrupt = session.interrupt_handle();
    let start = log_lines().len();
    let stopper = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if log_lines()[start..].iter().any(|l| l.starts_with("idle")) {
                std::thread::sleep(Duration::from_millis(300));
                interrupt.store(true, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let lines = log_lines().len();
    session
        .prompt(&script(
            "print(tool('counter/bump', {'continue':'never','delay_ms':10000}))",
        ))
        .unwrap();
    stopper.join().unwrap();
    assert!(log_lines()[lines..].iter().any(|l| l.starts_with("idle")));
    let none = |session: &mut Session| {
        !texts(session)
            .iter()
            .any(|t| t.starts_with("user: [pack-runtime continue]"))
    };
    assert!(none(&mut session));
    drop(session);
    assert!(
        !follow_up_pending(&dir),
        "a cancelled follow-up is not offered"
    );
    let mut session = Session::open(&dir, Arc::new(Watch::default())).unwrap();
    session.resume().unwrap();
    assert!(none(&mut session));
}

#[test]
fn pack_steers_of_one_batch_survive_a_crash_and_arrive_once_in_call_order() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let _serial = serial();
    let dir = session_dir("steer-crash");
    // Crash as the second run_code call starts: the first call's result is adopted,
    // its steer recorded only in the pack ledger.
    let at = |event: &Event<'_>| matches!(event, Event::ToolCall { .. });
    crash(
        create(&dir, CrashAt::new(Arc::default(), at, 2)),
        "two scripts\n```python\nprint(tool('counter/bump', {'steer':'first steer'}))\n```\n```python\nprint(tool('counter/bump', {'steer':'second steer'}))\n```",
    );
    let watch = Arc::new(Watch::default());
    let mut session = Session::open(&dir, watch.clone()).unwrap();
    session.resume().unwrap();
    let steered = watch.steered.lock().unwrap().clone();
    assert_eq!(steered.len(), 1, "{steered:?}");
    let first = steered[0]
        .find("first steer")
        .expect("the adopted call's steer");
    let second = steered[0]
        .find("second steer")
        .expect("the resumed call's steer");
    assert!(first < second, "call order: {steered:?}");
    let updates = |session: &mut Session| {
        texts(session)
            .iter()
            .filter(|t| t.starts_with("user: [queued-user-update]"))
            .count()
    };
    assert_eq!(updates(&mut session), 1);
    session.resume().unwrap();
    assert_eq!(updates(&mut session), 1);
    assert_eq!(watch.steered.lock().unwrap().len(), 1);
}

#[test]
fn a_crash_before_the_turn_end_record_still_runs_idle_once_and_delivers_the_follow_up() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let _serial = serial();
    let dir = session_dir("turn-end-crash");
    let before = log_lines().len();
    let idles = move || {
        log_lines()[before..]
            .iter()
            .filter(|l| l.starts_with("idle"))
            .count()
    };
    // The turn completes; the host dies before its pack turn-end ran.
    let at = |event: &Event<'_>| matches!(event, Event::Stopped { complete: true, .. });
    crash(
        create(&dir, CrashAt::new(Arc::default(), at, 1)),
        &script("print(tool('counter/bump', {'continue':'after the crash'}))"),
    );
    assert_eq!(idles(), 0);
    assert!(follow_up_pending(&dir), "the phone offers Continue");
    let mut session = Session::open(&dir, Arc::new(Watch::default())).unwrap();
    assert_eq!(continued(&mut session, "after the crash"), 0);
    session.resume().unwrap();
    assert_eq!(continued(&mut session, "after the crash"), 1);
    assert!(!follow_up_pending(&dir));
    // Idle ran for the crashed turn's end, then for the follow-up's.
    assert_eq!(idles(), 2);
    session.resume().unwrap();
    assert_eq!(continued(&mut session, "after the crash"), 1);
    assert_eq!(idles(), 2);
}

#[test]
fn a_revoked_or_unreadable_policy_drops_queued_continuations_with_a_note() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let _serial = serial();
    let policy = home().root.join("policy.json");
    struct Restore(PathBuf, Vec<u8>);
    impl Drop for Restore {
        fn drop(&mut self) {
            std::fs::write(&self.0, &self.1).unwrap();
        }
    }
    let _restore = Restore(policy.clone(), std::fs::read(&policy).unwrap());
    /// Replaces the operator's policy once the pack call has returned.
    struct Revoke(Arc<Watch>, PathBuf, &'static str);
    impl Observer for Revoke {
        fn event(&self, event: Event<'_>) {
            if matches!(event, Event::ToolResult { .. }) {
                std::fs::write(&self.1, self.2).unwrap();
            }
            self.0.event(event);
        }
    }
    for (label, replacement) in [
        ("revoked", r#"{"version":1,"models":[],"tools":[]}"#),
        ("unreadable", "{not json"),
    ] {
        std::fs::write(&policy, &_restore.1).unwrap();
        let dir = session_dir(label);
        let watch = Arc::new(Watch::default());
        let mut session = create(
            &dir,
            Arc::new(Revoke(watch.clone(), policy.clone(), replacement)),
        );
        session
            .prompt(&script(
                "print(tool('counter/bump', {'continue':'not for a revoked pack'}))",
            ))
            .unwrap();
        assert_eq!(
            continued(&mut session, "not for a revoked pack"),
            0,
            "{label}"
        );
        assert_eq!(watch.prompts.lock().unwrap().len(), 1, "{label}");
        let notes = watch.notes.lock().unwrap().join("\n");
        assert!(notes.contains("not delivered"), "{label}: {notes}");
        drop(session);
        let mut store = spindle_store::Store::open(&dir.join("store")).unwrap();
        assert!(
            store.pack_states("operator", None).unwrap()["counter"]
                .pending
                .is_empty(),
            "{label}"
        );
    }
}

#[test]
fn packs_see_one_session_id_per_session_and_one_turn_id_per_turn() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let _serial = serial();
    let dir = session_dir("ids");
    let before = id_lines().len();
    let mut session = create(&dir, Arc::new(Watch::default()));
    session
        .prompt(&script(
            "print(tool('counter/bump', {'continue':'next turn'}))",
        ))
        .unwrap();
    drop(session);
    let mut session = Session::open(&dir, Arc::new(Watch::default())).unwrap();
    session
        .prompt(&script("print(tool('counter/bump', {}))"))
        .unwrap();
    let lines = id_lines()[before..].to_vec();
    let events: Vec<&str> = lines.iter().map(|l| l[0].as_str()).collect();
    assert_eq!(
        events,
        ["activate", "tool", "idle", "idle", "tool", "idle"],
        "{lines:?}"
    );
    // One session id throughout, across a reopen.
    assert!(
        lines.iter().all(|l| l[1] == lines[0][1] && l[1] != "None"),
        "{lines:?}"
    );
    // The first turn's calls and its idle share its turn id; the follow-up and the
    // next prompt are turns of their own.
    let turns: Vec<&str> = lines.iter().map(|l| l[2].as_str()).collect();
    assert_eq!((turns[0], turns[1]), (turns[2], turns[2]), "{lines:?}");
    assert_eq!(turns[4], turns[5], "{lines:?}");
    assert!(
        turns[2] != turns[3] && turns[3] != turns[4] && turns[2] != turns[4],
        "{lines:?}"
    );
}
