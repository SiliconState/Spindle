//! Power loss and disk full against the real store code, through a crash-simulating
//! SQLite VFS (`support/crash_vfs.rs` describes the fault model).
//!
//! Each workload first runs once to count its I/O events and record the store's logical
//! state after every operation. Then, for each crash point, it runs again in a fresh
//! directory with power cut at that event, and every surviving image built from that
//! instant must open, pass the full integrity check, hold exactly the state after the
//! acknowledged operations (or after the one in flight too, never anything partial) and
//! accept new work. By default a reproducible sample of crash points is checked;
//! `SPINDLE_POWER_LOSS=all` checks every point with more images per point
//! (`SPINDLE_POWER_LOSS_SEED` changes the sample).
#![cfg(target_os = "linux")]

#[path = "support/crash_vfs.rs"]
mod crash_vfs;

use crash_vfs::{Image, Plan, Rng, Sim, scratch};
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use spindle_store::artifact::Stage;
use spindle_store::completion::Attempt;
use spindle_store::transition::{EffectIntent, Observation, Proposal, ReceiptBinding};
use spindle_store::workspace::Entry;
use spindle_store::{Command, Store};
use std::collections::{BTreeMap, HashMap};
#[cfg(feature = "dext-turn")]
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn exhaustive() -> bool {
    std::env::var("SPINDLE_POWER_LOSS").is_ok_and(|value| value == "all")
}

fn seed() -> u64 {
    std::env::var("SPINDLE_POWER_LOSS_SEED")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0x5eed)
}

/// Deterministic incompressible bytes.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        out.extend_from_slice(&rng.next().to_le_bytes());
    }
    out.truncate(len);
    out
}

/// A continuation that repeats the history before it, as real steps do, so most of its
/// chunks are shared with the previous step's.
fn history(step: u64) -> Vec<u8> {
    let mut out = noise(1, 96 * 1024);
    for earlier in 2..=step {
        out.extend(noise(earlier * 1000, 6 * 1024));
    }
    out.extend(format!("step {step}").bytes());
    out
}

fn result(step: u64) -> Vec<u8> {
    let mut out = noise(step * 7919, 24 * 1024);
    out.extend(std::iter::repeat_n(b'r', 16 * 1024));
    out
}

fn intent(step: u64) -> EffectIntent {
    EffectIntent {
        id: format!("effect-{step}"),
        adapter: "test-only".into(),
        input: noise(step * 31, 20 * 1024),
    }
}

/// One acknowledged-or-not store operation.
#[derive(Clone, Debug)]
enum Op {
    Open,
    Reopen,
    Close,
    Admit(u64, usize),
    Propose(u64),
    Reserve(u64),
    Receipt(u64),
    Adopt(u64),
    BeginStage(u64),
    Put(u64, u64),
    MapStage(u64),
    Publish(u64),
    Reject(u64),
    Metadata(u64),
    Gc,
}

struct Ctx {
    dir: PathBuf,
    store: Option<Store>,
    admissions: HashMap<u64, u64>,
    intent_revisions: HashMap<u64, u64>,
    attempts: HashMap<u64, Attempt>,
    entries: Vec<Entry>,
    root: Option<String>,
    identity: Option<String>,
}

impl Ctx {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            store: None,
            admissions: HashMap::new(),
            intent_revisions: HashMap::new(),
            attempts: HashMap::new(),
            entries: vec![],
            root: None,
            identity: None,
        }
    }

    fn store(&mut self) -> &mut Store {
        self.store.as_mut().expect("store is open")
    }

    fn run(&mut self, op: &Op) -> spindle_store::Result<()> {
        match *op {
            Op::Open | Op::Reopen => {
                self.store = None;
                self.store = Some(Store::open(&self.dir)?);
                let identity = self.store().call_id("power-loss")?;
                if let Some(prior) = &self.identity {
                    assert_eq!(&identity, prior, "store identity changed on reopen");
                }
                self.identity = Some(identity);
            }
            // Closing checkpoints the WAL into the database file.
            Op::Close => self.store = None,
            Op::Admit(step, size) => {
                let lineage = self.store().lineage()?;
                let admission = self.store().admit(&Command {
                    scope: "operator".into(),
                    id: format!("command-{step}"),
                    lineage,
                    kind: "turn".into(),
                    payload: noise(step, size),
                })?;
                self.admissions.insert(step, admission.sequence);
            }
            Op::Propose(step) | Op::Publish(step) => {
                let store = self.store.as_mut().unwrap();
                let publish = matches!(op, Op::Publish(_));
                let proposal = Proposal {
                    id: format!("proposal-{step}"),
                    scope: "operator".into(),
                    observation: Observation::Admission(self.admissions[&step]),
                    expected_revision: store.snapshot()?.revision,
                    owner_epoch: store.epoch(),
                    engine: "power-v1".into(),
                    continuation: history(step),
                    workspace: if publish { self.root.clone() } else { None },
                    effects: if publish { vec![] } else { vec![intent(step)] },
                    state: None,
                };
                let revision = store.commit_proposal(&proposal)?;
                self.intent_revisions.insert(step, revision);
            }
            Op::Reserve(step) => {
                let attempt = Attempt {
                    id: format!("attempt-{step}"),
                    effect: format!("effect-{step}"),
                    owner_epoch: self.store().epoch(),
                    implementation: "native-v1".into(),
                    grant_reference: "grant-v1".into(),
                };
                self.store().reserve_attempt(&attempt)?;
                self.attempts.insert(step, attempt);
            }
            Op::Receipt(step) => {
                self.store()
                    .ingest_receipt(&format!("attempt-{step}"), &result(step))?;
            }
            Op::Adopt(step) => {
                let store = self.store.as_mut().unwrap();
                let mut continuation = history(step);
                continuation.extend(b" adopted");
                let proposal = Proposal {
                    id: format!("adopt-{step}"),
                    scope: "operator".into(),
                    observation: Observation::Receipt(format!("attempt-{step}")),
                    expected_revision: store.snapshot()?.revision,
                    owner_epoch: store.epoch(),
                    engine: "power-v1".into(),
                    continuation,
                    workspace: None,
                    effects: vec![],
                    state: None,
                };
                let result = result(step);
                store.commit_receipt_proposal(
                    &proposal,
                    &ReceiptBinding {
                        attempt: &self.attempts[&step],
                        intent_revision: self.intent_revisions[&step],
                        intent: &intent(step),
                        result: &result,
                        objects: &[],
                    },
                )?;
            }
            Op::BeginStage(step) => {
                let stage = stage(self.store(), step);
                self.store().begin_stage(&stage)?;
            }
            Op::Put(step, file) => {
                let stage = stage(self.store(), step);
                let object = self
                    .store()
                    .put_object(&stage, &noise(step * 100 + file, 12 * 1024))?;
                self.entries.push(Entry {
                    path: format!("src/file-{file}.txt"),
                    object,
                    executable: false,
                });
            }
            Op::MapStage(step) => {
                let stage = stage(self.store(), step);
                let entries = std::mem::take(&mut self.entries);
                self.root = Some(self.store().stage_file_map(&stage, &entries)?);
            }
            Op::Reject(step) => {
                self.store()
                    .record_rejection(&format!("attempt-{step}"), 429, "rate limited")?;
            }
            Op::Metadata(step) => {
                self.store()
                    .set_metadata("settings", &noise(step, 24 * 1024))?;
            }
            Op::Gc => {
                self.store().collect_garbage()?;
            }
        }
        Ok(())
    }
}

