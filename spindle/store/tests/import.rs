//! Native bundle imports (`agent::import_session`, as the CLI and JSON-RPC import;
//! the HTTP host stages the same way): a bundle is unpacked into a private staging
//! directory and checked there, holding neither its files nor a store in memory
//! whatever its size, and a refused one leaves nothing behind.
#![cfg(feature = "agent")]
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use spindle_store::bundle::{self, BundleFile, STORE};
use spindle_store::transition::{Observation, Proposal};
use spindle_store::{
    Command, ConnectionProfile, Initialization, JournalMode, OwnerExclusion, Store, agent,
};
use std::path::{Path, PathBuf};

struct Owned;
impl OwnerExclusion for Owned {}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "spindle-import-{name}-{}",
        spindle_store::process_id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A store image (a rollback-journal file) with one committed step, settings, and
/// `objects` distinct, incompressible objects of 1 MiB each: a store whose every page a
/// whole check reads (each object against its digest).
fn image(dir: &Path, objects: usize) -> Vec<u8> {
    let path = dir.join("source.sqlite");
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
                continuation: b"saved".to_vec(),
                workspace: None,
                effects: vec![],
                state: None,
            })
            .unwrap();
        store
            .set_metadata("settings", br#"{"provider":"fake"}"#)
            .unwrap();
        let stage = spindle_store::artifact::Stage {
            id: "files".into(),
            scope: "operator".into(),
            owner_epoch: store.epoch(),
        };
        store.begin_stage(&stage).unwrap();
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..objects {
            let mut bytes = vec![0u8; 1 << 20];
            for word in bytes.chunks_exact_mut(8) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                word.copy_from_slice(&state.to_le_bytes());
            }
            store.put_object(&stage, &bytes).unwrap();
        }
    }
    let image = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    image
}

/// The names in `dir`.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// `bytes` with the body edited by `edit` and the trailing checksum made to match.
fn resummed(bytes: &[u8], edit: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut body = bytes[..bytes.len() - 32].to_vec();
    edit(&mut body);
    let sum = Sha256::digest(&body);
    body.extend_from_slice(&sum);
    body
}

/// Where in `bundle` the file at `path` starts, and its size (from its manifest).
fn span(bundle: &[u8], path: &str) -> (usize, usize) {
    let length = u32::from_le_bytes(bundle[20..24].try_into().unwrap()) as usize;
    let manifest: serde_json::Value = serde_json::from_slice(&bundle[24..24 + length]).unwrap();
    let mut at = 24 + length;
    for entry in manifest["files"].as_array().unwrap() {
        let size = entry["size"].as_u64().unwrap() as usize;
        if entry["path"] == path {
            return (at, size);
        }
        at += size;
    }
    panic!("{path} is not in the bundle")
}

/// `bundle` with the file at `path` replaced by `bytes` (of its size), its digest in
/// the manifest and the trailing checksum made to match.
fn replaced(bundle: &[u8], path: &str, bytes: &[u8]) -> Vec<u8> {
    let (at, size) = span(bundle, path);
    assert_eq!(bytes.len(), size, "{path}: the same size");
    let length = u32::from_le_bytes(bundle[20..24].try_into().unwrap()) as usize;
    let mut manifest: serde_json::Value = serde_json::from_slice(&bundle[24..24 + length]).unwrap();
    for entry in manifest["files"].as_array_mut().unwrap() {
        if entry["path"] == path {
            let digest: String = Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            entry["sha256"] = digest.into();
        }
    }
    let manifest = manifest.to_string();
    assert_eq!(manifest.len(), length, "a digest's hex for a digest's hex");
    resummed(bundle, |body| {
        body[24..24 + length].copy_from_slice(manifest.as_bytes());
        body[at..at + size].copy_from_slice(bytes);
    })
}

