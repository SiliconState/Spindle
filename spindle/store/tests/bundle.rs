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
    let mut foreign = Connection::open_in_memory().unwrap();
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
        // Where it lies, the same, and the file is left as it came.
        #[cfg(not(target_family = "wasm"))]
        {
            let dir = temp("refused-image");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("authority.sqlite");
            std::fs::write(&path, &bytes).unwrap();
            assert!(spindle_store::check_image_file(&path).is_err(), "{what}");
            assert!(std::fs::read(&path).unwrap() == bytes, "{what}");
            assert_eq!(files_under(&dir), 1, "{what}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    #[cfg(not(target_family = "wasm"))]
    assert!(spindle_store::check_image_file(&temp("missing").join("authority.sqlite")).is_err());
    assert!(matches!(
        Store::from_image(&[0; 10], Owned),
        Err(Error::Incompatible)
    ));
}

fn files(store: &mut Store) -> Vec<BundleFile> {
    vec![BundleFile {
        path: STORE.into(),
        bytes: store.image().unwrap(),
    }]
}

/// `bundle::read` of `bytes`, which every native import does from a file instead
/// (`bundle::unpack_file`, into a staging directory, holding no file in memory): both
/// must answer alike, the same refusal word for word or the same files (the staged ones
/// byte for byte, in the same order, nothing else beside them).
fn read_both(bytes: &[u8]) -> Result<bundle::Bundle, String> {
    let read = bundle::read(bytes);
    let dir = temp("unpack");
    let stage = dir.join("stage");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(dir.join("bundle"), bytes).unwrap();
    let mut file = std::fs::File::open(dir.join("bundle")).unwrap();
    let unpacked = bundle::unpack_file(&mut file, &stage);
    match (&read, unpacked) {
        (Ok(bundle), Ok(unpacked)) => {
            assert_eq!(
                (unpacked.head, &unpacked.identity),
                (bundle.head, &bundle.identity)
            );
            let paths: Vec<&str> = bundle.files.iter().map(|f| f.path.as_str()).collect();
            assert_eq!(unpacked.files, paths);
            for file in &bundle.files {
                assert!(
                    std::fs::read(stage.join(&file.path)).unwrap() == file.bytes,
                    "{} staged as read",
                    file.path
                );
            }
            assert_eq!(
                files_under(&stage),
                bundle.files.len(),
                "nothing else staged"
            );
        }
        (Err(why), Err(bundle::UnpackError::Refused(unpacked))) => assert_eq!(why, &unpacked),
        (read, unpacked) => panic!(
            "read: {:?}, unpacked: {unpacked:?}",
            read.as_ref().map(|bundle| bundle.files.len())
        ),
    }
    drop(file);
    let _ = std::fs::remove_dir_all(&dir);
    read
}

/// How many files are under `dir`, however deep.
fn files_under(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                files_under(&entry.path())
            } else {
                1
            }
        })
        .sum()
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
    let read = read_both(&made.bytes).unwrap();
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
    let good = bundle::write(files(&mut store)).unwrap().bytes;
    read_both(&good).unwrap();
    // A copy's bundle (its stores checked as far as the bundle records them) is the same
    // bundle, byte for byte: only how much was checked before writing it differs.
    assert_eq!(bundle::write_copy(files(&mut store)).unwrap().bytes, good);
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
        assert!(read_both(&bytes).is_err(), "{what}");
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
        let read = read_both(&made.bytes).unwrap();
        let kept = read
            .files
            .iter()
            .find(|file| file.path == "session.json")
            .unwrap();
        assert_eq!(kept.bytes, br#"{"provider":"fake"}"#);
    }
}