fn stage(store: &Store, step: u64) -> Stage {
    Stage {
        id: format!("stage-{step}"),
        scope: "operator".into(),
        owner_epoch: store.epoch(),
    }
}

/// The store's logical content: every table but the open-time watermark, and the realm
/// row without its epoch (each open advances it). Independent runs have random instance
/// identities; those are checked for persistence separately. Table -> row count and digest.
fn dump(store: &Path) -> Result<BTreeMap<String, String>, String> {
    // The platform VFS: reading is never a simulated event.
    let db = Connection::open_with_flags_and_vfs(
        store.join("authority.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
        c"unix",
    )
    .map_err(|error| error.to_string())?;
    let tables: Vec<String> = db
        .prepare(
            "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT IN ('validated','opens') ORDER BY name",
        )
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()
        })
        .map_err(|error| error.to_string())?;
    let mut out = BTreeMap::new();
    for table in tables {
        let columns: Vec<String> = db
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()
            })
            .map_err(|error| error.to_string())?;
        let columns: Vec<String> = columns
            .into_iter()
            .filter(|column| {
                !(table == "realm" && column == "epoch"
                    || table == "instance" && column == "identity")
            })
            .map(|column| format!("\"{column}\""))
            .collect();
        let order: Vec<String> = (1..=columns.len()).map(|n| n.to_string()).collect();
        let sql = format!(
            "SELECT {} FROM \"{table}\" ORDER BY {}",
            columns.join(","),
            order.join(",")
        );
        let mut statement = db.prepare(&sql).map_err(|error| error.to_string())?;
        let mut rows = statement.query([]).map_err(|error| error.to_string())?;
        let mut hash = Sha256::new();
        let mut count = 0;
        while let Some(row) = rows.next().map_err(|error| error.to_string())? {
            count += 1;
            for index in 0..columns.len() {
                use rusqlite::types::ValueRef;
                match row.get_ref(index).map_err(|error| error.to_string())? {
                    ValueRef::Null => hash.update(b"n"),
                    ValueRef::Integer(value) => {
                        hash.update(b"i");
                        hash.update(value.to_le_bytes());
                    }
                    ValueRef::Real(value) => {
                        hash.update(b"r");
                        hash.update(value.to_le_bytes());
                    }
                    ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
                        hash.update(b"b");
                        hash.update((bytes.len() as u64).to_le_bytes());
                        hash.update(bytes);
                    }
                }
            }
        }
        let digest: String = hash.finalize()[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        out.insert(table, format!("{count}:{digest}"));
    }
    Ok(out)
}

/// What a crash image must be: the state after `acked` operations (all acknowledged),
/// or after the one in flight too.
fn check_image(
    store: &Path,
    acked: usize,
    states: &[BTreeMap<String, String>],
    identity: Option<&str>,
) -> Result<(), String> {
    let mut opened = Store::open(store).map_err(|error| format!("Store::open: {error:?}"))?;
    opened
        .check_integrity()
        .map_err(|error| format!("check_integrity: {error:?}"))?;
    let recovered_identity = opened
        .call_id("power-loss")
        .map_err(|error| error.to_string())?;
    if identity.is_some_and(|identity| identity != recovered_identity) {
        return Err("acknowledged store identity lost after power loss".into());
    }
    let state = dump(store)?;
    let allowed = [acked.checked_sub(1), Some(acked)];
    let matches: Vec<usize> = (0..states.len()).filter(|k| states[*k] == state).collect();
    if !matches
        .iter()
        .any(|k| allowed.contains(&Some(*k)) || (acked == 0 && *k == 0))
    {
        let nearest = acked.saturating_sub(1).min(states.len() - 1);
        let differs: Vec<String> = state
            .iter()
            .filter(|(table, value)| states[nearest].get(*table) != Some(value))
            .map(|(table, value)| {
                format!(
                    "{table} {value} (expected {})",
                    states[nearest].get(table).map_or("-", String::as_str)
                )
            })
            .collect();
        return Err(format!(
            "state after {acked} acknowledged operation(s) is not the state after {} or {} (matches {matches:?}); differs in {differs:?}",
            acked.saturating_sub(1),
            acked
        ));
    }
    // New work is accepted and survives a clean reopen.
    let command = Command {
        scope: "operator".into(),
        id: "after-the-crash".into(),
        lineage: opened.lineage().map_err(|error| format!("{error:?}"))?,
        kind: "turn".into(),
        payload: b"more".to_vec(),
    };
    opened
        .admit(&command)
        .map_err(|error| format!("admit after the crash: {error:?}"))?;
    drop(opened);
    let mut again = Store::open(store).map_err(|error| format!("second open: {error:?}"))?;
    if again
        .call_id("power-loss")
        .map_err(|error| error.to_string())?
        != recovered_identity
    {
        return Err("store identity changed on second open".into());
    }
    match again.admitted("operator", "after-the-crash") {
        Ok(Some(payload)) if payload == b"more" => Ok(()),
        other => Err(format!("new work lost on reopen: {other:?}")),
    }
}

#[derive(Default)]
struct Sweep {
    events: u64,
    points: usize,
    images: usize,
    failures: Vec<String>,
}

fn images(point: u64) -> Vec<Image> {
    let mut out = vec![Image::Kept, Image::Lost];
    let random = if exhaustive() { 4 } else { 1 };
    for index in 0..random {
        out.push(Image::Random(point * 16 + index));
    }
    out
}

/// Crash points to check: all of them when exhaustive, else a reproducible sample that
/// always includes the first and last few.
fn crash_points(trace: &[String], sample: usize) -> Vec<u64> {
    let events = trace.len() as u64;
    // Reproduce reported cuts: SPINDLE_POWER_LOSS_POINTS=1914,1916
    if let Ok(list) = std::env::var("SPINDLE_POWER_LOSS_POINTS") {
        return list
            .split(',')
            .filter_map(|point| point.trim().parse().ok())
            .filter(|point| (1..=events).contains(point))
            .collect();
    }
    if exhaustive() || events as usize <= sample {
        return (1..=events).collect();
    }
    // Every flush and truncation is a boundary worth cutting at.
    let mut points: Vec<u64> = (1..=3).chain(events - 2..=events).collect();
    for (index, event) in trace.iter().enumerate() {
        let point = index as u64 + 1;
        if !event.starts_with("write") && !points.contains(&point) {
            points.push(point);
        }
    }
    let sample = sample.max(points.len() + sample / 4);
    let mut rng = Rng::new(seed() ^ events);
    while points.len() < sample {
        let point = 1 + rng.below(events);
        if !points.contains(&point) {
            points.push(point);
        }
    }
    points.sort_unstable();
    points
}

