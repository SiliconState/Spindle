//! Bounded composition of the shared CLI turn cursor and native value checkpoints.
//! Hosts own request construction, policy review, authorization and every I/O effect.
use crate::checkpoint::{NativeResponse, TranscriptCheckpoint, decode, encode};
use crate::continuation::{Error, ResponseContinuation};
use crate::runtime::TurnRuntimeState;
use crate::tool_results::ToolResultsContinuation;
use crate::{Block, Message};
use dext_core::model::{ModelWait, PreparedRequest};
use dext_core::recovery::{Contract, stream_recovery_reason};
use dext_core::response::ToolCall;
use dext_core::turn::{Phase, Stop, TurnControl};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Serialize, Deserialize)]
struct Header {
    scope: String,
    execution: String,
    contract: u8,
    max_iterations: Option<u32>,
    /// Batch positions answered by a shared guard verdict instead of dispatch;
    /// non-empty only while tools are pending.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    guarded: Vec<u32>,
}
#[derive(Clone)]
enum State {
    Transcript(TranscriptCheckpoint),
    Model(ResponseContinuation),
    Tools(ToolResultsContinuation),
}
#[derive(Clone)]
pub struct TurnContinuation {
    header: Header,
    control: TurnControl,
    state: State,
    runtime: TurnRuntimeState,
}
impl std::fmt::Debug for TurnContinuation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnContinuation")
            .field("control", &self.control)
            .finish_non_exhaustive()
    }
}

/// A pending obligation only, not a permission grant or proof of committed intent.
pub enum Effect {
    Model {
        id: String,
        waiting: ModelWait<Vec<u8>>,
    },
    Tool {
        id: String,
        call: ToolCall<Value>,
    },
}
impl std::fmt::Debug for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Model { .. } => f.write_str("Effect::Model(..)"),
            Self::Tool { .. } => f.write_str("Effect::Tool(..)"),
        }
    }
}

fn label(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
}
fn contract_tag(contract: Contract) -> u8 {
    match contract {
        Contract::AnthropicMessages => 0,
        Contract::OpenAiChatCompletions => 1,
        Contract::OpenAiResponses => 2,
        Contract::ChatGptResponses => 3,
    }
}
fn contract(tag: u8) -> Result<Contract, Error> {
    match tag {
        0 => Ok(Contract::AnthropicMessages),
        1 => Ok(Contract::OpenAiChatCompletions),
        2 => Ok(Contract::OpenAiResponses),
        3 => Ok(Contract::ChatGptResponses),
        _ => Err(Error::Checkpoint),
    }
}
fn contract_name(tag: u8) -> Result<&'static str, Error> {
    match contract(tag)? {
        Contract::AnthropicMessages => Ok("anthropic-messages"),
        Contract::OpenAiChatCompletions => Ok("openai-chat-completions"),
        Contract::OpenAiResponses => Ok("openai-responses"),
        Contract::ChatGptResponses => Ok("chatgpt-responses"),
    }
}

