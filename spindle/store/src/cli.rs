//! `spindle`: a durable coding-agent CLI over the Spindle store. Every step is a
//! committed revision: each model request is sent at most once and its response
//! recorded, bash runs in the strict sandbox over a versioned workspace, and a run
//! that is killed resumes with `spindle continue`. Credentials are read from Dext's
//! login store for each fresh send and never saved or printed.
mod cli_ui;
mod serve;

use dext_transcript::{Block, Message};
use serde_json::Value;
use spindle_store::agent::{
    self, Event, Isolation, Observer, Result, Session, Settings, Status, Target, describe_call,
    runs_process, steered_text,
};
use spindle_store::broker::{KILLED_RUN, UNSEALED_RUN};
use spindle_store::rewind::RewindTargetKind;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Live command preview; recorded output uses a bounded, paged window.
const LIVE_OUTPUT: usize = 4 * 1024;

fn usage() -> &'static str {
    "usage: spindle [--dir DIR] <command>

  new --provider ID [--model M] [--import DIR] [--network] [--toolchain]
      [--source DIR]... [--isolation auto|required|off]
      [--effort default|off|low|medium|high|xhigh|max]
      [--compaction model|digest] [--max-usd USD|none]
      [--attach FILE[=PATH]]... PROMPT                  start a session and run it
  say [--provider ID] [--model M] [--network|--no-network]
      [--toolchain|--no-toolchain] [--source DIR]...
      [--isolation auto|required|off] [--effort LEVEL]
      [--compaction model|digest] [--max-usd USD|none]
      [--attach FILE[=PATH]]... PROMPT                  send the next prompt
  continue [--max-usd USD|none] [--until unknown]       resume an interrupted run
                                                        (--until unknown: stop at the first
                                                        unknown outcome, resending nothing)
  retry [--max-usd USD|none]                            re-run an interrupted model call
  status                                                where the session stands
  usage [--json]                                        spend per turn and per model
  log                                                   the conversation so far
  targets                                               points you can rewind to
  rewind REVISION                                       go back to a target
  files | cat PATH | export DIR                         the versioned workspace
  export-session FILE [--keep]                          move the session into a bundle
                                                        (--keep: copy it, this one stays)
  import-session FILE                                   continue a bundled session in DIR
  serve [--port 8790] [--origin https://HOST] [--dev-source DIR]
      [--access-team TEAM.cloudflareaccess.com --access-aud AUD]
      [--resume known|none]                             mobile web UI on loopback
  token new NAME | list | revoke NAME                   named API tokens for serve's /v1
                                                        HTTP API (DIR is the host directory)
  api                                                   JSON-RPC 2.0 over stdin/stdout
                                                        (docs/API.md)
  code-packs [RUNTIME.json]                             offline discovery/binding candidate
  code-receipts                                         inspect nested effect journal
  providers                                             providers from Dext's catalog
  login [PROVIDER] [KEY|web|import] | logout [PROVIDER] | auth ...
                                                        sign in as `dext auth` does
  doctor                                                check and safely repair a session
  version                                               this build's version and schema
  upgrade (--from SRC | --binary BIN) [--session DIR]... [--to PATH]
                                                        build/verify/install a new spindle
  rollback [--to PATH] [--force]                        restore the binary an upgrade replaced

DIR defaults to $SPINDLE_DIR or ./.spindle. Credentials are Dext's (`spindle login` or `dext auth login`).
--network gives commands internet access until --no-network; what a networked
command does outside the workspace cannot be rewound. --toolchain gives commands
the host Rust toolchain (read-only) with a build cache kept beside the session;
--source DIR (with --toolchain) mounts a host directory read-only at /NAME beside it
(e.g. path dependencies).
Commands run in a bubblewrap sandbox where the host allows it. --isolation auto
(default) runs them directly on the host, and says so, where it does not; required
refuses instead; off never sandboxes (for hosts that isolate spindle themselves).
--max-usd sets the thread's dollar budget (none: unlimited, the default): a model
request whose worst case would pass it is not sent, and Continue sends it once raised."
}

