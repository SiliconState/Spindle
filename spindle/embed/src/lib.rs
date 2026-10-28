//! Spindle inside a JS/TS app: the embedded host of
//! `spindle-store` (`embedded`) built for wasm32-unknown-unknown, with storage for
//! Node (`node`: a VFS over `node:fs`) and browsers (the same VFS over the OPFS
//! adapter in `clients/js/opfs.mjs`), where sessions live (`places`), and the
//! wasm-bindgen surface the JavaScript `EmbeddedClient` drives (`js`). The store probe below is
//! the runtime check the package stands on: authorities opened through
//! `Store::from_connection` behind guards the host owns.
use rusqlite::Connection;
use serde_json::{Value, json};
use spindle_store::completion::Attempt;
use spindle_store::transition::{EffectIntent, Observation, Proposal};
use spindle_store::{Command, ConnectionProfile, Error, OwnerExclusion, Result, Store};

#[cfg(target_family = "wasm")]
mod js;
#[cfg(target_family = "wasm")]
pub mod node;
pub mod places;

/// A fresh in-memory authority. The guard is dropped only after SQLite closes, when
/// the store drops or when opening fails.
pub fn memory_store(guard: impl OwnerExclusion + 'static) -> Result<Store> {
    Store::from_connection(
        Connection::open_in_memory()?,
        ConnectionProfile::Memory,
        guard,
    )
}

const PROBE_EFFECT: &str = "probe-effect";
const PROBE_ATTEMPT: &str = "probe-attempt";

/// Admits the probe input (re-admission must reuse it), commits a step with one
/// effect and reserves an attempt for it: the state a crash between reservation and
/// receipt leaves behind.
pub fn reserve(store: &mut Store) -> Result<Value> {
    let command = Command {
        scope: "operator".into(),
        id: "probe-input".into(),
        lineage: 0,
        kind: "probe".into(),
        payload: b"retained input".to_vec(),
    };
    let admission = store.admit(&command)?;
    if store.admit(&command)? != admission {
        return Err(Error::Conflict);
    }
    let revision = store.commit_proposal(&Proposal {
        id: "probe-commit".into(),
        scope: "operator".into(),
        observation: Observation::Admission(admission.sequence),
        expected_revision: 0,
        owner_epoch: store.epoch(),
        engine: "embed-probe".into(),
        continuation: b"waiting".to_vec(),
        workspace: None,
        effects: vec![EffectIntent {
            id: PROBE_EFFECT.into(),
            adapter: "probe-host".into(),
            input: b"inert".to_vec(),
        }],
        state: None,
    })?;
    store.reserve_attempt(&Attempt {
        id: PROBE_ATTEMPT.into(),
        effect: PROBE_EFFECT.into(),
        owner_epoch: store.epoch(),
        implementation: "probe-host-v1".into(),
        grant_reference: "probe-only".into(),
    })?;
    Ok(json!({
        "revision": revision,
        "epoch": store.epoch(),
        "identity": store.call_id(PROBE_EFFECT)?,
    }))
}

/// What the store holds for the probe: its head, owner epoch, call identity, whether
/// the attempt is reserved and its receipt (None: the outcome is unknown).
pub fn report(store: &mut Store) -> Result<Value> {
    let revision = store.snapshot()?.revision;
    let reserved = revision > 0 && store.attempt_for(PROBE_EFFECT)?.is_some();
    let receipt = match reserved {
        true => store
            .receipt(PROBE_ATTEMPT)?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
        false => None,
    };
    store.check_integrity()?;
    Ok(json!({
        "revision": revision,
        "epoch": store.epoch(),
        "identity": if revision > 0 { json!(store.call_id(PROBE_EFFECT)?) } else { Value::Null },
        "reserved": reserved,
        "receipt": receipt,
    }))
}

/// Records `reply` for the reserved attempt: again is idempotent, a different reply
/// is refused.
pub fn settle(store: &mut Store, reply: &[u8]) -> Result<Value> {
    store.ingest_receipt(PROBE_ATTEMPT, reply)?;
    store.ingest_receipt(PROBE_ATTEMPT, reply)?;
    if store
        .ingest_receipt(PROBE_ATTEMPT, b"a different reply")
        .is_ok()
    {
        return Err(Error::Conflict);
    }
    store.check_integrity()?;
    Ok(json!({"conflicting_receipt_refused": true}))
}

/// Runs one admission, commit, reservation and receipt through `store`, checking the
/// rules a host relies on: re-admission reuses, a reservation without a receipt stays
/// unknown, a receipt is idempotent and a different result is refused. Returns what
/// it saw; any broken rule is an error.
pub fn exercise(store: &mut Store) -> Result<Value> {
    let mut seen = reserve(store)?;
    let unknown = report(store)?["receipt"].is_null();
    if !unknown {
        return Err(Error::Conflict);
    }
    let settled = settle(store, b"saved reply")?;
    seen["unknown_until_receipt"] = json!(unknown);
    seen["conflicting_receipt_refused"] = settled["conflicting_receipt_refused"].clone();
    Ok(seen)
}

/// Two fresh memory authorities, each exercised; their call identities must differ
/// (each store draws its own incarnation from getrandom).
pub fn probe(
    first: impl OwnerExclusion + 'static,
    second: impl OwnerExclusion + 'static,
) -> Result<Value> {
    let mut one = memory_store(first)?;
    let mut two = memory_store(second)?;
    let (a, b) = (exercise(&mut one)?, exercise(&mut two)?);
    if a["identity"] == b["identity"] {
        return Err(Error::Conflict);
    }
    Ok(json!({"stores": [a, b], "process_id": spindle_store::process_id()}))
}

/// Opening refused before any authority exists (a caller's open transaction) still
/// hands the guard back only after its connection closed.
pub fn refused_open(guard: impl OwnerExclusion + 'static) -> Result<()> {
    let db = Connection::open_in_memory()?;
    db.execute_batch("BEGIN")?;
    match Store::from_connection(db, ConnectionProfile::Memory, guard) {
        Err(Error::Invalid) => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(Error::Conflict),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counted(Arc<AtomicUsize>);
    impl OwnerExclusion for Counted {}
    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn probe_runs_two_distinct_authorities_and_releases_both_guards() {
        let released = Arc::new(AtomicUsize::new(0));
        let report = probe(Counted(released.clone()), Counted(released.clone())).unwrap();
        assert_eq!(released.load(Ordering::SeqCst), 2);
        let stores = report["stores"].as_array().unwrap();
        assert_ne!(stores[0]["identity"], stores[1]["identity"]);
        assert_eq!(stores[0]["unknown_until_receipt"], true);
        assert_eq!(stores[0]["conflicting_receipt_refused"], true);
    }

    #[test]
    fn a_refused_open_still_releases_its_guard() {
        let released = Arc::new(AtomicUsize::new(0));
        refused_open(Counted(released.clone())).unwrap();
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }
}
