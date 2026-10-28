use super::{ResponseTurn, into_response_turn};
use crate::events::AgentEvent;
use anyhow::{Result, bail, ensure};
use async_openai::types::{
    chat::{
        ChatChoice, ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
        ChatCompletionResponseMessage, CompletionUsage, CreateChatCompletionResponse,
        CreateChatCompletionStreamResponse, FinishReason, Role,
    },
    responses::{
        OutputContent, OutputItem, OutputMessageContent, ReasoningItem, ReasoningItemContent,
        ResponseErrorCode, ResponseStreamEvent, SummaryPart,
    },
};
use serde::{Deserialize, Deserializer, de::Error};
use std::collections::BTreeMap;

// 正常分片直接交给 SDK；仅在缺少可兼容字段时构造补齐所需的 JSON 树。
pub(super) struct ResponseEvent(pub(super) ResponseStreamEvent, pub(super) Option<u64>);

// 在 SDK 丢弃未知扩展前保留缓存写入量；不从输入余量推算。
fn cache_write_tokens(usage: &serde_json::Value, details: &str) -> Option<u64> {
    usage[details]["cache_write_tokens"]
        .as_u64()
        .or_else(|| usage["cache_write_tokens"].as_u64())
        .or_else(|| usage["cache_creation_input_tokens"].as_u64())
}

#[derive(Deserialize)]
struct ResponseUsage<'a> {
    #[serde(borrow)]
    response: Option<RawUsage<'a>>,
}
#[derive(Deserialize)]
struct RawUsage<'a> {
    #[serde(borrow)]
    usage: Option<&'a serde_json::value::RawValue>,
}

impl<'de> Deserialize<'de> for ResponseEvent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = <&serde_json::value::RawValue>::deserialize(deserializer)?;
        // usage 只随 response.* 生命周期事件出现；先用子串探测短路，
        // 让占绝大多数的 delta 分片免去一次全文 JSON 扫描。
        // 探测按带引号的字段名匹配，与空白写法无关；误报只多一次解析，结果不变。
        let written = raw
            .get()
            .contains("\"response\"")
            .then(|| {
                serde_json::from_str::<ResponseUsage<'_>>(raw.get())
                    .ok()
                    .and_then(|event| event.response?.usage)
                    .and_then(|usage| serde_json::from_str::<serde_json::Value>(usage.get()).ok())
                    .and_then(|usage| cache_write_tokens(&usage, "input_tokens_details"))
            })
            .flatten();
        match serde_json::from_str(raw.get()) {
            Ok(event) => return Ok(Self(event, written)),
            Err(error)
                if error.is_data()
                    && (error.to_string().starts_with("missing field `annotations`")
                        || error.to_string().starts_with("missing field `arguments`")) => {}
            Err(error) => return Err(D::Error::custom(error)),
        }
        let mut value: serde_json::Value =
            serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
        if value.get("type").and_then(serde_json::Value::as_str)
            == Some("response.output_item.added")
            && let Some(item) = value.get_mut("item")
            && item.get("type").and_then(serde_json::Value::as_str) == Some("function_call")
            && let Some(object) = item.as_object_mut()
        {
            // 起始事件的参数可由后续分片提供；done/completed 仍须带完整参数。
            object
                .entry("arguments")
                .or_insert_with(|| serde_json::Value::String(String::new()));
        }
        if let Some(part) = value.get_mut("part") {
            default_output_text_annotations(part);
        }
        if let Some(item) = value.get_mut("item") {
            default_message_annotations(item);
        }
        if let Some(output) = value
            .pointer_mut("/response/output")
            .and_then(serde_json::Value::as_array_mut)
        {
            for item in output {
                default_message_annotations(item);
            }
        }
        serde_json::from_value(value)
            .map(|event| Self(event, written))
            .map_err(D::Error::custom)
    }
}

fn default_message_annotations(item: &mut serde_json::Value) {
    if item.get("type").and_then(serde_json::Value::as_str) != Some("message") {
        return;
    }
    if let Some(content) = item
        .get_mut("content")
        .and_then(serde_json::Value::as_array_mut)
    {
        for part in content {
            default_output_text_annotations(part);
        }
    }
}

fn default_output_text_annotations(part: &mut serde_json::Value) {
    if part.get("type").and_then(serde_json::Value::as_str) != Some("output_text") {
        return;
    }
    if let Some(object) = part.as_object_mut() {
        object
            .entry("annotations")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    }
}

// 保留 SDK 的完整分片校验，同时读取兼容供应商的可读思考扩展。
pub(super) struct ChatChunk {
    native: CreateChatCompletionStreamResponse,
    thinking: String,
    cache_write_tokens: Option<u64>,
}

