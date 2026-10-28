//! Commit-before-dispatch boundary for trusted host adapters. No second agent loop
//! or automatic retry: a reserved attempt without a receipt requires reconciliation.
use crate::completion::Attempt;
use crate::transition::EffectIntent;
use crate::{Error, MAX_PAYLOAD, Result, Store};
use dext_core::model::{ModelWait, Phase};
use dext_transcript::Block;
use dext_transcript::checkpoint::NativeResponse;
use dext_transcript::turn::{Effect, TurnContinuation};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::sync::atomic::{AtomicBool, Ordering};

/// Expected bindings from the authenticated host. All retained fields are checked
/// again in the reservation transaction; possession of this value is not approval.
pub struct DispatchRequest {
    pub scope: String,
    pub owner_epoch: u64,
    pub expected_revision: u64,
    pub intent_revision: u64,
    pub intent: EffectIntent,
    pub attempt: Attempt,
}
impl std::fmt::Debug for DispatchRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DispatchRequest")
            .field("expected_revision", &self.expected_revision)
            .finish_non_exhaustive()
    }
}

/// Created only after durable reservation and fresh authority revalidation. The
/// native adapter must still revalidate resources/credentials and contain/reap work.
pub struct CommittedEffect {
    effect: Effect,
    attempt: Attempt,
    revision: u64,
}
impl CommittedEffect {
    pub fn effect(&self) -> &Effect {
        &self.effect
    }
    pub fn attempt(&self) -> &Attempt {
        &self.attempt
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
}
impl std::fmt::Debug for CommittedEffect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommittedEffect")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

pub enum DispatchOutcome {
    /// Durable ingress, not transcript application or workspace publication.
    Recorded(Vec<u8>),
    /// Already ingested, not necessarily semantically valid. No executor is invoked,
    /// even if the head advanced; checked receipt adoption is still mandatory.
    Reused(Vec<u8>),
    /// Physical execution may have happened. Never starts a replacement attempt.
    Reconcile,
}
impl std::fmt::Debug for DispatchOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Recorded(_) => "DispatchOutcome::Recorded(..)",
            Self::Reused(_) => "DispatchOutcome::Reused(..)",
            Self::Reconcile => "DispatchOutcome::Reconcile",
        })
    }
}
enum ResultProfile {
    Native,
    Model,
    #[cfg(feature = "dext-image-continuation")]
    Image([u8; 32]),
    #[cfg(feature = "dext-native-image")]
    ReadImage(bool),
    #[cfg(feature = "workspace-tools")]
    StrictWorkspace,
    /// A tool the embedding app supplies: any name outside the workspace tools.
    #[cfg(feature = "driver")]
    App,
}
impl ResultProfile {
    fn check_effect(&self, effect: &Effect) -> Result<()> {
        match (self, effect) {
            (Self::Native, _) => Ok(()),
            (Self::Model, Effect::Model { .. }) => Ok(()),
            (Self::Model, Effect::Tool { .. }) => Err(Error::Invalid),
            #[cfg(feature = "dext-image-continuation")]
            (Self::Image(_), Effect::Tool { .. }) => Ok(()),
            #[cfg(feature = "dext-image-continuation")]
            (Self::Image(_), Effect::Model { .. }) => Err(Error::Invalid),
            #[cfg(feature = "dext-native-image")]
            (Self::ReadImage(_), effect) => check_read_image(effect),
            #[cfg(feature = "workspace-tools")]
            (Self::StrictWorkspace, effect) => strict_tool(effect).map(|_| ()),
            #[cfg(feature = "driver")]
            (Self::App, Effect::Tool { call, .. })
                if crate::turn::delegated_tool(&call.name) && call.input.is_object() =>
            {
                Ok(())
            }
            #[cfg(feature = "driver")]
            (Self::App, _) => Err(Error::Invalid),
        }
    }
}

/// What reserving a turn's model request came to (`Store::reserve_turn_model`).
pub enum ModelReservation {
    /// Reached without sending: a reply recorded before (`Reused`), or a reservation
    /// an owner made and never recorded (`Reconcile`).
    Settled(DispatchOutcome),
    /// Reserved now: send it once and hand the response to `ingest_turn_model`.
    Reserved(Box<CommittedEffect>),
}

/// What reserving an app call came to (`Store::reserve_app_call`).
#[cfg(feature = "driver")]
pub enum AppReservation {
    Settled(DispatchOutcome),
    Reserved {
        call: dext_core::response::ToolCall<serde_json::Value>,
        again: bool,
    },
}

enum PreparedDispatch {
    Complete(DispatchOutcome),
    Execute(Box<CommittedEffect>),
}

enum Reservation {
    Fresh,
    Recorded(Vec<u8>),
    Unresolved,
}

fn validate_request(request: &DispatchRequest) -> Result<()> {
    if !crate::valid_label(&request.scope)
        || !crate::valid_label(&request.intent.id)
        || !crate::valid_label(&request.intent.adapter)
        || !crate::valid_label(&request.attempt.id)
        || !crate::valid_label(&request.attempt.implementation)
        || !crate::valid_label(&request.attempt.grant_reference)
        || request.intent.id != request.attempt.effect
        || request.intent.input.len() > MAX_PAYLOAD
        || request.owner_epoch == 0
        || request.owner_epoch > i64::MAX as u64
        || request.attempt.owner_epoch == 0
        || request.attempt.owner_epoch > i64::MAX as u64
        || request.intent_revision == 0
        || request.intent_revision > i64::MAX as u64
        || request.expected_revision > i64::MAX as u64
    {
        return Err(Error::Invalid);
    }
    Ok(())
}

/// Post-reservation check: tolerates suspension so reserved work can drain.
fn pending_effect(db: &rusqlite::Connection, request: &DispatchRequest) -> Result<Effect> {
    crate::ownership::live_allowed(db, &request.scope)?;
    let cancelled: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM cancellations WHERE scope=?1)",
        [&request.scope],
        |r| r.get(0),
    )?;
    if cancelled {
        return Err(Error::Cancelled);
    }
    bound_effect(db, request)
}

/// The tool call `request` reserved, still pending at the scope head with no recorded
/// result: reserved by this owner (`epoch`) or by an earlier one whose epoch has ended.
#[cfg(feature = "driver")]
fn reserved_effect(
    db: &rusqlite::Connection,
    epoch: u64,
    request: &DispatchRequest,
) -> Result<Effect> {
    // This owner's epoch must still be the realm's (a fenced owner records nothing).
    let realm: u64 = db.query_row("SELECT epoch FROM realm WHERE id=1", [], |r| {
        crate::nonnegative_id(r, 0)
    })?;
    if request.owner_epoch != epoch || realm != epoch || request.attempt.owner_epoch > epoch {
        return Err(Error::Stale);
    }
    let attempt: Option<(String, u64, String, String)> = db
        .query_row(
            "SELECT effect,owner_epoch,implementation,grant_reference FROM attempts WHERE id=?1",
            [&request.attempt.id],
            |r| {
                Ok((
                    crate::bounded_label(r, 0)?,
                    crate::nonnegative_id(r, 1)?,
                    crate::bounded_label(r, 2)?,
                    crate::bounded_label(r, 3)?,
                ))
            },
        )
        .optional()?;
    let recorded: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM receipts WHERE attempt=?1)",
        [&request.attempt.id],
        |r| r.get(0),
    )?;
    if attempt
        != Some((
            request.attempt.effect.clone(),
            request.attempt.owner_epoch,
            request.attempt.implementation.clone(),
            request.attempt.grant_reference.clone(),
        ))
        || recorded
        || crate::rewind::superseded(db, &request.intent.id)?
    {
        return Err(Error::Conflict);
    }
    let intent: Option<(u64, String, Vec<u8>, String)> = db.query_row(
        "SELECT i.revision,i.adapter,i.input,r.scope FROM intents i JOIN revisions r ON r.revision=i.revision WHERE i.id=?1",
        [&request.intent.id],
        |row| Ok((crate::nonnegative_id(row, 0)?, crate::bounded_label(row, 1)?, crate::packed_blob(db, row, 2, MAX_PAYLOAD)?, crate::bounded_label(row, 3)?)),
    ).optional()?;
    if intent
        != Some((
            request.intent_revision,
            request.intent.adapter.clone(),
            request.intent.input.clone(),
            request.scope.clone(),
        ))
    {
        return Err(Error::Conflict);
    }
    crate::ownership::live_allowed(db, &request.scope)?;
    let cancelled: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM cancellations WHERE scope=?1)",
        [&request.scope],
        |r| r.get(0),
    )?;
    if cancelled {
        return Err(Error::Cancelled);
    }
    head_effect(db, request)
}

fn bound_effect(db: &rusqlite::Connection, request: &DispatchRequest) -> Result<Effect> {
    let epoch: u64 = db.query_row("SELECT epoch FROM realm WHERE id=1", [], |r| {
        crate::nonnegative_id(r, 0)
    })?;
    if epoch != request.owner_epoch || request.attempt.owner_epoch != epoch {
        return Err(Error::Stale);
    }
    head_effect(db, request)
}

/// The still-pending obligation at the scope head that exactly matches the intent.
fn head_effect(db: &rusqlite::Connection, request: &DispatchRequest) -> Result<Effect> {
    let selected = crate::transition::scope_revision(db, &request.scope)?;
    let (revision, scope, engine, bytes): (u64, String, String, Vec<u8>) = db.query_row(
        "SELECT revision,scope,engine,continuation FROM revisions WHERE revision=?1",
        [selected as i64],
        |r| {
            Ok((
                crate::nonnegative_id(r, 0)?,
                crate::bounded_label(r, 1)?,
                crate::bounded_label(r, 2)?,
                crate::packed_blob(db, r, 3, MAX_PAYLOAD)?,
            ))
        },
    )?;
    if revision != request.expected_revision {
        return Err(Error::Stale);
    }
    if scope != request.scope || engine != crate::turn::ENGINE {
        return Err(Error::Conflict);
    }
    let state = TurnContinuation::restore(&bytes).map_err(|_| Error::Incompatible)?;
    if state.scope() != request.scope {
        return Err(Error::Conflict);
    }
    let effect = state
        .pending_effects()
        .map_err(|_| Error::Incompatible)?
        .into_iter()
        .find(|effect| match effect {
            Effect::Model { id, .. } | Effect::Tool { id, .. } => id == &request.intent.id,
        })
        .ok_or(Error::Conflict)?;
    let matches = match &effect {
        Effect::Model { waiting, .. } => {
            request.intent.adapter == "dext-model-v1"
                && request.intent.input == waiting.checkpoint()
        }
        Effect::Tool { call, .. } => {
            request.intent.adapter == call.name
                && request.intent.input
                    == serde_json::to_vec(&call.input).map_err(|_| Error::Invalid)?
        }
    };
    if !matches {
        return Err(Error::Conflict);
    }
    Ok(effect)
}

/// CLI parity at fresh reservation only: a call the shared runtime guards would
/// short-circuit is never dispatched (the host commits Action::Guard instead).
/// Guard verdicts use batch-start state, so they cannot change mid-batch.
fn refuse_guarded(db: &rusqlite::Connection, request: &DispatchRequest) -> Result<()> {
    let selected = crate::transition::scope_revision(db, &request.scope)?;
    let bytes: Vec<u8> = db.query_row(
        "SELECT continuation FROM revisions WHERE revision=?1",
        [selected as i64],
        |r| crate::packed_blob(db, r, 0, MAX_PAYLOAD),
    )?;
    let state = TurnContinuation::restore(&bytes).map_err(|_| Error::Incompatible)?;
    if state
        .guard_tool(&request.intent.id)
        .map_err(|_| Error::Invalid)?
        .is_some()
    {
        return Err(Error::Conflict);
    }
    Ok(())
}

#[cfg(feature = "dext-native-image")]
/// Supplied by current host resource policy for this exact committed call. The
/// handle and content approval are ephemeral; labels and saved paths grant nothing.
pub struct ImageSource {
    file: std::fs::File,
    approved_digest: [u8; 32],
}
#[cfg(feature = "dext-native-image")]
impl ImageSource {
    /// The host must bind the handle/hash to the call's authorized workspace path.
    pub fn new(file: std::fs::File, approved_digest: [u8; 32]) -> Self {
        Self {
            file,
            approved_digest,
        }
    }
}
#[cfg(feature = "dext-native-image")]
impl std::fmt::Debug for ImageSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageSource").finish_non_exhaustive()
    }
}

#[cfg(feature = "dext-native-image")]
fn check_read_image(effect: &Effect) -> Result<()> {
    let Effect::Tool { call, .. } = effect else {
        return Err(Error::Invalid);
    };
    let input = call.input.as_object().ok_or(Error::Invalid)?;
    let path = input
        .get("path")
        .and_then(serde_json::Value::as_str)
        .ok_or(Error::Invalid)?;
    if call.name != "read_image"
        || input.len() != 1
        || path.is_empty()
        || path.len() > 16 * 1024
        || path.contains('\0')
    {
        return Err(Error::Invalid);
    }
    Ok(())
}

/// Largest text a file tool accepts per string field. Tool inputs stay in history,
/// and one revision must hold the history plus the next request that repeats it
/// (escaped twice in chat-completions arguments), within MAX_PAYLOAD.
#[cfg(feature = "workspace-tools")]
pub const MAX_FILE_TOOL_TEXT: usize = 1024 * 1024;

/// Largest window read_file and read_output return in one call (`max_output` and
/// `limit` respectively); windows are exempt from the turn's output budget.
#[cfg(feature = "driver")]
pub const MAX_READ_WINDOW: usize = 256 * 1024;

/// Tool output one prompt's turn keeps in its transcript across bash and run_code
/// results (paging with read_file and read_output is not charged). Larger output is
/// retained as an artifact and its pointer always survives.
#[cfg(feature = "driver")]
pub const TURN_OUTPUT_BUDGET: usize = 256 * 1024;

/// What a host tool did (`Store::dispatch_host_workspace`).
#[cfg(feature = "workspace-tools")]
pub struct HostResult {
    /// The result text, as the model reads it.
    pub content: String,
    pub success: bool,
    /// Files that replace a subtree of the workspace; None leaves it as it was.
    pub replace: Option<Replace>,
}

#[cfg(feature = "workspace-tools")]
impl HostResult {
    /// A result that changes nothing.
    pub fn text(content: impl Into<String>, success: bool) -> Self {
        Self {
            content: content.into(),
            success,
            replace: None,
        }
    }
}

/// What the app answered for a call of one of its tools (`Store::dispatch_app_workspace`).
#[cfg(feature = "driver")]
pub struct AppResult {
    /// The result text, as the model reads it.
    pub content: String,
    pub success: bool,
    /// Files the call wrote (workspace path, bytes, executable), kept with its result.
    pub writes: Vec<(String, Vec<u8>, bool)>,
    /// Workspace paths it removed; a directory's path removes everything under it.
    pub deletes: Vec<String>,
    pub links: Vec<crate::workspace::Link>,
    pub directories: Vec<String>,
    /// Its change to the app state (a JSON merge patch), kept with the result and set
    /// by the step that adopts it.
    pub state: Option<Vec<u8>>,
    pub details: Option<Vec<u8>>,
    pub max_output: usize,
}

/// Workspace changes an answer brings: files written (path, bytes, executable), paths
/// removed (a directory's removes everything under it), links and empty directories.
#[cfg(feature = "driver")]
struct Changes {
    writes: Vec<(String, Vec<u8>, bool)>,
    deletes: Vec<String>,
    links: Vec<crate::workspace::Link>,
    directories: Vec<String>,
}

#[cfg(feature = "driver")]
impl Changes {
    fn is_empty(&self) -> bool {
        self.writes.is_empty()
            && self.deletes.is_empty()
            && self.links.is_empty()
            && self.directories.is_empty()
    }
}

/// A reserved `bash` call for a runner outside the store (`Store::reserve_runner_call`):
/// the command as it is run, its timeout in seconds and the workspace root it runs
/// over (`runner_tree`, `runner_file`).
#[cfg(feature = "workspace-tools")]
pub struct RunnerCall {
    pub command: String,
    pub timeout: u64,
    pub root: String,
}

/// What reserving a runner call came to.
#[cfg(feature = "workspace-tools")]
pub enum RunnerReservation {
    /// Reached without running: a result recorded before (`Reused`), one recorded now
    /// without running (the store is full), or a reservation an earlier owner made and
    /// never recorded (`Reconcile`; the driver seals it as interrupted).
    Settled(DispatchOutcome),
    /// Reserved now: run it once and hand what happened to `record_runner_result`.
    Reserved(RunnerCall),
}

/// What a runner reported for a `bash` call: its output (stdout, then stderr), whether
/// the command succeeded, and the workspace changes it made. `failure` says why the
/// run could not start or finish; its changes are then not kept.
#[cfg(feature = "workspace-tools")]
#[derive(Default)]
pub struct RunnerResult {
    pub output: String,
    pub success: bool,
    pub writes: Vec<(String, Vec<u8>, bool)>,
    pub deletes: Vec<String>,
    pub links: Vec<crate::workspace::Link>,
    pub directories: Vec<String>,
    pub failure: Option<String>,
}

/// Every file under `prefix` ("" for the whole workspace) replaced by `files`, whose
/// paths are relative to the prefix, and `links` (workspace path, target) added where
/// nothing is yet.
#[cfg(feature = "workspace-tools")]
pub struct Replace {
    pub prefix: String,
    pub files: Vec<(String, Vec<u8>, bool)>,
    pub links: Vec<(String, String)>,
    /// Called with the new workspace root once it is staged and before the result is
    /// recorded, so the host can durably note something about that root first (an
    /// error records the call as failed over the unchanged workspace).
    pub staged: Option<Staged>,
}

/// A host tool's hook for the root its change staged (`Replace::staged`).
#[cfg(feature = "workspace-tools")]
pub type Staged = Box<dyn FnOnce(&str) -> std::result::Result<(), String>>;

/// The workspace a host tool call was dispatched on, read-only.
#[cfg(feature = "driver")]
pub struct HostWorkspace<'a> {
    store: &'a mut Store,
    scope: &'a str,
    root: &'a str,
    tree: &'a crate::workspace::Tree,
    revision: u64,
}

#[cfg(feature = "driver")]
impl HostWorkspace<'_> {
    /// The revision whose workspace this is (the call's pending step).
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// The workspace root.
    pub fn root(&self) -> &str {
        self.root
    }
    /// Its files (path, object digest, executable), in path order.
    pub fn files(&self) -> &[crate::workspace::Entry] {
        &self.tree.files
    }
    pub fn links(&self) -> &[crate::workspace::Link] {
        &self.tree.links
    }
    pub fn directories(&self) -> &[String] {
        &self.tree.directories
    }
    /// Whether `path` is an empty directory or a link (not a file).
    pub fn is_directory(&self, path: &str) -> bool {
        self.tree.directories.iter().any(|d| d == path)
            || self
                .tree
                .files
                .iter()
                .any(|f| f.path.starts_with(&format!("{path}/")))
    }
    /// The size of a file's bytes, without reading them.
    pub fn size(&mut self, object: &str) -> Result<u64> {
        let length: i64 = self.store.db.query_row(
            "SELECT length(bytes) FROM objects WHERE digest=?1",
            [object],
            |row| row.get(0),
        )?;
        Ok(length.max(0) as u64)
    }
    /// A file's bytes.
    pub fn read(&mut self, object: &str) -> Result<Vec<u8>> {
        self.store.read_object(self.scope, object)
    }
}

