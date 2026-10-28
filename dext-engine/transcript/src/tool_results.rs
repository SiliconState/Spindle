//! Inert result adoption for one applied model response. Hosts authenticate and
//! fence receipts before calling this seam; no tool or model is dispatched here.
use crate::checkpoint::{TranscriptCheckpoint, decode, encode};
use crate::continuation::{Error, ResponseContinuation};
use crate::{Block, Message};
use dext_core::response::ToolCall;
use serde_json::Value;

/// Shared message construction only. Native CLI policy may include image references;
/// this helper does not validate receipts or authorize replay of those references.
pub fn result_message(blocks: Vec<Block>) -> Message {
    Message {
        role: "user".into(),
        content: blocks,
    }
}

#[derive(Clone)]
pub struct ToolResultsContinuation {
    response: ResponseContinuation,
    results: Vec<Option<Vec<Block>>>,
}

impl std::fmt::Debug for ToolResultsContinuation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolResultsContinuation")
            .field("calls", &self.results.len())
            .field(
                "received",
                &self.results.iter().filter(|r| r.is_some()).count(),
            )
            .finish_non_exhaustive()
    }
}

impl ToolResultsContinuation {
    pub fn new(response: ResponseContinuation) -> Result<Self, Error> {
        let calls = response.pending_calls()?;
        if calls.is_empty() {
            return Err(Error::State);
        }
        let next = Self {
            response,
            results: vec![None; calls.len()],
        };
        next.checkpoint()?;
        Ok(next)
    }

    pub fn complete(&self) -> bool {
        self.results.iter().all(Option::is_some)
    }

    pub fn model_effect(&self) -> &str {
        &self.response.model().request().identity().effect
    }

    pub fn model_attempt(&self) -> u32 {
        self.response.model().attempt()
    }

    pub fn model_contract(&self) -> &str {
        &self.response.model().request().identity().contract
    }

    pub fn retained_result_blocks(&self) -> impl Iterator<Item = &Block> {
        self.results.iter().flatten().flatten()
    }

    pub fn all_calls(&self) -> Result<Vec<ToolCall<Value>>, Error> {
        self.response.pending_calls()
    }

    pub fn pending_calls(&self) -> Result<Vec<ToolCall<Value>>, Error> {
        Ok(self
            .response
            .pending_calls()?
            .into_iter()
            .zip(&self.results)
            .filter_map(|(call, result)| result.is_none().then_some(call))
            .collect())
    }

    /// A trusted host supplies the parent model identity/attempt and exact call.
    /// Tool physical attempts, grants, epochs and outcome selection remain host-owned.
    pub fn observe(
        &self,
        model_effect: &str,
        model_attempt: u32,
        call: &ToolCall<Value>,
        result: Block,
    ) -> Result<Self, Error> {
        self.observe_blocks(model_effect, model_attempt, call, vec![result])
    }

    /// The first block is the call's result; only immutable image artifacts may
    /// follow. Artifact/provenance checks remain the host's commit responsibility.
    pub fn observe_blocks(
        &self,
        model_effect: &str,
        model_attempt: u32,
        call: &ToolCall<Value>,
        blocks: Vec<Block>,
    ) -> Result<Self, Error> {
        if model_effect != self.response.model().request().identity().effect
            || model_attempt != self.response.model().attempt()
        {
            return Err(Error::Binding);
        }
        let calls = self.response.pending_calls()?;
        let index = calls
            .iter()
            .position(|expected| expected.id == call.id && expected.name == call.name)
            .ok_or(Error::Binding)?;
        if encode(b"DCA1", &calls[index].input)? != encode(b"DCA1", &call.input)? {
            return Err(Error::Binding);
        }
        validate_result_blocks(&call.id, &blocks)?;
        if let Some(existing) = &self.results[index] {
            return if existing == &blocks {
                Ok(self.clone())
            } else {
                Err(Error::Binding)
            };
        }
        let mut next = self.clone();
        next.results[index] = Some(blocks);
        next.checkpoint()?;
        Ok(next)
    }

