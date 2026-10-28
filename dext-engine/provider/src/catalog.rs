//! Pure catalog normalization and provider request shaping; no ambient configuration.
use anyhow::{Context, Result};
use http::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingEffort {
    Off,
    Minimal,
    Low,
    #[default]
    Medium,
    High,
    XHigh,
    Max,
}

impl ThinkingEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn guidance(self) -> &'static str {
        match self {
            Self::Off => {
                "Disable provider-side reasoning controls where supported; answer directly."
            }
            Self::Minimal => "Use the least available reasoning beyond none.",
            Self::Low => "Keep reasoning concise and deliver a direct answer quickly.",
            Self::Medium => "Use balanced reasoning depth and concise explanations.",
            Self::High => "Reason carefully through edge cases before answering.",
            Self::XHigh => {
                "Use deep, methodical reasoning with explicit verification before answering."
            }
            Self::Max => {
                "Use maximum supported reasoning depth with explicit verification before answering."
            }
        }
    }

    pub fn parse(v: &str) -> Option<Self> {
        match v.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "disable" | "disabled" | "0" => Some(Self::Off),
            "minimal" | "min" => Some(Self::Minimal),
            "low" | "l" => Some(Self::Low),
            "medium" | "med" | "m" | "default" => Some(Self::Medium),
            "high" | "h" => Some(Self::High),
            "xhigh" | "x-high" | "veryhigh" | "very-high" | "xh" => Some(Self::XHigh),
            "max" | "maximum" | "ultra" | "ultracode" => Some(Self::Max),
            _ => None,
        }
    }

    pub fn cycle(self, step: i8) -> Self {
        let levels = [
            Self::Off,
            Self::Minimal,
            Self::Low,
            Self::Medium,
            Self::High,
            Self::XHigh,
            Self::Max,
        ];
        let idx = levels.iter().position(|v| *v == self).unwrap_or(2) as i32;
        let len = levels.len() as i32;
        let next = (idx + i32::from(step)).rem_euclid(len) as usize;
        levels[next]
    }
}

/// `anthropic-version` request header value sent on all Anthropic Messages API calls.
pub const ANTHROPIC_API_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ApiProvider {
    #[default]
    Anthropic,
    OpenAi,
    ChatGpt,
}

impl serde::Serialize for ApiProvider {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for ApiProvider {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "openai" | "openai-compatible" | "ollama" | "llama" | "llama.cpp" | "local"
            | "deepseek" => Self::OpenAi,
            "chatgpt" | "openai-codex" | "codex" | "codex-openai" => Self::ChatGpt,
            _ => Self::Anthropic,
        })
    }
}

impl ApiProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::ChatGpt => "chatgpt",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestContract {
    #[serde(rename = "anthropic-messages", alias = "anthropic")]
    AnthropicMessages,
    #[serde(
        rename = "openai-chat-completions",
        alias = "openai",
        alias = "openai-compatible"
    )]
    OpenAiChatCompletions,
    #[serde(rename = "openai-responses", alias = "responses")]
    OpenAiResponses,
    #[serde(rename = "chatgpt-responses", alias = "chatgpt", alias = "codex")]
    ChatGptResponses,
}

impl From<RequestContract> for dext_core::recovery::Contract {
    fn from(value: RequestContract) -> Self {
        value.recovery_contract()
    }
}

impl RequestContract {
    pub fn recovery_contract(self) -> dext_core::recovery::Contract {
        match self {
            Self::AnthropicMessages => dext_core::recovery::Contract::AnthropicMessages,
            Self::OpenAiChatCompletions => dext_core::recovery::Contract::OpenAiChatCompletions,
            Self::OpenAiResponses => dext_core::recovery::Contract::OpenAiResponses,
            Self::ChatGptResponses => dext_core::recovery::Contract::ChatGptResponses,
        }
    }

    pub fn for_api_provider(api_provider: ApiProvider) -> Self {
        match api_provider {
            ApiProvider::Anthropic => Self::AnthropicMessages,
            ApiProvider::OpenAi => Self::OpenAiChatCompletions,
            ApiProvider::ChatGpt => Self::ChatGptResponses,
        }
    }

    pub fn api_provider(self) -> ApiProvider {
        match self {
            Self::AnthropicMessages => ApiProvider::Anthropic,
            Self::OpenAiChatCompletions | Self::OpenAiResponses => ApiProvider::OpenAi,
            Self::ChatGptResponses => ApiProvider::ChatGpt,
        }
    }

