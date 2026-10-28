//! Checked Dext turn scheduling. This module commits pure transitions and inert
//! intents only; native request construction, policy and dispatch stay host-owned.
use crate::completion::Attempt;
use crate::transition::{EffectIntent, MAX_EFFECTS, Observation, Proposal, ReceiptBinding};
use crate::{Error, MAX_PAYLOAD, Result, Store};
use dext_core::model::{ModelWait, Phase as ModelPhase};
use dext_core::turn::{Phase, TurnControl};
use dext_transcript::Block;
use dext_transcript::turn::{Effect, TurnContinuation};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

pub const ENGINE: &str = "dext-turn-v1";

/// Tools the strict workspace adapter executes and seals with a workspace receipt.
/// The last four (`HOST_TOOLS`) are run in-process by the host serving the session.
pub const WORKSPACE_TOOLS: [&str; 13] = [
    "bash",
    "run_code",
    "read_output",
    "read_file",
    "write_file",
    "edit_file",
    "start_job",
    "read_job",
    "stop_job",
    "show",
    "settings",
    "spindle_source",
    "spindle_install",
];

/// Workspace tools the host runs in-process (`Store::dispatch_host_workspace`): they
/// read the workspace, change host state (settings, install requests) or, for
/// `spindle_source`, replace one subtree of the workspace.
pub const HOST_TOOLS: [&str; 4] = ["show", "settings", "spindle_source", "spindle_install"];

/// Names Spindle keeps for its own tools (present or planned); an app tool cannot
/// take one, nor any name starting with `spindle_` or `dext`.
pub const RESERVED_TOOL_NAMES: [&str; 5] = ["task", "tasks", "http", "state", "dext-model-v1"];

/// Whether calls of `name` are delegated, answered through the app tool path: an app
/// tool, or Spindle's `task` (which a child session answers).
pub fn delegated_tool(name: &str) -> bool {
    app_tool_name(name) || name == "task"
}

/// Whether `name` can name a tool the embedding app supplies: 1-64 ASCII letters,
/// digits, `_` or `-` (what every provider accepts), not a workspace tool and not
/// reserved.
pub fn app_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        && !WORKSPACE_TOOLS.contains(&name)
        && !RESERVED_TOOL_NAMES.contains(&name)
        && !name.starts_with("spindle_")
        && !name.starts_with("dext")
}

