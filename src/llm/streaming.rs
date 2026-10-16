use super::{ResponseTurn, into_response_turn};
use crate::events::AgentEvent;
use anyhow::{Result, bail, ensure};
use async_openai::types::{
    chat::{
        ChatChoice, ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
        ChatCompletionResponseMessage, CreateChatCompletionResponse,
        CreateChatCompletionStreamResponse, FinishReason, Role,
    },
    responses::{OutputContent, OutputItem, OutputMessageContent, ResponseStreamEvent},
};
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct ChatStream {
    role: Option<Role>,
    content: String,
    refusal: String,
    calls: BTreeMap<u32, ChatCompletionMessageToolCall>,
    finish_reason: Option<FinishReason>,
}

impl ChatStream {
    #[allow(deprecated)] // Reject legacy function_call rather than silently dropping it.
    pub(super) fn push(
        &mut self,
        chunk: CreateChatCompletionStreamResponse,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        for choice in chunk.choices.into_iter().filter(|choice| choice.index == 0) {
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
            usage: None,
            metadata: None,
            moderation: None,
        })
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
            ResponseStreamEvent::ResponseFailed(_)
            | ResponseStreamEvent::ResponseIncomplete(_)
            | ResponseStreamEvent::ResponseError(_) => {
                bail!("Responses protocol: stream failed");
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
            // Added events, reasoning, annotations and argument deltas do not
            // contain authoritative output and cannot trigger tool execution.
            _ => {}
        }
        self.flush(emit);
        Ok(None)
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
    use crate::llm::into_message;
    use async_openai::types::chat::ChatCompletionRequestAssistantMessageContent;
    use serde_json::{Value, json};

    fn chat(delta: Value, finish: Option<&str>) -> CreateChatCompletionStreamResponse {
        serde_json::from_value(json!({
            "id":"fixture", "object":"chat.completion.chunk", "created":0, "model":"fixture",
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
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
        event(json!({
            "type":"response.completed","sequence_number":100,
            "response":{"id":"fixture","object":"response","created_at":0,
                "model":"fixture","status":"completed","output":output}
        }))
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
        let message = into_message(stream.finish().unwrap()).unwrap();
        assert_eq!(visible, [("before".into(), false)]);
        let calls = message.tool_calls.unwrap();
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
            assert!(stream.finish().and_then(into_message).is_err());
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
    fn streaming_chat_refusal_is_provisional_and_usage_chunks_are_ignored() {
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
        let mut usage = chat(json!({}), None);
        usage.choices.clear();
        stream.push(usage, &mut capture(&mut visible)).unwrap();
        let message = into_message(stream.finish().unwrap()).unwrap();
        assert_eq!(
            message.content,
            Some(ChatCompletionRequestAssistantMessageContent::Text(
                "yes".into()
            ))
        );
        assert_eq!(visible, [("no".into(), true), ("yes".into(), false)]);
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
    fn streaming_responses_stream_errors_hide_provider_details() {
        let mut stream = ResponsesStream::default();
        let error = stream.push(event(json!({"type":"error","sequence_number":1,
            "code":"private-provider-code","message":"private-provider-body fixture-key","param":null})),
            &mut |_| {}).err().unwrap().to_string();
        assert_eq!(error, "Responses protocol: stream failed");
    }
}