/// Runs `ops` under every chosen crash point and checks every image of each.
/// How a sweep runs: its starting files, a check of migration backups, and whether the
/// simulated drive honours flushes.
#[derive(Clone, Copy)]
struct Setup {
    seed_files: fn(&Path),
    backup: Option<fn() -> BTreeMap<String, String>>,
    lie: bool,
}

const PLAIN: Setup = Setup {
    seed_files: nothing,
    backup: None,
    lie: false,
};

fn sweep(label: &str, setup: Setup, ops: &[Op], sample: usize) -> Sweep {
    let seed_files = setup.seed_files;
    let started = Instant::now();
    let reference = Sim::start(label, Plan::Run, seed_files);
    reference.trace();
    let mut ctx = Ctx::new(reference.root().join("store"));
    let mut states = vec![];
    for op in ops {
        ctx.run(op)
            .unwrap_or_else(|error| panic!("reference run, {op:?}: {error:?}"));
        states.push(dump(&ctx.dir).unwrap());
    }
    drop(ctx);
    let events = reference.events();
    let trace = reference.traced();
    if std::env::var_os("SPINDLE_POWER_LOSS_TRACE").is_some() {
        for (index, event) in trace.iter().enumerate() {
            eprintln!("{label} {}: {event}", index + 1);
        }
    }
    drop(reference);
    let mut sweep = Sweep {
        events,
        ..Sweep::default()
    };
    for point in crash_points(&trace, sample) {
        let sim = Sim::start(label, Plan::CrashAt(point), seed_files);
        if setup.lie {
            sim.lie();
        }
        let mut ctx = Ctx::new(sim.root().join("store"));
        let mut acked = 0;
        for op in ops {
            if ctx.run(op).is_err() {
                break;
            }
            acked += 1;
        }
        let Some(snapshot) = sim.snapshot() else {
            panic!(
                "{label}: no crash at event {point}: the run made {} of the reference run's {events} events (acked {acked}); reference trace tail {:?}",
                sim.events(),
                &trace[trace.len().saturating_sub(5)..]
            );
        };
        let identity = ctx.identity.clone();
        drop(ctx);
        drop(sim);
        sweep.points += 1;
        for image in images(point) {
            let out = scratch("image");
            snapshot.write_image(image, &out.join("root"));
            sweep.images += 1;
            let checked = match setup.backup {
                Some(expected) => check_backup(&out.join("root/store"), &expected(), || {
                    check_image(&out.join("root/store"), acked, &states, identity.as_deref())
                }),
                None => check_image(&out.join("root/store"), acked, &states, identity.as_deref()),
            };
            if let Err(error) = checked {
                sweep.failures.push(format!(
                    "{label}: crash at event {point}/{events} ({}) during {:?}, {image:?} image: {error}",
                    trace[point as usize - 1],
                    ops.get(acked)
                ));
            }
            keep_or_remove(&out);
        }
    }
    eprintln!(
        "{label}: {} events, {} crash points, {} images, {} failures, {:.1}s",
        sweep.events,
        sweep.points,
        sweep.images,
        sweep.failures.len(),
        started.elapsed().as_secs_f64()
    );
    sweep
}

/// Removes a checked image, or keeps it for inspection with SPINDLE_POWER_LOSS_KEEP.
fn keep_or_remove(image: &Path) {
    if std::env::var_os("SPINDLE_POWER_LOSS_KEEP").is_some() {
        eprintln!("kept {}", image.display());
    } else {
        let _ = std::fs::remove_dir_all(image);
    }
}

fn assert_clean(sweep: Sweep) {
    assert!(
        sweep.failures.is_empty(),
        "{} of {} images failed:\n{}",
        sweep.failures.len(),
        sweep.images,
        sweep.failures[..sweep.failures.len().min(40)].join("\n")
    );
}

fn nothing(_: &Path) {}

#[test]
fn store_operations_survive_power_loss_at_every_sampled_point() {
    let ops = [
        Op::Open,
        Op::Metadata(1),
        Op::Admit(1, 300),
        Op::Propose(1),
        Op::Reserve(1),
        Op::Receipt(1),
        Op::Adopt(1),
        Op::Admit(2, 96 * 1024),
        Op::Propose(2),
        Op::Reserve(2),
        Op::Receipt(2),
        Op::Adopt(2),
        Op::Metadata(2),
        Op::Reopen,
        Op::BeginStage(3),
        Op::Put(3, 0),
        Op::Put(3, 1),
        Op::Put(3, 2),
        Op::MapStage(3),
        Op::Admit(3, 40),
        Op::Publish(3),
        Op::Gc,
        Op::Admit(4, 4096),
        Op::Propose(4),
        Op::Reserve(4),
        Op::Reject(4),
        Op::Reopen,
        Op::Close,
    ];
    assert_clean(sweep("store", PLAIN, &ops, 40));
}

#[test]
fn creating_a_store_survives_power_loss_at_every_point() {
    let ops = [Op::Open, Op::Admit(1, 64), Op::Close];
    assert_clean(sweep("create", PLAIN, &ops, usize::MAX));
}

/// Negative control: the same checks catch a drive that acknowledges flushes it never
/// makes (acknowledged work is missing from the images that keep only flushed data).
#[test]
fn a_drive_that_ignores_flushes_is_caught() {
    let ops = [
        Op::Open,
        Op::Admit(1, 64),
        Op::Admit(2, 64),
        Op::Admit(3, 64),
        Op::Close,
    ];
    let setup = Setup { lie: true, ..PLAIN };
    let sweep = sweep("lying", setup, &ops, 12);
    assert!(
        sweep
            .failures
            .iter()
            .any(|failure| failure.contains("Lost image")),
        "lost flushes went unnoticed"
    );
    assert!(
        crash_vfs::sqlite_fsyncs() > 0,
        "SQLite's own fsync calls are observed"
    );
}

/// A schema-15 store (the schema-14 fixture with every large value in the schema-15
/// encoding), as the previous release left it.
#[cfg(feature = "dext-turn")]
fn schema15() -> &'static [u8] {
    static BYTES: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    BYTES.get_or_init(|| {
        let dir = scratch("schema15");
        let path = dir.join("authority.sqlite");
        let packed = include_bytes!("fixtures/schema14.sqlite.deflate");
        std::fs::write(
            &path,
            miniz_oxide::inflate::decompress_to_vec(packed).unwrap(),
        )
        .unwrap();
        let db =
            Connection::open_with_flags_and_vfs(&path, OpenFlags::SQLITE_OPEN_READ_WRITE, c"unix")
                .unwrap();
        for (table, key, column) in [
            ("admissions", "sequence", "payload"),
            ("revisions", "revision", "continuation"),
            ("proposals", "id", "canonical"),
            ("intents", "id", "input"),
            ("receipts", "attempt", "result"),
        ] {
            let rows: Vec<(rusqlite::types::Value, Vec<u8>)> = db
                .prepare(&format!("SELECT {key},{column} FROM {table}"))
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            for (id, value) in rows {
                db.execute(
                    &format!("UPDATE {table} SET {column}=?1 WHERE {key}=?2"),
                    rusqlite::params![spindle_store::codec::pack(&value), id],
                )
                .unwrap();
            }
        }
        db.pragma_update(None, "user_version", 15).unwrap();
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        drop(db);
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        bytes
    })
}