/// Bound to the authenticated admission scope and command ID, not clock/PID or
/// provider call IDs. The digest is over nonsecret identity, not payload content.
pub fn execution_id(scope: &str, command_id: &str) -> Result<String> {
    if !crate::valid_label(scope) || !crate::valid_label(command_id) {
        return Err(Error::Invalid);
    }
    let mut hash = Sha256::new();
    hash.update(b"spindle-dext-execution-v1\0");
    for value in [scope, command_id] {
        hash.update((value.len() as u32).to_be_bytes());
        hash.update(value.as_bytes());
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Retention-approved native inputs. Start's initial checkpoint has no pending
/// effects; request is a Prepared/attempt-zero ModelWait checkpoint.
#[derive(Clone, PartialEq)]
pub enum Action {
    Start {
        initial: Vec<u8>,
        request: Vec<u8>,
    },
    Prepare {
        request: Vec<u8>,
    },
    Followup {
        text: String,
    },
    Finish,
    /// Replaces one pending tool dispatch with the shared CLI guard verdict.
    Guard {
        effect: String,
    },
    /// Literal queued user update, injected with the CLI's shared steering text.
    Steer {
        text: String,
    },
    /// Host-priced usage against a cap; the store recomputes the CLI stop text.
    Budget {
        usd: Option<f64>,
        tokens: Option<u64>,
        usage: dext_transcript::Usage,
    },
    /// Host-produced compacted history (its summary request is a separate model
    /// effect). Authenticated host input, like Finish; not semantically verified.
    Compact {
        transcript: Vec<u8>,
    },
    /// Host refusal of one pending tool call it will never dispatch (unknown tool or
    /// executor-rejected input). Authenticated host input, like Guard.
    Refuse {
        effect: String,
        reason: String,
    },
    /// Explicit operator Continue after the step limit: a fresh step count.
    Extend,
    /// The app's change to the scope's app state, a JSON merge patch (RFC 7386); the
    /// store derives the new state from the prior one. The conversation is unchanged.
    State {
        patch: Vec<u8>,
    },
    TaskReport {
        task: String,
        text: String,
    },
}
impl std::fmt::Debug for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TurnAction(..)")
    }
}
impl Action {
    pub fn encode(&self) -> Result<Vec<u8>> {
        fn field(bytes: &mut Vec<u8>, value: &[u8]) -> Result<()> {
            crate::fits((bytes.len() + 4).saturating_add(value.len()))?;
            bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
            bytes.extend_from_slice(value);
            Ok(())
        }
        let mut bytes = b"DTA1".to_vec();
        match self {
            Self::Start { initial, request } => {
                bytes.push(0);
                field(&mut bytes, initial)?;
                field(&mut bytes, request)?;
            }
            Self::Prepare { request } => {
                bytes.push(1);
                field(&mut bytes, request)?;
            }
            Self::Followup { text } => {
                bytes.push(2);
                field(&mut bytes, text.as_bytes())?;
            }
            Self::Finish => bytes.push(3),
            Self::Guard { effect } => {
                bytes.push(4);
                field(&mut bytes, effect.as_bytes())?;
            }
            Self::Steer { text } => {
                bytes.push(5);
                field(&mut bytes, text.as_bytes())?;
            }
            Self::Budget { usd, tokens, usage } => {
                bytes.push(6);
                let value =
                    serde_json::to_vec(&(usd, tokens, usage)).map_err(|_| Error::Invalid)?;
                field(&mut bytes, &value)?;
            }
            Self::Compact { transcript } => {
                bytes.push(7);
                field(&mut bytes, transcript)?;
            }
            Self::Refuse { effect, reason } => {
                bytes.push(8);
                field(&mut bytes, effect.as_bytes())?;
                field(&mut bytes, reason.as_bytes())?;
            }
            Self::Extend => bytes.push(9),
            Self::State { patch } => {
                bytes.push(10);
                field(&mut bytes, patch)?;
            }
            Self::TaskReport { task, text } => {
                bytes.push(11);
                field(&mut bytes, task.as_bytes())?;
                field(&mut bytes, text.as_bytes())?;
            }
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 5 || bytes.len() > MAX_PAYLOAD || &bytes[..4] != b"DTA1" {
            return Err(Error::Invalid);
        }
        fn field<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
            let header = input.get(..4).ok_or(Error::Invalid)?;
            let len = u32::from_be_bytes(header.try_into().map_err(|_| Error::Invalid)?) as usize;
            let end = 4usize.checked_add(len).ok_or(Error::Invalid)?;
            let bytes = input.get(4..end).ok_or(Error::Invalid)?;
            *input = &input[end..];
            Ok(bytes)
        }
        let mut input = &bytes[5..];
        let action = match bytes[4] {
            0 => Self::Start {
                initial: field(&mut input)?.to_vec(),
                request: field(&mut input)?.to_vec(),
            },
            1 => Self::Prepare {
                request: field(&mut input)?.to_vec(),
            },
            2 => Self::Followup {
                text: std::str::from_utf8(field(&mut input)?)
                    .map_err(|_| Error::Invalid)?
                    .to_owned(),
            },
            3 => Self::Finish,
            4 => Self::Guard {
                effect: std::str::from_utf8(field(&mut input)?)
                    .map_err(|_| Error::Invalid)?
                    .to_owned(),
            },
            5 => Self::Steer {
                text: std::str::from_utf8(field(&mut input)?)
                    .map_err(|_| Error::Invalid)?
                    .to_owned(),
            },
            6 => {
                let (usd, tokens, usage) =
                    serde_json::from_slice(field(&mut input)?).map_err(|_| Error::Invalid)?;
                Self::Budget { usd, tokens, usage }
            }
            7 => Self::Compact {
                transcript: field(&mut input)?.to_vec(),
            },
            8 => {
                let text = |bytes: &[u8]| {
                    std::str::from_utf8(bytes)
                        .map(str::to_owned)
                        .map_err(|_| Error::Invalid)
                };
                Self::Refuse {
                    effect: text(field(&mut input)?)?,
                    reason: text(field(&mut input)?)?,
                }
            }
            9 => Self::Extend,
            10 => Self::State {
                patch: field(&mut input)?.to_vec(),
            },
            11 => Self::TaskReport {
                task: std::str::from_utf8(field(&mut input)?)
                    .map_err(|_| Error::Invalid)?
                    .into(),
                text: std::str::from_utf8(field(&mut input)?)
                    .map_err(|_| Error::Invalid)?
                    .into(),
            },
            _ => return Err(Error::Invalid),
        };
        if !input.is_empty() || action.encode()? != bytes {
            return Err(Error::Invalid);
        }
        Ok(action)
    }
}

