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
                      and queues the answer; the answer and the task's file changes come \
                      with its report, read at the end of a later turn."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "background": {"type":"boolean", "description":"Queue the answer instead of waiting; its answer and file changes come with its report later."},
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

/// The answer to a `task` call its task cannot run for (its input, the session's task
/// policy, or a model or effort it cannot make a request with), whether or not it had
/// begun: a failed result its model reads, so the turn goes on. Starting or continuing
/// the task would only meet the same refusal, so the call is never left waiting on it.
pub fn refused(why: &str) -> AppAnswer {
    AppAnswer::Result {
        content: format!("The task could not run: {}.", why.trim_end_matches('.')),
        failed: true,
        writes: vec![],
        deletes: vec![],
        links: vec![],
        directories: vec![],
        state: None,
        details: None,
    }
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
    if conflicts.is_empty() {
        return;
    }
    let file = |workspace: &Workspace, path: &str| {
        workspace
            .files
            .iter()
            .find(|(at, _, _)| at == path)
            .map(|(_, bytes, executable)| (bytes.clone(), *executable))
    };
    let settled = settle_conflicts(
        &conflicts.into_iter().collect::<Vec<_>>(),
        writes,
        deletes,
        links,
        directories,
        |path| {
            Ok::<_, std::convert::Infallible>(
                file(current, path).map(|now| (file(base, path), now)),
            )
        },
        |path| now.contains_key(path),
    );
    let Ok(settled) = settled;
    content.push('\n');
    content.push_str(&settled.note);
    *failed |= settled.unresolved;
}

/// What became of a finished task's changes to paths the session changed too since the
/// task began (`settle_conflicts`): said in the task's answer.
pub(crate) struct Settled {
    pub note: String,
    /// Some change was left out (the session's stands).
    pub unresolved: bool,
}

/// Settles the paths a finished task changed that the session changed too since the task
/// began (`conflicts`), in its changes: a text file both changed on separate lines (or
/// both only added to its end) is merged, as git merges; any other conflicting change is
/// left out, so the session's stands, and a file the task wrote over one the session has
/// is kept beside it (`side_path`), so nothing it made is lost. The task's other changes
/// go as they are, and one the session already made the same is left alone. `versions`
/// gives a file's bytes and executable bit in the session now (when it is a file there)
/// and at the task's base (when it was one then); `taken`, whether the session has a
/// path now.
pub(crate) fn settle_conflicts<E>(
    conflicts: &[String],
    writes: &mut Vec<(String, Vec<u8>, bool)>,
    deletes: &mut Vec<String>,
    links: &mut Vec<crate::workspace::Link>,
    directories: &mut Vec<String>,
    mut versions: impl FnMut(&str) -> std::result::Result<Option<VersionPair>, E>,
    taken: impl Fn(&str) -> bool,
) -> std::result::Result<Settled, E> {
    let mut merged = Vec::new();
    let mut same = std::collections::BTreeSet::new();
    let mut unresolved = std::collections::BTreeSet::new();
    for path in conflicts {
        if let Some(write) = writes.iter_mut().find(|(at, _, _)| at == path)
            && let Some((base, (now, now_x))) = versions(path)?
        {
            // Both made it the same: nothing to settle.
            if now == write.1 && now_x == write.2 {
                same.insert(path.clone());
                continue;
            }
            if let Some((base, base_x)) = base
                && let Some(bytes) = merge_text(&base, &now, &write.1)
            {
                // The executable bit as the task left it if it changed it, else the session's.
                write.2 = if write.2 == base_x { now_x } else { write.2 };
                write.1 = bytes;
                merged.push(path.as_str());
                continue;
            }
        }
        unresolved.insert(path.clone());
    }
    writes.retain(|(path, _, _)| !same.contains(path));
    let mut said = Vec::new();
    let mut beside = Vec::new();
    writes.retain(|(path, bytes, executable)| {
        if !unresolved.contains(path) {
            return true;
        }
        let side = side_path(path, bytes);
        if taken(path) && !taken(&side) {
            said.push(format!(
                "{path} (the session's copy is kept; the task's is {side})"
            ));
            beside.push((side, bytes.clone(), *executable));
        } else {
            said.push(format!("{path} (kept as the session has it)"));
        }
        false
    });
    let rest = |path: &String| !unresolved.contains(path);
    for path in deletes.iter().filter(|path| !rest(path)) {
        said.push(format!("{path} (kept as the session has it)"));
    }
    deletes.retain(rest);
    for link in links.iter().filter(|link| !rest(&link.path)) {
        said.push(format!("{} (kept as the session has it)", link.path));
    }
    links.retain(|link| rest(&link.path));
    for path in directories.iter().filter(|path| !rest(path)) {
        said.push(format!("{path} (kept as the session has it)"));
    }
    directories.retain(rest);
    let others =
        !(writes.is_empty() && deletes.is_empty() && links.is_empty() && directories.is_empty());
    writes.extend(beside);
    let mut note = Vec::new();
    if !merged.is_empty() {
        note.push(format!(
            "[task files merged with what the session changed in them since the task began: {}]",
            merged.join(", ")
        ));
    }
    if !said.is_empty() {
        said.sort();
        note.push(format!(
            "[task file conflict, changed here too since the task began: {}; {}]",
            said.join(", "),
            if others {
                "the task's other changes were adopted"
            } else {
                "it changed nothing else"
            }
        ));
    }
    Ok(Settled {
        note: note.join("\n"),
        unresolved: !unresolved.is_empty(),
    })
}

