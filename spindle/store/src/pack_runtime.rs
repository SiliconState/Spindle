//! Stateful Pack Runtime v1 for code mode, with Dext's request/response shapes, caps
//! and validation (Dext `src/pack_runtime.rs` and `src/ui_bridge.rs`). A pack bound with
//! `"lifecycle":"session"` keeps host-held state between calls; a script never reads or
//! writes it except through the pack's own responses.
//!
//! Every accepted response becomes a ledger anchor: the pack's whole snapshot (state,
//! continuation accounting, queued continuations, promoted tools, latest view) as a
//! store object, and an anchor naming where it happened: a run_code attempt and child
//! (in effect at the revision that attempt's receipt was applied at) or the revision a
//! turn ended at (idle). The state at a revision is the newest anchor on that revision's
//! ancestry, so after a rewind to R the pack sees the snapshot it had at R and abandoned
//! branches drop out. Ledger stages (anchors `pack-ledger-e<epoch>`, snapshots
//! `pack-snap-e<epoch>`, marks `pack-mark-e<epoch>`) are never collected; a store
//! handle reads a scope's anchors and marks once and keeps them current itself.
use crate::artifact::Stage;
use crate::broker::DispatchRequest;
use crate::{Error, Result, Store};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MANIFEST_CAP: usize = 256 * 1024;
const REQUEST_CAP: usize = 256 * 1024;
const RESPONSE_CAP: usize = 256 * 1024;
const STATE_CAP: usize = 64 * 1024;
const CONTENT_CAP: usize = 128 * 1024;
const VIEW_CAP: usize = 128 * 1024;
const EFFECT_LIMIT: usize = 16;
/// UI round trips one invocation chain may take.
pub const UI_ROUND_LIMIT: usize = 16;
const TOOL_LIMIT: usize = 32;
const SCHEMA_CAP: usize = 32 * 1024;
const MAX_SCHEMA_DEPTH: usize = 16;
const MAX_SCHEMA_PROPERTIES: usize = 256;
const MAX_ENUM_VALUES: usize = 256;
const PENDING_CONTINUATION_LIMIT: usize = 32;
/// Longest delay a `continue` effect may ask for.
pub const MAX_DELAY_MS: u64 = 30_000;
const MAX_CONTINUATIONS: u64 = 1_000;
const DEFAULT_MAX_CONTINUATIONS: u64 = 100;
const MAX_TIMEOUT_SECS: u64 = 7 * 24 * 60 * 60;
const UI_PAYLOAD_CAP: usize = 64 * 1024;
const UI_TOKEN_CAP: usize = 64;
const UI_ERROR_MESSAGE_CAP: usize = 2_000;
/// The longest a UI request waits for an answer (Dext's bridge timeout).
pub const UI_REPLY_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Methods the phone UI and a terminal render; Dext defines `form` and `progress`,
/// `confirm`, `select` and `input` are small conventions beside them.
pub const UI_METHODS: [&str; 5] = ["confirm", "form", "input", "progress", "select"];

type Checked<T> = std::result::Result<T, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    Read,
    Write,
    Danger,
}

impl Risk {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "danger" => Some(Self::Danger),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Danger => "danger",
        }
    }
}

/// A tool a manifest declares or a `tools` effect proposes.
#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub risk: Risk,
}

impl Tool {
    /// Dext's `RuntimeTool`: unknown fields are refused and risk defaults to write.
    pub fn parse(value: &Value) -> Checked<Self> {
        let object = value
            .as_object()
            .ok_or("pack runtime tool must be an object")?;
        if let Some(key) = object.keys().find(|key| {
            !matches!(
                key.as_str(),
                "name" | "description" | "input_schema" | "risk"
            )
        }) {
            return Err(format!("pack runtime tool has unknown field {key}"));
        }
        let text = |key: &str| {
            object
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or(format!("pack runtime tool {key} must be a string"))
        };
        Ok(Self {
            name: text("name")?,
            description: text("description")?,
            input_schema: object
                .get("input_schema")
                .cloned()
                .ok_or("pack runtime tool input_schema is required")?,
            risk: match object.get("risk") {
                None => Risk::Write,
                Some(value) => value
                    .as_str()
                    .and_then(Risk::parse)
                    .ok_or("pack runtime tool risk must be read, write or danger")?,
            },
        })
    }
    pub fn to_json(&self) -> Value {
        json!({"name":self.name,"description":self.description,"input_schema":self.input_schema,"risk":self.risk.as_str()})
    }
}

/// The parts of `runtime.json` a session host needs, validated as Dext does.
#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub command: String,
    pub args: Vec<String>,
    pub timeout_seconds: Option<u64>,
    pub tools: Vec<Tool>,
    pub ui_protocol: Option<u64>,
    pub tool_protocol: Option<u64>,
    pub max_continuations: u64,
}

fn optional_u64(object: &serde_json::Map<String, Value>, key: &str) -> Checked<Option<u64>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or(format!("pack runtime {key} must be a nonnegative integer")),
    }
}

/// Parses and validates `runtime.json` bytes (unknown fields refused).
pub fn parse_manifest(bytes: &[u8]) -> Checked<Manifest> {
    if bytes.len() > MANIFEST_CAP {
        return Err(format!(
            "pack runtime manifest exceeds {MANIFEST_CAP} bytes"
        ));
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|e| format!("parsing pack runtime manifest: {e}"))?;
    let object = value
        .as_object()
        .ok_or("pack runtime manifest must be an object")?;
    if let Some(key) = object.keys().find(|key| {
        !matches!(
            key.as_str(),
            "version"
                | "command"
                | "args"
                | "timeout_seconds"
                | "tools"
                | "ui_protocol"
                | "tool_protocol"
                | "max_continuations"
        )
    }) {
        return Err(format!("pack runtime manifest has unknown field {key}"));
    }
    if object.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("unsupported pack runtime protocol version; expected 1".into());
    }
    let command = object
        .get("command")
        .and_then(Value::as_str)
        .ok_or("pack runtime command must be a string")?
        .to_string();
    let args = match object.get("args") {
        None => vec![],
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .ok_or("pack runtime args must be strings")?,
        Some(_) => return Err("pack runtime args must be an array".into()),
    };
    let tools = match object.get("tools") {
        None => vec![],
        Some(Value::Array(items)) => items.iter().map(Tool::parse).collect::<Checked<_>>()?,
        Some(_) => return Err("pack runtime tools must be an array".into()),
    };
    let manifest = Manifest {
        command,
        args,
        timeout_seconds: optional_u64(object, "timeout_seconds")?,
        tools,
        ui_protocol: optional_u64(object, "ui_protocol")?,
        tool_protocol: optional_u64(object, "tool_protocol")?,
        max_continuations: optional_u64(object, "max_continuations")?
            .unwrap_or(DEFAULT_MAX_CONTINUATIONS),
    };
    if manifest.ui_protocol.is_some_and(|version| version != 1) {
        return Err("unsupported pack runtime UI protocol; expected 1".into());
    }
    if manifest.tool_protocol.is_some_and(|version| version != 1) {
        return Err("unsupported pack runtime tool protocol; expected 1".into());
    }
    validate_tools(&manifest.tools, &[])?;
    if manifest.max_continuations > MAX_CONTINUATIONS {
        return Err(format!(
            "pack runtime max_continuations exceeds {MAX_CONTINUATIONS}"
        ));
    }
    if manifest.args.len() > 32
        || manifest
            .args
            .iter()
            .any(|arg| arg.len() > 4_096 || arg.contains('\0'))
    {
        return Err("pack runtime args exceed their count or size limit".into());
    }
    if manifest
        .timeout_seconds
        .is_some_and(|seconds| !(1..=MAX_TIMEOUT_SECS).contains(&seconds))
    {
        return Err(format!(
            "pack runtime timeout_seconds must be between 1 and {MAX_TIMEOUT_SECS}"
        ));
    }
    Ok(manifest)
}

fn unsafe_control(text: &str, multiline: bool) -> bool {
    text.chars()
        .any(|ch| ch.is_control() && !(multiline && matches!(ch, '\n' | '\t')))
}

fn valid_tool_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes[0].is_ascii_alphabetic()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn value_matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn validate_schema(schema: &Value) -> Checked<()> {
    if serde_json::to_vec(schema).map_or(true, |bytes| bytes.len() > SCHEMA_CAP) {
        return Err(format!(
            "pack runtime tool input_schema exceeds {SCHEMA_CAP} bytes"
        ));
    }
    validate_schema_node(schema, "$", 0)?;
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err("pack runtime tool input_schema must declare type=object".into());
    }
    Ok(())
}

