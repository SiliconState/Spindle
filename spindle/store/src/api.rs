//! Spindle's public API, shared by every transport. `API_VERSION` names the contract
//! in docs/API.md: the Rust `agent` surface, the HTTP `/v1` routes of `spindle serve`
//! and the `spindle api` stdio protocol (`api::stdio`). Within one version changes are
//! additive (new fields, event types, methods, routes and feature names); anything a
//! client could notice otherwise is a new version.
//!
//! This module holds the stable JSON forms every transport uses (events, messages,
//! settings, status, targets) and read-only helpers that read a session's committed
//! state through a [`View`]: they take no owner lock, so they also answer while a turn
//! runs in this or another process.
pub mod stdio;

use crate::agent::{self, Finding, Result, Settings, Status, Target};
use crate::view::View;
use dext_core::turn::{Phase, Stop};
use dext_transcript::Block;
use dext_transcript::turn::{Effect, TurnContinuation};
use serde_json::{Value, json};
use std::path::Path;

/// Major version of the public API. Clients compare it for equality and test
/// `features` (HTTP `GET /v1`, stdio `info`) for anything added within it.
pub const API_VERSION: u32 = 1;

/// `Session::status` in its stable form (`head` is the current revision).
pub fn status_json(status: &Status) -> Value {
    let Status {
        session,
        provider,
        model,
        revision,
        epoch,
        prompts,
        state,
        files,
        unresolved,
        abandoned,
        rejected,
        isolation,
        effort,
    } = status;
    json!({
        "session": session.to_string_lossy(),
        "provider": provider,
        "model": model,
        "head": revision,
        "epoch": epoch,
        "prompts": prompts,
        "state": state,
        "files": files,
        "unresolved": unresolved,
        "abandoned": abandoned,
        "rejected": rejected,
        "isolation": isolation.as_str(),
        "effort": effort.as_str(),
    })
}

/// A rewind target in its stable form.
pub fn target_json(target: &Target) -> Value {
    let Target {
        revision,
        kind,
        last,
    } = target;
    json!({"revision": revision, "kind": rewind_kind(*kind), "last": last})
}

/// A `doctor` finding in its stable form.
pub fn finding_json(finding: &Finding) -> Value {
    let Finding {
        check,
        ok,
        warning,
        detail,
    } = finding;
    json!({"check": check, "ok": ok, "warning": warning, "detail": detail})
}

/// Providers from Dext's catalog: `ready` with the request contract and default
/// model, or why the provider cannot be used (`error`).
pub fn providers_json() -> Result<Value> {
    let list: Vec<Value> = agent::providers()?
        .into_iter()
        .map(|(id, state)| match state {
            Ok((contract, model)) => {
                json!({"id": id, "ready": true, "contract": contract, "model": model, "error": null})
            }
            Err(error) => {
                json!({"id": id, "ready": false, "contract": null, "model": null, "error": error})
            }
        })
        .collect();
    Ok(Value::Array(list))
}

fn view(dir: &Path) -> Result<View> {
    View::open(&dir.join("store")).map_err(|error| match error {
        crate::Error::Io(_) if !dir.join("store").exists() => {
            format!("no session in {}", dir.display())
        }
        other => format!("cannot read the session store: {other}"),
    })
}

fn view_error(error: crate::Error) -> String {
    format!("cannot read the session store: {error}")
}

/// Owners that may stop without closing at the same step (since the head last moved)
/// before a session counts as stuck in a crash loop: `recovery` then says `blocked`
/// and hosts stop resuming it by themselves (a person's `continue` still runs it).
pub const CRASH_LOOP: u64 = 5;

