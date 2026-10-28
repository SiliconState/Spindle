use anyhow::{Context, Result};
use base64::Engine;
use reqwest::RequestBuilder;
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub use crate::catalog::*;

impl ApiProvider {
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_env() -> Self {
        match std::env::var("DEXT_API_PROVIDER")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "openai" | "openai-compatible" | "ollama" | "llama" | "llama.cpp" | "local"
            | "deepseek" => Self::OpenAi,
            "chatgpt" | "openai-codex" | "codex" | "codex-openai" => Self::ChatGpt,
            "anthropic" | "claude" | "glm" | "zai" => Self::Anthropic,
            _ => {
                let base = std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if base.contains("openai")
                    || base.contains("ollama")
                    || base.contains("llama")
                    || base.contains("127.0.0.1")
                    || base.contains("localhost:11434")
                {
                    Self::OpenAi
                } else {
                    Self::Anthropic
                }
            }
        }
    }
}

impl ProviderRequestBody {
    pub fn apply(&self, request: RequestBuilder) -> RequestBuilder {
        let request = if self.gzip {
            request.header(reqwest::header::CONTENT_ENCODING, "gzip")
        } else {
            request
        };
        request.body(self.bytes.clone())
    }
}

use crate::host::{atomic_write_bytes, atomic_write_secret, dext_state_dir, unix_timestamp_secs};

const AUTH_STORE_VERSION: u32 = 1;
const STATE_INSPECTION_MAX_BYTES: u64 = 1024 * 1024;
const LLAMA_CONTEXT_DISCOVERY_TIMEOUT: Duration = Duration::from_millis(700);
const LLAMA_CONTEXT_DISCOVERY_PATHS: &[&str] = &["/props", "/slots", "/v1/models", "/models"];

const OAUTH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const OAUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const OAUTH_CALLBACK_IO_TIMEOUT: Duration = Duration::from_secs(2);
const OAUTH_RESPONSE_MAX_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderCatalogIntegrity {
    Missing,
    Valid {
        version: u32,
        legacy: bool,
    },
    InvalidSchema,
    UnsupportedVersion {
        version: u32,
    },
    Symlink,
    NonRegular,
    #[cfg(unix)]
    UnsafeOwner,
    #[cfg(unix)]
    UnsafeMode {
        mode: u32,
    },
    Unreadable,
    TooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCatalogInspection {
    pub path: PathBuf,
    pub integrity: ProviderCatalogIntegrity,
    pub active_provider: Option<String>,
    pub provider_count: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StoredCredential {
    ApiKey {
        key: String,
    },
    OAuth {
        access_token: String,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        expires_at: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthStore {
    #[serde(default = "default_auth_store_version")]
    pub version: u32,
    #[serde(default)]
    pub providers: HashMap<String, StoredCredential>,
}

impl Default for AuthStore {
    fn default() -> Self {
        Self {
            version: AUTH_STORE_VERSION,
            providers: HashMap::new(),
        }
    }
}

pub fn default_auth_store_version() -> u32 {
    AUTH_STORE_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStoreFileSecurity {
    Missing,
    #[cfg(unix)]
    Secure {
        mode: u32,
    },
    #[cfg(unix)]
    UnsafeMode {
        mode: u32,
    },
    Symlink,
    NonRegular,
    #[cfg(unix)]
    UnsafeOwner,
    Unreadable,
    #[cfg(windows)]
    WindowsAclNotEvaluated,
    #[cfg(not(any(unix, windows)))]
    PermissionsNotEvaluated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStoreIntegrity {
    NotChecked,
    Valid { version: u32, legacy: bool },
    InvalidSchema,
    UnsupportedVersion { version: u32 },
    Unreadable,
    TooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthStoreInspection {
    pub path: PathBuf,
    pub security: AuthStoreFileSecurity,
    pub integrity: AuthStoreIntegrity,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RuntimeAuthKind {
    #[default]
    None,
    ApiKey,
    OAuth,
}

#[derive(Debug, Clone)]
pub struct ResolvedProviderConfig {
    pub profile: ProviderProfile,
    pub api_key: String,
    pub auth_kind: RuntimeAuthKind,
    pub key_source: String,
    pub model: String,
    pub base_url: String,
    pub requires_api_key: bool,
}

pub fn provider_catalog_path() -> PathBuf {
    dext_state_dir().join("providers.json")
}

pub fn auth_store_path() -> PathBuf {
    dext_state_dir().join("auth.json")
}

fn local_llama_cache() -> &'static Mutex<HashMap<String, u64>> {
    static CACHE: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn local_llama_cache_key(base_url: &str, model: &str) -> String {
    format!(
        "{}|{}",
        base_url.trim().trim_end_matches('/').to_ascii_lowercase(),
        model.trim().to_ascii_lowercase()
    )
}

#[cfg(any(test, feature = "test-support"))]
pub fn clear_cached_local_llama_context_windows() {
    if let Ok(mut cache) = local_llama_cache().lock() {
        cache.clear();
    }
}

fn llama_endpoint_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.ends_with("/v1") && !path.starts_with("/v1/") {
        format!("{}{}", base.trim_end_matches("/v1"), path)
    } else {
        format!("{base}{path}")
    }
}

fn provider_blocking_http_client_builder() -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder()
        .no_gzip()
        .no_brotli()
        .http1_only()
}

fn fetch_llama_context_window(client: &reqwest::blocking::Client, base_url: &str) -> Option<u64> {
    for path in LLAMA_CONTEXT_DISCOVERY_PATHS {
        let Ok(resp) = client.get(llama_endpoint_url(base_url, path)).send() else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(json) = resp.json::<Value>() else {
            continue;
        };
        if let Some(tokens) = parse_llama_context_window(&json) {
            return Some(tokens);
        }
    }
    None
}

pub fn refresh_local_llama_context_window(
    provider_id: &str,
    api_provider: ApiProvider,
    base_url: &str,
    model: &str,
) -> Option<u64> {
    if !is_local_llama_provider(provider_id, api_provider, base_url) {
        return None;
    }
    let base_url = base_url.trim().trim_end_matches('/');
    if base_url.is_empty() {
        return None;
    }
    let endpoint_key = local_llama_cache_key(base_url, model);
    if let Some(tokens) = local_llama_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(&endpoint_key).copied())
        .filter(|tokens| *tokens > 0)
    {
        return Some(tokens);
    }

    let base_url = base_url.to_string();
    let tokens = std::thread::Builder::new()
        .name("dext-local-context-probe".to_string())
        .spawn(move || {
            let client = provider_blocking_http_client_builder()
                .timeout(LLAMA_CONTEXT_DISCOVERY_TIMEOUT)
                .build()
                .ok()?;
            fetch_llama_context_window(&client, &base_url)
        })
        .ok()?
        .join()
        .ok()??;
    if let Ok(mut cache) = local_llama_cache().lock() {
        cache.insert(endpoint_key, tokens);
    }
    Some(tokens)
}

fn read_state_inspection_bytes(
    path: &Path,
    expected: &std::fs::Metadata,
) -> std::result::Result<(Vec<u8>, std::fs::Metadata), bool> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(path).map_err(|_| false)?;
    let opened = file.metadata().map_err(|_| false)?;
    if !opened.is_file() {
        return Err(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if opened.dev() != expected.dev() || opened.ino() != expected.ino() {
            return Err(false);
        }
    }
    #[cfg(not(unix))]
    let _ = expected;

    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(STATE_INSPECTION_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| false)?;
    if bytes.len() as u64 > STATE_INSPECTION_MAX_BYTES {
        Err(true)
    } else {
        Ok((bytes, opened))
    }
}

pub fn inspect_provider_catalog() -> ProviderCatalogInspection {
    let path = provider_catalog_path();
    let empty = |integrity| ProviderCatalogInspection {
        path: path.clone(),
        integrity,
        active_provider: None,
        provider_count: None,
    };
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return ProviderCatalogInspection {
                path,
                integrity: ProviderCatalogIntegrity::Missing,
                active_provider: Some(default_active_provider()),
                provider_count: Some(built_in_provider_profiles().len()),
            };
        }
        Err(_) => return empty(ProviderCatalogIntegrity::Unreadable),
    };
    if metadata.file_type().is_symlink() {
        return empty(ProviderCatalogIntegrity::Symlink);
    }
    if !metadata.is_file() {
        return empty(ProviderCatalogIntegrity::NonRegular);
    }
    if metadata.len() > STATE_INSPECTION_MAX_BYTES {
        return empty(ProviderCatalogIntegrity::TooLarge);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if !runtime_state_owned_by_current_user(&metadata) {
            return empty(ProviderCatalogIntegrity::UnsafeOwner);
        }
        let mode = metadata.mode() & 0o777;
        if mode & 0o022 != 0 {
            return empty(ProviderCatalogIntegrity::UnsafeMode { mode });
        }
    }
    let (bytes, opened) = match read_state_inspection_bytes(&path, &metadata) {
        Ok(result) => result,
        Err(true) => return empty(ProviderCatalogIntegrity::TooLarge),
        Err(false) => return empty(ProviderCatalogIntegrity::Unreadable),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if !runtime_state_owned_by_current_user(&opened) {
            return empty(ProviderCatalogIntegrity::UnsafeOwner);
        }
        let mode = opened.mode() & 0o777;
        if mode & 0o022 != 0 {
            return empty(ProviderCatalogIntegrity::UnsafeMode { mode });
        }
    }
    #[cfg(not(unix))]
    let _ = &opened;
    let raw = match serde_json::from_slice::<Value>(&bytes) {
        Ok(raw) => raw,
        Err(_) => return empty(ProviderCatalogIntegrity::InvalidSchema),
    };
    if raw.is_array() {
        return match serde_json::from_value::<Vec<ProviderProfile>>(raw) {
            Ok(providers) => {
                let normalized = match normalize_provider_catalog(ProviderCatalog {
                    version: 1,
                    active_provider: default_active_provider(),
                    providers,
                }) {
                    Ok(catalog) => catalog,
                    Err(_) => return empty(ProviderCatalogIntegrity::InvalidSchema),
                };
                ProviderCatalogInspection {
                    path,
                    integrity: ProviderCatalogIntegrity::Valid {
                        version: 1,
                        legacy: true,
                    },
                    active_provider: Some(normalized.active_provider),
                    provider_count: Some(normalized.providers.len()),
                }
            }
            Err(_) => empty(ProviderCatalogIntegrity::InvalidSchema),
        };
    }
    let version = match raw.get("version") {
        None => PROVIDER_CATALOG_VERSION,
        Some(value) => match value.as_u64().and_then(|v| u32::try_from(v).ok()) {
            Some(version) => version,
            None => return empty(ProviderCatalogIntegrity::InvalidSchema),
        },
    };
    if version == 0 || version > PROVIDER_CATALOG_VERSION {
        return empty(ProviderCatalogIntegrity::UnsupportedVersion { version });
    }
    match serde_json::from_value::<ProviderCatalog>(raw) {
        Ok(catalog) => {
            let normalized = match normalize_provider_catalog(catalog) {
                Ok(catalog) => catalog,
                Err(_) => return empty(ProviderCatalogIntegrity::InvalidSchema),
            };
            ProviderCatalogInspection {
                path,
                integrity: ProviderCatalogIntegrity::Valid {
                    version,
                    legacy: version < PROVIDER_CATALOG_VERSION,
                },
                active_provider: Some(normalized.active_provider),
                provider_count: Some(normalized.providers.len()),
            }
        }
        Err(_) => empty(ProviderCatalogIntegrity::InvalidSchema),
    }
}

#[cfg(unix)]
fn runtime_state_owned_by_current_user(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    metadata.uid() == unsafe { libc::geteuid() }
}

fn read_runtime_state_file(path: &Path, secret: bool) -> Result<Option<String>> {
    #[cfg(not(unix))]
    let _ = secret;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting state file {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "state file must be a regular non-symlink file: {}",
            path.display()
        );
    }
    if metadata.len() > STATE_INSPECTION_MAX_BYTES {
        anyhow::bail!(
            "state file exceeds the {} byte limit: {}",
            STATE_INSPECTION_MAX_BYTES,
            path.display()
        );
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("opening state file {}", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("validating open state file {}", path.display()))?;
    if !opened.is_file() {
        anyhow::bail!(
            "opened state path is not a regular file: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            anyhow::bail!("state file changed while opening: {}", path.display());
        }
        if !runtime_state_owned_by_current_user(&opened) {
            anyhow::bail!(
                "state file is not owned by the current user: {}",
                path.display()
            );
        }
        let mode = opened.mode() & 0o777;
        if secret && mode & 0o077 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("repairing owner-only mode on {}", path.display()))?;
        } else if !secret && mode & 0o022 != 0 {
            anyhow::bail!(
                "provider state has unsafe writable mode {mode:04o}; remove group/world write bits: {}",
                path.display()
            );
        }
    }

    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(STATE_INSPECTION_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading state file {}", path.display()))?;
    if bytes.len() as u64 > STATE_INSPECTION_MAX_BYTES {
        anyhow::bail!(
            "state file exceeds the {} byte limit: {}",
            STATE_INSPECTION_MAX_BYTES,
            path.display()
        );
    }
    String::from_utf8(bytes)
        .with_context(|| format!("state file is not valid UTF-8: {}", path.display()))
        .map(Some)
}

pub fn load_provider_catalog() -> Result<ProviderCatalog> {
    let path = provider_catalog_path();
    let Some(text) = read_runtime_state_file(&path, false)? else {
        return Ok(default_provider_catalog());
    };
    let raw: Value = serde_json::from_str(&text).context("invalid provider catalog JSON")?;

    let catalog = if raw.is_array() {
        ProviderCatalog {
            version: 1,
            active_provider: default_active_provider(),
            providers: serde_json::from_value(raw).context("invalid provider catalog JSON")?,
        }
    } else {
        let version = raw
            .get("version")
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|version| u32::try_from(version).ok())
                    .context("provider catalog version must be a positive integer")
            })
            .transpose()?
            .unwrap_or(PROVIDER_CATALOG_VERSION);
        if version == 0 || version > PROVIDER_CATALOG_VERSION {
            anyhow::bail!(
                "unsupported provider catalog version {version} (supported: 1-{PROVIDER_CATALOG_VERSION})"
            );
        }
        serde_json::from_value::<ProviderCatalog>(raw).context("invalid provider catalog JSON")?
    };
    normalize_provider_catalog(catalog)
}

pub fn save_provider_catalog(catalog: &ProviderCatalog) -> Result<()> {
    let path = provider_catalog_path();
    let normalized = normalize_provider_catalog(catalog.clone())?;
    let bytes = serde_json::to_vec_pretty(&normalized)?;
    atomic_write_bytes(&path, &bytes)?;
    Ok(())
}

fn normalize_auth_store(mut store: AuthStore) -> AuthStore {
    store.version = AUTH_STORE_VERSION;
    let mut entries = std::mem::take(&mut store.providers)
        .into_iter()
        .collect::<Vec<_>>();
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));
    let mut providers = HashMap::new();
    for (provider, credential) in entries {
        let canonical = canonical_provider_id(&provider);
        if canonical.is_empty() {
            continue;
        }
        if provider.trim().eq_ignore_ascii_case(&canonical) || !providers.contains_key(&canonical) {
            providers.insert(canonical, credential);
        }
    }
    store.providers = providers;
    store
}

#[cfg(test)]
#[test]
fn auth_store_normalization_prefers_canonical_id_over_aliases() {
    let normalized = normalize_auth_store(AuthStore {
        version: AUTH_STORE_VERSION,
        providers: HashMap::from([
            (
                "codex".to_string(),
                StoredCredential::ApiKey {
                    key: "alias-key".to_string(),
                },
            ),
            (
                "ChatGPT".to_string(),
                StoredCredential::ApiKey {
                    key: "canonical-key".to_string(),
                },
            ),
            (
                "openai-codex".to_string(),
                StoredCredential::ApiKey {
                    key: "second-alias-key".to_string(),
                },
            ),
        ]),
    });

    assert_eq!(normalized.providers.len(), 1);
    assert!(matches!(
        normalized.providers.get("chatgpt"),
        Some(StoredCredential::ApiKey { key }) if key == "canonical-key"
    ));
}

fn auth_store_declared_version(raw: &Value) -> Result<Option<u32>> {
    if raw.get("providers").is_none() && raw.get("version").is_none() {
        return Ok(None);
    }
    let version = raw
        .get("version")
        .map(|value| {
            value
                .as_u64()
                .and_then(|version| u32::try_from(version).ok())
                .context("auth store version must be a positive integer")
        })
        .transpose()?
        .unwrap_or(AUTH_STORE_VERSION);
    if version == 0 || version > AUTH_STORE_VERSION {
        anyhow::bail!("unsupported auth store version {version} (supported: {AUTH_STORE_VERSION})");
    }
    Ok(Some(version))
}

pub fn load_auth_store() -> Result<AuthStore> {
    let path = auth_store_path();
    let Some(text) = read_runtime_state_file(&path, true)? else {
        return Ok(AuthStore::default());
    };

    let raw: Value = serde_json::from_str(&text).context("invalid auth store JSON")?;

    if auth_store_declared_version(&raw)?.is_some() {
        let store: AuthStore = serde_json::from_value(raw).context("invalid auth store JSON")?;
        return Ok(normalize_auth_store(store));
    }

    let mut store = AuthStore::default();
    let object = raw
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("auth store must be a JSON object"))?;

    for (provider, value) in object {
        if canonical_provider_id(provider).is_empty() {
            continue;
        }

        if let Some(cred) = parse_external_auth_credential(value) {
            store.providers.insert(provider.clone(), cred);
            continue;
        }

        if let Some(api_key) = value.as_str() {
            let trimmed = api_key.trim();
            if !trimmed.is_empty() {
                store.providers.insert(
                    provider.clone(),
                    StoredCredential::ApiKey {
                        key: trimmed.to_string(),
                    },
                );
            }
        }
    }

    Ok(normalize_auth_store(store))
}

