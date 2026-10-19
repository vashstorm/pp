//! The session container is readable by generic viewers; only pp native extensions restore requests.
use super::{Metadata, Session, SessionRef};
use crate::{
    config::{EditMode, ThinkLevel, ToolLimits},
    events::AgentEvent,
};
use async_openai::types::{chat as c, responses as r};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    borrow::Cow,
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
pub(super) struct Header {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: PathBuf,
}

#[derive(Serialize)]
pub(super) struct HeaderRef<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub version: u32,
    pub id: &'a str,
    pub timestamp: &'a str,
    pub cwd: &'a Path,
}

#[derive(Serialize)]
pub(super) struct EntryRef<'a> {
    pub id: &'a str,
    #[serde(rename = "parentId")]
    pub parent_id: Option<&'a str>,
    pub timestamp: &'a str,
    #[serde(flatten)]
    pub payload: PayloadRef<'a>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum PayloadRef<'a> {
    Message {
        message: &'a Message<'a>,
    },
    ModelChange {
        model: &'a str,
        role: &'static str,
    },
    ThinkingLevelChange {
        #[serde(rename = "thinkingLevel")]
        thinking_level: ThinkLevel,
    },
    Custom {
        #[serde(flatten)]
        custom: CustomRef<'a>,
    },
}

