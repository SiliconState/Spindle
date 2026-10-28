//! Self-edit from any thread. The model checks out the host's Spindle source into
//! `spindle/` with the `spindle_source` tool (a legacy self-edit thread imported it as
//! its whole workspace at r1 instead); the checkout is a sync (sync.rs: the host-source
//! snapshot recorded for the workspace root that holds it), so promotion and Update
//! from host source merge the thread's `spindle/` against it exactly as they merge a
//! legacy thread's workspace. `spindle_install` only records an install request; the
//! user's Install tap sends `promote`, which composes, gates, builds and installs as
//! before. A thread is a self-edit thread at a head when it is a legacy one or when a
//! sync is on that head's line (a rewind to before the checkout ends it).
use super::{Host, Result, SourceFile, Watch, disk, err, host_source_files, now_ms, private_dir};
use super::{random, sync};
use serde_json::{Value, json};
use spindle_store::agent::{SessionHost, Settings};
use spindle_store::broker::{HostResult, HostWorkspace, Replace};
use spindle_store::view::View;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where a checkout keeps the source in a thread's workspace.
pub(super) const PREFIX: &str = "spindle/";
/// A thread's pending install request (`spindle_install`), beside its store.
const REQUEST: &str = "install-request.json";
/// Changed paths a snapshot lists (the rest are counted).
const LISTED: usize = 200;
/// How long the host source's digests serve snapshots before it is read again.
const SOURCE_FRESH: Duration = Duration::from_secs(5);
/// Sync snapshots kept as digests for snapshots.
const KEPT_BASES: usize = 16;

/// Path → (object digest, executable), git metadata left out.
type Digests = BTreeMap<String, (String, bool)>;

/// What snapshots reuse: decoded sync bases and the host source's digests.
#[derive(Default)]
pub(super) struct Cache {
    bases: Mutex<VecDeque<(String, Arc<Digests>)>>,
    source: Mutex<Option<(Instant, Arc<Digests>)>>,
}

/// Where a thread holds Spindle's source at `head`: "" (a legacy self-edit thread, the
/// whole workspace) or PREFIX; None when it holds none. `dev` is the thread's legacy
/// self-edit flag (host.sqlite).
pub(super) fn prefix_at(dir: &Path, dev: bool, head: u64) -> Result<Option<&'static str>> {
    if dev {
        return Ok(Some(""));
    }
    Ok(sync::latest(dir, head)?.map(|_| PREFIX))
}

/// The host source the thread last synced with as of `head`, and when that was: its
/// newest sync, or a legacy thread's import at r1.
pub(super) fn base(
    dir: &Path,
    prefix: &str,
    head: u64,
    warn: impl FnMut(String),
) -> Result<(Vec<SourceFile>, String)> {
    match sync::find(dir, head, warn)? {
        Some((revision, files)) => Ok((
            files,
            format!("its sync with the host source at r{revision}"),
        )),
        None if prefix.is_empty() => Ok((files_at(dir, 1)?, "its import at r1".into())),
        None => Err(
            "this thread's record of the source it checked out is unreadable; check out the \
             source again in a new thread"
                .into(),
        ),
    }
}

/// Every file of a revision with its bytes, read from the store (no owner lock).
fn files_at(dir: &Path, revision: u64) -> Result<Vec<SourceFile>> {
    let mut view = View::open(&dir.join("store")).map_err(err)?;
    view.read(|view| {
        let (_, root) = view.at(revision)?.ok_or(spindle_store::Error::Invalid)?;
        let Some(root) = root else {
            return Ok(vec![]);
        };
        let mut files = vec![];
        for entry in view.tree(&root)?.files {
            let bytes = view.object(&entry.object)?;
            files.push((entry.path, bytes, entry.executable));
        }
        Ok(files)
    })
    .map_err(err)
}

