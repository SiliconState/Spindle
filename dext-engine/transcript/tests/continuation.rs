use dext_core::model::{Encoding, Identity, ModelWait, Phase, PreparedRequest};
use dext_transcript::checkpoint::{NativeResponse, TranscriptCheckpoint};
use dext_transcript::continuation::{ResponseApplication, ResponseContinuation, append_assistant};
use dext_transcript::{Block, Message, Usage};

fn waiting(effect: &str) -> ResponseContinuation {
    let request = PreparedRequest::new(
        Identity {
            effect: effect.into(),
            provider: "fake".into(),
            contract: "fake-v1".into(),
            endpoint: "https://example.invalid".into(),
            model: "fake".into(),
            credential_scope: "opaque-version".into(),
            configuration: "policy-v1".into(),
        },
        Encoding::Json,
        b"request".to_vec(),
    )
    .unwrap();
    ResponseContinuation::waiting(
        TranscriptCheckpoint {
            route_binding: "route".into(),
            messages: vec![],
        },
        ModelWait::new(request).start().unwrap(),
    )
    .unwrap()
}
fn receipt(wait: &ResponseContinuation) -> ModelWait<Vec<u8>> {
    let response = NativeResponse {
        blocks: vec![
            Block::Text {
                text: "same answer".into(),
            },
            Block::ToolUse {
                id: "call".into(),
                name: "inert".into(),
                input: serde_json::json!({"secret":"original"}),
            },
        ],
        stop_reason: Some("tool_use".into()),
        usage: Usage::default(),
        unknown_events: 0,
        unfinished_tool_calls: 0,
    };
    wait.model()
        .record(
            wait.model().request(),
            wait.model().attempt(),
            &response.checkpoint().unwrap(),
        )
        .unwrap()
}

fn tools() -> dext_transcript::tool_results::ToolResultsContinuation {
    let wait = waiting("model");
    let mut native = NativeResponse::restore(receipt(&wait).response().unwrap()).unwrap();
    native.blocks.push(Block::ToolUse {
        id: "second".into(),
        name: "inert".into(),
        input: serde_json::json!({"path":"two"}),
    });
    let recorded = wait
        .model()
        .record(wait.model().request(), 1, &native.checkpoint().unwrap())
        .unwrap();
    dext_transcript::tool_results::ToolResultsContinuation::new(
        wait.apply(&recorded, "route").unwrap(),
    )
    .unwrap()
}
fn result(id: &str, content: &str) -> Block {
    Block::ToolResult {
        tool_use_id: id.into(),
        content: content.into(),
        is_error: Some(true),
        metadata: dext_transcript::ToolResultMetadata {
            status: Some("error".into()),
            exit_code: Some(1),
            duration_ms: Some(42),
            artifact: Some("opaque-artifact".into()),
        },
    }
}

fn artifact_image() -> Block {
    Block::ImageArtifact {
        reference: dext_transcript::ArtifactImage {
            scope: "operator".into(),
            digest: "a".repeat(64),
            byte_length: 663,
            width: 2,
            height: 1,
        },
    }
}

#[test]
fn immutable_images_survive_partial_result_restore_without_disclosure_grants() {
    use dext_transcript::tool_results::ToolResultsContinuation;
    let baseline = tools();
    let calls = baseline.pending_calls().unwrap();
    let blocks = vec![result(&calls[0].id, "image retained"), artifact_image()];
    let partial = baseline
        .observe_blocks("model", 1, &calls[0], blocks.clone())
        .unwrap();
    let bytes = partial.checkpoint().unwrap();
    assert!(bytes.starts_with(b"DTC2"));
    assert!(!String::from_utf8_lossy(&bytes).contains("disclosure"));
    let restored = ToolResultsContinuation::restore(&bytes).unwrap();
    assert_eq!(restored.pending_calls().unwrap(), vec![calls[1].clone()]);
    assert_eq!(
        restored
            .observe_blocks("model", 1, &calls[0], blocks.clone())
            .unwrap()
            .checkpoint()
            .unwrap(),
        bytes
    );
    let done = restored
        .observe("model", 1, &calls[1], result(&calls[1].id, "done"))
        .unwrap();
    let transcript = done.transcript().unwrap();
    assert_eq!(
        transcript.messages[1].content,
        [blocks, vec![result(&calls[1].id, "done")]].concat()
    );
    let transcript_bytes = transcript.checkpoint().unwrap();
    assert_eq!(
        dext_transcript::checkpoint::TranscriptCheckpoint::restore(&transcript_bytes)
            .unwrap()
            .checkpoint()
            .unwrap(),
        transcript_bytes
    );
}

