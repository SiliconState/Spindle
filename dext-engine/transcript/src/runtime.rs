//! Shared CLI runtime decisions over supplied observations. No I/O or approval grants.
pub const AUTH_CIRCUIT_BREAKER_THRESHOLD: usize = 2;

use serde_json::Value;
use std::collections::{HashMap, HashSet};

const MIN_DYNAMIC_UI_CAP: usize = 2_000;
const MIN_DYNAMIC_TOOL_RESULT_CAP: usize = 6_000;

fn cap_bytes_with_hint(s: String, cap: usize, hint: &str) -> String {
    if s.len() <= cap {
        return s;
    }
    let mut end = cap;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let suffix = if hint.is_empty() {
        String::new()
    } else {
        format!(" {hint}")
    };
    format!(
        "{}\n\n…[truncated after {} bytes; kept first {}.{}]",
        &s[..end],
        s.len(),
        cap,
        suffix
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkPhase {
    Probe,
    Scale,
    Synthesize,
}

impl WorkPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Scale => "scale",
            Self::Synthesize => "synthesize",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseTrigger {
    ScaleCollection,
    DeliverableWrite,
    FinalResponse,
    Steering,
    PartialDeliveryFallback,
    Fix,
}

pub struct ExternalOutcomeInput<'a> {
    pub tool_name: &'a str,
    pub hosts: &'a [String],
    pub cache_key: Option<&'a str>,
    pub bash_similarity_key: Option<&'a str>,
    pub command: Option<&'a str>,
    pub content: &'a mut String,
    pub is_error: Option<bool>,
}

pub fn phase_transition(
    current: WorkPhase,
    trigger: PhaseTrigger,
) -> Option<(WorkPhase, &'static str)> {
    let next = match trigger {
        PhaseTrigger::ScaleCollection => match current {
            WorkPhase::Probe => WorkPhase::Scale,
            _ => current,
        },
        PhaseTrigger::DeliverableWrite
        | PhaseTrigger::FinalResponse
        | PhaseTrigger::PartialDeliveryFallback
        | PhaseTrigger::Steering
        | PhaseTrigger::Fix => WorkPhase::Synthesize,
    };

    if next == current {
        return None;
    }

    let message = match trigger {
        PhaseTrigger::ScaleCollection => "probe passed; scaling external collection",
        PhaseTrigger::DeliverableWrite => "consolidating results into deliverable artifacts",
        PhaseTrigger::FinalResponse => "preparing final response",
        PhaseTrigger::PartialDeliveryFallback => {
            "consolidate partial results and request user decision"
        }
        PhaseTrigger::Steering => "redirecting based on user input",
        PhaseTrigger::Fix => "fix/apply requested; code changes are in scope",
    };

    Some((next, message))
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExternalObservation {
    pub round_external_failures: usize,
    pub followup_warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalTelemetry {
    pub dedupe_hits: usize,
    pub similarity_blocks: usize,
    pub circuit_breaker_trips: usize,
    pub partial_delivery_hints: usize,
    pub http_retries: usize,
    pub empty_tool_call_hints: usize,
}

#[derive(Debug, Clone)]
pub struct TurnRuntimeState {
    phase: WorkPhase,
    partial_delivery_hint_emitted: bool,
    total_auth_failure_events: usize,
    blocked_hosts: HashSet<String>,
    host_auth_failures: HashMap<String, usize>,
    host_probe_passed: HashSet<String>,
    external_result_cache: HashMap<String, (String, Option<bool>)>,
    bash_attempt_history: Vec<(String, bool, u64)>,
    tool_failure_budget: HashMap<String, (String, usize)>,
    mutation_epoch: u64,
    telemetry: ExternalTelemetry,
    empty_tool_call_streak: usize,
    empty_tool_call_note_emitted: bool,
}

impl Default for TurnRuntimeState {
    fn default() -> Self {
        Self {
            phase: WorkPhase::Probe,
            partial_delivery_hint_emitted: false,
            total_auth_failure_events: 0,
            blocked_hosts: HashSet::new(),
            host_auth_failures: HashMap::new(),
            host_probe_passed: HashSet::new(),
            external_result_cache: HashMap::new(),
            bash_attempt_history: Vec::new(),
            tool_failure_budget: HashMap::new(),
            mutation_epoch: 0,
            telemetry: ExternalTelemetry::default(),
            empty_tool_call_streak: 0,
            empty_tool_call_note_emitted: false,
        }
    }
}

impl TurnRuntimeState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn phase(&self) -> WorkPhase {
        self.phase
    }

    pub fn telemetry(&self) -> ExternalTelemetry {
        self.telemetry
    }

    pub fn advance_phase(&mut self, trigger: PhaseTrigger) -> Option<(WorkPhase, &'static str)> {
        let (next, msg) = phase_transition(self.phase, trigger)?;
        self.phase = next;
        Some((next, msg))
    }

    pub fn dedupe_guard(&mut self, cache_key: Option<&str>) -> Option<(String, Option<bool>)> {
        let hit = dedupe_cache_short_circuit(&self.external_result_cache, cache_key);
        if hit.is_some() {
            self.telemetry.dedupe_hits = self.telemetry.dedupe_hits.saturating_add(1);
        }
        hit
    }

    pub fn mark_mutation_succeeded(&mut self) {
        self.mutation_epoch = self.mutation_epoch.saturating_add(1);
    }

    pub fn bash_similarity_guard(&mut self, similarity_key: Option<&str>) -> Option<String> {
        let sim_key = similarity_key?;
        let is_file_tool = sim_key.contains("write_file:") || sim_key.contains("edit_file:");
        let similar_unproductive = self
            .bash_attempt_history
            .iter()
            .filter(|(seen_key, productive, epoch)| {
                if *productive || *epoch != self.mutation_epoch {
                    return false;
                }
                if is_file_tool {
                    seen_key == sim_key
                } else {
                    commands_similar(sim_key, seen_key)
                }
            })
            .count();
        if similar_unproductive >= 3 {
            self.telemetry.similarity_blocks = self.telemetry.similarity_blocks.saturating_add(1);
            if is_file_tool {
                Some(
                    "file write similarity guard: this file operation is too similar to multiple earlier unproductive attempts this turn. Stop retrying the same file path and pivot strategy (check file content first, use a different approach, or ask the user).".to_string(),
                )
            } else {
                Some(
                    "bash similarity guard: this command is too similar to multiple earlier unproductive attempts this turn. Stop looping on near-duplicates and pivot strategy (new source, new method, or ask user).".to_string(),
                )
            }
        } else {
            None
        }
    }

    const TOOL_RETRY_BUDGET: usize = 3;

    pub fn tool_retry_guard(&mut self, tool_name: &str, error_summary: &str) -> Option<String> {
        let key = normalize_tool_failure_key(tool_name, error_summary);
        let entry = self
            .tool_failure_budget
            .entry(key)
            .or_insert_with(|| (error_summary.to_string(), 0));
        entry.1 = entry.1.saturating_add(1);
        if entry.1 > Self::TOOL_RETRY_BUDGET {
            self.telemetry.circuit_breaker_trips =
                self.telemetry.circuit_breaker_trips.saturating_add(1);
            Some(format!(
                "tool retry budget exceeded: {tool_name} has failed {} times with the same error ({}). Stop retrying this tool with the same arguments and pivot strategy (use a different tool, change approach, or ask the user).",
                entry.1, entry.0
            ))
        } else {
            None
        }
    }

    pub fn record_empty_tool_calls(&mut self, count: usize) {
        if count > 0 {
            self.empty_tool_call_streak = self.empty_tool_call_streak.saturating_add(1);
        } else {
            self.empty_tool_call_streak = 0;
        }
    }

    /// After enough consecutive empty-tool-call iterations, inject a recovery
    /// hint.  The provider (notably GLM) sometimes drops function_call
    /// arguments when the intended payload is large, producing `input: {}`.
    /// The model cannot self-correct from this loop without a structural hint.
    pub fn empty_tool_call_loop_note(&mut self) -> Option<String> {
        if self.empty_tool_call_note_emitted {
            return None;
        }
        // Provider keeps emitting tool calls with empty arguments — likely a
        // generation bug where large payloads get silently dropped.
        const EMPTY_STREAK_THRESHOLD: usize = 4;
        if self.empty_tool_call_streak >= EMPTY_STREAK_THRESHOLD {
            self.empty_tool_call_note_emitted = true;
            self.telemetry.empty_tool_call_hints =
                self.telemetry.empty_tool_call_hints.saturating_add(1);
            Some(
                "Your last several tool calls had completely empty arguments (no parameters at all). \
                 This is a provider generation bug that occurs when the intended payload is too large. \
                 Recovery strategy: (1) write a small placeholder file first with write_file, \
                 (2) then build the full content incrementally using multiple edit_file calls, \
                 each with a small chunk (under 4 KB of new_string). \
                 Do NOT attempt to pass large content in a single tool call again this turn."
                    .to_string(),
            )
        } else {
            None
        }
    }

    pub fn empty_tool_call_halt_message(&self) -> Option<String> {
        const EMPTY_HALT_THRESHOLD: usize = 8;
        if self.empty_tool_call_note_emitted && self.empty_tool_call_streak >= EMPTY_HALT_THRESHOLD
        {
            Some(format!(
                "[halted: provider emitted empty tool arguments for {} consecutive tool rounds, even after a recovery hint. Stop this turn to avoid burning iterations; retry with smaller chunks or switch provider/model.]",
                self.empty_tool_call_streak
            ))
        } else {
            None
        }
    }

    pub fn blocked_host_guard(&self, hosts: &[String]) -> Option<String> {
        let host = hosts.iter().find(|h| self.blocked_hosts.contains(*h))?;
        Some(format!(
            "source circuit breaker: host '{host}' already produced repeated auth failures this turn. Stop retrying this source and pivot or ask for credentials."
        ))
    }

    pub fn feasibility_guard(&self, hosts: &[String], bulk_network: bool) -> Option<String> {
        if !bulk_network {
            return None;
        }
        let unprobed: Vec<String> = hosts
            .iter()
            .filter(|h| !self.host_probe_passed.contains(*h))
            .cloned()
            .collect();
        if unprobed.is_empty() {
            None
        } else {
            Some(format!(
                "source feasibility gate: host(s) {} have not passed a single-item probe yet. Bulk external collection is blocked until each host succeeds once on a representative single item (for example, limit=1 or one item URL). Run that smaller probe, confirm the expected fields/schema, then retry the original bulk request.",
                unprobed.join(", ")
            ))
        }
    }

    pub fn record_external_outcome(
        &mut self,
        input: ExternalOutcomeInput<'_>,
    ) -> ExternalObservation {
        let mut observation = ExternalObservation::default();
        let ok = !input.is_error.unwrap_or(false);
        let auth_failure = dext_core::recovery::output_has_auth_failure_markers(input.content);

        if matches!(input.tool_name, "bash" | "http") {
            observation.round_external_failures = external_failure_increment(ok, auth_failure);

            if !input.hosts.is_empty() {
                if auth_failure {
                    self.total_auth_failure_events =
                        self.total_auth_failure_events.saturating_add(1);
                    let mut newly_blocked: Vec<String> = Vec::new();
                    for host in input.hosts {
                        let entry = self.host_auth_failures.entry(host.clone()).or_insert(0);
                        *entry = entry.saturating_add(1);
                        if *entry >= AUTH_CIRCUIT_BREAKER_THRESHOLD
                            && self.blocked_hosts.insert(host.clone())
                        {
                            newly_blocked.push(host.clone());
                        }
                    }
                    if !newly_blocked.is_empty() {
                        let blocked_list = newly_blocked.join(", ");
                        input.content.push_str(&format!(
                            "\n\n[circuit-breaker] repeated auth failures for host(s): {blocked_list}. Stop retrying this source in this turn; pivot to another provider or ask the user for credentials."
                        ));
                        observation
                            .followup_warnings
                            .push(format!("source circuit breaker tripped for {blocked_list}"));
                        self.telemetry.circuit_breaker_trips =
                            self.telemetry.circuit_breaker_trips.saturating_add(1);
                    }
                } else if ok {
                    for host in input.hosts {
                        self.host_probe_passed.insert(host.clone());
                    }
                }
            }

            if let Some(cache_key) = input.cache_key {
                self.external_result_cache.insert(
                    cache_key.to_string(),
                    (
                        cap_bytes_with_hint(input.content.clone(), 6_000, "cache entry truncated"),
                        input.is_error,
                    ),
                );
            }
        }

        if let Some(sim_key) = input.bash_similarity_key {
            let productive = ok
                && !auth_failure
                && (input.content.trim().chars().count() >= 80
                    || input
                        .command
                        .is_some_and(is_safe_repeated_validation_command));
            self.bash_attempt_history
                .push((sim_key.to_string(), productive, self.mutation_epoch));
            if self.bash_attempt_history.len() > MAX_BASH_ATTEMPT_HISTORY {
                self.bash_attempt_history.remove(0);
            }
        }

        observation
    }

    pub fn should_emit_partial_delivery_hint(&self, round_external_failures: usize) -> bool {
        should_emit_partial_delivery_hint(
            self.partial_delivery_hint_emitted,
            self.blocked_hosts.len(),
            self.total_auth_failure_events,
            round_external_failures,
        )
    }

    pub fn mark_partial_delivery_hint_emitted(&mut self) {
        self.partial_delivery_hint_emitted = true;
        self.telemetry.partial_delivery_hints =
            self.telemetry.partial_delivery_hints.saturating_add(1);
    }

    pub fn record_http_retry(&mut self) {
        self.telemetry.http_retries = self.telemetry.http_retries.saturating_add(1);
    }
}

pub const MAX_RUNTIME_ENTRIES: usize = 1024;
const MAX_RUNTIME_KEY_BYTES: usize = 4096;
const MAX_RUNTIME_TEXT_BYTES: usize = 8192;
const MAX_BASH_ATTEMPT_HISTORY: usize = 64;

/// Canonical `DRS1` body: every map/set is a strictly ascending vector so one
/// state has exactly one encoding. Decision state only; it carries no approval,
/// credential, disclosure or dispatch authority. Cached results are copies of
/// tool results already in the paired transcript checkpoint.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeCheckpoint {
    phase: WorkPhase,
    partial_delivery_hint_emitted: bool,
    total_auth_failure_events: usize,
    host_auth_failures: Vec<(String, usize)>,
    blocked_hosts: Vec<String>,
    host_probe_passed: Vec<String>,
    external_result_cache: Vec<(String, String, Option<bool>)>,
    bash_attempt_history: Vec<(String, bool, u64)>,
    tool_failure_budget: Vec<(String, String, usize)>,
    mutation_epoch: u64,
    telemetry: ExternalTelemetry,
    empty_tool_call_streak: usize,
    empty_tool_call_note_emitted: bool,
}

fn sorted_keys<'a, I: Iterator<Item = &'a String>>(keys: I) -> Vec<String> {
    let mut keys: Vec<String> = keys.cloned().collect();
    keys.sort();
    keys
}

fn strictly_ascending<T, K: Ord>(items: &[T], key: impl Fn(&T) -> &K) -> bool {
    items.windows(2).all(|pair| key(&pair[0]) < key(&pair[1]))
}

impl RuntimeCheckpoint {
    fn validate(&self) -> Result<(), crate::checkpoint::Error> {
        use crate::checkpoint::Error;
        let collections = [
            self.host_auth_failures.len(),
            self.blocked_hosts.len(),
            self.host_probe_passed.len(),
            self.external_result_cache.len(),
            self.tool_failure_budget.len(),
        ];
        if collections.iter().any(|len| *len > MAX_RUNTIME_ENTRIES)
            || self.bash_attempt_history.len() > MAX_BASH_ATTEMPT_HISTORY
        {
            return Err(Error::Bounds);
        }
        let key_ok = |key: &String| !key.is_empty() && key.len() <= MAX_RUNTIME_KEY_BYTES;
        let text_ok = |text: &String| text.len() <= MAX_RUNTIME_TEXT_BYTES;
        let bounded = self.host_auth_failures.iter().all(|(h, _)| key_ok(h))
            && self.blocked_hosts.iter().all(key_ok)
            && self.host_probe_passed.iter().all(key_ok)
            && self
                .external_result_cache
                .iter()
                .all(|(k, v, _)| key_ok(k) && text_ok(v))
            && self.bash_attempt_history.iter().all(|(k, _, _)| key_ok(k))
            && self
                .tool_failure_budget
                .iter()
                .all(|(k, v, _)| key_ok(k) && text_ok(v));
        if !bounded {
            return Err(Error::Bounds);
        }
        let canonical = strictly_ascending(&self.host_auth_failures, |(h, _)| h)
            && strictly_ascending(&self.blocked_hosts, |h| h)
            && strictly_ascending(&self.host_probe_passed, |h| h)
            && strictly_ascending(&self.external_result_cache, |(k, _, _)| k)
            && strictly_ascending(&self.tool_failure_budget, |(k, _, _)| k);
        // Mirrors record_external_outcome: every auth event bumps the total and each
        // named host once, and a host is blocked exactly when it reaches the threshold.
        let auth_consistent = self.host_auth_failures.iter().all(|(host, count)| {
            *count >= 1
                && *count <= self.total_auth_failure_events
                && (*count >= AUTH_CIRCUIT_BREAKER_THRESHOLD)
                    == self.blocked_hosts.binary_search(host).is_ok()
        }) && self.blocked_hosts.iter().all(|host| {
            self.host_auth_failures
                .binary_search_by(|(h, _)| h.cmp(host))
                .is_ok()
        });
        let history_consistent = self
            .bash_attempt_history
            .windows(2)
            .all(|pair| pair[0].2 <= pair[1].2)
            && self
                .bash_attempt_history
                .iter()
                .all(|(_, _, epoch)| *epoch <= self.mutation_epoch);
        let hints_consistent = (!self.partial_delivery_hint_emitted
            || self.telemetry.partial_delivery_hints >= 1)
            && (!self.empty_tool_call_note_emitted || self.telemetry.empty_tool_call_hints >= 1)
            && self.tool_failure_budget.iter().all(|(_, _, n)| *n >= 1);
        if canonical && auth_consistent && history_consistent && hints_consistent {
            Ok(())
        } else {
            Err(Error::InvalidRuntime)
        }
    }
}

fn phase_rank(phase: WorkPhase) -> u8 {
    match phase {
        WorkPhase::Probe => 0,
        WorkPhase::Scale => 1,
        WorkPhase::Synthesize => 2,
    }
}

impl TurnRuntimeState {
    /// True when `self` is reachable from `previous` through the public recording
    /// API: breakers, budgets, hints, probes, cache keys, epochs and telemetry only
    /// grow. Hosts use this so a supplied update cannot silently reset a guard.
    pub fn is_successor_of(&self, previous: &Self) -> bool {
        let a = self.telemetry;
        let b = previous.telemetry;
        phase_rank(self.phase) >= phase_rank(previous.phase)
            && self.total_auth_failure_events >= previous.total_auth_failure_events
            && self.mutation_epoch >= previous.mutation_epoch
            && (self.partial_delivery_hint_emitted || !previous.partial_delivery_hint_emitted)
            && (self.empty_tool_call_note_emitted || !previous.empty_tool_call_note_emitted)
            && previous.blocked_hosts.is_subset(&self.blocked_hosts)
            && previous
                .host_probe_passed
                .is_subset(&self.host_probe_passed)
            && previous
                .host_auth_failures
                .iter()
                .all(|(host, n)| self.host_auth_failures.get(host).is_some_and(|m| m >= n))
            && previous.tool_failure_budget.iter().all(|(key, (_, n))| {
                self.tool_failure_budget
                    .get(key)
                    .is_some_and(|(_, m)| m >= n)
            })
            && previous
                .external_result_cache
                .keys()
                .all(|key| self.external_result_cache.contains_key(key))
            && a.dedupe_hits >= b.dedupe_hits
            && a.similarity_blocks >= b.similarity_blocks
            && a.circuit_breaker_trips >= b.circuit_breaker_trips
            && a.partial_delivery_hints >= b.partial_delivery_hints
            && a.http_retries >= b.http_retries
            && a.empty_tool_call_hints >= b.empty_tool_call_hints
    }
}

impl TurnRuntimeState {
    /// Fails closed with `Bounds` instead of truncating decision state.
    pub fn checkpoint(&self) -> Result<Vec<u8>, crate::checkpoint::Error> {
        let mut external_result_cache: Vec<_> = self
            .external_result_cache
            .iter()
            .map(|(k, (v, e))| (k.clone(), v.clone(), *e))
            .collect();
        external_result_cache.sort_by(|a, b| a.0.cmp(&b.0));
        let mut tool_failure_budget: Vec<_> = self
            .tool_failure_budget
            .iter()
            .map(|(k, (v, n))| (k.clone(), v.clone(), *n))
            .collect();
        tool_failure_budget.sort_by(|a, b| a.0.cmp(&b.0));
        let mut host_auth_failures: Vec<_> = self
            .host_auth_failures
            .iter()
            .map(|(h, n)| (h.clone(), *n))
            .collect();
        host_auth_failures.sort();
        let snapshot = RuntimeCheckpoint {
            phase: self.phase,
            partial_delivery_hint_emitted: self.partial_delivery_hint_emitted,
            total_auth_failure_events: self.total_auth_failure_events,
            host_auth_failures,
            blocked_hosts: sorted_keys(self.blocked_hosts.iter()),
            host_probe_passed: sorted_keys(self.host_probe_passed.iter()),
            external_result_cache,
            bash_attempt_history: self.bash_attempt_history.clone(),
            tool_failure_budget,
            mutation_epoch: self.mutation_epoch,
            telemetry: self.telemetry,
            empty_tool_call_streak: self.empty_tool_call_streak,
            empty_tool_call_note_emitted: self.empty_tool_call_note_emitted,
        };
        snapshot.validate()?;
        crate::checkpoint::encode(b"DRS1", &snapshot)
    }

    pub fn restore(bytes: &[u8]) -> Result<Self, crate::checkpoint::Error> {
        let snapshot: RuntimeCheckpoint = crate::checkpoint::decode(b"DRS1", bytes)?;
        snapshot.validate()?;
        Ok(Self {
            phase: snapshot.phase,
            partial_delivery_hint_emitted: snapshot.partial_delivery_hint_emitted,
            total_auth_failure_events: snapshot.total_auth_failure_events,
            blocked_hosts: snapshot.blocked_hosts.into_iter().collect(),
            host_auth_failures: snapshot.host_auth_failures.into_iter().collect(),
            host_probe_passed: snapshot.host_probe_passed.into_iter().collect(),
            external_result_cache: snapshot
                .external_result_cache
                .into_iter()
                .map(|(k, v, e)| (k, (v, e)))
                .collect(),
            bash_attempt_history: snapshot.bash_attempt_history,
            tool_failure_budget: snapshot
                .tool_failure_budget
                .into_iter()
                .map(|(k, v, n)| (k, (v, n)))
                .collect(),
            mutation_epoch: snapshot.mutation_epoch,
            telemetry: snapshot.telemetry,
            empty_tool_call_streak: snapshot.empty_tool_call_streak,
            empty_tool_call_note_emitted: snapshot.empty_tool_call_note_emitted,
        })
    }
}

fn str_array(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

pub fn extract_url_hosts(text: &str) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();

    for token in text.split(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']')
    }) {
        let cleaned = token.trim_matches(|c: char| matches!(c, ',' | ';' | '.' | ')' | ']' | '}'));
        let rest = if let Some(r) = cleaned.strip_prefix("https://") {
            r
        } else if let Some(r) = cleaned.strip_prefix("http://") {
            r
        } else {
            continue;
        };

        let host = rest
            .split('/')
            .next()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("")
            .trim_matches('.')
            .to_ascii_lowercase();
        if host.is_empty() {
            continue;
        }
        if seen.insert(host.clone()) {
            out.push(host);
        }
    }

    out
}

