use spindle_store::transition::{EffectIntent, Observation, Proposal};
use spindle_store::{Command, Error, Store};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SERIAL: AtomicU64 = AtomicU64::new(0);
struct Realm(PathBuf);
impl Realm {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "spindle-transition-{}-{}",
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
fn prepare(store: &mut Store, id: &str) -> Proposal {
    let admission = store
        .admit(&Command {
            scope: "operator".into(),
            id: id.into(),
            lineage: 0,
            kind: "turn".into(),
            payload: b"input".to_vec(),
        })
        .unwrap();
    Proposal {
        id: format!("proposal-{id}"),
        scope: "operator".into(),
        observation: Observation::Admission(admission.sequence),
        expected_revision: store.snapshot().unwrap().revision,
        owner_epoch: store.epoch(),
        engine: "test-machine-v1".into(),
        continuation: b"waiting-for-effect".to_vec(),
        workspace: None,
        effects: vec![EffectIntent {
            id: format!("effect-{id}"),
            adapter: "test-only".into(),
            input: b"effect-input".to_vec(),
        }],
        state: None,
    }
}

#[test]
fn checked_receipt_commit_binds_every_retained_field_and_retries_after_takeover() {
    use spindle_store::completion::Attempt;
    use spindle_store::transition::ReceiptBinding;
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let prepared = prepare(&mut store, "checked");
    store.commit_proposal(&prepared).unwrap();
    let attempt = Attempt {
        id: "checked-attempt".into(),
        effect: prepared.effects[0].id.clone(),
        owner_epoch: store.epoch(),
        implementation: "native-v1".into(),
        grant_reference: "grant-v1".into(),
    };
    store.reserve_attempt(&attempt).unwrap();
    store.ingest_receipt(&attempt.id, b"result").unwrap();
    let mut proposed = Proposal {
        id: "checked-apply".into(),
        scope: "operator".into(),
        observation: Observation::Receipt(attempt.id.clone()),
        expected_revision: 1,
        owner_epoch: store.epoch(),
        engine: "test-v1".into(),
        continuation: b"adopted".to_vec(),
        workspace: None,
        effects: vec![],
        state: None,
    };
    let baseline = store.snapshot().unwrap();
    for field in 0..8 {
        let mut wrong_attempt = attempt.clone();
        let mut wrong_intent = prepared.effects[0].clone();
        let mut revision = 1;
        let mut result = b"result".as_slice();
        match field {
            0 => wrong_attempt.owner_epoch += 1,
            1 => wrong_attempt.implementation = "wrong".into(),
            2 => wrong_attempt.grant_reference = "wrong".into(),
            3 => wrong_intent.adapter = "wrong".into(),
            4 => wrong_intent.input.push(0),
            5 => revision = 2,
            6 => result = b"wrong",
            _ => {
                wrong_attempt.effect = "wrong".into();
                wrong_intent.id = "wrong".into();
            }
        }
        let binding = ReceiptBinding {
            attempt: &wrong_attempt,
            intent_revision: revision,
            intent: &wrong_intent,
            result,
            objects: &[],
        };
        assert!(matches!(
            store.commit_receipt_proposal(&proposed, &binding),
            Err(Error::Conflict)
        ));
        assert_eq!(store.snapshot().unwrap(), baseline);
    }
    let binding = ReceiptBinding {
        attempt: &attempt,
        intent_revision: 1,
        intent: &prepared.effects[0],
        result: b"result",
        objects: &[],
    };
    let mut wrong_scope = proposed.clone();
    wrong_scope.scope = "other".into();
    assert!(matches!(
        store.commit_receipt_proposal(&wrong_scope, &binding),
        Err(Error::Conflict)
    ));
    let mut missing = attempt.clone();
    missing.id = "missing".into();
    let mut missing_proposal = proposed.clone();
    missing_proposal.observation = Observation::Receipt(missing.id.clone());
    let missing_binding = ReceiptBinding {
        attempt: &missing,
        ..binding
    };
    assert!(matches!(
        store.commit_receipt_proposal(&missing_proposal, &missing_binding),
        Err(Error::Invalid)
    ));
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert!(matches!(
        store.commit_receipt_proposal(&proposed, &binding),
        Err(Error::Stale)
    ));
    proposed.owner_epoch = store.epoch();
    // A current owner may adopt an old-owner receipt, without reserving/rerunning it.
    assert_eq!(
        store.commit_receipt_proposal(&proposed, &binding).unwrap(),
        2
    );
    store.cancel_scope("operator", "cancel").unwrap();
    assert_eq!(
        store.commit_receipt_proposal(&proposed, &binding).unwrap(),
        2
    );
    let wrong = ReceiptBinding {
        result: b"changed",
        ..binding
    };
    assert!(matches!(
        store.commit_receipt_proposal(&proposed, &wrong),
        Err(Error::Conflict)
    ));
}

#[test]
fn checked_receipt_objects_require_scope_integrity_and_survive_adoption_rollback() {
    use spindle_store::artifact::{ObjectReference, Stage};
    use spindle_store::completion::Attempt;
    use spindle_store::transition::ReceiptBinding;
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let prepared = prepare(&mut store, "objects");
    store.commit_proposal(&prepared).unwrap();
    let stage = Stage {
        id: "objects-stage".into(),
        scope: "operator".into(),
        owner_epoch: store.epoch(),
    };
    store.begin_stage(&stage).unwrap();
    let digest = store.put_object(&stage, b"synthetic-image-bytes").unwrap();
    let objects = [ObjectReference {
        digest,
        byte_length: 21,
    }];
    let attempt = Attempt {
        id: "object-attempt".into(),
        effect: prepared.effects[0].id.clone(),
        owner_epoch: store.epoch(),
        implementation: "native-v1".into(),
        grant_reference: "grant-v1".into(),
    };
    store.reserve_attempt(&attempt).unwrap();
    let receipt = format!("object:{}:{}", objects[0].digest, objects[0].byte_length).into_bytes();
    store.ingest_receipt(&attempt.id, &receipt).unwrap();
    let mut proposed = Proposal {
        id: "object-adopt".into(),
        scope: "operator".into(),
        observation: Observation::Receipt(attempt.id.clone()),
        expected_revision: 1,
        owner_epoch: store.epoch(),
        engine: "test-v1".into(),
        continuation: receipt.clone(),
        workspace: None,
        effects: vec![],
        state: None,
    };
    let binding = ReceiptBinding {
        attempt: &attempt,
        intent_revision: 1,
        intent: &prepared.effects[0],
        result: &receipt,
        objects: &objects,
    };
    for bad in [
        vec![ObjectReference {
            byte_length: 20,
            ..objects[0].clone()
        }],
        vec![ObjectReference {
            digest: "0".repeat(64),
            byte_length: 21,
        }],
        vec![objects[0].clone(); 2],
        vec![ObjectReference {
            digest: "invalid".into(),
            byte_length: 21,
        }],
        vec![objects[0].clone(); 9],
    ] {
        let wrong = ReceiptBinding {
            objects: &bad,
            ..binding
        };
        assert!(store.commit_receipt_proposal(&proposed, &wrong).is_err());
        assert_eq!(store.snapshot().unwrap().revision, 1);
    }
    // A real object pinned only in a different scope must not be adopted here.
    store
        .admit(&Command {
            scope: "other".into(),
            id: "other".into(),
            lineage: 0,
            kind: "turn".into(),
            payload: vec![],
        })
        .unwrap();
    let other = Stage {
        id: "other-stage".into(),
        scope: "other".into(),
        owner_epoch: store.epoch(),
    };
    store.begin_stage(&other).unwrap();
    let foreign = [ObjectReference {
        digest: store.put_object(&other, b"foreign").unwrap(),
        byte_length: 7,
    }];
    assert!(matches!(
        store.commit_receipt_proposal(
            &proposed,
            &ReceiptBinding {
                objects: &foreign,
                ..binding
            }
        ),
        Err(Error::Invalid)
    ));
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_object_adopt BEFORE UPDATE ON head BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(matches!(
        store.commit_receipt_proposal(&proposed, &binding),
        Err(Error::Sql(_))
    ));
    db.execute_batch("DROP TRIGGER fail_object_adopt").unwrap();
    drop(db);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.snapshot().unwrap().revision, 1);
    assert_eq!(
        store.read_object("operator", &objects[0].digest).unwrap(),
        b"synthetic-image-bytes"
    );
    proposed.owner_epoch = store.epoch();
    assert_eq!(
        store.commit_receipt_proposal(&proposed, &binding).unwrap(),
        2
    );
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(
        store.commit_receipt_proposal(&proposed, &binding).unwrap(),
        2
    );
    // Live damage must also be detected on the committed retry path.
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute(
        "UPDATE objects SET bytes=?1 WHERE digest=?2",
        rusqlite::params![b"damaged".as_slice(), objects[0].digest],
    )
    .unwrap();
    assert!(matches!(
        store.commit_receipt_proposal(&proposed, &binding),
        Err(Error::Incompatible)
    ));
    assert!(matches!(store.snapshot(), Err(Error::Paused)));
}

