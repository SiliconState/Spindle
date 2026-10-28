use spindle_store::artifact::Stage;
use spindle_store::transition::{Observation, Proposal};
use spindle_store::workspace::Entry;
use spindle_store::{Command, Error, Store};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SERIAL: AtomicU64 = AtomicU64::new(0);
struct Realm(PathBuf);
impl Realm {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "spindle-workspace-{}-{}",
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
fn admission(store: &mut Store, id: &str) -> u64 {
    store
        .admit(&Command {
            scope: "operator".into(),
            id: id.into(),
            lineage: 0,
            kind: "edit".into(),
            payload: id.as_bytes().to_vec(),
        })
        .unwrap()
        .sequence
}
fn stage(store: &mut Store) -> Stage {
    let stage = Stage {
        id: "stage".into(),
        scope: "operator".into(),
        owner_epoch: store.epoch(),
    };
    store.begin_stage(&stage).unwrap();
    stage
}
fn proposal(
    store: &mut Store,
    admission: u64,
    root: Option<String>,
    continuation: &[u8],
) -> Proposal {
    Proposal {
        id: format!("proposal-{admission}"),
        scope: "operator".into(),
        observation: Observation::Admission(admission),
        expected_revision: store.snapshot().unwrap().revision,
        owner_epoch: store.epoch(),
        engine: "test-edit-v1".into(),
        continuation: continuation.to_vec(),
        workspace: root,
        effects: vec![],
        state: None,
    }
}
fn file(path: &str, object: &str) -> Entry {
    Entry {
        path: path.into(),
        object: object.into(),
        executable: false,
    }
}

#[test]
fn file_roots_and_continuation_publish_jointly_and_old_reads_stay_pinned() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let first = admission(&mut store, "one");
    let stage = stage(&mut store);
    let old_object = store.put_object(&stage, b"old").unwrap();
    let old_root = store
        .stage_file_map(&stage, &[file("src/file.txt", &old_object)])
        .unwrap();
    let first_proposal = proposal(
        &mut store,
        first,
        Some(old_root.clone()),
        b"old conversation",
    );
    store.commit_proposal(&first_proposal).unwrap();
    let pinned = store.snapshot().unwrap();
    let second = admission(&mut store, "two");
    let new_object = store.put_object(&stage, b"new").unwrap();
    let new_root = store
        .stage_file_map(&stage, &[file("src/file.txt", &new_object)])
        .unwrap();
    assert_eq!(
        store.snapshot().unwrap(),
        pinned,
        "staging must not publish"
    );
    let second_proposal = proposal(
        &mut store,
        second,
        Some(new_root.clone()),
        b"new conversation",
    );
    store.commit_proposal(&second_proposal).unwrap();
    assert_eq!(
        store
            .read_versioned_file("operator", &old_root, "src/file.txt")
            .unwrap(),
        b"old"
    );
    assert_eq!(
        store
            .read_versioned_file("operator", &new_root, "src/file.txt")
            .unwrap(),
        b"new"
    );
    drop(store);
    let mut recovered = Store::open(&realm.0).unwrap();
    let current = recovered.snapshot().unwrap();
    assert_eq!(current.workspace.as_deref(), Some(new_root.as_str()));
    assert_eq!(current.continuation, b"new conversation");
    assert_eq!(
        recovered.snapshot_at("operator", pinned.revision).unwrap(),
        pinned
    );
    assert!(matches!(
        recovered.snapshot_at("other-operator", pinned.revision),
        Err(Error::Invalid)
    ));
    assert!(matches!(
        recovered.snapshot_at("operator", 999),
        Err(Error::Invalid)
    ));
    assert!(matches!(
        recovered.snapshot_at("operator", u64::MAX),
        Err(Error::Invalid)
    ));
    assert_eq!(
        recovered
            .read_versioned_file(
                "operator",
                pinned.workspace.as_ref().unwrap(),
                "src/file.txt"
            )
            .unwrap(),
        b"old"
    );
    assert_eq!(recovered.commit_proposal(&second_proposal).unwrap(), 2);
    let third = admission(&mut recovered, "three");
    let preserved = proposal(&mut recovered, third, None, b"same workspace");
    recovered.commit_proposal(&preserved).unwrap();
    assert_eq!(recovered.snapshot().unwrap().workspace, Some(new_root));
}

