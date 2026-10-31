//! The turn driver: what a turn needs from its host next, decided from the committed
//! head, and what it does with the host's answer. Store reads/commits, repeatable host
//! facts and semantic events only; sends, execution, clocks and waits belong to the host.
#[path = "agent/app_tools.rs"]
pub mod app_tools;
#[path = "task_tool.rs"]
pub mod task_tool;
#[path = "driver_types.rs"]
mod types;
pub use types::*;
#[path = "driver_api.rs"]
mod api;
/// The allowance of a compaction's summary request, and the longest summary kept (about
/// six bytes per token of it): each compaction leaves room for one.
pub(crate) const SUMMARY_TOKENS: u64 = 8192;
pub(crate) const SUMMARY_BYTES: usize = SUMMARY_TOKENS as usize * 6;
// Model-written compaction: for the hosts that ask a model for its summary.
#[cfg(any(feature = "agent", feature = "embedded"))]
#[path = "driver_summary.rs"]
mod summary;
use crate::artifact::Stage;
use crate::rewind::RewindTargetKind;
use crate::transition::{EffectIntent, Proposal};
use crate::turn::{Action, ENGINE, execution_id, new_intents};
use crate::workspace::{Entry, Tree};
use crate::{Command, Store};
use api::Step;
pub use api::*;
use app_tools::AppAnswer;
use dext_core::model::PreparedRequest;
use dext_core::recovery::Contract;
use dext_core::turn::{Phase, Stop};
use dext_transcript::checkpoint::TranscriptCheckpoint;
use dext_transcript::{Block, Message};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(any(feature = "agent", feature = "embedded"))]
pub(crate) use summary::*;
pub type Result<T> = std::result::Result<T, String>;
const SCOPE: &str = "operator";
const ROUTE: &str = "spindle-cli-v1";
const MODEL_IMPLEMENTATION: &str = "dext-provider-v1";
const MAX_PROVIDER_RETRIES: u32 = 6;
const MAX_DROPPED_RETRIES: u32 = 3;
pub const DROPPED: u16 = 0;
const MAX_RETRY_WAIT: Duration = Duration::from_secs(300);
const COMPACT_KEEP_SHARE: usize = 4;
const COMPACTED: &str = "[Earlier conversation compacted by spindle to stay within session limits]";
const COMPACTED_TRAILER: &str =
    "The workspace files are current; inspect them rather than relying on this summary.";
pub(crate) const CURRENT_REQUEST: &str = "(the current request follows verbatim)";
use crate::Error;
use crate::broker::{DispatchOutcome, DispatchRequest};
use crate::completion::Attempt;
use crate::transition::Observation;
use crate::turn::receipt_binding;
use dext_core::model::ModelWait;

use dext_transcript::checkpoint::NativeResponse;
use dext_transcript::turn::{Effect, TurnContinuation};
use std::time::Duration;

/// The pending model request the turn sends next, checked and announced.
pub struct ModelNeed {
    /// The request's effect id.
    pub id: String,
    pub intent_revision: u64,
    pub request: DispatchRequest,
    /// A fresh send: the route to resolve credentials for, before anything is
    /// reserved (so a missing login never strands a reserved request). None when an
    /// attempt is reserved already: its outcome is settled from the store, never sent.
    pub route: Option<Route>,
}

/// What a refused or dropped request comes to when it is sent again: after `wait`,
/// compacting the history first when it did not fit (`compact`). A stop during the
/// wait ends the turn with `stopped`.
pub struct Backoff {
    pub wait: Duration,
    pub compact: bool,
    pub stopped: String,
}

/// What the host's send of a `ModelNeed` came to.
pub struct ModelSent {
    pub outcome: std::result::Result<DispatchOutcome, ModelFailure>,
    /// Live text was shown while the reply streamed (a preview of it).
    pub streamed: bool,
    /// The send was cut short to read queued messages first: the point the request
    /// is prepared again from.
    pub cut: Option<u64>,
    /// A provider refusal's detail, as the transport kept it.
    pub rejection: Option<Rejection>,
}

impl Step<'_> {
    /// The model request the head waits on: for a fresh send, its endpoint is still
    /// the route's and the budget allows it (both refuse before reservation, so
    /// Continue sends it once they are fixed), and `ModelStep` announces it.
    pub fn model_need(&mut self, revision: u64, state: &TurnContinuation) -> Result<ModelNeed> {
        let Some(Effect::Model { id, waiting }) = state
            .pending_effects()
            .map_err(turn_error)?
            .into_iter()
            .next()
        else {
            return Err("no pending model request".into());
        };
        let (intent_revision, intent) = self.intent(&id)?;
        let existing = self.store.attempt_for(&id).map_err(store_error)?;
        let identity = waiting.request().identity().clone();
        let route = if existing.is_none() {
            let route = self.session_route(&identity.provider, Some(&identity.model))?;
            if route.endpoint != identity.endpoint {
                return Err(format!(
                    "the `{}` endpoint changed to {} since this request was prepared for {}; restore it or start a new session",
                    route.provider, route.endpoint, identity.endpoint
                ));
            }
            self.check_budget(waiting.request())?;
            self.emit(Event::ModelStep {
                provider: &route.provider,
                model: &identity.model,
                step: state.control().iterations(),
            });
            Some(route)
        } else {
            None
        };
        let attempt = existing.unwrap_or(Attempt {
            id: format!("a:{id}"),
            effect: id.clone(),
            owner_epoch: self.store.epoch(),
            implementation: MODEL_IMPLEMENTATION.into(),
            grant_reference: "dext-login".into(),
        });
        let request = DispatchRequest {
            scope: SCOPE.into(),
            owner_epoch: self.store.epoch(),
            expected_revision: revision,
            intent_revision,
            intent,
            attempt,
        };
        Ok(ModelNeed {
            id,
            intent_revision,
            request,
            route,
        })
    }

    /// What `need`'s send came to, in the turn: a reply is adopted (the step commits
    /// it); a refusal is recorded, and it or a drop is returned for the caller's
    /// recovery (`recover_rejection`); a cut prepares the request again from the point
    /// before it, where the queued messages are read first.
    pub fn model_sent(&mut self, need: ModelNeed, sent: ModelSent) -> Result<ModelNext> {
        let ModelNeed {
            id,
            intent_revision,
            request,
            ..
        } = need;
        if sent.streamed && sent.outcome.is_err() {
            self.emit(Event::ModelText { text: "" });
        }
        if let Some(point) = sent.cut {
            self.emit(Event::ReplyCut);
            return self.retry_in_place(Some(point)).map(ModelNext::Need);
        }
        let reused = matches!(sent.outcome, Ok(DispatchOutcome::Reused(_)));
        let bytes = match sent.outcome {
            Ok(DispatchOutcome::Recorded(bytes)) => bytes,
            Ok(DispatchOutcome::Reused(bytes)) => {
                self.emit(Event::ResponseReused);
                bytes
            }
            Ok(DispatchOutcome::Reconcile) => {
                // A refusal recorded before a stop or crash is still a known outcome.
                if let Some((status, body)) = self
                    .store
                    .rejection(&request.attempt.id)
                    .map_err(store_error)?
                {
                    return Ok(ModelNext::Retry(Rejection {
                        status,
                        retry_after: self
                            .store
                            .retry_delay(&request.attempt.id)
                            .map_err(store_error)?,
                        body,
                    }));
                }
                // A session that chose to resend these sends it again, as a dropped
                // connection is (bounded, and counted as possibly billed).
                if self.settings()?.resend_unknown {
                    return Ok(ModelNext::Retry(Rejection {
                        status: DROPPED,
                        retry_after: None,
                        body: "the model request in flight when spindle stopped has an \
                               unknown outcome"
                            .into(),
                    }));
                }
                return Err(
                    "the model request in flight when spindle stopped has an unknown \
                    outcome (it may have been billed). Run `spindle retry` to re-run it from \
                    the point before the request."
                        .into(),
                );
            }
            Err(ModelFailure::Http(status)) => {
                let rejection = sent.rejection.unwrap_or(Rejection {
                    status,
                    ..Default::default()
                });
                // A refusal is a known outcome: no reply to adopt.
                self.store
                    .record_rejection_with_delay(
                        &request.attempt.id,
                        status,
                        &rejection.body,
                        rejection.retry_after,
                    )
                    .map_err(store_error)?;
                return Ok(ModelNext::Retry(rejection));
            }
            // A drop is unknown, not a refusal. Backoff requires explicit resend consent.
            Err(ModelFailure::Dropped(error)) => {
                return Ok(ModelNext::Retry(Rejection {
                    status: DROPPED,
                    retry_after: None,
                    body: error,
                }));
            }
            Err(error) => {
                return Err(format!(
                    "{}. The request is recorded as sent with an unknown outcome; \
                     run `spindle retry` to re-run it.",
                    error.text()
                ));
            }
        };
        let (revision, state) = self.head()?;
        let recorded = ModelWait::restore(&bytes).map_err(|_| "invalid recorded response")?;
        let response = recorded
            .response()
            .and_then(|bytes| NativeResponse::restore(bytes).ok())
            .ok_or("invalid recorded response")?;
        let after = state.apply_model(&recorded, ROUTE).map_err(turn_error)?;
        let proposal = self.proposal(
            format!("adopt-r{revision}"),
            Observation::Receipt(request.attempt.id.clone()),
            revision,
            &state,
            &after,
        )?;
        self.store
            .commit_turn_receipt(
                &proposal,
                &receipt_binding(&request.attempt, intent_revision, &request.intent, &bytes),
            )
            .map_err(store_error)?;
        let text = text_of(&response.blocks);
        if sent.streamed {
            self.emit(Event::ModelText { text: "" });
        } else if !text.trim().is_empty() {
            self.emit(Event::ModelText { text: &text });
        }
        let usage = response.usage;
        self.emit(Event::Usage {
            input: usage.input + usage.cache_read + usage.cache_create,
            output: usage.output,
        });
        // A reply adopted from the store was counted when it was recorded.
        if !reused {
            self.note_cost(Counted::Reply(&id));
        }
        Ok(ModelNext::Adopted)
    }

    /// Whether a refused request (or one whose connection dropped) is sent again, and
    /// after how long, announced with `ProviderRetry`; otherwise the turn stops with what
    /// the provider said. A refusal is sent again when it is transient, when the history
    /// did not fit the model's window (compacted first) or when it states a lower output
    /// limit (learned, and asked for). Drops require explicit resend consent and have
    /// their own bounded backoff. Never the session's first request (nothing to send
    /// it again from) nor past the retry caps.
    pub fn backoff(&mut self, rejection: Rejection, attempts: &mut u32) -> Result<Backoff> {
        let status = rejection.status;
        if status == DROPPED {
            return self.backoff_drop(rejection, attempts);
        }
        let overflow = context_overflow(status, &rejection.body);
        let transient = matches!(status, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 529);
        // A refusal that states the model's output limit: remember it and ask for less.
        let lowered = (|| -> Result<bool> {
            let config = self.settings()?;
            let route = self.session_route(&config.provider, Some(&config.model))?;
            let sent = route.sent_tokens;
            let Some(limit) = stated_output_limit(status, &rejection.body, sent) else {
                return Ok(false);
            };
            self.remember_limit(&route, limit)?;
            let after = self.session_route(&config.provider, Some(&config.model))?;
            Ok(after.sent_tokens < sent)
        })()?;
        let said = clip(&rejection.body, 600);
        let said = if said.is_empty() {
            String::new()
        } else {
            format!(": {said}")
        };
        let tried = if *attempts > 0 {
            format!(" after {} automatic retries", *attempts)
        } else {
            String::new()
        };
        if !(overflow || transient || lowered) || *attempts >= MAX_PROVIDER_RETRIES {
            return Err(format!(
                "{}{said}. It was refused before any reply{tried}; `spindle retry` sends the request again.",
                http_error(status)
            ));
        }
        // The session's first request has no earlier point to send it again from.
        if self
            .store
            .rewind_targets(SCOPE)
            .map_err(store_error)?
            .is_empty()
        {
            return Err(format!(
                "{}{said}. The session's first request was refused before any reply{tried}; \
                 `spindle retry` sends it again.",
                http_error(status)
            ));
        }
        *attempts += 1;
        let wait = if overflow || lowered {
            Duration::ZERO
        } else {
            rejection
                .retry_after
                .unwrap_or(Duration::from_secs(2u64 << (*attempts).min(5)))
                .min(MAX_RETRY_WAIT)
        };
        self.emit(Event::ProviderRetry {
            status,
            seconds: wait.as_secs(),
            attempt: *attempts,
            compacting: overflow,
        });
        Ok(Backoff {
            wait,
            compact: overflow,
            stopped: "stopped while waiting to retry the provider request".into(),
        })
    }

    /// Unknown outcomes may be billed twice; only explicit session consent permits
    /// bounded automatic retries. Manual retry remains available without that consent.
    fn backoff_drop(&mut self, dropped: Rejection, attempts: &mut u32) -> Result<Backoff> {
        let stop = |tried: u32| {
            format!(
                "{}{}. The request is recorded as sent with an unknown outcome (it may have \
                 been billed); `spindle retry` sends it again.",
                dropped.body,
                if tried > 0 {
                    format!(" after {tried} automatic retries")
                } else {
                    String::new()
                }
            )
        };
        if self.driver.until_unknown
            || !self.settings()?.resend_unknown
            || *attempts >= MAX_DROPPED_RETRIES
            || self
                .store
                .rewind_targets(SCOPE)
                .map_err(store_error)?
                .is_empty()
        {
            return Err(stop(*attempts));
        }
        *attempts += 1;
        let wait = Duration::from_secs(1 << (*attempts - 1));
        self.emit(Event::ProviderRetry {
            status: DROPPED,
            seconds: wait.as_secs(),
            attempt: *attempts,
            compacting: false,
        });
        Ok(Backoff {
            wait,
            compact: false,
            stopped: stop(*attempts - 1),
        })
    }

    /// The wait is over: the request is prepared again from the point before it
    /// (compacting first when the history did not fit).
    pub fn after_backoff(&mut self, backoff: Backoff) -> Result<Need> {
        self.driver.compact_next |= backoff.compact;
        self.retry_in_place(None)
    }
}

