mod access;
mod app_tool;
mod attention;
mod auth;
mod disk;
mod http;
mod pack_ui;
mod preview;
mod selfedit;
mod skin;
mod sync;
mod tar;
mod v1;
pub use v1::token;

use dext_core::turn::{Phase, Stop, TurnControl};
use dext_transcript::turn::TurnContinuation;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use spindle_store::agent::{self, Event, Isolation, Observer, Session, SessionHost, Settings};
use spindle_store::jobs;
use spindle_store::rewind::RewindTargetKind;
use spindle_store::view::View;
use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type Result<T> = std::result::Result<T, String>;
/// Messages waiting for a running turn.
const MAX_STEERS: usize = 32;
const MAX_EVENTS: usize = 1024;
const MAX_PREVIEW: usize = 512 * 1024;
const MAX_SESSIONS: usize = 64;
const MAX_COMMANDS: i64 = 8192;
/// One uploaded attachment, and attachments per message.
const MAX_UPLOAD: usize = 8 * 1024 * 1024;
/// Uploads one thread keeps waiting to be attached.
const MAX_UPLOADS: u64 = 64 * 1024 * 1024;
const MAX_ATTACHMENTS: usize = 32;
/// Finished receipts are kept a week (retries stay idempotent), a deleted thread's an hour.
const KEEP_RECEIPTS: u64 = 7 * 24 * 3600 * 1000;
const KEEP_DELETED: u64 = 3600 * 1000;
/// A snapshot lists the newest steps; the newest of those are checked as rewind points.
const LISTED_STEPS: u64 = 8192;
const CHECKED_TARGETS: usize = 48;
const HEARTBEAT: Duration = Duration::from_secs(10);
/// Holds the fingerprint of a binary a promotion or rollback just installed; if that
/// binary cannot start the host, it restores the kept previous one (see run).
const UPGRADE_MARKER: &str = "upgrade.pending";
const BUSY_DIRECTORY: &str = "another spindle serve owns this directory";

/// A refusal with its HTTP status. Below 500 nothing was admitted, so a client may
/// forget its pending request; 500 means the outcome is unknown.
#[derive(Debug)]
struct Refused(u16, String);
impl From<String> for Refused {
    fn from(error: String) -> Self {
        Self(409, error)
    }
}
impl From<&str> for Refused {
    fn from(error: &str) -> Self {
        Self(409, error.into())
    }
}
fn bad(error: impl Into<String>) -> Refused {
    Refused(400, error.into())
}
fn unknown(error: impl std::fmt::Display) -> Refused {
    Refused(500, error.to_string())
}

/// One request that panics must not take the host with it: every lock ignores poison
/// (state behind these locks is rewritten whole, never left half-updated).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

struct Live {
    stopping: bool,
    deleted: bool,
    busy: bool,
    /// A promotion or rollback has installed its binary; Stop no longer applies.
    installed: bool,
    /// The running command is a promotion (the snapshot's `self_edit.installing`).
    installing: bool,
    /// The cards of a `show` call, until its result event carries them.
    display: Option<Value>,
    cancel: Option<Arc<AtomicBool>>,
    /// Messages for the running turn (`Session::steering_handle`) while one runs.
    steering: Option<Arc<std::sync::Mutex<Vec<String>>>>,
    /// Asks the running turn to read its queued messages now (`Session::hurry_handle`).
    hurry: Option<Arc<AtomicBool>>,
    /// App state changes for the running turn (`Session::state_handle`).
    app_state: Option<Arc<std::sync::Mutex<Vec<Value>>>>,
    /// A session pack's UI request waiting for an answer (`pack_ui`).
    pack_ui: Option<pack_ui::Pending>,
    /// An app tool call waiting for its answer over /v1 (`app_tool`).
    app_tool: BTreeMap<String, app_tool::Pending>,
    /// The last promotion was refused because the host source changed the same files
    /// as this thread: the head it was refused at, and those files. Shown until the
    /// head moves (Update from host source commits a new one).
    source_moved: Option<(u64, Vec<String>)>,
    /// The last install of this thread that a gate stopped: the head it was made at,
    /// the gate's command and the end of its output, for the Spindle change card.
    install_failed: Option<(u64, String, String)>,
    /// When the running command was admitted (ms since the epoch).
    since: u64,
    seq: u64,
    /// Ring position when the running command was admitted; a fresh subscriber
    /// replays the current command's retained events from here.
    turn_start: u64,
    bytes: usize,
    events: VecDeque<(u64, String)>,
    /// The same events in their stable form, for `/v1` streams.
    api: v1::Ring,
}
impl Live {
    fn new() -> Self {
        Self {
            stopping: false,
            deleted: false,
            busy: false,
            installed: false,
            installing: false,
            display: None,
            steering: None,
            hurry: None,
            app_state: None,
            pack_ui: None,
            app_tool: BTreeMap::new(),
            source_moved: None,
            install_failed: None,
            cancel: None,
            since: 0,
            seq: 0,
            turn_start: 0,
            bytes: 0,
            events: VecDeque::new(),
            api: v1::Ring::default(),
        }
    }
    /// The messages waiting for the running turn, oldest first, and whether the user
    /// asked for them to be read now; null when none wait.
    fn queued(&self) -> Value {
        let Some(queue) = &self.steering else {
            return Value::Null;
        };
        let queue = queue.lock().unwrap_or_else(PoisonError::into_inner);
        if queue.is_empty() {
            return Value::Null;
        }
        let now = self
            .hurry
            .as_ref()
            .is_some_and(|hurry| hurry.load(Ordering::SeqCst));
        json!({"messages": *queue, "now": now})
    }
    fn push(&mut self, event: Value) {
        let text = event.to_string();
        self.seq += 1;
        self.bytes += text.len();
        self.events.push_back((self.seq, text));
        while self.events.len() > MAX_EVENTS || self.bytes > MAX_PREVIEW {
            if let Some((_, text)) = self.events.pop_front() {
                self.bytes -= text.len();
            }
        }
    }
}
/// A thread's live state; event streams sleep on `changed` instead of polling it.
struct Watch {
    state: Mutex<Live>,
    changed: Condvar,
}
impl Watch {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(Live::new()),
            changed: Condvar::new(),
        })
    }
    fn lock(&self) -> MutexGuard<'_, Live> {
        lock(&self.state)
    }
    fn push(&self, event: Value) {
        self.lock().push(event);
        self.changed.notify_all();
    }
    /// Adds `text` for the running turn, or with `now` alone asks for the messages
    /// already waiting to be read now (the reply being written is cut). Done under the
    /// live lock, where the worker closes and drains the queue, and every screen hears
    /// what now waits. Returns how many wait; refusals carry status, code and text.
    fn steer(
        &self,
        text: Option<&str>,
        now: bool,
    ) -> std::result::Result<usize, (u16, &'static str, String)> {
        let mut live = self.lock();
        let Some(queue) = live.steering.clone() else {
            return Err((
                409,
                "not_running",
                "no turn is running; send it as a message".into(),
            ));
        };
        let mut queue = queue.lock().unwrap_or_else(PoisonError::into_inner);
        match text {
            Some(_) if queue.len() >= MAX_STEERS => {
                return Err((
                    429,
                    "rate_limited",
                    format!("{MAX_STEERS} messages are already waiting for the turn"),
                ));
            }
            Some(text) => queue.push(text.to_string()),
            None if queue.is_empty() => {
                return Err((
                    409,
                    "nothing_waiting",
                    "no message is waiting for the turn".into(),
                ));
            }
            None => {}
        }
        if now && let Some(hurry) = &live.hurry {
            hurry.store(true, Ordering::SeqCst);
        }
        let waiting = queue.len();
        drop(queue);
        let event = live.queued();
        live.push(json!({"type":"queued","queued":event}));
        drop(live);
        self.changed.notify_all();
        Ok(waiting)
    }
    /// Waits until the ring moves past `cursor`, the thread is deleted, or `until`.
    fn wait(&self, cursor: Option<u64>, until: Instant) -> MutexGuard<'_, Live> {
        let mut live = self.lock();
        while !live.deleted && cursor == Some(live.seq) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            live = match self.changed.wait_timeout(live, left) {
                Ok((live, _)) => live,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        live
    }
}
/// Puts a worker's thread back to idle however the worker ends, panics included, and
/// announces the outcome to event streams.
struct Idle<'a> {
    live: &'a Watch,
    updating: &'a AtomicBool,
    restarting: bool,
    done: Option<Value>,
}
impl Drop for Idle<'_> {
    fn drop(&mut self) {
        let mut live = self.live.lock();
        live.cancel = None;
        live.steering = None;
        live.hurry = None;
        // A worker that panicked never drained it: later changes are refused, not lost.
        live.app_state = None;
        live.busy = false;
        live.stopping = false;
        live.installed = false;
        live.installing = false;
        live.display = None;
        if !self.restarting {
            self.updating.store(false, Ordering::SeqCst);
        }
        let done = self.done.take().unwrap_or_else(|| json!({"type":"done","state":"failed","error":"the worker failed while recording its outcome; inspect the thread and its receipt","receipt_saved":false}));
        live.api.done(&done);
        live.push(done);
        drop(live);
        self.live.changed.notify_all();
    }
}
/// Why a run is refused while four others run (`submit`).
const BUSY_HOST: &str = "four sessions are already running";
/// How long `serve --resume known` waits for room to resume an interrupted thread.
const RESUME_WAIT: Duration = Duration::from_secs(30 * 60);

struct Host {
    root: PathBuf,
    skins: skin::Skins,
    db: Mutex<Connection>,
    lives: Mutex<BTreeMap<String, Arc<Watch>>>,
    token: String,
    boot: String,
    origin: Option<String>,
    /// Cloudflare Access verification for requests through the public origin.
    access: OnceLock<access::Access>,
    source: Option<PathBuf>,
    updating: AtomicBool,
    connections: AtomicUsize,
    started: Instant,
    /// Fingerprint of the running binary, taken once at start (never under a lock, and
    /// never of a file a promotion has since replaced).
    build: OnceLock<String>,
    /// Thread-list status per session, valid while the store files are unchanged.
    summaries: Mutex<BTreeMap<String, (Stamp, Value)>>,
    disk: Mutex<Option<(Instant, Value)>>,
    /// The device token most recently minted for a legacy key cookie (see auth.rs).
    legacy: Mutex<Option<(Instant, String, String, String)>>,
    /// Outstanding static-site preview tokens (preview.rs).
    previews: preview::Grants,
    /// What self-edit snapshots reuse (selfedit.rs).
    self_edit: selfedit::Cache,
    /// What the operator opened in the UI since the last turn (attention.rs), keyed
    /// by session store.
    attention: Mutex<attention::Attention>,
    /// Sessions whose commands were running when the host last stopped.
    interrupted: Vec<String>,
    _lock: File,
}
/// A worker's observer: events to the thread's streams, and the thread's host for the
/// host tools (none for promotions, which run no turn).
struct Sink(Arc<Watch>, Option<Arc<dyn SessionHost>>);
impl Observer for Sink {
    fn event(&self, event: Event<'_>) {
        // A show call's cards ride on its result event (both streams).
        if let Event::Display { items } = &event {
            self.0.lock().display = Some((*items).clone());
            return;
        }
        let display = match event {
            Event::ToolResult { .. } => self.0.lock().display.take(),
            _ => None,
        };
        let mut api = event.to_json();
        if let Some(display) = &display {
            api["display"] = display.clone();
        }
        self.0.push_api(api);
        let mut data = match event {
            Event::Prompt { text } => json!({"type":"prompt","text":text}),
            Event::Compacted { before, after } => {
                json!({"type":"compacted","before":before,"after":after})
            }
            Event::ModelStep {
                provider,
                model,
                step,
            } => json!({"type":"model","provider":provider,"model":model,"step":step}),
            Event::ModelDelta { text } => json!({"type":"delta","text":text}),
            Event::ModelTextWithdrawn => json!({"type":"withdrawn"}),
            Event::ModelText { text } => json!({"type":"model_text","text":text}),
            Event::Usage { input, output } => json!({"type":"usage","input":input,"output":output}),
            Event::Cost {
                usd,
                turn,
                total,
                billing,
            } => json!({"type":"cost","usd":usd,"turn":turn,"total":total,"billing":billing}),
            Event::ToolCall { id, name, input } => {
                json!({"type":"tool","id":id,"name":name,"input":input})
            }
            Event::Output { stream, bytes } => {
                json!({"type":"output","stream":stream,"data":base64(bytes)})
            }
            Event::ToolResult { content, failed } => {
                json!({"type":"result","content":content,"failed":failed})
            }
            Event::SettingsApplied { text } => json!({"type":"notice","text":text}),
            Event::Nested { label } => json!({"type":"nested","label":label}),
            Event::OutputEnd => json!({"type":"output_end"}),
            Event::ToolRefused { reason } => {
                json!({"type":"notice","text":format!("Refused: {reason}")})
            }
            Event::ToolSkipped => json!({"type":"notice","text":"Skipped by repeat guard"}),
            Event::ToolInterrupted => json!({"type":"notice","text":"Interrupted; not re-run"}),
            Event::ToolReused | Event::ResponseReused => {
                json!({"type":"notice","text":"Reusing a recorded result; nothing re-executed"})
            }
            Event::Unisolated { reason } => {
                json!({"type":"warning","text":format!("Not sandboxed: {reason}")})
            }
            Event::Stopped {
                reason,
                revision,
                complete,
                ..
            } => json!({"type":"stopped","reason":reason,"revision":revision,"complete":complete}),
            Event::Rewound {
                target, revision, ..
            } => json!({"type":"rewound","target":target,"revision":revision}),
            Event::Retrying { target, .. } => {
                json!({"type":"notice","text":format!("Retry from revision {target}; may incur another provider charge")})
            }
            Event::RetryingFirst { .. } => {
                json!({"type":"notice","text":"Original unknown model request retained; retrying in a fresh store"})
            }
            Event::NothingToContinue => json!({"type":"notice","text":"Nothing to continue"}),
            Event::Extended { steps } => {
                json!({"type":"notice","text":format!("Continuing past the step limit with {steps} more steps")})
            }
            Event::Steered { text } => json!({"type":"steered","text":text}),
            Event::ReplyCut => {
                json!({"type":"notice","text":"Cut the reply being written so your message is read now (what it had written may still be billed)"})
            }
            Event::ReplyNotCut => {
                json!({"type":"notice","text":"This reply cannot be cut here; your message is read as soon as it ends"})
            }
            Event::PackView {
                pack,
                title,
                markdown,
            } => json!({"type":"pack_view","pack":pack,"title":title,"markdown":markdown}),
            Event::ProviderRetry {
                status,
                seconds,
                attempt,
                compacting,
            } => {
                let then = if compacting {
                    "compacting history and retrying".to_string()
                } else {
                    format!("retrying in {seconds}s")
                };
                json!({"type":"notice","text": if status == agent::DROPPED {
                    format!("The connection to the provider dropped; sending again in {seconds}s (retry {attempt}/3, the dropped request may have been billed)")
                } else {
                    format!("Provider refused (HTTP {status}); {then} ({attempt}/6)")
                }})
            }
            _ => return,
        };
        if let Some(display) = display {
            data["display"] = display;
        }
        self.0.push(data);
    }
    fn pack_ui(&self) -> Option<Arc<dyn spindle_store::pack_runtime::PackUi>> {
        Some(Arc::new(pack_ui::Phone(self.0.clone())))
    }
    fn host(&self) -> Option<Arc<dyn SessionHost>> {
        self.1.clone()
    }
    fn app_tools(&self) -> Option<Arc<dyn spindle_store::agent::AppTools>> {
        Some(Arc::new(app_tool::Desk(self.0.clone())))
    }
}

fn private_dir(path: &Path) -> Result<()> {
    spindle_store::durable::create_dir(path).map_err(|e| e.to_string())?;
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err("private directory required".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(format!("{} must have mode 700", path.display()));
        }
    }
    Ok(())
}
/// host.sqlite holds prompts, receipts and device hashes: it is created 0600 before
/// SQLite opens it (SQLite gives -wal/-shm the main file's mode), and files left by
/// an older build are tightened.
fn private_database(root: &Path) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(root.join("host.sqlite")).map_err(err)?;
    #[cfg(unix)]
    for name in ["host.sqlite", "host.sqlite-wal", "host.sqlite-shm"] {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join(name);
        if path.exists() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(err)?;
        }
    }
    Ok(())
}
fn random() -> Result<String> {
    #[cfg(unix)]
    {
        let mut bytes = [0u8; 32];
        File::open("/dev/urandom")
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
            .map_err(|e| e.to_string())?;
        Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }
    #[cfg(not(unix))]
    {
        Err("serve currently requires a Unix host".into())
    }
}
fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn hash(value: &[u8]) -> String {
    Sha256::digest(value)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn admin_name(data: &Value) -> Result<String> {
    let name: String = data["name"]
        .as_str()
        .ok_or("a name is required")?
        .chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect();
    let name = name.trim();
    if name.is_empty() {
        return Err("a name is required".into());
    }
    Ok(name.into())
}
/// Settings for a new thread, resolved exactly as its worker will resolve them.
fn new_settings(data: &Value) -> Result<Settings> {
    let provider = data["provider"].as_str().unwrap_or("glm");
    let model = data["model"]
        .as_str()
        .filter(|m| !m.is_empty())
        .map(String::from);
    let mut settings = Settings::resolve(provider, model)?;
    settings.isolation = Isolation::Required;
    settings.network = data["network"].as_bool().unwrap_or(false);
    settings.toolchain = toolchain_choice(data)?.unwrap_or(false);
    if let Some(effort) = data["effort"].as_str() {
        settings.effort = agent::Effort::parse(effort)
            .ok_or("effort must be default, off, low, medium, high, xhigh or max")?;
        agent::describe_effort(&settings.provider, &settings.model, settings.effort)?;
    }
    if let Some(compaction) = data["compaction"].as_str() {
        settings.compaction = compaction_mode(compaction)?;
    }
    agent::app_tools::apply_json(&mut settings, data)?;
    if !data["max_usd"].is_null() {
        settings.max_usd = Some(
            data["max_usd"]
                .as_f64()
                .filter(|max| max.is_finite() && *max >= 0.0)
                .ok_or("max_usd must be a non-negative number of dollars, or null")?,
        );
    }
    Ok(settings)
}
/// A command's `toolchain` choice: on only where the host has a Rust toolchain.
fn toolchain_choice(data: &Value) -> Result<Option<bool>> {
    match data["toolchain"].as_bool() {
        Some(true) if !agent::toolchain_available() => {
            Err("this host has no Rust toolchain to offer".into())
        }
        choice => Ok(choice),
    }
}
fn compaction_mode(text: &str) -> Result<agent::Compaction> {
    agent::Compaction::parse(text).ok_or_else(|| "compaction must be model or digest".into())
}
/// Output chunks travel as base64 text: a JSON byte array is 3–4x larger and
/// exhausts the bounded preview ring on chatty commands.
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}
/// Forgets finished receipts and their submissions after a week, and a deleted
/// thread's after an hour. Running commands and recent IDs stay, so a retry of a
/// recent request still returns its receipt instead of running again.
fn prune(db: &Connection) -> rusqlite::Result<usize> {
    // A host-level command (a rollback without a thread) has session '' and is kept
    // the week, like a live thread's.
    const OLD: &str = "SELECT c.id FROM commands c JOIN command_times t ON t.id=c.id WHERE c.state!='running' AND (t.at<?1 OR (t.at<?2 AND c.session!='' AND c.session NOT IN (SELECT id FROM sessions)))";
    let now = now_ms() as i64;
    let cutoffs = params![now - KEEP_RECEIPTS as i64, now - KEEP_DELETED as i64];
    let tx = db.unchecked_transaction()?;
    tx.execute(
        &format!("DELETE FROM submissions WHERE id IN ({OLD})"),
        cutoffs,
    )?;
    let removed = tx.execute(
        &format!("DELETE FROM commands WHERE id IN ({OLD})"),
        cutoffs,
    )?;
    tx.execute(
        "DELETE FROM command_times WHERE id NOT IN (SELECT id FROM commands)",
        [],
    )?;
    tx.commit()?;
    Ok(removed)
}

/// Identity of a store's files: every commit changes the WAL's length or time.
type Stamp = [(u64, u128); 2];
fn stamp(store: &Path) -> Option<Stamp> {
    let main = std::fs::metadata(store.join("authority.sqlite")).ok()?;
    let of = |meta: &std::fs::Metadata| {
        let at = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        (meta.len(), at)
    };
    let wal = std::fs::metadata(store.join("authority.sqlite-wal"))
        .map(|meta| of(&meta))
        .unwrap_or((0, 0));
    Some([of(&main), wal])
}
/// What the thread list says about an idle thread, read from the head's control
/// header; only a waiting model step decodes the whole continuation.
fn summarize(store: &Path) -> Result<&'static str> {
    let mut view = View::open(store).map_err(err)?;
    view.read(|view| {
        let (head, bytes, _) = view.head()?;
        if head == 0 {
            return Ok("empty");
        }
        let header = (bytes.len() >= 27 && &bytes[..4] == b"DTT1")
            .then(|| TurnControl::restore(&bytes[4..27]).ok())
            .flatten();
        let phase = match header {
            Some(control) => control.phase(),
            None => TurnContinuation::restore(&bytes)
                .map_err(|_| spindle_store::Error::Incompatible)?
                .control()
                .phase(),
        };
        Ok(match phase {
            Phase::Stopped(Stop::Complete) => "done",
            Phase::Stopped(Stop::Cancelled) => "stopped",
            Phase::ModelWait => {
                let state = TurnContinuation::restore(&bytes)
                    .map_err(|_| spindle_store::Error::Incompatible)?;
                let mut recorded = false;
                for effect in state
                    .pending_effects()
                    .map_err(|_| spindle_store::Error::Incompatible)?
                {
                    if let dext_transcript::turn::Effect::Model { id, .. } = effect {
                        recorded = view.recorded(&id)?;
                    }
                }
                if recorded {
                    "reply saved"
                } else {
                    "needs attention"
                }
            }
            _ => "needs attention",
        })
    })
    .map_err(err)
}

