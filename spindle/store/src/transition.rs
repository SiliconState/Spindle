//! Atomic admission consumption and continuation/intents publication.
//! Intents are inert records; optional trusted-host dispatch lives in broker.

use crate::{Error, MAX_ADMISSIONS, MAX_PAYLOAD, Result, Store, valid_label};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::collections::BTreeSet;

pub const MAX_EFFECTS: usize = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectIntent {
    pub id: String,
    pub adapter: String,
    pub input: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Observation {
    Admission(u64),
    Receipt(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Proposal {
    pub id: String,
    pub scope: String,
    pub observation: Observation,
    /// Prior revision for this scope, not the global commit-journal position.
    pub expected_revision: u64,
    pub owner_epoch: u64,
    pub engine: String,
    pub continuation: Vec<u8>,
    /// Select a retained immutable file map, or preserve the prior root.
    pub workspace: Option<String>,
    pub effects: Vec<EffectIntent>,
    /// The scope's app state from this revision on (a JSON object, `MAX_STATE` bytes
    /// at most), committed with it; None keeps the prior state.
    pub state: Option<Vec<u8>>,
}

/// Largest app state of a scope, as JSON.
pub const MAX_STATE: usize = 1024 * 1024;

/// Whether `bytes` can be an app state: a JSON object of at most `MAX_STATE` bytes.
pub fn valid_state(bytes: &[u8]) -> bool {
    if bytes.len() > MAX_STATE {
        return false;
    }
    #[cfg(feature = "dext-turn")]
    return serde_json::from_slice::<serde_json::Value>(bytes).is_ok_and(|value| value.is_object());
    // Without a JSON parser (the bare store), the shape is checked by its builders.
    #[cfg(not(feature = "dext-turn"))]
    return bytes.first() == Some(&b'{') && bytes.last() == Some(&b'}');
}

/// Trusted-host expected bindings, compared with retained records in the same
/// transaction as adoption. This is not worker authentication or dispatch authority.
pub struct ReceiptBinding<'a> {
    pub attempt: &'a crate::completion::Attempt,
    pub intent_revision: u64,
    pub intent: &'a EffectIntent,
    pub result: &'a [u8],
    /// Host-extracted object references from the retained receipt. Existing staging
    /// pins remain retained; this check neither parses receipts nor grants disclosure.
    pub objects: &'a [crate::artifact::ObjectReference],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    pub revision: u64,
    pub engine: String,
    pub continuation: Vec<u8>,
    pub workspace: Option<String>,
}

fn validate_receipt_binding(
    tx: &Transaction<'_>,
    scope: &str,
    binding: &ReceiptBinding<'_>,
) -> Result<()> {
    let retained = tx.query_row(
        "SELECT a.effect,a.owner_epoch,a.implementation,a.grant_reference,i.revision,i.adapter,i.input,q.result,r.scope FROM attempts a JOIN intents i ON a.effect=i.id JOIN revisions r ON i.revision=r.revision JOIN receipts q ON q.attempt=a.id WHERE a.id=?1",
        [&binding.attempt.id],
        |row| Ok((
            crate::bounded_label(row, 0)?,
            crate::nonnegative_id(row, 1)?,
            crate::bounded_label(row, 2)?,
            crate::bounded_label(row, 3)?,
            crate::nonnegative_id(row, 4)?,
            crate::bounded_label(row, 5)?,
            crate::packed_blob(tx, row, 6, MAX_PAYLOAD)?,
            crate::packed_blob(tx, row, 7, MAX_PAYLOAD)?,
            crate::bounded_label(row, 8)?,
        )),
    ).optional()?.ok_or(Error::Invalid)?;
    if retained.0 != binding.attempt.effect
        || retained.1 != binding.attempt.owner_epoch
        || retained.2 != binding.attempt.implementation
        || retained.3 != binding.attempt.grant_reference
        || retained.4 != binding.intent_revision
        || retained.5 != binding.intent.adapter
        || retained.6 != binding.intent.input
        || retained.7 != binding.result
        || retained.8 != scope
    {
        return Err(Error::Conflict);
    }
    crate::artifact::validate_references(tx, scope, binding.objects)?;
    Ok(())
}

pub(crate) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE revisions(revision INTEGER PRIMARY KEY, scope TEXT NOT NULL, engine TEXT NOT NULL, continuation BLOB NOT NULL, workspace TEXT REFERENCES objects(digest));
         INSERT INTO revisions VALUES(0,'','uninitialized',X'',NULL);
         CREATE TABLE head(id INTEGER PRIMARY KEY CHECK(id=1), revision INTEGER NOT NULL REFERENCES revisions(revision));
         INSERT INTO head VALUES(1,0);
         CREATE TABLE proposals(id TEXT PRIMARY KEY, canonical BLOB NOT NULL, revision INTEGER NOT NULL UNIQUE REFERENCES revisions(revision));
         CREATE TABLE consumed(admission INTEGER PRIMARY KEY REFERENCES admissions(sequence), revision INTEGER NOT NULL UNIQUE REFERENCES revisions(revision));
         CREATE TABLE intents(id TEXT PRIMARY KEY, revision INTEGER NOT NULL REFERENCES revisions(revision), position INTEGER NOT NULL, adapter TEXT NOT NULL, input BLOB NOT NULL, UNIQUE(revision,position));
         CREATE TABLE attempts(id TEXT PRIMARY KEY, effect TEXT NOT NULL UNIQUE REFERENCES intents(id), owner_epoch INTEGER NOT NULL, implementation TEXT NOT NULL, grant_reference TEXT NOT NULL);
         CREATE TABLE receipts(attempt TEXT PRIMARY KEY REFERENCES attempts(id), result BLOB NOT NULL, workspace TEXT REFERENCES objects(digest));
         CREATE TABLE applied_receipts(attempt TEXT PRIMARY KEY REFERENCES receipts(attempt), revision INTEGER NOT NULL UNIQUE REFERENCES revisions(revision));
         CREATE TABLE cancellations(scope TEXT PRIMARY KEY, command_id TEXT NOT NULL, epoch INTEGER NOT NULL);
         CREATE TABLE image_executions(attempt TEXT PRIMARY KEY REFERENCES attempts(id), worker TEXT NOT NULL, state INTEGER NOT NULL CHECK(state BETWEEN 0 AND 3));
         CREATE TABLE strict_receipts(attempt TEXT PRIMARY KEY REFERENCES receipts(attempt),base TEXT NOT NULL REFERENCES objects(digest),root TEXT NOT NULL REFERENCES objects(digest),helper TEXT NOT NULL);", 
    )?;
    Ok(())
}

