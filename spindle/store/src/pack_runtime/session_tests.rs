//! Session packs, HTTP grants and MCP mutations through the real `CodeHost` and the
//! store's child journal (`code_request`), with local helpers and loopback servers.
use super::*;
use crate::agent::{Event as AgentEvent, Observer};
use crate::code_host::CodeHost;
use crate::completion::Attempt;
use crate::transition::EffectIntent;
use crate::{Command, Store};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;

static SERIAL: AtomicU64 = AtomicU64::new(0);
const FIXTURE: &str = include_str!("../../tests/fixtures/pack_fixture.py");

struct Dir(PathBuf);
impl Dir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "spindle-pack-{label}-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The fixture pack under `root/counter` and the policy an operator would make from its
/// candidate export (each entry's lifecycle set to the suggested one).
fn counter_pack(root: &Path) -> (PathBuf, Value) {
    let pack = root.join("counter");
    executable(&pack.join("bin/run"), FIXTURE);
    std::fs::write(pack.join("PACK.md"), "# counter\n").unwrap();
    let log = root.join("counter.log");
    let schema = |properties: Value| json!({"type":"object","properties":properties,"additionalProperties":false});
    let manifest = json!({
        "version":1,"command":"bin/run","args":[log],"timeout_seconds":3,
        "ui_protocol":1,"tool_protocol":1,"max_continuations":5,
        "tools":[
            {"name":"bump","description":"Bump the counter","risk":"write","input_schema":schema(json!({
                "by":{"type":"integer"},"steer":{"type":"string"},"continue":{"type":"string"},
                "delay_ms":{"type":"integer"},"view":{"type":"string"},"promote":{"type":"string"},
                "ask":{"type":"string"},"idle_continue":{"type":"boolean"},"sleep":{"type":"number"}}))},
            {"name":"fail","description":"Report an error","risk":"write","input_schema":schema(json!({"message":{"type":"string"}}))},
            {"name":"hang","description":"Never answer","risk":"write","input_schema":schema(json!({}))}
        ]
    });
    std::fs::write(
        pack.join("runtime.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let candidate = crate::code_host::pack_candidate(&pack.join("runtime.json")).unwrap();
    let tools: Vec<Value> = candidate["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| {
            assert_eq!(tool["suggested_lifecycle"], "session");
            let mut tool = tool.clone();
            tool["lifecycle"] = tool["suggested_lifecycle"].clone();
            tool
        })
        .collect();
    (
        log,
        json!({"version":1,"models":[],"tools":tools,"limits":{"calls":64}}),
    )
}

fn fixture(root: &Path) -> (Store, DispatchRequest) {
    let mut store = Store::open(&root.join("store")).unwrap();
    store
        .admit(&Command {
            scope: "operator".into(),
            id: "fixture".into(),
            lineage: 0,
            kind: "fixture".into(),
            payload: vec![],
        })
        .unwrap();
    let parent = parent(&store, "parent");
    (store, parent)
}

fn parent(store: &Store, attempt: &str) -> DispatchRequest {
    DispatchRequest {
        scope: "operator".into(),
        owner_epoch: store.epoch(),
        expected_revision: 0,
        intent_revision: 0,
        intent: EffectIntent {
            id: "effect".into(),
            adapter: "run_code".into(),
            input: vec![],
        },
        attempt: Attempt {
            id: attempt.into(),
            effect: "effect".into(),
            owner_epoch: store.epoch(),
            implementation: "fixture".into(),
            grant_reference: "fixture".into(),
        },
    }
}

#[derive(Default)]
struct Watcher {
    views: Mutex<Vec<String>>,
    ui: Option<Arc<dyn PackUi>>,
}
impl Observer for Watcher {
    fn event(&self, event: AgentEvent<'_>) {
        if let AgentEvent::PackView { pack, title, .. } = event {
            self.views.lock().unwrap().push(format!("{pack}:{title}"));
        }
    }
    fn pack_ui(&self) -> Option<Arc<dyn PackUi>> {
        self.ui.clone()
    }
}

struct Answering(UiResponse, Mutex<Vec<String>>);
impl PackUi for Answering {
    fn methods(&self) -> Vec<String> {
        UI_METHODS.iter().map(|m| m.to_string()).collect()
    }
    fn ask(&self, pack: &str, request: &UiRequest, _: &AtomicBool, _: Instant) -> UiResponse {
        self.1
            .lock()
            .unwrap()
            .push(format!("{pack}:{}:{}", request.id, request.method));
        self.0.clone()
    }
}

fn host(policy: &Value, root: &Path, observer: Option<Arc<dyn Observer>>) -> CodeHost {
    CodeHost::with_policy(policy.clone(), "fake", "fake", "fixture", root, observer).unwrap()
}

fn call(
    store: &mut Store,
    parent: &DispatchRequest,
    host: &mut CodeHost,
    id: u64,
    method: &str,
    input: Value,
) -> Value {
    let bytes = serde_json::to_vec(&json!({"id":id,"method":method,"input":input})).unwrap();
    serde_json::from_slice(&store.code_request(
        parent,
        &bytes,
        host,
        &AtomicBool::new(false),
        Duration::from_secs(30),
    ))
    .unwrap()
}

fn tool(name: &str, arguments: Value) -> Value {
    json!({"name":name,"arguments":arguments})
}

fn log(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn session_pack_state_is_host_held_journaled_and_survives_reopen() {
    let dir = Dir::new("state");
    let (log_path, policy) = counter_pack(&dir.0);
    let (mut store, parent) = fixture(&dir.0);
    let mut code = host(&policy, &dir.0, None);
    let first = call(
        &mut store,
        &parent,
        &mut code,
        0,
        "tool",
        tool("counter/bump", json!({"by":1})),
    );
    assert_eq!(first["ok"], true, "{first}");
    assert_eq!(
        first["data"]["content"],
        "[pack runtime activation]\nactivated\n\ncount=1"
    );
    // The script sees content only, never the state.
    assert!(first["data"].get("state").is_none());
    let second = call(
        &mut store,
        &parent,
        &mut code,
        1,
        "tool",
        tool("counter/bump", json!({"by":2})),
    );
    assert_eq!(second["data"]["content"], "count=3", "{second}");
    // A repeated child identity reuses its result; nothing runs again.
    assert_eq!(
        first,
        call(
            &mut store,
            &parent,
            &mut code,
            0,
            "tool",
            tool("counter/bump", json!({"by":1}))
        )
    );
    assert_eq!(
        log(&log_path),
        ["activate - -", "tool bump -", "tool bump -"]
    );
    drop(store);
    let mut store = Store::open(&dir.0.join("store")).unwrap();
    let parent = DispatchRequest {
        owner_epoch: store.epoch(),
        ..parent
    };
    let third = call(
        &mut store,
        &parent,
        &mut code,
        2,
        "tool",
        tool("counter/bump", json!({"by":1})),
    );
    assert_eq!(third["data"]["content"], "count=4", "{third}");
    let states = store.pack_states("operator", Some("parent")).unwrap();
    assert_eq!(states["counter"].state["count"], 4);
    assert_eq!(states["counter"].state["activations"], 1);
    // Another attempt that was never applied sees none of it.
    assert!(
        store
            .pack_states("operator", Some("other"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(log(&log_path).len(), 4);
}

#[test]
fn pack_effects_steer_continue_view_and_promote_tools() {
    let dir = Dir::new("effects");
    let (log_path, policy) = counter_pack(&dir.0);
    let (mut store, parent) = fixture(&dir.0);
    let watcher = Arc::new(Watcher::default());
    let mut code = host(&policy, &dir.0, Some(watcher.clone()));
    let result = call(
        &mut store,
        &parent,
        &mut code,
        0,
        "tool",
        tool(
            "counter/bump",
            json!({"steer":"compare with the baseline","continue":"run the next experiment","delay_ms":20,"view":"# 1 run","promote":"lookup"}),
        ),
    );
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(*watcher.views.lock().unwrap(), ["counter:Counter"]);
    assert_eq!(
        store.pack_steers("operator", "parent").unwrap(),
        [(
            "counter".to_string(),
            "compare with the baseline".to_string()
        )]
    );
    let snapshot = &store.pack_states("operator", Some("parent")).unwrap()["counter"];
    assert_eq!(
        snapshot.pending,
        [("run the next experiment".to_string(), 20)]
    );
    assert_eq!(snapshot.continuations_used, 1);
    assert_eq!(snapshot.view, Some(("Counter".into(), "# 1 run".into())));
    // The promoted tool is discoverable, described and callable in the same script.
    let found = call(
        &mut store,
        &parent,
        &mut code,
        0,
        "search",
        json!({"query":"look up"}),
    );
    assert_eq!(found["data"][0]["name"], "counter/lookup", "{found}");
    let described = call(
        &mut store,
        &parent,
        &mut code,
        0,
        "describe",
        json!({"name":"counter/lookup"}),
    );
    assert_eq!(described["data"]["risk"], "read");
    let looked = call(
        &mut store,
        &parent,
        &mut code,
        1,
        "tool",
        tool("counter/lookup", json!({"key":"k"})),
    );
    assert_eq!(looked["data"]["content"], "lookup k count=1", "{looked}");
    let refused = call(
        &mut store,
        &parent,
        &mut code,
        2,
        "tool",
        tool("counter/lookup", json!({"key":1})),
    );
    assert_eq!(refused["ok"], false);
    // A changed definition, or one above the operator's risk ceiling, is refused and
    // commits nothing.
    let changed = call(
        &mut store,
        &parent,
        &mut code,
        2,
        "tool",
        tool("counter/bump", json!({"promote":"changed"})),
    );
    assert!(
        changed["error"]
            .as_str()
            .unwrap()
            .contains("definition changed"),
        "{changed}"
    );
    let danger = call(
        &mut store,
        &parent,
        &mut code,
        3,
        "tool",
        tool("counter/bump", json!({"promote":"danger"})),
    );
    assert!(
        danger["error"].as_str().unwrap().contains("up to write"),
        "{danger}"
    );
    assert_eq!(
        store.pack_states("operator", Some("parent")).unwrap()["counter"].state["count"],
        1
    );
    // Promoted tools persist with the pack state: a new host after reopen still has it.
    drop(store);
    let mut store = Store::open(&dir.0.join("store")).unwrap();
    let parent = DispatchRequest {
        owner_epoch: store.epoch(),
        ..parent
    };
    let mut code = host(&policy, &dir.0, None);
    let again = call(
        &mut store,
        &parent,
        &mut code,
        4,
        "tool",
        tool("counter/lookup", json!({"key":"z"})),
    );
    assert_eq!(again["data"]["content"], "lookup z count=1", "{again}");
    assert_eq!(
        log(&log_path)
            .iter()
            .filter(|l| l.starts_with("activate"))
            .count(),
        1
    );
}

#[test]
fn ui_requests_are_answered_through_the_frontend_or_refused_without_one() {
    let dir = Dir::new("ui");
    let (log_path, policy) = counter_pack(&dir.0);
    let (mut store, parent) = fixture(&dir.0);
    let ui = Arc::new(Answering(
        UiResponse::Ok(json!({"name":"Ada"})),
        Mutex::new(vec![]),
    ));
    let watcher = Arc::new(Watcher {
        ui: Some(ui.clone()),
        ..Watcher::default()
    });
    let mut code = host(&policy, &dir.0, Some(watcher));
    let answered = call(
        &mut store,
        &parent,
        &mut code,
        0,
        "tool",
        tool("counter/bump", json!({"ask":"form"})),
    );
    assert_eq!(answered["ok"], true, "{answered}");
    assert!(
        answered["data"]["content"]
            .as_str()
            .unwrap()
            .ends_with("count=1\n\nanswer={\"status\": \"ok\", \"value\": {\"name\": \"Ada\"}}"),
        "{answered}"
    );
    assert_eq!(*ui.1.lock().unwrap(), ["counter:q1:form"]);
    // The pack saw the frontend's methods, and the answer is journaled in the child.
    assert!(log(&log_path)[1].ends_with("confirm,form,input,progress,select"));
    let receipts = store.code_receipts("operator").unwrap();
    assert!(
        receipts
            .iter()
            .any(|r| r["kind"] == "pack-ui-answer" && r["response"]["value"]["name"] == "Ada")
    );
    // No frontend: the pack hears a clear refusal and no UI methods are advertised.
    let mut code = host(&policy, &dir.0, None);
    let refused = call(
        &mut store,
        &parent,
        &mut code,
        1,
        "tool",
        tool("counter/bump", json!({"ask":"form"})),
    );
    let content = refused["data"]["content"].as_str().unwrap();
    assert!(content.contains("\"code\": \"unsupported\""), "{content}");
    assert!(content.contains("no UI is attached"), "{content}");
    assert!(log(&log_path).last().unwrap().ends_with(" -"));
    // An unadvertised method is refused by name.
    let mut code = host(
        &policy,
        &dir.0,
        Some(Arc::new(Watcher {
            ui: Some(ui.clone()),
            ..Watcher::default()
        })),
    );
    let odd = call(
        &mut store,
        &parent,
        &mut code,
        2,
        "tool",
        tool("counter/bump", json!({"ask":"vendor.map"})),
    );
    assert!(
        odd["data"]["content"]
            .as_str()
            .unwrap()
            .contains("vendor.map"),
        "{odd}"
    );
}

#[test]
fn reported_and_killed_pack_calls_are_known_or_uncertain() {
    let dir = Dir::new("outcomes");
    let (log_path, policy) = counter_pack(&dir.0);
    let (mut store, parent) = fixture(&dir.0);
    let mut code = host(&policy, &dir.0, None);
    let failed = call(
        &mut store,
        &parent,
        &mut code,
        0,
        "tool",
        tool("counter/fail", json!({})),
    );
    assert_eq!(
        (failed["ok"].clone(), failed.get("unknown").cloned()),
        (json!(false), None)
    );
    // A known failure does not block the next call.
    let next = call(
        &mut store,
        &parent,
        &mut code,
        1,
        "tool",
        tool("counter/bump", json!({})),
    );
    assert_eq!(next["ok"], true, "{next}");
    let lost = call(
        &mut store,
        &parent,
        &mut code,
        2,
        "tool",
        tool(
            "counter/fail",
            json!({"message":"may_have_run: response lost"}),
        ),
    );
    assert_eq!(lost["unknown"], true, "{lost}");
    let blocked = call(
        &mut store,
        &parent,
        &mut code,
        3,
        "tool",
        tool("counter/bump", json!({})),
    );
    assert!(
        blocked["error"].as_str().unwrap().contains("uncertain"),
        "{blocked}"
    );
    // A helper killed at its timeout is uncertain too.
    let other = Dir::new("outcomes-hang");
    let (mut store, parent) = fixture(&other.0);
    let started = Instant::now();
    let hung = call(
        &mut store,
        &parent,
        &mut code,
        0,
        "tool",
        tool("counter/hang", json!({})),
    );
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(
        (hung["unknown"].clone(), hung["may_have_run"].clone()),
        (json!(true), json!(true)),
        "{hung}"
    );
    let blocked = call(
        &mut store,
        &parent,
        &mut code,
        1,
        "tool",
        tool("counter/bump", json!({})),
    );
    assert!(
        blocked["error"].as_str().unwrap().contains("uncertain"),
        "{blocked}"
    );
    assert_eq!(
        log(&log_path).iter().filter(|l| l.contains("hang")).count(),
        1
    );
}

#[test]
fn a_changed_pack_executable_or_manifest_is_never_restored() {
    let dir = Dir::new("identity");
    let (_, policy) = counter_pack(&dir.0);
    let (mut store, parent) = fixture(&dir.0);
    let mut code = host(&policy, &dir.0, None);
    assert_eq!(
        call(
            &mut store,
            &parent,
            &mut code,
            0,
            "tool",
            tool("counter/bump", json!({}))
        )["ok"],
        true
    );
    // Bytes changed under the reviewed pin: refused before anything starts.
    let run = dir.0.join("counter/bin/run");
    executable(&run, &format!("{FIXTURE}\n# changed\n"));
    let refused = call(
        &mut store,
        &parent,
        &mut code,
        1,
        "tool",
        tool("counter/bump", json!({})),
    );
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("executable changed"),
        "{refused}"
    );
    assert!(
        store
            .code_receipts("operator")
            .unwrap()
            .iter()
            .all(|r| r["request"]["id"] != 1)
    );
    // Re-reviewed (a fresh candidate pins the new bytes): the saved state belongs to the
    // old executable and is not restored.
    let (_, policy) = counter_pack_policy_only(&dir.0);
    let mut code = host(&policy, &dir.0, None);
    let refused = call(
        &mut store,
        &parent,
        &mut code,
        1,
        "tool",
        tool("counter/bump", json!({})),
    );
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("changed its identity"),
        "{refused}"
    );
}

/// The policy from a fresh candidate export, without rewriting the pack files.
fn counter_pack_policy_only(root: &Path) -> (PathBuf, Value) {
    let candidate = crate::code_host::pack_candidate(&root.join("counter/runtime.json")).unwrap();
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
    (
        root.join("counter.log"),
        json!({"version":1,"models":[],"tools":tools}),
    )
}

#[test]
fn one_shot_bindings_still_refuse_state_and_session_bindings_need_a_pinned_manifest() {
    let dir = Dir::new("lifecycles");
    let (_, mut policy) = counter_pack(&dir.0);
    let (mut store, parent) = fixture(&dir.0);
    for tool in policy["tools"].as_array_mut().unwrap() {
        tool["lifecycle"] = json!("one-shot");
    }
    let mut code = host(&policy, &dir.0, None);
    let refused = call(
        &mut store,
        &parent,
        &mut code,
        0,
        "tool",
        tool("counter/bump", json!({})),
    );
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("stateful pack effects are unsupported"),
        "{refused}"
    );
    for tool in policy["tools"].as_array_mut().unwrap() {
        tool["lifecycle"] = json!("session");
        tool["manifest"] = json!("/elsewhere/runtime.json");
    }
    let mut code = host(&policy, &dir.0, None);
    let refused = call(
        &mut store,
        &parent,
        &mut code,
        1,
        "tool",
        tool("counter/bump", json!({})),
    );
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("pinned in `files`"),
        "{refused}"
    );
}

/// A loopback HTTP server: `/api/echo` returns the request it saw (headers included),
/// `/api/redirect-in` and `/api/redirect-out` redirect inside and outside the grant,
/// `/api/slow` answers after 5 s; `/api/tail` ends its body with the request's bearer
/// token after 100 bytes, `/api/leak-redirect` and `/api/leak-out` redirect to a URL
/// holding it (inside and outside the grant).
struct Http {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}
impl Http {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(vec![]));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let log = log.clone();
                std::thread::spawn(move || {
                    let mut request = vec![];
                    let mut chunk = [0; 4096];
                    let mut body_start = None;
                    loop {
                        let Ok(n) = stream.read(&mut chunk) else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        request.extend_from_slice(&chunk[..n]);
                        if body_start.is_none() {
                            body_start = request
                                .windows(4)
                                .position(|w| w == b"\r\n\r\n")
                                .map(|p| p + 4);
                        }
                        if let Some(start) = body_start {
                            let head =
                                String::from_utf8_lossy(&request[..start]).to_ascii_lowercase();
                            let length = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length: "))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if request.len() >= start + length {
                                break;
                            }
                        }
                    }
                    let text = String::from_utf8_lossy(&request).to_string();
                    log.lock().unwrap().push(text.clone());
                    let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let token = text
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .starts_with("authorization: bearer ")
                                .then(|| l[22..].trim().to_string())
                        })
                        .unwrap_or_default();
                    let (status, extra, body) = match path.as_str() {
                        "/api/tail" => (
                            "200 OK",
                            String::new(),
                            format!("{}Bearer {token}", "x".repeat(100)),
                        ),
                        "/api/leak-redirect" => (
                            "302 Found",
                            format!("Location: /api/echo?t={token}\r\n"),
                            String::new(),
                        ),
                        "/api/leak-out" => (
                            "302 Found",
                            format!("Location: http://127.0.0.1:9/evil?t={token}\r\n"),
                            String::new(),
                        ),
                        "/api/redirect-in" => (
                            "302 Found",
                            "Location: /api/echo\r\n".to_string(),
                            String::new(),
                        ),
                        "/api/redirect-out" => (
                            "302 Found",
                            "Location: http://127.0.0.1:9/evil\r\n".to_string(),
                            String::new(),
                        ),
                        "/api/slow" => {
                            std::thread::sleep(Duration::from_secs(5));
                            ("200 OK", String::new(), "late".into())
                        }
                        _ => (
                            "200 OK",
                            "Content-Type: application/json\r\n".into(),
                            json!({"request":text}).to_string(),
                        ),
                    };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        Self { port, seen }
    }
}