/// `--source` mounts a directory beside the host toolchain only: given for a thread
/// without the toolchain, it would mount nothing.
fn sources_need_toolchain(flags: &Flags, settings: &Settings) -> Result<()> {
    match !flags.sources.is_empty() && !settings.toolchain {
        true => {
            Err("--source mounts a directory beside the host toolchain: add --toolchain".into())
        }
        false => Ok(()),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(message) = run(args) {
        eprintln!("spindle: {message}");
        std::process::exit(1);
    }
}

fn run(mut args: Vec<String>) -> Result<()> {
    let mut dir = std::env::var_os("SPINDLE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".spindle"));
    if args.first().map(String::as_str) == Some("--dir") {
        args.remove(0);
        if args.is_empty() {
            return Err(usage().into());
        }
        dir = PathBuf::from(args.remove(0));
    }
    if args.is_empty() {
        return Err(usage().into());
    }
    let command = args.remove(0);
    // Logins are Dext's own: same flows, same credential store, same messages.
    if command == "auth" || command == "login" || command == "logout" {
        agent::install_host_hooks();
        let mut argv = vec!["auth".to_string()];
        if command != "auth" {
            argv.push(command.clone());
        }
        argv.extend(args);
        return match dext_provider_catalog::provider::handle_auth_cli(&argv) {
            Ok(Some(0)) => Ok(()),
            Ok(Some(code)) => std::process::exit(code),
            Ok(None) => Err(usage().into()),
            Err(error) => Err(format!("{error:#}")),
        };
    }
    if command == "serve" {
        return serve::run(dir, args);
    }
    if command == "api" && args.is_empty() {
        return spindle_store::api::stdio::run();
    }
    if command == "token" {
        return serve::token(dir, args);
    }
    if command == "code-packs" {
        let value = match args.as_slice() {
            [] => spindle_store::code_host::packs()?,
            [manifest] => spindle_store::code_host::pack_candidate(std::path::Path::new(manifest))?,
            _ => return Err("code-packs accepts an optional exact runtime manifest path".into()),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    if command == "code-receipts" && args.is_empty() {
        let mut store =
            spindle_store::Store::open(&dir.join("store")).map_err(|e| e.to_string())?;
        // Child journals, then the session-pack ledger (anchors and turn-end marks).
        let mut records = store.code_receipts("operator").map_err(|e| e.to_string())?;
        records.extend(store.pack_ledger("operator").map_err(|e| e.to_string())?);
        println!(
            "{}",
            serde_json::to_string_pretty(&records).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    let mut flags = Flags::parse(args)?;
    match command.as_str() {
        "new" => {
            let provider = flags.provider.take().ok_or("new needs --provider ID")?;
            let mut settings = Settings::resolve(&provider, flags.model.take())?;
            settings.network = flags.network.unwrap_or(false);
            settings.toolchain = flags.toolchain.unwrap_or(false);
            settings.sources = agent::sources(&flags.sources)?;
            sources_need_toolchain(&flags, &settings)?;
            settings.isolation = flags.isolation.unwrap_or_default();
            settings.effort = flags.effort.unwrap_or_default();
            settings.compaction = flags.compaction.unwrap_or_default();
            settings.max_usd = flags.max_usd.take().flatten();
            agent::describe_effort(&settings.provider, &settings.model, settings.effort)?;
            let prompt = flags.prompt()?;
            if dir.join("store").exists() {
                return Err(format!(
                    "{} already holds a session; use `spindle say` or another --dir",
                    dir.display()
                ));
            }
            let (mut files, imported) = match &flags.import {
                Some(source) => {
                    let (files, skipped) = agent::import_files(source)?;
                    let count = files.len();
                    (files, Some((source, count, skipped)))
                }
                None => (vec![], None),
            };
            for (path, bytes, executable) in flags.attachments()? {
                files.retain(|(kept, _, _)| *kept != path);
                files.push((path, bytes, executable));
            }
            let mut session =
                Session::create(&dir, settings, files, Arc::new(Terminal::default()))?;
            if let Some((source, count, skipped)) = imported {
                println!(
                    "Imported {} file(s) from {}{}",
                    count,
                    source.display(),
                    if skipped > 0 {
                        format!(" ({skipped} skipped: unsupported names, links or large files)")
                    } else {
                        String::new()
                    }
                );
            }
            session.prompt(&prompt)
        }
        "say" => {
            let prompt = flags.prompt()?;
            let mut settings = Settings::load(&dir)?;
            let network = flags.network.unwrap_or(settings.network);
            let toolchain = flags.toolchain.unwrap_or(settings.toolchain);
            let compaction = flags.compaction.unwrap_or(settings.compaction);
            let max_usd = flags.max_usd.take().unwrap_or(settings.max_usd);
            // Another provider or model changes only that: the thread keeps its other
            // settings (an effort the new model does not take goes back to its default).
            let switched = flags.provider.is_some() || flags.model.is_some();
            if let Some(provider) = flags.provider.take() {
                let route = Settings::resolve(&provider, flags.model.take())?;
                // Task models are the thread provider's: another provider's list would
                // refuse every task, so it goes (depth and concurrency stay).
                if route.provider != settings.provider && !settings.task_policy.models.is_empty() {
                    eprintln!(
                        "spindle: tasks.models cleared (it named {} models)",
                        settings.provider
                    );
                    settings.task_policy.models.clear();
                }
                settings.provider = route.provider;
                settings.model = route.model;
            } else if let Some(model) = flags.model.take() {
                settings.model = model;
            }
            if switched
                && flags.effort.is_none()
                && agent::describe_effort(&settings.provider, &settings.model, settings.effort)
                    .is_err()
            {
                eprintln!(
                    "spindle: effort {} is not available on {}; using the default",
                    settings.effort.as_str(),
                    settings.model
                );
                settings.effort = Default::default();
            }
            settings.network = network;
            settings.toolchain = toolchain;
            settings.compaction = compaction;
            settings.max_usd = max_usd;
            if !flags.sources.is_empty() {
                settings.sources = agent::sources(&flags.sources)?;
            }
            sources_need_toolchain(&flags, &settings)?;
            if let Some(effort) = flags.effort {
                settings.effort = effort;
            }
            agent::describe_effort(&settings.provider, &settings.model, settings.effort)?;
            if let Some(isolation) = flags.isolation {
                settings.isolation = isolation;
            }
            // Attachments are read before anything changes, so a bad one changes nothing.
            let files = flags.attachments()?;
            let mut session = Session::open(&dir, Arc::new(Terminal::default()))?;
            // A turn in flight re-derives its next request from the saved config, so
            // the config may change only between turns.
            session.set_settings(settings)?;
            session.prompt_with_files(&prompt, files)
        }
        "continue" => {
            let budget = flags.max_usd.take();
            let until = flags.until.take();
            if until.as_deref().is_some_and(|value| value != "unknown") {
                return Err("--until must be unknown".into());
            }
            flags.none()?;
            let mut session = Session::open(&dir, Arc::new(Terminal::default()))?;
            if let Some(max_usd) = budget {
                session.set_budget(max_usd)?;
            }
            if until.is_some() {
                session.resume_until_unknown()
            } else {
                session.resume()
            }
        }
        "retry" => {
            let budget = flags.max_usd.take();
            flags.none()?;
            let mut session = Session::open(&dir, Arc::new(Terminal::default()))?;
            if let Some(max_usd) = budget {
                session.set_budget(max_usd)?;
            }
            session.retry()
        }
        "status" => {
            flags.none()?;
            let status = Session::open(&dir, Arc::new(Terminal::default()))?.status()?;
            print_status(status);
            Ok(())
        }
        "usage" => {
            let json = flags.words == ["--json"];
            if json {
                flags.words.clear();
            }
            flags.none()?;
            let report = agent::usage_report(&dir)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report.to_json()).map_err(|e| e.to_string())?
                );
            } else {
                print_usage(&report);
            }
            Ok(())
        }
        "log" => {
            flags.none()?;
            let history = Session::open(&dir, Arc::new(Terminal::default()))?.history()?;
            print_log(history);
            Ok(())
        }
        "targets" => {
            flags.none()?;
            let targets = Session::open(&dir, Arc::new(Terminal::default()))?.targets()?;
            print_targets(targets);
            Ok(())
        }
        "rewind" => {
            let target = flags
                .words
                .first()
                .and_then(|word| word.parse::<u64>().ok())
                .filter(|_| flags.words.len() == 1)
                .ok_or("rewind needs one REVISION from `spindle targets`")?;
            Session::open(&dir, Arc::new(Terminal::default()))?.rewind(target)
        }
        "files" => {
            flags.none()?;
            let files = Session::open(&dir, Arc::new(Terminal::default()))?.files()?;
            print_files(files);
            Ok(())
        }
        "cat" => {
            let [path] = flags.words.as_slice() else {
                return Err("cat needs one PATH".into());
            };
            let path = path.clone();
            let mut session = Session::open(&dir, Arc::new(Terminal::default()))?;
            let bytes = session.read(&path)?;
            std::io::stdout()
                .write_all(&bytes)
                .map_err(|error| error.to_string())
        }
        "export" => {
            let [target] = flags.words.as_slice() else {
                return Err("export needs one DIR".into());
            };
            let target = PathBuf::from(target);
            let mut session = Session::open(&dir, Arc::new(Terminal::default()))?;
            let count = session.export(&target)?;
            println!("Exported {} file(s) to {}", count, target.display());
            Ok(())
        }
        "export-session" => {
            let [target] = flags.words.as_slice() else {
                return Err("export-session needs one FILE".into());
            };
            let target = PathBuf::from(target);
            let mut session = Session::open(&dir, Arc::new(Terminal::default()))?;
            let exported = session.export_session(&target, flags.keep)?;
            drop(session);
            println!(
                "Wrote the session (head {}, {} file(s), {} bytes) to {}",
                exported["head"],
                exported["files"],
                exported["size"],
                target.display()
            );
            if flags.keep {
                println!("Kept it here too: continue only one of the two copies.");
            } else {
                println!(
                    "It moved: it continues where the bundle is imported (`spindle --dir NEW \
                     import-session {}`); this copy no longer opens.",
                    target.display()
                );
            }
            Ok(())
        }
        "import-session" => {
            let [source] = flags.words.as_slice() else {
                return Err("import-session needs one FILE".into());
            };
            let imported = agent::import_session(&PathBuf::from(source), &dir)?;
            println!(
                "Imported the session (head {}, {} file(s)) into {}",
                imported["head"],
                imported["files"],
                dir.display()
            );
            Ok(())
        }
        "providers" => print_providers(),
        "doctor" => {
            flags.none()?;
            let findings = agent::doctor(&dir);
            for finding in &findings {
                println!(
                    "{} {:<12} {}",
                    match (finding.ok, finding.warning) {
                        (false, _) => "FAIL",
                        (true, true) => "warn",
                        (true, false) => "ok  ",
                    },
                    finding.check,
                    finding.detail
                );
            }
            if findings.iter().all(|finding| finding.ok) {
                Ok(())
            } else {
                Err("some checks failed".into())
            }
        }
        "version" => {
            flags.none()?;
            println!("{}", version());
            Ok(())
        }
        "upgrade" => upgrade(flags, &dir),
        "rollback" => {
            let target = flags.to.take().map_or_else(current_exe, Ok)?;
            let force = std::mem::take(&mut flags.force);
            flags.none()?;
            spindle_store::upgrade::rollback(&target, force)?;
            println!("Restored the previous binary: {}", version_of(&target));
            Ok(())
        }
        "help" | "--help" | "-h" => {
            println!("{}", usage());
            Ok(())
        }
        other => Err(format!("unknown command `{other}`\n\n{}", usage())),
    }
}

#[derive(Default)]
struct Flags {
    provider: Option<String>,
    model: Option<String>,
    import: Option<PathBuf>,
    network: Option<bool>,
    toolchain: Option<bool>,
    sources: Vec<PathBuf>,
    isolation: Option<Isolation>,
    effort: Option<agent::Effort>,
    compaction: Option<agent::Compaction>,
    /// `--max-usd`: Some(None) clears the budget.
    max_usd: Option<Option<f64>>,
    attach: Vec<String>,
    from: Option<PathBuf>,
    binary: Option<PathBuf>,
    to: Option<PathBuf>,
    sessions: Vec<PathBuf>,
    until: Option<String>,
    force: bool,
    keep: bool,
    words: Vec<String>,
}
impl Flags {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut flags = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
            match arg.as_str() {
                "--provider" => flags.provider = Some(value("--provider")?),
                "--model" => flags.model = Some(value("--model")?),
                "--import" => flags.import = Some(PathBuf::from(value("--import")?)),
                "--network" => flags.network = Some(true),
                "--no-network" => flags.network = Some(false),
                "--toolchain" => flags.toolchain = Some(true),
                "--no-toolchain" => flags.toolchain = Some(false),
                "--source" => flags.sources.push(PathBuf::from(value("--source")?)),
                "--isolation" => {
                    let mode = value("--isolation")?;
                    flags.isolation = Some(Isolation::parse(&mode).ok_or(format!(
                        "--isolation must be auto, required or off, not `{mode}`"
                    ))?);
                }
                "--effort" => {
                    let level = value("--effort")?;
                    flags.effort = Some(agent::Effort::parse(&level).ok_or(format!(
                        "--effort must be default, off, low, medium, high, xhigh or max, not `{level}`"
                    ))?);
                }
                "--compaction" => {
                    let mode = value("--compaction")?;
                    flags.compaction = Some(agent::Compaction::parse(&mode).ok_or(format!(
                        "--compaction must be model or digest, not `{mode}`"
                    ))?);
                }
                "--max-usd" => {
                    let raw = value("--max-usd")?;
                    flags.max_usd = Some(match raw.as_str() {
                        "none" | "off" | "unlimited" => None,
                        _ => Some(
                            raw.trim_start_matches('$')
                                .parse::<f64>()
                                .ok()
                                .filter(|max| max.is_finite() && *max >= 0.0)
                                .ok_or(format!(
                                    "--max-usd must be a dollar amount such as 5 or 0.50, or none, not `{raw}`"
                                ))?,
                        ),
                    });
                }
                "--attach" => flags.attach.push(value("--attach")?),
                "--from" => flags.from = Some(PathBuf::from(value("--from")?)),
                "--binary" => flags.binary = Some(PathBuf::from(value("--binary")?)),
                "--to" => flags.to = Some(PathBuf::from(value("--to")?)),
                "--session" => flags.sessions.push(PathBuf::from(value("--session")?)),
                "--until" => flags.until = Some(value("--until")?),
                "--force" => flags.force = true,
                "--keep" => flags.keep = true,
                _ => flags.words.push(arg),
            }
        }
        Ok(flags)
    }
    /// `--attach FILE` (stored under its file name) or `--attach FILE=PATH`; an
    /// existing file whose name contains `=` is attached under its own name.
    fn attachments(&self) -> Result<Vec<(String, Vec<u8>, bool)>> {
        let mut files = vec![];
        for spec in &self.attach {
            let split = spec
                .split_once('=')
                .filter(|_| !std::path::Path::new(spec).is_file());
            let (source, target) = match split {
                Some((source, target)) => (PathBuf::from(source), target.to_string()),
                None => {
                    let source = PathBuf::from(spec);
                    let name = source
                        .file_name()
                        .and_then(|name| name.to_str())
                        .ok_or(format!("--attach {spec}: not a file name"))?
                        .to_string();
                    (source, name)
                }
            };
            let bytes =
                std::fs::read(&source).map_err(|error| format!("--attach {spec}: {error}"))?;
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(&source)
                    .map(|meta| meta.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            };
            #[cfg(not(unix))]
            let executable = false;
            files.push((target, bytes, executable));
        }
        Ok(files)
    }
    fn prompt(&self) -> Result<String> {
        let prompt = self.words.join(" ");
        if prompt.trim().is_empty() {
            return Err("a PROMPT is required".into());
        }
        Ok(prompt)
    }
    fn none(&self) -> Result<()> {
        if self.words.is_empty()
            && self.provider.is_none()
            && self.model.is_none()
            && self.import.is_none()
            && self.network.is_none()
            && self.toolchain.is_none()
            && self.sources.is_empty()
            && self.isolation.is_none()
            && self.effort.is_none()
            && self.compaction.is_none()
            && self.max_usd.is_none()
            && self.attach.is_empty()
            && self.from.is_none()
            && self.binary.is_none()
            && self.to.is_none()
            && self.sessions.is_empty()
            && self.until.is_none()
            && !self.force
            && !self.keep
        {
            Ok(())
        } else {
            Err(format!("unexpected arguments\n\n{}", usage()))
        }
    }
}

fn version() -> String {
    let build = current_exe()
        .and_then(|exe| spindle_store::upgrade::fingerprint(&exe))
        .unwrap_or_else(|_| "unknown".into());
    format!(
        "spindle {} schema {} engine {} build {build}",
        env!("CARGO_PKG_VERSION"),
        spindle_store::CAPABILITIES.schema,
        spindle_store::turn::ENGINE
    )
}

fn version_of(binary: &std::path::Path) -> String {
    std::process::Command::new(binary)
        .arg("version")
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| binary.display().to_string())
}

fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().map_err(|error| format!("cannot locate this binary: {error}"))
}

/// Build (or take) a candidate, verify it against copies of the given sessions, and
/// install it only if every check passes; the replaced binary stays for `rollback`.
fn upgrade(mut flags: Flags, dir: &std::path::Path) -> Result<()> {
    use spindle_store::upgrade;
    let target = flags.to.take().map_or_else(current_exe, Ok)?;
    let mut sessions = std::mem::take(&mut flags.sessions);
    if sessions.is_empty() && dir.join("store").is_dir() {
        sessions.push(dir.to_path_buf());
    }
    let candidate = match (flags.from.take(), flags.binary.take()) {
        (Some(source), None) => {
            println!("Building {} ...", source.display());
            upgrade::build(&source)?
        }
        (None, Some(binary)) => binary,
        _ => return Err("upgrade needs exactly one of --from SRC or --binary BIN".into()),
    };
    flags.none()?;
    let (version, checks) = upgrade::verify(&candidate, &sessions)?;
    println!("Candidate: {version}");
    for check in &checks {
        println!(
            "{} {}",
            if check.passed { "ok  " } else { "FAIL" },
            check.session.display()
        );
        if !check.passed {
            for line in check.report.lines().take(12) {
                println!("  | {line}");
            }
        }
    }
    if checks.iter().any(|check| !check.passed) {
        return Err("the candidate failed on a session copy; nothing was installed".into());
    }
    let before = version_of(&target);
    upgrade::install(&candidate, &target)?;
    println!(
        "Installed {} (was: {before}). Undo with `spindle rollback`.",
        target.display()
    );
    Ok(())
}