/// Runs a candidate binary with a deadline; its output goes to `log`.
fn run_bounded(command: &mut std::process::Command, log: &Path, limit: Duration) -> Result<bool> {
    let output = File::create(log).map_err(err)?;
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(output.try_clone().map_err(err)?)
        .stderr(output)
        .spawn()
        .map_err(err)?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(err)? {
            return Ok(status.success());
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return Err("the candidate did not finish its host check in time".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn run(root: PathBuf, args: Vec<String>) -> Result<()> {
    if args == ["--check"] {
        return check(root);
    }
    let mut port = 8790u16;
    let mut origin = None;
    let mut source = None;
    let (mut team, mut audience) = (None, None);
    let mut resume = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let value = args.next().ok_or("serve options need values")?;
        match arg.as_str() {
            "--port" => port = value.parse().map_err(|_| "invalid port")?,
            "--origin" => {
                let url = reqwest::Url::parse(&value).map_err(err)?;
                if url.scheme() != "https"
                    || url.path() != "/"
                    || url.query().is_some()
                    || url.fragment().is_some()
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                {
                    return Err("--origin needs an HTTPS origin".into());
                }
                origin = Some(value.trim_end_matches('/').to_string());
            }
            "--dev-source" => source = Some(PathBuf::from(value).canonicalize().map_err(err)?),
            "--access-team" => team = Some(value),
            "--access-aud" => audience = Some(value),
            "--resume" => {
                resume = match value.as_str() {
                    "known" => true,
                    "none" => false,
                    _ => return Err("--resume takes known or none".into()),
                }
            }
            _ => return Err(format!("unknown serve option {arg}")),
        }
    }
    let access = match (team, audience) {
        (None, None) => None,
        (Some(team), Some(audience)) => {
            if origin.is_none() {
                return Err("--access-team checks requests to --origin; set it too".into());
            }
            Some((team, audience))
        }
        _ => return Err("--access-team and --access-aud go together".into()),
    };
    // The team's keys are kept in the host directory, so only once it is checked.
    let started = Host::open(root.clone(), origin, source).and_then(|host| {
        if let Some((team, audience)) = &access {
            let _ = host
                .access
                .set(access::Access::new(team, audience, &host.root)?);
        }
        let listener = TcpListener::bind(("127.0.0.1", port)).map_err(err)?;
        Ok((host, listener))
    });
    let (host, listener) = started.map_err(|error| after_failed_start(&root, error))?;
    let _ = std::fs::remove_file(root.join(UPGRADE_MARKER));
    let host = Arc::new(host);
    if let Some(build) = std::env::current_exe()
        .ok()
        .and_then(|exe| spindle_store::upgrade::fingerprint(&exe).ok())
    {
        let _ = host.build.set(build);
    }
    println!(
        "Spindle listening on http://127.0.0.1:{}; pairing key in {}/pairing.key",
        listener.local_addr().map_err(err)?.port(),
        host.root.display()
    );
    if resume {
        let host = host.clone();
        std::thread::spawn(move || host.resume_known());
    }
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                // Out of descriptors or memory: keep serving once they free up rather
                // than exit and lock the phone out.
                eprintln!("spindle serve: accept failed: {error}");
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if host.connections.fetch_add(1, Ordering::SeqCst) >= 48 {
            host.connections.fetch_sub(1, Ordering::SeqCst);
            let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
            // The v1 envelope; the phone shows its `error` as it showed the plain text.
            let limit = json!({"error": "connection limit", "code": "rate_limited"});
            let _ = send_json(&mut stream, 429, &limit, "");
            continue;
        }
        let host = host.clone();
        std::thread::spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle(&host, &mut stream)
            }));
            host.connections.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

/// `serve --check`. Given a host directory holding host.sqlite (the private copy a
/// promoting host makes, no sessions), it opens it exactly as the host does and binds
/// an ephemeral loopback port, so a candidate that could not start is never installed.
fn check(root: PathBuf) -> Result<()> {
    if !root.join("host.sqlite").is_file() {
        println!("spindle web host v2");
        return Ok(());
    }
    let host = Host::open(root, None, None)?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(err)?;
    let port = listener.local_addr().map_err(err)?.port();
    drop(listener);
    drop(host);
    println!("spindle web host v2: host.sqlite opened, 127.0.0.1:{port} bound");
    Ok(())
}

/// A binary that a promotion just installed and that cannot start the host is
/// swapped back for the kept previous one (once: the marker is consumed); the
/// non-zero exit then lets the supervisor start the restored binary.
fn after_failed_start(root: &Path, error: String) -> String {
    let marker = root.join(UPGRADE_MARKER);
    let Ok(expected) = std::fs::read_to_string(&marker) else {
        return error;
    };
    if error == BUSY_DIRECTORY {
        return error;
    }
    let _ = std::fs::remove_file(&marker);
    let Ok(exe) = std::env::current_exe() else {
        return error;
    };
    if spindle_store::upgrade::fingerprint(&exe).ok().as_deref() != Some(expected.trim()) {
        return error;
    }
    match spindle_store::upgrade::rollback(&exe, false) {
        Ok(()) => format!(
            "{error}; this newly installed binary could not start the host, so the previous binary was restored and starts on the next restart"
        ),
        Err(rollback) => format!("{error}; automatic rollback failed: {rollback}"),
    }
}

impl Host {
    fn open(root: PathBuf, origin: Option<String>, source: Option<PathBuf>) -> Result<Self> {
        private_dir(&root)?;
        private_dir(&root.join("sessions"))?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(root.join("serve.lock")).map_err(err)?;
        lock.try_lock().map_err(|_| BUSY_DIRECTORY)?;
        let key = root.join("pairing.key");
        let token = if key.exists() {
            std::fs::read_to_string(&key)
                .map_err(err)?
                .trim()
                .to_string()
        } else {
            // Whole or absent after a power cut: an empty key would stop the host.
            let token = random()?;
            spindle_store::durable::write(&key, token.as_bytes()).map_err(err)?;
            token
        };
        if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid pairing.key".into());
        }
        private_database(&root)?;
        let db = Connection::open(root.join("host.sqlite")).map_err(err)?;
        // Read before they are marked interrupted below (none on a fresh host).
        let interrupted: Vec<String> = db
            .prepare("SELECT DISTINCT session FROM commands WHERE state='running'")
            .and_then(|mut query| {
                query
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()
            })
            .unwrap_or_default();
        // New tables only: an older binary restored by rollback still reads this file.
        spindle_store::durable::full_flushes(&db).map_err(err)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA trusted_schema=OFF;
            CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY,name TEXT NOT NULL,dev INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS commands(id TEXT PRIMARY KEY,session TEXT NOT NULL,digest TEXT NOT NULL,state TEXT NOT NULL,error TEXT);
            CREATE TABLE IF NOT EXISTS submissions(id TEXT PRIMARY KEY REFERENCES commands(id),payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS command_times(id TEXT PRIMARY KEY,at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS devices(id TEXT PRIMARY KEY,hash TEXT NOT NULL UNIQUE,label TEXT NOT NULL,created INTEGER NOT NULL,seen INTEGER NOT NULL,issued INTEGER NOT NULL);
            UPDATE commands SET state='interrupted',error='Host restarted. Outcome may be committed; inspect the session. This command will not be re-dispatched.' WHERE state='running';").map_err(err)?;
        // Receipts from before command_times start their retention window now.
        db.execute(
            "INSERT OR IGNORE INTO command_times SELECT id,?1 FROM commands",
            [now_ms() as i64],
        )
        .map_err(err)?;
        if let Err(error) = prune(&db) {
            eprintln!("spindle serve: receipt pruning failed: {error}");
        }
        let mut lives = BTreeMap::new();
        let mut statement = db.prepare("SELECT id FROM sessions").map_err(err)?;
        for id in statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(err)?
        {
            let id = id.map_err(err)?;
            if !id_ok(&id) {
                return Err("invalid session id".into());
            }
            lives.insert(id, Watch::new());
        }
        drop(statement);
        for id in lives.keys() {
            let dir = root.join("sessions").join(id);
            let moved = root.join("trash").join(id);
            if !dir.exists() && moved.exists() {
                std::fs::rename(&moved, &dir).map_err(err)?;
            }
            // Migrate older stores now rather than on a thread's first command.
            let store = dir.join("store");
            if spindle_store::legacy_store(&store) {
                match spindle_store::Store::open(&store) {
                    Ok(_) => println!(
                        "session {id}: store migrated to schema {} (the pre-migration copy is kept as store/authority.vNN.sqlite)",
                        spindle_store::CAPABILITIES.schema
                    ),
                    Err(error) => eprintln!("session {id}: store not migrated: {error}"),
                }
            }
        }
        // Whatever remains in trash belongs to no thread: deletions whose removal
        // failed or was cut off. Remove it in the background so start is not delayed.
        let stale: Vec<PathBuf> = std::fs::read_dir(root.join("trash"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| id_ok(name) && !lives.contains_key(name))
            })
            .map(|entry| entry.path())
            .collect();
        if !stale.is_empty() {
            std::thread::spawn(move || stale.iter().for_each(|path| disk::purge(path)));
        }
        Ok(Self {
            skins: skin::Skins::open(&root)?,
            root,
            db: Mutex::new(db),
            lives: Mutex::new(lives),
            token,
            boot: random()?,
            origin,
            access: OnceLock::new(),
            source,
            updating: AtomicBool::new(false),
            connections: AtomicUsize::new(0),
            started: Instant::now(),
            build: OnceLock::new(),
            summaries: Mutex::new(BTreeMap::new()),
            disk: Mutex::new(None),
            legacy: Mutex::new(None),
            previews: preview::Grants::default(),
            self_edit: selfedit::Cache::default(),
            attention: Mutex::new(attention::Attention::new()),
            interrupted,
            _lock: lock,
        })
    }