pub fn task_report_text(task: &str, answer: &[u8]) -> Result<String> {
    let answer: serde_json::Value = serde_json::from_slice(answer).map_err(|_| Error::Invalid)?;
    let content = answer["content"].as_str().ok_or(Error::Invalid)?;
    let prefix = format!(
        "[task {task}: {}]\n",
        if answer["failed"] == true {
            "failed"
        } else {
            "finished"
        }
    );
    let limit = (64 * 1024usize).saturating_sub(prefix.len());
    let mut end = content.len().min(limit);
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    Ok(format!("{prefix}{}", &content[..end]))
}

fn state(bytes: &[u8]) -> Result<TurnContinuation> {
    TurnContinuation::restore(bytes).map_err(|_| Error::Invalid)
}
fn ready_request(bytes: &[u8]) -> Result<ModelWait<Vec<u8>>> {
    let wait = ModelWait::restore(bytes).map_err(|_| Error::Invalid)?;
    if wait.phase() != ModelPhase::Prepared || wait.attempt() != 0 {
        return Err(Error::Invalid);
    }
    Ok(wait)
}
fn snapshot(db: &Connection, proposal: &Proposal) -> Result<(String, String, Vec<u8>)> {
    Ok(db.query_row(
        "SELECT scope,engine,continuation FROM revisions WHERE revision=?1",
        [proposal.expected_revision as i64],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                crate::bounded_label(row, 1)?,
                crate::packed_blob(db, row, 2, MAX_PAYLOAD)?,
            ))
        },
    )?)
}

/// Only effects newly introduced by the transition become intents. Pending old
/// tools are retained obligations, never re-dispatched because another result arrived.
pub fn new_intents(
    before: Option<&TurnContinuation>,
    after: &TurnContinuation,
) -> Result<Vec<EffectIntent>> {
    let prior = if let Some(before) = before {
        before.pending_effects().map_err(|_| Error::Invalid)?
    } else {
        vec![]
    };
    let prior_ids: std::collections::BTreeSet<String> = prior
        .into_iter()
        .map(|effect| match effect {
            Effect::Model { id, .. } | Effect::Tool { id, .. } => id,
        })
        .collect();
    let mut intents = vec![];
    for effect in after.pending_effects().map_err(|_| Error::Invalid)? {
        let intent = match effect {
            Effect::Model { id, waiting } => EffectIntent {
                id,
                adapter: "dext-model-v1".into(),
                input: waiting.checkpoint(),
            },
            Effect::Tool { id, call } => EffectIntent {
                id,
                adapter: call.name,
                input: serde_json::to_vec(&call.input).map_err(|_| Error::Invalid)?,
            },
        };
        if !prior_ids.contains(&intent.id) {
            intents.push(intent);
        }
    }
    if intents.len() > MAX_EFFECTS
        || intents
            .iter()
            .any(|intent| !crate::valid_label(&intent.id) || !crate::valid_label(&intent.adapter))
    {
        return Err(Error::Invalid);
    }
    for intent in &intents {
        crate::fits(intent.input.len())?;
    }
    Ok(intents)
}
fn matches_proposal(
    proposal: &Proposal,
    before: Option<&TurnContinuation>,
    after: &TurnContinuation,
) -> Result<()> {
    if after.scope() != proposal.scope
        || proposal.engine != ENGINE
        || proposal.workspace.is_some()
        || after.checkpoint().map_err(|_| Error::Invalid)? != proposal.continuation
        || new_intents(before, after)? != proposal.effects
    {
        return Err(Error::Conflict);
    }
    Ok(())
}

