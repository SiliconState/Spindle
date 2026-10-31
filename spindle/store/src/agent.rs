//! Embeddable Spindle agent: every step is a committed revision, each model request
//! is sent at most once, bash runs in the strict sandbox over a versioned workspace,
//! and a run that is killed resumes. Credentials come from Dext's login store.
//! This module is the public Rust surface of `crate::API_VERSION` (docs/API.md).
use crate::artifact::Stage;
use crate::broker::{DispatchRequest, ModelReservation};
use crate::completion::Attempt;
use crate::rewind::RewindTargetKind;
use crate::workspace::{Entry, Tree};
use crate::{Error, Store};
use dext_core::model::{Encoding, Identity, ModelWait, PreparedRequest};
use dext_core::turn::{Phase, Stop};
use dext_native::model_effect::{AttemptError, Deadlines, LiveRoute, ModelTransport};
use dext_native::streaming::StreamUpdate;
use dext_native::strict::{RunOptions, StrictExecutor, Toolchain};
use dext_provider_catalog::provider::{self, ProviderProfile, RequestContract, RuntimeAuthKind};
use dext_transcript::checkpoint::NativeResponse;
#[cfg(test)]
use dext_transcript::checkpoint::TranscriptCheckpoint;
use dext_transcript::turn::{Effect, TurnContinuation};
use dext_transcript::{Block, Message};
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

pub use crate::driver::app_tools;
use crate::driver::model_error;
pub use crate::reads::{annotate_steering, steered_text};
pub use crate::shape::Route;
use crate::shape::*;
mod catalog;
mod cost;
mod host_tools;
pub mod runner;
pub mod tasks;
pub use crate::spend::{Billing, Call, CallKind, ModelUsage, TurnUsage, UsageReport};
pub use app_tools::{AppAnswer, AppCall, AppTool, AppTools, BuiltinTools};
pub use catalog::{context_window, model_catalog, valid_efforts};
pub use cost::{admit_nested_spend, annotate_snapshot, host_usage, recorded_usd, usage_report};
pub use host_tools::{
    SessionHost, annotate_display, display_of, drop_pending_settings, kind_of, pending_settings,
    queue_settings,
};

const SCOPE: &str = "operator";
pub use crate::driver::DROPPED;
const COMPACTED: &str = "[Earlier conversation compacted by spindle to stay within session limits]";
/// How long the one summary request of a model-written compaction may take (its text
/// and bounds are shared with the embedded host: `driver::summary_input`).
const SUMMARY_TIME: Duration = Duration::from_secs(600);

/// What a self-edit thread is told about how its changes reach the host, naming the
/// running binary's build and store schema. The serving process is the host, so its
/// own executable is the host's binary (fingerprinted once per process).
fn self_edit_guidance() -> String {
    let build = host_build();
    format!(
        "This is a self-edit thread: the workspace is Spindle's own source, and the host \
         serving this conversation runs build {build} (store schema {}). Your edits change \
         only this workspace. They reach the host only when the user installs them: call \
         spindle_install with a summary when they are ready; it raises the Spindle change \
         card at the end of the chat, and the user taps its Install (or types /install): \
         the host three-way merges this thread's changes with its current source, runs \
         formatting, Clippy, tests and a release build in isolation, then installs the \
         result and restarts itself. A binary you build here is a sandbox build, never the \
         host's binary, and you cannot restart, upgrade or reach the host: never tell the \
         user to restart the host or that your build is running; to deploy, ask them to \
         tap Install on the card. If an install reports that the host source changed the \
         same files as this thread, call spindle_source with action update, or the user \
         taps Update on the card (or types \"update from host source\"): it merges the \
         current host source into this workspace and asks you to resolve any conflicts. Install runs these gates on the merged source and stops at the first failure: \
         `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --all-features \
         --locked -- -D warnings` (every warning fails it, and the host source has none, so \
         any warning is yours to fix); `cargo test --release --locked --workspace \
         --all-features --lib --bins`; a release build; then `node store/tests/ui.mjs` and \
         `node store/tests/ui_app.mjs`. Run the same commands in this \
         workspace and get them all clean before spindle_install.",
        crate::CAPABILITIES.schema
    )
}

/// What every thread of a host that offers Spindle's source is told about changing
/// Spindle itself (`SessionHost::offers_source`).
fn source_guidance() -> String {
    format!(
        "You can change Spindle itself (the app serving this chat, build {}) from this \
         thread: spindle_source with action checkout copies its current source into \
         spindle/ (and gives your commands the Rust toolchain and the dext-engine sources); \
         edit and test it there, then call spindle_install with a summary, which raises the \
         Spindle change card in the chat. Nothing is installed until the user taps its \
         Install (or types /install); the host then merges your \
         changes with its current source, runs formatting, Clippy, tests and a release \
         build, installs and restarts itself. Builds you make are sandbox builds, never the \
         host's binary: never tell the user to restart the host. spindle_source with action \
         update brings in the host's newer source. Install runs these gates on the merged source and stops at the first failure: \
         `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --all-features \
         --locked -- -D warnings` (every warning fails it, and the host source has none, so \
         any warning is yours to fix); `cargo test --release --locked --workspace \
         --all-features --lib --bins`; a release build; then `node store/tests/ui.mjs` and \
         `node store/tests/ui_app.mjs`. Run the same commands in spindle/ and \
         get them all clean before spindle_install.",
        host_build()
    )
}

/// What a thread with an operator shelf is told: the shelf is context the operator
/// curates, read-only from here (`SessionHost::shelf`), mounted where commands with
/// the toolchain see it.
fn shelf_guidance() -> String {
    " The operator keeps cross-thread notes on a shelf, mounted read-only at /shelf in \
     your commands: things worth remembering across threads, in their words. List it \
     (ls /shelf) when a task starts and respect what it says; never edit or delete it. \
     To propose a change, write the note in the workspace and ask the operator to keep \
     it (the file viewer's Shelf button)."
        .into()
}

/// The operator's shelf when it holds notes (an empty or missing one is not offered).
fn shelf_of(host: Option<&dyn SessionHost>) -> Option<PathBuf> {
    host?.shelf().filter(|shelf| {
        shelf
            .read_dir()
            .is_ok_and(|mut entries| entries.any(|entry| entry.is_ok()))
    })
}

/// The serving binary's fingerprint (the serving process is the host).
fn host_build() -> &'static str {
    static BUILD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BUILD.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| crate::upgrade::fingerprint(&exe).ok())
            .unwrap_or_else(|| "unknown".into())
    })
}

use crate::driver;
pub use crate::driver::{Compaction, Effort, Event, Isolation, Settings, TaskPolicy};
use crate::driver::{File, Start, StartState};
mod pack_turn;

/// Result of an agent operation; the error is a message for a frontend.
pub type Result<T> = std::result::Result<T, String>;

/// Receives session events as they happen.
pub trait Observer: Send + Sync {
    /// Called once per event, in order.
    fn event(&self, event: Event<'_>);
    /// Where session packs' UI requests are shown and answered; None refuses them.
    fn pack_ui(&self) -> Option<Arc<dyn crate::pack_runtime::PackUi>> {
        None
    }
    /// The host serving the session, for the host tools that need one (self-edit,
    /// thread names); None: they say they are unavailable.
    fn host(&self) -> Option<Arc<dyn SessionHost>> {
        None
    }
    /// Where calls of the session's app tools (`Settings::tools`) are answered; None:
    /// no app is attached, and a turn that reaches one stops until an app continues it.
    fn app_tools(&self) -> Option<Arc<dyn AppTools>> {
        None
    }
}

/// Canonical read-only source directories for toolchain runs, mounted at /NAME.
pub fn sources(raw: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = vec![];
    for source in raw {
        let source = source
            .canonicalize()
            .map_err(|error| format!("--source {}: {error}", source.display()))?;
        let mount = dext_native::strict::source_mount(&source).ok_or(format!(
            "--source {}: the directory name cannot be mounted",
            source.display()
        ))?;
        if out
            .iter()
            .any(|old| dext_native::strict::source_mount(old).as_deref() == Some(mount.as_str()))
        {
            return Err(format!(
                "two --source directories would both mount at {mount}"
            ));
        }
        out.push(source);
    }
    Ok(out)
}

/// Whether this host has a Rust toolchain to offer sessions (`Settings::toolchain`).
pub fn toolchain_available() -> bool {
    host_toolchain().is_some()
}

/// Host rustup and cargo homes, if a toolchain is installed.
fn host_toolchain() -> Option<(PathBuf, PathBuf)> {
    let home = PathBuf::from(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?);
    let rustup = std::env::var_os("RUSTUP_HOME").map_or(home.join(".rustup"), PathBuf::from);
    let cargo = std::env::var_os("CARGO_HOME").map_or(home.join(".cargo"), PathBuf::from);
    let binary = if cfg!(windows) { "cargo.exe" } else { "cargo" };
    (rustup.join("toolchains").is_dir() && cargo.join("bin").join(binary).is_file())
        .then_some((rustup, cargo))
}

fn create_private_dir(dir: &Path) -> Result<()> {
    crate::durable::create_dir(dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))
}

fn read_json(path: &Path) -> Result<Value> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|_| format!("{} is not valid JSON", path.display()))
}

/// Host-owned provider configuration and live authentication. Secrets are resolved
/// only for a send, never retained in the driver's request-shaping facts.
pub trait Credentials: Send + Sync {
    fn route(&self, provider: &str, model: Option<&str>) -> Result<Route>;
    fn live_headers(&self, route: &Route, execution: &str) -> Result<HeaderMap>;
    fn auth_kind(&self, route: &Route) -> Result<RuntimeAuthKind>;
    fn identity(&self) -> Option<dext_provider_catalog::claude_subscription::ClaudeIdentity> {
        None
    }
    fn install_host(&self) {}
}

/// Credentials and provider catalog from the native Dext login store.
#[derive(Default)]
pub struct NativeCredentials;

impl Credentials for NativeCredentials {
    fn route(&self, provider: &str, model: Option<&str>) -> Result<Route> {
        native_route(provider, model)
    }
    fn live_headers(&self, route: &Route, execution: &str) -> Result<HeaderMap> {
        native_live_headers(route, execution)
    }
    fn auth_kind(&self, route: &Route) -> Result<RuntimeAuthKind> {
        provider::resolve_runtime_provider(Some(&route.profile.id), false)
            .map(|resolved| resolved.auth_kind)
            .map_err(|error| error.to_string())
    }
    fn identity(&self) -> Option<dext_provider_catalog::claude_subscription::ClaudeIdentity> {
        dext_provider_catalog::claude_subscription::discover_identity()
    }
    fn install_host(&self) {
        install_host();
    }
}

fn route(id: &str, model: Option<&str>) -> Result<Route> {
    NativeCredentials.route(id, model)
}

fn native_route(id: &str, model: Option<&str>) -> Result<Route> {
    let catalog = provider::load_provider_catalog().map_err(|error| error.to_string())?;
    let profile = provider::find_provider_profile(&catalog, id).ok_or_else(|| {
        let known: Vec<&str> = catalog.providers.iter().map(|p| p.id.as_str()).collect();
        format!("unknown provider `{id}`; known: {}", known.join(", "))
    })?;
    route_for(profile, model)
}

/// The route `profile` gives for `model` (its default when None) with Dext's
/// environment overrides of model and base URL applied.
fn route_for(profile: ProviderProfile, model: Option<&str>) -> Result<Route> {
    let model = match model {
        Some(model) => model.to_string(),
        None => provider::resolve_provider_model(&profile),
    };
    let base_url = provider::resolve_provider_base_url(&profile);
    route_with(profile, model, &base_url)
}

/// Fresh authentication headers for one send, through Dext's credential resolver
/// (env overrides, stored logins, OAuth refresh). Never stored, logged or printed.
fn live_headers(route: &Route, session: &str) -> Result<HeaderMap> {
    NativeCredentials.live_headers(route, session)
}

fn native_live_headers(route: &Route, session: &str) -> Result<HeaderMap> {
    let resolved = provider::resolve_runtime_provider(Some(&route.profile.id), true)
        .map_err(|error| error.to_string())?;
    let subscription = resolved.auth_kind == RuntimeAuthKind::OAuth
        && route.contract == RequestContract::AnthropicMessages;
    let mut headers = provider::provider_headers(
        route.contract,
        &resolved.api_key,
        provider::is_official_kimi_profile(&resolved.profile, &resolved.base_url),
        subscription,
        false,
        Some(&subscription_session(session)),
    )
    .map_err(|error| error.to_string())?;
    // `block_binding` in the body needs its beta (a subscription body carries none).
    if binds_thinking(route) && !subscription {
        let beta = match headers.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
            Some(betas) => format!("{betas},{THINKING_BINDING_BETA}"),
            None => THINKING_BINDING_BETA.to_string(),
        };
        headers.insert(
            "anthropic-beta",
            reqwest::header::HeaderValue::from_str(&beta).map_err(|e| e.to_string())?,
        );
    }
    Ok(headers)
}

/// Whether this route's login is an Anthropic subscription (Claude OAuth), whose
/// requests carry the Claude Code system blocks and metadata, as Dext sends them.
fn anthropic_subscription(route: &Route) -> bool {
    subscription_on(&NativeCredentials, route)
}

/// Whether a route sends through a subscription login now (ChatGPT, or a Claude login
/// these credentials hold): what the shared spend ledger asks the native host.
pub(crate) fn route_is_subscription(provider: &str, model: &str) -> bool {
    match route(provider, Some(model)) {
        Ok(route) => {
            route.contract == RequestContract::ChatGptResponses || anthropic_subscription(&route)
        }
        Err(_) => false,
    }
}

fn subscription_on(credentials: &dyn Credentials, route: &Route) -> bool {
    route.contract == RequestContract::AnthropicMessages
        && credentials
            .auth_kind(route)
            .is_ok_and(|kind| kind == RuntimeAuthKind::OAuth)
}

#[cfg(test)]
fn subscription_body(body: Value, first_prompt: &str, session: &str) -> Result<Vec<u8>> {
    subscription_body_with_identity(
        body,
        first_prompt,
        session,
        dext_provider_catalog::claude_subscription::discover_identity().as_ref(),
    )
}

/// Runs a `!command` credential reference from Dext's auth store, bounded like
/// Dext's own runner: no stdin, 5 seconds, 16 KiB of output.
fn secret_command(command: &str) -> Option<String> {
    use std::io::Read;
    use std::process::Stdio;
    let shell = dext_native::strict::host_program(dext_native::strict::Program::Bash);
    let mut child = match shell {
        Some(bash) => {
            let mut child = std::process::Command::new(bash);
            child.args(["--noprofile", "--norc", "-c", command]);
            child
        }
        None if cfg!(windows) => {
            let mut child = std::process::Command::new(dext_native::strict::system_shell());
            child.args(["/d", "/c", command]);
            child
        }
        None => return None,
    };
    let mut child = child
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let mut output = String::new();
    child
        .stdout
        .take()?
        .take(16 * 1024 + 1)
        .read_to_string(&mut output)
        .ok()?;
    let secret = output.trim();
    (status.success() && output.len() <= 16 * 1024 && !secret.is_empty())
        .then(|| secret.to_string())
}

fn no_launcher(
    _command: std::process::Command,
    label: &str,
    _timeout: Duration,
) -> std::result::Result<dext_provider_catalog::host::CommandOutput, String> {
    Err(format!(
        "{label} is not available in spindle; use `dext login`"
    ))
}

fn install_host() {
    dext_provider_catalog::host::install(secret_command, no_launcher);
}

/// Installs the host hooks Dext's provider code expects (credential `!commands`; no
/// process launching), for front ends that call it directly.
pub fn install_host_hooks() {
    install_host();
}

/// One provider's sign-in state, as `spindle login`/`dext auth` would report it.
#[non_exhaustive]
pub struct Login {
    /// Provider profile id.
    pub provider: String,
    /// Display name.
    pub name: String,
    /// Dext's one-line sign-in status.
    pub status: String,
    /// Whether usable credentials are stored or configured.
    pub signed_in: bool,
}

/// Every provider in Dext's catalog with its sign-in state.
pub fn logins() -> Result<Vec<Login>> {
    install_host();
    let catalog = provider::load_provider_catalog().map_err(|error| error.to_string())?;
    let store = provider::load_auth_store().map_err(|error| error.to_string())?;
    Ok(catalog
        .providers
        .iter()
        .map(|profile| Login {
            provider: profile.id.clone(),
            name: profile.display_name.clone(),
            status: provider::provider_auth_status(profile, &store),
            signed_in: provider::provider_has_available_credentials(profile, &store),
        })
        .collect())
}

pub use crate::spend::Tally;

/// The session's model usage and cost in total (parent replies, nested code-mode calls
/// and compaction summaries); `usage_report` has the breakdowns.
pub fn usage(dir: &Path) -> Result<Tally> {
    Ok(usage_report(dir)?.total)
}

/// Serializes usage.json updates in this process (snapshots, turns, nested-spend
/// checks and status read, extend and rewrite it).
fn usage_lock() -> std::sync::MutexGuard<'static, ()> {
    static USAGE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    USAGE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Writes a private file atomically and durably beside its final name.
pub(crate) fn save_private(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::durable::write(path, bytes).map_err(|error| error.to_string())
}

/// Signs in to `provider` the way `dext auth login` does, without a terminal: with a
/// key or token (or a pasted OAuth callback URL/code) it is stored; without one, the
/// result says where to sign in (OAuth: the URL to open, then paste the callback).
/// Returns the message and whether a credential is still awaited.
pub fn login(provider: &str, credential: Option<&str>) -> Result<(String, bool)> {
    install_host();
    let result = provider::login_provider(Some(provider), credential, false)
        .map_err(|error| format!("{error:#}"))?;
    Ok((result.message, result.awaiting_credentials))
}

/// `login` for a credential that arrived over the network (the web UI): only a
/// literal key or token is stored. Dext also accepts `!command` (run on the host to
/// produce the key) and environment variable names as stored credentials; from a
/// remote screen those would run host commands or disclose host variables.
pub fn login_remote(provider: &str, credential: Option<&str>) -> Result<(String, bool)> {
    if let Some(credential) = credential {
        let secret = provider::normalize_login_secret(credential)
            .unwrap_or_else(|| credential.trim().to_string());
        if secret.starts_with('!') || provider::is_env_var_token(&secret) {
            return Err(
                "paste the key itself; commands and environment variable names can only be \
                 used with `spindle login` on the host"
                    .into(),
            );
        }
    }
    login(provider, credential)
}