pub fn inspect_auth_store() -> AuthStoreInspection {
    let path = auth_store_path();
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return AuthStoreInspection {
                path,
                security: AuthStoreFileSecurity::Missing,
                integrity: AuthStoreIntegrity::NotChecked,
            };
        }
        Err(_) => {
            return AuthStoreInspection {
                path,
                security: AuthStoreFileSecurity::Unreadable,
                integrity: AuthStoreIntegrity::NotChecked,
            };
        }
    };

    if metadata.file_type().is_symlink() {
        return AuthStoreInspection {
            path,
            security: AuthStoreFileSecurity::Symlink,
            integrity: AuthStoreIntegrity::NotChecked,
        };
    }
    if !metadata.is_file() {
        return AuthStoreInspection {
            path,
            security: AuthStoreFileSecurity::NonRegular,
            integrity: AuthStoreIntegrity::NotChecked,
        };
    }

    #[cfg(unix)]
    if !runtime_state_owned_by_current_user(&metadata) {
        return AuthStoreInspection {
            path,
            security: AuthStoreFileSecurity::UnsafeOwner,
            integrity: AuthStoreIntegrity::NotChecked,
        };
    }

    #[cfg(unix)]
    let security = {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 == 0 {
            AuthStoreFileSecurity::Secure { mode }
        } else {
            AuthStoreFileSecurity::UnsafeMode { mode }
        }
    };
    #[cfg(windows)]
    let security = AuthStoreFileSecurity::WindowsAclNotEvaluated;
    #[cfg(not(any(unix, windows)))]
    let security = AuthStoreFileSecurity::PermissionsNotEvaluated;

    if metadata.len() > STATE_INSPECTION_MAX_BYTES {
        return AuthStoreInspection {
            path,
            security,
            integrity: AuthStoreIntegrity::TooLarge,
        };
    }
    let (bytes, opened) = match read_state_inspection_bytes(&path, &metadata) {
        Ok(result) => result,
        Err(too_large) => {
            return AuthStoreInspection {
                path,
                security,
                integrity: if too_large {
                    AuthStoreIntegrity::TooLarge
                } else {
                    AuthStoreIntegrity::Unreadable
                },
            };
        }
    };
    #[cfg(unix)]
    if !runtime_state_owned_by_current_user(&opened) {
        return AuthStoreInspection {
            path,
            security: AuthStoreFileSecurity::UnsafeOwner,
            integrity: AuthStoreIntegrity::NotChecked,
        };
    }
    #[cfg(unix)]
    let security = {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = opened.permissions().mode() & 0o777;
        if mode & 0o077 == 0 {
            AuthStoreFileSecurity::Secure { mode }
        } else {
            AuthStoreFileSecurity::UnsafeMode { mode }
        }
    };
    #[cfg(not(unix))]
    let _ = &opened;
    let raw = match serde_json::from_slice::<Value>(&bytes) {
        Ok(raw) => raw,
        Err(_) => {
            return AuthStoreInspection {
                path,
                security,
                integrity: AuthStoreIntegrity::InvalidSchema,
            };
        }
    };
    let declared = match auth_store_declared_version(&raw) {
        Ok(declared) => declared,
        Err(_) => {
            let integrity = raw
                .get("version")
                .and_then(Value::as_u64)
                .and_then(|version| u32::try_from(version).ok())
                .filter(|version| *version == 0 || *version > AUTH_STORE_VERSION)
                .map_or(AuthStoreIntegrity::InvalidSchema, |version| {
                    AuthStoreIntegrity::UnsupportedVersion { version }
                });
            return AuthStoreInspection {
                path,
                security,
                integrity,
            };
        }
    };
    let valid_schema = if declared.is_some() {
        serde_json::from_value::<AuthStore>(raw).is_ok()
    } else {
        raw.is_object()
    };
    let integrity = if valid_schema {
        AuthStoreIntegrity::Valid {
            version: declared.unwrap_or(AUTH_STORE_VERSION),
            legacy: declared.is_none(),
        }
    } else {
        AuthStoreIntegrity::InvalidSchema
    };

    AuthStoreInspection {
        path,
        security,
        integrity,
    }
}

fn verify_saved_auth_store(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("verifying saved auth store {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("saved auth store is not a regular file: {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "saved auth store has unsafe mode {mode:04o}; run chmod 600 {}",
                path.display()
            );
        }
    }
    Ok(())
}

pub fn save_auth_store(store: &AuthStore) -> Result<()> {
    let path = auth_store_path();
    let normalized = normalize_auth_store(store.clone());
    let bytes = serde_json::to_vec_pretty(&normalized)?;
    atomic_write_secret(&path, &bytes)?;
    verify_saved_auth_store(&path)
}

pub fn is_env_var_token(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

pub fn command_secret_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn resolve_secret_reference(spec: &str) -> Option<String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(cmd) = trimmed.strip_prefix('!') {
        let key = cmd.trim().to_string();
        if key.is_empty() {
            return None;
        }
        if let Some(cached) = command_secret_cache()
            .lock()
            .ok()
            .and_then(|m| m.get(&key).cloned())
        {
            // A cached None records a prior failure; don't re-run the command.
            return cached;
        }

        let secret = crate::host::run_secret_command(&key);
        if let Ok(mut cache) = command_secret_cache().lock() {
            cache.insert(key, secret.clone());
        }
        return secret;
    }

    if is_env_var_token(trimmed)
        && let Ok(v) = std::env::var(trimmed)
    {
        let val = v.trim().to_string();
        if !val.is_empty() {
            return Some(val);
        }
    }

    Some(trimmed.to_string())
}

impl StoredCredential {
    pub fn resolve_secret(&self) -> Option<String> {
        match self {
            StoredCredential::ApiKey { key } => resolve_secret_reference(key),
            StoredCredential::OAuth {
                access_token,
                expires_at,
                ..
            } => {
                if (*expires_at).is_some_and(|ts| ts > 0 && unix_timestamp_secs() >= ts) {
                    return None;
                }
                let t = access_token.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            }
        }
    }
}

pub fn resolve_active_provider_id(catalog: &ProviderCatalog) -> String {
    if let Ok(v) = std::env::var("DEXT_PROVIDER") {
        let c = canonical_provider_id(&v);
        if !c.is_empty() {
            return c;
        }
    }
    if let Ok(v) = std::env::var("DEXT_PROFILE") {
        let c = canonical_provider_id(&v);
        if !c.is_empty() {
            return c;
        }
    }
    if let Ok(v) = std::env::var("DEXT_API_PROVIDER") {
        let lower = v.trim().to_ascii_lowercase();
        let canonical = canonical_provider_id(&lower);
        if lower == "anthropic" && find_provider_profile(catalog, "anthropic").is_none() {
            return "glm".to_string();
        }
        if find_provider_profile(catalog, &canonical).is_some() {
            return canonical;
        }
    }
    canonical_provider_id(&catalog.active_provider)
}

fn dext_api_key_override() -> Option<(String, String)> {
    let key = std::env::var("DEXT_API_KEY").ok()?;
    let key = key.trim();
    (!key.is_empty()).then(|| (key.to_string(), "env:DEXT_API_KEY".to_string()))
}

fn resolve_provider_auth(
    profile: &ProviderProfile,
    store: &AuthStore,
) -> Option<(String, String, RuntimeAuthKind)> {
    if let Some((secret, source)) = dext_api_key_override() {
        return Some((secret, source, RuntimeAuthKind::ApiKey));
    }

    let canonical_id = canonical_provider_id(&profile.id);
    if let Some(entry) = store
        .providers
        .get(&profile.id)
        .or_else(|| store.providers.get(&canonical_id))
        && !(canonical_id == "kimi" && matches!(entry, StoredCredential::OAuth { .. }))
        && let Some(secret) = entry.resolve_secret()
    {
        let kind = match entry {
            StoredCredential::ApiKey { .. } => RuntimeAuthKind::ApiKey,
            StoredCredential::OAuth { .. } => RuntimeAuthKind::OAuth,
        };
        return Some((secret, format!("auth:{}", profile.id), kind));
    }

    for env in &profile.env_vars {
        if let Ok(v) = std::env::var(env) {
            let t = v.trim().to_string();
            if !t.is_empty() {
                return Some((t, format!("env:{env}"), RuntimeAuthKind::ApiKey));
            }
        }
    }
    None
}

fn resolve_provider_credential(
    profile: &ProviderProfile,
    store: &AuthStore,
) -> Option<(String, String)> {
    resolve_provider_auth(profile, store).map(|(secret, source, _)| (secret, source))
}

pub fn resolve_provider_api_key(
    profile: &ProviderProfile,
    store: &AuthStore,
) -> Option<(String, String)> {
    resolve_provider_credential(profile, store)
}

pub fn resolve_provider_model(profile: &ProviderProfile) -> String {
    let provider_env = format!(
        "DEXT_MODEL_{}",
        canonical_provider_id(&profile.id)
            .replace('-', "_")
            .to_ascii_uppercase()
    );
    if let Ok(v) = std::env::var(&provider_env) {
        let t = normalize_provider_model_value(profile, &v);
        if !t.is_empty() {
            return t;
        }
    }

    if let Ok(v) = std::env::var("DEXT_MODEL") {
        let t = normalize_provider_model_value(profile, &v);
        if !t.is_empty() {
            let force = std::env::var("DEXT_MODEL_FORCE").ok().is_some_and(|raw| {
                let low = raw.trim().to_ascii_lowercase();
                !(low.is_empty() || low == "0" || low == "false" || low == "off" || low == "no")
            });
            let compatible = profile.models.is_empty() || profile.models.iter().any(|m| m == &t);
            if force || compatible {
                return t;
            }
            if !force
                && !compatible
                && request_contract_for_profile(profile).api_provider() == ApiProvider::OpenAi
                && !profile.requires_api_key
            {
                let looks_local = is_local_llama_provider(
                    &profile.id,
                    request_contract_for_profile(profile).api_provider(),
                    &profile.base_url,
                );
                if looks_local {
                    return t;
                }
            }
        }
    }
    normalize_provider_model_value(profile, &profile.default_model)
}

pub fn resolve_provider_base_url(profile: &ProviderProfile) -> String {
    if let Ok(v) = std::env::var("DEXT_BASE_URL") {
        let t = v.trim().trim_end_matches('/').to_string();
        if !t.is_empty() {
            return t;
        }
    }

    match request_contract_for_profile(profile).api_provider() {
        ApiProvider::OpenAi => {
            if let Ok(v) = std::env::var("OPENAI_BASE_URL") {
                let t = v.trim().trim_end_matches('/').to_string();
                if !t.is_empty() {
                    return t;
                }
            }
        }
        ApiProvider::ChatGpt => {}
        ApiProvider::Anthropic => {
            if let Ok(v) = std::env::var("ANTHROPIC_BASE_URL") {
                let t = v.trim().trim_end_matches('/').to_string();
                if !t.is_empty() {
                    return t;
                }
            }
        }
    }

    profile.base_url.trim_end_matches('/').to_string()
}