/// Refused anywhere along the way (the whole file's checksum, a file's digest after
/// others were staged, a store, a sidecar read back), an import says what `read` says
/// and leaves nothing: no staging directory, and the place as it was.
#[test]
fn a_refused_import_leaves_nothing_staged_or_in_place() {
    let root = temp("refused");
    let image = image(&root, 2);
    let key = "0123456789abcdef0123";
    let good = bundle::write(vec![
        BundleFile {
            path: STORE.into(),
            bytes: image.clone(),
        },
        BundleFile {
            path: format!("tasks/{key}/{STORE}"),
            bytes: image.clone(),
        },
        BundleFile {
            path: format!("tasks/{key}/task.json"),
            bytes: br#"{"prompt":"p"}"#.to_vec(),
        },
    ])
    .unwrap()
    .bytes;
    let (store_at, store_size) = span(&good, STORE);
    // The session's store is the bundle's last file: every other file is staged first.
    assert_eq!(store_at + store_size, good.len() - 32);
    // A store damaged with its digest made to match: refused by the store's check.
    let mut damaged = image.clone();
    damaged[4096..4108].fill(0xff);
    let bad_store = replaced(&good, STORE, &damaged);
    // A task identity that is not JSON, its digest made to match: refused read back.
    let bad_sidecar = replaced(
        &good,
        &format!("tasks/{key}/task.json"),
        br#"{"prompt":"p","#,
    );
    for (what, bytes, why) in [
        (
            "cut short",
            good[..good.len() - 100].to_vec(),
            "its checksum does not match",
        ),
        (
            "a file not its digest",
            resummed(&good, |body| body[store_at + 5000] ^= 1),
            "store/authority.sqlite does not match its digest",
        ),
        (
            "a damaged store",
            bad_store,
            "store/authority.sqlite is not a store this build opens",
        ),
        (
            "a sidecar not JSON",
            bad_sidecar,
            "task.json in the bundle is not JSON",
        ),
    ] {
        let read = bundle::read(&bytes).err().unwrap();
        assert!(read.contains(why), "{what}: {read}");
        let source = root.join("bundle.spindle");
        std::fs::write(&source, &bytes).unwrap();
        // Into a new place, and into an empty one.
        for empty in [false, true] {
            let place = root.join("place");
            if empty {
                std::fs::create_dir(&place).unwrap();
            }
            let refused = agent::import_session(&source, &place).err().unwrap();
            assert_eq!(refused, read, "{what}: the same refusal");
            let mut left = vec!["bundle.spindle".to_string()];
            if empty {
                assert!(names(&place).is_empty(), "{what}: the place stays empty");
                left.push("place".into());
            }
            assert_eq!(names(&root), left, "{what}: nothing staged stays");
            let _ = std::fs::remove_dir(&place);
        }
    }
    // The good bundle imports whole, and opens with its task.
    let source = root.join("bundle.spindle");
    std::fs::write(&source, &good).unwrap();
    let place = root.join("place");
    let imported = agent::import_session(&source, &place).unwrap();
    assert_eq!(imported["files"], 5);
    assert_eq!(names(&root), ["bundle.spindle", "place"]);
    for store in [
        place.join("store"),
        place.join(format!("tasks/{key}/store")),
    ] {
        assert!(store.join("owner.lock").is_file(), "{}", store.display());
        Store::open(&store).unwrap();
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Peak resident memory since the last reset (Linux: VmHWM, reset through clear_refs).
#[cfg(target_os = "linux")]
fn peak_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status
        .lines()
        .find(|line| line.starts_with("VmHWM:"))
        .unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// Run by the memory tests below in a process of its own: how much the peak resident
/// memory grew importing (`import`), reading in memory (`read`: the bundle's bytes
/// already held, as `read`'s caller holds them) or checking a store file where it lies
/// (`check`), and what that answered (`ok`, or why not).
#[cfg(target_os = "linux")]
#[test]
#[ignore = "run in a child process by the memory tests"]
fn import_probe() {
    let (Some(source), Some(how)) = (
        std::env::var_os("SPINDLE_IMPORT_SOURCE"),
        std::env::var("SPINDLE_IMPORT_HOW").ok(),
    ) else {
        return;
    };
    let source = PathBuf::from(source);
    let place = std::env::var_os("SPINDLE_IMPORT_PLACE").map(PathBuf::from);
    let bytes = (how == "read").then(|| std::fs::read(&source).unwrap());
    let _ = std::fs::write("/proc/self/clear_refs", "5");
    let before = peak_kib();
    let answer = match how.as_str() {
        "import" => agent::import_session(&source, &place.unwrap()).map(drop),
        "read" => bundle::read(bytes.as_deref().unwrap()).map(drop),
        "check" => spindle_store::check_image_file(&source)
            .map(drop)
            .map_err(|error| format!("{error:?}")),
        other => panic!("{other}"),
    };
    println!("probe-grew-kib={}", peak_kib().saturating_sub(before));
    println!("probe-answer={}", answer.err().as_deref().unwrap_or("ok"));
}

/// `import_probe` in a child process: how much its peak grew (KiB) and its answer.
#[cfg(target_os = "linux")]
fn probe(how: &str, source: &Path, place: &Path) -> (u64, String) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "import_probe", "--ignored", "--nocapture"])
        .env("SPINDLE_IMPORT_SOURCE", source)
        .env("SPINDLE_IMPORT_HOW", how)
        .env("SPINDLE_IMPORT_PLACE", place)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{how}: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let line = |prefix: &str| {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .unwrap_or_else(|| panic!("{how}: no {prefix} in {stdout}"))
            .to_string()
    };
    (
        line("probe-grew-kib=").parse().unwrap(),
        line("probe-answer="),
    )
}