/// Counts and bounds always; proposals, intents and receipt roots in full only above
/// revision `since` and receipt row `receipts_since` (the last full pass).
pub(crate) fn validate_storage(db: &Connection, since: i64, receipts_since: i64) -> Result<()> {
    let heads: i64 = db.query_row("SELECT count(*) FROM head WHERE id=1", [], |r| r.get(0))?;
    let revisions: i64 = db.query_row("SELECT count(*) FROM revisions", [], |r| r.get(0))?;
    let proposals: i64 = db.query_row("SELECT count(*) FROM proposals", [], |r| r.get(0))?;
    let consumed: i64 = db.query_row("SELECT count(*) FROM consumed", [], |r| r.get(0))?;
    let applied_receipts: i64 =
        db.query_row("SELECT count(*) FROM applied_receipts", [], |r| r.get(0))?;
    let intents: i64 = db.query_row("SELECT count(*) FROM intents", [], |r| r.get(0))?;
    let attempts: i64 = db.query_row("SELECT count(*) FROM attempts", [], |r| r.get(0))?;
    let receipts: i64 = db.query_row("SELECT count(*) FROM receipts", [], |r| r.get(0))?;
    let head: i64 = db.query_row("SELECT revision FROM head WHERE id=1", [], |r| r.get(0))?;
    let max: i64 = db.query_row("SELECT max(revision) FROM revisions", [], |r| r.get(0))?;
    let invalid: i64 = db.query_row(
        "SELECT count(*) FROM revisions WHERE revision<0 OR length(continuation)>?1 OR length(engine) NOT BETWEEN 1 AND 128",
        [crate::codec::stored_bound(MAX_PAYLOAD) as i64], |r| r.get(0),
    )?;
    let foreign_fault = db.prepare("PRAGMA foreign_key_check")?.exists([])?;
    if heads != 1
        || revisions != proposals + 1
        || proposals != consumed + applied_receipts
        || attempts > intents
        || receipts > attempts
        || applied_receipts > receipts
        || proposals > MAX_ADMISSIONS
        || intents > MAX_ADMISSIONS * MAX_EFFECTS as i64
        || head != max
        || max != proposals
        || invalid != 0
        || foreign_fault
    {
        return Err(Error::Incompatible);
    }
    validate_records(db, since)?;
    let mut receipts = db.prepare("SELECT r.scope,q.workspace FROM receipts q JOIN attempts a ON q.attempt=a.id JOIN intents i ON a.effect=i.id JOIN revisions r ON i.revision=r.revision WHERE q.workspace IS NOT NULL AND q.rowid>?1")?;
    let mut rows = receipts.query([receipts_since])?;
    while let Some(row) = rows.next()? {
        let scope: String = row.get(0)?;
        let root: String = row.get(1)?;
        crate::workspace::validate_root(db, &scope, &root)?;
    }
    Ok(())
}

