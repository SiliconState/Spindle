//! Tools the embedding app supplies. A session's settings name them (`Settings::tools`)
//! beside a choice of Spindle's own tools (`BuiltinTools`); the model sees them in every
//! request after the built-in ones, in the order given. A call is reserved in the store
//! before the app is asked (`AppTools::call`), recorded with the app's answer, and adopted
//! like any workspace result, so files the answer writes are rewound with the step.
//!
//! Every call carries the effect id of the model's tool call (`AppCall::call`): the same
//! for as long as the call exists, across crashes and restarts, and new for a call a
//! rewound conversation makes again. An app that keeps its own record of calls by that id
//! never runs one twice. A call whose owner stopped before an answer was recorded is asked
//! again with `again` set; nothing is recorded for it until the app answers.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Most app tools a session can offer.
pub const MAX_TOOLS: usize = 64;
/// Longest tool description.
pub const MAX_DESCRIPTION: usize = 4096;
/// Largest input schema, as JSON.
pub const MAX_SCHEMA: usize = 64 * 1024;
/// Longest answer text the app can give (longer text is refused, not cut).
pub const MAX_ANSWER: usize = 4 * 1024 * 1024;
/// Longest app instructions added to the system prompt.
pub const MAX_INSTRUCTIONS: usize = 32 * 1024;
/// How long a call waits for its answer when its tool sets no `timeout_s`.
pub const DEFAULT_TIMEOUT_S: u64 = 600;
/// Longest wait a tool may set.
pub const MAX_TIMEOUT_S: u64 = 86_400;
pub const DEFAULT_MAX_OUTPUT: usize = 64 * 1024;
pub const MAX_OUTPUT: usize = 1024 * 1024;
pub const MAX_DETAILS: usize = 1024 * 1024;

/// One tool the app supplies.
#[derive(Clone, Debug, PartialEq)]
pub struct AppTool {
    /// What the model calls it (`crate::turn::app_tool_name`).
    pub name: String,
    /// What it does, as the model reads it.
    pub description: String,
    /// JSON Schema of its input (an object).
    pub input_schema: Value,
    /// Whether running a call twice is harmless, so a call asked `again` may simply run
    /// again. Clients use it to answer `again` calls; Spindle only passes it on.
    pub rerun: bool,
    /// How long a call waits for the app's answer, in seconds.
    pub timeout_s: u64,
    /// Maximum text bytes exposed to the model; full output stays in its artifact.
    pub max_output: usize,
}

