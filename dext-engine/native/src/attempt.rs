//! The response half of one provider attempt, with no transport: the host's HTTP
//! layer (reqwest natively, `fetch` in an app) pushes in what it receives and gets
//! the recorded response, or why there is none. Native and embedded sends record the
//! same bytes for the same stream. Failures never imply that the request was not
//! executed or billed.
use crate::sse::{SseDecoder, SseFrame};
use crate::streaming::{ProviderStreamParser, StreamUpdate, sse_event_cap};
use dext_core::model::{Identity, ModelWait, Phase};
use dext_core::recovery::Contract;
use std::time::Duration;

/// Largest response body one attempt reads.
pub const MAX_WIRE_RESPONSE: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptError {
    Invalid,
    Unauthorized,
    Interrupted,
    Deadline,
    Transport,
    Http(u16),
    Protocol,
    Exhausted,
}
impl std::fmt::Display for AttemptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "provider attempt: {self:?}")
    }
}
impl std::error::Error for AttemptError {}

/// What a provider answered when it refused a request with a non-2xx status before
/// any stream: the status, its `Retry-After` and the start of the body. Diagnostic
/// input for host retry policy; it never proves the request was not billed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rejection {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub body: String,
}

/// Longest rejection body kept.
pub const MAX_REJECTION_BODY: usize = 4096;

/// Stream frames that come before any output: the message/response opening, keepalives,
/// and Chat Completions chunks whose delta carries nothing yet.
fn preamble(frame: &SseFrame) -> bool {
    let data = frame.data.as_deref().unwrap_or_default();
    if data.is_empty() {
        return true;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        return false;
    };
    let kind = frame
        .event
        .as_deref()
        .or_else(|| value["type"].as_str())
        .unwrap_or_default();
    if matches!(
        kind,
        "message_start" | "ping" | "response.created" | "response.in_progress" | "response.queued"
    ) {
        return true;
    }
    value["choices"].as_array().is_some_and(|choices| {
        choices.iter().all(|choice| {
            choice["delta"].as_object().is_none_or(|delta| {
                delta
                    .iter()
                    .all(|(key, value)| key == "role" || value.is_null())
            })
        })
    })
}

/// A streamed error as the refusal it stands for: Anthropic `error` events, Responses
/// `error`/`response.failed`, and Chat Completions `{"error": ...}` chunks. The status
/// is the HTTP status the same refusal has when sent before the stream.
fn stream_refusal(frame: &SseFrame) -> Option<Rejection> {
    let data = frame.data.as_deref()?;
    let value = serde_json::from_str::<serde_json::Value>(data).ok()?;
    let kind = frame
        .event
        .as_deref()
        .or_else(|| value["type"].as_str())
        .unwrap_or_default();
    let error = match kind {
        "response.failed" => &value["response"]["error"],
        "error" => {
            if value["error"].is_object() {
                &value["error"]
            } else {
                &value
            }
        }
        _ if value["error"].is_object() && value.get("choices").is_none() => &value["error"],
        _ => return None,
    };
    let code = [&error["type"], &error["code"]]
        .iter()
        .filter_map(|field| field.as_str())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ");
    let status = if code.contains("overloaded") {
        529
    } else if code.contains("rate_limit") {
        429
    } else if code.contains("context_length")
        || code.contains("invalid_request")
        || code.contains("request_too_large")
    {
        400
    } else if code.contains("timeout") {
        504
    } else {
        500
    };
    let mut body = data.to_string();
    if body.len() > MAX_REJECTION_BODY {
        let mut end = MAX_REJECTION_BODY;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
    }
    Some(Rejection {
        status,
        retry_after: None,
        body,
    })
}

/// The contract named by an identity, or `Invalid`.
pub fn contract(identity: &Identity) -> Result<Contract, AttemptError> {
    match identity.contract.as_str() {
        "anthropic-messages" => Ok(Contract::AnthropicMessages),
        "openai-chat-completions" => Ok(Contract::OpenAiChatCompletions),
        "openai-responses" => Ok(Contract::OpenAiResponses),
        "chatgpt-responses" => Ok(Contract::ChatGptResponses),
        _ => Err(AttemptError::Invalid),
    }
}

/// Whether a response's `Content-Type` allows reading it as the stream. The ChatGPT
/// Codex backend streams SSE without any Content-Type header, so only a declared
/// non-SSE type is refused; the SSE parser validates framing.
pub fn event_stream(content_type: Option<&str>) -> bool {
    content_type.is_none_or(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("text/event-stream"))
    })
}

