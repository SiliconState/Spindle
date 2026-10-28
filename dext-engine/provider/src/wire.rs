//! Pure transcript-to-provider wire shaping shared by every Dext host.

use std::collections::HashSet;

use anyhow::Result;
use dext_transcript::{Block, Message, ToolResultMetadata};
use serde::Serialize;
use serde_json::{Value, json};

pub const MANAGED_IMAGE_PLACEHOLDER: &str =
    "[Managed image retained; current disclosure approval and managed host required]";

/// What a failed tool result with no text is sent to Anthropic as. Its API refuses a
/// `tool_result` that has `is_error` and empty content (HTTP 400), and a command that
/// fails silently (`grep -q` finding nothing, `test -f` on a missing file) records just
/// that; every later request of the thread would be refused.
pub const EMPTY_FAILURE: &str = "(failed with no output)";

/// A user message that represents a fresh prompt (not tool results and not a
/// runtime-injected note). Used as the current-turn boundary when deciding
/// which thinking blocks must still be sent back to the provider.
pub fn is_fresh_user_prompt_message(msg: &Message) -> bool {
    if msg.role != "user"
        || msg
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolResult { .. }))
    {
        return false;
    }
    msg.content.iter().any(|block| match block {
        Block::Text { text } | Block::PartialStream { text } => {
            !text.starts_with("[runtime-note]")
                && !text.starts_with("[queued-update]")
                && !text.starts_with("[queued-user-update]")
                && !text.starts_with("[prior conversation,")
        }
        _ => false,
    })
}

pub fn current_turn_start(messages: &[Message]) -> usize {
    messages
        .iter()
        .rposition(is_fresh_user_prompt_message)
        .unwrap_or(0)
}

/// Request copy of `messages` for Anthropic Messages: thinking is kept only for
/// the current turn (and only with a signature unless allowed), tool-result
/// metadata and Responses reasoning items are dropped, and a failed tool result
/// with no text says so (`EMPTY_FAILURE`).
pub fn sanitize_anthropic_messages(
    messages: &[Message],
    preserve_thinking: bool,
    allow_empty_thinking_signature: bool,
) -> Vec<Message> {
    // Providers only require thinking blocks for the tool loop of the current
    // turn. Older turns' thinking is dead weight in every subsequent request,
    // so it is dropped at serialization time (history keeps the full record).
    // The one-time prefix change this causes at a turn boundary costs a single
    // partial cache re-write; carrying the blocks forever costs more.
    let current_turn_start = current_turn_start(messages);
    messages
        .iter()
        .enumerate()
        .map(|(idx, message)| {
            let preserve_thinking = preserve_thinking && idx >= current_turn_start;
            Message {
                role: message.role.clone(),
                content: message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        Block::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                            ..
                        } => Some(Block::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: match is_error {
                                Some(true) if content.trim().is_empty() => EMPTY_FAILURE.into(),
                                _ => content.clone(),
                            },
                            is_error: *is_error,
                            metadata: ToolResultMetadata::default(),
                        }),
                        Block::Thinking { text, signature } => {
                            let signature = signature.as_ref().filter(|signature| {
                                allow_empty_thinking_signature || !signature.is_empty()
                            })?;
                            preserve_thinking.then_some(Block::Thinking {
                                text: text.clone(),
                                signature: Some(signature.clone()),
                            })
                        }
                        Block::RedactedThinking { data } => (preserve_thinking && !data.is_empty())
                            .then_some(Block::RedactedThinking { data: data.clone() }),
                        Block::ResponsesReasoning { .. } => None,
                        other => Some(other.clone()),
                    })
                    .collect(),
            }
        })
        .collect()
}

pub enum ImagePart {
    Inline { media_type: String, data: String },
    Text(String),
}

#[derive(Serialize)]
pub struct OaiMessage {
    pub role: String,
    pub content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OaiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Serialize)]
pub struct OaiToolCall {
    pub id: String,
    pub r#type: String,
    pub function: OaiFunction,
}

#[derive(Serialize)]
pub struct OaiFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize)]
pub struct OaiTool {
    pub r#type: String,
    pub function: OaiFunctionDef,
}

#[derive(Serialize)]
pub struct OaiFunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Serialize)]
pub struct ProviderNeutralTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

pub fn chat_tools(tools: Vec<ProviderNeutralTool>) -> Vec<OaiTool> {
    tools
        .into_iter()
        .map(|tool| OaiTool {
            r#type: "function".to_string(),
            function: OaiFunctionDef {
                name: tool.name,
                description: tool.description,
                parameters: tool.schema,
            },
        })
        .collect()
}