#[cfg(feature = "dext-turn")]
fn seed_schema15(root: &Path) {
    let store = root.join("store");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&store)
        .unwrap();
    std::fs::write(store.join("authority.sqlite"), schema15()).unwrap();
    std::fs::write(store.join("owner.lock"), b"").unwrap();
}

/// The logical content of the schema-15 store before migration.
#[cfg(feature = "dext-turn")]
fn schema15_state() -> BTreeMap<String, String> {
    static STATE: std::sync::OnceLock<BTreeMap<String, String>> = std::sync::OnceLock::new();
    STATE
        .get_or_init(|| {
            let dir = scratch("schema15-state");
            std::fs::write(dir.join("authority.sqlite"), schema15()).unwrap();
            let state = dump(&dir).unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            state
        })
        .clone()
}

/// Read from a disposable copy, writable so a hot journal can be rolled back.
fn user_version(path: &Path) -> Result<i64, String> {
    Connection::open_with_flags_and_vfs(path, OpenFlags::SQLITE_OPEN_READ_WRITE, c"unix")
        .and_then(|db| db.pragma_query_value(None, "user_version", |row| row.get(0)))
        .map_err(|error| error.to_string())
}

/// The logical content of a database file, read from a copy (reading must not change
/// the image the store opens afterwards).
fn state_of(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let copy = scratch("inspect");
    let state = std::fs::copy(path, copy.join("authority.sqlite"))
        .map_err(|error| error.to_string())
        .and_then(|_| dump(&copy));
    let _ = std::fs::remove_dir_all(&copy);
    state
}

/// Around `check`: once a migration has committed, the backup of the old store is
/// present and holds exactly the old store's records; after the image has been opened
/// (migrating it again if the crash came first) the backup is there as well.
fn check_backup(
    store: &Path,
    expected: &BTreeMap<String, String>,
    check: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let backup = store.join(spindle_store::backup_name(15));
    let copy = scratch("inspect");
    for entry in std::fs::read_dir(store).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        std::fs::copy(entry.path(), copy.join(entry.file_name()))
            .map_err(|error| error.to_string())?;
    }
    let migrated = user_version(&copy.join("authority.sqlite"));
    let _ = std::fs::remove_dir_all(&copy);
    if migrated? == i64::from(spindle_store::CAPABILITIES.schema)
        && state_of(&backup).as_ref() != Ok(expected)
    {
        return Err("the migration committed but its backup is missing or differs".into());
    }
    check()?;
    if state_of(&backup).as_ref() != Ok(expected) {
        return Err("after opening, the backup is missing or differs from the old store".into());
    }
    Ok(())
}

#[cfg(feature = "dext-turn")]
#[test]
fn migration_from_schema_15_survives_power_loss_and_keeps_its_backup() {
    let ops = [Op::Open, Op::Admit(1, 4096), Op::Close];
    let setup = Setup {
        seed_files: seed_schema15,
        backup: Some(schema15_state),
        lie: false,
    };
    assert_clean(sweep("migration", setup, &ops, 24));
}

/// Disk full (or a failing disk) at a byte budget: the step that hits it fails with an
/// error that says so, never anything later; the handle stops; once space returns a
/// reopened store passes the full check, holds every acknowledged step (the failed one
/// at most in full, never in part), and the rest of the work completes to exactly the
/// state of an undisturbed run.
fn exhaust(label: &str, ops: &[Op], code: std::ffi::c_int, budgets: usize) -> Vec<String> {
    let started = Instant::now();
    let reference = Sim::start(label, Plan::Run, nothing);
    let mut ctx = Ctx::new(reference.root().join("store"));
    let mut states = vec![];
    for op in ops {
        ctx.run(op).unwrap();
        states.push(dump(&ctx.dir).unwrap());
    }
    drop(ctx);
    let offsets = reference.write_offsets();
    drop(reference);
    let chosen: Vec<u64> = if exhaustive() {
        offsets.clone()
    } else {
        let mut rng = Rng::new(seed() ^ 0xf011);
        let mut chosen = vec![0, *offsets.last().unwrap()];
        while chosen.len() < budgets {
            chosen.push(offsets[rng.below(offsets.len() as u64) as usize]);
        }
        chosen
    };
    let mut failures = vec![];
    let mut ambiguous = 0;
    for budget in &chosen {
        let sim = Sim::start(
            label,
            Plan::Full {
                budget: *budget,
                code,
            },
            nothing,
        );
        let mut ctx = Ctx::new(sim.root().join("store"));
        let mut acked = 0;
        let mut failed = None;
        for op in ops {
            match ctx.run(op) {
                Ok(()) => acked += 1,
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            }
        }
        let Some(error) = failed else {
            continue;
        };
        let disk_full = code == rusqlite::ffi::SQLITE_FULL;
        let context = format!("{label}: budget {budget}, {:?} failed", ops[acked]);
        if error.is_disk_full() != disk_full || sim.faults() == 0 {
            failures.push(format!(
                "{context} with {error:?} (disk full expected: {disk_full})"
            ));
        }
        if let Some(store) = ctx.store.as_mut()
            && !matches!(store.lineage(), Err(spindle_store::Error::Paused))
        {
            failures.push(format!("{context}, and the handle was not fenced"));
        }
        // Space returns.
        sim.set_plan(Plan::Run);
        ctx.store = None;
        if let Err(error) = ctx.run(&Op::Open) {
            failures.push(format!("{context}; reopening then failed: {error:?}"));
            continue;
        }
        if let Err(error) = ctx.store().check_integrity() {
            failures.push(format!("{context}; the full check then failed: {error:?}"));
            continue;
        }
        let state = dump(&ctx.dir).unwrap();
        if state == states[acked] && (acked == 0 || states[acked - 1] != states[acked]) {
            // The failed step's commit record reached the disk before the failure (a WAL
            // padding frame after it hit the budget): ambiguous, as the store documents.
            ambiguous += 1;
        } else if acked == 0 || state != states[acked - 1] {
            failures.push(format!(
                "{context}; the reopened store is not the state before it"
            ));
            continue;
        }
        if let Err(error) = more_work(&mut ctx) {
            failures.push(format!("{context}; then new work failed: {error}"));
        }
    }
    eprintln!(
        "{label}: {} write budgets, {} failures, {ambiguous} failed steps found committed after reopen, {:.1}s",
        chosen.len(),
        failures.len(),
        started.elapsed().as_secs_f64()
    );
    failures
}