/// One call accepted by the strict workspace adapter. File tools are computed
/// in-process over the immutable base; only bash runs in the sandbox.
#[cfg(feature = "workspace-tools")]
#[cfg_attr(not(feature = "dext-strict-workspace"), allow(dead_code))]
enum StrictTool<'a> {
    Bash {
        command: &'a str,
        timeout: u64,
    },
    /// One script run as a single sandboxed step; on failure its workspace changes
    /// are discarded, so a script is all-or-nothing.
    Code {
        language: CodeLanguage,
        code: &'a str,
        timeout: u64,
    },
    Read {
        path: String,
        offset: usize,
        limit: usize,
        window: usize,
    },
    Output {
        artifact: String,
        offset: usize,
        limit: usize,
    },
    Write {
        path: String,
        content: &'a str,
    },
    Edit {
        path: String,
        old: &'a str,
        new: &'a str,
        all: bool,
    },
    /// A background job over a snapshot of the workspace; nothing it does is sealed.
    JobStart {
        command: &'a str,
        lifetime: u64,
    },
    /// A job's output from `offset` (default: its newest output), after waiting up to
    /// `wait` seconds for more; without a job, the session's jobs.
    JobRead {
        job: Option<&'a str>,
        offset: Option<u64>,
        limit: usize,
        wait: u64,
    },
    JobStop {
        job: &'a str,
    },
    /// A tool the host runs in-process (`crate::turn::HOST_TOOLS`), its input checked
    /// for shape here and for meaning by the host.
    Host {
        name: &'a str,
    },
}

/// A background job's default lifetime, in seconds.
#[cfg(feature = "workspace-tools")]
pub const JOB_SECS: u64 = 3600;
/// Longest wait of one read_job call, in seconds.
#[cfg(feature = "workspace-tools")]
pub const MAX_JOB_WAIT: u64 = 600;
/// Largest read_job window.
#[cfg(feature = "workspace-tools")]
pub const MAX_JOB_WINDOW: usize = 64 * 1024;

#[cfg(feature = "workspace-tools")]
#[derive(Clone, Copy)]
enum CodeLanguage {
    Python,
    JavaScript,
    TypeScript,
}

/// Helpers shared by both languages: read, write, edit, ls and sh run inside the
/// sandbox, so a script performs many workspace operations in one model step.
#[cfg(feature = "dext-strict-workspace")]
const PYTHON_PRELUDE: &str = r#"import glob as _glob, os as _os, subprocess as _subprocess, sys as _sys
def read(path):
    with open(path, encoding="utf-8") as file:
        return file.read()
def write(path, text):
    folder = _os.path.dirname(path)
    if folder:
        _os.makedirs(folder, exist_ok=True)
    with open(path, "w", encoding="utf-8") as file:
        file.write(text)
def edit(path, old, new, count=1):
    if not old or not isinstance(count, int) or isinstance(count, bool) or count < 0:
        raise ValueError("edit: nonempty old text and a nonnegative integer count required")
    text = read(path)
    found = text.count(old)
    if found == 0:
        raise ValueError(f"edit: text not found in {path}")
    if count == 1 and found > 1:
        raise ValueError(f"edit: text occurs {found} times in {path}; pass count=0 to replace all")
    write(path, text.replace(old, new) if count == 0 else text.replace(old, new, count))
    return found if count == 0 else min(found, count)
def ls(pattern="**/*"):
    return sorted(p for p in _glob.glob(pattern, recursive=True) if _os.path.isfile(p))
def sh(command, check=True):
    if not _sys.argv[2]:
        raise RuntimeError("sh: bash is not available on this host; install it or set SPINDLE_BASH")
    run = _subprocess.run([_sys.argv[2], "--noprofile", "--norc", "-o", "pipefail", "-c", command], capture_output=True, text=True, stdin=_subprocess.DEVNULL)
    out = run.stdout + run.stderr
    if check and run.returncode:
        raise RuntimeError(f"sh exited {run.returncode}: {out[-2000:]}")
    return out
import json as _json, threading as _threading, types as _types
_child_id = 0
_rpc_seq = 0
_rpc_lock = _threading.Lock()
_rpc_buffer = b''
# Requests and replies use their own pipes (SPINDLE_RPC_FDS=<reply>,<request>), so
# printed output can never be taken for a request; Windows hosts frame them on stdio.
_rpc_fds = [int(_fd) for _fd in _os.environ.pop('SPINDLE_RPC_FDS', '').split(',') if _fd]
for _fd in _rpc_fds:
    _os.set_inheritable(_fd, False)
def _rpc_line():
    global _rpc_buffer
    if not _rpc_fds:
        return _sys.stdin.buffer.readline()
    while b'\n' not in _rpc_buffer:
        chunk = _os.read(_rpc_fds[0], 1 << 16)
        if not chunk:
            raise RuntimeError('the host closed the code-mode channel')
        _rpc_buffer += chunk
        if len(_rpc_buffer) > 3 << 20:
            raise RuntimeError('broker reply too large')
    line, _, _rpc_buffer = _rpc_buffer.partition(b'\n')
    return line
def _rpc(method, value, width=1):
    global _child_id, _rpc_seq
    # One call at a time, so threads in a script each get their own reply.
    with _rpc_lock:
        _rpc_seq += 1
        seq = _rpc_seq
        frame = (_json.dumps({'seq':seq,'id':_child_id,'method':method,'input':value}) + '\n').encode()
        if len(frame) > 64 << 20:
            raise RuntimeError(f'{method} request is {len(frame) >> 20} MiB; the limit is 64 MiB, so split the batch or shrink the images')
        if method in ('model','tool','batch','http'):
            _child_id += width
        if _rpc_fds:
            view = memoryview(frame)
            while view:
                view = view[_os.write(_rpc_fds[1], view):]
        else:
            _sys.stdout.write('\x1e' + frame.decode())
            _sys.stdout.flush()
        reply = _json.loads(_rpc_line())
    if reply.get('seq') != seq:
        raise RuntimeError('code-mode reply out of order; scripts must not use the channel directly')
    if not reply.get('ok'):
        raise RuntimeError(str(reply.get('error', 'external call failed')) + (' [may_have_run]' if reply.get('may_have_run') else ''))
    return reply.get('data')
def _model_input(prompt, provider=None, max_tokens=None, **options):
    return {k: v for k, v in dict(options,prompt=prompt,provider=provider,max_tokens=max_tokens).items() if v is not None}
def image(path):
    import base64 as _b64
    with open(path, 'rb') as file:
        data = file.read()
    kind = 'image/png' if data[:8] == b'\x89PNG\r\n\x1a\n' else 'image/jpeg' if data[:3] == b'\xff\xd8\xff' else 'image/gif' if data[:4] == b'GIF8' else 'image/webp' if data[:4] == b'RIFF' and data[8:12] == b'WEBP' else None
    if kind is None:
        raise ValueError(f'image: {path} is not PNG, JPEG, GIF or WebP')
    return {'media_type': kind, 'data': _b64.b64encode(data).decode('ascii')}
def _tool_input(name, arguments=None):
    return {'name':name,'arguments':arguments or {}}
def model(prompt, provider=None, max_tokens=None, **options):
    return _rpc('model',_model_input(prompt,provider,max_tokens,**options))
def tool(name, arguments=None):
    return _rpc('tool',_tool_input(name,arguments))
def batch(calls):
    calls = [{'method':m,'input':_model_input(**i) if m == 'model' else dict(i) if m == 'http' else _tool_input(**i)} for m, i in calls]
    return _rpc('batch',{'calls':calls},len(calls))
def http(url, options=None, **kwargs):
    return _rpc('http',{k: v for k, v in dict(options or {}, **kwargs, url=url).items() if v is not None})
def limits():
    return _rpc('limits',{})
def models(prompts, **options):
    return [r.get('data') if r.get('ok') else {'error':r.get('error'),'may_have_run':r.get('may_have_run',False)} for r in batch([('model',dict(options,prompt=p)) for p in prompts])]
class _Tools:
    def search(self, query=''): return _rpc('search',{'query':query})
    def describe(self, name): return _rpc('describe',{'name':name})
    def call(self, name, arguments=None): return tool(name,arguments)
tools = _Tools()
class _UI:
    def skin(self, css):
        write('.spindle/skin.css', css)
        return '.spindle/skin.css'
ui = _UI()
spindle = _types.ModuleType('spindle')
for _name in ('read','write','edit','ls','sh','model','tool','tools','batch','models','limits','ui','image','http'):
    setattr(spindle, _name, globals()[_name])
_sys.modules['spindle'] = spindle
_sys.dont_write_bytecode = True
# Helpers first, workspace last: a workspace file named like a stdlib module must not shadow it.
_sys.path.insert(0, _os.path.join(_os.getcwd(), '.spindle', 'helpers'))
_sys.path.append(_os.getcwd())
_os.environ.pop("PWD", None)
exec(compile(_sys.argv[1], "<run_code>", "exec"))
"#;

/// Scripts travel as one argv element; Windows caps a whole command line near 32K.
pub const MAX_SCRIPT: usize = if cfg!(windows) { 16 * 1024 } else { 64 * 1024 };

#[cfg(feature = "dext-strict-workspace")]
const JAVASCRIPT_PRELUDE: &str = r#"const fs = require("fs"), path = require("path"), cp = require("child_process");
const read = (file) => fs.readFileSync(file, "utf8");
const write = (file, text) => { fs.mkdirSync(path.dirname(file), { recursive: true }); fs.writeFileSync(file, text); };
const edit = (file, oldText, newText, count = 1) => {
  if (!oldText || !Number.isSafeInteger(count) || count < 0) throw new Error('edit: nonempty old text and a nonnegative integer count required');
  const text = read(file), found = text.split(oldText).length - 1;
  if (found === 0) throw new Error(`edit: text not found in ${file}`);
  if (count === 1 && found > 1) throw new Error(`edit: text occurs ${found} times in ${file}; pass count=0 to replace all`);
  const parts = text.split(oldText), replacements = count === 0 ? found : Math.min(count, found);
  write(file, parts.slice(0, replacements + 1).join(newText) + (replacements < found ? oldText + parts.slice(replacements + 1).join(oldText) : ''));
  return replacements;
};
const ls = (pattern = "**/*") => fs.globSync(pattern).filter((f) => fs.statSync(f).isFile()).sort();
const sh = (command, check = true) => {
  if (!process.argv[2]) throw new Error("sh: bash is not available on this host; install it or set SPINDLE_BASH");
  const run = cp.spawnSync(process.argv[2], ["--noprofile", "--norc", "-o", "pipefail", "-c", command], { encoding: "utf8", maxBuffer: 64 << 20, stdio: ["ignore", "pipe", "pipe"] });
  if (run.error) throw run.error;
  const out = (run.stdout || "") + (run.stderr || "");
  if (check && run.status !== 0) throw new Error(`sh exited ${run.status}: ${out.slice(-2000)}`);
  return out;
};
let childId = 0, rpcSeq = 0, rpcPending = Buffer.alloc(0);
// Requests and replies use their own pipes (SPINDLE_RPC_FDS=<reply>,<request>), so
// printed output can never be taken for a request; Windows hosts frame them on stdio.
const rpcFds = (process.env.SPINDLE_RPC_FDS || '').split(',').filter(Boolean).map(Number);
delete process.env.SPINDLE_RPC_FDS;
const [replyFd, requestFd] = rpcFds.length ? rpcFds : [0, 1];
const pause = (ms) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
// Node makes stdio pipes non-blocking once console.log has run, so writes can be partial
// and reads can raise EAGAIN; both are retried instead of truncating an RPC frame.
const retry = (op) => { for (;;) { try { return op(); } catch (error) { if (error.code !== 'EAGAIN') throw error; pause(1); } } };
const rpc = (method, input, width = 1) => {
  const seq = ++rpcSeq;
  const body = JSON.stringify({seq, id: childId, method, input}) + '\n';
  const frame = Buffer.from(rpcFds.length ? body : '\x1e' + body);
  if (frame.length > 64 << 20) throw new Error(`${method} request is ${frame.length >> 20} MiB; the limit is 64 MiB, so split the batch or shrink the images`);
  if (method === 'model' || method === 'tool' || method === 'batch' || method === 'http') childId += width;
  for (let offset = 0; offset < frame.length;) offset += retry(() => fs.writeSync(requestFd, frame, offset, frame.length - offset));
  // One request is outstanding at a time, so a reply never shares a read with the next.
  const chunk = Buffer.alloc(65536);
  let newline;
  while ((newline = rpcPending.indexOf(10)) < 0) {
    const count = retry(() => fs.readSync(replyFd, chunk, 0, chunk.length, null));
    if (!count) throw new Error('the host closed the code-mode channel');
    rpcPending = Buffer.concat([rpcPending, chunk.subarray(0, count)]);
    if (rpcPending.length > 3 << 20) throw new Error('broker reply too large');
  }
  const reply = JSON.parse(rpcPending.subarray(0, newline).toString('utf8'));
  rpcPending = rpcPending.subarray(newline + 1);
  if (reply.seq !== seq) throw new Error('code-mode reply out of order; scripts must not use the channel directly');
  if (!reply.ok) throw new Error((reply.error || 'external call failed') + (reply.may_have_run ? ' [may_have_run]' : ''));
  return reply.data;
};
const model = (prompt, options = {}) => rpc('model', {prompt, ...options});
const tool = (name, args = {}) => rpc('tool', {name, arguments:args});
const tools = {search:(query='')=>rpc('search',{query}), describe:name=>rpc('describe',{name}), call:tool};
const batch = (calls) => {
  const list = calls.map(({method, input = {}}) => ({method, input: method === 'tool' ? {name: input.name, arguments: input.arguments || {}} : {...input}}));
  return rpc('batch', {calls: list}, list.length);
};
const models = (prompts, options = {}) => batch(prompts.map((prompt) => ({method: 'model', input: {prompt, ...options}})))
  .map((r) => r.ok ? r.data : {error: r.error, may_have_run: !!r.may_have_run});
const limits = () => rpc('limits', {});
const http = (url, options = {}) => rpc('http', {...options, url});
const image = (file) => {
  const data = fs.readFileSync(file);
  const kind = data.subarray(0, 8).equals(Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])) ? 'image/png'
    : data[0] === 0xff && data[1] === 0xd8 && data[2] === 0xff ? 'image/jpeg'
    : data.subarray(0, 4).toString('latin1') === 'GIF8' ? 'image/gif'
    : data.subarray(0, 4).toString('latin1') === 'RIFF' && data.subarray(8, 12).toString('latin1') === 'WEBP' ? 'image/webp' : null;
  if (!kind) throw new Error(`image: ${file} is not PNG, JPEG, GIF or WebP`);
  return {media_type: kind, data: data.toString('base64')};
};
const ui = {skin: (css) => {write('.spindle/skin.css', css); return '.spindle/skin.css';}};
globalThis.spindle = {read, write, edit, ls, sh, model, tool, tools, batch, models, limits, ui, image, http};
module.paths.unshift(path.resolve('.spindle/helpers'), path.resolve('node_modules'));
let code = process.argv[1];
const names = ['read','write','edit','ls','sh','require','model','tool','tools','batch','models','limits','ui','image','http'];
let script;
if (process.argv[3]) {
  const ts = require(process.argv[3]);
  const compiled = ts.transpileModule(`(async function(${names.join(',')}) {\n{\n${code}\n}\n})`, {compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.None},reportDiagnostics:true});
  const errors = (compiled.diagnostics || []).filter(d=>d.category===ts.DiagnosticCategory.Error);
  if (errors.length) throw new Error(errors.map(d=>ts.flattenDiagnosticMessageText(d.messageText,'\n')).join('\n'));
  script = new Function('return ' + compiled.outputText)();
} else {
  const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
  // A block lets the script declare names that shadow helper parameters (const image = ...).
  script = new AsyncFunction(...names, `{\n${code}\n}`);
}
script(read, write, edit, ls, sh, require, model, tool, tools, batch, models, limits, ui, image, http)
  .catch((error) => { console.error((error && error.stack) || String(error)); process.exitCode = 1; });
"#;

#[cfg(feature = "dext-strict-workspace")]
impl CodeLanguage {
    /// The interpreter command line, or why this runner has no interpreter. `sh()`
    /// inside a script runs the runner's bash, passed as the script's second argument
    /// (empty when the host has none, so `sh()` fails with a clear message).
    fn argv(
        self,
        code: &str,
        executor: &dext_native::strict::StrictExecutor,
    ) -> std::result::Result<Vec<String>, String> {
        use dext_native::strict::Program;
        let (program, name) = match self {
            Self::Python => (Program::Python, "python"),
            Self::JavaScript | Self::TypeScript => (Program::Node, "node"),
        };
        let interpreter = executor.program(program).ok_or(format!(
            "{name} is not available on this host; install it or set {}",
            program.variable()
        ))?;
        let bash = executor.program(Program::Bash).unwrap_or_default();
        Ok(match self {
            Self::Python => vec![
                interpreter,
                "-I".into(),
                "-c".into(),
                PYTHON_PRELUDE.into(),
                code.into(),
                bash,
            ],
            Self::JavaScript | Self::TypeScript => vec![
                interpreter,
                "--max-old-space-size=128".into(),
                "--jitless".into(),
                "--no-expose-wasm".into(),
                "-e".into(),
                JAVASCRIPT_PRELUDE.into(),
                code.into(),
                bash,
                if matches!(self, Self::TypeScript) {
                    let source = typescript_source()?;
                    if executor.isolated() {
                        "/typescript/lib/typescript.js".into()
                    } else {
                        source
                            .join("lib/typescript.js")
                            .to_string_lossy()
                            .into_owned()
                    }
                } else {
                    String::new()
                },
            ],
        })
    }
}

#[cfg(feature = "dext-strict-workspace")]
fn typescript_source() -> std::result::Result<std::path::PathBuf, String> {
    let source = std::env::var_os("SPINDLE_TYPESCRIPT_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::current_exe().ok().and_then(|p| {
                p.parent()?
                    .parent()
                    .map(|p| p.join("code-runtime/node_modules/typescript"))
            })
        })
        .ok_or(
            "TypeScript needs SPINDLE_TYPESCRIPT_DIR pointing to a reviewed typescript package",
        )?;
    if !source.is_absolute()
        || source.file_name().is_none_or(|n| n != "typescript")
        || !source.join("lib/typescript.js").is_file()
    {
        return Err(
            "SPINDLE_TYPESCRIPT_DIR must be an absolute typescript package directory".into(),
        );
    }
    Ok(source)
}

