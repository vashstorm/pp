//! Split session native history messages into User / Developer Messages, Assistant Messages, Tool Calls and Tool Output.
//!
//! Classification results are cached by message identity (position + native JSON byte count + content hash): an append
//! only recomputes the new tail, and a wholesale history replacement (compaction, tree navigation, cross-protocol conversion, restore)
//! recomputes from scratch. Large bodies are judged by byte budget first: beyond `MAX_PREVIEW_BYTES` they are no longer cloned,
//! joined, parsed or pretty-printed as a whole; only a byte summary is given.

use super::sections::{
    body_preview, first_line_preview, json_body_preview, oversized_preview, parts_total,
    truncate_preview,
};
use super::{DiagnosticItem, FingerprintWriter, MAX_PREVIEW_BYTES};
use crate::agent::{Agent, Session, tool_images};
use anyhow::Result;
use async_openai::types::{
    chat::{
        ChatCompletionMessageToolCalls, ChatCompletionRequestAssistantMessageContent,
        ChatCompletionRequestAssistantMessageContentPart,
        ChatCompletionRequestDeveloperMessageContent,
        ChatCompletionRequestDeveloperMessageContentPart, ChatCompletionRequestMessage,
        ChatCompletionRequestSystemMessageContent, ChatCompletionRequestToolMessageContent,
        ChatCompletionRequestUserMessageContent, ChatCompletionRequestUserMessageContentPart,
    },
    responses::{
        EasyInputContent, FunctionCallOutput, InputContent, InputItem, InputParam, InputRole, Item,
        MessageItem, OutputMessageContent, ReasoningItemContent, Role, SummaryPart,
    },
};
use pp_core::context::estimate_bytes;
use serde::Serialize;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

#[derive(Clone, Debug, Default)]
pub(super) struct HistoryBucket {
    pub items: Vec<DiagnosticItem>,
    pub tokens: u64,
    pub bytes: u64,
}

impl HistoryBucket {
    fn push(&mut self, item: DiagnosticItem) {
        self.bytes = self.bytes.saturating_add(item.bytes);
        self.tokens = self.tokens.saturating_add(item.tokens);
        self.items.push(item);
    }
}

#[derive(Clone, Debug)]
pub(super) struct HistoryBreakdown {
    pub instruction_messages: HistoryBucket,
    pub instruction_paths: std::collections::BTreeSet<String>,
    pub user_messages: HistoryBucket,
    pub assistant_messages: HistoryBucket,
    pub tool_calls: HistoryBucket,
    pub tool_outputs: HistoryBucket,
}

/// Cheap identity of one message: structured byte counts of the classification-relevant fields + a bounded content hash. The index
/// is implied by position, so "the cached identity is a prefix of the new identity" means only the tail is new.
///
/// The identity covers only the fields classification reads (body, role, refusal, tool calls and results, thinking content);
/// each body hashes only its first and last [`ID_SAMPLE_BYTES`] bytes, so the cost per message is independent of body size;
/// a wholesale history replacement changes the length or the head/tail fragments and thus forces a full recompute, and the only
/// system prompt rewritten in place is covered by the semi-static section fingerprint as well. Rare item variants fall back to streaming serialization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MessageId {
    bytes: u64,
    hash: u64,
}

/// A single message identity hashes only the head and tail fragments (32 bytes each), so the cost is independent of body size.
const ID_SAMPLE_BYTES: usize = 32;

#[derive(Default)]
struct MessageIdBuilder {
    hasher: DefaultHasher,
    bytes: u64,
}

impl MessageIdBuilder {
    fn tag(&mut self, tag: u8) {
        self.hasher.write_u8(tag);
        self.bytes = self.bytes.saturating_add(1);
    }

    fn flag(&mut self, value: bool) {
        self.tag(u8::from(value));
    }

    fn count(&mut self, count: usize) {
        self.hasher.write_usize(count);
        self.bytes = self.bytes.saturating_add(count as u64);
    }

    fn text(&mut self, text: &str) {
        let bytes = text.as_bytes();
        self.hasher.write_usize(bytes.len());
        self.hasher
            .write(&bytes[..ID_SAMPLE_BYTES.min(bytes.len())]);
        self.hasher
            .write(&bytes[bytes.len().saturating_sub(ID_SAMPLE_BYTES)..]);
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
    }

