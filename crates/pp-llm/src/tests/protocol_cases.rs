use super::*;

#[test]
fn refuses_truncated_or_invalid_batches() {
    let call = json!({"id":"x","type":"function","function":{"name":"write","arguments":"{}"}});
    for finish in ["length", "content_filter", "unknown"] {
        assert!(decode(json!({"role":"assistant","tool_calls":[call]}), finish).is_err());
    }
    assert!(
        decode(
            json!({"role":"assistant","tool_calls":[call,call]}),
            "tool_calls"
        )
        .is_err()
    );
    assert!(decode(json!({"role":"user","content":"wrong"}), "stop").is_err());
    assert!(decode(json!({"role":"assistant","content":null}), "stop").is_err());
    assert!(
        decode(
            json!({"role":"assistant","tool_calls":[call, {
                "id":"custom", "type":"custom", "custom_tool":{"name":"write","input":"{}"}
            }]}),
            "tool_calls"
        )
        .is_err()
    );
}

#[test]
fn accepts_refusal_and_nullable_calls() {
    for message in [
        json!({"role":"assistant","refusal":"Cannot help"}),
        json!({"role":"assistant","content":null,"tool_calls":null,"refusal":"Cannot help"}),
    ] {
        let turn = decode(message, "stop").unwrap();
        assert_eq!(
            turn.message.content,
            Some(ChatCompletionRequestAssistantMessageContent::Text(
                "Cannot help".into()
            ))
        );
        assert!(turn.message.tool_calls.is_none());
        assert!(turn.refusal);
        assert_eq!(turn.usage, None);
    }
}

#[test]
fn context_chat_refusal_flag_requires_no_normal_content() {
    let refusal_only = decode(json!({"role":"assistant","refusal":"no"}), "stop").unwrap();
    assert!(refusal_only.refusal);
    let mixed = decode(
        json!({"role":"assistant","content":"answer","refusal":"no"}),
        "stop",
    )
    .unwrap();
    assert!(!mixed.refusal);
    let blank_content = decode(
        json!({"role":"assistant","content":"  ","refusal":"no"}),
        "stop",
    )
    .unwrap();
    assert!(blank_content.refusal);
    let text_only = decode(json!({"role":"assistant","content":"answer"}), "stop").unwrap();
    assert!(!text_only.refusal);
}