/// Accepts `name`, `./name` and `/workspace/name`; the result is a valid map path that
/// the runner can also materialize (Windows hosts keep a portable alphabet, so a name
/// cannot carry a separator, drive or stream there).
#[cfg(feature = "workspace-tools")]
fn workspace_path(raw: &str) -> Option<String> {
    let relative = raw
        .strip_prefix("/workspace/")
        .or_else(|| raw.strip_prefix("./"))
        .unwrap_or(raw);
    (crate::workspace::valid_path(relative) && dext_native::limits::valid_path(relative))
        .then(|| relative.to_string())
}

#[cfg(feature = "workspace-tools")]
fn strict_tool(effect: &Effect) -> Result<StrictTool<'_>> {
    use serde_json::Value;
    let Effect::Tool { call, .. } = effect else {
        return Err(Error::Invalid);
    };
    dext_native::tool_policy::validate_tool_input(&call.name, &call.input)
        .map_err(|_| Error::Invalid)?;
    let input = call.input.as_object().ok_or(Error::Invalid)?;
    let allowed = strict_fields(&call.name).ok_or(Error::Invalid)?;
    if input.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(Error::Invalid);
    }
    // Any non-negative size is accepted; receipts clamp it into the 4096-32768 window.
    if input
        .get("max_output")
        .is_some_and(|size| size.as_u64().is_none())
    {
        return Err(Error::Invalid);
    }
    let text = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| value.len() <= MAX_FILE_TOOL_TEXT)
            .ok_or(Error::Invalid)
    };
    let path = || {
        input
            .get("path")
            .and_then(Value::as_str)
            .and_then(workspace_path)
            .ok_or(Error::Invalid)
    };
    Ok(match call.name.as_str() {
        "bash" => {
            let command = input
                .get("command")
                .and_then(Value::as_str)
                .ok_or(Error::Invalid)?;
            let timeout = match input.get("timeout") {
                None => 30,
                Some(value) => value.as_u64().ok_or(Error::Invalid)?,
            };
            if command.is_empty()
                || command.len() > 16 * 1024
                || command.contains('\0')
                || !(1..=dext_native::limits::MAX_RUN_SECS).contains(&timeout)
            {
                return Err(Error::Invalid);
            }
            dext_native::tool_policy::apply_bash_guardrails(command, false)
                .map_err(|_| Error::Invalid)?;
            StrictTool::Bash { command, timeout }
        }
        "run_code" => {
            let language = match input.get("language").and_then(Value::as_str) {
                Some("python") => CodeLanguage::Python,
                Some("javascript") => CodeLanguage::JavaScript,
                Some("typescript") => CodeLanguage::TypeScript,
                _ => return Err(Error::Invalid),
            };
            let code = input
                .get("code")
                .and_then(Value::as_str)
                .filter(|code| !code.trim().is_empty() && code.len() <= MAX_SCRIPT)
                .filter(|code| !code.contains('\0'))
                .ok_or(Error::Invalid)?;
            let timeout = match input.get("timeout") {
                None => 60,
                Some(value) => value
                    .as_u64()
                    .filter(|timeout| (1..=dext_native::limits::MAX_RUN_SECS).contains(timeout))
                    .ok_or(Error::Invalid)?,
            };
            StrictTool::Code {
                language,
                code,
                timeout,
            }
        }
        "read_file" => {
            let number = |key: &str, default: usize| match input.get(key) {
                None => Ok(default),
                Some(value) => value
                    .as_u64()
                    .filter(|number| (1..=1_000_000).contains(number))
                    .map(|number| number as usize)
                    .ok_or(Error::Invalid),
            };
            StrictTool::Read {
                path: path()?,
                offset: number("offset", 1)?,
                limit: number("limit", 2000)?,
                window: input
                    .get("max_output")
                    .and_then(Value::as_u64)
                    .map_or(MAX_TOOL_TEXT, |size| {
                        (size as usize).clamp(4096, MAX_READ_WINDOW)
                    }),
            }
        }
        "read_output" => StrictTool::Output {
            artifact: text("artifact")?.to_string(),
            offset: match input.get("offset") {
                None => 0,
                Some(n) => n
                    .as_u64()
                    .filter(|n| *n <= crate::artifact::MAX_OBJECT as u64)
                    .ok_or(Error::Invalid)? as usize,
            },
            limit: match input.get("limit") {
                None => MAX_TOOL_TEXT,
                Some(n) => n
                    .as_u64()
                    .filter(|n| (1..=MAX_READ_WINDOW as u64).contains(n))
                    .ok_or(Error::Invalid)? as usize,
            },
        },
        "write_file" => StrictTool::Write {
            path: path()?,
            content: text("content")?,
        },
        "start_job" => {
            let command = input
                .get("command")
                .and_then(Value::as_str)
                .ok_or(Error::Invalid)?;
            let lifetime = match input.get("timeout") {
                None => JOB_SECS,
                Some(value) => value.as_u64().ok_or(Error::Invalid)?,
            };
            if command.is_empty()
                || command.len() > 16 * 1024
                || command.contains('\0')
                || !(1..=dext_native::limits::MAX_JOB_SECS).contains(&lifetime)
            {
                return Err(Error::Invalid);
            }
            dext_native::tool_policy::apply_bash_guardrails(command, false)
                .map_err(|_| Error::Invalid)?;
            StrictTool::JobStart { command, lifetime }
        }
        "read_job" => StrictTool::JobRead {
            job: match input.get("job") {
                None => None,
                Some(_) => Some(job_name(input)?),
            },
            offset: match input.get("offset") {
                None => None,
                Some(value) => Some(value.as_u64().ok_or(Error::Invalid)?),
            },
            limit: match input.get("limit") {
                None => MAX_TOOL_TEXT,
                Some(value) => value
                    .as_u64()
                    .filter(|limit| (1..=MAX_JOB_WINDOW as u64).contains(limit))
                    .ok_or(Error::Invalid)? as usize,
            },
            wait: match input.get("wait") {
                None => 0,
                Some(value) => value
                    .as_u64()
                    .filter(|wait| *wait <= MAX_JOB_WAIT)
                    .ok_or(Error::Invalid)?,
            },
        },
        "stop_job" => StrictTool::JobStop {
            job: job_name(input)?,
        },
        "show" | "settings" | "spindle_source" | "spindle_install" => {
            host_input(&call.name, input)?;
            StrictTool::Host { name: &call.name }
        }
        _ => {
            let old = text("old_string")?;
            if old.is_empty() {
                return Err(Error::Invalid);
            }
            StrictTool::Edit {
                path: path()?,
                old,
                new: text("new_string")?,
                all: match input.get("replace_all") {
                    None => false,
                    Some(value) => value.as_bool().ok_or(Error::Invalid)?,
                },
            }
        }
    })
}

/// Longest `title` of a `show` call, `summary` of a `spindle_install` and settings
/// text value.
#[cfg(feature = "workspace-tools")]
pub const MAX_HOST_TEXT: usize = 2000;
/// Paths one `show` call displays.
#[cfg(feature = "workspace-tools")]
pub const MAX_SHOWN: usize = 8;

/// The shape of a host tool's input; what the values mean (a known model, an existing
/// file) is the host's to check, so the model gets a result naming the valid choices.
#[cfg(feature = "workspace-tools")]
fn host_input(name: &str, input: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
    use serde_json::Value;
    let text = |value: &Value, empty: bool| {
        value.as_str().is_some_and(|text| {
            text.len() <= MAX_HOST_TEXT
                && (empty || !text.trim().is_empty())
                && !text.contains('\0')
        })
    };
    let ok = match name {
        "show" => {
            input
                .get("paths")
                .and_then(Value::as_array)
                .is_some_and(|paths| {
                    (1..=MAX_SHOWN).contains(&paths.len())
                        && paths.iter().all(|path| {
                            path.as_str().is_some_and(|path| {
                                workspace_path(path.trim_end_matches('/')).is_some()
                            })
                        })
                })
                && input.get("title").is_none_or(|title| text(title, false))
        }
        "settings" => input.iter().all(|(key, value)| match key.as_str() {
            "network" | "toolchain" => value.is_boolean(),
            "max_usd" => value.is_null() || value.as_f64().is_some(),
            _ => text(value, false),
        }),
        "spindle_source" => matches!(
            input.get("action").and_then(Value::as_str),
            Some("checkout" | "update")
        ),
        "spindle_install" => input
            .get("summary")
            .is_some_and(|summary| text(summary, false)),
        _ => false,
    };
    if ok { Ok(()) } else { Err(Error::Invalid) }
}

/// A job id as given: short printable text (an unknown id is a result, not a refusal).
#[cfg(feature = "workspace-tools")]
fn job_name(input: &serde_json::Map<String, serde_json::Value>) -> Result<&str> {
    input
        .get("job")
        .and_then(serde_json::Value::as_str)
        .filter(|job| !job.is_empty() && job.len() <= 64 && !job.chars().any(char::is_control))
        .ok_or(Error::Invalid)
}

/// Why the strict workspace adapter would refuse this call, or None if it would run
/// it. Hosts record a refusal with `Action::Refuse` instead of dispatching.
#[cfg(feature = "workspace-tools")]
pub fn strict_tool_refusal(name: &str, input: &serde_json::Value) -> Option<String> {
    if !crate::turn::WORKSPACE_TOOLS.contains(&name) {
        return Some(format!(
            "unknown tool `{name}`; available: {}",
            crate::turn::WORKSPACE_TOOLS.join(", ")
        ));
    }
    if let Err(issue) = dext_native::tool_policy::validate_tool_input(name, input) {
        return Some(issue);
    }
    if matches!(name, "bash" | "start_job")
        && let Some(command) = input.get("command").and_then(serde_json::Value::as_str)
        && let Err(issue) = dext_native::tool_policy::apply_bash_guardrails(command, false)
    {
        return Some(issue);
    }
    let effect = Effect::Tool {
        id: String::new(),
        call: dext_core::response::ToolCall {
            id: String::new(),
            name: name.into(),
            input: input.clone(),
        },
    };
    strict_tool(&effect).err()?;
    if let Some(issue) = strict_field_issue(name, input) {
        return Some(issue);
    }
    const PATH: &str = "a path relative to /workspace (no empty, '.' or '..' parts; names up to 255 bytes, paths up to 1024)";
    let text = MAX_FILE_TOOL_TEXT / 1024;
    Some(match name {
        "bash" => format!(
            "bash input must be {{\"command\": non-empty text up to 16 KiB, \"timeout\": optional 1-{} seconds, \"max_output\": optional bytes (4096-32768)}}",
            dext_native::limits::MAX_RUN_SECS
        ),
        "run_code" => format!(
            "run_code input must be {{\"language\": \"python\", \"javascript\" or \"typescript\", \"code\": non-empty text up to {} KiB, \"timeout\": optional 1-{} seconds, \"max_output\": optional bytes (4096-32768)}}",
            MAX_SCRIPT / 1024,
            dext_native::limits::MAX_RUN_SECS
        ),
        "read_output" => format!(
            "read_output input must have an output artifact digest, byte offset (default 0) and limit (1-{} bytes)",
            MAX_READ_WINDOW
        ),
        "read_file" => format!(
            "read_file input must be {{\"path\": {PATH}, \"offset\"/\"limit\": optional positive line counts, \"max_output\": optional window bytes (4096-{})}}",
            MAX_READ_WINDOW
        ),
        "write_file" => format!(
            "write_file input must be {{\"path\": {PATH}, \"content\": text up to {text} KiB}}; write larger files in parts with bash"
        ),
        "start_job" => format!(
            "start_job input must be {{\"command\": non-empty text up to 16 KiB, \"timeout\": optional lifetime of 1-{} seconds}}",
            dext_native::limits::MAX_JOB_SECS
        ),
        "read_job" => format!(
            "read_job input must be {{\"job\": optional job id, \"offset\": optional output byte offset, \"limit\": optional bytes (1-{MAX_JOB_WINDOW}), \"wait\": optional seconds (0-{MAX_JOB_WAIT})}}"
        ),
        "stop_job" => "stop_job input must be {\"job\": a job id from start_job}".into(),
        "show" => format!(
            "show input must be {{\"paths\": 1-{MAX_SHOWN} workspace paths ({PATH}), \"title\": optional text up to {MAX_HOST_TEXT} bytes}}"
        ),
        "settings" => format!(
            "settings input may have \"provider\", \"model\", \"effort\", \"compaction\" and \"name\" (non-empty text up to {MAX_HOST_TEXT} bytes), \"network\" and \"toolchain\" (true or false) and \"max_usd\" (dollars, or null for no budget); none reads the current settings"
        ),
        "spindle_source" => {
            "spindle_source input must be {\"action\": \"checkout\" or \"update\"}".into()
        }
        "spindle_install" => format!(
            "spindle_install input must be {{\"summary\": what the change does, up to {MAX_HOST_TEXT} bytes}}"
        ),
        _ => format!(
            "edit_file input must be {{\"path\": {PATH}, \"old_string\": non-empty text, \"new_string\": text (each up to {text} KiB), \"replace_all\": optional boolean}}"
        ),
    })
}

#[cfg(feature = "workspace-tools")]
fn strict_fields(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "bash" => &["command", "timeout", "max_output"],
        "run_code" => &["language", "code", "timeout", "max_output"],
        "read_file" => &["path", "offset", "limit", "max_output"],
        "read_output" => &["artifact", "offset", "limit"],
        "write_file" => &["path", "content"],
        "edit_file" => &["path", "old_string", "new_string", "replace_all"],
        "start_job" => &["command", "timeout"],
        "read_job" => &["job", "offset", "limit", "wait"],
        "stop_job" => &["job"],
        "show" => &["paths", "title"],
        "settings" => &[
            "provider",
            "model",
            "effort",
            "network",
            "toolchain",
            "compaction",
            "max_usd",
            "name",
        ],
        "spindle_source" => &["action"],
        "spindle_install" => &["summary"],
        _ => return None,
    })
}

/// The first offending field of a refused call, so a model can fix exactly that.
#[cfg(feature = "workspace-tools")]
fn strict_field_issue(name: &str, input: &serde_json::Value) -> Option<String> {
    let object = input.as_object()?;
    let allowed = strict_fields(name)?;
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Some(format!(
            "{name}: unknown field `{key}`; allowed fields: {}",
            allowed.join(", ")
        ));
    }
    if object
        .get("max_output")
        .is_some_and(|v| v.as_u64().is_none())
    {
        let most = if name == "read_file" {
            MAX_READ_WINDOW
        } else {
            32768
        };
        return Some(format!(
            "{name}: max_output must be a whole number of bytes (it is clamped to 4096-{most})"
        ));
    }
    let max = if name == "start_job" {
        dext_native::limits::MAX_JOB_SECS
    } else {
        dext_native::limits::MAX_RUN_SECS
    };
    if object
        .get("timeout")
        .is_some_and(|v| !v.as_u64().is_some_and(|n| (1..=max).contains(&n)))
    {
        return Some(format!("{name}: timeout must be 1-{max} seconds"));
    }
    if name == "run_code" {
        if !matches!(
            object.get("language").and_then(serde_json::Value::as_str),
            Some("python" | "javascript" | "typescript")
        ) {
            return Some(
                "run_code: language must be \"python\", \"javascript\" or \"typescript\"".into(),
            );
        }
        let code = object.get("code").and_then(serde_json::Value::as_str);
        if code.is_none_or(|code| code.trim().is_empty() || code.contains('\0')) {
            return Some("run_code: code must be non-empty text without NUL bytes".into());
        }
        if code.is_some_and(|code| code.len() > MAX_SCRIPT) {
            return Some(format!(
                "run_code: code exceeds {} KiB; put helpers in workspace files and import them",
                MAX_SCRIPT / 1024
            ));
        }
    }
    None
}

/// Line-numbered window of `text`, bounded below `window` bytes at a line boundary.
#[cfg(feature = "workspace-tools")]
fn numbered_lines(
    path: &str,
    text: &str,
    offset: usize,
    limit: usize,
    window: usize,
) -> std::result::Result<String, String> {
    let total = text.lines().count();
    if total == 0 {
        return Ok(format!("({path} is empty)"));
    }
    if offset > total {
        return Err(format!(
            "offset {offset} is past the end of {path} ({total} lines)"
        ));
    }
    let mut out = String::new();
    let mut next = offset;
    for (index, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        let row = format!("{:>6}\t{line}\n", index + 1);
        if out.len() + row.len() > window - 256 {
            break;
        }
        out.push_str(&row);
        next = index + 2;
    }
    if next == offset {
        let line: String = text
            .lines()
            .nth(offset - 1)
            .unwrap_or_default()
            .chars()
            .take(window / 4)
            .collect();
        let mut out = format!("{offset:>6}\t{line} [line truncated]");
        if offset < total {
            out.push_str(&format!(
                "\n[{} more lines; pass offset={} to continue]",
                total - offset,
                offset + 1
            ));
        }
        return Ok(out);
    }
    if next <= total {
        out.push_str(&format!(
            "[{} more lines; pass offset={next} to continue]",
            total - next + 1
        ));
    }
    Ok(out)
}

#[cfg(feature = "driver")]
struct AppReceipt<'a> {
    patch: Option<&'a [u8]>,
    details: Option<&'a [u8]>,
    max_output: usize,
}

/// A strict tool result to record: its text (bounded to the turn's budget, with the
/// rest retained), outcome, workspace root, and a closing line that is always kept.
#[cfg(feature = "driver")]
struct Recorded {
    content: String,
    success: bool,
    root: String,
    trailer: String,
}

/// Why a finished run's changes were not kept when the store filled up meanwhile.
#[cfg(feature = "driver")]
const STORE_FULL_SEAL: &str = "the session store is full";

/// The implementation label of an app tool's attempts: its definition's digest.
pub fn app_label(helper: &str) -> String {
    format!("spindle-app-tool-v1-{helper}")
}

#[cfg(feature = "driver")]
fn app_room() -> crate::artifact::Room {
    crate::artifact::Room {
        stages: 1,
        pins: crate::workspace::MAX_FILES as i64 + 2,
        objects: crate::workspace::MAX_FILES as i64 + 2,
        bytes: (crate::workspace::MAX_TREE + crate::artifact::MAX_OBJECT + 2 * 8 * 1024 * 1024)
            as i64,
    }
}

/// Store room a strict tool call may need: read-only tools none; file tools a file and
/// a manifest; runs a whole workspace, its manifest, retained output and (scripts)
/// child journals.
#[cfg(feature = "workspace-tools")]
fn strict_room(tool: &str) -> crate::artifact::Room {
    use crate::artifact::{MAX_OBJECT, Room};
    use dext_native::limits::{MAX_FILES, MAX_OUTPUT, MAX_TREE};
    let run = Room {
        stages: 1,
        pins: MAX_FILES as i64 + 2,
        objects: MAX_FILES as i64 + 2,
        bytes: (MAX_TREE + MAX_OBJECT + 2 * MAX_OUTPUT) as i64,
    };
    let calls = crate::artifact::MAX_CALLS as i64;
    match tool {
        "read_file" | "read_output" | "start_job" | "read_job" | "stop_job" | "show"
        | "settings" | "spindle_install" => Room::default(),
        "write_file" | "edit_file" => Room {
            stages: 1,
            pins: 2,
            objects: 2,
            bytes: 2 * MAX_OBJECT as i64,
        },
        "run_code" => Room {
            stages: run.stages + calls,
            pins: run.pins + 2 * calls,
            objects: run.objects + 2 * calls,
            bytes: run.bytes + MAX_OBJECT as i64,
        },
        _ => run,
    }
}

