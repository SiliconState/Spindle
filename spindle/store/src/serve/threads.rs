//! Threads a client brings to this host and takes back (`POST /v1/threads`, `POST
//! /v1/sessions/:id/export`): a session bundle (`spindle_store::bundle`) becomes one of
//! this host's sessions, and leaves again as one.
//!
//! Nothing here may lose a thread or leave two live copies, whenever the host stops:
//!
//! - An import writes the upload to `incoming/` (synced), records the key as pending
//!   (committed), stages the session in `incoming/<id>` (synced, as
//!   `agent::stage_session` stages: unpacked and checked beside it, then renamed),
//!   renames it into `sessions/` (both directories synced), and only then
//!   registers it: the `sessions` row, its owner and the key's record in one
//!   transaction. A host that stopped before that commit finds the pending record when
//!   it opens and takes the half-made session away (`recover`), so the key imports
//!   again from nothing; after it, the key answers with that session.
//! - An export without `keep` writes the bundle into the session's directory
//!   (`moved.spindle`), synced and read back, before it seals the session (the store's
//!   `spindle-moved` fence, synced). A file found beside an unsealed session is the
//!   remains of an export that stopped before its seal: it is replaced, never sent. A
//!   sealed session's file is sent again on every export until the session is deleted
//!   (its directory, file included, goes to trash only with its rows).
use super::v1::Failure;
use super::{Host, Sink, Watch, disk, err, http, id_ok, lock, now_ms, private_dir, random};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use spindle_store::agent::{self, BuiltinTools, Effort, Isolation, Session, Settings};
use spindle_store::bundle::UnpackError;
use spindle_store::driver::Runner;
use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The largest bundle a client may bring (the workspace's own cap).
pub(super) const MAX_THREAD: usize = 2 * 1024 * 1024 * 1024;
/// Uploads and staged imports, all removed when the host opens.
const INCOMING: &str = "incoming";
/// A moved session's bundle, kept in its directory until the session is deleted.
pub(super) const MOVED_FILE: &str = "moved.spindle";
/// What a host refuses for a session that moved (`submit`, v1's `moved` code).
pub(super) const MOVED_AWAY: &str =
    "this thread moved back to its client; export it again for its bundle, or delete it";
/// A thread's name when the client gives none.
const DEFAULT_NAME: &str = "Imported thread";

/// Tables of host.sqlite this module keeps (created if missing, so a binary rolled
/// back to one without them still opens the host and ignores them).
pub(super) const TABLES: &str = "
    CREATE TABLE IF NOT EXISTS owners(session TEXT PRIMARY KEY,client TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS threads(client TEXT NOT NULL,key TEXT NOT NULL,digest TEXT NOT NULL,session TEXT NOT NULL,state TEXT NOT NULL,fit TEXT,at INTEGER NOT NULL,PRIMARY KEY(client,key));";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether the session in `dir` moved elsewhere (its store's fence says so).
pub(super) fn moved(dir: &Path) -> bool {
    spindle_store::fence_note(&dir.join("store"))
        .is_some_and(|note| note.starts_with(spindle_store::MOVED_FENCE))
}

/// Whether `client` owns the session `id` (it brought it).
pub(super) fn owned(db: &Connection, id: &str, client: &str) -> bool {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM owners WHERE session=?1 AND client=?2)",
        [id, client],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

/// Moves `root/sessions/<id>` to trash (both directories synced) and returns where it
/// went (None: it was not there). Only for a session no `sessions` row names; the
/// caller removes it (the open, for what `recover` discards).
fn discard(root: &Path, id: &str) -> std::io::Result<Option<PathBuf>> {
    let dir = root.join("sessions").join(id);
    if std::fs::symlink_metadata(&dir).is_err() {
        return Ok(None);
    }
    let trash = root.join("trash");
    private_dir(&trash).map_err(std::io::Error::other)?;
    let target = trash.join(id);
    disk::remove_tree(&target)?;
    std::fs::rename(&dir, &target)?;
    spindle_store::durable::sync_dir(&trash)?;
    spindle_store::durable::sync_dir(&root.join("sessions"))?;
    Ok(Some(target))
}

/// At open, before sessions are listed: uploads and staged imports go (no row names
/// them), an import that stopped before its commit is taken away and forgotten (its
/// key imports again), and rows of sessions that are gone are dropped. Returns where
/// the half-made sessions went in trash, for the open to remove.
pub(super) fn recover(root: &Path, db: &Connection) -> super::Result<Vec<PathBuf>> {
    disk::remove_tree(&root.join(INCOMING)).map_err(err)?;
    let mut discarded = vec![];
    let pending: Vec<String> = db
        .prepare("SELECT session FROM threads WHERE state='pending'")
        .and_then(|mut query| {
            query
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()
        })
        .map_err(err)?;
    for id in pending {
        let registered: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                [&id],
                |row| row.get(0),
            )
            .map_err(err)?;
        // Into trash, which the open then sweeps (no row names it).
        if id_ok(&id) && !registered {
            discarded.extend(discard(root, &id).map_err(err)?);
        }
        db.execute(
            "DELETE FROM threads WHERE session=?1 AND state='pending'",
            [&id],
        )
        .map_err(err)?;
    }
    db.execute_batch(
        "DELETE FROM owners WHERE session NOT IN (SELECT id FROM sessions);
         DELETE FROM threads WHERE session NOT IN (SELECT id FROM sessions);",
    )
    .map_err(err)?;
    Ok(discarded)
}

/// A file removed when dropped, unless kept.
struct Scratch(Option<PathBuf>);
impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A pending import: until it is registered, dropping it takes away whatever it wrote
/// (its staging, its directory in `sessions/`) and its pending record.
struct Admission<'a> {
    host: &'a Host,
    client: String,
    key: String,
    id: String,
    armed: bool,
}
impl Drop for Admission<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = disk::remove_tree(&self.host.root.join(INCOMING).join(&self.id));
        let db = lock(&self.host.db);
        let registered: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                [&self.id],
                |row| row.get(0),
            )
            .unwrap_or(true);
        if registered {
            return;
        }
        if let Ok(discarded) = discard(&self.host.root, &self.id) {
            if let Some(target) = discarded {
                std::thread::spawn(move || disk::purge(&target));
            }
            let _ = db.execute(
                "DELETE FROM threads WHERE client=?1 AND key=?2 AND state='pending'",
                params![self.client, self.key],
            );
        }
        // Otherwise the record stays pending and the next open finishes the cleanup.
    }
}