fn validate_schema_node(schema: &Value, path: &str, depth: usize) -> Checked<()> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(format!("{path} schema exceeds the nesting limit"));
    }
    let object = schema
        .as_object()
        .ok_or(format!("{path} schema must be an object"))?;
    const SUPPORTED: &[&str] = &[
        "type",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
        "description",
    ];
    if let Some(keyword) = object.keys().find(|key| !SUPPORTED.contains(&key.as_str())) {
        return Err(format!("{path} schema uses unsupported keyword {keyword}"));
    }
    let kind = match object.get("type") {
        Some(Value::String(kind))
            if matches!(
                kind.as_str(),
                "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
            ) =>
        {
            kind.as_str()
        }
        Some(Value::String(kind)) => {
            return Err(format!("{path} schema has unsupported type {kind}"));
        }
        Some(_) => return Err(format!("{path} schema type must be a string")),
        None => return Err(format!("{path} schema must declare a supported type")),
    };
    if let Some(description) = object.get("description") {
        let description = description
            .as_str()
            .ok_or(format!("{path} schema description must be a string"))?;
        if description.len() > 2_000 || unsafe_control(description, true) {
            return Err(format!(
                "{path} schema description must be at most 2000 bytes without control characters"
            ));
        }
    }
    if let Some(values) = object.get("enum") {
        let values = values
            .as_array()
            .ok_or(format!("{path} schema enum must be an array"))?;
        if values.is_empty() || values.len() > MAX_ENUM_VALUES {
            return Err(format!(
                "{path} schema enum must contain 1-{MAX_ENUM_VALUES} values"
            ));
        }
        if let Some(value) = values.iter().find(|value| !value_matches_type(value, kind)) {
            return Err(format!(
                "{path} schema enum value {value} does not match type {kind}"
            ));
        }
    }
    let properties = match object.get("properties") {
        None => None,
        Some(Value::Object(properties)) => Some(properties),
        Some(_) => return Err(format!("{path} schema properties must be an object")),
    };
    if (properties.is_some()
        || object.contains_key("required")
        || object.contains_key("additionalProperties"))
        && kind != "object"
    {
        return Err(format!("{path} schema object keywords require type=object"));
    }
    if let Some(properties) = properties {
        if properties.len() > MAX_SCHEMA_PROPERTIES {
            return Err(format!(
                "{path} schema declares more than {MAX_SCHEMA_PROPERTIES} properties"
            ));
        }
        for (name, child) in properties {
            if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
                return Err(format!("{path} schema contains an invalid property name"));
            }
            validate_schema_node(child, &format!("{path}.{name}"), depth + 1)?;
        }
    }
    if let Some(required) = object.get("required") {
        let required = required
            .as_array()
            .ok_or(format!("{path} schema required must be an array"))?;
        if required.len() > MAX_SCHEMA_PROPERTIES {
            return Err(format!("{path} schema required list is too large"));
        }
        let mut names = std::collections::BTreeSet::new();
        for name in required {
            let name = name
                .as_str()
                .ok_or(format!("{path} schema required entries must be strings"))?;
            if !names.insert(name) {
                return Err(format!("{path} schema required contains duplicate {name}"));
            }
            if properties.is_none_or(|properties| !properties.contains_key(name)) {
                return Err(format!(
                    "{path} schema required entry {name} has no property schema"
                ));
            }
        }
    }
    if let Some(additional) = object.get("additionalProperties")
        && !additional.is_boolean()
    {
        return Err(format!(
            "{path} schema additionalProperties must be a boolean"
        ));
    }
    if let Some(items) = object.get("items") {
        if kind != "array" {
            return Err(format!("{path} schema items requires type=array"));
        }
        validate_schema_node(items, &format!("{path}[]"), depth + 1)?;
    } else if kind == "array" {
        return Err(format!("{path} array schema must declare items"));
    }
    Ok(())
}

/// Dext's tool catalog rules: provider-safe names, no collisions with `occupied`,
/// bounded descriptions and the supported schema subset.
pub fn validate_tools(tools: &[Tool], occupied: &[String]) -> Checked<()> {
    if tools.len() > TOOL_LIMIT {
        return Err(format!(
            "pack runtime declares more than {TOOL_LIMIT} tools"
        ));
    }
    let catalog: Vec<Value> = tools.iter().map(Tool::to_json).collect();
    if serde_json::to_vec(&catalog).map_or(true, |bytes| bytes.len() > STATE_CAP) {
        return Err(format!(
            "pack runtime tool catalog exceeds {STATE_CAP} bytes"
        ));
    }
    let mut names = std::collections::BTreeSet::new();
    for tool in tools {
        if !valid_tool_name(&tool.name) {
            return Err(format!(
                "pack runtime tool names must be 1-64 ASCII letters, digits, or underscores and start with a letter: {}",
                tool.name
            ));
        }
        if occupied.contains(&tool.name) || !names.insert(tool.name.clone()) {
            return Err(format!(
                "pack runtime tool name collides with an exposed tool: {}",
                tool.name
            ));
        }
        if tool.description.trim().is_empty()
            || tool.description.len() > 2_000
            || unsafe_control(&tool.description, true)
        {
            return Err(format!(
                "pack runtime tool description must be 1-2000 bytes without control characters: {}",
                tool.name
            ));
        }
        validate_schema(&tool.input_schema)
            .map_err(|error| format!("validating pack runtime tool {}: {error}", tool.name))?;
    }
    Ok(())
}

/// Dext's input check against a promoted tool's schema.
pub fn validate_tool_input(tool: &Tool, input: &Value) -> Checked<()> {
    validate_schema_value(&tool.input_schema, input, "$", 0)
        .map_err(|error| format!("invalid tool args for {}: {error}", tool.name))
}

fn validate_schema_value(schema: &Value, value: &Value, path: &str, depth: usize) -> Checked<()> {
    if depth > 16 {
        return Err(format!("{path} exceeds the schema nesting limit"));
    }
    let schema = schema
        .as_object()
        .ok_or(format!("{path} schema must be an object"))?;
    if let Some(kind) = schema.get("type").and_then(Value::as_str)
        && !value_matches_type(value, kind)
    {
        return Err(format!("{path} must be {kind}"));
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && !values.contains(value)
    {
        return Err(format!("{path} is not one of the allowed values"));
    }
    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(name) {
                    return Err(format!("{path}.{name} is required"));
                }
            }
        }
        if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false) {
            for name in object.keys() {
                if properties.is_none_or(|properties| !properties.contains_key(name)) {
                    return Err(format!("{path}.{name} is not allowed"));
                }
            }
        }
        if let Some(properties) = properties {
            for (name, child_schema) in properties {
                if let Some(child) = object.get(name) {
                    validate_schema_value(
                        child_schema,
                        child,
                        &format!("{path}.{name}"),
                        depth + 1,
                    )?;
                }
            }
        }
    }
    if let Some(array) = value.as_array()
        && let Some(items) = schema.get("items")
    {
        for (index, child) in array.iter().enumerate() {
            validate_schema_value(items, child, &format!("{path}[{index}]"), depth + 1)?;
        }
    }
    Ok(())
}

fn validate_state(state: &Value) -> Checked<()> {
    if serde_json::to_vec(state).map_or(true, |bytes| bytes.len() > STATE_CAP) {
        return Err(format!("pack runtime state exceeds {STATE_CAP} bytes"));
    }
    Ok(())
}

fn validate_pending(prompts: &[(String, u64)]) -> Checked<()> {
    if prompts.len() > PENDING_CONTINUATION_LIMIT {
        return Err(format!(
            "pack runtime pending continuation count exceeds {PENDING_CONTINUATION_LIMIT}"
        ));
    }
    let mut bytes = 0usize;
    for (prompt, delay_ms) in prompts {
        if prompt.trim().is_empty()
            || prompt.len() > CONTENT_CAP
            || *delay_ms > MAX_DELAY_MS
            || unsafe_control(prompt, true)
        {
            return Err("pack runtime pending continuation exceeds its size or delay limit".into());
        }
        bytes += prompt.len();
    }
    if bytes > STATE_CAP {
        return Err(format!(
            "pack runtime pending continuations exceed {STATE_CAP} bytes"
        ));
    }
    Ok(())
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= UI_TOKEN_CAP
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn value_contains_unsafe_control(value: &Value) -> bool {
    match value {
        Value::String(text) => unsafe_control(text, true),
        Value::Array(items) => items.iter().any(value_contains_unsafe_control),
        Value::Object(fields) => fields.iter().any(|(key, value)| {
            key.chars().any(char::is_control) || value_contains_unsafe_control(value)
        }),
        _ => false,
    }
}

fn validate_ui_value(value: &Value, label: &str) -> Checked<()> {
    if serde_json::to_vec(value).map_or(true, |bytes| bytes.len() > UI_PAYLOAD_CAP) {
        return Err(format!("{label} exceeds {UI_PAYLOAD_CAP} bytes"));
    }
    if value_contains_unsafe_control(value) {
        return Err(format!(
            "{label} contains unsafe terminal control characters"
        ));
    }
    Ok(())
}

/// One host UI request a pack asked for (Dext `UiRequest`).
#[derive(Clone, Debug, PartialEq)]
pub struct UiRequest {
    pub id: String,
    pub method: String,
    pub params: Value,
}

impl UiRequest {
    fn parse(value: &Value) -> Checked<Self> {
        let object = value.as_object().ok_or("ui_request must be an object")?;
        if let Some(key) = object
            .keys()
            .find(|key| !matches!(key.as_str(), "id" | "method" | "params"))
        {
            return Err(format!("ui_request has unknown field {key}"));
        }
        let request = Self {
            id: object
                .get("id")
                .and_then(Value::as_str)
                .ok_or("ui_request id must be a string")?
                .into(),
            method: object
                .get("method")
                .and_then(Value::as_str)
                .ok_or("ui_request method must be a string")?
                .into(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        };
        request.validate()?;
        Ok(request)
    }
    pub fn validate(&self) -> Checked<()> {
        if !valid_token(&self.id) {
            return Err(
                "pack runtime UI request id must be 1-64 ASCII letters, digits, '.', '_', or '-'"
                    .into(),
            );
        }
        if !valid_token(&self.method) {
            return Err(
                "pack runtime UI request method must be 1-64 ASCII letters, digits, '.', '_', or '-'"
                    .into(),
            );
        }
        validate_ui_value(&self.params, "pack runtime UI params")
    }
    pub fn to_json(&self) -> Value {
        json!({"id":self.id,"method":self.method,"params":self.params})
    }
}

/// A host's answer to a UI request (Dext `UiResponse`).
#[derive(Clone, Debug, PartialEq)]
pub enum UiResponse {
    Ok(Value),
    Cancelled,
    Error { code: String, message: String },
}

impl UiResponse {
    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Self::Error {
            code: code.into(),
            message: message.into(),
        }
    }
    /// An answer as a frontend sends it: `{"status":"ok","value":..}`, `{"status":
    /// "cancelled"}` or `{"status":"error","code":..,"message":..}` (Dext ui.response).
    pub fn parse(frame: &Value) -> Checked<Self> {
        let response = match frame["status"].as_str() {
            Some("ok") => Self::Ok(frame.get("value").cloned().unwrap_or(Value::Null)),
            Some("cancelled") => Self::Cancelled,
            Some("error") => {
                let field = |key: &str| {
                    frame[key]
                        .as_str()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                };
                Self::Error {
                    code: field("code").ok_or("ui.response error: needs code and message")?,
                    message: field("message").ok_or("ui.response error: needs code and message")?,
                }
            }
            _ => return Err("ui.response: needs status ok|cancelled|error".into()),
        };
        response.validate()?;
        Ok(response)
    }
    pub fn validate(&self) -> Checked<()> {
        match self {
            Self::Ok(value) => validate_ui_value(value, "pack UI response value"),
            Self::Cancelled => Ok(()),
            Self::Error { code, message } => {
                if !valid_token(code) {
                    return Err("pack UI response error code is invalid".into());
                }
                if message.trim().is_empty()
                    || message.len() > UI_ERROR_MESSAGE_CAP
                    || unsafe_control(message, true)
                {
                    return Err("pack UI response error message exceeds its limit".into());
                }
                Ok(())
            }
        }
    }
    pub fn to_json(&self) -> Value {
        match self {
            Self::Ok(value) => json!({"status":"ok","value":value}),
            Self::Cancelled => json!({"status":"cancelled"}),
            Self::Error { code, message } => {
                json!({"status":"error","code":code,"message":message})
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Steer(String),
    Continue(String, u64),
    View(String, String),
    Tools(Vec<Tool>),
}

/// A validated helper response (Dext `RuntimeResponse`).
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub content: String,
    pub is_error: bool,
    pub state: Option<Value>,
    pub effects: Vec<Effect>,
    pub ui_request: Option<UiRequest>,
}

fn parse_effect(value: &Value) -> Checked<Effect> {
    let text = |key: &str| {
        value[key]
            .as_str()
            .map(str::to_string)
            .ok_or(format!("pack runtime effect {key} must be a string"))
    };
    Ok(match value["type"].as_str() {
        Some("steer") => Effect::Steer(text("text")?),
        Some("continue") => Effect::Continue(
            text("prompt")?,
            match value.get("delay_ms") {
                None => 0,
                Some(delay) => delay
                    .as_u64()
                    .ok_or("pack runtime continue delay_ms must be a nonnegative integer")?,
            },
        ),
        Some("view") => Effect::View(text("title")?, text("markdown")?),
        Some("tools") => Effect::Tools(
            value["tools"]
                .as_array()
                .ok_or("pack runtime tools effect needs a tools array")?
                .iter()
                .map(Tool::parse)
                .collect::<Checked<_>>()?,
        ),
        _ => return Err("unknown pack runtime effect type".into()),
    })
}

/// Parses one helper response with Dext's field rules and caps.
pub fn parse_response(value: &Value) -> Checked<Response> {
    let object = value
        .as_object()
        .ok_or("pack runtime stdout must contain one JSON response object")?;
    if let Some(key) = object.keys().find(|key| {
        !matches!(
            key.as_str(),
            "version" | "content" | "is_error" | "state" | "effects" | "ui_request"
        )
    }) {
        return Err(format!("pack runtime response has unknown field {key}"));
    }
    if object.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("pack runtime response version does not match protocol 1".into());
    }
    let content = match object.get("content") {
        None => String::new(),
        Some(value) => value
            .as_str()
            .ok_or("pack runtime content must be a string")?
            .to_string(),
    };
    let is_error = match object.get("is_error") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or("pack runtime is_error must be a boolean")?,
    };
    let effects = match object.get("effects") {
        None => vec![],
        Some(Value::Array(items)) => {
            if items.len() > EFFECT_LIMIT {
                return Err(format!(
                    "pack runtime returned more than {EFFECT_LIMIT} effects"
                ));
            }
            items.iter().map(parse_effect).collect::<Checked<_>>()?
        }
        Some(_) => return Err("pack runtime effects must be an array".into()),
    };
    let ui_request = match object.get("ui_request") {
        None | Some(Value::Null) => None,
        Some(value) => Some(UiRequest::parse(value)?),
    };
    let response = Response {
        content,
        is_error,
        state: object
            .get("state")
            .filter(|state| !state.is_null())
            .cloned(),
        effects,
        ui_request,
    };
    if response.content.len() > CONTENT_CAP {
        return Err(format!("pack runtime content exceeds {CONTENT_CAP} bytes"));
    }
    if unsafe_control(&response.content, true) {
        return Err("pack runtime content contains unsafe terminal control characters".into());
    }
    if let Some(state) = &response.state {
        validate_state(state)?;
    }
    for effect in &response.effects {
        match effect {
            Effect::Steer(text)
                if text.trim().is_empty()
                    || text.len() > CONTENT_CAP
                    || unsafe_control(text, true) =>
            {
                return Err("pack runtime steer effect exceeds its size limit".into());
            }
            Effect::Continue(prompt, delay_ms)
                if prompt.trim().is_empty()
                    || prompt.len() > CONTENT_CAP
                    || *delay_ms > MAX_DELAY_MS
                    || unsafe_control(prompt, true) =>
            {
                return Err("pack runtime continue effect exceeds its size or delay limit".into());
            }
            Effect::View(title, markdown)
                if title.trim().is_empty()
                    || title.len() > 256
                    || markdown.len() > VIEW_CAP
                    || unsafe_control(title, false)
                    || unsafe_control(markdown, true) =>
            {
                return Err("pack runtime view effect exceeds its size limit".into());
            }
            Effect::Tools(tools) => validate_tools(tools, &[])?,
            _ => {}
        }
    }
    Ok(response)
}

/// What the operator's policy binds for one session pack.
#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    pub pack: String,
    /// The bound executable path (Dext's pack source identity).
    pub source: String,
    pub manifest_sha256: String,
    pub executable_sha256: String,
    pub manifest: Manifest,
    /// Manifest tools the policy grants (bare names).
    pub granted: Vec<String>,
    /// The highest risk a promoted tool may declare.
    pub max_dynamic_risk: Risk,
}