fn validate_records(db: &Connection, since: i64) -> Result<()> {
    let invalid: i64 = db.query_row(
        "SELECT
          (SELECT count(*) FROM intents WHERE revision<=0 OR position<0 OR position>=8 OR length(id) NOT BETWEEN 1 AND 128 OR length(adapter) NOT BETWEEN 1 AND 128 OR length(input)>?1)
        + (SELECT count(*) FROM receipts WHERE length(result)>?1)
        + (SELECT count(*) FROM attempts WHERE owner_epoch<=0 OR owner_epoch>(SELECT epoch FROM realm WHERE id=1) OR length(id) NOT BETWEEN 1 AND 128 OR length(implementation) NOT BETWEEN 1 AND 128 OR length(grant_reference) NOT BETWEEN 1 AND 128)
        + (SELECT count(*) FROM cancellations WHERE epoch<=0 OR epoch>(SELECT epoch FROM realm WHERE id=1) OR length(scope) NOT BETWEEN 1 AND 128 OR length(command_id) NOT BETWEEN 1 AND 128 OR NOT EXISTS(SELECT 1 FROM admissions WHERE admissions.scope=cancellations.scope))
        + (SELECT count(*) FROM image_executions e JOIN attempts a ON e.attempt=a.id WHERE e.state NOT BETWEEN 0 AND 3 OR length(e.worker)!=64 OR e.worker GLOB '*[^0-9a-f]*' OR a.implementation!='dext-read-image-v1-'||e.worker)
        + (SELECT count(*) FROM strict_receipts s JOIN receipts q ON q.attempt=s.attempt JOIN attempts a ON a.id=s.attempt JOIN intents i ON i.id=a.effect JOIN revisions r ON r.revision=i.revision WHERE length(s.helper)!=64 OR s.helper GLOB '*[^0-9a-f]*' OR a.implementation NOT IN ('dext-strict-bash-v1-'||s.helper,'dext-host-bash-v1-'||s.helper,'spindle-app-tool-v1-'||s.helper) OR q.workspace IS NULL OR q.workspace!=s.root OR NOT EXISTS(SELECT 1 FROM revisions b WHERE b.workspace=s.base AND b.scope=r.scope))
        + (SELECT count(*) FROM proposals WHERE length(canonical)>?2 OR revision<=0)
        + (SELECT count(*) FROM consumed WHERE revision<=0)
        + (SELECT count(*) FROM applied_receipts WHERE revision<=0)",
        [
            crate::codec::stored_bound(MAX_PAYLOAD) as i64,
            crate::codec::stored_bound(crate::MAX_CANONICAL) as i64,
        ],
        |r| r.get(0),
    )?;
    if invalid != 0 {
        return Err(Error::Incompatible);
    }
    let initial: (String, String, Vec<u8>) = db.query_row(
        "SELECT scope,engine,continuation FROM revisions WHERE revision=0",
        [],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                crate::packed_blob(db, r, 2, MAX_PAYLOAD)?,
            ))
        },
    )?;
    if initial != (String::new(), "uninitialized".into(), Vec::new()) {
        return Err(Error::Incompatible);
    }
    let initial_workspace: Option<String> = db.query_row(
        "SELECT workspace FROM revisions WHERE revision=0",
        [],
        |r| r.get(0),
    )?;
    if initial_workspace.is_some() {
        return Err(Error::Incompatible);
    }
    let epoch: i64 = db.query_row("SELECT epoch FROM realm WHERE id=1", [], |r| r.get(0))?;
    // Records up to `since` were validated before; only each scope's newest revision
    // among them matters for the chain after it (one grouped query, so their records
    // are never read).
    let mut scope_revisions = std::collections::BTreeMap::new();
    let mut statement = db.prepare(
        "SELECT scope,max(revision) FROM revisions WHERE revision BETWEEN 1 AND ?1 GROUP BY scope",
    )?;
    let mut rows = statement.query([since])?;
    while let Some(row) = rows.next()? {
        let scope: String = crate::bounded_label(row, 0)?;
        let revision: i64 = row.get(1)?;
        scope_revisions.insert(scope, revision as u64);
    }
    drop(rows);
    drop(statement);
    let mut statement = db.prepare(
        "SELECT id,canonical,revision FROM proposals WHERE revision>?1 ORDER BY revision",
    )?;
    let mut rows = statement.query([since])?;
    while let Some(row) = rows.next()? {
        let revision: i64 = row.get(2)?;
        let id: String = row.get(0)?;
        let bytes = crate::packed_blob(db, row, 1, crate::MAX_CANONICAL)?;
        let proposal = decode(&bytes)?;
        if proposal.id != id
            || proposal.expected_revision
                != scope_revisions.get(&proposal.scope).copied().unwrap_or(0)
            || proposal.owner_epoch > epoch as u64
            || canonical(&proposal)? != bytes
        {
            return Err(Error::Incompatible);
        }
        let retained: (String, String, Vec<u8>) = db.query_row(
            "SELECT scope,engine,continuation FROM revisions WHERE revision=?1",
            [revision],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    crate::packed_blob(db, r, 2, MAX_PAYLOAD)?,
                ))
            },
        )?;
        if retained
            != (
                proposal.scope.clone(),
                proposal.engine.clone(),
                proposal.continuation.clone(),
            )
        {
            return Err(Error::Incompatible);
        }
        let prior_root: Option<String> = db.query_row(
            "SELECT workspace FROM revisions WHERE revision=?1",
            [proposal.expected_revision as i64],
            |r| r.get(0),
        )?;
        let selected_root: Option<String> = db.query_row(
            "SELECT workspace FROM revisions WHERE revision=?1",
            [revision],
            |r| r.get(0),
        )?;
        if selected_root != proposal.workspace.clone().or(prior_root) {
            return Err(Error::Incompatible);
        }
        if let Some(root) = selected_root {
            crate::workspace::validate_root(db, &proposal.scope, &root)?;
        }
        let admission: Option<i64> = db
            .query_row(
                "SELECT admission FROM consumed WHERE revision=?1",
                [revision],
                |r| r.get(0),
            )
            .optional()?;
        let receipt: Option<String> = db
            .query_row(
                "SELECT attempt FROM applied_receipts WHERE revision=?1",
                [revision],
                |r| r.get(0),
            )
            .optional()?;
        let scope: String = match &proposal.observation {
            Observation::Admission(expected) if admission == Some(*expected as i64) && receipt.is_none() => {
                db.query_row("SELECT scope FROM admissions WHERE sequence=?1", [*expected as i64], |r| r.get(0))?
            }
            Observation::Receipt(expected) if receipt.as_ref() == Some(expected) && admission.is_none() => {
                db.query_row("SELECT r.scope FROM attempts a JOIN intents i ON a.effect=i.id JOIN revisions r ON i.revision=r.revision WHERE a.id=?1 AND r.revision<?2", params![expected, revision], |r| r.get(0))?
            }
            _ => return Err(Error::Incompatible),
        };
        if scope != proposal.scope {
            return Err(Error::Incompatible);
        }
        crate::rewind::validate_proposal(db, &proposal, revision as u64)?;
        if let Observation::Receipt(attempt) = &proposal.observation {
            let sealed_root: Option<String> = db.query_row(
                "SELECT workspace FROM receipts WHERE attempt=?1",
                [attempt],
                |r| r.get(0),
            )?;
            if proposal.workspace != sealed_root {
                return Err(Error::Incompatible);
            }
            if sealed_root.is_some() {
                let base_root: Option<String> = db.query_row("SELECT coalesce((SELECT s.base FROM strict_receipts s WHERE s.attempt=a.id),r.workspace) FROM attempts a JOIN intents i ON a.effect=i.id JOIN revisions r ON i.revision=r.revision WHERE a.id=?1", [attempt], |r| r.get(0))?;
                let prior_root: Option<String> = db.query_row(
                    "SELECT workspace FROM revisions WHERE revision=?1",
                    [proposal.expected_revision as i64],
                    |r| r.get(0),
                )?;
                if base_root != prior_root {
                    return Err(Error::Incompatible);
                }
            }
        }
        let mut intents = db.prepare(
            "SELECT id,adapter,input,position FROM intents WHERE revision=?1 ORDER BY position",
        )?;
        let mut retained = intents.query([revision])?;
        for (position, expected) in proposal.effects.iter().enumerate() {
            let row = retained.next()?.ok_or(Error::Incompatible)?;
            if row.get::<_, String>(0)? != expected.id
                || row.get::<_, String>(1)? != expected.adapter
                || crate::packed_blob(db, row, 2, MAX_PAYLOAD)? != expected.input
                || row.get::<_, i64>(3)? != position as i64
            {
                return Err(Error::Incompatible);
            }
        }
        if retained.next()?.is_some() {
            return Err(Error::Incompatible);
        }
        drop(retained);
        drop(intents);
        let state = app_state_row(db, revision as u64)?;
        if state != proposal.state {
            return Err(Error::Incompatible);
        }
        scope_revisions.insert(proposal.scope, revision as u64);
    }
    Ok(())
}

