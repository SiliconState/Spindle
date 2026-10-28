//! Session-pack UI requests on the phone: a request waits in the thread's live state
//! (one at a time, shown by the event stream and in snapshots) until
//! `POST /api/sessions/:id/pack-ui` answers it, the run stops or the wait ends.
use super::{Refused, Watch, bad};
use serde_json::{Value, json};
use spindle_store::pack_runtime::{PackUi, UI_METHODS, UiRequest, UiResponse};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(1);

/// The request a thread is waiting on, as its event stream showed it.
pub(super) struct Pending {
    id: String,
    pub(super) event: Value,
    answer: Option<UiResponse>,
}

pub(super) struct Phone(pub(super) Arc<Watch>);

impl PackUi for Phone {
    fn methods(&self) -> Vec<String> {
        UI_METHODS.iter().map(|m| m.to_string()).collect()
    }

    fn ask(
        &self,
        pack: &str,
        request: &UiRequest,
        interrupt: &AtomicBool,
        deadline: Instant,
    ) -> UiResponse {
        let id = format!(
            "ui-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let event = json!({"type":"pack_ui","id":id,"pack":pack,"request_id":request.id,"method":request.method,"params":request.params});
        // Progress is shown, not asked: Dext's convention acknowledges each update.
        if request.method == "progress" {
            self.0.push(event);
            return UiResponse::Ok(Value::Null);
        }
        {
            let mut live = self.0.lock();
            if live.pack_ui.is_some() {
                return UiResponse::error(
                    "unavailable",
                    "another pack UI request is already pending",
                );
            }
            live.pack_ui = Some(Pending {
                id: id.clone(),
                event: event.clone(),
                answer: None,
            });
            live.push(event);
        }
        self.0.changed.notify_all();
        let mut live = self.0.lock();
        let answer = loop {
            if let Some(answer) = live
                .pack_ui
                .as_mut()
                .filter(|pending| pending.id == id)
                .and_then(|pending| pending.answer.take())
            {
                break answer;
            }
            if interrupt.load(Ordering::SeqCst) || live.deleted || live.stopping {
                break UiResponse::Cancelled;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break UiResponse::error("timeout", "nobody answered the request in time");
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
        live.pack_ui = None;
        live.push(json!({"type":"pack_ui_done","id":id}));
        drop(live);
        self.0.changed.notify_all();
        answer
    }
}

/// Delivers an answer to the request `data.id` the thread is waiting on.
pub(super) fn answer(live: &Watch, data: &Value) -> Result<(), Refused> {
    let id = data["id"].as_str().ok_or_else(|| bad("id required"))?;
    let response = UiResponse::parse(data).map_err(bad)?;
    let mut state = live.lock();
    match state.pack_ui.as_mut() {
        Some(pending) if pending.id == id && pending.answer.is_none() => {
            pending.answer = Some(response);
        }
        _ => {
            return Err(Refused(
                409,
                "that pack request is no longer waiting for an answer".into(),
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

    fn request(method: &str) -> UiRequest {
        UiRequest {
            id: "q1".into(),
            method: method.into(),
            params: json!({"message":"Proceed?"}),
        }
    }

    fn events(watch: &Watch) -> Vec<Value> {
        watch
            .lock()
            .events
            .iter()
            .map(|(_, text)| serde_json::from_str(text).unwrap())
            .collect()
    }

    #[test]
    fn a_request_waits_for_its_own_answer_and_then_leaves_the_thread() {
        let watch = Watch::new();
        let phone = Phone(watch.clone());
        let interrupt = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let asking = scope.spawn(|| {
                phone.ask(
                    "counter",
                    &request("confirm"),
                    &interrupt,
                    Instant::now() + Duration::from_secs(20),
                )
            });
            let id = loop {
                if let Some(pending) = &watch.lock().pack_ui {
                    break pending.id.clone();
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            // One request at a time; stale or malformed answers are refused.
            assert_eq!(
                phone.ask("counter", &request("confirm"), &interrupt, Instant::now()),
                UiResponse::error("unavailable", "another pack UI request is already pending")
            );
            assert_eq!(
                answer(&watch, &json!({"id":"ui-0-0","status":"ok"}))
                    .unwrap_err()
                    .0,
                409
            );
            assert_eq!(
                answer(&watch, &json!({"id":id,"status":"maybe"}))
                    .unwrap_err()
                    .0,
                400
            );
            answer(&watch, &json!({"id":id,"status":"ok","value":true})).unwrap();
            assert_eq!(asking.join().unwrap(), UiResponse::Ok(json!(true)));
            assert_eq!(
                answer(&watch, &json!({"id":id,"status":"ok"}))
                    .unwrap_err()
                    .0,
                409
            );
        });
        assert!(watch.lock().pack_ui.is_none());
        let events = events(&watch);
        assert_eq!(events[0]["type"], "pack_ui");
        assert_eq!(events[0]["params"]["message"], "Proceed?");
        assert_eq!(events.last().unwrap()["type"], "pack_ui_done");
    }

    #[test]
    fn progress_is_acknowledged_and_a_stop_or_deadline_ends_the_wait() {
        let watch = Watch::new();
        let phone = Phone(watch.clone());
        let ack = phone.ask(
            "counter",
            &request("progress"),
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(5),
        );
        assert_eq!(ack, UiResponse::Ok(Value::Null));
        assert!(watch.lock().pack_ui.is_none());
        assert_eq!(
            phone.ask(
                "counter",
                &request("form"),
                &AtomicBool::new(true),
                Instant::now() + Duration::from_secs(5)
            ),
            UiResponse::Cancelled
        );
        let timed_out = phone.ask(
            "counter",
            &request("input"),
            &AtomicBool::new(false),
            Instant::now() + Duration::from_millis(50),
        );
        assert!(matches!(timed_out, UiResponse::Error { ref code, .. } if code == "timeout"));
        assert!(watch.lock().pack_ui.is_none());
    }
}
