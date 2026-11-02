//! Threads a client brings and takes back, browser tokens and one-click connect,
//! through `handle` on a loopback socket as a client sends them. Stops of the host are
//! simulated at each boundary of an import (`Host::cut`) and by reopening the host.
use super::super::connect;
use super::super::{Host, Sink, Watch, lock};
use super::*;
use serde_json::{Value, json};
use spindle_store::agent::{Session, Settings};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn temp_host(name: &str) -> (PathBuf, Arc<Host>) {
    let root = std::env::temp_dir().join(format!("spindle-threads-{name}-{}", std::process::id()));
    let _ = super::super::disk::remove_tree(&root);
    let host = Arc::new(Host::open(root.clone(), None, None).unwrap());
    (root, host)
}

/// serve.lock may be briefly held by a child another test forks (see mod.rs).
fn reopen(root: &Path) -> Arc<Host> {
    let started = Instant::now();
    loop {
        match Host::open(root.to_path_buf(), None, None) {
            Ok(host) => return Arc::new(host),
            Err(error)
                if error.contains("another spindle serve")
                    && started.elapsed() < Duration::from_secs(5) =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("reopen: {error}"),
        }
    }
}

/// Waits for a background removal.
fn settle(path: &Path) {
    let started = Instant::now();
    while path.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{} stays",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// One request through `handle`: its head (with HOST for the address) and body;
/// `cut` sends only that many body bytes, then closes.
fn exchange(
    host: &Arc<Host>,
    head: &str,
    body: &[u8],
    cut: Option<usize>,
) -> (u16, String, Vec<u8>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server_host = host.clone();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        super::super::handle(&server_host, &mut stream);
    });
    let mut client = TcpStream::connect(addr).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    client
        .write_all(head.replace("HOST", &addr.to_string()).as_bytes())
        .unwrap();
    match cut {
        Some(count) => {
            client.write_all(&body[..count]).unwrap();
            client.shutdown(std::net::Shutdown::Write).unwrap();
        }
        None => {
            let _ = client.write_all(body);
        }
    }
    let mut bytes = Vec::new();
    let _ = client.read_to_end(&mut bytes);
    server.join().unwrap();
    let end = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(bytes.len(), |at| at + 4);
    let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
    let status = head.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0);
    (status, head, bytes[end..].to_vec())
}

fn json_of(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

fn get(host: &Arc<Host>, path: &str, token: &str) -> (u16, Value) {
    let (status, _, body) = exchange(
        host,
        &format!("GET {path} HTTP/1.1\r\nHost: HOST\r\nAuthorization: Bearer {token}\r\n\r\n"),
        b"",
        None,
    );
    (status, json_of(&body))
}

fn post(host: &Arc<Host>, path: &str, token: &str, body: &Value) -> (u16, Value) {
    let body = body.to_string();
    let auth = if token.is_empty() {
        String::new()
    } else {
        format!("Authorization: Bearer {token}\r\n")
    };
    let (status, _, reply) = exchange(
        host,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: HOST\r\n{auth}Content-Length: {}\r\n\r\n",
            body.len()
        ),
        body.as_bytes(),
        None,
    );
    (status, json_of(&reply))
}

/// `POST /v1/threads` with `bundle`.
fn bring(host: &Arc<Host>, token: &str, key: &str, query: &str, bundle: &[u8]) -> (u16, Value) {
    let (status, _, reply) = exchange(
        host,
        &format!(
            "POST /v1/threads{query} HTTP/1.1\r\nHost: HOST\r\nAuthorization: Bearer {token}\r\nIdempotency-Key: {key}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
            bundle.len()
        ),
        bundle,
        None,
    );
    (status, json_of(&reply))
}

/// `POST /v1/sessions/:id/export`: status, head and bytes.
fn take(host: &Arc<Host>, token: &str, id: &str, keep: bool) -> (u16, String, Vec<u8>) {
    let body = json!({ "keep": keep }).to_string();
    exchange(
        host,
        &format!(
            "POST /v1/sessions/{id}/export HTTP/1.1\r\nHost: HOST\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        ),
        body.as_bytes(),
        None,
    )
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// A model on loopback that answers every request with a short reply (an OpenAI
/// chat-completions stream), for the life of the test process; its endpoint.
fn model() -> String {
    static ENDPOINT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ENDPOINT
        .get_or_init(|| {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!(
                "http://{}/v1/chat/completions",
                listener.local_addr().unwrap()
            );
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    std::thread::spawn(move || answer(stream));
                }
            });
            endpoint
        })
        .clone()
}