#[test]
fn typed_artifact_adoption_derives_references_from_exact_retained_receipt() {
    use spindle_store::artifact::{ArtifactReceipt, ObjectReference, Stage};
    use spindle_store::completion::Attempt;
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let prepared = prepare(&mut store, "typed");
    store.commit_proposal(&prepared).unwrap();
    let stage = Stage {
        id: "typed-stage".into(),
        scope: "operator".into(),
        owner_epoch: store.epoch(),
    };
    store.begin_stage(&stage).unwrap();
    let object = ObjectReference {
        digest: store.put_object(&stage, b"artifact").unwrap(),
        byte_length: 8,
    };
    let receipt = ArtifactReceipt {
        objects: vec![object.clone()],
        payload: b"opaque-result".to_vec(),
    };
    let bytes = receipt.encode().unwrap();
    let attempt = Attempt {
        id: "typed-attempt".into(),
        effect: prepared.effects[0].id.clone(),
        owner_epoch: store.epoch(),
        implementation: "v1".into(),
        grant_reference: "grant".into(),
    };
    store.reserve_attempt(&attempt).unwrap();
    store.ingest_receipt(&attempt.id, &bytes).unwrap();
    let mut proposed = Proposal {
        id: "typed-apply".into(),
        scope: "operator".into(),
        observation: Observation::Receipt(attempt.id.clone()),
        expected_revision: 1,
        owner_epoch: store.epoch(),
        engine: "test-v1".into(),
        continuation: bytes.clone(),
        workspace: None,
        effects: vec![],
        state: None,
    };
    for objects in [
        vec![],
        vec![ObjectReference {
            digest: "0".repeat(64),
            ..object.clone()
        }],
    ] {
        let substituted = ArtifactReceipt {
            objects,
            payload: receipt.payload.clone(),
        }
        .encode()
        .unwrap();
        assert!(matches!(
            store.commit_artifact_receipt_proposal(
                &proposed,
                &attempt,
                1,
                &prepared.effects[0],
                &substituted
            ),
            Err(Error::Conflict)
        ));
    }
    assert!(
        store
            .commit_artifact_receipt_proposal(&proposed, &attempt, 1, &prepared.effects[0], b"SAR2")
            .is_err()
    );
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_typed BEFORE UPDATE ON head BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(matches!(
        store.commit_artifact_receipt_proposal(
            &proposed,
            &attempt,
            1,
            &prepared.effects[0],
            &bytes
        ),
        Err(Error::Sql(_))
    ));
    db.execute_batch("DROP TRIGGER fail_typed").unwrap();
    drop(db);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.snapshot().unwrap().revision, 1);
    assert_eq!(store.receipt(&attempt.id).unwrap().unwrap(), bytes);
    proposed.owner_epoch = store.epoch();
    assert_eq!(
        store
            .commit_artifact_receipt_proposal(&proposed, &attempt, 1, &prepared.effects[0], &bytes)
            .unwrap(),
        2
    );
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(
        store
            .commit_artifact_receipt_proposal(&proposed, &attempt, 1, &prepared.effects[0], &bytes)
            .unwrap(),
        2
    );
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute(
        "UPDATE objects SET bytes=X'00' WHERE digest=?1",
        [&object.digest],
    )
    .unwrap();
    assert!(matches!(
        store.commit_artifact_receipt_proposal(
            &proposed,
            &attempt,
            1,
            &prepared.effects[0],
            &bytes
        ),
        Err(Error::Incompatible)
    ));
    assert!(matches!(store.snapshot(), Err(Error::Paused)));
}

