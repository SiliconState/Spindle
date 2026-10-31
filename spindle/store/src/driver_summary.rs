//! Model-written compaction, shared by both hosts: the one summary request's text and
//! bounds, and its journal in stage `summary-<command>`. The request is reserved before
//! it is sent and its reply kept, so it is counted once, reused if the compaction is
//! committed after a crash, and never sent twice (a reservation without a reply is an
//! unknown outcome: the digest stands in). Sending it is the host's.
use super::{COMPACTED, SCOPE, clip, describe_call, folded, hex};
use crate::Store;
use crate::artifact::Stage;
use crate::view::ReadView;
use dext_core::turn::Phase;
use dext_transcript::turn::TurnContinuation;
use dext_transcript::{Block, Message};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{SUMMARY_BYTES, SUMMARY_TOKENS};

/// Bounds and instructions for the one summary request.
pub(crate) const SUMMARY_SOURCE: usize = 768 * 1024;
pub(crate) const SUMMARY_SYSTEM: &str = "You summarize the earlier part of a coding session so that \
work can continue from the summary alone. Write plain text, no preamble.";
pub(crate) const SUMMARY_PROMPT: &str = "Summarize the session history below for the assistant that \
continues it. Keep everything needed to carry on: the user's goals, requirements and \
preferences; decisions made and why; files created or changed and what they hold; commands \
run and their outcomes, especially failures and how they were fixed; anything still \
broken, open questions and the next steps. Be specific: exact paths, names, numbers and \
error messages. At most about 1,500 words.";
/// Stage prefix of a compaction summary's journal (`summary-<command>`).
pub(crate) const SUMMARY_STAGE: &str = "summary-";

/// The dropped part of a compaction as text for the model to summarize, newest kept
/// when it is longer than `max`. Built from the newest message back, so what is
/// formatted is the part kept and at most one message more, however long the dropped
/// history is.
pub(crate) fn summary_source(dropped: &[Message], prompt: Option<usize>, max: usize) -> String {
    let mut kept = vec![];
    let mut size = 0;
    'messages: for (index, message) in dropped.iter().enumerate().rev() {
        for entry in summary_entries(message, Some(index) == prompt)
            .into_iter()
            .rev()
        {
            size += entry.len() + 1;
            if size > max {
                kept.push("(older history omitted)".to_string());
                break 'messages;
            }
            kept.push(entry);
        }
    }
    kept.reverse();
    kept.join("\n")
}

/// One dropped message's lines of the summary source (`prompt`: the user's current
/// request, which is kept verbatim after the summary).
fn summary_entries(message: &Message, prompt: bool) -> Vec<String> {
    if prompt {
        return vec!["[The user's current request is kept verbatim after this summary.]".into()];
    }
    let mut entries = vec![];
    for block in &message.content {
        match block {
            Block::Text { text } if text.starts_with(COMPACTED) => entries.push(format!(
                "Summary of the part before this:\n{}",
                folded(text)
            )),
            Block::Text { text } if message.role == "user" => {
                entries.push(format!("User: {}", clip(text, 8 * 1024)))
            }
            Block::Text { text } | Block::PartialStream { text } if !text.trim().is_empty() => {
                entries.push(format!("Assistant: {}", clip(text, 4 * 1024)))
            }
            Block::ToolUse { name, input, .. } => entries.push(format!(
                "Tool call: {}",
                clip(&describe_call(name, input), 1024)
            )),
            Block::ToolResult {
                content, is_error, ..
            } => entries.push(format!(
                "{}: {}",
                if *is_error == Some(true) {
                    "Failed"
                } else {
                    "Result"
                },
                clip(content, 2 * 1024)
            )),
            _ => {}
        }
    }
    entries
}

/// The summary request of a compaction's dropped part, as a nested `model()` call takes
/// it (`code_model_request`), and the source text it summarizes (whose digest names
/// the history a recorded reply is for).
pub(crate) fn summary_input(dropped: &[Message], prompt: Option<usize>) -> (String, Value) {
    let source = summary_source(dropped, prompt, SUMMARY_SOURCE);
    let input = json!({
        "prompt": format!("{SUMMARY_PROMPT}\n\n{source}"),
        "system": SUMMARY_SYSTEM,
        "max_tokens": SUMMARY_TOKENS,
    });
    (source, input)
}

/// What a fresh summary reply used, as a `usage` event counts it (input with cached
/// tokens, output). None for a failure or a reused reply (counted when it came).
pub(crate) fn summary_usage(reply: &Value) -> Option<(u64, u64)> {
    if reply["ok"] != true || reply["reused"] == true {
        return None;
    }
    let usage: dext_transcript::Usage =
        serde_json::from_value(reply["data"]["usage"].clone()).ok()?;
    Some((
        usage.input + usage.cache_read + usage.cache_create,
        usage.output,
    ))
}

/// The summary a reply carries, at most `SUMMARY_BYTES`. None (the digest stands in)
/// for a failure or an empty text.
pub(crate) fn summary_text(reply: &Value) -> Option<String> {
    if reply["ok"] != true {
        return None;
    }
    let text = reply["data"]["text"].as_str()?.trim();
    (!text.is_empty()).then(|| clip(text, SUMMARY_BYTES))
}

/// A compaction summary's journal before anything is sent: its stage, and the digest
/// of the source it summarizes (a reply is reused for the same history only).
pub(crate) struct SummaryJournal {
    stage: Stage,
    command: String,
    source: String,
}