    fn optional_text(&mut self, text: Option<&str>) {
        match text {
            Some(text) => {
                self.flag(true);
                self.text(text);
            }
            None => self.flag(false),
        }
    }

    fn parts(&mut self, parts: &[Cow<'_, str>]) {
        self.count(parts.len());
        for part in parts {
            self.text(part);
        }
    }

    /// Rare items that cannot be described field by field fall back to streaming serialization.
    fn serde(&mut self, value: &impl Serialize) {
        let mut writer = FingerprintWriter::default();
        let _ = serde_json::to_writer(&mut writer, value);
        self.hasher.write_u64(writer.bytes());
        self.bytes = self.bytes.saturating_add(writer.bytes());
        self.hasher.write_u64(writer.finish());
    }

    fn finish(self) -> MessageId {
        MessageId {
            bytes: self.bytes,
            hash: self.hasher.finish(),
        }
    }
}

fn chat_message_id(message: &ChatCompletionRequestMessage) -> MessageId {
    let mut id = MessageIdBuilder::default();
    match message {
        ChatCompletionRequestMessage::System(msg) => {
            id.tag(1);
            id.parts(&chat_system_parts(&msg.content));
        }
        ChatCompletionRequestMessage::User(msg) => {
            id.tag(2);
            id.parts(&chat_user_parts(&msg.content));
        }
        ChatCompletionRequestMessage::Developer(msg) => {
            id.tag(3);
            id.parts(&chat_developer_parts(&msg.content));
        }
        ChatCompletionRequestMessage::Assistant(msg) => {
            id.tag(4);
            id.parts(&chat_assistant_parts(msg.content.as_ref()));
            id.optional_text(msg.refusal.as_deref());
            let calls = msg.tool_calls.as_deref().unwrap_or_default();
            id.count(calls.len());
            for call in calls {
                match call {
                    ChatCompletionMessageToolCalls::Function(call) => {
                        id.tag(1);
                        id.text(&call.id);
                        id.text(&call.function.name);
                        id.text(&call.function.arguments);
                    }
                    ChatCompletionMessageToolCalls::Custom(call) => {
                        id.tag(2);
                        id.text(&call.id);
                        id.text(&call.custom_tool.name);
                        id.text(&call.custom_tool.input);
                    }
                }
            }
        }
        ChatCompletionRequestMessage::Tool(msg) => {
            id.tag(5);
            id.parts(&chat_tool_parts(&msg.content));
            id.text(&msg.tool_call_id);
        }
        ChatCompletionRequestMessage::Function(msg) => {
            id.tag(6);
            id.optional_text(msg.content.as_deref());
            id.text(&msg.name);
        }
    }
    id.finish()
}

fn response_item_id(item: &InputItem) -> MessageId {
    let mut id = MessageIdBuilder::default();
    match item {
        InputItem::EasyMessage(msg) => {
            id.tag(1);
            id.tag(match msg.role {
                Role::User => 1,
                Role::Developer => 2,
                Role::System => 3,
                Role::Assistant => 4,
            });
            id.parts(&easy_content_parts(&msg.content));
        }
        InputItem::Item(Item::Message(MessageItem::Input(msg))) => {
            id.tag(2);
            id.tag(match msg.role {
                InputRole::User => 1,
                InputRole::Developer => 2,
                InputRole::System => 3,
            });
            id.parts(&response_parts(&msg.content));
        }
        InputItem::Item(Item::Message(MessageItem::Output(msg))) => {
            id.tag(3);
            id.count(msg.content.len());
            for part in &msg.content {
                match part {
                    OutputMessageContent::OutputText(text) => id.text(&text.text),
                    OutputMessageContent::Refusal(refusal) => id.text(&refusal.refusal),
                }
            }
        }
        InputItem::Item(Item::Reasoning(item)) => {
            id.tag(4);
            id.count(item.summary.len());
            for part in &item.summary {
                let SummaryPart::SummaryText(text) = part;
                id.text(&text.text);
            }
            let content = item.content.as_deref().unwrap_or_default();
            id.count(content.len());
            for part in content {
                let ReasoningItemContent::ReasoningText(text) = part;
                id.text(&text.text);
            }
            id.optional_text(item.encrypted_content.as_deref());
        }
        InputItem::Item(Item::FunctionCall(call)) => {
            id.tag(5);
            id.text(&call.call_id);
            id.text(&call.name);
            id.text(&call.arguments);
        }
        InputItem::Item(Item::FunctionCallOutput(output)) => {
            id.tag(6);
            id.optional_text(output.call_id.as_deref());
            match &output.output {
                FunctionCallOutput::Text(text) => {
                    id.tag(1);
                    id.text(text);
                }
                FunctionCallOutput::Content(parts) => {
                    id.tag(2);
                    id.parts(&response_parts(parts));
                }
            }
        }
        other => {
            id.tag(9);
            id.serde(other);
        }
    }
    id.finish()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProtocolKind {
    Completion,
    Response,
}

impl ProtocolKind {
    fn of(session: &Session) -> Self {
        match session {
            Session::Completion(_) => Self::Completion,
            Session::Response(_) => Self::Response,
        }
    }
}

/// Identity of each message in the current session, used for cache-prefix decisions of history and thinking classification.
pub(super) fn message_ids(session: &Session) -> Vec<MessageId> {
    match session {
        Session::Completion(request) => request.messages.iter().map(chat_message_id).collect(),
        Session::Response(request) => match &request.input {
            // Text input has no per-item granularity; consistent with the existing history classification, it yields no entries.
            InputParam::Text(_) => Vec::new(),
            InputParam::Items(items) => items.iter().map(response_item_id).collect(),
        },
    }
}

/// Incremental state of the history classification: the already-classified prefix + the call table shared across messages.
#[derive(Default)]
pub(super) struct HistoryState {
    protocol: Option<ProtocolKind>,
    ids: Vec<MessageId>,
    seq: usize,
    call_map: HashMap<String, (String, String)>,
    instruction_messages: HistoryBucket,
    instruction_paths: std::collections::BTreeSet<String>,
    user_messages: HistoryBucket,
    assistant_messages: HistoryBucket,
    tool_calls: HistoryBucket,
    tool_outputs: HistoryBucket,
}

impl HistoryState {
    /// Classify only the new messages corresponding to `ids[self.ids.len()..]`; `ids` must align one-to-one with the current session.
    fn extend(&mut self, session: &Session, ids: &[MessageId]) -> Result<()> {
        match session {
            Session::Completion(request) => {
                for (index, message) in request.messages.iter().enumerate().skip(self.ids.len()) {
                    let Some(id) = ids.get(index) else {
                        break;
                    };
                    self.ids.push(*id);
                    if index == 0 && matches!(message, ChatCompletionRequestMessage::System(_)) {
                        continue;
                    }
                    self.seq += 1;
                    let seq = self.seq;
                    self.classify_chat(seq, message)?;
                }
            }
            Session::Response(request) => {
                let InputParam::Items(items) = &request.input else {
                    return Ok(());
                };
                for (index, item) in items.iter().enumerate().skip(self.ids.len()) {
                    let Some(id) = ids.get(index) else {
                        break;
                    };
                    self.ids.push(*id);
                    self.classify_response(index + 1, item)?;
                }
            }
        }
        Ok(())
    }

    fn breakdown(&self) -> HistoryBreakdown {
        HistoryBreakdown {
            instruction_messages: self.instruction_messages.clone(),
            instruction_paths: self.instruction_paths.clone(),
            user_messages: self.user_messages.clone(),
            assistant_messages: self.assistant_messages.clone(),
            tool_calls: self.tool_calls.clone(),
            tool_outputs: self.tool_outputs.clone(),
        }
    }

    fn push_developer(&mut self, seq: usize, parts: &[Cow<'_, str>], tokens: u64, bytes: u64) {
        let first = parts.first().map(|part| part.as_ref()).unwrap_or("");
        if first.starts_with(crate::agent::PROJECT_INSTRUCTIONS_PREFIX) {
            let preview = parts_preview(parts, false);
            for part in parts {
                let mut pos = 0usize;
                let prefix = "[Project instructions: ";
                while let Some(rel) = part[pos..].find(prefix) {
                    let start = pos + rel + prefix.len();
                    if let Some(end) = part[start..].find(']') {
                        let path = part[start..start + end].trim();
                        if !path.is_empty() {
                            self.instruction_paths.insert(path.to_owned());
                        }
                        pos = start + end + 1;
                    } else {
                        break;
                    }
                }
            }
            self.instruction_messages.push(DiagnosticItem {
                kind_label: "AGENTS.md".into(),
                title: format!("#{seq} Project Instructions (dynamic)"),
                subtitle: first_line_preview(&preview),
                tokens,
                bytes,
                preview,
            });
            return;
        }
        let is_subagent = first.starts_with("[pp-subagent-result:");
        let (kind, title) = if is_subagent {
            ("Subagent", format!("#{seq} Subagent Result"))
        } else {
            ("Developer", format!("#{seq} Developer"))
        };
        let preview = parts_preview(parts, false);
        self.user_messages.push(DiagnosticItem {
            kind_label: kind.into(),
            title,
            subtitle: first_line_preview(&preview),
            tokens,
            bytes,
            preview,
        });
    }

    fn classify_chat(&mut self, seq: usize, message: &ChatCompletionRequestMessage) -> Result<()> {
        let bytes = estimate_bytes(message)?;
        let tokens = bytes.div_ceil(3);
        match message {
            ChatCompletionRequestMessage::System(msg) => {
                let parts = chat_system_parts(&msg.content);
                let preview = parts_preview(&parts, false);
                self.user_messages.push(DiagnosticItem {
                    kind_label: "System".into(),
                    title: format!("#{seq} System"),
                    subtitle: first_line_preview(&preview),
                    tokens,
                    bytes,
                    preview,
                });
            }
            ChatCompletionRequestMessage::User(msg) => {
                if let Some(call_id) = tool_images::chat_id(message) {
                    let (tool_name, args) = self
                        .call_map
                        .get(call_id)
                        .cloned()
                        .unwrap_or_else(|| ("read".into(), call_id.to_owned()));
                    let preview = parts_preview(&chat_user_parts(&msg.content), false);
                    self.tool_outputs.push(DiagnosticItem {
                        kind_label: "Tool Image".into(),
                        title: format!("#{seq} Tool Image · {tool_name}"),
                        subtitle: args,
                        tokens,
                        bytes,
                        preview,
                    });
                    return Ok(());
                }
                let parts = chat_user_parts(&msg.content);
                let is_summary = parts
                    .first()
                    .is_some_and(|part| part.starts_with("[Context summary]\n"));
                let (kind, title) = if is_summary {
                    ("Summary", format!("#{seq} Context Summary"))
                } else {
                    ("User", format!("#{seq} User"))
                };
                let preview = parts_preview(&parts, false);
                self.user_messages.push(DiagnosticItem {
                    kind_label: kind.into(),
                    title,
                    subtitle: first_line_preview(&preview),
                    tokens,
                    bytes,
                    preview,
                });
            }
            ChatCompletionRequestMessage::Developer(msg) => {
                let parts = chat_developer_parts(&msg.content);
                self.push_developer(seq, &parts, tokens, bytes);
            }
            ChatCompletionRequestMessage::Assistant(msg) => {
                let parts = chat_assistant_parts(msg.content.as_ref());
                let refusal = msg.refusal.as_deref().unwrap_or("");
                let preview = assistant_preview(&parts, refusal);
                let has_body =
                    !refusal.is_empty() || parts.iter().any(|part| !part.trim().is_empty());
                let calls = msg.tool_calls.as_deref().unwrap_or_default();
                if calls.is_empty() {
                    self.assistant_messages.push(DiagnosticItem {
                        kind_label: "Assistant".into(),
                        title: format!("#{seq} Assistant"),
                        subtitle: first_line_preview(&preview),
                        tokens,
                        bytes,
                        preview,
                    });
                } else {
                    if has_body {
                        let mut stripped = msg.clone();
                        stripped.tool_calls = None;
                        let msg_bytes = estimate_bytes(&stripped)?;
                        let msg_tokens = msg_bytes.div_ceil(3);
                        self.assistant_messages.push(DiagnosticItem {
                            kind_label: "Assistant".into(),
                            title: format!("#{seq} Assistant"),
                            subtitle: first_line_preview(&preview),
                            tokens: msg_tokens,
                            bytes: msg_bytes,
                            preview: preview.clone(),
                        });
                    }
                    for call in calls {
                        if let ChatCompletionMessageToolCalls::Function(call) = call {
                            let args_summary =
                                summarize_tool_args(&call.function.name, &call.function.arguments);
                            self.call_map.insert(
                                call.id.clone(),
                                (call.function.name.clone(), args_summary.clone()),
                            );
                            let call_bytes = estimate_bytes(call)?;
                            let call_tokens = call_bytes.div_ceil(3);
                            self.tool_calls.push(DiagnosticItem {
                                kind_label: "Tool Call".into(),
                                title: format!("#{seq} {}({})", call.function.name, args_summary),
                                subtitle: call.id.clone(),
                                tokens: call_tokens,
                                bytes: call_bytes,
                                preview: format!(
                                    "Tool: {}\nCall ID: {}\nArguments:\n{}",
                                    call.function.name,
                                    call.id,
                                    json_body_preview(&call.function.arguments)
                                ),
                            });
                        }
                    }
                }
            }
            ChatCompletionRequestMessage::Tool(msg) => {
                let parts = chat_tool_parts(&msg.content);
                let total = parts_total(&parts);
                let joined =
                    (parts.len() > 1 && total <= MAX_PREVIEW_BYTES).then(|| parts.join("\n"));
                let text = joined
                    .as_deref()
                    .or_else(|| parts.first().map(|part| part.as_ref()))
                    .unwrap_or("");
                let (tool_name, args_summary) = self
                    .call_map
                    .get(&msg.tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| ("unknown".into(), msg.tool_call_id.clone()));
                let kind_label = if is_skill_body(&tool_name, &args_summary, text) {
                    "Skill Body"
                } else {
                    "Tool Output"
                };
                let preview = if total > MAX_PREVIEW_BYTES {
                    oversized_preview(total)
                } else {
                    json_body_preview(text)
                };
                self.tool_outputs.push(DiagnosticItem {
                    kind_label: kind_label.into(),
                    title: format!("#{seq} {tool_name}"),
                    subtitle: args_summary,
                    tokens,
                    bytes,
                    preview,
                });
            }
            ChatCompletionRequestMessage::Function(_) => {
                self.tool_outputs.push(DiagnosticItem {
                    kind_label: "Function".into(),
                    title: format!("#{seq} Legacy Function"),
                    subtitle: String::new(),
                    tokens,
                    bytes,
                    preview: String::new(),
                });
            }
        }
        Ok(())
    }

    fn classify_response(&mut self, seq: usize, item: &InputItem) -> Result<()> {
        let bytes = estimate_bytes(item)?;
        let tokens = bytes.div_ceil(3);
        match item {
            InputItem::EasyMessage(msg) => {
                let parts = easy_content_parts(&msg.content);
                match msg.role {
                    Role::User => {
                        let is_summary = parts
                            .first()
                            .is_some_and(|part| part.starts_with("[Context summary]\n"));
                        let (kind, title) = if is_summary {
                            ("Summary", format!("#{seq} Context Summary"))
                        } else {
                            ("User", format!("#{seq} User"))
                        };
                        let preview = parts_preview(&parts, false);
                        self.user_messages.push(DiagnosticItem {
                            kind_label: kind.into(),
                            title,
                            subtitle: first_line_preview(&preview),
                            tokens,
                            bytes,
                            preview,
                        });
                    }
                    Role::Developer | Role::System => {
                        self.push_developer(seq, &parts, tokens, bytes);
                    }
                    Role::Assistant => {
                        let preview = parts_preview(&parts, false);
                        self.assistant_messages.push(DiagnosticItem {
                            kind_label: "Assistant".into(),
                            title: format!("#{seq} Assistant"),
                            subtitle: first_line_preview(&preview),
                            tokens,
                            bytes,
                            preview,
                        });
                    }
                }
            }
            InputItem::Item(Item::Message(MessageItem::Input(msg))) => {
                let parts = response_parts(&msg.content);
                match msg.role {
                    InputRole::User => {
                        let is_summary = parts
                            .first()
                            .is_some_and(|part| part.starts_with("[Context summary]\n"));
                        let (kind, title) = if is_summary {
                            ("Summary", format!("#{seq} Context Summary"))
                        } else {
                            ("User", format!("#{seq} User"))
                        };
                        let preview = parts_preview(&parts, false);
                        self.user_messages.push(DiagnosticItem {
                            kind_label: kind.into(),
                            title,
                            subtitle: first_line_preview(&preview),
                            tokens,
                            bytes,
                            preview,
                        });
                    }
                    InputRole::Developer | InputRole::System => {
                        self.push_developer(seq, &parts, tokens, bytes);
                    }
                }
            }
            InputItem::Item(Item::Message(MessageItem::Output(msg))) => {
                let parts: Vec<Cow<'_, str>> = msg
                    .content
                    .iter()
                    .map(|part| match part {
                        OutputMessageContent::OutputText(text) => Cow::Borrowed(text.text.as_str()),
                        OutputMessageContent::Refusal(refusal) => {
                            Cow::Borrowed(refusal.refusal.as_str())
                        }
                    })
                    .collect();
                let preview = parts_preview(&parts, false);
                self.assistant_messages.push(DiagnosticItem {
                    kind_label: "Assistant".into(),
                    title: format!("#{seq} Assistant"),
                    subtitle: first_line_preview(&preview),
                    tokens,
                    bytes,
                    preview,
                });
            }
            InputItem::Item(Item::Reasoning(_)) => {
                // Thinking items are counted separately by the `thinking` module and belong to Assistant Thinking.
            }
            InputItem::Item(Item::FunctionCall(call)) => {
                let args_summary = summarize_tool_args(&call.name, &call.arguments);
                self.call_map.insert(
                    call.call_id.clone(),
                    (call.name.clone(), args_summary.clone()),
                );
                let preview = format!(
                    "Tool: {}\nCall ID: {}\nArguments:\n{}",
                    call.name,
                    call.call_id,
                    json_body_preview(&call.arguments)
                );
                self.tool_calls.push(DiagnosticItem {
                    kind_label: "Tool Call".into(),
                    title: format!("#{seq} {}({})", call.name, args_summary),
                    subtitle: call.call_id.clone(),
                    tokens,
                    bytes,
                    preview,
                });
            }
            InputItem::Item(Item::FunctionCallOutput(output)) => {
                let call_id = output.call_id.as_deref().unwrap_or("");
                let (tool_name, args_summary) = self
                    .call_map
                    .get(call_id)
                    .cloned()
                    .unwrap_or_else(|| ("unknown".into(), call_id.to_owned()));
                let parts: Vec<Cow<'_, str>> = match &output.output {
                    FunctionCallOutput::Text(text) => vec![Cow::Borrowed(text.as_str())],
                    FunctionCallOutput::Content(parts) => response_parts(parts),
                };
                let total = parts_total(&parts);
                let joined =
                    (parts.len() > 1 && total <= MAX_PREVIEW_BYTES).then(|| parts.join("\n"));
                let text = joined
                    .as_deref()
                    .or_else(|| parts.first().map(|part| part.as_ref()))
                    .unwrap_or("");
                let kind_label = if is_skill_body(&tool_name, &args_summary, text) {
                    "Skill Body"
                } else {
                    "Tool Output"
                };
                let preview = if total > MAX_PREVIEW_BYTES {
                    oversized_preview(total)
                } else {
                    json_body_preview(text)
                };
                self.tool_outputs.push(DiagnosticItem {
                    kind_label: kind_label.into(),
                    title: format!("#{seq} {tool_name}"),
                    subtitle: args_summary,
                    tokens,
                    bytes,
                    preview,
                });
            }
            other => {
                let preview = if bytes > MAX_PREVIEW_BYTES as u64 {
                    oversized_preview(bytes as usize)
                } else {
                    let json = serde_json::to_string_pretty(other)
                        .unwrap_or_else(|_| "(unprintable item)".into());
                    truncate_preview(&json)
                };
                self.user_messages.push(DiagnosticItem {
                    kind_label: "Item".into(),
                    title: format!("#{seq} Native Item"),
                    subtitle: first_line_preview(&preview),
                    tokens,
                    bytes,
                    preview,
                });
            }
        }
        Ok(())
    }
}

impl Agent {
    /// Reuse the history classification when it matches the `ids` prefix and classify only the new tail; recompute fully when the cache is empty or the prefix diverges.
    pub(super) fn inspect_history_cached(
        &self,
        cache: &mut Option<HistoryState>,
        ids: &[MessageId],
    ) -> Result<HistoryBreakdown> {
        let protocol = ProtocolKind::of(&self.session);
        let reusable = cache.as_ref().is_some_and(|state| {
            state.protocol == Some(protocol) && ids.starts_with(state.ids.as_slice())
        });
        if !reusable {
            *cache = None;
        }
        let state = cache.get_or_insert_with(HistoryState::default);
        state.protocol = Some(protocol);
        state.extend(&self.session, ids)?;
        Ok(state.breakdown())
    }
}

/// Preview of one or more body parts; when `json` is true the JSON body is pretty-printed (within the budget).
fn parts_preview(parts: &[Cow<'_, str>], json: bool) -> String {
    let total = parts_total(parts);
    if total > MAX_PREVIEW_BYTES {
        return oversized_preview(total);
    }
    if parts.len() == 1 {
        return if json {
            json_body_preview(&parts[0])
        } else {
            body_preview(&parts[0])
        };
    }
    let joined = parts.join("\n");
    if json {
        json_body_preview(&joined)
    } else {
        body_preview(&joined)
    }
}

/// Preview of assistant body + refusal body; the refusal block keeps the existing `"\n\n[Refusal]\n"` format.
fn assistant_preview(parts: &[Cow<'_, str>], refusal: &str) -> String {
    if refusal.is_empty() {
        return parts_preview(parts, false);
    }
    let body_len = parts_total(parts);
    let total = body_len + usize::from(body_len > 0) * 2 + "[Refusal]\n".len() + refusal.len();
    if total > MAX_PREVIEW_BYTES {
        return oversized_preview(total);
    }
    let mut body = parts.join("\n");
    if !body.is_empty() {
        body.push_str("\n\n");
    }
    body.push_str("[Refusal]\n");
    body.push_str(refusal);
    truncate_preview(&body)
}

fn chat_system_parts<'a>(
    content: &'a ChatCompletionRequestSystemMessageContent,
) -> Vec<Cow<'a, str>> {
    match content {
        ChatCompletionRequestSystemMessageContent::Text(text) => vec![Cow::Borrowed(text.as_str())],
        ChatCompletionRequestSystemMessageContent::Array(parts) => parts
            .iter()
            .map(|part| match part {
                async_openai::types::chat::ChatCompletionRequestSystemMessageContentPart::Text(
                    text,
                ) => Cow::Borrowed(text.text.as_str()),
            })
            .collect(),
    }
}

fn chat_developer_parts<'a>(
    content: &'a ChatCompletionRequestDeveloperMessageContent,
) -> Vec<Cow<'a, str>> {
    match content {
        ChatCompletionRequestDeveloperMessageContent::Text(text) => {
            vec![Cow::Borrowed(text.as_str())]
        }
        ChatCompletionRequestDeveloperMessageContent::Array(parts) => parts
            .iter()
            .map(|part| {
                let ChatCompletionRequestDeveloperMessageContentPart::Text(text) = part;
                Cow::Borrowed(text.text.as_str())
            })
            .collect(),
    }
}