impl AppTool {
    /// A tool from its JSON form (`{name, description, input_schema, rerun?, timeout_s?}`).
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let object = value.as_object().ok_or("each tool must be an object")?;
        if let Some(key) = object.keys().find(|key| {
            ![
                "name",
                "description",
                "input_schema",
                "rerun",
                "timeout_s",
                "max_output",
            ]
            .contains(&key.as_str())
        }) {
            return Err(format!("unknown tool field `{key}`"));
        }
        let text = |key: &str| object.get(key).and_then(Value::as_str).map(String::from);
        let tool = Self {
            name: text("name").ok_or("a tool needs a `name`")?,
            description: text("description").ok_or("a tool needs a `description`")?,
            input_schema: object
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            rerun: match object.get("rerun") {
                None | Some(Value::Null) => false,
                Some(Value::String(text)) if text == "safe" => true,
                Some(Value::String(text)) if text == "no" => false,
                Some(Value::Bool(rerun)) => *rerun,
                Some(_) => return Err("`rerun` is \"safe\" or \"no\"".into()),
            },
            max_output: match object.get("max_output") {
                None | Some(Value::Null) => DEFAULT_MAX_OUTPUT,
                Some(value) => value
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or("`max_output` is a whole number of bytes")?,
            },
            timeout_s: match object.get("timeout_s") {
                None | Some(Value::Null) => DEFAULT_TIMEOUT_S,
                Some(value) => value
                    .as_u64()
                    .ok_or("`timeout_s` is a whole number of seconds")?,
            },
        };
        tool.check()?;
        Ok(tool)
    }

    /// Its JSON form, as `from_json` takes it.
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.input_schema,
            "rerun": if self.rerun { "safe" } else { "no" },
            "timeout_s": self.timeout_s,
            "max_output": self.max_output,
        })
    }

    fn check(&self) -> Result<(), String> {
        let name = &self.name;
        if !crate::turn::app_tool_name(name) {
            return Err(format!(
                "`{name}` cannot name an app tool: use 1-64 letters, digits, `_` or `-`, not \
                 a Spindle tool's name ({}), and nothing starting with `spindle_` or `dext`",
                crate::turn::WORKSPACE_TOOLS
                    .iter()
                    .chain(crate::turn::RESERVED_TOOL_NAMES.iter())
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if self.description.trim().is_empty() || self.description.len() > MAX_DESCRIPTION {
            return Err(format!(
                "`{name}` needs a description of 1-{MAX_DESCRIPTION} bytes"
            ));
        }
        if self.input_schema.get("type").and_then(Value::as_str) != Some("object") {
            return Err(format!(
                "`{name}`'s input_schema must be a JSON Schema with \"type\": \"object\""
            ));
        }
        if self.input_schema.to_string().len() > MAX_SCHEMA {
            return Err(format!(
                "`{name}`'s input_schema is over {} KiB",
                MAX_SCHEMA / 1024
            ));
        }
        if !(1..=MAX_OUTPUT).contains(&self.max_output) {
            return Err(format!("`{name}`'s max_output must be 1-{MAX_OUTPUT}"));
        }
        if !(1..=MAX_TIMEOUT_S).contains(&self.timeout_s) {
            return Err(format!("`{name}`'s timeout_s must be 1-{MAX_TIMEOUT_S}"));
        }
        Ok(())
    }

    /// The digest of what the model and the app agree the tool is (name, description,
    /// schema, rerun): its calls' implementation label names it, so a recorded answer
    /// is bound to the definition that was offered.
    pub fn digest(&self) -> [u8; 32] {
        let canonical = json!([
            self.name,
            self.description,
            self.input_schema,
            self.rerun,
            self.timeout_s,
            self.max_output
        ]);
        Sha256::digest(canonical.to_string().as_bytes()).into()
    }
}

/// A list of tools from its JSON form, checked as a whole (count, unique names).
pub fn tools_from_json(value: &Value) -> Result<Vec<AppTool>, String> {
    let list = match value {
        Value::Null => return Ok(vec![]),
        Value::Array(list) => list,
        _ => return Err("`tools` must be an array of tool definitions".into()),
    };
    if list.len() > MAX_TOOLS {
        return Err(format!("at most {MAX_TOOLS} app tools"));
    }
    let tools = list
        .iter()
        .map(AppTool::from_json)
        .collect::<Result<Vec<_>, _>>()?;
    for (index, tool) in tools.iter().enumerate() {
        if tools[..index].iter().any(|other| other.name == tool.name) {
            return Err(format!("two tools are named `{}`", tool.name));
        }
    }
    Ok(tools)
}

/// Applies the app settings `data` names (`tools`, `builtin_tools`, `instructions`,
/// `resend_unknown`, `tasks`; absent ones stay, null or "" clears instructions) and
/// checks the result.
pub fn apply_json(settings: &mut super::Settings, data: &Value) -> Result<(), String> {
    if let Some(tools) = data.get("tools") {
        settings.tools = tools_from_json(tools)?;
    }
    match data.get("builtin_tools") {
        None | Some(Value::Null) => {}
        Some(choice) => {
            settings.builtin_tools = choice
                .as_str()
                .and_then(BuiltinTools::parse)
                .ok_or("`builtin_tools` must be all, files or none")?;
        }
    }
    match data.get("instructions") {
        None => {}
        Some(Value::Null) => settings.instructions = None,
        Some(Value::String(text)) if text.is_empty() => settings.instructions = None,
        Some(Value::String(text)) => settings.instructions = Some(text.clone()),
        Some(_) => return Err("`instructions` must be text, or null".into()),
    }
    match data.get("resend_unknown") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(resend)) => settings.resend_unknown = *resend,
        Some(_) => return Err("`resend_unknown` must be true or false".into()),
    }
    match data.get("tasks") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(tasks)) => {
            settings.tasks = *tasks;
            if *tasks {
                settings.task_policy = Default::default();
            }
        }
        Some(value) if value.is_object() => {
            settings.task_policy = super::TaskPolicy::from_json(value)?;
            settings.tasks = true;
        }
        Some(_) => {
            return Err("`tasks` must be false, true, or {max_depth, max_running, models}".into());
        }
    }
    settings.check_app()
}

