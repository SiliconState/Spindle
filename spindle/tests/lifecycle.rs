use spindle_reference::*;

fn admit(command: u8) -> Operation {
    Operation::Admit {
        command: CommandId(command),
        lineage: LineageId(0),
        payload: command,
    }
}

fn apply(recovery: Recovery) -> Operation {
    Operation::Apply {
        command: CommandId(0),
        effect: EffectId(0),
        recovery,
    }
}

fn authorize(attempt: u8) -> Operation {
    Operation::Authorize {
        effect: EffectId(0),
        attempt: AttemptId(attempt),
    }
}

fn receipt(attempt: u8, workspace: u8) -> Receipt {
    Receipt {
        effect: EffectId(0),
        attempt: AttemptId(attempt),
        result: 7,
        workspace,
    }
}

fn recover(receipt: Option<u8>) -> Operation {
    Operation::RecoverIntent {
        effect: EffectId(0),
        receipt: receipt.map(AttemptId),
    }
}

fn publish(attempt: u8) -> Operation {
    Operation::Publish {
        attempt: AttemptId(attempt),
    }
}

fn stop(attempt: u8) -> Operation {
    Operation::ConfirmStopped {
        attempt: AttemptId(attempt),
    }
}

fn prepare_rewind() -> Operation {
    Operation::PrepareRewind {
        target: RevisionId(0),
    }
}

fn commit(host: &mut Authority, operation: Operation) -> u8 {
    let seq = host.state().seq;
    let before = host.clone();
    let result = host
        .commit(CommitId(seq), seq, operation.clone(), Fault::None)
        .unwrap_or_else(|e| panic!("{operation:?}: {e:?}"));
    assert_monotonic(&before, host);
    host.state().check_invariants().unwrap();
    result
}

fn rejected(host: &mut Authority, operation: Operation, error: Error) {
    let before = host.clone();
    let seq = host.state().seq;
    for fault in [Fault::None, Fault::BeforeCommit, Fault::LostAcknowledgement] {
        assert_eq!(
            host.commit(CommitId(seq), seq, operation.clone(), fault),
            Err(error),
            "{operation:?}, {fault:?}"
        );
        assert_eq!(*host, before, "rejection mutated state: {operation:?}");
    }
}

fn assert_monotonic(before: &Authority, after: &Authority) {
    let before = before.state();
    let after = after.state();
    assert!(after.epoch >= before.epoch);
    for (id, old) in &before.attempts {
        let new = &after.attempts[id];
        assert_eq!((new.effect, new.epoch), (old.effect, old.epoch));
        if old.stopped {
            assert!(new.stopped);
        }
        if old.receipt.is_some() {
            assert_eq!(new.receipt, old.receipt);
        }
    }
    for (id, old) in &before.intents {
        let new = &after.intents[id];
        assert_eq!(
            (new.command, new.lineage, new.base, new.recovery),
            (old.command, old.lineage, old.base, old.recovery)
        );
        if old.winner.is_some() {
            assert_eq!(new.winner, old.winner);
        }
    }
    for (id, old) in &before.submissions {
        let new = &after.submissions[id];
        assert_eq!((new.lineage, new.payload), (old.lineage, old.payload));
        if matches!(
            old.disposition,
            Disposition::Settled | Disposition::Cancelled
        ) {
            assert_eq!(new.disposition, old.disposition);
        }
    }
    for (id, revision) in &before.revisions {
        assert_eq!(after.revisions.get(id), Some(revision));
    }
    for (id, entry) in &before.commits {
        assert_eq!(after.commits.get(id), Some(entry));
    }
}

fn prepared(recovery: Recovery) -> Authority {
    let mut host = Authority::default();
    commit(&mut host, admit(0));
    commit(&mut host, apply(recovery));
    commit(&mut host, authorize(0));
    host
}

#[test]
fn late_workspace_receipt_cannot_publish_while_replacement_writer_runs() {
    let mut host = prepared(Recovery::Workspace);
    commit(&mut host, stop(0));
    commit(&mut host, authorize(1));
    commit(&mut host, Operation::Ingest(receipt(0, 9)));
    rejected(&mut host, publish(0), Error::Blocked);
    commit(&mut host, stop(1));
    commit(&mut host, publish(0));
    assert_eq!(host.state().head.workspace, 9);
}

