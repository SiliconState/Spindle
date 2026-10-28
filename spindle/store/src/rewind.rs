//! Restartable rewind. Targets are settled completed roots or, in realms without
//! owned scopes, idle points just before a model request. Settled child cuts are
//! history, not live forks.
use crate::{Error, Result, Store};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

pub(crate) const KIND: &str = "spindle-rewind-v1";
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RewindPhase {
    Pending,
    FinishedPaused,
    Resumed,
    CancelledPaused,
    CancelledResumed,
}
impl RewindPhase {
    fn decode(value: u64) -> Result<Self> {
        match value {
            0 => Ok(Self::Pending),
            1 => Ok(Self::FinishedPaused),
            2 => Ok(Self::Resumed),
            3 => Ok(Self::CancelledPaused),
            4 => Ok(Self::CancelledResumed),
            _ => Err(Error::Incompatible),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindStatus {
    pub scope: String,
    pub command: String,
    pub target: u64,
    pub old_revision: u64,
    pub old_lineage: u64,
    pub phase: RewindPhase,
    pub revision: Option<u64>,
}
/// What a rewind target restores. Neither kind restores processes or grants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RewindTargetKind {
    /// A completed turn with no pending effects.
    Settled,
    /// An idle mid-turn point whose next action is a model request, in a scope
    /// without owned children. The re-run request is a fresh effect.
    BeforeModel,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RewindTarget {
    pub revision: u64,
    pub kind: RewindTargetKind,
}

pub(crate) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE rewinds(scope TEXT NOT NULL,command TEXT NOT NULL,admission INTEGER NOT NULL UNIQUE REFERENCES admissions(sequence),target INTEGER NOT NULL REFERENCES revisions(revision),old_revision INTEGER NOT NULL REFERENCES revisions(revision),old_lineage INTEGER NOT NULL,epoch INTEGER NOT NULL,state INTEGER NOT NULL CHECK(state BETWEEN 0 AND 4),revision INTEGER UNIQUE REFERENCES revisions(revision),PRIMARY KEY(scope,command));
        CREATE TABLE execution_cuts(scope TEXT NOT NULL,revision INTEGER NOT NULL UNIQUE REFERENCES revisions(revision),admission INTEGER NOT NULL REFERENCES admissions(sequence),epoch INTEGER NOT NULL,manifest BLOB NOT NULL,PRIMARY KEY(scope,revision));
        CREATE TABLE rewound_tasks(scope TEXT PRIMARY KEY REFERENCES owned_scopes(scope),source_scope TEXT NOT NULL,rewind_command TEXT NOT NULL,source_revision INTEGER NOT NULL REFERENCES revisions(revision),admission INTEGER NOT NULL UNIQUE REFERENCES admissions(sequence),workspace TEXT NOT NULL REFERENCES objects(digest),FOREIGN KEY(source_scope,rewind_command) REFERENCES rewinds(scope,command));")?;
    Ok(())
}
pub struct RewoundTaskRequest {
    pub source_scope: String,
    pub rewind_command: String,
    pub source_revision: u64,
    pub command: crate::Command,
}
impl std::fmt::Debug for RewoundTaskRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RewoundTaskRequest(..)")
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewoundTaskAdmission {
    pub admission: crate::Admission,
    pub workspace: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CutMember {
    pub scope: String,
    pub parent: Option<String>,
    pub admission: u64,
    pub revision: u64,
    pub workspace: String,
    pub continuation_digest: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionCut {
    pub scope: String,
    pub revision: u64,
    pub admission: u64,
    pub members: Vec<CutMember>,
}

#[cfg(feature = "dext-turn")]
fn cut_manifest(
    db: &Connection,
    scope: &str,
    revision: u64,
    admission: u64,
    live: bool,
) -> Result<(ExecutionCut, Vec<u8>)> {
    use crate::ownership::{ScopeKind, ScopeState};
    use dext_core::turn::{Phase, Stop};
    use dext_transcript::{Block, turn::TurnContinuation};
    use sha2::{Digest, Sha256};
    let scopes = crate::ownership::cut_scopes(db, scope, admission)?;
    let mut cut = ExecutionCut {
        scope: scope.into(),
        revision,
        admission,
        members: vec![],
    };
    let mut manifest = b"SEC1".to_vec();
    fn field(out: &mut Vec<u8>, value: &str) {
        out.extend_from_slice(&(value.len() as u32).to_be_bytes());
        out.extend_from_slice(value.as_bytes());
    }
    field(&mut manifest, scope);
    manifest.extend_from_slice(&revision.to_be_bytes());
    manifest.extend_from_slice(&admission.to_be_bytes());
    manifest.extend_from_slice(&(scopes.len() as u32).to_be_bytes());
    for node in scopes {
        if node.kind != ScopeKind::Task
            || (live
                && (matches!(node.state, ScopeState::Cancelling | ScopeState::Suspended)
                    || (node.scope != scope && node.state != ScopeState::Settled)))
        {
            return Err(Error::Conflict);
        }
        let selected: u64 = db.query_row(
            "SELECT coalesce(max(revision),0) FROM revisions WHERE scope=?1 AND revision<=?2",
            params![node.scope, revision as i64],
            |row| crate::nonnegative_id(row, 0),
        )?;
        if selected == 0 || (node.scope == scope && selected != revision) {
            return Err(Error::Conflict);
        }
        let (engine, bytes, root): (String, Vec<u8>, Option<String>) = db.query_row(
            "SELECT engine,continuation,workspace FROM revisions WHERE revision=?1",
            [selected as i64],
            |row| {
                Ok((
                    crate::bounded_label(row, 0)?,
                    crate::packed_blob(db, row, 1, crate::MAX_PAYLOAD)?,
                    crate::optional_digest(row, 2)?,
                ))
            },
        )?;
        if engine != crate::turn::ENGINE {
            return Err(Error::Unsupported);
        }
        let state = TurnContinuation::restore(&bytes).map_err(|_| Error::Incompatible)?;
        if state.scope() != node.scope
            || state.control().phase() != Phase::Stopped(Stop::Complete)
            || !state
                .pending_effects()
                .map_err(|_| Error::Incompatible)?
                .is_empty()
        {
            return Err(Error::Conflict);
        }
        let pending: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM admissions a WHERE a.scope=?1 AND a.sequence<=?2 AND a.kind!=?4 AND NOT EXISTS(SELECT 1 FROM consumed c WHERE c.admission=a.sequence AND c.revision<=?3) AND NOT EXISTS(SELECT 1 FROM dropped_admissions d WHERE d.admission=a.sequence))
             OR EXISTS(SELECT 1 FROM intents i JOIN revisions r ON r.revision=i.revision LEFT JOIN attempts a ON a.effect=i.id LEFT JOIN applied_receipts p ON p.attempt=a.id WHERE r.scope=?1 AND i.revision<=?3 AND (p.attempt IS NULL OR p.revision>?3))
             OR EXISTS(SELECT 1 FROM attempts a JOIN intents i ON i.id=a.effect JOIN revisions r ON r.revision=i.revision WHERE r.scope=?1 AND i.revision<=?3 AND i.adapter!='dext-model-v1' AND NOT EXISTS(SELECT 1 FROM strict_receipts s WHERE s.attempt=a.id) AND NOT EXISTS(SELECT 1 FROM image_executions e WHERE e.attempt=a.id AND e.state IN (1,2,3)))
             OR (?5 AND (EXISTS(SELECT 1 FROM cancellations WHERE scope=?1)
             OR EXISTS(SELECT 1 FROM owned_waits w JOIN owned_scopes t ON t.scope=w.target WHERE w.waiter=?1 AND t.state!=2)))",
            params![node.scope, admission as i64, revision as i64, KIND, live], |row| row.get(0))?;
        if pending {
            return Err(Error::Conflict);
        }
        let root = root.ok_or(Error::Unsupported)?;
        crate::workspace::validate_root(db, &node.scope, &root)?;
        for block in state
            .transcript()
            .map_err(|_| Error::Incompatible)?
            .messages
            .iter()
            .flat_map(|message| &message.content)
        {
            match block {
                Block::ImageReference { .. } => return Err(Error::Unsupported),
                Block::ImageArtifact { reference } => {
                    if reference.scope != node.scope || !reference.is_valid() {
                        return Err(Error::Incompatible);
                    }
                    crate::artifact::reference_bytes(
                        db,
                        &node.scope,
                        &crate::artifact::ObjectReference {
                            digest: reference.digest.clone(),
                            byte_length: reference.byte_length,
                        },
                    )?;
                }
                _ => {}
            }
        }
        let digest: String = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        field(&mut manifest, &node.scope);
        field(&mut manifest, node.parent.as_deref().unwrap_or(""));
        manifest.extend_from_slice(&node.admission.to_be_bytes());
        manifest.extend_from_slice(&selected.to_be_bytes());
        field(&mut manifest, &root);
        field(&mut manifest, &digest);
        cut.members.push(CutMember {
            scope: node.scope,
            parent: node.parent,
            admission: node.admission,
            revision: selected,
            workspace: root,
            continuation_digest: digest,
        });
    }
    if manifest.len() > crate::MAX_PAYLOAD {
        return Err(Error::Exhausted);
    }
    Ok((cut, manifest))
}

#[cfg(feature = "dext-turn")]
fn retained_cut(db: &Connection, scope: &str, revision: u64) -> Result<Option<ExecutionCut>> {
    let saved: Option<(u64, u64, Vec<u8>)> = db
        .query_row(
            "SELECT admission,epoch,manifest FROM execution_cuts WHERE scope=?1 AND revision=?2",
            params![scope, revision as i64],
            |row| {
                Ok((
                    crate::nonnegative_id(row, 0)?,
                    crate::nonnegative_id(row, 1)?,
                    crate::bounded_blob(row, 2, crate::MAX_PAYLOAD)?,
                ))
            },
        )
        .optional()?;
    let Some((admission, epoch, manifest)) = saved else {
        return Ok(None);
    };
    let (current_epoch, maximum): (u64, u64) = db.query_row(
        "SELECT epoch,(SELECT coalesce(max(sequence),0) FROM admissions) FROM realm",
        [],
        |row| {
            Ok((
                crate::nonnegative_id(row, 0)?,
                crate::nonnegative_id(row, 1)?,
            ))
        },
    )?;
    if admission == 0 || admission > maximum || epoch == 0 || epoch > current_epoch {
        return Err(Error::Incompatible);
    }
    let (cut, expected) =
        cut_manifest(db, scope, revision, admission, false).map_err(|_| Error::Incompatible)?;
    if expected != manifest {
        return Err(Error::Incompatible);
    }
    Ok(Some(cut))
}

#[cfg(feature = "dext-turn")]
fn restart_initial(
    db: &Connection,
    source_scope: &str,
    source_revision: u64,
    command: &crate::Command,
) -> Result<String> {
    use crate::turn::{Action, ENGINE, execution_id};
    use dext_core::model::{ModelWait, Phase};
    use dext_transcript::{Block, turn::TurnContinuation};
    if command.kind != ENGINE || command.scope == source_scope {
        return Err(Error::Invalid);
    }
    let (engine, bytes, workspace): (String, Vec<u8>, Option<String>) = db.query_row(
        "SELECT engine,continuation,workspace FROM revisions WHERE scope=?1 AND revision=?2",
        params![source_scope, source_revision as i64],
        |row| {
            Ok((
                crate::bounded_label(row, 0)?,
                crate::packed_blob(db, row, 1, crate::MAX_PAYLOAD)?,
                crate::optional_digest(row, 2)?,
            ))
        },
    )?;
    if engine != ENGINE {
        return Err(Error::Unsupported);
    }
    let source = TurnContinuation::restore(&bytes).map_err(|_| Error::Incompatible)?;
    let Action::Start { initial, request } = Action::decode(&command.payload)? else {
        return Err(Error::Invalid);
    };
    let initial_state = TurnContinuation::restore(&initial).map_err(|_| Error::Invalid)?;
    let transcript = initial_state.transcript().map_err(|_| Error::Invalid)?;
    let prompt = transcript.messages.last().ok_or(Error::Invalid)?;
    let [Block::Text { text }] = prompt.content.as_slice() else {
        return Err(Error::Invalid);
    };
    if prompt.role != "user" {
        return Err(Error::Invalid);
    }
    let expected = source
        .restart_settled(
            command.scope.clone(),
            execution_id(&command.scope, &command.id)?,
            text.clone(),
        )
        .map_err(|_| Error::Invalid)?;
    if expected.checkpoint().map_err(|_| Error::Invalid)? != initial {
        return Err(Error::Conflict);
    }
    let request = ModelWait::restore(&request).map_err(|_| Error::Invalid)?;
    if request.phase() != Phase::Prepared || request.attempt() != 0 {
        return Err(Error::Invalid);
    }
    expected
        .prepare(request.request().clone(), &transcript.route_binding)
        .map_err(|_| Error::Invalid)?;
    let root = workspace.ok_or(Error::Unsupported)?;
    crate::workspace::validate_root(db, source_scope, &root)?;
    Ok(root)
}

#[cfg(feature = "dext-turn")]
pub(crate) fn validate_rewound_start(
    db: &Connection,
    proposal: &crate::transition::Proposal,
    admission: u64,
) -> Result<()> {
    let root: Option<String> = db
        .query_row(
            "SELECT workspace FROM rewound_tasks WHERE scope=?1 AND admission=?2",
            params![proposal.scope, admission as i64],
            |row| crate::bounded_label(row, 0),
        )
        .optional()?;
    if let Some(root) = root
        && (proposal.expected_revision != 0 || proposal.workspace.as_ref() != Some(&root))
    {
        return Err(Error::Conflict);
    }
    Ok(())
}

pub(crate) fn validate_proposal(
    db: &Connection,
    proposal: &crate::transition::Proposal,
    revision: u64,
) -> Result<()> {
    let crate::transition::Observation::Admission(admission) = proposal.observation else {
        return Ok(());
    };
    #[cfg(feature = "dext-turn")]
    validate_rewound_start(db, proposal, admission)?;
    let kind: String = db.query_row(
        "SELECT kind FROM admissions WHERE sequence=?1",
        [admission as i64],
        |row| crate::bounded_label(row, 0),
    )?;
    if kind != KIND {
        return Ok(());
    }
    let target: Option<u64> = db.query_row("SELECT target FROM rewinds WHERE admission=?1 AND scope=?2 AND state IN (1,2) AND revision=?3 AND old_revision=?4", params![admission as i64,proposal.scope,revision as i64,proposal.expected_revision as i64], |row| crate::nonnegative_id(row, 0)).optional()?;
    let Some(target) = target else {
        return Err(Error::Incompatible);
    };
    if !proposal.effects.is_empty() || proposal.workspace.is_none() {
        return Err(Error::Incompatible);
    }
    // A rewind sets the target's app state when it differs from the old head's.
    let state = crate::transition::app_state_at(db, &proposal.scope, target)?;
    let old = crate::transition::app_state_at(db, &proposal.scope, proposal.expected_revision)?;
    if proposal.state != (state != old).then_some(state) {
        return Err(Error::Incompatible);
    }
    Ok(())
}

pub(crate) fn ensure_open(db: &Connection) -> Result<()> {
    let held: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM rewinds WHERE state IN (0,1,3))",
        [],
        |row| row.get(0),
    )?;
    if held { Err(Error::Paused) } else { Ok(()) }
}

