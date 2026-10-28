use spindle_store::completion::Attempt;
use spindle_store::ownership::{MAX_DEPTH, MAX_WAITS, ScopeKind, ScopeState};
use spindle_store::transition::{EffectIntent, Observation, Proposal};
use spindle_store::{Command, Error, Store};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SERIAL: AtomicU64 = AtomicU64::new(0);
struct Realm(PathBuf);
impl Realm {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "spindle-owned-{}-{}",
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
fn command(scope: &str) -> Command {
    Command {
        scope: scope.into(),
        id: "create".into(),
        lineage: 0,
        kind: "fixture".into(),
        payload: b"synthetic".to_vec(),
    }
}
fn root(store: &mut Store) {
    store
        .admit_owned(None, ScopeKind::Task, &command("root"))
        .unwrap();
}
fn child(store: &mut Store, parent: &str, scope: &str, kind: ScopeKind) {
    store
        .admit_owned(Some(parent), kind, &command(scope))
        .unwrap();
}

#[test]
fn attachment_is_atomic_idempotent_and_survives_takeover_without_abandoning_work() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    root(&mut store);
    let admitted = store
        .admit_owned(Some("root"), ScopeKind::Task, &command("child"))
        .unwrap();
    assert_eq!(
        store
            .admit_owned(Some("root"), ScopeKind::Task, &command("child"))
            .unwrap(),
        admitted
    );
    for parent in [None, Some("different")] {
        assert!(matches!(
            store.admit_owned(parent, ScopeKind::Task, &command("child")),
            Err(Error::Conflict)
        ));
    }
    assert!(matches!(
        store.admit_owned(Some("root"), ScopeKind::Service, &command("child")),
        Err(Error::Conflict)
    ));
    let mut changed = command("child");
    changed.payload.push(1);
    assert!(matches!(
        store.admit_owned(Some("root"), ScopeKind::Task, &changed),
        Err(Error::Conflict)
    ));
    let before = store.owned_scopes().unwrap();
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.owned_scopes().unwrap(), before);
    assert_eq!(
        store
            .admit_owned(Some("root"), ScopeKind::Task, &command("child"))
            .unwrap(),
        admitted
    );
    assert!(
        store
            .owned_scopes()
            .unwrap()
            .iter()
            .all(|node| node.state == ScopeState::Active)
    );
}

#[test]
fn subtree_cancel_and_bottom_up_settlement_preserve_longer_lived_background_owner() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    root(&mut store);
    child(&mut store, "root", "foreground", ScopeKind::Task);
    child(&mut store, "foreground", "nested", ScopeKind::Task);
    child(&mut store, "root", "background", ScopeKind::Service);
    assert!(matches!(
        store.admit_owned(None, ScopeKind::Service, &command("orphan-service")),
        Err(Error::Invalid)
    ));
    assert!(matches!(store.settle_owned("root"), Err(Error::Conflict)));
    let epoch = store.cancel_scope("foreground", "stop-foreground").unwrap();
    assert!(matches!(
        store.admit_owned(
            Some("foreground"),
            ScopeKind::Task,
            &command("racing-child")
        ),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        store.admit(&Command {
            id: "later".into(),
            ..command("nested")
        }),
        Err(Error::Cancelled)
    ));
    let nodes = store.owned_scopes().unwrap();
    assert_eq!(
        nodes
            .iter()
            .find(|n| n.scope == "background")
            .unwrap()
            .state,
        ScopeState::Active
    );
    assert_eq!(
        nodes.iter().find(|n| n.scope == "root").unwrap().state,
        ScopeState::Active
    );
    assert_eq!(
        nodes.iter().find(|n| n.scope == "nested").unwrap().state,
        ScopeState::Cancelling
    );
    assert!(matches!(
        store.settle_owned("foreground"),
        Err(Error::Conflict)
    ));
    store.settle_owned("nested").unwrap();
    store.settle_owned("foreground").unwrap();
    store.settle_owned("foreground").unwrap();
    assert!(matches!(store.settle_owned("root"), Err(Error::Conflict)));
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(
        store.cancel_scope("foreground", "stop-foreground").unwrap(),
        epoch
    );
    assert!(
        store
            .owned_scopes()
            .unwrap()
            .iter()
            .any(|n| n.scope == "background" && n.state == ScopeState::Active)
    );
    store.cancel_scope("root", "stop-all").unwrap();
    store.settle_owned("background").unwrap();
    store.settle_owned("root").unwrap();
    assert!(
        store
            .owned_scopes()
            .unwrap()
            .iter()
            .all(|n| n.state == ScopeState::Settled)
    );
    assert!(matches!(
        store.admit_owned(Some("root"), ScopeKind::Task, &command("after-settlement")),
        Err(Error::Cancelled)
    ));
    drop(store);
    assert!(Store::open(&realm.0).is_ok());
}