#[derive(Serialize)]
#[serde(tag = "customType", content = "data")]
pub(super) enum CustomRef<'a> {
    #[serde(rename = "pp.session_state")]
    State(StateRef<'a>),
    #[serde(rename = "pp.native_append")]
    NativeAppend(NativeAppendRef<'a>),
    #[serde(rename = "pp.instructions")]
    Instructions { system: &'a str },
    #[serde(rename = "pp.event")]
    Event { event: &'a AgentEvent },
    #[serde(rename = "pp.error")]
    Error { message: &'a str },
    #[serde(rename = "pp.cancelled")]
    Cancelled {},
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StateRef<'a> {
    pub version: u32,
    pub model_reference: &'a str,
    pub think_level: ThinkLevel,
    pub edit_mode: EditMode,
    pub tool_limits: ToolLimits,
    pub base_system: &'a str,
    pub session: SessionRef<'a>,
}
impl<'a> StateRef<'a> {
    pub fn new(metadata: &'a Metadata<'a>, session: SessionRef<'a>) -> Self {
        Self {
            version: 1,
            model_reference: metadata.model_reference,
            think_level: metadata.think_level,
            edit_mode: metadata.edit_mode,
            tool_limits: metadata.tool_limits,
            base_system: metadata.base_system,
            session,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "protocol", rename_all = "lowercase")]
pub(super) enum NativeAppendRef<'a> {
    Completion {
        messages: &'a [c::ChatCompletionRequestMessage],
    },
    Response {
        items: &'a [r::InputItem],
    },
}

#[derive(Deserialize)]
pub(super) struct Entry {
    pub id: String,
    // Unlike an ordinary Option field, a missing parentId is an invalid entry.
    #[serde(rename = "parentId", deserialize_with = "required_parent")]
    pub parent_id: Option<String>,
    pub timestamp: String,
    #[serde(flatten)]
    pub payload: Payload,
}
fn required_parent<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Option<String>, D::Error> {
    Option::deserialize(de)
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Payload {
    Message {
        message: SavedMessage,
    },
    ModelChange {
        model: String,
        role: DefaultRole,
    },
    ThinkingLevelChange {
        #[serde(rename = "thinkingLevel")]
        thinking_level: ThinkLevel,
    },
    Custom {
        #[serde(flatten)]
        custom: Custom,
    },
}
#[derive(Deserialize)]
pub(super) enum DefaultRole {
    #[serde(rename = "default")]
    Default,
}
#[derive(Deserialize)]
#[serde(tag = "role")]
pub(super) enum SavedMessage {
    #[serde(rename = "user")]
    User {
        content: String,
        #[serde(rename = "timestamp")]
        _timestamp: u64,
    },
    #[serde(rename = "assistant")]
    Assistant {
        #[serde(rename = "content")]
        _content: serde::de::IgnoredAny,
    },
    #[serde(rename = "toolResult")]
    ToolResult {
        #[serde(rename = "content")]
        _content: serde::de::IgnoredAny,
    },
}
#[derive(Deserialize)]
#[serde(tag = "customType", content = "data")]
pub(super) enum Custom {
    #[serde(rename = "pp.session_state")]
    State(Box<State>),
    #[serde(rename = "pp.native_append")]
    NativeAppend(NativeAppend),
    #[serde(rename = "pp.instructions")]
    Instructions { system: String },
    #[serde(rename = "pp.event")]
    Event { event: AgentEvent },
    #[serde(rename = "pp.error")]
    Error { message: String },
    #[serde(rename = "pp.cancelled")]
    Cancelled {},
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct State {
    pub version: u32,
    pub model_reference: String,
    pub think_level: ThinkLevel,
    pub edit_mode: EditMode,
    pub tool_limits: ToolLimits,
    pub base_system: String,
    pub session: Session,
}
#[derive(Deserialize)]
#[serde(tag = "protocol", rename_all = "lowercase")]
pub(super) enum NativeAppend {
    Completion {
        messages: Vec<c::ChatCompletionRequestMessage>,
    },
    Response {
        items: Vec<r::InputItem>,
    },
}

#[derive(Serialize)]
#[serde(tag = "role")]
pub(super) enum Message<'a> {
    #[serde(rename = "user")]
    User { content: &'a str, timestamp: u64 },
    #[serde(rename = "assistant", rename_all = "camelCase")]
    Assistant {
        api: &'static str,
        provider: &'a str,
        model: &'a str,
        content: Vec<Content<'a>>,
        stop_reason: &'static str,
        timestamp: u64,
    },
    #[serde(rename = "toolResult", rename_all = "camelCase")]
    ToolResult {
        tool_call_id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_name: Option<&'a str>,
        content: Vec<Content<'a>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        is_error: bool,
        timestamp: u64,
    },
}
#[derive(Serialize)]
#[serde(tag = "type")]
pub(super) enum Content<'a> {
    #[serde(rename = "text")]
    Text { text: Cow<'a, str> },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<bool>,
    },
    #[serde(rename = "toolCall")]
    ToolCall {
        id: &'a str,
        name: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<Map<String, Value>>,
    },
}
fn text(text: &str) -> Content<'_> {
    Content::Text {
        text: Cow::Borrowed(text),
    }
}
fn call<'a>(id: &'a str, name: &'a str, arguments: &str) -> Content<'a> {
    Content::ToolCall {
        id,
        name,
        arguments: serde_json::from_str(arguments).ok(),
    }
}
fn assistant<'a>(
    api: &'static str,
    provider: &'a str,
    model: &'a str,
    content: Vec<Content<'a>>,
    timestamp: u64,
) -> Message<'a> {
    let stop_reason = if content
        .iter()
        .any(|part| matches!(part, Content::ToolCall { .. }))
    {
        "toolUse"
    } else {
        "stop"
    };
    Message::Assistant {
        api,
        provider,
        model,
        content,
        stop_reason,
        timestamp,
    }
}
fn result<'a>(
    id: &'a str,
    name: Option<&'a str>,
    output: Cow<'a, str>,
    timestamp: u64,
) -> Message<'a> {
    let details: Option<Value> = serde_json::from_str(&output).ok();
    let is_error = details
        .as_ref()
        .and_then(|value| value.get("ok"))
        .and_then(Value::as_bool)
        == Some(false);
    Message::ToolResult {
        tool_call_id: id,
        tool_name: name,
        content: vec![Content::Text { text: output }],
        details,
        is_error,
        timestamp,
    }
}