/// Removes the stored credential for `provider`, as `dext auth logout` does.
pub fn logout(provider: &str) -> Result<String> {
    install_host();
    provider::logout_provider(Some(provider)).map_err(|error| format!("{error:#}"))
}

/// Validate `effort` for a provider/model and describe it: (label, short label).
pub fn describe_effort(provider: &str, model: &str, effort: Effort) -> Result<(String, String)> {
    let route = route(provider, Some(model))?;
    reasoning(&route, effort).map(|r| (r.label, r.short))
}

impl Settings {
    /// The provider id and model `resolve` would choose, from the catalog alone (no
    /// credential lookup, which may run a command or refresh a token).
    pub fn route_of(provider: &str, model: Option<&str>) -> Result<(String, String)> {
        let route = route(provider, model)?;
        Ok((route.profile.id, route.model))
    }

    /// Resolve a provider and model with fresh credentials from Dext's login store;
    /// the other settings start at their defaults.
    pub fn resolve(provider: &str, model: Option<String>) -> Result<Self> {
        install_host();
        let route = route(provider, model.as_deref())?;
        live_headers(&route, "check")?;
        Ok(Self {
            provider: route.profile.id,
            model: route.model,
            network: false,
            toolchain: false,
            sources: vec![],
            isolation: Isolation::Auto,
            effort: Effort::Default,
            compaction: Compaction::default(),
            max_usd: None,
            self_edit: false,
            tools: vec![],
            builtin_tools: BuiltinTools::All,
            instructions: None,
            resend_unknown: false,
            tasks: false,
            task_policy: Default::default(),
            runner: Default::default(),
        })
    }
    /// Read the settings saved in a session directory.
    pub fn load(dir: &Path) -> Result<Self> {
        let value = match crate::meta::read(dir, "settings").map_err(store_error)? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| "stored settings are not JSON".to_string()),
            None => read_json(&dir.join("session.json")),
        }
        .map_err(|_| {
            format!(
                "no session in {}; start one with `spindle new`",
                dir.display()
            )
        })?;
        Self::from_json(&value)
    }

    /// Save settings in metadata and mirror them beside the native session store.
    pub fn save(&self, dir: &Path) -> Result<()> {
        let bytes =
            serde_json::to_vec_pretty(&self.to_json()?).map_err(|error| error.to_string())?;
        crate::meta::update(dir, "settings", |_| Ok(bytes.clone())).map_err(store_error)?;
        crate::durable::write(&dir.join("session.json"), &bytes).map_err(|error| error.to_string())
    }
}

/// A user's request: an all-text user message that is neither a compaction digest nor
/// a message sent while the turn ran (steering).
pub(crate) fn is_prompt(message: &Message) -> bool {
    message.role == "user"
        && !message.content.is_empty()
        && message.content.iter().all(|block| {
            matches!(block, Block::Text { text }
                if !text.starts_with(COMPACTED) && !text.starts_with(STEERED) && !text.starts_with("[task "))
        })
}

const BAD_FILES: &str = "the files do not form a valid workspace (paths are relative, without \
     empty, `.` or `..` parts; a file cannot sit where a file or link already is)";

/// `tree` with `files` added (a path already there, as a file, link or empty directory,
/// is replaced), each file under its object digest.
fn planned_tree(mut tree: Tree, files: &[File]) -> Tree {
    for (path, bytes, executable) in files {
        let inside = |directory: &str| path.starts_with(&format!("{directory}/"));
        tree.files.retain(|entry| entry.path != *path);
        tree.links.retain(|link| link.path != *path);
        tree.directories
            .retain(|directory| directory != path && !inside(directory));
        tree.files.push(Entry {
            path: path.clone(),
            object: crate::artifact::object_digest(bytes),
            executable: *executable,
        });
    }
    tree.files.sort_by(|a, b| a.path.cmp(&b.path));
    tree
}

/// Longest message the turn state accepts as steering.
pub const MAX_STEER: usize = 64 * 1024;

/// How a steering message starts in history (the engine's shared wrapper).
const STEERED: &str = "[queued-user-update]";

struct ReplyWatch {
    cut: AtomicBool,
    point: AtomicU64,
    done: AtomicBool,
    refused: AtomicBool,
    interrupt: Arc<AtomicBool>,
    hurry: Arc<AtomicBool>,
    steering: Arc<std::sync::Mutex<Vec<String>>>,
    redo: Option<u64>,
    fresh: bool,
    observer: Arc<dyn Observer>,
}

impl ReplyWatch {
    fn poll(&self) -> bool {
        if self.interrupt.load(Ordering::SeqCst) {
            self.cut.store(true, Ordering::SeqCst);
            return true;
        }
        if self.fresh
            && !self.refused.load(Ordering::SeqCst)
            && self.hurry.load(Ordering::SeqCst)
            && !self
                .steering
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        {
            if let Some(point) = self.redo {
                self.point.store(point, Ordering::SeqCst);
                self.cut.store(true, Ordering::SeqCst);
                return true;
            }
            if !self.refused.swap(true, Ordering::SeqCst) {
                self.observer.event(Event::ReplyNotCut);
            }
        }
        false
    }

    async fn run(&self) {
        while !self.done.load(Ordering::SeqCst) {
            if self.poll() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn redo_point(store: &Store) -> Option<u64> {
    let mut view = crate::view::ReadView::from_store(store);
    let steps = view.recent_steps(32).ok()?;
    let before = steps.get(steps.len().checked_sub(2)?)?.revision;
    matches!(view.targets(&[before]).as_slice(), [(revision, _)] if *revision == before)
        .then_some(before)
}

/// Catalog models that accept images, for refusals that should say what would work.
pub(crate) fn image_models() -> Vec<String> {
    let Ok(catalog) = provider::load_provider_catalog() else {
        return vec![];
    };
    let mut found = vec![];
    for profile in &catalog.providers {
        let mut models = profile.models.clone();
        if !models.contains(&profile.default_model) {
            models.push(profile.default_model.clone());
        }
        for model in models {
            if provider::resolve_model_spec(profile, &model).image_input {
                found.push(format!("{}/{model}", profile.id));
            }
        }
    }
    found
}

pub(crate) fn execute_code_model_with_credentials(
    prepared: &Value,
    interrupt: &AtomicBool,
    remaining: Duration,
    credentials: &dyn Credentials,
) -> Value {
    // Failures before the request leaves the host are certain: nothing was sent.
    let sent = AtomicBool::new(false);
    // A provider refusal (HTTP status) is a known outcome: nothing was billed.
    let refused = std::sync::atomic::AtomicU16::new(0);
    let run = || -> Result<Value> {
        if interrupt.load(Ordering::SeqCst) || remaining.is_zero() {
            return Err("nested call stopped".into());
        }
        let route = credentials.route(
            prepared["provider"].as_str().ok_or("provider required")?,
            prepared["model"].as_str(),
        )?;
        if prepared["endpoint"] != route.endpoint || prepared["contract"] != route.contract.as_str()
        {
            return Err("nested route changed; operator review required".into());
        }
        let session = prepared["session"].as_str().ok_or("session required")?;
        let subscription = subscription_on(credentials, &route);
        let body = if subscription {
            let prompt = prepared["body"]["messages"][0]["content"]
                .as_array()
                .and_then(|parts| parts.iter().rev().find_map(|part| part["text"].as_str()))
                .unwrap_or_default()
                .to_string();
            subscription_body_with_identity(
                prepared["body"].clone(),
                &prompt,
                session,
                credentials.identity().as_ref(),
            )?
        } else {
            serde_json::to_vec(&prepared["body"]).map_err(|e| e.to_string())?
        };
        let request = PreparedRequest::new(
            Identity {
                effect: format!("code/{session}"),
                provider: route.profile.id.clone(),
                contract: route.contract.as_str().into(),
                endpoint: route.endpoint.clone(),
                model: route.model.clone(),
                credential_scope: format!("dext-login/{}", route.profile.id),
                configuration: "spindle-code-model-v1".into(),
            },
            Encoding::Json,
            body,
        )
        .map_err(|e| e.to_string())?;
        let waiting = ModelWait::new(request).start().map_err(|e| e.to_string())?;
        let live = LiveRoute::new(
            waiting.request().identity().clone(),
            credentials.live_headers(&route, session)?,
        )
        .map_err(|e| e.to_string())?;
        // The transport bounds one model call at 600 s, whatever the script's timeout.
        let total = remaining.min(Duration::from_secs(600));
        let transport = ModelTransport::new(Deadlines {
            connect: total.min(Duration::from_secs(15)),
            first_byte: total,
            idle: total,
            total,
        })
        .map_err(|e| e.to_string())?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        sent.store(true, Ordering::SeqCst);
        let recorded = runtime
            .block_on(transport.send_once(&waiting, live, interrupt))
            .map_err(|error| {
                if let AttemptError::Http(status) = &error {
                    refused.store(*status, Ordering::SeqCst);
                }
                provider_error(error)
            })?;
        let response =
            NativeResponse::restore(recorded.response().ok_or("missing child response")?)
                .map_err(|e| e.to_string())?;
        if response
            .blocks
            .iter()
            .any(|b| matches!(b, Block::ToolUse { .. }))
        {
            return Err("nested model returned an unexpected tool call".into());
        }
        // `subscription`: a login with no marginal cost (usage accounting).
        Ok(
            json!({"text":text_of(&response.blocks),"usage":response.usage,"provider":route.profile.id,"model":route.model,
                "subscription":subscription || route.contract == RequestContract::ChatGptResponses}),
        )
    };
    match run() {
        Ok(data) => json!({"ok":true,"data":data}),
        Err(error) => {
            let mut failure =
                json!({"ok":false,"error":error,"may_have_run":sent.load(Ordering::SeqCst)});
            let status = refused.load(Ordering::SeqCst);
            if status != 0 {
                failure["status"] = json!(status);
            }
            failure
        }
    }
}

/// The store refuses a root that group or others can access; copies made with a
/// default umask hit this, so say which directory needs which mode.
fn store_open_error(store: &Path, error: Error) -> String {
    if matches!(error, Error::Locked)
        && let Some(note) = crate::fence_note(store)
    {
        if let Some(moved) = note.strip_prefix(crate::MOVED_FENCE) {
            return format!(
                "this session moved elsewhere ({moved}); it continues where its bundle was \
                 imported. To use this copy as the session instead, remove {}",
                store.join(crate::FENCE).display()
            );
        }
        if note.starts_with("spindle-embed: ") {
            return format!(
                "an app's embedded Spindle holds this session ({note}); close it there first, \
                 or, if that app is gone, remove {}",
                store.join(crate::FENCE).display()
            );
        }
        if !note.starts_with(crate::NATIVE_FENCE) {
            return format!(
                "{} fences this session ({}); if no host is running it, remove that file",
                store.join(crate::FENCE).display(),
                if note.is_empty() {
                    "empty"
                } else {
                    note.as_str()
                }
            );
        }
    }
    #[cfg(unix)]
    if matches!(error, Error::Invalid) {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::symlink_metadata(store) {
            let mode = meta.permissions().mode() & 0o777;
            if meta.is_dir() && mode & 0o077 != 0 {
                return format!(
                    "{} must be private to its owner (mode 0700, found {mode:04o}); run `chmod 700` on it",
                    store.display()
                );
            }
        }
    }
    store_error(error)
}

/// Another owner, or a connection briefly holding SQLite's lock, has the store.
const LOCKED: &str = "another spindle process or connection is using this session's store; \
wait for it to finish, then run the command again";

/// A full disk or quota. Nothing saved is lost: the step that failed is repeated (or
/// found saved after all) once there is room.
const DISK_FULL: &str = "the disk is full, so the session could not save its next step; \
free some space, then run `spindle continue` (it resumes from the last saved step)";

fn store_error(error: Error) -> String {
    match error {
        Error::Locked => LOCKED.into(),
        error if error.is_disk_full() => DISK_FULL.into(),
        Error::Sql(rusqlite::Error::SqliteFailure(failure, _))
            if failure.code == rusqlite::ErrorCode::SystemIoFailure =>
        {
            "the disk reported an error while the session was saving a step (SQLite I/O error); \
             check the disk and free space, then run `spindle continue`"
                .into()
        }
        Error::Paused => {
            "the session's store is paused (an earlier storage error, or a rewind still being \
             finished); run the command again"
                .into()
        }
        Error::Model(error) => model_error(error),
        Error::Exhausted => {
            "a store capacity limit was reached (revisions or stored files); `spindle doctor` shows which".into()
        }
        error @ Error::TooLarge { .. } => error.to_string(),
        other => format!("store refused the step ({other:?})"),
    }
}

fn provider_error(error: AttemptError) -> String {
    model_error(error.into())
}

fn turn_error(error: dext_transcript::continuation::Error) -> String {
    format!("turn state rejected the step ({error:?})")
}

/// Whether a tool runs a process in the sandbox (and can stream output).
pub fn runs_process(name: &str) -> bool {
    matches!(name, "bash" | "run_code" | "start_job")
}

/// One terminal line for a tool call, also used by compaction digests.
pub fn describe_call(name: &str, input: &Value) -> String {
    let path = input["path"].as_str().unwrap_or("?");
    let line = match name {
        "bash" => format!(
            "$ {}",
            input["command"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| input.to_string())
        ),
        "read_file" => match input["offset"].as_u64() {
            Some(offset) => format!("read {path} from line {offset}"),
            None => format!("read {path}"),
        },
        "write_file" => format!(
            "write {path} ({} bytes)",
            input["content"].as_str().map(str::len).unwrap_or_default()
        ),
        "edit_file" => format!("edit {path}"),
        "start_job" => format!(
            "& {}",
            input["command"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| input.to_string())
        ),
        "read_job" => match (input["job"].as_str(), input["wait"].as_u64()) {
            (None, _) => "list background jobs".into(),
            (Some(job), Some(wait)) if wait > 0 => format!("read {job} (wait up to {wait} s)"),
            (Some(job), _) => format!("read {job}"),
        },
        "stop_job" => format!("stop {}", input["job"].as_str().unwrap_or("?")),
        "show" => format!(
            "show {}",
            input["paths"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        "settings" if input.as_object().is_none_or(|fields| fields.is_empty()) => {
            "read settings".into()
        }
        "spindle_source" => format!(
            "spindle source: {}",
            input["action"].as_str().unwrap_or("?")
        ),
        "spindle_install" => format!(
            "request install: {}",
            input["summary"].as_str().unwrap_or_default()
        ),
        "run_code" => format!(
            "{} script:\n{}",
            input["language"].as_str().unwrap_or("?"),
            input["code"].as_str().unwrap_or_default()
        ),
        other => format!("{other} {input}"),
    };
    let mut lines = line.lines();
    let first: String = lines.next().unwrap_or_default().chars().take(300).collect();
    match lines.count() {
        0 => first,
        more => format!("{first}\n  ... ({more} more lines)"),
    }
}

/// Refuses an export while a background task of the session in `dir` still runs: it
/// would go on writing to this copy (and its answer to it) after the session moved.
fn refuse_running_tasks(dir: &Path, store: &Store) -> Result<()> {
    for run in store.task_runs().map_err(store_error)? {
        if run.queued && run.state == "running" && tasks::active(dir, &run.task) {
            return Err(format!(
                "background task {} is still running; wait for it (`task_wait`) or stop it \
                 (`task_stop`), then export",
                run.task.strip_prefix("app:").unwrap_or(&run.task)
            ));
        }
    }
    Ok(())
}

/// Adds the task sessions under `dir` (`tasks/<key>/`, at bundle path `prefix`) to a
/// bundle's `files`: each store opened (so no other owner runs it meanwhile), imaged,
/// kept open in `opened`, with its sidecars.
fn bundle_tasks(
    dir: &Path,
    prefix: &str,
    files: &mut Vec<crate::bundle::BundleFile>,
    opened: &mut Vec<Store>,
) -> Result<()> {
    use crate::bundle::{BundleFile, STORE};
    // As the native host lists them: real directories only, never a link out.
    for place in tasks::task_dirs(dir)? {
        let Some(key) = place
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|key| crate::bundle::valid_path(&format!("tasks/{key}/session.json")))
            .map(String::from)
        else {
            continue;
        };
        let store_dir = place.join("store");
        if !std::fs::symlink_metadata(&store_dir).is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
        let store = Store::open(&store_dir).map_err(|error| match error {
            // A native owner's fence (or none): some host is running the task now.
            Error::Locked
                if crate::fence_note(&store_dir)
                    .is_none_or(|note| note.starts_with(crate::NATIVE_FENCE)) =>
            {
                format!(
                    "task {key} is in use (a background task, or another process); wait for \
                     it, then export again"
                )
            }
            other => format!("task {key}: {}", store_open_error(&store_dir, other)),
        })?;
        refuse_running_tasks(&place, &store)?;
        let at = format!("{prefix}tasks/{key}/");
        files.push(BundleFile {
            path: format!("{at}{STORE}"),
            bytes: store.image().map_err(store_error)?,
        });
        for name in ["session.json", "task.json"] {
            if let Ok(bytes) = std::fs::read(place.join(name)) {
                files.push(BundleFile {
                    path: format!("{at}{name}"),
                    bytes,
                });
            }
        }
        opened.push(store);
        bundle_tasks(&place, &at, files, opened)?;
    }
    Ok(())
}

/// Imports the session bundle at `source` into `dir` (absent, or an empty directory):
/// see `import_bundle`.
pub fn import_session(source: &Path, dir: &Path) -> Result<Value> {
    let bytes = std::fs::read(source)
        .map_err(|error| format!("cannot read {}: {error}", source.display()))?;
    let bundle = crate::bundle::read(&bytes)?;
    import_bundle(&bundle, dir)
}

/// Writes a checked bundle as the session directory `dir` (absent, or an empty
/// directory): every file goes into a private staging directory beside it, synced,
/// then the staging directory is renamed into place, so `dir` holds the whole session
/// or nothing. The session keeps its identity: it is the same session, moved.
pub fn import_bundle(bundle: &crate::bundle::Bundle, dir: &Path) -> Result<Value> {
    use crate::bundle::STORE;
    let empty = match std::fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(format!(
                    "{} is not empty; import a session into a new directory",
                    dir.display()
                ));
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(format!("cannot read {}: {error}", dir.display())),
    };
    let parent = dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = dir
        .file_name()
        .ok_or_else(|| format!("{} names no directory", dir.display()))?
        .to_string_lossy();
    let stage = parent.join(format!(".{name}.importing-{}", crate::process_id()));
    let _ = std::fs::remove_dir_all(&stage);
    let staged = (|| -> std::io::Result<()> {
        // Private, as a native host keeps its session directories.
        crate::durable::create_dir(&stage)?;
        let mut directories = vec![stage.clone()];
        for file in &bundle.files {
            let path = stage.join(&file.path);
            let folder = path.parent().expect("a bundle path has a directory");
            crate::durable::create_dir(folder)?;
            directories.push(folder.to_path_buf());
            crate::durable::write(&path, &file.bytes)?;
            if file.path.ends_with(STORE) {
                let mut lock = std::fs::OpenOptions::new();
                lock.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    lock.mode(0o600);
                }
                lock.open(folder.join("owner.lock"))?.sync_all()?;
            }
        }
        for directory in directories.iter().rev() {
            crate::durable::sync_dir(directory)?;
        }
        if empty {
            std::fs::remove_dir(dir)?;
        }
        std::fs::rename(&stage, dir)?;
        crate::durable::sync_dir(parent)
    })();
    if let Err(error) = staged {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(format!("cannot import into {}: {error}", dir.display()));
    }
    // A native host opens it as one of its own sessions (checking it again).
    let mut store = Store::open(&dir.join("store"))
        .map_err(|error| store_open_error(&dir.join("store"), error))?;
    let head = store.snapshot().map_err(store_error)?.revision;
    drop(store);
    Ok(json!({
        "session": dir.to_string_lossy(),
        "head": head,
        "identity": bundle.identity,
        "files": bundle.files.len(),
    }))
}

/// Import a host directory as initial workspace files; returns the files and skipped count.
/// Refused, before more is read, past the workspace's file count or total size.
#[allow(clippy::type_complexity)]
pub fn import_files(source: &Path) -> Result<(Vec<(String, Vec<u8>, bool)>, usize)> {
    // Any name the workspace keeps (UTF-8, `.git` included); build output stays out.
    fn valid(part: &str) -> bool {
        !part.is_empty()
            && part.len() <= crate::workspace::MAX_NAME
            && part != "."
            && part != ".."
            && !part.contains('\0')
    }
    fn walk(
        root: &Path,
        dir: &Path,
        out: &mut Vec<(String, Vec<u8>, bool)>,
        skipped: &mut usize,
        total: &mut u64,
    ) -> Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .map_err(|error| format!("cannot read {}: {error}", dir.display()))?
            .filter_map(|entry| entry.ok())
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                *skipped += 1;
                continue;
            };
            let kind = entry.file_type().map_err(|error| error.to_string())?;
            if !valid(&name) || ["target", "node_modules"].contains(&name.as_str()) {
                *skipped += 1;
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                walk(root, &path, out, skipped, total)?;
            } else if kind.is_file() {
                let meta = entry.metadata().map_err(|error| error.to_string())?;
                if meta.len() > dext_native::strict::MAX_FILE as u64 {
                    *skipped += 1;
                    continue;
                }
                let relative = path
                    .strip_prefix(root)
                    .map_err(|error| error.to_string())?
                    .to_string_lossy()
                    .into_owned();
                #[cfg(windows)]
                let relative = relative.replace('\\', "/");
                if !crate::workspace::valid_path(&relative) {
                    *skipped += 1;
                    continue;
                }
                *total += meta.len();
                if out.len() >= dext_native::strict::MAX_FILES
                    || *total > dext_native::strict::MAX_TREE as u64
                {
                    return Err(format!(
                        "{} holds more than the workspace's {} files or {} GiB",
                        root.display(),
                        dext_native::strict::MAX_FILES,
                        dext_native::strict::MAX_TREE >> 30
                    ));
                }
                let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
                #[cfg(unix)]
                let executable = {
                    use std::os::unix::fs::PermissionsExt;
                    meta.permissions().mode() & 0o111 != 0
                };
                #[cfg(not(unix))]
                let executable = false;
                out.push((relative, bytes, executable));
            } else {
                *skipped += 1;
            }
        }
        Ok(())
    }
    let mut files = vec![];
    let mut skipped = 0;
    walk(source, source, &mut files, &mut skipped, &mut 0)?;
    Ok((files, skipped))
}