fn answer(mut stream: TcpStream) {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    let end = loop {
        let Ok(count) = stream.read(&mut chunk) else {
            return;
        };
        if count == 0 {
            return;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    while bytes.len() < end + length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
        }
    }
    let delta = |delta: Value, finish: Value| {
        json!({"id": "x", "object": "chat.completion.chunk", "model": "m",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    };
    let mut out = format!(
        "data: {}\n\ndata: {}\n\n",
        delta(
            json!({"role": "assistant", "content": "Noted."}),
            Value::Null
        ),
        delta(json!({}), json!("stop"))
    );
    out.push_str("data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n");
    out.push_str("data: [DONE]\n\n");
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
        out.len()
    );
}

/// Every provider is the loopback model, with no credentials.
struct Loopback;
impl spindle_store::agent::Credentials for Loopback {
    fn route(&self, _: &str, model: Option<&str>) -> Result<spindle_store::agent::Route, String> {
        let mut route = spindle_store::agent::NativeCredentials.route("local", model)?;
        route.endpoint = self::model();
        Ok(route)
    }
    fn live_headers(
        &self,
        _: &spindle_store::agent::Route,
        _: &str,
    ) -> Result<reqwest::header::HeaderMap, String> {
        Ok(reqwest::header::HeaderMap::new())
    }
    fn auth_kind(
        &self,
        _: &spindle_store::agent::Route,
    ) -> Result<dext_provider_catalog::provider::RuntimeAuthKind, String> {
        Err("not a subscription".into())
    }
}

/// A turn on the session in `dir`: a prompt and the model's reply. Returns the head.
fn turn(dir: &Path, text: &str) -> u64 {
    let mut session =
        Session::open_with_credentials(dir, Arc::new(Sink(Watch::new(), None)), Arc::new(Loopback))
            .unwrap();
    session.prompt(text).unwrap();
    drop(session);
    spindle_store::api::head_state(dir).unwrap()["head"]
        .as_u64()
        .unwrap()
}

/// A browser thread's settings (as the package keeps them), on `provider`.
fn browser_settings(provider: &str) -> Value {
    json!({"provider": provider, "model": local().1, "network": true, "toolchain": false,
        "sources": ["/etc"], "isolation": "auto", "effort": "default", "compaction": "model",
        "max_usd": 2.5, "self_edit": true, "builtin_tools": "files", "runner": "app"})
}

/// A session made and moved into a bundle by `Session::export_session`, as a client
/// would bring it: its bytes, head and identity.
fn bundle_of(root: &Path, name: &str, settings: &Value, notes: &[&str]) -> (Vec<u8>, u64, String) {
    let dir = root.join(format!("source-{name}"));
    let settings = Settings::from_json(settings).unwrap();
    drop(
        Session::create_with_credentials(
            &dir,
            settings,
            vec![],
            Arc::new(Sink(Watch::new(), None)),
            Arc::new(Loopback),
        )
        .unwrap(),
    );
    for note in notes {
        turn(&dir, note);
    }
    std::fs::write(dir.join("pack-session-id"), "1761990000-77-a1b2c3d4e5f6").unwrap();
    let target = root.join(format!("{name}.spindle"));
    let mut session = Session::open(&dir, Arc::new(Sink(Watch::new(), None))).unwrap();
    let exported = session.export_session(&target, false).unwrap();
    drop(session);
    let bytes = std::fs::read(&target).unwrap();
    (
        bytes,
        exported["head"].as_u64().unwrap(),
        exported["identity"].as_str().unwrap().into(),
    )
}

fn local() -> (String, String) {
    Settings::route_of("local", None).unwrap()
}

fn browser_token(host: &Host, name: &str) -> String {
    connect::mint(&mut lock(&host.db), name).unwrap()
}

fn count(host: &Host, sql: &str) -> i64 {
    lock(&host.db).query_row(sql, [], |row| row.get(0)).unwrap()
}

/// Nothing of an import that did not happen: no row, owner, record, staging or upload.
fn nothing_registered(host: &Host, root: &Path, sessions_before: i64) {
    assert_eq!(
        count(host, "SELECT count(*) FROM sessions"),
        sessions_before
    );
    assert_eq!(
        count(host, "SELECT count(*) FROM threads WHERE state='pending'"),
        0
    );
    let incoming: Vec<_> = std::fs::read_dir(root.join("incoming"))
        .map(|entries| entries.flatten().map(|entry| entry.file_name()).collect())
        .unwrap_or_default();
    assert!(incoming.is_empty(), "{incoming:?}");
    let listed = lock(&host.lives).len() as i64;
    assert_eq!(listed, sessions_before);
}

#[test]
fn a_thread_comes_and_goes_back_with_its_head_and_identity() {
    let (root, host) = temp_host("round");
    let token = browser_token(&host, "Spindle for Browser");
    let (bundle, head, identity) =
        bundle_of(&root, "round", &browser_settings("local"), &["one", "two"]);
    assert!(head >= 2);
    // Another model of the host's (an alias the catalog resolves).
    let (route_provider, route_model) = Settings::route_of("local", Some("qwen3.8")).unwrap();
    assert_ne!(route_model, local().1);
    let (status, body) = bring(
        &host,
        &token,
        "key-round-0000000001",
        &format!("?name=From%20Edge&provider={route_provider}&model={route_model}"),
        &bundle,
    );
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["replayed"], false);
    let id = body["session"]["id"].as_str().unwrap().to_string();
    assert_eq!(body["session"]["head"], head);
    assert_eq!(body["session"]["name"], "From Edge");
    assert_eq!(body["session"]["moved"], false);
    // The host's fit, applied and said.
    let changes = &body["fit"]["changes"];
    assert_eq!(changes["isolation"], "required", "{body}");
    assert_eq!(changes["builtin_tools"], "all");
    assert_eq!(changes["runner"], "none");
    assert_eq!(changes["sources"], json!([]));
    assert_eq!(changes["self_edit"], false);
    assert_eq!(changes["model"], route_model.as_str());
    assert!(
        body["fit"]["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note["setting"] == "max_usd")
    );
    let settings = Settings::load(&host.dir(&id)).unwrap();
    assert_eq!(
        settings.isolation,
        spindle_store::agent::Isolation::Required
    );
    assert_eq!(
        settings.builtin_tools,
        spindle_store::agent::BuiltinTools::All
    );
    assert_eq!(settings.runner, spindle_store::driver::Runner::None);
    assert!(settings.sources.is_empty() && !settings.self_edit && settings.network);
    assert_eq!(settings.max_usd, Some(2.5));
    // The pack keeps its session id across the move.
    assert_eq!(
        std::fs::read_to_string(host.dir(&id).join("pack-session-id")).unwrap(),
        "1761990000-77-a1b2c3d4e5f6"
    );
    let (status, listed) = get(&host, "/v1/sessions", &token);
    assert_eq!(status, 200);
    assert_eq!(listed["sessions"][0]["id"], id.as_str());
    // Back to the client: sealed here once the bundle is on disk.
    let (status, reply, bytes) = take(&host, &token, &id, false);
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    assert_eq!(
        header(&reply, "content-type"),
        Some("application/octet-stream")
    );
    assert_eq!(
        header(&reply, "x-spindle-head"),
        Some(head.to_string().as_str())
    );
    assert_eq!(
        header(&reply, "x-spindle-identity"),
        Some(identity.as_str())
    );
    assert_eq!(
        header(&reply, "content-length"),
        Some(bytes.len().to_string().as_str())
    );
    let back = spindle_store::bundle::read(&bytes).unwrap();
    assert_eq!(
        (back.head, back.identity.as_str()),
        (head, identity.as_str())
    );
    assert!(back.files.iter().any(|file| file.path == "pack-session-id"));
    // The client imports it as the same session.
    let place = root.join("client");
    let imported = spindle_store::agent::import_bundle(&back, &place).unwrap();
    assert_eq!(imported["head"], head);
    assert_eq!(imported["identity"], identity.as_str());
    let (_, listed) = get(&host, "/v1/sessions", &token);
    assert_eq!(listed["sessions"][0]["status"]["state"], "moved");
    assert_eq!(
        get(&host, &format!("/v1/sessions/{id}"), &token).1["moved"],
        true
    );
    // Asked again (a lost reply): the same bytes, also after a restart.
    let (status, _, again) = take(&host, &token, &id, false);
    assert_eq!((status, again == bytes), (200, true));
    drop(host);
    let host = reopen(&root);
    let (status, _, again) = take(&host, &token, &id, false);
    assert_eq!((status, again == bytes), (200, true));
    // A copy of a moved thread is not this host's to give, and no work is taken.
    let (status, _, refused) = take(&host, &token, &id, true);
    assert_eq!(
        (status, json_of(&refused)["code"].clone()),
        (409, json!("moved"))
    );
    let (status, refused) = post(
        &host,
        "/v1/commands",
        &token,
        &json!({"request_id": "say-moved", "kind": "say", "session": id, "text": "hi"}),
    );
    assert_eq!(
        (status, refused["code"].clone()),
        (409, json!("moved")),
        "{refused}"
    );
    // The client holds it: deleting the sealed copy takes its bundle and rows too.
    let (status, receipt) = post(
        &host,
        "/v1/commands",
        &token,
        &json!({"request_id": "del-moved", "kind": "delete", "session": id}),
    );
    assert_eq!(
        (status, receipt["state"].clone()),
        (202, json!("completed")),
        "{receipt}"
    );
    assert!(!host.dir(&id).exists());
    settle(&root.join("trash").join(&id));
    assert_eq!(count(&host, "SELECT count(*) FROM owners"), 0);
    assert_eq!(count(&host, "SELECT count(*) FROM threads"), 0);
    // Brought again later (its record went with it): a new session, the same thread.
    let (status, body) = bring(&host, &token, "key-round-0000000001", "", &bytes);
    assert_eq!(status, 201, "{body}");
    assert_ne!(body["session"]["id"], id.as_str());
    assert_eq!(body["session"]["head"], head);
    drop(host);
    let _ = super::super::disk::remove_tree(&root);
}

#[test]
fn imports_replay_by_key_and_refuse_what_they_cannot_take() {
    let (root, host) = temp_host("replay");
    let token = browser_token(&host, "edge");
    let (bundle, head, _) = bundle_of(&root, "a", &browser_settings("local"), &["a"]);
    let (other, _, _) = bundle_of(&root, "b", &browser_settings("local"), &["b", "c"]);
    let key = "replay-key-000000001";
    let (status, first) = bring(&host, &token, key, "", &bundle);
    assert_eq!(status, 201, "{first}");
    assert_eq!(first["session"]["name"], "Imported thread");
    // The same key and bundle: the same session and fit, nothing imported again.
    let (status, again) = bring(&host, &token, key, "", &bundle);
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["replayed"], true);
    assert_eq!(again["session"]["id"], first["session"]["id"]);
    assert_eq!(again["session"]["head"], head);
    assert_eq!(again["fit"], first["fit"]);
    assert_eq!(count(&host, "SELECT count(*) FROM sessions"), 1);
    // The same key with another bundle is refused, and so is a bad key.
    let (status, refused) = bring(&host, &token, key, "", &other);
    assert_eq!((status, refused["code"].clone()), (409, json!("conflict")));
    let (status, refused) = bring(&host, &token, "short", "", &bundle);
    assert_eq!(
        (status, refused["code"].clone()),
        (400, json!("bad_request"))
    );
    let (status, refused) = bring(&host, &token, "bad key with spaces!", "", &bundle);
    assert_eq!(status, 400, "{refused}");
    // Another client's key space is its own.
    let other_token = browser_token(&host, "chrome");
    let (status, _) = bring(&host, &other_token, key, "", &other);
    assert_eq!(status, 201);
    // Damaged or not a bundle: nothing registered, no upload left.
    let mut damaged = bundle.clone();
    let middle = damaged.len() / 2;
    damaged[middle] ^= 1;
    for (body, key) in [
        (&damaged[..], "damaged-key-00000001"),
        (b"not a bundle".as_slice(), "garbage-key-00000001"),
    ] {
        let (status, refused) = bring(&host, &token, key, "", body);
        assert_eq!(
            (status, refused["code"].clone()),
            (400, json!("bad_bundle")),
            "{refused}"
        );
    }
    nothing_registered(&host, &root, 2);
    // Too large: refused before a byte of it is read.
    let (status, _, reply) = exchange(
        &host,
        &format!(
            "POST /v1/threads HTTP/1.1\r\nHost: HOST\r\nAuthorization: Bearer {token}\r\nIdempotency-Key: large-key-0000000001\r\nContent-Length: {}\r\n\r\n",
            MAX_THREAD + 1
        ),
        b"",
        None,
    );
    assert_eq!(
        (status, json_of(&reply)["code"].clone()),
        (413, json!("too_large"))
    );
    // Only the bundle's own query options.
    let (status, _) = bring(
        &host,
        &token,
        "query-key-0000000001",
        "?colour=red",
        &bundle,
    );
    assert_eq!(status, 400);
    let (status, _) = bring(
        &host,
        &token,
        "query-key-0000000001",
        "?provider=local",
        &bundle,
    );
    assert_eq!(status, 400, "provider goes with model");
    // A full host refuses with its session limit (before the upload).
    for n in 0..(super::super::MAX_SESSIONS - 2) {
        lock(&host.lives).insert(format!("filler{n}"), Watch::new());
    }
    let (status, refused) = bring(&host, &token, "full-key-00000000001", "", &bundle);
    assert_eq!(
        (status, refused["error"].clone()),
        (409, json!("session limit reached"))
    );
    drop(host);
    let _ = super::super::disk::remove_tree(&root);
}