pub(super) fn completion<'a>(
    messages: &'a [c::ChatCompletionRequestMessage],
    provider: &'a str,
    model: &'a str,
    thinking: &'a str,
    timestamp: u64,
) -> Vec<Message<'a>> {
    let mut projected = Vec::new();
    let mut names = HashMap::new();
    let mut thinking = Some(thinking).filter(|text| !text.is_empty());
    for message in messages {
        match message {
            c::ChatCompletionRequestMessage::Assistant(message) => {
                let mut content = Vec::new();
                if let Some(thinking) = thinking.take() {
                    content.push(Content::Thinking {
                        thinking,
                        summary: None,
                    });
                }
                match &message.content {
                    Some(c::ChatCompletionRequestAssistantMessageContent::Text(value)) => {
                        content.push(text(value))
                    }
                    Some(c::ChatCompletionRequestAssistantMessageContent::Array(parts)) => {
                        for part in parts {
                            content.push(text(match part {
                                c::ChatCompletionRequestAssistantMessageContentPart::Text(part) => {
                                    &part.text
                                }
                                c::ChatCompletionRequestAssistantMessageContentPart::Refusal(
                                    part,
                                ) => &part.refusal,
                            }));
                        }
                    }
                    None => {}
                }
                if let Some(refusal) = &message.refusal {
                    content.push(text(refusal));
                }
                for tool in message.tool_calls.iter().flatten() {
                    if let c::ChatCompletionMessageToolCalls::Function(tool) = tool {
                        names.insert(tool.id.as_str(), tool.function.name.as_str());
                        content.push(call(
                            &tool.id,
                            &tool.function.name,
                            &tool.function.arguments,
                        ));
                    }
                }
                projected.push(assistant(
                    "openai-completions",
                    provider,
                    model,
                    content,
                    timestamp,
                ));
            }
            c::ChatCompletionRequestMessage::Tool(output) => {
                let value = match &output.content {
                    c::ChatCompletionRequestToolMessageContent::Text(value) => {
                        Cow::Borrowed(value.as_str())
                    }
                    c::ChatCompletionRequestToolMessageContent::Array(parts) => Cow::Owned(
                        parts
                            .iter()
                            .map(|part| {
                                let c::ChatCompletionRequestToolMessageContentPart::Text(part) =
                                    part;
                                part.text.as_str()
                            })
                            .collect(),
                    ),
                };
                projected.push(result(
                    &output.tool_call_id,
                    names.get(output.tool_call_id.as_str()).copied(),
                    value,
                    timestamp,
                ));
            }
            _ => {}
        }
    }
    projected
}