/// Providers from Dext's catalog with their readiness and default model.
#[allow(clippy::type_complexity)]
pub fn providers() -> Result<Vec<(String, Result<(String, String)>)>> {
    install_host();
    let catalog = provider::load_provider_catalog().map_err(|error| error.to_string())?;
    Ok(catalog
        .providers
        .iter()
        .map(|profile| {
            let state = match route(&profile.id, None)
                .and_then(|route| live_headers(&route, "check").map(|_| route))
            {
                Ok(route) => Ok((route.contract.as_str().to_string(), route.model)),
                Err(why) => Err(why),
            };
            (profile.id.clone(), state)
        })
        .collect())
}

/// One `doctor` finding: what was checked, whether it is healthy, and detail or the
/// repair that was made.
#[non_exhaustive]
pub struct Finding {
    /// What was checked, such as `store` or `sandbox`.
    pub check: &'static str,
    /// Whether it is healthy.
    pub ok: bool,
    /// Healthy but worth knowing, such as running without isolation.
    pub warning: bool,
    /// What was found, or the repair that was made.
    pub detail: String,
}

/// Checks a session and its host, repairing only what is safe to repair: abandoned
/// sandbox scratch is reclaimed, a missing build cache is recreated, and stored
/// files no revision, receipt or pending step references are collected. No step is
/// changed; unresolved steps are reported with the command that resolves them.
pub fn doctor(dir: &Path) -> Vec<Finding> {
    install_host();
    let findings = std::cell::RefCell::new(vec![]);
    let push = |check, ok, warning, detail: String| {
        findings.borrow_mut().push(Finding {
            check,
            ok,
            warning,
            detail,
        })
    };
    let add = |check, ok, detail: String| push(check, ok, false, detail);
    let warn = |check, detail: String| push(check, true, true, detail);
    let settings = Settings::load(dir);
    match &settings {
        Ok(settings) => add(
            "settings",
            true,
            format!("{} / {}", settings.provider, settings.model),
        ),
        Err(error) => add("settings", false, error.clone()),
    }
    match Store::open(&dir.join("store")) {
        Ok(mut store) => {
            match store.check_integrity() {
                Ok(()) => add(
                    "store",
                    true,
                    format!(
                        "schema {}; every page and record checked",
                        crate::CAPABILITIES.schema
                    ),
                ),
                Err(error) => add(
                    "store",
                    false,
                    format!("the full check failed: {}", store_error(error)),
                ),
            }
            let backups: Vec<String> = std::fs::read_dir(dir.join("store"))
                .map(|entries| {
                    entries
                        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                        .filter(|name| name.starts_with("authority.v") && name.ends_with(".sqlite"))
                        .collect()
                })
                .unwrap_or_default();
            if !backups.is_empty() {
                add(
                    "backup",
                    true,
                    format!(
                        "store/{} kept from before a schema migration, for older builds; delete once you will not roll back",
                        backups.join(", store/")
                    ),
                );
            }
            match store.reconciliation() {
                Ok(report) if report.unresolved_attempts.is_empty() => {
                    add("steps", true, "no step with an unknown outcome".into())
                }
                // Unknown outcomes are thread state, not store damage: Retry/Rewind
                // resolve a model request and Continue seals an interrupted command
                // without re-running it. Warnings keep upgrade verification usable.
                Ok(report)
                    if report
                        .unresolved_attempts
                        .iter()
                        .all(|id| is_model_effect(id)) =>
                {
                    warn(
                        "steps",
                        format!(
                            "{} model request(s) have an unknown outcome; `spindle retry` re-runs from the step before, or rewind (`spindle targets`)",
                            report.unresolved_attempts.len()
                        ),
                    )
                }
                Ok(report) => warn(
                    "steps",
                    format!(
                        "{} step(s) were interrupted; `spindle continue` records an interrupted command as not completed (never re-run); `spindle retry` re-runs a model call",
                        report.unresolved_attempts.len()
                    ),
                ),
                Err(error) => add("steps", false, store_error(error)),
            }
            match store.collect_garbage() {
                Ok(freed) if freed.stages + freed.objects > 0 => add(
                    "garbage",
                    true,
                    format!(
                        "reclaimed {} unreachable stage(s) and {} object(s) ({})",
                        freed.stages,
                        freed.objects,
                        crate::human_size(freed.bytes as usize)
                    ),
                ),
                Ok(_) => add("garbage", true, "nothing unreachable to reclaim".into()),
                Err(error) => add("garbage", false, store_error(error)),
            }
            match store.capacity() {
                Ok(usage) => {
                    let full: Vec<String> = usage
                        .iter()
                        .filter(|(_, used, cap)| used * 10 >= cap * 9)
                        .map(|(name, used, cap)| format!("{name} {used}/{cap}"))
                        .collect();
                    if full.is_empty() {
                        add("capacity", true, "below 90% of every store limit".into());
                    } else if usage.iter().any(|(_, used, cap)| used >= cap) {
                        add(
                            "capacity",
                            false,
                            format!(
                                "store limit reached ({}); start a new thread to keep working",
                                full.join(", ")
                            ),
                        );
                    } else {
                        warn(
                            "capacity",
                            format!("near a store limit: {}", full.join(", ")),
                        );
                    }
                }
                Err(error) => add("capacity", false, store_error(error)),
            }
        }
        Err(Error::Incompatible) => add(
            "store",
            false,
            format!(
                "written by a different spindle schema (this build reads schema {}); use the matching spindle or `spindle rollback`",
                crate::CAPABILITIES.schema
            ),
        ),
        // Also says where a moved session went, or that an app's package holds it.
        Err(error) => add("store", false, store_open_error(&dir.join("store"), error)),
    }
    let mode = settings
        .as_ref()
        .map(|settings| settings.isolation)
        .unwrap_or_default();
    match (mode, open_isolated()) {
        (Isolation::Off, _) => warn(
            "isolation",
            "off for this session: commands run directly on the host".into(),
        ),
        (_, Ok(_)) => add("isolation", true, "sandboxed (bubblewrap)".into()),
        (Isolation::Required, Err(why)) => add(
            "isolation",
            false,
            format!("required but unavailable: {why}"),
        ),
        (Isolation::Auto, Err(why)) => warn(
            "isolation",
            format!("{why}; commands run directly on the host without isolation"),
        ),
    }
    match dext_native::strict::sweep_abandoned_candidates() {
        Ok(0) => add("scratch", true, "no abandoned sandbox scratch".into()),
        Ok(count) => add(
            "scratch",
            true,
            format!("reclaimed {count} abandoned sandbox scratch dir(s)"),
        ),
        Err(error) => add(
            "scratch",
            false,
            format!("cannot sweep sandbox scratch: {error}"),
        ),
    }
    if let Ok(settings) = &settings {
        match route(&settings.provider, Some(&settings.model))
            .and_then(|route| live_headers(&route, "check"))
        {
            Ok(_) => add(
                "credentials",
                true,
                format!("`{}` is ready", settings.provider),
            ),
            Err(error) => add("credentials", false, error),
        }
        if settings.toolchain {
            match host_toolchain() {
                Some(_) => {
                    let cache = dir.join("cache");
                    let repaired = !cache.is_dir();
                    match create_private_dir(&cache) {
                        Ok(()) if repaired => {
                            add("toolchain", true, "recreated the build cache".into())
                        }
                        Ok(()) => add(
                            "toolchain",
                            true,
                            "host Rust toolchain and build cache ready".into(),
                        ),
                        Err(error) => add("toolchain", false, error),
                    }
                }
                None => add(
                    "toolchain",
                    false,
                    "no host Rust toolchain (~/.rustup and ~/.cargo)".into(),
                ),
            }
        }
        for source in &settings.sources {
            if !source.is_dir() {
                add(
                    "sources",
                    false,
                    format!(
                        "{} no longer exists; set sources again with `say --source`",
                        source.display()
                    ),
                );
            }
        }
    }
    findings.into_inner()
}

/// The bubblewrap runner, pinned to the installed helper's bytes.
fn open_bwrap() -> std::result::Result<StrictExecutor, String> {
    if !cfg!(target_os = "linux") {
        return Err("namespace isolation needs Linux".into());
    }
    let helper = Path::new("/usr/bin/bwrap");
    let bytes = std::fs::read(helper).map_err(|_| "bubblewrap is not installed".to_string())?;
    StrictExecutor::open(helper, Sha256::digest(bytes).into())
        .map_err(|error| format!("bubblewrap is unusable: {error}"))
}

/// Whether this host can run commands in its sandbox (bubblewrap, proven once).
pub fn sandbox_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| open_isolated().is_ok())
}

/// The bubblewrap runner, proven to work on this host by a trivial run.
fn open_isolated() -> std::result::Result<StrictExecutor, String> {
    let runner = open_bwrap()?;
    runner
        .probe()
        .map_err(|_| "this host does not allow the namespaces bubblewrap needs".to_string())?;
    Ok(runner)
}

/// Snapshot of where a session stands.
#[non_exhaustive]
pub struct Status {
    /// Session directory.
    pub session: PathBuf,
    /// Provider profile id.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Current revision.
    pub revision: u64,
    /// Owner epoch.
    pub epoch: u64,
    /// Prompts in the current history.
    pub prompts: usize,
    /// Human-readable turn phase.
    pub state: String,
    /// Workspace file count.
    pub files: usize,
    /// Steps with an unknown outcome.
    pub unresolved: usize,
    /// Requests abandoned by rewinds.
    pub abandoned: usize,
    /// Requests the provider refused before generating anything (known outcomes).
    pub rejected: usize,
    /// The isolation setting for commands.
    pub isolation: Isolation,
    /// Requested reasoning effort.
    pub effort: Effort,
}

/// A point a session can rewind to.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Target {
    /// Revision to pass to `rewind`.
    pub revision: u64,
    /// What the target restores.
    pub kind: RewindTargetKind,
    /// Last message text, flattened for display.
    pub last: String,
}

/// A durable agent session in one directory.
pub struct Session {
    dir: PathBuf,
    store: Store,
    /// The sandbox runner, opened on first use (or why it is unavailable).
    isolated: Option<std::result::Result<Arc<StrictExecutor>, String>>,
    host: Arc<StrictExecutor>,
    told_unisolated: bool,
    transport: ModelTransport,
    runtime: tokio::runtime::Runtime,
    interrupt: Arc<AtomicBool>,
    observer: Arc<dyn Observer>,
    credentials: Arc<dyn Credentials>,
    initial_files: Option<Vec<(String, Vec<u8>, bool)>>,
    driver: driver::Driver,
    /// Messages sent while a turn runs, injected at its next tool boundary, before its
    /// final answer or before its next request (`steering_handle`).
    steering: Arc<std::sync::Mutex<Vec<String>>>,
    /// Set when the user asks for the queued messages to be read now (`hurry_handle`).
    hurry: Arc<AtomicBool>,
    /// App state changes sent while a turn runs (`state_handle`).
    state_queue: Arc<std::sync::Mutex<Vec<Value>>>,
    /// What the next turn's first step does to the app state; taken by that start (a
    /// prompt call clears it whatever happens), so nothing is left for a later one.
    start_state: Option<StartState>,
}

impl Session {
    /// Open an existing session and send its events to `observer`.
    pub fn open(dir: &Path, observer: Arc<dyn Observer>) -> Result<Self> {
        Self::open_with_credentials(dir, observer, Arc::new(NativeCredentials))
    }

    /// Opens a session with the embedding host's provider and credential resolver.
    pub fn open_with_credentials(
        dir: &Path,
        observer: Arc<dyn Observer>,
        credentials: Arc<dyn Credentials>,
    ) -> Result<Self> {
        credentials.install_host();
        let mut store = Store::open(&dir.join("store")).map_err(|error| match error {
            Error::Io(_) | Error::Invalid if !dir.join("store").exists() => format!(
                "no session in {}; start one with `spindle new`",
                dir.display()
            ),
            Error::Incompatible => format!(
                "{} was written by a different spindle schema (this build reads schema {}); use the matching spindle, `spindle rollback`, or `spindle doctor`",
                dir.display(),
                crate::CAPABILITIES.schema
            ),
            other => store_open_error(&dir.join("store"), other),
        })?;
        crate::meta::seed(&mut store, dir).map_err(store_error)?;
        let transport = ModelTransport::new(Deadlines {
            connect: Duration::from_secs(15),
            first_byte: Duration::from_secs(180),
            idle: Duration::from_secs(180),
            total: Duration::from_secs(600),
        })
        .map_err(|error| error.to_string())?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let interrupt = Arc::new(AtomicBool::new(false));
        let steering: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let hurry: Arc<AtomicBool> = Arc::default();
        let state_queue: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
        let env = Arc::new(NativeEnv {
            dir: dir.into(),
            observer: observer.clone(),
            credentials: credentials.clone(),
        });
        let inbox = Arc::new(NativeInbox {
            interrupt: interrupt.clone(),
            steering: steering.clone(),
            hurry: hurry.clone(),
            state_queue: state_queue.clone(),
        });
        let driver = driver::Driver::new(env.clone(), inbox, env);
        let mut session = Self {
            dir: dir.into(),
            store,
            isolated: None,
            host: Arc::new(StrictExecutor::host()),
            told_unisolated: false,
            transport,
            runtime,
            interrupt,
            driver,
            steering,
            hurry,
            state_queue,
            start_state: None,
            observer,
            credentials,
            initial_files: None,
        };
        // A failure fences the store, and the next call reports it.
        let _ = session.finish_interrupted_rewind();
        Ok(session)
    }

    /// Completes a rewind that a stopped process began: until it is resumed the store
    /// refuses all new work. A begun rewind is the user's recorded request, so it is
    /// finished, never silently undone.
    fn finish_interrupted_rewind(&mut self) -> Result<()> {
        let Some(rewind) = self.store.unfinished_rewind(SCOPE).map_err(store_error)? else {
            return Ok(());
        };
        if rewind.phase == crate::rewind::RewindPhase::Pending {
            self.store
                .finish_rewind(SCOPE, &rewind.command)
                .map_err(store_error)?;
        }
        self.store
            .resume_rewind(SCOPE, &rewind.command)
            .map_err(store_error)?;
        self.store.stop_jobs();
        // As after any rewind: settings a rewound-away call queued go with it.
        let _ = drop_pending_settings(&self.dir, None);
        Ok(())
    }