    pub fn is_responses(self) -> bool {
        matches!(self, Self::OpenAiResponses | Self::ChatGptResponses)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic-messages",
            Self::OpenAiChatCompletions => "openai-chat-completions",
            Self::OpenAiResponses => "openai-responses",
            Self::ChatGptResponses => "chatgpt-responses",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelCapabilities {
    pub tools: Option<bool>,
    pub reasoning: Option<bool>,
    pub image_input: Option<bool>,
    pub prompt_cache: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input_usd_per_mtok: f64,
    pub output_usd_per_mtok: f64,
    pub cache_read_usd_per_mtok: f64,
    pub cache_create_usd_per_mtok: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelSpec {
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub effort_levels: Vec<String>,
    pub reasoning_modes: Vec<String>,
    pub capabilities: ModelCapabilities,
    pub pricing: Option<ModelPricing>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModelSpec {
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub effort_levels: Vec<String>,
    pub reasoning_modes: Vec<String>,
    pub tools: bool,
    pub reasoning: bool,
    pub image_input: bool,
    pub prompt_cache: bool,
    pub pricing: Option<ModelPricing>,
    pub source: &'static str,
}

pub(crate) const PROVIDER_CATALOG_VERSION: u32 = 4;
pub(crate) const STRUCTURED_PROVIDER_CATALOG_VERSION: u32 = 2;

pub const DEFAULT_LOCAL_MODEL: &str = "qwen3.6-35b-a3b-mtp-ud-q5_k_m";

pub(crate) const KIMI_CODE_BASE_URL: &str = "https://api.kimi.com/coding";
pub(crate) const KIMI_BUILTIN_PROFILE_MARKER: &str = "kimi-code";
pub(crate) const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
pub(crate) const ANTHROPIC_BUILTIN_PROFILE_MARKER: &str = "anthropic";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OAuthProtocol {
    #[default]
    Generic,
    AnthropicClaude,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthFlow {
    pub authorize_url: String,
    pub token_url: String,
    pub client_id: String,
    pub scope: String,
    #[serde(default)]
    pub audience: String,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub callback_host: Option<String>,
    #[serde(default)]
    pub protocol: OAuthProtocol,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProfile {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin: Option<String>,
    #[serde(default)]
    pub display_name: String,
    pub api_provider: ApiProvider,
    #[serde(default)]
    pub request_contract: Option<RequestContract>,
    pub base_url: String,
    pub default_model: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub model_aliases: HashMap<String, String>,
    #[serde(default)]
    pub model_defaults: ModelSpec,
    #[serde(default)]
    pub model_specs: HashMap<String, ModelSpec>,
    #[serde(default)]
    pub env_vars: Vec<String>,
    #[serde(default = "default_provider_requires_api_key")]
    pub requires_api_key: bool,
    #[serde(default)]
    pub login_url: Option<String>,
    #[serde(default)]
    pub oauth_flow: Option<OAuthFlow>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Default request context window (in tokens) for this provider's models.
    /// llama.cpp/local may override this at runtime. Env override: DEXT_CONTEXT_WINDOW[_TOKENS].
    #[serde(default)]
    pub context_window: Option<u64>,
    /// Optional per-model override of context_window. Map key = model id.
    #[serde(default)]
    pub model_context_windows: HashMap<String, u64>,
    /// Optional per-model provider-native reasoning effort levels.
    /// Map key = model id; values are strings such as "high"/"max".
    #[serde(default)]
    pub model_effort_levels: HashMap<String, Vec<String>>,
}

pub fn default_provider_requires_api_key() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCatalog {
    #[serde(default = "default_provider_catalog_version")]
    pub version: u32,
    #[serde(default = "default_active_provider")]
    pub active_provider: String,
    pub providers: Vec<ProviderProfile>,
}

pub fn default_provider_catalog_version() -> u32 {
    PROVIDER_CATALOG_VERSION
}

pub fn default_active_provider() -> String {
    "glm".to_string()
}

pub fn canonical_provider_id(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "zai" | "bigmodel" | "bigmodel-cn" => "glm".to_string(),
        "openai-codex" | "codex-openai" | "codex" => "chatgpt".to_string(),
        "chatgpt-plus" | "chatgpt-pro" => "chatgpt".to_string(),
        "claude" => "anthropic".to_string(),
        "kimi-code" | "kimi-coding" | "kimi-membership" => "kimi".to_string(),
        "llama" | "llama.cpp" | "llamacpp" | "qwen" => "local".to_string(),
        other => other.to_string(),
    }
}

pub(crate) const RETIRED_BUNDLED_PROVIDER_IDS: &[&str] = &["openrouter", "ollama"];

pub(crate) fn model_pricing(
    input: f64,
    output: f64,
    cache_read: f64,
    cache_create: f64,
) -> ModelPricing {
    ModelPricing {
        input_usd_per_mtok: input,
        output_usd_per_mtok: output,
        cache_read_usd_per_mtok: cache_read,
        cache_create_usd_per_mtok: cache_create,
    }
}

pub(crate) fn openai_responses_model_specs(
    include_unsuffixed_5_6_alias: bool,
    include_5_6_pro_mode: bool,
) -> HashMap<String, ModelSpec> {
    let spec =
        |effort_levels: &[&str], reasoning_modes: &[&str], pricing: ModelPricing| ModelSpec {
            context_window: Some(1_050_000),
            max_output_tokens: Some(128_000),
            effort_levels: effort_levels
                .iter()
                .map(|level| (*level).to_string())
                .collect(),
            reasoning_modes: reasoning_modes
                .iter()
                .map(|mode| (*mode).to_string())
                .collect(),
            capabilities: ModelCapabilities {
                tools: Some(true),
                reasoning: Some(true),
                image_input: Some(true),
                prompt_cache: Some(true),
            },
            pricing: Some(pricing),
        };
    let gpt_5_6_efforts: &[&str] = if include_5_6_pro_mode {
        &["none", "minimal", "low", "medium", "high", "xhigh", "max"]
    } else {
        &["none", "low", "medium", "high", "xhigh"]
    };
    let gpt_5_6_modes: &[&str] = if include_5_6_pro_mode {
        &["standard", "pro"]
    } else {
        &[]
    };
    let gpt_5_6_sol = spec(
        gpt_5_6_efforts,
        gpt_5_6_modes,
        model_pricing(5.0, 30.0, 0.5, 6.25),
    );
    let mut specs = HashMap::from([
        (
            "gpt-6.1-sol".to_string(),
            spec(
                &["low", "medium", "high", "xhigh", "max"],
                &[],
                model_pricing(2.0, 10.0, 0.1, 2.5),
            ),
        ),
        (
            "gpt-6-astra".to_string(),
            spec(
                &["low", "medium", "high", "xhigh", "max"],
                &[],
                model_pricing(10.0, 50.0, 1.0, 12.5),
            ),
        ),
        (
            "gpt-6-sol".to_string(),
            spec(
                &["none", "low", "medium", "high", "xhigh", "max"],
                &[],
                model_pricing(2.0, 10.0, 0.2, 2.5),
            ),
        ),
        (
            "gpt-6-luna".to_string(),
            spec(
                &["none", "low", "medium", "high", "xhigh", "max"],
                &[],
                model_pricing(0.1, 0.5, 0.01, 0.125),
            ),
        ),
        ("gpt-5.6-sol".to_string(), gpt_5_6_sol.clone()),
        (
            "gpt-5.6-terra".to_string(),
            spec(
                gpt_5_6_efforts,
                gpt_5_6_modes,
                model_pricing(2.5, 15.0, 0.25, 3.125),
            ),
        ),
        (
            "gpt-5.6-luna".to_string(),
            spec(
                gpt_5_6_efforts,
                gpt_5_6_modes,
                model_pricing(1.0, 6.0, 0.1, 1.25),
            ),
        ),
    ]);
    if include_unsuffixed_5_6_alias {
        specs.insert("gpt-5.6".to_string(), gpt_5_6_sol);
    }
    specs
}

pub(crate) fn model_has_family_fragment(model: &str, fragments: &[&str]) -> bool {
    let model = model.trim().to_ascii_lowercase();
    let model = model.rsplit('/').next().unwrap_or(&model);
    let model = model.strip_prefix("claude-").unwrap_or(model);
    fragments.iter().any(|fragment| {
        model
            .strip_prefix(fragment)
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('-'))
    })
}

pub fn is_claude_fable_5_1_model(model: &str) -> bool {
    model_has_family_fragment(model, &["fable-5-1", "fable-5.1", "fable5.1"])
}

pub fn is_claude_opus_5_5_model(model: &str) -> bool {
    model_has_family_fragment(model, &["opus-5-5", "opus-5.5", "opus5.5"])
}

pub(crate) fn canonical_deepseek_model(model: &str) -> Option<&'static str> {
    match model.trim().to_ascii_lowercase().as_str() {
        "deepseek-chat"
        | "deepseek-flash"
        | "deepseek-v4-flash"
        | "deepseek-v4-flash-vision-exp" => Some("deepseek-flash"),
        "deepseek-reasoner" | "deepseek-v4-pro" => Some("deepseek-v4-pro"),
        _ => None,
    }
}

pub fn deepseek_model_pricing(model: &str) -> Option<ModelPricing> {
    match model.trim().to_ascii_lowercase().as_str() {
        "deepseek-flash" | "deepseek-v4-flash" | "deepseek-v4-flash-vision-exp" => {
            Some(model_pricing(0.3, 1.2, 0.006, 0.0))
        }
        "deepseek-v4-pro" => Some(model_pricing(1.32, 3.96, 0.044, 0.0)),
        _ => None,
    }
}

pub(crate) fn builtin_model_pricing(provider_id: &str, model: &str) -> Option<ModelPricing> {
    let model = model.to_ascii_lowercase();
    match canonical_provider_id(provider_id).as_str() {
        "local" | "kimi" => Some(model_pricing(0.0, 0.0, 0.0, 0.0)),
        "glm" if model.trim_end_matches("[1m]") == "glm-5.3-flash" => {
            Some(model_pricing(0.15, 0.5, 0.03, 0.0))
        }
        "glm" => Some(model_pricing(1.0, 5.0, 0.1, 1.25)),
        "deepseek" => deepseek_model_pricing(&model),
        "anthropic" if is_claude_fable_5_1_model(&model) => {
            Some(model_pricing(10.0, 50.0, 0.25, 12.5))
        }
        "anthropic" if model.contains("fable") => Some(model_pricing(10.0, 50.0, 1.0, 12.5)),
        "anthropic" if is_claude_opus_5_5_model(&model) => Some(model_pricing(4.0, 20.0, 0.2, 5.0)),
        // Opus 4.5 through Opus 5 share one published rate; Opus 4.1-and-earlier
        // retain legacy pricing via the plain "opus" arm below.
        "anthropic"
            if [
                "opus-5", "opus5", "opus-4-5", "opus-4.5", "opus-4-6", "opus-4.6", "opus-4-7",
                "opus-4.7", "opus-4-8", "opus-4.8",
            ]
            .iter()
            .any(|generation| model.contains(generation)) =>
        {
            Some(model_pricing(5.0, 25.0, 0.5, 6.25))
        }
        "anthropic" if model.contains("opus") => Some(model_pricing(15.0, 75.0, 1.5, 18.75)),
        "anthropic" if model.contains("sonnet-5") || model.contains("sonnet5") => {
            Some(model_pricing(2.0, 10.0, 0.2, 2.5))
        }
        "anthropic" if model.contains("sonnet") => Some(model_pricing(3.0, 15.0, 0.3, 3.75)),
        "anthropic" if model.contains("haiku-4-5") || model.contains("haiku-4.5") => {
            Some(model_pricing(1.0, 5.0, 0.1, 1.25))
        }
        "anthropic" if model.contains("haiku") => Some(model_pricing(0.8, 4.0, 0.08, 1.0)),
        "openai" | "chatgpt" if model == "gpt-6-astra" => {
            Some(model_pricing(10.0, 50.0, 1.0, 12.5))
        }
        "openai" | "chatgpt" if model == "gpt-6.1-sol" => Some(model_pricing(2.0, 10.0, 0.1, 2.5)),
        "openai" | "chatgpt" if model == "gpt-6-sol" => Some(model_pricing(2.0, 10.0, 0.2, 2.5)),
        "openai" | "chatgpt" if model == "gpt-6-luna" => Some(model_pricing(0.1, 0.5, 0.01, 0.125)),
        "openai" | "chatgpt" if model == "gpt-5.6-sol" => Some(model_pricing(5.0, 30.0, 0.5, 6.25)),
        "openai" | "chatgpt" if model == "gpt-5.6" => Some(model_pricing(5.0, 30.0, 0.5, 6.25)),
        "openai" | "chatgpt" if model == "gpt-5.6-terra" => {
            Some(model_pricing(2.5, 15.0, 0.25, 3.125))
        }
        "openai" | "chatgpt" if model == "gpt-5.6-luna" => Some(model_pricing(1.0, 6.0, 0.1, 1.25)),
        "openai" | "chatgpt" if model.starts_with("gpt-5.4-mini") => {
            Some(model_pricing(0.25, 2.0, 0.025, 0.25))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-5.3-codex-spark") => {
            Some(model_pricing(0.25, 2.0, 0.025, 0.25))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-5-mini") => {
            Some(model_pricing(0.25, 2.0, 0.025, 0.25))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-5-nano") => {
            Some(model_pricing(0.05, 0.4, 0.005, 0.05))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-5") => {
            Some(model_pricing(1.25, 10.0, 0.125, 1.25))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-4.1-mini") => {
            Some(model_pricing(0.4, 1.6, 0.1, 0.4))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-4.1-nano") => {
            Some(model_pricing(0.1, 0.4, 0.025, 0.1))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-4.1") => {
            Some(model_pricing(2.0, 8.0, 0.5, 2.0))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-4o-mini") => {
            Some(model_pricing(0.15, 0.6, 0.075, 0.15))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-4o") => {
            Some(model_pricing(2.5, 10.0, 1.25, 2.5))
        }
        "openai" | "chatgpt" if model.starts_with("o3-mini") || model.starts_with("o4-mini") => {
            Some(model_pricing(1.1, 4.4, 0.55, 1.1))
        }
        "openai" | "chatgpt" if model.starts_with("o3") => Some(model_pricing(2.0, 8.0, 0.5, 2.0)),
        _ => None,
    }
}

pub(crate) fn hydrate_builtin_model_specs(profiles: &mut [ProviderProfile]) {
    for profile in profiles {
        let provider_id = canonical_provider_id(&profile.id);
        let contract = request_contract_for_profile(profile);
        if profile.model_defaults.max_output_tokens.is_none() {
            profile.model_defaults.max_output_tokens = Some(8_192);
        }
        profile.model_defaults.capabilities = ModelCapabilities {
            tools: Some(true),
            reasoning: Some(true),
            image_input: Some(
                matches!(
                    contract,
                    RequestContract::AnthropicMessages
                        | RequestContract::OpenAiResponses
                        | RequestContract::ChatGptResponses
                ) || provider_id == "openai",
            ),
            prompt_cache: Some(matches!(
                provider_id.as_str(),
                "anthropic" | "openai" | "chatgpt" | "deepseek" | "local"
            )),
        };
        if provider_id == "glm" {
            profile.model_defaults.capabilities.image_input = Some(false);
        }
        for model in profile.models.clone() {
            let normalized = model.to_ascii_lowercase();
            let mut spec = profile.model_specs.remove(&normalized).unwrap_or_default();
            spec.pricing = builtin_model_pricing(&provider_id, &normalized);
            if provider_id == "glm" {
                spec.capabilities.image_input =
                    Some(normalized.trim_end_matches("[1m]") == "glm-5.3-flash");
            }
            if matches!(provider_id.as_str(), "openai" | "chatgpt")
                && (normalized.starts_with("gpt-4.1") || normalized.starts_with("gpt-4o"))
            {
                spec.capabilities.reasoning = Some(false);
            }
            profile.model_specs.insert(normalized, spec);
        }
    }
}

pub fn built_in_provider_profiles() -> Vec<ProviderProfile> {
    let glm_flash_spec = ModelSpec {
        context_window: Some(1_000_000),
        max_output_tokens: Some(131_072),
        effort_levels: ["low", "high", "max"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        reasoning_modes: Vec::new(),
        capabilities: ModelCapabilities {
            tools: Some(true),
            reasoning: Some(true),
            image_input: Some(true),
            prompt_cache: None,
        },
        pricing: None,
    };
    let mut profiles = vec![
        ProviderProfile {
            id: "glm".to_string(),
            builtin: None,
            display_name: "ZAI GLM".to_string(),
            api_provider: ApiProvider::Anthropic,
            base_url: "https://api.z.ai/api/anthropic".to_string(),
            default_model: "glm-5.2[1m]".to_string(),
            models: vec![
                "glm-5.3-flash".to_string(),
                "glm-5.3-flash[1m]".to_string(),
                "glm-5.2[1m]".to_string(),
                "glm-5.2".to_string(),
                "glm-5.1".to_string(),
                "glm-5.0".to_string(),
                "glm-4.6".to_string(),
            ],
            env_vars: vec!["ZAI_API_KEY".to_string()],
            requires_api_key: true,
            login_url: Some("https://open.bigmodel.cn/usercenter/apikeys".to_string()),
            oauth_flow: None,
            notes: Some(
                "Use your ZAI key. The catalog includes GLM-5.3-Flash with 1M context, 131,072-token output, and low/high/max effort; model-name context hints such as [1m] are honored, and other entitled models can be set directly with /model."
                    .to_string(),
            ),
            context_window: Some(200_000),
            model_context_windows: {
                let mut m = HashMap::new();
                m.insert("glm-5.3-flash".to_string(), 1_000_000);
                m.insert("glm-5.3-flash[1m]".to_string(), 1_000_000);
                m.insert("glm-5.2".to_string(), 1_000_000);
                m.insert("glm-5.2[1m]".to_string(), 1_000_000);
                m
            },
            model_effort_levels: {
                let mut m = HashMap::new();
                m.insert(
                    "glm-5.3-flash".to_string(),
                    vec!["low".to_string(), "high".to_string(), "max".to_string()],
                );
                m.insert(
                    "glm-5.3-flash[1m]".to_string(),
                    vec!["low".to_string(), "high".to_string(), "max".to_string()],
                );
                m.insert("glm-5.2".to_string(), vec!["high".to_string(), "max".to_string()]);
                m.insert(
                    "glm-5.2[1m]".to_string(),
                    vec!["high".to_string(), "max".to_string()],
                );
                m
            },
            request_contract: Some(RequestContract::AnthropicMessages),
            model_aliases: HashMap::new(),
            model_defaults: ModelSpec::default(),
            model_specs: HashMap::from([
                ("glm-5.3-flash".to_string(), glm_flash_spec.clone()),
                ("glm-5.3-flash[1m]".to_string(), glm_flash_spec),
            ]),
        },
        ProviderProfile {
            id: "chatgpt".to_string(),
            builtin: None,
            display_name: "ChatGPT".to_string(),
            api_provider: ApiProvider::ChatGpt,
            base_url: "https://chatgpt.com/backend-api/codex".to_string(),
            // The ChatGPT backend refuses gpt-5.4 for ChatGPT accounts.
            default_model: CHATGPT_DEFAULT_MODEL.to_string(),
            models: vec![
                "gpt-6.1-sol".to_string(),
                "gpt-6-astra".to_string(),
                "gpt-6-sol".to_string(),
                "gpt-6-luna".to_string(),
                "gpt-5.6-sol".to_string(),
                "gpt-5.6-terra".to_string(),
                "gpt-5.6-luna".to_string(),
                "gpt-5.4".to_string(),
                "gpt-5.4-mini".to_string(),
                "gpt-5.5".to_string(),
                "gpt-5.3-codex".to_string(),
                "gpt-5.3-codex-spark".to_string(),
                "gpt-5-codex".to_string(),
                "gpt-5".to_string(),
                "gpt-5-mini".to_string(),
                "gpt-4.1".to_string(),
                "gpt-4o".to_string(),
                "gpt-4o-mini".to_string(),
                "o3".to_string(),
                "o3-mini".to_string(),
                "o4-mini".to_string(),
            ],
            env_vars: vec!["CHATGPT_ACCESS_TOKEN".to_string()],
            requires_api_key: true,
            login_url: Some("https://chatgpt.com".to_string()),
            oauth_flow: Some(OAuthFlow {
                authorize_url: "https://auth.openai.com/oauth/authorize".to_string(),
                token_url: "https://auth.openai.com/oauth/token".to_string(),
                client_id: "app_EMoamEEZ73f0CkXaXp7hrann".to_string(),
                scope: "openid profile email offline_access".to_string(),
                audience: String::new(),
                redirect_uri: Some("http://localhost:1455/auth/callback".to_string()),
                callback_host: Some("127.0.0.1".to_string()),
                protocol: OAuthProtocol::Generic,
            }),
            notes: Some(
                "ChatGPT/Codex login opens an OpenAI OAuth flow; paste the callback URL or authorization code if browser callback is unavailable."
                    .to_string(),
            ),
            context_window: Some(272_000),
            model_context_windows: {
                let mut m = HashMap::new();
                m.insert("gpt-4.1".to_string(), 1_000_000);
                m.insert("gpt-4o".to_string(), 128_000);
                m.insert("gpt-4o-mini".to_string(), 128_000);
                m
            },
            model_effort_levels: HashMap::new(),
            request_contract: Some(RequestContract::ChatGptResponses),
            model_aliases: HashMap::from([(
                "gpt-5.6".to_string(),
                "gpt-5.6-sol".to_string(),
            )]),
            model_defaults: ModelSpec::default(),
            model_specs: openai_responses_model_specs(false, false),
        },
        ProviderProfile {
            id: "openai".to_string(),
            builtin: None,
            display_name: "OpenAI API".to_string(),
            api_provider: ApiProvider::OpenAi,
            base_url: "https://api.openai.com".to_string(),
            default_model: "gpt-5".to_string(),
            models: vec![
                "gpt-6.1-sol".to_string(),
                "gpt-6-astra".to_string(),
                "gpt-6-sol".to_string(),
                "gpt-6-luna".to_string(),
                "gpt-5.6".to_string(),
                "gpt-5.6-sol".to_string(),
                "gpt-5.6-terra".to_string(),
                "gpt-5.6-luna".to_string(),
                "gpt-5".to_string(),
                "gpt-5-mini".to_string(),
                "gpt-4.1".to_string(),
                "gpt-4.1-mini".to_string(),
                "gpt-4o".to_string(),
                "gpt-4o-mini".to_string(),
                "o3".to_string(),
                "o3-mini".to_string(),
                "o4-mini".to_string(),
            ],
            env_vars: vec!["OPENAI_API_KEY".to_string()],
            requires_api_key: true,
            login_url: Some("https://platform.openai.com/api-keys".to_string()),
            oauth_flow: None,
            notes: Some(
                "Use an OpenAI Platform API key (not ChatGPT OAuth). GPT-5.6 is the official alias for GPT-5.6 Sol."
                    .to_string(),
            ),
            context_window: Some(400_000),
            model_context_windows: {
                let mut m = HashMap::new();
                m.insert("gpt-4.1".to_string(), 1_000_000);
                m.insert("gpt-4.1-mini".to_string(), 1_000_000);
                m.insert("gpt-4o".to_string(), 128_000);
                m.insert("gpt-4o-mini".to_string(), 128_000);
                m
            },
            model_effort_levels: HashMap::new(),
            request_contract: Some(RequestContract::OpenAiChatCompletions),
            model_aliases: HashMap::from([
                ("gpt56".to_string(), "gpt-5.6".to_string()),
                ("gpt56sol".to_string(), "gpt-5.6-sol".to_string()),
                ("gpt56terra".to_string(), "gpt-5.6-terra".to_string()),
                ("gpt56luna".to_string(), "gpt-5.6-luna".to_string()),
                ("gpt61sol".to_string(), "gpt-6.1-sol".to_string()),
                ("gpt6astra".to_string(), "gpt-6-astra".to_string()),
                ("gpt6sol".to_string(), "gpt-6-sol".to_string()),
                ("gpt6luna".to_string(), "gpt-6-luna".to_string()),
            ]),
            model_defaults: ModelSpec::default(),
            model_specs: openai_responses_model_specs(true, true),
        },
        ProviderProfile {
            id: "anthropic".to_string(),
            builtin: Some(ANTHROPIC_BUILTIN_PROFILE_MARKER.to_string()),
            display_name: "Anthropic".to_string(),
            api_provider: ApiProvider::Anthropic,
            base_url: ANTHROPIC_BASE_URL.to_string(),
            default_model: "claude-sonnet-4-6".to_string(),
            models: vec![
                "claude-sonnet-4-6".to_string(),
                "claude-sonnet-5".to_string(),
                "claude-opus-5-5".to_string(),
                "claude-opus-5".to_string(),
                "claude-opus-4-8".to_string(),
                "claude-opus-4-7".to_string(),
                "claude-opus-4-6".to_string(),
                "claude-fable-5-1".to_string(),
                "claude-fable-5".to_string(),
                "claude-sonnet-4-5".to_string(),
                "claude-opus-4-1".to_string(),
                "claude-opus-4-0".to_string(),
                "claude-haiku-4-5".to_string(),
                "claude-3-5-haiku-latest".to_string(),
            ],
            env_vars: vec!["ANTHROPIC_API_KEY".to_string()],
            requires_api_key: true,
            login_url: Some("https://console.anthropic.com/settings/keys".to_string()),
            oauth_flow: Some(OAuthFlow {
                authorize_url: "https://claude.ai/oauth/authorize".to_string(),
                token_url: "https://platform.claude.com/v1/oauth/token".to_string(),
                client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e".to_string(),
                scope: "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload".to_string(),
                audience: String::new(),
                redirect_uri: Some("http://localhost:53692/callback".to_string()),
                callback_host: Some("127.0.0.1".to_string()),
                protocol: OAuthProtocol::AnthropicClaude,
            }),
            notes: Some("Claude Pro/Max subscription OAuth is the default /login flow. ANTHROPIC_API_KEY continues to use standard API-key billing.".to_string()),
            context_window: Some(200_000),
            model_context_windows: HashMap::from([
                ("claude-sonnet-5".to_string(), 1_000_000),
                ("claude-opus-5-5".to_string(), 1_000_000),
                ("claude-opus-5".to_string(), 1_000_000),
                ("claude-fable-5-1".to_string(), 1_000_000),
                ("claude-fable-5".to_string(), 1_000_000),
            ]),
            model_effort_levels: HashMap::new(),
            request_contract: Some(RequestContract::AnthropicMessages),
            model_aliases: HashMap::new(),
            model_defaults: ModelSpec::default(),
            model_specs: HashMap::from([
                (
                    "claude-sonnet-5".to_string(),
                    ModelSpec {
                        context_window: Some(1_000_000),
                        max_output_tokens: Some(128_000),
                        capabilities: ModelCapabilities {
                            tools: Some(true),
                            reasoning: Some(true),
                            image_input: Some(true),
                            prompt_cache: Some(true),
                        },
                        ..Default::default()
                    },
                ),
                (
                    "claude-opus-5-5".to_string(),
                    ModelSpec {
                        context_window: Some(1_000_000),
                        max_output_tokens: Some(128_000),
                        effort_levels: ["low", "medium", "high", "max"]
                            .into_iter()
                            .map(str::to_string)
                            .collect(),
                        reasoning_modes: Vec::new(),
                        capabilities: ModelCapabilities {
                            tools: Some(true),
                            reasoning: Some(true),
                            image_input: Some(true),
                            prompt_cache: Some(true),
                        },
                        pricing: None,
                    },
                ),
                (
                    "claude-fable-5-1".to_string(),
                    ModelSpec {
                        context_window: Some(1_000_000),
                        max_output_tokens: Some(128_000),
                        effort_levels: ["low", "medium", "high", "max"]
                            .into_iter()
                            .map(str::to_string)
                            .collect(),
                        reasoning_modes: Vec::new(),
                        capabilities: ModelCapabilities {
                            tools: Some(true),
                            reasoning: Some(true),
                            image_input: Some(true),
                            prompt_cache: Some(true),
                        },
                        pricing: None,
                    },
                ),
            ]),
        },
        ProviderProfile {
            id: "kimi".to_string(),
            builtin: Some(KIMI_BUILTIN_PROFILE_MARKER.to_string()),
            display_name: "Kimi Code".to_string(),
            api_provider: ApiProvider::Anthropic,
            base_url: KIMI_CODE_BASE_URL.to_string(),
            default_model: "k3".to_string(),
            models: vec![
                "k3".to_string(),
                "k2p7".to_string(),
                "kimi-for-coding".to_string(),
                "kimi-for-coding-highspeed".to_string(),
                "kimi-k2-thinking".to_string(),
            ],
            env_vars: vec!["KIMI_API_KEY".to_string()],
            requires_api_key: true,
            login_url: Some("https://www.kimi.com/code/console".to_string()),
            oauth_flow: None,
            notes: Some(
                "Kimi Code plan provider. /login kimi opens the Kimi Code console to create an API key; KIMI_API_KEY is not the separately billed MOONSHOT_API_KEY."
                    .to_string(),
            ),
            context_window: Some(262_144),
            model_context_windows: HashMap::from([("k3".to_string(), 1_048_576)]),
            model_effort_levels: HashMap::from([("k3".to_string(), vec!["max".to_string()])]),
            request_contract: Some(RequestContract::AnthropicMessages),
            model_aliases: HashMap::new(),
            model_defaults: ModelSpec {
                context_window: Some(262_144),
                max_output_tokens: Some(32_768),
                effort_levels: Vec::new(),
                reasoning_modes: Vec::new(),
                capabilities: ModelCapabilities {
                    tools: Some(true),
                    reasoning: Some(true),
                    image_input: Some(true),
                    prompt_cache: None,
                },
                pricing: None,
            },
            model_specs: HashMap::from([(
                "k3".to_string(),
                ModelSpec {
                    context_window: Some(1_048_576),
                    max_output_tokens: Some(131_072),
                    effort_levels: vec!["max".to_string()],
                    reasoning_modes: Vec::new(),
                    capabilities: ModelCapabilities {
                        tools: Some(true),
                        reasoning: Some(true),
                        image_input: Some(true),
                        prompt_cache: None,
                    },
                    pricing: None,
                },
            )]),
        },
        ProviderProfile {
            id: "deepseek".to_string(),
            builtin: None,
            display_name: "DeepSeek".to_string(),
            api_provider: ApiProvider::OpenAi,
            base_url: "https://api.deepseek.com".to_string(),
            default_model: "deepseek-flash".to_string(),
            models: vec!["deepseek-flash".to_string(), "deepseek-v4-pro".to_string()],
            env_vars: vec!["DEEPSEEK_API_KEY".to_string()],
            requires_api_key: true,
            login_url: Some("https://platform.deepseek.com/api_keys".to_string()),
            oauth_flow: None,
            notes: Some("DeepSeek V4.1 Flash / V4 Pro: 1M context, up to 393,216 output tokens, low/high/max thinking and tool calls. Catalog costs use conservative peak rates; off-peak billing is half. Retired model selections migrate to current names.".to_string()),
            context_window: Some(1_000_000),
            model_context_windows: HashMap::new(),
            model_effort_levels: HashMap::new(),
            request_contract: Some(RequestContract::OpenAiChatCompletions),
            model_aliases: HashMap::from([
                ("deepseek-chat".to_string(), "deepseek-flash".to_string()),
                ("deepseek-reasoner".to_string(), "deepseek-v4-pro".to_string()),
                ("deepseek-v4-flash".to_string(), "deepseek-flash".to_string()),
                ("deepseek-v4-flash-vision-exp".to_string(), "deepseek-flash".to_string()),
            ]),
            model_defaults: ModelSpec::default(),
            model_specs: ["deepseek-flash", "deepseek-v4-pro"]
                .into_iter()
                .map(|model| (model.to_string(), ModelSpec {
                    context_window: Some(1_000_000),
                    max_output_tokens: Some(393_216),
                    effort_levels: ["low", "high", "max"].into_iter().map(str::to_string).collect(),
                    capabilities: ModelCapabilities {
                        tools: Some(true), reasoning: Some(true),
                        image_input: Some(model == "deepseek-flash"), prompt_cache: Some(true),
                    },
                    ..ModelSpec::default()
                }))
                .collect(),
        },
        ProviderProfile {
            id: "local".to_string(),
            builtin: None,
            display_name: "Local llama.cpp".to_string(),
            api_provider: ApiProvider::OpenAi,
            base_url: "http://127.0.0.1:8080".to_string(),
            default_model: DEFAULT_LOCAL_MODEL.to_string(),
            models: vec![DEFAULT_LOCAL_MODEL.to_string()],
            env_vars: Vec::new(),
            requires_api_key: false,
            login_url: None,
            oauth_flow: None,
            notes: Some("Local OpenAI-compatible llama.cpp server. Start one server on 127.0.0.1:8080 and select its model alias; Qwen3.8 selects qwen3.8-27b-ud-q5_k_xl when that server id is configured. No cloud credentials are used. Dext probes llama.cpp for the live runtime context window.".to_string()),
            context_window: None,
            model_context_windows: HashMap::new(),
            model_effort_levels: HashMap::new(),
            request_contract: Some(RequestContract::OpenAiChatCompletions),
            model_aliases: HashMap::from([(
                "qwen3.8".to_string(),
                "qwen3.8-27b-ud-q5_k_xl".to_string(),
            )]),
            model_defaults: ModelSpec::default(),
            model_specs: HashMap::new(),
        },
    ];
    hydrate_builtin_model_specs(&mut profiles);
    profiles
}

pub fn is_deepseek_provider(provider_id: &str, api_provider: ApiProvider, base_url: &str) -> bool {
    api_provider == ApiProvider::OpenAi
        && (canonical_provider_id(provider_id) == "deepseek"
            || url::Url::parse(base_url.trim()).is_ok_and(|url| {
                url.host_str()
                    .is_some_and(|host| host.eq_ignore_ascii_case("api.deepseek.com"))
            }))
}

pub fn is_local_llama_provider(
    provider_id: &str,
    api_provider: ApiProvider,
    base_url: &str,
) -> bool {
    if api_provider != ApiProvider::OpenAi
        || is_deepseek_provider(provider_id, api_provider, base_url)
    {
        return false;
    }
    if canonical_provider_id(provider_id) == "local" {
        return true;
    }
    let Ok(url) = url::Url::parse(base_url.trim()) else {
        return false;
    };
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

pub(crate) fn context_field_score(key: &str) -> Option<u8> {
    let normalized = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    match normalized.as_str() {
        "nctx" | "ctxsize" | "contextsize" | "contextlength" | "contextwindow" => Some(0),
        "maxcontextlength" | "maxcontexttokens" | "contexttokens" => Some(1),
        "nctxtrain" => Some(3),
        _ => None,
    }
}

pub(crate) fn collect_llama_context_candidates(value: &Value, best: &mut Option<(u8, u64)>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if let Some(score) = context_field_score(key)
                    && let Some(tokens) = child
                        .as_u64()
                        .or_else(|| child.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
                        .filter(|tokens| (512..=10_000_000).contains(tokens))
                    && best.is_none_or(|(best_score, best_tokens)| {
                        score < best_score || (score == best_score && tokens > best_tokens)
                    })
                {
                    *best = Some((score, tokens));
                }
                collect_llama_context_candidates(child, best);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_llama_context_candidates(child, best);
            }
        }
        _ => {}
    }
}

pub fn parse_llama_context_window(value: &Value) -> Option<u64> {
    let mut best = None;
    collect_llama_context_candidates(value, &mut best);
    best.map(|(_, tokens)| tokens)
}

pub(crate) fn ordered_model_entries<T>(
    profile: &ProviderProfile,
    entries: HashMap<String, T>,
) -> Vec<(String, T)> {
    let mut entries = entries.into_iter().collect::<Vec<_>>();
    if profile.id == "deepseek" {
        // Alias metadata fills omissions; exact canonical keys win conflicts.
        entries.sort_by_cached_key(|(model, _)| {
            let canonical = normalize_provider_model_value(profile, model);
            (model == &canonical, model.clone())
        });
    }
    entries
}

pub fn normalize_provider_profile(mut profile: ProviderProfile) -> Option<ProviderProfile> {
    profile.id = canonical_provider_id(&profile.id);
    if profile.id.trim().is_empty() {
        return None;
    }
    profile.builtin = match (profile.id.as_str(), profile.builtin.as_deref()) {
        ("kimi", Some(KIMI_BUILTIN_PROFILE_MARKER)) => {
            Some(KIMI_BUILTIN_PROFILE_MARKER.to_string())
        }
        ("anthropic", Some(ANTHROPIC_BUILTIN_PROFILE_MARKER)) => {
            Some(ANTHROPIC_BUILTIN_PROFILE_MARKER.to_string())
        }
        _ => None,
    };

    let fallback_model = if profile.id == "local" {
        DEFAULT_LOCAL_MODEL
    } else {
        "glm-5.2[1m]"
    };
    profile.base_url = profile.base_url.trim().trim_end_matches('/').to_string();
    if profile.display_name.trim().is_empty() {
        profile.display_name = profile.id.clone();
    }
    profile.request_contract = Some(request_contract_for_profile(&profile));
    profile.api_provider = request_contract_for_profile(&profile).api_provider();
    let mut normalized_aliases = HashMap::new();
    for (alias, target) in std::mem::take(&mut profile.model_aliases) {
        let alias = normalize_model_alias_key(&alias);
        let target = if request_contract_for_profile(&profile) == RequestContract::ChatGptResponses
        {
            normalize_chatgpt_model_slug(&target)
        } else if profile.id == "deepseek" {
            canonical_deepseek_model(&target)
                .unwrap_or(target.trim())
                .to_string()
        } else {
            target.trim().to_string()
        };
        if !alias.is_empty() && !target.is_empty() && alias != target.to_ascii_lowercase() {
            normalized_aliases.insert(alias, target);
        }
    }
    profile.model_aliases = normalized_aliases;
    if profile.default_model.trim().is_empty() {
        profile.default_model = fallback_model.to_string();
    }
    profile.default_model = normalize_provider_model_value(&profile, &profile.default_model);
    let mut seen_models = HashSet::new();
    let mut models = Vec::new();
    for model in std::mem::take(&mut profile.models) {
        let normalized = normalize_provider_model_value(&profile, &model);
        if normalized.is_empty() {
            continue;
        }
        if seen_models.insert(normalized.to_ascii_lowercase()) {
            models.push(normalized);
        }
    }
    if !models.iter().any(|m| m == &profile.default_model) {
        models.insert(0, profile.default_model.clone());
    }
    profile.models = models;

    let mut normalized_context_windows = HashMap::new();
    let entries = std::mem::take(&mut profile.model_context_windows);
    for (model, window) in ordered_model_entries(&profile, entries) {
        if window == 0 {
            continue;
        }
        let key = normalize_provider_model_value(&profile, &model).to_ascii_lowercase();
        if !key.is_empty() {
            normalized_context_windows.insert(key, window);
        }
    }
    profile.model_context_windows = normalized_context_windows;

    let mut normalized_effort_levels = HashMap::new();
    let entries = std::mem::take(&mut profile.model_effort_levels);
    for (model, levels) in ordered_model_entries(&profile, entries) {
        let key = normalize_provider_model_value(&profile, &model).to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        let mut seen_levels = HashSet::new();
        let levels = levels
            .into_iter()
            .map(|level| level.trim().to_ascii_lowercase())
            .filter(|level| !level.is_empty())
            .filter(|level| seen_levels.insert(level.clone()))
            .collect::<Vec<_>>();
        if !levels.is_empty() {
            normalized_effort_levels.insert(key, levels);
        }
    }
    profile.model_effort_levels = normalized_effort_levels;

    profile.model_defaults = normalize_model_spec(profile.model_defaults);
    let mut normalized_model_specs = HashMap::new();
    let entries = std::mem::take(&mut profile.model_specs);
    for (model, spec) in ordered_model_entries(&profile, entries) {
        let key = normalize_provider_model_value(&profile, &model).to_ascii_lowercase();
        if !key.is_empty() {
            let spec = normalize_model_spec(spec);
            if profile.id == "deepseek" {
                merge_model_spec(normalized_model_specs.entry(key).or_default(), spec);
            } else {
                normalized_model_specs.insert(key, spec);
            }
        }
    }
    profile.model_specs = normalized_model_specs;

    let mut seen_env = HashSet::new();
    profile.env_vars = profile
        .env_vars
        .into_iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .filter(|v| seen_env.insert(v.clone()))
        .collect();

    Some(profile)
}

pub(crate) fn refresh_retired_builtin_pricing(
    provider_id: &str,
    model: &str,
    spec: &mut ModelSpec,
) {
    if canonical_provider_id(provider_id) != "anthropic" {
        return;
    }
    let model = model.to_ascii_lowercase();
    let retired = if model == "claude-fable-5-1" {
        Some(model_pricing(10.0, 50.0, 1.0, 12.5))
    } else if matches!(
        model.as_str(),
        "claude-opus-4-5" | "claude-opus-4-6" | "claude-opus-4-7" | "claude-opus-4-8"
    ) {
        Some(model_pricing(15.0, 75.0, 1.5, 18.75))
    } else {
        None
    };
    if spec.pricing.as_ref() == retired.as_ref() {
        spec.pricing = None;
    }
}

pub(crate) fn merge_provider_profile_with_migrations(
    mut builtin: ProviderProfile,
    mut stored: ProviderProfile,
    refresh_retired_generated_prices: bool,
) -> ProviderProfile {
    let builtin_id = canonical_provider_id(&builtin.id);
    let stored_default = stored.default_model.trim();
    if !stored_default.is_empty() {
        builtin.default_model = normalize_provider_model_value(&builtin, stored_default);
    }
    if let Some(contract) = stored.request_contract {
        builtin.request_contract = Some(contract);
        builtin.api_provider = contract.api_provider();
    }
    for (alias, target) in stored.model_aliases {
        builtin.model_aliases.insert(alias, target);
    }
    if builtin_id == "deepseek" {
        for (model, window) in &stored.model_context_windows {
            if *window > 0 {
                let key = normalize_provider_model_value(&builtin, model).to_ascii_lowercase();
                stored
                    .model_specs
                    .entry(key)
                    .or_default()
                    .context_window
                    .get_or_insert(*window);
            }
        }
        for (model, levels) in &stored.model_effort_levels {
            if !levels.is_empty() {
                let key = normalize_provider_model_value(&builtin, model).to_ascii_lowercase();
                let spec = stored.model_specs.entry(key).or_default();
                if spec.effort_levels.is_empty() {
                    spec.effort_levels = levels.clone();
                }
            }
        }
    }
    merge_model_spec(&mut builtin.model_defaults, stored.model_defaults);
    for (model, mut spec) in stored.model_specs {
        if refresh_retired_generated_prices {
            refresh_retired_builtin_pricing(&builtin_id, &model, &mut spec);
        }
        let model = if builtin_id == "deepseek" {
            normalize_provider_model_value(&builtin, &model).to_ascii_lowercase()
        } else {
            model
        };
        merge_model_spec(builtin.model_specs.entry(model).or_default(), spec);
    }

    if let Some(window) = stored.context_window.filter(|window| *window > 0) {
        builtin.context_window = Some(window);
    }

    for (model, window) in stored.model_context_windows {
        if window == 0 {
            continue;
        }
        let key = normalize_provider_model_value(&builtin, &model).to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        builtin.model_context_windows.insert(key, window);
    }

    for (model, levels) in stored.model_effort_levels {
        let key = normalize_provider_model_value(&builtin, &model).to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        let mut seen_levels = HashSet::new();
        let levels = levels
            .into_iter()
            .map(|level| level.trim().to_ascii_lowercase())
            .filter(|level| !level.is_empty())
            .filter(|level| seen_levels.insert(level.clone()))
            .collect::<Vec<_>>();
        if !levels.is_empty() {
            builtin.model_effort_levels.insert(key, levels);
        }
    }

    let builtin_owned: HashSet<String> = built_in_provider_profiles()
        .into_iter()
        .flat_map(|profile| {
            profile
                .models
                .into_iter()
                .chain(std::iter::once(profile.default_model))
        })
        .map(|model| model.to_ascii_lowercase())
        .collect();
    let mut seen_models: HashSet<String> = builtin
        .models
        .iter()
        .map(|model| model.to_ascii_lowercase())
        .collect();
    let chatgpt_route = request_contract_for_profile(&builtin) == RequestContract::ChatGptResponses;
    let extra_models = stored
        .models
        .into_iter()
        .map(|model| {
            if chatgpt_route {
                normalize_chatgpt_model_slug(&model)
            } else if builtin_id == "deepseek" {
                normalize_provider_model_value(&builtin, &model)
            } else {
                model.trim().to_string()
            }
        })
        .filter(|model| !model.is_empty())
        .filter(|model| {
            builtin_id == "chatgpt"
                || model
                    .to_ascii_lowercase()
                    .starts_with(&format!("{builtin_id}-"))
                || !builtin_owned.contains(&model.to_ascii_lowercase())
        })
        .filter(|model| seen_models.insert(model.to_ascii_lowercase()))
        .collect::<Vec<_>>();
    builtin.models.extend(extra_models);

    if !builtin
        .models
        .iter()
        .any(|model| model == &builtin.default_model)
    {
        builtin.models.insert(0, builtin.default_model.clone());
    }
    builtin
}

#[cfg(any(test, feature = "test-support"))]
pub fn merge_provider_profile(
    builtin: ProviderProfile,
    stored: ProviderProfile,
) -> ProviderProfile {
    merge_provider_profile_with_migrations(builtin, stored, false)
}

pub(crate) fn validate_kimi_profile_provenance(catalog: &ProviderCatalog) -> Result<()> {
    if let Some(profile) = catalog.providers.iter().find(|profile| {
        canonical_provider_id(&profile.id) == "kimi"
            && profile.builtin.as_deref() != Some(KIMI_BUILTIN_PROFILE_MARKER)
    }) {
        anyhow::bail!(
            "provider id '{}' conflicts with the built-in Kimi Code provider; rename the custom profile before upgrading (reserved ids: kimi, kimi-code, kimi-coding, kimi-membership)",
            profile.id
        );
    }
    Ok(())
}

pub fn normalize_provider_catalog(mut catalog: ProviderCatalog) -> Result<ProviderCatalog> {
    if catalog.version == 0 || catalog.version > PROVIDER_CATALOG_VERSION {
        anyhow::bail!(
            "unsupported provider catalog version {} (supported: 1-{PROVIDER_CATALOG_VERSION})",
            catalog.version
        );
    }
    validate_kimi_profile_provenance(&catalog)?;
    let legacy_catalog = catalog.version < STRUCTURED_PROVIDER_CATALOG_VERSION;
    let refresh_retired_generated_prices = catalog.version < 3;
    let refresh_deepseek = catalog.version < 4;
    let mut stored_by_id: HashMap<String, ProviderProfile> = HashMap::new();
    // Custom profiles keep the order they were first written in, not HashMap order.
    let mut stored_order: Vec<String> = Vec::new();
    let mut providers: Vec<ProviderProfile> = Vec::new();
    let builtin_ids: HashSet<String> = built_in_provider_profiles()
        .into_iter()
        .map(|profile| canonical_provider_id(&profile.id))
        .collect();

    for mut profile in catalog.providers.drain(..) {
        if refresh_deepseek && canonical_provider_id(&profile.id) == "deepseek" {
            if profile.context_window == Some(128_000) {
                profile.context_window = None;
            }
            if profile.model_defaults.max_output_tokens == Some(8_192) {
                profile.model_defaults.max_output_tokens = None;
            }
            for (model, spec) in &mut profile.model_specs {
                let model = model.trim().to_ascii_lowercase();
                if matches!(model.as_str(), "deepseek-chat" | "deepseek-reasoner") {
                    if spec.context_window == Some(128_000) {
                        spec.context_window = None;
                    }
                    if spec.max_output_tokens == Some(8_192) {
                        spec.max_output_tokens = None;
                    }
                    if model == "deepseek-chat" && spec.capabilities.reasoning == Some(false) {
                        spec.capabilities.reasoning = None;
                    }
                    let retired = if model == "deepseek-chat" {
                        model_pricing(0.27, 1.1, 0.07, 0.27)
                    } else {
                        model_pricing(0.55, 2.19, 0.14, 0.55)
                    };
                    if spec.pricing.as_ref() == Some(&retired) {
                        spec.pricing = None;
                    }
                }
            }
            profile.model_context_windows.retain(|model, window| {
                !(*window == 128_000
                    && matches!(
                        model.trim().to_ascii_lowercase().as_str(),
                        "deepseek-chat" | "deepseek-reasoner"
                    ))
            });
        }
        if legacy_catalog {
            profile.request_contract = None;
            profile.model_aliases.clear();
            profile.model_defaults = ModelSpec::default();
            profile.model_specs.clear();
        }
        if let Some(profile) = normalize_provider_profile(profile) {
            let id = canonical_provider_id(&profile.id);
            if !stored_by_id.contains_key(&id) {
                stored_order.push(id.clone());
            }
            stored_by_id.insert(id, profile);
        }
    }

    for builtin in built_in_provider_profiles() {
        if let Some(builtin) = normalize_provider_profile(builtin) {
            let id = canonical_provider_id(&builtin.id);
            let merged = match stored_by_id.remove(&id) {
                Some(stored) => merge_provider_profile_with_migrations(
                    builtin,
                    stored,
                    refresh_retired_generated_prices,
                ),
                None => builtin,
            };
            providers.push(merged);
        }
    }

    for id in stored_order {
        let Some(profile) = stored_by_id.remove(&id) else {
            continue;
        };
        if builtin_ids.contains(&id) {
            continue;
        }
        if RETIRED_BUNDLED_PROVIDER_IDS.contains(&id.as_str()) {
            continue;
        }
        providers.push(profile);
    }

    if providers.is_empty() {
        providers = built_in_provider_profiles()
            .into_iter()
            .filter_map(normalize_provider_profile)
            .collect();
    }

    let active = canonical_provider_id(&catalog.active_provider);
    let active_exists = providers
        .iter()
        .any(|p| canonical_provider_id(&p.id) == active);
    let active_provider = if active_exists {
        active
    } else if providers
        .iter()
        .any(|p| canonical_provider_id(&p.id) == "glm")
    {
        "glm".to_string()
    } else {
        providers
            .first()
            .map(|p| p.id.clone())
            .unwrap_or_else(default_active_provider)
    };

    Ok(ProviderCatalog {
        version: PROVIDER_CATALOG_VERSION,
        active_provider,
        providers,
    })
}

pub fn default_provider_catalog() -> ProviderCatalog {
    normalize_provider_catalog(ProviderCatalog {
        version: PROVIDER_CATALOG_VERSION,
        active_provider: default_active_provider(),
        providers: built_in_provider_profiles(),
    })
    .expect("built-in provider catalog must be valid")
}

pub fn find_provider_profile(catalog: &ProviderCatalog, id: &str) -> Option<ProviderProfile> {
    let wanted = canonical_provider_id(id);
    catalog
        .providers
        .iter()
        .find(|p| canonical_provider_id(&p.id) == wanted)
        .cloned()
}

pub fn request_contract_for_profile(profile: &ProviderProfile) -> RequestContract {
    profile
        .request_contract
        .unwrap_or_else(|| RequestContract::for_api_provider(profile.api_provider))
}

pub fn is_gpt_5_6_model(model: &str) -> bool {
    matches!(
        model.trim().to_ascii_lowercase().as_str(),
        "gpt-5.6" | "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna"
    )
}

pub fn is_gpt_6_model(model: &str) -> bool {
    matches!(
        model.trim().to_ascii_lowercase().as_str(),
        "gpt-6.1-sol" | "gpt-6-astra" | "gpt-6-sol" | "gpt-6-luna"
    )
}

pub fn is_gpt_6_sol_or_luna_model(model: &str) -> bool {
    matches!(
        model.trim().to_ascii_lowercase().as_str(),
        "gpt-6-sol" | "gpt-6-luna"
    )
}

pub fn is_openai_responses_model(model: &str) -> bool {
    is_gpt_5_6_model(model) || is_gpt_6_model(model)
}

pub(crate) fn is_official_openai_base_url(base_url: &str) -> bool {
    let Ok(url) = url::Url::parse(base_url.trim()) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str() == Some("api.openai.com")
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && matches!(url.path().trim_end_matches('/'), "" | "/v1")
}

pub fn official_openai_responses_model(
    profile: &ProviderProfile,
    base_url: &str,
    model: &str,
) -> bool {
    canonical_provider_id(&profile.id) == "openai"
        && is_official_openai_base_url(base_url)
        && is_openai_responses_model(model)
}

pub fn effective_request_contract(
    profile: &ProviderProfile,
    base_url: &str,
    model: &str,
) -> RequestContract {
    let configured = request_contract_for_profile(profile);
    if configured == RequestContract::OpenAiChatCompletions
        && official_openai_responses_model(profile, base_url, model)
    {
        RequestContract::OpenAiResponses
    } else {
        configured
    }
}

pub(crate) fn normalize_model_alias_key(raw: &str) -> String {
    raw.trim().to_ascii_lowercase().replace(['_', ' '], "-")
}

pub(crate) fn normalize_effort_levels(levels: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    levels
        .into_iter()
        .map(|level| level.trim().to_ascii_lowercase())
        .filter(|level| !level.is_empty())
        .filter(|level| seen.insert(level.clone()))
        .collect()
}

pub(crate) fn merge_model_spec(base: &mut ModelSpec, overlay: ModelSpec) {
    if overlay.context_window.is_some() {
        base.context_window = overlay.context_window;
    }
    if overlay.max_output_tokens.is_some() {
        base.max_output_tokens = overlay.max_output_tokens;
    }
    if !overlay.effort_levels.is_empty() {
        base.effort_levels = overlay.effort_levels;
    }
    if !overlay.reasoning_modes.is_empty() {
        base.reasoning_modes = overlay.reasoning_modes;
    }
    for (target, value) in [
        (&mut base.capabilities.tools, overlay.capabilities.tools),
        (
            &mut base.capabilities.reasoning,
            overlay.capabilities.reasoning,
        ),
        (
            &mut base.capabilities.image_input,
            overlay.capabilities.image_input,
        ),
        (
            &mut base.capabilities.prompt_cache,
            overlay.capabilities.prompt_cache,
        ),
    ] {
        if value.is_some() {
            *target = value;
        }
    }
    if overlay.pricing.is_some() {
        base.pricing = overlay.pricing;
    }
}

pub(crate) fn normalize_model_spec(mut spec: ModelSpec) -> ModelSpec {
    spec.context_window = spec.context_window.filter(|value| *value > 0);
    spec.max_output_tokens = spec.max_output_tokens.filter(|value| *value > 0);
    spec.effort_levels = normalize_effort_levels(spec.effort_levels);
    spec.reasoning_modes = normalize_effort_levels(spec.reasoning_modes);
    spec.pricing = spec.pricing.filter(|pricing| {
        [
            pricing.input_usd_per_mtok,
            pricing.output_usd_per_mtok,
            pricing.cache_read_usd_per_mtok,
            pricing.cache_create_usd_per_mtok,
        ]
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0)
    });
    spec
}

pub fn resolve_model_spec(profile: &ProviderProfile, model: &str) -> ResolvedModelSpec {
    let model = normalize_provider_model_value(profile, model).to_ascii_lowercase();
    let explicit = profile.model_specs.get(&model);
    let defaults = &profile.model_defaults;
    let capability = |model_value: Option<bool>, default_value: Option<bool>, fallback: bool| {
        model_value.or(default_value).unwrap_or(fallback)
    };
    let effort_levels = explicit
        .filter(|spec| !spec.effort_levels.is_empty())
        .map(|spec| spec.effort_levels.clone())
        .or_else(|| {
            profile
                .model_effort_levels
                .get(&model)
                .filter(|levels| !levels.is_empty())
                .cloned()
        })
        .unwrap_or_else(|| defaults.effort_levels.clone());
    let reasoning_modes = explicit
        .filter(|spec| !spec.reasoning_modes.is_empty())
        .map(|spec| spec.reasoning_modes.clone())
        .unwrap_or_else(|| defaults.reasoning_modes.clone());
    let contract = request_contract_for_profile(profile);
    ResolvedModelSpec {
        context_window: explicit
            .and_then(|spec| spec.context_window)
            .or_else(|| profile.model_context_windows.get(&model).copied())
            .or(defaults.context_window)
            .or(profile.context_window),
        max_output_tokens: explicit
            .and_then(|spec| spec.max_output_tokens)
            .or(defaults.max_output_tokens),
        effort_levels,
        reasoning_modes,
        tools: capability(
            explicit.and_then(|spec| spec.capabilities.tools),
            defaults.capabilities.tools,
            true,
        ),
        reasoning: capability(
            explicit.and_then(|spec| spec.capabilities.reasoning),
            defaults.capabilities.reasoning,
            true,
        ),
        image_input: capability(
            explicit.and_then(|spec| spec.capabilities.image_input),
            defaults.capabilities.image_input,
            false,
        ),
        prompt_cache: capability(
            explicit.and_then(|spec| spec.capabilities.prompt_cache),
            defaults.capabilities.prompt_cache,
            matches!(
                contract,
                RequestContract::OpenAiResponses | RequestContract::ChatGptResponses
            ),
        ),
        pricing: explicit
            .and_then(|spec| spec.pricing.clone())
            .or_else(|| defaults.pricing.clone()),
        source: if explicit.is_some() {
            "model"
        } else if profile.model_specs.is_empty() && profile.model_defaults == ModelSpec::default() {
            "legacy"
        } else {
            "provider"
        },
    }
}

pub fn normalize_provider_model_value(profile: &ProviderProfile, model: &str) -> String {
    let trimmed = model.trim();
    let chatgpt_contract =
        request_contract_for_profile(profile) == RequestContract::ChatGptResponses;
    let normalized_input = if chatgpt_contract {
        normalize_chatgpt_model_slug(trimmed)
    } else {
        trimmed.split_whitespace().collect::<Vec<_>>().join("-")
    };
    let alias_key = normalize_model_alias_key(&normalized_input);
    if let Some(target) = profile.model_aliases.get(&alias_key) {
        return if chatgpt_contract {
            normalize_chatgpt_model_slug(target)
        } else if canonical_provider_id(&profile.id) == "deepseek" {
            canonical_deepseek_model(target)
                .unwrap_or(target.trim())
                .to_string()
        } else {
            target.trim().to_string()
        };
    }
    if canonical_provider_id(&profile.id) == "deepseek"
        && let Some(model) = canonical_deepseek_model(&normalized_input)
    {
        return model.to_string();
    }
    if chatgpt_contract {
        return normalized_input;
    }

    let find_curated = |needle: &str| {
        std::iter::once(profile.default_model.as_str())
            .chain(profile.models.iter().map(String::as_str))
            .find(|candidate| candidate.eq_ignore_ascii_case(needle))
            .map(str::to_string)
    };
    if let Some(found) = find_curated(trimmed) {
        return found;
    }

    let hyphenated = normalized_input;
    if hyphenated != trimmed
        && let Some(found) = find_curated(&hyphenated)
    {
        return found;
    }

    if canonical_provider_id(&profile.id) == "glm"
        && !hyphenated.to_ascii_lowercase().starts_with("glm-")
        && !hyphenated.is_empty()
    {
        let prefixed = format!("glm-{hyphenated}");
        if let Some(found) = find_curated(&prefixed) {
            return found;
        }
        return prefixed;
    }

    if hyphenated.is_empty() {
        trimmed.to_string()
    } else {
        hyphenated
    }
}

pub fn provider_request_url(base_url: &str, contract: RequestContract) -> String {
    let base = base_url.trim_end_matches('/');
    match contract {
        RequestContract::OpenAiChatCompletions => {
            if base.ends_with("/v1") {
                format!("{base}/chat/completions")
            } else {
                format!("{base}/v1/chat/completions")
            }
        }
        RequestContract::OpenAiResponses => {
            if base.ends_with("/v1") {
                format!("{base}/responses")
            } else {
                format!("{base}/v1/responses")
            }
        }
        RequestContract::ChatGptResponses => {
            if base.ends_with("/codex/responses") {
                base.to_string()
            } else if base.ends_with("/codex") {
                format!("{base}/responses")
            } else {
                format!("{base}/codex/responses")
            }
        }
        RequestContract::AnthropicMessages => {
            if base.ends_with("/v1") {
                format!("{base}/messages")
            } else {
                format!("{base}/v1/messages")
            }
        }
    }
}

pub const CHATGPT_REQUEST_WIRE_LIMIT: usize = 1024 * 1024;

pub struct ProviderRequestBody {
    pub bytes: bytes::Bytes,
    pub json_bytes: usize,
    pub gzip: bool,
}

impl ProviderRequestBody {
    pub fn encode(
        contract: RequestContract,
        json: Vec<u8>,
        interrupt: &std::sync::atomic::AtomicBool,
    ) -> Result<Self> {
        let interrupted = || {
            if interrupt.load(std::sync::atomic::Ordering::Relaxed) {
                anyhow::bail!("interrupted by user during provider request encoding");
            }
            Ok(())
        };
        interrupted()?;
        let json_bytes = json.len();
        if contract != RequestContract::ChatGptResponses || json_bytes <= CHATGPT_REQUEST_WIRE_LIMIT
        {
            return Ok(Self {
                bytes: json.into(),
                json_bytes,
                gzip: false,
            });
        }
        // The Codex edge returns an upstream 503 for wire bodies over 1 MiB,
        // including whitespace-only padding. Compress without changing JSON.
        let check_size = |size: usize| {
            if size > CHATGPT_REQUEST_WIRE_LIMIT {
                anyhow::bail!(
                    "ChatGPT request exceeds the {CHATGPT_REQUEST_WIRE_LIMIT}-byte wire limit even with gzip (JSON: {json_bytes} bytes). No request was sent. Use /compact or send fewer/smaller images; restarting or retrying the same history will not reduce its size."
                );
            }
            Ok(())
        };
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        for chunk in json.chunks(64 * 1024) {
            interrupted()?;
            encoder
                .write_all(chunk)
                .context("encoding ChatGPT request as gzip")?;
            check_size(encoder.get_ref().len())?;
        }
        let bytes = encoder.finish().context("finishing ChatGPT request gzip")?;
        interrupted()?;
        check_size(bytes.len())?;
        Ok(Self {
            bytes: bytes.into(),
            json_bytes,
            gzip: true,
        })
    }
}

/// The model a ChatGPT sign-in uses unless one is chosen.
pub const CHATGPT_DEFAULT_MODEL: &str = "gpt-6.1-sol";

pub fn normalize_chatgpt_model_slug(model: &str) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return CHATGPT_DEFAULT_MODEL.to_string();
    }

