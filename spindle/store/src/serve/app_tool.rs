//! App tool calls over `/v1`: a call waits in the thread's live state (one at a time,
//! announced as an `app_tool` event on the v1 stream and kept in the session snapshot)
//! until `POST /v1/sessions/:id/app_tool_result` answers it, the run stops or the
//! tool's timeout passes.
use super::{Refused, Watch, bad};
use serde_json::{Value, json};
use spindle_store::agent::{AppAnswer, AppCall, AppTools};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The call a thread waits on, as the stream announced it, and its answer once given.
pub(super) struct Pending {
    pub(super) event: Value,
    answer: Option<AppAnswer>,
}

pub(super) struct Desk(pub(super) Arc<Watch>);

impl AppTools for Desk {
    fn call(&self, call: &AppCall<'_>, interrupt: &AtomicBool) -> AppAnswer {
        let event = json!({
            "type": "app_tool",
            "call": call.call,
            "tool_use_id": call.tool_use_id,
            "name": call.name,
            "input": call.input,
            "again": call.again,
            "rerun": if call.rerun { "safe" } else { "no" },
            "timeout_s": call.timeout.as_secs(),
        });
        self.0.lock().app_tool.insert(
            call.call.to_string(),
            Pending {
                event: event.clone(),
                answer: None,
            },
        );
        self.0.push_api(event);
        let deadline = Instant::now() + call.timeout;
        let mut live = self.0.lock();
        let answer = loop {
            if let Some(answer) = live
                .app_tool
                .get_mut(call.call)
                .and_then(|pending| pending.answer.take())
            {
                break answer;
            }
            if interrupt.load(Ordering::SeqCst) || live.deleted || live.stopping {
                break AppAnswer::Stopped;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break AppAnswer::NoAnswer;
            }
            live = match self
                .0
                .changed
                .wait_timeout(live, left.min(Duration::from_millis(200)))
            {
                Ok((live, _)) => live,
                Err(poisoned) => poisoned.into_inner().0,
            };
        };
        live.app_tool.remove(call.call);
        drop(live);
        let outcome = match &answer {
            AppAnswer::Result { failed: false, .. } => "answered",
            AppAnswer::Result { failed: true, .. } => "failed",
            AppAnswer::Unknown(_) => "unknown",
            AppAnswer::NoAnswer => "no_answer",
            _ => "stopped",
        };
        self.0
            .push_api(json!({"type": "app_tool_done", "call": call.call, "outcome": outcome}));
        answer
    }
}

/// Delivers the answer `data` (`{call, content | error | unknown, failed?, files?,
/// state?}`) to the call the thread waits on.
pub(super) fn answer(live: &Watch, data: &Value) -> Result<(), Refused> {
    let object = data
        .as_object()
        .ok_or_else(|| bad("the body is a JSON object"))?;
    let call = object
        .get("call")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("`call` names the app tool call answered"))?
        .to_string();
    let mut fields = object.clone();
    fields.remove("call");
    let answer = AppAnswer::from_json(&Value::Object(fields)).map_err(bad)?;
    let mut state = live.lock();
    match state.app_tool.get_mut(&call) {
        Some(pending) if pending.answer.is_none() => {
            pending.answer = Some(answer);
        }
        _ => {
            return Err(Refused(
                409,
                "that app tool call is not waiting for an answer".into(),
            ));
        }
    }
    drop(state);
    live.changed.notify_all();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(watch: &Watch) -> Vec<Value> {
        watch
            .lock()
            .api
            .events()
            .map(|text| serde_json::from_str(text).unwrap())
            .collect()
    }

    fn ask(desk: &Desk, interrupt: &AtomicBool, timeout: Duration) -> AppAnswer {
        let input = json!({"q": "notes"});
        desk.call(
            &AppCall::new(
                "e/m/1/t/0",
                "call_1",
                "lookup",
                &input,
                false,
                false,
                timeout,
            ),
            interrupt,
        )
    }

    #[test]
    fn a_call_waits_for_its_own_answer_and_is_kept_in_the_snapshot_meanwhile() {
        let watch = Watch::new();
        let desk = Desk(watch.clone());
        let interrupt = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let asking = scope.spawn(|| ask(&desk, &interrupt, Duration::from_secs(20)));
            let event = loop {
                if let Some(pending) = watch.lock().app_tool.values().next() {
                    break pending.event.clone();
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            assert_eq!(event["call"], "e/m/1/t/0");
            assert_eq!(event["name"], "lookup");
            assert_eq!(
                answer(&watch, &json!({"call": "other", "content": "x"}))
                    .unwrap_err()
                    .0,
                409
            );
            assert_eq!(
                answer(&watch, &json!({"call": "e/m/1/t/0"})).unwrap_err().0,
                400
            );
            answer(
                &watch,
                &json!({"call": "e/m/1/t/0", "content": "Found it."}),
            )
            .unwrap();
            assert!(matches!(
                asking.join().unwrap(),
                AppAnswer::Result { ref content, failed: false, .. } if content == "Found it."
            ));
            assert_eq!(
                answer(&watch, &json!({"call": "e/m/1/t/0", "content": "again"}))
                    .unwrap_err()
                    .0,
                409
            );
        });
        assert!(watch.lock().app_tool.is_empty());
        let events = events(&watch);
        assert_eq!(events[0]["type"], "app_tool");
        assert_eq!(events[1]["type"], "app_tool_done");
        assert_eq!(events[1]["outcome"], "answered");
    }

    #[test]
    fn a_stop_or_the_timeout_ends_the_wait() {
        let watch = Watch::new();
        let desk = Desk(watch.clone());
        assert_eq!(
            ask(&desk, &AtomicBool::new(true), Duration::from_secs(5)),
            AppAnswer::Stopped
        );
        assert_eq!(
            ask(&desk, &AtomicBool::new(false), Duration::from_millis(50)),
            AppAnswer::NoAnswer
        );
        assert!(watch.lock().app_tool.is_empty());
    }
}
