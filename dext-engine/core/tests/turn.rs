use dext_core::recovery::{Decision, HaltReason, Observation, RecoveryState};
use dext_core::turn::{Phase, Stop, TurnControl};

fn restored(state: TurnControl) -> TurnControl {
    let bytes = state.checkpoint();
    let decoded = TurnControl::restore(&bytes).unwrap();
    assert_eq!(decoded, state);
    decoded
}
fn waiting(state: TurnControl) -> TurnControl {
    restored(
        restored(state.begin_round(None).unwrap())
            .prepare_request()
            .unwrap(),
    )
}

#[test]
fn complete_requires_final_policy_review_and_tools_require_result_adoption() {
    let initial = restored(TurnControl::default());
    assert!(initial.finish().is_err());
    assert!(initial.tools_applied().is_err());
    let wait = waiting(initial);
    let (tools, decision) = wait.response(None, true).unwrap();
    assert_eq!(decision, Decision::Proceed);
    let tools = restored(tools);
    assert_eq!(tools.phase(), Phase::ToolsWait);
    assert!(tools.finish().is_err());
    assert!(tools.begin_round(None).is_err());
    assert!(tools.prepare_request().is_err());
    let after = restored(tools.tools_applied().unwrap());
    let wait = waiting(after);
    let (review, _) = wait.response(None, false).unwrap();
    let review = restored(review);
    assert_eq!(review.phase(), Phase::FinalReview);
    assert!(review.begin_round(None).is_err());
    let followup = restored(review.final_followup().unwrap());
    assert_eq!(followup.iterations(), 2);
    let wait = waiting(followup);
    let (review, _) = wait.response(None, false).unwrap();
    let done = restored(review.finish().unwrap());
    assert_eq!(done.phase(), Phase::Stopped(Stop::Complete));
    assert!(done.begin_round(None).is_err());
    assert!(done.prepare_request().is_err());
    assert!(done.reset_recovery().is_err());
    assert!(done.final_followup().is_err());
}

#[test]
fn free_recovery_rounds_and_runtime_controls_preserve_legacy_counters() {
    let mut state = TurnControl::default();
    for ordinal in 1..=3 {
        let wait = waiting(state);
        let baseline = wait.checkpoint();
        let (next, decision) = wait.response(Some("output_token_limit"), false).unwrap();
        assert_eq!(wait.checkpoint(), baseline);
        assert_eq!(decision, Decision::Retry { ordinal });
        state = restored(next);
        assert_eq!(state.iterations(), 0);
        assert_eq!(state.request_ordinal(), ordinal as u64);
    }
    let (halted, decision) = waiting(state)
        .response(Some("output_token_limit"), false)
        .unwrap();
    assert_eq!(decision, Decision::Halt(HaltReason::RetryBudget));
    assert_eq!(
        restored(halted).phase(),
        Phase::Stopped(Stop::RecoveryBudget)
    );
    let controlled = restored(waiting(state).runtime_control().unwrap());
    assert_eq!(
        controlled.iterations(),
        1,
        "runtime control consumes a round, unlike incomplete-response recovery"
    );
    assert_eq!(controlled.recovery(), RecoveryState::default());
    let (tools, decision) = waiting(state)
        .response(Some("content_filter"), true)
        .unwrap();
    assert_eq!(
        decision,
        Decision::Proceed,
        "a separately truncated call does not discard completed calls"
    );
    assert_eq!(restored(tools).phase(), Phase::ToolsWait);
    let (halted, decision) = waiting(TurnControl::default())
        .response(Some("content_filter"), false)
        .unwrap();
    assert_eq!(decision, Decision::Halt(HaltReason::ContentFilter));
    restored(halted);
}