#[test]
fn takeover_preserves_admissions_intents_and_committed_receipts() {
    let mut host = prepared(Recovery::Workspace);
    commit(&mut host, admit(1));
    commit(&mut host, Operation::Ingest(receipt(0, 9)));
    let before = host.state().clone();
    commit(&mut host, Operation::Takeover);
    let state = host.state();
    assert_eq!(state.epoch, Epoch(1));
    assert!(state.paused);
    assert_eq!(state.head, before.head);
    assert_eq!(state.submissions, before.submissions);
    assert_eq!(state.intents, before.intents);
    assert_eq!(state.attempts, before.attempts);
    assert_eq!(state.revisions, before.revisions);
    rejected(&mut host, authorize(1), Error::Blocked);
    rejected(&mut host, publish(0), Error::Blocked);
    commit(&mut host, Operation::Resume);
    rejected(&mut host, authorize(1), Error::Stale);
    rejected(&mut host, publish(0), Error::Stale);
    rejected(&mut host, recover(None), Error::Blocked);
    commit(&mut host, recover(Some(0)));
    rejected(&mut host, authorize(1), Error::Blocked);
    commit(&mut host, publish(0));
    assert_eq!(host.state().attempts.len(), 1);
    assert_eq!(host.state().intents.len(), 1);
    assert_eq!(host.state().attempts[&AttemptId(0)].epoch, Epoch(0));
    assert_eq!(host.state().intents[&EffectId(0)].epoch, Epoch(1));
    assert_eq!(
        host.state().submissions[&CommandId(1)].disposition,
        Disposition::Admitted
    );
    assert!(host.state().head.settled);
}

#[test]
fn workspace_recovery_requires_all_writers_stopped_and_preserves_old_attempt_fences() {
    let mut host = prepared(Recovery::Workspace);
    let original = host.state().head;
    commit(&mut host, Operation::Takeover);
    rejected(&mut host, recover(None), Error::Blocked);
    rejected(&mut host, recover(Some(0)), Error::Blocked);
    rejected(&mut host, Operation::Resume, Error::Blocked);
    commit(&mut host, stop(0));
    commit(&mut host, recover(None));
    assert!(host.state().paused);
    assert_eq!(host.state().head, original);
    commit(&mut host, Operation::Resume);
    commit(&mut host, authorize(1));
    commit(&mut host, Operation::Ingest(receipt(0, 8)));
    rejected(&mut host, publish(0), Error::Stale);
    commit(&mut host, Operation::Ingest(receipt(1, 9)));
    commit(&mut host, publish(1));
    assert_eq!(host.state().head.workspace, 9);
    assert_eq!(
        host.state().intents[&EffectId(0)].winner,
        Some(AttemptId(1))
    );
    assert_eq!(
        host.state().attempts[&AttemptId(0)].receipt,
        Some(receipt(0, 8))
    );
}

#[test]
fn undispatched_intents_and_admitted_work_survive_takeover() {
    for recovery in [
        Recovery::Workspace,
        Recovery::KeyedExternal,
        Recovery::OpaqueExternal,
    ] {
        let mut host = Authority::default();
        commit(&mut host, admit(0));
        commit(&mut host, Operation::Takeover);
        commit(&mut host, Operation::Resume);
        commit(&mut host, apply(recovery));
        commit(&mut host, Operation::Takeover);
        commit(&mut host, recover(None));
        commit(&mut host, Operation::Resume);
        commit(&mut host, authorize(0));
        assert_eq!(host.state().attempts[&AttemptId(0)].epoch, Epoch(2));
        assert_eq!(
            host.state().submissions[&CommandId(0)].disposition,
            Disposition::Applied
        );
    }
}

#[test]
fn opaque_uncertainty_blocks_recovery_even_after_stop_but_sealed_receipt_is_reused() {
    let mut host = prepared(Recovery::OpaqueExternal);
    commit(&mut host, Operation::Takeover);
    commit(&mut host, stop(0));
    rejected(&mut host, recover(None), Error::Blocked);
    rejected(&mut host, recover(Some(0)), Error::Blocked);
    rejected(&mut host, Operation::Resume, Error::Blocked);
    commit(&mut host, Operation::Ingest(receipt(0, 0)));
    rejected(&mut host, recover(None), Error::Blocked);
    commit(&mut host, recover(Some(0)));
    commit(&mut host, Operation::Resume);
    rejected(&mut host, authorize(1), Error::Blocked);
    commit(&mut host, publish(0));
    assert_eq!(host.state().attempts.len(), 1);
    assert!(host.state().head.settled);
}