    let canonical = trimmed.to_ascii_lowercase().replace(['_', ' '], "-");
    if canonical == "auto" {
        return CHATGPT_DEFAULT_MODEL.to_string();
    }

    let compact: String = canonical
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    match compact.as_str() {
        "gpt5" => "gpt-5".to_string(),
        "gpt5mini" => "gpt-5-mini".to_string(),
        "gpt5codex" => "gpt-5-codex".to_string(),
        "gpt54" => "gpt-5.4".to_string(),
        "gpt54mini" => "gpt-5.4-mini".to_string(),
        "gpt55" => "gpt-5.5".to_string(),
        "gpt56" | "gpt56sol" => "gpt-5.6-sol".to_string(),
        "gpt56terra" => "gpt-5.6-terra".to_string(),
        "gpt56luna" => "gpt-5.6-luna".to_string(),
        "gpt61sol" => "gpt-6.1-sol".to_string(),
        "gpt6astra" => "gpt-6-astra".to_string(),
        "gpt6sol" => "gpt-6-sol".to_string(),
        "gpt6luna" => "gpt-6-luna".to_string(),
        "gpt53codex" => "gpt-5.3-codex".to_string(),
        "gpt53codexspark" => "gpt-5.3-codex-spark".to_string(),
        _ => canonical,
    }
}