/// The next tool step either committed a guard/refusal, needs a native runner, or
/// needs the app. Runner selection and execution belong to the host.
pub enum ToolNeed {
    Committed,
    Run(Box<RunCall>),
    Ask(Box<AppNeed>),
    Task(Box<AppNeed>),
    Tasks(Vec<AppNeed>),
}

pub struct RunCall {
    pub id: String,
    pub call: dext_core::response::ToolCall<Value>,
    pub intent_revision: u64,
    pub intent: EffectIntent,
    pub existing: Option<Attempt>,
    pub config: Settings,
}

pub struct AppNeed {
    pub id: String,
    pub call_id: String,
    pub call: dext_core::response::ToolCall<Value>,
    pub request: DispatchRequest,
    pub app: AppDispatch,
}

pub struct ToolRan {
    pub request: DispatchRequest,
    pub helper: [u8; 32],
    pub outcome: crate::Result<DispatchOutcome>,
}

impl Step<'_> {
    pub fn tool_need(
        &mut self,
        revision: u64,
        state: &TurnContinuation,
    ) -> Result<Option<ToolNeed>> {
        let config = self.settings()?;
        let effects = state.pending_effects().map_err(turn_error)?;
        let mut batch = Vec::new();
        for effect in &effects {
            let Effect::Tool { id, call } = effect else {
                break;
            };
            if call.name != "task"
                || self.env.refusal(&config, &call.name, &call.input).is_some()
                || state.guard_tool(id).map_err(turn_error)?.is_some()
            {
                break;
            }
            let existing = self.store.attempt_for(id).map_err(store_error)?;
            let Some(app) = self
                .env
                .app_dispatch(&config, &call.name, existing.as_ref())
            else {
                break;
            };
            let (intent_revision, intent) = self.intent(id)?;
            let attempt = existing.unwrap_or(Attempt {
                id: format!("a:{id}"),
                effect: id.clone(),
                owner_epoch: self.store.epoch(),
                implementation: crate::broker::app_label(&hex(&app.helper)),
                grant_reference: "spindle-app-tool".into(),
            });
            batch.push(AppNeed {
                id: id.clone(),
                call_id: self.store.call_id(id).map_err(store_error)?,
                call: call.clone(),
                app,
                request: DispatchRequest {
                    scope: SCOPE.into(),
                    owner_epoch: self.store.epoch(),
                    expected_revision: revision,
                    intent_revision,
                    intent,
                    attempt,
                },
            });
        }
        if batch.len() > 1 && !self.driver.until_unknown {
            for need in &batch {
                if self
                    .store
                    .attempt_for(&need.id)
                    .map_err(store_error)?
                    .is_none()
                {
                    self.emit(Event::ToolCall {
                        id: &need.call.id,
                        name: &need.call.name,
                        input: &need.call.input,
                    });
                }
            }
            return Ok(Some(ToolNeed::Tasks(batch)));
        }
        for effect in effects {
            let Effect::Tool { id, call } = effect else {
                continue;
            };
            let (intent_revision, intent) = self.intent(&id)?;
            let existing = self.store.attempt_for(&id).map_err(store_error)?;
            let config = self.settings()?;
            if existing.is_none() {
                if let Some(after) = state.guard_tool(&id).map_err(turn_error)? {
                    self.commit_action(
                        format!("guard-r{revision}"),
                        &Action::Guard { effect: id },
                        state,
                        &after,
                    )?;
                    self.emit(Event::ToolCall {
                        id: &call.id,
                        name: &call.name,
                        input: &call.input,
                    });
                    self.emit(Event::ToolSkipped);
                    return Ok(Some(ToolNeed::Committed));
                }
                // Calls of one batch run in order, each over the workspace the call before
                // it left (the broker runs over the scope head), so none is refused as stale.
                let refusal = self.env.refusal(&config, &call.name, &call.input);
                if let Some(reason) = refusal {
                    let after = state.refuse_tool(&id, &reason).map_err(turn_error)?;
                    self.commit_action(
                        format!("refuse-r{revision}"),
                        &Action::Refuse {
                            effect: id,
                            reason: reason.clone(),
                        },
                        state,
                        &after,
                    )?;
                    self.emit(Event::ToolCall {
                        id: &call.id,
                        name: &call.name,
                        input: &call.input,
                    });
                    self.emit(Event::ToolRefused { reason: &reason });
                    return Ok(Some(ToolNeed::Committed));
                }
                self.emit(Event::ToolCall {
                    id: &call.id,
                    name: &call.name,
                    input: &call.input,
                });
            }
            if let Some(app) = self
                .env
                .app_dispatch(&config, &call.name, existing.as_ref())
            {
                let attempt = existing.unwrap_or(Attempt {
                    id: format!("a:{id}"),
                    effect: id.clone(),
                    owner_epoch: self.store.epoch(),
                    implementation: crate::broker::app_label(&hex(&app.helper)),
                    grant_reference: "spindle-app-tool".into(),
                });
                let request = DispatchRequest {
                    scope: SCOPE.into(),
                    owner_epoch: self.store.epoch(),
                    expected_revision: revision,
                    intent_revision,
                    intent,
                    attempt,
                };
                let task = call.name == "task";
                let need = Box::new(AppNeed {
                    call_id: self.store.call_id(&id).map_err(store_error)?,
                    id,
                    call,
                    request,
                    app,
                });
                return Ok(Some(if task {
                    ToolNeed::Task(need)
                } else {
                    ToolNeed::Ask(need)
                }));
            }
            return Ok(Some(ToolNeed::Run(Box::new(RunCall {
                id,
                call,
                intent_revision,
                intent,
                existing,
                config,
            }))));
        }
        Ok(None)
    }

    pub fn tool_ran(&mut self, need: RunCall, ran: ToolRan) -> Result<()> {
        let RunCall {
            id,
            call,
            intent_revision,
            ..
        } = need;
        let request = ran.request;
        let bytes = match ran.outcome {
            Ok(DispatchOutcome::Recorded(bytes)) => bytes,
            Ok(DispatchOutcome::Reused(bytes)) => {
                self.emit(Event::ToolCall {
                    id: &call.id,
                    name: &call.name,
                    input: &call.input,
                });
                self.emit(Event::ToolReused);
                bytes
            }
            Ok(DispatchOutcome::Reconcile) if request.attempt.owner_epoch < self.store.epoch() => {
                self.emit(Event::ToolCall {
                    id: &call.id,
                    name: &call.name,
                    input: &call.input,
                });
                self.emit(Event::ToolInterrupted);
                self.store
                    .seal_interrupted_strict_with_digest(&request, ran.helper)
                    .map_err(store_error)?
            }
            Ok(DispatchOutcome::Reconcile) => {
                return Err("a command's outcome is unknown; run `spindle continue`".into());
            }
            Err(error) => {
                return Err(format!(
                    "the tool call failed to run: {}. Run `spindle continue` to record it \
                 as not completed.",
                    store_error(error)
                ));
            }
        };
        let block: Block =
            serde_json::from_slice(&bytes).map_err(|_| "invalid recorded command result")?;
        if let Block::ToolResult {
            content, is_error, ..
        } = &block
        {
            if call.name == "show"
                && is_error != &Some(true)
                && let Some(items) = display_of(content)
            {
                self.emit(Event::Display { items: &items });
            }
            self.emit(Event::ToolResult {
                content,
                failed: is_error == &Some(true),
            });
        }
        let (revision, state) = self.head()?;
        let after = state.apply_tool(&id, vec![block]).map_err(turn_error)?;
        let mut proposal = self.proposal(
            format!("adopt-r{revision}"),
            Observation::Receipt(request.attempt.id.clone()),
            revision,
            &state,
            &after,
        )?;
        proposal.workspace = self
            .store
            .workspace_receipt(&request.attempt.id)
            .map_err(store_error)?;
        self.store
            .commit_turn_workspace_receipt(
                &proposal,
                &receipt_binding(&request.attempt, intent_revision, &request.intent, &bytes),
                ran.helper,
            )
            .map_err(store_error)?;
        if call.name == "run_code" {
            self.note_cost(Counted::Script(&request.attempt.id));
        }
        Ok(())
    }
}

