use rusqlite::Connection;
use spindle_store::transition::{Observation, Proposal};
use spindle_store::{
    Command, ConnectionProfile, Error, Initialization, JournalMode, OwnerExclusion, Store,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

struct Owned;
impl OwnerExclusion for Owned {}
static SERIAL: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "spindle-connection-{}-{}.sqlite",
            spindle_store::process_id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        )))
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
fn commit(store: &mut Store) {
    let command = Command {
        scope: "operator".into(),
        id: "input".into(),
        lineage: 0,
        kind: "test".into(),
        payload: b"retained".to_vec(),
    };
    let admission = store.admit(&command).unwrap();
    assert_eq!(store.admit(&command).unwrap(), admission);
    store
        .commit_proposal(&Proposal {
            id: "commit".into(),
            scope: command.scope,
            observation: Observation::Admission(admission.sequence),
            expected_revision: 0,
            owner_epoch: store.epoch(),
            engine: "fixture".into(),
            continuation: b"saved".to_vec(),
            workspace: None,
            effects: vec![],
            state: None,
        })
        .unwrap();
    store.check_integrity().unwrap();
}

#[test]
fn memory_and_rollback_connections_run_the_same_authority_and_reopen() {
    let mut memory = Store::from_connection(
        Connection::open_in_memory().unwrap(),
        ConnectionProfile::Memory,
        Owned,
    )
    .unwrap();
    commit(&mut memory);
    assert_eq!(memory.snapshot().unwrap().continuation, b"saved");
    let name = Temp::new();
    let profile = ConnectionProfile::Persistent {
        journal: JournalMode::Delete,
        initialization: Initialization::Create,
    };
    let mut persistent =
        Store::from_connection(Connection::open(&name.0).unwrap(), profile, Owned).unwrap();
    commit(&mut persistent);
    let identity = persistent.call_id("effect").unwrap();
    let epoch = persistent.epoch();
    drop(persistent);
    let mut reopened = Store::from_connection(
        Connection::open(&name.0).unwrap(),
        ConnectionProfile::Persistent {
            initialization: Initialization::Open,
            journal: JournalMode::Delete,
        },
        Owned,
    )
    .unwrap();
    assert_eq!(reopened.call_id("effect").unwrap(), identity);
    assert_eq!(reopened.epoch(), epoch + 1);
    assert_eq!(reopened.snapshot().unwrap().continuation, b"saved");
    assert_ne!(
        reopened.call_id("effect").unwrap(),
        memory.call_id("effect").unwrap()
    );
    reopened.check_integrity().unwrap();
}

#[test]
fn supplied_profiles_reject_transactions_mismatch_foreign_and_legacy_schema() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("BEGIN").unwrap();
    assert!(matches!(
        Store::from_connection(db, ConnectionProfile::Memory, Owned),
        Err(Error::Invalid)
    ));
    assert!(matches!(
        Store::from_connection(
            Connection::open_in_memory().unwrap(),
            ConnectionProfile::Persistent {
                journal: JournalMode::Delete,
                initialization: Initialization::Open
            },
            Owned
        ),
        Err(Error::Invalid)
    ));
    for setup in [
        "ATTACH ':memory:' AS extra",
        "CREATE TEMP TABLE trap(value)",
    ] {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(setup).unwrap();
        assert!(matches!(
            Store::from_connection(db, ConnectionProfile::Memory, Owned),
            Err(Error::Invalid)
        ));
    }
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE foreign_data(value)")
        .unwrap();
    assert!(matches!(
        Store::from_connection(db, ConnectionProfile::Memory, Owned),
        Err(Error::Incompatible)
    ));
    let name = Temp::new();
    drop(
        Store::from_connection(
            Connection::open(&name.0).unwrap(),
            ConnectionProfile::Persistent {
                journal: JournalMode::Wal,
                initialization: Initialization::Create,
            },
            Owned,
        )
        .unwrap(),
    );
    let db = Connection::open(&name.0).unwrap();
    db.pragma_update(None, "user_version", 18).unwrap();
    assert!(matches!(
        Store::from_connection(
            db,
            ConnectionProfile::Persistent {
                journal: JournalMode::Delete,
                initialization: Initialization::Open
            },
            Owned
        ),
        Err(Error::Incompatible)
    ));
}