struct Decoder<'a>(&'a [u8]);
impl<'a> Decoder<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        if count > self.0.len() {
            return Err(Error::Incompatible);
        }
        let (value, rest) = self.0.split_at(count);
        self.0 = rest;
        Ok(value)
    }
    fn number(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().map_err(|_| Error::Incompatible)?,
        ))
    }
    fn length(&mut self) -> Result<usize> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().map_err(|_| Error::Incompatible)?) as usize)
    }
    fn bytes(&mut self, bound: usize) -> Result<Vec<u8>> {
        let length = self.length()?;
        if length > bound {
            return Err(Error::Incompatible);
        }
        Ok(self.take(length)?.to_vec())
    }
    fn label(&mut self) -> Result<String> {
        String::from_utf8(self.bytes(128)?).map_err(|_| Error::Incompatible)
    }
}

fn decode(bytes: &[u8]) -> Result<Proposal> {
    let prefix = b"spindle-proposal-v4\0";
    let mut decoder = Decoder(bytes);
    if decoder.take(prefix.len())? != prefix {
        return Err(Error::Incompatible);
    }
    let id = decoder.label()?;
    let scope = decoder.label()?;
    let observation = match decoder.take(1)?[0] {
        0 => Observation::Admission(decoder.number()?),
        1 => Observation::Receipt(decoder.label()?),
        _ => return Err(Error::Incompatible),
    };
    let expected_revision = decoder.number()?;
    let owner_epoch = decoder.number()?;
    let engine = decoder.label()?;
    let continuation = decoder.bytes(MAX_PAYLOAD)?;
    let workspace = match decoder.take(1)?[0] {
        0 => None,
        1 => Some(decoder.label()?),
        _ => return Err(Error::Incompatible),
    };
    let count = decoder.length()?;
    if count > MAX_EFFECTS {
        return Err(Error::Incompatible);
    }
    let mut effects = Vec::with_capacity(count);
    for _ in 0..count {
        effects.push(EffectIntent {
            id: decoder.label()?,
            adapter: decoder.label()?,
            input: decoder.bytes(MAX_PAYLOAD)?,
        });
    }
    let state = match decoder.0.first() {
        None => None,
        Some(1) => {
            decoder.take(1)?;
            Some(decoder.bytes(MAX_STATE)?)
        }
        Some(_) => return Err(Error::Incompatible),
    };
    if !decoder.0.is_empty() {
        return Err(Error::Incompatible);
    }
    let proposal = Proposal {
        id,
        scope,
        observation,
        expected_revision,
        owner_epoch,
        engine,
        continuation,
        workspace,
        effects,
        state,
    };
    canonical(&proposal).map_err(|_| Error::Incompatible)?;
    Ok(proposal)
}

fn field(bytes: &mut Vec<u8>, value: &[u8]) {
    bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
    bytes.extend_from_slice(value);
}