#[test]
fn checked_receipt_commit_cancellation_blocks_new_adoption_not_ingress() {
    use spindle_store::completion::Attempt;
    use spindle_store::transition::ReceiptBinding;
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let prepared = prepare(&mut store, "cancel-checked");
    store.commit_proposal(&prepared).unwrap();
    let attempt = Attempt {
        id: "attempt".into(),
        effect: prepared.effects[0].id.clone(),
        owner_epoch: store.epoch(),
        implementation: "native-v1".into(),
        grant_reference: "grant-v1".into(),
    };
    store.reserve_attempt(&attempt).unwrap();
    store.cancel_scope("operator", "cancel").unwrap();
    store.ingest_receipt(&attempt.id, b"late").unwrap();
    let proposed = Proposal {
        id: "apply".into(),
        scope: "operator".into(),
        observation: Observation::Receipt(attempt.id.clone()),
        expected_revision: 1,
        owner_epoch: store.epoch(),
        engine: "test-v1".into(),
        continuation: vec![],
        workspace: None,
        effects: vec![],
        state: None,
    };
    let binding = ReceiptBinding {
        attempt: &attempt,
        intent_revision: 1,
        intent: &prepared.effects[0],
        result: b"late",
        objects: &[],
    };
    assert!(matches!(
        store.commit_receipt_proposal(&proposed, &binding),
        Err(Error::Cancelled)
    ));
    assert_eq!(store.snapshot().unwrap().revision, 1);
    assert_eq!(store.receipt(&attempt.id).unwrap(), Some(b"late".to_vec()));
}

