//! Reads of committed state in the API's stable forms (docs/API.md): the head, the
//! conversation, the app state, the workspace, one file and the rewind targets. Both
//! hosts answer them from a `ReadView` (the native one over a second connection, the
//! embedded one over the store's own), so the same store reads the same everywhere.
use crate::driver::{is_prompt, stop_text};
use crate::protocol::{message_json, phase_name, rewind_kind, stop_code};
use crate::view::ReadView;
use dext_core::turn::{Phase, Stop};
use dext_transcript::Message;
use dext_transcript::turn::{Effect, TurnContinuation};
use serde_json::{Value, json};

pub type Result<T> = std::result::Result<T, String>;

/// Rewind points listed by `targets_on`: the newest steps checked as candidates.
const LISTED_TARGETS: usize = 48;
/// How a steering message starts in the transcript (`driver`'s steering text).
const STEERED: &str = "[queued-user-update]";
/// The last line of a `show` result: its cards as JSON.
pub(crate) const DISPLAY: &str = "display: ";

fn view_error(error: crate::Error) -> String {
    format!("cannot read the session store: {error:?}")
}

fn restore(bytes: &[u8]) -> Result<TurnContinuation> {
    TurnContinuation::restore(bytes).map_err(|_| "unreadable continuation".to_string())
}
/// The committed continuation and workspace root at `revision` (the head when None),
/// with the head and the revision read. Revision 0 is the empty state before the
/// first turn: no conversation, no workspace.
fn at(
    view: &mut ReadView<'_>,
    revision: Option<u64>,
) -> crate::Result<(u64, u64, Vec<u8>, Option<String>)> {
    let (head, continuation, root) = view.head()?;
    match revision {
        None => Ok((head, head, continuation, root)),
        Some(revision) if revision == head => Ok((head, head, continuation, root)),
        Some(0) => Ok((head, 0, vec![], None)),
        Some(revision) => {
            let (continuation, root) = view.at(revision)?.ok_or(crate::Error::Invalid)?;
            Ok((head, revision, continuation, root))
        }
    }
}
/// Where a session's committed head stands: `head`,
/// `phase` (`empty` before the first turn), `stop`/`stop_text` once stopped,
/// `complete`, `accepts_prompt`, `prompts`, `files`, and for a waiting model step
/// whether its reply is already `recorded` (Continue adopts it without sending) or
/// the provider's `rejection` of it.
pub fn head_on(view: &mut ReadView<'_>) -> Result<Value> {
    let read = view.read(|view| {
        let (head, continuation, root) = view.head()?;
        let files = match &root {
            Some(root) => view.files(root)?.len(),
            None => 0,
        };
        let mut recorded = false;
        let mut rejection = None;
        if head == 0 {
            return Ok((head, None, files, recorded, rejection));
        }
        let Ok(state) = TurnContinuation::restore(&continuation) else {
            return Err(crate::Error::Incompatible);
        };
        if state.control().phase() == Phase::ModelWait {
            for effect in state
                .pending_effects()
                .map_err(|_| crate::Error::Incompatible)?
            {
                if let Effect::Model { id, .. } = effect {
                    recorded = view.recorded(&id)?;
                    rejection = view.rejection(&id)?;
                }
            }
        }
        Ok((head, Some(state), files, recorded, rejection))
    });
    let (head, state, files, recorded, rejection) = read.map_err(view_error)?;
    let rejection = rejection.map(|(status, detail)| json!({"status": status, "detail": detail}));
    let Some(state) = state else {
        return Ok(json!({
            "head": head, "phase": "empty", "stop": null, "stop_text": null,
            "complete": false, "accepts_prompt": true, "prompts": 0, "files": files,
            "recorded": false, "rejection": null,
        }));
    };
    let phase = state.control().phase();
    let stop = match phase {
        Phase::Stopped(stop) => Some(stop),
        _ => None,
    };
    let prompts = state
        .transcript()
        .map_err(|_| "unreadable transcript".to_string())?
        .messages
        .iter()
        .filter(|message| is_prompt(message))
        .count();
    Ok(json!({
        "head": head,
        "phase": phase_name(phase),
        "stop": stop.map(stop_code),
        "stop_text": stop.map(stop_text),
        "complete": stop == Some(Stop::Complete),
        "accepts_prompt": stop == Some(Stop::Complete),
        "prompts": prompts,
        "files": files,
        "recorded": recorded,
        "rejection": rejection,
    }))
}
pub fn messages_on(view: &mut ReadView<'_>, revision: Option<u64>) -> Result<Value> {
    messages_from(view, revision, 0)
}
/// The transcript committed as of `revision` (the head when None) from message `from`
/// on (a screen that holds the ones before): `{head, revision, messages, total,
/// app_details}`, `total` counting them all.
pub fn messages_from(view: &mut ReadView<'_>, revision: Option<u64>, from: usize) -> Result<Value> {
    let (head, revision, continuation, _, raw_details) = view
        .read(|view| {
            let (head, revision, continuation, root) = at(view, revision)?;
            let details = view.app_details_at(revision)?;
            Ok((head, revision, continuation, root, details))
        })
        .map_err(|_| no_revision(revision))?;
    let (messages, total): (Vec<Value>, usize) = if revision == 0 {
        (vec![], 0)
    } else {
        let transcript = restore(&continuation)?
            .transcript()
            .map_err(|_| "unreadable transcript".to_string())?;
        let total = transcript.messages.len();
        (window(&transcript.messages, from), total)
    };
    let details = raw_details.into_iter().map(|(effect, bytes)| {
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| "unreadable private app details".to_string())?;
        Ok(json!({"call":view.call_id(&effect).map_err(view_error)?, "effect":effect, "details":value}))
    }).collect::<Result<Vec<Value>>>()?;
    Ok(
        json!({"head": head, "revision": revision, "messages": messages, "total": total, "app_details": details}),
    )
}
/// Messages from index `from` on in their JSON form, a `show` result's cards (`display`)
/// included though its call (the message before it) is not.
fn window(messages: &[Message], from: usize) -> Vec<Value> {
    let before = usize::from(from > 0 && from <= messages.len());
    let mut json = Value::Array(
        messages
            .iter()
            .skip(from - before)
            .map(message_json)
            .collect(),
    );
    if before == 1 {
        annotate_display(&mut json);
    }
    let mut json = match json {
        Value::Array(json) => json,
        _ => vec![],
    };
    json.drain(..before);
    json
}
/// The app state committed as of `revision` (the head when None): `{head, revision,
/// state}`.
pub fn state_on(view: &mut ReadView<'_>, revision: Option<u64>) -> Result<Value> {
    let (head, revision, state) = view
        .read(|view| {
            let (head, revision, _, _) = at(view, revision)?;
            Ok((
                head,
                revision,
                view.app_state(crate::view::SCOPE, revision)?,
            ))
        })
        .map_err(|_| no_revision(revision))?;
    let state: Value =
        serde_json::from_slice(&state).map_err(|_| "unreadable app state".to_string())?;
    Ok(json!({"head": head, "revision": revision, "state": state}))
}
fn no_revision(revision: Option<u64>) -> String {
    match revision {
        Some(revision) => format!("revision {revision} is not a committed revision"),
        None => "cannot read the session store".into(),
    }
}
/// The workspace committed at `revision` (the head when None): files with size,
/// content digest and executable bit, symbolic links with their stored target, and
/// empty directories.
pub fn files_on(view: &mut ReadView<'_>, revision: Option<u64>) -> Result<Value> {
    view.read(|view| {
        let (head, revision, _, root) = at(view, revision)?;
        let Some(root) = root else {
            return Ok(json!({"head": head, "revision": revision, "files": [], "links": [], "directories": []}));
        };
        let files: Vec<Value> = view
            .files(&root)?
            .into_iter()
            .map(|file| {
                json!({"path": file.entry.path, "size": file.size, "digest": file.entry.object, "executable": file.entry.executable})
            })
            .collect();
        let tree = view.tree(&root)?;
        let links: Vec<Value> = tree
            .links
            .iter()
            .map(|link| json!({"path": link.path, "target": link.target}))
            .collect();
        Ok(json!({"head": head, "revision": revision, "files": files, "links": links, "directories": tree.directories}))
    })
    .map_err(|_| no_revision(revision))
}
/// The bytes of one workspace file committed at `revision` (the head when None).
pub fn read_on(view: &mut ReadView<'_>, revision: Option<u64>, path: &str) -> Result<Vec<u8>> {
    view.read(|view| {
        let (_, _, _, root) = at(view, revision)?;
        let root = root.ok_or(crate::Error::Invalid)?;
        let file = view
            .files(&root)?
            .into_iter()
            .find(|file| file.entry.path == path)
            .ok_or(crate::Error::Invalid)?;
        view.object(&file.entry.object)
    })
    .map_err(|_| format!("no file `{path}` in the workspace at that revision"))
}
/// Rewind points among the newest steps, newest first, with what each restores. A
/// rewind re-checks its target in full; the list is cheap enough for every request.
pub fn targets_on(view: &mut ReadView<'_>) -> Result<Vec<Value>> {
    targets_among(view, LISTED_TARGETS)
}
/// Rewind targets among the newest `listed` steps (at most 8192), newest first.
pub fn targets_among(view: &mut ReadView<'_>, listed: usize) -> Result<Vec<Value>> {
    let steps = view
        .read(|view| view.recent_steps(8192))
        .map_err(view_error)?;
    let candidates: Vec<u64> = steps
        .iter()
        .rev()
        .take(listed)
        .map(|step| step.revision)
        .collect();
    Ok(view
        .targets(&candidates)
        .into_iter()
        .map(|(revision, kind)| json!({"revision": revision, "kind": rewind_kind(kind)}))
        .collect())
}