#[derive(Default)]
struct Live {
    shown: usize,
    noted: bool,
    mid_line: bool,
    streamed: bool,
    name: String,
}

#[derive(Default)]
struct Terminal {
    live: Mutex<Live>,
    /// A retry rewinds and then runs on by itself; its rewind needs no next-step hint.
    retrying: std::sync::atomic::AtomicBool,
}

impl Observer for Terminal {
    fn event(&self, event: Event<'_>) {
        match event {
            Event::Prompt { text: prompt } => println!("> {prompt}"),
            Event::Compacted { before, after } => println!(
                "(compacted earlier history: {} KiB -> {} KiB)",
                before / 1024,
                after / 1024
            ),
            Event::ModelStep {
                provider,
                model,
                step,
            } => println!("... {} / {} (step {})", provider, model, step),
            Event::ModelDelta { text } => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(text.as_bytes());
                let _ = out.flush();
            }
            Event::ModelTextWithdrawn => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(b"\n(the provider withdrew the text above)");
                let _ = out.flush();
            }
            Event::ModelText { text } => println!("{}", text.trim_end()),
            Event::Usage { input, output } => {
                println!("    [tokens in {} out {}]", input, output)
            }
            Event::Cost {
                usd,
                turn,
                total,
                billing,
            } => {
                let call = match billing {
                    "subscription" | "unpriced" => billing.to_string(),
                    _ => dollars(usd),
                };
                println!(
                    "    [cost {call} · turn {} · thread {}]",
                    dollars(turn),
                    dollars(total)
                )
            }
            Event::ResponseReused => {
                println!("(reusing the recorded response; nothing was re-sent)")
            }
            Event::ToolCall { name, input, .. } => self.call(name, input),
            Event::ToolSkipped => println!("  | (skipped by the repeat guard)"),
            Event::ToolRefused { reason } => println!("  | (refused: {reason})"),
            Event::ToolReused => println!("  | (reusing the recorded result; not run again)"),
            Event::Unisolated { reason } => {
                println!("(no isolation: {reason}; commands run directly on this host)")
            }
            Event::ToolInterrupted => {
                println!("  | (interrupted when spindle stopped; recorded as not completed)")
            }
            Event::ToolResult { content, failed } => self.tool_result(content, failed),
            Event::Steered { text } => println!("> (while running) {text}"),
            Event::ReplyCut => println!(
                "(cut the reply being written to read your message now; it may still be billed)"
            ),
            Event::ReplyNotCut => {
                println!("(this reply cannot be cut; your message is read when it ends)")
            }
            Event::PackView {
                pack,
                title,
                markdown,
            } => {
                println!("  ┌ {pack}: {title}");
                for line in markdown.lines() {
                    println!("  │ {line}");
                }
            }
            Event::ProviderRetry {
                status,
                seconds,
                attempt,
                ..
            } if status == spindle_store::agent::DROPPED => println!(
                "(the connection to the provider dropped; sending again in {seconds}s, retry {attempt}; the dropped request may have been billed)"
            ),
            Event::ProviderRetry {
                status,
                seconds,
                attempt,
                compacting,
            } => println!(
                "(the provider refused the request with HTTP {status}; {}sending it again, retry {attempt})",
                if compacting {
                    "compacting the history and ".to_string()
                } else if seconds > 0 {
                    format!("waiting {seconds}s and ")
                } else {
                    String::new()
                }
            ),
            Event::Nested { label } => println!("  | ↳ {label}"),
            Event::Output { stream: _, bytes } => self.output(bytes),
            Event::OutputEnd => self.output_end(),
            Event::Stopped {
                reason,
                steps,
                revision,
                files,
                complete,
            } => {
                println!(
                    "\n[{}] after {} model step(s); revision {revision}; workspace: {files} file(s).",
                    reason, steps
                );
                if complete {
                    println!("Next: `spindle say \"...\"`, `spindle files`, or `spindle targets`.");
                } else if reason.contains("step limit") {
                    println!(
                        "Run `spindle continue` for another step budget, or rewind with `spindle targets`."
                    );
                } else {
                    println!(
                        "Rewind to an earlier point with `spindle targets` and `spindle rewind`."
                    );
                }
            }
            Event::Rewound {
                target,
                revision,
                kind,
                abandoned,
            } => {
                println!(
                    "Rewound to revision {target} (now revision {}). Workspace restored.",
                    revision
                );
                if abandoned > 0 {
                    println!(
                        "{} abandoned request(s) can never be adopted or re-sent.",
                        abandoned
                    );
                }
                if self
                    .retrying
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    return;
                }
                match kind {
                    RewindTargetKind::Settled => {
                        println!("Send the next prompt with `spindle say`.")
                    }
                    RewindTargetKind::BeforeModel => {
                        println!("Run `spindle continue` to re-run the next model request.")
                    }
                }
            }
            Event::Retrying {
                target,
                head: revision,
            } => {
                self.retrying
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                println!("Retrying from revision {target} (head was {revision}).")
            }
            Event::RetryingFirst { path } => println!(
                "Retrying the first request in a fresh store; the interrupted one is kept in {}.",
                path.display()
            ),
            Event::NothingToContinue => {
                println!("Nothing to continue: the last turn has finished.")
            }
            Event::Extended { steps } => {
                println!("Continuing past the step limit with {steps} more steps.")
            }
            Event::SettingsApplied { text } => println!("{text}"),
            _ => {}
        }
    }

    /// Pack UI requests are asked on the terminal when stdin is one.
    fn pack_ui(&self) -> Option<Arc<dyn spindle_store::pack_runtime::PackUi>> {
        use std::io::IsTerminal;
        std::io::stdin()
            .is_terminal()
            .then(|| Arc::new(cli_ui::Tty) as Arc<dyn spindle_store::pack_runtime::PackUi>)
    }
}