#[test]
fn receipt_ingress_survives_restart_before_atomic_consumption() {
    use spindle_store::completion::Attempt;
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let prepared = prepare(&mut store, "one");
    let attempt = Attempt {
        id: "attempt-one".into(),
        effect: "effect-one".into(),
        owner_epoch: store.epoch(),
        implementation: "test-native-v1".into(),
        grant_reference: "test-grant".into(),
    };
    assert!(matches!(
        store.reserve_attempt(&attempt),
        Err(Error::Invalid)
    ));
    assert!(matches!(
        store.ingest_receipt(&attempt.id, b"answer"),
        Err(Error::Invalid)
    ));
    store.commit_proposal(&prepared).unwrap();
    store.reserve_attempt(&attempt).unwrap();
    store.reserve_attempt(&attempt).unwrap();
    let before = store.snapshot().unwrap();
    store.ingest_receipt(&attempt.id, b"answer").unwrap();
    assert_eq!(store.snapshot().unwrap(), before);
    store.ingest_receipt(&attempt.id, b"answer").unwrap();
    assert!(matches!(
        store.ingest_receipt(&attempt.id, b"different"),
        Err(Error::Conflict)
    ));
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(
        store.receipt(&attempt.id).unwrap(),
        Some(b"answer".to_vec())
    );
    assert_eq!(store.snapshot().unwrap(), before);
    assert!(matches!(store.reserve_attempt(&attempt), Err(Error::Stale)));
    let mut another = attempt.clone();
    another.id = "attempt-two".into();
    another.owner_epoch = store.epoch();
    assert!(matches!(
        store.reserve_attempt(&another),
        Err(Error::Conflict)
    ));
    let applied = Proposal {
        id: "consume-receipt".into(),
        scope: "operator".into(),
        observation: Observation::Receipt(attempt.id.clone()),
        expected_revision: 1,
        owner_epoch: store.epoch(),
        engine: "test-machine-v1".into(),
        continuation: b"answer-applied".to_vec(),
        workspace: None,
        effects: vec![],
        state: None,
    };
    let mut wrong_scope = applied.clone();
    wrong_scope.scope = "other-operator".into();
    assert!(matches!(
        store.commit_proposal(&wrong_scope),
        Err(Error::Invalid)
    ));
    assert_eq!(store.commit_proposal(&applied).unwrap(), 2);
    assert_eq!(store.commit_proposal(&applied).unwrap(), 2);
    let mut duplicate = applied.clone();
    duplicate.id = "duplicate".into();
    duplicate.expected_revision = 2;
    assert!(matches!(
        store.commit_proposal(&duplicate),
        Err(Error::Conflict)
    ));
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.commit_proposal(&applied).unwrap(), 2);
    assert_eq!(store.snapshot().unwrap().continuation, b"answer-applied");
    assert_eq!(
        store.receipt(&attempt.id).unwrap(),
        Some(b"answer".to_vec())
    );
}

#[test]
fn cancellation_fences_new_work_but_keeps_late_receipts_and_committed_results() {
    use spindle_store::completion::Attempt;
    for publish_first in [false, true] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let prepared = prepare(&mut store, "one");
        store.commit_proposal(&prepared).unwrap();
        let attempt = Attempt {
            id: "attempt".into(),
            effect: "effect-one".into(),
            owner_epoch: store.epoch(),
            implementation: "test-v1".into(),
            grant_reference: "grant".into(),
        };
        store.reserve_attempt(&attempt).unwrap();
        let applied = Proposal {
            id: "consume".into(),
            scope: "operator".into(),
            observation: Observation::Receipt(attempt.id.clone()),
            expected_revision: 1,
            owner_epoch: store.epoch(),
            engine: "test-machine-v1".into(),
            continuation: b"complete".to_vec(),
            workspace: None,
            effects: vec![],
            state: None,
        };
        if publish_first {
            store.ingest_receipt(&attempt.id, b"answer").unwrap();
            store.commit_proposal(&applied).unwrap();
        }
        let before = store.snapshot().unwrap();
        let cancellation = store.cancel_scope("operator", "cancel-one").unwrap();
        assert_eq!(
            store.cancel_scope("operator", "cancel-one").unwrap(),
            cancellation
        );
        assert!(matches!(
            store.cancel_scope("operator", "cancel-other"),
            Err(Error::Conflict)
        ));
        assert!(matches!(
            store.reserve_attempt(&attempt),
            Err(Error::Cancelled)
        ));
        store.ingest_receipt(&attempt.id, b"answer").unwrap();
        assert_eq!(store.snapshot().unwrap(), before);
        if publish_first {
            assert_eq!(store.commit_proposal(&applied).unwrap(), 2);
        } else {
            assert!(matches!(
                store.commit_proposal(&applied),
                Err(Error::Cancelled)
            ));
        }
        let command = Command {
            scope: "operator".into(),
            id: "new".into(),
            lineage: 0,
            kind: "turn".into(),
            payload: vec![],
        };
        assert!(matches!(store.admit(&command), Err(Error::Cancelled)));
        assert_eq!(store.commit_proposal(&prepared).unwrap(), 1);
        drop(store);
        let mut reopened = Store::open(&realm.0).unwrap();
        assert_eq!(
            reopened.cancel_scope("operator", "cancel-one").unwrap(),
            cancellation
        );
        assert!(matches!(reopened.admit(&command), Err(Error::Cancelled)));
        assert_eq!(
            reopened.receipt(&attempt.id).unwrap(),
            Some(b"answer".to_vec())
        );
        assert_eq!(reopened.snapshot().unwrap(), before);
    }
}

