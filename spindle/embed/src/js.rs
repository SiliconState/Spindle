//! The wasm-bindgen surface. Guards are JavaScript callbacks, as a Web Lock's release
//! is: they are not `Send`, which `OwnerExclusion` allows on Wasm. Errors are
//! JavaScript `Error`s whose `code` is stable (`locked`, `incompatible`, `invalid`,
//! `conflict`, `failed`).
use super::{Error, OwnerExclusion, Store, node};
use spindle_store::embedded::{Catalog, Embedded, Places, answer_of};
use spindle_store::{Initialization, JournalMode};
use wasm_bindgen::prelude::*;

/// Called once, after SQLite has closed (or opening failed).
struct Release(js_sys::Function);
impl OwnerExclusion for Release {}
impl Drop for Release {
    fn drop(&mut self) {
        let _ = self.0.call0(&JsValue::NULL);
    }
}

/// A JavaScript error for `error`, with the storage layer's own reason when it has one.
pub(crate) fn fail(error: Error) -> JsValue {
    let code = match &error {
        Error::Locked => "locked",
        Error::Incompatible => "incompatible",
        Error::Invalid => "invalid",
        Error::Conflict => "conflict",
        _ => "failed",
    };
    let mut message = error.to_string();
    if let Some(reason) = node::take_last_error() {
        message = format!("{message}: {reason}");
    }
    let value = js_sys::Error::new(&message);
    let _ = js_sys::Reflect::set(&value, &"code".into(), &code.into());
    value.into()
}

/// Two memory authorities exercised end to end; the JSON report on success.
#[wasm_bindgen]
pub fn probe(first: js_sys::Function, second: js_sys::Function) -> Result<String, JsValue> {
    super::probe(Release(first), Release(second))
        .map(|report| report.to_string())
        .map_err(fail)
}

/// An open refused before any authority exists; the guard is still released.
#[wasm_bindgen(js_name = refusedOpen)]
pub fn refused_open(release: js_sys::Function) -> Result<(), JsValue> {
    super::refused_open(Release(release)).map_err(fail)
}

/// Gives the Node storage its `fs` (`node:fs`, or a wrapper) and `process.platform`.
#[wasm_bindgen(js_name = installNodeFs)]
pub fn install_node_fs(fs: node::NodeFs, platform: &str) -> Result<(), JsValue> {
    node::install(fs, platform).map_err(fail)
}

/// What the lock file of the store at `database` says about its owner.
#[wasm_bindgen(js_name = nodeLockHolder)]
pub fn node_lock_holder(database: &str) -> Option<String> {
    node::NodeOwner::holder(database)
}

pub(crate) fn journal(name: &str) -> Result<JournalMode, JsValue> {
    match name {
        "delete" => Ok(JournalMode::Delete),
        "wal" => Ok(JournalMode::Wal),
        _ => Err(fail(Error::Invalid)),
    }
}

/// A persistent store opened for the storage probe (tests and diagnostics; the
/// session API opens its own).
#[wasm_bindgen]
pub struct ProbeStore {
    store: Option<Store>,
}

#[wasm_bindgen]
impl ProbeStore {
    /// Creates (`create`) or opens the store at `database` through the Node VFS, owned
    /// by a lock file holding `note`.
    #[wasm_bindgen(js_name = openNode)]
    pub fn open_node(
        database: &str,
        journal_mode: &str,
        create: bool,
        note: &str,
    ) -> Result<ProbeStore, JsValue> {
        let initialization = if create {
            Initialization::Create
        } else {
            Initialization::Open
        };
        let store =
            node::open(database, journal(journal_mode)?, initialization, note).map_err(fail)?;
        Ok(Self { store: Some(store) })
    }

    fn store(&mut self) -> Result<&mut Store, JsValue> {
        self.store.as_mut().ok_or_else(|| fail(Error::Invalid))
    }

    /// Admits, commits and reserves the probe's effect; JSON {revision, epoch, identity}.
    pub fn reserve(&mut self) -> Result<String, JsValue> {
        super::reserve(self.store()?)
            .map(|seen| seen.to_string())
            .map_err(fail)
    }

    /// JSON {revision, epoch, identity, reserved, receipt}.
    pub fn report(&mut self) -> Result<String, JsValue> {
        super::report(self.store()?)
            .map(|seen| seen.to_string())
            .map_err(fail)
    }

    /// Records `reply` for the reserved attempt (idempotent; a different one is refused).
    pub fn settle(&mut self, reply: &str) -> Result<String, JsValue> {
        super::settle(self.store()?, reply.as_bytes())
            .map(|seen| seen.to_string())
            .map_err(fail)
    }