#[test]
fn keyed_takeover_retries_only_the_original_logical_identity_and_waits_to_settle() {
    let mut host = prepared(Recovery::KeyedExternal);
    commit(&mut host, Operation::Takeover);
    rejected(&mut host, Operation::Resume, Error::Blocked);
    commit(&mut host, recover(None));
    commit(&mut host, Operation::Resume);
    rejected(
        &mut host,
        Operation::Apply {
            command: CommandId(0),
            effect: EffectId(1),
            recovery: Recovery::KeyedExternal,
        },
        Error::Stale,
    );
    commit(&mut host, authorize(1));
    commit(&mut host, Operation::Ingest(receipt(1, 0)));
    commit(&mut host, publish(1));
    let unsettled = host.state().head;
    assert!(!unsettled.settled);
    rejected(&mut host, Operation::SettleCut, Error::Blocked);
    rejected(
        &mut host,
        Operation::PrepareRewind {
            target: unsettled.id,
        },
        Error::Blocked,
    );
    commit(&mut host, stop(0));
    rejected(&mut host, Operation::SettleCut, Error::Blocked);
    commit(&mut host, Operation::Ingest(receipt(0, 0)));
    rejected(&mut host, publish(0), Error::Stale);
    commit(&mut host, Operation::SettleCut);
    assert!(host.state().head.settled);
    assert_eq!(host.state().revisions[&unsettled.id], unsettled);
    assert_eq!(
        (host.state().head.conversation, host.state().head.workspace),
        (7, 0)
    );
    let target = host.state().head.id;
    commit(&mut host, Operation::PrepareRewind { target });
    commit(&mut host, Operation::FinishRewind);
}

#[test]
fn a_late_keyed_receipt_requires_explicit_selection_not_another_retry() {
    let mut host = prepared(Recovery::KeyedExternal);
    commit(&mut host, Operation::Takeover);
    commit(&mut host, recover(None));
    commit(&mut host, Operation::Resume);
    commit(&mut host, Operation::Ingest(receipt(0, 0)));
    rejected(&mut host, publish(0), Error::Stale);
    rejected(&mut host, authorize(1), Error::Blocked);
    commit(&mut host, recover(Some(0)));
    commit(&mut host, publish(0));
    assert_eq!(host.state().attempts.len(), 1);
}

#[test]
fn workspace_receipt_recovery_still_waits_for_other_writers() {
    let mut host = prepared(Recovery::Workspace);
    commit(&mut host, stop(0));
    commit(&mut host, authorize(1));
    commit(&mut host, Operation::Takeover);
    commit(&mut host, Operation::Ingest(receipt(0, 9)));
    rejected(&mut host, recover(Some(0)), Error::Blocked);
    commit(&mut host, stop(1));
    commit(&mut host, recover(Some(0)));
    commit(&mut host, Operation::Resume);
    rejected(&mut host, authorize(2), Error::Blocked);
    commit(&mut host, publish(0));
    assert!(host.state().head.settled);
    assert_eq!(host.state().attempts.len(), 2);
}

#[test]
fn cancellation_and_rewind_cancellation_never_resurrect_pending_work() {
    let mut host = prepared(Recovery::Workspace);
    commit(&mut host, admit(1));
    let old_head = host.state().head;
    commit(&mut host, prepare_rewind());
    let pending = host.state().rewind.unwrap();
    rejected(&mut host, recover(None), Error::Blocked);
    rejected(&mut host, Operation::Resume, Error::Blocked);
    commit(&mut host, Operation::Takeover);
    assert_eq!(host.state().rewind, Some(pending));
    commit(&mut host, Operation::CancelRewind);
    let cancelled = host.state().rewind.unwrap();
    assert_eq!(cancelled.outcome, RewindOutcome::Cancelled);
    assert_eq!(
        (cancelled.target, cancelled.old_head),
        (RevisionId(0), old_head.id)
    );
    assert!(host.state().epoch > pending.epoch);
    assert_eq!(host.state().head, old_head);
    assert!(host.state().paused);
    rejected(&mut host, Operation::FinishRewind, Error::Blocked);
    rejected(&mut host, recover(None), Error::Stale);
    rejected(&mut host, Operation::Resume, Error::Blocked);
    commit(&mut host, stop(0));
    commit(&mut host, Operation::Resume);
    rejected(&mut host, authorize(1), Error::Stale);
    rejected(&mut host, apply(Recovery::Workspace), Error::Conflict);
    commit(&mut host, Operation::Ingest(receipt(0, 9)));
    rejected(&mut host, publish(0), Error::Stale);
    commit(&mut host, admit(0));
    commit(&mut host, admit(1));
    assert!(
        host.state()
            .submissions
            .values()
            .all(|s| s.disposition == Disposition::Cancelled)
    );
    assert_eq!(host.state().head, old_head);
    commit(&mut host, Operation::SettleCut);
    assert_eq!(
        (host.state().head.conversation, host.state().head.workspace),
        (old_head.conversation, old_head.workspace)
    );
    assert!(host.state().head.settled);
}