fn next_turn_prefix_matches(prior: &TurnContinuation, initial: &TurnContinuation) -> Result<bool> {
    let prior = prior.transcript().map_err(|_| Error::Invalid)?;
    let initial = initial.transcript().map_err(|_| Error::Invalid)?;
    if initial.route_binding != prior.route_binding
        || initial.messages.len() != prior.messages.len() + 1
    {
        return Ok(false);
    }
    let prompt = initial.messages.last().ok_or(Error::Invalid)?;
    if prompt.role != "user"
        || prompt.content.is_empty()
        || !prompt
            .content
            .iter()
            .all(|block| matches!(block, Block::Text { text } if !text.trim().is_empty()))
    {
        return Ok(false);
    }
    Ok(
        serde_json::to_vec(&prior.messages).map_err(|_| Error::Invalid)?
            == serde_json::to_vec(&initial.messages[..prior.messages.len()])
                .map_err(|_| Error::Invalid)?,
    )
}

impl Store {
    /// Checks the exact admitted action and derives state/intents in the commit
    /// transaction. Host policy verdicts such as Finish remain authenticated inputs.
    pub fn commit_turn_action(&mut self, proposal: &Proposal) -> Result<u64> {
        self.commit_turn_action_profile(proposal, false)
    }

    #[cfg(any(feature = "driver", feature = "dext-strict-workspace"))]
    /// Attaches a coherently supplied immutable workspace to a turn's start: the first
    /// turn's imported files, or a later turn's files with attachments added. Host
    /// input like the prompt; this does not import an ambient checkout or authorize
    /// live execution.
    pub fn commit_turn_workspace_start(&mut self, proposal: &Proposal) -> Result<u64> {
        if proposal.workspace.is_none() {
            return Err(Error::Invalid);
        }
        self.commit_turn_action_profile(proposal, true)
    }

