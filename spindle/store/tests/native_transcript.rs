use dext_core::model::{Encoding, Identity, ModelWait, PreparedRequest};
use dext_transcript::checkpoint::{NativeResponse, TranscriptCheckpoint};
use dext_transcript::continuation::ResponseContinuation;
use dext_transcript::{Block, Message, Usage};
use spindle_store::completion::Attempt;
use spindle_store::transition::{EffectIntent, Observation, Proposal};
use spindle_store::{Command, Store};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SERIAL: AtomicU64 = AtomicU64::new(0);
struct Realm(PathBuf);
impl Realm {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "spindle-native-transcript-{}-{}",
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
fn prepare(store: &mut Store) {
    let admitted = store
        .admit(&Command {
            scope: "operator".into(),
            id: "prompt".into(),
            lineage: 0,
            kind: "test-native".into(),
            payload: b"prompt".to_vec(),
        })
        .unwrap();
    let transcript = TranscriptCheckpoint {
        route_binding: "synthetic-route-v1".into(),
        messages: vec![Message {
            role: "user".into(),
            content: vec![Block::Text {
                text: "prompt".into(),
            }],
        }],
    };
    let request = PreparedRequest::new(
        Identity {
            effect: "model".into(),
            provider: "fake".into(),
            contract: "synthetic-v1".into(),
            endpoint: "https://example.invalid".into(),
            model: "fake".into(),
            credential_scope: "opaque-version".into(),
            configuration: "policy-v1".into(),
        },
        Encoding::Json,
        b"synthetic request".to_vec(),
    )
    .unwrap();
    let continuation =
        ResponseContinuation::waiting(transcript, ModelWait::new(request).start().unwrap())
            .unwrap();
    store
        .commit_proposal(&Proposal {
            id: "prepare".into(),
            scope: "operator".into(),
            observation: Observation::Admission(admitted.sequence),
            expected_revision: 0,
            owner_epoch: store.epoch(),
            engine: "native-codec-test-v1".into(),
            continuation: continuation.checkpoint().unwrap(),
            workspace: None,
            effects: vec![EffectIntent {
                id: "model".into(),
                adapter: "inert-model".into(),
                input: vec![],
            }],
            state: None,
        })
        .unwrap();
    store
        .reserve_attempt(&Attempt {
            id: "attempt".into(),
            effect: "model".into(),
            owner_epoch: store.epoch(),
            implementation: "synthetic-v1".into(),
            grant_reference: "test-only".into(),
        })
        .unwrap();
    let response = NativeResponse {
        blocks: vec![
            Block::Thinking {
                text: "thought".into(),
                signature: Some("signature".into()),
            },
            Block::Text {
                text: "answer".into(),
            },
            Block::ToolUse {
                id: "call".into(),
                name: "inert-tool".into(),
                input: serde_json::json!({"path":"test"}),
            },
        ],
        stop_reason: Some("tool_use".into()),
        usage: Usage {
            input: 12,
            output: 5,
            ..Default::default()
        },
        unknown_events: 1,
        unfinished_tool_calls: 0,
    };
    let recorded = continuation
        .model()
        .record(
            continuation.model().request(),
            1,
            &response.checkpoint().unwrap(),
        )
        .unwrap();
    store
        .ingest_receipt("attempt", &recorded.checkpoint())
        .unwrap();
}
fn application(store: &mut Store) -> Proposal {
    let snapshot = store.snapshot().unwrap();
    let waiting = ResponseContinuation::restore(&snapshot.continuation).unwrap();
    let recorded = ModelWait::restore(&store.receipt("attempt").unwrap().unwrap()).unwrap();
    let continuation = waiting.apply(&recorded, "synthetic-route-v1").unwrap();
    let calls = continuation.pending_calls().unwrap();
    Proposal {
        id: "apply-native".into(),
        scope: "operator".into(),
        observation: Observation::Receipt("attempt".into()),
        expected_revision: snapshot.revision,
        owner_epoch: store.epoch(),
        engine: "native-codec-test-v1".into(),
        continuation: continuation.checkpoint().unwrap(),
        workspace: None,
        effects: calls
            .into_iter()
            .map(|call| EffectIntent {
                id: call.id,
                adapter: call.name,
                input: serde_json::to_vec(&call.input).unwrap(),
            })
            .collect(),
        state: None,
    }
}

#[test]
fn native_receipt_and_transcript_survive_reopen_and_atomic_rollback() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    prepare(&mut store);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    let receipt = store.receipt("attempt").unwrap().unwrap();
    let proposed = application(&mut store);
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_apply BEFORE UPDATE ON head BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(store.commit_proposal(&proposed).is_err());
    db.execute_batch("DROP TRIGGER fail_apply").unwrap();
    drop(db);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.snapshot().unwrap().revision, 1);
    assert_eq!(store.receipt("attempt").unwrap().unwrap(), receipt);
    assert!(store.intents(2).unwrap().is_empty());
    let proposed = application(&mut store);
    assert_eq!(store.commit_proposal(&proposed).unwrap(), 2);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.commit_proposal(&proposed).unwrap(), 2);
    let restored = ResponseContinuation::restore(&store.snapshot().unwrap().continuation).unwrap();
    assert_eq!(restored.transcript().messages.len(), 2);
    assert_eq!(
        restored.transcript().messages[1].content,
        NativeResponse::restore(ModelWait::restore(&receipt).unwrap().response().unwrap())
            .unwrap()
            .blocks
    );
    assert_eq!(store.intents(2).unwrap(), proposed.effects);
    assert_eq!(store.receipt("attempt").unwrap().unwrap(), receipt);
}

