//! Request shaping shared by Spindle's hosts: the route a provider profile gives, the
//! reasoning fields an effort level becomes, the tools and system prompt a session's
//! settings offer, and the exact request body. Pure: no credentials, files or network,
//! so the same settings and transcript always give the same bytes.
// Only a host (`agent` or `embedded`) uses these; `shape` alone only builds them.
#![cfg_attr(not(any(feature = "agent", feature = "embedded")), allow(dead_code))]
use crate::broker::{MAX_SHOWN, strict_tool_refusal};
use crate::completion::Attempt;
use crate::driver::app_tools::{self, AppTool, BuiltinTools};
use crate::driver::task_tool as tasks;
use crate::driver::{Effort, Settings, is_prompt};
use dext_core::model::{Encoding, Identity, PreparedRequest};
use dext_provider_catalog::catalog::{self as provider, ProviderProfile, RequestContract};
use dext_provider_catalog::wire;
use dext_transcript::checkpoint::TranscriptCheckpoint;
use dext_transcript::turn::TurnContinuation;
use dext_transcript::{Block, Message};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;

pub type Result<T> = std::result::Result<T, String>;

fn turn_error(error: dext_transcript::continuation::Error) -> String {
    format!("turn state rejected the step ({error:?})")
}

pub(crate) const ROUTE: &str = "spindle-cli-v1";

/// Answer tokens a request may spend, before any reasoning allowance on top; a model's
/// own declared output limit can lower it (`output_tokens`).
pub(crate) const MAX_OUTPUT_TOKENS: u32 = 32768;

/// History (transcript bytes) kept verbatim before older turns are compacted: 80% of
/// what the model's context window leaves beside the reply, at three bytes per token
/// (JSON, code and non-English text run denser than Dext's four), within these bounds.
/// Steps share unchanged chunks of history (schema 16), so a long history costs each
/// step only what changed.
pub(crate) const HISTORY_MIN: usize = 16 * 1024;

pub(crate) const HISTORY_MAX: usize = 8 * 1024 * 1024;

/// Context window assumed for a model the catalog does not describe.
pub(crate) const DEFAULT_WINDOW: u64 = 200_000;

/// Room reserved per reply token before a model call: UTF-8 plus JSON escaping, twice
/// over (a recorded reply sits in the history and in the recorded response).
pub(crate) const RESERVE_PER_TOKEN: usize = 2 * 16;

/// Room for what travels beside reply text, such as thinking signatures.
pub(crate) const RESERVE_SLACK: usize = 4 * 1024 * 1024;

pub(crate) fn system(network: bool, toolchain: bool) -> String {
    format!(
        "You are a coding agent. You work in a workspace with the \
         bash, run_code, read_file, read_output, write_file, edit_file, start_job, read_job and \
         stop_job tools (show puts files in the chat, settings changes this thread's model \
         and options); commands start in the \
         workspace directory, so refer to files by relative paths. Prefer one run_code \
         script over many small tool calls when work spans several files or steps. {} Workspace files persist between \
         calls; use $TMPDIR for scratch files, which are not kept. Workspace \
         paths are relative (any UTF-8 name; .git, symbolic links and empty directories are \
         kept), up to 50,000 entries, 64 MiB per file and 2 GiB in all. A bash or run_code \
         command ends with its call; for servers, watchers and long builds or test runs use \
         start_job, which keeps a command running in the background over a snapshot of the \
         workspace (its file changes are not saved) while you continue: read_job shows its \
         output (and can wait for more), stop_job ends it. Prefer edit_file for \
         small changes to existing files. You may issue several tool calls at once; they run \
         in order, each on the workspace the previous one left. Check results and keep going \
         until the task is done. Then reply with a short plain-text summary of what you did.{}",
        if network {
            "Commands may use the internet."
        } else {
            "Commands have no internet access, but local sockets work (loopback servers, \
             IPC)."
        },
        if toolchain {
            " The Rust toolchain (cargo, rustc, rustfmt, clippy) is available; build output \
             goes to a persistent cache outside the workspace, so builds stay incremental and \
             never add files to the workspace. Allow long builds a bash timeout of up to 1800 \
             seconds."
        } else {
            ""
        }
    )
}

/// One provider route resolved by Dext's own provider layer from its catalog
/// (`~/.dext/providers.json`, or `DEXT_HOME`). Holds no secret.
/// A resolved model route, without credentials. A host may supply it from its own
/// catalog through `Credentials` instead of the native Dext configuration.
#[derive(Clone)]
pub struct Route {
    pub profile: ProviderProfile,
    pub contract: RequestContract,
    pub endpoint: String,
    pub model: String,
    pub(crate) learned_limit: Option<u32>,
}

impl Route {
    #[cfg_attr(not(feature = "agent"), allow(dead_code))]
    pub fn new(
        profile: ProviderProfile,
        contract: RequestContract,
        endpoint: String,
        model: String,
    ) -> Self {
        Self {
            profile,
            contract,
            endpoint,
            model,
            learned_limit: None,
        }
    }
}

/// The route `profile` gives for `model`, against `base_url` (both as the host
/// resolved them: native hosts apply Dext's environment overrides first).
pub(crate) fn route_with(profile: ProviderProfile, model: String, base_url: &str) -> Result<Route> {
    if model.trim().is_empty() {
        return Err(format!(
            "provider `{}` has no default model; pass --model",
            profile.id
        ));
    }
    let contract = provider::effective_request_contract(&profile, base_url, &model);
    Ok(Route {
        endpoint: provider::provider_request_url(base_url, contract),
        profile,
        contract,
        model,
        learned_limit: None,
    })
}

/// The route `profile` gives as it stands: `model`, or its default model, against its
/// own base URL (a host whose catalog is the app's, with no environment overrides).
#[cfg_attr(not(feature = "embedded"), allow(dead_code))]
pub(crate) fn profile_route(profile: ProviderProfile, model: Option<&str>) -> Result<Route> {
    let model = match model {
        Some(model) => model.to_string(),
        None => provider::normalize_provider_model_value(&profile, &profile.default_model),
    };
    let base_url = profile.base_url.trim_end_matches('/').to_string();
    route_with(profile, model, &base_url)
}

