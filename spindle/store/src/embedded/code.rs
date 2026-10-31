//! The code-mode bridge of the embedded package. A script run by the app's runner (in
//! its bash) asks for Spindle calls as a native run_code script does: `model`, `tool`,
//! `batch`, `search`, `describe`, `limits`. The store checks, prepares and journals each
//! child exactly as native code mode does (`code_plan`, `code_reserve`, `code_record`);
//! the app does the I/O (a model request with its credentials and fetch, an app tool
//! with its handler, an HTTP request its grants allow), and this module turns what it
//! brings back into the child's result.
use super::env::Catalog;
use crate::code_mode::{Host as CodeHost, Limits, validate_input};
use crate::driver::app_tools::{AppAnswer, AppTool};
use dext_core::model::{Encoding, Identity, ModelWait, PreparedRequest};
use dext_native::attempt::{AttemptError, WireAttempt};
use dext_transcript::Block;
use dext_transcript::checkpoint::NativeResponse;
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// A script's limits per command: within code mode's ceilings, above an unconfigured
/// native host's defaults (no code policy here; nested models run on the user's key).
pub(crate) const LIMITS: Limits = Limits {
    calls: 128,
    model_calls: 32,
    model_budget: 1024 * 1024,
    parallel: 4,
};

/// What a script's children may use: the session's app tools and its catalog.
pub(crate) struct Bridge<'a> {
    pub tools: &'a [AppTool],
    pub catalog: &'a Catalog,
    pub provider: &'a str,
    pub model: &'a str,
    /// The running turn's execution id (a nested request's identity).
    pub session: &'a str,
    /// The app's HTTP grants (a code policy's `http` list).
    pub http: &'a Value,
    /// The session's budget and what it (and its task tree) spent, when it has one.
    pub budget: Option<(f64, f64)>,
}

impl CodeHost for Bridge<'_> {
    /// Under a budget, new nested model calls fit at their worst case beside what was
    /// spent, or none is sent (as a native host admits them).
    fn admit_spend(&mut self, prepared: &[&Value], _journal: &[&Value]) -> Result<(), String> {
        let Some((max, spent)) = self.budget else {
            return Ok(());
        };
        let cost = crate::spend::nested_worst(Some(self.catalog.provider_catalog()), prepared)
            .map_err(|(provider, model)| crate::spend::unpriced_text(&provider, &model, max))?;
        if spent + cost > max {
            return Err(format!(
                "spending limit: this thread has spent {} of its {} budget and the new nested call(s) could cost up to {}; nothing was sent",
                crate::spend::dollars(spent),
                crate::spend::dollars(max),
                crate::spend::dollars(cost)
            ));
        }
        Ok(())
    }

    fn discover(&mut self, method: &str, input: &Value) -> Result<Value, String> {
        if method == "describe" {
            let name = input["name"].as_str().ok_or("tool name required")?;
            let tool = self
                .tools
                .iter()
                .find(|tool| tool.name == name)
                .ok_or_else(|| format!("there is no tool {name}; tools.search lists them"))?;
            return Ok(json!({"name": tool.name, "description": tool.description,
                "risk": "app", "input_schema": tool.input_schema, "protocol": "app"}));
        }
        let query = input["query"].as_str().unwrap_or("").to_lowercase();
        Ok(Value::Array(
            self.tools
                .iter()
                .map(|tool| json!({"name": tool.name, "description": tool.description, "risk": "app"}))
                .filter(|tool| {
                    format!("{} {}", tool["name"], tool["description"])
                        .to_lowercase()
                        .contains(&query)
                })
                .take(20)
                .collect(),
        ))
    }

    fn prepare(&mut self, method: &str, input: &Value) -> Result<Value, String> {
        match method {
            "model" => {
                // As native code mode routes: the thread's model unless the script names
                // another provider (its default model) or model.
                let explicit = input["model"].as_str();
                let (provider, model) = match input["provider"].as_str() {
                    Some(provider) => (
                        provider,
                        explicit.or((provider == self.provider).then_some(self.model)),
                    ),
                    None => (self.provider, explicit.or(Some(self.model))),
                };
                let route = self.catalog.route(provider, model)?;
                crate::shape::code_model_request(input, &route, self.session, &|| {
                    self.catalog.image_models()
                })
            }
            "http" => {
                // Checked and frozen as native code mode does; the app sends it, with
                // the most of its body the grant brings back.
                let mut prepared = crate::code_http::prepare(self.http, input)?;
                let grant = prepared["grant"].as_str().unwrap_or_default();
                prepared["max_response_bytes"] =
                    json!(crate::code_http::response_limit(self.http, grant));
                Ok(prepared)
            }
            _ => {
                let name = input["name"].as_str().ok_or("tool name required")?;
                let tool = self
                    .tools
                    .iter()
                    .find(|tool| tool.name == name)
                    .ok_or_else(|| format!("there is no tool {name}; tools.search lists them"))?;
                validate_input(&tool.input_schema, &input["arguments"], 0)?;
                Ok(
                    json!({"kind": "app", "name": name, "input": input["arguments"],
                    "max_output": tool.max_output, "session": self.session}),
                )
            }
        }
    }

    fn limits(&self) -> Limits {
        LIMITS
    }

    fn execute(&mut self, _: &Value, _: &AtomicBool, _: Duration) -> Value {
        // Children run in the app (`code_answer` brings their results).
        json!({"ok": false, "may_have_run": false, "error": "this host runs children through the app"})
    }
}