/// True for an intent left in a finalized rewind's abandoned future. It must never
/// be reserved: its continuation is no longer selected.
pub(crate) fn superseded(db: &Connection, intent: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM intents i JOIN revisions r ON r.revision=i.revision JOIN rewinds w ON w.scope=r.scope WHERE i.id=?1 AND w.state IN (1,2) AND i.revision>w.target AND i.revision<=w.old_revision)",
        [intent],
        |row| row.get(0),
    )?)
}

/// Admissions still queued: neither consumed by a revision nor explicitly dropped.
/// Requires alias `a` for admissions.
pub(crate) const QUEUED: &str = "NOT EXISTS(SELECT 1 FROM consumed c WHERE c.admission=a.sequence) AND NOT EXISTS(SELECT 1 FROM dropped_admissions d WHERE d.admission=a.sequence)";
/// Intents not left behind in a finalized rewind's abandoned future. Abandoned ones
/// were never reserved, or are model requests of unknown outcome whose results can no
/// longer be adopted; reservation refuses them, so they stop counting as pending
/// work. Requires aliases `i` (intents) and `r` (their revision).
pub(crate) const LIVE_INTENT: &str = "NOT EXISTS(SELECT 1 FROM rewinds w WHERE w.scope=r.scope AND w.state IN (1,2) AND i.revision>w.target AND i.revision<=w.old_revision)";
fn payload(target: u64, expected: u64, lineage: u64) -> Vec<u8> {
    [
        b"SRW1".as_slice(),
        &target.to_be_bytes(),
        &expected.to_be_bytes(),
        &lineage.to_be_bytes(),
    ]
    .concat()
}
fn status(db: &Connection, scope: &str, command: &str) -> Result<Option<RewindStatus>> {
    let record = db.query_row("SELECT target,old_revision,old_lineage,state,revision FROM rewinds WHERE scope=?1 AND command=?2", params![scope,command], |row| {
        Ok((crate::nonnegative_id(row, 0)?, crate::nonnegative_id(row, 1)?, crate::nonnegative_id(row, 2)?, crate::nonnegative_id(row, 3)?, if row.get_ref(4)? == rusqlite::types::ValueRef::Null { None } else { Some(crate::nonnegative_id(row, 4)?) }))
    }).optional()?;
    record
        .map(|(target, old_revision, old_lineage, state, revision)| {
            Ok(RewindStatus {
                scope: scope.into(),
                command: command.into(),
                target,
                old_revision,
                old_lineage,
                phase: RewindPhase::decode(state)?,
                revision,
            })
        })
        .transpose()
}

