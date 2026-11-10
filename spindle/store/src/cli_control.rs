//! Reaching a turn another `spindle` process runs: `spindle steer` and `spindle stop`
//! leave a message in the session's `inbox/`, and the process running the turn takes
//! it within a fraction of a second (`Listener`) as the web host takes a steer or a
//! Stop. Ctrl-C (and SIGTERM) in the running process is the same Stop: the running
//! command is ended and recorded, never left as a crash; a second Ctrl-C quits at once.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The folder beside the store where other processes leave messages.
const INBOX: &str = "inbox";
/// In the inbox: the pid of the process taking messages, while it does.
const LISTENING: &str = "listening";
/// How often the running process looks at its inbox and at Ctrl-C.
const EVERY: Duration = Duration::from_millis(100);
/// How long a sender waits for its message to be taken.
const TAKEN_WITHIN: Duration = Duration::from_secs(5);

/// Interrupt signals received (SIGINT, SIGTERM); the second quits at once.
static SIGNALS: AtomicUsize = AtomicUsize::new(0);
/// Set once another process's `spindle stop` was taken.
static STOP_TAKEN: AtomicBool = AtomicBool::new(false);
/// Set once a stop the user asked for reached a running turn (`Listener`).
static STOP_ACTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_signal(_: libc::c_int) {
    if SIGNALS.fetch_add(1, Ordering::SeqCst) >= 1 {
        // Async-signal-safe: nothing but the exit.
        unsafe { libc::_exit(130) };
    }
}

/// Makes Ctrl-C and SIGTERM ask for a stop (`Listener` acts on it) instead of
/// killing the process mid-step.
pub(crate) fn catch_interrupts() {
    #[cfg(unix)]
    unsafe {
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

/// Whether the user asked this process to stop (Ctrl-C, SIGTERM or `spindle stop`).
fn interrupted() -> bool {
    SIGNALS.load(Ordering::SeqCst) > 0 || STOP_TAKEN.load(Ordering::SeqCst)
}

/// Whether a stop the user asked for reached the turn this process ran (a Ctrl-C after
/// it ended does not count).
pub(crate) fn stop_reached_turn() -> bool {
    STOP_ACTED.load(Ordering::SeqCst)
}

/// Takes the session's inbox messages and Ctrl-C into its running turn until dropped.
pub(crate) struct Listener {
    done: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    marker: PathBuf,
}

/// Where a running turn takes messages: its queue, its read-now flag and its stop.
pub(crate) struct Handles {
    pub steering: Arc<Mutex<Vec<String>>>,
    pub hurry: Arc<AtomicBool>,
    pub interrupt: Arc<AtomicBool>,
}

impl Listener {
    /// Starts taking messages for the session in `dir`; `note` tells the user what
    /// arrived (a line on stderr).
    pub(crate) fn start(dir: &Path, handles: Handles, note: fn(&str)) -> Self {
        let inbox = dir.join(INBOX);
        let marker = inbox.join(LISTENING);
        if private_dir(&inbox).is_ok() {
            let _ = std::fs::remove_file(&marker);
            let _ = write_private(&marker, std::process::id().to_string().as_bytes());
        }
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let thread = std::thread::Builder::new()
            .name("spindle-inbox".into())
            .spawn(move || {
                let mut stopping = false;
                while !flag.load(Ordering::SeqCst) {
                    if interrupted() && !stopping {
                        stopping = true;
                        STOP_ACTED.store(true, Ordering::SeqCst);
                        handles.interrupt.store(true, Ordering::SeqCst);
                        note(
                            "(stopping: the running step ends and is recorded; Ctrl-C again quits at once)",
                        );
                    }
                    for (path, message) in waiting(&inbox) {
                        // As many wait as a host lets wait; the rest are left to be
                        // withdrawn by their senders, who say so.
                        let full = || {
                            handles
                                .steering
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .len()
                                >= spindle_store::protocol::MAX_STEERS
                        };
                        if matches!(message, Message::Steer { .. }) && full() {
                            continue;
                        }
                        // Taken first, so a message is never given twice.
                        if std::fs::remove_file(&path).is_err() {
                            continue;
                        }
                        match message {
                            Message::Stop => {
                                STOP_TAKEN.store(true, Ordering::SeqCst);
                                if !stopping {
                                    stopping = true;
                                    STOP_ACTED.store(true, Ordering::SeqCst);
                                    handles.interrupt.store(true, Ordering::SeqCst);
                                    note("(another process asked this turn to stop; stopping)");
                                }
                            }
                            Message::Steer { text, now } => {
                                handles
                                    .steering
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .push(text);
                                if now {
                                    handles.hurry.store(true, Ordering::SeqCst);
                                }
                            }
                        }
                    }
                    std::thread::sleep(EVERY);
                }
            })
            .ok();
        Self {
            done,
            thread,
            marker,
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.marker);
    }
}

enum Message {
    Steer { text: String, now: bool },
    Stop,
}

/// The messages waiting in `inbox`, oldest first (their names sort by time).
fn waiting(inbox: &Path) -> Vec<(PathBuf, Message)> {
    let Ok(entries) = std::fs::read_dir(inbox) else {
        return vec![];
    };
    let mut names: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("msg-") && name.ends_with(".json"))
        })
        .collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|path| {
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
            let message = match value["kind"].as_str()? {
                "stop" => Message::Stop,
                "steer" => Message::Steer {
                    text: value["text"].as_str()?.to_string(),
                    now: value["now"] == true,
                },
                _ => return None,
            };
            Some((path, message))
        })
        .collect()
}

