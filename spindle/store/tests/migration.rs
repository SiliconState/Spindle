//! Schema 14 -> 15 against a store written by the schema-14 build (eb20a6eac58f with a
//! scripted local provider: three turns of model calls and sandboxed bash, a history
//! compaction, a rewind and a turn after it). The fixture is raw-deflated to keep the
//! repository small.
#![cfg(feature = "dext-turn")]
use rusqlite::{Connection, params};
use spindle_store::artifact::Stage;
use spindle_store::view::View;
use spindle_store::{Command, Error, LEGACY_BACKUP, MAX_PAYLOAD, Store, codec};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SERIAL: AtomicU64 = AtomicU64::new(0);
struct Realm(PathBuf);
impl Realm {
    /// A private store directory holding the schema-14 fixture.
    fn legacy() -> Self {
        let realm = Self(std::env::temp_dir().join(format!(
            "spindle-migration-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        )));
        let _ = std::fs::remove_dir_all(&realm.0);
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&realm.0).unwrap();
        let packed = include_bytes!("fixtures/schema14.sqlite.deflate");
        let bytes = miniz_oxide::inflate::decompress_to_vec(packed).unwrap();
        std::fs::write(realm.0.join("authority.sqlite"), bytes).unwrap();
        std::fs::write(realm.0.join("owner.lock"), b"").unwrap();
        realm
    }
}
impl Drop for Realm {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const COLUMNS: [(&str, &str, &str); 5] = [
    ("admissions", "sequence", "payload"),
    ("revisions", "revision", "continuation"),
    ("proposals", "id", "canonical"),
    ("intents", "id", "input"),
    ("receipts", "attempt", "result"),
];

/// Every large value by (table, key), as stored.
fn values(path: &Path) -> BTreeMap<(String, String), Vec<u8>> {
    let db = Connection::open(path).unwrap();
    let mut out = BTreeMap::new();
    for (table, key, column) in COLUMNS {
        let mut statement = db
            .prepare(&format!("SELECT CAST({key} AS TEXT),{column} FROM {table}"))
            .unwrap();
        let rows = statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get(1)?)))
            .unwrap();
        for row in rows {
            let (id, value) = row.unwrap();
            out.insert((table.to_string(), id), value);
        }
    }
    out
}

