//! Host-owned code-mode grants. The sandbox receives descriptors and results, never
//! this policy, provider credentials, or authority to choose an executable.
use crate::agent::{self, Event, Observer};
use crate::code_mode::{Host, validate_input};
use crate::pack_runtime::{self, Binding, Invoked, PackHost, PackUi, Risk, Snapshot};
use dext_native::process_tree::{ChildProcessTree, configure_std_process_group};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, String>;

#[derive(Clone)]
pub struct CodeHost {
    policy: Value,
    provider: String,
    model: String,
    /// The running turn's execution id (nested model identity, pack `turn_id`).
    session: String,
    /// The session's stable pack `session_id` (`session_identity`).
    identity: String,
    cwd: PathBuf,
    observer: Option<Arc<dyn Observer>>,
    credentials: Arc<dyn agent::Credentials>,
    /// Promoted tools of the session packs active for the running script.
    dynamic: BTreeMap<String, Vec<pack_runtime::Tool>>,
}

fn read_json(path: &Path) -> Result<Value> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 256 * 1024 {
        return Err("policy must be a regular JSON file below 256 KiB".into());
    }
    serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}
fn digest(path: &Path) -> Result<String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 256 * 1024 * 1024 {
        return Err("tool must be a regular executable below 256 MiB".into());
    }
    Ok(
        Sha256::digest(std::fs::read(path).map_err(|e| e.to_string())?)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}
fn fail(error: String, may_have_run: bool) -> Value {
    json!({"ok":false,"error":error,"may_have_run":may_have_run})
}

/// The user's session bus, for bound tools that keep credentials in the OS keyring
/// (the MCP bridge's OAuth tokens live in the Secret Service). A binding already runs
/// with the user's own authority and could open the bus socket by its path; this only
/// tells it where the bus is. A host started without these variables (a systemd user
/// service) falls back to the standard per-user runtime directory when it exists.
#[cfg(unix)]
fn session_bus() -> Vec<(&'static str, std::ffi::OsString)> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            // SAFETY: getuid has no preconditions and cannot fail.
            let dir = PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
            dir.is_dir().then_some(dir)
        });
    let bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").or_else(|| {
        let socket = runtime.as_ref()?.join("bus");
        socket
            .exists()
            .then(|| format!("unix:path={}", socket.display()).into())
    });
    [
        ("XDG_RUNTIME_DIR", runtime.map(PathBuf::into_os_string)),
        ("DBUS_SESSION_BUS_ADDRESS", bus),
    ]
    .into_iter()
    .filter_map(|(name, value)| Some((name, value?)))
    .collect()
}

