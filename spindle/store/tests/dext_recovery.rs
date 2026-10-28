use dext_core::recovery::{Decision, HaltReason, Observation as CoreObservation, RecoveryState};
use spindle_store::completion::Attempt;
use spindle_store::transition::{EffectIntent, Observation, Proposal};
use spindle_store::{Command, Error, Store};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const ENGINE: &str = "dext-core-recovery-v1";
static SERIAL: AtomicU64 = AtomicU64::new(0);

struct Realm(PathBuf);
impl Realm {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "spindle-dext-recovery-{}-{}",
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

fn begin(store: &mut Store) {
    let admitted = store
        .admit(&Command {
            scope: "operator".into(),
            id: "begin".into(),
            lineage: 0,
            kind: "recovery-fixture".into(),
            payload: vec![],
        })
        .unwrap();
    store
        .commit_proposal(&Proposal {
            id: "begin-proposal".into(),
            scope: "operator".into(),
            observation: Observation::Admission(admitted.sequence),
            expected_revision: 0,
            owner_epoch: store.epoch(),
            engine: ENGINE.into(),
            continuation: RecoveryState::default().checkpoint().to_vec(),
            workspace: None,
            effects: vec![effect(0)],
            state: None,
        })
        .unwrap();
}

fn effect(ordinal: u32) -> EffectIntent {
    EffectIntent {
        id: format!("model-{ordinal}"),
        adapter: "recorded-test-observation".into(),
        input: vec![],
    }
}

fn ingest(store: &mut Store, ordinal: u32, reason: &str) -> String {
    let id = format!("attempt-{ordinal}");
    store
        .reserve_attempt(&Attempt {
            id: id.clone(),
            effect: format!("model-{ordinal}"),
            owner_epoch: store.epoch(),
            implementation: "fake-model-v1".into(),
            grant_reference: "test-only-no-dispatch".into(),
        })
        .unwrap();
    store.ingest_receipt(&id, reason.as_bytes()).unwrap();
    id
}

fn proposal(store: &mut Store, attempt: &str) -> (Proposal, Decision) {
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.engine, ENGINE);
    let baseline = RecoveryState::restore(&snapshot.continuation).unwrap();
    let recorded = store.receipt(attempt).unwrap().unwrap();
    let reason = std::str::from_utf8(&recorded).unwrap();
    let observation = CoreObservation::Response {
        incomplete_reason: Some(reason),
        executable_calls: false,
    };
    let transition = baseline.advance(observation);
    assert_eq!(baseline.checkpoint().as_slice(), snapshot.continuation);
    let effects = match transition.decision {
        Decision::Retry { ordinal } => vec![effect(ordinal)],
        Decision::Proceed | Decision::Halt(_) => vec![],
    };
    (
        Proposal {
            id: format!("consume-{attempt}"),
            scope: "operator".into(),
            observation: Observation::Receipt(attempt.into()),
            expected_revision: snapshot.revision,
            owner_epoch: store.epoch(),
            engine: ENGINE.into(),
            continuation: transition.next.checkpoint().to_vec(),
            workspace: None,
            effects,
            state: None,
        },
        transition.decision,
    )
}

#[test]
fn shared_cli_core_preserves_budget_across_receipt_ingress_and_commit_restarts() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    begin(&mut store);
    for ordinal in 0..4 {
        let attempt = ingest(&mut store, ordinal, "output_token_limit");
        let baseline = store.snapshot().unwrap();
        // Receipt survives without adopting a speculative continuation.
        drop(store);
        store = Store::open(&realm.0).unwrap();
        assert_eq!(store.snapshot().unwrap(), baseline);
        let (proposed, decision) = proposal(&mut store, &attempt);
        let expected = if ordinal < 3 {
            Decision::Retry {
                ordinal: ordinal + 1,
            }
        } else {
            Decision::Halt(HaltReason::RetryBudget)
        };
        assert_eq!(decision, expected);
        assert!(store.intents(baseline.revision + 1).unwrap().is_empty());
        let revision = store.commit_proposal(&proposed).unwrap();
        // Simulated lost reply: reopen and retry the exact committed proposal.
        drop(store);
        store = Store::open(&realm.0).unwrap();
        assert_eq!(store.commit_proposal(&proposed).unwrap(), revision);
        assert_eq!(store.intents(revision).unwrap(), proposed.effects);
        assert_eq!(
            store.receipt(&attempt).unwrap().unwrap(),
            b"output_token_limit"
        );
    }
    let snapshot = store.snapshot().unwrap();
    assert!(store.intents(snapshot.revision).unwrap().is_empty());
    let halted = RecoveryState::restore(&snapshot.continuation).unwrap();
    assert_eq!(
        halted
            .advance(CoreObservation::Response {
                incomplete_reason: Some("output_token_limit"),
                executable_calls: false,
            })
            .decision,
        Decision::Halt(HaltReason::RetryBudget)
    );
}