    /// `serve --resume known`: continues each session a stop of the host interrupted
    /// whose recovery is `ready` (only known work: nothing it sends was sent before);
    /// any other waits for a person, as without the option.
    fn resume_known(self: &Arc<Self>) {
        for id in &self.interrupted {
            let Ok(report) = spindle_store::api::recovery(&self.dir(id)) else {
                continue;
            };
            if report["wake"]["work"] != "ready"
                || report["items"].as_array().is_some_and(|items| {
                    items.iter().any(|item| {
                        item["next"] != "none"
                            && (item["outcome"] == "unknown" || item["unknown"] == true)
                    })
                })
            {
                eprintln!(
                    "spindle serve: {id} waits for a person: {}",
                    report["wake"]["why"]
                        .as_str()
                        .unwrap_or("nothing to resume")
                );
                continue;
            }
            let request = match random() {
                Ok(random) => format!("resume-{}", &random[..32]),
                Err(_) => continue,
            };
            let data = json!({"request_id": request, "kind": "continue",
                "expected_revision": report["head"]});
            // At most four sessions run at once: the rest wait for room (a while).
            let started = std::time::Instant::now();
            loop {
                match self.submit(id, data.clone()) {
                    Ok(_) => eprintln!("spindle serve: resumed {id} ({request})"),
                    Err(Refused(_, why)) if why == BUSY_HOST && started.elapsed() < RESUME_WAIT => {
                        std::thread::sleep(Duration::from_secs(5));
                        continue;
                    }
                    Err(Refused(_, why)) => {
                        eprintln!("spindle serve: could not resume {id}: {why}");
                    }
                }
                break;
            }
        }
    }
    fn dir(&self, id: &str) -> PathBuf {
        self.root.join("sessions").join(id)
    }
    fn workspace_file(&self, id: &str, revision: u64, path: &str) -> Result<Vec<u8>> {
        let mut view = View::open(&self.dir(id).join("store")).map_err(err)?;
        view.read(|view| {
            let (_, root) = view.at(revision)?.ok_or(spindle_store::Error::Invalid)?;
            let root = root.ok_or(spindle_store::Error::Invalid)?;
            let file = view
                .files(&root)?
                .into_iter()
                .find(|f| f.entry.path == path)
                .ok_or(spindle_store::Error::Invalid)?;
            view.object(&file.entry.object)
        })
        .map_err(err)
    }
    /// The files of a revision (path, digest, size, executable); none before the first turn.
    fn files_at(&self, id: &str, revision: u64) -> Result<Vec<spindle_store::view::File>> {
        let mut view = View::open(&self.dir(id).join("store")).map_err(err)?;
        view.read(|view| {
            let (_, root) = view.at(revision)?.ok_or(spindle_store::Error::Invalid)?;
            match root {
                Some(root) => view.files(&root),
                None => Ok(vec![]),
            }
        })
        .map_err(err)
    }
    /// The links and empty directories at a revision.
    fn tree_at(&self, id: &str, revision: u64) -> Result<spindle_store::workspace::Tree> {
        let mut view = View::open(&self.dir(id).join("store")).map_err(err)?;
        view.read(|view| {
            let (_, root) = view.at(revision)?.ok_or(spindle_store::Error::Invalid)?;
            match root {
                Some(root) => view.tree(&root),
                None => Ok(Default::default()),
            }
        })
        .map_err(err)
    }
    /// Copies a file of the thread's head revision onto the operator's shelf, under
    /// its own name; the viewer's Keep on shelf.
    /// One promotion gate. A failure is kept for the thread's Spindle change card
    /// (the command and the end of its output, at `revision`), so the model can be
    /// asked to fix exactly what stopped the install; a Stop is reported as itself.
    fn gate(
        &self,
        tree: &Path,
        program: &str,
        args: &[&str],
        revision: u64,
        live: &Watch,
        cancel: &AtomicBool,
    ) -> Result<()> {
        sandboxed_gate(tree, program, args, live, cancel).map_err(|(error, output)| {
            if cancel.load(Ordering::SeqCst) {
                return STOPPED.to_string();
            }
            let command = error.trim_end_matches(" failed").to_string();
            live.lock().install_failed = Some((revision, command, output));
            error
        })
    }
    fn keep_on_shelf(&self, id: &str, body: &Value) -> Result<Value> {
        let path = body["path"]
            .as_str()
            .filter(|path| !path.is_empty())
            .ok_or("path required")?;
        let mut view = View::open(&self.dir(id).join("store")).map_err(err)?;
        let head = view
            .read(|view| view.head().map(|(head, _, _)| head))
            .map_err(err)?;
        let found = self
            .files_at(id, head)?
            .into_iter()
            .find(|file| file.entry.path == path)
            .ok_or("no such file at the head")?;
        if found.size > SHELF_FILE_MAX as u64 {
            return Err("a shelf note is at most 256 KiB".into());
        }
        let bytes = view
            .read(|view| view.object(&found.entry.object))
            .map_err(err)?;
        // The file's name, made a valid note name (other characters become '-').
        let name: String = path
            .rsplit('/')
            .next()
            .unwrap_or(path)
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '-' | '_') {
                    c
                } else {
                    '-'
                }
            })
            .take(64)
            .collect::<String>()
            .trim_start_matches('.')
            .trim()
            .to_string();
        let name = if name.is_empty() {
            "note".to_string()
        } else {
            name
        };
        let target = shelf_path(self, &name)?;
        std::fs::create_dir_all(self.root.join("shelf")).map_err(err)?;
        spindle_store::durable::write(&target, &bytes).map_err(err)?;
        Ok(json!({"kept": name, "files": shelf_files(self)?}))
    }
    fn live(&self, id: &str) -> Result<Arc<Watch>> {
        lock(&self.lives)
            .get(id)
            .cloned()
            .ok_or("session not found".into())
    }
    fn receipt(&self, id: &str) -> Result<Value> {
        lock(&self.db).query_row("SELECT c.session,c.state,c.error,s.payload FROM commands c LEFT JOIN submissions s ON s.id=c.id WHERE c.id=?1", [id], |r| Ok(json!({"id":id,"session":r.get::<_,String>(0)?,"state":r.get::<_,String>(1)?,"error":r.get::<_,Option<String>>(2)?,"submission":r.get::<_,Option<String>>(3)?.and_then(|s| serde_json::from_str::<Value>(&s).ok())}))).map_err(err)
    }
    /// The receipt of an already admitted request ID, refusing a changed payload.
    fn admitted(&self, request: &str, digest: &str) -> std::result::Result<Option<Value>, Refused> {
        let existing: Option<String> = lock(&self.db)
            .query_row("SELECT digest FROM commands WHERE id=?1", [request], |r| {
                r.get(0)
            })
            .optional()
            .map_err(unknown)?;
        match existing {
            None => Ok(None),
            Some(existing) if existing != digest => {
                Err("request_id reused with different payload".into())
            }
            Some(_) => self.receipt(request).map(Some).map_err(unknown),
        }
    }
    fn submit(self: &Arc<Self>, id: &str, data: Value) -> std::result::Result<Value, Refused> {
        let request = data["request_id"]
            .as_str()
            .filter(|s| id_ok(s))
            .ok_or_else(|| bad("a stable request_id is required"))?
            .to_string();
        let kind = data["kind"].as_str().ok_or_else(|| bad("kind required"))?;
        if !matches!(
            kind,
            "new"
                | "say"
                | "continue"
                | "retry"
                | "rewind"
                | "stop"
                | "promote"
                | "rollback"
                | "delete"
                | "rename"
                | "settings"
                | "update"
                | "dismiss_install"
                | "state"
                | "task_start"
        ) {
            return Err(bad("unsupported command"));
        }
        // App state changes are JSON merge patches (objects) of bounded size.
        for (field, needed) in [("patch", kind == "state"), ("state", false)] {
            match data.get(field) {
                None | Some(Value::Null) if !needed => {}
                Some(patch)
                    if patch.is_object()
                        && patch.to_string().len() <= spindle_store::transition::MAX_STATE => {}
                _ => {
                    return Err(bad(format!(
                        "`{field}` is a JSON merge patch: an object of at most 1 MiB"
                    )));
                }
            }
        }
        if data.get("state").is_some_and(|state| !state.is_null()) && !matches!(kind, "new" | "say")
        {
            return Err(bad("`state` goes with a prompt (`new` or `say`)"));
        }
        if kind == "task_start" {
            let task = data["task"]
                .as_str()
                .filter(|task| {
                    (1..=80).contains(&task.len())
                        && task
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                })
                .ok_or_else(|| bad("task id required"))?;
            let _ = task;
            if data["report"]
                .as_str()
                .is_some_and(|report| !matches!(report, "wait" | "queue"))
            {
                return Err(bad("report must be wait or queue"));
            }
            if let Some(why) = agent::tasks::refusal(&data) {
                return Err(bad(why));
            }
        }
        // Administrative commands complete under the session locks; no worker runs.
        let administrative = matches!(kind, "delete" | "rename" | "settings" | "dismiss_install");
        // Dismissing an install request touches no store: allowed while a turn runs.
        let anytime = matches!(kind, "stop" | "dismiss_install");
        let digest = hash(format!("{id}\n{data}").as_bytes());
        // A retry of an admitted request gets its receipt before anything is re-checked.
        if let Some(receipt) = self.admitted(&request, &digest)? {
            return Ok(receipt);
        }
        if matches!(kind, "new" | "say")
            && data["text"]
                .as_str()
                .is_none_or(|s| s.trim().is_empty() || s.len() > 16 * 1024)
        {
            return Err(bad("prompt must be 1–16384 bytes"));
        }
        if kind == "rename" || kind == "new" {
            admin_name(&data).map_err(bad)?;
        }
        // Attachments are checked before the command is admitted, so a bad one is
        // refused with the draft intact instead of failing the turn.
        if data["attachments"]
            .as_array()
            .is_some_and(|list| !list.is_empty())
        {
            if kind != "say" {
                return Err(bad("attach files to a message in an existing thread"));
            }
            self.attachments(id, &data["attachments"]).map_err(bad)?;
        }
        if kind == "update" && self.source.is_none() {
            return Err(bad("self-edit is not configured on this host"));
        }
        if kind == "new" {
            if data["dev"].as_bool().unwrap_or(false) && self.source.is_none() {
                return Err(bad("self-edit is not configured"));
            }
            // Provider, model and reasoning are checked as the worker will resolve them
            // (credential lookups may run a command, so outside every lock): a route the
            // provider cannot take is refused with its reason and the draft survives.
            new_settings(&data).map_err(bad)?;
        }
        if kind == "settings"
            && let Some(provider) = data["provider"].as_str().filter(|p| !p.is_empty())
        {
            let model = data["model"]
                .as_str()
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(String::from);
            Settings::resolve(provider, model).map_err(bad)?;
        }
        let mut db = lock(&self.db);
        if let Some(existing) = db
            .query_row("SELECT digest FROM commands WHERE id=?1", [&request], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .map_err(unknown)?
        {
            drop(db);
            if existing != digest {
                return Err("request_id reused with different payload".into());
            }
            return self.receipt(&request).map_err(unknown);
        }
        let count = |db: &Connection| -> rusqlite::Result<i64> {
            db.query_row("SELECT count(*) FROM commands", [], |r| r.get(0))
        };
        if !anytime && count(&db).map_err(unknown)? >= MAX_COMMANDS {
            let _ = prune(&db);
            if count(&db).map_err(unknown)? >= MAX_COMMANDS {
                return Err("command retention limit reached: 8,192 receipts from the last week are kept; try again later".into());
            }
        }
        let session_id = if kind == "new" {
            hash(request.as_bytes())[..20].to_string()
        } else {
            id.to_string()
        };
        let mut lives = lock(&self.lives);
        if kind == "new" && lives.len() >= MAX_SESSIONS {
            return Err("session limit reached".into());
        }
        // A rollback is the host's, not a thread's: without a `session` its events go
        // to a stream of its own (its receipt is read by request ID).
        let live = if kind == "new" || (kind == "rollback" && id.is_empty()) {
            Watch::new()
        } else {
            lives
                .get(id)
                .cloned()
                .ok_or(Refused(404, "session not found".into()))?
        };
        let upgrading = matches!(kind, "promote" | "rollback");
        if !anytime && self.updating.load(Ordering::SeqCst) {
            return Err(
                "an upgrade or restart is in progress; wait for the host to reconnect".into(),
            );
        }
        // Self-edit commands need Spindle's source in the thread at its head: a legacy
        // self-edit thread, or a checkout (selfedit.rs), checked below once the head is read.
        let self_edit = matches!(kind, "promote" | "update");
        let dev: bool = self_edit
            && db
                .query_row("SELECT dev FROM sessions WHERE id=?1", [id], |r| r.get(0))
                .map_err(err)?;
        if upgrading
            && lives
                .values()
                .filter(|v| !Arc::ptr_eq(v, &live))
                .any(|v| v.lock().busy)
        {
            return Err("all other sessions must be idle before upgrading".into());
        }
        let mut state = live.lock();
        // A settings change sent during a run waits for the turn's end (as the model's
        // `settings` tool queues it) instead of being refused.
        let queued = kind == "settings" && state.busy;
        // The app's settings change what the running turn offers, so only between turns
        // (the queued change knows only the operator's fields).
        if queued
            && [
                "tools",
                "builtin_tools",
                "instructions",
                "resend_unknown",
                "tasks",
            ]
            .iter()
            .any(|key| data.get(*key).is_some_and(|value| !value.is_null()))
        {
            return Err(
                "session busy: tools, builtin_tools, instructions, resend_unknown and \
                        tasks change only between turns"
                    .into(),
            );
        }
        if !anytime {
            if state.busy && !queued {
                return Err("session busy; wait or stop the run".into());
            }
            if !administrative
                && lives
                    .values()
                    .filter(|v| !Arc::ptr_eq(v, &live))
                    .filter(|v| v.lock().busy)
                    .count()
                    >= 4
            {
                return Err(BUSY_HOST.into());
            }
            if !matches!(kind, "new" | "rename" | "delete" | "rollback") && !queued {
                let mut view = View::open(&self.dir(id).join("store")).map_err(err)?;
                let (head, bytes, _) = view.head().map_err(err)?;
                if self_edit && selfedit::prefix_at(&self.dir(id), dev, head)?.is_none() {
                    return Err(if upgrading {
                        "this thread holds no Spindle source to install: ask it to change \
                         Spindle (it checks the source out first)"
                    } else {
                        "only a thread holding Spindle's source can be updated from the host \
                         source"
                    }
                    .into());
                }
                if self_edit
                    && (head == 0
                        || TurnContinuation::restore(&bytes)
                            .map_err(err)?
                            .control()
                            .phase()
                            != Phase::Stopped(Stop::Complete))
                {
                    return Err(if upgrading {
                        "finish the self-edit turn before promoting"
                    } else {
                        "finish the current turn before updating from the host source"
                    }
                    .into());
                }
                if data["expected_revision"].as_u64() != Some(head) {
                    return Err("session changed; refresh before submitting".into());
                }
            }
        }
        if kind == "stop" && state.installed {
            return Err("the new binary is already installed and the host is restarting; Stop no longer applies".into());
        }
        if kind == "stop" && (!state.busy || state.stopping) {
            return Err("no unstopped worker is running".into());
        }
        if kind == "dismiss_install" && selfedit::request(&self.dir(id)).is_none() {
            return Err(bad("no install request is waiting"));
        }
        let tx = db.transaction().map_err(err)?;
        if kind == "new" {
            let name = admin_name(&data)?;
            tx.execute(
                "INSERT INTO sessions VALUES(?1,?2,?3)",
                params![session_id, name, data["dev"].as_bool().unwrap_or(false)],
            )
            .map_err(err)?;
        }
        tx.execute(
            "INSERT INTO commands VALUES(?1,?2,?3,'running',NULL)",
            params![request, session_id, digest],
        )
        .map_err(err)?;
        tx.execute(
            "INSERT INTO submissions VALUES(?1,?2)",
            params![request, data.to_string()],
        )
        .map_err(err)?;
        tx.execute(
            "INSERT OR REPLACE INTO command_times VALUES(?1,?2)",
            params![request, now_ms() as i64],
        )
        .map_err(err)?;
        // From here the command may be admitted: failures are not definitive refusals.
        tx.commit().map_err(unknown)?;
        if kind == "stop" {
            agent::tasks::stop_all(&self.dir(id));
            state.stopping = true;
            if let Some(cancel) = &state.cancel {
                cancel.store(true, Ordering::SeqCst);
            }
            db.execute(
                "UPDATE commands SET state='completed' WHERE id=?1",
                [&request],
            )
            .map_err(unknown)?;
        } else if administrative {
            let outcome = if queued {
                agent::queue_settings(&self.dir(&session_id), &data)
            } else {
                self.administer(kind, &session_id, &data, &db)
            };
            let (result, error) = match outcome {
                Ok(()) => ("completed", None),
                Err(error) => ("failed", Some(error)),
            };
            let recorded = db
                .execute(
                    "UPDATE commands SET state=?2,error=?3 WHERE id=?1",
                    params![request, result, error],
                )
                .map_err(unknown);
            if kind == "delete" && result == "completed" {
                state.deleted = true;
                drop(state);
                live.changed.notify_all();
                lives.remove(&session_id);
                drop(lives);
                lock(&self.summaries).remove(&session_id);
                self.previews.forget(&session_id);
                let _ = prune(&db);
                drop(db);
                recorded?;
                return self.receipt(&request).map_err(unknown);
            }
            recorded?;
        } else {
            state.busy = true;
            state.stopping = false;
            state.installed = false;
            state.installing = kind == "promote";
            state.since = now_ms();
            state.turn_start = state.seq;
            if upgrading {
                self.updating.store(true, Ordering::SeqCst);
            }
            if kind == "new" {
                lives.insert(session_id.clone(), live.clone());
            }
            drop(state);
            drop(lives);
            drop(db);
            let host = self.clone();
            let rid = request.clone();
            std::thread::spawn(move || host.work(&session_id, &rid, data, live));
            return self.receipt(&request).map_err(unknown);
        }
        drop(state);
        drop(lives);
        drop(db);
        self.receipt(&request).map_err(unknown)
    }
    fn administer(&self, kind: &str, id: &str, data: &Value, db: &Connection) -> Result<()> {
        match kind {
            "rename" => {
                db.execute(
                    "UPDATE sessions SET name=?2 WHERE id=?1",
                    params![id, admin_name(data)?],
                )
                .map_err(err)?;
                Ok(())
            }
            "settings" => {
                let dir = self.dir(id);
                let lock = File::open(dir.join("store/owner.lock")).map_err(err)?;
                spindle_store::take_owner_lock(&lock)
                    .map_err(|_| "another spindle process owns this session")?;
                let outcome = (|| {
                    let mut settings = Settings::load(&dir)?;
                    if let Some(provider) = data["provider"].as_str().filter(|p| !p.is_empty()) {
                        let model = data["model"]
                            .as_str()
                            .map(str::trim)
                            .filter(|m| !m.is_empty())
                            .map(String::from);
                        if model
                            .as_ref()
                            .is_some_and(|m| m.len() > 120 || m.chars().any(char::is_control))
                        {
                            return Err(
                                "model must be at most 120 bytes without control characters".into(),
                            );
                        }
                        // Credentials were checked by submit, outside every lock.
                        let (provider, model) = Settings::route_of(provider, model.as_deref())?;
                        settings.provider = provider;
                        settings.model = model;
                    }
                    if let Some(network) = data["network"].as_bool() {
                        settings.network = network;
                    }
                    if let Some(toolchain) = toolchain_choice(data)? {
                        settings.toolchain = toolchain;
                    }
                    if let Some(effort) = data["effort"].as_str() {
                        settings.effort = agent::Effort::parse(effort).ok_or(
                            "effort must be default, off, low, medium, high, xhigh or max",
                        )?;
                    }
                    if let Some(compaction) = data["compaction"].as_str() {
                        settings.compaction = compaction_mode(compaction)?;
                    }
                    // A number sets the thread's dollar budget; null clears it.
                    match data.get("max_usd") {
                        None => {}
                        Some(Value::Null) => settings.max_usd = None,
                        Some(max) => {
                            settings.max_usd = Some(
                                max.as_f64()
                                    .filter(|max| max.is_finite() && *max >= 0.0)
                                    .ok_or(
                                        "max_usd must be a non-negative number of dollars, or null",
                                    )?,
                            );
                        }
                    }
                    agent::app_tools::apply_json(&mut settings, data)?;
                    // Refuse a combination the route cannot send (also after a provider change).
                    agent::describe_effort(&settings.provider, &settings.model, settings.effort)?;
                    settings.save(&dir)?;
                    // The user's choice wins over what the model queued for these fields
                    // (a provider and its model go together).
                    let mut set: Vec<&str> = data
                        .as_object()
                        .into_iter()
                        .flat_map(|fields| fields.keys().map(String::as_str))
                        .collect();
                    if set.iter().any(|key| matches!(*key, "provider" | "model")) {
                        set.extend(["provider", "model"]);
                    }
                    agent::drop_pending_settings(&dir, Some(&set))
                })();
                let _ = lock.unlock();
                outcome
            }
            "delete" => {
                let dir = self.dir(id);
                let store = dir.join("store");
                // A host stopped between registering a thread and creating its store
                // leaves no owner lock; such a thread can still be deleted.
                let lock = if store.exists() {
                    let lock = File::open(store.join("owner.lock")).map_err(err)?;
                    spindle_store::take_owner_lock(&lock)
                        .map_err(|_| "another spindle process owns this session")?;
                    Some(lock)
                } else {
                    None
                };
                // Its background jobs go first: they run over copies of its workspace.
                if store.exists() {
                    spindle_store::jobs::stop_store_jobs(&store);
                    agent::tasks::stop_all(&dir);
                }
                let outcome = (|| {
                    let trash = self.root.join("trash");
                    private_dir(&trash)?;
                    let target = trash.join(id);
                    if target.exists() {
                        return Err(
                            "a previous deletion remains in trash; operator cleanup required"
                                .into(),
                        );
                    }
                    let moved = dir.exists();
                    if moved {
                        std::fs::rename(&dir, &target).map_err(err)?;
                        // Durable before the row goes: a reverted move would leave a
                        // directory no registry row names, which nothing removes.
                        if let Err(error) = spindle_store::durable::sync_dir(&trash)
                            .and_then(|()| spindle_store::durable::sync_parent(&dir))
                        {
                            let _ = std::fs::rename(&target, &dir);
                            return Err(err(error));
                        }
                    }
                    let removed = db.execute("DELETE FROM sessions WHERE id=?1", [id]);
                    if !matches!(removed, Ok(1)) {
                        let error = match removed {
                            Ok(_) => "session registry deletion did not remove one row".into(),
                            Err(error) => err(error),
                        };
                        if moved {
                            std::fs::rename(&target, &dir).map_err(|restore| {
                                format!(
                                    "delete failed: {error}; directory restore failed: {restore}"
                                )
                            })?;
                        }
                        return Err(error);
                    }
                    // Removal can take minutes for a large build cache: never under the locks.
                    if moved {
                        std::thread::spawn(move || disk::purge(&target));
                    }
                    Ok(())
                })();
                if let Some(lock) = lock {
                    let _ = lock.unlock();
                }
                outcome
            }
            "dismiss_install" => selfedit::clear_request(&self.dir(id)),
            _ => Err("unsupported administrative command".into()),
        }
    }
    fn work(self: Arc<Self>, id: &str, request: &str, data: Value, live: Arc<Watch>) {
        // Resolved now: once an install replaces the file, /proc/self/exe reads "(deleted)".
        let exe = std::env::current_exe();
        let mut idle = Idle {
            live: &live,
            updating: &self.updating,
            restarting: false,
            done: None,
        };
        let kind = data["kind"].as_str().unwrap_or_default();
        // Steering messages a turn ended without reading and that could not be sent.
        let unsent = std::cell::RefCell::new(Vec::<String>::new());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
            let thread: Arc<dyn SessionHost> = Arc::new(selfedit::ThreadHost {
                host: self.clone(),
                id: id.to_string(),
            });
            let observer = Arc::new(Sink(live.clone(), Some(thread.clone())));
            let dir = self.dir(id);
            if matches!(kind, "promote" | "rollback") {
                // Stop may already have arrived; it is honoured between phases and
                // always before anything is installed.
                let cancel = Arc::new(AtomicBool::new(false));
                {
                    let mut state = live.lock();
                    if state.stopping {
                        cancel.store(true, Ordering::SeqCst);
                    }
                    state.cancel = Some(cancel.clone());
                }
                let expected = match kind {
                    "promote" => data["expected_revision"]
                        .as_u64()
                        .ok_or("expected revision required")?,
                    _ => 0,
                };
                return self.upgrade(id, request, kind, expected, &live, &cancel);
            }
            let mut session = if kind == "new" {
                let mut settings = new_settings(&data)?;
                let dev = data["dev"].as_bool().unwrap_or(false);
                let files = if dev {
                    let source = self.source.as_ref().ok_or("dev source unavailable")?;
                    settings.toolchain = true;
                    settings.self_edit = true;
                    settings.sources = agent::sources(&[source
                        .parent()
                        .ok_or("source parent missing")?
                        .join("dext-engine")])?;
                    agent::import_files(source)?
                        .0
                        .into_iter()
                        .filter(|(path, _, _)| {
                            !path.starts_with(".dext/") && !path.starts_with(".crew/")
                        })
                        .collect()
                } else {
                    vec![]
                };
                Session::create(&dir, settings, files, observer)?
            } else {
                // Self-edit threads made before the model was told how promotion works.
                if self.is_dev(id)
                    && let Ok(mut settings) = Settings::load(&dir)
                    && !settings.self_edit
                {
                    settings.self_edit = true;
                    settings.save(&dir)?;
                }
                Session::open(&dir, observer)?
            };
            if kind != "new"
                && session.status()?.revision
                    != data["expected_revision"]
                        .as_u64()
                        .ok_or("expected revision required")?
            {
                return Err("session changed before the writer lock was acquired; refresh".into());
            }
            let mut cancel = session.interrupt_handle();
            // Only a running turn takes messages; a rewind never reads them.
            let steers = matches!(kind, "new" | "say" | "continue" | "retry" | "update");
            let mut steering = session.steering_handle();
            let mut hurry = session.hurry_handle();
            let mut app_state = session.state_handle();
            {
                let mut state = live.lock();
                if state.stopping {
                    cancel.store(true, Ordering::SeqCst);
                }
                state.cancel = Some(cancel.clone());
                state.steering = steers.then(|| steering.clone());
                state.hurry = steers.then(|| hurry.clone());
                state.app_state = steers.then(|| app_state.clone());
            }
            if matches!(kind, "new" | "say")
                && let Some(patch) = data.get("state").filter(|patch| patch.is_object())
            {
                session.state_with_next_prompt(patch.clone())?;
            }
            // What the operator opened or wrote for this turn (attention.rs) rides in
            // the prompt itself: append-only and visible in the thread, where a note in
            // the system prompt would reset the provider's cached history twice.
            let prompt = match kind {
                "new" | "say" => {
                    let text = data["text"].as_str().unwrap_or_default();
                    let key = jobs::session_key(&dir.join("store"));
                    let mut attention = lock(&self.attention);
                    attention.compose(&key);
                    match attention.take(&key) {
                        Some(note) => format!("{text}{note}"),
                        None => text.to_string(),
                    }
                }
                _ => String::new(),
            };
            let mut outcome = match kind {
                "retry" => session.retry(),
                _ => {
                    let outcome = match kind {
                        "new" | "say" => self
                            .attachments(id, &data["attachments"])
                            .and_then(|files| session.prompt_with_files(&prompt, files)),
                        "continue" if data["until"] == "unknown" => session.resume_until_unknown(),
                        "continue" => session.resume(),
                        "task_start" => {
                            let task = data["task"]
                                .as_str()
                                .filter(|task| {
                                    (1..=80).contains(&task.len())
                                        && task.bytes().all(|byte| {
                                            byte.is_ascii_alphanumeric() || b"-_.".contains(&byte)
                                        })
                                })
                                .ok_or("task id required")?;
                            let mut input = serde_json::Map::new();
                            for key in ["prompt", "model", "effort", "files", "settings"] {
                                if let Some(value) = data.get(key) {
                                    input.insert(key.into(), value.clone());
                                }
                            }
                            let result = session.start_task(
                                task,
                                &Value::Object(input),
                                data["report"].as_str().unwrap_or("wait"),
                            )?;
                            live.push_api(
                                json!({"type":"task_started","task":task,"result":result}),
                            );
                            Ok(())
                        }
                        "state" => session.set_state(&data["patch"]).map(|_| ()),
                        "update" => self.update_from_source(id, request, &mut session, &live),
                        "rewind" => match data["revision"].as_u64() {
                            Some(revision) => session.rewind(revision),
                            None => Err("revision required".into()),
                        },
                        _ => Err("unsupported command".into()),
                    };
                    drop(session);
                    outcome
                }
            };
            // Messages sent while the turn ran that it ended before reading are the
            // next prompt, as the user meant them to be read, when the turn completed.
            // The queue is closed and drained under the live lock, where the steer
            // endpoint adds to it, so none slips in between. Whatever cannot be sent
            // goes back to the screen in the done event.
            loop {
                let rest = {
                    let mut state = live.lock();
                    state.steering = None;
                    state.hurry = None;
                    hurry.store(false, Ordering::SeqCst);
                    std::mem::take(
                        &mut *steering
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner),
                    )
                };
                if rest.is_empty() {
                    break;
                }
                if outcome.is_err() || cancel.load(Ordering::SeqCst) {
                    *unsent.borrow_mut() = rest;
                    break;
                }
                let mut next =
                    match Session::open(&dir, Arc::new(Sink(live.clone(), Some(thread.clone())))) {
                        Ok(next) => next,
                        Err(error) => {
                            *unsent.borrow_mut() = rest;
                            outcome = Err(error);
                            break;
                        }
                    };
                if !next.accepts_prompt() {
                    *unsent.borrow_mut() = rest;
                    break;
                }
                cancel = next.interrupt_handle();
                steering = next.steering_handle();
                hurry = next.hurry_handle();
                let queue = next.state_handle();
                {
                    let mut state = live.lock();
                    if state.stopping {
                        drop(state);
                        *unsent.borrow_mut() = rest;
                        break;
                    }
                    state.cancel = Some(cancel.clone());
                    state.steering = Some(steering.clone());
                    state.hurry = Some(hurry.clone());
                    // State changes go to the turn that runs now, after any the last one
                    // had not reached (moved under the live lock the route takes too).
                    let waiting = std::mem::take(
                        &mut *app_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner),
                    );
                    queue
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .extend(waiting);
                    state.app_state = Some(queue.clone());
                }
                app_state = queue;
                outcome = next.prompt(&rest.join("\n\n"));
            }
            // State changes that reached the queue after the turn's last step are kept
            // now, each a step of its own.
            let late = {
                let mut state = live.lock();
                state.app_state = None;
                std::mem::take(
                    &mut *app_state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                )
            };
            // One that cannot be kept is said so; the turn's own outcome stands.
            if !late.is_empty() {
                let sink = Arc::new(Sink(live.clone(), Some(thread.clone())));
                let refused = |reason: &str| sink.event(Event::StateRefused { reason });
                match Session::open(&dir, sink.clone()) {
                    Ok(mut next) => {
                        for patch in late {
                            if let Err(reason) = next.set_state(&patch) {
                                refused(&reason);
                            }
                        }
                    }
                    Err(reason) => refused(&reason),
                }
            }
            outcome
        }))
        .unwrap_or_else(|_| {
            Err("worker panicked; inspect committed state before continuing".into())
        });
        let (state, error) = match outcome {
            Ok(()) => ("completed", None),
            Err(error) => ("failed", Some(error)),
        };
        let db = lock(&self.db);
        let recorded = db.execute(
            "UPDATE commands SET state=?2,error=?3 WHERE id=?1",
            params![request, state, error],
        );
        // A failed create leaves no store; keep it out of the session list.
        let orphaned = kind == "new" && state == "failed" && !self.dir(id).join("store").exists();
        if orphaned {
            let _ = db.execute("DELETE FROM sessions WHERE id=?1", [id]);
            lock(&self.lives).remove(id);
        }
        drop(db);
        if orphaned {
            disk::purge(&self.dir(id));
        }
        if matches!(kind, "promote" | "rollback") {
            disk::purge(&self.root.join("promotions").join(request));
        }
        let saved = recorded.is_ok();
        let restarting = saved && state == "completed" && matches!(kind, "promote" | "rollback");
        idle.restarting = restarting;
        idle.done = Some(
            json!({"type":"done","request_id":request,"state":state,"error":error,"receipt_saved":saved,"unsent":unsent.take()}),
        );
        drop(idle);
        if orphaned {
            // Open event streams end now, after the `done` above, as for a deletion.
            live.lock().deleted = true;
            live.changed.notify_all();
        }
        if restarting {
            // If the installed binary cannot start the host, it restores this one.
            if let Ok(target) = &exe
                && let Ok(build) = spindle_store::upgrade::fingerprint(target)
            {
                let _ = spindle_store::durable::write(
                    &self.root.join(UPGRADE_MARKER),
                    build.as_bytes(),
                );
            }
            std::thread::sleep(Duration::from_secs(1));
            std::process::exit(75);
        }
    }
    fn upgrade(
        &self,
        id: &str,
        request: &str,
        kind: &str,
        expected_revision: u64,
        live: &Arc<Watch>,
        cancel: &AtomicBool,
    ) -> Result<()> {
        use spindle_store::upgrade;
        let stopped = || {
            if cancel.load(Ordering::SeqCst) {
                Err(STOPPED.to_string())
            } else {
                Ok(())
            }
        };
        let target = std::env::current_exe().map_err(err)?;
        if std::env::var("SPINDLE_SUPERVISED").as_deref() != Ok("1") {
            return Err("promotion requires SPINDLE_SUPERVISED=1 and an OS supervisor".into());
        }
        if target.parent() != Some(self.root.join("bin").as_path()) {
            return Err("promotion requires the supervised binary under HOST_DIR/bin".into());
        }
        if kind == "rollback" {
            // The host's own previous binary, whichever thread (if any) asked.
            stopped()?;
            let previous = upgrade::previous(&target);
            self.verify_candidate(&previous)?;
            install_unless_stopped(live, cancel, || upgrade::rollback(&target, false))?;
        } else {
            live.push(json!({"type":"notice","text":"Promotion builds run offline in isolation. Installation authorizes this candidate as trusted host code."}));
            let mut session = Session::open(&self.dir(id), Arc::new(Sink(live.clone(), None)))?;
            let revision = session.status()?.revision;
            if revision == 0 || revision != expected_revision {
                return Err("self-edit workspace changed before promotion; refresh".into());
            }
            stopped()?;
            let source = self.source.as_ref().ok_or("dev source unavailable")?;
            let export = self.root.join("promotions").join(request);
            private_dir(&export)?;
            let tree = export.join("source");
            // The thread forked from the host source at r1 (or checked it out into
            // spindle/), or last synced with it in an Update from host source (sync.rs);
            // the host may have moved on since. Promote the current host source plus
            // only the thread's own changes.
            let (prefix, base, since) = self.sync_base(id, revision, live)?;
            let theirs = selfedit::strip(session.files_at(revision)?, prefix);
            drop(session);
            // An emptied checkout would compose as a deletion of the host source.
            if theirs.is_empty() {
                return Err(format!(
                    "Not promoted: {} holds no files. Nothing was built or installed.",
                    if prefix.is_empty() {
                        "the workspace"
                    } else {
                        prefix
                    }
                ));
            }
            let ours = host_source_files(source)?;
            // A whole-workspace thread's other work (a site beside the source) is not
            // Spindle's and never reaches the host source.
            let owned = selfedit::spindle_owned(
                prefix,
                base.iter().chain(&ours).map(|file| file.0.as_str()),
            );
            let mut theirs = theirs;
            theirs.retain(|file| owned(&file.0));
            let composed = match compose_promotion(&base, &ours, &theirs, &export)? {
                Ok(composed) => composed,
                Err(conflicts) => {
                    live.lock().source_moved = Some((revision, conflicts.clone()));
                    live.push(json!({"type":"source_moved","paths":conflicts}));
                    return Err(source_moved_text(&conflicts));
                }
            };
            live.push(json!({"type":"notice","text":format!("Composed {} file(s): the current host source plus this thread's changes since {since}.", composed.len())}));
            for (path, bytes, executable) in &composed {
                let path = tree.join(path);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(err)?;
                }
                std::fs::write(&path, bytes).map_err(err)?;
                #[cfg(unix)]
                if *executable {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                        .map_err(err)?;
                }
                #[cfg(not(unix))]
                let _ = executable;
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(
                source
                    .parent()
                    .ok_or("source parent missing")?
                    .join("dext-engine"),
                export.join("dext-engine"),
            )
            .map_err(err)?;
            live.lock().install_failed = None;
            for args in [
                vec!["fmt", "--all", "--", "--check"],
                vec![
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--all-features",
                    "--locked",
                    "--",
                    "-D",
                    "warnings",
                ],
                vec![
                    "test",
                    "--release",
                    "--locked",
                    "--workspace",
                    "--all-features",
                    "--lib",
                    "--bins",
                ],
                vec![
                    "build",
                    "--release",
                    "--locked",
                    "-p",
                    "spindle-store",
                    "--features",
                    "cli",
                    "--bin",
                    "spindle",
                ],
            ] {
                stopped()?;
                // A gate killed by Stop fails; report the Stop, not the gate.
                self.gate(&tree, CARGO, &args, revision, live, cancel)?;
            }
            // The phone's own tests, when this host has Node: a UI that does not load
            // (one syntax error in app.js) must never be installed.
            if Path::new(NODE).is_file() {
                for test in ["store/tests/ui.mjs", "store/tests/ui_app.mjs"] {
                    stopped()?;
                    self.gate(&tree, NODE, &[test], revision, live, cancel)?;
                }
            }
            let candidate = self.root.join("promotions/cache/target/release/spindle");
            stopped()?;
            self.verify_candidate(&candidate)?;
            install_unless_stopped(live, cancel, || upgrade::install(&candidate, &target))?;
            // Installed: the request it answered is done.
            let _ = selfedit::clear_request(&self.dir(id));
            // The installed binary now carries this thread's changes; keep the host
            // source in step so the next build or promotion does not drop them.
            match write_back(source, &ours, &composed) {
                Ok(count) => {
                    if count > 0 {
                        live.push(json!({"type":"notice","text":format!("Updated {count} host source file(s) with this thread's changes.")}));
                    }
                    // The host source now holds exactly this revision's files: later
                    // promotions and updates of this thread merge against it.
                    if sync::same_files(&composed, &theirs) {
                        let recorded = self.root_at(id, revision).and_then(|root| match root {
                            Some(root) => sync::record(&self.dir(id), &root, &composed),
                            None => Ok(()),
                        });
                        if let Err(error) = recorded {
                            live.push(json!({"type":"warning","text":format!("This thread's sync with the host source was not recorded ({error}); its next promotion may report conflicts.")}));
                        }
                    }
                }
                Err(error) => live.push(json!({"type":"warning","text":format!("Installed, but the host source was not updated ({error}); a later promotion or build may drop these changes.")})),
            }
        }
        live.push(json!({"type":"notice","text":"Binary installed. Committing upgrade receipt before supervised restart."}));
        Ok(())
    }
    /// Where a self-edit thread holds Spindle's source as of `head` ("" or "spindle/",
    /// selfedit.rs), the host source it last synced with (sync.rs) and when that was:
    /// its import at r1, or a later sync (a checkout is one).
    fn sync_base(
        &self,
        id: &str,
        head: u64,
        live: &Watch,
    ) -> Result<(&'static str, Vec<SourceFile>, String)> {
        let dir = self.dir(id);
        let prefix = selfedit::prefix_at(&dir, self.is_dev(id), head)?
            .ok_or("this thread holds no Spindle source")?;
        let warn = |text: String| live.push(json!({"type":"warning","text":text}));
        let (base, since) = selfedit::base(&dir, prefix, head, warn)?;
        Ok((prefix, base, since))
    }
    /// Update from host source: merges the current host source into a self-edit
    /// thread's head against its last sync (sync.rs) and starts a turn over the merged
    /// workspace, whose prompt says what changed and which conflicts to resolve. The
    /// sync is recorded durably before the turn can commit its workspace.
    fn update_from_source(
        &self,
        id: &str,
        request: &str,
        session: &mut Session,
        live: &Watch,
    ) -> Result<()> {
        let source = self
            .source
            .as_ref()
            .ok_or("self-edit is not configured on this host")?;
        if !session.accepts_prompt() {
            return Err("finish the current turn before updating from the host source".into());
        }
        let head = session.status()?.revision;
        let scratch = self
            .root
            .join("promotions")
            .join(format!("update-{request}"));
        private_dir(&scratch)?;
        let planned = (|| -> Result<Option<(String, Vec<SourceFile>)>> {
            let (prefix, base, since) = self.sync_base(id, head, live)?;
            let mut ours = host_source_files(source)?;
            // A checkout holds no git metadata (the host's repository stays the host's).
            if !prefix.is_empty() {
                ours.retain(|file| !sync::repository(&file.0));
            }
            let workspace = session.files_at(head)?;
            let theirs = selfedit::strip(workspace.clone(), prefix);
            let update = sync::merge(&base, &ours, &theirs, &scratch)?;
            if update.unchanged(&theirs) && update.decisions.is_empty() {
                // It already holds every change in the host source; record that, so
                // promotions merge from here.
                if let Some(root) = self.root_at(id, head)? {
                    sync::record(&self.dir(id), &root, &ours)?;
                }
                live.push(json!({"type":"notice","text":"This thread already has every change in the host source; nothing to update."}));
                return Ok(None);
            }
            let files = selfedit::with_subtree(workspace, prefix, update.files.clone());
            let root = session.tree_root(&files)?;
            sync::record(&self.dir(id), &root, &ours)?;
            let open = update.conflicts.len() + update.decisions.len();
            live.push(json!({"type":"notice","text":format!(
                "Merged the host source into this thread: {} file(s) from the host, {} merged with this thread's changes, {open} to resolve. A turn now checks the result{}.",
                update.from_host.len(),
                update.merged.len(),
                if open > 0 { " and resolves the conflicts" } else { "" }
            )}));
            Ok(Some((sync::prompt(&update, &since, prefix, false), files)))
        })();
        disk::purge(&scratch);
        match planned? {
            Some((prompt, files)) => session.prompt_with_tree(&prompt, files),
            None => Ok(()),
        }
    }
    /// The workspace root of one of a thread's revisions.
    fn root_at(&self, id: &str, revision: u64) -> Result<Option<String>> {
        let mut view = View::open(&self.dir(id).join("store")).map_err(err)?;
        view.read(|view| Ok(view.at(revision)?.and_then(|(_, root)| root)))
            .map_err(err)
    }
    /// A static-site preview token for one revision of a thread (preview.rs).
    fn preview_grant(&self, id: &str, data: &Value) -> Result<Value> {
        let revision = data["revision"]
            .as_u64()
            .filter(|revision| *revision > 0)
            .ok_or("a revision (1 or more) is required")?;
        if self.root_at(id, revision)?.is_none() {
            return Err(format!("r{revision} has no workspace files"));
        }
        let github = match data.get("network") {
            None | Some(Value::Null) => false,
            Some(Value::String(mode)) if mode == "none" => false,
            Some(Value::String(mode)) if mode == "github" => true,
            Some(_) => return Err("preview network must be none or github".into()),
        };
        let (token, lasts) = self.previews.mint(id, revision, github)?;
        Ok(
            json!({"token":token,"url":format!("{}{token}/", preview::PREFIX),"revision":revision,"expires_in":lasts.as_secs(),"network":if github { "github" } else { "none" }}),
        )
    }
    fn is_dev(&self, id: &str) -> bool {
        lock(&self.db)
            .query_row("SELECT dev FROM sessions WHERE id=?1", [id], |r| r.get(0))
            .unwrap_or(false)
    }
    fn verify_candidate(&self, binary: &Path) -> Result<()> {
        let db = lock(&self.db);
        let ids = db
            .prepare("SELECT id FROM sessions")
            .map_err(err)?
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(err)?;
        drop(db);
        let sessions = ids
            .into_iter()
            .map(|id| self.dir(&id))
            .filter(|dir| dir.join("store").exists())
            .collect::<Vec<_>>();
        let (_, checks) = spindle_store::upgrade::verify(binary, &sessions)?;
        if checks.iter().any(|check| !check.passed) {
            return Err("candidate failed doctor on session copies; nothing installed".into());
        }
        // The candidate must open this host's registry and bind, on a private copy.
        let scratch = self
            .root
            .join("promotions")
            .join(format!("check-{}", &random()?[..16]));
        private_dir(&scratch)?;
        let outcome = (|| {
            let copy = scratch.join("host.sqlite");
            lock(&self.db)
                .execute(
                    "VACUUM INTO ?1",
                    [copy.to_str().ok_or("non-UTF-8 host path")?],
                )
                .map_err(err)?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options
                .open(scratch.join("pairing.key"))
                .and_then(|mut file| file.write_all(self.token.as_bytes()))
                .map_err(err)?;
            let passed = run_bounded(
                std::process::Command::new(binary)
                    .arg("--dir")
                    .arg(&scratch)
                    .args(["serve", "--check"]),
                &scratch.join("check.log"),
                Duration::from_secs(60),
            )?;
            if passed {
                Ok(())
            } else {
                let log = std::fs::read_to_string(scratch.join("check.log")).unwrap_or_default();
                Err(format!(
                    "the candidate cannot start this host ({}); nothing installed",
                    log.trim().lines().last().unwrap_or("no output")
                ))
            }
        })();
        disk::purge(&scratch);
        outcome
    }
    fn snapshot(&self, id: &str, revision: Option<u64>) -> Result<Value> {
        let live = self.live(id)?;
        let busy = live.lock().busy;
        let dir = self.dir(id);
        let settings = Settings::load(&dir).ok();
        let mut view = match View::open(&dir.join("store")) {
            Ok(view) => view,
            Err(_) if busy => {
                return Ok(
                    json!({"head":0,"revision":0,"busy":true,"messages":[],"files":[],"steps":[],"forks":[],"phase":"Starting","self_edit":null,"pending_settings":null}),
                );
            }
            // The host stopped between registering the thread and creating its store.
            Err(_) if !dir.join("store").exists() => {
                return Ok(
                    json!({"head":0,"revision":0,"busy":false,"missing":true,"complete":false,"phase":"Missing","stop":"this thread has no store (the host stopped while creating it); delete it","messages":[],"files":[],"steps":[],"forks":[],"targets":[],"self_edit":null,"pending_settings":null}),
                );
            }
            Err(e) => return Err(err(e)),
        };
        let snapshot = view.read(|view| {
            let (head, continuation, root) = view.head()?;
            let revision = revision.unwrap_or(head);
            let (bytes, root) = if revision == head { (continuation, root) } else { view.at(revision)?.ok_or(spindle_store::Error::Invalid)? };
            let mut messages = vec![];
            let mut pending = vec![];
            let mut phase = "Empty".to_string();
            let mut complete = true;
            let mut stop = None;
            let mut recorded = false;
            let mut rejected = None;
            let mut follow_up = false;
            if revision > 0 {
                let state = TurnContinuation::restore(&bytes).map_err(|_| spindle_store::Error::Incompatible)?;
                phase = format!("{:?}", state.control().phase());
                complete = state.control().phase() == Phase::Stopped(Stop::Complete);
                // A session pack's follow-up the host stopped before delivering: Continue.
                follow_up = complete && revision == head && !busy && view.pack_follow_up(state.execution()).unwrap_or(false);
                if let Phase::Stopped(kind) = state.control().phase() {
                    stop = Some(agent::stop_text(kind));
                }
                messages = state.transcript().map_err(|_| spindle_store::Error::Incompatible)?.messages;
                for effect in state.pending_effects().map_err(|_| spindle_store::Error::Incompatible)? {
                    match effect {
                        dext_transcript::turn::Effect::Tool {call,..} => pending.push(json!({"id":call.id,"name":call.name,"input":call.input})),
                        // A paid reply already saved: Continue adopts it, Retry would re-send.
                        dext_transcript::turn::Effect::Model {id,..} => {
                            recorded = view.recorded(&id)?;
                            rejected = view.rejection(&id)?.map(|(status, detail)| json!({"status":status,"detail":detail}));
                        }
                    }
                }
            }
            let files = match root { Some(root) => view.files(&root)?.into_iter().map(|f| json!({"path":f.entry.path,"digest":f.entry.object,"size":f.size})).collect::<Vec<_>>(), None => vec![] };
            // The newest steps, so a long thread lists where it is, not where it began.
            let listed = view.recent_steps(LISTED_STEPS)?;
            let forks = view.forks()?.into_iter().map(|f| json!({"revision":f.revision,"target":f.target,"abandoned":f.abandoned})).collect::<Vec<_>>();
            // Rewind points among the newest steps (History marks them) plus the viewed
            // revision, in one batch: the head is checked once and each candidate by its
            // control header. A turn halted mid-flight (budget, filter, repeated
            // incomplete responses) cannot be continued: Recover offers the newest point
            // that would re-run the model.
            let mut candidates: Vec<u64> = if busy { vec![] } else { listed.iter().rev().take(CHECKED_TARGETS).map(|s| s.revision).collect() };
            let newest = candidates.len();
            if !busy && !candidates.contains(&revision) {
                candidates.push(revision);
            }
            let eligible = view.targets(&candidates);
            let target = eligible.iter().find(|(r, _)| *r == revision).map(|(_, k)| format!("{k:?}"));
            let mut targets = vec![];
            let mut recover = None;
            for (step, kind) in eligible.iter().filter(|(r, _)| candidates[..newest].contains(r)) {
                if *kind == RewindTargetKind::BeforeModel && recover.is_none() {
                    recover = Some(*step);
                }
                targets.push(*step);
            }
            let halted = stop.is_some() && !complete && revision == head && stop != Some(agent::stop_text(Stop::Iterations));
            if !halted {
                recover = None;
            }
            let steps = listed.into_iter().map(|s| json!({"revision":s.revision,"label":s.adapter.unwrap_or(s.proposal)})).collect::<Vec<_>>();
            Ok(json!({"head":head,"revision":revision,"busy":busy,"phase":phase,"complete":complete,"follow_up":follow_up,"stop":stop,"recover":recover,"recorded":recorded,"rejected":rejected,"messages":messages,"pending":pending,"files":files,"steps":steps,"forks":forks,"target":target,"targets":targets}))
        }).map_err(err)?;
        let mut snapshot = snapshot;
        // A promotion refused at this head because the host source moved: offer Update.
        if let Some((at, paths)) = &live.lock().source_moved
            && !busy
            && snapshot["head"] == json!(at)
        {
            snapshot["source_moved"] = json!({"paths": paths});
        }
        // Session packs: their latest views, and a UI request waiting for an answer.
        if let Ok(packs) = view.read(|view| view.packs()) {
            snapshot["packs"] = packs;
        }
        if let Some(pending) = &live.lock().pack_ui {
            snapshot["pack_ui"] = pending.event.clone();
        }
        // The context gauge: the window of the thread's model, next to how large the
        // transcript has grown (compaction is sized from the same window). Read before
        // the settings block below, which takes the value.
        if let Some(settings) = settings.as_ref()
            && let Some(window) = agent::context_window(&settings.provider, &settings.model)
        {
            snapshot["context"] = json!({"window": window});
        }
        if let Some(settings) = settings {
            snapshot["provider"] = json!(settings.provider);
            snapshot["model"] = json!(settings.model);
            snapshot["isolation"] = json!(settings.isolation.as_str());
            snapshot["network"] = json!(settings.network);
            snapshot["toolchain"] = json!(settings.toolchain);
            snapshot["effort"] = json!(settings.effort.as_str());
            if let Ok((label, short)) =
                agent::describe_effort(&settings.provider, &settings.model, settings.effort)
            {
                snapshot["reasoning"] = json!({"label": label, "short": short});
            }
            snapshot["compaction"] = json!(settings.compaction.as_str());
            snapshot["settings"] = json!({
                "provider": settings.provider,
                "model": settings.model,
                "effort": settings.effort.as_str(),
                "network": settings.network,
                "toolchain": settings.toolchain,
                "compaction": settings.compaction.as_str(),
                "max_usd": settings.max_usd,
            });
        }
        // What the model queued with its settings tool, applied when the turn ends.
        snapshot["pending_settings"] = agent::pending_settings(&dir).unwrap_or(Value::Null);
        // The thread's background jobs, oldest first: id, command, how it stands.
        // What waits for the next turn: the flag in the composer shows it (a note
        // written, or files held open), and the next request reads it once.
        snapshot["attention"] =
            lock(&self.attention).summary(&jobs::session_key(&dir.join("store")));
        snapshot["jobs"] = json!(
            jobs::list(&jobs::session_key(&dir.join("store")))
                .into_iter()
                .map(|(id, command, state)| json!({
                    "id": id,
                    "command": brief(&command, 80),
                    "running": state == dext_native::strict::JobState::Running,
                    "state": jobs::describe(&state),
                }))
                .collect::<Vec<_>>()
        );
        // Spindle's source in this thread (self-edit from any thread), as of the head.
        let head = snapshot["head"].as_u64().unwrap_or(0);
        snapshot["self_edit"] = selfedit::snapshot(self, id, head, &live).unwrap_or_else(|error| {
            eprintln!("spindle serve: self-edit state of {id}: {error}");
            Value::Null
        });
        // A show call's cards on its result, and the user's words in steering messages.
        agent::annotate_display(&mut snapshot["messages"]);
        agent::annotate_steering(&mut snapshot["messages"]);
        // Messages waiting for the running turn.
        snapshot["queued"] = live.lock().queued();
        // Totals, each shown prompt's turn cost and the budget.
        agent::annotate_snapshot(&dir, &mut snapshot);
        Ok(snapshot)
    }
    /// Stores an uploaded file for a later message's attachments, by content digest,
    /// beside the thread (private; uploads older than a day are cleared on the way).
    fn upload(&self, id: &str, bytes: &[u8]) -> std::result::Result<Value, String> {
        if bytes.is_empty() || bytes.len() > MAX_UPLOAD {
            return Err(format!("a file must be 1–{MAX_UPLOAD} bytes"));
        }
        // Never recreates a thread directory a deletion has just moved away.
        let dir = self.dir(id).join("uploads");
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&dir) {
            Ok(()) => spindle_store::durable::sync_parent(&dir).map_err(err)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err("thread not found".into());
            }
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(err(error));
            }
            _ => {}
        }
        private_dir(&dir)?;
        let digest = hash(bytes);
        let mut kept = 0u64;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                let old = meta
                    .modified()
                    .ok()
                    .and_then(|time| time.elapsed().ok())
                    .is_some_and(|age| age > Duration::from_secs(86_400));
                if old {
                    let _ = std::fs::remove_file(entry.path());
                } else if entry.file_name() != digest.as_str() {
                    kept += meta.len();
                }
            }
        }
        let target = dir.join(&digest);
        // The same file again: it is already here, so it only stays a day longer (never
        // rewritten while a turn may be reading it).
        if std::fs::read(&target).is_ok_and(|saved| hash(&saved) == digest) {
            let _ = File::options()
                .write(true)
                .open(&target)
                .and_then(|file| file.set_modified(SystemTime::now()));
            return Ok(json!({"digest":digest,"size":bytes.len()}));
        }
        if kept + bytes.len() as u64 > MAX_UPLOADS {
            return Err(format!(
                "this thread's uploads from the last day already hold {}; send the attached message first",
                spindle_store::human_size(kept as usize)
            ));
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let pending = dir.join(format!("{digest}.{}.tmp", random()?));
        options
            .open(&pending)
            .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
            .and_then(|()| std::fs::rename(&pending, &target))
            .and_then(|()| spindle_store::durable::sync_dir(&dir))
            .map_err(|error| {
                let _ = std::fs::remove_file(&pending);
                err(error)
            })?;
        Ok(json!({"digest":digest,"size":bytes.len()}))
    }
    /// A message's attachments, `[{path, digest}]` of earlier uploads, read back and
    /// checked against their digests.
    fn attachments(&self, id: &str, list: &Value) -> Result<Vec<(String, Vec<u8>, bool)>> {
        let Some(list) = list.as_array() else {
            return Ok(vec![]);
        };
        if list.len() > MAX_ATTACHMENTS {
            return Err(format!("at most {MAX_ATTACHMENTS} attachments"));
        }
        let mut files = vec![];
        for item in list {
            let path = item["path"]
                .as_str()
                .filter(|p| spindle_store::workspace::valid_path(p))
                .ok_or("attachment path must be a workspace path (no empty, . or .. parts)")?;
            let digest = item["digest"]
                .as_str()
                .filter(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit()))
                .ok_or("attachment digest required")?;
            let bytes = std::fs::read(self.dir(id).join("uploads").join(digest))
                .map_err(|_| format!("the upload for {path} is gone; attach it again"))?;
            if hash(&bytes) != digest {
                return Err(format!("the upload for {path} changed; attach it again"));
            }
            files.push((path.to_string(), bytes, false));
        }
        Ok(files)
    }
    /// Thread-list status of an idle thread, cached until its store changes on disk.
    fn summary(&self, id: &str) -> Value {
        let store = self.dir(id).join("store");
        let Some(stamp) = stamp(&store) else {
            return json!({"state":"missing"});
        };
        if let Some((seen, value)) = lock(&self.summaries).get(id)
            && *seen == stamp
        {
            return value.clone();
        }
        let at = (stamp[0].1.max(stamp[1].1) / 1_000_000) as u64;
        let state = summarize(&store).unwrap_or("unreadable");
        let value = json!({"state":state,"at":at});
        lock(&self.summaries).insert(id.into(), (stamp, value.clone()));
        value
    }
    /// Registry rows with their status: running/stopping from live state, otherwise
    /// the cached store summary. No store is decoded under a lock.
    fn sessions(&self) -> Result<Vec<Value>> {
        let rows: Vec<(String, String, bool)> = {
            let db = lock(&self.db);
            let mut statement = db
                .prepare("SELECT id,name,dev FROM sessions ORDER BY rowid DESC")
                .map_err(err)?;
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .map_err(err)?
                .collect::<std::result::Result<_, _>>()
                .map_err(err)?
        };
        let running: BTreeMap<String, (bool, u64)> = lock(&self.lives)
            .iter()
            .filter_map(|(id, live)| {
                let live = live.lock();
                live.busy.then(|| (id.clone(), (live.stopping, live.since)))
            })
            .collect();
        Ok(rows
            .into_iter()
            .map(|(id, name, dev)| {
                let status = match running.get(&id) {
                    Some((true, since)) => json!({"state":"stopping","at":since}),
                    Some((false, since)) => json!({"state":"running","at":since}),
                    None => self.summary(&id),
                };
                // Spend as last counted (a list never reads stores for it).
                let usd = agent::recorded_usd(&self.dir(&id));
                json!({"id":id,"name":name,"dev":dev,"status":status,"usd":usd})
            })
            .collect())
    }
    /// Spend across the registered threads (`agent::host_usage`), read beside their
    /// owners like snapshots.
    fn usage(&self, offset: i64) -> Result<Value> {
        let rows: Vec<(String, String)> = {
            let db = lock(&self.db);
            let mut statement = db
                .prepare("SELECT id,name FROM sessions ORDER BY rowid DESC")
                .map_err(err)?;
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(err)?
                .collect::<std::result::Result<_, _>>()
                .map_err(err)?
        };
        let threads: Vec<(String, String, PathBuf)> = rows
            .into_iter()
            .map(|(id, name)| {
                let dir = self.dir(&id);
                (id, name, dir)
            })
            .collect();
        Ok(agent::host_usage(&threads, offset))
    }
    fn about(&self) -> Value {
        let (sessions, running) = {
            let lives = lock(&self.lives);
            let running = lives.values().filter(|v| v.lock().busy).count();
            (lives.len(), running)
        };
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "build": self.build.get(),
            "schema": spindle_store::CAPABILITIES.schema,
            "engine": spindle_store::turn::ENGINE,
            "dev": self.source.is_some(),
            "origin": self.origin,
            "access": self.access.get().map(access::Access::team),
            "sessions": sessions,
            "running": running,
            "uptime": self.started.elapsed().as_secs(),
            "supervised": std::env::var("SPINDLE_SUPERVISED").as_deref() == Ok("1"),
            "disk": self.disk(),
            "code_policy": code_policy_summary(),
        })
    }
    /// Disk use of sessions and trash plus free space; walking a build cache takes a
    /// moment, so an answer is reused for half a minute.
    fn disk(&self) -> Value {
        let mut cached = lock(&self.disk);
        if let Some((at, value)) = &*cached
            && at.elapsed() < Duration::from_secs(30)
        {
            return value.clone();
        }
        let space = disk::space(&self.root);
        let value = json!({
            "sessions": disk::usage(&self.root.join("sessions")),
            "trash": disk::usage(&self.root.join("trash")),
            "free": space.map(|s| s.0),
            "total": space.map(|s| s.1),
        });
        *cached = Some((Instant::now(), value.clone()));
        value
    }
    /// Restarts the supervised host (exit 75) when no worker is running.
    fn restart(&self) -> std::result::Result<(), Refused> {
        if std::env::var("SPINDLE_SUPERVISED").as_deref() != Ok("1") {
            return Err("restart needs an OS supervisor (SPINDLE_SUPERVISED=1)".into());
        }
        let lives = lock(&self.lives);
        if lives.values().any(|live| live.lock().busy) {
            return Err("a thread is running; stop it or let it finish first".into());
        }
        if self.updating.swap(true, Ordering::SeqCst) {
            return Err("an upgrade or restart is already in progress".into());
        }
        drop(lives);
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(500));
            std::process::exit(75);
        });
        Ok(())
    }
}