#[test]
fn result_images_reject_mutable_paths_invalid_shapes_duplicates_and_conflicting_retry() {
    let baseline = tools();
    let call = baseline.pending_calls().unwrap().remove(0);
    let mut invalid = artifact_image();
    if let Block::ImageArtifact { reference } = &mut invalid {
        reference.width = 0;
    }
    for blocks in [
        vec![artifact_image(), result(&call.id, "out")],
        vec![result(&call.id, "out"), artifact_image(), artifact_image()],
        vec![result(&call.id, "out"), invalid],
        vec![
            result(&call.id, "out"),
            Block::ImageReference {
                path: "/mutable.jpg".into(),
                media_type: "image/jpeg".into(),
                width: 2,
                height: 1,
                source_sha256: "a".repeat(64),
                disclosure_target: Some("old approval".into()),
            },
        ],
    ] {
        assert!(baseline.observe_blocks("model", 1, &call, blocks).is_err());
    }
    let partial = baseline
        .observe_blocks(
            "model",
            1,
            &call,
            vec![result(&call.id, "out"), artifact_image()],
        )
        .unwrap();
    assert!(
        partial
            .observe("model", 1, &call, result(&call.id, "out"))
            .is_err()
    );
    assert!(
        dext_transcript::continuation::ResponseApplication::new(vec![artifact_image()]).is_err()
    );
}

#[test]
fn tool_results_restore_partial_batches_and_publish_once_in_call_order() {
    use dext_transcript::tool_results::ToolResultsContinuation;
    let baseline = tools();
    let calls = baseline.pending_calls().unwrap();
    let second = result("second", "second output");
    let partial = baseline
        .observe("model", 1, &calls[1], second.clone())
        .unwrap();
    assert!(!partial.complete());
    assert_eq!(partial.transcript().unwrap().messages.len(), 1);
    assert_eq!(baseline.pending_calls().unwrap().len(), 2);
    let restored = ToolResultsContinuation::restore(&partial.checkpoint().unwrap()).unwrap();
    assert_eq!(restored.pending_calls().unwrap(), vec![calls[0].clone()]);
    assert_eq!(
        restored
            .observe("model", 1, &calls[1], second.clone())
            .unwrap()
            .checkpoint()
            .unwrap(),
        partial.checkpoint().unwrap()
    );
    let first = result("call", "first output");
    let done = restored
        .observe("model", 1, &calls[0], first.clone())
        .unwrap();
    assert!(done.complete());
    assert!(done.pending_calls().unwrap().is_empty());
    let done = ToolResultsContinuation::restore(&done.checkpoint().unwrap()).unwrap();
    let transcript = done.transcript().unwrap();
    assert_eq!(transcript.messages.len(), 2);
    assert_eq!(transcript.messages[1].role, "user");
    assert_eq!(transcript.messages[1].content, vec![first.clone(), second]);
    assert_eq!(
        done.observe("model", 1, &calls[0], first)
            .unwrap()
            .transcript()
            .unwrap()
            .messages
            .len(),
        2
    );
    assert!(!format!("{done:?}").contains("output"));
}