pub(super) fn response<'a>(
    items: &'a [r::InputItem],
    provider: &'a str,
    model: &'a str,
    timestamp: u64,
) -> Vec<Message<'a>> {
    let mut projected = Vec::new();
    let mut content = Vec::new();
    let mut pending = false;
    let mut names = HashMap::new();
    for item in items {
        match item {
            r::InputItem::Item(r::Item::Reasoning(reasoning)) => {
                pending = true;
                for part in &reasoning.summary {
                    let r::SummaryPart::SummaryText(part) = part;
                    if !part.text.is_empty() {
                        content.push(Content::Thinking {
                            thinking: &part.text,
                            summary: Some(true),
                        });
                    }
                }
                for part in reasoning.content.iter().flatten() {
                    let r::ReasoningItemContent::ReasoningText(part) = part;
                    if !part.text.is_empty() {
                        content.push(Content::Thinking {
                            thinking: &part.text,
                            summary: Some(false),
                        });
                    }
                }
            }
            r::InputItem::Item(r::Item::Message(r::MessageItem::Output(message))) => {
                pending = true;
                for part in &message.content {
                    content.push(text(match part {
                        r::OutputMessageContent::OutputText(part) => &part.text,
                        r::OutputMessageContent::Refusal(part) => &part.refusal,
                    }));
                }
            }
            r::InputItem::Item(r::Item::FunctionCall(tool)) => {
                pending = true;
                names.insert(tool.call_id.as_str(), tool.name.as_str());
                content.push(call(&tool.call_id, &tool.name, &tool.arguments));
            }
            r::InputItem::Item(r::Item::FunctionCallOutput(output)) => {
                if pending {
                    projected.push(assistant(
                        "openai-responses",
                        provider,
                        model,
                        std::mem::take(&mut content),
                        timestamp,
                    ));
                    pending = false;
                }
                let value = match &output.output {
                    r::FunctionCallOutput::Text(value) => Cow::Borrowed(value.as_str()),
                    r::FunctionCallOutput::Content(parts) => Cow::Owned(
                        parts
                            .iter()
                            .filter_map(|part| {
                                if let r::InputContent::InputText(part) = part {
                                    Some(part.text.as_str())
                                } else {
                                    None
                                }
                            })
                            .collect(),
                    ),
                };
                if let Some(id) = &output.call_id {
                    projected.push(result(
                        id,
                        names.get(id.as_str()).copied(),
                        value,
                        timestamp,
                    ));
                }
            }
            _ => {}
        }
    }
    if pending {
        projected.push(assistant(
            "openai-responses",
            provider,
            model,
            content,
            timestamp,
        ));
    }
    projected
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_projection_keeps_refusals_and_omits_nonobject_arguments() {
        let messages: Vec<c::ChatCompletionRequestMessage> = serde_json::from_value(json!([
            {"role":"assistant", "content":[{"type":"text","text":"text"},
                {"type":"refusal","refusal":"part refusal"}], "refusal":"refusal",
                "tool_calls":[{"id":"real-call", "type":"function",
                    "function":{"name":"read", "arguments":"[]"}},
                    {"id":"malformed-call", "type":"function", "function":{"name":"write", "arguments":"{"}},
                    {"id":"object-call", "type":"function", "function":{"name":"read", "arguments":"{}"}}]},
            {"role":"tool", "tool_call_id":"real-call", "content":"{\"ok\":false}"},
            {"role":"tool", "tool_call_id":"unmatched", "content":"not json"}
        ])).unwrap();
        let value = serde_json::to_value(completion(
            &messages,
            "provider",
            "actual-model",
            "thought",
            42,
        ))
        .unwrap();
        assert_eq!(value[0]["model"], "actual-model");
        assert_eq!(
            value[0]["content"][0],
            json!({"type":"thinking", "thinking":"thought"})
        );
        assert_eq!(value[0]["content"][2]["text"], "part refusal");
        assert_eq!(value[0]["content"][3]["text"], "refusal");
        assert_eq!(value[0]["content"][4]["id"], "real-call");
        assert!(value[0]["content"][4].get("arguments").is_none());
        assert!(value[0]["content"][5].get("arguments").is_none());
        assert_eq!(value[0]["content"][6]["arguments"], json!({}));
        assert_eq!(value[0]["stopReason"], "toolUse");
        assert_eq!(value[1]["toolName"], "read");
        assert_eq!(value[1]["isError"], true);
        assert_eq!(value[1]["details"], json!({"ok":false}));
        assert!(value[2].get("toolName").is_none());
        assert!(value[2].get("details").is_none());
    }

    #[test]
    fn response_projection_batches_reasoning_calls_and_native_text_outputs() {
        let items: Vec<r::InputItem> = serde_json::from_value(json!([
            {"type":"reasoning", "id":"reasoning-provider-id", "summary":[
                {"type":"summary_text", "text":"summary"}, {"type":"summary_text", "text":""}],
                "content":[{"type":"reasoning_text", "text":"readable"}], "encrypted_content":"opaque-only-native"},
            {"type":"message", "role":"assistant", "id":"provider-message", "status":"completed",
                "content":[{"type":"output_text", "text":"text", "annotations":[]},
                    {"type":"refusal", "refusal":"refusal"}]},
            {"type":"function_call", "id":"provider-item-id", "call_id":"real-call", "name":"read", "arguments":"{\"path\":\"file\"}"},
            {"type":"function_call_output", "call_id":"real-call", "output":[
                {"type":"input_text", "text":"{\"ok\":"}, {"type":"input_text", "text":"true}"}]}
        ])).unwrap();
        let value = serde_json::to_value(response(&items, "provider", "actual-model", 42)).unwrap();
        assert_eq!(value.as_array().unwrap().len(), 2);
        assert_eq!(
            value[0]["content"][0],
            json!({"type":"thinking", "thinking":"summary", "summary":true})
        );
        assert_eq!(
            value[0]["content"][1],
            json!({"type":"thinking", "thinking":"readable", "summary":false})
        );
        assert_eq!(value[0]["content"][2]["text"], "text");
        assert_eq!(value[0]["content"][3]["text"], "refusal");
        assert_eq!(
            value[0]["content"][4],
            json!({"type":"toolCall", "id":"real-call", "name":"read", "arguments":{"path":"file"}})
        );
        assert!(!value.to_string().contains("opaque-only-native"));
        assert_eq!(value[1]["toolCallId"], "real-call");
        assert_eq!(value[1]["toolName"], "read");
        assert_eq!(value[1]["content"][0]["text"], "{\"ok\":true}");
        assert_eq!(value[1]["isError"], false);
    }
}