#[test]
fn paths_take_linux_names_but_reject_traversal_duplicates_and_prefix_conflicts() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    admission(&mut store, "one");
    let stage = stage(&mut store);
    let object = store.put_object(&stage, b"file").unwrap();
    let long_name = "x".repeat(256);
    // 1,025 bytes of valid components: only the total length is wrong.
    let long_path = format!("{}x", "d/".repeat(512));
    for path in [
        "",
        "/absolute",
        "../escape",
        "a/../b",
        "a/./b",
        "a//b",
        "a/",
        "a\0b",
        long_name.as_str(),
        long_path.as_str(),
    ] {
        let path = path.replace("\\0", "\0");
        assert!(
            matches!(
                store.stage_file_map(&stage, &[file(&path, &object)]),
                Err(Error::Invalid)
            ),
            "{path:?}"
        );
    }
    for path in [
        ".git/config",
        "A/.GIT/HEAD",
        "CON.txt",
        "a.",
        "a b",
        "a/é",
        "a\\b",
        "node_modules/@scope/pkg+1/index.js",
    ] {
        store
            .stage_file_map(&stage, &[file(path, &object)])
            .unwrap_or_else(|error| panic!("{path}: {error:?}"));
    }
    for paths in [["a", "a/b"], ["a/b", "a"], ["a", "a"]] {
        let entries: Vec<_> = paths.iter().map(|p| file(p, &object)).collect();
        assert!(
            matches!(store.stage_file_map(&stage, &entries), Err(Error::Invalid)),
            "{paths:?}"
        );
    }
    for paths in [["A", "a"], ["Dir/a", "dir/b"], ["a/B/c", "a/b/d"]] {
        let entries: Vec<_> = paths.iter().map(|p| file(p, &object)).collect();
        store.stage_file_map(&stage, &entries).unwrap();
    }
    let entries = [file("b.txt", &object), file("a.txt", &object)];
    let root = store.stage_file_map(&stage, &entries).unwrap();
    assert_eq!(
        root,
        store
            .stage_file_map(&stage, &[entries[1].clone(), entries[0].clone()])
            .unwrap()
    );
    let mut executable = entries[0].clone();
    executable.executable = true;
    assert_ne!(
        root,
        store
            .stage_file_map(&stage, &[entries[1].clone(), executable])
            .unwrap()
    );
}

#[test]
fn refused_root_does_not_consume_input_or_publish_partial_continuation() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let input = admission(&mut store, "one");
    let stage = stage(&mut store);
    let missing = "0".repeat(64);
    let mut invalid = proposal(&mut store, input, Some(missing), b"new");
    assert!(matches!(
        store.commit_proposal(&invalid),
        Err(Error::Invalid)
    ));
    assert_eq!(store.snapshot().unwrap().revision, 0);
    let root = store.stage_file_map(&stage, &[]).unwrap();
    invalid.workspace = Some(root);
    assert_eq!(store.commit_proposal(&invalid).unwrap(), 1);
}

#[test]
fn damaged_file_object_blocks_publication_before_head_changes() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let input = admission(&mut store, "one");
    let stage = stage(&mut store);
    let object = store.put_object(&stage, b"original").unwrap();
    let root = store
        .stage_file_map(&stage, &[file("file.txt", &object)])
        .unwrap();
    let proposed = proposal(&mut store, input, Some(root), b"must-not-publish");
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute("UPDATE objects SET bytes=X'00' WHERE digest=?1", [&object])
        .unwrap();
    assert!(matches!(
        store.commit_proposal(&proposed),
        Err(Error::Incompatible)
    ));
    let head: i64 = db
        .query_row("SELECT revision FROM head", [], |r| r.get(0))
        .unwrap();
    assert_eq!(head, 0);
    assert!(matches!(store.snapshot(), Err(Error::Paused)));
}