/// A file's bytes and executable bit at a task's base (if it was a file there), then in
/// the session now.
pub(crate) type VersionPair = (Option<(Vec<u8>, bool)>, (Vec<u8>, bool));

/// Where a task's copy of a file it could not merge is kept: beside it, named by its
/// bytes (`notes/NOTES.md` -> `notes/NOTES.task-1a2b3c4d.md`).
fn side_path(path: &str, bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let tag: String = Sha256::digest(bytes)[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let (dir, name) = path
        .rsplit_once('/')
        .map_or(("", path), |(dir, name)| (dir, name));
    let side = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => format!("{stem}.task-{tag}.{extension}"),
        _ => format!("{name}.task-{tag}"),
    };
    if dir.is_empty() {
        side
    } else {
        format!("{dir}/{side}")
    }
}

/// A three-way merge of a text file by lines: the session's change (`ours`, from `base`)
/// and the task's (`theirs`), each the lines between those it shares with `base` at its
/// start and its end. `Some` when the two lie apart, with a line of `base` between them,
/// or both only added lines at its end (the session's first); `None` for any other text,
/// for bytes that are not text, or for a file over 1 MiB.
pub(crate) fn merge_text(base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    const MOST: usize = 1 << 20;
    if [base, ours, theirs]
        .iter()
        .any(|text| text.len() > MOST || text.contains(&0) || std::str::from_utf8(text).is_err())
    {
        return None;
    }
    if ours == theirs || theirs == base {
        return Some(ours.to_vec());
    }
    if ours == base {
        return Some(theirs.to_vec());
    }
    let lines = |text: &[u8]| -> Vec<Vec<u8>> {
        text.split_inclusive(|byte| *byte == b'\n')
            .map(<[u8]>::to_vec)
            .collect()
    };
    let (b, o, t) = (lines(base), lines(ours), lines(theirs));
    // (where it starts in both, where it ends in base, where it ends in the side)
    let region = |side: &[Vec<u8>]| {
        let start = b.iter().zip(side).take_while(|(l, r)| l == r).count();
        let end = b[start..]
            .iter()
            .rev()
            .zip(side[start..].iter().rev())
            .take_while(|(l, r)| l == r)
            .count();
        (start, b.len() - end, side.len() - end)
    };
    let (os, oe, ox) = region(&o);
    let (ts, te, tx) = region(&t);
    let mut out: Vec<u8> = Vec::with_capacity(ours.len() + theirs.len());
    let mut put = |part: &[Vec<u8>]| part.iter().for_each(|line| out.extend_from_slice(line));
    if os == b.len() && ts == b.len() {
        // Both only added to its end.
        put(&o);
        put(&t[ts..tx]);
    } else if oe < ts {
        put(&b[..os]);
        put(&o[os..ox]);
        put(&b[oe..ts]);
        put(&t[ts..tx]);
        put(&b[te..]);
    } else if te < os {
        put(&b[..ts]);
        put(&t[ts..tx]);
        put(&b[te..os]);
        put(&o[os..ox]);
        put(&b[oe..]);
    } else {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod merge_tests {
    use super::{merge_text, settle_conflicts};

    fn merge(base: &str, ours: &str, theirs: &str) -> Option<String> {
        merge_text(base.as_bytes(), ours.as_bytes(), theirs.as_bytes())
            .map(|bytes| String::from_utf8(bytes).unwrap())
    }

    #[test]
    fn text_both_changed_merges_only_where_the_changes_lie_apart() {
        let base = "one\ntwo\nthree\n";
        // The session edits the top, the task adds at the end: both kept, as git would.
        assert_eq!(
            merge(base, "ONE\ntwo\nthree\n", "one\ntwo\nthree\nfour\n").as_deref(),
            Some("ONE\ntwo\nthree\nfour\n")
        );
        // And the other way round.
        assert_eq!(
            merge(base, "one\ntwo\nthree\nfour\n", "ONE\ntwo\nthree\n").as_deref(),
            Some("ONE\ntwo\nthree\nfour\n")
        );
        // Both only added to its end: the session's lines first, then the task's.
        assert_eq!(
            merge(base, "one\ntwo\nthree\nours\n", "one\ntwo\nthree\ntheirs\n").as_deref(),
            Some("one\ntwo\nthree\nours\ntheirs\n")
        );
        // The same change on both sides is that change.
        assert_eq!(
            merge(base, "one\nTWO\nthree\n", "one\nTWO\nthree\n").as_deref(),
            Some("one\nTWO\nthree\n")
        );
        // One line, changed two ways; changes with no line of the base between them.
        assert_eq!(merge(base, "one\nTWO\nthree\n", "one\nzwei\nthree\n"), None);
        assert_eq!(merge(base, "ONE\ntwo\nthree\n", "one\nTWO\nthree\n"), None);
        // A last line with no newline that both extend is changed by both.
        assert_eq!(merge("a\nb", "a\nb\nc\n", "a\nb\nd\n"), None);
        // Not text, or too large to merge here.
        assert_eq!(merge_text(b"a\0\n", b"b\0\n", b"a\0\nc\n"), None);
        let big = "x\n".repeat(600_000);
        assert_eq!(
            merge(&big, &format!("y\n{big}"), &format!("{big}z\n")),
            None
        );
    }

    #[test]
    fn conflicts_merge_or_stay_the_sessions_with_the_tasks_copy_beside_them() {
        let link = |path: &str| crate::workspace::Link {
            path: path.into(),
            target: "t".into(),
        };
        let mut writes = vec![
            ("notes.md".to_string(), b"one\ntwo\nmine\n".to_vec(), false),
            ("report.md".to_string(), b"the task's\n".to_vec(), false),
            ("fresh.txt".to_string(), b"new\n".to_vec(), false),
        ];
        let mut deletes = vec!["old.txt".to_string()];
        let mut links = vec![link("both-link")];
        let mut directories = vec!["made".to_string()];
        let conflicts: Vec<String> = ["both-link", "notes.md", "old.txt", "report.md"]
            .map(String::from)
            .to_vec();
        let settled = settle_conflicts(
            &conflicts,
            &mut writes,
            &mut deletes,
            &mut links,
            &mut directories,
            |path| {
                Ok::<_, ()>(match path {
                    "notes.md" => Some((
                        Some((b"one\ntwo\n".to_vec(), false)),
                        (b"ONE\ntwo\n".to_vec(), false),
                    )),
                    "report.md" => Some((
                        Some((b"draft\n".to_vec(), false)),
                        (b"the session's\n".to_vec(), false),
                    )),
                    _ => None,
                })
            },
            |path| ["notes.md", "report.md", "old.txt", "both-link"].contains(&path),
        )
        .unwrap();
        assert!(settled.unresolved);
        let written: Vec<_> = writes
            .iter()
            .map(|(path, bytes, _)| (path.as_str(), String::from_utf8_lossy(bytes).into_owned()))
            .collect();
        assert_eq!(written[0], ("notes.md", "ONE\ntwo\nmine\n".to_string()));
        assert_eq!(written[1], ("fresh.txt", "new\n".to_string()));
        assert_eq!(written.len(), 3, "{written:?}");
        assert!(
            written[2].0.starts_with("report.task-") && written[2].0.ends_with(".md"),
            "{written:?}"
        );
        assert_eq!(written[2].1, "the task's\n");
        assert!(
            deletes.is_empty() && links.is_empty(),
            "the session's stand"
        );
        assert_eq!(directories, ["made"]);
        assert!(settled.note.contains("[task files merged with what the session changed in them since the task began: notes.md]"), "{}", settled.note);
        assert!(
            settled.note.contains(&format!(
                "report.md (the session's copy is kept; the task's is {})",
                written[2].0
            )),
            "{}",
            settled.note
        );
        assert!(
            settled
                .note
                .contains("old.txt (kept as the session has it)")
                && settled
                    .note
                    .contains("both-link (kept as the session has it)"),
            "{}",
            settled.note
        );
        assert!(
            settled
                .note
                .ends_with("the task's other changes were adopted]"),
            "{}",
            settled.note
        );

        // An executable bit only one side changed is as that side made it.
        let mut writes = vec![("run.sh".to_string(), b"a\nb\nc\n".to_vec(), true)];
        let settled = settle_conflicts(
            &["run.sh".to_string()],
            &mut writes,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            |_| {
                Ok::<_, ()>(Some((
                    Some((b"a\nb\n".to_vec(), false)),
                    (b"A\nb\n".to_vec(), false),
                )))
            },
            |_| true,
        )
        .unwrap();
        assert!(!settled.unresolved);
        assert_eq!(
            writes,
            [("run.sh".to_string(), b"A\nb\nc\n".to_vec(), true)]
        );
        // Both made the same new file: left alone, nothing said.
        let mut writes = vec![("same.txt".to_string(), b"made\n".to_vec(), false)];
        let settled = settle_conflicts(
            &["same.txt".to_string()],
            &mut writes,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            |_| Ok::<_, ()>(Some((None, (b"made\n".to_vec(), false)))),
            |_| true,
        )
        .unwrap();
        assert!(
            writes.is_empty() && settled.note.is_empty() && !settled.unresolved,
            "{writes:?} {}",
            settled.note
        );
        // Only conflicting changes: nothing else to adopt, said so.
        let mut writes = vec![("x.txt".to_string(), b"task\n".to_vec(), false)];
        let settled = settle_conflicts(
            &["x.txt".to_string()],
            &mut writes,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            |_| Ok::<_, ()>(None),
            |path| path == "x.txt",
        )
        .unwrap();
        assert!(
            settled.note.ends_with("it changed nothing else]"),
            "{}",
            settled.note
        );
        assert_eq!(writes.len(), 1, "only the copy beside it: {writes:?}");
    }
}
