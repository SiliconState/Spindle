//! Bounded, in-memory authority reference model. Not a durable host or Dext engine.
//! A successful operation represents one atomic store transaction. Real I/O,
//! authentication, artifact sealing and process fencing are deliberately absent.

use std::collections::BTreeMap;

pub mod replay;

pub const MODEL_SCHEMA: u8 = 2;
pub const MAX_RECORDS: usize = 8;
pub const MAX_COMMITS: usize = 16;

macro_rules! id {
    ($($name:ident),+ $(,)?) => {$(
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
        pub struct $name(pub u8);
    )+};
}
id!(
    CommandId, EffectId, AttemptId, CommitId, RevisionId, LineageId, Epoch
);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Recovery {
    Workspace,
    KeyedExternal,
    OpaqueExternal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Disposition {
    Admitted,
    Applied,
    Settled,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Submission {
    pub lineage: LineageId,
    pub payload: u8,
    pub disposition: Disposition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Revision {
    pub id: RevisionId,
    pub lineage: LineageId,
    pub conversation: u8,
    pub workspace: u8,
    pub settled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Intent {
    pub command: CommandId,
    pub lineage: LineageId,
    pub epoch: Epoch,
    pub base: RevisionId,
    pub recovery: Recovery,
    /// Explicit publication selection; the sealed attempt retains its original epoch.
    pub recovered_receipt: Option<AttemptId>,
    pub winner: Option<AttemptId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Receipt {
    pub effect: EffectId,
    pub attempt: AttemptId,
    pub result: u8,
    pub workspace: u8,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Attempt {
    pub effect: EffectId,
    pub epoch: Epoch,
    pub stopped: bool,
    pub receipt: Option<Receipt>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RewindOutcome {
    Pending,
    Finished { revision: RevisionId },
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Rewind {
    pub target: RevisionId,
    pub old_head: RevisionId,
    pub epoch: Epoch,
    pub outcome: RewindOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Operation {
    Admit {
        command: CommandId,
        lineage: LineageId,
        payload: u8,
    },
    Apply {
        command: CommandId,
        effect: EffectId,
        recovery: Recovery,
    },
    Authorize {
        effect: EffectId,
        attempt: AttemptId,
    },
    ConfirmStopped {
        attempt: AttemptId,
    },
    Ingest(Receipt),
    Publish {
        attempt: AttemptId,
    },
    Cancel,
    Takeover,
    RecoverIntent {
        effect: EffectId,
        receipt: Option<AttemptId>,
    },
    SettleCut,
    PrepareRewind {
        target: RevisionId,
    },
    FinishRewind,
    CancelRewind,
    Resume,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fault {
    None,
    BeforeCommit,
    LostAcknowledgement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Conflict,
    Stale,
    Missing,
    Busy,
    Blocked,
    Exhausted,
    StoreUnavailable,
    NotCommitted,
    OutcomeUnknown,
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Commit {
    pub expected_seq: u8,
    pub operation: Operation,
    pub seq: u8,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct State {
    pub seq: u8,
    pub epoch: Epoch,
    pub head: Revision,
    pub paused: bool,
    pub rewind: Option<Rewind>,
    pub submissions: BTreeMap<CommandId, Submission>,
    pub intents: BTreeMap<EffectId, Intent>,
    pub attempts: BTreeMap<AttemptId, Attempt>,
    pub revisions: BTreeMap<RevisionId, Revision>,
    pub commits: BTreeMap<CommitId, Commit>,
}

impl Default for State {
    fn default() -> Self {
        let head = Revision {
            id: RevisionId(0),
            lineage: LineageId(0),
            conversation: 0,
            workspace: 0,
            settled: true,
        };
        Self {
            seq: 0,
            epoch: Epoch(0),
            head,
            paused: false,
            rewind: None,
            submissions: BTreeMap::new(),
            intents: BTreeMap::new(),
            attempts: BTreeMap::new(),
            revisions: BTreeMap::from([(head.id, head)]),
            commits: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct Authority {
    durable: State,
    uncertain: bool,
}

impl Authority {
    pub fn state(&self) -> &State {
        &self.durable
    }

    pub fn commit(
        &mut self,
        id: CommitId,
        expected_seq: u8,
        operation: Operation,
        fault: Fault,
    ) -> Result<u8> {
        if self.uncertain {
            return Err(Error::StoreUnavailable);
        }
        if let Some(previous) = self.durable.commits.get(&id) {
            return if previous.expected_seq == expected_seq && previous.operation == operation {
                Ok(previous.seq)
            } else {
                Err(Error::Conflict)
            };
        }
        if self.durable.seq != expected_seq {
            return Err(Error::Stale);
        }
        if self.durable.commits.len() >= MAX_COMMITS {
            return Err(Error::Exhausted);
        }
        let mut candidate = self.durable.clone();
        candidate.apply(&operation)?;
        candidate.seq = next(candidate.seq)?;
        candidate.commits.insert(
            id,
            Commit {
                expected_seq,
                operation,
                seq: candidate.seq,
            },
        );
        if fault == Fault::BeforeCommit {
            return Err(Error::NotCommitted);
        }
        self.durable = candidate;
        if fault == Fault::LostAcknowledgement {
            self.uncertain = true;
            return Err(Error::OutcomeUnknown);
        }
        Ok(self.durable.seq)
    }

    /// Models reopening a verified healthy store, not takeover or backup restore.
    pub fn recover_store(&mut self) {
        self.uncertain = false;
    }
}

fn next(value: u8) -> Result<u8> {
    value.checked_add(1).ok_or(Error::Exhausted)
}
fn capacity(len: usize) -> Result<()> {
    if len >= MAX_RECORDS {
        Err(Error::Exhausted)
    } else {
        Ok(())
    }
}

impl State {
    fn pending_rewind(&self) -> bool {
        self.rewind
            .is_some_and(|rewind| rewind.outcome == RewindOutcome::Pending)
    }

    fn runnable(&self) -> Result<()> {
        if self.paused || self.pending_rewind() {
            Err(Error::Blocked)
        } else {
            Ok(())
        }
    }

    fn active(&self, effect: EffectId) -> Result<&Intent> {
        let intent = self.intents.get(&effect).ok_or(Error::Missing)?;
        if intent.epoch != self.epoch || intent.lineage != self.head.lineage {
            return Err(Error::Stale);
        }
        Ok(intent)
    }

    fn save_revision(&mut self, conversation: u8, workspace: u8, settled: bool) -> Result<()> {
        capacity(self.revisions.len())?;
        let id = RevisionId(next(
            self.revisions.keys().next_back().ok_or(Error::Missing)?.0,
        )?);
        self.head = Revision {
            id,
            conversation,
            workspace,
            settled,
            ..self.head
        };
        self.revisions.insert(id, self.head);
        Ok(())
    }

    fn fence(&mut self) -> Result<()> {
        self.epoch = Epoch(next(self.epoch.0)?);
        self.paused = true;
        Ok(())
    }

    fn revoke(&mut self) -> Result<()> {
        self.fence()?;
        for submission in self.submissions.values_mut() {
            if matches!(
                submission.disposition,
                Disposition::Admitted | Disposition::Applied
            ) {
                submission.disposition = Disposition::Cancelled;
            }
        }
        Ok(())
    }

    fn quiescent(&self) -> bool {
        self.attempts.values().all(|attempt| {
            attempt.receipt.is_some()
                || (attempt.stopped
                    && self.intents[&attempt.effect].recovery == Recovery::Workspace)
        })
    }

    // A rebound keyed effect may retry its logical key without being a settled cut.
    fn scheduling_ready(&self) -> bool {
        self.attempts.values().all(|attempt| {
            let intent = &self.intents[&attempt.effect];
            attempt.receipt.is_some()
                || (attempt.stopped && intent.recovery == Recovery::Workspace)
                || (intent.recovery == Recovery::KeyedExternal
                    && intent.epoch == self.epoch
                    && intent.lineage == self.head.lineage
                    && self.submissions[&intent.command].disposition == Disposition::Applied)
        })
    }

    fn settled_cut(&self) -> bool {
        self.quiescent()
            && !self
                .submissions
                .values()
                .any(|submission| submission.disposition == Disposition::Applied)
    }

    fn recover_intent(&mut self, effect: EffectId, receipt: Option<AttemptId>) -> Result<()> {
        if self.pending_rewind() {
            return Err(Error::Blocked);
        }
        let intent = self.intents.get(&effect).ok_or(Error::Missing)?;
        if intent.lineage != self.head.lineage
            || intent.winner.is_some()
            || self.submissions[&intent.command].disposition != Disposition::Applied
        {
            return Err(Error::Stale);
        }
        if let Some(selected) = intent.recovered_receipt
            && receipt != Some(selected)
        {
            return Err(Error::Conflict);
        }
        if let Some(selected) = receipt {
            let attempt = self.attempts.get(&selected).ok_or(Error::Missing)?;
            if attempt.effect != effect {
                return Err(Error::Conflict);
            }
            if attempt.receipt.is_none() {
                return Err(Error::Blocked);
            }
        }
        for attempt in self.attempts.values().filter(|a| a.effect == effect) {
            if receipt.is_none() && attempt.receipt.is_some() {
                return Err(Error::Blocked);
            }
            match intent.recovery {
                Recovery::Workspace if !attempt.stopped => return Err(Error::Blocked),
                Recovery::OpaqueExternal if attempt.receipt.is_none() => {
                    return Err(Error::Blocked);
                }
                Recovery::Workspace | Recovery::KeyedExternal | Recovery::OpaqueExternal => {}
            }
        }
        let intent = self.intents.get_mut(&effect).ok_or(Error::Missing)?;
        intent.epoch = self.epoch;
        intent.recovered_receipt = receipt;
        Ok(())
    }

    fn apply(&mut self, operation: &Operation) -> Result<()> {
        match *operation {
            Operation::Admit {
                command,
                lineage,
                payload,
            } => {
                if let Some(old) = self.submissions.get(&command) {
                    return if old.lineage == lineage && old.payload == payload {
                        Ok(())
                    } else {
                        Err(Error::Conflict)
                    };
                }
                self.runnable()?;
                if lineage != self.head.lineage {
                    return Err(Error::Stale);
                }
                capacity(self.submissions.len())?;
                self.submissions.insert(
                    command,
                    Submission {
                        lineage,
                        payload,
                        disposition: Disposition::Admitted,
                    },
                );
            }
            Operation::Apply {
                command,
                effect,
                recovery,
            } => {
                self.runnable()?;
                if self.intents.contains_key(&effect) {
                    return Err(Error::Conflict);
                }
                let submission = self.submissions.get(&command).ok_or(Error::Missing)?;
                if submission.disposition != Disposition::Admitted
                    || submission.lineage != self.head.lineage
                {
                    return Err(Error::Stale);
                }
                if self
                    .submissions
                    .values()
                    .any(|s| s.disposition == Disposition::Applied)
                {
                    return Err(Error::Busy);
                }
                capacity(self.intents.len())?;
                self.intents.insert(
                    effect,
                    Intent {
                        command,
                        lineage: self.head.lineage,
                        epoch: self.epoch,
                        base: self.head.id,
                        recovery,
                        recovered_receipt: None,
                        winner: None,
                    },
                );
                self.submissions
                    .get_mut(&command)
                    .ok_or(Error::Missing)?
                    .disposition = Disposition::Applied;
                self.save_revision(self.head.conversation, self.head.workspace, false)?;
            }
            Operation::Authorize { effect, attempt } => {
                self.runnable()?;
                let intent = self.active(effect)?;
                if intent.winner.is_some() {
                    return Err(Error::Stale);
                }
                if self.attempts.contains_key(&attempt) {
                    return Err(Error::Conflict);
                }
                for prior in self.attempts.values().filter(|a| a.effect == effect) {
                    if prior.receipt.is_some() {
                        return Err(Error::Blocked);
                    }
                    match intent.recovery {
                        Recovery::OpaqueExternal => return Err(Error::Blocked),
                        Recovery::Workspace if !prior.stopped => return Err(Error::Blocked),
                        Recovery::Workspace | Recovery::KeyedExternal => {}
                    }
                }
                capacity(self.attempts.len())?;
                self.attempts.insert(
                    attempt,
                    Attempt {
                        effect,
                        epoch: self.epoch,
                        stopped: false,
                        receipt: None,
                    },
                );
            }
            Operation::ConfirmStopped { attempt } => {
                self.attempts
                    .get_mut(&attempt)
                    .ok_or(Error::Missing)?
                    .stopped = true;
            }
            Operation::Ingest(receipt) => {
                let attempt = self
                    .attempts
                    .get_mut(&receipt.attempt)
                    .ok_or(Error::Missing)?;
                if attempt.effect != receipt.effect {
                    return Err(Error::Conflict);
                }
                if let Some(old) = attempt.receipt {
                    return if old == receipt {
                        Ok(())
                    } else {
                        Err(Error::Conflict)
                    };
                }
                // A receipt denotes a trusted sealed result with all writers stopped.
                attempt.receipt = Some(receipt);
                attempt.stopped = true;
            }
            Operation::Publish { attempt } => {
                self.runnable()?;
                let attempt_record = self.attempts.get(&attempt).ok_or(Error::Missing)?;
                let receipt = attempt_record.receipt.ok_or(Error::Missing)?;
                let intent = self.active(receipt.effect)?;
                if attempt_record.epoch != self.epoch && intent.recovered_receipt != Some(attempt) {
                    return Err(Error::Stale);
                }
                if intent
                    .recovered_receipt
                    .is_some_and(|selected| selected != attempt)
                {
                    return Err(Error::Conflict);
                }
                if let Some(winner) = intent.winner {
                    return if winner == attempt {
                        Ok(())
                    } else {
                        Err(Error::Conflict)
                    };
                }
                if self.revisions[&intent.base].workspace != self.head.workspace {
                    return Err(Error::Stale);
                }
                if intent.recovery != Recovery::Workspace
                    && receipt.workspace != self.head.workspace
                {
                    return Err(Error::Conflict);
                }
                if intent.recovery == Recovery::Workspace
                    && self
                        .attempts
                        .values()
                        .any(|other| other.effect == receipt.effect && !other.stopped)
                {
                    return Err(Error::Blocked);
                }
                let command = intent.command;
                self.intents
                    .get_mut(&receipt.effect)
                    .ok_or(Error::Missing)?
                    .winner = Some(attempt);
                self.submissions
                    .get_mut(&command)
                    .ok_or(Error::Missing)?
                    .disposition = Disposition::Settled;
                self.save_revision(receipt.result, receipt.workspace, self.settled_cut())?;
            }
            Operation::Cancel => {
                self.revoke()?;
            }
            Operation::Takeover => {
                self.fence()?;
            }
            Operation::RecoverIntent { effect, receipt } => {
                self.recover_intent(effect, receipt)?;
            }
            Operation::SettleCut => {
                self.runnable()?;
                if !self.settled_cut() {
                    return Err(Error::Blocked);
                }
                if !self.head.settled {
                    self.save_revision(self.head.conversation, self.head.workspace, true)?;
                }
            }
            Operation::PrepareRewind { target } => {
                if self.pending_rewind() {
                    return Err(Error::Busy);
                }
                let revision = self.revisions.get(&target).ok_or(Error::Missing)?;
                if !revision.settled {
                    return Err(Error::Blocked);
                }
                self.revoke()?;
                self.rewind = Some(Rewind {
                    target,
                    old_head: self.head.id,
                    epoch: self.epoch,
                    outcome: RewindOutcome::Pending,
                });
            }
            Operation::FinishRewind => {
                let rewind = self.rewind.ok_or(Error::Missing)?;
                match rewind.outcome {
                    RewindOutcome::Finished { .. } => return Ok(()),
                    RewindOutcome::Cancelled => return Err(Error::Blocked),
                    RewindOutcome::Pending => {}
                }
                if !self.quiescent() {
                    return Err(Error::Blocked);
                }
                let revision = self.revisions[&rewind.target];
                self.head.lineage = LineageId(next(self.head.lineage.0)?);
                self.save_revision(revision.conversation, revision.workspace, true)?;
                self.rewind = Some(Rewind {
                    outcome: RewindOutcome::Finished {
                        revision: self.head.id,
                    },
                    ..rewind
                });
            }
            Operation::CancelRewind => {
                let rewind = self.rewind.ok_or(Error::Missing)?;
                match rewind.outcome {
                    RewindOutcome::Cancelled => return Ok(()),
                    RewindOutcome::Finished { .. } => return Err(Error::Conflict),
                    RewindOutcome::Pending => {}
                }
                self.fence()?;
                self.rewind = Some(Rewind {
                    outcome: RewindOutcome::Cancelled,
                    ..rewind
                });
            }
            Operation::Resume => {
                if self.pending_rewind() || !self.scheduling_ready() {
                    return Err(Error::Blocked);
                }
                self.paused = false;
            }
        }
        Ok(())
    }

    pub fn check_invariants(&self) -> std::result::Result<(), &'static str> {
        if self.commits.len() > MAX_COMMITS
            || [
                self.submissions.len(),
                self.intents.len(),
                self.attempts.len(),
                self.revisions.len(),
            ]
            .into_iter()
            .any(|len| len > MAX_RECORDS)
        {
            return Err("record quota exceeded");
        }
        // Validate reference closure before any indexed lifecycle predicates.
        if self
            .attempts
            .values()
            .any(|attempt| !self.intents.contains_key(&attempt.effect))
        {
            return Err("attempt lacks intent");
        }
        if self.intents.values().any(|intent| {
            !self.submissions.contains_key(&intent.command)
                || !self.revisions.contains_key(&intent.base)
        }) {
            return Err("intent lacks admission or base");
        }
        if self
            .revisions
            .iter()
            .any(|(id, revision)| *id != revision.id)
        {
            return Err("revision identity differs from key");
        }
        let mut positions = std::collections::BTreeSet::new();
        for commit in self.commits.values() {
            if commit.seq == 0
                || commit.seq > self.seq
                || commit.expected_seq.checked_add(1) != Some(commit.seq)
                || !positions.insert(commit.seq)
            {
                return Err("invalid commit sequence");
            }
        }
        if self.revisions.get(&self.head.id) != Some(&self.head) {
            return Err("head is not a retained coherent revision");
        }
        if self.seq as usize != self.commits.len() {
            return Err("commit sequence has a gap");
        }
        if self.pending_rewind() && !self.paused {
            return Err("rewind did not fence scheduling");
        }
        if self.head.settled && !self.settled_cut() {
            return Err("settled head has unresolved owned work");
        }
        if let Some(rewind) = self.rewind {
            if !self
                .revisions
                .get(&rewind.target)
                .is_some_and(|r| r.settled)
                || !self.revisions.contains_key(&rewind.old_head)
                || rewind.epoch > self.epoch
            {
                return Err("rewind lost its retained plan");
            }
            match rewind.outcome {
                RewindOutcome::Pending if self.head.id != rewind.old_head => {
                    return Err("pending rewind changed the selected head");
                }
                RewindOutcome::Finished { revision }
                    if !self.revisions.get(&revision).is_some_and(|r| r.settled) =>
                {
                    return Err("rewind lost its finished cut");
                }
                _ => {}
            }
        }
        for (id, attempt) in &self.attempts {
            let intent = self
                .intents
                .get(&attempt.effect)
                .ok_or("attempt lacks intent")?;
            if attempt.epoch > intent.epoch || intent.epoch > self.epoch {
                return Err("attempt or intent belongs to a future epoch");
            }
            if let Some(receipt) = attempt.receipt
                && (receipt.effect != attempt.effect || receipt.attempt != *id || !attempt.stopped)
            {
                return Err("receipt is not bound to a sealed attempt");
            }
        }
        for (effect, intent) in &self.intents {
            let submission = self
                .submissions
                .get(&intent.command)
                .ok_or("intent lacks admission")?;
            if submission.lineage != intent.lineage {
                return Err("intent changed command lineage");
            }
            if intent.epoch > self.epoch || !self.revisions.contains_key(&intent.base) {
                return Err("intent lacks a valid epoch or retained base");
            }
            if let Some(selected) = intent.recovered_receipt {
                let attempt = self
                    .attempts
                    .get(&selected)
                    .ok_or("recovery lacks attempt")?;
                if attempt.effect != *effect || attempt.receipt.is_none() {
                    return Err("recovery lacks bound sealed receipt");
                }
            }
            if let Some(winner) = intent.winner {
                let attempt = self.attempts.get(&winner).ok_or("winner lacks attempt")?;
                if attempt.effect != *effect
                    || attempt.receipt.is_none()
                    || (attempt.epoch != intent.epoch && intent.recovered_receipt != Some(winner))
                    || submission.disposition != Disposition::Settled
                {
                    return Err("winner lacks applicable receipt and settled submission");
                }
            }
        }
        Ok(())
    }
}