/// The files under `prefix`, with the prefix taken off (all of them for "").
pub(super) fn strip(files: Vec<SourceFile>, prefix: &str) -> Vec<SourceFile> {
    files
        .into_iter()
        .filter_map(|(path, bytes, executable)| {
            path.strip_prefix(prefix)
                .map(|path| (path.to_string(), bytes, executable))
        })
        .collect()
}

/// Build output the host source never holds (`agent::import_files` skips these names
/// at any depth), so a thread's copy of it is never Spindle's change either.
const BUILD_OUTPUT: [&str; 2] = ["target", "node_modules"];

/// Which of a thread's paths are Spindle's. Never build output (a `node_modules` the
/// model installed would otherwise be written into the host source by every
/// promotion). A whole-workspace (legacy `dev`) thread also holds the user's other work
/// beside Spindle's source (a site, an app): there only paths whose first component
/// the host source or the sync base has are Spindle's; the rest is never listed as a
/// Spindle change or promoted into the host source.
pub(super) fn spindle_owned<'a>(
    prefix: &str,
    known: impl IntoIterator<Item = &'a str>,
) -> impl Fn(&str) -> bool {
    let tops: BTreeSet<String> = known
        .into_iter()
        .map(|path| top(path).to_string())
        .collect();
    let whole = prefix.is_empty();
    move |path: &str| {
        !path.split('/').any(|part| BUILD_OUTPUT.contains(&part))
            && (!whole || tops.contains(top(path)))
    }
}

fn top(path: &str) -> &str {
    path.split('/').next().unwrap_or(path)
}

/// A workspace's files with those under `prefix` replaced by `inside` (relative to it).
pub(super) fn with_subtree(
    files: Vec<SourceFile>,
    prefix: &str,
    inside: Vec<SourceFile>,
) -> Vec<SourceFile> {
    let mut out: Vec<SourceFile> = files
        .into_iter()
        .filter(|file| !file.0.starts_with(prefix))
        .collect();
    out.extend(
        inside
            .into_iter()
            .map(|(path, bytes, executable)| (format!("{prefix}{path}"), bytes, executable)),
    );
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The host source a checkout holds: the host source without its git metadata.
fn checkout_files(source: &Path) -> Result<Vec<SourceFile>> {
    let mut files = host_source_files(source)?;
    files.retain(|file| !sync::repository(&file.0));
    Ok(files)
}

fn digests(files: &[SourceFile]) -> Digests {
    files
        .iter()
        .filter(|file| !sync::repository(&file.0))
        .map(|(path, bytes, executable)| {
            (
                path.clone(),
                (spindle_store::artifact::object_digest(bytes), *executable),
            )
        })
        .collect()
}

/// Paths whose content or executable bit differ, sorted.
fn changed(base: &Digests, thread: &Digests) -> Vec<String> {
    let paths: BTreeSet<&String> = base.keys().chain(thread.keys()).collect();
    paths
        .into_iter()
        .filter(|path| base.get(*path) != thread.get(*path))
        .cloned()
        .collect()
}

/// The host source's digests, as `host_source_files` reads it, without git metadata
/// and without reading `.git` (a stat walk of the same rules).
fn source_digests(source: &Path) -> Result<Digests> {
    fn walk(root: &Path, dir: &Path, out: &mut Digests) -> Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .map_err(err)?
            .filter_map(|entry| entry.ok())
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.is_empty()
                || name.len() > spindle_store::workspace::MAX_NAME
                || ["target", "node_modules", ".git"].contains(&name.as_str())
            {
                continue;
            }
            let kind = entry.file_type().map_err(err)?;
            let path = entry.path();
            if kind.is_dir() {
                walk(root, &path, out)?;
            } else if kind.is_file() {
                let meta = entry.metadata().map_err(err)?;
                if meta.len() > dext_native::strict::MAX_FILE as u64 {
                    continue;
                }
                let relative = path
                    .strip_prefix(root)
                    .map_err(err)?
                    .to_string_lossy()
                    .into_owned();
                if !spindle_store::workspace::valid_path(&relative)
                    || relative.starts_with(".dext/")
                    || relative.starts_with(".crew/")
                {
                    continue;
                }
                #[cfg(unix)]
                let executable = {
                    use std::os::unix::fs::PermissionsExt;
                    meta.permissions().mode() & 0o111 != 0
                };
                #[cfg(not(unix))]
                let executable = false;
                let bytes = std::fs::read(&path).map_err(err)?;
                out.insert(
                    relative,
                    (spindle_store::artifact::object_digest(&bytes), executable),
                );
            }
        }
        Ok(())
    }
    let mut out = Digests::new();
    walk(source, source, &mut out)?;
    Ok(out)
}