/// What a session's owner left unfinished, read without the owner lock: one item per
/// open effect or stop, each `{kind, id?, name?, outcome, next, why}` (`count` on a
/// `queued` one), and `wake`, what a host that has just started should do: `none`
/// (nothing to do), `ready` (`continue` does known work only: nothing new is sent that
/// was sent before, except a request the provider refused, and an unknown model request
/// in a session that chose `resend_unknown`), or `blocked` (`why` says what a person
/// must decide: a request that may have been billed, the step limit, a stop that needs
/// a rewind, a task that waits for its own session to be settled, or a crash loop).
/// `crashes` counts owners that stopped without closing at this step. A session
/// without a store yet (its host stopped before creating it) has nothing unfinished.
pub fn recovery(dir: &Path) -> Result<Value> {
    report_of(dir, true)
}

pub(crate) fn has_unknown(report: &Value) -> bool {
    report["items"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item["next"] != "none" && (item["outcome"] == "unknown" || item["unknown"] == true)
        })
    })
}

/// A task with resend consent may explicitly replace its uncertain first store.
fn retries_first(task: &Path, report: &Value) -> bool {
    Settings::load(task).is_ok_and(|settings| settings.resend_unknown)
        && report["items"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["kind"] == "model" && item["outcome"] == "unknown" && item["next"] == "retry"
            })
        })
}

/// `recovery`, probing the owner lock only with `probe` (see `recovery_with`).
fn report_of(dir: &Path, probe: bool) -> Result<Value> {
    if !dir.join("store").is_dir() {
        return Ok(
            json!({"head": 0, "phase": "empty", "crashes": 0, "items": [],
            "wake": {"work": "none", "why": null}}),
        );
    }
    recovery_with(dir, probe)
}

