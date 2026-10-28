//! Session-pack work at turn boundaries (see `pack_runtime`): steering the packs of a
//! tool round asked for, and the idle event and `continue` follow-up after a turn
//! completes.
use super::*;

/// How often a follow-up's delay checks for a stop.
const DELAY_TICK: Duration = Duration::from_millis(25);

/// Steering effects of the packs the run_code calls of the tool round that just
/// ended asked for (in call order), as the text of one queued update. They are read
/// from the pack ledger, not memory, so a restart between a call's adoption and
/// this update loses none; the `steer-r<revision>` admission delivers them once.
pub(super) fn round_steer(store: &mut Store, state: &TurnContinuation) -> Result<Option<String>> {
    let messages = state.transcript().map_err(turn_error)?.messages;
    let Some(last) = messages
        .iter()
        .rposition(|message| message.role == "assistant")
    else {
        return Ok(None);
    };
    // The phase stays AfterTools after a steering update, which follows the round's
    // results as text (compaction keeps the last reply and everything after it):
    // the round's packs have been heard then.
    if messages[last + 1..].iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, Block::Text { .. }))
    }) {
        return Ok(None);
    }
    let calls = messages[last]
        .content
        .iter()
        .filter(|block| matches!(block, Block::ToolUse { .. }))
        .count();
    if calls == 0 {
        return Ok(None);
    }
    // The engine's tool effect ids: `<execution>/m/<request ordinal>/t/<position>`.
    let round = format!(
        "{}/m/{}/t/",
        state.execution(),
        state.control().request_ordinal()
    );
    let effects: Vec<String> = (0..calls)
        .map(|position| format!("{round}{position}"))
        .collect();
    let steers = store
        .pack_round_steers(SCOPE, &effects)
        .map_err(store_error)?;
    if steers.is_empty() {
        return Ok(None);
    }
    let mut text = steers
        .iter()
        .map(|(pack, text)| format!("[pack-runtime steer] {pack}\n{text}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.len() > MAX_STEER {
        let mut cut = MAX_STEER - 64;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[pack steering truncated]");
    }
    Ok(Some(text))
}

impl Session {
    /// Fulfills a completed turn's pack need and any delayed follow-up.
    pub(super) fn pack_follow_up(&mut self, need: driver::PackIdle) -> Result<bool> {
        let end = self.run_pack_idle(&need)?;
        let next = self
            .driver
            .deliver(&mut self.store, driver::Answer::Pack { idle: need, end })?;
        let driver::Need::FollowUp { idle, follow } = next else {
            return Ok(false);
        };
        let waited = self.wait_follow_up(&follow);
        let next = self.driver.deliver(
            &mut self.store,
            driver::Answer::FollowUp {
                idle,
                follow,
                waited,
            },
        )?;
        let next = self.fulfill_summaries(next)?;
        Ok(!matches!(next, driver::Need::Stopped(_)))
    }

    pub(super) fn run_pack_idle(&mut self, need: &driver::PackIdle) -> Result<driver::PackEnd> {
        let execution = &need.execution;
        let revision = need.revision;
        let config = self.settings()?;
        let mut host = match crate::code_host::CodeHost::open(
            &config.provider,
            &config.model,
            execution,
            &self.dir,
            Some(self.observer.clone()),
        ) {
            Ok(host) => Some(host.with_credentials(self.credentials.clone())),
            Err(error) => {
                self.emit(Event::Nested {
                    label: &format!(
                        "pack idle skipped: the code policy could not be read: {error}"
                    ),
                });
                None
            }
        };
        let interrupt = self.interrupt.clone();
        let end = self
            .store
            .pack_turn_end(
                SCOPE,
                revision,
                execution,
                host.as_mut()
                    .map(|host| host as &mut dyn crate::pack_runtime::PackHost),
                &interrupt,
            )
            .map_err(store_error)?;
        Ok(driver::PackEnd {
            notes: end.notes,
            follow_up: end.follow_up.map(|f| driver::FollowUp {
                prompt: f.prompt,
                delay_ms: f.delay_ms,
            }),
        })
    }

    pub(super) fn wait_follow_up(&self, follow: &driver::FollowUp) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_millis(follow.delay_ms);
        loop {
            if self.interrupt.load(Ordering::SeqCst) {
                return false;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return true;
            }
            std::thread::sleep(left.min(DELAY_TICK));
        }
    }
}