#[test]
fn a_cut_upload_leaves_nothing_registered() {
    let (root, host) = temp_host("cut-upload");
    let token = browser_token(&host, "edge");
    let (bundle, _, _) = bundle_of(&root, "cut", &browser_settings("local"), &["x"]);
    let (status, _, _) = exchange(
        &host,
        &format!(
            "POST /v1/threads HTTP/1.1\r\nHost: HOST\r\nAuthorization: Bearer {token}\r\nIdempotency-Key: cut-key-000000000001\r\nContent-Length: {}\r\n\r\n",
            bundle.len()
        ),
        &bundle,
        Some(bundle.len() / 2),
    );
    assert_eq!(status, 400);
    nothing_registered(&host, &root, 0);
    assert_eq!(count(&host, "SELECT count(*) FROM threads"), 0);
    // The whole bundle under the same key then imports.
    let (status, _) = bring(&host, &token, "cut-key-000000000001", "", &bundle);
    assert_eq!(status, 201);
    drop(host);
    let _ = super::super::disk::remove_tree(&root);
}

/// A host stopped at each boundary of an import: reopened, it holds the whole session
/// or nothing of it, and the same key then imports it once.
#[test]
fn an_import_stopped_at_any_point_is_whole_or_absent_after_a_restart() {
    let (root, host) = temp_host("import-cuts");
    let token = browser_token(&host, "edge");
    let (bundle, head, _) = bundle_of(&root, "cuts", &browser_settings("local"), &["x"]);
    drop(host);
    for point in ["pending", "staged", "renamed", "before-commit"] {
        let host = reopen(&root);
        lock(&host.cuts).push(point);
        let key = format!("cut-at-{point}-000000000");
        let (status, _) = bring(&host, &token, &key, "", &bundle);
        assert_eq!(status, 500, "{point}");
        let id: String = lock(&host.db)
            .query_row("SELECT session FROM threads WHERE key=?1", [&key], |row| {
                row.get(0)
            })
            .unwrap();
        // As a stop leaves it: the pending record and what was written so far.
        assert_eq!(
            host.dir(&id).exists(),
            matches!(point, "renamed" | "before-commit"),
            "{point}"
        );
        assert!(host.live(&id).is_err());
        drop(host);
        let host = reopen(&root);
        nothing_registered(&host, &root, 0);
        assert!(
            !host.dir(&id).exists(),
            "{point}: a half-made session stays"
        );
        settle(&root.join("trash").join(&id));
        assert_eq!(count(&host, "SELECT count(*) FROM threads"), 0, "{point}");
        // Not imported: the client sends it again, and gets one session.
        let (status, body) = bring(&host, &token, &key, "", &bundle);
        assert_eq!(status, 201, "{point}: {body}");
        assert_eq!(body["session"]["head"], head);
        let (status, again) = bring(&host, &token, &key, "", &bundle);
        assert_eq!(
            (status, &again["session"]["id"]),
            (200, &body["session"]["id"])
        );
        let id = body["session"]["id"].as_str().unwrap().to_string();
        let (status, receipt) = post(
            &host,
            "/v1/commands",
            &token,
            &json!({"request_id": format!("del-{point}"), "kind": "delete", "session": id}),
        );
        assert_eq!(status, 202, "{receipt}");
        settle(&root.join("trash").join(&id));
        drop(host);
    }
    // After the commit: the session, through a restart, and the key answers with it.
    let host = reopen(&root);
    let (status, body) = bring(&host, &token, "after-commit-0000000", "", &bundle);
    assert_eq!(status, 201);
    drop(host);
    let host = reopen(&root);
    let (status, again) = bring(&host, &token, "after-commit-0000000", "", &bundle);
    assert_eq!(
        (status, &again["session"]["id"]),
        (200, &body["session"]["id"])
    );
    let id = body["session"]["id"].as_str().unwrap();
    let (status, session) = get(&host, &format!("/v1/sessions/{id}"), &token);
    assert_eq!((status, session["head"].clone()), (200, json!(head)));
    assert_eq!(count(&host, "SELECT count(*) FROM sessions"), 1);
    drop(host);
    let _ = super::super::disk::remove_tree(&root);
}