#[test]
fn ignored_sql_mutations_never_acknowledge_partial_publication() {
    for table in ["revisions", "consumed", "intents", "proposals", "head"] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let proposed = prepare(&mut store, "one");
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        let operation = if table == "head" { "UPDATE" } else { "INSERT" };
        db.execute_batch(&format!("CREATE TRIGGER ignore_mutation BEFORE {operation} ON {table} BEGIN SELECT RAISE(IGNORE); END;")).unwrap();
        assert!(
            store.commit_proposal(&proposed).is_err(),
            "acknowledged ignored {table}"
        );
        assert!(matches!(store.snapshot(), Err(Error::Paused)));
        db.execute_batch("DROP TRIGGER ignore_mutation").unwrap();
        assert_eq!(
            db.query_row("SELECT revision FROM head", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM intents", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(db);
        drop(store);
        assert!(Store::open(&realm.0).is_ok());
    }
}

#[test]
fn ignored_admission_attempt_receipt_and_cancellation_never_return_success() {
    use spindle_store::completion::Attempt;
    for table in ["admissions", "attempts", "receipts", "cancellations"] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let proposed = prepare(&mut store, "one");
        store.commit_proposal(&proposed).unwrap();
        let attempt = Attempt {
            id: "attempt".into(),
            effect: "effect-one".into(),
            owner_epoch: store.epoch(),
            implementation: "test-v1".into(),
            grant_reference: "grant".into(),
        };
        if table == "receipts" {
            store.reserve_attempt(&attempt).unwrap();
        }
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        db.execute_batch(&format!(
            "CREATE TRIGGER ignore_insert BEFORE INSERT ON {table} BEGIN SELECT RAISE(IGNORE); END;"
        ))
        .unwrap();
        let result = match table {
            "admissions" => store
                .admit(&Command {
                    scope: "operator".into(),
                    id: "two".into(),
                    lineage: 0,
                    kind: "turn".into(),
                    payload: vec![],
                })
                .map(|_| ()),
            "attempts" => store.reserve_attempt(&attempt),
            "receipts" => store.ingest_receipt(&attempt.id, b"result"),
            "cancellations" => store.cancel_scope("operator", "cancel").map(|_| ()),
            _ => unreachable!(),
        };
        assert!(
            matches!(result, Err(Error::Incompatible)),
            "{table}: {result:?}"
        );
        assert!(matches!(store.snapshot(), Err(Error::Paused)));
        db.execute_batch("DROP TRIGGER ignore_insert").unwrap();
        drop(db);
        drop(store);
        assert!(Store::open(&realm.0).is_ok());
    }
}

#[test]
fn corrupted_cancellation_epoch_is_not_returned_as_a_valid_receipt() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    prepare(&mut store, "one");
    store.cancel_scope("operator", "cancel").unwrap();
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute("UPDATE cancellations SET epoch=-1", []).unwrap();
    assert!(store.cancel_scope("operator", "cancel").is_err());
    assert!(matches!(store.snapshot(), Err(Error::Paused)));
}

#[test]
fn live_reads_refuse_oversized_blobs_and_negative_revision_ids() {
    // Stored values are compressed (codec); one byte past the stored bound, or a value
    // that does not decode, is refused like any other damage.
    let oversized = format!(
        "UPDATE revisions SET continuation=zeroblob({}) WHERE revision=1",
        spindle_store::MAX_PAYLOAD + 2
    );
    for mutation in [
        oversized.as_str(),
        "UPDATE revisions SET continuation=X'0201' WHERE revision=1",
        "UPDATE revisions SET continuation=X'01000000ff00' WHERE revision=1",
        "PRAGMA foreign_keys=OFF; UPDATE revisions SET revision=-1 WHERE revision=1; UPDATE head SET revision=-1",
    ] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let proposed = prepare(&mut store, "one");
        store.commit_proposal(&proposed).unwrap();
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        db.execute_batch(mutation).unwrap();
        assert!(
            store.snapshot().is_err(),
            "returned malformed snapshot: {mutation}"
        );
        assert!(matches!(store.snapshot(), Err(Error::Paused)));
    }
}

#[test]
fn live_receipt_and_intent_reads_enforce_payload_limits() {
    use spindle_store::completion::Attempt;
    for receipt in [false, true] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let proposed = prepare(&mut store, "one");
        store.commit_proposal(&proposed).unwrap();
        store
            .reserve_attempt(&Attempt {
                id: "attempt".into(),
                effect: "effect-one".into(),
                owner_epoch: store.epoch(),
                implementation: "test-v1".into(),
                grant_reference: "grant".into(),
            })
            .unwrap();
        store.ingest_receipt("attempt", b"result").unwrap();
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        let oversized = spindle_store::MAX_PAYLOAD as i64 + 2;
        if receipt {
            db.execute("UPDATE receipts SET result=zeroblob(?1)", [oversized])
                .unwrap();
            assert!(store.receipt("attempt").is_err());
        } else {
            db.execute("UPDATE intents SET input=zeroblob(?1)", [oversized])
                .unwrap();
            assert!(store.intents(1).is_err());
        }
        assert!(matches!(store.snapshot(), Err(Error::Paused)));
    }
}