impl Cache {
    fn source(&self, source: &Path) -> Result<Arc<Digests>> {
        let mut kept = super::lock(&self.source);
        if let Some((at, digests)) = kept.as_ref()
            && at.elapsed() < SOURCE_FRESH
        {
            return Ok(digests.clone());
        }
        let digests = Arc::new(source_digests(source)?);
        *kept = Some((Instant::now(), digests.clone()));
        Ok(digests)
    }

    /// The base a snapshot compares with: the newest sync's record, or r1's files.
    fn base(&self, dir: &Path, synced: Option<&(u64, String)>) -> Result<Arc<Digests>> {
        let key = format!(
            "{}\n{}",
            dir.display(),
            synced.map_or("r1", |(_, root)| root.as_str())
        );
        if let Some((_, digests)) = super::lock(&self.bases).iter().find(|(k, _)| *k == key) {
            return Ok(digests.clone());
        }
        let digests = Arc::new(match synced {
            Some((_, root)) => digests(&sync::load(dir, root)?),
            None => {
                let mut view = View::open(&dir.join("store")).map_err(err)?;
                view.read(|view| match view.at(1)? {
                    Some((_, Some(root))) => Ok(view
                        .tree(&root)?
                        .files
                        .into_iter()
                        .filter(|entry| !sync::repository(&entry.path))
                        .map(|entry| (entry.path, (entry.object, entry.executable)))
                        .collect()),
                    _ => Ok(Digests::new()),
                })
                .map_err(err)?
            }
        });
        let mut bases = super::lock(&self.bases);
        bases.push_back((key, digests.clone()));
        while bases.len() > KEPT_BASES {
            bases.pop_front();
        }
        Ok(digests)
    }
}

/// A thread's pending install request: `{revision, summary, at}`.
pub(super) fn request(dir: &Path) -> Option<Value> {
    let bytes = std::fs::read(dir.join(REQUEST)).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    (value["revision"].is_u64() && value["summary"].is_string()).then_some(value)
}

/// The install request as of `head`: only while the revision it was made at is on
/// the head's line (a rewind to before it leaves the change it describes behind).
fn request_at(dir: &Path, head: u64) -> Result<Option<Value>> {
    let Some(request) = request(dir) else {
        return Ok(None);
    };
    let made = request["revision"].as_u64().unwrap_or(u64::MAX);
    let mut view = View::open(&dir.join("store")).map_err(err)?;
    let mut found = false;
    view.read(|view| {
        view.line(head, |revision, _| {
            found = revision == made;
            found || revision < made
        })
    })
    .map_err(err)?;
    Ok(found.then_some(request))
}

/// Forgets a thread's install request (dismissed, or installed).
pub(super) fn clear_request(dir: &Path) -> Result<()> {
    match std::fs::remove_file(dir.join(REQUEST)) {
        Ok(()) => spindle_store::durable::sync_parent(&dir.join(REQUEST)).map_err(err),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(err(error)),
    }
}