/// Installs only if Stop has not been requested. The check and the install happen
/// under the thread's live lock, which Stop also takes, so a Stop is either seen here
/// or refused afterwards because the binary is already installed.
fn install_unless_stopped(
    live: &Watch,
    cancel: &AtomicBool,
    install: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let mut state = live.lock();
    if cancel.load(Ordering::SeqCst) || state.stopping {
        return Err("stopped before installation; nothing was installed".into());
    }
    install()?;
    state.installed = true;
    Ok(())
}

/// Read-only summary of the host's code-mode grants; never the policy file itself.
fn code_policy_summary() -> Value {
    let path = std::env::var_os("SPINDLE_CODE_POLICY")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent()?.parent().map(|p| p.join("code-policy.json")))
        });
    let Some(path) = path.filter(|p| p.is_file()) else {
        return json!({"models":[],"tools":[]});
    };
    let policy: Value = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let models = policy["models"]
        .as_array()
        .map(|m| {
            m.iter()
                .map(|v| json!({"provider":v["provider"],"model":v["model"]}))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let tools = policy["tools"]
        .as_array()
        .map(|t| {
            t.iter()
                .map(|v| json!({"name":v["name"],"risk":v["risk"],"protocol":v["protocol"]}))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({"models":models,"tools":tools,"limits":spindle_store::code_mode::Limits::from_policy(&policy).to_json()})
}

type SourceFile = (String, Vec<u8>, bool);

/// The host source a self-edit thread would import today.
fn host_source_files(source: &Path) -> Result<Vec<SourceFile>> {
    Ok(agent::import_files(source)?
        .0
        .into_iter()
        .filter(|(path, _, _)| !path.starts_with(".dext/") && !path.starts_with(".crew/"))
        .collect())
}

/// Three-way merge per file: `base` is the host source the thread last synced with
/// (its import, or an Update from host source), `ours` the current host source,
/// `theirs` the thread's head. Files only one side changed take that side; text both
/// changed goes through `git merge-file`; any conflict refuses the promotion (the inner
/// error lists the files) so a stale self-edit can never silently revert host upgrades.
fn compose_promotion(
    base: &[SourceFile],
    ours: &[SourceFile],
    theirs: &[SourceFile],
    scratch: &Path,
) -> Result<std::result::Result<Vec<SourceFile>, Vec<String>>> {
    let index = |files: &[SourceFile]| {
        files
            .iter()
            .map(|(path, bytes, executable)| (path.clone(), (bytes.clone(), *executable)))
            .collect::<BTreeMap<_, _>>()
    };
    let (base, ours, theirs) = (index(base), index(ours), index(theirs));
    let mut paths: Vec<&String> = base
        .keys()
        .chain(ours.keys())
        .chain(theirs.keys())
        .collect();
    paths.sort();
    paths.dedup();
    let mut out = vec![];
    let mut conflicts = vec![];
    for path in paths {
        let (b, o, t) = (base.get(path), ours.get(path), theirs.get(path));
        // Git metadata is the host's: never the thread's change to promote.
        let chosen = if sync::repository(path) || t == b || o == t {
            o.cloned()
        } else if o == b {
            t.cloned()
        } else {
            match (b, o, t) {
                (Some(b), Some(o), Some(t)) => {
                    match sync::merge_file(&b.0, &o.0, &t.0, scratch, false)? {
                        sync::Merged::Clean(bytes) => Some((bytes, o.1 || t.1)),
                        sync::Merged::Conflicted(_) | sync::Merged::NotText => {
                            conflicts.push(path.clone());
                            continue;
                        }
                    }
                }
                _ => {
                    conflicts.push(path.clone());
                    continue;
                }
            }
        };
        if let Some((bytes, executable)) = chosen {
            out.push((path.clone(), bytes, executable));
        }
    }
    if !conflicts.is_empty() {
        return Ok(Err(conflicts));
    }
    Ok(Ok(out))
}

/// Why a promotion stopped when the host source changed the same files as the thread.
fn source_moved_text(conflicts: &[String]) -> String {
    const SHOWN: usize = 6;
    let mut files = conflicts
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if conflicts.len() > SHOWN {
        files.push_str(&format!(" and {} more", conflicts.len() - SHOWN));
    }
    format!(
        "Not promoted: the host source changed since this thread last synced with it, and both changed {files}. Nothing was built or installed. Tap Update on the Spindle change card (or type \"update from host source\") to merge the current source into this thread, then Install again once its turn has resolved the conflicts."
    )
}

/// Applies the composed tree to the host source: changed and new files are written,
/// files the composition dropped are removed. Paths come from the validated import.
fn write_back(source: &Path, ours: &[SourceFile], composed: &[SourceFile]) -> Result<usize> {
    // The build takes minutes; never overwrite host edits made meanwhile.
    if host_source_files(source)? != ours {
        return Err("the host source changed during the promotion".into());
    }
    let before: BTreeMap<&str, (&[u8], bool)> = ours
        .iter()
        .map(|(path, bytes, executable)| (path.as_str(), (bytes.as_slice(), *executable)))
        .collect();
    let mut changed = 0;
    for (path, bytes, executable) in composed {
        if before.get(path.as_str()) == Some(&(bytes.as_slice(), *executable)) {
            continue;
        }
        let target = source.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(err)?;
        }
        // Each file whole (old or new) after a power cut; the tree as a whole is not.
        spindle_store::durable::write(&target, bytes).map_err(err)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if *executable { 0o755 } else { 0o644 };
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                .map_err(err)?;
        }
        changed += 1;
    }
    let kept: std::collections::BTreeSet<&str> = composed.iter().map(|f| f.0.as_str()).collect();
    for path in before.keys().filter(|path| !kept.contains(*path)) {
        std::fs::remove_file(source.join(path)).map_err(err)?;
        spindle_store::durable::sync_parent(&source.join(path)).map_err(err)?;
        changed += 1;
    }
    Ok(changed)
}

/// Cargo and Node as the promotion gates run them in the sandbox.
const CARGO: &str = "/opt/cargo/bin/cargo";
const NODE: &str = "/usr/bin/node";

/// Runs one gate program in the sandbox over `tree`. Err carries the message and the
/// end of the program's output (empty when it never ran).
fn sandboxed_gate(
    tree: &Path,
    program: &str,
    args: &[&str],
    live: &Watch,
    cancel: &AtomicBool,
) -> std::result::Result<(), (String, String)> {
    let failed = |error: String| (error, String::new());
    use dext_native::strict::{FileValue, RunOptions, StrictExecutor, Toolchain};
    let helper = Path::new("/usr/bin/bwrap");
    let runner = StrictExecutor::open(
        helper,
        Sha256::digest(std::fs::read(helper).map_err(|e| failed(err(e)))?).into(),
    )
    .map_err(|e| failed(err(e)))?;
    let files = agent::import_files(tree)
        .map_err(failed)?
        .0
        .into_iter()
        .map(|(path, bytes, executable)| (path, FileValue { bytes, executable }))
        .collect();
    let home =
        PathBuf::from(std::env::var_os("HOME").ok_or_else(|| failed("HOME missing".into()))?);
    let rustup = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".rustup"));
    let cargo = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cargo"));
    // One build cache for every promotion; exports are removed after each attempt.
    let cache = tree
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| failed("missing promotion directory".into()))?
        .join("cache");
    private_dir(&cache).map_err(failed)?;
    let sources = agent::sources(&[tree
        .parent()
        .ok_or_else(|| failed("missing source parent".into()))?
        .join("dext-engine")])
    .map_err(failed)?;
    let name = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    let command = format!("{name} {}", args.join(" "));
    live.push(json!({"type":"tool","id":format!("gate-{name}-{}",args[0]),"name":"bash","input":{"command":command}}));
    let observe = |stream, bytes: &[u8]| {
        live.push(json!({"type":"output","stream":stream,"data":base64(bytes)}))
    };
    let argv = std::iter::once(program.to_string())
        .chain(args.iter().map(|s| s.to_string()))
        .collect::<Vec<_>>();
    let output = runner
        .execute_with(
            &files,
            &argv,
            Duration::from_secs(600),
            cancel,
            RunOptions {
                network: false,
                observe: Some(&observe),
                local_ipc: true,
                toolchain: Some(Toolchain {
                    rustup_home: &rustup,
                    cargo_home: &cargo,
                    cache: &cache,
                    sources: &sources,
                }),
            },
        )
        .map_err(|e| failed(err(e)))?;
    let success = output.success();
    let mut tail = String::new();
    if !success {
        let details = format!(
            "{}{}",
            String::from_utf8_lossy(output.stdout()),
            String::from_utf8_lossy(output.stderr())
        );
        tail = details
            .chars()
            .rev()
            .take(GATE_OUTPUT)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        live.push(json!({"type":"notice","text":tail}));
    }
    live.push(json!({"type":"result","content":if success {"Sandboxed build gate passed"} else {"Sandboxed build gate failed; nothing installed"},"failed":!success}));
    if success {
        Ok(())
    } else {
        Err((format!("{command} failed"), tail))
    }
}