#[test]
fn joins_refuse_cross_tree_self_cycles_and_implicit_parent_child_deadlocks() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    root(&mut store);
    child(&mut store, "root", "a", ScopeKind::Task);
    child(&mut store, "root", "b", ScopeKind::Task);
    child(&mut store, "a", "grandchild", ScopeKind::Task);
    store
        .admit_owned(None, ScopeKind::Task, &command("other-tree"))
        .unwrap();
    assert!(matches!(store.join_owned("a", "a"), Err(Error::Invalid)));
    assert!(matches!(
        store.join_owned("a", "other-tree"),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        store.join_owned("grandchild", "root"),
        Err(Error::Conflict)
    ));
    store.join_owned("a", "b").unwrap();
    store.join_owned("a", "b").unwrap();
    assert!(matches!(store.join_owned("b", "a"), Err(Error::Conflict)));
    store.join_owned("b", "grandchild").unwrap();
    assert!(matches!(
        store.join_owned("grandchild", "b"),
        Err(Error::Conflict)
    ));
    store.cancel_scope("root", "stop-tree").unwrap();
    store.settle_owned("grandchild").unwrap();
    assert!(matches!(store.settle_owned("a"), Err(Error::Conflict)));
    store.settle_owned("b").unwrap();
    store.settle_owned("a").unwrap();
    store.settle_owned("root").unwrap();
    drop(store);
    assert!(Store::open(&realm.0).is_ok());
}

#[test]
fn ownership_depth_wait_fanout_and_attachment_identity_are_bounded() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    root(&mut store);
    let mut parent = "root".to_string();
    for i in 1..MAX_DEPTH {
        let next = format!("depth-{i}");
        child(&mut store, &parent, &next, ScopeKind::Task);
        parent = next;
    }
    assert!(matches!(
        store.admit_owned(Some(&parent), ScopeKind::Task, &command("too-deep")),
        Err(Error::Exhausted)
    ));
    for i in 0..=MAX_WAITS {
        let scope = format!("wait-{i}");
        child(&mut store, "root", &scope, ScopeKind::Task);
        if i < MAX_WAITS {
            store.join_owned(&parent, &scope).unwrap();
        } else {
            assert!(matches!(
                store.join_owned(&parent, &scope),
                Err(Error::Exhausted)
            ));
        }
    }
    assert!(matches!(
        store.admit_owned(Some("missing"), ScopeKind::Task, &command("unattached")),
        Err(Error::Invalid)
    ));
    assert!(matches!(
        store.admit_owned(Some("self"), ScopeKind::Task, &command("self")),
        Err(Error::Invalid)
    ));
    store.admit(&command("legacy")).unwrap();
    assert!(matches!(
        store.admit_owned(None, ScopeKind::Task, &command("legacy")),
        Err(Error::Conflict)
    ));
    drop(store);
    assert!(Store::open(&realm.0).is_ok());
}