#[cfg(feature = "dext-turn")]
fn fork(
    db: &Connection,
    scope: &str,
    command: &str,
    target: u64,
) -> Result<(Vec<u8>, String, RewindTargetKind)> {
    use dext_core::turn::{Phase, Stop};
    use dext_transcript::turn::TurnContinuation;
    use sha2::{Digest, Sha256};
    let (engine, bytes, root): (String, Vec<u8>, Option<String>) = db
        .query_row(
            "SELECT engine,continuation,workspace FROM revisions WHERE revision=?1 AND scope=?2",
            params![target as i64, scope],
            |row| {
                Ok((
                    crate::bounded_label(row, 0)?,
                    crate::packed_blob(db, row, 1, crate::MAX_PAYLOAD)?,
                    crate::optional_digest(row, 2)?,
                ))
            },
        )
        .optional()?
        .ok_or(Error::Invalid)?;
    if engine != crate::turn::ENGINE {
        return Err(Error::Incompatible);
    }
    let state = TurnContinuation::restore(&bytes).map_err(|_| Error::Incompatible)?;
    if state.scope() != scope
        || !state
            .pending_effects()
            .map_err(|_| Error::Incompatible)?
            .is_empty()
    {
        return Err(Error::Conflict);
    }
    let kind = match state.control().phase() {
        Phase::Stopped(Stop::Complete) => RewindTargetKind::Settled,
        Phase::Ready | Phase::AfterTools => RewindTargetKind::BeforeModel,
        _ => return Err(Error::Conflict),
    };
    let root = root.ok_or(Error::Unsupported)?;
    crate::workspace::validate_root(db, scope, &root)?;
    let mut hash = Sha256::new();
    hash.update(b"spindle-rewind-execution-v1\0");
    for field in [scope, command] {
        hash.update((field.len() as u32).to_be_bytes());
        hash.update(field.as_bytes());
    }
    let identity = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let next = match kind {
        RewindTargetKind::Settled => state.fork_settled(identity),
        RewindTargetKind::BeforeModel => state.fork_before_model(identity),
    }
    .map_err(|_| Error::Incompatible)?;
    Ok((
        next.checkpoint().map_err(|_| Error::Incompatible)?,
        root,
        kind,
    ))
}
#[cfg(feature = "dext-turn")]
/// Owned task trees need an atomic cut of every member, which only a completed root
/// has, so any owned scope in the realm keeps rewinds on the settled-cut protocol.
/// Callers check this; `fork` stays a pure recomputation for historical validation.
fn owned_realm(db: &Connection) -> Result<bool> {
    Ok(
        db.query_row("SELECT EXISTS(SELECT 1 FROM owned_scopes)", [], |row| {
            row.get(0)
        })?,
    )
}
/// Non-model attempts without sealed strict or image cleanup evidence. Requires
/// aliases `a` (attempts) and `i` (intents).
#[cfg(feature = "dext-turn")]
const UNSEALED: &str = "i.adapter!='dext-model-v1' AND NOT EXISTS(SELECT 1 FROM strict_receipts s WHERE s.attempt=a.id) AND NOT EXISTS(SELECT 1 FROM image_executions e WHERE e.attempt=a.id AND e.state IN (1,2,3))";

#[cfg(feature = "dext-turn")]
pub(crate) fn eligible_target(
    db: &Connection,
    scope: &str,
    revision: u64,
) -> Result<RewindTargetKind> {
    let (_, _, kind) = fork(db, scope, "eligibility-probe", revision)?;
    eligible_kind(db, scope, revision, kind)
}

