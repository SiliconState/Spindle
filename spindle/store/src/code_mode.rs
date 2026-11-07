//! Bounded child effects of a reserved run_code attempt. Journal objects share the
//! store's retention and owner lock; they are not workspace files or transcript text.
use crate::artifact::Stage;
use crate::broker::DispatchRequest;
use crate::{Error, Result, Store};
use rusqlite::params;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Hard ceilings. Policy may lower these, never raise them.
pub use crate::artifact::MAX_CALLS;
pub const MAX_MODEL_CALLS: usize = 256;
pub const MAX_MODEL_BUDGET: u64 = 16 * 1024 * 1024;
/// Each journaled result is one store object (1 MiB), so a reply stays just below it.
pub const MAX_REPLY: usize = 1000 * 1024;
pub const MAX_PARALLEL: usize = 16;
pub const MAX_BATCH: usize = 128;
/// One prepared child (prompt, system text and inline images).
pub const MAX_PREPARED: usize = 24 * 1024 * 1024;
/// Journal strings above this are kept as a digest, so images and long prompts do not
/// fill the store; equal digests still prove a replayed child asks for the same thing.
const JOURNAL_INLINE: usize = 4096;

fn journal_form(value: &Value) -> Value {
    match value {
        Value::String(text) if text.len() > JOURNAL_INLINE => {
            let hash: String = Sha256::digest(text.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            json!({"sha256":hash,"bytes":text.len()})
        }
        Value::Array(items) => Value::Array(items.iter().map(journal_form).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), journal_form(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A script request's children, checked and prepared: each is reserved just before it
/// may run (`Store::code_reserve`) and its result sealed as it ends (`code_record`).
pub(crate) struct CodeChildren {
    /// The request's method (`model`, `tool`, `http` or `batch`): how its reply is shaped.
    pub method: String,
    first: u64,
    stages: Vec<Stage>,
    children: Vec<Value>,
    /// What each child runs, as the host prepared it.
    pub prepared: Vec<Value>,
    allowances: Vec<u64>,
}

/// What a script request comes to before anything runs.
pub(crate) enum CodePlan {
    /// An answer that runs nothing: discovery, limits, a replay or a refusal.
    Reply(Value),
    Children(CodeChildren),
}

/// A thread-safe executor for children that are already durably reserved.
pub type ParallelExecute = Arc<dyn Fn(&Value, &AtomicBool, Duration) -> Value + Send + Sync>;

/// Per-script limits resolved from `code-policy.json` `"limits"`, clamped to the ceilings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub calls: usize,
    pub model_calls: usize,
    pub model_budget: u64,
    pub parallel: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            calls: 32,
            model_calls: 8,
            model_budget: 32768,
            parallel: 1,
        }
    }
}

impl Limits {
    pub fn from_policy(policy: &Value) -> Self {
        let base = Self::default();
        let limits = &policy["limits"];
        let pick = |key: &str, default: u64, ceiling: u64| -> u64 {
            limits[key].as_u64().unwrap_or(default).clamp(1, ceiling)
        };
        Self {
            calls: pick("calls", base.calls as u64, MAX_CALLS as u64) as usize,
            model_calls: pick(
                "model_calls",
                base.model_calls as u64,
                MAX_MODEL_CALLS as u64,
            ) as usize,
            model_budget: pick("model_budget", base.model_budget, MAX_MODEL_BUDGET),
            parallel: pick("parallel", base.parallel as u64, MAX_PARALLEL as u64) as usize,
        }
    }
    pub fn to_json(self) -> Value {
        json!({"calls":self.calls,"model_calls":self.model_calls,"model_budget":self.model_budget,"parallel":self.parallel})
    }
}

/// A host prepares non-secret frozen inputs before reservation. Only execute may
/// perform external I/O; it must respect the supplied deadline and cancellation.
pub trait Host {
    fn discover(&mut self, method: &str, input: &Value) -> std::result::Result<Value, String>;
    fn prepare(&mut self, method: &str, input: &Value) -> std::result::Result<Value, String>;
    fn execute(&mut self, prepared: &Value, interrupt: &AtomicBool, remaining: Duration) -> Value;
    fn recorded(&mut self, _prepared: &Value, _private: &Value) {}
    fn sources(&self) -> Vec<std::path::PathBuf> {
        vec![]
    }
    fn limits(&self) -> Limits {
        Limits::default()
    }
    /// None keeps every child on `execute`, one at a time.
    fn parallel(&self) -> Option<ParallelExecute> {
        None
    }
    /// Dollar caps, checked before new children are reserved: `prepared` are the new
    /// children, `journal` the records of this script's earlier model calls (strings
    /// over 4 KiB as digests). An error refuses them all (nothing is reserved or sent).
    /// The default sets no dollar cap.
    fn admit_spend(
        &mut self,
        prepared: &[&Value],
        journal: &[&Value],
    ) -> std::result::Result<(), String> {
        let _ = (prepared, journal);
        Ok(())
    }
    /// Session-lifecycle packs (`pack_runtime`); None when the host binds none.
    #[cfg(feature = "dext-strict-workspace")]
    fn packs(&mut self) -> Option<&mut dyn crate::pack_runtime::PackHost> {
        None
    }
}

/// A recorded child result whose outcome the journal itself calls unknown (its worker
/// stopped, or its reply was too large to keep); journals written before the marker
/// existed are recognized by their text.
pub(crate) fn unknown_outcome(response: &Value) -> bool {
    response["unknown"] == true
        || matches!(
            response["error"].as_str(),
            Some(
                "child worker stopped; outcome unknown"
                    | "child reply exceeded 1000 KiB; inspect external effects"
            )
        )
}

fn failure(message: &str) -> Value {
    json!({"ok":false,"error":message})
}
/// The script's reply to a request whose children answered `responses`.
pub(crate) fn reply(method: &str, mut responses: Vec<Value>) -> Value {
    if method == "batch" {
        json!({"ok":true,"data":fit_batch(responses)})
    } else {
        responses
            .pop()
            .unwrap_or_else(|| failure("no child result"))
    }
}
/// Every batched child already ran and is journaled, so an oversized combined reply
/// drops the largest results (kept in code-receipts) instead of failing the batch.
fn fit_batch(mut responses: Vec<Value>) -> Vec<Value> {
    let size = |v: &Value| serde_json::to_vec(v).map_or(usize::MAX, |b| b.len());
    let mut sizes: Vec<usize> = responses.iter().map(size).collect();
    while sizes.iter().sum::<usize>() + responses.len() + 32 > MAX_REPLY {
        let Some((index, _)) = sizes
            .iter()
            .enumerate()
            .filter(|(i, _)| responses[*i]["omitted"] != true)
            .max_by_key(|(_, s)| **s)
        else {
            break;
        };
        responses[index] = json!({"ok":false,"may_have_run":true,"omitted":true,"error":"result omitted: the batch reply exceeded 1000 KiB; it ran and is kept in code-receipts; use smaller batches"});
        sizes[index] = size(&responses[index]);
    }
    responses
}
pub(crate) use crate::artifact::child_stage;

/// What `code_plan` checks of one script's journal, kept on this connection as
/// `artifact::usage` keeps its counts: a row per reserved child (whether its outcome is
/// known; a model call's records for dollar caps, strings over 4 KiB as digests), for
/// the script last asked about. It is read from the journal once per connection and
/// script, then kept in the transaction writing each record, so a script's nested
/// calls never reread its journal.
const TALLY: &str = "CREATE TEMP TABLE IF NOT EXISTS code_tallied(id INTEGER PRIMARY KEY CHECK(id=1), parent TEXT NOT NULL, epoch INTEGER NOT NULL, calls INTEGER NOT NULL);
     CREATE TEMP TABLE IF NOT EXISTS code_tally(id INTEGER PRIMARY KEY, model INTEGER NOT NULL, budget INTEGER NOT NULL, known INTEGER NOT NULL, reserved TEXT, result TEXT);";

/// Counts reserved child `id` of `parent` when the tally is that script's. Its budget is
/// held at most just past the ceiling: only how the sum compares with a cap matters.
fn tally_reserved(
    db: &rusqlite::Connection,
    parent: &str,
    epoch: u64,
    id: u64,
    record: &Value,
) -> Result<()> {
    db.execute_batch(TALLY)?;
    let model = record["request"]["method"] == "model";
    let budget = record["budget"]
        .as_u64()
        .unwrap_or(0)
        .min(MAX_MODEL_BUDGET + 1);
    db.prepare_cached("INSERT OR REPLACE INTO temp.code_tally SELECT ?1,?2,?3,0,?4,NULL FROM temp.code_tallied WHERE parent=?5 AND epoch=?6")?
        .execute(params![id as i64, model, budget as i64, model.then(|| record.to_string()), parent, epoch as i64])?;
    Ok(())
}

/// Settles child `id` of `parent` in the tally (when it is that script's): a result
/// recording an unknown outcome leaves it uncertain.
fn tally_result(
    db: &rusqlite::Connection,
    parent: &str,
    epoch: u64,
    id: u64,
    record: &Value,
) -> Result<()> {
    use rusqlite::OptionalExtension;
    db.execute_batch(TALLY)?;
    let Some(model) = db
        .prepare_cached("SELECT model FROM temp.code_tally WHERE id=?1 AND EXISTS(SELECT 1 FROM temp.code_tallied WHERE parent=?2 AND epoch=?3)")?
        .query_row(params![id as i64, parent, epoch as i64], |r| r.get::<_, bool>(0))
        .optional()?
    else {
        return Ok(());
    };
    db.prepare_cached("UPDATE temp.code_tally SET known=?2, result=?3 WHERE id=?1")?
        .execute(params![
            id as i64,
            !unknown_outcome(&record["response"]),
            model.then(|| journal_form(record).to_string())
        ])?;
    Ok(())
}

/// A script's journal as `code_plan` checks it: children reserved, model calls among
/// them, their summed budget and children whose outcome is not known.
struct Tally {
    used: usize,
    models: usize,
    budget: u64,
    uncertain: usize,
}

impl Store {
    /// The tally of `parent`'s children `0..calls`, read from its journal when this
    /// connection does not hold it yet.
    fn code_tally(&mut self, parent: &str, calls: usize) -> Result<Tally> {
        let epoch = self.epoch;
        self.db.execute_batch(TALLY)?;
        let held: bool = self
            .db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM temp.code_tallied WHERE parent=?1 AND epoch=?2 AND calls=?3)")?
            .query_row(params![parent, epoch as i64, calls as i64], |r| r.get(0))?;
        if !held {
            let journals = self.child_records(parent, 0..calls as u64)?;
            let tx = self.db.transaction()?;
            tx.execute_batch("DELETE FROM temp.code_tally; DELETE FROM temp.code_tallied;")?;
            tx.execute(
                "INSERT INTO temp.code_tallied VALUES(1,?1,?2,?3)",
                params![parent, epoch as i64, calls as i64],
            )?;
            for (id, records) in &journals {
                if let Some(reserved) = records.iter().find(|v| v["kind"] == "reserved") {
                    tally_reserved(&tx, parent, epoch, *id, reserved)?;
                    if let Some(result) = records.iter().find(|v| v["kind"] == "result") {
                        tally_result(&tx, parent, epoch, *id, result)?;
                    }
                }
            }
            tx.commit()?;
        }
        Ok(self.db.prepare_cached("SELECT count(*),coalesce(sum(model),0),coalesce(sum(budget),0),coalesce(sum(1-known),0) FROM temp.code_tally")?
            .query_row([], |r| {
                Ok(Tally {
                    used: r.get::<_, i64>(0)? as usize,
                    models: r.get::<_, i64>(1)? as usize,
                    budget: r.get::<_, i64>(2)? as u64,
                    uncertain: r.get::<_, i64>(3)? as usize,
                })
            })?)
    }

    /// The tallied script's model calls, as journaled (reservation, then any result).
    fn code_tally_models(&self) -> Result<Vec<Value>> {
        let mut query = self.db.prepare_cached(
            "SELECT reserved,result FROM temp.code_tally WHERE model ORDER BY id",
        )?;
        let rows = query.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        let mut records = vec![];
        for row in rows {
            let (reserved, result) = row?;
            for text in std::iter::once(reserved).chain(result) {
                records.push(serde_json::from_str(&text).map_err(|_| Error::Incompatible)?);
            }
        }
        Ok(records)
    }

    /// Journal records of children `ids` of `parent`, by child id, in one query.
    fn child_records(
        &self,
        parent: &str,
        ids: std::ops::Range<u64>,
    ) -> Result<BTreeMap<u64, Vec<Value>>> {
        let stages: BTreeMap<String, u64> = ids.map(|id| (child_stage(parent, id), id)).collect();
        let mut records: BTreeMap<u64, Vec<Value>> = BTreeMap::new();
        if stages.is_empty() {
            return Ok(records);
        }
        let mut query = self.db.prepare(&format!(
            "SELECT p.stage,o.bytes,o.size,o.digest FROM objects o JOIN staging_pins p ON p.digest=o.digest WHERE p.stage IN ({})",
            vec!["?"; stages.len()].join(",")
        ))?;
        let rows = query.query_map(rusqlite::params_from_iter(stages.keys()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                crate::artifact::checked_at(r, 3, 1)?,
            ))
        })?;
        for row in rows {
            let (stage, bytes) = row?;
            let record = serde_json::from_slice(&bytes).map_err(|_| Error::Incompatible)?;
            records
                .entry(*stages.get(&stage).ok_or(Error::Incompatible)?)
                .or_default()
                .push(record);
        }
        Ok(records)
    }

    /// Replies to one request line. The script's `seq` is echoed so it can match each
    /// reply to its own request; it is not part of the journaled request. (Native code
    /// mode, whose host runs children itself; the embedded package uses the parts.)
    #[cfg(feature = "dext-strict-workspace")]
    pub(crate) fn code_request(
        &mut self,
        parent: &DispatchRequest,
        bytes: &[u8],
        host: &mut dyn Host,
        interrupt: &AtomicBool,
        remaining: Duration,
    ) -> Vec<u8> {
        let ran = std::cell::Cell::new(false);
        let mut seq = None;
        let result = match serde_json::from_slice::<Value>(bytes) {
            Err(_) => failure("invalid code request JSON"),
            Ok(mut request) => {
                seq = request
                    .as_object_mut()
                    .and_then(|object| object.remove("seq"));
                match self.code_request_inner(parent, &request, host, interrupt, remaining, &ran) {
                    Ok(value) => value,
                    Err(error) => self.code_failure(&error, ran.get()),
                }
            }
        };
        let mut result = if serde_json::to_vec(&result).map_or(true, |v| v.len() > MAX_REPLY) {
            failure("code reply exceeds 1000 KiB; narrow the discovery query")
        } else {
            result
        };
        if let (Some(seq), Some(object)) = (seq, result.as_object_mut()) {
            object.insert("seq".into(), seq);
        }
        serde_json::to_vec(&result)
            .unwrap_or_else(|_| b"{\"ok\":false,\"error\":\"invalid child result\"}".to_vec())
    }

    /// Checks a script request and prepares its children, reserving nothing: an answer
    /// that runs nothing (discovery, limits, a replay, a refusal), or the children to
    /// reserve (`code_reserve`), run and record (`code_record`) one by one.
    pub(crate) fn code_plan(
        &mut self,
        parent: &DispatchRequest,
        request: &Value,
        host: &mut dyn Host,
        interrupt: &AtomicBool,
        out_of_time: &dyn Fn() -> bool,
    ) -> Result<CodePlan> {
        if self.paused
            || parent.owner_epoch != self.epoch
            || interrupt.load(Ordering::SeqCst)
            || out_of_time()
        {
            return Ok(CodePlan::Reply(failure("code execution is stopped")));
        }
        let Some(method) = request["method"].as_str() else {
            return Ok(CodePlan::Reply(failure("method required")));
        };
        let input = &request["input"];
        if matches!(method, "search" | "describe") {
            #[cfg(feature = "dext-strict-workspace")]
            self.refresh_packs(parent, host)?;
            return Ok(CodePlan::Reply(match host.discover(method, input) {
                Ok(data) => json!({"ok":true,"data":data}),
                Err(error) => failure(&error),
            }));
        }
        if method == "limits" {
            return Ok(CodePlan::Reply(
                json!({"ok":true,"data":host.limits().to_json()}),
            ));
        }
        let calls: Vec<(&str, &Value)> = match method {
            "model" | "tool" | "http" => vec![(method, input)],
            "batch" => {
                let Some(list) = input["calls"]
                    .as_array()
                    .filter(|list| (1..=MAX_BATCH).contains(&list.len()))
                else {
                    return Ok(CodePlan::Reply(failure(&format!(
                        "batch needs 1..{MAX_BATCH} calls"
                    ))));
                };
                let mut calls = Vec::with_capacity(list.len());
                for call in list {
                    match call["method"].as_str() {
                        Some(name @ ("model" | "tool" | "http")) => {
                            calls.push((name, &call["input"]))
                        }
                        _ => {
                            return Ok(CodePlan::Reply(failure(
                                "batch calls must be model, tool or http",
                            )));
                        }
                    }
                }
                calls
            }
            _ => return Ok(CodePlan::Reply(failure("unknown code method"))),
        };
        let limits = host.limits();
        let Some(first) = request["id"]
            .as_u64()
            .filter(|id| id.saturating_add(calls.len() as u64) <= limits.calls as u64)
        else {
            return Ok(CodePlan::Reply(failure(&format!(
                "child ids must stay within 0..{}",
                limits.calls - 1
            ))));
        };
        // Batched children journal the same request shape as a serial call, so either
        // form replays the other's receipt.
        let children: Vec<Value> = if method == "batch" {
            calls
                .iter()
                .zip(first..)
                .map(|((name, input), id)| {
                    journal_form(&json!({"id":id,"method":name,"input":input}))
                })
                .collect()
        } else {
            vec![journal_form(request)]
        };
        let journals =
            self.child_records(&parent.attempt.id, first..first + children.len() as u64)?;
        let mut stages = Vec::with_capacity(children.len());
        let mut replayed = Vec::new();
        for (child, id) in children.iter().zip(first..) {
            let stage = Stage {
                id: child_stage(&parent.attempt.id, id),
                scope: parent.scope.clone(),
                owner_epoch: self.epoch,
            };
            let records = journals.get(&id).map(Vec::as_slice).unwrap_or_default();
            if let Some(reserved) = records.iter().find(|v| v["kind"] == "reserved") {
                // journal_form is idempotent, so journals written before digesting still match.
                if journal_form(&reserved["request"]) != *child {
                    return Ok(CodePlan::Reply(failure(
                        "child id reused with different input",
                    )));
                }
                replayed.push(records.iter().find(|v| v["kind"] == "result").map(|v| v["response"].clone())
                    .unwrap_or_else(|| json!({"ok":false,"may_have_run":true,"error":"child outcome unknown; inspect effects before an explicit new attempt"})));
            }
            stages.push(stage);
        }
        if replayed.len() == children.len() {
            return Ok(CodePlan::Reply(reply(method, replayed)));
        }
        if !replayed.is_empty() {
            return Ok(CodePlan::Reply(
                json!({"ok":false,"may_have_run":true,"error":"batch is partially reserved; inspect receipts before an explicit new attempt"}),
            ));
        }
        let Tally {
            used,
            models: models_used,
            mut budget,
            uncertain,
        } = self.code_tally(&parent.attempt.id, limits.calls)?;
        // A reservation without a recorded result is uncertain, as is a result that
        // itself records an unknown outcome; a recorded failure (a refused request, an
        // exit status) is a known outcome.
        if uncertain > 0 {
            return Ok(CodePlan::Reply(
                json!({"ok":false,"may_have_run":true,"error":"an earlier child outcome is uncertain; inspect receipts before starting new external work"}),
            ));
        }
        let new_models = calls.iter().filter(|(name, _)| *name == "model").count();
        if used + calls.len() > limits.calls || models_used + new_models > limits.model_calls {
            return Ok(CodePlan::Reply(failure("child call cap reached")));
        }
        #[cfg(feature = "dext-strict-workspace")]
        self.refresh_packs(parent, host)?;
        let mut prepared = Vec::with_capacity(calls.len());
        for (index, (name, input)) in calls.iter().enumerate() {
            let mut value = match host.prepare(name, input) {
                Ok(value) => value,
                Err(error) => return Ok(CodePlan::Reply(failure(&error))),
            };
            // An app tool call is named to the app by its journal stage: stable for
            // as long as the child exists, as the effect id names a model's call.
            if value["kind"] == "app" {
                value["call"] = json!(stages[index].id);
            }
            if serde_json::to_vec(&value)
                .map_err(|_| Error::Invalid)?
                .len()
                > MAX_PREPARED
            {
                return Ok(CodePlan::Reply(failure(&format!(
                    "prepared child input exceeds {} MiB",
                    MAX_PREPARED >> 20
                ))));
            }
            let allowance = value["budget"].as_u64().unwrap_or(0);
            budget = budget.saturating_add(allowance);
            if *name == "model" && (allowance == 0 || budget > limits.model_budget) {
                return Ok(CodePlan::Reply(failure(&format!(
                    "nested model budget exhausted ({} input-byte + requested-output-token allowance)",
                    limits.model_budget
                ))));
            }
            prepared.push((value, allowance));
        }
        if new_models > 0 {
            let recorded = self.code_tally_models()?;
            let recorded: Vec<&Value> = recorded.iter().collect();
            let fresh: Vec<&Value> = prepared.iter().map(|(value, _)| value).collect();
            if let Err(error) = host.admit_spend(&fresh, &recorded) {
                return Ok(CodePlan::Reply(failure(&error)));
            }
        }
        if interrupt.load(Ordering::SeqCst) || out_of_time() {
            return Ok(CodePlan::Reply(failure(
                "code execution stopped before reservation",
            )));
        }
        let (prepared, allowances) = prepared.into_iter().unzip();
        Ok(CodePlan::Children(CodeChildren {
            method: method.to_string(),
            first,
            stages,
            children,
            prepared,
            allowances,
        }))
    }

    /// A script's reply when the journal itself failed: a store that cannot write is
    /// paused, and the script told whether a child may already have run.
    pub(crate) fn code_failure(&mut self, error: &Error, ran: bool) -> Value {
        if matches!(
            error,
            Error::Sql(_) | Error::Io(_) | Error::Locked | Error::Incompatible
        ) {
            self.paused = true;
        }
        if ran {
            json!({"ok":false,"may_have_run":true,"error":"child journal failed; outcome may be unknown; no automatic retry"})
        } else {
            json!({"ok":false,"may_have_run":false,"error":"child journal refused the reservation; nothing ran"})
        }
    }

    /// Durably reserves child `index` of `plan`, just before it may run: after a crash
    /// it is uncertain, and never sent again by itself.
    pub(crate) fn code_reserve(
        &mut self,
        parent: &DispatchRequest,
        plan: &CodeChildren,
        index: usize,
    ) -> Result<()> {
        self.begin_stage(&plan.stages[index])?;
        let record = json!({"kind":"reserved","parent":parent.attempt.id,"request":plan.children[index],"prepared":journal_form(&plan.prepared[index]),"budget":plan.allowances[index]});
        let (epoch, id) = (self.epoch, plan.first + index as u64);
        self.put_object_checked(
            &plan.stages[index],
            &serde_json::to_vec(&record).map_err(|_| Error::Invalid)?,
            false,
            |tx, _| tally_reserved(tx, &parent.attempt.id, epoch, id, &record),
        )?;
        Ok(())
    }

    /// Seals the result of reserved child `index` of `plan`: what the script is told.
    pub(crate) fn code_record(
        &mut self,
        parent: &DispatchRequest,
        plan: &CodeChildren,
        index: usize,
        response: Value,
    ) -> Result<Value> {
        self.journal_result(
            &plan.stages[index],
            parent,
            plan.first + index as u64,
            response,
        )
    }

    #[cfg(feature = "dext-strict-workspace")]
    fn code_request_inner(
        &mut self,
        parent: &DispatchRequest,
        request: &Value,
        host: &mut dyn Host,
        interrupt: &AtomicBool,
        remaining: Duration,
        ran: &std::cell::Cell<bool>,
    ) -> Result<Value> {
        let started = std::time::Instant::now();
        let out_of_time = || started.elapsed() >= remaining;
        let plan = match self.code_plan(parent, request, host, interrupt, &out_of_time)? {
            CodePlan::Reply(reply) => return Ok(reply),
            CodePlan::Children(plan) => plan,
        };
        let limits = host.limits();
        // Each chunk is durably reserved just before the host may send/spawn it, so after a
        // crash only the chunk that was running is uncertain; later chunks stay unreserved and
        // a replay of the batch is refused as partially reserved.
        // Session packs hold state, and the app answers one call at a time, so their
        // calls always run one at a time.
        let parallel = host.parallel().filter(|_| {
            limits.parallel > 1
                && plan.prepared.len() > 1
                && !plan
                    .prepared
                    .iter()
                    .any(|value| value["kind"] == "pack" || value["kind"] == "app")
        });
        let width = if parallel.is_some() {
            limits.parallel
        } else {
            1
        };
        let mut responses = Vec::with_capacity(plan.prepared.len());
        for (chunk_index, chunk) in plan.prepared.chunks(width).enumerate() {
            let first_index = chunk_index * width;
            if chunk_index > 0
                && (interrupt.load(Ordering::SeqCst) || started.elapsed() >= remaining)
            {
                responses.extend(chunk.iter().map(|_| {
                    json!({"ok":false,"may_have_run":false,"error":"code execution stopped before this call was dispatched"})
                }));
                continue;
            }
            for offset in 0..chunk.len() {
                self.code_reserve(parent, &plan, first_index + offset)?;
            }
            ran.set(true);
            let left = remaining.saturating_sub(started.elapsed());
            let mut results = vec![Value::Null; chunk.len()];
            match &parallel {
                // Each child's result is journaled as soon as its worker finishes, so a
                // crash leaves only the children still running uncertain.
                Some(execute) => std::thread::scope(|scope| -> Result<()> {
                    let (done, finished) = std::sync::mpsc::channel();
                    for (offset, value) in chunk.iter().enumerate() {
                        let (done, execute) = (done.clone(), execute.clone());
                        scope.spawn(move || {
                            let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                execute(value, interrupt, left)
                            }))
                            .unwrap_or_else(|_| {
                                json!({"ok":false,"may_have_run":true,"unknown":true,"error":"child worker stopped; outcome unknown"})
                            });
                            let _ = done.send((offset, response));
                        });
                    }
                    drop(done);
                    for (offset, response) in finished {
                        results[offset] =
                            self.code_record(parent, &plan, first_index + offset, response)?;
                    }
                    Ok(())
                })?,
                None => {
                    for (offset, value) in chunk.iter().enumerate() {
                        let index = first_index + offset;
                        #[cfg(feature = "dext-strict-workspace")]
                        let response = match host.packs() {
                            Some(packs) if value["kind"] == "pack" => self.run_pack_child(
                                parent,
                                &plan.stages[index],
                                plan.first + index as u64,
                                value,
                                packs,
                                interrupt,
                                left,
                            )?,
                            _ => host.execute(value, interrupt, left),
                        };
                        #[cfg(not(feature = "dext-strict-workspace"))]
                        let response = host.execute(value, interrupt, left);
                        let private = response.get("_spindle_app").cloned().unwrap_or(Value::Null);
                        results[offset] = self.code_record(parent, &plan, index, response)?;
                        host.recorded(value, &private);
                    }
                }
            }
            responses.extend(results);
        }
        Ok(reply(&plan.method, responses))
    }

    /// Seals one child's result in its journal.
    fn journal_result(
        &mut self,
        stage: &Stage,
        parent: &DispatchRequest,
        id: u64,
        mut response: Value,
    ) -> Result<Value> {
        let mut private = response
            .as_object_mut()
            .and_then(|reply| reply.remove("_spindle_app"))
            .filter(|private| !private.is_null());
        if let Some(private) = private.as_mut() {
            let cap = private["max_output"]
                .as_u64()
                .filter(|cap| (1..=1048576).contains(cap))
                .ok_or(Error::Invalid)? as usize;
            if private
                .get("details")
                .is_some_and(|details| details.to_string().len() > crate::transition::MAX_STATE)
            {
                return Err(Error::Invalid);
            }
            let key = if response["data"].is_string() {
                "data"
            } else {
                "error"
            };
            if let Some(content) = response[key].as_str().filter(|content| content.len() > cap) {
                let full = content.to_string();
                let mut end = cap;
                while !full.is_char_boundary(end) {
                    end -= 1;
                }
                response[key] = json!(&full[..end]);
                if key == "data" && response["ok"] == false {
                    response["error"] = response["data"].clone();
                }
                private["output"] = json!(full);
            }
        }
        if serde_json::to_vec(&response)
            .map_err(|_| Error::Invalid)?
            .len()
            > MAX_REPLY
        {
            response = json!({"ok":false,"may_have_run":true,"unknown":true,"error":"child reply exceeded 1000 KiB; inspect external effects"});
        }
        let mut record = json!({"kind":"result","parent":parent.attempt.id,"id":id,"response":response,"private":private});
        let epoch = self.epoch;
        self.put_object_checked(
            stage,
            &serde_json::to_vec(&record).map_err(|_| Error::Invalid)?,
            false,
            |tx, _| tally_result(tx, &parent.attempt.id, epoch, id, &record),
        )?;
        Ok(record["response"].take())
    }

    /// Operator inspection; this never resumes or retries child effects.
    pub fn code_receipts(&mut self, scope: &str) -> Result<Vec<Value>> {
        let mut query = self.db.prepare("SELECT o.bytes,o.size,o.digest FROM objects o JOIN staging_pins p ON p.digest=o.digest JOIN artifact_stages s ON s.id=p.stage WHERE s.scope=?1 AND substr(s.id,1,5)='code-' ORDER BY s.id,o.digest")?;
        let rows = query.query_map(params![scope], |r| crate::artifact::checked_at(r, 2, 0))?;
        rows.map(|bytes| serde_json::from_slice(&bytes?).map_err(|_| Error::Incompatible))
            .collect()
    }
}