/// The snapshot's `self_edit`: null unless the thread holds Spindle's source at its
/// head; else where (`prefix`), what changed against its last sync, whether the host
/// source moved since, the last refused promotion's conflicts at this head, the
/// pending install request and whether a promotion is running.
pub(super) fn snapshot(host: &Host, id: &str, head: u64, live: &Watch) -> Result<Value> {
    let Some(source) = host.source.as_ref() else {
        return Ok(Value::Null);
    };
    let dir = host.dir(id);
    let dev = host.is_dev(id);
    let synced = sync::latest(&dir, head)?;
    let prefix = match (dev, &synced) {
        (true, _) => "",
        (false, Some(_)) => PREFIX,
        (false, None) => return Ok(Value::Null),
    };
    let base = host.self_edit.base(&dir, synced.as_ref())?;
    let mut view = View::open(&dir.join("store")).map_err(err)?;
    // A revision's files under the prefix, without git metadata.
    let subtree = |view: &mut View, root: Option<String>| -> spindle_store::Result<Digests> {
        let Some(root) = root else {
            return Ok(Digests::new());
        };
        Ok(view
            .tree(&root)?
            .files
            .into_iter()
            .filter_map(|entry| {
                let path = entry.path.strip_prefix(prefix)?.to_string();
                (!sync::repository(&path)).then_some((path, (entry.object, entry.executable)))
            })
            .collect())
    };
    let (thread, synced_files) = view
        .read(|view| {
            let at = |view: &mut View, revision| {
                Ok::<_, spindle_store::Error>(view.at(revision)?.and_then(|(_, root)| root))
            };
            let thread = at(view, head)?;
            let thread = subtree(view, thread)?;
            let synced = match &synced {
                Some((_, root)) => subtree(view, Some(root.clone()))?,
                None => {
                    let first = at(view, 1)?;
                    subtree(view, first)?
                }
            };
            Ok((thread, synced))
        })
        .map_err(err)?;
    let source_files = host.self_edit.source(source)?;
    let owned = spindle_owned(
        prefix,
        base.keys().chain(source_files.keys()).map(String::as_str),
    );
    let mut changed = changed(&base, &thread);
    changed.retain(|path| owned(path));
    // A revision whose subtree is the base exactly (the checkout or import, or an
    // update that merged nothing of the thread's own), to diff `changed` against.
    let base_revision =
        (synced_files == *base).then(|| synced.as_ref().map_or(1, |(revision, _)| *revision));
    let behind = *source_files != *base;
    let (conflicts, installing, failure) = {
        let live = live.lock();
        let conflicts = match &live.source_moved {
            Some((at, paths)) if *at == head => paths.clone(),
            _ => vec![],
        };
        // A gate's refusal stands while the head is the one it judged.
        let failure = match &live.install_failed {
            Some((at, command, output)) if *at == head => {
                json!({"command": command, "output": output})
            }
            _ => Value::Null,
        };
        (conflicts, live.installing, failure)
    };
    Ok(json!({
        "prefix": prefix,
        "changed": changed.iter().take(LISTED).collect::<Vec<_>>(),
        "changed_count": changed.len(),
        "base_revision": base_revision,
        "behind": behind,
        "conflicts": conflicts,
        "request": request_at(&dir, head)?,
        "installing": installing,
        "failure": failure,
        "host_build": host.build.get().cloned().unwrap_or_default(),
    }))
}

/// A thread's host for its host tools (`agent::SessionHost`): self-edit and its name.
pub(super) struct ThreadHost {
    pub host: Arc<Host>,
    pub id: String,
}

impl ThreadHost {
    /// The thread's files under `prefix` (relative to it) in the call's workspace.
    fn files(workspace: &mut HostWorkspace<'_>, prefix: &str) -> Result<Vec<SourceFile>> {
        let entries: Vec<_> = workspace
            .files()
            .iter()
            .filter(|entry| entry.path.starts_with(prefix))
            .cloned()
            .collect();
        let mut files = Vec::with_capacity(entries.len());
        for entry in entries {
            let bytes = workspace.read(&entry.object).map_err(err)?;
            files.push((
                entry.path[prefix.len()..].to_string(),
                bytes,
                entry.executable,
            ));
        }
        Ok(files)
    }