#[test]
fn http_grants_inject_secrets_stay_inside_their_prefix_and_are_journaled() {
    const SECRET: &str = "s3cr3t-token-for-spindle-tests";
    // SAFETY: only this test reads this variable.
    unsafe { std::env::set_var("SPINDLE_TEST_HTTP_SECRET", SECRET) };
    let server = Http::start();
    let dir = Dir::new("http");
    let policy = json!({"version":1,"models":[],"tools":[],"http":[{
        "name":"echo","url":format!("http://127.0.0.1:{}/api/", server.port),"methods":["GET","POST"],"risk":"write",
        "headers":{"Authorization":{"env":"SPINDLE_TEST_HTTP_SECRET","prefix":"Bearer "},"X-Static":"public"},
        "timeout_seconds":2,"max_response_bytes":65536}]});
    let (mut store, parent) = fixture(&dir.0);
    let mut code = host(&policy, &dir.0, None);
    let url = |path: &str| format!("http://127.0.0.1:{}{path}", server.port);
    let posted = call(
        &mut store,
        &parent,
        &mut code,
        0,
        "http",
        json!({"url":url("/api/echo"),"method":"POST","json":{"n":1},"headers":{"X-Trace":"t1"}}),
    );
    assert_eq!(posted["ok"], true, "{posted}");
    let seen = server.seen.lock().unwrap()[0].clone();
    assert!(
        seen.contains(&format!("authorization: Bearer {SECRET}"))
            || seen.contains(&format!("Authorization: Bearer {SECRET}")),
        "{seen}"
    );
    assert!(seen.contains("{\"n\":1}") && seen.to_ascii_lowercase().contains("x-trace: t1"));
    // The echo carried the secret back; the script never sees it.
    let shown = posted.to_string();
    assert!(!shown.contains(SECRET), "{shown}");
    assert!(shown.contains("Bearer [redacted]"), "{shown}");
    assert_eq!(posted["data"]["status"], 200);
    assert!(posted["data"]["json"]["request"].is_string());
    // The journal holds the request without the secret, and a repeat reuses it.
    let journal = serde_json::to_string(&store.code_receipts("operator").unwrap()).unwrap();
    assert!(!journal.contains(SECRET));
    assert_eq!(
        posted,
        call(
            &mut store,
            &parent,
            &mut code,
            0,
            "http",
            json!({"url":url("/api/echo"),"method":"POST","json":{"n":1},"headers":{"X-Trace":"t1"}})
        )
    );
    drop(store);
    let mut store = Store::open(&dir.0.join("store")).unwrap();
    let parent = DispatchRequest {
        owner_epoch: store.epoch(),
        ..parent
    };
    assert_eq!(
        posted,
        call(
            &mut store,
            &parent,
            &mut code,
            0,
            "http",
            json!({"url":url("/api/echo"),"method":"POST","json":{"n":1},"headers":{"X-Trace":"t1"}})
        )
    );
    assert_eq!(
        server.seen.lock().unwrap().len(),
        1,
        "a reused result is never re-sent"
    );
    // Redirects are followed only inside the grant.
    let inside = call(
        &mut store,
        &parent,
        &mut code,
        1,
        "http",
        json!({"url":url("/api/redirect-in")}),
    );
    assert_eq!(
        (
            inside["data"]["status"].clone(),
            inside["data"]["redirects"].clone()
        ),
        (json!(200), json!(1)),
        "{inside}"
    );
    let outside = call(
        &mut store,
        &parent,
        &mut code,
        2,
        "http",
        json!({"url":url("/api/redirect-out")}),
    );
    assert!(
        outside["error"]
            .as_str()
            .unwrap()
            .contains("leaves grant echo"),
        "{outside}"
    );
    assert_eq!(outside["may_have_run"], true);
    assert!(
        !server
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.contains("/evil"))
    );
    // Refusals name the grants; nothing is sent and no call is used up.
    for input in [
        json!({"url":url("/other")}),
        json!({"url":url("/api/echo"),"method":"DELETE"}),
        json!({"url":format!("http://user@127.0.0.1:{}/api/echo", server.port)}),
        json!({"url":url("/api/echo"),"headers":{"Authorization":"mine"}}),
    ] {
        let refused = call(&mut store, &parent, &mut code, 3, "http", input.clone());
        assert_eq!(refused["ok"], false, "{input}");
    }
    let named = call(
        &mut store,
        &parent,
        &mut code,
        3,
        "http",
        json!({"url":url("/other")}),
    );
    assert!(
        named["error"].as_str().unwrap().contains(&format!(
            "echo (GET, POST http://127.0.0.1:{}/api/)",
            server.port
        )),
        "{named}"
    );
    // A request that times out after it was sent is uncertain and blocks the rest.
    let slow = call(
        &mut store,
        &parent,
        &mut code,
        3,
        "http",
        json!({"url":url("/api/slow")}),
    );
    assert_eq!(slow["unknown"], true, "{slow}");
    let blocked = call(
        &mut store,
        &parent,
        &mut code,
        4,
        "http",
        json!({"url":url("/api/echo")}),
    );
    assert!(
        blocked["error"].as_str().unwrap().contains("uncertain"),
        "{blocked}"
    );
}