    /// Closes the store, then removes its lock file.
    pub fn close(&mut self) {
        self.store = None;
    }
}

fn refused(message: impl Into<String>) -> JsValue {
    let value = js_sys::Error::new(&message.into());
    let _ = js_sys::Reflect::set(&value, &"code".into(), &"invalid".into());
    value.into()
}

/// A JavaScript error for an API fault: its `code` is the fault's kind (`conflict`,
/// `busy`, `failed`, ...), as the client's `SpindleError` names it.
fn faulted(fault: spindle_store::protocol::Fault) -> JsValue {
    let value = js_sys::Error::new(&fault.message);
    let _ = js_sys::Reflect::set(&value, &"code".into(), &fault.kind.into());
    value.into()
}

fn frames(frames: Vec<serde_json::Value>) -> String {
    serde_json::Value::Array(frames).to_string()
}

/// The embedded sessions of one app (`spindle_store::embedded::Embedded`). Every call
/// returns the frames written since the last one, as a JSON array: replies, `event`
/// notifications, and `work`/`work_cancel` for the app (the `EmbeddedClient`).
#[wasm_bindgen(js_name = Embedded)]
pub struct JsEmbedded {
    inner: Embedded,
    opfs: Option<std::rc::Rc<super::places::OpfsPlaces>>,
}

#[wasm_bindgen(js_class = Embedded)]
impl JsEmbedded {
    /// `storage` is `memory`, `node` or `opfs` (after `installNodeFs` with `node:fs`,
    /// or with the OPFS adapter in a browser worker); `options` is JSON: `{journal:
    /// "wal"|"delete", note}` (`note` names this owner in Node lock files; an OPFS
    /// session is owned through `ownSession`). `catalog` is a provider catalog in
    /// Dext's format (JSON), or null for Dext's built-in providers.
    #[wasm_bindgen(constructor)]
    pub fn new(
        storage: &str,
        options: &str,
        catalog: Option<String>,
    ) -> Result<JsEmbedded, JsValue> {
        let options: serde_json::Value = match options {
            "" => serde_json::Value::Null,
            text => serde_json::from_str(text).map_err(|e| refused(e.to_string()))?,
        };
        let mode = journal(options["journal"].as_str().unwrap_or("wal"))?;
        let mut opfs = None;
        let places: Box<dyn Places> = match storage {
            "memory" => Box::new(super::places::MemoryPlaces),
            "node" => Box::new(super::places::NodePlaces {
                journal: mode,
                note: options["note"]
                    .as_str()
                    .unwrap_or("spindle-embed")
                    .to_string(),
            }),
            "opfs" => {
                let shared = std::rc::Rc::new(super::places::OpfsPlaces::new(mode));
                opfs = Some(shared.clone());
                Box::new(super::places::SharedOpfs(shared))
            }
            other => return Err(refused(format!("unknown storage `{other}`"))),
        };
        let catalog = match catalog {
            None => Catalog::builtin(),
            Some(text) => {
                let value: serde_json::Value =
                    serde_json::from_str(&text).map_err(|e| refused(e.to_string()))?;
                Catalog::from_json(&value).map_err(refused)?
            }
        };
        Ok(Self {
            inner: Embedded::new(places, catalog),
            opfs,
        })
    }

    /// OPFS storage: the app holds `dir`'s exclusive Web Lock; `release` gives it back,
    /// and is called once every store of the session has closed.
    #[wasm_bindgen(js_name = ownSession)]
    pub fn own_session(&self, dir: &str, release: js_sys::Function) -> Result<(), JsValue> {
        let opfs = self
            .opfs
            .as_ref()
            .ok_or_else(|| refused("only OPFS sessions are owned through Web Locks"))?;
        opfs.own(dir, release).map_err(refused)
    }

    /// One JSON-RPC request line.
    pub fn handle(&self, line: &str) -> String {
        frames(self.inner.handle(line.as_bytes()))
    }

    /// The provider catalog these sessions use, in Dext's format (JSON; no credentials):
    /// each provider's endpoint, contract and models, and the active provider.
    pub fn catalog(&self) -> String {
        self.inner.catalog().to_string()
    }

    /// The efforts a provider's model takes (JSON `[{effort, label, short}]`, `default`
    /// first; empty for a route the catalog does not know).
    pub fn efforts(&self, provider: &str, model: &str) -> String {
        self.inner.efforts(provider, model).to_string()
    }