/// Whether some revision at or below `head` is a rewind target, as
/// `Store::rewind_targets` would list it, leaving the head's in-flight work aside: an
/// owner settles that first (a resend records the request it sends again as dropped).
/// Each candidate is read by its control header, as `listed_targets` does (a view
/// cannot re-derive a fork).
#[cfg(feature = "dext-turn")]
pub(crate) fn has_target(db: &Connection, scope: &str, head: u64) -> bool {
    if ensure_open(db).is_err() {
        return false;
    }
    let revisions: Vec<u64> = match db.prepare(
        "SELECT revision FROM revisions WHERE scope=?1 AND revision<=?2 AND engine=?3 ORDER BY revision DESC LIMIT ?4",
    ) {
        Ok(mut query) => query
            .query_map(
                params![
                    scope,
                    head.min(i64::MAX as u64) as i64,
                    crate::turn::ENGINE,
                    crate::ownership::MAX_REPORT as i64
                ],
                |row| crate::nonnegative_id(row, 0),
            )
            .and_then(|rows| rows.collect())
            .unwrap_or_default(),
        Err(_) => return false,
    };
    revisions.into_iter().any(|revision| {
        header_kind(db, scope, revision)
            .and_then(|kind| eligible_kind(db, scope, revision, kind))
            .is_ok()
    })
}

/// Rewind targets among `revisions`, for listing: the head is checked once, and each
/// candidate by its control header instead of re-deriving the fork and re-hashing its
/// workspace (both were checked when it was committed; `begin_rewind` checks again).
#[cfg(feature = "dext-turn")]
pub(crate) fn listed_targets(
    db: &Connection,
    scope: &str,
    head: u64,
    revisions: &[u64],
) -> Vec<(u64, RewindTargetKind)> {
    if ensure_open(db).is_err() || eligible_head(db, scope, head).is_err() {
        return vec![];
    }
    revisions
        .iter()
        .filter_map(|&revision| {
            let kind = header_kind(db, scope, revision).ok()?;
            Some((revision, eligible_kind(db, scope, revision, kind).ok()?))
        })
        .collect()
}

/// What a revision would restore, read from its DTT1 control header (bytes 4..27) and
/// state tag (byte 27; 0 = transcript, no pending effect), as `fork` decides it.
#[cfg(feature = "dext-turn")]
fn header_kind(db: &Connection, scope: &str, revision: u64) -> Result<RewindTargetKind> {
    use dext_core::turn::{Phase, Stop, TurnControl};
    let (engine, bytes, root): (String, Vec<u8>, Option<String>) = db
        .query_row(
            "SELECT engine,continuation,workspace FROM revisions WHERE revision=?1 AND scope=?2",
            params![revision as i64, scope],
            |row| {
                Ok((
                    crate::bounded_label(row, 0)?,
                    crate::packed_blob(db, row, 1, crate::MAX_PAYLOAD)?,
                    crate::optional_digest(row, 2)?,
                ))
            },
        )
        .optional()?
        .ok_or(Error::Invalid)?;
    if engine != crate::turn::ENGINE || bytes.len() < 28 || &bytes[..4] != b"DTT1" {
        return Err(Error::Incompatible);
    }
    let control = TurnControl::restore(&bytes[4..27]).map_err(|_| Error::Incompatible)?;
    if bytes[27] != 0 {
        return Err(Error::Conflict);
    }
    root.ok_or(Error::Unsupported)?;
    match control.phase() {
        Phase::Stopped(Stop::Complete) => Ok(RewindTargetKind::Settled),
        Phase::Ready | Phase::AfterTools => Ok(RewindTargetKind::BeforeModel),
        _ => Err(Error::Conflict),
    }
}

#[cfg(feature = "dext-turn")]
fn eligible_kind(
    db: &Connection,
    scope: &str,
    revision: u64,
    kind: RewindTargetKind,
) -> Result<RewindTargetKind> {
    if owned_realm(db)? {
        if kind != RewindTargetKind::Settled {
            return Err(Error::Unsupported);
        }
        retained_cut(db, scope, revision)?.ok_or(Error::Unsupported)?;
        let maximum: u64 = db.query_row(
            "SELECT coalesce(max(sequence),0) FROM admissions",
            [],
            |row| crate::nonnegative_id(row, 0),
        )?;
        for node in crate::ownership::cut_scopes(db, scope, maximum)? {
            if node.kind != crate::ownership::ScopeKind::Task
                || node.state == crate::ownership::ScopeState::Cancelling
                || node.state == crate::ownership::ScopeState::Suspended
                || (node.scope != scope && node.state != crate::ownership::ScopeState::Settled)
            {
                return Err(Error::Conflict);
            }
        }
    }
    // An intent never reserved is not pending here: `fork` already refused a target
    // with pending effects, so it was resolved without dispatch (guard or refusal).
    let pending: bool = db.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM intents i JOIN revisions r ON r.revision=i.revision JOIN attempts a ON a.effect=i.id LEFT JOIN applied_receipts p ON p.attempt=a.id WHERE r.scope=?1 AND i.revision<=?2 AND (p.attempt IS NULL OR p.revision>?2) AND {LIVE_INTENT})
             OR EXISTS(SELECT 1 FROM attempts a JOIN intents i ON i.id=a.effect JOIN revisions r ON r.revision=i.revision WHERE r.scope=?1 AND i.revision<=?2 AND {UNSEALED})"
        ),
        params![scope, revision as i64],
        |row| row.get(0),
    )?;
    if pending {
        return Err(Error::Conflict);
    }
    Ok(kind)
}

/// The head being rewound away from may hold no queued admission, no native attempt
/// without a receipt and no unsealed native effect. Undispatched intents and model
/// attempts of unknown outcome are allowed: a model call has no external effects, and
/// the forked branch re-runs it under a fresh identity, so a late result can never be
/// adopted. The finalized rewind supersedes them. Owned realms keep the settled-cut
/// rule for the head too.
#[cfg(feature = "dext-turn")]
pub(crate) fn eligible_head(db: &Connection, scope: &str, revision: u64) -> Result<()> {
    if crate::transition::scope_revision(db, scope)? != revision {
        return Err(Error::Conflict);
    }
    let engine: String = db.query_row(
        "SELECT engine FROM revisions WHERE revision=?1",
        [revision as i64],
        |row| crate::bounded_label(row, 0),
    )?;
    if engine != crate::turn::ENGINE {
        return Err(Error::Unsupported);
    }
    if owned_realm(db)? {
        eligible_target(db, scope, revision)?;
    }
    let in_flight: bool = db.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM admissions a WHERE a.scope=?1 AND a.kind!=?2 AND {QUEUED})
             OR EXISTS(SELECT 1 FROM attempts a JOIN intents i ON i.id=a.effect JOIN revisions r ON r.revision=i.revision WHERE r.scope=?1 AND ((i.adapter!='dext-model-v1' AND NOT EXISTS(SELECT 1 FROM receipts q WHERE q.attempt=a.id)) OR ({UNSEALED})))"
        ),
        params![scope, KIND],
        |row| row.get(0),
    )?;
    if in_flight {
        return Err(Error::Conflict);
    }
    let outside: bool = db.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM admissions a WHERE a.scope!=?1 AND a.kind!=?2 AND {QUEUED})
             OR EXISTS(SELECT 1 FROM intents i JOIN revisions r ON r.revision=i.revision LEFT JOIN attempts a ON a.effect=i.id LEFT JOIN applied_receipts p ON p.attempt=a.id WHERE r.scope!=?1 AND p.attempt IS NULL AND {LIVE_INTENT})"
        ),
        params![scope, KIND],
        |row| row.get(0),
    )?;
    if outside {
        return Err(Error::Conflict);
    }
    let cancelled: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM cancellations WHERE scope=?1)",
        [scope],
        |row| row.get(0),
    )?;
    if cancelled {
        return Err(Error::Cancelled);
    }
    Ok(())
}