/// A pack's host-held session state (Dext `RuntimeSnapshot` plus its promoted tools,
/// which the task asks to persist, and the latest view for frontends).
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub pack: String,
    pub source: String,
    pub manifest_sha256: String,
    pub executable_sha256: String,
    pub state: Value,
    pub continuations_used: u64,
    pub pending: Vec<(String, u64)>,
    pub tools: Vec<Tool>,
    pub view: Option<(String, String)>,
}

impl Snapshot {
    fn fresh(binding: &Binding) -> Self {
        Self {
            pack: binding.pack.clone(),
            source: binding.source.clone(),
            manifest_sha256: binding.manifest_sha256.clone(),
            executable_sha256: binding.executable_sha256.clone(),
            state: Value::Null,
            continuations_used: 0,
            pending: vec![],
            tools: vec![],
            view: None,
        }
    }
    fn to_json(&self) -> Value {
        json!({
            "version":1,
            "pack":self.pack,
            "source":self.source,
            "manifest_sha256":self.manifest_sha256,
            "executable_sha256":self.executable_sha256,
            "state":self.state,
            "continuations_used":self.continuations_used,
            "pending_continuations":self.pending.iter().map(|(p, d)| json!([p, d])).collect::<Vec<_>>(),
            "tools":self.tools.iter().map(Tool::to_json).collect::<Vec<_>>(),
            "view":self.view.as_ref().map(|(title, markdown)| json!({"title":title,"markdown":markdown})),
        })
    }
    fn parse(value: &Value) -> Checked<Self> {
        let text = |key: &str| {
            value[key]
                .as_str()
                .map(str::to_string)
                .ok_or(format!("pack snapshot {key} missing"))
        };
        Ok(Self {
            pack: text("pack")?,
            source: text("source")?,
            manifest_sha256: text("manifest_sha256")?,
            executable_sha256: text("executable_sha256")?,
            state: value["state"].clone(),
            continuations_used: value["continuations_used"].as_u64().unwrap_or(0),
            pending: value["pending_continuations"]
                .as_array()
                .ok_or("pack snapshot continuations missing")?
                .iter()
                .map(|pair| Some((pair[0].as_str()?.to_string(), pair[1].as_u64()?)))
                .collect::<Option<_>>()
                .ok_or("pack snapshot continuation malformed")?,
            tools: value["tools"]
                .as_array()
                .ok_or("pack snapshot tools missing")?
                .iter()
                .map(Tool::parse)
                .collect::<Checked<_>>()?,
            view: value["view"].as_object().map(|view| {
                (
                    view["title"].as_str().unwrap_or_default().to_string(),
                    view["markdown"].as_str().unwrap_or_default().to_string(),
                )
            }),
        })
    }
    /// Dext's restore rule: a changed identity, manifest or executable is refused, and
    /// the saved accounting must fit the manifest's declared budget.
    pub fn restore(&self, binding: &Binding) -> Checked<()> {
        if self.pack != binding.pack
            || self.source != binding.source
            || self.manifest_sha256 != binding.manifest_sha256
            || self.executable_sha256 != binding.executable_sha256
        {
            return Err(format!(
                "pack {} changed its identity, manifest or executable since its session state was saved; its state is not restored. Rewind to before its first use or start a new thread",
                binding.pack
            ));
        }
        validate_state(&self.state)?;
        validate_pending(&self.pending)?;
        if self.continuations_used > binding.manifest.max_continuations
            || self.pending.len() as u64 > self.continuations_used
        {
            return Err("pack runtime continuation snapshot exceeds its declared budget".into());
        }
        Ok(())
    }
    /// A tool the session can call: a granted manifest tool, or a promoted one the
    /// binding's ceiling allows now (the operator may have lowered it, or regranted the
    /// pack lower, since the tool was promoted).
    pub fn tool<'a>(&'a self, binding: &'a Binding, name: &str) -> Option<&'a Tool> {
        binding
            .manifest
            .tools
            .iter()
            .filter(|tool| binding.granted.contains(&tool.name))
            .chain(
                self.tools
                    .iter()
                    .filter(|tool| tool.risk <= binding.max_dynamic_risk),
            )
            .find(|tool| tool.name == name)
    }
}

/// The outcome of spawning a helper once.
pub enum Invoked {
    /// The helper ran and printed this JSON.
    Output(Value),
    /// Nothing was started.
    NotStarted(String),
    /// It ran and failed in a known way (exit status, bad output).
    Failed(String),
    /// It was stopped (timeout, interrupt, flood): its effects are unknown.
    Unknown(String),
}

/// Shows UI requests to a person and returns their answers.
pub trait PackUi: Send + Sync {
    /// Method names this frontend renders (`context.ui_methods`).
    fn methods(&self) -> Vec<String>;
    /// Shows `request` and waits for an answer until `deadline` or `interrupt`.
    fn ask(
        &self,
        pack: &str,
        request: &UiRequest,
        interrupt: &AtomicBool,
        deadline: Instant,
    ) -> UiResponse;
}

/// The operator-policy side of session packs (implemented by `CodeHost`).
pub trait PackHost {
    /// The policy binding of a session pack, its pins verified.
    fn binding(&mut self, pack: &str) -> Checked<Binding>;
    /// Whether the policy binds any session pack (otherwise the ledger is not read).
    fn has_session_packs(&self) -> bool;
    /// Active packs at this point, so promoted tools are discoverable and callable.
    fn refresh(&mut self, packs: &BTreeMap<String, Snapshot>);
    /// Spawns the bound helper once with `request` on stdin.
    fn invoke(
        &mut self,
        pack: &str,
        request: &Value,
        interrupt: &AtomicBool,
        remaining: Duration,
    ) -> Invoked;
    fn ui(&self) -> Option<Arc<dyn PackUi>>;
    /// A view effect, for observers.
    fn view(&self, pack: &str, title: &str, markdown: &str);
    /// The session's stable identity (`session_id`, as Dext's: one per session).
    fn session(&self) -> String;
    /// The running turn's identity (`context.turn_id`): one per turn, shared by its
    /// tool calls and its idle.
    fn turn(&self) -> String;
    fn cwd(&self) -> String;
}