    /// Create a session directory, save its settings and stage `files` for the first prompt.
    pub fn create(
        dir: &Path,
        settings: Settings,
        files: Vec<(String, Vec<u8>, bool)>,
        observer: Arc<dyn Observer>,
    ) -> Result<Self> {
        Self::create_with_credentials(dir, settings, files, observer, Arc::new(NativeCredentials))
    }

    /// Creates a session whose model calls use the embedding host's resolver.
    pub fn create_with_credentials(
        dir: &Path,
        settings: Settings,
        files: Vec<(String, Vec<u8>, bool)>,
        observer: Arc<dyn Observer>,
        credentials: Arc<dyn Credentials>,
    ) -> Result<Self> {
        credentials.install_host();
        create_private_dir(dir)?;
        settings.save(dir)?;
        cost::start_ledger(dir)?;
        let mut session = Self::open_with_credentials(dir, observer, credentials)?;
        session.initial_files = Some(files);
        Ok(session)
    }

    /// The settings currently saved for this session.
    pub fn settings(&self) -> Result<Settings> {
        let bytes = self
            .store
            .metadata("settings")
            .map_err(store_error)?
            .ok_or("session settings are missing")?;
        let value = serde_json::from_slice(&bytes).map_err(|_| "stored settings are not JSON")?;
        Settings::from_json(&value)
    }

    fn save_settings(&mut self, settings: &Settings) -> Result<()> {
        let bytes =
            serde_json::to_vec_pretty(&settings.to_json()?).map_err(|error| error.to_string())?;
        self.store
            .set_metadata("settings", &bytes)
            .map_err(store_error)?;
        save_private(&self.dir.join("session.json"), &bytes)
    }

    /// Save new settings; refused while a turn is in progress.
    pub fn set_settings(&mut self, settings: Settings) -> Result<()> {
        self.ensure_idle()?;
        self.save_settings(&settings)
    }

    /// Start a turn with `text` and drive it to its next stop.
    pub fn prompt(&mut self, text: &str) -> Result<()> {
        let outcome = self.prompt_inner(text);
        // A state given for this prompt never outlives it (it may fail before starting).
        self.start_state = None;
        outcome
    }

    fn prompt_inner(&mut self, text: &str) -> Result<()> {
        let start = match self.initial_files.take() {
            Some(files) => Start::Import(files, vec![], vec![]),
            // A session whose first turn never started (its creator stopped first) still
            // gets the empty workspace that turn would have made, or no tool could run.
            None if self.store.snapshot().map_err(store_error)?.revision == 0 => {
                Start::Import(vec![], vec![], vec![])
            }
            None => Start::Same,
        };
        self.start_turn(text, start)?;
        self.drive()
    }

    /// Sends the next prompt with `files` (path, bytes, executable) added to the
    /// workspace first; a path already there is replaced. Paths follow the workspace
    /// rules (`import_files`).
    pub fn prompt_with_files(&mut self, text: &str, files: Vec<File>) -> Result<()> {
        let outcome = self.prompt_with_files_inner(text, files);
        self.start_state = None;
        outcome
    }

    fn prompt_with_files_inner(&mut self, text: &str, files: Vec<File>) -> Result<()> {
        if files.is_empty() {
            return self.prompt_inner(text);
        }
        let first = self.store.snapshot().map_err(store_error)?.revision == 0;
        let start = match self.initial_files.take() {
            Some(mut initial) if first => {
                for (path, bytes, executable) in files {
                    initial.retain(|(kept, _, _)| *kept != path);
                    initial.push((path, bytes, executable));
                }
                Start::Import(initial, vec![], vec![])
            }
            // A first turn that never started (as in `prompt`) has no workspace to add to.
            None if first => Start::Import(files, vec![], vec![]),
            _ => Start::Add(files),
        };
        self.start_turn(text, start)?;
        self.drive()
    }

    /// The workspace root `prompt_with_tree(_, files)` would start its turn from:
    /// exactly `files`, with the current links and empty directories unless a file
    /// takes their place. A caller can record something about that root before the
    /// turn commits it.
    pub fn tree_root(&mut self, files: &[File]) -> Result<String> {
        let tree = self.links_and_directories()?;
        crate::workspace::tree_digest(&planned_tree(tree, files)).map_err(|_| BAD_FILES.into())
    }

    /// Sends the next prompt over a workspace that holds exactly `files` (every other
    /// file is removed; links and empty directories stay unless a file replaces
    /// them), as one turn start: the new workspace and the prompt commit together.
    /// Only after a completed turn (a session's first turn imports its files).
    pub fn prompt_with_tree(&mut self, text: &str, files: Vec<File>) -> Result<()> {
        let outcome = (|| {
            if self.store.snapshot().map_err(store_error)?.revision == 0 {
                return Err("the thread has not started yet".into());
            }
            self.ensure_idle()?;
            let tree = self.links_and_directories()?;
            self.start_turn(text, Start::Import(files, tree.links, tree.directories))?;
            self.drive()
        })();
        self.start_state = None;
        outcome
    }

    /// The current workspace's links and empty directories (no files).
    fn links_and_directories(&mut self) -> Result<Tree> {
        let tree = match self.store.snapshot().map_err(store_error)?.workspace {
            Some(root) => self.store.tree_map(SCOPE, &root).map_err(store_error)?,
            None => Tree::default(),
        };
        Ok(Tree {
            files: vec![],
            ..tree
        })
    }

    /// Resume an interrupted run.
    pub fn resume(&mut self) -> Result<()> {
        self.driver.until_unknown = false;
        self.restart_tasks()?;
        match self.driver.resume(&mut self.store)? {
            driver::Need::Stopped(_) => self.report_stop(),
            driver::Need::PackIdle(idle) => {
                if self.pack_follow_up(idle)? {
                    self.drive_from(None)
                } else {
                    self.emit(Event::NothingToContinue);
                    self.report_stop()
                }
            }
            need => self.drive_from(Some(need)),
        }
    }

    pub fn resume_until_unknown(&mut self) -> Result<()> {
        let report = crate::api::recovery(&self.dir)?;
        if let Some(item) = report["items"].as_array().and_then(|items| {
            items.iter().find(|item| {
                item["kind"] == "task"
                    && (item["outcome"] == "unknown"
                        || item["outcome"] == "blocked"
                        || item["unknown"] == true)
            })
        }) {
            return Err(format!(
                "stopped at unknown outcome `{}`; {}",
                item["id"].as_str().unwrap_or("task"),
                item["why"]
                    .as_str()
                    .unwrap_or("inspect recovery before continuing")
            ));
        }
        self.driver.until_unknown = true;
        self.restart_tasks()?;
        let need = self.driver.resume_until_unknown(&mut self.store)?;
        self.drive_from(Some(need))
    }

    /// A queue for messages sent while a turn runs. Each is committed as a steering
    /// update at the turn's next tool boundary, before its final answer or before its
    /// next request (as Dext's queued updates), so the model reads it without the run
    /// stopping. Messages still queued when the turn ends are returned by
    /// `take_steering`.
    pub fn steering_handle(&self) -> Arc<std::sync::Mutex<Vec<String>>> {
        self.steering.clone()
    }

    /// A flag asking for the queued messages to be read now: set it (from any thread)
    /// and a model reply being written is cut, then its request is prepared again
    /// after them. Running tools are never cut; the messages follow their results. It
    /// clears itself once the queue is empty.
    pub fn hurry_handle(&self) -> Arc<AtomicBool> {
        self.hurry.clone()
    }