// Without the turn engine a stored row cannot be checked, so the first one refuses.
#[cfg_attr(not(feature = "dext-turn"), allow(clippy::never_loop))]
fn validate_rewound_tasks(db: &Connection) -> Result<()> {
    let mut query = db.prepare("SELECT scope,source_scope,rewind_command,source_revision,admission,workspace FROM rewound_tasks ORDER BY admission")?;
    let mut rows = query.query([])?;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        count += 1;
        if count > crate::ownership::MAX_SCOPES {
            return Err(Error::Incompatible);
        }
        let scope = crate::bounded_label(row, 0)?;
        let source = crate::bounded_label(row, 1)?;
        let command = crate::bounded_label(row, 2)?;
        let revision = crate::nonnegative_id(row, 3)?;
        let admission = crate::nonnegative_id(row, 4)?;
        let workspace = crate::bounded_label(row, 5)?;
        #[cfg(feature = "dext-turn")]
        {
            let plan = status(db, &source, &command)?.ok_or(Error::Incompatible)?;
            if plan.phase != RewindPhase::Resumed || plan.revision != Some(revision) {
                return Err(Error::Incompatible);
            }
            let (id, lineage, kind, payload, epoch): (String, u64, String, Vec<u8>, u64) = db.query_row(
                "SELECT command_id,lineage,kind,payload,admitted_epoch FROM admissions WHERE sequence=?1 AND scope=?2",
                params![admission as i64, scope],
                |row| Ok((crate::bounded_label(row, 0)?, crate::nonnegative_id(row, 1)?, crate::bounded_label(row, 2)?, crate::packed_blob(db, row, 3, crate::MAX_PAYLOAD)?, crate::nonnegative_id(row, 4)?)),
            )?;
            let initial = crate::Command {
                scope: scope.clone(),
                id,
                lineage,
                kind,
                payload,
            };
            if lineage != plan.old_lineage + 1
                || restart_initial(db, &source, revision, &initial)
                    .map_err(|_| Error::Incompatible)?
                    != workspace
            {
                return Err(Error::Incompatible);
            }
            let owned: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM owned_scopes WHERE scope=?1 AND parent IS NULL AND kind=0 AND admission=?2)",
                params![scope, admission as i64], |row| row.get(0),
            )?;
            let stage = format!("rewound-task-{admission}");
            let pinned: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM artifact_stages WHERE id=?1 AND scope=?2 AND owner_epoch=?3)",
                params![stage, scope, epoch as i64], |row| row.get(0),
            )?;
            if !owned || !pinned {
                return Err(Error::Incompatible);
            }
            let entries = crate::workspace::validate_root(db, &scope, &workspace)?;
            let mut expected = std::collections::BTreeSet::from([workspace]);
            expected.extend(entries.into_iter().map(|entry| entry.object));
            let mut pins =
                db.prepare("SELECT digest FROM staging_pins WHERE stage=?1 ORDER BY digest")?;
            let retained = pins
                .query_map([stage], |row| crate::bounded_label(row, 0))?
                .collect::<std::result::Result<std::collections::BTreeSet<_>, _>>()?;
            if retained != expected {
                return Err(Error::Incompatible);
            }
        }
        #[cfg(not(feature = "dext-turn"))]
        {
            let _ = (scope, source, command, revision, admission, workspace);
            return Err(Error::Unsupported);
        }
    }
    let orphan: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM artifact_stages s WHERE s.id GLOB 'rewound-task-[0-9]*' AND NOT EXISTS(SELECT 1 FROM rewound_tasks t WHERE s.id='rewound-task-'||t.admission AND s.scope=t.scope))",
        [], |row| row.get(0),
    )?;
    if orphan {
        return Err(Error::Incompatible);
    }
    Ok(())
}

// Without the turn engine a stored row cannot be checked, so the first one refuses.
#[cfg_attr(not(feature = "dext-turn"), allow(clippy::never_loop))]
pub(crate) fn validate_storage(db: &Connection) -> Result<()> {
    validate_rewound_tasks(db)?;
    let mut cuts = db.prepare("SELECT scope,revision FROM execution_cuts ORDER BY revision")?;
    let mut rows = cuts.query([])?;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        count += 1;
        if count > crate::MAX_ADMISSIONS {
            return Err(Error::Incompatible);
        }
        let scope = crate::bounded_label(row, 0)?;
        let revision = crate::nonnegative_id(row, 1)?;
        #[cfg(feature = "dext-turn")]
        retained_cut(db, &scope, revision)?.ok_or(Error::Incompatible)?;
        #[cfg(not(feature = "dext-turn"))]
        {
            let _ = (scope, revision);
            return Err(Error::Unsupported);
        }
    }
    let invalid: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM rewinds w JOIN admissions a ON a.sequence=w.admission WHERE w.target<=0 OR w.target>w.old_revision OR w.old_lineage<0 OR w.old_lineage>(SELECT lineage FROM realm) OR w.epoch<=0 OR w.epoch>(SELECT epoch FROM realm) OR w.epoch!=a.admitted_epoch+1 OR w.state NOT BETWEEN 0 AND 4 OR a.scope!=w.scope OR a.command_id!=w.command OR a.kind!=?1 OR a.lineage!=w.old_lineage OR (w.state IN (1,2))!=(w.revision IS NOT NULL) OR (w.revision IS NOT NULL AND w.revision!=w.old_revision+1))", [KIND], |row| row.get(0))?;
    let held: i64 = db.query_row(
        "SELECT count(*) FROM rewinds WHERE state IN (0,1,3)",
        [],
        |row| row.get(0),
    )?;
    if invalid || held > 1 {
        return Err(Error::Incompatible);
    }
    let mut query = db.prepare("SELECT scope,command,admission,target,old_revision,old_lineage,state,revision FROM rewinds ORDER BY admission")?;
    let mut rows = query.query([])?;
    let mut count = 0;
    let mut completed = 0u64;
    let mut prior_revision = 0u64;
    while let Some(row) = rows.next()? {
        count += 1;
        if count > crate::MAX_ADMISSIONS {
            return Err(Error::Incompatible);
        }
        let scope = crate::bounded_label(row, 0)?;
        let command = crate::bounded_label(row, 1)?;
        let admission = crate::nonnegative_id(row, 2)?;
        let target = crate::nonnegative_id(row, 3)?;
        let old = crate::nonnegative_id(row, 4)?;
        let lineage = crate::nonnegative_id(row, 5)?;
        let state = crate::nonnegative_id(row, 6)?;
        if lineage != completed || old < prior_revision {
            return Err(Error::Incompatible);
        }
        let finalized = matches!(state, 1 | 2);
        completed += u64::from(finalized);
        prior_revision = old
            .checked_add(u64::from(finalized))
            .ok_or(Error::Incompatible)?;
        if row.get_ref(7)? != rusqlite::types::ValueRef::Null
            && !matches!(row.get_ref(7)?, rusqlite::types::ValueRef::Integer(value) if value > 0)
        {
            return Err(Error::Incompatible);
        }
        let bytes: Vec<u8> = db.query_row(
            "SELECT payload FROM admissions WHERE sequence=?1",
            [admission as i64],
            |row| crate::packed_blob(db, row, 0, crate::MAX_PAYLOAD),
        )?;
        if bytes != payload(target, old, lineage) {
            return Err(Error::Incompatible);
        }
        if matches!(state, 0 | 1 | 3) {
            let head: u64 = db.query_row("SELECT revision FROM head", [], |row| {
                crate::nonnegative_id(row, 0)
            })?;
            if head != old + u64::from(state == 1) {
                return Err(Error::Incompatible);
            }
        }
        #[cfg(feature = "dext-turn")]
        {
            let owned: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM owned_scopes WHERE scope=?1)",
                [&scope],
                |row| row.get(0),
            )?;
            if owned {
                retained_cut(db, &scope, target)?.ok_or(Error::Incompatible)?;
                retained_cut(db, &scope, old)?.ok_or(Error::Incompatible)?;
            }
        }
        #[cfg(feature = "dext-turn")]
        if matches!(state, 1 | 2) {
            let revision = crate::nonnegative_id(row, 7)?;
            let (expected, root, _) = fork(db, &scope, &command, target)?;
            let (saved, saved_root): (Vec<u8>, Option<String>) = db.query_row(
                "SELECT continuation,workspace FROM revisions WHERE revision=?1",
                [revision as i64],
                |row| {
                    Ok((
                        crate::packed_blob(db, row, 0, crate::MAX_PAYLOAD)?,
                        crate::optional_digest(row, 1)?,
                    ))
                },
            )?;
            if saved != expected || saved_root.as_ref() != Some(&root) {
                return Err(Error::Incompatible);
            }
        }
        #[cfg(not(feature = "dext-turn"))]
        {
            let _ = (&scope, &command, completed, prior_revision);
            return Err(Error::Unsupported);
        }
    }
    let lineage: u64 = db.query_row("SELECT lineage FROM realm", [], |row| {
        crate::nonnegative_id(row, 0)
    })?;
    if lineage != completed {
        return Err(Error::Incompatible);
    }
    let future_admission: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM admissions WHERE lineage>(SELECT lineage FROM realm))",
        [],
        |row| row.get(0),
    )?;
    if future_admission {
        return Err(Error::Incompatible);
    }
    Ok(())
}