/// Which of Spindle's own tools a session offers.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum BuiltinTools {
    /// Every one (commands, scripts, jobs, files, show, settings, self-edit).
    #[default]
    All,
    /// The workspace read, write and edit tools only.
    Files,
    /// No built-in tools.
    None,
}

impl BuiltinTools {
    /// The choice named `all`, `files` or `none`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "all" => Some(Self::All),
            "files" => Some(Self::Files),
            "none" => Some(Self::None),
            _ => None,
        }
    }
    /// Its name, as `parse` takes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Files => "files",
            Self::None => "none",
        }
    }
    /// Whether the built-in tool `name` is offered.
    pub fn offers(self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Files => matches!(name, "read_file" | "write_file" | "edit_file"),
            Self::None => false,
        }
    }
}

/// A call the app is asked to answer.
#[non_exhaustive]
pub struct AppCall<'a> {
    /// The call's effect id: stable across restarts, new for a rewound conversation's
    /// call. Answers name it.
    pub call: &'a str,
    /// The model's id for the call (the result block answers it).
    pub tool_use_id: &'a str,
    pub name: &'a str,
    pub input: &'a Value,
    /// An earlier owner of the session reserved this call and stopped before an answer
    /// was recorded: the app may already have run it.
    pub again: bool,
    /// The tool's `rerun` (whether running it twice is harmless).
    pub rerun: bool,
    /// How long the answer is waited for.
    pub timeout: Duration,
}

impl<'a> AppCall<'a> {
    /// A call to ask an app (a script's call of an app tool, or a transport's test).
    pub fn new(
        call: &'a str,
        tool_use_id: &'a str,
        name: &'a str,
        input: &'a Value,
        again: bool,
        rerun: bool,
        timeout: Duration,
    ) -> Self {
        Self {
            call,
            tool_use_id,
            name,
            input,
            again,
            rerun,
            timeout,
        }
    }
}

/// The app's answer to a call.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum AppAnswer {
    /// What the call did: text for the model, whether it failed, files it wrote
    /// (workspace path, bytes, executable) or removed, and its change to the app state
    /// (a JSON merge patch), all kept with the result.
    Result {
        content: String,
        failed: bool,
        writes: Vec<(String, Vec<u8>, bool)>,
        deletes: Vec<String>,
        links: Vec<crate::workspace::Link>,
        directories: Vec<String>,
        state: Option<Value>,
        /// Retained for the app/UI, never passed into model history.
        details: Option<Value>,
    },
    /// The app cannot tell whether a call asked `again` ran (with what it does know).
    Unknown(String),
    /// Nobody answered in time.
    NoAnswer,
    /// The run was stopped while waiting; nothing is recorded and the call is asked
    /// again when the session continues.
    Stopped,
}

impl AppAnswer {
    /// An answer from its JSON form: `{content, failed?, files?: {write: {path:
    /// text}, write_base64: {path: base64}, delete: [path]}, state?: {merge patch}}`,
    /// `{error}` or `{unknown: true, content?}`.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let object = value.as_object().ok_or("an answer must be an object")?;
        if let Some(key) = object.keys().find(|key| {
            ![
                "content", "error", "failed", "unknown", "files", "state", "details",
            ]
            .contains(&key.as_str())
        }) {
            return Err(format!("unknown answer field `{key}`"));
        }
        let text = |key: &str| -> Result<Option<String>, String> {
            match object.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(text)) if text.len() <= MAX_ANSWER => Ok(Some(text.clone())),
                Some(Value::String(_)) => Err(format!(
                    "`{key}` is over {} MiB",
                    MAX_ANSWER / (1024 * 1024)
                )),
                Some(_) => Err(format!("`{key}` must be text")),
            }
        };
        if object.get("unknown").and_then(Value::as_bool) == Some(true) {
            return Ok(Self::Unknown(text("content")?.unwrap_or_default()));
        }
        let (content, failed) = match (text("content")?, text("error")?) {
            (Some(_), Some(_)) => return Err("an answer has `content` or `error`, not both".into()),
            (Some(content), None) => (
                content,
                match object.get("failed") {
                    None | Some(Value::Null) => false,
                    Some(Value::Bool(failed)) => *failed,
                    Some(_) => return Err("`failed` must be true or false".into()),
                },
            ),
            (None, Some(error)) => (error, true),
            (None, None) => return Err("an answer needs `content` or `error`".into()),
        };
        let (writes, deletes) = files_from_json(object.get("files"))?;
        let state = match object.get("state") {
            None | Some(Value::Null) => None,
            Some(patch) if patch.is_object() => Some(patch.clone()),
            Some(_) => return Err("`state` is a JSON merge patch: an object".into()),
        };
        let details = object
            .get("details")
            .filter(|value| !value.is_null())
            .cloned();
        if details
            .as_ref()
            .is_some_and(|value| value.to_string().len() > MAX_DETAILS)
        {
            return Err("`details` exceeds 1 MiB".into());
        }
        Ok(Self::Result {
            content,
            failed,
            writes,
            deletes,
            links: vec![],
            directories: vec![],
            state,
            details,
        })
    }
}