/// The session's pack `session_id`, stable for its whole life as Dext's is: kept in
/// the session directory (`pack-session-id`) once made, so restarts and moves keep it.
/// One that cannot be saved is derived from the directory's path instead.
fn session_identity(dir: &Path) -> String {
    let path = dir.join("pack-session-id");
    let valid = |id: &str| {
        (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    if let Ok(text) = std::fs::read_to_string(&path)
        && valid(text.trim())
    {
        return text.trim().to_string();
    }
    let hex = |bytes: &[u8]| -> String {
        Sha256::digest(bytes)
            .iter()
            .take(6)
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let fresh = format!(
        "{}-{}-{}",
        now.as_secs(),
        std::process::id(),
        hex(format!(
            "{}\0{}\0{}",
            now.as_nanos(),
            std::process::id(),
            dir.display()
        )
        .as_bytes())
    );
    if agent::save_private(&path, fresh.as_bytes()).is_ok() {
        return fresh;
    }
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.into());
    format!("dir-{}", hex(dir.display().to_string().as_bytes()))
}

impl CodeHost {
    pub fn open(
        provider: &str,
        model: &str,
        session: &str,
        cwd: &Path,
        observer: Option<Arc<dyn Observer>>,
    ) -> Result<Self> {
        let policy_path = std::env::var_os("SPINDLE_CODE_POLICY")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent()?.parent().map(|p| p.join("code-policy.json")))
            });
        let policy = match policy_path {
            Some(path) if path.exists() => read_json(&path)?,
            Some(_) if std::env::var_os("SPINDLE_CODE_POLICY").is_some() => {
                return Err("SPINDLE_CODE_POLICY does not exist".into());
            }
            _ => json!({"version":1,"models":[],"tools":[]}),
        };
        Self::with_policy(policy, provider, model, session, cwd, observer)
    }

    /// A host over an already loaded policy (tests and embedders).
    pub fn with_policy(
        policy: Value,
        provider: &str,
        model: &str,
        session: &str,
        cwd: &Path,
        observer: Option<Arc<dyn Observer>>,
    ) -> Result<Self> {
        if policy["version"] != 1
            || !policy["models"].is_array()
            || !policy["tools"].is_array()
            || policy["models"].as_array().unwrap().len() > 32
            || policy["tools"].as_array().unwrap().len() > 128
        {
            return Err("invalid code policy v1".into());
        }
        let mut names = std::collections::BTreeSet::new();
        for entry in policy["tools"].as_array().unwrap() {
            let name = entry["name"]
                .as_str()
                .filter(|n| !n.is_empty() && n.len() <= 128)
                .ok_or("invalid tool identity")?;
            if !names.insert(name) {
                return Err("duplicate code tool identity".into());
            }
        }
        crate::code_http::validate(&policy)?;
        Ok(Self {
            policy,
            provider: provider.into(),
            model: model.into(),
            session: session.into(),
            identity: session_identity(cwd),
            cwd: cwd.into(),
            observer,
            credentials: Arc::new(agent::NativeCredentials),
            dynamic: BTreeMap::new(),
        })
    }
    pub fn with_credentials(mut self, credentials: Arc<dyn agent::Credentials>) -> Self {
        self.credentials = credentials;
        self
    }

    fn nested(&self, label: &str) {
        if let Some(observer) = &self.observer {
            observer.event(Event::Nested { label });
        }
    }
    fn entry(&self, name: &str) -> Result<&Value> {
        self.policy["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == name)
            .ok_or("tool is not granted by SPINDLE_CODE_POLICY".into())
    }
    /// `{"provider":"*"}` grants every catalog provider; a missing `model` grants every
    /// model of that provider. Without an explicit provider a script uses the thread's
    /// route when granted, else the first explicitly granted provider.
    fn model_route(&self, input: &Value) -> Result<(String, Option<String>)> {
        let grants = self.policy["models"].as_array().unwrap();
        let granted = |provider: &str, model: Option<&str>| {
            grants.iter().any(|v| {
                (v["provider"] == "*" || v["provider"] == provider)
                    && (v["model"].is_null() || v["model"] == "*" || v["model"].as_str() == model)
            })
        };
        let explicit = input["model"].as_str().map(str::to_string);
        let concrete = |v: &Value| {
            v["model"]
                .as_str()
                .filter(|m| *m != "*")
                .map(str::to_string)
        };
        // Each candidate is a whole route, so a grant for another model of the thread's
        // provider (glm/glm-5.3-flash on a glm-5.3 thread) is still reachable.
        let candidates: Vec<(String, Option<String>)> = match input["provider"].as_str() {
            Some(provider) => match &explicit {
                Some(model) => vec![(provider.to_string(), Some(model.clone()))],
                // Thread model first, then granted models of that provider, then its
                // catalog default (allowed only by a provider-wide grant).
                None => (provider == self.provider)
                    .then(|| (provider.to_string(), Some(self.model.clone())))
                    .into_iter()
                    .chain(
                        grants
                            .iter()
                            .filter(|v| v["provider"] == provider)
                            .filter_map(concrete)
                            .map(|m| (provider.to_string(), Some(m))),
                    )
                    .chain(std::iter::once((provider.to_string(), None)))
                    .collect(),
            },
            None => std::iter::once((
                self.provider.clone(),
                explicit.clone().or(Some(self.model.clone())),
            ))
            .chain(grants.iter().filter_map(|v| {
                let provider = v["provider"].as_str().filter(|p| *p != "*")?;
                Some((
                    provider.to_string(),
                    explicit.clone().or_else(|| concrete(v)),
                ))
            }))
            .collect(),
        };
        for (provider, model) in candidates {
            if granted(&provider, model.as_deref()) {
                return Ok((provider, model));
            }
        }
        let list: Vec<String> = grants
            .iter()
            .map(|v| {
                let provider = v["provider"].as_str().unwrap_or("?");
                match v["model"].as_str().filter(|m| *m != "*") {
                    Some(model) => format!("{provider}/{model}"),
                    None => format!("{provider} (any model)"),
                }
            })
            .collect();
        Err(if list.is_empty() {
            "no nested models are granted by the operator's code policy".into()
        } else {
            format!(
                "nested {}{} is not granted by the operator's code policy; granted: {}; pass provider= (and model=) to choose one",
                input["provider"].as_str().unwrap_or(&self.provider),
                input["model"]
                    .as_str()
                    .map(|m| format!("/{m}"))
                    .unwrap_or_default(),
                list.join(", ")
            )
        })
    }
    fn descriptor(&self, entry: &Value) -> Result<Value> {
        if let Some(pins) = entry["files"].as_array() {
            if pins.len() > 16 {
                return Err("too many tool dependency pins".into());
            }
            for pin in pins {
                let path = Path::new(pin["path"].as_str().ok_or("dependency path required")?);
                if !path.is_absolute() || digest(path)? != pin["sha256"].as_str().unwrap_or("") {
                    return Err("reviewed tool dependency changed; operator review required".into());
                }
            }
        }
        let executable = entry["command"].as_str().ok_or("tool command required")?;
        let path = Path::new(executable);
        if !path.is_absolute() || digest(path)? != entry["sha256"].as_str().unwrap_or("") {
            return Err("tool executable changed; operator review required".into());
        }
        let args = entry["args"]
            .as_array()
            .ok_or("tool args must be an array")?;
        if args.len() > 64
            || args.iter().any(|a| {
                a.as_str()
                    .is_none_or(|s| s.len() > 4096 || s.contains('\0'))
            })
        {
            return Err("invalid tool argv".into());
        }
        if !matches!(entry["risk"].as_str(), Some("read" | "write" | "danger")) {
            return Err("tool risk must be explicitly granted".into());
        }
        if entry["protocol"] == "pack-v1"
            && !matches!(entry["lifecycle"].as_str(), Some("one-shot" | "session"))
        {
            return Err("pack binding needs an explicit reviewed lifecycle grant (\"one-shot\" or \"session\")".into());
        }
        // A session pack's manifest decides its protocols, so it must be pinned.
        if entry["lifecycle"] == "session"
            && !entry["manifest"].as_str().is_some_and(|manifest| {
                Path::new(manifest).is_absolute()
                    && entry["files"]
                        .as_array()
                        .is_some_and(|pins| pins.iter().any(|pin| pin["path"] == manifest))
            })
        {
            return Err(
                "a session pack binding needs its runtime.json as `manifest`, pinned in `files`"
                    .into(),
            );
        }
        if entry["protocol"] == "pack-v1" && !entry["pack"].is_string() {
            return Err("pack binding needs a pack identity".into());
        }
        if !matches!(entry["protocol"].as_str(), None | Some("json" | "pack-v1")) {
            return Err("unsupported external tool protocol".into());
        }
        if let Some(path) = entry["cwd"].as_str()
            && (!Path::new(path).is_absolute() || !Path::new(path).is_dir())
        {
            return Err("external cwd must be an existing absolute directory".into());
        }
        Ok(entry.clone())
    }
}

impl CodeHost {
    /// The session's app tool named `name`, unless the operator's policy names one so.
    fn app_tool(&self, name: &str) -> Option<agent::AppTool> {
        if self.entry(name).is_ok() {
            return None;
        }
        agent::Settings::load(&self.cwd)
            .ok()?
            .tools
            .into_iter()
            .find(|tool| tool.name == name)
    }