/// The installed MCP bridge, if this machine has one (tests copy it; nothing is
/// installed or logged in).
fn bridge() -> Option<PathBuf> {
    let path = std::env::var_os("SPINDLE_MCP_BRIDGE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                PathBuf::from(home)
                    .join(".dext/shelves/engineering/packs/mcp_bridge/bin/mcp-bridge")
            })
        })?;
    path.is_file().then_some(path)
}

const MCP_SERVER: &str = r#"import json, sys, time
notes = sys.argv[1]
tools = [
  {'name':'create_note','description':'Append a note.','inputSchema':{'type':'object','properties':{'title':{'type':'string'}},'required':['title'],'additionalProperties':False}},
  {'name':'list_notes','description':'List notes.','inputSchema':{'type':'object','properties':{},'additionalProperties':False}},
  {'name':'stall','description':'Never answers.','inputSchema':{'type':'object','properties':{},'additionalProperties':False}},
]
for line in sys.stdin:
    req = json.loads(line)
    if 'id' not in req:
        continue
    method = req.get('method')
    if method == 'initialize':
        result = {'protocolVersion':req['params']['protocolVersion'],'capabilities':{'tools':{}},'serverInfo':{'name':'notes','version':'1'}}
    elif method == 'tools/list':
        result = {'tools':tools}
    elif method == 'tools/call':
        name = req['params']['name']
        if name == 'create_note':
            with open(notes, 'a') as f:
                f.write(req['params']['arguments']['title'] + '\n')
            text = 'created ' + req['params']['arguments']['title']
        elif name == 'stall':
            time.sleep(60)
            text = 'late'
        else:
            text = open(notes).read() if __import__('os').path.exists(notes) else ''
        result = {'content':[{'type':'text','text':text}],'isError':False}
    elif method == 'ping':
        result = {}
    else:
        print(json.dumps({'jsonrpc':'2.0','id':req['id'],'error':{'code':-32601,'message':'Method not found'}}), flush=True)
        continue
    print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':result}), flush=True)