pub fn responses_tools(tools: Vec<ProviderNeutralTool>) -> Vec<Value> {
    tools
        .into_iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.schema,
                "strict": Value::Null,
            })
        })
        .collect()
}

/// The conversation cache marker covers only eligible blocks before live pixels.
pub fn anthropic_messages(
    messages: &[Message],
    image: &mut dyn FnMut(usize, &Block) -> ImagePart,
) -> Result<Vec<Value>> {
    let mut wire = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        let mut content = Vec::new();
        let mut image_messages = Vec::new();
        for block in &message.content {
            if matches!(block, Block::ImageArtifact { .. }) {
                content.push(json!({"type":"text", "text":MANAGED_IMAGE_PLACEHOLDER}));
                continue;
            }
            if let Block::PartialStream { text } = block {
                content.push(json!({"type": "text", "text": text}));
                continue;
            }
            if matches!(block, Block::ImageReference { .. }) {
                let image_block = match image(message_index, block) {
                    ImagePart::Inline { media_type, data } => json!({
                        "type": "image",
                        "source": {"type": "base64", "media_type": media_type, "data": data}
                    }),
                    ImagePart::Text(text) => json!({
                        "type": "text",
                        "text": text
                    }),
                };
                image_messages.push(json!({"role": "user", "content": [image_block]}));
                continue;
            }
            content.push(
                serde_json::to_value(block)
                    .map_err(|e| anyhow::anyhow!("serialize message block: {e}"))?,
            );
        }
        if !content.is_empty() {
            wire.push(json!({"role": message.role, "content": content}));
        }
        wire.extend(image_messages);
    }
    Ok(wire)
}

fn tool_use_ids_in_messages(messages: &[Message]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for m in messages {
        for b in &m.content {
            if let Block::ToolUse { id, .. } = b {
                ids.insert(id.clone());
            }
        }
    }
    ids
}