pub fn chatgpt_reasoning_effort(model: &str, effort: ThinkingEffort) -> Option<&'static str> {
    let raw = effort.as_str();
    if is_gpt_5_6_model(model) {
        return Some(match effort {
            ThinkingEffort::Off => "none",
            ThinkingEffort::Minimal => "low",
            ThinkingEffort::Low => "low",
            ThinkingEffort::Medium => "medium",
            ThinkingEffort::High => "high",
            ThinkingEffort::XHigh | ThinkingEffort::Max => "xhigh",
        });
    }
    if effort == ThinkingEffort::Off {
        return None;
    }
    if effort == ThinkingEffort::Minimal {
        return Some("low");
    }
    if model.starts_with("gpt-5.2")
        || model.starts_with("gpt-5.3")
        || model.starts_with("gpt-5.4")
        || model.starts_with("gpt-5.5")
        || model.starts_with("gpt-5-codex")
    {
        return Some(match raw {
            "minimal" => "low",
            "low" => "low",
            "medium" => "medium",
            "high" => "high",
            "xhigh" | "max" => "xhigh",
            _ => "medium",
        });
    }
    if model == "gpt-5.1-codex-mini" {
        return Some(match raw {
            "high" | "xhigh" | "max" => "high",
            _ => "medium",
        });
    }
    if model.starts_with("gpt-5.1") {
        return Some(match raw {
            "xhigh" | "max" => "high",
            other => other,
        });
    }
    Some(raw)
}

