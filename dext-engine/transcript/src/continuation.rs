//! Response-application seam only; not a complete turn or dispatch authority.
use crate::checkpoint::{Error as CodecError, NativeResponse, TranscriptCheckpoint};
use crate::{Block, Message};
use dext_core::model::{ModelWait, Phase};
use dext_core::response::{ResponseProposal, ToolCall};
use serde_json::Value;

/// Shared CLI/managed-host extraction. History projection remains host policy;
/// executable arguments must never be replaced by a redacted history projection.
pub struct ResponseApplication(ResponseProposal<Block, ()>);
impl ResponseApplication {
    pub fn new(blocks: Vec<Block>) -> Result<Self, dext_core::response::Error> {
        Ok(Self(ResponseProposal::from_validated(blocks, (), true)?))
    }
    pub fn blocks(&self) -> &[Block] {
        self.0.blocks()
    }
    pub fn into_calls(self) -> Vec<ToolCall<Value>> {
        self.0.into_calls()
    }
}

/// The CLI preserves its existing adjacent-message suppression. Managed hosts use
/// effect identity instead: identical answers from distinct requests are retained.
pub fn append_assistant(
    history: &mut Vec<Message>,
    blocks: Vec<Block>,
    suppress_adjacent: bool,
) -> bool {
    if blocks.is_empty()
        || (suppress_adjacent
            && history
                .last()
                .is_some_and(|m| m.role == "assistant" && m.content == blocks))
    {
        return false;
    }
    history.push(Message {
        role: "assistant".into(),
        content: blocks,
    });
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    State,
    Binding,
    Checkpoint,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "response continuation: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<CodecError> for Error {
    fn from(_: CodecError) -> Self {
        Self::Checkpoint
    }
}

#[derive(Clone)]
pub struct ResponseContinuation {
    transcript: TranscriptCheckpoint,
    model: ModelWait<Vec<u8>>,
}
impl std::fmt::Debug for ResponseContinuation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseContinuation")
            .field("phase", &self.model.phase())
            .finish_non_exhaustive()
    }
}

impl ResponseContinuation {
    /// The host supplies a route-validated, retention-approved transcript and a
    /// waiting request. Publication must precede any physical model dispatch.
    pub fn waiting(
        transcript: TranscriptCheckpoint,
        model: ModelWait<Vec<u8>>,
    ) -> Result<Self, Error> {
        if model.phase() != Phase::Waiting {
            return Err(Error::State);
        }
        transcript.checkpoint()?;
        Ok(Self { transcript, model })
    }
    pub fn transcript(&self) -> &TranscriptCheckpoint {
        &self.transcript
    }
    pub fn into_transcript(self) -> TranscriptCheckpoint {
        self.transcript
    }
    pub fn model(&self) -> &ModelWait<Vec<u8>> {
        &self.model
    }

    /// Receipt ingress is host-owned. This speculative transition leaves the
    /// waiting baseline untouched; the host commits it and its intents together.
    pub fn apply(&self, recorded: &ModelWait<Vec<u8>>, current_route: &str) -> Result<Self, Error> {
        if self.model.phase() != Phase::Waiting || recorded.phase() != Phase::Recorded {
            return Err(Error::State);
        }
        if current_route != self.transcript.route_binding {
            return Err(Error::Binding);
        }
        let checked = self
            .model
            .record(
                recorded.request(),
                recorded.attempt(),
                recorded.response().ok_or(Error::State)?,
            )
            .map_err(|_| Error::Binding)?;
        let response = NativeResponse::restore(checked.response().ok_or(Error::State)?)?;
        let application =
            ResponseApplication::new(response.blocks).map_err(|_| Error::Checkpoint)?;
        let mut transcript = self.transcript.clone();
        append_assistant(
            &mut transcript.messages,
            application.blocks().to_vec(),
            false,
        );
        transcript.checkpoint()?;
        Ok(Self {
            transcript,
            model: checked.apply().map_err(|_| Error::State)?,
        })
    }

    /// Pending calls are inert values, not proof of approval or a replay policy.
    pub fn pending_calls(&self) -> Result<Vec<ToolCall<Value>>, Error> {
        if self.model.phase() != Phase::Applied {
            return Err(Error::State);
        }
        let response = NativeResponse::restore(self.model.response().ok_or(Error::State)?)?;
        Ok(ResponseApplication::new(response.blocks)
            .map_err(|_| Error::Checkpoint)?
            .into_calls())
    }

    pub fn checkpoint(&self) -> Result<Vec<u8>, Error> {
        let transcript = self.transcript.checkpoint()?;
        let model = self.model.checkpoint();
        let mut out = b"DNC1".to_vec();
        for field in [&transcript, &model] {
            out.extend_from_slice(
                &u32::try_from(field.len())
                    .map_err(|_| Error::Checkpoint)?
                    .to_be_bytes(),
            );
            out.extend_from_slice(field);
        }
        Ok(out)
    }
    pub fn restore(bytes: &[u8]) -> Result<Self, Error> {
        // Bound aggregate framing before either decoder allocates.
        const MAX: usize = crate::checkpoint::MAX_CHECKPOINT_BYTES
            + dext_core::model::MAX_BODY
            + dext_core::model::MAX_RESPONSE
            + 8 * 4096
            + 140;
        if bytes.len() > MAX || !bytes.starts_with(b"DNC1") {
            return Err(Error::Checkpoint);
        }
        fn field<'a>(input: &mut &'a [u8]) -> Result<&'a [u8], Error> {
            let header = input.get(..4).ok_or(Error::Checkpoint)?;
            let length =
                u32::from_be_bytes(header.try_into().map_err(|_| Error::Checkpoint)?) as usize;
            let value = input
                .get(4..4usize.checked_add(length).ok_or(Error::Checkpoint)?)
                .ok_or(Error::Checkpoint)?;
            *input = &input[4 + length..];
            Ok(value)
        }
        let mut input = &bytes[4..];
        let transcript = TranscriptCheckpoint::restore(field(&mut input)?)?;
        let model = ModelWait::restore(field(&mut input)?).map_err(|_| Error::Checkpoint)?;
        if !input.is_empty() || !matches!(model.phase(), Phase::Waiting | Phase::Applied) {
            return Err(Error::State);
        }
        if model.phase() == Phase::Applied {
            let response = NativeResponse::restore(model.response().ok_or(Error::State)?)?;
            if !response.blocks.is_empty() {
                let last = transcript.messages.last().ok_or(Error::Binding)?;
                if last.role != "assistant"
                    || crate::checkpoint::encode(b"DBL1", &last.content)?
                        != crate::checkpoint::encode(b"DBL1", &response.blocks)?
                {
                    return Err(Error::Binding);
                }
            }
        }
        Ok(Self { transcript, model })
    }
}