/// A bundle whose store is over 64 MiB imports with its peak memory growing by a
/// fraction of that: its files are copied through a fixed buffer and its store is
/// checked where it lies through a bounded page cache. Reading the same bundle in
/// memory (as every native import did) grows past the store's size, so the measure
/// sees a store held in memory.
#[cfg(target_os = "linux")]
#[test]
fn an_import_holds_neither_the_bundle_nor_a_store_in_memory() {
    let root = temp("memory");
    let image = image(&root, 72);
    let store_kib = (image.len() / 1024) as u64;
    assert!(image.len() >= 64 << 20, "{store_kib} KiB");
    let made = bundle::write(vec![BundleFile {
        path: STORE.into(),
        bytes: image,
    }])
    .unwrap();
    let source = root.join("large.spindle");
    std::fs::write(&source, &made.bytes).unwrap();
    let head = made.facts.head;
    drop(made);
    let place = root.join("place");
    let (imported, answer) = probe("import", &source, &place);
    assert_eq!(answer, "ok");
    println!("store {store_kib} KiB: an import grew {imported} KiB");
    assert!(
        imported < store_kib / 2,
        "an import of a {store_kib} KiB store grew {imported} KiB"
    );
    let mut store = Store::open(&place.join("store")).unwrap();
    assert_eq!(store.snapshot().unwrap().revision, head);
    drop(store);
    let (held, answer) = probe("read", &source, &root.join("unused"));
    assert_eq!(answer, "ok");
    println!("store {store_kib} KiB: a read in memory grew {held} KiB");
    assert!(
        held > store_kib,
        "a read in memory of a {store_kib} KiB store grew only {held} KiB"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A store with `settings` as its settings (and one committed step).
#[cfg(target_os = "linux")]
fn store_with_settings(path: &Path, settings: &[u8]) -> Vec<u8> {
    {
        let mut store = Store::from_connection(
            Connection::open(path).unwrap(),
            ConnectionProfile::Persistent {
                journal: JournalMode::Delete,
                initialization: Initialization::Create,
            },
            Owned,
        )
        .unwrap();
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
                continuation: b"saved".to_vec(),
                workspace: None,
                effects: vec![],
                state: None,
            })
            .unwrap();
        store.set_metadata("settings", settings).unwrap();
    }
    let image = std::fs::read(path).unwrap();
    std::fs::remove_file(path).unwrap();
    image
}

