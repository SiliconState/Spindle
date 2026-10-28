use dext_transcript::checkpoint::{
    Error, MAX_CHECKPOINT_BYTES, NativeResponse, TranscriptCheckpoint,
};
use dext_transcript::{Block, Message, ToolResultMetadata, Usage};
use serde_json::json;

fn response() -> NativeResponse {
    NativeResponse {
        blocks: vec![
            Block::Thinking {
                text: "private thinking".into(),
                signature: Some("signed".into()),
            },
            Block::RedactedThinking {
                data: "opaque".into(),
            },
            Block::ResponsesReasoning {
                item: json!({"type":"reasoning","id":"r1","encrypted_content":"cipher","extra":{"preserved":true}}),
            },
            Block::ToolUse {
                id: "z".into(),
                name: "read_file".into(),
                input: json!({"path":"one"}),
            },
            Block::Text {
                text: "between 🔒".into(),
            },
            Block::ToolUse {
                id: "a".into(),
                name: "read_file".into(),
                input: json!({"path":"two"}),
            },
        ],
        stop_reason: Some("max_tokens".into()),
        usage: Usage {
            input: 19,
            output: 29,
            cache_create: 3,
            cache_read: 4,
            cost_usd: Some(0.125),
        },
        unknown_events: 3,
        unfinished_tool_calls: 1,
    }
}

#[test]
fn native_response_roundtrip_preserves_order_metadata_and_opaque_values() {
    let value = response();
    let bytes = value.checkpoint().unwrap();
    let restored = NativeResponse::restore(&bytes).unwrap();
    assert_eq!(restored, value);
    assert_eq!(restored.checkpoint().unwrap(), bytes);
    let proposal =
        dext_core::response::ResponseProposal::from_validated(restored.blocks, (), true).unwrap();
    assert_eq!(
        proposal
            .into_calls()
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        ["z", "a"]
    );
    assert!(!format!("{value:?}").contains("private"));
}

#[test]
fn transcript_retains_partial_output_results_and_revokes_image_disclosure() {
    let mut transcript = TranscriptCheckpoint {
        route_binding: "opaque-route-version".into(),
        messages: vec![
            Message {
                role: "user".into(),
                content: vec![Block::ImageReference {
                    path: "image.png".into(),
                    media_type: "image/png".into(),
                    width: 1,
                    height: 2,
                    source_sha256: "a".repeat(64),
                    disclosure_target: Some("ephemeral-grant".into()),
                }],
            },
            Message {
                role: "assistant".into(),
                content: response().blocks,
            },
            Message {
                role: "user".into(),
                content: vec![Block::ToolResult {
                    tool_use_id: "z".into(),
                    content: "result".into(),
                    is_error: Some(true),
                    metadata: ToolResultMetadata {
                        status: Some("failed".into()),
                        exit_code: Some(1),
                        duration_ms: Some(10),
                        artifact: Some("artifact-id".into()),
                    },
                }],
            },
            Message {
                role: "assistant".into(),
                content: vec![Block::PartialStream {
                    text: "aborted".into(),
                }],
            },
        ],
    };
    let bytes = transcript.checkpoint().unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("ephemeral-grant"));
    let restored = TranscriptCheckpoint::restore(&bytes).unwrap();
    if let Block::ImageReference {
        disclosure_target, ..
    } = &mut transcript.messages[0].content[0]
    {
        *disclosure_target = None;
    }
    assert!(restored == transcript);
    assert!(!format!("{restored:?}").contains("opaque-route"));
}

#[test]
fn checkpoints_refuse_truncation_unknown_fields_versions_and_noncanonical_json() {
    let bytes = response().checkpoint().unwrap();
    for end in 0..bytes.len() {
        assert!(NativeResponse::restore(&bytes[..end]).is_err());
    }
    for replacement in [b'D', b'2', 0] {
        let mut bad = bytes.clone();
        bad[3] = replacement;
        assert!(NativeResponse::restore(&bad).is_err());
    }
    let mut bad = bytes.clone();
    bad.extend_from_slice(b" ");
    assert!(NativeResponse::restore(&bad).is_err());
    let mut bad = bytes.clone();
    bad.splice(5..5, b"\"unknown\":1,".iter().copied());
    assert!(NativeResponse::restore(&bad).is_err());
    let mut bad = bytes.clone();
    bad.splice(5..5, b"\"blocks\":[],".iter().copied());
    assert!(NativeResponse::restore(&bad).is_err());
    assert!(TranscriptCheckpoint::restore(&bytes).is_err());
}