pub fn chat_messages(
    messages: &[Message],
    system_text: &str,
    reasoning_replay_start: Option<usize>,
    image: &mut dyn FnMut(usize, &Block) -> ImagePart,
) -> Vec<OaiMessage> {
    let valid_ids = tool_use_ids_in_messages(messages);
    let mut msgs = vec![OaiMessage {
        role: "system".to_string(),
        content: Some(Value::String(system_text.to_string())),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
    }];
    for (message_index, m) in messages.iter().enumerate() {
        match m.role.as_str() {
            "user" => {
                let texts: Vec<&str> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text } | Block::PartialStream { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                let tool_results: Vec<&Block> = m
                    .content
                    .iter()
                    .filter(|b| matches!(b, Block::ToolResult { .. }))
                    .collect();
                if !tool_results.is_empty() {
                    for b in &tool_results {
                        if let Block::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } = b
                        {
                            if !valid_ids.contains(tool_use_id) {
                                continue;
                            }
                            msgs.push(OaiMessage {
                                role: "tool".to_string(),
                                content: Some(Value::String(content.clone())),
                                reasoning_content: None,
                                tool_calls: None,
                                tool_call_id: Some(tool_use_id.clone()),
                            });
                        }
                    }
                } else if !texts.is_empty() {
                    msgs.push(OaiMessage {
                        role: "user".to_string(),
                        content: Some(Value::String(texts.join("\n"))),
                        reasoning_content: None,
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
                for block in &m.content {
                    if matches!(block, Block::ImageArtifact { .. }) {
                        msgs.push(OaiMessage {
                            role: "user".into(),
                            content: Some(Value::String(MANAGED_IMAGE_PLACEHOLDER.into())),
                            reasoning_content: None,
                            tool_calls: None,
                            tool_call_id: None,
                        });
                        continue;
                    }
                    let Block::ImageReference { .. } = block else {
                        continue;
                    };
                    let content = match image(message_index, block) {
                        ImagePart::Inline { media_type, data } => json!([
                            {"type": "text", "text": "Approved workspace image"},
                            {"type": "image_url", "image_url": {"url": format!("data:{media_type};base64,{data}")}}
                        ]),
                        ImagePart::Text(text) => Value::String(text),
                    };
                    msgs.push(OaiMessage {
                        role: "user".to_string(),
                        content: Some(content),
                        reasoning_content: None,
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
            }
            "assistant" => {
                let texts: Vec<&str> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text } | Block::PartialStream { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                let tool_uses: Vec<&Block> = m
                    .content
                    .iter()
                    .filter(|b| matches!(b, Block::ToolUse { .. }))
                    .collect();
                let has_tools = !tool_uses.is_empty();
                let oai_tool_calls: Option<Vec<OaiToolCall>> = if has_tools {
                    Some(
                        tool_uses
                            .iter()
                            .filter_map(|b| {
                                if let Block::ToolUse { id, name, input } = b {
                                    Some(OaiToolCall {
                                        id: id.clone(),
                                        r#type: "function".to_string(),
                                        function: OaiFunction {
                                            name: name.clone(),
                                            arguments: input.to_string(),
                                        },
                                    })
                                } else {
                                    None
                                }
                            })
                            .collect(),
                    )
                } else {
                    None
                };
                let reasoning_content = reasoning_replay_start
                    .filter(|start| message_index >= *start)
                    .map(|_| {
                        m.content
                            .iter()
                            .filter_map(|block| match block {
                                Block::Thinking { text, .. } if !text.is_empty() => {
                                    Some(text.as_str())
                                }
                                _ => None,
                            })
                            .collect::<String>()
                    })
                    .filter(|reasoning| !reasoning.is_empty());
                let content = if texts.is_empty() {
                    reasoning_content
                        .as_ref()
                        .map(|_| Value::String(String::new()))
                } else {
                    Some(Value::String(texts.join("\n")))
                };
                if content.is_none() && oai_tool_calls.is_none() {
                    continue;
                }
                msgs.push(OaiMessage {
                    role: "assistant".to_string(),
                    content,
                    reasoning_content,
                    tool_calls: oai_tool_calls,
                    tool_call_id: None,
                });
            }
            _ => {}
        }
    }
    msgs
}

fn valid_openai_reasoning_item(item: &Value) -> bool {
    let Some(object) = item.as_object() else {
        return false;
    };
    object.get("type").and_then(Value::as_str) == Some("reasoning")
        && object
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.trim().is_empty())
        && object
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_some_and(|content| !content.is_empty())
}

pub fn responses_input(
    messages: &[Message],
    preserve_reasoning_items: bool,
    image: &mut dyn FnMut(usize, &Block) -> ImagePart,
) -> Vec<Value> {
    let current_turn_start = current_turn_start(messages);
    let valid_ids = tool_use_ids_in_messages(messages);
    let mut items = Vec::new();
    let mut msg_counter = 0usize;

    for (message_index, msg) in messages.iter().enumerate() {
        for block in msg
            .content
            .iter()
            .filter(|block| !matches!(block, Block::ImageReference { .. }))
            .chain(
                msg.content
                    .iter()
                    .filter(|block| matches!(block, Block::ImageReference { .. })),
            )
        {
            match block {
                Block::Text { text } if text.trim().is_empty() => continue,
                Block::Text { text } => {
                    let role = if msg.role == "assistant" {
                        "assistant"
                    } else {
                        "user"
                    };
                    items.push(json!({
                        "type": "message",
                        "role": role,
                        "content": [{
                            "type": if role == "assistant" { "output_text" } else { "input_text" },
                            "text": text,
                        }],
                        "id": format!("msg_{msg_counter}"),
                    }));
                    msg_counter += 1;
                }
                Block::ToolUse { id, name, input } => {
                    // Responses API requires item `id` to start with "fc_".
                    // Dext stores the server-provided call_id (starts with "call_") in
                    // Block.id and uses it to pair tool results. `id` is optional —
                    // omit it so the backend auto-assigns a valid item id.
                    items.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": input.to_string(),
                    }));
                }
                Block::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    if !valid_ids.contains(tool_use_id) {
                        continue;
                    }
                    items.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": content,
                    }));
                }
                Block::ImageReference { .. } => {
                    let content = match image(message_index, block) {
                        ImagePart::Inline { media_type, data } => json!([{
                            "type": "input_image",
                            "image_url": format!("data:{media_type};base64,{data}"),
                        }]),
                        ImagePart::Text(text) => json!([{
                            "type": "input_text",
                            "text": text,
                        }]),
                    };
                    items.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": content,
                        "id": format!("msg_{msg_counter}"),
                    }));
                    msg_counter += 1;
                }
                Block::ImageArtifact { .. } => {
                    items.push(json!({
                        "type":"message", "role":"user",
                        "content":[{"type":"input_text","text":MANAGED_IMAGE_PLACEHOLDER}],
                        "id":format!("msg_{msg_counter}"),
                    }));
                    msg_counter += 1;
                }
                Block::ResponsesReasoning { item }
                    if preserve_reasoning_items
                        && message_index >= current_turn_start
                        && valid_openai_reasoning_item(item) =>
                {
                    items.push(item.clone());
                }
                Block::PartialStream { text } => {
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": text,
                        }],
                        "id": format!("msg_{msg_counter}"),
                    }));
                    msg_counter += 1;
                }
                _ => {}
            }
        }
    }

    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use dext_transcript::ToolResultMetadata;

    fn transcript() -> Vec<Message> {
        vec![
            Message {
                role: "user".into(),
                content: vec![Block::Text {
                    text: "hello".into(),
                }],
            },
            Message {
                role: "assistant".into(),
                content: vec![Block::ToolUse {
                    id: "call_1".into(),
                    name: "read".into(),
                    input: json!({"path": "a"}),
                }],
            },
            Message {
                role: "user".into(),
                content: vec![
                    Block::ToolResult {
                        tool_use_id: "call_1".into(),
                        content: "ok".into(),
                        is_error: None,
                        metadata: ToolResultMetadata::default(),
                    },
                    Block::ToolResult {
                        tool_use_id: "call_orphan".into(),
                        content: "orphan".into(),
                        is_error: None,
                        metadata: ToolResultMetadata::default(),
                    },
                    Block::ImageReference {
                        path: "a.png".into(),
                        media_type: "image/png".into(),
                        width: 1,
                        height: 2,
                        source_sha256: "abc".into(),
                        disclosure_target: Some("target".into()),
                    },
                ],
            },
        ]
    }

    fn placeholder_image(index: usize, _block: &Block) -> ImagePart {
        assert_eq!(index, 2);
        ImagePart::Text("[img]".into())
    }

    #[test]
    fn anthropic_messages_shape_transcript_and_images() {
        let wire = anthropic_messages(&transcript(), &mut placeholder_image).expect("wire");
        assert_eq!(
            wire,
            vec![
                json!({"role": "user", "content": [{"type": "text", "text": "hello"}]}),
                json!({"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "read", "input": {"path": "a"}}
                ]}),
                json!({"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "ok"},
                    {"type": "tool_result", "tool_use_id": "call_orphan", "content": "orphan"},
                ]}),
                json!({"role": "user", "content": [{"type": "text", "text": "[img]"}]}),
            ]
        );
    }

    #[test]
    fn anthropic_failed_tool_results_are_never_empty() {
        let result = |content: &str, is_error| Block::ToolResult {
            tool_use_id: "call".into(),
            content: content.into(),
            is_error,
            metadata: ToolResultMetadata::default(),
        };
        let messages = vec![Message {
            role: "user".into(),
            content: vec![
                result("", Some(true)),
                result(" \n", Some(true)),
                result("", Some(false)),
                result("", None),
                result("exit 2", Some(true)),
            ],
        }];
        let wire = anthropic_messages(
            &sanitize_anthropic_messages(&messages, false, false),
            &mut placeholder_image,
        )
        .expect("wire");
        let contents: Vec<&str> = wire[0]["content"]
            .as_array()
            .expect("blocks")
            .iter()
            .map(|block| block["content"].as_str().expect("text"))
            .collect();
        assert_eq!(
            contents,
            [EMPTY_FAILURE, EMPTY_FAILURE, "", "", "exit 2"],
            "only a failed result with no text is filled in"
        );
        // The transcript itself is left as recorded.
        assert!(
            matches!(&messages[0].content[0], Block::ToolResult { content, .. } if content.is_empty())
        );
    }

    #[test]
    fn chat_messages_shape_transcript_and_images() {
        let msgs = chat_messages(&transcript(), "sys", None, &mut placeholder_image);
        assert_eq!(
            serde_json::to_value(&msgs).expect("serialize"),
            json!([
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read", "arguments": "{\"path\":\"a\"}"},
                }]},
                {"role": "tool", "content": "ok", "tool_call_id": "call_1"},
                {"role": "user", "content": "[img]"},
            ])
        );
    }

    #[test]
    fn responses_input_shape_transcript_and_images() {
        let items = responses_input(&transcript(), false, &mut placeholder_image);
        assert_eq!(
            items,
            vec![
                json!({
                    "type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "hello"}],
                    "id": "msg_0",
                }),
                json!({
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "read",
                    "arguments": "{\"path\":\"a\"}",
                }),
                json!({
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "ok",
                }),
                json!({
                    "type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "[img]"}],
                    "id": "msg_1",
                }),
            ]
        );
    }
}