    fn commit_turn_action_profile(
        &mut self,
        proposal: &Proposal,
        workspace_start: bool,
    ) -> Result<u64> {
        if self.paused {
            return Err(Error::Paused);
        }
        let Observation::Admission(sequence) = proposal.observation else {
            return Err(Error::Invalid);
        };
        self.commit_bound_proposal(proposal, None, |transaction| {
            let (id, kind, payload): (String, String, Vec<u8>) = transaction
                .query_row(
                    "SELECT command_id,kind,payload FROM admissions WHERE sequence=?1 AND scope=?2",
                    params![sequence as i64, proposal.scope],
                    |row| {
                        Ok((
                            crate::bounded_label(row, 0)?,
                            crate::bounded_label(row, 1)?,
                            crate::packed_blob(transaction, row, 2, MAX_PAYLOAD)?,
                        ))
                    },
                )
                .optional()?
                .ok_or(Error::Invalid)?;
            if kind != ENGINE {
                return Err(Error::Invalid);
            }
            let action = Action::decode(&payload)?;
            let (scope, engine, baseline) = snapshot(transaction, proposal)?;
            if let Action::Start { initial, request } = action {
                crate::rewind::validate_rewound_start(transaction, proposal, sequence)?;
                let initial = state(&initial)?;
                if initial.scope() != proposal.scope
                    || initial.execution() != execution_id(&proposal.scope, &id)?
                    || initial.control() != TurnControl::default()
                {
                    return Err(Error::Conflict);
                }
                if proposal.expected_revision != 0 {
                    let prior = state(&baseline)?;
                    if scope != proposal.scope
                        || engine != ENGINE
                        || !matches!(
                            prior.control().phase(),
                            Phase::Stopped(dext_core::turn::Stop::Complete)
                        )
                        || !next_turn_prefix_matches(&prior, &initial)?
                    {
                        return Err(Error::Conflict);
                    }
                }
                let request = ready_request(&request)?;
                let route = initial
                    .transcript()
                    .map_err(|_| Error::Invalid)?
                    .route_binding;
                let next = initial
                    .prepare(request.request().clone(), &route)
                    .map_err(|_| Error::Invalid)?;
                if workspace_start {
                    let root = proposal.workspace.as_ref().ok_or(Error::Invalid)?;
                    crate::workspace::validate_root(transaction, &proposal.scope, root)?;
                    let mut checked = proposal.clone();
                    checked.workspace = None;
                    return matches_proposal(&checked, Some(&initial), &next);
                }
                return matches_proposal(proposal, Some(&initial), &next);
            }
            if scope != proposal.scope || engine != ENGINE {
                return Err(Error::Conflict);
            }
            let before = state(&baseline)?;
            if let Action::State { patch } = &action {
                let prior = crate::transition::app_state_at(
                    transaction,
                    &proposal.scope,
                    proposal.expected_revision,
                )?;
                if proposal.state != Some(crate::transition::apply_state_patch(&prior, patch)?) {
                    return Err(Error::Conflict);
                }
                return matches_proposal(proposal, Some(&before), &before);
            }
            if proposal.state.is_some() {
                return Err(Error::Invalid);
            }
            if let Action::Guard { effect } | Action::Refuse { effect, .. } = &action {
                let reserved: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM attempts WHERE effect=?1)",
                    [effect],
                    |row| row.get(0),
                )?;
                if reserved {
                    return Err(Error::Conflict);
                }
            }
            let after = match action {
                Action::Prepare { request } => {
                    let request = ready_request(&request)?;
                    let route = before
                        .transcript()
                        .map_err(|_| Error::Invalid)?
                        .route_binding;
                    before.prepare(request.request().clone(), &route)
                }
                Action::Followup { text } => before.followup(text),
                Action::Steer { text } => before.steer(text),
                Action::TaskReport { task, text } => {
                    let answer: Vec<u8> = transaction.query_row("SELECT answer FROM task_runs WHERE task=?1 AND queued=1 AND state='finished' AND delivered IS NULL", [&task], |row| crate::bounded_blob(row, 0, MAX_PAYLOAD))?;
                    let expected = task_report_text(&task, &answer)?;
                    if text != expected { return Err(Error::Conflict); }
                    before.task_report(text)
                }
                Action::Budget { usd, tokens, usage } => {
                    before.stop_for_budget(usd, tokens, &usage)
                }
                Action::Compact { transcript } => {
                    let replacement =
                        dext_transcript::checkpoint::TranscriptCheckpoint::restore(&transcript)
                            .map_err(|_| Error::Invalid)?;
                    before.compact(replacement)
                }
                Action::Finish => before.finish(),
                Action::Guard { effect } => before
                    .guard_tool(&effect)
                    .and_then(|verdict| verdict.ok_or(dext_transcript::continuation::Error::State)),
                Action::Refuse { effect, reason } => before.refuse_tool(&effect, &reason),
                Action::Extend => before.extend_steps(),
                Action::Start { .. } | Action::State { .. } => unreachable!(),
            }
            .map_err(|_| Error::Invalid)?;
            matches_proposal(proposal, Some(&before), &after)
        })
    }

    #[cfg(feature = "dext-image-continuation")]
    /// Derives the DTT1 tool transition and artifact references from the retained
    /// image receipt. Current worker approval is host policy, not a disclosure grant.
    pub fn commit_turn_image_receipt(
        &mut self,
        proposal: &Proposal,
        binding: &ReceiptBinding<'_>,
        approved_worker: [u8; 32],
    ) -> Result<u64> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !binding.objects.is_empty() {
            return Err(Error::Invalid);
        }
        let receipt = crate::image::ImageReceipt::decode(binding.result)?;
        let objects = receipt.objects();
        let checked = ReceiptBinding {
            objects: &objects,
            ..*binding
        };
        self.commit_checked_receipt_proposal(proposal, &checked, |transaction| {
            crate::image::validate_verified_images(
                transaction,
                &proposal.scope,
                &receipt,
                &approved_worker,
            )?;
            let (scope, engine, baseline) = snapshot(transaction, proposal)?;
            if scope != proposal.scope || engine != ENGINE {
                return Err(Error::Conflict);
            }
            let before = state(&baseline)?;
            let after = image_receipt_transition(&before, binding.intent, &receipt)?;
            matches_proposal(proposal, Some(&before), &after)
        })
    }

    #[cfg(any(feature = "driver", feature = "dext-strict-workspace"))]
    /// Atomically selects the sealed strict workspace and its exact tool-result
    /// continuation. Historical retries revalidate helper, root and receipt bindings.
    pub fn commit_turn_workspace_receipt(
        &mut self,
        proposal: &Proposal,
        binding: &ReceiptBinding<'_>,
        approved_helper: [u8; 32],
    ) -> Result<u64> {
        if !binding.objects.is_empty() || proposal.workspace.is_none() {
            return Err(Error::Invalid);
        }
        self.commit_turn_receipt_profile(proposal, binding, Some(approved_helper))
    }

    /// The receipt must match an actual pending obligation from the retained DTT1
    /// baseline. Generic tool application accepts only one ToolResult; immutable
    /// images use verified image adoption, never hidden references in this channel.
    pub fn commit_turn_receipt(
        &mut self,
        proposal: &Proposal,
        binding: &ReceiptBinding<'_>,
    ) -> Result<u64> {
        if !binding.objects.is_empty() {
            return Err(Error::Invalid);
        }
        self.commit_turn_receipt_profile(proposal, binding, None)
    }

    fn commit_turn_receipt_profile(
        &mut self,
        proposal: &Proposal,
        binding: &ReceiptBinding<'_>,
        approved_helper: Option<[u8; 32]>,
    ) -> Result<u64> {
        self.commit_checked_receipt_proposal(proposal, binding, |transaction| {
            if let Some(helper) = approved_helper {
                let record: Option<(String, String, String)> = transaction.query_row(
                    "SELECT base,root,helper FROM strict_receipts WHERE attempt=?1", [&binding.attempt.id],
                    |row| Ok((crate::bounded_label(row, 0)?, crate::bounded_label(row, 1)?, crate::bounded_label(row, 2)?)),
                ).optional()?;
                let (base, root, saved_helper) = record.ok_or(Error::Invalid)?;
                let approved: String = helper.iter().map(|byte| format!("{byte:02x}")).collect();
                let adapter = binding.intent.adapter.as_str();
                let labelled = (WORKSPACE_TOOLS.contains(&adapter) && binding.attempt.implementation == runner_label(&helper))
                    || (delegated_tool(adapter) && binding.attempt.implementation == crate::broker::app_label(&approved));
                if saved_helper != approved || proposal.workspace.as_deref() != Some(&root) || !labelled {
                    return Err(Error::Conflict);
                }
                crate::workspace::validate_root(transaction, &proposal.scope, &base)?;
                crate::workspace::validate_root(transaction, &proposal.scope, &root)?;
            }
            let (scope, engine, baseline) = snapshot(transaction, proposal)?;
            if scope != proposal.scope || engine != ENGINE { return Err(Error::Conflict); }
            let before = state(&baseline)?;
            let pending = before.pending_effects().map_err(|_| Error::Invalid)?.into_iter().find(|effect| match effect {
                Effect::Model { id, .. } | Effect::Tool { id, .. } => id == &binding.attempt.effect,
            }).ok_or(Error::Conflict)?;
            let after = match pending {
                Effect::Model { id, waiting } => {
                    if binding.intent.id != id || binding.intent.adapter != "dext-model-v1"
                        || binding.intent.input != waiting.checkpoint() { return Err(Error::Conflict); }
                    let recorded = ModelWait::restore(binding.result).map_err(|_| Error::Invalid)?;
                    let route = before.transcript().map_err(|_| Error::Invalid)?.route_binding;
                    before.apply_model(&recorded, &route).map_err(|_| Error::Invalid)?
                }
                Effect::Tool { id, call } => {
                    if binding.intent.id != id || binding.intent.adapter != call.name
                        || binding.intent.input != serde_json::to_vec(&call.input).map_err(|_| Error::Invalid)? { return Err(Error::Conflict); }
                    let block: Block = serde_json::from_slice(binding.result).map_err(|_| Error::Invalid)?;
                    if !matches!(&block, Block::ToolResult { tool_use_id, .. } if tool_use_id == &call.id)
                        || serde_json::to_vec(&block).map_err(|_| Error::Invalid)? != binding.result { return Err(Error::Invalid); }
                    before.apply_tool(&id, vec![block]).map_err(|_| Error::Invalid)?
                }
            };
            let mut checked = proposal.clone();
            if approved_helper.is_some() { checked.workspace = None; }
            matches_proposal(&checked, Some(&before), &after)
        })
    }
}