#[test]
fn exports_refuse_busy_threads_and_never_send_a_stale_file() {
    let (root, host) = temp_host("export");
    let token = browser_token(&host, "edge");
    let (bundle, head, _) = bundle_of(&root, "e", &browser_settings("local"), &["x"]);
    let (_, body) = bring(&host, &token, "export-key-000000001", "", &bundle);
    let id = body["session"]["id"].as_str().unwrap().to_string();
    // Busy: a running command (or another export) refuses it.
    host.live(&id).unwrap().lock().busy = true;
    let (status, _, reply) = take(&host, &token, &id, false);
    assert_eq!(
        (status, json_of(&reply)["code"].clone()),
        (409, json!("busy"))
    );
    host.live(&id).unwrap().lock().busy = false;
    // A copy: the session stays, nothing is left behind.
    let (status, _, copy) = take(&host, &token, &id, true);
    assert_eq!(status, 200);
    assert_eq!(spindle_store::bundle::read(&copy).unwrap().head, head);
    assert!(!super::moved(&host.dir(&id)));
    assert!(
        std::fs::read_dir(root.join("incoming"))
            .unwrap()
            .next()
            .is_none()
    );
    // What an export stopped before its seal leaves (an older head's file): replaced.
    std::fs::write(host.dir(&id).join(MOVED_FILE), &copy).unwrap();
    let newer = turn(&host.dir(&id), "newer");
    drop(host);
    let host = reopen(&root);
    assert!(
        !super::moved(&host.dir(&id)),
        "the session stays live and unsealed"
    );
    let (status, reply, bytes) = take(&host, &token, &id, false);
    assert_eq!(status, 200);
    assert_eq!(
        header(&reply, "x-spindle-head"),
        Some(newer.to_string().as_str())
    );
    assert_eq!(spindle_store::bundle::read(&bytes).unwrap().head, newer);
    assert_ne!(bytes, copy);
    // Unknown fields, and keep that is not a boolean.
    let (status, _) = post(
        &host,
        &format!("/v1/sessions/{id}/export"),
        &token,
        &json!({"keep": 1}),
    );
    assert_eq!(status, 400);
    let (status, _) = post(
        &host,
        &format!("/v1/sessions/{id}/export"),
        &token,
        &json!({"x": 1}),
    );
    assert_eq!(status, 400);
    // A moved session's delete cut after its directory went to trash: the row stays,
    // and the reopened host puts the directory (bundle and seal) back.
    std::fs::create_dir_all(root.join("trash")).unwrap();
    std::fs::rename(host.dir(&id), root.join("trash").join(&id)).unwrap();
    drop(host);
    let host = reopen(&root);
    assert!(super::moved(&host.dir(&id)));
    let (status, _, again) = take(&host, &token, &id, false);
    assert_eq!((status, again == bytes), (200, true));
    drop(host);
    let _ = super::super::disk::remove_tree(&root);
}

