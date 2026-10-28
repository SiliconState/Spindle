//! One physical provider attempt. Authorization/credential refresh belongs to the
//! host; failures never imply that the remote request was not executed or billed.
pub use crate::attempt::{AttemptError, MAX_WIRE_RESPONSE, Rejection};
use crate::attempt::{WireAttempt, event_stream};
use dext_core::model::{Identity, ModelWait, Phase};
use reqwest::header::{HeaderMap, HeaderValue};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Fresh host authorization for the exact request route and current credential
/// version. Never serialize this value or derive it from saved grant labels.
pub struct LiveRoute {
    identity: Identity,
    headers: HeaderMap,
    capture_reasoning: bool,
    preserve_timing_cache: bool,
}
impl std::fmt::Debug for LiveRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveRoute").finish_non_exhaustive()
    }
}
impl LiveRoute {
    /// The caller authenticates route, model, configuration, credential version and
    /// permission to disclose the complete request before constructing this value.
    pub fn new(identity: Identity, mut headers: HeaderMap) -> anyhow::Result<Self> {
        if headers.len() > 32
            || headers.iter().any(|(name, value)| {
                name.as_str().len() + value.as_bytes().len() > 16 * 1024
                    || matches!(
                        name.as_str(),
                        "host"
                            | "content-length"
                            | "transfer-encoding"
                            | "connection"
                            | "content-encoding"
                            | "content-type"
                            | "accept"
                    )
            })
        {
            anyhow::bail!("invalid live provider headers");
        }
        for value in headers.values_mut() {
            value.set_sensitive(true);
        }
        Ok(Self {
            identity,
            headers,
            capture_reasoning: false,
            preserve_timing_cache: false,
        })
    }
    pub fn capture_reasoning(mut self, enabled: bool) -> Self {
        self.capture_reasoning = enabled;
        self
    }
    pub fn preserve_timing_cache(mut self, enabled: bool) -> Self {
        self.preserve_timing_cache = enabled;
        self
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Deadlines {
    pub connect: Duration,
    pub first_byte: Duration,
    pub idle: Duration,
    pub total: Duration,
}
impl Default for Deadlines {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            first_byte: Duration::from_secs(60),
            idle: Duration::from_secs(60),
            total: Duration::from_secs(300),
        }
    }
}
impl Deadlines {
    fn validate(self) -> anyhow::Result<()> {
        if [self.connect, self.first_byte, self.idle, self.total]
            .iter()
            .any(|value| value.is_zero() || *value > Duration::from_secs(600))
            || self.connect > self.total
            || self.first_byte > self.total
            || self.idle > self.total
        {
            anyhow::bail!("invalid provider deadline bounds");
        }
        Ok(())
    }
}

pub struct ModelTransport {
    client: reqwest::Client,
    deadlines: Deadlines,
    rejection: std::sync::Mutex<Option<Rejection>>,
}
impl ModelTransport {
    /// No redirect, proxy/environment discovery, cookies or implicit retries.
    /// Supported TLS/runtime configuration is host-owned, not a persisted grant.
    pub fn new(deadlines: Deadlines) -> anyhow::Result<Self> {
        deadlines.validate()?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(deadlines.connect)
            .timeout(deadlines.total)
            .build()?;
        Ok(Self {
            client,
            deadlines,
            rejection: std::sync::Mutex::new(None),
        })
    }