#[test]
fn tool_results_refuse_wrong_scope_call_payload_conflicts_and_nonresults() {
    let baseline = tools();
    let call = baseline.pending_calls().unwrap().remove(0);
    let good = result("call", "output");
    assert!(baseline.observe("other", 1, &call, good.clone()).is_err());
    assert!(baseline.observe("model", 2, &call, good.clone()).is_err());
    let mut wrong = call.clone();
    wrong.input = serde_json::json!({"changed":true});
    assert!(baseline.observe("model", 1, &wrong, good.clone()).is_err());
    let mut wrong = call.clone();
    wrong.name = "other".into();
    assert!(baseline.observe("model", 1, &wrong, good.clone()).is_err());
    assert!(
        baseline
            .observe("model", 1, &call, result("other", "output"))
            .is_err()
    );
    assert!(
        baseline
            .observe(
                "model",
                1,
                &call,
                Block::Text {
                    text: "output".into()
                }
            )
            .is_err()
    );
    let partial = baseline.observe("model", 1, &call, good).unwrap();
    assert!(
        partial
            .observe("model", 1, &call, result("call", "conflict"))
            .is_err()
    );
    let oversized = result(
        "call",
        &"x".repeat(dext_transcript::checkpoint::MAX_CHECKPOINT_BYTES),
    );
    assert!(baseline.observe("model", 1, &call, oversized).is_err());
    assert_eq!(baseline.pending_calls().unwrap().len(), 2);
}

#[test]
fn tool_results_checkpoint_rejects_truncation_version_trailing_and_wrong_slots() {
    use dext_transcript::tool_results::ToolResultsContinuation;
    let baseline = tools();
    let call = baseline.pending_calls().unwrap().remove(0);
    let partial = baseline
        .observe("model", 1, &call, result("call", "output"))
        .unwrap();
    let bytes = partial.checkpoint().unwrap();
    for end in 0..bytes.len() {
        assert!(ToolResultsContinuation::restore(&bytes[..end]).is_err());
    }
    let mut bad = bytes.clone();
    bad[3] = b'1';
    assert!(ToolResultsContinuation::restore(&bad).is_err());
    let mut bad = bytes.clone();
    bad.push(0);
    assert!(ToolResultsContinuation::restore(&bad).is_err());
    let response_len = u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize;
    // Use canonical native encodings so refusal exercises slot/link validation,
    // rather than merely rejecting serde_json::Value's different field ordering.
    for slots in [
        vec![],
        vec![None],
        vec![None, Some(vec![result("call", "output")])],
        vec![
            Some(vec![Block::Text {
                text: "not a result".into(),
            }]),
            None,
        ],
    ] {
        let mut bad = bytes[..8 + response_len].to_vec();
        let mut results = b"DTR2".to_vec();
        results.extend(serde_json::to_vec(&slots).unwrap());
        bad.extend_from_slice(&(results.len() as u32).to_be_bytes());
        bad.extend(results);
        assert!(ToolResultsContinuation::restore(&bad).is_err());
    }
}

#[test]
fn durable_transition_refuses_uncheckpointable_history_before_adoption() {
    let wait = waiting("model");
    let transcript = TranscriptCheckpoint {
        route_binding: "route".into(),
        messages: vec![Message {
            role: "user".into(),
            content: vec![Block::Text {
                text: "x".repeat(dext_transcript::checkpoint::MAX_CHECKPOINT_BYTES),
            }],
        }],
    };
    assert!(ResponseContinuation::waiting(transcript, wait.model().clone()).is_err());
    let transcript = TranscriptCheckpoint {
        route_binding: "route".into(),
        messages: vec![Message {
            role: "user".into(),
            content: vec![Block::Text {
                text: "x".repeat(dext_transcript::checkpoint::MAX_CHECKPOINT_BYTES - 200),
            }],
        }],
    };
    let wait = ResponseContinuation::waiting(transcript, wait.model().clone()).unwrap();
    let mut response = NativeResponse::restore(receipt(&wait).response().unwrap()).unwrap();
    response.blocks = vec![Block::Text {
        text: "y".repeat(512),
    }];
    let recorded = wait
        .model()
        .record(wait.model().request(), 1, &response.checkpoint().unwrap())
        .unwrap();
    assert!(wait.apply(&recorded, "route").is_err());
}

#[test]
fn tool_batch_reserves_room_for_its_result_message() {
    use dext_transcript::tool_results::ToolResultsContinuation;
    let wait = waiting("model");
    let transcript = TranscriptCheckpoint {
        route_binding: "route".into(),
        messages: vec![
            Message {
                role: "user".into(),
                content: vec![]
            };
            dext_transcript::checkpoint::MAX_MESSAGES - 1
        ],
    };
    let wait = ResponseContinuation::waiting(transcript, wait.model().clone()).unwrap();
    let applied = wait.apply(&receipt(&wait), "route").unwrap();
    assert_eq!(
        applied.transcript().messages.len(),
        dext_transcript::checkpoint::MAX_MESSAGES
    );
    assert!(ToolResultsContinuation::new(applied).is_err());
}

