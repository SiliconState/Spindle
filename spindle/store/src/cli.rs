//! `spindle`: a durable coding-agent CLI over the Spindle store. Every step is a
//! committed revision: each model request is sent at most once and its response
//! recorded, bash runs in the strict sandbox over a versioned workspace, and a run
//! that is killed resumes with `spindle continue`. Credentials are read from Dext's
//! login store for each fresh send and never saved or printed.
mod cli_control;
mod cli_diff;
mod cli_ui;
mod serve;

use dext_transcript::{Block, Message};
use serde_json::{Value, json};
use spindle_store::agent::{
    self, Event, Isolation, Observer, Result, Session, Settings, Status, Target, describe_call,
    runs_process, steered_text,
};
use spindle_store::api;
use spindle_store::broker::{KILLED_RUN, STOPPED_RUN, STOPPED_SCRIPT, UNSEALED_RUN};
use spindle_store::rewind::RewindTargetKind;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Live command preview; recorded output uses a bounded, paged window.
const LIVE_OUTPUT: usize = 4 * 1024;

fn usage() -> &'static str {
    "usage: spindle [--dir DIR] [--json] <command> [options]

Work in a session
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
  rewind REVISION                                       go back to a target
  steer [--now] TEXT                                    give the turn another spindle process
                                                        runs a message, read at its next step
                                                        (--now: cut the reply being written)
  stop                                                  stop the turn another process runs:
                                                        its running command ends, recorded

Look (these also work while another process runs a turn)
  status                                                where the session stands
  log [--last] [--revision N]                           the conversation (--last: only what
                                                        the model said last: a finished turn's
                                                        answer)
  files [--revision N]                                  the workspace's files
  cat [--revision N] PATH                               one file
  diff [FROM [TO]] [--stat]                             what changed, as a patch for `git
                                                        apply`: by default what the last turn
                                                        changed; FROM alone: since revision FROM
  usage                                                 spend per turn and per model
  targets                                               points you can rewind to (not while
                                                        a turn runs)
  export DIR                                            the workspace into DIR/workspace and
                                                        its app state into DIR/state.json
  export-session FILE [--keep]                          move the session into a bundle
                                                        (--keep: copy it, this one stays)
  import-session FILE                                   continue a bundled session in DIR

Hosts and tools
  serve [--port 8790] [--origin https://HOST] [--dev-source DIR]
      [--access-team TEAM.cloudflareaccess.com --access-aud AUD]
      [--resume known|none]                             mobile web UI on loopback
  token new NAME [--runner|--browser] | list | revoke NAME
                                                        named API tokens for serve's /v1
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
  help [COMMAND]                                        this text, or one command's

DIR defaults to $SPINDLE_DIR or ./.spindle; --dir and --json go before or after the
command. PROMPT and TEXT are the words given, `-` to read standard input, or
--prompt-file FILE; prompt words that start with -- go after `--`. --json prints one
JSON object on standard output (a turn's narration goes to standard error) with
`next`, the command that goes on from there. Exit status: 0 done, 1 failed, 2 bad
arguments, 130 stopped (Ctrl-C or `spindle stop`: the running command is ended and
recorded, and `spindle continue` goes on; a second Ctrl-C quits at once).
Credentials are Dext's (`spindle login` or `dext auth login`).
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

/// One command's lines of `usage()` (its synopsis and description), or None.
fn command_usage(command: &str) -> Option<String> {
    let lines: Vec<&str> = usage().lines().collect();
    let names = |line: &str| {
        line.strip_prefix("  ").is_some_and(|rest| {
            !rest.starts_with(' ')
                && rest
                    .split(" | ")
                    .any(|part| part == command || part.starts_with(&format!("{command} ")))
        })
    };
    let first = lines.iter().position(|line| names(line))?;
    let rest = lines[first + 1..]
        .iter()
        .take_while(|line| line.starts_with("    "));
    let mut text = format!(
        "usage: spindle [--dir DIR] [--json] <command> [options]\n\n{}",
        lines[first]
    );
    for line in rest {
        text.push('\n');
        text.push_str(line);
    }
    text.push_str("\n\nRun `spindle help` for every command and the options they share.");
    Some(text)
}

/// How a command ends when it does not succeed: its message and exit status.
struct Exit {
    code: i32,
    message: String,
}

impl From<String> for Exit {
    fn from(message: String) -> Self {
        Self { code: 1, message }
    }
}

impl From<&str> for Exit {
    fn from(message: &str) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }
}

/// A command line spindle cannot take: exit status 2, nothing done.
fn bad(message: impl Into<String>) -> Exit {
    Exit {
        code: 2,
        message: message.into(),
    }
}

/// A turn the user stopped (Ctrl-C, `spindle stop`): exit status 130.
const STOPPED: i32 = 130;

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
    let json = args
        .iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--json");
    if let Err(exit) = run(args) {
        if !exit.message.is_empty() {
            // Under --json the failure is a result too, so a caller always gets one.
            if json {
                println!("{}", json!({"error": exit.message, "exit": exit.code}));
            }
            eprintln!("{}", failure_line(&exit.message));
        }
        std::process::exit(exit.code);
    }
}

/// What a failed command prints: its error (which may carry a provider's reply) as text
/// the terminal shows rather than runs (`shown`).
fn failure_line(message: &str) -> String {
    format!("spindle: {}", shown(message))
}

/// What goes around the command on its line: the session directory, `--json` and
/// `--help`, taken wherever they stand (before `--`).
struct Global {
    dir: PathBuf,
    json: bool,
    help: bool,
    command: Option<String>,
    args: Vec<String>,
}

fn global(args: Vec<String>) -> std::result::Result<Global, Exit> {
    let mut global = Global {
        dir: std::env::var_os("SPINDLE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".spindle")),
        json: false,
        help: false,
        command: None,
        args: vec![],
    };
    let mut args = args.into_iter();
    let mut literal = false;
    while let Some(arg) = args.next() {
        // Sign-in commands are Dext's, with their own arguments.
        let passthrough = global
            .command
            .as_deref()
            .is_some_and(|command| matches!(command, "auth" | "login" | "logout"));
        if literal || passthrough {
            global.args.push(arg);
            continue;
        }
        match arg.as_str() {
            "--" => {
                literal = true;
                global.args.push(arg);
            }
            "--dir" => {
                global.dir = args
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| bad("--dir needs a directory"))?;
            }
            "--json" => global.json = true,
            "--help" | "-h" => global.help = true,
            _ if global.command.is_none() && !arg.starts_with('-') => global.command = Some(arg),
            // A one-letter word before the command is no option of spindle's.
            _ if global.command.is_none() && !arg.starts_with("--") => {
                return Err(bad(format!(
                    "unknown option `{arg}`; `spindle help` lists the commands and options"
                )));
            }
            _ => global.args.push(arg),
        }
    }
    Ok(global)
}

/// The commands `--json` applies to.
const JSON_COMMANDS: [&str; 15] = [
    "new",
    "say",
    "continue",
    "retry",
    "rewind",
    "steer",
    "stop",
    "status",
    "log",
    "files",
    "diff",
    "usage",
    "targets",
    "providers",
    "version",
];