#[test]
fn attachment_and_cancel_sql_faults_roll_back_the_entire_subtree() {
    for table in ["admissions", "owned_scopes"] {
        for fault in ["ABORT,'injected'", "IGNORE"] {
            let realm = Realm::new();
            let mut store = Store::open(&realm.0).unwrap();
            root(&mut store);
            let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
            db.execute_batch(&format!("CREATE TRIGGER fail_owned BEFORE INSERT ON {table} BEGIN SELECT RAISE({fault}); END;")).unwrap();
            assert!(matches!(
                store.admit_owned(Some("root"), ScopeKind::Task, &command("child")),
                Err(Error::Sql(_) | Error::Incompatible)
            ));
            assert!(matches!(store.owned_scopes(), Err(Error::Paused)));
            assert_eq!(
                db.query_row("SELECT count(*) FROM admissions", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM owned_scopes", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
            db.execute_batch("DROP TRIGGER fail_owned").unwrap();
            drop(store);
            let mut store = Store::open(&realm.0).unwrap();
            child(&mut store, "root", "child", ScopeKind::Task);
        }
    }
    for table in ["cancellations", "owned_scopes"] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        root(&mut store);
        child(&mut store, "root", "child", ScopeKind::Task);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        let operation = if table == "cancellations" {
            "INSERT"
        } else {
            "UPDATE"
        };
        db.execute_batch(&format!("CREATE TRIGGER fail_cancel_owned BEFORE {operation} ON {table} WHEN NEW.scope='root' BEGIN SELECT RAISE(ABORT,'injected'); END;")).unwrap();
        assert!(matches!(
            store.cancel_scope("root", "cancel"),
            Err(Error::Sql(_))
        ));
        assert_eq!(
            db.query_row("SELECT count(*) FROM cancellations", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM owned_scopes WHERE state!=0",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
}

#[test]
fn cancelled_opaque_attempts_block_settlement_even_with_a_late_receipt() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let admission = store
        .admit_owned(None, ScopeKind::Task, &command("root"))
        .unwrap();
    let proposal = Proposal {
        id: "effect-proposal".into(),
        scope: "root".into(),
        observation: Observation::Admission(admission.sequence),
        expected_revision: 0,
        owner_epoch: store.epoch(),
        engine: "fixture".into(),
        continuation: b"waiting".to_vec(),
        workspace: None,
        effects: vec![EffectIntent {
            id: "opaque-effect".into(),
            adapter: "opaque".into(),
            input: vec![],
        }],
        state: None,
    };
    store.commit_proposal(&proposal).unwrap();
    let attempt = Attempt {
        id: "opaque-attempt".into(),
        effect: "opaque-effect".into(),
        owner_epoch: store.epoch(),
        implementation: "fixture".into(),
        grant_reference: "fixture".into(),
    };
    store.reserve_attempt(&attempt).unwrap();
    store.cancel_scope("root", "cancel").unwrap();
    assert!(matches!(store.settle_owned("root"), Err(Error::Conflict)));
    store.ingest_receipt(&attempt.id, b"late output").unwrap();
    assert!(matches!(store.settle_owned("root"), Err(Error::Conflict)));
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert!(matches!(store.settle_owned("root"), Err(Error::Conflict)));
    assert_eq!(
        store.receipt(&attempt.id).unwrap(),
        Some(b"late output".to_vec())
    );
}

#[test]
fn owned_scopes_keep_independent_revision_chains_and_reject_cross_scope_bases() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    root(&mut store);
    child(&mut store, "root", "child", ScopeKind::Task);
    let mut proposals = vec![];
    for scope in ["root", "child", "root", "child"] {
        let before = store.snapshot_scope(scope).unwrap();
        let admission = store
            .admit(&Command {
                scope: scope.into(),
                id: format!("progress-{}", proposals.len()),
                lineage: 0,
                kind: "fixture".into(),
                payload: vec![],
            })
            .unwrap();
        let proposal = Proposal {
            id: format!("progress-{}", proposals.len()),
            scope: scope.into(),
            observation: Observation::Admission(admission.sequence),
            expected_revision: before.revision,
            owner_epoch: store.epoch(),
            engine: "fixture".into(),
            continuation: scope.as_bytes().to_vec(),
            workspace: None,
            effects: vec![],
            state: None,
        };
        let other = if scope == "root" { "child" } else { "root" };
        let other_before = store.snapshot_scope(other).unwrap();
        if other_before.revision != before.revision {
            let mut wrong = proposal.clone();
            wrong.expected_revision = other_before.revision;
            assert!(matches!(store.commit_proposal(&wrong), Err(Error::Stale)));
        }
        let revision = store.commit_proposal(&proposal).unwrap();
        assert_eq!(revision, proposals.len() as u64 + 1);
        assert_eq!(store.snapshot_scope(other).unwrap(), other_before);
        assert_eq!(
            store.snapshot_scope(scope).unwrap().continuation,
            scope.as_bytes()
        );
        proposals.push(proposal);
    }
    let parent = store.snapshot_scope("root").unwrap();
    let child = store.snapshot_scope("child").unwrap();
    assert_eq!((parent.revision, child.revision), (3, 4));
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.snapshot_scope("root").unwrap(), parent);
    assert_eq!(store.snapshot_scope("child").unwrap(), child);
    for (position, proposal) in proposals.iter().enumerate() {
        assert_eq!(
            store.commit_proposal(proposal).unwrap(),
            position as u64 + 1
        );
    }
}

#[test]
fn forged_ownership_cycles_and_settlement_fail_before_epoch_change() {
    for sql in [
        "UPDATE owned_scopes SET parent='child' WHERE scope='root'",
        "UPDATE owned_scopes SET parent='missing' WHERE scope='child'",
        "UPDATE owned_scopes SET admission=999 WHERE scope='child'",
        "UPDATE owned_scopes SET kind=1 WHERE scope='root'",
        "UPDATE owned_scopes SET state=2 WHERE scope='root'",
        "UPDATE owned_scopes SET state=1 WHERE scope='child'",
        "INSERT INTO owned_waits VALUES('child','root')",
    ] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        root(&mut store);
        child(&mut store, "root", "child", ScopeKind::Task);
        drop(store);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        db.execute_batch(&format!("PRAGMA foreign_keys=OFF; {sql}"))
            .unwrap();
        assert!(Store::open(&realm.0).is_err(), "accepted {sql}");
        assert_eq!(
            db.query_row("SELECT epoch FROM realm", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}

#[test]
fn suspension_cascades_blocks_new_work_keeps_receipts_and_resumes_top_down() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    root(&mut store);
    child(&mut store, "root", "worker", ScopeKind::Task);
    child(&mut store, "worker", "leaf", ScopeKind::Task);
    child(&mut store, "root", "service", ScopeKind::Service);
    let admission = store
        .admit(&Command {
            id: "work".into(),
            ..command("leaf")
        })
        .unwrap();
    store
        .commit_proposal(&Proposal {
            id: "leaf-effect".into(),
            scope: "leaf".into(),
            observation: Observation::Admission(admission.sequence),
            expected_revision: 0,
            owner_epoch: store.epoch(),
            engine: "fixture".into(),
            continuation: b"waiting".to_vec(),
            workspace: None,
            effects: vec![EffectIntent {
                id: "leaf-op".into(),
                adapter: "opaque".into(),
                input: vec![],
            }],
            state: None,
        })
        .unwrap();
    let in_flight = Attempt {
        id: "leaf-attempt".into(),
        effect: "leaf-op".into(),
        owner_epoch: store.epoch(),
        implementation: "fixture".into(),
        grant_reference: "fixture".into(),
    };
    store.reserve_attempt(&in_flight).unwrap();

    store.suspend_owned("worker").unwrap();
    store.suspend_owned("worker").unwrap();
    let state = |store: &mut Store, scope: &str| {
        store
            .owned_scopes()
            .unwrap()
            .into_iter()
            .find(|node| node.scope == scope)
            .unwrap()
            .state
    };
    assert_eq!(state(&mut store, "worker"), ScopeState::Suspended);
    assert_eq!(state(&mut store, "leaf"), ScopeState::Suspended);
    assert_eq!(state(&mut store, "root"), ScopeState::Active);
    assert_eq!(state(&mut store, "service"), ScopeState::Active);

    assert!(matches!(
        store.admit(&Command {
            id: "more".into(),
            ..command("leaf")
        }),
        Err(Error::Suspended)
    ));
    assert!(matches!(
        store.admit_owned(Some("worker"), ScopeKind::Task, &command("new-child")),
        Err(Error::Suspended)
    ));
    assert!(matches!(
        store.join_owned("leaf", "service"),
        Err(Error::Suspended)
    ));
    assert!(matches!(store.settle_owned("leaf"), Err(Error::Conflict)));
    assert!(
        matches!(store.resume_owned("leaf"), Err(Error::Conflict)),
        "owner still suspended"
    );

    store
        .ingest_receipt(&in_flight.id, b"finished while suspended")
        .unwrap();
    assert!(
        store
            .reserve_attempt(&Attempt {
                id: "leaf-attempt-2".into(),
                ..in_flight.clone()
            })
            .is_err()
    );

    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(state(&mut store, "leaf"), ScopeState::Suspended);
    assert_eq!(
        store.receipt(&in_flight.id).unwrap(),
        Some(b"finished while suspended".to_vec())
    );
    store.resume_owned("worker").unwrap();
    assert_eq!(state(&mut store, "worker"), ScopeState::Active);
    assert_eq!(state(&mut store, "leaf"), ScopeState::Active);
    store
        .admit(&Command {
            id: "more".into(),
            ..command("leaf")
        })
        .unwrap();

    store.suspend_owned("root").unwrap();
    assert_eq!(state(&mut store, "service"), ScopeState::Suspended);
    store.cancel_scope("root", "stop").unwrap();
    for scope in ["root", "worker", "leaf", "service"] {
        assert_eq!(state(&mut store, scope), ScopeState::Cancelling, "{scope}");
    }
    assert!(matches!(store.resume_owned("root"), Err(Error::Conflict)));
    assert!(matches!(store.suspend_owned("root"), Err(Error::Conflict)));
    drop(store);
    Store::open(&realm.0).unwrap();
}

#[test]
fn forged_suspension_states_are_refused_on_reopen() {
    for sql in [
        "UPDATE owned_scopes SET state=3 WHERE scope='root'",
        "UPDATE owned_scopes SET state=4 WHERE scope='child'",
    ] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        root(&mut store);
        child(&mut store, "root", "child", ScopeKind::Task);
        drop(store);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        db.execute_batch(&format!("PRAGMA ignore_check_constraints=ON; {sql}"))
            .unwrap();
        drop(db);
        assert!(Store::open(&realm.0).is_err(), "accepted {sql}");
    }
}

#[test]
fn owner_stops_its_services_then_settles_and_takeover_reports_obligations() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    root(&mut store);
    child(&mut store, "root", "worker", ScopeKind::Task);
    child(&mut store, "root", "watcher", ScopeKind::Service);
    child(&mut store, "root", "indexer", ScopeKind::Service);
    child(&mut store, "watcher", "watcher-helper", ScopeKind::Task);
    child(&mut store, "worker", "worker-service", ScopeKind::Service);
    store.suspend_owned("indexer").unwrap();
    let queued = store
        .admit(&Command {
            id: "queued".into(),
            ..command("worker")
        })
        .unwrap();

    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    let report = store.reconciliation().unwrap();
    assert_eq!(
        report.live_services,
        ["watcher", "indexer", "worker-service"]
    );
    assert!(report.cancelling.is_empty());
    assert!(
        report
            .queued
            .contains(&("worker".to_string(), queued.sequence))
    );
    assert!(!report.truncated);

    assert!(matches!(
        store.stop_services("missing", "stop"),
        Err(Error::Invalid)
    ));
    let stopped = store.stop_services("root", "stop").unwrap();
    assert_eq!(stopped, ["watcher", "indexer"], "direct services only");
    assert!(
        store.stop_services("root", "stop").unwrap().is_empty(),
        "repeat is a no-op"
    );
    let state = |store: &mut Store, scope: &str| {
        store
            .owned_scopes()
            .unwrap()
            .into_iter()
            .find(|node| node.scope == scope)
            .unwrap()
            .state
    };
    assert_eq!(state(&mut store, "watcher-helper"), ScopeState::Cancelling);
    assert_eq!(state(&mut store, "worker"), ScopeState::Active);
    assert_eq!(state(&mut store, "worker-service"), ScopeState::Active);
    let report = store.reconciliation().unwrap();
    assert_eq!(report.live_services, ["worker-service"]);
    assert_eq!(report.cancelling, ["watcher", "indexer", "watcher-helper"]);

    for scope in ["watcher-helper", "watcher", "indexer"] {
        store.settle_owned(scope).unwrap();
    }
    assert!(
        matches!(store.settle_owned("root"), Err(Error::Conflict)),
        "worker still active"
    );
    store.cancel_scope("worker", "done").unwrap();
    store.settle_owned("worker-service").unwrap();
    store.settle_owned("worker").unwrap();
    assert!(
        matches!(store.settle_owned("root"), Err(Error::Conflict)),
        "unprocessed root admission"
    );
    store.cancel_scope("root", "finish").unwrap();
    store.settle_owned("root").unwrap();
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    let report = store.reconciliation().unwrap();
    assert!(report.live_services.is_empty() && report.cancelling.is_empty());
}