pub fn hosts_for_tool_call(name: &str, input: &Value) -> Vec<String> {
    match name {
        "bash" => extract_url_hosts(input["command"].as_str().unwrap_or("")),
        "http" => {
            let joined = str_array(&input["args"]).join(" ");
            extract_url_hosts(&joined)
        }
        _ => Vec::new(),
    }
}

pub fn looks_like_bulk_network_call(name: &str, input: &Value) -> bool {
    match name {
        "bash" => {
            let command = input["command"].as_str().unwrap_or("");
            let lower = command.to_ascii_lowercase();
            let has_url = lower.contains("http://") || lower.contains("https://");
            let shell_loop = (lower.contains("for ") || lower.contains("while "))
                && (lower.contains("; do")
                    || lower.lines().any(|line| {
                        let line = line.trim_start();
                        line == "do" || line.starts_with("do ")
                    }));
            let loopish =
                has_url && (shell_loop || lower.contains("xargs") || lower.contains("parallel"));
            let bulk_query = lower.contains("size=")
                || lower.contains("limit=")
                || lower.contains("offset=")
                || lower.contains("symbols=")
                || lower.contains("ids=")
                || lower.contains("batch")
                || lower.contains("bulk");
            loopish || (has_url && bulk_query)
        }
        "http" => {
            let joined = str_array(&input["args"]).join(" ");
            let lower = joined.to_ascii_lowercase();
            lower.contains("size=")
                || lower.contains("limit=")
                || lower.contains("offset=")
                || lower.contains("symbols=")
                || lower.contains("ids=")
                || lower.contains("batch")
                || lower.contains("bulk")
        }
        _ => false,
    }
}

pub const MUTATING_FILE_TOOLS: &[&str] = &["edit_file", "multi_edit", "write_file"];

/// Guard and observation keys derived from one tool call, shared by the CLI round
/// and managed continuation so both index the same breaker/cache/similarity state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallKeys {
    pub hosts: Vec<String>,
    pub bulk_network: bool,
    pub cache_key: Option<String>,
    pub similarity_key: Option<String>,
}

impl ToolCallKeys {
    pub fn derive(name: &str, input: &Value) -> Self {
        let similarity_key = if name == "bash" {
            Some(normalize_bash_similarity_key(
                input["command"].as_str().unwrap_or(""),
            ))
        } else if matches!(name, "write_file" | "edit_file") {
            input["path"].as_str().map(|p| format!("{name}:{p}"))
        } else {
            None
        };
        Self {
            hosts: hosts_for_tool_call(name, input),
            bulk_network: looks_like_bulk_network_call(name, input),
            cache_key: network_cache_key(name, input),
            similarity_key,
        }
    }
}