    /// Asks the app to answer a script's call of one of its tools. The call is named by
    /// its journal stage (`prepared.call`); file changes are kept only for the model's
    /// own calls, so a script is told they were not applied.
    fn execute_app(&self, prepared: &Value, interrupt: &AtomicBool, remaining: Duration) -> Value {
        let name = prepared["name"].as_str().unwrap_or("?");
        self.nested(&format!("app tool: {name}"));
        let Some(answerer) = self
            .observer
            .as_ref()
            .and_then(|observer| observer.app_tools())
        else {
            return fail("no app is attached to answer its tools".into(), false);
        };
        let Some(tool) = self.app_tool(name) else {
            return fail(
                format!("the session no longer offers the app tool {name}"),
                false,
            );
        };
        let call = prepared["call"].as_str().unwrap_or_default();
        let external = match crate::meta::external_call_id(&self.cwd, call) {
            Ok(id) => format!("code-{id}"),
            Err(error) => return fail(format!("cannot identify app call: {error}"), false),
        };
        let ask = agent::AppCall::new(
            &external,
            call,
            name,
            &prepared["input"],
            false,
            tool.rerun,
            Duration::from_secs(tool.timeout_s).min(remaining),
        );
        let mut reply = match answerer.call(&ask, interrupt) {
            agent::AppAnswer::Result {
                content,
                failed,
                writes,
                deletes,
                links,
                directories,
                state,
                details,
            } => {
                let mut reply = json!({"ok": !failed, "data": content, "_spindle_app": {"max_output":tool.max_output,"details":details}});
                if failed {
                    reply["error"] = reply["data"].clone();
                    reply["may_have_run"] = json!(true);
                }
                if !writes.is_empty()
                    || !deletes.is_empty()
                    || !links.is_empty()
                    || !directories.is_empty()
                    || state.is_some()
                {
                    reply["warning"] = json!(
                        "the app's file and state changes are kept only when the model calls the tool itself"
                    );
                }
                reply
            }
            agent::AppAnswer::Unknown(note) => {
                let mut error = "the app could not confirm whether the call ran".to_string();
                if !note.trim().is_empty() {
                    error = format!("{error}: {}", note.trim());
                }
                json!({"ok": false, "may_have_run": true, "unknown": true, "error": error})
            }
            agent::AppAnswer::NoAnswer => {
                json!({"ok": false, "may_have_run": true, "unknown": true, "error": "no answer from the app in time; the call may still have run"})
            }
            _ => {
                json!({"ok": false, "may_have_run": true, "unknown": true, "error": "stopped while the app was answering"})
            }
        };
        reply["_spindle_app"]["max_output"] = json!(tool.max_output);
        reply
    }
}

impl Host for CodeHost {
    fn recorded(&mut self, prepared: &Value, private: &Value) {
        if prepared["kind"] == "app"
            && let Some(details) = private.get("details").filter(|details| !details.is_null())
            && let Some(observer) = &self.observer
            && let Some(call) = prepared["call"].as_str()
            && let Ok(call) = crate::meta::external_call_id(&self.cwd, call)
        {
            observer.event(Event::AppToolDetails {
                call: &format!("code-{call}"),
                details,
            });
        }
    }

    fn limits(&self) -> crate::code_mode::Limits {
        crate::code_mode::Limits::from_policy(&self.policy)
    }

    fn parallel(&self) -> Option<crate::code_mode::ParallelExecute> {
        let host = self.clone();
        Some(Arc::new(move |prepared, interrupt, remaining| {
            host.clone().execute(prepared, interrupt, remaining)
        }))
    }

    /// The policy's `limits.max_usd` per script and the thread's own budget (`cwd` is
    /// the session directory), against the new children's worst case.
    fn admit_spend(&mut self, prepared: &[&Value], journal: &[&Value]) -> Result<()> {
        agent::admit_nested_spend(
            &self.cwd,
            self.policy["limits"]["max_usd"].as_f64(),
            prepared,
            journal,
        )
    }