fn handle(host: &Arc<Host>, stream: &mut TcpStream) {
    // A Set-Cookie (device token issued or renewed) that every reply must carry.
    let mut extra = String::new();
    if let Err(Refused(status, error)) = route(host, stream, &mut extra) {
        let _ = send_json(stream, status, &json!({"error":error}), &extra);
    }
}
fn route(
    host: &Arc<Host>,
    stream: &mut TcpStream,
    extra: &mut String,
) -> std::result::Result<(), Refused> {
    let mut request = http::read(stream).map_err(bad)?;
    let local_host = request
        .headers
        .get("host")
        .ok_or_else(|| bad("host required"))?;
    let public = host
        .origin
        .as_ref()
        .is_some_and(|origin| origin.strip_prefix("https://") == Some(local_host.as_str()));
    let local = local_host
        .split(':')
        .next()
        .is_some_and(|s| matches!(s, "localhost" | "127.0.0.1"));
    if !public && !local {
        return Err(Refused(403, "unrecognized host".into()));
    }
    let origin = if public {
        host.origin.clone().unwrap_or_default()
    } else {
        format!("http://{local_host}")
    };
    // A preview's own requests come from its sandboxed (opaque-origin) document, which
    // browsers report as cross-site; the token in the path authorizes them (preview.rs).
    let preview = request.path.starts_with(preview::PREFIX);
    if !preview
        && request
            .headers
            .get("sec-fetch-site")
            .is_some_and(|s| s == "cross-site")
    {
        return Err(Refused(403, "cross-site request refused".into()));
    }
    // Through the tunnel, Cloudflare Access must have let the request in.
    if public && let Some(access) = host.access.get() {
        access.check(&request.headers).map_err(|why| {
            // A preview path holds its token: never logged.
            let path = if preview {
                "/api/preview/…"
            } else {
                request.path.as_str()
            };
            eprintln!("spindle serve: refused {} {path}: {why}", request.method);
            Refused(403, why)
        })?;
    }
    if preview {
        return preview::serve(host, stream, &request, &origin);
    }
    if v1::path(&request.path) {
        return v1::route(host, stream, request, extra, public, &origin);
    }
    if request.method == "POST"
        && (request.headers.get("origin") != Some(&origin)
            || request.headers.get("x-spindle").map(String::as_str) != Some("1"))
    {
        return Err(Refused(403, "same-origin command required".into()));
    }
    if let Some((mime, body)) = ui_file(&request.path, &request.query) {
        http::respond(stream, 200, mime, &body, "").map_err(err)?;
        return Ok(());
    }
    if request.path == "/api/pair" && request.method == "POST" {
        http::read_body(stream, &mut request).map_err(bad)?;
        let data: Value = serde_json::from_slice(&request.body).map_err(|e| bad(err(e)))?;
        let key = data["key"].as_str().unwrap_or_default();
        if !auth::same(key.as_bytes(), host.token.as_bytes()) {
            return Err(Refused(401, "Invalid pairing key".into()));
        }
        let label = auth::label(request.headers.get("user-agent").map(String::as_str));
        let (_, token) = host.pair(&label)?;
        http::respond(
            stream,
            200,
            "application/json",
            b"{}",
            &auth::set_cookie(&token, public),
        )
        .map_err(err)?;
        return Ok(());
    }
    let device = match auth::cookie(request.headers.get("cookie")) {
        Some(token) => host.authenticate(
            token,
            request.headers.get("user-agent").map(String::as_str),
            public,
            extra,
        )?,
        None => None,
    };
    let Some(device) = device else {
        return Err(Refused(401, "Pair this device first".into()));
    };
    // The body is read only for a paired device (an upload can be 8 MiB).
    if request.method == "POST" {
        http::read_body(stream, &mut request).map_err(bad)?;
    }
    let request = request;
    let json_body = || -> std::result::Result<Value, Refused> {
        serde_json::from_slice(&request.body).map_err(|e| bad(err(e)))
    };
    let get = request.method == "GET";
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/api/sessions") => {
            let rows = host.sessions()?;
            let providers = agent::providers()?
                .into_iter()
                .filter_map(|(id, r)| r.ok().map(|(_, model)| json!({"id":id,"model":model})))
                .collect::<Vec<_>>();
            return send_json(
                stream,
                200,
                &json!({"sessions":rows,"providers":providers,"dev":host.source.is_some(),"boot":host.boot}),
                extra,
            );
        }
        ("GET", "/api/ping") => {
            return send_json(
                stream,
                200,
                &json!({"boot":host.boot,"build":host.build.get()}),
                extra,
            );
        }
        // Forgetting a pairing revokes this screen's token on the host, not just the cookie.
        ("POST", "/api/unpair") => {
            host.revoke(&device)?;
            http::respond(
                stream,
                200,
                "application/json",
                b"{}",
                &auth::clear_cookie(public),
            )
            .map_err(err)?;
            return Ok(());
        }
        ("GET", "/api/host") => return send_json(stream, 200, &host.about(), extra),
        // Spend of every thread: today (the screen's day: `tz` minutes east of UTC),
        // the last 30 days and all time.
        ("GET", "/api/usage") => {
            let offset = request
                .query
                .get("tz")
                .and_then(|tz| tz.parse::<i64>().ok())
                .filter(|tz| tz.abs() <= 18 * 60)
                .unwrap_or(0);
            return send_json(stream, 200, &host.usage(offset)?, extra);
        }
        ("GET", "/api/devices") => return send_json(stream, 200, &host.devices(&device)?, extra),
        ("POST", "/api/devices") => {
            let data = json_body()?;
            let id = data["id"].as_str().unwrap_or_default();
            if data["action"] != "revoke" || !host.revoke(id)? {
                return Err(Refused(404, "unknown device".into()));
            }
            let cookie = if id == device {
                auth::clear_cookie(public)
            } else {
                extra.clone()
            };
            let listed = host.devices(&device)?;
            return send_json(stream, 200, &listed, &cookie);
        }
        ("POST", "/api/restart") => {
            host.restart()?;
            return send_json(stream, 202, &json!({"restarting":true}), extra);
        }
        ("GET", "/skin.css") => {
            let css = host.skins.served(request.query.contains_key("staged"));
            http::respond(
                stream,
                200,
                "text/css; charset=utf-8",
                css.as_bytes(),
                extra,
            )
            .map_err(err)?;
            return Ok(());
        }
        ("GET", "/api/skin") => return send_json(stream, 200, &host.skins.state(), extra),
        ("POST", "/api/skin") => {
            let data = json_body()?;
            let text = |key: &str| data[key].as_str().unwrap_or_default().to_string();
            let outcome = match data["action"].as_str() {
                Some("stage") if data["session"].is_string() => {
                    let id = text("session");
                    if !id_ok(&id) || host.live(&id).is_err() {
                        Err("unknown thread".to_string())
                    } else {
                        let revision = data["revision"]
                            .as_u64()
                            .ok_or_else(|| bad("revision required"))?;
                        host.workspace_file(&id, revision, skin::WORKSPACE_FILE)
                            .map_err(|_| format!("r{revision} has no {}", skin::WORKSPACE_FILE))
                            .and_then(|bytes| String::from_utf8(bytes).map_err(err))
                            .and_then(|css| host.skins.stage(&css))
                    }
                }
                Some("stage") => host.skins.stage(&text("css")),
                Some("apply") => host.skins.apply(&text("digest")),
                Some("discard") => host.skins.discard(),
                Some("restore") => host.skins.restore(&text("id")),
                Some("off") => host.skins.off(),
                _ => Err("unknown skin action".into()),
            };
            return match outcome {
                Ok(state) => send_json(stream, 200, &state, extra),
                Err(error) => Err(bad(error)),
            };
        }
        ("POST", "/api/commands") => {
            let data = json_body()?;
            let id = data["session"].as_str().unwrap_or_default().to_string();
            let receipt = host.submit(&id, data)?;
            return send_json(stream, 202, &receipt, extra);
        }
        // Sign-in is Dext's own (same flows and credential store as `dext auth`). A
        // credential travels only in this request: never logged, admitted or retained.
        // Providers and models for the picker, from the shared catalog (no network).
        ("GET", "/api/models") => {
            return send_json(stream, 200, &agent::model_catalog().map_err(bad)?, extra);
        }
        ("GET", "/api/logins") => {
            let logins = agent::logins()
                .map_err(bad)?
                .into_iter()
                .map(|l| json!({"provider":l.provider,"name":l.name,"status":l.status,"signed_in":l.signed_in}))
                .collect::<Vec<_>>();
            return send_json(stream, 200, &json!({"logins":logins}), extra);
        }
        ("POST", "/api/login") => {
            let data = json_body()?;
            let provider = data["provider"]
                .as_str()
                .ok_or_else(|| bad("provider required"))?;
            let credential = data["credential"].as_str().filter(|c| !c.trim().is_empty());
            if credential.is_some_and(|c| c.len() > 64 * 1024) {
                return Err(bad("credential too long"));
            }
            let (message, awaiting) = agent::login_remote(provider, credential).map_err(bad)?;
            return send_json(
                stream,
                200,
                &json!({"message":message,"awaiting":awaiting}),
                extra,
            );
        }
        ("POST", "/api/logout") => {
            let data = json_body()?;
            let provider = data["provider"]
                .as_str()
                .ok_or_else(|| bad("provider required"))?;
            let message = agent::logout(provider).map_err(bad)?;
            return send_json(stream, 200, &json!({"message":message}), extra);
        }
        _ => {}
    }
    if request.method == "POST"
        && let Some(rest) = request.path.strip_prefix("/api/sessions/")
        && let Some((id, "pack-ui")) = rest.split_once('/')
    {
        if !id_ok(id) || host.live(id).is_err() {
            return Err(Refused(404, "thread not found".into()));
        }
        let live = host.live(id)?;
        pack_ui::answer(&live, &json_body()?)?;
        return send_json(stream, 200, &json!({"answered":true}), extra);
    }
    if request.method == "POST"
        && let Some(rest) = request.path.strip_prefix("/api/sessions/")
        && let Some((id, action)) = rest.split_once('/')
        && matches!(action, "steer" | "upload" | "preview" | "attention")
    {
        if !id_ok(id) || host.live(id).is_err() {
            return Err(Refused(404, "thread not found".into()));
        }
        if action == "upload" {
            let saved = host.upload(id, &request.body).map_err(bad)?;
            return send_json(stream, 200, &saved, extra);
        }
        if action == "preview" {
            let granted = host.preview_grant(id, &json_body()?).map_err(bad)?;
            return send_json(stream, 200, &granted, extra);
        }
        if action == "attention" {
            // What the operator opened or wrote: advisory, so a post that cannot be
            // parsed is dropped, not refused.
            let data = json_body()?;
            let key = jobs::session_key(&host.dir(id).join("store"));
            let mut state = lock(&host.attention);
            if let Some(text) = data["text"].as_str() {
                state.write(&key, text);
            } else if data["clear"].as_bool() == Some(true) {
                state.clear(&key);
            } else if let Some((kind, name, what)) = attention::parse(&data) {
                state.note(&key, kind, &name, what);
            }
            return send_json(stream, 200, &json!({"noted": true}), extra);
        }
        let data = json_body()?;
        // `now` cuts the reply being written so the queued messages are read first; it
        // may come with a message or alone, for those already waiting.
        let now = data["now"].as_bool() == Some(true);
        let text = match data["text"].as_str() {
            None if now => None,
            text => Some(
                text.filter(|t| !t.trim().is_empty() && t.len() <= 16 * 1024)
                    .ok_or_else(|| bad("message must be 1–16384 bytes"))?,
            ),
        };
        let live = host.live(id)?;
        live.steer(text, now)
            .map_err(|(status, _, text)| Refused(status, text))?;
        // What waits now, as the `queued` event says (which may reach the screen first).
        let waiting = live.lock().queued();
        return send_json(
            stream,
            202,
            &json!({"queued":true,"now":now,"waiting":waiting}),
            extra,
        );
    }
    if let Some(id) = request.path.strip_prefix("/api/receipts/")
        && get
    {
        let exists: bool = lock(&host.db)
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM commands WHERE id=?1)",
                [id],
                |r| r.get(0),
            )
            .map_err(unknown)?;
        if !exists {
            return Err(Refused(404, "request ID not admitted".into()));
        }
        return send_json(stream, 200, &host.receipt(id).map_err(unknown)?, extra);
    }
    // The operator's shelf of cross-thread notes (attention.rs's sibling on disk):
    // listed, read, written and deleted from the settings sheet, and files kept from
    // a thread's workspace by the viewer.
    if request.method == "GET" && request.path == "/api/shelf" {
        return send_json(stream, 200, &json!({"files": shelf_files(host)?}), extra);
    }
    if request.method == "GET" && request.path == "/api/shelf/file" {
        let name = request
            .query
            .get("name")
            .ok_or_else(|| bad("name required"))?;
        let text = std::fs::read_to_string(shelf_path(host, name).map_err(bad)?)
            .map_err(|_| Refused(404, "no such note".into()))?;
        return send_json(stream, 200, &json!({"name": name, "content": text}), extra);
    }
    if request.method == "POST" && request.path == "/api/shelf" {
        let data = json_body()?;
        if data["action"].as_str() == Some("delete") {
            let name = data["name"].as_str().ok_or_else(|| bad("name required"))?;
            std::fs::remove_file(shelf_path(host, name).map_err(bad)?)
                .map_err(|_| Refused(404, "no such note".into()))?;
            return send_json(stream, 200, &json!({"files": shelf_files(host)?}), extra);
        }
        let name = data["name"].as_str().ok_or_else(|| bad("name required"))?;
        let content = data["content"]
            .as_str()
            .ok_or_else(|| bad("content required"))?;
        if content.len() > SHELF_FILE_MAX {
            return Err(Refused(413, "a note is at most 256 KiB".into()));
        }
        let path = shelf_path(host, name).map_err(bad)?;
        std::fs::create_dir_all(host.root.join("shelf")).map_err(unknown)?;
        let files = shelf_files(host)?;
        if !files.iter().any(|file| file["name"] == json!(name)) && files.len() >= SHELF_FILES_MAX {
            return Err(Refused(413, "the shelf holds at most 64 notes".into()));
        }
        spindle_store::durable::write(&path, content.as_bytes()).map_err(unknown)?;
        return send_json(stream, 200, &json!({"files": shelf_files(host)?}), extra);
    }
    if request.method == "POST"
        && let Some(rest) = request.path.strip_prefix("/api/sessions/")
        && let Some((id, "shelf")) = rest.split_once('/')
    {
        if !id_ok(id) || host.live(id).is_err() {
            return Err(Refused(404, "thread not found".into()));
        }
        let kept = host.keep_on_shelf(id, &json_body()?).map_err(bad)?;
        return send_json(stream, 200, &kept, extra);
    }
    let parts: Vec<_> = request.path.trim_matches('/').split('/').collect();
    if parts.len() != 4 || parts[0] != "api" || parts[1] != "sessions" || !get {
        return Err(Refused(404, "not found".into()));
    }
    let id = parts[2];
    // A deleted (or never created) thread is gone for good: 404, so an EventSource
    // stops reconnecting instead of retrying forever.
    if !id_ok(id) || host.live(id).is_err() {
        return Err(Refused(404, "thread not found".into()));
    }
    let revision = request
        .query
        .get("revision")
        .map(|s| s.parse::<u64>())
        .transpose()
        .map_err(|e| bad(err(e)))?;
    match parts[3] {
        "snapshot" => send_json(stream, 200, &host.snapshot(id, revision)?, extra),
        "file" => {
            let text = host
                .workspace_file(
                    id,
                    revision.ok_or_else(|| bad("revision required"))?,
                    request
                        .query
                        .get("path")
                        .ok_or_else(|| bad("path required"))?,
                )
                .map_err(|e| Refused(404, e))?;
            // render=1: an HTML file for the preview frame, under its own sandbox policy.
            if request.query.get("render").map(String::as_str) == Some("1") {
                http::headers_with(
                    stream,
                    200,
                    "text/html; charset=utf-8",
                    &format!("Content-Disposition: inline\r\n{extra}"),
                    Some(text.len()),
                    http::PREVIEW_POLICY,
                )
                .and_then(|()| stream.write_all(&text))
                .map_err(err)?;
                return Ok(());
            }
            // Raster images by their type (nosniff keeps anything else inert); every other
            // file, SVG and HTML included, is plain text here: those render only through
            // a preview, under its own sandbox policy.
            let path = request.query.get("path").map(String::as_str).unwrap_or("");
            let mime = match agent::kind_of(path) {
                Some("image") => preview::mime(path),
                _ => "text/plain; charset=utf-8",
            };
            let mime = if mime == "application/octet-stream" {
                "text/plain; charset=utf-8"
            } else {
                mime
            };
            http::respond(
                stream,
                200,
                mime,
                &text,
                &format!("Content-Disposition: inline\r\n{extra}"),
            )
            .map_err(err)?;
            Ok(())
        }
        "files" => {
            let revision = revision.ok_or_else(|| bad("revision required"))?;
            let files = host
                .files_at(id, revision)
                .map_err(|e| Refused(404, e))?
                .into_iter()
                .map(|f| json!({"path":f.entry.path,"digest":f.entry.object,"size":f.size}))
                .collect::<Vec<_>>();
            send_json(
                stream,
                200,
                &json!({"revision":revision,"files":files}),
                extra,
            )
        }
        "archive" => archive(
            host,
            stream,
            id,
            revision.ok_or_else(|| bad("revision required"))?,
            extra,
        ),
        "events" => events(host, stream, id, &request, extra, &device),
        _ => Err(Refused(404, "not found".into())),
    }
}
fn send_json(
    stream: &mut TcpStream,
    status: u16,
    data: &Value,
    extra: &str,
) -> std::result::Result<(), Refused> {
    http::respond(
        stream,
        status,
        "application/json",
        data.to_string().as_bytes(),
        extra,
    )
    .map_err(|e| Refused(500, err(e)))
}
/// What a promotion stopped before it installed reports.
const STOPPED: &str = "stopped before installation; nothing was installed";
/// How much of a failed gate's output is kept (its end, where the errors are).
const GATE_OUTPUT: usize = 6000;
/// A shelf note's size cap.
const SHELF_FILE_MAX: usize = 256 * 1024;
/// How many notes the shelf holds.
const SHELF_FILES_MAX: usize = 64;

