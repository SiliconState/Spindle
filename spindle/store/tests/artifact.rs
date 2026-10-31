use spindle_store::artifact::{MAX_OBJECT, Stage};
use spindle_store::{Command, Error, Store};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SERIAL: AtomicU64 = AtomicU64::new(0);
struct Realm(PathBuf);
impl Realm {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "spindle-artifact-{}-{}",
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
fn stage(store: &mut Store) -> Stage {
    store
        .admit(&Command {
            scope: "operator".into(),
            id: "request".into(),
            lineage: 0,
            kind: "turn".into(),
            payload: vec![],
        })
        .unwrap();
    let stage = Stage {
        id: "stage-one".into(),
        scope: "operator".into(),
        owner_epoch: store.epoch(),
    };
    store.begin_stage(&stage).unwrap();
    stage
}

#[test]
fn artifact_receipt_codec_is_bounded_exact_and_redacted() {
    use spindle_store::artifact::{ArtifactReceipt, MAX_RECEIPT_OBJECTS, ObjectReference};
    let receipt = ArtifactReceipt {
        objects: vec![ObjectReference {
            digest: "a".repeat(64),
            byte_length: MAX_OBJECT as u64,
        }],
        payload: b"sensitive receipt".to_vec(),
    };
    let bytes = receipt.encode().unwrap();
    assert_eq!(ArtifactReceipt::decode(&bytes).unwrap(), receipt);
    assert!(!format!("{receipt:?}").contains("sensitive"));
    for end in 0..bytes.len() {
        assert!(ArtifactReceipt::decode(&bytes[..end]).is_err());
    }
    for (index, value) in [(3, b'2'), (4, 9), (5, b'Z'), (69, 0xff)] {
        let mut invalid = bytes.clone();
        invalid[index] = value;
        assert!(ArtifactReceipt::decode(&invalid).is_err());
    }
    let mut invalid = bytes.clone();
    invalid.push(0);
    assert!(ArtifactReceipt::decode(&invalid).is_err());
    let mut duplicate = b"SAR1".to_vec();
    duplicate.push(2);
    duplicate.extend_from_slice(&bytes[5..77]);
    duplicate.extend_from_slice(&bytes[5..]);
    assert!(ArtifactReceipt::decode(&duplicate).is_err());
    let mut bad = receipt.clone();
    bad.objects.push(bad.objects[0].clone());
    assert!(bad.encode().is_err());
    let max = ArtifactReceipt {
        objects: (0..MAX_RECEIPT_OBJECTS)
            .map(|i| ObjectReference {
                digest: format!("{i:064x}"),
                byte_length: 0,
            })
            .collect(),
        payload: vec![0; spindle_store::MAX_PAYLOAD - 9 - MAX_RECEIPT_OBJECTS * 72],
    };
    assert_eq!(max.encode().unwrap().len(), spindle_store::MAX_PAYLOAD);
    assert_eq!(
        ArtifactReceipt::decode(&max.encode().unwrap()).unwrap(),
        max
    );
    let mut over = max;
    over.payload.push(0);
    assert!(over.encode().is_err());
    for index in 0..bytes.len() {
        let mut mutation = bytes.clone();
        mutation[index] ^= 0x80;
        if let Ok(decoded) = ArtifactReceipt::decode(&mutation) {
            assert_eq!(decoded.encode().unwrap(), mutation);
        }
    }
}

#[test]
fn immutable_objects_and_pins_survive_reopen_without_granting_old_authority() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let stage = stage(&mut store);
    let first = store.put_object(&stage, b"immutable").unwrap();
    assert_eq!(store.put_object(&stage, b"immutable").unwrap(), first);
    let other = store.put_object(&stage, b"changed").unwrap();
    assert_ne!(other, first);
    assert_eq!(store.read_object("operator", &first).unwrap(), b"immutable");
    assert!(matches!(
        store.read_object("other-operator", &first),
        Err(Error::Invalid)
    ));
    drop(store);
    let mut recovered = Store::open(&realm.0).unwrap();
    assert_eq!(
        recovered.read_object("operator", &first).unwrap(),
        b"immutable"
    );
    assert_eq!(
        recovered.read_object("operator", &other).unwrap(),
        b"changed"
    );
    assert!(matches!(
        recovered.put_object(&stage, b"new"),
        Err(Error::Stale)
    ));
    let mut fresh = stage.clone();
    fresh.owner_epoch = recovered.epoch();
    fresh.id = "stage-two".into();
    recovered.begin_stage(&fresh).unwrap();
    assert_eq!(recovered.put_object(&fresh, b"immutable").unwrap(), first);
}

