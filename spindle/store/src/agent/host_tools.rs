//! Tools the host runs in-process (`crate::turn::HOST_TOOLS`), under the same
//! reservation and receipts as the workspace tools (`Store::dispatch_host_workspace`):
//! `show` puts workspace files in the chat as cards, `settings` reads the thread's
//! settings or queues a change for the end of the turn, and `spindle_source` /
//! `spindle_install` (self-edit) belong to the host serving the session
//! (`SessionHost`); without one they say they are unavailable.
use super::{Compaction, Effort, Result, Settings, catalog, provider};
use crate::broker::{HostResult, HostWorkspace, MAX_SHOWN};
use serde_json::{Map, Value, json};
use std::path::Path;

/// Settings the `settings` tool queued; applied when the turn ends (`apply_pending`).
const PENDING: &str = "pending-settings.json";
/// The line of a `show` result that carries its cards, as JSON.
use crate::reads::DISPLAY;
pub use crate::reads::{annotate_display, display_of};
/// Largest file `show` classifies as text; larger ones are shown as files.
const MAX_TEXT: u64 = 1024 * 1024;
/// Longest thread name (as the web host's rename).
const MAX_NAME: usize = 80;
const NO_SOURCE: &str = "Spindle's own source is not available here: only `spindle serve` \
     with --dev-source offers it. Nothing was checked out or requested.";