#[test]
fn contradictory_and_malformed_committed_records_block_recovery() {
    let mutations = [
        "UPDATE revisions SET continuation=X'00' WHERE revision=1",
        "UPDATE intents SET input=X'00'",
        "UPDATE intents SET position=7",
        "UPDATE proposals SET canonical=X'00'",
        "DELETE FROM consumed",
        "UPDATE head SET revision=0",
        "UPDATE revisions SET engine='changed-engine' WHERE revision=1",
        "UPDATE revisions SET scope='other-operator' WHERE revision=1",
        "UPDATE revisions SET continuation=X'00' WHERE revision=0",
    ];
    for mutation in mutations {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let proposal = prepare(&mut store, "one");
        store.commit_proposal(&proposal).unwrap();
        drop(store);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        db.execute(mutation, []).unwrap();
        drop(db);
        assert!(
            Store::open(&realm.0).is_err(),
            "accepted damaged records: {mutation}"
        );
    }
}

#[test]
fn canonical_decoder_refuses_every_truncated_prefix_and_trailing_bytes() {
    let source = Realm::new();
    let mut store = Store::open(&source.0).unwrap();
    let proposal = prepare(&mut store, "one");
    store.commit_proposal(&proposal).unwrap();
    drop(store);
    let db = rusqlite::Connection::open(source.0.join("authority.sqlite")).unwrap();
    use spindle_store::codec::{pack, unpack};
    let stored: Vec<u8> = db
        .query_row("SELECT canonical FROM proposals", [], |r| r.get(0))
        .unwrap();
    let canonical = unpack(&stored, usize::MAX).unwrap();
    for length in 0..canonical.len() {
        db.execute(
            "UPDATE proposals SET canonical=?1",
            [&pack(&canonical[..length])],
        )
        .unwrap();
        assert!(Store::open(&source.0).is_err(), "accepted prefix {length}");
    }
    // A truncated stored form never decodes to a shorter record.
    for length in 1..stored.len() {
        db.execute("UPDATE proposals SET canonical=?1", [&stored[..length]])
            .unwrap();
        assert!(
            Store::open(&source.0).is_err(),
            "accepted stored prefix {length}"
        );
    }
    let mut extended = canonical.clone();
    extended.push(0);
    db.execute("UPDATE proposals SET canonical=?1", [&pack(&extended)])
        .unwrap();
    assert!(Store::open(&source.0).is_err());
    // First field length follows the fixed versioned domain prefix.
    let mut oversize = canonical.clone();
    let start = b"spindle-proposal-v4\0".len();
    oversize[start..start + 4].copy_from_slice(&u32::MAX.to_be_bytes());
    db.execute("UPDATE proposals SET canonical=?1", [&pack(&oversize)])
        .unwrap();
    assert!(Store::open(&source.0).is_err());
    db.execute("UPDATE proposals SET canonical=?1", [&stored])
        .unwrap();
    drop(db);
    assert!(Store::open(&source.0).is_ok());
}

#[test]
fn late_receipt_is_retained_without_reauthorizing_old_attempt() {
    use spindle_store::completion::Attempt;
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let prepared = prepare(&mut store, "one");
    store.commit_proposal(&prepared).unwrap();
    let attempt = Attempt {
        id: "late".into(),
        effect: "effect-one".into(),
        owner_epoch: store.epoch(),
        implementation: "test-native-v1".into(),
        grant_reference: "grant".into(),
    };
    store.reserve_attempt(&attempt).unwrap();
    let before = store.snapshot().unwrap();
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert!(matches!(store.reserve_attempt(&attempt), Err(Error::Stale)));
    store.ingest_receipt(&attempt.id, b"late-result").unwrap();
    assert_eq!(store.snapshot().unwrap(), before);
    assert_eq!(
        store.receipt(&attempt.id).unwrap(),
        Some(b"late-result".to_vec())
    );
}

#[test]
fn proposal_commits_continuation_and_intents_once_across_restart() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let proposal = prepare(&mut store, "one");
    assert!(store.intents(1).unwrap().is_empty());
    assert_eq!(store.snapshot().unwrap().revision, 0);
    assert_eq!(store.commit_proposal(&proposal).unwrap(), 1);
    assert_eq!(
        store.snapshot().unwrap().continuation,
        proposal.continuation
    );
    assert_eq!(store.intents(1).unwrap(), proposal.effects);
    drop(store);
    let mut recovered = Store::open(&realm.0).unwrap();
    assert_eq!(recovered.commit_proposal(&proposal).unwrap(), 1);
    assert_eq!(recovered.intents(1).unwrap(), proposal.effects);
    assert_eq!(recovered.snapshot().unwrap().revision, 1);
    let mut changed = proposal.clone();
    changed.continuation.push(0);
    assert!(matches!(
        recovered.commit_proposal(&changed),
        Err(Error::Conflict)
    ));
    changed = proposal.clone();
    changed.id = "new-identity".into();
    assert!(matches!(
        recovered.commit_proposal(&changed),
        Err(Error::Stale)
    ));
    changed.owner_epoch = recovered.epoch();
    changed.expected_revision = 1;
    assert!(matches!(
        recovered.commit_proposal(&changed),
        Err(Error::Conflict)
    ));
    assert_eq!(recovered.snapshot().unwrap().revision, 1);
}

