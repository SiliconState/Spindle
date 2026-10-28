//! Native conversation values shared by the CLI and opt-in durable hosts.
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub mod checkpoint;
pub mod continuation;
pub mod runtime;
pub mod tool_results;
pub mod turn;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Thinking {
        #[serde(rename = "thinking", alias = "text", default)]
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    ResponsesReasoning {
        item: Value,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(
            default = "empty_tool_result_metadata",
            skip_serializing_if = "ToolResultMetadata::is_empty"
        )]
        metadata: ToolResultMetadata,
    },
    ImageReference {
        path: String,
        media_type: String,
        width: u32,
        height: u32,
        source_sha256: String,
        #[serde(skip)]
        disclosure_target: Option<String>,
    },
    ImageArtifact {
        reference: ArtifactImage,
    },
    PartialStream {
        text: String,
    },
}

impl dext_core::response::AssistantBlock for Block {
    type Input = Value;

    fn kind(&self) -> dext_core::response::BlockKind<'_> {
        use dext_core::response::{BlockKind, CallRef};
        match self {
            Self::Text { .. }
            | Self::Thinking { .. }
            | Self::RedactedThinking { .. }
            | Self::ResponsesReasoning { .. } => BlockKind::Content,
            Self::ToolUse { id, name, input } if input.is_object() => {
                BlockKind::CompleteCall(CallRef { id, name })
            }
            Self::PartialStream { .. } => BlockKind::IncompleteCall,
            Self::ToolUse { .. }
            | Self::ToolResult { .. }
            | Self::ImageReference { .. }
            | Self::ImageArtifact { .. } => BlockKind::Invalid,
        }
    }

    fn into_call(self) -> Option<dext_core::response::ToolCall<Self::Input>> {
        match self {
            Self::ToolUse { id, name, input } => {
                Some(dext_core::response::ToolCall { id, name, input })
            }
            _ => None,
        }
    }
}