/// A small bundle of many stores whose settings are each far over a sidecar's size
/// (30 MiB, compressed in the store to almost nothing) is refused at the first, before
/// its settings are kept: `read` never holds every store's settings in memory (nor an
/// import on its disk), and both say what the sidecar check says.
#[cfg(target_os = "linux")]
#[test]
fn settings_over_a_sidecars_size_are_refused_before_any_store_keeps_them() {
    let root = temp("settings");
    let mut settings = br#"{"provider":"fake","pad":""#.to_vec();
    settings.extend(std::iter::repeat_n(b'x', 30 << 20));
    settings.extend_from_slice(br#""}"#);
    let image = store_with_settings(&root.join("settings.sqlite"), &settings);
    let facts = spindle_store::check_image(&image).unwrap();
    let mut paths = vec![STORE.to_string()];
    paths.extend((0..15).map(|task| format!("tasks/{task:020x}/{STORE}")));
    let digest: String = Sha256::digest(&image)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let manifest = serde_json::json!({
        "format": bundle::FORMAT,
        "schema": spindle_store::CAPABILITIES.schema,
        "head": facts.head,
        "identity": facts.identity,
        "files": paths
            .iter()
            .map(|path| serde_json::json!({"path": path, "size": image.len(), "sha256": digest}))
            .collect::<Vec<_>>(),
    })
    .to_string();
    let mut bytes = bundle::MAGIC.to_vec();
    bytes.extend_from_slice(&bundle::FORMAT.to_le_bytes());
    bytes.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
    bytes.extend_from_slice(manifest.as_bytes());
    bytes.extend_from_slice(&image.repeat(paths.len()));
    let sum = Sha256::digest(&bytes);
    bytes.extend_from_slice(&sum);
    assert!(bytes.len() < 8 << 20, "{} bytes", bytes.len());
    let source = root.join("settings.spindle");
    std::fs::write(&source, &bytes).unwrap();
    let why = "session.json is too large for a sidecar";
    let all_kib = (settings.len() / 1024 * paths.len()) as u64;
    let (held, answer) = probe("read", &source, &root.join("unused"));
    println!(
        "{} stores' settings, {all_kib} KiB: a read grew {held} KiB",
        paths.len()
    );
    assert!(answer.ends_with(why), "{answer}");
    assert!(
        held < all_kib / 4,
        "a read of {} stores of {all_kib} KiB of settings grew {held} KiB",
        paths.len()
    );
    let refused = agent::import_session(&source, &root.join("place"))
        .err()
        .unwrap();
    assert!(refused.ends_with(why), "{refused}");
    assert_eq!(names(&root), ["settings.spindle"], "nothing staged stays");
    let _ = std::fs::remove_dir_all(&root);
}

/// A store keeping many tasks, each with a 1 MiB answer, is checked where it lies one
/// task at a time: the check's peak grows by a fraction of the answers it reads.
#[cfg(target_os = "linux")]
#[test]
fn a_stores_tasks_are_checked_one_at_a_time() {
    use spindle_store::meta::{TaskEnd, TaskRun};
    let root = temp("tasks");
    let path = root.join("tasks.sqlite");
    let tasks = 64;
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
        let command = Command {
            scope: "operator".into(),
            id: "input".into(),
            lineage: 0,
            kind: "test".into(),
            payload: b"retained".to_vec(),
        };
        let admission = store.admit(&command).unwrap();
        let stage = spindle_store::artifact::Stage {
            id: "files".into(),
            scope: "operator".into(),
            owner_epoch: store.epoch(),
        };
        store.begin_stage(&stage).unwrap();
        let base = store.stage_file_map(&stage, &[]).unwrap();
        store
            .commit_proposal(&Proposal {
                id: "commit".into(),
                scope: command.scope,
                observation: Observation::Admission(admission.sequence),
                expected_revision: 0,
                owner_epoch: store.epoch(),
                engine: "fixture".into(),
                continuation: b"saved".to_vec(),
                workspace: Some(base.clone()),
                effects: vec![],
                state: None,
            })
            .unwrap();
        store
            .set_metadata("settings", br#"{"provider":"fake"}"#)
            .unwrap();
        let revision = store.snapshot().unwrap().revision;
        let input = br#"{"prompt":"p"}"#;
        let names: Vec<String> = (0..tasks).map(|task| format!("app:t{task}")).collect();
        for task in &names {
            store
                .register_task(&TaskRun {
                    task: task.clone(),
                    origin: None,
                    input: input.to_vec(),
                    base: base.clone(),
                    revision,
                    queued: false,
                    state: "running".into(),
                    answer: None,
                    delivered: None,
                    lease: None,
                })
                .unwrap();
        }
        let answer = serde_json::to_vec(
            &serde_json::json!({"content": "a".repeat(1 << 20), "failed": false}),
        )
        .unwrap();
        for task in &names {
            store
                .finish_task_run(task, input, TaskEnd::Answered(&answer))
                .unwrap();
        }
    }
    let store_kib = std::fs::metadata(&path).unwrap().len() / 1024;
    assert!(store_kib >= tasks << 10, "{store_kib} KiB");
    let (grew, answer) = probe("check", &path, &root.join("unused"));
    println!("store {store_kib} KiB of {tasks} tasks' answers: its check grew {grew} KiB");
    assert_eq!(answer, "ok");
    assert!(
        grew < store_kib / 2,
        "a check of a {store_kib} KiB store of {tasks} tasks' answers grew {grew} KiB"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Each import stages into a directory of its own beside the place, made new and
/// private, so two at once for one place never meet; and what an import stopped midway
/// left there (its process gone, or this process's own that no import uses, in either
/// name's shape) goes with the next import for that place, while a live import's,
/// another place's and anything not a directory stay.
#[test]
fn a_stopped_imports_staging_is_swept_and_a_live_ones_kept() {
    let root = temp("sweep");
    let image = image(&root, 0);
    let bytes = bundle::write(vec![BundleFile {
        path: STORE.into(),
        bytes: image,
    }])
    .unwrap()
    .bytes;
    let source = root.join("bundle.spindle");
    std::fs::write(&source, &bytes).unwrap();
    let place = root.join("place");
    let own = spindle_store::process_id();
    let staging = |root: &Path| -> Vec<String> {
        names(root)
            .into_iter()
            .filter(|name| name.starts_with(".place.importing-"))
            .collect()
    };
    let first = agent::stage_session(&source, &place).unwrap();
    let second = agent::stage_session(&source, &place).unwrap();
    let live = staging(&root);
    assert_eq!(live.len(), 2, "{live:?}");
    for name in &live {
        let (pid, random) = name[".place.importing-".len()..].split_once('-').unwrap();
        assert_eq!(
            (pid, random.len()),
            (own.to_string().as_str(), 16),
            "{name}"
        );
        let meta = std::fs::symlink_metadata(root.join(name)).unwrap();
        assert!(meta.is_dir(), "{name}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(meta.permissions().mode() & 0o777, 0o700, "{name}");
        }
    }
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut stale = vec![
        format!(".place.importing-{own}-0123456789abcdef"),
        format!(".place.importing-{own}"),
    ];
    let mut kept = vec![format!(".other.importing-{own}-0123456789abcdef")];
    #[cfg(unix)]
    {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        child.wait().unwrap();
        stale.push(format!(".place.importing-{}-fedcba9876543210", child.id()));
        kept.push(format!(
            ".place.importing-{}-0123456789abcdef",
            std::os::unix::process::parent_id()
        ));
    }
    for name in stale.iter().chain(&kept) {
        std::fs::create_dir_all(root.join(name).join("store")).unwrap();
        std::fs::write(root.join(name).join(STORE), b"partial").unwrap();
    }
    let file = format!(".place.importing-{own}-00000000000000ff");
    std::fs::write(root.join(&file), b"not a directory").unwrap();
    kept.push(file);
    let third = agent::stage_session(&source, &place).unwrap();
    let left = names(&root);
    for name in &stale {
        assert!(!left.contains(name), "{name} stays");
    }
    for name in kept.iter().chain(&live) {
        assert!(left.contains(name), "{name} was removed");
    }
    assert_eq!(staging(&root).len(), live.len() + 1 + kept.len() - 1);
    drop(first);
    drop(third);
    second.finish().unwrap();
    for name in &live {
        assert!(!root.join(name).exists(), "{name} stays");
    }
    Store::open(&place.join("store")).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

/// Run by `a_relative_path_beginning_file_is_checked_as_a_path` in a process of its own
/// (its working directory set): the identity `check_image_file` finds at the path.
#[cfg(unix)]
#[test]
#[ignore = "run in a child process by a_relative_path_beginning_file_is_checked_as_a_path"]
fn uri_probe() {
    let Some(path) = std::env::var_os("SPINDLE_URI_PROBE") else {
        return;
    };
    let checked = spindle_store::check_image_file(Path::new(&path));
    println!(
        "probe-identity={}",
        checked.map_or_else(|error| format!("{error:?}"), |facts| facts.identity)
    );
}

/// A relative path beginning `file:` is checked as the path it is, never read by
/// SQLite as a URI naming another file: the store checked is the one whose header was
/// read.
#[cfg(unix)]
#[test]
fn a_relative_path_beginning_file_is_checked_as_a_path() {
    let root = temp("uri");
    std::fs::create_dir_all(root.join("file:a")).unwrap();
    std::fs::create_dir_all(root.join("a")).unwrap();
    let named = image(&root, 0);
    std::fs::write(root.join("file:a/s.sqlite"), &named).unwrap();
    // What the URI `file:a/s.sqlite` names: another store, of another identity.
    std::fs::write(root.join("a/s.sqlite"), image(&root, 0)).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "uri_probe", "--ignored", "--nocapture"])
        .env("SPINDLE_URI_PROBE", "file:a/s.sqlite")
        .current_dir(&root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    let identity = stdout
        .lines()
        .find_map(|line| line.strip_prefix("probe-identity="))
        .unwrap_or_else(|| panic!("no identity in {stdout}"));
    assert_eq!(
        identity,
        spindle_store::check_image(&named).unwrap().identity
    );
    let _ = std::fs::remove_dir_all(&root);
}
