//! Opt-in checkpoint codecs. Callers own retention policy, encryption, identity
//! binding and atomic publication; these values do not authorize execution.
use crate::{Block, Message, Usage};
use dext_core::response::{AssistantBlock, BlockKind, MAX_BLOCKS};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::Write;

pub const MAX_CHECKPOINT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_MESSAGES: usize = 4096;
pub const MAX_TRANSCRIPT_BLOCKS: usize = 16384;
const MAX_STRUCTURE: usize = 131072;
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Bounds,
    Encoding,
    InvalidResponse,
    InvalidTranscript,
    InvalidRuntime,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native checkpoint: {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct NativeResponse {
    pub blocks: Vec<Block>,
    pub stop_reason: Option<String>,
    pub usage: Usage,
    pub unknown_events: usize,
    pub unfinished_tool_calls: usize,
}

impl std::fmt::Debug for NativeResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeResponse")
            .field("blocks", &self.blocks.len())
            .finish_non_exhaustive()
    }
}

impl NativeResponse {
    pub fn validate(&self) -> Result<(), Error> {
        if !self.usage.is_valid()
            || self.stop_reason.as_ref().is_some_and(|s| s.len() > 4096)
            || self.unknown_events > u32::MAX as usize
            || self.unfinished_tool_calls > MAX_BLOCKS
        {
            return Err(Error::InvalidResponse);
        }
        validate_blocks(&self.blocks)?;
        if self
            .blocks
            .iter()
            .any(|block| matches!(block.kind(), BlockKind::Invalid | BlockKind::IncompleteCall))
        {
            return Err(Error::InvalidResponse);
        }
        Ok(())
    }

    pub fn checkpoint(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        encode(b"DNR1", self)
    }

    pub fn restore(bytes: &[u8]) -> Result<Self, Error> {
        let response: Self = decode(b"DNR1", bytes)?;
        response.validate()?;
        Ok(response)
    }
}

/// Conversation values only, not a complete turn continuation. The route binding
/// must be checked by the host before provider-bound reasoning is replayed.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct TranscriptCheckpoint {
    pub route_binding: String,
    pub messages: Vec<Message>,
}

impl std::fmt::Debug for TranscriptCheckpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscriptCheckpoint")
            .field("messages", &self.messages.len())
            .finish_non_exhaustive()
    }
}

impl TranscriptCheckpoint {
    pub fn validate(&self) -> Result<(), Error> {
        if self.route_binding.trim().is_empty() || self.route_binding.len() > 4096 {
            return Err(Error::InvalidTranscript);
        }
        if self.messages.len() > MAX_MESSAGES {
            return Err(Error::Bounds);
        }
        let mut blocks = 0usize;
        for message in &self.messages {
            blocks = blocks
                .checked_add(message.content.len())
                .ok_or(Error::Bounds)?;
            if blocks > MAX_TRANSCRIPT_BLOCKS || message.content.len() > MAX_BLOCKS {
                return Err(Error::Bounds);
            }
            if !matches!(
                message.role.as_str(),
                "user" | "assistant" | "system" | "tool"
            ) {
                return Err(Error::InvalidTranscript);
            }
            validate_blocks(&message.content)?;
            for block in &message.content {
                let permitted = match block {
                    Block::Text { .. } => true,
                    Block::ToolResult { .. } => matches!(message.role.as_str(), "user" | "tool"),
                    Block::ImageReference { .. } | Block::ImageArtifact { .. } => {
                        message.role == "user"
                    }
                    _ => message.role == "assistant",
                };
                if !permitted {
                    return Err(Error::InvalidTranscript);
                }
            }
        }
        Ok(())
    }

    pub fn checkpoint(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        encode(b"DNT1", self)
    }

    pub fn restore(bytes: &[u8]) -> Result<Self, Error> {
        let transcript: Self = decode(b"DNT1", bytes)?;
        transcript.validate()?;
        Ok(transcript)
    }
}

fn validate_blocks(blocks: &[Block]) -> Result<(), Error> {
    if blocks.len() > MAX_BLOCKS {
        return Err(Error::Bounds);
    }
    let mut ids = std::collections::BTreeSet::new();
    for block in blocks {
        match block {
            Block::ToolUse { id, name, input } => {
                if id.trim().is_empty()
                    || name.trim().is_empty()
                    || id.len() > 4096
                    || name.len() > 4096
                    || !input.is_object()
                    || !ids.insert(id)
                    || ids.len() > dext_core::response::MAX_CALLS
                {
                    return Err(Error::InvalidTranscript);
                }
            }
            Block::ToolResult { tool_use_id, .. } => {
                if tool_use_id.trim().is_empty() || tool_use_id.len() > 4096 {
                    return Err(Error::InvalidTranscript);
                }
            }
            Block::ResponsesReasoning { item } => {
                if item["type"] != "reasoning"
                    || item["id"].as_str().is_none_or(|s| s.trim().is_empty())
                    || item["encrypted_content"]
                        .as_str()
                        .is_none_or(|s| s.is_empty())
                {
                    return Err(Error::InvalidTranscript);
                }
            }
            Block::ImageArtifact { reference } if !reference.is_valid() => {
                return Err(Error::InvalidTranscript);
            }
            Block::ImageReference {
                path,
                media_type,
                width,
                height,
                source_sha256,
                ..
            } if path.is_empty()
                || path.contains('\0')
                || *width == 0
                || *height == 0
                || !matches!(
                    media_type.as_str(),
                    "image/jpeg" | "image/png" | "image/webp"
                )
                || source_sha256.len() != 64
                || !source_sha256.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                return Err(Error::InvalidTranscript);
            }
            _ => {}
        }
    }
    Ok(())
}