/// A runtime guard verdict that replaces dispatch with an immediate result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preflight {
    pub content: String,
    pub is_error: Option<bool>,
    pub telemetry_changed: bool,
}

impl TurnRuntimeState {
    /// The CLI's dedupe, similarity, blocked-host and feasibility guards, in CLI
    /// order. A verdict is advisory runtime state, never an execution grant.
    pub fn preflight(&mut self, keys: &ToolCallKeys) -> Option<Preflight> {
        if let Some((content, is_error)) = self.dedupe_guard(keys.cache_key.as_deref()) {
            return Some(Preflight {
                content,
                is_error,
                telemetry_changed: true,
            });
        }
        if let Some(content) = self.bash_similarity_guard(keys.similarity_key.as_deref()) {
            return Some(Preflight {
                content,
                is_error: Some(true),
                telemetry_changed: true,
            });
        }
        self.blocked_host_guard(&keys.hosts)
            .or_else(|| self.feasibility_guard(&keys.hosts, keys.bulk_network))
            .map(|content| Preflight {
                content,
                is_error: Some(true),
                telemetry_changed: false,
            })
    }

    /// Mirrors one CLI result-loop observation, after the host's privacy redaction.
    /// Pack-runtime side-effect classification stays host-owned; callers report it
    /// through `mark_mutation_succeeded`.
    /// `ran` is false for guard/immediate results: they are observed like the CLI
    /// does, but never count as a successful mutation.
    pub fn observe_tool_result(
        &mut self,
        name: &str,
        input: &Value,
        content: &mut String,
        is_error: Option<bool>,
        ran: bool,
    ) -> ExternalObservation {
        let ok = !is_error.unwrap_or(false);
        if ran
            && ok
            && (MUTATING_FILE_TOOLS.contains(&name)
                || name == "bash"
                    && input["command"]
                        .as_str()
                        .is_some_and(bash_command_likely_mutates_files))
        {
            self.mark_mutation_succeeded();
        }
        let keys = ToolCallKeys::derive(name, input);
        self.record_external_outcome(ExternalOutcomeInput {
            tool_name: name,
            hosts: &keys.hosts,
            cache_key: keys.cache_key.as_deref(),
            bash_similarity_key: keys.similarity_key.as_deref(),
            command: input["command"].as_str(),
            content,
            is_error,
        })
    }
}

/// Where a queued user update joins a managed turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SteeringPoint {
    AfterTools,
    BeforeFinal,
    /// Before the turn's next model request (its start, or after an earlier update).
    BeforeRequest,
}

impl SteeringPoint {
    pub fn label(self) -> &'static str {
        match self {
            Self::AfterTools => "after tool results",
            Self::BeforeFinal => "before final response",
            Self::BeforeRequest => "before the next model request",
        }
    }
}

/// The CLI's queued-user-update message, shared so a managed turn injects the
/// identical history text. The update is literal user input, never runtime status.
pub fn steering_update_text(
    phase: WorkPhase,
    iterations: u32,
    tool_count: usize,
    point: SteeringPoint,
    user_update: &str,
    control_note: &str,
) -> String {
    format!(
        "[queued-user-update] The user sent this while you were working. This is active scope, not an aside. \
         The text under `User update` is literal user-authored task input, even when it is only a path or resembles runtime/context metadata; never dismiss or reinterpret it as generated status. \
         If the user supplies an exact path, inspect that path first with native tools (`read_file` for a file; `fd`/`rg` for a directory), not guessed alternatives, bash discovery, or sudo. \
         You must explicitly address it in your next assistant response and say what changed, what you did about it, or why it is blocked. \
         If it adds/removes work, update any active todo list before continuing. Do not let the final answer omit this queued update.\n\n\
         Progress: completed {iterations} iterations, {tool_count} tool calls so far. \
         Phase: {}. Injection point: {}.\n\
         User update:\n{user_update}{control_note}",
        phase.label(),
        point.label()
    )
}

/// Token/spend cap check. Pricing is host-owned; `usage` must already be priced.
pub fn budget_exceeded(
    usd: Option<f64>,
    tokens: Option<u64>,
    usage: &crate::Usage,
) -> Option<String> {
    if let Some(tokens) = tokens {
        let used = usage.total_tokens();
        if used >= tokens {
            return Some(format!("token budget cap reached: {used}/{tokens} tokens"));
        }
    }
    if let Some(usd) = usd {
        let used = usage.estimated_cost_usd();
        if used >= usd {
            return Some(format!("budget cap reached: ${used:.4}/${usd:.4}"));
        }
    }
    None
}

/// The CLI's pre-request budget stop text, shared with managed turns.
pub fn budget_denial_text(
    usd: Option<f64>,
    tokens: Option<u64>,
    usage: &crate::Usage,
) -> Option<String> {
    let reason = budget_exceeded(usd, tokens, usage)?;
    Some(format!(
        "{reason}; refusing another model request. Raise/clear with /budget <cap|off> or restart with --budget. Current usage: {}",
        usage.line()
    ))
}

#[derive(Debug, Clone)]
pub struct ObjectiveTracker {
    pub summary: String,
    pub checkpoints: Vec<String>,
    pub apply_fixes_requested: bool,
    pub advisory_only_requested: bool,
    pub plan_execution_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectiveCoverage {
    pub satisfied: Vec<String>,
    pub unresolved: Vec<String>,
}

#[derive(Debug)]
struct ObjectiveToolUse {
    name: String,
    input: Value,
}

#[derive(Debug, Default)]
struct ObjectiveEvidence {
    assistant_text_raw: String,
    assistant_text: String,
    final_assistant_text_raw: String,
    tool_names: HashSet<String>,
    bash_commands: Vec<String>,
    attempted_bash_commands: Vec<String>,
    touched_paths: Vec<String>,
    tool_result_text: String,
    mutation_count: usize,
    commit_count: usize,
    tool_uses: HashMap<String, ObjectiveToolUse>,
}

fn normalized_words(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|word| !word.is_empty())
        .collect()
}

fn contains_word_sequence(text: &str, sequence: &[&str]) -> bool {
    if sequence.is_empty() {
        return false;
    }
    normalized_words(text)
        .windows(sequence.len())
        .any(|window| window == sequence)
}

fn intent_text(input: &str) -> String {
    let mut text = String::with_capacity(input.len());
    let mut closing_quote = None;
    let mut previous = ' ';
    for ch in input.chars() {
        if let Some(quote) = closing_quote {
            if ch == quote {
                closing_quote = None;
            }
            text.push(if ch == '\n' { '\n' } else { ' ' });
        } else {
            closing_quote = match ch {
                '"' | '`' => Some(ch),
                '\'' if !previous.is_alphanumeric() => Some('\''),
                '“' => Some('”'),
                '‘' if !previous.is_alphanumeric() => Some('’'),
                _ => None,
            };
            text.push(if closing_quote.is_some() { ' ' } else { ch });
        }
        previous = ch;
    }
    text.to_ascii_lowercase()
}

fn non_negated_at(words: &[&str], index: usize) -> bool {
    !words[index.saturating_sub(4)..index]
        .iter()
        .any(|previous| {
            matches!(
                *previous,
                "not"
                    | "don"
                    | "dont"
                    | "never"
                    | "no"
                    | "without"
                    | "cannot"
                    | "cant"
                    | "isn"
                    | "wasn"
                    | "weren"
                    | "hasn"
                    | "haven"
                    | "hadn"
                    | "didn"
                    | "couldn"
                    | "wouldn"
            )
        })
}

fn contains_non_negated_sequence(text: &str, sequence: &[&str]) -> bool {
    if sequence.is_empty() {
        return false;
    }
    let words = normalized_words(text);
    words
        .windows(sequence.len())
        .enumerate()
        .any(|(index, window)| window == sequence && non_negated_at(&words, index))
}

fn any_word_starts_with(words: &[&str], prefixes: &[&str]) -> bool {
    words
        .iter()
        .any(|word| prefixes.iter().any(|prefix| word.starts_with(prefix)))
}

fn cleanup_requested_as_action(words: &[&str]) -> bool {
    for (idx, word) in words.iter().enumerate() {
        let cleanup_len = if *word == "cleanup" {
            1
        } else if *word == "clean" && words.get(idx + 1) == Some(&"up") {
            2
        } else {
            continue;
        };
        if words.get(idx + cleanup_len).is_none() {
            continue;
        }
        let prev = idx.checked_sub(1).and_then(|i| words.get(i)).copied();
        let polite = idx >= 2
            && matches!(words.get(idx - 2).copied(), Some("can" | "could" | "would"))
            && words.get(idx - 1) == Some(&"you");
        if idx == 0
            || polite
            || matches!(
                prev,
                Some(
                    "and"
                        | "then"
                        | "please"
                        | "also"
                        | "now"
                        | "do"
                        | "handle"
                        | "fix"
                        | "go"
                        | "apply"
                )
            )
        {
            return true;
        }
    }
    false
}

fn commit_requested_as_action(words: &[&str]) -> bool {
    for (idx, word) in words.iter().enumerate() {
        if !matches!(*word, "commit" | "committing" | "committ") {
            continue;
        }
        let prev = idx.checked_sub(1).and_then(|i| words.get(i)).copied();
        let polite = idx >= 2
            && matches!(words.get(idx - 2).copied(), Some("can" | "could" | "would"))
            && words.get(idx - 1) == Some(&"you");
        if idx == 0
            || polite
            || matches!(
                prev,
                Some("and" | "then" | "please" | "also" | "now" | "do" | "handle" | "go" | "apply")
            )
        {
            return true;
        }
    }
    false
}

fn explicit_implementation_requested(lowered: &str) -> bool {
    let words = normalized_words(lowered);
    let configure = words.iter().enumerate().any(|(index, word)| {
        *word == "configure"
            && non_negated_at(&words, index)
            && (index == 0
                || (index >= 2
                    && matches!(words[index - 2], "can" | "could" | "would")
                    && words[index - 1] == "you")
                || matches!(
                    index
                        .checked_sub(1)
                        .and_then(|previous| words.get(previous)),
                    Some(&"and" | &"then" | &"please" | &"also")
                ))
    });
    configure
        || [
            &["apply", "fix"][..],
            &["apply", "fixes"],
            &["apply", "the", "fix"],
            &["apply", "the", "fixes"],
            &["fix", "it"],
            &["fix", "them"],
            &["fix", "this"],
            &["fix", "the"],
            &["fix", "anything"],
            &["fix", "any"],
            &["fix", "issues"],
            &["fix", "errors"],
            &["fix", "bugs"],
            &["fix", "problems"],
            &["fix", "whatever"],
            &["implement"],
            &["execute", "the", "plan"],
            &["execute", "that", "plan"],
            &["start", "implementation"],
            &["proceed", "with", "implementation"],
            &["proceed", "with", "the", "plan"],
            &["go", "ahead"],
            &["rip", "it", "out"],
            &["rip", "out"],
            &["patch"],
            &["merge"],
            &["go", "for", "it"],
            &["handle", "my", "todo"],
            &["make", "changes"],
            &["update", "the", "code"],
            &["update", "docs"],
            &["update", "documentation"],
            &["update", "tests"],
            &["do", "it"],
        ]
        .iter()
        .any(|sequence| contains_non_negated_sequence(lowered, sequence))
}

fn planning_requested_as_action(words: &[&str]) -> bool {
    for (index, word) in words.iter().enumerate() {
        if *word != "plan" {
            continue;
        }
        if words.get(index + 1).is_some_and(|next| {
            matches!(
                *next,
                "mode" | "command" | "feature" | "implementation" | "workflow" | "slash"
            )
        }) {
            continue;
        }
        let previous = index.checked_sub(1).and_then(|i| words.get(i)).copied();
        let polite = index >= 2
            && matches!(
                words.get(index - 2).copied(),
                Some("can" | "could" | "would")
            )
            && words.get(index - 1) == Some(&"you");
        let lets = index >= 2
            && words.get(index - 2) == Some(&"let")
            && words.get(index - 1) == Some(&"s");
        if index == 0
            || polite
            || lets
            || matches!(
                previous,
                Some(
                    "please"
                        | "first"
                        | "then"
                        | "also"
                        | "just"
                        | "only"
                        | "create"
                        | "draft"
                        | "propose"
                        | "produce"
                        | "outline"
                        | "lets"
                        | "to"
                        | "me"
                )
            )
        {
            return true;
        }
    }
    [
        &["want", "a", "plan"][..],
        &["want", "to", "plan"],
        &["need", "a", "plan"],
        &["give", "me", "a", "plan"],
        &["show", "me", "a", "plan"],
        &["come", "up", "with", "a", "plan"],
    ]
    .iter()
    .any(|sequence| {
        words
            .windows(sequence.len())
            .any(|window| window == *sequence)
    })
}