#[test]
fn proposals_bind_scope_head_epoch_and_effect_identity() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let original = prepare(&mut store, "one");
    let mut altered = original.clone();
    altered.scope = "other-operator".into();
    assert!(matches!(
        store.commit_proposal(&altered),
        Err(Error::Invalid)
    ));
    altered = original.clone();
    altered.expected_revision = 5;
    assert!(matches!(store.commit_proposal(&altered), Err(Error::Stale)));
    altered = original.clone();
    altered.owner_epoch += 1;
    assert!(matches!(store.commit_proposal(&altered), Err(Error::Stale)));
    altered = original.clone();
    altered.effects.push(altered.effects[0].clone());
    assert!(matches!(
        store.commit_proposal(&altered),
        Err(Error::Invalid)
    ));
    assert_eq!(store.snapshot().unwrap().revision, 0);
    store.commit_proposal(&original).unwrap();
    let mut next = prepare(&mut store, "two");
    next.effects = original.effects;
    assert!(matches!(store.commit_proposal(&next), Err(Error::Conflict)));
    assert_eq!(store.snapshot().unwrap().revision, 1);
    next.effects[0].id = "new-effect".into();
    assert_eq!(store.commit_proposal(&next).unwrap(), 2);
}

#[test]
fn failure_at_each_mutation_boundary_leaves_old_coherent_state() {
    for table in ["revisions", "consumed", "intents", "proposals", "head"] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let proposal = prepare(&mut store, "one");
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        let operation = if table == "head" { "UPDATE" } else { "INSERT" };
        // Trusted test fault injection, not a supported extension point.
        db.execute_batch(&format!("CREATE TRIGGER fail_boundary BEFORE {operation} ON {table} BEGIN SELECT RAISE(ABORT, 'injected'); END;")).unwrap();
        assert!(
            matches!(store.commit_proposal(&proposal), Err(Error::Sql(_))),
            "{table}"
        );
        assert!(matches!(store.snapshot(), Err(Error::Paused)));
        db.execute_batch("DROP TRIGGER fail_boundary").unwrap();
        drop(db);
        drop(store);
        let mut recovered = Store::open(&realm.0).unwrap();
        assert_eq!(recovered.snapshot().unwrap().revision, 0, "{table}");
        assert!(recovered.intents(1).unwrap().is_empty());
        // A proven absent old-epoch proposal cannot silently acquire fresh authority.
        assert!(matches!(
            recovered.commit_proposal(&proposal),
            Err(Error::Stale)
        ));
        let mut fresh = proposal.clone();
        fresh.owner_epoch = recovered.epoch();
        fresh.id = "fresh-proposal".into();
        assert_eq!(recovered.commit_proposal(&fresh).unwrap(), 1);
        assert_eq!(recovered.intents(1).unwrap(), fresh.effects);
    }
}

#[test]
#[ignore = "invoked by parent kill test"]
fn proposal_child() {
    let root = PathBuf::from(std::env::var_os("SPINDLE_PROPOSAL_REALM").unwrap());
    let mut store = Store::open(&root).unwrap();
    let proposal = prepare(&mut store, "one");
    store.commit_proposal(&proposal).unwrap();
    std::fs::write(root.join("test-ready"), []).unwrap();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn killed_owner_recovers_joint_continuation_and_intent_publication() {
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
            .args(["--exact", "proposal_child", "--ignored"])
            .env("SPINDLE_PROPOSAL_REALM", &realm.0)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let ready = loop {
        if realm.0.join("test-ready").is_file() {
            break true;
        }
        if child.0.try_wait().unwrap().is_some() || std::time::Instant::now() > deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    drop(child);
    assert!(ready);
    let mut recovered = Store::open(&realm.0).unwrap();
    let snapshot = recovered.snapshot().unwrap();
    assert_eq!(snapshot.revision, 1);
    assert_eq!(snapshot.continuation, b"waiting-for-effect");
    let intents = recovered.intents(1).unwrap();
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].id, "effect-one");
    let mut retry = prepare(&mut recovered, "one");
    retry.owner_epoch = 1;
    retry.expected_revision = 0;
    assert_eq!(recovered.commit_proposal(&retry).unwrap(), 1);
}

/// A step of `width` intents for `prepare`d `proposal` (ids prefixed by `name`).
fn widened(mut proposal: Proposal, name: &str, width: usize) -> Proposal {
    proposal.effects = (0..width)
        .map(|n| EffectIntent {
            id: format!("{name}-{n}"),
            adapter: "test-only".into(),
            input: vec![],
        })
        .collect();
    proposal
}

