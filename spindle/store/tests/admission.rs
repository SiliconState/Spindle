#![cfg(target_os = "linux")]

use spindle_store::{Command, Error, MAX_PAYLOAD, Store};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SERIAL: AtomicU64 = AtomicU64::new(0);
struct Realm(PathBuf);
impl Realm {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "spindle-store-{}-{}",
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
fn command() -> Command {
    Command {
        scope: "local-operator".into(),
        id: "request-1".into(),
        lineage: 0,
        kind: "new-turn".into(),
        payload: b"canonical-test-payload".to_vec(),
    }
}

#[test]
fn single_component_relative_realm_can_be_created_and_reopened() {
    let realm = Realm(PathBuf::from(format!(
        "spindle-relative-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    )));
    let mut store = Store::open(&realm.0).unwrap();
    let receipt = store.admit(&command()).unwrap();
    drop(store);
    let mut reopened = Store::open(&realm.0).unwrap();
    assert_eq!(reopened.admit(&command()).unwrap(), receipt);
}

#[test]
fn missing_admission_history_is_not_silently_accepted() {
    for removal in [1, 2] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        store.admit(&command()).unwrap();
        let mut second = command();
        second.id = "second".into();
        store.admit(&second).unwrap();
        drop(store);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        db.execute("DELETE FROM admissions WHERE sequence=?1", [removal])
            .unwrap();
        assert!(Store::open(&realm.0).is_err());
        assert_eq!(
            db.query_row("SELECT epoch FROM realm", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}

#[test]
fn altered_schema_and_unexpected_triggers_are_refused_before_takeover() {
    for alteration in [
        "CREATE TRIGGER sabotage AFTER UPDATE ON realm BEGIN DELETE FROM admissions; END;",
        "ALTER TABLE admissions ADD COLUMN unexpected TEXT;",
        "ALTER TABLE admissions RENAME TO old_admissions; CREATE TABLE admissions AS SELECT * FROM old_admissions; DROP TABLE old_admissions;",
        "CREATE VIEW misleading AS SELECT * FROM realm;",
    ] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        store.admit(&command()).unwrap();
        drop(store);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        db.execute_batch(alteration).unwrap();
        assert!(
            Store::open(&realm.0).is_err(),
            "accepted altered schema: {alteration}"
        );
        assert_eq!(
            db.query_row("SELECT epoch FROM realm", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM admissions", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}

#[test]
fn previous_cleanup_free_schema_is_refused_without_takeover_or_migration() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    store.admit(&command()).unwrap();
    drop(store);
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute_batch("DROP TABLE image_executions; PRAGMA user_version=8;")
        .unwrap();
    assert!(matches!(Store::open(&realm.0), Err(Error::Incompatible)));
    assert_eq!(
        db.query_row("SELECT epoch FROM realm", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        8
    );
}

#[test]
fn missing_owner_lock_is_never_recreated_for_an_existing_realm() {
    let realm = Realm::new();
    let owner = Store::open(&realm.0).unwrap();
    std::fs::remove_file(realm.0.join("owner.lock")).unwrap();
    assert!(Store::open(&realm.0).is_err(), "opened a second lock inode");
    assert!(!realm.0.join("owner.lock").exists());
    drop(owner);
    assert!(
        Store::open(&realm.0).is_err(),
        "missing ownership evidence was reset"
    );
}

#[test]
fn hardlinked_storage_files_cannot_alias_another_realm() {
    for name in [
        "owner.lock",
        "authority.sqlite",
        "authority.sqlite-wal",
        "authority.sqlite-shm",
        "authority.sqlite-journal",
    ] {
        let realm = Realm::new();
        drop(Store::open(&realm.0).unwrap());
        let source = realm.0.join(name);
        if !source.exists() {
            std::fs::write(&source, []).unwrap();
        }
        std::fs::hard_link(&source, realm.0.join("alias")).unwrap();
        assert!(Store::open(&realm.0).is_err(), "accepted hardlinked {name}");
    }
}

#[test]
fn malformed_sql_types_labels_and_future_admissions_fail_before_epoch_increment() {
    for sql in [
        "UPDATE admissions SET admitted_epoch=99",
        "UPDATE admissions SET admitted_epoch=1.5",
        "UPDATE admissions SET payload='text-not-blob'",
        "UPDATE admissions SET scope='has space'",
        "UPDATE admissions SET command_id=CAST(X'610062' AS TEXT)",
        "UPDATE realm SET lineage=1.5",
    ] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        store.admit(&command()).unwrap();
        drop(store);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        db.execute(sql, []).unwrap();
        assert!(Store::open(&realm.0).is_err(), "accepted: {sql}");
        let epoch: i64 = db
            .query_row("SELECT epoch FROM realm", [], |r| r.get(0))
            .unwrap();
        assert_eq!(epoch, 1, "validation mutated ownership: {sql}");
    }
}

#[test]
fn lost_reply_and_restart_return_original_receipt() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let first = store.admit(&command()).unwrap();
    assert_eq!(store.admit(&command()).unwrap(), first);
    let epoch = store.epoch();
    drop(store);
    let mut recovered = Store::open(&realm.0).unwrap();
    assert_eq!(recovered.epoch(), epoch + 1);
    assert_eq!(recovered.admit(&command()).unwrap(), first);
    let mut another = command();
    another.id = "request-2".into();
    let next = recovered.admit(&another).unwrap();
    assert_eq!(next.sequence, first.sequence + 1);
    assert_eq!(next.admitted_epoch, epoch + 1);
}

#[test]
fn admission_quota_preserves_lookup_and_cancellation_capacity() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let original = store.admit(&command()).unwrap();
    // The quota is far beyond what one-by-one admission can reach in a test: fill the
    // rest in one transaction, exactly as admit stores them (payload packed raw).
    let epoch = store.epoch();
    drop(store);
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute(
        "WITH RECURSIVE n(i) AS (SELECT 2 UNION ALL SELECT i+1 FROM n WHERE i<?1)
         INSERT INTO admissions(sequence,scope,command_id,lineage,kind,payload,admitted_epoch)
         SELECT i,'local-operator','quota-'||i,0,'new-turn',?2,?3 FROM n",
        rusqlite::params![
            spindle_store::MAX_ADMISSIONS,
            spindle_store::codec::pack(&command().payload),
            epoch as i64
        ],
    )
    .unwrap();
    drop(db);
    let mut store = Store::open(&realm.0).unwrap();
    let mut overflow = command();
    overflow.id = "overflow".into();
    assert!(matches!(store.admit(&overflow), Err(Error::Exhausted)));
    assert_eq!(store.admit(&command()).unwrap(), original);
    store
        .cancel_scope("local-operator", "quota-cancel")
        .unwrap();
    drop(store);
    let mut recovered = Store::open(&realm.0).unwrap();
    assert_eq!(recovered.admit(&command()).unwrap(), original);
    assert!(matches!(recovered.admit(&overflow), Err(Error::Cancelled)));
}

#[test]
fn conflicts_and_stale_lineage_do_not_admit() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let original = store.admit(&command()).unwrap();
    let mut altered = command();
    altered.payload.push(0);
    assert!(matches!(store.admit(&altered), Err(Error::Conflict)));
    altered = command();
    altered.kind = "steer".into();
    assert!(matches!(store.admit(&altered), Err(Error::Conflict)));
    altered = command();
    altered.lineage = 1;
    assert!(matches!(store.admit(&altered), Err(Error::Conflict)));
    altered.id = "new-request".into();
    assert!(matches!(store.admit(&altered), Err(Error::Stale)));
    assert_eq!(store.admit(&command()).unwrap(), original);
    altered.lineage = 0;
    assert_eq!(
        store.admit(&altered).unwrap().sequence,
        original.sequence + 1
    );
}

#[test]
fn ownership_lock_and_database_pragmas_are_enforced() {
    let realm = Realm::new();
    let store = Store::open(&realm.0).unwrap();
    assert!(matches!(Store::open(&realm.0), Err(Error::Locked)));
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    let mode: String = db
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    drop(db);
    drop(store);
    assert!(Store::open(&realm.0).is_ok());
}

#[test]
fn graceful_drop_releases_owner_lock_even_while_a_child_is_before_exec() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;
    use std::time::Duration;

    struct Release(UnixStream);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = self.0.write_all(b"x");
        }
    }

    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let admission = store.admit(&command()).unwrap();
    let (mut ready, mut child_ready) = UnixStream::pair().unwrap();
    let (release, mut child_release) = UnixStream::pair().unwrap();
    ready
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    child_release
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut child_command = std::process::Command::new("/bin/true");
    unsafe {
        child_command.pre_exec(move || {
            child_ready.write_all(b"x")?;
            child_release.read_exact(&mut [0u8; 1])?;
            Ok(())
        });
    }
    std::thread::scope(|scope| {
        let spawned = scope.spawn(move || child_command.spawn());
        let release = Release(release);
        ready.read_exact(&mut [0u8; 1]).unwrap();
        assert!(matches!(Store::open(&realm.0), Err(Error::Locked)));
        drop(store);
        let reopened = Store::open(&realm.0);
        drop(release);
        assert!(spawned.join().unwrap().unwrap().wait().unwrap().success());
        let mut reopened = reopened.expect("pre-exec child must not retain released authority");
        assert_eq!(reopened.epoch(), 2);
        assert_eq!(reopened.admit(&command()).unwrap(), admission);
    });
}

#[test]
fn rejects_invalid_inputs_and_oversize_payload_before_admission() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let mut invalid = command();
    invalid.payload = vec![0; MAX_PAYLOAD + 1];
    assert!(matches!(
        store.admit(&invalid),
        Err(Error::TooLarge { size, limit }) if size == MAX_PAYLOAD + 1 && limit == MAX_PAYLOAD
    ));
    invalid = command();
    invalid.id.clear();
    assert!(matches!(store.admit(&invalid), Err(Error::Invalid)));
    invalid = command();
    invalid.lineage = u64::MAX;
    assert!(matches!(store.admit(&invalid), Err(Error::Invalid)));
    assert_eq!(store.admit(&command()).unwrap().sequence, 1);
}

#[test]
fn sql_failure_fences_handle_instead_of_continuing() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    // Deliberate trusted test-only corruption; production callers cannot access db.
    db.execute("DROP TABLE admissions", []).unwrap();
    assert!(matches!(store.admit(&command()), Err(Error::Sql(_))));
    assert!(matches!(store.admit(&command()), Err(Error::Paused)));
}