/// A strict run's view of the store: input files come straight from their objects
/// (the root was validated for the scope), brokered requests go to the code host.
#[cfg(feature = "dext-strict-workspace")]
struct StoreRun<'a, 'h> {
    store: &'a mut Store,
    parent: &'a DispatchRequest,
    objects: &'a std::collections::BTreeMap<String, String>,
    code: Option<&'a mut (dyn crate::code_mode::Host + 'h)>,
    /// A store failure while loading input; the run never started.
    failed: Option<Error>,
}

#[cfg(feature = "dext-strict-workspace")]
impl dext_native::strict::RunHost for StoreRun<'_, '_> {
    fn load(&mut self, path: &str, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        let bytes = (|| -> Result<Vec<u8>> {
            let digest = self.objects.get(path).ok_or(Error::Invalid)?;
            let bytes = self
                .store
                .db
                .prepare_cached("SELECT bytes FROM objects WHERE digest=?1")?
                .query_row([digest], |row| {
                    crate::bounded_blob(row, 0, crate::artifact::MAX_OBJECT)
                })?;
            if crate::artifact::object_digest(&bytes) != *digest {
                return Err(Error::Incompatible);
            }
            Ok(bytes)
        })();
        match bytes {
            Ok(bytes) => out.write_all(&bytes),
            Err(error) => {
                self.failed = Some(error);
                Err(std::io::Error::other("workspace input could not be read"))
            }
        }
    }

    fn request(
        &mut self,
        request: &[u8],
        remaining: std::time::Duration,
        cancel: &AtomicBool,
    ) -> Vec<u8> {
        match self.code.as_deref_mut() {
            Some(host) => self
                .store
                .code_request(self.parent, request, host, cancel, remaining),
            None => {
                let mut refused =
                    serde_json::json!({"ok":false,"error":"no external code policy granted"});
                if let Some(seq) = serde_json::from_slice::<serde_json::Value>(request)
                    .ok()
                    .and_then(|request| request.get("seq").cloned())
                {
                    refused["seq"] = seq;
                }
                serde_json::to_vec(&refused).unwrap_or_default()
            }
        }
    }
}

#[cfg(feature = "driver")]
const INTERRUPTED_STRICT: &str = "The tool call did not complete: the runner stopped before \
its result was saved. Nothing it changed in the workspace was kept; the workspace is as it was \
before. External model/tool calls may already have run or been billed. Inspect code-receipts; they are never rolled back or automatically retried.";

/// Ends the recorded text of a run killed by its own deadline or size bounds.
#[cfg(feature = "workspace-tools")]
pub const KILLED_RUN: &str = "the command was killed and its changes discarded";
/// Ends the recorded text of a run that finished but whose workspace changes could not
/// be kept (or that could not start); the line before it says why.
#[cfg(feature = "workspace-tools")]
pub const UNSEALED_RUN: &str = "none of its workspace changes were kept";
/// Recorded when the session store cannot hold another tool step; nothing ran.
#[cfg(feature = "driver")]
pub const STORE_FULL: &str = "The session store is full, so this tool call was not run: the files and outputs it keeps for every revision already fill its limits (see `spindle doctor`). Nothing changed. Reply to the user, or work in a new thread.";
/// A failed run_code script is all-or-nothing: none of its file changes are kept.
pub const DISCARDED_SCRIPT: &str = "the script failed, so none of its workspace changes were kept";

/// Tool text retained in the transcript. The whole continuation (history plus the
/// next frozen request, which repeats it) must fit one MAX_PAYLOAD revision.
#[cfg(feature = "driver")]
pub const MAX_TOOL_TEXT: usize = 8 * 1024;

#[cfg(feature = "driver")]
fn bounded_tool_text(text: String, cap: usize) -> String {
    if text.len() <= cap {
        return text;
    }
    let keep = cap.saturating_sub(256) / 2;
    let mut head = keep;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - keep;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n[... {} bytes omitted ...]\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

/// A result's text within `cap`: the output (cut in the middle when needed), then the
/// retained-output pointer and the closing line, which always survive whatever budget
/// is left.
#[cfg(feature = "driver")]
fn fit_result(content: String, artifact: Option<&str>, trailer: &str, cap: usize) -> String {
    let pointer = artifact
        .map(|digest| format!("\n[retained output: read_output artifact={digest} offset=0]"))
        .unwrap_or_default();
    let room = cap.saturating_sub(pointer.len() + trailer.len());
    let mut content = if content.len() <= room {
        content
    } else if room < 256 {
        if artifact.is_some() {
            "[turn output budget used up; page the retained output with read_output]".into()
        } else {
            "[turn output budget used up; the output was not kept]".into()
        }
    } else {
        bounded_tool_text(content, room)
    };
    content.push_str(&pointer);
    content.push_str(trailer);
    content
}

#[cfg(feature = "dext-native-image")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImagePhase {
    BeforeSource,
    BeforeWorker,
    Running,
    Joined,
    Sealed,
}

#[cfg(feature = "dext-native-image")]
/// Trusted host control for this request's scope, polled on the authority thread.
/// Callbacks must be bounded and must not wait on the worker. A saved label is not
/// authorization; the host authenticates each requested cancellation command.
pub trait ImageControl {
    fn cancellation(&mut self, phase: ImagePhase) -> Result<Option<String>>;

    /// Durable intent acknowledgement, not proof of termination. Runs before the
    /// stop flag is set; an error/panic instead initiates containment on unwind.
    fn committed(&mut self, _command: &str, _epoch: u64) -> Result<()> {
        Ok(())
    }

    /// Durable root-only cleanup observation, never effect resolution or proof
    /// that descendants/external effects stopped. Not invoked for Unknown.
    fn cleanup_committed(&mut self, _cleanup: crate::completion::ImageCleanup) -> Result<()> {
        Ok(())
    }
}

#[cfg(feature = "dext-native-image")]
struct StopImageOnDrop<'a>(&'a AtomicBool);
#[cfg(feature = "dext-native-image")]
impl Drop for StopImageOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Store {
    #[cfg(feature = "dext-provider")]
    /// Sends one exact retained model request after durable reservation. Host policy
    /// authenticates execution, retention and historical disclosure; live credentials
    /// are resolved only for fresh dispatch and are never saved. Dropping this future
    /// or any transport/storage error leaves uncertainty, not permission to resend.
    pub async fn dispatch_turn_model<A, R>(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        transport: &dext_native::model_effect::ModelTransport,
        authorize: A,
        resolve_route: R,
    ) -> Result<DispatchOutcome>
    where
        A: FnMut(&DispatchRequest) -> Result<()>,
        R: FnOnce(&CommittedEffect) -> Result<dext_native::model_effect::LiveRoute>,
    {
        self.dispatch_turn_model_observed(
            request,
            interrupt,
            transport,
            &|_| {},
            authorize,
            resolve_route,
        )
        .await
    }

    /// The first half of `dispatch_turn_model`, for a caller that sends the request
    /// itself: checks it and durably reserves it. `Settled` is an outcome reached
    /// without sending (a reply recorded before, or a reservation to reconcile);
    /// `Reserved` is to be sent exactly once, its response handed to
    /// `ingest_turn_model`. Dropping a reservation leaves an unknown outcome, never
    /// permission to send again.
    pub fn reserve_turn_model<A>(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        mut authorize: A,
    ) -> Result<ModelReservation>
    where
        A: FnMut(&DispatchRequest) -> Result<()>,
    {
        if request.attempt.implementation != "dext-provider-v1" {
            return Err(Error::Conflict);
        }
        Ok(
            match self.prepare_dispatch(
                request,
                interrupt,
                &mut authorize,
                &ResultProfile::Model,
            )? {
                PreparedDispatch::Complete(outcome) => ModelReservation::Settled(outcome),
                PreparedDispatch::Execute(committed) => ModelReservation::Reserved(committed),
            },
        )
    }

    /// The second half: records `result` (the checkpoint of the response the reserved
    /// request got) for `request`'s attempt, checked against what was reserved.
    pub fn ingest_turn_model(
        &mut self,
        request: &DispatchRequest,
        committed: &CommittedEffect,
        result: Vec<u8>,
    ) -> Result<DispatchOutcome> {
        if committed.attempt != request.attempt {
            return Err(Error::Conflict);
        }
        crate::fits(result.len())?;
        self.ingest_receipt(&request.attempt.id, &result)?;
        let validation = validate_result(committed.effect(), &result);
        self.broker_result(validation)?;
        Ok(DispatchOutcome::Recorded(result))
    }

    #[cfg(feature = "dext-provider")]
    /// As `dispatch_turn_model`, passing live stream updates of a fresh send to
    /// `observe`. Updates are an unauthoritative preview of the recorded response.
    pub async fn dispatch_turn_model_observed<A, R>(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        transport: &dext_native::model_effect::ModelTransport,
        observe: &(dyn Fn(&dext_native::streaming::StreamUpdate) + Sync),
        authorize: A,
        resolve_route: R,
    ) -> Result<DispatchOutcome>
    where
        A: FnMut(&DispatchRequest) -> Result<()>,
        R: FnOnce(&CommittedEffect) -> Result<dext_native::model_effect::LiveRoute>,
    {
        let committed = match self.reserve_turn_model(request, interrupt, authorize)? {
            ModelReservation::Settled(outcome) => return Ok(outcome),
            ModelReservation::Reserved(committed) => committed,
        };
        let route = resolve_route(&committed)?;
        let Effect::Model { waiting, .. } = committed.effect() else {
            return Err(Error::Invalid);
        };
        let recorded = transport
            .send_once_observed(waiting, route, interrupt, observe)
            .await
            .map_err(|error| Error::Model(error.into()))?;
        self.ingest_turn_model(request, &committed, recorded.checkpoint())
    }

    #[cfg(feature = "dext-strict-workspace")]
    /// Optional strict workspace adapter (sandboxed on Linux, or a host runner). Runs only an exact committed bash or
    /// file-tool obligation against its retained immutable base. Host policy
    /// authenticates execution/retention; no grant is reconstructed from the label.
    pub fn dispatch_strict_workspace<A>(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        executor: &dext_native::strict::StrictExecutor,
        authorize: A,
    ) -> Result<DispatchOutcome>
    where
        A: FnMut(&DispatchRequest) -> Result<()>,
    {
        self.dispatch_strict_workspace_with(
            request,
            interrupt,
            executor,
            dext_native::strict::RunOptions::default(),
            authorize,
        )
    }

    #[cfg(feature = "dext-strict-workspace")]
    /// As `dispatch_strict_workspace` with per-run options for bash. `network` is a
    /// fresh host grant for this run only (never derived from saved labels); a
    /// networked run can have external effects that rewinds do not undo. Observed
    /// output is an unauthoritative preview; only the sealed receipt is adopted.
    pub fn dispatch_strict_workspace_with<A>(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        executor: &dext_native::strict::StrictExecutor,
        options: dext_native::strict::RunOptions<'_>,
        authorize: A,
    ) -> Result<DispatchOutcome>
    where
        A: FnMut(&DispatchRequest) -> Result<()>,
    {
        self.dispatch_code_workspace(request, interrupt, executor, options, authorize, None)
    }