/// A stable UUID-shaped session id derived from Spindle's execution identity, so a
/// re-derived request after a crash is byte-identical.
pub(crate) fn subscription_session(session: &str) -> String {
    let hex: String = Sha256::digest(session.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!(
        "{}-{}-4{}-a{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

pub(crate) fn subscription_body_with_identity(
    mut body: Value,
    first_prompt: &str,
    session: &str,
    identity: Option<&dext_provider_catalog::claude_subscription::ClaudeIdentity>,
) -> Result<Vec<u8>> {
    if let Some(system) = body["system"].as_str() {
        body["system"] = json!([{"type": "text", "text": system}]);
    }
    // A subscription request sends Dext's betas only, not the thinking-binding one.
    if let Some(thinking) = body["thinking"].as_object_mut() {
        thinking.remove("block_binding");
    }
    let bytes = serde_json::to_vec(&body).map_err(|error| error.to_string())?;
    dext_provider_catalog::claude_subscription::transform_body(
        bytes,
        first_prompt,
        &subscription_session(session),
        identity,
    )
    .map_err(|error| format!("subscription request rejected: {error:#}"))
}

impl Effort {
    pub(crate) fn provider(self) -> provider::ThinkingEffort {
        match self {
            Self::Default | Self::Medium => provider::ThinkingEffort::Medium,
            Self::Off => provider::ThinkingEffort::Off,
            Self::Low => provider::ThinkingEffort::Low,
            Self::High => provider::ThinkingEffort::High,
            Self::XHigh => provider::ThinkingEffort::XHigh,
            Self::Max => provider::ThinkingEffort::Max,
        }
    }
}

/// The model's declared level nearest to `level`; undeclared models get at most `high`,
/// which every reasoning_effort endpoint accepts.
pub(crate) fn chat_effort(route: &Route, level: Effort) -> String {
    let levels = provider::resolve_model_spec(&route.profile, &route.model).effort_levels;
    if levels.is_empty() {
        return match level {
            Effort::XHigh | Effort::Max => "high".into(),
            other => other.as_str().into(),
        };
    }
    let nearest: &[&str] = match level {
        Effort::Low => &["low", "minimal", "medium", "high"],
        Effort::Medium => &["medium", "low", "high"],
        Effort::High => &["high", "medium", "xhigh", "max"],
        Effort::XHigh => &["xhigh", "high", "max"],
        _ => &["max", "xhigh", "high"],
    };
    nearest
        .iter()
        .find(|wanted| levels.iter().any(|level| level == *wanted))
        .map(|wanted| wanted.to_string())
        .unwrap_or_else(|| levels[levels.len() - 1].clone())
}

/// Answer tokens and reasoning tokens a request on this route may spend at `level`:
/// up to `MAX_OUTPUT_TOKENS` for the answer and the level's allowance on top, within
/// what the model's own catalog entry declares it can output (Some(limit) when that
/// lowered them; reasoning then gets what remains beside at least half the limit for
/// the answer). Profile-wide defaults are not limits: Dext fills them with 8,192.
pub(crate) fn output_tokens(route: &Route, level: Effort) -> (u32, u32, Option<u32>) {
    let wanted = level.reasoning_allowance();
    let model =
        provider::normalize_provider_model_value(&route.profile, &route.model).to_ascii_lowercase();
    let declared = route
        .profile
        .model_specs
        .get(&model)
        .and_then(|spec| spec.max_output_tokens);
    let limit = match (declared, route.learned_limit) {
        (Some(declared), Some(learned)) => Some(declared.min(learned)),
        (declared, learned) => declared.or(learned),
    };
    match limit {
        Some(limit) if limit < MAX_OUTPUT_TOKENS + wanted => {
            let thinking = wanted.min(limit - MAX_OUTPUT_TOKENS.min(limit / 2));
            (
                (limit - thinking).min(MAX_OUTPUT_TOKENS),
                thinking,
                Some(limit),
            )
        }
        _ => (MAX_OUTPUT_TOKENS, wanted, None),
    }
}

pub(crate) fn limit_note(limit: Option<u32>) -> String {
    limit
        .map(|limit| format!("; the model's {limit}-token output limit caps it"))
        .unwrap_or_default()
}

/// How a Claude model takes reasoning on the Messages API. From the 4.6 generation on,
/// thinking is `{"type": "adaptive"}` with `output_config.effort`; Claude Opus 5.5,
/// Sonnet 5.5, Opus 5/4.8/4.7, Sonnet 5, Haiku 5.5 and the Fable/Mythos models refuse a
/// `budget_tokens` budget with a 400, and some cannot turn thinking off at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClaudeThinking {
    /// Thinking is always on: `disabled` is refused (Fable, Mythos, Opus 5.5+, Sonnet 5.5+).
    AlwaysOn,
    /// Adaptive when `thinking` is omitted; `disabled` accepted (Opus 5, Sonnet 5, Haiku 5+).
    OnByDefault,
    /// No thinking when omitted; adaptive is the only on-mode (Opus 4.7, 4.8).
    OffByDefault,
    /// Adaptive, without the `xhigh` level (Opus 4.6, Sonnet 4.6).
    Adaptive46,
    /// Older models (and every non-Claude model): a `budget_tokens` budget.
    Budget,
}

/// The thinking class of a model id (`claude-opus-5-5`, `anthropic.claude-sonnet-4-6`,
/// `claude-haiku-4-5-20251001`, `claude-opus-5-5[1m]`).
pub(crate) fn claude_thinking(model: &str) -> ClaudeThinking {
    let model = model.to_ascii_lowercase();
    let Some(at) = model.find("claude-") else {
        return ClaudeThinking::Budget;
    };
    let name = model[at + "claude-".len()..]
        .split(['[', '@', ':'])
        .next()
        .unwrap_or_default();
    let mut parts = name.split('-');
    let family = parts.next().unwrap_or_default();
    // Major and minor version; a date suffix (20251001) is not a minor version.
    let mut numbers = parts.filter_map(|part| part.parse::<u32>().ok());
    let Some(major) = numbers.next() else {
        return ClaudeThinking::Budget;
    };
    let minor = numbers.next().filter(|minor| *minor < 100).unwrap_or(0);
    let version = (major, minor);
    match family {
        "fable" | "mythos" => ClaudeThinking::AlwaysOn,
        "opus" | "sonnet" if version >= (5, 5) => ClaudeThinking::AlwaysOn,
        "opus" | "sonnet" if version >= (5, 0) => ClaudeThinking::OnByDefault,
        "haiku" if version >= (5, 0) => ClaudeThinking::OnByDefault,
        "opus" if version >= (4, 7) => ClaudeThinking::OffByDefault,
        "opus" | "sonnet" if version == (4, 6) => ClaudeThinking::Adaptive46,
        _ => ClaudeThinking::Budget,
    }
}

/// Whether requests on this route bind thinking blocks with `drop_block`: Claude models
/// with adaptive thinking on Anthropic's own API. Such a block is tied to the
/// conversation before it, so compaction, a settings change or a new host build that
/// alters the system prompt would make replaying it a 400 for accounts the API
/// enforces; with `drop_block` the API drops it instead. Needs the
/// `thinking-binding-controls-2026-08-01` beta (`live_headers`).
pub(crate) fn binds_thinking(route: &Route) -> bool {
    route.contract == RequestContract::AnthropicMessages
        && route.endpoint.contains("api.anthropic.com")
        && claude_thinking(&route.model) != ClaudeThinking::Budget
}

pub(crate) const THINKING_BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

/// What a request on this route carries for `effort`, plus a description for people.
pub(crate) struct Reasoning {
    pub(crate) fields: Vec<(&'static str, Value)>,
    /// The answer cap on this route.
    pub(crate) answer: u32,
    /// Output tokens added to the answer cap for reasoning.
    pub(crate) allowance: u32,
    #[cfg_attr(not(feature = "agent"), allow(dead_code))]
    pub(crate) label: String,
    #[cfg_attr(not(feature = "agent"), allow(dead_code))]
    pub(crate) short: String,
}

/// Adaptive thinking for a Claude model (`claude_thinking`): `effort` becomes
/// `output_config.effort`, and the level's reasoning allowance is added to the answer
/// cap (adaptive thinking spends `max_tokens` too). `default` leaves the model's own
/// default; `off` disables thinking where the model allows it and is its lowest effort
/// where it does not.
pub(crate) fn adaptive(route: &Route, effort: Effort) -> Result<Reasoning> {
    let class = claude_thinking(&route.model);
    let thinking = || {
        let mut thinking = json!({"type": "adaptive"});
        if binds_thinking(route) {
            thinking["block_binding"] = json!({"prefix_mismatch_behavior": "drop_block"});
        }
        thinking
    };
    let budgeted = |level: Effort, sent: &str, label: String, short: String| {
        let (answer, allowance, limit) = output_tokens(route, level);
        Ok(Reasoning {
            fields: vec![
                ("thinking", thinking()),
                ("output_config", json!({"effort": sent})),
                ("max_tokens", json!(answer + allowance)),
            ],
            answer,
            allowance,
            label: format!("{label}{}", limit_note(limit)),
            short,
        })
    };
    match (class, effort) {
        // Thinking on by default: say so, and bind its blocks.
        (ClaudeThinking::AlwaysOn | ClaudeThinking::OnByDefault, Effort::Default) => {
            let (answer, allowance, limit) = output_tokens(route, Effort::Medium);
            Ok(Reasoning {
                fields: vec![
                    ("thinking", thinking()),
                    ("max_tokens", json!(answer + allowance)),
                ],
                answer,
                allowance,
                label: format!(
                    "adaptive thinking at the model's default effort{}",
                    limit_note(limit)
                ),
                short: "think auto".into(),
            })
        }
        (_, Effort::Default) => {
            let (answer, _, _) = output_tokens(route, Effort::Default);
            Ok(Reasoning {
                fields: vec![],
                answer,
                allowance: 0,
                label: "provider default (no thinking)".into(),
                short: "default".into(),
            })
        }
        // These models refuse `disabled`: the least they think is effort low.
        (ClaudeThinking::AlwaysOn, Effort::Off) => budgeted(
            Effort::Low,
            "low",
            "this model always thinks: adaptive thinking at effort low".into(),
            "think low".into(),
        ),
        (_, Effort::Off) => {
            let (answer, _, _) = output_tokens(route, Effort::Default);
            Ok(Reasoning {
                fields: vec![("thinking", json!({"type": "disabled"}))],
                answer,
                allowance: 0,
                label: "thinking off".into(),
                short: "think off".into(),
            })
        }
        // The 4.6 models have no `xhigh`: the nearest level below.
        (ClaudeThinking::Adaptive46, Effort::XHigh) => budgeted(
            Effort::XHigh,
            "high",
            "adaptive thinking, effort high (this model has no xhigh)".into(),
            "think high".into(),
        ),
        (_, level) => budgeted(
            level,
            level.as_str(),
            format!("adaptive thinking, effort {}", level.as_str()),
            format!("think {}", level.as_str()),
        ),
    }
}

pub(crate) fn reasoning(route: &Route, effort: Effort) -> Result<Reasoning> {
    let glm = route.profile.id == "glm" || route.endpoint.contains("z.ai");
    let deepseek = provider::is_deepseek_provider(
        &route.profile.id,
        route.contract.api_provider(),
        &route.endpoint,
    );
    let local = provider::is_local_llama_provider(
        &route.profile.id,
        route.contract.api_provider(),
        &route.endpoint,
    );
    let off = json!({"type": "disabled"});
    let (answer, _, _) = output_tokens(route, Effort::Default);
    let plain = |fields, label: &str, short: &str| {
        Ok(Reasoning {
            fields,
            answer,
            allowance: 0,
            label: label.into(),
            short: short.into(),
        })
    };
    match (route.contract, effort) {
        // GLM's endpoint thinks by default and a long chain can spend the whole answer
        // cap with no text or calls (three of those halt the turn).
        (RequestContract::AnthropicMessages, Effort::Default) if glm => plain(
            vec![("thinking", off)],
            "thinking off (Spindle default for GLM)",
            "think off",
        ),
        (RequestContract::AnthropicMessages, effort)
            if claude_thinking(&route.model) != ClaudeThinking::Budget =>
        {
            adaptive(route, effort)
        }
        (RequestContract::AnthropicMessages, Effort::Default) => {
            plain(vec![], "provider default (no extended thinking)", "default")
        }
        (RequestContract::AnthropicMessages, Effort::Off) => {
            plain(vec![("thinking", off)], "thinking off", "think off")
        }
        (RequestContract::AnthropicMessages, level) => {
            // Anthropic refuses a thinking budget below 1024 tokens; the answer gives
            // way when a declared output limit leaves no more.
            let (answer, budget, limit) = output_tokens(route, level);
            let budget = budget.max(1024);
            let answer = limit.map_or(answer, |limit| {
                answer.min(limit.saturating_sub(budget)).max(1)
            });
            // The thinking budget is added on top of the answer cap so thinking can
            // never starve the reply of tokens.
            Ok(Reasoning {
                fields: vec![
                    (
                        "thinking",
                        json!({"type": "enabled", "budget_tokens": budget}),
                    ),
                    ("max_tokens", json!(answer + budget)),
                ],
                answer,
                allowance: budget,
                label: format!(
                    "extended thinking, {budget}-token budget ({}{})",
                    level.as_str(),
                    limit_note(limit)
                ),
                short: format!("think {}K", budget / 1024),
            })
        }
        (RequestContract::OpenAiChatCompletions, Effort::Default | Effort::Off) if deepseek => {
            plain(
                vec![("thinking", off)],
                "thinking off (Spindle default for DeepSeek)",
                "think off",
            )
        }
        // DeepSeek refuses a tool-call follow-up unless the reasoning it produced is
        // sent back, so thinking requests capture reasoning_content and replay it.
        (RequestContract::OpenAiChatCompletions, level) if deepseek => {
            let sent = match level {
                Effort::Low => "low",
                Effort::Max => "max",
                _ => "high",
            };
            let (answer, budget, limit) = output_tokens(route, level);
            Ok(Reasoning {
                fields: vec![
                    ("thinking", json!({"type": "enabled"})),
                    ("reasoning_effort", json!(sent)),
                    ("max_tokens", json!(answer + budget)),
                ],
                answer,
                allowance: budget,
                label: format!(
                    "DeepSeek thinking, effort {sent}, {budget}-token reasoning allowance{}",
                    limit_note(limit)
                ),
                short: format!("think {sent}"),
            })
        }
        (RequestContract::OpenAiChatCompletions, Effort::Off) if local => plain(
            vec![("chat_template_kwargs", json!({"enable_thinking": false}))],
            "thinking off",
            "think off",
        ),
        (RequestContract::OpenAiChatCompletions, Effort::Default) => {
            plain(vec![], "provider default", "default")
        }
        (RequestContract::OpenAiChatCompletions, Effort::Off) => Err(
            "this route has no reasoning off switch; choose default, low, medium, high, xhigh or max"
                .into(),
        ),
        (RequestContract::OpenAiChatCompletions, level) => {
            let sent = chat_effort(route, level);
            let (answer, allowance, _) = output_tokens(route, level);
            let mut fields = vec![
                ("reasoning_effort", json!(sent)),
                ("max_tokens", json!(answer + allowance)),
            ];
            if local {
                fields.push(("chat_template_kwargs", json!({"enable_thinking": true})));
            }
            Ok(Reasoning {
                fields,
                answer,
                allowance,
                label: format!("reasoning effort {sent}"),
                short: format!("effort {sent}"),
            })
        }
        (RequestContract::OpenAiResponses | RequestContract::ChatGptResponses, Effort::Default) => {
            plain(vec![], "provider default", "default")
        }
        (RequestContract::OpenAiResponses | RequestContract::ChatGptResponses, level) => {
            let declared = !provider::resolve_model_spec(&route.profile, &route.model)
                .effort_levels
                .is_empty();
            let sent = if level != Effort::Off && declared {
                chat_effort(route, level)
            } else {
                provider::chatgpt_reasoning_effort(&route.model, level.provider())
                    .ok_or(format!(
                        "{} has no reasoning off switch; choose default, low, medium, high, xhigh or max",
                        route.model
                    ))?
                    .to_string()
            };
            let (answer, allowance, _) = output_tokens(route, level);
            let mut fields = vec![("reasoning", json!({"effort": sent}))];
            // Hidden reasoning counts against max_output_tokens; the ChatGPT backend
            // refuses any output cap.
            if route.contract == RequestContract::OpenAiResponses {
                fields.push(("max_output_tokens", json!(answer + allowance)));
            }
            Ok(Reasoning {
                fields,
                answer,
                allowance,
                label: format!("reasoning effort {sent}"),
                short: format!("effort {sent}"),
            })
        }
    }
}

impl Settings {
    /// What a request built under these settings offers beyond the transcript, in a
    /// session that may (`tasks`) start tasks.
    pub(crate) fn offer(&self, tasks: bool) -> Offer<'_> {
        Offer {
            builtin: self.builtin_tools,
            app: &self.tools,
            instructions: self.instructions.as_deref(),
            tasks,
            runner: self.builtin_tools == BuiltinTools::Files
                && self.runner != crate::driver::Runner::None,
        }
    }
}

/// What a request offers the model besides its transcript: which built-in tools, the
/// app's tools and the app's instructions.
#[derive(Clone, Copy)]
pub(crate) struct Offer<'a> {
    pub(crate) builtin: BuiltinTools,
    pub(crate) app: &'a [AppTool],
    pub(crate) instructions: Option<&'a str>,
    /// The `task` tool, after the built-in ones.
    pub(crate) tasks: bool,
    /// `bash` (and `read_output`, which pages its retained output) beside the file
    /// tools, run by the session's runner (embedded hosts).
    pub(crate) runner: bool,
}

impl Offer<'_> {
    /// Spindle's own tools only, every one.
    #[cfg_attr(not(feature = "agent"), allow(dead_code))]
    const BUILTIN: Offer<'static> = Offer {
        builtin: BuiltinTools::All,
        app: &[],
        instructions: None,
        tasks: false,
        runner: false,
    };
    /// The tools offered: the built-in ones chosen, then the app's in their order.
    pub(crate) fn tools(&self, network: bool, toolchain: bool) -> Vec<(String, String, Value)> {
        let mut tools: Vec<_> = tool_specs(network, toolchain)
            .into_iter()
            .filter(|(name, _, _)| self.offers(name))
            .map(|(name, description, schema)| (name.to_string(), description, schema))
            .collect();
        if self.tasks {
            let task = tasks::definition();
            tools.push((task.name, task.description, task.input_schema));
        }
        tools.extend(self.app.iter().map(|tool| {
            (
                tool.name.clone(),
                tool.description.clone(),
                tool.input_schema.clone(),
            )
        }));
        tools
    }
    /// Whether the built-in tool `name` is offered.
    pub(crate) fn offers(&self, name: &str) -> bool {
        self.builtin.offers(name) || (self.runner && matches!(name, "bash" | "read_output"))
    }
    /// The system prompt: Spindle's coding prompt unchanged when the offer is the
    /// built-in one, otherwise one that names only what is offered, then the app's
    /// instructions.
    /// `guidance` (a host's: self-edit, source, shelf) names Spindle's own tools, so
    /// it follows Spindle's prompt only when they are all offered.
    pub(crate) fn system(&self, network: bool, toolchain: bool, guidance: Option<&str>) -> String {
        let mut text = match self.builtin {
            BuiltinTools::All => system(network, toolchain),
            BuiltinTools::Files if self.runner => RUNNER_SYSTEM.to_string(),
            BuiltinTools::Files => FILES_SYSTEM.to_string(),
            BuiltinTools::None => NO_TOOLS_SYSTEM.to_string(),
        };
        if let Some(guidance) = guidance.filter(|_| self.builtin == BuiltinTools::All) {
            text.push(' ');
            text.push_str(guidance);
        }
        if !self.app.is_empty() {
            let names: Vec<&str> = self.app.iter().map(|tool| tool.name.as_str()).collect();
            text.push_str(&format!(
                " The app you work in also gives you these tools: {}. Their descriptions say \
                 what each does. A result that says the app could not confirm whether a call \
                 ran means its effect must be checked before the call is repeated.",
                names.join(", ")
            ));
        }
        if let Some(instructions) = self.instructions {
            text.push_str("\n\n");
            text.push_str(instructions);
        }
        text
    }
}