    /// Partial batches keep the original transcript. A complete batch contributes
    /// exactly one result message in original call order, never arrival order.
    pub fn transcript(&self) -> Result<TranscriptCheckpoint, Error> {
        let mut transcript = self.response.transcript().clone();
        if self.complete() {
            transcript.messages.push(result_message(
                self.results.iter().flatten().flatten().cloned().collect(),
            ));
        }
        transcript.validate()?;
        Ok(transcript)
    }

    pub fn checkpoint(&self) -> Result<Vec<u8>, Error> {
        let response = self.response.checkpoint()?;
        let results = encode(b"DTR2", &self.results)?;
        // A tool-result continuation is intentionally smaller than DNC1's maximum:
        // large request/history/result combinations need artifact references first.
        let total = 12usize
            .checked_add(response.len())
            .and_then(|n| n.checked_add(results.len()))
            .ok_or(Error::Checkpoint)?;
        if total > crate::checkpoint::MAX_CHECKPOINT_BYTES {
            return Err(Error::Checkpoint);
        }
        let mut eventual = self.response.transcript().clone();
        let calls = self.response.pending_calls()?;
        let minimal_results: Vec<Option<Vec<Block>>> = calls
            .into_iter()
            .zip(&self.results)
            .map(|(call, result)| {
                Some(result.clone().unwrap_or_else(|| {
                    vec![Block::ToolResult {
                        tool_use_id: call.id,
                        content: String::new(),
                        is_error: None,
                        metadata: Default::default(),
                    }]
                }))
            })
            .collect();
        let minimal_bytes = encode(b"DTR2", &minimal_results)?;
        if 12usize
            .checked_add(response.len())
            .and_then(|n| n.checked_add(minimal_bytes.len()))
            .is_none_or(|n| n > crate::checkpoint::MAX_CHECKPOINT_BYTES)
        {
            return Err(Error::Checkpoint);
        }
        eventual.messages.push(result_message(
            minimal_results.into_iter().flatten().flatten().collect(),
        ));
        // Refuse batches that cannot publish even minimal results. Actual outputs
        // still need independent bounded admission; this is not disk reservation.
        eventual.checkpoint()?;
        let mut out = b"DTC2".to_vec();
        for field in [response, results] {
            out.extend_from_slice(&(field.len() as u32).to_be_bytes());
            out.extend(field);
        }
        Ok(out)
    }

    pub fn restore(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > crate::checkpoint::MAX_CHECKPOINT_BYTES || !bytes.starts_with(b"DTC2") {
            return Err(Error::Checkpoint);
        }
        fn field<'a>(input: &mut &'a [u8]) -> Result<&'a [u8], Error> {
            let header = input.get(..4).ok_or(Error::Checkpoint)?;
            let length =
                u32::from_be_bytes(header.try_into().map_err(|_| Error::Checkpoint)?) as usize;
            let end = 4usize.checked_add(length).ok_or(Error::Checkpoint)?;
            let value = input.get(4..end).ok_or(Error::Checkpoint)?;
            *input = &input[end..];
            Ok(value)
        }
        let mut input = &bytes[4..];
        let response = ResponseContinuation::restore(field(&mut input)?)?;
        let results: Vec<Option<Vec<Block>>> = decode(b"DTR2", field(&mut input)?)?;
        let calls = response.pending_calls()?;
        if !input.is_empty() || calls.is_empty() || results.len() != calls.len() {
            return Err(Error::State);
        }
        for (call, result) in calls.iter().zip(&results) {
            if let Some(blocks) = result {
                validate_result_blocks(&call.id, blocks)?;
            }
        }
        let next = Self { response, results };
        if next.checkpoint()? != bytes {
            return Err(Error::Checkpoint);
        }
        Ok(next)
    }
}

fn validate_result_blocks(call_id: &str, blocks: &[Block]) -> Result<(), Error> {
    if blocks.is_empty()
        || blocks.len() > 9
        || !matches!(&blocks[0], Block::ToolResult { tool_use_id, .. } if tool_use_id == call_id)
    {
        return Err(Error::Binding);
    }
    let mut images = std::collections::BTreeSet::new();
    for block in &blocks[1..] {
        match block {
            Block::ImageArtifact { reference }
                if reference.is_valid() && images.insert((&reference.scope, &reference.digest)) => {
            }
            _ => return Err(Error::Binding),
        }
    }
    Ok(())
}