type Files = (Vec<(String, Vec<u8>, bool)>, Vec<String>);

fn files_from_json(value: Option<&Value>) -> Result<Files, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok((vec![], vec![]));
    };
    let object = value.as_object().ok_or("`files` must be an object")?;
    if let Some(key) = object
        .keys()
        .find(|key| !["write", "write_base64", "executable", "delete"].contains(&key.as_str()))
    {
        return Err(format!("unknown `files` field `{key}`"));
    }
    let executable: Vec<&str> = match object.get("executable") {
        None | Some(Value::Null) => vec![],
        Some(Value::Array(list)) => list
            .iter()
            .map(|path| path.as_str().ok_or("`files.executable` lists paths"))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("`files.executable` lists paths".into()),
    };
    let mut writes = vec![];
    for (key, decode) in [("write", false), ("write_base64", true)] {
        let Some(map) = object.get(key).filter(|value| !value.is_null()) else {
            continue;
        };
        let map = map
            .as_object()
            .ok_or_else(|| format!("`files.{key}` maps paths to contents"))?;
        for (path, content) in map {
            let content = content
                .as_str()
                .ok_or_else(|| format!("`files.{key}` maps paths to contents"))?;
            let bytes = if decode {
                crate::driver::base64_decode(content)
                    .ok_or_else(|| format!("`files.{key}.{path}` is not base64"))?
            } else {
                content.as_bytes().to_vec()
            };
            if writes.iter().any(|(kept, _, _)| kept == path) {
                return Err(format!("`{path}` is written twice"));
            }
            writes.push((path.clone(), bytes, executable.contains(&path.as_str())));
        }
    }
    // A mode change rides on a write: naming a file the answer does not write is a
    // mistake, not a request to change one already there.
    if let Some(path) = executable
        .iter()
        .find(|path| !writes.iter().any(|(written, _, _)| written == *path))
    {
        return Err(format!(
            "`files.executable` names `{path}`, which the answer does not write"
        ));
    }
    let deletes = match object.get("delete") {
        None | Some(Value::Null) => vec![],
        Some(Value::Array(list)) => list
            .iter()
            .map(|path| {
                path.as_str()
                    .map(String::from)
                    .ok_or("`files.delete` lists paths")
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("`files.delete` lists paths".into()),
    };
    Ok((writes, deletes))
}

/// Where a session's app tool calls go. Implemented by the API transports (stdio and
/// HTTP), or by a Rust embedder directly.
pub trait AppTools: Send + Sync {
    /// Asks the app to answer `call` and waits until it does, `call.timeout` passes
    /// (`NoAnswer`) or `interrupt` is set (`Stopped`).
    fn call(&self, call: &AppCall<'_>, interrupt: &AtomicBool) -> AppAnswer;
}

/// What the model reads for an answer the app could not give.
pub(crate) fn unknown_text(note: &str) -> String {
    let mut text = "The app could not confirm whether this call ran: it was asked again after \
                    an interruption. Check its effect before repeating it."
        .to_string();
    if !note.trim().is_empty() {
        text.push(' ');
        text.push_str(note.trim());
    }
    text
}

/// What the model reads when nobody answered a call.
pub(crate) fn no_answer_text(timeout: Duration) -> String {
    format!(
        "No answer from the app within {} s; the call may still have run.",
        timeout.as_secs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> Value {
        json!({"name": name, "description": "Looks something up.",
            "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}}})
    }

    #[test]
    fn tools_are_checked_as_definitions_and_as_a_list() {
        let tools = tools_from_json(&json!([tool("lookup"), tool("save-note")])).unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].timeout_s, DEFAULT_TIMEOUT_S);
        assert!(!tools[0].rerun);
        assert_eq!(AppTool::from_json(&tools[0].to_json()).unwrap(), tools[0]);
        for bad in [
            "bash",
            "task",
            "spindle_x",
            "dext-model-v1",
            "has space",
            "",
        ] {
            assert!(tools_from_json(&json!([tool(bad)])).is_err(), "{bad}");
        }
        assert!(tools_from_json(&json!([tool("a"), tool("a")])).is_err());
        let mut wrong = tool("a");
        wrong["input_schema"] = json!({"type": "string"});
        assert!(tools_from_json(&json!([wrong])).is_err());
        let mut extra = tool("a");
        extra["risk"] = json!("high");
        assert!(tools_from_json(&json!([extra])).is_err());
        assert_ne!(
            tools[0].digest(),
            AppTool::from_json(&json!({"name": "lookup", "description": "Other.",
                "input_schema": {"type": "object"}}))
            .unwrap()
            .digest()
        );
    }

    #[test]
    fn answers_carry_text_failures_files_or_an_unknown_outcome() {
        let answer = AppAnswer::from_json(&json!({"content": "done",
            "files": {"write": {"notes/a.md": "hi"}, "executable": ["notes/a.md"],
                "delete": ["old"]}}))
        .unwrap();
        assert_eq!(
            answer,
            AppAnswer::Result {
                content: "done".into(),
                failed: false,
                writes: vec![("notes/a.md".into(), b"hi".to_vec(), true)],
                deletes: vec!["old".into()],
                links: vec![],
                directories: vec![],
                state: None,
                details: None,
            }
        );
        assert!(matches!(
            AppAnswer::from_json(&json!({"error": "no such note"})).unwrap(),
            AppAnswer::Result { failed: true, .. }
        ));
        assert_eq!(
            AppAnswer::from_json(&json!({"unknown": true})).unwrap(),
            AppAnswer::Unknown(String::new())
        );
        assert!(AppAnswer::from_json(&json!({})).is_err());
        assert!(AppAnswer::from_json(&json!({"content": "a", "error": "b"})).is_err());
        assert!(AppAnswer::from_json(&json!({"content": "a", "extra": 1})).is_err());
        assert!(AppAnswer::from_json(&json!({"content": "a", "failed": "yes"})).is_err());
        assert!(
            AppAnswer::from_json(&json!({"content": "a", "files": {"executable": ["run.sh"]}}))
                .is_err()
        );
    }

    #[test]
    fn enabling_tasks_with_true_restores_default_policy() {
        let mut settings = super::super::Settings::from_json(&json!({"provider":"local", "model":"fixture", "tasks":{"max_depth":1,"max_running":1,"models":["restricted"]}})).unwrap();
        apply_json(&mut settings, &json!({"tasks":true})).unwrap();
        assert!(settings.tasks);
        assert_eq!(settings.task_policy, Default::default());
    }

    #[test]
    fn app_output_limits_and_private_details_are_validated() {
        let default = AppTool::from_json(&tool("lookup")).unwrap();
        assert_eq!(default.max_output, DEFAULT_MAX_OUTPUT);
        for limit in [json!(0), json!(MAX_OUTPUT + 1), json!(-1), json!("large")] {
            let mut input = tool("lookup");
            input["max_output"] = limit;
            assert!(AppTool::from_json(&input).is_err());
        }
        let mut input = tool("lookup");
        input["max_output"] = json!(MAX_OUTPUT);
        let changed = AppTool::from_json(&input).unwrap();
        assert_ne!(default.digest(), changed.digest());
        assert!(matches!(
            AppAnswer::from_json(&json!({"content":"public","details":{"private":true}})).unwrap(),
            AppAnswer::Result {
                details: Some(_),
                ..
            }
        ));
        assert!(
            AppAnswer::from_json(&json!({"content":"public","details":"x".repeat(MAX_DETAILS)}))
                .is_err()
        );
    }

    #[test]
    fn builtin_choices_offer_what_they_say() {
        assert!(BuiltinTools::All.offers("bash"));
        assert!(BuiltinTools::Files.offers("edit_file") && !BuiltinTools::Files.offers("bash"));
        assert!(
            !BuiltinTools::None.offers("read_output") && !BuiltinTools::None.offers("read_file")
        );
        assert!(!BuiltinTools::Files.offers("show") && !BuiltinTools::Files.offers("read_output"));
        assert_eq!(BuiltinTools::parse("files"), Some(BuiltinTools::Files));
    }
}