impl Step<'_> {
    pub fn app_need(&mut self, need: &AppNeed) -> crate::Result<crate::broker::AppReservation> {
        let reservation = self.store.reserve_app_call(
            &need.request,
            &std::sync::atomic::AtomicBool::new(self.inbox.interrupted()),
            &need.app.helper,
        )?;
        if let crate::broker::AppReservation::Reserved { call, again: true } = &reservation {
            self.emit(Event::ToolCall {
                id: &call.id,
                name: &call.name,
                input: &call.input,
            });
        }
        Ok(reservation)
    }

    pub fn record_app_answer(
        &mut self,
        need: &AppNeed,
        answer: AppAnswer,
    ) -> crate::Result<DispatchOutcome> {
        let result = |content: String| crate::broker::AppResult {
            content,
            success: false,
            writes: vec![],
            deletes: vec![],
            links: vec![],
            directories: vec![],
            state: None,
            details: None,
            max_output: need.app.max_output,
        };
        let answer = match answer {
            AppAnswer::Result {
                content,
                failed,
                writes,
                deletes,
                links,
                directories,
                state,
                details,
            } => crate::broker::AppResult {
                content,
                success: !failed,
                writes,
                deletes,
                links,
                directories,
                state: state.map(|patch| patch.to_string().into_bytes()),
                details: details.map(|details| details.to_string().into_bytes()),
                max_output: need.app.max_output,
            },
            AppAnswer::Unknown(note) => result(app_tools::unknown_text(&note)),
            AppAnswer::NoAnswer => result(app_tools::no_answer_text(need.app.timeout)),
            AppAnswer::Stopped => return Err(Error::Cancelled),
        };
        self.store
            .record_app_answer(&need.request, &need.app.helper, answer)
    }

    pub fn app_answered(
        &mut self,
        need: AppNeed,
        outcome: crate::Result<DispatchOutcome>,
        waiting: Option<String>,
    ) -> Result<bool> {
        let AppNeed {
            id,
            call_id,
            call,
            request,
            app,
        } = need;
        let task = call.name == "task";
        let bytes = match outcome {
            Ok(DispatchOutcome::Recorded(bytes)) => bytes,
            Ok(DispatchOutcome::Reused(bytes)) => {
                self.emit(Event::ToolCall {
                    id: &call.id,
                    name: &call.name,
                    input: &call.input,
                });
                self.emit(Event::ToolReused);
                bytes
            }
            Ok(DispatchOutcome::Reconcile) => {
                return Err("an app tool call's answer is missing; run `spindle continue`".into());
            }
            Err(Error::Cancelled) if task => {
                let task_dir = self.env.task_place(&id);
                return Err(match waiting {
                    Some(why) => format!(
                        "the task cannot go on by itself: {why}. Settle it in its own session \
                         ({}), then continue this one.",
                        task_dir
                    ),
                    None => "stopped while the task was running; continue resumes it".into(),
                });
            }
            Err(Error::Cancelled) => {
                return Err(format!(
                    "stopped while the app was answering `{}`; continue asks it again",
                    call.name
                ));
            }
            Err(error) => {
                return Err(format!(
                    "the {} call failed: {}. Run `spindle continue` to ask again.",
                    if task { "task" } else { "app tool" },
                    store_error(error)
                ));
            }
        };
        let block: Block =
            serde_json::from_slice(&bytes).map_err(|_| "invalid recorded app tool result")?;
        if let Block::ToolResult {
            content, is_error, ..
        } = &block
        {
            self.emit(Event::ToolResult {
                content,
                failed: is_error == &Some(true),
            });
        }
        if let Some(bytes) = self
            .store
            .app_details(&request.attempt.id)
            .map_err(store_error)?
        {
            let details: Value =
                serde_json::from_slice(&bytes).map_err(|_| "invalid app details")?;
            self.emit(Event::AppToolDetails {
                call: &call_id,
                details: &details,
            });
        }
        let (revision, state) = self.head()?;
        let after = state.apply_tool(&id, vec![block]).map_err(turn_error)?;
        let mut proposal = self.proposal(
            format!("adopt-r{revision}"),
            Observation::Receipt(request.attempt.id.clone()),
            revision,
            &state,
            &after,
        )?;
        proposal.workspace = self
            .store
            .workspace_receipt(&request.attempt.id)
            .map_err(store_error)?;
        // The answer's change to the app state is set by the same step.
        proposal.state = self
            .store
            .adopted_state(SCOPE, revision, &request.attempt.id)
            .map_err(store_error)?;
        let prior = match proposal.state {
            Some(_) => Some(self.store.app_state(SCOPE, revision).map_err(store_error)?),
            None => None,
        };
        let committed = self
            .store
            .commit_turn_workspace_receipt(
                &proposal,
                &receipt_binding(
                    &request.attempt,
                    request.intent_revision,
                    &request.intent,
                    &bytes,
                ),
                app.helper,
            )
            .map_err(store_error)?;
        if let (Some(prior), Some(next)) = (prior, &proposal.state) {
            self.emit_state(&prior, next, committed);
        }
        Ok(true)
    }
}

pub enum Need {
    Continue,
    Summarize(Box<SummaryNeed>),
    Send(Box<ModelNeed>),
    Tool(ToolNeed),
    TaskWait,
    Stopped(Stop),
    Start(Box<StartCall>),
    Wait(Backoff),
    PackIdle(PackIdle),
    FollowUp { idle: PackIdle, follow: FollowUp },
}