#[test]
fn invalid_response_and_transcript_values_fail_closed() {
    for block in [
        Block::ToolUse {
            id: "a".into(),
            name: "read".into(),
            input: json!(null),
        },
        Block::PartialStream {
            text: "partial".into(),
        },
        Block::ResponsesReasoning {
            item: json!({"type":"reasoning"}),
        },
    ] {
        let mut value = response();
        value.blocks = vec![block];
        assert!(value.checkpoint().is_err());
    }
    let mut value = response();
    value.blocks.push(value.blocks[3].clone());
    assert!(value.checkpoint().is_err());
    for cost in [f64::NAN, f64::INFINITY, -1.0] {
        let mut value = response();
        value.usage.cost_usd = Some(cost);
        assert!(value.checkpoint().is_err());
    }
    let value = TranscriptCheckpoint {
        route_binding: "route".into(),
        messages: vec![Message {
            role: "user".into(),
            content: response().blocks,
        }],
    };
    assert!(value.checkpoint().is_err());
}

#[test]
fn resource_bounds_precede_deserialization() {
    assert_eq!(
        NativeResponse::restore(&vec![0; MAX_CHECKPOINT_BYTES + 1]),
        Err(Error::Bounds)
    );
    let deep = format!("DNR1{}0{}", "[".repeat(65), "]".repeat(65));
    assert_eq!(NativeResponse::restore(deep.as_bytes()), Err(Error::Bounds));
    let wide = format!("DNR1[{}0]", "0,".repeat(131073));
    assert_eq!(NativeResponse::restore(wide.as_bytes()), Err(Error::Bounds));
    let mut value = response();
    value.blocks = vec![
        Block::Text {
            text: String::new()
        };
        4097
    ];
    assert!(value.checkpoint().is_err());
    let value = TranscriptCheckpoint {
        route_binding: "route".into(),
        messages: vec![
            Message {
                role: "user".into(),
                content: vec![]
            };
            4097
        ],
    };
    assert!(value.checkpoint().is_err());
}

#[test]
fn finite_costs_and_numeric_arguments_roundtrip_exactly() {
    let mut bits = 1u64;
    for _ in 0..4096 {
        bits = bits.wrapping_mul(6364136223846793005).wrapping_add(1);
        let cost = f64::from_bits(bits & !(1 << 63));
        if !cost.is_finite() {
            continue;
        }
        let mut value = response();
        value.usage.cost_usd = Some(cost);
        value.blocks = vec![Block::ToolUse {
            id: "n".into(),
            name: "numeric".into(),
            input: json!({"number":cost}),
        }];
        assert_eq!(
            NativeResponse::restore(&value.checkpoint().unwrap()).unwrap(),
            value
        );
    }
}

#[test]
fn encoder_decoder_depth_boundaries_agree_for_native_values() {
    for depth in 0..70 {
        let mut nested = json!(0);
        for _ in 0..depth {
            nested = json!([nested]);
        }
        let mut value = response();
        value.blocks = vec![Block::ToolUse {
            id: "call".into(),
            name: "inert".into(),
            input: json!({"nested": nested}),
        }];
        if let Ok(bytes) = value.checkpoint() {
            assert_eq!(
                NativeResponse::restore(&bytes).unwrap(),
                value,
                "depth {depth}"
            );
        }
        let transcript = TranscriptCheckpoint {
            route_binding: "route".into(),
            messages: vec![Message {
                role: "assistant".into(),
                content: value.blocks,
            }],
        };
        if let Ok(bytes) = transcript.checkpoint() {
            let restored = TranscriptCheckpoint::restore(&bytes).unwrap();
            assert_eq!(restored.checkpoint().unwrap(), bytes, "depth {depth}");
        }
    }
}

#[test]
fn legacy_session_json_contract_is_unchanged() {
    let legacy = json!({"role":"assistant","content":[{"type":"thinking","text":"legacy"},{"type":"tool_result","tool_use_id":"a","content":"result"}]});
    let message: Message = serde_json::from_value(legacy).unwrap();
    assert_eq!(
        serde_json::to_value(message).unwrap(),
        json!({"role":"assistant","content":[{"type":"thinking","thinking":"legacy"},{"type":"tool_result","tool_use_id":"a","content":"result"}]})
    );
}
