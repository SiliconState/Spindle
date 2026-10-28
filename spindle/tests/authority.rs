use spindle_reference::*;

fn admit() -> Operation {
    Operation::Admit {
        command: CommandId(0),
        lineage: LineageId(0),
        payload: 42,
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
fn commit(host: &mut Authority, operation: Operation) -> Result<u8, Error> {
    let seq = host.state().seq;
    let result = host.commit(CommitId(seq), seq, operation, Fault::None);
    host.state().check_invariants().unwrap();
    result
}
fn prepared(recovery: Recovery) -> Authority {
    let mut host = Authority::default();
    commit(&mut host, admit()).unwrap();
    commit(&mut host, apply(recovery)).unwrap();
    commit(&mut host, authorize(0)).unwrap();
    host
}

#[test]
fn receipt_ingress_is_not_publication_and_duplicate_publication_is_noop() {
    let mut host = prepared(Recovery::Workspace);
    let pinned = host.state().head;
    commit(&mut host, Operation::Ingest(receipt(0, 9))).unwrap();
    assert_eq!(host.state().head, pinned);
    host.recover_store();
    assert_eq!(commit(&mut host, authorize(1)), Err(Error::Blocked));
    commit(
        &mut host,
        Operation::Publish {
            attempt: AttemptId(0),
        },
    )
    .unwrap();
    let published = host.state().head;
    assert_eq!((published.conversation, published.workspace), (7, 9));
    assert_eq!(host.state().revisions[&pinned.id], pinned);
    commit(
        &mut host,
        Operation::Publish {
            attempt: AttemptId(0),
        },
    )
    .unwrap();
    assert_eq!(host.state().head, published);
    assert_eq!(
        host.state().intents[&EffectId(0)].winner,
        Some(AttemptId(0))
    );
}

#[test]
fn identity_conflicts_never_mutate_state() {
    let mut host = prepared(Recovery::Workspace);
    let operations = [
        Operation::Admit {
            command: CommandId(0),
            lineage: LineageId(0),
            payload: 43,
        },
        Operation::Admit {
            command: CommandId(0),
            lineage: LineageId(1),
            payload: 42,
        },
        Operation::Ingest(Receipt {
            effect: EffectId(1),
            ..receipt(0, 9)
        }),
    ];
    for op in operations {
        let before = host.clone();
        assert_eq!(commit(&mut host, op), Err(Error::Conflict));
        assert_eq!(host, before);
    }
    commit(&mut host, Operation::Ingest(receipt(0, 9))).unwrap();
    let before = host.clone();
    assert_eq!(
        commit(&mut host, Operation::Ingest(receipt(0, 10))),
        Err(Error::Conflict)
    );
    assert_eq!(host, before);
    commit(&mut host, Operation::Ingest(receipt(0, 9))).unwrap();
    assert_eq!(host.state().attempts.len(), 1);
}

#[test]
fn fault_every_boundary_of_admit_publish_rewind_trace() {
    let trace = [
        admit(),
        apply(Recovery::Workspace),
        authorize(0),
        Operation::Ingest(receipt(0, 9)),
        Operation::Publish {
            attempt: AttemptId(0),
        },
        Operation::PrepareRewind {
            target: RevisionId(0),
        },
        Operation::FinishRewind,
    ];
    let mut baseline = Authority::default();
    for op in &trace {
        commit(&mut baseline, op.clone()).unwrap();
    }
    // Every combination of no fault, proven rollback and lost commit reply.
    for schedule in 0..3usize.pow(trace.len() as u32) {
        let mut choices = schedule;
        let mut host = Authority::default();
        for (index, op) in trace.iter().enumerate() {
            let fault = [Fault::None, Fault::BeforeCommit, Fault::LostAcknowledgement][choices % 3];
            choices /= 3;
            let before = host.clone();
            let seq = host.state().seq;
            let id = CommitId(index as u8);
            let result = host.commit(id, seq, op.clone(), fault);
            match fault {
                Fault::None => {
                    result.unwrap();
                }
                Fault::BeforeCommit => {
                    assert_eq!(
                        result,
                        Err(Error::NotCommitted),
                        "schedule {schedule}, step {index}"
                    );
                    assert_eq!(host, before);
                }
                Fault::LostAcknowledgement => {
                    assert_eq!(result, Err(Error::OutcomeUnknown));
                    assert_eq!(
                        host.commit(id, seq, op.clone(), Fault::None),
                        Err(Error::StoreUnavailable)
                    );
                }
            }
            host.recover_store();
            host.commit(id, seq, op.clone(), Fault::None).unwrap();
            host.state().check_invariants().unwrap();
        }
        assert_eq!(host, baseline, "schedule {schedule}: {trace:?}");
    }
    assert_eq!(baseline.state().head.lineage, LineageId(1));
    assert_eq!(
        (
            baseline.state().head.conversation,
            baseline.state().head.workspace
        ),
        (0, 0)
    );
    assert!(baseline.state().paused);
    assert_eq!(
        baseline.state().submissions[&CommandId(0)].disposition,
        Disposition::Settled
    );
    assert_eq!(
        baseline.state().attempts[&AttemptId(0)].receipt,
        Some(receipt(0, 9))
    );
}

#[test]
fn ambiguous_commit_requires_resolution_and_exact_identity() {
    let mut host = Authority::default();
    assert_eq!(
        host.commit(CommitId(4), 0, admit(), Fault::LostAcknowledgement),
        Err(Error::OutcomeUnknown)
    );
    assert_eq!(
        commit(&mut host, apply(Recovery::Workspace)),
        Err(Error::StoreUnavailable)
    );
    host.recover_store();
    assert_eq!(host.commit(CommitId(4), 0, admit(), Fault::None), Ok(1));
    assert_eq!(
        host.commit(CommitId(4), 1, admit(), Fault::None),
        Err(Error::Conflict)
    );
    assert_eq!(
        host.commit(CommitId(4), 0, Operation::Cancel, Fault::None),
        Err(Error::Conflict)
    );
    assert_eq!(
        host.commit(CommitId(5), 0, Operation::Cancel, Fault::None),
        Err(Error::Stale)
    );
}

#[test]
fn recovery_contracts_are_not_interchangeable() {
    for recovery in [
        Recovery::Workspace,
        Recovery::OpaqueExternal,
        Recovery::KeyedExternal,
    ] {
        let mut host = prepared(recovery);
        let retry = commit(&mut host, authorize(1));
        if recovery == Recovery::KeyedExternal {
            retry.unwrap();
        } else {
            assert_eq!(retry, Err(Error::Blocked));
        }
        commit(
            &mut host,
            Operation::ConfirmStopped {
                attempt: AttemptId(0),
            },
        )
        .unwrap();
        let retry = commit(&mut host, authorize(2));
        if recovery == Recovery::OpaqueExternal {
            assert_eq!(retry, Err(Error::Blocked));
        } else {
            retry.unwrap();
        }
    }
}

#[test]
fn competing_keyed_attempts_have_only_one_winner() {
    let mut host = prepared(Recovery::KeyedExternal);
    commit(&mut host, authorize(1)).unwrap();
    commit(&mut host, Operation::Ingest(receipt(1, 0))).unwrap();
    commit(
        &mut host,
        Operation::Publish {
            attempt: AttemptId(1),
        },
    )
    .unwrap();
    commit(&mut host, Operation::Ingest(receipt(0, 0))).unwrap();
    let before = host.clone();
    assert_eq!(
        commit(
            &mut host,
            Operation::Publish {
                attempt: AttemptId(0)
            }
        ),
        Err(Error::Conflict)
    );
    assert_eq!(host, before);
}

#[test]
fn cancel_and_takeover_fence_publication_but_keep_late_receipts() {
    for fence in [Operation::Cancel, Operation::Takeover] {
        let mut host = prepared(Recovery::Workspace);
        commit(&mut host, fence.clone()).unwrap();
        let head = host.state().head;
        commit(&mut host, Operation::Ingest(receipt(0, 9))).unwrap();
        assert_eq!(
            commit(
                &mut host,
                Operation::Publish {
                    attempt: AttemptId(0)
                }
            ),
            Err(Error::Blocked)
        );
        commit(&mut host, Operation::Resume).unwrap();
        assert_eq!(
            commit(
                &mut host,
                Operation::Publish {
                    attempt: AttemptId(0)
                }
            ),
            Err(Error::Stale)
        );
        assert_eq!(host.state().head, head);
        assert_eq!(
            host.state().submissions[&CommandId(0)].disposition,
            if fence == Operation::Takeover {
                Disposition::Applied
            } else {
                Disposition::Cancelled
            }
        );
        assert!(host.state().attempts[&AttemptId(0)].receipt.is_some());
    }
}

#[test]
fn rewind_waits_for_writers_and_preserves_command_tombstones() {
    let mut host = prepared(Recovery::Workspace);
    let old_head = host.state().head;
    commit(
        &mut host,
        Operation::PrepareRewind {
            target: RevisionId(0),
        },
    )
    .unwrap();
    assert_eq!(host.state().head, old_head);
    assert_eq!(
        commit(&mut host, Operation::FinishRewind),
        Err(Error::Blocked)
    );
    commit(
        &mut host,
        Operation::ConfirmStopped {
            attempt: AttemptId(0),
        },
    )
    .unwrap();
    commit(&mut host, Operation::FinishRewind).unwrap();
    assert!(host.state().paused);
    commit(&mut host, admit()).unwrap();
    assert_eq!(
        host.state().submissions[&CommandId(0)].disposition,
        Disposition::Cancelled
    );
    commit(&mut host, Operation::Resume).unwrap();
    assert_eq!(host.state().submissions.len(), 1);
    let mut host = Authority::default();
    commit(
        &mut host,
        Operation::PrepareRewind {
            target: RevisionId(0),
        },
    )
    .unwrap();
    commit(&mut host, Operation::FinishRewind).unwrap();
    commit(&mut host, Operation::Resume).unwrap();
    assert_eq!(commit(&mut host, admit()), Err(Error::Stale));
}

#[test]
fn opaque_uncertainty_survives_stop_and_blocks_rewind() {
    let mut host = prepared(Recovery::OpaqueExternal);
    commit(
        &mut host,
        Operation::PrepareRewind {
            target: RevisionId(0),
        },
    )
    .unwrap();
    commit(
        &mut host,
        Operation::ConfirmStopped {
            attempt: AttemptId(0),
        },
    )
    .unwrap();
    let before = host.clone();
    assert_eq!(
        commit(&mut host, Operation::FinishRewind),
        Err(Error::Blocked)
    );
    assert_eq!(host, before);
}

#[test]
fn nonsettled_cut_and_uncommitted_intents_are_refused() {
    let mut host = Authority::default();
    assert_eq!(commit(&mut host, authorize(0)), Err(Error::Missing));
    commit(&mut host, admit()).unwrap();
    let before = host.clone();
    assert_eq!(
        host.commit(
            CommitId(1),
            1,
            apply(Recovery::Workspace),
            Fault::BeforeCommit
        ),
        Err(Error::NotCommitted)
    );
    assert_eq!(host, before);
    assert_eq!(commit(&mut host, authorize(0)), Err(Error::Missing));
    commit(&mut host, apply(Recovery::Workspace)).unwrap();
    let cut = host.state().head.id;
    assert_eq!(
        commit(&mut host, Operation::PrepareRewind { target: cut }),
        Err(Error::Blocked)
    );
}

#[test]
fn quotas_fail_atomically_but_existing_commit_receipts_remain_queryable() {
    let mut host = Authority::default();
    for _ in 0..MAX_COMMITS {
        commit(&mut host, admit()).unwrap();
    }
    let before = host.clone();
    assert_eq!(commit(&mut host, Operation::Cancel), Err(Error::Exhausted));
    assert_eq!(host, before);
    assert_eq!(host.commit(CommitId(0), 0, admit(), Fault::None), Ok(1));
}

#[test]
fn exhaustively_explore_short_operation_orders_with_faults() {
    let alphabet = [
        admit(),
        apply(Recovery::Workspace),
        authorize(0),
        Operation::Ingest(receipt(0, 9)),
        Operation::Publish {
            attempt: AttemptId(0),
        },
        Operation::Cancel,
        Operation::Takeover,
        Operation::RecoverIntent {
            effect: EffectId(0),
            receipt: None,
        },
        Operation::SettleCut,
        Operation::PrepareRewind {
            target: RevisionId(0),
        },
        Operation::FinishRewind,
        Operation::CancelRewind,
        Operation::Resume,
    ];
    let mut checked = 0usize;
    fn explore(
        host: Authority,
        trace: &mut Vec<Operation>,
        alphabet: &[Operation],
        checked: &mut usize,
    ) {
        if trace.len() == 5 {
            return;
        }
        for op in alphabet {
            trace.push(op.clone());
            let seq = host.state().seq;
            let id = CommitId(seq);
            let mut normal = host.clone();
            let outcome = normal.commit(id, seq, op.clone(), Fault::None);
            normal
                .state()
                .check_invariants()
                .unwrap_or_else(|e| panic!("{e}: {trace:?}"));
            if outcome.is_err() {
                assert_eq!(normal, host, "rejected: {trace:?}");
            }
            for fault in [Fault::BeforeCommit, Fault::LostAcknowledgement] {
                let mut faulty = host.clone();
                let result = faulty.commit(id, seq, op.clone(), fault);
                if outcome.is_ok() {
                    if fault == Fault::BeforeCommit {
                        assert_eq!(result, Err(Error::NotCommitted));
                        assert_eq!(faulty, host, "rollback: {trace:?}");
                    } else {
                        assert_eq!(result, Err(Error::OutcomeUnknown));
                    }
                    faulty.recover_store();
                    faulty.commit(id, seq, op.clone(), Fault::None).unwrap();
                    assert_eq!(faulty, normal, "recovery: {trace:?}");
                } else {
                    assert_eq!(result, outcome);
                    assert_eq!(faulty, host);
                }
                *checked += 1;
            }
            explore(normal, trace, alphabet, checked);
            trace.pop();
        }
    }
    explore(
        Authority::default(),
        &mut Vec::new(),
        &alphabet,
        &mut checked,
    );
    assert_eq!(checked, 804_466);
    eprintln!(
        "checked {checked} faulted edges; alphabet={}, depth=5, schema={MODEL_SCHEMA}",
        alphabet.len()
    );
}