// Fail closed on unsupported schema keywords instead of deleting constraints.
pub(crate) fn validate_input(
    schema: &Value,
    input: &Value,
    depth: usize,
) -> std::result::Result<(), String> {
    if depth > 16 {
        return Err("schema nesting limit".into());
    }
    let object = schema.as_object().ok_or("input schema must be an object")?;
    for key in object.keys() {
        if ![
            "type",
            "properties",
            "required",
            "additionalProperties",
            "items",
            "enum",
            "description",
            "title",
            "default",
            "minimum",
            "maximum",
            "minLength",
            "maxLength",
            "minItems",
            "maxItems",
        ]
        .contains(&key.as_str())
        {
            return Err(format!(
                "unsupported schema keyword {key}; use a validating proxy binding"
            ));
        }
    }
    let valid = match schema["type"].as_str() {
        Some("object") => input.is_object(),
        Some("array") => input.is_array(),
        Some("string") => input.is_string(),
        Some("integer") => input.as_i64().is_some() || input.as_u64().is_some(),
        Some("number") => input.is_number(),
        Some("boolean") => input.is_boolean(),
        Some("null") => input.is_null(),
        _ => false,
    };
    if !valid {
        return Err("tool input has the wrong type".into());
    }
    if let Some(values) = schema["enum"].as_array()
        && !values.contains(input)
    {
        return Err("value is not in schema enum".into());
    }
    if let Some(values) = input.as_object() {
        for required in schema["required"].as_array().into_iter().flatten() {
            if !values.contains_key(required.as_str().ok_or("invalid required field")?) {
                return Err("required tool argument missing".into());
            }
        }
        for (key, value) in values {
            if let Some(child) = schema["properties"].get(key) {
                validate_input(child, value, depth + 1)?;
            } else if schema["additionalProperties"] != true {
                return Err("unknown tool argument".into());
            }
        }
    }
    if let Some(values) = input.as_array() {
        for value in values {
            validate_input(&schema["items"], value, depth + 1)?;
        }
        bounds(schema, "minItems", "maxItems", values.len() as f64)?;
    }
    if let Some(value) = input.as_str() {
        bounds(
            schema,
            "minLength",
            "maxLength",
            value.chars().count() as f64,
        )?;
    }
    if let Some(value) = input.as_f64() {
        bounds(schema, "minimum", "maximum", value)?;
    }
    Ok(())
}