/// What a compaction summary's journal holds.
pub(crate) enum Journaled {
    /// Nothing yet: the request may be reserved (`summary_reserve`) and sent once.
    Fresh(SummaryJournal),
    /// The reply recorded for the same history, marked `reused`: the compaction it was
    /// asked for did not commit (its owner stopped first).
    Reused(Value),
    /// Asked for once already with no reply to reuse (an unknown outcome, a failure, or
    /// another history): the digest stands in.
    Asked,
}

impl Store {
    /// The journal of compaction `command`'s summary of `source`. None (the digest
    /// stands in) when it cannot be read.
    pub(crate) fn summary_journal(&self, command: &str, source: &str) -> Option<Journaled> {
        let stage = Stage {
            id: format!("{SUMMARY_STAGE}{command}"),
            scope: SCOPE.into(),
            owner_epoch: self.epoch(),
        };
        let source = hex(&Sha256::digest(source.as_bytes()));
        let mut view = ReadView::from_store(self);
        let records = view
            .read(|view| view.stage_records(std::slice::from_ref(&stage.id)))
            .ok()?;
        let records: Vec<Value> = records
            .iter()
            .filter_map(|(_, bytes)| serde_json::from_slice(bytes).ok())
            .collect();
        if let Some(result) = records.iter().find(|record| record["kind"] == "result") {
            let mut reply = result["response"].clone();
            if result["source"] != source.as_str() || reply["ok"] != true {
                return Some(Journaled::Asked);
            }
            reply["reused"] = json!(true);
            return Some(Journaled::Reused(reply));
        }
        if records.iter().any(|record| record["kind"] == "reserved") {
            return Some(Journaled::Asked);
        }
        Some(Journaled::Fresh(SummaryJournal {
            stage,
            command: command.into(),
            source,
        }))
    }

    /// Durably reserves a summary just before it may be sent: after a crash it is an
    /// unknown outcome, never sent again. The record names the turn it is made for and
    /// `prepared`'s route (`code_model_request`'s form) for the spend report, with the
    /// request's worst case where the host prices one (so an unknown outcome counts at
    /// it against a budget).
    pub(crate) fn summary_reserve(
        &mut self,
        journal: &SummaryJournal,
        state: &TurnContinuation,
        prepared: &Value,
        worst: Option<f64>,
    ) -> crate::Result<()> {
        let idle = matches!(state.control().phase(), Phase::Stopped(_));
        let revision = self.snapshot()?.revision;
        let reserved = json!({"kind": "reserved", "command": journal.command,
            "revision": revision, "turn": state.execution(), "idle": idle,
            "provider": prepared["provider"], "model": prepared["model"],
            "contract": prepared["contract"], "source": journal.source, "worst": worst});
        self.begin_stage(&journal.stage)?;
        self.put_object(
            &journal.stage,
            &serde_json::to_vec(&reserved).map_err(|_| crate::Error::Invalid)?,
        )?;
        Ok(())
    }

    /// Keeps a sent summary's reply with its text (reused if the compaction commits
    /// after a crash); when that cannot be saved, without it (a reuse then falls back to
    /// the digest), so the call is still counted as billed. Failing both, the
    /// reservation reads as an unknown outcome once the compaction is committed.
    pub(crate) fn summary_record(&mut self, journal: &SummaryJournal, reply: &Value) {
        let mut result = json!({"kind": "result", "command": journal.command,
            "source": journal.source, "response": reply});
        let saved = |store: &mut Store, result: &Value| {
            serde_json::to_vec(result).ok().is_some_and(|bytes| {
                bytes.len() <= crate::artifact::MAX_OBJECT
                    && store.put_object(&journal.stage, &bytes).is_ok()
            })
        };
        if !saved(self, &result) {
            if let Some(data) = result["response"]["data"].as_object_mut() {
                data.remove("text");
            }
            saved(self, &result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The source as it was built before: every entry formatted, then the newest kept.
    fn whole_then_newest(dropped: &[Message], prompt: Option<usize>, max: usize) -> String {
        let entries: Vec<String> = dropped
            .iter()
            .enumerate()
            .flat_map(|(index, message)| summary_entries(message, Some(index) == prompt))
            .collect();
        let mut kept = vec![];
        let mut size = 0;
        for entry in entries.iter().rev() {
            size += entry.len() + 1;
            if size > max {
                kept.push("(older history omitted)".to_string());
                break;
            }
            kept.push(entry.clone());
        }
        kept.reverse();
        kept.join("\n")
    }

    #[test]
    fn the_source_is_built_newest_first_with_the_same_text() {
        let text = |role: &str, text: String| Message {
            role: role.into(),
            content: vec![Block::Text { text }],
        };
        let mut dropped = vec![];
        for turn in 0..40 {
            dropped.push(text(
                "user",
                format!("ask {turn} {}", "u".repeat(turn * 97)),
            ));
            let mut answer = text(
                "assistant",
                format!("answer {turn} {}", "a".repeat(turn * 211)),
            );
            answer.content.push(Block::ToolUse {
                id: format!("t{turn}"),
                name: "bash".into(),
                input: json!({"command": format!("echo {turn}")}),
            });
            dropped.push(answer);
            dropped.push(Message {
                role: "user".into(),
                content: vec![Block::ToolResult {
                    tool_use_id: format!("t{turn}"),
                    content: "r".repeat(turn * 151),
                    is_error: (turn % 3 == 0).then_some(true),
                    metadata: Default::default(),
                }],
            });
        }
        for prompt in [None, Some(0), Some(57), Some(dropped.len() - 3)] {
            for max in [0, 1, 40, 1000, 4096, 50_000, 200_000, usize::MAX] {
                assert_eq!(
                    summary_source(&dropped, prompt, max),
                    whole_then_newest(&dropped, prompt, max),
                    "prompt {prompt:?}, max {max}"
                );
            }
        }
    }
}
