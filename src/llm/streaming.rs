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

// 正常分片直接交给 SDK；仅在缺少正文注释时构造兼容补齐所需的 JSON 树。
pub(super) struct ResponseEvent(pub(super) ResponseStreamEvent);

impl<'de> Deserialize<'de> for ResponseEvent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = <&serde_json::value::RawValue>::deserialize(deserializer)?;
        match serde_json::from_str(raw.get()) {
            Ok(event) => return Ok(Self(event)),
            Err(error)
                if error.is_data()
                    && error.to_string().starts_with("missing field `annotations`") => {}
            Err(error) => return Err(D::Error::custom(error)),
        }
        let mut value: serde_json::Value =
            serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
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
            .map(Self)
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
        let native = serde_json::from_value(value).map_err(D::Error::custom)?;
        Ok(Self { native, thinking })
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
}

impl ChatStream {
    #[allow(deprecated)] // Reject legacy function_call rather than silently dropping it.
    pub(super) fn push(
        &mut self,
        chunk: ChatChunk,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        // Keep the last non-empty usage even when the chunk carries no choices:
        // streams with include_usage end with a usage-only chunk.
        if chunk.native.usage.is_some() {
            self.usage = chunk.native.usage;
        }
        if !chunk.thinking.is_empty() {
            ensure!(
                self.finish_reason.is_none(),
                "Chat Completions protocol: choice continued after finish_reason"
            );
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
                self.content.push_str(&text);
                emit(AgentEvent::AssistantDelta {
                    text,
                    refusal: false,
                });
            }
            if let Some(text) = delta.refusal.filter(|text| !text.is_empty()) {
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
                        call.function.arguments.push_str(&arguments);
                    }
                }
            }
            self.finish_reason = choice.finish_reason;
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
}