    fn discover(&mut self, method: &str, input: &Value) -> Result<Value> {
        if method == "describe" {
            let name = input["name"].as_str().ok_or("tool name required")?;
            self.nested(&format!("describe {name}"));
            if let Some(tool) = self.app_tool(name) {
                return Ok(
                    json!({"name":tool.name,"description":tool.description,"risk":"app","input_schema":tool.input_schema,"protocol":"app"}),
                );
            }
            if self.entry(name).is_err()
                && let Some((pack, tool)) = self.promoted(name)
            {
                return Ok(
                    json!({"name":name,"description":tool.description,"risk":tool.risk.as_str(),"input_schema":tool.input_schema,"protocol":"pack-v1","pack":pack,"promoted":true}),
                );
            }
            let entry = self.entry(name)?;
            return Ok(
                json!({"name":entry["name"],"description":entry["description"],"risk":entry["risk"],"input_schema":entry["input_schema"],"protocol":entry["protocol"]}),
            );
        }
        let query = input["query"].as_str().unwrap_or("").to_lowercase();
        self.nested(&format!(
            "search tools: {}",
            query.chars().take(80).collect::<String>()
        ));
        let promoted = self.dynamic.iter().flat_map(|(pack, tools)| {
            tools.iter().map(move |tool| {
                json!({"name":format!("{pack}/{}", tool.name),"description":tool.description,"risk":tool.risk.as_str(),"promoted":true})
            })
        });
        let app: Vec<Value> = agent::Settings::load(&self.cwd)
            .map(|settings| settings.tools)
            .unwrap_or_default()
            .into_iter()
            .filter(|tool| self.entry(&tool.name).is_err())
            .map(|tool| json!({"name":tool.name,"description":tool.description,"risk":"app"}))
            .collect();
        Ok(Value::Array(
            self.policy["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| json!({"name":t["name"],"description":t["description"],"risk":t["risk"]}))
                .chain(promoted)
                .chain(app)
                .filter(|t| {
                    format!("{} {}", t["name"], t["description"])
                        .to_lowercase()
                        .contains(&query)
                })
                .take(20)
                .collect(),
        ))
    }
    fn prepare(&mut self, method: &str, input: &Value) -> Result<Value> {
        if method == "model" {
            let (provider, model) = self.model_route(input)?;
            let route = self.credentials.route(&provider, model.as_deref())?;
            return crate::shape::code_model_request(
                input,
                &route,
                &self.session,
                &agent::image_models,
            );
        }
        if method == "http" {
            return crate::code_http::prepare(&self.policy, input);
        }
        let name = input["name"].as_str().ok_or("tool name required")?;
        if let Some(tool) = self.app_tool(name) {
            validate_input(&tool.input_schema, &input["arguments"], 0)?;
            return Ok(
                json!({"kind":"app","name":name,"input":input["arguments"],"session":self.session}),
            );
        }
        if self.entry(name).is_err()
            && let Some((pack, tool)) = self.promoted(name)
        {
            pack_runtime::validate_tool_input(&tool, &input["arguments"])?;
            return Ok(
                json!({"kind":"pack","pack":pack,"tool":tool.name,"input":input["arguments"],"session":self.session,"promoted":true}),
            );
        }
        let descriptor = self.descriptor(self.entry(name)?)?;
        let schema = &descriptor["input_schema"];
        validate_input(schema, &input["arguments"], 0)?;
        if descriptor["lifecycle"] == "session" {
            return Ok(
                json!({"kind":"pack","pack":descriptor["pack"],"tool":descriptor["tool"],"input":input["arguments"],"session":self.session}),
            );
        }
        Ok(
            json!({"kind":"tool","descriptor":descriptor,"input":input["arguments"],"session":self.session}),
        )
    }
    fn execute(&mut self, prepared: &Value, interrupt: &AtomicBool, remaining: Duration) -> Value {
        if interrupt.load(Ordering::SeqCst) || remaining.is_zero() {
            return fail("external call stopped before dispatch".into(), false);
        }
        if prepared["kind"] == "model" {
            self.nested(&format!(
                "nested model call: {} / {}",
                prepared["provider"].as_str().unwrap_or("?"),
                prepared["model"].as_str().unwrap_or("?")
            ));
            return agent::execute_code_model_with_credentials(
                prepared,
                interrupt,
                remaining,
                self.credentials.as_ref(),
            );
        }
        if prepared["kind"] == "http" {
            self.nested(&format!(
                "http {} {}",
                prepared["method"].as_str().unwrap_or("?"),
                prepared["url"].as_str().unwrap_or("?")
            ));
            return crate::code_http::execute(&self.policy, prepared, interrupt, remaining);
        }
        if prepared["kind"] == "pack" {
            return fail("session pack calls run through the store".into(), false);
        }
        if prepared["kind"] == "app" {
            return self.execute_app(prepared, interrupt, remaining);
        }
        let descriptor = &prepared["descriptor"];
        self.nested(&format!(
            "external tool: {}",
            descriptor["name"].as_str().unwrap_or("?")
        ));
        if let Err(error) = self.descriptor(descriptor) {
            return fail(error, false);
        }
        let protocol = descriptor["protocol"].as_str().unwrap_or("json");
        let input = match protocol {
            "json" => prepared["input"].clone(),
            "pack-v1" => {
                json!({"version":1,"event":"tool","pack":descriptor["pack"],"session_id":self.identity,"cwd":self.cwd,"state":{},"context":{"turn_id":self.session,"iteration":0,"history_messages":0,"compacted":false,"ui_methods":[]},"tool":descriptor["tool"],"input":prepared["input"]})
            }
            _ => return fail("unsupported tool protocol".into(), false),
        };
        let timeout = Duration::from_secs(
            descriptor["timeout_seconds"]
                .as_u64()
                .unwrap_or(30)
                .clamp(1, dext_native::strict::MAX_RUN_SECS),
        )
        .min(remaining);
        let cwd = descriptor["cwd"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cwd.clone());
        match execute(descriptor, &input, &cwd, interrupt, timeout) {
            Ok(output) if protocol == "pack-v1" => {
                if output["version"] != 1
                    || !output["content"].is_string()
                    || output.get("is_error").is_some_and(|v| !v.is_boolean())
                {
                    return fail("invalid pack response envelope".into(), true);
                }
                // A helper that says `may_have_run` (the MCP bridge after dispatch) does
                // not know whether its effect happened: an unknown outcome.
                if output["is_error"] == true
                    && output["content"]
                        .as_str()
                        .is_some_and(|c| c.trim_start().starts_with("may_have_run"))
                {
                    return json!({"ok":false,"may_have_run":true,"unknown":true,"error":output["content"]});
                }
                // State/effects need a separate lifecycle. Never silently discard them.
                if !empty_pack_field(&output["state"])
                    || !empty_pack_field(&output["effects"])
                    || !output["ui_request"].is_null()
                {
                    return fail("stateful pack effects are unsupported by this one-shot binding; use the pack's supervised host".into(), true);
                }
                let failed = output["is_error"].as_bool().unwrap_or(false);
                let not_sent = output["content"]
                    .as_str()
                    .is_some_and(pack_runtime::not_sent);
                json!({"ok":!failed,"may_have_run":failed && !not_sent,"data":output,"error":output["content"]})
            }
            Ok(output) => json!({"ok":true,"data":output}),
            Err(error) => external_failure(error),
        }
    }

    fn packs(&mut self) -> Option<&mut dyn PackHost> {
        Some(self)
    }
}

/// A stopped helper (timeout, interrupt, output flood) has an unknown outcome; one
/// that never started ran nothing.
fn external_failure(error: String) -> Value {
    if let Some(reason) = error.strip_prefix(NOT_STARTED) {
        fail(format!("external tool did not start: {reason}"), false)
    } else if error.starts_with(STOPPED) {
        json!({"ok":false,"may_have_run":true,"unknown":true,"error":error})
    } else {
        fail(error, true)
    }
}

const NOT_STARTED: &str = "not started: ";
const STOPPED: &str = "external tool stopped";

impl CodeHost {
    /// Session-pack policy entries for `pack`.
    fn session_entries(&self, pack: &str) -> Vec<&Value> {
        self.policy["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| {
                t["protocol"] == "pack-v1" && t["lifecycle"] == "session" && t["pack"] == pack
            })
            .collect()
    }