impl Step<'_> {
    pub fn next_need(&mut self, revision: u64, state: &TurnContinuation) -> Result<Need> {
        if self.driver.until_unknown
            && let Some(effect) = state
                .pending_effects()
                .map_err(turn_error)?
                .into_iter()
                .next()
        {
            let (id, app) = match effect {
                Effect::Model { id, .. } => (id, false),
                Effect::Tool { id, call } => (id, crate::turn::app_tool_name(&call.name)),
            };
            if let Some(attempt) = self.store.attempt_for(&id).map_err(store_error)?
                && self
                    .store
                    .receipt(&attempt.id)
                    .map_err(store_error)?
                    .is_none()
                && (app
                    || (attempt.implementation == MODEL_IMPLEMENTATION
                        && self
                            .store
                            .rejection(&attempt.id)
                            .map_err(store_error)?
                            .is_none()))
            {
                let id = if app {
                    self.store.call_id(&id).map_err(store_error)?
                } else {
                    id
                };
                return Err(format!(
                    "stopped at unknown outcome `{id}`; inspect recovery before retrying or answering"
                ));
            }
        }
        match state.control().phase() {
            Phase::Ready | Phase::AfterTools => self.prepare_need(revision, state),
            Phase::ModelWait => self
                .model_need(revision, state)
                .map(|need| Need::Send(Box::new(need))),
            Phase::ToolsWait => self
                .tool_need(revision, state)?
                .map(Need::Tool)
                .ok_or_else(|| "no tool call could be resolved".into()),
            Phase::FinalReview => {
                if self.steer_queued(revision, state)? || self.commit_queued_state()? {
                    return Ok(Need::Continue);
                }
                let after = state.finish().map_err(turn_error)?;
                self.commit_action(
                    format!("finish-r{revision}"),
                    &Action::Finish,
                    state,
                    &after,
                )?;
                Ok(Need::Continue)
            }
            Phase::Stopped(stop) => {
                if stop == Stop::Complete
                    && let Some(need) = self.task_report_need(revision, state)?
                {
                    return Ok(need);
                }
                if self.commit_queued_state()? {
                    return Ok(Need::Continue);
                }
                Ok(Need::Stopped(stop))
            }
            Phase::Preparing => Err("unexpected turn phase".into()),
        }
    }

    pub fn prepare_need(&mut self, revision: u64, state: &TurnContinuation) -> Result<Need> {
        let command = format!("prepare-r{revision}");
        if let Some(payload) = self.store.admitted(SCOPE, &command).map_err(store_error)? {
            let Action::Prepare { request } = Action::decode(&payload).map_err(store_error)? else {
                return Err(format!(
                    "`{command}` holds another command; run `spindle doctor`"
                ));
            };
            let waiting =
                ModelWait::restore(&request).map_err(|_| "invalid admitted model request")?;
            let after = state
                .prepare(waiting.request().clone(), ROUTE)
                .map_err(turn_error)?;
            self.commit_action(command, &Action::Prepare { request }, state, &after)?;
            return Ok(Need::Continue);
        }
        // Steering first: its step is named by this revision, which a state
        // step would move on.
        if self.steer_queued(revision, state)? || self.commit_queued_state()? {
            return Ok(Need::Continue);
        }
        let config = self.settings()?;
        let route = self.session_route(&config.provider, Some(&config.model))?;
        let budget = route.history_budget;
        let overflowed = std::mem::take(&mut self.driver.compact_next);
        let prepared = PrepareNext {
            config,
            route,
            budget,
            overflowed,
        };
        if let Some(compact) = self.compact_need(revision, state, budget, overflowed)? {
            return self.compact_then(
                revision,
                state,
                compact,
                PrepareStage::BeforeRequest(prepared),
            );
        }
        self.prepare_after_compact(
            revision,
            state,
            PrepareStage::BeforeRequest(prepared),
            false,
        )
    }

    fn compact_then(
        &mut self,
        revision: u64,
        state: &TurnContinuation,
        compact: CompactCall,
        next: PrepareStage,
    ) -> Result<Need> {
        if matches!(compact.replacement, CompactReplacement::Summary { .. }) {
            return Ok(Need::Summarize(Box::new(SummaryNeed { compact, next })));
        }
        let changed = self.compact_done(state, compact, None)?.is_some();
        self.prepare_after_compact(revision, state, next, changed)
    }

    fn prepare_after_compact(
        &mut self,
        revision: u64,
        state: &TurnContinuation,
        stage: PrepareStage,
        changed: bool,
    ) -> Result<Need> {
        if changed && !matches!(stage, PrepareStage::Start(_)) {
            return Ok(Need::Continue);
        }
        match stage {
            PrepareStage::Start(mut pending) => {
                if changed {
                    pending.transcript = self.head()?.1.transcript().map_err(turn_error)?;
                } else if pending.overflowed {
                    return Err(OVERFLOW_STUCK.into());
                }
                self.commit_start(*pending)
            }
            PrepareStage::BeforeRequest(prepared) => {
                if prepared.overflowed {
                    return Err(OVERFLOW_STUCK.into());
                }
                let request = self.request(state, &prepared.config)?;
                let after = state.prepare(request.clone(), ROUTE).map_err(turn_error)?;
                let needed =
                    after.checkpoint().map_err(turn_error)?.len() + prepared.route.reply_reserve;
                let stage = PrepareStage::BeforeCommit {
                    request,
                    after: Box::new(after),
                    needed,
                };
                if needed > crate::MAX_PAYLOAD
                    && let Some(compact) =
                        self.compact_need(revision, state, prepared.budget, true)?
                {
                    return self.compact_then(revision, state, compact, stage);
                }
                self.prepare_after_compact(revision, state, stage, false)
            }
            PrepareStage::BeforeCommit {
                request,
                after,
                needed,
            } => {
                self.ensure_room(needed)?;
                let action = Action::Prepare {
                    request: ModelWait::new(request).checkpoint(),
                };
                self.commit_action(format!("prepare-r{revision}"), &action, state, &after)?;
                Ok(Need::Continue)
            }
        }
    }

    pub fn summary_done(
        &mut self,
        revision: u64,
        state: &TurnContinuation,
        need: SummaryNeed,
        summary: Option<String>,
    ) -> Result<Need> {
        let changed = self.compact_done(state, need.compact, summary)?.is_some();
        self.prepare_after_compact(revision, state, need.next, changed)
    }
}

/// The driver's input queue. Hosts own synchronization and waking; consuming input
/// is explicit so it can be restored if a restart fails before committing it.
pub trait Inbox: Send + Sync {
    fn next_steer(&self) -> Option<String>;
    fn take_state(&self) -> Vec<Value>;
    fn return_steering(&self, texts: Vec<String>);
    fn take_steering(&self) -> Vec<String>;
    fn interrupted(&self) -> bool;
}

impl Step<'_> {
    /// What a finished turn does about its background tasks: reads the report of one
    /// that ended (its answer, or that it stopped without one, and why when it says: the
    /// session goes on either way), or waits while any still runs.
    pub(crate) fn task_report_need(
        &mut self,
        revision: u64,
        state: &TurnContinuation,
    ) -> Result<Option<Need>> {
        if self.commit_task_report(revision, state)? {
            return Ok(Some(Need::Continue));
        }
        let runs = self.store.task_runs().map_err(store_error)?;
        Ok(runs
            .iter()
            .any(|run| run.queued && run.state == "running")
            .then_some(Need::TaskWait))
    }

    pub(crate) fn commit_task_report(
        &mut self,
        _revision: u64,
        state: &TurnContinuation,
    ) -> Result<bool> {
        let runs = self.store.task_runs().map_err(store_error)?;
        let mut retained = None;
        for (sequence, command, payload) in crate::view::ReadView::from_store(self.store)
            .queued_inputs()
            .map_err(store_error)?
        {
            if let Action::TaskReport { task, text } =
                Action::decode(&payload).map_err(store_error)?
            {
                retained = Some((sequence, command, task, text));
                break;
            }
        }
        let pending = |run: &&crate::meta::TaskRun| {
            run.queued
                && matches!(run.state.as_str(), "finished" | "stopped")
                && run.delivered.is_none()
        };
        let report = |run: &crate::meta::TaskRun| {
            crate::turn::task_report_of(&run.task, run.state == "stopped", run.answer.as_deref())
                .map_err(store_error)
        };
        // One admitted before a crash goes as it was admitted, unless its task moved on
        // since (started again after it stopped, say): then it tells nothing true now,
        // and is dropped for the task's report as it stands.
        let mut chosen = None;
        if let Some((sequence, command, task, text)) = retained {
            match runs.iter().find(|run| run.task == task).filter(pending) {
                Some(run) if report(run)? == text => chosen = Some((run, command, text)),
                _ => self
                    .store
                    .drop_admission(sequence, &format!("drop-{sequence}"))
                    .map_err(store_error)?,
            }
        }
        let (run, command, text) = match chosen {
            Some(chosen) => chosen,
            None => {
                let Some(run) = runs.iter().find(pending) else {
                    return Ok(false);
                };
                (run, self.report_command(&run.task)?, report(run)?)
            }
        };
        let after = state.task_report(text.clone()).map_err(turn_error)?;
        self.commit_action(
            command,
            &Action::TaskReport {
                task: run.task.clone(),
                text: text.clone(),
            },
            state,
            &after,
        )?;
        self.emit(Event::TaskReported {
            task: &run.task,
            text: &text,
        });
        Ok(true)
    }

    /// The command a task's next report goes as: a task started again after it reported
    /// stopping reports again, under a command of its own.
    fn report_command(&mut self, task: &str) -> Result<String> {
        let base = format!("task-report-{task}");
        let mut command = base.clone();
        for next in 2.. {
            if self
                .store
                .admitted(SCOPE, &command)
                .map_err(store_error)?
                .is_none()
            {
                break;
            }
            command = format!("{base}-{next}");
        }
        Ok(command)
    }

    fn steer_queued(&mut self, revision: u64, state: &TurnContinuation) -> Result<bool> {
        if self.commit_task_report(revision, state)? {
            return Ok(true);
        }
        let command = format!("steer-r{revision}");
        let text = match self.store.admitted(SCOPE, &command).map_err(store_error)? {
            Some(payload) => match Action::decode(&payload).map_err(store_error)? {
                Action::Steer { text } => text,
                _ => {
                    return Err(format!(
                        "`{command}` holds another command; run `spindle doctor`"
                    ));
                }
            },
            None if state.control().phase() == Phase::AfterTools
                && let Some(text) = self.env.pack_round_steer(self.store, state)? =>
            {
                text
            }
            None => match self.inbox.next_steer() {
                Some(text) => text,
                None => return Ok(false),
            },
        };
        let after = state.steer(text.clone()).map_err(turn_error)?;
        self.commit_action(
            command,
            &Action::Steer { text: text.clone() },
            state,
            &after,
        )?;
        self.emit(Event::Steered { text: &text });
        Ok(true)
    }

    pub fn commit_state(&mut self, patch: &Value) -> Result<u64> {
        if !patch.is_object() {
            return Err("`state` is a JSON merge patch: an object".into());
        }
        let (revision, state) = self.head()?;
        // A step of its own goes between steps: in the middle of one (a request or tool
        // call cut off by a stop), a state step would hide the point a retry goes back to.
        if !matches!(
            state.control().phase(),
            Phase::Ready | Phase::AfterTools | Phase::FinalReview | Phase::Stopped(_)
        ) {
            return Err(
                "the turn stopped in the middle of a step; continue or rewind it \
                        first, or send the change with the next prompt"
                    .into(),
            );
        }
        let prior = self.store.app_state(SCOPE, revision).map_err(store_error)?;
        let bytes = patch.to_string().into_bytes();
        let next = state_after(&prior, &bytes)?;
        let id = format!("state-r{revision}-{}", hex(&Sha256::digest(&bytes)[..6]));
        let sequence = self.admit(&id, &Action::State { patch: bytes })?;
        let mut proposal = self.proposal(
            format!("commit-{id}"),
            Observation::Admission(sequence),
            revision,
            &state,
            &state,
        )?;
        proposal.state = Some(next.clone());
        let committed = self
            .store
            .commit_turn_action(&proposal)
            .map_err(store_error)?;
        self.emit_state(&prior, &next, committed);
        Ok(committed)
    }

    /// Commits the state changes queued while the turn ran (each a step of its own);
    /// one the store cannot keep is dropped and said so. Whether any was committed.
    pub fn commit_queued_state(&mut self) -> Result<bool> {
        let queued = self.inbox.take_state();
        let any = !queued.is_empty();
        for patch in queued {
            if let Err(reason) = self.commit_state(&patch) {
                self.emit(Event::StateRefused { reason: &reason });
            }
        }
        Ok(any)
    }

    /// Tells the observer which top-level keys of the app state a step changed.
    pub fn emit_state(&self, prior: &[u8], next: &[u8], revision: u64) {
        let (Ok(Value::Object(before)), Ok(Value::Object(after))) = (
            serde_json::from_slice::<Value>(prior),
            serde_json::from_slice::<Value>(next),
        ) else {
            return;
        };
        let mut changed: Vec<String> = after
            .iter()
            .filter(|(key, value)| before.get(*key) != Some(value))
            .map(|(key, _)| key.clone())
            .chain(
                before
                    .keys()
                    .filter(|key| !after.contains_key(*key))
                    .cloned(),
            )
            .collect();
        changed.sort();
        if !changed.is_empty() {
            self.emit(Event::State {
                revision,
                changed: &changed,
            });
        }
    }
}

