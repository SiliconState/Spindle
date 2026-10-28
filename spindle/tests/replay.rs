use spindle_reference::replay::{MODEL_RUNTIME, ReplayError, reconstruct};
use spindle_reference::*;

fn trace() -> Authority {
    let mut host = Authority::default();
    let operations = [
        Operation::Admit {
            command: CommandId(0),
            lineage: LineageId(0),
            payload: 42,
        },
        Operation::Apply {
            command: CommandId(0),
            effect: EffectId(0),
            recovery: Recovery::Workspace,
        },
        Operation::Authorize {
            effect: EffectId(0),
            attempt: AttemptId(0),
        },
        Operation::Ingest(Receipt {
            effect: EffectId(0),
            attempt: AttemptId(0),
            result: 7,
            workspace: 9,
        }),
        Operation::Publish {
            attempt: AttemptId(0),
        },
        Operation::PrepareRewind {
            target: RevisionId(0),
        },
        Operation::FinishRewind,
    ];
    for (index, operation) in operations.into_iter().enumerate() {
        // Commit identities are deliberately not sequence positions.
        host.commit(
            CommitId(100 - index as u8),
            index as u8,
            operation,
            Fault::None,
        )
        .unwrap();
        assert_eq!(
            reconstruct(MODEL_SCHEMA, MODEL_RUNTIME, host.state()).unwrap(),
            *host.state()
        );
    }
    host
}

#[test]
fn invariant_check_rejects_missing_references_without_panicking() {
    let mut state = Authority::default().state().clone();
    state.attempts.insert(
        AttemptId(0),
        Attempt {
            effect: EffectId(0),
            epoch: Epoch(0),
            stopped: true,
            receipt: None,
        },
    );
    let checked = std::panic::catch_unwind(|| state.check_invariants());
    assert!(
        checked.is_ok(),
        "missing intent caused invariant checker to panic"
    );
    assert!(checked.unwrap().is_err());
}

#[test]
fn invariant_check_detects_duplicate_commit_positions() {
    let mut state = trace().state().clone();
    state.commits.get_mut(&CommitId(99)).unwrap().seq = 1;
    assert!(state.check_invariants().is_err());
}

#[test]
fn reconstructs_all_cuts_without_live_authority_or_external_dispatch() {
    let host = trace();
    let rebuilt = reconstruct(MODEL_SCHEMA, MODEL_RUNTIME, host.state()).unwrap();
    assert_eq!(rebuilt.head.lineage, LineageId(1));
    assert_eq!((rebuilt.head.conversation, rebuilt.head.workspace), (0, 0));
    assert_eq!(rebuilt.attempts.len(), 1);
    assert_eq!(host.state(), &rebuilt);
}

#[test]
fn refuses_incompatible_runtime_and_schema() {
    let host = trace();
    assert_eq!(
        reconstruct(MODEL_SCHEMA + 1, MODEL_RUNTIME, host.state()),
        Err(ReplayError::Schema)
    );
    assert_eq!(
        reconstruct(MODEL_SCHEMA, "changed-engine", host.state()),
        Err(ReplayError::Runtime)
    );
}

#[test]
fn refuses_missing_duplicate_and_reordered_commit_sequences() {
    let host = trace();
    for id in host.state().commits.keys() {
        let mut damaged = host.state().clone();
        damaged.commits.remove(id);
        assert!(reconstruct(MODEL_SCHEMA, MODEL_RUNTIME, &damaged).is_err());
        let mut damaged = host.state().clone();
        damaged.commits.get_mut(id).unwrap().seq = 1;
        if *id != CommitId(100) {
            assert_eq!(
                reconstruct(MODEL_SCHEMA, MODEL_RUNTIME, &damaged),
                Err(ReplayError::Sequence)
            );
        }
    }
    let mut damaged = host.state().clone();
    damaged.commits.get_mut(&CommitId(99)).unwrap().expected_seq = 0;
    assert_eq!(
        reconstruct(MODEL_SCHEMA, MODEL_RUNTIME, &damaged),
        Err(ReplayError::Sequence)
    );
}

#[test]
fn rejects_changed_payload_results_and_reference_closure() {
    let host = trace();
    let mut damaged = host.state().clone();
    damaged.submissions.get_mut(&CommandId(0)).unwrap().payload ^= 1;
    assert_eq!(
        reconstruct(MODEL_SCHEMA, MODEL_RUNTIME, &damaged),
        Err(ReplayError::Divergence)
    );
    let mut damaged = host.state().clone();
    damaged.commits.get_mut(&CommitId(97)).unwrap().operation = Operation::Ingest(Receipt {
        effect: EffectId(0),
        attempt: AttemptId(0),
        result: 8,
        workspace: 9,
    });
    assert_eq!(
        reconstruct(MODEL_SCHEMA, MODEL_RUNTIME, &damaged),
        Err(ReplayError::Divergence)
    );
    let mut damaged = host.state().clone();
    damaged.revisions.remove(&RevisionId(0));
    assert_eq!(
        reconstruct(MODEL_SCHEMA, MODEL_RUNTIME, &damaged),
        Err(ReplayError::Divergence)
    );
    let mut damaged = host.state().clone();
    damaged.commits.get_mut(&CommitId(100)).unwrap().operation = Operation::Publish {
        attempt: AttemptId(0),
    };
    assert!(matches!(
        reconstruct(MODEL_SCHEMA, MODEL_RUNTIME, &damaged),
        Err(ReplayError::Rejected { .. })
    ));
}

#[test]
fn trusted_checkpoint_detects_tail_truncation_but_not_a_different_valid_history() {
    let host = trace();
    let mut damaged = host.state().clone();
    damaged.commits.remove(&CommitId(94));
    assert_eq!(
        reconstruct(MODEL_SCHEMA, MODEL_RUNTIME, &damaged),
        Err(ReplayError::Divergence)
    );
    // An internally valid empty realm cannot be distinguished from rollback without
    // an independently trusted retained head. Replay intentionally makes no such claim.
    let empty = Authority::default();
    assert!(reconstruct(MODEL_SCHEMA, MODEL_RUNTIME, empty.state()).is_ok());
}