    /// A promoted tool (`pack/tool`) of a session pack active for this script.
    fn promoted(&self, name: &str) -> Option<(String, pack_runtime::Tool)> {
        let (pack, tool) = name.split_once('/')?;
        let tool = self.dynamic.get(pack)?.iter().find(|t| t.name == tool)?;
        Some((pack.to_string(), tool.clone()))
    }

    /// The highest risk `pack`'s promoted tools may have under the policy as it is now:
    /// its granted entries' highest risk, lowered by an explicit `max_dynamic_risk`.
    fn ceiling(&self, pack: &str) -> Result<Risk> {
        let entries = self.session_entries(pack);
        if entries.is_empty() {
            return Err(format!(
                "pack {pack} is not granted with a session lifecycle by the operator's code policy"
            ));
        }
        let mut ceiling = Risk::Read;
        for entry in &entries {
            if let Some(risk) = entry["risk"].as_str().and_then(Risk::parse) {
                ceiling = ceiling.max(risk);
            }
        }
        for entry in &entries {
            if let Some(risk) = entry.get("max_dynamic_risk") {
                ceiling = ceiling.min(
                    risk.as_str()
                        .and_then(Risk::parse)
                        .ok_or("max_dynamic_risk must be read, write or danger")?,
                );
            }
        }
        Ok(ceiling)
    }
}