/// New steps after a fault: a large admission and a staged object, which must commit,
/// survive a clean reopen and pass the full check.
fn more_work(ctx: &mut Ctx) -> Result<(), String> {
    for op in [
        Op::Admit(90, 64 * 1024),
        Op::BeginStage(91),
        Op::Put(91, 0),
        Op::Close,
        Op::Open,
    ] {
        ctx.run(&op).map_err(|error| format!("{op:?}: {error:?}"))?;
    }
    let store = ctx.store();
    store
        .check_integrity()
        .map_err(|error| format!("{error:?}"))?;
    match store.admitted("operator", "command-90") {
        Ok(Some(payload)) if payload == noise(90, 64 * 1024) => Ok(()),
        other => Err(format!("the new admission is missing: {other:?}")),
    }
}

fn workload() -> Vec<Op> {
    vec![
        Op::Open,
        Op::Metadata(1),
        Op::Admit(1, 300),
        Op::Propose(1),
        Op::Reserve(1),
        Op::Receipt(1),
        Op::Adopt(1),
        Op::Admit(2, 96 * 1024),
        Op::Propose(2),
        Op::BeginStage(3),
        Op::Put(3, 0),
        Op::Put(3, 1),
        Op::Reserve(2),
        Op::Receipt(2),
        Op::Adopt(2),
        Op::Metadata(2),
        Op::MapStage(3),
        Op::Admit(3, 40),
        Op::Publish(3),
        Op::Gc,
        Op::Admit(4, 4096),
        Op::Propose(4),
        Op::Reserve(4),
        Op::Reject(4),
        Op::Close,
    ]
}

