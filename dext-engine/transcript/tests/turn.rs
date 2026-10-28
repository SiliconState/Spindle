use dext_core::model::{Encoding, Identity, ModelWait, PreparedRequest};
use dext_core::recovery::Contract;
use dext_core::turn::{Phase, Stop};
use dext_transcript::checkpoint::{NativeResponse, TranscriptCheckpoint};
use dext_transcript::turn::{Effect, TurnContinuation};
use dext_transcript::{ArtifactImage, Block, Usage};

fn initial(limit: Option<u32>) -> TurnContinuation {
    TurnContinuation::new(
        "operator".into(),
        "lineage-1/turn-1".into(),
        Contract::OpenAiResponses,
        limit,
        TranscriptCheckpoint {
            route_binding: "route-v1".into(),
            messages: vec![],
        },
    )
    .unwrap()
}
fn restore(state: &TurnContinuation) -> TurnContinuation {
    let bytes = state.checkpoint().unwrap();
    let restored = TurnContinuation::restore(&bytes).unwrap();
    assert_eq!(restored.checkpoint().unwrap(), bytes);
    restored
}
fn prepare(state: &TurnContinuation) -> TurnContinuation {
    let request = PreparedRequest::new(
        Identity {
            effect: state.next_model_id().unwrap(),
            provider: "fake".into(),
            contract: "openai-responses".into(),
            endpoint: "https://example.invalid/responses".into(),
            model: "fake-model".into(),
            credential_scope: "operator/credential-version".into(),
            configuration: "policy-v1".into(),
        },
        Encoding::Json,
        b"frozen native request".to_vec(),
    )
    .unwrap();
    state.prepare(request, "route-v1").unwrap()
}
fn response(state: &TurnContinuation, calls: usize, reason: &str) -> ModelWait<Vec<u8>> {
    let Effect::Model { waiting, .. } = state.pending_effects().unwrap().remove(0) else {
        panic!("model wait");
    };
    let mut blocks = vec![Block::Text {
        text: "assistant response".into(),
    }];
    for i in 0..calls {
        blocks.push(Block::ToolUse {
            id: format!("provider-{i}"),
            name: "inert-tool".into(),
            input: serde_json::json!({"position": i}),
        });
    }
    let bytes = NativeResponse {
        blocks,
        stop_reason: Some(reason.into()),
        usage: Usage::default(),
        unknown_events: 0,
        unfinished_tool_calls: 0,
    }
    .checkpoint()
    .unwrap();
    waiting
        .record(waiting.request(), waiting.attempt(), &bytes)
        .unwrap()
}
fn result(call_id: &str) -> Vec<Block> {
    vec![Block::ToolResult {
        tool_use_id: call_id.into(),
        content: "saved result".into(),
        is_error: Some(false),
        metadata: Default::default(),
    }]
}