pub fn chatgpt_client_user_agent() -> String {
    format!(
        "dext ({}; {})",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

pub fn apply_provider_headers(
    req: RequestBuilder,
    contract: RequestContract,
    api_key: &str,
    official_kimi: bool,
    anthropic_subscription: bool,
    extended_anthropic_cache: bool,
    session_id: Option<&str>,
) -> Result<RequestBuilder> {
    Ok(req.headers(provider_headers(
        contract,
        api_key,
        official_kimi,
        anthropic_subscription,
        extended_anthropic_cache,
        session_id,
    )?))
}

/// Native header facts, resolved only when a request is sent.
pub fn provider_headers(
    contract: RequestContract,
    api_key: &str,
    official_kimi: bool,
    anthropic_subscription: bool,
    extended_anthropic_cache: bool,
    session_id: Option<&str>,
) -> Result<HeaderMap> {
    let anthropic = contract.api_provider() == ApiProvider::Anthropic;
    let request_id = (anthropic && anthropic_subscription)
        .then(crate::claude_subscription::random_uuid_v4)
        .transpose()?;
    let kimi = if anthropic && !anthropic_subscription && official_kimi {
        Some((
            kimi_device_name(),
            format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            kimi_device_id()?,
        ))
    } else {
        None
    };
    let context = HeaderContext {
        originator: &oauth_originator(),
        user_agent: &chatgpt_client_user_agent(),
        request_id: request_id.as_deref(),
        kimi: kimi.as_ref().map(|(name, model, id)| KimiHeaderContext {
            device_name: name,
            device_model: model,
            os_version: std::env::consts::OS,
            device_id: id,
        }),
    };
    provider_headers_with_context(
        contract,
        api_key,
        official_kimi,
        anthropic_subscription,
        extended_anthropic_cache,
        session_id,
        &context,
    )
}

pub fn resolve_runtime_provider(
    selected: Option<&str>,
    require_credentials: bool,
) -> Result<ResolvedProviderConfig> {
    let catalog = load_provider_catalog()?;
    let store = load_auth_store()?;

    let provider_id = selected
        .map(canonical_provider_id)
        .unwrap_or_else(|| resolve_active_provider_id(&catalog));

    let mut profile = find_provider_profile(&catalog, &provider_id).ok_or_else(|| {
        let available = catalog
            .providers
            .iter()
            .map(|p| p.id.clone())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::anyhow!("unknown provider '{provider_id}'. available: {available}")
    })?;

    let mut model = resolve_provider_model(&profile);

    // Stale-default auto-reroute: older builds could persist a model as the
    // default for the wrong provider (e.g. `/model glm-5.1` saved glm-5.1 as
    // chatgpt's default_model). On startup without an explicit provider, if
    // the resolved model isn't owned by this provider per the built-in
    // catalog (which is authoritative — the merged profile's `models` can be
    // polluted by a stale default_model auto-injection), redirect to the
    // built-in owner if it's authenticated. Without this, ChatGPT's Codex
    // backend returns HTTP 400 "The '<model>' model is not supported when
    // using Codex with a ChatGPT account." on the first turn after the stale
    // save.
    if selected.is_none() {
        let builtins = built_in_provider_profiles();
        let active_canonical = canonical_provider_id(&profile.id);
        let owned_by_active = builtins
            .iter()
            .find(|b| canonical_provider_id(&b.id) == active_canonical)
            .map(|b| b.models.iter().any(|m| m.eq_ignore_ascii_case(&model)))
            .unwrap_or(true);
        if !owned_by_active {
            let mut candidate: Option<(ProviderProfile, String)> = None;
            let mut ambiguous = false;
            for builtin in &builtins {
                if canonical_provider_id(&builtin.id) == active_canonical {
                    continue;
                }
                if !builtin
                    .models
                    .iter()
                    .any(|m| m.eq_ignore_ascii_case(&model))
                {
                    continue;
                }
                let Some(stored_profile) = find_provider_profile(&catalog, &builtin.id) else {
                    continue;
                };
                if !provider_has_available_credentials(&stored_profile, &store) {
                    continue;
                }
                if candidate.is_some() {
                    ambiguous = true;
                    break;
                }
                let normalized = normalize_provider_model_value(&stored_profile, &model);
                candidate = Some((stored_profile, normalized));
            }
            if !ambiguous && let Some((new_profile, new_model)) = candidate {
                profile = new_profile;
                model = new_model;
            }
        }
    }

    let base_url = resolve_provider_base_url(&profile);
    let mut store = store;
    let explicit_api_key = dext_api_key_override();
    let stored_oauth = store
        .providers
        .get(&profile.id)
        .or_else(|| store.providers.get(&canonical_provider_id(&profile.id)))
        .is_some_and(|credential| matches!(credential, StoredCredential::OAuth { .. }));
    if explicit_api_key.is_none()
        && stored_oauth
        && is_marked_builtin_anthropic(&profile)
        && !is_official_anthropic_profile(&profile, &base_url)
    {
        anyhow::bail!(
            "Anthropic subscription OAuth is restricted to {ANTHROPIC_BASE_URL}. Unset the Anthropic base-URL override or run `dext auth logout anthropic` and use an API key for the custom endpoint."
        );
    }
    let refreshed_token = if explicit_api_key.is_some() {
        None
    } else {
        refresh_oauth_credential_if_needed(&profile, &mut store)?
    };
    let resolved_auth = explicit_api_key
        .map(|(key, source)| (key, source, RuntimeAuthKind::ApiKey))
        .or_else(|| {
            refreshed_token.map(|token| {
                (
                    token,
                    format!("auth:{} (refreshed)", profile.id),
                    RuntimeAuthKind::OAuth,
                )
            })
        })
        .or_else(|| resolve_provider_auth(&profile, &store));

    let (api_key, key_source, auth_kind) = match (profile.requires_api_key, resolved_auth) {
        (_, Some((key, source, kind))) => (key, source, kind),
        (false, None) => (
            String::new(),
            "none (provider does not require key)".to_string(),
            RuntimeAuthKind::None,
        ),
        (true, None) if !require_credentials => (
            String::new(),
            "missing (login required)".to_string(),
            RuntimeAuthKind::None,
        ),
        (true, None) => {
            let env_hint = if profile.env_vars.is_empty() {
                "(no env vars configured)".to_string()
            } else {
                profile.env_vars.join(" or ")
            };
            anyhow::bail!(
                "missing credentials for provider '{}'. Run `dext auth login {}` or set {}.",
                profile.id,
                profile.id,
                env_hint
            );
        }
    };

    if auth_kind == RuntimeAuthKind::OAuth
        && is_marked_builtin_anthropic(&profile)
        && !is_official_anthropic_profile(&profile, &base_url)
    {
        anyhow::bail!(
            "Anthropic subscription OAuth is restricted to {ANTHROPIC_BASE_URL}. Unset the Anthropic base-URL override or run `dext auth logout anthropic` and use an API key for the custom endpoint."
        );
    }

    Ok(ResolvedProviderConfig {
        requires_api_key: profile.requires_api_key,
        profile,
        api_key,
        auth_kind,
        key_source,
        model,
        base_url,
    })
}

pub fn provider_auth_status(profile: &ProviderProfile, store: &AuthStore) -> String {
    let canonical = canonical_provider_id(&profile.id);
    if let Some(entry) = store
        .providers
        .get(&profile.id)
        .or_else(|| store.providers.get(&canonical))
        && !(canonical == "kimi" && matches!(entry, StoredCredential::OAuth { .. }))
    {
        let state = if entry.resolve_secret().is_some() {
            "auth"
        } else {
            "auth(unresolved)"
        };
        return state.to_string();
    }
    for env in &profile.env_vars {
        if std::env::var(env)
            .ok()
            .is_some_and(|v| !v.trim().is_empty())
        {
            return format!("env:{env}");
        }
    }
    if profile.requires_api_key {
        "missing".to_string()
    } else {
        "not-required".to_string()
    }
}

pub fn provider_has_available_credentials(profile: &ProviderProfile, store: &AuthStore) -> bool {
    !profile.requires_api_key || resolve_provider_api_key(profile, store).is_some()
}

fn pick_matching_model(
    matches: &[ProviderModelRef],
    active_provider: &str,
) -> Result<Option<ProviderModelRef>> {
    if matches.is_empty() {
        return Ok(None);
    }
    let active_provider = canonical_provider_id(active_provider);
    if let Some(active) = matches
        .iter()
        .find(|m| canonical_provider_id(&m.provider_id) == active_provider)
    {
        return Ok(Some(active.clone()));
    }
    if matches.len() == 1 {
        return Ok(Some(matches[0].clone()));
    }
    let rendered = matches
        .iter()
        .map(|m| format!("{}/{}", m.provider_id, m.model))
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!("model matches multiple providers: {rendered}. use /model <provider>/<model>.");
}

pub fn resolve_provider_model_selection(
    catalog: &ProviderCatalog,
    store: &AuthStore,
    active_provider: &str,
    selector: &str,
) -> Result<ProviderModelRef> {
    let selector = selector.trim();
    if selector.is_empty() {
        anyhow::bail!("model selector cannot be empty");
    }

    for sep in ['/', ':'] {
        if let Some((provider_sel, model_sel)) = selector.split_once(sep) {
            if provider_sel.trim().is_empty() {
                continue;
            }
            if let Ok(provider_id) = provider_id_from_selector(catalog, provider_sel) {
                let profile = find_provider_profile(catalog, &provider_id)
                    .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?;
                let model = normalize_provider_model_value(&profile, model_sel);
                if model.is_empty() {
                    anyhow::bail!("model selector cannot be empty");
                }
                if profile.requires_api_key && !provider_has_available_credentials(&profile, store)
                {
                    anyhow::bail!(
                        "provider '{provider_id}' is not authenticated. Run `/login {provider_id}` first."
                    );
                }
                return Ok(ProviderModelRef { provider_id, model });
            }
        }
    }

    let mut auth_matches = Vec::new();
    let mut all_matches = Vec::new();
    for profile in &catalog.providers {
        let normalized = normalize_provider_model_value(profile, selector);
        if normalized.is_empty() {
            continue;
        }
        let alias_key = normalize_model_alias_key(selector);
        let matches_alias = profile.model_aliases.contains_key(&alias_key);
        let matches_curated = matches_alias
            || curated_provider_models(profile)
                .iter()
                .any(|model| model.eq_ignore_ascii_case(&normalized));
        if !matches_curated {
            continue;
        }
        let matched = ProviderModelRef {
            provider_id: canonical_provider_id(&profile.id),
            model: normalized,
        };
        if provider_has_available_credentials(profile, store) {
            auth_matches.push(matched.clone());
        }
        all_matches.push(matched);
    }

    if let Some(matched) = pick_matching_model(&auth_matches, active_provider)? {
        return Ok(matched);
    }
    if let Some(matched) = pick_matching_model(&all_matches, active_provider)? {
        return Ok(matched);
    }

    let active_provider = canonical_provider_id(active_provider);
    let profile = find_provider_profile(catalog, &active_provider)
        .ok_or_else(|| anyhow::anyhow!("unknown provider '{active_provider}'"))?;
    let model = normalize_provider_model_value(&profile, selector);
    if model.is_empty() {
        anyhow::bail!("model selector cannot be empty");
    }
    Ok(ProviderModelRef {
        provider_id: active_provider,
        model,
    })
}

pub fn render_provider_list(catalog: &ProviderCatalog, store: &AuthStore, active: &str) -> String {
    let mut lines = Vec::new();
    for profile in &catalog.providers {
        let marker = if canonical_provider_id(&profile.id) == canonical_provider_id(active) {
            "*"
        } else {
            " "
        };
        let name = if profile.display_name.trim().is_empty() {
            profile.id.clone()
        } else {
            profile.display_name.clone()
        };
        let status = provider_auth_status(profile, store);
        let contract = request_contract_for_profile(profile);
        let spec = resolve_model_spec(profile, &profile.default_model);
        lines.push(format!(
            "{marker} {:<12} {:<18} model={} contract={} api={} spec={} auth={} base={}",
            profile.id,
            name,
            profile.default_model,
            contract.as_str(),
            contract.api_provider().as_str(),
            spec.source,
            status,
            profile.base_url
        ));
    }
    lines.join("\n")
}

pub fn render_provider_picker(
    catalog: &ProviderCatalog,
    store: &AuthStore,
    active: &str,
) -> String {
    let mut lines = Vec::new();
    for (i, profile) in catalog.providers.iter().enumerate() {
        let marker = if canonical_provider_id(&profile.id) == canonical_provider_id(active) {
            "*"
        } else {
            " "
        };
        let status = provider_auth_status(profile, store);
        let contract = request_contract_for_profile(profile);
        let spec = resolve_model_spec(profile, &profile.default_model);
        lines.push(format!(
            "{:>2}) {} {:<10} model={:<16} contract={} spec={} auth={}",
            i + 1,
            marker,
            profile.id,
            profile.default_model,
            contract.as_str(),
            spec.source,
            status
        ));
    }
    lines.join("\n")
}

pub fn external_auth_path() -> PathBuf {
    if let Ok(path) = std::env::var("DEXT_EXTERNAL_AUTH_FILE") {
        return PathBuf::from(path);
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".dext").join("external-auth.json")
}

pub fn parse_expiry_epoch_seconds(v: &Value) -> Option<u64> {
    let mut epoch = if let Some(n) = v.as_u64() {
        n
    } else {
        v.as_str()?.trim().parse::<u64>().ok()?
    };
    if epoch > 1_000_000_000_000 {
        epoch /= 1000;
    }
    Some(epoch)
}

pub fn parse_external_auth_credential(value: &Value) -> Option<StoredCredential> {
    if let Ok(cred) = serde_json::from_value::<StoredCredential>(value.clone()) {
        return Some(cred);
    }

    if let Some(raw) = value.as_str() {
        let key = raw.trim();
        if !key.is_empty() {
            return Some(StoredCredential::ApiKey {
                key: key.to_string(),
            });
        }
    }

    let key = value
        .get("key")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    if let Some(key) = key {
        return Some(StoredCredential::ApiKey { key });
    }

    let access = value
        .get("access_token")
        .or_else(|| value.get("access"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    let access_token = access?;

    let refresh_token = value
        .get("refresh_token")
        .or_else(|| value.get("refresh"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);

    let expires_at = value
        .get("expires_at")
        .or_else(|| value.get("expires"))
        .and_then(parse_expiry_epoch_seconds);

    Some(StoredCredential::OAuth {
        access_token,
        refresh_token,
        expires_at,
    })
}

pub fn external_auth_candidates(provider_id: &str) -> Vec<String> {
    let canonical = canonical_provider_id(provider_id);
    let mut candidates: Vec<String> = match canonical.as_str() {
        "chatgpt" => vec!["openai-codex", "codex", "chatgpt", "openai"],
        "openai" => vec!["openai"],
        "glm" => vec!["zai", "glm", "bigmodel", "anthropic"],
        "anthropic" => vec!["anthropic", "claude"],
        other => vec![other],
    }
    .into_iter()
    .map(str::to_string)
    .collect();

    if !candidates.iter().any(|c| c == &canonical) {
        candidates.push(canonical);
    }
    candidates
}

pub fn import_provider_credential_from_external(
    profile: &ProviderProfile,
    store: &mut AuthStore,
) -> Option<String> {
    let path = external_auth_path();
    let text = std::fs::read_to_string(&path).ok()?;
    let raw: Value = serde_json::from_str(&text).ok()?;
    let object = raw.as_object()?;

    for candidate in external_auth_candidates(&profile.id) {
        let Some(value) = object.get(&candidate) else {
            continue;
        };
        let Some(credential) = parse_external_auth_credential(value) else {
            continue;
        };
        store
            .providers
            .insert(canonical_provider_id(&profile.id), credential);
        return Some(format!("external-auth:{candidate}"));
    }

    None
}

pub fn login_arg_requests_web_flow(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "web" | "--web" | "browser" | "--browser" | "reauth" | "--reauth"
    )
}

pub fn login_arg_requests_import(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "import" | "--import" | "reuse" | "--reuse"
    )
}

pub fn looks_like_login_secret_input(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    if extract_oauth_code_from_callback(trimmed).is_some() {
        return true;
    }
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return true;
    }
    if trimmed.contains("accessToken") || trimmed.contains("access_token") {
        return true;
    }
    normalize_login_secret(trimmed)
        .is_some_and(|secret| secret.len() >= 20 && !secret.contains(char::is_whitespace))
}

pub fn normalize_login_secret(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if login_arg_requests_web_flow(trimmed) {
        return None;
    }

    let bearer_prefix = "bearer ";
    if trimmed.len() > bearer_prefix.len()
        && trimmed
            .get(..bearer_prefix.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(bearer_prefix))
    {
        return Some(trimmed[bearer_prefix.len()..].trim().to_string());
    }

    if trimmed.starts_with('{')
        && trimmed.ends_with('}')
        && let Ok(v) = serde_json::from_str::<Value>(trimmed)
        && let Some(token) = v
            .get("accessToken")
            .or_else(|| v.get("access_token"))
            .or_else(|| v.get("access"))
            .and_then(|s| s.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    {
        return Some(token.to_string());
    }

    Some(trimmed.to_string())
}

pub fn validate_login_secret_for_provider(profile: &ProviderProfile, secret: &str) -> Result<()> {
    if request_contract_for_profile(profile) == RequestContract::ChatGptResponses
        && chatgpt_account_id_from_token(secret).is_none()
    {
        anyhow::bail!(
            "invalid ChatGPT access token format. paste the 'accessToken' JWT from chatgpt.com/api/auth/session or the full JSON session blob."
        );
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn running_in_wsl() -> bool {
    if std::env::var("WSL_DISTRO_NAME")
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
    {
        return true;
    }
    if std::env::var("WSL_INTEROP")
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
    {
        return true;
    }
    std::fs::read_to_string("/proc/version")
        .map(|s| s.to_ascii_lowercase().contains("microsoft"))
        .unwrap_or(false)
}

#[cfg(any(not(unix), target_os = "macos"))]
pub fn running_in_wsl() -> bool {
    false
}

fn browser_launcher_timeout() -> Duration {
    let seconds = std::env::var("DEXT_BROWSER_OPEN_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| (1..=30).contains(seconds))
        .unwrap_or(10);
    Duration::from_secs(seconds)
}

fn run_browser_launcher(mut command: Command, label: &str) -> Result<crate::host::CommandOutput> {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::host::run_command_limited(command, label, browser_launcher_timeout())
        .map_err(anyhow::Error::msg)
}

pub fn open_url_in_browser(url: &str) -> Result<String> {
    if std::env::var("DEXT_SKIP_BROWSER_OPEN")
        .ok()
        .is_some_and(|raw| {
            let low = raw.trim().to_ascii_lowercase();
            !(low.is_empty() || low == "0" || low == "false" || low == "off" || low == "no")
        })
    {
        return Ok("disabled-by-env".to_string());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    if running_in_wsl()
        && std::env::var("DEXT_DISABLE_WSL_BROWSER_OPEN")
            .ok()
            .is_some_and(|raw| {
                let low = raw.trim().to_ascii_lowercase();
                !(low.is_empty() || low == "0" || low == "false" || low == "off" || low == "no")
            })
    {
        return Ok("disabled-by-wsl".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        let powershell_cmd = format!("Start-Process '{}'", url.replace('\'', "''"));
        let mut command = Command::new("powershell");
        command
            .args(["-NoProfile", "-Command", &powershell_cmd])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let output = run_browser_launcher(command, "powershell browser launcher")
            .context("failed to launch browser via powershell Start-Process")?;
        if !output.success() {
            anyhow::bail!("browser launcher exited with status {}", output.code);
        }
        Ok("powershell Start-Process".to_string())
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("open");
        command
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let output = run_browser_launcher(command, "macOS browser launcher")
            .context("failed to launch browser via open")?;
        if !output.success() {
            anyhow::bail!("browser launcher exited with status {}", output.code);
        }
        Ok("open".to_string())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut launchers: Vec<(String, Vec<String>)> = vec![
            ("xdg-open".to_string(), vec![url.to_string()]),
            ("gio".to_string(), vec!["open".to_string(), url.to_string()]),
            ("sensible-browser".to_string(), vec![url.to_string()]),
            ("x-www-browser".to_string(), vec![url.to_string()]),
        ];
        if running_in_wsl() {
            let powershell_cmd = format!("Start-Process '{}'", url.replace('\'', "''"));
            launchers.splice(
                0..0,
                [
                    ("wslview".to_string(), vec![url.to_string()]),
                    (
                        "powershell.exe".to_string(),
                        vec![
                            "-NoProfile".to_string(),
                            "-Command".to_string(),
                            powershell_cmd,
                        ],
                    ),
                ],
            );
        }

        let mut errors = Vec::new();
        for (bin, args) in launchers {
            let mut command = Command::new(&bin);
            command
                .args(&args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            match run_browser_launcher(command, &format!("{bin} browser launcher")) {
                Ok(output) if output.success() => return Ok(bin),
                Ok(output) => errors.push(format!("{bin} exited {}", output.code)),
                Err(error) => errors.push(format!("{bin}: {error}")),
            }
        }

        anyhow::bail!(
            "could not auto-open URL; tried launchers: {}",
            errors.join("; ")
        );
    }
}

pub fn prompt_input_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;

    if line.trim_start().starts_with('{') {
        loop {
            let trimmed = line.trim();
            if trimmed.ends_with('}') {
                break;
            }
            let mut extra = String::new();
            let read = io::stdin().read_line(&mut extra)?;
            if read == 0 {
                break;
            }
            line.push_str(&extra);
        }
    }

    Ok(line.trim().to_string())
}

pub fn set_active_provider_in_catalog(provider_id: &str) -> Result<()> {
    let mut catalog = load_provider_catalog()?;
    let canonical = canonical_provider_id(provider_id);
    let profile = find_provider_profile(&catalog, &canonical)
        .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?;
    catalog.active_provider = canonical_provider_id(&profile.id);
    save_provider_catalog(&catalog)
}

pub fn set_provider_default_model_in_catalog(provider_id: &str, model: &str) -> Result<()> {
    let model = model.trim();
    if model.is_empty() {
        anyhow::bail!("model cannot be empty");
    }

    let mut catalog = load_provider_catalog()?;
    let canonical = canonical_provider_id(provider_id);
    let Some(profile) = catalog
        .providers
        .iter_mut()
        .find(|p| canonical_provider_id(&p.id) == canonical)
    else {
        anyhow::bail!("unknown provider '{provider_id}'");
    };

    let normalized = normalize_provider_model_value(profile, model);
    profile.default_model = normalized.clone();
    if !profile
        .models
        .iter()
        .any(|m| m.eq_ignore_ascii_case(&normalized))
    {
        profile.models.insert(0, normalized);
    }
    catalog.active_provider = canonical;

    save_provider_catalog(&catalog)
}

#[derive(Debug)]
pub struct LoginResult {
    pub message: String,
    pub provider_id: String,
    pub awaiting_credentials: bool,
}

fn generate_code_verifier() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).context("generate OAuth PKCE verifier")?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn pkce_code_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash)
}

fn generate_oauth_state() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).context("generate OAuth state")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

fn oauth_html_response(status_line: &str, body: &str) -> String {
    format!(
        "{status_line}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; frame-ancestors 'none'\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn oauth_result_page(success: bool) -> String {
    let (eyebrow, title, detail, accent, mark) = if success {
        (
            "AUTHENTICATION COMPLETE",
            "You're signed in",
            "Dext received and stored your provider credentials. You can close this tab and return to your terminal.",
            "#72e0a8",
            "&#10003;",
        )
    } else {
        (
            "AUTHENTICATION INCOMPLETE",
            "Return to Dext",
            "The callback was received, but Dext could not finish the login. Check the terminal for details and retry there.",
            "#ff8f8f",
            "!",
        )
    };
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Dext · {title}</title><style>:root{{color-scheme:dark}}*{{box-sizing:border-box}}body{{margin:0;min-height:100vh;display:grid;place-items:center;padding:24px;background:#090b10;color:#edf2f7;font:16px/1.55 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}}main{{width:min(560px,100%);padding:38px;border:1px solid #28303d;border-radius:18px;background:linear-gradient(145deg,#121722,#0d1119);box-shadow:0 24px 80px #0009}}.brand{{margin-bottom:34px;color:#a9b4c5;letter-spacing:.16em}}.brand b{{color:#f5f7fa}}.mark{{display:grid;place-items:center;width:54px;height:54px;margin-bottom:22px;border:1px solid {accent};border-radius:14px;color:{accent};font-size:28px;font-weight:700;box-shadow:0 0 32px color-mix(in srgb,{accent} 24%,transparent)}}.eyebrow{{margin:0 0 8px;color:{accent};font-size:12px;font-weight:700;letter-spacing:.14em}}h1{{margin:0 0 14px;font:600 clamp(28px,7vw,42px)/1.15 ui-sans-serif,system-ui,sans-serif;letter-spacing:-.03em}}p{{margin:0;color:#aeb8c7}}.foot{{margin-top:30px;padding-top:20px;border-top:1px solid #252d39;color:#778397;font-size:13px}}</style></head><body><main><div class="brand"><b>DEXT</b> / LOGIN</div><div class="mark">{mark}</div><p class="eyebrow">{eyebrow}</p><h1>{title}</h1><p>{detail}</p><div class="foot">This window contains no credential data.</div></main></body></html>"#
    )
}

fn await_oauth_browser_result(
    receiver: &std::sync::mpsc::Receiver<OAuthBrowserResult>,
    cancelled: &std::sync::atomic::AtomicBool,
    timeout: Duration,
) -> Option<OAuthBrowserResult> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // Login queues its result before Drop cancels and joins the listener.
        if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            return receiver.try_recv().ok();
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return receiver.try_recv().ok();
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(result) => return Some(result),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum OAuthBrowserResult {
    Success,
    Failure,
}

enum OAuthCallbackKind {
    Code(String),
    Rejected,
}

struct ReceivedOAuthCallback {
    kind: OAuthCallbackKind,
    browser_result: std::sync::mpsc::Sender<OAuthBrowserResult>,
}

struct OAuthCallbackListener {
    receiver: std::sync::mpsc::Receiver<ReceivedOAuthCallback>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl OAuthCallbackListener {
    fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<ReceivedOAuthCallback, std::sync::mpsc::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

impl Drop for OAuthCallbackListener {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn spawn_oauth_code_listener(
    listener: std::net::TcpListener,
    expected_state: String,
    expected_path: String,
) -> Result<OAuthCallbackListener> {
    listener
        .set_nonblocking(true)
        .context("configure OAuth callback listener")?;
    let (tx, receiver) = std::sync::mpsc::channel::<ReceivedOAuthCallback>();
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread_cancelled = cancelled.clone();

    let thread = std::thread::spawn(move || {
        while !thread_cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                }
                Err(_) => break,
            };
            use std::io::{Read, Write};

            let request_started = std::time::Instant::now();
            if stream.set_nonblocking(false).is_err()
                || stream
                    .set_write_timeout(Some(OAUTH_CALLBACK_IO_TIMEOUT))
                    .is_err()
            {
                continue;
            }
            let mut request = Vec::with_capacity(1024);
            let mut buf = [0u8; 1024];
            let mut headers_complete = false;
            while request.len() < 8192 {
                let read_timeout =
                    OAUTH_CALLBACK_IO_TIMEOUT.saturating_sub(request_started.elapsed());
                if read_timeout.is_zero() {
                    break;
                }
                if stream.set_read_timeout(Some(read_timeout)).is_err() {
                    break;
                }
                let remaining = 8192 - request.len();
                let read_len = remaining.min(buf.len());
                match stream.read(&mut buf[..read_len]) {
                    Ok(0) => break,
                    Ok(n) => {
                        request.extend_from_slice(&buf[..n]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            headers_complete = true;
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let req = String::from_utf8_lossy(&request);
            let path = headers_complete
                .then(|| req.lines().next())
                .flatten()
                .and_then(|line| {
                    let mut parts = line.split_whitespace();
                    let method = parts.next()?;
                    let path = parts.next()?;
                    let version = parts.next()?;
                    (method == "GET"
                        && matches!(version, "HTTP/1.0" | "HTTP/1.1")
                        && parts.next().is_none())
                    .then_some(path)
                });

            let mut accepted_callback = false;
            let response = if let Some(url) =
                path.and_then(|path| reqwest::Url::parse(&format!("http://localhost{path}")).ok())
            {
                if url.path() != expected_path {
                    oauth_html_response(
                        "HTTP/1.1 404 Not Found",
                        "<html><body><h2>Login failed</h2><p>Callback route not found.</p></body></html>",
                    )
                } else {
                    let returned_state = url
                        .query_pairs()
                        .find(|(key, _)| key == "state")
                        .map(|(_, value)| value.into_owned());
                    if returned_state.as_deref() != Some(expected_state.as_str()) {
                        oauth_html_response(
                            "HTTP/1.1 400 Bad Request",
                            "<html><body><h2>Login failed</h2><p>State mismatch.</p></body></html>",
                        )
                    } else if let Some(code) = url
                        .query_pairs()
                        .find(|(key, _)| key == "code")
                        .map(|(_, value)| value.into_owned())
                        .filter(|code| !code.trim().is_empty())
                        .filter(|_| !url.query_pairs().any(|(key, _)| key == "error"))
                    {
                        let (browser_result, result_receiver) = std::sync::mpsc::channel();
                        accepted_callback = true;
                        let delivered = tx
                            .send(ReceivedOAuthCallback {
                                kind: OAuthCallbackKind::Code(code),
                                browser_result,
                            })
                            .is_ok();
                        match delivered.then(|| {
                            await_oauth_browser_result(
                                &result_receiver,
                                &thread_cancelled,
                                OAUTH_REQUEST_TIMEOUT + Duration::from_secs(5),
                            )
                        }) {
                            Some(Some(OAuthBrowserResult::Success)) => {
                                oauth_html_response("HTTP/1.1 200 OK", &oauth_result_page(true))
                            }
                            Some(Some(OAuthBrowserResult::Failure)) | Some(None) | None => {
                                oauth_html_response(
                                    "HTTP/1.1 500 Internal Server Error",
                                    &oauth_result_page(false),
                                )
                            }
                        }
                    } else if url.query_pairs().any(|(key, _)| key == "error") {
                        let (browser_result, result_receiver) = std::sync::mpsc::channel();
                        accepted_callback = true;
                        let delivered = tx
                            .send(ReceivedOAuthCallback {
                                kind: OAuthCallbackKind::Rejected,
                                browser_result,
                            })
                            .is_ok();
                        if delivered {
                            let _ = await_oauth_browser_result(
                                &result_receiver,
                                &thread_cancelled,
                                Duration::from_secs(2),
                            );
                        }
                        oauth_html_response("HTTP/1.1 400 Bad Request", &oauth_result_page(false))
                    } else {
                        oauth_html_response(
                            "HTTP/1.1 400 Bad Request",
                            "<html><body><h2>Login failed</h2><p>Missing authorization code.</p></body></html>",
                        )
                    }
                }
            } else {
                oauth_html_response(
                    "HTTP/1.1 400 Bad Request",
                    "<html><body><h2>Login failed</h2><p>Invalid callback request.</p></body></html>",
                )
            };
            let _ = stream.write_all(response.as_bytes());
            if accepted_callback {
                break;
            }
        }
    });

    Ok(OAuthCallbackListener {
        receiver,
        cancelled,
        thread: Some(thread),
    })
}

#[cfg(test)]
fn receive_oauth_callback_for_test(
    expected_state: &str,
    expected_path: &str,
    requests: &[String],
) -> Result<String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let callback = spawn_oauth_code_listener(
        listener,
        expected_state.to_string(),
        expected_path.to_string(),
    )?;
    for request in requests {
        let request = request.clone();
        let client = std::thread::spawn(move || -> io::Result<Vec<u8>> {
            let mut stream = std::net::TcpStream::connect(address)?;
            stream.write_all(request.as_bytes())?;
            stream.flush()?;
            let mut response = Vec::new();
            if let Err(error) = stream.read_to_end(&mut response)
                && error.kind() != io::ErrorKind::ConnectionReset
            {
                return Err(error);
            }
            Ok(response)
        });
        match callback.recv_timeout(Duration::from_millis(500)) {
            Ok(received) => {
                let OAuthCallbackKind::Code(code) = received.kind else {
                    let _ = received.browser_result.send(OAuthBrowserResult::Failure);
                    let _ = client.join();
                    continue;
                };
                let _ = received.browser_result.send(OAuthBrowserResult::Success);
                let _ = client.join();
                return Ok(code);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                client
                    .join()
                    .map_err(|_| anyhow::anyhow!("OAuth test client panicked"))??;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("OAuth callback listener disconnected")
            }
        }
    }
    anyhow::bail!("receive OAuth callback in test")
}

fn oauth_callback_host(oauth: &OAuthFlow) -> Result<String> {
    let host = std::env::var("DEXT_OAUTH_CALLBACK_HOST")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| oauth.callback_host.clone())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let address = host
        .parse::<std::net::IpAddr>()
        .with_context(|| format!("OAuth callback host must be a loopback address, got '{host}'"))?;
    if !address.is_loopback() {
        anyhow::bail!("OAuth callback host must be loopback-only, got '{host}'");
    }
    Ok(address.to_string())
}

fn parse_windows_oauth_unavailable_ports(output: &str) -> Vec<(u16, u16)> {
    output
        .lines()
        .take(2048)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let start = fields.next()?.parse::<u16>().ok()?;
            let end = fields.next()?.parse::<u16>().ok()?;
            if start == 0
                || start > end
                || fields.next().is_some_and(|field| field != "*")
                || fields.next().is_some()
            {
                return None;
            }
            Some((start, end))
        })
        .collect()
}

fn wsl_oauth_unavailable_ports(host: &str) -> Vec<(u16, u16)> {
    if !running_in_wsl() {
        return Vec::new();
    }
    let family = if host.contains(':') { "ipv6" } else { "ipv4" };
    let script = format!(
        "$ErrorActionPreference='Stop'; netsh.exe interface {family} show excludedportrange protocol=tcp; \
         if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}; \
         Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue | \
         ForEach-Object {{ '{{0}} {{0}}' -f $_.LocalPort }}; exit 0"
    );
    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdin(std::process::Stdio::null());
    // WSL can bind Windows-reserved/occupied ports that localhost forwarding cannot use.
    let Ok(output) = crate::host::run_command_limited(
        command,
        "WSL OAuth port availability",
        Duration::from_secs(5),
    ) else {
        return Vec::new();
    };
    if !output.success() || output.stdout.len() > 64 * 1024 {
        return Vec::new();
    }
    parse_windows_oauth_unavailable_ports(&String::from_utf8_lossy(&output.stdout))
}

fn oauth_allows_dynamic_callback(oauth: &OAuthFlow) -> bool {
    oauth.protocol == OAuthProtocol::AnthropicClaude
        && oauth.authorize_url == "https://claude.ai/oauth/authorize"
        && oauth.client_id == "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
}

fn bind_oauth_callback(
    host: &str,
    redirect_uri: &str,
    allow_dynamic_port: bool,
    unavailable_ports: &[(u16, u16)],
) -> Result<(std::net::TcpListener, String)> {
    let preferred_port = oauth_callback_port(redirect_uri)?;
    let unavailable = |port| {
        unavailable_ports
            .iter()
            .any(|&(start, end)| start <= port && port <= end)
    };
    let preferred = if unavailable(preferred_port) {
        Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "port is reserved or occupied on Windows and unavailable to WSL localhost forwarding",
        ))
    } else {
        std::net::TcpListener::bind(oauth_bind_address(host, preferred_port))
    };
    let preferred_error = match preferred {
        Ok(listener) => return Ok((listener, redirect_uri.to_string())),
        Err(error) if !allow_dynamic_port => return Err(error.into()),
        Err(error) => error,
    };
    // Only the reviewed Anthropic client permits variable loopback redirect ports.
    for _ in 0..32 {
        let listener = std::net::TcpListener::bind(oauth_bind_address(host, 0))?;
        let port = listener.local_addr()?.port();
        if unavailable(port) {
            continue;
        }
        let mut redirect = reqwest::Url::parse(redirect_uri)?;
        redirect
            .set_port(Some(port))
            .map_err(|()| anyhow::anyhow!("set OAuth callback port"))?;
        return Ok((listener, redirect.to_string()));
    }
    anyhow::bail!(
        "preferred port {preferred_port} unavailable: {preferred_error}; no usable loopback port after 32 attempts"
    )
}

fn oauth_bind_address(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn oauth_callback_port(redirect_uri: &str) -> Result<u16> {
    let url = reqwest::Url::parse(redirect_uri)
        .with_context(|| format!("invalid OAuth redirect URI '{redirect_uri}'"))?;
    if url.scheme() != "http"
        || url.host_str() != Some("localhost")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        anyhow::bail!(
            "OAuth redirect URI must use plain HTTP localhost without credentials or a fragment, got '{redirect_uri}'"
        );
    }
    url.port_or_known_default()
        .filter(|port| *port != 0)
        .ok_or_else(|| {
            anyhow::anyhow!("OAuth redirect URI must have a nonzero port: {redirect_uri}")
        })
}

pub fn oauth_originator() -> String {
    std::env::var("DEXT_OAUTH_ORIGINATOR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "dext".to_string())
}

fn build_oauth_authorize_url(
    oauth: &OAuthFlow,
    profile: &ProviderProfile,
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
) -> Result<String> {
    let mut url = reqwest::Url::parse(&oauth.authorize_url)
        .with_context(|| format!("invalid OAuth authorize URL '{}'", oauth.authorize_url))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", &oauth.client_id);
        query.append_pair("redirect_uri", redirect_uri);
        query.append_pair("scope", &oauth.scope);
        query.append_pair("code_challenge", code_challenge);
        query.append_pair("code_challenge_method", "S256");
        query.append_pair("state", state);
        if !oauth.audience.trim().is_empty() {
            query.append_pair("audience", &oauth.audience);
        }
        if oauth.protocol == OAuthProtocol::AnthropicClaude {
            query.append_pair("code", "true");
        }
        if profile.id == "chatgpt" {
            query.append_pair("id_token_add_organizations", "true");
            query.append_pair("codex_cli_simplified_flow", "true");
            let originator = oauth_originator();
            query.append_pair("originator", &originator);
        }
    }
    Ok(url.to_string())
}

fn oauth_exchange_failure_result(
    profile_id: &str,
    manual_hint: &str,
    error: &anyhow::Error,
) -> LoginResult {
    LoginResult {
        message: format!(
            "OAuth token exchange failed: {error:#}\n\n{manual_hint}\nYou can paste the callback URL or authorization code into dext to retry without restarting /login."
        ),
        provider_id: profile_id.to_string(),
        awaiting_credentials: true,
    }
}

fn run_oauth_login(oauth: &OAuthFlow, profile: &ProviderProfile) -> Result<LoginResult> {
    let code_verifier = generate_code_verifier()?;
    let code_challenge = pkce_code_challenge(&code_verifier);
    let state = if oauth.protocol == OAuthProtocol::AnthropicClaude {
        code_verifier.clone()
    } else {
        generate_oauth_state()?
    };

    let preferred_redirect_uri = oauth
        .redirect_uri
        .as_deref()
        .unwrap_or("http://localhost:1455/auth/callback");
    let callback_host = oauth_callback_host(oauth)?;
    let preferred_port = oauth_callback_port(preferred_redirect_uri)?;
    let callback_path = reqwest::Url::parse(preferred_redirect_uri)
        .context("parse OAuth callback URI")?
        .path()
        .to_string();
    let allow_dynamic_port = oauth_allows_dynamic_callback(oauth);
    let unavailable_ports = if allow_dynamic_port {
        wsl_oauth_unavailable_ports(&callback_host)
    } else {
        Vec::new()
    };
    let (rx2, redirect_uri, listener_notice) = match bind_oauth_callback(
        &callback_host,
        preferred_redirect_uri,
        allow_dynamic_port,
        &unavailable_ports,
    ) {
        Ok((listener, redirect_uri)) => {
            let selected_port = listener.local_addr()?.port();
            let notice = (selected_port != preferred_port).then(|| format!(
                "[info] preferred OAuth callback port {preferred_port} is unavailable; using loopback port {selected_port} for this login."
            ));
            (
                Some(spawn_oauth_code_listener(
                    listener,
                    state.clone(),
                    callback_path.clone(),
                )?),
                redirect_uri,
                notice,
            )
        }
        Err(e) => (
            None,
            preferred_redirect_uri.to_string(),
            Some(format!(
                "[warn] could not bind OAuth callback listener on {}: {e}. You must paste the callback URL or authorization code manually.",
                oauth_bind_address(&callback_host, preferred_port)
            )),
        ),
    };
    let callback_port = oauth_callback_port(&redirect_uri)?;
    let authorize_url =
        build_oauth_authorize_url(oauth, profile, &redirect_uri, &state, &code_challenge)?;

    save_pending_oauth(&profile.id, &code_verifier, &state, &redirect_uri)?;

    let manual_code_description = if oauth.protocol == OAuthProtocol::AnthropicClaude {
        "the authorization code"
    } else {
        "the authorization code (starts with `ac_`)"
    };
    let manual_hint = format!(
        "If the browser callback doesn't auto-complete, paste the callback URL \
(http://localhost:{callback_port}{callback_path}?code=...) or just {manual_code_description} \
directly into dext. /login cancel aborts."
    );
    let listener_suffix = listener_notice
        .as_ref()
        .map(|notice| format!("\n{notice}"))
        .unwrap_or_default();

    match open_url_in_browser(&authorize_url) {
        Ok(msg) if msg.starts_with("disabled-by-") => {
            set_active_provider_in_catalog(&profile.id)?;
            let reason = if msg == "disabled-by-wsl" {
                "browser open disabled by DEXT_DISABLE_WSL_BROWSER_OPEN. "
            } else {
                "browser open disabled. "
            };
            return Ok(LoginResult {
                message: format!(
                    "{reason}Open this URL manually:\n{authorize_url}\n\n{manual_hint}{listener_suffix}"
                ),
                provider_id: profile.id.clone(),
                awaiting_credentials: true,
            });
        }
        Ok(_) => {}
        Err(e) => {
            set_active_provider_in_catalog(&profile.id)?;
            return Ok(LoginResult {
                message: format!(
                    "could not open browser for OAuth login: {e:#}\n\nOpen this URL manually:\n{authorize_url}\n\n{manual_hint}{listener_suffix}"
                ),
                provider_id: profile.id.clone(),
                awaiting_credentials: true,
            });
        }
    }

    let Some(rx2) = rx2 else {
        set_active_provider_in_catalog(&profile.id)?;
        return Ok(LoginResult {
            message: format!(
                "browser opened for OAuth login, but callback listener is unavailable.\n\nOpen this URL manually:\n{authorize_url}\n\n{manual_hint}{listener_suffix}"
            ),
            provider_id: profile.id.clone(),
            awaiting_credentials: true,
        });
    };

    let received = match rx2.recv_timeout(std::time::Duration::from_secs(120)) {
        Ok(received) => received,
        Err(_) => {
            set_active_provider_in_catalog(&profile.id)?;
            return Ok(LoginResult {
                message: format!(
                    "OAuth login timed out (120s).\n\nOpen this URL manually:\n{authorize_url}\n\n{manual_hint}{listener_suffix}"
                ),
                provider_id: profile.id.clone(),
                awaiting_credentials: true,
            });
        }
    };

    let kind = received.kind;
    let code = match kind {
        OAuthCallbackKind::Code(code) => code,
        OAuthCallbackKind::Rejected => {
            let _ = received.browser_result.send(OAuthBrowserResult::Failure);
            clear_pending_oauth_if_matches(&profile.id, &code_verifier, &state);
            anyhow::bail!(
                "OAuth authorization was declined for provider '{}'. Start a fresh login when ready.",
                profile.id
            );
        }
    };
    let token_response = exchange_oauth_code(oauth, &code, &code_verifier, &state, &redirect_uri);

    match token_response {
        Ok(tokens) => {
            if !pending_oauth_matches(&profile.id, &code_verifier, &state) {
                let _ = received.browser_result.send(OAuthBrowserResult::Failure);
                anyhow::bail!(
                    "OAuth login was superseded by a newer login attempt. Complete the newest browser flow instead."
                );
            }
            let login = (|| -> Result<LoginResult> {
                set_active_provider_in_catalog(&profile.id)?;
                let mut current_store = load_auth_store()?;
                current_store.providers.insert(
                    canonical_provider_id(&profile.id),
                    StoredCredential::OAuth {
                        access_token: tokens.access_token,
                        refresh_token: tokens.refresh_token,
                        expires_at: tokens.expires_at,
                    },
                );
                save_auth_store(&current_store)?;
                clear_pending_oauth_if_matches(&profile.id, &code_verifier, &state);
                Ok(LoginResult {
                    message: format!(
                        "OAuth login successful for provider '{}'. Credentials stored.{listener_suffix}",
                        profile.id
                    ),
                    provider_id: profile.id.clone(),
                    awaiting_credentials: false,
                })
            })();
            let browser_result = if login.is_ok() {
                OAuthBrowserResult::Success
            } else {
                OAuthBrowserResult::Failure
            };
            let _ = received.browser_result.send(browser_result);
            login
        }
        Err(e) => {
            let _ = received.browser_result.send(OAuthBrowserResult::Failure);
            Ok(oauth_exchange_failure_result(&profile.id, &manual_hint, &e))
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PendingOAuthState {
    provider_id: String,
    code_verifier: String,
    state: String,
    redirect_uri: String,
    created_at: u64,
}

#[cfg(any(test, feature = "test-support"))]
pub fn pending_oauth_path() -> PathBuf {
    dext_state_dir().join("pending_oauth.json")
}

#[cfg(not(any(test, feature = "test-support")))]
fn pending_oauth_path() -> PathBuf {
    dext_state_dir().join("pending_oauth.json")
}

#[cfg(any(test, feature = "test-support"))]
pub fn save_pending_oauth(
    provider_id: &str,
    code_verifier: &str,
    state: &str,
    redirect_uri: &str,
) -> Result<()> {
    let pending = PendingOAuthState {
        provider_id: provider_id.to_string(),
        code_verifier: code_verifier.to_string(),
        state: state.to_string(),
        redirect_uri: redirect_uri.to_string(),
        created_at: unix_timestamp_secs(),
    };
    let json = serde_json::to_string_pretty(&pending)?;
    Ok(atomic_write_secret(&pending_oauth_path(), json.as_bytes())?)
}

#[cfg(not(any(test, feature = "test-support")))]
fn save_pending_oauth(
    provider_id: &str,
    code_verifier: &str,
    state: &str,
    redirect_uri: &str,
) -> Result<()> {
    let pending = PendingOAuthState {
        provider_id: provider_id.to_string(),
        code_verifier: code_verifier.to_string(),
        state: state.to_string(),
        redirect_uri: redirect_uri.to_string(),
        created_at: unix_timestamp_secs(),
    };
    let json = serde_json::to_string_pretty(&pending)?;
    Ok(atomic_write_secret(&pending_oauth_path(), json.as_bytes())?)
}

fn load_pending_oauth() -> Option<PendingOAuthState> {
    let path = pending_oauth_path();
    let data = match read_runtime_state_file(&path, true) {
        Ok(Some(data)) => data,
        Ok(None) => return None,
        Err(_) => {
            remove_pending_oauth_file(&path);
            return None;
        }
    };
    let state: PendingOAuthState = match serde_json::from_str(&data) {
        Ok(state) => state,
        Err(_) => {
            remove_pending_oauth_file(&path);
            return None;
        }
    };
    let age = unix_timestamp_secs().saturating_sub(state.created_at);
    if age > 600 {
        remove_pending_oauth_file(&path);
        return None;
    }
    Some(state)
}

fn remove_pending_oauth_file(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(_) => {}
    }
}

fn clear_pending_oauth() {
    let path = pending_oauth_path();
    remove_pending_oauth_file(&path);
}

fn pending_oauth_matches(provider_id: &str, code_verifier: &str, state: &str) -> bool {
    load_pending_oauth().is_some_and(|pending| {
        canonical_provider_id(&pending.provider_id) == canonical_provider_id(provider_id)
            && pending.code_verifier == code_verifier
            && pending.state == state
    })
}

fn clear_pending_oauth_for_provider(provider_id: &str) {
    if load_pending_oauth().is_some_and(|pending| {
        canonical_provider_id(&pending.provider_id) == canonical_provider_id(provider_id)
    }) {
        clear_pending_oauth();
    }
}

fn clear_pending_oauth_if_matches(provider_id: &str, code_verifier: &str, state: &str) {
    if pending_oauth_matches(provider_id, code_verifier, state) {
        clear_pending_oauth();
    }
}

pub fn cancel_pending_oauth_login() -> bool {
    let had_callback_login = pending_oauth_path().exists();
    clear_pending_oauth();
    had_callback_login
}

#[derive(Debug, Clone)]
struct ParsedOAuthAuthorizationInput {
    code: String,
    state: Option<String>,
    state_required: bool,
}

fn parse_oauth_authorization_input(
    input: &str,
    allow_plain_code_fallback: bool,
    allow_secret_like_plain_code: bool,
) -> Option<ParsedOAuthAuthorizationInput> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(url) = reqwest::Url::parse(trimmed) {
        let code = url
            .query_pairs()
            .find(|(k, value)| k == "code" && !value.trim().is_empty())
            .map(|(_, v)| v.into_owned());
        if let Some(code) = code {
            let state = url
                .query_pairs()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.into_owned());
            return Some(ParsedOAuthAuthorizationInput {
                code,
                state,
                state_required: true,
            });
        }
    }

    if let Some((code, state)) = trimmed.split_once('#')
        && !trimmed.contains("://")
        && !trimmed.contains(char::is_whitespace)
    {
        let code = code.trim();
        let state = state.trim();
        if !code.is_empty() {
            return Some(ParsedOAuthAuthorizationInput {
                code: code.to_string(),
                state: if state.is_empty() {
                    None
                } else {
                    Some(state.to_string())
                },
                state_required: true,
            });
        }
    }

    let query_like = trimmed
        .strip_prefix('?')
        .or_else(|| trimmed.split_once('?').map(|(_, q)| q))
        .unwrap_or(trimmed);
    if query_like.contains("code=")
        && let Ok(url) =
            reqwest::Url::parse(&format!("http://localhost/auth/callback?{query_like}"))
    {
        let code = url
            .query_pairs()
            .find(|(k, value)| k == "code" && !value.trim().is_empty())
            .map(|(_, v)| v.into_owned());
        if let Some(code) = code {
            let state = url
                .query_pairs()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.into_owned());
            return Some(ParsedOAuthAuthorizationInput {
                code,
                state,
                state_required: true,
            });
        }
    }

    let plain_code_allowed = if allow_plain_code_fallback {
        !trimmed.contains(char::is_whitespace)
            && !trimmed.contains("://")
            && !trimmed.starts_with('{')
            && !trimmed.starts_with("sk-ant-api")
            && (allow_secret_like_plain_code
                || (!trimmed.starts_with("sk-") && !trimmed.starts_with("eyJ")))
            && trimmed.len() >= 12
            && chatgpt_account_id_from_token(trimmed).is_none()
    } else {
        trimmed.starts_with("ac_")
            && !trimmed.contains(char::is_whitespace)
            && chatgpt_account_id_from_token(trimmed).is_none()
    };

    if plain_code_allowed {
        return Some(ParsedOAuthAuthorizationInput {
            code: trimmed.to_string(),
            state: None,
            state_required: false,
        });
    }

    None
}

pub fn extract_oauth_code_from_callback(input: &str) -> Option<String> {
    parse_oauth_authorization_input(input, false, false).map(|parsed| parsed.code)
}

fn validate_pending_oauth_callback(
    input: &str,
    expected_provider: Option<&str>,
) -> Result<Option<(ParsedOAuthAuthorizationInput, PendingOAuthState)>> {
    let structured = parse_oauth_authorization_input(input, false, false);
    let pending = load_pending_oauth();
    let allow_plain_code = pending.is_some()
        || expected_provider.is_some_and(|provider| canonical_provider_id(provider) != "anthropic");
    let allow_secret_like_plain_code = expected_provider
        .map(canonical_provider_id)
        .or_else(|| {
            pending
                .as_ref()
                .map(|pending| canonical_provider_id(&pending.provider_id))
        })
        .as_deref()
        == Some("anthropic");
    let parsed = structured.or_else(|| {
        allow_plain_code
            .then(|| parse_oauth_authorization_input(input, true, allow_secret_like_plain_code))
            .flatten()
    });
    let Some(parsed) = parsed else {
        return Ok(None);
    };
    let pending = match pending {
        Some(pending) => pending,
        None => anyhow::bail!(
            "received OAuth callback code but no pending OAuth session found. Start a fresh login with /login <provider> web"
        ),
    };

    if let Some(expected_provider) = expected_provider
        && canonical_provider_id(expected_provider) != canonical_provider_id(&pending.provider_id)
    {
        anyhow::bail!(
            "OAuth callback belongs to pending provider '{}', not '{}'. Complete or cancel the pending login first.",
            pending.provider_id,
            expected_provider
        );
    }

    if parsed.state_required && parsed.state.is_none() {
        anyhow::bail!(
            "OAuth callback is missing state. Paste the complete callback URL or just the authorization code, or start a fresh login with /login {} web",
            pending.provider_id
        );
    }
    if let Some(returned_state) = parsed.state.as_deref()
        && returned_state != pending.state
    {
        anyhow::bail!(
            "OAuth state mismatch. Start a fresh login with /login {} web",
            pending.provider_id
        );
    }
    Ok(Some((parsed, pending)))
}

pub fn try_complete_oauth_from_callback(
    input: &str,
    expected_provider: Option<&str>,
) -> Result<Option<String>> {
    let Some((parsed, pending)) = validate_pending_oauth_callback(input, expected_provider)? else {
        return Ok(None);
    };

    let catalog = load_provider_catalog()?;
    let profile = find_provider_profile(&catalog, &pending.provider_id).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown provider '{}' for pending OAuth",
            pending.provider_id
        )
    })?;

    let oauth = profile
        .oauth_flow
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("provider '{}' has no OAuth config", pending.provider_id))?;

    let tokens = exchange_oauth_code(
        oauth,
        &parsed.code,
        &pending.code_verifier,
        &pending.state,
        &pending.redirect_uri,
    )?;
    if !pending_oauth_matches(&pending.provider_id, &pending.code_verifier, &pending.state) {
        anyhow::bail!(
            "OAuth login was superseded by a newer login attempt. Complete the newest browser flow instead."
        );
    }

    set_active_provider_in_catalog(&pending.provider_id)?;

    let mut store = load_auth_store()?;
    store.providers.insert(
        canonical_provider_id(&pending.provider_id),
        StoredCredential::OAuth {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: tokens.expires_at,
        },
    );
    save_auth_store(&store)?;
    clear_pending_oauth_if_matches(&pending.provider_id, &pending.code_verifier, &pending.state);

    Ok(Some(format!(
        "OAuth login successful for provider '{}'. Credentials stored.",
        pending.provider_id
    )))
}

