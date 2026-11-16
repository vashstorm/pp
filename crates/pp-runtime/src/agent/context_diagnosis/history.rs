//! 会话原生历史消息、工具调用与工具结果占用诊断（思考项由 `thinking` 模块独立统计）。

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
use std::collections::{BTreeMap, HashMap};

pub(super) struct HistoryBreakdown {
    pub message_items: Vec<DiagnosticItem>,
    pub tool_result_items: Vec<DiagnosticItem>,
    pub loaded_skills: Vec<DiagnosticItem>,
    pub message_tokens: u64,
    pub message_bytes: u64,
    pub message_count: usize,
    pub tool_result_tokens: u64,
    pub tool_result_bytes: u64,
    pub tool_result_count: usize,
    pub has_existing_summary: bool,
}

impl Agent {
    pub(super) fn inspect_history(&self) -> Result<HistoryBreakdown> {
        let mut raw_messages: Vec<DiagnosticItem> = Vec::new();
        let mut raw_tool_results: Vec<DiagnosticItem> = Vec::new();
        let mut loaded_skills: Vec<DiagnosticItem> = Vec::new();
        let mut role_stats: BTreeMap<&'static str, (usize, u64, u64)> = BTreeMap::new();
        let mut tool_stats: BTreeMap<String, (usize, u64, u64)> = BTreeMap::new();
        let mut call_map: HashMap<String, (String, String)> = HashMap::new();
        let mut has_existing_summary = false;

        let mut message_tokens = 0u64;
        let mut message_bytes = 0u64;
        let mut message_count = 0usize;
        let mut tool_result_tokens = 0u64;
        let mut tool_result_bytes = 0u64;
        let mut tool_result_count = 0usize;

        let mut push_item = |role_bucket: &'static str,
                             is_tool_result: bool,
                             tool_key: Option<String>,
                             item: DiagnosticItem| {
            let entry = role_stats.entry(role_bucket).or_insert((0, 0, 0));
            entry.0 += 1;
            entry.1 = entry.1.saturating_add(item.bytes);
            entry.2 = entry.2.saturating_add(item.tokens);
            if let Some(name) = tool_key {
                let tool_entry = tool_stats.entry(name).or_insert((0, 0, 0));
                tool_entry.0 += 1;
                tool_entry.1 = tool_entry.1.saturating_add(item.bytes);
                tool_entry.2 = tool_entry.2.saturating_add(item.tokens);
            }
            if is_tool_result {
                tool_result_count += 1;
                tool_result_bytes = tool_result_bytes.saturating_add(item.bytes);
                tool_result_tokens = tool_result_tokens.saturating_add(item.tokens);
                raw_tool_results.push(item);
            } else {
                message_count += 1;
                message_bytes = message_bytes.saturating_add(item.bytes);
                message_tokens = message_tokens.saturating_add(item.tokens);
                raw_messages.push(item);
            }
        };

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
                            push_item(
                                "Developer",
                                false,
                                None,
                                DiagnosticItem {
                                    kind_label: "System".into(),
                                    title: format!("#{seq} System"),
                                    subtitle: first_line_preview(&text),
                                    tokens,
                                    bytes,
                                    preview: truncate_preview(&text),
                                    is_header: false,
                                    rankable: true,
                                },
                            );
                        }
                        ChatCompletionRequestMessage::User(msg) => {
                            if let Some(call_id) = tool_images::chat_id(message) {
                                let (tool_name, args) = call_map
                                    .get(call_id)
                                    .cloned()
                                    .unwrap_or_else(|| ("read".into(), call_id.to_owned()));
                                let preview = format_chat_user_content(&msg.content);
                                push_item(
                                    "Tool Image",
                                    true,
                                    Some(tool_name.clone()),
                                    DiagnosticItem {
                                        kind_label: "Tool Image".into(),
                                        title: format!("#{seq} Tool Image · {tool_name}"),
                                        subtitle: args,
                                        tokens,
                                        bytes,
                                        preview,
                                        is_header: false,
                                        rankable: true,
                                    },
                                );
                                continue;
                            }
                            let preview = format_chat_user_content(&msg.content);
                            let is_summary = preview.starts_with("[Context summary]\n");
                            if is_summary {
                                has_existing_summary = true;
                            }
                            let (bucket, kind, title) = if is_summary {
                                ("Summary", "Summary", format!("#{seq} Context Summary"))
                            } else {
                                ("User", "User", format!("#{seq} User"))
                            };
                            push_item(
                                bucket,
                                false,
                                None,
                                DiagnosticItem {
                                    kind_label: kind.into(),
                                    title,
                                    subtitle: first_line_preview(&preview),
                                    tokens,
                                    bytes,
                                    preview,
                                    is_header: false,
                                    rankable: true,
                                },
                            );
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
                            let (bucket, kind, title) = if text.starts_with("[pp-subagent-result:")
                            {
                                ("Subagent", "Subagent", format!("#{seq} Subagent Result"))
                            } else {
                                ("Developer", "Developer", format!("#{seq} Developer"))
                            };
                            push_item(
                                bucket,
                                false,
                                None,
                                DiagnosticItem {
                                    kind_label: kind.into(),
                                    title,
                                    subtitle: first_line_preview(&text),
                                    tokens,
                                    bytes,
                                    preview: truncate_preview(&text),
                                    is_header: false,
                                    rankable: true,
                                },
                            );
                        }
                        ChatCompletionRequestMessage::Assistant(msg) => {
                            let mut call_names = Vec::new();
                            let mut call_details = Vec::new();
                            for call in msg.tool_calls.iter().flatten() {
                                if let ChatCompletionMessageToolCalls::Function(call) = call {
                                    let args_summary = summarize_tool_args(
                                        &call.function.name,
                                        &call.function.arguments,
                                    );
                                    call_map.insert(
                                        call.id.clone(),
                                        (call.function.name.clone(), args_summary.clone()),
                                    );
                                    call_names.push(call.function.name.clone());
                                    call_details.push(format!(
                                        "-> {}({}) [{}]\n{}",
                                        call.function.name,
                                        args_summary,
                                        call.id,
                                        pretty_json_or_raw(&call.function.arguments)
                                    ));
                                }
                            }
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
                            if !call_details.is_empty() {
                                if !body.is_empty() {
                                    body.push_str("\n\n");
                                }
                                body.push_str(&call_details.join("\n\n"));
                            }
                            let title = if call_names.is_empty() {
                                format!("#{seq} Assistant")
                            } else {
                                format!(
                                    "#{seq} Assistant ({} call{}: {})",
                                    call_names.len(),
                                    if call_names.len() == 1 { "" } else { "s" },
                                    join_limited(&call_names, 3)
                                )
                            };
                            push_item(
                                "Assistant",
                                false,
                                None,
                                DiagnosticItem {
                                    kind_label: "Assistant".into(),
                                    title,
                                    subtitle: first_line_preview(&body),
                                    tokens,
                                    bytes,
                                    preview: truncate_preview(&body),
                                    is_header: false,
                                    rankable: true,
                                },
                            );
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
                            let skill_uri = detect_skill_uri(&tool_name, &args_summary, &text);
                            let kind_label = if skill_uri.is_some() {
                                "Skill Body"
                            } else {
                                "Tool Result"
                            };
                            let item = DiagnosticItem {
                                kind_label: kind_label.into(),
                                title: format!("#{seq} Tool Result · {tool_name}"),
                                subtitle: args_summary,
                                tokens,
                                bytes,
                                preview: truncate_preview(&pretty_json_or_raw(&text)),
                                is_header: false,
                                rankable: true,
                            };
                            if let Some(uri) = skill_uri {
                                loaded_skills.push(DiagnosticItem {
                                    kind_label: "Skill Body".into(),
                                    title: uri,
                                    subtitle: format!(
                                        "loaded by #{seq} read (counted in Tool Results)"
                                    ),
                                    tokens: item.tokens,
                                    bytes: item.bytes,
                                    preview: item.preview.clone(),
                                    is_header: false,
                                    rankable: false,
                                });
                            }
                            push_item("Tool Result", true, Some(tool_name), item);
                        }
                        ChatCompletionRequestMessage::Function(_) => {
                            push_item(
                                "Tool Result",
                                true,
                                None,
                                DiagnosticItem {
                                    kind_label: "Function".into(),
                                    title: format!("#{seq} Legacy Function"),
                                    subtitle: String::new(),
                                    tokens,
                                    bytes,
                                    preview: String::new(),
                                    is_header: false,
                                    rankable: true,
                                },
                            );
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
                                        if is_summary {
                                            has_existing_summary = true;
                                        }
                                        let (bucket, kind, title) = if is_summary {
                                            (
                                                "Summary",
                                                "Summary",
                                                format!("#{seq} Context Summary"),
                                            )
                                        } else {
                                            ("User", "User", format!("#{seq} User"))
                                        };
                                        push_item(
                                            bucket,
                                            false,
                                            None,
                                            DiagnosticItem {
                                                kind_label: kind.into(),
                                                title,
                                                subtitle: first_line_preview(&preview),
                                                tokens,
                                                bytes,
                                                preview,
                                                is_header: false,
                                                rankable: true,
                                            },
                                        );
                                    }
                                    Role::Developer | Role::System => {
                                        let (bucket, kind, title) = if preview
                                            .starts_with("[pp-subagent-result:")
                                        {
                                            (
                                                "Subagent",
                                                "Subagent",
                                                format!("#{seq} Subagent Result"),
                                            )
                                        } else {
                                            ("Developer", "Developer", format!("#{seq} Developer"))
                                        };
                                        push_item(
                                            bucket,
                                            false,
                                            None,
                                            DiagnosticItem {
                                                kind_label: kind.into(),
                                                title,
                                                subtitle: first_line_preview(&preview),
                                                tokens,
                                                bytes,
                                                preview,
                                                is_header: false,
                                                rankable: true,
                                            },
                                        );
                                    }
                                    Role::Assistant => {
                                        push_item(
                                            "Assistant",
                                            false,
                                            None,
                                            DiagnosticItem {
                                                kind_label: "Assistant".into(),
                                                title: format!("#{seq} Assistant"),
                                                subtitle: first_line_preview(&preview),
                                                tokens,
                                                bytes,
                                                preview,
                                                is_header: false,
                                                rankable: true,
                                            },
                                        );
                                    }
                                }
                            }
                            InputItem::Item(Item::Message(MessageItem::Input(msg))) => {
                                let preview = format_response_parts(&msg.content);
                                match msg.role {
                                    InputRole::User => {
                                        let is_summary = preview.starts_with("[Context summary]\n");
                                        if is_summary {
                                            has_existing_summary = true;
                                        }
                                        let (bucket, kind, title) = if is_summary {
                                            (
                                                "Summary",
                                                "Summary",
                                                format!("#{seq} Context Summary"),
                                            )
                                        } else {
                                            ("User", "User", format!("#{seq} User"))
                                        };
                                        push_item(
                                            bucket,
                                            false,
                                            None,
                                            DiagnosticItem {
                                                kind_label: kind.into(),
                                                title,
                                                subtitle: first_line_preview(&preview),
                                                tokens,
                                                bytes,
                                                preview,
                                                is_header: false,
                                                rankable: true,
                                            },
                                        );
                                    }
                                    InputRole::Developer | InputRole::System => {
                                        push_item(
                                            "Developer",
                                            false,
                                            None,
                                            DiagnosticItem {
                                                kind_label: "Developer".into(),
                                                title: format!("#{seq} Developer"),
                                                subtitle: first_line_preview(&preview),
                                                tokens,
                                                bytes,
                                                preview,
                                                is_header: false,
                                                rankable: true,
                                            },
                                        );
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
                                push_item(
                                    "Assistant",
                                    false,
                                    None,
                                    DiagnosticItem {
                                        kind_label: "Assistant".into(),
                                        title: format!("#{seq} Assistant"),
                                        subtitle: first_line_preview(&text),
                                        tokens,
                                        bytes,
                                        preview: truncate_preview(&text),
                                        is_header: false,
                                        rankable: true,
                                    },
                                );
                            }
                            InputItem::Item(Item::Reasoning(_)) => {
                                // 思考项由 `Thinking` 分类独立统计，不混入普通消息列表。
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
                                push_item(
                                    "Tool Call",
                                    false,
                                    None,
                                    DiagnosticItem {
                                        kind_label: "Tool Call".into(),
                                        title: format!("#{seq} Tool Call · {}", call.name),
                                        subtitle: args_summary,
                                        tokens,
                                        bytes,
                                        preview,
                                        is_header: false,
                                        rankable: true,
                                    },
                                );
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
                                let skill_uri =
                                    detect_skill_uri(&tool_name, &args_summary, &raw_text);
                                let kind_label = if skill_uri.is_some() {
                                    "Skill Body"
                                } else {
                                    "Tool Result"
                                };
                                let diag = DiagnosticItem {
                                    kind_label: kind_label.into(),
                                    title: format!("#{seq} Tool Result · {tool_name}"),
                                    subtitle: args_summary,
                                    tokens,
                                    bytes,
                                    preview,
                                    is_header: false,
                                    rankable: true,
                                };
                                if let Some(uri) = skill_uri {
                                    loaded_skills.push(DiagnosticItem {
                                        kind_label: "Skill Body".into(),
                                        title: uri,
                                        subtitle: format!(
                                            "loaded by #{seq} read (counted in Tool Results)"
                                        ),
                                        tokens: diag.tokens,
                                        bytes: diag.bytes,
                                        preview: diag.preview.clone(),
                                        is_header: false,
                                        rankable: false,
                                    });
                                }
                                push_item("Tool Result", true, Some(tool_name), diag);
                            }
                            other => {
                                let json = serde_json::to_string_pretty(other)
                                    .unwrap_or_else(|_| "(unprintable item)".into());
                                push_item(
                                    "Developer",
                                    false,
                                    None,
                                    DiagnosticItem {
                                        kind_label: "Item".into(),
                                        title: format!("#{seq} Native Item"),
                                        subtitle: first_line_preview(&json),
                                        tokens,
                                        bytes,
                                        preview: truncate_preview(&json),
                                        is_header: false,
                                        rankable: true,
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }

        let mut message_items = Vec::new();
        if message_count > 0 {
            let mut lines = vec![format!(
                "Messages: {} items · ~{} tokens ({} bytes)\n",
                message_count, message_tokens, message_bytes
            )];
            let mut sorted_roles: Vec<_> = role_stats
                .iter()
                .filter(|(role, _)| !matches!(**role, "Tool Result" | "Tool Image"))
                .collect();
            sorted_roles.sort_by_key(|a| std::cmp::Reverse(a.1.2));
            for (role, (count, bytes, tokens)) in sorted_roles {
                let pct = if message_tokens > 0 {
                    *tokens as f64 * 100.0 / message_tokens as f64
                } else {
                    0.0
                };
                lines.push(format!(
                    "- {role:<12} {:>4} items  ~{:>8} tok ({pct:>5.1}%)  {} bytes",
                    count, tokens, bytes
                ));
            }
            message_items.push(DiagnosticItem {
                kind_label: "Summary Row".into(),
                title: "Messages Breakdown by Role".into(),
                subtitle: format!("{message_count} messages & tool calls"),
                tokens: message_tokens,
                bytes: message_bytes,
                preview: lines.join("\n"),
                is_header: true,
                rankable: false,
            });
        }
        message_items.extend(raw_messages);

        let mut tool_result_items = Vec::new();
        if !tool_stats.is_empty() {
            let mut sorted_tools: Vec<_> = tool_stats.into_iter().collect();
            sorted_tools.sort_by(|a, b| b.1.2.cmp(&a.1.2).then_with(|| a.0.cmp(&b.0)));
            let mut lines = vec!["Tool results aggregated by tool name:\n".to_owned()];
            for (name, (count, bytes, tokens)) in &sorted_tools {
                let avg = tokens.checked_div(*count as u64).unwrap_or(0);
                lines.push(format!(
                    "- {name:<24} {:>3}x  ~{:>8} tok  (avg ~{avg} tok)  {bytes} bytes",
                    count, tokens
                ));
            }
            tool_result_items.push(DiagnosticItem {
                kind_label: "Summary Row".into(),
                title: "Tool Result Footprint by Tool".into(),
                subtitle: format!(
                    "{tool_result_count} results across {} tools",
                    sorted_tools.len()
                ),
                tokens: tool_result_tokens,
                bytes: tool_result_bytes,
                preview: lines.join("\n"),
                is_header: true,
                rankable: false,
            });
        }
        tool_result_items.extend(raw_tool_results);

        Ok(HistoryBreakdown {
            message_items,
            tool_result_items,
            loaded_skills,
            message_tokens,
            message_bytes,
            message_count,
            tool_result_tokens,
            tool_result_bytes,
            tool_result_count,
            has_existing_summary,
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

fn join_limited(items: &[String], limit: usize) -> String {
    if items.len() <= limit {
        items.join(", ")
    } else {
        format!("{}, +{}", items[..limit].join(", "), items.len() - limit)
    }
}