/// One line of a text, at most `max` characters, for commands and paths shown in
/// lists where the whole thing would only be noise.
fn brief(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    let mut short: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        short.push('…');
    }
    short
}

/// A shelf note's path, from a name the client may not trust: letters, digits,
/// space, dot, dash and underscore, not starting with a dot, one path segment.
fn shelf_path(host: &Host, name: &str) -> Result<PathBuf> {
    let name = name.trim();
    let valid = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '-' | '_'));
    if !valid {
        return Err(
            "a note's name is 1–64 characters: letters, digits, space, dot, dash, underscore"
                .into(),
        );
    }
    Ok(host.root.join("shelf").join(name))
}

/// The shelf's notes, by name, with size and modified time; an empty shelf is no
/// error (it is only not there yet).
fn shelf_files(host: &Host) -> Result<Vec<Value>> {
    let dir = host.root.join("shelf");
    let mut files = vec![];
    if !dir.is_dir() {
        return Ok(files);
    }
    for entry in std::fs::read_dir(&dir).map_err(err)? {
        let entry = entry.map_err(err)?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let size = entry.metadata().map_or(0, |meta| meta.len());
        let modified = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_millis() as u64);
        files.push(json!({"name": name, "size": size, "modified": modified}));
    }
    files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(files)
}