impl Terminal {
    fn call(&self, name: &str, input: &Value) {
        if let Ok(mut live) = self.live.lock() {
            live.shown = 0;
            live.noted = false;
            live.mid_line = false;
            live.streamed = false;
            live.name.clear();
            live.name.push_str(name);
        }
        println!("{}", describe_call(name, input));
    }

    fn output(&self, bytes: &[u8]) {
        let Ok(mut live) = self.live.lock() else {
            return;
        };
        let mut out = std::io::stdout().lock();
        let room = LIVE_OUTPUT.saturating_sub(live.shown);
        if room > 0 {
            let part = &bytes[..bytes.len().min(room)];
            live.shown += part.len();
            live.streamed |= !part.is_empty();
            for piece in String::from_utf8_lossy(part).split_inclusive('\n') {
                if !live.mid_line {
                    let _ = out.write_all(b"  | ");
                }
                let _ = out.write_all(piece.as_bytes());
                live.mid_line = !piece.ends_with('\n');
            }
        } else if !live.noted {
            live.noted = true;
            if live.mid_line {
                let _ = out.write_all(b"\n");
            }
            let _ = out.write_all(
                b"  | ... (preview capped; the recorded result has a paged output window)\n",
            );
            live.mid_line = false;
        }
        let _ = out.flush();
    }