#[test]
fn model_tool_model_review_completion_survives_every_checkpoint_with_stable_effects() {
    let baseline = initial(Some(4));
    let waiting = restore(&prepare(&baseline));
    assert_eq!(baseline.control().phase(), Phase::Ready);
    assert!(waiting.finish().is_err());
    assert!(waiting.next_model_id().is_err());
    let saved = response(&waiting, 2, "tool_use");
    assert!(waiting.apply_model(&saved, "wrong-route").is_err());
    let tools = restore(&waiting.apply_model(&saved, "route-v1").unwrap());
    assert_eq!(tools.control().phase(), Phase::ToolsWait);
    assert!(tools.finish().is_err());
    let effects = tools.pending_effects().unwrap();
    let Effect::Tool {
        id: first_id,
        call: first,
    } = &effects[0]
    else {
        panic!("tool");
    };
    let Effect::Tool {
        id: second_id,
        call: second,
    } = &effects[1]
    else {
        panic!("tool");
    };
    assert_eq!(first_id, "lineage-1/turn-1/m/1/t/0");
    assert_eq!(second_id, "lineage-1/turn-1/m/1/t/1");
    assert_ne!(first_id, &first.id);
    assert!(tools.apply_tool(&first.id, result(&first.id)).is_err());
    let partial = restore(&tools.apply_tool(second_id, result(&second.id)).unwrap());
    assert_eq!(partial.transcript().unwrap().messages.len(), 1);
    assert_eq!(partial.pending_effects().unwrap().len(), 1);
    assert_eq!(
        partial
            .apply_tool(second_id, result(&second.id))
            .unwrap()
            .checkpoint()
            .unwrap(),
        partial.checkpoint().unwrap()
    );
    let after = restore(&partial.apply_tool(first_id, result(&first.id)).unwrap());
    assert_eq!(after.control().phase(), Phase::AfterTools);
    assert!(after.pending_effects().unwrap().is_empty());
    assert_eq!(
        after.transcript().unwrap().messages[1].content,
        [result(&first.id), result(&second.id)].concat()
    );
    let next_wait = restore(&prepare(&after));
    let Effect::Model { id, .. } = &next_wait.pending_effects().unwrap()[0] else {
        panic!("model");
    };
    assert_eq!(id, "lineage-1/turn-1/m/2");
    let review = restore(
        &next_wait
            .apply_model(&response(&next_wait, 0, "stop"), "route-v1")
            .unwrap(),
    );
    assert_eq!(review.control().phase(), Phase::FinalReview);
    let followup = restore(
        &review
            .followup("policy requires another answer".into())
            .unwrap(),
    );
    assert_eq!(followup.control().phase(), Phase::Ready);
    let wait = prepare(&followup);
    let review = wait
        .apply_model(&response(&wait, 0, "stop"), "route-v1")
        .unwrap();
    let done = restore(&review.finish().unwrap());
    assert_eq!(done.control().phase(), Phase::Stopped(Stop::Complete));
    assert!(done.pending_effects().unwrap().is_empty());
    assert!(done.next_model_id().is_err());
    assert!(done.followup("extra".into()).is_err());
}

#[test]
fn before_model_fork_reruns_only_the_next_model_request_with_a_fresh_identity() {
    let baseline = initial(Some(4));
    let ready = restore(&baseline.fork_before_model("rewound".into()).unwrap());
    assert_eq!(ready.next_model_id().unwrap(), "rewound/m/1");
    assert_eq!(baseline.next_model_id().unwrap(), "lineage-1/turn-1/m/1");

    let waiting = prepare(&baseline);
    assert!(waiting.fork_before_model("rewound".into()).is_err());
    let tools = waiting
        .apply_model(&response(&waiting, 1, "tool_use"), "route-v1")
        .unwrap();
    assert!(tools.fork_before_model("rewound".into()).is_err());
    let Effect::Tool { id, call } = tools.pending_effects().unwrap().remove(0) else {
        panic!("tool");
    };
    let after = tools.apply_tool(&id, result(&call.id)).unwrap();
    assert_eq!(after.control().phase(), Phase::AfterTools);
    assert!(after.fork_before_model(after.execution().into()).is_err());

    let forked = restore(&after.fork_before_model("rewound".into()).unwrap());
    assert_eq!(forked.transcript().unwrap(), after.transcript().unwrap());
    assert_eq!(forked.next_model_id().unwrap(), "rewound/m/2");
    let original = prepare(&after);
    let Effect::Model { waiting, .. } = original.pending_effects().unwrap().remove(0) else {
        panic!("model");
    };
    assert!(
        forked
            .prepare(waiting.request().clone(), "route-v1")
            .is_err(),
        "the original request identity must not be reusable after a rewind"
    );
    let rerun = restore(&prepare(&forked));
    let Effect::Model { id, .. } = rerun.pending_effects().unwrap().remove(0) else {
        panic!("model");
    };
    assert_eq!(id, "rewound/m/2");
    let review = rerun
        .apply_model(&response(&rerun, 0, "stop"), "route-v1")
        .unwrap();
    assert_eq!(review.control().phase(), Phase::FinalReview);
    assert!(review.fork_before_model("again".into()).is_err());
    let done = review.finish().unwrap();
    assert!(done.fork_before_model("again".into()).is_err());
    assert!(done.fork_settled("again".into()).is_ok());
}