fn chat_assistant_parts(
    content: Option<&ChatCompletionRequestAssistantMessageContent>,
) -> Vec<Cow<'_, str>> {
    match content {
        Some(ChatCompletionRequestAssistantMessageContent::Text(text)) => {
            vec![Cow::Borrowed(text.as_str())]
        }
        Some(ChatCompletionRequestAssistantMessageContent::Array(parts)) => parts
            .iter()
            .map(|part| match part {
                ChatCompletionRequestAssistantMessageContentPart::Text(text) => {
                    Cow::Borrowed(text.text.as_str())
                }
                ChatCompletionRequestAssistantMessageContentPart::Refusal(refusal) => {
                    Cow::Borrowed(refusal.refusal.as_str())
                }
            })
            .collect(),
        None => Vec::new(),
    }
}

fn chat_tool_parts<'a>(content: &'a ChatCompletionRequestToolMessageContent) -> Vec<Cow<'a, str>> {
    match content {
        ChatCompletionRequestToolMessageContent::Text(text) => vec![Cow::Borrowed(text.as_str())],
        ChatCompletionRequestToolMessageContent::Array(parts) => parts
            .iter()
            .map(|part| match part {
                async_openai::types::chat::ChatCompletionRequestToolMessageContentPart::Text(
                    text,
                ) => Cow::Borrowed(text.text.as_str()),
            })
            .collect(),
    }
}