    fn output_end(&self) {
        let mid_line = match self.live.lock() {
            Ok(mut live) => {
                let mid_line = live.mid_line;
                live.shown = 0;
                live.noted = false;
                live.mid_line = false;
                mid_line
            }
            Err(_) => false,
        };
        if mid_line {
            println!();
        }
    }

    fn tool_result(&self, content: &str, failed: bool) {
        let (streamed, name) = match self.live.lock() {
            Ok(live) => (live.streamed, live.name.clone()),
            Err(_) => return,
        };
        let mut text =
            if streamed && (content.ends_with(KILLED_RUN) || content.ends_with(UNSEALED_RUN)) {
                format!("  | ({})", content.lines().last().unwrap_or_default())
            } else if streamed {
                String::new()
            } else if name == "read_file" && !failed {
                format!("  | ({} lines read)", content.lines().count())
            } else {
                shorten(content, 12)
            };
        if failed {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(if runs_process(&name) {
                "  | (exit status: failure)"
            } else {
                "  | (failed)"
            });
        }
        if !text.is_empty() {
            println!("{text}");
        }
    }
}

fn shorten(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = all
        .iter()
        .take(lines)
        .map(|line| {
            let line: String = line.chars().take(200).collect();
            format!("  | {line}")
        })
        .collect();
    if all.len() > lines {
        out.push(format!("  | ... ({} more lines)", all.len() - lines));
    }
    out.join("\n")
}