fn explicit_no_mutation_requested(lowered: &str) -> bool {
    [
        &["plan", "only"][..],
        &["analysis", "only"],
        &["review", "only"],
        &["only", "plan"],
        &["just", "plan"],
        &["do", "not", "implement"],
        &["do", "not", "fix"],
        &["do", "not", "patch"],
        &["do", "not", "make", "changes"],
        &["do", "not", "make", "any", "changes"],
        &["do", "not", "change", "anything"],
        &["do", "not", "edit", "anything"],
        &["do", "not", "modify", "anything"],
        &["don", "t", "implement"],
        &["don", "t", "fix"],
        &["don", "t", "patch"],
        &["don", "t", "make", "changes"],
        &["don", "t", "change", "anything"],
        &["don", "t", "edit", "anything"],
        &["dont", "implement"],
        &["dont", "fix"],
        &["dont", "patch"],
        &["dont", "make", "changes"],
        &["dont", "change", "anything"],
        &["dont", "edit", "anything"],
        &["no", "changes", "yet"],
        &["no", "edits", "yet"],
        &["without", "making", "changes"],
        &["without", "editing"],
        &["without", "modifying"],
        &["look", "but", "don", "t", "touch"],
        &["look", "but", "dont", "touch"],
    ]
    .iter()
    .any(|sequence| contains_word_sequence(lowered, sequence))
}

fn global_hold_off_requested(lowered: &str) -> bool {
    let words = normalized_words(lowered);
    [
        &["analysis", "only"][..],
        &["review", "only"],
        &["no", "changes", "yet"],
        &["no", "edits", "yet"],
        &["do", "not", "change", "anything"],
        &["do", "not", "edit", "anything"],
        &["do", "not", "modify", "anything"],
        &["don", "t", "change", "anything"],
        &["don", "t", "edit", "anything"],
        &["dont", "change", "anything"],
        &["dont", "edit", "anything"],
    ]
    .iter()
    .any(|sequence| {
        words
            .windows(sequence.len())
            .enumerate()
            .any(|(index, window)| {
                window == *sequence
                    && !matches!(words.get(index + sequence.len()), Some(&"else" | &"other"))
            })
    })
}

fn planned_execution_requested(lowered: &str) -> bool {
    let first_clause = lowered
        .split(['.', '!', '\n', ','])
        .next()
        .unwrap_or_default()
        .trim();
    let words = normalized_words(first_clause);
    if !first_clause.contains(['\'', '"', '`', '?'])
        && matches!(
            words.as_slice(),
            ["go"]
                | ["proceed"]
                | ["approved"]
                | ["ship", "it"]
                | ["continue", "work"]
                | ["sure" | "yes" | "okay" | "ok", "go" | "proceed"]
        )
    {
        return true;
    }
    [
        &["execute", "the", "plan"][..],
        &["execute", "that", "plan"],
        &["implement", "the", "plan"],
        &["implement", "that", "plan"],
        &["proceed", "with", "the", "plan"],
        &["start", "implementation"],
        &["go", "ahead"],
        &["go", "for", "it"],
        &["do", "it"],
    ]
    .iter()
    .any(|sequence| contains_non_negated_sequence(lowered, sequence))
}

fn explicit_apply_fixes_requested(lowered: &str) -> bool {
    let words = normalized_words(lowered);
    cleanup_requested_as_action(&words)
        || commit_requested_as_action(&words)
        || explicit_implementation_requested(lowered)
}

impl ObjectiveTracker {
    pub fn from_user_prompt(input: &str) -> Self {
        let compact = input
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();
        if compact.is_empty() {
            return Self {
                summary: "(empty prompt)".to_string(),
                checkpoints: Vec::new(),
                apply_fixes_requested: false,
                advisory_only_requested: false,
                plan_execution_requested: false,
            };
        }

        let lowered = intent_text(input);
        let words = normalized_words(&lowered);
        let cleanup_requested = cleanup_requested_as_action(&words);
        let commit_requested = commit_requested_as_action(&words);
        let hold_off = global_hold_off_requested(&lowered);
        let question = lowered.trim_end().ends_with('?')
            && !matches!(words.as_slice(), ["can" | "could" | "would", "you", ..]);
        let implementation_requested =
            !hold_off && !question && explicit_implementation_requested(&lowered);
        let plan_execution_requested =
            !hold_off && !question && planned_execution_requested(&lowered);
        let apply_fixes_requested = !hold_off
            && !question
            && (explicit_apply_fixes_requested(&lowered) || plan_execution_requested);
        let planning_requested = planning_requested_as_action(&words);
        let analysis_requested = any_word_starts_with(&words, &["analy", "review"]);
        // Explicit mutation intent wins: a "don't touch X" scoping clause inside
        // a fix request must not demote the turn to advisory.
        let advisory_only_requested = hold_off
            || (!apply_fixes_requested
                && (explicit_no_mutation_requested(&lowered) || planning_requested));
        let mut checkpoints: Vec<String> = Vec::new();

        if planning_requested {
            checkpoints.push("produce execution plan".to_string());
        }
        if analysis_requested {
            checkpoints.push("analyze current behavior and constraints".to_string());
        }
        if implementation_requested {
            checkpoints.push("implement requested changes".to_string());
        }
        if cleanup_requested && apply_fixes_requested {
            checkpoints.push("clean up repository".to_string());
        }
        if commit_requested && apply_fixes_requested {
            checkpoints.push("commit requested changes".to_string());
        }
        if words.iter().any(|word| {
            matches!(
                *word,
                "test" | "tests" | "testing" | "verify" | "verification"
            )
        }) {
            checkpoints.push("run verification checks".to_string());
        }
        if any_word_starts_with(&words, &["log", "document"]) {
            checkpoints.push("log decisions and follow-up improvements".to_string());
        }

        if checkpoints.is_empty() {
            checkpoints.push("deliver requested outcome with verifiable steps".to_string());
        }

        let summary = if compact.chars().count() > 180 {
            let mut cut = 180;
            while cut > 0 && !compact.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}…", &compact[..cut])
        } else {
            compact
        };

        Self {
            summary,
            checkpoints,
            apply_fixes_requested,
            advisory_only_requested,
            plan_execution_requested,
        }
    }

    pub fn apply_fixes_allowed(&self) -> bool {
        self.apply_fixes_requested
    }

    pub fn advisory_only(&self) -> bool {
        self.advisory_only_requested
    }

    pub fn planned_execution(&self) -> bool {
        self.plan_execution_requested
    }

    pub fn display_line(&self) -> String {
        if self.checkpoints.is_empty() {
            format!("objective: {}", self.summary)
        } else {
            format!(
                "objective: {} | checkpoints: {}",
                self.summary,
                self.checkpoints.join("; ")
            )
        }
    }

    pub fn assess_history(&self, history: &[crate::Message]) -> ObjectiveCoverage {
        let evidence = gather_objective_evidence(history);
        let mut satisfied = Vec::new();
        let mut unresolved = Vec::new();

        for checkpoint in &self.checkpoints {
            if checkpoint_satisfied(checkpoint, &evidence) {
                satisfied.push(checkpoint.clone());
            } else {
                unresolved.push(checkpoint.clone());
            }
        }

        ObjectiveCoverage {
            satisfied,
            unresolved,
        }
    }
}

#[cfg(test)]
pub fn objective_runtime_reminder(
    objective: &ObjectiveTracker,
    history: &[crate::Message],
) -> Option<String> {
    let coverage = objective.assess_history(history);
    if coverage.unresolved.is_empty() {
        return None;
    }

    Some(objective_runtime_reminder_from_coverage(&coverage))
}

pub fn objective_runtime_reminder_from_coverage(coverage: &ObjectiveCoverage) -> String {
    format!(
        "runtime guidance: objective checkpoints still look unresolved: {}. Before ending this turn, address them or explicitly say why each remaining item is not applicable / blocked.",
        coverage.unresolved.join("; ")
    )
}

fn gather_objective_evidence(history: &[crate::Message]) -> ObjectiveEvidence {
    let mut evidence = ObjectiveEvidence::default();
    let start = objective_evidence_start(history);

    for msg in &history[start..] {
        let mut assistant_text_for_message = String::new();
        for block in &msg.content {
            match block {
                crate::Block::Text { text } if msg.role == "assistant" => {
                    evidence.assistant_text_raw.push_str(text);
                    evidence.assistant_text_raw.push('\n');
                    assistant_text_for_message.push_str(text);
                    assistant_text_for_message.push('\n');
                }
                crate::Block::ToolUse { id, name, input } => {
                    evidence.tool_uses.insert(
                        id.clone(),
                        ObjectiveToolUse {
                            name: name.clone(),
                            input: input.clone(),
                        },
                    );
                }
                crate::Block::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    metadata,
                } => {
                    let Some(tool_use) = evidence.tool_uses.remove(tool_use_id) else {
                        continue;
                    };
                    let status = metadata.status.as_deref();
                    if tool_use.name == "bash"
                        && matches!(status, None | Some("ok" | "passed" | "failed"))
                        && let Some(cmd) = tool_use.input["command"].as_str()
                    {
                        evidence.attempted_bash_commands.push(cmd.to_string());
                    }
                    let succeeded = !is_error.unwrap_or(false)
                        && status.is_none_or(|status| {
                            !matches!(status, "error" | "failed" | "denied" | "interrupted")
                        });
                    if !succeeded {
                        continue;
                    }
                    evidence.tool_result_text.push_str(content);
                    evidence.tool_result_text.push('\n');
                    evidence.tool_names.insert(tool_use.name.clone());
                    match tool_use.name.as_str() {
                        "edit_file" | "multi_edit" | "write_file" => {
                            evidence.mutation_count += 1;
                            if let Some(path) = tool_use.input["path"].as_str() {
                                evidence.touched_paths.push(path.to_string());
                            }
                        }
                        "git_commit" => {
                            evidence.commit_count += 1;
                        }
                        "bash" => {
                            if let Some(cmd) = tool_use.input["command"].as_str() {
                                if bash_command_likely_mutates_files(cmd) {
                                    evidence.mutation_count += 1;
                                }
                                evidence.bash_commands.push(cmd.to_string());
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        if msg.role == "assistant" {
            evidence.final_assistant_text_raw = assistant_text_for_message;
        }
    }

    evidence.assistant_text = normalize_token(&evidence.assistant_text_raw);
    evidence.tool_result_text = normalize_token(&evidence.tool_result_text);
    evidence
}

fn objective_evidence_start(history: &[crate::Message]) -> usize {
    history
        .iter()
        .rposition(|msg| {
            msg.role == "user"
                && msg.content.iter().any(|block| match block {
                    crate::Block::Text { text } | crate::Block::PartialStream { text } => {
                        !text.starts_with("[runtime-note]")
                            && !text.starts_with("[queued-update]")
                            && !text.starts_with("[queued-user-update]")
                            && !text.starts_with("[prior conversation,")
                    }
                    _ => false,
                })
        })
        .unwrap_or(0)
}

fn checkpoint_satisfied(checkpoint: &str, evidence: &ObjectiveEvidence) -> bool {
    match checkpoint {
        "produce execution plan" => {
            evidence.assistant_text_raw.contains("\n1.")
                || evidence.assistant_text_raw.contains("\n- ")
                || evidence.assistant_text_raw.contains("\n* ")
                || contains_any(
                    &evidence.assistant_text,
                    &["plan", "steps", "approach", "game plan", "next i will"],
                )
        }
        "analyze current behavior and constraints" => {
            evidence.tool_names.iter().any(|name| {
                matches!(
                    name.as_str(),
                    "read_file"
                        | "rg"
                        | "fd"
                        | "jq"
                        | "awk"
                        | "http"
                        | "git_diff"
                        | "git_status"
                        | "git_log"
                        | "csvkit"
                )
            }) || contains_any(
                &evidence.assistant_text,
                &[
                    "analysis",
                    "i found",
                    "observed",
                    "current behavior",
                    "constraint",
                    "root cause",
                    "because",
                ],
            )
        }
        "implement requested changes" => {
            evidence.mutation_count > 0
                || assistant_text_has_blocked_reason(&evidence.final_assistant_text_raw)
        }
        "clean up repository" => {
            evidence.mutation_count > 0
                || evidence.commit_count > 0
                || assistant_text_has_blocked_reason(&evidence.final_assistant_text_raw)
        }
        "commit requested changes" => {
            evidence.commit_count > 0
                || assistant_text_has_blocked_reason(&evidence.final_assistant_text_raw)
        }
        "run verification checks" => {
            commands_contain(
                &evidence.attempted_bash_commands,
                &[
                    "cargo test",
                    "cargo check",
                    "cargo clippy",
                    "pytest",
                    "pnpm test",
                    "npm test",
                    "vitest",
                    "jest",
                    "go test",
                    "mvn test",
                    "gradle test",
                    "ruff",
                    "mypy",
                ],
            ) || contains_non_negated_sequence(&evidence.assistant_text, &["verified"])
                || [
                    &["checks", "pass"][..],
                    &["checks", "passed"],
                    &["tests", "pass"],
                    &["tests", "passed"],
                    &["checked", "with"],
                ]
                .iter()
                .any(|sequence| contains_non_negated_sequence(&evidence.assistant_text, sequence))
                || contains_any(
                    &evidence.tool_result_text,
                    &["test result: ok", "tests passed"],
                )
                || contains_word_sequence(&evidence.tool_result_text, &["0", "failed"])
        }
        "log decisions and follow-up improvements" => {
            evidence.tool_names.contains("todo_write")
                || evidence
                    .touched_paths
                    .iter()
                    .any(|path| is_decision_log_path(path))
                || contains_any(
                    &evidence.assistant_text,
                    &[
                        "documented",
                        "logged",
                        "recorded",
                        "recall.md",
                        "pending",
                        "follow-up",
                    ],
                )
        }
        "deliver requested outcome with verifiable steps" => {
            evidence.mutation_count > 0
                || evidence.commit_count > 0
                || !evidence.bash_commands.is_empty()
                || evidence.assistant_text_raw.trim().chars().count() >= 80
                || contains_any(
                    &evidence.assistant_text,
                    &["done", "updated", "implemented", "verified", "found"],
                )
        }
        _ => false,
    }
}

pub fn bash_command_likely_mutates_files(command: &str) -> bool {
    shell_command_segments(command)
        .into_iter()
        .any(bash_command_segment_likely_mutates_files)
}

fn bash_command_segment_likely_mutates_files(command: &str) -> bool {
    let lower = command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }

    if shell_redirection_likely_writes(&lower) {
        return true;
    }

    if segment_is_readonly_formatter_check(&lower) {
        return false;
    }

    let mutation_needles = [
        "rm ",
        "rm -",
        " rmdir ",
        "mkdir ",
        "mkdir -",
        "mv ",
        "mv -",
        "cp ",
        "cp -",
        "touch ",
        "truncate ",
        "git apply",
        "git checkout --",
        "git restore",
        "git mv",
        "git rm",
        "patch -p",
        "apply_patch",
        "cargo fmt",
        "rustfmt ",
        "gofmt -w",
        "go fmt",
        "ruff format",
        "black ",
        "prettier --write",
        "shfmt -w",
        "terraform fmt",
        "npm install",
        "pnpm install",
        "yarn install",
        "bun install",
        "install ",
        "sed -i",
        "perl -pi",
        "tee ",
        "cat >",
        "write_text(",
        "write_bytes(",
        ".write(",
        "--output-dir",
        "--out-dir",
        "--output ",
    ];
    if mutation_needles.iter().any(|needle| lower.contains(needle)) {
        return true;
    }

    lower.contains("find ") && (lower.contains(" -delete") || lower.contains(" -exec rm"))
}

fn shell_command_segments(command: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = command.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ';' | '\n' if !in_single && !in_double => {
                segments.push(&command[start..idx]);
                start = idx + ch.len_utf8();
            }
            '&' if !in_single && !in_double && chars.peek().is_some_and(|(_, c)| *c == '&') => {
                segments.push(&command[start..idx]);
                chars.next();
                start = idx + 2;
            }
            '|' if !in_single && !in_double => {
                segments.push(&command[start..idx]);
                if chars.peek().is_some_and(|(_, c)| *c == '|') {
                    chars.next();
                    start = idx + 2;
                } else {
                    start = idx + ch.len_utf8();
                }
            }
            _ => {}
        }
    }
    segments.push(&command[start..]);
    segments
}

fn segment_is_readonly_formatter_check(command: &str) -> bool {
    (command.starts_with("cargo fmt") && command.contains("--check"))
        || (command.starts_with("rustfmt") && command.contains("--check"))
        || (command.starts_with("prettier") && command.contains("--check"))
        || (command.starts_with("ruff format") && command.contains("--check"))
        || (command.starts_with("shfmt") && command.contains("-d"))
}

fn shell_redirection_likely_writes(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let mut in_single = false;
    let mut in_double = false;
    for (idx, ch) in chars.iter().enumerate() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '>' if !in_single && !in_double => {
                let rest: String = chars[idx + 1..].iter().collect();
                let target = rest
                    .trim_start_matches('>')
                    .trim_start_matches('|')
                    .trim_start();
                if target.starts_with('&') || target.starts_with("/dev/null") {
                    continue;
                }
                return true;
            }
            _ => {}
        }
    }
    false
}

