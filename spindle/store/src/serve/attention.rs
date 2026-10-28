//! What the operator opened in this host's UI since the last turn: a file held in
//! the viewer, a note written for the turn ahead. The next model request is told
//! about it — an operator who spent three minutes in a file is context worth
//! having — and the ring starts over. Attention is context, never a command, and
//! never a secret from the operator: the client posts what it will send, and
//! nothing is kept after the turn reads it.
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// Entries kept per ring before the oldest are forgotten.
const KEPT: usize = 24;
/// How long an entry is worth reporting; older ones are dropped as it is read.
const REMEMBERED: Duration = Duration::from_secs(30 * 60);
/// Longest name reported, so a path cannot become the message.
const NAME: usize = 48;
/// Most items one note lists; the rest become "and N more".
const LISTED: usize = 6;

#[derive(Default)]
struct Ring {
    entries: VecDeque<Entry>,
    /// Notes the operator wrote for the turn ahead, newest last.
    notes: VecDeque<String>,
    /// The note waiting for the next model request; taken once.
    pending: Option<String>,
}

struct Entry {
    at: Instant,
    kind: &'static str,
    name: String,
    /// None while it is open; Some(dwell) once closed.
    dwell: Option<Duration>,
}

/// One operator action from the client: `open` a file or card, or `close` it (with
/// how long it was held, in milliseconds).
#[derive(Clone, Copy)]
pub(super) enum What {
    Open,
    Close(u64),
}

/// The rings of every session of this host, keyed by store path (`jobs::session_key`).
pub(super) struct Attention(BTreeMap<String, Ring>);

impl Attention {
    pub(super) fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Records an open or close. A repeated open for the same name replaces the
    /// unclosed one rather than piling up; a close without an open is dropped.
    pub(super) fn note(&mut self, session: &str, kind: &'static str, name: &str, what: What) {
        let now = Instant::now();
        let name = truncate(name);
        let ring = self.0.entry(session.to_string()).or_default();
        // Reading is pruning: what fell out of memory was never worth reporting.
        ring.entries
            .retain(|entry| now.duration_since(entry.at) < REMEMBERED);
        match what {
            What::Open => {
                ring.entries.push_back(Entry {
                    at: now,
                    kind,
                    name,
                    dwell: None,
                });
                while ring.entries.len() > KEPT {
                    ring.entries.pop_front();
                }
            }
            What::Close(ms) => {
                let dwell = Duration::from_millis(ms.min(REMEMBERED.as_millis() as u64));
                if let Some(entry) =
                    ring.entries.iter_mut().rev().find(|entry| {
                        entry.kind == kind && entry.name == name && entry.dwell.is_none()
                    })
                {
                    entry.dwell = Some(dwell);
                }
            }
        }
    }

    /// A note the operator wrote for the turn ahead: kept short and few, read once.
    pub(super) fn write(&mut self, session: &str, text: &str) {
        let text: String = text.chars().filter(|c| *c != '\0').take(512).collect();
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let ring = self.0.entry(session.to_string()).or_default();
        ring.notes.push_back(text);
        while ring.notes.len() > 4 {
            ring.notes.pop_front();
        }
    }

    /// Drops whatever waits for the next turn: the operator changed their mind.
    pub(super) fn clear(&mut self, session: &str) {
        if let Some(ring) = self.0.get_mut(session) {
            ring.entries.clear();
            ring.notes.clear();
            ring.pending = None;
        }
    }

    /// What waits for the next turn, for the composer's flag: how many looks are
    /// held, and the notes themselves.
    pub(super) fn summary(&self, session: &str) -> Value {
        let Some(ring) = self.0.get(session) else {
            return Value::Null;
        };
        if ring.entries.is_empty() && ring.notes.is_empty() && ring.pending.is_none() {
            return Value::Null;
        }
        json!({
            "held": ring.entries.len(),
            "notes": ring.notes.iter().cloned().collect::<Vec<_>>(),
        })
    }