/// Every object by digest: its stored bytes and `size` (None before schema 20).
fn objects(path: &Path) -> BTreeMap<String, (Vec<u8>, Option<i64>)> {
    let db = Connection::open(path).unwrap();
    let sized = db
        .prepare("SELECT 1 FROM pragma_table_info('objects') WHERE name='size'")
        .unwrap()
        .exists([])
        .unwrap();
    let sql = if sized {
        "SELECT digest,bytes,size FROM objects"
    } else {
        "SELECT digest,bytes,NULL FROM objects"
    };
    let mut statement = db.prepare(sql).unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Makes a migrated store the schema-19 store the previous build left: every object
/// kept as it is, no `size` column.
fn as_schema_19(database: &Path) {
    let db = Connection::open(database).unwrap();
    for (digest, (bytes, size)) in objects(database) {
        if size.is_some() {
            let whole = miniz_oxide::inflate::decompress_to_vec(&bytes).unwrap();
            db.execute(
                "UPDATE objects SET bytes=?1,size=NULL WHERE digest=?2",
                params![whole, digest],
            )
            .unwrap();
        }
    }
    db.execute_batch("ALTER TABLE objects DROP COLUMN size; PRAGMA user_version=19;")
        .unwrap();
}

fn schema(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

#[test]
fn a_schema_14_store_migrates_in_place_losslessly_and_keeps_a_backup() {
    let realm = Realm::legacy();
    let database = realm.0.join("authority.sqlite");
    assert_eq!(schema(&database), 14);
    let original = values(&database);
    assert!(original.len() > 80, "the fixture covers real steps");
    let raw: usize = original.values().map(Vec::len).sum();

    // An observer reads the unmigrated store before its owner opens it.
    let mut view = View::open(&realm.0).unwrap();
    let (head, continuation, _) = view.read(|view| view.head()).unwrap();
    assert_eq!(
        continuation,
        original[&("revisions".to_string(), head.to_string())]
    );
    assert!(view.read(|view| view.steps(0)).unwrap().len() as u64 >= head - 1);

    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(schema(&database), 20);
    let migrated = values(&database);
    assert_eq!(
        migrated.keys().collect::<Vec<_>>(),
        original.keys().collect::<Vec<_>>()
    );
    let reader = Connection::open(&database).unwrap();
    for (key, stored) in &migrated {
        assert_eq!(
            &codec::load(&reader, stored, usize::MAX).unwrap(),
            &original[key],
            "{key:?}"
        );
    }
    let chunks: i64 = reader
        .query_row(
            "SELECT coalesce(sum(length(bytes)),0) FROM chunks",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let packed = migrated.values().map(Vec::len).sum::<usize>() + chunks as usize;
    assert!(packed * 3 < raw, "{raw} -> {packed}");
    drop(reader);

    // The backup is the schema-14 store, byte for byte in every value.
    let backup = realm.0.join(LEGACY_BACKUP);
    assert_eq!(schema(&backup), 14);
    assert_eq!(values(&backup), original);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&backup).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "backup is private: {mode:o}");
    }
    assert!(!realm.0.join(format!("{LEGACY_BACKUP}.new")).exists());

    // Reads through the store and the view see the same logical bytes as before.
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.revision, head);
    assert_eq!(snapshot.continuation, continuation);
    for revision in 1..=head {
        for intent in store.intents(revision).unwrap() {
            assert_eq!(intent.input, original[&("intents".to_string(), intent.id)]);
        }
    }
    let receipt = original
        .keys()
        .find(|(table, _)| table == "receipts")
        .unwrap()
        .1
        .clone();
    assert_eq!(
        store.receipt(&receipt).unwrap().unwrap(),
        original[&("receipts".to_string(), receipt)]
    );
    assert!(
        store
            .reconciliation()
            .unwrap()
            .unresolved_attempts
            .is_empty()
    );
    let targets: Vec<_> = store
        .rewind_targets("operator")
        .unwrap()
        .into_iter()
        .map(|target| (target.revision, target.kind))
        .collect();
    assert!(!targets.is_empty());
    // The cheap listing the web host uses agrees with the full check.
    let mut listed = view
        .read(|view| Ok(view.targets(&(1..=head).collect::<Vec<_>>())))
        .unwrap();
    listed.reverse();
    let mut full = targets.clone();
    full.sort_by_key(|(revision, _)| std::cmp::Reverse(*revision));
    listed.sort_by_key(|(revision, _)| std::cmp::Reverse(*revision));
    assert_eq!(listed, full);
    let recent = view.read(|view| view.recent_steps(3)).unwrap();
    assert_eq!(
        recent.iter().map(|step| step.revision).collect::<Vec<_>>(),
        vec![head - 2, head - 1, head]
    );
    let (_, after, _) = view.read(|view| view.head()).unwrap();
    assert_eq!(after, continuation);

    // A value far past the old 256 KiB cap is kept now; one past the cap is refused
    // with its size.
    let lineage = store.lineage().unwrap();
    let large = Command {
        scope: "operator".into(),
        id: "large".into(),
        lineage,
        kind: "test-large".into(),
        payload: br#"{"text":"spindle"}"#.repeat(200_000),
    };
    store.admit(&large).unwrap();
    assert_eq!(
        store.admit(&large).unwrap().sequence,
        store.admit(&large).unwrap().sequence
    );
    let refused = Command {
        id: "too-large".into(),
        payload: vec![1; MAX_PAYLOAD + 1],
        ..large.clone()
    };
    let error = store.admit(&refused).unwrap_err();
    assert!(
        matches!(error, Error::TooLarge { size, limit } if size == MAX_PAYLOAD + 1 && limit == MAX_PAYLOAD)
    );
    // Sizes that round alike are told in exact bytes.
    assert!(error.to_string().contains("33554433 bytes"), "{error}");
    drop(store);

    // Reopening does not migrate again or touch the backup.
    let saved = std::fs::read(&backup).unwrap();
    let store = Store::open(&realm.0).unwrap();
    assert_eq!(std::fs::read(&backup).unwrap(), saved);
    drop(view);
    drop(store);
    // Closing the last writer checkpoints the log; the migration's VACUUM returned the freed space.
    let size = |path: &Path| std::fs::metadata(path).unwrap().len();
    assert!(
        size(&database) * 2 < size(&backup),
        "the migrated file shrinks: {} vs {} (wal {:?})",
        size(&database),
        size(&backup),
        std::fs::metadata(realm.0.join("authority.sqlite-wal"))
            .map(|m| m.len())
            .ok()
    );
}

