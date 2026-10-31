//! Model spend. Every billed call is counted once, from durable records only: recorded
//! replies (including ones a rewind abandoned), model-written compaction summaries (each
//! journaled in its own stage) and nested code-mode `model()` calls (their child
//! journal, or the spend record garbage collection leaves). A call is priced when it is
//! first counted (record time) and `usage.json` keeps that price, so a later catalog
//! change never rewrites history; a deleted `usage.json` is rebuilt at current prices
//! and without dates. Requests whose outcome is unknown and provider refusals are
//! counted beside the billed calls, never priced. Optional dollar budgets refuse a
//! request before it is sent when its worst case would not fit beside what was spent
//! and the worst case of every request whose outcome is unknown.
use super::*;
use crate::spend::*;
use crate::view::{ReadView, View};
use dext_provider_catalog::provider::ProviderCatalog;
use dext_transcript::Usage;
use std::collections::BTreeMap;

impl Ledger {
    fn load(path: &Path) -> Self {
        let value = path
            .parent()
            .and_then(|dir| crate::meta::read(dir, "usage").ok().flatten())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .or_else(|| read_json(path).ok());
        Self::from_value(value)
    }

    fn save(&self, path: &Path) -> Result<()> {
        let bytes = self.encoded()?;
        if let Some(dir) = path.parent() {
            crate::meta::update(dir, "usage", |_| Ok(bytes.clone())).map_err(store_error)?;
        }
        save_private(path, &bytes)
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The session's spend, computed from its store read-only (no owner lock): calls
/// recorded since the last look are priced now and kept in `usage.json`.
pub fn usage_report(dir: &Path) -> Result<UsageReport> {
    report_with(dir, provider::load_provider_catalog().ok().as_ref())
}

/// `usage_report` with the catalog at hand (a host reads it once for all its threads).
fn report_with(dir: &Path, catalog: Option<&ProviderCatalog>) -> Result<UsageReport> {
    let mut view = View::open(&dir.join("store")).map_err(store_error)?;
    report_on_view(dir, catalog, &mut view)
}

fn report_on_view(
    dir: &Path,
    catalog: Option<&ProviderCatalog>,
    view: &mut ReadView<'_>,
) -> Result<UsageReport> {
    let path = dir.join("usage.json");
    let _writer = ledger_lock(dir);
    let stored = view.metadata("usage").map_err(store_error)?;
    let mut ledger = match stored {
        Some(bytes) => Ledger::from_value(serde_json::from_slice(&bytes).ok()),
        None => Ledger::from_value(read_json(&path).ok()),
    };
    let now = unix_now();
    let mut live = Live::default();
    let changed = view
        .read(|view| ledger.update(view, catalog, now, &mut live))
        .map_err(store_error)?;
    if changed || ledger.undated {
        ledger.undated = false;
        let bytes = ledger.encoded()?;
        view.retain_metadata("usage", &bytes).map_err(store_error)?;
        save_private(&path, &bytes)?;
    }
    drop(_writer);
    let mut report = ledger.report(live);
    // The session's tasks spend from its budget: their totals count as its own. One
    // whose spend cannot be read fails the report (a budget must not take it for none);
    // one that never got a store spent nothing.
    for child in crate::agent::tasks::task_dirs(dir)? {
        if !child.join("store").is_dir() {
            continue;
        }
        let spent = report_with(&child, catalog)?;
        report.tasks = report.tasks.plus(&spent.total);
        report.total = report.total.plus(&spent.total);
        report.uncertain_usd += spent.uncertain_usd;
    }
    Ok(report)
}

/// Holds `usage.json` for one read-extend-write: this process's lock, and `usage.lock`
/// beside it for other processes (the CLI and `spindle serve` may count one session
/// at once). Where that file cannot be opened, only this process is serialized.
struct LedgerLock {
    _file: Option<std::fs::File>,
    _process: std::sync::MutexGuard<'static, ()>,
}

fn ledger_lock(dir: &Path) -> LedgerLock {
    let process = usage_lock();
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(dir.join("usage.lock"))
        .ok()
        .filter(|file| file.lock().is_ok());
    LedgerLock {
        _file: file,
        _process: process,
    }
}

/// Starts a new session's ledger, so its calls are dated as they are recorded (a
/// missing ledger means an older or lost one: what it then counts gets no date).
pub(super) fn start_ledger(dir: &Path) -> Result<()> {
    let path = dir.join("usage.json");
    let _writer = ledger_lock(dir);
    if path.exists() {
        return Ok(());
    }
    Ledger::default().save(&path)
}

/// A first request retried in a fresh store (`restart_first`): the requests the store
/// set aside (`aside`) sent without a reply stay counted, refused or unknown (at their
/// worst case), on the turn that restarts the same prompt.
pub(super) fn keep_aside(dir: &Path, aside: &Path) -> Result<()> {
    let name = aside
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("store");
    let catalog = provider::load_provider_catalog().ok();
    let mut view = View::open(aside).map_err(store_error)?;
    let requests = view
        .read(|view| {
            let mut requests = vec![];
            for (effect, status) in view.unanswered_model_attempts()? {
                let worst = match status {
                    Some(_) => 0.0,
                    None => request_worst(view, catalog.as_ref(), &effect)?,
                };
                requests.push((effect, status.is_some(), worst));
            }
            Ok(requests)
        })
        .map_err(store_error)?;
    let stored = view.metadata("usage").map_err(store_error)?;
    let path = dir.join("usage.json");
    let _writer = ledger_lock(dir);
    let mut ledger = match stored {
        Some(bytes) => Ledger::from_value(serde_json::from_slice(&bytes).ok()),
        None => Ledger::load(&path),
    };
    let now = unix_now();
    for (effect, refused, worst) in requests {
        let at = ledger.at(now);
        ledger.push(Call {
            key: format!("{ASIDE}{name}:{effect}"),
            kind: CallKind::Parent,
            turn: Some(turn_of(&effect)),
            after: None,
            provider: String::new(),
            model: String::new(),
            billing: if refused {
                Billing::Refused
            } else {
                Billing::Unknown
            },
            usage: Usage::default(),
            usd: 0.0,
            at,
            count: 1,
            worst,
        });
    }
    ledger.save(&path)
}

/// The priced total `usage.json` last recorded, without reading the store (for lists).
pub fn recorded_usd(dir: &Path) -> Option<f64> {
    let value: Value = match crate::meta::read(dir, "usage").ok()? {
        Some(bytes) => serde_json::from_slice(&bytes).ok()?,
        None => read_json(&dir.join("usage.json")).ok()?,
    };
    if value["version"].as_u64() != Some(LEDGER) {
        return None;
    }
    let mut total = 0.0;
    for call in value["calls"].as_array()? {
        if call["billing"] == "priced" {
            total += call["usd"]
                .as_f64()
                .filter(|usd| valid_usd(*usd))
                .unwrap_or(0.0);
        }
    }
    // Its tasks' spend is its own, as in `usage_report`.
    for child in crate::agent::tasks::task_dirs(dir).ok()? {
        total += recorded_usd(&child)?;
    }
    Some(total)
}

/// Why this thread's budget refuses `request` now, or None (no budget, or it fits).
fn budget_refusal(
    dir: &Path,
    max_usd: Option<f64>,
    request: &PreparedRequest<Vec<u8>>,
    report: Option<(&UsageReport, f64)>,
) -> Result<Option<String>> {
    let Some(max) = max_usd else {
        return Ok(None);
    };
    let catalog = provider::load_provider_catalog().ok();
    let read;
    let (report, reserved) = match report {
        Some(pair) => pair,
        None => {
            let reserved = super::tasks::reserved_usd(dir, None)?;
            read = report_with(dir, catalog.as_ref())?;
            (&read, reserved)
        }
    };
    Ok(refusal(catalog.as_ref(), max, request, report, reserved))
}

/// The code policy's `limits.max_usd` per script and the thread's budget, against the
/// worst case of a script's new nested model calls (`prepared`), given what the
/// script's journal already records. Without either cap nothing is checked; with one,
/// an unpriced (non-subscription) model is refused.
pub fn admit_nested_spend(
    dir: &Path,
    script_cap: Option<f64>,
    prepared: &[&Value],
    journal: &[&Value],
) -> Result<()> {
    let thread_cap = Settings::load(dir)
        .ok()
        .and_then(|settings| settings.max_usd);
    let script_cap = script_cap.filter(|cap| cap.is_finite() && *cap >= 0.0);
    if thread_cap.is_none() && script_cap.is_none() {
        return Ok(());
    }
    let catalog = provider::load_provider_catalog().ok();
    let cost = nested_worst(catalog.as_ref(), prepared).map_err(|(provider, model)| {
        unpriced_text(
            &provider,
            &model,
            thread_cap.or(script_cap).unwrap_or_default(),
        )
    })?;
    if let Some(cap) = script_cap {
        let mut children: BTreeMap<u64, Child> = BTreeMap::new();
        for record in journal {
            match record["kind"].as_str() {
                Some("reserved") => {
                    if let Some(id) = record["request"]["id"].as_u64() {
                        children.entry(id).or_default().reserved = Some((*record).clone());
                    }
                }
                Some("result") => {
                    if let Some(id) = record["id"].as_u64() {
                        children.entry(id).or_default().result = Some(record["response"].clone());
                    }
                }
                _ => {}
            }
        }
        // A child that may have been billed without saying what counts at its worst case.
        let spent: f64 = children
            .values()
            .filter_map(|child| {
                let prepared = child.prepared()?;
                let worst = || journal_worst(catalog.as_ref(), prepared);
                Some(match &child.result {
                    Some(result) => match outcome(catalog.as_ref(), prepared, result)? {
                        outcome if outcome.billing == Billing::Unknown => worst(),
                        outcome => outcome.usd,
                    },
                    None => worst(),
                })
            })
            .sum();
        if spent + cost > cap {
            return Err(format!(
                "nested model spend limit: this script has spent {} of its {} limit (code policy limits.max_usd) and the new call(s) could cost up to {}; nothing was sent",
                dollars(spent),
                dollars(cap),
                dollars(cost)
            ));
        }
    }
    if let Some(cap) = thread_cap {
        let reserved = super::tasks::reserved_usd(dir, None)?;
        let report = report_with(dir, catalog.as_ref())?;
        if report.budget_spent() + reserved + cost > cap {
            return Err(format!(
                "spending limit: this thread has spent {} of its {} budget and the new nested call(s) could cost up to {}; nothing was sent",
                spent_text(&report),
                dollars(cap),
                dollars(cost)
            ));
        }
    }
    Ok(())
}

/// Indexes of the user's prompts in a snapshot's messages (as `is_prompt` tells them).
fn prompt_indexes(messages: &Value) -> Vec<usize> {
    let Some(messages) = messages.as_array() else {
        return vec![];
    };
    // As `is_prompt`, on the JSON form (no message is copied).
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message["role"] == "user"
                && message["content"].as_array().is_some_and(|blocks| {
                    !blocks.is_empty()
                        && blocks.iter().all(|block| {
                            block["type"] == "text"
                                && block["text"].as_str().is_some_and(|text| {
                                    !text.starts_with(COMPACTED)
                                        && !text.starts_with(STEERED)
                                        && !text.starts_with("[task ")
                                })
                        })
                })
        })
        .map(|(index, _)| index)
        .collect()
}

