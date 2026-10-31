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

    /// The length of what `checkpoint` gives (and its refusals), without building it.
    pub fn checkpoint_len(&self) -> Result<usize, Error> {
        self.validate()?;
        encodes(b"DNT1", self)
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

/// Writes nothing: what is written must be the next bytes of `rest`.
struct SameAs<'a> {
    rest: &'a [u8],
}
impl Write for SameAs<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let rest = self
            .rest
            .strip_prefix(bytes)
            .ok_or_else(|| std::io::Error::other("not the checkpoint's encoding"))?;
        self.rest = rest;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Keeps nothing: counts what is written, with `BoundedWriter`'s bounds.
struct BoundedCount {
    length: usize,
    structure: StructureBound,
}
impl Write for BoundedCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_CHECKPOINT_BYTES.saturating_sub(self.length) {
            return Err(std::io::Error::other("checkpoint bound"));
        }
        self.structure
            .scan(bytes)
            .map_err(|_| std::io::Error::other("checkpoint structure bound"))?;
        self.length += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The length of `encode(magic, value)` when it succeeds, without building it.
pub(crate) fn encodes<T: Serialize>(magic: &[u8; 4], value: &T) -> Result<usize, Error> {
    let mut count = BoundedCount {
        length: magic.len(),
        structure: StructureBound::default(),
    };
    serde_json::to_writer(&mut count, value).map_err(|_| Error::Encoding)?;
    Ok(count.length)
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
    // without changing the existing CLI/session serde compatibility contract. The value
    // is encoded against the bytes rather than into a second copy of them: they passed
    // the bounds above, so an encoding equal to them passes them too.
    let mut same = SameAs { rest: &bytes[4..] };
    if serde_json::to_writer(&mut same, &value).is_err() || !same.rest.is_empty() {
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
        let mut at = 0;
        while at < bytes.len() {
            if self.quoted {
                if self.escaped {
                    self.escaped = false;
                    at += 1;
                    continue;
                }
                // Inside a string only a quote or a backslash means anything.
                let Some(found) = bytes[at..].iter().position(|&b| b == b'"' || b == b'\\') else {
                    return Ok(());
                };
                at += found;
                if bytes[at] == b'\\' {
                    self.escaped = true;
                } else {
                    self.quoted = false;
                }
                at += 1;
            } else {
                let byte = bytes[at];
                at += 1;
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

    /// The scan as it was, a byte at a time: the string-skipping one must match it.
    fn scanned_bytewise(bytes: &[u8], state: &mut StructureBound) -> Result<(), Error> {
        for &byte in bytes {
            if state.quoted {
                if state.escaped {
                    state.escaped = false;
                } else if byte == b'\\' {
                    state.escaped = true;
                } else if byte == b'"' {
                    state.quoted = false;
                }
            } else {
                match byte {
                    b'"' => state.quoted = true,
                    b'{' | b'[' => {
                        state.depth += 1;
                        state.count += 1;
                    }
                    b'}' | b']' => {
                        state.depth = state.depth.checked_sub(1).ok_or(Error::Encoding)?;
                    }
                    b',' | b':' => state.count += 1,
                    _ => {}
                }
                if state.depth > MAX_DEPTH || state.count > MAX_STRUCTURE {
                    return Err(Error::Bounds);
                }
            }
        }
        Ok(())
    }

    #[test]
    fn string_skipping_scan_matches_the_bytewise_scan_at_every_split() {
        let mut inputs: Vec<Vec<u8>> = [
            br#"{"a":"x\"y","b":["\\",{"c":"]}{["}],"d":"\\\""}"#.as_slice(),
            br#""unterminated \"#,
            br#"]]"#,
            br#"{"k":"v"}}"#,
            br#""""\"\\""#,
        ]
        .iter()
        .map(|bytes| bytes.to_vec())
        .collect();
        inputs.push("[".repeat(MAX_DEPTH + 1).into_bytes());
        inputs.push(",".repeat(MAX_STRUCTURE + 1).into_bytes());
        // Pseudo-random mixes of the bytes that matter and ones that do not.
        let mut x = 7u32;
        for _ in 0..200 {
            let bytes = (0..64)
                .map(|_| {
                    x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    b"\"\\{}[],:ab"[(x >> 16) as usize % 10]
                })
                .collect();
            inputs.push(bytes);
        }
        for bytes in &inputs {
            // The long inputs are checked whole; the short ones at every split too.
            let splits = if bytes.len() > 256 {
                vec![0]
            } else {
                (0..=bytes.len()).collect()
            };
            for split in splits {
                let (mut fast, mut slow) = (StructureBound::default(), StructureBound::default());
                let fast_result = fast
                    .scan(&bytes[..split])
                    .and_then(|()| fast.scan(&bytes[split..]));
                let slow_result = scanned_bytewise(&bytes[..split], &mut slow)
                    .and_then(|()| scanned_bytewise(&bytes[split..], &mut slow));
                assert_eq!(
                    fast_result,
                    slow_result,
                    "{}",
                    String::from_utf8_lossy(bytes)
                );
                if fast_result.is_ok() {
                    assert_eq!(
                        (fast.quoted, fast.escaped, fast.depth, fast.count),
                        (slow.quoted, slow.escaped, slow.depth, slow.count)
                    );
                }
            }
        }
    }

    #[test]
    fn counting_and_comparing_writers_agree_with_encoding() {
        let value = serde_json::json!({"text": "quote\" and \\", "list": [1, 2, {"k": null}]});
        let bytes = encode(b"TEST", &value).unwrap();
        assert_eq!(encodes(b"TEST", &value).unwrap(), bytes.len());
        assert!(decode::<serde_json::Value>(b"TEST", &bytes).unwrap() == value);
        // Trailing whitespace parses, but is not the value's encoding.
        let mut spaced = bytes.clone();
        spaced.push(b' ');
        assert!(decode::<serde_json::Value>(b"TEST", &spaced).is_err());
        // Nor is one with its keys out of order (Value sorts them), or a shorter one.
        let mut reordered = b"TEST".to_vec();
        reordered.extend_from_slice(br#"{"text":"quote\" and \\","list":[1,2,{"k":null}]}"#);
        assert!(serde_json::from_slice::<serde_json::Value>(&reordered[4..]).unwrap() == value);
        assert!(decode::<serde_json::Value>(b"TEST", &reordered).is_err());
        assert!(decode::<serde_json::Value>(b"TEST", &bytes[..bytes.len() - 1]).is_err());
        let deep =
            (0..=MAX_DEPTH).fold(serde_json::json!(0), |inner, _| serde_json::json!([inner]));
        assert!(encode(b"TEST", &deep).is_err());
        assert!(encodes(b"TEST", &deep).is_err());
    }
}
