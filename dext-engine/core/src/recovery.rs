//! Provider-neutral recovery decisions extracted from the existing CLI.
//! These classifications do not authorize another request or tool execution.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contract {
    AnthropicMessages,
    OpenAiChatCompletions,
    OpenAiResponses,
    ChatGptResponses,
}

impl Contract {
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

pub fn incomplete_reason(contract: Contract, stop_reason: Option<&str>) -> Option<&str> {
    if !matches!(
        contract,
        Contract::OpenAiResponses | Contract::ChatGptResponses
    ) {
        return None;
    }
    match stop_reason? {
        "incomplete" => Some("unknown"),
        reason => reason.strip_prefix("incomplete:"),
    }
}

/// The host only continues automatically when no executable tool call remains.
pub fn stream_recovery_reason(
    contract: Contract,
    stop_reason: Option<&str>,
    unfinished_tool_calls: usize,
) -> Option<String> {
    incomplete_reason(contract, stop_reason)
        .map(str::to_string)
        .or_else(|| {
            if matches!(
                contract,
                Contract::AnthropicMessages | Contract::OpenAiChatCompletions
            ) && unfinished_tool_calls > 0
            {
                Some("unfinished_tool_call".to_string())
            } else if matches!(
                (contract, stop_reason),
                (Contract::AnthropicMessages, Some("max_tokens"))
                    | (Contract::OpenAiChatCompletions, Some("length"))
            ) {
                Some("output_token_limit".to_string())
            } else {
                None
            }
        })
}

pub fn malformed_tool_arguments(contract: Contract, body: &str) -> bool {
    contract == Contract::ChatGptResponses
        && body.contains("stream protocol error [chatgpt-responses/finalize]")
        && body.contains("function item")
        && body.contains("has malformed arguments")
}

pub const MAX_INCOMPLETE_RESPONSE_RECOVERIES: u32 = 3;
const CHECKPOINT_MAGIC: &[u8; 4] = b"DRC1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HaltReason {
    ContentFilter,
    RetryBudget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Ready,
    Retrying(u8),
    Halted(HaltReason),
}