/// A model child's result from what the app's request brought back: the HTTP status and
/// body, or why it failed (`sent`: whether the request may have left). Worded as native
/// code mode words it: a request that left may have run, its status said.
pub(crate) fn model_result(prepared: &Value, answer: &Value) -> Value {
    if let Some(error) = answer["error"].as_str() {
        return json!({"ok": false, "error": error, "may_have_run": answer["sent"] != false});
    }
    // A reply that does not read as a response: whether the request ran is not known.
    let Some(status) = answer["status"]
        .as_u64()
        .filter(|s| (100..=999).contains(s))
    else {
        return json!({"ok": false, "may_have_run": true, "unknown": true,
            "error": "the app's answer has no response status"});
    };
    if status != 200 {
        let detail = answer["body"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(400)
            .collect::<String>();
        return json!({"ok": false, "may_have_run": true, "status": status,
            "error": format!("the provider answered HTTP {status}: {detail}")});
    }
    let body = answer["body"].as_str().unwrap_or("").as_bytes();
    match parse(prepared, body) {
        Ok(response) => {
            if response
                .blocks
                .iter()
                .any(|block| matches!(block, Block::ToolUse { .. }))
            {
                return json!({"ok": false, "may_have_run": true,
                    "error": "nested model returned an unexpected tool call"});
            }
            json!({"ok": true, "data": {"text": crate::shape::text_of(&response.blocks),
                "usage": response.usage, "provider": prepared["provider"], "model": prepared["model"]}})
        }
        // An error the provider streams before any output: its status, as for a turn.
        Err(Unread::Refused(status)) => {
            json!({"ok": false, "may_have_run": true, "status": status,
            "error": format!("the provider refused the request (HTTP {status} in its stream)")})
        }
        Err(Unread::Garbled(error)) => json!({"ok": false, "may_have_run": true,
            "error": format!("the provider's reply could not be read ({error}); it may have been billed")}),
    }
}

/// Why a reply body did not read as a response.
enum Unread {
    Refused(u16),
    Garbled(String),
}

/// The body read as a turn's reply is (the provider's stream, parsed by the wire layer).
fn parse(prepared: &Value, body: &[u8]) -> Result<NativeResponse, Unread> {
    let mut reading = Reading::new(prepared).map_err(Unread::Garbled)?;
    reading.push(body).map_err(|error| match error {
        AttemptError::Http(status) => Unread::Refused(status),
        other => Unread::Garbled(format!("{other:?}")),
    })?;
    reading
        .finish()
        .map_err(|error| Unread::Garbled(format!("{error:?}")))
}

/// A nested request's response read as it streams in, as a turn's reply is read (the
/// provider's stream, parsed by the wire layer).
pub(crate) struct Reading {
    waiting: ModelWait<Vec<u8>>,
    wire: WireAttempt,
}

impl Reading {
    /// For the request `prepared` (`code_model_request`'s form), about to be sent.
    pub(crate) fn new(prepared: &Value) -> Result<Self, String> {
        let request = PreparedRequest::new(
            Identity {
                effect: format!("code/{}", prepared["session"].as_str().unwrap_or("")),
                provider: prepared["provider"].as_str().unwrap_or("").into(),
                contract: prepared["contract"].as_str().unwrap_or("").into(),
                endpoint: prepared["endpoint"].as_str().unwrap_or("").into(),
                model: prepared["model"].as_str().unwrap_or("").into(),
                credential_scope: "embedded-app".into(),
                configuration: "spindle-code-model-v1".into(),
            },
            Encoding::Json,
            serde_json::to_vec(&prepared["body"]).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let waiting = ModelWait::new(request)
            .start()
            .map_err(|error| error.to_string())?;
        let wire =
            WireAttempt::new(&waiting, false, false).map_err(|error| format!("{error:?}"))?;
        Ok(Self { waiting, wire })
    }

    /// The next part of the body. An error the provider streams before any output is a
    /// refusal (`Http`).
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<(), AttemptError> {
        self.wire.push(bytes).map(drop)
    }

    /// The body ended: the response it holds.
    pub(crate) fn finish(self) -> Result<NativeResponse, AttemptError> {
        let recorded = self.wire.finish(&self.waiting, &|_| {})?;
        let response = recorded.response().ok_or(AttemptError::Protocol)?;
        NativeResponse::restore(response).map_err(|_| AttemptError::Protocol)
    }
}

/// An app child's result from the app's answer, as native code mode words it.
pub(crate) fn app_result(prepared: &Value, answer: &AppAnswer) -> Value {
    let max_output = prepared["max_output"].as_u64().unwrap_or(65536);
    match answer {
        AppAnswer::Result {
            content,
            failed,
            writes,
            deletes,
            links,
            directories,
            state,
            details,
        } => {
            let mut reply = json!({"ok": !failed, "data": content,
                "_spindle_app": {"max_output": max_output, "details": details}});
            if *failed {
                reply["error"] = reply["data"].clone();
                reply["may_have_run"] = json!(true);
            }
            if !writes.is_empty()
                || !deletes.is_empty()
                || !links.is_empty()
                || !directories.is_empty()
                || state.is_some()
            {
                reply["warning"] = json!(
                    "the app's file and state changes are kept only when the model calls the tool itself"
                );
            }
            reply
        }
        AppAnswer::Unknown(note) => {
            let mut error = "the app could not confirm whether the call ran".to_string();
            if !note.trim().is_empty() {
                error = format!("{error}: {}", note.trim());
            }
            json!({"ok": false, "may_have_run": true, "unknown": true, "error": error})
        }
        _ => json!({"ok": false, "may_have_run": true, "unknown": true,
            "error": "no answer from the app; the call may still have run"}),
    }
}