/// Imports under way at once (uploading, staging or checking).
const MAX_ARRIVING: usize = 2;
/// Free disk an import leaves the host, beyond its upload and its staged copy.
const DISK_RESERVE: u64 = 1 << 30;

/// An import under way, counted with the disk it may take until dropped: at most
/// `MAX_ARRIVING`, each with room for its upload and staged copy (twice the bundle)
/// beside those already under way. A new one counts toward the session limit too.
struct Arriving<'a> {
    host: &'a Host,
    bytes: u64,
}
impl<'a> Arriving<'a> {
    fn take(host: &'a Host, bytes: u64, new: bool) -> std::result::Result<Self, Failure> {
        let mut arriving = lock(&host.arriving);
        let (count, taking) = *arriving;
        if count >= MAX_ARRIVING {
            return Err(Failure::new(
                503,
                "busy",
                "two threads are already arriving here; send it again when one has",
            ));
        }
        if new && lock(&host.lives).len() + count >= super::MAX_SESSIONS {
            return Err(limit());
        }
        let needs = bytes
            .saturating_add(taking)
            .saturating_mul(2)
            .saturating_add(DISK_RESERVE);
        if let Some((free, _)) = disk::space(&host.root)
            && free < needs
        {
            return Err(Failure::new(
                507,
                "insufficient_storage",
                format!(
                    "this host has {} MiB free; bringing this thread needs {} MiB",
                    free >> 20,
                    needs >> 20
                ),
            ));
        }
        *arriving = (count + 1, taking.saturating_add(bytes));
        Ok(Self { host, bytes })
    }
}
impl Drop for Arriving<'_> {
    fn drop(&mut self) {
        let mut arriving = lock(&self.host.arriving);
        *arriving = (
            arriving.0.saturating_sub(1),
            arriving.1.saturating_sub(self.bytes),
        );
    }
}