pub struct CompactCall {
    pub command: String,
    size: usize,
    pub replacement: CompactReplacement,
}

pub enum CompactReplacement {
    Ready(TranscriptCheckpoint),
    Summary {
        dropped: Vec<Message>,
        prompt: Option<usize>,
        keep: usize,
        transcript: TranscriptCheckpoint,
    },
}

impl Step<'_> {
    pub fn compact_need(
        &mut self,
        revision: u64,
        state: &TurnContinuation,
        budget: usize,
        force: bool,
    ) -> Result<Option<CompactCall>> {
        let transcript = state.transcript_ref().map_err(turn_error)?;
        let size = transcript_len(&transcript)?;
        let command = format!("compact-r{revision}");
        // A compaction admitted before a crash is committed exactly as admitted (its
        // summary was paid for once and is never asked for again), whatever the size.
        let admitted = self.store.admitted(SCOPE, &command).map_err(store_error)?;
        if admitted.is_none() && size <= budget && !force {
            return Ok(None);
        }
        let replacement = match admitted {
            Some(payload) => match Action::decode(&payload).map_err(store_error)? {
                Action::Compact { transcript } => CompactReplacement::Ready(
                    TranscriptCheckpoint::restore(&transcript)
                        .map_err(|error| format!("history rejected ({error:?})"))?,
                ),
                _ => {
                    return Err(format!(
                        "`{command}` holds another command; run `spindle doctor`"
                    ));
                }
            },
            None => {
                // A forced compaction (the provider said the request did not fit) keeps
                // a quarter of what there is, however the budget was estimated.
                let keep = if force {
                    (size / 4).min(budget / COMPACT_KEEP_SHARE).max(8 * 1024)
                } else {
                    (budget / COMPACT_KEEP_SHARE).max(24 * 1024)
                };
                let Some((cut, prompt)) = compaction_cut(&transcript.messages, keep) else {
                    return Ok(None);
                };
                // Each compaction must shrink history by a quarter (below); when the part
                // it can drop is too small for that even with the largest summary,
                // nothing is asked of the model.
                let dropped: usize = transcript.messages[..cut]
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| Some(*index) != prompt)
                    .map(|(_, message)| message_size(message))
                    .sum();
                if dropped < size / 4 + SUMMARY_BYTES {
                    return Ok(None);
                }
                CompactReplacement::Summary {
                    dropped: transcript.messages[..cut].to_vec(),
                    prompt,
                    keep,
                    transcript: transcript.into_owned(),
                }
            }
        };
        Ok(Some(CompactCall {
            command,
            size,
            replacement,
        }))
    }

    pub fn compact_done(
        &mut self,
        state: &TurnContinuation,
        need: CompactCall,
        summary: Option<String>,
    ) -> Result<Option<TurnContinuation>> {
        let CompactCall {
            command,
            size,
            replacement,
        } = need;
        let replacement = match replacement {
            CompactReplacement::Ready(transcript) => transcript,
            CompactReplacement::Summary {
                transcript, keep, ..
            } => {
                let Some(replacement) = compacted(&transcript, keep, summary.as_deref()) else {
                    return Ok(None);
                };
                replacement
            }
        };
        let bytes = transcript_bytes(&replacement)?;
        // Each compaction must shrink history by a quarter, so repeats are bounded.
        if bytes.len() > size / 4 * 3 {
            return Ok(None);
        }
        let after = state.compact(replacement).map_err(turn_error)?;
        let compacted_size = bytes.len();
        self.commit_action(
            command,
            &Action::Compact { transcript: bytes },
            state,
            &after,
        )?;
        self.emit(Event::Compacted {
            before: size,
            after: compacted_size,
        });
        Ok(Some(after))
    }
}

pub struct SummaryNeed {
    pub compact: CompactCall,
    next: PrepareStage,
}

struct PrepareNext {
    config: Settings,
    route: Route,
    budget: usize,
    overflowed: bool,
}

enum PrepareStage {
    Start(Box<StartPending>),
    BeforeRequest(PrepareNext),
    BeforeCommit {
        request: PreparedRequest<Vec<u8>>,
        after: Box<TurnContinuation>,
        needed: usize,
    },
}

struct StartPending {
    prompt: String,
    start: Start,
    start_state: Option<StartState>,
    config: Settings,
    live: Route,
    transcript: TranscriptCheckpoint,
    overflowed: bool,
}

impl Step<'_> {
    #[allow(clippy::too_many_arguments)]
    pub fn start_need(
        &mut self,
        prompt: String,
        start: Start,
        start_state: Option<StartState>,
        config: Settings,
        live: Route,
        budget: usize,
    ) -> Result<Need> {
        let snapshot = self.store.snapshot().map_err(store_error)?;
        if snapshot.revision == 0 {
            return self.commit_start(StartPending {
                prompt,
                start,
                start_state,
                config,
                live,
                transcript: TranscriptCheckpoint {
                    route_binding: ROUTE.into(),
                    messages: vec![],
                },
                overflowed: false,
            });
        }
        self.ensure_idle()?;
        let prior = crate::turn::restored(&snapshot.continuation).map_err(turn_error)?;
        let transcript = prior.transcript().map_err(turn_error)?;
        let command = self.turn_command(snapshot.revision, &prompt)?;
        if self
            .store
            .admitted(SCOPE, &command)
            .map_err(store_error)?
            .is_some()
        {
            return self.commit_start(StartPending {
                prompt,
                start,
                start_state,
                config,
                live,
                transcript,
                overflowed: false,
            });
        }
        let history = transcript_len(&transcript)?;
        let overflowed = std::mem::take(&mut self.driver.compact_next);
        let force = overflowed || 2 * history + live.reply_reserve > crate::MAX_PAYLOAD;
        let pending = StartPending {
            prompt,
            start,
            start_state,
            config,
            live,
            transcript,
            overflowed,
        };
        if let Some(compact) = self.compact_need(snapshot.revision, &prior, budget, force)? {
            return self.compact_then(
                snapshot.revision,
                &prior,
                compact,
                PrepareStage::Start(Box::new(pending)),
            );
        }
        self.prepare_after_compact(
            snapshot.revision,
            &prior,
            PrepareStage::Start(Box::new(pending)),
            false,
        )
    }

    fn commit_start(&mut self, pending: StartPending) -> Result<Need> {
        let StartPending {
            prompt,
            start,
            start_state,
            config,
            live,
            mut transcript,
            ..
        } = pending;
        let revision = self.store.snapshot().map_err(store_error)?.revision;
        transcript.messages.push(Message {
            role: "user".into(),
            content: vec![Block::Text {
                text: prompt.clone(),
            }],
        });
        let command = self.turn_command(revision, &prompt)?;
        let (initial, request) = match self.store.admitted(SCOPE, &command).map_err(store_error)? {
            Some(payload) => {
                let Action::Start { initial, request } =
                    Action::decode(&payload).map_err(store_error)?
                else {
                    return Err(format!(
                        "`{command}` holds another command; run `spindle doctor`"
                    ));
                };
                let initial = crate::turn::restored(&initial).map_err(turn_error)?;
                let request =
                    ModelWait::restore(&request).map_err(|_| "invalid admitted model request")?;
                (initial, request.request().clone())
            }
            None => {
                let initial = TurnContinuation::new(
                    SCOPE.into(),
                    execution_id(SCOPE, &command).map_err(store_error)?,
                    live.contract,
                    None,
                    transcript,
                )
                .map_err(turn_error)?;
                let request = self.request(&initial, &config)?;
                (initial, request)
            }
        };
        let next = initial
            .prepare(request.clone(), ROUTE)
            .map_err(turn_error)?;
        self.ensure_room(next.checkpoint().map_err(turn_error)?.len() + live.reply_reserve)?;
        // Files that cannot form a workspace are refused before the turn is admitted
        // (a queued admission that never commits would block every later prompt).
        let planned = match &start {
            Start::Same | Start::Root(_) => None,
            Start::Import(files, links, directories) => Some(planned_tree(
                Tree {
                    files: vec![],
                    links: links.clone(),
                    directories: directories.clone(),
                },
                files,
            )),
            Start::Add(files) => {
                let tree = match self.store.snapshot().map_err(store_error)?.workspace {
                    Some(root) => self.store.tree_map(SCOPE, &root).map_err(store_error)?,
                    None => Tree::default(),
                };
                Some(planned_tree(tree, files))
            }
        };
        if planned.is_some_and(|tree| crate::workspace::validate_tree(&tree).is_err()) {
            return Err(BAD_FILES.into());
        }
        // App state sent with the prompt is set by the turn's first step; a change that
        // cannot be kept is refused before the turn is admitted, as bad files are.
        let prior_state = self.store.app_state(SCOPE, revision).map_err(store_error)?;
        let start_state = match start_state {
            Some(StartState::Patch(patch)) => {
                Some(state_after(&prior_state, patch.to_string().as_bytes())?)
            }
            Some(StartState::Exact(state)) if state != prior_state => Some(state),
            _ => None,
        };
        let action = Action::Start {
            initial: initial.checkpoint().map_err(turn_error)?,
            request: ModelWait::new(request).checkpoint(),
        };
        let sequence = self.admit(&command, &action)?;
        let mut proposal = self.proposal(
            format!("commit-{command}"),
            Observation::Admission(sequence),
            revision,
            &initial,
            &next,
        )?;
        proposal.state = start_state;
        // Per owner epoch: a stage an earlier process began cannot be continued.
        let stage = format!(
            "{}-e{}",
            if revision == 0 {
                "initial-workspace".to_string()
            } else {
                format!("workspace-{command}")
            },
            self.store.epoch()
        );
        let root = match start {
            Start::Same => None,
            Start::Root(root) => Some(root),
            Start::Import(files, links, directories) => Some(self.stage_workspace(
                stage,
                Tree {
                    files: vec![],
                    links,
                    directories,
                },
                files,
            )?),
            Start::Add(files) => {
                let tree = match self.store.snapshot().map_err(store_error)?.workspace {
                    Some(root) => self.store.tree_map(SCOPE, &root).map_err(store_error)?,
                    None => Tree::default(),
                };
                Some(self.stage_workspace(stage, tree, files)?)
            }
        };
        let committed = if let Some(root) = root {
            proposal.workspace = Some(root);
            self.store
                .commit_turn_workspace_start(&proposal)
                .map_err(store_error)?
        } else {
            self.store
                .commit_turn_action(&proposal)
                .map_err(store_error)?
        };
        self.emit(Event::Prompt { text: &prompt });
        if let Some(next) = &proposal.state {
            self.emit_state(&prior_state, next, committed);
        }
        Ok(Need::Continue)
    }
}