#[test]
fn incompatible_database_and_symlink_are_refused() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let realm = Realm::new();
    std::fs::create_dir(&realm.0).unwrap();
    std::fs::set_permissions(&realm.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute("CREATE TABLE other_app(data BLOB)", []).unwrap();
    drop(db);
    // Supply ownership evidence so this fixture reaches database identity validation.
    std::fs::write(realm.0.join("owner.lock"), []).unwrap();
    assert!(matches!(Store::open(&realm.0), Err(Error::Incompatible)));
    std::fs::remove_file(realm.0.join("authority.sqlite")).unwrap();
    symlink("missing-target", realm.0.join("authority.sqlite")).unwrap();
    assert!(matches!(Store::open(&realm.0), Err(Error::Invalid)));
}

#[test]
fn lost_or_empty_database_is_not_reinitialized() {
    for truncate in [false, true] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        store.admit(&command()).unwrap();
        drop(store);
        let database = realm.0.join("authority.sqlite");
        if truncate {
            std::fs::write(&database, []).unwrap();
        } else {
            std::fs::remove_file(&database).unwrap();
        }
        assert!(Store::open(&realm.0).is_err());
        if truncate {
            assert_eq!(std::fs::metadata(&database).unwrap().len(), 0);
        } else {
            assert!(!database.exists());
        }
    }
}