impl TurnContinuation {
    pub fn new(
        scope: String,
        execution: String,
        contract: Contract,
        max_iterations: Option<u32>,
        transcript: TranscriptCheckpoint,
    ) -> Result<Self, Error> {
        let next = Self {
            header: Header {
                scope,
                execution,
                contract: contract_tag(contract),
                max_iterations,
                guarded: Vec::new(),
            },
            control: TurnControl::default(),
            state: State::Transcript(transcript),
            runtime: TurnRuntimeState::new(),
        };
        next.checkpoint()?;
        Ok(next)
    }
    pub fn scope(&self) -> &str {
        &self.header.scope
    }
    pub fn execution(&self) -> &str {
        &self.header.execution
    }
    pub fn control(&self) -> TurnControl {
        self.control
    }
    /// Shared CLI decision state for this turn; it never grants authority.
    pub fn runtime(&self) -> &TurnRuntimeState {
        &self.runtime
    }
    /// Adopts host-computed runtime decisions in one checkpointed transition. Only
    /// monotone successors are accepted so a resumed turn cannot reset breakers.
    pub fn with_runtime(&self, runtime: TurnRuntimeState) -> Result<Self, Error> {
        if !runtime.is_successor_of(&self.runtime) {
            return Err(Error::State);
        }
        let mut next = self.clone();
        next.runtime = runtime;
        next.checkpoint()?;
        Ok(next)
    }
    /// The shared CLI pre-dispatch guards for one still-pending tool call, against
    /// the runtime state at the start of this batch: the CLI plans every call of a
    /// round before any outcome is observed. `None` means no guard fired.
    pub fn guard_tool(&self, effect_id: &str) -> Result<Option<Self>, Error> {
        let State::Tools(tools) = &self.state else {
            return Err(Error::State);
        };
        if self.control.phase() != Phase::ToolsWait {
            return Err(Error::State);
        }
        let pending = tools.pending_calls()?;
        let (position, call) = tools
            .all_calls()?
            .into_iter()
            .enumerate()
            .find(|(position, _)| self.tool_id(*position) == effect_id)
            .filter(|(_, call)| pending.iter().any(|p| p.id == call.id))
            .ok_or(Error::Binding)?;
        // Preflight only advances guard telemetry, never a later verdict.
        let mut runtime = self.runtime.clone();
        let keys = crate::runtime::ToolCallKeys::derive(&call.name, &call.input);
        let Some(verdict) = runtime.preflight(&keys) else {
            return Ok(None);
        };
        let blocks = vec![Block::ToolResult {
            tool_use_id: call.id.clone(),
            content: verdict.content,
            is_error: verdict.is_error,
            metadata: Default::default(),
        }];
        let proposed =
            tools.observe_blocks(tools.model_effect(), tools.model_attempt(), &call, blocks)?;
        let mut guarded = self.header.guarded.clone();
        guarded.push(u32::try_from(position).map_err(|_| Error::Binding)?);
        guarded.sort_unstable();
        self.after_result(proposed, guarded, runtime).map(Some)
    }

    /// Host policy refusal of one pending call it will never dispatch (an unknown
    /// tool, or input its executor rejects). Recorded like a guard verdict: the call
    /// gets an error result and is never observed as having run.
    pub fn refuse_tool(&self, effect_id: &str, reason: &str) -> Result<Self, Error> {
        let State::Tools(tools) = &self.state else {
            return Err(Error::State);
        };
        if self.control.phase() != Phase::ToolsWait
            || reason.trim().is_empty()
            || reason.len() > 4096
        {
            return Err(Error::State);
        }
        let pending = tools.pending_calls()?;
        let (position, call) = tools
            .all_calls()?
            .into_iter()
            .enumerate()
            .find(|(position, _)| self.tool_id(*position) == effect_id)
            .filter(|(_, call)| pending.iter().any(|p| p.id == call.id))
            .ok_or(Error::Binding)?;
        let blocks = vec![Block::ToolResult {
            tool_use_id: call.id.clone(),
            content: reason.into(),
            is_error: Some(true),
            metadata: Default::default(),
        }];
        let proposed =
            tools.observe_blocks(tools.model_effect(), tools.model_attempt(), &call, blocks)?;
        let mut guarded = self.header.guarded.clone();
        guarded.push(u32::try_from(position).map_err(|_| Error::Binding)?);
        guarded.sort_unstable();
        self.after_result(proposed, guarded, self.runtime.clone())
    }

