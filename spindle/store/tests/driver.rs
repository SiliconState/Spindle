#![cfg(feature = "driver")]
use dext_core::model::{Encoding, Identity, PreparedRequest};
use dext_core::recovery::Contract;
use dext_core::turn::Stop;
use dext_transcript::checkpoint::NativeResponse;
use dext_transcript::turn::{Effect, TurnContinuation};
use dext_transcript::{Block, Usage};
use serde_json::{Value, json};
use spindle_store::Store;
use spindle_store::broker::{AppReservation, DispatchOutcome, ModelReservation};
use spindle_store::completion::Attempt;
use spindle_store::driver::app_tools::AppAnswer;
use spindle_store::driver::{
    self, Answer, Driver, Env, Event, Inbox, Need, Observer, Settings, Start, ToolNeed,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static SERIAL: AtomicU64 = AtomicU64::new(0);
struct Realm(PathBuf);
impl Realm {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "spindle-driver-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        )))
    }
}
impl Drop for Realm {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[derive(Default)]
struct Facts {
    events: Mutex<Vec<&'static str>>,
}
impl Inbox for Facts {
    fn next_steer(&self) -> Option<String> {
        None
    }
    fn take_state(&self) -> Vec<Value> {
        vec![]
    }
    fn return_steering(&self, _: Vec<String>) {}
    fn take_steering(&self) -> Vec<String> {
        vec![]
    }
    fn interrupted(&self) -> bool {
        false
    }
}
impl Observer for Facts {
    fn event(&self, event: Event<'_>) {
        self.events.lock().unwrap().push(match event {
            Event::Prompt { .. } => "prompt",
            Event::ModelStep { .. } => "model_step",
            Event::ModelText { .. } => "model_text",
            Event::Usage { .. } => "usage",
            Event::ResponseReused => "response_reused",
            Event::ToolCall { .. } => "tool_call",
            Event::ToolResult { .. } => "tool_result",
            Event::ToolReused => "tool_reused",
            Event::State { .. } => "state",
            Event::Stopped { .. } => "stopped",
            Event::AppToolDetails { .. } => "app_tool_details",
            Event::ProviderRetry { .. } => "provider_retry",
            _ => "other",
        });
    }
}
impl Env for Facts {
    fn route(
        &self,
        provider: &str,
        model: Option<&str>,
        _: driver::Effort,
        learned: Option<u32>,
    ) -> driver::Result<driver::Route> {
        Ok(driver::Route {
            provider: provider.into(),
            model: model.unwrap_or("model").into(),
            endpoint: "https://example.invalid/responses".into(),
            contract: Contract::OpenAiResponses,
            history_budget: 512 * 1024,
            reply_reserve: 1024,
            sent_tokens: learned.unwrap_or(4096),
        })
    }
    fn request(
        &self,
        state: &TurnContinuation,
        settings: &Settings,
        _: Option<u32>,
    ) -> driver::Result<PreparedRequest<Vec<u8>>> {
        PreparedRequest::new(
            Identity {
                effect: state.next_model_id().map_err(|e| format!("{e:?}"))?,
                provider: settings.provider.clone(),
                contract: "openai-responses".into(),
                endpoint: "https://example.invalid/responses".into(),
                model: settings.model.clone(),
                credential_scope: "operator/credential-v1".into(),
                configuration: "scripted-v1".into(),
            },
            Encoding::Json,
            serde_json::to_vec(&state.transcript().map_err(|e| format!("{e:?}"))?.messages)
                .unwrap(),
        )
        .map_err(|e| format!("{e:?}"))
    }
    fn refusal(&self, _: &Settings, _: &str, _: &Value) -> Option<String> {
        None
    }
    fn app_dispatch(
        &self,
        _: &Settings,
        name: &str,
        _: Option<&Attempt>,
    ) -> Option<driver::AppDispatch> {
        (name == "lookup").then_some(driver::AppDispatch {
            helper: [17; 32],
            rerun: false,
            timeout: Duration::from_secs(600),
            max_output: driver::app_tools::DEFAULT_MAX_OUTPUT,
        })
    }
    fn tasks_offered(&self, _: &Settings) -> bool {
        false
    }
    fn task_place(&self, id: &str) -> String {
        id.into()
    }
    fn budget_refusal(
        &self,
        _: &Store,
        _: &PreparedRequest<Vec<u8>>,
    ) -> driver::Result<Option<String>> {
        Ok(None)
    }
    fn counted(&self, _: &Store, _: driver::Counted<'_>) -> Option<driver::Cost> {
        None
    }
}
fn driver(facts: &Arc<Facts>) -> Driver {
    Driver::new(facts.clone(), facts.clone(), facts.clone())
}
fn setup() -> (Realm, Store, Arc<Facts>, Driver) {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    store.set_metadata("settings", &serde_json::to_vec(&json!({"provider":"fake", "model":"model", "builtin_tools":"none", "compaction":"digest", "tools":[{"name":"lookup", "description":"Look up a note"}]})).unwrap()).unwrap();
    let facts = Arc::new(Facts::default());
    let driver = driver(&facts);
    (realm, store, facts, driver)
}
fn send(driver: &mut Driver, store: &mut Store) -> Box<driver::ModelNeed> {
    for _ in 0..20 {
        match driver.next(store).unwrap() {
            Need::Continue => {}
            Need::Send(need) => return need,
            _ => panic!("expected model need"),
        }
    }
    panic!("driver made no progress")
}
fn record(store: &mut Store, need: &driver::ModelNeed, blocks: Vec<Block>) -> DispatchOutcome {
    let ModelReservation::Reserved(committed) = store
        .reserve_turn_model(&need.request, &false.into(), |_| Ok(()))
        .unwrap()
    else {
        panic!("fresh reservation")
    };
    let Effect::Model { waiting, .. } = committed.effect() else {
        panic!("model effect")
    };
    let response = NativeResponse {
        blocks,
        stop_reason: Some("stop".into()),
        usage: Usage::default(),
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
    store
        .ingest_turn_model(&need.request, &committed, recorded.checkpoint())
        .unwrap()
}
fn deliver(
    driver: &mut Driver,
    store: &mut Store,
    need: Box<driver::ModelNeed>,
    outcome: DispatchOutcome,
) {
    assert!(matches!(
        driver
            .deliver(
                store,
                Answer::Model {
                    need,
                    sent: driver::ModelSent {
                        outcome: Ok(outcome),
                        streamed: false,
                        cut: None,
                        rejection: None
                    }
                }
            )
            .unwrap(),
        Need::Continue
    ));
}
fn finish(driver: &mut Driver, store: &mut Store) {
    for _ in 0..20 {
        match driver.next(store).unwrap() {
            Need::Continue => {}
            Need::Stopped(Stop::Complete) => {
                driver.report_stop(store).unwrap();
                return;
            }
            _ => panic!("expected completed turn"),
        }
    }
    panic!("driver made no progress")
}
fn text(text: &str) -> Block {
    Block::Text { text: text.into() }
}
fn start(driver: &mut Driver, store: &mut Store) {
    assert!(matches!(
        driver
            .start(
                store,
                "go".into(),
                Start::Import(vec![], vec![], vec![]),
                None
            )
            .unwrap(),
        Need::Continue
    ));
}
fn app(driver: &mut Driver, store: &mut Store) -> Box<driver::AppNeed> {
    let Need::Tool(ToolNeed::Ask(need)) = driver.next(store).unwrap() else {
        panic!("app need")
    };
    need
}
fn call() -> Block {
    Block::ToolUse {
        id: "call-1".into(),
        name: "lookup".into(),
        input: json!({"q":"note"}),
    }
}

#[test]
fn an_admitted_start_replays_identical_bytes_after_settings_change() {
    let (_reference_realm, mut reference, _, mut original) = setup();
    start(&mut original, &mut reference);
    let payload = reference.admitted("operator", "start").unwrap().unwrap();
    let spindle_store::turn::Action::Start { request, .. } =
        spindle_store::turn::Action::decode(&payload).unwrap()
    else {
        panic!("start action")
    };
    let expected = dext_core::model::ModelWait::restore(&request)
        .unwrap()
        .start()
        .unwrap()
        .checkpoint();
    let (_realm, mut store, _, mut engine) = setup();
    let lineage = store.lineage().unwrap();
    store
        .admit(&spindle_store::Command {
            scope: "operator".into(),
            id: "start".into(),
            lineage,
            kind: "dext-turn-v1".into(),
            payload,
        })
        .unwrap();
    let mut settings: Value =
        serde_json::from_slice(&store.metadata("settings").unwrap().unwrap()).unwrap();
    settings["model"] = json!("changed-model");
    store
        .set_metadata("settings", &serde_json::to_vec(&settings).unwrap())
        .unwrap();
    start(&mut engine, &mut store);
    let need = send(&mut engine, &mut store);
    assert_eq!(need.request.intent.input, expected);
    let outcome = record(&mut store, &need, vec![text("done")]);
    deliver(&mut engine, &mut store, need, outcome);
    finish(&mut engine, &mut store);
    store.check_integrity().unwrap();
}

#[test]
#[cfg(feature = "agent")]
fn an_admitted_prompt_blocks_automatic_wake_until_original_input_is_resubmitted() {
    let (_reference_realm, mut reference, _, mut original) = setup();
    start(&mut original, &mut reference);
    let payload = reference.admitted("operator", "start").unwrap().unwrap();
    let realm = Realm::new();
    std::fs::create_dir_all(&realm.0).unwrap();
    let mut store = Store::open(&realm.0.join("store")).unwrap();
    store
        .set_metadata(
            "settings",
            &reference.metadata("settings").unwrap().unwrap(),
        )
        .unwrap();
    let lineage = store.lineage().unwrap();
    store
        .admit(&spindle_store::Command {
            scope: "operator".into(),
            id: "start".into(),
            lineage,
            kind: spindle_store::turn::ENGINE.into(),
            payload,
        })
        .unwrap();
    let report = spindle_store::api::recovery(&realm.0).unwrap();
    assert_eq!(report["wake"]["work"], "blocked", "{report}");
    assert!(
        report["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "prompt" && item["next"] == "prompt")
    );
    assert_eq!(store.snapshot().unwrap().revision, 0);
}

#[test]
fn an_admitted_prepare_replays_identical_bytes_after_settings_change() {
    let (realm, mut store, facts, mut engine) = setup();
    start(&mut engine, &mut store);
    let need = send(&mut engine, &mut store);
    let outcome = record(&mut store, &need, vec![call()]);
    deliver(&mut engine, &mut store, need, outcome);
    let need = app(&mut engine, &mut store);
    engine.reserve_app(&mut store, &need).unwrap();
    let outcome = engine
        .record_app(
            &mut store,
            &need,
            AppAnswer::from_json(&json!({"content":"found"})).unwrap(),
        )
        .unwrap();
    engine
        .deliver(
            &mut store,
            Answer::App {
                need,
                outcome: Ok(outcome),
                waiting: None,
            },
        )
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    let before = TurnContinuation::restore(&snapshot.continuation).unwrap();
    let settings = Settings::from_json(
        &serde_json::from_slice(&store.metadata("settings").unwrap().unwrap()).unwrap(),
    )
    .unwrap();
    let request = facts.request(&before, &settings, None).unwrap();
    let waiting = dext_core::model::ModelWait::new(request);
    let bytes = waiting.checkpoint();
    let expected_intent = waiting.start().unwrap().checkpoint();
    let lineage = store.lineage().unwrap();
    store
        .admit(&spindle_store::Command {
            scope: "operator".into(),
            id: format!("prepare-r{}", snapshot.revision),
            lineage,
            kind: "dext-turn-v1".into(),
            payload: spindle_store::turn::Action::Prepare {
                request: bytes.clone(),
            }
            .encode()
            .unwrap(),
        })
        .unwrap();
    let mut settings = settings.to_json().unwrap();
    settings["model"] = json!("changed-model");
    store
        .set_metadata("settings", &serde_json::to_vec(&settings).unwrap())
        .unwrap();
    drop(store);
    store = Store::open(&realm.0).unwrap();
    engine = driver(&facts);
    assert!(matches!(engine.resume(&mut store).unwrap(), Need::Continue));
    let need = send(&mut engine, &mut store);
    assert_eq!(need.request.intent.input, expected_intent);
    let outcome = record(&mut store, &need, vec![text("done")]);
    deliver(&mut engine, &mut store, need, outcome);
    finish(&mut engine, &mut store);
    store.check_integrity().unwrap();
}

#[test]
fn a_recorded_reply_reopens_and_is_adopted_without_a_transport() {
    let (realm, mut store, facts, mut engine) = setup();
    start(&mut engine, &mut store);
    let need = send(&mut engine, &mut store);
    let body = need.request.intent.input.clone();
    record(&mut store, &need, vec![text("done")]);
    drop(store);
    store = Store::open(&realm.0).unwrap();
    engine = driver(&facts);
    let Need::Send(need) = engine.resume(&mut store).unwrap() else {
        panic!("saved model need")
    };
    assert!(need.route.is_none());
    assert_eq!(need.request.intent.input, body);
    let ModelReservation::Settled(outcome @ DispatchOutcome::Reused(_)) = store
        .reserve_turn_model(&need.request, &false.into(), |_| Ok(()))
        .unwrap()
    else {
        panic!("receipt reuse")
    };
    deliver(&mut engine, &mut store, need, outcome);
    finish(&mut engine, &mut store);
    assert_eq!(
        *facts.events.lock().unwrap(),
        [
            "prompt",
            "model_step",
            "response_reused",
            "model_text",
            "usage",
            "stopped"
        ]
    );
    store.check_integrity().unwrap();
}

#[test]
fn an_unknown_first_send_requires_the_host_to_replace_the_store() {
    let (realm, mut store, facts, mut engine) = setup();
    start(&mut engine, &mut store);
    let need = send(&mut engine, &mut store);
    assert!(matches!(
        store
            .reserve_turn_model(&need.request, &false.into(), |_| Ok(()))
            .unwrap(),
        ModelReservation::Reserved(_)
    ));
    drop(store);
    store = Store::open(&realm.0).unwrap();
    engine = driver(&facts);
    let need = send(&mut engine, &mut store);
    let ModelReservation::Settled(outcome @ DispatchOutcome::Reconcile) = store
        .reserve_turn_model(&need.request, &false.into(), |_| Ok(()))
        .unwrap()
    else {
        panic!("unknown request")
    };
    let error = engine
        .deliver(
            &mut store,
            Answer::Model {
                need,
                sent: driver::ModelSent {
                    outcome: Ok(outcome),
                    streamed: false,
                    cut: None,
                    rejection: None,
                },
            },
        )
        .err()
        .unwrap();
    assert!(error.contains("unknown outcome") && error.contains("spindle retry"));
    assert!(
        matches!(engine.retry(&mut store).unwrap(), driver::Retry::FreshStore { prompt } if prompt == "go")
    );
}

#[test]
fn app_answers_keep_files_and_state_together_and_reuse_after_reopen() {
    let (realm, mut store, facts, mut engine) = setup();
    start(&mut engine, &mut store);
    let need = send(&mut engine, &mut store);
    let outcome = record(&mut store, &need, vec![call()]);
    deliver(&mut engine, &mut store, need, outcome);
    let need = app(&mut engine, &mut store);
    let id = need.id.clone();
    assert!(matches!(
        engine.reserve_app(&mut store, &need).unwrap(),
        AppReservation::Reserved { again: false, .. }
    ));
    engine
        .record_app(
            &mut store,
            &need,
            AppAnswer::Result {
                content: "found".into(),
                failed: false,
                writes: vec![("note.txt".into(), b"note".to_vec(), false)],
                deletes: vec![],
                links: vec![],
                directories: vec![],
                state: Some(json!({"found":true})),
                details: Some(json!({"private":"not-for-model"})),
            },
        )
        .unwrap();
    drop(store);
    store = Store::open(&realm.0).unwrap();
    engine = driver(&facts);
    let need = app(&mut engine, &mut store);
    assert_eq!(need.id, id);
    let AppReservation::Settled(outcome @ DispatchOutcome::Reused(_)) =
        engine.reserve_app(&mut store, &need).unwrap()
    else {
        panic!("saved app answer")
    };
    engine
        .deliver(
            &mut store,
            Answer::App {
                need,
                outcome: Ok(outcome),
                waiting: None,
            },
        )
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    assert_eq!(
        store.app_state("operator", snapshot.revision).unwrap(),
        br#"{"found":true}"#
    );
    let root = snapshot.workspace.unwrap();
    let tree = store.tree_map("operator", &root).unwrap();
    assert_eq!(tree.files[0].path, "note.txt");
    assert_eq!(
        store
            .read_object("operator", &tree.files[0].object)
            .unwrap(),
        b"note"
    );
    let need = send(&mut engine, &mut store);
    assert!(
        !need
            .request
            .intent
            .input
            .windows(b"not-for-model".len())
            .any(|window| window == b"not-for-model")
    );
    assert!(facts.events.lock().unwrap().contains(&"app_tool_details"));
    let outcome = record(&mut store, &need, vec![text("done")]);
    deliver(&mut engine, &mut store, need, outcome);
    finish(&mut engine, &mut store);
    store.check_integrity().unwrap();
}

#[test]
fn app_output_limit_and_store_incarnation_survive_reopen() {
    let (realm, mut store, _facts, mut engine) = setup();
    let (_, other, _, _) = setup();
    assert_ne!(
        store.call_id("same-effect").unwrap(),
        other.call_id("same-effect").unwrap()
    );
    start(&mut engine, &mut store);
    let need = send(&mut engine, &mut store);
    let outcome = record(&mut store, &need, vec![call()]);
    deliver(&mut engine, &mut store, need, outcome);
    let mut need = app(&mut engine, &mut store);
    need.app.max_output = 4096;
    let external = need.call_id.clone();
    let attempt = need.request.attempt.id.clone();
    engine.reserve_app(&mut store, &need).unwrap();
    let outcome = engine
        .record_app(
            &mut store,
            &need,
            AppAnswer::Result {
                content: "hello".repeat(4000),
                failed: false,
                writes: vec![],
                deletes: vec![],
                links: vec![],
                directories: vec![],
                state: None,
                details: Some(json!({"ui":"private-value"})),
            },
        )
        .unwrap();
    let DispatchOutcome::Recorded(bytes) = outcome else {
        panic!("fresh answer")
    };
    let block: Block = serde_json::from_slice(&bytes).unwrap();
    let Block::ToolResult {
        content, metadata, ..
    } = block
    else {
        panic!("tool result")
    };
    assert!(content.len() <= 4096, "{}", content.len());
    assert!(metadata.artifact.is_some());
    drop(store);
    store = Store::open(&realm.0).unwrap();
    assert_eq!(store.call_id(&need.id).unwrap(), external);
    assert_eq!(
        serde_json::from_slice::<Value>(&store.app_details(&attempt).unwrap().unwrap()).unwrap(),
        json!({"ui":"private-value"})
    );
    store.check_integrity().unwrap();
}

#[test]
fn admitted_task_report_replays_before_a_newly_finished_earlier_task() {
    let realm = Realm::new();
    std::fs::create_dir_all(&realm.0).unwrap();
    let mut store = Store::open(&realm.0.join("store")).unwrap();
    store.set_metadata("settings", &serde_json::to_vec(&json!({"provider":"fake", "model":"model", "builtin_tools":"none", "compaction":"digest"})).unwrap()).unwrap();
    let facts = Arc::new(Facts::default());
    let mut engine = driver(&facts);
    start(&mut engine, &mut store);
    let need = send(&mut engine, &mut store);
    let outcome = record(&mut store, &need, vec![text("done")]);
    deliver(&mut engine, &mut store, need, outcome);
    finish(&mut engine, &mut store);
    let snapshot = store.snapshot().unwrap();
    let input = br#"{"prompt":"child"}"#;
    let answer = br#"{"content":"answer","failed":false}"#;
    for task in ["app:a", "app:b"] {
        store
            .register_task(&spindle_store::meta::TaskRun {
                task: task.into(),
                origin: None,
                input: input.to_vec(),
                base: snapshot.workspace.clone().unwrap(),
                revision: snapshot.revision,
                queued: true,
                state: "running".into(),
                answer: None,
                delivered: None,
                lease: None,
            })
            .unwrap();
    }
    spindle_store::meta::finish_task(
        &realm.0,
        "app:b",
        input,
        spindle_store::meta::TaskEnd::Answered(answer),
    )
    .unwrap();
    let action = spindle_store::turn::Action::TaskReport {
        task: "app:b".into(),
        text: "[task app:b: finished]\nanswer".into(),
    };
    // A crash after admission must not select a different now-finished task on reopen.
    let lineage = store.lineage().unwrap();
    store
        .admit(&spindle_store::Command {
            scope: "operator".into(),
            id: "task-report-app:b".into(),
            lineage,
            kind: spindle_store::turn::ENGINE.into(),
            payload: action.encode().unwrap(),
        })
        .unwrap();
    drop(store);
    spindle_store::meta::finish_task(
        &realm.0,
        "app:a",
        input,
        spindle_store::meta::TaskEnd::Answered(answer),
    )
    .unwrap();
    let mut store = Store::open(&realm.0.join("store")).unwrap();
    let mut engine = driver(&facts);
    assert!(matches!(engine.resume(&mut store).unwrap(), Need::Continue));
    let runs = store.task_runs().unwrap();
    assert!(runs[0].delivered.is_none());
    assert!(runs[1].delivered.is_some());
    for _ in 0..40 {
        match engine.next(&mut store).unwrap() {
            Need::Continue => {}
            Need::Send(need) => {
                let outcome = record(&mut store, &need, vec![text("received")]);
                deliver(&mut engine, &mut store, need, outcome);
            }
            Need::Stopped(Stop::Complete) => break,
            _ => panic!("unexpected task report recovery need"),
        }
    }
    let runs = store.task_runs().unwrap();
    assert!(runs[0].delivered.is_some());
    assert!(runs[1].delivered < runs[0].delivered);
    store.check_integrity().unwrap();
}

#[test]
fn a_stopped_task_reports_so_and_a_stale_admitted_report_is_dropped_once_it_goes_on() {
    let realm = Realm::new();
    std::fs::create_dir_all(&realm.0).unwrap();
    let mut store = Store::open(&realm.0.join("store")).unwrap();
    store.set_metadata("settings", &serde_json::to_vec(&json!({"provider":"fake", "model":"model", "builtin_tools":"none", "compaction":"digest"})).unwrap()).unwrap();
    let facts = Arc::new(Facts::default());
    let mut engine = driver(&facts);
    start(&mut engine, &mut store);
    let need = send(&mut engine, &mut store);
    let outcome = record(&mut store, &need, vec![text("done")]);
    deliver(&mut engine, &mut store, need, outcome);
    finish(&mut engine, &mut store);
    let snapshot = store.snapshot().unwrap();
    let input = br#"{"prompt":"child"}"#;
    store
        .register_task(&spindle_store::meta::TaskRun {
            task: "app:a".into(),
            origin: None,
            input: input.to_vec(),
            base: snapshot.workspace.clone().unwrap(),
            revision: snapshot.revision,
            queued: true,
            state: "running".into(),
            answer: None,
            delivered: None,
            lease: None,
        })
        .unwrap();
    // It stops without an answer, and a crash cuts its report off once admitted.
    spindle_store::meta::finish_task(
        &realm.0,
        "app:a",
        input,
        spindle_store::meta::TaskEnd::Stopped(None),
    )
    .unwrap();
    // Its worker's own end, after the stop, changes nothing: it stays as it was stopped.
    spindle_store::meta::finish_task(
        &realm.0,
        "app:a",
        input,
        spindle_store::meta::TaskEnd::Answered(br#"{"content":"late","failed":false}"#),
    )
    .unwrap();
    let runs = store.task_runs().unwrap();
    assert_eq!(
        (runs[0].state.as_str(), runs[0].answer.is_none()),
        ("stopped", true)
    );
    let stopped = "[task app:a: stopped]\nIt stopped before it finished and gave no answer.";
    let lineage = store.lineage().unwrap();
    store
        .admit(&spindle_store::Command {
            scope: "operator".into(),
            id: "task-report-app:a".into(),
            lineage,
            kind: spindle_store::turn::ENGINE.into(),
            payload: spindle_store::turn::Action::TaskReport {
                task: "app:a".into(),
                text: stopped.into(),
            }
            .encode()
            .unwrap(),
        })
        .unwrap();
    drop(store);
    // Before the session goes on, the task is started again: the admitted report says
    // nothing true now. It is dropped, and the turn waits for the task instead.
    let mut store = Store::open(&realm.0.join("store")).unwrap();
    store.resume_task("app:a").unwrap();
    let mut engine = driver(&facts);
    assert!(matches!(engine.resume(&mut store).unwrap(), Need::TaskWait));
    assert!(store.task_runs().unwrap()[0].delivered.is_none());
    // It finishes this time: its answer is read in, under a command of its own.
    drop(store);
    spindle_store::meta::finish_task(
        &realm.0,
        "app:a",
        input,
        spindle_store::meta::TaskEnd::Answered(br#"{"content":"answer","failed":false}"#),
    )
    .unwrap();
    let mut store = Store::open(&realm.0.join("store")).unwrap();
    let mut engine = driver(&facts);
    let mut need = engine.resume(&mut store).unwrap();
    for _ in 0..40 {
        match need {
            Need::Continue => {}
            Need::Send(sent) => {
                let outcome = record(&mut store, &sent, vec![text("received")]);
                deliver(&mut engine, &mut store, sent, outcome);
            }
            Need::Stopped(Stop::Complete) => break,
            _ => panic!("unexpected task report need"),
        }
        need = engine.next(&mut store).unwrap();
    }
    assert!(store.task_runs().unwrap()[0].delivered.is_some());
    assert!(
        store
            .admitted("operator", "task-report-app:a-2")
            .unwrap()
            .is_some()
    );
    store.check_integrity().unwrap();
}

#[test]
fn until_unknown_overrides_resend_consent_and_preserves_the_reservation() {
    let (realm, mut store, facts, mut engine) = setup();
    let mut settings: Value =
        serde_json::from_slice(&store.metadata("settings").unwrap().unwrap()).unwrap();
    settings["resend_unknown"] = json!(true);
    store
        .set_metadata("settings", &serde_json::to_vec(&settings).unwrap())
        .unwrap();
    start(&mut engine, &mut store);
    let need = send(&mut engine, &mut store);
    store
        .reserve_turn_model(&need.request, &false.into(), |_| Ok(()))
        .unwrap();
    drop(store);
    store = Store::open(&realm.0).unwrap();
    engine = driver(&facts);
    let error = engine.resume_until_unknown(&mut store).err().unwrap();
    assert!(
        error.contains("stopped at unknown outcome") && error.contains(&need.id),
        "{error}"
    );
    assert!(store.receipt(&need.request.attempt.id).unwrap().is_none());
}

#[test]
fn retry_after_is_retained_exactly_and_reused_after_reopen() {
    let (realm, mut store, facts, mut engine) = setup();
    start(&mut engine, &mut store);
    let first = send(&mut engine, &mut store);
    let outcome = record(&mut store, &first, vec![text("done")]);
    deliver(&mut engine, &mut store, first, outcome);
    finish(&mut engine, &mut store);
    start(&mut engine, &mut store);
    let need = send(&mut engine, &mut store);
    store
        .reserve_turn_model(&need.request, &false.into(), |_| Ok(()))
        .unwrap();
    let delay = Duration::from_millis(1234);
    let Need::Wait(wait) = engine
        .deliver(
            &mut store,
            Answer::Model {
                need,
                sent: driver::ModelSent {
                    outcome: Err(driver::ModelFailure::Http(429)),
                    streamed: false,
                    cut: None,
                    rejection: Some(driver::Rejection {
                        status: 429,
                        retry_after: Some(delay),
                        body: "busy".into(),
                    }),
                },
            },
        )
        .unwrap()
    else {
        panic!("backoff")
    };
    assert_eq!(wait.wait, delay);
    drop(store);
    store = Store::open(&realm.0).unwrap();
    engine = driver(&facts);
    let need = send(&mut engine, &mut store);
    assert_eq!(
        store.retry_delay(&need.request.attempt.id).unwrap(),
        Some(delay)
    );
    let ModelReservation::Settled(outcome) = store
        .reserve_turn_model(&need.request, &false.into(), |_| Ok(()))
        .unwrap()
    else {
        panic!("known refusal")
    };
    let Need::Wait(wait) = engine
        .deliver(
            &mut store,
            Answer::Model {
                need,
                sent: driver::ModelSent {
                    outcome: Ok(outcome),
                    streamed: false,
                    cut: None,
                    rejection: None,
                },
            },
        )
        .unwrap()
    else {
        panic!("backoff after restart")
    };
    assert_eq!(wait.wait, delay);
    store.check_integrity().unwrap();
}

#[test]
fn retry_policy_is_scripted_without_sleeping_or_a_network() {
    for (status, seconds) in [
        (429, vec![4, 8, 16, 32, 64, 64]),
        (driver::DROPPED, vec![1, 2, 4]),
    ] {
        let (_realm, mut store, _facts, mut engine) = setup();
        let mut settings: Value =
            serde_json::from_slice(&store.metadata("settings").unwrap().unwrap()).unwrap();
        settings["resend_unknown"] = json!(true);
        store
            .set_metadata("settings", &serde_json::to_vec(&settings).unwrap())
            .unwrap();
        start(&mut engine, &mut store);
        let need = send(&mut engine, &mut store);
        let outcome = record(&mut store, &need, vec![text("first turn")]);
        deliver(&mut engine, &mut store, need, outcome);
        finish(&mut engine, &mut store);
        start(&mut engine, &mut store);
        for expected in seconds {
            let need = send(&mut engine, &mut store);
            store
                .reserve_turn_model(&need.request, &false.into(), |_| Ok(()))
                .unwrap();
            let failure = if status == driver::DROPPED {
                driver::ModelFailure::Dropped("lost connection".into())
            } else {
                driver::ModelFailure::Http(status)
            };
            let Need::Wait(backoff) = engine
                .deliver(
                    &mut store,
                    Answer::Model {
                        need,
                        sent: driver::ModelSent {
                            outcome: Err(failure),
                            streamed: false,
                            cut: None,
                            rejection: None,
                        },
                    },
                )
                .unwrap()
            else {
                panic!("retry wait")
            };
            assert_eq!(backoff.wait.as_secs(), expected);
            let next = engine.deliver(&mut store, Answer::Waited(backoff)).unwrap();
            let Need::Start(start) = next else {
                panic!("restart turn")
            };
            engine
                .start(&mut store, start.prompt, start.start, start.state)
                .unwrap();
        }
        let need = send(&mut engine, &mut store);
        store
            .reserve_turn_model(&need.request, &false.into(), |_| Ok(()))
            .unwrap();
        let failure = if status == driver::DROPPED {
            driver::ModelFailure::Dropped("lost connection".into())
        } else {
            driver::ModelFailure::Http(status)
        };
        assert!(
            engine
                .deliver(
                    &mut store,
                    Answer::Model {
                        need,
                        sent: driver::ModelSent {
                            outcome: Err(failure),
                            streamed: false,
                            cut: None,
                            rejection: None
                        }
                    }
                )
                .is_err()
        );
    }
}

#[test]
fn a_live_drop_without_consent_stops_and_reopens_without_resending() {
    let (realm, mut store, facts, mut engine) = setup();
    start(&mut engine, &mut store);
    let need = send(&mut engine, &mut store);
    let outcome = record(&mut store, &need, vec![text("first turn")]);
    deliver(&mut engine, &mut store, need, outcome);
    finish(&mut engine, &mut store);
    start(&mut engine, &mut store);
    let need = send(&mut engine, &mut store);
    let id = need.id.clone();
    store
        .reserve_turn_model(&need.request, &false.into(), |_| Ok(()))
        .unwrap();
    let error = engine
        .deliver(
            &mut store,
            Answer::Model {
                need,
                sent: driver::ModelSent {
                    outcome: Err(driver::ModelFailure::Dropped("lost connection".into())),
                    streamed: true,
                    cut: None,
                    rejection: None,
                },
            },
        )
        .err()
        .unwrap();
    assert!(
        error.contains("unknown outcome") && error.contains("spindle retry"),
        "{error}"
    );
    assert!(!facts.events.lock().unwrap().contains(&"provider_retry"));
    drop(store);
    store = Store::open(&realm.0).unwrap();
    engine = driver(&facts);
    let need = send(&mut engine, &mut store);
    assert_eq!(need.id, id);
    assert!(need.route.is_none());
    let ModelReservation::Settled(outcome @ DispatchOutcome::Reconcile) = store
        .reserve_turn_model(&need.request, &false.into(), |_| Ok(()))
        .unwrap()
    else {
        panic!("unknown reservation must not be resent")
    };
    let error = engine
        .deliver(
            &mut store,
            Answer::Model {
                need,
                sent: driver::ModelSent {
                    outcome: Ok(outcome),
                    streamed: false,
                    cut: None,
                    rejection: None,
                },
            },
        )
        .err()
        .unwrap();
    assert!(error.contains("unknown"), "{error}");
    store.check_integrity().unwrap();
}

#[test]
fn an_unanswered_app_reservation_keeps_its_identity_after_reopen() {
    let (realm, mut store, facts, mut engine) = setup();
    start(&mut engine, &mut store);
    let need = send(&mut engine, &mut store);
    let outcome = record(&mut store, &need, vec![call()]);
    deliver(&mut engine, &mut store, need, outcome);
    let need = app(&mut engine, &mut store);
    let id = need.id.clone();
    assert!(matches!(
        engine.reserve_app(&mut store, &need).unwrap(),
        AppReservation::Reserved { again: false, .. }
    ));
    drop(store);
    store = Store::open(&realm.0).unwrap();
    engine = driver(&facts);
    let need = app(&mut engine, &mut store);
    assert_eq!(need.id, id);
    assert!(matches!(
        engine.reserve_app(&mut store, &need).unwrap(),
        AppReservation::Reserved { again: true, .. }
    ));
    let outcome = engine
        .record_app(
            &mut store,
            &need,
            AppAnswer::Unknown("host lost the answer".into()),
        )
        .unwrap();
    engine
        .deliver(
            &mut store,
            Answer::App {
                need,
                outcome: Ok(outcome),
                waiting: None,
            },
        )
        .unwrap();
    store.check_integrity().unwrap();
}

#[test]
fn a_failed_app_receipt_reports_its_outcome_before_reading_a_paused_head() {
    let (realm, mut store, _facts, mut engine) = setup();
    start(&mut engine, &mut store);
    let need = send(&mut engine, &mut store);
    let outcome = record(&mut store, &need, vec![call()]);
    deliver(&mut engine, &mut store, need, outcome);
    let need = app(&mut engine, &mut store);
    engine.reserve_app(&mut store, &need).unwrap();
    let fault = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    fault
        .execute_batch(
            "CREATE TRIGGER ignore_meta BEFORE UPDATE ON meta BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
    assert!(matches!(
        store.set_metadata("settings", b"lost"),
        Err(spindle_store::Error::Incompatible)
    ));
    assert!(matches!(
        store.snapshot(),
        Err(spindle_store::Error::Paused)
    ));
    fault.execute_batch("DROP TRIGGER ignore_meta;").unwrap();
    let error = engine
        .deliver(
            &mut store,
            Answer::App {
                need,
                outcome: Err(spindle_store::Error::Paused),
                waiting: None,
            },
        )
        .err()
        .unwrap();
    assert!(error.starts_with("the app tool call failed:"), "{error}");
    assert!(
        error.ends_with("Run `spindle continue` to ask again."),
        "{error}"
    );
}

#[test]
fn malformed_saved_settings_never_widen_isolation_or_remove_a_budget() {
    for bad in [
        json!(null),
        json!([]),
        json!({"isolation":"require"}),
        json!({"isolation":false}),
        json!({"max_usd":-1}),
        json!({"max_usd":"2.5"}),
        json!({"network":"false"}),
        json!({"resend_unknown":"true"}),
        json!({"sources":[null]}),
        json!({"instructions":true}),
        json!({"instructions":"x".repeat(32 * 1024 + 1)}),
    ] {
        assert!(
            Settings::from_json(&bad).is_err(),
            "accepted malformed stored setting"
        );
    }
    let defaults = Settings::from_json(&json!({"provider":"fake","model":"model"})).unwrap();
    assert_eq!(defaults.isolation, driver::Isolation::Auto);
    assert_eq!(defaults.max_usd, None);
}

#[test]
fn driver_sources_have_no_native_effect_dependencies() {
    for (file, source) in [
        ("driver", include_str!("../src/driver.rs")),
        ("api", include_str!("../src/driver_api.rs")),
        ("types", include_str!("../src/driver_types.rs")),
        ("summary", include_str!("../src/driver_summary.rs")),
        ("app_tools", include_str!("../src/agent/app_tools.rs")),
    ] {
        for forbidden in [
            "std::fs",
            "std::thread",
            "Instant",
            "SystemTime",
            "std::process",
            "reqwest",
            "tokio",
            "dext_native",
            "Error::Provider",
        ] {
            assert!(!source.contains(forbidden), "{file} contains {forbidden}");
        }
    }
}