pub struct PackIdle {
    pub revision: u64,
    pub execution: String,
}

impl Step<'_> {
    pub fn pack_idle_need(&mut self, revision: u64) -> Result<Option<PackIdle>> {
        if !self.env.pack_active(self.store)? {
            return Ok(None);
        }
        let (head, state) = self.head()?;
        if head != revision {
            return Ok(None);
        }
        Ok(Some(PackIdle {
            revision,
            execution: state.execution().into(),
        }))
    }

    pub fn pack_idle_done(&self, end: PackEnd) -> Option<FollowUp> {
        for note in &end.notes {
            self.emit(Event::Nested { label: note });
        }
        end.follow_up
    }

    pub fn follow_up_done(
        &mut self,
        need: PackIdle,
        follow: FollowUp,
        waited: bool,
    ) -> Result<Need> {
        if !waited {
            self.env
                .cancel_follow_up(self.store, need.revision, &need.execution)?;
            return Ok(Need::Stopped(Stop::Complete));
        }
        Ok(Need::Start(Box::new(StartCall {
            prompt: follow.prompt,
            start: Start::Same,
            state: None,
            joined: vec![],
        })))
    }
}

impl Step<'_> {
    /// Rewinds to the newest point before the pending model request so the next drive
    /// prepares it again under a fresh identity (a settled point re-sends its prompt).
    /// Jobs keep running and queued settings stay: only the request is undone. A `cut`
    /// for queued messages names that point (`redo_point`) and nothing else will do;
    /// there they are read first, or, re-sending a turn's prompt, join it.
    pub fn retry_in_place(&mut self, cut: Option<u64>) -> Result<Need> {
        let (revision, state) = self.head()?;
        let prompt = state
            .transcript()
            .map_err(turn_error)?
            .messages
            .iter()
            .rev()
            .find(|message| is_prompt(message))
            .map(|message| text_of(&message.content));
        let targets = self.store.rewind_targets(SCOPE).map_err(store_error)?;
        let Some(target) = targets.first().copied() else {
            return Err(
                "the session's first request cannot be re-sent automatically; run `spindle retry`"
                    .into(),
            );
        };
        match cut {
            Some(point) if point != target.revision => {
                return Err(
                    "the reply was cut, but the point before its request cannot be restored; \
                     the request has an unknown outcome (it may have been billed); run `spindle retry`"
                        .into(),
                );
            }
            Some(_) => {}
            None => self.emit(Event::Retrying {
                target: target.revision,
                head: revision,
            }),
        }
        // A turn's first request re-sends its prompt from the end of the turn before; the
        // files it brought (attachments) are in the workspace the request saw.
        let started = self.store.snapshot().map_err(store_error)?.workspace;
        // The app state the interrupted turn had (its prompt's, and any change made
        // before its request) comes back with the restarted turn.
        let carried = self.store.app_state(SCOPE, revision).map_err(store_error)?;
        self.rewind_to(target.revision, true)?;
        if target.kind == RewindTargetKind::Settled {
            let mut prompt = prompt.ok_or("the interrupted prompt could not be found")?;
            let current = self.store.snapshot().map_err(store_error)?.workspace;
            let start = match started {
                Some(root) if Some(&root) != current.as_ref() => Start::Root(root),
                _ => Start::Same,
            };
            // A turn starts with one message: the waiting ones join the prompt they
            // followed, as the user would have written it.
            let joined = match cut {
                Some(_) => self.inbox.take_steering(),
                None => vec![],
            }
            .into_iter()
            .filter(|text| !text.trim().is_empty() && text.len() <= MAX_STEER)
            .collect::<Vec<_>>();
            for text in &joined {
                prompt = format!("{prompt}\n\n{text}");
            }
            return Ok(Need::Start(Box::new(StartCall {
                prompt,
                start,
                state: Some(StartState::Exact(carried)),
                joined,
            })));
        }
        Ok(Need::Continue)
    }
}

pub enum ModelNext {
    Adopted,
    Retry(Rejection),
    Need(Need),
}

pub struct StartCall {
    pub prompt: String,
    pub start: Start,
    pub state: Option<StartState>,
    pub joined: Vec<String>,
}

impl Step<'_> {
    pub fn model_delivered(
        &mut self,
        need: ModelNeed,
        sent: ModelSent,
        retries: &mut u32,
    ) -> Result<Need> {
        match self.model_sent(need, sent)? {
            ModelNext::Adopted => {
                *retries = 0;
                Ok(Need::Continue)
            }
            ModelNext::Need(need) => {
                *retries = 0;
                Ok(need)
            }
            ModelNext::Retry(rejection) => self.backoff(rejection, retries).map(Need::Wait),
        }
    }

    pub fn stopped_next(&mut self, revision: u64, stop: Stop) -> Result<Need> {
        if stop == Stop::Complete {
            let (_, state) = self.head()?;
            if let Some(need) = self.task_report_need(revision, &state)? {
                return Ok(need);
            }
        }
        if stop == Stop::Complete
            && let Some(idle) = self.pack_idle_need(revision)?
        {
            return Ok(Need::PackIdle(idle));
        }
        Ok(Need::Stopped(stop))
    }
}

fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [...]", &text[..end])
}

fn message_size(message: &Message) -> usize {
    serde_json::to_vec(message)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX / 8)
}

/// Deterministic, host-produced compaction: older messages become one digest of the
/// prompts, replies and tool calls they held; about `keep` bytes of recent history stay
/// verbatim. The kept part starts at an assistant message, so tool results always
/// follow their calls. None if nothing can be dropped.
pub(crate) fn compacted(
    transcript: &TranscriptCheckpoint,
    keep: usize,
    summary: Option<&str>,
) -> Option<TranscriptCheckpoint> {
    let messages = &transcript.messages;
    let (cut, prompt) = compaction_cut(messages, keep)?;
    let digest = match summary {
        Some(summary) => summary.trim().to_string(),
        None => digest(&messages[..cut], prompt),
    };
    let mut replacement = vec![Message {
        role: "user".into(),
        content: vec![Block::Text {
            text: format!("{COMPACTED}\n{digest}\n{COMPACTED_TRAILER}"),
        }],
    }];
    replacement.extend(prompt.map(|index| messages[index].clone()));
    replacement.extend(messages[cut..].iter().cloned());
    if replacement.len() > messages.len() {
        return None;
    }
    Some(TranscriptCheckpoint {
        route_binding: transcript.route_binding.clone(),
        messages: replacement,
    })
}

/// Where a compaction cuts: about `keep` bytes of recent history stay, starting at an
/// assistant message. Also the current prompt's index when it falls in the dropped
/// part; it stays verbatim (retry re-sends it), placed after the summary.
fn compaction_cut(messages: &[Message], keep: usize) -> Option<(usize, Option<usize>)> {
    let mut kept = 0usize;
    let mut cut = None;
    for index in (1..messages.len()).rev() {
        kept += message_size(&messages[index]);
        if messages[index].role == "assistant" {
            if kept <= keep || cut.is_none() {
                cut = Some(index);
            }
            if kept > keep {
                break;
            }
        }
    }
    let cut = cut?;
    let prompt = if messages[cut..].iter().any(is_prompt) {
        None
    } else {
        messages[..cut].iter().rposition(is_prompt)
    };
    Some((cut, prompt))
}

/// The text of an earlier compaction folded into the next one.
fn folded(text: &str) -> &str {
    text.trim_start_matches(COMPACTED)
        .trim_end()
        .trim_end_matches(COMPACTED_TRAILER)
        .trim_end()
        .trim_end_matches(CURRENT_REQUEST)
}

/// The deterministic digest of a compaction's dropped part.
fn digest(dropped: &[Message], prompt: Option<usize>) -> String {
    let messages = dropped;
    let mut entries = vec![];
    for (index, message) in messages.iter().enumerate() {
        if Some(index) == prompt {
            entries.push(CURRENT_REQUEST.into());
            continue;
        }
        for block in &message.content {
            match block {
                Block::Text { text } if text.starts_with(COMPACTED) => {
                    entries.push(clip(folded(text), 6 * 1024))
                }
                Block::Text { text } if message.role == "user" => {
                    entries.push(format!("User asked: {}", clip(text, 1024)))
                }
                Block::Text { text } | Block::PartialStream { text } => {
                    if !text.trim().is_empty() {
                        entries.push(format!("Assistant: {}", clip(text, 512)))
                    }
                }
                Block::ToolUse { name, input, .. } => {
                    entries.push(format!("Tool: {}", clip(&describe_call(name, input), 200)))
                }
                Block::ToolResult {
                    is_error: Some(true),
                    ..
                } => entries.push("  (that call failed)".into()),
                _ => {}
            }
        }
    }
    // Newest entries matter most; older ones are dropped first to bound the digest.
    let mut digest = vec![];
    let mut size = 0;
    for entry in entries.iter().rev() {
        size += entry.len() + 1;
        if size > 8 * 1024 {
            digest.push("(older history omitted)".to_string());
            break;
        }
        digest.push(entry.clone());
    }
    digest.reverse();
    digest.join("\n")
}

