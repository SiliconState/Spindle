//! Prepared model effects and their local wait state. No transport or credentials.
//! Hosts commit these transitions and revalidate authorization before dispatch.
//! Body owners must provide immutable bytes (for example Vec<u8> or Bytes).

const MAGIC: &[u8; 4] = b"DMW1";
pub const MAX_BODY: usize = 64 * 1024 * 1024;
pub const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_LABEL: usize = 4096;
const MAX_CHECKPOINT: usize = MAX_BODY + MAX_RESPONSE + 8 * MAX_LABEL + 128;

#[derive(Clone, PartialEq, Eq)]
pub struct Identity {
    pub effect: String,
    pub provider: String,
    pub contract: String,
    pub endpoint: String,
    pub model: String,
    /// Opaque authorization scope/version supplied by the host, never a token/hash of a secret.
    pub credential_scope: String,
    /// Frozen effective configuration identity, including effort and policy provenance.
    pub configuration: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Json,
    Gzip,
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedRequest<B> {
    identity: Identity,
    encoding: Encoding,
    body: B,
}

impl<B: AsRef<[u8]>> std::fmt::Debug for PreparedRequest<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedRequest")
            .field("encoding", &self.encoding)
            .field("body_len", &self.body.as_ref().len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Bound,
    Identity,
    State,
    Conflict,
    Version,
    Encoding,
    Truncated,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "model wait: {self:?}")
    }
}
impl std::error::Error for Error {}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity").finish_non_exhaustive()
    }
}

impl Identity {
    fn fields(&self) -> [&str; 7] {
        [
            &self.effect,
            &self.provider,
            &self.contract,
            &self.endpoint,
            &self.model,
            &self.credential_scope,
            &self.configuration,
        ]
    }
    fn validate(&self) -> Result<(), Error> {
        if self
            .fields()
            .iter()
            .any(|v| v.is_empty() || v.len() > MAX_LABEL || v.chars().any(char::is_control))
        {
            return Err(Error::Identity);
        }
        Ok(())
    }
}

impl<B: AsRef<[u8]>> PreparedRequest<B> {
    pub fn new(identity: Identity, encoding: Encoding, body: B) -> Result<Self, Error> {
        identity.validate()?;
        if body.as_ref().is_empty() || body.as_ref().len() > MAX_BODY {
            return Err(Error::Bound);
        }
        Ok(Self {
            identity,
            encoding,
            body,
        })
    }
    pub fn identity(&self) -> &Identity {
        &self.identity
    }
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }
    pub fn body(&self) -> &[u8] {
        self.body.as_ref()
    }
    pub fn body_owner(&self) -> &B {
        &self.body
    }
    pub fn matches<C: AsRef<[u8]>>(&self, other: &PreparedRequest<C>) -> bool {
        self.identity == other.identity
            && self.encoding == other.encoding
            && self.body() == other.body()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Prepared,
    Waiting,
    Recorded,
    Applied,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ModelWait<B> {
    request: PreparedRequest<B>,
    attempt: u32,
    phase: Phase,
    response: Option<Vec<u8>>,
}

impl<B: AsRef<[u8]>> std::fmt::Debug for ModelWait<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelWait")
            .field("phase", &self.phase)
            .field("attempt", &self.attempt)
            .field("response_len", &self.response.as_ref().map(Vec::len))
            .finish_non_exhaustive()
    }
}