#[derive(Debug)]
struct ExchangedTokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<u64>,
}

fn oauth_expires_at_from_response(body: &Value, refresh_early_secs: u64) -> Option<u64> {
    let expires_in = body.get("expires_in").and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
            .filter(|seconds| *seconds > 0)
    });
    expires_in.map(|seconds| {
        unix_timestamp_secs().saturating_add(seconds.saturating_sub(refresh_early_secs))
    })
}

fn kimi_device_id_path() -> PathBuf {
    dext_state_dir().join("kimi_device_id")
}

fn kimi_device_id_is_valid(value: &str) -> bool {
    (8..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn kimi_device_id() -> Result<String> {
    let path = kimi_device_id_path();
    if let Ok(value) = std::fs::read_to_string(&path) {
        let value = value.trim();
        if kimi_device_id_is_valid(value) {
            return Ok(value.to_string());
        }
    }

    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).context("failed to generate Kimi device id")?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let value = format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    );
    let _ = atomic_write_secret(&path, value.as_bytes());
    Ok(value)
}

fn kimi_ascii_header(value: &str) -> String {
    let value = value
        .chars()
        .filter(|character| character.is_ascii() && !character.is_ascii_control())
        .collect::<String>();
    let value = value.trim();
    if value.is_empty() {
        "unknown".to_string()
    } else {
        value.to_string()
    }
}

