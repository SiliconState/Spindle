use super::app_tools::{AppTool, BuiltinTools};
use super::{Result, app_tools};
use crate::rewind::RewindTargetKind;
use crate::workspace::Link;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
/// One observable thing a session did.
#[non_exhaustive]
pub enum Event<'a> {
    /// A prompt was committed and starts the next turn.
    Prompt {
        text: &'a str,
    },
    /// Older history was compacted; sizes are checkpoint bytes.
    Compacted {
        before: usize,
        after: usize,
    },
    /// A model request is about to be sent.
    ModelStep {
        provider: &'a str,
        model: &'a str,
        step: u32,
    },
    /// A streamed model text preview delta.
    ModelDelta {
        text: &'a str,
    },
    /// The provider withdrew the text preview it streamed.
    ModelTextWithdrawn,
    /// The recorded model text of a step; empty text ends streamed text with a newline.
    ModelText {
        text: &'a str,
    },
    /// Token usage of a recorded model response.
    Usage {
        input: u64,
        output: u64,
    },
    /// What a recorded call (or a script's nested calls, or a compaction summary) cost
    /// at its record-time price, the turn so far and the thread in total; `billing` is
    /// priced, subscription, unpriced or mixed.
    Cost {
        usd: f64,
        turn: f64,
        total: f64,
        billing: &'a str,
    },
    /// A recorded model response was adopted without sending.
    ResponseReused,
    /// A tool call is shown before it runs.
    ToolCall {
        id: &'a str,
        name: &'a str,
        input: &'a Value,
    },
    /// The repeat guard skipped a tool call.
    ToolSkipped,
    /// A tool call was refused before it ran.
    ToolRefused {
        reason: &'a str,
    },
    /// A recorded tool result was adopted without running.
    ToolReused,
    /// A tool call interrupted by a stop was recorded as not completed.
    ToolInterrupted,
    /// Commands in this session run directly on the host, without isolation, and why.
    /// Sent once per session, before the first such command.
    Unisolated {
        reason: &'a str,
    },
    /// The cards a `show` call put in the chat (`[{path, kind, title, size,
    /// revision}]`), sent just before its `ToolResult`.
    Display {
        items: &'a Value,
    },
    /// The recorded result of a tool call.
    ToolResult {
        content: &'a str,
        failed: bool,
    },
    TaskReported {
        task: &'a str,
        text: &'a str,
    },
    Task {
        task: &'a str,
        event: &'a Value,
    },
    AppToolDetails {
        call: &'a str,
        details: &'a Value,
    },
    /// A nested code-mode call (model, tool or discovery) made by a running script.
    Nested {
        label: &'a str,
    },
    /// A raw chunk of live process output.
    Output {
        stream: u8,
        bytes: &'a [u8],
    },
    /// Live process output for the current call has ended.
    OutputEnd,
    /// The turn stopped.
    Stopped {
        reason: &'a str,
        steps: u32,
        revision: u64,
        files: usize,
        complete: bool,
    },
    /// A rewind completed.
    Rewound {
        target: u64,
        revision: u64,
        kind: RewindTargetKind,
        abandoned: usize,
    },
    /// The app state changed at `revision`: these top-level keys differ from before.
    State {
        revision: u64,
        changed: &'a [String],
    },
    /// A state change sent while the turn ran could not be kept (and why).
    StateRefused {
        reason: &'a str,
    },
    /// App tool calls (their `AppCall::call` ids) whose answers a rewind took back:
    /// what they did inside the session is undone, anything else is the app's to undo.
    AppCallsRewound {
        calls: &'a [String],
    },
    /// A retry starts from an earlier target.
    Retrying {
        target: u64,
        head: u64,
    },
    /// The first request is retried in a fresh store; the old store is kept aside.
    RetryingFirst {
        path: &'a Path,
    },
    /// The last turn already finished, so there is nothing to continue.
    NothingToContinue,
    /// Continue after the step limit granted the turn a fresh step budget.
    Extended {
        steps: u32,
    },
    /// A message sent while the turn ran was given to the model.
    Steered {
        text: &'a str,
    },
    /// The reply being written was cut so a message the user asked to send now is read
    /// first; the request is prepared again with it (the cut reply may still be billed).
    ReplyCut,
    /// A message the user asked to send now could not cut the reply being written (no
    /// earlier point to prepare the request from); it is read once the reply ends.
    ReplyNotCut,
    /// A session pack showed a view (its latest per pack is kept with its state).
    PackView {
        pack: &'a str,
        title: &'a str,
        markdown: &'a str,
    },
    /// Settings the model queued (`settings` tool) were applied, or refused, as the
    /// turn ended; `text` says which.
    SettingsApplied {
        text: &'a str,
    },
    /// The provider refused a request before generating anything; it is sent again
    /// after `seconds` (and after compacting the history when it was too long).
    ProviderRetry {
        status: u16,
        seconds: u64,
        attempt: u32,
        compacting: bool,
    },
}