#[test]
fn recovery_budget_limits_and_cancel_do_not_silently_restart_or_finish() {
    let mut state = initial(Some(1));
    for _ in 0..3 {
        let wait = prepare(&state);
        state = restore(
            &wait
                .apply_model(
                    &response(&wait, 0, "incomplete:max_output_tokens"),
                    "route-v1",
                )
                .unwrap(),
        );
        assert_eq!(state.control().phase(), Phase::Ready);
        assert_eq!(state.control().iterations(), 0);
    }
    let wait = prepare(&state);
    let halted = restore(
        &wait
            .apply_model(
                &response(&wait, 0, "incomplete:max_output_tokens"),
                "route-v1",
            )
            .unwrap(),
    );
    assert_eq!(
        halted.control().phase(),
        Phase::Stopped(Stop::RecoveryBudget)
    );
    assert!(halted.next_model_id().is_err());
    let wait = prepare(&initial(Some(1)));
    let tools = wait
        .apply_model(&response(&wait, 1, "tool_use"), "route-v1")
        .unwrap();
    let Effect::Tool { id, call } = tools.pending_effects().unwrap().remove(0) else {
        panic!("tool");
    };
    let after = tools.apply_tool(&id, result(&call.id)).unwrap();
    let stopped = restore(&prepare(&after));
    assert_eq!(stopped.control().phase(), Phase::Stopped(Stop::Iterations));
    assert!(stopped.pending_effects().unwrap().is_empty());
    let cancelled = restore(&wait.cancel().unwrap());
    assert_eq!(cancelled.control().phase(), Phase::Stopped(Stop::Cancelled));
    assert!(cancelled.pending_effects().unwrap().is_empty());
    assert!(
        cancelled
            .apply_model(&response(&wait, 0, "stop"), "route-v1")
            .is_err()
    );
}

#[test]
fn foreign_image_scope_and_wrong_request_identity_are_refused_without_changing_baseline() {
    let state = initial(None);
    let wait = prepare(&state);
    let tools = wait
        .apply_model(&response(&wait, 2, "tool_use"), "route-v1")
        .unwrap();
    let Effect::Tool { id, call } = tools.pending_effects().unwrap().remove(0) else {
        panic!("tool");
    };
    let mut blocks = result(&call.id);
    blocks.push(Block::ImageArtifact {
        reference: ArtifactImage {
            scope: "other".into(),
            digest: "a".repeat(64),
            byte_length: 663,
            width: 2,
            height: 1,
        },
    });
    let before = tools.checkpoint().unwrap();
    assert!(
        tools.apply_tool(&id, blocks).is_err(),
        "foreign partial image must be refused, not only checked when batch finishes"
    );
    assert_eq!(tools.checkpoint().unwrap(), before);
    let Effect::Model { waiting, .. } = wait.pending_effects().unwrap().remove(0) else {
        panic!("model");
    };
    let mut identity = waiting.request().identity().clone();
    identity.effect = "different execution".into();
    let wrong = PreparedRequest::new(identity, Encoding::Json, b"request".to_vec()).unwrap();
    assert!(state.prepare(wrong, "route-v1").is_err());
}

#[test]
fn settled_restart_has_fresh_scope_effects_and_refuses_live_or_image_history() {
    let wait = prepare(&initial(Some(4)));
    let review = wait
        .apply_model(&response(&wait, 0, "stop"), "route-v1")
        .unwrap();
    let done = restore(&review.finish().unwrap());
    let baseline = done.checkpoint().unwrap();
    let restarted = restore(
        &done
            .restart_settled(
                "fresh-task".into(),
                "fresh-execution".into(),
                "continue explicitly".into(),
            )
            .unwrap(),
    );
    assert_eq!(restarted.control().phase(), Phase::Ready);
    assert_eq!(restarted.control().iterations(), 0);
    assert_eq!(restarted.next_model_id().unwrap(), "fresh-execution/m/1");
    let history = done.transcript().unwrap();
    let next = restarted.transcript().unwrap();
    assert!(next.messages[..history.messages.len()] == history.messages);
    assert!(matches!(next.messages.last().unwrap().content.as_slice(),
        [Block::Text { text }] if text == "continue explicitly"));
    assert_eq!(done.checkpoint().unwrap(), baseline);
    for state in [initial(None), wait.clone(), review, wait.cancel().unwrap()] {
        assert!(
            state
                .restart_settled("new".into(), "new-exec".into(), "prompt".into())
                .is_err()
        );
    }
    for (scope, execution, prompt) in [
        (done.scope(), "new-exec", "prompt"),
        ("new", done.execution(), "prompt"),
        ("new", "new-exec", " "),
    ] {
        assert!(
            done.restart_settled(scope.into(), execution.into(), prompt.into())
                .is_err()
        );
    }
    let mut transcript = initial(None).transcript().unwrap();
    transcript.messages.push(dext_transcript::Message {
        role: "user".into(),
        content: vec![Block::ImageArtifact {
            reference: ArtifactImage {
                scope: "operator".into(),
                digest: "a".repeat(64),
                byte_length: 100,
                width: 2,
                height: 1,
            },
        }],
    });
    let image_initial = TurnContinuation::new(
        "operator".into(),
        "image-exec".into(),
        Contract::OpenAiResponses,
        None,
        transcript,
    )
    .unwrap();
    let wait = prepare(&image_initial);
    let done = wait
        .apply_model(&response(&wait, 0, "stop"), "route-v1")
        .unwrap()
        .finish()
        .unwrap();
    assert!(
        done.restart_settled("new".into(), "new-exec".into(), "prompt".into())
            .is_err()
    );
}

