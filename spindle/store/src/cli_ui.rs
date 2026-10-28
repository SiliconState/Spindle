//! Session-pack UI requests answered on an interactive terminal (stdin is a TTY):
//! Dext's `form` and `progress` conventions plus `confirm`, `select` and `input`.
use serde_json::{Map, Value, json};
use spindle_store::pack_runtime::{PackUi, UI_METHODS, UiRequest, UiResponse};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

pub struct Tty;

/// Lines from one input, read by a single thread for the life of the process: a
/// prompt that times out or is interrupted leaves no reader of its own behind to
/// swallow the answer to the next one.
struct Lines {
    receiver: Mutex<mpsc::Receiver<Option<String>>>,
    /// A prompt gave up waiting: what was typed for it is dropped when the next starts.
    stale: AtomicBool,
    /// The input ended.
    closed: AtomicBool,
}

impl Lines {
    fn over(mut input: impl BufRead + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let mut text = String::new();
                let line = input
                    .read_line(&mut text)
                    .ok()
                    .filter(|n| *n > 0)
                    .map(|_| text);
                let ended = line.is_none();
                if sender.send(line).is_err() || ended {
                    return;
                }
            }
        });
        Self {
            receiver: Mutex::new(receiver),
            stale: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        }
    }

    /// The next line, or None on end of input, a stop or the deadline.
    fn next(&self, interrupt: &AtomicBool, deadline: Instant) -> Option<String> {
        let receiver = self
            .receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.stale.swap(false, Ordering::SeqCst) {
            while let Ok(line) = receiver.try_recv() {
                if line.is_none() {
                    self.closed.store(true, Ordering::SeqCst);
                }
            }
        }
        loop {
            if self.closed.load(Ordering::SeqCst) {
                return None;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if interrupt.load(Ordering::SeqCst) || left.is_zero() {
                self.stale.store(true, Ordering::SeqCst);
                return None;
            }
            match receiver.recv_timeout(left.min(Duration::from_millis(200))) {
                Ok(Some(text)) => return Some(text.trim_end_matches(['\r', '\n']).to_string()),
                Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.closed.store(true, Ordering::SeqCst);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

/// One line from stdin, or None on end of input, a stop or the deadline.
fn line(interrupt: &AtomicBool, deadline: Instant) -> Option<String> {
    static STDIN: OnceLock<Lines> = OnceLock::new();
    STDIN
        .get_or_init(|| Lines::over(std::io::BufReader::new(std::io::stdin())))
        .next(interrupt, deadline)
}

fn ask_line(question: &str, interrupt: &AtomicBool, deadline: Instant) -> Option<String> {
    eprint!("{question}");
    let _ = std::io::stderr().flush();
    line(interrupt, deadline)
}

fn text(params: &Value, key: &str) -> String {
    params[key].as_str().unwrap_or_default().to_string()
}

/// `options` as (value, label) pairs; plain strings are both.
fn options(list: &Value) -> Vec<(Value, String)> {
    list.as_array()
        .into_iter()
        .flatten()
        .map(|option| match option {
            Value::String(s) => (json!(s), s.clone()),
            other => (
                other.get("value").cloned().unwrap_or(Value::Null),
                other["label"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| other["value"].to_string()),
            ),
        })
        .collect()
}

fn choose(
    list: &[(Value, String)],
    interrupt: &AtomicBool,
    deadline: Instant,
) -> Option<Option<Value>> {
    for (index, (_, label)) in list.iter().enumerate() {
        eprintln!("  {}. {label}", index + 1);
    }
    let answer = ask_line("  choose a number: ", interrupt, deadline)?;
    Some(
        answer
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|n| list.get(n.wrapping_sub(1)))
            .map(|(value, _)| value.clone()),
    )
}

impl PackUi for Tty {
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
        let params = &request.params;
        let title = text(params, "title");
        eprintln!(
            "\n[{pack}] {}",
            if title.is_empty() {
                &request.method
            } else {
                &title
            }
        );
        let message = text(params, "message");
        if !message.is_empty() {
            eprintln!("{message}");
        }
        let answer = match request.method.as_str() {
            "progress" => {
                eprintln!(
                    "  {} {}/{}",
                    text(params, "state"),
                    params["current"],
                    params["total"]
                );
                return UiResponse::Ok(Value::Null);
            }
            "confirm" => ask_line("  [y/N] ", interrupt, deadline).map(|a| {
                json!(matches!(
                    a.trim().to_ascii_lowercase().as_str(),
                    "y" | "yes"
                ))
            }),
            "input" => {
                let default = text(params, "default");
                ask_line(
                    &if default.is_empty() {
                        "  > ".to_string()
                    } else {
                        format!("  [{default}] > ")
                    },
                    interrupt,
                    deadline,
                )
                .map(|a| json!(if a.is_empty() { default } else { a }))
            }
            "select" => match choose(&options(&params["options"]), interrupt, deadline) {
                Some(Some(value)) => Some(value),
                Some(None) => return UiResponse::error("invalid", "no such option"),
                None => None,
            },
            "form" => {
                let mut values = Map::new();
                for field in params["fields"].as_array().into_iter().flatten() {
                    let id = field["id"].as_str().unwrap_or_default().to_string();
                    let label = field["label"].as_str().unwrap_or(&id).to_string();
                    let kind = field["type"].as_str().unwrap_or("text");
                    let value = match kind {
                        "select" => {
                            eprintln!("  {label}:");
                            match choose(&options(&field["options"]), interrupt, deadline) {
                                Some(value) => value,
                                None => return UiResponse::Cancelled,
                            }
                        }
                        _ => {
                            let Some(answer) =
                                ask_line(&format!("  {label} ({kind}): "), interrupt, deadline)
                            else {
                                return UiResponse::Cancelled;
                            };
                            match kind {
                                _ if answer.is_empty() => field.get("default").cloned(),
                                "number" => answer.trim().parse::<f64>().ok().map(|n| json!(n)),
                                "boolean" => Some(json!(matches!(
                                    answer.trim().to_ascii_lowercase().as_str(),
                                    "y" | "yes" | "true"
                                ))),
                                "multiselect" => Some(json!(
                                    answer
                                        .split(',')
                                        .map(|s| s.trim().to_string())
                                        .filter(|s| !s.is_empty())
                                        .collect::<Vec<_>>()
                                )),
                                _ => Some(json!(answer)),
                            }
                        }
                    };
                    match value {
                        Some(value) => {
                            values.insert(id, value);
                        }
                        None if field["required"] == true => {
                            return UiResponse::error("invalid", format!("{label} is required"));
                        }
                        None => {}
                    }
                }
                Some(Value::Object(values))
            }
            _ => {
                return UiResponse::error(
                    "unsupported",
                    format!("terminal does not render '{}'", request.method),
                );
            }
        };
        match answer {
            Some(value) => UiResponse::Ok(value),
            None => UiResponse::Cancelled,
        }
    }
}

// A socket pair stands in for the terminal (Unix only).
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn a_prompt_that_gave_up_does_not_swallow_the_next_answer() {
        let (mut typist, terminal) = UnixStream::pair().unwrap();
        let lines = Lines::over(std::io::BufReader::new(terminal));
        let quiet = AtomicBool::new(false);
        let soon = || Instant::now() + Duration::from_millis(150);
        // Nobody answers the first prompt: it times out.
        assert_eq!(lines.next(&quiet, soon()), None);
        // An answer typed late, for the prompt that gave up, is not taken as the next.
        typist.write_all(b"late\n").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let answer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            typist.write_all(b"yes\r\nmore\n").unwrap();
            typist
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        assert_eq!(lines.next(&quiet, deadline).as_deref(), Some("yes"));
        // Typed ahead during a prompt that answered: kept for the next one.
        assert_eq!(lines.next(&quiet, deadline).as_deref(), Some("more"));
        // An interrupted prompt returns at once; the input ending ends every prompt.
        let stop = AtomicBool::new(true);
        assert_eq!(lines.next(&stop, deadline), None);
        drop(answer.join().unwrap());
        assert_eq!(lines.next(&quiet, deadline), None);
        assert_eq!(lines.next(&quiet, deadline), None);
    }
}