#[test]
fn cancelling_rewind_records_an_outcome_but_does_not_resolve_opaque_work() {
    let mut host = prepared(Recovery::OpaqueExternal);
    commit(&mut host, prepare_rewind());
    let old_head = host.state().head;
    commit(&mut host, Operation::CancelRewind);
    let cancelled = host.state().clone();
    commit(&mut host, Operation::CancelRewind);
    assert_eq!(host.state().epoch, cancelled.epoch);
    assert_eq!(host.state().rewind, cancelled.rewind);
    commit(&mut host, stop(0));
    rejected(&mut host, Operation::Resume, Error::Blocked);
    rejected(&mut host, recover(None), Error::Stale);
    commit(&mut host, Operation::Ingest(receipt(0, 0)));
    commit(&mut host, Operation::Resume);
    rejected(&mut host, publish(0), Error::Stale);
    assert_eq!(host.state().head, old_head);
    assert_eq!(
        host.state().submissions[&CommandId(0)].disposition,
        Disposition::Cancelled
    );
}

#[test]
fn settled_results_and_receipts_remain_monotonic_across_takeover_cancel_and_rewind() {
    let mut host = prepared(Recovery::Workspace);
    commit(&mut host, Operation::Ingest(receipt(0, 9)));
    commit(&mut host, publish(0));
    let settled = host.state().submissions.clone();
    let intent = host.state().intents.clone();
    commit(&mut host, Operation::Takeover);
    commit(&mut host, Operation::Cancel);
    assert_eq!(host.state().submissions, settled);
    assert_eq!(host.state().intents, intent);
    rejected(&mut host, recover(Some(0)), Error::Stale);
    commit(&mut host, prepare_rewind());
    commit(&mut host, Operation::FinishRewind);
    let finished = host.state().rewind.unwrap();
    assert_eq!(
        finished.outcome,
        RewindOutcome::Finished {
            revision: host.state().head.id
        }
    );
    commit(&mut host, Operation::FinishRewind);
    assert_eq!(host.state().rewind, Some(finished));
    rejected(&mut host, Operation::CancelRewind, Error::Conflict);
    commit(&mut host, Operation::Ingest(receipt(0, 9)));
    rejected(
        &mut host,
        Operation::Ingest(receipt(0, 10)),
        Error::Conflict,
    );
    commit(&mut host, Operation::Resume);
    rejected(&mut host, publish(0), Error::Stale);
    assert_eq!(host.state().submissions, settled);
    assert_eq!(host.state().intents, intent);
    assert_eq!(
        host.state().attempts[&AttemptId(0)].receipt,
        Some(receipt(0, 9))
    );
}

#[test]
fn receipt_selection_cannot_be_switched_or_rewritten_after_recovery() {
    let mut host = prepared(Recovery::KeyedExternal);
    commit(&mut host, authorize(1));
    commit(&mut host, Operation::Ingest(receipt(0, 0)));
    commit(&mut host, Operation::Ingest(receipt(1, 0)));
    commit(&mut host, Operation::Takeover);
    commit(&mut host, recover(Some(0)));
    rejected(&mut host, recover(None), Error::Conflict);
    rejected(&mut host, recover(Some(1)), Error::Conflict);
    rejected(&mut host, Operation::Ingest(receipt(0, 1)), Error::Conflict);
    commit(&mut host, Operation::Takeover);
    commit(&mut host, recover(Some(0)));
    commit(&mut host, Operation::Resume);
    rejected(&mut host, publish(1), Error::Stale);
    commit(&mut host, publish(0));
    assert_eq!(host.state().attempts[&AttemptId(0)].epoch, Epoch(0));
    assert_eq!(host.state().intents[&EffectId(0)].epoch, Epoch(2));
}

#[test]
fn invalid_lifecycle_operations_fail_without_mutation() {
    let mut host = Authority::default();
    rejected(&mut host, Operation::CancelRewind, Error::Missing);
    rejected(&mut host, Operation::FinishRewind, Error::Missing);
    rejected(&mut host, recover(None), Error::Missing);
    commit(&mut host, admit(0));
    commit(&mut host, apply(Recovery::Workspace));
    rejected(&mut host, Operation::SettleCut, Error::Blocked);
    rejected(&mut host, recover(Some(7)), Error::Missing);
    commit(&mut host, prepare_rewind());
    rejected(&mut host, prepare_rewind(), Error::Busy);
    rejected(&mut host, recover(None), Error::Blocked);
}