impl<'de> Deserialize<'de> for ChatChunk {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let thinking = value["choices"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|choice| choice["index"] == 0)
            .filter_map(|choice| {
                choice["delta"]["reasoning_content"]
                    .as_str()
                    .or_else(|| choice["delta"]["reasoning"].as_str())
            })
            .collect::<String>();
        let cache_write_tokens = cache_write_tokens(&value["usage"], "prompt_tokens_details");
        let native = serde_json::from_value(value).map_err(D::Error::custom)?;
        Ok(Self {
            native,
            thinking,
            cache_write_tokens,
        })
    }
}

#[derive(Default)]
pub(super) struct ChatStream {
    role: Option<Role>,
    content: String,
    refusal: String,
    calls: BTreeMap<u32, ChatCompletionMessageToolCall>,
    finish_reason: Option<FinishReason>,
    usage: Option<CompletionUsage>,
    pub(super) cache_write_tokens: Option<u64>,
    output_bytes: u64,
}

impl ChatStream {
    pub(super) fn stats_snapshot(&self) -> serde_json::Value {
        let mut value = serde_json::json!({"role":self.role,"content":self.content,"refusal":self.refusal,"tool_calls":self.calls.values().collect::<Vec<_>>(),"finish_reason":self.finish_reason,"usage":self.usage});
        if let Some(written) = self.cache_write_tokens {
            value["usage"]["prompt_tokens_details"]["cache_write_tokens"] = written.into();
        }
        value
    }

    #[allow(deprecated)] // Reject legacy function_call rather than silently dropping it.
    pub(super) fn push(
        &mut self,
        chunk: ChatChunk,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        // Only borrowed lengths feed the heuristic speed, and the cumulative
        // value is released once this chunk passed the protocol validation.
        let mut bytes: u64 = 0;
        // Keep the last non-empty usage even when the chunk carries no choices:
        // streams with include_usage end with a usage-only chunk.
        if chunk.native.usage.is_some() {
            self.usage = chunk.native.usage;
            self.cache_write_tokens = chunk.cache_write_tokens;
        }
        if !chunk.thinking.is_empty() {
            ensure!(
                self.finish_reason.is_none(),
                "Chat Completions protocol: choice continued after finish_reason"
            );
            bytes = bytes.saturating_add(chunk.thinking.len() as u64);
            emit(AgentEvent::ThinkingDelta {
                text: chunk.thinking,
            });
        }
        for choice in chunk
            .native
            .choices
            .into_iter()
            .filter(|choice| choice.index == 0)
        {
            ensure!(
                self.finish_reason.is_none(),
                "Chat Completions protocol: choice continued after finish_reason"
            );
            let delta = choice.delta;
            ensure!(
                delta.function_call.is_none(),
                "Chat Completions protocol: legacy function_call is unsupported"
            );
            if let Some(role) = delta.role {
                ensure!(
                    self.role.as_ref().is_none_or(|previous| *previous == role),
                    "Chat Completions protocol: conflicting role"
                );
                self.role = Some(role);
            }
            if let Some(text) = delta.content.filter(|text| !text.is_empty()) {
                bytes = bytes.saturating_add(text.len() as u64);
                self.content.push_str(&text);
                emit(AgentEvent::AssistantDelta {
                    text,
                    refusal: false,
                });
            }
            if let Some(text) = delta.refusal.filter(|text| !text.is_empty()) {
                bytes = bytes.saturating_add(text.len() as u64);
                self.refusal.push_str(&text);
                emit(AgentEvent::AssistantDelta {
                    text,
                    refusal: true,
                });
            }
            for delta in delta.tool_calls.unwrap_or_default() {
                // The SDK's FunctionType enum rejects non-function chunks at decoding.
                let call = self.calls.entry(delta.index).or_default();
                if let Some(id) = delta.id {
                    call.id.push_str(&id);
                }
                if let Some(function) = delta.function {
                    if let Some(name) = function.name {
                        call.function.name.push_str(&name);
                    }
                    if let Some(arguments) = function.arguments {
                        bytes = bytes.saturating_add(arguments.len() as u64);
                        call.function.arguments.push_str(&arguments);
                    }
                }
            }
            self.finish_reason = choice.finish_reason;
        }
        if bytes > 0 {
            self.output_bytes = self.output_bytes.saturating_add(bytes);
            emit(AgentEvent::ModelOutputProgress {
                output_bytes: self.output_bytes,
            });
        }
        Ok(())
    }