pub fn build_chatgpt_request(
    model: &str,
    reasoning_effort: Option<&str>,
    system_text: &str,
    session_id: &str,
    input: Vec<Value>,
    tools: Vec<Value>,
) -> Value {
    let model = normalize_chatgpt_model_slug(model);
    // The ChatGPT codex backend rejects max_output_tokens with HTTP 400
    // ("Unsupported parameter"), so this request must never carry an output
    // cap — not even from DEXT_MAX_OUTPUT_TOKENS or catalog model specs.
    let mut body = json!({
        "model": model,
        "store": false,
        "stream": true,
        "instructions": system_text,
        "input": input,
        "include": ["reasoning.encrypted_content"],
        "text": { "verbosity": "medium" },
        "prompt_cache_key": session_id,
    });
    if let Some(effort) = reasoning_effort {
        body.as_object_mut()
            .expect("chatgpt request body is always an object")
            .insert(
                "reasoning".to_string(),
                json!({ "effort": effort, "summary": "auto" }),
            );
    }
    if !tools.is_empty() {
        let object = body
            .as_object_mut()
            .expect("chatgpt request body is always an object");
        object.insert("tool_choice".to_string(), json!("auto"));
        object.insert("parallel_tool_calls".to_string(), json!(true));
        object.insert("tools".to_string(), json!(tools));
    }
    body
}