#[test]
fn tool_batch_reserves_aggregate_checkpoint_space_for_minimal_results() {
    use dext_transcript::tool_results::ToolResultsContinuation;
    let initial = waiting("model");
    let make = |body_len| {
        let request = PreparedRequest::new(
            initial.model().request().identity().clone(),
            Encoding::Json,
            vec![b'x'; body_len],
        )
        .unwrap();
        let wait = ResponseContinuation::waiting(
            initial.transcript().clone(),
            ModelWait::new(request).start().unwrap(),
        )
        .unwrap();
        wait.apply(&receipt(&wait), "route").unwrap()
    };
    let small = make(1);
    let overhead = ToolResultsContinuation::new(small)
        .unwrap()
        .checkpoint()
        .unwrap()
        .len()
        - 1;
    let tight = make(dext_transcript::checkpoint::MAX_CHECKPOINT_BYTES - overhead);
    assert!(
        ToolResultsContinuation::new(tight).is_err(),
        "null slots fit but even empty results cannot fit"
    );
}

#[test]
fn minimal_result_fits_at_exact_aggregate_checkpoint_limit() {
    use dext_transcript::tool_results::ToolResultsContinuation;
    let initial = waiting("model");
    let make = |body_len| {
        let request = PreparedRequest::new(
            initial.model().request().identity().clone(),
            Encoding::Json,
            vec![b'x'; body_len],
        )
        .unwrap();
        let wait = ResponseContinuation::waiting(
            initial.transcript().clone(),
            ModelWait::new(request).start().unwrap(),
        )
        .unwrap();
        ToolResultsContinuation::new(wait.apply(&receipt(&wait), "route").unwrap()).unwrap()
    };
    let small = make(1);
    let call = small.pending_calls().unwrap().remove(0);
    let minimal = Block::ToolResult {
        tool_use_id: call.id.clone(),
        content: String::new(),
        is_error: None,
        metadata: Default::default(),
    };
    let overhead = small
        .observe("model", 1, &call, minimal.clone())
        .unwrap()
        .checkpoint()
        .unwrap()
        .len()
        - 1;
    let tight = make(dext_transcript::checkpoint::MAX_CHECKPOINT_BYTES - overhead);
    let done = tight.observe("model", 1, &call, minimal).unwrap();
    let bytes = done.checkpoint().unwrap();
    assert_eq!(
        bytes.len(),
        dext_transcript::checkpoint::MAX_CHECKPOINT_BYTES
    );
    assert!(ToolResultsContinuation::restore(&bytes).unwrap().complete());
}

#[test]
fn result_arrival_orders_and_retries_converge_to_identical_checkpoints() {
    use dext_transcript::tool_results::ToolResultsContinuation;
    let baseline = tools();
    let calls = baseline.pending_calls().unwrap();
    let mut final_bytes = None;
    for order in [[0, 1], [1, 0]] {
        let mut state = baseline.clone();
        for index in order {
            let output = result(&calls[index].id, "retained output");
            state = state
                .observe("model", 1, &calls[index], output.clone())
                .unwrap();
            state = ToolResultsContinuation::restore(&state.checkpoint().unwrap()).unwrap();
            let repeated = state.observe("model", 1, &calls[index], output).unwrap();
            assert_eq!(state.checkpoint().unwrap(), repeated.checkpoint().unwrap());
        }
        let bytes = state.checkpoint().unwrap();
        if let Some(expected) = &final_bytes {
            assert_eq!(expected, &bytes);
        }
        final_bytes = Some(bytes);
    }
}

