use spindle_reference::*;
use std::collections::BTreeMap;

#[derive(Default)]
struct Oracle {
    admitted: BTreeMap<CommandId, (LineageId, u8)>,
    receipts: BTreeMap<AttemptId, Receipt>,
    winners: BTreeMap<EffectId, AttemptId>,
    roots: BTreeMap<RevisionId, (u8, u8)>,
}

impl Oracle {
    fn observe(&mut self, before: &State, after: &State, operation: &Operation) {
        for (id, submission) in &after.submissions {
            let identity = (submission.lineage, submission.payload);
            assert_eq!(*self.admitted.entry(*id).or_insert(identity), identity);
        }
        for (id, attempt) in &after.attempts {
            assert!(before.intents.contains_key(&attempt.effect));
            if let Some(receipt) = attempt.receipt {
                assert_eq!(*self.receipts.entry(*id).or_insert(receipt), receipt);
            }
        }
        for (id, intent) in &after.intents {
            if let Some(winner) = intent.winner {
                assert_eq!(*self.winners.entry(*id).or_insert(winner), winner);
            }
        }
        for (id, revision) in &after.revisions {
            let pair = (revision.conversation, revision.workspace);
            assert_eq!(*self.roots.entry(*id).or_insert(pair), pair);
        }
        let old_pair = (before.head.conversation, before.head.workspace);
        let new_pair = (after.head.conversation, after.head.workspace);
        match operation {
            Operation::Publish { attempt } if before.head.id != after.head.id => {
                let sealed = before.attempts[attempt]
                    .receipt
                    .expect("publish before ingress");
                assert_eq!(new_pair, (sealed.result, sealed.workspace));
                let intent = &before.intents[&sealed.effect];
                assert_eq!(intent.lineage, before.head.lineage);
                assert_eq!(intent.epoch, before.epoch);
                assert!(!before.paused);
            }
            Operation::FinishRewind if before.head.id != after.head.id => {
                let target = before.revisions[&before.rewind.unwrap().target];
                assert_eq!(new_pair, (target.conversation, target.workspace));
                assert_eq!(after.head.lineage.0, before.head.lineage.0 + 1);
                assert!(after.paused);
            }
            _ => assert_eq!(
                new_pair, old_pair,
                "nonpublication changed roots: {operation:?}"
            ),
        }
        for (effect, attempt) in &self.winners {
            assert_eq!(after.intents[effect].winner, Some(*attempt));
        }
        for (attempt, receipt) in &self.receipts {
            assert_eq!(after.attempts[attempt].receipt, Some(*receipt));
        }
    }
}

#[test]
fn independent_oracle_checks_publication_and_monotonic_history_under_faults() {
    for recovery in [
        Recovery::Workspace,
        Recovery::KeyedExternal,
        Recovery::OpaqueExternal,
    ] {
        let workspace = if recovery == Recovery::Workspace {
            9
        } else {
            0
        };
        let trace = [
            Operation::Admit {
                command: CommandId(0),
                lineage: LineageId(0),
                payload: 42,
            },
            Operation::Apply {
                command: CommandId(0),
                effect: EffectId(0),
                recovery,
            },
            Operation::Authorize {
                effect: EffectId(0),
                attempt: AttemptId(0),
            },
            Operation::Ingest(Receipt {
                effect: EffectId(0),
                attempt: AttemptId(0),
                result: 7,
                workspace,
            }),
            Operation::Takeover,
            Operation::RecoverIntent {
                effect: EffectId(0),
                receipt: Some(AttemptId(0)),
            },
            Operation::Resume,
            Operation::Publish {
                attempt: AttemptId(0),
            },
            Operation::PrepareRewind {
                target: RevisionId(0),
            },
            Operation::FinishRewind,
        ];
        for boundary in 0..trace.len() {
            for fault in [Fault::BeforeCommit, Fault::LostAcknowledgement] {
                let mut host = Authority::default();
                let mut oracle = Oracle::default();
                for (index, operation) in trace.iter().enumerate() {
                    let before = host.state().clone();
                    let selected = if index == boundary {
                        fault
                    } else {
                        Fault::None
                    };
                    let result = host.commit(
                        CommitId(index as u8),
                        index as u8,
                        operation.clone(),
                        selected,
                    );
                    match selected {
                        Fault::None => {
                            result.unwrap();
                        }
                        Fault::BeforeCommit => {
                            assert_eq!(result, Err(Error::NotCommitted));
                            assert_eq!(*host.state(), before);
                        }
                        Fault::LostAcknowledgement => {
                            assert_eq!(result, Err(Error::OutcomeUnknown))
                        }
                    }
                    host.recover_store();
                    host.commit(
                        CommitId(index as u8),
                        index as u8,
                        operation.clone(),
                        Fault::None,
                    )
                    .unwrap();
                    oracle.observe(&before, host.state(), operation);
                }
                assert_eq!(oracle.winners.len(), 1);
                assert_eq!(oracle.receipts.len(), 1);
                assert_eq!(oracle.admitted.len(), 1);
            }
        }
    }
}