pub fn assistant_text_has_blocked_reason(text: &str) -> bool {
    let normalized = normalize_token(text);
    !normalized.is_empty()
        && contains_any(
            &normalized,
            &[
                "blocked",
                "cannot",
                "can't",
                "unable",
                "not allowed",
                "permission denied",
                "need credentials",
                "missing credentials",
                "requires clarification",
                "need clarification",
                "no changes needed",
                "nothing to commit",
                "nothing to clean",
                "repo is clean",
                "repository is clean",
                "working tree is clean",
                "not applicable",
            ],
        )
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn commands_contain(commands: &[String], needles: &[&str]) -> bool {
    commands.iter().any(|cmd| {
        let lowered = cmd.to_ascii_lowercase();
        needles.iter().any(|needle| lowered.contains(needle))
    })
}

fn is_decision_log_path(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with("recall.md")
}

fn adaptive_tool_cap_for_pressure(
    session_usage: &crate::Usage,
    window_tokens: u64,
    default_cap: usize,
    min_cap: usize,
    severe_divisor: usize,
) -> usize {
    let window = window_tokens as f64;
    let used = session_usage.context_tokens() as f64;
    if window <= 0.0 || used <= 0.0 {
        return default_cap;
    }

    let pressure = (used / window).clamp(0.0, 2.0);
    if pressure >= 0.9 {
        min_cap.max(default_cap / severe_divisor)
    } else if pressure >= 0.75 {
        min_cap.max(default_cap / 2)
    } else if pressure >= 0.6 && severe_divisor == 4 {
        min_cap.max((default_cap * 3) / 4)
    } else {
        default_cap
    }
}

pub fn adaptive_tool_ui_cap_for_window(
    session_usage: &crate::Usage,
    window_tokens: u64,
    default_cap: usize,
) -> usize {
    adaptive_tool_cap_for_pressure(
        session_usage,
        window_tokens,
        default_cap,
        MIN_DYNAMIC_UI_CAP,
        4,
    )
}

pub fn adaptive_tool_result_cap_for_window(
    session_usage: &crate::Usage,
    window_tokens: u64,
    default_cap: usize,
) -> usize {
    adaptive_tool_cap_for_pressure(
        session_usage,
        window_tokens,
        default_cap,
        MIN_DYNAMIC_TOOL_RESULT_CAP,
        3,
    )
}

pub fn compress_tool_ui_content(content: &str, cap_chars: usize) -> String {
    let len = content.chars().count();
    if len <= cap_chars {
        return content.to_string();
    }
    let head = cap_chars.saturating_mul(2) / 3;
    let tail = cap_chars.saturating_sub(head).saturating_sub(32);
    let prefix = take_chars(content, head);
    let suffix = take_last_chars(content, tail);

    let mut out = format!(
        "{}\n\n…[compressed {} chars -> {} shown]\n\n{}",
        prefix,
        len,
        head + tail,
        suffix
    );
    if let Some(json_hint) = json_shape_hint(content) {
        out.push_str("\n\n[json hint] ");
        out.push_str(&json_hint);
    }
    out
}

fn take_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

fn take_last_chars(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    s.chars().skip(count - max_chars).collect()
}

fn json_shape_hint(content: &str) -> Option<String> {
    let start = content.find('{').or_else(|| content.find('['))?;
    let end = content.rfind('}').or_else(|| content.rfind(']'))?;
    if end <= start || end.saturating_sub(start) > 200_000 {
        return None;
    }
    let blob = &content[start..=end];
    let parsed: Value = serde_json::from_str(blob).ok()?;
    match parsed {
        Value::Object(map) => {
            let mut keys: Vec<String> = map.keys().take(12).cloned().collect();
            keys.sort();
            Some(format!("object keys: {}", keys.join(", ")))
        }
        Value::Array(arr) => Some(format!("array length={} (sampled)", arr.len())),
        _ => None,
    }
}

pub fn network_cache_key(name: &str, input: &Value) -> Option<String> {
    match name {
        "http" => {
            let args = input["args"].as_array()?;
            let joined = args
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let lower = joined.to_ascii_lowercase();
            if lower.contains(" post ")
                || lower.starts_with("post ")
                || lower.contains(" --data")
                || lower.contains(" -d ")
            {
                return None;
            }
            let url = first_url(&joined)?;
            Some(format!("http:get:{}", normalize_token(&url)))
        }
        "bash" => {
            let cmd = input["command"].as_str()?;
            let lower = cmd.to_ascii_lowercase();
            if !(lower.contains("curl") || lower.contains("wget")) {
                return None;
            }
            if lower.contains("--request post")
                || lower.contains(" -x post")
                || lower.contains(" --data")
                || lower.contains(" -d ")
            {
                return None;
            }
            let url = first_url(cmd)?;
            Some(format!(
                "bash:get:{}::{}",
                normalize_token(&url),
                normalize_token(cmd)
            ))
        }
        _ => None,
    }
}

pub fn dedupe_cache_short_circuit(
    cache: &HashMap<String, (String, Option<bool>)>,
    cache_key: Option<&str>,
) -> Option<(String, Option<bool>)> {
    let key = cache_key?;
    let (cached, cached_err) = cache.get(key)?;
    let display_key = summarize_cache_key(key, 120);
    Some((
        format!(
            "request dedupe cache hit: reused prior result for identical external request ({display_key})\n\n{cached}"
        ),
        *cached_err,
    ))
}

pub fn is_safe_repeated_validation_command(command: &str) -> bool {
    let normalized = command
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with("set "))
        .collect::<Vec<_>>()
        .join(" && ")
        .to_ascii_lowercase();
    let normalized = normalized.trim();
    matches!(
        normalized,
        "git status --short" | "git diff --check" | "cargo fmt --check"
    ) || [
        "cargo test",
        "cargo nextest",
        "cargo build",
        "cargo check",
        "cargo clippy",
        "cargo install",
    ]
    .iter()
    .any(|prefix| normalized == *prefix || normalized.starts_with(&format!("{prefix} ")))
}

fn primary_bash_similarity_line(command: &str) -> &str {
    command
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with("set "))
        .or_else(|| {
            command
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !line.starts_with('#'))
        })
        .or_else(|| command.lines().map(str::trim).find(|line| !line.is_empty()))
        .unwrap_or(command)
}

fn normalize_tool_failure_key(tool_name: &str, error_summary: &str) -> String {
    let normalized_error: String = error_summary
        .split_whitespace()
        .take(12)
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    format!("{tool_name}::{normalized_error}")
}

pub fn normalize_bash_similarity_key(command: &str) -> String {
    let primary = primary_bash_similarity_line(command);

    let tokens: Vec<String> = primary
        .split_whitespace()
        .map(|tok| {
            if tok.starts_with("http://") || tok.starts_with("https://") {
                "<url>".to_string()
            } else if tok.chars().all(|c| c.is_ascii_digit()) {
                "<num>".to_string()
            } else {
                tok.to_ascii_lowercase()
            }
        })
        .take(24)
        .collect();

    tokens.join(" ")
}

pub fn commands_similar(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let ta: HashSet<&str> = a.split_whitespace().collect();
    let tb: HashSet<&str> = b.split_whitespace().collect();
    if ta.is_empty() || tb.is_empty() {
        return false;
    }
    let inter = ta.intersection(&tb).count() as f32;
    let union = ta.union(&tb).count() as f32;
    if union <= 0.0 {
        return false;
    }
    (inter / union) >= 0.72
}

pub use dext_core::recovery::{FailureKind, RetryPlan, classify_stream_error};

pub fn classify_http_failure(status: u16, body: &str) -> RetryPlan {
    if matches!(status, 401 | 403 | 407)
        || dext_core::recovery::output_has_auth_failure_markers(body)
    {
        return RetryPlan {
            kind: FailureKind::Auth,
            retry: false,
        };
    }

    if matches!(status, 400 | 404 | 405 | 410 | 411 | 413 | 414 | 415 | 422) {
        return RetryPlan {
            kind: FailureKind::Schema,
            retry: false,
        };
    }

    if matches!(
        status,
        408 | 425 | 429 | 500 | 502 | 503 | 504 | 520..=525 | 527
    ) {
        return RetryPlan {
            kind: FailureKind::Transient,
            retry: true,
        };
    }

    RetryPlan {
        kind: FailureKind::Permanent,
        retry: false,
    }
}

/// True when a provider error body says the request itself no longer fits the
/// model context window (as opposed to an invalid max_tokens parameter or any
/// other schema problem). These are recoverable in place: compact history and
/// resend, instead of failing the turn and leaving the session wedged behind
/// an oversized transcript.
pub fn stream_error_is_context_overflow(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    [
        // OpenAI / ChatGPT-Codex backends.
        "context_length_exceeded",
        "exceeds the context window",
        "maximum context length",
        // Anthropic-style messages.
        "prompt is too long",
        "input is too long",
        "exceed context limit",
        "request_too_large",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

pub fn classify_transport_failure(connect: bool, timeout: bool) -> RetryPlan {
    if connect || timeout {
        RetryPlan {
            kind: FailureKind::Transient,
            retry: true,
        }
    } else {
        RetryPlan {
            kind: FailureKind::Permanent,
            retry: false,
        }
    }
}

pub fn external_failure_increment(ok: bool, auth_failure: bool) -> usize {
    if !ok || auth_failure { 1 } else { 0 }
}

pub fn should_hint_partial_delivery(
    blocked_hosts: usize,
    auth_failures: usize,
    round_failures: usize,
) -> bool {
    blocked_hosts > 0 && (auth_failures >= 2 || round_failures >= 3)
}

pub fn should_emit_partial_delivery_hint(
    already_emitted: bool,
    blocked_hosts: usize,
    auth_failures: usize,
    round_failures: usize,
) -> bool {
    !already_emitted && should_hint_partial_delivery(blocked_hosts, auth_failures, round_failures)
}

pub fn partial_delivery_hint() -> &'static str {
    "runtime guidance: external sources are repeatedly failing. Stop retries, provide the best partial deliverable now, and ask the user whether to provide credentials or approve a different data source."
}

fn summarize_cache_key(key: &str, max_chars: usize) -> String {
    let collapsed = key.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }
    let mut out: String = collapsed
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect();
    out.push('…');
    out
}