/// Adds usage to a thread snapshot (`revision`, `forks`, `messages` already in it):
/// `usage` (totals with their parent/nested/compaction parts), `turn_costs` (each
/// prompt still in the shown history, by message index, with what its turn cost) and
/// `budget` (when one is set: limit, spent, what requests of unknown outcome may have
/// added, and why a waiting request is held back). The budget is a setting: it is
/// added even when the spend cannot be read, so a screen never shows it empty.
pub fn annotate_snapshot(dir: &Path, snapshot: &mut Value) {
    let report = usage_report(dir).ok();
    if let Some(report) = &report {
        snapshot["usage"] = report.totals_json();
        snapshot["turn_costs"] = json!(turn_costs(report, snapshot));
    }
    let Some(max) = Settings::load(dir)
        .ok()
        .and_then(|settings| settings.max_usd)
    else {
        return;
    };
    let viewed = snapshot["revision"].as_u64().unwrap_or(0);
    let head = snapshot["head"].as_u64() == Some(viewed);
    let waiting = head
        && snapshot["busy"] != true
        && snapshot["recorded"] != true
        && snapshot["rejected"].is_null()
        && snapshot["phase"].as_str() == Some("ModelWait");
    let blocked = match &report {
        Some(report) if waiting => pending_request(dir)
            .filter(|request| !report.unanswered.contains(&request.identity().effect))
            .and_then(|request| {
                budget_refusal(dir, Some(max), &request, None)
                    .ok()
                    .flatten()
            }),
        _ => None,
    };
    snapshot["budget"] = json!({"max_usd": max, "spent": report.as_ref().map(|r| r.total.usd),
        "uncertain": report.as_ref().map(|r| r.uncertain_usd), "blocked": blocked});
}