/// Session configuration, stored as metadata (native hosts also mirror `session.json`).
#[derive(Clone)]
#[non_exhaustive]
pub struct Settings {
    /// Provider profile id from Dext's provider catalog.
    pub provider: String,
    /// Model id sent in requests.
    pub model: String,
    /// Whether commands get internet access.
    pub network: bool,
    /// Whether commands get the host Rust toolchain.
    pub toolchain: bool,
    /// Host directories mounted read-only beside /workspace in toolchain runs.
    pub sources: Vec<PathBuf>,
    /// Whether commands must, may or must not run in the sandbox.
    pub isolation: Isolation,
    /// Requested model reasoning; `Default` keeps Spindle's per-provider default.
    pub effort: Effort,
    /// How older history is compacted when it outgrows the model's window.
    pub compaction: Compaction,
    /// Dollar budget for the thread (None: unlimited). A model request whose worst case
    /// would take the thread's spend past it is not sent; nested calls count too.
    pub max_usd: Option<f64>,
    /// The workspace is Spindle's own source (a self-edit thread of `spindle serve`):
    /// the model is told how its changes reach the host (`self_edit_guidance`).
    pub self_edit: bool,
    /// Tools the embedding app supplies, offered after Spindle's own.
    pub tools: Vec<AppTool>,
    /// Which of Spindle's own tools are offered.
    pub builtin_tools: BuiltinTools,
    /// The app's instructions, added to the system prompt.
    pub instructions: Option<String>,
    /// Send again, on its own, a model request whose outcome a crash left unknown (it
    /// may be billed twice); otherwise `retry` is the user's to run.
    pub resend_unknown: bool,
    /// The model may start tasks (`agent::tasks`): other sessions that work on a copy of
    /// the workspace and answer.
    pub tasks: bool,
    pub task_policy: TaskPolicy,
    /// Who runs `bash` for a host without a sandbox (the embedded package); `None`
    /// leaves it out of the offer there. Native hosts run it in their own sandbox.
    pub runner: Runner,
}

/// Who runs commands for an embedded session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum Runner {
    /// No one: `bash` is not offered (the file tools still are).
    #[default]
    None,
    /// A Spindle host runs each command over a copy of the workspace and returns the
    /// changed files.
    Remote,
    /// The app runs each command (its own sandbox, or none).
    App,
}

impl Runner {
    /// The runner named `none`, `remote` or `app`.
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "none" => Self::None,
            "remote" => Self::Remote,
            "app" => Self::App,
            _ => return None,
        })
    }
    /// The runner's name, as `parse` takes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Remote => "remote",
            Self::App => "app",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskPolicy {
    pub max_depth: usize,
    pub max_running: usize,
    pub models: Vec<String>,
}
impl Default for TaskPolicy {
    fn default() -> Self {
        Self {
            max_depth: 2,
            max_running: 4,
            models: vec![],
        }
    }
}
impl TaskPolicy {
    pub fn from_json(value: &Value) -> Result<Self> {
        if !value.is_object() {
            return Err("tasks policy must be an object".into());
        }
        if let Some(key) = value
            .as_object()
            .unwrap()
            .keys()
            .find(|key| !["max_depth", "max_running", "models"].contains(&key.as_str()))
        {
            return Err(format!("unknown tasks policy field `{key}`"));
        }
        let number = |key: &str, default, max| -> Result<usize> {
            match value.get(key) {
                None => Ok(default),
                Some(value) => value
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .filter(|value| (1..=max).contains(value))
                    .ok_or_else(|| format!("tasks.{key} must be 1-{max}")),
            }
        };
        let models = match value.get("models") {
            None => vec![],
            Some(Value::Array(list)) if list.len() <= 64 => list
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .filter(|text| !text.trim().is_empty() && text.len() <= 256)
                        .map(String::from)
                        .ok_or("tasks.models contains model names".into())
                })
                .collect::<Result<Vec<_>>>()?,
            _ => return Err("tasks.models must list at most 64 model names".into()),
        };
        Ok(Self {
            max_depth: number("max_depth", 2, 16)?,
            max_running: number("max_running", 4, 16)?,
            models,
        })
    }
    pub fn to_json(&self) -> Value {
        json!({"max_depth":self.max_depth,"max_running":self.max_running,"models":self.models})
    }
}