/// The schema a store's file records.
fn schema(root: &std::path::Path) -> i64 {
    rusqlite::Connection::open(root.join("authority.sqlite"))
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

/// Builds of schema 20 refuse a store holding a step of more than 8 intents: the
/// transaction that first commits one moves the store to schema 21, which they refuse
/// by its number; a store without one stays 20, readable by them, and a refused commit
/// moves nothing. A wide step an earlier build left at 20 moves it on the next open.
#[test]
fn a_step_wider_than_schema_20_holds_moves_its_store_to_21() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(schema(&realm.0), 20, "a new store");
    let narrow = widened(prepare(&mut store, "narrow"), "narrow", 8);
    store.commit_proposal(&narrow).unwrap();
    assert_eq!(schema(&realm.0), 20);
    let wide = widened(prepare(&mut store, "wide"), "wide", 9);
    let mut stale = wide.clone();
    stale.expected_revision = 0;
    assert!(matches!(store.commit_proposal(&stale), Err(Error::Stale)));
    assert_eq!(schema(&realm.0), 20, "a refused commit moves nothing");
    assert_eq!(store.commit_proposal(&wide).unwrap(), 2);
    assert_eq!(schema(&realm.0), 21);
    spindle_store::check_image(&store.image().unwrap()).unwrap();
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.intents(2).unwrap(), wide.effects);
    store.check_integrity().unwrap();
    drop(store);
    rusqlite::Connection::open(realm.0.join("authority.sqlite"))
        .unwrap()
        .execute_batch("PRAGMA user_version=20")
        .unwrap();
    drop(Store::open(&realm.0).unwrap());
    assert_eq!(schema(&realm.0), 21, "moved on open");
}

/// `spindle rollback` to a build of schema 20 is refused while a session store of the
/// host took a step wider than that schema holds, naming it (and only it); `--force`
/// restores the binary anyway, and with no such store the rollback goes ahead.
#[cfg(unix)]
#[test]
fn rollback_names_the_session_stores_the_kept_binary_could_not_open() {
    use spindle_store::upgrade::{previous, rollback};
    use std::os::unix::fs::PermissionsExt;
    let realm = Realm::new();
    let bin = realm.0.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let binary = |path: &std::path::Path, schema: u32| {
        std::fs::write(
            path,
            format!("#!/bin/sh\necho 'spindle 0.0.0 schema {schema} engine test build test'\n"),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    let target = bin.join("spindle");
    binary(&target, 21);
    binary(&previous(&target), 20);
    let reads = |path: &std::path::Path| std::fs::read_to_string(path).unwrap();
    for (name, width) in [("narrow", 8), ("wide", 9)] {
        let session = realm.0.join("sessions").join(name);
        std::fs::create_dir_all(&session).unwrap();
        let mut store = Store::open(&session.join("store")).unwrap();
        let step = widened(prepare(&mut store, name), name, width);
        store.commit_proposal(&step).unwrap();
    }
    let refused = rollback(&target, false).unwrap_err();
    assert!(
        refused.contains(&format!(
            "{} (schema 21)",
            realm.0.join("sessions/wide/store").display()
        )),
        "{refused}"
    );
    assert!(!refused.contains("narrow"), "{refused}");
    assert!(reads(&target).contains("schema 21"), "nothing swapped");
    rollback(&target, true).unwrap();
    assert!(reads(&target).contains("schema 20"));
    // Running it again undoes it: the kept binary reads every store.
    rollback(&target, false).unwrap();
    assert!(reads(&target).contains("schema 21"));
    std::fs::remove_dir_all(realm.0.join("sessions/wide")).unwrap();
    rollback(&target, false).unwrap();
    assert!(reads(&target).contains("schema 20"));
}

/// `doctor`'s full check reads every record's column types, also of those an earlier
/// open validated (an open reads only the ones above its watermark again).
#[test]
fn the_full_check_reads_the_types_of_records_an_open_passes_over() {
    use spindle_store::completion::Attempt;
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let step = prepare(&mut store, "one");
    store.commit_proposal(&step).unwrap();
    let attempt = Attempt {
        id: "attempt-one".into(),
        effect: "effect-one".into(),
        owner_epoch: store.epoch(),
        implementation: "test-native-v1".into(),
        grant_reference: "grant".into(),
    };
    store.reserve_attempt(&attempt).unwrap();
    store.ingest_receipt(&attempt.id, b"result").unwrap();
    drop(store);
    // Validated past the receipt.
    drop(Store::open(&realm.0).unwrap());
    rusqlite::Connection::open(realm.0.join("authority.sqlite"))
        .unwrap()
        .execute("UPDATE receipts SET result='not a blob'", [])
        .unwrap();
    let mut store = Store::open(&realm.0).unwrap();
    assert!(matches!(store.check_integrity(), Err(Error::Incompatible)));
}