/// What the host serving a session lets its model do beyond the workspace: the web
/// host's Spindle source for self-edit, and thread names. Experimental (not API v1).
pub trait SessionHost: Send + Sync {
    /// Whether `spindle_source` and `spindle_install` work here.
    fn offers_source(&self) -> bool {
        false
    }
    /// `spindle_source {action}` ("checkout" or "update") over the call's workspace.
    fn spindle_source(&self, _action: &str, _workspace: &mut HostWorkspace<'_>) -> HostResult {
        HostResult::text(NO_SOURCE, false)
    }
    /// `spindle_install {summary}`: records the request the user confirms with Install.
    fn spindle_install(&self, _summary: &str, _workspace: &mut HostWorkspace<'_>) -> HostResult {
        HostResult::text(NO_SOURCE, false)
    }
    /// Renames the thread (a queued `settings` name, applied when the turn ends).
    fn rename(&self, _name: &str) -> Result<()> {
        Err("this host keeps no thread names".into())
    }
    /// The operator's shelf of cross-thread notes, mounted read-only at `/shelf` in
    /// commands of threads with the toolchain (its sources). None: no shelf.
    fn shelf(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// Runs one host tool call. `dir` is the session directory.
pub(super) fn run(
    dir: &Path,
    host: Option<&dyn SessionHost>,
    name: &str,
    input: &Value,
    workspace: &mut HostWorkspace<'_>,
) -> HostResult {
    match name {
        "show" => show(input, workspace),
        "settings" => settings(dir, host, input, true),
        "spindle_source" => match host.filter(|host| host.offers_source()) {
            Some(host) => host.spindle_source(input["action"].as_str().unwrap_or(""), workspace),
            None => HostResult::text(NO_SOURCE, false),
        },
        "spindle_install" => match host.filter(|host| host.offers_source()) {
            Some(host) => host.spindle_install(input["summary"].as_str().unwrap_or(""), workspace),
            None => HostResult::text(NO_SOURCE, false),
        },
        _ => HostResult::text(format!("unknown host tool `{name}`"), false),
    }
}

/// The card kind of a path by its extension; None: text or a plain file, by content.
pub fn kind_of(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    Some(match extension.as_str() {
        "html" | "htm" => "html",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "bmp" | "ico" => "image",
        "svg" => "svg",
        "md" | "markdown" => "markdown",
        "pdf" => "pdf",
        _ => return None,
    })
}

/// `show`: every path must name a workspace file (a directory stands for its
/// index.html). The result names them for the model and ends with a `display:` line,
/// the cards' JSON (`display_of`).
fn show(input: &Value, workspace: &mut HostWorkspace<'_>) -> HostResult {
    let title = input["title"].as_str().map(str::trim);
    let paths = input["paths"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if paths.is_empty() || paths.len() > MAX_SHOWN {
        return HostResult::text(
            format!("Nothing shown: paths must name 1-{MAX_SHOWN} workspace files."),
            false,
        );
    }
    let mut problems = vec![];
    let mut chosen: Vec<(String, String)> = vec![];
    for raw in paths {
        let raw = raw.as_str().unwrap_or_default();
        let path = raw
            .strip_prefix("/workspace/")
            .or_else(|| raw.strip_prefix("./"))
            .unwrap_or(raw)
            .trim_end_matches('/');
        let file = |path: &str| {
            workspace
                .files()
                .iter()
                .find(|entry| entry.path == path)
                .map(|entry| (entry.path.clone(), entry.object.clone()))
        };
        let found = match file(path) {
            Some(found) => Ok(found),
            None if workspace.is_directory(path) => file(&format!("{path}/index.html"))
                .ok_or(format!("`{path}` is a directory without an index.html")),
            None => {
                let lower = path.to_lowercase();
                Err(
                    match workspace
                        .files()
                        .iter()
                        .find(|entry| entry.path.to_lowercase() == lower)
                    {
                        Some(entry) => format!("no file `{path}` (did you mean `{}`?)", entry.path),
                        None => format!("no file `{path}` in the workspace"),
                    },
                )
            }
        };
        match found {
            Ok(found) if !chosen.iter().any(|(path, _)| *path == found.0) => chosen.push(found),
            Ok(_) => {}
            Err(problem) => problems.push(problem),
        }
    }
    if !problems.is_empty() {
        return HostResult::text(format!("Nothing shown: {}.", problems.join("; ")), false);
    }
    let revision = workspace.revision();
    let several = chosen.len() > 1;
    let mut items = vec![];
    for (path, object) in chosen {
        let size = match workspace.size(&object) {
            Ok(size) => size,
            Err(error) => return HostResult::text(format!("Nothing shown: {error}"), false),
        };
        let kind = match kind_of(&path) {
            Some(kind) => kind,
            None if size <= MAX_TEXT => match workspace.read(&object) {
                Ok(bytes) if std::str::from_utf8(&bytes).is_ok() && !bytes.contains(&0) => "text",
                Ok(_) => "file",
                Err(error) => return HostResult::text(format!("Nothing shown: {error}"), false),
            },
            None => "file",
        };
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        // One caption for several cards names each one too.
        let title = match title.filter(|t| !t.is_empty()) {
            Some(title) if several => format!("{title}: {name}"),
            Some(title) => title.to_string(),
            None => name,
        };
        items.push(json!({
            "path": path,
            "kind": kind,
            "title": title,
            "size": size,
            "revision": revision,
        }));
    }
    let listed = items
        .iter()
        .map(|item| {
            format!(
                "{} ({})",
                item["path"].as_str().unwrap_or_default(),
                item["kind"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    HostResult::text(
        format!(
            "Shown in the chat (r{revision}): {listed}. The user sees each as a card they can \
             open; don't repeat its contents.\n{DISPLAY}{}",
            Value::Array(items)
        ),
        true,
    )
}

/// The settings change the `settings` tool queued and the turn has not applied yet.
pub fn pending_settings(dir: &Path) -> Option<Value> {
    let bytes = match crate::meta::read(dir, "pending_settings").ok()? {
        Some(bytes) => bytes,
        None => std::fs::read(dir.join(PENDING)).ok()?,
    };
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .filter(|value| value.as_object().is_some_and(|fields| !fields.is_empty()))
}

fn pending_update(dir: &Path, change: impl FnOnce(&mut Map<String, Value>)) -> Result<()> {
    let mut change = Some(change);
    let mut saved = None;
    let updated = crate::meta::update(dir, "pending_settings", |bytes| {
        let mut value = match bytes {
            Some(bytes) => serde_json::from_slice::<Value>(bytes)
                .ok()
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default(),
            None => pending_file(dir).unwrap_or_default(),
        };
        change.take().expect("called once")(&mut value);
        let value = if value.is_empty() {
            Value::Null
        } else {
            Value::Object(value)
        };
        let bytes = serde_json::to_vec(&value).map_err(|_| crate::Error::Invalid)?;
        saved = Some(bytes.clone());
        Ok(bytes)
    })
    .map_err(super::store_error)?;
    let bytes = if updated {
        saved.expect("updated")
    } else {
        let mut value = pending_file(dir).unwrap_or_default();
        change.take().expect("not called")(&mut value);
        serde_json::to_vec(&if value.is_empty() {
            Value::Null
        } else {
            Value::Object(value)
        })
        .map_err(|e| e.to_string())?
    };
    if bytes == b"null" {
        match std::fs::remove_file(dir.join(PENDING)) {
            Ok(()) => crate::durable::sync_parent(&dir.join(PENDING)).map_err(|e| e.to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    } else {
        super::save_private(&dir.join(PENDING), &bytes)
    }
}

fn pending_file(dir: &Path) -> Option<Map<String, Value>> {
    serde_json::from_slice::<Value>(&std::fs::read(dir.join(PENDING)).ok()?)
        .ok()?
        .as_object()
        .cloned()
}

/// Forgets queued settings: a user's explicit choice wins over queued model fields.
pub fn drop_pending_settings(dir: &Path, fields: Option<&[&str]>) -> Result<()> {
    pending_update(dir, |pending| match fields {
        Some(fields) => pending.retain(|key, _| !fields.contains(&key.as_str())),
        None => pending.clear(),
    })
}

/// Checks a change (the `settings` tool's fields) against `settings`: unknown values
/// are refused with the valid choices. Returns the settings it makes and the new name.
fn checked(
    settings: &Settings,
    change: &Map<String, Value>,
    host: Option<&dyn SessionHost>,
    credentials: bool,
) -> Result<(Settings, Option<String>)> {
    let mut next = settings.clone();
    let text = |key: &str| change.get(key).and_then(Value::as_str).map(str::trim);
    let provider_id = text("provider");
    let model = text("model");
    if provider_id.is_some() || model.is_some() {
        super::install_host();
        let catalog = provider::load_provider_catalog().map_err(|e| e.to_string())?;
        let id = provider_id.unwrap_or(&settings.provider);
        let profile = provider::find_provider_profile(&catalog, id).ok_or_else(|| {
            let known: Vec<&str> = catalog.providers.iter().map(|p| p.id.as_str()).collect();
            format!("unknown provider `{id}`; providers: {}", known.join(", "))
        })?;
        // A credential check may run a `!command` secret: `credentials` false when the
        // caller already checked them (the web host, outside its locks).
        if provider_id.is_some() && credentials {
            let store = provider::load_auth_store().map_err(|e| e.to_string())?;
            if !provider::provider_has_available_credentials(&profile, &store) {
                let ready: Vec<&str> = catalog
                    .providers
                    .iter()
                    .filter(|p| provider::provider_has_available_credentials(p, &store))
                    .map(|p| p.id.as_str())
                    .collect();
                return Err(format!(
                    "provider `{id}` is not signed in (the user can sign in under Sign-ins in the host sheet); signed in: {}",
                    ready.join(", ")
                ));
            }
        }
        let models = catalog::models_of(&profile);
        let model = match model {
            Some(model) => {
                let wanted = provider::normalize_provider_model_value(&profile, model);
                models
                    .iter()
                    .find(|known| known.eq_ignore_ascii_case(&wanted))
                    .cloned()
                    .ok_or_else(|| {
                        format!(
                            "unknown model `{model}` for {}; its models: {}",
                            profile.id,
                            models.join(", ")
                        )
                    })?
            }
            None if provider_id.is_some() => provider::resolve_provider_model(&profile),
            None => settings.model.clone(),
        };
        let (provider, model) = Settings::route_of(&profile.id, Some(&model))?;
        next.provider = provider;
        next.model = model;
    }
    if let Some(effort) = text("effort") {
        let valid = catalog::valid_efforts(&next.provider, &next.model);
        next.effort = Effort::parse(effort)
            .filter(|effort| valid.contains(&effort.as_str()))
            .ok_or_else(|| {
                format!(
                    "effort `{effort}` is not available for {}/{}; choose one of: {}",
                    next.provider,
                    next.model,
                    valid.join(", ")
                )
            })?;
    } else if (provider_id.is_some() || model.is_some())
        && super::describe_effort(&next.provider, &next.model, next.effort).is_err()
    {
        return Err(format!(
            "the current effort `{}` is not available for {}/{}; also set effort to one of: {}",
            next.effort.as_str(),
            next.provider,
            next.model,
            catalog::valid_efforts(&next.provider, &next.model).join(", ")
        ));
    }
    if let Some(network) = change.get("network").and_then(Value::as_bool) {
        next.network = network;
    }
    if let Some(toolchain) = change.get("toolchain").and_then(Value::as_bool) {
        if toolchain && super::host_toolchain().is_none() {
            return Err("this host has no Rust toolchain to offer".into());
        }
        next.toolchain = toolchain;
    }
    if let Some(compaction) = text("compaction") {
        next.compaction = Compaction::parse(compaction).ok_or_else(|| {
            format!("compaction `{compaction}` is unknown; choose model or digest")
        })?;
    }
    match change.get("max_usd") {
        None => {}
        Some(Value::Null) => next.max_usd = None,
        Some(max) => {
            next.max_usd = Some(
                max.as_f64()
                    .filter(|max| max.is_finite() && *max >= 0.0)
                    .ok_or("max_usd must be a non-negative number of dollars, or null for none")?,
            );
        }
    }
    let name = match text("name") {
        None => None,
        Some(name) => {
            let name: String = name.chars().filter(|c| !c.is_control()).collect();
            if name.trim().is_empty() || name.chars().count() > MAX_NAME {
                return Err(format!("name must be 1-{MAX_NAME} characters"));
            }
            if host.is_none() {
                return Err("thread names are kept by the web host; this session has none".into());
            }
            Some(name.trim().to_string())
        }
    };
    Ok((next, name))
}

/// Each field of a change is a known setting of the right type: a value `checked`
/// would skip (a string `network`) must not be reported as queued.
fn typed(change: &Map<String, Value>) -> Result<()> {
    for (key, value) in change {
        let fits = match key.as_str() {
            "provider" | "model" | "effort" | "compaction" | "name" => value.is_string(),
            "network" | "toolchain" => value.is_boolean(),
            "max_usd" => value.is_number() || value.is_null(),
            _ => {
                return Err(format!(
                    "unknown setting `{key}`; settings are provider, model, effort, network, \
                     toolchain, compaction, max_usd and name"
                ));
            }
        };
        if !fits {
            return Err(format!("`{key}` has the wrong type ({value})"));
        }
    }
    Ok(())
}

fn describe(settings: &Settings) -> String {
    format!(
        "provider {}, model {}, effort {}, network {}, toolchain {}, compaction {}, max_usd {}",
        settings.provider,
        settings.model,
        settings.effort.as_str(),
        if settings.network { "on" } else { "off" },
        if settings.toolchain { "on" } else { "off" },
        settings.compaction.as_str(),
        settings
            .max_usd
            .map_or("none".to_string(), |max| format!("${max}")),
    )
}

/// `settings`: without fields, the current settings and what is available; with
/// fields, the change checked and queued for the end of this turn.
fn settings(
    dir: &Path,
    host: Option<&dyn SessionHost>,
    input: &Value,
    credentials: bool,
) -> HostResult {
    let current = match Settings::load(dir) {
        Ok(current) => current,
        Err(error) => return HostResult::text(error, false),
    };
    let change = input.as_object().cloned().unwrap_or_default();
    if change.is_empty() {
        let mut text = format!("Current settings: {}.", describe(&current));
        if let Some(pending) = pending_settings(dir) {
            text.push_str(&format!(" Queued for the end of this turn: {pending}."));
        }
        match catalog::compact((&current.provider, &current.model)) {
            Ok(available) => text.push_str(&format!(
                "\nProviders and models (* default):{available}\nChange any of provider, model, \
                 effort, network, toolchain, compaction, max_usd and name with settings; a \
                 change applies when this turn ends."
            )),
            Err(error) => text.push_str(&format!("\nThe model catalog is unreadable: {error}")),
        }
        return HostResult::text(text, true);
    }
    if let Err(error) = typed(&change) {
        return HostResult::text(format!("Not changed: {error}."), false);
    }
    // Checked against what the turn will end with: the settings plus anything queued.
    let queued = match pending_settings(dir) {
        Some(Value::Object(pending)) => pending,
        _ => Map::new(),
    };
    let mut merged = queued.clone();
    merged.extend(change.clone());
    if let Err(error) = checked(&current, &merged, host, credentials) {
        return HostResult::text(format!("Not changed: {error}."), false);
    }
    let saved = pending_update(dir, |pending| pending.extend(change.clone()));
    if let Err(error) = saved {
        return HostResult::text(format!("Not changed: {error}"), false);
    }
    let listed = change
        .iter()
        .map(|(key, value)| format!("{key} → {value}"))
        .collect::<Vec<_>>()
        .join(", ");
    HostResult::text(
        format!(
            "Queued: {listed}. It applies when this turn ends (this turn keeps {}/{}); the \
             user sees it as pending until then.",
            current.provider, current.model
        ),
        true,
    )
}

/// The web host while a queued change is only checked: it keeps thread names (the
/// rename itself happens with the real host when the turn ends).
struct KeepsNames;

impl SessionHost for KeepsNames {
    fn rename(&self, _name: &str) -> Result<()> {
        Ok(())
    }
}

/// Queues a settings change the web host received while a turn runs (the phone's model
/// picker), exactly as the `settings` tool does: checked now, applied when the turn ends.
pub fn queue_settings(dir: &Path, data: &Value) -> Result<()> {
    const FIELDS: [&str; 8] = [
        "provider",
        "model",
        "effort",
        "network",
        "toolchain",
        "compaction",
        "max_usd",
        "name",
    ];
    let change: Map<String, Value> = data
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| FIELDS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if change.is_empty() {
        return Err("no setting to change".into());
    }
    // Its provider's credentials were checked by the web host before it took its locks.
    let result = settings(dir, Some(&KeepsNames), &Value::Object(change), false);
    if result.success {
        Ok(())
    } else {
        Err(result.content)
    }
}

/// Applies queued settings when a turn ends: checked again against the current
/// settings, saved, and the queue cleared either way. Returns what to report.
pub(super) fn apply_pending(dir: &Path, host: Option<&dyn SessionHost>) -> Option<String> {
    let Some(Value::Object(change)) = pending_settings(dir) else {
        return None;
    };
    let outcome = Settings::load(dir).and_then(|current| {
        let (next, name) = checked(&current, &change, host, true)?;
        next.save(dir)?;
        if let (Some(name), Some(host)) = (&name, host) {
            host.rename(name)?;
        }
        Ok(describe(&next))
    });
    // Only what was read is cleared: a change queued meanwhile (the phone's picker as the
    // turn ends) waits for the next turn instead of being lost.
    let cleared = pending_update(dir, |pending| {
        pending.retain(|key, value| change.get(key) != Some(value))
    });
    Some(match (outcome, cleared) {
        (Ok(now), Ok(())) => format!("Settings changed at the end of the turn: {now}."),
        (Ok(now), Err(error)) => {
            format!("Settings changed: {now}; the queue was not cleared ({error}).")
        }
        (Err(error), _) => format!("The queued settings were not applied: {error}."),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_changes_must_be_known_fields_of_the_right_type() {
        let change = |value: Value| value.as_object().cloned().unwrap();
        assert!(
            typed(&change(
                json!({"provider": "glm", "network": true, "max_usd": null})
            ))
            .is_ok()
        );
        assert!(typed(&change(json!({"max_usd": 2.5, "toolchain": false}))).is_ok());
        for (bad, why) in [
            (json!({"network": "yes"}), "wrong type"),
            (json!({"max_usd": "5"}), "wrong type"),
            (json!({"effort": 3}), "wrong type"),
            (json!({"temperature": 0.2}), "unknown setting `temperature`"),
        ] {
            let error = typed(&change(bad.clone())).unwrap_err();
            assert!(error.contains(why), "{bad}: {error}");
        }
    }

    #[test]
    fn kinds_follow_extensions_and_display_lines_round_trip() {
        for (path, kind) in [
            ("site/index.html", Some("html")),
            ("a.HTM", Some("html")),
            ("logo.PNG", Some("image")),
            ("p.jpeg", Some("image")),
            ("i.avif", Some("image")),
            ("f.ico", Some("image")),
            ("d.svg", Some("svg")),
            ("README.md", Some("markdown")),
            ("x.markdown", Some("markdown")),
            ("doc.pdf", Some("pdf")),
            ("main.rs", None),
            ("Makefile", None),
        ] {
            assert_eq!(kind_of(path), kind, "{path}");
        }
        let items = json!([{"path": "a b\nc.html", "kind": "html", "title": "t", "size": 3, "revision": 4}]);
        let content = format!("Shown in the chat (r4): x.\n{DISPLAY}{items}");
        assert_eq!(display_of(&content), Some(items.clone()));
        assert_eq!(display_of("display: [1]"), None);
        assert_eq!(display_of("no cards"), None);
        let mut messages = json!([
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "s1", "name": "show", "input": {}},
                {"type": "tool_use", "id": "b1", "name": "bash", "input": {}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "s1", "content": content, "is_error": false},
                {"type": "tool_result", "tool_use_id": "b1", "content": content, "is_error": false}]}
        ]);
        annotate_display(&mut messages);
        assert_eq!(messages[1]["content"][0]["display"], items);
        assert!(
            messages[1]["content"][1].get("display").is_none(),
            "only show results carry cards"
        );
    }
}