fn chat_user_parts<'a>(content: &'a ChatCompletionRequestUserMessageContent) -> Vec<Cow<'a, str>> {
    match content {
        ChatCompletionRequestUserMessageContent::Text(text) => vec![Cow::Borrowed(text.as_str())],
        ChatCompletionRequestUserMessageContent::Array(parts) => parts
            .iter()
            .map(|part| match part {
                ChatCompletionRequestUserMessageContentPart::Text(text) => {
                    Cow::Borrowed(text.text.as_str())
                }
                ChatCompletionRequestUserMessageContentPart::ImageUrl(image) => {
                    Cow::Owned(summarize_image_url(&image.image_url.url))
                }
                _ => Cow::Borrowed("<non-text content part>"),
            })
            .collect(),
    }
}

fn easy_content_parts(content: &EasyInputContent) -> Vec<Cow<'_, str>> {
    match content {
        EasyInputContent::Text(text) => vec![Cow::Borrowed(text.as_str())],
        EasyInputContent::ContentList(parts) => response_parts(parts),
    }
}

fn response_parts(parts: &[InputContent]) -> Vec<Cow<'_, str>> {
    parts
        .iter()
        .map(|part| match part {
            InputContent::InputText(text) => Cow::Borrowed(text.text.as_str()),
            InputContent::InputImage(image) => Cow::Owned(
                image
                    .image_url
                    .as_deref()
                    .map(summarize_image_url)
                    .unwrap_or_else(|| "<input image>".into()),
            ),
            _ => Cow::Borrowed("<non-text input part>"),
        })
        .collect()
}