#[test]
fn schema_17_migrates_without_reencoding_and_metadata_is_retained() {
    let realm = Realm::legacy();
    let database = realm.0.join("authority.sqlite");
    drop(Store::open(&realm.0).unwrap());
    as_schema_19(&database);
    Connection::open(&database)
        .unwrap()
        .execute_batch("DROP TABLE task_runs; DROP TABLE retry_delays; DROP TABLE app_details; DROP TABLE instance; DROP TABLE meta; PRAGMA user_version=17;")
        .unwrap();
    let original = values(&database);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(schema(&database), 20);
    assert_eq!(values(&database), original);
    store
        .set_metadata("settings", b"{\"model\":\"fixture\"}")
        .unwrap();
    drop(store);
    assert_eq!(schema(&realm.0.join("authority.v17.sqlite")), 17);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(
        store.metadata("settings").unwrap().unwrap(),
        b"{\"model\":\"fixture\"}"
    );
    assert_eq!(values(&database), original);
    store.check_integrity().unwrap();
}

#[test]
fn schema_18_preserves_all_metadata_and_history_and_keeps_its_backup() {
    let realm = Realm::legacy();
    let database = realm.0.join("authority.sqlite");
    let mut store = Store::open(&realm.0).unwrap();
    let metadata = [
        ("settings", br#"{"model":"fixture"}"#.as_slice()),
        ("pending_settings", br#"{"effort":"high"}"#),
        ("output_limits", br#"{"fixture":4096}"#),
        ("usage", br#"{"calls":[]}"#),
    ];
    for (key, value) in metadata {
        store.set_metadata(key, value).unwrap();
    }
    drop(store);
    as_schema_19(&database);
    Connection::open(&database).unwrap().execute_batch("DROP TABLE task_runs; DROP TABLE retry_delays; DROP TABLE app_details; DROP TABLE instance; PRAGMA user_version=18;").unwrap();
    let original = values(&database);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(schema(&database), 20);
    assert_eq!(values(&database), original);
    for (key, value) in metadata {
        assert_eq!(store.metadata(key).unwrap().unwrap(), value);
    }
    let identity = store.call_id("fixture").unwrap();
    store.check_integrity().unwrap();
    drop(store);
    let backup = realm.0.join("authority.v18.sqlite");
    assert_eq!(schema(&backup), 18);
    assert_eq!(values(&backup), original);
    let store = Store::open(&realm.0).unwrap();
    assert_eq!(store.call_id("fixture").unwrap(), identity);
}

#[test]
fn other_schemas_are_refused_untouched() {
    for version in [13, 21] {
        let realm = Realm::legacy();
        let database = realm.0.join("authority.sqlite");
        Connection::open(&database)
            .unwrap()
            .pragma_update(None, "user_version", version)
            .unwrap();
        let before = values(&database);
        assert!(matches!(Store::open(&realm.0), Err(Error::Incompatible)));
        assert!(matches!(View::open(&realm.0), Err(Error::Incompatible)));
        assert_eq!(schema(&database), version);
        assert_eq!(values(&database), before);
        assert!(!realm.0.join(LEGACY_BACKUP).exists());
    }
}

#[test]
fn damaged_schema_14_values_fail_validation_after_a_lossless_migration() {
    // Migration only re-encodes; the full validation that follows still sees the
    // damage, and the backup keeps the original bytes.
    let realm = Realm::legacy();
    let database = realm.0.join("authority.sqlite");
    Connection::open(&database)
        .unwrap()
        .execute("UPDATE intents SET input=X'00' WHERE rowid=1", [])
        .unwrap();
    assert!(Store::open(&realm.0).is_err());
    assert_eq!(schema(&realm.0.join(LEGACY_BACKUP)), 14);
}

#[test]
fn a_schema_16_store_gains_app_state_and_its_open_record_and_keeps_a_backup() {
    // A schema-16 store is a 17 one without the tables 17 added: make one from the
    // fixture, as the previous build left it.
    let realm = Realm::legacy();
    let database = realm.0.join("authority.sqlite");
    drop(Store::open(&realm.0).unwrap());
    std::fs::remove_file(realm.0.join(LEGACY_BACKUP)).unwrap();
    as_schema_19(&database);
    {
        let db = Connection::open(&database).unwrap();
        db.execute_batch(
            "DROP TABLE task_runs; DROP TABLE retry_delays; DROP TABLE app_details; DROP TABLE instance; DROP TABLE meta; DROP TABLE app_patches; DROP TABLE opens; DROP TABLE app_state; PRAGMA user_version=16;",
        )
        .unwrap();
    }
    let original = values(&database);
    // An observer reads the unmigrated store: no app state yet, which reads as empty.
    let mut view = View::open(&realm.0).unwrap();
    let head = view.read(|view| view.head()).unwrap().0;
    assert_eq!(
        view.read(|view| view.app_state("operator", head)).unwrap(),
        b"{}"
    );
    drop(view);

    let store = Store::open(&realm.0).unwrap();
    assert_eq!(schema(&database), 20);
    assert_eq!(
        values(&database),
        original,
        "16 values are kept as they are"
    );
    assert_eq!(store.app_state("operator", head).unwrap(), b"{}");
    assert_eq!(store.crashes(), 0);
    drop(store);
    let backup = realm.0.join("authority.v16.sqlite");
    assert_eq!(schema(&backup), 16);
    assert_eq!(values(&backup), original);
    // The open record counts owners that stop without closing at the same head.
    let killed = Store::open(&realm.0).unwrap();
    std::mem::forget(killed);
    // (A forgotten store keeps its owner lock until the test process ends; the plain
    // connection below only reads.)
    let db = Connection::open(&database).unwrap();
    let closed: i64 = db
        .query_row("SELECT closed FROM opens WHERE id=1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(closed, 0, "an owner that never closed is not marked closed");
}

/// The fixture at schema 19 with a large text file and an incompressible one among its
/// objects (pinned by a stage of the fixture's scope, as a command's files are).
fn schema_19_realm() -> (Realm, Vec<u8>, Vec<u8>) {
    let realm = Realm::legacy();
    let database = realm.0.join("authority.sqlite");
    let text = br#"{"row":"spindle keeps a workspace","n":12345}"#.repeat(4096);
    let mut noise = vec![0u8; 256 * 1024];
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    for byte in &mut noise {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *byte = seed as u8;
    }
    let mut store = Store::open(&realm.0).unwrap();
    let stage = Stage {
        id: "fixture-files".into(),
        scope: "operator".into(),
        owner_epoch: store.epoch(),
    };
    store.begin_stage(&stage).unwrap();
    store.put_object(&stage, &text).unwrap();
    store.put_object(&stage, &noise).unwrap();
    drop(store);
    std::fs::remove_file(realm.0.join(LEGACY_BACKUP)).unwrap();
    as_schema_19(&database);
    (realm, text, noise)
}

#[test]
fn a_schema_19_store_packs_its_objects_in_place_and_keeps_a_backup() {
    let (realm, text, noise) = schema_19_realm();
    let database = realm.0.join("authority.sqlite");
    let before = objects(&database);
    assert!(before.values().all(|(_, size)| size.is_none()));
    // An observer reads the unmigrated store: sizes and bytes as they are.
    let digest = spindle_store::artifact::object_digest(&text);
    let mut view = View::open(&realm.0).unwrap();
    assert_eq!(view.read(|view| view.object(&digest)).unwrap(), text);
    drop(view);

    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(schema(&database), 20);
    let after = objects(&database);
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        before.keys().collect::<Vec<_>>()
    );
    let (stored, size) = &after[&digest];
    assert_eq!(*size, Some(text.len() as i64));
    assert!(
        stored.len() * 20 < text.len(),
        "{} -> {}",
        text.len(),
        stored.len()
    );
    let noisy = spindle_store::artifact::object_digest(&noise);
    assert_eq!(
        after[&noisy],
        (noise.clone(), None),
        "incompressible bytes stay as they are"
    );
    for (digest, (bytes, _)) in &before {
        if bytes.len() < 1024 {
            assert_eq!(
                after[digest],
                (bytes.clone(), None),
                "small objects stay as they are"
            );
        }
        assert_eq!(&store.read_object("operator", digest).unwrap(), bytes);
    }
    store.check_integrity().unwrap();
    drop(store);

    // The backup is the schema-19 store; the view reads both alike.
    let backup = realm.0.join("authority.v19.sqlite");
    assert_eq!(schema(&backup), 19);
    assert_eq!(objects(&backup), before);
    let mut view = View::open(&realm.0).unwrap();
    assert_eq!(view.read(|view| view.object(&digest)).unwrap(), text);
    let size = |path: &Path| std::fs::metadata(path).unwrap().len();
    assert!(
        size(&database) + text.len() as u64 / 2 < size(&backup),
        "the packed file gives the space back: {} vs {}",
        size(&database),
        size(&backup)
    );
}

#[test]
fn a_damaged_packed_object_is_refused_where_it_is_read() {
    let (realm, text, _) = schema_19_realm();
    let database = realm.0.join("authority.sqlite");
    drop(Store::open(&realm.0).unwrap());
    let digest = spindle_store::artifact::object_digest(&text);
    let (stored, size) = objects(&database)[&digest].clone();
    let db = Connection::open(&database).unwrap();
    // A stream that inflates to more or fewer bytes than its row says, a size it could
    // not have been packed from, and flipped bytes inside the stream.
    let mut flipped = stored.clone();
    let at = flipped.len() / 2;
    flipped[at] ^= 0x5a;
    for (bytes, size, opens) in [
        (stored.clone(), size.map(|size| size + 1), true),
        (stored.clone(), size.map(|size| size - 1), true),
        (flipped, size, true),
        // A size no packed row can have is refused by the open's own checks.
        (stored.clone(), Some(stored.len() as i64), false),
    ] {
        db.execute(
            "UPDATE objects SET bytes=?1,size=?2 WHERE digest=?3",
            params![bytes, size, digest],
        )
        .unwrap();
        // An open does not inflate every object; the read and the full check refuse it.
        if opens {
            let mut store = Store::open(&realm.0).unwrap();
            assert!(store.read_object("operator", &digest).is_err());
            drop(store);
            let mut store = Store::open(&realm.0).unwrap();
            assert!(store.check_integrity().is_err());
        } else {
            assert!(Store::open(&realm.0).is_err());
        }
    }
    // A size that is not a whole number of bytes is refused by the open too.
    db.execute(
        "UPDATE objects SET bytes=?1,size=?2 WHERE digest=?3",
        params![stored, size.unwrap() as f64 + 0.5, digest],
    )
    .unwrap();
    assert!(Store::open(&realm.0).is_err());
}