/// A refusal before any stream: `status`, its `Retry-After` header and up to
/// `MAX_REJECTION_BODY` bytes of its body.
pub fn rejection(status: u16, retry_after: Option<&str>, body: &[u8]) -> Rejection {
    Rejection {
        status,
        retry_after: retry_after
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs),
        body: String::from_utf8_lossy(&body[..body.len().min(MAX_REJECTION_BODY)]).into_owned(),
    }
}

/// A host without its own URL parser checks an endpoint here before it sends: HTTPS,
/// or HTTP to a literal loopback address, with no user info and no fragment.
pub fn endpoint_allowed(endpoint: &str) -> bool {
    let lower = endpoint.to_ascii_lowercase();
    let (secure, rest) = match (
        lower.strip_prefix("https://"),
        lower.strip_prefix("http://"),
    ) {
        (Some(rest), _) => (true, rest),
        (None, Some(rest)) => (false, rest),
        _ => return false,
    };
    if endpoint.contains('#') {
        return false;
    }
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    if secure {
        return true;
    }
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    };
    // An IPv6 address is written in brackets (`[::1]:8080`).
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// One attempt's response stream, parsed as it arrives.
pub struct WireAttempt {
    decoder: SseDecoder,
    parser: ProviderStreamParser,
    total: usize,
    generating: bool,
    rejection: Option<Rejection>,
}

impl WireAttempt {
    /// For the request `waiting` is about to send (it must be waiting).
    pub fn new<B: AsRef<[u8]> + Clone>(
        waiting: &ModelWait<B>,
        capture_reasoning: bool,
        preserve_timing_cache: bool,
    ) -> Result<Self, AttemptError> {
        if waiting.phase() != Phase::Waiting {
            return Err(AttemptError::Invalid);
        }
        let contract = contract(waiting.request().identity())?;
        Ok(Self {
            decoder: SseDecoder::new(sse_event_cap(contract)),
            parser: ProviderStreamParser::new(contract, preserve_timing_cache)
                .capture_openai_reasoning(capture_reasoning),
            total: 0,
            generating: false,
            rejection: None,
        })
    }

    /// Takes the next chunk of the body: the preview updates it completes. An error
    /// the provider streams before any output is a refusal like an HTTP one
    /// (`Http(status)`; `take_rejection` has its detail).
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamUpdate>, AttemptError> {
        self.total = self
            .total
            .checked_add(chunk.len())
            .ok_or(AttemptError::Exhausted)?;
        if self.total > MAX_WIRE_RESPONSE {
            return Err(AttemptError::Exhausted);
        }
        let mut updates = Vec::new();
        for frame in self
            .decoder
            .push(chunk)
            .map_err(|_| AttemptError::Protocol)?
        {
            match stream_refusal(&frame) {
                Some(rejection) if !self.generating => {
                    let status = rejection.status;
                    self.rejection = Some(rejection);
                    return Err(AttemptError::Http(status));
                }
                None if !preamble(&frame) => self.generating = true,
                _ => {}
            }
            updates.extend(
                self.parser
                    .push_frame(frame)
                    .map_err(|_| AttemptError::Protocol)?,
            );
        }
        Ok(updates)
    }

    /// The refusal behind the last `Http` error from `push`, taken once.
    pub fn take_rejection(&mut self) -> Option<Rejection> {
        self.rejection.take()
    }

