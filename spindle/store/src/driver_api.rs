//! Host contracts and owned effect data for the portable turn driver.
use super::*;
use std::sync::Arc;

/// Immutable route facts; request shaping is supplied by the host's wire layer.
#[derive(Clone)]
pub struct Route {
    pub provider: String,
    pub model: String,
    pub endpoint: String,
    pub contract: Contract,
    pub history_budget: usize,
    pub reply_reserve: usize,
    pub sent_tokens: u32,
}

#[derive(Clone)]
pub struct AppDispatch {
    pub helper: [u8; 32],
    pub rerun: bool,
    pub timeout: Duration,
    pub max_output: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Rejection {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub body: String,
}

pub enum ModelFailure {
    Http(u16),
    Dropped(String),
    Failed(String),
}
impl ModelFailure {
    pub(super) fn text(self) -> String {
        match self {
            Self::Http(status) => http_error(status),
            Self::Dropped(text) | Self::Failed(text) => text,
        }
    }
}

pub enum Counted<'a> {
    Reply(&'a str),
    Script(&'a str),
    Summary(&'a str),
}
pub struct Cost {
    pub usd: f64,
    pub turn: f64,
    pub total: f64,
    pub billing: String,
}

#[derive(Clone, Debug)]
pub struct FollowUp {
    pub prompt: String,
    pub delay_ms: u64,
}
#[derive(Default)]
pub struct PackEnd {
    pub follow_up: Option<FollowUp>,
    pub notes: Vec<String>,
}

/// Synchronous host facts. Implementations must not perform effects (model sends,
/// execution, sleeps, or user waits); request-shaping facts must be repeatable for
/// the same admitted state. Native configuration discovery stays behind this seam.
pub trait Env: Send + Sync {
    fn limit_key(&self, provider: &str, model: &str) -> Result<String> {
        Ok(format!("{provider}/{}", model.to_ascii_lowercase()))
    }
    fn route(
        &self,
        provider: &str,
        model: Option<&str>,
        effort: Effort,
        learned: Option<u32>,
    ) -> Result<Route>;
    fn request(
        &self,
        state: &TurnContinuation,
        settings: &Settings,
        learned: Option<u32>,
    ) -> Result<PreparedRequest<Vec<u8>>>;
    fn refusal(&self, settings: &Settings, name: &str, input: &Value) -> Option<String>;
    fn app_dispatch(
        &self,
        settings: &Settings,
        name: &str,
        existing: Option<&Attempt>,
    ) -> Option<AppDispatch>;
    fn tasks_offered(&self, settings: &Settings) -> bool;
    fn task_place(&self, id: &str) -> String;
    fn budget_refusal(
        &self,
        store: &Store,
        request: &PreparedRequest<Vec<u8>>,
    ) -> Result<Option<String>>;
    fn counted(&self, store: &Store, counted: Counted<'_>) -> Option<Cost>;
    fn pack_active(&self, _store: &mut Store) -> Result<bool> {
        Ok(false)
    }
    fn pack_round_steer(
        &self,
        _store: &mut Store,
        _state: &TurnContinuation,
    ) -> Result<Option<String>> {
        Ok(None)
    }
    fn cancel_follow_up(&self, _store: &mut Store, _revision: u64, _execution: &str) -> Result<()> {
        Ok(())
    }
}

/// The driver only emits semantic events; streaming previews are host-owned.
pub trait Observer: Send + Sync {
    fn event(&self, event: Event<'_>);
}

/// A driver owns transient policy state, never a connection, transport or runner.
/// Every call borrows the caller's sole authority connection.
pub struct Driver {
    pub(crate) compact_next: bool,
    pub(crate) until_unknown: bool,
    retries: u32,
    env: Arc<dyn Env>,
    inbox: Arc<dyn Inbox>,
    observer: Arc<dyn Observer>,
}

pub(crate) struct Step<'a> {
    pub(crate) store: &'a mut Store,
    pub(crate) driver: &'a mut Driver,
    pub(crate) env: Arc<dyn Env>,
    pub(crate) inbox: Arc<dyn Inbox>,
    pub(crate) observer: Arc<dyn Observer>,
}

impl Driver {
    pub fn new(env: Arc<dyn Env>, inbox: Arc<dyn Inbox>, observer: Arc<dyn Observer>) -> Self {
        Self {
            compact_next: false,
            until_unknown: false,
            retries: 0,
            env,
            inbox,
            observer,
        }
    }
    pub(crate) fn step<'a>(&'a mut self, store: &'a mut Store) -> Step<'a> {
        let (env, inbox, observer) = (self.env.clone(), self.inbox.clone(), self.observer.clone());
        Step {
            store,
            driver: self,
            env,
            inbox,
            observer,
        }
    }
    /// Rebinds the host's queues after replacing a native store for a first-request retry.
    pub fn set_inbox(&mut self, inbox: Arc<dyn Inbox>) {
        self.inbox = inbox;
    }
    /// Reserves an app call before asking the app. Receipt reuse never asks it again.
    pub fn reserve_app(
        &mut self,
        store: &mut Store,
        need: &AppNeed,
    ) -> crate::Result<crate::broker::AppReservation> {
        self.step(store).app_need(need)
    }
    /// Retains an app answer before delivering its outcome for adoption.
    pub fn record_app(
        &mut self,
        store: &mut Store,
        need: &AppNeed,
        answer: AppAnswer,
    ) -> crate::Result<DispatchOutcome> {
        self.step(store).record_app_answer(need, answer)
    }
    pub fn resume_until_unknown(&mut self, store: &mut Store) -> Result<Need> {
        self.until_unknown = true;
        self.resume_inner(store)
    }

    /// Continues the committed head, including a legacy step limit or pack follow-up.
    pub fn resume(&mut self, store: &mut Store) -> Result<Need> {
        self.until_unknown = false;
        self.resume_inner(store)
    }

    fn resume_inner(&mut self, store: &mut Store) -> Result<Need> {
        self.retries = 0;
        let mut step = self.step(store);
        let (revision, state) = step.head()?;
        if state.control().phase() == Phase::Stopped(Stop::Complete)
            && let Some(need) = step.task_report_need(revision, &state)?
        {
            return Ok(need);
        }
        match state.control().phase() {
            Phase::Stopped(Stop::Iterations) => {
                let after = state.extend_steps().map_err(turn_error)?;
                step.commit_action(
                    format!("extend-r{revision}"),
                    &Action::Extend,
                    &state,
                    &after,
                )?;
                step.emit(Event::Extended { steps: 24 });
                Ok(Need::Continue)
            }
            Phase::Stopped(stop) => {
                if stop == Stop::Complete
                    && let Some(idle) = step.pack_idle_need(revision)?
                {
                    return Ok(Need::PackIdle(idle));
                }
                step.emit(Event::NothingToContinue);
                Ok(Need::Stopped(stop))
            }
            _ => step.next_need(revision, &state),
        }
    }
    /// Retries only an unresolved model request. Replacing a first store is host I/O.
    pub fn retry(&mut self, store: &mut Store) -> Result<Retry> {
        self.retries = 0;
        let mut step = self.step(store);
        let (_, state) = step.head()?;
        let pending = state.pending_effects().map_err(turn_error)?;
        let model = pending.iter().find_map(|effect| match effect {
            Effect::Model { id, .. } => Some(id),
            _ => None,
        });
        let attempt = model
            .map(|id| step.store.attempt_for(id).map_err(store_error))
            .transpose()?
            .flatten();
        let unresolved = match &attempt {
            Some(attempt) => step
                .store
                .receipt(&attempt.id)
                .map_err(store_error)?
                .is_none(),
            None => false,
        };
        if !unresolved {
            return Err(match (model, attempt.is_some()) {
                (Some(_), true) => {
                    "the model response is already recorded; `spindle continue` uses it"
                }
                (Some(_), false) => "the model request was never sent; `spindle continue` sends it",
                (None, _) => "no model request is waiting; `spindle continue` resumes the run",
            }
            .into());
        }
        if step
            .store
            .rewind_targets(SCOPE)
            .map_err(store_error)?
            .is_empty()
        {
            let prompt = state
                .transcript()
                .map_err(turn_error)?
                .messages
                .iter()
                .rev()
                .find(|message| is_prompt(message))
                .map(|message| text_of(&message.content))
                .ok_or("the interrupted prompt could not be found")?;
            return Ok(Retry::FreshStore { prompt });
        }
        step.retry_in_place(None).map(Retry::InPlace)
    }
    /// Emits the committed stop after the host has applied its between-turn settings.
    pub fn report_stop(&mut self, store: &mut Store) -> Result<()> {
        let mut step = self.step(store);
        let (revision, state) = step.head()?;
        let Phase::Stopped(stop) = state.control().phase() else {
            return Ok(());
        };
        let files = match step.store.snapshot().map_err(store_error)?.workspace {
            Some(root) => step
                .store
                .tree_map(SCOPE, &root)
                .map_err(store_error)?
                .files
                .len(),
            None => 0,
        };
        step.emit(Event::Stopped {
            reason: stop_text(stop),
            steps: state.control().iterations(),
            revision,
            files,
            complete: stop == Stop::Complete,
        });
        Ok(())
    }
    /// Resets transient retry policy for a new host-driven run, not nested restarts.
    pub fn begin_run(&mut self) {
        self.until_unknown = false;
        self.retries = 0;
    }
    /// Retains a state patch at a safe boundary.
    pub fn set_state(&mut self, store: &mut Store, patch: &Value) -> Result<u64> {
        if store.snapshot().map_err(store_error)?.revision == 0 {
            return Err(
                "the session has no turn yet; send its first state with the first prompt".into(),
            );
        }
        self.step(store).commit_state(patch)
    }
    /// Decides and commits the next local transition, or returns an owned host need.
    pub fn next(&mut self, store: &mut Store) -> Result<Need> {
        let mut step = self.step(store);
        let (revision, state) = step.head()?;
        step.next_need(revision, &state)
    }
    /// Starts a turn; a summary may be requested before its first step commits.
    pub fn start(
        &mut self,
        store: &mut Store,
        prompt: String,
        start: Start,
        state: Option<StartState>,
    ) -> Result<Need> {
        let mut step = self.step(store);
        let config = step.settings()?;
        let route = step.session_route(&config.provider, Some(&config.model))?;
        let budget = route.history_budget;
        step.start_need(prompt, start, state, config, route, budget)
    }
    /// Delivers an answer only after the host has emitted the need's preview events
    /// and retained its receipt. The returned need may continue nested work.
    pub fn deliver(&mut self, store: &mut Store, answer: Answer) -> Result<Need> {
        let mut retries = self.retries;
        let mut step = self.step(store);
        match answer {
            Answer::Model { need, sent } => {
                let result = step.model_delivered(*need, sent, &mut retries);
                self.retries = retries;
                result
            }
            Answer::Ran { need, ran } => {
                step.tool_ran(*need, *ran)?;
                Ok(Need::Continue)
            }
            Answer::App {
                need,
                outcome,
                waiting,
            } => {
                step.app_answered(*need, outcome, waiting)?;
                Ok(Need::Continue)
            }
            Answer::Summary { need, summary } => {
                let (revision, state) = step.head()?;
                step.summary_done(revision, &state, *need, summary)
            }
            Answer::Waited(backoff) => step.after_backoff(backoff),
            Answer::Pack { idle, end } => Ok(match step.pack_idle_done(end) {
                Some(follow) => Need::FollowUp { idle, follow },
                None => Need::Stopped(Stop::Complete),
            }),
            Answer::FollowUp {
                idle,
                follow,
                waited,
            } => step.follow_up_done(idle, follow, waited),
            Answer::Stopped(stop) => {
                let (revision, _) = step.head()?;
                step.stopped_next(revision, stop)
            }
        }
    }
}

pub enum Retry {
    InPlace(Need),
    FreshStore { prompt: String },
}

pub enum Answer {
    Model {
        need: Box<ModelNeed>,
        sent: ModelSent,
    },
    Ran {
        need: Box<RunCall>,
        ran: Box<ToolRan>,
    },
    App {
        need: Box<AppNeed>,
        outcome: crate::Result<DispatchOutcome>,
        waiting: Option<String>,
    },
    Summary {
        need: Box<SummaryNeed>,
        summary: Option<String>,
    },
    Waited(Backoff),
    Pack {
        idle: PackIdle,
        end: PackEnd,
    },
    FollowUp {
        idle: PackIdle,
        follow: FollowUp,
        waited: bool,
    },
    Stopped(Stop),
}