pub(crate) fn canonical(proposal: &Proposal) -> Result<Vec<u8>> {
    if !valid_label(&proposal.id)
        || !valid_label(&proposal.scope)
        || !valid_label(&proposal.engine)
        || match &proposal.observation {
            Observation::Admission(id) => *id == 0 || *id > i64::MAX as u64,
            Observation::Receipt(id) => !valid_label(id),
        }
        || proposal.expected_revision >= i64::MAX as u64
        || proposal.owner_epoch == 0
        || proposal.owner_epoch > i64::MAX as u64
        || proposal
            .workspace
            .as_ref()
            .is_some_and(|root| !crate::artifact::valid_digest(root))
        || proposal.effects.len() > MAX_EFFECTS
    {
        return Err(Error::Invalid);
    }
    let mut ids = BTreeSet::new();
    for effect in &proposal.effects {
        if !valid_label(&effect.id) || !valid_label(&effect.adapter) || !ids.insert(&effect.id) {
            return Err(Error::Invalid);
        }
    }
    crate::fits(proposal.continuation.len())?;
    for effect in &proposal.effects {
        crate::fits(effect.input.len())?;
    }
    let mut bytes = b"spindle-proposal-v4\0".to_vec();
    field(&mut bytes, proposal.id.as_bytes());
    field(&mut bytes, proposal.scope.as_bytes());
    match &proposal.observation {
        Observation::Admission(id) => {
            bytes.push(0);
            bytes.extend_from_slice(&id.to_be_bytes());
        }
        Observation::Receipt(id) => {
            bytes.push(1);
            field(&mut bytes, id.as_bytes());
        }
    }
    bytes.extend_from_slice(&proposal.expected_revision.to_be_bytes());
    bytes.extend_from_slice(&proposal.owner_epoch.to_be_bytes());
    field(&mut bytes, proposal.engine.as_bytes());
    field(&mut bytes, &proposal.continuation);
    match &proposal.workspace {
        None => bytes.push(0),
        Some(root) => {
            bytes.push(1);
            field(&mut bytes, root.as_bytes());
        }
    }
    bytes.extend_from_slice(&(proposal.effects.len() as u32).to_be_bytes());
    for effect in &proposal.effects {
        field(&mut bytes, effect.id.as_bytes());
        field(&mut bytes, effect.adapter.as_bytes());
        field(&mut bytes, &effect.input);
    }
    // Only a proposal that sets app state carries it, so every other one keeps the
    // bytes earlier builds wrote.
    if let Some(state) = &proposal.state {
        if !valid_state(state) {
            return Err(Error::Invalid);
        }
        bytes.push(1);
        field(&mut bytes, state);
    }
    Ok(bytes)
}

/// The app state adopting `attempt`'s result sets: its recorded patch (an app tool's
/// answer) applied to the scope's state at `prior`, or None.
fn receipt_state(
    db: &Connection,
    scope: &str,
    prior: u64,
    attempt: &str,
) -> Result<Option<Vec<u8>>> {
    let patch: Option<Vec<u8>> = db
        .query_row(
            "SELECT patch FROM app_patches WHERE attempt=?1",
            [attempt],
            |r| crate::packed_blob(db, r, 0, MAX_STATE),
        )
        .optional()?;
    let Some(patch) = patch else {
        return Ok(None);
    };
    #[cfg(feature = "dext-turn")]
    return Ok(Some(apply_state_patch(
        &app_state_at(db, scope, prior)?,
        &patch,
    )?));
    #[cfg(not(feature = "dext-turn"))]
    {
        let _ = (scope, prior, patch);
        Err(Error::Unsupported)
    }
}

impl Store {
    /// The app state that adopting `attempt`'s result after revision `prior` must set
    /// (`receipt_state`): None unless it is an app tool answer with a state patch.
    pub fn adopted_state(&self, scope: &str, prior: u64, attempt: &str) -> Result<Option<Vec<u8>>> {
        if self.paused {
            return Err(Error::Paused);
        }
        receipt_state(&self.db, scope, prior, attempt)
    }
}

/// The app state committed with `revision`, if that revision set one.
pub(crate) fn app_state_row(db: &Connection, revision: u64) -> Result<Option<Vec<u8>>> {
    Ok(db
        .query_row(
            "SELECT value FROM app_state WHERE revision=?1",
            [revision as i64],
            |r| crate::packed_blob(db, r, 0, MAX_STATE),
        )
        .optional()?)
}

/// The app state of `scope` as of `revision`: what the newest revision at or before
/// it that set one committed, or an empty object.
pub fn app_state_at(db: &Connection, scope: &str, revision: u64) -> Result<Vec<u8>> {
    Ok(db
        .query_row(
            "SELECT s.value FROM app_state s JOIN revisions r ON r.revision=s.revision WHERE r.scope=?1 AND s.revision<=?2 ORDER BY s.revision DESC LIMIT 1",
            params![scope, revision.min(i64::MAX as u64) as i64],
            |r| crate::packed_blob(db, r, 0, MAX_STATE),
        )
        .optional()?
        .unwrap_or_else(|| b"{}".to_vec()))
}

pub(crate) fn scope_revision(db: &Connection, scope: &str) -> Result<u64> {
    Ok(db.query_row(
        "SELECT coalesce(max(revision),0) FROM revisions WHERE scope=?1",
        [scope],
        |row| crate::nonnegative_id(row, 0),
    )?)
}

impl Store {
    pub fn snapshot(&mut self) -> Result<Snapshot> {
        if self.paused {
            return Err(Error::Paused);
        }
        let result = self.db.query_row(
            "SELECT r.revision,r.engine,r.continuation,r.workspace FROM head h JOIN revisions r ON h.revision=r.revision WHERE h.id=1",
            [], |r| Ok(Snapshot { revision: crate::nonnegative_id(r, 0)?, engine: crate::bounded_label(r, 1)?, continuation: crate::packed_blob(&self.db, r, 2, MAX_PAYLOAD)?, workspace: crate::optional_digest(r, 3)? }),
        );
        match result {
            Ok(snapshot) => Ok(snapshot),
            Err(error) => {
                self.paused = true;
                Err(error.into())
            }
        }
    }