fn state_after(prior: &[u8], patch: &[u8]) -> Result<Vec<u8>> {
    crate::transition::apply_state_patch(prior, patch).map_err(|error| match error {
        Error::Exhausted => format!(
            "the app state would be over {} KiB",
            crate::transition::MAX_STATE / 1024
        ),
        _ => "`state` is a JSON merge patch: an object".into(),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A user's request: an all-text user message that is neither a compaction digest nor
/// a message sent while the turn ran (steering).
pub(crate) fn is_prompt(message: &Message) -> bool {
    message.role == "user"
        && !message.content.is_empty()
        && message.content.iter().all(|block| {
            matches!(block, Block::Text { text }
                if !text.starts_with(COMPACTED) && !text.starts_with(STEERED) && !text.starts_with("[task "))
        })
}

const BAD_FILES: &str = "the files do not form a valid workspace (paths are relative, without \
     empty, `.` or `..` parts; a file cannot sit where a file or link already is)";

/// `tree` with `files` added (a path already there, as a file, link or empty directory,
/// is replaced), each file under its object digest.
fn planned_tree(mut tree: Tree, files: &[File]) -> Tree {
    for (path, bytes, executable) in files {
        let inside = |directory: &str| path.starts_with(&format!("{directory}/"));
        tree.files.retain(|entry| entry.path != *path);
        tree.links.retain(|link| link.path != *path);
        tree.directories
            .retain(|directory| directory != path && !inside(directory));
        tree.files.push(Entry {
            path: path.clone(),
            object: crate::artifact::object_digest(bytes),
            executable: *executable,
        });
    }
    tree.files.sort_by(|a, b| a.path.cmp(&b.path));
    tree
}

/// Why a turn stops when the provider keeps saying the request is too long.
const OVERFLOW_STUCK: &str = "the provider says the request does not fit the model's context \
     window, and the history cannot be compacted further; rewind to an earlier point \
     (`spindle targets`) or start a new thread";

/// Longest message the turn state accepts as steering.
pub const MAX_STEER: usize = 64 * 1024;

/// How a steering message starts in history (the engine's shared wrapper).
const STEERED: &str = "[queued-user-update]";

/// What the user wrote in a steering message: the wrapper's guidance for the model
/// ends at its first `User update:` line, and the rest is the literal text.
pub fn steered_text(text: &str) -> Option<&str> {
    let rest = text.strip_prefix(STEERED)?;
    rest.split_once("\nUser update:\n").map(|(_, user)| user)
}

/// Adds `steer` (what the user wrote) to each steering message's text block among
/// `messages` (the transcript's JSON form), so screens show the user's words and not
/// the guidance the model reads with them.
pub fn annotate_steering(messages: &mut Value) {
    for message in messages.as_array_mut().into_iter().flatten() {
        if message["role"] != "user" {
            continue;
        }
        for block in message["content"].as_array_mut().into_iter().flatten() {
            if block["type"] == "text"
                && let Some(user) = block["text"].as_str().and_then(steered_text)
            {
                block["steer"] = json!(user);
            }
        }
    }
}

/// The prompt of an admitted turn start, if `payload` is one.
fn admitted_prompt(payload: &[u8]) -> Option<String> {
    let Ok(Action::Start { initial, .. }) = Action::decode(payload) else {
        return None;
    };
    let transcript = crate::turn::restored(&initial)
        .ok()?
        .into_transcript()
        .ok()?;
    match transcript.messages.last()?.content.as_slice() {
        [Block::Text { text }] => Some(text.clone()),
        _ => None,
    }
}

/// Another owner, or a connection briefly holding SQLite's lock, has the store.
const LOCKED: &str = "another spindle process or connection is using this session's store; \
wait for it to finish, then run the command again";

/// A full disk or quota. Nothing saved is lost: the step that failed is repeated (or
/// found saved after all) once there is room.
const DISK_FULL: &str = "the disk is full, so the session could not save its next step; \
free some space, then run `spindle continue` (it resumes from the last saved step)";

pub(crate) fn store_error(error: Error) -> String {
    match error {
        Error::Locked => LOCKED.into(),
        error if error.is_disk_full() => DISK_FULL.into(),
        Error::Sql(rusqlite::Error::SqliteFailure(failure, _))
            if failure.code == rusqlite::ErrorCode::SystemIoFailure =>
        {
            "the disk reported an error while the session was saving a step (SQLite I/O error); \
             check the disk and free space, then run `spindle continue`"
                .into()
        }
        Error::Paused => {
            "the session's store is paused (an earlier storage error, or a rewind still being \
             finished); run the command again"
                .into()
        }

        Error::Exhausted => {
            "a store capacity limit was reached (revisions or stored files); `spindle doctor` shows which".into()
        }
        Error::Model(error) => model_error(error),
        error @ Error::TooLarge { .. } => error.to_string(),
        other => format!("store refused the step ({other:?})"),
    }
}

/// A model send's failure as people read it.
pub(crate) fn model_error(error: crate::ModelError) -> String {
    match error {
        crate::ModelError::Http(401 | 403) => "the provider rejected the credentials".into(),
        crate::ModelError::Http(429) => "the provider rate-limited the request (HTTP 429)".into(),
        crate::ModelError::Http(status) => format!("the provider returned HTTP {status}"),
        crate::ModelError::Deadline => "the provider did not answer in time".into(),
        crate::ModelError::Transport => "the connection to the provider failed".into(),
        crate::ModelError::Protocol => "the provider stream could not be parsed".into(),
        crate::ModelError::Exhausted => "the provider response was too large".into(),
        other => format!("provider attempt failed ({other:?})"),
    }
}

pub(crate) fn context_overflow(status: u16, body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    matches!(status, 400 | 413 | 422)
        && [
            "context_length",
            "context length",
            "context window",
            "maximum context",
            "too many tokens",
            "prompt is too long",
            "input is too long",
            "exceeds max length",
            "reduce the length",
        ]
        .iter()
        .any(|phrase| body.contains(phrase))
}

pub(crate) fn transcript_bytes(transcript: &TranscriptCheckpoint) -> Result<Vec<u8>> {
    transcript
        .checkpoint()
        .map_err(|error| format!("history rejected ({error:?})"))
}

/// `transcript_bytes(transcript)?.len()`, without building them.
fn transcript_len(transcript: &TranscriptCheckpoint) -> Result<usize> {
    transcript
        .checkpoint_len()
        .map_err(|error| format!("history rejected ({error:?})"))
}

fn turn_error(error: dext_transcript::continuation::Error) -> String {
    format!("turn state rejected the step ({error:?})")
}

pub(crate) fn stated_output_limit(status: u16, body: &str, sent: u32) -> Option<u32> {
    let lower = body.to_ascii_lowercase();
    if !matches!(status, 400 | 422)
        || context_overflow(status, body)
        || ![
            "max_tokens",
            "max_output_tokens",
            "max_completion_tokens",
            "output tokens",
            "completion tokens",
        ]
        .iter()
        .any(|phrase| lower.contains(phrase))
    {
        return None;
    }
    lower
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|digits| digits.parse::<u32>().ok())
        .filter(|number| (1024..sent).contains(number))
        .max()
}

pub fn describe_call(name: &str, input: &Value) -> String {
    let path = input["path"].as_str().unwrap_or("?");
    let line = match name {
        "bash" => format!(
            "$ {}",
            input["command"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| input.to_string())
        ),
        "read_file" => match input["offset"].as_u64() {
            Some(offset) => format!("read {path} from line {offset}"),
            None => format!("read {path}"),
        },
        "write_file" => format!(
            "write {path} ({} bytes)",
            input["content"].as_str().map(str::len).unwrap_or_default()
        ),
        "edit_file" => format!("edit {path}"),
        "start_job" => format!(
            "& {}",
            input["command"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| input.to_string())
        ),
        "read_job" => match (input["job"].as_str(), input["wait"].as_u64()) {
            (None, _) => "list background jobs".into(),
            (Some(job), Some(wait)) if wait > 0 => format!("read {job} (wait up to {wait} s)"),
            (Some(job), _) => format!("read {job}"),
        },
        "stop_job" => format!("stop {}", input["job"].as_str().unwrap_or("?")),
        "show" => format!(
            "show {}",
            input["paths"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        "settings" if input.as_object().is_none_or(|fields| fields.is_empty()) => {
            "read settings".into()
        }
        "spindle_source" => format!(
            "spindle source: {}",
            input["action"].as_str().unwrap_or("?")
        ),
        "spindle_install" => format!(
            "request install: {}",
            input["summary"].as_str().unwrap_or_default()
        ),
        "run_code" => format!(
            "{} script:\n{}",
            input["language"].as_str().unwrap_or("?"),
            input["code"].as_str().unwrap_or_default()
        ),
        other => format!("{other} {input}"),
    };
    let mut lines = line.lines();
    let first: String = lines.next().unwrap_or_default().chars().take(300).collect();
    match lines.count() {
        0 => first,
        more => format!("{first}\n  ... ({more} more lines)"),
    }
}

impl Step<'_> {
    pub(crate) fn head(&mut self) -> Result<(u64, TurnContinuation)> {
        let snapshot = self.store.snapshot().map_err(store_error)?;
        if snapshot.revision == 0 {
            return Err("this session has no turns yet".into());
        }
        let state = crate::turn::restored(&snapshot.continuation).map_err(turn_error)?;
        Ok((snapshot.revision, state))
    }
    pub(crate) fn proposal(
        &mut self,
        id: String,
        observation: Observation,
        expected_revision: u64,
        before: &TurnContinuation,
        after: &TurnContinuation,
    ) -> Result<Proposal> {
        Ok(Proposal {
            id,
            scope: SCOPE.into(),
            observation,
            expected_revision,
            owner_epoch: self.store.epoch(),
            engine: ENGINE.into(),
            continuation: after.checkpoint().map_err(turn_error)?,
            workspace: None,
            effects: new_intents(Some(before), after).map_err(store_error)?,
            state: None,
        })
    }
    pub(crate) fn admit(&mut self, id: &str, action: &Action) -> Result<u64> {
        let lineage = self.store.lineage().map_err(store_error)?;
        let admission = self
            .store
            .admit(&Command {
                scope: SCOPE.into(),
                id: id.into(),
                lineage,
                kind: ENGINE.into(),
                payload: action.encode().map_err(store_error)?,
            })
            .map_err(store_error)?;
        Ok(admission.sequence)
    }
    pub(crate) fn turn_command(&mut self, revision: u64, prompt: &str) -> Result<String> {
        let base = if revision == 0 {
            "start".to_string()
        } else {
            format!("turn-r{revision}")
        };
        let mut command = base.clone();
        for next in 2.. {
            let Some(payload) = self.store.admitted(SCOPE, &command).map_err(store_error)? else {
                break;
            };
            if admitted_prompt(&payload).as_deref() == Some(prompt) {
                return Ok(command);
            }
            command = format!("{base}-{next}");
        }
        if command != base {
            self.drop_queued()?;
        }
        Ok(command)
    }
    pub(crate) fn commit_action(
        &mut self,
        id: String,
        action: &Action,
        before: &TurnContinuation,
        after: &TurnContinuation,
    ) -> Result<u64> {
        let sequence = self.admit(&id, action)?;
        let revision = self.store.snapshot().map_err(store_error)?.revision;
        let proposal = self.proposal(
            format!("commit-{id}"),
            Observation::Admission(sequence),
            revision,
            before,
            after,
        )?;
        self.store
            .commit_turn_action(&proposal)
            .map_err(store_error)
    }
    pub(crate) fn intent(&mut self, effect: &str) -> Result<(u64, EffectIntent)> {
        let revision = self
            .store
            .intent_revision(effect)
            .map_err(store_error)?
            .ok_or("the pending step has no committed intent")?;
        let intent = self
            .store
            .intents(revision)
            .map_err(store_error)?
            .into_iter()
            .find(|intent| intent.id == effect)
            .ok_or("the pending step has no committed intent")?;
        Ok((revision, intent))
    }
    pub(crate) fn stage_workspace(
        &mut self,
        id: String,
        tree: Tree,
        files: Vec<File>,
    ) -> Result<String> {
        let stage = Stage {
            id,
            scope: SCOPE.into(),
            owner_epoch: self.store.epoch(),
        };
        self.store.begin_stage(&stage).map_err(store_error)?;
        for (_, bytes, _) in &files {
            self.store.put_object(&stage, bytes).map_err(store_error)?;
        }
        let tree = planned_tree(tree, &files);
        self.store
            .stage_tree(&stage, &tree)
            .map_err(|_| BAD_FILES.into())
    }
    pub(crate) fn ensure_idle(&mut self) -> Result<()> {
        let snapshot = self.store.snapshot().map_err(store_error)?;
        if snapshot.revision == 0 {
            return Ok(());
        }
        let prior = crate::turn::restored(&snapshot.continuation).map_err(turn_error)?;
        match prior.control().phase() {
            Phase::Stopped(Stop::Complete) => Ok(()),
            Phase::Stopped(stop) => Err(format!(
                "the last turn stopped ({}); rewind to a target first (`spindle targets`)",
                stop_text(stop)
            )),
            _ => Err("a turn is still in progress; run `spindle continue`".into()),
        }
    }
    pub(crate) fn ensure_room(&mut self, needed: usize) -> Result<()> {
        if needed > crate::MAX_PAYLOAD {
            return Err(format!(
                "the next model call needs {} (history, its request and room for the largest reply it allows), more than the {} one step can hold, and the history cannot be compacted further; lower the effort or rewind",
                crate::human_size(needed),
                crate::human_size(crate::MAX_PAYLOAD)
            ));
        }
        let records = 4 + crate::transition::MAX_EFFECTS as i64;
        for (name, used, cap) in self.store.capacity().map_err(store_error)? {
            if matches!(name, "revisions" | "admissions") && used + records > cap {
                return Err(format!(
                    "this session has used {used} of its {cap} {name}, so the next model step could not be saved; nothing was sent. Start a new thread to keep working"
                ));
            }
        }
        Ok(())
    }
    pub(crate) fn drop_queued(&mut self) -> Result<()> {
        let report = self.store.reconciliation().map_err(store_error)?;
        for (scope, sequence) in report.queued {
            if scope == SCOPE {
                self.store
                    .drop_admission(sequence, &format!("drop-{sequence}"))
                    .map_err(store_error)?;
            }
        }
        Ok(())
    }
    fn rewind_to(&mut self, target: u64, _same_work: bool) -> Result<()> {
        self.drop_queued()?;
        let targets = self.store.rewind_targets(SCOPE).map_err(|error| {
            format!(
                "cannot rewind now: {}",
                match error {
                    Error::Conflict =>
                        "a command is still unresolved; run `spindle continue` first".into(),
                    other => store_error(other),
                }
            )
        })?;
        let Some(chosen) = targets.iter().find(|item| item.revision == target) else {
            return Err(format!(
                "revision {target} is not a rewind target; see `spindle targets`"
            ));
        };
        let kind = chosen.kind;
        let head = self.store.snapshot().map_err(store_error)?.revision;
        let lineage = self.store.lineage().map_err(store_error)?;
        let cut = self
            .store
            .answered_app_calls(SCOPE, target)
            .map_err(store_error)?
            .iter()
            .map(|id| self.store.call_id(id).map_err(store_error))
            .collect::<Result<Vec<_>>>()?;
        let state_before = self.store.app_state(SCOPE, head).map_err(store_error)?;
        let command = format!("rw-{head}");
        self.store
            .begin_rewind(SCOPE, &command, target, head, lineage)
            .map_err(store_error)?;
        self.store
            .finish_rewind(SCOPE, &command)
            .map_err(store_error)?;
        self.store
            .resume_rewind(SCOPE, &command)
            .map_err(store_error)?;
        let report = self.store.reconciliation().map_err(store_error)?;
        let revision = self.store.snapshot().map_err(store_error)?.revision;
        self.emit(Event::Rewound {
            target,
            revision,
            kind,
            abandoned: report.abandoned_attempts.len(),
        });
        if !cut.is_empty() {
            self.emit(Event::AppCallsRewound { calls: &cut });
        }
        let state_after = self.store.app_state(SCOPE, revision).map_err(store_error)?;
        self.emit_state(&state_before, &state_after, revision);
        Ok(())
    }

    fn emit(&self, event: Event<'_>) {
        self.observer.event(event);
    }
    fn settings(&self) -> Result<Settings> {
        let bytes = self
            .store
            .metadata("settings")
            .map_err(store_error)?
            .ok_or("session settings are missing")?;
        Settings::from_json(
            &serde_json::from_slice(&bytes).map_err(|_| "stored settings are not JSON")?,
        )
    }
    fn learned(&self, provider: &str, model: &str) -> Result<Option<u32>> {
        let bytes = self.store.metadata("output_limits").map_err(store_error)?;
        let value: Value = bytes
            .as_deref()
            .and_then(|bytes| serde_json::from_slice(bytes).ok())
            .unwrap_or_default();
        let key = self.env.limit_key(provider, model)?;
        Ok(value[key].as_u64().and_then(|v| u32::try_from(v).ok()))
    }
    fn session_route(&self, provider: &str, model: Option<&str>) -> Result<Route> {
        let settings = self.settings()?;
        let model = model.unwrap_or(&settings.model);
        self.env.route(
            provider,
            Some(model),
            settings.effort,
            self.learned(provider, model)?,
        )
    }
    fn request(
        &self,
        state: &TurnContinuation,
        config: &Settings,
    ) -> Result<PreparedRequest<Vec<u8>>> {
        self.env.request(
            state,
            config,
            self.learned(&config.provider, &config.model)?,
        )
    }
    fn remember_limit(&mut self, route: &Route, limit: u32) -> Result<()> {
        let bytes = self.store.metadata("output_limits").map_err(store_error)?;
        let mut value: serde_json::Map<String, Value> = bytes
            .as_deref()
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or_default();
        let key = self.env.limit_key(&route.provider, &route.model)?;
        let prior = value
            .get(&key)
            .and_then(Value::as_u64)
            .unwrap_or(u64::from(limit));
        value.insert(key, Value::from(prior.min(u64::from(limit))));
        self.store
            .set_metadata(
                "output_limits",
                &serde_json::to_vec(&value).map_err(|e| e.to_string())?,
            )
            .map_err(store_error)
    }
    fn check_budget(&self, request: &PreparedRequest<Vec<u8>>) -> Result<()> {
        match self.env.budget_refusal(self.store, request)? {
            Some(why) => Err(why),
            None => Ok(()),
        }
    }
    fn note_cost(&self, counted: Counted<'_>) {
        if let Some(cost) = self.env.counted(self.store, counted) {
            self.emit(Event::Cost {
                usd: cost.usd,
                turn: cost.turn,
                total: cost.total,
                billing: &cost.billing,
            });
        }
    }
}

fn display_of(content: &str) -> Option<Value> {
    let value: Value =
        serde_json::from_str(content.lines().last()?.strip_prefix("display: ")?).ok()?;
    value
        .as_array()?
        .iter()
        .all(|item| item["path"].is_string() && item["kind"].is_string())
        .then_some(value)
}
fn http_error(status: u16) -> String {
    match status {
        401 | 403 => "the provider rejected the credentials".into(),
        429 => "the provider rate-limited the request (HTTP 429)".into(),
        _ => format!("the provider returned HTTP {status}"),
    }
}

fn text_of(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } | Block::PartialStream { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn stop_text(stop: Stop) -> &'static str {
    match stop {
        Stop::Complete => "done",
        Stop::Iterations => "stopped: step limit reached",
        Stop::Budget => "stopped: budget reached",
        Stop::ToolPolicy => "stopped by tool policy",
        Stop::ContentFilter => "stopped by the provider's content filter",
        Stop::RecoveryBudget => "stopped: too many incomplete responses",
        Stop::Cancelled => "cancelled",
    }
}

pub use crate::protocol::base64_decode;
