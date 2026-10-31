//! The JSON-RPC 2.0 protocol of the public API (docs/API.md), shared by its
//! transports: `spindle api` (stdin/stdout, `api::stdio`) and the embedded package
//! (`spindle-embed`). A transport frames lines and runs sessions; this module parses
//! requests, checks params, names faults and shapes every frame, so both answer the
//! same methods with the same bytes.
use crate::driver::{Event, Settings};
use crate::rewind::RewindTargetKind;
use dext_core::turn::{Phase, Stop};
use dext_transcript::{Block, Message};
use serde_json::{Map, Value, json};
use std::panic::{AssertUnwindSafe, catch_unwind};

/// Longest request line (a prompt may carry attachments).
pub const MAX_LINE: usize = 64 * 1024 * 1024;
/// Longest prompt text.
pub const MAX_PROMPT: usize = 4 * 1024 * 1024;
/// Steering messages (or state changes) that may wait for one running turn.
pub const MAX_STEERS: usize = 32;

pub type Outcome = std::result::Result<Value, Fault>;

/// A JSON-RPC error: numeric `code`, `message`, and `data.code`, a stable name shared
/// with the HTTP API.
#[derive(Debug)]
pub struct Fault {
    pub code: i64,
    pub kind: &'static str,
    pub message: String,
    pub data: Map<String, Value>,
}
impl Fault {
    pub fn new(code: i64, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            kind,
            message: message.into(),
            data: Map::new(),
        }
    }
    pub fn with(mut self, key: &str, value: Value) -> Self {
        self.data.insert(key.into(), value);
        self
    }
    pub fn to_json(&self) -> Value {
        let mut data = self.data.clone();
        data.insert("code".into(), json!(self.kind));
        json!({"code": self.code, "message": self.message, "data": data})
    }
}
pub fn invalid_request(message: impl Into<String>) -> Fault {
    Fault::new(-32600, "invalid_request", message)
}
pub fn bad(message: impl Into<String>) -> Fault {
    Fault::new(-32602, "bad_request", message)
}
pub fn internal(message: impl Into<String>) -> Fault {
    Fault::new(-32603, "internal", message)
}
pub fn failed(message: impl Into<String>) -> Fault {
    Fault::new(-32000, "failed", message)
}
pub fn busy(message: impl Into<String>) -> Fault {
    Fault::new(-32001, "busy", message)
}
pub fn not_running(message: impl Into<String>) -> Fault {
    Fault::new(-32002, "not_running", message)
}
pub fn not_found(message: impl Into<String>) -> Fault {
    Fault::new(-32004, "not_found", message)
}
/// How a budget refusal's message starts (`agent::cost`): the next model request would
/// pass the thread's budget, so it was not sent; Continue sends it once the budget
/// allows it. (A budget on a model with no price is refused as `failed`: raising the
/// budget would not help.)
pub const BUDGET_REFUSAL: &str = "spending limit:";
/// A turn-like operation's failure: `budget` when the thread's budget refused the next
/// model request (nothing was sent), `failed` otherwise.
pub fn turn_failed(message: impl Into<String>) -> Fault {
    let message = message.into();
    match message.starts_with(BUDGET_REFUSAL) {
        true => Fault::new(-32007, "budget", message),
        false => failed(message),
    }
}
pub fn conflict(message: impl Into<String>) -> Fault {
    Fault::new(-32005, "conflict", message)
}

/// A successful result or an error, as the response frame for request `id`.
pub fn reply_frame(id: &Value, outcome: Outcome) -> Value {
    match outcome {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(fault) => json!({"jsonrpc": "2.0", "id": id, "error": fault.to_json()}),
    }
}

/// An `event` notification from `session`, caused by request `request` (or null).
pub fn event_frame(session: &str, request: &Value, event: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "event",
        "params": {"session": session, "request": request, "event": event},
    })
}

/// A request's params, with typed accessors that refuse wrong types.
pub struct Params(pub Map<String, Value>);
impl Params {
    pub fn allow(&self, keys: &[&str]) -> std::result::Result<(), Fault> {
        match self.0.keys().find(|key| !keys.contains(&key.as_str())) {
            Some(key) => Err(bad(format!("unknown parameter `{key}`"))),
            None => Ok(()),
        }
    }
    pub fn str(&self, key: &str) -> std::result::Result<Option<&str>, Fault> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(Some(text)),
            Some(_) => Err(bad(format!("`{key}` must be a string"))),
        }
    }
    pub fn bool(&self, key: &str) -> std::result::Result<Option<bool>, Fault> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(*value)),
            Some(_) => Err(bad(format!("`{key}` must be true or false"))),
        }
    }
    pub fn u64(&self, key: &str) -> std::result::Result<Option<u64>, Fault> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_u64()
                .map(Some)
                .ok_or_else(|| bad(format!("`{key}` must be a non-negative integer"))),
        }
    }
    pub fn required(&self, key: &str) -> std::result::Result<&str, Fault> {
        self.str(key)?
            .filter(|text| !text.is_empty())
            .ok_or_else(|| bad(format!("`{key}` is required")))
    }
}