fn first_url(text: &str) -> Option<String> {
    for token in text.split_whitespace() {
        let trimmed = token
            .trim_matches('"')
            .trim_matches('\'')
            .trim_matches(',')
            .trim_matches(';');
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn normalize_token(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn successful_tool_result(tool_use_id: &str, content: &str) -> crate::Message {
        crate::Message {
            role: "user".to_string(),
            content: vec![crate::Block::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: content.to_string(),
                is_error: Some(false),
                metadata: crate::ToolResultMetadata {
                    status: Some("ok".to_string()),
                    ..crate::ToolResultMetadata::default()
                },
            }],
        }
    }

    fn failed_tool_result(tool_use_id: &str, content: &str) -> crate::Message {
        crate::Message {
            role: "user".to_string(),
            content: vec![crate::Block::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: content.to_string(),
                is_error: Some(true),
                metadata: crate::ToolResultMetadata {
                    status: Some("failed".to_string()),
                    exit_code: Some(1),
                    ..crate::ToolResultMetadata::default()
                },
            }],
        }
    }

    #[test]
    fn objective_tracker_extracts_checkpoints() {
        let tracker = ObjectiveTracker::from_user_prompt(
            "Plan then analyze current flow, implement fixes, test, and log outcomes",
        );
        let line = tracker.display_line();
        assert!(line.contains("objective:"), "{line}");
        assert!(
            tracker
                .checkpoints
                .iter()
                .any(|c| c.contains("execution plan")),
            "{:?}",
            tracker.checkpoints
        );
        assert!(
            tracker
                .checkpoints
                .iter()
                .any(|c| c.contains("analyze current behavior")),
            "{:?}",
            tracker.checkpoints
        );
        assert!(tracker.apply_fixes_allowed());

        let review_task = ObjectiveTracker::from_user_prompt("review dext for bugs");
        assert_eq!(
            review_task.checkpoints,
            vec!["analyze current behavior and constraints".to_string()]
        );
        assert!(!review_task.apply_fixes_allowed());

        let apply_after_review =
            ObjectiveTracker::from_user_prompt("review the flow, then apply fixes");
        assert!(apply_after_review.apply_fixes_allowed());
        assert!(
            apply_after_review
                .checkpoints
                .iter()
                .any(|c| c.contains("implement requested changes")),
            "{:?}",
            apply_after_review.checkpoints
        );

        let commitment =
            ObjectiveTracker::from_user_prompt("summarize the team's commitment risks");
        assert!(!commitment.apply_fixes_allowed());
        assert_eq!(
            commitment.checkpoints,
            vec!["deliver requested outcome with verifiable steps".to_string()]
        );

        let factual = ObjectiveTracker::from_user_prompt("it did IPO, this verifiable info");
        assert!(
            !factual
                .checkpoints
                .contains(&"run verification checks".to_string())
        );

        let cleanup_mention = ObjectiveTracker::from_user_prompt("what cleanup is still pending?");
        assert!(!cleanup_mention.apply_fixes_allowed());

        let commit_mention =
            ObjectiveTracker::from_user_prompt("review the last commit for regressions");
        assert!(!commit_mention.apply_fixes_allowed());

        let terse =
            ObjectiveTracker::from_user_prompt("go for it, handle my todo and cleanup master");
        assert!(
            terse
                .checkpoints
                .iter()
                .any(|c| c.contains("implement requested changes")),
            "{:?}",
            terse.checkpoints
        );
        assert!(
            terse
                .checkpoints
                .iter()
                .any(|c| c.contains("clean up repository")),
            "{:?}",
            terse.checkpoints
        );
    }

    #[test]
    fn advisory_and_plan_execution_intent_detection() {
        for advisory in [
            "plan the auth refactor",
            "please plan the migration first",
            "plan the removal, do not implement",
            "analysis only: examine provider behavior",
            "review only: inspect the change",
            "let's plan how to restructure the session store",
            "give me a plan for the upgrade",
            "review dext for bugs, don't change anything",
            "analyze the resize flicker, no changes yet",
            "how does compaction work? don't change anything",
        ] {
            let tracker = ObjectiveTracker::from_user_prompt(advisory);
            assert!(tracker.advisory_only(), "{advisory}");
            assert!(!tracker.apply_fixes_allowed(), "{advisory}");
        }

        for mutating in [
            "fix the read-only sandbox bug",
            "fix the bug but don't change anything else",
            "plan then implement the retry backoff",
            "review the flow, then apply fixes",
            "review plan mode and rip it out",
            "update docs for the new flag",
            "Review Dext and configure openai's latest gpt-6.1-sol model. Pull the documentation.",
            "OK, go ahead. Stop asking me for a go after every step unless something is dangerous; just get it done, and add that to the process review.",
        ] {
            let tracker = ObjectiveTracker::from_user_prompt(mutating);
            assert!(!tracker.advisory_only(), "{mutating}");
            assert!(tracker.apply_fixes_allowed(), "{mutating}");
        }

        for approval in [
            "go",
            "proceed",
            "approved",
            "ship it",
            "go ahead",
            "proceed with the plan",
            "execute the plan",
            "Sure go. Also review the process.",
            "go, fix the bug as well",
        ] {
            let tracker = ObjectiveTracker::from_user_prompt(approval);
            assert!(tracker.planned_execution(), "{approval}");
            assert!(tracker.apply_fixes_allowed(), "{approval}");
            assert!(!tracker.advisory_only(), "{approval}");
        }

        for neutral in [
            "summarize the team's commitment risks",
            "what cleanup is still pending?",
            "the plan mode code is in main.rs",
            "we should not go with option B",
            "should I go ahead?",
            "how should I go ahead with this?",
            "execute the plan?",
            "review dext for bugs",
            "review the workflow for asking me to go",
            "analyze provider behavior",
            "'go'. Is the word shown in the prompt",
            "do not go",
            "don't configure the model",
            "do not review and configure the model",
        ] {
            let tracker = ObjectiveTracker::from_user_prompt(neutral);
            assert!(!tracker.planned_execution(), "{neutral}");
            assert!(!tracker.advisory_only(), "{neutral}");
        }
        for withheld in [
            "don't configure the model",
            "do not review and configure the model",
            "review the workflow for asking me to go",
            "'go'. Is the word shown in the prompt",
        ] {
            assert!(
                !ObjectiveTracker::from_user_prompt(withheld).apply_fixes_allowed(),
                "{withheld}"
            );
        }
    }

    #[test]
    fn intent_review_regressions() {
        for approval in [
            "Sure go\nAlso review the process",
            "go\nConfigure the model",
        ] {
            assert!(
                ObjectiveTracker::from_user_prompt(approval).planned_execution(),
                "{approval}"
            );
        }
        for withheld in [
            "Go, but don't change anything yet",
            "Go ahead, but no changes yet",
            "The prompt says \"go ahead\"; review it",
            "The prompt says `do it`; review it",
            "Review ‘go ahead’ in the prompt",
            "Should I go ahead?",
            "How should I configure the model?",
            "Do not review and configure the model. Explain how users configure their tools",
        ] {
            let objective = ObjectiveTracker::from_user_prompt(withheld);
            assert!(!objective.planned_execution(), "{withheld}");
            assert!(!objective.apply_fixes_allowed(), "{withheld}");
        }
        for withheld in [
            "Should I commit the changes?",
            "Commit the changes, but no changes yet",
        ] {
            let objective = ObjectiveTracker::from_user_prompt(withheld);
            assert!(!objective.apply_fixes_allowed(), "{withheld}");
            assert!(
                !objective
                    .checkpoints
                    .iter()
                    .any(|item| item == "commit requested changes"),
                "{withheld}"
            );
        }
        for held in [
            "Go, but don't change anything yet",
            "Go ahead, but no changes yet",
        ] {
            assert!(
                ObjectiveTracker::from_user_prompt(held).advisory_only(),
                "{held}"
            );
        }
        for request in [
            "Please configure the model",
            "Can you configure the model?",
            "Fix the bug but don't change anything else",
            "Configure 'gpt-6.1-sol'",
            "Review and configure OpenAI's model",
        ] {
            assert!(
                ObjectiveTracker::from_user_prompt(request).apply_fixes_allowed(),
                "{request}"
            );
        }
    }

    #[test]
    fn cleanup_and_commit_task_does_not_require_an_extra_file_edit() {
        let tracker =
            ObjectiveTracker::from_user_prompt("clean up repo and committ all thats needed");
        assert_eq!(
            tracker.checkpoints,
            vec![
                "clean up repository".to_string(),
                "commit requested changes".to_string(),
            ]
        );

        let history = vec![
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::ToolUse {
                    id: "tool-commit".to_string(),
                    name: "git_commit".to_string(),
                    input: json!({"message": "chore: commit reviewed changes"}),
                }],
            },
            successful_tool_result("tool-commit", "committed as abc1234"),
        ];
        let coverage = tracker.assess_history(&history);
        assert!(coverage.unresolved.is_empty(), "{coverage:?}");
    }

    #[test]
    fn implementation_and_commit_task_keeps_both_requirements() {
        let tracker = ObjectiveTracker::from_user_prompt("fix the bug and commit the changes");
        assert!(
            tracker
                .checkpoints
                .iter()
                .any(|checkpoint| checkpoint == "implement requested changes")
        );
        assert!(
            tracker
                .checkpoints
                .iter()
                .any(|checkpoint| checkpoint == "commit requested changes")
        );

        let history = vec![
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::ToolUse {
                    id: "tool-commit".to_string(),
                    name: "git_commit".to_string(),
                    input: json!({"message": "fix: existing work"}),
                }],
            },
            successful_tool_result("tool-commit", "committed as abc1234"),
        ];
        let coverage = tracker.assess_history(&history);
        assert!(
            coverage
                .unresolved
                .iter()
                .any(|checkpoint| checkpoint == "implement requested changes"),
            "{coverage:?}"
        );
        assert!(
            coverage
                .satisfied
                .iter()
                .any(|checkpoint| checkpoint == "commit requested changes"),
            "{coverage:?}"
        );
    }

    #[test]
    fn bash_mutation_detector_recognizes_common_file_mutators() {
        for command in [
            "cargo fmt --check",
            "cargo fmt",
            "cargo fmt --check && touch out",
            "git apply fix.patch",
            "python - <<'PY'\nfrom pathlib import Path\nPath('x').write_text('y')\nPY",
            "echo ok > out.txt",
            "printf err >&2",
            "ruff format src",
            "npm install",
        ] {
            let expected = command != "cargo fmt --check" && command != "printf err >&2";
            assert_eq!(
                bash_command_likely_mutates_files(command),
                expected,
                "{command}"
            );
        }
    }

    #[test]
    fn objective_coverage_marks_completed_checkpoints() {
        let tracker = ObjectiveTracker::from_user_prompt(
            "Plan then analyze current flow, implement fixes, test, and log outcomes",
        );
        let history = vec![
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::Text {
                    text: "Plan:\n1. Inspect the provider flow\n2. Patch the bug\nI found the root cause in provider selection.".to_string(),
                }],
            },
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::ToolUse {
                    id: "tool-read".to_string(),
                    name: "read_file".to_string(),
                    input: json!({"path": "src/provider.rs"}),
                }],
            },
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::ToolUse {
                    id: "tool-edit".to_string(),
                    name: "edit_file".to_string(),
                    input: json!({"path": "src/provider.rs", "old_string": "a", "new_string": "b"}),
                }],
            },
            successful_tool_result("tool-edit", "updated src/provider.rs"),
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::ToolUse {
                    id: "tool-test".to_string(),
                    name: "bash".to_string(),
                    input: json!({"command": "cargo test --quiet"}),
                }],
            },
            crate::Message {
                role: "user".to_string(),
                content: vec![crate::Block::ToolResult {
                    tool_use_id: "tool-test".to_string(),
                    content: "test result: ok. 96 passed; 0 failed".to_string(),
                    is_error: Some(false),
                    metadata: crate::ToolResultMetadata::default(),
                }],
            },
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::ToolUse {
                    id: "tool-log".to_string(),
                    name: "write_file".to_string(),
                    input: json!({"path": "recall.md", "content": "documented fix"}),
                }],
            },
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::Text {
                    text: "Changed code, verified it with cargo test, and documented the follow-up notes.".to_string(),
                }],
            },
        ];

        let coverage = tracker.assess_history(&history);
        assert!(coverage.unresolved.is_empty(), "{:?}", coverage);
        assert_eq!(coverage.satisfied.len(), tracker.checkpoints.len());
        assert!(objective_runtime_reminder(&tracker, &history).is_none());
    }

    #[test]
    fn implementation_checkpoint_requires_mutation_or_blocked_reason() {
        let tracker = ObjectiveTracker::from_user_prompt("Implement the requested fix");
        let text_only_history = vec![crate::Message {
            role: "assistant".to_string(),
            content: vec![crate::Block::Text {
                text: "Implemented the requested change.".to_string(),
            }],
        }];
        let coverage = tracker.assess_history(&text_only_history);
        assert!(
            coverage
                .unresolved
                .iter()
                .any(|item| item == "implement requested changes"),
            "{:?}",
            coverage
        );

        let bash_mutation_history = vec![
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::ToolUse {
                    id: "tool-bash-rm".to_string(),
                    name: "bash".to_string(),
                    input: json!({"command": "rm -f reports/*.json"}),
                }],
            },
            successful_tool_result("tool-bash-rm", "exit: 0"),
        ];
        let coverage = tracker.assess_history(&bash_mutation_history);
        assert!(
            coverage
                .satisfied
                .iter()
                .any(|item| item == "implement requested changes"),
            "{:?}",
            coverage
        );

        let blocked_history = vec![crate::Message {
            role: "assistant".to_string(),
            content: vec![crate::Block::Text {
                text: "Blocked: I need clarification before changing code.".to_string(),
            }],
        }];
        let coverage = tracker.assess_history(&blocked_history);
        assert!(
            coverage
                .satisfied
                .iter()
                .any(|item| item == "implement requested changes"),
            "{:?}",
            coverage
        );
    }

    #[test]
    fn failed_mutations_and_unpaired_commits_do_not_satisfy_objective_checkpoints() {
        let tracker = ObjectiveTracker::from_user_prompt(
            "fix the bug, run the tests, and commit the changes",
        );
        let history = vec![
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::ToolUse {
                    id: "tool-edit".to_string(),
                    name: "edit_file".to_string(),
                    input: json!({"path": "src/main.rs", "old_string": "a", "new_string": "b"}),
                }],
            },
            failed_tool_result("tool-edit", "old_string not found"),
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::ToolUse {
                    id: "tool-test".to_string(),
                    name: "bash".to_string(),
                    input: json!({"command": "cargo test --release"}),
                }],
            },
            failed_tool_result("tool-test", "test result: FAILED. 1 failed"),
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::ToolUse {
                    id: "tool-commit".to_string(),
                    name: "git_commit".to_string(),
                    input: json!({"message": "fix: pending work"}),
                }],
            },
        ];

        let coverage = tracker.assess_history(&history);
        for checkpoint in ["implement requested changes", "commit requested changes"] {
            assert!(
                coverage.unresolved.iter().any(|item| item == checkpoint),
                "{checkpoint}: {coverage:?}"
            );
        }
        assert!(
            coverage
                .satisfied
                .iter()
                .any(|item| item == "run verification checks"),
            "{coverage:?}"
        );
    }

    #[test]
    fn objective_evidence_starts_at_latest_real_user_prompt() {
        let tracker = ObjectiveTracker::from_user_prompt("cleanup reports");
        let history = vec![
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::ToolUse {
                    id: "old-edit".to_string(),
                    name: "edit_file".to_string(),
                    input: json!({"path": "old.txt", "old_string": "a", "new_string": "b"}),
                }],
            },
            crate::Message {
                role: "user".to_string(),
                content: vec![crate::Block::Text {
                    text: "cleanup all json files".to_string(),
                }],
            },
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::Text {
                    text: "Done.".to_string(),
                }],
            },
        ];

        let coverage = tracker.assess_history(&history);
        assert!(
            coverage
                .unresolved
                .iter()
                .any(|item| item == "clean up repository"),
            "{:?}",
            coverage
        );
    }

    #[test]
    fn objective_runtime_reminder_lists_missing_checkpoints() {
        let tracker = ObjectiveTracker::from_user_prompt("Implement the fix and test it");
        let mut history = vec![
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::ToolUse {
                    id: "tool-edit".to_string(),
                    name: "edit_file".to_string(),
                    input: json!({"path": "src/main.rs", "old_string": "a", "new_string": "b"}),
                }],
            },
            successful_tool_result("tool-edit", "updated src/main.rs"),
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::Text {
                    text: "Implemented the requested change; one external estimate remains unverified."
                        .to_string(),
                }],
            },
        ];

        let coverage = tracker.assess_history(&history);
        assert!(
            coverage
                .satisfied
                .iter()
                .any(|item| item == "implement requested changes"),
            "{:?}",
            coverage
        );
        assert!(
            coverage
                .unresolved
                .iter()
                .any(|item| item == "run verification checks"),
            "{:?}",
            coverage
        );
        let reminder = objective_runtime_reminder(&tracker, &history).expect("reminder");
        assert!(reminder.contains("run verification checks"), "{reminder}");

        let still_unverified = vec![
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::ToolUse {
                    id: "tool-edit".to_string(),
                    name: "edit_file".to_string(),
                    input: json!({"path": "src/main.rs", "old_string": "a", "new_string": "b"}),
                }],
            },
            successful_tool_result("tool-edit", "updated src/main.rs"),
            crate::Message {
                role: "assistant".to_string(),
                content: vec![crate::Block::Text {
                    text: "The implementation is not verified; not all checks pass.".to_string(),
                }],
            },
        ];
        let negated = tracker.assess_history(&still_unverified);
        assert!(
            negated
                .unresolved
                .iter()
                .any(|item| item == "run verification checks"),
            "{negated:?}"
        );

        history.push(crate::Message {
            role: "assistant".to_string(),
            content: vec![crate::Block::Text {
                text: "All 14 reconciliation checks pass.".to_string(),
            }],
        });
        assert!(objective_runtime_reminder(&tracker, &history).is_none());
    }

    #[test]
    fn compress_tool_ui_content_adds_compression_and_json_hint() {
        let json_blob = serde_json::to_string(&json!({
            "alpha": 1,
            "beta": 2,
            "gamma": [1, 2, 3]
        }))
        .expect("json");
        let content = format!("{}\n{}\n{}", "x".repeat(300), json_blob, "y".repeat(300));
        let compressed = compress_tool_ui_content(&content, 120);
        assert!(compressed.contains("[compressed"), "{compressed}");
        assert!(compressed.contains("[json hint]"), "{compressed}");
    }

    #[test]
    fn adaptive_tool_result_cap_never_shrinks_below_standard_floor() {
        let usage = crate::Usage {
            input: 195_000,
            output: 8_000,
            cache_create: 0,
            cache_read: 0,
            cost_usd: None,
        };

        assert_eq!(
            adaptive_tool_result_cap_for_window(&usage, 200_000, 12_000),
            6_000
        );
    }

    #[test]
    fn dedupe_cache_short_circuit_preserves_cached_error_semantics() {
        let mut cache: HashMap<String, (String, Option<bool>)> = HashMap::new();
        cache.insert(
            "ok-key".to_string(),
            ("cached ok payload".to_string(), None),
        );
        cache.insert(
            "err-key".to_string(),
            ("cached err payload".to_string(), Some(true)),
        );

        let ok = dedupe_cache_short_circuit(&cache, Some("ok-key")).expect("ok hit");
        assert!(ok.0.contains("cached ok payload"), "{}", ok.0);
        assert_eq!(ok.1, None);

        let err = dedupe_cache_short_circuit(&cache, Some("err-key")).expect("err hit");
        assert!(err.0.contains("cached err payload"), "{}", err.0);
        assert_eq!(err.1, Some(true));

        assert!(dedupe_cache_short_circuit(&cache, Some("missing")).is_none());
        assert!(dedupe_cache_short_circuit(&cache, None).is_none());
    }

    #[test]
    fn phase_transition_is_monotonic_and_non_spammy() {
        let mut phase = WorkPhase::Probe;

        let (next, msg) = phase_transition(phase, PhaseTrigger::ScaleCollection)
            .expect("probe->scale transition expected");
        assert_eq!(next, WorkPhase::Scale);
        assert!(msg.contains("scaling"), "{msg}");
        phase = next;

        assert!(
            phase_transition(phase, PhaseTrigger::ScaleCollection).is_none(),
            "same transition should not spam"
        );

        let (next, _) = phase_transition(phase, PhaseTrigger::DeliverableWrite)
            .expect("scale->synthesize expected");
        assert_eq!(next, WorkPhase::Synthesize);
        phase = next;

        assert!(
            phase_transition(phase, PhaseTrigger::ScaleCollection).is_none(),
            "phase should not downgrade"
        );
        assert!(
            phase_transition(phase, PhaseTrigger::FinalResponse).is_none(),
            "already synthesize should not re-emit"
        );
    }

    #[test]
    fn turn_runtime_state_advances_phase_without_regression() {
        let mut state = TurnRuntimeState::new();
        assert_eq!(state.phase(), WorkPhase::Probe);

        let (_, msg) = state
            .advance_phase(PhaseTrigger::ScaleCollection)
            .expect("probe should advance to scale");
        assert!(msg.contains("scaling"), "{msg}");
        assert_eq!(state.phase(), WorkPhase::Scale);

        let (_, msg) = state
            .advance_phase(PhaseTrigger::DeliverableWrite)
            .expect("scale should advance to synthesize");
        assert!(msg.contains("deliverable"), "{msg}");
        assert_eq!(state.phase(), WorkPhase::Synthesize);
        assert!(state.advance_phase(PhaseTrigger::FinalResponse).is_none());
    }

    #[test]
    fn turn_runtime_state_enforces_feasibility_and_similarity_guards() {
        let mut state = TurnRuntimeState::new();
        let hosts = vec!["api.example.com".to_string()];

        let guard = state
            .feasibility_guard(&hosts, true)
            .expect("bulk call should require probe first");
        assert!(
            guard.contains("Bulk external collection is blocked"),
            "{guard}"
        );
        assert!(guard.contains("single item"), "{guard}");
        assert!(guard.contains("limit=1"), "{guard}");
        assert!(guard.contains("retry the original bulk request"), "{guard}");

        let mut probe = "{\"id\":1,\"title\":\"ok\"}".to_string();
        state.record_external_outcome(ExternalOutcomeInput {
            tool_name: "bash",
            hosts: &hosts,
            cache_key: None,
            bash_similarity_key: None,
            command: Some("curl https://api.example.com/items/1"),
            content: &mut probe,
            is_error: Some(false),
        });
        assert!(
            state.feasibility_guard(&hosts, true).is_none(),
            "bulk should be allowed after a successful single-item probe"
        );

        let mut state = TurnRuntimeState::new();
        let mut first = "short auth failure".to_string();
        state.record_external_outcome(ExternalOutcomeInput {
            tool_name: "bash",
            hosts: &hosts,
            cache_key: None,
            bash_similarity_key: Some("curl <url>"),
            command: None,
            content: &mut first,
            is_error: Some(true),
        });
        let mut second = "still short and failing".to_string();
        state.record_external_outcome(ExternalOutcomeInput {
            tool_name: "bash",
            hosts: &hosts,
            cache_key: None,
            bash_similarity_key: Some("curl <url>"),
            command: None,
            content: &mut second,
            is_error: Some(true),
        });
        let mut third = "yet another short failure".to_string();
        state.record_external_outcome(ExternalOutcomeInput {
            tool_name: "bash",
            hosts: &hosts,
            cache_key: None,
            bash_similarity_key: Some("curl <url>"),
            command: None,
            content: &mut third,
            is_error: Some(true),
        });

        let similarity = state
            .bash_similarity_guard(Some("curl <url>"))
            .expect("similarity guard should trigger after repeated unproductive attempts");
        assert!(similarity.contains("pivot strategy"), "{similarity}");
    }

    #[test]
    fn turn_runtime_state_records_circuit_breaker_and_partial_delivery_gate() {
        let mut state = TurnRuntimeState::new();
        let hosts = vec!["api.example.com".to_string()];

        let mut first = "HTTP 401 unauthorized".to_string();
        let first_obs = state.record_external_outcome(ExternalOutcomeInput {
            tool_name: "bash",
            hosts: &hosts,
            cache_key: Some("cache-key"),
            bash_similarity_key: Some("curl one"),
            command: None,
            content: &mut first,
            is_error: Some(true),
        });
        assert_eq!(first_obs.round_external_failures, 1);
        assert!(first_obs.followup_warnings.is_empty());
        assert!(!state.should_emit_partial_delivery_hint(1));

        let mut second = "HTTP 401 unauthorized".to_string();
        let second_obs = state.record_external_outcome(ExternalOutcomeInput {
            tool_name: "bash",
            hosts: &hosts,
            cache_key: Some("cache-key-2"),
            bash_similarity_key: Some("curl two"),
            command: None,
            content: &mut second,
            is_error: Some(true),
        });
        assert!(second.contains("[circuit-breaker]"), "{second}");
        assert_eq!(second_obs.round_external_failures, 1);
        assert_eq!(second_obs.followup_warnings.len(), 1, "{:?}", second_obs);
        assert!(
            state
                .blocked_host_guard(&hosts)
                .expect("host should now be blocked")
                .contains("already produced repeated auth failures")
        );
        assert!(state.should_emit_partial_delivery_hint(2));
        state.mark_partial_delivery_hint_emitted();
        assert!(!state.should_emit_partial_delivery_hint(2));

        let mut validation_state = TurnRuntimeState::new();
        for _ in 0..4 {
            let mut output = "exit: 0".to_string();
            validation_state.record_external_outcome(ExternalOutcomeInput {
                tool_name: "bash",
                hosts: &[],
                cache_key: None,
                bash_similarity_key: Some("git diff --check"),
                command: Some("git diff --check"),
                content: &mut output,
                is_error: Some(false),
            });
        }

        assert!(
            validation_state
                .bash_similarity_guard(Some("git diff --check"))
                .is_none(),
            "safe validation reruns should not trigger similarity guard"
        );

        let mut stale_failure_state = TurnRuntimeState::new();
        for _ in 0..3 {
            let mut output = "exit: 101\nlinker failed".to_string();
            stale_failure_state.record_external_outcome(ExternalOutcomeInput {
                tool_name: "bash",
                hosts: &[],
                cache_key: None,
                bash_similarity_key: Some("set -euo pipefail cargo test --release"),
                command: Some("./verify-release.sh"),
                content: &mut output,
                is_error: Some(true),
            });
        }
        assert!(
            stale_failure_state
                .bash_similarity_guard(Some("set -euo pipefail cargo test --release"))
                .is_some(),
            "repeated stale failures are blocked before edits"
        );
        stale_failure_state.mark_mutation_succeeded();
        assert!(
            stale_failure_state
                .bash_similarity_guard(Some("set -euo pipefail cargo test --release"))
                .is_none(),
            "file mutations should reset stale failed cargo-test similarity history"
        );

        let cached = state
            .dedupe_guard(Some("cache-key"))
            .expect("cache entry should be stored after first result");
        assert_eq!(cached.1, Some(true));
    }

    #[test]
    fn partial_delivery_hint_gate_and_once_semantics_hold() {
        assert!(should_hint_partial_delivery(1, 2, 0));
        assert!(should_hint_partial_delivery(1, 0, 3));
        assert!(!should_hint_partial_delivery(0, 10, 10));

        assert!(should_emit_partial_delivery_hint(false, 1, 2, 0));
        assert!(!should_emit_partial_delivery_hint(true, 1, 2, 0));
        assert!(
            partial_delivery_hint().contains("partial deliverable"),
            "{}",
            partial_delivery_hint()
        );
    }

    #[test]
    fn classify_http_failure_is_deterministic() {
        assert_eq!(
            classify_http_failure(401, "unauthorized").kind,
            FailureKind::Auth
        );
        assert!(!classify_http_failure(401, "unauthorized").retry);

        assert_eq!(
            classify_http_failure(422, "invalid conversation body").kind,
            FailureKind::Schema
        );
        assert!(!classify_http_failure(422, "invalid conversation body").retry);

        assert_eq!(
            classify_http_failure(429, "rate limited").kind,
            FailureKind::Transient
        );
        assert!(classify_http_failure(429, "rate limited").retry);

        assert_eq!(
            classify_http_failure(520, "error code: 520").kind,
            FailureKind::Transient
        );
        assert!(classify_http_failure(520, "error code: 520").retry);

        assert_eq!(
            classify_http_failure(526, "invalid SSL certificate").kind,
            FailureKind::Permanent
        );
        assert!(!classify_http_failure(526, "invalid SSL certificate").retry);

        assert_eq!(
            classify_http_failure(418, "teapot").kind,
            FailureKind::Permanent
        );
        assert!(!classify_http_failure(418, "teapot").retry);
    }

    #[test]
    fn classify_transport_failure_retries_only_transient_errors() {
        assert_eq!(
            classify_transport_failure(true, false).kind,
            FailureKind::Transient
        );
        assert!(classify_transport_failure(true, false).retry);
        assert_eq!(
            classify_transport_failure(false, true).kind,
            FailureKind::Transient
        );
        assert!(classify_transport_failure(false, true).retry);
        assert_eq!(
            classify_transport_failure(false, false).kind,
            FailureKind::Permanent
        );
        assert!(!classify_transport_failure(false, false).retry);
    }

    #[test]
    fn classify_stream_error_retries_transient_provider_errors() {
        // ZAI/GLM internal network failure — the exact shape users see in the wild.
        let zai = r#"{"error":{"code":"1234","message":"Internal network failure, error id: x, please contact customer service."},"request_id":"x"}"#;
        let plan = classify_stream_error(zai);
        assert_eq!(plan.kind, FailureKind::Transient);
        assert!(plan.retry, "ZAI 1234 must retry");

        // ZAI/GLM generic network failure — another observed shape from glm-5.1.
        let zai_network = r#"{"error":{"code":"1234","message":"Network error, error id: x, please try again later"},"request_id":"x"}"#;
        let plan = classify_stream_error(zai_network);
        assert_eq!(plan.kind, FailureKind::Transient);
        assert!(plan.retry, "ZAI network errors must retry");

        // Anthropic overloaded
        let overloaded =
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        assert!(classify_stream_error(overloaded).retry);

        // ChatGPT/Codex generic request failure observed in response.failed events.
        let chatgpt = "An error occurred while processing your request. You can retry your request, or contact us through our help center.";
        let plan = classify_stream_error(chatgpt);
        assert_eq!(plan.kind, FailureKind::Transient);
        assert!(plan.retry, "ChatGPT request-id failures must retry");

        // OpenAI server_error
        let server_err = r#"{"error":{"code":"server_error","message":"Please try again."}}"#;
        assert!(classify_stream_error(server_err).retry);
    }

    #[test]
    fn classify_stream_error_does_not_retry_permanent_failures() {
        // Context length exceeded is terminal — retry would just waste tokens.
        let ctx =
            r#"{"error":{"code":"context_length_exceeded","message":"maximum context length"}}"#;
        let plan = classify_stream_error(ctx);
        assert!(!plan.retry);
        assert_eq!(plan.kind, FailureKind::Schema);

        // Quota / usage limits
        let quota = r#"{"error":{"code":"usage_limit_reached","message":"plan exhausted"}}"#;
        assert!(!classify_stream_error(quota).retry);

        // Auth failures
        let auth = r#"{"error":{"code":"invalid_api_key","message":"Invalid API Key provided"}}"#;
        let plan = classify_stream_error(auth);
        assert!(!plan.retry);
        assert_eq!(plan.kind, FailureKind::Auth);

        // Body with no known markers → permanent default
        assert!(!classify_stream_error("nothing relevant").retry);
    }

    #[test]
    fn context_overflow_detector_matches_overflow_bodies_across_providers() {
        // The exact ChatGPT-Codex shape that wedged a live session: input larger
        // than the backend's real window, declared window notwithstanding.
        for body in [
            "context_length_exceeded: Your input exceeds the context window of this model. Please adjust your input and try again.",
            r#"{"error":{"code":"context_length_exceeded","message":"..."}}"#,
            "This model's maximum context length is 128000 tokens. However, your messages resulted in 200000 tokens.",
            "prompt is too long: 210000 tokens > 200000 maximum",
            "input length and max_tokens exceed context limit: 199999 + 8192 > 200000",
            r#"{"type":"error","error":{"type":"request_too_large","message":"Request exceeds the maximum allowed size"}}"#,
        ] {
            assert!(stream_error_is_context_overflow(body), "{body}");
        }
    }

    #[test]
    fn context_overflow_detector_rejects_non_overflow_failures() {
        // Compacting history cannot fix these; the hook must not fire on them.
        for body in [
            "max_tokens must be at least 1",
            "Internal network failure, please contact customer service",
            r#"{"error":{"code":"invalid_api_key","message":"Invalid API Key provided"}}"#,
            "stream protocol error [chatgpt-responses/response.incomplete]: response did not complete; function calls were not accepted",
            "nothing relevant",
        ] {
            assert!(!stream_error_is_context_overflow(body), "{body}");
        }
    }

    #[test]
    fn classify_stream_error_retries_responses_incomplete_protocol_errors() {
        // Exact historical failure shape that permanently failed live turns.
        for body in [
            "stream protocol error [chatgpt-responses/response.incomplete]: response did not complete; function calls were not accepted",
            "stream protocol error [openai-responses/response.incomplete]: response.incomplete conflicted with response status",
            "stream protocol error [chatgpt-responses/response.incomplete]: missing response",
        ] {
            let plan = classify_stream_error(body);
            assert!(plan.retry, "{body}");
            assert_eq!(plan.kind, FailureKind::Transient, "{body}");
        }
        // Plain incomplete markers without a protocol-error frame keep their
        // existing classification.
        assert!(!classify_stream_error("incomplete_details: max_output_tokens").retry);
    }

    #[test]
    fn classify_stream_error_treats_unknown_numeric_codes_as_transient() {
        // Any string-wrapped numeric code from an unknown provider gets the
        // benefit of the doubt — usually server-side hiccups.
        let odd = r#"{"error":{"code":"9999","message":"something happened"}}"#;
        let plan = classify_stream_error(odd);
        assert!(plan.retry, "unknown numeric code should retry");
        assert_eq!(plan.kind, FailureKind::Transient);

        // Non-numeric unknown code stays permanent (can't tell if it's server-side).
        let named = r#"{"error":{"code":"weird_thing","message":"no clues"}}"#;
        assert!(!classify_stream_error(named).retry);
    }

    #[test]
    fn external_failure_increment_never_double_counts_single_attempt() {
        assert_eq!(external_failure_increment(true, false), 0);
        assert_eq!(external_failure_increment(false, false), 1);
        assert_eq!(external_failure_increment(true, true), 1);
        assert_eq!(external_failure_increment(false, true), 1);
    }

    #[test]
    fn network_cache_key_tracks_get_not_post() {
        let get_http = network_cache_key(
            "http",
            &json!({"args": ["GET", "https://api.example.com/v1/items?limit=20"]}),
        );
        assert!(get_http.is_some(), "GET should be cacheable");

        let post_http = network_cache_key(
            "http",
            &json!({"args": ["POST", "https://api.example.com/v1/items", "--data", "x=1"]}),
        );
        assert!(
            post_http.is_none(),
            "POST should not be cached by GET cache"
        );

        let get_bash = network_cache_key(
            "bash",
            &json!({"command": "curl -s https://api.example.com/v1/items?limit=20"}),
        );
        assert!(get_bash.is_some(), "curl GET should be cacheable");

        let post_bash = network_cache_key(
            "bash",
            &json!({"command": "curl -s -X POST https://api.example.com/v1/items -d 'x=1'"}),
        );
        assert!(post_bash.is_none(), "curl POST should not be cached");
    }

    #[test]
    fn bash_similarity_key_ignores_comments_and_urls() {
        let a = normalize_bash_similarity_key(
            "# attempt one\ncurl -s https://api.one.example.com/v1/items | jq '.items[] | .id'",
        );
        let b = normalize_bash_similarity_key(
            "# retry with mirror\ncurl -s https://api.two.example.com/v1/items | jq '.items[] | .id'",
        );
        let cargo =
            normalize_bash_similarity_key("set -euo pipefail\ncargo test --release --quiet");
        assert_eq!(cargo, "cargo test --release --quiet");
        assert!(is_safe_repeated_validation_command(
            "set -euo pipefail\ncargo build --release"
        ));
        assert!(is_safe_repeated_validation_command(
            "set -euo pipefail\ncargo install --path . --force"
        ));
        assert!(
            commands_similar(&a, &b),
            "expected near-duplicate bash fallback commands to be similar"
        );
    }

    #[test]
    fn adaptive_caps_shrink_under_context_pressure() {
        let usage = crate::Usage {
            input: 185_000,
            output: 8_000,
            cache_create: 0,
            cache_read: 0,
            cost_usd: None,
        };
        let ui_cap = adaptive_tool_ui_cap_for_window(&usage, 200_000, 8_000);
        let result_cap = adaptive_tool_result_cap_for_window(&usage, 200_000, 12_000);

        assert!(ui_cap < 8_000, "ui cap should shrink");
        assert!(result_cap < 12_000, "result cap should shrink");
    }

    #[test]
    fn empty_tool_call_streak_triggers_after_threshold() {
        let mut state = TurnRuntimeState::new();
        assert!(state.empty_tool_call_loop_note().is_none());

        for _ in 0..3 {
            state.record_empty_tool_calls(2);
            assert!(state.empty_tool_call_loop_note().is_none());
        }

        state.record_empty_tool_calls(1);
        let note = state
            .empty_tool_call_loop_note()
            .expect("should trigger after 4 consecutive empty-call iterations");
        assert!(note.contains("empty arguments"), "{note}");
        assert!(note.contains("edit_file"), "{note}");

        // Does not re-trigger
        state.record_empty_tool_calls(5);
        assert!(state.empty_tool_call_loop_note().is_none());
    }

    #[test]
    fn empty_tool_call_streak_resets_on_nonempty_calls() {
        let mut state = TurnRuntimeState::new();

        for _ in 0..3 {
            state.record_empty_tool_calls(1);
        }
        assert!(state.empty_tool_call_loop_note().is_none());

        // Non-empty calls reset the streak
        state.record_empty_tool_calls(0);
        assert!(state.empty_tool_call_loop_note().is_none());

        for _ in 0..3 {
            state.record_empty_tool_calls(1);
        }
        assert!(state.empty_tool_call_loop_note().is_none());

        state.record_empty_tool_calls(1);
        assert!(state.empty_tool_call_loop_note().is_some());

        for _ in 0..3 {
            state.record_empty_tool_calls(1);
            assert!(state.empty_tool_call_halt_message().is_none());
        }
        state.record_empty_tool_calls(1);
        let halt = state
            .empty_tool_call_halt_message()
            .expect("should halt after continued empty-call loop");
        assert!(halt.contains("halted"), "{halt}");
        assert!(halt.contains("switch provider/model"), "{halt}");
    }
}