    fn checkout(&self, workspace: &mut HostWorkspace<'_>) -> Result<HostResult> {
        let source = self.host.source.as_ref().ok_or("no host source")?;
        let dir = self.host.dir(&self.id);
        if self.host.is_dev(&self.id) {
            return Ok(HostResult::text(
                "This thread's whole workspace is already Spindle's source (a self-edit \
                 thread): edit it here, and use action update to bring in the host's newer \
                 source.",
                false,
            ));
        }
        let synced = sync::latest(&dir, workspace.revision())?;
        if workspace
            .files()
            .iter()
            .any(|entry| entry.path.starts_with(PREFIX))
            || workspace.is_directory("spindle")
        {
            return Ok(HostResult::text(
                match synced {
                    Some((revision, _)) => format!(
                        "spindle/ already holds Spindle's source (synced with the host at \
                         r{revision}): edit it there, or use action update to bring in the \
                         host's newer source."
                    ),
                    None => "spindle/ already exists in this workspace and is not a checkout; \
                             rename or remove it first."
                        .into(),
                },
                false,
            ));
        }
        let files = checkout_files(source)?;
        let count = files.len();
        let snapshot = files.clone();
        let engine = source.parent().map(|parent| parent.join("dext-engine"));
        let mount = engine
            .as_deref()
            .filter(|engine| engine.is_dir())
            .and_then(dext_native::strict::source_mount);
        let links = mount
            .iter()
            .map(|mount| ("dext-engine".to_string(), mount.clone()))
            .collect();
        let thread = dir.clone();
        let staged = move |root: &str| -> Result<()> {
            sync::record(&thread, root, &snapshot)?;
            enable_build(&thread, engine.as_deref())
        };
        Ok(HostResult {
            content: format!(
                "Checked out Spindle's source (the host runs build {}) into spindle/: {count} \
                 files, without .git. From your next command the Rust toolchain is available{}. \
                 Build and test inside spindle/ (cargo fmt, cargo clippy --all-targets \
                 --all-features, cargo test); build output goes to a cache outside the \
                 workspace. When the change is ready, call spindle_install with a summary: \
                 it raises the Spindle change card in the chat, and the user taps Install \
                 there to deploy it.",
                self.host.build.get().map_or("unknown", String::as_str),
                match &mount {
                    Some(mount) => format!(
                        " and the dext-engine sources are mounted read-only at {mount} (the \
                         workspace link dext-engine points there, so spindle/'s \
                         ../dext-engine paths resolve)"
                    ),
                    None => String::new(),
                }
            ),
            success: true,
            replace: Some(Replace {
                prefix: PREFIX.into(),
                files,
                links,
                staged: Some(Box::new(staged)),
            }),
        })
    }

    fn update(&self, workspace: &mut HostWorkspace<'_>) -> Result<HostResult> {
        let source = self.host.source.as_ref().ok_or("no host source")?;
        let dir = self.host.dir(&self.id);
        let head = workspace.revision();
        let Some(prefix) = prefix_at(&dir, self.host.is_dev(&self.id), head)? else {
            return Ok(HostResult::text(
                "There is no checkout to update: call spindle_source with action checkout \
                 first.",
                false,
            ));
        };
        let mut warnings = vec![];
        let (base, since) = base(&dir, prefix, head, |warning| warnings.push(warning))?;
        let ours = if prefix.is_empty() {
            host_source_files(source)?
        } else {
            checkout_files(source)?
        };
        let theirs = Self::files(workspace, prefix)?;
        let scratch = self
            .host
            .root
            .join("promotions")
            .join(format!("update-{}", &random()?[..16]));
        private_dir(&scratch)?;
        let merged = sync::merge(&base, &ours, &theirs, &scratch);
        disk::purge(&scratch);
        let update = merged?;
        let at = if prefix.is_empty() {
            "This workspace"
        } else {
            "spindle/"
        };
        if update.unchanged(&theirs) && update.decisions.is_empty() {
            // It already holds every host change: promotions merge from here.
            sync::record(&dir, workspace.root(), &ours)?;
            return Ok(HostResult::text(
                format!("{at} already has every change in the host source; nothing to update."),
                true,
            ));
        }
        let mut content = sync::prompt(&update, &since, prefix, true);
        for warning in warnings {
            content.push_str(&format!("\n{warning}"));
        }
        let thread = dir.clone();
        Ok(HostResult {
            content,
            success: true,
            replace: Some(Replace {
                prefix: prefix.into(),
                files: update.files,
                links: vec![],
                staged: Some(Box::new(move |root: &str| {
                    sync::record(&thread, root, &ours)
                })),
            }),
        })
    }