/// `recovery`; `probe` also counts an owner that is gone without closing the store
/// (it takes the owner lock for an instant, so status reads that run all the time
/// leave it out).
fn recovery_with(dir: &Path, probe: bool) -> Result<Value> {
    // A session that resends unknown requests by itself continues past them.
    let resend = Settings::load(dir).is_ok_and(|settings| settings.resend_unknown);
    let mut view = view(dir)?;
    // An owner counts as gone without closing only when no one held the lock both
    // before and after the read: one closing (or opening) in between is not a crash.
    let free_before = probe && !view.owner_alive();
    let read = view.read(|view| {
        let (head, continuation, _) = view.head()?;
        let crashes = view.open_record()?;
        let queued = view.queued()?;
        let task_runs = view.task_runs()?;
        let mut items = vec![];
        let inputs = view.queued_inputs()?;
        for (id, payload) in inputs.iter().take(256) {
            let action = crate::turn::Action::decode(payload)?;
            let (kind, next, why) = match action {
                crate::turn::Action::Start { .. } => ("prompt", "prompt", "a prompt was admitted but not committed; resubmit it with its original files and state"),
                crate::turn::Action::Steer { .. } => ("steer", "continue", "a steering update was admitted; continue commits the retained update"),
                _ => ("transition", "continue", "a local transition was admitted; continue replays its retained action"),
            };
            items.push(json!({"kind":kind,"id":id,"outcome":"known","next":next,"why":why}));
        }
        if inputs.len() > 256 {
            items.push(json!({"kind":"truncated","outcome":"blocked","next":"inspect","why":"more than 256 admitted inputs await inspection"}));
        }
        if head == 0 {
            return Ok((head, None, crashes, queued, items, task_runs));
        }
        let Ok(state) = TurnContinuation::restore(&continuation) else {
            return Err(crate::Error::Incompatible);
        };
        for effect in state
            .pending_effects()
            .map_err(|_| crate::Error::Incompatible)?
        {
            let item = match effect {
                Effect::Model { id, .. } => {
                    let (outcome, next, why) = match view.attempt(&id)? {
                        None => ("not_sent", "continue", "the next model request was prepared and never sent"),
                        Some((_, _, true)) => ("recorded", "continue", "its reply is saved; continue adopts it without sending"),
                        Some(_) if view.rejection(&id)?.is_some() => ("refused", "continue", "the provider refused it before answering; continue sends it again"),
                        // With nothing to rewind to, only `retry` (a fresh store) can send it.
                        Some(_) if resend && view.has_target() => ("unknown", "continue", "it was sent and no reply was saved: continue sends it again by itself (resend_unknown; it may be billed twice)"),
                        Some(_) => ("unknown", "retry", "it was sent and no reply was saved: it may have been billed; retry sends it again, or rewind"),
                    };
                    let retry_after = view.retry_delay(&id)?.map(|delay| delay.as_secs_f64());
                    json!({"kind": "model", "id": id, "outcome": outcome, "next": next, "why": why, "retry_after": retry_after})
                }
                Effect::Tool { id, call } => {
                    let (kind, outcome, next, why) = match view.attempt(&id)? {
                        None => ("tool", "not_run", "continue", "it has not run"),
                        Some((_, _, true)) => ("tool", "recorded", "continue", "its result is saved"),
                        // A task's call is answered by the task's own session.
                        Some((label, _, false))
                            if label.starts_with("spindle-app-tool-v1-") && call.name == agent::tasks::NAME =>
                        {
                            let child = agent::tasks::task_dir(dir, &id);
                            match report_of(&child, probe) {
                                Ok(report)
                                    if report["wake"]["work"] == "blocked"
                                        && !retries_first(&child, &report) =>
                                {
                                    let why = report["wake"]["why"].as_str().unwrap_or_default().to_string();
                                    items.push(json!({"kind": "task", "id": id, "name": call.name, "task": child.to_string_lossy(),
                                        "outcome": "blocked", "next": "settle",
                                        "why": format!("its task waits for a person in its own session: {why}")}));
                                    continue;
                                }
                                Ok(report) => {
                                    items.push(json!({"kind":"task","id":id,"effect":id,"name":call.name,"task":child,"outcome":"unanswered","next":"continue","unknown":has_unknown(&report),"why":"its task was running: continue resumes it"}));
                                    continue;
                                }
                                Err(error) => {
                                    items.push(json!({"kind":"task","id":id,"effect":id,"name":call.name,"task":child,"outcome":"blocked","next":"settle","why":error}));
                                    continue;
                                }
                            }
                        }
                        Some((label, _, false)) if label.starts_with("spindle-app-tool-v1-") => (
                            "app_tool",
                            "unanswered",
                            "answer",
                            "the app was asked and no answer was saved: continue asks it again (`again`)",
                        ),
                        Some(_) => (
                            "tool",
                            "interrupted",
                            "continue",
                            "it was running when its owner stopped: continue records it as not completed (the workspace is unchanged; anything it did outside it is not undone)",
                        ),
                    };
                    let external = if kind == "app_tool" { view.call_id(&id).unwrap_or_else(|_| id.clone()) } else { id.clone() };
                    json!({"kind": kind, "id": external, "effect": id, "name": call.name, "outcome": outcome, "next": next, "why": why})
                }
            };
            items.push(item);
        }
        let live_jobs = crate::jobs::list(&crate::jobs::session_key(&dir.join("store")));
        let mut seen_jobs = std::collections::BTreeSet::new();
        for job in view.job_effects(head)? {
            let input: Value = serde_json::from_slice(&job.input).map_err(|_| crate::Error::Incompatible)?;
            let result: Block = serde_json::from_slice(&job.result).map_err(|_| crate::Error::Incompatible)?;
            let Block::ToolResult {content, is_error, ..} = result else { return Err(crate::Error::Incompatible); };
            if job.adapter == "start_job" {
                let id = crate::jobs::job_id(&job.id);
                if is_error == Some(true) || !seen_jobs.insert(id.clone()) { continue; }
                if let Some((_,_,state)) = live_jobs.iter().find(|(key,_,_)| *key==id) {
                    items.push(json!({"kind":"job","id":id,"outcome":"known","next":"none","why":crate::jobs::describe(state)}));
                } else {
                    let (outcome, why) = if view.owner_alive() {
                        ("unknown", "the job is not in this observer process; inspect its owner, never automatically restart it")
                    } else {
                        ("lost", "the job's owner is gone; only recorded output remains and it is never automatically restarted")
                    };
                    items.push(json!({"kind":"job","id":id,"outcome":outcome,"next":"none","why":why}));
                }
            } else if is_error != Some(true)
                && (job.adapter == "stop_job" || !content.contains(" running after "))
                && let Some(id) = input["job"].as_str()
            {
                seen_jobs.insert(id.to_string());
            }
        }
        Ok((head, Some(state), crashes, queued, items, task_runs))
    });
    let (head, state, (crashes, closed), queued, mut items, task_runs) =
        read.map_err(view_error)?;
    for run in task_runs
        .into_iter()
        .filter(|run| run.queued && run.delivered.is_none() && run.state != "abandoned")
    {
        if run.origin.as_ref().is_some_and(|origin| {
            items
                .iter()
                .any(|item| item["id"] == *origin || item["effect"] == *origin)
        }) {
            continue;
        }
        let child = agent::tasks::task_dir(dir, &run.task);
        let mut unknown = false;
        let (outcome, next, why) = match run.state.as_str() {
            "finished" => ("recorded", "continue", "its saved answer is waiting to be delivered to the parent".to_string()),
            "stopped" => ("blocked", "settle", "its worker stopped without an answer; inspect its session and explicitly start the same task again".to_string()),
            _ => {
                let input = serde_json::from_slice::<Value>(&run.input).map_err(|_| "invalid retained task input".to_string());
                let recovery = input.and_then(|input| agent::tasks::validate_input(dir, &run.task, &input)).and_then(|()| report_of(&child, probe));
                match recovery {
                    Ok(report) if report["wake"]["work"] == "blocked" && !retries_first(&child, &report) => (
                        "blocked", "settle", format!("its task waits for a person in its own session: {}", report["wake"]["why"].as_str().unwrap_or_default())
                    ),
                    Ok(report) => {
                        unknown = has_unknown(&report);
                        ("running", "continue", "continue reattaches its worker and delivers its answer once".to_string())
                    },
                    Err(error) => ("blocked", "settle", format!("its task cannot be recovered: {error}")),
                }
            }
        };
        items.push(json!({"kind":"task","id":run.task,"task":child,"outcome":outcome,"next":next,"why":why,"queued":true,"unknown":unknown}));
    }
    let gone = free_before && !closed && !view.owner_alive();
    let crashes = crashes + u64::from(gone);
    let phase = state.as_ref().map(|state| state.control().phase());
    // The step limit is a person's call: `continue` grants more steps, so no host
    // resumes past it by itself.
    if let Some(Phase::Stopped(Stop::Iterations)) = phase {
        items.push(
            json!({"kind": "stop", "outcome": stop_code(Stop::Iterations), "next": "continue",
            "why": "the turn reached its step limit; continue gives it more steps"}),
        );
    } else if let Some(Phase::Stopped(stop)) = phase
        && stop != Stop::Complete
    {
        items.push(
            json!({"kind": "stop", "outcome": stop_code(stop), "next": "rewind",
            "why": agent::stop_text(stop)}),
        );
    }
    if queued > 0 {
        items.push(json!({"kind": "queued", "count": queued, "outcome": "admitted",
            "next": "continue", "why": "a prompt or message was admitted and never started; sending the same text again uses it, anything else drops it"}));
    }
    let waiting = !matches!(phase, None | Some(Phase::Stopped(Stop::Complete)))
        || queued > 0
        || items.iter().any(|item| item["queued"] == true);
    let blocked = items.iter().find(|item| {
        item["kind"] == "stop"
            || item["kind"] == "prompt"
            || item["outcome"] == "blocked"
            || matches!(item["next"].as_str(), Some("retry" | "rewind"))
    });
    let mut wake = if crashes >= CRASH_LOOP && waiting {
        json!({"work": "blocked", "why": format!("its owner stopped {crashes} times at this step without saving progress; look before continuing")})
    } else if let Some(item) = blocked {
        json!({"work": "blocked", "why": item["why"]})
    } else if waiting {
        let app = items.iter().any(|item| item["kind"] == "app_tool");
        let unknown = items.iter().any(|item| {
            item["next"] != "none" && (item["outcome"] == "unknown" || item["unknown"] == true)
        });
        json!({"work": "ready", "why": if unknown { "continue may resend an unknown request under resend_unknown consent; known-only hosts wait" } else if app { "continue asks the app again for its unanswered call" } else { "continue resumes known work only" }})
    } else {
        json!({"work": "none", "why": null})
    };
    if let Some(delay) = items
        .iter()
        .filter_map(|item| item["retry_after"].as_f64())
        .reduce(f64::max)
    {
        wake["retry_after"] = json!(delay);
    }
    Ok(json!({
        "head": head,
        "phase": phase.map_or("empty", phase_name),
        "crashes": crashes,
        "items": items,
        "wake": wake,
    }))
}