#[test]
fn round_limits_request_ordinals_and_overflow_refuse_dispatch() {
    assert_eq!(
        restored(TurnControl::default().begin_round(Some(0)).unwrap()).phase(),
        Phase::Stopped(Stop::Iterations)
    );
    let wait = waiting(TurnControl::default());
    let rebuilt = restored(wait.prepare_request().unwrap());
    assert_eq!(rebuilt.iterations(), 1);
    assert_eq!(rebuilt.request_ordinal(), 2);
    let (tools, _) = rebuilt.response(None, true).unwrap();
    let after = restored(tools.tools_applied().unwrap());
    assert_eq!(
        restored(after.begin_round(Some(1)).unwrap()).phase(),
        Phase::Stopped(Stop::Iterations)
    );
    let limited = restored(after.begin_round(Some(1)).unwrap());
    let extended = restored(limited.extend_iterations().unwrap());
    assert_eq!((extended.phase(), extended.iterations()), (Phase::Ready, 0));
    assert_eq!(extended.request_ordinal(), limited.request_ordinal());
    assert!(after.extend_iterations().is_err());
    assert!(extended.extend_iterations().is_err());
    let mut bytes = after.checkpoint();
    bytes[5..9].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(
        TurnControl::restore(&bytes)
            .unwrap()
            .begin_round(None)
            .is_err()
    );
    let mut bytes = rebuilt.checkpoint();
    bytes[9..17].copy_from_slice(&u64::MAX.to_be_bytes());
    assert!(
        TurnControl::restore(&bytes)
            .unwrap()
            .prepare_request()
            .is_err()
    );
}

#[test]
fn illegal_checkpoint_states_and_terminal_transitions_fail_closed() {
    let wait = waiting(TurnControl::default());
    let bytes = wait.checkpoint();
    for end in 0..bytes.len() {
        assert!(TurnControl::restore(&bytes[..end]).is_err());
    }
    let mut trailing = bytes.to_vec();
    trailing.push(0);
    assert!(TurnControl::restore(&trailing).is_err());
    for phase in 0..=u8::MAX {
        for recovery_tag in 0..=4 {
            for recovery_count in 0..=4 {
                let mut candidate = bytes;
                candidate[4] = phase;
                candidate[21] = recovery_tag;
                candidate[22] = recovery_count;
                if let Ok(state) = TurnControl::restore(&candidate) {
                    assert_eq!(state.checkpoint(), candidate);
                }
            }
        }
    }
    for phase in [2, 3, 4, 5, 6, 9, 10, 11] {
        let mut candidate = bytes;
        candidate[4] = phase;
        candidate[9..17].fill(0);
        assert!(TurnControl::restore(&candidate).is_err());
    }
    assert!(TurnControl::default().stop(Stop::ContentFilter).is_err());
    assert!(TurnControl::default().stop(Stop::RecoveryBudget).is_err());
    assert!(TurnControl::default().stop(Stop::ToolPolicy).is_err());
}

#[test]
fn bounded_transition_schedules_roundtrip_and_match_prior_recovery_decisions() {
    fn explore(state: TurnControl, depth: u8) {
        let state = restored(state);
        if depth == 0 {
            return;
        }
        let baseline = state.checkpoint();
        let mut next = Vec::new();
        next.extend(state.begin_round(Some(3)));
        next.extend(state.prepare_request());
        next.extend(state.runtime_control());
        next.extend(state.reset_recovery());
        next.extend(state.tools_applied());
        next.extend(state.final_followup());
        next.extend(state.finish());
        next.extend(state.stop(Stop::Budget));
        next.extend(state.stop(Stop::Cancelled));
        next.extend(state.stop(Stop::ToolPolicy));
        for reason in [None, Some("output_token_limit"), Some("content_filter")] {
            for calls in [false, true] {
                if let Ok((candidate, decision)) = state.response(reason, calls) {
                    let legacy = state.recovery().advance(Observation::Response {
                        incomplete_reason: reason,
                        executable_calls: calls,
                    });
                    assert_eq!(decision, legacy.decision);
                    assert_eq!(candidate.recovery(), legacy.next);
                    next.push(candidate);
                }
            }
        }
        assert_eq!(state.checkpoint(), baseline);
        for candidate in next {
            explore(candidate, depth - 1);
        }
    }
    explore(TurnControl::default(), 6);
}