"#;

fn run(command: &Path, args: &[&str], cwd: &Path) {
    // A binary this test just wrote can be "busy" for a moment: a test forking
    // meanwhile holds its write descriptor until that child execs.
    let mut tries = 0;
    let output = loop {
        match std::process::Command::new(command)
            .args(args)
            .current_dir(cwd)
            .output()
        {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 50 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            result => break result.unwrap(),
        }
    };
    assert!(
        output.status.success(),
        "{} {args:?}: {}{}",
        command.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn mcp_mutations_through_the_bridge_are_journaled_reused_and_uncertainty_blocks() {
    let Some(installed) = bridge() else {
        eprintln!("no mcp-bridge binary (set SPINDLE_MCP_BRIDGE); MCP end-to-end skipped");
        return;
    };
    let dir = Dir::new("mcp");
    let pack = dir.0.join("packs/mcp_bridge");
    std::fs::create_dir_all(pack.join("bin")).unwrap();
    std::fs::copy(&installed, pack.join("bin/mcp-bridge")).unwrap();
    std::fs::write(pack.join("PACK.md"), "# mcp_bridge (test copy)\n").unwrap();
    let server = dir.0.join("notes_server.py");
    std::fs::write(&server, MCP_SERVER).unwrap();
    let notes = dir.0.join("notes.txt");
    let config = json!({"version":1,"servers":{"notes":{
        "transport":{"transport":"stdio","command":"/usr/bin/python3","args":[server, notes],"cwd":dir.0},
        "tools":{"create_note":"write","list_notes":"read","stall":"write"},"persistent":false}}});
    std::fs::write(pack.join("config.local.json"), config.to_string()).unwrap();
    let bin = pack.join("bin/mcp-bridge");
    run(
        &bin,
        &[
            "discover",
            "--config",
            "config.local.json",
            "--server",
            "notes",
            "--output",
            "notes.catalog.json",
        ],
        &pack,
    );
    run(
        &bin,
        &[
            "prepare",
            "--config",
            "config.local.json",
            "--catalog",
            "notes.catalog.json",
            "--output",
            pack.to_str().unwrap(),
        ],
        &pack,
    );
    std::fs::rename(
        pack.join("runtime.candidate.json"),
        pack.join("runtime.json"),
    )
    .unwrap();
    // The operator's review: the candidate names what mcp_call reaches, marks browser
    // Apps unsupported and suggests the session lifecycle (promotion needs state).
    let candidate = crate::code_host::pack_candidate(&pack.join("runtime.json")).unwrap();
    let entries = candidate["tools"].as_array().unwrap();
    let find = |name: &str| entries.iter().find(|t| t["tool"] == name).unwrap().clone();
    assert_eq!(find("mcp_ui")["lifecycle"], "unsupported");
    assert_eq!(find("mcp_call")["risk"], "danger");
    let note = find("mcp_call")["notes"][0].as_str().unwrap().to_string();
    assert!(
        note.contains("mutations included") && note.contains("notes/create_note (write)"),
        "{note}"
    );
    let tools: Vec<Value> = ["mcp_search", "mcp_describe", "mcp_call"]
        .iter()
        .map(|name| {
            let mut entry = find(name);
            assert_eq!(entry["suggested_lifecycle"], "session");
            entry["lifecycle"] = json!("session");
            entry["timeout_seconds"] = json!(4);
            entry
        })
        .collect();
    let policy = json!({"version":1,"models":[],"tools":tools});
    let (mut store, parent) = fixture(&dir.0);
    let mut code = host(&policy, &dir.0, None);
    let create = |title: &str| {
        tool(
            "mcp_bridge/mcp_call",
            json!({"server":"notes","tool":"create_note","arguments":json!({"title":title}).to_string()}),
        )
    };
    let created = call(&mut store, &parent, &mut code, 0, "tool", create("first"));
    assert_eq!(created["ok"], true, "{created}");
    assert!(
        created["data"]["content"]
            .as_str()
            .unwrap()
            .contains("created first")
    );
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "first\n");
    // Journaled: a reservation before the call and its result after.
    let receipts = store.code_receipts("operator").unwrap();
    assert!(
        receipts
            .iter()
            .any(|r| r["kind"] == "reserved"
                && r["request"]["input"]["name"] == "mcp_bridge/mcp_call")
    );
    assert!(
        receipts
            .iter()
            .any(|r| r["kind"] == "result" && r["response"]["ok"] == true)
    );
    // Reused, not re-run, before and after reopen.
    assert_eq!(
        created,
        call(&mut store, &parent, &mut code, 0, "tool", create("first"))
    );
    drop(store);
    let mut store = Store::open(&dir.0.join("store")).unwrap();
    let parent = DispatchRequest {
        owner_epoch: store.epoch(),
        ..parent
    };
    assert_eq!(
        created,
        call(&mut store, &parent, &mut code, 0, "tool", create("first"))
    );
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "first\n");
    // Promotion registers the mutating tool directly (persisted with the pack state).
    let promoted = call(
        &mut store,
        &parent,
        &mut code,
        1,
        "tool",
        tool(
            "mcp_bridge/mcp_search",
            json!({"query":"create note","promote":true}),
        ),
    );
    assert_eq!(promoted["ok"], true, "{promoted}");
    let direct = call(
        &mut store,
        &parent,
        &mut code,
        1,
        "search",
        json!({"query":"append a note"}),
    );
    let direct_name = direct["data"][0]["name"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        direct_name.starts_with("mcp_bridge/") && direct_name.contains("create_note"),
        "{direct}"
    );
    let second = call(
        &mut store,
        &parent,
        &mut code,
        2,
        "tool",
        tool(&direct_name, json!({"title":"second"})),
    );
    assert_eq!(second["ok"], true, "{second}");
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "first\nsecond\n");
    // A call that may have been sent but never answered is uncertain and blocks the
    // script's later external calls; nothing is retried.
    let stalled = call(
        &mut store,
        &parent,
        &mut code,
        3,
        "tool",
        tool(
            "mcp_bridge/mcp_call",
            json!({"server":"notes","tool":"stall","arguments":"{}"}),
        ),
    );
    assert_eq!(
        (stalled["unknown"].clone(), stalled["may_have_run"].clone()),
        (json!(true), json!(true)),
        "{stalled}"
    );
    let blocked = call(&mut store, &parent, &mut code, 4, "tool", create("third"));
    assert!(
        blocked["error"].as_str().unwrap().contains("uncertain"),
        "{blocked}"
    );
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "first\nsecond\n");
}