/// What the user wrote in a steering message: the wrapper's guidance for the model
/// ends at its first `User update:` line, and the rest is the literal text.
pub fn steered_text(text: &str) -> Option<&str> {
    let rest = text.strip_prefix(STEERED)?;
    rest.split_once("\nUser update:\n").map(|(_, user)| user)
}
/// Adds `steer` (what the user wrote) to each steering message's text block among
/// `messages` (the transcript's JSON form), so screens show the user's words and not
/// the guidance the model reads with them.
pub fn annotate_steering(messages: &mut Value) {
    for message in messages.as_array_mut().into_iter().flatten() {
        if message["role"] != "user" {
            continue;
        }
        for block in message["content"].as_array_mut().into_iter().flatten() {
            if block["type"] == "text"
                && let Some(user) = block["text"].as_str().and_then(steered_text)
            {
                block["steer"] = json!(user);
            }
        }
    }
}
/// The cards of a `show` result (its `display:` line), or None.
pub fn display_of(content: &str) -> Option<Value> {
    let line = content.lines().last()?.strip_prefix(DISPLAY)?;
    let items: Value = serde_json::from_str(line).ok()?;
    items
        .as_array()?
        .iter()
        .all(|item| item["path"].is_string() && item["kind"].is_string())
        .then_some(items)
}
/// Adds `display` (the cards) to each successful `show` result among `messages` (the
/// transcript's JSON form or `api::message_json`'s: blocks `tool_use` {id, name} and
/// `tool_result` {tool_use_id, content, is_error}).
pub fn annotate_display(messages: &mut Value) {
    let Some(messages) = messages.as_array_mut() else {
        return;
    };
    let blocks = |message: &Value| message["content"].as_array().cloned().unwrap_or_default();
    let shown: Vec<String> = messages
        .iter()
        .flat_map(blocks)
        .filter(|block| block["type"] == "tool_use" && block["name"] == "show")
        .filter_map(|block| block["id"].as_str().map(String::from))
        .collect();
    if shown.is_empty() {
        return;
    }
    for message in messages {
        for block in message["content"].as_array_mut().into_iter().flatten() {
            if block["type"] == "tool_result"
                && block["is_error"] != true
                && block["tool_use_id"]
                    .as_str()
                    .is_some_and(|id| shown.iter().any(|shown| shown == id))
                && let Some(items) = block["content"].as_str().and_then(display_of)
            {
                block["display"] = items;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_keeps_the_cards_of_a_show_result_whose_call_is_before_it() {
        let message = |value: Value| serde_json::from_value::<Message>(value).unwrap();
        let messages = [
            message(json!({"role": "user", "content": [{"type": "text", "text": "show it"}]})),
            message(
                json!({"role": "assistant", "content": [{"type": "tool_use", "id": "c1", "name": "show", "input": {"paths": ["a.md"]}}]}),
            ),
            message(
                json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "c1",
                "content": format!("Shown.\n{DISPLAY}[{{\"path\":\"a.md\",\"kind\":\"markdown\"}}]")}]}),
            ),
        ];
        let whole = window(&messages, 0);
        let mut all = Value::Array(whole.clone());
        annotate_display(&mut all);
        let from = window(&messages, 2);
        assert_eq!(from.len(), 1);
        assert_eq!(from[0], all[2], "the result as the whole history gives it");
        assert!(from[0]["content"][0]["display"].is_array(), "{}", from[0]);
        assert!(window(&messages, 3).is_empty());
        assert!(window(&messages, 9).is_empty());
    }
}