fn kimi_device_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .map(|value| kimi_ascii_header(&value))
        .unwrap_or_else(|| "unknown".to_string())
}

fn oauth_http_client() -> Result<reqwest::blocking::Client> {
    provider_blocking_http_client_builder()
        .connect_timeout(OAUTH_CONNECT_TIMEOUT)
        .timeout(OAUTH_REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build OAuth HTTP client")
}

fn read_oauth_json_response(mut response: reqwest::blocking::Response) -> Result<Value> {
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(OAUTH_RESPONSE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read OAuth response")?;
    if bytes.len() as u64 > OAUTH_RESPONSE_MAX_BYTES {
        anyhow::bail!("OAuth response exceeded the {OAUTH_RESPONSE_MAX_BYTES} byte limit");
    }
    serde_json::from_slice(&bytes).context("OAuth response was not valid JSON")
}

fn exchange_oauth_code(
    oauth: &OAuthFlow,
    code: &str,
    code_verifier: &str,
    state: &str,
    redirect_uri: &str,
) -> Result<ExchangedTokens> {
    let client = oauth_http_client()?;
    let mut request = client.post(&oauth.token_url);
    if oauth.protocol == OAuthProtocol::AnthropicClaude {
        request = request.json(&json!({
            "grant_type": "authorization_code",
            "code": code,
            "client_id": oauth.client_id,
            "code_verifier": code_verifier,
            "state": state,
            "redirect_uri": redirect_uri,
        }));
    } else {
        request = request.form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", oauth.client_id.as_str()),
            ("code_verifier", code_verifier),
            ("redirect_uri", redirect_uri),
        ]);
    }
    let resp = request.send().context("token exchange request failed")?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("token exchange failed ({status}): provider rejected the OAuth exchange");
    }
    let body = read_oauth_json_response(resp).context("token exchange response was invalid")?;

    let access_token = body
        .get("access_token")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("no access_token in OAuth response"))?;
    let refresh_token = body
        .get("refresh_token")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    let expires_at = oauth_expires_at_from_response(
        &body,
        if oauth.protocol == OAuthProtocol::AnthropicClaude {
            300
        } else {
            0
        },
    );
    if oauth.protocol == OAuthProtocol::AnthropicClaude {
        if refresh_token.is_none() {
            anyhow::bail!("no refresh_token in Anthropic OAuth response");
        }
        if expires_at.is_none() {
            anyhow::bail!("no valid expires_in in Anthropic OAuth response");
        }
    }

    Ok(ExchangedTokens {
        access_token,
        refresh_token,
        expires_at,
    })
}

fn exchange_oauth_refresh_token(oauth: &OAuthFlow, refresh_token: &str) -> Result<ExchangedTokens> {
    let client = oauth_http_client()?;
    let mut request = client.post(&oauth.token_url);
    if oauth.protocol == OAuthProtocol::AnthropicClaude {
        request = request.json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": oauth.client_id,
        }));
    } else {
        request = request.form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", oauth.client_id.as_str()),
        ]);
    }
    let resp = request.send().context("token refresh request failed")?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("token refresh failed ({status}): provider rejected the OAuth refresh");
    }
    let body = read_oauth_json_response(resp).context("token refresh response was invalid")?;

    let access_token = body
        .get("access_token")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("no access_token in OAuth refresh response"))?;
    let next_refresh = body
        .get("refresh_token")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .or_else(|| Some(refresh_token.to_string()));
    let expires_at = oauth_expires_at_from_response(
        &body,
        if oauth.protocol == OAuthProtocol::AnthropicClaude {
            300
        } else {
            0
        },
    );
    if oauth.protocol == OAuthProtocol::AnthropicClaude && expires_at.is_none() {
        anyhow::bail!("no valid expires_in in Anthropic OAuth refresh response");
    }

    Ok(ExchangedTokens {
        access_token,
        refresh_token: next_refresh,
        expires_at,
    })
}