#[test]
fn a_thread_on_a_model_the_host_cannot_send_to_asks_for_one() {
    let (root, host) = temp_host("fit");
    let token = browser_token(&host, "edge");
    let (bundle, _, _) = bundle_of(&root, "f", &browser_settings("no-such-provider"), &["x"]);
    let (status, refused) = bring(&host, &token, "fit-key-000000000001", "", &bundle);
    assert_eq!(
        (status, refused["code"].clone()),
        (409, json!("no_credentials")),
        "{refused}"
    );
    nothing_registered(&host, &root, 0);
    // A model the host does not have is the client's mistake.
    let (status, refused) = bring(
        &host,
        &token,
        "fit-key-000000000001",
        "?provider=no-such-provider&model=x",
        &bundle,
    );
    assert_eq!(
        (status, refused["code"].clone()),
        (400, json!("bad_request"))
    );
    let (provider, model) = local();
    let (status, body) = bring(
        &host,
        &token,
        "fit-key-000000000001",
        &format!("?provider={provider}&model={model}"),
        &bundle,
    );
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["fit"]["changes"]["provider"], provider.as_str());
    assert!(
        body["fit"]["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note["setting"] == "provider" && note["from"] == "no-such-provider")
    );
    let id = body["session"]["id"].as_str().unwrap();
    let settings = Settings::load(&host.dir(id)).unwrap();
    assert_eq!((settings.provider, settings.model), (provider, model));
    drop(host);
    let _ = super::super::disk::remove_tree(&root);
}