impl<B: AsRef<[u8]> + Clone> ModelWait<B> {
    pub fn new(request: PreparedRequest<B>) -> Self {
        Self {
            request,
            attempt: 0,
            phase: Phase::Prepared,
            response: None,
        }
    }
    pub fn request(&self) -> &PreparedRequest<B> {
        &self.request
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
    pub fn response(&self) -> Option<&[u8]> {
        self.response.as_deref()
    }

    /// Proposal only: the returned state's attempt must be committed before sending.
    pub fn start(&self) -> Result<Self, Error> {
        if self.phase != Phase::Prepared {
            return Err(Error::State);
        }
        let mut next = self.clone();
        next.attempt = next.attempt.checked_add(1).ok_or(Error::Bound)?;
        next.phase = Phase::Waiting;
        Ok(next)
    }

    /// Explicitly records willingness to replace an unrecorded attempt. The prior
    /// request may already have run or been billed; this is NOT exactly-once send.
    /// The native host owns retry policy, budget and proof of local request cleanup.
    pub fn abandon_unrecorded_for_retry(&self) -> Result<Self, Error> {
        if self.phase != Phase::Waiting {
            return Err(Error::State);
        }
        let mut next = self.clone();
        next.phase = Phase::Prepared;
        Ok(next)
    }

    /// Only complete, adapter-validated response bytes may enter this method.
    /// Exact prepared identity, scope, bytes and physical attempt are bound together.
    pub fn record<C: AsRef<[u8]>>(
        &self,
        request: &PreparedRequest<C>,
        attempt: u32,
        response: &[u8],
    ) -> Result<Self, Error> {
        if !self.request.matches(request) || attempt != self.attempt {
            return Err(Error::Conflict);
        }
        if response.len() > MAX_RESPONSE || response.is_empty() {
            return Err(Error::Bound);
        }
        if matches!(self.phase, Phase::Recorded | Phase::Applied) {
            return if self.response.as_deref() == Some(response) {
                Ok(self.clone())
            } else {
                Err(Error::Conflict)
            };
        }
        if self.phase != Phase::Waiting {
            return Err(Error::State);
        }
        let mut next = self.clone();
        next.phase = Phase::Recorded;
        next.response = Some(response.to_vec());
        Ok(next)
    }

    /// Host commits assistant state/tool intents in the same transaction as this
    /// consumption. This method executes no tool and offers no permission grant.
    pub fn apply(&self) -> Result<Self, Error> {
        if !matches!(self.phase, Phase::Recorded | Phase::Applied) {
            return Err(Error::State);
        }
        let mut next = self.clone();
        next.phase = Phase::Applied;
        Ok(next)
    }

    /// Sensitive execution data: hosts must approve retention before calling this.
    /// Encoding is framing, not redaction, encryption, authenticity or policy approval.
    pub fn checkpoint(&self) -> Vec<u8> {
        fn field(out: &mut Vec<u8>, bytes: &[u8]) {
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend_from_slice(bytes);
        }
        let mut out = MAGIC.to_vec();
        for value in self.request.identity.fields() {
            field(&mut out, value.as_bytes());
        }
        out.push(match self.request.encoding {
            Encoding::Json => 0,
            Encoding::Gzip => 1,
        });
        field(&mut out, self.request.body());
        out.extend_from_slice(&self.attempt.to_be_bytes());
        out.push(match self.phase {
            Phase::Prepared => 0,
            Phase::Waiting => 1,
            Phase::Recorded => 2,
            Phase::Applied => 3,
        });
        field(&mut out, self.response.as_deref().unwrap_or_default());
        out
    }
}

impl ModelWait<Vec<u8>> {
    pub fn restore(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_CHECKPOINT {
            return Err(Error::Bound);
        }
        let mut input = Decoder(bytes);
        if input.take(4)? != MAGIC {
            return Err(Error::Version);
        }
        let identity = Identity {
            effect: input.label()?,
            provider: input.label()?,
            contract: input.label()?,
            endpoint: input.label()?,
            model: input.label()?,
            credential_scope: input.label()?,
            configuration: input.label()?,
        };
        let encoding = match input.take(1)?[0] {
            0 => Encoding::Json,
            1 => Encoding::Gzip,
            _ => return Err(Error::Encoding),
        };
        let body = input.field(MAX_BODY)?.to_vec();
        let request = PreparedRequest::new(identity, encoding, body)?;
        let attempt = input.number()?;
        let phase = match input.take(1)?[0] {
            0 => Phase::Prepared,
            1 => Phase::Waiting,
            2 => Phase::Recorded,
            3 => Phase::Applied,
            _ => return Err(Error::State),
        };
        let bytes = input.field(MAX_RESPONSE)?;
        let response = if bytes.is_empty() {
            None
        } else {
            Some(bytes.to_vec())
        };
        if !input.0.is_empty()
            || (phase != Phase::Prepared && attempt == 0)
            || (matches!(phase, Phase::Recorded | Phase::Applied) != response.is_some())
        {
            return Err(Error::State);
        }
        Ok(Self {
            request,
            attempt,
            phase,
            response,
        })
    }
}

struct Decoder<'a>(&'a [u8]);
impl<'a> Decoder<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], Error> {
        if length > self.0.len() {
            return Err(Error::Truncated);
        }
        let (value, rest) = self.0.split_at(length);
        self.0 = rest;
        Ok(value)
    }
    fn number(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().map_err(|_| Error::Truncated)?,
        ))
    }
    fn field(&mut self, bound: usize) -> Result<&'a [u8], Error> {
        let length = self.number()? as usize;
        if length > bound {
            return Err(Error::Bound);
        }
        self.take(length)
    }
    fn label(&mut self) -> Result<String, Error> {
        String::from_utf8(self.field(MAX_LABEL)?.to_vec()).map_err(|_| Error::Identity)
    }
}