/// Files given with a prompt or a new session: `[{path, text | base64, executable}]`.
pub fn files_param(
    value: Option<&Value>,
) -> std::result::Result<Vec<(String, Vec<u8>, bool)>, Fault> {
    let list = match value {
        None | Some(Value::Null) => return Ok(vec![]),
        Some(Value::Array(list)) => list,
        Some(_) => return Err(bad("`files` must be an array")),
    };
    let mut files = Vec::with_capacity(list.len());
    for item in list {
        let Some(item) = item.as_object() else {
            return Err(bad(
                "each file is an object with `path` and `text` or `base64`",
            ));
        };
        let item = Params(item.clone());
        item.allow(&["path", "text", "base64", "executable"])?;
        let path = item.required("path")?;
        if !crate::workspace::valid_path(path) {
            return Err(bad(format!(
                "`{path}` is not a workspace path (relative, without empty, `.` or `..` parts)"
            )));
        }
        let bytes = match (item.str("text")?, item.str("base64")?) {
            (Some(text), None) => text.as_bytes().to_vec(),
            (None, Some(encoded)) => base64_decode(encoded)
                .ok_or_else(|| bad(format!("`{path}`: `base64` is not standard base64")))?,
            _ => {
                return Err(bad(format!(
                    "`{path}`: give exactly one of `text` and `base64`"
                )));
            }
        };
        files.push((
            path.to_string(),
            bytes,
            item.bool("executable")?.unwrap_or(false),
        ));
    }
    Ok(files)
}

pub const SETTINGS: [&str; 14] = [
    "provider",
    "model",
    "network",
    "toolchain",
    "sources",
    "isolation",
    "effort",
    "compaction",
    "max_usd",
    "tools",
    "builtin_tools",
    "instructions",
    "resend_unknown",
    "tasks",
];

/// How a call is answered: now by the reading thread, or later by a session worker.
pub enum Reply {
    Now(Outcome),
    Later,
}

/// A turn-like operation a request starts: `prompt`, `continue`, `retry`, `rewind`
/// or `task_start` (its request is answered when the turn ends).
pub enum TurnOp {
    Prompt {
        text: String,
        files: Vec<(String, Vec<u8>, bool)>,
        state: Option<Value>,
    },
    Continue {
        until_unknown: bool,
    },
    Retry,
    Rewind {
        revision: u64,
    },
    /// `task_start`: runs an app-started task to the end of its turn.
    Task {
        task: String,
        input: Value,
        report: String,
    },
}