impl Store {
    #[cfg(feature = "dext-turn")]
    /// Admits fresh owned work from a resumed rewind without reopening historical
    /// scopes. Host authorization covers retention, scope transfer and current policy.
    /// Admission, ownership, provenance and immutable workspace pins commit together;
    /// dispatch still requires a separately committed intent and fresh authorization.
    pub fn admit_rewound_task<A>(
        &mut self,
        request: &RewoundTaskRequest,
        mut authorize: A,
    ) -> Result<RewoundTaskAdmission>
    where
        A: FnMut(&RewoundTaskRequest) -> Result<()>,
    {
        if self.paused {
            return Err(Error::Paused);
        }
        crate::validate(&request.command)?;
        if !crate::valid_label(&request.source_scope)
            || !crate::valid_label(&request.rewind_command)
            || request.source_revision == 0
            || request.source_revision > i64::MAX as u64
        {
            return Err(Error::Invalid);
        }
        authorize(request)?;
        let result = (|| -> Result<_> {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let old: Option<(String, String, u64, u64, String)> = tx.query_row(
                "SELECT source_scope,rewind_command,source_revision,admission,workspace FROM rewound_tasks WHERE scope=?1",
                [&request.command.scope],
                |row| Ok((crate::bounded_label(row, 0)?, crate::bounded_label(row, 1)?, crate::nonnegative_id(row, 2)?, crate::nonnegative_id(row, 3)?, crate::bounded_label(row, 4)?)),
            ).optional()?;
            if let Some((scope, command, revision, admission, workspace)) = old {
                if scope != request.source_scope
                    || command != request.rewind_command
                    || revision != request.source_revision
                {
                    return Err(Error::Conflict);
                }
                let saved = crate::admit_in_transaction(&tx, self.epoch, &request.command)?;
                if saved.sequence != admission {
                    return Err(Error::Conflict);
                }
                return Ok(RewoundTaskAdmission {
                    admission: saved,
                    workspace,
                });
            }
            ensure_open(&tx)?;
            let existing: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM admissions WHERE scope=?1)",
                [&request.command.scope],
                |row| row.get(0),
            )?;
            if existing {
                return Err(Error::Conflict);
            }
            let plan = status(&tx, &request.source_scope, &request.rewind_command)?
                .ok_or(Error::Invalid)?;
            if plan.phase != RewindPhase::Resumed || plan.revision != Some(request.source_revision)
            {
                return Err(Error::Conflict);
            }
            let (epoch, head, lineage): (u64, u64, u64) = tx.query_row(
                "SELECT epoch,(SELECT revision FROM head),lineage FROM realm",
                [],
                |row| {
                    Ok((
                        crate::nonnegative_id(row, 0)?,
                        crate::nonnegative_id(row, 1)?,
                        crate::nonnegative_id(row, 2)?,
                    ))
                },
            )?;
            if epoch != self.epoch
                || head != request.source_revision
                || request.command.lineage != lineage
                || lineage != plan.old_lineage + 1
            {
                return Err(Error::Stale);
            }
            let cancelled: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM cancellations WHERE scope=?1)",
                [&request.source_scope],
                |row| row.get(0),
            )?;
            if cancelled {
                return Err(Error::Cancelled);
            }
            let workspace = restart_initial(
                &tx,
                &request.source_scope,
                request.source_revision,
                &request.command,
            )?;
            let entries = crate::workspace::validate_root(&tx, &request.source_scope, &workspace)?;
            let mut objects = std::collections::BTreeSet::from([workspace.clone()]);
            objects.extend(entries.into_iter().map(|entry| entry.object));
            let (stages, pins): (i64, i64) = tx.query_row(
                "SELECT (SELECT count(*) FROM artifact_stages),(SELECT count(*) FROM staging_pins)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if stages >= crate::artifact::MAX_STAGES
                || pins + objects.len() as i64 > crate::artifact::MAX_PINS
            {
                return Err(Error::Exhausted);
            }
            authorize(request)?;
            let admission = crate::ownership::admit_owned_in_transaction(
                &tx,
                self.epoch,
                None,
                crate::ownership::ScopeKind::Task,
                &request.command,
            )?;
            let stage = format!("rewound-task-{}", admission.sequence);
            crate::execute_one(
                &tx,
                "INSERT INTO artifact_stages VALUES(?1,?2,?3)",
                params![stage, request.command.scope, epoch as i64],
            )?;
            for object in objects {
                crate::execute_one(
                    &tx,
                    "INSERT INTO staging_pins VALUES(?1,?2)",
                    params![stage, object],
                )?;
            }
            crate::execute_one(
                &tx,
                "INSERT INTO rewound_tasks VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    request.command.scope,
                    request.source_scope,
                    request.rewind_command,
                    request.source_revision as i64,
                    admission.sequence as i64,
                    workspace
                ],
            )?;
            tx.commit()?;
            Ok(RewoundTaskAdmission {
                admission,
                workspace,
            })
        })();
        self.rewind_result(result)
    }

    #[cfg(feature = "dext-turn")]
    /// Trusted host capture of a completed root and settled task descendants. This
    /// pins historical membership/revisions, not live child processes or old grants.
    pub fn capture_owned_cut(&mut self, scope: &str, expected: u64) -> Result<ExecutionCut> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !crate::valid_label(scope) || expected == 0 || expected > i64::MAX as u64 {
            return Err(Error::Invalid);
        }
        let result = (|| -> Result<_> {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(saved) = retained_cut(&tx, scope, expected)? {
                return Ok(saved);
            }
            ensure_open(&tx)?;
            let (epoch, head, admission): (u64, u64, u64) = tx.query_row(
                "SELECT epoch,(SELECT revision FROM head),(SELECT coalesce(max(sequence),0) FROM admissions) FROM realm", [],
                |row| Ok((crate::nonnegative_id(row, 0)?, crate::nonnegative_id(row, 1)?, crate::nonnegative_id(row, 2)?)))?;
            if epoch != self.epoch
                || head != expected
                || crate::transition::scope_revision(&tx, scope)? != expected
            {
                return Err(Error::Stale);
            }
            let (cut, manifest) = cut_manifest(&tx, scope, expected, admission, true)?;
            let count: i64 =
                tx.query_row("SELECT count(*) FROM execution_cuts", [], |row| row.get(0))?;
            if count >= crate::MAX_ADMISSIONS {
                return Err(Error::Exhausted);
            }
            crate::execute_one(
                &tx,
                "INSERT INTO execution_cuts VALUES(?1,?2,?3,?4,?5)",
                params![
                    scope,
                    expected as i64,
                    admission as i64,
                    epoch as i64,
                    manifest
                ],
            )?;
            tx.commit()?;
            Ok(cut)
        })();
        self.rewind_result(result)
    }

    #[cfg(feature = "dext-turn")]
    pub fn owned_execution_cut(
        &mut self,
        scope: &str,
        revision: u64,
    ) -> Result<Option<ExecutionCut>> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !crate::valid_label(scope) || revision == 0 || revision > i64::MAX as u64 {
            return Err(Error::Invalid);
        }
        let result = retained_cut(&self.db, scope, revision);
        self.rewind_result(result)
    }

    /// The scope's rewind a process began but did not resume (pending, finished or
    /// cancelled while still holding the realm), if any: until it is resumed every new
    /// admission is refused with `Error::Paused`.
    pub fn unfinished_rewind(&mut self, scope: &str) -> Result<Option<RewindStatus>> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !crate::valid_label(scope) {
            return Err(Error::Invalid);
        }
        let result = self
            .db
            .query_row(
                "SELECT command FROM rewinds WHERE scope=?1 AND state IN (0,1,3) ORDER BY admission LIMIT 1",
                [scope],
                |row| crate::bounded_label(row, 0),
            )
            .optional()
            .map_err(Error::from)
            .and_then(|command| match command {
                Some(command) => status(&self.db, scope, &command),
                None => Ok(None),
            });
        self.rewind_result(result)
    }

    pub fn rewind_status(&mut self, scope: &str, command: &str) -> Result<Option<RewindStatus>> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !crate::valid_label(scope) || !crate::valid_label(command) {
            return Err(Error::Invalid);
        }
        let result = status(&self.db, scope, command);
        self.rewind_result(result)
    }

    #[cfg(feature = "dext-turn")]
    /// Local-only completed-root rewind. Owned roots require retained task cuts;
    /// child records stay settled history. Host authorization never comes from a cut.
    pub fn begin_rewind(
        &mut self,
        scope: &str,
        command: &str,
        target: u64,
        expected: u64,
        lineage: u64,
    ) -> Result<RewindStatus> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !crate::valid_label(scope)
            || !crate::valid_label(command)
            || target == 0
            || target > expected
            || expected >= i64::MAX as u64
            || lineage >= i64::MAX as u64
        {
            return Err(Error::Invalid);
        }
        let result = (|| -> Result<_> {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(old) = status(&tx, scope, command)? {
                if old.target != target
                    || old.old_revision != expected
                    || old.old_lineage != lineage
                {
                    return Err(Error::Conflict);
                }
                return Ok((old, None));
            }
            ensure_open(&tx)?;
            let (epoch, current_lineage): (u64, u64) =
                tx.query_row("SELECT epoch,lineage FROM realm", [], |row| {
                    Ok((
                        crate::nonnegative_id(row, 0)?,
                        crate::nonnegative_id(row, 1)?,
                    ))
                })?;
            let head: u64 = tx.query_row("SELECT revision FROM head", [], |row| {
                crate::nonnegative_id(row, 0)
            })?;
            if epoch != self.epoch || head != expected || current_lineage != lineage {
                return Err(Error::Stale);
            }
            if epoch >= i64::MAX as u64 {
                return Err(Error::Exhausted);
            }
            eligible_head(&tx, scope, expected)?;
            eligible_target(&tx, scope, target)?;
            let admission = crate::admit_rewind_in_transaction(
                &tx,
                self.epoch,
                &crate::Command {
                    scope: scope.into(),
                    id: command.into(),
                    lineage,
                    kind: KIND.into(),
                    payload: payload(target, expected, lineage),
                },
            )?;
            crate::execute_one(
                &tx,
                "INSERT INTO rewinds VALUES(?1,?2,?3,?4,?5,?6,?7,0,NULL)",
                params![
                    scope,
                    command,
                    admission.sequence as i64,
                    target as i64,
                    expected as i64,
                    lineage as i64,
                    (epoch + 1) as i64
                ],
            )?;
            crate::execute_one(&tx, "UPDATE realm SET epoch=epoch+1 WHERE id=1", [])?;
            let record = status(&tx, scope, command)?.ok_or(Error::Incompatible)?;
            tx.commit()?;
            Ok((record, Some(epoch + 1)))
        })();
        let (record, epoch) = self.rewind_result(result)?;
        if let Some(epoch) = epoch {
            self.epoch = epoch;
        }
        #[cfg(feature = "dext-image")]
        self.revoke_image_disclosures();
        Ok(record)
    }

    #[cfg(feature = "dext-turn")]
    pub fn finish_rewind(&mut self, scope: &str, command: &str) -> Result<RewindStatus> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !crate::valid_label(scope) || !crate::valid_label(command) {
            return Err(Error::Invalid);
        }
        let result = (|| -> Result<_> {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let plan = status(&tx, scope, command)?.ok_or(Error::Invalid)?;
            if matches!(
                plan.phase,
                RewindPhase::FinishedPaused | RewindPhase::Resumed
            ) {
                return Ok(plan);
            }
            if plan.phase != RewindPhase::Pending {
                return Err(Error::Conflict);
            }
            let (epoch, lineage): (u64, u64) =
                tx.query_row("SELECT epoch,lineage FROM realm", [], |row| {
                    Ok((
                        crate::nonnegative_id(row, 0)?,
                        crate::nonnegative_id(row, 1)?,
                    ))
                })?;
            let head: u64 = tx.query_row("SELECT revision FROM head", [], |row| {
                crate::nonnegative_id(row, 0)
            })?;
            if epoch != self.epoch || head != plan.old_revision || lineage != plan.old_lineage {
                return Err(Error::Stale);
            }
            if head >= crate::MAX_ADMISSIONS as u64 {
                return Err(Error::Exhausted);
            }
            eligible_head(&tx, scope, head)?;
            let (continuation, root, _) = fork(&tx, scope, command, plan.target)?;
            // The app state goes back with the conversation and the files.
            let target_state = crate::transition::app_state_at(&tx, scope, plan.target)?;
            let state = (target_state != crate::transition::app_state_at(&tx, scope, head)?)
                .then_some(target_state);
            let admission: u64 = tx.query_row(
                "SELECT admission FROM rewinds WHERE scope=?1 AND command=?2",
                params![scope, command],
                |row| crate::nonnegative_id(row, 0),
            )?;
            let proposal = crate::transition::Proposal {
                id: format!("rewind-{admission}"),
                scope: scope.into(),
                observation: crate::transition::Observation::Admission(admission),
                expected_revision: head,
                owner_epoch: self.epoch,
                engine: crate::turn::ENGINE.into(),
                continuation,
                workspace: Some(root),
                effects: vec![],
                state,
            };
            let canonical = crate::transition::canonical(&proposal)?;
            crate::execute_one(
                &tx,
                "INSERT INTO revisions VALUES(?1,?2,?3,?4,?5)",
                params![
                    (head + 1) as i64,
                    scope,
                    proposal.engine,
                    crate::codec::store(&tx, &proposal.continuation)?,
                    proposal.workspace
                ],
            )?;
            crate::execute_one(
                &tx,
                "INSERT INTO consumed VALUES(?1,?2)",
                params![admission as i64, (head + 1) as i64],
            )?;
            if let Some(state) = &proposal.state {
                crate::execute_one(
                    &tx,
                    "INSERT INTO app_state VALUES(?1,?2)",
                    params![(head + 1) as i64, crate::codec::store(&tx, state)?],
                )?;
            }
            crate::execute_one(
                &tx,
                "INSERT INTO proposals VALUES(?1,?2,?3)",
                params![
                    proposal.id,
                    crate::codec::store(&tx, &canonical)?,
                    (head + 1) as i64
                ],
            )?;
            crate::execute_one(
                &tx,
                "UPDATE head SET revision=?1 WHERE id=1",
                [(head + 1) as i64],
            )?;
            crate::execute_one(&tx, "UPDATE realm SET lineage=lineage+1 WHERE id=1", [])?;
            crate::execute_one(
                &tx,
                "UPDATE rewinds SET state=1,revision=?3 WHERE scope=?1 AND command=?2",
                params![scope, command, (head + 1) as i64],
            )?;
            tx.execute(
                "UPDATE task_runs SET state='abandoned' WHERE revision>?1 AND revision<=?2",
                params![plan.target as i64, head as i64],
            )?;
            let result = status(&tx, scope, command)?.ok_or(Error::Incompatible)?;
            tx.commit()?;
            Ok(result)
        })();
        self.rewind_result(result)
    }

    #[cfg(feature = "dext-turn")]
    /// Revisions `begin_rewind` would accept for this scope against the current head,
    /// newest first, from the scope's newest MAX_REPORT revisions (each candidate's
    /// workspace is verified, so the scan is bounded). An error means the head itself
    /// cannot be rewound now (held rewind, queued or in-flight work, or another scope
    /// at the head).
    pub fn rewind_targets(&mut self, scope: &str) -> Result<Vec<RewindTarget>> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !crate::valid_label(scope) {
            return Err(Error::Invalid);
        }
        let result = (|| -> Result<_> {
            ensure_open(&self.db)?;
            let head: u64 = self.db.query_row("SELECT revision FROM head", [], |row| {
                crate::nonnegative_id(row, 0)
            })?;
            eligible_head(&self.db, scope, head)?;
            let revisions: Vec<u64> = self
                .db
                .prepare("SELECT revision FROM revisions WHERE scope=?1 AND revision<=?2 AND engine=?3 ORDER BY revision DESC LIMIT ?4")?
                .query_map(
                    params![
                        scope,
                        head as i64,
                        crate::turn::ENGINE,
                        crate::ownership::MAX_REPORT as i64
                    ],
                    |row| crate::nonnegative_id(row, 0),
                )?
                .collect::<rusqlite::Result<_>>()?;
            let mut targets = vec![];
            for revision in revisions {
                match eligible_target(&self.db, scope, revision) {
                    Ok(kind) => targets.push(RewindTarget { revision, kind }),
                    Err(Error::Conflict | Error::Unsupported) => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(targets)
        })();
        self.rewind_result(result)
    }

    /// Cancelling a plan keeps its old coherent head; it does not undo fencing.
    pub fn cancel_rewind(&mut self, scope: &str, command: &str) -> Result<RewindStatus> {
        self.rewind_phase(scope, command, false)
    }
    /// Fresh authenticated host scheduling decision. Releases pause only; no work
    /// starts, cancelled scopes/grants stay cancelled and old attempts stay fenced.
    pub fn resume_rewind(&mut self, scope: &str, command: &str) -> Result<RewindStatus> {
        self.rewind_phase(scope, command, true)
    }
    fn rewind_phase(&mut self, scope: &str, command: &str, resume: bool) -> Result<RewindStatus> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !crate::valid_label(scope) || !crate::valid_label(command) {
            return Err(Error::Invalid);
        }
        let result = (|| -> Result<_> {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let epoch: u64 = tx.query_row("SELECT epoch FROM realm", [], |row| {
                crate::nonnegative_id(row, 0)
            })?;
            if epoch != self.epoch {
                return Err(Error::Stale);
            }
            let record = status(&tx, scope, command)?.ok_or(Error::Invalid)?;
            let next = match (resume, record.phase) {
                (false, RewindPhase::Pending) => 3,
                (false, RewindPhase::CancelledPaused | RewindPhase::CancelledResumed) => {
                    return Ok(record);
                }
                (true, RewindPhase::FinishedPaused) => 2,
                (true, RewindPhase::CancelledPaused) => 4,
                (true, RewindPhase::Resumed | RewindPhase::CancelledResumed) => return Ok(record),
                _ => return Err(Error::Conflict),
            };
            crate::execute_one(
                &tx,
                "UPDATE rewinds SET state=?3 WHERE scope=?1 AND command=?2",
                params![scope, command, next],
            )?;
            let record = status(&tx, scope, command)?.ok_or(Error::Incompatible)?;
            tx.commit()?;
            Ok(record)
        })();
        self.rewind_result(result)
    }
    fn rewind_result<T>(&mut self, result: Result<T>) -> Result<T> {
        if matches!(
            result,
            Err(Error::Sql(_) | Error::Io(_) | Error::Locked | Error::Incompatible)
        ) {
            self.paused = true;
        }
        result
    }
}
