//! The embedded host's driver contracts. They hold plain data only (the
//! app's provider catalog, the session's place in a task tree, queued input and the
//! events the app drains), so the driver's `Send + Sync` traits stay as they are.
use crate::Store;
use crate::driver::{self, AppDispatch, Effort, Event, Settings};
use crate::shape::{self, RequestShape};
use crate::view::ReadView;
use dext_core::model::PreparedRequest;
use dext_provider_catalog::catalog::{self as provider, ProviderCatalog};
use dext_transcript::turn::TurnContinuation;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

pub type Result<T> = std::result::Result<T, String>;

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The providers an app offers its sessions: Dext's catalog format
/// (`~/.dext/providers.json`), checked as Dext checks it. Holds no credentials.
pub struct Catalog(ProviderCatalog);

impl Catalog {
    /// Dext's built-in providers.
    pub fn builtin() -> Self {
        Self(provider::default_provider_catalog())
    }

    /// A catalog in Dext's format (its custom providers keep their order).
    pub fn from_json(value: &Value) -> Result<Self> {
        let catalog: ProviderCatalog = serde_json::from_value(value.clone())
            .map_err(|error| format!("the provider catalog does not read as one: {error}"))?;
        provider::normalize_provider_catalog(catalog)
            .map(Self)
            .map_err(|error| format!("the provider catalog was refused: {error:#}"))
    }

    /// The catalog itself (prices included).
    pub(crate) fn provider_catalog(&self) -> &ProviderCatalog {
        &self.0
    }

    /// The route `provider` gives for `model` (its default model when None).
    pub(crate) fn route(&self, provider: &str, model: Option<&str>) -> Result<shape::Route> {
        let profile = provider::find_provider_profile(&self.0, provider).ok_or_else(|| {
            let known: Vec<&str> = self.0.providers.iter().map(|p| p.id.as_str()).collect();
            format!("unknown provider `{provider}`; known: {}", known.join(", "))
        })?;
        shape::profile_route(profile, model)
    }

    /// The whole catalog in Dext's format (normalized as it is used): what a relay
    /// or page reads endpoints and the active provider from. It holds no credentials.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(&self.0).unwrap_or(Value::Null)
    }

    /// The catalog's models that accept images, as `provider/model` (for a refusal to name).
    pub(crate) fn image_models(&self) -> Vec<String> {
        let mut found = vec![];
        for profile in &self.0.providers {
            let mut models = profile.models.clone();
            if !models.contains(&profile.default_model) {
                models.push(profile.default_model.clone());
            }
            for model in models {
                if provider::resolve_model_spec(profile, &model).image_input {
                    found.push(format!("{}/{model}", profile.id));
                }
            }
        }
        found
    }

    /// Each provider with its default model, as `providers` lists them.
    pub fn providers_json(&self) -> Value {
        Value::Array(
            self.0
                .providers
                .iter()
                .map(|profile| {
                    serde_json::json!({
                        "id": profile.id,
                        "name": profile.display_name,
                        "model": provider::normalize_provider_model_value(profile, &profile.default_model),
                        "contract": provider::request_contract_for_profile(profile).as_str(),
                    })
                })
                .collect(),
        )
    }
}

/// Repeatable host facts for one session: request shaping from the app's catalog.
pub(crate) struct Facts {
    pub catalog: std::sync::Arc<Catalog>,
    /// How many sessions up started this one (0: no task).
    pub depth: usize,
    /// The session's spend ledger while this host has its store open.
    pub spend: Mutex<crate::spend::HeldLedger>,
    /// The session's budget (`max_usd`) as its settings last said (the host sets it
    /// when it opens them and when they change).
    pub budget: Mutex<Option<f64>>,
    /// What the sessions of this one's task tree spent, as each last counted it, with
    /// the worst case of a request each has sent since (`pool`: a task's budget is its
    /// parent's, so every check counts them all), and this session's place in it: its
    /// label (`dir`, `dir#key`, ...), the same whichever host opens it.
    pub pool: Mutex<Pool>,
    pub member: Mutex<String>,
}

/// Spend of a task tree's sessions while their hosts are open, by session label.
pub(crate) type Pool = Arc<Mutex<BTreeMap<String, f64>>>;

impl Facts {
    /// The session's spend from its records (`save`: keep the ledger in the store now).
    pub(crate) fn spent(&self, store: &Store, save: bool) -> Result<crate::spend::UsageReport> {
        let report = lock(&self.spend).report(
            &mut ReadView::from_store(store),
            Some(&self.catalog.0),
            None,
            save,
        )?;
        self.publish(report.budget_spent());
        Ok(report)
    }

    /// This session's spend as the rest of its task tree counts it.
    fn publish(&self, usd: f64) {
        let member = lock(&self.member).clone();
        lock(&lock(&self.pool)).insert(member, usd);
    }

