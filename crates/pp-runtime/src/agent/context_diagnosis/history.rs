//! 会话原生历史消息拆分为 User / Developer Messages、Assistant Messages、Tool Calls 与 Tool Output。

use super::DiagnosticItem;
use super::sections::{first_line_preview, truncate_preview};
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
        MessageItem, OutputMessageContent, Role,
    },
};
use pp_core::context::estimate_bytes;
use serde_json::Value;
use std::collections::HashMap;

pub(super) struct HistoryBucket {
    pub items: Vec<DiagnosticItem>,
    pub tokens: u64,
    pub bytes: u64,
}

impl HistoryBucket {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            tokens: 0,
            bytes: 0,
        }
    }

    fn push(&mut self, item: DiagnosticItem) {
        self.bytes = self.bytes.saturating_add(item.bytes);
        self.tokens = self.tokens.saturating_add(item.tokens);
        self.items.push(item);
    }
}

pub(super) struct HistoryBreakdown {
    pub user_messages: HistoryBucket,
    pub assistant_messages: HistoryBucket,
    pub tool_calls: HistoryBucket,
    pub tool_outputs: HistoryBucket,
}

impl Agent {
    pub(super) fn inspect_history(&self) -> Result<HistoryBreakdown> {
        let mut user_messages = HistoryBucket::new();
        let mut assistant_messages = HistoryBucket::new();
        let mut tool_calls = HistoryBucket::new();
        let mut tool_outputs = HistoryBucket::new();
        let mut call_map: HashMap<String, (String, String)> = HashMap::new();

        match &self.session {
            Session::Completion(request) => {
                let mut seq = 0usize;
                for (index, message) in request.messages.iter().enumerate() {
                    if index == 0 && matches!(message, ChatCompletionRequestMessage::System(_)) {
                        continue;
                    }
                    seq += 1;
                    let bytes = estimate_bytes(message)?;
                    let tokens = bytes.div_ceil(3);
                    match message {
                        ChatCompletionRequestMessage::System(msg) => {
                            let text = match &msg.content {
                                ChatCompletionRequestSystemMessageContent::Text(t) => t.clone(),
                                ChatCompletionRequestSystemMessageContent::Array(parts) => parts
                                    .iter()
                                    .map(|p| match p {
                                        async_openai::types::chat::ChatCompletionRequestSystemMessageContentPart::Text(t) => t.text.as_str(),
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            };
                            user_messages.push(DiagnosticItem {
                                kind_label: "System".into(),
                                title: format!("#{seq} System"),
                                subtitle: first_line_preview(&text),
                                tokens,
                                bytes,
                                preview: truncate_preview(&text),
                            });
                        }
                        ChatCompletionRequestMessage::User(msg) => {
                            if let Some(call_id) = tool_images::chat_id(message) {
                                let (tool_name, args) = call_map
                                    .get(call_id)
                                    .cloned()
                                    .unwrap_or_else(|| ("read".into(), call_id.to_owned()));
                                let preview = format_chat_user_content(&msg.content);
                                tool_outputs.push(DiagnosticItem {
                                    kind_label: "Tool Image".into(),
                                    title: format!("#{seq} Tool Image · {tool_name}"),
                                    subtitle: args,
                                    tokens,
                                    bytes,
                                    preview,
                                });
                                continue;
                            }
                            let preview = format_chat_user_content(&msg.content);
                            let is_summary = preview.starts_with("[Context summary]\n");
                            let (kind, title) = if is_summary {
                                ("Summary", format!("#{seq} Context Summary"))
                            } else {
                                ("User", format!("#{seq} User"))
                            };
                            user_messages.push(DiagnosticItem {
                                kind_label: kind.into(),
                                title,
                                subtitle: first_line_preview(&preview),
                                tokens,
                                bytes,
                                preview,
                            });
                        }
                        ChatCompletionRequestMessage::Developer(msg) => {
                            let text = match &msg.content {
                                ChatCompletionRequestDeveloperMessageContent::Text(t) => t.clone(),
                                ChatCompletionRequestDeveloperMessageContent::Array(parts) => parts
                                    .iter()
                                    .map(|p| {
                                        let ChatCompletionRequestDeveloperMessageContentPart::Text(
                                            t,
                                        ) = p;
                                        t.text.as_str()
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            };
                            let (kind, title) = if text.starts_with("[pp-subagent-result:") {
                                ("Subagent", format!("#{seq} Subagent Result"))
                            } else {
                                ("Developer", format!("#{seq} Developer"))
                            };
                            user_messages.push(DiagnosticItem {
                                kind_label: kind.into(),
                                title,
                                subtitle: first_line_preview(&text),
                                tokens,
                                bytes,
                                preview: truncate_preview(&text),
                            });
                        }
                        ChatCompletionRequestMessage::Assistant(msg) => {
                            let mut body = match &msg.content {
                                Some(ChatCompletionRequestAssistantMessageContent::Text(t)) => {
                                    t.clone()
                                }
                                Some(ChatCompletionRequestAssistantMessageContent::Array(parts)) => {
                                    parts
                                        .iter()
                                        .map(|p| match p {
                                            ChatCompletionRequestAssistantMessageContentPart::Text(t) => t.text.as_str(),
                                            ChatCompletionRequestAssistantMessageContentPart::Refusal(r) => r.refusal.as_str(),
                                        })
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                }
                                None => String::new(),
                            };
                            if let Some(refusal) = &msg.refusal
                                && !refusal.is_empty()
                            {
                                if !body.is_empty() {
                                    body.push_str("\n\n");
                                }
                                body.push_str("[Refusal]\n");
                                body.push_str(refusal);
                            }

                            let calls = msg.tool_calls.as_deref().unwrap_or_default();
                            if calls.is_empty() {
                                assistant_messages.push(DiagnosticItem {
                                    kind_label: "Assistant".into(),
                                    title: format!("#{seq} Assistant"),
                                    subtitle: first_line_preview(&body),
                                    tokens,
                                    bytes,
                                    preview: truncate_preview(&body),
                                });
                            } else {
                                if !body.trim().is_empty() {
                                    let mut stripped = msg.clone();
                                    stripped.tool_calls = None;
                                    let msg_bytes = estimate_bytes(&stripped)?;
                                    let msg_tokens = msg_bytes.div_ceil(3);
                                    assistant_messages.push(DiagnosticItem {
                                        kind_label: "Assistant".into(),
                                        title: format!("#{seq} Assistant"),
                                        subtitle: first_line_preview(&body),
                                        tokens: msg_tokens,
                                        bytes: msg_bytes,
                                        preview: truncate_preview(&body),
                                    });
                                }
                                for call in calls {
                                    if let ChatCompletionMessageToolCalls::Function(call) = call {
                                        let args_summary = summarize_tool_args(
                                            &call.function.name,
                                            &call.function.arguments,
                                        );
                                        call_map.insert(
                                            call.id.clone(),
                                            (call.function.name.clone(), args_summary.clone()),
                                        );
                                        let call_bytes = estimate_bytes(call)?;
                                        let call_tokens = call_bytes.div_ceil(3);
                                        tool_calls.push(DiagnosticItem {
                                            kind_label: "Tool Call".into(),
                                            title: format!(
                                                "#{seq} {}({})",
                                                call.function.name, args_summary
                                            ),
                                            subtitle: call.id.clone(),
                                            tokens: call_tokens,
                                            bytes: call_bytes,
                                            preview: format!(
                                                "Tool: {}\nCall ID: {}\nArguments:\n{}",
                                                call.function.name,
                                                call.id,
                                                truncate_preview(&pretty_json_or_raw(
                                                    &call.function.arguments
                                                ))
                                            ),
                                        });
                                    }
                                }
                            }
                        }
                        ChatCompletionRequestMessage::Tool(msg) => {
                            let text = match &msg.content {
                                ChatCompletionRequestToolMessageContent::Text(t) => t.clone(),
                                ChatCompletionRequestToolMessageContent::Array(parts) => parts
                                    .iter()
                                    .map(|p| match p {
                                        async_openai::types::chat::ChatCompletionRequestToolMessageContentPart::Text(t) => t.text.as_str(),
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            };
                            let (tool_name, args_summary) = call_map
                                .get(&msg.tool_call_id)
                                .cloned()
                                .unwrap_or_else(|| ("unknown".into(), msg.tool_call_id.clone()));
                            let kind_label =
                                if detect_skill_uri(&tool_name, &args_summary, &text).is_some() {
                                    "Skill Body"
                                } else {
                                    "Tool Output"
                                };
                            tool_outputs.push(DiagnosticItem {
                                kind_label: kind_label.into(),
                                title: format!("#{seq} {tool_name}"),
                                subtitle: args_summary,
                                tokens,
                                bytes,
                                preview: truncate_preview(&pretty_json_or_raw(&text)),
                            });
                        }
                        ChatCompletionRequestMessage::Function(_) => {
                            tool_outputs.push(DiagnosticItem {
                                kind_label: "Function".into(),
                                title: format!("#{seq} Legacy Function"),
                                subtitle: String::new(),
                                tokens,
                                bytes,
                                preview: String::new(),
                            });
                        }
                    }
                }
            }
            Session::Response(request) => {
                if let InputParam::Items(items) = &request.input {
                    for (idx, item) in items.iter().enumerate() {
                        let seq = idx + 1;
                        let bytes = estimate_bytes(item)?;
                        let tokens = bytes.div_ceil(3);
                        match item {
                            InputItem::EasyMessage(msg) => {
                                let preview = format_easy_content(&msg.content);
                                match msg.role {
                                    Role::User => {
                                        let is_summary = preview.starts_with("[Context summary]\n");
                                        let (kind, title) = if is_summary {
                                            ("Summary", format!("#{seq} Context Summary"))
                                        } else {
                                            ("User", format!("#{seq} User"))
                                        };
                                        user_messages.push(DiagnosticItem {
                                            kind_label: kind.into(),
                                            title,
                                            subtitle: first_line_preview(&preview),
                                            tokens,
                                            bytes,
                                            preview,
                                        });
                                    }
                                    Role::Developer | Role::System => {
                                        let (kind, title) =
                                            if preview.starts_with("[pp-subagent-result:") {
                                                ("Subagent", format!("#{seq} Subagent Result"))
                                            } else {
                                                ("Developer", format!("#{seq} Developer"))
                                            };
                                        user_messages.push(DiagnosticItem {
                                            kind_label: kind.into(),
                                            title,
                                            subtitle: first_line_preview(&preview),
                                            tokens,
                                            bytes,
                                            preview,
                                        });
                                    }
                                    Role::Assistant => {
                                        assistant_messages.push(DiagnosticItem {
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
                                let preview = format_response_parts(&msg.content);
                                match msg.role {
                                    InputRole::User => {
                                        let is_summary = preview.starts_with("[Context summary]\n");
                                        let (kind, title) = if is_summary {
                                            ("Summary", format!("#{seq} Context Summary"))
                                        } else {
                                            ("User", format!("#{seq} User"))
                                        };
                                        user_messages.push(DiagnosticItem {
                                            kind_label: kind.into(),
                                            title,
                                            subtitle: first_line_preview(&preview),
                                            tokens,
                                            bytes,
                                            preview,
                                        });
                                    }
                                    InputRole::Developer | InputRole::System => {
                                        user_messages.push(DiagnosticItem {
                                            kind_label: "Developer".into(),
                                            title: format!("#{seq} Developer"),
                                            subtitle: first_line_preview(&preview),
                                            tokens,
                                            bytes,
                                            preview,
                                        });
                                    }
                                }
                            }
                            InputItem::Item(Item::Message(MessageItem::Output(msg))) => {
                                let text = msg
                                    .content
                                    .iter()
                                    .map(|p| match p {
                                        OutputMessageContent::OutputText(t) => t.text.as_str(),
                                        OutputMessageContent::Refusal(r) => r.refusal.as_str(),
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                assistant_messages.push(DiagnosticItem {
                                    kind_label: "Assistant".into(),
                                    title: format!("#{seq} Assistant"),
                                    subtitle: first_line_preview(&text),
                                    tokens,
                                    bytes,
                                    preview: truncate_preview(&text),
                                });
                            }
                            InputItem::Item(Item::Reasoning(_)) => {
                                // 思考项由 `thinking` 模块独立统计，归入 Assistant Thinking。
                            }
                            InputItem::Item(Item::FunctionCall(call)) => {
                                let args_summary = summarize_tool_args(&call.name, &call.arguments);
                                call_map.insert(
                                    call.call_id.clone(),
                                    (call.name.clone(), args_summary.clone()),
                                );
                                let preview = format!(
                                    "Tool: {}\nCall ID: {}\nArguments:\n{}",
                                    call.name,
                                    call.call_id,
                                    truncate_preview(&pretty_json_or_raw(&call.arguments))
                                );
                                tool_calls.push(DiagnosticItem {
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
                                let (tool_name, args_summary) = call_map
                                    .get(call_id)
                                    .cloned()
                                    .unwrap_or_else(|| ("unknown".into(), call_id.to_owned()));
                                let (preview, raw_text) = match &output.output {
                                    FunctionCallOutput::Text(t) => {
                                        (truncate_preview(&pretty_json_or_raw(t)), t.clone())
                                    }
                                    FunctionCallOutput::Content(parts) => {
                                        let p = format_response_parts(parts);
                                        (p.clone(), p)
                                    }
                                };
                                let kind_label =
                                    if detect_skill_uri(&tool_name, &args_summary, &raw_text)
                                        .is_some()
                                    {
                                        "Skill Body"
                                    } else {
                                        "Tool Output"
                                    };
                                tool_outputs.push(DiagnosticItem {
                                    kind_label: kind_label.into(),
                                    title: format!("#{seq} {tool_name}"),
                                    subtitle: args_summary,
                                    tokens,
                                    bytes,
                                    preview,
                                });
                            }
                            other => {
                                let json = serde_json::to_string_pretty(other)
                                    .unwrap_or_else(|_| "(unprintable item)".into());
                                user_messages.push(DiagnosticItem {
                                    kind_label: "Item".into(),
                                    title: format!("#{seq} Native Item"),
                                    subtitle: first_line_preview(&json),
                                    tokens,
                                    bytes,
                                    preview: truncate_preview(&json),
                                });
                            }
                        }
                    }
                }
            }
        }

        Ok(HistoryBreakdown {
            user_messages,
            assistant_messages,
            tool_calls,
            tool_outputs,
        })
    }
}

fn detect_skill_uri(tool_name: &str, args_summary: &str, output: &str) -> Option<String> {
    if tool_name != "read" {
        return None;
    }
    if args_summary.starts_with("skill://") {
        return Some(args_summary.to_owned());
    }
    serde_json::from_str::<Value>(output)
        .ok()
        .and_then(|value| value.get("uri").and_then(Value::as_str).map(str::to_owned))
        .filter(|uri| uri.starts_with("skill://"))
}

fn summarize_tool_args(name: &str, raw: &str) -> String {
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
        "edit" => crate::tools::edit_path(&value),
        _ => None,
    };
    match picked {
        Some(text) => first_line_preview(text),
        None => first_line_preview(raw),
    }
}

fn format_chat_user_content(content: &ChatCompletionRequestUserMessageContent) -> String {
    match content {
        ChatCompletionRequestUserMessageContent::Text(text) => truncate_preview(text),
        ChatCompletionRequestUserMessageContent::Array(parts) => {
            let lines: Vec<String> = parts
                .iter()
                .map(|part| match part {
                    ChatCompletionRequestUserMessageContentPart::Text(t) => t.text.clone(),
                    ChatCompletionRequestUserMessageContentPart::ImageUrl(img) => {
                        summarize_image_url(&img.image_url.url)
                    }
                    _ => "<non-text content part>".into(),
                })
                .collect();
            truncate_preview(&lines.join("\n"))
        }
    }
}

fn format_easy_content(content: &EasyInputContent) -> String {
    match content {
        EasyInputContent::Text(text) => truncate_preview(text),
        EasyInputContent::ContentList(parts) => format_response_parts(parts),
    }
}

fn format_response_parts(parts: &[InputContent]) -> String {
    let lines: Vec<String> = parts
        .iter()
        .map(|part| match part {
            InputContent::InputText(t) => t.text.clone(),
            InputContent::InputImage(img) => img
                .image_url
                .as_deref()
                .map(summarize_image_url)
                .unwrap_or_else(|| "<input image>".into()),
            _ => "<non-text input part>".into(),
        })
        .collect();
    truncate_preview(&lines.join("\n"))
}

fn summarize_image_url(url: &str) -> String {
    if url.starts_with("data:image/") {
        format!("<base64 image data: {} chars, ~4096 tokens>", url.len())
    } else {
        url.to_owned()
    }
}

fn pretty_json_or_raw(text: &str) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| text.to_owned()),
        Err(_) => text.to_owned(),
    }
}