#[derive(Debug, Clone, Copy)]
pub struct OpenAiResponsesReasoning<'a> {
    pub effort: &'a str,
    pub mode: Option<&'a str>,
    pub include_encrypted_content: bool,
}

pub fn build_openai_responses_request(
    model: &str,
    reasoning: Option<OpenAiResponsesReasoning<'_>>,
    system_text: &str,
    prompt_cache_key: &str,
    input: Vec<Value>,
    tools: Vec<Value>,
    max_output_tokens: u32,
) -> Value {
    let mut body = json!({
        "model": model.trim(),
        "store": false,
        "stream": true,
        "instructions": system_text,
        "input": input,
        "max_output_tokens": max_output_tokens,
        "text": { "verbosity": "medium" },
        "prompt_cache_key": prompt_cache_key,
    });
    if let Some(reasoning) = reasoning {
        body["reasoning"] = json!({
            "effort": reasoning.effort,
            "summary": "auto",
        });
        if reasoning.include_encrypted_content {
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
        if let Some(reasoning_mode) = reasoning.mode {
            body["reasoning"]["mode"] = json!(reasoning_mode);
        }
    }
    if !tools.is_empty() {
        let object = body
            .as_object_mut()
            .expect("OpenAI Responses request body is always an object");
        object.insert("tool_choice".to_string(), json!("auto"));
        object.insert("parallel_tool_calls".to_string(), json!(true));
        object.insert("tools".to_string(), json!(tools));
    }
    body
}

pub fn build_chatgpt_summary_request(
    model: &str,
    compact_system: &str,
    user_text: &str,
    reasoning_effort: Option<&str>,
) -> Value {
    let mut body = json!({
        "model": normalize_chatgpt_model_slug(model),
        "store": false,
        "stream": true,
        "instructions": compact_system,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": user_text }],
        }],
        "include": ["reasoning.encrypted_content"],
        "text": { "verbosity": "low" },
        "tool_choice": "none",
        "parallel_tool_calls": false,
    });
    if let Some(reasoning_effort) = reasoning_effort {
        body.as_object_mut()
            .expect("chatgpt summary request body is always an object")
            .insert(
                "reasoning".to_string(),
                json!({ "effort": reasoning_effort, "summary": "auto" }),
            );
    }
    body
}