/// What `import` did: the session, the fit applied, and whether this was a replay of
/// an import done before under the same key.
pub(super) struct Imported {
    pub id: String,
    pub fit: Value,
    pub replayed: bool,
}

/// An `Idempotency-Key`: 16-128 of `A-Z a-z 0-9 - _`.
fn key_ok(key: &str) -> bool {
    (16..=128).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `POST /v1/threads`: brings the bundle in the request's body in as a new session of
/// `client`'s (see the module's notes for what survives a stop at any point).
pub(super) fn import(
    host: &Arc<Host>,
    stream: &mut TcpStream,
    request: &mut http::Request,
    client: &str,
) -> std::result::Result<Imported, Failure> {
    let bad = |message: &str| Failure::new(400, "bad_request", message);
    let key = request
        .headers
        .get("idempotency-key")
        .filter(|key| key_ok(key))
        .ok_or_else(|| bad("Idempotency-Key must be 16-128 of A-Z a-z 0-9 - _"))?
        .clone();
    if let Some(unknown) = request
        .query
        .keys()
        .find(|key| !matches!(key.as_str(), "name" | "provider" | "model"))
    {
        return Err(bad(&format!("`{unknown}` is not a query of this route")));
    }
    let name = match request.query.get("name") {
        None => DEFAULT_NAME.to_string(),
        Some(name) => super::admin_name(&json!({ "name": name }))
            .map_err(|_| bad("name must be 1-80 characters"))?,
    };
    let route = match (request.query.get("provider"), request.query.get("model")) {
        (None, None) => None,
        (Some(provider), Some(model)) if !provider.is_empty() && !model.is_empty() => Some(
            Settings::route_of(provider, Some(model))
                .map_err(|why| bad(&format!("provider and model: {why}")))?,
        ),
        _ => return Err(bad("provider and model go together")),
    };
    if request.length() > MAX_THREAD {
        return Err(Failure::new(
            413,
            "too_large",
            "a thread's bundle is at most 2 GiB",
        ));
    }
    if request.length() == 0 {
        return Err(bad(
            "the body is a session bundle (with its Content-Length)",
        ));
    }
    if host.updating.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(Failure::new(
            409,
            "conflict",
            "an upgrade or restart is in progress; wait for the host to reconnect",
        ));
    }
    // Refused before the upload: a host that is full, or a key still importing.
    let known: Option<(String, String, String, Option<String>)> = lock(&host.db)
        .query_row(
            "SELECT digest,session,state,fit FROM threads WHERE client=?1 AND key=?2",
            [client, &key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(super::v1::internal)?;
    if let Some((expected, session, state, fit)) = known {
        if state == "pending" {
            return Err(still_importing());
        }
        // A committed replay only verifies the bytes: no upload or staged copy is
        // needed, even when the original import consumed the remaining disk space.
        let mut digest = Sha256::new();
        http::stream_body(stream, request, |chunk| {
            digest.update(chunk);
            Ok(())
        })
        .map_err(|why| bad(&why))?;
        if hex(&digest.finalize()) != expected {
            return Err(Failure::new(
                409,
                "conflict",
                "this Idempotency-Key was used for another bundle",
            ));
        }
        return Ok(Imported {
            id: session,
            fit: fit
                .and_then(|fit| serde_json::from_str(&fit).ok())
                .unwrap_or(Value::Null),
            replayed: true,
        });
    }
    let _arriving = Arriving::take(host, request.length() as u64, true)?;
    // The upload: to a private file, hashed as it arrives, synced once whole.
    let incoming = host.root.join(INCOMING);
    private_dir(&incoming).map_err(super::v1::internal)?;
    let path = incoming.join(format!(
        "upload-{}",
        &random().map_err(super::v1::internal)?[..32]
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(super::v1::internal)?;
    let mut upload = Scratch(Some(path.clone()));
    let mut digest = Sha256::new();
    http::stream_body(stream, request, |chunk| {
        digest.update(chunk);
        file.write_all(chunk)
            .map_err(|error| format!("cannot keep the upload: {error}"))
    })
    .map_err(|why| bad(&why))?;
    file.sync_all().map_err(super::v1::internal)?;
    drop(file);
    let digest = hex(&digest.finalize());
    // The same key: the same session (same bundle) or a refusal (another bundle).
    let id = random().map_err(super::v1::internal)?[..20].to_string();
    {
        let db = lock(&host.db);
        let known: Option<(String, String, String, Option<String>)> = db
            .query_row(
                "SELECT digest,session,state,fit FROM threads WHERE client=?1 AND key=?2",
                [client, &key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(super::v1::internal)?;
        match known {
            Some((_, _, state, _)) if state == "pending" => return Err(still_importing()),
            Some((known, ..)) if known != digest => {
                return Err(Failure::new(
                    409,
                    "conflict",
                    "this Idempotency-Key was used for another bundle",
                ));
            }
            Some((_, session, _, fit)) => {
                return Ok(Imported {
                    id: session,
                    fit: fit
                        .and_then(|fit| serde_json::from_str(&fit).ok())
                        .unwrap_or(Value::Null),
                    replayed: true,
                });
            }
            None => {}
        }
        if lock(&host.lives).len() >= super::MAX_SESSIONS {
            return Err(limit());
        }
        db.execute(
            "INSERT INTO threads VALUES(?1,?2,?3,?4,'pending',NULL,?5)",
            params![client, key, digest, id, now_ms() as i64],
        )
        .map_err(super::v1::internal)?;
    }
    let mut admission = Admission {
        host,
        client: client.to_string(),
        key: key.clone(),
        id: id.clone(),
        armed: true,
    };
    if host.cut("pending") {
        return Err(crash(&mut admission, &mut upload));
    }
    // One import at a time unpacks and checks: each copies its bundle into staging and
    // checks every store there (a bounded page cache, never the bundle or a store in
    // memory), so imports that arrive together wait their turn (their uploads ran side
    // by side, and stay on the disk while they wait). Refused or failed, the staging is
    // gone (`StagedSession`), whether or not the turn is held then.
    let staged = host.root.join(INCOMING).join(&id);
    let turn = lock(&host.importing);
    let import = agent::stage_session(&path, &staged).map_err(|error| match error {
        UnpackError::Refused(why) => Failure::new(400, "bad_bundle", why),
        UnpackError::Io(error) => super::v1::internal(error),
    })?;
    // The credential check below may refresh a sign-in over the network or run a key
    // command: other imports take their turn meanwhile, while this one's staged copy
    // waits on the disk.
    drop(turn);
    drop(upload);
    let brought = import
        .sidecar("session.json")
        .map_err(super::v1::internal)?
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .ok_or_else(|| Failure::new(400, "bad_bundle", "the bundle's session has no settings"))?;
    let mut settings = Settings::from_json(&brought)
        .map_err(|why| Failure::new(400, "bad_bundle", format!("the bundle's settings: {why}")))?;
    let fit = fit(&mut settings, route);
    #[cfg(test)]
    {
        // Taken, and the lock let go, before it waits.
        let hold = lock(&host.checking).take();
        if let Some(wait) = hold {
            let _ = wait.recv();
        }
    }
    // The thread runs on this host's credentials (they may run a command: no lock held).
    if Settings::route_of(&settings.provider, Some(&settings.model))
        .and_then(|(provider, model)| Settings::resolve(&provider, Some(model)))
        .is_err()
    {
        return Err(Failure::new(
            409,
            "no_credentials",
            format!(
                "this host cannot send to {} {}: sign in there, or choose one of its models \
                 (`provider` and `model`; GET /v1/models lists them)",
                settings.provider, settings.model
            ),
        ));
    }
    // Put in place and opened (checked again) in turn, as one at a time is checked.
    let turn = lock(&host.importing);
    import.finish().map_err(super::v1::internal)?;
    drop(turn);
    settings.save(&staged).map_err(super::v1::internal)?;
    fit_tasks(&staged).map_err(super::v1::internal)?;
    if host.cut("staged") {
        return Err(crash(&mut admission, &mut Scratch(None)));
    }
    let dir = host.dir(&id);
    std::fs::rename(&staged, &dir).map_err(super::v1::internal)?;
    spindle_store::durable::sync_dir(&host.root.join(INCOMING))
        .and_then(|()| spindle_store::durable::sync_dir(&host.root.join("sessions")))
        .map_err(super::v1::internal)?;
    if host.cut("renamed") {
        return Err(crash(&mut admission, &mut Scratch(None)));
    }
    // One step: the row, its owner and the key's record commit together, and the
    // session is live from then on (the locks are held across both).
    {
        let mut db = lock(&host.db);
        let mut lives = lock(&host.lives);
        if lives.len() >= super::MAX_SESSIONS {
            return Err(limit());
        }
        let tx = db.transaction().map_err(super::v1::internal)?;
        tx.execute("INSERT INTO sessions VALUES(?1,?2,0)", params![id, name])
            .map_err(super::v1::internal)?;
        tx.execute("INSERT INTO owners VALUES(?1,?2)", params![id, client])
            .map_err(super::v1::internal)?;
        let recorded = tx
            .execute(
                "UPDATE threads SET state='done',fit=?3 WHERE client=?1 AND key=?2 AND state='pending'",
                params![client, key, fit.to_string()],
            )
            .map_err(super::v1::internal)?;
        if recorded != 1 {
            return Err(super::v1::internal("the import's record is missing"));
        }
        if host.cut("before-commit") {
            // Dropped uncommitted: what a stop before the commit leaves.
            drop(tx);
            drop(lives);
            drop(db);
            return Err(crash(&mut admission, &mut Scratch(None)));
        }
        tx.commit().map_err(super::v1::internal)?;
        lives.insert(id.clone(), Watch::new());
    }
    admission.armed = false;
    Ok(Imported {
        id,
        fit,
        replayed: false,
    })
}

/// A simulated stop of the host (tests, `Host::cut`): everything stays as a stopped
/// host leaves it, nothing is cleaned up.
fn crash(admission: &mut Admission<'_>, upload: &mut Scratch) -> Failure {
    admission.armed = false;
    upload.0 = None;
    Failure::new(500, "internal", "cut (test)")
}

fn still_importing() -> Failure {
    Failure::new(
        409,
        "busy",
        "a bundle with this Idempotency-Key is still being imported; send it again when that ends",
    )
}

fn limit() -> Failure {
    Failure::new(409, "conflict", "session limit reached")
}

/// What a thread's settings become on this host (applied, and said as the package's
/// `fit` says it: `changes`, the settings changed, and `notes`, `{setting, from, to,
/// why}` per setting). This host runs every thread's commands in its own sandbox,
/// where every tool works; it mounts its directories only for threads it made itself,
/// and only a thread it made from its own source edits Spindle. `route` is the client's
/// choice of provider and model.
fn fit(settings: &mut Settings, route: Option<(String, String)>) -> Value {
    let mut changes = Map::new();
    let mut notes = vec![];
    let mut note =
        |changes: &mut Map<String, Value>, setting: &str, from: Value, to: Value, why: &str| {
            if from != to {
                changes.insert(setting.into(), to.clone());
            }
            notes.push(json!({"setting": setting, "from": from, "to": to, "why": why}));
        };
    if let Some((provider, model)) = route {
        if provider != settings.provider {
            note(
                &mut changes,
                "provider",
                json!(settings.provider),
                json!(provider),
                "the client chose this host's provider",
            );
            settings.provider = provider;
        }
        if model != settings.model {
            note(
                &mut changes,
                "model",
                json!(settings.model),
                json!(model),
                "the client chose this host's model",
            );
            settings.model = model;
        }
    }
    if agent::describe_effort(&settings.provider, &settings.model, settings.effort).is_err() {
        note(
            &mut changes,
            "effort",
            json!(settings.effort.as_str()),
            json!(Effort::Default.as_str()),
            "the model here does not take that effort",
        );
        settings.effort = Effort::Default;
    }
    for ((setting, from, to), why) in task_fit(settings) {
        note(&mut changes, setting, from, to, why);
    }
    if let Some(max) = settings.max_usd {
        note(
            &mut changes,
            "max_usd",
            json!(max),
            json!(max),
            "the budget carries over: spend is counted here as there",
        );
    }
    json!({"changes": changes, "notes": notes})
}

type Change = (&'static str, Value, Value);

/// The part of `fit` every store of a thread gets, its tasks' included (a task that
/// runs again reads its own settings): the sandbox always, every tool, no runner, no
/// host directories, no self-edit. Returns what changed and why.
fn task_fit(settings: &mut Settings) -> Vec<(Change, &'static str)> {
    let mut changed = vec![];
    if settings.isolation != Isolation::Required {
        changed.push((
            (
                "isolation",
                json!(settings.isolation.as_str()),
                json!(Isolation::Required.as_str()),
            ),
            "this host runs every thread's commands in its sandbox",
        ));
        settings.isolation = Isolation::Required;
    }
    if settings.builtin_tools == BuiltinTools::Files {
        changed.push((
            ("builtin_tools", json!("files"), json!("all")),
            "this host has a command sandbox: bash, scripts and jobs work here",
        ));
        settings.builtin_tools = BuiltinTools::All;
    }
    if settings.runner != Runner::None {
        changed.push((
            (
                "runner",
                json!(settings.runner.as_str()),
                json!(Runner::None.as_str()),
            ),
            "commands run in this host's sandbox, not through a runner",
        ));
        settings.runner = Runner::None;
    }
    if !settings.sources.is_empty() {
        changed.push((
            ("sources", json!(settings.sources), json!([])),
            "this host mounts its directories only for threads it made",
        ));
        settings.sources.clear();
    }
    if settings.self_edit {
        changed.push((
            ("self_edit", json!(true), json!(false)),
            "only a thread this host made from its own source edits Spindle",
        ));
        settings.self_edit = false;
    }
    changed
}

/// `task_fit` for every task store under the staged session `dir`.
fn fit_tasks(dir: &Path) -> std::result::Result<(), String> {
    for place in agent::tasks::task_dirs(dir)? {
        if place.join("store").is_dir() {
            let mut settings = Settings::load(&place)?;
            if !task_fit(&mut settings).is_empty() {
                settings.save(&place)?;
            }
        }
        fit_tasks(&place)?;
    }
    Ok(())
}

/// A bundle ready to send: where it is, its size and what it holds.
pub(super) struct Exported {
    pub path: PathBuf,
    pub size: u64,
    pub head: u64,
    pub identity: String,
    /// A copy's file goes once sent; a move's stays until the session is deleted.
    pub temporary: bool,
}

/// Holds a session busy for an export (no command is admitted meanwhile), however it
/// ends.
struct Exporting(Arc<Watch>);
impl Drop for Exporting {
    fn drop(&mut self) {
        let mut live = self.0.lock();
        live.busy = false;
        // A stop sent meanwhile found nothing to stop.
        live.stopping = false;
        drop(live);
        self.0.changed.notify_all();
    }
}

/// `POST /v1/sessions/:id/export`: the session as a bundle. Without `keep` it moves:
/// sealed here once its bundle is on disk, and every later export sends that same
/// file. With `keep`, a copy; the session stays.
pub(super) fn export(
    host: &Arc<Host>,
    id: &str,
    keep: bool,
) -> std::result::Result<Exported, Failure> {
    let busy = || Failure::new(409, "busy", super::v1::BUSY);
    let live = host
        .live(id)
        .map_err(|_| Failure::new(404, "not_found", "session not found"))?;
    // Moved already: every export sends the same sealed bytes, busy or not (a client
    // whose earlier export's answer was lost asks again while nothing else may run here).
    if !keep && moved(&host.dir(id)) {
        return sealed(&host.dir(id), &host.dir(id).join(MOVED_FILE));
    }
    {
        let mut state = live.lock();
        if state.busy {
            return Err(busy());
        }
        if host.updating.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Failure::new(
                409,
                "conflict",
                "an upgrade or restart is in progress; wait for the host to reconnect",
            ));
        }
        state.busy = true;
    }
    let _exporting = Exporting(live.clone());
    let dir = host.dir(id);
    let kept = dir.join(MOVED_FILE);
    if moved(&dir) {
        if keep {
            return Err(Failure::new(409, "moved", MOVED_AWAY));
        }
        return sealed(&dir, &kept);
    }
    // An unsealed session's file is what an export that stopped before its seal left:
    // never sent (the session may have moved on since), written anew below.
    if std::fs::symlink_metadata(&kept).is_ok() {
        std::fs::remove_file(&kept)
            .and_then(|()| spindle_store::durable::sync_dir(&dir))
            .map_err(super::v1::internal)?;
    }
    let target = if keep {
        let incoming = host.root.join(INCOMING);
        private_dir(&incoming).map_err(super::v1::internal)?;
        incoming.join(format!(
            "copy-{}.spindle",
            &random().map_err(super::v1::internal)?[..32]
        ))
    } else {
        kept.clone()
    };
    let observer = Arc::new(Sink(live.clone(), None));
    // Another process holding the store (a background job's) is a busy session.
    let mut session = Session::open(&dir, observer).map_err(|why| {
        if why.contains("is using this session") {
            Failure::new(409, "busy", why)
        } else {
            super::v1::internal(why)
        }
    })?;
    let exported = session.export_session(&target, keep).map_err(|why| {
        if why.starts_with("background task") || why.contains("is in use") {
            Failure::new(409, "busy", why)
        } else {
            super::v1::internal(why)
        }
    })?;
    drop(session);
    Ok(Exported {
        size: exported["size"].as_u64().unwrap_or(0),
        head: exported["head"].as_u64().unwrap_or(0),
        identity: exported["identity"].as_str().unwrap_or_default().into(),
        path: target,
        temporary: keep,
    })
}

/// A sealed session's kept bundle, checked against the store it was sealed at.
fn sealed(dir: &Path, kept: &Path) -> std::result::Result<Exported, Failure> {
    let mut file = std::fs::File::open(kept).map_err(|_| {
        Failure::new(
            409,
            "moved",
            "this thread moved, and its bundle is no longer on this host",
        )
    })?;
    let size = file.metadata().map_err(super::v1::internal)?.len();
    let (head, identity) =
        spindle_store::bundle::manifest_facts(&mut file).map_err(super::v1::internal)?;
    let mut view =
        spindle_store::view::View::open(&dir.join("store")).map_err(super::v1::internal)?;
    let now = view
        .read(|view| view.head().map(|(head, _, _)| head))
        .map_err(super::v1::internal)?;
    if now != head {
        return Err(super::v1::internal(format!(
            "the kept bundle is at head {head}, the sealed store at {now}"
        )));
    }
    Ok(Exported {
        path: kept.to_path_buf(),
        size,
        head,
        identity,
        temporary: false,
    })
}

/// Sends an exported bundle: `application/octet-stream` with its head, identity and
/// length, read from the file in chunks.
pub(super) fn send(
    stream: &mut TcpStream,
    exported: &Exported,
    extra: &str,
) -> std::result::Result<(), Failure> {
    let _remove = Scratch(exported.temporary.then(|| exported.path.clone()));
    let mut file = std::fs::File::open(&exported.path).map_err(super::v1::internal)?;
    let length = usize::try_from(exported.size).map_err(super::v1::internal)?;
    http::headers(
        stream,
        200,
        "application/octet-stream",
        &format!(
            "X-Spindle-Head: {}\r\nX-Spindle-Identity: {}\r\n{extra}",
            exported.head, exported.identity
        ),
        Some(length),
    )
    .map_err(super::v1::internal)?;
    // From here the reply has begun: a failure only ends the connection (the client
    // sees a short body, and asks again).
    let _ = std::io::copy(&mut std::io::Read::take(&mut file, exported.size), stream);
    Ok(())
}

// The host these tests open runs only on Unix.
#[cfg(all(test, unix))]
#[path = "threads_tests.rs"]
mod tests;