    /// Current state for one scope; an uninitialized scope has the empty revision zero.
    /// The realm snapshot remains the global journal tail, not a scheduling baseline.
    pub fn snapshot_scope(&mut self, scope: &str) -> Result<Snapshot> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(scope) {
            return Err(Error::Invalid);
        }
        let revision = scope_revision(&self.db, scope);
        let revision = match revision {
            Ok(revision) => revision,
            Err(error) => {
                self.paused = true;
                return Err(error);
            }
        };
        if revision == 0 {
            return Ok(Snapshot {
                revision: 0,
                engine: "uninitialized".into(),
                continuation: vec![],
                workspace: None,
            });
        }
        self.snapshot_at(scope, revision)
    }

    /// Historical inspection is scope-bound and never selects a newer revision.
    pub fn snapshot_at(&mut self, scope: &str, revision: u64) -> Result<Snapshot> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(scope) || revision > i64::MAX as u64 {
            return Err(Error::Invalid);
        }
        let result = self.db.query_row(
            "SELECT revision,engine,continuation,workspace FROM revisions WHERE revision=?1 AND scope=?2",
            params![revision as i64, scope],
            |r| Ok(Snapshot { revision: crate::nonnegative_id(r, 0)?, engine: crate::bounded_label(r, 1)?, continuation: crate::packed_blob(&self.db, r, 2, MAX_PAYLOAD)?, workspace: crate::optional_digest(r, 3)? }),
        ).optional();
        match result {
            Ok(Some(snapshot)) => Ok(snapshot),
            Ok(None) => Err(Error::Invalid),
            Err(error) => {
                self.paused = true;
                Err(error.into())
            }
        }
    }

    /// Commits admission consumption, continuation, intents and publication identity
    /// together. The caller computes the proposal without I/O; this is not an engine.
    pub fn commit_proposal(&mut self, proposal: &Proposal) -> Result<u64> {
        // App state is set only by the turn paths: a prompt's start (the host's value,
        // like the prompt itself), `Action::State` and an app tool's adopted answer
        // (both re-derived from what was admitted or recorded), and rewinds.
        if proposal.state.is_some() {
            return Err(Error::Invalid);
        }
        self.commit_bound_proposal(proposal, None, |_| Ok(()))
    }

    /// Derives every declared object from a SAR1 receipt, then checks the exact
    /// receipt bytes and their objects in the adoption transaction. Opaque payload
    /// semantics and continuation derivation remain trusted-host responsibilities.
    pub fn commit_artifact_receipt_proposal(
        &mut self,
        proposal: &Proposal,
        attempt: &crate::completion::Attempt,
        intent_revision: u64,
        intent: &EffectIntent,
        result: &[u8],
    ) -> Result<u64> {
        if self.paused {
            return Err(Error::Paused);
        }
        let receipt = crate::artifact::ArtifactReceipt::decode(result)?;
        self.commit_receipt_proposal(
            proposal,
            &ReceiptBinding {
                attempt,
                intent_revision,
                intent,
                result,
                objects: &receipt.objects,
            },
        )
    }

    /// Checks retained attempt, intent and result in the adoption transaction.
    /// Old-owner receipts remain adoptable by the current owner; cancellation and
    /// head fences still apply. Exact committed retries remain readable after cancel.
    pub fn commit_receipt_proposal(
        &mut self,
        proposal: &Proposal,
        binding: &ReceiptBinding<'_>,
    ) -> Result<u64> {
        self.commit_checked_receipt_proposal(proposal, binding, |_| Ok(()))
    }

    pub(crate) fn commit_checked_receipt_proposal<F>(
        &mut self,
        proposal: &Proposal,
        binding: &ReceiptBinding<'_>,
        validate: F,
    ) -> Result<u64>
    where
        F: FnOnce(&Transaction<'_>) -> Result<()>,
    {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(&binding.attempt.id)
            || !valid_label(&binding.attempt.effect)
            || !valid_label(&binding.attempt.implementation)
            || !valid_label(&binding.attempt.grant_reference)
            || !valid_label(&binding.intent.adapter)
            || binding.attempt.owner_epoch == 0
            || binding.attempt.owner_epoch > i64::MAX as u64
            || proposal.observation != Observation::Receipt(binding.attempt.id.clone())
            || binding.attempt.effect != binding.intent.id
            || binding.intent_revision == 0
            || binding.intent_revision > i64::MAX as u64
        {
            return Err(Error::Invalid);
        }
        crate::fits(binding.result.len())?;
        crate::fits(binding.intent.input.len())?;
        if binding.objects.len() > crate::artifact::MAX_RECEIPT_OBJECTS {
            return Err(Error::Invalid);
        }
        let mut digests = BTreeSet::new();
        for object in binding.objects {
            if !crate::artifact::valid_digest(&object.digest)
                || object.byte_length > crate::artifact::MAX_OBJECT as u64
                || !digests.insert(&object.digest)
            {
                return Err(Error::Invalid);
            }
        }
        self.commit_bound_proposal(proposal, Some(binding), validate)
    }

    pub(crate) fn commit_bound_proposal<F>(
        &mut self,
        proposal: &Proposal,
        binding: Option<&ReceiptBinding<'_>>,
        validate: F,
    ) -> Result<u64>
    where
        F: FnOnce(&Transaction<'_>) -> Result<()>,
    {
        if self.paused {
            return Err(Error::Paused);
        }
        let bytes = canonical(proposal)?;
        // What validation on open reads back; anything longer would refuse the store.
        if bytes.len() > crate::MAX_CANONICAL {
            return Err(Error::Exhausted);
        }
        let result = self.proposal_transaction(proposal, &bytes, binding, validate);
        if matches!(
            result,
            Err(Error::Sql(_) | Error::Io(_) | Error::Locked | Error::Incompatible)
        ) {
            self.paused = true;
        }
        result
    }

    fn proposal_transaction<F>(
        &mut self,
        proposal: &Proposal,
        bytes: &[u8],
        binding: Option<&ReceiptBinding<'_>>,
        validate: F,
    ) -> Result<u64>
    where
        F: FnOnce(&Transaction<'_>) -> Result<()>,
    {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let epoch: i64 = tx.query_row("SELECT epoch FROM realm WHERE id=1", [], |r| r.get(0))?;
        if epoch != self.epoch as i64 {
            return Err(Error::Stale);
        }
        if let Observation::Admission(admission) = &proposal.observation {
            let kind: Option<String> = tx
                .query_row(
                    "SELECT kind FROM admissions WHERE sequence=?1",
                    [*admission as i64],
                    |row| crate::bounded_label(row, 0),
                )
                .optional()?;
            if kind.as_deref() == Some(crate::rewind::KIND) {
                return Err(Error::Invalid);
            }
        }
        if let Some(binding) = binding {
            validate_receipt_binding(&tx, &proposal.scope, binding)?;
        }
        validate(&tx)?;
        let old: Option<(Vec<u8>, u64)> = tx
            .query_row(
                "SELECT canonical,revision FROM proposals WHERE id=?1",
                [&proposal.id],
                |r| {
                    Ok((
                        crate::packed_blob(&tx, r, 0, crate::MAX_CANONICAL)?,
                        crate::nonnegative_id(r, 1)?,
                    ))
                },
            )
            .optional()?;
        if let Some((old_bytes, revision)) = old {
            if revision == 0 {
                return Err(Error::Incompatible);
            }
            return if bytes == old_bytes {
                Ok(revision)
            } else {
                Err(Error::Conflict)
            };
        }
        let cancelled: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM cancellations WHERE scope=?1)",
            [&proposal.scope],
            |r| r.get(0),
        )?;
        if cancelled {
            return Err(Error::Cancelled);
        }
        crate::ownership::admit_allowed(&tx, &proposal.scope)?;
        if proposal.owner_epoch != self.epoch {
            return Err(Error::Stale);
        }
        let head: i64 = tx.query_row("SELECT revision FROM head WHERE id=1", [], |r| r.get(0))?;
        let baseline = scope_revision(&tx, &proposal.scope)?;
        if head >= MAX_ADMISSIONS {
            return Err(Error::Exhausted);
        }
        let (exists, consumed): (bool, bool) = match &proposal.observation {
            Observation::Admission(id) => (
                tx.query_row("SELECT EXISTS(SELECT 1 FROM admissions a JOIN realm r ON a.lineage=r.lineage WHERE a.sequence=?1 AND a.scope=?2 AND r.id=1)", params![*id as i64, proposal.scope], |r| r.get(0))?,
                tx.query_row("SELECT EXISTS(SELECT 1 FROM consumed WHERE admission=?1)", [*id as i64], |r| r.get(0))?,
            ),
            Observation::Receipt(id) => (
                tx.query_row("SELECT EXISTS(SELECT 1 FROM receipts q JOIN attempts a ON q.attempt=a.id JOIN intents i ON a.effect=i.id JOIN revisions r ON i.revision=r.revision WHERE q.attempt=?1 AND r.scope=?2)", params![id, proposal.scope], |r| r.get(0))?,
                tx.query_row("SELECT EXISTS(SELECT 1 FROM applied_receipts WHERE attempt=?1)", [id], |r| r.get(0))?,
            ),
        };
        if !exists {
            return Err(Error::Invalid);
        }
        if baseline != proposal.expected_revision {
            return Err(Error::Stale);
        }
        let dropped = match &proposal.observation {
            Observation::Admission(id) => tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM dropped_admissions WHERE admission=?1)",
                [*id as i64],
                |r| r.get(0),
            )?,
            Observation::Receipt(_) => false,
        };
        if consumed || dropped {
            return Err(Error::Conflict);
        }
        for effect in &proposal.effects {
            let used: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM intents WHERE id=?1)",
                [&effect.id],
                |r| r.get(0),
            )?;
            if used {
                return Err(Error::Conflict);
            }
        }
        let prior_root: Option<String> = tx.query_row(
            "SELECT workspace FROM revisions WHERE revision=?1",
            [baseline as i64],
            |r| r.get(0),
        )?;
        if let Observation::Receipt(attempt) = &proposal.observation {
            let sealed_root: Option<String> = tx.query_row(
                "SELECT workspace FROM receipts WHERE attempt=?1",
                [attempt],
                |r| r.get(0),
            )?;
            if proposal.workspace != sealed_root {
                return Err(Error::Conflict);
            }
            if sealed_root.is_some() {
                let base_root: Option<String> = tx.query_row("SELECT coalesce((SELECT s.base FROM strict_receipts s WHERE s.attempt=a.id),r.workspace) FROM attempts a JOIN intents i ON a.effect=i.id JOIN revisions r ON i.revision=r.revision WHERE a.id=?1", [attempt], |r| r.get(0))?;
                if base_root != prior_root {
                    return Err(Error::Stale);
                }
            }
        }
        let workspace = proposal.workspace.clone().or(prior_root);
        if let Some(root) = &workspace {
            crate::workspace::validate_root(&tx, &proposal.scope, root)?;
        }
        // Adopting an app tool's answer sets exactly the state its recorded patch makes
        // of the prior one; adopting any other result sets none.
        if let Observation::Receipt(attempt) = &proposal.observation
            && proposal.state != receipt_state(&tx, &proposal.scope, baseline, attempt)?
        {
            return Err(Error::Conflict);
        }
        let revision = head + 1;
        crate::execute_one(
            &tx,
            "INSERT INTO revisions VALUES(?1,?2,?3,?4,?5)",
            params![
                revision,
                proposal.scope,
                proposal.engine,
                crate::codec::store(&tx, &proposal.continuation)?,
                workspace
            ],
        )?;
        match &proposal.observation {
            Observation::Admission(id) => {
                crate::execute_one(
                    &tx,
                    "INSERT INTO consumed VALUES(?1,?2)",
                    params![*id as i64, revision],
                )?;
            }
            Observation::Receipt(id) => {
                crate::execute_one(
                    &tx,
                    "INSERT INTO applied_receipts VALUES(?1,?2)",
                    params![id, revision],
                )?;
            }
        }
        for (position, effect) in proposal.effects.iter().enumerate() {
            crate::execute_one(
                &tx,
                "INSERT INTO intents VALUES(?1,?2,?3,?4,?5)",
                params![
                    effect.id,
                    revision,
                    position as i64,
                    effect.adapter,
                    crate::codec::store(&tx, &effect.input)?
                ],
            )?;
        }
        if let Some(state) = &proposal.state {
            crate::execute_one(
                &tx,
                "INSERT INTO app_state VALUES(?1,?2)",
                params![revision, crate::codec::store(&tx, state)?],
            )?;
        }
        crate::execute_one(
            &tx,
            "INSERT INTO proposals VALUES(?1,?2,?3)",
            params![proposal.id, crate::codec::store(&tx, bytes)?, revision],
        )?;
        crate::execute_one(&tx, "UPDATE head SET revision=?1 WHERE id=1", [revision])?;
        #[cfg(feature = "dext-turn")]
        if let Observation::Admission(admission) = proposal.observation {
            let (kind, payload): (String, Vec<u8>) = tx.query_row(
                "SELECT kind,payload FROM admissions WHERE sequence=?1",
                [admission as i64],
                |row| Ok((row.get(0)?, crate::packed_blob(&tx, row, 1, MAX_PAYLOAD)?)),
            )?;
            if kind == crate::turn::ENGINE
                && let crate::turn::Action::TaskReport { task, .. } =
                    crate::turn::Action::decode(&payload)?
            {
                crate::execute_one(
                    &tx,
                    "UPDATE task_runs SET delivered=?1 WHERE task=?2 AND delivered IS NULL AND state='finished'",
                    params![revision, task],
                )?;
            }
        }
        tx.commit()?;
        Ok(revision as u64)
    }

    /// The revision that committed an intent; dispatch binds to it.
    pub fn intent_revision(&mut self, id: &str) -> Result<Option<u64>> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !valid_label(id) {
            return Err(Error::Invalid);
        }
        let result = self
            .db
            .query_row("SELECT revision FROM intents WHERE id=?1", [id], |r| {
                crate::nonnegative_id(r, 0)
            })
            .optional();
        result.map_err(|error| {
            self.paused = true;
            error.into()
        })
    }

    /// Inspection only. Returned intents confer no execution capability.
    pub fn intents(&mut self, revision: u64) -> Result<Vec<EffectIntent>> {
        if self.paused {
            return Err(Error::Paused);
        }
        if revision > i64::MAX as u64 {
            return Err(Error::Invalid);
        }
        let result = (|| -> std::result::Result<Vec<EffectIntent>, rusqlite::Error> {
            let mut statement = self.db.prepare(
                "SELECT id,adapter,input FROM intents WHERE revision=?1 ORDER BY position LIMIT 9",
            )?;
            statement
                .query_map([revision as i64], |r| {
                    Ok(EffectIntent {
                        id: crate::bounded_label(r, 0)?,
                        adapter: crate::bounded_label(r, 1)?,
                        input: crate::packed_blob(&self.db, r, 2, MAX_PAYLOAD)?,
                    })
                })?
                .collect()
        })();
        match result {
            Ok(intents) if intents.len() <= MAX_EFFECTS => Ok(intents),
            Ok(_) => {
                self.paused = true;
                Err(Error::Incompatible)
            }
            Err(error) => {
                self.paused = true;
                Err(error.into())
            }
        }
    }
}