pub(crate) fn is_marked_builtin_anthropic(profile: &ProviderProfile) -> bool {
    profile.builtin.as_deref() == Some(ANTHROPIC_BUILTIN_PROFILE_MARKER)
        && canonical_provider_id(&profile.id) == "anthropic"
}

pub fn is_builtin_anthropic_profile(profile: &ProviderProfile) -> bool {
    is_marked_builtin_anthropic(profile)
        && request_contract_for_profile(profile) == RequestContract::AnthropicMessages
}

pub fn is_official_anthropic_profile(profile: &ProviderProfile, base_url: &str) -> bool {
    is_builtin_anthropic_profile(profile) && base_url.trim_end_matches('/') == ANTHROPIC_BASE_URL
}

pub fn is_official_kimi_profile(profile: &ProviderProfile, base_url: &str) -> bool {
    profile.builtin.as_deref() == Some(KIMI_BUILTIN_PROFILE_MARKER)
        && canonical_provider_id(&profile.id) == "kimi"
        && request_contract_for_profile(profile) == RequestContract::AnthropicMessages
        && base_url.trim_end_matches('/') == KIMI_CODE_BASE_URL
}

pub fn base64_url_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

pub fn decode_base64_url(input: &str) -> Option<Vec<u8>> {
    let mut bytes: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    while !bytes.len().is_multiple_of(4) {
        bytes.push(b'=');
    }

    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        if chunk.len() != 4 {
            return None;
        }
        let mut vals = [0u8; 4];
        let mut pad = 0usize;
        for (i, byte) in chunk.iter().enumerate() {
            if *byte == b'=' {
                pad += 1;
            } else {
                vals[i] = base64_url_value(*byte)?;
            }
        }
        let n = ((vals[0] as u32) << 18)
            | ((vals[1] as u32) << 12)
            | ((vals[2] as u32) << 6)
            | vals[3] as u32;
        out.push(((n >> 16) & 0xff) as u8);
        if pad < 2 {
            out.push(((n >> 8) & 0xff) as u8);
        }
        if pad < 1 {
            out.push((n & 0xff) as u8);
        }
    }
    Some(out)
}

