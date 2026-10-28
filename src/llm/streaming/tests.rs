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

fn arguments_delta(value: &str) -> ResponseStreamEvent {
    event(
        json!({"type":"response.function_call_arguments.delta","sequence_number":1,
            "item_id":"fc","output_index":0,"delta":value}),
    )
}

fn capture(target: &mut Vec<(String, bool)>) -> impl FnMut(AgentEvent) + '_ {
    |event| {
        if let AgentEvent::AssistantDelta { text, refusal } = event {
            target.push((text, refusal));
        }
    }
}

fn progress_of(push: impl FnOnce(&mut dyn FnMut(AgentEvent))) -> Option<u64> {
    let mut seen = None;
    push(&mut |event| {
        if let AgentEvent::ModelOutputProgress { output_bytes } = event {
            seen = Some(output_bytes);
        }
    });
    seen
}

#[test]
fn streaming_chat_progress_matches_across_utf8_chunk_splits() {
    let payload = "指标完成 value";
    let whole: [&str; 1] = [payload];
    let pieces: [&str; 3] = ["指", "标完成 ", "value"];
    let mut totals = Vec::new();
    for split in [&whole[..], &pieces[..]] {
        let mut stream = ChatStream::default();
        totals.push(progress_of(|emit| {
            for piece in split {
                stream
                    .push(chat(json!({"content": piece}), None), &mut *emit)
                    .unwrap();
            }
        }));
    }
    assert_eq!(
        totals,
        [Some(payload.len() as u64), Some(payload.len() as u64)]
    );
}

#[test]
fn streaming_chat_tool_arguments_advance_progress_without_answer_text() {
    let arguments = ["{\"command\":", "\"true\"}"];
    let mut stream = ChatStream::default();
    let first = progress_of(|emit| {
        stream
            .push(
                chat(
                    json!({"tool_calls":[{"index":0,"id":"call","type":"function",
                            "function":{"name":"shell","arguments":arguments[0]}}]}),
                    None,
                ),
                emit,
            )
            .unwrap();
    });
    let second = progress_of(|emit| {
        stream
            .push(
                chat(
                    json!({"tool_calls":[{"index":0,
                            "function":{"arguments":arguments[1]}}]}),
                    Some("tool_calls"),
                ),
                emit,
            )
            .unwrap();
    });
    // Tool ids and names never count: only the argument fragments do.
    assert_eq!(first, Some(arguments[0].len() as u64));
    assert_eq!(
        second,
        Some((arguments[0].len() + arguments[1].len()) as u64)
    );
}

#[test]
fn streaming_responses_progress_ignores_done_and_completed_copies() {
    let mut stream = ResponsesStream::default();
    let streamed = progress_of(|emit| {
        assert!(stream.push(delta(0, 0, "answer"), emit).unwrap().is_none());
    });
    assert_eq!(streamed, Some("answer".len() as u64));
    let done = progress_of(|emit| {
        assert!(
            stream
                .push(part_done(0, 0, "answer"), emit)
                .unwrap()
                .is_none()
        );
    });
    assert_eq!(done, None);
    let completed_bytes = progress_of(|emit| {
        assert!(
            stream
                .push(
                    completed(json!([message("m", json!([text("answer")]))])),
                    emit,
                )
                .unwrap()
                .is_some()
        );
    });
    assert_eq!(completed_bytes, None);
}

#[test]
fn streaming_responses_argument_progress_without_visible_answer() {
    let arguments = ["{\"command\":", "\"true\"}"];
    let mut stream = ResponsesStream::default();
    let mut visible = Vec::new();
    let first = {
        let mut seen = None;
        let mut emit = |event| match event {
            AgentEvent::AssistantDelta { text, refusal } => visible.push((text, refusal)),
            AgentEvent::ModelOutputProgress { output_bytes } => seen = Some(output_bytes),
            _ => {}
        };
        assert!(
            stream
                .push(arguments_delta(arguments[0]), &mut emit)
                .unwrap()
                .is_none()
        );
        seen
    };
    let second = {
        let mut seen = None;
        let mut emit = |event| match event {
            AgentEvent::AssistantDelta { text, refusal } => visible.push((text, refusal)),
            AgentEvent::ModelOutputProgress { output_bytes } => seen = Some(output_bytes),
            _ => {}
        };
        assert!(
            stream
                .push(arguments_delta(arguments[1]), &mut emit)
                .unwrap()
                .is_none()
        );
        seen
    };
    assert_eq!(first, Some(arguments[0].len() as u64));
    assert_eq!(
        second,
        Some((arguments[0].len() + arguments[1].len()) as u64)
    );
    assert!(visible.is_empty());
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
            _ => {}
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
    stream
        .push(
            chat(
                json!({"role":"assistant","tool_calls":[
                    {"index":0,"function":{"name":"ite","arguments":"\"a\",\"content\":\"b\"}"}},
                    {"index":1,"function":{"name":"sh","arguments":"\"true\"}"}}
                ]}),
                Some("tool_calls"),
            ),
            &mut capture(&mut visible),
        )
        .unwrap();
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
                    json!({"prompt_tokens":12000,"completion_tokens":300,"total_tokens":12300,"prompt_tokens_details":{"cached_tokens":40}}),
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
            output_tokens: 300,
            cache_write_tokens: None,
            cached_tokens: Some(40),
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
            output_tokens: 300,
            cache_write_tokens: None,
            cached_tokens: Some(40),
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
    let turn = stream
        .push(
            completed(json!([
                {"type":"function_call","id":"fc","call_id":"call","name":"write","arguments":"{}"},
                message("m", json!([text("tool preface")]))
            ])),
            &mut capture(&mut visible),
        )
        .unwrap()
        .unwrap();
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