/// Immutable scoped sanitized-JPEG identity. No path, bytes or disclosure grant.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ArtifactImage {
    pub scope: String,
    pub digest: String,
    pub byte_length: u64,
    pub width: u32,
    pub height: u32,
}
impl std::fmt::Debug for ArtifactImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}
impl ArtifactImage {
    pub fn is_valid(&self) -> bool {
        !self.scope.is_empty()
            && self.scope.len() <= 128
            && self
                .scope
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
            && self.digest.len() == 64
            && self
                .digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && self.byte_length > 0
            && self.byte_length <= 1024 * 1024
            && self.width > 0
            && self.height > 0
            && self.width.max(self.height) <= dext_core::image::LONGEST_SIDE_CAP
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ToolResultMetadata {
    pub status: Option<String>,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    pub artifact: Option<String>,
}

impl ToolResultMetadata {
    pub fn is_empty(&self) -> bool {
        self.status.is_none()
            && self.exit_code.is_none()
            && self.duration_ms.is_none()
            && self.artifact.is_none()
    }
}

fn empty_tool_result_metadata() -> ToolResultMetadata {
    ToolResultMetadata::default()
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Message {
    pub role: String,
    pub content: Vec<Block>,
}

pub const DEFAULT_INPUT_USD_PER_MTOK: f64 = 1.0;
pub const DEFAULT_OUTPUT_USD_PER_MTOK: f64 = 5.0;
pub const DEFAULT_CACHE_READ_USD_PER_MTOK: f64 = 0.1;
pub const DEFAULT_CACHE_CREATE_USD_PER_MTOK: f64 = 1.25;

#[derive(Default, Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    // Provider usage is normalized into disjoint input buckets:
    // - Anthropic: input_tokens plus cache_creation/read_input_tokens.
    // - OpenAI/ChatGPT: prompt/input tokens minus cached_tokens, with cached_tokens as cache_read.
    // - Z.ai/GLM: Anthropic-compatible fields when present; otherwise no cache buckets.
    // - local llama.cpp: timings.prompt_n as new prompt input and timings.cache_n as cache_read.
    pub input: u64,
    pub output: u64,
    pub cache_create: u64,
    pub cache_read: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

impl Usage {
    pub fn add(&mut self, o: Usage) {
        let lhs_cost = self.cost_usd;
        let rhs_cost = o.cost_usd;
        let lhs_tokens = self.total_tokens();
        let rhs_tokens = o.total_tokens();
        self.input = self.input.saturating_add(o.input);
        self.output = self.output.saturating_add(o.output);
        self.cache_create = self.cache_create.saturating_add(o.cache_create);
        self.cache_read = self.cache_read.saturating_add(o.cache_read);
        self.cost_usd = match (lhs_cost, rhs_cost) {
            (Some(a), Some(b)) => Some(a + b).filter(|cost| cost.is_finite()),
            (Some(a), None) if rhs_tokens == 0 => Some(a),
            (None, Some(b)) if lhs_tokens == 0 => Some(b),
            (None, None) if lhs_tokens == 0 && rhs_tokens == 0 => None,
            _ => None,
        };
    }

    pub fn actual_input_tokens(&self) -> u64 {
        self.input
    }

    pub fn cached_input_tokens(&self) -> u64 {
        self.cache_create.saturating_add(self.cache_read)
    }

    pub fn total_input_tokens(&self) -> u64 {
        self.input.saturating_add(self.cached_input_tokens())
    }

    pub fn billed_tokens(&self) -> u64 {
        self.total_input_tokens().saturating_add(self.output)
    }

    pub fn context_tokens(&self) -> u64 {
        self.billed_tokens()
    }

    pub fn total_tokens(&self) -> u64 {
        self.billed_tokens()
    }

    pub fn is_valid(&self) -> bool {
        self.cost_usd
            .is_none_or(|cost| cost.is_finite() && cost >= 0.0)
    }

    pub fn estimated_cost_usd(&self) -> f64 {
        if let Some(cost) = self.cost_usd {
            return cost;
        }
        let per_mtok = 1_000_000.0;
        (self.input as f64 / per_mtok) * DEFAULT_INPUT_USD_PER_MTOK
            + (self.output as f64 / per_mtok) * DEFAULT_OUTPUT_USD_PER_MTOK
            + (self.cache_read as f64 / per_mtok) * DEFAULT_CACHE_READ_USD_PER_MTOK
            + (self.cache_create as f64 / per_mtok) * DEFAULT_CACHE_CREATE_USD_PER_MTOK
    }

    pub fn parse(v: &Value) -> Self {
        let cache_create = v["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        let cache_read = v["cache_read_input_tokens"].as_u64().unwrap_or(0);
        let input = if let Some(input) = v["input_tokens"].as_u64() {
            input
        } else {
            v["prompt_tokens"]
                .as_u64()
                .unwrap_or(0)
                .saturating_sub(cache_create)
                .saturating_sub(cache_read)
        };
        let output = v["output_tokens"]
            .as_u64()
            .or_else(|| v["completion_tokens"].as_u64())
            .unwrap_or(0);
        Self {
            input,
            output,
            cache_create,
            cache_read,
            cost_usd: parse_usage_cost(v),
        }
    }

    pub fn parse_openai(v: &Value) -> Self {
        let prompt_cache_hit = v["prompt_cache_hit_tokens"].as_u64().unwrap_or(0);
        let prompt_cache_miss = v["prompt_cache_miss_tokens"].as_u64();
        let total_input = v["prompt_tokens"]
            .as_u64()
            .or_else(|| v["input_tokens"].as_u64())
            .or_else(|| prompt_cache_miss.map(|miss| miss.saturating_add(prompt_cache_hit)))
            .unwrap_or(0);
        let output = v["completion_tokens"]
            .as_u64()
            .or_else(|| v["output_tokens"].as_u64())
            .or_else(|| v["completion_tokens_details"]["accepted_prediction_tokens"].as_u64())
            .unwrap_or(0);
        let cache_read = v
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .or_else(|| {
                v.get("input_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(Value::as_u64)
            })
            .or_else(|| v["cache_read_input_tokens"].as_u64())
            .or_else(|| v["cached_tokens"].as_u64())
            .or(Some(prompt_cache_hit).filter(|tokens| *tokens > 0))
            .unwrap_or(0);
        let cache_create = v["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        let cost_usd = parse_usage_cost(v);
        Self {
            input: total_input
                .saturating_sub(cache_read)
                .saturating_sub(cache_create),
            output,
            cache_create,
            cache_read,
            cost_usd,
        }
    }

    pub fn parse_openai_timings(v: &Value) -> Option<Self> {
        let cache_read = v["cache_n"].as_u64().unwrap_or(0);
        let input = v["prompt_n"].as_u64().unwrap_or(0);
        let output = v["predicted_n"].as_u64().unwrap_or(0);
        (cache_read > 0 || input > 0 || output > 0).then_some(Self {
            input,
            output,
            cache_create: 0,
            cache_read,
            cost_usd: None,
        })
    }

    pub fn line(&self) -> String {
        let mut input = format!("input={}", self.total_input_tokens());
        if self.cached_input_tokens() > 0 {
            input.push_str(&format!(
                " new_in={} cache_r={} cache_w={}",
                self.actual_input_tokens(),
                self.cache_read,
                self.cache_create
            ));
        }
        format!(
            "{} out={} total={} est=${:.4}",
            input,
            self.output,
            self.total_tokens(),
            self.estimated_cost_usd()
        )
    }
}

pub fn parse_usage_cost(v: &Value) -> Option<f64> {
    v["cost"]
        .as_f64()
        .or_else(|| v["cost_usd"].as_f64())
        .or_else(|| v["total_cost"].as_f64())
        .or_else(|| v["total_cost_usd"].as_f64())
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}