#[test]
fn takeover_requires_service_reconfirmation_and_queued_work_can_be_declined() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    root(&mut store);
    child(&mut store, "root", "watcher", ScopeKind::Service);
    child(&mut store, "watcher", "helper", ScopeKind::Task);
    let work = |scope: &str, id: &str| Command {
        id: id.into(),
        ..command(scope)
    };
    let queued = store.admit(&work("root", "queued")).unwrap();
    let declined = store
        .admit_owned(None, ScopeKind::Task, &command("declined"))
        .unwrap();
    assert!(
        store
            .reconciliation()
            .unwrap()
            .unconfirmed_services
            .is_empty()
    );
    store.admit(&work("helper", "before-takeover")).unwrap();

    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    let report = store.reconciliation().unwrap();
    assert_eq!(report.live_services, ["watcher"]);
    assert_eq!(report.unconfirmed_services, ["watcher"]);
    assert!(
        report
            .queued
            .contains(&("root".to_string(), queued.sequence))
    );
    for scope in ["watcher", "helper"] {
        assert!(
            matches!(
                store.admit(&work(scope, "after-takeover")),
                Err(Error::Unconfirmed)
            ),
            "{scope} accepted work under an unconfirmed service"
        );
    }
    assert!(matches!(
        store.admit_owned(Some("watcher"), ScopeKind::Task, &command("late-child")),
        Err(Error::Unconfirmed)
    ));
    let unaffected = store.admit(&work("root", "unaffected")).unwrap();
    for scope in ["helper", "missing"] {
        assert!(
            matches!(store.confirm_service(scope), Err(Error::Invalid)),
            "{scope}"
        );
    }
    store.confirm_service("watcher").unwrap();
    store.confirm_service("watcher").unwrap();
    assert!(
        store
            .reconciliation()
            .unwrap()
            .unconfirmed_services
            .is_empty()
    );
    store.admit(&work("helper", "after-confirm")).unwrap();

    assert!(matches!(
        store.settle_owned("declined"),
        Err(Error::Conflict)
    ));
    store.drop_admission(declined.sequence, "decline").unwrap();
    store.drop_admission(declined.sequence, "decline").unwrap();
    assert!(matches!(
        store.drop_admission(declined.sequence, "other"),
        Err(Error::Conflict)
    ));
    store.settle_owned("declined").unwrap();
    store
        .drop_admission(queued.sequence, "decline-queued")
        .unwrap();
    assert!(
        !store
            .reconciliation()
            .unwrap()
            .queued
            .iter()
            .any(|(_, sequence)| [queued.sequence, declined.sequence].contains(sequence))
    );
    let consume = |admission: u64, id: &str, epoch: u64| Proposal {
        id: id.into(),
        scope: "root".into(),
        observation: Observation::Admission(admission),
        expected_revision: 0,
        owner_epoch: epoch,
        engine: "fixture".into(),
        continuation: b"progress".to_vec(),
        workspace: None,
        effects: vec![],
        state: None,
    };
    let epoch = store.epoch();
    assert!(matches!(
        store.commit_proposal(&consume(queued.sequence, "consume-dropped", epoch)),
        Err(Error::Conflict)
    ));
    store
        .commit_proposal(&consume(unaffected.sequence, "consume", epoch))
        .unwrap();
    assert!(matches!(
        store.drop_admission(unaffected.sequence, "too-late"),
        Err(Error::Conflict)
    ));
    for sequence in [0, 9_999] {
        assert!(matches!(
            store.drop_admission(sequence, "missing"),
            Err(Error::Invalid)
        ));
    }
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(
        store.reconciliation().unwrap().unconfirmed_services,
        ["watcher"],
        "each takeover needs a fresh confirmation"
    );
}