#[test]
fn malformed_retained_row_and_missing_table_block_reopen() {
    for damage in [
        "UPDATE admissions SET admitted_epoch=-1",
        "DROP TABLE admissions",
    ] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        store.admit(&command()).unwrap();
        drop(store);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        db.execute(damage, []).unwrap();
        drop(db);
        assert!(Store::open(&realm.0).is_err());
    }
}

#[test]
fn independent_scopes_do_not_share_receipts() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let first = store.admit(&command()).unwrap();
    let mut other = command();
    other.scope = "other-operator".into();
    let second = store.admit(&other).unwrap();
    assert_eq!(second.sequence, first.sequence + 1);
    assert_eq!(store.admit(&command()).unwrap(), first);
}

#[test]
#[ignore = "launched only by process-kill test with isolated test realm"]
fn crash_child() {
    let root = PathBuf::from(std::env::var_os("SPINDLE_TEST_REALM").unwrap());
    let mut store = Store::open(&root).unwrap();
    store.admit(&command()).unwrap();
    // External marker means the child returned from durable admission. The parent
    // never relies on process exit as evidence of commit or writer termination.
    std::fs::write(root.join("test-ready"), b"admitted").unwrap();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn process_kill_preserves_acknowledged_admission_and_releases_lock() {
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
            .args(["--exact", "crash_child", "--ignored", "--nocapture"])
            .env("SPINDLE_TEST_REALM", &realm.0)
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
    let locked = matches!(Store::open(&realm.0), Err(Error::Locked));
    drop(child);
    assert!(ready, "child failed before admission marker");
    assert!(locked, "second authority acquired live owner lock");
    let mut recovered = Store::open(&realm.0).unwrap();
    let receipt = recovered.admit(&command()).unwrap();
    assert_eq!(receipt.sequence, 1);
    assert_eq!(receipt.admitted_epoch, 1);
    assert_eq!(recovered.epoch(), 2);
}