fn refresh_oauth_credential_if_needed(
    profile: &ProviderProfile,
    store: &mut AuthStore,
) -> Result<Option<String>> {
    let canonical = canonical_provider_id(&profile.id);
    if canonical == "kimi" {
        return Ok(None);
    }
    let Some(StoredCredential::OAuth {
        access_token,
        refresh_token,
        expires_at,
    }) = store
        .providers
        .get(&profile.id)
        .or_else(|| store.providers.get(&canonical))
        .cloned()
    else {
        return Ok(None);
    };

    let Some(expires_at) = expires_at else {
        return Ok(None);
    };
    let now = unix_timestamp_secs();
    if expires_at == 0 || now.saturating_add(60) < expires_at {
        return Ok(None);
    }

    let Some(refresh_token) = refresh_token else {
        if now < expires_at {
            return Ok(None);
        }
        anyhow::bail!(
            "OAuth access token for provider '{}' is expired and has no refresh token. Re-run /login {} web.",
            profile.id,
            profile.id
        );
    };
    let refreshed = {
        let oauth = profile.oauth_flow.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "provider '{}' has OAuth credential but no OAuth config",
                profile.id
            )
        })?;
        match exchange_oauth_refresh_token(oauth, &refresh_token) {
            Ok(refreshed) => refreshed,
            Err(error) => {
                let current_store = load_auth_store()?;
                let credential_changed = current_store
                    .providers
                    .get(&profile.id)
                    .or_else(|| current_store.providers.get(&canonical))
                    .is_none_or(|credential| {
                        !matches!(
                            credential,
                            StoredCredential::OAuth {
                                access_token: current_access,
                                refresh_token: Some(current_refresh),
                                expires_at: Some(current_expires),
                            } if current_access == &access_token
                                && current_refresh == &refresh_token
                                && *current_expires == expires_at
                        )
                    });
                if credential_changed {
                    *store = current_store;
                    return Ok(None);
                }
                return Err(error);
            }
        }
    };

    let mut current_store = load_auth_store()?;
    let credential_is_current = current_store
        .providers
        .get(&profile.id)
        .or_else(|| current_store.providers.get(&canonical))
        .is_some_and(|credential| {
            matches!(
                credential,
                StoredCredential::OAuth {
                    access_token: current_access,
                    refresh_token: Some(current_refresh),
                    expires_at: Some(current_expires),
                } if current_access == &access_token
                    && current_refresh == &refresh_token
                    && *current_expires == expires_at
            )
        });
    if !credential_is_current {
        *store = current_store;
        return Ok(None);
    }

    let token = refreshed.access_token.clone();
    current_store.providers.insert(
        canonical,
        StoredCredential::OAuth {
            access_token: refreshed.access_token,
            refresh_token: refreshed.refresh_token,
            expires_at: refreshed.expires_at,
        },
    );
    save_auth_store(&current_store)?;
    *store = current_store;
    Ok(Some(token))
}

pub fn login_provider(
    selected: Option<&str>,
    key_from_arg: Option<&str>,
    allow_prompt: bool,
) -> Result<LoginResult> {
    // A login is an explicit retry signal: drop cached `!command` secret
    // results (including cached failures) so a fixed external store —
    // e.g. an unlocked keychain — is picked up without restarting dext.
    if let Ok(mut cache) = command_secret_cache().lock() {
        cache.clear();
    }
    let mut catalog = load_provider_catalog()?;
    let provider_id = match selected {
        Some(sel) => provider_id_from_selector(&catalog, sel)?,
        None => resolve_active_provider_id(&catalog),
    };
    let profile = find_provider_profile(&catalog, &provider_id)
        .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?;

    let mut store = load_auth_store()?;
    let explicit_web = key_from_arg.is_some_and(login_arg_requests_web_flow);
    let explicit_import = key_from_arg.is_some_and(login_arg_requests_import);
    let anthropic_subscription_login = profile
        .oauth_flow
        .as_ref()
        .is_some_and(|oauth| oauth.protocol == OAuthProtocol::AnthropicClaude);
    let mut key = if explicit_web || explicit_import {
        String::new()
    } else {
        key_from_arg
            .and_then(normalize_login_secret)
            .unwrap_or_default()
    };

    if key.trim().is_empty() {
        if !profile.requires_api_key {
            catalog.active_provider = profile.id.clone();
            save_provider_catalog(&catalog)?;
            return Ok(LoginResult {
                message: format!("provider '{}' selected. No API key required.", profile.id),
                provider_id: profile.id.clone(),
                awaiting_credentials: false,
            });
        }

        if explicit_web
            && !anthropic_subscription_login
            && store
                .providers
                .remove(&canonical_provider_id(&profile.id))
                .is_some()
        {
            save_auth_store(&store)?;
        }

        if !explicit_web && anthropic_subscription_login {
            let canonical = canonical_provider_id(&profile.id);
            if let Some(entry @ StoredCredential::OAuth { .. }) = store
                .providers
                .get(&profile.id)
                .or_else(|| store.providers.get(&canonical))
                && entry.resolve_secret().is_some()
            {
                clear_pending_oauth_for_provider(&profile.id);
                catalog.active_provider = profile.id.clone();
                save_provider_catalog(&catalog)?;
                return Ok(LoginResult {
                    message: format!(
                        "provider '{}' already authenticated via subscription OAuth and set active",
                        profile.id
                    ),
                    provider_id: profile.id.clone(),
                    awaiting_credentials: false,
                });
            }
        }

        if !explicit_web
            && !anthropic_subscription_login
            && let Some((secret, source)) = resolve_provider_api_key(&profile, &store)
        {
            catalog.active_provider = profile.id.clone();
            save_provider_catalog(&catalog)?;
            if source.starts_with("auth:") {
                return Ok(LoginResult {
                    message: format!(
                        "provider '{}' already authenticated via {} and set active",
                        profile.id, source
                    ),
                    provider_id: profile.id.clone(),
                    awaiting_credentials: false,
                });
            }

            if source.starts_with("env:") {
                store.providers.insert(
                    canonical_provider_id(&profile.id),
                    StoredCredential::ApiKey { key: secret },
                );
                save_auth_store(&store)?;
                clear_pending_oauth_for_provider(&profile.id);
                return Ok(LoginResult {
                    message: format!(
                        "imported credentials for provider '{}' from {} into {} and set it active",
                        profile.id,
                        source,
                        auth_store_path().display()
                    ),
                    provider_id: profile.id.clone(),
                    awaiting_credentials: false,
                });
            }
        }

        if explicit_import {
            if let Some(source) = import_provider_credential_from_external(&profile, &mut store) {
                save_auth_store(&store)?;
                clear_pending_oauth_for_provider(&profile.id);
                catalog.active_provider = profile.id.clone();
                save_provider_catalog(&catalog)?;
                return Ok(LoginResult {
                    message: format!(
                        "imported credentials for provider '{}' from {} into {} and set it active",
                        profile.id,
                        source,
                        auth_store_path().display()
                    ),
                    provider_id: profile.id.clone(),
                    awaiting_credentials: false,
                });
            }
            anyhow::bail!(
                "no reusable external credential found for provider '{}'. Run `dext auth login {} web` or paste a token/key directly.",
                profile.id,
                profile.id
            );
        }

        if let Some(oauth) = &profile.oauth_flow {
            return run_oauth_login(oauth, &profile);
        }

        let mut login_urls: Vec<String> = Vec::new();
        if let Some(url) = &profile.login_url {
            login_urls.push(url.clone());
        }

        let mut opened_any_url = false;
        let mut launch_warnings: Vec<String> = Vec::new();
        for url in &login_urls {
            match open_url_in_browser(url) {
                Ok(_) => opened_any_url = true,
                Err(e) => launch_warnings.push(format!("could not auto-open {url}: {e:#}")),
            }
        }

        if allow_prompt {
            let prompt = if request_contract_for_profile(&profile)
                == RequestContract::ChatGptResponses
            {
                "paste ChatGPT access token (or full JSON session blob) now, or press Enter to skip: "
            } else {
                "paste API key/token now, or press Enter to skip: "
            };
            let entered = prompt_input_line(prompt)?;
            if let Some(secret) = normalize_login_secret(&entered) {
                key = secret;
            }
        }

        if key.trim().is_empty() {
            catalog.active_provider = profile.id.clone();
            save_provider_catalog(&catalog)?;

            let mut msg = if request_contract_for_profile(&profile)
                == RequestContract::ChatGptResponses
            {
                "opened ChatGPT in your browser. sign in if needed, then paste the access token or full session JSON directly into dext.".to_string()
            } else {
                format!(
                    "opened login page for provider '{}'. paste the credential directly into dext when ready.",
                    profile.id
                )
            };
            if !opened_any_url && !login_urls.is_empty() {
                msg.push_str("\nopen this URL:");
                for url in &login_urls {
                    msg.push_str(&format!("\n- {url}"));
                }
            }
            for warn in launch_warnings {
                msg.push_str(&format!("\n[warn] {warn}"));
            }
            if request_contract_for_profile(&profile) != RequestContract::ChatGptResponses
                && let Some(notes) = &profile.notes
            {
                msg.push_str(&format!("\n{notes}"));
            }
            return Ok(LoginResult {
                message: msg,
                provider_id: profile.id.clone(),
                awaiting_credentials: true,
            });
        }
    }

    // Only treat pasted input as an OAuth artifact when this provider actually
    // uses OAuth, or the input is unambiguously a callback (URL / `ac_` code).
    // Plain API keys for non-OAuth providers (e.g. ZAI GLM `id.secret` keys)
    // would otherwise match the plain-code fallback and fail login with a
    // bogus "no pending OAuth session" error instead of being stored.
    if (profile.oauth_flow.is_some() || extract_oauth_code_from_callback(&key).is_some())
        && let Some(msg) = try_complete_oauth_from_callback(&key, Some(&profile.id))?
    {
        catalog.active_provider = profile.id.clone();
        save_provider_catalog(&catalog)?;
        return Ok(LoginResult {
            message: msg,
            provider_id: profile.id.clone(),
            awaiting_credentials: false,
        });
    }

    if profile.oauth_flow.is_some() && (key.contains("://") || key.contains("code=")) {
        anyhow::bail!(
            "malformed OAuth callback. Paste the complete callback URL, including its non-empty code and state, or paste just the authorization code."
        );
    }

    validate_login_secret_for_provider(&profile, &key)?;
    store.providers.insert(
        canonical_provider_id(&profile.id),
        StoredCredential::ApiKey { key },
    );
    save_auth_store(&store)?;
    clear_pending_oauth_for_provider(&profile.id);

    catalog.active_provider = profile.id.clone();
    save_provider_catalog(&catalog)?;

    Ok(LoginResult {
        message: format!(
            "stored credentials for provider '{}' in {} and set it active",
            profile.id,
            auth_store_path().display()
        ),
        provider_id: profile.id.clone(),
        awaiting_credentials: false,
    })
}

#[cfg(any(test, feature = "test-support"))]
pub fn oauth_exchange_failure_result_message(profile_id: &str, manual_hint: &str) -> String {
    oauth_exchange_failure_result(
        profile_id,
        manual_hint,
        &anyhow::anyhow!("token exchange failed (400 Bad Request): unknown error"),
    )
    .message
}

#[cfg(any(test, feature = "test-support"))]
pub fn login_provider_with_key(
    selected: Option<&str>,
    key_from_arg: Option<&str>,
    allow_prompt: bool,
) -> Result<String> {
    Ok(login_provider(selected, key_from_arg, allow_prompt)?.message)
}

pub fn logout_provider(selected: Option<&str>) -> Result<String> {
    let catalog = load_provider_catalog()?;
    let provider_id = match selected {
        Some(sel) => provider_id_from_selector(&catalog, sel)?,
        None => resolve_active_provider_id(&catalog),
    };
    let canonical = canonical_provider_id(&provider_id);

    let mut store = load_auth_store()?;
    let removed = store.providers.remove(&canonical).is_some();
    save_auth_store(&store)?;

    Ok(if removed {
        format!("removed stored credentials for provider '{canonical}'")
    } else {
        format!("no stored credentials for provider '{canonical}'")
    })
}

