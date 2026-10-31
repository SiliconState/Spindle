//! Store images and session bundles: an image carries a store's
//! whole history, identity and unknown outcomes, from WAL and rollback stores alike;
//! a bundle refuses every damaged, cut, foreign or escaping input before anything is
//! written.
#![cfg(feature = "driver")]
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use spindle_store::bundle::{self, BundleFile, STORE};
use spindle_store::completion::Attempt;
use spindle_store::transition::{EffectIntent, Observation, Proposal};
use spindle_store::{
    Command, ConnectionProfile, Error, Initialization, JournalMode, OwnerExclusion, Store,
    check_image,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

struct Owned;
impl OwnerExclusion for Owned {}
static SERIAL: AtomicU64 = AtomicU64::new(0);

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "spindle-bundle-{name}-{}-{}",
        spindle_store::process_id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// One committed step with an effect whose attempt is reserved and never answered:
/// the unknown outcome a move must carry.
fn history(store: &mut Store) {
    let command = Command {
        scope: "operator".into(),
        id: "input".into(),
        lineage: 0,
        kind: "test".into(),
        payload: b"retained".to_vec(),
    };
    let admission = store.admit(&command).unwrap();
    store
        .commit_proposal(&Proposal {
            id: "commit".into(),
            scope: command.scope,
            observation: Observation::Admission(admission.sequence),
            expected_revision: 0,
            owner_epoch: store.epoch(),
            engine: "fixture".into(),
            continuation: b"waiting".to_vec(),
            workspace: None,
            effects: vec![EffectIntent {
                id: "effect".into(),
                adapter: "fixture-host".into(),
                input: b"inert".to_vec(),
            }],
            state: None,
        })
        .unwrap();
    store
        .reserve_attempt(&Attempt {
            id: "attempt".into(),
            effect: "effect".into(),
            owner_epoch: store.epoch(),
            implementation: "fixture-v1".into(),
            grant_reference: "fixture".into(),
        })
        .unwrap();
    store
        .set_metadata("settings", br#"{"provider":"fake"}"#)
        .unwrap();
}

fn memory() -> Store {
    Store::from_connection(
        Connection::open_in_memory().unwrap(),
        ConnectionProfile::Memory,
        Owned,
    )
    .unwrap()
}

/// What a moved store must keep: head, call identity, the unknown, the settings.
fn assert_same_session(moved: &mut Store, head: u64, call: &str) {
    assert_eq!(moved.snapshot().unwrap().revision, head);
    assert_eq!(
        moved.call_id("effect").unwrap(),
        call,
        "the same call identity"
    );
    assert_eq!(
        moved.reconciliation().unwrap().unresolved_attempts,
        ["attempt"],
        "an unknown outcome moves unknown"
    );
    assert!(moved.receipt("attempt").unwrap().is_none());
    assert_eq!(
        moved.metadata("settings").unwrap().as_deref(),
        Some(&br#"{"provider":"fake"}"#[..])
    );
    moved.check_integrity().unwrap();
}

#[test]
fn an_image_carries_the_whole_store_from_memory_and_reopens_as_memory() {
    let mut store = memory();
    history(&mut store);
    let head = store.snapshot().unwrap().revision;
    let call = store.call_id("effect").unwrap();
    let image = store.image().unwrap();
    let facts = check_image(&image).unwrap();
    assert_eq!(facts.head, head);
    assert!(call.starts_with(&facts.identity), "{call} {facts:?}");
    let mut moved = Store::from_image(&image, Owned).unwrap();
    assert_same_session(&mut moved, head, &call);
    assert!(moved.epoch() > 0);
    assert_eq!(
        moved.crashes(),
        0,
        "the exporting owner, still open, is no crash of the copy"
    );
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn images_of_wal_and_rollback_stores_reopen_as_files_with_the_same_session() {
    // A native (WAL) store, its frames not yet checkpointed while it is open.
    let root = temp("wal").join("store");
    std::fs::create_dir_all(root.parent().unwrap()).unwrap();
    let mut native = Store::open(&root).unwrap();
    history(&mut native);
    let head = native.snapshot().unwrap().revision;
    let call = native.call_id("effect").unwrap();
    let image = native.image().unwrap();
    assert_eq!(&image[18..20], &[1, 1], "a rollback-journal file");
    drop(native);
    // Written as another directory's store, it opens natively.
    let copy = temp("copy").join("store");
    std::fs::create_dir_all(&copy).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(copy.join("authority.sqlite"), &image).unwrap();
    std::fs::write(copy.join("owner.lock"), b"").unwrap();
    let mut reopened = Store::open(&copy).unwrap();
    assert_same_session(&mut reopened, head, &call);
    drop(reopened);
    // And through a host-supplied rollback connection (as the package opens files).
    let file = temp("rollback");
    std::fs::create_dir_all(&file).unwrap();
    let path = file.join("authority.sqlite");
    std::fs::write(&path, &image).unwrap();
    let mut rollback = Store::from_connection(
        Connection::open(&path).unwrap(),
        ConnectionProfile::Persistent {
            journal: JournalMode::Delete,
            initialization: Initialization::Open,
        },
        Owned,
    )
    .unwrap();
    assert_same_session(&mut rollback, head, &call);
    // An image of that rollback store is the same session again.
    let again = rollback.image().unwrap();
    assert_eq!(check_image(&again).unwrap().head, head);
    for dir in [
        root.parent().unwrap().to_path_buf(),
        copy.parent().unwrap().to_path_buf(),
        file,
    ] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn an_image_is_refused_unless_it_is_this_builds_whole_valid_store() {
    let mut store = memory();
    history(&mut store);
    let image = store.image().unwrap();
    let mut wal = image.clone();
    wal[18] = 2;
    wal[19] = 2;
    let foreign = Connection::open_in_memory().unwrap();
    foreign.execute_batch("CREATE TABLE t(x)").unwrap();
    let foreign = foreign.serialize(rusqlite::MAIN_DB).unwrap().to_vec();
    // The second page's b-tree header (pages are 4096 bytes).
    let mut damaged = image.clone();
    damaged[4096..4108].fill(0xff);
    for (what, bytes) in [
        ("empty", vec![]),
        ("cut", image[..image.len() / 2].to_vec()),
        ("WAL header", wal),
        ("another application", foreign),
        ("damaged pages", damaged),
    ] {
        assert!(check_image(&bytes).is_err(), "{what}");
        assert!(Store::from_image(&bytes, Owned).is_err(), "{what}");
    }
    assert!(matches!(
        Store::from_image(&[0; 10], Owned),
        Err(Error::Incompatible)
    ));
}

fn files(store: &Store) -> Vec<BundleFile> {
    vec![BundleFile {
        path: STORE.into(),
        bytes: store.image().unwrap(),
    }]
}

/// The bundle with its manifest edited by `edit` (and its checksum made to match).
fn reframed(bundle: &[u8], edit: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
    let length = u32::from_le_bytes(bundle[20..24].try_into().unwrap()) as usize;
    let mut manifest: serde_json::Value = serde_json::from_slice(&bundle[24..24 + length]).unwrap();
    edit(&mut manifest);
    let manifest = manifest.to_string();
    let mut out = bundle[..20].to_vec();
    out.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
    out.extend_from_slice(manifest.as_bytes());
    out.extend_from_slice(&bundle[24 + length..bundle.len() - 32]);
    let sum = Sha256::digest(&out);
    out.extend_from_slice(&sum);
    out
}

#[test]
fn a_bundle_round_trips_with_mirrors_filled_and_the_store_last() {
    let mut store = memory();
    history(&mut store);
    let head = store.snapshot().unwrap().revision;
    let call = store.call_id("effect").unwrap();
    let mut task = memory();
    history(&mut task);
    let key = "0123456789abcdef0123";
    let made = bundle::write(vec![
        BundleFile {
            path: STORE.into(),
            bytes: store.image().unwrap(),
        },
        BundleFile {
            path: format!("tasks/{key}/{STORE}"),
            bytes: task.image().unwrap(),
        },
        BundleFile {
            path: format!("tasks/{key}/task.json"),
            bytes: br#"{"prompt":"p"}"#.to_vec(),
        },
    ])
    .unwrap();
    assert_eq!(
        (made.facts.head, made.facts.identity.len(), made.files),
        (head, 32, 5)
    );
    assert!(made.bytes.starts_with(bundle::MAGIC));
    let read = bundle::read(&made.bytes).unwrap();
    assert_eq!(read.head, head);
    assert!(call.starts_with(&read.identity));
    let paths: Vec<&str> = read.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            format!("tasks/{key}/session.json").as_str(),
            &format!("tasks/{key}/task.json"),
            &format!("tasks/{key}/{STORE}"),
            "session.json",
            STORE,
        ],
        "each store's settings mirror is filled in; the session's store comes last"
    );
    let image = &read.files.last().unwrap().bytes;
    assert_same_session(&mut Store::from_image(image, Owned).unwrap(), head, &call);
}

#[test]
fn a_damaged_cut_foreign_or_escaping_bundle_is_refused() {
    let mut store = memory();
    history(&mut store);
    let good = bundle::write(files(&store)).unwrap().bytes;
    bundle::read(&good).unwrap();
    // A copy's bundle (its stores checked as far as the bundle records them) is the same
    // bundle, byte for byte: only how much was checked before writing it differs.
    assert_eq!(bundle::write_copy(files(&store)).unwrap().bytes, good);
    let mut flipped = good.clone();
    let middle = flipped.len() / 2;
    flipped[middle] ^= 1;
    let mut wrong_format = good.clone();
    wrong_format[16] = 2;
    let refused: Vec<(&str, Vec<u8>)> = vec![
        ("empty", vec![]),
        (
            "not a bundle",
            b"spindle-sessionX0000000000000000000000000000000000000000".to_vec(),
        ),
        ("a flipped byte", flipped),
        ("cut short", good[..good.len() - 10].to_vec()),
        ("trailing bytes", [good.clone(), vec![0]].concat()),
        ("a newer format", reframed(&wrong_format, |_| {})),
        (
            "another schema",
            reframed(&good, |m| m["schema"] = serde_json::json!(18)),
        ),
        (
            "a path outside the session",
            reframed(&good, |m| {
                m["files"][0]["path"] = serde_json::json!("../escape")
            }),
        ),
        (
            "a wrong digest",
            reframed(&good, |m| m["files"][0]["sha256"] = serde_json::json!("00")),
        ),
        (
            "a size past the end",
            reframed(&good, |m| {
                m["files"][0]["size"] = serde_json::json!(1u64 << 40)
            }),
        ),
        (
            "another head",
            reframed(&good, |m| m["head"] = serde_json::json!(99)),
        ),
        (
            "another identity",
            reframed(&good, |m| m["identity"] = serde_json::json!("00")),
        ),
    ];
    for (what, bytes) in refused {
        assert!(bundle::read(&bytes).is_err(), "{what}");
    }
    // Writing refuses what reading would.
    let image = store.image().unwrap();
    for (what, files) in [
        (
            "no session store",
            vec![BundleFile {
                path: "session.json".into(),
                bytes: b"{}".to_vec(),
            }],
        ),
        (
            "an escaping path",
            vec![BundleFile {
                path: "../x".into(),
                bytes: vec![],
            }],
        ),
        (
            "a store that is no store",
            vec![BundleFile {
                path: STORE.into(),
                bytes: b"not sqlite".to_vec(),
            }],
        ),
        (
            "a path twice",
            vec![
                BundleFile {
                    path: STORE.into(),
                    bytes: image.clone(),
                },
                BundleFile {
                    path: STORE.into(),
                    bytes: image.clone(),
                },
            ],
        ),
        (
            "a task sidecar with no task store",
            vec![
                BundleFile {
                    path: STORE.into(),
                    bytes: image.clone(),
                },
                BundleFile {
                    path: "tasks/0123456789abcdef0123/task.json".into(),
                    bytes: b"{}".to_vec(),
                },
            ],
        ),
        (
            "a task identity that is not JSON",
            vec![
                BundleFile {
                    path: STORE.into(),
                    bytes: image.clone(),
                },
                BundleFile {
                    path: format!("tasks/0123456789abcdef0123/{STORE}"),
                    bytes: image.clone(),
                },
                BundleFile {
                    path: "tasks/0123456789abcdef0123/task.json".into(),
                    bytes: b"{".to_vec(),
                },
            ],
        ),
    ] {
        assert!(bundle::write_copy(files.clone()).is_err(), "copy: {what}");
        assert!(bundle::write(files).is_err(), "{what}");
    }
    // A settings mirror that disagrees with its store (even one that is not JSON) is
    // set right from the store, which every host reads.
    for mirror in [br#"{"provider":"other"}"#.to_vec(), b"{".to_vec()] {
        let made = bundle::write(vec![
            BundleFile {
                path: STORE.into(),
                bytes: image.clone(),
            },
            BundleFile {
                path: "session.json".into(),
                bytes: mirror,
            },
        ])
        .unwrap();
        let read = bundle::read(&made.bytes).unwrap();
        let kept = read
            .files
            .iter()
            .find(|file| file.path == "session.json")
            .unwrap();
        assert_eq!(kept.bytes, br#"{"provider":"fake"}"#);
    }
}