#[test]
fn publication_fault_leaves_pinned_objects_but_never_mixed_roots() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let input = admission(&mut store, "one");
    let stage = stage(&mut store);
    let object = store.put_object(&stage, b"file").unwrap();
    let root = store
        .stage_file_map(&stage, &[file("file.txt", &object)])
        .unwrap();
    let proposed = proposal(&mut store, input, Some(root.clone()), b"new");
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
    let mut reopened = Store::open(&realm.0).unwrap();
    let head = reopened.snapshot().unwrap();
    assert_eq!(head.revision, 0);
    assert_eq!(head.workspace, None);
    assert!(head.continuation.is_empty());
    assert_eq!(
        reopened
            .read_versioned_file("operator", &root, "file.txt")
            .unwrap(),
        b"file"
    );
}

#[test]
fn completion_binds_exact_candidate_and_rejects_real_workspace_write_races() {
    use spindle_store::completion::Attempt;
    use spindle_store::transition::EffectIntent;
    for intervening_write in [false, true] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let input = admission(&mut store, "one");
        let stage = stage(&mut store);
        let base_object = store.put_object(&stage, b"base").unwrap();
        let base = store
            .stage_file_map(&stage, &[file("file.txt", &base_object)])
            .unwrap();
        let mut start = proposal(&mut store, input, Some(base.clone()), b"waiting");
        start.effects.push(EffectIntent {
            id: "edit".into(),
            adapter: "test-edit".into(),
            input: vec![],
        });
        store.commit_proposal(&start).unwrap();
        let attempt = Attempt {
            id: "edit-attempt".into(),
            effect: "edit".into(),
            owner_epoch: store.epoch(),
            implementation: "test-v1".into(),
            grant_reference: "grant".into(),
        };
        store.reserve_attempt(&attempt).unwrap();
        let candidate_object = store.put_object(&stage, b"candidate").unwrap();
        let candidate = store
            .stage_file_map(&stage, &[file("file.txt", &candidate_object)])
            .unwrap();
        store
            .ingest_workspace_receipt(&attempt.id, b"edited", Some(&candidate))
            .unwrap();
        assert!(matches!(
            store.ingest_workspace_receipt(&attempt.id, b"edited", Some(&base)),
            Err(Error::Conflict)
        ));
        assert!(matches!(
            store.ingest_receipt(&attempt.id, b"edited"),
            Err(Error::Conflict)
        ));
        let extra = admission(&mut store, "two");
        let concurrent_root = if intervening_write {
            Some(store.stage_file_map(&stage, &[]).unwrap())
        } else {
            None
        };
        let concurrent = proposal(&mut store, extra, concurrent_root, b"progress");
        store.commit_proposal(&concurrent).unwrap();
        let before = store.snapshot().unwrap();
        let mut finish = Proposal {
            id: "finish-edit".into(),
            scope: "operator".into(),
            observation: Observation::Receipt(attempt.id.clone()),
            expected_revision: before.revision,
            owner_epoch: store.epoch(),
            engine: "test-edit-v1".into(),
            continuation: b"edited".to_vec(),
            workspace: Some(base),
            effects: vec![],
            state: None,
        };
        assert!(matches!(
            store.commit_proposal(&finish),
            Err(Error::Conflict)
        ));
        finish.workspace = Some(candidate.clone());
        if intervening_write {
            assert!(matches!(store.commit_proposal(&finish), Err(Error::Stale)));
            assert_eq!(store.snapshot().unwrap(), before);
        } else {
            store.commit_proposal(&finish).unwrap();
            assert_eq!(store.snapshot().unwrap().workspace, Some(candidate));
            assert_eq!(store.snapshot().unwrap().continuation, b"edited");
        }
        drop(store);
        let mut reopened = Store::open(&realm.0).unwrap();
        assert_eq!(
            reopened.receipt(&attempt.id).unwrap(),
            Some(b"edited".to_vec())
        );
        if intervening_write {
            assert_eq!(reopened.snapshot().unwrap(), before);
        } else {
            assert_eq!(reopened.commit_proposal(&finish).unwrap(), 3);
        }
    }
}