#[test]
fn promoted_tools_follow_the_operators_current_risk_ceiling_and_grant() {
    let dir = Dir::new("ceiling");
    let (_, policy) = counter_pack(&dir.0);
    let (mut store, parent) = fixture(&dir.0);
    let mut code = host(&policy, &dir.0, None);
    let promoted = call(
        &mut store,
        &parent,
        &mut code,
        0,
        "tool",
        tool("counter/bump", json!({"promote":"write"})),
    );
    assert_eq!(promoted["ok"], true, "{promoted}");
    let stashed = call(
        &mut store,
        &parent,
        &mut code,
        1,
        "tool",
        tool("counter/stash", json!({"key":"a"})),
    );
    assert_eq!(stashed["data"]["content"], "stash a count=1", "{stashed}");
    // The operator lowers the ceiling to read after the promotion: the write tool is
    // no longer listed, described or callable, by the host or by the store.
    let mut lowered = policy.clone();
    for entry in lowered["tools"].as_array_mut().unwrap() {
        entry["max_dynamic_risk"] = json!("read");
    }
    let mut code = host(&lowered, &dir.0, None);
    let found = call(
        &mut store,
        &parent,
        &mut code,
        0,
        "search",
        json!({"query":"look up"}),
    );
    assert!(!found.to_string().contains("counter/stash"), "{found}");
    let described = call(
        &mut store,
        &parent,
        &mut code,
        0,
        "describe",
        json!({"name":"counter/stash"}),
    );
    assert_eq!(described["ok"], false, "{described}");
    let refused = call(
        &mut store,
        &parent,
        &mut code,
        2,
        "tool",
        tool("counter/stash", json!({"key":"b"})),
    );
    assert_eq!(refused["ok"], false, "{refused}");
    let stage = Stage {
        id: crate::code_mode::child_stage(&parent.attempt.id, 3),
        scope: parent.scope.clone(),
        owner_epoch: store.epoch(),
    };
    let direct = store
        .run_pack_child(
            &parent,
            &stage,
            3,
            &json!({"kind":"pack","pack":"counter","tool":"stash","input":{"key":"c"}}),
            &mut code,
            &AtomicBool::new(false),
            Duration::from_secs(10),
        )
        .unwrap();
    assert!(
        direct["error"]
            .as_str()
            .unwrap_or_default()
            .contains("does not declare tool stash"),
        "{direct}"
    );
    // Read-risk promoted tools stay available under the lower ceiling.
    let mut code = host(&lowered, &dir.0, None);
    call(
        &mut store,
        &parent,
        &mut code,
        4,
        "tool",
        tool("counter/bump", json!({"promote":"lookup"})),
    );
    let looked = call(
        &mut store,
        &parent,
        &mut code,
        5,
        "tool",
        tool("counter/lookup", json!({"key":"k"})),
    );
    assert_eq!(looked["ok"], true, "{looked}");
    // The pack's grant is removed: none of its promoted tools remain.
    let revoked = json!({"version":1,"models":[],"tools":[]});
    let mut code = host(&revoked, &dir.0, None);
    for name in ["counter/lookup", "counter/stash"] {
        let described = call(
            &mut store,
            &parent,
            &mut code,
            0,
            "describe",
            json!({"name":name}),
        );
        assert_eq!(described["ok"], false, "{name}: {described}");
    }
    let refused = call(
        &mut store,
        &parent,
        &mut code,
        6,
        "tool",
        tool("counter/lookup", json!({"key":"k"})),
    );
    assert_eq!(refused["ok"], false, "{refused}");
}

