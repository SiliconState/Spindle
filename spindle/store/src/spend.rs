//! The ledger of model spend, shared by the native and embedded hosts: what each
//! billed call cost, read from a session's durable records through a `ReadView`
//! (see `agent::cost` for the native host's files, locks and budgets around it).
use crate::driver::{SUMMARY_PROMPT, SUMMARY_SOURCE, SUMMARY_STAGE, SUMMARY_SYSTEM, is_prompt};
use crate::shape::{MAX_OUTPUT_TOKENS, text_of};
use crate::view::{ReadView, SCOPE};
use dext_core::model::ModelWait;
use dext_provider_catalog::catalog::{
    self as provider, ModelPricing, ProviderCatalog, RequestContract,
};
use dext_transcript::Usage;
use dext_transcript::checkpoint::NativeResponse;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Failures here are said in words, as the hosts' own are.
type Result<T> = std::result::Result<T, String>;

/// Version of `usage.json`; version 1 kept only totals.
pub(crate) const LEDGER: u64 = 2;
/// ASCII bytes per token in a worst-case estimate (as the history budget assumes);
/// every other byte counts as a token of its own, as a byte-level tokenizer never
/// needs more.
pub(crate) const WORST_BYTES_PER_TOKEN: u64 = 3;
/// Most input tokens one inline image is estimated at, whatever its encoded size:
/// providers bill images by their pixels (Anthropic and OpenAI scale large ones down
/// to under 2,000 tokens; Gemini charges 258 per 768-pixel tile), never by base64 bytes.
pub(crate) const IMAGE_TOKENS: u64 = 10_000;
/// OpenAI's long-context tier: above this many input tokens a Responses model costs
/// twice the input and 1.5 times the output rates (Dext CLI `usage.rs`).
pub(crate) const LONG_CONTEXT_TOKENS: u64 = 272_000;
/// Longest prompt kept per turn in `usage.json`.
pub(crate) const PROMPT_CHARS: usize = 120;

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
    pub(crate) fn parse(text: &str) -> Option<Self> {
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
    pub(crate) fn parse(text: &str) -> Option<Self> {
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
    pub(crate) fn to_json(&self) -> Value {
        json!({"key": self.key, "kind": self.kind.as_str(), "turn": self.turn, "after": self.after,
            "provider": self.provider, "model": self.model, "billing": self.billing.as_str(),
            "input": self.usage.input, "output": self.usage.output,
            "cache_read": self.usage.cache_read, "cache_create": self.usage.cache_create,
            "usd": self.usd, "at": self.at, "count": self.count, "worst": self.worst})
    }
    /// None for an entry that cannot be read back (the ledger then counts again).
    pub(crate) fn from_json(value: &Value) -> Option<Self> {
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
pub(crate) fn valid_usd(usd: f64) -> bool {
    usd.is_finite() && usd >= 0.0
}

/// A turn: the prompt that started it and the revision its start committed.
#[derive(Clone, Debug)]
pub(crate) struct Started {
    pub(crate) turn: String,
    pub(crate) revision: u64,
    pub(crate) prompt: String,
}

/// `usage.json`: every counted call at its record-time price, and how far each durable
/// source has been read.
#[derive(Default)]
pub(crate) struct Ledger {
    /// Recorded replies read up to this receipt row, and how many rows that was.
    pub(crate) through: i64,
    pub(crate) counted: u64,
    /// Turn starts read up to this revision.
    pub(crate) turns_through: u64,
    pub(crate) turns: Vec<Started>,
    /// `run_code` attempts and summary stages whose calls are all final and counted.
    pub(crate) done: BTreeSet<String>,
    pub(crate) calls: Vec<Call>,
    pub(crate) keys: BTreeSet<String>,
    /// Calls counted now get no date: the ledger was lost or written by an older build.
    pub(crate) undated: bool,
    /// The store the ledger was counted from (`View::incarnation`).
    pub(crate) store: Option<String>,
    /// Rewinds read up to this fork revision, and the execution each one forked to,
    /// with the turn it continues.
    pub(crate) forks_through: u64,
    pub(crate) aliases: BTreeMap<String, String>,
    /// Worst cases of parent requests with an unknown outcome, by effect (their
    /// request is read once).
    pub(crate) estimates: BTreeMap<String, f64>,
}

impl Ledger {
    pub(crate) fn from_value(value: Option<Value>) -> Self {
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

    pub(crate) fn encoded(&self) -> Result<Vec<u8>> {
        let value = json!({"version": LEDGER, "through": self.through, "counted": self.counted,
            "turns_through": self.turns_through,
            "turns": self.turns.iter().map(|t| json!({"turn": t.turn, "revision": t.revision, "prompt": t.prompt})).collect::<Vec<_>>(),
            "done": self.done, "calls": self.calls.iter().map(Call::to_json).collect::<Vec<_>>(),
            "store": self.store, "forks_through": self.forks_through, "aliases": self.aliases,
            "estimates": self.estimates});
        serde_json::to_vec(&value).map_err(|error| error.to_string())
    }

    pub(crate) fn push(&mut self, call: Call) {
        if self.keys.insert(call.key.clone()) {
            self.calls.push(call);
        }
    }

    pub(crate) fn at(&self, now: u64) -> Option<u64> {
        (!self.undated).then_some(now)
    }

    /// Reads every durable source again; calls already counted keep their price, and
    /// what is counted now gets no date.
    pub(crate) fn recount(&mut self) {
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
    pub(crate) fn forget_store(&mut self) {
        self.calls.retain(|call| !from_store(&call.key));
        self.keys = self.calls.iter().map(|call| call.key.clone()).collect();
        self.recount();
    }

    /// Reads what the store recorded since the last update. Returns whether anything
    /// changed; `live` gets what is not final (unknown and refused parent requests,
    /// children and summaries still in flight).
    pub(crate) fn update(
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
                .and_then(|(bytes, _)| crate::turn::restored(&bytes).ok())
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

    pub(crate) fn report(&self, live: Live) -> UsageReport {
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
pub(crate) struct Live {
    pub(crate) refused: Vec<Pending>,
    pub(crate) unknown: Vec<Pending>,
    /// Parent effects sent with an unknown outcome (or in flight).
    pub(crate) unanswered: BTreeSet<String>,
    pub(crate) forks: Vec<(u64, u64, u64)>,
    pub(crate) head: u64,
}

/// A request counted beside the billed calls: refused, or of unknown outcome.
pub(crate) struct Pending {
    pub(crate) kind: CallKind,
    pub(crate) turn: Option<String>,
    /// As `Call::after`.
    pub(crate) after: Option<u64>,
    /// The most an unknown outcome could have cost.
    pub(crate) worst: f64,
}

/// The execution id of a committed revision's turn.
pub(crate) fn execution_at(
    view: &mut ReadView<'_>,
    revision: u64,
) -> crate::Result<Option<String>> {
    Ok(view
        .at(revision)?
        .and_then(|(bytes, _)| crate::turn::restored(&bytes).ok())
        .map(|state| state.execution().to_string()))
}

/// Whether a ledger key was counted from the store itself (not from a store set aside
/// by a restart, nor kept by an older ledger).
pub(crate) fn from_store(key: &str) -> bool {
    !key.starts_with(ASIDE) && !key.starts_with("summaries-before-ledger")
}

/// Key prefix of requests kept from a store a restart set aside.
pub(crate) const ASIDE: &str = "aside:";

/// One nested child's journal records.
#[derive(Default)]
pub(crate) struct Child {
    pub(crate) reserved: Option<Value>,
    pub(crate) result: Option<Value>,
    pub(crate) spend: Option<Value>,
}

impl Child {
    /// The reserved route, for model children only.
    pub(crate) fn prepared(&self) -> Option<&Value> {
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
pub(crate) struct Outcome {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) billing: Billing,
    pub(crate) usage: Usage,
    pub(crate) usd: f64,
}

impl Outcome {
    pub(crate) fn unknown(route: &Value) -> Self {
        Self {
            provider: route["provider"].as_str().unwrap_or_default().into(),
            model: route["model"].as_str().unwrap_or_default().into(),
            billing: Billing::Unknown,
            usage: Usage::default(),
            usd: 0.0,
        }
    }
    /// The call it was; `worst` is kept for an unknown outcome only.
    pub(crate) fn call(
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
pub(crate) fn outcome(
    catalog: Option<&ProviderCatalog>,
    route: &Value,
    response: &Value,
) -> Option<Outcome> {
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
pub(crate) fn tokens(usage: &Usage) -> Usage {
    Usage {
        cost_usd: None,
        ..*usage
    }
}

/// The turn (execution id) of an effect or attempt id (`[a:]<execution>/m/<n>...`).
pub(crate) fn turn_of(id: &str) -> String {
    id.trim_start_matches("a:")
        .split('/')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// The first line of `text`, at most `max` characters.
pub(crate) fn one_line(text: &str, max: usize) -> String {
    let line = text.trim().lines().next().unwrap_or_default();
    let mut short: String = line.chars().take(max).collect();
    if short.len() < line.len() || text.trim().lines().nth(1).is_some() {
        short.push('…');
    }
    short
}

/// Dollars in messages: to a hundredth of a cent under a dollar, else to the cent.
pub(crate) fn dollars(usd: f64) -> String {
    if usd < 1.0 {
        format!("${usd:.4}")
    } else {
        format!("${usd:.2}")
    }
}

/// Whether a recorded request went to a subscription login: the ChatGPT backend takes
/// nothing else, and an Anthropic subscription request carries Claude Code's billing
/// block first in its system text (`claude_subscription::transform_body`).
pub(crate) fn subscription_request(contract: &str, body: &[u8]) -> bool {
    const MARKER: &[u8] = b"\"system\":[{\"type\":\"text\",\"text\":\"x-anthropic-billing-header: ";
    contract == RequestContract::ChatGptResponses.as_str()
        || (contract == RequestContract::AnthropicMessages.as_str()
            && body.windows(MARKER.len()).any(|window| window == MARKER))
}

/// Whether a route sends through a subscription login now: the native host asks its
/// credentials (a Claude login); an app has none that this asks about.
pub(crate) fn route_subscription(provider: &str, model: &str) -> bool {
    #[cfg(feature = "agent")]
    return crate::agent::route_is_subscription(provider, model);
    #[cfg(not(feature = "agent"))]
    {
        let _ = (provider, model);
        false
    }
}

/// Dext's pricing overrides (`DEXT_*_USD_PER_MTOK`), as Dext CLI `usage.rs` applies
/// them to a catalog price.
pub(crate) fn env_price(name: &str) -> Option<f64> {
    override_price(std::env::var(name).ok().as_deref())
}

/// An override as Dext CLI's `env_f64` reads one: a finite, non-negative number
/// (anything else is ignored, as if unset).
pub(crate) fn override_price(raw: Option<&str>) -> Option<f64> {
    raw.and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|price| valid_usd(*price))
}

pub(crate) fn priced_by_env(pricing: &ModelPricing) -> (ModelPricing, bool) {
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
pub(crate) fn builtin_pricing(provider: &str, model: &str) -> Option<ModelPricing> {
    static BUILTIN: std::sync::OnceLock<ProviderCatalog> = std::sync::OnceLock::new();
    let catalog = BUILTIN.get_or_init(provider::default_provider_catalog);
    provider::find_provider_profile(catalog, &provider::canonical_provider_id(provider))
        .and_then(|profile| provider::resolve_model_spec(&profile, model).pricing)
}

/// Ported from Dext CLI `usage.rs` `openai_long_context_pricing_with_override_state`:
/// an OpenAI (or ChatGPT-route) Responses model whose input exceeds 272,000 tokens is
/// billed at twice the input/cache rates and 1.5 times the output rate, unless the
/// price was overridden (environment, or a catalog price that is not the built-in one).
pub(crate) fn long_context(
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
pub(crate) fn estimate_overrides_wire_cost(provider: &str, model: &str, contract: &str) -> bool {
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
pub(crate) fn price(
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
pub(crate) fn priced_at(rates: &ModelPricing, usage: &Usage) -> (Billing, f64) {
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
pub(crate) fn worst_case(
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
pub(crate) fn allowed_output(body: &Value) -> u64 {
    ["max_tokens", "max_output_tokens", "max_completion_tokens"]
        .iter()
        .find_map(|key| body[*key].as_u64())
        .unwrap_or(u64::from(MAX_OUTPUT_TOKENS) + 65_536)
}

/// Worst-case input tokens of a request body (as `worst_case` counts them). Strings a
/// journal keeps as digests (`{"sha256", "bytes"}`) count at their original size, as
/// if none of it were ASCII.
pub(crate) fn worst_input(body: &Value) -> u64 {
    #[derive(Default)]
    struct Size {
        ascii: u64,
        other: u64,
        images: u64,
    }
    pub(crate) fn digested(value: &Value) -> Option<u64> {
        let object = value.as_object()?;
        (object.len() == 2 && object.contains_key("sha256"))
            .then(|| object.get("bytes")?.as_u64())
            .flatten()
    }
    pub(crate) fn measure(value: &Value, image: bool, size: &mut Size) {
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
pub(crate) fn journal_worst(catalog: Option<&ProviderCatalog>, prepared: &Value) -> f64 {
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
pub(crate) fn summary_worst(catalog: Option<&ProviderCatalog>, reserved: &Value) -> f64 {
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
pub(crate) fn request_worst(
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
pub(crate) fn worst_tokens(
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
    pub(crate) fn add_call(&mut self, call: &Call) {
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
    pub(crate) fn note(&mut self, refused: bool) {
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
    pub(crate) fn by_kind_mut(&mut self, kind: CallKind) -> &mut Tally {
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
    pub(crate) unanswered: BTreeSet<String>,
    /// Execution ids rewinds forked to, with the turn each continues.
    pub(crate) aliases: BTreeMap<String, String>,
    /// Turns by execution id.
    pub(crate) index: BTreeMap<String, usize>,
}

impl UsageReport {
    pub(crate) fn by_kind_mut(&mut self, kind: CallKind) -> &mut Tally {
        match kind {
            CallKind::Parent => &mut self.parent,
            CallKind::Nested => &mut self.nested,
            CallKind::Compaction => &mut self.compaction,
        }
    }

    #[cfg(feature = "agent")]
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
    pub(crate) fn owner(&self, turn: Option<&str>, after: Option<u64>) -> Option<usize> {
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

/// Model usage and cost: billed calls with their token buckets and the cost of those
/// priced (`priced` of `calls`; `subscription` and `unpriced` are the rest), and
/// requests counted beside them: `refused` (not billed) and `unknown` (sent with an
/// unknown outcome: they may have been billed). See `usage_report`.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Tally {
    pub calls: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_create: u64,
    pub usd: f64,
    pub priced: u64,
    pub subscription: u64,
    pub unpriced: u64,
    pub refused: u64,
    pub unknown: u64,
}

impl Tally {
    /// The stable JSON form (docs/API.md, `usage`).
    pub fn to_json(&self) -> Value {
        json!({"calls": self.calls, "input": self.input, "output": self.output,
            "cache_read": self.cache_read, "cache_create": self.cache_create,
            "usd": self.usd, "priced": self.priced, "subscription": self.subscription,
            "unpriced": self.unpriced, "refused": self.refused, "unknown": self.unknown})
    }
    #[cfg(feature = "agent")]
    pub(crate) fn plus(&self, other: &Tally) -> Tally {
        Tally {
            calls: self.calls + other.calls,
            input: self.input + other.input,
            output: self.output + other.output,
            cache_read: self.cache_read + other.cache_read,
            cache_create: self.cache_create + other.cache_create,
            usd: self.usd + other.usd,
            priced: self.priced + other.priced,
            subscription: self.subscription + other.subscription,
            unpriced: self.unpriced + other.unpriced,
            refused: self.refused + other.refused,
            unknown: self.unknown + other.unknown,
        }
    }
}

/// The revisions of the branch that leads to `viewed`, as inclusive ranges. Walking
/// back from it: the newest rewind at or before the cursor keeps its own revision up
/// to the cursor, then the walk goes on from its target (a rewind may target any
/// committed revision, including one an earlier rewind left behind).
pub(crate) fn branch(viewed: u64, forks: &[(u64, u64, u64)]) -> Vec<(u64, u64)> {
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
pub(crate) fn on_branch(revision: u64, branch: &[(u64, u64)]) -> bool {
    branch
        .iter()
        .any(|(first, last)| (*first..=*last).contains(&revision))
}

/// Counts between the times a `HeldLedger` keeps its ledger in the store.
#[cfg(feature = "embedded")]
const SAVE_EVERY: u32 = 16;

/// A session's ledger held by the host that has its store open (an app counts each
/// reply as it comes): read from the store once, extended in memory, and kept in the
/// store every `SAVE_EVERY` counts and when asked (`save`, at a turn's end), not on
/// every count. What was not kept yet is counted again from the records by the next
/// reader, at the prices then; nothing is counted twice (calls are keyed).
#[cfg(feature = "embedded")]
#[derive(Default)]
pub(crate) struct HeldLedger(Option<(Ledger, u32)>);

#[cfg(feature = "embedded")]
impl HeldLedger {
    /// `report_on`, from the held ledger; `save`: keep it in the store now if it
    /// changed since it last was.
    pub(crate) fn report(
        &mut self,
        view: &mut ReadView<'_>,
        catalog: Option<&ProviderCatalog>,
        now: Option<u64>,
        save: bool,
    ) -> Result<UsageReport> {
        let (mut ledger, mut unsaved) = match self.0.take() {
            Some(held) => held,
            None => {
                let stored = view
                    .metadata("usage")
                    .map_err(|error| format!("{error:?}"))?;
                (
                    Ledger::from_value(
                        stored.and_then(|bytes| serde_json::from_slice(&bytes).ok()),
                    ),
                    0,
                )
            }
        };
        ledger.undated |= now.is_none();
        let mut live = Live::default();
        let changed = view
            .read(|view| ledger.update(view, catalog, now.unwrap_or(0), &mut live))
            .map_err(|error| format!("{error:?}"))?;
        unsaved += u32::from(changed);
        if unsaved > 0 && (save || unsaved >= SAVE_EVERY) {
            view.retain_metadata("usage", &ledger.encoded()?)
                .map_err(|error| format!("{error:?}"))?;
            unsaved = 0;
        }
        let report = ledger.report(live);
        self.0 = Some((ledger, unsaved));
        Ok(report)
    }

    /// Keeps `calls` of a store set aside (`aside` naming it) in this ledger, and the
    /// ledger in `view`'s store now.
    pub(crate) fn aside(
        &mut self,
        view: &mut ReadView<'_>,
        aside: &str,
        calls: Vec<Call>,
    ) -> Result<()> {
        let (mut ledger, _) = match self.0.take() {
            Some(held) => held,
            None => {
                let stored = view
                    .metadata("usage")
                    .map_err(|error| format!("{error:?}"))?;
                (
                    Ledger::from_value(
                        stored.and_then(|bytes| serde_json::from_slice(&bytes).ok()),
                    ),
                    0,
                )
            }
        };
        for mut call in calls {
            call.key = format!("{ASIDE}{aside}:{}", call.key);
            ledger.push(call);
        }
        view.retain_metadata("usage", &ledger.encoded()?)
            .map_err(|error| format!("{error:?}"))?;
        self.0 = Some((ledger, 0));
        Ok(())
    }

    /// Keeps the held ledger in the store if it counted calls since it last was.
    pub(crate) fn keep(&mut self, view: &mut ReadView<'_>) -> Result<()> {
        if let Some((ledger, unsaved)) = &mut self.0
            && *unsaved > 0
        {
            view.retain_metadata("usage", &ledger.encoded()?)
                .map_err(|error| format!("{error:?}"))?;
            *unsaved = 0;
        }
        Ok(())
    }
}

/// What a call the driver just counted (`spent`) cost, for its `cost` event: the
/// call's dollars, its turn's and the session's so far, and how it was paid.
pub(crate) fn counted_cost(
    report: &UsageReport,
    spent: crate::driver::Counted<'_>,
) -> Option<crate::driver::Cost> {
    let calls: Vec<&Call> = report
        .calls
        .iter()
        .filter(|call| match spent {
            crate::driver::Counted::Reply(effect) => call.key == effect,
            crate::driver::Counted::Script(attempt) => call
                .key
                .strip_prefix("code:")
                .and_then(|key| key.strip_prefix(attempt))
                .is_some_and(|rest| rest.starts_with('#')),
            crate::driver::Counted::Summary(command) => {
                call.key.strip_prefix("summary:") == Some(command)
            }
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
    Some(crate::driver::Cost {
        usd,
        turn,
        total: report.total.usd,
        billing: billing.into(),
    })
}

/// The worst case of a prepared model request (zero for a subscription login's), or
/// None when its model has no price.
pub(crate) fn prepared_worst(
    catalog: Option<&ProviderCatalog>,
    request: &dext_core::model::PreparedRequest<Vec<u8>>,
) -> Option<f64> {
    let identity = request.identity();
    let subscription = subscription_request(&identity.contract, request.body());
    worst_case(
        catalog,
        &identity.provider,
        &identity.model,
        &identity.contract,
        subscription,
        request.body(),
    )
}

/// The worst case of a model request in journal form (a nested call's or a compaction
/// summary's: `{provider, model, contract, body}`), or None when its model has no price.
pub(crate) fn journal_request_worst(
    catalog: Option<&ProviderCatalog>,
    prepared: &Value,
) -> Option<f64> {
    let provider = prepared["provider"].as_str().unwrap_or_default();
    let model = prepared["model"].as_str().unwrap_or_default();
    let contract = prepared["contract"].as_str().unwrap_or_default();
    let subscription = contract == RequestContract::ChatGptResponses.as_str()
        || (contract == RequestContract::AnthropicMessages.as_str()
            && route_subscription(provider, model));
    if subscription {
        return Some(0.0);
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
}

/// The worst case of a script's new nested model calls together, or the provider and
/// model of one with no price.
pub(crate) fn nested_worst(
    catalog: Option<&ProviderCatalog>,
    prepared: &[&Value],
) -> std::result::Result<f64, (String, String)> {
    let mut cost = 0.0;
    for child in prepared.iter().filter(|child| child["kind"] == "model") {
        match journal_request_worst(catalog, child) {
            Some(worst) => cost += worst,
            None => {
                let name = |key: &str| child[key].as_str().unwrap_or_default().to_string();
                return Err((name("provider"), name("model")));
            }
        }
    }
    Ok(cost)
}

/// The requests of unknown outcome (and refusals) a store set aside holds, as calls
/// the store taking its place keeps counted: unknown ones at their worst case.
#[cfg(feature = "embedded")]
pub(crate) fn aside_calls(
    view: &mut ReadView<'_>,
    catalog: Option<&ProviderCatalog>,
) -> crate::Result<Vec<Call>> {
    view.read(|view| {
        let mut calls = vec![];
        for (effect, status) in view.unanswered_model_attempts()? {
            let worst = match status {
                Some(_) => 0.0,
                None => request_worst(view, catalog, &effect)?,
            };
            calls.push(Call {
                key: effect.clone(),
                kind: CallKind::Parent,
                turn: Some(turn_of(&effect)),
                after: None,
                provider: String::new(),
                model: String::new(),
                billing: if status.is_some() {
                    Billing::Refused
                } else {
                    Billing::Unknown
                },
                usage: Usage::default(),
                usd: 0.0,
                at: None,
                count: 1,
                worst,
            });
        }
        Ok(calls)
    })
}

/// Why `request` is not sent under a `max` budget, given what `report` counts spent
/// and `reserved` (what else counts against the same budget: running tasks' leases on
/// a native host, the rest of the task tree in an app): its worst case would not fit,
/// or its model has no price to count it by.
pub(crate) fn refusal(
    catalog: Option<&ProviderCatalog>,
    max: f64,
    request: &dext_core::model::PreparedRequest<Vec<u8>>,
    report: &UsageReport,
    reserved: f64,
) -> Option<String> {
    let identity = request.identity();
    let Some(cost) = prepared_worst(catalog, request) else {
        return Some(unpriced_text(&identity.provider, &identity.model, max));
    };
    (report.budget_spent() + reserved + cost > max).then(|| {
        format!(
            "{} this thread has spent {} of its {} budget and the next model request could cost up to {}, so nothing was sent. Raise or clear the budget (thread settings, or `spindle continue --max-usd USD|none`), then Continue",
            crate::protocol::BUDGET_REFUSAL,
            spent_text(report),
            dollars(max),
            dollars(cost)
        )
    })
}

/// What a thread spent, with what requests of unknown outcome may have added.
pub(crate) fn spent_text(report: &UsageReport) -> String {
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

pub(crate) fn unpriced_text(provider: &str, model: &str, max: f64) -> String {
    format!(
        "this thread has a {} budget, but Dext's catalog has no price for {provider}/{model}, so its spend could not be counted; nothing was sent. Add the model's pricing to the provider catalog, choose a priced model, or clear the budget, then Continue",
        dollars(max)
    )
}