fn print_status(status: Status) {
    println!("session    {}", status.session.display());
    println!("provider   {} / {}", status.provider, status.model);
    println!(
        "revision   {} (owner epoch {})",
        status.revision, status.epoch
    );
    println!(
        "prompts    {} in current history (compaction folds older ones)",
        status.prompts
    );
    println!("state      {}", status.state);
    println!("workspace  {} file(s)", status.files);
    println!("isolation  {}", status.isolation.as_str());
    match agent::describe_effort(&status.provider, &status.model, status.effort) {
        Ok((label, _)) => println!("reasoning  {label} (effort {})", status.effort.as_str()),
        Err(error) => println!("reasoning  effort {}: {error}", status.effort.as_str()),
    }
    if let Ok(report) = agent::usage_report(&status.session) {
        let usage = &report.total;
        println!(
            "usage      {} model call(s): {} input ({} cached) / {} output tokens",
            usage.calls,
            usage.input + usage.cache_read + usage.cache_create,
            usage.cache_read,
            usage.output
        );
        println!("cost       {}", cost_line(&report));
        if let Some(max) = Settings::load(&status.session)
            .ok()
            .and_then(|settings| settings.max_usd)
        {
            println!(
                "budget     {} of {} spent",
                dollars(usage.usd),
                dollars(max)
            );
        }
        if usage.unknown > 0 {
            println!(
                "uncertain  {} model request(s) may have been billed (sent, outcome unknown)",
                usage.unknown
            );
        }
    }
    if status.unresolved > 0 {
        println!(
            "unresolved {} step(s) with unknown outcome",
            status.unresolved
        );
    }
    if status.rejected > 0 {
        println!(
            "refused    {} request(s) the provider refused before generating anything",
            status.rejected
        );
    }
    if status.abandoned > 0 {
        println!(
            "abandoned  {} request(s) left behind by rewinds",
            status.abandoned
        );
    }
}

/// Dollars, to the cent when large and to a hundredth of a cent when small.
fn dollars(usd: f64) -> String {
    if usd >= 100.0 {
        format!("${usd:.2}")
    } else {
        format!("${usd:.4}")
    }
}

/// Token counts as 950, 12.3K or 4.1M.
fn count(tokens: u64) -> String {
    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..1_000_000 => format!("{:.1}K", tokens as f64 / 1e3),
        _ => format!("{:.1}M", tokens as f64 / 1e6),
    }
}