/// A grant over a loopback server answering `/api/big-json` (about 500 KiB of JSON)
/// and `/api/controls` (control characters, six bytes each once escaped).
fn large_bodies() -> (u16, Value) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut head = vec![];
                let mut byte = [0; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let text = String::from_utf8_lossy(&head).to_string();
                let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (kind, body) = match path.as_str() {
                    "/api/big-json" => (
                        "application/json",
                        json!({"items":vec!["0123456789\"abcdef"; 24 * 1024]}).to_string(),
                    ),
                    _ => ("text/plain", "\u{1}".repeat(300 * 1024)),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    let policy = json!({"version":1,"models":[],"tools":[],"http":[{
        "name":"big","url":format!("http://127.0.0.1:{port}/api/"),"risk":"read",
        "timeout_seconds":10,"max_response_bytes":655360}]});
    (port, policy)
}

fn head(value: &Value) -> String {
    value.to_string().chars().take(400).collect()
}

#[test]
fn large_http_responses_are_known_outcomes_with_a_truncation_marker() {
    let (port, policy) = large_bodies();
    let dir = Dir::new("http-large");
    let (mut store, parent) = fixture(&dir.0);
    let mut code = host(&policy, &dir.0, None);
    let url = |path: &str| format!("http://127.0.0.1:{port}{path}");
    // JSON that parses is returned once, as `json` (not again as `body`).
    let parsed = call(
        &mut store,
        &parent,
        &mut code,
        0,
        "http",
        json!({"url":url("/api/big-json")}),
    );
    assert_eq!(parsed["ok"], true, "{}", head(&parsed));
    assert!(parsed.get("unknown").is_none());
    assert_eq!(
        parsed["data"]["json"]["items"].as_array().unwrap().len(),
        24 * 1024
    );
    assert!(parsed["data"].get("body").is_none());
    assert_eq!(parsed["data"]["truncated"], false);
    // Escaping would take this body far past one reply: it is cut, and says so.
    let controls = call(
        &mut store,
        &parent,
        &mut code,
        1,
        "http",
        json!({"url":url("/api/controls")}),
    );
    assert_eq!(controls["ok"], true, "{}", head(&controls));
    assert_eq!(controls["data"]["status"], 200);
    assert_eq!(controls["data"]["truncated"], true);
    let body = controls["data"]["body"].as_str().unwrap();
    assert!(!body.is_empty() && body.len() < 300 * 1024);
    assert!(serde_json::to_vec(&controls).unwrap().len() <= crate::code_mode::MAX_REPLY);
    // Both were known outcomes: the script goes on.
    let again = call(
        &mut store,
        &parent,
        &mut code,
        2,
        "http",
        json!({"url":url("/api/big-json"),"method":"GET"}),
    );
    assert_eq!(again["ok"], true, "{}", head(&again));
}

#[test]
fn http_headers_that_cannot_be_sent_are_refused_before_anything_is_sent() {
    // SAFETY: only this test reads this variable.
    unsafe { std::env::set_var("SPINDLE_TEST_HTTP_CONTROL", "tok\u{1}en") };
    let server = Http::start();
    let dir = Dir::new("http-headers");
    let url = format!("http://127.0.0.1:{}/api/", server.port);
    let policy = json!({"version":1,"models":[],"tools":[],"http":[
        {"name":"echo","url":url,"risk":"read","timeout_seconds":2},
        {"name":"secret","url":format!("http://127.0.0.1:{}/hidden/", server.port),"risk":"read","timeout_seconds":2,
         "headers":{"X-Key":{"env":"SPINDLE_TEST_HTTP_CONTROL"}}}]});
    let (mut store, parent) = fixture(&dir.0);
    let mut code = host(&policy, &dir.0, None);
    let target = format!("http://127.0.0.1:{}/api/echo", server.port);
    for (id, value) in ["a\u{1}b", "a\u{7f}b", "a\u{1f}b"].iter().enumerate() {
        let refused = call(
            &mut store,
            &parent,
            &mut code,
            id as u64,
            "http",
            json!({"url":target,"headers":{"X-Trace":value}}),
        );
        assert_eq!(refused["ok"], false, "{refused}");
        assert!(refused.get("unknown").is_none(), "{refused}");
    }
    // A grant secret that cannot form a header value fails before sending: known.
    let secret = call(
        &mut store,
        &parent,
        &mut code,
        0,
        "http",
        json!({"url":format!("http://127.0.0.1:{}/hidden/x", server.port)}),
    );
    assert_eq!(secret["ok"], false, "{secret}");
    assert_eq!(secret["may_have_run"], false, "{secret}");
    assert!(secret.get("unknown").is_none(), "{secret}");
    // Nothing reached the server and nothing is uncertain: the script goes on.
    let sent = call(
        &mut store,
        &parent,
        &mut code,
        1,
        "http",
        json!({"url":target,"headers":{"X-Trace":"tab\tok"}}),
    );
    assert_eq!(sent["ok"], true, "{sent}");
    assert_eq!(server.seen.lock().unwrap().len(), 1);
}

#[test]
fn http_secrets_are_scrubbed_before_any_cut_and_from_urls_and_errors() {
    const SECRET: &str = "s3cr3t-token-for-scrub-tests";
    // SAFETY: only this test reads this variable.
    unsafe { std::env::set_var("SPINDLE_TEST_HTTP_SCRUB", SECRET) };
    let server = Http::start();
    let dir = Dir::new("http-scrub");
    // The limit falls inside the echoed secret: 100 bytes, "Bearer ", 5 of its bytes.
    let policy = json!({"version":1,"models":[],"tools":[],"http":[{
        "name":"echo","url":format!("http://127.0.0.1:{}/api/", server.port),"risk":"read",
        "headers":{"Authorization":{"env":"SPINDLE_TEST_HTTP_SCRUB","prefix":"Bearer "}},
        "timeout_seconds":2,"max_response_bytes":112}]});
    let (mut store, parent) = fixture(&dir.0);
    let mut code = host(&policy, &dir.0, None);
    let url = |path: &str| format!("http://127.0.0.1:{}{path}", server.port);
    let leaked = |value: &Value| value.to_string().contains(&SECRET[..5]);
    let cut = call(
        &mut store,
        &parent,
        &mut code,
        0,
        "http",
        json!({"url":url("/api/tail")}),
    );
    assert_eq!(cut["data"]["truncated"], true, "{cut}");
    assert!(!leaked(&cut), "{cut}");
    assert!(
        cut["data"]["body"]
            .as_str()
            .unwrap()
            .starts_with(&"x".repeat(100))
    );
    let followed = call(
        &mut store,
        &parent,
        &mut code,
        1,
        "http",
        json!({"url":url("/api/leak-redirect")}),
    );
    assert_eq!(followed["data"]["redirects"], 1, "{followed}");
    assert!(!leaked(&followed), "{followed}");
    let refused = call(
        &mut store,
        &parent,
        &mut code,
        2,
        "http",
        json!({"url":url("/api/leak-out")}),
    );
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("leaves grant echo"),
        "{refused}"
    );
    assert!(!leaked(&refused), "{refused}");
    let journal = serde_json::to_string(&store.code_receipts("operator").unwrap()).unwrap();
    assert!(!journal.contains(&SECRET[..5]));
}