    fn install(&self, summary: &str, workspace: &mut HostWorkspace<'_>) -> Result<HostResult> {
        let dir = self.host.dir(&self.id);
        let head = workspace.revision();
        let Some(prefix) = prefix_at(&dir, self.host.is_dev(&self.id), head)? else {
            return Ok(HostResult::text(
                "There is nothing to install: Spindle's source is not checked out here (call \
                 spindle_source with action checkout, then change it).",
                false,
            ));
        };
        let (base, _) = base(&dir, prefix, head, |_| {})?;
        let thread: Digests = workspace
            .files()
            .iter()
            .filter_map(|entry| {
                let path = entry.path.strip_prefix(prefix)?.to_string();
                (!sync::repository(&path)).then(|| (path, (entry.object.clone(), entry.executable)))
            })
            .collect();
        let source = self.host.source.as_ref().ok_or("no host source")?;
        let source_files = self.host.self_edit.source(source)?;
        let owned = spindle_owned(
            prefix,
            base.iter()
                .map(|file| file.0.as_str())
                .chain(source_files.keys().map(String::as_str)),
        );
        let mut changed = changed(&digests(&base), &thread);
        changed.retain(|path| owned(path));
        if changed.is_empty() {
            return Ok(HostResult::text(
                format!(
                    "Nothing to install: {} is the same as the host source it last synced \
                     with.",
                    if prefix.is_empty() {
                        "the workspace"
                    } else {
                        "spindle/"
                    }
                ),
                false,
            ));
        }
        let request = json!({"revision": head, "summary": summary.trim(), "at": now_ms()});
        spindle_store::durable::write(&dir.join(REQUEST), request.to_string().as_bytes())
            .map_err(err)?;
        let mut listed = changed
            .iter()
            .take(12)
            .map(|path| format!("{prefix}{path}"))
            .collect::<Vec<_>>()
            .join(", ");
        if changed.len() > 12 {
            listed.push_str(&format!(" and {} more", changed.len() - 12));
        }
        Ok(HostResult::text(
            format!(
                "Install requested for r{head}: {} changed file(s) ({listed}). Nothing is \
                 installed yet. The user sees the Spindle change card at the end of the chat; \
                 when they tap its Install (or type /install) the host merges these changes with its current source, runs fmt, clippy, \
                 tests and a release build in isolation, installs the result and restarts. \
                 Tell the user it is ready to install; never tell them to restart the host.",
                changed.len()
            ),
            true,
        ))
    }
}

/// What building Spindle needs, for a thread's next commands: the toolchain and the
/// dext-engine sources mounted read-only (as a legacy self-edit thread has them).
fn enable_build(dir: &Path, engine: Option<&Path>) -> Result<()> {
    let mut settings = Settings::load(dir)?;
    settings.toolchain = true;
    if let Some(engine) = engine.filter(|engine| engine.is_dir()) {
        for mount in spindle_store::agent::sources(&[engine.to_path_buf()])? {
            if !settings.sources.contains(&mount) {
                settings.sources.push(mount);
            }
        }
    }
    settings.save(dir)
}