    #[cfg(feature = "dext-strict-workspace")]
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch_code_workspace<A>(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        executor: &dext_native::strict::StrictExecutor,
        mut options: dext_native::strict::RunOptions<'_>,
        mut authorize: A,
        host: Option<&mut dyn crate::code_mode::Host>,
    ) -> Result<DispatchOutcome>
    where
        A: FnMut(&DispatchRequest) -> Result<()>,
    {
        let helper: String = executor
            .helper_digest()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if request.attempt.implementation != executor.implementation() {
            return Err(Error::Conflict);
        }
        // Room for the step is checked (collecting garbage near the caps) before the
        // reservation, so a full store refuses the call as a result instead of
        // stranding a reserved attempt; read-only tools need none.
        let room = strict_room(&request.intent.adapter);
        let fits = room == crate::artifact::Room::default() || self.make_room(room)?;
        let committed = match self.prepare_dispatch(
            request,
            interrupt,
            &mut authorize,
            &ResultProfile::StrictWorkspace,
        )? {
            PreparedDispatch::Complete(outcome) => return Ok(outcome),
            PreparedDispatch::Execute(committed) => committed,
        };
        let tool = strict_tool(committed.effect())?;
        let base = self.strict_base(request)?;
        let Effect::Tool { call, .. } = committed.effect() else {
            return Err(Error::Invalid);
        };
        let call_id = call.id.clone();
        let failed = |content: String| Recorded {
            content,
            success: false,
            root: base.clone(),
            trailer: String::new(),
        };
        if !fits {
            let recorded = failed(STORE_FULL.into());
            return self.record_strict(request, &call_id, &helper, &base, recorded);
        }
        if let StrictTool::Host { name } = tool {
            let recorded = failed(format!(
                "`{name}` is run by the host serving this session, which does not offer it here"
            ));
            return self.record_strict(request, &call_id, &helper, &base, recorded);
        }
        if let StrictTool::JobRead { .. } | StrictTool::JobStop { .. } = tool {
            let (content, success) = self.job_call(&tool, interrupt);
            let recorded = Recorded {
                content,
                success,
                root: base.clone(),
                trailer: String::new(),
            };
            return self.record_strict(request, &call_id, &helper, &base, recorded);
        }
        let tree = self.tree_map(&request.scope, &base)?;
        let stage = crate::artifact::Stage {
            id: request.attempt.id.clone(),
            scope: request.scope.clone(),
            owner_epoch: request.owner_epoch,
        };
        let prepared = match tool {
            StrictTool::Bash { command, timeout } => {
                let command = dext_native::tool_policy::apply_bash_guardrails(command, false)
                    .map_err(|_| Error::Invalid)?;
                executor
                    .program(dext_native::strict::Program::Bash)
                    .ok_or(format!(
                        "bash is not available on this host; install it (on Windows, Git for \
                         Windows) or set {}",
                        dext_native::strict::Program::Bash.variable()
                    ))
                    .map(|bash| {
                        let argv = vec![
                            bash,
                            "--noprofile".into(),
                            "--norc".into(),
                            "-c".into(),
                            command,
                        ];
                        (argv, timeout, false)
                    })
            }
            StrictTool::JobStart { command, lifetime } => {
                let (content, success) =
                    self.start_job(request, &tree, command, lifetime, executor, options)?;
                let recorded = Recorded {
                    content,
                    success,
                    root: base.clone(),
                    trailer: String::new(),
                };
                return self.record_strict(request, &call_id, &helper, &base, recorded);
            }
            StrictTool::Code {
                language,
                code,
                timeout,
            } => {
                options.local_ipc = true;
                language
                    .argv(code, executor)
                    .map(|argv| (argv, timeout, true))
            }
            file_tool => {
                let recorded = match self.apply_file_tool(&stage, &base, &tree, &file_tool)? {
                    Ok((content, root)) => Recorded {
                        content,
                        success: true,
                        root,
                        trailer: String::new(),
                    },
                    Err(message) => failed(message),
                };
                return self.record_strict(request, &call_id, &helper, &base, recorded);
            }
        };
        // A missing interpreter is an ordinary failed result over the unchanged base;
        // nothing ran.
        let (argv, timeout, atomic) = match prepared {
            Ok(prepared) => prepared,
            Err(message) => {
                return self.record_strict(request, &call_id, &helper, &base, failed(message));
            }
        };
        let mut sources = host.as_ref().map(|h| h.sources()).unwrap_or_default();
        if matches!(
            tool,
            StrictTool::Code {
                language: CodeLanguage::TypeScript,
                ..
            }
        ) {
            sources.push(typescript_source().map_err(|_| Error::Invalid)?);
        }
        let objects: std::collections::BTreeMap<String, String> = tree
            .files
            .iter()
            .map(|entry| (entry.path.clone(), entry.object.clone()))
            .collect();
        let inputs = self.run_inputs(&tree)?;
        let mut run = StoreRun {
            store: self,
            parent: request,
            objects: &objects,
            code: host,
            failed: None,
        };
        let executed = executor.execute_tree(
            &inputs,
            &argv,
            std::time::Duration::from_secs(timeout),
            interrupt,
            options,
            &sources,
            atomic,
            &mut run,
        );
        // Reading the store's own input is a store failure, not the run's.
        if let Some(error) = run.failed.take() {
            return self.broker_result(Err(error));
        }
        let output = |stdout: &[u8], stderr: &[u8]| {
            format!(
                "{}{}",
                String::from_utf8_lossy(stdout),
                String::from_utf8_lossy(stderr)
            )
        };
        // Whatever ended the run (its own bounds, unsealable output, a sandbox that
        // could not start), the executor already discarded the candidate, so the call is
        // recorded as a failed result over the unchanged base with its output and reason.
        // Only an interrupted owner leaves the attempt to be sealed as interrupted.
        let recorded = match executed {
            Ok(sealed) => {
                if sealed.helper_digest() != executor.helper_digest() {
                    return Err(Error::Conflict);
                }
                let content = output(sealed.stdout(), sealed.stderr());
                if atomic && !sealed.success() {
                    Recorded {
                        trailer: format!("\n[{DISCARDED_SCRIPT}]"),
                        ..failed(content)
                    }
                } else {
                    match self.seal_tree(&stage, &base, &tree, &sealed)? {
                        Ok(root) => Recorded {
                            content,
                            success: sealed.success(),
                            root,
                            trailer: String::new(),
                        },
                        Err(reason) => Recorded {
                            trailer: format!("\n{reason}; {UNSEALED_RUN}"),
                            ..failed(content)
                        },
                    }
                }
            }
            Err(_) if interrupt.load(Ordering::SeqCst) => return Err(Error::Cancelled),
            Err(error) => {
                let (reason, content) = match dext_native::strict::run_failure(&error) {
                    Some(failure) => (
                        failure.reason.clone(),
                        output(&failure.stdout, &failure.stderr),
                    ),
                    None => (
                        format!("the run could not be completed: {error}"),
                        String::new(),
                    ),
                };
                let ending = if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::FileTooLarge
                        | std::io::ErrorKind::OutOfMemory
                ) {
                    KILLED_RUN
                } else {
                    UNSEALED_RUN
                };
                let separator = if content.is_empty() { "" } else { "\n" };
                Recorded {
                    trailer: format!("{separator}{reason}; {ending}"),
                    ..failed(content)
                }
            }
        };
        self.record_strict(request, &call_id, &helper, &base, recorded)
    }

    /// A host tool (`crate::turn::HOST_TOOLS`) under the strict workspace adapter's
    /// reservation and receipts: `run` gets the call over the workspace it was
    /// dispatched on and says what it did; a result that replaces a subtree is staged
    /// as the call's new root, handed to its `staged` hook before anything is recorded,
    /// and adopted like any workspace receipt. A reserved call whose owner died seals
    /// with `seal_interrupted_strict` (nothing re-runs); a recorded one is reused.
    #[cfg(feature = "dext-strict-workspace")]
    pub fn dispatch_host_workspace<A, R>(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        executor: &dext_native::strict::StrictExecutor,
        mut authorize: A,
        run: R,
    ) -> Result<DispatchOutcome>
    where
        A: FnMut(&DispatchRequest) -> Result<()>,
        R: FnOnce(&str, &serde_json::Value, &mut HostWorkspace<'_>) -> HostResult,
    {
        let helper: String = executor
            .helper_digest()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if request.attempt.implementation != executor.implementation()
            || !crate::turn::HOST_TOOLS.contains(&request.intent.adapter.as_str())
        {
            return Err(Error::Conflict);
        }
        let room = strict_room(&request.intent.adapter);
        let fits = room == crate::artifact::Room::default() || self.make_room(room)?;
        let committed = match self.prepare_dispatch(
            request,
            interrupt,
            &mut authorize,
            &ResultProfile::StrictWorkspace,
        )? {
            PreparedDispatch::Complete(outcome) => return Ok(outcome),
            PreparedDispatch::Execute(committed) => committed,
        };
        let base = self.strict_base(request)?;
        let Effect::Tool { call, .. } = committed.effect() else {
            return Err(Error::Invalid);
        };
        let failed = |content: String| Recorded {
            content,
            success: false,
            root: base.clone(),
            trailer: String::new(),
        };
        if !fits {
            return self.record_strict(
                request,
                &call.id,
                &helper,
                &base,
                failed(STORE_FULL.into()),
            );
        }
        let tree = self.tree_map(&request.scope, &base)?;
        let mut workspace = HostWorkspace {
            store: self,
            scope: &request.scope,
            root: &base,
            tree: &tree,
            revision: request.expected_revision,
        };
        let result = run(&call.name, &call.input, &mut workspace);
        let recorded = match result.replace {
            None => Recorded {
                content: result.content,
                success: result.success,
                root: base.clone(),
                trailer: String::new(),
            },
            Some(replace) => {
                let stage = crate::artifact::Stage {
                    id: request.attempt.id.clone(),
                    scope: request.scope.clone(),
                    owner_epoch: request.owner_epoch,
                };
                match self.replace_subtree(
                    &stage,
                    &tree,
                    &replace.prefix,
                    &replace.files,
                    &replace.links,
                )? {
                    Err(why) => failed(format!("{}; nothing changed", why)),
                    Ok(root) => match replace.staged.map_or(Ok(()), |staged| staged(&root)) {
                        Ok(()) => Recorded {
                            content: result.content,
                            success: result.success,
                            root,
                            trailer: String::new(),
                        },
                        Err(why) => failed(format!("{why}; nothing changed")),
                    },
                }
            }
        };
        self.record_strict(request, &call.id, &helper, &base, recorded)
    }

    /// Dispatches a call of a tool the embedding app supplies. The attempt is reserved
    /// before `ask` hands the call to the app; `again` tells it an earlier owner
    /// reserved this same call and never recorded an answer, so it may have run. `ask`
    /// returns the app's answer, or None when there is none to keep (the run stopped):
    /// the reservation then stays unresolved and the next run asks again. Files the
    /// answer writes are staged as the call's new root and adopted with its result, so
    /// rewind undoes them. `helper` is the digest of the tool's definition, named by
    /// the attempt's implementation label (`app_label`).
    #[cfg(feature = "driver")]
    pub fn dispatch_app_workspace<A>(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        helper: &[u8; 32],
        ask: A,
    ) -> Result<DispatchOutcome>
    where
        A: FnOnce(
            &dext_core::response::ToolCall<serde_json::Value>,
            bool,
            &mut HostWorkspace<'_>,
        ) -> Option<AppResult>,
    {
        let (call, again) = match self.reserve_app_call(request, interrupt, helper)? {
            AppReservation::Settled(outcome) => return Ok(outcome),
            AppReservation::Reserved { call, again } => (call, again),
        };
        let answer = self.app_workspace(request, |workspace| ask(&call, again, workspace))?;
        let Some(answer) = answer else {
            return Err(Error::Cancelled);
        };
        self.record_app_answer(request, helper, answer)
    }

    /// Reserves an app call before the host asks for its answer. An unanswered
    /// reservation is offered again with the same effect id, never silently retried.
    #[cfg(feature = "driver")]
    pub fn reserve_app_call(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        helper: &[u8; 32],
    ) -> Result<AppReservation> {
        let helper: String = helper.iter().map(|byte| format!("{byte:02x}")).collect();
        if request.attempt.implementation != app_label(&helper)
            || !crate::turn::delegated_tool(&request.intent.adapter)
        {
            return Err(Error::Conflict);
        }
        let again = match self.prepare_dispatch(
            request,
            interrupt,
            &mut |_: &DispatchRequest| Ok(()),
            &ResultProfile::App,
        )? {
            PreparedDispatch::Execute(_) => false,
            // Reserved and never answered: by an earlier owner, or by this one before a
            // stop (an embedder that keeps its `Session` continues it in the same epoch).
            PreparedDispatch::Complete(DispatchOutcome::Reconcile) => true,
            PreparedDispatch::Complete(outcome) => return Ok(AppReservation::Settled(outcome)),
        };
        let effect = reserved_effect(&self.db, self.epoch, request)?;
        let Effect::Tool { call, .. } = effect else {
            return Err(Error::Invalid);
        };
        Ok(AppReservation::Reserved { call, again })
    }

    /// Reads the reserved call's immutable workspace while the host prepares its
    /// answer (tasks copy these files into their own session).
    #[cfg(feature = "driver")]
    pub fn app_workspace<T>(
        &mut self,
        request: &DispatchRequest,
        ask: impl FnOnce(&mut HostWorkspace<'_>) -> T,
    ) -> Result<T> {
        if self.paused {
            return Err(Error::Paused);
        }
        validate_request(request)?;
        reserved_effect(&self.db, self.epoch, request)?;
        let base = self.strict_base(request)?;
        let tree = self.tree_map(&request.scope, &base)?;
        let mut workspace = HostWorkspace {
            store: self,
            scope: &request.scope,
            root: &base,
            tree: &tree,
            revision: request.expected_revision,
        };
        Ok(ask(&mut workspace))
    }

    /// Records the first valid answer of a still-pending app reservation. This is
    /// receipt ingress only; adoption commits files and app state with the transcript.
    #[cfg(feature = "driver")]
    pub fn record_app_answer(
        &mut self,
        request: &DispatchRequest,
        helper: &[u8; 32],
        answer: AppResult,
    ) -> Result<DispatchOutcome> {
        if self.paused {
            return Err(Error::Paused);
        }
        validate_request(request)?;
        let helper: String = helper.iter().map(|byte| format!("{byte:02x}")).collect();
        if request.attempt.implementation != app_label(&helper)
            || !crate::turn::delegated_tool(&request.intent.adapter)
        {
            return Err(Error::Conflict);
        }
        let Effect::Tool { call, .. } = reserved_effect(&self.db, self.epoch, request)? else {
            return Err(Error::Invalid);
        };
        let base = self.strict_base(request)?;
        let AppResult {
            mut content,
            success,
            writes,
            deletes,
            links,
            directories,
            state,
            details,
            max_output,
        } = answer;
        if !(1..=1024 * 1024).contains(&max_output)
            || details.as_ref().is_some_and(|bytes| {
                bytes.len() > crate::transition::MAX_STATE
                    || serde_json::from_slice::<serde_json::Value>(bytes).is_err()
            })
        {
            return Err(Error::Invalid);
        }
        // A state change the adopting step could not set is not kept, with a note: the
        // store would otherwise refuse that step every time it is tried.
        let state = match state {
            Some(patch) => {
                let prior = crate::transition::app_state_at(
                    &self.db,
                    &request.scope,
                    request.expected_revision,
                )?;
                let kept = match patch.len() <= crate::transition::MAX_STATE {
                    true => crate::transition::apply_state_patch(&prior, &patch),
                    false => Err(Error::Exhausted),
                };
                match kept {
                    Ok(_) => Some(patch),
                    Err(error) => {
                        let why = match error {
                            Error::Exhausted => format!(
                                "the app state would be over {} KiB",
                                crate::transition::MAX_STATE / 1024
                            ),
                            _ => "it is not a JSON merge patch (an object)".into(),
                        };
                        content.push_str(&format!(
                            "\n[spindle: the app's state change was not kept: {why}]"
                        ));
                        None
                    }
                }
            }
            None => None,
        };
        let mut root = base.clone();
        let changes = Changes {
            writes,
            deletes,
            links,
            directories,
        };
        if !changes.is_empty() {
            let room = app_room();
            if !self.make_room(room)? {
                content.push_str(&format!(
                    "\n[spindle: {STORE_FULL}; the file changes were not kept]"
                ));
            } else {
                match self.stage_answer(request, &base, changes)? {
                    Ok(changed) => root = changed,
                    Err(why) => content.push_str(&format!(
                        "\n[spindle: the file changes were not kept: {why}]"
                    )),
                }
            }
        }
        let recorded = Recorded {
            content,
            success,
            root,
            trailer: String::new(),
        };
        self.adopt_stage(&crate::artifact::Stage {
            id: request.attempt.id.clone(),
            scope: request.scope.clone(),
            owner_epoch: self.epoch,
        })?;
        self.record_sealed(
            request,
            &call.id,
            &helper,
            &base,
            recorded,
            Some(AppReceipt {
                patch: state.as_deref(),
                details: details.as_deref(),
                max_output,
            }),
        )
    }

    pub fn app_details(&self, attempt: &str) -> Result<Option<Vec<u8>>> {
        if self.paused {
            return Err(Error::Paused);
        }
        self.db
            .query_row(
                "SELECT value FROM app_details WHERE attempt=?1",
                [attempt],
                |row| crate::packed_blob(&self.db, row, 0, crate::transition::MAX_STATE),
            )
            .optional()
            .map_err(Error::from)
    }

    /// Effect ids of the app tool calls answered in `scope` after revision `after` that
    /// no earlier rewind cut: the calls a rewind to `after` takes back. `task` calls
    /// share the label but a child session answered them, not the app.
    pub fn answered_app_calls(&self, scope: &str, after: u64) -> Result<Vec<String>> {
        if self.paused {
            return Err(Error::Paused);
        }
        let mut query = self.db.prepare(
            "SELECT i.id FROM attempts a JOIN intents i ON i.id=a.effect JOIN revisions r ON r.revision=i.revision WHERE r.scope=?1 AND i.revision>?2 AND substr(a.implementation,1,20)='spindle-app-tool-v1-' AND i.adapter!='task' AND EXISTS(SELECT 1 FROM receipts c WHERE c.attempt=a.id) ORDER BY i.revision,i.position",
        )?;
        let ids = query
            .query_map(params![scope, after.min(i64::MAX as u64) as i64], |r| {
                crate::bounded_label(r, 0)
            })?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        let mut live = vec![];
        for id in ids {
            if !crate::rewind::superseded(&self.db, &id)? {
                live.push(id);
            }
        }
        Ok(live)
    }

    /// Stages `changes` over the reserved call's `base` as the call's new root, or
    /// says why they cannot be kept. Written paths replace what was there; a deleted
    /// directory's path removes everything under it.
    #[cfg(feature = "driver")]
    fn stage_answer(
        &mut self,
        request: &DispatchRequest,
        base: &str,
        changes: Changes,
    ) -> Result<std::result::Result<String, String>> {
        let Changes {
            writes,
            deletes,
            links,
            directories,
        } = changes;
        let tree = self.tree_map(&request.scope, base)?;
        let stage = crate::artifact::Stage {
            id: request.attempt.id.clone(),
            scope: request.scope.clone(),
            owner_epoch: self.epoch,
        };
        self.adopt_stage(&stage)?;
        let written: std::collections::BTreeSet<String> = writes
            .iter()
            .map(|(path, _, _)| path.clone())
            .chain(links.iter().map(|link| link.path.clone()))
            .chain(directories.iter().cloned())
            .collect();
        let drop = |path: &str| {
            written.contains(path)
                || deletes.iter().any(|gone| {
                    path == gone
                        || path
                            .strip_prefix(gone.as_str())
                            .is_some_and(|rest| rest.starts_with('/'))
                })
        };
        let files: Vec<_> = writes
            .iter()
            .map(|(path, bytes, executable)| (path.clone(), bytes.as_slice(), *executable))
            .collect();
        let mut before = tree;
        before.links.retain(|link| !drop(&link.path));
        before.directories.retain(|path| !drop(path));
        before.directories.extend(directories);
        let links: Vec<_> = links
            .into_iter()
            .map(|link| (link.path, link.target))
            .collect();
        self.stage_changes(&stage, &before, &drop, &files, &links)
    }

    /// Takes over the stage an earlier owner of a reservation began (its epoch has
    /// ended, so nothing else writes to it); a stage of the current owner is left as is.
    #[cfg(feature = "driver")]
    fn adopt_stage(&mut self, stage: &crate::artifact::Stage) -> Result<()> {
        let adopted = self
            .db
            .execute(
                "UPDATE artifact_stages SET owner_epoch=?1 WHERE id=?2 AND scope=?3 AND owner_epoch<?1",
                params![stage.owner_epoch as i64, stage.id, stage.scope],
            )
            .map(|_| ())
            .map_err(Error::from);
        self.broker_result(adopted)
    }

    /// Stages `before` with every file under `prefix` ("" = all) replaced by `files`
    /// (paths relative to the prefix); links and empty directories stay unless a file
    /// takes their place. The inner `Err` says why the tree cannot be kept.
    #[cfg(feature = "dext-strict-workspace")]
    fn replace_subtree(
        &mut self,
        stage: &crate::artifact::Stage,
        before: &crate::workspace::Tree,
        prefix: &str,
        files: &[(String, Vec<u8>, bool)],
        links: &[(String, String)],
    ) -> Result<std::result::Result<String, String>> {
        let files: Vec<_> = files
            .iter()
            .map(|(path, bytes, executable)| {
                (format!("{prefix}{path}"), bytes.as_slice(), *executable)
            })
            .collect();
        self.stage_changes(
            stage,
            before,
            &|path| path.starts_with(prefix),
            &files,
            links,
        )
    }

    /// Stages `before` without the files `drop` names, then with `files` (workspace
    /// paths) and `links` (where nothing is yet) added; the new root, or why not.
    #[cfg(feature = "driver")]
    fn stage_changes(
        &mut self,
        stage: &crate::artifact::Stage,
        before: &crate::workspace::Tree,
        drop: &dyn Fn(&str) -> bool,
        files: &[(String, &[u8], bool)],
        links: &[(String, String)],
    ) -> Result<std::result::Result<String, String>> {
        let mut next = before.clone();
        next.files.retain(|entry| !drop(&entry.path));
        for (path, target) in links {
            let inside = format!("{path}/");
            let taken = next
                .files
                .iter()
                .any(|f| f.path == *path || f.path.starts_with(&inside))
                || next.links.iter().any(|l| l.path == *path)
                || next.directories.iter().any(|d| d == path);
            if !taken && crate::workspace::valid_path(path) {
                next.links.push(crate::workspace::Link {
                    path: path.clone(),
                    target: target.clone(),
                });
            }
        }
        let mut total = 0u64;
        let mut added = Vec::with_capacity(files.len());
        for (path, bytes, executable) in files {
            let path = path.clone();
            if !crate::workspace::valid_path(&path) {
                return Ok(Err(format!("`{path}` is not a valid workspace path")));
            }
            if bytes.len() > crate::workspace::MAX_FILE {
                return Ok(Err(format!(
                    "{path} exceeds the {} file limit",
                    crate::human_size(crate::workspace::MAX_FILE)
                )));
            }
            total = total.saturating_add(bytes.len() as u64);
            let inside = |directory: &str| path.starts_with(&format!("{directory}/"));
            next.links.retain(|link| link.path != path);
            next.directories
                .retain(|directory| *directory != path && !inside(directory));
            next.files.push(crate::workspace::Entry {
                path: path.clone(),
                object: crate::artifact::object_digest(bytes),
                executable: *executable,
            });
            added.push(*bytes);
        }
        next.files.sort_by(|a, b| a.path.cmp(&b.path));
        if next
            .files
            .windows(2)
            .any(|pair| pair[0].path == pair[1].path)
            || crate::workspace::validate_tree(&next).is_err()
        {
            return Ok(Err(format!(
                "the files do not fit the workspace (at most {} entries; a path cannot be both a file and a directory)",
                crate::workspace::MAX_FILES
            )));
        }
        let kept = (|| -> Result<u64> {
            let mut size = self
                .db
                .prepare_cached("SELECT length(bytes) FROM objects WHERE digest=?1")?;
            let mut total = 0u64;
            for entry in before.files.iter().filter(|e| !drop(&e.path)) {
                let length: i64 = size.query_row([&entry.object], |row| row.get(0))?;
                total = total.saturating_add(length.max(0) as u64);
            }
            Ok(total)
        })();
        if self.broker_result(kept)?.saturating_add(total) > crate::workspace::MAX_TREE as u64 {
            return Ok(Err(format!(
                "the workspace would exceed {}",
                crate::human_size(crate::workspace::MAX_TREE)
            )));
        }
        self.begin_stage(stage)?;
        for bytes in added {
            match self.put_workspace_object(stage, bytes) {
                Ok(_) => {}
                Err(Error::Exhausted) => return Ok(Err(STORE_FULL_SEAL.into())),
                Err(error) => return Err(error),
            }
        }
        match crate::workspace::encode_tree(&next)
            .and_then(|manifest| self.put_workspace_object(stage, &manifest))
        {
            Ok(root) => Ok(Ok(root)),
            Err(Error::Exhausted) => Ok(Err(STORE_FULL_SEAL.into())),
            Err(error) => Err(error),
        }
    }

    /// The workspace root a strict call runs over: the scope head's. Calls of one batch
    /// run in order, each on the tree the call before it left; adoption checks the
    /// sealed base against the root just before it.
    #[cfg(feature = "driver")]
    fn strict_base(&self, request: &DispatchRequest) -> Result<String> {
        let selected: Option<String> = self.db.query_row(
            "SELECT workspace FROM revisions WHERE revision=?1",
            [crate::transition::scope_revision(&self.db, &request.scope)? as i64],
            |row| crate::optional_digest(row, 0),
        )?;
        selected.ok_or(Error::Invalid)
    }

    /// The run's input tree with each file's stored size.
    #[cfg(feature = "dext-strict-workspace")]
    fn run_inputs(
        &mut self,
        tree: &crate::workspace::Tree,
    ) -> Result<std::collections::BTreeMap<String, dext_native::strict::Input>> {
        use dext_native::strict::Input;
        let mut inputs = std::collections::BTreeMap::new();
        let sizes = (|| -> Result<()> {
            let mut size = self
                .db
                .prepare_cached("SELECT length(bytes) FROM objects WHERE digest=?1")?;
            for entry in &tree.files {
                let length: i64 = size.query_row([&entry.object], |row| row.get(0))?;
                inputs.insert(
                    entry.path.clone(),
                    Input::File {
                        size: u64::try_from(length).map_err(|_| Error::Incompatible)?,
                        executable: entry.executable,
                    },
                );
            }
            Ok(())
        })();
        self.broker_result(sizes)?;
        for link in &tree.links {
            inputs.insert(link.path.clone(), Input::Link(link.target.clone()));
        }
        for directory in &tree.directories {
            inputs.insert(directory.clone(), Input::Directory);
        }
        Ok(inputs)
    }

    /// Stores a sealed run's tree: unchanged files keep their base objects without being
    /// read, changed ones are stored in one transaction. The inner `Err` says why the
    /// tree cannot be kept (then nothing is).
    #[cfg(feature = "dext-strict-workspace")]
    fn seal_tree(
        &mut self,
        stage: &crate::artifact::Stage,
        base: &str,
        before: &crate::workspace::Tree,
        sealed: &dext_native::strict::SealedOutput,
    ) -> Result<std::result::Result<String, String>> {
        use dext_native::strict::SealedEntry;
        let objects: std::collections::BTreeMap<&str, &str> = before
            .files
            .iter()
            .map(|entry| (entry.path.as_str(), entry.object.as_str()))
            .collect();
        let changed: Vec<String> = sealed
            .entries()
            .iter()
            .filter(|(_, entry)| matches!(entry, SealedEntry::File { .. }))
            .map(|(path, _)| path.clone())
            .collect();
        let mut stored = std::collections::BTreeMap::new();
        if !changed.is_empty() {
            self.begin_stage(stage)?;
            let digests =
                match self.put_workspace_files(stage, &changed, &mut |path| sealed.read(path)) {
                    Ok(Ok(digests)) => digests,
                    Ok(Err(reason)) => return Ok(Err(reason)),
                    Err(Error::Exhausted) => return Ok(Err(STORE_FULL_SEAL.into())),
                    Err(error) => return Err(error),
                };
            stored.extend(changed.iter().cloned().zip(digests));
        }
        let mut tree = crate::workspace::Tree::default();
        for (path, entry) in sealed.entries() {
            match entry {
                SealedEntry::Unchanged { executable } => tree.files.push(crate::workspace::Entry {
                    path: path.clone(),
                    object: objects
                        .get(path.as_str())
                        .ok_or(Error::Invalid)?
                        .to_string(),
                    executable: *executable,
                }),
                SealedEntry::File { executable, .. } => tree.files.push(crate::workspace::Entry {
                    path: path.clone(),
                    object: stored.remove(path).ok_or(Error::Invalid)?,
                    executable: *executable,
                }),
                SealedEntry::Link(target) => tree.links.push(crate::workspace::Link {
                    path: path.clone(),
                    target: target.clone(),
                }),
                SealedEntry::Directory => tree.directories.push(path.clone()),
            }
        }
        let Ok(manifest) = crate::workspace::encode_tree(&tree) else {
            return Ok(Err(
                "the workspace's file list is larger than the store can hold in one manifest"
                    .into(),
            ));
        };
        let root = crate::artifact::object_digest(&manifest);
        if root == base {
            return Ok(Ok(root));
        }
        self.begin_stage(stage)?;
        match self.put_workspace_object(stage, &manifest) {
            Ok(root) => Ok(Ok(root)),
            Err(Error::Exhausted) => Ok(Err(STORE_FULL_SEAL.into())),
            Err(error) => Err(error),
        }
    }

    /// File tools over the immutable base, computed in-process (no sandbox needed:
    /// nothing runs). Only the file a tool names is read. A failed edit is an error
    /// result (`Err`) over the unchanged base; reads stage nothing.
    #[cfg(feature = "workspace-tools")]
    fn apply_file_tool(
        &mut self,
        stage: &crate::artifact::Stage,
        base: &str,
        tree: &crate::workspace::Tree,
        tool: &StrictTool<'_>,
    ) -> Result<std::result::Result<(String, String), String>> {
        let existing = |path: &str| -> std::result::Result<&crate::workspace::Entry, String> {
            if let Some(entry) = tree.files.iter().find(|entry| entry.path == path) {
                return Ok(entry);
            }
            if tree.links.iter().any(|link| link.path == path) {
                return Err(format!(
                    "`{path}` is a symbolic link; use bash to read or change what it points to"
                ));
            }
            let lower = path.to_lowercase();
            Err(
                match tree
                    .files
                    .iter()
                    .find(|entry| entry.path.to_lowercase() == lower)
                {
                    Some(entry) => format!(
                        "no file `{path}` in the workspace (did you mean `{}`?)",
                        entry.path
                    ),
                    None => format!("no file `{path}` in the workspace"),
                },
            )
        };
        let text = |store: &mut Self,
                    path: &str,
                    object: &str|
         -> Result<std::result::Result<String, String>> {
            let bytes = store.read_object(&stage.scope, object)?;
            Ok(String::from_utf8(bytes)
                .map_err(|_| format!("`{path}` is not UTF-8 text; use bash to inspect it")))
        };
        let (path, content, executable, message) = match tool {
            StrictTool::Bash { .. }
            | StrictTool::Code { .. }
            | StrictTool::JobStart { .. }
            | StrictTool::JobRead { .. }
            | StrictTool::JobStop { .. }
            | StrictTool::Host { .. } => return Err(Error::Invalid),
            StrictTool::Output {
                artifact,
                offset,
                limit,
            } => {
                return match self.retained_output(&stage.scope, artifact, *offset, *limit) {
                    Ok(page) => Ok(Ok((page, base.to_string()))),
                    Err(Error::Invalid) => Ok(Err(
                        "no retained output artifact or invalid output window".into(),
                    )),
                    Err(error) => Err(error),
                };
            }
            StrictTool::Read {
                path,
                offset,
                limit,
                window,
            } => {
                let entry = match existing(path) {
                    Ok(entry) => entry,
                    Err(message) => return Ok(Err(message)),
                };
                let read = text(self, path, &entry.object)?
                    .and_then(|body| numbered_lines(path, &body, *offset, *limit, *window));
                return Ok(read.map(|listing| (listing, base.to_string())));
            }
            StrictTool::Write { path, content } => {
                if tree.links.iter().any(|link| link.path == *path)
                    || tree.directories.iter().any(|directory| directory == path)
                {
                    return Ok(Err(format!(
                        "`{path}` is a symbolic link or a directory; use bash to replace it"
                    )));
                }
                let old = tree.files.iter().find(|entry| entry.path == *path);
                let verb = if old.is_some() {
                    "Overwrote"
                } else {
                    "Created"
                };
                (
                    path,
                    content.to_string(),
                    old.is_some_and(|entry| entry.executable),
                    format!("{verb} {path} ({} bytes)", content.len()),
                )
            }
            StrictTool::Edit {
                path,
                old,
                new,
                all,
            } => {
                let entry = match existing(path) {
                    Ok(entry) => entry,
                    Err(message) => return Ok(Err(message)),
                };
                let body = match text(self, path, &entry.object)? {
                    Ok(body) => body,
                    Err(message) => return Ok(Err(message)),
                };
                let count = body.matches(old).count();
                if count == 0 {
                    return Ok(Err(format!(
                        "old_string was not found in {path}; read the file and copy the text exactly"
                    )));
                }
                if count > 1 && !all {
                    return Ok(Err(format!(
                        "old_string occurs {count} times in {path}; add surrounding context or set replace_all"
                    )));
                }
                // The result's size is known before it is built.
                let replaced = if *all { count } else { 1 };
                let size = (body.len() - replaced * old.len())
                    .saturating_add(replaced.saturating_mul(new.len()));
                if size > dext_native::limits::MAX_FILE {
                    return Ok(Err(format!(
                        "{path} would exceed the {} file limit",
                        crate::human_size(crate::workspace::MAX_FILE)
                    )));
                }
                let replaced = if *all {
                    body.replace(old, new)
                } else {
                    body.replacen(old, new, 1)
                };
                (
                    path,
                    replaced,
                    entry.executable,
                    format!(
                        "Edited {path} ({count} replacement{})",
                        if count == 1 { "" } else { "s" }
                    ),
                )
            }
        };
        if content.len() > dext_native::limits::MAX_FILE {
            return Ok(Err(format!(
                "{path} would exceed the {} file limit",
                crate::human_size(crate::workspace::MAX_FILE)
            )));
        }
        let mut next = tree.clone();
        next.files.retain(|entry| entry.path != *path);
        // A file inside an empty directory entry makes that directory implicit.
        next.directories
            .retain(|directory| !path.starts_with(&format!("{directory}/")));
        next.files.push(crate::workspace::Entry {
            path: path.clone(),
            object: crate::artifact::object_digest(content.as_bytes()),
            executable,
        });
        if crate::workspace::validate_tree(&next).is_err() {
            return Ok(Err(format!(
                "{path} cannot be added: the workspace holds at most {} entries and a path cannot be both a file and a directory",
                crate::workspace::MAX_FILES
            )));
        }
        let others = (|| -> Result<u64> {
            let mut size = self
                .db
                .prepare_cached("SELECT length(bytes) FROM objects WHERE digest=?1")?;
            let mut total = 0u64;
            for entry in tree.files.iter().filter(|entry| entry.path != *path) {
                let length: i64 = size.query_row([&entry.object], |row| row.get(0))?;
                total = total.saturating_add(length.max(0) as u64);
            }
            Ok(total)
        })();
        let total = self.broker_result(others)? + content.len() as u64;
        if total > crate::workspace::MAX_TREE as u64 {
            return Ok(Err(format!(
                "{path} cannot be written: the workspace would exceed {}",
                crate::human_size(crate::workspace::MAX_TREE)
            )));
        }
        self.begin_stage(stage)?;
        let full = |error: Error| match error {
            Error::Exhausted => Ok(Err(STORE_FULL_SEAL.to_string())),
            error => Err(error),
        };
        let object = match self.put_workspace_object(stage, content.as_bytes()) {
            Ok(object) => object,
            Err(error) => return full(error),
        };
        if next.files.last().is_none_or(|entry| entry.object != object) {
            return Err(Error::Invalid);
        }
        match crate::workspace::encode_tree(&next)
            .and_then(|manifest| self.put_workspace_object(stage, &manifest))
        {
            Ok(root) => Ok(Ok((message, root))),
            Err(error) => full(error),
        }
    }

    /// The key this store's jobs are kept under: its directory.
    #[cfg(feature = "dext-strict-workspace")]
    fn job_session(&self) -> String {
        let file = std::path::Path::new(self.db.path().unwrap_or_default());
        crate::jobs::session_key(file.parent().unwrap_or(file))
    }

    /// Stops and forgets this session's background jobs (rewinds and restarts begin
    /// from a workspace the jobs never saw). Returns how many were still running.
    #[cfg(feature = "dext-strict-workspace")]
    pub fn stop_jobs(&self) -> usize {
        crate::jobs::stop_all(&self.job_session())
    }

    /// This session's background jobs still running.
    #[cfg(feature = "dext-strict-workspace")]
    pub fn running_jobs(&self) -> usize {
        crate::jobs::running(&self.job_session())
    }

    /// Starts a background job with bash over the run's input tree. Its result says
    /// how it started (waiting a moment, so a command that fails at once says why).
    #[cfg(feature = "dext-strict-workspace")]
    fn start_job(
        &mut self,
        request: &DispatchRequest,
        tree: &crate::workspace::Tree,
        command: &str,
        lifetime: u64,
        executor: &dext_native::strict::StrictExecutor,
        options: dext_native::strict::RunOptions<'_>,
    ) -> Result<(String, bool)> {
        use dext_native::strict::{JobState, Program};
        let line = dext_native::tool_policy::apply_bash_guardrails(command, false)
            .map_err(|_| Error::Invalid)?;
        let Some(bash) = executor.program(Program::Bash) else {
            return Ok((
                format!(
                    "bash is not available on this host; install it or set {}",
                    Program::Bash.variable()
                ),
                false,
            ));
        };
        let argv = vec![
            bash,
            "--noprofile".into(),
            "--norc".into(),
            "-c".into(),
            line,
        ];
        let objects: std::collections::BTreeMap<String, String> = tree
            .files
            .iter()
            .map(|entry| (entry.path.clone(), entry.object.clone()))
            .collect();
        let inputs = self.run_inputs(tree)?;
        let mut run = StoreRun {
            store: self,
            parent: request,
            objects: &objects,
            code: None,
            failed: None,
        };
        let started = executor.start_job(
            &inputs,
            &argv,
            std::time::Duration::from_secs(lifetime),
            dext_native::strict::RunOptions {
                observe: None,
                ..options
            },
            &[],
            &mut run,
        );
        if let Some(error) = run.failed.take() {
            return self.broker_result(Err(error));
        }
        let job = match started {
            Ok(job) => std::sync::Arc::new(job),
            Err(error) => {
                let reason = match dext_native::strict::run_failure(&error) {
                    Some(failure) => failure.reason.clone(),
                    None => format!("the job could not start: {error}"),
                };
                return Ok((reason, false));
            }
        };
        let id = crate::jobs::job_id(&request.intent.id);
        let never = std::sync::atomic::AtomicBool::new(false);
        // A command that fails at once (not found, syntax) is reported now.
        let settled = job.read(u64::MAX, 0, std::time::Duration::from_secs(1), &never);
        let first = job.read(0, 2048, std::time::Duration::ZERO, &never);
        crate::jobs::insert(
            &self.job_session(),
            id.clone(),
            crate::jobs::Entry {
                job: job.clone(),
                command: command.chars().take(200).collect(),
                started: std::time::SystemTime::now(),
            },
        );
        let printed = if first.bytes.is_empty() {
            "(no output yet)".to_string()
        } else {
            String::from_utf8_lossy(&first.bytes).into_owned()
        };
        Ok(match settled.state {
            JobState::Running => (
                format!(
                    "Started background job {id} (lifetime {lifetime} s). It runs over a \
                     snapshot of the workspace as it was now; files it changes are not \
                     saved. Read its output with read_job {{\"job\": \"{id}\"}}, end it \
                     with stop_job. First output:\n{printed}"
                ),
                true,
            ),
            state => (
                format!(
                    "Background job {id} {} within a second. Output:\n{printed}",
                    crate::jobs::describe(&state)
                ),
                state == JobState::Exited(Some(0)),
            ),
        })
    }

    /// read_job and stop_job: what a job printed and how it stands, or the session's
    /// jobs. An unknown job is an error result, never a refusal.
    #[cfg(feature = "dext-strict-workspace")]
    fn job_call(&self, tool: &StrictTool<'_>, interrupt: &AtomicBool) -> (String, bool) {
        use std::time::Duration;
        let session = self.job_session();
        let unknown = |id: &str| {
            (
                format!(
                    "no background job `{id}` in this session: jobs end when they are \
                     stopped, on rewind and when Spindle restarts (read_job without a job \
                     lists them)"
                ),
                false,
            )
        };
        let page = |id: &str, page: &dext_native::strict::JobOutput| {
            format!(
                "{}\n[{id} {} after {} s; output bytes {}..{} of {}; next offset={}]",
                String::from_utf8_lossy(&page.bytes),
                crate::jobs::describe(&page.state),
                page.elapsed.as_secs(),
                page.start,
                page.end,
                page.total,
                page.end
            )
        };
        match *tool {
            StrictTool::JobRead { job: None, .. } => {
                let jobs = crate::jobs::list(&session);
                if jobs.is_empty() {
                    return ("No background jobs in this session.".into(), true);
                }
                let mut text = String::from("Background jobs in this session:");
                for (id, command, state) in jobs {
                    text.push_str(&format!(
                        "\n{id}  {}  `{command}`",
                        crate::jobs::describe(&state)
                    ));
                }
                (text, true)
            }
            StrictTool::JobRead {
                job: Some(id),
                offset,
                limit,
                wait,
            } => {
                let Some(job) = crate::jobs::get(&session, id) else {
                    return unknown(id);
                };
                let wait = Duration::from_secs(wait);
                let output = match offset {
                    Some(offset) => job.read(offset, limit, wait, interrupt),
                    None => {
                        // The newest output, after waiting for the job to end.
                        let total = job.read(u64::MAX, 0, wait, interrupt).total;
                        job.read(
                            total.saturating_sub(limit as u64),
                            limit,
                            Duration::ZERO,
                            interrupt,
                        )
                    }
                };
                let note = match offset {
                    Some(offset) if output.start > offset => format!(
                        "[bytes {offset}..{} were dropped; a job keeps its newest {}]\n",
                        output.start,
                        crate::human_size(dext_native::strict::JOB_OUTPUT)
                    ),
                    _ => String::new(),
                };
                (format!("{note}{}", page(id, &output)), true)
            }
            StrictTool::JobStop { job: id } => {
                let Some(job) = crate::jobs::get(&session, id) else {
                    return unknown(id);
                };
                let before = job.state();
                job.stop();
                let never = AtomicBool::new(false);
                let total = job.read(u64::MAX, 0, Duration::ZERO, &never).total;
                let tail = job.read(total.saturating_sub(4096), 4096, Duration::ZERO, &never);
                let verb = if before == dext_native::strict::JobState::Running {
                    "Stopped"
                } else {
                    "Already ended:"
                };
                (
                    format!("{verb} {id}. Newest output:\n{}", page(id, &tail)),
                    true,
                )
            }
            _ => (String::new(), false),
        }
    }

    /// A bounded byte page, authenticated by a recorded output artifact reference.
    #[cfg(feature = "workspace-tools")]
    pub fn retained_output(
        &mut self,
        scope: &str,
        artifact: &str,
        offset: usize,
        limit: usize,
    ) -> Result<String> {
        if self.paused
            || !crate::valid_label(scope)
            || !crate::artifact::valid_digest(artifact)
            || !(1..=MAX_READ_WINDOW).contains(&limit)
        {
            return Err(Error::Invalid);
        }
        let mut query = self.db.prepare("SELECT r.result FROM staging_pins p JOIN artifact_stages s ON s.id=p.stage JOIN receipts r ON r.attempt=s.id WHERE p.digest=?1 AND s.scope=?2")?;
        let candidates = query.query_map(params![artifact, scope], |r| {
            crate::packed_blob(&self.db, r, 0, MAX_PAYLOAD)
        })?;
        let mut authorized = false;
        for bytes in candidates {
            let block: Block = serde_json::from_slice(&bytes?).map_err(|_| Error::Incompatible)?;
            if let Block::ToolResult { metadata, .. } = block {
                authorized |= metadata.artifact.as_deref() == Some(artifact);
            }
        }
        drop(query);
        if !authorized {
            return Err(Error::Invalid);
        }
        let bytes = self.read_object(scope, artifact)?;
        if offset > bytes.len() {
            return Err(Error::Invalid);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| Error::Incompatible)?;
        let mut start = offset;
        while !text.is_char_boundary(start) {
            start -= 1;
        }
        let mut end = offset.saturating_add(limit).min(bytes.len());
        while !text.is_char_boundary(end) {
            end += 1;
        }
        Ok(format!(
            "{}\n[output bytes {start}..{end}/{}; next offset={end}]",
            &text[start..end],
            bytes.len()
        ))
    }

    #[cfg(feature = "workspace-tools")]
    fn record_strict(
        &mut self,
        request: &DispatchRequest,
        call_id: &str,
        helper: &str,
        base: &str,
        recorded: Recorded,
    ) -> Result<DispatchOutcome> {
        self.record_sealed(request, call_id, helper, base, recorded, None)
    }

    /// Records a sealed workspace result. `app`: an app tool's answer (with its state
    /// patch, if any), whose reservation may be an earlier owner's (`reserved_effect`)
    /// and whose input's fields are the app's, not an output size.
    #[cfg(feature = "driver")]
    fn record_sealed(
        &mut self,
        request: &DispatchRequest,
        call_id: &str,
        helper: &str,
        base: &str,
        recorded: Recorded,
        app: Option<AppReceipt<'_>>,
    ) -> Result<DispatchOutcome> {
        let patch = app.as_ref().and_then(|app| app.patch);
        let details = app.as_ref().and_then(|app| app.details);
        let app_cap = app.as_ref().map(|app| app.max_output);
        let app = app.is_some();
        let Recorded {
            content,
            success,
            root,
            trailer,
        } = recorded;
        let stage = crate::artifact::Stage {
            id: request.attempt.id.clone(),
            scope: request.scope.clone(),
            owner_epoch: self.epoch,
        };
        let args = serde_json::from_slice::<serde_json::Value>(&request.intent.input)
            .map_err(|_| Error::Invalid)?;
        // Paging tools return bounded windows of what is already stored; they are not
        // charged to the turn's budget, which they exist to work around. Nor are host
        // tools: their results are short by construction, and a `show` result keeps its
        // display line whole.
        let paging = matches!(
            request.intent.adapter.as_str(),
            "read_file" | "read_output" | "read_job"
        ) || crate::turn::HOST_TOOLS.contains(&request.intent.adapter.as_str());
        let requested = app_cap.unwrap_or_else(|| {
            args["max_output"].as_u64().unwrap_or(MAX_TOOL_TEXT as u64) as usize
        });
        // Tool effect ids are `<execution>/m/<n>/t/<position>`.
        let execution = match request.intent.id.rsplit_once("/m/") {
            Some((execution, _)) => format!("{execution}/"),
            None => {
                let state = TurnContinuation::restore(&self.snapshot()?.continuation)
                    .map_err(|_| Error::Invalid)?;
                format!("{}/", state.execution())
            }
        };
        // Every id with the prefix sorts in [prefix, prefix with its final '/' as '0'),
        // a range the intents primary key answers without a scan.
        let upper = format!("{}0", &execution[..execution.len() - 1]);
        let mut used = 0usize;
        let mut rows = self.db.prepare("SELECT r.result FROM receipts r JOIN attempts a ON a.id=r.attempt JOIN intents i ON i.id=a.effect WHERE i.id>=?1 AND i.id<?2 AND ?3 AND i.adapter NOT IN ('dext-model-v1','read_file','read_output','read_job','show','settings','spindle_source','spindle_install')")?;
        let prior = rows.query_map(params![execution, upper, !paging], |r| {
            crate::packed_blob(&self.db, r, 0, MAX_PAYLOAD)
        })?;
        for bytes in prior {
            if let Ok(Block::ToolResult { content, .. }) = serde_json::from_slice::<Block>(&bytes?)
            {
                used = used.saturating_add(content.len());
            }
        }
        drop(rows);
        let cap = if let Some(cap) = app_cap {
            cap
        } else if paging {
            usize::MAX
        } else {
            requested
                .clamp(4096, 32768)
                .min(TURN_OUTPUT_BUDGET.saturating_sub(used))
        };
        let artifact = if content.len() + trailer.len() > cap {
            let stored = self
                .begin_stage(&stage)
                .and_then(|()| self.put_object(&stage, content.as_bytes()));
            match stored {
                Ok(digest) => Some(digest),
                // A full store keeps only the window below.
                Err(Error::Exhausted) => None,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let mut content = fit_result(content, artifact.as_deref(), &trailer, cap);
        if app_cap.is_some() && content.len() > cap {
            let mut end = cap;
            while !content.is_char_boundary(end) {
                end -= 1;
            }
            content.truncate(end);
        }
        let block = Block::ToolResult {
            tool_use_id: call_id.to_string(),
            content,
            is_error: Some(!success),
            metadata: dext_transcript::ToolResultMetadata {
                status: Some(if success { "ok" } else { "error" }.into()),
                artifact,
                ..Default::default()
            },
        };
        let result = serde_json::to_vec(&block).map_err(|_| Error::Invalid)?;
        crate::fits(result.len())?;
        let recorded = (|| -> Result<()> {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            match app {
                true => reserved_effect(&tx, self.epoch, request).map(|_| ())?,
                false => pending_effect(&tx, request).map(|_| ())?,
            }
            crate::workspace::validate_root(&tx, &request.scope, &root)?;
            crate::completion::record_receipt(&tx, &request.attempt.id, &result, Some(&root))?;
            crate::execute_one(
                &tx,
                "INSERT INTO strict_receipts VALUES(?1,?2,?3,?4)",
                params![request.attempt.id, base, root, helper],
            )?;
            if let Some(details) = details {
                crate::execute_one(
                    &tx,
                    "INSERT INTO app_details VALUES(?1,?2)",
                    params![request.attempt.id, crate::codec::store(&tx, details)?],
                )?;
            }
            if let Some(patch) = patch {
                crate::execute_one(
                    &tx,
                    "INSERT INTO app_patches VALUES(?1,?2)",
                    params![request.attempt.id, crate::codec::store(&tx, patch)?],
                )?;
            }
            tx.commit()?;
            Ok(())
        })();
        self.broker_result(recorded)?;
        Ok(DispatchOutcome::Recorded(result))
    }

    /// The store's own workspace tools, for a host with no command runner: read_file,
    /// write_file, edit_file and read_output, computed in-process over the call's
    /// immutable base. `helper` is the host's runner identity, which the attempt's
    /// label names (`runner_label`). Any other workspace tool is recorded as a failed
    /// result (this host does not run it); hosts refuse those before reserving.
    #[cfg(feature = "workspace-tools")]
    pub fn dispatch_file_workspace<A>(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        helper: &[u8; 32],
        mut authorize: A,
    ) -> Result<DispatchOutcome>
    where
        A: FnMut(&DispatchRequest) -> Result<()>,
    {
        if request.attempt.implementation != crate::turn::runner_label(helper) {
            return Err(Error::Conflict);
        }
        let hex: String = helper.iter().map(|byte| format!("{byte:02x}")).collect();
        let room = strict_room(&request.intent.adapter);
        let fits = room == crate::artifact::Room::default() || self.make_room(room)?;
        let committed = match self.prepare_dispatch(
            request,
            interrupt,
            &mut authorize,
            &ResultProfile::StrictWorkspace,
        )? {
            PreparedDispatch::Complete(outcome) => return Ok(outcome),
            PreparedDispatch::Execute(committed) => committed,
        };
        let Effect::Tool { call, .. } = committed.effect() else {
            return Err(Error::Invalid);
        };
        let (call_id, name) = (call.id.clone(), call.name.clone());
        let tool = strict_tool(committed.effect())?;
        let base = self.strict_base(request)?;
        let failed = |content: String| Recorded {
            content,
            success: false,
            root: base.clone(),
            trailer: String::new(),
        };
        if !fits {
            return self.record_strict(request, &call_id, &hex, &base, failed(STORE_FULL.into()));
        }
        let recorded = match tool {
            StrictTool::Read { .. }
            | StrictTool::Write { .. }
            | StrictTool::Edit { .. }
            | StrictTool::Output { .. } => {
                let tree = self.tree_map(&request.scope, &base)?;
                let stage = crate::artifact::Stage {
                    id: request.attempt.id.clone(),
                    scope: request.scope.clone(),
                    owner_epoch: request.owner_epoch,
                };
                match self.apply_file_tool(&stage, &base, &tree, &tool)? {
                    Ok((content, root)) => Recorded {
                        content,
                        success: true,
                        root,
                        trailer: String::new(),
                    },
                    Err(message) => failed(message),
                }
            }
            _ => failed(format!(
                "`{name}` needs a command runner, which this host does not have"
            )),
        };
        self.record_strict(request, &call_id, &hex, &base, recorded)
    }

    /// Reserves a `bash` call that a runner outside the store runs (an app's own, or a
    /// Spindle host's over the network). The command is checked as the sandbox checks
    /// it; `helper` is the runner's identity, which the attempt's label names.
    #[cfg(feature = "workspace-tools")]
    pub fn reserve_runner_call(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        helper: &[u8; 32],
    ) -> Result<RunnerReservation> {
        if request.attempt.implementation != crate::turn::runner_label(helper)
            || request.intent.adapter != "bash"
        {
            return Err(Error::Conflict);
        }
        let room = strict_room("bash");
        let fits = self.make_room(room)?;
        let committed = match self.prepare_dispatch(
            request,
            interrupt,
            &mut |_: &DispatchRequest| Ok(()),
            &ResultProfile::StrictWorkspace,
        )? {
            PreparedDispatch::Complete(outcome) => return Ok(RunnerReservation::Settled(outcome)),
            PreparedDispatch::Execute(committed) => committed,
        };
        let StrictTool::Bash { command, timeout } = strict_tool(committed.effect())? else {
            return Err(Error::Invalid);
        };
        let command = dext_native::tool_policy::apply_bash_guardrails(command, false)
            .map_err(|_| Error::Invalid)?;
        let base = self.strict_base(request)?;
        if !fits {
            let Effect::Tool { call, .. } = committed.effect() else {
                return Err(Error::Invalid);
            };
            let hex: String = helper.iter().map(|byte| format!("{byte:02x}")).collect();
            let recorded = Recorded {
                content: STORE_FULL.into(),
                success: false,
                root: base.clone(),
                trailer: String::new(),
            };
            let outcome = self.record_strict(request, &call.id.clone(), &hex, &base, recorded)?;
            return Ok(RunnerReservation::Settled(outcome));
        }
        Ok(RunnerReservation::Reserved(RunnerCall {
            command,
            timeout,
            root: base,
        }))
    }

    /// The workspace a reserved runner call runs over: its files (with objects to read
    /// through `runner_file`), links and empty directories.
    #[cfg(feature = "workspace-tools")]
    pub fn runner_tree(&mut self, request: &DispatchRequest) -> Result<crate::workspace::Tree> {
        if self.paused {
            return Err(Error::Paused);
        }
        validate_request(request)?;
        pending_effect(&self.db, request)?;
        let base = self.strict_base(request)?;
        self.tree_map(&request.scope, &base)
    }

    /// One file of a reserved runner call's workspace, by its tree entry's object.
    #[cfg(feature = "workspace-tools")]
    pub fn runner_file(&mut self, request: &DispatchRequest, object: &str) -> Result<Vec<u8>> {
        let tree = self.runner_tree(request)?;
        if !tree.files.iter().any(|entry| entry.object == object) {
            return Err(Error::Invalid);
        }
        self.read_object(&request.scope, object)
    }

    /// Records what the runner reported for a call this owner reserved: its output and
    /// the workspace it left (changes staged as the call's new root, adopted with the
    /// result, so rewind undoes them). A failed run keeps no changes. A result for a
    /// reservation from an earlier epoch is refused (the driver seals that one).
    #[cfg(feature = "workspace-tools")]
    pub fn record_runner_result(
        &mut self,
        request: &DispatchRequest,
        helper: &[u8; 32],
        result: RunnerResult,
    ) -> Result<DispatchOutcome> {
        if self.paused {
            return Err(Error::Paused);
        }
        validate_request(request)?;
        if request.attempt.implementation != crate::turn::runner_label(helper)
            || request.intent.adapter != "bash"
        {
            return Err(Error::Conflict);
        }
        let Effect::Tool { call, .. } = pending_effect(&self.db, request)? else {
            return Err(Error::Invalid);
        };
        let hex: String = helper.iter().map(|byte| format!("{byte:02x}")).collect();
        let base = self.strict_base(request)?;
        let RunnerResult {
            output,
            success,
            writes,
            deletes,
            links,
            directories,
            failure,
        } = result;
        let separator = if output.is_empty() { "" } else { "\n" };
        let recorded = match failure {
            Some(reason) => Recorded {
                trailer: format!("{separator}{reason}; {UNSEALED_RUN}"),
                content: output,
                success: false,
                root: base.clone(),
            },
            None => {
                let changes = Changes {
                    writes,
                    deletes,
                    links,
                    directories,
                };
                let staged = match changes.is_empty() {
                    true => Ok(base.clone()),
                    false => self.stage_answer(request, &base, changes)?,
                };
                match staged {
                    Ok(root) => Recorded {
                        content: output,
                        success,
                        root,
                        trailer: String::new(),
                    },
                    Err(reason) => Recorded {
                        trailer: format!("{separator}{reason}; {UNSEALED_RUN}"),
                        content: output,
                        success: false,
                        root: base.clone(),
                    },
                }
            }
        };
        self.record_strict(request, &call.id, &hex, &base, recorded)
    }

    #[cfg(feature = "dext-strict-workspace")]
    /// Resolves a strict bash attempt that an earlier owner epoch reserved but never
    /// recorded. That owner released the realm lock, so its executor call has ended:
    /// the sandbox dies with its owner (die-with-parent, PDEATHSIG, PID namespace;
    /// host runs: process group or kill-on-close job object),
    /// has no host writes, and its private candidate was never published. A run the
    /// host granted network may still have had external effects; only the workspace
    /// is guaranteed unchanged.
    /// The call is recorded as an interrupted error over its unchanged base; checked
    /// receipt adoption is still required. Never runs anything.
    pub fn seal_interrupted_strict(
        &mut self,
        request: &DispatchRequest,
        executor: &dext_native::strict::StrictExecutor,
    ) -> Result<Vec<u8>> {
        self.seal_interrupted_strict_with_digest(request, *executor.helper_digest())
    }

    /// Seals an interrupted reservation using the runner identity returned by the
    /// host, without requiring a live executor. The identity must match its attempt.
    #[cfg(feature = "driver")]
    pub fn seal_interrupted_strict_with_digest(
        &mut self,
        request: &DispatchRequest,
        digest: [u8; 32],
    ) -> Result<Vec<u8>> {
        if self.paused {
            return Err(Error::Paused);
        }
        validate_request(request)?;
        let helper: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        if request.attempt.implementation != crate::turn::runner_label(&digest)
            || request.owner_epoch != self.epoch
            || request.attempt.owner_epoch >= self.epoch
        {
            return Err(Error::Conflict);
        }
        let result = (|| -> Result<Vec<u8>> {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let epoch: u64 = tx.query_row("SELECT epoch FROM realm WHERE id=1", [], |r| {
                crate::nonnegative_id(r, 0)
            })?;
            if epoch != self.epoch {
                return Err(Error::Stale);
            }
            let attempt: Option<(String, u64, String, String)> = tx.query_row(
                "SELECT effect,owner_epoch,implementation,grant_reference FROM attempts WHERE id=?1",
                [&request.attempt.id],
                |r| Ok((crate::bounded_label(r, 0)?, crate::nonnegative_id(r, 1)?, crate::bounded_label(r, 2)?, crate::bounded_label(r, 3)?)),
            ).optional()?;
            let recorded: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM receipts WHERE attempt=?1)",
                [&request.attempt.id],
                |r| r.get(0),
            )?;
            if attempt
                != Some((
                    request.attempt.effect.clone(),
                    request.attempt.owner_epoch,
                    request.attempt.implementation.clone(),
                    request.attempt.grant_reference.clone(),
                ))
                || recorded
            {
                return Err(Error::Conflict);
            }
            let intent: Option<(u64, String, Vec<u8>, String)> = tx.query_row(
                "SELECT i.revision,i.adapter,i.input,r.scope FROM intents i JOIN revisions r ON r.revision=i.revision WHERE i.id=?1", [&request.intent.id],
                |r| Ok((crate::nonnegative_id(r, 0)?, crate::bounded_label(r, 1)?, crate::packed_blob(&tx, r, 2, MAX_PAYLOAD)?, crate::bounded_label(r, 3)?)),
            ).optional()?;
            if intent
                != Some((
                    request.intent_revision,
                    request.intent.adapter.clone(),
                    request.intent.input.clone(),
                    request.scope.clone(),
                ))
                || crate::rewind::superseded(&tx, &request.intent.id)?
            {
                return Err(Error::Conflict);
            }
            crate::ownership::live_allowed(&tx, &request.scope)?;
            let effect = head_effect(&tx, request)?;
            let Effect::Tool { call, .. } = effect else {
                return Err(Error::Invalid);
            };
            // The call ran (as every strict call does) over the scope head's tree,
            // which head_effect just proved `expected_revision` is; calls of a batch
            // chain, so that can be a later tree than the one that made the call.
            let base: Option<String> = tx.query_row(
                "SELECT workspace FROM revisions WHERE revision=?1 AND scope=?2",
                params![request.expected_revision as i64, request.scope],
                |row| crate::optional_digest(row, 0),
            )?;
            let base = base.ok_or(Error::Invalid)?;
            let block = Block::ToolResult {
                tool_use_id: call.id.clone(),
                content: INTERRUPTED_STRICT.into(),
                is_error: Some(true),
                metadata: dext_transcript::ToolResultMetadata {
                    status: Some("error".into()),
                    ..Default::default()
                },
            };
            let result = serde_json::to_vec(&block).map_err(|_| Error::Invalid)?;
            crate::completion::record_receipt(&tx, &request.attempt.id, &result, Some(&base))?;
            crate::execute_one(
                &tx,
                "INSERT INTO strict_receipts VALUES(?1,?2,?3,?4)",
                params![request.attempt.id, base, base, helper],
            )?;
            tx.commit()?;
            Ok(result)
        })();
        self.broker_result(result)
    }

    #[cfg(feature = "dext-native-image")]
    /// Narrow native read_image profile, not a general native executor. Authorization
    /// runs before reuse and after reservation; source policy opens a fresh handle
    /// only for a new committed attempt. The pinned worker is Dext-owned and bounded,
    /// not an OS sandbox. Source resolution/containment remains trusted host policy.
    /// Objects, scoped pins, provenance and ingress commit together. Failure leaves
    /// the reserved attempt unresolved; successful ingress is not turn adoption.
    pub fn dispatch_read_image<A, S>(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        worker: &dext_native::image::PinnedWorker,
        mut authorize: A,
        open_source: S,
    ) -> Result<DispatchOutcome>
    where
        A: FnMut(&DispatchRequest) -> Result<()>,
        S: FnOnce(&CommittedEffect, &AtomicBool) -> Result<ImageSource>,
    {
        let committed =
            match self.prepare_read_image(request, interrupt, worker, &mut authorize, false)? {
                PreparedDispatch::Complete(outcome) => return Ok(outcome),
                PreparedDispatch::Execute(committed) => committed,
            };
        let stage = self.image_stage(request)?;
        let source = open_source(&committed, interrupt)?;
        let effect = pending_effect(&self.db, request);
        let effect = self.broker_result(effect)?;
        check_read_image(&effect)?;
        let image = worker
            .sanitize_file(&source.file, source.approved_digest, Some(interrupt))
            .map_err(|_| {
                if interrupt.load(Ordering::SeqCst) {
                    Error::Cancelled
                } else {
                    Error::Invalid
                }
            })?;
        self.record_image_output(request, &stage, &image, false)
    }

    #[cfg(feature = "dext-native-image")]
    /// Single-writer cancellation path. Trusted control input is committed before
    /// signalling the scoped worker. Storage/control failure stops work as containment,
    /// never as an acknowledged cancel. A sealed racing result remains historical;
    /// this method never adopts it or classifies an unresolved attempt as retriable.
    pub fn dispatch_read_image_controlled<A, S, C>(
        &mut self,
        request: &DispatchRequest,
        worker: &dext_native::image::PinnedWorker,
        mut authorize: A,
        open_source: S,
        control: &mut C,
    ) -> Result<DispatchOutcome>
    where
        A: FnMut(&DispatchRequest) -> Result<()>,
        S: FnOnce(&CommittedEffect, &AtomicBool) -> Result<ImageSource>,
        C: ImageControl,
    {
        let stop = AtomicBool::new(false);
        let committed =
            match self.prepare_read_image(request, &stop, worker, &mut authorize, true)? {
                PreparedDispatch::Complete(outcome) => return Ok(outcome),
                PreparedDispatch::Execute(committed) => committed,
            };
        let stage = self.image_stage(request)?;
        if self.poll_image_control(request, control, ImagePhase::BeforeSource)? {
            self.finish_image_cleanup(request, worker, None)?;
            control.cleanup_committed(crate::completion::ImageCleanup::NotStarted)?;
            return Err(Error::Cancelled);
        }
        let source = match open_source(&committed, &stop) {
            Ok(source) => source,
            Err(error) => {
                self.finish_image_cleanup(request, worker, None)?;
                control.cleanup_committed(crate::completion::ImageCleanup::NotStarted)?;
                return Err(error);
            }
        };
        if self.poll_image_control(request, control, ImagePhase::BeforeWorker)? {
            self.finish_image_cleanup(request, worker, None)?;
            control.cleanup_committed(crate::completion::ImageCleanup::NotStarted)?;
            return Err(Error::Cancelled);
        }
        let effect = pending_effect(&self.db, request);
        self.broker_result(effect)?;
        let (image, cancelled) = std::thread::scope(|scope| -> Result<_> {
            let task = std::thread::Builder::new()
                .name("spindle-image".into())
                .spawn_scoped(scope, || {
                    worker.sanitize_file_report(&source.file, source.approved_digest, Some(&stop))
                })?;
            // Drop before the scope joins, including callback panics/unwind.
            let _stop_on_drop = StopImageOnDrop(&stop);
            let cancellation = loop {
                match self.poll_image_control(request, control, ImagePhase::Running) {
                    Ok(true) => {
                        stop.store(true, Ordering::SeqCst);
                        break Ok(true);
                    }
                    Err(error) => {
                        stop.store(true, Ordering::SeqCst);
                        break Err(error);
                    }
                    Ok(false) => {}
                }
                if task.is_finished() {
                    break Ok(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            };
            let image = task.join().map_err(|_| Error::Invalid)?;
            Ok((image, cancellation))
        })?;
        let cancelled = match cancelled {
            Ok(false) => self.poll_image_control(request, control, ImagePhase::Joined),
            other => other,
        };
        let cleanup_ack = if !self.paused {
            let cleanup = self.finish_image_cleanup(request, worker, Some(&image))?;
            if cleanup != crate::completion::ImageCleanup::Unknown {
                control.cleanup_committed(cleanup)
            } else {
                Ok(())
            }
        } else {
            Ok(())
        };
        match image.into_result() {
            Ok(image) => {
                let cancellation = match cancelled {
                    Ok(false) => self.poll_image_control(request, control, ImagePhase::Sealed),
                    other => other,
                };
                // Sealed output is historical even if control acknowledgement failed.
                // A paused store cannot safely retain it; the attempt stays unresolved.
                if self.paused {
                    cancellation?;
                    return Err(Error::Paused);
                }
                let recorded = self.record_image_output(request, &stage, &image, true)?;
                cleanup_ack?;
                cancellation?;
                Ok(recorded)
            }
            Err(_) => {
                cleanup_ack?;
                if cancelled? {
                    Err(Error::Cancelled)
                } else {
                    Err(Error::Invalid)
                }
            }
        }
    }

    #[cfg(feature = "dext-native-image")]
    fn finish_image_cleanup(
        &mut self,
        request: &DispatchRequest,
        worker: &dext_native::image::PinnedWorker,
        execution: Option<&dext_native::image::ImageExecution>,
    ) -> Result<crate::completion::ImageCleanup> {
        use crate::completion::ImageCleanup;
        use dext_native::image::RootCleanup;
        if self.paused {
            return Err(Error::Paused);
        }
        let state = match execution {
            None => 1,
            Some(report) => {
                if report.worker_digest() != worker.digest() {
                    return Err(Error::Conflict);
                }
                match report.cleanup() {
                    RootCleanup::Unknown => 0,
                    RootCleanup::NotStarted => 1,
                    RootCleanup::Reaped => 2,
                    RootCleanup::TreeEmptied => 3,
                }
            }
        };
        let result = (|| -> Result<_> {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            bound_effect(&tx, request)?;
            let expected_worker: String = worker
                .digest()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let (saved_worker, old): (String, u64) = tx.query_row(
                "SELECT worker,state FROM image_executions WHERE attempt=?1",
                [&request.attempt.id],
                |row| {
                    Ok((
                        crate::bounded_label(row, 0)?,
                        crate::nonnegative_id(row, 1)?,
                    ))
                },
            )?;
            if saved_worker != expected_worker || (old != 0 && old != state) {
                return Err(Error::Conflict);
            }
            if old != state {
                crate::execute_one(
                    &tx,
                    "UPDATE image_executions SET state=?2 WHERE attempt=?1 AND state=0",
                    params![request.attempt.id, state as i64],
                )?;
            }
            tx.commit()?;
            ImageCleanup::decode(state)
        })();
        self.broker_result(result)
    }

    #[cfg(feature = "dext-native-image")]
    fn poll_image_control<C: ImageControl>(
        &mut self,
        request: &DispatchRequest,
        control: &mut C,
        phase: ImagePhase,
    ) -> Result<bool> {
        let Some(command) = control.cancellation(phase)? else {
            return Ok(false);
        };
        let epoch = self.cancel_scope(&request.scope, &command)?;
        control.committed(&command, epoch)?;
        Ok(true)
    }

    #[cfg(feature = "dext-native-image")]
    fn prepare_read_image<A>(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        worker: &dext_native::image::PinnedWorker,
        authorize: &mut A,
        tracked: bool,
    ) -> Result<PreparedDispatch>
    where
        A: FnMut(&DispatchRequest) -> Result<()>,
    {
        let implementation = format!(
            "dext-read-image-v1-{}",
            worker
                .digest()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        if request.attempt.implementation != implementation {
            return Err(Error::Conflict);
        }
        self.prepare_dispatch(
            request,
            interrupt,
            authorize,
            &ResultProfile::ReadImage(tracked),
        )
    }

    #[cfg(feature = "dext-native-image")]
    fn image_stage(&mut self, request: &DispatchRequest) -> Result<crate::artifact::Stage> {
        let stage = crate::artifact::Stage {
            id: request.attempt.id.clone(),
            scope: request.scope.clone(),
            owner_epoch: request.owner_epoch,
        };
        self.begin_stage(&stage)?;
        Ok(stage)
    }

    #[cfg(feature = "dext-native-image")]
    fn record_image_output(
        &mut self,
        request: &DispatchRequest,
        stage: &crate::artifact::Stage,
        image: &dext_native::image::SanitizedImage,
        historical: bool,
    ) -> Result<DispatchOutcome> {
        let mut result = Vec::new();
        self.put_sanitized_image_checked(stage, image, historical, |tx, image| {
            if historical {
                bound_effect(tx, request)?;
            } else {
                pending_effect(tx, request)?;
            }
            result = crate::image::ImageReceipt {
                images: vec![image.clone()],
            }
            .encode()?;
            crate::completion::record_receipt(tx, &request.attempt.id, &result, None)
        })?;
        Ok(DispatchOutcome::Recorded(result))
    }

    /// Authorization is trusted host policy, never a saved low-risk label. It runs
    /// before historical receipt disclosure and again after fresh reservation.
    /// Executor errors/panics, denied revalidation, crashes or ambiguous storage
    /// leave the attempt unresolved; neither the same ID nor a new ID reruns it.
    /// The exclusive Store borrow serializes cooperating local authority changes;
    /// adapters must observe cancellation and revalidate external resources.
    pub fn dispatch_turn_effect<A, E>(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        authorize: A,
        execute: E,
    ) -> Result<DispatchOutcome>
    where
        A: FnMut(&DispatchRequest) -> Result<()>,
        E: FnOnce(&CommittedEffect, &AtomicBool) -> Result<Vec<u8>>,
    {
        self.dispatch_profile(
            request,
            interrupt,
            authorize,
            execute,
            ResultProfile::Native,
        )
    }

    #[cfg(feature = "dext-image-continuation")]
    /// Dispatches only tool obligations returning SAR1/SIJ1 images already staged
    /// by the trusted host. Retained bytes/pins/provenance are checked on fresh
    /// ingress; checked adoption revalidates them even on historical reuse.
    /// Worker approval does not authorize disclosure or authenticate an executor.
    pub fn dispatch_turn_image_effect<A, E>(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        approved_worker: [u8; 32],
        authorize: A,
        execute: E,
    ) -> Result<DispatchOutcome>
    where
        A: FnMut(&DispatchRequest) -> Result<()>,
        E: FnOnce(&CommittedEffect, &AtomicBool) -> Result<Vec<u8>>,
    {
        self.dispatch_profile(
            request,
            interrupt,
            authorize,
            execute,
            ResultProfile::Image(approved_worker),
        )
    }

    fn dispatch_profile<A, E>(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        mut authorize: A,
        execute: E,
        profile: ResultProfile,
    ) -> Result<DispatchOutcome>
    where
        A: FnMut(&DispatchRequest) -> Result<()>,
        E: FnOnce(&CommittedEffect, &AtomicBool) -> Result<Vec<u8>>,
    {
        let committed = match self.prepare_dispatch(request, interrupt, &mut authorize, &profile)? {
            PreparedDispatch::Complete(outcome) => return Ok(outcome),
            PreparedDispatch::Execute(committed) => committed,
        };
        let result = execute(&committed, interrupt)?;
        if result.is_empty() {
            return Err(Error::Invalid);
        }
        crate::fits(result.len())?;
        // Preserve bounded sealed output even when semantic admission fails. A bad
        // adapter result remains inspectable, but cannot advance the checked turn.
        self.ingest_receipt(&request.attempt.id, &result)?;
        let validation = match profile {
            ResultProfile::Native => validate_result(&committed.effect, &result),
            ResultProfile::Model => Err(Error::Invalid),
            #[cfg(feature = "dext-image-continuation")]
            ResultProfile::Image(worker) => self.validate_image_result(request, &result, &worker),
            #[cfg(feature = "dext-native-image")]
            ResultProfile::ReadImage(_) => Err(Error::Invalid),
            #[cfg(feature = "workspace-tools")]
            ResultProfile::StrictWorkspace => Err(Error::Invalid),
            #[cfg(feature = "driver")]
            ResultProfile::App => Err(Error::Invalid),
        };
        self.broker_result(validation)?;
        Ok(DispatchOutcome::Recorded(result))
    }

    fn prepare_dispatch<A>(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        authorize: &mut A,
        profile: &ResultProfile,
    ) -> Result<PreparedDispatch>
    where
        A: FnMut(&DispatchRequest) -> Result<()>,
    {
        if self.paused {
            return Err(Error::Paused);
        }
        validate_request(request)?;
        if request.owner_epoch != self.epoch {
            return Err(Error::Stale);
        }
        authorize(request)?;
        let reserved = self.reserve_dispatch(request, interrupt, profile);
        let reserved = self.broker_result(reserved)?;
        match reserved {
            Reservation::Recorded(bytes) => {
                return Ok(PreparedDispatch::Complete(DispatchOutcome::Reused(bytes)));
            }
            Reservation::Unresolved => {
                return Ok(PreparedDispatch::Complete(DispatchOutcome::Reconcile));
            }
            Reservation::Fresh => {}
        }
        if interrupt.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        authorize(request)?;
        let effect = pending_effect(&self.db, request);
        let effect = self.broker_result(effect)?;
        profile.check_effect(&effect)?;
        if interrupt.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        Ok(PreparedDispatch::Execute(Box::new(CommittedEffect {
            effect,
            attempt: request.attempt.clone(),
            revision: request.expected_revision,
        })))
    }

    #[cfg(feature = "dext-image-continuation")]
    fn validate_image_result(
        &mut self,
        request: &DispatchRequest,
        bytes: &[u8],
        approved_worker: &[u8; 32],
    ) -> Result<()> {
        let receipt = crate::image::ImageReceipt::decode(bytes)?;
        let tx = self.db.transaction()?;
        let epoch = tx.query_row("SELECT epoch FROM realm WHERE id=1", [], |row| {
            crate::nonnegative_id(row, 0)
        })?;
        if epoch != self.epoch || epoch != request.owner_epoch {
            return Err(Error::Stale);
        }
        crate::image::validate_verified_images(&tx, &request.scope, &receipt, approved_worker)?;
        tx.commit()?;
        Ok(())
    }

    fn reserve_dispatch(
        &mut self,
        request: &DispatchRequest,
        interrupt: &AtomicBool,
        profile: &ResultProfile,
    ) -> Result<Reservation> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let epoch: u64 = tx.query_row("SELECT epoch FROM realm WHERE id=1", [], |r| {
            crate::nonnegative_id(r, 0)
        })?;
        if epoch != self.epoch || epoch != request.owner_epoch {
            return Err(Error::Stale);
        }
        let intent: Option<(u64, String, Vec<u8>, String)> = tx.query_row(
            "SELECT i.revision,i.adapter,i.input,r.scope FROM intents i JOIN revisions r ON r.revision=i.revision WHERE i.id=?1", [&request.intent.id],
            |r| Ok((crate::nonnegative_id(r, 0)?, crate::bounded_label(r, 1)?, crate::packed_blob(&tx, r, 2, MAX_PAYLOAD)?, crate::bounded_label(r, 3)?)),
        ).optional()?;
        let intent = intent.ok_or(Error::Invalid)?;
        if intent
            != (
                request.intent_revision,
                request.intent.adapter.clone(),
                request.intent.input.clone(),
                request.scope.clone(),
            )
        {
            return Err(Error::Conflict);
        }
        let old: Option<(String, String, u64, String, String)> = tx.query_row(
            "SELECT id,effect,owner_epoch,implementation,grant_reference FROM attempts WHERE id=?1 OR effect=?2 LIMIT 1",
            params![request.attempt.id, request.attempt.effect],
            |r| Ok((crate::bounded_label(r, 0)?, crate::bounded_label(r, 1)?, crate::nonnegative_id(r, 2)?, crate::bounded_label(r, 3)?, crate::bounded_label(r, 4)?)),
        ).optional()?;
        if let Some(old) = old {
            if old
                != (
                    request.attempt.id.clone(),
                    request.attempt.effect.clone(),
                    request.attempt.owner_epoch,
                    request.attempt.implementation.clone(),
                    request.attempt.grant_reference.clone(),
                )
            {
                return Err(Error::Conflict);
            }
            let receipt: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT result FROM receipts WHERE attempt=?1",
                    [&request.attempt.id],
                    |r| crate::packed_blob(&tx, r, 0, MAX_PAYLOAD),
                )
                .optional()?;
            return Ok(match receipt {
                Some(bytes) => Reservation::Recorded(bytes),
                None => Reservation::Unresolved,
            });
        }
        crate::ownership::admit_allowed(&tx, &request.scope)?;
        if crate::rewind::superseded(&tx, &request.intent.id)? {
            return Err(Error::Conflict);
        }
        let effect = pending_effect(&tx, request)?;
        profile.check_effect(&effect)?;
        if matches!(effect, Effect::Tool { .. }) {
            refuse_guarded(&tx, request)?;
        }
        if interrupt.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        crate::execute_one(
            &tx,
            "INSERT INTO attempts VALUES(?1,?2,?3,?4,?5)",
            params![
                request.attempt.id,
                request.attempt.effect,
                request.attempt.owner_epoch as i64,
                request.attempt.implementation,
                request.attempt.grant_reference,
            ],
        )?;
        #[cfg(feature = "dext-native-image")]
        if matches!(profile, ResultProfile::ReadImage(true)) {
            let worker = request
                .attempt
                .implementation
                .strip_prefix("dext-read-image-v1-")
                .ok_or(Error::Invalid)?;
            crate::execute_one(
                &tx,
                "INSERT INTO image_executions VALUES(?1,?2,0)",
                params![request.attempt.id, worker],
            )?;
        }
        tx.commit()?;
        Ok(Reservation::Fresh)
    }
    fn broker_result<T>(&mut self, result: Result<T>) -> Result<T> {
        if matches!(
            result,
            Err(Error::Sql(_) | Error::Io(_) | Error::Locked | Error::Incompatible)
        ) {
            self.paused = true;
        }
        result
    }
}

fn validate_result(effect: &Effect, bytes: &[u8]) -> Result<()> {
    match effect {
        Effect::Model { waiting, .. } => {
            let recorded = ModelWait::restore(bytes).map_err(|_| Error::Invalid)?;
            if recorded.phase() != Phase::Recorded {
                return Err(Error::Invalid);
            }
            let response = recorded.response().ok_or(Error::Invalid)?;
            waiting
                .record(recorded.request(), recorded.attempt(), response)
                .map_err(|_| Error::Conflict)?;
            NativeResponse::restore(response).map_err(|_| Error::Invalid)?;
        }
        Effect::Tool { call, .. } => {
            let block: Block = serde_json::from_slice(bytes).map_err(|_| Error::Invalid)?;
            if !matches!(&block, Block::ToolResult { tool_use_id, .. } if tool_use_id == &call.id)
                || serde_json::to_vec(&block).map_err(|_| Error::Invalid)? != bytes
            {
                return Err(Error::Invalid);
            }
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "dext-strict-workspace"))]
mod tests {
    use super::*;

    #[test]
    fn results_keep_the_pointer_and_closing_line_whatever_budget_is_left() {
        let digest = "a".repeat(64);
        let output = "x".repeat(50_000);
        for cap in [0, 100, 300, 4096, 32768] {
            let text = fit_result(output.clone(), Some(&digest), "\nclosing; done", cap);
            assert!(
                text.ends_with(&format!(
                    "[retained output: read_output artifact={digest} offset=0]\nclosing; done"
                )),
                "{cap}: {text}"
            );
            if cap >= 4096 {
                assert!(text.len() <= cap && text.contains("bytes omitted"), "{cap}");
            } else {
                assert!(
                    text.starts_with("[turn output budget used up; page"),
                    "{cap}"
                );
            }
        }
        let short = fit_result("short".into(), None, "", 4096);
        assert_eq!(short, "short");
    }
}