/// Each prompt of a snapshot's messages with its turn's cost. The prompts shown are
/// the newest turns of the viewed branch (compaction folds older ones away but keeps
/// the newest in order); each is matched, newest first, to the newest turn of that
/// branch started by the same prompt.
fn turn_costs(report: &UsageReport, snapshot: &Value) -> Vec<Value> {
    let viewed = snapshot["revision"].as_u64().unwrap_or(0);
    let forks: Vec<(u64, u64, u64)> = snapshot["forks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|fork| {
            Some((
                fork["revision"].as_u64()?,
                fork["target"].as_u64()?,
                fork["abandoned"].as_u64()?,
            ))
        })
        .collect();
    let ranges = branch(viewed, &forks);
    let shown: Vec<&TurnUsage> = report
        .turns
        .iter()
        .filter(|turn| on_branch(turn.revision, &ranges))
        .collect();
    let messages = &snapshot["messages"];
    let mut unmatched = shown.len();
    let mut costs = vec![];
    for index in prompt_indexes(messages).into_iter().rev() {
        let text: Vec<&str> = messages[index]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|block| block["text"].as_str())
            .collect();
        let prompt = one_line(&text.join("\n"), PROMPT_CHARS);
        // A turn whose prompt could not be read matches any.
        let Some(position) = shown[..unmatched]
            .iter()
            .rposition(|turn| turn.prompt == prompt || turn.prompt.is_empty())
        else {
            continue;
        };
        unmatched = position;
        let turn = shown[position];
        // Attempts a retry left behind (the same prompt, restarted after the turn
        // before) are shown on the prompt that replaced them.
        let since = position
            .checked_sub(1)
            .map_or(0, |before| shown[before].revision);
        let mut usage = turn.usage.clone();
        for retried in report.turns.iter().filter(|other| {
            !on_branch(other.revision, &ranges)
                && other.prompt == turn.prompt
                && other.revision > since
                && other.revision < turn.revision
        }) {
            usage = usage.plus(&retried.usage);
        }
        let mut value = usage.to_json();
        value["message"] = json!(index);
        value["turn"] = json!(turn.turn);
        value["revision"] = json!(turn.revision);
        costs.push(value);
    }
    costs.reverse();
    costs
}