impl PackHost for CodeHost {
    fn binding(&mut self, pack: &str) -> Result<Binding> {
        let entries = self.session_entries(pack);
        let Some(base) = entries.first() else {
            return Err(format!(
                "pack {pack} is not granted with a session lifecycle by the operator's code policy"
            ));
        };
        let identity = |entry: &Value| {
            json!([
                entry["command"],
                entry["args"],
                entry["sha256"],
                entry["files"],
                entry["cwd"],
                entry["manifest"],
                entry["timeout_seconds"]
            ])
        };
        if entries
            .iter()
            .any(|entry| identity(entry) != identity(base))
        {
            return Err(format!(
                "the policy binds pack {pack} with different commands, pins or manifests; operator review required"
            ));
        }
        let descriptor = self.descriptor(base)?;
        let manifest_path = descriptor["manifest"].as_str().unwrap_or_default();
        let bytes = std::fs::read(manifest_path).map_err(|e| e.to_string())?;
        let manifest_sha256: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let pinned = descriptor["files"]
            .as_array()
            .and_then(|pins| pins.iter().find(|pin| pin["path"] == manifest_path))
            .and_then(|pin| pin["sha256"].as_str());
        if pinned != Some(manifest_sha256.as_str()) {
            return Err("reviewed pack manifest changed; operator review required".into());
        }
        let manifest = pack_runtime::parse_manifest(&bytes)?;
        let command = descriptor["command"].as_str().unwrap_or_default();
        let args: Vec<String> = descriptor["args"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if !Path::new(command).ends_with(&manifest.command) || args != manifest.args {
            return Err(format!(
                "the policy's command or args for pack {pack} differ from its manifest"
            ));
        }
        let mut granted = vec![];
        for entry in &entries {
            let tool = entry["tool"].as_str().unwrap_or_default();
            if !manifest.tools.iter().any(|t| t.name == tool) {
                return Err(format!("pack {pack} does not declare granted tool {tool}"));
            }
            granted.push(tool.to_string());
        }
        let ceiling = self.ceiling(pack)?;
        Ok(Binding {
            pack: pack.into(),
            source: command.into(),
            manifest_sha256,
            executable_sha256: descriptor["sha256"].as_str().unwrap_or_default().into(),
            manifest,
            granted,
            max_dynamic_risk: ceiling,
        })
    }

    fn has_session_packs(&self) -> bool {
        self.policy["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["lifecycle"] == "session")
    }

    /// Promoted tools follow the policy as it is now: none for a pack whose grant is
    /// gone, and none above its current risk ceiling (the store checks again per call).
    fn refresh(&mut self, packs: &BTreeMap<String, Snapshot>) {
        self.dynamic = packs
            .iter()
            .filter_map(|(pack, snapshot)| {
                let ceiling = self.ceiling(pack).ok()?;
                let tools = snapshot
                    .tools
                    .iter()
                    .filter(|tool| tool.risk <= ceiling)
                    .cloned()
                    .collect();
                Some((pack.clone(), tools))
            })
            .collect();
    }

    fn invoke(
        &mut self,
        pack: &str,
        request: &Value,
        interrupt: &AtomicBool,
        remaining: Duration,
    ) -> Invoked {
        let Some(base) = self.session_entries(pack).first().map(|e| (*e).clone()) else {
            return Invoked::NotStarted(format!("pack {pack} is not granted"));
        };
        self.nested(&format!(
            "pack {pack}: {}{}",
            request["event"].as_str().unwrap_or("?"),
            request["tool"]
                .as_str()
                .map(|t| format!(" {t}"))
                .unwrap_or_default()
        ));
        let descriptor = match self.descriptor(&base) {
            Ok(descriptor) => descriptor,
            Err(error) => return Invoked::NotStarted(error),
        };
        if interrupt.load(Ordering::SeqCst) || remaining.is_zero() {
            return Invoked::NotStarted("pack call stopped before dispatch".into());
        }
        let timeout = Duration::from_secs(
            descriptor["timeout_seconds"]
                .as_u64()
                .unwrap_or(120)
                .clamp(1, dext_native::strict::MAX_RUN_SECS),
        )
        .min(remaining);
        let cwd = descriptor["cwd"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cwd.clone());
        match execute(&descriptor, request, &cwd, interrupt, timeout) {
            Ok(output) => Invoked::Output(output),
            Err(error) => match error.strip_prefix(NOT_STARTED) {
                Some(reason) => Invoked::NotStarted(reason.into()),
                None if error.starts_with(STOPPED) => Invoked::Unknown(error),
                None => Invoked::Failed(error),
            },
        }
    }

    fn ui(&self) -> Option<Arc<dyn PackUi>> {
        self.observer
            .as_ref()
            .and_then(|observer| observer.pack_ui())
    }

    fn view(&self, pack: &str, title: &str, markdown: &str) {
        if let Some(observer) = &self.observer {
            observer.event(Event::PackView {
                pack,
                title,
                markdown,
            });
        }
    }

    fn session(&self) -> String {
        self.identity.clone()
    }

    fn turn(&self) -> String {
        self.session.clone()
    }

    fn cwd(&self) -> String {
        self.cwd.display().to_string()
    }
}

fn empty_pack_field(value: &Value) -> bool {
    value.is_null()
        || value.as_object().is_some_and(|v| v.is_empty())
        || value.as_array().is_some_and(|v| v.is_empty())
}

/// Offline pack discovery. Nothing is launched, activated, or granted here.
pub fn packs() -> Result<Value> {
    let roots = match std::env::var_os("DEXT_SHELVES_DIR") {
        Some(root) => vec![PathBuf::from(root)],
        None => vec![
            PathBuf::from(std::env::var_os("HOME").ok_or("HOME missing")?).join(".dext/shelves"),
        ],
    };
    let mut found = vec![];
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        for shelf in std::fs::read_dir(root).map_err(|e| e.to_string())?.take(64) {
            let shelf = shelf.map_err(|e| e.to_string())?.path();
            let dir = shelf.join("packs");
            if !dir.is_dir() {
                continue;
            }
            for pack in std::fs::read_dir(dir).map_err(|e| e.to_string())?.take(128) {
                let path = pack.map_err(|e| e.to_string())?.path();
                if path.join("PACK.md").is_file() {
                    found.push(json!({"name":path.file_name().and_then(|v|v.to_str()),"docs":path.join("PACK.md"),"runtime":path.join("runtime.json").is_file()}));
                }
            }
        }
    }
    Ok(json!(found))
}

/// Export reviewed-manifest bindings for operator review, not activation. The caller
/// chooses an exact manifest; instruction-only packs remain documentation.
pub fn pack_candidate(manifest: &Path) -> Result<Value> {
    let manifest = manifest.canonicalize().map_err(|e| e.to_string())?;
    let root = manifest.parent().ok_or("pack root missing")?;
    let value = read_json(&manifest)?;
    if value["version"] != 1 {
        return Err("unsupported pack runtime version".into());
    }
    // Dext's own manifest rules (unknown fields, protocols, schemas, risks).
    let parsed =
        pack_runtime::parse_manifest(&std::fs::read(&manifest).map_err(|e| e.to_string())?)?;
    let relative = Path::new(value["command"].as_str().ok_or("pack command required")?);
    if relative.is_absolute()
        || relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err("pack command must stay under its root".into());
    }
    let command = root.join(relative);
    if !command
        .canonicalize()
        .map_err(|e| e.to_string())?
        .starts_with(root)
    {
        return Err("pack executable escapes root".into());
    }
    let hash = digest(&command)?;
    let pack = root
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or("invalid pack name")?;
    let mut pins = vec![json!({"path":manifest,"sha256":digest(&manifest)?})];
    let args = value["args"].as_array().cloned().unwrap_or_default();
    // MCP's reviewed bundle already binds transport/catalogues. Pin its bytes too.
    let mut bundle = None;
    for pair in args.windows(2) {
        if pair[0] == "--bundle" {
            let path = Path::new(pair[1].as_str().ok_or("bundle path required")?);
            pins.push(json!({"path":path,"sha256":digest(path)?}));
            bundle = Some(read_bundle(path)?);
        }
    }
    // Promotion, continuations and UI need host-held state between calls.
    let stateful = parsed.tool_protocol == Some(1)
        || parsed.ui_protocol == Some(1)
        || parsed.max_continuations > 0;
    let mut seen = std::collections::BTreeSet::new();
    if args.len() > 64 || value["tools"].as_array().is_none_or(|t| t.len() > 128) {
        return Err("pack metadata bounds exceeded".into());
    }
    let mut tools = vec![];
    for tool in value["tools"]
        .as_array()
        .ok_or("pack tools required")?
        .iter()
        .take(128)
    {
        let name = tool["name"].as_str().ok_or("pack tool name required")?;
        if !seen.insert(name) {
            return Err("duplicate pack tool identity".into());
        }
        let declared = parsed
            .tools
            .iter()
            .find(|t| t.name == name)
            .ok_or("pack tool missing from the parsed manifest")?;
        let mut notes = vec![];
        let mut lifecycle = "review-required";
        if let Some(bundle) = &bundle {
            notes.extend(mcp_notes(name, bundle));
            if name.starts_with("mcp_ui") {
                lifecycle = "unsupported";
            }
        }
        if declared.risk != Risk::Read && notes.is_empty() {
            notes.push("can change things outside the workspace; external effects are journaled but never rolled back".to_string());
        }
        tools.push(json!({"name":format!("{pack}/{name}"),"description":declared.description,"risk":declared.risk.as_str(),"input_schema":declared.input_schema,"command":command,"args":args,"sha256":hash,"files":pins,"protocol":"pack-v1","pack":pack,"tool":name,"timeout_seconds":parsed.timeout_seconds.unwrap_or(120),"lifecycle":lifecycle,"suggested_lifecycle":if stateful {"session"} else {"one-shot"},"manifest":manifest,"tool_protocol":parsed.tool_protocol,"ui_protocol":parsed.ui_protocol,"max_continuations":parsed.max_continuations,"cwd":root,"notes":notes}));
    }
    Ok(
        json!({"version":1,"models":[],"tools":tools,"warning":"Review commands, dependencies, risks and pack lifecycle before granting: set each chosen entry's lifecycle to its suggested_lifecycle (\"session\" keeps host-held pack state, promoted tools, continuations and UI requests; \"one-shot\" refuses them) and drop the rest. Candidate export enables nothing."}),
    )
}

/// The pinned MCP bundle, read only to describe what a binding will reach.
fn read_bundle(path: &Path) -> Result<Value> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 4 * 1024 * 1024 {
        return Err("MCP bundle must be a regular file below 4 MiB".into());
    }
    serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|_| "MCP bundle is not JSON".into())
}

