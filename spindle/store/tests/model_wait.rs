use dext_core::model::{Encoding, Identity, ModelWait, Phase, PreparedRequest};
use dext_core::response::{AssistantBlock, BlockKind, CallRef, ResponseProposal, ToolCall};

struct FixtureBlock(serde_json::Value);
impl AssistantBlock for FixtureBlock {
    type Input = Vec<u8>;
    fn kind(&self) -> BlockKind<'_> {
        match self.0["type"].as_str() {
            Some("text") if self.0["text"].is_string() => BlockKind::Content,
            Some("tool_use") if self.0["input"].is_object() => {
                match (self.0["id"].as_str(), self.0["name"].as_str()) {
                    (Some(id), Some(name)) => BlockKind::CompleteCall(CallRef { id, name }),
                    _ => BlockKind::Invalid,
                }
            }
            _ => BlockKind::Invalid,
        }
    }
    fn into_call(self) -> Option<ToolCall<Self::Input>> {
        if self.0["type"] != "tool_use" {
            return None;
        }
        Some(ToolCall {
            id: self.0["id"].as_str()?.to_owned(),
            name: self.0["name"].as_str()?.to_owned(),
            input: serde_json::to_vec(&self.0["input"]).ok()?,
        })
    }
}

fn response_proposal(bytes: &[u8]) -> Result<ResponseProposal<FixtureBlock, ()>, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let blocks = value["blocks"]
        .as_array()
        .ok_or("missing blocks")?
        .iter()
        .cloned()
        .map(FixtureBlock)
        .collect();
    ResponseProposal::from_validated(blocks, (), value["complete"] == true)
        .map_err(|e| e.to_string())
}
use spindle_store::completion::Attempt;
use spindle_store::transition::{EffectIntent, Observation, Proposal};
use spindle_store::{Command, Error, Store};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SERIAL: AtomicU64 = AtomicU64::new(0);
struct Realm(PathBuf);
impl Realm {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "spindle-model-wait-{}-{}",
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
fn setup(store: &mut Store) -> ModelWait<Vec<u8>> {
    let request = PreparedRequest::new(
        Identity {
            effect: "model-1".into(),
            provider: "fake".into(),
            contract: "test-json".into(),
            endpoint: "https://example.invalid/model".into(),
            model: "fake-model".into(),
            credential_scope: "operator/credential-version-1".into(),
            configuration: "effort=high;policy=1".into(),
        },
        Encoding::Json,
        br#"{"messages":["test"],"tools":["edit"]}"#.to_vec(),
    )
    .unwrap();
    let waiting = ModelWait::new(request).start().unwrap();
    let admission = store
        .admit(&Command {
            scope: "operator".into(),
            id: "input".into(),
            lineage: 0,
            kind: "test-model".into(),
            payload: vec![],
        })
        .unwrap();
    store
        .commit_proposal(&Proposal {
            id: "prepare-model".into(),
            scope: "operator".into(),
            observation: Observation::Admission(admission.sequence),
            expected_revision: 0,
            owner_epoch: store.epoch(),
            engine: "dext-model-wait-v1".into(),
            continuation: waiting.checkpoint(),
            workspace: None,
            effects: vec![EffectIntent {
                id: "model-1".into(),
                adapter: "fake-model".into(),
                input: waiting.checkpoint(),
            }],
            state: None,
        })
        .unwrap();
    store
        .reserve_attempt(&Attempt {
            id: "model-attempt-1".into(),
            effect: "model-1".into(),
            owner_epoch: store.epoch(),
            implementation: "fake-v1".into(),
            grant_reference: "test-only".into(),
        })
        .unwrap();
    waiting
}
fn receive(store: &mut Store, waiting: &ModelWait<Vec<u8>>) {
    let recorded = waiting
        .record(
            waiting.request(),
            1,
            br#"{"complete":true,"blocks":[{"type":"text","text":"edit requested"},{"type":"tool_use","id":"edit-1","name":"inert-test-tool","input":{"path":"test.txt"}}]}"#,
        )
        .unwrap();
    store
        .ingest_receipt("model-attempt-1", &recorded.checkpoint())
        .unwrap();
}
fn application(store: &mut Store) -> Proposal {
    let baseline = ModelWait::restore(&store.snapshot().unwrap().continuation).unwrap();
    let receipt = store.receipt("model-attempt-1").unwrap().unwrap();
    let recorded = ModelWait::restore(&receipt).unwrap();
    assert_eq!(recorded.phase(), Phase::Recorded);
    assert!(baseline.request().matches(recorded.request()));
    // Validate receipt against the actual waiting baseline, not just a well-framed blob.
    let checked = baseline
        .record(
            recorded.request(),
            recorded.attempt(),
            recorded.response().unwrap(),
        )
        .unwrap();
    let response = response_proposal(checked.response().unwrap()).unwrap();
    let effects = response
        .into_calls()
        .into_iter()
        .map(|call| EffectIntent {
            id: call.id,
            adapter: call.name,
            input: call.input,
        })
        .collect();
    let applied = checked.apply().unwrap();
    Proposal {
        id: "apply-model".into(),
        scope: "operator".into(),
        observation: Observation::Receipt("model-attempt-1".into()),
        expected_revision: 1,
        owner_epoch: store.epoch(),
        engine: "dext-model-wait-v1".into(),
        continuation: applied.checkpoint(),
        workspace: None,
        effects,
        state: None,
    }
}

#[test]
fn inadmissible_saved_responses_produce_no_tool_proposal() {
    for bytes in [
        br#"{"complete":false,"blocks":[{"type":"tool_use","id":"a","name":"edit","input":{}}]}"#.as_slice(),
        br#"{"complete":true,"blocks":[{"type":"tool_use","id":"a","name":"edit","input":{}},{"type":"tool_use","id":"a","name":"edit","input":{}}]}"#.as_slice(),
        br#"{"complete":true,"blocks":[{"type":"tool_use","id":"a","name":"edit","input":null}]}"#.as_slice(),
    ] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let waiting = setup(&mut store);
        // Deliberately bypass adapter validation to exercise fail-closed application
        // of a retained malformed receipt. Production ingress must validate first.
        let recorded = waiting.record(waiting.request(), 1, bytes).unwrap();
        store.ingest_receipt("model-attempt-1", &recorded.checkpoint()).unwrap();
        drop(store);
        let mut recovered = Store::open(&realm.0).unwrap();
        let saved = ModelWait::restore(&recovered.receipt("model-attempt-1").unwrap().unwrap()).unwrap();
        assert!(response_proposal(saved.response().unwrap()).is_err());
        assert_eq!(recovered.snapshot().unwrap().revision, 1);
        assert!(recovered.intents(2).unwrap().is_empty());
    }
}