/// How older history is compacted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum Compaction {
    /// The session's model writes a summary of the older part (one extra request per
    /// compaction); the deterministic digest stands in if that request fails.
    #[default]
    Model,
    /// A deterministic digest of prompts, replies and tool calls; no extra request.
    Digest,
}

impl Compaction {
    /// The mode named `model` or `digest`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "model" => Some(Self::Model),
            "digest" => Some(Self::Digest),
            _ => None,
        }
    }
    /// The mode's name, as `parse` takes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Digest => "digest",
        }
    }
}

/// Model reasoning effort. `Default` adds no reasoning fields (GLM and DeepSeek get
/// thinking turned off); the other levels add a thinking allowance on top of the
/// answer cap.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum Effort {
    /// Spindle's per-provider default.
    #[default]
    Default,
    /// Reasoning turned off where the provider allows it.
    Off,
    /// 2K reasoning tokens on top of the answer cap.
    Low,
    /// 6K.
    Medium,
    /// 16K.
    High,
    /// 32K.
    XHigh,
    /// 64K.
    Max,
}

impl Effort {
    /// Every level, in increasing order after `Default`.
    pub const ALL: [Self; 7] = [
        Self::Default,
        Self::Off,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];
    /// The level named as `as_str` names it.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.as_str() == text)
    }
    /// The level's name: `default`, `off`, `low`, `medium`, `high`, `xhigh` or `max`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
    /// Output tokens a level may spend on reasoning on top of the answer cap.
    pub fn reasoning_allowance(self) -> u32 {
        match self {
            Self::Default | Self::Off => 0,
            Self::Low => 2048,
            Self::Medium => 6144,
            Self::High => 16384,
            Self::XHigh => 32768,
            Self::Max => 65536,
        }
    }
}

/// How commands are isolated. Isolation needs Linux namespaces (bubblewrap); hosts
/// without them, such as unprivileged containers, can still run commands directly.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum Isolation {
    /// Sandbox when this host supports it; otherwise run directly and say so.
    #[default]
    Auto,
    /// Refuse to run commands without the sandbox.
    Required,
    /// Never sandbox: the embedding host provides isolation, or none is wanted.
    Off,
}

impl Isolation {
    /// The mode named `auto`, `required` or `off`.
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "auto" => Self::Auto,
            "required" => Self::Required,
            "off" => Self::Off,
            _ => return None,
        })
    }
    /// The mode's name, as `parse` takes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Required => "required",
            Self::Off => "off",
        }
    }
}
/// A file to put in a workspace: path, bytes, executable.
pub type File = (String, Vec<u8>, bool);

/// What a new turn's workspace starts from.
pub enum Start {
    /// The workspace as the last turn left it.
    Same,
    /// A fresh workspace of these files, links and empty directories (an import).
    Import(Vec<File>, Vec<Link>, Vec<String>),
    /// The current workspace with these files added or replaced (attachments).
    Add(Vec<File>),
    /// A root this store already holds, as it is (a retried turn's attachments).
    Root(String),
}

/// What a turn's first step does to the app state.
pub enum StartState {
    /// Applies the app's merge patch (`Session::state_with_next_prompt`).
    Patch(Value),
    /// Sets exactly this state: a retry that restarts a turn keeps the state it had.
    Exact(Vec<u8>),
}