/// The conversation committed at `revision` (the head when None), in stable form.
/// A session's spend in its stable form: the `usage` totals (with `parent`, `nested`
/// and `compaction` parts) plus `turns` and `models` breakdowns.
pub fn usage_json(dir: &Path) -> Result<Value> {
    let report = agent::usage_report(dir).map_err(|e| e.to_string())?;
    let mut value = report.totals_json();
    let full = report.to_json();
    value["turns"] = full["turns"].clone();
    value["models"] = full["models"].clone();
    Ok(value)
}

/// The tasks a session started: `{tasks: [{task, dir, status}]}`, each `status` the
/// task's head state.
pub fn tasks_json(dir: &Path) -> Result<Value> {
    let tasks: Vec<Value> = agent::tasks::task_dirs(dir)?
        .into_iter()
        .map(|child| {
            json!({
                "task": child.file_name().map(|name| name.to_string_lossy().into_owned()),
                "dir": child.to_string_lossy(),
                "status": head_state(&child).unwrap_or(Value::Null),
            })
        })
        .collect();
    Ok(json!({ "tasks": tasks }))
}

/// Where a session's committed head stands, read without its owner lock: `head`,
/// `phase` (`empty` before the first turn), `stop`/`stop_text` once stopped,
/// `complete`, `accepts_prompt`, `prompts`, `files`, for a waiting model step whether
/// its reply is already `recorded` (Continue adopts it without sending) or the
/// provider's `rejection` of it, and `wake`.
pub fn head_state(dir: &Path) -> Result<Value> {
    let mut state = crate::reads::head_on(&mut view(dir)?)?;
    state["wake"] = if state["head"] == 0 {
        json!({"work": "none", "why": null})
    } else {
        recovery_with(dir, false)
            .map(|report| report["wake"].clone())
            .unwrap_or(Value::Null)
    };
    Ok(state)
}