#[test]
fn browser_tokens_reach_only_their_own_threads() {
    let (root, host) = temp_host("scope");
    let token = browser_token(&host, "edge");
    let (bundle, _, _) = bundle_of(&root, "s", &browser_settings("local"), &["x"]);
    let (_, body) = bring(&host, &token, "scope-key-0000000001", "", &bundle);
    let mine = body["session"]["id"].as_str().unwrap().to_string();
    // The phone's own thread.
    lock(&host.db)
        .execute("INSERT INTO sessions VALUES('phone','phone thread',0)", [])
        .unwrap();
    std::fs::create_dir_all(host.dir("phone")).unwrap();
    drop(spindle_store::Store::open(&host.dir("phone").join("store")).unwrap());
    lock(&host.lives).insert("phone".into(), Watch::new());
    let (_, phone) = host.pair("Linux · Firefox").unwrap();
    host.submit(
        "phone",
        json!({"request_id": "phone-rename", "kind": "rename", "name": "p"}),
    )
    .unwrap();
    let (_, listed) = get(&host, "/v1/sessions", &token);
    let ids: Vec<&str> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert_eq!(ids, [mine.as_str()]);
    // The phone (a paired screen) sees both.
    let (_, listed) = get(&host, "/v1/sessions", &phone);
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 2);
    for path in [
        "/v1/sessions/phone",
        "/v1/sessions/phone/messages",
        "/v1/receipts/phone-rename",
    ] {
        let (status, body) = get(&host, path, &token);
        assert_eq!(
            (status, body["code"].clone()),
            (404, json!("not_found")),
            "{path}"
        );
    }
    assert_eq!(
        get(&host, &format!("/v1/sessions/{mine}/messages"), &token).0,
        200
    );
    let (status, body) = post(
        &host,
        "/v1/commands",
        &token,
        &json!({"request_id": "x1", "kind": "rename", "session": "phone", "name": "mine now"}),
    );
    assert_eq!((status, body["code"].clone()), (404, json!("not_found")));
    let (status, body) = post(
        &host,
        "/v1/commands",
        &token,
        &json!({"request_id": "phone-rename", "kind": "rename", "session": "phone", "name": "p"}),
    );
    assert_eq!(status, 404, "another's receipt is not told: {body}");
    let (status, body) = post(
        &host,
        "/v1/commands",
        &token,
        &json!({"request_id": "x2", "kind": "new", "provider": "local", "text": "hi"}),
    );
    assert_eq!((status, body["code"].clone()), (403, json!("forbidden")));
    let (status, body) = post(
        &host,
        "/v1/commands",
        &token,
        &json!({"request_id": "x3", "kind": "rename", "session": mine, "name": "renamed"}),
    );
    assert_eq!(status, 202, "{body}");
    assert_eq!(get(&host, "/v1/receipts/x3", &token).0, 200);
    for path in ["/v1/usage", "/v1/sessions/phone/export"] {
        let status = if path.ends_with("export") {
            post(&host, path, &token, &json!({})).0
        } else {
            get(&host, path, &token).0
        };
        assert!(status == 403 || status == 404, "{path}: {status}");
    }
    assert_eq!(get(&host, "/v1/usage", &token).0, 403);
    for path in ["/v1", "/v1/models", "/v1/providers"] {
        assert_eq!(get(&host, path, &token).0, 200, "{path}");
    }
    // No screen: /api refuses it as a cookie.
    let (status, _, _) = exchange(
        &host,
        &format!("GET /api/sessions HTTP/1.1\r\nHost: HOST\r\nCookie: spindle={token}\r\n\r\n"),
        b"",
        None,
    );
    assert_eq!(status, 403);
    // Minted again under its name: the old token is gone, the threads stay its.
    let replaced = browser_token(&host, "edge");
    assert_eq!(get(&host, "/v1/sessions", &token).0, 401);
    let (_, listed) = get(&host, "/v1/sessions", &replaced);
    assert_eq!(listed["sessions"][0]["id"], mine.as_str());
    assert_eq!(
        count(
            &host,
            "SELECT count(*) FROM devices WHERE id LIKE 'api:browser:%'"
        ),
        1
    );
    // One name, one token: an API token's name is not a browser token's, and back.
    super::super::v1::token(root.clone(), vec!["new".into(), "ci".into()]).unwrap();
    assert!(connect::mint(&mut lock(&host.db), "ci").is_err());
    assert!(super::super::v1::token(root.clone(), vec!["new".into(), "edge".into()]).is_err());
    super::super::v1::token(
        root.clone(),
        vec!["new".into(), "laptop tab".into(), "--browser".into()],
    )
    .unwrap();
    assert!(host.paired("api:browser:laptop tab"));
    super::super::v1::token(root.clone(), vec!["revoke".into(), "laptop tab".into()]).unwrap();
    assert!(!host.paired("api:browser:laptop tab"));
    // The cap counts browser tokens; replacing one is not another.
    for n in 0..14 {
        super::super::v1::token(root.clone(), vec!["new".into(), format!("t{n}")]).unwrap();
    }
    assert!(connect::mint(&mut lock(&host.db), "one more").is_err());
    assert!(connect::mint(&mut lock(&host.db), "edge").is_ok());
    drop(host);
    let _ = super::super::disk::remove_tree(&root);
}