    #[allow(deprecated)] // SDK response construction requires deprecated nullable fields.
    pub(super) fn finish(self) -> Result<CreateChatCompletionResponse> {
        ensure!(
            self.finish_reason.is_some(),
            "Chat Completions protocol: stream ended before finish_reason"
        );
        let role = self
            .role
            .ok_or_else(|| anyhow::anyhow!("Chat Completions protocol: missing assistant role"))?;
        // Feed the reconstructed batch through the existing complete-response validator.
        Ok(CreateChatCompletionResponse {
            id: String::new(),
            object: "chat.completion".into(),
            created: 0,
            model: String::new(),
            choices: vec![ChatChoice {
                index: 0,
                message: ChatCompletionResponseMessage {
                    content: Some(self.content),
                    refusal: Some(self.refusal),
                    role,
                    tool_calls: Some(
                        self.calls
                            .into_values()
                            .map(ChatCompletionMessageToolCalls::Function)
                            .collect(),
                    ),
                    annotations: None,
                    function_call: None,
                    audio: None,
                },
                finish_reason: self.finish_reason,
                logprobs: None,
            }],
            service_tier: None,
            system_fingerprint: None,
            usage: self.usage,
            metadata: None,
            moderation: None,
        })
    }
}

// Only static allowlisted descriptions cross the provider-to-user trust boundary.
// Never format provider messages, parameters, or unrecognized codes.
fn response_error_description(code: &str) -> &'static str {
    match code {
        "invalid_request" => {
            "invalid_request: check the model's reasoning.effort (try /effort low) and other request options"
        }
        "invalid_request_error" => {
            "invalid_request_error: check the model's reasoning.effort (try /effort low) and other request options"
        }
        "server_error" => "server_error: provider could not complete the response",
        "rate_limit_exceeded" => "rate_limit_exceeded: provider rate limit reached",
        "invalid_prompt" => "invalid_prompt: provider rejected the prompt",
        _ => "unrecognized error code",
    }
}

fn response_failure_description(code: &ResponseErrorCode) -> &'static str {
    match code {
        ResponseErrorCode::ServerError => response_error_description("server_error"),
        ResponseErrorCode::RateLimitExceeded => response_error_description("rate_limit_exceeded"),
        ResponseErrorCode::InvalidPrompt => response_error_description("invalid_prompt"),
        ResponseErrorCode::Other(code) => response_error_description(code),
        _ => "unrecognized error code",
    }
}

#[derive(Default)]
struct Part {
    text: String,
    refusal: Option<bool>,
    emitted: usize,
    // Lifecycle payloads only decide when the next part may be displayed. They
    // never supply display text or history; completed remains authoritative.
    done: Option<(String, bool)>,
}

#[derive(Default)]
struct Item {
    parts: BTreeMap<usize, Part>,
    done_parts: Option<usize>,
}

#[derive(Default)]
pub(super) struct ResponsesStream {
    items: BTreeMap<usize, Item>,
    output_index: usize,
    content_index: usize,
    content: String,
    refusal: String,
    thinking: BTreeMap<(u32, bool, u32), String>,
    last_thinking: Option<(u32, bool, u32)>,
    output_bytes: u64,
}