#[test]
fn saved_response_is_reused_and_tool_intent_published_once_after_reopen() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let waiting = setup(&mut store);
    receive(&mut store, &waiting);
    assert_eq!(
        ModelWait::restore(&store.snapshot().unwrap().continuation)
            .unwrap()
            .phase(),
        Phase::Waiting
    );
    assert!(store.intents(2).unwrap().is_empty());
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    let saved = ModelWait::restore(&store.receipt("model-attempt-1").unwrap().unwrap()).unwrap();
    assert!(saved.start().is_err());
    assert!(saved.abandon_unrecorded_for_retry().is_err());
    assert!(matches!(
        store.reserve_attempt(&Attempt {
            id: "model-attempt-2".into(),
            effect: "model-1".into(),
            owner_epoch: store.epoch(),
            implementation: "fake-v1".into(),
            grant_reference: "test-only".into(),
        }),
        Err(Error::Conflict)
    ));
    let proposed = application(&mut store);
    store.commit_proposal(&proposed).unwrap();
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.commit_proposal(&proposed).unwrap(), 2);
    let applied = ModelWait::restore(&store.snapshot().unwrap().continuation).unwrap();
    assert_eq!(applied.phase(), Phase::Applied);
    assert_eq!(applied.response(), saved.response());
    assert_eq!(store.intents(2).unwrap(), proposed.effects);
}

#[test]
fn failed_response_application_retains_receipt_and_never_publishes_tool_intent() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let waiting = setup(&mut store);
    receive(&mut store, &waiting);
    let proposed = application(&mut store);
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_apply BEFORE UPDATE ON head BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(matches!(
        store.commit_proposal(&proposed),
        Err(Error::Sql(_))
    ));
    db.execute_batch("DROP TRIGGER fail_apply").unwrap();
    drop(db);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.snapshot().unwrap().revision, 1);
    assert!(store.intents(2).unwrap().is_empty());
    let mut fresh = application(&mut store);
    fresh.id = "recomputed-application".into();
    store.commit_proposal(&fresh).unwrap();
    assert_eq!(store.intents(2).unwrap().len(), 1);
}

#[test]
#[ignore = "invoked by parent process-kill test"]
fn receipt_child() {
    let root = PathBuf::from(std::env::var_os("SPINDLE_MODEL_REALM").unwrap());
    let mut store = Store::open(&root).unwrap();
    let waiting = setup(&mut store);
    receive(&mut store, &waiting);
    std::fs::write(root.join("receipt-ready"), b"committed").unwrap();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn kill_between_receipt_and_application_needs_no_second_model_attempt() {
    struct Guard(std::process::Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let realm = Realm::new();
    let mut child = Guard(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "receipt_child", "--ignored"])
            .env("SPINDLE_MODEL_REALM", &realm.0)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !realm.0.join("receipt-ready").is_file() {
        assert!(child.0.try_wait().unwrap().is_none());
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    drop(child);
    let mut store = Store::open(&realm.0).unwrap();
    let proposed = application(&mut store);
    assert_eq!(store.commit_proposal(&proposed).unwrap(), 2);
    assert_eq!(store.intents(2).unwrap(), proposed.effects);
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM attempts", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}
