//! 缓存友好延迟批量裁剪层：在达到上下文水位阈值或遭遇 `ContextOverflow` 时，
//! 一次性清理早期冗长工具输出、历史图片载荷与已结束轮次的 Responses 推理项，
//! 平时保持历史严格 append-only 以保护 Prompt Cache。

use super::{
    Agent, Session,
    context::{completion_boundaries, response_boundaries},
    persistence::Metadata,
    tool_images,
    turn::response_input,
};
use anyhow::Result;
use async_openai::types::{chat as c, responses as r};
use serde_json::{Value, json};
use std::collections::HashMap;

/// 常规水位触发时保留的最近完整 exchange 数。
const PRUNE_KEEP_EXCHANGES: usize = 6;
/// 超窗应急裁剪时保留的最近完整 exchange 数。
const AGGRESSIVE_KEEP_EXCHANGES: usize = 2;
/// 历史图片保留的最近完整 exchange 数（早于此的旧图替换为占位符）。
const IMAGE_KEEP_EXCHANGES: usize = 2;
/// 仅裁剪超过该字节数的旧工具输出，小输出直接保留以免得不偿失。
const MIN_PRUNE_OUTPUT_BYTES: usize = 2048;
/// 幂等标记：已裁剪桩不再重复处理。
const PRUNED_OUTPUT_MARKER: &str = "[Pruned tool output:";
/// 旧图片移除后的文本占位提示。
const IMAGE_OMITTED_PLACEHOLDER: &str = "[Image omitted; re-read if needed]";

struct CallHint {
    tool: String,
    target: Option<String>,
}

fn extract_target_hint(arguments: &str) -> Option<String> {
    let val: Value = serde_json::from_str(arguments).ok()?;
    for key in ["path", "command", "pattern", "query", "url", "action"] {
        if let Some(s) = val.get(key).and_then(Value::as_str)
            && !s.trim().is_empty()
        {
            let trimmed = s.trim();
            let mut end = trimmed.len().min(80);
            while end > 0 && !trimmed.is_char_boundary(end) {
                end -= 1;
            }
            return Some(trimmed[..end].to_owned());
        }
    }
    None
}

fn boundary_cutoff(boundaries: &[usize], keep_exchanges: usize) -> usize {
    if boundaries.len() > keep_exchanges {
        boundaries[boundaries.len() - keep_exchanges - 1]
    } else {
        0
    }
}

/// 将超大旧工具输出压缩为轻量桩，保留 `ok`、工具名、路径/命令与 `artifact://` 引用。
fn prune_tool_output_text(raw: &str, hint: Option<&CallHint>) -> Option<String> {
    if raw.len() < MIN_PRUNE_OUTPUT_BYTES || raw.contains(PRUNED_OUTPUT_MARKER) {
        return None;
    }
    let parsed = serde_json::from_str::<Value>(raw).ok();
    let artifact_uri = parsed
        .as_ref()
        .and_then(|v| v.get("artifact"))
        .and_then(|a| a.get("uri"))
        .and_then(Value::as_str);
    let source = parsed
        .as_ref()
        .and_then(|v| v.get("source"))
        .and_then(Value::as_str);
    let mut note = format!("{PRUNED_OUTPUT_MARKER} {} bytes", raw.len());
    if let Some(hint) = hint {
        note.push_str("; tool=");
        note.push_str(&hint.tool);
        if let Some(target) = &hint.target {
            note.push_str(" (");
            note.push_str(target);
            note.push(')');
        }
    }
    if let Some(src) = source {
        note.push_str("; source=");
        note.push_str(src);
    }
    if let Some(uri) = artifact_uri {
        note.push_str("; full output saved at ");
        note.push_str(uri);
    }
    note.push_str(". Re-read or re-run if needed.]");

    let replacement = if let Some(val) = parsed.as_ref() {
        let ok = val.get("ok").and_then(Value::as_bool).unwrap_or(true);
        let mut obj = serde_json::Map::new();
        obj.insert("ok".into(), Value::Bool(ok));
        obj.insert("pruned".into(), Value::Bool(true));
        obj.insert("original_bytes".into(), json!(raw.len()));
        if let Some(kind) = val.get("kind").and_then(Value::as_str) {
            obj.insert("kind".into(), Value::String(kind.to_owned()));
        }
        if let Some(src) = source {
            obj.insert("source".into(), Value::String(src.to_owned()));
        }
        if let Some(uri) = artifact_uri {
            obj.insert("artifact".into(), json!({"uri": uri}));
        }
        obj.insert("content".into(), Value::String(note));
        Value::Object(obj).to_string()
    } else {
        note
    };
    (replacement.len() < raw.len()).then_some(replacement)
}