impl ResponsesStream {
    pub(super) fn push(
        &mut self,
        event: ResponseStreamEvent,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Option<ResponseTurn>> {
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
                self.delta(
                    event.output_index,
                    event.content_index,
                    event.delta,
                    false,
                    emit,
                )?;
            }
            ResponseStreamEvent::ResponseRefusalDelta(event) => {
                self.delta(
                    event.output_index,
                    event.content_index,
                    event.delta,
                    true,
                    emit,
                )?;
            }
            ResponseStreamEvent::ResponseReasoningSummaryTextDelta(event) => {
                self.thinking_text(
                    (event.output_index, true, event.summary_index),
                    &event.delta,
                    false,
                    emit,
                );
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
                self.thinking_text(
                    (event.output_index, false, event.content_index),
                    &event.delta,
                    false,
                    emit,
                );
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
            // 新增项、注释和参数分片不能触发工具执行。
            _ => {}
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
mod tests {
    use super::*;
    use crate::context::TokenUsage;
    use crate::llm::into_completion_turn;
    use async_openai::types::chat::ChatCompletionRequestAssistantMessageContent;
    use serde_json::{Value, json};

    fn chat(delta: Value, finish: Option<&str>) -> ChatChunk {
        serde_json::from_value(json!({
            "id":"fixture", "object":"chat.completion.chunk", "created":0, "model":"fixture",
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
        }))
        .unwrap()
    }

    fn usage_chunk(usage: Value) -> ChatChunk {
        serde_json::from_value(json!({
            "id":"fixture", "object":"chat.completion.chunk", "created":0, "model":"fixture",
            "choices":[], "usage":usage
        }))
        .unwrap()
    }

    fn event(value: Value) -> ResponseStreamEvent {
        serde_json::from_value(value).unwrap()
    }

    fn text(value: &str) -> Value {
        json!({"type":"output_text","text":value,"annotations":[]})
    }

    fn message(id: &str, content: Value) -> Value {
        json!({"type":"message","id":id,"role":"assistant","status":"completed","content":content})
    }

    fn completed(output: Value) -> ResponseStreamEvent {
        completed_with(output, None)
    }

    fn completed_with(output: Value, usage: Option<Value>) -> ResponseStreamEvent {
        let mut response = json!({
            "id":"fixture","object":"response","created_at":0,
            "model":"fixture","status":"completed","output":output
        });
        if let Some(usage) = usage {
            response["usage"] = usage;
        }
        event(json!({"type":"response.completed","sequence_number":100,"response":response}))
    }

    fn terminal_response(kind: &str, details: Value) -> ResponseStreamEvent {
        let mut response = json!({
            "id":"fixture", "object":"response", "created_at":0, "model":"fixture",
            "status":kind.strip_prefix("response.").unwrap(), "output":[]
        });
        let Value::Object(details) = details else {
            panic!("terminal response details must be an object");
        };
        response.as_object_mut().unwrap().extend(details);
        event(json!({"type":kind,"sequence_number":100,"response":response}))
    }

    fn delta(output: u32, content: u32, value: &str) -> ResponseStreamEvent {
        event(
            json!({"type":"response.output_text.delta","sequence_number":1,
            "item_id":"fixture","output_index":output,"content_index":content,"delta":value}),
        )
    }

    fn part_done(output: u32, content: u32, value: &str) -> ResponseStreamEvent {
        event(
            json!({"type":"response.content_part.done","sequence_number":2,
            "item_id":"fixture","output_index":output,"content_index":content,"part":text(value)}),
        )
    }

    fn capture(target: &mut Vec<(String, bool)>) -> impl FnMut(AgentEvent) + '_ {
        |event| match event {
            AgentEvent::AssistantDelta { text, refusal } => target.push((text, refusal)),
            _ => panic!("model client must only emit deltas"),
        }
    }

    #[test]
    fn thinking_chat_extensions_are_visible_and_separate_from_answer() {
        for field in ["reasoning_content", "reasoning"] {
            let mut stream = ChatStream::default();
            let mut thinking = String::new();
            let mut answer = String::new();
            let mut emit = |event| match event {
                AgentEvent::ThinkingDelta { text } => thinking.push_str(&text),
                AgentEvent::AssistantDelta { text, .. } => answer.push_str(&text),
                _ => panic!(),
            };
            stream
                .push(
                    chat(json!({"role":"assistant",field:"inspect 中"}), None),
                    &mut emit,
                )
                .unwrap();
            stream
                .push(
                    chat(json!({field:" then answer","content":"done"}), Some("stop")),
                    &mut emit,
                )
                .unwrap();
            let turn = into_completion_turn(stream.finish().unwrap()).unwrap();
            assert_eq!(thinking, "inspect 中 then answer");
            assert_eq!(answer, "done");
            assert_eq!(
                turn.message.content,
                Some(ChatCompletionRequestAssistantMessageContent::Text(
                    "done".into()
                ))
            );
            assert_eq!(turn.usage, None);
        }
    }

    #[test]
    fn thinking_responses_deduplicate_deltas_done_and_completed() {
        for summary in [true, false] {
            let mut stream = ResponsesStream::default();
            let mut thinking = String::new();
            let mut emit = |event| {
                if let AgentEvent::ThinkingDelta { text } = event {
                    thinking.push_str(&text);
                }
            };
            let kind = if summary {
                "response.reasoning_summary_text"
            } else {
                "response.reasoning_text"
            };
            let index = if summary {
                "summary_index"
            } else {
                "content_index"
            };
            stream.push(event(json!({"type":format!("{kind}.delta"),"sequence_number":1,"item_id":"r","output_index":0,index:0,"delta":"inspect"})), &mut emit).unwrap();
            stream.push(event(json!({"type":format!("{kind}.done"),"sequence_number":2,"item_id":"r","output_index":0,index:0,"text":"inspect 中"})), &mut emit).unwrap();
            let reasoning = if summary {
                json!({"type":"reasoning","id":"r","status":"completed","summary":[{"type":"summary_text","text":"inspect 中"}],"encrypted_content":"opaque-never-display"})
            } else {
                json!({"type":"reasoning","id":"r","status":"completed","summary":[],"content":[{"type":"reasoning_text","text":"inspect 中"}],"encrypted_content":"opaque-never-display"})
            };
            stream.push(event(json!({"type":"response.output_item.done","sequence_number":3,"output_index":0,"item":reasoning})), &mut emit).unwrap();
            let turn = stream
                .push(
                    completed(json!([reasoning, message("m", json!([text("answer")]))])),
                    &mut emit,
                )
                .unwrap()
                .unwrap();
            assert_eq!(thinking, "inspect 中");
            assert_eq!(turn.text, "answer");
        }
    }

    #[test]
    fn streaming_chat_aggregates_interleaved_tool_fragments_and_late_role() {
        let mut stream = ChatStream::default();
        let mut visible = Vec::new();
        stream.push(chat(json!({"content":"before","tool_calls":[
            {"index":1,"id":"second","type":"function","function":{"name":"ba","arguments":"{\"command\":"}},
            {"index":0,"id":"first","type":"function","function":{"name":"wr","arguments":"{\"path\":"}}
        ]}), None), &mut capture(&mut visible)).unwrap();
        stream.push(chat(json!({"role":"assistant","tool_calls":[
            {"index":0,"function":{"name":"ite","arguments":"\"a\",\"content\":\"b\"}"}},
            {"index":1,"function":{"name":"sh","arguments":"\"true\"}"}}
        ]}), Some("tool_calls")), &mut capture(&mut visible)).unwrap();
        let turn = into_completion_turn(stream.finish().unwrap()).unwrap();
        assert_eq!(visible, [("before".into(), false)]);
        let calls = turn.message.tool_calls.unwrap();
        let ChatCompletionMessageToolCalls::Function(first) = &calls[0] else {
            panic!()
        };
        let ChatCompletionMessageToolCalls::Function(second) = &calls[1] else {
            panic!()
        };
        assert_eq!((&*first.id, &*first.function.name), ("first", "write"));
        assert_eq!(first.function.arguments, r#"{"path":"a","content":"b"}"#);
        assert_eq!((&*second.id, &*second.function.name), ("second", "bash"));
        assert_eq!(second.function.arguments, r#"{"command":"true"}"#);
    }

    #[test]
    fn streaming_chat_requires_finish_role_and_valid_complete_batch() {
        let mut unfinished = ChatStream::default();
        unfinished
            .push(
                chat(json!({"role":"assistant","content":"partial"}), None),
                &mut |_| {},
            )
            .unwrap();
        assert!(
            unfinished
                .finish()
                .err()
                .unwrap()
                .to_string()
                .contains("before finish_reason")
        );
        for (delta, finish) in [
            (json!({"content":"no role"}), "stop"),
            (json!({"role":"user","content":"wrong role"}), "stop"),
            (json!({"role":"assistant","content":"truncated"}), "length"),
            (
                json!({"role":"assistant","tool_calls":[
                    {"index":0,"id":"same","function":{"name":"write","arguments":"{}"}},
                    {"index":1,"id":"same","function":{"name":"write","arguments":"{}"}}
                ]}),
                "tool_calls",
            ),
        ] {
            let mut stream = ChatStream::default();
            stream.push(chat(delta, Some(finish)), &mut |_| {}).unwrap();
            assert!(stream.finish().and_then(into_completion_turn).is_err());
        }
        let mut stream = ChatStream::default();
        assert!(
            stream
                .push(
                    chat(
                        json!({"function_call":{"name":"write","arguments":"{}"}}),
                        None
                    ),
                    &mut |_| {}
                )
                .is_err()
        );
    }

    #[test]
    fn streaming_chat_refusal_is_provisional_and_usage_only_chunk_keeps_usage() {
        let mut stream = ChatStream::default();
        let mut visible = Vec::new();
        stream
            .push(
                chat(json!({"role":"assistant","refusal":"no"}), None),
                &mut capture(&mut visible),
            )
            .unwrap();
        stream
            .push(
                chat(json!({"content":"yes"}), Some("stop")),
                &mut capture(&mut visible),
            )
            .unwrap();
        // The trailing usage-only chunk carries no choices; its usage must still
        // be kept and later non-empty usage wins over earlier ones.
        stream
            .push(
                usage_chunk(json!({"prompt_tokens":10,"completion_tokens":2,"total_tokens":12})),
                &mut capture(&mut visible),
            )
            .unwrap();
        stream
            .push(
                usage_chunk(
                    json!({"prompt_tokens":12000,"completion_tokens":300,"total_tokens":12300}),
                ),
                &mut capture(&mut visible),
            )
            .unwrap();
        let turn = into_completion_turn(stream.finish().unwrap()).unwrap();
        assert_eq!(
            turn.message.content,
            Some(ChatCompletionRequestAssistantMessageContent::Text(
                "yes".into()
            ))
        );
        assert!(!turn.refusal);
        assert_eq!(
            turn.usage,
            Some(TokenUsage {
                input_tokens: 12000,
                output_tokens: 300
            })
        );
        assert_eq!(visible, [("no".into(), true), ("yes".into(), false)]);
    }

    #[test]
    fn context_responses_usage_and_refusal_flags_from_completed_response() {
        let usage = json!({
            "input_tokens":12000, "output_tokens":300, "total_tokens":12300,
            "input_tokens_details":{"cached_tokens":40},
            "output_tokens_details":{"reasoning_tokens":50}
        });
        let mut stream = ResponsesStream::default();
        let turn = stream
            .push(
                completed_with(json!([message("m", json!([text("answer")]))]), Some(usage)),
                &mut |_| {},
            )
            .unwrap()
            .unwrap();
        // Input/output only; cached and reasoning details are not added on top.
        assert_eq!(
            turn.usage,
            Some(TokenUsage {
                input_tokens: 12000,
                output_tokens: 300
            })
        );
        assert!(!turn.refusal);

        let refusal = json!([{"type":"refusal","refusal":"cannot"}]);
        let mut stream = ResponsesStream::default();
        let turn = stream
            .push(completed(json!([message("m", refusal)])), &mut |_| {})
            .unwrap()
            .unwrap();
        assert!(turn.refusal);
        assert_eq!(turn.usage, None);

        let mut stream = ResponsesStream::default();
        let turn = stream
            .push(
                completed(json!([message(
                    "m",
                    json!([{"type":"refusal","refusal":"cannot"}, text("answer")])
                )])),
                &mut |_| {},
            )
            .unwrap()
            .unwrap();
        assert!(!turn.refusal);
    }

    #[test]
    fn streaming_responses_orders_parts_and_items_without_replaying_final_text() {
        let mut stream = ResponsesStream::default();
        let mut visible = Vec::new();
        stream
            .push(delta(1, 0, "C"), &mut capture(&mut visible))
            .unwrap();
        stream
            .push(delta(0, 1, "B"), &mut capture(&mut visible))
            .unwrap();
        assert!(visible.is_empty());
        stream
            .push(delta(0, 0, "A"), &mut capture(&mut visible))
            .unwrap();
        assert_eq!(visible, [("A".into(), false)]);
        stream
            .push(part_done(0, 0, "A"), &mut capture(&mut visible))
            .unwrap();
        assert_eq!(visible, [("A".into(), false), ("B".into(), false)]);
        stream
            .push(
                event(
                    json!({"type":"response.output_item.done","sequence_number":3,
            "output_index":0,"item":message("first", json!([text("A"),text("B")]))}),
                ),
                &mut capture(&mut visible),
            )
            .unwrap();
        assert_eq!(
            visible,
            [
                ("A".into(), false),
                ("B".into(), false),
                ("C".into(), false)
            ]
        );
        let turn = stream
            .push(
                completed(json!([
                    message("first", json!([text("A"), text("B")])),
                    message("second", json!([text("C")]))
                ])),
                &mut capture(&mut visible),
            )
            .unwrap()
            .unwrap();
        assert_eq!(turn.text, "ABC");
        assert!(!turn.has_calls);
        assert_eq!(visible.len(), 3);
    }

    #[test]
    fn streaming_responses_missing_part_tail_defers_successors_until_completion() {
        let mut stream = ResponsesStream::default();
        let mut visible = Vec::new();
        stream
            .push(delta(0, 0, "A"), &mut capture(&mut visible))
            .unwrap();
        stream
            .push(delta(0, 1, "C"), &mut capture(&mut visible))
            .unwrap();
        stream
            .push(part_done(0, 0, "AB"), &mut capture(&mut visible))
            .unwrap();
        assert_eq!(visible, [("A".into(), false)]);
        stream
            .push(
                completed(json!([message("m", json!([text("AB"), text("C")]))])),
                &mut capture(&mut visible),
            )
            .unwrap();
        assert_eq!(visible, [("A".into(), false), ("BC".into(), false)]);
    }

    #[test]
    fn streaming_responses_completed_only_and_refusal_priority() {
        for (parts, expected, refusal) in [
            (json!([text("whole")]), "whole", false),
            (
                json!([{"type":"refusal","refusal":"cannot"}]),
                "cannot",
                true,
            ),
            (
                json!([{"type":"refusal","refusal":"cannot"},text("answer")]),
                "answer",
                false,
            ),
        ] {
            let mut stream = ResponsesStream::default();
            let mut visible = Vec::new();
            let turn = stream
                .push(
                    completed(json!([message("m", parts)])),
                    &mut capture(&mut visible),
                )
                .unwrap()
                .unwrap();
            assert_eq!(turn.text, expected);
            assert_eq!(visible, [(expected.into(), refusal)]);
        }
    }

    #[test]
    fn streaming_responses_rejects_conflicts_and_invalid_batches_before_return() {
        for output in [
            json!([message("m", json!([text("different")])),
                {"type":"function_call","id":"fc","call_id":"call","name":"write","arguments":"{}"}]),
            json!([message("m", json!([text("partial-tail")])),
                {"type":"function_call","id":"fc","call_id":"","name":"write","arguments":"{}"}]),
        ] {
            let mut stream = ResponsesStream::default();
            let mut visible = Vec::new();
            stream
                .push(delta(0, 0, "partial"), &mut capture(&mut visible))
                .unwrap();
            assert!(
                stream
                    .push(completed(output), &mut capture(&mut visible))
                    .is_err()
            );
            assert_eq!(visible, [("partial".into(), false)]);
        }
    }

    #[test]
    fn streaming_responses_argument_deltas_never_trigger_tools_or_text() {
        let mut stream = ResponsesStream::default();
        let mut visible = Vec::new();
        for value in [
            json!({"type":"response.function_call_arguments.delta","sequence_number":1,
                "item_id":"fc","output_index":0,"delta":"{}"}),
            json!({"type":"response.function_call_arguments.done","sequence_number":2,
                "item_id":"fc","output_index":0,"arguments":"{}","name":"write"}),
        ] {
            assert!(
                stream
                    .push(event(value), &mut capture(&mut visible))
                    .unwrap()
                    .is_none()
            );
        }
        assert!(visible.is_empty());
        let turn = stream.push(completed(json!([
            {"type":"function_call","id":"fc","call_id":"call","name":"write","arguments":"{}"},
            message("m", json!([text("tool preface")]))
        ])), &mut capture(&mut visible)).unwrap().unwrap();
        assert!(turn.has_calls);
        assert_eq!(turn.text, "tool preface");
        assert_eq!(visible, [("tool preface".into(), false)]);
    }

    #[test]
    fn streaming_responses_failure_codes_are_allowlisted_and_actionable() {
        let private = "private-provider-body fixture-key https://private.invalid/\u{1b}[31m";
        for kind in ["response.failed", "error"] {
            for code in [
                "invalid_request",
                "invalid_request_error",
                "server_error",
                "rate_limit_exceeded",
                "invalid_prompt",
            ] {
                let failure = if kind == "error" {
                    event(json!({"type":kind,"sequence_number":1,
                        "code":code,"message":private,"param":private}))
                } else {
                    terminal_response(
                        kind,
                        json!({"error":{
                            "code":code,"message":private,"param":private
                        }}),
                    )
                };
                let error = ResponsesStream::default()
                    .push(failure, &mut |_| panic!("failure must not emit output"))
                    .err()
                    .unwrap();
                let diagnostic = format!("{error:#}\n{error:?}");
                assert!(diagnostic.contains(if kind == "error" { "error event" } else { kind }));
                assert!(diagnostic.contains(code));
                if code.starts_with("invalid_request") {
                    assert!(diagnostic.contains("reasoning.effort"));
                    assert!(diagnostic.contains("/effort low"));
                    assert!(diagnostic.contains("request options"));
                }
                for secret in ["private-provider-body", "fixture-key", "https://", "\u{1b}"] {
                    assert!(!diagnostic.contains(secret));
                }
            }
        }
    }

    #[test]
    fn streaming_responses_unknown_and_missing_error_codes_still_fail_safely() {
        for kind in ["response.failed", "error"] {
            for code in [
                Some("private-code fixture-key https://private.invalid/"),
                Some("invalid_request\nprivate-suffix"),
                Some("server_error-private-suffix"),
                Some(""),
                None,
            ] {
                let private = "private-message fixture-key https://private.invalid/";
                let failure = if kind == "error" {
                    event(json!({"type":kind,"sequence_number":1,
                        "code":code,"message":private,"param":"private-param"}))
                } else {
                    let error = code.map(|code| json!({"code":code,"message":private}));
                    terminal_response(kind, json!({"error":error}))
                };
                let error = ResponsesStream::default()
                    .push(failure, &mut |_| panic!("failure must not emit output"))
                    .err()
                    .unwrap();
                let diagnostic = format!("{error:#}\n{error:?}");
                assert!(diagnostic.contains(if kind == "error" { "error event" } else { kind }));
                for secret in [
                    "private-code",
                    "private-suffix",
                    "private-message",
                    "private-param",
                    "fixture-key",
                    "https://",
                    "invalid_request",
                    "server_error",
                ] {
                    assert!(!diagnostic.contains(secret));
                }
            }
        }
        for failure in [
            terminal_response("response.failed", json!({})),
            event(json!({"type":"error","sequence_number":1,"message":"private-message"})),
        ] {
            assert!(
                ResponsesStream::default()
                    .push(failure, &mut |_| {})
                    .is_err()
            );
        }
    }

    #[test]
    fn streaming_responses_incomplete_reasons_are_allowlisted() {
        for reason in [
            "max_output_tokens",
            "content_filter",
            "private-reason fixture-key https://private.invalid/",
            "max_output_tokens\nprivate-suffix",
            "content_filter-private-suffix",
            "",
        ] {
            let error = ResponsesStream::default()
                .push(
                    terminal_response(
                        "response.incomplete",
                        json!({
                            "incomplete_details":{"reason":reason},
                            "error":{"code":"private-code","message":"private-message"}
                        }),
                    ),
                    &mut |_| panic!("incomplete response must not emit output"),
                )
                .err()
                .unwrap();
            let diagnostic = format!("{error:#}\n{error:?}");
            assert!(diagnostic.contains("response.incomplete"));
            if matches!(reason, "max_output_tokens" | "content_filter") {
                assert!(diagnostic.contains(reason));
            } else {
                assert!(!diagnostic.contains("max_output_tokens"));
                assert!(!diagnostic.contains("content_filter"));
                if !reason.is_empty() {
                    assert!(!diagnostic.contains(reason));
                }
            }
            for secret in ["private-", "fixture-key", "https://"] {
                assert!(!diagnostic.contains(secret));
            }
        }
        for details in [json!({}), json!({"incomplete_details":null})] {
            let error = ResponsesStream::default()
                .push(
                    terminal_response("response.incomplete", details),
                    &mut |_| {},
                )
                .err()
                .unwrap();
            assert!(error.to_string().contains("response.incomplete"));
        }
    }

    #[test]
    fn streaming_responses_terminal_failures_keep_partial_display_without_committing_output() {
        let output = json!([
            message("m", json!([text("partial-uncommitted-tail")])),
            {"type":"function_call","id":"fc","call_id":"call","name":"write","arguments":"{}"}
        ]);
        for (failure, label) in [
            (
                terminal_response("response.failed", json!({"output":output})),
                "response.failed",
            ),
            (
                terminal_response("response.incomplete", json!({"output":output})),
                "response.incomplete",
            ),
            (
                event(json!({"type":"error","sequence_number":3,"message":"private-message"})),
                "error event",
            ),
        ] {
            let mut stream = ResponsesStream::default();
            let mut visible = Vec::new();
            assert!(
                stream
                    .push(delta(0, 0, "partial"), &mut capture(&mut visible))
                    .unwrap()
                    .is_none()
            );
            assert!(
                stream
                    .push(
                        event(json!({
                            "type":"response.output_item.done","sequence_number":2,"output_index":1,
                            "item":{"type":"function_call","id":"fc","call_id":"call",
                                "name":"write","arguments":"{}"}
                        })),
                        &mut capture(&mut visible)
                    )
                    .unwrap()
                    .is_none()
            );
            // No ResponseTurn is returned, so neither history nor tools can consume
            // the terminal payload or the previously streamed function call.
            let error = stream
                .push(failure, &mut capture(&mut visible))
                .err()
                .unwrap();
            let diagnostic = error.to_string();
            assert!(diagnostic.contains(label));
            for other in ["response.failed", "response.incomplete", "error event"] {
                if other != label {
                    assert!(!diagnostic.contains(other));
                }
            }
            assert!(!diagnostic.contains("private-message"));
            assert_eq!(visible, [("partial".into(), false)]);
        }
    }
}