#[test]
fn mutated_checkpoint_decoders_never_panic_and_accepted_states_roundtrip() {
    use dext_transcript::tool_results::ToolResultsContinuation;
    let wait = waiting("model");
    let applied = wait.apply(&receipt(&wait), "route").unwrap();
    for bytes in [wait.checkpoint().unwrap(), applied.checkpoint().unwrap()] {
        for index in 0..bytes.len() {
            let mut mutated = bytes.clone();
            mutated[index] ^= 0x80;
            if let Ok(restored) = ResponseContinuation::restore(&mutated) {
                assert_eq!(restored.checkpoint().unwrap(), mutated);
            }
        }
    }
    let state = tools();
    let call = state.pending_calls().unwrap().remove(0);
    let state = state
        .observe("model", 1, &call, result(&call.id, "output"))
        .unwrap();
    let bytes = state.checkpoint().unwrap();
    for index in 0..bytes.len() {
        for mask in [1, 0x80, 0xff] {
            let mut mutated = bytes.clone();
            mutated[index] ^= mask;
            if let Ok(restored) = ToolResultsContinuation::restore(&mutated) {
                assert_eq!(restored.checkpoint().unwrap(), mutated);
                restored.transcript().unwrap().checkpoint().unwrap();
            }
        }
    }
}

#[test]
fn exact_call_binding_distinguishes_signed_zero() {
    use dext_transcript::tool_results::ToolResultsContinuation;
    let wait = waiting("model");
    let mut response = NativeResponse::restore(receipt(&wait).response().unwrap()).unwrap();
    response.blocks = vec![Block::ToolUse {
        id: "call".into(),
        name: "inert".into(),
        input: serde_json::json!({"value": -0.0}),
    }];
    let recorded = wait
        .model()
        .record(wait.model().request(), 1, &response.checkpoint().unwrap())
        .unwrap();
    let applied = wait.apply(&recorded, "route").unwrap();
    let tools = ToolResultsContinuation::new(applied.clone()).unwrap();
    let mut call = tools.pending_calls().unwrap().remove(0);
    call.input = serde_json::json!({"value": 0.0});
    assert!(
        tools
            .observe("model", 1, &call, result("call", "output"))
            .is_err()
    );
    let mut transcript = applied.transcript().clone();
    if let Block::ToolUse { input, .. } = &mut transcript.messages[0].content[0] {
        *input = call.input;
    }
    let mut bytes = b"DNC1".to_vec();
    for field in [
        transcript.checkpoint().unwrap(),
        applied.model().checkpoint(),
    ] {
        bytes.extend_from_slice(&(field.len() as u32).to_be_bytes());
        bytes.extend(field);
    }
    assert!(ResponseContinuation::restore(&bytes).is_err());
}

#[test]
fn speculative_application_roundtrips_with_pending_calls_and_no_duplicate_adoption() {
    let wait = waiting("model-1");
    let baseline = wait.checkpoint().unwrap();
    let receipt = receipt(&wait);
    let next = wait.apply(&receipt, "route").unwrap();
    assert_eq!(wait.checkpoint().unwrap(), baseline);
    assert!(wait.pending_calls().is_err());
    assert_eq!(next.model().phase(), Phase::Applied);
    assert_eq!(next.transcript().messages.len(), 1);
    assert_eq!(next.pending_calls().unwrap()[0].input["secret"], "original");
    assert!(next.apply(&receipt, "route").is_err());
    assert!(next.model().start().is_err());
    assert_eq!(
        next.checkpoint().unwrap(),
        wait.apply(&receipt, "route").unwrap().checkpoint().unwrap()
    );
    let restored = ResponseContinuation::restore(&next.checkpoint().unwrap()).unwrap();
    assert_eq!(restored.checkpoint().unwrap(), next.checkpoint().unwrap());
    assert_eq!(
        restored.pending_calls().unwrap(),
        next.pending_calls().unwrap()
    );
    assert!(!format!("{next:?}").contains("original"));
}