fn run(args: Vec<String>) -> std::result::Result<(), Exit> {
    let Global {
        dir,
        json,
        help,
        command,
        args,
    } = global(args)?;
    let Some(command) = command else {
        if help {
            println!("{}", usage());
            return Ok(());
        }
        return Err(bad(usage()));
    };
    if help || command == "help" {
        match (command.as_str(), args.first().map(String::as_str)) {
            ("help", None) => println!("{}", usage()),
            ("help", Some(topic)) | (topic, _) => match command_usage(topic) {
                Some(text) => println!("{text}"),
                None if command == "help" => {
                    return Err(bad(format!(
                        "no command `{topic}`; `spindle help` lists them"
                    )));
                }
                None => println!("{}", usage()),
            },
        }
        return Ok(());
    }
    if json && !JSON_COMMANDS.contains(&command.as_str()) {
        return Err(bad(format!("--json is not offered for `{command}`")));
    }
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
            Ok(None) => Err(bad(usage())),
            Err(error) => Err(format!("{error:#}").into()),
        };
    }
    if command == "serve" {
        return serve::run(dir, args).map_err(Exit::from);
    }
    if (command == "api" || command == "code-receipts") && !args.is_empty() {
        return Err(bad(format!("`{command}` takes no arguments")));
    }
    if command == "api" {
        return spindle_store::api::stdio::run().map_err(Exit::from);
    }
    if command == "token" {
        return serve::token(dir, args).map_err(Exit::from);
    }
    if command == "code-packs" {
        let value = match args.as_slice() {
            [] => spindle_store::code_host::packs()?,
            [manifest] => spindle_store::code_host::pack_candidate(std::path::Path::new(manifest))?,
            _ => {
                return Err(bad(
                    "code-packs accepts an optional exact runtime manifest path",
                ));
            }
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    if command == "code-receipts" {
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
    let mut flags = Flags::parse(&command, args)?;
    // A turn's session is let go of first, so its result is read as others see it.
    let turn = |outcome: Result<()>, terminal: &Terminal, unsent: Vec<String>| {
        finish_turn(&dir, json, outcome, terminal, unsent)
    };
    match command.as_str() {
        "new" => {
            let provider = flags
                .provider
                .take()
                .ok_or_else(|| bad("new needs --provider ID"))?;
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
                )
                .into());
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
            let terminal = Arc::new(Terminal::new(json));
            let mut session = Session::create(&dir, settings, files, terminal.clone())?;
            if let Some((source, count, skipped)) = imported {
                terminal.line(format!(
                    "Imported {} file(s) from {}{}",
                    count,
                    source.display(),
                    if skipped > 0 {
                        format!(" ({skipped} skipped: unsupported names, links or large files)")
                    } else {
                        String::new()
                    }
                ));
            }
            let listener = listen(&dir, &session);
            let outcome = session.prompt(&prompt);
            drop(listener);
            let unsent = session.take_steering();
            drop(session);
            turn(outcome, &terminal, unsent)
        }
        "say" => {
            let prompt = flags.prompt()?;
            // The settings the user names (a queued change to them gives way).
            let named: Vec<&str> = [
                ("provider", flags.provider.is_some()),
                ("model", flags.model.is_some()),
                ("effort", flags.effort.is_some()),
                ("network", flags.network.is_some()),
                ("toolchain", flags.toolchain.is_some()),
                ("compaction", flags.compaction.is_some()),
                ("max_usd", flags.max_usd.is_some()),
            ]
            .into_iter()
            .filter_map(|(key, given)| given.then_some(key))
            .collect();
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
            let terminal = Arc::new(Terminal::new(json));
            let mut session = open(&dir, terminal.clone())?;
            // A turn in flight re-derives its next request from the saved config, so
            // the config may change only between turns. A turn stopped part way takes the
            // message as it is set up (unchanged settings are not saved again).
            if session.accepts_prompt() || settings.to_json()? != Settings::load(&dir)?.to_json()? {
                session.set_chosen_settings(settings, &named)?;
            }
            let listener = listen(&dir, &session);
            let outcome = session.prompt_with_files(&prompt, files);
            drop(listener);
            let unsent = session.take_steering();
            drop(session);
            turn(outcome, &terminal, unsent)
        }
        "continue" => {
            let budget = flags.max_usd.take();
            let until = flags.until.take();
            if until.as_deref().is_some_and(|value| value != "unknown") {
                return Err(bad("--until must be unknown"));
            }
            flags.none()?;
            let terminal = Arc::new(Terminal::new(json));
            let mut session = open(&dir, terminal.clone())?;
            if let Some(max_usd) = budget {
                session.set_budget(max_usd)?;
            }
            let listener = listen(&dir, &session);
            let outcome = if until.is_some() {
                session.resume_until_unknown()
            } else {
                session.resume()
            };
            drop(listener);
            let unsent = session.take_steering();
            drop(session);
            turn(outcome, &terminal, unsent)
        }
        "retry" => {
            let budget = flags.max_usd.take();
            flags.none()?;
            let terminal = Arc::new(Terminal::new(json));
            let mut session = open(&dir, terminal.clone())?;
            if let Some(max_usd) = budget {
                session.set_budget(max_usd)?;
            }
            // A retry runs on in a session it may make anew, with these same handles.
            let listener = listen(&dir, &session);
            let steering = session.steering_handle();
            let outcome = session.retry();
            drop(listener);
            let unsent = std::mem::take(
                &mut *steering
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            turn(outcome, &terminal, unsent)
        }
        "rewind" => {
            let target = flags
                .words
                .first()
                .and_then(|word| word.trim_start_matches('r').parse::<u64>().ok())
                .filter(|_| flags.words.len() == 1)
                .ok_or_else(|| bad("rewind needs one REVISION from `spindle targets`"))?;
            let terminal = Arc::new(Terminal::new(json));
            let outcome =
                open(&dir, terminal.clone()).and_then(|mut session| session.rewind(target));
            turn(outcome, &terminal, vec![])
        }
        "steer" | "stop" => {
            let text = match command.as_str() {
                "steer" => {
                    let text = flags.prompt()?;
                    if text.len() > agent::MAX_STEER {
                        return Err(bad(format!(
                            "a message for a running turn is at most {} KiB; send a longer one with `spindle say` once the turn ends",
                            agent::MAX_STEER / 1024
                        )));
                    }
                    Some(text)
                }
                _ => {
                    flags.none()?;
                    None
                }
            };
            let sent = cli_control::send(&dir, text.as_deref(), flags.now)?;
            let (done, said) = match (&sent, text.is_some()) {
                (cli_control::Sent::Taken, true) => (
                    true,
                    "Given to the running turn: it reads the message at its next step.".to_string(),
                ),
                (cli_control::Sent::Taken, false) => (
                    true,
                    "Asked the running turn to stop: its running command ends and is recorded."
                        .to_string(),
                ),
                (cli_control::Sent::NotRunning, true) => (
                    false,
                    format!(
                        "no turn is running in {}; send it with `spindle say`",
                        dir.display()
                    ),
                ),
                (cli_control::Sent::NotRunning, false) => {
                    (false, format!("no turn is running in {}", dir.display()))
                }
                (cli_control::Sent::Elsewhere, _) => (
                    false,
                    "the session is held by a process that takes no messages from here \
                     (`spindle serve` or `spindle api`): send it through that one"
                        .to_string(),
                ),
                (cli_control::Sent::NotTaken, _) => (
                    false,
                    "the running process did not take it in time (its turn may be ending, or \
                     32 messages already wait); nothing was sent"
                        .to_string(),
                ),
            };
            if json {
                println!(
                    "{}",
                    json!({"session": dir, "taken": done, "message": said})
                );
                return match done {
                    true => Ok(()),
                    false => Err(Exit {
                        code: 1,
                        message: String::new(),
                    }),
                };
            }
            match done {
                true => {
                    println!("{said}");
                    Ok(())
                }
                false => Err(said.into()),
            }
        }
        "status" => {
            flags.none()?;
            let status = status_json(&dir)?;
            if json {
                println!("{}", pretty(&status)?);
            } else {
                print_status(&status);
            }
            Ok(())
        }
        "usage" => {
            flags.none()?;
            let report = agent::usage_report(&dir)?;
            if json {
                println!("{}", pretty(&report.to_json())?);
            } else {
                print_usage(&report);
            }
            Ok(())
        }
        "log" => {
            flags.none()?;
            let (head, revision, history) = api::history_at(&dir, flags.revision)?;
            if flags.last {
                let answer = last_answer(&history);
                match json {
                    true => println!(
                        "{}",
                        pretty(&json!({"head": head, "revision": revision, "text": answer}))?
                    ),
                    false => println!("{}", shown(answer.as_deref().unwrap_or_default())),
                }
                return Ok(());
            }
            if json {
                println!("{}", pretty(&api::messages_at(&dir, flags.revision, 0)?)?);
            } else {
                print_log(history);
            }
            Ok(())
        }
        "targets" => {
            flags.none()?;
            let targets = open(&dir, Arc::new(Terminal::new(json)))?.targets()?;
            if json {
                let list: Vec<Value> = targets
                    .iter()
                    .map(|target| {
                        json!({
                            "revision": target.revision,
                            "kind": match target.kind {
                                RewindTargetKind::Settled => "settled",
                                RewindTargetKind::BeforeModel => "before_model",
                            },
                            "last": target.last,
                        })
                    })
                    .collect();
                println!("{}", pretty(&json!({ "targets": list }))?);
            } else {
                print_targets(targets);
            }
            Ok(())
        }
        "files" => {
            flags.none()?;
            let form = api::files_at(&dir, flags.revision)?;
            if json {
                println!("{}", pretty(&form)?);
            } else {
                print_files(&form);
            }
            Ok(())
        }
        "cat" => {
            let [path] = flags.words.as_slice() else {
                return Err(bad("cat needs one PATH"));
            };
            let bytes = api::read_at(&dir, flags.revision, path)?;
            std::io::stdout()
                .write_all(&bytes)
                .map_err(|error| Exit::from(error.to_string()))
        }
        "diff" => {
            let revisions: Vec<u64> = flags
                .words
                .iter()
                .map(|word| word.trim_start_matches('r').parse::<u64>())
                .collect::<std::result::Result<_, _>>()
                .map_err(|_| {
                    bad("diff takes revisions, such as `spindle diff 12` or `diff 12 20`")
                })?;
            let (from, to) = match revisions.as_slice() {
                [] => (last_turn_start(&dir)?, None),
                [from] => (*from, None),
                [from, to] => (*from, Some(*to)),
                _ => return Err(bad("diff takes at most two revisions (FROM and TO)")),
            };
            let (from, to, patch) = cli_diff::diff(&dir, from, to)?;
            if json {
                println!("{}", pretty(&cli_diff::json(from, to, &patch))?);
                return Ok(());
            }
            if flags.stat {
                print!("{}", cli_diff::stat_lines(&patch.stats));
            } else {
                print!("{}", patch.text);
            }
            for (path, why) in &patch.skipped {
                match *why {
                    "binary" => eprintln!(
                        "spindle: {path} is binary, so not in the patch; `spindle cat {path} > FILE` copies it"
                    ),
                    why => eprintln!("spindle: {path} is not in the patch ({why})"),
                }
            }
            if patch.stats.is_empty() && patch.skipped.is_empty() {
                eprintln!("spindle: nothing changed from revision {from} to {to}");
            }
            Ok(())
        }
        "export" => {
            let [target] = flags.words.as_slice() else {
                return Err(bad("export needs one DIR"));
            };
            let target = PathBuf::from(target);
            let mut session = open(&dir, Arc::new(Terminal::new(false)))?;
            let count = session.export(&target)?;
            println!(
                "Exported {} file(s) to {} (the app state to {})",
                count,
                target.join("workspace").display(),
                target.join("state.json").display()
            );
            Ok(())
        }
        "export-session" => {
            let [target] = flags.words.as_slice() else {
                return Err(bad("export-session needs one FILE"));
            };
            let target = PathBuf::from(target);
            let mut session = open(&dir, Arc::new(Terminal::new(false)))?;
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
                return Err(bad("import-session needs one FILE"));
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
        "providers" => {
            flags.none()?;
            match json {
                true => println!(
                    "{}",
                    pretty(&json!({ "providers": api::providers_json()? }))?
                ),
                false => print_providers()?,
            }
            Ok(())
        }
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
            match json {
                true => println!(
                    "{}",
                    json!({
                        "version": env!("CARGO_PKG_VERSION"),
                        "schema": spindle_store::CAPABILITIES.schema,
                        "engine": spindle_store::turn::ENGINE,
                        "build": build(),
                    })
                ),
                false => println!("{}", version()),
            }
            Ok(())
        }
        "upgrade" => upgrade(flags, &dir).map_err(Exit::from),
        "rollback" => {
            let target = flags.to.take().map_or_else(current_exe, Ok)?;
            let force = std::mem::take(&mut flags.force);
            flags.none()?;
            spindle_store::upgrade::rollback_checking(&target, force, &[dir.as_path()])?;
            println!("Restored the previous binary: {}", version_of(&target));
            Ok(())
        }
        other => Err(bad(format!(
            "unknown command `{other}`; `spindle help` lists the commands"
        ))),
    }
}

/// Opens the session for a command that changes it; one another process runs is
/// refused with what can be done meanwhile.
fn open(dir: &Path, observer: Arc<Terminal>) -> Result<Session> {
    Session::open(dir, observer).map_err(|error| {
        if error != agent::LOCKED || !api::held(dir) {
            return error;
        }
        match cli_control::listening(dir) {
            true => "another process is running this session's turn: watch it with `spindle \
                     status` or `spindle log`, give it a message with `spindle steer`, or stop \
                     it with `spindle stop`"
                .to_string(),
            false => "another process holds this session (`spindle serve`, `spindle api` or \
                      another command): wait for it, or work through that host; `spindle \
                      status` and `spindle log` read it meanwhile"
                .to_string(),
        }
    })
}

/// Lets other processes reach the turn `session` is about to run (`cli_control`), and
/// Ctrl-C stop it.
fn listen(dir: &Path, session: &Session) -> cli_control::Listener {
    cli_control::catch_interrupts();
    cli_control::Listener::start(
        dir,
        cli_control::Handles {
            steering: session.steering_handle(),
            hurry: session.hurry_handle(),
            interrupt: session.interrupt_handle(),
        },
        |note| eprintln!("{note}"),
    )
}

/// How a turn command ends: messages its run never read are said, then the result (one
/// JSON object with `--json`), and the exit status: 130 for a stop the user asked for.
fn finish_turn(
    dir: &Path,
    json: bool,
    outcome: Result<()>,
    terminal: &Terminal,
    unsent: Vec<String>,
) -> std::result::Result<(), Exit> {
    for text in &unsent {
        terminal.line(format!(
            "(the turn ended before reading your message: {}; send it with `spindle say`)",
            shown(text)
        ));
    }
    // A stop counts when it reached the turn and the turn did not finish anyway.
    let stopped = cli_control::stop_reached_turn()
        && api::head_state(dir).is_ok_and(|head| head["complete"] != true);
    if json {
        let mut result = terminal.result(dir);
        result["unsent"] = json!(unsent);
        result["stopped_by_user"] = json!(stopped);
        if let Err(error) = &outcome {
            result["error"] = json!(error);
            eprintln!("{}", failure_line(error));
        }
        println!("{}", pretty(&result)?);
        return match (outcome, stopped) {
            (_, true) => Err(Exit {
                code: STOPPED,
                message: String::new(),
            }),
            (Ok(()), false) => Ok(()),
            (Err(_), false) => Err(Exit {
                code: 1,
                message: String::new(),
            }),
        };
    }
    if stopped {
        if let Err(error) = &outcome {
            eprintln!("{}", failure_line(error));
        }
        let head = api::head_state(dir).unwrap_or(Value::Null);
        let (next, why) = next_step(dir, &head);
        let command = match next.as_str() {
            "say" => "spindle say \"...\"".to_string(),
            other => format!("spindle {other}"),
        };
        terminal.line(format!(
            "\nStopped. Next: `{command}`{}.",
            why.map(|why| format!(" ({why})")).unwrap_or_default()
        ));
        return Err(Exit {
            code: STOPPED,
            message: String::new(),
        });
    }
    outcome.map_err(Exit::from)
}

/// The revision the last turn started at (its prompt's), for `diff`'s default.
fn last_turn_start(dir: &Path) -> Result<u64> {
    let report = agent::usage_report(dir)?;
    report
        .turns
        .iter()
        .rev()
        .find(|turn| !turn.abandoned)
        .map(|turn| turn.revision)
        .ok_or_else(|| "no turn has run yet; nothing to compare".into())
}

/// The final answer: the text of the last assistant message that has any.
fn last_answer(history: &[Message]) -> Option<String> {
    history
        .iter()
        .rev()
        .filter(|message| message.role == "assistant")
        .map(|message| {
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    Block::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .find(|text| !text.trim().is_empty())
}

fn pretty(value: &Value) -> Result<String> {
    serde_json::to_string_pretty(value).map_err(|error| error.to_string())
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
    prompt_file: Option<PathBuf>,
    from: Option<PathBuf>,
    binary: Option<PathBuf>,
    to: Option<PathBuf>,
    sessions: Vec<PathBuf>,
    until: Option<String>,
    revision: Option<u64>,
    force: bool,
    keep: bool,
    now: bool,
    stat: bool,
    last: bool,
    words: Vec<String>,
}

/// The options each command takes; anything else is refused, never ignored.
fn options_of(command: &str) -> &'static [&'static str] {
    match command {
        "new" => &[
            "--provider",
            "--model",
            "--import",
            "--network",
            "--toolchain",
            "--source",
            "--isolation",
            "--effort",
            "--compaction",
            "--max-usd",
            "--attach",
            "--prompt-file",
        ],
        "say" => &[
            "--provider",
            "--model",
            "--network",
            "--no-network",
            "--toolchain",
            "--no-toolchain",
            "--source",
            "--isolation",
            "--effort",
            "--compaction",
            "--max-usd",
            "--attach",
            "--prompt-file",
        ],
        "continue" => &["--max-usd", "--until"],
        "retry" => &["--max-usd"],
        "steer" => &["--now", "--prompt-file"],
        "log" => &["--revision", "--last"],
        "files" | "cat" => &["--revision"],
        "diff" => &["--stat"],
        "export-session" => &["--keep"],
        "upgrade" => &["--from", "--binary", "--to", "--session"],
        "rollback" => &["--to", "--force"],
        _ => &[],
    }
}

impl Flags {
    fn parse(command: &str, args: Vec<String>) -> std::result::Result<Self, Exit> {
        let mut flags = Self::default();
        let allowed = options_of(command);
        let mut args = args.into_iter();
        let mut literal = false;
        while let Some(arg) = args.next() {
            // Only `--` words are options (spindle has no one-letter ones): `-5` or `-v`
            // in a prompt are words.
            if literal || !arg.starts_with("--") {
                flags.words.push(arg);
                continue;
            }
            if arg == "--" {
                literal = true;
                continue;
            }
            if !allowed.contains(&arg.as_str()) {
                let known = KNOWN.contains(&arg.as_str());
                return Err(bad(match known {
                    true => format!(
                        "`{arg}` does not apply to `{command}`; `spindle help {command}` shows what does"
                    ),
                    false => format!(
                        "unknown option `{arg}`; `spindle help {command}` shows the options (prompt words that start with -- go after `--`)"
                    ),
                }));
            }
            let mut value = |name: &str| {
                args.next()
                    .ok_or_else(|| bad(format!("{name} needs a value")))
            };
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
                    flags.isolation = Some(Isolation::parse(&mode).ok_or_else(|| {
                        bad(format!(
                            "--isolation must be auto, required or off, not `{mode}`"
                        ))
                    })?);
                }
                "--effort" => {
                    let level = value("--effort")?;
                    flags.effort = Some(agent::Effort::parse(&level).ok_or_else(|| {
                        bad(format!(
                            "--effort must be default, off, low, medium, high, xhigh or max, not `{level}`"
                        ))
                    })?);
                }
                "--compaction" => {
                    let mode = value("--compaction")?;
                    flags.compaction = Some(agent::Compaction::parse(&mode).ok_or_else(|| {
                        bad(format!(
                            "--compaction must be model or digest, not `{mode}`"
                        ))
                    })?);
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
                                .ok_or_else(|| bad(format!(
                                    "--max-usd must be a dollar amount such as 5 or 0.50, or none, not `{raw}`"
                                )))?,
                        ),
                    });
                }
                "--attach" => flags.attach.push(value("--attach")?),
                "--prompt-file" => flags.prompt_file = Some(PathBuf::from(value("--prompt-file")?)),
                "--from" => flags.from = Some(PathBuf::from(value("--from")?)),
                "--binary" => flags.binary = Some(PathBuf::from(value("--binary")?)),
                "--to" => flags.to = Some(PathBuf::from(value("--to")?)),
                "--session" => flags.sessions.push(PathBuf::from(value("--session")?)),
                "--until" => flags.until = Some(value("--until")?),
                "--revision" => {
                    let raw = value("--revision")?;
                    flags.revision = Some(raw.trim_start_matches('r').parse().map_err(|_| {
                        bad(format!("--revision takes a revision number, not `{raw}`"))
                    })?);
                }
                "--force" => flags.force = true,
                "--keep" => flags.keep = true,
                "--now" => flags.now = true,
                "--stat" => flags.stat = true,
                "--last" => flags.last = true,
                _ => unreachable!("every allowed option is handled"),
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
    /// The prompt: the words, standard input for `-`, or `--prompt-file`.
    fn prompt(&self) -> std::result::Result<String, Exit> {
        let prompt = match (&self.prompt_file, self.words.as_slice()) {
            (Some(_), [_, ..]) => {
                return Err(bad("give the prompt as words or --prompt-file, not both"));
            }
            (Some(file), []) => std::fs::read_to_string(file)
                .map_err(|error| bad(format!("--prompt-file {}: {error}", file.display())))?,
            (None, [dash]) if dash == "-" => {
                let mut text = String::new();
                std::io::stdin()
                    .read_to_string(&mut text)
                    .map_err(|error| {
                        bad(format!("reading the prompt from standard input: {error}"))
                    })?;
                text
            }
            (None, words) => words.join(" "),
        };
        if prompt.trim().is_empty() {
            return Err(bad(
                "a PROMPT is required: words, `-` for standard input, or --prompt-file FILE",
            ));
        }
        Ok(prompt)
    }
    fn none(&self) -> std::result::Result<(), Exit> {
        match self.words.first() {
            None => Ok(()),
            Some(word) => Err(bad(format!(
                "unexpected `{word}`; `spindle help` shows what each command takes"
            ))),
        }
    }
}