#[test]
fn open_never_initializes_an_empty_file_and_create_never_replaces_existing_history() {
    let name = Temp::new();
    let profile = |initialization| ConnectionProfile::Persistent {
        journal: JournalMode::Delete,
        initialization,
    };
    assert!(matches!(
        Store::from_connection(
            Connection::open(&name.0).unwrap(),
            profile(Initialization::Open),
            Owned
        ),
        Err(Error::Incompatible)
    ));
    let mut created = Store::from_connection(
        Connection::open(&name.0).unwrap(),
        profile(Initialization::Create),
        Owned,
    )
    .unwrap();
    commit(&mut created);
    let identity = created.call_id("effect").unwrap();
    drop(created);
    assert!(matches!(
        Store::from_connection(
            Connection::open(&name.0).unwrap(),
            profile(Initialization::Create),
            Owned
        ),
        Err(Error::Incompatible)
    ));
    let mut reopened = Store::from_connection(
        Connection::open(&name.0).unwrap(),
        profile(Initialization::Open),
        Owned,
    )
    .unwrap();
    assert_eq!(reopened.call_id("effect").unwrap(), identity);
    assert_eq!(reopened.snapshot().unwrap().continuation, b"saved");
}

#[test]
fn sqlite_contention_maps_to_locked_before_takeover() {
    let name = Temp::new();
    let profile = ConnectionProfile::Persistent {
        journal: JournalMode::Delete,
        initialization: Initialization::Create,
    };
    let first = Store::from_connection(Connection::open(&name.0).unwrap(), profile, Owned).unwrap();
    assert!(matches!(
        Store::from_connection(Connection::open(&name.0).unwrap(), profile, Owned),
        Err(Error::Locked)
    ));
    let epoch = first.epoch();
    drop(first);
    let next = Store::from_connection(
        Connection::open(&name.0).unwrap(),
        ConnectionProfile::Persistent {
            journal: JournalMode::Delete,
            initialization: Initialization::Open,
        },
        Owned,
    )
    .unwrap();
    assert_eq!(next.epoch(), epoch + 1);
}