fn render_provider_models(profile: &ProviderProfile) -> String {
    let models = curated_provider_models(profile);
    let mut out = if models.is_empty() {
        format!(
            "provider '{}' default model: {} (no curated model list configured)",
            profile.id, profile.default_model
        )
    } else {
        format!(
            "provider '{}' models:\n{}",
            profile.id,
            models
                .iter()
                .map(|m| format!("- {m}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    if !profile.model_aliases.is_empty() {
        let mut aliases = profile.model_aliases.iter().collect::<Vec<_>>();
        aliases.sort_by_key(|(alias, _)| *alias);
        out.push_str("\naliases:\n");
        out.push_str(
            &aliases
                .into_iter()
                .map(|(alias, target)| format!("- {alias} -> {target}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if !profile.requires_api_key {
        out.push_str(
            "\nnote: no credentials required; the local/server endpoint must already be running.",
        );
    }
    if let Some(notes) = &profile.notes {
        out.push('\n');
        out.push_str(notes);
    }
    if request_contract_for_profile(profile) == RequestContract::ChatGptResponses {
        out.push_str(
            "\nnote: this is a local curated list, not your full account entitlement list.\nset any model slug directly with /model <slug>.",
        );
    }
    out
}

fn prefix_first_line(prefix: &str, text: &str) -> String {
    if let Some((first, rest)) = text.split_once('\n') {
        format!("{prefix}{first}\n{rest}")
    } else {
        format!("{prefix}{text}")
    }
}

pub fn list_models_for_provider(catalog: &ProviderCatalog, provider_id: &str) -> Result<String> {
    let profile = find_provider_profile(catalog, provider_id)
        .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?;
    Ok(render_provider_models(&profile))
}

pub fn list_models_for_available_providers(
    catalog: &ProviderCatalog,
    store: &AuthStore,
    active_provider: &str,
) -> Result<String> {
    let active_provider = canonical_provider_id(active_provider);
    let mut sections = Vec::new();
    for profile in &catalog.providers {
        if !provider_has_available_credentials(profile, store) {
            continue;
        }
        let marker = if canonical_provider_id(&profile.id) == active_provider {
            "* "
        } else {
            "  "
        };
        sections.push(prefix_first_line(marker, &render_provider_models(profile)));
    }
    if sections.is_empty() {
        let profile = find_provider_profile(catalog, &active_provider)
            .ok_or_else(|| anyhow::anyhow!("unknown provider '{active_provider}'"))?;
        return Ok(format!(
            "no authenticated providers found; showing active provider.\n{}",
            prefix_first_line("* ", &render_provider_models(&profile))
        ));
    }
    Ok(sections.join("\n\n"))
}

const AUTH_LOGIN_ARGUMENTS: &str = "[credential|web|import]";

const STDIN_CREDENTIAL_MAX_BYTES: usize = 64 * 1024;

/// First line of a piped stdin, trimmed; `None` when stdin is empty. Bounded so
/// a runaway pipe cannot grow memory; a JSON blob (service-account style) is
/// accepted on one line only.
pub fn read_credential_from_stdin(input: &mut impl io::BufRead) -> Result<Option<String>> {
    use io::BufRead as _;
    let mut line = String::new();
    let mut limited = input.take(STDIN_CREDENTIAL_MAX_BYTES as u64 + 1);
    limited.read_line(&mut line)?;
    if line.len() > STDIN_CREDENTIAL_MAX_BYTES {
        anyhow::bail!("credential on stdin exceeds the {STDIN_CREDENTIAL_MAX_BYTES} byte limit");
    }
    let trimmed = line.trim();
    Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
}

fn provider_json(profile: &ProviderProfile, store: &AuthStore, active: &str) -> Value {
    let contract = request_contract_for_profile(profile);
    json!({
        "id": profile.id,
        "label": if profile.display_name.trim().is_empty() { profile.id.clone() } else { profile.display_name.clone() },
        "active": canonical_provider_id(&profile.id) == canonical_provider_id(active),
        "default_model": profile.default_model,
        "auth": provider_auth_status(profile, store),
        "available": provider_has_available_credentials(profile, store),
        "contract": contract.as_str(),
        "api": contract.api_provider().as_str(),
        "base_url": profile.base_url,
        "requires_api_key": profile.requires_api_key,
    })
}

/// `dext auth status --json`: stable document for hosts; prose output stays
/// free to change without breaking them.
pub fn auth_status_json(catalog: &ProviderCatalog, store: &AuthStore, active: &str) -> String {
    let doc = json!({
        "version": 1,
        "active_provider": active,
        "providers": catalog.providers.iter().map(|p| provider_json(p, store, active)).collect::<Vec<_>>(),
        "provider_catalog": provider_catalog_path().display().to_string(),
        "auth_store": auth_store_path().display().to_string(),
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
}

/// `dext auth models [provider] --json`: curated models per provider. Without a
/// selector, mirrors the prose command: providers with credentials, falling
/// back to the active provider when none are authenticated.
pub fn auth_models_json(
    catalog: &ProviderCatalog,
    store: &AuthStore,
    active: &str,
    selected: Option<&str>,
) -> String {
    let mut providers: Vec<&ProviderProfile> = match selected {
        Some(id) => catalog
            .providers
            .iter()
            .filter(|p| canonical_provider_id(&p.id) == canonical_provider_id(id))
            .collect(),
        None => catalog
            .providers
            .iter()
            .filter(|p| provider_has_available_credentials(p, store))
            .collect(),
    };
    if providers.is_empty() && selected.is_none() {
        providers = catalog
            .providers
            .iter()
            .filter(|p| canonical_provider_id(&p.id) == canonical_provider_id(active))
            .collect();
    }
    let doc = json!({
        "version": 1,
        "active_provider": active,
        "providers": providers.iter().map(|p| {
            let mut aliases = p.model_aliases.iter().collect::<Vec<_>>();
            aliases.sort_by_key(|(alias, _)| *alias);
            json!({
                "id": p.id,
                "active": canonical_provider_id(&p.id) == canonical_provider_id(active),
                "default_model": p.default_model,
                "models": curated_provider_models(p),
                "aliases": aliases.into_iter().map(|(a, t)| json!({"alias": a, "model": t})).collect::<Vec<_>>(),
                "auth": provider_auth_status(p, store),
                "available": provider_has_available_credentials(p, store),
            })
        }).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
}

fn auth_retry_guidance(provider_id: &str) -> String {
    format!(
        "login for provider '{provider_id}' remains incomplete. retry `dext auth login {provider_id}`; to paste a credential or manual OAuth callback without putting it in shell arguments, open Dext and use `/login {provider_id}`. shell history and process listings may retain command-line secrets"
    )
}

pub fn handle_auth_cli(argv: &[String]) -> Result<Option<i32>> {
    if argv.first().map(String::as_str) != Some("auth") {
        return Ok(None);
    }

    let sub = argv.get(1).map(String::as_str).unwrap_or("status");
    let args: Vec<&str> = argv.iter().skip(2).map(String::as_str).collect();

    match sub {
        "help" | "-h" | "--help" => {
            println!("dext auth commands:");
            println!("  dext auth status");
            println!("  dext auth providers            list configured providers and auth status");
            println!("  dext auth provider [id|index]  show/set active provider");
            println!("  dext auth models [provider|index] list known models for provider");
            println!(
                "  dext auth login [provider|index] {AUTH_LOGIN_ARGUMENTS}   open login flow and store credentials"
            );
            println!(
                "      omit the credential to paste at Dext's prompt; shell arguments may be retained"
            );
            println!(
                "      with stdin piped (not a TTY), the credential is read from stdin (first line) and never touches argv"
            );
            println!("  dext auth logout [provider|index] remove stored credential");
            println!("  --json on status/providers/models prints a machine-readable document");
            Ok(Some(0))
        }
        "status" | "providers" | "list" => {
            let catalog = load_provider_catalog()?;
            let store = load_auth_store()?;
            let active = resolve_active_provider_id(&catalog);
            if args.contains(&"--json") {
                println!("{}", auth_status_json(&catalog, &store, &active));
                return Ok(Some(0));
            }
            println!(
                "active provider: {}\n{}\n\nprovider catalog: {}\nauth store: {}",
                active,
                render_provider_list(&catalog, &store, &active),
                provider_catalog_path().display(),
                auth_store_path().display(),
            );
            Ok(Some(0))
        }
        "provider" | "use" => {
            let catalog = load_provider_catalog()?;
            if args.is_empty() {
                let active = resolve_active_provider_id(&catalog);
                println!("active provider: {active}");
                return Ok(Some(0));
            }
            let target = provider_id_from_selector(&catalog, args[0])?;
            set_active_provider_in_catalog(&target)?;
            println!("active provider -> {target}");
            Ok(Some(0))
        }
        "models" => {
            let catalog = load_provider_catalog()?;
            let store = load_auth_store()?;
            let active = resolve_active_provider_id(&catalog);
            let json = args.contains(&"--json");
            let args: Vec<&str> = args.iter().copied().filter(|a| *a != "--json").collect();
            if json {
                let selected = match args.first().copied() {
                    None | Some("all") => None,
                    Some(sel) => Some(provider_id_from_selector(&catalog, sel)?),
                };
                println!(
                    "{}",
                    auth_models_json(&catalog, &store, &active, selected.as_deref())
                );
                return Ok(Some(0));
            }
            let list = match args.first().copied() {
                None | Some("all") => {
                    list_models_for_available_providers(&catalog, &store, &active)
                }
                Some(sel) => provider_id_from_selector(&catalog, sel)
                    .and_then(|target| list_models_for_provider(&catalog, &target)),
            }?;
            println!("{list}");
            Ok(Some(0))
        }
        "login" => {
            let catalog = load_provider_catalog()?;
            let store = load_auth_store()?;
            let active = resolve_active_provider_id(&catalog);

            if args.is_empty() {
                println!("select provider for login (id or index):");
                println!("{}", render_provider_picker(&catalog, &store, &active));

                if io::stdin().is_terminal() {
                    let selection = prompt_input_line("provider (Enter for active): ")?;
                    let selected = if selection.trim().is_empty() {
                        None
                    } else {
                        Some(selection)
                    };
                    let login = login_provider(selected.as_deref(), None, true)?;
                    println!("{}", login.message);
                    if login.awaiting_credentials {
                        println!("{}", auth_retry_guidance(&login.provider_id));
                    }
                } else {
                    println!("usage: dext auth login <provider|index> {AUTH_LOGIN_ARGUMENTS}");
                }
                return Ok(Some(0));
            }

            let provider = args.first().copied();
            let mut key_buf = if args.len() > 1 {
                Some(args[1..].join(" "))
            } else {
                None
            };
            let allow_prompt = io::stdin().is_terminal();
            // Non-TTY callers (hosts, scripts) pipe the credential instead of
            // placing it on argv, where it is visible in /proc/<pid>/cmdline.
            if key_buf.is_none() && !allow_prompt {
                key_buf = read_credential_from_stdin(&mut io::stdin().lock())?;
            }
            let login = login_provider(provider, key_buf.as_deref(), allow_prompt)?;
            println!("{}", login.message);
            if login.awaiting_credentials {
                println!("{}", auth_retry_guidance(&login.provider_id));
            }
            Ok(Some(0))
        }
        "logout" => {
            let provider = args.first().copied();
            let msg = logout_provider(provider)?;
            println!("{msg}");
            Ok(Some(0))
        }
        other => {
            eprintln!("unknown auth command: {other}. try `dext auth help`");
            Ok(Some(2))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dext-provider-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create temp home");
        path
    }

    #[test]
    fn provider_request_encoding_preserves_plain_boundaries_and_other_contracts() -> Result<()> {
        let interrupt = std::sync::atomic::AtomicBool::new(false);
        let client = reqwest::Client::new();
        for (contract, size) in [
            (RequestContract::ChatGptResponses, 0),
            (
                RequestContract::ChatGptResponses,
                CHATGPT_REQUEST_WIRE_LIMIT,
            ),
            (
                RequestContract::OpenAiResponses,
                CHATGPT_REQUEST_WIRE_LIMIT + 1,
            ),
            (
                RequestContract::OpenAiChatCompletions,
                CHATGPT_REQUEST_WIRE_LIMIT + 1,
            ),
            (
                RequestContract::AnthropicMessages,
                CHATGPT_REQUEST_WIRE_LIMIT + 1,
            ),
        ] {
            let json = vec![b' '; size];
            let body = ProviderRequestBody::encode(contract, json.clone(), &interrupt)?;
            assert!(!body.gzip);
            assert_eq!(body.json_bytes, size);
            assert_eq!(body.bytes.as_ref(), json);
            let request = body
                .apply(client.post("http://localhost/responses"))
                .build()?;
            assert!(
                request
                    .headers()
                    .get(reqwest::header::CONTENT_ENCODING)
                    .is_none()
            );
            assert_eq!(
                request.body().and_then(reqwest::Body::as_bytes),
                Some(json.as_slice())
            );
        }
        Ok(())
    }

    #[test]
    fn provider_request_encoding_gzip_is_lossless_and_reused() -> Result<()> {
        let interrupt = std::sync::atomic::AtomicBool::new(false);
        let mut json = serde_json::to_vec(&json!({"text": "λ\\n\"\\".repeat(180_000)}))?;
        json.resize(json.len().max(CHATGPT_REQUEST_WIRE_LIMIT + 1), b' ');
        let body = ProviderRequestBody::encode(
            RequestContract::ChatGptResponses,
            json.clone(),
            &interrupt,
        )?;
        assert!(body.gzip);
        assert_eq!(body.json_bytes, json.len());
        assert!(body.bytes.len() <= CHATGPT_REQUEST_WIRE_LIMIT);
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(body.bytes.as_ref()).read_to_end(&mut decoded)?;
        assert_eq!(decoded, json);
        let client = reqwest::Client::new();
        for _ in 0..2 {
            let request = body
                .apply(client.post("http://localhost/responses"))
                .build()?;
            assert_eq!(request.headers()[reqwest::header::CONTENT_ENCODING], "gzip");
            assert_eq!(
                request.body().and_then(reqwest::Body::as_bytes),
                Some(body.bytes.as_ref())
            );
        }
        let again =
            ProviderRequestBody::encode(RequestContract::ChatGptResponses, json, &interrupt)?;
        assert_eq!(body.bytes, again.bytes);
        Ok(())
    }

    #[test]
    fn provider_request_encoding_rejects_oversize_gzip_and_cancellation() {
        let interrupt = std::sync::atomic::AtomicBool::new(false);
        let mut seed = 0x42a7_391b_03d8_7ac9_u64;
        let random: Vec<u8> = (0..2 * CHATGPT_REQUEST_WIRE_LIMIT)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        let json = serde_json::to_vec(&json!({
            "input": base64::engine::general_purpose::STANDARD.encode(random)
        }))
        .unwrap();
        let error =
            ProviderRequestBody::encode(RequestContract::ChatGptResponses, json, &interrupt)
                .err()
                .expect("incompressible body refused")
                .to_string();
        assert!(error.contains("wire limit even with gzip"), "{error}");
        assert!(error.contains("No request was sent"), "{error}");
        assert!(error.contains("/compact"), "{error}");
        interrupt.store(true, std::sync::atomic::Ordering::Relaxed);
        for size in [0, CHATGPT_REQUEST_WIRE_LIMIT + 1] {
            let error = ProviderRequestBody::encode(
                RequestContract::ChatGptResponses,
                vec![b' '; size],
                &interrupt,
            )
            .err()
            .expect("cancelled encoding")
            .to_string();
            assert!(error.contains("interrupted by user"), "{error}");
        }
    }

    #[test]
    fn stdin_credential_takes_first_line_trimmed() {
        let mut input = io::Cursor::new("  sk-test-123  \nsecond line\n");
        assert_eq!(
            read_credential_from_stdin(&mut input).expect("read"),
            Some("sk-test-123".to_string())
        );
        let mut empty = io::Cursor::new("\n");
        assert_eq!(read_credential_from_stdin(&mut empty).expect("read"), None);
        let mut eof = io::Cursor::new("");
        assert_eq!(read_credential_from_stdin(&mut eof).expect("read"), None);
        let oversized = "x".repeat(STDIN_CREDENTIAL_MAX_BYTES + 1);
        let mut big = io::Cursor::new(oversized);
        assert!(read_credential_from_stdin(&mut big).is_err());
    }

    #[test]
    fn auth_json_documents_are_stable() {
        // The built-in anthropic profile is the fixture: no hand-rolled struct
        // literal to keep in sync with ProviderProfile's fields.
        let catalog = ProviderCatalog {
            version: PROVIDER_CATALOG_VERSION,
            active_provider: "anthropic".to_string(),
            providers: built_in_provider_profiles()
                .into_iter()
                .filter(|p| p.id == "anthropic")
                .collect(),
        };
        let store = AuthStore::default();
        let status: Value =
            serde_json::from_str(&auth_status_json(&catalog, &store, "anthropic")).expect("json");
        assert_eq!(status["version"], 1);
        assert_eq!(status["active_provider"], "anthropic");
        assert_eq!(status["providers"][0]["id"], "anthropic");
        assert_eq!(status["providers"][0]["active"], true);
        assert!(
            status["providers"][0]["default_model"]
                .as_str()
                .is_some_and(|m| !m.is_empty())
        );
        assert!(status["providers"][0]["auth"].is_string());
        // Unauthenticated: models falls back to the active provider; an unknown selector is empty.
        let models: Value =
            serde_json::from_str(&auth_models_json(&catalog, &store, "anthropic", None))
                .expect("json");
        assert_eq!(models["providers"][0]["id"], "anthropic");
        assert_eq!(
            models["providers"][0]["models"][0],
            status["providers"][0]["default_model"]
        );
        let none: Value = serde_json::from_str(&auth_models_json(
            &catalog,
            &store,
            "anthropic",
            Some("nope"),
        ))
        .expect("json");
        assert_eq!(none["providers"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn local_llama_detection_uses_provider_identity_or_loopback_host() {
        assert!(is_local_llama_provider(
            "local",
            ApiProvider::OpenAi,
            "https://remote.example/v1"
        ));
        for base_url in [
            "http://127.0.0.1:8080",
            "http://127.42.7.9:8080/v1",
            "http://localhost:8080",
            "http://[::1]:8080",
        ] {
            assert!(
                is_local_llama_provider("custom", ApiProvider::OpenAi, base_url),
                "{base_url}"
            );
        }
        for base_url in [
            "https://localhost.evil.example/v1",
            "https://evil.example/localhost/v1",
            "https://evil.example/?next=http://127.0.0.1:8080",
            "not a URL containing localhost",
        ] {
            assert!(
                !is_local_llama_provider("custom", ApiProvider::OpenAi, base_url),
                "{base_url}"
            );
        }
        assert!(!is_local_llama_provider(
            "local",
            ApiProvider::Anthropic,
            "http://127.0.0.1:8080"
        ));
    }

    #[test]
    fn oauth_browser_result_survives_listener_shutdown_or_deadline() {
        for cancelled in [false, true] {
            let cancelled = std::sync::atomic::AtomicBool::new(cancelled);
            for result in [OAuthBrowserResult::Success, OAuthBrowserResult::Failure] {
                let (sender, receiver) = std::sync::mpsc::channel();
                sender.send(result).unwrap();
                let received = await_oauth_browser_result(&receiver, &cancelled, Duration::ZERO);
                assert!(matches!(
                    (result, received),
                    (
                        OAuthBrowserResult::Success,
                        Some(OAuthBrowserResult::Success)
                    ) | (
                        OAuthBrowserResult::Failure,
                        Some(OAuthBrowserResult::Failure)
                    )
                ));
            }
            let (_sender, receiver) = std::sync::mpsc::channel();
            assert!(await_oauth_browser_result(&receiver, &cancelled, Duration::ZERO).is_none());
        }
    }

    #[test]
    fn oauth_callback_rejects_unreachable_or_unsafe_redirects() {
        for redirect in [
            "http://localhost:0/callback",
            "https://localhost:53692/callback",
            "ftp://localhost:53692/callback",
            "http://user@localhost:53692/callback",
            "http://localhost:53692/callback#fragment",
            "http://example.com:53692/callback",
        ] {
            assert!(oauth_callback_port(redirect).is_err(), "{redirect}");
            assert!(bind_oauth_callback("127.0.0.1", redirect, true, &[]).is_err());
        }
        assert_eq!(
            oauth_callback_port("http://localhost/callback").unwrap(),
            80
        );
    }

    #[cfg(any(not(unix), target_os = "macos"))]
    #[test]
    fn native_oauth_platforms_skip_wsl_port_query() {
        assert!(!running_in_wsl());
        for host in ["127.0.0.1", "::1"] {
            assert!(wsl_oauth_unavailable_ports(host).is_empty());
        }
    }

    #[test]
    fn windows_oauth_ports_parse_ranges_and_listeners() {
        let output = "Protocol tcp Port Exclusion Ranges\nStart Port End Port\n\
                      ---------- --------\n5357 5357\n50000 50059 *\n53688 53787\n\
                      * - Administered port exclusions.\n8788 8788\n\
                      65536 65537\n10 9\n0 10\n123 456 unexpected\n123 456 * extra\n";
        assert_eq!(
            parse_windows_oauth_unavailable_ports(output),
            vec![(5357, 5357), (50000, 50059), (53688, 53787), (8788, 8788)]
        );
    }

    #[test]
    fn oauth_callback_prefers_configured_port() -> Result<()> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let redirect = format!("http://localhost:{port}/callback");
        let (listener, selected) = bind_oauth_callback("127.0.0.1", &redirect, true, &[])?;
        assert_eq!(selected, redirect);
        assert_eq!(listener.local_addr()?.port(), port);
        Ok(())
    }

    #[test]
    fn oauth_callback_falls_back_on_occupied_or_windows_unavailable_port() -> Result<()> {
        let occupied = std::net::TcpListener::bind("127.0.0.1:0")?;
        let preferred = occupied.local_addr()?.port();
        let redirect = format!("http://localhost:{preferred}/callback");
        let (fallback, selected) = bind_oauth_callback("127.0.0.1", &redirect, true, &[])?;
        assert_ne!(fallback.local_addr()?.port(), preferred);
        let parsed = reqwest::Url::parse(&selected)?;
        assert_eq!(parsed.host_str(), Some("localhost"));
        assert_eq!(parsed.path(), "/callback");
        assert_eq!(parsed.port(), Some(fallback.local_addr()?.port()));
        assert!(fallback.local_addr()?.ip().is_loopback());
        assert!(bind_oauth_callback("127.0.0.1", &redirect, false, &[]).is_err());
        drop(occupied);
        let blocked = [(preferred, preferred)];
        let (fallback, _) = bind_oauth_callback("127.0.0.1", &redirect, true, &blocked)?;
        assert_ne!(fallback.local_addr()?.port(), preferred);
        assert!(bind_oauth_callback("127.0.0.1", &redirect, false, &blocked).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn oauth_callback_completion_drop_keeps_success_and_releases_listener() -> Result<()> {
        const CHILD: &str = "DEXT_TEST_OAUTH_CALLBACK_LIFECYCLE";
        if std::env::var_os(CHILD).is_none() {
            // Isolate the close probe from concurrent socket reuse and forked descriptors.
            let mut command = tokio::process::Command::new(std::env::current_exe()?);
            command
                .args(["--exact", "provider::tests::oauth_callback_completion_drop_keeps_success_and_releases_listener", "--nocapture"])
                .env(CHILD, "1")
                .kill_on_drop(true);
            let output = tokio::time::timeout(Duration::from_secs(20), command.output())
                .await
                .context("callback lifecycle child timed out")??;
            anyhow::ensure!(
                output.status.success(),
                "callback lifecycle child failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return Ok(());
        }
        let occupied = std::net::TcpListener::bind("127.0.0.1:0")?;
        let preferred = occupied.local_addr()?.port();
        let redirect = format!("http://localhost:{preferred}/callback");
        let (listener, selected) = bind_oauth_callback("127.0.0.1", &redirect, true, &[])?;
        let address = listener.local_addr()?;
        let callback = spawn_oauth_code_listener(
            listener,
            "expected-state".to_string(),
            "/callback".to_string(),
        )?;
        let browser = std::thread::spawn(move || -> io::Result<String> {
            let mut stream = std::net::TcpStream::connect(address)?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.write_all(b"GET /callback?code=accepted-code&state=expected-state HTTP/1.1\r\nHost: localhost\r\n\r\n")?;
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            Ok(response)
        });
        let received = callback.recv_timeout(Duration::from_secs(5))?;
        received.browser_result.send(OAuthBrowserResult::Success)?;
        drop(callback);
        let page = browser
            .join()
            .map_err(|_| anyhow::anyhow!("browser panic"))??;
        assert!(page.contains("200 OK"));
        assert!(page.contains("You're signed in"));
        assert_eq!(oauth_callback_port(&selected)?, address.port());
        assert!(std::net::TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_err());
        Ok(())
    }

    #[test]
    fn oauth_callback_fallback_is_bounded_and_provider_scoped() -> Result<()> {
        assert!(
            bind_oauth_callback(
                "127.0.0.1",
                "http://localhost:53692/callback",
                true,
                &[(1, u16::MAX)],
            )
            .unwrap_err()
            .to_string()
            .contains("32 attempts")
        );
        let profiles = built_in_provider_profiles();
        let mut anthropic = profiles
            .iter()
            .find(|profile| profile.id == "anthropic")
            .unwrap()
            .oauth_flow
            .clone()
            .unwrap();
        assert!(oauth_allows_dynamic_callback(&anthropic));
        anthropic.client_id = "unreviewed-client".to_string();
        assert!(!oauth_allows_dynamic_callback(&anthropic));
        anthropic.client_id = "9d1c250a-e61b-44d9-88ed-5944d1962f5e".to_string();
        anthropic.authorize_url = "https://custom.example/authorize".to_string();
        assert!(!oauth_allows_dynamic_callback(&anthropic));
        assert!(!oauth_allows_dynamic_callback(
            profiles
                .iter()
                .find(|profile| profile.id == "chatgpt")
                .unwrap()
                .oauth_flow
                .as_ref()
                .unwrap()
        ));
        assert!(
            bind_oauth_callback("127.0.0.1", "http://example.com:53692/callback", true, &[])
                .is_err()
        );
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn oauth_dynamic_callback_browser_exchange_and_storage_use_same_redirect() -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let _guard = crate::test_env_lock();
        let home = temp_home("dynamic-oauth");
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin)?;
        let marker = home.join("authorize-url");
        for name in ["wslview", "xdg-open"] {
            let path = bin.join(name);
            std::fs::write(
                &path,
                "#!/bin/sh\nprintf '%s' \"$1\" > \"$BROWSER_TEST_MARKER.tmp\" && mv \"$BROWSER_TEST_MARKER.tmp\" \"$BROWSER_TEST_MARKER\"\n",
            )?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        let occupied = std::net::TcpListener::bind("127.0.0.1:0")?;
        let preferred = occupied.local_addr()?.port();
        let probe = bin.join("powershell.exe");
        std::fs::write(
            &probe,
            format!("#!/bin/sh\nprintf '{preferred} {preferred}\\n'\n"),
        )?;
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o700))?;
        let names = [
            "DEXT_HOME",
            "PATH",
            "BROWSER_TEST_MARKER",
            "DEXT_SKIP_BROWSER_OPEN",
            "DEXT_DISABLE_WSL_BROWSER_OPEN",
            "DEXT_OAUTH_CALLBACK_HOST",
            "NO_PROXY",
            "no_proxy",
            "WSL_DISTRO_NAME",
        ];
        struct Cleanup {
            home: PathBuf,
            old: [(&'static str, Option<std::ffi::OsString>); 9],
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for (name, value) in &self.old {
                    unsafe {
                        match value {
                            Some(value) => std::env::set_var(name, value),
                            None => std::env::remove_var(name),
                        }
                    }
                }
                let _ = std::fs::remove_dir_all(&self.home);
            }
        }
        let _cleanup = Cleanup {
            home: home.clone(),
            old: names.map(|name| (name, std::env::var_os(name))),
        };
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let path = std::env::join_paths(paths)?;
        unsafe {
            std::env::set_var("DEXT_HOME", &home);
            std::env::set_var("PATH", path);
            std::env::set_var("BROWSER_TEST_MARKER", &marker);
            std::env::set_var("DEXT_OAUTH_CALLBACK_HOST", "127.0.0.1");
            std::env::remove_var("DEXT_SKIP_BROWSER_OPEN");
            std::env::remove_var("DEXT_DISABLE_WSL_BROWSER_OPEN");
            std::env::set_var("NO_PROXY", "localhost,127.0.0.1");
            std::env::set_var("no_proxy", "localhost,127.0.0.1");
            std::env::set_var("WSL_DISTRO_NAME", "dext-oauth-fixture");
        }
        (|| -> Result<()> {
            assert_eq!(
                wsl_oauth_unavailable_ports("127.0.0.1"),
                vec![(preferred, preferred)]
            );
            let token_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let token_address = token_listener.local_addr()?;
            token_listener.set_nonblocking(true)?;
            let token_server = std::thread::spawn(move || -> Result<Value> {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                let mut stream = loop {
                    match token_listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => return Err(error.into()),
                    }
                };
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut request = Vec::new();
                let body = loop {
                    let mut buffer = [0u8; 4096];
                    let count = stream.read(&mut buffer)?;
                    anyhow::ensure!(
                        count > 0 && request.len() + count <= 16 * 1024,
                        "incomplete token request"
                    );
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .context("token request length")?;
                        if request.len() >= end + 4 + length {
                            break serde_json::from_slice::<Value>(
                                &request[end + 4..end + 4 + length],
                            )?;
                        }
                    }
                };
                let response = serde_json::json!({
                    "access_token": "dynamic-fixture-access",
                    "refresh_token": "dynamic-fixture-refresh",
                    "expires_in": 3600,
                })
                .to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )?;
                Ok(body)
            });
            let browser = std::thread::spawn(move || -> Result<(String, String)> {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                let url = loop {
                    if let Ok(value) = std::fs::read_to_string(&marker)
                        && !value.is_empty()
                    {
                        break reqwest::Url::parse(&value)?;
                    }
                    anyhow::ensure!(
                        std::time::Instant::now() < deadline,
                        "browser launcher timeout"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                };
                let params = url.query_pairs().collect::<HashMap<_, _>>();
                let redirect = params
                    .get("redirect_uri")
                    .context("browser redirect")?
                    .to_string();
                let callback = reqwest::Url::parse(&redirect)?;
                let port = callback.port().context("callback port")?;
                assert_ne!(port, preferred);
                let state = params.get("state").context("callback state")?;
                let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))?;
                stream.set_read_timeout(Some(Duration::from_secs(10)))?;
                write!(
                    stream,
                    "GET /callback?code=dynamic-code&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n"
                )?;
                let mut response = String::new();
                stream.read_to_string(&mut response)?;
                Ok((redirect, response))
            });
            let mut profile = built_in_provider_profiles()
                .into_iter()
                .find(|profile| profile.id == "anthropic")
                .unwrap();
            let mut oauth = profile.oauth_flow.clone().unwrap();
            oauth.redirect_uri = Some(format!("http://localhost:{preferred}/callback"));
            oauth.token_url = format!("http://{token_address}/token");
            profile.oauth_flow = Some(oauth.clone());
            let login = run_oauth_login(&oauth, &profile);
            let token_result = token_server.join();
            let browser_result = browser.join();
            let token_request =
                token_result.map_err(|_| anyhow::anyhow!("token server panic"))??;
            let (redirect, page) =
                browser_result.map_err(|_| anyhow::anyhow!("browser panic"))??;
            assert!(!login?.awaiting_credentials);
            assert_eq!(token_request["redirect_uri"], redirect);
            assert_eq!(token_request["code"], "dynamic-code");
            assert!(page.contains("200 OK"));
            assert!(page.contains("You're signed in"));
            assert!(!page.contains("dynamic-fixture-access"));
            assert!(load_auth_store()?.providers.contains_key("anthropic"));
            assert!(load_pending_oauth().is_none());
            Ok(())
        })()
    }

    #[test]
    fn oauth_callback_listener_ignores_invalid_requests_until_valid_callback() -> Result<()> {
        let code = receive_oauth_callback_for_test(
            "expected-state",
            "/callback",
            &[
                "GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n".to_string(),
                "GET /callback?code=wrong&state=wrong-state HTTP/1.1\r\nHost: localhost\r\n\r\n"
                    .to_string(),
                "GET /callback?code=accepted-code&state=expected-state HTTP/1.1\r\nHost: localhost\r\n\r\n"
                    .to_string(),
            ],
        )?;
        assert_eq!(code, "accepted-code");
        Ok(())
    }

    #[test]
    fn oauth_callback_listener_rejects_malformed_and_oversized_requests() -> Result<()> {
        let oversized = format!(
            "GET /callback?code=oversized&state=expected-state HTTP/1.1\r\nX-Fill: {}\r\n\r\n",
            "x".repeat(8192)
        );
        let code = receive_oauth_callback_for_test(
            "expected-state",
            "/callback",
            &[
                "POST /callback?code=post&state=expected-state HTTP/1.1\r\nHost: localhost\r\n\r\n"
                    .to_string(),
                "GET /callback?code=incomplete&state=expected-state HTTP/1.1\r\nHost: localhost\r\n"
                    .to_string(),
                oversized,
                "GET /callback?code=accepted-code&state=expected-state HTTP/1.1\r\nHost: localhost\r\n\r\n"
                    .to_string(),
            ],
        )?;
        assert_eq!(code, "accepted-code");
        Ok(())
    }

    #[test]
    fn oauth_callback_listener_renders_failure_only_after_completion_fails() -> Result<()> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let callback = spawn_oauth_code_listener(
            listener,
            "expected-state".to_string(),
            "/callback".to_string(),
        )?;
        let client = std::thread::spawn(move || -> io::Result<Vec<u8>> {
            let mut stream = std::net::TcpStream::connect(address)?;
            stream.write_all(
                b"GET /callback?code=accepted-code&state=expected-state HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )?;
            stream.flush()?;
            let mut response = Vec::new();
            stream.read_to_end(&mut response)?;
            Ok(response)
        });
        let received = callback.recv_timeout(Duration::from_secs(5))?;
        let OAuthCallbackKind::Code(code) = received.kind else {
            anyhow::bail!("expected OAuth code callback");
        };
        assert_eq!(code, "accepted-code");
        received
            .browser_result
            .send(OAuthBrowserResult::Failure)
            .map_err(|_| anyhow::anyhow!("send OAuth browser result"))?;
        let response = client
            .join()
            .map_err(|_| anyhow::anyhow!("OAuth test client panicked"))??;
        let response = String::from_utf8_lossy(&response);
        assert!(response.contains("500 Internal Server Error"));
        assert!(response.contains("AUTHENTICATION INCOMPLETE"));
        assert!(!response.contains("You're signed in"));
        Ok(())
    }

    #[test]
    fn oauth_callback_listener_renders_provider_denial() -> Result<()> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let callback = spawn_oauth_code_listener(
            listener,
            "expected-state".to_string(),
            "/callback".to_string(),
        )?;
        let client = std::thread::spawn(move || -> io::Result<Vec<u8>> {
            let mut stream = std::net::TcpStream::connect(address)?;
            stream.write_all(
                b"GET /callback?error=access_denied&state=expected-state HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )?;
            stream.flush()?;
            let mut response = Vec::new();
            stream.read_to_end(&mut response)?;
            Ok(response)
        });
        let received = callback.recv_timeout(Duration::from_secs(2))?;
        assert!(matches!(received.kind, OAuthCallbackKind::Rejected));
        received
            .browser_result
            .send(OAuthBrowserResult::Failure)
            .map_err(|_| anyhow::anyhow!("send OAuth browser result"))?;
        let response = client
            .join()
            .map_err(|_| anyhow::anyhow!("OAuth test client panicked"))??;
        let response = String::from_utf8_lossy(&response);
        assert!(response.contains("400 Bad Request"));
        assert!(response.contains("AUTHENTICATION INCOMPLETE"));
        assert!(!response.contains("access_denied"));
        Ok(())
    }

    #[test]
    fn oauth_refresh_rotates_and_persists_tokens() -> Result<()> {
        let _guard = crate::test_env_lock();
        let home = temp_home("oauth-refresh-rotation");
        let old_home = std::env::var_os("DEXT_HOME");
        unsafe { std::env::set_var("DEXT_HOME", &home) };

        let result = (|| -> Result<()> {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let address = listener.local_addr()?;
            let server = std::thread::spawn(move || -> io::Result<()> {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                let mut request = [0u8; 4096];
                let size = stream.read(&mut request)?;
                let request = String::from_utf8_lossy(&request[..size]);
                assert!(request.contains("POST /token HTTP/1.1"));
                assert!(request.contains("old-refresh"));
                let body = r#"{"access_token":" new-access ","refresh_token":" new-refresh ","expires_in":3600}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )?;
                Ok(())
            });

            let mut profile = built_in_provider_profiles()
                .into_iter()
                .find(|profile| profile.id == "anthropic")
                .context("Anthropic profile")?;
            profile
                .oauth_flow
                .as_mut()
                .context("Anthropic OAuth")?
                .token_url = format!("http://{address}/token");
            let mut store = AuthStore::default();
            store.providers.insert(
                "anthropic".to_string(),
                StoredCredential::OAuth {
                    access_token: "old-access".to_string(),
                    refresh_token: Some("old-refresh".to_string()),
                    expires_at: Some(unix_timestamp_secs()),
                },
            );
            save_auth_store(&store)?;

            assert_eq!(
                refresh_oauth_credential_if_needed(&profile, &mut store)?.as_deref(),
                Some("new-access")
            );
            server
                .join()
                .map_err(|_| anyhow::anyhow!("OAuth refresh server panicked"))??;
            let persisted = load_auth_store()?;
            assert!(matches!(
                persisted.providers.get("anthropic"),
                Some(StoredCredential::OAuth {
                    access_token,
                    refresh_token: Some(refresh_token),
                    expires_at: Some(expires_at),
                }) if access_token == "new-access"
                    && refresh_token == "new-refresh"
                    && *expires_at > unix_timestamp_secs()
            ));
            Ok(())
        })();

        if let Some(value) = old_home {
            unsafe { std::env::set_var("DEXT_HOME", value) };
        } else {
            unsafe { std::env::remove_var("DEXT_HOME") };
        }
        let _ = std::fs::remove_dir_all(home);
        result
    }

    #[test]
    fn oauth_callback_listener_accepts_fragmented_request_headers() -> Result<()> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let callback = spawn_oauth_code_listener(
            listener,
            "expected-state".to_string(),
            "/callback".to_string(),
        )?;
        let mut stream = std::net::TcpStream::connect(address)?;
        stream.write_all(b"GET /callback?code=fragmented")?;
        stream.flush()?;
        std::thread::sleep(Duration::from_millis(350));
        stream.write_all(b"-code&state=expected-state HTTP/1.1\r\nHost: localhost\r\n\r\n")?;
        stream.flush()?;
        let mut response = Vec::new();
        let received = callback.recv_timeout(Duration::from_secs(5))?;
        let OAuthCallbackKind::Code(code) = received.kind else {
            anyhow::bail!("expected OAuth code callback");
        };
        received
            .browser_result
            .send(OAuthBrowserResult::Success)
            .map_err(|_| anyhow::anyhow!("send OAuth browser result"))?;
        stream.read_to_end(&mut response)?;
        let response = String::from_utf8_lossy(&response);
        assert!(response.contains("200 OK"));
        assert!(response.contains("DEXT</b> / LOGIN"));
        assert!(response.contains("You're signed in"));
        assert!(response.contains("Cache-Control: no-store"));
        assert_eq!(code, "fragmented-code");
        Ok(())
    }

    #[test]
    fn auth_login_guidance_avoids_shell_secret_arguments() {
        assert_eq!(AUTH_LOGIN_ARGUMENTS, "[credential|web|import]");
        let guidance = auth_retry_guidance("anthropic");
        assert!(guidance.contains("remains incomplete"), "{guidance}");
        assert!(guidance.contains("dext auth login anthropic"), "{guidance}");
        assert!(guidance.contains("/login anthropic"), "{guidance}");
        assert!(guidance.contains("manual OAuth callback"), "{guidance}");
        assert!(guidance.contains("shell history"), "{guidance}");
        assert!(!guidance.contains("<api-key"), "{guidance}");
    }

    #[test]
    fn kimi_device_id_is_stable_uuid_and_ascii_headers_are_safe() -> Result<()> {
        let _guard = crate::test_env_lock();
        let home = temp_home("device-id");
        let old_home = std::env::var_os("DEXT_HOME");
        unsafe { std::env::set_var("DEXT_HOME", &home) };

        let result = (|| -> Result<()> {
            let first = kimi_device_id()?;
            let second = kimi_device_id()?;
            assert_eq!(first, second);
            assert_eq!(first.len(), 36);
            assert_eq!(first.bytes().filter(|byte| *byte == b'-').count(), 4);
            assert!(kimi_device_id_is_valid(&first));
            assert_eq!(kimi_ascii_header(" h\u{00f4}st\n"), "hst");
            assert_eq!(kimi_ascii_header("\n\t"), "unknown");
            Ok(())
        })();

        unsafe {
            match old_home {
                Some(value) => std::env::set_var("DEXT_HOME", value),
                None => std::env::remove_var("DEXT_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&home);
        result
    }
}
