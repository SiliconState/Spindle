//! Pure assistant-response admission after native provider parsing.
//! Native adapters remain responsible for JSON/schema and terminal validation.
//! A proposal is not a committed receipt, permission grant or dispatch authority.

use std::collections::BTreeSet;

pub struct CallRef<'a> {
    pub id: &'a str,
    pub name: &'a str,
}

pub enum BlockKind<'a> {
    Content,
    CompleteCall(CallRef<'a>),
    IncompleteCall,
    Invalid,
}

/// Trusted native adapter contract: complete calls must already have validated
/// arguments; classification and consumption must describe the same immutable block.
pub trait AssistantBlock: Sized {
    type Input;
    fn kind(&self) -> BlockKind<'_>;
    fn into_call(self) -> Option<ToolCall<Self::Input>>;
}

#[derive(Clone, PartialEq)]
pub struct ToolCall<I> {
    pub id: String,
    pub name: String,
    pub input: I,
}

impl<I> std::fmt::Debug for ToolCall<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCall").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Unfinalized,
    IncompleteCall,
    InvalidBlock,
    MissingIdentity,
    DuplicateIdentity,
    Bounds,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "assistant response proposal: {self:?}")
    }
}
impl std::error::Error for Error {}

pub const MAX_BLOCKS: usize = 4096;
pub const MAX_CALLS: usize = 256;
pub const MAX_IDENTITY_BYTES: usize = 4096;

pub struct ResponseProposal<B, M> {
    blocks: Vec<B>,
    metadata: M,
    calls: usize,
}

impl<B, M> std::fmt::Debug for ResponseProposal<B, M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseProposal")
            .field("blocks", &self.blocks.len())
            .field("calls", &self.calls)
            .finish_non_exhaustive()
    }
}

impl<B: AssistantBlock, M> ResponseProposal<B, M> {
    pub fn from_validated(blocks: Vec<B>, metadata: M, finalized: bool) -> Result<Self, Error> {
        if !finalized {
            return Err(Error::Unfinalized);
        }
        if blocks.len() > MAX_BLOCKS {
            return Err(Error::Bounds);
        }
        let mut ids = BTreeSet::new();
        for block in &blocks {
            match block.kind() {
                BlockKind::Content => {}
                BlockKind::IncompleteCall => return Err(Error::IncompleteCall),
                BlockKind::Invalid => return Err(Error::InvalidBlock),
                BlockKind::CompleteCall(call) => {
                    if call.id.trim().is_empty() || call.name.trim().is_empty() {
                        return Err(Error::MissingIdentity);
                    }
                    if call.id.len() > MAX_IDENTITY_BYTES
                        || call.name.len() > MAX_IDENTITY_BYTES
                        || ids.len() >= MAX_CALLS
                    {
                        return Err(Error::Bounds);
                    }
                    if !ids.insert(call.id) {
                        return Err(Error::DuplicateIdentity);
                    }
                }
            }
        }
        let calls = ids.len();
        Ok(Self {
            blocks,
            metadata,
            calls,
        })
    }
    pub fn blocks(&self) -> &[B] {
        &self.blocks
    }
    pub fn metadata(&self) -> &M {
        &self.metadata
    }
    pub fn call_count(&self) -> usize {
        self.calls
    }
    pub fn into_calls(self) -> Vec<ToolCall<B::Input>> {
        self.blocks
            .into_iter()
            .filter_map(AssistantBlock::into_call)
            .collect()
    }
    pub fn into_parts(self) -> (Vec<B>, M) {
        (self.blocks, self.metadata)
    }
}