/// Where a ChatGPT access token (a JWT) names its account: this path in its payload.
/// The `chatgpt-account-id` header carries it.
pub const CHATGPT_ACCOUNT_CLAIM: [&str; 2] = ["https://api.openai.com/auth", "chatgpt_account_id"];

pub fn chatgpt_account_id_from_token(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = decode_base64_url(payload)?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    let [object, field] = CHATGPT_ACCOUNT_CLAIM;
    value.get(object)?.get(field)?.as_str().map(str::to_string)
}

pub fn curated_provider_models(profile: &ProviderProfile) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for candidate in std::iter::once(profile.default_model.as_str())
        .chain(profile.models.iter().map(String::as_str))
    {
        let normalized = normalize_provider_model_value(profile, candidate);
        if normalized.is_empty() {
            continue;
        }
        if seen.insert(normalized.to_ascii_lowercase()) {
            models.push(normalized);
        }
    }
    models
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModelRef {
    pub provider_id: String,
    pub model: String,
}

pub fn provider_id_from_selector(catalog: &ProviderCatalog, selector: &str) -> Result<String> {
    let token = selector.trim();
    if token.is_empty() {
        anyhow::bail!("provider selector cannot be empty");
    }

    if let Ok(index) = token.parse::<usize>() {
        if index == 0 || index > catalog.providers.len() {
            anyhow::bail!(
                "provider index {} out of range 1..={}",
                index,
                catalog.providers.len()
            );
        }
        return Ok(canonical_provider_id(&catalog.providers[index - 1].id));
    }

    let canonical = canonical_provider_id(token);
    if find_provider_profile(catalog, &canonical).is_none() {
        let available = catalog
            .providers
            .iter()
            .map(|p| p.id.clone())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!("unknown provider '{token}'. available: {available}");
    }
    Ok(canonical)
}

#[derive(Clone, Copy)]
pub struct KimiHeaderContext<'a> {
    pub device_name: &'a str,
    pub device_model: &'a str,
    pub os_version: &'a str,
    pub device_id: &'a str,
}

pub struct HeaderContext<'a> {
    pub originator: &'a str,
    pub user_agent: &'a str,
    pub request_id: Option<&'a str>,
    pub kimi: Option<KimiHeaderContext<'a>>,
}

/// Authentication and client headers for one provider request. Shared by every
/// host so all of them present the same client identity to providers.
pub fn provider_headers_with_context(
    contract: RequestContract,
    api_key: &str,
    official_kimi: bool,
    anthropic_subscription: bool,
    extended_anthropic_cache: bool,
    session_id: Option<&str>,
    context: &HeaderContext<'_>,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    let mut put = |name: &'static str, value: String| -> Result<()> {
        let mut value = HeaderValue::from_str(&value)
            .map_err(|_| anyhow::anyhow!("invalid `{name}` header value"))?;
        value.set_sensitive(matches!(name, "authorization" | "x-api-key"));
        headers.insert(name, value);
        Ok(())
    };
    match contract.api_provider() {
        ApiProvider::OpenAi => {
            if !api_key.trim().is_empty() {
                put("authorization", format!("Bearer {api_key}"))?;
            }
        }
        ApiProvider::ChatGpt => {
            let account_id = chatgpt_account_id_from_token(api_key).ok_or_else(|| {
                anyhow::anyhow!(
                    "could not extract chatgpt account id from token. re-run /login chatgpt."
                )
            })?;
            put("authorization", format!("Bearer {api_key}"))?;
            put("chatgpt-account-id", account_id)?;
            put("originator", context.originator.to_string())?;
            put("user-agent", context.user_agent.to_string())?;
            put("openai-beta", "responses=experimental".into())?;
            if let Some(id) = session_id {
                put("session_id", id.into())?;
                put("x-client-request-id", id.into())?;
            }
        }
        ApiProvider::Anthropic => {
            put("anthropic-version", ANTHROPIC_API_VERSION.into())?;
            if anthropic_subscription {
                let beta = if extended_anthropic_cache {
                    "claude-code-20250219,oauth-2025-04-20,extended-cache-ttl-2025-04-11"
                } else {
                    "claude-code-20250219,oauth-2025-04-20"
                };
                put("authorization", format!("Bearer {api_key}"))?;
                put("anthropic-beta", beta.into())?;
                put("anthropic-dangerous-direct-browser-access", "true".into())?;
                put("user-agent", crate::claude_subscription::user_agent())?;
                put("x-app", "cli".into())?;
                put(
                    "x-client-request-id",
                    context
                        .request_id
                        .context("subscription request id required")?
                        .to_string(),
                )?;
                if let Some(id) = session_id {
                    put("x-claude-code-session-id", id.into())?;
                }
                return Ok(headers);
            }
            if extended_anthropic_cache {
                put("anthropic-beta", "extended-cache-ttl-2025-04-11".into())?;
            }
            if official_kimi {
                let version = crate::CLIENT_VERSION;
                put("user-agent", format!("dext/{version}"))?;
                put("x-msh-platform", "kimi_code_cli".into())?;
                put("x-msh-version", version.into())?;
                put(
                    "x-msh-device-name",
                    context
                        .kimi
                        .context("Kimi header facts required")?
                        .device_name
                        .to_string(),
                )?;
                put(
                    "x-msh-device-model",
                    context
                        .kimi
                        .context("Kimi header facts required")?
                        .device_model
                        .to_string(),
                )?;
                put(
                    "x-msh-os-version",
                    context
                        .kimi
                        .context("Kimi header facts required")?
                        .os_version
                        .to_string(),
                )?;
                put(
                    "x-msh-device-id",
                    context
                        .kimi
                        .context("Kimi header facts required")?
                        .device_id
                        .to_string(),
                )?;
            }
            if !api_key.trim().is_empty() {
                put("x-api-key", api_key.into())?;
            }
        }
    }
    Ok(headers)
}