fn bounds(schema: &Value, min: &str, max: &str, value: f64) -> std::result::Result<(), String> {
    if schema[min].as_f64().is_some_and(|n| value < n)
        || schema[max].as_f64().is_some_and(|n| value > n)
    {
        Err("tool argument outside schema bounds".into())
    } else {
        Ok(())
    }
}

// Native code mode's path; the embedded package's is tested in tests/embedded.rs.
#[cfg(all(test, feature = "dext-strict-workspace"))]
mod tests {
    use super::*;
    use crate::Command;
    use crate::completion::Attempt;
    use crate::transition::EffectIntent;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        calls: usize,
        budget: u64,
        panic: bool,
    }
    impl Host for Fixture {
        fn discover(&mut self, _: &str, _: &Value) -> std::result::Result<Value, String> {
            Ok(json!([]))
        }
        fn prepare(&mut self, method: &str, input: &Value) -> std::result::Result<Value, String> {
            Ok(json!({"method":method,"input":input,"budget":self.budget}))
        }
        fn execute(&mut self, _: &Value, _: &AtomicBool, _: Duration) -> Value {
            self.calls += 1;
            assert!(!self.panic, "injected owner stop after send");
            json!({"ok":true,"data":"recorded answer"})
        }
    }
    fn fixture() -> (std::path::PathBuf, Store, DispatchRequest) {
        let root = std::env::temp_dir().join(format!(
            "spindle-code-journal-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let mut store = Store::open(&root).unwrap();
        store
            .admit(&Command {
                scope: "operator".into(),
                id: "fixture".into(),
                lineage: 0,
                kind: "fixture".into(),
                payload: vec![],
            })
            .unwrap();
        let parent = DispatchRequest {
            scope: "operator".into(),
            owner_epoch: store.epoch(),
            expected_revision: 0,
            intent_revision: 0,
            intent: EffectIntent {
                id: "effect".into(),
                adapter: "run_code".into(),
                input: vec![],
            },
            attempt: Attempt {
                id: "parent".into(),
                effect: "effect".into(),
                owner_epoch: store.epoch(),
                implementation: "fixture".into(),
                grant_reference: "fixture".into(),
            },
        };
        (root, store, parent)
    }
    fn call(
        store: &mut Store,
        parent: &DispatchRequest,
        host: &mut Fixture,
        id: u64,
        method: &str,
    ) -> Value {
        let bytes = serde_json::to_vec(&json!({"id":id,"method":method,"input":{"prompt":"test"}}))
            .unwrap();
        serde_json::from_slice(&store.code_request(
            parent,
            &bytes,
            host,
            &AtomicBool::new(false),
            Duration::from_secs(10),
        ))
        .unwrap()
    }
    #[test]
    fn app_child_private_details_and_full_output_are_retained_but_never_replied() {
        let (root, mut store, parent) = fixture();
        let stage = Stage {
            id: child_stage(&parent.attempt.id, 0),
            scope: parent.scope.clone(),
            owner_epoch: store.epoch(),
        };
        store.begin_stage(&stage).unwrap();
        let full = "é".repeat(1000);
        let reply = store.journal_result(&stage,&parent,0,json!({"ok":true,"data":full,"_spindle_app":{"max_output":65,"details":{"private":"receipt-only"}}})).unwrap();
        assert_eq!(reply["data"].as_str().unwrap().len(), 64);
        assert!(!reply.to_string().contains("receipt-only"));
        assert!(reply.get("_spindle_app").is_none());
        drop(store);
        let mut store = Store::open(&root).unwrap();
        let records = store.code_receipts("operator").unwrap();
        assert_eq!(records[0]["private"]["details"]["private"], "receipt-only");
        assert_eq!(records[0]["private"]["output"], full);
        assert_eq!(records[0]["response"], reply);
        let error_stage = Stage {
            id: child_stage(&parent.attempt.id, 1),
            owner_epoch: store.epoch(),
            ..stage
        };
        store.begin_stage(&error_stage).unwrap();
        let error = store
            .journal_result(
                &error_stage,
                &parent,
                1,
                json!({"ok":false,"unknown":true,"error":full,"_spindle_app":{"max_output":65}}),
            )
            .unwrap();
        assert_eq!(error["error"].as_str().unwrap().len(), 64);
        assert_eq!(error["unknown"], true);
        assert!(
            store
                .code_receipts("operator")
                .unwrap()
                .iter()
                .any(|record| record["id"] == 1 && record["private"]["output"] == full)
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn journal_reuses_results_rejects_conflicting_ids_and_survives_reopen() {
        let (root, mut store, mut parent) = fixture();
        let mut host = Fixture {
            calls: 0,
            budget: 100,
            panic: false,
        };
        let first = call(&mut store, &parent, &mut host, 0, "model");
        assert_eq!(first["ok"], true);
        assert_eq!(first, call(&mut store, &parent, &mut host, 0, "model"));
        assert_eq!(host.calls, 1);
        assert_eq!(call(&mut store, &parent, &mut host, 0, "tool")["ok"], false);
        drop(store);
        let mut store = Store::open(&root).unwrap();
        parent.owner_epoch = store.epoch();
        assert_eq!(first, call(&mut store, &parent, &mut host, 0, "model"));
        assert_eq!(host.calls, 1);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn unknown_child_is_not_resent_and_blocks_following_effects() {
        let (root, mut store, mut parent) = fixture();
        let mut host = Fixture {
            calls: 0,
            budget: 100,
            panic: true,
        };
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| call(
                &mut store, &parent, &mut host, 0, "model"
            )))
            .is_err()
        );
        drop(store);
        let mut store = Store::open(&root).unwrap();
        parent.owner_epoch = store.epoch();
        host.panic = false;
        assert_eq!(
            call(&mut store, &parent, &mut host, 0, "model")["may_have_run"],
            true
        );
        assert_eq!(
            call(&mut store, &parent, &mut host, 1, "tool")["may_have_run"],
            true
        );
        assert_eq!(host.calls, 1);
        assert_eq!(store.code_receipts("operator").unwrap().len(), 1);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn child_caps_reserve_worst_case_budget_before_execution() {
        let (root, mut store, parent) = fixture();
        let mut host = Fixture {
            calls: 0,
            budget: 16384,
            panic: false,
        };
        assert_eq!(call(&mut store, &parent, &mut host, 0, "model")["ok"], true);
        assert_eq!(call(&mut store, &parent, &mut host, 1, "model")["ok"], true);
        assert_eq!(
            call(&mut store, &parent, &mut host, 2, "model")["ok"],
            false
        );
        assert_eq!(host.calls, 2);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
        let (root, mut store, parent) = fixture();
        host.budget = 1;
        host.calls = 0;
        let cap = Limits::default().model_calls;
        for n in 0..cap as u64 {
            assert_eq!(call(&mut store, &parent, &mut host, n, "model")["ok"], true);
        }
        assert_eq!(
            call(&mut store, &parent, &mut host, cap as u64, "model")["ok"],
            false
        );
        assert_eq!(host.calls, cap);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The caps refuse at the same count across a reopen (the tally is read from the
    /// journal once), and a connection's later requests never reread the journal.
    #[test]
    fn journal_tally_is_read_once_per_connection_with_the_same_caps() {
        let (root, mut store, mut parent) = fixture();
        let mut host = Fixture {
            calls: 0,
            budget: 1,
            panic: false,
        };
        let cap = Limits::default().model_calls as u64;
        for n in 0..cap / 2 {
            assert_eq!(call(&mut store, &parent, &mut host, n, "model")["ok"], true);
        }
        drop(store);
        let mut store = Store::open(&root).unwrap();
        parent.owner_epoch = store.epoch();
        for n in cap / 2..cap {
            assert_eq!(call(&mut store, &parent, &mut host, n, "model")["ok"], true);
        }
        let refused = call(&mut store, &parent, &mut host, cap, "model");
        assert_eq!(refused["error"], "child call cap reached", "{refused}");
        assert_eq!(host.calls as u64, cap);
        // Unpinning the earlier children's journal changes nothing this connection
        // counts: it was read once, then kept as each record was written.
        for n in 0..cap {
            store
                .db
                .execute(
                    "DELETE FROM staging_pins WHERE stage=?1",
                    [child_stage("parent", n)],
                )
                .unwrap();
        }
        let refused = call(&mut store, &parent, &mut host, cap, "model");
        assert_eq!(refused["error"], "child call cap reached", "{refused}");
        assert_eq!(host.calls as u64, cap);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A model child that reports usage, and a host whose dollar cap refuses new ones.
    struct Billed {
        calls: usize,
        refuse: bool,
        seen: usize,
    }
    impl Host for Billed {
        fn discover(&mut self, _: &str, _: &Value) -> std::result::Result<Value, String> {
            Ok(json!([]))
        }
        fn prepare(&mut self, _: &str, _: &Value) -> std::result::Result<Value, String> {
            Ok(
                json!({"kind":"model","provider":"fake","model":"m","contract":"openai-chat-completions","budget":10}),
            )
        }
        fn execute(&mut self, _: &Value, _: &AtomicBool, _: Duration) -> Value {
            self.calls += 1;
            json!({"ok":true,"data":{"text":"a private reply","usage":{"input":7,"output":3,"cache_create":0,"cache_read":0},"provider":"fake","model":"m","subscription":false}})
        }
        fn admit_spend(
            &mut self,
            prepared: &[&Value],
            journal: &[&Value],
        ) -> std::result::Result<(), String> {
            assert_eq!(prepared.len(), 1);
            self.seen = journal.len();
            if self.refuse {
                Err("nested model spend limit".into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn dollar_caps_refuse_before_reservation_and_collected_journals_keep_what_was_billed() {
        let (root, mut store, parent) = fixture();
        let mut host = Billed {
            calls: 0,
            refuse: false,
            seen: 0,
        };
        let ask = |store: &mut Store, host: &mut Billed, id: u64| -> Value {
            let bytes =
                serde_json::to_vec(&json!({"id":id,"method":"model","input":{"prompt":"p"}}))
                    .unwrap();
            serde_json::from_slice(&store.code_request(
                &parent,
                &bytes,
                host,
                &AtomicBool::new(false),
                Duration::from_secs(10),
            ))
            .unwrap()
        };
        assert_eq!(ask(&mut store, &mut host, 0)["ok"], true);
        host.refuse = true;
        let refused = ask(&mut store, &mut host, 1);
        assert_eq!(refused["ok"], false);
        assert_eq!(refused["error"], "nested model spend limit");
        assert_eq!(
            host.seen, 2,
            "the host sees the script's journal (reservation and result)"
        );
        assert_eq!(host.calls, 1, "a refused child is never sent");
        assert_eq!(
            store.code_receipts("operator").unwrap().len(),
            2,
            "nor reserved"
        );
        // Collecting the journal leaves a spend record without the reply text.
        let tx = store.db.transaction().unwrap();
        crate::artifact::keep_spend(
            &tx,
            store.epoch as i64,
            &[(child_stage("parent", 0), "operator".into(), None)],
        )
        .unwrap();
        tx.commit().unwrap();
        let spend: Vec<Value> = store
            .db
            .prepare("SELECT o.bytes,o.size FROM staging_pins p JOIN objects o ON o.digest=p.digest WHERE p.stage=?1")
            .unwrap()
            .query_map([crate::artifact::spend_stage("parent")], |r| crate::artifact::object_at(r, 0))
            .unwrap()
            .map(|bytes| serde_json::from_slice(&bytes.unwrap()).unwrap())
            .collect();
        assert_eq!(spend.len(), 1);
        assert_eq!(spend[0]["kind"], "spend");
        assert_eq!(spend[0]["id"], 0);
        assert_eq!(spend[0]["prepared"]["contract"], "openai-chat-completions");
        assert_eq!(spend[0]["response"]["data"]["usage"]["input"], 7);
        assert!(spend[0]["response"]["data"].get("text").is_none());
        drop(store);
        assert!(
            Store::open(&root).is_ok(),
            "the spend stage passes validation"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn policy_limits_are_clamped_to_fixed_ceilings() {
        assert_eq!(Limits::from_policy(&json!({})), Limits::default());
        assert_eq!(
            Limits::from_policy(
                &json!({"limits":{"calls":100000,"model_calls":0,"model_budget":65536,"parallel":99}})
            ),
            Limits {
                calls: MAX_CALLS,
                model_calls: 1,
                model_budget: 65536,
                parallel: MAX_PARALLEL
            }
        );
    }

    #[test]
    fn journal_keeps_large_strings_as_idempotent_digests() {
        let request = json!({"id":0,"method":"model","input":{"prompt":"short","images":[{"media_type":"image/png","data":"A".repeat(100_000)}]}});
        let journal = journal_form(&request);
        assert_eq!(journal["input"]["prompt"], "short");
        assert_eq!(journal["input"]["images"][0]["data"]["bytes"], 100_000);
        assert_eq!(journal_form(&journal), journal);
        assert!(serde_json::to_vec(&journal).unwrap().len() < 512);
        let other = json!({"id":0,"method":"model","input":{"prompt":"short","images":[{"media_type":"image/png","data":"B".repeat(100_000)}]}});
        assert_ne!(journal_form(&other), journal);
    }

    #[test]
    fn oversized_batch_reply_drops_the_largest_results_not_the_batch() {
        let big = json!({"ok":true,"data":"x".repeat(MAX_REPLY * 2 / 5)});
        let small = json!({"ok":true,"data":"y"});
        let fitted = fit_batch(vec![big.clone(), small.clone(), big.clone(), big]);
        assert_eq!(fitted.len(), 4);
        assert_eq!(fitted[1], small);
        assert_eq!(fitted.iter().filter(|v| v["omitted"] == true).count(), 1);
        assert!(
            serde_json::to_vec(&json!({"ok":true,"data":fitted}))
                .unwrap()
                .len()
                <= MAX_REPLY
        );
    }

    #[derive(Default)]
    struct Parallel {
        live: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Host for Parallel {
        fn discover(&mut self, _: &str, _: &Value) -> std::result::Result<Value, String> {
            Ok(json!([]))
        }
        fn prepare(&mut self, method: &str, input: &Value) -> std::result::Result<Value, String> {
            Ok(json!({"method":method,"input":input,"budget":100}))
        }
        fn execute(&mut self, prepared: &Value, interrupt: &AtomicBool, left: Duration) -> Value {
            self.parallel().unwrap()(prepared, interrupt, left)
        }
        fn limits(&self) -> Limits {
            Limits {
                parallel: 4,
                ..Limits::default()
            }
        }
        fn parallel(&self) -> Option<ParallelExecute> {
            let (live, peak, calls) = (self.live.clone(), self.peak.clone(), self.calls.clone());
            Some(Arc::new(move |prepared, _, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(60));
                live.fetch_sub(1, Ordering::SeqCst);
                json!({"ok":true,"data":prepared["input"]["prompt"]})
            }))
        }
    }
    fn send(
        store: &mut Store,
        parent: &DispatchRequest,
        host: &mut Parallel,
        request: Value,
    ) -> Value {
        let bytes = serde_json::to_vec(&request).unwrap();
        serde_json::from_slice(&store.code_request(
            parent,
            &bytes,
            host,
            &AtomicBool::new(false),
            Duration::from_secs(10),
        ))
        .unwrap()
    }

    #[test]
    fn batch_reserves_every_child_runs_concurrently_and_replays_without_resend() {
        let (root, mut store, parent) = fixture();
        let mut host = Parallel::default();
        let batch = |first: u64, count: u64| {
            let calls: Vec<Value> = (first..first + count)
                .map(|n| json!({"method":"model","input":{"prompt":format!("p{n}")}}))
                .collect();
            json!({"id":first,"method":"batch","input":{"calls":calls}})
        };
        let limits = send(
            &mut store,
            &parent,
            &mut host,
            json!({"id":0,"method":"limits","input":{}}),
        );
        assert_eq!(limits["data"]["parallel"], 4);
        let first = send(&mut store, &parent, &mut host, batch(0, 4));
        assert_eq!(first["ok"], true);
        assert_eq!(first["data"].as_array().unwrap().len(), 4);
        assert_eq!(first["data"][2]["data"], "p2");
        assert!(
            host.peak.load(Ordering::SeqCst) > 1,
            "children ran one at a time"
        );
        assert_eq!(host.calls.load(Ordering::SeqCst), 4);
        assert_eq!(first, send(&mut store, &parent, &mut host, batch(0, 4)));
        let single = send(
            &mut store,
            &parent,
            &mut host,
            json!({"id":1,"method":"model","input":{"prompt":"p1"}}),
        );
        assert_eq!(single, first["data"][1]);
        assert_eq!(host.calls.load(Ordering::SeqCst), 4);
        assert_eq!(store.code_receipts("operator").unwrap().len(), 8);
        let over = send(&mut store, &parent, &mut host, batch(4, 5));
        assert_eq!(over["ok"], false, "{over}");
        assert_eq!(host.calls.load(Ordering::SeqCst), 4);
        assert_eq!(store.code_receipts("operator").unwrap().len(), 8);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A failure the host recorded (HTTP 429, a non-zero tool exit) is a known outcome:
    /// only a reservation with no recorded result blocks later children.
    #[test]
    fn recorded_failure_is_known_and_does_not_block_later_children() {
        struct Flaky {
            calls: usize,
        }
        impl Host for Flaky {
            fn discover(&mut self, _: &str, _: &Value) -> std::result::Result<Value, String> {
                Ok(json!([]))
            }
            fn prepare(
                &mut self,
                method: &str,
                input: &Value,
            ) -> std::result::Result<Value, String> {
                Ok(json!({"method":method,"input":input,"budget":10}))
            }
            fn execute(&mut self, _: &Value, _: &AtomicBool, _: Duration) -> Value {
                self.calls += 1;
                if self.calls == 1 {
                    json!({"ok":false,"may_have_run":true,"error":"the provider rate-limited the request (HTTP 429)"})
                } else {
                    json!({"ok":true,"data":"answer"})
                }
            }
        }
        let (root, mut store, parent) = fixture();
        let mut host = Flaky { calls: 0 };
        let request = |id: u64| {
            serde_json::to_vec(
                &json!({"seq":id + 7,"id":id,"method":"model","input":{"prompt":"p"}}),
            )
            .unwrap()
        };
        let first: Value = serde_json::from_slice(&store.code_request(
            &parent,
            &request(0),
            &mut host,
            &AtomicBool::new(false),
            Duration::from_secs(10),
        ))
        .unwrap();
        assert_eq!(
            (first["ok"].clone(), first["seq"].clone()),
            (json!(false), json!(7))
        );
        let second: Value = serde_json::from_slice(&store.code_request(
            &parent,
            &request(1),
            &mut host,
            &AtomicBool::new(false),
            Duration::from_secs(10),
        ))
        .unwrap();
        assert_eq!(second["data"], "answer", "{second}");
        assert_eq!(second["seq"], 8);
        assert_eq!(host.calls, 2);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// In a parallel chunk each child's result is journaled as soon as its worker ends,
    /// while slower siblings still run.
    #[test]
    fn parallel_children_are_journaled_as_each_finishes() {
        struct Watch(std::path::PathBuf);
        impl Host for Watch {
            fn discover(&mut self, _: &str, _: &Value) -> std::result::Result<Value, String> {
                Ok(json!([]))
            }
            fn prepare(
                &mut self,
                method: &str,
                input: &Value,
            ) -> std::result::Result<Value, String> {
                Ok(json!({"method":method,"input":input,"budget":10}))
            }
            fn execute(
                &mut self,
                prepared: &Value,
                interrupt: &AtomicBool,
                left: Duration,
            ) -> Value {
                self.parallel().unwrap()(prepared, interrupt, left)
            }
            fn limits(&self) -> Limits {
                Limits {
                    parallel: 2,
                    ..Limits::default()
                }
            }
            fn parallel(&self) -> Option<ParallelExecute> {
                let database = self.0.join("authority.sqlite");
                Some(Arc::new(move |prepared, _, _| {
                    if prepared["input"]["prompt"] == "fast" {
                        return json!({"ok":true,"data":"fast"});
                    }
                    let db = rusqlite::Connection::open(&database).unwrap();
                    // Every object as stored (packed or not), read as the store reads it.
                    let journaled = || -> rusqlite::Result<bool> {
                        let mut query = db.prepare("SELECT bytes,size FROM objects")?;
                        let objects = query
                            .query_map([], |row| crate::artifact::object_at(row, 0))?
                            .collect::<rusqlite::Result<Vec<_>>>()?;
                        Ok(objects.iter().any(|bytes| {
                            String::from_utf8_lossy(bytes).contains("\"kind\":\"result\"")
                        }))
                    };
                    for _ in 0..500 {
                        if journaled().unwrap_or(false) {
                            return json!({"ok":true,"data":"saw the fast result journaled"});
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    json!({"ok":true,"data":"not journaled while running"})
                }))
            }
        }
        let (root, mut store, parent) = fixture();
        let mut host = Watch(root.clone());
        let batch = json!({"id":0,"method":"batch","input":{"calls":[
            {"method":"model","input":{"prompt":"fast"}},
            {"method":"model","input":{"prompt":"slow"}}
        ]}});
        let reply: Value = serde_json::from_slice(&store.code_request(
            &parent,
            &serde_json::to_vec(&batch).unwrap(),
            &mut host,
            &AtomicBool::new(false),
            Duration::from_secs(10),
        ))
        .unwrap();
        assert_eq!(reply["data"][0]["data"], "fast");
        assert_eq!(
            reply["data"][1]["data"], "saw the fast result journaled",
            "{reply}"
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