#[test]
fn failed_commit_cannot_adopt_core_state_or_publish_retry_intent() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    begin(&mut store);
    let attempt = ingest(&mut store, 0, "output_token_limit");
    let baseline = store.snapshot().unwrap();
    let (proposed, decision) = proposal(&mut store, &attempt);
    assert_eq!(decision, Decision::Retry { ordinal: 1 });
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_head BEFORE UPDATE ON head BEGIN SELECT RAISE(ABORT,'injected'); END;",
    )
    .unwrap();
    assert!(matches!(
        store.commit_proposal(&proposed),
        Err(Error::Sql(_))
    ));
    db.execute_batch("DROP TRIGGER fail_head").unwrap();
    drop(db);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.snapshot().unwrap(), baseline);
    assert!(store.intents(baseline.revision + 1).unwrap().is_empty());
    let (mut fresh, decision) = proposal(&mut store, &attempt);
    assert_eq!(decision, Decision::Retry { ordinal: 1 });
    fresh.id = "recomputed-after-reopen".into();
    store.commit_proposal(&fresh).unwrap();
    assert_eq!(store.intents(baseline.revision + 1).unwrap().len(), 1);
}

#[test]
#[ignore = "invoked by parent process-kill test"]
fn recovery_checkpoint_child() {
    let root = PathBuf::from(std::env::var_os("SPINDLE_RECOVERY_REALM").unwrap());
    let mut store = Store::open(&root).unwrap();
    begin(&mut store);
    for ordinal in 0..2 {
        let attempt = ingest(&mut store, ordinal, "output_token_limit");
        let (proposed, _) = proposal(&mut store, &attempt);
        store.commit_proposal(&proposed).unwrap();
    }
    std::fs::write(root.join("checkpoint-ready"), b"committed").unwrap();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn killed_host_retains_shared_core_retry_budget() {
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let realm = Realm::new();
    let mut child = ChildGuard(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "recovery_checkpoint_child", "--ignored"])
            .env("SPINDLE_RECOVERY_REALM", &realm.0)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !realm.0.join("checkpoint-ready").is_file() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "child exited before checkpoint"
        );
        assert!(std::time::Instant::now() < deadline, "checkpoint deadline");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(matches!(Store::open(&realm.0), Err(Error::Locked)));
    drop(child);
    let mut recovered = Store::open(&realm.0).unwrap();
    assert_eq!(recovered.snapshot().unwrap().revision, 3);
    let attempt = ingest(&mut recovered, 2, "output_token_limit");
    let (proposed, decision) = proposal(&mut recovered, &attempt);
    assert_eq!(decision, Decision::Retry { ordinal: 3 });
    recovered.commit_proposal(&proposed).unwrap();
    let attempt = ingest(&mut recovered, 3, "output_token_limit");
    let (proposed, decision) = proposal(&mut recovered, &attempt);
    assert_eq!(decision, Decision::Halt(HaltReason::RetryBudget));
    let revision = recovered.commit_proposal(&proposed).unwrap();
    assert!(recovered.intents(revision).unwrap().is_empty());
}

#[test]
fn content_filter_commits_halt_with_no_retry_intent() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    begin(&mut store);
    let attempt = ingest(&mut store, 0, "content_filter");
    let (proposed, decision) = proposal(&mut store, &attempt);
    assert_eq!(decision, Decision::Halt(HaltReason::ContentFilter));
    store.commit_proposal(&proposed).unwrap();
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    let snapshot = store.snapshot().unwrap();
    assert!(store.intents(snapshot.revision).unwrap().is_empty());
    let restored = RecoveryState::restore(&snapshot.continuation).unwrap();
    assert_eq!(
        restored
            .advance(CoreObservation::Response {
                incomplete_reason: None,
                executable_calls: false,
            })
            .decision,
        Decision::Halt(HaltReason::ContentFilter)
    );
}