/// Ledger records sit in stages per owner epoch, one kind each: anchors and marks are
/// read without touching the snapshot objects (whole states and views) they name.
const ANCHORS: &str = "pack-ledger-e";
const SNAPSHOTS: &str = "pack-snap-e";
const MARKS: &str = "pack-mark-e";
const ANCHOR: &[u8] = b"spindle-pack-anchor-v1\n";
const SNAPSHOT: &[u8] = b"spindle-pack-snapshot-v1\n";
const MARK: &[u8] = b"spindle-pack-mark-v1\n";

fn tagged(tag: &[u8], value: &Value) -> Result<Vec<u8>> {
    let mut bytes = tag.to_vec();
    bytes.extend(serde_json::to_vec(value).map_err(|_| Error::Invalid)?);
    Ok(bytes)
}

/// The `tag` records `scope` pinned in its stages named `kind<epoch>`, with their
/// digests (a range over the stage key; no object outside those stages is read).
fn records(db: &Connection, scope: &str, kind: &str, tag: &[u8]) -> Result<Vec<(String, Value)>> {
    let upper = format!("{}f", &kind[..kind.len() - 1]);
    let mut query = db.prepare_cached(
        "SELECT o.digest,o.bytes,o.size FROM artifact_stages s JOIN staging_pins p ON p.stage=s.id JOIN objects o ON o.digest=p.digest WHERE s.id>=?1 AND s.id<?2 AND s.scope=?3",
    )?;
    let rows = query.query_map(params![kind, upper, scope], |row| {
        Ok((
            crate::bounded_label(row, 0)?,
            crate::artifact::checked_at(row, 0, 1)?,
        ))
    })?;
    let mut out = vec![];
    for row in rows {
        let (digest, bytes) = row?;
        if let Some(body) = bytes.strip_prefix(tag) {
            out.push((
                digest,
                serde_json::from_slice(body).map_err(|_| Error::Incompatible)?,
            ));
        }
    }
    Ok(out)
}

/// Whether `revision` is a finished rewind's own: its completed state is a fork of a
/// turn that ended where it came from (with the turn end run there), so it has no turn
/// end of its own to run.
fn rewound_to(db: &Connection, scope: &str, revision: u64) -> Result<bool> {
    Ok(db
        .prepare_cached("SELECT 1 FROM rewinds WHERE scope=?1 AND revision=?2")?
        .query_row(params![scope, revision as i64], |_| Ok(()))
        .optional()?
        .is_some())
}

fn head_revision(db: &Connection) -> Result<u64> {
    Ok(db
        .prepare_cached("SELECT revision FROM head WHERE id=1")?
        .query_row([], |row| crate::nonnegative_id(row, 0))?)
}

/// Revisions on the head's history as inclusive ranges: a finished rewind's revision
/// continues from its target, every other revision from the one before it.
fn ancestry(db: &Connection, scope: &str) -> Result<(u64, Vec<(u64, u64)>)> {
    let head = head_revision(db)?;
    let mut rewound = BTreeMap::new();
    let mut query = db.prepare_cached(
        "SELECT revision,target FROM rewinds WHERE scope=?1 AND revision IS NOT NULL",
    )?;
    for row in query.query_map([scope], |row| {
        Ok((
            crate::nonnegative_id(row, 0)?,
            crate::nonnegative_id(row, 1)?,
        ))
    })? {
        let (revision, target) = row?;
        rewound.insert(revision, target);
    }
    let mut ranges = vec![];
    let mut top = head;
    while top > 0 {
        match rewound.range(..=top).next_back() {
            Some((&revision, &target)) if target < revision => {
                ranges.push((revision, top));
                top = target;
            }
            _ => {
                ranges.push((1, top));
                break;
            }
        }
    }
    Ok((head, ranges))
}

#[derive(Clone, Debug)]
struct Anchor {
    attempt: Option<String>,
    revision: Option<u64>,
    child: u64,
    step: u64,
    pack: String,
    event: String,
    snapshot: String,
    steer: Vec<String>,
}