/// The conversation committed at `revision` (the head when None), in stable form, from
/// message `from` on.
pub fn messages_at(dir: &Path, revision: Option<u64>, from: usize) -> Result<Value> {
    let mut view = view(dir).map_err(|_| no_revision(revision))?;
    crate::reads::messages_from(&mut view, revision, from)
}

/// The app state committed as of `revision` (the head when None): `{head, revision,
/// state}`, read without the owner lock.
pub fn state_at(dir: &Path, revision: Option<u64>) -> Result<Value> {
    let mut view = view(dir).map_err(|_| no_revision(revision))?;
    crate::reads::state_on(&mut view, revision)
}

/// The workspace committed at `revision` (the head when None).
pub fn files_at(dir: &Path, revision: Option<u64>) -> Result<Value> {
    let mut view = view(dir).map_err(|_| no_revision(revision))?;
    crate::reads::files_on(&mut view, revision)
}

/// The bytes of one workspace file committed at `revision` (the head when None).
pub fn read_at(dir: &Path, revision: Option<u64>, path: &str) -> Result<Vec<u8>> {
    let mut view =
        view(dir).map_err(|_| format!("no file `{path}` in the workspace at that revision"))?;
    crate::reads::read_on(&mut view, revision, path)
}

/// Rewind points among the newest steps, newest first, with what each restores.
pub fn recent_targets(dir: &Path) -> Result<Vec<Value>> {
    crate::reads::targets_on(&mut view(dir)?)
}