/// The model request the session's head waits on, read without the owner lock.
fn pending_request(dir: &Path) -> Option<PreparedRequest<Vec<u8>>> {
    let mut view = View::open(&dir.join("store")).ok()?;
    let (_, bytes, _) = view.read(|view| view.head()).ok()?;
    let state = crate::turn::restored(&bytes).ok()?;
    state
        .pending_effects()
        .ok()?
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Model { waiting, .. } => Some(waiting.request().clone()),
            Effect::Tool { .. } => None,
        })
}

/// Spend across a host's threads: today (from local midnight, `offset` minutes east of
/// UTC), the last 30 days and all time, in total and per thread.
pub fn host_usage(threads: &[(String, String, PathBuf)], offset: i64) -> Value {
    let now = unix_now();
    let local = now as i64 + offset * 60;
    let midnight = (local - local.rem_euclid(86_400) - offset * 60).max(0) as u64;
    let month = now.saturating_sub(30 * 86_400);
    let (mut today, mut recent, mut all) = (Tally::default(), Tally::default(), Tally::default());
    let mut rows = vec![];
    let catalog = provider::load_provider_catalog().ok();
    for (id, name, dir) in threads {
        let Ok(report) = report_with(dir, catalog.as_ref()) else {
            continue;
        };
        let (day, days) = (report.since(midnight), report.since(month));
        for (sum, part) in [
            (&mut today, &day),
            (&mut recent, &days),
            (&mut all, &report.total),
        ] {
            *sum = sum.plus(part);
        }
        rows.push(json!({"id": id, "name": name, "today": day.to_json(),
            "month": days.to_json(), "all": report.total.to_json()}));
    }
    json!({"today": today.to_json(), "month": recent.to_json(), "all": all.to_json(),
        "threads": rows, "since": {"today": midnight, "month": month}})
}

impl Session {
    pub(super) fn note_cost(&self, spent: driver::Counted<'_>) {
        if let Some(cost) = owner_counted(&self.dir, &self.store, spent) {
            self.emit(Event::Cost {
                usd: cost.usd,
                turn: cost.turn,
                total: cost.total,
                billing: &cost.billing,
            });
        }
    }

    /// Saves a new budget (None: unlimited). Allowed at any point of a turn: it is
    /// checked before each request, never part of one.
    pub fn set_budget(&mut self, max_usd: Option<f64>) -> Result<()> {
        if max_usd.is_some_and(|max| !max.is_finite() || max < 0.0) {
            return Err("a budget must be a non-negative number of US dollars".into());
        }
        let mut settings = self.settings()?;
        settings.max_usd = max_usd;
        self.save_settings(&settings)
    }

    /// A compaction summary journaled like a nested call, in stage `summary-<command>`:
    /// reserved before it is sent and its reply kept, so it is counted once, reused if
    /// the compaction is committed after a crash, and never sent twice (a reservation
    /// without a reply is an unknown outcome: the digest stands in). None: no summary
    /// (the digest stands in), including when the budget cannot cover it.
    pub(super) fn journaled_summary(
        &mut self,
        command: &str,
        state: &TurnContinuation,
        route: &Route,
        prepared: &Value,
        source: &str,
    ) -> Option<Value> {
        let journal = match self.store.summary_journal(command, source)? {
            driver::Journaled::Fresh(journal) => journal,
            // Asked for once already: reuse that reply for the same history only.
            driver::Journaled::Reused(reply) => return Some(reply),
            driver::Journaled::Asked => return None,
        };
        let subscription = route.contract == RequestContract::ChatGptResponses
            || subscription_on(self.credentials.as_ref(), route);
        let body = serde_json::to_vec(&prepared["body"]).ok()?;
        let max_usd = self.settings().ok()?.max_usd;
        let catalog = provider::load_provider_catalog().ok();
        let worst = worst_case(
            catalog.as_ref(),
            &route.profile.id,
            &route.model,
            route.contract.as_str(),
            subscription,
            &body,
        );
        if let Some(max) = max_usd {
            // Read leases before spend: child usage may advance between the snapshots.
            let reserved = super::tasks::reserved_usd(&self.dir, None).ok()?;
            let mut view = ReadView::from_store(&self.store);
            let report = report_on_view(&self.dir, catalog.as_ref(), &mut view).ok()?;
            if report.budget_spent() + reserved + worst? > max {
                return None;
            }
        }
        self.store
            .summary_reserve(&journal, state, prepared, worst)
            .ok()?;
        let reply = execute_code_model_with_credentials(
            prepared,
            &self.interrupt,
            SUMMARY_TIME,
            self.credentials.as_ref(),
        );
        self.store.summary_record(&journal, &reply);
        Some(reply)
    }
}

pub(super) fn owner_counted(
    dir: &Path,
    store: &Store,
    spent: driver::Counted<'_>,
) -> Option<driver::Cost> {
    let catalog = provider::load_provider_catalog().ok();
    let mut view = ReadView::from_store(store);
    let report = report_on_view(dir, catalog.as_ref(), &mut view).ok()?;
    counted_cost(&report, spent)
}