impl Settings {
    pub fn from_json(value: &Value) -> Result<Self> {
        if !value.is_object() {
            return Err("stored settings must be an object".into());
        }
        for key in ["network", "toolchain", "self_edit", "resend_unknown"] {
            if !value[key].is_null() && !value[key].is_boolean() {
                return Err(format!("stored settings: `{key}` must be a boolean"));
            }
        }
        for (key, valid) in [
            (
                "isolation",
                value["isolation"]
                    .as_str()
                    .and_then(Isolation::parse)
                    .is_some(),
            ),
            (
                "effort",
                value["effort"].as_str().and_then(Effort::parse).is_some(),
            ),
            (
                "compaction",
                value["compaction"]
                    .as_str()
                    .and_then(Compaction::parse)
                    .is_some(),
            ),
        ] {
            if !value[key].is_null() && !valid {
                return Err(format!("stored settings: invalid `{key}`"));
            }
        }
        if !value["max_usd"].is_null()
            && !value["max_usd"]
                .as_f64()
                .is_some_and(|max| max.is_finite() && max >= 0.0)
        {
            return Err("stored settings: `max_usd` must be a non-negative number or null".into());
        }
        if !value["sources"].is_null()
            && !value["sources"]
                .as_array()
                .is_some_and(|sources| sources.iter().all(Value::is_string))
        {
            return Err("stored settings: `sources` must be paths".into());
        }
        if !value["instructions"].is_null() && !value["instructions"].is_string() {
            return Err("stored settings: `instructions` must be text".into());
        }
        let settings = Self {
            provider: value["provider"].as_str().unwrap_or_default().into(),
            model: value["model"].as_str().unwrap_or_default().into(),
            network: value["network"].as_bool().unwrap_or(false),
            toolchain: value["toolchain"].as_bool().unwrap_or(false),
            sources: value["sources"]
                .as_array()
                .map(|sources| {
                    sources
                        .iter()
                        .filter_map(|source| source.as_str().map(PathBuf::from))
                        .collect()
                })
                .unwrap_or_default(),
            isolation: value["isolation"]
                .as_str()
                .and_then(Isolation::parse)
                .unwrap_or_default(),
            effort: value["effort"]
                .as_str()
                .and_then(Effort::parse)
                .unwrap_or_default(),
            compaction: value["compaction"]
                .as_str()
                .and_then(Compaction::parse)
                .unwrap_or_default(),
            max_usd: value["max_usd"]
                .as_f64()
                .filter(|max| max.is_finite() && *max >= 0.0),
            self_edit: value["self_edit"].as_bool().unwrap_or(false),
            // Saved tools were checked when they were set; a list that no longer reads
            // as one is refused, not silently dropped.
            tools: app_tools::tools_from_json(&value["tools"])
                .map_err(|error| format!("session.json: {error}"))?,
            // A choice that no longer reads as one is refused, never widened to all.
            builtin_tools: match &value["builtin_tools"] {
                Value::Null => BuiltinTools::All,
                choice => choice
                    .as_str()
                    .and_then(BuiltinTools::parse)
                    .ok_or("session.json: `builtin_tools` must be all, files or none")?,
            },
            instructions: value["instructions"]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(String::from),
            resend_unknown: value["resend_unknown"].as_bool().unwrap_or(false),
            tasks: !value["tasks"].is_null() && value["tasks"] != false,
            task_policy: match &value["tasks"] {
                Value::Null | Value::Bool(_) => TaskPolicy::default(),
                value => TaskPolicy::from_json(value)?,
            },
            // A runner that no longer reads as one is refused, never dropped.
            runner: match &value["runner"] {
                Value::Null => Runner::None,
                choice => choice
                    .as_str()
                    .and_then(Runner::parse)
                    .ok_or("session.json: `runner` must be none, remote or app")?,
            },
        };
        settings.check_app()?;
        Ok(settings)
    }
    pub fn to_json(&self) -> Result<Value> {
        self.check_app()?;
        let mut value = json!({
            "provider": self.provider,
            "model": self.model,
            "network": self.network,
            "toolchain": self.toolchain,
            "sources": self.sources,
            "isolation": self.isolation.as_str(),
            "effort": self.effort.as_str(),
            "compaction": self.compaction.as_str(),
            "max_usd": self.max_usd,
            "self_edit": self.self_edit,
        });
        // App settings are written only when set, so a session without them keeps the
        // same file an older build reads and writes.
        if !self.tools.is_empty() {
            value["tools"] = self.tools.iter().map(AppTool::to_json).collect();
        }
        if self.builtin_tools != BuiltinTools::All {
            value["builtin_tools"] = json!(self.builtin_tools.as_str());
        }
        if let Some(instructions) = &self.instructions {
            value["instructions"] = json!(instructions);
        }
        if self.resend_unknown {
            value["resend_unknown"] = json!(true);
        }
        if self.tasks {
            value["tasks"] = self.task_policy.to_json();
        }
        if self.runner != Runner::None {
            value["runner"] = json!(self.runner.as_str());
        }
        Ok(value)
    }
}

impl Settings {
    /// Whether the app settings can be kept: valid tools with unique names and
    /// instructions of bounded size.
    pub fn check_app(&self) -> Result<()> {
        TaskPolicy::from_json(&self.task_policy.to_json())?;
        let tools = Value::Array(self.tools.iter().map(AppTool::to_json).collect());
        app_tools::tools_from_json(&tools)?;
        if let Some(instructions) = &self.instructions
            && instructions.len() > app_tools::MAX_INSTRUCTIONS
        {
            return Err(format!(
                "instructions are over {} KiB",
                app_tools::MAX_INSTRUCTIONS / 1024
            ));
        }
        Ok(())
    }
}