fn no_revision(revision: Option<u64>) -> String {
    match revision {
        Some(revision) => format!("revision {revision} is not a committed revision"),
        None => "cannot read the session store".into(),
    }
}

pub use crate::protocol::{
    base64_decode, base64_encode, message_json, phase_name, rewind_kind, settings_json, stop_code,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Event;
    use crate::rewind::RewindTargetKind;
    use dext_transcript::Message;

    #[test]
    fn base64_round_trips_and_refuses_malformed_text() {
        for bytes in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            &[0xff, 0x00, 0x10, 0x7f],
        ] {
            assert_eq!(base64_decode(&base64_encode(bytes)).unwrap(), bytes);
        }
        assert_eq!(base64_encode(&[0xff, 0x00, 0x10, 0x7f]), "/wAQfw==");
        for bad in ["Zg=", "Z===", "Zg==Zg==", "Z!==", "Zm9v\n"] {
            assert!(base64_decode(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn events_have_snake_case_types_and_stable_fields() {
        let input = json!({"command": "ls"});
        let cases = [
            (
                Event::Prompt { text: "go" },
                json!({"type": "prompt", "text": "go"}),
            ),
            (
                Event::ModelTextWithdrawn,
                json!({"type": "model_text_withdrawn"}),
            ),
            (
                Event::ToolCall {
                    id: "c1",
                    name: "bash",
                    input: &input,
                },
                json!({"type": "tool_call", "id": "c1", "name": "bash", "input": {"command": "ls"}}),
            ),
            (
                Event::Output {
                    stream: 2,
                    bytes: b"oops",
                },
                json!({"type": "output", "stream": "stderr", "data": "b29wcw=="}),
            ),
            (
                Event::Rewound {
                    target: 3,
                    revision: 9,
                    kind: RewindTargetKind::BeforeModel,
                    abandoned: 1,
                },
                json!({"type": "rewound", "target": 3, "revision": 9, "kind": "before_model", "abandoned": 1}),
            ),
            (
                Event::ProviderRetry {
                    status: 429,
                    seconds: 2,
                    attempt: 1,
                    compacting: false,
                },
                json!({"type": "provider_retry", "status": 429, "seconds": 2, "attempt": 1, "compacting": false}),
            ),
            (
                Event::Cost {
                    usd: 0.5,
                    turn: 0.75,
                    total: 2.0,
                    billing: "priced",
                },
                json!({"type": "cost", "usd": 0.5, "turn": 0.75, "total": 2.0, "billing": "priced"}),
            ),
            (
                Event::PackView {
                    pack: "demo",
                    title: "Board",
                    markdown: "# ok",
                },
                json!({"type": "pack_view", "pack": "demo", "title": "Board", "markdown": "# ok"}),
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(event.to_json(), expected);
        }
    }

    #[test]
    fn messages_keep_block_positions_and_drop_opaque_reasoning() {
        let message = Message {
            role: "assistant".into(),
            content: vec![
                Block::Thinking {
                    text: "hmm".into(),
                    signature: Some("secret-signature".into()),
                },
                Block::Text { text: "hi".into() },
                Block::ToolUse {
                    id: "c1".into(),
                    name: "bash".into(),
                    input: json!({"command": "ls"}),
                },
            ],
        };
        let value = message_json(&message);
        assert_eq!(value["role"], "assistant");
        assert_eq!(
            value["content"][0],
            json!({"type": "thinking", "text": "hmm"})
        );
        assert_eq!(value["content"][1], json!({"type": "text", "text": "hi"}));
        assert_eq!(value["content"][2]["type"], "tool_use");
        assert!(!value.to_string().contains("secret-signature"));
    }
}