struct BoundedWriter {
    bytes: Vec<u8>,
    structure: StructureBound,
}
impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_CHECKPOINT_BYTES.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("checkpoint bound"));
        }
        self.structure
            .scan(bytes)
            .map_err(|_| std::io::Error::other("checkpoint structure bound"))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn encode<T: Serialize>(magic: &[u8; 4], value: &T) -> Result<Vec<u8>, Error> {
    let mut writer = BoundedWriter {
        bytes: magic.to_vec(),
        structure: StructureBound::default(),
    };
    serde_json::to_writer(&mut writer, value).map_err(|_| Error::Encoding)?;
    Ok(writer.bytes)
}

pub(crate) fn decode<T: DeserializeOwned + Serialize>(
    magic: &[u8; 4],
    bytes: &[u8],
) -> Result<T, Error> {
    if bytes.len() > MAX_CHECKPOINT_BYTES {
        return Err(Error::Bounds);
    }
    if !bytes.starts_with(magic) {
        return Err(Error::Encoding);
    }
    structure_bound(&bytes[4..])?;
    let value = serde_json::from_slice(&bytes[4..]).map_err(|_| Error::Encoding)?;
    // Exact v1 encodings reject unknown/duplicate fields, aliases and trailing data
    // without changing the existing CLI/session serde compatibility contract.
    if encode(magic, &value)? != bytes {
        return Err(Error::Encoding);
    }
    Ok(value)
}

#[derive(Default)]
struct StructureBound {
    quoted: bool,
    escaped: bool,
    depth: usize,
    count: usize,
}
impl StructureBound {
    fn scan(&mut self, bytes: &[u8]) -> Result<(), Error> {
        for &byte in bytes {
            if self.quoted {
                if self.escaped {
                    self.escaped = false;
                } else if byte == b'\\' {
                    self.escaped = true;
                } else if byte == b'"' {
                    self.quoted = false;
                }
            } else {
                match byte {
                    b'"' => self.quoted = true,
                    b'{' | b'[' => {
                        self.depth += 1;
                        self.count += 1;
                    }
                    b'}' | b']' => {
                        self.depth = self.depth.checked_sub(1).ok_or(Error::Encoding)?;
                    }
                    b',' | b':' => self.count += 1,
                    _ => {}
                }
                if self.depth > MAX_DEPTH || self.count > MAX_STRUCTURE {
                    return Err(Error::Bounds);
                }
            }
        }
        Ok(())
    }
}
fn structure_bound(bytes: &[u8]) -> Result<(), Error> {
    StructureBound::default().scan(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::ser::SerializeSeq;
    use std::cell::Cell;

    struct Nested<'a> {
        remaining: usize,
        visited: &'a Cell<usize>,
    }
    impl Serialize for Nested<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            self.visited.set(self.visited.get() + 1);
            let mut sequence = serializer.serialize_seq(Some(1))?;
            if self.remaining > 0 {
                sequence.serialize_element(&Nested {
                    remaining: self.remaining - 1,
                    visited: self.visited,
                })?;
            } else {
                sequence.serialize_element(&0)?;
            }
            sequence.end()
        }
    }
    #[test]
    fn encoder_stops_recursive_serialization_at_depth_limit() {
        let visited = Cell::new(0);
        assert!(
            encode(
                b"TEST",
                &Nested {
                    remaining: 512,
                    visited: &visited
                }
            )
            .is_err()
        );
        assert!(
            visited.get() <= MAX_DEPTH + 1,
            "visited {} levels",
            visited.get()
        );
    }

    #[test]
    fn incremental_structure_scan_matches_whole_input_at_every_split() {
        let value =
            serde_json::json!({"text":"quote\" slash\\ brackets[]{}\n", "nested":[{"k":1}]});
        let bytes = serde_json::to_vec(&value).unwrap();
        for split in 0..=bytes.len() {
            let mut scan = StructureBound::default();
            scan.scan(&bytes[..split]).unwrap();
            scan.scan(&bytes[split..]).unwrap();
            assert!(!scan.quoted);
            assert!(!scan.escaped);
            assert_eq!(scan.depth, 0);
        }
    }
}