    /// Freezes the ring into the note the next model request reads. Called as a turn
    /// is admitted, so what the operator did during a finished turn is told to the
    /// turn after it.
    pub(super) fn compose(&mut self, session: &str) {
        let ring = self.0.entry(session.to_string()).or_default();
        let mut files = vec![];
        let mut cards = vec![];
        for entry in &ring.entries {
            let list = if entry.kind == "file" {
                &mut files
            } else {
                &mut cards
            };
            let told = match entry.dwell {
                // A glance is not attention: only a held-open look says anything.
                Some(dwell) if dwell >= Duration::from_secs(5) => Some(held(dwell)),
                Some(_) => None,
                None => Some("now open".into()),
            };
            if let Some(told) = told
                && !list.iter().any(|(name, _)| *name == entry.name)
            {
                list.push((entry.name.clone(), told));
            }
        }
        let mut items: Vec<String> = std::mem::take(&mut ring.notes)
            .into_iter()
            .map(|note| format!("note: “{note}”"))
            .collect();
        items.extend(
            files
                .into_iter()
                .chain(cards)
                .map(|(name, held)| format!("{name} {held}")),
        );
        if items.len() > LISTED {
            let more = items.len() - LISTED;
            items.truncate(LISTED);
            items.push(format!("and {more} more"));
        }
        if !items.is_empty() {
            ring.pending = Some(format!(
                "\n\nThe operator's attention since the last turn: {}. Notes are \
                 the operator's words; longer looks are worth noting, brief ones are \
                 not. This is context, not a command.",
                items.join("; ")
            ));
        }
        ring.entries.clear();
    }

    /// The note waiting for the next model request of this session, if any; taking
    /// it consumes it (a retried request is not told twice).
    pub(super) fn take(&mut self, session: &str) -> Option<String> {
        self.0.get_mut(session).and_then(|ring| ring.pending.take())
    }
}

/// A dwell in words: "for 3m 12s".
fn held(dwell: Duration) -> String {
    let seconds = dwell.as_secs();
    format!("for {}m {:02}s", seconds / 60, seconds % 60)
}

fn truncate(name: &str) -> String {
    let name = name.trim();
    name.chars().take(NAME).collect()
}

/// Parses one attention post: `{kind, name, what, ms?}`. Anything unexpected is
/// dropped by the caller rather than refused — attention is advisory, and a bad post
/// should never fail a request.
pub(super) fn parse(body: &Value) -> Option<(&'static str, String, What)> {
    let kind = match body["kind"].as_str()? {
        "file" => "file",
        "card" => "card",
        _ => return None,
    };
    let name = body["name"].as_str()?.to_string();
    let what = match body["what"].as_str()? {
        "open" => What::Open,
        "close" => What::Close(body["ms"].as_u64().unwrap_or(0)),
        _ => return None,
    };
    Some((kind, name, what))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_looks_and_notes_are_told_once_and_glances_are_not() {
        let mut attention = Attention::new();
        let session = "s";
        attention.note(session, "file", "src/long.rs", What::Open);
        attention.note(session, "file", "src/long.rs", What::Close(190_000));
        attention.note(session, "file", "glance.rs", What::Open);
        attention.note(session, "file", "glance.rs", What::Close(1_000));
        attention.note(session, "file", "still-open.md", What::Open);
        attention.write(session, "  keep the paper look  ");
        assert_eq!(
            attention.summary(session)["notes"],
            json!(["keep the paper look"])
        );
        attention.compose(session);
        let note = attention.take(session).unwrap();
        assert!(note.contains("note: “keep the paper look”"), "{note}");
        assert!(note.contains("src/long.rs for 3m 10s"), "{note}");
        assert!(note.contains("still-open.md now open"), "{note}");
        assert!(!note.contains("glance.rs"), "a glance says nothing: {note}");
        // Told once.
        assert_eq!(attention.take(session), None);
        attention.compose(session);
        assert_eq!(attention.take(session), None);
        // Parsing is strict about kinds and actions.
        assert!(parse(&json!({"kind": "file", "name": "a", "what": "open"})).is_some());
        assert!(parse(&json!({"kind": "secret", "name": "a", "what": "open"})).is_none());
        assert!(parse(&json!({"kind": "file", "name": "a", "what": "delete"})).is_none());
    }
}