impl Anchor {
    fn parse(value: &Value) -> Option<Self> {
        Some(Self {
            attempt: value["attempt"].as_str().map(str::to_string),
            revision: value["revision"].as_u64(),
            child: value["child"].as_u64()?,
            step: value["step"].as_u64()?,
            pack: value["pack"].as_str()?.to_string(),
            event: value["event"].as_str()?.to_string(),
            snapshot: value["snapshot"].as_str()?.to_string(),
            steer: value["steer"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

/// A store handle's copy of the pack ledger. The owner lock makes that handle the only
/// writer, so each scope's anchors and marks are read once and then kept current by its
/// own writes: a call reads no ledger record again, however long the ledger grows.
#[derive(Default)]
pub(crate) struct Ledger(BTreeMap<String, Scoped>);

#[derive(Default)]
struct Scoped {
    anchors: Vec<Anchor>,
    marks: Vec<Value>,
    seen: std::collections::BTreeSet<String>,
    by_attempt: BTreeMap<String, Vec<usize>>,
    by_revision: BTreeMap<u64, Vec<usize>>,
    /// Where run_code receipts were applied (fixed once recorded).
    applied: BTreeMap<String, u64>,
    /// Each pack's newest snapshot on the history of this head revision.
    base: Option<(u64, BTreeMap<String, String>)>,
    /// Ledger records read from the database.
    reads: usize,
}

impl Scoped {
    fn load(db: &Connection, scope: &str) -> Result<Self> {
        let mut scoped = Self::default();
        for (digest, value) in records(db, scope, ANCHORS, ANCHOR)? {
            scoped.reads += 1;
            if let Some(anchor) = Anchor::parse(&value) {
                scoped.add_anchor(digest, anchor);
            }
        }
        for (digest, value) in records(db, scope, MARKS, MARK)? {
            scoped.reads += 1;
            scoped.add_mark(digest, value);
        }
        Ok(scoped)
    }

    fn add_anchor(&mut self, digest: String, anchor: Anchor) {
        if !self.seen.insert(digest) {
            return;
        }
        let index = self.anchors.len();
        match (&anchor.attempt, anchor.revision) {
            // A script's anchors are written before its receipt is applied, which moves
            // the head: they never change the cached base of the head they ran at.
            (Some(attempt), _) => self.by_attempt.entry(attempt.clone()).or_default(),
            (None, Some(revision)) => {
                self.base = None;
                self.by_revision.entry(revision).or_default()
            }
            (None, None) => return,
        }
        .push(index);
        self.anchors.push(anchor);
    }

    /// The step after the last turn-end or idle anchor at `revision`.
    fn next_step(&self, revision: u64) -> u64 {
        self.by_revision
            .get(&revision)
            .into_iter()
            .flatten()
            .map(|index| self.anchors[*index].step + 1)
            .max()
            .unwrap_or(0)
    }

    fn add_mark(&mut self, digest: String, mark: Value) {
        if self.seen.insert(digest) {
            self.marks.push(mark);
        }
    }

    fn applied(&mut self, db: &Connection, attempt: &str) -> Result<Option<u64>> {
        if let Some(revision) = self.applied.get(attempt) {
            return Ok(Some(*revision));
        }
        let revision = db
            .prepare_cached("SELECT revision FROM applied_receipts WHERE attempt=?1")?
            .query_row([attempt], |row| crate::nonnegative_id(row, 0))
            .optional()?;
        if let Some(revision) = revision {
            self.applied.insert(attempt.into(), revision);
        }
        Ok(revision)
    }

    /// Anchors in effect on the head's history (plus those of the running attempt
    /// `current`, which come last), in order.
    fn effective(
        &mut self,
        db: &Connection,
        scope: &str,
        current: Option<&str>,
    ) -> Result<Vec<usize>> {
        if self.anchors.is_empty() {
            return Ok(vec![]);
        }
        let (_, ranges) = ancestry(db, scope)?;
        let on = |revision: u64| {
            ranges
                .iter()
                .any(|(low, high)| (*low..=*high).contains(&revision))
        };
        let attempts: Vec<String> = self.by_attempt.keys().cloned().collect();
        let mut placed = BTreeMap::new();
        for attempt in attempts {
            if Some(attempt.as_str()) == current {
                placed.insert(attempt, u64::MAX);
            } else if let Some(revision) = self.applied(db, &attempt)?.filter(|r| on(*r)) {
                placed.insert(attempt, revision);
            }
        }
        let mut keyed = vec![];
        for (index, anchor) in self.anchors.iter().enumerate() {
            let position = match (&anchor.attempt, anchor.revision) {
                (Some(attempt), _) => placed.get(attempt).copied(),
                (None, Some(revision)) => on(revision).then_some(revision),
                (None, None) => None,
            };
            if let Some(position) = position {
                keyed.push(((position, anchor.child, anchor.step), index));
            }
        }
        keyed.sort();
        Ok(keyed.into_iter().map(|(_, index)| index).collect())
    }

    /// Each active pack's newest snapshot digest at the head; `current` adds that
    /// running attempt's own anchors. The head's part is kept until the head moves or a
    /// turn-end anchor lands, so a script's calls read nothing but the head revision.
    fn latest(
        &mut self,
        db: &Connection,
        scope: &str,
        current: Option<&str>,
    ) -> Result<BTreeMap<String, String>> {
        let fold = |anchors: &[Anchor], order: Vec<usize>| {
            order
                .into_iter()
                .map(|index| (anchors[index].pack.clone(), anchors[index].snapshot.clone()))
                .collect::<BTreeMap<_, _>>()
        };
        if self.anchors.is_empty() {
            return Ok(BTreeMap::new());
        }
        // An attempt whose receipt is applied already (a replay) is placed in full.
        if let Some(attempt) = current
            && self.applied(db, attempt)?.is_some()
        {
            let order = self.effective(db, scope, current)?;
            return Ok(fold(&self.anchors, order));
        }
        let head = head_revision(db)?;
        if self.base.as_ref().is_none_or(|(at, _)| *at != head) {
            let order = self.effective(db, scope, None)?;
            self.base = Some((head, fold(&self.anchors, order)));
        }
        let mut latest = self
            .base
            .as_ref()
            .map(|(_, base)| base.clone())
            .unwrap_or_default();
        if let Some(own) = current.and_then(|attempt| self.by_attempt.get(attempt)) {
            let mut own = own.clone();
            own.sort_by_key(|index| (self.anchors[*index].child, self.anchors[*index].step));
            for index in own {
                let anchor = &self.anchors[index];
                latest.insert(anchor.pack.clone(), anchor.snapshot.clone());
            }
        }
        Ok(latest)
    }
}

fn load_snapshot(db: &Connection, digest: &str) -> Result<Snapshot> {
    let bytes: Vec<u8> = db
        .prepare_cached(crate::artifact::STORED)?
        .query_row([digest], |row| crate::artifact::object_at(row, 0))?;
    if crate::artifact::object_digest(&bytes) != digest || !bytes.starts_with(SNAPSHOT) {
        return Err(Error::Incompatible);
    }
    let value: Value =
        serde_json::from_slice(&bytes[SNAPSHOT.len()..]).map_err(|_| Error::Incompatible)?;
    Snapshot::parse(&value).map_err(|_| Error::Incompatible)
}

fn load_snapshots(
    db: &Connection,
    latest: BTreeMap<String, String>,
) -> Result<BTreeMap<String, Snapshot>> {
    latest
        .into_iter()
        .map(|(pack, digest)| Ok((pack, load_snapshot(db, &digest)?)))
        .collect()
}

/// Active packs at the head with their promoted tools and latest view, for frontends.
pub(crate) fn overview(db: &Connection, scope: &str) -> Result<Value> {
    let latest = Scoped::load(db, scope)?.latest(db, scope, None)?;
    Ok(Value::Array(
        load_snapshots(db, latest)?
            .into_values()
            .map(|snapshot| {
                json!({
                    "pack":snapshot.pack,
                    "tools":snapshot.tools.iter().map(|tool| format!("{}/{}", snapshot.pack, tool.name)).collect::<Vec<_>>(),
                    "continuations_used":snapshot.continuations_used,
                    "view":snapshot.view.map(|(title, markdown)| json!({"title":title,"markdown":markdown})),
                })
            })
            .collect(),
    ))
}

/// Whether `resume` has pack work to do after the completed turn `execution` at the
/// head: a decided follow-up not yet delivered or cancelled, or (the host stopped
/// before the turn's end was recorded) idle for the packs active there.
pub(crate) fn follow_up_pending(db: &Connection, scope: &str, execution: &str) -> Result<bool> {
    let mut scoped = Scoped::load(db, scope)?;
    let marks: Vec<&Value> = scoped
        .marks
        .iter()
        .filter(|mark| mark["execution"] == execution)
        .collect();
    if let Some(end) = marks.iter().find(|mark| mark["kind"] == "turn-end") {
        return Ok(end["parts"]
            .as_array()
            .is_some_and(|parts| !parts.is_empty())
            && !marks.iter().any(|mark| mark["kind"] == "cancelled"));
    }
    Ok(!rewound_to(db, scope, head_revision(db)?)? && !scoped.latest(db, scope, None)?.is_empty())
}

/// What a follow-up turn after a turn's end says, and how long to wait first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FollowUp {
    pub prompt: String,
    pub delay_ms: u64,
}

/// What ending a turn did for its active packs.
#[derive(Clone, Debug, Default)]
pub struct TurnEnd {
    pub follow_up: Option<FollowUp>,
    /// Things worth telling the operator (an idle that failed or was not re-run).
    pub notes: Vec<String>,
}

/// A failed step of a chain: whether the helper may have run, and whether its
/// outcome is unknown (blocks further external effects).
struct Failure {
    message: String,
    ran: bool,
    unknown: bool,
}

/// A helper error that says its effect was never dispatched (the MCP bridge's
/// `not_sent:` prefix, e.g. a locked keyring before connecting).
pub(crate) fn not_sent(content: &str) -> bool {
    content.trim_start().starts_with("not_sent")
}

impl Failure {
    fn known(message: String) -> Self {
        Self {
            message,
            ran: true,
            unknown: false,
        }
    }
    fn to_json(&self) -> Value {
        let mut value = json!({"ok":false,"may_have_run":self.ran,"error":self.message});
        if self.unknown {
            value["unknown"] = json!(true);
        }
        value
    }
}

enum Event<'a> {
    Activate,
    Tool(&'a str, &'a Value),
    Idle,
    UiResponse(&'a UiRequest, &'a UiResponse),
}

/// Continuation rule for idle chains (Dext counts idle content as a continuation).
#[derive(Clone, Copy)]
enum Implicit {
    No,
    Idle { explicit: bool, content: bool },
}

#[derive(Debug)]
struct Applied {
    content: String,
    ui_request: Option<UiRequest>,
    steer: Vec<String>,
    continued: bool,
}

/// One pack's invocations for a tool call or a turn's end, each anchored as it is
/// accepted.
struct Chain {
    scope: String,
    attempt: Option<String>,
    revision: Option<u64>,
    child: u64,
    step: u64,
    binding: Binding,
    snapshot: Snapshot,
    turn_id: String,
    iteration: u64,
}

impl Chain {
    #[allow(clippy::too_many_arguments)]
    fn step(
        &mut self,
        store: &mut Store,
        host: &mut dyn PackHost,
        event: Event<'_>,
        implicit: Implicit,
        interrupt: &AtomicBool,
        deadline: Instant,
    ) -> Result<std::result::Result<Applied, Failure>> {
        let not_started = |message: String| {
            Ok(Err(Failure {
                message,
                ran: false,
                unknown: false,
            }))
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if interrupt.load(Ordering::SeqCst) || remaining.is_zero() {
            return not_started("pack call stopped before it started".into());
        }
        let methods = match (self.binding.manifest.ui_protocol, host.ui()) {
            (Some(1), Some(ui)) => {
                let mut methods = ui.methods();
                methods.sort();
                methods
            }
            _ => vec![],
        };
        let mut context = json!({"turn_id":self.turn_id,"iteration":self.iteration,"history_messages":0,"compacted":false});
        if !methods.is_empty() {
            context["ui_methods"] = json!(methods);
        }
        let mut request = json!({
            "version":1,
            "pack":self.binding.pack,
            "session_id":host.session(),
            "cwd":host.cwd(),
            "state":self.snapshot.state,
            "context":context,
        });
        let name = match event {
            Event::Activate => "activate",
            Event::Idle => "idle",
            Event::Tool(tool, input) => {
                request["tool"] = json!(tool);
                request["input"] = input.clone();
                "tool"
            }
            Event::UiResponse(ui, response) => {
                request["ui"] =
                    json!({"request_id":ui.id,"method":ui.method,"response":response.to_json()});
                "ui_response"
            }
        };
        request["event"] = json!(name);
        if serde_json::to_vec(&request).map_or(true, |bytes| bytes.len() > REQUEST_CAP) {
            return not_started(format!("pack runtime request exceeds {REQUEST_CAP} bytes"));
        }
        let output = match host.invoke(&self.binding.pack, &request, interrupt, remaining) {
            Invoked::Output(output) => output,
            Invoked::NotStarted(message) => return not_started(message),
            Invoked::Failed(message) => return Ok(Err(Failure::known(message))),
            Invoked::Unknown(message) => {
                return Ok(Err(Failure {
                    message,
                    ran: true,
                    unknown: true,
                }));
            }
        };
        if serde_json::to_vec(&output).map_or(true, |bytes| bytes.len() > RESPONSE_CAP) {
            return Ok(Err(Failure::known(format!(
                "pack runtime response exceeds {RESPONSE_CAP} bytes"
            ))));
        }
        let response = match parse_response(&output) {
            Ok(response) => response,
            Err(error) => return Ok(Err(Failure::known(error))),
        };
        if response.is_error {
            // A helper reporting `may_have_run` (the MCP bridge after dispatch) does not
            // know whether its effect happened.
            let unknown = response.content.trim_start().starts_with("may_have_run");
            return Ok(Err(Failure {
                message: format!(
                    "pack runtime reported an error{}",
                    if response.content.trim().is_empty() {
                        String::new()
                    } else {
                        format!(": {}", response.content.trim())
                    }
                ),
                // `not_sent` (the bridge before dispatch) is a known outcome: nothing ran.
                ran: !not_sent(&response.content),
                unknown,
            }));
        }
        let implicit = match implicit {
            Implicit::No => false,
            Implicit::Idle { explicit, content } => {
                !explicit
                    && response.ui_request.is_none()
                    && !response
                        .effects
                        .iter()
                        .any(|effect| matches!(effect, Effect::Continue(..)))
                    && (content
                        || !response.content.trim().is_empty()
                        || response
                            .effects
                            .iter()
                            .any(|effect| matches!(effect, Effect::Steer(_))))
            }
        };
        let (after, applied, views) = match apply(&self.binding, &self.snapshot, response, implicit)
        {
            Ok(applied) => applied,
            Err(error) => return Ok(Err(Failure::known(error))),
        };
        store.put_anchor(
            &self.scope,
            json!({
                "attempt":self.attempt,
                "revision":self.revision,
                "child":self.child,
                "step":self.step,
                "pack":self.binding.pack,
                "event":name,
                "steer":applied.steer,
            }),
            &after,
        )?;
        self.step += 1;
        self.snapshot = after;
        for (title, markdown) in &views {
            host.view(&self.binding.pack, title, markdown);
        }
        Ok(Ok(applied))
    }
}

/// The snapshot a response leads to, what it says, and the views it shows.
type Commit = (Snapshot, Applied, Vec<(String, String)>);

/// Dext's `apply_pack_runtime_invocation`: everything or nothing is committed.
fn apply(
    binding: &Binding,
    before: &Snapshot,
    response: Response,
    implicit_continuation: bool,
) -> Checked<Commit> {
    if response
        .effects
        .iter()
        .any(|effect| matches!(effect, Effect::Tools(_)))
        && binding.manifest.tool_protocol != Some(1)
    {
        return Err("pack runtime tools effect requires tool_protocol: 1".into());
    }
    if response.ui_request.is_some() && binding.manifest.ui_protocol != Some(1) {
        return Err("pack runtime returned ui_request without declaring ui_protocol: 1".into());
    }
    let mut prompts = vec![];
    let mut views = vec![];
    let mut proposed = vec![];
    let mut steer = vec![];
    for effect in response.effects {
        match effect {
            Effect::Steer(text) => steer.push(text),
            Effect::Continue(prompt, delay_ms) => prompts.push((prompt, delay_ms)),
            Effect::Tools(tools) => proposed.extend(tools),
            Effect::View(title, markdown) => views.push((title, markdown)),
        }
    }
    let explicit = prompts.len() as u64;
    let mut next_pending = before.pending.clone();
    next_pending.extend(prompts);
    validate_pending(&next_pending)?;
    if let Some(state) = &response.state {
        validate_state(state)?;
    }
    let next_continuations = before
        .continuations_used
        .checked_add(explicit + u64::from(implicit_continuation))
        .ok_or("pack runtime continuation counter overflow")?;
    if next_continuations > binding.manifest.max_continuations {
        return Err(format!(
            "pack runtime continuation limit reached ({})",
            binding.manifest.max_continuations
        ));
    }
    let mut next_tools = before.tools.clone();
    if !proposed.is_empty() {
        validate_tools(&proposed, &[])?;
        for tool in &proposed {
            if tool.risk > binding.max_dynamic_risk {
                return Err(format!(
                    "pack runtime tool {} declares {} risk; the operator's grants for {} allow promoted tools up to {}",
                    tool.name,
                    tool.risk.as_str(),
                    binding.pack,
                    binding.max_dynamic_risk.as_str()
                ));
            }
            let existing = binding
                .manifest
                .tools
                .iter()
                .chain(&next_tools)
                .find(|known| known.name == tool.name);
            match existing {
                Some(previous) if previous != tool => {
                    return Err(format!(
                        "pack runtime tool definition changed; reactivate the pack: {}",
                        tool.name
                    ));
                }
                Some(_) => {}
                None => next_tools.push(tool.clone()),
            }
        }
        let catalog: Vec<Tool> = binding
            .manifest
            .tools
            .iter()
            .chain(&next_tools)
            .cloned()
            .collect();
        validate_tools(&catalog, &[])?;
    }
    let mut after = before.clone();
    after.tools = next_tools;
    if let Some(state) = response.state {
        after.state = state;
    }
    after.continuations_used = next_continuations;
    after.pending = next_pending;
    if let Some(view) = views.last() {
        after.view = Some(view.clone());
    }
    Ok((
        after,
        Applied {
            content: response.content,
            ui_request: response.ui_request,
            steer,
            continued: explicit > 0,
        },
        views,
    ))
}

fn append(content: &mut String, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    if !content.is_empty() {
        content.push_str("\n\n");
    }
    content.push_str(text);
}

impl Store {
    fn ledger_stage(&mut self, kind: &str, scope: &str) -> Result<Stage> {
        let stage = Stage {
            id: format!("{kind}{}", self.epoch),
            scope: scope.into(),
            owner_epoch: self.epoch,
        };
        self.begin_stage(&stage)?;
        Ok(stage)
    }

    /// This handle's ledger of `scope`, read from the database on first use.
    fn scoped(&mut self, scope: &str) -> Result<&mut Scoped> {
        if !self.pack_ledger.0.contains_key(scope) {
            let loaded = Scoped::load(&self.db, scope)?;
            self.pack_ledger.0.insert(scope.into(), loaded);
        }
        Ok(self.pack_ledger.0.get_mut(scope).expect("loaded above"))
    }

    /// The snapshot object first, then the anchor naming it: a crash between them
    /// leaves an unreferenced snapshot, never an anchor without one.
    fn put_anchor(&mut self, scope: &str, mut anchor: Value, snapshot: &Snapshot) -> Result<()> {
        self.scoped(scope)?;
        let stage = self.ledger_stage(SNAPSHOTS, scope)?;
        let digest = self.put_object(&stage, &tagged(SNAPSHOT, &snapshot.to_json())?)?;
        anchor["snapshot"] = json!(digest);
        anchor["scope"] = json!(scope);
        let stage = self.ledger_stage(ANCHORS, scope)?;
        let digest = self.put_object(&stage, &tagged(ANCHOR, &anchor)?)?;
        if let Some(parsed) = Anchor::parse(&anchor) {
            self.scoped(scope)?.add_anchor(digest, parsed);
        }
        Ok(())
    }

    fn put_mark(&mut self, scope: &str, mut mark: Value) -> Result<()> {
        self.scoped(scope)?;
        let stage = self.ledger_stage(MARKS, scope)?;
        mark["scope"] = json!(scope);
        let digest = self.put_object(&stage, &tagged(MARK, &mark)?)?;
        self.scoped(scope)?.add_mark(digest, mark);
        Ok(())
    }

    /// Ledger records this handle read from the database for `scope`.
    #[cfg(all(test, feature = "agent", unix))]
    pub(crate) fn pack_ledger_reads(&mut self, scope: &str) -> Result<usize> {
        Ok(self.scoped(scope)?.reads)
    }

    /// Each active pack's snapshot at the head; `current` adds that running attempt's
    /// own anchors.
    pub fn pack_states(
        &mut self,
        scope: &str,
        current: Option<&str>,
    ) -> Result<BTreeMap<String, Snapshot>> {
        self.scoped(scope)?;
        let scoped = self.pack_ledger.0.get_mut(scope).expect("loaded above");
        let latest = scoped.latest(&self.db, scope, current)?;
        load_snapshots(&self.db, latest)
    }

    /// Hands the host the packs active for this script, so promoted tools are
    /// searchable and callable. Skipped when the policy binds no session pack.
    pub(crate) fn refresh_packs(
        &mut self,
        parent: &DispatchRequest,
        host: &mut dyn crate::code_mode::Host,
    ) -> Result<()> {
        if let Some(packs) = host.packs()
            && packs.has_session_packs()
        {
            let states = self.pack_states(&parent.scope, Some(&parent.attempt.id))?;
            packs.refresh(&states);
        }
        Ok(())
    }

    /// Steering text the packs of one run_code attempt asked for, in call order.
    pub fn pack_steers(&mut self, scope: &str, attempt: &str) -> Result<Vec<(String, String)>> {
        let scoped = self.scoped(scope)?;
        let mut own: Vec<&Anchor> = scoped
            .by_attempt
            .get(attempt)
            .into_iter()
            .flatten()
            .map(|index| &scoped.anchors[*index])
            .collect();
        own.sort_by_key(|anchor| (anchor.child, anchor.step));
        Ok(own
            .into_iter()
            .flat_map(|anchor| {
                anchor
                    .steer
                    .iter()
                    .map(|text| (anchor.pack.clone(), text.clone()))
            })
            .collect())
    }

    /// Steering text of the run_code calls of one tool round (`effects`, in call
    /// order) whose results were adopted. Read from the ledger, so a crash after a
    /// call's result was committed loses none of it.
    pub fn pack_round_steers(
        &mut self,
        scope: &str,
        effects: &[String],
    ) -> Result<Vec<(String, String)>> {
        if self.scoped(scope)?.by_attempt.is_empty() {
            return Ok(vec![]);
        }
        let mut steers = vec![];
        for effect in effects {
            let Some(attempt) = self.attempt_for(effect)? else {
                continue;
            };
            let applied = self
                .pack_ledger
                .0
                .get_mut(scope)
                .expect("loaded above")
                .applied(&self.db, &attempt.id)?;
            if applied.is_some() {
                steers.extend(self.pack_steers(scope, &attempt.id)?);
            }
        }
        Ok(steers)
    }

    /// Whether any pack is active at the head (cheap when none ever was).
    pub fn pack_active(&mut self, scope: &str) -> Result<bool> {
        Ok(!self.scoped(scope)?.anchors.is_empty() && !self.pack_states(scope, None)?.is_empty())
    }

    /// Runs one session pack tool call of a reserved run_code child: activation on
    /// first use, the tool event, then up to 16 UI round trips. Each accepted response
    /// is anchored before the next starts, so a crash keeps the state the pack had
    /// reached and never repeats an invocation (the child stays uncertain).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_pack_child(
        &mut self,
        parent: &DispatchRequest,
        stage: &Stage,
        id: u64,
        prepared: &Value,
        host: &mut dyn PackHost,
        interrupt: &AtomicBool,
        remaining: Duration,
    ) -> Result<Value> {
        let deadline = Instant::now() + remaining;
        let refuse = |message: String| {
            Failure {
                message,
                ran: false,
                unknown: false,
            }
            .to_json()
        };
        let (Some(pack), Some(tool)) = (prepared["pack"].as_str(), prepared["tool"].as_str())
        else {
            return Ok(refuse("invalid pack call".into()));
        };
        let binding = match host.binding(pack) {
            Ok(binding) => binding,
            Err(error) => return Ok(refuse(error)),
        };
        let saved = self
            .pack_states(&parent.scope, Some(&parent.attempt.id))?
            .remove(pack);
        if let Some(snapshot) = &saved
            && let Err(error) = snapshot.restore(&binding)
        {
            return Ok(refuse(error));
        }
        let fresh = saved.is_none();
        let mut chain = Chain {
            scope: parent.scope.clone(),
            attempt: Some(parent.attempt.id.clone()),
            revision: None,
            child: id,
            step: 0,
            snapshot: saved.unwrap_or_else(|| Snapshot::fresh(&binding)),
            binding,
            turn_id: host.turn(),
            iteration: parent.expected_revision,
        };
        let mut content = String::new();
        if fresh {
            match chain.step(
                self,
                host,
                Event::Activate,
                Implicit::No,
                interrupt,
                deadline,
            )? {
                Ok(applied) => {
                    if !applied.content.trim().is_empty() {
                        content = format!("[pack runtime activation]\n{}", applied.content);
                    }
                    // Activation may promote tools or ask for UI only through the
                    // tool call that follows; a request it makes is answered here.
                    if let Some(request) = applied.ui_request
                        && let Err(failure) = self.ui_rounds(
                            &mut chain,
                            host,
                            stage,
                            parent,
                            id,
                            request,
                            &mut content,
                            interrupt,
                            deadline,
                        )?
                    {
                        // Activation already ran.
                        return Ok(Failure {
                            ran: true,
                            ..failure
                        }
                        .to_json());
                    }
                }
                Err(failure) => return Ok(failure.to_json()),
            }
        }
        let declared = chain.snapshot.tool(&chain.binding, tool).cloned();
        // Refused before the tool event: only an activation may have run.
        let refused = |message: String| {
            Failure {
                message,
                ran: fresh,
                unknown: false,
            }
            .to_json()
        };
        let Some(declared) = declared else {
            return Ok(refused(format!(
                "pack runtime does not declare tool {tool}"
            )));
        };
        if let Err(error) = validate_tool_input(&declared, &prepared["input"]) {
            return Ok(refused(error));
        }
        let applied = match chain.step(
            self,
            host,
            Event::Tool(tool, &prepared["input"]),
            Implicit::No,
            interrupt,
            deadline,
        )? {
            Ok(applied) => applied,
            Err(failure) => {
                // Activation already ran, so the call as a whole may have run.
                let ran = failure.ran || fresh;
                return Ok(Failure { ran, ..failure }.to_json());
            }
        };
        append(&mut content, &applied.content);
        if let Some(request) = applied.ui_request
            && let Err(failure) = self.ui_rounds(
                &mut chain,
                host,
                stage,
                parent,
                id,
                request,
                &mut content,
                interrupt,
                deadline,
            )?
        {
            return Ok(Failure {
                ran: true,
                ..failure
            }
            .to_json());
        }
        Ok(json!({"ok":true,"data":{"version":1,"content":content,"is_error":false}}))
    }

    /// Dext's `resolve_pack_runtime_ui` for a tool chain: each answer is journaled in
    /// the child's stage before the pack hears it.
    #[allow(clippy::too_many_arguments)]
    fn ui_rounds(
        &mut self,
        chain: &mut Chain,
        host: &mut dyn PackHost,
        stage: &Stage,
        parent: &DispatchRequest,
        id: u64,
        mut request: UiRequest,
        content: &mut String,
        interrupt: &AtomicBool,
        deadline: Instant,
    ) -> Result<std::result::Result<(), Failure>> {
        let mut rounds = 0;
        loop {
            rounds += 1;
            if rounds > UI_ROUND_LIMIT {
                return Ok(Err(Failure::known(format!(
                    "pack runtime requested more than {UI_ROUND_LIMIT} UI round trips"
                ))));
            }
            let answer = ask(host, &chain.binding.pack, &request, interrupt, deadline);
            if interrupt.load(Ordering::SeqCst) {
                return Ok(Err(Failure::known(
                    "pack runtime UI request interrupted".into(),
                )));
            }
            self.put_object(
                stage,
                &serde_json::to_vec(&json!({"kind":"pack-ui-answer","parent":parent.attempt.id,"id":id,"step":chain.step,"pack":chain.binding.pack,"request":request.to_json(),"response":answer.to_json()}))
                    .map_err(|_| Error::Invalid)?,
            )?;
            let applied = match chain.step(
                self,
                host,
                Event::UiResponse(&request, &answer),
                Implicit::No,
                interrupt,
                deadline,
            )? {
                Ok(applied) => applied,
                Err(failure) => return Ok(Err(failure)),
            };
            append(content, &applied.content);
            match applied.ui_request {
                Some(next) => request = next,
                None => return Ok(Ok(())),
            }
        }
    }

    /// Runs idle once for every pack active when the turn `execution` completed (at
    /// the head, `revision`) and returns the follow-up turn its continuations ask for.
    /// Records are keyed by the turn's execution id, so a compaction or a restart
    /// (before the turn's end was recorded, or before its follow-up started) finds
    /// them: finished idles are reused, one whose outcome is unknown is not run again,
    /// and the decided follow-up is recorded before it is delivered. Only packs the
    /// operator's policy still binds (and whose state restores) idle or get their
    /// queued continuations delivered; the others' are dropped with a note.
    pub fn pack_turn_end(
        &mut self,
        scope: &str,
        revision: u64,
        execution: &str,
        mut host: Option<&mut dyn PackHost>,
        interrupt: &AtomicBool,
    ) -> Result<TurnEnd> {
        let marks: Vec<Value> = self
            .scoped(scope)?
            .marks
            .iter()
            .filter(|mark| mark["execution"] == execution)
            .cloned()
            .collect();
        let mut end = TurnEnd::default();
        if let Some(recorded) = marks.iter().find(|mark| mark["kind"] == "turn-end") {
            self.clear_pending(scope, revision)?;
            if marks.iter().any(|mark| mark["kind"] == "cancelled") {
                return Ok(end);
            }
            // Decided before a restart: each part still needs its pack's binding.
            let states = self.pack_states(scope, None)?;
            let mut parts = vec![];
            for part in recorded["parts"].as_array().into_iter().flatten() {
                let (Some(pack), Some(text)) = (part[0].as_str(), part[1].as_str()) else {
                    continue;
                };
                match bound(host.as_deref_mut(), pack, states.get(pack)) {
                    Ok(_) => parts.push(text.to_string()),
                    Err(error) => end.notes.push(format!(
                        "pack {pack}: its recorded follow-up is not delivered: {error}"
                    )),
                }
            }
            end.follow_up = (!parts.is_empty()).then(|| FollowUp {
                prompt: parts.join("\n\n"),
                delay_ms: recorded["delay_ms"].as_u64().unwrap_or(0).min(MAX_DELAY_MS),
            });
            return Ok(end);
        }
        let states = self.pack_states(scope, None)?;
        if states.is_empty() || rewound_to(&self.db, scope, revision)? {
            return Ok(end);
        }
        let mut step = self.scoped(scope)?.next_step(revision);
        let mut refused: BTreeMap<String, String> = BTreeMap::new();
        let mut idle_text: Vec<(String, String)> = vec![];
        for (pack, snapshot) in states {
            let binding = match bound(host.as_deref_mut(), &pack, Some(&snapshot)) {
                Ok(binding) => binding,
                Err(error) => {
                    end.notes
                        .push(format!("pack {pack}: idle skipped: {error}"));
                    refused.insert(pack, error);
                    continue;
                }
            };
            if marks
                .iter()
                .any(|mark| mark["kind"] == "idle-call" && mark["pack"] == pack.as_str())
            {
                let scoped = self.scoped(scope)?;
                let idled = scoped
                    .by_revision
                    .get(&revision)
                    .into_iter()
                    .flatten()
                    .any(|index| {
                        let anchor = &scoped.anchors[*index];
                        anchor.pack == pack && anchor.event == "idle"
                    });
                if !idled {
                    end.notes.push(format!(
                        "pack {pack}: the idle call before the restart has an unknown outcome; it is not run again"
                    ));
                }
                continue;
            }
            let host = host.as_deref_mut().expect("bound needs a host");
            self.put_mark(
                scope,
                json!({"kind":"idle-call","execution":execution,"revision":revision,"pack":pack,"step":step}),
            )?;
            let mut chain = Chain {
                scope: scope.into(),
                attempt: None,
                revision: Some(revision),
                child: 0,
                step,
                binding,
                snapshot,
                turn_id: execution.into(),
                iteration: revision,
            };
            let deadline = Instant::now() + UI_REPLY_TIMEOUT;
            let mut content = String::new();
            let mut implicit = Implicit::Idle {
                explicit: false,
                content: false,
            };
            let mut event_ui: Option<(UiRequest, UiResponse)> = None;
            let mut rounds = 0;
            let outcome = loop {
                let event = match &event_ui {
                    None => Event::Idle,
                    Some((request, answer)) => Event::UiResponse(request, answer),
                };
                let applied = match chain.step(self, host, event, implicit, interrupt, deadline)? {
                    Ok(applied) => applied,
                    Err(failure) => break Err(failure),
                };
                append(&mut content, &applied.content);
                for text in &applied.steer {
                    append(&mut content, &format!("[pack-runtime steer]\n{text}"));
                }
                if let Implicit::Idle { explicit, .. } = implicit {
                    implicit = Implicit::Idle {
                        explicit: explicit || applied.continued,
                        content: !content.trim().is_empty(),
                    };
                }
                let Some(request) = applied.ui_request else {
                    break Ok(());
                };
                rounds += 1;
                if rounds > UI_ROUND_LIMIT {
                    break Err(Failure::known(format!(
                        "pack runtime requested more than {UI_ROUND_LIMIT} UI round trips"
                    )));
                }
                let answer = ask(host, &pack, &request, interrupt, deadline);
                if interrupt.load(Ordering::SeqCst) {
                    break Err(Failure::known("pack runtime UI request interrupted".into()));
                }
                event_ui = Some((request, answer));
            };
            step = chain.step;
            if let Err(failure) = outcome {
                end.notes
                    .push(format!("pack {pack}: idle failed: {}", failure.message));
            }
            if !content.trim().is_empty() {
                idle_text.push((pack, content));
            }
        }
        let mut parts = vec![];
        let mut delay_ms = 0;
        for (pack, text) in idle_text {
            let text = format!("[pack-runtime idle] {pack}\n{text}");
            parts.push(json!([pack, text]));
        }
        for (pack, snapshot) in self.pack_states(scope, None)? {
            if snapshot.pending.is_empty() {
                continue;
            }
            if let Some(error) = refused.get(&pack) {
                end.notes.push(format!(
                    "pack {pack}: {} queued continuation(s) not delivered: {error}",
                    snapshot.pending.len()
                ));
                continue;
            }
            for (prompt, delay) in snapshot.pending {
                parts.push(json!([
                    pack,
                    format!("[pack-runtime continue] {pack}\n{prompt}")
                ]));
                delay_ms = delay_ms.max(delay);
            }
        }
        end.follow_up = (!parts.is_empty()).then(|| FollowUp {
            prompt: parts
                .iter()
                .filter_map(|part| part[1].as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
            delay_ms,
        });
        self.put_mark(
            scope,
            json!({"kind":"turn-end","execution":execution,"revision":revision,"parts":parts,"delay_ms":delay_ms}),
        )?;
        self.clear_pending(scope, revision)?;
        Ok(end)
    }

    /// Delivered (or cancelled) continuations leave every pack's queue at `revision`.
    fn clear_pending(&mut self, scope: &str, revision: u64) -> Result<()> {
        let step = self.scoped(scope)?.next_step(revision);
        for (offset, (pack, mut snapshot)) in self
            .pack_states(scope, None)?
            .into_iter()
            .filter(|(_, snapshot)| !snapshot.pending.is_empty())
            .enumerate()
        {
            snapshot.pending.clear();
            self.put_anchor(
                scope,
                json!({"attempt":null,"revision":revision,"child":0,"step":step + offset as u64,"pack":pack,"event":"turn-end","steer":[]}),
                &snapshot,
            )?;
        }
        Ok(())
    }

    /// A follow-up interrupted while it waited is not delivered after a restart.
    pub fn pack_cancel_follow_up(
        &mut self,
        scope: &str,
        revision: u64,
        execution: &str,
    ) -> Result<()> {
        self.put_mark(
            scope,
            json!({"kind":"cancelled","execution":execution,"revision":revision}),
        )
    }

    /// Ledger records for operator inspection (`code-receipts`).
    pub fn pack_ledger(&mut self, scope: &str) -> Result<Vec<Value>> {
        let mut out = vec![];
        for (stages, tag, kind) in [(ANCHORS, ANCHOR, "pack-anchor"), (MARKS, MARK, "pack-mark")] {
            for (_, mut record) in records(&self.db, scope, stages, tag)? {
                record["record"] = json!(kind);
                out.push(record);
            }
        }
        Ok(out)
    }
}

/// A pack's current binding when the operator's policy still grants it and its saved
/// state restores under it; otherwise why not.
fn bound<'h>(
    host: Option<&mut (dyn PackHost + 'h)>,
    pack: &str,
    snapshot: Option<&Snapshot>,
) -> Checked<Binding> {
    let host = host.ok_or("the code policy could not be read")?;
    let binding = host.binding(pack)?;
    if let Some(snapshot) = snapshot {
        snapshot.restore(&binding)?;
    }
    Ok(binding)
}

/// Asks the attached UI, or refuses clearly when there is none.
fn ask(
    host: &dyn PackHost,
    pack: &str,
    request: &UiRequest,
    interrupt: &AtomicBool,
    deadline: Instant,
) -> UiResponse {
    let Some(ui) = host.ui() else {
        return UiResponse::error(
            "unsupported",
            "no UI is attached to this session; answer pack requests from the phone UI or an interactive terminal",
        );
    };
    if !ui.methods().iter().any(|method| method == &request.method) {
        return UiResponse::error(
            "unsupported",
            format!("host did not advertise UI method '{}'", request.method),
        );
    }
    let answer = ui.ask(
        pack,
        request,
        interrupt,
        deadline.min(Instant::now() + UI_REPLY_TIMEOUT),
    );
    match answer.validate() {
        Ok(()) => answer,
        Err(error) => UiResponse::error("invalid", error),
    }
}

#[cfg(all(test, feature = "agent", unix))]
mod session_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, risk: Risk) -> Tool {
        Tool {
            name: name.into(),
            description: "Look up a key".into(),
            input_schema: json!({"type":"object","properties":{"key":{"type":"string"}},"required":["key"],"additionalProperties":false}),
            risk,
        }
    }

    pub(super) fn binding() -> Binding {
        Binding {
            pack: "fixture".into(),
            source: "/packs/fixture/bin/run".into(),
            manifest_sha256: "m".into(),
            executable_sha256: "e".into(),
            manifest: Manifest {
                command: "bin/run".into(),
                args: vec![],
                timeout_seconds: None,
                tools: vec![tool("measure", Risk::Read)],
                ui_protocol: Some(1),
                tool_protocol: Some(1),
                max_continuations: 2,
            },
            granted: vec!["measure".into()],
            max_dynamic_risk: Risk::Write,
        }
    }

    #[test]
    fn manifests_follow_dexts_fail_closed_rules() {
        let good = json!({"version":1,"command":"bin/run","tools":[tool("measure", Risk::Read).to_json()],"tool_protocol":1});
        let manifest = parse_manifest(&serde_json::to_vec(&good).unwrap()).unwrap();
        assert_eq!(manifest.max_continuations, DEFAULT_MAX_CONTINUATIONS);
        for bad in [
            json!({"version":2,"command":"x"}),
            json!({"version":1,"command":"x","extra":true}),
            json!({"version":1,"command":"x","ui_protocol":2}),
            json!({"version":1,"command":"x","max_continuations":1001}),
            json!({"version":1,"command":"x","timeout_seconds":0}),
            json!({"version":1,"command":"x","tools":[{"name":"9bad","description":"d","input_schema":{"type":"object"}}]}),
            json!({"version":1,"command":"x","tools":[{"name":"t","description":"d","input_schema":{"type":"object","pattern":"x"}}]}),
            json!({"version":1,"command":"x","tools":[{"name":"t","description":"d","input_schema":{"type":"array"}}]}),
        ] {
            assert!(
                parse_manifest(&serde_json::to_vec(&bad).unwrap()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn responses_are_bounded_and_terminal_safe() {
        let ok = json!({"version":1,"content":"done","state":{"n":1},"effects":[{"type":"steer","text":"look"},{"type":"continue","prompt":"next","delay_ms":5},{"type":"view","title":"T","markdown":"# M"}]});
        let response = parse_response(&ok).unwrap();
        assert_eq!(response.effects.len(), 3);
        assert_eq!(response.state, Some(json!({"n":1})));
        let many: Vec<Value> = (0..17)
            .map(|_| json!({"type":"steer","text":"x"}))
            .collect();
        for bad in [
            json!({"version":2}),
            json!({"version":1,"unknown":1}),
            json!({"version":1,"content":"\u{1b}[31m"}),
            json!({"version":1,"effects":many}),
            json!({"version":1,"effects":[{"type":"continue","prompt":"p","delay_ms":30001}]}),
            json!({"version":1,"effects":[{"type":"view","title":"","markdown":""}]}),
            json!({"version":1,"effects":[{"type":"explode"}]}),
            json!({"version":1,"state":"x".repeat(STATE_CAP)}),
            json!({"version":1,"ui_request":{"id":"bad id","method":"form"}}),
            json!({"version":1,"ui_request":{"id":"a","method":"form","extra":1}}),
        ] {
            assert!(parse_response(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ui_answers_parse_and_validate_like_the_dext_bridge() {
        assert_eq!(
            UiResponse::parse(&json!({"status":"ok","value":{"name":"Ada"}})).unwrap(),
            UiResponse::Ok(json!({"name":"Ada"}))
        );
        assert_eq!(
            UiResponse::parse(&json!({"status":"cancelled"})).unwrap(),
            UiResponse::Cancelled
        );
        for bad in [
            json!({"status":"maybe"}),
            json!({"status":"error","code":"bad code","message":"m"}),
            json!({"status":"error","code":"c"}),
            json!({"status":"ok","value":"x".repeat(UI_PAYLOAD_CAP)}),
            json!({"status":"ok","value":"\u{7}"}),
        ] {
            assert!(UiResponse::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn effects_commit_atomically_with_continuation_and_tool_rules() {
        let binding = binding();
        let before = Snapshot::fresh(&binding);
        let response = |effects: Value| {
            parse_response(&json!({"version":1,"content":"c","state":{"n":2},"effects":effects}))
                .unwrap()
        };
        let (after, applied, views) = apply(
            &binding,
            &before,
            response(json!([
                {"type":"continue","prompt":"again","delay_ms":10},
                {"type":"tools","tools":[tool("lookup", Risk::Write).to_json()]},
                {"type":"view","title":"V","markdown":"m"},
                {"type":"steer","text":"s"}
            ])),
            false,
        )
        .unwrap();
        assert_eq!(after.state, json!({"n":2}));
        assert_eq!(after.pending, vec![("again".to_string(), 10)]);
        assert_eq!(after.continuations_used, 1);
        assert_eq!(after.tools.len(), 1);
        assert_eq!(after.view, Some(("V".into(), "m".into())));
        assert_eq!(applied.steer, vec!["s".to_string()]);
        assert_eq!(views.len(), 1);
        assert!(after.tool(&binding, "lookup").is_some());
        assert!(after.tool(&binding, "measure").is_some());
        // Identical definitions are idempotent; a changed one is refused.
        assert!(
            apply(
                &binding,
                &after,
                response(json!([{"type":"tools","tools":[tool("lookup", Risk::Write).to_json()]}])),
                false
            )
            .is_ok()
        );
        let mut changed = tool("lookup", Risk::Write);
        changed.description = "Changed".into();
        let refused = apply(
            &binding,
            &after,
            response(json!([{"type":"tools","tools":[changed.to_json()]}])),
            false,
        )
        .unwrap_err();
        assert!(refused.contains("definition changed"), "{refused}");
        // Above the operator's risk ceiling, and over the continuation budget.
        assert!(
            apply(
                &binding,
                &before,
                response(json!([{"type":"tools","tools":[tool("wipe", Risk::Danger).to_json()]}])),
                false
            )
            .unwrap_err()
            .contains("allow promoted tools up to write")
        );
        let refused = apply(
            &binding,
            &after,
            response(json!([{"type":"continue","prompt":"a"},{"type":"continue","prompt":"b"}])),
            false,
        )
        .unwrap_err();
        assert!(refused.contains("continuation limit"), "{refused}");
        let mut plain = binding.clone();
        plain.manifest.tool_protocol = None;
        plain.manifest.ui_protocol = None;
        assert!(
            apply(
                &plain,
                &before,
                response(json!([{"type":"tools","tools":[tool("lookup", Risk::Read).to_json()]}])),
                false
            )
            .is_err()
        );
        let asking =
            parse_response(&json!({"version":1,"ui_request":{"id":"q","method":"form"}})).unwrap();
        assert!(apply(&plain, &before, asking, false).is_err());
    }

    #[test]
    fn restore_refuses_changed_identity_and_overdrawn_budgets() {
        let binding = binding();
        let snapshot = Snapshot::fresh(&binding);
        assert!(snapshot.restore(&binding).is_ok());
        for change in 0..4 {
            let mut other = binding.clone();
            match change {
                0 => other.pack = "other".into(),
                1 => other.source = "/elsewhere".into(),
                2 => other.manifest_sha256 = "m2".into(),
                _ => other.executable_sha256 = "e2".into(),
            }
            assert!(
                snapshot
                    .restore(&other)
                    .unwrap_err()
                    .contains("changed its identity")
            );
        }
        let mut overdrawn = snapshot.clone();
        overdrawn.continuations_used = 3;
        assert!(overdrawn.restore(&binding).is_err());
        let round = Snapshot::parse(&snapshot.to_json()).unwrap();
        assert_eq!(round, snapshot);
    }

    #[test]
    fn promoted_tool_inputs_use_dexts_schema_checks() {
        let lookup = tool("lookup", Risk::Read);
        assert!(validate_tool_input(&lookup, &json!({"key":"k"})).is_ok());
        assert!(validate_tool_input(&lookup, &json!({})).is_err());
        assert!(validate_tool_input(&lookup, &json!({"key":1})).is_err());
        assert!(validate_tool_input(&lookup, &json!({"key":"k","x":1})).is_err());
    }
}