/// What an MCP bridge tool reaches through its bundle, for the operator's review.
fn mcp_notes(tool: &str, bundle: &Value) -> Vec<String> {
    let servers = bundle["config"]["servers"].as_object();
    let allowlisted: Vec<String> = servers
        .into_iter()
        .flatten()
        .flat_map(|(server, config)| {
            config["tools"]
                .as_object()
                .into_iter()
                .flatten()
                .map(move |(name, risk)| {
                    format!("{server}/{name} ({})", risk.as_str().unwrap_or("?"))
                })
        })
        .collect();
    let persistent = servers
        .into_iter()
        .flatten()
        .any(|(_, config)| config["persistent"] == true);
    let mut notes = vec![];
    match tool {
        "mcp_search" | "mcp_describe" => notes.push(
            "offline catalogue reads; mcp_search with promote:true proposes direct tools, which need lifecycle \"session\" (tool_protocol 1)".to_string(),
        ),
        "mcp_call" | "mcp_script" => {
            let mutating = allowlisted.iter().any(|tool| !tool.ends_with("(read)"));
            notes.push(format!(
                "runs any allowlisted MCP tool ({}): {}",
                if mutating {
                    "mutations included"
                } else {
                    "all allowlisted as read; the bridge refuses others"
                },
                if allowlisted.is_empty() {
                    "none allowlisted".to_string()
                } else {
                    allowlisted.join(", ")
                }
            ));
            notes.push("a result the bridge reports as may_have_run is journaled as an unknown outcome and blocks later external calls in that script; nothing is retried".to_string());
        }
        _ if tool.starts_with("mcp_ui") => notes.push(
            "browser MCP Apps need the bridge's supervised service and a browser; Spindle does not host them".to_string(),
        ),
        _ => {}
    }
    if persistent {
        notes.push("persistent servers run through the explicitly started bridge service, under its own authority".to_string());
    }
    notes
}