/// Streams the workspace of a revision as an uncompressed ustar archive. File bytes are
/// read one object at a time; a failure after the headers ends the connection, and
/// the client sees a truncated download rather than a wrong one.
fn archive(
    host: &Host,
    stream: &mut TcpStream,
    id: &str,
    revision: u64,
    extra: &str,
) -> std::result::Result<(), Refused> {
    // Revision 0, before the first turn, is an empty workspace.
    let (files, tree) = match revision {
        0 => (vec![], Default::default()),
        _ => (
            host.files_at(id, revision).map_err(|e| Refused(404, e))?,
            host.tree_at(id, revision).map_err(|e| Refused(404, e))?,
        ),
    };
    let mut view = View::open(&host.dir(id).join("store")).map_err(err)?;
    let state = view
        .read(|view| view.app_state(spindle_store::view::SCOPE, revision))
        .map_err(err)?;
    let name = format!("spindle-{}-r{revision}.tar", &id[..id.len().min(8)]);
    http::headers(
        stream,
        200,
        "application/x-tar",
        &format!("Content-Disposition: attachment; filename=\"{name}\"\r\n{extra}"),
        None,
    )
    .map_err(err)?;
    let mtime = now_ms() / 1000;
    for file in files {
        let Ok(bytes) = view.object(&file.entry.object) else {
            return Ok(());
        };
        let mut out = tar::header(
            &format!("workspace/{}", file.entry.path),
            bytes.len() as u64,
            file.entry.executable,
            mtime,
        );
        out.extend_from_slice(&bytes);
        out.resize(out.len() + tar::padding(bytes.len()), 0);
        if stream.write_all(&out).is_err() {
            return Ok(());
        }
    }
    let mut rest = vec![];
    for directory in &tree.directories {
        rest.extend(tar::entry(
            &format!("workspace/{directory}"),
            tar::Kind::Directory,
            mtime,
        ));
    }
    for link in &tree.links {
        rest.extend(tar::entry(
            &format!("workspace/{}", link.path),
            tar::Kind::Link(&link.target),
            mtime,
        ));
    }
    rest.extend(tar::header("state.json", state.len() as u64, false, mtime));
    rest.extend_from_slice(&state);
    rest.resize(rest.len() + tar::padding(state.len()), 0);
    rest.extend_from_slice(&tar::END);
    let _ = stream.write_all(&rest);
    Ok(())
}
fn events(
    host: &Arc<Host>,
    stream: &mut TcpStream,
    id: &str,
    request: &http::Request,
    extra: &str,
    device: &str,
) -> std::result::Result<(), Refused> {
    let live = host.live(id).map_err(|e| Refused(404, e))?;
    http::headers(
        stream,
        200,
        "text/event-stream",
        &format!("X-Accel-Buffering: no\r\n{extra}"),
        None,
    )
    .map_err(err)?;
    // From here every outcome is written into the stream; nothing returns an error.
    let previous = request
        .headers
        .get("last-event-id")
        .or_else(|| request.query.get("after"));
    let mut cursor = previous
        .and_then(|s| s.split_once(':'))
        .filter(|(boot, _)| *boot == host.boot)
        .and_then(|(_, seq)| seq.parse::<u64>().ok());
    let started = Instant::now();
    let mut heartbeat = Instant::now();
    // Background jobs are told as their own events, so a quiet thread still hears a
    // job begin or end (the client knocks on an end). Tracked by id and state.
    // A stream starts from the jobs as they stand (the snapshot lists them): only a
    // change after it connected is announced, not every job again on each reconnect.
    let job_key = jobs::session_key(&host.dir(id).join("store"));
    let mut jobs_seen: Vec<(String, String)> = jobs::list(&job_key)
        .into_iter()
        .map(|(id, _, state)| (id, jobs::describe(&state)))
        .collect();
    while started.elapsed() < Duration::from_secs(300) {
        let (seq, first, batch, deleted) = {
            let live = live.wait(cursor, heartbeat + HEARTBEAT);
            let batch = live
                .events
                .iter()
                .filter(|(n, _)| cursor.is_some_and(|c| *n > c))
                .cloned()
                .collect::<Vec<_>>();
            (
                live.seq,
                live.events.front().map(|e| e.0).unwrap_or(live.seq + 1),
                batch,
                live.deleted,
            )
        };
        let mut out = String::new();
        if !deleted && cursor.is_none_or(|c| c > seq || c.saturating_add(1) < first) {
            // A fresh subscriber during a run replays the retained events of the
            // current command, so a reopened phone shows what already happened.
            let (busy, start) = {
                let live = live.lock();
                (live.busy, live.turn_start)
            };
            let replay = cursor.is_none() && busy && start.saturating_add(1) >= first;
            // The snapshot is labelled with the ring position read before it; if events
            // arrived while it was built, build it again so they are neither shown twice
            // (once committed, once as a preview) nor skipped.
            let mut attempts = 0;
            let (snapshot, at) = loop {
                let before = live.lock().seq;
                let snapshot = host.snapshot(id, None);
                attempts += 1;
                if snapshot.is_err() || live.lock().seq == before || attempts == 3 {
                    break (snapshot, before);
                }
            };
            let mut data = match snapshot {
                Ok(data) => data,
                Err(error) => {
                    // An unreadable thread: say so and end, rather than let the client
                    // reconnect into the same failure forever.
                    let _ = write!(
                        stream,
                        "event: failure\ndata: {}\n\n",
                        json!({ "error": error })
                    );
                    return Ok(());
                }
            };
            data["replay"] = json!(replay);
            out.push_str(&format!(
                "id: {}:{at}\nevent: snapshot\ndata: {data}\n\n",
                host.boot
            ));
            cursor = Some(if replay { start } else { at });
        } else {
            for (n, text) in batch {
                out.push_str(&format!(
                    "id: {}:{n}\nevent: preview\ndata: {text}\n\n",
                    host.boot
                ));
                cursor = Some(n);
            }
        }
        if heartbeat.elapsed() >= HEARTBEAT {
            // A revoked screen loses its open stream too.
            if !host.paired(device) {
                return Ok(());
            }
            out.push_str(": heartbeat\n\n");
            heartbeat = Instant::now();
        }
        // A background job that began or ended since the last look is an event of
        // its own, carried the same way as the others.
        let listed: Vec<(String, String, String, bool)> = jobs::list(&job_key)
            .into_iter()
            .map(|(id, command, state)| {
                let running = state == dext_native::strict::JobState::Running;
                (id, brief(&command, 80), jobs::describe(&state), running)
            })
            .collect();
        let known: Vec<(String, String)> = listed
            .iter()
            .map(|(id, _, state, _)| (id.clone(), state.clone()))
            .collect();
        if known != jobs_seen {
            for (id, command, state, running) in &listed {
                let was = jobs_seen.iter().find(|(seen, _)| seen == id);
                if was.map(|(_, state)| state) != Some(state) {
                    out.push_str(&format!(
                        "event: job\ndata: {}\n\n",
                        json!({"id": id, "command": command, "state": state, "running": running})
                    ));
                }
            }
            jobs_seen = known;
        }
        if !out.is_empty() && (stream.write_all(out.as_bytes()).is_err() || stream.flush().is_err())
        {
            return Ok(());
        }
        if deleted {
            return Ok(());
        }
    }
    Ok(())
}
/// `safe` serves no skin at all (recovery from a skin that hides controls); `preview`
/// serves the staged skin instead of the applied one.
fn index_page(safe: bool, preview: bool) -> String {
    const STYLE: &str = r#"<link rel="stylesheet" href="/style.css">"#;
    let page = include_str!("../../ui/index.html");
    let mode = if safe {
        "safe"
    } else if preview {
        "preview"
    } else {
        "normal"
    };
    let page = page.replacen("<html ", &format!("<html data-skin=\"{mode}\" "), 1);
    if safe {
        return page;
    }
    let link = if preview {
        skin::LINK.replace("/skin.css", "/skin.css?staged=1")
    } else {
        skin::LINK.to_string()
    };
    page.replacen(STYLE, &format!("{STYLE}{link}"), 1)
}
/// The embedded stylesheet is the `base` layer so an applied skin layer wins over
/// it; the skin's `@scope (body) to (dialog, #toast)` never matches dialogs, and they
/// reset every inherited text property themselves.
fn base_style() -> String {
    format!(
        "@layer base, skin;\n@layer base {{\n{}\n}}\n",
        include_str!("../../ui/style.css")
    )
}
fn ui_file(path: &str, query: &BTreeMap<String, String>) -> Option<(&'static str, Vec<u8>)> {
    match path {
        "/" => Some((
            "text/html; charset=utf-8",
            index_page(query.contains_key("safe"), query.contains_key("preview")).into_bytes(),
        )),
        "/style.css" => Some(("text/css; charset=utf-8", base_style().into_bytes())),
        _ => asset(path).map(|(mime, body)| (mime, body.to_vec())),
    }
}
fn asset(path: &str) -> Option<(&'static str, &'static [u8])> {
    Some(match path {
        "/app.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../../ui/app.js"),
        ),
        "/commands.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../../ui/commands.js"),
        ),
        "/theme.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../../ui/theme.js"),
        ),
        "/terminal.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../../ui/terminal.js"),
        ),
        "/text.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../../ui/text.js"),
        ),
        "/sw.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../../ui/sw.js"),
        ),
        "/style.css" => (
            "text/css; charset=utf-8",
            include_bytes!("../../ui/style.css"),
        ),
        "/icon.svg" => ("image/svg+xml", include_bytes!("../../ui/icon.svg")),
        "/manifest.webmanifest" => (
            "application/manifest+json",
            include_bytes!("../../ui/manifest.webmanifest"),
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    /// serve.lock is an flock on one open file description. A child that another test
    /// is forking holds a copy of it until exec closes it (O_CLOEXEC), so an immediate
    /// reopen in the same process can briefly see "another spindle serve".
    #[cfg(unix)]
    fn reopen(root: &Path) -> Host {
        let started = Instant::now();
        loop {
            match Host::open(root.to_path_buf(), None, None) {
                Ok(host) => return host,
                Err(error)
                    if error.to_string().contains("another spindle serve")
                        && started.elapsed() < Duration::from_secs(5) =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("reopen: {error}"),
            }
        }
    }
    /// Deleted trees are removed in the background; wait for that before cleanup.
    #[cfg(unix)]
    fn settle(path: &Path) {
        let started = Instant::now();
        while path.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "{} was not removed",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[cfg(unix)]
    fn temp_host(name: &str) -> (PathBuf, Arc<Host>) {
        let root = std::env::temp_dir().join(format!("spindle-host-{name}-{}", std::process::id()));
        let _ = disk::remove_tree(&root);
        let host = Arc::new(Host::open(root.clone(), None, None).unwrap());
        (root, host)
    }
    /// A registered thread with an empty store directory and owner lock.
    #[cfg(unix)]
    fn thread(host: &Host, id: &str) {
        lock(&host.db)
            .execute("INSERT INTO sessions VALUES(?1,'t',0)", [id])
            .unwrap();
        std::fs::create_dir_all(host.dir(id).join("store")).unwrap();
        File::create(host.dir(id).join("store/owner.lock")).unwrap();
        lock(&host.lives).insert(id.into(), Watch::new());
    }
    /// A registered thread with a real (empty) store.
    #[cfg(unix)]
    fn store_thread(host: &Host, id: &str) {
        lock(&host.db)
            .execute("INSERT INTO sessions VALUES(?1,'t',1)", [id])
            .unwrap();
        std::fs::create_dir_all(host.dir(id)).unwrap();
        drop(spindle_store::Store::open(&host.dir(id).join("store")).unwrap());
        lock(&host.lives).insert(id.into(), Watch::new());
    }
    /// Commits a revision whose workspace holds exactly `files` (a synthetic step, not
    /// a turn), returning it and its workspace root.
    #[cfg(unix)]
    fn commit_files(host: &Host, id: &str, files: &[SourceFile]) -> (u64, String) {
        use spindle_store::artifact::Stage;
        use spindle_store::transition::{Observation, Proposal};
        use spindle_store::workspace::Entry;
        let mut store = spindle_store::Store::open(&host.dir(id).join("store")).unwrap();
        let revision = store.snapshot().unwrap().revision;
        let label = format!("files-{revision}");
        let lineage = store.lineage().unwrap();
        let admission = store
            .admit(&spindle_store::Command {
                scope: "operator".into(),
                id: label.clone(),
                lineage,
                kind: "edit".into(),
                payload: b"files".to_vec(),
            })
            .unwrap()
            .sequence;
        let stage = Stage {
            id: label.clone(),
            scope: "operator".into(),
            owner_epoch: store.epoch(),
        };
        store.begin_stage(&stage).unwrap();
        let entries: Vec<Entry> = files
            .iter()
            .map(|(path, bytes, executable)| Entry {
                path: path.clone(),
                object: store.put_object(&stage, bytes).unwrap(),
                executable: *executable,
            })
            .collect();
        let root = store.stage_file_map(&stage, &entries).unwrap();
        let owner_epoch = store.epoch();
        let committed = store
            .commit_proposal(&Proposal {
                id: label,
                scope: "operator".into(),
                observation: Observation::Admission(admission),
                expected_revision: revision,
                owner_epoch,
                engine: "test-edit-v1".into(),
                continuation: b"continuation".to_vec(),
                workspace: Some(root.clone()),
                effects: vec![],
                state: None,
            })
            .unwrap();
        (committed, root)
    }
    /// The files of a thread revision with their bytes.
    #[cfg(unix)]
    fn files_of(host: &Host, id: &str, revision: u64) -> Vec<SourceFile> {
        let mut view = View::open(&host.dir(id).join("store")).unwrap();
        view.read(|view| {
            let root = view.at(revision)?.and_then(|(_, root)| root).unwrap();
            let mut out = vec![];
            for file in view.files(&root)? {
                let bytes = view.object(&file.entry.object)?;
                out.push((file.entry.path, bytes, file.entry.executable));
            }
            Ok(out)
        })
        .unwrap()
    }
    /// One raw request through `handle` on a loopback socket (HOST in it becomes the
    /// listener's address): status, head and body.
    #[cfg(unix)]
    fn exchange(host: &Arc<Host>, raw: &str) -> (u16, String, Vec<u8>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server_host = host.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            handle(&server_host, &mut stream);
        });
        let mut client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        client
            .write_all(raw.replace("HOST", &addr.to_string()).as_bytes())
            .unwrap();
        let mut bytes = vec![];
        client.read_to_end(&mut bytes).unwrap();
        server.join().unwrap();
        let split = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&bytes[..split]).into_owned();
        let status = head[9..12].parse().unwrap();
        (status, head, bytes[split + 4..].to_vec())
    }
    #[test]
    #[cfg(unix)]
    fn shelf_note_names_are_one_plain_segment() {
        let (root, host) = temp_host("shelf");
        for good in ["notes.md", "my notes-1_2.txt"] {
            assert!(shelf_path(&host, good).is_ok(), "{good}");
        }
        let long = "n".repeat(65);
        for bad in [
            "",
            "  ",
            ".hidden",
            "a/b",
            "../x",
            "x\\y",
            "(paren).md",
            &long,
        ] {
            assert!(shelf_path(&host, bad).is_err(), "{bad:?}");
        }
        drop(host);
        let _ = disk::remove_tree(&root);
    }
    #[test]
    #[cfg(unix)]
    fn a_preview_serves_a_multi_file_site_only_under_its_token() {
        let (root, host) = temp_host("preview");
        store_thread(&host, "s");
        store_thread(&host, "o");
        let (_, cookie) = host.pair("Linux · Firefox").unwrap();
        let file = |path: &str, bytes: &[u8]| (path.to_string(), bytes.to_vec(), false);
        let site = vec![
            file("secret.txt", b"same revision, outside the site"),
            file(
                "site/app.mjs",
                b"import {x} from './util.js'; fetch('data.json');",
            ),
            file("site/data.json", b"{\"ok\":true}"),
            file("site/docs/index.html", b"<p>docs</p>"),
            file("site/img/logo.png", b"\x89PNG\r\n\x1a\n"),
            file(
                "site/index.html",
                b"<!doctype html><link rel=stylesheet href=style.css><script type=module src=app.mjs></script><img src=img/logo.png>",
            ),
            file("site/style.css", b"body{color:rgb(1,2,3)}"),
            file("site/util.js", b"export const x = 1;"),
        ];
        let (revision, _) = commit_files(&host, "s", &site);
        commit_files(&host, "o", &[file("o.txt", b"another thread")]);
        let mint = |headers: &str, body: &str| {
            exchange(
                &host,
                &format!(
                    "POST /api/sessions/s/preview HTTP/1.1\r\nHost: HOST\r\n{headers}Content-Length: {}\r\n\r\n{body}",
                    body.len()
                ),
            )
        };
        let body = format!("{{\"revision\":{revision}}}");
        let paired = format!("Origin: http://HOST\r\nX-Spindle: 1\r\nCookie: spindle={cookie}\r\n");
        // Minting is an ordinary command: a paired device and the same-origin header.
        assert_eq!(mint(&format!("Cookie: spindle={cookie}\r\n"), &body).0, 403);
        assert_eq!(
            mint("Origin: http://HOST\r\nX-Spindle: 1\r\n", &body).0,
            401
        );
        assert_eq!(mint(&paired, "{\"revision\":0}").0, 400);
        assert_eq!(mint(&paired, "{\"revision\":99}").0, 400);
        let (status, _, reply) = mint(&paired, &body);
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&reply));
        let grant: Value = serde_json::from_slice(&reply).unwrap();
        let token = grant["token"].as_str().unwrap().to_string();
        let url = grant["url"].as_str().unwrap().to_string();
        assert_eq!(url, format!("/api/preview/{token}/"));
        assert_eq!(token.len(), 64, "256 random bits");
        assert!(grant["expires_in"].as_u64().unwrap() <= 3600);
        // What a sandboxed document sends: no cookie, an opaque origin, cross-site.
        let fetch = |path: &str| {
            exchange(
                &host,
                &format!(
                    "GET {path} HTTP/1.1\r\nHost: HOST\r\nOrigin: null\r\nSec-Fetch-Site: cross-site\r\n\r\n"
                ),
            )
        };
        for (path, mime) in [
            ("site/index.html", "text/html; charset=utf-8"),
            ("site/style.css", "text/css; charset=utf-8"),
            ("site/app.mjs", "text/javascript; charset=utf-8"),
            ("site/util.js", "text/javascript; charset=utf-8"),
            ("site/data.json", "application/json"),
            ("site/img/logo.png", "image/png"),
        ] {
            let (status, head, bytes) = fetch(&format!("{url}{path}"));
            assert_eq!(status, 200, "{path}: {head}");
            let stored = &site.iter().find(|f| f.0 == path).unwrap().1;
            if path.ends_with(".html") {
                // The page comes whole: its stylesheet and image inlined behind the
                // storage shim (after the doctype); the module is left to load.
                let page = String::from_utf8(bytes.clone()).unwrap();
                assert!(page.starts_with("<!doctype html><script>"), "{page}");
                assert!(page.contains("<style>"), "{page}");
                assert!(page.contains("type=module src=app.mjs"), "{page}");
                assert!(page.contains("src=\"data:image/png;base64,"), "{page}");
            } else {
                assert_eq!(&bytes, stored, "{path}");
            }
            for header in [
                format!("Content-Type: {mime}\r\n"),
                "X-Content-Type-Options: nosniff\r\n".into(),
                "Access-Control-Allow-Origin: *\r\n".into(),
                "Cross-Origin-Resource-Policy: cross-origin\r\n".into(),
                "Content-Security-Policy: sandbox allow-scripts allow-forms allow-modals; default-src 'none';"
                    .into(),
                format!("/api/preview/{token}/ 'unsafe-inline'"),
                "connect-src http://127.0.0.1:".into(),
                "form-action 'none'; frame-ancestors http://127.0.0.1:".into(),
            ] {
                assert!(head.contains(&header), "{path}: {header} in {head}");
            }
            assert!(!head.contains("allow-same-origin") && !head.contains("Set-Cookie"));
        }
        // A directory serves its index.html; named without its slash, it redirects.
        let (status, _, bytes) = fetch(&format!("{url}site/docs/"));
        assert_eq!(status, 200);
        assert!(
            String::from_utf8(bytes)
                .unwrap()
                .ends_with("</script><p>docs</p>")
        );
        let (status, head, _) = fetch(&format!("{url}site/docs"));
        assert_eq!(status, 308);
        assert!(head.contains(&format!("Location: /api/preview/{token}/site/docs/\r\n")));
        let (status, head, _) = fetch(&format!("/api/preview/{token}"));
        assert_eq!(status, 308);
        assert!(head.contains(&format!("Location: /api/preview/{token}/\r\n")));
        assert_eq!(
            fetch(&url).0,
            404,
            "the revision has no top-level index.html"
        );
        // Nothing outside the granted revision's files: traversal, absolute paths,
        // another thread's files, unknown files, forged or expired tokens.
        for path in [
            format!("{url}site/../secret.txt"),
            format!("{url}%2e%2e/secret.txt"),
            format!("{url}/etc/passwd"),
            format!("{url}o.txt"),
            format!("{url}site/missing.js"),
            format!("/api/preview/{}/site/index.html", "0".repeat(64)),
            format!("/api/preview/{}/site/index.html", &token[..32]),
            "/api/preview/../sessions".into(),
        ] {
            let (status, head, _) = fetch(&path);
            assert_eq!(status, 404, "{path}");
            assert!(head.contains("Content-Security-Policy: sandbox"), "{head}");
        }
        assert_eq!(
            exchange(
                &host,
                &format!(
                    "POST {url}site/index.html HTTP/1.1\r\nHost: HOST\r\nContent-Length: 0\r\n\r\n"
                )
            )
            .0,
            405,
            "previews are read-only"
        );
        // Everything else still refuses a cross-site request, cookie or not.
        let (status, _, _) = exchange(
            &host,
            &format!(
                "GET /api/sessions HTTP/1.1\r\nHost: HOST\r\nSec-Fetch-Site: cross-site\r\nCookie: spindle={cookie}\r\n\r\n"
            ),
        );
        assert_eq!(status, 403);
        // The single-file preview (`render=1`, used for SVG) still works with the cookie.
        let (status, head, _) = exchange(
            &host,
            &format!(
                "GET /api/sessions/s/file?revision={revision}&path=site/index.html&render=1 HTTP/1.1\r\nHost: HOST\r\nCookie: spindle={cookie}\r\n\r\n"
            ),
        );
        assert_eq!(status, 200);
        assert!(
            head.contains("Content-Security-Policy: sandbox allow-scripts;"),
            "{head}"
        );
        assert!(!head.contains("api.github.com"));
        for invalid in [json!(true), json!("*"), json!(["https://api.github.com"])] {
            assert_eq!(
                mint(
                    &paired,
                    &json!({"revision": revision, "network": invalid}).to_string()
                )
                .0,
                400
            );
        }
        let (_, _, connected) = mint(
            &paired,
            &json!({"revision": revision, "network": "github"}).to_string(),
        );
        let connected: Value = serde_json::from_slice(&connected).unwrap();
        assert_eq!(connected["network"], "github");
        let connected_url = connected["url"].as_str().unwrap();
        let (status, head, _) = fetch(&format!("{connected_url}site/index.html"));
        assert_eq!(status, 200);
        assert!(
            head.contains(" https://api.github.com;")
                && head.contains(" https://avatars.githubusercontent.com;")
        );
        assert!(!head.contains("allow-same-origin") && !head.contains("Set-Cookie"));
        assert!(
            !fetch(&format!("{url}site/index.html"))
                .1
                .contains("api.github.com"),
            "a new grant cannot widen an existing one"
        );
        assert!(
            !fetch(&format!("{url}site/index.html?network=github"))
                .1
                .contains("api.github.com"),
            "query parameters cannot widen a token"
        );
        // A token lapses after its lifetime, and with its thread.
        host.previews.age(Duration::from_secs(3600));
        assert_eq!(fetch(&format!("{url}site/index.html")).0, 404);
        let (_, _, reply) = mint(&paired, &body);
        let url: String = serde_json::from_slice::<Value>(&reply).unwrap()["url"]
            .as_str()
            .unwrap()
            .into();
        assert_eq!(fetch(&format!("{url}site/style.css")).0, 200);
        host.previews.forget("s");
        assert_eq!(fetch(&format!("{url}site/style.css")).0, 404);
        drop(host);
        let _ = disk::remove_tree(&root);
    }
    #[test]
    #[cfg(unix)]
    fn an_updated_thread_promotes_against_its_sync_and_keeps_host_upgrades() {
        use spindle_store::workspace::{Entry, Tree, tree_digest};
        let (root, host) = temp_host("sync");
        store_thread(&host, "s");
        let dir = host.dir("s");
        let scratch = root.join("scratch");
        private_dir(&scratch).unwrap();
        let f = |path: &str, text: &str| (path.to_string(), text.as_bytes().to_vec(), false);
        // The ideasCon shape: the thread changed a; the host changed a (the same
        // line) and b.
        let import = vec![f("a", "1\n2\n3\n"), f("b", "b\n")];
        let (r1, _) = commit_files(&host, "s", &import);
        let (r2, _) = commit_files(&host, "s", &[f("a", "1\n2\nthread\n"), f("b", "b\n")]);
        assert_eq!(files_of(&host, "s", r1), import);
        let source = vec![f("a", "1\n2\nhost\n"), f("b", "b host\n")];
        assert!(
            sync::find(&dir, r2, |_| ()).unwrap().is_none(),
            "never synced"
        );
        let refused = compose_promotion(&import, &source, &files_of(&host, "s", r2), &scratch);
        assert_eq!(refused.unwrap().unwrap_err(), ["a"]);
        // Update from host source: merge, record the sync, then commit the merge.
        let update = sync::merge(&import, &source, &files_of(&host, "s", r2), &scratch).unwrap();
        assert_eq!(
            (update.conflicts.clone(), update.from_host.clone()),
            (vec!["a".to_string()], vec!["b".to_string()])
        );
        let planned = tree_digest(&Tree {
            files: update
                .files
                .iter()
                .map(|(path, bytes, executable)| Entry {
                    path: path.clone(),
                    object: spindle_store::artifact::object_digest(bytes),
                    executable: *executable,
                })
                .collect(),
            ..Tree::default()
        })
        .unwrap();
        sync::record(&dir, &planned, &source).unwrap();
        // A record whose turn never committed (a crash first) is never found.
        sync::record(&dir, &"f".repeat(64), &[f("a", "never committed\n")]).unwrap();
        assert!(
            sync::find(&dir, r2, |_| ()).unwrap().is_none(),
            "nothing committed yet"
        );
        let (r3, merged) = commit_files(&host, "s", &update.files);
        assert_eq!(
            merged, planned,
            "the recorded root is the one the turn commits"
        );
        let marked = String::from_utf8(files_of(&host, "s", r3)[0].1.clone()).unwrap();
        assert!(marked.contains("<<<<<<< host source") && marked.contains(">>>>>>> this thread"));
        // The model resolves the conflict, keeping both sides.
        let resolved = vec![f("a", "1\n2\nhost thread\n"), f("b", "b host\n")];
        let (r4, _) = commit_files(&host, "s", &resolved);
        let (at, base) = sync::find(&dir, r4, |_| ()).unwrap().unwrap();
        assert_eq!((at, &base), (r3, &source));
        let composed = compose_promotion(&base, &source, &resolved, &scratch)
            .unwrap()
            .unwrap();
        assert_eq!(composed, resolved);
        // The host moved on again meanwhile: its newer upgrades stay too.
        let later = vec![
            f("a", "1\n2\nhost\n"),
            f("b", "b host 2\n"),
            f("c", "new\n"),
        ];
        let composed = compose_promotion(&base, &later, &resolved, &scratch)
            .unwrap()
            .unwrap();
        assert_eq!(
            composed,
            [
                f("a", "1\n2\nhost thread\n"),
                f("b", "b host 2\n"),
                f("c", "new\n")
            ]
        );
        // A damaged record is passed over with a warning (an older base only refuses).
        std::fs::write(dir.join("host-sync").join(&planned), b"damaged").unwrap();
        let mut warnings = vec![];
        assert!(
            sync::find(&dir, r4, |w| warnings.push(w))
                .unwrap()
                .is_none()
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        drop(host);
        let _ = disk::remove_tree(&root);
    }
    #[test]
    #[cfg(unix)]
    fn promotion_keeps_host_upgrades_and_refuses_conflicts() {
        let scratch = std::env::temp_dir().join(format!("spindle-compose-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).unwrap();
        let file = |path: &str, text: &str| (path.to_string(), text.as_bytes().to_vec(), false);
        let base = vec![
            file("a", "1\n2\n3\n"),
            file("b", "old\n"),
            file("c", "same\n"),
        ];
        let ours = vec![
            file("a", "1\n2\n3\n"),
            file("b", "host\n"),
            file("c", "same\n"),
            file("d", "new host\n"),
        ];
        let theirs = vec![
            file("a", "1\n2\nthread\n"),
            file("b", "old\n"),
            file("e", "thread file\n"),
        ];
        let composed = compose_promotion(&base, &ours, &theirs, &scratch)
            .unwrap()
            .unwrap();
        let text = |path: &str| {
            composed
                .iter()
                .find(|f| f.0 == path)
                .map(|f| String::from_utf8(f.1.clone()).unwrap())
        };
        assert_eq!(text("a").as_deref(), Some("1\n2\nthread\n"));
        assert_eq!(
            text("b").as_deref(),
            Some("host\n"),
            "a stale thread never reverts the host"
        );
        assert_eq!(text("c"), None, "the thread deleted c");
        assert_eq!(text("d").as_deref(), Some("new host\n"));
        assert_eq!(text("e").as_deref(), Some("thread file\n"));
        let both = vec![
            file("a", "1\n2\nhost\n"),
            file("b", "old\n"),
            file("c", "same\n"),
        ];
        let merged = compose_promotion(
            &base,
            &both,
            &[
                file("a", "thread\n2\n3\n"),
                file("b", "old\n"),
                file("c", "same\n"),
            ],
            &scratch,
        )
        .unwrap()
        .unwrap();
        assert_eq!(merged[0].1, b"thread\n2\nhost\n");
        let conflicts = compose_promotion(&base, &both, &theirs, &scratch)
            .unwrap()
            .unwrap_err();
        assert_eq!(conflicts, ["a"]);
        // Git metadata both sides rewrote is the host's: never a conflict, never promoted.
        let git = |byte: u8| (".git/index".to_string(), vec![0, byte], false);
        let composed = compose_promotion(
            &[git(1), file("x", "1\n")],
            &[git(2), file("x", "1\n")],
            &[
                git(3),
                (".git/objects/thread".into(), vec![9], false),
                file("x", "2\n"),
            ],
            &scratch,
        )
        .unwrap()
        .unwrap();
        assert_eq!(composed, [git(2), file("x", "2\n")]);
        let text = source_moved_text(&conflicts);
        assert!(
            text.contains("both changed a.")
                && text.contains("Tap Update on the Spindle change card")
                && !text.contains("Settings"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }
    #[test]
    fn ring_is_bounded_and_monotonic() {
        let mut live = Live::new();
        for _ in 0..2000 {
            live.push(json!({"text":"x".repeat(1024)}));
        }
        assert_eq!(live.seq, 2000);
        assert!(live.bytes <= MAX_PREVIEW);
        assert!(live.events.len() <= MAX_EVENTS);
        assert_eq!(live.events.back().unwrap().0, 2000);
    }
    #[test]
    fn ids_cannot_escape_or_alias_paths() {
        for id in ["../secret", "/tmp", ".", "", "a/b", "a.b"] {
            assert!(!id_ok(id));
        }
        assert!(id_ok("request_42-good"));
    }
    #[test]
    fn output_chunks_encode_as_standard_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(&[0xff, 0x00, 0x10, 0x7f]), "/wAQfw==");
    }
    #[test]
    #[cfg(unix)]
    fn rename_and_delete_are_fenced_and_durable() {
        assert!(admin_name(&json!({"name":"\u{7}\u{1b}"})).is_err());
        let root = std::env::temp_dir().join(format!("spindle-host-admin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let host = Arc::new(Host::open(root.clone(), None, None).unwrap());
        host.db
            .lock()
            .unwrap()
            .execute("INSERT INTO sessions VALUES('s','old',0)", [])
            .unwrap();
        std::fs::create_dir_all(host.dir("s").join("store")).unwrap();
        File::create(host.dir("s").join("store/owner.lock")).unwrap();
        host.lives.lock().unwrap().insert("s".into(), Watch::new());
        assert!(
            host.submit(
                "s",
                json!({"request_id":"blank","kind":"rename","name":"  "})
            )
            .is_err()
        );
        let renamed = host
            .submit(
                "s",
                json!({"request_id":"rn","kind":"rename","name":"new\u{7}name"}),
            )
            .unwrap();
        assert_eq!(renamed["state"], "completed");
        let name: String = host
            .db
            .lock()
            .unwrap()
            .query_row("SELECT name FROM sessions WHERE id='s'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "newname");
        host.live("s").unwrap().lock().busy = true;
        assert!(
            host.submit("s", json!({"request_id":"busy","kind":"delete"}))
                .is_err(),
            "a running thread cannot be deleted"
        );
        host.live("s").unwrap().lock().busy = false;
        let deleted = host
            .submit("s", json!({"request_id":"del","kind":"delete"}))
            .unwrap();
        assert_eq!(deleted["state"], "completed");
        assert!(host.live("s").is_err());
        assert!(!host.dir("s").exists());
        let rows: i64 = host
            .db
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
        assert_eq!(host.receipt("del").unwrap()["state"], "completed");
        assert!(
            host.submit("s", json!({"request_id":"again","kind":"delete"}))
                .is_err()
        );
        settle(&root.join("trash/s"));
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn deletion_restores_directory_after_registry_failure_and_restart_cut() {
        let root =
            std::env::temp_dir().join(format!("spindle-host-delete-cut-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let host = Arc::new(Host::open(root.clone(), None, None).unwrap());
        host.db
            .lock()
            .unwrap()
            .execute("INSERT INTO sessions VALUES('s','saved',0)", [])
            .unwrap();
        std::fs::create_dir_all(host.dir("s").join("store")).unwrap();
        File::create(host.dir("s").join("store/owner.lock")).unwrap();
        host.lives.lock().unwrap().insert("s".into(), Watch::new());
        let owner = File::open(host.dir("s").join("store/owner.lock")).unwrap();
        owner.try_lock().unwrap();
        let refused = host
            .submit("s", json!({"request_id":"locked","kind":"delete"}))
            .unwrap();
        assert_eq!(refused["state"], "failed");
        assert!(host.dir("s").exists());
        owner.unlock().unwrap();
        host.db.lock().unwrap().execute_batch("CREATE TRIGGER refuse_delete BEFORE DELETE ON sessions BEGIN SELECT RAISE(ABORT,'injected storage failure'); END;").unwrap();
        let refused = host
            .submit("s", json!({"request_id":"fault","kind":"delete"}))
            .unwrap();
        assert_eq!(refused["state"], "failed");
        assert!(host.dir("s").join("store/owner.lock").exists());
        host.db.lock().unwrap().execute_batch("DROP TRIGGER refuse_delete; CREATE TRIGGER refuse_delete BEFORE DELETE ON sessions BEGIN SELECT RAISE(IGNORE); END;").unwrap();
        let refused = host
            .submit("s", json!({"request_id":"ignored","kind":"delete"}))
            .unwrap();
        assert_eq!(refused["state"], "failed");
        assert!(host.dir("s").join("store/owner.lock").exists());
        host.db
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER refuse_delete")
            .unwrap();
        private_dir(&root.join("trash")).unwrap();
        std::fs::rename(host.dir("s"), root.join("trash/s")).unwrap();
        drop(owner);
        drop(host);
        let host = reopen(&root);
        assert!(host.dir("s").join("store/owner.lock").exists());
        assert!(!root.join("trash/s").exists());
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[ignore = "requires loopback sockets; run explicitly outside offline promotion"]
    fn all_embedded_assets_are_reachable_before_authentication() {
        let root = std::env::temp_dir().join(format!("spindle-host-assets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let host = Arc::new(Host::open(root.clone(), None, None).unwrap());
        for path in [
            "/",
            "/app.js",
            "/terminal.js",
            "/commands.js",
            "/text.js",
            "/sw.js",
            "/theme.js",
            "/style.css",
            "/manifest.webmanifest",
            "/icon.svg",
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let host = host.clone();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                handle(&host, &mut stream);
            });
            let mut client = TcpStream::connect(addr).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            write!(client, "GET {path} HTTP/1.1\r\nHost: {addr}\r\n\r\n").unwrap();
            let mut bytes = Vec::new();
            client.read_to_end(&mut bytes).unwrap();
            server.join().unwrap();
            let headers = String::from_utf8_lossy(
                &bytes[..bytes.windows(4).position(|b| b == b"\r\n\r\n").unwrap()],
            );
            assert!(headers.starts_with("HTTP/1.1 200"), "{path}: {headers}");
            if path.ends_with(".js") {
                assert!(headers.contains("Content-Type: text/javascript"));
            }
        }
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn duplicate_stop_keeps_receipt_and_payload_conflict_fails() {
        let root = std::env::temp_dir().join(format!("spindle-host-dedupe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let host = Arc::new(Host::open(root.clone(), None, None).unwrap());
        host.lives.lock().unwrap().insert("s".into(), Watch::new());
        host.live("s").unwrap().lock().busy = true;
        let data = json!({"request_id":"r","kind":"stop"});
        assert_eq!(
            host.submit("s", data.clone()).unwrap()["state"],
            "completed"
        );
        assert_eq!(
            host.submit("s", data.clone()).unwrap()["state"],
            "completed"
        );
        assert!(
            host.submit("s", json!({"request_id":"r","kind":"retry"}))
                .is_err()
        );
        assert!(
            host.submit("s", json!({"request_id":"second-stop","kind":"stop"}))
                .is_err()
        );
        assert_eq!(host.receipt("r").unwrap()["submission"], data);
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn every_ui_file_is_served_without_pairing() {
        let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
        for file in std::fs::read_dir(ui).unwrap() {
            let file = file.unwrap();
            assert!(file.file_type().unwrap().is_file());
            let name = file.file_name();
            let name = name.to_str().unwrap();
            let path = if name == "index.html" {
                "/".into()
            } else {
                format!("/{name}")
            };
            let source = std::fs::read_to_string(file.path()).unwrap_or_default();
            let (mime, bytes) = ui_file(&path, &BTreeMap::new())
                .unwrap_or_else(|| panic!("unserved UI asset: {path}"));
            match name {
                "index.html" => {
                    let safe = ui_file("/", &BTreeMap::from([("safe".into(), String::new())]))
                        .unwrap()
                        .1;
                    assert_eq!(
                        String::from_utf8(safe).unwrap(),
                        source.replacen("<html ", "<html data-skin=\"safe\" ", 1),
                        "safe mode serves the page without a skin link"
                    );
                    let page = String::from_utf8(bytes).unwrap();
                    assert!(page.contains(skin::LINK) && page.contains("data-skin=\"normal\""));
                    assert!(page.find("/style.css") < page.find("/skin.css"));
                }
                "style.css" => {
                    let css = String::from_utf8(bytes).unwrap();
                    assert!(css.starts_with("@layer base, skin;\n@layer base {\n"));
                    assert!(css.contains(&source));
                }
                _ => assert_eq!(bytes, std::fs::read(file.path()).unwrap()),
            }
            if name.ends_with(".js") {
                assert_eq!(mime, "text/javascript; charset=utf-8");
            }
        }
    }
    #[test]
    #[cfg(unix)]
    fn token_assets_and_owner_lock_are_validated() {
        let root = std::env::temp_dir().join(format!("spindle-host-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let host = Arc::new(Host::open(root.clone(), None, None).unwrap());
        assert_eq!(host.token.len(), 64);
        assert!(
            ui_file("/", &BTreeMap::new())
                .unwrap()
                .1
                .windows(b"theme.js".len())
                .any(|part| part == b"theme.js")
        );
        assert!(host.skins.state()["active"].is_null());
        assert!(asset("/theme.js").is_some());
        assert!(
            host.submit("../escape", json!({"kind":"say","request_id":"r"}))
                .is_err()
        );
        assert!(Host::open(root.clone(), None, None).is_err());
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn failed_create_leaves_no_session_behind() {
        let root = std::env::temp_dir().join(format!("spindle-host-orphan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let host = Arc::new(Host::open(root.clone(), None, None).unwrap());
        // An impossible route is refused before admission, with its reason and a 400,
        // so the phone keeps the draft and nothing is registered.
        let Refused(status, error) = host
            .submit(
                "",
                json!({"request_id":"create","kind":"new","text":"hi","provider":"no-such-provider","name":"x"}),
            )
            .unwrap_err();
        assert_eq!(status, 400, "{error}");
        assert!(host.receipt("create").is_err(), "nothing admitted");
        let rows: i64 = host
            .db
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
        assert!(lock(&host.lives).is_empty());
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn restart_marks_admissions_unknown_and_never_resends() {
        let root = std::env::temp_dir().join(format!("spindle-host-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let host = Host::open(root.clone(), None, None).unwrap();
        host.db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO commands VALUES('r','s','d','running',NULL)",
                [],
            )
            .unwrap();
        drop(host);
        let host = reopen(&root);
        assert_eq!(host.receipt("r").unwrap()["state"], "interrupted");
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn resume_known_after_restart_adopts_saved_reply_but_never_resends_unknown() {
        use dext_core::model::{Encoding, Identity, ModelWait, PreparedRequest};
        use dext_core::recovery::Contract;
        use dext_transcript::checkpoint::{NativeResponse, TranscriptCheckpoint};
        use dext_transcript::turn::Effect;
        use spindle_store::broker::{DispatchRequest, ModelReservation};
        use spindle_store::completion::Attempt;
        use spindle_store::transition::{Observation, Proposal};
        use spindle_store::turn::{Action, ENGINE, execution_id, new_intents};
        use spindle_store::{Command, Store};
        let (root, host) = temp_host("resume-known");
        for id in ["known", "unknown"] {
            store_thread(&host, id);
            Settings::from_json(&json!({"provider":"local", "model":"fixture", "builtin_tools":"none", "compaction":"digest", "resend_unknown":true})).unwrap().save(&host.dir(id)).unwrap();
            let mut store = Store::open(&host.dir(id).join("store")).unwrap();
            let initial = TurnContinuation::new(
                "operator".into(),
                execution_id("operator", "start").unwrap(),
                Contract::OpenAiResponses,
                None,
                TranscriptCheckpoint {
                    route_binding: "spindle-cli-v1".into(),
                    messages: vec![],
                },
            )
            .unwrap();
            let request = PreparedRequest::new(
                Identity {
                    effect: initial.next_model_id().unwrap(),
                    provider: "local".into(),
                    contract: "openai-responses".into(),
                    endpoint: "http://127.0.0.1:1/responses".into(),
                    model: "fixture".into(),
                    credential_scope: "fixture".into(),
                    configuration: "fixture".into(),
                },
                Encoding::Json,
                b"{}".to_vec(),
            )
            .unwrap();
            let after = initial.prepare(request.clone(), "spindle-cli-v1").unwrap();
            let lineage = store.lineage().unwrap();
            let admission = store
                .admit(&Command {
                    scope: "operator".into(),
                    id: "start".into(),
                    lineage,
                    kind: ENGINE.into(),
                    payload: Action::Start {
                        initial: initial.checkpoint().unwrap(),
                        request: ModelWait::new(request).checkpoint(),
                    }
                    .encode()
                    .unwrap(),
                })
                .unwrap();
            let stage = spindle_store::artifact::Stage {
                id: "fixture-root".into(),
                scope: "operator".into(),
                owner_epoch: store.epoch(),
            };
            store.begin_stage(&stage).unwrap();
            let root = store.stage_tree(&stage, &Default::default()).unwrap();
            let revision = store
                .commit_turn_workspace_start(&Proposal {
                    id: "commit-start".into(),
                    scope: "operator".into(),
                    observation: Observation::Admission(admission.sequence),
                    expected_revision: 0,
                    owner_epoch: store.epoch(),
                    engine: ENGINE.into(),
                    continuation: after.checkpoint().unwrap(),
                    workspace: Some(root),
                    effects: new_intents(Some(&initial), &after).unwrap(),
                    state: None,
                })
                .unwrap();
            let intent = store.intents(revision).unwrap().remove(0);
            let request = DispatchRequest {
                scope: "operator".into(),
                owner_epoch: store.epoch(),
                expected_revision: revision,
                intent_revision: revision,
                attempt: Attempt {
                    id: format!("a:{}", intent.id),
                    effect: intent.id.clone(),
                    owner_epoch: store.epoch(),
                    implementation: "dext-provider-v1".into(),
                    grant_reference: "dext-login".into(),
                },
                intent,
            };
            let ModelReservation::Reserved(committed) = store
                .reserve_turn_model(&request, &false.into(), |_| Ok(()))
                .unwrap()
            else {
                panic!("new request")
            };
            {
                let Effect::Model { waiting, .. } = committed.effect() else {
                    panic!("model")
                };
                let response = NativeResponse {
                    blocks: vec![dext_transcript::Block::Text {
                        text: "saved reply".into(),
                    }],
                    stop_reason: Some("stop".into()),
                    usage: Default::default(),
                    unknown_events: 0,
                    unfinished_tool_calls: 0,
                };
                let recorded = waiting
                    .record(
                        waiting.request(),
                        waiting.attempt(),
                        &response.checkpoint().unwrap(),
                    )
                    .unwrap();
                let bytes = recorded.checkpoint();
                store
                    .ingest_turn_model(&request, &committed, bytes.clone())
                    .unwrap();
                if id == "unknown" {
                    let final_review = after.apply_model(&recorded, "spindle-cli-v1").unwrap();
                    let revision = store
                        .commit_turn_receipt(
                            &Proposal {
                                id: "adopt-first".into(),
                                scope: "operator".into(),
                                observation: Observation::Receipt(request.attempt.id.clone()),
                                expected_revision: revision,
                                owner_epoch: store.epoch(),
                                engine: ENGINE.into(),
                                continuation: final_review.checkpoint().unwrap(),
                                workspace: None,
                                effects: new_intents(Some(&after), &final_review).unwrap(),
                                state: None,
                            },
                            &spindle_store::turn::receipt_binding(
                                &request.attempt,
                                revision,
                                &request.intent,
                                &bytes,
                            ),
                        )
                        .unwrap();
                    let next = final_review.followup("another request".into()).unwrap();
                    let admission = store
                        .admit(&Command {
                            scope: "operator".into(),
                            id: "followup".into(),
                            lineage,
                            kind: ENGINE.into(),
                            payload: Action::Followup {
                                text: "another request".into(),
                            }
                            .encode()
                            .unwrap(),
                        })
                        .unwrap();
                    let revision = store
                        .commit_turn_action(&Proposal {
                            id: "followup".into(),
                            scope: "operator".into(),
                            observation: Observation::Admission(admission.sequence),
                            expected_revision: revision,
                            owner_epoch: store.epoch(),
                            engine: ENGINE.into(),
                            continuation: next.checkpoint().unwrap(),
                            workspace: None,
                            effects: vec![],
                            state: None,
                        })
                        .unwrap();
                    let model = PreparedRequest::new(
                        Identity {
                            effect: next.next_model_id().unwrap(),
                            ..waiting.request().identity().clone()
                        },
                        Encoding::Json,
                        b"{}".to_vec(),
                    )
                    .unwrap();
                    let prepared = next.prepare(model.clone(), "spindle-cli-v1").unwrap();
                    let admission = store
                        .admit(&Command {
                            scope: "operator".into(),
                            id: "prepare-next".into(),
                            lineage,
                            kind: ENGINE.into(),
                            payload: Action::Prepare {
                                request: ModelWait::new(model).checkpoint(),
                            }
                            .encode()
                            .unwrap(),
                        })
                        .unwrap();
                    let revision = store
                        .commit_turn_action(&Proposal {
                            id: "prepare-next".into(),
                            scope: "operator".into(),
                            observation: Observation::Admission(admission.sequence),
                            expected_revision: revision,
                            owner_epoch: store.epoch(),
                            engine: ENGINE.into(),
                            continuation: prepared.checkpoint().unwrap(),
                            workspace: None,
                            effects: new_intents(Some(&next), &prepared).unwrap(),
                            state: None,
                        })
                        .unwrap();
                    let intent = store.intents(revision).unwrap().remove(0);
                    let request = DispatchRequest {
                        scope: "operator".into(),
                        owner_epoch: store.epoch(),
                        expected_revision: revision,
                        intent_revision: revision,
                        attempt: Attempt {
                            id: format!("a:{}", intent.id),
                            effect: intent.id.clone(),
                            owner_epoch: store.epoch(),
                            implementation: "dext-provider-v1".into(),
                            grant_reference: "dext-login".into(),
                        },
                        intent,
                    };
                    store
                        .reserve_turn_model(&request, &false.into(), |_| Ok(()))
                        .unwrap();
                }
            }
            assert_eq!(
                spindle_store::api::recovery(&host.dir(id)).unwrap()["wake"]["work"],
                "ready"
            );
            lock(&host.db)
                .execute(
                    "INSERT INTO commands VALUES(?1,?2,'fixture','running',NULL)",
                    params![format!("old-{id}"), id],
                )
                .unwrap();
        }
        drop(host);
        let host = Arc::new(reopen(&root));
        assert_eq!(host.interrupted.len(), 2);
        let unknown_before =
            spindle_store::api::head_state(&host.dir("unknown")).unwrap()["head"].clone();
        host.resume_known();
        let started = Instant::now();
        loop {
            let state = spindle_store::api::head_state(&host.dir("known")).unwrap();
            if state["complete"] == true && !host.live("known").unwrap().lock().busy {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "{state}");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            spindle_store::api::head_state(&host.dir("unknown")).unwrap()["head"],
            unknown_before
        );
        assert_eq!(
            lock(&host.db)
                .query_row(
                    "SELECT count(*) FROM commands WHERE session='unknown'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn receipts_are_pruned_by_age_and_deleted_thread_but_recent_ids_stay() {
        let (root, host) = temp_host("prune");
        thread(&host, "live");
        let now = now_ms() as i64;
        let day = 24 * 3600 * 1000;
        {
            let db = lock(&host.db);
            for (id, session, state, age) in [
                ("old", "live", "completed", 8 * day),
                ("old-running", "live", "running", 8 * day),
                ("recent", "live", "failed", day),
                ("gone-old", "deleted", "completed", 2 * 3600 * 1000),
                ("gone-recent", "deleted", "completed", 60 * 1000),
            ] {
                db.execute(
                    "INSERT INTO commands VALUES(?1,?2,'d',?3,NULL)",
                    params![id, session, state],
                )
                .unwrap();
                db.execute("INSERT INTO submissions VALUES(?1,'{}')", [id])
                    .unwrap();
                db.execute(
                    "INSERT INTO command_times VALUES(?1,?2)",
                    params![id, now - age],
                )
                .unwrap();
            }
            assert_eq!(prune(&db).unwrap(), 2);
        }
        for (id, kept) in [
            ("old", false),
            ("old-running", true),
            ("recent", true),
            ("gone-old", false),
            ("gone-recent", true),
        ] {
            assert_eq!(host.receipt(id).is_ok(), kept, "{id}");
        }
        let orphans: i64 = lock(&host.db)
            .query_row(
                "SELECT (SELECT count(*) FROM submissions WHERE id NOT IN (SELECT id FROM commands)) + (SELECT count(*) FROM command_times WHERE id NOT IN (SELECT id FROM commands))",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphans, 0, "submissions and times go with their receipt");
        // At the cap, old receipts make room instead of refusing everything forever.
        {
            let mut db = lock(&host.db);
            let tx = db.transaction().unwrap();
            for n in 0..MAX_COMMANDS {
                tx.execute(
                    "INSERT INTO commands VALUES(?1,'live','d','completed',NULL)",
                    [format!("filler-{n}")],
                )
                .unwrap();
                tx.execute(
                    "INSERT INTO command_times VALUES(?1,?2)",
                    params![format!("filler-{n}"), now - 30 * day],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        let renamed = host
            .submit(
                "live",
                json!({"request_id":"after-cap","kind":"rename","name":"still works"}),
            )
            .unwrap();
        assert_eq!(renamed["state"], "completed");
        assert_eq!(
            host.submit(
                "live",
                json!({"request_id":"after-cap","kind":"rename","name":"still works"})
            )
            .unwrap()["id"],
            "after-cap",
            "a recent ID stays idempotent"
        );
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn stop_is_honoured_before_any_install_and_refused_after() {
        let (root, host) = temp_host("stop-upgrade");
        thread(&host, "s");
        let live = host.live("s").unwrap();
        let cancel = AtomicBool::new(true);
        let mut installed = false;
        let refused = install_unless_stopped(&live, &cancel, || {
            installed = true;
            Ok(())
        });
        assert!(refused.unwrap_err().contains("nothing was installed"));
        assert!(!installed, "a Stop always wins over the install");
        let cancel = AtomicBool::new(false);
        live.lock().stopping = true;
        assert!(install_unless_stopped(&live, &cancel, || Ok(())).is_err());
        live.lock().stopping = false;
        live.lock().busy = true;
        install_unless_stopped(&live, &cancel, || Ok(())).unwrap();
        let Refused(_, error) = host
            .submit("s", json!({"request_id":"late-stop","kind":"stop"}))
            .unwrap_err();
        assert!(error.contains("already installed"), "{error}");
        assert!(host.receipt("late-stop").is_err(), "nothing admitted");
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn a_panic_poisons_nothing_and_never_leaves_a_thread_busy() {
        let (root, host) = temp_host("poison");
        thread(&host, "s");
        let poisoner = host.clone();
        let _ = std::thread::spawn(move || {
            let _db = poisoner.db.lock();
            let _lives = poisoner.lives.lock();
            panic!("injected panic while holding host locks");
        })
        .join();
        assert!(host.db.is_poisoned() && host.lives.is_poisoned());
        assert_eq!(
            host.submit("s", json!({"request_id":"rn","kind":"rename","name":"ok"}))
                .unwrap()["state"],
            "completed"
        );
        assert!(host.sessions().unwrap()[0]["name"] == "ok");
        let live = host.live("s").unwrap();
        live.lock().busy = true;
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _idle = Idle {
                live: &live,
                updating: &host.updating,
                restarting: false,
                done: None,
            };
            panic!("injected worker panic");
        }));
        assert!(caught.is_err());
        let state = live.lock();
        assert!(!state.busy && state.cancel.is_none());
        assert!(state.events.back().unwrap().1.contains("\"type\":\"done\""));
        drop(state);
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn deletion_handles_missing_stores_and_unwritable_trash() {
        let (root, host) = temp_host("delete-hard");
        // A host death between registering a thread and creating its store.
        lock(&host.db)
            .execute("INSERT INTO sessions VALUES('bare','x',0)", [])
            .unwrap();
        lock(&host.lives).insert("bare".into(), Watch::new());
        let missing = host.snapshot("bare", None).unwrap();
        assert_eq!(missing["missing"], true);
        assert_eq!(
            host.submit("bare", json!({"request_id":"d1","kind":"delete"}))
                .unwrap()["state"],
            "completed"
        );
        assert!(host.live("bare").is_err());
        // A sandbox overlay leaves a mode-000 directory; the purge still removes it.
        thread(&host, "s");
        let work = host.dir("s").join("cache/overlay/work/work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("f"), b"x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        assert_eq!(
            host.submit("s", json!({"request_id":"d2","kind":"delete"}))
                .unwrap()["state"],
            "completed"
        );
        settle(&root.join("trash/s"));
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn start_restores_registered_trash_and_sweeps_the_rest() {
        let (root, host) = temp_host("sweep");
        thread(&host, "kept");
        std::fs::write(host.dir("kept").join("store/data"), vec![7u8; 8192]).unwrap();
        drop(host);
        private_dir(&root.join("trash")).unwrap();
        std::fs::rename(root.join("sessions/kept"), root.join("trash/kept")).unwrap();
        let orphan = root.join("trash/orphan/overlay/work/work");
        std::fs::create_dir_all(&orphan).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&orphan, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        let host = reopen(&root);
        assert!(host.dir("kept").join("store/owner.lock").exists());
        settle(&root.join("trash/orphan"));
        let disk = host.about()["disk"].clone();
        assert!(disk["sessions"].as_u64().unwrap() > 0 && disk["free"].as_u64().is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join("host.sqlite"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "host.sqlite is private");
        }
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn candidate_check_opens_a_private_copy_and_markers_never_misfire() {
        let (root, host) = temp_host("check");
        thread(&host, "s");
        let copy = root.join("copy");
        private_dir(&copy).unwrap();
        lock(&host.db)
            .execute(
                "VACUUM INTO ?1",
                [copy.join("host.sqlite").to_str().unwrap()],
            )
            .unwrap();
        std::fs::write(copy.join("pairing.key"), &host.token).unwrap();
        check(copy.clone()).unwrap();
        assert!(
            !copy.join("sessions/s").exists(),
            "the check never touches sessions"
        );
        let broken = root.join("broken");
        private_dir(&broken).unwrap();
        std::fs::write(broken.join("host.sqlite"), b"not a database at all").unwrap();
        std::fs::write(broken.join("pairing.key"), &host.token).unwrap();
        assert!(
            check(broken).is_err(),
            "an unopenable registry fails the check"
        );
        // A marker for some other binary is consumed without rolling anything back.
        std::fs::write(root.join(UPGRADE_MARKER), "000000000000").unwrap();
        assert_eq!(after_failed_start(&root, "boom".into()), "boom");
        assert!(!root.join(UPGRADE_MARKER).exists());
        std::fs::write(root.join(UPGRADE_MARKER), "000000000000").unwrap();
        assert_eq!(
            after_failed_start(&root, BUSY_DIRECTORY.into()),
            BUSY_DIRECTORY
        );
        assert!(
            root.join(UPGRADE_MARKER).exists(),
            "a second host is not a failed upgrade"
        );
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn event_streams_wake_on_events_not_on_a_timer() {
        let watch = Watch::new();
        watch.push(json!({"n":1}));
        let started = Instant::now();
        assert_eq!(
            watch.wait(None, started + HEARTBEAT).seq,
            1,
            "no cursor: no wait"
        );
        let pusher = watch.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            pusher.push(json!({"n":2}));
        });
        assert_eq!(watch.wait(Some(1), started + HEARTBEAT).seq, 2);
        assert!(started.elapsed() < Duration::from_secs(5));
        let quiet = Instant::now();
        assert_eq!(
            watch.wait(Some(2), quiet + Duration::from_millis(30)).seq,
            2
        );
        assert!(
            quiet.elapsed() >= Duration::from_millis(30),
            "times out for heartbeats"
        );
    }
    #[test]
    #[cfg(unix)]
    fn thread_list_reports_live_and_store_status() {
        let (root, host) = temp_host("status");
        thread(&host, "idle");
        lock(&host.db)
            .execute("INSERT INTO sessions VALUES('busy','b',0)", [])
            .unwrap();
        let busy = Watch::new();
        {
            let mut state = busy.lock();
            state.busy = true;
            state.since = 42;
        }
        lock(&host.lives).insert("busy".into(), busy);
        let rows = host.sessions().unwrap();
        let status = |id: &str| {
            rows.iter()
                .find(|r| r["id"] == id)
                .map(|r| r["status"].clone())
                .unwrap()
        };
        assert_eq!(status("busy"), json!({"state":"running","at":42}));
        // No authority.sqlite yet: the registry row exists but the store does not.
        assert_eq!(status("idle")["state"], "missing");
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn spend_reaches_the_stream_the_thread_list_and_host_usage() {
        let watch = Watch::new();
        Sink(watch.clone(), None).event(Event::Cost {
            usd: 0.25,
            turn: 0.5,
            total: 1.5,
            billing: "priced",
        });
        let event: Value = serde_json::from_str(&watch.lock().events[0].1).unwrap();
        assert_eq!(
            event,
            json!({"type":"cost","usd":0.25,"turn":0.5,"total":1.5,"billing":"priced"})
        );
        let (root, host) = temp_host("spend");
        thread(&host, "t1");
        let dir = host.dir("t1");
        std::fs::remove_dir_all(dir.join("store")).unwrap();
        drop(spindle_store::Store::open(&dir.join("store")).unwrap());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let call = |key: &str, usd: f64, at: Option<u64>| {
            json!({"key":key,"kind":"parent","turn":null,"after":null,"provider":"p","model":"m",
                "billing":"priced","input":10,"output":5,"cache_read":0,"cache_create":0,"usd":usd,"at":at,"count":1})
        };
        // Recorded today, forty days ago, and by an older ledger (no date).
        let ledger = json!({"version":2,"through":0,"counted":0,"turns_through":0,"turns":[],"done":[],
            "calls":[call("a", 0.5, Some(now)), call("b", 2.0, Some(now - 40 * 86_400)), call("c", 4.0, None)]});
        std::fs::write(dir.join("usage.json"), serde_json::to_vec(&ledger).unwrap()).unwrap();
        let rows = host.sessions().unwrap();
        assert_eq!(rows[0]["usd"], json!(6.5));
        let usage = host.usage(0).unwrap();
        assert_eq!(usage["today"]["usd"], json!(0.5));
        assert_eq!(usage["month"]["usd"], json!(0.5));
        assert_eq!(usage["all"]["usd"], json!(6.5));
        assert_eq!(usage["threads"][0]["id"], "t1");
        assert_eq!(usage["threads"][0]["all"]["calls"], 3);
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn steering_and_provider_retries_reach_the_stream_and_compaction_is_validated() {
        let watch = Watch::new();
        let sink = Sink(watch.clone(), None);
        sink.event(Event::Steered {
            text: "also run the tests",
        });
        sink.event(Event::ProviderRetry {
            status: 503,
            seconds: 4,
            attempt: 1,
            compacting: false,
        });
        sink.event(Event::ProviderRetry {
            status: 400,
            seconds: 0,
            attempt: 2,
            compacting: true,
        });
        let events: Vec<Value> = watch
            .lock()
            .events
            .iter()
            .map(|(_, text)| serde_json::from_str(text).unwrap())
            .collect();
        assert_eq!(
            events[0],
            json!({"type":"steered","text":"also run the tests"})
        );
        assert_eq!(
            events[1]["text"],
            "Provider refused (HTTP 503); retrying in 4s (1/6)"
        );
        assert_eq!(
            events[2]["text"],
            "Provider refused (HTTP 400); compacting history and retrying (2/6)"
        );
        assert_eq!(compaction_mode("digest").unwrap().as_str(), "digest");
        assert_eq!(compaction_mode("model").unwrap().as_str(), "model");
        assert!(compaction_mode("summary").is_err());
    }

    #[test]
    fn show_cards_ride_on_the_result_event_of_both_streams() {
        let watch = Watch::new();
        let sink = Sink(watch.clone(), None);
        let cards = json!([{"path":"a.html","kind":"html","title":"a.html","size":3,"revision":4}]);
        sink.event(Event::Display { items: &cards });
        sink.event(Event::ToolResult {
            content: "Shown",
            failed: false,
        });
        sink.event(Event::ToolResult {
            content: "next",
            failed: false,
        });
        sink.event(Event::SettingsApplied {
            text: "Settings changed at the end of the turn: …",
        });
        let live = watch.lock();
        let events: Vec<Value> = live
            .events
            .iter()
            .map(|(_, text)| serde_json::from_str(text).unwrap())
            .collect();
        assert_eq!(
            events[0],
            json!({"type":"result","content":"Shown","failed":false,"display":cards})
        );
        assert!(events[1].get("display").is_none(), "cards go to one result");
        assert_eq!(events[2]["type"], "notice");
        assert_eq!(events.len(), 3, "the cards are no event of their own");
        let api: Vec<Value> = live
            .api
            .events()
            .map(|text| serde_json::from_str(text).unwrap())
            .collect();
        assert_eq!(
            api[0],
            json!({"type":"tool_result","content":"Shown","failed":false,"display":cards})
        );
        assert!(api[1].get("display").is_none());
        assert_eq!(api[2]["type"], "settings_applied");
    }
}