    /// Session `dir` as a session bundle (bytes). Unless `keep`, the session moves: it
    /// closes here and its copy is sealed, continuing only where the bundle is imported.
    /// `settings` (JSON object) change the bundle's copy only, for the host it goes to;
    /// `whole`: a copy's stores are checked record by record too, as a move's are.
    #[wasm_bindgen(js_name = exportSession)]
    pub fn export_session(
        &self,
        dir: &str,
        keep: bool,
        settings: Option<String>,
        whole: Option<bool>,
    ) -> Result<Vec<u8>, JsValue> {
        let settings = settings
            .map(|text| serde_json::from_str(&text))
            .transpose()
            .map_err(|error| {
                faulted(spindle_store::protocol::bad(format!(
                    "settings are not JSON: {error}"
                )))
            })?;
        self.inner
            .export_session_checked(dir, keep, settings.as_ref(), whole.unwrap_or(false))
            .map_err(faulted)
    }

    /// Checks a session bundle whole, as an import does (its framing and checksum, its
    /// manifest, every path and every store), writing nothing: `{head, identity,
    /// files}` (JSON).
    #[wasm_bindgen(js_name = checkBundle)]
    pub fn check_bundle(&self, bytes: &[u8]) -> Result<String, JsValue> {
        let bundle = spindle_store::bundle::read(bytes).map_err(refused)?;
        Ok(
            serde_json::json!({"head": bundle.head, "identity": bundle.identity,
            "files": bundle.files.len()})
            .to_string(),
        )
    }

    /// Imports a session bundle as session `dir` (nothing there yet) and opens it; its
    /// status, with `imported` (JSON).
    #[wasm_bindgen(js_name = importSession)]
    pub fn import_session(&self, dir: &str, bytes: &[u8]) -> Result<String, JsValue> {
        self.inner
            .import_session(dir, bytes)
            .map(|status| status.to_string())
            .map_err(faulted)
    }

    /// The app's answer (JSON) to work `id` of session `session`.
    pub fn deliver(&self, session: &str, id: u32, answer: &str) -> Result<String, JsValue> {
        let value: serde_json::Value =
            serde_json::from_str(answer).map_err(|e| refused(e.to_string()))?;
        let answer = answer_of(&value).map_err(refused)?;
        self.inner
            .deliver(session, u64::from(id), answer)
            .map(frames)
            .map_err(refused)
    }

    /// A streamed part of the response to work `id`: `{frames, streaming}` (false:
    /// stop reading it).
    pub fn chunk(&self, session: &str, id: u32, bytes: &[u8]) -> String {
        let (frames, streaming) = self.inner.chunk(session, u64::from(id), bytes);
        serde_json::json!({"frames": frames, "streaming": streaming}).to_string()
    }

    /// One file (by object digest) of the workspace a runner call (work `id`) runs over.
    #[wasm_bindgen(js_name = runnerFile)]
    pub fn runner_file(&self, session: &str, id: u32, object: &str) -> Result<Vec<u8>, JsValue> {
        self.inner
            .runner_file(session, u64::from(id), object)
            .map_err(refused)
    }

    /// The remote runner's identity: a Spindle host's sandbox (64 hex digits), for
    /// sessions whose `runner` is `remote`; null disconnects it.
    #[wasm_bindgen(js_name = setRunner)]
    pub fn set_runner(&self, helper: Option<String>) -> Result<(), JsValue> {
        self.inner.set_runner(runner_identity(helper)?);
        Ok(())
    }

    /// The sandbox identity the app's own runner declares (64 hex digits), for
    /// sessions whose `runner` is `app`: its commands then count as isolated. null:
    /// the app's runner isolates nothing.
    #[wasm_bindgen(js_name = setAppRunner)]
    pub fn set_app_runner(&self, helper: Option<String>) -> Result<(), JsValue> {
        self.inner.set_app_runner(runner_identity(helper)?);
        Ok(())
    }

    /// The sites scripts' `http()` may reach (JSON: grants as a native code policy's
    /// `http` list, without headers); `[]` grants none. The app sends each request.
    #[wasm_bindgen(js_name = setHttpGrants)]
    pub fn set_http_grants(&self, grants: &str) -> Result<(), JsValue> {
        let grants = serde_json::from_str(grants).map_err(|error| {
            faulted(spindle_store::protocol::bad(format!(
                "grants are not JSON: {error}"
            )))
        })?;
        self.inner.set_http_grants(&grants).map_err(faulted)
    }
}

/// A runner identity from its 64 hex digits (None stays None).
fn runner_identity(helper: Option<String>) -> Result<Option<[u8; 32]>, JsValue> {
    let Some(hex) = helper else {
        return Ok(None);
    };
    let mut bytes = [0u8; 32];
    if hex.len() != 64 || !hex.is_ascii() {
        return Err(refused("a runner identity is 64 hex digits"));
    }
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * index..2 * index + 2], 16)
            .map_err(|_| refused("a runner identity is 64 hex digits"))?;
    }
    Ok(Some(bytes))
}