#[test]
fn pack_calls_read_no_ledger_record_again_however_long_the_ledger_grows() {
    let dir = Dir::new("ledger");
    let (_, policy) = counter_pack(&dir.0);
    let (mut store, parent) = fixture(&dir.0);
    let mut code = host(&policy, &dir.0, None);
    // A long ledger of large snapshots: every call shows a 100 KiB view.
    let view = "v".repeat(100 * 1024);
    for id in 0..40 {
        let bumped = call(
            &mut store,
            &parent,
            &mut code,
            id,
            "tool",
            tool("counter/bump", json!({"view":view})),
        );
        assert_eq!(bumped["ok"], true, "{bumped}");
    }
    drop(store);
    let mut store = Store::open(&dir.0.join("store")).unwrap();
    let parent = DispatchRequest {
        owner_epoch: store.epoch(),
        ..parent
    };
    let mut code = host(&policy, &dir.0, None);
    // A reopened handle reads the 41 anchors once (activation and 40 calls), and no
    // snapshot object: those live in their own stages.
    call(
        &mut store,
        &parent,
        &mut code,
        40,
        "tool",
        tool("counter/bump", json!({})),
    );
    assert_eq!(store.pack_ledger_reads("operator").unwrap(), 41);
    for id in 41..60 {
        call(
            &mut store,
            &parent,
            &mut code,
            id,
            "tool",
            tool("counter/bump", json!({})),
        );
    }
    assert_eq!(
        store.pack_ledger_reads("operator").unwrap(),
        41,
        "later calls read no ledger record"
    );
    let state = &store.pack_states("operator", Some("parent")).unwrap()["counter"];
    assert_eq!(state.state["count"], 60);
}