#[test]
fn receipt_binding_checks_identity_attempt_route_and_native_bytes() {
    let wait = waiting("model-1");
    assert!(wait.apply(&receipt(&waiting("model-2")), "route").is_err());
    assert!(wait.apply(&receipt(&wait), "changed-route").is_err());
    let retried = wait
        .model()
        .abandon_unrecorded_for_retry()
        .unwrap()
        .start()
        .unwrap();
    let late = retried
        .record(
            retried.request(),
            retried.attempt(),
            receipt(&wait).response().unwrap(),
        )
        .unwrap();
    assert!(wait.apply(&late, "route").is_err());
    let invalid = wait
        .model()
        .record(wait.model().request(), 1, b"not native response")
        .unwrap();
    assert!(wait.apply(&invalid, "route").is_err());
    let changed = PreparedRequest::new(
        wait.model().request().identity().clone(),
        Encoding::Json,
        b"different bytes".to_vec(),
    )
    .unwrap();
    let changed = ModelWait::new(changed).start().unwrap();
    let changed = changed
        .record(changed.request(), 1, receipt(&wait).response().unwrap())
        .unwrap();
    assert!(wait.apply(&changed, "route").is_err());
}

#[test]
fn identical_answers_from_distinct_effects_are_not_content_deduplicated() {
    let first = waiting("first");
    let first = first.apply(&receipt(&first), "route").unwrap();
    let second = ResponseContinuation::waiting(
        first.transcript().clone(),
        waiting("second").model().clone(),
    )
    .unwrap();
    let second = second.apply(&receipt(&second), "route").unwrap();
    assert_eq!(second.transcript().messages.len(), 2);
    assert!(second.transcript().messages[0] == second.transcript().messages[1]);
}

#[test]
fn empty_and_text_only_responses_are_applied_without_inventing_tool_work() {
    for blocks in [
        vec![],
        vec![Block::Text {
            text: "answer".into(),
        }],
    ] {
        let wait = waiting("model");
        let response = NativeResponse {
            blocks: blocks.clone(),
            stop_reason: Some("stop".into()),
            usage: Usage::default(),
            unknown_events: 0,
            unfinished_tool_calls: 0,
        };
        let recorded = wait
            .model()
            .record(wait.model().request(), 1, &response.checkpoint().unwrap())
            .unwrap();
        let next = wait.apply(&recorded, "route").unwrap();
        assert!(next.pending_calls().unwrap().is_empty());
        assert_eq!(
            next.transcript().messages.len(),
            usize::from(!blocks.is_empty())
        );
        assert!(ResponseContinuation::restore(&next.checkpoint().unwrap()).is_ok());
    }
}

#[test]
fn cli_history_projection_does_not_change_executable_arguments() {
    let wait = waiting("model");
    let native = NativeResponse::restore(receipt(&wait).response().unwrap()).unwrap();
    let application = ResponseApplication::new(native.blocks).unwrap();
    let mut projection = application.blocks().to_vec();
    if let Block::ToolUse { input, .. } = &mut projection[1] {
        *input = serde_json::json!({"secret":"redacted"});
    }
    let mut history = vec![];
    assert!(append_assistant(&mut history, projection.clone(), true));
    assert!(!append_assistant(&mut history, projection, true));
    assert!(!append_assistant(&mut history, vec![], true));
    assert_eq!(application.into_calls()[0].input["secret"], "original");
}

#[test]
fn continuation_rejects_truncation_version_trailing_data_and_inconsistent_tail() {
    let wait = waiting("model");
    let next = wait.apply(&receipt(&wait), "route").unwrap();
    for state in [&wait, &next] {
        let bytes = state.checkpoint().unwrap();
        for end in 0..bytes.len() {
            assert!(ResponseContinuation::restore(&bytes[..end]).is_err());
        }
        let mut bad = bytes.clone();
        bad[3] = b'2';
        assert!(ResponseContinuation::restore(&bad).is_err());
        let mut bad = bytes;
        bad.push(0);
        assert!(ResponseContinuation::restore(&bad).is_err());
    }
    let wrong = TranscriptCheckpoint {
        route_binding: "route".into(),
        messages: vec![Message {
            role: "assistant".into(),
            content: vec![Block::Text {
                text: "wrong".into(),
            }],
        }],
    }
    .checkpoint()
    .unwrap();
    let model = next.model().checkpoint();
    let mut bytes = b"DNC1".to_vec();
    for field in [wrong, model] {
        bytes.extend_from_slice(&(field.len() as u32).to_be_bytes());
        bytes.extend(field);
    }
    assert!(ResponseContinuation::restore(&bytes).is_err());
}