/// Every option some command takes (`options_of`).
const KNOWN: [&str; 28] = [
    "--provider",
    "--model",
    "--import",
    "--network",
    "--no-network",
    "--toolchain",
    "--no-toolchain",
    "--source",
    "--isolation",
    "--effort",
    "--compaction",
    "--max-usd",
    "--attach",
    "--prompt-file",
    "--from",
    "--binary",
    "--to",
    "--session",
    "--until",
    "--revision",
    "--force",
    "--keep",
    "--now",
    "--stat",
    "--last",
    "--json",
    "--dir",
    "--help",
];

fn build() -> String {
    current_exe()
        .and_then(|exe| spindle_store::upgrade::fingerprint(&exe))
        .unwrap_or_else(|_| "unknown".into())
}

fn version() -> String {
    format!(
        "spindle {} schema {} engine {} build {}",
        env!("CARGO_PKG_VERSION"),
        spindle_store::CAPABILITIES.schema,
        spindle_store::turn::ENGINE,
        build()
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
    flags.none().map_err(|exit| exit.message)?;
    let (version, checks) =
        upgrade::verify(&candidate, &sessions, &upgrade_scratch(dir, &sessions))?;
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

/// Where `upgrade` copies the sessions its candidate is checked on: beside them, not in
/// a temp directory that may be memory. A host directory's `promotions/` (where its own
/// promotions check candidates, and its start removes what a stopped check left), else
/// the directory of the first session.
fn upgrade_scratch(dir: &std::path::Path, sessions: &[PathBuf]) -> PathBuf {
    if dir.join("host.sqlite").is_file() {
        return dir.join("promotions");
    }
    match sessions
        .first()
        .and_then(|session| session.parent())
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => parent.to_path_buf(),
        None if dir.is_dir() => dir.to_path_buf(),
        None => PathBuf::from("."),
    }
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
    /// `--json`: the narration goes to stderr, stdout holds the result alone.
    narrate_to_stderr: bool,
    /// What the turn came to, for `--json`'s result.
    outcome: Mutex<Outcome>,
}

#[derive(Default)]
struct Outcome {
    stopped: Option<Value>,
    rewound: Option<Value>,
    /// The last cost line: this turn's and the thread's dollars.
    cost: Option<(f64, f64)>,
}

impl Observer for Terminal {
    fn event(&self, event: Event<'_>) {
        match event {
            Event::Prompt { text: prompt } => self.line(format!("> {}", shown(&prompt_shown(prompt)))),
            Event::Compacted { before, after } => self.line(format!(
                "(compacted earlier history: {} KiB -> {} KiB)",
                before / 1024,
                after / 1024
            )),
            Event::ModelStep {
                provider,
                model,
                step,
            } => self.line(format!("... {} / {} (step {})", provider, model, step)),
            Event::ModelDelta { text } => self.raw(shown(text).as_bytes()),
            Event::ModelTextWithdrawn => self.raw(b"\n(the provider withdrew the text above)"),
            Event::ModelText { text } => self.line(format!("{}", shown(text.trim_end()))),
            Event::Usage { input, output } => {
                self.line(format!("    [tokens in {} out {}]", input, output))
            }
            Event::Cost {
                usd,
                turn,
                total,
                billing,
            } => {
                if let Ok(mut outcome) = self.outcome.lock() {
                    outcome.cost = Some((turn, total));
                }
                let call = match billing {
                    "subscription" | "unpriced" => billing.to_string(),
                    _ => dollars(usd),
                };
                self.line(format!(
                    "    [cost {call} · turn {} · thread {}]",
                    dollars(turn),
                    dollars(total)
                ))
            }
            Event::ResponseReused => {
                self.line("(reusing the recorded response; nothing was re-sent)")
            }
            Event::ToolCall { name, input, .. } => self.call(name, input),
            Event::ToolSkipped => self.line("  | (skipped by the repeat guard)"),
            Event::ToolRefused { reason } => self.line(format!("  | (refused: {})", shown(reason))),
            Event::ToolReused => self.line("  | (reusing the recorded result; not run again)"),
            Event::Unisolated { reason } => {
                self.line(format!("(no isolation: {reason}; commands run directly on this host)"))
            }
            Event::ToolInterrupted => {
                self.line("  | (interrupted when spindle stopped; recorded as not completed)")
            }
            Event::ToolResult { content, failed } => self.tool_result(content, failed),
            Event::Steered { text } => self.line(format!("> (while running) {}", shown(text))),
            Event::ReplyCut => self.line("(cut the reply being written to read your message now; it may still be billed)"),
            Event::ReplyNotCut => {
                self.line("(this reply cannot be cut; your message is read when it ends)")
            }
            Event::PackView {
                pack,
                title,
                markdown,
            } => {
                self.line(format!("  ┌ {}: {}", shown(pack), shown(title)));
                for line in markdown.lines() {
                    self.line(format!("  │ {}", shown(line)));
                }
            }
            Event::ProviderRetry {
                status,
                seconds,
                attempt,
                ..
            } if status == spindle_store::agent::UNREACHED => self.line(format!(
                "(could not connect to the provider, so nothing was sent; trying again in {seconds}s, retry {attempt})"
            )),
            Event::ProviderRetry {
                status,
                seconds,
                attempt,
                ..
            } if status == spindle_store::agent::DROPPED => self.line(format!(
                "(the connection to the provider dropped; sending again in {seconds}s, retry {attempt}; the dropped request may have been billed)"
            )),
            Event::ProviderRetry {
                status,
                seconds,
                attempt,
                compacting,
            } => self.line(format!(
                "(the provider refused the request with HTTP {status}; {}sending it again, retry {attempt})",
                if compacting {
                    "compacting the history and ".to_string()
                } else if seconds > 0 {
                    format!("waiting {seconds}s and ")
                } else {
                    String::new()
                }
            )),
            Event::Nested { label } => self.line(format!("  | ↳ {}", shown(label))),
            Event::Output { stream: _, bytes } => self.output(bytes),
            Event::OutputEnd => self.output_end(),
            Event::Stopped {
                reason,
                steps,
                revision,
                files,
                complete,
            } => {
                if let Ok(mut outcome) = self.outcome.lock() {
                    outcome.stopped = Some(json!({
                        "reason": reason, "steps": steps, "revision": revision,
                        "files": files, "complete": complete,
                    }));
                }
                self.line(format!(
                    "\n[{}] after {} model step(s); revision {revision}; workspace: {files} file(s).",
                    reason, steps
                ));
                if complete {
                    self.line("Next: `spindle say \"...\"`, `spindle files`, or `spindle targets`.");
                } else if reason.contains("step limit") {
                    self.line("Run `spindle continue` for another step budget, or rewind with `spindle targets`.");
                } else {
                    self.line("Rewind to an earlier point with `spindle targets` and `spindle rewind`.");
                }
            }
            Event::Rewound {
                target,
                revision,
                kind,
                abandoned,
            } => {
                if let Ok(mut outcome) = self.outcome.lock() {
                    outcome.rewound = Some(json!({
                        "target": target, "revision": revision, "abandoned": abandoned,
                        "kind": match kind {
                            RewindTargetKind::Settled => "settled",
                            RewindTargetKind::BeforeModel => "before_model",
                        },
                    }));
                }
                self.line(format!(
                    "Rewound to revision {target} (now revision {}). Workspace restored.",
                    revision
                ));
                if abandoned > 0 {
                    self.line(format!(
                        "{} abandoned request(s) can never be adopted or re-sent.",
                        abandoned
                    ));
                }
                if self
                    .retrying
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    return;
                }
                match kind {
                    RewindTargetKind::Settled => {
                        self.line("Send the next prompt with `spindle say`.")
                    }
                    RewindTargetKind::BeforeModel => {
                        self.line("Run `spindle continue` to re-run the next model request.")
                    }
                }
            }
            Event::Retrying {
                target,
                head: revision,
            } => {
                self.retrying
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                self.line(format!("Retrying from revision {target} (head was {revision})."))
            }
            Event::RetryingFirst { path } => self.line(format!(
                "Retrying the first request in a fresh store; the interrupted one is kept in {}.",
                path.display()
            )),
            Event::NothingToContinue => {
                self.line("Nothing to continue: the last turn has finished.")
            }
            Event::Extended { steps } => {
                self.line(format!("Continuing past the step limit with {steps} more steps."))
            }
            Event::SettingsApplied { text } => self.line(format!("{}", shown(text))),
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
    fn new(json: bool) -> Self {
        Self {
            narrate_to_stderr: json,
            ..Self::default()
        }
    }

    /// One line of narration (stderr under `--json`).
    fn line(&self, text: impl AsRef<str>) {
        let text = text.as_ref();
        match self.narrate_to_stderr {
            true => eprintln!("{text}"),
            false => println!("{text}"),
        }
    }

    /// Narration as it streams, without a line ending of its own.
    fn raw(&self, bytes: &[u8]) {
        let _ = match self.narrate_to_stderr {
            true => {
                let mut out = std::io::stderr().lock();
                out.write_all(bytes).and_then(|()| out.flush())
            }
            false => {
                let mut out = std::io::stdout().lock();
                out.write_all(bytes).and_then(|()| out.flush())
            }
        };
    }

    /// `--json`'s result of a turn command: how the turn stopped (or what a rewind did),
    /// its final answer when it completed, what it cost, and the command that goes on.
    fn result(&self, dir: &Path) -> Value {
        let outcome = self
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let head = api::head_state(dir).unwrap_or(Value::Null);
        let complete = head["complete"] == true;
        let answer = complete
            .then(|| api::history_at(dir, None).ok())
            .flatten()
            .and_then(|(_, _, history)| last_answer(&history));
        let (next, why) = next_step(dir, &head);
        json!({
            "session": dir,
            "head": head["head"],
            "phase": head["phase"],
            "complete": complete,
            "stopped": outcome.stopped,
            "rewound": outcome.rewound,
            "answer": answer,
            "cost": outcome.cost.map(|(turn, thread)| json!({"turn": turn, "thread": thread})),
            "next": next,
            "why": why,
        })
    }

    fn call(&self, name: &str, input: &Value) {
        if let Ok(mut live) = self.live.lock() {
            live.shown = 0;
            live.noted = false;
            live.mid_line = false;
            live.streamed = false;
            live.name.clear();
            live.name.push_str(name);
        }
        self.line(format!("{}", shown(&describe_call(name, input))));
    }

    fn output(&self, bytes: &[u8]) {
        let Ok(mut live) = self.live.lock() else {
            return;
        };
        let mut out: Vec<u8> = vec![];
        let room = LIVE_OUTPUT.saturating_sub(live.shown);
        if room > 0 {
            let part = &bytes[..bytes.len().min(room)];
            live.shown += part.len();
            live.streamed |= !part.is_empty();
            for piece in String::from_utf8_lossy(part).split_inclusive('\n') {
                if !live.mid_line {
                    out.extend_from_slice(b"  | ");
                }
                out.extend_from_slice(shown(piece).as_bytes());
                live.mid_line = !piece.ends_with('\n');
            }
        } else if !live.noted {
            live.noted = true;
            if live.mid_line {
                out.push(b'\n');
            }
            out.extend_from_slice(
                b"  | ... (preview capped; the recorded result has a paged output window)\n",
            );
            live.mid_line = false;
        }
        // Written before the state is let go: stdout and stderr arrive on two threads.
        self.raw(&out);
        drop(live);
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
            self.line("");
        }
    }

    fn tool_result(&self, content: &str, failed: bool) {
        let (streamed, name) = match self.live.lock() {
            Ok(live) => (live.streamed, live.name.clone()),
            Err(_) => return,
        };
        // A run its bounds or the user ended, or whose changes were not kept, closes with
        // a line saying so (a stop's is bracketed already).
        let closing = [KILLED_RUN, UNSEALED_RUN, STOPPED_RUN, STOPPED_SCRIPT]
            .iter()
            .any(|ending| content.ends_with(ending));
        let mut text = if streamed && closing {
            let last = content.lines().last().unwrap_or_default();
            match last.starts_with('[') {
                true => format!("  | {last}"),
                false => format!("  | ({last})"),
            }
        } else if streamed {
            String::new()
        } else if name == "read_file" && !failed {
            format!("  | ({} lines read)", content.lines().count())
        } else {
            shorten(content, 12)
        };
        // How a command ended, as its result says (`[exit status 3]`), shown whether or
        // not the rest was (streamed live, or cut short above).
        let ended =
            content.lines().rev().take(3).find(|line| {
                line.starts_with("[exit status") || line.starts_with("[ended by signal")
            });
        if let Some(ended) = ended.filter(|ended| !text.contains(*ended)) {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("  | {ended}"));
        } else if failed && ended.is_none() && !closing {
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
            self.line(format!("{}", shown(&text)));
        }
    }
}