    /// What the other sessions of this task tree spent, as they last counted it.
    pub(crate) fn others_spent(&self) -> f64 {
        let member = lock(&self.member).clone();
        let pool = lock(&self.pool).clone();
        let spent = lock(&pool);
        spent
            .iter()
            .filter(|(other, _)| **other != member)
            .map(|(_, usd)| usd)
            .sum()
    }

    /// The catalog (prices included).
    pub(crate) fn catalog_ref(&self) -> &ProviderCatalog {
        &self.catalog.0
    }

    /// Keeps `calls` (a set-aside store's requests of unknown outcome, `aside` naming
    /// it) counted in `store`'s ledger.
    pub(crate) fn keep_aside(
        &self,
        store: &Store,
        aside: &str,
        calls: Vec<crate::spend::Call>,
    ) -> Result<()> {
        if calls.is_empty() {
            return Ok(());
        }
        lock(&self.spend).aside(&mut ReadView::from_store(store), aside, calls)
    }

    /// Keeps what was counted and not kept yet (at a turn's end).
    pub(crate) fn keep_spent(&self, store: &Store) -> Result<()> {
        lock(&self.spend).keep(&mut ReadView::from_store(store))
    }
}

impl Facts {
    fn shape_route(
        &self,
        provider: &str,
        model: Option<&str>,
        learned: Option<u32>,
    ) -> Result<shape::Route> {
        let mut route = self.catalog.route(provider, model)?;
        route.learned_limit = learned;
        Ok(route)
    }
}

impl driver::Env for Facts {
    fn limit_key(&self, provider: &str, model: &str) -> Result<String> {
        let route = self.catalog.route(provider, Some(model))?;
        Ok(format!(
            "{}/{}",
            route.profile.id,
            provider::normalize_provider_model_value(&route.profile, &route.model)
                .to_ascii_lowercase()
        ))
    }
    fn route(
        &self,
        provider: &str,
        model: Option<&str>,
        effort: Effort,
        learned: Option<u32>,
    ) -> Result<driver::Route> {
        let route = self.shape_route(provider, model, learned)?;
        let reasoning = shape::reasoning(&route, effort)?;
        Ok(driver::Route {
            provider: route.profile.id.clone(),
            model: route.model.clone(),
            endpoint: route.endpoint.clone(),
            contract: route.contract.into(),
            history_budget: shape::history_budget(&route, effort)?,
            reply_reserve: shape::reply_reserve(&route, effort)?,
            sent_tokens: reasoning.answer + reasoning.allowance,
        })
    }
    fn request(
        &self,
        state: &TurnContinuation,
        config: &Settings,
        learned: Option<u32>,
    ) -> Result<PreparedRequest<Vec<u8>>> {
        let route = self.shape_route(&config.provider, Some(&config.model), learned)?;
        shape::prepared_request(
            state,
            &RequestShape {
                config: config.clone(),
                route,
                guidance: None,
                tasks_offered: self.tasks_offered(config),
                subscription: false,
                identity: None,
            },
        )
    }
    fn refusal(&self, settings: &Settings, name: &str, input: &Value) -> Option<String> {
        shape::offered_refusal(settings, self.tasks_offered(settings), name, input)
    }
    fn app_dispatch(
        &self,
        settings: &Settings,
        name: &str,
        existing: Option<&crate::completion::Attempt>,
    ) -> Option<AppDispatch> {
        shape::app_dispatch(settings, name, existing).map(|app| AppDispatch {
            helper: app.helper,
            rerun: app.rerun,
            timeout: app.timeout,
            max_output: app.max_output,
        })
    }
    fn tasks_offered(&self, settings: &Settings) -> bool {
        settings.tasks && self.depth < settings.task_policy.max_depth
    }
    fn task_place(&self, id: &str) -> String {
        format!("task {}", super::host::task_key(id))
    }
    /// The session's budget (`max_usd`), as the native host keeps one: what its records
    /// count spent (replies, compaction summaries, scripts' nested calls), with what the
    /// rest of its task tree spent while open here and the request's worst case, must
    /// fit.
    fn budget_refusal(
        &self,
        store: &Store,
        request: &PreparedRequest<Vec<u8>>,
    ) -> Result<Option<String>> {
        let Some(max) = *lock(&self.budget) else {
            return Ok(None);
        };
        let report = self.spent(store, false)?;
        let refused = crate::spend::refusal(
            Some(&self.catalog.0),
            max,
            request,
            &report,
            self.others_spent(),
        );
        // Sent: until it is counted, its worst case counts for the rest of the tree.
        if refused.is_none() {
            let worst = crate::spend::prepared_worst(Some(&self.catalog.0), request);
            self.publish(report.budget_spent() + worst.unwrap_or(0.0));
        }
        Ok(refused)
    }
    /// What a call just counted cost (its `cost` event), from the same ledger. Counted
    /// as the turn goes only under a budget: without one nothing is counted until the
    /// app asks (`usage`), so a session costs no counting it does not use.
    fn counted(&self, store: &Store, counted: driver::Counted<'_>) -> Option<driver::Cost> {
        (*lock(&self.budget))?;
        let report = self.spent(store, false).ok()?;
        crate::spend::counted_cost(&report, counted)
    }
}