pub(crate) const FILES_SYSTEM: &str = "You are an assistant working in a workspace of files with the \
     read_file, write_file and edit_file tools. \
     Refer to files by relative paths; workspace files persist between calls. Prefer \
     edit_file for small changes to existing files. You may issue several tool calls at \
     once; they run in order, each on the workspace the previous one left. Check results \
     and keep going until the task is done. Then reply with a short plain-text summary of \
     what you did.";

pub(crate) const RUNNER_SYSTEM: &str = "You are an assistant working in a workspace of files with the \
     bash, read_file, read_output, write_file and edit_file tools. bash runs a command in the \
     workspace directory; its changes to the workspace are kept, and output too long to show \
     is kept for read_output. Refer to files by relative paths; \
     workspace files persist between calls. Prefer edit_file for small changes to existing \
     files. You may issue several tool calls at once; they run in order, each on the \
     workspace the previous one left. Check results and keep going until the task is done. \
     Then reply with a short plain-text summary of what you did.";
pub(crate) const NO_TOOLS_SYSTEM: &str = "You are an assistant. Use the tools you are given when they \
     help. You may issue several tool \
     calls at once; they run in order. Check results and keep going until the task is \
     done, then reply plainly.";

pub(crate) fn tool_specs(network: bool, toolchain: bool) -> Vec<(&'static str, String, Value)> {
    let max_timeout = dext_native::limits::MAX_RUN_SECS;
    let path = json!({"type": "string", "description": "Path relative to the workspace."});
    let mut specs = vec![
        (
            "bash",
            format!(
                "Run a bash command in the workspace directory. {}{} Output defaults to an \
                 8 KiB window; max_output selects 4096–32768 bytes. Larger output is retained \
                 as an artifact for read_output. timeout is in seconds (default 30, max {max_timeout}).",
                if network {
                    "Internet access is available."
                } else {
                    "Do not use the network."
                },
                if toolchain {
                    " cargo/rustc are available; builds go to a cache outside the workspace."
                } else {
                    ""
                }
            ),
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The bash command to run."},
                    "timeout": {"type": "integer", "minimum": 1, "maximum": max_timeout},
                    "max_output": {"type": "integer", "minimum": 4096, "maximum": 32768}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        ),
        (
            "run_code",
            format!(
                "Run a Python, JavaScript or TypeScript script in the workspace directory as ONE step. Use it \
                 instead of many separate tool calls when a task needs several reads, edits, \
                 searches or commands: loop, filter and aggregate inside the script and print only \
                 what you need to see. Only printed output (8 KiB by default, max_output selects \
                 4096–32768 bytes; larger output is retained for read_output) returns to you. Built-in \
                 helpers: read(path) -> text; write(path, text); edit(path, old, new, count=1) \
                 (count=0 replaces all; raises if not found or ambiguous); ls(pattern='**/*') -> \
                 file list; sh(command, check=True) -> combined output (raises on failure). \
                 JavaScript runs as an async function body (await works; require is available). \
                 The script's workspace changes are all-or-nothing: if it fails they are discarded. \
                 External model/tool calls are NOT rolled back. model(prompt,options) returns \
                 {{text,usage,provider,model}}; Python uses keyword options. model options: provider, \
                 model, max_tokens (default 4096), system, effort (off|low|medium|high|xhigh|max), images (a list of \
                 image(path) values; PNG/JPEG/GIF/WebP; the model must accept images, the error lists \
                 those that do). Unknown options are refused. tools.search(query), \
                 tools.describe(name), tool(name,args) use host-reviewed bindings (Python also tools.call). \
                 http(url, {{method, headers, body|json, timeout}}) sends a request through an operator \
                 HTTP grant (the host adds credentials; refusals list the grants) and returns \
                 {{status,headers,json|body,truncated}} (JSON that parses as json, else body). \
                 batch([(method, input), ...]) (JS: batch([{{method,input}}])) reserves every call \
                 first, then runs them concurrently up to the operator's parallel limit and returns \
                 per-call {{ok,data|error}} in order; models(prompts, options) fans out model calls and \
                 returns texts/errors. Calls are durably reserved and never automatically retried after \
                 unknown outcomes. Per-script call/model/budget/parallel limits come from the \
                 operator's code policy (defaults 32 calls, 8 model calls, 32768 prompt-byte+output \
                 allowance, serial); limits() returns the effective values. Defaults: the session \
                 provider/model when granted, else the first granted provider, 4096 output tokens; only \
                 operator-granted routes/tools work. Reusable helpers: put modules in \
                 .spindle/helpers/ (or the workspace root) and import/require them; helper modules \
                 reach the built-ins via `import spindle` (Python) or globalThis.spindle (JS/TS). \
                 ui.skin(css) writes a host-UI skin proposal to .spindle/skin.css (CSS colours and \
                 layout only: no url(), @import, content, !important, quotes or escapes); the operator \
                 stages it from Settings, previews and applies it, so say what you changed. TS \
                 is transpiled (not typechecked), async function body; no top-level import/export. \
                 {} timeout is in seconds (default 60, max {max_timeout}).",
                if network {
                    "Internet access is available."
                } else {
                    "Do not use the network."
                }
            ),
            json!({
                "type": "object",
                "properties": {
                    "language": {"type": "string", "enum": ["python", "javascript", "typescript"]},
                    "code": {"type": "string", "description": format!("The script (up to {} KiB).", crate::broker::MAX_SCRIPT / 1024)},
                    "timeout": {"type": "integer", "minimum": 1, "maximum": max_timeout},
                    "max_output": {"type": "integer", "minimum": 4096, "maximum": 32768}
                },
                "required": ["language", "code"],
                "additionalProperties": false
            }),
        ),
        (
            "read_output",
            "Read a byte window of retained tool output without executing anything again. Use the artifact digest returned by the original result; offset is zero-based, limit defaults to 8192 bytes (max 262144). Tool output shares a 256 KiB turn budget; paging with read_output and read_file is not charged.".into(),
            json!({"type":"object","properties":{
                "artifact":{"type":"string"},
                "offset":{"type":"integer","minimum":0},
                "limit":{"type":"integer","minimum":1,"maximum":262144}
            },"required":["artifact"],"additionalProperties":false}),
        ),
        (
            "read_file",
            "Read a UTF-8 text file from the workspace with line numbers. offset is the first \
             line (1-based), limit the number of lines (default 2000); the window is 8 KiB by \
             default (max_output up to 262144 bytes) with a note on how to continue."
                .into(),
            json!({
                "type": "object",
                "properties": {
                    "path": path,
                    "offset": {"type": "integer", "minimum": 1},
                    "limit": {"type": "integer", "minimum": 1},
                    "max_output": {"type": "integer", "minimum": 4096, "maximum": 262144}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        (
            "write_file",
            "Create or overwrite a workspace file with the given content (up to 1 MiB; \
             parent directories are implied)."
                .into(),
            json!({
                "type": "object",
                "properties": {"path": path, "content": {"type": "string"}},
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        ),
        (
            "edit_file",
            "Replace exact text in a workspace file. old_string must occur exactly once \
             unless replace_all is true."
                .into(),
            json!({
                "type": "object",
                "properties": {
                    "path": path,
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        ),
        (
            "start_job",
            format!(
                "Start a bash command in the background (a server, watcher, long build or test \
                 run) and return at once with its job id and first output. It runs in its own \
                 sandbox over a snapshot of the workspace as it is now: files it changes are \
                 NOT saved to the workspace and later workspace changes are not visible to it \
                 (start a new job to pick them up). {} It keeps running while you make other \
                 calls until it exits, stop_job ends it, its timeout passes (seconds, default \
                 {}, max {}), the session is rewound, or Spindle restarts.",
                if network {
                    "Internet access is available; a server it starts on localhost is \
                     reachable from later bash calls."
                } else {
                    "Each run has its own private network, so later bash calls cannot reach a \
                     server the job starts: exercise it inside the job's own command."
                },
                crate::broker::JOB_SECS,
                dext_native::limits::MAX_JOB_SECS
            ),
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The bash command to run."},
                    "timeout": {"type": "integer", "minimum": 1, "maximum": dext_native::limits::MAX_JOB_SECS}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        ),
        (
            "read_job",
            format!(
                "Read a background job's output and status. Without offset it returns the \
                 newest output (after waiting up to wait seconds for the job to end); with \
                 offset (from the previous result's next offset) it returns output from there, \
                 first waiting up to wait seconds (max {}) for some. limit is in bytes \
                 (default 8192, max {}). Without job it lists the session's jobs. A job keeps \
                 its newest {} of output.",
                crate::broker::MAX_JOB_WAIT,
                crate::broker::MAX_JOB_WINDOW,
                crate::human_size(dext_native::limits::JOB_OUTPUT)
            ),
            json!({
                "type": "object",
                "properties": {
                    "job": {"type": "string"},
                    "offset": {"type": "integer", "minimum": 0},
                    "limit": {"type": "integer", "minimum": 1, "maximum": crate::broker::MAX_JOB_WINDOW},
                    "wait": {"type": "integer", "minimum": 0, "maximum": crate::broker::MAX_JOB_WAIT}
                },
                "additionalProperties": false
            }),
        ),
        (
            "stop_job",
            "Stop a background job and everything it started; returns its final status and \
             newest output."
                .into(),
            json!({
                "type": "object",
                "properties": {"job": {"type": "string"}},
                "required": ["job"],
                "additionalProperties": false
            }),
        ),
    ];
    specs.extend(host_specs());
    specs
}

/// Why a call cannot run under `config`'s offer, or None: an app tool needs an object
/// input; a built-in one must be offered and pass its own checks.
pub(crate) fn offered_refusal(
    config: &Settings,
    tasks: bool,
    name: &str,
    input: &Value,
) -> Option<String> {
    if tasks && name == tasks::NAME {
        if let Some(why) = tasks::refusal(input) {
            return Some(why);
        }
        let model = input["model"]
            .as_str()
            .or(input["settings"]["model"].as_str())
            .unwrap_or(&config.model);
        return (!config.task_policy.models.is_empty()
            && !config
                .task_policy
                .models
                .iter()
                .any(|allowed| allowed == model))
        .then(|| format!("model `{model}` is not permitted by tasks.models"));
    }
    if config.tools.iter().any(|tool| tool.name == name) {
        return (!input.is_object()).then(|| format!("`{name}` takes a JSON object"));
    }
    if !crate::turn::WORKSPACE_TOOLS.contains(&name) || !config.offer(tasks).offers(name) {
        return Some(format!(
            "unknown tool `{name}`; available: {}",
            offered_names(config, tasks).join(", ")
        ));
    }
    strict_tool_refusal(name, input)
}

/// The names of the tools `config` offers, in request order.
pub(crate) fn offered_names(config: &Settings, tasks: bool) -> Vec<String> {
    config
        .offer(tasks)
        .tools(config.network, config.toolchain)
        .into_iter()
        .map(|(name, _, _)| name)
        .collect()
}

/// How an app tool call is dispatched: the definition digest its attempts are labelled
/// with, and the tool's rerun and timeout.
pub(crate) struct AppDispatch {
    pub(crate) helper: [u8; 32],
    pub(crate) rerun: bool,
    pub(crate) timeout: Duration,
    pub(crate) max_output: usize,
}

/// How to dispatch a call of `name` when it is an app tool's: an attempt already
/// reserved keeps the definition its label names; a new one takes the offered tool's.
pub(crate) fn app_dispatch(
    config: &Settings,
    name: &str,
    existing: Option<&Attempt>,
) -> Option<AppDispatch> {
    let task = (name == tasks::NAME).then(tasks::definition);
    let tool = config
        .tools
        .iter()
        .find(|tool| tool.name == name)
        .or(task.as_ref());
    let helper = match existing {
        Some(attempt) => {
            let hex = attempt
                .implementation
                .strip_prefix("spindle-app-tool-v1-")?;
            let mut helper = [0u8; 32];
            if hex.len() != 64 || !hex.is_ascii() {
                return None;
            }
            for (index, byte) in helper.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&hex[2 * index..2 * index + 2], 16).ok()?;
            }
            helper
        }
        None => tool?.digest(),
    };
    Some(AppDispatch {
        helper,
        rerun: tool.is_some_and(|tool| tool.rerun),
        max_output: tool.map_or(app_tools::DEFAULT_MAX_OUTPUT, |tool| tool.max_output),
        timeout: Duration::from_secs(
            tool.map_or(app_tools::DEFAULT_TIMEOUT_S, |tool| tool.timeout_s),
        ),
    })
}

pub(crate) fn text_of(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } | Block::PartialStream { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn thinking_enabled(fields: &[(&str, Value)]) -> bool {
    fields
        .iter()
        .any(|(key, value)| *key == "thinking" && value["type"] == "enabled")
}

/// Decided from the recorded request, so a retried or resumed call captures exactly
/// what the original request asked for. Only OpenAI Chat parsing reads the flag.
pub(crate) fn captures_reasoning(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body).is_ok_and(|body| body["thinking"]["type"] == "enabled")
}

/// Spindle tools never attach images, so any image reference is described, not sent.
pub(crate) fn no_images(_: usize, _: &Block) -> wire::ImagePart {
    wire::ImagePart::Text("[image not available in spindle]".into())
}

/// Deterministic for a given state and configuration, so a re-admitted step after a
/// crash is byte-identical to the original.
#[cfg_attr(not(feature = "agent"), allow(dead_code))]
pub(crate) fn request_body(
    route: &Route,
    model: &str,
    session: &str,
    network: bool,
    toolchain: bool,
    effort: Effort,
    transcript: &TranscriptCheckpoint,
) -> Result<Value> {
    request_body_for(
        route,
        model,
        session,
        network,
        toolchain,
        None,
        effort,
        transcript,
        Offer::BUILTIN,
    )
}

/// `request_body` with `guidance` (a self-edit thread's) appended to the system prompt.
#[allow(clippy::too_many_arguments)]
pub(crate) fn request_body_for(
    route: &Route,
    model: &str,
    session: &str,
    network: bool,
    toolchain: bool,
    guidance: Option<&str>,
    effort: Effort,
    transcript: &TranscriptCheckpoint,
    offer: Offer<'_>,
) -> Result<Value> {
    let tools = offer.tools(network, toolchain);
    let system = offer.system(network, toolchain, guidance);
    let reasoning = reasoning(route, effort)?;
    let mut body = match route.contract {
        RequestContract::AnthropicMessages => {
            json!({
            "model": model,
            "max_tokens": reasoning.answer,
            "stream": true,
            "system": system,
            "messages": wire::anthropic_messages(
                &wire::sanitize_anthropic_messages(&transcript.messages, true, false),
                &mut no_images,
            )
            .map_err(|error| error.to_string())?,
            "tools": tools.iter().map(|(name, description, schema)| json!(
                {"name": name, "description": description, "input_schema": schema})).collect::<Vec<_>>(),
            "tool_choice": {"type": "auto"},
            })
        }
        RequestContract::OpenAiChatCompletions => {
            json!({
                "model": model,
                "stream": true,
                "stream_options": {"include_usage": true},
                "max_tokens": reasoning.answer,
                "messages": wire::chat_messages(
                    &transcript.messages,
                    &system,
                    // DeepSeek needs the reasoning of the current turn's tool calls;
                    // earlier turns' reasoning is ignored, so it is not resent.
                    thinking_enabled(&reasoning.fields).then(|| {
                        transcript.messages.iter().rposition(is_prompt).unwrap_or(0)
                    }),
                    &mut no_images,
                ),
                "tools": tools.iter().map(|(name, description, schema)| json!(
                    {"type": "function", "function": {
                        "name": name, "description": description, "parameters": schema}}))
                    .collect::<Vec<_>>(),
            })
        }
        RequestContract::OpenAiResponses | RequestContract::ChatGptResponses => {
            let mut body = json!({
                "model": model,
                "store": false,
                "stream": true,
                "instructions": system,
                "input": wire::responses_input(
                    &transcript.messages,
                    route.contract == RequestContract::OpenAiResponses,
                    &mut no_images,
                ),
                "text": {"verbosity": "medium"},
                "prompt_cache_key": session,
                "tools": tools.iter().map(|(name, description, schema)| json!(
                    {"type": "function", "name": name, "description": description,
                        "parameters": schema, "strict": false})).collect::<Vec<_>>(),
                "tool_choice": "auto",
                "parallel_tool_calls": true,
            });
            if route.contract == RequestContract::OpenAiResponses {
                body["max_output_tokens"] = json!(reasoning.answer);
            }
            body
        }
    };
    for (key, value) in reasoning.fields {
        body[key] = value;
    }
    if route.contract == RequestContract::AnthropicMessages
        && provider::resolve_model_spec(&route.profile, &route.model).prompt_cache
    {
        mark_prompt_cache(&mut body);
    }
    Ok(body)
}

/// Anthropic prompt caching, as Dext places it: the system prompt, the tool list and a
/// sliding breakpoint on the newest message block that can carry one (markers cache
/// everything before them; signed thinking and images cannot carry one, and nothing at
/// or after the first image is marked). Each step then pays full price only for what
/// changed since the last one.
pub(crate) fn mark_prompt_cache(body: &mut Value) {
    let marker = json!({"type": "ephemeral"});
    if let Some(system) = body["system"].as_str() {
        body["system"] = json!([{"type": "text", "text": system, "cache_control": marker}]);
    }
    if let Some(last) = body["tools"]
        .as_array_mut()
        .and_then(|tools| tools.last_mut())
    {
        last["cache_control"] = marker.clone();
    }
    let Some(messages) = body["messages"].as_array_mut() else {
        return;
    };
    let first_image = messages.iter().enumerate().find_map(|(index, message)| {
        message["content"]
            .as_array()?
            .iter()
            .position(|block| block["type"] == "image")
            .map(|block| (index, block))
    });
    for (index, message) in messages.iter_mut().enumerate().rev() {
        let Some(blocks) = message["content"].as_array_mut() else {
            continue;
        };
        for (position, block) in blocks.iter_mut().enumerate().rev() {
            let kind = block["type"].as_str().unwrap_or("");
            if first_image.is_some_and(|first| (index, position) >= first)
                || matches!(kind, "thinking" | "redacted_thinking" | "image")
                || !block.is_object()
            {
                continue;
            }
            block["cache_control"] = marker;
            return;
        }
    }
}

/// History a session on this route keeps verbatim before compacting older turns.
pub(crate) fn history_budget(route: &Route, effort: Effort) -> Result<usize> {
    let window = provider::resolve_model_spec(&route.profile, &route.model)
        .context_window
        .unwrap_or(DEFAULT_WINDOW);
    let reasoning = reasoning(route, effort)?;
    let reply = u64::from(reasoning.answer + reasoning.allowance);
    let bytes = window.saturating_sub(reply).saturating_mul(3) / 10 * 8;
    Ok(usize::try_from(bytes)
        .unwrap_or(usize::MAX)
        .clamp(HISTORY_MIN, HISTORY_MAX))
}

/// Room a model call needs beside its prepared step: the largest reply it allows.
pub(crate) fn reply_reserve(route: &Route, effort: Effort) -> Result<usize> {
    let reasoning = reasoning(route, effort)?;
    let tokens = reasoning.answer + reasoning.allowance;
    Ok((tokens as usize).saturating_mul(RESERVE_PER_TOKEN) + RESERVE_SLACK)
}

/// Request-shaping facts sampled by the host at the preparation boundary.
pub(crate) struct RequestShape {
    pub(crate) config: Settings,
    pub(crate) route: Route,
    pub(crate) guidance: Option<String>,
    pub(crate) tasks_offered: bool,
    pub(crate) subscription: bool,
    pub(crate) identity: Option<dext_provider_catalog::claude_subscription::ClaudeIdentity>,
}

pub(crate) fn prepared_request(
    state: &TurnContinuation,
    shape: &RequestShape,
) -> Result<PreparedRequest<Vec<u8>>> {
    let config = &shape.config;
    let route = &shape.route;
    let effect = state.next_model_id().map_err(turn_error)?;
    let transcript = state.transcript_ref().map_err(turn_error)?;
    let body = request_body_for(
        route,
        &config.model,
        state.execution(),
        config.network,
        config.toolchain,
        shape.guidance.as_deref(),
        config.effort,
        &transcript,
        config.offer(shape.tasks_offered),
    )?;
    let identity = Identity {
        effect,
        provider: route.profile.id.clone(),
        contract: route.contract.as_str().into(),
        endpoint: route.endpoint.clone(),
        model: route.model.clone(),
        credential_scope: format!("dext-login/{}", route.profile.id),
        configuration: format!(
            "{ROUTE}/max-output-{}",
            reasoning(route, config.effort)?.answer
        ),
    };
    let body = if shape.subscription {
        let first = transcript
            .messages
            .iter()
            .find(|message| is_prompt(message))
            .map(|message| text_of(&message.content))
            .unwrap_or_default();
        subscription_body_with_identity(body, &first, state.execution(), shape.identity.as_ref())?
    } else {
        serde_json::to_vec(&body).map_err(|error| error.to_string())?
    };
    PreparedRequest::new(identity, Encoding::Json, body)
        .map_err(|error| format!("request rejected: {error:?}"))
}

/// The four host tools' specifications, in `HOST_TOOLS` order.
pub(crate) fn host_specs() -> Vec<(&'static str, String, Value)> {
    vec![
        (
            "show",
            format!(
                "Show workspace files to the user in the chat as cards they can open: web \
                 pages (rendered in a sandboxed preview with their CSS, scripts and images), \
                 images, SVG, Markdown, PDF and text. Use it whenever the user asks to see, \
                 display, preview or open something, and after you build something visual (a \
                 page, chart, image or document) so they can look at it without leaving the \
                 chat. paths: 1-{MAX_SHOWN} workspace files (a directory shows its \
                 index.html); title: an optional caption. The files are shown as they are at \
                 this step."
            ),
            json!({
                "type": "object",
                "properties": {
                    "paths": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_SHOWN},
                    "title": {"type": "string"}
                },
                "required": ["paths"],
                "additionalProperties": false
            }),
        ),
        (
            "settings",
            "Read or change this thread's settings when the user asks (\"use a cheaper \
             model\", \"think harder\", \"turn on internet\", \"what models can I use?\"). \
             Without arguments it returns the current settings and the available providers, \
             models and efforts. A change (provider, model, effort, network, toolchain, \
             compaction, max_usd as dollars or null for no budget, name for the thread) is \
             checked, queued and applied when this turn ends, never in the middle of it; \
             unknown values are refused with the valid choices."
                .into(),
            json!({
                "type": "object",
                "properties": {
                    "provider": {"type": "string"},
                    "model": {"type": "string"},
                    "effort": {"type": "string", "enum": ["default", "off", "low", "medium", "high", "xhigh", "max"]},
                    "network": {"type": "boolean"},
                    "toolchain": {"type": "boolean"},
                    "compaction": {"type": "string", "enum": ["model", "digest"]},
                    "max_usd": {"type": ["number", "null"]},
                    "name": {"type": "string"}
                },
                "additionalProperties": false
            }),
        ),
        (
            "spindle_source",
            "Spindle's own source (the app serving this chat), when the host offers it. \
             checkout copies the host's current source into spindle/ and enables what \
             building it needs (Rust toolchain, the dext-engine sources) from your next \
             command; edit and test it there. update merges the host's newer source into \
             spindle/, writing conflict markers where both changed the same lines for you \
             to resolve."
                .into(),
            json!({
                "type": "object",
                "properties": {"action": {"type": "string", "enum": ["checkout", "update"]}},
                "required": ["action"],
                "additionalProperties": false
            }),
        ),
        (
            "spindle_install",
            "Ask to install your changes to Spindle's source (spindle/) into the running \
             host. This does not install: it records the request and raises the Spindle \
             change card in the chat, whose Install the user taps (or types /install); the \
             host then merges your changes with its current \
             source, runs fmt, clippy, tests and a release build, installs and restarts. \
             summary: what the change does, for the user."
                .into(),
            json!({
                "type": "object",
                "properties": {"summary": {"type": "string"}},
                "required": ["summary"],
                "additionalProperties": false
            }),
        ),
    ]
}

/// Options a nested `model()` call understands; anything else is refused, never dropped.
pub(crate) const CODE_MODEL_KEYS: [&str; 7] = [
    "prompt",
    "provider",
    "model",
    "max_tokens",
    "system",
    "images",
    "effort",
];
pub(crate) const CODE_MODEL_MAX_PROMPT: usize = 1024 * 1024;
pub(crate) const CODE_MODEL_MAX_TOKENS: u64 = 131_072;
pub(crate) const CODE_MODEL_MAX_IMAGES: usize = 16;
pub(crate) const CODE_MODEL_MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
pub(crate) const CODE_MODEL_SYSTEM: &str =
    "Answer the supplied task. You have no tools or parent conversation.";

pub(crate) fn code_images(input: &Value) -> Result<Vec<(String, String)>> {
    let Some(list) = input.get("images").filter(|v| !v.is_null()) else {
        return Ok(vec![]);
    };
    let list = list
        .as_array()
        .ok_or("images must be a list of {media_type, data} (use image(path))")?;
    if list.len() > CODE_MODEL_MAX_IMAGES {
        return Err(format!("at most {CODE_MODEL_MAX_IMAGES} images per call"));
    }
    let mut total = 0usize;
    let mut images = Vec::with_capacity(list.len());
    for (index, image) in list.iter().enumerate() {
        if let Some(key) = image.as_object().and_then(|o| {
            o.keys()
                .find(|k| !matches!(k.as_str(), "media_type" | "data"))
        }) {
            return Err(format!(
                "images[{index}]: unknown field `{key}`; use media_type and data"
            ));
        }
        let media_type = image["media_type"]
            .as_str()
            .filter(|m| matches!(*m, "image/png" | "image/jpeg" | "image/webp" | "image/gif"))
            .ok_or(format!(
                "images[{index}].media_type must be image/png, image/jpeg, image/webp or image/gif"
            ))?;
        let data = image["data"]
            .as_str()
            .filter(|d| {
                !d.is_empty()
                    && d.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
            })
            .ok_or(format!("images[{index}].data must be non-empty base64"))?;
        total += data.len();
        images.push((media_type.to_string(), data.to_string()));
    }
    if total > CODE_MODEL_MAX_IMAGE_BYTES {
        return Err(format!(
            "images total {} KiB of base64; the limit is {} MiB per call",
            total / 1024,
            CODE_MODEL_MAX_IMAGE_BYTES >> 20
        ));
    }
    Ok(images)
}

/// A nested `model()` call of a script as a request: one user message (and images), no
/// tools or parent history. `image_models` lists the catalog's image-capable models for
/// a refusal to name.
pub(crate) fn code_model_request(
    input: &Value,
    route: &Route,
    session: &str,
    image_models: &dyn Fn() -> Vec<String>,
) -> Result<Value> {
    if let Some(key) = input
        .as_object()
        .and_then(|o| o.keys().find(|k| !CODE_MODEL_KEYS.contains(&k.as_str())))
    {
        return Err(format!(
            "model(): unknown option `{key}`; options: {}",
            CODE_MODEL_KEYS[1..].join(", ")
        ));
    }
    let prompt = input["prompt"]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= CODE_MODEL_MAX_PROMPT)
        .ok_or(format!(
            "nested prompt must be 1-{} bytes",
            CODE_MODEL_MAX_PROMPT
        ))?;
    let max_tokens = match input.get("max_tokens").filter(|v| !v.is_null()) {
        None => 4096,
        Some(n) => n
            .as_u64()
            .filter(|n| (1..=CODE_MODEL_MAX_TOKENS).contains(n))
            .ok_or(format!("max_tokens must be 1-{CODE_MODEL_MAX_TOKENS}"))?,
    };
    let system = match input.get("system").filter(|v| !v.is_null()) {
        None => CODE_MODEL_SYSTEM,
        Some(text) => text
            .as_str()
            .filter(|s| s.len() <= CODE_MODEL_MAX_PROMPT)
            .ok_or("system must be text up to 1 MiB")?,
    };
    let effort = match input.get("effort").filter(|v| !v.is_null()) {
        None => Effort::Default,
        Some(level) => level
            .as_str()
            .and_then(Effort::parse)
            .ok_or("effort must be default, off, low, medium, high, xhigh or max")?,
    };
    let images = code_images(input)?;
    if !images.is_empty() && !provider::resolve_model_spec(&route.profile, &route.model).image_input
    {
        let capable = image_models();
        return Err(format!(
            "{}/{} does not accept images; image-capable models: {}; pass provider= and model=",
            route.profile.id,
            route.model,
            if capable.is_empty() {
                "none in the catalog".into()
            } else {
                capable.join(", ")
            }
        ));
    }
    // No parent history or tool catalogue is copied into a child model request.
    let transcript = TranscriptCheckpoint {
        route_binding: ROUTE.into(),
        messages: vec![Message {
            role: "user".into(),
            content: vec![Block::Text {
                text: prompt.into(),
            }],
        }],
    };
    let mut body = request_body(
        route,
        &route.model,
        session,
        false,
        false,
        effort,
        &transcript,
    )?;
    let thinking = u64::from(reasoning(route, effort)?.allowance);
    let object = body.as_object_mut().ok_or("invalid child body")?;
    for key in ["tools", "tool_choice", "parallel_tool_calls"] {
        object.remove(key);
    }
    match route.contract {
        RequestContract::AnthropicMessages => {
            let mut content: Vec<Value> = images
                .iter()
                .map(|(media_type, data)| json!({"type":"image","source":{"type":"base64","media_type":media_type,"data":data}}))
                .collect();
            content.push(json!({"type":"text","text":prompt}));
            object.insert(
                "messages".into(),
                json!([{"role":"user","content":content}]),
            );
            object.insert("system".into(), json!(system));
            // A thinking budget is added on top so it can never starve the answer.
            object.insert("max_tokens".into(), json!(max_tokens + thinking));
        }
        RequestContract::OpenAiChatCompletions => {
            let user = if images.is_empty() {
                json!(prompt)
            } else {
                let mut parts = vec![json!({"type":"text","text":prompt})];
                parts.extend(images.iter().map(|(media_type, data)| {
                    json!({"type":"image_url","image_url":{"url":format!("data:{media_type};base64,{data}")}})
                }));
                Value::Array(parts)
            };
            object.insert(
                "messages".into(),
                json!([{"role":"system","content":system},{"role":"user","content":user}]),
            );
            object.insert("max_tokens".into(), json!(max_tokens + thinking));
        }
        RequestContract::OpenAiResponses | RequestContract::ChatGptResponses => {
            let mut content = vec![json!({"type":"input_text","text":prompt})];
            content.extend(images.iter().map(|(media_type, data)| {
                json!({"type":"input_image","image_url":format!("data:{media_type};base64,{data}")})
            }));
            object.insert(
                "input".into(),
                json!([{"type":"message","role":"user","content":content}]),
            );
            object.insert("instructions".into(), json!(system));
            // The ChatGPT subscription backend refuses an output cap; the allowance below
            // still counts max_tokens against the script's model budget.
            if route.contract == RequestContract::OpenAiResponses {
                object.insert("max_output_tokens".into(), json!(max_tokens + thinking));
            }
        }
    }
    // Thinking tokens are billed output too, so they count against the script's allowance.
    let budget =
        (prompt.len() + system.len()) as u64 + max_tokens + thinking + 1024 * images.len() as u64;
    Ok(
        json!({"kind":"model","provider":route.profile.id,"model":route.model,"endpoint":route.endpoint,"contract":route.contract.as_str(),"body":body,"session":session,"budget":budget}),
    )
}
