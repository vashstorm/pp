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
                reasoning_tokens: None,
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
                reasoning_tokens: None,
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
                reasoning_tokens: Some(50),
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

#[test]
fn retry_precheck_failure_closes_pending_request_lifecycle() {
    use pp_config::task_status::View;

    for use_responses in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let stats_db = dir.path().join("stats.db");
        let server = TestServer::start().unwrap();

        let settings_path = dir.path().join("settings.yml");
        let models_path = dir.path().join("models.yml");
        std::fs::write(
            &models_path,
            format!(
                "providers:\n  fixture:\n    api_key: k\n    base_url: http://{}\n    models:\n      primary:\n        model: gpt-5\n        cost:\n          input: 1.0\n          cacheRead: 0.0\n          output: 1.0\n",
                server.addr()
            ),
        )
        .unwrap();
        let catalog =
            pp_config::config::ModelCatalog::from_files(&settings_path, &models_path).unwrap();
        let view = View::default();
        let ledger = view.accounting(&catalog);

        let mut client = test_client(&format!("http://{}", server.addr()));
        client.model_reference = "fixture/primary".into();
        client.task_accounting = Some(ledger);
        client.stats_context = Some(pp_stats::RequestContext {
            cwd: dir.path().to_path_buf(),
            model_reference: "fixture/primary".into(),
            purpose: "chat",
            session_file: None,
            unlinked_requests: None,
            first_request: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            skills: Vec::new(),
            database: pp_stats::Database::new(stats_db.clone()),
        });

        let token = CancellationToken::new();
        let mut started = 0usize;
        let mut finished = Vec::new();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let req = server
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                req.respond(
                    Response::builder()
                        .status(429)
                        .header("Retry-After", "0")
                        .body(Body::from("rate limit"))
                        .unwrap(),
                )
                .unwrap();
            });
            let mut on_event = |event| match event {
                AgentEvent::ModelRequestStarted => started += 1,
                AgentEvent::ModelRequestFinished { success, .. } => finished.push(success),
                _ => {}
            };
            let mut precheck = |attempt: u32| {
                if attempt >= 2 {
                    bail!("simulated auth refresh failure");
                }
                Ok(())
            };
            let res: Result<()> = if use_responses {
                client
                    .respond_request_with(
                        &CreateResponse::default(),
                        &token,
                        &mut || Ok(()),
                        &mut on_event,
                        false,
                        &mut precheck,
                    )
                    .map(|_| ())
            } else {
                client
                    .complete_request_with(
                        &CreateChatCompletionRequest::default(),
                        &token,
                        &mut || Ok(()),
                        &mut on_event,
                        false,
                        &mut precheck,
                    )
                    .map(|_| ())
            };
            assert!(res.is_err());
        });
        assert_eq!(started, 1);
        assert_eq!(finished, vec![false]);
        assert!(client.stats_key.is_some());
        assert_eq!(view.snapshot().unwrap().live, 0.0);
        assert_eq!(usage_of(&client, "fixture/primary"), 1);
    }
}

#[test]
fn chat_completion_reasoning_tokens_extracted_from_details() {
    let response = json!({
        "id": "chatcmpl-reasoning",
        "object": "chat.completion",
        "created": 1,
        "model": "fixture-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 500,
            "completion_tokens": 200,
            "total_tokens": 700,
            "prompt_tokens_details": {"cached_tokens": 300},
            "completion_tokens_details": {"reasoning_tokens": 120}
        }
    });
    let turn = into_completion_turn(serde_json::from_value(response).unwrap()).unwrap();
    assert_eq!(
        turn.usage,
        Some(TokenUsage {
            input_tokens: 500,
            output_tokens: 200,
            cached_tokens: Some(300),
            cache_write_tokens: None,
            reasoning_tokens: Some(120),
        })
    );
}