#[test]
fn malformed_and_mutated_checkpoints_fail_closed_or_roundtrip_exactly() {
    let wait = prepare(&initial(None));
    let tools = wait
        .apply_model(&response(&wait, 2, "tool_use"), "route-v1")
        .unwrap();
    for state in [initial(None), wait, tools] {
        let bytes = state.checkpoint().unwrap();
        for end in 0..bytes.len() {
            assert!(TurnContinuation::restore(&bytes[..end]).is_err());
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(TurnContinuation::restore(&extra).is_err());
        for index in 0..bytes.len() {
            for mask in [1, 0x80] {
                let mut mutated = bytes.clone();
                mutated[index] ^= mask;
                if let Ok(restored) = TurnContinuation::restore(&mutated) {
                    assert_eq!(restored.checkpoint().unwrap(), mutated);
                }
            }
        }
    }
}

#[test]
fn runtime_decisions_ride_the_turn_checkpoint_and_cannot_regress() {
    use dext_transcript::runtime::{ExternalOutcomeInput, TurnRuntimeState};
    let waiting = prepare(&initial(Some(4)));
    let tools = waiting
        .apply_model(&response(&waiting, 1, "tool_use"), "route-v1")
        .unwrap();
    let Effect::Tool { id, call } = tools.pending_effects().unwrap().remove(0) else {
        panic!("tool");
    };
    let mut observed = tools.runtime().clone();
    let hosts = vec!["auth.example".to_string()];
    for _ in 0..2 {
        let mut content = "HTTP 401 unauthorized".to_string();
        observed.record_external_outcome(ExternalOutcomeInput {
            tool_name: "http",
            hosts: &hosts,
            cache_key: None,
            bash_similarity_key: None,
            command: None,
            content: &mut content,
            is_error: Some(true),
        });
    }
    let fresh = TurnRuntimeState::new();
    assert!(tools.with_runtime(fresh.clone()).is_ok());

    let applied = restore(
        &tools
            .with_runtime(observed.clone())
            .unwrap()
            .apply_tool(&id, result(&call.id))
            .unwrap(),
    );
    assert_eq!(applied.control().phase(), Phase::AfterTools);
    assert!(applied.runtime().blocked_host_guard(&hosts).is_some());
    assert_eq!(
        applied.runtime().checkpoint().unwrap(),
        observed.checkpoint().unwrap()
    );
    // A resumed host cannot clear a tripped breaker by supplying older state.
    assert!(applied.with_runtime(fresh).is_err());
    assert!(
        tools
            .apply_tool("lineage-1/turn-1/m/1/t/9", result(&call.id))
            .is_err()
    );
    // Runtime bytes are part of the canonical turn encoding.
    let mut bytes = applied.checkpoint().unwrap();
    bytes.push(0);
    assert!(TurnContinuation::restore(&bytes).is_err());
}

#[test]
fn guard_tool_short_circuits_like_the_cli_and_is_deterministic() {
    use dext_transcript::runtime::ExternalOutcomeInput;
    let waiting = prepare(&initial(Some(4)));
    let saved = {
        let Effect::Model { waiting: wait, .. } = waiting.pending_effects().unwrap().remove(0)
        else {
            panic!("model");
        };
        let bytes = NativeResponse {
            blocks: vec![Block::ToolUse {
                id: "provider-0".into(),
                name: "http".into(),
                input: serde_json::json!({"args": ["GET", "https://auth.example/v1"]}),
            }],
            stop_reason: Some("tool_use".into()),
            usage: Usage::default(),
            unknown_events: 0,
            unfinished_tool_calls: 0,
        }
        .checkpoint()
        .unwrap();
        wait.record(wait.request(), wait.attempt(), &bytes).unwrap()
    };
    let tools = waiting.apply_model(&saved, "route-v1").unwrap();
    let Effect::Tool { id, .. } = tools.pending_effects().unwrap().remove(0) else {
        panic!("tool");
    };
    assert!(
        tools.guard_tool(&id).unwrap().is_none(),
        "no guard fires on a fresh turn"
    );
    assert!(tools.guard_tool("lineage-1/turn-1/m/1/t/7").is_err());

    let mut runtime = tools.runtime().clone();
    let hosts = vec!["auth.example".to_string()];
    for _ in 0..2 {
        let mut content = "HTTP 401 unauthorized".to_string();
        runtime.record_external_outcome(ExternalOutcomeInput {
            tool_name: "http",
            hosts: &hosts,
            cache_key: None,
            bash_similarity_key: None,
            command: None,
            content: &mut content,
            is_error: Some(true),
        });
    }
    let blocked = restore(&tools.with_runtime(runtime).unwrap());
    let guarded = blocked.guard_tool(&id).unwrap().expect("breaker guard");
    assert_eq!(
        guarded.checkpoint().unwrap(),
        blocked
            .guard_tool(&id)
            .unwrap()
            .unwrap()
            .checkpoint()
            .unwrap()
    );
    assert_eq!(guarded.control().phase(), Phase::AfterTools);
    assert!(guarded.pending_effects().unwrap().is_empty());
    let last = guarded.transcript().unwrap().messages.pop().unwrap();
    assert!(matches!(
        &last.content[0],
        Block::ToolResult { content, is_error: Some(true), .. } if content.contains("source circuit breaker")
    ));
    assert!(
        guarded.guard_tool(&id).is_err(),
        "settled call cannot be guarded again"
    );
}

#[test]
fn steering_matches_cli_text_phase_and_injection_point() {
    use dext_transcript::runtime::{SteeringPoint, WorkPhase, steering_update_text};
    let waiting = prepare(&initial(Some(4)));
    assert!(
        waiting.steer("too early".into()).is_err(),
        "model wait refuses steering"
    );
    let review = restore(
        &waiting
            .apply_model(&response(&waiting, 0, "stop"), "route-v1")
            .unwrap(),
    );
    assert_eq!(review.control().phase(), Phase::FinalReview);
    assert!(review.steer("   ".into()).is_err());
    let steered = restore(&review.steer("use the other file".into()).unwrap());
    assert_eq!(steered.control().phase(), Phase::Ready);
    assert_eq!(steered.runtime().phase(), WorkPhase::Synthesize);
    let last = steered.transcript().unwrap().messages.pop().unwrap();
    assert_eq!(last.role, "user");
    let expected = steering_update_text(
        WorkPhase::Probe,
        review.control().iterations(),
        0,
        SteeringPoint::BeforeFinal,
        "use the other file",
        "",
    );
    assert!(matches!(&last.content[..], [Block::Text { text }] if *text == expected));
    assert!(expected.contains("Injection point: before final response"));
    assert!(
        steered.next_model_id().is_ok(),
        "steering continues the turn"
    );
    // A second update waiting at Ready joins before the next request, phase unchanged.
    let again = restore(&steered.steer("and the tests".into()).unwrap());
    assert_eq!(again.control().phase(), Phase::Ready);
    let last = again.transcript().unwrap().messages.pop().unwrap();
    assert!(matches!(
        &last.content[..],
        [Block::Text { text }] if text.contains("Injection point: before the next model request")
            && text.ends_with("User update:\nand the tests")
    ));
    assert!(
        again.next_model_id().is_ok(),
        "the turn goes on to its next request"
    );
    let fresh = initial(Some(4));
    assert_eq!(fresh.control().phase(), Phase::Ready);
    assert!(
        fresh.steer("before the first request".into()).is_ok(),
        "a turn's start takes an update"
    );

    let waiting = prepare(&initial(Some(4)));
    let tools = waiting
        .apply_model(&response(&waiting, 1, "tool_use"), "route-v1")
        .unwrap();
    assert!(
        tools.steer("mid tools".into()).is_err(),
        "tools wait refuses steering"
    );
    let Effect::Tool { id, call } = tools.pending_effects().unwrap().remove(0) else {
        panic!("tool");
    };
    let after = tools.apply_tool(&id, result(&call.id)).unwrap();
    assert_eq!(after.control().phase(), Phase::AfterTools);
    let steered = restore(&after.steer("narrow the scope".into()).unwrap());
    assert_eq!(steered.control().phase(), Phase::AfterTools);
    let last = steered.transcript().unwrap().messages.pop().unwrap();
    assert!(matches!(
        &last.content[..],
        [Block::Text { text }] if text.contains("Injection point: after tool results")
            && text.contains("1 tool calls so far")
            && text.ends_with("User update:\nnarrow the scope")
    ));
    assert_eq!(
        after
            .steer("narrow the scope".into())
            .unwrap()
            .checkpoint()
            .unwrap(),
        steered.checkpoint().unwrap(),
        "deterministic across restore"
    );
}

#[test]
fn budget_stop_matches_cli_text_and_only_fires_at_the_cap() {
    use dext_transcript::runtime::budget_denial_text;
    let ready = initial(Some(4));
    let usage = Usage {
        input: 900,
        output: 200,
        ..Usage::default()
    };
    assert!(
        ready.stop_for_budget(None, Some(5_000), &usage).is_err(),
        "under cap"
    );
    assert!(ready.stop_for_budget(None, None, &usage).is_err(), "no cap");
    assert!(ready.stop_for_budget(Some(f64::NAN), None, &usage).is_err());
    assert!(ready.stop_for_budget(None, Some(0), &usage).is_err());
    let stopped = restore(&ready.stop_for_budget(None, Some(1_000), &usage).unwrap());
    assert_eq!(stopped.control().phase(), Phase::Stopped(Stop::Budget));
    assert!(stopped.pending_effects().unwrap().is_empty());
    let last = stopped.transcript().unwrap().messages.pop().unwrap();
    assert_eq!(last.role, "assistant");
    let expected = budget_denial_text(None, Some(1_000), &usage).unwrap();
    assert!(
        expected.starts_with(
            "token budget cap reached: 1100/1000 tokens; refusing another model request"
        )
    );
    assert!(matches!(&last.content[..], [Block::Text { text }] if *text == expected));
    assert!(
        stopped.next_model_id().is_err(),
        "no request after a budget stop"
    );
    assert!(stopped.stop_for_budget(None, Some(1_000), &usage).is_err());

    let waiting = prepare(&initial(Some(4)));
    assert!(
        waiting.stop_for_budget(None, Some(1), &usage).is_err(),
        "model wait is host cancel"
    );
    let review = waiting
        .apply_model(&response(&waiting, 0, "stop"), "route-v1")
        .unwrap();
    assert_eq!(review.control().phase(), Phase::FinalReview);
    assert!(
        review.stop_for_budget(None, Some(1), &usage).is_err(),
        "CLI never budget-stops at final review"
    );
}

#[test]
fn compaction_replaces_settled_history_only_under_the_same_route() {
    let waiting = prepare(&initial(Some(4)));
    let tools = waiting
        .apply_model(&response(&waiting, 1, "tool_use"), "route-v1")
        .unwrap();
    let summary = TranscriptCheckpoint {
        route_binding: "route-v1".into(),
        messages: vec![dext_transcript::Message {
            role: "user".into(),
            content: vec![Block::Text {
                text: "[compacted] earlier work summary".into(),
            }],
        }],
    };
    assert!(
        tools.compact(summary.clone()).is_err(),
        "tools wait refuses compaction"
    );
    let Effect::Tool { id, call } = tools.pending_effects().unwrap().remove(0) else {
        panic!("tool");
    };
    let after = tools.apply_tool(&id, result(&call.id)).unwrap();
    let mut wrong_route = summary.clone();
    wrong_route.route_binding = "route-v2".into();
    assert!(after.compact(wrong_route).is_err());
    let empty = TranscriptCheckpoint {
        route_binding: "route-v1".into(),
        messages: vec![],
    };
    assert!(after.compact(empty).is_err());
    let mut longer = after.transcript().unwrap();
    longer.messages.extend(summary.messages.clone());
    assert!(
        after.compact(longer).is_err(),
        "compaction never grows history"
    );
    let mut with_image = summary.clone();
    with_image.messages[0].content.push(Block::ImageArtifact {
        reference: ArtifactImage {
            scope: "operator".into(),
            digest: "b".repeat(64),
            byte_length: 663,
            width: 2,
            height: 1,
        },
    });
    assert!(
        after.compact(with_image).is_err(),
        "compaction cannot introduce images"
    );
    let compacted = restore(&after.compact(summary.clone()).unwrap());
    assert_eq!(compacted.transcript().unwrap(), summary);
    assert_eq!(compacted.control(), after.control());
    assert_eq!(
        compacted.runtime().checkpoint().unwrap(),
        after.runtime().checkpoint().unwrap(),
        "runtime guards survive compaction"
    );
    assert!(compacted.next_model_id().is_ok());
}

#[test]
fn round_outcomes_are_observed_in_call_order_and_same_round_repeats_are_not_guarded() {
    let fetch = |id: &str| Block::ToolUse {
        id: id.into(),
        name: "http".into(),
        input: serde_json::json!({"args": ["GET", "https://auth.example/v1"]}),
    };
    let round = |order: [usize; 3]| {
        let waiting = prepare(&initial(Some(4)));
        let Effect::Model { waiting: wait, .. } = waiting.pending_effects().unwrap().remove(0)
        else {
            panic!("model");
        };
        let bytes = NativeResponse {
            blocks: vec![fetch("a"), fetch("b"), fetch("c")],
            stop_reason: Some("tool_use".into()),
            usage: Usage::default(),
            unknown_events: 0,
            unfinished_tool_calls: 0,
        }
        .checkpoint()
        .unwrap();
        let recorded = wait.record(wait.request(), wait.attempt(), &bytes).unwrap();
        let mut state = waiting.apply_model(&recorded, "route-v1").unwrap();
        let effects: Vec<(String, String)> = state
            .pending_effects()
            .unwrap()
            .into_iter()
            .map(|effect| match effect {
                Effect::Tool { id, call } => (id, call.id),
                Effect::Model { .. } => panic!("tool"),
            })
            .collect();
        for index in order {
            let (id, call_id) = &effects[index];
            assert!(
                state.guard_tool(id).unwrap().is_none(),
                "round-start state never guards same-round repeats"
            );
            state = state
                .apply_tool(
                    id,
                    vec![Block::ToolResult {
                        tool_use_id: call_id.clone(),
                        content: "HTTP 401 unauthorized".into(),
                        is_error: Some(true),
                        metadata: Default::default(),
                    }],
                )
                .unwrap();
        }
        restore(&state)
    };
    let forward = round([0, 1, 2]);
    let shuffled = round([2, 0, 1]);
    assert_eq!(
        forward.checkpoint().unwrap(),
        shuffled.checkpoint().unwrap(),
        "arrival order changed runtime decisions"
    );
    assert_eq!(forward.control().phase(), Phase::AfterTools);
    assert!(
        forward
            .runtime()
            .blocked_host_guard(&["auth.example".to_string()])
            .is_some()
    );
    let results = forward.transcript().unwrap().messages.pop().unwrap();
    let notes: Vec<bool> = results
        .content
        .iter()
        .map(|block| matches!(block, Block::ToolResult { content, .. } if content.contains("[circuit-breaker]")))
        .collect();
    assert_eq!(
        notes,
        [false, true, false],
        "breaker note lands on the second call in call order"
    );
}
