use rusqlite::Connection;
use spindle_store::completion::Attempt;
use spindle_store::transition::{EffectIntent, Observation, Proposal};
use spindle_store::{
    Command, ConnectionProfile, Error, Initialization, JournalMode, OwnerExclusion, Store,
};

struct HostOwnsDatabase;
impl OwnerExclusion for HostOwnsDatabase {}

fn exercise(store: &mut Store) -> spindle_store::Result<()> {
    let command = Command {
        scope: "operator".into(),
        id: "probe-input".into(),
        lineage: 0,
        kind: "probe".into(),
        payload: b"retained input".to_vec(),
    };
    let admission = store.admit(&command)?;
    assert_eq!(store.admit(&command)?, admission);
    let proposal = Proposal {
        id: "probe-commit".into(),
        scope: "operator".into(),
        observation: Observation::Admission(admission.sequence),
        expected_revision: 0,
        owner_epoch: store.epoch(),
        engine: "store-probe".into(),
        continuation: b"waiting".to_vec(),
        workspace: None,
        effects: vec![EffectIntent {
            id: "probe-effect".into(),
            adapter: "probe-host".into(),
            input: b"inert".to_vec(),
        }],
        state: None,
    };
    assert_eq!(store.commit_proposal(&proposal)?, 1);
    store.reserve_attempt(&Attempt {
        id: "probe-attempt".into(),
        effect: "probe-effect".into(),
        owner_epoch: store.epoch(),
        implementation: "probe-host-v1".into(),
        grant_reference: "probe-only".into(),
    })?;
    assert!(store.receipt("probe-attempt")?.is_none());
    store.check_integrity()
}

/// Creates a persistent store with `journal`, refuses Open-before-Create and
/// Create-over-history, reopens it with the same identity and an unknown attempt left
/// unknown, then settles that attempt once. Returns its identity.
fn persistent(
    path: &str,
    journal: JournalMode,
    other: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let profile = |initialization| ConnectionProfile::Persistent {
        journal,
        initialization,
    };
    assert!(
        matches!(
            Store::from_connection(
                Connection::open(path)?,
                profile(Initialization::Open),
                HostOwnsDatabase
            ),
            Err(Error::Incompatible)
        ),
        "opening an empty store must never silently initialize it"
    );
    let mut store = Store::from_connection(
        Connection::open(path)?,
        profile(Initialization::Create),
        HostOwnsDatabase,
    )?;
    exercise(&mut store)?;
    let epoch = store.epoch();
    let identity = store.call_id("probe-effect")?;
    assert_ne!(
        identity, other,
        "each store draws its own incarnation identity"
    );
    drop(store);
    assert!(
        matches!(
            Store::from_connection(
                Connection::open(path)?,
                profile(Initialization::Create),
                HostOwnsDatabase
            ),
            Err(Error::Incompatible)
        ),
        "creating over existing history must be refused"
    );
    let mut store = Store::from_connection(
        Connection::open(path)?,
        profile(Initialization::Open),
        HostOwnsDatabase,
    )?;
    assert_eq!(store.epoch(), epoch + 1);
    assert_eq!(store.call_id("probe-effect")?, identity);
    assert_eq!(store.snapshot()?.continuation, b"waiting");
    assert!(
        store.receipt("probe-attempt")?.is_none(),
        "reopen cannot invent an unknown result"
    );
    store.ingest_receipt("probe-attempt", b"saved reply")?;
    store.ingest_receipt("probe-attempt", b"saved reply")?;
    assert!(store.ingest_receipt("probe-attempt", b"different").is_err());
    store.check_integrity()?;
    Ok(identity)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(delete), Some(wal)) = (args.next(), args.next()) else {
        return Err("usage: store-probe NEW_DELETE_DATABASE NEW_WAL_DATABASE".into());
    };
    for path in [&delete, &wal] {
        if std::path::Path::new(path).exists() {
            return Err("probe requires absent databases".into());
        }
    }
    #[cfg(target_os = "wasi")]
    assert!(matches!(
        Store::open(std::path::Path::new("/probe/native-store")),
        Err(Error::Unsupported)
    ));
    let mut memory = Store::from_connection(
        Connection::open_in_memory()?,
        ConnectionProfile::Memory,
        HostOwnsDatabase,
    )?;
    exercise(&mut memory)?;
    let memory_identity = memory.call_id("probe-effect")?;
    let delete_identity = persistent(&delete, JournalMode::Delete, &memory_identity)?;
    persistent(&wal, JournalMode::Wal, &delete_identity)?;
    // A store written in WAL reopens under the rollback journal, and back.
    for journal in [JournalMode::Delete, JournalMode::Wal] {
        let mut store = Store::from_connection(
            Connection::open(&wal)?,
            ConnectionProfile::Persistent {
                journal,
                initialization: Initialization::Open,
            },
            HostOwnsDatabase,
        )?;
        assert_eq!(
            store.receipt("probe-attempt")?.as_deref(),
            Some(&b"saved reply"[..])
        );
        store.check_integrity()?;
    }
    println!(
        "store-probe passed: memory, DELETE and WAL create/open refusal, admit/reuse, commit, unknown/reopen, identity, receipt/conflict, journal switch; pid={}",
        spindle_store::process_id()
    );
    Ok(())
}