#[test]
fn a_full_disk_fails_the_step_cleanly_and_work_continues_once_space_returns() {
    let failures = exhaust("full", &workload(), rusqlite::ffi::SQLITE_FULL, 40);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_failing_disk_fails_the_step_and_work_continues_after_reopen() {
    let failures = exhaust("eio", &workload(), rusqlite::ffi::SQLITE_IOERR_WRITE, 16);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Whole sessions: `agent::Session` turns with tool calls, history compaction (a model
/// summary), usage accounting, a rewind and doctor's garbage collection, against an
/// in-process loopback provider that answers like an OpenAI-compatible server.
#[cfg(feature = "agent")]
mod session {
    use super::*;
    use serde_json::{Value, json};
    use spindle_store::agent::{self, Event, Isolation, Observer, Session, Settings};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::{Arc, OnceLock};

    static LISTENER: OnceLock<TcpListener> = OnceLock::new();

    /// Runs before `main`, while the process has one thread: the provider catalog
    /// (`DEXT_HOME`) names the fake provider's port, so the environment is set before
    /// any test thread could read it.
    #[used]
    #[unsafe(link_section = ".init_array")]
    static SETUP: extern "C" fn() = setup;

    extern "C" fn setup() {
        let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
            return;
        };
        let Ok(address) = listener.local_addr() else {
            return;
        };
        // Homes of earlier runs whose process is gone.
        for entry in std::fs::read_dir(std::env::temp_dir())
            .into_iter()
            .flatten()
            .flatten()
        {
            if let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix("spindle-power-home-"))
                && !Path::new("/proc").join(pid).exists()
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        let home = std::env::temp_dir().join(format!("spindle-power-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        if std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&home)
            .is_err()
        {
            return;
        }
        let catalog = json!({
            "version": 4,
            "active_provider": "fake",
            "providers": [{
                "id": "fake",
                "display_name": "Loopback fake",
                "api_provider": "openai",
                "request_contract": "openai-chat-completions",
                "base_url": format!("http://{address}"),
                "default_model": "fake-model",
                "models": ["fake-model"],
                "requires_api_key": false,
                "model_context_windows": {"fake-model": 12000},
            }],
        });
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(home.join("providers.json"))
            .and_then(|mut file| file.write_all(catalog.to_string().as_bytes()));
        if written.is_ok() {
            // SAFETY: before main; no other thread exists yet.
            unsafe { std::env::set_var("DEXT_HOME", &home) };
            let _ = LISTENER.set(listener);
        }
    }

    /// Starts the fake provider (once) and returns whether it is available.
    fn provider() -> bool {
        static STARTED: OnceLock<bool> = OnceLock::new();
        *STARTED.get_or_init(|| {
            let Some(listener) = LISTENER.get() else {
                return false;
            };
            let listener = listener.try_clone().unwrap();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    std::thread::spawn(move || answer(stream));
                }
            });
            true
        })
    }

    fn answer(mut stream: std::net::TcpStream) {
        let mut request = vec![];
        let mut chunk = [0; 65536];
        let body = loop {
            let Ok(read) = stream.read(&mut chunk) else {
                return;
            };
            if read == 0 {
                return;
            }
            request.extend_from_slice(&chunk[..read]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break request[end + 4..end + 4 + length].to_vec();
                }
            }
        };
        let reply = serde_json::from_slice::<Value>(&body)
            .map(|request| reply(&request))
            .unwrap_or_else(|_| text_stream("unreadable request"));
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
    }

    fn chunk(delta: Value, finish: Value) -> String {
        let usage = if finish.is_null() {
            Value::Null
        } else {
            json!({"prompt_tokens": 1000, "completion_tokens": 50})
        };
        format!(
            "data: {}\n\n",
            json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish}], "usage": usage})
        )
    }

    fn text_stream(text: &str) -> String {
        chunk(json!({"role": "assistant", "content": text}), Value::Null)
            + &chunk(json!({}), json!("stop"))
            + "data: [DONE]\n\n"
    }

    fn tool_stream(id: &str, name: &str, input: Value) -> String {
        chunk(
            json!({"role": "assistant", "tool_calls": [{"index": 0, "id": id, "type": "function",
                "function": {"name": name, "arguments": input.to_string()}}]}),
            Value::Null,
        ) + &chunk(json!({}), json!("tool_calls"))
            + "data: [DONE]\n\n"
    }

    /// A note large enough that a few turns overflow the small context window.
    fn note(turn: u64) -> String {
        (0..400)
            .map(|line| format!("note {turn} line {line}: the store keeps every step.\n"))
            .collect()
    }

    /// Decided from the request alone, so a resent request gets the same answer: each
    /// `TURN-n` prompt writes a note, counts it with bash, then answers.
    fn reply(request: &Value) -> String {
        let messages = request["messages"].as_array().cloned().unwrap_or_default();
        if messages
            .first()
            .is_some_and(|m| m.to_string().contains("You summarize the earlier part"))
        {
            return text_stream("Summary: earlier turns wrote notes and counted them.");
        }
        let Some(prompt) = messages
            .iter()
            .rposition(|m| m["role"] == "user" && m.to_string().contains("TURN-"))
        else {
            return text_stream("no turn found");
        };
        let text = messages[prompt].to_string();
        let turn: u64 = text
            .rsplit("TURN-")
            .next()
            .and_then(|rest| {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().ok()
            })
            .unwrap_or(0);
        let tools = messages[prompt..]
            .iter()
            .filter(|m| m["role"] == "tool")
            .count();
        match tools {
            0 => tool_stream(
                &format!("call-{turn}-0"),
                "write_file",
                json!({"path": format!("notes/turn-{turn}.txt"), "content": note(turn)}),
            ),
            1 => tool_stream(
                &format!("call-{turn}-1"),
                "bash",
                json!({"command": format!("wc -c notes/*.txt && head -c 6000 notes/turn-{turn}.txt")}),
            ),
            _ => text_stream(&format!("done with turn {turn}")),
        }
    }

    struct Quiet;
    impl Observer for Quiet {
        fn event(&self, _: Event<'_>) {}
    }

    #[derive(Clone, Copy, Debug)]
    enum Step {
        Create,
        Prompt(u64),
        /// An app state change between turns: a step of its own.
        State(u64),
        Usage,
        Rewind,
        Doctor,
        Close,
    }

    struct Run {
        dir: PathBuf,
        session: Option<Session>,
    }

    fn settings() -> Settings {
        let mut settings = Settings::resolve("fake", Some("fake-model".into())).unwrap();
        settings.isolation = Isolation::Off;
        settings
    }

    impl Run {
        fn session(&mut self) -> Result<&mut Session, String> {
            if self.session.is_none() {
                self.session = Some(Session::open(&self.dir, Arc::new(Quiet))?);
            }
            Ok(self.session.as_mut().unwrap())
        }

        fn step(&mut self, step: Step) -> Result<(), String> {
            match step {
                Step::Create => {
                    let files = vec![("README.md".to_string(), b"# notes\n".to_vec(), false)];
                    self.session = Some(Session::create(
                        &self.dir,
                        settings(),
                        files,
                        Arc::new(Quiet),
                    )?);
                }
                Step::Prompt(turn) => {
                    self.session()?
                        .prompt(&format!("TURN-{turn}: write a note and count it"))?;
                }
                Step::State(n) => {
                    self.session()?
                        .set_state(&serde_json::json!({"step": n, "note": format!("after {n}")}))?;
                }
                Step::Usage => {
                    agent::usage(&self.dir)?;
                }
                Step::Rewind => {
                    let session = self.session()?;
                    let targets = session.targets()?;
                    let target = targets
                        .get(targets.len() / 2)
                        .ok_or("no rewind target")?
                        .revision;
                    session.rewind(target)?;
                }
                Step::Doctor => {
                    self.session = None;
                    let failed: Vec<String> = agent::doctor(&self.dir)
                        .into_iter()
                        .filter(|finding| !finding.ok)
                        .map(|finding| format!("{}: {}", finding.check, finding.detail))
                        .collect();
                    if !failed.is_empty() {
                        return Err(failed.join("; "));
                    }
                }
                Step::Close => self.session = None,
            }
            Ok(())
        }
    }

    /// Revision rows (number, scope, engine, stored continuation digest, workspace).
    fn revisions(dir: &Path) -> Result<Vec<String>, String> {
        let db = Connection::open_with_flags_and_vfs(
            dir.join("store/authority.sqlite"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
            c"unix",
        )
        .map_err(|error| error.to_string())?;
        let mut statement = db
            .prepare("SELECT r.revision,r.scope,r.engine,r.continuation,r.workspace,s.value FROM revisions r LEFT JOIN app_state s ON s.revision=r.revision ORDER BY r.revision")
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                let continuation: Vec<u8> = row.get(3)?;
                let digest: String = Sha256::digest(&continuation)[..8]
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                // The app state a revision set is part of it: never one without the other.
                let state: Option<Vec<u8>> = row.get(5)?;
                let state = state.map(|value| {
                    Sha256::digest(&value)[..8]
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                });
                Ok(format!(
                    "{} {} {} {digest} {:?} {state:?}",
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(4)?
                ))
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<rusqlite::Result<_>>()
            .map_err(|error| error.to_string())
    }

    /// Billed compaction summaries the session's spend report counts. Each summary is
    /// journaled in the store, so the count survives a lost or torn usage.json.
    fn summaries(dir: &Path) -> u64 {
        spindle_store::agent::usage_report(dir).map_or(0, |report| report.compaction.calls)
    }

    struct Reference {
        /// Revision rows after the whole workload; rows are only ever appended.
        rows: Vec<String>,
        /// Revisions present after each step.
        heads: Vec<usize>,
        summaries: Vec<u64>,
    }

    fn check_session(
        dir: &Path,
        acked: usize,
        steps: &[Step],
        reference: &Reference,
    ) -> Result<(), String> {
        if acked == 0 && !dir.join("session.json").exists() {
            // The session was never created; creating it now must work.
            Run {
                dir: dir.into(),
                session: None,
            }
            .step(Step::Create)?;
        } else {
            let failed: Vec<String> = agent::doctor(dir)
                .into_iter()
                .filter(|finding| !finding.ok)
                .map(|finding| format!("{}: {}", finding.check, finding.detail))
                .collect();
            if !failed.is_empty() {
                return Err(format!("doctor: {}", failed.join("; ")));
            }
        }
        let rows = revisions(dir)?;
        let at_least = acked.checked_sub(1).map_or(0, |last| reference.heads[last]);
        let at_most = reference.heads[acked.min(steps.len() - 1)];
        if rows.len() < at_least
            || rows.len() > at_most.max(at_least)
            || rows[..] != reference.rows[..rows.len()]
        {
            return Err(format!(
                "{} revisions, expected the first {at_least}..={at_most} of the undisturbed run",
                rows.len()
            ));
        }
        let wanted = acked
            .checked_sub(1)
            .map_or(0, |last| reference.summaries[last]);
        if summaries(dir) < wanted {
            return Err(format!(
                "the spend report lost compaction summaries: {} < {wanted}",
                summaries(dir)
            ));
        }
        continue_and_prompt(dir, "TURN-99: after the crash")
    }

    /// What a user does after an interruption: `continue`, or `retry` when continuing
    /// is refused, then a new prompt, which must finish with no unresolved step.
    fn continue_and_prompt(dir: &Path, prompt: &str) -> Result<(), String> {
        let mut session = Session::open(dir, Arc::new(Quiet))?;
        let accepts = session.accepts_prompt();
        if std::env::var_os("SPINDLE_POWER_LOSS_KEEP").is_some() {
            eprintln!("{}: accepts a prompt: {accepts}", dir.display());
        }
        if !accepts && let Err(error) = session.resume() {
            drop(session);
            Session::open(dir, Arc::new(Quiet))?
                .retry()
                .map_err(|retry| format!("resume: {error}; retry: {retry}"))?;
            session = Session::open(dir, Arc::new(Quiet))?;
        }
        session
            .prompt(prompt)
            .map_err(|error| format!("new prompt: {error}"))?;
        let status = session.status()?;
        if status.unresolved != 0 {
            return Err(format!(
                "{} unresolved steps after new work",
                status.unresolved
            ));
        }
        Ok(())
    }

    fn sweep_session(label: &str, steps: &[Step], sample: usize) -> Sweep {
        let started = Instant::now();
        assert!(provider(), "the loopback provider could not start");
        let reference = Sim::start(label, Plan::Run, nothing);
        reference.trace();
        let mut run = Run {
            dir: reference.root().join("session"),
            session: None,
        };
        let mut heads = vec![];
        let mut sums = vec![];
        for step in steps {
            run.step(*step)
                .unwrap_or_else(|error| panic!("reference run, {step:?}: {error}"));
            heads.push(revisions(&run.dir).map_or(0, |rows| rows.len()));
            sums.push(summaries(&run.dir));
        }
        let rows = revisions(&run.dir).unwrap();
        drop(run);
        let events = reference.events();
        let trace = reference.traced();
        drop(reference);
        assert!(
            sums.last().is_some_and(|calls| *calls > 0),
            "the workload compacts with a model summary"
        );
        let reference = Reference {
            rows,
            heads,
            summaries: sums,
        };
        let mut sweep = Sweep {
            events,
            ..Sweep::default()
        };
        // A whole session is too long to cut at every event (hours): a seeded sample,
        // larger when exhaustive.
        let mut points = crash_points(&trace, sample);
        let mut rng = Rng::new(seed() ^ 0x5e55);
        while points.len() > sample {
            points.remove(rng.below(points.len() as u64) as usize);
        }
        for point in points {
            let sim = Sim::start(label, Plan::CrashAt(point), nothing);
            let mut run = Run {
                dir: sim.root().join("session"),
                session: None,
            };
            let mut acked = 0;
            for step in steps {
                let outcome =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run.step(*step)));
                match outcome {
                    Ok(Ok(())) => acked += 1,
                    Ok(Err(_)) => break,
                    Err(_) => {
                        sweep.failures.push(format!(
                            "{label}: {step:?} panicked after power was cut at {point}"
                        ));
                        break;
                    }
                }
            }
            // Tool runs make the event count vary by a few between runs: a point past
            // this run's end is skipped.
            let Some(snapshot) = sim.snapshot() else {
                assert!(
                    point + 64 > events,
                    "{label}: no crash at event {point} of {events}"
                );
                continue;
            };
            drop(run);
            drop(sim);
            sweep.points += 1;
            for image in [Image::Lost, Image::Random(point)] {
                let out = scratch("session-image");
                snapshot.write_image(image, &out.join("root"));
                sweep.images += 1;
                if let Err(error) =
                    check_session(&out.join("root/session"), acked, steps, &reference)
                {
                    sweep.failures.push(format!(
                        "{label}: crash at event {point}/{events} ({}) during {:?}, {image:?} image: {error}",
                        trace[point as usize - 1],
                        steps.get(acked)
                    ));
                }
                keep_or_remove(&out);
            }
        }
        eprintln!(
            "{label}: {} events, {} crash points, {} images, {} failures, {:.1}s",
            sweep.events,
            sweep.points,
            sweep.images,
            sweep.failures.len(),
            started.elapsed().as_secs_f64()
        );
        sweep
    }

    const STEPS: [Step; 11] = [
        Step::Create,
        Step::Prompt(1),
        Step::Prompt(2),
        Step::State(1),
        Step::Usage,
        Step::Prompt(3),
        Step::State(2),
        Step::Rewind,
        Step::Prompt(4),
        Step::Doctor,
        Step::Close,
    ];

    /// A full disk at sampled points of two turns: the call that hits it says the disk
    /// is full (no panic, no misleading text), and once space returns the session
    /// continues and takes new prompts.
    #[test]
    fn a_full_disk_mid_turn_says_so_and_the_session_continues_once_space_returns() {
        assert!(provider());
        let started = Instant::now();
        let steps = [Step::Create, Step::Prompt(1), Step::Prompt(2), Step::Close];
        let reference = Sim::start("session-full", Plan::Run, nothing);
        let mut run = Run {
            dir: reference.root().join("session"),
            session: None,
        };
        for step in steps {
            run.step(step).unwrap();
        }
        drop(run);
        let offsets = reference.write_offsets();
        drop(reference);
        let count = if exhaustive() { 48 } else { 6 };
        let mut rng = Rng::new(seed() ^ 0xd15c);
        let mut failures = vec![];
        let mut hit = 0;
        for _ in 0..count {
            let budget = offsets[rng.below(offsets.len() as u64) as usize];
            let code = rusqlite::ffi::SQLITE_FULL;
            let sim = Sim::start("session-full", Plan::Full { budget, code }, nothing);
            let dir = sim.root().join("session");
            let mut run = Run {
                dir: dir.clone(),
                session: None,
            };
            let mut message = None;
            for step in steps {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run.step(step))) {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        message = Some(format!("{step:?}: {error}"));
                        break;
                    }
                    Err(_) => {
                        failures.push(format!("budget {budget}: {step:?} panicked"));
                        break;
                    }
                }
            }
            drop(run);
            let Some(message) = message else {
                continue;
            };
            hit += 1;
            if !message.contains("disk is full") || message.contains("SqliteFailure") {
                failures.push(format!("budget {budget}: {message}"));
            }
            sim.set_plan(Plan::Run);
            if let Err(error) = continue_and_prompt(&dir, "TURN-98: space is back") {
                failures.push(format!("budget {budget} ({message}); then {error}"));
            }
        }
        eprintln!(
            "session-full: {count} budgets, {hit} hit, {} failures, {:.1}s",
            failures.len(),
            started.elapsed().as_secs_f64()
        );
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// Regression: a session whose creator stopped before the first turn committed has
    /// no workspace yet; its next prompt must still be able to run tools.
    #[test]
    fn a_session_whose_first_turn_never_started_still_runs_tools() {
        assert!(provider());
        let dir = scratch("first-turn");
        let session = dir.join("session");
        drop(Session::create(&session, settings(), vec![], Arc::new(Quiet)).unwrap());
        continue_and_prompt(&session, "TURN-5: the first turn").unwrap();
        let mut reopened = Session::open(&session, Arc::new(Quiet)).unwrap();
        assert!(reopened.read("notes/turn-5.txt").is_ok(), "write_file ran");
        drop(reopened);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: power cut right after a prompt was admitted but before its turn
    /// started; a different prompt afterwards must run (it used to be refused with
    /// Conflict forever), and so must resending the same prompt.
    #[test]
    fn a_prompt_admitted_but_never_started_does_not_block_the_next() {
        assert!(provider());
        // (turn cut, transactions of it that committed, prompt sent afterwards): after
        // its admission, or after its workspace stage also began (revision 0 imports).
        for (turn, commits, next) in [
            (1, 1, "TURN-7: something else"),
            (1, 2, "TURN-1: write a note and count it"),
            (2, 1, "TURN-2: write a note and count it"),
        ] {
            let steps = [Step::Create, Step::Prompt(1), Step::Prompt(2)];
            let out = scratch("admitted-image");
            let session = cut_after(&steps[..turn + 1], turn, commits, &out);
            let recovery = spindle_store::api::recovery(&session).unwrap();
            assert!(
                recovery["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["kind"] == "prompt"),
                "{recovery}"
            );
            continue_and_prompt(&session, next).unwrap();
            let mut reopened = Session::open(&session, Arc::new(Quiet)).unwrap();
            assert!(
                reopened.history().unwrap().iter().any(|message| {
                    serde_json::to_string(message).is_ok_and(|text| text.contains(next))
                }),
                "{next} ran"
            );
            drop(reopened);
            let _ = std::fs::remove_dir_all(&out);
        }
    }

    /// Regression: power cut after a rewind began (and after it also published its
    /// target, still paused); the store refused every prompt until something resumed
    /// the rewind. Opening the session now finishes it.
    #[test]
    fn an_interrupted_rewind_is_finished_when_the_session_opens() {
        assert!(provider());
        let steps = [Step::Create, Step::Prompt(1), Step::Prompt(2), Step::Rewind];
        for commits in [1, 2] {
            let out = scratch("rewind-image");
            let session = cut_after(&steps, 3, commits, &out);
            let mut reopened = Session::open(&session, Arc::new(Quiet)).unwrap();
            assert_eq!(
                reopened.status().unwrap().prompts,
                1,
                "rewound to the first turn"
            );
            drop(reopened);
            continue_and_prompt(&session, "TURN-8: after the rewind").unwrap();
            let _ = std::fs::remove_dir_all(&out);
        }
    }

    /// Runs `steps` with power cut just after the `commits`-th store commit made by
    /// step `index`, and returns the session directory of the image that keeps only
    /// what was flushed (written under `out`).
    fn cut_after(steps: &[Step], index: usize, commits: usize, out: &Path) -> PathBuf {
        let reference = Sim::start("cut", Plan::Run, nothing);
        reference.trace();
        let mut run = Run {
            dir: reference.root().join("session"),
            session: None,
        };
        let mut before = 0;
        for (at, step) in steps.iter().enumerate() {
            if at == index {
                before = reference.events();
            }
            run.step(*step).unwrap();
        }
        drop(run);
        let trace = reference.traced();
        drop(reference);
        let committed = (before as usize..trace.len())
            .filter(|at| trace[*at].starts_with("sync session/store/authority.sqlite-wal"))
            .nth(commits - 1)
            .unwrap() as u64
            + 1;
        let sim = Sim::start("cut", Plan::CrashAt(committed + 1), nothing);
        let mut run = Run {
            dir: sim.root().join("session"),
            session: None,
        };
        for step in steps {
            if run.step(*step).is_err() {
                break;
            }
        }
        let snapshot = sim.snapshot().unwrap();
        drop(run);
        drop(sim);
        snapshot.write_image(Image::Lost, &out.join("root"));
        out.join("root/session")
    }

    /// One `spindle` CLI run; a file size limit (bytes) makes the kernel refuse writes
    /// past it with EFBIG (SIGXFSZ ignored), as a full disk refuses them with ENOSPC.
    #[cfg(feature = "cli")]
    fn cli(dir: &Path, args: &[&str], limit: Option<u64>) -> (Option<i32>, String) {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_spindle"));
        command
            .arg("--dir")
            .arg(dir)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(limit) = limit {
            // SAFETY: only async-signal-safe calls between fork and exec.
            unsafe {
                command.pre_exec(move || {
                    let bound = libc::rlimit {
                        rlim_cur: limit,
                        rlim_max: limit,
                    };
                    if libc::setrlimit(libc::RLIMIT_FSIZE, &bound) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
                    Ok(())
                });
            }
        }
        let mut child = command.spawn().unwrap();
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if started.elapsed() > std::time::Duration::from_secs(120) {
                let _ = child.kill();
                break child.wait().unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let mut stderr = String::new();
        let _ = child.stderr.take().unwrap().read_to_string(&mut stderr);
        (status.code(), stderr)
    }

    /// A real `spindle` process whose writes start failing (RLIMIT_FSIZE, EFBIG) at a
    /// range of sizes during a turn: it must end with a message, not a panic, signal or
    /// hang, and the next unlimited `continue`, prompt and `doctor` must succeed.
    #[cfg(feature = "cli")]
    #[test]
    fn a_cli_run_whose_writes_fail_ends_cleanly_and_the_session_continues() {
        assert!(provider());
        let root = scratch("fsize");
        let mut outcomes = vec![];
        let mut failures = vec![];
        for (index, limit) in [
            4096u64,
            16 << 10,
            48 << 10,
            96 << 10,
            160 << 10,
            256 << 10,
            400 << 10,
        ]
        .into_iter()
        .enumerate()
        {
            let dir = root.join(format!("s{index}"));
            let new = cli(
                &dir,
                &[
                    "new",
                    "--provider",
                    "fake",
                    "--model",
                    "fake-model",
                    "--isolation",
                    "off",
                    "TURN-1: write a note and count it",
                ],
                None,
            );
            assert_eq!(new.0, Some(0), "{}", new.1);
            let (code, stderr) = cli(
                &dir,
                &["say", "TURN-2: write a note and count it"],
                Some(limit),
            );
            let last = stderr.lines().last().unwrap_or_default().to_string();
            outcomes.push(format!("limit {limit}: exit {code:?}: {last}"));
            if code.is_none()
                || stderr.contains("panicked")
                || (code != Some(0) && !last.starts_with("spindle: "))
            {
                failures.push(format!("limit {limit}: exit {code:?}, stderr:\n{stderr}"));
            }
            if code != Some(0)
                && (last.contains("(Sql(") || last.contains("(Io(") || last.contains("(Paused)"))
            {
                failures.push(format!(
                    "limit {limit}: an internal error reached the user: {last}"
                ));
            }
            // A failure after the model request was sent leaves it unknown (it may have
            // been billed): the message says to retry, which is never done silently.
            let resume = if last.contains("run `spindle retry`") {
                "retry"
            } else {
                "continue"
            };
            for args in [
                &[resume][..],
                &["say", "TURN-3: write a note and count it"][..],
                &["doctor"][..],
            ] {
                let (code, stderr) = cli(&dir, args, None);
                if code != Some(0) {
                    failures.push(format!(
                        "limit {limit}, then {args:?}: exit {code:?}\n{stderr}"
                    ));
                    break;
                }
            }
        }
        eprintln!("{}", outcomes.join("\n"));
        let _ = std::fs::remove_dir_all(&root);
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn sessions_survive_power_loss_at_sampled_points() {
        let sample = if exhaustive() { 240 } else { 8 };
        assert_clean(sweep_session("session", &STEPS, sample));
    }
}