#[test]
fn tool_receipt_adoption_is_atomic_and_reuses_saved_outcome_after_reopen() {
    use dext_transcript::tool_results::ToolResultsContinuation;
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    prepare(&mut store);
    let mut proposed = application(&mut store);
    let response = ResponseContinuation::restore(&proposed.continuation).unwrap();
    let tools = ToolResultsContinuation::new(response).unwrap();
    proposed.continuation = tools.checkpoint().unwrap();
    store.commit_proposal(&proposed).unwrap();
    let call = tools.pending_calls().unwrap().remove(0);
    let attempt = Attempt {
        id: "tool-attempt".into(),
        effect: call.id.clone(),
        owner_epoch: store.epoch(),
        implementation: "synthetic-tool-v1".into(),
        grant_reference: "test-only".into(),
    };
    let intent = proposed.effects[0].clone();
    store.reserve_attempt(&attempt).unwrap();
    let result = Block::ToolResult {
        tool_use_id: call.id.clone(),
        content: "saved outcome".into(),
        is_error: Some(false),
        metadata: Default::default(),
    };
    let receipt = serde_json::to_vec(&result).unwrap();
    store.ingest_receipt("tool-attempt", &receipt).unwrap();
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    let snapshot = store.snapshot().unwrap();
    let baseline = ToolResultsContinuation::restore(&snapshot.continuation).unwrap();
    let result: Block =
        serde_json::from_slice(&store.receipt("tool-attempt").unwrap().unwrap()).unwrap();
    let next = baseline.observe("model", 1, &call, result).unwrap();
    let mut proposed = Proposal {
        id: "apply-tool".into(),
        scope: "operator".into(),
        observation: Observation::Receipt("tool-attempt".into()),
        expected_revision: snapshot.revision,
        owner_epoch: store.epoch(),
        engine: "native-codec-test-v1".into(),
        continuation: next.checkpoint().unwrap(),
        workspace: None,
        effects: vec![],
        state: None,
    };
    let binding = spindle_store::transition::ReceiptBinding {
        attempt: &attempt,
        intent_revision: 2,
        intent: &intent,
        result: &receipt,
        objects: &[],
    };
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_tool BEFORE UPDATE ON head BEGIN SELECT RAISE(ABORT,'injected'); END;",
    )
    .unwrap();
    assert!(store.commit_receipt_proposal(&proposed, &binding).is_err());
    db.execute_batch("DROP TRIGGER fail_tool").unwrap();
    drop(db);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(
        store.snapshot().unwrap().continuation,
        snapshot.continuation
    );
    assert_eq!(store.receipt("tool-attempt").unwrap().unwrap(), receipt);
    assert!(
        !ToolResultsContinuation::restore(&store.snapshot().unwrap().continuation)
            .unwrap()
            .complete()
    );
    proposed.owner_epoch = store.epoch();
    assert_eq!(
        store.commit_receipt_proposal(&proposed, &binding).unwrap(),
        3
    );
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(
        store.commit_receipt_proposal(&proposed, &binding).unwrap(),
        3
    );
    let done = ToolResultsContinuation::restore(&store.snapshot().unwrap().continuation).unwrap();
    assert!(done.complete());
    assert!(done.pending_calls().unwrap().is_empty());
    let transcript = done.transcript().unwrap();
    assert_eq!(transcript.messages.len(), 3);
    assert!(
        matches!(&transcript.messages[2].content[0], Block::ToolResult { content, .. } if content == "saved outcome")
    );
    assert!(store.intents(3).unwrap().is_empty());
    assert_eq!(store.receipt("tool-attempt").unwrap().unwrap(), receipt);
}