/// A bundle of `manifest` and its files' bytes `body`, its checksum made to match.
fn framed(manifest: &serde_json::Value, body: &[u8]) -> Vec<u8> {
    let manifest = manifest.to_string();
    let mut out = bundle::MAGIC.to_vec();
    out.extend_from_slice(&bundle::FORMAT.to_le_bytes());
    out.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
    out.extend_from_slice(manifest.as_bytes());
    out.extend_from_slice(body);
    let sum = Sha256::digest(&out);
    out.extend_from_slice(&sum);
    out
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn a_manifest_is_checked_before_any_file_and_no_sidecar_is_held_past_its_size() {
    let mut store = memory();
    history(&mut store);
    let image = store.image().unwrap();
    let good = bundle::write(files(&mut store)).unwrap().bytes;
    let length = u32::from_le_bytes(good[20..24].try_into().unwrap()) as usize;
    let base: serde_json::Value = serde_json::from_slice(&good[24..24 + length]).unwrap();
    let manifest = |files: &[(&str, &[u8], bool)]| {
        let mut manifest = base.clone();
        manifest["files"] = files
            .iter()
            .map(|(path, bytes, right)| {
                serde_json::json!({
                    "path": path,
                    "size": bytes.len(),
                    "sha256": if *right { sha256(bytes) } else { "00".into() },
                })
            })
            .collect();
        let body: Vec<u8> = files
            .iter()
            .flat_map(|(_, bytes, _)| bytes.to_vec())
            .collect();
        framed(&manifest, &body)
    };
    let key = "0123456789abcdef0123";
    let task_identity = format!("tasks/{key}/task.json");
    // One byte more than a sidecar may hold (4 MiB).
    let large = vec![b' '; 4 * 1024 * 1024 + 1];
    // Every file's digest is wrong: a check that read any file first would refuse for
    // that instead.
    for (what, files, extra, why) in [
        (
            "an escaping path",
            vec![(STORE, &image[..]), ("../escape", b"abc")],
            0,
            "is not a path a session holds",
        ),
        (
            "a path twice",
            vec![(STORE, &image[..]), (STORE, &image[..])],
            0,
            "is in the bundle twice",
        ),
        (
            "a sidecar over its size",
            vec![(bundle::PACK_SESSION, &large[..]), (STORE, &image[..])],
            0,
            "too large for a sidecar",
        ),
        (
            "a task's sidecar with no task store",
            vec![(task_identity.as_str(), b"{}"), (STORE, &image[..])],
            0,
            "has no store beside it",
        ),
        (
            "no session store",
            vec![("session.json", b"{}")],
            0,
            "carries the session's own store",
        ),
        (
            "sizes short of the body",
            vec![(STORE, &image[..])],
            1,
            "bytes follow its last file",
        ),
    ] {
        let wrong: Vec<(&str, &[u8], bool)> = files
            .iter()
            .map(|(path, bytes)| (*path, *bytes, false))
            .collect();
        let mut bytes = manifest(&wrong);
        if extra > 0 {
            // A byte no file names, before the checksum.
            bytes.insert(bytes.len() - 32, 0);
            let sum = Sha256::digest(&bytes[..bytes.len() - 32]);
            let at = bytes.len() - 32;
            bytes[at..].copy_from_slice(&sum);
        }
        let error = read_both(&bytes).unwrap_err();
        assert!(error.contains(why), "{what}: {error}");
    }
    // A settings mirror larger than a sidecar is checked against its digest but never
    // held: its store's settings replace it, as they always did...
    let read = read_both(&manifest(&[
        ("session.json", &large, true),
        (STORE, &image, true),
    ]))
    .unwrap();
    let mirror = read
        .files
        .iter()
        .find(|f| f.path == "session.json")
        .unwrap();
    assert_eq!(mirror.bytes, br#"{"provider":"fake"}"#);
    let error = read_both(&manifest(&[
        ("session.json", &large, false),
        (STORE, &image, true),
    ]))
    .unwrap_err();
    assert!(
        error.contains("session.json does not match its digest"),
        "{error}"
    );
    // ...and beside a store with no settings to replace it, it is refused as it always
    // was. Writing takes and refuses the same.
    let bare = memory().image().unwrap();
    let task_store = format!("tasks/{key}/{STORE}");
    let task_mirror = format!("tasks/{key}/session.json");
    let error = read_both(&manifest(&[
        (&task_mirror, &large, true),
        (&task_store, &bare, true),
        (STORE, &image, true),
    ]))
    .unwrap_err();
    assert!(
        error.contains("session.json is too large for a sidecar"),
        "{error}"
    );
    let file = |path: &str, bytes: &[u8]| BundleFile {
        path: path.into(),
        bytes: bytes.to_vec(),
    };
    bundle::write(vec![file(STORE, &image), file("session.json", &large)]).unwrap();
    let error = bundle::write(vec![
        file(STORE, &image),
        file(&task_store, &bare),
        file(&task_mirror, &large),
    ])
    .err()
    .unwrap();
    assert!(
        error.contains("session.json is too large for a sidecar"),
        "{error}"
    );
}

/// How many bytes the files under `dir` hold, however deep.
fn bytes_under(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                bytes_under(&entry.path())
            } else {
                entry.metadata().unwrap().len()
            }
        })
        .sum()
}