/// The operation `method` (one of the turn-like ones) asks for, its params checked;
/// the name is how `busy` faults and `list` call it.
pub fn turn_op(
    method: &str,
    params: &Params,
) -> std::result::Result<(&'static str, TurnOp), Fault> {
    Ok(match method {
        "prompt" => {
            params.allow(&["dir", "text", "files", "state", "expected_head"])?;
            let text = params.required("text")?;
            if text.trim().is_empty() || text.len() > MAX_PROMPT {
                return Err(bad("`text` must be 1 byte to 4 MiB and not blank"));
            }
            let files = files_param(params.0.get("files"))?;
            let text = text.to_string();
            let state = match params.0.get("state") {
                None | Some(Value::Null) => None,
                Some(patch)
                    if patch.is_object()
                        && patch.to_string().len() <= crate::transition::MAX_STATE =>
                {
                    Some(patch.clone())
                }
                Some(_) => {
                    return Err(bad(
                        "`state` is a JSON merge patch: an object of at most 1 MiB",
                    ));
                }
            };
            ("prompt", TurnOp::Prompt { text, files, state })
        }
        "continue" => {
            params.allow(&["dir", "expected_head", "until"])?;
            let until_unknown = match params.str("until")? {
                None => false,
                Some("unknown") => true,
                Some(_) => return Err(bad("`until` is unknown")),
            };
            ("continue", TurnOp::Continue { until_unknown })
        }
        "retry" => {
            params.allow(&["dir", "expected_head"])?;
            ("retry", TurnOp::Retry)
        }
        "task_start" => {
            params.allow(&[
                "dir",
                "task",
                "prompt",
                "model",
                "effort",
                "files",
                "settings",
                "report",
                "expected_head",
            ])?;
            let report = params.str("report")?.unwrap_or("wait").to_string();
            if !matches!(report.as_str(), "wait" | "queue") {
                return Err(bad("report must be wait or queue"));
            }
            let task = params
                .str("task")?
                .filter(|task| {
                    (1..=80).contains(&task.len())
                        && task
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                })
                .ok_or_else(|| bad("`task` is an id of 1-80 letters, digits, `-`, `_` or `.`"))?
                .to_string();
            let mut input = params.0.clone();
            input.remove("dir");
            input.remove("task");
            input.remove("expected_head");
            input.remove("report");
            let input = Value::Object(input);
            if let Some(reason) = crate::driver::task_tool::refusal(&input) {
                return Err(bad(reason));
            }
            (
                "task",
                TurnOp::Task {
                    task,
                    input,
                    report,
                },
            )
        }
        _ => {
            params.allow(&["dir", "revision", "expected_head"])?;
            let revision = params
                .u64("revision")?
                .ok_or_else(|| bad("`revision` is required (see `targets`)"))?;
            ("rewind", TurnOp::Rewind { revision })
        }
    })
}

/// What runs the methods: a transport's sessions. `call` answers now or later (then
/// the backend replies to `id` itself, through the transport's frames).
pub trait Backend {
    fn call(&self, method: &str, params: Params, id: Option<Value>) -> Reply;
}

/// Parses one request line and hands it to `backend`; frames answered now (and
/// malformed requests) go to `send`. A notification (no `id`) is never answered.
pub fn handle(backend: &impl Backend, line: &[u8], send: impl Fn(&Value)) {
    let request: Value = match serde_json::from_slice(line) {
        Ok(request) => request,
        Err(error) => {
            return send_reply(
                &send,
                &Value::Null,
                Err(Fault::new(
                    -32700,
                    "parse_error",
                    format!("not JSON: {error}"),
                )),
            );
        }
    };
    let Value::Object(mut request) = request else {
        let why = if request.is_array() {
            "batches are not supported; send one request per line"
        } else {
            "a request is a JSON object"
        };
        return send_reply(&send, &Value::Null, Err(invalid_request(why)));
    };
    let id = request.remove("id");
    if let Some(id) = &id
        && !(id.is_string() || id.is_number() || id.is_null())
    {
        return send_reply(
            &send,
            &Value::Null,
            Err(invalid_request("`id` must be a string, a number or null")),
        );
    }
    let answer = |outcome| {
        if let Some(id) = &id {
            send_reply(&send, id, outcome);
        }
    };
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return answer(Err(invalid_request("`jsonrpc` must be \"2.0\"")));
    }
    let Some(Value::String(method)) = request.remove("method") else {
        return answer(Err(invalid_request("`method` must be a string")));
    };
    let params = match request.remove("params") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(params)) => params,
        Some(_) => return answer(Err(bad("`params` must be an object"))),
    };
    let called = catch_unwind(AssertUnwindSafe(|| {
        backend.call(&method, Params(params), id.clone())
    }));
    match called {
        Ok(Reply::Now(outcome)) => answer(outcome),
        Ok(Reply::Later) => {}
        Err(_) => answer(Err(internal("the call panicked"))),
    }
}

fn send_reply(send: &impl Fn(&Value), id: &Value, outcome: Outcome) {
    send(&reply_frame(id, outcome));
}

