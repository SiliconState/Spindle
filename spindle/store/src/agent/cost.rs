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
use crate::driver::{SUMMARY_PROMPT, SUMMARY_SOURCE, SUMMARY_STAGE, SUMMARY_SYSTEM};
use crate::view::{ReadView, View};
use dext_provider_catalog::provider::{ModelPricing, ProviderCatalog};
use dext_transcript::Usage;
use std::collections::{BTreeMap, BTreeSet};

/// Version of `usage.json`; version 1 kept only totals.
const LEDGER: u64 = 2;
/// ASCII bytes per token in a worst-case estimate (as the history budget assumes);
/// every other byte counts as a token of its own, as a byte-level tokenizer never
/// needs more.
const WORST_BYTES_PER_TOKEN: u64 = 3;
/// Most input tokens one inline image is estimated at, whatever its encoded size:
/// providers bill images by their pixels (Anthropic and OpenAI scale large ones down
/// to under 2,000 tokens; Gemini charges 258 per 768-pixel tile), never by base64 bytes.
const IMAGE_TOKENS: u64 = 10_000;
/// OpenAI's long-context tier: above this many input tokens a Responses model costs
/// twice the input and 1.5 times the output rates (Dext CLI `usage.rs`).
const LONG_CONTEXT_TOKENS: u64 = 272_000;
/// Longest prompt kept per turn in `usage.json`.
const PROMPT_CHARS: usize = 120;

/// How a call was paid for.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Billing {
    /// Catalog-priced (a provider-stated cost where Dext uses one).
    Priced,
    /// A subscription login (ChatGPT, Anthropic OAuth): no marginal cost.
    Subscription,
    /// Billed, but the catalog has no price for the model.
    Unpriced,
    /// Refused by the provider before generating anything: not billed.
    Refused,
    /// Sent with an unknown outcome: it may have been billed.
    Unknown,
}

impl Billing {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Priced => "priced",
            Self::Subscription => "subscription",
            Self::Unpriced => "unpriced",
            Self::Refused => "refused",
            Self::Unknown => "unknown",
        }
    }
    fn parse(text: &str) -> Option<Self> {
        [
            Self::Priced,
            Self::Subscription,
            Self::Unpriced,
            Self::Refused,
            Self::Unknown,
        ]
        .into_iter()
        .find(|billing| billing.as_str() == text)
    }
}

/// What made a call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallKind {
    /// A turn's own model request.
    Parent,
    /// A `model()` call of a `run_code` script.
    Nested,
    /// A model-written compaction summary.
    Compaction,
}

impl CallKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parent => "parent",
            Self::Nested => "nested",
            Self::Compaction => "compaction",
        }
    }
    fn parse(text: &str) -> Option<Self> {
        [Self::Parent, Self::Nested, Self::Compaction]
            .into_iter()
            .find(|kind| kind.as_str() == text)
    }
}

/// One model call as `usage.json` keeps it, at the price it had when first counted.
#[derive(Clone, Debug)]
pub struct Call {
    /// The reply's effect id, `code:<attempt>#<child>` or `summary:<command>`.
    pub key: String,
    pub kind: CallKind,
    /// The turn (execution id) it belongs to, when known.
    pub turn: Option<String>,
    /// A compaction made for a new prompt belongs to the first turn started after
    /// this revision.
    pub after: Option<u64>,
    pub provider: String,
    pub model: String,
    pub billing: Billing,
    /// Token buckets as the provider reported them (disjoint input buckets).
    pub usage: Usage,
    pub usd: f64,
    /// Unix seconds when it was counted; None when an older or rebuilt `usage.json`
    /// counted it later than it happened.
    pub at: Option<u64>,
    /// Calls this entry stands for (more than one only for summaries an older build
    /// totalled).
    pub count: u64,
    /// For an unknown outcome, the most it could have cost: counted against a budget,
    /// never in the totals.
    pub worst: f64,
}

impl Call {
    fn to_json(&self) -> Value {
        json!({"key": self.key, "kind": self.kind.as_str(), "turn": self.turn, "after": self.after,
            "provider": self.provider, "model": self.model, "billing": self.billing.as_str(),
            "input": self.usage.input, "output": self.usage.output,
            "cache_read": self.usage.cache_read, "cache_create": self.usage.cache_create,
            "usd": self.usd, "at": self.at, "count": self.count, "worst": self.worst})
    }
    /// None for an entry that cannot be read back (the ledger then counts again).
    fn from_json(value: &Value) -> Option<Self> {
        let number = |key: &str| value[key].as_u64().unwrap_or(0);
        let billing = Billing::parse(value["billing"].as_str()?)?;
        // An unknown outcome counted before its worst case was kept is counted again.
        let worst = match value["worst"].as_f64().filter(|usd| valid_usd(*usd)) {
            Some(worst) => worst,
            None if billing == Billing::Unknown => return None,
            None => 0.0,
        };
        Some(Self {
            key: value["key"].as_str()?.into(),
            kind: CallKind::parse(value["kind"].as_str()?)?,
            turn: value["turn"].as_str().map(String::from),
            after: value["after"].as_u64(),
            provider: value["provider"].as_str().unwrap_or_default().into(),
            model: value["model"].as_str().unwrap_or_default().into(),
            billing,
            usage: Usage {
                input: number("input"),
                output: number("output"),
                cache_read: number("cache_read"),
                cache_create: number("cache_create"),
                cost_usd: None,
            },
            usd: value["usd"].as_f64().filter(|usd| valid_usd(*usd))?,
            at: value["at"].as_u64(),
            count: value["count"].as_u64().unwrap_or(1).max(1),
            worst,
        })
    }
}

/// A dollar amount or rate that can be counted: finite and not negative.
fn valid_usd(usd: f64) -> bool {
    usd.is_finite() && usd >= 0.0
}

/// A turn: the prompt that started it and the revision its start committed.
#[derive(Clone, Debug)]
struct Started {
    turn: String,
    revision: u64,
    prompt: String,
}

/// `usage.json`: every counted call at its record-time price, and how far each durable
/// source has been read.
#[derive(Default)]
struct Ledger {
    /// Recorded replies read up to this receipt row, and how many rows that was.
    through: i64,
    counted: u64,
    /// Turn starts read up to this revision.
    turns_through: u64,
    turns: Vec<Started>,
    /// `run_code` attempts and summary stages whose calls are all final and counted.
    done: BTreeSet<String>,
    calls: Vec<Call>,
    keys: BTreeSet<String>,
    /// Calls counted now get no date: the ledger was lost or written by an older build.
    undated: bool,
    /// The store the ledger was counted from (`View::incarnation`).
    store: Option<String>,
    /// Rewinds read up to this fork revision, and the execution each one forked to,
    /// with the turn it continues.
    forks_through: u64,
    aliases: BTreeMap<String, String>,
    /// Worst cases of parent requests with an unknown outcome, by effect (their
    /// request is read once).
    estimates: BTreeMap<String, f64>,
}