fn strip_response_image_parts(parts: &mut [r::InputContent]) -> bool {
    let mut changed = false;
    for part in parts.iter_mut() {
        if matches!(part, r::InputContent::InputImage(_)) {
            *part = r::InputContent::InputText(r::InputTextContent {
                text: IMAGE_OMITTED_PLACEHOLDER.into(),
                prompt_cache_breakpoint: None,
            });
            changed = true;
        }
    }
    changed
}

fn omit_function_output_images(parts: &[r::InputContent]) -> Option<String> {
    if !parts
        .iter()
        .any(|p| matches!(p, r::InputContent::InputImage(_)))
    {
        return None;
    }
    let mut text = String::new();
    for part in parts {
        if let r::InputContent::InputText(t) = part {
            text.push_str(&t.text);
        }
    }
    if let Ok(mut val) = serde_json::from_str::<Value>(&text)
        && let Some(obj) = val.as_object_mut()
    {
        let summary = obj
            .get("content")
            .and_then(Value::as_str)
            .and_then(|s| s.lines().next())
            .unwrap_or("Image");
        obj.insert(
            "content".into(),
            Value::String(format!("{summary}\n{IMAGE_OMITTED_PLACEHOLDER}")),
        );
        obj.insert("image_omitted".into(), Value::Bool(true));
        return Some(val.to_string());
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(IMAGE_OMITTED_PLACEHOLDER);
    Some(text)
}

fn prune_completion(
    request: &mut c::CreateChatCompletionRequest,
    aggressive: bool,
) -> Result<bool> {
    let boundaries = completion_boundaries(&request.messages)?;
    let keep = if aggressive {
        AGGRESSIVE_KEEP_EXCHANGES
    } else {
        PRUNE_KEEP_EXCHANGES
    };
    let tool_cutoff = boundary_cutoff(&boundaries, keep);
    let image_cutoff = boundary_cutoff(&boundaries, IMAGE_KEEP_EXCHANGES);
    if tool_cutoff == 0 && image_cutoff == 0 {
        return Ok(false);
    }
    let mut call_hints: HashMap<String, CallHint> = HashMap::new();
    for message in &request.messages[..tool_cutoff] {
        if let c::ChatCompletionRequestMessage::Assistant(assistant) = message
            && let Some(calls) = &assistant.tool_calls
        {
            for call in calls {
                if let c::ChatCompletionMessageToolCalls::Function(func) = call {
                    call_hints.insert(
                        func.id.clone(),
                        CallHint {
                            tool: func.function.name.clone(),
                            target: extract_target_hint(&func.function.arguments),
                        },
                    );
                }
            }
        }
    }
    let mut changed = false;
    let mut remove_indices = Vec::new();
    for (index, message) in request.messages.iter_mut().enumerate() {
        if index == 0 {
            continue;
        }
        if index < image_cutoff {
            if tool_images::chat_id(message).is_some() {
                remove_indices.push(index);
                changed = true;
                continue;
            }
            if let c::ChatCompletionRequestMessage::User(user) = message
                && let c::ChatCompletionRequestUserMessageContent::Array(parts) = &mut user.content
            {
                for part in parts.iter_mut() {
                    if matches!(
                        part,
                        c::ChatCompletionRequestUserMessageContentPart::ImageUrl(_)
                    ) {
                        *part = c::ChatCompletionRequestUserMessageContentPart::Text(
                            c::ChatCompletionRequestMessageContentPartText {
                                text: IMAGE_OMITTED_PLACEHOLDER.into(),
                                prompt_cache_breakpoint: None,
                            },
                        );
                        changed = true;
                    }
                }
            }
        }
        if index < tool_cutoff
            && let c::ChatCompletionRequestMessage::Tool(tool_msg) = message
            && let c::ChatCompletionRequestToolMessageContent::Text(text) = &mut tool_msg.content
            && let Some(pruned) =
                prune_tool_output_text(text, call_hints.get(&tool_msg.tool_call_id))
        {
            *text = pruned;
            changed = true;
        }
    }
    if !remove_indices.is_empty() {
        for index in remove_indices.into_iter().rev() {
            request.messages.remove(index);
        }
    }
    Ok(changed)
}

fn prune_response(request: &mut r::CreateResponse, aggressive: bool) -> Result<bool> {
    let items = response_input(request);
    let boundaries = response_boundaries(items)?;
    let keep = if aggressive {
        AGGRESSIVE_KEEP_EXCHANGES
    } else {
        PRUNE_KEEP_EXCHANGES
    };
    let tool_cutoff = boundary_cutoff(&boundaries, keep);
    let image_cutoff = boundary_cutoff(&boundaries, IMAGE_KEEP_EXCHANGES);
    if tool_cutoff == 0 && image_cutoff == 0 {
        return Ok(false);
    }
    let mut call_hints: HashMap<String, CallHint> = HashMap::new();
    for item in &items[..tool_cutoff] {
        if let r::InputItem::Item(r::Item::FunctionCall(call)) = item {
            call_hints.insert(
                call.call_id.clone(),
                CallHint {
                    tool: call.name.clone(),
                    target: extract_target_hint(&call.arguments),
                },
            );
        }
    }
    let mut changed = false;
    let mut remove_indices = Vec::new();
    for (index, item) in items.iter_mut().enumerate() {
        if index < tool_cutoff && matches!(item, r::InputItem::Item(r::Item::Reasoning(_))) {
            remove_indices.push(index);
            changed = true;
            continue;
        }
        if index < image_cutoff {
            match item {
                r::InputItem::EasyMessage(msg) => {
                    if let r::EasyInputContent::ContentList(parts) = &mut msg.content
                        && strip_response_image_parts(parts)
                    {
                        changed = true;
                    }
                }
                r::InputItem::Item(r::Item::Message(r::MessageItem::Input(msg))) => {
                    if strip_response_image_parts(&mut msg.content) {
                        changed = true;
                    }
                }
                r::InputItem::Item(r::Item::FunctionCallOutput(output)) => {
                    if let r::FunctionCallOutput::Content(parts) = &output.output
                        && let Some(replacement) = omit_function_output_images(parts)
                    {
                        output.output = r::FunctionCallOutput::Text(replacement);
                        changed = true;
                    }
                }
                _ => {}
            }
        }
        if index < tool_cutoff
            && let r::InputItem::Item(r::Item::FunctionCallOutput(output)) = item
            && let r::FunctionCallOutput::Text(text) = &mut output.output
        {
            let hint = output.call_id.as_deref().and_then(|id| call_hints.get(id));
            if let Some(pruned) = prune_tool_output_text(text, hint) {
                *text = pruned;
                changed = true;
            }
        }
    }
    if !remove_indices.is_empty() {
        for index in remove_indices.into_iter().rev() {
            items.remove(index);
        }
    }
    Ok(changed)
}

impl Agent {
    /// 批量轻量裁剪历史上下文：不调用模型、事务性提交到 journal，失败完整回滚。
    /// 返回 `Ok(Some((before_tokens, after_tokens)))` 表示成功释放了上下文空间。
    pub(in crate::agent) fn prune_history(
        &mut self,
        aggressive: bool,
    ) -> Result<Option<(u64, u64)>> {
        let before_tokens = self.context_usage()?.estimated_tokens;
        let backup = self.session.clone();
        let changed = match &mut self.session {
            Session::Completion(request) => prune_completion(request, aggressive),
            Session::Response(request) => prune_response(request, aggressive),
        };
        let changed = match changed {
            Ok(changed) => changed,
            Err(_) => {
                // 不支持的畸形历史保持原样，交由既有压缩/错误路径处理。
                self.session = backup;
                return Ok(None);
            }
        };
        if !changed {
            self.session = backup;
            return Ok(None);
        }
        let after_tokens = match self.context_usage() {
            Ok(usage) => usage.estimated_tokens,
            Err(error) => {
                self.session = backup;
                return Err(error);
            }
        };
        if after_tokens >= before_tokens {
            self.session = backup;
            return Ok(None);
        }
        let metadata = Metadata {
            role_route: self.role_route.as_ref(),
            cwd: self.tools.cwd(),
            model_reference: &self.current_model.reference,
            think_level: self.think_level,
            edit_mode: self.tools.edit_mode(),
            tool_limits: self.tools.limits,
            base_system: &self.base_system,
            prompt_source: self.prompt_source.as_ref(),
        };
        if let Err(error) =
            self.journal
                .replace_history(&metadata, (&self.session).into(), &self.workflow)
        {
            self.session = backup;
            return Err(error);
        }
        self.last_request = None;
        self.last_request_raw_tokens = None;
        self.tools.invalidate_read_dedup();
        let retained_turns = match &self.session {
            Session::Completion(request) => request
                .messages
                .iter()
                .filter(|m| matches!(m, c::ChatCompletionRequestMessage::Assistant(_)))
                .count(),
            Session::Response(request) => match &request.input {
                r::InputParam::Items(items) => items
                    .iter()
                    .filter(|i| matches!(i, r::InputItem::Item(r::Item::Reasoning(_))))
                    .count(),
                _ => 0,
            },
        };
        super::context_diagnosis::trim_for_retained_turns(
            &mut self.thinking_history,
            retained_turns,
        );
        tracing::info!(
            event = "context_pruned",
            aggressive,
            before_tokens,
            after_tokens
        );
        Ok(Some((before_tokens, after_tokens)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::history;
    use pp_config::config::EditMode;
    use pp_config::models::Protocol;
    use pp_tools::read::ToolImage;

    fn make_response_tool_exchange(
        request: &mut r::CreateResponse,
        idx: usize,
        output_text: String,
        with_reasoning: bool,
        with_image: bool,
    ) {
        let items = response_input(request);
        items.push(r::InputItem::EasyMessage(r::EasyInputMessage {
            role: r::Role::User,
            content: format!("turn {idx}").into(),
            ..Default::default()
        }));
        if with_reasoning {
            items.push(r::InputItem::Item(r::Item::Reasoning(r::ReasoningItem {
                id: Some(format!("rs_{idx}")),
                summary: vec![r::SummaryPart::SummaryText(r::SummaryTextContent {
                    text: "thought".into(),
                })],
                content: None,
                encrypted_content: Some("E".repeat(4096)),
                status: None,
            })));
        }
        let call_id = format!("call_{idx}");
        items.push(r::InputItem::Item(r::Item::FunctionCall(
            r::FunctionToolCall {
                call_id: call_id.clone(),
                name: "read".into(),
                arguments: json!({"path": format!("src/file_{idx}.rs")}).to_string(),
                id: None,
                status: None,
                namespace: None,
                caller: None,
                r#async: None,
            },
        )));
        let images = if with_image {
            vec![ToolImage {
                data_url: format!("data:image/png;base64,{}", "A".repeat(4096)),
            }]
        } else {
            Vec::new()
        };
        items.push(r::InputItem::Item(r::Item::FunctionCallOutput(
            r::FunctionCallOutputItemParam {
                call_id: Some(call_id),
                output: tool_images::response_output(output_text, images),
                id: None,
                status: None,
                name: None,
                namespace: None,
                caller: None,
            },
        )));
    }

    #[test]
    fn prune_response_trims_old_outputs_reasoning_and_images_while_keeping_recent_suffix() {
        let Session::Response(mut req) = Session::new(
            "fixture/model".into(),
            Protocol::Response,
            "system".into(),
            EditMode::Replace,
        ) else {
            unreachable!();
        };
        let big = json!({
            "ok": true,
            "source": "src/big.rs",
            "artifact": {"uri": "artifact://7"},
            "content": "z".repeat(4096)
        })
        .to_string();
        let img_json = json!({
            "ok": true,
            "kind": "image",
            "content": "Image: shot.png\nDimensions: 800 x 600"
        })
        .to_string();
        for idx in 0..8 {
            let body = if idx == 3 || idx == 7 {
                img_json.clone()
            } else {
                big.clone()
            };
            make_response_tool_exchange(&mut req, idx, body, true, idx == 3 || idx == 7);
        }
        assert!(prune_response(&mut req, false).unwrap());

        let items = response_input(&mut req);
        // 裁剪后历史边界与跨协议转换仍然完全合法。
        assert!(response_boundaries(items).is_ok());
        assert!(
            history::convert(
                &Session::Response(req.clone()),
                "fixture/model".into(),
                Protocol::Completion,
                EditMode::Replace,
            )
            .is_ok()
        );

        let items = response_input(&mut req);
        // 最早 2 个 exchange 的 reasoning 已被清理，最近 6 个保留。
        let reasoning_ids: Vec<&str> = items
            .iter()
            .filter_map(|item| match item {
                r::InputItem::Item(r::Item::Reasoning(rs)) => rs.id.as_deref(),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasoning_ids,
            vec!["rs_2", "rs_3", "rs_4", "rs_5", "rs_6", "rs_7"]
        );

        // call_3 位于 image_cutoff 之前、tool_cutoff 之后：旧图替换为文本占位，文字保留。
        let call_3_out = items
            .iter()
            .find_map(|item| match item {
                r::InputItem::Item(r::Item::FunctionCallOutput(o))
                    if o.call_id.as_deref() == Some("call_3") =>
                {
                    Some(&o.output)
                }
                _ => None,
            })
            .unwrap();
        let r::FunctionCallOutput::Text(call_3_text) = call_3_out else {
            panic!("expected old image output collapsed to text");
        };
        assert!(
            call_3_text.contains(IMAGE_OMITTED_PLACEHOLDER),
            "{call_3_text}"
        );

        let call_1_out = items
            .iter()
            .find_map(|item| match item {
                r::InputItem::Item(r::Item::FunctionCallOutput(o))
                    if o.call_id.as_deref() == Some("call_1") =>
                {
                    Some(&o.output)
                }
                _ => None,
            })
            .unwrap();
        let r::FunctionCallOutput::Text(call_1_text) = call_1_out else {
            panic!("expected text output");
        };
        assert!(call_1_text.contains(PRUNED_OUTPUT_MARKER), "{call_1_text}");
        assert!(call_1_text.contains("artifact://7"), "{call_1_text}");

        let call_7_out = items
            .iter()
            .find_map(|item| match item {
                r::InputItem::Item(r::Item::FunctionCallOutput(o))
                    if o.call_id.as_deref() == Some("call_7") =>
                {
                    Some(&o.output)
                }
                _ => None,
            })
            .unwrap();
        assert!(matches!(call_7_out, r::FunctionCallOutput::Content(_)));

        // 幂等：立刻再跑一次常规裁剪不再改动历史。
        assert!(!prune_response(&mut req, false).unwrap());
    }

    #[test]
    fn prune_completion_evicts_old_tool_outputs_and_image_attachments_cleanly() {
        let Session::Completion(mut req) = Session::new(
            "fixture/model".into(),
            Protocol::Completion,
            "system".into(),
            EditMode::Replace,
        ) else {
            unreachable!();
        };
        let big = json!({
            "ok": true,
            "source": "src/demo.rs",
            "content": "y".repeat(4096)
        })
        .to_string();
        for idx in 0..8 {
            req.messages.push(
                serde_json::from_value(json!({"role": "user", "content": format!("u{idx}")}))
                    .unwrap(),
            );
            let call_id = format!("c_{idx}");
            req.messages.push(
                serde_json::from_value(json!({
                    "role": "assistant",
                    "tool_calls": [{
                        "id": call_id,
                        "type": "function",
                        "function": {"name": "read", "arguments": "{\"path\":\"img.png\"}"}
                    }]
                }))
                .unwrap(),
            );
            let tool_text = if idx == 0 {
                json!({"ok": true, "kind": "image", "content": "Image: img.png"}).to_string()
            } else {
                big.clone()
            };
            req.messages.push(
                serde_json::from_value(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": tool_text
                }))
                .unwrap(),
            );
            if idx == 0 {
                tool_images::append_chat(
                    &mut req.messages,
                    vec![(
                        call_id,
                        vec![ToolImage {
                            data_url: format!("data:image/png;base64,{}", "B".repeat(2048)),
                        }],
                    )],
                );
            }
        }
        assert!(prune_completion(&mut req, false).unwrap());
        // 校验边界与图片附件完整性仍通过。
        assert!(completion_boundaries(&req.messages).is_ok());
        assert!(
            req.messages
                .iter()
                .all(|m| tool_images::chat_id(m).is_none()),
            "old image attachment should be removed"
        );
        // 跨协议转换仍然完全合法。
        assert!(
            history::convert(
                &Session::Completion(req.clone()),
                "fixture/model".into(),
                Protocol::Response,
                EditMode::Replace,
            )
            .is_ok()
        );
        // 幂等。
        assert!(!prune_completion(&mut req, false).unwrap());
    }
}