fn summarize_image_url(url: &str) -> String {
    if url.starts_with("data:image/") {
        format!("<base64 image data: {} chars, ~4096 tokens>", url.len())
    } else {
        url.to_owned()
    }
}

/// Whether the tool body comes from a `skill://` read (counted under `Skill Body`).
///
/// Bodies beyond the preview budget are not parsed as JSON wholesale, only probed with substring searches, so classification never parses large objects.
fn is_skill_body(tool_name: &str, args_summary: &str, output: &str) -> bool {
    if tool_name != "read" {
        return false;
    }
    if args_summary.starts_with("skill://") {
        return true;
    }
    if output.len() > MAX_PREVIEW_BYTES {
        return output.contains("\"skill://");
    }
    serde_json::from_str::<Value>(output)
        .ok()
        .and_then(|value| {
            value
                .get("uri")
                .and_then(Value::as_str)
                .map(|uri| uri.starts_with("skill://"))
        })
        .unwrap_or(false)
}

/// Tool call argument summary: oversized arguments are not parsed as JSON and fall back to a first-line preview.
fn summarize_tool_args(name: &str, raw: &str) -> String {
    if raw.len() > MAX_PREVIEW_BYTES {
        return first_line_preview(raw);
    }
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return first_line_preview(raw);
    };
    let picked = match name {
        "read" | "write" => value.get("path").and_then(Value::as_str),
        "rg" => value
            .get("pattern")
            .and_then(Value::as_str)
            .or_else(|| value.get("path").and_then(Value::as_str)),
        "glob" => value.get("pattern").and_then(Value::as_str),
        "bash" => value.get("command").and_then(Value::as_str),
        "lsp" => value
            .get("action")
            .and_then(Value::as_str)
            .or_else(|| value.get("file").and_then(Value::as_str)),
        "edit" => pp_tools::edit_path(&value),
        _ => None,
    };
    match picked {
        Some(text) => first_line_preview(text),
        None => first_line_preview(raw),
    }
}