impl Ledger {
    fn load(path: &Path) -> Self {
        let value = path
            .parent()
            .and_then(|dir| crate::meta::read(dir, "usage").ok().flatten())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .or_else(|| read_json(path).ok());
        Self::from_value(value)
    }

    fn from_value(value: Option<Value>) -> Self {
        let Some(value) = value else {
            return Self {
                undated: true,
                ..Self::default()
            };
        };
        if value["version"].as_u64() != Some(LEDGER) {
            // Version 1 kept totals only; its summaries had no other record.
            let mut ledger = Self {
                undated: true,
                ..Self::default()
            };
            let summaries = &value["summaries"];
            let number = |key: &str| summaries[key].as_u64().unwrap_or(0);
            let count = number("calls");
            // Its priced summaries carry the dollars and tokens; the rest were unpriced.
            let priced = number("priced").min(count);
            for (key, billing, count) in [
                ("summaries-before-ledger", Billing::Priced, priced),
                (
                    "summaries-before-ledger-unpriced",
                    Billing::Unpriced,
                    count - priced,
                ),
            ] {
                if count == 0 {
                    continue;
                }
                let first = ledger.calls.is_empty();
                ledger.push(Call {
                    key: key.into(),
                    kind: CallKind::Compaction,
                    turn: None,
                    after: None,
                    provider: String::new(),
                    model: String::new(),
                    billing,
                    usage: if first {
                        Usage {
                            input: number("input"),
                            output: number("output"),
                            cache_read: number("cache_read"),
                            cache_create: number("cache_create"),
                            cost_usd: None,
                        }
                    } else {
                        Usage::default()
                    },
                    usd: match billing {
                        Billing::Priced => summaries["usd"]
                            .as_f64()
                            .filter(|usd| valid_usd(*usd))
                            .unwrap_or(0.0),
                        _ => 0.0,
                    },
                    at: None,
                    count,
                    worst: 0.0,
                });
            }
            return ledger;
        }
        let mut ledger = Self {
            through: value["through"].as_i64().unwrap_or(0),
            counted: value["counted"].as_u64().unwrap_or(0),
            turns_through: value["turns_through"].as_u64().unwrap_or(0),
            store: value["store"].as_str().map(String::from),
            forks_through: value["forks_through"].as_u64().unwrap_or(0),
            ..Self::default()
        };
        if let Some(aliases) = value["aliases"].as_object() {
            for (forked, turn) in aliases {
                if let Some(turn) = turn.as_str() {
                    ledger.aliases.insert(forked.clone(), turn.into());
                }
            }
        }
        if let Some(estimates) = value["estimates"].as_object() {
            for (effect, worst) in estimates {
                if let Some(worst) = worst.as_f64().filter(|usd| valid_usd(*usd)) {
                    ledger.estimates.insert(effect.clone(), worst);
                }
            }
        }
        for turn in value["turns"].as_array().into_iter().flatten() {
            if let (Some(id), Some(revision)) = (turn["turn"].as_str(), turn["revision"].as_u64()) {
                ledger.turns.push(Started {
                    turn: id.into(),
                    revision,
                    prompt: turn["prompt"].as_str().unwrap_or_default().into(),
                });
            }
        }
        ledger.done = value["done"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        let mut unreadable = false;
        for call in value["calls"].as_array().into_iter().flatten() {
            match Call::from_json(call) {
                Some(call) => ledger.push(call),
                None => unreadable = true,
            }
        }
        // A call that cannot be read back is counted again from the store (at current
        // prices, undated) rather than lost behind the cursors.
        if unreadable {
            ledger.recount();
        }
        ledger
    }

    fn encoded(&self) -> Result<Vec<u8>> {
        let value = json!({"version": LEDGER, "through": self.through, "counted": self.counted,
            "turns_through": self.turns_through,
            "turns": self.turns.iter().map(|t| json!({"turn": t.turn, "revision": t.revision, "prompt": t.prompt})).collect::<Vec<_>>(),
            "done": self.done, "calls": self.calls.iter().map(Call::to_json).collect::<Vec<_>>(),
            "store": self.store, "forks_through": self.forks_through, "aliases": self.aliases,
            "estimates": self.estimates});
        serde_json::to_vec(&value).map_err(|error| error.to_string())
    }

    fn save(&self, path: &Path) -> Result<()> {
        let bytes = self.encoded()?;
        if let Some(dir) = path.parent() {
            crate::meta::update(dir, "usage", |_| Ok(bytes.clone())).map_err(store_error)?;
        }
        save_private(path, &bytes)
    }

    fn push(&mut self, call: Call) {
        if self.keys.insert(call.key.clone()) {
            self.calls.push(call);
        }
    }

    fn at(&self, now: u64) -> Option<u64> {
        (!self.undated).then_some(now)
    }

    /// Reads every durable source again; calls already counted keep their price, and
    /// what is counted now gets no date.
    fn recount(&mut self) {
        self.through = 0;
        self.counted = 0;
        self.turns_through = 0;
        self.turns.clear();
        self.done.clear();
        self.forks_through = 0;
        self.aliases.clear();
        self.estimates.clear();
        self.undated = true;
    }

    /// The ledger was counted from another store (copied beside this one, or this one
    /// replaced): only what that store recorded is dropped, and this one is counted.
    fn forget_store(&mut self) {
        self.calls.retain(|call| !from_store(&call.key));
        self.keys = self.calls.iter().map(|call| call.key.clone()).collect();
        self.recount();
    }

    /// Reads what the store recorded since the last update. Returns whether anything
    /// changed; `live` gets what is not final (unknown and refused parent requests,
    /// children and summaries still in flight).
    fn update(
        &mut self,
        view: &mut ReadView<'_>,
        catalog: Option<&ProviderCatalog>,
        now: u64,
        live: &mut Live,
    ) -> crate::Result<bool> {
        let mut changed = false;
        // A ledger counted from another store keeps nothing that store recorded: its
        // keys (fixed command ids such as `start`) would hide this store's calls.
        let store = view.incarnation()?;
        if store.is_some() && self.store != store {
            if self.store.is_some() {
                self.forget_store();
            }
            self.store = store;
            changed = true;
        }
        // Replies. A store rewritten under the ledger (migration, a first request
        // retried in a fresh store) renumbers rows: read them all again (calls already
        // counted keep their price).
        if view.model_receipt_count(self.through)? != self.counted {
            self.through = 0;
            self.counted = 0;
            changed = true;
        }
        loop {
            let batch = view.model_receipts_after(self.through, 256)?;
            if batch.is_empty() {
                break;
            }
            for (row, bytes) in batch {
                self.through = row;
                self.counted += 1;
                changed = true;
                let Ok(recorded) = ModelWait::restore(&bytes) else {
                    continue;
                };
                let request = recorded.request();
                let identity = request.identity();
                if self.keys.contains(&identity.effect) {
                    continue;
                }
                let Some(response) = recorded
                    .response()
                    .and_then(|bytes| NativeResponse::restore(bytes).ok())
                else {
                    continue;
                };
                let subscription = subscription_request(&identity.contract, request.body());
                let (billing, usd) = price(
                    catalog,
                    &identity.provider,
                    &identity.model,
                    &identity.contract,
                    subscription,
                    &response.usage,
                );
                let at = self.at(now);
                self.push(Call {
                    key: identity.effect.clone(),
                    kind: CallKind::Parent,
                    turn: Some(turn_of(&identity.effect)),
                    after: None,
                    provider: identity.provider.clone(),
                    model: identity.model.clone(),
                    billing,
                    usage: tokens(&response.usage),
                    usd,
                    at,
                    count: 1,
                    worst: 0.0,
                });
            }
        }
        // Turns, with the prompt that started each.
        let (head, _, _) = view.head()?;
        if head < self.turns_through {
            self.turns_through = 0;
        }
        if head < self.forks_through {
            self.forks_through = 0;
            self.aliases.clear();
        }
        for (revision, command) in view.turn_starts(self.turns_through)? {
            self.turns_through = revision;
            changed = true;
            let turn = crate::turn::execution_id(SCOPE, &command)?;
            if self.turns.iter().any(|started| started.turn == turn) {
                continue;
            }
            let prompt = view
                .at(revision)?
                .and_then(|(bytes, _)| TurnContinuation::restore(&bytes).ok())
                .and_then(|state| state.transcript().ok())
                .and_then(|transcript| {
                    transcript
                        .messages
                        .iter()
                        .rev()
                        .find(|message| is_prompt(message))
                        .map(|message| text_of(&message.content))
                })
                .unwrap_or_default();
            self.turns.push(Started {
                turn,
                revision,
                prompt: one_line(&prompt, PROMPT_CHARS),
            });
        }
        self.turns.sort_by_key(|started| started.revision);
        // Rewinds. One to a point before a model request continues that turn under a
        // fresh execution id (retries do this too): its calls belong to the turn it
        // forked from.
        let forks = view.forks()?;
        let through = self.forks_through;
        for fork in forks.iter().filter(|fork| fork.revision > through) {
            self.forks_through = fork.revision;
            changed = true;
            if let (Some(forked), Some(target)) = (
                execution_at(view, fork.revision)?,
                execution_at(view, fork.target)?,
            ) && forked != target
            {
                let turn = self.aliases.get(&target).cloned().unwrap_or(target);
                self.aliases.insert(forked, turn);
            }
        }
        // Nested model calls of run_code scripts, from their journals (or the spend
        // records garbage collection keeps once a journal is collected).
        for (attempt, settled) in view.code_attempts()? {
            if self.done.contains(&attempt) {
                continue;
            }
            let mut stages: Vec<String> = (0..crate::code_mode::MAX_CALLS as u64)
                .map(|id| crate::code_mode::child_stage(&attempt, id))
                .collect();
            stages.push(crate::artifact::spend_stage(&attempt));
            let mut children: BTreeMap<u64, Child> = BTreeMap::new();
            for (_, bytes) in view.stage_records(&stages)? {
                let Ok(record) = serde_json::from_slice::<Value>(&bytes) else {
                    continue;
                };
                match record["kind"].as_str() {
                    Some("reserved") => {
                        if let Some(id) = record["request"]["id"].as_u64() {
                            children.entry(id).or_default().reserved = Some(record);
                        }
                    }
                    Some("result") => {
                        if let Some(id) = record["id"].as_u64() {
                            children.entry(id).or_default().result =
                                Some(record["response"].clone());
                        }
                    }
                    Some("spend") => {
                        if let Some(id) = record["id"].as_u64() {
                            let child = children.entry(id).or_default();
                            child.result.get_or_insert(record["response"].clone());
                            child.spend = Some(record);
                        }
                    }
                    _ => {}
                }
            }
            for (id, child) in &children {
                let Some(prepared) = child.prepared() else {
                    continue;
                };
                let key = format!("code:{attempt}#{id}");
                if self.keys.contains(&key) {
                    continue;
                }
                let worst = journal_worst(catalog, prepared);
                let outcome = match child.result.as_ref().filter(|result| !result.is_null()) {
                    Some(response) => outcome(catalog, prepared, response),
                    // Reserved with no result: still running, or lost with its worker.
                    None if settled => Some(Outcome::unknown(prepared)),
                    None => {
                        live.unknown.push(Pending {
                            kind: CallKind::Nested,
                            turn: Some(turn_of(&attempt)),
                            after: None,
                            worst,
                        });
                        continue;
                    }
                };
                let Some(outcome) = outcome else {
                    continue;
                };
                let at = self.at(now);
                let turn = Some(turn_of(&attempt));
                self.push(outcome.call(key, CallKind::Nested, turn, None, at, worst));
                changed = true;
            }
            if settled {
                self.done.insert(attempt);
                changed = true;
            }
        }
        // Model-written compaction summaries, each journaled in its own stage.
        for stage in view.stages_named(SUMMARY_STAGE)? {
            if self.done.contains(&stage) {
                continue;
            }
            let mut reserved = None;
            let mut result = None;
            for (_, bytes) in view.stage_records(std::slice::from_ref(&stage))? {
                let Ok(record) = serde_json::from_slice::<Value>(&bytes) else {
                    continue;
                };
                match record["kind"].as_str() {
                    Some("reserved") => reserved = Some(record),
                    Some("result") => result = Some(record),
                    _ => {}
                }
            }
            let Some(reserved) = reserved else {
                continue;
            };
            let turn = reserved["turn"].as_str().map(String::from);
            let after = reserved["idle"]
                .as_bool()
                .filter(|idle| *idle)
                .and_then(|_| reserved["revision"].as_u64());
            let worst = summary_worst(catalog, &reserved);
            let outcome = match result {
                Some(result) => outcome(catalog, &reserved, &result["response"]),
                // Its compaction was committed (or given up) with no reply recorded: an
                // unknown outcome, final.
                None if reserved["revision"]
                    .as_u64()
                    .is_some_and(|revision| head > revision) =>
                {
                    Some(Outcome::unknown(&reserved))
                }
                // Not final: it may still be on its way (or was lost with its process).
                None => {
                    live.unknown.push(Pending {
                        kind: CallKind::Compaction,
                        turn,
                        after,
                        worst,
                    });
                    continue;
                }
            };
            if let Some(outcome) = outcome {
                let key = format!("summary:{}", &stage[SUMMARY_STAGE.len()..]);
                let at = self.at(now);
                self.push(outcome.call(key, CallKind::Compaction, turn, after, at, worst));
            }
            self.done.insert(stage);
            changed = true;
        }
        // Parent requests without a reply: refused (known, not billed) or unknown,
        // each unknown one at its request's worst case (read once).
        for (effect, status) in view.unanswered_model_attempts()? {
            let turn = Some(turn_of(&effect));
            if status.is_some() {
                live.refused.push(Pending {
                    kind: CallKind::Parent,
                    turn,
                    after: None,
                    worst: 0.0,
                });
                continue;
            }
            let worst = match self.estimates.get(&effect) {
                Some(worst) => *worst,
                None => {
                    let worst = request_worst(view, catalog, &effect)?;
                    self.estimates.insert(effect.clone(), worst);
                    changed = true;
                    worst
                }
            };
            live.unknown.push(Pending {
                kind: CallKind::Parent,
                turn,
                after: None,
                worst,
            });
            live.unanswered.insert(effect);
        }
        // An estimate is kept while its request has no reply.
        let estimates = self.estimates.len();
        self.estimates
            .retain(|effect, _| live.unanswered.contains(effect));
        changed |= self.estimates.len() != estimates;
        live.forks = forks
            .into_iter()
            .map(|fork| (fork.revision, fork.target, fork.abandoned))
            .collect();
        live.head = head;
        Ok(changed)
    }

    fn report(&self, live: Live) -> UsageReport {
        let mut report = UsageReport {
            calls: self.calls.clone(),
            aliases: self.aliases.clone(),
            ..UsageReport::default()
        };
        // A turn is left behind when it is off the branch that leads to the head.
        let current = branch(live.head, &live.forks);
        report.turns = self
            .turns
            .iter()
            .map(|started| TurnUsage {
                turn: started.turn.clone(),
                revision: started.revision,
                prompt: started.prompt.clone(),
                abandoned: !on_branch(started.revision, &current),
                ..TurnUsage::default()
            })
            .collect();
        report.index = report
            .turns
            .iter()
            .enumerate()
            .map(|(index, turn)| (turn.turn.clone(), index))
            .collect();
        let mut models: BTreeMap<(String, String), Tally> = BTreeMap::new();
        for call in &self.calls {
            report.total.add_call(call);
            report.by_kind_mut(call.kind).add_call(call);
            if call.billing == Billing::Unknown {
                report.uncertain_usd += call.worst * call.count as f64;
            }
            if let Some(turn) = report.owner(call.turn.as_deref(), call.after) {
                report.turns[turn].usage.add_call(call);
                report.turns[turn].by_kind_mut(call.kind).add_call(call);
            }
            if matches!(
                call.billing,
                Billing::Priced | Billing::Subscription | Billing::Unpriced
            ) && !call.model.is_empty()
            {
                models
                    .entry((call.provider.clone(), call.model.clone()))
                    .or_default()
                    .add_call(call);
            }
        }
        for (list, refused) in [(&live.refused, true), (&live.unknown, false)] {
            for pending in list {
                report.total.note(refused);
                report.by_kind_mut(pending.kind).note(refused);
                if !refused {
                    report.uncertain_usd += pending.worst;
                }
                if let Some(turn) = report.owner(pending.turn.as_deref(), pending.after) {
                    report.turns[turn].usage.note(refused);
                    report.turns[turn].by_kind_mut(pending.kind).note(refused);
                }
            }
        }
        report.models = models
            .into_iter()
            .map(|((provider, model), usage)| ModelUsage {
                provider,
                model,
                usage,
            })
            .collect();
        report.unanswered = live.unanswered;
        report
    }
}

/// What a report shows that is not final, read fresh each time.
#[derive(Default)]
struct Live {
    refused: Vec<Pending>,
    unknown: Vec<Pending>,
    /// Parent effects sent with an unknown outcome (or in flight).
    unanswered: BTreeSet<String>,
    forks: Vec<(u64, u64, u64)>,
    head: u64,
}

/// A request counted beside the billed calls: refused, or of unknown outcome.
struct Pending {
    kind: CallKind,
    turn: Option<String>,
    /// As `Call::after`.
    after: Option<u64>,
    /// The most an unknown outcome could have cost.
    worst: f64,
}

/// The execution id of a committed revision's turn.
fn execution_at(view: &mut ReadView<'_>, revision: u64) -> crate::Result<Option<String>> {
    Ok(view
        .at(revision)?
        .and_then(|(bytes, _)| TurnContinuation::restore(&bytes).ok())
        .map(|state| state.execution().to_string()))
}

/// Whether a ledger key was counted from the store itself (not from a store set aside
/// by a restart, nor kept by an older ledger).
fn from_store(key: &str) -> bool {
    !key.starts_with(ASIDE) && !key.starts_with("summaries-before-ledger")
}

/// Key prefix of requests kept from a store a restart set aside.
const ASIDE: &str = "aside:";

/// One nested child's journal records.
#[derive(Default)]
struct Child {
    reserved: Option<Value>,
    result: Option<Value>,
    spend: Option<Value>,
}

impl Child {
    /// The reserved route, for model children only.
    fn prepared(&self) -> Option<&Value> {
        match (&self.reserved, &self.spend) {
            (Some(reserved), _) if reserved["request"]["method"] == "model" => {
                Some(&reserved["prepared"])
            }
            (None, Some(spend)) => Some(&spend["prepared"]),
            _ => None,
        }
    }
}

/// A journaled model call's priced outcome.
struct Outcome {
    provider: String,
    model: String,
    billing: Billing,
    usage: Usage,
    usd: f64,
}

impl Outcome {
    fn unknown(route: &Value) -> Self {
        Self {
            provider: route["provider"].as_str().unwrap_or_default().into(),
            model: route["model"].as_str().unwrap_or_default().into(),
            billing: Billing::Unknown,
            usage: Usage::default(),
            usd: 0.0,
        }
    }
    /// The call it was; `worst` is kept for an unknown outcome only.
    fn call(
        self,
        key: String,
        kind: CallKind,
        turn: Option<String>,
        after: Option<u64>,
        at: Option<u64>,
        worst: f64,
    ) -> Call {
        Call {
            key,
            kind,
            turn,
            after,
            provider: self.provider,
            model: self.model,
            billing: self.billing,
            usage: self.usage,
            usd: self.usd,
            at,
            count: 1,
            worst: if self.billing == Billing::Unknown {
                worst
            } else {
                0.0
            },
        }
    }
}

/// How a journaled model call (nested or summary) ended, from its route (`provider`,
/// `model`, `contract`) and its recorded response. None: it never left the host.
fn outcome(catalog: Option<&ProviderCatalog>, route: &Value, response: &Value) -> Option<Outcome> {
    let data = &response["data"];
    let provider = data["provider"]
        .as_str()
        .or(route["provider"].as_str())
        .unwrap_or_default()
        .to_string();
    let model = data["model"]
        .as_str()
        .or(route["model"].as_str())
        .unwrap_or_default()
        .to_string();
    let contract = route["contract"].as_str().unwrap_or_default();
    let mut outcome = Outcome {
        provider,
        model,
        billing: Billing::Unknown,
        usage: Usage::default(),
        usd: 0.0,
    };
    if crate::code_mode::unknown_outcome(response) {
        return Some(outcome);
    }
    if response["ok"] == true {
        let usage: Usage = serde_json::from_value(data["usage"].clone()).unwrap_or_default();
        let subscription = match data["subscription"].as_bool() {
            Some(subscription) => subscription,
            // Journals written before the flag: the login the route has now.
            None => {
                contract == RequestContract::ChatGptResponses.as_str()
                    || (contract == RequestContract::AnthropicMessages.as_str()
                        && route_subscription(&outcome.provider, &outcome.model))
            }
        };
        let (billing, usd) = price(
            catalog,
            &outcome.provider,
            &outcome.model,
            contract,
            subscription,
            &usage,
        );
        outcome.billing = billing;
        outcome.usd = usd;
        outcome.usage = tokens(&usage);
        return Some(outcome);
    }
    if response["status"].as_u64().is_some() {
        outcome.billing = Billing::Refused;
        return Some(outcome);
    }
    // A failure after the request left the host may have been billed.
    (response["may_have_run"] == true).then_some(outcome)
}

/// Token buckets only (a provider-stated cost is part of the price, not the tokens).
fn tokens(usage: &Usage) -> Usage {
    Usage {
        cost_usd: None,
        ..*usage
    }
}

/// The turn (execution id) of an effect or attempt id (`[a:]<execution>/m/<n>...`).
fn turn_of(id: &str) -> String {
    id.trim_start_matches("a:")
        .split('/')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// The first line of `text`, at most `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let line = text.trim().lines().next().unwrap_or_default();
    let mut short: String = line.chars().take(max).collect();
    if short.len() < line.len() || text.trim().lines().nth(1).is_some() {
        short.push('…');
    }
    short
}

/// Dollars in messages: to a hundredth of a cent under a dollar, else to the cent.
fn dollars(usd: f64) -> String {
    if usd < 1.0 {
        format!("${usd:.4}")
    } else {
        format!("${usd:.2}")
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Whether a recorded request went to a subscription login: the ChatGPT backend takes
/// nothing else, and an Anthropic subscription request carries Claude Code's billing
/// block first in its system text (`claude_subscription::transform_body`).
fn subscription_request(contract: &str, body: &[u8]) -> bool {
    const MARKER: &[u8] = b"\"system\":[{\"type\":\"text\",\"text\":\"x-anthropic-billing-header: ";
    contract == RequestContract::ChatGptResponses.as_str()
        || (contract == RequestContract::AnthropicMessages.as_str()
            && body.windows(MARKER.len()).any(|window| window == MARKER))
}

/// Whether a route sends through a subscription login now.
fn route_subscription(provider: &str, model: &str) -> bool {
    match route(provider, Some(model)) {
        Ok(route) => {
            route.contract == RequestContract::ChatGptResponses || anthropic_subscription(&route)
        }
        Err(_) => false,
    }
}

/// Dext's pricing overrides (`DEXT_*_USD_PER_MTOK`), as Dext CLI `usage.rs` applies
/// them to a catalog price.
fn env_price(name: &str) -> Option<f64> {
    override_price(std::env::var(name).ok().as_deref())
}

/// An override as Dext CLI's `env_f64` reads one: a finite, non-negative number
/// (anything else is ignored, as if unset).
fn override_price(raw: Option<&str>) -> Option<f64> {
    raw.and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|price| valid_usd(*price))
}

pub(super) fn priced_by_env(pricing: &ModelPricing) -> (ModelPricing, bool) {
    let overrides = [
        env_price("DEXT_INPUT_USD_PER_MTOK"),
        env_price("DEXT_OUTPUT_USD_PER_MTOK"),
        env_price("DEXT_CACHE_READ_USD_PER_MTOK"),
        env_price("DEXT_CACHE_CREATE_USD_PER_MTOK"),
    ];
    let set = overrides.iter().any(Option::is_some);
    (
        ModelPricing {
            input_usd_per_mtok: overrides[0].unwrap_or(pricing.input_usd_per_mtok),
            output_usd_per_mtok: overrides[1].unwrap_or(pricing.output_usd_per_mtok),
            cache_read_usd_per_mtok: overrides[2].unwrap_or(pricing.cache_read_usd_per_mtok),
            cache_create_usd_per_mtok: overrides[3].unwrap_or(pricing.cache_create_usd_per_mtok),
        },
        set,
    )
}

/// The built-in catalog's price for a model, the "official" one the long-context tier
/// applies to.
fn builtin_pricing(provider: &str, model: &str) -> Option<ModelPricing> {
    static BUILTIN: std::sync::OnceLock<ProviderCatalog> = std::sync::OnceLock::new();
    let catalog = BUILTIN.get_or_init(provider::default_provider_catalog);
    provider::find_provider_profile(catalog, &provider::canonical_provider_id(provider))
        .and_then(|profile| provider::resolve_model_spec(&profile, model).pricing)
}

/// Ported from Dext CLI `usage.rs` `openai_long_context_pricing_with_override_state`:
/// an OpenAI (or ChatGPT-route) Responses model whose input exceeds 272,000 tokens is
/// billed at twice the input/cache rates and 1.5 times the output rate, unless the
/// price was overridden (environment, or a catalog price that is not the built-in one).
fn long_context(
    provider: &str,
    model: &str,
    usage: &Usage,
    pricing: ModelPricing,
    overridden: bool,
) -> ModelPricing {
    let tiered = matches!(
        provider::canonical_provider_id(provider).as_str(),
        "openai" | "chatgpt"
    ) && provider::is_openai_responses_model(model)
        && usage.total_input_tokens() > LONG_CONTEXT_TOKENS
        && !overridden
        && builtin_pricing(provider, model).as_ref() == Some(&pricing);
    if !tiered {
        return pricing;
    }
    ModelPricing {
        input_usd_per_mtok: pricing.input_usd_per_mtok * 2.0,
        output_usd_per_mtok: pricing.output_usd_per_mtok * 1.5,
        cache_read_usd_per_mtok: pricing.cache_read_usd_per_mtok * 2.0,
        cache_create_usd_per_mtok: pricing.cache_create_usd_per_mtok * 2.0,
    }
}

/// Dext CLI `provider_cost_estimate_overrides_wire_cost`: on Anthropic routes that
/// cache prompts (the anthropic provider, or a `claude-*` model) Dext's own estimate
/// replaces a provider-stated cost.
fn estimate_overrides_wire_cost(provider: &str, model: &str, contract: &str) -> bool {
    contract == RequestContract::AnthropicMessages.as_str()
        && (provider::canonical_provider_id(provider) == "anthropic"
            || model
                .trim()
                .to_ascii_lowercase()
                .rsplit('/')
                .next()
                .is_some_and(|model| model.starts_with("claude-")))
}

/// What one call cost. Usage arrives in disjoint buckets on every contract (the
/// engine's parsers subtract cached tokens from OpenAI-style totals), and output
/// includes reasoning/thinking tokens everywhere, so each bucket is charged at its own
/// rate exactly once. Rules as Dext CLI `usage_with_current_pricing`: a cost the
/// provider states is used as is, except where Dext's estimate overrides it;
/// otherwise the catalog price (with Dext's environment overrides and OpenAI's
/// long-context tier). Subscription logins cost nothing per call; a model without a
/// catalog price is unpriced (Dext's generic default rates are not used).
pub(super) fn price(
    catalog: Option<&ProviderCatalog>,
    provider: &str,
    model: &str,
    contract: &str,
    subscription: bool,
    usage: &Usage,
) -> (Billing, f64) {
    if subscription {
        return (Billing::Subscription, 0.0);
    }
    let wire = usage
        .cost_usd
        .filter(|cost| cost.is_finite() && *cost >= 0.0);
    if let Some(cost) = wire
        && !estimate_overrides_wire_cost(provider, model, contract)
    {
        return (Billing::Priced, cost);
    }
    let pricing = catalog
        .and_then(|catalog| provider::find_provider_profile(catalog, provider))
        .and_then(|profile| provider::resolve_model_spec(&profile, model).pricing);
    let Some(pricing) = pricing else {
        return match wire {
            Some(cost) => (Billing::Priced, cost),
            None => (Billing::Unpriced, 0.0),
        };
    };
    let (pricing, overridden) = priced_by_env(&pricing);
    let rates = long_context(provider, model, usage, pricing, overridden);
    priced_at(&rates, usage)
}

/// Usage at catalog rates; a rate (or a total) that is not a finite, non-negative
/// number of dollars is no price, so it never reaches a total or a budget.
fn priced_at(rates: &ModelPricing, usage: &Usage) -> (Billing, f64) {
    let rates = [
        (usage.input, rates.input_usd_per_mtok),
        (usage.output, rates.output_usd_per_mtok),
        (usage.cache_read, rates.cache_read_usd_per_mtok),
        (usage.cache_create, rates.cache_create_usd_per_mtok),
    ];
    if !rates.iter().all(|(_, rate)| valid_usd(*rate)) {
        return (Billing::Unpriced, 0.0);
    }
    let usd = rates
        .iter()
        .map(|(tokens, rate)| *tokens as f64 * rate)
        .sum::<f64>()
        / 1_000_000.0;
    if !valid_usd(usd) {
        return (Billing::Unpriced, 0.0);
    }
    (Billing::Priced, usd)
}

/// The most one request could cost: its body's text at three ASCII bytes per token
/// (any other byte a token) and each inline image at `IMAGE_TOKENS`, all billed as
/// fresh input (or as a cache write where that is dearer), plus every output token it
/// allows (answer and reasoning). Some(0) for a subscription; None when the model has
/// no price.
pub(super) fn worst_case(
    catalog: Option<&ProviderCatalog>,
    provider: &str,
    model: &str,
    contract: &str,
    subscription: bool,
    body: &[u8],
) -> Option<f64> {
    if subscription {
        return Some(0.0);
    }
    let (input, output) = match serde_json::from_slice::<Value>(body) {
        Ok(body) => (worst_input(&body), allowed_output(&body)),
        Err(_) => {
            let ascii = body.iter().filter(|byte| byte.is_ascii()).count() as u64;
            (
                ascii.div_ceil(WORST_BYTES_PER_TOKEN) + (body.len() as u64 - ascii),
                allowed_output(&Value::Null),
            )
        }
    };
    worst_tokens(catalog, provider, model, contract, input, output)
}

/// Every output token a request body allows (Spindle's largest when it states none).
fn allowed_output(body: &Value) -> u64 {
    ["max_tokens", "max_output_tokens", "max_completion_tokens"]
        .iter()
        .find_map(|key| body[*key].as_u64())
        .unwrap_or(u64::from(MAX_OUTPUT_TOKENS) + 65_536)
}

/// Worst-case input tokens of a request body (as `worst_case` counts them). Strings a
/// journal keeps as digests (`{"sha256", "bytes"}`) count at their original size, as
/// if none of it were ASCII.
fn worst_input(body: &Value) -> u64 {
    #[derive(Default)]
    struct Size {
        ascii: u64,
        other: u64,
        images: u64,
    }
    fn digested(value: &Value) -> Option<u64> {
        let object = value.as_object()?;
        (object.len() == 2 && object.contains_key("sha256"))
            .then(|| object.get("bytes")?.as_u64())
            .flatten()
    }
    fn measure(value: &Value, image: bool, size: &mut Size) {
        match value {
            Value::String(text) => {
                if text.starts_with("data:image/") || (image && text.len() > 256) {
                    size.images += 1;
                } else {
                    let ascii = text.bytes().filter(u8::is_ascii).count() as u64;
                    size.ascii += ascii + 2;
                    size.other += text.len() as u64 - ascii;
                }
            }
            Value::Object(object) => {
                if let Some(bytes) = digested(value) {
                    if image {
                        size.images += 1;
                    } else {
                        size.other += bytes;
                    }
                    return;
                }
                // An image block (Anthropic `image`, OpenAI `image_url`/`input_image`)
                // or base64 data beside an image media type.
                let image = image
                    || matches!(
                        object.get("type").and_then(Value::as_str),
                        Some("image" | "image_url" | "input_image")
                    )
                    || ["media_type", "mime_type", "mimeType"].iter().any(|key| {
                        object
                            .get(*key)
                            .and_then(Value::as_str)
                            .is_some_and(|media| media.starts_with("image/"))
                    });
                size.ascii += 2;
                for (key, value) in object {
                    size.ascii += key.len() as u64 + 4;
                    measure(value, image, size);
                }
            }
            Value::Array(items) => {
                size.ascii += 2;
                for item in items {
                    size.ascii += 1;
                    measure(item, image, size);
                }
            }
            other => size.ascii += other.to_string().len() as u64,
        }
    }
    let mut size = Size::default();
    measure(body, false, &mut size);
    size.ascii.div_ceil(WORST_BYTES_PER_TOKEN) + size.other + size.images * IMAGE_TOKENS
}

/// The worst case of a journaled model call (nested, from its reserved route and
/// body in journal form): 0 when its model has no price.
fn journal_worst(catalog: Option<&ProviderCatalog>, prepared: &Value) -> f64 {
    let provider = prepared["provider"].as_str().unwrap_or_default();
    let model = prepared["model"].as_str().unwrap_or_default();
    let contract = prepared["contract"].as_str().unwrap_or_default();
    let subscription = contract == RequestContract::ChatGptResponses.as_str()
        || (contract == RequestContract::AnthropicMessages.as_str()
            && route_subscription(provider, model));
    if subscription {
        return 0.0;
    }
    let body = &prepared["body"];
    worst_tokens(
        catalog,
        provider,
        model,
        contract,
        worst_input(body),
        allowed_output(body),
    )
    .unwrap_or(0.0)
}

/// The worst case a summary's reservation recorded, or (journals before it did) one
/// for the largest summary request: its whole source, none of it ASCII.
fn summary_worst(catalog: Option<&ProviderCatalog>, reserved: &Value) -> f64 {
    if let Some(worst) = reserved["worst"].as_f64().filter(|usd| valid_usd(*usd)) {
        return worst;
    }
    let route = |key: &str| reserved[key].as_str().unwrap_or_default().to_string();
    let (provider, model, contract) = (route("provider"), route("model"), route("contract"));
    if contract == RequestContract::ChatGptResponses.as_str()
        || (contract == RequestContract::AnthropicMessages.as_str()
            && route_subscription(&provider, &model))
    {
        return 0.0;
    }
    let input = (SUMMARY_SOURCE + SUMMARY_PROMPT.len() + SUMMARY_SYSTEM.len()) as u64;
    worst_tokens(
        catalog,
        &provider,
        &model,
        &contract,
        input,
        allowed_output(&Value::Null),
    )
    .unwrap_or(0.0)
}

/// The worst case of a parent request, from its committed intent: 0 when it cannot
/// be read or its model has no price.
fn request_worst(
    view: &mut ReadView<'_>,
    catalog: Option<&ProviderCatalog>,
    effect: &str,
) -> crate::Result<f64> {
    let Some(bytes) = view.intent_input(effect)? else {
        return Ok(0.0);
    };
    let Ok(waiting) = ModelWait::restore(&bytes) else {
        return Ok(0.0);
    };
    let request = waiting.request();
    let identity = request.identity();
    let subscription = subscription_request(&identity.contract, request.body());
    Ok(worst_case(
        catalog,
        &identity.provider,
        &identity.model,
        &identity.contract,
        subscription,
        request.body(),
    )
    .unwrap_or(0.0))
}

/// `input` tokens billed as fresh input or as a cache write, whichever is dearer, plus
/// `output` tokens. None when the model has no price.
fn worst_tokens(
    catalog: Option<&ProviderCatalog>,
    provider: &str,
    model: &str,
    contract: &str,
    input: u64,
    output: u64,
) -> Option<f64> {
    let fresh = Usage {
        input,
        output,
        ..Usage::default()
    };
    let written = Usage {
        cache_create: input,
        output,
        ..Usage::default()
    };
    match (
        price(catalog, provider, model, contract, false, &fresh),
        price(catalog, provider, model, contract, false, &written),
    ) {
        ((Billing::Priced, a), (_, b)) => Some(a.max(b)),
        _ => None,
    }
}

impl Tally {
    fn add_call(&mut self, call: &Call) {
        match call.billing {
            Billing::Refused => self.refused += call.count,
            Billing::Unknown => self.unknown += call.count,
            billing => {
                self.calls += call.count;
                self.input += call.usage.input;
                self.output += call.usage.output;
                self.cache_read += call.usage.cache_read;
                self.cache_create += call.usage.cache_create;
                self.usd += call.usd;
                match billing {
                    Billing::Priced => self.priced += call.count,
                    Billing::Subscription => self.subscription += call.count,
                    _ => self.unpriced += call.count,
                }
            }
        }
    }
    fn note(&mut self, refused: bool) {
        if refused {
            self.refused += 1;
        } else {
            self.unknown += 1;
        }
    }
}

/// One turn's spend: the prompt that started it and every call it caused.
#[derive(Clone, Debug, Default)]
pub struct TurnUsage {
    /// Execution id of the turn.
    pub turn: String,
    /// Revision its start committed.
    pub revision: u64,
    /// The prompt's first line (shortened).
    pub prompt: String,
    /// A rewind left this turn behind (it was billed all the same).
    pub abandoned: bool,
    pub usage: Tally,
    pub parent: Tally,
    pub nested: Tally,
    pub compaction: Tally,
}

impl TurnUsage {
    fn by_kind_mut(&mut self, kind: CallKind) -> &mut Tally {
        match kind {
            CallKind::Parent => &mut self.parent,
            CallKind::Nested => &mut self.nested,
            CallKind::Compaction => &mut self.compaction,
        }
    }
    pub fn to_json(&self) -> Value {
        let mut value = self.usage.to_json();
        value["turn"] = json!(self.turn);
        value["revision"] = json!(self.revision);
        value["prompt"] = json!(self.prompt);
        value["abandoned"] = json!(self.abandoned);
        value["parent"] = self.parent.to_json();
        value["nested"] = self.nested.to_json();
        value["compaction"] = self.compaction.to_json();
        value
    }
}

/// One model's spend across the session.
#[derive(Clone, Debug, Default)]
pub struct ModelUsage {
    pub provider: String,
    pub model: String,
    pub usage: Tally,
}

/// A session's spend: totals, what made each call, each turn and each model.
#[derive(Clone, Debug, Default)]
pub struct UsageReport {
    /// Every call: billed ones in `calls` (priced, subscription, unpriced), and
    /// refused or unknown requests counted beside them.
    pub total: Tally,
    pub parent: Tally,
    pub nested: Tally,
    pub compaction: Tally,
    /// What the session's tasks (child sessions it ran) spent; in `total` too.
    pub tasks: Tally,
    /// Turns in the order they started, including ones a rewind left behind. Calls an
    /// older build counted without a turn are only in the totals.
    pub turns: Vec<TurnUsage>,
    pub models: Vec<ModelUsage>,
    /// Every counted call at its record-time price.
    pub calls: Vec<Call>,
    /// The most the requests whose outcome is unknown (or still in flight) could have
    /// cost: counted against a budget, never in the totals.
    pub uncertain_usd: f64,
    /// Parent effects sent with an unknown outcome (or still in flight).
    unanswered: BTreeSet<String>,
    /// Execution ids rewinds forked to, with the turn each continues.
    aliases: BTreeMap<String, String>,
    /// Turns by execution id.
    index: BTreeMap<String, usize>,
}

impl UsageReport {
    fn by_kind_mut(&mut self, kind: CallKind) -> &mut Tally {
        match kind {
            CallKind::Parent => &mut self.parent,
            CallKind::Nested => &mut self.nested,
            CallKind::Compaction => &mut self.compaction,
        }
    }

    /// Billed calls counted at or after `since` (Unix seconds). Calls counted without
    /// a date (an older or rebuilt `usage.json`) are only in the totals.
    pub fn since(&self, since: u64) -> Tally {
        let mut tally = Tally::default();
        for call in &self.calls {
            if call.at.is_some_and(|at| at >= since) {
                tally.add_call(call);
            }
        }
        tally
    }

    /// Totals with the parent, nested and compaction parts beside them.
    pub fn totals_json(&self) -> Value {
        let mut value = self.total.to_json();
        value["parent"] = self.parent.to_json();
        value["nested"] = self.nested.to_json();
        value["compaction"] = self.compaction.to_json();
        value["tasks"] = self.tasks.to_json();
        value
    }

    pub fn to_json(&self) -> Value {
        json!({
            "total": self.totals_json(),
            "turns": self.turns.iter().map(TurnUsage::to_json).collect::<Vec<_>>(),
            "models": self.models.iter().map(|m| {
                let mut value = m.usage.to_json();
                value["provider"] = json!(m.provider);
                value["model"] = json!(m.model);
                value
            }).collect::<Vec<_>>(),
        })
    }

    /// What a budget counts as spent: billed calls, and every request that may have
    /// been billed at its worst case.
    pub fn budget_spent(&self) -> f64 {
        self.total.usd + self.uncertain_usd
    }

    /// The turn (index) a call or request belongs to: a compaction made for a new
    /// prompt (`after` a revision) to the first turn started after it, none until that
    /// turn starts; anything else to its turn, a rewind's fork to the turn it continues.
    fn owner(&self, turn: Option<&str>, after: Option<u64>) -> Option<usize> {
        match after {
            Some(revision) => {
                let first = self.turns.partition_point(|turn| turn.revision <= revision);
                (first < self.turns.len()).then_some(first)
            }
            None => {
                let turn = turn?;
                let turn = self.aliases.get(turn).map_or(turn, String::as_str);
                self.index.get(turn).copied()
            }
        }
    }
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
    let identity = request.identity();
    let catalog = provider::load_provider_catalog().ok();
    let subscription = subscription_request(&identity.contract, request.body());
    let Some(cost) = worst_case(
        catalog.as_ref(),
        &identity.provider,
        &identity.model,
        &identity.contract,
        subscription,
        request.body(),
    ) else {
        return Ok(Some(unpriced_text(
            &identity.provider,
            &identity.model,
            max,
        )));
    };
    let read;
    let (report, reserved) = match report {
        Some(pair) => pair,
        None => {
            let reserved = super::tasks::reserved_usd(dir, None)?;
            read = report_with(dir, catalog.as_ref())?;
            (&read, reserved)
        }
    };
    Ok((report.budget_spent() + reserved + cost > max).then(|| {
        format!(
            "{} this thread has spent {} of its {} budget and the next model request could cost up to {}, so nothing was sent. Raise or clear the budget (thread settings, or `spindle continue --max-usd USD|none`), then Continue",
            crate::protocol::BUDGET_REFUSAL,
            spent_text(report),
            dollars(max),
            dollars(cost)
        )
    }))
}

/// What a thread spent, with what requests of unknown outcome may have added.
fn spent_text(report: &UsageReport) -> String {
    if report.uncertain_usd > 0.0 {
        format!(
            "{} (and up to {} more in requests whose outcome is unknown)",
            dollars(report.total.usd),
            dollars(report.uncertain_usd)
        )
    } else {
        dollars(report.total.usd)
    }
}

fn unpriced_text(provider: &str, model: &str, max: f64) -> String {
    format!(
        "this thread has a {} budget, but Dext's catalog has no price for {provider}/{model}, so its spend could not be counted; nothing was sent. Add the model's pricing to the provider catalog, choose a priced model, or clear the budget, then Continue",
        dollars(max)
    )
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
    let mut cost = 0.0;
    for child in prepared.iter().filter(|child| child["kind"] == "model") {
        let provider = child["provider"].as_str().unwrap_or_default();
        let model = child["model"].as_str().unwrap_or_default();
        let contract = child["contract"].as_str().unwrap_or_default();
        let subscription = contract == RequestContract::ChatGptResponses.as_str()
            || (contract == RequestContract::AnthropicMessages.as_str()
                && route_subscription(provider, model));
        let body = &child["body"];
        let worst = if subscription {
            Some(0.0)
        } else {
            worst_tokens(
                catalog.as_ref(),
                provider,
                model,
                contract,
                worst_input(body),
                allowed_output(body),
            )
        };
        match worst {
            Some(worst) => cost += worst,
            None => {
                return Err(unpriced_text(
                    provider,
                    model,
                    thread_cap.or(script_cap).unwrap_or_default(),
                ));
            }
        }
    }
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

/// The revisions of the branch that leads to `viewed`, as inclusive ranges. Walking
/// back from it: the newest rewind at or before the cursor keeps its own revision up
/// to the cursor, then the walk goes on from its target (a rewind may target any
/// committed revision, including one an earlier rewind left behind).
fn branch(viewed: u64, forks: &[(u64, u64, u64)]) -> Vec<(u64, u64)> {
    let mut ranges = vec![];
    let mut cursor = viewed;
    loop {
        let newest = forks
            .iter()
            .filter(|(fork, target, _)| *fork <= cursor && target < fork)
            .max_by_key(|(fork, _, _)| *fork);
        match newest {
            Some(&(fork, target, _)) => {
                ranges.push((fork, cursor));
                cursor = target;
            }
            None => {
                ranges.push((0, cursor));
                return ranges;
            }
        }
    }
}

/// Whether `revision` is on a branch (`branch`).
fn on_branch(revision: u64, branch: &[(u64, u64)]) -> bool {
    branch
        .iter()
        .any(|(first, last)| (*first..=*last).contains(&revision))
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
    let state = TurnContinuation::restore(&bytes).ok()?;
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
    let calls: Vec<&Call> = report
        .calls
        .iter()
        .filter(|call| match spent {
            driver::Counted::Reply(effect) => call.key == effect,
            driver::Counted::Script(attempt) => call
                .key
                .strip_prefix("code:")
                .and_then(|key| key.strip_prefix(attempt))
                .is_some_and(|rest| rest.starts_with('#')),
            driver::Counted::Summary(command) => call.key.strip_prefix("summary:") == Some(command),
        })
        .filter(|call| {
            matches!(
                call.billing,
                Billing::Priced | Billing::Subscription | Billing::Unpriced
            )
        })
        .collect();
    let first = calls.first()?;
    let billing = if calls.iter().all(|call| call.billing == first.billing) {
        first.billing.as_str()
    } else {
        "mixed"
    };
    let usd = calls.iter().map(|call| call.usd).sum();
    // The turn as the report attributes the call (a summary made for a new prompt
    // is that turn's first spend, before the turn has started).
    let turn = report
        .owner(first.turn.as_deref(), first.after)
        .map_or(usd, |turn| report.turns[turn].usage.usd);
    Some(driver::Cost {
        usd,
        turn,
        total: report.total.usd,
        billing: billing.into(),
    })
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
            Some(br#"{"content":"done","failed":false}"#),
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