fn exercise_fault_schedules(trace: &[Operation]) {
    assert!(trace.len() <= MAX_COMMITS);
    let mut baseline = Authority::default();
    for operation in trace {
        commit(&mut baseline, operation.clone());
    }
    let verify = |fault_at: Option<usize>, fault: Fault| {
        let mut host = Authority::default();
        for (index, operation) in trace.iter().enumerate() {
            let seq = host.state().seq;
            let id = CommitId(seq);
            let before = host.clone();
            let selected = if fault_at.is_none() || fault_at == Some(index) {
                fault
            } else {
                Fault::None
            };
            let outcome = host.commit(id, seq, operation.clone(), selected);
            match selected {
                Fault::None => assert_eq!(outcome, Ok(seq + 1)),
                Fault::BeforeCommit => {
                    assert_eq!(outcome, Err(Error::NotCommitted));
                    assert_eq!(host, before);
                }
                Fault::LostAcknowledgement => {
                    assert_eq!(outcome, Err(Error::OutcomeUnknown));
                    let uncertain = host.clone();
                    assert_eq!(
                        host.commit(id, seq, operation.clone(), Fault::None),
                        Err(Error::StoreUnavailable)
                    );
                    assert_eq!(host, uncertain);
                }
            }
            host.recover_store();
            assert_eq!(
                host.commit(id, seq, operation.clone(), Fault::None),
                Ok(seq + 1)
            );
            let committed = host.clone();
            for retry_fault in [Fault::None, Fault::BeforeCommit, Fault::LostAcknowledgement] {
                assert_eq!(
                    host.commit(id, seq, operation.clone(), retry_fault),
                    Ok(seq + 1)
                );
                assert_eq!(host, committed);
            }
            assert_eq!(
                host.commit(id, seq + 1, operation.clone(), Fault::None),
                Err(Error::Conflict)
            );
            assert_eq!(
                host.commit(
                    id,
                    seq,
                    Operation::Admit {
                        command: CommandId(7),
                        lineage: LineageId(7),
                        payload: 255
                    },
                    Fault::None
                ),
                Err(Error::Conflict)
            );
            assert_eq!(host, committed);
            assert_monotonic(&before, &host);
            host.state().check_invariants().unwrap();
        }
        assert_eq!(
            host, baseline,
            "fault_at={fault_at:?}, fault={fault:?}, trace={trace:?}"
        );
    };
    for fault in [Fault::BeforeCommit, Fault::LostAcknowledgement] {
        verify(None, fault);
        for index in 0..trace.len() {
            verify(Some(index), fault);
        }
    }
}

#[test]
fn lifecycle_faults_and_exact_commit_retries_preserve_all_identities() {
    exercise_fault_schedules(&[
        admit(0),
        apply(Recovery::Workspace),
        authorize(0),
        Operation::Takeover,
        stop(0),
        recover(None),
        Operation::Resume,
        authorize(1),
        Operation::Ingest(receipt(1, 9)),
        publish(1),
    ]);
    exercise_fault_schedules(&[
        admit(0),
        apply(Recovery::Workspace),
        authorize(0),
        Operation::Takeover,
        Operation::Ingest(receipt(0, 9)),
        recover(Some(0)),
        Operation::Resume,
        publish(0),
        prepare_rewind(),
        Operation::FinishRewind,
    ]);
    exercise_fault_schedules(&[
        admit(0),
        apply(Recovery::KeyedExternal),
        authorize(0),
        Operation::Takeover,
        recover(None),
        Operation::Resume,
        authorize(1),
        Operation::Ingest(receipt(1, 0)),
        publish(1),
        Operation::Ingest(receipt(0, 0)),
        Operation::SettleCut,
    ]);
    exercise_fault_schedules(&[
        admit(0),
        apply(Recovery::Workspace),
        authorize(0),
        prepare_rewind(),
        Operation::Takeover,
        Operation::CancelRewind,
        stop(0),
        Operation::Resume,
        Operation::Ingest(receipt(0, 9)),
        Operation::SettleCut,
    ]);
    exercise_fault_schedules(&[
        admit(0),
        apply(Recovery::OpaqueExternal),
        authorize(0),
        Operation::Takeover,
        stop(0),
        Operation::Ingest(receipt(0, 0)),
        recover(Some(0)),
        Operation::Resume,
        publish(0),
    ]);
}