/// Priced total with what made it, and the calls no price covers.
fn cost_line(report: &agent::UsageReport) -> String {
    let total = &report.total;
    let mut line = format!(
        "{} (parent {} · nested {} · compaction {})",
        dollars(total.usd),
        dollars(report.parent.usd),
        dollars(report.nested.usd),
        dollars(report.compaction.usd)
    );
    if total.subscription > 0 {
        line.push_str(&format!(
            "; {} subscription call(s), no marginal cost",
            total.subscription
        ));
    }
    if total.unpriced > 0 {
        line.push_str(&format!(
            "; {} call(s) of models without a catalog price",
            total.unpriced
        ));
    }
    if total.refused > 0 {
        line.push_str(&format!(
            "; {} refused request(s), not billed",
            total.refused
        ));
    }
    line
}

/// What a tally holds besides priced calls, for a table's last columns.
fn notes(usage: &agent::Tally) -> String {
    let mut notes = vec![];
    for (count, label) in [
        (usage.subscription, "subscription"),
        (usage.unpriced, "unpriced"),
        (usage.refused, "refused"),
        (usage.unknown, "unknown outcome"),
    ] {
        if count > 0 {
            notes.push(format!("{count} {label}"));
        }
    }
    notes.join(", ")
}

fn print_usage(report: &agent::UsageReport) {
    println!(
        "{:<7} {:>10} {:>6} {:>8} {:>8} {:>8}  PROMPT",
        "TURN", "COST", "CALLS", "INPUT", "CACHED", "OUTPUT"
    );
    for turn in &report.turns {
        let usage = &turn.usage;
        let mut prompt = turn.prompt.clone();
        let mut parts = vec![];
        if turn.nested.calls > 0 {
            parts.push(format!("nested {}", dollars(turn.nested.usd)));
        }
        if turn.compaction.calls > 0 {
            parts.push(format!("compaction {}", dollars(turn.compaction.usd)));
        }
        let notes = notes(usage);
        if !notes.is_empty() {
            parts.push(notes);
        }
        if turn.abandoned {
            parts.push("left by a rewind".into());
        }
        if !parts.is_empty() {
            prompt = format!("{prompt}  [{}]", parts.join("; "));
        }
        println!(
            "r{:<6} {:>10} {:>6} {:>8} {:>8} {:>8}  {prompt}",
            turn.revision,
            dollars(usage.usd),
            usage.calls,
            count(usage.input + usage.cache_create),
            count(usage.cache_read),
            count(usage.output)
        );
    }
    println!();
    println!(
        "{:<40} {:>10} {:>6} {:>8} {:>8} {:>8}",
        "MODEL", "COST", "CALLS", "INPUT", "CACHED", "OUTPUT"
    );
    for model in &report.models {
        let usage = &model.usage;
        let notes = notes(usage);
        println!(
            "{:<40} {:>10} {:>6} {:>8} {:>8} {:>8}{}",
            format!("{}/{}", model.provider, model.model),
            dollars(usage.usd),
            usage.calls,
            count(usage.input + usage.cache_create),
            count(usage.cache_read),
            count(usage.output),
            if notes.is_empty() {
                String::new()
            } else {
                format!("  [{notes}]")
            }
        );
    }
    println!();
    println!("total      {}", cost_line(report));
    if report.total.unknown > 0 {
        println!(
            "uncertain  {} request(s) may have been billed (sent, outcome unknown)",
            report.total.unknown
        );
    }
}

fn print_providers() -> Result<()> {
    for (id, state) in agent::providers()? {
        let state = match state {
            Ok((contract, model)) => format!("ready  {contract}  {model}"),
            Err(why) => format!("unavailable: {why}"),
        };
        println!("{:<18} {state}", id);
    }
    Ok(())
}

fn print_log(history: Vec<Message>) {
    for message in history {
        for block in &message.content {
            match block {
                Block::Text { text } if message.role == "user" => match steered_text(text) {
                    Some(user) => println!("\n> (while running) {user}"),
                    None => println!("\n> {text}"),
                },
                Block::Text { text } | Block::PartialStream { text } => {
                    println!("{}", text.trim_end())
                }
                Block::ToolUse { name, input, .. } => {
                    println!("{}", describe_call(name, input))
                }
                Block::ToolResult { content, .. } => println!("{}", shorten(content, 6)),
                _ => {}
            }
        }
    }
}

fn print_targets(targets: Vec<Target>) {
    if targets.is_empty() {
        println!("No rewind targets yet.");
    }
    for target in targets {
        let kind = match target.kind {
            RewindTargetKind::Settled => "after a finished turn",
            RewindTargetKind::BeforeModel => "before a model request",
        };
        let last = target.last;
        println!("{:>5}  {kind:<24} {last}", target.revision);
    }
}

fn print_files(files: Vec<(String, u64)>) {
    for (path, size) in files {
        println!("{size:>9}  {}", path);
    }
}