// The pinned image worker runs only on Linux.
#[cfg(all(feature = "dext-native-image", target_os = "linux"))]
#[test]
fn verified_image_result_derives_transcript_atomically_and_rejects_unrelated_next_state() {
    use dext_native::image::PinnedWorker;
    use dext_transcript::tool_results::ToolResultsContinuation;
    use sha2::{Digest, Sha256};
    use spindle_store::artifact::Stage;
    use spindle_store::image::{ImageReceipt, image_result_transition};
    let worker_path = std::path::Path::new(env!("CARGO_BIN_EXE_spindle-image-worker"));
    let worker_digest: [u8; 32] = Sha256::digest(std::fs::read(worker_path).unwrap()).into();
    let worker = PinnedWorker::open(worker_path, worker_digest).unwrap();
    let pixels = worker
        .sanitize(include_bytes!("fixtures/dext-worker-2x1.jpg"), None)
        .unwrap();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    prepare(&mut store);
    let mut prepared = application(&mut store);
    let tools = ToolResultsContinuation::new(
        ResponseContinuation::restore(&prepared.continuation).unwrap(),
    )
    .unwrap();
    prepared.continuation = tools.checkpoint().unwrap();
    store.commit_proposal(&prepared).unwrap();
    let intent = prepared.effects[0].clone();
    let stage = Stage {
        id: "pixels".into(),
        scope: "operator".into(),
        owner_epoch: store.epoch(),
    };
    store.begin_stage(&stage).unwrap();
    let image = store.put_sanitized_image(&stage, &pixels).unwrap();
    let image_receipt = ImageReceipt {
        images: vec![image],
    };
    let receipt = image_receipt.encode().unwrap();
    let attempt = Attempt {
        id: "image-tool-attempt".into(),
        effect: intent.id.clone(),
        owner_epoch: store.epoch(),
        implementation: "pinned-image-worker-v1".into(),
        grant_reference: "retention-approved".into(),
    };
    store.reserve_attempt(&attempt).unwrap();
    store.ingest_receipt(&attempt.id, &receipt).unwrap();
    let baseline = store.snapshot().unwrap();
    let next = image_result_transition(&baseline.continuation, "operator", &intent, &image_receipt)
        .unwrap();
    let mut proposed = Proposal {
        id: "apply-image-result".into(),
        scope: "operator".into(),
        observation: Observation::Receipt(attempt.id.clone()),
        expected_revision: baseline.revision,
        owner_epoch: store.epoch(),
        engine: baseline.engine.clone(),
        continuation: next,
        workspace: None,
        effects: vec![],
        state: None,
    };
    let mut unrelated = proposed.clone();
    unrelated.continuation = baseline.continuation.clone();
    assert!(matches!(
        store.commit_image_tool_result_proposal(
            &unrelated,
            &attempt,
            2,
            &intent,
            &receipt,
            worker_digest
        ),
        Err(spindle_store::Error::Conflict)
    ));
    assert_eq!(store.snapshot().unwrap(), baseline);
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_image_result BEFORE UPDATE ON head BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(matches!(
        store.commit_image_tool_result_proposal(
            &proposed,
            &attempt,
            2,
            &intent,
            &receipt,
            worker_digest
        ),
        Err(spindle_store::Error::Sql(_))
    ));
    db.execute_batch("DROP TRIGGER fail_image_result").unwrap();
    drop(db);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.snapshot().unwrap(), baseline);
    proposed.owner_epoch = store.epoch();
    assert_eq!(
        store
            .commit_image_tool_result_proposal(
                &proposed,
                &attempt,
                2,
                &intent,
                &receipt,
                worker_digest
            )
            .unwrap(),
        3
    );
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(
        store
            .commit_image_tool_result_proposal(
                &proposed,
                &attempt,
                2,
                &intent,
                &receipt,
                worker_digest
            )
            .unwrap(),
        3
    );
    let done = ToolResultsContinuation::restore(&store.snapshot().unwrap().continuation).unwrap();
    assert!(done.complete());
    let transcript = done.transcript().unwrap();
    assert_eq!(transcript.messages.len(), 3);
    assert!(
        matches!(&transcript.messages[2].content[1], Block::ImageArtifact { reference } if reference.scope == "operator" && reference.width == 2)
    );
    assert!(store.intents(3).unwrap().is_empty());
}

#[test]
#[ignore = "invoked by process-kill parent"]
fn native_receipt_child() {
    let root = PathBuf::from(std::env::var_os("SPINDLE_NATIVE_REALM").unwrap());
    let mut store = Store::open(&root).unwrap();
    prepare(&mut store);
    std::fs::write(root.join("ready"), b"committed").unwrap();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn killed_native_receipt_writer_recovers_without_a_second_model_effect() {
    let realm = Realm::new();
    let mut child = ChildGuard(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "native_receipt_child", "--ignored"])
            .env("SPINDLE_NATIVE_REALM", &realm.0)
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !realm.0.join("ready").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let ready = realm.0.join("ready").exists();
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(ready);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.intents(1).unwrap().len(), 1);
    let proposed = application(&mut store);
    store.commit_proposal(&proposed).unwrap();
    assert_eq!(store.intents(2).unwrap()[0].adapter, "inert-tool");
    assert_eq!(
        ResponseContinuation::restore(&store.snapshot().unwrap().continuation)
            .unwrap()
            .transcript()
            .messages
            .len(),
        2
    );
}