/// Model, tool and command text as the terminal shows it: a control character other
/// than newline and tab is written out (`\u{1b}`), so no escape sequence in it can set
/// the clipboard, plant a link, move the cursor or clear the screen.
fn shown(text: &str) -> std::borrow::Cow<'_, str> {
    let hidden = |c: char| c.is_control() && c != '\n' && c != '\t';
    if !text.chars().any(hidden) {
        return text.into();
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if hidden(c) {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out.into()
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

/// The command that goes on from where the session stands (`next`), and why: `say`
/// after a finished turn, what the recovery report names for unfinished work
/// (`continue`, `retry`, `rewind`...), `wait` while another process runs it.
fn next_step(dir: &Path, head: &Value) -> (String, Option<String>) {
    if api::held(dir) {
        let why = match cli_control::listening(dir) {
            true => {
                "another process runs this session: `spindle steer` gives it a message, \
                 `spindle stop` stops it"
            }
            false => {
                "another process holds this session (`spindle serve`, `spindle api` or \
                 another command): reach it through that host, or wait"
            }
        };
        return ("wait".into(), Some(why.into()));
    }
    if head["complete"] == true || head["head"] == 0 {
        return ("say".into(), None);
    }
    if let Ok(report) = api::recovery(dir) {
        let items = report["items"].as_array().cloned().unwrap_or_default();
        if let Some(item) = items
            .iter()
            .find(|item| item["next"].as_str().is_some_and(|next| next != "none"))
        {
            return (
                item["next"].as_str().unwrap_or("continue").into(),
                item["why"].as_str().map(String::from),
            );
        }
        if report["wake"]["work"] == "ready" {
            return (
                "continue".into(),
                report["wake"]["why"].as_str().map(String::from),
            );
        }
    }
    match head["accepts_message"] == true {
        true => ("say".into(), None),
        false => ("continue".into(), None),
    }
}

/// Where the session stands, as `status` shows it. While another process runs it,
/// what is committed is read without its lock (the counts only an owner can make are
/// null then); otherwise the session is opened for a moment, as before.
fn status_json(dir: &Path) -> Result<Value> {
    if !dir.join("store").exists() {
        return Err(format!(
            "no session in {}; start one with `spindle new`",
            dir.display()
        ));
    }
    let settings = Settings::load(dir)?;
    let mut owned: Option<Status> = None;
    if !api::held(dir) {
        match Session::open(dir, Arc::new(Terminal::new(true))) {
            Ok(mut session) => owned = Some(session.status()?),
            Err(error) if error == agent::LOCKED => {}
            Err(error) => return Err(error),
        }
    }
    let head = api::head_state(dir)?;
    // Held by another process; running only while its turn is under way.
    let held = owned.is_none();
    let running = held && !matches!(head["phase"].as_str(), Some("stopped" | "empty"));
    let state = match (&owned, head["phase"].as_str()) {
        (Some(status), _) => status.state.clone(),
        (None, Some("model_wait")) => "running: waiting for the model".into(),
        (None, Some("tools_wait")) => "running: tools".into(),
        (None, Some("empty")) => "no turns yet (another process holds it open)".into(),
        (None, Some("stopped")) => format!(
            "{} (another process holds it open)",
            head["stop_text"].as_str().unwrap_or("stopped")
        ),
        (None, _) => "running".into(),
    };
    let reasoning = agent::describe_effort(&settings.provider, &settings.model, settings.effort)
        .map(|(label, _)| label)
        .unwrap_or_else(|error| error);
    let report = agent::usage_report(dir).ok();
    let usage = report.as_ref().map(|report| {
        let total = &report.total;
        json!({
            "calls": total.calls,
            "input": total.input + total.cache_read + total.cache_create,
            "cached": total.cache_read,
            "output": total.output,
            "usd": total.usd,
            "parent_usd": report.parent.usd,
            "nested_usd": report.nested.usd,
            "compaction_usd": report.compaction.usd,
            "subscription_calls": total.subscription,
            "unpriced_calls": total.unpriced,
            "refused_calls": total.refused,
            "uncertain_calls": total.unknown,
        })
    });
    let (next, why) = next_step(dir, &head);
    Ok(json!({
        "session": dir,
        "provider": settings.provider,
        "model": settings.model,
        "effort": settings.effort.as_str(),
        "reasoning": reasoning,
        "isolation": settings.isolation.as_str(),
        "network": settings.network,
        "toolchain": settings.toolchain,
        "held": held,
        "running": running,
        "head": head["head"],
        "epoch": owned.as_ref().map(|status| status.epoch),
        "phase": head["phase"],
        "complete": head["complete"],
        "state": state,
        "prompts": head["prompts"],
        "files": head["files"],
        "unresolved": owned.as_ref().map(|status| status.unresolved),
        "abandoned": owned.as_ref().map(|status| status.abandoned),
        "refused": owned.as_ref().map(|status| status.rejected),
        "usage": usage,
        "budget": settings.max_usd.map(|max| json!({
            "max_usd": max,
            "spent_usd": report.as_ref().map(|report| report.total.usd),
        })),
        "next": next,
        "why": why,
    }))
}

fn print_status(status: &Value) {
    let dir = PathBuf::from(status["session"].as_str().unwrap_or_default());
    println!("session    {}", dir.display());
    println!(
        "provider   {} / {}",
        status["provider"].as_str().unwrap_or_default(),
        status["model"].as_str().unwrap_or_default()
    );
    match status["epoch"].as_u64() {
        Some(epoch) => println!("revision   {} (owner epoch {epoch})", status["head"]),
        None => println!("revision   {} (another process holds it)", status["head"]),
    }
    println!(
        "prompts    {} in current history (compaction folds older ones)",
        status["prompts"]
    );
    println!(
        "state      {}",
        status["state"].as_str().unwrap_or_default()
    );
    println!("workspace  {} file(s)", status["files"]);
    println!(
        "isolation  {}",
        status["isolation"].as_str().unwrap_or_default()
    );
    println!(
        "reasoning  {} (effort {})",
        status["reasoning"].as_str().unwrap_or_default(),
        status["effort"].as_str().unwrap_or_default()
    );
    if let Ok(report) = agent::usage_report(&dir) {
        let usage = &report.total;
        println!(
            "usage      {} model call(s): {} input ({} cached) / {} output tokens",
            usage.calls,
            usage.input + usage.cache_read + usage.cache_create,
            usage.cache_read,
            usage.output
        );
        println!("cost       {}", cost_line(&report));
        if let Some(max) = status["budget"]["max_usd"].as_f64() {
            println!(
                "budget     {} of {} spent",
                dollars(usage.usd),
                dollars(max)
            );
        }
        if usage.unknown > 0 {
            // A request another process has in flight counts as unknown until it ends.
            let in_flight = status["running"] == true && status["phase"] == "model_wait";
            println!(
                "uncertain  {} model request(s) may have been billed (sent, outcome unknown){}",
                usage.unknown,
                if in_flight {
                    "; one may be the request in flight now"
                } else {
                    ""
                }
            );
        }
    }
    let count = |key: &str| status[key].as_u64().unwrap_or(0);
    if count("unresolved") > 0 {
        println!(
            "unresolved {} step(s) with unknown outcome",
            count("unresolved")
        );
    }
    if count("refused") > 0 {
        println!(
            "refused    {} request(s) refused before anything was generated (not billed)",
            count("refused")
        );
    }
    if count("abandoned") > 0 {
        println!(
            "abandoned  {} request(s) left behind by rewinds",
            count("abandoned")
        );
    }
    let next = status["next"].as_str().unwrap_or_default();
    let command = match next {
        "say" => "spindle say \"...\"".to_string(),
        "wait" => "wait".to_string(),
        other => format!("spindle {other}"),
    };
    match status["why"].as_str() {
        Some(why) if next != "say" => println!("next       {command}  ({why})"),
        _ => println!("next       {command}"),
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
        let mut prompt = shown(&turn.prompt).into_owned();
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
                    Some(user) => println!("\n> (while running) {}", shown(user)),
                    None => println!("\n> {}", shown(&prompt_shown(text))),
                },
                Block::Text { text } | Block::PartialStream { text } => {
                    println!("{}", shown(text.trim_end()))
                }
                Block::ToolUse { name, input, .. } => {
                    println!("{}", shown(&describe_call(name, input)))
                }
                Block::ToolResult { content, .. } => {
                    println!("{}", shown(&shorten(content, 6)))
                }
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
        let last = shown(&target.last).into_owned();
        println!("{:>5}  {kind:<24} {last}", target.revision);
    }
}

/// The files form (`api::files_at`): each file with its size, then links.
fn print_files(form: &Value) {
    for file in form["files"].as_array().into_iter().flatten() {
        println!(
            "{:>9}  {}",
            file["size"].as_u64().unwrap_or_default(),
            shown(file["path"].as_str().unwrap_or_default())
        );
    }
    for link in form["links"].as_array().into_iter().flatten() {
        println!(
            "     link  {} -> {}",
            shown(link["path"].as_str().unwrap_or_default()),
            shown(link["target"].as_str().unwrap_or_default())
        );
    }
}

/// A prompt as the terminal shows it: the workspace list a first prompt carries for the
/// model (`agent::WORKSPACE_NOTE`) is named, not printed.
fn prompt_shown(text: &str) -> std::borrow::Cow<'_, str> {
    match text.find(agent::WORKSPACE_NOTE) {
        Some(at) => format!(
            "{}\n  (+ the workspace's file list, for the model)",
            &text[..at]
        )
        .into(),
        None => text.into(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn global_options_are_taken_wherever_they_stand_before_a_double_dash() {
        let args = |line: &str| line.split(' ').map(String::from).collect::<Vec<_>>();
        let global =
            super::global(args("say --dir d --json hello -- --dir x")).unwrap_or_else(|_| panic!());
        assert_eq!(global.dir, std::path::PathBuf::from("d"));
        assert!(global.json);
        assert_eq!(global.command.as_deref(), Some("say"));
        assert_eq!(global.args, args("hello -- --dir x"));
        // Sign-in commands keep their own arguments.
        let login = super::global(args("login deepseek --json")).unwrap_or_else(|_| panic!());
        assert!(!login.json);
        assert_eq!(login.args, args("deepseek --json"));
        let words =
            super::Flags::parse("say", args("-- --not-a-flag")).unwrap_or_else(|_| panic!());
        assert_eq!(words.words, ["--not-a-flag"]);
        let words = super::Flags::parse("say", args("subtract -5 with the -v flag"))
            .unwrap_or_else(|_| panic!());
        assert_eq!(words.words.join(" "), "subtract -5 with the -v flag");
    }

    #[test]
    fn each_command_has_its_own_help() {
        for command in [
            "new", "say", "steer", "stop", "diff", "cat", "files", "log", "status", "help",
        ] {
            let help = super::command_usage(command).unwrap();
            assert!(
                help.contains(&format!("\n  {command}")),
                "{command}: {help}"
            );
        }
        assert!(
            super::command_usage("logout")
                .unwrap()
                .contains("login [PROVIDER]")
        );
        assert!(super::command_usage("nope").is_none());
    }

    #[test]
    fn terminal_text_shows_escape_sequences_instead_of_running_them() {
        // OSC 52 (set the clipboard), OSC 8 (a link), clear screen, C1 CSI, a bell, and a
        // carriage return that would draw over the line.
        let text =
            "a\u{1b}]52;c;cHduZWQ=\u{7}b\u{1b}]8;;https://x\u{1b}\\c\u{1b}[2J\u{9b}31m\r\n\tz";
        assert_eq!(
            super::shown(text),
            "a\\u{1b}]52;c;cHduZWQ=\\u{7}b\\u{1b}]8;;https://x\\u{1b}\\c\\u{1b}[2J\\u{9b}31m\\r\n\tz"
        );
        assert!(matches!(
            super::shown("plain\ttext\n"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn a_failure_shows_a_providers_escape_sequences_instead_of_running_them() {
        assert_eq!(
            super::failure_line("provider said: \u{1b}]52;c;eA==\u{7}no"),
            "spindle: provider said: \\u{1b}]52;c;eA==\\u{7}no"
        );
    }

    #[test]
    fn an_upgrade_copies_sessions_beside_them() {
        use std::path::{Path, PathBuf};
        let host = std::env::temp_dir().join(format!("spindle-cli-host-{}", std::process::id()));
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join("host.sqlite"), b"").unwrap();
        let sessions = [PathBuf::from("/data/threads/one")];
        assert_eq!(
            super::upgrade_scratch(&host, &sessions),
            host.join("promotions")
        );
        let plain = host.join("sessions");
        assert_eq!(
            super::upgrade_scratch(&plain, &sessions),
            Path::new("/data/threads")
        );
        assert_eq!(
            super::upgrade_scratch(&plain, &[PathBuf::from("one")]),
            Path::new(".")
        );
        assert_eq!(super::upgrade_scratch(&host, &[]), host.join("promotions"));
        std::fs::remove_dir_all(host).unwrap();
    }
}