/// Only the incomplete-response continuation, not the full Dext turn state.
/// Request effort, provider configuration and history remain host-owned inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecoveryState {
    phase: Phase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation<'a> {
    Reset,
    Response {
        incomplete_reason: Option<&'a str>,
        executable_calls: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Proceed,
    Retry { ordinal: u32 },
    Halt(HaltReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub next: RecoveryState,
    pub decision: Decision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointError {
    Length,
    Version,
    InvalidState,
}

impl RecoveryState {
    /// Pure proposal: computing this does not mutate the committed baseline.
    pub fn advance(self, observation: Observation<'_>) -> Transition {
        let (phase, decision) = match observation {
            Observation::Reset => (Phase::Ready, Decision::Proceed),
            Observation::Response { .. } if matches!(self.phase, Phase::Halted(_)) => {
                let Phase::Halted(reason) = self.phase else {
                    unreachable!()
                };
                (self.phase, Decision::Halt(reason))
            }
            Observation::Response {
                incomplete_reason: None,
                ..
            }
            | Observation::Response {
                executable_calls: true,
                ..
            } => (Phase::Ready, Decision::Proceed),
            Observation::Response {
                incomplete_reason: Some("content_filter"),
                ..
            } => (
                Phase::Halted(HaltReason::ContentFilter),
                Decision::Halt(HaltReason::ContentFilter),
            ),
            Observation::Response {
                incomplete_reason: Some(_),
                ..
            } => {
                let count = match self.phase {
                    Phase::Retrying(count) => count,
                    Phase::Ready => 0,
                    Phase::Halted(_) => unreachable!(),
                };
                if u32::from(count) >= MAX_INCOMPLETE_RESPONSE_RECOVERIES {
                    (
                        Phase::Halted(HaltReason::RetryBudget),
                        Decision::Halt(HaltReason::RetryBudget),
                    )
                } else {
                    let ordinal = count + 1;
                    (
                        Phase::Retrying(ordinal),
                        Decision::Retry {
                            ordinal: u32::from(ordinal),
                        },
                    )
                }
            }
        };
        Transition {
            next: Self { phase },
            decision,
        }
    }

    /// Fixed-size, versioned checkpoint; no allocation or permissive trailing bytes.
    /// The host must additionally pin the implementation and surrounding turn state.
    pub fn checkpoint(self) -> [u8; 6] {
        let (tag, count) = match self.phase {
            Phase::Ready => (0, 0),
            Phase::Retrying(count) => (1, count),
            Phase::Halted(HaltReason::ContentFilter) => (2, 0),
            Phase::Halted(HaltReason::RetryBudget) => (3, 0),
        };
        [b'D', b'R', b'C', b'1', tag, count]
    }

    pub fn restore(bytes: &[u8]) -> Result<Self, CheckpointError> {
        if bytes.len() != 6 {
            return Err(CheckpointError::Length);
        }
        if &bytes[..4] != CHECKPOINT_MAGIC {
            return Err(CheckpointError::Version);
        }
        let phase = match (bytes[4], bytes[5]) {
            (0, 0) => Phase::Ready,
            (1, count) if (1..=MAX_INCOMPLETE_RESPONSE_RECOVERIES).contains(&u32::from(count)) => {
                Phase::Retrying(count)
            }
            (2, 0) => Phase::Halted(HaltReason::ContentFilter),
            (3, 0) => Phase::Halted(HaltReason::RetryBudget),
            _ => return Err(CheckpointError::InvalidState),
        };
        Ok(Self { phase })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn incomplete() -> Observation<'static> {
        Observation::Response {
            incomplete_reason: Some("output_token_limit"),
            executable_calls: false,
        }
    }

    #[test]
    fn retry_budget_survives_every_checkpoint_and_requires_explicit_reset() {
        let mut state = RecoveryState::default();
        for ordinal in 1..=MAX_INCOMPLETE_RESPONSE_RECOVERIES {
            let before = state.checkpoint();
            let proposed = state.advance(incomplete());
            assert_eq!(state.checkpoint(), before, "speculation changed baseline");
            assert_eq!(state.advance(incomplete()), proposed, "replay diverged");
            assert_eq!(proposed.decision, Decision::Retry { ordinal });
            state = RecoveryState::restore(&proposed.next.checkpoint()).unwrap();
        }
        let halted = state.advance(incomplete());
        assert_eq!(halted.decision, Decision::Halt(HaltReason::RetryBudget));
        let halted = RecoveryState::restore(&halted.next.checkpoint()).unwrap();
        for observation in [
            incomplete(),
            Observation::Response {
                incomplete_reason: None,
                executable_calls: true,
            },
        ] {
            assert_eq!(
                halted.advance(observation).decision,
                Decision::Halt(HaltReason::RetryBudget)
            );
        }
        let reset = halted.advance(Observation::Reset);
        assert_eq!(reset.next, RecoveryState::default());
        assert_eq!(
            reset.next.advance(incomplete()).decision,
            Decision::Retry { ordinal: 1 }
        );
    }

    #[test]
    fn content_filter_halts_without_retry_but_executable_calls_preserve_existing_policy() {
        let state = RecoveryState::default().advance(incomplete()).next;
        let filtered = Observation::Response {
            incomplete_reason: Some("content_filter"),
            executable_calls: false,
        };
        let halt = state.advance(filtered);
        assert_eq!(halt.decision, Decision::Halt(HaltReason::ContentFilter));
        assert_eq!(
            RecoveryState::restore(&halt.next.checkpoint()).unwrap(),
            halt.next
        );
        for reason in [Some("content_filter"), Some("unfinished_tool_call"), None] {
            let transition = state.advance(Observation::Response {
                incomplete_reason: reason,
                executable_calls: true,
            });
            assert_eq!(transition.decision, Decision::Proceed);
            assert_eq!(transition.next, RecoveryState::default());
        }
    }

    #[test]
    fn checkpoint_decoder_accepts_only_canonical_states() {
        let mut accepted = 0;
        for tag in 0..=u8::MAX {
            for count in 0..=u8::MAX {
                let bytes = [b'D', b'R', b'C', b'1', tag, count];
                match RecoveryState::restore(&bytes) {
                    Ok(state) => {
                        accepted += 1;
                        assert_eq!(state.checkpoint(), bytes);
                    }
                    Err(error) => assert_eq!(error, CheckpointError::InvalidState),
                }
            }
        }
        assert_eq!(accepted, 6);
        let bytes = RecoveryState::default().checkpoint();
        for length in 0..bytes.len() {
            assert_eq!(
                RecoveryState::restore(&bytes[..length]),
                Err(CheckpointError::Length)
            );
        }
        let mut trailing = bytes.to_vec();
        trailing.push(0);
        assert_eq!(
            RecoveryState::restore(&trailing),
            Err(CheckpointError::Length)
        );
        let mut future = bytes;
        future[3] = b'2';
        assert_eq!(
            RecoveryState::restore(&future),
            Err(CheckpointError::Version)
        );
    }

    #[test]
    fn bounded_observation_traces_match_legacy_counter_decisions() {
        let alphabet = [
            Observation::Reset,
            incomplete(),
            Observation::Response {
                incomplete_reason: Some("content_filter"),
                executable_calls: false,
            },
            Observation::Response {
                incomplete_reason: Some("unfinished_tool_call"),
                executable_calls: true,
            },
            Observation::Response {
                incomplete_reason: None,
                executable_calls: false,
            },
        ];
        for schedule in 0..5usize.pow(7) {
            let mut choices = schedule;
            let mut state = RecoveryState::default();
            let mut legacy_count = 0u32;
            for _ in 0..7 {
                let observation = alphabet[choices % alphabet.len()];
                choices /= alphabet.len();
                let expected = match observation {
                    Observation::Reset => {
                        legacy_count = 0;
                        Decision::Proceed
                    }
                    Observation::Response {
                        incomplete_reason: Some(reason),
                        executable_calls: false,
                    } => {
                        if reason == "content_filter" {
                            Decision::Halt(HaltReason::ContentFilter)
                        } else {
                            legacy_count = legacy_count.saturating_add(1);
                            if legacy_count > MAX_INCOMPLETE_RESPONSE_RECOVERIES {
                                Decision::Halt(HaltReason::RetryBudget)
                            } else {
                                Decision::Retry {
                                    ordinal: legacy_count,
                                }
                            }
                        }
                    }
                    _ => {
                        legacy_count = 0;
                        Decision::Proceed
                    }
                };
                let transition = state.advance(observation);
                assert_eq!(
                    transition.decision, expected,
                    "schedule={schedule}, observation={observation:?}"
                );
                state = RecoveryState::restore(&transition.next.checkpoint()).unwrap();
                if matches!(expected, Decision::Halt(_)) {
                    break;
                }
            }
        }
    }

    #[test]
    fn classification_matches_contract_specific_precedence() {
        let contracts = [
            Contract::AnthropicMessages,
            Contract::OpenAiChatCompletions,
            Contract::OpenAiResponses,
            Contract::ChatGptResponses,
        ];
        let stops = [
            None,
            Some("end_turn"),
            Some("completed"),
            Some("max_tokens"),
            Some("length"),
            Some("incomplete"),
            Some("incomplete:content_filter"),
            Some("incomplete:"),
        ];
        for contract in contracts {
            for stop in stops {
                for unfinished in [0, 1, usize::MAX] {
                    let expected = match contract {
                        Contract::OpenAiResponses | Contract::ChatGptResponses => match stop {
                            Some("incomplete") => Some("unknown"),
                            Some("incomplete:content_filter") => Some("content_filter"),
                            Some("incomplete:") => Some(""),
                            _ => None,
                        },
                        Contract::AnthropicMessages | Contract::OpenAiChatCompletions
                            if unfinished > 0 =>
                        {
                            Some("unfinished_tool_call")
                        }
                        Contract::AnthropicMessages if stop == Some("max_tokens") => {
                            Some("output_token_limit")
                        }
                        Contract::OpenAiChatCompletions if stop == Some("length") => {
                            Some("output_token_limit")
                        }
                        _ => None,
                    };
                    assert_eq!(
                        stream_recovery_reason(contract, stop, unfinished).as_deref(),
                        expected,
                        "{contract:?} {stop:?} {unfinished}"
                    );
                }
            }
        }
    }

    #[test]
    fn malformed_arguments_recovery_stays_exactly_scoped() {
        let body = "stream protocol error [chatgpt-responses/finalize]: function item 0 has malformed arguments";
        assert!(malformed_tool_arguments(Contract::ChatGptResponses, body));
        for contract in [
            Contract::OpenAiResponses,
            Contract::AnthropicMessages,
            Contract::OpenAiChatCompletions,
        ] {
            assert!(!malformed_tool_arguments(contract, body));
        }
        for changed in [
            body.replace("finalize", "event"),
            body.replace("malformed", "incomplete"),
            body.replace("function item", "message"),
        ] {
            assert!(!malformed_tool_arguments(
                Contract::ChatGptResponses,
                &changed
            ));
        }
    }
}

pub fn output_has_auth_failure_markers(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    [
        "unauthorized",
        "invalid crumb",
        "invalid api key",
        "api key is invalid",
        "api key required",
        "unable to access this feature",
        "http 401",
        "http 403",
        "status 401",
        "status 403",
        "forbidden",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Auth,
    Transient,
    Schema,
    Permanent,
}

impl FailureKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Transient => "transient",
            Self::Schema => "schema",
            Self::Permanent => "permanent",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPlan {
    pub kind: FailureKind,
    pub retry: bool,
}

impl RetryPlan {
    pub fn label(self) -> &'static str {
        self.kind.label()
    }
}

/// Classify a mid-stream error event body. Many providers return HTTP 200 and then
/// emit an error inside the SSE stream — those never go through classify_http_failure.
/// Examples that should be retryable (transient server hiccup, not the agent's fault):
///   - ZAI/GLM: `{"error":{"code":"1234","message":"Internal network failure, ... please contact customer service."}}`
///   - Anthropic: `{"type":"error","error":{"type":"overloaded_error", ...}}`
///   - OpenAI: `{"error":{"code":"server_error", ...}}` with messages like "try again"
pub fn classify_stream_error(body: &str) -> RetryPlan {
    let lower = body.to_ascii_lowercase();

    // Responses streams that die inside a `response.incomplete` terminal event
    // (missing/conflicting response objects) are backend truncation, not a
    // request defect. Retry the request — bounded upstream by
    // MAX_STREAM_ATTEMPTS — instead of failing the
    // turn permanently. This must run before the token-limit rule below
    // because these bodies can mention `max_output_tokens`.
    if lower.contains("stream protocol error") && lower.contains("response.incomplete") {
        return RetryPlan {
            kind: FailureKind::Transient,
            retry: true,
        };
    }

    if lower
        == "stream protocol error [anthropic-messages/finalize]: stream ended before message_stop"
    {
        return RetryPlan {
            kind: FailureKind::Transient,
            retry: true,
        };
    }

    // Explicit non-retryable signals win first (context exhausted, quota, auth).
    if lower.contains("context_length_exceeded")
        || lower.contains("context length")
        || lower.contains("max_tokens")
        || lower.contains("token limit")
    {
        return RetryPlan {
            kind: FailureKind::Schema,
            retry: false,
        };
    }
    if lower.contains("usage_limit_reached")
        || lower.contains("usage_not_included")
        || lower.contains("quota")
    {
        return RetryPlan {
            kind: FailureKind::Permanent,
            retry: false,
        };
    }
    if lower.contains("invalid api key")
        || lower.contains("invalid_api_key")
        || lower.contains("unauthorized")
        || lower.contains("authentication")
    {
        return RetryPlan {
            kind: FailureKind::Auth,
            retry: false,
        };
    }

    if [
        "invalid_request_error",
        "invalid_request",
        "permission_error",
        "permission_denied",
        "content_filter",
        "unsupported_parameter",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return RetryPlan {
            kind: FailureKind::Schema,
            retry: false,
        };
    }

    // Transient markers — retry with backoff.
    let transient_phrases = [
        "internal network failure",
        "network error",
        "please contact customer service",
        "overloaded",
        "service_unavailable",
        "service unavailable",
        "temporarily unavailable",
        "transport error",
        "connection closed",
        "connection reset",
        "unexpected eof",
        "chunked response",
        "chunk size line",
        "error decoding response body",
        "incomplete message",
        "rate limit",
        "rate_limit",
        "try again",
        "retry your request",
        "server_error",
        "internal server error",
        "gateway timeout",
        "bad gateway",
        "upstream",
    ];
    if transient_phrases.iter().any(|p| lower.contains(p)) {
        return RetryPlan {
            kind: FailureKind::Transient,
            retry: true,
        };
    }

    // ZAI-style numeric error codes wrapped as strings. Treat unknown numeric codes as
    // transient — their service only returns string codes on server-side issues.
    if let Some(idx) = lower.find("\"code\":\"") {
        let start = idx + "\"code\":\"".len();
        if let Some(end) = lower[start..].find('"') {
            let code = &lower[start..start + end];
            if code.chars().all(|c| c.is_ascii_digit()) && !code.is_empty() {
                return RetryPlan {
                    kind: FailureKind::Transient,
                    retry: true,
                };
            }
        }
    }

    RetryPlan {
        kind: FailureKind::Permanent,
        retry: false,
    }
}