impl Event<'_> {
    /// The event's stable JSON form: an object whose `type` is the variant's name in
    /// snake_case and whose other fields keep the variant's field names. Live output
    /// travels base64-encoded (`data`), with `stream` `stdout` or `stderr`.
    pub fn to_json(&self) -> Value {
        // Exhaustive on purpose: a new variant must be given its JSON form here.
        match self {
            Event::Prompt { text } => json!({"type": "prompt", "text": text}),
            Event::Compacted { before, after } => {
                json!({"type": "compacted", "before": before, "after": after})
            }
            Event::ModelStep {
                provider,
                model,
                step,
            } => json!({"type": "model_step", "provider": provider, "model": model, "step": step}),
            Event::ModelDelta { text } => json!({"type": "model_delta", "text": text}),
            Event::ModelTextWithdrawn => json!({"type": "model_text_withdrawn"}),
            Event::ModelText { text } => json!({"type": "model_text", "text": text}),
            Event::Usage { input, output } => {
                json!({"type": "usage", "input": input, "output": output})
            }
            Event::Cost {
                usd,
                turn,
                total,
                billing,
            } => {
                json!({"type": "cost", "usd": usd, "turn": turn, "total": total, "billing": billing})
            }
            Event::ResponseReused => json!({"type": "response_reused"}),
            Event::ToolCall { id, name, input } => {
                json!({"type": "tool_call", "id": id, "name": name, "input": input})
            }
            Event::ToolSkipped => json!({"type": "tool_skipped"}),
            Event::ToolRefused { reason } => json!({"type": "tool_refused", "reason": reason}),
            Event::ToolReused => json!({"type": "tool_reused"}),
            Event::ToolInterrupted => json!({"type": "tool_interrupted"}),
            Event::Unisolated { reason } => json!({"type": "unisolated", "reason": reason}),
            Event::Display { items } => json!({"type": "display", "display": items}),
            Event::ToolResult { content, failed } => {
                json!({"type": "tool_result", "content": content, "failed": failed})
            }
            Event::TaskReported { task, text } => {
                json!({"type":"task_reported","task":task,"name":crate::reads::task_name(task),"text":text})
            }
            Event::Task { task, event } => json!({"type":"task","task":task,"event":event}),
            Event::AppToolDetails { call, details } => {
                json!({"type":"app_tool_details","call":call,"details":details})
            }
            Event::Nested { label } => json!({"type": "nested", "label": label}),
            Event::Output { stream, bytes } => json!({
                "type": "output",
                "stream": if *stream == 2 { "stderr" } else { "stdout" },
                "data": base64_encode(bytes),
            }),
            Event::OutputEnd => json!({"type": "output_end"}),
            Event::Stopped {
                reason,
                steps,
                revision,
                files,
                complete,
            } => json!({
                "type": "stopped",
                "reason": reason,
                "steps": steps,
                "revision": revision,
                "files": files,
                "complete": complete,
            }),
            Event::Rewound {
                target,
                revision,
                kind,
                abandoned,
            } => json!({
                "type": "rewound",
                "target": target,
                "revision": revision,
                "kind": rewind_kind(*kind),
                "abandoned": abandoned,
            }),
            Event::State { revision, changed } => {
                json!({"type": "state", "revision": revision, "changed": changed})
            }
            Event::StateRefused { reason } => json!({"type": "state_refused", "reason": reason}),
            Event::AppCallsRewound { calls } => {
                json!({"type": "app_calls_rewound", "calls": calls})
            }
            Event::Retrying { target, head } => {
                json!({"type": "retrying", "target": target, "head": head})
            }
            Event::RetryingFirst { path } => {
                json!({"type": "retrying_first", "path": path.to_string_lossy()})
            }
            Event::NothingToContinue => json!({"type": "nothing_to_continue"}),
            Event::Extended { steps } => json!({"type": "extended", "steps": steps}),
            Event::Steered { text } => json!({"type": "steered", "text": text}),
            Event::ReplyCut => json!({"type": "reply_cut"}),
            Event::ReplyNotCut => json!({"type": "reply_not_cut"}),
            Event::PackView {
                pack,
                title,
                markdown,
            } => json!({"type": "pack_view", "pack": pack, "title": title, "markdown": markdown}),
            Event::SettingsApplied { text } => json!({"type": "settings_applied", "text": text}),
            Event::ProviderRetry {
                status,
                seconds,
                attempt,
                compacting,
            } => json!({
                "type": "provider_retry",
                "status": status,
                "seconds": seconds,
                "attempt": attempt,
                "compacting": compacting,
            }),
        }
    }
}

/// Stable name of a rewind target kind: `settled` or `before_model`.
pub fn rewind_kind(kind: RewindTargetKind) -> &'static str {
    match kind {
        RewindTargetKind::Settled => "settled",
        RewindTargetKind::BeforeModel => "before_model",
    }
}

/// Stable name of a turn phase; `stopped` carries its reason in `stop_code`.
pub fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Ready => "ready",
        Phase::Preparing => "preparing",
        Phase::ModelWait => "model_wait",
        Phase::ToolsWait => "tools_wait",
        Phase::FinalReview => "final_review",
        Phase::AfterTools => "after_tools",
        Phase::Stopped(_) => "stopped",
    }
}

