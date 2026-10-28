use dext_core::response::{
    AssistantBlock, BlockKind, CallRef, Error, MAX_BLOCKS, MAX_CALLS, ResponseProposal, ToolCall,
};

#[derive(Clone, Debug, PartialEq)]
enum Block {
    Text(String),
    Reasoning {
        text: String,
        signature: String,
    },
    Call {
        id: String,
        name: String,
        input: Vec<u8>,
    },
    Partial,
    Invalid,
}
impl AssistantBlock for Block {
    type Input = Vec<u8>;
    fn kind(&self) -> BlockKind<'_> {
        match self {
            Self::Text(_) | Self::Reasoning { .. } => BlockKind::Content,
            Self::Call { id, name, .. } => BlockKind::CompleteCall(CallRef { id, name }),
            Self::Partial => BlockKind::IncompleteCall,
            Self::Invalid => BlockKind::Invalid,
        }
    }
    fn into_call(self) -> Option<ToolCall<Self::Input>> {
        match self {
            Self::Call { id, name, input } => Some(ToolCall { id, name, input }),
            _ => None,
        }
    }
}
fn call(id: &str) -> Block {
    Block::Call {
        id: id.into(),
        name: "test-tool".into(),
        input: b"validated-input".to_vec(),
    }
}

#[test]
fn proposal_preserves_block_order_metadata_and_reasoning() {
    let blocks = vec![
        Block::Text("before".into()),
        call("second-lexically"),
        Block::Reasoning {
            text: "private-reasoning".into(),
            signature: "opaque-signature".into(),
        },
        call("first-lexically"),
        Block::Text("after".into()),
    ];
    let metadata = ("max_tokens", 123u64, 1usize);
    let proposal = ResponseProposal::from_validated(blocks.clone(), metadata, true).unwrap();
    assert_eq!(proposal.blocks(), blocks);
    assert_eq!(proposal.metadata(), &metadata);
    assert_eq!(proposal.call_count(), 2);
    let calls = ResponseProposal::from_validated(blocks.clone(), metadata, true)
        .unwrap()
        .into_calls();
    assert_eq!(
        calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["second-lexically", "first-lexically"]
    );
    assert_eq!(calls[0].input, b"validated-input");
    assert_eq!(proposal.into_parts(), (blocks, metadata));
}

#[test]
fn refuses_partial_unfinalized_invalid_or_ambiguous_calls_atomically() {
    assert!(matches!(
        ResponseProposal::from_validated(vec![call("a")], (), false),
        Err(Error::Unfinalized)
    ));
    for (bad, error) in [
        (Block::Partial, Error::IncompleteCall),
        (Block::Invalid, Error::InvalidBlock),
        (call("a"), Error::DuplicateIdentity),
        (call(" "), Error::MissingIdentity),
    ] {
        assert!(
            matches!(ResponseProposal::from_validated(vec![call("a"), bad], (), true), Err(e) if e == error)
        );
    }
    let mut empty_name = call("b");
    if let Block::Call { name, .. } = &mut empty_name {
        name.clear();
    }
    assert!(matches!(
        ResponseProposal::from_validated(vec![empty_name], (), true),
        Err(Error::MissingIdentity)
    ));
}

#[test]
fn complete_calls_survive_dropped_partial_call_metadata() {
    // The native parser has already removed the cut call, but retains its count.
    let proposal =
        ResponseProposal::from_validated(vec![call("complete")], ("length", 1usize), true).unwrap();
    assert_eq!(proposal.call_count(), 1);
    assert_eq!(proposal.metadata().1, 1);
    assert_eq!(proposal.into_calls()[0].id, "complete");
}

#[test]
fn bounds_and_debug_are_explicit() {
    let calls: Vec<_> = (0..MAX_CALLS).map(|i| call(&format!("call-{i}"))).collect();
    assert!(ResponseProposal::from_validated(calls.clone(), (), true).is_ok());
    let mut over = calls;
    over.push(call("overflow"));
    assert!(matches!(
        ResponseProposal::from_validated(over, (), true),
        Err(Error::Bounds)
    ));
    assert!(matches!(
        ResponseProposal::from_validated(
            vec![Block::Text(String::new()); MAX_BLOCKS + 1],
            (),
            true
        ),
        Err(Error::Bounds)
    ));
    let proposal = ResponseProposal::from_validated(
        vec![call("secret-id"), Block::Text("secret-content".into())],
        "secret-metadata",
        true,
    )
    .unwrap();
    assert!(!format!("{proposal:?}").contains("secret"));
    assert!(!format!("{:?}", proposal.into_calls()).contains("secret"));
    assert!(
        ResponseProposal::<Block, ()>::from_validated(vec![], (), true)
            .unwrap()
            .into_calls()
            .is_empty()
    );
}