#[test]
fn retained_root_closure_and_digest_are_verified_on_reopen() {
    for damage in [
        "UPDATE objects SET bytes=X'00' WHERE length(bytes)=4",
        "DELETE FROM staging_pins",
        "UPDATE revisions SET workspace=NULL WHERE revision=1",
    ] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let input = admission(&mut store, "one");
        let stage = stage(&mut store);
        let object = store.put_object(&stage, b"file").unwrap();
        let root = store
            .stage_file_map(&stage, &[file("file.txt", &object)])
            .unwrap();
        let proposed = proposal(&mut store, input, Some(root), b"new");
        store.commit_proposal(&proposed).unwrap();
        drop(store);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        db.execute(damage, []).unwrap();
        drop(db);
        assert!(Store::open(&realm.0).is_err(), "{damage}");
    }
}

/// Trees that profile v1 can hold keep its encoding (and so their root digests); links,
/// empty directories and Linux names use v2. Both reopen, and links are only text.
#[test]
fn manifests_keep_v1_where_possible_and_carry_links_and_empty_directories() {
    use spindle_store::workspace::{Link, Tree};
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let input = admission(&mut store, "one");
    let stage = stage(&mut store);
    let object = store.put_object(&stage, b"file").unwrap();
    let legacy = store
        .stage_file_map(&stage, &[file("src/main.rs", &object)])
        .unwrap();
    let bytes = store.read_object("operator", &legacy).unwrap();
    assert!(bytes.starts_with(b"spindle-file-map-v1\0"));
    let tree = Tree {
        files: vec![file("repo/.git/HEAD", &object)],
        links: vec![Link {
            path: "escaped".into(),
            target: "/etc/passwd".into(),
        }],
        directories: vec!["repo/.git/refs/tags".into()],
    };
    let root = store.stage_tree(&stage, &tree).unwrap();
    assert!(
        store
            .read_object("operator", &root)
            .unwrap()
            .starts_with(b"spindle-file-map-v2\0")
    );
    assert_eq!(store.tree_map("operator", &root).unwrap(), tree);
    assert_eq!(
        store.file_map("operator", &root).unwrap(),
        tree.files,
        "file maps list regular files only"
    );
    for bad in [
        Tree {
            links: vec![Link {
                path: "repo".into(),
                target: "x".into(),
            }],
            ..tree.clone()
        },
        Tree {
            directories: vec!["repo".into()],
            ..tree.clone()
        },
        Tree {
            links: vec![Link {
                path: "empty-target".into(),
                target: String::new(),
            }],
            ..Tree::default()
        },
    ] {
        assert!(matches!(
            store.stage_tree(&stage, &bad),
            Err(Error::Invalid)
        ));
    }
    let published = proposal(&mut store, input, Some(root.clone()), b"tree");
    store.commit_proposal(&published).unwrap();
    drop(store);
    let mut reopened = Store::open(&realm.0).unwrap();
    assert_eq!(reopened.tree_map("operator", &root).unwrap(), tree);
}

/// Garbage collection keeps everything a revision reaches and frees what nothing does.
#[test]
fn garbage_collection_keeps_reachable_objects() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let input = admission(&mut store, "one");
    let stage = stage(&mut store);
    let object = store.put_object(&stage, b"kept").unwrap();
    let root = store
        .stage_file_map(&stage, &[file("kept.txt", &object)])
        .unwrap();
    let kept = proposal(&mut store, input, Some(root.clone()), b"kept");
    store.commit_proposal(&kept).unwrap();
    let collected = store.collect_garbage().unwrap();
    assert_eq!(collected.objects, 0, "host stages are never collected");
    assert_eq!(
        store
            .read_versioned_file("operator", &root, "kept.txt")
            .unwrap(),
        b"kept"
    );
    drop(store);
    Store::open(&realm.0).unwrap();
}