/// What happened to a message `send` left.
pub(crate) enum Sent {
    /// The running process took it.
    Taken,
    /// No process runs a turn of the session here.
    NotRunning,
    /// A process holds the session but takes no messages this way (`spindle serve`,
    /// `spindle api`, or a command that runs no turn).
    Elsewhere,
    /// It was not taken in time (the turn may be ending); it was withdrawn.
    NotTaken,
}

/// Leaves a steer (`text`, read now when `now`) or, with no text, a stop for the
/// process running `dir`'s turn, and waits for it to be taken.
pub(crate) fn send(dir: &Path, text: Option<&str>, now: bool) -> Result<Sent, String> {
    if !dir.join("store").is_dir() {
        return Err(format!(
            "no session in {}; start one with `spindle new`",
            dir.display()
        ));
    }
    let inbox = dir.join(INBOX);
    let held = spindle_store::api::held(dir);
    if !(held && listening(dir)) {
        return Ok(match held {
            true => Sent::Elsewhere,
            false => Sent::NotRunning,
        });
    }
    private_dir(&inbox).map_err(|error| format!("cannot write to {}: {error}", inbox.display()))?;
    let message = match text {
        Some(text) => serde_json::json!({"kind": "steer", "text": text, "now": now}),
        None => serde_json::json!({"kind": "stop"}),
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.as_nanos())
        .unwrap_or_default();
    let name = format!("msg-{stamp:020}-{}.json", std::process::id());
    let path = inbox.join(&name);
    let draft = inbox.join(format!(".{name}.tmp"));
    write_private(&draft, message.to_string().as_bytes())
        .and_then(|()| std::fs::rename(&draft, &path))
        .map_err(|error| {
            let _ = std::fs::remove_file(&draft);
            format!("cannot leave the message in {}: {error}", inbox.display())
        })?;
    let until = Instant::now() + TAKEN_WITHIN;
    while Instant::now() < until {
        if !path.exists() {
            return Ok(Sent::Taken);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Withdrawn, unless it was taken just now.
    Ok(match std::fs::remove_file(&path) {
        Ok(()) => Sent::NotTaken,
        Err(_) => Sent::Taken,
    })
}

/// Whether a live process says it takes messages for `dir`'s running turn (a turn that
/// does also holds the session: callers check that too).
pub(crate) fn listening(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(INBOX).join(LISTENING))
        .ok()
        .and_then(|pid| pid.trim().parse::<u32>().ok())
        .is_some_and(alive)
}

/// Whether a process with this pid exists (where that cannot be asked, the session's
/// held lock alone stands for it).
fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        pid > 0
            && (unsafe { libc::kill(pid, 0) } == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

fn private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => Err(error),
        _ => Ok(()),
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A steer and a stop left by another process reach the turn's handles once each;
    /// with no process listening, a message is never left behind.
    #[test]
    fn messages_reach_the_running_turn_once_and_none_are_left_without_one() {
        let dir = std::env::temp_dir().join(format!("spindle-inbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A store nobody holds: nothing runs there.
        drop(spindle_store::Store::open(&dir.join("store")).unwrap());
        assert!(matches!(
            send(&dir, Some("early"), false),
            Ok(Sent::NotRunning)
        ));
        // Held, as a running turn holds it.
        let store = spindle_store::Store::open(&dir.join("store")).unwrap();
        let handles = Handles {
            steering: Arc::default(),
            hurry: Arc::default(),
            interrupt: Arc::default(),
        };
        let (steering, hurry, interrupt) = (
            handles.steering.clone(),
            handles.hurry.clone(),
            handles.interrupt.clone(),
        );
        let listener = Listener::start(&dir, handles, |_| {});
        assert!(matches!(send(&dir, Some("first"), false), Ok(Sent::Taken)));
        assert!(matches!(send(&dir, Some("now"), true), Ok(Sent::Taken)));
        assert_eq!(*steering.lock().unwrap(), ["first", "now"]);
        assert!(hurry.load(Ordering::SeqCst));
        assert!(!interrupt.load(Ordering::SeqCst));
        assert!(matches!(send(&dir, None, false), Ok(Sent::Taken)));
        assert!(interrupt.load(Ordering::SeqCst));
        drop(listener);
        // Held but not listening: a host that takes messages its own way.
        assert!(matches!(
            send(&dir, Some("elsewhere"), false),
            Ok(Sent::Elsewhere)
        ));
        drop(store);
        assert!(matches!(
            send(&dir, Some("late"), false),
            Ok(Sent::NotRunning)
        ));
        let left: Vec<_> = std::fs::read_dir(dir.join(INBOX))
            .unwrap()
            .flatten()
            .collect();
        assert!(left.is_empty(), "{left:?}");
        assert_eq!(steering.lock().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }
}