impl Store {
    /// Records a settled child task's final answer as the receipt for the parent's
    /// reserved tool attempt. The child must be a settled owned task directly under
    /// the parent scope with a Complete turn. Adoption still uses commit_turn_receipt;
    /// which tool call delegates to which child is authenticated host policy.
    pub fn ingest_child_result(&mut self, attempt: &str, child: &str) -> Result<Vec<u8>> {
        if self.paused {
            return Err(Error::Paused);
        }
        if !crate::valid_label(attempt) || !crate::valid_label(child) {
            return Err(Error::Invalid);
        }
        let epoch = self.epoch;
        let result = (|| -> Result<Vec<u8>> {
            let tx = self
                .db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let current: u64 = tx.query_row("SELECT epoch FROM realm WHERE id=1", [], |row| {
                crate::nonnegative_id(row, 0)
            })?;
            if current != epoch {
                return Err(Error::Stale);
            }
            let (effect, parent): (String, String) = tx
                .query_row(
                    "SELECT i.id,r.scope FROM attempts a JOIN intents i ON a.effect=i.id JOIN revisions r ON r.revision=i.revision WHERE a.id=?1",
                    [attempt],
                    |row| Ok((crate::bounded_label(row, 0)?, crate::bounded_label(row, 1)?)),
                )
                .optional()?
                .ok_or(Error::Invalid)?;
            let node = crate::ownership::scope_node(&tx, child)?.ok_or(Error::Invalid)?;
            if node.parent.as_deref() != Some(parent.as_str())
                || node.kind != crate::ownership::ScopeKind::Task
            {
                return Err(Error::Conflict);
            }
            if node.state != crate::ownership::ScopeState::Settled {
                return Err(Error::Conflict);
            }
            let head = |scope: &str| -> Result<TurnContinuation> {
                let revision = crate::transition::scope_revision(&tx, scope)?;
                let (engine, bytes): (String, Vec<u8>) = tx.query_row(
                    "SELECT engine,continuation FROM revisions WHERE revision=?1 AND scope=?2",
                    params![revision as i64, scope],
                    |row| {
                        Ok((
                            crate::bounded_label(row, 0)?,
                            crate::packed_blob(&tx, row, 1, MAX_PAYLOAD)?,
                        ))
                    },
                )?;
                if engine != ENGINE {
                    return Err(Error::Incompatible);
                }
                state(&bytes)
            };
            let finished = head(child)?;
            if finished.control().phase() != Phase::Stopped(dext_core::turn::Stop::Complete) {
                return Err(Error::Conflict);
            }
            let transcript = finished.transcript().map_err(|_| Error::Invalid)?;
            let answer = transcript
                .messages
                .iter()
                .rev()
                .find(|message| message.role == "assistant")
                .map(|message| {
                    message
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            Block::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|text| !text.trim().is_empty())
                .ok_or(Error::Conflict)?;
            let call = head(&parent)?
                .pending_effects()
                .map_err(|_| Error::Invalid)?
                .into_iter()
                .find_map(|pending| match pending {
                    Effect::Tool { id, call } if id == effect => Some(call),
                    _ => None,
                })
                .ok_or(Error::Conflict)?;
            let block = Block::ToolResult {
                tool_use_id: call.id,
                content: answer,
                is_error: Some(false),
                metadata: Default::default(),
            };
            let bytes = serde_json::to_vec(&block).map_err(|_| Error::Invalid)?;
            crate::fits(bytes.len())?;
            crate::completion::record_receipt(&tx, attempt, &bytes, None)?;
            tx.commit()?;
            Ok(bytes)
        })();
        if matches!(
            result,
            Err(Error::Sql(_) | Error::Io(_) | Error::Locked | Error::Incompatible)
        ) {
            self.paused = true;
        }
        result
    }
}

#[cfg(feature = "dext-image-continuation")]
/// Pure DTT1 image-result proposal. Artifact integrity, provenance, retention and
/// disclosure authorization are not established by this speculative transition.
pub fn image_receipt_transition(
    before: &TurnContinuation,
    intent: &EffectIntent,
    receipt: &crate::image::ImageReceipt,
) -> Result<TurnContinuation> {
    let effect = before
        .pending_effects()
        .map_err(|_| Error::Invalid)?
        .into_iter()
        .find(|effect| matches!(effect, Effect::Tool { id, .. } if id == &intent.id))
        .ok_or(Error::Conflict)?;
    let Effect::Tool { id, call } = effect else {
        unreachable!()
    };
    if intent.adapter != call.name
        || intent.input != serde_json::to_vec(&call.input).map_err(|_| Error::Invalid)?
    {
        return Err(Error::Conflict);
    }
    let blocks = receipt.result_blocks(before.scope(), &call.id)?;
    before.apply_tool(&id, blocks).map_err(|_| Error::Invalid)
}

/// Receipt identity is a store reservation. The model's attempt counter remains
/// checked by ModelWait; its correspondence to physical dispatch is broker-owned.
pub fn receipt_binding<'a>(
    attempt: &'a Attempt,
    revision: u64,
    intent: &'a EffectIntent,
    result: &'a [u8],
) -> ReceiptBinding<'a> {
    ReceiptBinding {
        attempt,
        intent_revision: revision,
        intent,
        result,
        objects: &[],
    }
}

/// Attempt label for a runner identity, independent of a native executor.
pub(crate) fn runner_label(helper: &[u8; 32]) -> String {
    use sha2::{Digest, Sha256};
    let host: [u8; 32] = Sha256::digest(b"dext-host-runner-v1 (no isolation)").into();
    let hex: String = helper.iter().map(|byte| format!("{byte:02x}")).collect();
    if *helper == host {
        format!("dext-host-bash-v1-{hex}")
    } else {
        format!("dext-strict-bash-v1-{hex}")
    }
}