/// Stable code of why a turn stopped (`agent::stop_text` is the sentence).
pub fn stop_code(stop: Stop) -> &'static str {
    match stop {
        Stop::Complete => "complete",
        Stop::Iterations => "step_limit",
        Stop::Budget => "budget",
        Stop::ToolPolicy => "tool_policy",
        Stop::ContentFilter => "content_filter",
        Stop::RecoveryBudget => "recovery_budget",
        Stop::Cancelled => "cancelled",
    }
}

/// A conversation message in its stable form: `role` and `content` blocks. Thinking
/// signatures, redacted reasoning and provider-specific reasoning items are opaque to
/// clients and left out; their blocks remain, so positions are preserved.
pub fn message_json(message: &Message) -> Value {
    let content: Vec<Value> = message.content.iter().map(block_json).collect();
    json!({"role": message.role, "content": content})
}

fn block_json(block: &Block) -> Value {
    match block {
        Block::Text { text } => json!({"type": "text", "text": text}),
        Block::Thinking { text, signature: _ } => json!({"type": "thinking", "text": text}),
        Block::RedactedThinking { data: _ } => json!({"type": "redacted_thinking"}),
        Block::ResponsesReasoning { item: _ } => json!({"type": "reasoning"}),
        Block::ToolUse { id, name, input } => {
            json!({"type": "tool_use", "id": id, "name": name, "input": input})
        }
        Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            metadata,
        } => json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error.unwrap_or(false),
            "exit_code": metadata.exit_code,
            "duration_ms": metadata.duration_ms,
        }),
        Block::ImageReference {
            path,
            media_type,
            width,
            height,
            source_sha256,
            disclosure_target: _,
        } => json!({
            "type": "image",
            "path": path,
            "media_type": media_type,
            "width": width,
            "height": height,
            "sha256": source_sha256,
        }),
        Block::ImageArtifact { reference } => json!({
            "type": "image_artifact",
            "digest": reference.digest,
            "size": reference.byte_length,
            "width": reference.width,
            "height": reference.height,
        }),
        Block::PartialStream { text } => json!({"type": "partial", "text": text}),
    }
}

/// A session's settings in their stable form.
pub fn settings_json(settings: &Settings) -> Value {
    let Settings {
        provider,
        model,
        network,
        toolchain,
        sources,
        isolation,
        effort,
        compaction,
        max_usd,
        self_edit,
        tools,
        builtin_tools,
        instructions,
        resend_unknown,
        tasks,
        task_policy,
        runner,
    } = settings;
    let mut value = json!({
        "provider": provider,
        "model": model,
        "network": network,
        "toolchain": toolchain,
        "sources": sources.iter().map(|s| s.to_string_lossy()).collect::<Vec<_>>(),
        "isolation": isolation.as_str(),
        "effort": effort.as_str(),
        "compaction": compaction.as_str(),
        "max_usd": max_usd,
        "self_edit": self_edit,
        "tools": tools.iter().map(crate::driver::app_tools::AppTool::to_json).collect::<Vec<_>>(),
        "builtin_tools": builtin_tools.as_str(),
        "instructions": instructions,
        "resend_unknown": resend_unknown,
        "tasks": if *tasks { task_policy.to_json() } else { json!(false) },
    });
    // Only an embedded session has a runner; others keep the form they always had.
    if *runner != crate::driver::Runner::None {
        value["runner"] = json!(runner.as_str());
    }
    value
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (RFC 4648 section 4).
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(BASE64[(n >> 18) as usize & 63] as char);
        out.push(BASE64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            BASE64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Decodes standard base64 (padding required, no whitespace); None if malformed.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let value = |b: u8| BASE64.iter().position(|&c| c == b).map(|v| v as u32);
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.chunks(4).enumerate() {
        let last = index + 1 == bytes.len() / 4;
        let pad = chunk.iter().rev().take_while(|&&b| b == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &b in &chunk[..4 - pad] {
            n = (n << 6) | value(b)?;
        }
        n <<= 6 * pad as u32;
        let decoded = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&decoded[..3 - pad]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_budget_refusal_is_its_own_kind_and_other_failures_stay_failed() {
        let refused = turn_failed(format!("{BUDGET_REFUSAL} this thread has spent $1"));
        assert_eq!((refused.code, refused.kind), (-32007, "budget"));
        let failed = turn_failed("the provider returned HTTP 500");
        assert_eq!((failed.code, failed.kind), (-32000, "failed"));
    }
}