pub(super) fn owner_budget(
    dir: &Path,
    store: &Store,
    request: &PreparedRequest<Vec<u8>>,
) -> Result<Option<String>> {
    let settings: Value = serde_json::from_slice(
        &store
            .metadata("settings")
            .map_err(store_error)?
            .ok_or("session settings missing")?,
    )
    .map_err(|_| "settings are not JSON")?;
    let settings = Settings::from_json(&settings)?;
    let max = settings.max_usd;
    let report = match max {
        None => None,
        Some(_) => {
            let reserved = super::tasks::reserved_usd(dir, None)?;
            let catalog = provider::load_provider_catalog().ok();
            let mut view = ReadView::from_store(store);
            Some((report_on_view(dir, catalog.as_ref(), &mut view)?, reserved))
        }
    };
    budget_refusal(
        dir,
        max,
        request,
        report
            .as_ref()
            .map(|(report, reserved)| (report, *reserved)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use dext_native::streaming::{ProviderStreamParser, SseFrame};
    use dext_provider_catalog::provider::ModelPricing;

    fn parse(contract: RequestContract, events: &[(&str, Value)]) -> Usage {
        let mut parser = ProviderStreamParser::new(contract, false);
        for (event, data) in events {
            parser
                .push_frame(SseFrame {
                    event: (!event.is_empty()).then(|| event.to_string()),
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        parser.finish().unwrap().usage
    }

    fn catalog() -> ProviderCatalog {
        provider::default_provider_catalog()
    }

    fn priced(provider: &str, model: &str, contract: RequestContract, usage: &Usage) -> f64 {
        match price(
            Some(&catalog()),
            provider,
            model,
            contract.as_str(),
            false,
            usage,
        ) {
            (Billing::Priced, usd) => usd,
            other => panic!("{provider}/{model} not priced: {other:?}"),
        }
    }

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn summary_cannot_spend_a_running_tasks_reserved_lease() {
        use std::sync::atomic::AtomicUsize;
        struct Probe(AtomicUsize);
        impl Credentials for Probe {
            fn route(&self, _: &str, _: Option<&str>) -> Result<Route> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err("unexpected route resolution".into())
            }
            fn auth_kind(&self, _: &Route) -> Result<RuntimeAuthKind> {
                Err("not a subscription".into())
            }
            fn live_headers(&self, _: &Route, _: &str) -> Result<reqwest::header::HeaderMap> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err("unexpected send".into())
            }
        }
        struct Quiet;
        impl Observer for Quiet {
            fn event(&self, _: Event<'_>) {}
        }
        let root =
            std::env::temp_dir().join(format!("spindle-summary-lease-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut session = Session::create(&root, Settings::from_json(&json!({"provider":"local", "model":"fixture", "compaction":"digest", "max_usd":1.0})).unwrap(), vec![], Arc::new(Quiet)).unwrap();
        session
            .start_turn("parent", Start::Import(vec![], vec![], vec![]))
            .unwrap();
        let snapshot = session.store.snapshot().unwrap();
        session
            .store
            .register_task(&crate::meta::TaskRun {
                task: "app:reserved".into(),
                origin: None,
                input: br#"{"prompt":"child"}"#.to_vec(),
                base: snapshot.workspace.unwrap(),
                revision: snapshot.revision,
                queued: true,
                state: "running".into(),
                answer: None,
                delivered: None,
                lease: Some(1.0),
            })
            .unwrap();
        let mut route = NativeCredentials.route("openai", Some("gpt-4.1")).unwrap();
        route.endpoint = "http://127.0.0.1:1/responses".into();
        let probe = Arc::new(Probe(AtomicUsize::new(0)));
        session.credentials = probe.clone();
        let state = session.head().unwrap().1;
        let result = session.journaled_summary(
            "lease-test",
            &state,
            &route,
            &json!({"provider":"openai", "model":route.model, "body":{"max_tokens":1000}}),
            "history",
        );
        assert!(result.is_none(), "{result:?}");
        assert_eq!(probe.0.load(Ordering::SeqCst), 0);
        assert!(
            ReadView::from_store(&session.store)
                .stage_records(&["summary-lease-test".into()])
                .unwrap()
                .is_empty()
        );
        crate::meta::finish_task(
            &root,
            "app:reserved",
            br#"{"prompt":"child"}"#,
            crate::meta::TaskEnd::Answered(br#"{"content":"done","failed":false}"#),
        )
        .unwrap();
        assert!(
            session
                .journaled_summary(
                    "released-test",
                    &state,
                    &route,
                    &json!({"provider":"openai", "model":route.model, "body":{"max_tokens":1000}}),
                    "history",
                )
                .is_some()
        );
        assert_eq!(probe.0.load(Ordering::SeqCst), 1);
        let request = PreparedRequest::new(
            Identity {
                effect: "budget-probe".into(),
                provider: "openai".into(),
                contract: route.contract.as_str().into(),
                endpoint: route.endpoint,
                model: route.model,
                credential_scope: "fixture".into(),
                configuration: "fixture".into(),
            },
            Encoding::Json,
            br#"{"max_tokens":1000}"#.to_vec(),
        )
        .unwrap();
        let mut ledger = Ledger::default();
        ledger.push(Call {
            key: "spent-by-child".into(),
            kind: CallKind::Parent,
            turn: None,
            after: None,
            provider: "openai".into(),
            model: "gpt-4.1".into(),
            billing: Billing::Priced,
            usage: Usage::default(),
            usd: 0.999,
            at: None,
            count: 1,
            worst: 0.0,
        });
        let child = super::tasks::task_dir(&root, "app:reserved");
        std::fs::create_dir_all(&child).unwrap();
        ledger.save(&child.join("usage.json")).unwrap();
        drop(crate::Store::open(&child.join("store")).unwrap());
        assert!(
            budget_refusal(&root, Some(1.0), &request, None)
                .unwrap()
                .is_some(),
            "a cached parent report cannot hide a child's newly recorded spend"
        );
        drop(session);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn anthropic_messages_input_excludes_cache_and_output_includes_thinking() {
        let usage = parse(
            RequestContract::AnthropicMessages,
            &[
                (
                    "message_start",
                    json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"claude-opus-5-5","usage":{"input_tokens":1000,"cache_creation_input_tokens":2000,"cache_read_input_tokens":30000,"output_tokens":1}}}),
                ),
                (
                    "content_block_start",
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
                ),
                (
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}),
                ),
                (
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":0}),
                ),
                (
                    "content_block_start",
                    json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
                ),
                (
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ok"}}),
                ),
                (
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":1}),
                ),
                (
                    "message_delta",
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":500}}),
                ),
                ("message_stop", json!({"type":"message_stop"})),
            ],
        );
        // Anthropic reports disjoint buckets; output_tokens already includes thinking.
        assert_eq!(
            (
                usage.input,
                usage.cache_create,
                usage.cache_read,
                usage.output
            ),
            (1000, 2000, 30000, 500)
        );
        // claude-opus-5-5: 4 in, 20 out, 0.2 cache read, 5 cache write per MTok.
        close(
            priced(
                "anthropic",
                "claude-opus-5-5",
                RequestContract::AnthropicMessages,
                &usage,
            ),
            (1000.0 * 4.0 + 2000.0 * 5.0 + 30000.0 * 0.2 + 500.0 * 20.0) / 1e6,
        );
        // GLM speaks the same contract: the same buckets at its own rates.
        close(
            priced("glm", "glm-5.1", RequestContract::AnthropicMessages, &usage),
            (1000.0 * 1.0 + 2000.0 * 1.25 + 30000.0 * 0.1 + 500.0 * 5.0) / 1e6,
        );
    }

    #[test]
    fn openai_chat_cached_tokens_are_subtracted_once_and_reasoning_is_output() {
        let usage = parse(
            RequestContract::OpenAiChatCompletions,
            &[
                (
                    "",
                    json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"ok"},"finish_reason":null}]}),
                ),
                (
                    "",
                    json!({"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                ),
                (
                    "",
                    json!({"id":"c","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":10000,"completion_tokens":800,"total_tokens":10800,"prompt_tokens_details":{"cached_tokens":6000},"completion_tokens_details":{"reasoning_tokens":500}}}),
                ),
            ],
        );
        // prompt_tokens includes the cached ones; completion_tokens includes reasoning.
        assert_eq!(
            (
                usage.input,
                usage.cache_read,
                usage.cache_create,
                usage.output
            ),
            (4000, 6000, 0, 800)
        );
        close(
            priced(
                "openai",
                "gpt-4.1",
                RequestContract::OpenAiChatCompletions,
                &usage,
            ),
            (4000.0 * 2.0 + 6000.0 * 0.5 + 800.0 * 8.0) / 1e6,
        );
    }

    #[test]
    fn deepseek_cache_hits_and_reasoning_are_each_charged_once() {
        let usage = parse(
            RequestContract::OpenAiChatCompletions,
            &[
                (
                    "",
                    json!({"id":"d","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}),
                ),
                (
                    "",
                    json!({"id":"d","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":9000,"completion_tokens":1200,"total_tokens":10200,"prompt_cache_hit_tokens":7000,"prompt_cache_miss_tokens":2000,"completion_tokens_details":{"reasoning_tokens":900}}}),
                ),
            ],
        );
        assert_eq!(
            (usage.input, usage.cache_read, usage.output),
            (2000, 7000, 1200)
        );
        close(
            priced(
                "deepseek",
                "deepseek-v4-pro",
                RequestContract::OpenAiChatCompletions,
                &usage,
            ),
            (2000.0 * 1.32 + 7000.0 * 0.044 + 1200.0 * 3.96) / 1e6,
        );
    }

    fn responses(contract: RequestContract, input: u64, cached: u64, output: u64) -> Usage {
        parse(
            contract,
            &[(
                "response.completed",
                json!({"type":"response.completed","response":{"id":"r","status":"completed","output":[{"type":"message","id":"m","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok","annotations":[]}]}],"usage":{"input_tokens":input,"input_tokens_details":{"cached_tokens":cached},"output_tokens":output,"output_tokens_details":{"reasoning_tokens":output / 2},"total_tokens":input + output}}}),
            )],
        )
    }

    #[test]
    fn responses_and_the_chatgpt_backend_subtract_cached_input_and_tier_long_context() {
        for contract in [
            RequestContract::OpenAiResponses,
            RequestContract::ChatGptResponses,
        ] {
            let usage = responses(contract, 50_000, 20_000, 3_000);
            assert_eq!(
                (usage.input, usage.cache_read, usage.output),
                (30_000, 20_000, 3_000),
                "{contract:?}"
            );
        }
        let short = responses(RequestContract::OpenAiResponses, 50_000, 20_000, 3_000);
        close(
            priced(
                "openai",
                "gpt-6.1-sol",
                RequestContract::OpenAiResponses,
                &short,
            ),
            (30_000.0 * 2.0 + 20_000.0 * 0.1 + 3_000.0 * 10.0) / 1e6,
        );
        // Above 272,000 input tokens (cached ones included) the tier applies.
        let long = responses(RequestContract::OpenAiResponses, 300_000, 100_000, 3_000);
        close(
            priced(
                "openai",
                "gpt-6.1-sol",
                RequestContract::OpenAiResponses,
                &long,
            ),
            (200_000.0 * 4.0 + 100_000.0 * 0.2 + 3_000.0 * 15.0) / 1e6,
        );
        // Not for models outside the Responses families Dext tiers.
        let chat = Usage {
            input: 300_000,
            output: 1_000,
            ..Usage::default()
        };
        close(
            priced(
                "openai",
                "gpt-4.1",
                RequestContract::OpenAiChatCompletions,
                &chat,
            ),
            (300_000.0 * 2.0 + 1_000.0 * 8.0) / 1e6,
        );
    }

    #[test]
    fn subscriptions_cost_nothing_and_are_told_apart_from_unpriced_models() {
        let usage = Usage {
            input: 1_000_000,
            output: 1_000_000,
            ..Usage::default()
        };
        let catalog = catalog();
        assert_eq!(
            price(
                Some(&catalog),
                "chatgpt",
                "gpt-6.1-sol",
                "chatgpt-responses",
                true,
                &usage
            ),
            (Billing::Subscription, 0.0)
        );
        assert!(subscription_request("chatgpt-responses", b"{}"));
        let oauth = br#"{"model":"claude-opus-5-5","system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=1; cch=00000;"}]}"#;
        assert!(subscription_request("anthropic-messages", oauth));
        assert!(!subscription_request(
            "anthropic-messages",
            br#"{"system":"x-anthropic-billing-header: in a prompt"}"#
        ));
        assert!(!subscription_request("openai-responses", oauth));
        assert_eq!(
            price(
                Some(&catalog),
                "openai",
                "no-such-model",
                "openai-responses",
                false,
                &usage
            ),
            (Billing::Unpriced, 0.0)
        );
        // A provider-stated cost is used as Dext uses it, except on Anthropic caching routes.
        let stated = Usage {
            cost_usd: Some(0.25),
            ..usage
        };
        assert_eq!(
            price(
                Some(&catalog),
                "openai",
                "no-such-model",
                "openai-chat-completions",
                false,
                &stated
            ),
            (Billing::Priced, 0.25)
        );
        let (billing, usd) = price(
            Some(&catalog),
            "anthropic",
            "claude-opus-5-5",
            "anthropic-messages",
            false,
            &stated,
        );
        assert_eq!(billing, Billing::Priced);
        close(usd, 24.0);
    }

    #[test]
    fn worst_case_charges_every_byte_and_the_full_output_allowance() {
        let catalog = catalog();
        let body = json!({"model":"claude-opus-5-5","max_tokens":1000,"messages":[{"role":"user","content":"x".repeat(2900)}]});
        let bytes = serde_json::to_vec(&body).unwrap();
        let tokens = worst_input(&body) as f64;
        // Every byte of the body is charged, at three ASCII bytes per token.
        assert!(tokens >= (bytes.len() as f64 / 3.0).ceil());
        let worst = worst_case(
            Some(&catalog),
            "anthropic",
            "claude-opus-5-5",
            "anthropic-messages",
            false,
            &bytes,
        )
        .unwrap();
        // A cache write (5/MTok) is dearer than fresh input (4/MTok) on this model.
        close(worst, (tokens * 5.0 + 1000.0 * 20.0) / 1e6);
        assert_eq!(
            worst_case(
                Some(&catalog),
                "chatgpt",
                "gpt-6.1-sol",
                "chatgpt-responses",
                true,
                &bytes
            ),
            Some(0.0)
        );
        assert_eq!(
            worst_case(
                Some(&catalog),
                "openai",
                "no-such-model",
                "openai-responses",
                false,
                &bytes
            ),
            None
        );
    }

    #[test]
    fn prompts_map_to_the_newest_turns_of_the_viewed_branch() {
        let messages = json!([
            {"role":"user","content":[{"type":"text","text":format!("{COMPACTED}\nsummary")}]},
            {"role":"user","content":[{"type":"text","text":"second"}]},
            {"role":"assistant","content":[{"type":"text","text":"a"}]},
            {"role":"user","content":[{"type":"text","text":format!("{STEERED} more")}]},
            {"role":"user","content":[{"type":"text","text":"third"}]},
        ]);
        assert_eq!(prompt_indexes(&messages), vec![1, 4]);
        // A rewind at 9 back to 4 left 5..=8 behind: only for views at or after 9.
        let forks = [(9, 4, 8)];
        assert!(on_branch(3, &branch(12, &forks)));
        assert!(!on_branch(6, &branch(12, &forks)));
        assert!(on_branch(6, &branch(8, &forks)));
        assert!(!on_branch(13, &branch(12, &forks)));
    }

    #[test]
    fn a_rewind_back_into_a_left_branch_restores_it() {
        // Turn A at 1..=3, B at 4..=6; a rewind at 7 back to 3 left B behind; C at
        // 8..=10; a rewind at 11 back to 6 brings B back and leaves C behind.
        let forks = [(7, 3, 6), (11, 6, 10)];
        let head = branch(11, &forks);
        assert_eq!(head, vec![(11, 11), (0, 6)]);
        assert!(on_branch(1, &head) && on_branch(4, &head));
        assert!(!on_branch(8, &head));
        // Viewed at 10, C is on its branch and B is not.
        let earlier = branch(10, &forks);
        assert!(on_branch(8, &earlier) && on_branch(2, &earlier));
        assert!(!on_branch(4, &earlier));
    }

    #[test]
    fn price_overrides_and_prices_that_are_not_finite_non_negative_dollars_are_refused() {
        // As Dext CLI's env_f64: NaN, infinities and negatives are ignored.
        for raw in ["NaN", "nan", "inf", "-inf", "-1", "x", ""] {
            assert_eq!(override_price(Some(raw)), None, "{raw}");
        }
        assert_eq!(override_price(Some(" 2.5 ")), Some(2.5));
        assert_eq!(override_price(None), None);
        let usage = Usage {
            input: 1000,
            output: 100,
            ..Usage::default()
        };
        let rates = |input: f64| ModelPricing {
            input_usd_per_mtok: input,
            output_usd_per_mtok: 2.0,
            cache_read_usd_per_mtok: 0.1,
            cache_create_usd_per_mtok: 1.25,
        };
        let (billing, usd) = priced_at(&rates(1.0), &usage);
        assert_eq!(billing, Billing::Priced);
        close(usd, (1000.0 + 200.0) / 1e6);
        // A rate that would make the total NaN or negative is no price at all.
        for bad in [f64::NAN, f64::INFINITY, -1.0] {
            assert_eq!(priced_at(&rates(bad), &usage), (Billing::Unpriced, 0.0));
        }
        // A ledger entry saved with a null price is not read back (it is recounted).
        let call = json!({"key": "k", "kind": "parent", "billing": "priced", "usd": null});
        assert!(Call::from_json(&call).is_none());
    }

    #[test]
    fn worst_case_bounds_images_by_their_own_size_and_non_ascii_text_per_byte() {
        let catalog = catalog();
        let worst = |body: &Value| {
            worst_case(
                Some(&catalog),
                "anthropic",
                "claude-opus-5-5",
                "anthropic-messages",
                false,
                &serde_json::to_vec(body).unwrap(),
            )
            .unwrap()
        };
        let output = 1000.0 * 20.0 / 1e6;
        // A 1 MB base64 image counts as one image, not 450,000 tokens of text...
        let image = "A".repeat(1_000_000);
        let anthropic = json!({"max_tokens": 1000, "messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": image}},
            {"type": "text", "text": "what is this?"}]}]});
        let tokens = worst_input(&anthropic);
        assert!(
            (IMAGE_TOKENS..IMAGE_TOKENS + 100).contains(&tokens),
            "{tokens}"
        );
        assert!(worst(&anthropic) < (2.0 * IMAGE_TOKENS as f64 * 5.0) / 1e6 + output);
        // ...as data URLs on OpenAI routes, and as the digests journals keep.
        let url = format!("data:image/png;base64,{image}");
        let chat = json!({"max_tokens": 1000, "messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": url}}]}]});
        assert!(worst_input(&chat) < IMAGE_TOKENS + 100);
        let journal = json!({"messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": {"sha256": "ab", "bytes": 1_333_356}}}]}]});
        assert!(worst_input(&journal) < IMAGE_TOKENS + 100);
        // Text: ASCII at three bytes per token, any other byte a token of its own (CJK
        // and emoji can need more than one token per three bytes).
        let ascii = json!({"text": "x".repeat(3000)});
        let cjk = json!({"text": "漢".repeat(1000)});
        assert!((1000..1010).contains(&worst_input(&ascii)));
        assert!(worst_input(&cjk) >= 3000);
        let digested = json!({"text": {"sha256": "ab", "bytes": 3000}});
        assert!(worst_input(&digested) >= 3000);
    }

    #[test]
    fn an_older_ledger_keeps_its_unpriced_summaries_apart() {
        let dir = std::env::temp_dir().join(format!("spindle-ledger-v1-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("usage.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({"version": 1, "summaries": {"calls": 3, "priced": 1,
                "usd": 0.25, "input": 900, "output": 30}}))
            .unwrap(),
        )
        .unwrap();
        let mut ledger = Ledger::load(&path);
        let report = ledger.report(Live::default());
        assert_eq!(
            (
                report.total.calls,
                report.total.priced,
                report.total.unpriced
            ),
            (3, 1, 2)
        );
        close(report.total.usd, 0.25);
        assert_eq!(report.total.input, 900);
        ledger.undated = false;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