    /// The rejection behind the most recent `AttemptError::Http` from this transport,
    /// taken once.
    pub fn take_rejection(&self) -> Option<Rejection> {
        self.rejection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// `waiting` must already have a committed dispatch reservation. The trusted
    /// host calls this only through its broker and freshly authorizes the route.
    /// This does not retry, apply a response or emit authoritative preview output.
    pub async fn send_once<B: AsRef<[u8]> + Clone>(
        &self,
        waiting: &ModelWait<B>,
        route: LiveRoute,
        interrupt: &AtomicBool,
    ) -> Result<ModelWait<B>, AttemptError> {
        self.send_once_observed(waiting, route, interrupt, &|_| {})
            .await
    }

    /// As `send_once`, also passing each parsed stream update to `observe` as it
    /// arrives. Updates are an unauthoritative preview: only the recorded response
    /// is adopted, and a preview can be discarded or contradicted by it.
    pub async fn send_once_observed<B: AsRef<[u8]> + Clone>(
        &self,
        waiting: &ModelWait<B>,
        route: LiveRoute,
        interrupt: &AtomicBool,
        observe: &(dyn Fn(&crate::streaming::StreamUpdate) + Sync),
    ) -> Result<ModelWait<B>, AttemptError> {
        if waiting.phase() != Phase::Waiting {
            return Err(AttemptError::Invalid);
        }
        if waiting.request().identity() != &route.identity {
            return Err(AttemptError::Unauthorized);
        }
        let endpoint =
            reqwest::Url::parse(&route.identity.endpoint).map_err(|_| AttemptError::Invalid)?;
        let loopback = endpoint.host_str().is_some_and(|host| {
            host.parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        });
        if !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.fragment().is_some()
            || !(endpoint.scheme() == "https" || (endpoint.scheme() == "http" && loopback))
        {
            return Err(AttemptError::Invalid);
        }
        let mut attempt = WireAttempt::new(
            waiting,
            route.capture_reasoning,
            route.preserve_timing_cache,
        )?;
        let mut request = self
            .client
            .post(endpoint)
            .headers(route.headers)
            .header("content-type", HeaderValue::from_static("application/json"))
            .header("accept", HeaderValue::from_static("text/event-stream"))
            .body(waiting.request().body().to_vec());
        if waiting.request().encoding() == dext_core::model::Encoding::Gzip {
            request = request.header("content-encoding", "gzip");
        }
        if interrupt.load(Ordering::SeqCst) {
            return Err(AttemptError::Interrupted);
        }
        let execution = async {
            use futures_util::StreamExt;
            let response = tokio::time::timeout(self.deadlines.first_byte, request.send())
                .await
                .map_err(|_| AttemptError::Deadline)?
                .map_err(|_| AttemptError::Transport)?;
            if !response.status().is_success() {
                let status = response.status().as_u16();
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .map(String::from);
                let mut response = response;
                let mut body = Vec::new();
                // The refusal is known from its status; a body that stalls is cut short
                // rather than letting the total deadline turn it into an unknown outcome.
                let until = tokio::time::Instant::now() + Duration::from_secs(2);
                while body.len() < crate::attempt::MAX_REJECTION_BODY {
                    match tokio::time::timeout_at(until, response.chunk()).await {
                        Ok(Ok(Some(chunk))) => body.extend_from_slice(&chunk),
                        _ => break,
                    }
                }
                *self
                    .rejection
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(
                    crate::attempt::rejection(status, retry_after.as_deref(), &body),
                );
                return Err(AttemptError::Http(status));
            }
            let content_type = response
                .headers()
                .get("content-type")
                .map(|value| value.to_str().unwrap_or("\u{0}"));
            if !event_stream(content_type) {
                return Err(AttemptError::Protocol);
            }
            let mut stream = response.bytes_stream();
            while let Some(chunk) = tokio::time::timeout(self.deadlines.idle, stream.next())
                .await
                .map_err(|_| AttemptError::Deadline)?
            {
                let chunk = chunk.map_err(|_| AttemptError::Transport)?;
                // An error the provider streams before any output is a refusal like an
                // HTTP one (overloaded, rate limited, context too long).
                match attempt.push(&chunk) {
                    Ok(updates) => updates.iter().for_each(observe),
                    Err(AttemptError::Http(status)) => {
                        *self
                            .rejection
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            attempt.take_rejection();
                        return Err(AttemptError::Http(status));
                    }
                    Err(error) => return Err(error),
                }
            }
            attempt.finish(waiting, observe)
        };
        tokio::pin!(execution);
        let total = tokio::time::sleep(self.deadlines.total);
        tokio::pin!(total);
        let mut ticker = tokio::time::interval(Duration::from_millis(10));
        loop {
            if interrupt.load(Ordering::SeqCst) {
                return Err(AttemptError::Interrupted);
            }
            tokio::select! {
                result = &mut execution => return result,
                _ = &mut total => return Err(AttemptError::Deadline),
                _ = ticker.tick() => {}
            }
        }
    }
}