    /// Outcomes are observed only when the batch completes, in call order, exactly
    /// like the CLI result loop, so arrival order cannot change runtime decisions.
    /// Observation may append breaker notes to result text, as the CLI does.
    fn after_result(
        &self,
        proposed: ToolResultsContinuation,
        guarded: Vec<u32>,
        runtime: TurnRuntimeState,
    ) -> Result<Self, Error> {
        let mut next = self.clone();
        next.runtime = runtime;
        if !proposed.complete() {
            next.header.guarded = guarded;
            next.state = State::Tools(proposed);
            next.checkpoint()?;
            return Ok(next);
        }
        let calls = proposed.all_calls()?;
        let mut transcript = proposed.transcript()?;
        let results = transcript.messages.last_mut().ok_or(Error::State)?;
        for block in &mut results.content {
            if let Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } = block
            {
                let position = calls
                    .iter()
                    .position(|call| call.id == *tool_use_id)
                    .ok_or(Error::Binding)?;
                let call = &calls[position];
                let ran = !guarded.contains(&(position as u32));
                next.runtime
                    .observe_tool_result(&call.name, &call.input, content, *is_error, ran);
            }
        }
        next.control = self.control.tools_applied().map_err(|_| Error::State)?;
        next.header.guarded = Vec::new();
        next.state = State::Transcript(transcript);
        next.checkpoint()?;
        Ok(next)
    }
    pub fn transcript(&self) -> Result<TranscriptCheckpoint, Error> {
        self.transcript_ref().map(std::borrow::Cow::into_owned)
    }
    /// `transcript`, borrowed where the continuation holds it as it is (a tool batch's
    /// is built: a copy, with its results appended once the batch is complete).
    pub fn transcript_ref(&self) -> Result<std::borrow::Cow<'_, TranscriptCheckpoint>, Error> {
        match &self.state {
            State::Transcript(transcript) => Ok(std::borrow::Cow::Borrowed(transcript)),
            State::Model(wait) => Ok(std::borrow::Cow::Borrowed(wait.transcript())),
            State::Tools(tools) => tools.transcript().map(std::borrow::Cow::Owned),
        }
    }
    /// `transcript`, taking the continuation instead of copying from it.
    pub fn into_transcript(self) -> Result<TranscriptCheckpoint, Error> {
        match self.state {
            State::Transcript(transcript) => Ok(transcript),
            State::Model(wait) => Ok(wait.into_transcript()),
            State::Tools(tools) => tools.transcript(),
        }
    }
    fn model_id(&self, ordinal: u64) -> String {
        format!("{}/m/{ordinal}", self.header.execution)
    }
    fn tool_id(&self, position: usize) -> String {
        format!(
            "{}/t/{position}",
            self.model_id(self.control.request_ordinal())
        )
    }
    pub fn next_model_id(&self) -> Result<String, Error> {
        if !matches!(self.control.phase(), Phase::Ready | Phase::AfterTools) {
            return Err(Error::State);
        }
        Ok(self.model_id(
            self.control
                .request_ordinal()
                .checked_add(1)
                .ok_or(Error::State)?,
        ))
    }

    /// Request bytes and configuration come from Dext's existing native builder.
    /// This prepares one speculative wait; host admission commits it before send.
    pub fn prepare(
        &self,
        request: PreparedRequest<Vec<u8>>,
        current_route: &str,
    ) -> Result<Self, Error> {
        let State::Transcript(transcript) = &self.state else {
            return Err(Error::State);
        };
        if current_route != transcript.route_binding {
            return Err(Error::Binding);
        }
        let mut next = self.clone();
        next.control = self
            .control
            .begin_round(self.header.max_iterations)
            .map_err(|_| Error::State)?;
        if next.control.phase() == Phase::Stopped(Stop::Iterations) {
            next.checkpoint()?;
            return Ok(next);
        }
        next.control = next.control.prepare_request().map_err(|_| Error::State)?;
        if request.identity().effect != next.model_id(next.control.request_ordinal())
            || request.identity().contract != contract_name(self.header.contract)?
        {
            return Err(Error::Binding);
        }
        let waiting = ModelWait::new(request).start().map_err(|_| Error::State)?;
        next.state = State::Model(ResponseContinuation::waiting(transcript.clone(), waiting)?);
        next.checkpoint()?;
        Ok(next)
    }

    pub fn apply_model(
        &self,
        recorded: &ModelWait<Vec<u8>>,
        current_route: &str,
    ) -> Result<Self, Error> {
        let State::Model(wait) = &self.state else {
            return Err(Error::State);
        };
        let applied = wait.apply(recorded, current_route)?;
        let response = NativeResponse::restore(applied.model().response().ok_or(Error::State)?)?;
        let calls = applied.pending_calls()?;
        let reason = stream_recovery_reason(
            contract(self.header.contract)?,
            response.stop_reason.as_deref(),
            response.unfinished_tool_calls,
        );
        let (control, _) = self
            .control
            .response(reason.as_deref(), !calls.is_empty())
            .map_err(|_| Error::State)?;
        let mut next = self.clone();
        next.control = control;
        next.state = if control.phase() == Phase::ToolsWait {
            State::Tools(ToolResultsContinuation::new(applied)?)
        } else {
            State::Transcript(applied.transcript().clone())
        };
        next.checkpoint()?;
        Ok(next)
    }

    /// An executed (dispatched) result for one pending call.
    pub fn apply_tool(&self, effect_id: &str, blocks: Vec<Block>) -> Result<Self, Error> {
        let State::Tools(tools) = &self.state else {
            return Err(Error::State);
        };
        if self.control.phase() != Phase::ToolsWait {
            return Err(Error::State);
        }
        let call = tools
            .all_calls()?
            .into_iter()
            .enumerate()
            .find_map(|(position, call)| (self.tool_id(position) == effect_id).then_some(call))
            .ok_or(Error::Binding)?;
        let proposed =
            tools.observe_blocks(tools.model_effect(), tools.model_attempt(), &call, blocks)?;
        self.after_result(proposed, self.header.guarded.clone(), self.runtime.clone())
    }

    /// Fresh host policy input, after hooks/steering/objective checks. The cursor
    /// never interprets a tool-free model answer as unconditional completion.
    pub fn finish(&self) -> Result<Self, Error> {
        let mut next = self.clone();
        next.control = self.control.finish().map_err(|_| Error::State)?;
        next.checkpoint()?;
        Ok(next)
    }
    pub fn followup(&self, text: String) -> Result<Self, Error> {
        if text.trim().is_empty() {
            return Err(Error::Binding);
        }
        let State::Transcript(transcript) = &self.state else {
            return Err(Error::State);
        };
        if !matches!(
            self.control.phase(),
            Phase::FinalReview | Phase::Ready | Phase::AfterTools
        ) {
            return Err(Error::State);
        }
        let mut next = self.clone();
        if self.control.phase() == Phase::FinalReview {
            next.control = self.control.final_followup().map_err(|_| Error::State)?;
        }
        let mut transcript = transcript.clone();
        transcript.messages.push(Message {
            role: "user".into(),
            content: vec![Block::Text { text }],
        });
        next.state = State::Transcript(transcript);
        next.checkpoint()?;
        Ok(next)
    }
    /// A task report may reopen a completed turn without starting another execution.
    pub fn task_report(&self, text: String) -> Result<Self, Error> {
        if self.control.phase() != Phase::Stopped(Stop::Complete) {
            return self.followup(text);
        }
        let mut next = self.clone();
        next.control = next.control.task_followup().map_err(|_| Error::State)?;
        next.followup(text)
    }

    /// Managed equivalent of the CLI's queued steering injection. The injection
    /// point comes from the checkpointed phase, never from the host, and the update
    /// moves the shared runtime to Synthesize exactly as the CLI does.
    pub fn steer(&self, update: String) -> Result<Self, Error> {
        if update.trim().is_empty() || update.len() > 64 * 1024 {
            return Err(Error::Binding);
        }
        let State::Transcript(transcript) = &self.state else {
            return Err(Error::State);
        };
        use crate::runtime::SteeringPoint;
        let point = match self.control.phase() {
            Phase::FinalReview => SteeringPoint::BeforeFinal,
            Phase::AfterTools => SteeringPoint::AfterTools,
            // Nothing is in flight: the update simply precedes the next request.
            Phase::Ready => SteeringPoint::BeforeRequest,
            _ => return Err(Error::State),
        };
        let tool_count = transcript
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter(|block| matches!(block, Block::ToolResult { .. }))
            .count();
        let text = crate::runtime::steering_update_text(
            self.runtime.phase(),
            self.control.iterations(),
            tool_count,
            point,
            &update,
            "",
        );
        let mut next = self.clone();
        if point == SteeringPoint::BeforeFinal {
            next.control = self.control.final_followup().map_err(|_| Error::State)?;
        }
        next.runtime
            .advance_phase(crate::runtime::PhaseTrigger::Steering);
        let mut transcript = transcript.clone();
        transcript.messages.push(Message {
            role: "user".into(),
            content: vec![Block::Text { text }],
        });
        next.state = State::Transcript(transcript);
        next.checkpoint()?;
        Ok(next)
    }
    /// Managed equivalent of the CLI's pre-request budget stop. `usage` is the
    /// host's priced session usage; the turn stops only when the cap is reached.
    pub fn stop_for_budget(
        &self,
        usd: Option<f64>,
        tokens: Option<u64>,
        usage: &crate::Usage,
    ) -> Result<Self, Error> {
        let valid_cap = (usd.is_some() || tokens.is_some())
            && usd.is_none_or(|v| v.is_finite() && v > 0.0)
            && tokens != Some(0);
        if !valid_cap || !usage.is_valid() {
            return Err(Error::Binding);
        }
        let State::Transcript(transcript) = &self.state else {
            return Err(Error::State);
        };
        // The CLI checks its cap only before a new round; FinalReview first finishes
        // or follows up (to Ready), so it is never a budget-stop point.
        if !matches!(self.control.phase(), Phase::Ready | Phase::AfterTools) {
            return Err(Error::State);
        }
        let text = crate::runtime::budget_denial_text(usd, tokens, usage).ok_or(Error::State)?;
        let mut next = self.clone();
        next.control = self
            .control
            .stop(dext_core::turn::Stop::Budget)
            .map_err(|_| Error::State)?;
        let mut transcript = transcript.clone();
        transcript.messages.push(Message {
            role: "assistant".into(),
            content: vec![Block::Text { text }],
        });
        next.state = State::Transcript(transcript);
        next.checkpoint()?;
        Ok(next)
    }

    /// Replaces settled history with a host-produced compaction (the summary
    /// request is its own model effect). Same route binding only, no pending
    /// effects; restored scoped images must still belong to this scope. Allowed
    /// before a model request and on a completed turn (before the next one starts).
    pub fn compact(&self, replacement: TranscriptCheckpoint) -> Result<Self, Error> {
        let State::Transcript(transcript) = &self.state else {
            return Err(Error::State);
        };
        // A settled turn may also be compacted, so the next turn can start from the
        // compacted history (a new turn's initial history must extend the prior one).
        if !matches!(
            self.control.phase(),
            Phase::Ready | Phase::AfterTools | Phase::Stopped(Stop::Complete)
        ) {
            return Err(Error::State);
        }
        if replacement.route_binding != transcript.route_binding
            || replacement.messages.is_empty()
            || replacement.messages.len() > transcript.messages.len()
        {
            return Err(Error::Binding);
        }
        // Images reach history only through provenance-checked adoption; a summary
        // may keep existing ones but never introduce new ones.
        let prior: Vec<&Block> = transcript
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter(|block| {
                matches!(
                    block,
                    Block::ImageReference { .. } | Block::ImageArtifact { .. }
                )
            })
            .collect();
        if replacement
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter(|block| {
                matches!(
                    block,
                    Block::ImageReference { .. } | Block::ImageArtifact { .. }
                )
            })
            .any(|block| !prior.contains(&block))
        {
            return Err(Error::Binding);
        }
        let mut next = self.clone();
        next.state = State::Transcript(replacement);
        next.checkpoint()?;
        Ok(next)
    }

    /// Forks only a completed, effect-free cut. Historical transcript stays intact;
    /// future effect identity is fresh. This grants no approval or workspace authority.
    pub fn fork_settled(&self, execution: String) -> Result<Self, Error> {
        if self.control.phase() != Phase::Stopped(Stop::Complete)
            || !matches!(self.state, State::Transcript(_))
            || !self.pending_effects()?.is_empty()
            || execution == self.header.execution
        {
            return Err(Error::State);
        }
        let mut next = self.clone();
        next.header.execution = execution;
        next.checkpoint()?;
        Ok(next)
    }

    /// Forks an idle mid-turn cut whose next action is a model request: nothing is
    /// in flight, and the fresh execution makes the re-run request a new effect, so
    /// a result recorded for the original request can never be adopted here.
    /// This grants no approval or workspace authority.
    pub fn fork_before_model(&self, execution: String) -> Result<Self, Error> {
        if !matches!(self.control.phase(), Phase::Ready | Phase::AfterTools)
            || !matches!(self.state, State::Transcript(_))
            || !self.pending_effects()?.is_empty()
            || execution == self.header.execution
        {
            return Err(Error::State);
        }
        let mut next = self.clone();
        next.header.execution = execution;
        next.checkpoint()?;
        Ok(next)
    }

    /// Starts a new owned execution from completed history. Managed images need an
    /// explicit scoped-artifact transfer contract, not raw scope substitution.
    pub fn restart_settled(
        &self,
        scope: String,
        execution: String,
        prompt: String,
    ) -> Result<Self, Error> {
        if self.control.phase() != Phase::Stopped(Stop::Complete)
            || !matches!(self.state, State::Transcript(_))
            || !self.pending_effects()?.is_empty()
            || scope == self.header.scope
            || execution == self.header.execution
            || prompt.trim().is_empty()
        {
            return Err(Error::State);
        }
        let mut transcript = self.transcript()?;
        if transcript
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| {
                matches!(
                    block,
                    Block::ImageArtifact { .. } | Block::ImageReference { .. }
                )
            })
        {
            return Err(Error::Binding);
        }
        transcript.messages.push(Message {
            role: "user".into(),
            content: vec![Block::Text { text: prompt }],
        });
        Self::new(
            scope,
            execution,
            contract(self.header.contract)?,
            self.header.max_iterations,
            transcript,
        )
    }

    /// Resumes a turn halted by the step limit with a fresh step count. The limit
    /// stops only between rounds, so history is whole and no effect is pending.
    pub fn extend_steps(&self) -> Result<Self, Error> {
        if !matches!(self.state, State::Transcript(_)) || !self.pending_effects()?.is_empty() {
            return Err(Error::State);
        }
        let mut next = self.clone();
        next.control = self.control.extend_iterations().map_err(|_| Error::State)?;
        next.checkpoint()?;
        Ok(next)
    }

    /// Cleanup/reconciliation of outstanding effects remains host-owned.
    pub fn cancel(&self) -> Result<Self, Error> {
        let mut next = self.clone();
        next.control = self
            .control
            .stop(Stop::Cancelled)
            .map_err(|_| Error::State)?;
        next.header.guarded = Vec::new();
        next.state = State::Transcript(self.transcript()?);
        next.checkpoint()?;
        Ok(next)
    }
    pub fn pending_effects(&self) -> Result<Vec<Effect>, Error> {
        match &self.state {
            State::Model(wait) => Ok(vec![Effect::Model {
                id: wait.model().request().identity().effect.clone(),
                waiting: wait.model().clone(),
            }]),
            State::Tools(tools) => {
                let pending = tools.pending_calls()?;
                Ok(tools
                    .all_calls()?
                    .into_iter()
                    .enumerate()
                    .filter_map(|(position, call)| {
                        pending
                            .iter()
                            .any(|p| p.id == call.id)
                            .then(|| Effect::Tool {
                                id: self.tool_id(position),
                                call,
                            })
                    })
                    .collect())
            }
            State::Transcript(_) => Ok(vec![]),
        }
    }

    fn validate(&self) -> Result<(), Error> {
        if !label(&self.header.scope, 128) || !label(&self.header.execution, 64) {
            return Err(Error::Binding);
        }
        contract(self.header.contract)?;
        if !self.header.guarded.is_empty() {
            let State::Tools(tools) = &self.state else {
                return Err(Error::State);
            };
            let calls = tools.all_calls()?;
            let pending = tools.pending_calls()?;
            if !self.header.guarded.windows(2).all(|pair| pair[0] < pair[1])
                || self.header.guarded.iter().any(|position| {
                    calls
                        .get(*position as usize)
                        .is_none_or(|call| pending.iter().any(|p| p.id == call.id))
                })
            {
                return Err(Error::State);
            }
        }
        TurnControl::restore(&self.control.checkpoint()).map_err(|_| Error::Checkpoint)?;
        let phase = self.control.phase();
        match &self.state {
            State::Transcript(_)
                if matches!(
                    phase,
                    Phase::Ready | Phase::AfterTools | Phase::FinalReview | Phase::Stopped(_)
                ) => {}
            State::Model(wait) if phase == Phase::ModelWait => {
                if wait.model().phase() != dext_core::model::Phase::Waiting
                    || wait.model().request().identity().effect
                        != self.model_id(self.control.request_ordinal())
                    || wait.model().request().identity().contract
                        != contract_name(self.header.contract)?
                {
                    return Err(Error::Binding);
                }
            }
            State::Tools(tools) if phase == Phase::ToolsWait && !tools.complete() => {
                if tools.model_effect() != self.model_id(self.control.request_ordinal())
                    || tools.model_contract() != contract_name(self.header.contract)?
                {
                    return Err(Error::Binding);
                }
                for block in tools.retained_result_blocks() {
                    if let Block::ImageArtifact { reference } = block
                        && reference.scope != self.header.scope
                    {
                        return Err(Error::Binding);
                    }
                }
            }
            _ => return Err(Error::State),
        }
        let transcript = self.transcript()?;
        transcript.validate()?;
        for block in transcript
            .messages
            .iter()
            .flat_map(|message| &message.content)
        {
            if let Block::ImageArtifact { reference } = block
                && reference.scope != self.header.scope
            {
                return Err(Error::Binding);
            }
        }
        Ok(())
    }
    pub fn checkpoint(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        let header = encode(b"DTM1", &self.header)?;
        let (tag, body) = match &self.state {
            State::Transcript(transcript) => (0u8, transcript.checkpoint()?),
            State::Model(wait) => (1, wait.checkpoint()?),
            State::Tools(tools) => (2, tools.checkpoint()?),
        };
        let runtime = self.runtime.checkpoint()?;
        let total = 4 + 23 + 1 + 12 + header.len() + body.len() + runtime.len();
        if total > crate::checkpoint::MAX_CHECKPOINT_BYTES {
            return Err(Error::Checkpoint);
        }
        let mut bytes = b"DTT1".to_vec();
        bytes.extend_from_slice(&self.control.checkpoint());
        bytes.push(tag);
        for field in [header, body, runtime] {
            bytes.extend_from_slice(&(field.len() as u32).to_be_bytes());
            bytes.extend(field);
        }
        Ok(bytes)
    }
    pub fn restore(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > crate::checkpoint::MAX_CHECKPOINT_BYTES
            || bytes.len() < 40
            || !bytes.starts_with(b"DTT1")
        {
            return Err(Error::Checkpoint);
        }
        let control = TurnControl::restore(&bytes[4..27]).map_err(|_| Error::Checkpoint)?;
        let tag = bytes[27];
        fn field<'a>(input: &mut &'a [u8]) -> Result<&'a [u8], Error> {
            let prefix = input.get(..4).ok_or(Error::Checkpoint)?;
            let length =
                u32::from_be_bytes(prefix.try_into().map_err(|_| Error::Checkpoint)?) as usize;
            let end = 4usize.checked_add(length).ok_or(Error::Checkpoint)?;
            let field = input.get(4..end).ok_or(Error::Checkpoint)?;
            *input = &input[end..];
            Ok(field)
        }
        let mut input = &bytes[28..];
        let header = decode(b"DTM1", field(&mut input)?)?;
        let body = field(&mut input)?;
        let state = match tag {
            0 => State::Transcript(TranscriptCheckpoint::restore(body)?),
            1 => State::Model(ResponseContinuation::restore(body)?),
            2 => State::Tools(ToolResultsContinuation::restore(body)?),
            _ => return Err(Error::Checkpoint),
        };
        let runtime = TurnRuntimeState::restore(field(&mut input)?)?;
        if !input.is_empty() {
            return Err(Error::Checkpoint);
        }
        let next = Self {
            header,
            control,
            state,
            runtime,
        };
        next.validate()?;
        if next.checkpoint()? != bytes {
            return Err(Error::Checkpoint);
        }
        Ok(next)
    }
}
