#![cfg(all(target_os = "linux", feature = "dext-image"))]

use spindle_store::artifact::{ArtifactReceipt, MAX_OBJECT, ObjectReference, Stage};
use spindle_store::completion::Attempt;
use spindle_store::image::{ImageReceipt, JpegDescriptor};
use spindle_store::transition::{EffectIntent, Observation, Proposal};
use spindle_store::{Command, Error, Store};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

// Produced by the isolated Dext image worker from a synthetic 2x1 RGB PNG.
const JPEG: &[u8] = include_bytes!("fixtures/dext-worker-2x1.jpg");
static SERIAL: AtomicU64 = AtomicU64::new(0);
struct Realm(PathBuf);
impl Realm {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "spindle-image-{}-{}",
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

fn descriptor() -> JpegDescriptor {
    JpegDescriptor {
        object: ObjectReference {
            digest: "a".repeat(64),
            byte_length: JPEG.len() as u64,
        },
        width: 2,
        height: 1,
    }
}

#[test]
fn image_codec_is_exact_bounded_and_has_no_path_or_grant() {
    let receipt = ImageReceipt {
        images: vec![descriptor()],
    };
    let bytes = receipt.encode().unwrap();
    assert_eq!(ImageReceipt::decode(&bytes).unwrap(), receipt);
    assert!(!format!("{receipt:?}").contains(&receipt.images[0].object.digest));
    assert!(ImageReceipt { images: vec![] }.encode().is_err());
    assert!(
        ImageReceipt {
            images: vec![descriptor(); 9]
        }
        .encode()
        .is_err()
    );
    assert!(
        ImageReceipt {
            images: vec![descriptor(); 2]
        }
        .encode()
        .is_err()
    );
    for field in 0..4 {
        let mut bad = descriptor();
        match field {
            0 => bad.width = 0,
            1 => bad.height = 1569,
            2 => bad.object.byte_length = 0,
            _ => bad.object.byte_length = MAX_OBJECT as u64 + 1,
        }
        assert!(ImageReceipt { images: vec![bad] }.encode().is_err());
    }
    let max = ImageReceipt {
        images: (0..8)
            .map(|i| JpegDescriptor {
                object: ObjectReference {
                    digest: format!("{i:064x}"),
                    byte_length: MAX_OBJECT as u64,
                },
                width: 1568,
                height: 1568,
            })
            .collect(),
    };
    assert_eq!(ImageReceipt::decode(&max.encode().unwrap()).unwrap(), max);
}

#[test]
fn image_codec_refuses_wrong_profile_shape_and_mutations_roundtrip() {
    let receipt = ImageReceipt {
        images: vec![descriptor()],
    };
    let bytes = receipt.encode().unwrap();
    for end in 0..bytes.len() {
        assert!(ImageReceipt::decode(&bytes[..end]).is_err());
    }
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(ImageReceipt::decode(&extra).is_err());
    let artifact = ArtifactReceipt::decode(&bytes).unwrap();
    for payload in [
        b"SIJ2".to_vec(),
        b"SIJ1".to_vec(),
        [
            b"SIJ1".as_slice(),
            &[2],
            &2u32.to_be_bytes(),
            &1u32.to_be_bytes(),
        ]
        .concat(),
    ] {
        let bad = ArtifactReceipt {
            objects: artifact.objects.clone(),
            payload,
        }
        .encode()
        .unwrap();
        assert!(ImageReceipt::decode(&bad).is_err());
    }
    for index in 0..bytes.len() {
        for mask in [1, 0x80, 0xff] {
            let mut bad = bytes.clone();
            bad[index] ^= mask;
            if let Ok(restored) = ImageReceipt::decode(&bad) {
                assert_eq!(restored.encode().unwrap(), bad);
            }
        }
    }
}

#[cfg(feature = "dext-native-image")]
fn pinned_worker() -> dext_native::image::PinnedWorker {
    use sha2::{Digest, Sha256};
    let path = std::path::Path::new(env!("CARGO_BIN_EXE_spindle-image-worker"));
    let expected: [u8; 32] = Sha256::digest(std::fs::read(path).unwrap()).into();
    dext_native::image::PinnedWorker::open(path, expected).unwrap()
}

#[cfg(feature = "dext-native-image")]
#[test]
fn sanitized_pixels_pin_and_provenance_commit_together_and_survive_reopen() {
    let worker = pinned_worker();
    let token = worker.sanitize(JPEG, None).unwrap();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let mut prepared = setup(&mut store, token.bytes(), token.width(), "operator");
    let stage = Stage {
        id: "image-stage".into(),
        scope: "operator".into(),
        owner_epoch: store.epoch(),
    };
    assert!(matches!(
        store.commit_verified_image_receipt_proposal(
            &prepared.proposal,
            &prepared.attempt,
            1,
            &prepared.intent,
            &prepared.receipt,
            *worker.digest()
        ),
        Err(Error::Invalid)
    ));
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_provenance BEFORE INSERT ON image_provenance BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(matches!(
        store.put_sanitized_image(&stage, &token),
        Err(Error::Sql(_))
    ));
    assert_eq!(
        db.query_row("SELECT count(*) FROM image_provenance", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    db.execute_batch("DROP TRIGGER fail_provenance").unwrap();
    drop(db);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    let stage = Stage {
        id: "verified-stage".into(),
        scope: "operator".into(),
        owner_epoch: store.epoch(),
    };
    store.begin_stage(&stage).unwrap();
    let image = store.put_sanitized_image(&stage, &token).unwrap();
    assert_eq!(store.put_sanitized_image(&stage, &token).unwrap(), image);
    assert_eq!(image, prepared.image);
    prepared.proposal.owner_epoch = store.epoch();
    assert!(matches!(
        store.commit_verified_image_receipt_proposal(
            &prepared.proposal,
            &prepared.attempt,
            1,
            &prepared.intent,
            &prepared.receipt,
            [0; 32]
        ),
        Err(Error::Invalid)
    ));
    assert_eq!(
        store
            .commit_verified_image_receipt_proposal(
                &prepared.proposal,
                &prepared.attempt,
                1,
                &prepared.intent,
                &prepared.receipt,
                *worker.digest()
            )
            .unwrap(),
        2
    );
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(
        store
            .commit_verified_image_receipt_proposal(
                &prepared.proposal,
                &prepared.attempt,
                1,
                &prepared.intent,
                &prepared.receipt,
                *worker.digest()
            )
            .unwrap(),
        2
    );
    assert!(matches!(
        store.commit_verified_image_receipt_proposal(
            &prepared.proposal,
            &prepared.attempt,
            1,
            &prepared.intent,
            &prepared.receipt,
            [0; 32]
        ),
        Err(Error::Invalid)
    ));
    assert_eq!(
        store.read_object("operator", &image.object.digest).unwrap(),
        token.bytes()
    );
}

#[cfg(feature = "dext-native-image")]
#[test]
fn disclosure_grants_bind_current_context_and_cannot_survive_revocation_or_restore() {
    use spindle_store::image::DisclosureContext;
    let worker = pinned_worker();
    let token = worker.sanitize(JPEG, None).unwrap();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let prepared = setup(&mut store, token.bytes(), token.width(), "operator");
    let stage = Stage {
        id: "image-stage".into(),
        scope: "operator".into(),
        owner_epoch: store.epoch(),
    };
    let context = DisclosureContext {
        scope: "operator".into(),
        route: "a".repeat(64),
        turn: "lineage-0/turn-1".into(),
        policy_generation: 1,
        approved_worker: *worker.digest(),
    };
    assert!(
        store
            .approve_image_disclosure(&prepared.image, &context)
            .is_err()
    );
    store.set_image_disclosure_context(context.clone()).unwrap();
    assert!(matches!(
        store.approve_image_disclosure(&prepared.image, &context),
        Err(Error::Invalid)
    ));
    store.put_sanitized_image(&stage, &token).unwrap();
    let grant = store
        .approve_image_disclosure(&prepared.image, &context)
        .unwrap();
    assert_eq!(
        store.image_bytes_for_disclosure(&grant, &context).unwrap(),
        token.bytes()
    );
    let mut wrong_image = prepared.image.clone();
    wrong_image.width += 1;
    assert!(
        store
            .approve_image_disclosure(&wrong_image, &context)
            .is_err()
    );
    for field in 0..5 {
        let mut next = context.clone();
        match field {
            0 => next.route = "b".repeat(64),
            1 => next.turn = "lineage-1/turn-1".into(),
            2 => next.policy_generation += 1,
            3 => next.scope = "other".into(),
            _ => next.approved_worker = [0; 32],
        }
        store.set_image_disclosure_context(next).unwrap();
        assert!(matches!(
            store.image_bytes_for_disclosure(&grant, &context),
            Err(Error::Stale)
        ));
        store.set_image_disclosure_context(context.clone()).unwrap();
        assert!(matches!(
            store.image_bytes_for_disclosure(&grant, &context),
            Err(Error::Stale)
        ));
    }
    let grant = store
        .approve_image_disclosure(&prepared.image, &context)
        .unwrap();
    store.revoke_image_disclosures();
    store.set_image_disclosure_context(context.clone()).unwrap();
    assert!(matches!(
        store.image_bytes_for_disclosure(&grant, &context),
        Err(Error::Stale)
    ));
    let grant = store
        .approve_image_disclosure(&prepared.image, &context)
        .unwrap();
    let mut invalid = context.clone();
    invalid.route = "invalid-route".into();
    assert!(store.set_image_disclosure_context(invalid).is_err());
    store.set_image_disclosure_context(context.clone()).unwrap();
    assert!(matches!(
        store.image_bytes_for_disclosure(&grant, &context),
        Err(Error::Stale)
    ));
    let grant = store
        .approve_image_disclosure(&prepared.image, &context)
        .unwrap();
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    store.set_image_disclosure_context(context.clone()).unwrap();
    assert!(matches!(
        store.image_bytes_for_disclosure(&grant, &context),
        Err(Error::Stale)
    ));
    let grant = store
        .approve_image_disclosure(&prepared.image, &context)
        .unwrap();
    store.cancel_scope("operator", "cancel").unwrap();
    assert!(matches!(
        store.image_bytes_for_disclosure(&grant, &context),
        Err(Error::Cancelled)
    ));
    assert!(!format!("{grant:?}{context:?}").contains(&context.route));
}

#[cfg(feature = "dext-native-image")]
#[test]
fn new_object_pin_and_provenance_rollback_on_ignored_provenance_insert() {
    let worker = pinned_worker();
    let token = worker.sanitize(JPEG, None).unwrap();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    store
        .admit(&Command {
            scope: "operator".into(),
            id: "admit".into(),
            lineage: 0,
            kind: "test".into(),
            payload: vec![],
        })
        .unwrap();
    let stage = Stage {
        id: "stage".into(),
        scope: "operator".into(),
        owner_epoch: store.epoch(),
    };
    store.begin_stage(&stage).unwrap();
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER ignore_provenance BEFORE INSERT ON image_provenance BEGIN SELECT RAISE(IGNORE); END;").unwrap();
    assert!(matches!(
        store.put_sanitized_image(&stage, &token),
        Err(Error::Incompatible)
    ));
    for table in ["objects", "staging_pins", "image_provenance"] {
        assert_eq!(
            db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    assert!(matches!(store.snapshot(), Err(Error::Paused)));
    db.execute_batch("DROP TRIGGER ignore_provenance").unwrap();
    drop(db);
    drop(store);
    assert!(Store::open(&realm.0).is_ok());
}

struct Prepared {
    proposal: Proposal,
    attempt: Attempt,
    intent: EffectIntent,
    receipt: Vec<u8>,
    image: JpegDescriptor,
}
impl Prepared {
    fn commit(&self, store: &mut Store) -> spindle_store::Result<u64> {
        store.commit_image_receipt_proposal(
            &self.proposal,
            &self.attempt,
            1,
            &self.intent,
            &self.receipt,
        )
    }
}
fn setup(store: &mut Store, source: &[u8], width: u32, pin_scope: &str) -> Prepared {
    let admission = store
        .admit(&Command {
            scope: "operator".into(),
            id: "image".into(),
            lineage: 0,
            kind: "test-only".into(),
            payload: vec![],
        })
        .unwrap();
    let intent = EffectIntent {
        id: "image-tool".into(),
        adapter: "inert-image".into(),
        input: b"synthetic".to_vec(),
    };
    store
        .commit_proposal(&Proposal {
            id: "prepare".into(),
            scope: "operator".into(),
            observation: Observation::Admission(admission.sequence),
            expected_revision: 0,
            owner_epoch: store.epoch(),
            engine: "image-test-v1".into(),
            continuation: vec![],
            workspace: None,
            effects: vec![intent.clone()],
            state: None,
        })
        .unwrap();
    if pin_scope != "operator" {
        store
            .admit(&Command {
                scope: pin_scope.into(),
                id: "foreign".into(),
                lineage: 0,
                kind: "test-only".into(),
                payload: vec![],
            })
            .unwrap();
    }
    let stage = Stage {
        id: "image-stage".into(),
        scope: pin_scope.into(),
        owner_epoch: store.epoch(),
    };
    store.begin_stage(&stage).unwrap();
    let image = JpegDescriptor {
        object: ObjectReference {
            digest: store.put_object(&stage, source).unwrap(),
            byte_length: source.len() as u64,
        },
        width,
        height: 1,
    };
    let receipt = ImageReceipt {
        images: vec![image.clone()],
    }
    .encode()
    .unwrap();
    let attempt = Attempt {
        id: "image-attempt".into(),
        effect: intent.id.clone(),
        owner_epoch: store.epoch(),
        implementation: "test-worker-v1".into(),
        grant_reference: "test-only".into(),
    };
    store.reserve_attempt(&attempt).unwrap();
    store.ingest_receipt(&attempt.id, &receipt).unwrap();
    let proposal = Proposal {
        id: "adopt-image".into(),
        scope: "operator".into(),
        observation: Observation::Receipt(attempt.id.clone()),
        expected_revision: 1,
        owner_epoch: store.epoch(),
        engine: "image-test-v1".into(),
        continuation: receipt.clone(),
        workspace: None,
        effects: vec![],
        state: None,
    };
    Prepared {
        proposal,
        attempt,
        intent,
        receipt,
        image,
    }
}

#[test]
fn real_worker_jpeg_adoption_rolls_back_reopens_and_rechecks_retries() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let mut prepared = setup(&mut store, JPEG, 2, "operator");
    let mut substituted = ImageReceipt::decode(&prepared.receipt).unwrap();
    substituted.images[0].width = 3;
    assert!(matches!(
        store.commit_image_receipt_proposal(
            &prepared.proposal,
            &prepared.attempt,
            1,
            &prepared.intent,
            &substituted.encode().unwrap()
        ),
        Err(Error::Conflict)
    ));
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_image BEFORE UPDATE ON head BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(matches!(prepared.commit(&mut store), Err(Error::Sql(_))));
    db.execute_batch("DROP TRIGGER fail_image").unwrap();
    drop(db);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.snapshot().unwrap().revision, 1);
    assert_eq!(
        store.receipt(&prepared.attempt.id).unwrap().unwrap(),
        prepared.receipt
    );
    assert!(matches!(prepared.commit(&mut store), Err(Error::Stale)));
    prepared.proposal.owner_epoch = store.epoch();
    assert_eq!(prepared.commit(&mut store).unwrap(), 2);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    store.cancel_scope("operator", "cancel").unwrap();
    assert_eq!(prepared.commit(&mut store).unwrap(), 2);
    assert_eq!(
        store
            .read_object("operator", &prepared.image.object.digest)
            .unwrap(),
        JPEG
    );
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute(
        "UPDATE objects SET bytes=X'00' WHERE digest=?1",
        [&prepared.image.object.digest],
    )
    .unwrap();
    assert!(matches!(
        prepared.commit(&mut store),
        Err(Error::Incompatible)
    ));
    assert!(matches!(store.snapshot(), Err(Error::Paused)));
}

#[test]
fn immutable_but_nonprofile_bytes_and_wrong_dimensions_never_publish() {
    let mut exif = JPEG.to_vec();
    exif.splice(2..2, [0xff, 0xe1, 0, 8, b'E', b'x', b'i', b'f', 0, 0]);
    let mut trailing = JPEG.to_vec();
    trailing.push(0);
    for (source, width) in [
        (b"not an image".as_slice(), 2),
        (b"\x89PNG\r\n\x1a\n".as_slice(), 2),
        (&JPEG[..JPEG.len() - 2], 2),
        (&exif, 2),
        (&trailing, 2),
        (JPEG, 3),
    ] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let prepared = setup(&mut store, source, width, "operator");
        assert!(matches!(prepared.commit(&mut store), Err(Error::Invalid)));
        assert_eq!(store.snapshot().unwrap().revision, 1);
        assert_eq!(
            store.receipt(&prepared.attempt.id).unwrap().unwrap(),
            prepared.receipt
        );
        assert!(store.intents(2).unwrap().is_empty());
    }
}

#[test]
fn image_adoption_refuses_foreign_pins_and_cancelled_publication() {
    for foreign in [false, true] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let prepared = setup(
            &mut store,
            JPEG,
            2,
            if foreign { "other" } else { "operator" },
        );
        if !foreign {
            store.cancel_scope("operator", "cancel").unwrap();
        }
        assert!(matches!(
            prepared.commit(&mut store),
            Err(Error::Invalid | Error::Cancelled)
        ));
        assert_eq!(store.snapshot().unwrap().revision, 1);
    }
}