/// Input for the running turn: stop, steering messages, "read them now", and state
/// changes, set by the app between the turn's steps.
#[derive(Default)]
pub(crate) struct Queue {
    pub interrupt: AtomicBool,
    pub hurry: AtomicBool,
    pub steering: Mutex<Vec<String>>,
    pub state: Mutex<Vec<Value>>,
}

impl driver::Inbox for Queue {
    fn next_steer(&self) -> Option<String> {
        let mut queue = lock(&self.steering);
        loop {
            if queue.is_empty() {
                self.hurry.store(false, Ordering::SeqCst);
                return None;
            }
            let text = queue.remove(0);
            if !text.trim().is_empty() && text.len() <= crate::driver::MAX_STEER {
                return Some(text);
            }
        }
    }
    fn take_state(&self) -> Vec<Value> {
        std::mem::take(&mut *lock(&self.state))
    }
    fn return_steering(&self, texts: Vec<String>) {
        lock(&self.steering).splice(0..0, texts);
    }
    fn take_steering(&self) -> Vec<String> {
        self.hurry.store(false, Ordering::SeqCst);
        std::mem::take(&mut *lock(&self.steering))
    }
    fn interrupted(&self) -> bool {
        self.interrupt.load(Ordering::SeqCst)
    }
}

/// The session's events in their stable JSON form, kept for the app to drain, with
/// the last stop and rewind kept for the operation's result. A task's events go to
/// its parent instead (`relay`), as the native host relays them: each as a `task`
/// event (a tool call also as a `nested` line), and its app calls as the parent's own,
/// named `tasks/<task>/<call>`.
#[derive(Default)]
pub(crate) struct Events {
    pub queue: Mutex<Vec<Value>>,
    pub stopped: Mutex<Option<Value>>,
    pub rewound: Mutex<Option<Value>>,
    pub relay: Option<(std::sync::Arc<Events>, String)>,
}

impl Events {
    pub fn push(&self, value: Value) {
        match value["type"].as_str() {
            Some("stopped") => *lock(&self.stopped) = Some(value.clone()),
            Some("rewound") => *lock(&self.rewound) = Some(value.clone()),
            _ => {}
        }
        let Some((parent, task)) = &self.relay else {
            lock(&self.queue).push(value);
            return;
        };
        match value["type"].as_str() {
            Some("app_tool" | "app_tool_done") => {
                let mut asked = value.clone();
                asked["call"] = Value::String(format!(
                    "tasks/{task}/{}",
                    value["call"].as_str().unwrap_or_default()
                ));
                parent.push(asked);
            }
            kind => {
                let name = value["name"].as_str().map(String::from);
                parent.push(serde_json::json!({"type": "task", "task": task, "event": value}));
                if let (Some("tool_call"), Some(name)) = (kind, name) {
                    parent.push(serde_json::json!({
                        "type": "nested",
                        "label": format!("task {task}: {name}"),
                    }));
                }
            }
        }
    }
    pub fn take(&self) -> Vec<Value> {
        std::mem::take(&mut *lock(&self.queue))
    }
}

impl driver::Observer for Events {
    fn event(&self, event: Event<'_>) {
        self.push(event.to_json());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(pool: &Pool, label: &str) -> Facts {
        Facts {
            catalog: Arc::new(Catalog::builtin()),
            depth: 0,
            spend: Default::default(),
            budget: Default::default(),
            pool: Mutex::new(pool.clone()),
            member: Mutex::new(label.into()),
        }
    }

    #[test]
    fn a_task_tree_counts_every_members_spend_against_one_budget() {
        let pool = Pool::default();
        let (parent, task, other) = (facts(&pool, "d"), facts(&pool, "d#a"), facts(&pool, "d#b"));
        parent.publish(0.5);
        task.publish(0.25);
        other.publish(0.125);
        assert_eq!(parent.others_spent(), 0.375);
        assert_eq!(task.others_spent(), 0.625);
        // The same task opened again by another host takes its own place, not a second.
        let reopened = facts(&pool, "d#a");
        reopened.publish(0.25);
        assert_eq!(parent.others_spent(), 0.375);
        // A session of another tree counts none of it.
        assert_eq!(facts(&Pool::default(), "e").others_spent(), 0.0);
    }
}