impl ResponsesStream {
    pub(super) fn push(
        &mut self,
        event: ResponseStreamEvent,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Option<ResponseTurn>> {
        // Deltas are the only credited bytes; done/completed copies repeat text
        // and would double count. The sum is emitted after validation passes.
        let mut bytes: u64 = 0;
        match event {
            ResponseStreamEvent::ResponseCompleted(event) => {
                let turn = into_response_turn(event.response)?;
                for (index, item) in turn.output.iter().enumerate() {
                    if let OutputItem::Reasoning(reasoning) = item {
                        self.reasoning_done(index as u32, reasoning, emit);
                    }
                }
                let refusal = !turn.output.iter().any(|item| match item {
                    OutputItem::Message(message) => message.content.iter().any(|part| {
                        matches!(part, OutputMessageContent::OutputText(text) if !text.text.trim().is_empty())
                    }),
                    _ => false,
                });
                ensure!(
                    self.content.trim().is_empty()
                        || (!refusal && turn.text.starts_with(&self.content)),
                    "Responses protocol: completed text conflicts with streamed content"
                );
                let sent = if refusal {
                    &self.refusal
                } else {
                    &self.content
                };
                if let Some(tail) = turn.text.strip_prefix(sent.as_str()) {
                    if !tail.is_empty() {
                        emit(AgentEvent::AssistantDelta {
                            text: tail.to_owned(),
                            refusal,
                        });
                    }
                } else if !refusal && self.content.trim().is_empty() && !turn.text.is_empty() {
                    emit(AgentEvent::AssistantDelta {
                        text: turn.text.clone(),
                        refusal: false,
                    });
                }
                return Ok(Some(turn));
            }
            ResponseStreamEvent::ResponseFailed(event) => {
                let detail = event
                    .response
                    .error
                    .as_ref()
                    .map(|error| response_failure_description(&error.code))
                    .unwrap_or("no error details");
                bail!("Responses protocol: response.failed ({detail})");
            }
            ResponseStreamEvent::ResponseIncomplete(event) => {
                let detail = match event.response.incomplete_details.as_ref() {
                    Some(details) => match details.reason.as_str() {
                        "max_output_tokens" => "max_output_tokens: output token limit reached",
                        "content_filter" => "content_filter: output blocked by content filtering",
                        _ => "unrecognized incomplete reason",
                    },
                    None => "no incomplete details",
                };
                bail!("Responses protocol: response.incomplete ({detail})");
            }
            ResponseStreamEvent::ResponseError(event) => {
                let detail = event
                    .code
                    .as_deref()
                    .map(response_error_description)
                    .unwrap_or("no error code");
                bail!("Responses protocol: error event ({detail})");
            }
            ResponseStreamEvent::ResponseOutputTextDelta(event) => {
                let len = event.delta.len() as u64;
                self.delta(
                    event.output_index,
                    event.content_index,
                    event.delta,
                    false,
                    emit,
                )?;
                bytes = bytes.saturating_add(len);
            }
            ResponseStreamEvent::ResponseRefusalDelta(event) => {
                let len = event.delta.len() as u64;
                self.delta(
                    event.output_index,
                    event.content_index,
                    event.delta,
                    true,
                    emit,
                )?;
                bytes = bytes.saturating_add(len);
            }
            ResponseStreamEvent::ResponseReasoningSummaryTextDelta(event) => {
                let len = event.delta.len() as u64;
                self.thinking_text(
                    (event.output_index, true, event.summary_index),
                    &event.delta,
                    false,
                    emit,
                );
                bytes = bytes.saturating_add(len);
            }
            ResponseStreamEvent::ResponseReasoningSummaryTextDone(event) => {
                self.thinking_text(
                    (event.output_index, true, event.summary_index),
                    &event.text,
                    true,
                    emit,
                );
            }
            ResponseStreamEvent::ResponseReasoningSummaryPartDone(event) => {
                let SummaryPart::SummaryText(part) = event.part;
                self.thinking_text(
                    (event.output_index, true, event.summary_index),
                    &part.text,
                    true,
                    emit,
                );
            }
            ResponseStreamEvent::ResponseReasoningTextDelta(event) => {
                let len = event.delta.len() as u64;
                self.thinking_text(
                    (event.output_index, false, event.content_index),
                    &event.delta,
                    false,
                    emit,
                );
                bytes = bytes.saturating_add(len);
            }
            ResponseStreamEvent::ResponseReasoningTextDone(event) => {
                self.thinking_text(
                    (event.output_index, false, event.content_index),
                    &event.text,
                    true,
                    emit,
                );
            }
            ResponseStreamEvent::ResponseContentPartDone(event) => {
                let done = match event.part {
                    OutputContent::OutputText(text) => Some((text.text, false)),
                    OutputContent::Refusal(refusal) => Some((refusal.refusal, true)),
                    OutputContent::ReasoningText(_) => None,
                };
                if let Some(done) = done {
                    self.part_done(event.output_index, event.content_index, done);
                }
            }
            ResponseStreamEvent::ResponseOutputTextDone(event) => {
                self.part_done(event.output_index, event.content_index, (event.text, false));
            }
            ResponseStreamEvent::ResponseRefusalDone(event) => {
                self.part_done(
                    event.output_index,
                    event.content_index,
                    (event.refusal, true),
                );
            }
            ResponseStreamEvent::ResponseOutputItemDone(event) => {
                if let OutputItem::Reasoning(reasoning) = &event.item {
                    self.reasoning_done(event.output_index, reasoning, emit);
                }
                let index = event.output_index as usize;
                if index >= self.output_index {
                    let item = self.items.entry(index).or_default();
                    item.done_parts = Some(match event.item {
                        OutputItem::Message(message) => {
                            let count = message.content.len();
                            for (index, content) in message.content.into_iter().enumerate() {
                                // Earlier parts were already emitted and removed.
                                if event.output_index as usize == self.output_index
                                    && index < self.content_index
                                {
                                    continue;
                                }
                                let done = match content {
                                    OutputMessageContent::OutputText(text) => (text.text, false),
                                    OutputMessageContent::Refusal(refusal) => {
                                        (refusal.refusal, true)
                                    }
                                };
                                item.parts.entry(index).or_default().done = Some(done);
                            }
                            count
                        }
                        _ => 0,
                    });
                }
            }
            // Tool arguments produce no visible text but still advance generation.
            ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(event) => {
                bytes = bytes.saturating_add(event.delta.len() as u64);
            }
            // 新增项和注释不能触发工具执行，也不计入输出字节。
            _ => {}
        }
        if bytes > 0 {
            self.output_bytes = self.output_bytes.saturating_add(bytes);
            emit(AgentEvent::ModelOutputProgress {
                output_bytes: self.output_bytes,
            });
        }
        self.flush(emit);
        Ok(None)
    }

    fn reasoning_done(
        &mut self,
        output: u32,
        item: &ReasoningItem,
        emit: &mut dyn FnMut(AgentEvent),
    ) {
        for (index, part) in item.summary.iter().enumerate() {
            let SummaryPart::SummaryText(part) = part;
            self.thinking_text((output, true, index as u32), &part.text, true, emit);
        }
        for (index, part) in item.content.iter().flatten().enumerate() {
            let ReasoningItemContent::ReasoningText(part) = part;
            self.thinking_text((output, false, index as u32), &part.text, true, emit);
        }
    }

    fn thinking_text(
        &mut self,
        key: (u32, bool, u32),
        text: &str,
        done: bool,
        emit: &mut dyn FnMut(AgentEvent),
    ) {
        let sent = self.thinking.entry(key).or_default();
        // done 与 completed 可能重复同一段，只显示尚未输出的后缀。
        let tail = if done {
            text.strip_prefix(sent.as_str()).unwrap_or_default()
        } else {
            text
        };
        if tail.is_empty() {
            return;
        }
        let separator = if self.last_thinking.is_some_and(|last| last != key) {
            "\n\n"
        } else {
            ""
        };
        emit(AgentEvent::ThinkingDelta {
            text: format!("{separator}{tail}"),
        });
        sent.push_str(tail);
        self.last_thinking = Some(key);
    }

    fn delta(
        &mut self,
        output: u32,
        content: u32,
        text: String,
        refusal: bool,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        let key = (output as usize, content as usize);
        ensure!(
            key >= (self.output_index, self.content_index),
            "Responses protocol: text continued after completed part"
        );
        let part = self
            .items
            .entry(key.0)
            .or_default()
            .parts
            .entry(key.1)
            .or_default();
        ensure!(
            part.refusal.is_none_or(|previous| previous == refusal),
            "Responses protocol: conflicting content part type"
        );
        part.refusal = Some(refusal);
        part.text.push_str(&text);
        if key == (self.output_index, self.content_index) && !text.is_empty() {
            // Move the SDK's owned delta directly to the display callback on
            // the common ordered path, instead of allocating a replacement.
            if refusal {
                self.refusal.push_str(&text);
            } else {
                self.content.push_str(&text);
            }
            part.emitted = part.text.len();
            emit(AgentEvent::AssistantDelta { text, refusal });
        }
        Ok(())
    }

    fn part_done(&mut self, output: u32, content: u32, done: (String, bool)) {
        let key = (output as usize, content as usize);
        if key >= (self.output_index, self.content_index) {
            self.items
                .entry(key.0)
                .or_default()
                .parts
                .entry(key.1)
                .or_default()
                .done = Some(done);
        }
    }

    fn flush(&mut self, emit: &mut dyn FnMut(AgentEvent)) {
        while let Some(item) = self.items.get_mut(&self.output_index) {
            if let Some(part) = item.parts.get_mut(&self.content_index) {
                let refusal = part.refusal.unwrap_or(false);
                if part.emitted < part.text.len() {
                    let text = &part.text[part.emitted..];
                    if refusal {
                        self.refusal.push_str(text);
                    } else {
                        self.content.push_str(text);
                    }
                    emit(AgentEvent::AssistantDelta {
                        text: text.to_owned(),
                        refusal,
                    });
                    part.emitted = part.text.len();
                }
                if part.done.as_ref().is_some_and(|(text, kind)| {
                    *text == part.text && (text.is_empty() || *kind == refusal)
                }) {
                    item.parts.remove(&self.content_index);
                    self.content_index += 1;
                    continue;
                }
            }
            if item.done_parts == Some(self.content_index) {
                self.items.remove(&self.output_index);
                self.output_index += 1;
                self.content_index = 0;
                continue;
            }
            break;
        }
    }
}

#[cfg(test)]
mod tests;