impl SessionHost for ThreadHost {
    fn offers_source(&self) -> bool {
        self.host.source.is_some()
    }
    fn spindle_source(&self, action: &str, workspace: &mut HostWorkspace<'_>) -> HostResult {
        let outcome = match action {
            "checkout" => self.checkout(workspace),
            "update" => self.update(workspace),
            _ => Err("action must be checkout or update".into()),
        };
        outcome.unwrap_or_else(|error| HostResult::text(format!("Nothing changed: {error}"), false))
    }
    fn spindle_install(&self, summary: &str, workspace: &mut HostWorkspace<'_>) -> HostResult {
        self.install(summary, workspace).unwrap_or_else(|error| {
            HostResult::text(format!("No install was requested: {error}"), false)
        })
    }
    fn rename(&self, name: &str) -> Result<()> {
        let renamed = super::lock(&self.host.db)
            .execute(
                "UPDATE sessions SET name=?2 WHERE id=?1",
                rusqlite::params![self.id, name],
            )
            .map_err(err)?;
        super::lock(&self.host.summaries).remove(&self.id);
        if renamed == 1 {
            Ok(())
        } else {
            Err("the thread is gone".into())
        }
    }
    fn shelf(&self) -> Option<std::path::PathBuf> {
        Some(self.host.root.join("shelf"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, text: &str) -> SourceFile {
        (path.to_string(), text.as_bytes().to_vec(), false)
    }

    #[test]
    fn a_whole_workspace_thread_keeps_its_other_work_out_of_spindle() {
        // ideasCon and Small hold a site and an app beside Spindle's source.
        let owned = spindle_owned("", ["store/src/lib.rs", "Cargo.toml", "docs/a.md"]);
        assert!(owned("store/ui/app.js") && owned("Cargo.toml") && owned("docs/new.md"));
        for other in [
            "carbon-lab/index.html",
            "stellar.html",
            ".tools/cloudflared",
        ] {
            assert!(!owned(other), "{other}");
        }
        // Everything under a spindle/ checkout is Spindle's, new top-level paths too,
        // apart from build output (which the host source never holds).
        let checkout = spindle_owned(PREFIX, ["store/src/lib.rs"]);
        assert!(checkout("clients/go/main.go"));
        for output in ["clients/js/node_modules/x/index.js", "target/debug/spindle"] {
            assert!(!checkout(output) && !owned(output), "{output}");
        }
    }

    #[test]
    fn a_checkout_is_the_subtree_and_changes_ignore_git_metadata() {
        let workspace = vec![
            file("notes.txt", "mine\n"),
            file("spindle/a.txt", "thread\n"),
            file("spindle/b.txt", "b\n"),
            file("spindlex/c.txt", "not the checkout\n"),
        ];
        let inside = strip(workspace.clone(), PREFIX);
        assert_eq!(inside, [file("a.txt", "thread\n"), file("b.txt", "b\n")]);
        assert_eq!(
            strip(workspace.clone(), ""),
            workspace,
            "a legacy thread's is all"
        );
        let merged = with_subtree(
            workspace,
            PREFIX,
            vec![file("a.txt", "merged\n"), file("d.txt", "host\n")],
        );
        assert_eq!(
            merged,
            [
                file("notes.txt", "mine\n"),
                file("spindle/a.txt", "merged\n"),
                file("spindle/d.txt", "host\n"),
                file("spindlex/c.txt", "not the checkout\n"),
            ]
        );
        assert_eq!(
            with_subtree(merged, "", vec![file("x", "x\n")]),
            [file("x", "x\n")]
        );
        let base = digests(&[
            file("a.txt", "1\n"),
            file("b.txt", "b\n"),
            file(".git/HEAD", "x"),
        ]);
        let thread = digests(&[
            file("a.txt", "2\n"),
            file("c.txt", "c\n"),
            file(".git/HEAD", "y"),
        ]);
        assert_eq!(changed(&base, &thread), ["a.txt", "b.txt", "c.txt"]);
        let mut executable = thread.clone();
        executable.get_mut("c.txt").unwrap().1 = true;
        assert_eq!(changed(&thread, &executable), ["c.txt"]);
    }
}
