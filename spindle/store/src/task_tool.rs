//! Spindle's `task` tool, shared by every host: its definition, the input it takes
//! and how a finished task's file changes merge into its parent's workspace. Running
//! the child session is the host's (native: `agent::tasks`; embedded: its own loop).
use super::app_tools::{AppAnswer, AppTool};
use serde_json::{Value, json};

/// How deep tasks may nest: a task's task is the deepest.
pub const MAX_DEPTH: usize = 2;
/// The tool's name.
pub const NAME: &str = "task";

/// The `task` tool as the model sees it (its digest labels its calls' attempts).
pub fn definition() -> AppTool {
    AppTool {
        name: NAME.into(),
        description: "Start a task: another agent that works on its own copy of this \
                      workspace with the same tools until it is done, then answers. Give \
                      it a self-contained prompt (it sees none of this conversation). Its \
                      file changes are kept unless files is \"discard\". Use it for work \
                      that can be done apart, such as research or a separate change; calls \
                      in one step run concurrently over the same base and merge in call order; \
                      conflicts are reported, never overwritten. background returns immediately \
                      and queues the answer; its files remain in the child workspace."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "background": {"type":"boolean", "description":"Queue the answer instead of waiting; file changes stay in the child."},
                "prompt": {"type": "string", "description": "What the task should do, complete in itself."},
                "model": {"type": "string", "description": "Another model of the same provider (default: this session's)."},
                "effort": {"type": "string", "enum": ["default", "off", "low", "medium", "high", "xhigh", "max"]},
                "files": {"type": "string", "enum": ["keep", "discard"], "description": "Keep the task's file changes (default) or discard them."}
            },
            "required": ["prompt"]
        }),
        rerun: false,
        timeout_s: super::app_tools::MAX_TIMEOUT_S,
        max_output: super::app_tools::DEFAULT_MAX_OUTPUT,
    }
}

/// The part of a `task` input that decides what the task does: a task's identity,
/// which a later start under the same id must repeat.
pub fn identity(input: &Value) -> Value {
    let mut kept = serde_json::Map::new();
    for key in [
        "prompt",
        "model",
        "effort",
        "files",
        "background",
        "settings",
    ] {
        if let Some(value) = input.get(key).filter(|value| !value.is_null()) {
            kept.insert(key.into(), value.clone());
        }
    }
    Value::Object(kept)
}

/// Why a task's model is refused by the session's `tasks.models` (none: any model).
pub fn model_refusal(models: &[String], default: &str, input: &Value) -> Option<String> {
    let model = input["model"]
        .as_str()
        .or(input["settings"]["model"].as_str())
        .unwrap_or(default);
    (!models.is_empty() && !models.iter().any(|allowed| allowed == model))
        .then(|| format!("model `{model}` is not permitted by tasks.models"))
}

/// Why a `task` call cannot run, or None.
pub fn refusal(input: &Value) -> Option<String> {
    if !input.is_object() {
        return Some("task input must be an object".into());
    }
    if input
        .get("background")
        .is_some_and(|value| !value.is_boolean())
    {
        return Some("background must be a boolean".into());
    }
    if let Some(settings) = input.get("settings") {
        let Some(settings) = settings.as_object() else {
            return Some("task settings must be an object".into());
        };
        if settings
            .keys()
            .any(|key| !["model", "effort", "max_usd"].contains(&key.as_str()))
        {
            return Some("task settings may narrow model, effort and max_usd only".into());
        }
        if settings.get("model").is_some_and(|value| {
            !value
                .as_str()
                .is_some_and(|value| !value.trim().is_empty() && value.len() <= 256)
        }) {
            return Some("task settings.model must be a model name".into());
        }
        if settings
            .get("effort")
            .is_some_and(|value| value.as_str().and_then(super::Effort::parse).is_none())
        {
            return Some("invalid task settings.effort".into());
        }
        if settings.get("max_usd").is_some_and(|value| {
            !value
                .as_f64()
                .is_some_and(|value| value.is_finite() && value >= 0.0)
        }) {
            return Some("task settings.max_usd must be non-negative".into());
        }
    }
    if input.get("model").is_some_and(|value| {
        !value
            .as_str()
            .is_some_and(|value| !value.trim().is_empty() && value.len() <= 256)
    }) {
        return Some("task model must be a model name".into());
    }
    let prompt = input.get("prompt").and_then(Value::as_str);
    if prompt.is_none_or(|text| text.trim().is_empty() || text.len() > 64 * 1024) {
        return Some("`task` needs a `prompt` of 1-65536 bytes".into());
    }
    if input
        .get("files")
        .is_some_and(|files| !matches!(files.as_str(), Some("keep" | "discard")))
    {
        return Some("`files` is \"keep\" or \"discard\"".into());
    }
    if input
        .get("effort")
        .is_some_and(|effort| effort.as_str().and_then(super::Effort::parse).is_none())
    {
        return Some("`effort` must be default, off, low, medium, high, xhigh or max".into());
    }
    None
}

#[derive(Clone, Debug, Default)]
pub struct Workspace {
    pub files: Vec<super::File>,
    pub links: Vec<crate::workspace::Link>,
    pub directories: Vec<String>,
}

#[cfg_attr(not(any(feature = "agent", feature = "embedded")), allow(dead_code))]
pub(crate) fn refuse_conflicts(base: &Workspace, current: &Workspace, answer: &mut AppAnswer) {
    #[derive(PartialEq)]
    enum Node<'a> {
        File(&'a [u8], bool),
        Link(&'a str),
        Directory,
    }
    fn nodes(workspace: &Workspace) -> std::collections::BTreeMap<&str, Node<'_>> {
        workspace
            .files
            .iter()
            .map(|(path, bytes, executable)| (path.as_str(), Node::File(bytes, *executable)))
            .chain(
                workspace
                    .links
                    .iter()
                    .map(|link| (link.path.as_str(), Node::Link(&link.target))),
            )
            .chain(
                workspace
                    .directories
                    .iter()
                    .map(|path| (path.as_str(), Node::Directory)),
            )
            .collect()
    }
    let AppAnswer::Result {
        content,
        failed,
        writes,
        deletes,
        links,
        directories,
        ..
    } = answer
    else {
        return;
    };
    let prior = nodes(base);
    let now = nodes(current);
    let changed: std::collections::BTreeSet<_> = prior
        .keys()
        .chain(now.keys())
        .filter(|path| prior.get(**path) != now.get(**path))
        .copied()
        .collect();
    let overlap = |a: &str, b: &str| {
        a == b
            || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
            || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
    };
    let conflicts: std::collections::BTreeSet<_> = writes
        .iter()
        .map(|(path, _, _)| path.as_str())
        .chain(deletes.iter().map(String::as_str))
        .chain(links.iter().map(|link| link.path.as_str()))
        .chain(directories.iter().map(String::as_str))
        .filter(|path| changed.iter().any(|changed| overlap(path, changed)))
        .map(String::from)
        .collect();
    if !conflicts.is_empty() {
        content.push_str(&format!(
            "\n[task file conflict: {}; none of this task's changes were adopted]",
            conflicts.into_iter().collect::<Vec<_>>().join(", ")
        ));
        *failed = true;
        writes.clear();
        deletes.clear();
        links.clear();
        directories.clear();
    }
}