#[test]
fn context_chat_cached_breakdown_preserves_unknown_and_zero() {
    for (details, expected) in [
        (None, None),
        (Some(json!({"cached_tokens":0})), Some(0)),
        (Some(json!({"cached_tokens":40})), Some(40)),
    ] {
        let mut response = json!({
            "id":"fixture", "object":"chat.completion", "created":1, "model":"fixture",
            "choices":[{"index":0,"message":{"role":"assistant","content":"answer"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":12000,"completion_tokens":300,"total_tokens":12300}
        });
        if let Some(details) = details {
            response["usage"]["prompt_tokens_details"] = details;
        }
        let turn = into_completion_turn(serde_json::from_value(response).unwrap()).unwrap();
        assert_eq!(
            turn.usage,
            Some(TokenUsage {
                input_tokens: 12000,
                output_tokens: 300,
                cache_write_tokens: None,
                cached_tokens: expected,
            })
        );
    }
}

#[test]
fn context_chat_usage_only_chunk_after_finish_returns_usage() {
    let server = TestServer::start().unwrap();
    let mut client = test_client(&format!("http://{}", server.addr()));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            // First request: normal chunks, then a choices-empty usage-only chunk.
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let mut writer = request.start_sse().unwrap();
            writer
                .frame(
                    &chat_chunk(json!({"role":"assistant","content":"answer"}), None).to_string(),
                )
                .unwrap();
            writer
                .frame(&chat_chunk(json!({}), Some("stop")).to_string())
                .unwrap();
            writer
                .frame(
                    r#"{"id":"chatcmpl-fixture","object":"chat.completion.chunk","created":2,"model":"fixture-model","choices":[],"usage":{"prompt_tokens":12000,"completion_tokens":300,"total_tokens":12300}}"#,
                )
                .unwrap();
            writer.frame("[DONE]").unwrap();
            writer.finish();
            // Second request: no usage at all; the answer still succeeds.
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let mut writer = request.start_sse().unwrap();
            writer
                .frame(
                    &chat_chunk(json!({"role":"assistant","content":"again"}), Some("stop"))
                        .to_string(),
                )
                .unwrap();
            writer.frame("[DONE]").unwrap();
            writer.finish();
        });
        let token = CancellationToken::new();
        let mut finished = None;
        let turn = client
            .complete(
                &CreateChatCompletionRequest::default(),
                &token,
                &mut || Ok(()),
                &mut |event| {
                    if let AgentEvent::ModelRequestFinished { usage, success, .. } = event {
                        finished = Some((usage, success));
                    }
                },
            )
            .unwrap();
        assert_eq!(
            turn.usage,
            Some(TokenUsage {
                input_tokens: 12000,
                output_tokens: 300,
                cache_write_tokens: None,
                cached_tokens: None,
            })
        );
        assert_eq!(finished, Some((turn.usage, true)));
        assert_eq!(
            turn.message.content,
            Some(ChatCompletionRequestAssistantMessageContent::Text(
                "answer".into()
            ))
        );
        assert!(!turn.refusal);
        let token = CancellationToken::new();
        let mut finished = None;
        let turn = client
            .complete(
                &CreateChatCompletionRequest::default(),
                &token,
                &mut || Ok(()),
                &mut |event| {
                    if let AgentEvent::ModelRequestFinished { usage, success, .. } = event {
                        finished = Some((usage, success));
                    }
                },
            )
            .unwrap();
        assert_eq!(turn.usage, None);
        assert_eq!(finished, Some((None, true)));
        assert_eq!(
            turn.message.content,
            Some(ChatCompletionRequestAssistantMessageContent::Text(
                "again".into()
            ))
        );
    });
}

#[test]
fn context_responses_usage_extracted_and_missing_usage_is_none() {
    let server = TestServer::start().unwrap();
    let mut client = test_client(&format!("http://{}", server.addr()));
    let completed = |usage: Option<Value>| {
        let mut response = json!({
            "id":"fixture", "object":"response", "created_at":0, "model":"fixture",
            "status":"completed",
            "output":[{"type":"message","id":"m","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"answer","annotations":[]}]}]
        });
        if let Some(usage) = usage {
            response["usage"] = usage;
        }
        json!({"type":"response.completed","sequence_number":5,"response":response}).to_string()
    };
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let mut writer = request.start_sse().unwrap();
            writer
                .frame(&completed(Some(json!({
                    "input_tokens":12000, "output_tokens":300, "total_tokens":12300,
                    "input_tokens_details":{"cached_tokens":40},
                    "output_tokens_details":{"reasoning_tokens":50}
                }))))
                .unwrap();
            writer.finish();
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let mut writer = request.start_sse().unwrap();
            writer.frame(&completed(None)).unwrap();
            writer.finish();
        });
        let token = CancellationToken::new();
        let mut finished = None;
        let turn = client
            .respond(
                &CreateResponse::default(),
                &token,
                &mut || Ok(()),
                &mut |event| {
                    if let AgentEvent::ModelRequestFinished { usage, success, .. } = event {
                        finished = Some((usage, success));
                    }
                },
            )
            .unwrap();
        assert_eq!(turn.text, "answer");
        assert!(!turn.has_calls);
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
        assert_eq!(finished, Some((turn.usage, true)));
        let token = CancellationToken::new();
        let mut finished = None;
        let turn = client
            .respond(
                &CreateResponse::default(),
                &token,
                &mut || Ok(()),
                &mut |event| {
                    if let AgentEvent::ModelRequestFinished { usage, success, .. } = event {
                        finished = Some((usage, success));
                    }
                },
            )
            .unwrap();
        assert_eq!(turn.usage, None);
        assert_eq!(finished, Some((None, true)));
    });
}