    /// The body ended: the response recorded for `waiting`, passing the last preview
    /// updates to `observe`.
    pub fn finish<B: AsRef<[u8]> + Clone>(
        self,
        waiting: &ModelWait<B>,
        observe: &dyn Fn(&StreamUpdate),
    ) -> Result<ModelWait<B>, AttemptError> {
        let mut parser = self.parser;
        for frame in self.decoder.finish().map_err(|_| AttemptError::Protocol)? {
            for update in parser
                .push_frame(frame)
                .map_err(|_| AttemptError::Protocol)?
            {
                observe(&update);
            }
        }
        let response = parser.finish().map_err(|_| AttemptError::Protocol)?;
        let bytes = response.checkpoint().map_err(|_| AttemptError::Exhausted)?;
        waiting
            .record(waiting.request(), waiting.attempt(), &bytes)
            .map_err(|_| AttemptError::Protocol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dext_core::model::{Encoding, PreparedRequest};

    fn waiting(contract: &str) -> ModelWait<Vec<u8>> {
        ModelWait::new(
            PreparedRequest::new(
                Identity {
                    effect: "fixture/m/1".into(),
                    provider: "fixture".into(),
                    contract: contract.into(),
                    endpoint: "https://example.invalid/v1".into(),
                    model: "fixture".into(),
                    credential_scope: "operator".into(),
                    configuration: "policy".into(),
                },
                Encoding::Json,
                b"{}".to_vec(),
            )
            .unwrap(),
        )
        .start()
        .unwrap()
    }

    const CHAT: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    #[test]
    fn endpoints_need_https_or_literal_loopback_http() {
        for good in [
            "https://api.example.com/v1/chat",
            "HTTPS://api.example.com",
            "http://127.0.0.1:8080/v1",
            "http://127.9.9.9/x?y=1",
            "http://[::1]:8080/v1",
            "http://[::1]/v1",
        ] {
            assert!(endpoint_allowed(good), "{good}");
        }
        for bad in [
            "http://example.com/v1",
            "http://localhost:8080/v1",
            "https://user:pass@example.com/",
            "https://example.com/#frag",
            "ftp://example.com",
            "https://",
            "http://10.0.0.1/",
            "http://[::2]:8080/",
            "http://::1/",
        ] {
            assert!(!endpoint_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn only_a_declared_non_sse_type_is_refused() {
        assert!(event_stream(None));
        assert!(event_stream(Some("text/event-stream; charset=utf-8")));
        assert!(event_stream(Some(" Text/Event-Stream")));
        assert!(!event_stream(Some("application/json")));
    }

    #[test]
    fn rejections_keep_status_retry_after_and_a_bounded_body() {
        let body = vec![b'x'; MAX_REJECTION_BODY + 10];
        let refused = rejection(429, Some(" 12 "), &body);
        assert_eq!(refused.status, 429);
        assert_eq!(refused.retry_after, Some(Duration::from_secs(12)));
        assert_eq!(refused.body.len(), MAX_REJECTION_BODY);
        assert_eq!(rejection(503, Some("soon"), b"").retry_after, None);
    }

    #[test]
    fn a_stream_records_the_same_response_however_it_is_chunked() {
        let waiting = self::waiting("openai-chat-completions");
        let whole = {
            let mut attempt = WireAttempt::new(&waiting, false, false).unwrap();
            let updates = attempt.push(CHAT.as_bytes()).unwrap();
            assert!(!updates.is_empty(), "text deltas preview the reply");
            attempt.finish(&waiting, &|_| {}).unwrap()
        };
        let mut attempt = WireAttempt::new(&waiting, false, false).unwrap();
        for byte in CHAT.as_bytes() {
            attempt.push(std::slice::from_ref(byte)).unwrap();
        }
        let split = attempt.finish(&waiting, &|_| {}).unwrap();
        assert_eq!(whole.checkpoint(), split.checkpoint());
        assert_eq!(whole.phase(), Phase::Recorded);
    }

    #[test]
    fn an_error_streamed_before_any_output_is_a_refusal() {
        let waiting = self::waiting("anthropic-messages");
        let mut attempt = WireAttempt::new(&waiting, false, false).unwrap();
        let stream = concat!(
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n",
        );
        assert_eq!(
            attempt.push(stream.as_bytes()).unwrap_err(),
            AttemptError::Http(529)
        );
        assert_eq!(attempt.take_rejection().unwrap().status, 529);
        assert!(attempt.take_rejection().is_none(), "taken once");
    }

    #[test]
    fn oversized_and_malformed_streams_fail_closed() {
        let fixture = self::waiting("openai-chat-completions");
        let waiting = &fixture;
        let mut attempt = WireAttempt::new(waiting, false, false).unwrap();
        assert_eq!(
            attempt
                .push(&vec![b' '; MAX_WIRE_RESPONSE + 1])
                .unwrap_err(),
            AttemptError::Exhausted
        );
        let mut attempt = WireAttempt::new(waiting, false, false).unwrap();
        attempt
            .push(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n")
            .unwrap();
        assert!(
            attempt.finish(waiting, &|_| {}).is_err(),
            "no end of stream"
        );
        assert_eq!(
            WireAttempt::new(&self::waiting("unknown"), false, false).err(),
            Some(AttemptError::Invalid)
        );
    }
}