/// `prior` (an app state) with the JSON merge patch `patch` applied (RFC 7386): an
/// object member set to null is removed, another object merges, any other value
/// replaces. The result must still be an object of at most `MAX_STATE` bytes.
#[cfg(feature = "dext-turn")]
pub fn apply_state_patch(prior: &[u8], patch: &[u8]) -> Result<Vec<u8>> {
    fn merge(target: &mut serde_json::Value, patch: &serde_json::Value) {
        let serde_json::Value::Object(members) = patch else {
            *target = patch.clone();
            return;
        };
        if !target.is_object() {
            *target = serde_json::Value::Object(Default::default());
        }
        let object = target.as_object_mut().expect("made an object above");
        for (key, value) in members {
            if value.is_null() {
                object.remove(key);
            } else {
                merge(
                    object.entry(key.clone()).or_insert(serde_json::Value::Null),
                    value,
                );
            }
        }
    }
    let mut state: serde_json::Value = serde_json::from_slice(prior).map_err(|_| Error::Invalid)?;
    let patch: serde_json::Value = serde_json::from_slice(patch).map_err(|_| Error::Invalid)?;
    if !patch.is_object() {
        return Err(Error::Invalid);
    }
    merge(&mut state, &patch);
    let bytes = serde_json::to_vec(&state).map_err(|_| Error::Invalid)?;
    if !valid_state(&bytes) {
        return Err(Error::Exhausted);
    }
    Ok(bytes)
}