/// The connect page, its answer and the trade of a code for a token.
#[test]
fn one_click_connect_trades_a_single_use_code_for_a_browser_token() {
    use sha2::{Digest, Sha256};
    let (root, host) = temp_host("connect");
    let verifier = "v".repeat(43);
    let challenge = connect::base64url(&Sha256::digest(verifier.as_bytes()));
    let page = |cookie: &str, query: &str| {
        let (status, _, body) = exchange(
            &host,
            &format!("GET /connect?{query} HTTP/1.1\r\nHost: HOST\r\n{cookie}\r\n"),
            b"",
            None,
        );
        (status, String::from_utf8_lossy(&body).into_owned())
    };
    let query = format!("client=%3Cb%3EEdge%3C%2Fb%3E&challenge={challenge}&state=s-1");
    // Not paired: the page pairs first; the name is shown as text.
    let (status, unpaired) = page("", &query);
    assert_eq!(status, 200);
    assert!(
        unpaired.contains(r#"<section id="connect-pair" >"#),
        "{unpaired}"
    );
    assert!(unpaired.contains(r#"<section id="connect-ask" hidden>"#));
    assert!(unpaired.contains("&lt;b&gt;Edge&lt;/b&gt;") && !unpaired.contains("<b>Edge"));
    let (_, device) = host.pair("Linux · Chrome").unwrap();
    let cookie = format!("Cookie: spindle={device}\r\n");
    let (status, paired) = page(&cookie, &query);
    assert_eq!(status, 200);
    assert!(paired.contains(r#"<section id="connect-pair" hidden>"#));
    assert!(paired.contains(r#"<section id="connect-ask" >"#));
    // Opened by the app in a tab of its own: a cross-site top-level navigation, which
    // carries no SameSite cookie (the page asks again same-origin). Framed, refused.
    let opened = |dest: &str| {
        page(
            &format!(
                "Sec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: {dest}\r\n"
            ),
            &query,
        )
    };
    let (status, tab) = opened("document");
    assert_eq!(status, 200);
    assert!(tab.contains(r#"<section id="connect-pair" >"#));
    assert_eq!(opened("iframe").0, 403);
    let (status, _, _) = exchange(
        &host,
        "GET /connect/done?error=denied&state=s-1 HTTP/1.1\r\nHost: HOST\r\nSec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\n\r\n",
        b"",
        None,
    );
    assert_eq!(status, 200);
    let (status, _, _) = exchange(
        &host,
        "GET /api/sessions HTTP/1.1\r\nHost: HOST\r\nSec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\n\r\n",
        b"",
        None,
    );
    assert_eq!(status, 403, "other pages stay same-site only");
    assert_eq!(page(&cookie, "client=Edge&challenge=short&state=s").0, 400);
    assert_eq!(
        page(&cookie, &format!("client=Edge&challenge={challenge}")).0,
        400
    );
    // The answer: a paired screen's same-origin POST only.
    let answer = |allow: bool, client: &str, csrf: bool| {
        let body =
            json!({"client": client, "challenge": challenge, "state": "s-1", "allow": allow})
                .to_string();
        let proof = if csrf {
            "Origin: http://HOST\r\nX-Spindle: 1\r\n"
        } else {
            ""
        };
        let (status, _, reply) = exchange(
            &host,
            &format!(
                "POST /api/connect HTTP/1.1\r\nHost: HOST\r\nCookie: spindle={device}\r\n{proof}Content-Length: {}\r\n\r\n",
                body.len()
            ),
            body.as_bytes(),
            None,
        );
        (status, json_of(&reply))
    };
    assert_eq!(answer(true, "Edge", false).0, 403);
    let (status, denied) = answer(false, "Edge", true);
    assert_eq!(
        (status, denied["redirect"].clone()),
        (200, json!("/connect/done?error=denied&state=s-1"))
    );
    let code_of = |reply: &Value| -> String {
        let redirect = reply["redirect"].as_str().unwrap();
        assert!(
            redirect.starts_with("/connect/done?code=") && redirect.ends_with("&state=s-1"),
            "{redirect}"
        );
        redirect["/connect/done?code=".len()..redirect.len() - "&state=s-1".len()].to_string()
    };
    let (status, allowed) = answer(true, "Edge", true);
    assert_eq!(status, 200);
    let code = code_of(&allowed);
    assert_eq!(code.len(), 64);
    let (status, done, body) = exchange(
        &host,
        &format!("GET /connect/done?code={code}&state=s-1 HTTP/1.1\r\nHost: HOST\r\n\r\n"),
        b"",
        None,
    );
    assert_eq!(status, 200, "{done}");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("Connected.") && !body.contains(&code));
    // A wrong verifier burns the code.
    let trade = |code: &str, verifier: &str| {
        post(
            &host,
            "/v1/connect/token",
            "",
            &json!({"code": code, "verifier": verifier}),
        )
    };
    let (status, refused) = trade(&code, &"w".repeat(43));
    assert_eq!((status, refused["code"].clone()), (400, json!("bad_code")));
    assert_eq!(trade(&code, &verifier).0, 400, "burned");
    // The right one, once.
    let code = code_of(&answer(true, "Edge", true).1);
    let (status, minted) = trade(&code, &verifier);
    assert_eq!(status, 200, "{minted}");
    assert_eq!(minted["client"], "Edge");
    let token = minted["token"].as_str().unwrap().to_string();
    assert_eq!(get(&host, "/v1", &token).0, 200);
    assert_eq!(get(&host, "/v1/usage", &token).0, 403, "a browser token");
    assert_eq!(trade(&code, &verifier).0, 400, "single use");
    // Connecting again replaces the token.
    let code = code_of(&answer(true, "Edge", true).1);
    let (_, again) = trade(&code, &verifier);
    assert_eq!(get(&host, "/v1", &token).0, 401);
    assert_eq!(get(&host, "/v1", again["token"].as_str().unwrap()).0, 200);
    // Expired after 120 s.
    let code = code_of(&answer(true, "Edge", true).1);
    lock(&host.connect).age(Duration::from_secs(121));
    assert_eq!(trade(&code, &verifier).0, 400);
    // Failures are limited: ten a minute, then 429 even for a good code.
    let code = code_of(&answer(true, "Edge", true).1);
    // Four so far (wrong, burned, used, expired).
    for _ in 0..6 {
        assert_eq!(trade("nope", &verifier).0, 400);
    }
    let (status, limited) = trade(&code, &verifier);
    assert_eq!(
        (status, limited["code"].clone()),
        (429, json!("rate_limited"))
    );
    drop(host);
    let _ = super::super::disk::remove_tree(&root);
}