/// A store's settings too large for a sidecar are refused before they are kept anywhere
/// (with what the sidecar check says of the mirror they would be): a small bundle of
/// many such stores never makes its staging directory hold more than its own files, nor
/// `read` hold every store's settings, and both refuse it alike.
#[test]
fn settings_too_large_for_a_sidecar_are_refused_before_they_are_kept() {
    let mut store = memory();
    history(&mut store);
    // A little over a sidecar's 4 MiB, compressed in the store to almost nothing.
    let mut settings = br#"{"provider":"fake","pad":""#.to_vec();
    settings.extend(std::iter::repeat_n(b'x', 5 << 20));
    settings.extend_from_slice(br#""}"#);
    store.set_metadata("settings", &settings).unwrap();
    let image = store.image().unwrap();
    let facts = check_image(&image).unwrap();
    let mut paths = vec![STORE.to_string()];
    paths.extend((0..8).map(|task| format!("tasks/{task:020x}/{STORE}")));
    let manifest = serde_json::json!({
        "format": bundle::FORMAT,
        "schema": spindle_store::CAPABILITIES.schema,
        "head": facts.head,
        "identity": facts.identity,
        "files": paths
            .iter()
            .map(|path| serde_json::json!({"path": path, "size": image.len(), "sha256": sha256(&image)}))
            .collect::<Vec<_>>(),
    });
    let bytes = framed(&manifest, &image.repeat(paths.len()));
    let why = "session.json is too large for a sidecar";
    let refused = read_both(&bytes).unwrap_err();
    assert!(refused.ends_with(why), "{refused}");
    let written = bundle::write(
        paths
            .iter()
            .map(|path| BundleFile {
                path: path.clone(),
                bytes: image.clone(),
            })
            .collect(),
    );
    assert!(
        written.err().unwrap().ends_with(why),
        "writing refuses it too"
    );
    let dir = temp("oversized-settings");
    let stage = dir.join("stage");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(dir.join("bundle"), &bytes).unwrap();
    let mut file = std::fs::File::open(dir.join("bundle")).unwrap();
    let unpacked = bundle::unpack_file(&mut file, &stage);
    assert!(
        matches!(&unpacked, Err(bundle::UnpackError::Refused(refused)) if refused.ends_with(why)),
        "{unpacked:?}"
    );
    let staged = bytes_under(&stage);
    assert!(
        staged <= image.len() as u64 * paths.len() as u64,
        "{staged} bytes staged from a bundle of {} bytes",
        bytes.len()
    );
    drop(file);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A store the previous build left (schema 19), as a file: checked where it lies, it
/// is checked as the open will migrate it, yet the file stays byte for byte as it came
/// (no journal beside it either); unpacked from a bundle, the same; and it then opens,
/// migrated, as any previous-schema store does.
#[cfg(not(target_family = "wasm"))]
#[test]
fn a_previous_schema_store_is_checked_in_place_and_left_as_it_came() {
    let dir = temp("previous");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("authority.sqlite");
    let text = b"the workspace keeps this line, again and again\n".repeat(2048);
    let digest = spindle_store::artifact::object_digest(&text);
    {
        let mut store = Store::from_connection(
            Connection::open(&path).unwrap(),
            ConnectionProfile::Persistent {
                journal: JournalMode::Delete,
                initialization: Initialization::Create,
            },
            Owned,
        )
        .unwrap();
        history(&mut store);
        let stage = spindle_store::artifact::Stage {
            id: "files".into(),
            scope: "operator".into(),
            owner_epoch: store.epoch(),
        };
        store.begin_stage(&stage).unwrap();
        assert_eq!(store.put_object(&stage, &text).unwrap(), digest);
    }
    let db = Connection::open(&path).unwrap();
    db.execute(
        "UPDATE objects SET bytes=?1,size=NULL WHERE digest=?2",
        rusqlite::params![text, digest],
    )
    .unwrap();
    db.execute_batch("ALTER TABLE objects DROP COLUMN size; PRAGMA user_version=19;")
        .unwrap();
    drop(db);
    let image = std::fs::read(&path).unwrap();
    assert_eq!(&image[60..64], &19u32.to_be_bytes());
    // Neither the file nor its directory is written while it is checked, not even for
    // a moment (a journal made and removed beside it would change the directory).
    #[cfg(unix)]
    let long_ago = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    #[cfg(unix)]
    for written in [&path, &dir] {
        std::fs::File::open(written)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
    }
    let facts = spindle_store::check_image_file(&path).unwrap();
    #[cfg(unix)]
    for written in [&path, &dir] {
        let modified = std::fs::metadata(written).unwrap().modified().unwrap();
        assert_eq!(modified, long_ago, "{} was written", written.display());
    }
    assert_eq!(facts, check_image(&image).unwrap(), "the same answer");
    assert!(std::fs::read(&path).unwrap() == image, "byte for byte");
    assert_eq!(files_under(&dir), 1, "no journal left beside it");
    // A damaged copy is refused there as in memory.
    let mut damaged = image.clone();
    damaged[4096..4108].fill(0xff);
    std::fs::write(&path, &damaged).unwrap();
    assert!(spindle_store::check_image_file(&path).is_err());
    assert!(std::fs::read(&path).unwrap() == damaged);
    std::fs::write(&path, &image).unwrap();
    // In a bundle (`read` compares the staged store with the image, byte for byte).
    let made = bundle::write(vec![BundleFile {
        path: STORE.into(),
        bytes: image.clone(),
    }])
    .unwrap();
    assert_eq!(read_both(&made.bytes).unwrap().head, facts.head);
    // Then it opens, migrated in place.
    let mut store = Store::from_connection(
        Connection::open(&path).unwrap(),
        ConnectionProfile::Persistent {
            journal: JournalMode::Delete,
            initialization: Initialization::Open,
        },
        Owned,
    )
    .unwrap();
    assert_eq!(store.read_object("operator", &digest).unwrap(), text);
    store.check_integrity().unwrap();
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}