    /// Steering messages a finished run never reached (send them as the next prompt).
    pub fn take_steering(&self) -> Vec<String> {
        self.hurry.store(false, Ordering::SeqCst);
        std::mem::take(
            &mut *self
                .steering
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// A flag that requests cancellation of the running turn: set it (from any
    /// thread) and the turn stops at its next safe point. A running command is ended;
    /// `resume` then records it as not completed (it is never run again).
    pub fn interrupt_handle(&self) -> Arc<AtomicBool> {
        self.interrupt.clone()
    }

    /// Whether a command runs in the sandbox. A recorded attempt keeps the runner that
    /// labelled it; a fresh one follows the session's isolation setting, and running
    /// without isolation is announced once.
    fn pick_runner(
        &mut self,
        recorded: Option<&str>,
        isolation: Isolation,
        process: bool,
    ) -> Result<bool> {
        if let Some(label) = recorded {
            if label == self.host.implementation() {
                return Ok(false);
            }
            if !matches!(self.isolated, Some(Ok(_))) {
                self.isolated = Some(open_bwrap().map(Arc::new));
            }
            return match &self.isolated {
                Some(Err(why)) => Err(format!(
                    "this step was started in the sandbox, which is unavailable now: {why}"
                )),
                _ => Ok(true),
            };
        }
        let reason = match isolation {
            Isolation::Off => "isolation is off for this session".to_string(),
            _ => {
                if self.isolated.is_none() {
                    self.isolated = Some(open_isolated().map(Arc::new));
                }
                match (&self.isolated, isolation) {
                    (Some(Ok(_)), _) => return Ok(true),
                    (Some(Err(why)), Isolation::Required) => {
                        return Err(format!(
                            "isolation is required for this session but unavailable: {why}; \
                             use `--isolation auto` to run commands without it"
                        ));
                    }
                    (Some(Err(why)), _) => why.clone(),
                    (None, _) => unreachable!("opened above"),
                }
            }
        };
        if process && !self.told_unisolated {
            self.told_unisolated = true;
            self.emit(Event::Unisolated { reason: &reason });
        }
        Ok(false)
    }

    fn emit(&self, event: Event<'_>) {
        self.observer.event(event);
    }

    fn head(&mut self) -> Result<(u64, TurnContinuation)> {
        let snapshot = self.store.snapshot().map_err(store_error)?;
        if snapshot.revision == 0 {
            return Err("this session has no turns yet".into());
        }
        let state = crate::turn::restored(&snapshot.continuation).map_err(turn_error)?;
        Ok((snapshot.revision, state))
    }

    fn start_turn(&mut self, prompt: &str, start: Start) -> Result<()> {
        let start_state = self.start_state.take();
        // Settings queued while no model turn ran to its end (sent during an install)
        // apply before this turn, as they would have when the last one stopped.
        let host = self.observer.host();
        if let Some(text) = host_tools::apply_pending(&self.dir, host.as_deref()) {
            self.emit(Event::SettingsApplied { text: &text });
        }
        let need = self
            .driver
            .start(&mut self.store, prompt.to_string(), start, start_state)?;
        self.fulfill_summaries(need).map(|_| ())
    }

    fn fulfill_summaries(&mut self, mut need: driver::Need) -> Result<driver::Need> {
        loop {
            need = match need {
                driver::Need::Summarize(summary) => {
                    let (_, state) = self.head()?;
                    let text = self.fulfill_summary(&summary.compact, &state);
                    self.driver.deliver(
                        &mut self.store,
                        driver::Answer::Summary {
                            need: summary,
                            summary: text,
                        },
                    )?
                }
                driver::Need::Start(start) => {
                    self.start_state = start.state;
                    if let Err(error) = self.start_turn(&start.prompt, start.start) {
                        self.steering
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .splice(0..0, start.joined);
                        return Err(error);
                    }
                    for text in &start.joined {
                        self.emit(Event::Steered { text });
                    }
                    driver::Need::Continue
                }
                other => return Ok(other),
            };
        }
    }

    /// Whether a new prompt can start a turn now (the last turn completed).
    pub fn accepts_prompt(&mut self) -> bool {
        self.ensure_idle().is_ok()
    }

    /// A new turn may start (and the session config change) only after a completed one.
    fn ensure_idle(&mut self) -> Result<()> {
        let snapshot = self.store.snapshot().map_err(store_error)?;
        if snapshot.revision == 0 {
            return Ok(());
        }
        let prior = crate::turn::restored(&snapshot.continuation).map_err(turn_error)?;
        match prior.control().phase() {
            Phase::Stopped(Stop::Complete) => Ok(()),
            Phase::Stopped(stop) => Err(format!(
                "the last turn stopped ({}); rewind to a target first (`spindle targets`)",
                stop_text(stop)
            )),
            _ => Err("a turn is still in progress; run `spindle continue`".into()),
        }
    }

    /// A summary of a compaction's dropped part written by the session's model, or None
    /// (the digest stands in) when the session compacts by digest or the request fails.
    fn summarize(
        &mut self,
        command: &str,
        state: &TurnContinuation,
        dropped: &[Message],
        prompt: Option<usize>,
    ) -> Option<String> {
        let config = self.settings().ok()?;
        if config.compaction != Compaction::Model {
            return None;
        }
        let route = self
            .credentials
            .route(&config.provider, Some(&config.model))
            .ok()?;
        let (source, input) = driver::summary_input(dropped, prompt);
        let prepared = code_model_request(&input, &route, state.execution(), &image_models).ok()?;
        // Journaled: counted once, reused after a crash, never sent twice.
        let reply = self.journaled_summary(command, state, &route, &prepared, &source)?;
        if let Some((input, output)) = driver::summary_usage(&reply) {
            self.emit(Event::Usage { input, output });
            self.note_cost(driver::Counted::Summary(command));
        }
        driver::summary_text(&reply)
    }

    fn drive(&mut self) -> Result<()> {
        self.driver.begin_run();
        self.drive_from(None)
    }

    fn drive_from(&mut self, mut next: Option<driver::Need>) -> Result<()> {
        loop {
            let (revision, state) = self.head()?;
            let need = match next.take() {
                Some(need) => need,
                None => self.driver.next(&mut self.store)?,
            };
            let need = self.fulfill_summaries(need)?;
            match need {
                driver::Need::Continue => {}
                driver::Need::TaskWait => {
                    if self.interrupt.load(Ordering::SeqCst) {
                        tasks::stop_all(&self.dir);
                        return Err("stopped while waiting for background tasks".into());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                driver::Need::Summarize(_) | driver::Need::Start(_) => {
                    unreachable!("nested needs were fulfilled above")
                }
                driver::Need::Send(need) => {
                    let sent = self.send_model(&need, &state)?;
                    next = Some(
                        self.driver
                            .deliver(&mut self.store, driver::Answer::Model { need, sent })?,
                    );
                }
                driver::Need::Wait(backoff) => {
                    self.wait_out(&backoff)?;
                    next = Some(
                        self.driver
                            .deliver(&mut self.store, driver::Answer::Waited(backoff))?,
                    );
                }
                driver::Need::Tool(need) => self.fulfill_tool(revision, &state, need)?,
                driver::Need::PackIdle(idle) => {
                    let end = self.run_pack_idle(&idle)?;
                    let need = self
                        .driver
                        .deliver(&mut self.store, driver::Answer::Pack { idle, end })?;
                    if matches!(need, driver::Need::Stopped(_)) {
                        return Ok(());
                    }
                    next = Some(need);
                }
                driver::Need::FollowUp { idle, follow } => {
                    let waited = self.wait_follow_up(&follow);
                    let need = self.driver.deliver(
                        &mut self.store,
                        driver::Answer::FollowUp {
                            idle,
                            follow,
                            waited,
                        },
                    )?;
                    if matches!(need, driver::Need::Stopped(_)) {
                        return Ok(());
                    }
                    next = Some(need);
                }
                driver::Need::Stopped(stop) => {
                    self.report_stop()?;
                    match self
                        .driver
                        .deliver(&mut self.store, driver::Answer::Stopped(stop))?
                    {
                        driver::Need::Stopped(_) => return Ok(()),
                        need => next = Some(need),
                    }
                }
            }
        }
    }

    fn fulfill_summary(
        &mut self,
        need: &driver::CompactCall,
        state: &TurnContinuation,
    ) -> Option<String> {
        match &need.replacement {
            driver::CompactReplacement::Summary {
                dropped, prompt, ..
            } => self.summarize(&need.command, state, dropped, *prompt),
            driver::CompactReplacement::Ready(_) => None,
        }
    }

    fn report_stop(&mut self) -> Result<()> {
        if !matches!(self.head()?.1.control().phase(), Phase::Stopped(_)) {
            return Ok(());
        }
        let host = self.observer.host();
        if let Some(text) = host_tools::apply_pending(&self.dir, host.as_deref()) {
            self.emit(Event::SettingsApplied { text: &text });
        }
        self.driver.report_stop(&mut self.store)
    }

    /// The host's half of the model step: credentials for a fresh send, then the
    /// reservation and the send, both watched (`watch_reply`) so the user's stop, or a
    /// request to read queued messages now, ends them early.
    fn send_model(
        &mut self,
        need: &driver::ModelNeed,
        state: &TurnContinuation,
    ) -> Result<driver::ModelSent> {
        // Resolved before reservation; only sent on a fresh attempt.
        let headers = match &need.route {
            Some(route) => {
                let native = self
                    .credentials
                    .route(&route.provider, Some(&route.model))?;
                Some(self.credentials.live_headers(&native, state.execution())?)
            }
            None => None,
        };
        // Live text is a preview: the recorded response is what gets adopted.
        let streamed = AtomicBool::new(false);
        let observer = self.observer.clone();
        let observe = |update: &StreamUpdate| match update {
            StreamUpdate::TextDelta(text) if !text.is_empty() => {
                streamed.store(true, Ordering::SeqCst);
                observer.event(Event::ModelDelta { text });
            }
            StreamUpdate::TextPreviewDiscarded if streamed.load(Ordering::SeqCst) => {
                observer.event(Event::ModelTextWithdrawn);
            }
            _ => {}
        };
        // The send ends early on the user's stop, or to read queued messages first when
        // the user asked for that (`hurry_handle`).
        let watch = Arc::new(ReplyWatch {
            cut: AtomicBool::new(self.interrupt.load(Ordering::SeqCst)),
            point: AtomicU64::new(0),
            done: AtomicBool::new(false),
            refused: AtomicBool::new(false),
            interrupt: self.interrupt.clone(),
            hurry: self.hurry.clone(),
            steering: self.steering.clone(),
            redo: need
                .route
                .is_some()
                .then(|| redo_point(&self.store))
                .flatten(),
            fresh: need.route.is_some(),
            observer: self.observer.clone(),
        });
        let (runtime, store, transport) = (&self.runtime, &mut self.store, &self.transport);
        let outcome = runtime.block_on(async {
            let watcher = watch.clone();
            let watcher = tokio::spawn(async move { watcher.run().await });
            // Reservation stays inside the cancellation window. Polling here and at
            // authorization also covers the synchronous reservation, before Tokio
            // first polls the watcher task.
            watch.poll();
            let outcome = async {
                match store.reserve_turn_model(&need.request, &watch.cut, |_| {
                    watch.poll();
                    Ok(())
                })? {
                    ModelReservation::Settled(outcome) => Ok(outcome),
                    ModelReservation::Reserved(committed) => {
                        let Effect::Model { waiting, .. } = committed.effect() else {
                            return Err(Error::Invalid);
                        };
                        let live = LiveRoute::new(
                            waiting.request().identity().clone(),
                            headers.ok_or(Error::Conflict)?,
                        )
                        .map(|live| {
                            live.capture_reasoning(captures_reasoning(waiting.request().body()))
                        })
                        .map_err(|_| Error::Invalid)?;
                        let recorded = transport
                            .send_once_observed(waiting, live, &watch.cut, &observe)
                            .await
                            .map_err(|error| Error::Model(error.into()))?;
                        store.ingest_turn_model(&need.request, &committed, recorded.checkpoint())
                    }
                }
            }
            .await;
            watch.done.store(true, Ordering::SeqCst);
            let _ = watcher.await;
            outcome
        });
        // Cut for queued messages, not stopped: the request is prepared again from the
        // point before it, where they are read first.
        let cut = (outcome.is_err()
            && watch.cut.load(Ordering::SeqCst)
            && !self.interrupt.load(Ordering::SeqCst))
        .then(|| watch.point.load(Ordering::SeqCst));
        let rejection = match &outcome {
            Err(Error::Model(crate::ModelError::Http(_))) => self.transport.take_rejection(),
            _ => None,
        };
        Ok(driver::ModelSent {
            outcome: outcome.map_err(|error| match error {
                Error::Model(crate::ModelError::Http(status)) => driver::ModelFailure::Http(status),
                Error::Model(
                    error @ (crate::ModelError::Transport | crate::ModelError::Deadline),
                ) => driver::ModelFailure::Dropped(model_error(error)),
                other => driver::ModelFailure::Failed(store_error(other)),
            }),
            streamed: streamed.load(Ordering::SeqCst),
            cut,
            rejection: rejection.map(|r| driver::Rejection {
                status: r.status,
                retry_after: r.retry_after,
                body: r.body,
            }),
        })
    }

    /// Waits `backoff.wait`, polling the stop flag (a zero wait returns at once).
    fn wait_out(&self, backoff: &driver::Backoff) -> Result<()> {
        let deadline = std::time::Instant::now() + backoff.wait;
        while std::time::Instant::now() < deadline {
            if self.interrupt.load(Ordering::SeqCst) {
                return Err(backoff.stopped.clone());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }

    /// Resolves one pending tool call per invocation. Returns false if none could be.
    fn fulfill_tool(
        &mut self,
        revision: u64,
        state: &TurnContinuation,
        need: driver::ToolNeed,
    ) -> Result<()> {
        match need {
            driver::ToolNeed::Committed => {}
            driver::ToolNeed::Tasks(needs) => self.run_task_batch(needs)?,
            driver::ToolNeed::Ask(need) | driver::ToolNeed::Task(need) => {
                self.run_app_tool(*need)?;
            }
            driver::ToolNeed::Run(need) => {
                let ran = self.run_builtin(revision, state, &need)?;
                self.driver.deliver(
                    &mut self.store,
                    driver::Answer::Ran {
                        need,
                        ran: Box::new(ran),
                    },
                )?;
            }
        }
        Ok(())
    }

    fn run_builtin(
        &mut self,
        revision: u64,
        state: &TurnContinuation,
        need: &driver::RunCall,
    ) -> Result<driver::ToolRan> {
        let live = runs_process(&need.call.name) && need.existing.is_none();
        // The fresh grant for this run; the attempt label below only records it.
        let networked = need.config.network && runs_process(&need.call.name);
        let sandboxed = self.pick_runner(
            need.existing
                .as_ref()
                .map(|attempt| attempt.implementation.as_str()),
            need.config.isolation,
            runs_process(&need.call.name),
        )?;
        let executor = match (&self.isolated, sandboxed) {
            (Some(Ok(runner)), true) => runner.clone(),
            _ => self.host.clone(),
        };
        let executor = executor.as_ref();
        let attempt = need.existing.clone().unwrap_or(Attempt {
            id: format!("a:{}", need.id),
            effect: need.id.clone(),
            owner_epoch: self.store.epoch(),
            implementation: executor.implementation(),
            // Audit label only: the grant itself is the fresh RunOptions below.
            grant_reference: match (executor.isolated(), networked) {
                (true, false) => "spindle-cli-sandbox",
                (true, true) => "spindle-cli-sandbox-network",
                (false, _) => "spindle-cli-host-unisolated",
            }
            .into(),
        });
        let request = DispatchRequest {
            scope: SCOPE.into(),
            owner_epoch: self.store.epoch(),
            expected_revision: revision,
            intent_revision: need.intent_revision,
            intent: need.intent.clone(),
            attempt,
        };
        // Live output is a preview; the sealed receipt is what gets adopted.
        let observer = self.observer.clone();
        let observe = |stream: u8, chunk: &[u8]| {
            observer.event(Event::Output {
                stream,
                bytes: chunk,
            });
        };
        let toolchain = match (
            need.config.toolchain && runs_process(&need.call.name),
            host_toolchain(),
        ) {
            (false, _) => None,
            (true, Some(homes)) => Some(homes),
            (true, None) => {
                return Err("--toolchain needs a host Rust toolchain (~/.rustup and ~/.cargo, or RUSTUP_HOME/CARGO_HOME)".into());
            }
        };
        let cache = self.dir.join("cache");
        if toolchain.is_some() {
            create_private_dir(&cache)?;
        }
        // The operator's shelf rides along as a read-only source, when there is one.
        let mut sources = need.config.sources.clone();
        if let Some(shelf) = shelf_of(self.observer.host().as_deref())
            && !sources.contains(&shelf)
        {
            sources.push(shelf);
        }
        let options = RunOptions {
            network: networked,
            observe: live.then_some(&observe as &(dyn Fn(u8, &[u8]) + Sync)),
            toolchain: toolchain
                .as_ref()
                .map(|(rustup_home, cargo_home)| Toolchain {
                    rustup_home,
                    cargo_home,
                    cache: &cache,
                    sources: &sources,
                }),
            // The broker enables this for run_code's language runtimes only.
            local_ipc: false,
        };
        let mut code_host = if need.call.name == "run_code" && live {
            Some(
                crate::code_host::CodeHost::open(
                    &need.config.provider,
                    &need.config.model,
                    state.execution(),
                    &self.dir,
                    Some(self.observer.clone()),
                )?
                .with_credentials(self.credentials.clone()),
            )
        } else {
            None
        };
        let outcome = if crate::turn::HOST_TOOLS.contains(&need.call.name.as_str()) {
            let host = self.observer.host();
            let dir = &self.dir;
            self.store.dispatch_host_workspace(
                &request,
                &self.interrupt,
                executor,
                |_| Ok(()),
                |name, input, workspace| {
                    host_tools::run(dir, host.as_deref(), name, input, workspace)
                },
            )
        } else {
            self.store.dispatch_code_workspace(
                &request,
                &self.interrupt,
                executor,
                options,
                |_| Ok(()),
                code_host
                    .as_mut()
                    .map(|host| host as &mut dyn crate::code_mode::Host),
            )
        };
        self.emit(Event::OutputEnd);
        Ok(driver::ToolRan {
            request,
            helper: *executor.helper_digest(),
            outcome,
        })
    }

    fn register_task_run(
        &mut self,
        key: &str,
        input: &Value,
        origin: Option<String>,
        queued: bool,
        revision: u64,
    ) -> Result<crate::meta::TaskRun> {
        if let Some(why) = tasks::refusal(input) {
            return Err(why);
        }
        let settings = self.settings()?;
        if tasks::depth(&self.dir) >= settings.task_policy.max_depth {
            return Err("task depth limit reached".into());
        }
        tasks::validate_input(&self.dir, key, input)?;
        let model = input["model"]
            .as_str()
            .or(input["settings"]["model"].as_str())
            .unwrap_or(&settings.model);
        if !settings.task_policy.models.is_empty()
            && !settings
                .task_policy
                .models
                .iter()
                .any(|allowed| allowed == model)
        {
            return Err(format!("model `{model}` is not permitted by tasks.models"));
        }
        let input = tasks::identity_input(input).to_string().into_bytes();
        if let Some(record) = self
            .store
            .task_runs()
            .map_err(store_error)?
            .into_iter()
            .find(|run| run.task == key)
        {
            if record.input != input
                || record.queued != queued
                || record.origin != origin
                || record.state == "abandoned"
            {
                return Err(
                    "task identity or report policy conflicts with its durable record".into(),
                );
            }
            if record.state == "stopped" {
                if tasks::active(&self.dir, key) {
                    return Err("task worker is still stopping; wait for task_wait to return pending=false before restarting".into());
                }
                self.store.resume_task(key).map_err(store_error)?;
                return Ok(crate::meta::TaskRun {
                    state: "running".into(),
                    ..record
                });
            }
            return Ok(record);
        }
        let base = match self
            .store
            .snapshot_at(SCOPE, revision)
            .map_err(store_error)?
            .workspace
        {
            Some(root) => root,
            None => {
                let stage = Stage {
                    id: format!("task-base-{}", self.store.epoch()),
                    scope: SCOPE.into(),
                    owner_epoch: self.store.epoch(),
                };
                self.store.begin_stage(&stage).map_err(store_error)?;
                self.store
                    .stage_tree(&stage, &Tree::default())
                    .map_err(store_error)?
            }
        };
        let lease = if let Some(cap) = settings.max_usd {
            let reserved = tasks::reserved_usd(&self.dir, None)?;
            let spent = usage_report(&self.dir)?.budget_spent();
            Some(
                ((cap - spent - reserved).max(0.0) / settings.task_policy.max_running as f64)
                    .max(0.0),
            )
        } else {
            None
        };
        let record = crate::meta::TaskRun {
            task: key.into(),
            origin,
            input,
            base,
            revision,
            queued,
            state: "running".into(),
            answer: None,
            delivered: None,
            lease,
        };
        self.store.register_task(&record).map_err(store_error)?;
        Ok(record)
    }

    pub fn start_task(&mut self, task: &str, input: &Value, report: &str) -> Result<Value> {
        if report == "wait" {
            let (dir, answer) = self.run_task(task, input)?;
            let AppAnswer::Result {
                content, failed, ..
            } = answer
            else {
                return Err("task stopped before answering".into());
            };
            return Ok(json!({"task":dir,"answer":content,"failed":failed,"state":"finished"}));
        }
        if report != "queue" {
            return Err("report must be wait or queue".into());
        }
        let key = format!("app:{task}");
        let revision = self.store.snapshot().map_err(store_error)?.revision;
        let record = self.register_task_run(&key, input, None, true, revision)?;
        let base = self.task_workspace_at(record.revision)?;
        if record.state != "finished" {
            tasks::spawn_registered(
                &self.dir,
                record,
                base,
                self.observer.clone(),
                self.credentials.clone(),
                false,
            )?;
        }
        Ok(json!({"task":tasks::task_dir(&self.dir,&key),"queued":true}))
    }

    pub fn wait_task(&self, task: &str, timeout: Duration) -> Result<Value> {
        tasks::wait(
            &self.dir,
            &tasks::key(&self.dir, task)?,
            timeout,
            &self.interrupt,
        )
    }

    pub fn stop_task(&self, task: &str) -> Result<bool> {
        tasks::stop(&self.dir, &tasks::key(&self.dir, task)?)
    }

    fn restart_tasks(&mut self) -> Result<()> {
        for record in self.store.task_runs().map_err(store_error)? {
            if !record.queued || record.state != "running" {
                continue;
            }
            let base = self.task_workspace_at(record.revision)?;
            tasks::spawn_registered(
                &self.dir,
                record,
                base,
                self.observer.clone(),
                self.credentials.clone(),
                self.driver.until_unknown,
            )?;
        }
        Ok(())
    }

    fn run_task_batch(&mut self, needs: Vec<driver::AppNeed>) -> Result<()> {
        let base_revision = needs
            .first()
            .ok_or("empty task batch")?
            .request
            .intent_revision;
        let base = self.task_workspace_at(base_revision)?;
        let limit = self.settings()?.task_policy.max_running;
        let mut results = Vec::new();
        for need in needs {
            let queued = need.call.input["background"] == true;
            let record = self.register_task_run(
                &need.id,
                &need.call.input,
                Some(need.id.clone()),
                queued,
                base_revision,
            )?;
            let reservation = self
                .driver
                .reserve_app(&mut self.store, &need)
                .map_err(store_error)?;
            let answer = if queued {
                if record.state != "finished" {
                    tasks::spawn_registered(
                        &self.dir,
                        record,
                        base.clone(),
                        self.observer.clone(),
                        self.credentials.clone(),
                        self.driver.until_unknown,
                    )?;
                }
                Some(Ok(AppAnswer::from_json(&json!({"content":format!("Task {} started in the background; its answer will arrive as a queued update", tasks::task_dir(&self.dir,&need.id).display())})).map_err(|error| error.to_string())?))
            } else {
                None
            };
            results.push((need, reservation, answer));
        }
        let parent = &self.dir;
        let interrupt = &self.interrupt;
        let observer = &self.observer;
        let credentials = &self.credentials;
        let until_unknown = self.driver.until_unknown;
        for chunk in results.chunks_mut(limit) {
            std::thread::scope(|scope| {
                let mut running = Vec::new();
                for (need, reservation, result) in chunk {
                    if need.call.input["background"] != true
                        && matches!(reservation, crate::broker::AppReservation::Reserved { .. })
                    {
                        let base = &base;
                        running.push((
                            result,
                            scope.spawn(move || {
                                tasks::run(
                                    parent,
                                    &need.id,
                                    &need.call.input,
                                    base,
                                    interrupt,
                                    tasks::Host {
                                        observer,
                                        credentials,
                                        until_unknown,
                                    },
                                )
                            }),
                        ));
                    }
                }
                for (result, task) in running {
                    *result = Some(task.join().unwrap_or_else(|_| {
                        Err(
                            "task worker panicked; inspect its own session before continuing"
                                .into(),
                        )
                    }));
                }
            });
        }
        for (mut need, reservation, result) in results {
            let mut waiting = None;
            need.request.expected_revision = self.store.snapshot().map_err(store_error)?.revision;
            let outcome = match reservation {
                crate::broker::AppReservation::Settled(outcome) => Ok(outcome),
                crate::broker::AppReservation::Reserved { .. } => {
                    match result.ok_or("task worker returned no outcome")? {
                        Ok(mut answer) => {
                            if need.call.input["background"] != true {
                                self.retain_task_answer(&need.id, &need.call.input, &answer)?;
                                let current =
                                    self.task_workspace_at(need.request.expected_revision)?;
                                tasks::refuse_conflicts(&base, &current, &mut answer);
                            }
                            self.driver.record_app(&mut self.store, &need, answer)
                        }
                        Err(why) => {
                            waiting = Some(why);
                            Err(Error::Cancelled)
                        }
                    }
                }
            };
            self.driver.deliver(
                &mut self.store,
                driver::Answer::App {
                    need: Box::new(need),
                    outcome,
                    waiting,
                },
            )?;
        }
        Ok(())
    }

    fn run_app_tool(&mut self, need: driver::AppNeed) -> Result<()> {
        if need.call.name == tasks::NAME {
            return self.run_task_batch(vec![need]);
        }
        let answerer = self.observer.app_tools().ok_or_else(|| format!(
            "`{}` is a tool of the app, and no app is attached to answer it; continue this session from the app",
            need.call.name
        ))?;
        let outcome = match self.driver.reserve_app(&mut self.store, &need) {
            Ok(crate::broker::AppReservation::Settled(outcome)) => Ok(outcome),
            Ok(crate::broker::AppReservation::Reserved { call, again }) => {
                let ask = AppCall {
                    call: &need.call_id,
                    tool_use_id: &call.id,
                    name: &call.name,
                    input: &call.input,
                    again,
                    rerun: need.app.rerun,
                    timeout: need.app.timeout,
                };
                let answer = answerer.call(&ask, &self.interrupt);
                self.driver.record_app(&mut self.store, &need, answer)
            }
            Err(error) => Err(error),
        };
        self.driver
            .deliver(
                &mut self.store,
                driver::Answer::App {
                    need: Box::new(need),
                    outcome,
                    waiting: None,
                },
            )
            .map(|_| ())
    }

    /// Runs the task `task` (an id the app chooses: the same id finds the same task)
    /// to the end of its turn, starting from this session's files at its head, and
    /// returns its directory and answer. `input` is the `task` tool's: `prompt`, and
    /// optionally `model` and `effort`. Its files stay in the task (read them there);
    /// its spend counts against this session.
    pub fn run_task(&mut self, task: &str, input: &Value) -> Result<(PathBuf, AppAnswer)> {
        if let Some(reason) = tasks::refusal(input) {
            return Err(reason);
        }
        if tasks::depth(&self.dir) >= self.settings()?.task_policy.max_depth {
            return Err("this session reached its configured task depth limit".into());
        }
        let key = format!("app:{task}");
        tasks::validate_input(&self.dir, &key, input)?;
        self.ensure_idle()?;
        let head = self.store.snapshot().map_err(store_error)?.revision;
        let record = self.register_task_run(&key, input, None, false, head)?;
        let base = self.task_workspace_at(record.revision)?;
        let dir = tasks::task_dir(&self.dir, &key);
        let interrupt = self.interrupt.clone();
        let answer = tasks::run(
            &self.dir,
            &key,
            input,
            &base,
            &interrupt,
            tasks::Host {
                observer: &self.observer,
                credentials: &self.credentials,
                until_unknown: false,
            },
        )
        .map_err(|why| {
            format!(
                "the task cannot go on by itself: {why}. Settle it in its own session ({}), \
                     then start it again",
                dir.display()
            )
        })?;
        self.retain_task_answer(&key, input, &answer)?;
        Ok((dir, answer))
    }

    fn retain_task_answer(&self, key: &str, input: &Value, answer: &AppAnswer) -> Result<()> {
        let answer = match answer {
            AppAnswer::Result {
                content, failed, ..
            } => Some(json!({"content":content,"failed":failed}).to_string()),
            _ => None,
        };
        let kept = self
            .store
            .task_runs()
            .map_err(store_error)?
            .into_iter()
            .find(|run| run.task == key)
            .ok_or("no registered task")?;
        if kept.state == "finished" {
            return Ok(());
        }
        crate::meta::finish_task(
            &self.dir,
            key,
            tasks::identity_input(input).to_string().as_bytes(),
            match &answer {
                Some(answer) => crate::meta::TaskEnd::Answered(answer.as_bytes()),
                None => crate::meta::TaskEnd::Stopped(None),
            },
        )
        .map_err(store_error)
    }

    fn task_workspace_at(&mut self, revision: u64) -> Result<tasks::Workspace> {
        let tree = match self
            .store
            .snapshot_at(SCOPE, revision)
            .map_err(store_error)?
            .workspace
        {
            Some(root) => self.store.tree_map(SCOPE, &root).map_err(store_error)?,
            None => Tree::default(),
        };
        Ok(tasks::Workspace {
            files: if revision == 0 {
                vec![]
            } else {
                self.files_at(revision)?
            },
            links: tree.links,
            directories: tree.directories,
        })
    }

    /// The app state as of `revision` (the head when None): a JSON object.
    pub fn state(&mut self, revision: Option<u64>) -> Result<Value> {
        let revision = match revision {
            Some(revision) => revision,
            None => self.store.snapshot().map_err(store_error)?.revision,
        };
        let bytes = self.store.app_state(SCOPE, revision).map_err(store_error)?;
        serde_json::from_slice(&bytes).map_err(|_| "the stored app state is not JSON".into())
    }

    /// Commits `patch` (a JSON merge patch) to the app state as a step of its own,
    /// leaving the conversation as it is; returns the step's revision. Between turns,
    /// or after a stop between two steps (not one that cut a request or tool call
    /// off); while a turn runs use `state_handle`. A session's first state goes with
    /// its first prompt (`state_with_next_prompt`).
    pub fn set_state(&mut self, patch: &Value) -> Result<u64> {
        self.driver.set_state(&mut self.store, patch)
    }

    /// A queue for app state changes (JSON merge patches) sent while a turn runs: each
    /// is committed as a step of its own at the turn's next step boundary, in order.
    pub fn state_handle(&self) -> Arc<std::sync::Mutex<Vec<Value>>> {
        self.state_queue.clone()
    }

    /// The next prompt's first step also applies `patch` to the app state (one commit
    /// with the prompt).
    pub fn state_with_next_prompt(&mut self, patch: Value) -> Result<()> {
        if !patch.is_object() {
            return Err("`state` is a JSON merge patch: an object".into());
        }
        self.start_state = Some(StartState::Patch(patch));
        Ok(())
    }

    /// Commits the state changes still queued (`state_handle`) as steps of their own,
    /// now: what a host does when a turn has ended, so none waits for the next turn
    /// and lands after a newer change. One that cannot be kept is refused with a
    /// `StateRefused` event.
    pub fn commit_waiting_state(&mut self) -> Result<()> {
        self.driver
            .step(&mut self.store)
            .commit_queued_state()
            .map(|_| ())
    }

    /// Queued admissions block rewinds; they are explicitly declined first.
    fn drop_queued(&mut self) -> Result<()> {
        let report = self.store.reconciliation().map_err(store_error)?;
        for (scope, sequence) in report.queued {
            if scope == SCOPE {
                self.store
                    .drop_admission(sequence, &format!("drop-{sequence}"))
                    .map_err(store_error)?;
            }
        }
        Ok(())
    }

    /// Go back to a point from `targets`, restoring its workspace.
    pub fn rewind(&mut self, target: u64) -> Result<()> {
        self.rewind_to(target, false)
    }

    /// `rewind`; `same_work` when only the pending model request is undone (the
    /// target's workspace and settings are the head's), so background jobs keep running
    /// and queued settings stay queued.
    fn rewind_to(&mut self, target: u64, same_work: bool) -> Result<()> {
        self.drop_queued()?;
        let targets = self.store.rewind_targets(SCOPE).map_err(|error| {
            format!(
                "cannot rewind now: {}",
                match error {
                    Error::Conflict =>
                        "a command is still unresolved; run `spindle continue` first".into(),
                    other => store_error(other),
                }
            )
        })?;
        let Some(chosen) = targets.iter().find(|item| item.revision == target) else {
            return Err(format!(
                "revision {target} is not a rewind target; see `spindle targets`"
            ));
        };
        let kind = chosen.kind;
        let head = self.store.snapshot().map_err(store_error)?.revision;
        let lineage = self.store.lineage().map_err(store_error)?;
        let cut = self
            .store
            .answered_app_calls(SCOPE, target)
            .map_err(store_error)?
            .iter()
            .map(|id| self.store.call_id(id).map_err(store_error))
            .collect::<Result<Vec<_>>>()?;
        let state_before = self.store.app_state(SCOPE, head).map_err(store_error)?;
        let command = format!("rw-{head}");
        self.store
            .begin_rewind(SCOPE, &command, target, head, lineage)
            .map_err(store_error)?;
        self.store
            .finish_rewind(SCOPE, &command)
            .map_err(store_error)?;
        self.store
            .resume_rewind(SCOPE, &command)
            .map_err(store_error)?;
        if !same_work {
            tasks::stop_all(&self.dir);
            // Jobs ran over a workspace the rewound session no longer has.
            self.store.stop_jobs();
            // Settings a rewound-away call queued are undone with it (the rewind itself
            // is done; a queue that cannot be cleared is only reported by its next use).
            let _ = drop_pending_settings(&self.dir, None);
        }
        let report = self.store.reconciliation().map_err(store_error)?;
        let revision = self.store.snapshot().map_err(store_error)?.revision;
        self.emit(Event::Rewound {
            target,
            revision,
            kind,
            abandoned: report.abandoned_attempts.len(),
        });
        if !cut.is_empty() {
            self.emit(Event::AppCallsRewound { calls: &cut });
        }
        let state_after = self.store.app_state(SCOPE, revision).map_err(store_error)?;
        self.driver
            .step(&mut self.store)
            .emit_state(&state_before, &state_after, revision);
        Ok(())
    }

    /// Re-run an interrupted model request from the point before it. This session's
    /// `interrupt_handle` and `steering_handle` keep reaching the retried turn.
    pub fn retry(mut self) -> Result<()> {
        match self.driver.retry(&mut self.store)? {
            driver::Retry::FreshStore { prompt } => self.restart_first(prompt),
            driver::Retry::InPlace(need) => {
                self.fulfill_summaries(need)?;
                self.drive()
            }
        }
    }

    /// The session's first request has no earlier point to rewind to. Nothing else has
    /// happened yet, so the old store is kept aside (its unknown request stays on
    /// record there) and a fresh store restarts the same prompt over the same files.
    fn restart_first(mut self, prompt: String) -> Result<()> {
        if self.store.snapshot().map_err(store_error)?.revision != 1 {
            return Err(
                "no earlier point can be rewound to (`spindle targets`); run `spindle doctor`"
                    .into(),
            );
        }
        let mut start = Start::Import(vec![], vec![], vec![]);
        if let Some(root) = self.store.snapshot().map_err(store_error)?.workspace {
            let tree = self.store.tree_map(SCOPE, &root).map_err(store_error)?;
            let mut files = vec![];
            for entry in tree.files {
                let bytes = self
                    .store
                    .read_object(SCOPE, &entry.object)
                    .map_err(store_error)?;
                files.push((entry.path, bytes, entry.executable));
            }
            start = Start::Import(files, tree.links, tree.directories);
        }
        let carried = self.store.app_state(SCOPE, 1).map_err(store_error)?;
        let metadata = ["settings", "pending_settings", "output_limits"]
            .into_iter()
            .map(|key| self.store.metadata(key).map(|bytes| (key, bytes)))
            .collect::<crate::Result<Vec<_>>>()
            .map_err(store_error)?;
        let dir = self.dir.clone();
        let observer = self.observer.clone();
        let credentials = self.credentials.clone();
        // The caller's stop flag, steering and state queues reach the restarted turn.
        let (interrupt, steering) = (self.interrupt.clone(), self.steering.clone());
        let hurry = self.hurry.clone();
        let state_queue = self.state_queue.clone();
        self.store.stop_jobs();
        drop(self);
        let mut index = 1;
        let mut aside = dir.join("store.abandoned-1");
        while aside.exists() {
            index += 1;
            aside = dir.join(format!("store.abandoned-{index}"));
        }
        std::fs::rename(dir.join("store"), &aside).map_err(|error| error.to_string())?;
        // Its unanswered request stays counted. Best effort: the store aside keeps it.
        let _ = cost::keep_aside(&dir, &aside);
        observer.event(Event::RetryingFirst { path: &aside });
        let mut session = Session::open_with_credentials(&dir, observer, credentials)?;
        for (key, bytes) in metadata {
            if let Some(bytes) = bytes {
                session
                    .store
                    .set_metadata(key, &bytes)
                    .map_err(store_error)?;
            }
        }
        session.interrupt = interrupt;
        session.steering = steering;
        session.hurry = hurry;
        session.state_queue = state_queue;
        session.driver.set_inbox(Arc::new(NativeInbox {
            interrupt: session.interrupt.clone(),
            steering: session.steering.clone(),
            hurry: session.hurry.clone(),
            state_queue: session.state_queue.clone(),
        }));
        session.start_state = Some(StartState::Exact(carried));
        session.start_turn(&prompt, start)?;
        session.drive()
    }

    /// Where the session stands.
    pub fn status(&mut self) -> Result<Status> {
        let settings = self.settings()?;
        let (revision, state) = self.head()?;
        let report = self.store.reconciliation().map_err(store_error)?;
        let prompts = state
            .transcript()
            .map_err(turn_error)?
            .messages
            .iter()
            .filter(|message| is_prompt(message))
            .count();
        let phase = match state.control().phase() {
            Phase::Stopped(stop) => stop_text(stop).to_string(),
            Phase::ModelWait => "waiting for a model response (run `spindle continue`)".into(),
            Phase::ToolsWait => "running commands (run `spindle continue`)".into(),
            _ => "between steps (run `spindle continue`)".into(),
        };
        Ok(Status {
            session: self.dir.clone(),
            provider: settings.provider,
            model: settings.model,
            revision,
            epoch: self.store.epoch(),
            prompts,
            state: phase,
            files: self.workspace_entries()?.len(),
            unresolved: report.unresolved_attempts.len(),
            abandoned: report.abandoned_attempts.len(),
            rejected: report.rejected_attempts.len(),
            isolation: settings.isolation,
            effort: settings.effort,
        })
    }

    /// The conversation of the current revision.
    pub fn history(&mut self) -> Result<Vec<Message>> {
        let (_, state) = self.head()?;
        Ok(state.transcript().map_err(turn_error)?.messages)
    }

    /// Points this session can rewind to.
    pub fn targets(&mut self) -> Result<Vec<Target>> {
        let targets = self
            .store
            .rewind_targets(SCOPE)
            .map_err(|error| match error {
                Error::Conflict => {
                    "no targets while a step is unresolved; run `spindle continue`".to_string()
                }
                other => store_error(other),
            })?;
        let mut out = vec![];
        for target in targets {
            let snapshot = self
                .store
                .snapshot_at(SCOPE, target.revision)
                .map_err(store_error)?;
            let state = crate::turn::restored(&snapshot.continuation).map_err(turn_error)?;
            let messages = state.transcript().map_err(turn_error)?.messages;
            let last = messages
                .iter()
                .rev()
                .find_map(|message| {
                    message.content.iter().rev().find_map(|block| match block {
                        Block::Text { text } if !text.trim().is_empty() => {
                            Some(match steered_text(text) {
                                Some(user) => format!("user (while running): {user}"),
                                None => format!("{}: {text}", message.role),
                            })
                        }
                        Block::ToolUse { name, input, .. } => Some(describe_call(name, input)),
                        _ => None,
                    })
                })
                .unwrap_or_default();
            let last: String = last.replace('\n', " ").chars().take(90).collect();
            out.push(Target {
                revision: target.revision,
                kind: target.kind,
                last,
            });
        }
        Ok(out)
    }

    fn workspace_entries(&mut self) -> Result<Vec<Entry>> {
        match self.store.snapshot().map_err(store_error)?.workspace {
            Some(root) => self.store.file_map(SCOPE, &root).map_err(store_error),
            None => Ok(vec![]),
        }
    }

    /// Workspace files with their sizes (from the rows alone: no file is read).
    pub fn files(&mut self) -> Result<Vec<(String, u64)>> {
        let Some(root) = self.store.snapshot().map_err(store_error)?.workspace else {
            return Ok(vec![]);
        };
        let files = crate::view::ReadView::from_store(&self.store)
            .files(&root)
            .map_err(store_error)?;
        Ok(files
            .into_iter()
            .map(|file| (file.entry.path, file.size))
            .collect())
    }

    /// Read one workspace file.
    pub fn read(&mut self, path: &str) -> Result<Vec<u8>> {
        let root = self
            .store
            .snapshot()
            .map_err(store_error)?
            .workspace
            .ok_or("the workspace is empty")?;
        self.store
            .read_versioned_file(SCOPE, &root, path)
            .map_err(|_| format!("no file `{path}` in the workspace"))
    }

    /// Workspace files committed at `revision` (path, bytes, executable).
    pub fn files_at(&mut self, revision: u64) -> Result<Vec<(String, Vec<u8>, bool)>> {
        let Some(root) = self
            .store
            .snapshot_at(SCOPE, revision)
            .map_err(store_error)?
            .workspace
        else {
            return Ok(vec![]);
        };
        let mut files = vec![];
        for entry in self.store.file_map(SCOPE, &root).map_err(store_error)? {
            let bytes = self
                .store
                .read_object(SCOPE, &entry.object)
                .map_err(store_error)?;
            files.push((entry.path, bytes, entry.executable));
        }
        Ok(files)
    }

    /// Writes this session to a bundle at `target` (which must not exist): its store
    /// and every task's, each as one image, with their sidecars. Unless
    /// `keep`, the session then moves: this copy is sealed (a `spindle-moved` fence)
    /// and refuses all work, so it continues only where the bundle is imported; drop
    /// this `Session` after. Its tasks' copies are sealed too (a task copy that cannot
    /// be is named in `warnings`; the move stands). If this copy cannot be sealed, the
    /// bundle is removed and nothing moved. A task in use (a background task running)
    /// refuses the export.
    pub fn export_session(&mut self, target: &Path, keep: bool) -> Result<Value> {
        use crate::bundle::{BundleFile, STORE};
        if std::fs::symlink_metadata(target).is_ok() {
            return Err(format!("{} already exists", target.display()));
        }
        refuse_running_tasks(&self.dir, &self.store)?;
        let mut files = vec![BundleFile {
            path: STORE.into(),
            bytes: self.store.image().map_err(store_error)?,
        }];
        if let Ok(bytes) = std::fs::read(self.dir.join("session.json")) {
            files.push(BundleFile {
                path: "session.json".into(),
                bytes,
            });
        }
        let mut tasks = Vec::new();
        bundle_tasks(&self.dir, "", &mut files, &mut tasks)?;
        // A copy (the session stays here) is checked as far as the bundle records it; a
        // move, whole, as it goes on only from the bundle.
        let written = if keep {
            crate::bundle::write_copy(files)?
        } else {
            crate::bundle::write(files)?
        };
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(target)
            .map_err(|error| format!("cannot write {}: {error}", target.display()))?;
        // From here the file is this export's own: removed if anything fails.
        let saved = (|| -> std::io::Result<()> {
            use std::io::Write;
            file.write_all(&written.bytes)?;
            file.sync_all()?;
            crate::durable::sync_parent(target)
        })();
        drop(file);
        if let Err(error) = saved {
            let _ = std::fs::remove_file(target);
            return Err(format!("cannot write {}: {error}", target.display()));
        }
        let mut warnings = Vec::new();
        if !keep {
            let place = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
            let note = format!(
                "exported at head {} to {}",
                written.facts.head,
                place.display()
            );
            // This copy first: if it cannot be sealed, nothing has moved.
            if let Err(error) = self.store.seal_moved(&note) {
                let _ = std::fs::remove_file(target);
                let _ = crate::durable::sync_parent(target);
                return Err(format!(
                    "this copy could not be sealed ({}), so the session did not move and \
                     the bundle was removed",
                    store_error(error)
                ));
            }
            for store in tasks.iter_mut() {
                if let Err(error) = store.seal_moved(&note) {
                    warnings.push(format!(
                        "a task's copy here could not be sealed: {}",
                        store_error(error)
                    ));
                }
            }
        }
        Ok(json!({
            "target": target.to_string_lossy(),
            "size": written.bytes.len(),
            "head": written.facts.head,
            "identity": written.facts.identity,
            "files": written.files,
            "moved": !keep,
            "warnings": warnings,
        }))
    }

    /// Exports a revision's workspace and app state in separate namespaces.
    pub fn export(&mut self, target: &Path) -> Result<usize> {
        if target.exists()
            && std::fs::read_dir(target)
                .map_err(|error| error.to_string())?
                .next()
                .is_some()
        {
            return Err(format!("{} is not empty", target.display()));
        }
        let revision = self.store.snapshot().map_err(store_error)?.revision;
        let state = self.store.app_state(SCOPE, revision).map_err(store_error)?;
        std::fs::create_dir_all(target.join("workspace")).map_err(|error| error.to_string())?;
        let count = if self
            .store
            .snapshot()
            .map_err(store_error)?
            .workspace
            .is_some()
        {
            self.export_workspace(&target.join("workspace"))?
        } else {
            0
        };
        crate::durable::write(&target.join("state.json"), &state)
            .map_err(|error| error.to_string())?;
        Ok(count)
    }

    fn export_workspace(&mut self, target: &Path) -> Result<usize> {
        if target.exists()
            && std::fs::read_dir(target)
                .map_err(|error| error.to_string())?
                .next()
                .is_some()
        {
            return Err(format!("{} is not empty", target.display()));
        }
        let root = self
            .store
            .snapshot()
            .map_err(store_error)?
            .workspace
            .ok_or("the workspace is empty")?;
        let tree = self.store.tree_map(SCOPE, &root).map_err(store_error)?;
        let io = |error: std::io::Error| error.to_string();
        // Only plain names: nothing a host would read as a parent, root or drive.
        for path in tree
            .files
            .iter()
            .map(|entry| &entry.path)
            .chain(tree.links.iter().map(|link| &link.path))
            .chain(&tree.directories)
        {
            if !Path::new(path)
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
            {
                return Err(format!("`{path}` cannot be exported on this host"));
            }
        }
        for entry in &tree.files {
            // The tree is validated once; each file is read by its object.
            let bytes = self
                .store
                .read_object(SCOPE, &entry.object)
                .map_err(store_error)?;
            let path = target.join(&entry.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(io)?;
            }
            std::fs::write(&path, bytes).map_err(io)?;
            #[cfg(unix)]
            if entry.executable {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .map_err(io)?;
            }
        }
        for directory in &tree.directories {
            std::fs::create_dir_all(target.join(directory)).map_err(io)?;
        }
        // Links are recreated as stored, never resolved here.
        for link in &tree.links {
            let path = target.join(&link.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(io)?;
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link.target, &path).map_err(io)?;
            #[cfg(not(unix))]
            std::fs::write(&path, &link.target).map_err(io)?;
        }
        Ok(tree.files.len())
    }
}

/// Effect ids are `<execution>/m/<ordinal>` for model requests and add `/t/<pos>`
/// for tool calls; attempt ids may carry an `a:` prefix.
fn is_model_effect(id: &str) -> bool {
    id.rsplit_once("/m/").is_some_and(|(_, ordinal)| {
        !ordinal.is_empty() && ordinal.bytes().all(|b| b.is_ascii_digit())
    })
}

/// The one-line reason a turn stopped, shared by the CLI and the web UI.
pub fn stop_text(stop: Stop) -> &'static str {
    match stop {
        Stop::Complete => "done",
        Stop::Iterations => "stopped: step limit reached",
        Stop::Budget => "stopped: budget reached",
        Stop::ToolPolicy => "stopped by tool policy",
        Stop::ContentFilter => "stopped by the provider's content filter",
        Stop::RecoveryBudget => "stopped: too many incomplete responses",
        Stop::Cancelled => "cancelled",
    }
}

struct NativeEnv {
    dir: PathBuf,
    observer: Arc<dyn Observer>,
    credentials: Arc<dyn Credentials>,
}
struct NativeInbox {
    interrupt: Arc<AtomicBool>,
    steering: Arc<std::sync::Mutex<Vec<String>>>,
    hurry: Arc<AtomicBool>,
    state_queue: Arc<std::sync::Mutex<Vec<Value>>>,
}
impl driver::Inbox for NativeInbox {
    fn next_steer(&self) -> Option<String> {
        let mut queue = self
            .steering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if queue.is_empty() {
                self.hurry.store(false, Ordering::SeqCst);
                return None;
            }
            let text = queue.remove(0);
            if !text.trim().is_empty() && text.len() <= MAX_STEER {
                return Some(text);
            }
        }
    }

    fn return_steering(&self, texts: Vec<String>) {
        self.steering
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .splice(0..0, texts);
    }

    fn take_steering(&self) -> Vec<String> {
        self.hurry.store(false, Ordering::SeqCst);
        std::mem::take(
            &mut *self
                .steering
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
    fn interrupted(&self) -> bool {
        self.interrupt.load(Ordering::SeqCst)
    }
    fn take_state(&self) -> Vec<Value> {
        std::mem::take(
            &mut *self
                .state_queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

impl driver::Observer for NativeEnv {
    fn event(&self, event: Event<'_>) {
        self.observer.event(event);
    }
}
impl driver::Env for NativeEnv {
    fn limit_key(&self, provider: &str, model: &str) -> Result<String> {
        let route = self.credentials.route(provider, Some(model))?;
        Ok(format!(
            "{}/{}",
            route.profile.id,
            provider::normalize_provider_model_value(&route.profile, &route.model)
                .to_ascii_lowercase()
        ))
    }
    fn route(
        &self,
        provider: &str,
        model: Option<&str>,
        effort: Effort,
        learned: Option<u32>,
    ) -> Result<driver::Route> {
        let mut route = self.credentials.route(provider, model)?;
        route.learned_limit = learned;
        let reasoning = reasoning(&route, effort)?;
        Ok(driver::Route {
            provider: route.profile.id.clone(),
            model: route.model.clone(),
            endpoint: route.endpoint.clone(),
            contract: route.contract.into(),
            history_budget: history_budget(&route, effort)?,
            reply_reserve: reply_reserve(&route, effort)?,
            sent_tokens: reasoning.answer + reasoning.allowance,
        })
    }
    fn request(
        &self,
        state: &TurnContinuation,
        config: &Settings,
        learned: Option<u32>,
    ) -> Result<PreparedRequest<Vec<u8>>> {
        let mut route = self
            .credentials
            .route(&config.provider, Some(&config.model))?;
        route.learned_limit = learned;
        let mut guidance = if config.self_edit {
            Some(self_edit_guidance())
        } else {
            self.observer
                .host()
                .filter(|host| host.offers_source())
                .map(|_| source_guidance())
        };
        if config.toolchain && shelf_of(self.observer.host().as_deref()).is_some() {
            guidance
                .get_or_insert_with(String::new)
                .push_str(&shelf_guidance());
        }
        let subscription = subscription_on(self.credentials.as_ref(), &route);
        let identity = subscription.then(|| self.credentials.identity()).flatten();
        prepared_request(
            state,
            &RequestShape {
                config: config.clone(),
                route,
                guidance,
                tasks_offered: tasks::offered(config, &self.dir),
                subscription,
                identity,
            },
        )
    }
    fn refusal(&self, settings: &Settings, name: &str, input: &Value) -> Option<String> {
        offered_refusal(settings, tasks::offered(settings, &self.dir), name, input)
    }
    fn app_dispatch(
        &self,
        settings: &Settings,
        name: &str,
        existing: Option<&Attempt>,
    ) -> Option<driver::AppDispatch> {
        app_dispatch(settings, name, existing).map(|a| driver::AppDispatch {
            helper: a.helper,
            rerun: a.rerun,
            timeout: a.timeout,
            max_output: a.max_output,
        })
    }
    fn tasks_offered(&self, settings: &Settings) -> bool {
        tasks::offered(settings, &self.dir)
    }
    fn task_place(&self, id: &str) -> String {
        tasks::task_dir(&self.dir, id).display().to_string()
    }
    fn budget_refusal(
        &self,
        store: &Store,
        request: &PreparedRequest<Vec<u8>>,
    ) -> Result<Option<String>> {
        cost::owner_budget(&self.dir, store, request)
    }
    fn counted(&self, store: &Store, counted: driver::Counted<'_>) -> Option<driver::Cost> {
        cost::owner_counted(&self.dir, store, counted)
    }
    fn pack_active(&self, store: &mut Store) -> Result<bool> {
        store.pack_active(SCOPE).map_err(store_error)
    }
    fn pack_round_steer(
        &self,
        store: &mut Store,
        state: &TurnContinuation,
    ) -> Result<Option<String>> {
        pack_turn::round_steer(store, state)
    }
    fn cancel_follow_up(&self, store: &mut Store, revision: u64, execution: &str) -> Result<()> {
        store
            .pack_cancel_follow_up(SCOPE, revision, execution)
            .map_err(store_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{
        CURRENT_REQUEST, compacted, context_overflow, stated_output_limit, transcript_bytes,
    };

    #[cfg(unix)]
    #[test]
    fn store_open_names_a_non_private_store_directory() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("spindle-mode-{}", std::process::id()));
        let store = root.join("store");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = Store::open(&store)
            .err()
            .expect("a 0755 store root is refused");
        let message = store_open_error(&store, error);
        assert!(message.contains("mode 0700, found 0755"), "{message}");
        let findings = doctor(&root);
        assert!(
            findings
                .iter()
                .any(|f| !f.ok && f.detail.contains("found 0755")),
            "doctor names the mode"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    fn builtin(id: &str, model: &str) -> Route {
        let catalog = provider::default_provider_catalog();
        route_for(
            provider::find_provider_profile(&catalog, id).unwrap(),
            Some(model),
        )
        .unwrap()
    }

    #[test]
    fn explicit_request_shapes_match_the_original_request_bytes_on_every_contract() {
        let config = |route: &Route| Settings {
            provider: route.profile.id.clone(),
            model: route.model.clone(),
            network: false,
            toolchain: false,
            sources: vec![],
            isolation: Isolation::Off,
            effort: Effort::High,
            compaction: Compaction::Digest,
            max_usd: None,
            self_edit: false,
            tools: vec![],
            builtin_tools: BuiltinTools::All,
            instructions: Some("App guidance.".into()),
            resend_unknown: false,
            tasks: true,
            task_policy: Default::default(),
            runner: Default::default(),
        };
        for (provider, model, subscription) in [
            ("deepseek", "deepseek-flash", false),
            ("openai", "gpt-5", false),
            ("chatgpt", "gpt-5", false),
            ("anthropic", "claude-sonnet-4-6", false),
            ("anthropic", "claude-sonnet-4-6", true),
        ] {
            let route = builtin(provider, model);
            let state = TurnContinuation::new(
                SCOPE.into(),
                "shape-test".into(),
                route.contract.into(),
                None,
                TranscriptCheckpoint {
                    route_binding: ROUTE.into(),
                    messages: vec![text("user", "go")],
                },
            )
            .unwrap();
            let shape = RequestShape {
                config: config(&route),
                route,
                guidance: Some("Host guidance.".into()),
                tasks_offered: true,
                subscription,
                identity: Some(dext_provider_catalog::claude_subscription::ClaudeIdentity {
                    device_id: "device".into(),
                    account_uuid: "account".into(),
                }),
            };
            let found = prepared_request(&state, &shape).unwrap();
            let body = request_body_for(
                &shape.route,
                model,
                state.execution(),
                false,
                false,
                Some("Host guidance."),
                Effort::High,
                &state.transcript().unwrap(),
                shape.config.offer(true),
            )
            .unwrap();
            let expected = if subscription {
                subscription_body_with_identity(
                    body,
                    "go",
                    state.execution(),
                    shape.identity.as_ref(),
                )
                .unwrap()
            } else {
                serde_json::to_vec(&body).unwrap()
            };
            assert_eq!(
                found.body(),
                expected,
                "{provider} subscription={subscription}"
            );
            assert_eq!(
                found.body(),
                prepared_request(&state, &shape).unwrap().body()
            );
        }
    }

    #[test]
    fn learned_output_limits_are_route_facts_not_process_global_state() {
        let mut limited = builtin("deepseek", "deepseek-flash");
        let unchanged = builtin("deepseek", "deepseek-flash");
        let original = output_tokens(&unchanged, Effort::High);
        limited.learned_limit = Some(4096);
        let (answer, reasoning, limit) = output_tokens(&limited, Effort::High);
        assert_eq!(answer + reasoning, 4096);
        assert_eq!(limit, Some(4096));
        assert_eq!(output_tokens(&unchanged, Effort::High), original);
    }

    #[test]
    fn every_level_gets_its_full_allowance_within_the_model_output_limit() {
        let prompt = TranscriptCheckpoint {
            route_binding: ROUTE.into(),
            messages: vec![text("user", "go")],
        };
        let body = |route: &Route, effort| {
            request_body(route, &route.model, "s", false, false, effort, &prompt).unwrap()
        };
        let deepseek = builtin("deepseek", "deepseek-flash");
        assert_eq!(body(&deepseek, Effort::Max)["reasoning_effort"], "max");
        assert_eq!(body(&deepseek, Effort::XHigh)["reasoning_effort"], "high");
        assert_eq!(
            body(&deepseek, Effort::Max)["max_tokens"],
            MAX_OUTPUT_TOKENS + 65536
        );
        assert_eq!(
            body(&deepseek, Effort::XHigh)["max_tokens"],
            MAX_OUTPUT_TOKENS + 32768
        );
        assert_eq!(
            body(&deepseek, Effort::High)["max_tokens"],
            MAX_OUTPUT_TOKENS + 16384
        );
        let sol = builtin("chatgpt", "gpt-6.1-sol");
        assert_eq!(body(&sol, Effort::XHigh)["reasoning"]["effort"], "xhigh");
        assert_eq!(body(&sol, Effort::Max)["reasoning"]["effort"], "max");
        assert!(body(&sol, Effort::Max).get("max_output_tokens").is_none());
        let openai = builtin("openai", "gpt-6.1-sol");
        assert_eq!(
            body(&openai, Effort::Max)["max_output_tokens"],
            MAX_OUTPUT_TOKENS + 65536
        );
        // Claude Opus 5.5 refuses a thinking budget: adaptive thinking with an effort.
        let claude = builtin("anthropic", "claude-opus-5-5");
        let max = body(&claude, Effort::Max);
        assert_eq!(max["thinking"]["type"], "adaptive");
        assert!(max["thinking"].get("budget_tokens").is_none());
        assert_eq!(max["output_config"]["effort"], "max");
        assert_eq!(max["max_tokens"], MAX_OUTPUT_TOKENS + 65536);
        let label = describe_effort("anthropic", "claude-opus-5-5", Effort::Max)
            .unwrap()
            .0;
        assert!(!label.contains("cap"), "{label}");
        for effort in [Effort::XHigh, Effort::Max] {
            assert_eq!(Effort::parse(effort.as_str()), Some(effort));
        }
        // A model that declares a smaller output limit gets what fits beside the answer.
        let mut small = builtin("deepseek", "deepseek-flash");
        let spec = small.profile.model_specs.get_mut("deepseek-flash").unwrap();
        spec.max_output_tokens = Some(32_768);
        let body = request_body(
            &small,
            &small.model,
            "s",
            false,
            false,
            Effort::Max,
            &prompt,
        )
        .unwrap();
        assert_eq!(body["max_tokens"], 32_768);
        assert!(
            reasoning(&small, Effort::Max)
                .unwrap()
                .label
                .contains("32768-token output limit")
        );
        // A profile-wide default (Dext writes 8,192 for every model) is not a limit.
        let mut defaulted = builtin("glm", "glm-5.3");
        defaulted.profile.model_specs.clear();
        defaulted.profile.model_defaults.max_output_tokens = Some(8_192);
        let body = request_body(
            &defaulted,
            &defaulted.model,
            "s",
            false,
            false,
            Effort::Max,
            &prompt,
        )
        .unwrap();
        assert_eq!(body["thinking"]["budget_tokens"], 65536);
        assert_eq!(body["max_tokens"], MAX_OUTPUT_TOKENS + 65536);
    }

    #[test]
    fn claude_models_get_adaptive_thinking_with_an_effort_and_older_ones_a_budget() {
        for (model, class) in [
            ("claude-opus-5-5", ClaudeThinking::AlwaysOn),
            ("claude-opus-5-5[1m]", ClaudeThinking::AlwaysOn),
            ("anthropic.claude-sonnet-5-5", ClaudeThinking::AlwaysOn),
            ("claude-fable-5-1", ClaudeThinking::AlwaysOn),
            ("claude-mythos-5-1", ClaudeThinking::AlwaysOn),
            ("claude-opus-5", ClaudeThinking::OnByDefault),
            ("claude-sonnet-5", ClaudeThinking::OnByDefault),
            ("claude-haiku-5-5", ClaudeThinking::OnByDefault),
            ("claude-opus-4-8", ClaudeThinking::OffByDefault),
            ("claude-opus-4-7", ClaudeThinking::OffByDefault),
            ("claude-opus-4-6", ClaudeThinking::Adaptive46),
            ("claude-sonnet-4-6", ClaudeThinking::Adaptive46),
            ("claude-haiku-4-5-20251001", ClaudeThinking::Budget),
            ("claude-sonnet-4-5", ClaudeThinking::Budget),
            ("claude-3-7-sonnet-20250219", ClaudeThinking::Budget),
            ("glm-5.3", ClaudeThinking::Budget),
        ] {
            assert_eq!(claude_thinking(model), class, "{model}");
        }
        let prompt = TranscriptCheckpoint {
            route_binding: ROUTE.into(),
            messages: vec![text("user", "go")],
        };
        let body = |route: &Route, effort| {
            request_body(route, &route.model, "s", false, false, effort, &prompt).unwrap()
        };
        let opus = builtin("anthropic", "claude-opus-5-5");
        assert!(binds_thinking(&opus));
        let high = body(&opus, Effort::High);
        assert_eq!(
            high["thinking"],
            json!({"type": "adaptive", "block_binding": {"prefix_mismatch_behavior": "drop_block"}})
        );
        assert_eq!(high["output_config"], json!({"effort": "high"}));
        assert_eq!(high["max_tokens"], MAX_OUTPUT_TOKENS + 16384);
        // Its own default effort, thinking bound; "off" is its lowest effort, never
        // the `disabled` it refuses.
        let default = body(&opus, Effort::Default);
        assert_eq!(default["thinking"]["type"], "adaptive");
        assert!(default.get("output_config").is_none());
        let off = body(&opus, Effort::Off);
        assert_eq!(off["thinking"]["type"], "adaptive");
        assert_eq!(off["output_config"]["effort"], "low");
        // Models that allow it turn thinking off; Opus 4.8 does not think by default.
        let older = builtin("anthropic", "claude-opus-4-8");
        assert_eq!(body(&older, Effort::Off)["thinking"]["type"], "disabled");
        assert!(body(&older, Effort::Default).get("thinking").is_none());
        // The 4.6 models have no xhigh.
        let sonnet46 = builtin("anthropic", "claude-sonnet-4-6");
        assert_eq!(
            body(&sonnet46, Effort::XHigh)["output_config"]["effort"],
            "high"
        );
        // Every level is offered for the picker and the settings tool.
        assert_eq!(
            catalog::valid_efforts("anthropic", "claude-opus-5-5").len(),
            Effort::ALL.len()
        );
        // Older Claude models and Anthropic-compatible providers keep a budget.
        let haiku = builtin("anthropic", "claude-haiku-4-5");
        assert_eq!(body(&haiku, Effort::High)["thinking"]["type"], "enabled");
        assert!(!binds_thinking(&haiku));
        // A subscription body carries no binding (its headers carry no binding beta).
        let shaped: Value =
            serde_json::from_slice(&subscription_body(high.clone(), "go", "s").unwrap()).unwrap();
        assert!(shaped["thinking"].get("block_binding").is_none());
        assert_eq!(shaped["thinking"]["type"], "adaptive");
    }

    #[test]
    fn anthropic_requests_carry_prompt_cache_breakpoints_where_the_model_caches() {
        let transcript = TranscriptCheckpoint {
            route_binding: ROUTE.into(),
            messages: vec![
                text("user", "first"),
                Message {
                    role: "assistant".into(),
                    content: vec![
                        Block::Text { text: "ok".into() },
                        Block::Thinking {
                            text: "hm".into(),
                            signature: Some("sig".into()),
                        },
                    ],
                },
            ],
        };
        let mut claude = builtin("anthropic", "claude-opus-5-5");
        let model = claude.model.to_ascii_lowercase();
        let spec = claude.profile.model_specs.entry(model).or_default();
        spec.capabilities.prompt_cache = Some(true);
        let body = request_body(
            &claude,
            &claude.model,
            "s",
            false,
            false,
            Effort::Default,
            &transcript,
        )
        .unwrap();
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.last().unwrap()["cache_control"]["type"], "ephemeral");
        assert!(tools[0].get("cache_control").is_none());
        // The newest block that can carry a marker: signed thinking cannot.
        let messages = body["messages"].as_array().unwrap();
        let last = messages.last().unwrap()["content"].as_array().unwrap();
        assert!(
            last.iter()
                .all(|block| block["type"] != "thinking" || block.get("cache_control").is_none())
        );
        let marked = messages
            .iter()
            .flat_map(|message| message["content"].as_array().unwrap())
            .filter(|block| block.get("cache_control").is_some())
            .count();
        assert_eq!(marked, 1);
        // Routes whose models do not cache keep the plain shape.
        let mut plain = builtin("anthropic", "claude-opus-5-5");
        plain.profile.model_specs.clear();
        plain.profile.model_defaults.capabilities.prompt_cache = Some(false);
        let body = request_body(
            &plain,
            &plain.model,
            "s",
            false,
            false,
            Effort::Default,
            &transcript,
        )
        .unwrap();
        assert!(body["system"].is_string());
    }

    #[test]
    fn subscription_requests_are_shaped_as_dext_sends_them_and_stay_deterministic() {
        let session = "exec-1234";
        let id = subscription_session(session);
        assert_eq!(
            id,
            subscription_session(session),
            "stable across re-derivation"
        );
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        let body = json!({"model": "claude-opus-5-5", "max_tokens": 10, "system": "spindle system",
            "messages": [{"role": "user", "content": "hi"}], "stream": true});
        let first = subscription_body(body.clone(), "hi", session).unwrap();
        assert_eq!(first, subscription_body(body, "hi", session).unwrap());
        let shaped: Value = serde_json::from_slice(&first).unwrap();
        let system = shaped["system"].as_array().unwrap();
        assert!(system.len() >= 3, "{shaped}");
        assert_eq!(system.last().unwrap()["text"], "spindle system");
    }

    #[test]
    fn stated_output_limits_are_read_from_refusals() {
        let anthropic = r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: 65536 > 64000, which is the maximum allowed number of output tokens for claude-sonnet-4-5-20250929"}}"#;
        assert_eq!(stated_output_limit(400, anthropic, 65536), Some(64000));
        let openai = "max_tokens is too large: 32768. This model supports at most 16384 completion tokens, whereas you provided 32768.";
        assert_eq!(stated_output_limit(400, openai, 32768), Some(16384));
        assert_eq!(stated_output_limit(429, openai, 32768), None);
        assert_eq!(
            stated_output_limit(400, "invalid tool schema 2048", 32768),
            None
        );
        assert_eq!(
            stated_output_limit(
                400,
                "prompt is too long: 250000 tokens > 200000 maximum",
                32768
            ),
            None,
            "a context overflow is not an output limit"
        );
    }

    #[test]
    fn steering_updates_are_not_prompts() {
        let user = |text: &str| Message {
            role: "user".into(),
            content: vec![Block::Text { text: text.into() }],
        };
        assert!(is_prompt(&user("fix the bug")));
        assert!(!is_prompt(&user(
            "[queued-user-update] The user sent this while you were working."
        )));
        assert!(!is_prompt(&user(&format!("{COMPACTED} earlier work"))));
    }

    #[test]
    fn screens_get_the_words_of_a_steering_message_not_its_guidance() {
        use dext_transcript::runtime::{SteeringPoint, WorkPhase, steering_update_text};
        let words = "Need a report\nUser update:\nfor today";
        let wrapped = steering_update_text(
            WorkPhase::Probe,
            2,
            1,
            SteeringPoint::BeforeFinal,
            words,
            "",
        );
        assert_eq!(steered_text(&wrapped), Some(words), "its own marker stays");
        assert_eq!(steered_text("User update:\nnot steering"), None);
        let mut messages = json!([
            {"role": "user", "content": [{"type": "text", "text": wrapped}]},
            {"role": "user", "content": [{"type": "text", "text": "a prompt"}]},
            {"role": "assistant", "content": [{"type": "text", "text": wrapped}]},
        ]);
        annotate_steering(&mut messages);
        assert_eq!(messages[0]["content"][0]["steer"], words);
        assert_eq!(
            messages[0]["content"][0]["text"], wrapped,
            "the model's text stays"
        );
        assert!(messages[1]["content"][0].get("steer").is_none());
        assert!(messages[2]["content"][0].get("steer").is_none());
    }

    #[test]
    fn context_overflow_refusals_are_told_apart_from_other_refusals() {
        for (status, body) in [
            (400, r#"{"error":{"code":"context_length_exceeded"}}"#),
            (400, "prompt is too long: 210000 tokens > 200000 maximum"),
            (400, "This model's maximum context length is 131072 tokens."),
            (413, "Request exceeds max length"),
        ] {
            assert!(context_overflow(status, body), "{body}");
        }
        for (status, body) in [
            (400, "invalid tool schema"),
            (401, "context window"),
            (429, "too many tokens per minute"),
        ] {
            assert!(!context_overflow(status, body), "{status} {body}");
        }
    }

    #[test]
    fn a_max_effort_step_fits_beside_the_largest_history_kept() {
        // Worst case: the whole max allowance is visible reasoning, kept in a history
        // already at its budget and replayed by the next request. The adopting step
        // holds history, request and reply twice; the reserve must cover it.
        let deepseek = builtin("deepseek", "deepseek-flash");
        let budget = history_budget(&deepseek, Effort::Max).unwrap();
        assert!(
            budget > 2 * 1024 * 1024 && budget <= HISTORY_MAX,
            "a 1M-token window keeps megabytes of history: {budget}"
        );
        let glm = builtin("glm", "glm-5.3");
        assert!(history_budget(&glm, Effort::Max).unwrap() >= HISTORY_MIN);
        let reasoning = "x\n".repeat((MAX_OUTPUT_TOKENS + 65536) as usize * 2);
        let mut messages = vec![text("user", "go")];
        while transcript_bytes(&TranscriptCheckpoint {
            route_binding: ROUTE.into(),
            messages: messages.clone(),
        })
        .unwrap()
        .len()
            < budget
        {
            messages.extend(step(messages.len()));
        }
        messages.push(Message {
            role: "assistant".into(),
            content: vec![
                Block::Thinking {
                    text: reasoning,
                    signature: None,
                },
                Block::ToolUse {
                    id: "c".into(),
                    name: "bash".into(),
                    input: json!({"command": "ls"}),
                },
            ],
        });
        let history = TranscriptCheckpoint {
            route_binding: ROUTE.into(),
            messages,
        };
        let next = request_body(
            &deepseek,
            "deepseek-flash",
            "s",
            false,
            false,
            Effort::Max,
            &history,
        )
        .unwrap();
        let held = 2 * transcript_bytes(&history).unwrap().len()
            + 2 * serde_json::to_vec(&next).unwrap().len();
        assert!(
            held + reply_reserve(&deepseek, Effort::Max).unwrap() < crate::MAX_PAYLOAD,
            "{held}"
        );
    }

    #[test]
    fn deepseek_thinking_replays_reasoning_with_tool_calls() {
        let route = builtin("deepseek", "deepseek-flash");
        let transcript = TranscriptCheckpoint {
            route_binding: ROUTE.into(),
            messages: vec![
                Message {
                    role: "user".into(),
                    content: vec![Block::Text {
                        text: "list files".into(),
                    }],
                },
                Message {
                    role: "assistant".into(),
                    content: vec![
                        Block::Thinking {
                            text: "run ls".into(),
                            signature: None,
                        },
                        Block::ToolUse {
                            id: "call-1".into(),
                            name: "bash".into(),
                            input: json!({"command": "ls"}),
                        },
                    ],
                },
                Message {
                    role: "user".into(),
                    content: vec![Block::ToolResult {
                        tool_use_id: "call-1".into(),
                        content: "a".into(),
                        is_error: None,
                        metadata: Default::default(),
                    }],
                },
            ],
        };
        let body = |effort| {
            request_body(
                &route,
                "deepseek-flash",
                "s",
                false,
                false,
                effort,
                &transcript,
            )
            .unwrap()
        };
        let high = body(Effort::High);
        assert_eq!(high["thinking"]["type"], "enabled");
        assert_eq!(high["reasoning_effort"], "high");
        assert_eq!(high["messages"][2]["reasoning_content"], "run ls");
        assert!(captures_reasoning(&serde_json::to_vec(&high).unwrap()));
        assert_eq!(body(Effort::Low)["reasoning_effort"], "low");
        let off = body(Effort::Default);
        assert_eq!(off["thinking"]["type"], "disabled");
        assert!(off["messages"][2].get("reasoning_content").is_none());
        assert!(!captures_reasoning(&serde_json::to_vec(&off).unwrap()));
    }

    #[test]
    fn nested_model_requests_carry_images_system_and_effort_on_every_contract() {
        let png = json!({"media_type":"image/png","data":"iVBORw0KGgo="});
        let input =
            json!({"prompt":"describe","images":[png],"system":"be terse","max_tokens":9000});
        let anthropic = code_model_request(
            &input,
            &builtin("anthropic", "claude-opus-5-5"),
            "s",
            &image_models,
        )
        .unwrap();
        let content = &anthropic["body"]["messages"][0]["content"];
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[1]["text"], "describe");
        assert_eq!(anthropic["body"]["system"], "be terse");
        // Opus 5.5 always thinks: its default effort's allowance rides on top.
        let opus = builtin("anthropic", "claude-opus-5-5");
        let allowance = reasoning(&opus, Effort::Default).unwrap().allowance;
        assert!(allowance > 0);
        assert_eq!(anthropic["body"]["max_tokens"], 9000 + allowance);
        assert!(anthropic["body"].get("tools").is_none());
        let chatgpt =
            code_model_request(&input, &builtin("chatgpt", "gpt-5"), "s", &image_models).unwrap();
        let content = &chatgpt["body"]["input"][0]["content"];
        assert_eq!(content[1]["type"], "input_image");
        assert!(
            content[1]["image_url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert_eq!(chatgpt["body"]["instructions"], "be terse");
        assert!(chatgpt["budget"].as_u64().unwrap() >= 9000);
        let effort = json!({"prompt":"p","effort":"high"});
        let thinking = code_model_request(
            &effort,
            &builtin("anthropic", "claude-opus-5-5"),
            "s",
            &image_models,
        )
        .unwrap();
        assert_eq!(thinking["body"]["max_tokens"], 4096 + 16384);
        assert!(
            thinking["budget"].as_u64().unwrap() >= 4096 + 16384,
            "thinking tokens count against the script allowance"
        );
        let text_only = builtin("glm", "glm-5.3");
        let refused = code_model_request(&input, &text_only, "s", &image_models).unwrap_err();
        assert!(refused.contains("does not accept images"), "{refused}");
        let flash =
            code_model_request(&input, &builtin("glm", "glm-5.3-flash"), "s", &image_models)
                .unwrap();
        assert_eq!(flash["model"], "glm-5.3-flash");
        for (bad, fragment) in [
            (
                json!({"prompt":"p","temperature":1}),
                "unknown option `temperature`",
            ),
            (
                json!({"prompt":"p","images":[{"media_type":"image/bmp","data":"AA=="}]}),
                "media_type must be",
            ),
            (
                json!({"prompt":"p","images":[{"media_type":"image/png","data":"not base64!"}]}),
                "non-empty base64",
            ),
            (json!({"prompt":"p","effort":"ultra"}), "effort must be"),
            (json!({"prompt":"p","max_tokens":0}), "max_tokens must be"),
        ] {
            let error = code_model_request(
                &bad,
                &builtin("anthropic", "claude-opus-5-5"),
                "s",
                &image_models,
            )
            .unwrap_err();
            assert!(error.contains(fragment), "{error}");
        }
    }

    #[test]
    fn settings_replacement_is_atomic_for_observers() {
        let dir = std::env::temp_dir().join(format!("spindle-settings-{}", std::process::id()));
        create_private_dir(&dir).unwrap();
        let mut settings = Settings {
            provider: "fixture".into(),
            model: "old".into(),
            network: false,
            toolchain: false,
            sources: vec![],
            isolation: Isolation::Required,
            effort: Effort::Default,
            compaction: Compaction::Model,
            max_usd: Some(2.5),
            self_edit: false,
            tools: vec![],
            builtin_tools: BuiltinTools::All,
            instructions: None,
            resend_unknown: false,
            tasks: false,
            task_policy: Default::default(),
            runner: Default::default(),
        };
        settings.save(&dir).unwrap();
        assert_eq!(Settings::load(&dir).unwrap().max_usd, Some(2.5));
        std::thread::scope(|scope| {
            let path = &dir;
            let reader = scope.spawn(move || {
                for _ in 0..500 {
                    let current = Settings::load(path).unwrap();
                    assert!(matches!(current.model.as_str(), "old" | "new"));
                }
            });
            for n in 0..50 {
                settings.model = if n % 2 == 0 { "new" } else { "old" }.into();
                settings.save(&dir).unwrap();
            }
            reader.join().unwrap();
        });
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn text(role: &str, text: &str) -> Message {
        Message {
            role: role.into(),
            content: vec![Block::Text { text: text.into() }],
        }
    }

    fn step(index: usize) -> [Message; 2] {
        [
            Message {
                role: "assistant".into(),
                content: vec![Block::ToolUse {
                    id: format!("call-{index}"),
                    name: "bash".into(),
                    input: json!({"command": format!("echo {index}")}),
                }],
            },
            Message {
                role: "user".into(),
                content: vec![Block::ToolResult {
                    tool_use_id: format!("call-{index}"),
                    content: "x".repeat(4096),
                    is_error: Some(index.is_multiple_of(2)),
                    metadata: Default::default(),
                }],
            },
        ]
    }

    #[test]
    fn compaction_keeps_recent_pairs_and_the_current_prompt() {
        let mut messages = vec![text("user", "first task"), text("assistant", "done")];
        messages.push(text("user", "second task"));
        for index in 0..40 {
            messages.extend(step(index));
        }
        let transcript = TranscriptCheckpoint {
            route_binding: ROUTE.into(),
            messages,
        };
        let before = transcript_bytes(&transcript).unwrap().len();
        let compacted = compacted(&transcript, 24 * 1024, None).unwrap();
        let after = transcript_bytes(&compacted).unwrap().len();
        assert!(after < before / 2, "{before} -> {after}");
        let digest = text_of(&compacted.messages[0].content);
        assert!(digest.starts_with(COMPACTED));
        assert!(digest.contains("User asked: first task") && digest.contains("$ echo 0"));
        assert!(!is_prompt(&compacted.messages[0]));
        assert_eq!(text_of(&compacted.messages[1].content), "second task");
        assert_eq!(compacted.messages[2].role, "assistant");
        // Every kept result follows its call.
        let mut calls = std::collections::BTreeSet::new();
        for message in &compacted.messages {
            for block in &message.content {
                match block {
                    Block::ToolUse { id, .. } => {
                        calls.insert(id.clone());
                    }
                    Block::ToolResult { tool_use_id, .. } => assert!(calls.contains(tool_use_id)),
                    _ => {}
                }
            }
        }
        // Re-compaction folds the earlier digest in and stays bounded.
        let mut longer = compacted.clone();
        for index in 40..80 {
            longer.messages.extend(step(index));
        }
        let again = crate::driver::compacted(&longer, 24 * 1024, None).unwrap();
        assert!(text_of(&again.messages[0].content).contains("first task"));
        assert_eq!(text_of(&again.messages[1].content), "second task");
        assert!(transcript_bytes(&again).unwrap().len() < 48 * 1024);
    }

    #[test]
    fn a_prompt_in_the_kept_part_is_not_repeated() {
        let mut messages = vec![text("user", "old task")];
        for index in 0..30 {
            messages.extend(step(index));
        }
        messages.push(text("assistant", "done"));
        messages.push(text("user", "new task"));
        messages.extend(step(99));
        let transcript = TranscriptCheckpoint {
            route_binding: ROUTE.into(),
            messages,
        };
        let compacted = compacted(&transcript, 24 * 1024, None).unwrap();
        let prompts: Vec<_> = compacted
            .messages
            .iter()
            .filter(|message| is_prompt(message))
            .map(|message| text_of(&message.content))
            .collect();
        assert_eq!(prompts, ["new task"]);
        assert!(!text_of(&compacted.messages[0].content).contains(CURRENT_REQUEST));
    }

    #[test]
    fn nothing_to_compact_without_an_earlier_assistant_message() {
        let transcript = TranscriptCheckpoint {
            route_binding: ROUTE.into(),
            messages: vec![text("user", "only")],
        };
        assert!(compacted(&transcript, 24 * 1024, None).is_none());
    }

    #[test]
    fn tool_lines_and_requests_cover_all_workspace_tools() {
        assert_eq!(
            describe_call("bash", &json!({"command": "ls\npwd"})),
            "$ ls\n  ... (1 more lines)"
        );
        assert_eq!(
            describe_call("edit_file", &json!({"path": "a.rs"})),
            "edit a.rs"
        );
        let names: Vec<_> = tool_specs(false, false)
            .into_iter()
            .map(|(name, ..)| name)
            .collect();
        assert_eq!(names, crate::turn::WORKSPACE_TOOLS);
        assert!(
            system(true, false).contains("may use the internet")
                && system(false, false).contains("no internet access")
                && system(false, false).contains("local sockets work")
                && system(false, false).contains("several tool calls at once")
        );
        assert!(system(false, true).contains("Rust toolchain"));
        assert!(tool_specs(false, true)[0].2.to_string().contains("1800"));
        assert!(tool_specs(false, false)[0].2.to_string().contains("1800"));
    }
}