#[test]
fn busy_native_mutation_returns_locked_and_fences_without_admitting_work() {
    let name = Temp::new();
    let realm = name.0.with_extension("realm");
    let mut store = Store::open(&realm).unwrap();
    let db = Connection::open(realm.join("authority.sqlite")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let command = Command {
        scope: "operator".into(),
        id: "busy".into(),
        lineage: 0,
        kind: "test".into(),
        payload: b"retained".to_vec(),
    };
    assert!(matches!(store.admit(&command), Err(Error::Locked)));
    assert!(matches!(store.snapshot(), Err(Error::Paused)));
    db.execute_batch("ROLLBACK").unwrap();
    drop(db);
    drop(store);
    let mut reopened = Store::open(&realm).unwrap();
    assert!(reopened.admitted("operator", "busy").unwrap().is_none());
    assert_eq!(reopened.admit(&command).unwrap().sequence, 1);
    reopened.check_integrity().unwrap();
    drop(reopened);
    std::fs::remove_dir_all(realm).unwrap();
}

#[test]
fn host_exclusion_outlives_sqlite_on_success_and_failure() {
    struct Guard {
        path: PathBuf,
        dropped: Arc<AtomicBool>,
    }
    impl OwnerExclusion for Guard {}
    impl Drop for Guard {
        fn drop(&mut self) {
            let db = Connection::open(&self.path).unwrap();
            db.busy_timeout(std::time::Duration::ZERO).unwrap();
            db.execute_batch("BEGIN EXCLUSIVE; ROLLBACK").unwrap();
            self.dropped.store(true, Ordering::SeqCst);
        }
    }
    for corrupt in [0, 1, 2] {
        let name = Temp::new();
        let profile = ConnectionProfile::Persistent {
            journal: JournalMode::Delete,
            initialization: Initialization::Create,
        };
        drop(Store::from_connection(Connection::open(&name.0).unwrap(), profile, Owned).unwrap());
        if corrupt == 1 {
            Connection::open(&name.0)
                .unwrap()
                .execute_batch("CREATE VIEW unexpected AS SELECT 1")
                .unwrap();
        } else if corrupt == 2 {
            Connection::open(&name.0)
                .unwrap()
                .execute("UPDATE realm SET epoch=?1", [i64::MAX])
                .unwrap();
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Guard {
            path: name.0.clone(),
            dropped: dropped.clone(),
        };
        let result = Store::from_connection(
            Connection::open(&name.0).unwrap(),
            ConnectionProfile::Persistent {
                journal: JournalMode::Delete,
                initialization: Initialization::Open,
            },
            guard,
        );
        assert_eq!(result.is_err(), corrupt != 0);
        if corrupt == 2 {
            assert!(matches!(result, Err(Error::Exhausted)));
        }
        drop(result);
        assert!(dropped.load(Ordering::SeqCst));
    }
    // A fresh Create that fails while writing the new authority still closes SQLite
    // before the host's exclusion ends.
    let name = Temp::new();
    let db = Connection::open(&name.0).unwrap();
    db.pragma_update(None, "query_only", "ON").unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let result = Store::from_connection(
        db,
        ConnectionProfile::Persistent {
            journal: JournalMode::Delete,
            initialization: Initialization::Create,
        },
        Guard {
            path: name.0.clone(),
            dropped: dropped.clone(),
        },
    );
    assert!(matches!(result, Err(Error::Sql(_))));
    drop(result);
    assert!(dropped.load(Ordering::SeqCst));
}

#[test]
fn wal_connections_reopen_and_switch_journal_without_losing_history() {
    let name = Temp::new();
    let wal = |initialization| ConnectionProfile::Persistent {
        journal: JournalMode::Wal,
        initialization,
    };
    let mut created = Store::from_connection(
        Connection::open(&name.0).unwrap(),
        wal(Initialization::Create),
        Owned,
    )
    .unwrap();
    commit(&mut created);
    let identity = created.call_id("effect").unwrap();
    drop(created);
    for journal in [JournalMode::Wal, JournalMode::Delete, JournalMode::Wal] {
        let db = Connection::open(&name.0).unwrap();
        let mut reopened = Store::from_connection(
            db,
            ConnectionProfile::Persistent {
                journal,
                initialization: Initialization::Open,
            },
            Owned,
        )
        .unwrap();
        assert_eq!(reopened.call_id("effect").unwrap(), identity);
        assert_eq!(reopened.snapshot().unwrap().continuation, b"saved");
        reopened.check_integrity().unwrap();
    }
    let _ = std::fs::remove_file(name.0.with_extension("sqlite-wal"));
}

#[test]
fn zero_instance_identity_fails_closed_on_reopen() {
    let name = Temp::new();
    let profile = |initialization| ConnectionProfile::Persistent {
        journal: JournalMode::Delete,
        initialization,
    };
    drop(
        Store::from_connection(
            Connection::open(&name.0).unwrap(),
            profile(Initialization::Create),
            Owned,
        )
        .unwrap(),
    );
    Connection::open(&name.0)
        .unwrap()
        .execute("UPDATE instance SET identity=zeroblob(16)", [])
        .unwrap();
    assert!(matches!(
        Store::from_connection(
            Connection::open(&name.0).unwrap(),
            profile(Initialization::Open),
            Owned
        ),
        Err(Error::Incompatible)
    ));
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn the_native_host_refuses_a_store_the_embedded_package_holds() {
    let temp = Temp::new();
    let root = temp.0.with_extension("store");
    let _ = std::fs::remove_dir_all(&root);
    drop(Store::open(&root).unwrap());
    // What the package's Node storage creates while it owns the store.
    std::fs::write(
        root.join("authority.sqlite.lock"),
        "spindle-embed: pid 1 on elsewhere",
    )
    .unwrap();
    assert!(matches!(Store::open(&root), Err(Error::Locked)));
    std::fs::remove_file(root.join("authority.sqlite.lock")).unwrap();
    assert!(Store::open(&root).is_ok(), "once the operator removes it");
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn a_native_owner_holds_the_fence_the_package_respects_and_clears_its_own_stale_one() {
    let temp = Temp::new();
    let root = temp.0.with_extension("fenced");
    let _ = std::fs::remove_dir_all(&root);
    let fence = root.join(spindle_store::FENCE);
    let mut store = Store::open(&root).unwrap();
    commit(&mut store);
    let note = std::fs::read_to_string(&fence).unwrap();
    assert!(
        note.starts_with(spindle_store::NATIVE_FENCE),
        "an open native owner holds the fence: {note:?}"
    );
    assert!(matches!(Store::open(&root), Err(Error::Locked)));
    assert!(
        fence.exists(),
        "a refused open leaves the owner's fence alone"
    );
    drop(store);
    assert!(!fence.exists(), "closing gives the directory back");
    // A native owner that crashed leaves its fence; the next native owner (who gets
    // the flock) takes it over, and closing removes it.
    // (A longer stale note: the new one is written over it and cut to its own length.)
    let stale = format!("{}pid 1 {}\n", spindle_store::NATIVE_FENCE, "x".repeat(200));
    std::fs::write(&fence, &stale).unwrap();
    let mut store = Store::open(&root).unwrap();
    assert_eq!(store.snapshot().unwrap().revision, 1, "history kept");
    assert_eq!(
        std::fs::read_to_string(&fence).unwrap(),
        format!(
            "{}pid {}\n",
            spindle_store::NATIVE_FENCE,
            spindle_store::process_id()
        ),
        "the owner's own note, nothing of the old one left"
    );
    drop(store);
    assert!(!fence.exists());
    // An empty fence (a host stopped while making it) is nobody's to take over.
    std::fs::write(&fence, b"").unwrap();
    assert!(matches!(Store::open(&root), Err(Error::Locked)));
    // A note that only claims to be native in its text after the package's prefix is
    // the package's.
    std::fs::write(
        &fence,
        format!("spindle-embed: {}pid 1", spindle_store::NATIVE_FENCE),
    )
    .unwrap();
    assert!(matches!(Store::open(&root), Err(Error::Locked)));
    let _ = std::fs::remove_dir_all(&root);
}