#[test]
fn forged_service_confirmations_or_dropped_admissions_fail_reopen() {
    for sql in [
        "INSERT INTO service_confirmations VALUES('root',1)",
        "DELETE FROM service_confirmations",
        "UPDATE service_confirmations SET epoch=999",
        "INSERT INTO dropped_admissions SELECT sequence,'forged',admitted_epoch FROM admissions WHERE command_id='consumed'",
        "INSERT INTO dropped_admissions VALUES(999,'forged',1)",
    ] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        root(&mut store);
        child(&mut store, "root", "watcher", ScopeKind::Service);
        let admission = store
            .admit(&Command {
                id: "consumed".into(),
                ..command("root")
            })
            .unwrap();
        store
            .commit_proposal(&Proposal {
                id: "consume".into(),
                scope: "root".into(),
                observation: Observation::Admission(admission.sequence),
                expected_revision: 0,
                owner_epoch: store.epoch(),
                engine: "fixture".into(),
                continuation: b"progress".to_vec(),
                workspace: None,
                effects: vec![],
                state: None,
            })
            .unwrap();
        drop(store);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        // A forger disables enforcement; reopen must still catch the damage.
        db.execute_batch(&format!("PRAGMA foreign_keys=OFF; {sql}"))
            .unwrap();
        drop(db);
        assert!(Store::open(&realm.0).is_err(), "accepted {sql}");
    }
}

#[test]
fn service_stop_crash_midway_is_completed_by_repeating_the_call() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    root(&mut store);
    child(&mut store, "root", "first", ScopeKind::Service);
    child(&mut store, "root", "second", ScopeKind::Service);
    store.cancel_scope("first", "stop/svc").unwrap();
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.stop_services("root", "stop").unwrap(), ["second"]);
    let all = store.owned_scopes().unwrap();
    assert!(
        all.iter()
            .filter(|node| node.kind == ScopeKind::Service)
            .all(|node| node.state == ScopeState::Cancelling)
    );
}