fn execute(
    descriptor: &Value,
    input: &Value,
    cwd: &Path,
    interrupt: &AtomicBool,
    timeout: Duration,
) -> Result<Value> {
    let mut command = Command::new(descriptor["command"].as_str().ok_or("command required")?);
    for arg in descriptor["args"].as_array().ok_or("args required")? {
        command.arg(arg.as_str().ok_or("invalid arg")?);
    }
    command.current_dir(cwd).env_clear();
    for name in ["PATH", "HOME", "LANG", "SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    #[cfg(unix)]
    for (name, value) in session_bus() {
        command.env(name, value);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_std_process_group(&mut command);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let owner = unsafe { libc::getpid() };
        // The synchronous creating thread stays alive until this child is reaped.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != owner {
                    return Err(std::io::Error::from_raw_os_error(libc::ESTALE));
                }
                Ok(())
            });
        }
    }
    let mut child = command.spawn().map_err(|e| format!("{NOT_STARTED}{e}"))?;
    let tree = match ChildProcessTree::for_std(&child) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.to_string());
        }
    };
    let mut stdin = child.stdin.take().ok_or("missing tool stdin")?;
    let stdout = child.stdout.take().ok_or("missing tool stdout")?;
    let stderr = child.stderr.take().ok_or("missing tool stderr")?;
    let bytes = serde_json::to_vec(input).map_err(|e| e.to_string())?;
    let started = Instant::now();
    let flood = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let writer = scope.spawn(move || {
            stdin.write_all(&bytes)?;
            stdin.write_all(b"\n")
        });
        let flood_ref = &flood;
        let output = scope.spawn(move || {
            let mut bytes = vec![];
            stdout
                .take(crate::code_mode::MAX_REPLY as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > crate::code_mode::MAX_REPLY {
                flood_ref.store(true, Ordering::SeqCst);
            }
            Ok::<_, std::io::Error>(bytes)
        });
        let flood_ref = &flood;
        let errors = scope.spawn(move || {
            let mut bytes = vec![];
            stderr.take(16385).read_to_end(&mut bytes)?;
            if bytes.len() > 16384 {
                flood_ref.store(true, Ordering::SeqCst);
            }
            Ok::<_, std::io::Error>(bytes)
        });
        let status = loop {
            if interrupt.load(Ordering::SeqCst)
                || started.elapsed() >= timeout
                || flood.load(Ordering::SeqCst)
            {
                tree.terminate_std_child(&mut child);
                break Err("external tool stopped; outcome may be unknown".to_string());
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    tree.terminate_after_root_exit();
                    break Ok(status);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => {
                    tree.terminate_std_child(&mut child);
                    break Err(error.to_string());
                }
            }
        };
        let out = output
            .join()
            .map_err(|_| "tool reader panicked")?
            .map_err(|e| e.to_string())?;
        let err = errors
            .join()
            .map_err(|_| "tool reader panicked")?
            .map_err(|e| e.to_string())?;
        writer
            .join()
            .map_err(|_| "tool writer panicked")?
            .map_err(|e| e.to_string())?;
        if !status?.success() {
            return Err(format!(
                "external tool failed: {}",
                String::from_utf8_lossy(&err)
            ));
        }
        if out.len() > crate::code_mode::MAX_REPLY || err.len() > 16384 {
            return Err("external tool output limit".into());
        }
        serde_json::from_slice(&out).map_err(|_| "external tool must return one JSON value".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(tools: Value) -> CodeHost {
        CodeHost {
            policy: json!({"version":1,"models":[],"tools":tools}),
            provider: "fake".into(),
            model: "fake".into(),
            session: "fixture".into(),
            identity: "fixture-session".into(),
            cwd: std::env::temp_dir(),
            observer: None,
            credentials: Arc::new(agent::NativeCredentials),
            dynamic: BTreeMap::new(),
        }
    }

    #[test]
    fn grants_fail_closed_and_discovery_stays_compact() {
        let mut host = host(
            json!([{"name":"fixture/echo","description":"Echo an argument","risk":"read","input_schema":{"type":"object"}}]),
        );
        assert_eq!(
            host.discover("search", &json!({"query":"echo"}))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            host.prepare("tool", &json!({"name":"missing","arguments":{}}))
                .is_err()
        );
        assert!(host.prepare("model", &json!({"prompt":"test"})).is_err());
        assert!(
            host.prepare("tool", &json!({"name":"fixture/echo","arguments":{}}))
                .is_err(),
            "missing executable pin"
        );
    }

    #[test]
    fn nested_model_routes_use_grants_fall_back_and_list_them() {
        let mut host = host(json!([]));
        host.provider = "chatgpt".into();
        host.model = "gpt-6.1-sol".into();
        host.policy["models"] = json!([{"provider":"glm"}]);
        assert_eq!(
            host.model_route(&json!({"prompt":"p"})).unwrap(),
            ("glm".into(), None),
            "an ungranted thread provider falls back to the first grant"
        );
        let refused = host
            .model_route(&json!({"prompt":"p","provider":"chatgpt"}))
            .unwrap_err();
        assert!(refused.contains("granted: glm (any model)"), "{refused}");
        host.policy["models"] = json!([{"provider":"glm","model":"glm-5.3-flash"}]);
        assert_eq!(
            host.model_route(&json!({"prompt":"p"})).unwrap(),
            ("glm".into(), Some("glm-5.3-flash".into()))
        );
        host.provider = "glm".into();
        host.model = "glm-5.3".into();
        assert_eq!(
            host.model_route(&json!({"prompt":"p"})).unwrap(),
            ("glm".into(), Some("glm-5.3-flash".into())),
            "another granted model of the thread's own provider is reachable"
        );
        assert_eq!(
            host.model_route(&json!({"prompt":"p","provider":"glm"}))
                .unwrap(),
            ("glm".into(), Some("glm-5.3-flash".into())),
            "an explicit provider without a model falls through to its granted model"
        );
        assert!(
            host.model_route(&json!({"prompt":"p","provider":"glm","model":"glm-5.3"}))
                .is_err()
        );
        host.provider = "chatgpt".into();
        host.model = "gpt-6.1-sol".into();
        host.policy["models"] = json!([{"provider":"*"}]);
        assert_eq!(
            host.model_route(&json!({"prompt":"p"})).unwrap(),
            ("chatgpt".into(), Some("gpt-6.1-sol".into()))
        );
        assert_eq!(
            host.model_route(&json!({"prompt":"p","provider":"anthropic","model":"m"}))
                .unwrap(),
            ("anthropic".into(), Some("m".into()))
        );
        host.policy["models"] = json!([]);
        assert!(
            host.model_route(&json!({"prompt":"p"}))
                .unwrap_err()
                .contains("no nested models are granted")
        );
    }

    #[test]
    fn cancelled_external_call_is_not_dispatched() {
        let mut host = host(json!([]));
        for (cancelled, remaining) in [(true, Duration::from_secs(1)), (false, Duration::ZERO)] {
            let result = host.execute(
                &json!({"kind":"tool"}),
                &AtomicBool::new(cancelled),
                remaining,
            );
            assert_eq!(result["ok"], false);
            assert_eq!(result["may_have_run"], false);
            assert_eq!(result["error"], "external call stopped before dispatch");
        }
    }

    #[test]
    fn bridge_errors_before_dispatch_are_not_sent_and_tools_can_reach_the_keyring() {
        assert!(pack_runtime::not_sent(
            "not_sent: secure OAuth store or metadata unavailable"
        ));
        assert!(!pack_runtime::not_sent("may_have_run: connection lost"));
        assert!(!pack_runtime::not_sent("upstream refused: not_sent later"));
        // Wherever this host finds the session bus, bound tools are told of it.
        #[cfg(unix)]
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
            assert!(
                session_bus()
                    .iter()
                    .any(|(name, _)| *name == "DBUS_SESSION_BUS_ADDRESS")
            );
        }
    }

    #[test]
    fn pack_state_and_effects_cannot_be_silently_discarded() {
        for value in [Value::Null, json!({}), json!([])] {
            assert!(empty_pack_field(&value));
        }
        for value in [
            json!("bad"),
            json!(1),
            json!(false),
            json!([{}]),
            json!({"pending":true}),
        ] {
            assert!(!empty_pack_field(&value));
        }
    }

    #[test]
    fn schemas_reject_unknown_constraints_types_and_bounds() {
        let schema = json!({"type":"object","properties":{"n":{"type":"integer","minimum":1,"maximum":3},"s":{"type":"string","maxLength":2}},"required":["n"],"additionalProperties":false});
        assert!(validate_input(&schema, &json!({"n":2,"s":"ok"}), 0).is_ok());
        for input in [
            json!({}),
            json!({"n":4}),
            json!({"n":2,"x":1}),
            json!({"n":"2"}),
            json!({"n":2,"s":"long"}),
        ] {
            assert!(validate_input(&schema, &input, 0).is_err(), "{input}");
        }
        assert!(validate_input(&json!({"type":"string","pattern":"x"}), &json!("x"), 0).is_err());
        assert!(
            validate_input(
                &json!({"type":"object","properties":{"x":{"type":"string","pattern":"x"}}}),
                &json!({"x":"x"}),
                0
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn external_json_is_bounded_scrubbed_and_cancelled() {
        let cwd = std::env::temp_dir();
        let descriptor = json!({"command":"/usr/bin/python3","args":["-c","import sys,json,os; data=json.load(sys.stdin); print(json.dumps({'n':data['n'],'keys':[k for k in os.environ if k.endswith('API_KEY')]}))"]});
        let value = execute(
            &descriptor,
            &json!({"n":42}),
            &cwd,
            &AtomicBool::new(false),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(value["n"], 42);
        assert_eq!(value["keys"], json!([]));
        let descriptor =
            json!({"command":"/usr/bin/python3","args":["-c","import time; time.sleep(30)"]});
        let start = Instant::now();
        assert!(
            execute(
                &descriptor,
                &json!({}),
                &cwd,
                &AtomicBool::new(false),
                Duration::from_millis(100)
            )
            .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(3));
        let descriptor = json!({"command":"/usr/bin/python3","args":["-c","print('x'*300000)"]});
        assert!(
            execute(
                &descriptor,
                &json!({}),
                &cwd,
                &AtomicBool::new(false),
                Duration::from_secs(5)
            )
            .is_err()
        );
    }
}