#[test]
fn workspace_objects_share_pins_within_a_scope() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let first = stage(&mut store);
    let object = store
        .put_workspace_object(&first, b"unchanged file")
        .unwrap();
    let mut second = first.clone();
    second.id = "stage-two".into();
    store.begin_stage(&second).unwrap();
    let before = store.capacity().unwrap();
    let pins = |usage: &[(&str, i64, i64)]| usage.iter().find(|u| u.0 == "pins").unwrap().1;
    assert_eq!(
        store
            .put_workspace_object(&second, b"unchanged file")
            .unwrap(),
        object
    );
    assert_eq!(
        pins(&store.capacity().unwrap()),
        pins(&before),
        "shared, not re-pinned"
    );
    store.put_object(&second, b"unchanged file").unwrap();
    assert_eq!(
        pins(&store.capacity().unwrap()),
        pins(&before) + 1,
        "artifacts keep their own pin"
    );
    store
        .put_workspace_object(&second, b"changed file")
        .unwrap();
    assert_eq!(pins(&store.capacity().unwrap()), pins(&before) + 2);
    assert_eq!(
        store.read_object("operator", &object).unwrap(),
        b"unchanged file"
    );
    drop(store);
    assert!(Store::open(&realm.0).is_ok());
}

#[test]
fn stage_identity_bounds_and_cancellation_fail_closed() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let stage = Stage {
        id: "stage-one".into(),
        scope: "operator".into(),
        owner_epoch: store.epoch(),
    };
    assert!(matches!(store.begin_stage(&stage), Err(Error::Invalid)));
    assert!(matches!(
        store.put_object(&stage, b"unbound"),
        Err(Error::Invalid)
    ));
    let stage = self::stage(&mut store);
    store.begin_stage(&stage).unwrap();
    let mut changed = stage.clone();
    changed.scope = "other".into();
    assert!(matches!(store.begin_stage(&changed), Err(Error::Conflict)));
    assert!(matches!(
        store.put_object(&stage, &vec![0; MAX_OBJECT + 1]),
        Err(Error::Invalid)
    ));
    let retained = store.put_object(&stage, b"retained").unwrap();
    store.cancel_scope("operator", "cancel").unwrap();
    assert!(matches!(
        store.put_object(&stage, b"denied"),
        Err(Error::Cancelled)
    ));
    assert_eq!(
        store.read_object("operator", &retained).unwrap(),
        b"retained"
    );
}

#[test]
fn object_bytes_and_staging_pin_are_atomic() {
    for table in ["objects", "staging_pins"] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let stage = stage(&mut store);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        db.execute_batch(&format!("CREATE TRIGGER fail_object BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END;")).unwrap();
        assert!(matches!(
            store.put_object(&stage, b"bytes"),
            Err(Error::Sql(_))
        ));
        assert!(matches!(
            store.put_object(&stage, b"bytes"),
            Err(Error::Paused)
        ));
        db.execute_batch("DROP TRIGGER fail_object").unwrap();
        for table in ["objects", "staging_pins"] {
            let count: i64 = db
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0);
        }
        drop(db);
        drop(store);
        assert!(Store::open(&realm.0).is_ok());
    }
}

#[test]
fn ignored_object_or_pin_insert_rolls_back_and_fences_handle() {
    for table in ["objects", "staging_pins"] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let stage = stage(&mut store);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        db.execute_batch(&format!(
            "CREATE TRIGGER skip_insert BEFORE INSERT ON {table} BEGIN SELECT RAISE(IGNORE); END;"
        ))
        .unwrap();
        assert!(store.put_object(&stage, b"not-committed").is_err());
        assert!(matches!(store.snapshot(), Err(Error::Paused)));
        for table in ["objects", "staging_pins"] {
            assert_eq!(
                db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        db.execute_batch("DROP TRIGGER skip_insert").unwrap();
        drop(db);
        drop(store);
        assert!(Store::open(&realm.0).is_ok());
    }
}

#[test]
fn live_object_read_rejects_oversized_persisted_value() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let stage = stage(&mut store);
    let object = store.put_object(&stage, b"original").unwrap();
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute(
        "UPDATE objects SET bytes=zeroblob(?1)",
        [(MAX_OBJECT + 1) as i64],
    )
    .unwrap();
    assert!(store.read_object("operator", &object).is_err());
    assert!(matches!(store.snapshot(), Err(Error::Paused)));
}

#[test]
fn digest_damage_fences_live_read_and_every_read_after_reopen() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let stage = stage(&mut store);
    let id = store.put_object(&stage, b"original").unwrap();
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute("UPDATE objects SET bytes=X'00'", []).unwrap();
    assert!(matches!(
        store.read_object("operator", &id),
        Err(Error::Incompatible)
    ));
    assert!(matches!(
        store.read_object("operator", &id),
        Err(Error::Paused)
    ));
    drop(db);
    drop(store);
    // An open leaves objects to their reads (it never hashes them all): the damaged
    // one is refused wherever it is read, and by the full check.
    let mut store = Store::open(&realm.0).unwrap();
    assert!(matches!(
        store.read_object("operator", &id),
        Err(Error::Incompatible)
    ));
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert!(matches!(store.check_integrity(), Err(Error::Incompatible)));
}
